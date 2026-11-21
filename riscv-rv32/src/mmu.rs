//! Sv32 address translation for a core with S and U modes (`Cpu::privileged`, the ESP32-S31
//! Linux profile). RISC-V privileged ISA v1.12, "Sv32: Page-Based 32-bit Virtual-Memory
//! Systems": two levels of 4-byte PTEs, 4 KiB pages and 4 MiB megapages, A/D bits set by the
//! walk (Svadu behaviour: Linux accepts both), MPRV/SUM/MXR, page faults with the faulting
//! virtual address in `xtval`.
//!
//! A small direct-mapped TLB keeps the leaf PTE of recent translations; permissions are
//! checked on every access against the current mode, so mode or mstatus changes need no
//! flush. `sfence.vma` and `satp` writes flush it.
use crate::bus::Bus;
use crate::state::{exc, mstatus, privilege, Cpu};

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Access { Fetch, Load, Store }

impl Access {
    fn page_fault(self) -> u32 {
        match self { Access::Fetch => exc::INSN_PAGE_FAULT, Access::Load => exc::LOAD_PAGE_FAULT, Access::Store => exc::STORE_PAGE_FAULT }
    }
    fn access_fault(self) -> u32 {
        match self { Access::Fetch => exc::INSN_ACCESS_FAULT, Access::Load => exc::LOAD_ACCESS_FAULT, Access::Store => exc::STORE_ACCESS_FAULT }
    }
}

pub mod pte {
    pub const V: u32 = 1; pub const R: u32 = 2; pub const W: u32 = 4; pub const X: u32 = 8;
    pub const U: u32 = 16; pub const G: u32 = 32; pub const A: u32 = 64; pub const D: u32 = 128;
}

const TLB_SIZE: usize = 256;

/// `repr(C)`: compiled code (`jit`) reads `tag`, `page` and `flags` at offsets 0, 4 and 8.
#[derive(Clone, Copy)]
#[repr(C)]
struct TlbEntry {
    /// virtual page number + 1 (0 = empty)
    tag: u32,
    /// physical address of the 4 KiB page this virtual page maps to
    page: u32,
    /// the leaf PTE's flags (low 8 bits)
    flags: u32,
}

/// The core's Sv32 TLB. Empty until translation is used.
pub struct Tlb {
    entries: Vec<TlbEntry>,
    pub walks: u64,
}

impl Default for Tlb {
    fn default() -> Self { Tlb { entries: Vec::new(), walks: 0 } }
}

impl Tlb {
    pub fn flush(&mut self) { self.entries.clear(); }
    /// The slot that holds `va`'s translation (`tag`, `page`, `flags` as u32s), null while the TLB
    /// is empty. It stays put until the TLB is flushed (`sfence.vma`, a `satp` write).
    pub(crate) fn slot_ptr(&self, va: u32) -> *const u32 {
        if self.entries.is_empty() { return std::ptr::null(); }
        (&self.entries[((va >> 12) as usize) & (TLB_SIZE - 1)]) as *const TlbEntry as *const u32
    }
}

impl Cpu {
    /// The privilege a data access is made with: MPRV substitutes MPP for loads and stores.
    #[inline(always)]
    fn data_priv(&self) -> u32 {
        if self.mstatus & mstatus::MPRV != 0 { (self.mstatus & mstatus::MPP) >> 11 } else { self.priv_mode }
    }

    /// Whether `access` at the current mode goes through Sv32.
    #[inline(always)]
    pub fn translating(&self, access: Access) -> bool {
        if !self.privileged || self.satp & 0x8000_0000 == 0 { return false; }
        let p = if access == Access::Fetch { self.priv_mode } else { self.data_priv() };
        p != privilege::MACHINE
    }

    /// Translate `va` for `access`; on failure the exception cause (page or access fault).
    #[inline(always)]
    pub fn translate<B: Bus>(&mut self, bus: &mut B, va: u32, access: Access) -> Result<u32, u32> {
        if !self.translating(access) { return Ok(va); }
        let vpn = va >> 12;
        let slot = (vpn as usize) & (TLB_SIZE - 1);
        if let Some(e) = self.tlb.entries.get(slot) {
            if e.tag == vpn + 1 && self.permits(e.flags, access) && (access != Access::Store || e.flags & pte::D != 0) {
                return Ok(e.page | (va & 0xfff));
            }
        }
        self.walk(bus, va, access)
    }

    /// The permission check of a leaf PTE for the access mode in effect.
    #[inline(always)]
    fn permits(&self, flags: u32, access: Access) -> bool {
        let p = if access == Access::Fetch { self.priv_mode } else { self.data_priv() };
        if flags & pte::U != 0 {
            if p == privilege::SUPERVISOR && (access == Access::Fetch || self.mstatus & mstatus::SUM == 0) { return false; }
        } else if p == privilege::USER { return false; }
        match access {
            Access::Fetch => flags & pte::X != 0,
            Access::Load => flags & pte::R != 0 || (self.mstatus & mstatus::MXR != 0 && flags & pte::X != 0),
            Access::Store => flags & pte::W != 0,
        }
    }

