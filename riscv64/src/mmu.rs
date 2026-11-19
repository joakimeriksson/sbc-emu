//! Sv39 address translation (privileged ISA 1.11, "Sv39: Page-Based 39-bit Virtual-Memory
//! System"): three levels of 8-byte PTEs, 4 KiB pages, 2 MiB and 1 GiB superpages, A/D
//! bits set by the walk, MPRV/SUM/MXR, and page faults with the virtual address in `xtval`.
//!
//! A direct-mapped TLB keeps the leaf of recent walks. Permissions are checked on every
//! access against the mode in effect, so mode and `mstatus` changes need no flush;
//! `sfence.vma` and `satp` writes flush it.
use crate::{exc, mstatus, prv, Bus, Hart};

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Access {
    Fetch,
    Load,
    Store,
}

impl Access {
    pub fn page_fault(self) -> u64 {
        match self {
            Access::Fetch => exc::INSN_PAGE,
            Access::Load => exc::LOAD_PAGE,
            Access::Store => exc::STORE_PAGE,
        }
    }
    pub fn access_fault(self) -> u64 {
        match self {
            Access::Fetch => exc::INSN_ACCESS,
            Access::Load => exc::LOAD_ACCESS,
            Access::Store => exc::STORE_ACCESS,
        }
    }
}

pub mod pte {
    pub const V: u64 = 1;
    pub const R: u64 = 2;
    pub const W: u64 = 4;
    pub const X: u64 = 8;
    pub const U: u64 = 16;
    pub const G: u64 = 32;
    pub const A: u64 = 64;
    pub const D: u64 = 128;
}

const TLB_SIZE: usize = 1024;

#[derive(Clone, Copy)]
struct Entry {
    /// virtual page number + 1 (0 = empty)
    tag: u64,
    /// physical address of the 4 KiB page
    page: u64,
    /// leaf PTE flags (low 8 bits)
    flags: u64,
}

#[derive(Default)]
pub struct Tlb {
    entries: Vec<Entry>,
    pub walks: u64,
}

impl Tlb {
    pub fn flush(&mut self) {
        self.entries.clear();
    }
}

impl Hart {
    /// The privilege a data access is made with: MPRV substitutes MPP in M mode.
    #[inline(always)]
    fn data_prv(&self) -> u64 {
        if self.prv == prv::M && self.mstatus & mstatus::MPRV != 0 {
            (self.mstatus & mstatus::MPP) >> 11
        } else {
            self.prv
        }
    }

    #[inline(always)]
    fn access_prv(&self, access: Access) -> u64 {
        if access == Access::Fetch {
            self.prv
        } else {
            self.data_prv()
        }
    }

    /// Translate `va` for `access`; on failure the exception cause.
    #[inline(always)]
    pub fn translate<B: Bus>(&mut self, bus: &mut B, va: u64, access: Access) -> Result<u64, u64> {
        if self.satp >> 60 == 0 || self.access_prv(access) == prv::M {
            return Ok(va);
        }
        let vpn = va >> 12;
        let slot = (vpn as usize) & (TLB_SIZE - 1);
        if let Some(e) = self.tlb.entries.get(slot) {
            if e.tag == vpn.wrapping_add(1)
                && self.permits(e.flags, access)
                && (access != Access::Store || e.flags & pte::D != 0)
            {
                return Ok(e.page | (va & 0xfff));
            }
        }
        self.walk(bus, va, access)
    }

    #[inline(always)]
    fn permits(&self, flags: u64, access: Access) -> bool {
        let p = self.access_prv(access);
        if flags & pte::U != 0 {
            if p == prv::S && (access == Access::Fetch || self.mstatus & mstatus::SUM == 0) {
                return false;
            }
        } else if p == prv::U {
            return false;
        }
        match access {
            Access::Fetch => flags & pte::X != 0,
            Access::Load => flags & pte::R != 0 || (self.mstatus & mstatus::MXR != 0 && flags & pte::X != 0),
            Access::Store => flags & pte::W != 0,
        }
    }

    #[cold]
    fn walk<B: Bus>(&mut self, bus: &mut B, va: u64, access: Access) -> Result<u64, u64> {
        self.tlb.walks += 1;
        let pf = access.page_fault();
        // bits 63:39 must all equal bit 38
        if ((va as i64) << 25 >> 25) as u64 != va {
            return Err(pf);
        }
        let mut table = (self.satp & 0xfff_ffff_ffff) << 12;
        let mut level = 2i32;
        loop {
            let vpn_i = (va >> (12 + 9 * level as u32)) & 0x1ff;
            let pte_addr = table + vpn_i * 8;
            let pte = read_pte(bus, pte_addr).ok_or(access.access_fault())?;
            if pte & pte::V == 0 || (pte & pte::R == 0 && pte & pte::W != 0) || pte >> 54 != 0 {
                return Err(pf);
            }
            let ppn = (pte >> 10) & 0xfff_ffff_ffff;
            if pte & (pte::R | pte::X) == 0 {
                level -= 1;
                if level < 0 {
                    return Err(pf);
                }
                table = ppn << 12;
                continue;
            }
            if !self.permits(pte, access) {
                return Err(pf);
            }
            // misaligned superpage
            let low_mask = (1u64 << (9 * level as u32)) - 1;
            if ppn & low_mask != 0 {
                return Err(pf);
            }
            let mut new = pte | pte::A;
            if access == Access::Store {
                new |= pte::D;
            }
            if new != pte && write_pte(bus, pte_addr, new).is_none() {
                return Err(access.access_fault());
            }
            let page = (ppn << 12) | (va & (low_mask << 12));
            if self.tlb.entries.is_empty() {
                self.tlb.entries = vec![Entry { tag: 0, page: 0, flags: 0 }; TLB_SIZE];
            }
            let vpn = va >> 12;
            self.tlb.entries[(vpn as usize) & (TLB_SIZE - 1)] = Entry { tag: vpn.wrapping_add(1), page, flags: new & 0xff };
            return Ok(page | (va & 0xfff));
        }
    }
}

fn read_pte<B: Bus>(bus: &mut B, pa: u64) -> Option<u64> {
    let base = bus.ram_base();
    let ram = bus.ram();
    let o = pa.wrapping_sub(base);
    if o + 8 <= ram.len() as u64 {
        let o = o as usize;
        return Some(u64::from_le_bytes(ram[o..o + 8].try_into().unwrap()));
    }
    bus.read(pa, 8).ok()
}

fn write_pte<B: Bus>(bus: &mut B, pa: u64, v: u64) -> Option<()> {
    let base = bus.ram_base();
    let ram = bus.ram();
    let o = pa.wrapping_sub(base);
    if o + 8 <= ram.len() as u64 {
        let o = o as usize;
        ram[o..o + 8].copy_from_slice(&v.to_le_bytes());
        return Some(());
    }
    bus.write(pa, 8, v).ok()
}