    #[cold]
    fn walk<B: Bus>(&mut self, bus: &mut B, va: u32, access: Access) -> Result<u32, u32> {
        self.tlb.walks += 1;
        let pf = access.page_fault();
        let mut table = (self.satp & 0x3f_ffff) << 12;
        let mut level = 1i32;
        loop {
            let vpn_i = (va >> (12 + 10 * level as u32)) & 0x3ff;
            let pte_addr = table.wrapping_add(vpn_i * 4);
            let pte = bus.read32(pte_addr).map_err(|_| access.access_fault())?;
            if pte & pte::V == 0 || (pte & pte::R == 0 && pte & pte::W != 0) { return Err(pf); }
            if pte & (pte::R | pte::X) == 0 {
                // pointer to the next level
                level -= 1;
                if level < 0 { return Err(pf); }
                table = (pte >> 10) << 12;
                continue;
            }
            // leaf
            if !self.permits(pte, access) { return Err(pf); }
            let ppn = pte >> 10;
            if level == 1 && ppn & 0x3ff != 0 { return Err(pf); }   // misaligned megapage
            let mut new = pte | pte::A;
            if access == Access::Store { new |= pte::D; }
            if new != pte && bus.write32(pte_addr, new).is_err() { return Err(access.access_fault()); }
            let page = if level == 1 { ((ppn >> 10) << 22) | (va & 0x3f_f000) } else { ppn << 12 };
            if self.tlb.entries.is_empty() { self.tlb.entries = vec![TlbEntry { tag: 0, page: 0, flags: 0 }; TLB_SIZE]; }
            let vpn = va >> 12;
            self.tlb.entries[(vpn as usize) & (TLB_SIZE - 1)] = TlbEntry { tag: vpn + 1, page, flags: new & 0xff };
            return Ok(page | (va & 0xfff));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use emu_core::FlatRam;

    fn cpu_s() -> Cpu {
        let mut c = Cpu::new_esp32s31(); c.priv_mode = privilege::SUPERVISOR; c
    }

    #[test]
    fn bare_mode_and_machine_mode_do_not_translate() {
        let mut ram = FlatRam::new(0x5000_0000, 0x10000);
        let mut c = cpu_s();
        assert_eq!(c.translate(&mut ram, 0x1234, Access::Load), Ok(0x1234));
        c.satp = 0x8000_0000 | (0x5000_0000 >> 12); c.priv_mode = privilege::MACHINE;
        assert_eq!(c.translate(&mut ram, 0x1234, Access::Load), Ok(0x1234));
    }

    #[test]
    fn megapage_and_4k_pages_translate_and_fault() {
        let base = 0x5000_0000u32;
        let mut ram = FlatRam::new(base, 0x10000);
        let root = base; let l0 = base + 0x1000;
        let w = |ram: &mut FlatRam, a: u32, v: u32| { let o = (a - base) as usize; ram.mem[o..o + 4].copy_from_slice(&v.to_le_bytes()); };
        // VA 0xc0000000 -> megapage at PA 0x40400000, RX, kernel
        w(&mut ram, root + 4 * 0x300, ((0x4040_0000 >> 12) << 10) | pte::V | pte::R | pte::X | pte::A);
        // VA 0x00400000 -> table l0; VA 0x00401000 -> PA 0x50002000, RW user
        w(&mut ram, root + 4 * 1, ((l0 >> 12) << 10) | pte::V);
        w(&mut ram, l0 + 4 * 1, ((0x5000_2000 >> 12) << 10) | pte::V | pte::R | pte::W | pte::U);
        let mut c = cpu_s(); c.satp = 0x8000_0000 | (root >> 12);
        assert_eq!(c.translate(&mut ram, 0xc000_1234, Access::Fetch), Ok(0x4040_1234));
        assert_eq!(c.translate(&mut ram, 0xc012_3456, Access::Load), Ok(0x4052_3456));
        assert_eq!(c.translate(&mut ram, 0xc000_0000, Access::Store), Err(exc::STORE_PAGE_FAULT));
        // S cannot touch a U page without SUM
        assert_eq!(c.translate(&mut ram, 0x0040_1010, Access::Load), Err(exc::LOAD_PAGE_FAULT));
        c.mstatus |= mstatus::SUM;
        assert_eq!(c.translate(&mut ram, 0x0040_1010, Access::Store), Ok(0x5000_2010));
        // the walk set A and D
        let o = (l0 + 4 - base) as usize;
        let pte = u32::from_le_bytes(ram.mem[o..o + 4].try_into().unwrap());
        assert_eq!(pte & (pte::A | pte::D), pte::A | pte::D);
        // U mode may use it, and may not fetch from it (no X)
        c.priv_mode = privilege::USER;
        assert_eq!(c.translate(&mut ram, 0x0040_1010, Access::Load), Ok(0x5000_2010));
        assert_eq!(c.translate(&mut ram, 0x0040_1010, Access::Fetch), Err(exc::INSN_PAGE_FAULT));
        // unmapped
        assert_eq!(c.translate(&mut ram, 0x0080_0000, Access::Load), Err(exc::LOAD_PAGE_FAULT));
        // a flush forgets; the PTE is walked again
        c.tlb.flush();
        assert_eq!(c.translate(&mut ram, 0x0040_1010, Access::Load), Ok(0x5000_2010));
    }
}
