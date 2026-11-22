use crate::cpu::{Cpu, MODE_USR};
use emu_core::Bus;

#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum Access {
    Read,
    Write,
    Execute,
}

/// One cached 4 KiB translation. Permissions are checked on every access against the
/// current mode and DACR, so mode and domain changes need no TLB maintenance.
#[derive(Clone, Copy)]
pub(crate) struct TlbEntry {
    /// `va >> 12`, or `u32::MAX` when invalid.
    tag: u32,
    pa_page: u32,
    domain: u8,
    ap: u8,
    xn: bool,
    pxn: bool,
    section: bool,
}

pub(crate) const TLB_ENTRIES: usize = 1024;

impl TlbEntry {
    pub(crate) const INVALID: Self = Self {
        tag: u32::MAX,
        pa_page: 0,
        domain: 0,
        ap: 0,
        xn: false,
        pxn: false,
        section: false,
    };
}

/// Micro-TLB tables (one per privilege and access kind) of host pointers into guest RAM:
/// index `utlb_table(privileged, kind) * UT + (va >> 12) % UT`. Shared by the interpreter
/// and compiled code (which probes the read/write tables inline).
pub(crate) const UT: usize = 256;
pub(crate) const U_READ: usize = 0;
pub(crate) const U_WRITE: usize = 1;
pub(crate) const U_EXEC: usize = 2;

/// One micro-TLB entry: `tag` is the virtual page (`va & !0xfff`; 0x800 = invalid: probes
/// compare it with the address masked to its page and alignment bits, which never has bit
/// 11 set) and `delta` is added to a virtual address in that page to give the host address
/// of its byte.
#[derive(Clone, Copy)]
#[repr(C)]
pub(crate) struct Utlb {
    pub tag: u64,
    pub delta: u64,
}

impl Utlb {
    pub(crate) const INVALID: Self = Self { tag: 0x800, delta: 0 };
}

pub(crate) fn utlb_table(privileged: bool, kind: usize) -> usize {
    privileged as usize * 3 + kind
}

impl Cpu {
    /// Drop every cached translation (TLB maintenance, TTBR/TTBCR/SCTLR/CONTEXTIDR writes).
    pub fn tlb_flush(&mut self) {
        self.tlb.fill(TlbEntry::INVALID);
        self.utlb_flush();
    }

    /// TLB maintenance by virtual address: the cached translations of `va`'s page, and of
    /// every page of its MiB if they came from a section (the guest may be changing it).
    pub(crate) fn tlb_invalidate_page(&mut self, va: u32) {
        let page = va >> 12;
        let mut section = false;
        for p in page & !0xff..(page & !0xff) + 256 {
            let e = &mut self.tlb[p as usize % TLB_ENTRIES];
            if e.tag == p && (p == page || e.section) {
                section |= e.section;
                *e = TlbEntry::INVALID;
            }
        }
        if section {
            self.utlb_flush();
            return;
        }
        for t in 0..6 {
            let e = &mut self.utlb[t * UT + page as usize % UT];
            if e.tag == (va & !0xfff) as u64 {
                *e = Utlb::INVALID;
            }
        }
    }

    /// Drop the micro-TLBs (also needed when DACR changes: permissions were checked at fill).
    pub(crate) fn utlb_flush(&mut self) {
        self.utlb.fill(Utlb::INVALID);
    }

    /// Drop the write micro-TLBs (a page became code: its stores must take the slow path).
    pub(crate) fn utlb_flush_writes(&mut self) {
        for privileged in [false, true] {
            let t = utlb_table(privileged, U_WRITE) * UT;
            self.utlb[t..t + UT].fill(Utlb::INVALID);
        }
    }

    pub(crate) fn privileged(&self) -> bool {
        self.cpsr & 0x1f != MODE_USR
    }

    /// Host address of `size` bytes at `va` if the micro-TLB maps them (same page).
    #[inline(always)]
    fn host(&self, va: u32, size: u32, kind: usize) -> Option<*mut u8> {
        if (va & 0xfff) + size > 0x1000 {
            return None;
        }
        let e = &self.utlb[utlb_table(self.privileged(), kind) * UT + (va >> 12) as usize % UT];
        (e.tag == (va & !0xfff) as u64).then(|| e.delta.wrapping_add(va as u64) as *mut u8)
    }

    /// Host address of `size` bytes at `va` in the write micro-TLB.
    pub(crate) fn host_ptr(&self, va: u32, size: u32) -> Option<*mut u8> {
        self.host(va, size, U_WRITE)
    }

    /// Is `pa` (physical) in the RAM the host pointer covers?
    #[inline(always)]
    pub(crate) fn in_ram(&self, pa: u32) -> bool {
        !self.ram.is_null() && pa.wrapping_sub(self.ram_base) < self.ram_len
    }

    /// Remember a checked translation of RAM in the micro-TLB. Pages holding compiled
    /// code never enter the write table, so every store to them reaches `code_written`.
    fn utlb_fill(&mut self, va: u32, pa: u32, kind: usize) {
        // while a debugger watches, data accesses take the slow path, which checks them
        if !self.watch.is_empty() && kind != U_EXEC {
            return;
        }
        if !self.in_ram(pa) {
            self.io = true;
            return;
        }
        if kind == U_WRITE && self.jit.as_ref().is_some_and(|j| j.watch.is_code(pa)) {
            return;
        }
        let host = self.ram as u64 + (pa & !0xfff).wrapping_sub(self.ram_base) as u64;
        self.utlb[utlb_table(self.privileged(), kind) * UT + (va >> 12) as usize % UT] =
            Utlb { tag: (va & !0xfff) as u64, delta: host.wrapping_sub((va & !0xfff) as u64) };
    }

    /// A store reached physical `pa` through the slow path: drop compiled code there.
    fn code_written(&mut self, pa: u32) {
        if let Some(j) = self.jit.as_ref() {
            if j.watch.is_code(pa) {
                j.watch.invalidate(pa);
            }
        }
    }

    fn permission(ap: u8, privileged: bool, write: bool) -> bool {
        match ap {
            1 => privileged,
            2 => privileged || !write,
            3 => true,
            5 => privileged && !write,
            6 | 7 => !write,
            _ => false,
        }
    }

    fn check(&self, entry: &TlbEntry, access: Access) -> Result<(), u32> {
        let domain = entry.domain as u32;
        let (domain_fault, permission_fault) = if entry.section {
            (0x09, 0x0d)
        } else {
            (0x0b, 0x0f)
        };
        let manager = match (self.dacr >> (domain * 2)) & 3 {
            1 => false,
            3 => true,
            _ => return Err(domain_fault | domain << 4),
        };
        let privileged = self.cpsr & 0x1f != MODE_USR;
        let execute_never =
            access == Access::Execute && (entry.xn || privileged && entry.pxn);
        if execute_never
            || !manager && !Self::permission(entry.ap, privileged, access == Access::Write)
        {
            return Err(permission_fault | domain << 4);
        }
        Ok(())
    }

    /// Walk ARMv7 short-descriptor tables for `va`. The fault is the FSR status
    /// (without the write bit).
    fn walk(&self, bus: &mut dyn Bus, va: u32) -> Result<TlbEntry, u32> {
        let n = self.ttbcr & 7;
        let use_ttbr1 = n != 0 && (va as u64) >= (0x1_0000_0000u64 >> n);
        if use_ttbr1 && self.ttbcr & (1 << 5) != 0 || !use_ttbr1 && self.ttbcr & (1 << 4) != 0 {
            return Err(0x05);
        }
        let table = if use_ttbr1 {
            self.ttbr1 & 0xffff_c000
        } else {
            self.ttbr0 & !((1u32 << (14 - n)) - 1)
        };
        let l1_address = table as u64 + ((va >> 20) as u64) * 4;
        let l1 = bus.read32(l1_address).map_err(|_| 0x0cu32)?;
        let domain = ((l1 >> 5) & 0xf) as u8;
        match l1 & 3 {
            0 | 3 => Err(0x05),
            2 => {
                // Section; bit 18 selects a 16 MiB supersection (domain 0, PA[31:24]).
                let (base, offset_mask, domain) = if l1 & (1 << 18) != 0 {
                    (l1 & 0xff00_0000, 0x00ff_ffff, 0)
                } else {
                    (l1 & 0xfff0_0000, 0x000f_ffff, domain)
                };
                Ok(TlbEntry {
                    tag: va >> 12,
                    pa_page: (base | (va & offset_mask)) & !0xfff,
                    domain,
                    ap: ((l1 >> 10) & 3 | (l1 >> 13) & 4) as u8,
                    xn: l1 & (1 << 4) != 0,
                    pxn: l1 & 1 != 0,
                    section: true,
                })
            }
            _ => {
                let l2_address = (l1 & 0xffff_fc00) as u64 + ((va >> 12) & 0xff) as u64 * 4;
                let l2 = bus.read32(l2_address).map_err(|_| 0x0eu32)?;
                let (pa_page, xn) = match l2 & 3 {
                    0 => return Err(0x07 | (domain as u32) << 4),
                    1 => ((l2 & 0xffff_0000) | (va & 0xf000), l2 & (1 << 15) != 0),
                    _ => (l2 & 0xffff_f000, l2 & 1 != 0),
                };
                Ok(TlbEntry {
                    tag: va >> 12,
                    pa_page,
                    domain,
                    ap: ((l2 >> 4) & 3 | (l2 >> 7) & 4) as u8,
                    xn,
                    pxn: l1 & (1 << 2) != 0,
                    section: false,
                })
            }
        }
    }

    /// Translate one virtual address. Errors carry (faulting VA, FSR status).
    pub(crate) fn translate(
        &mut self,
        bus: &mut dyn Bus,
        va: u32,
        access: Access,
    ) -> Result<u32, (u32, u32)> {
        if self.el1.is_some() {
            let access = match access {
                Access::Read => crate::el0::El0Access::Read,
                Access::Write => crate::el0::El0Access::Write,
                Access::Execute => crate::el0::El0Access::Exec,
            };
            return self.el0_translate(bus, va, access);
        }
        if self.sctlr & 1 == 0 {
            return Ok(va);
        }
        let slot = (va >> 12) as usize % TLB_ENTRIES;
        let entry = if self.tlb[slot].tag == va >> 12 {
            self.tlb[slot]
        } else {
            let entry = self.walk(bus, va).map_err(|status| (va, status))?;
            self.tlb[slot] = entry;
            entry
        };
        self.check(&entry, access).map_err(|status| (va, status))?;
        Ok(entry.pa_page | (va & 0xfff))
    }

    /// Set or clear a debugger's watch range (`emu_core::debug::Watch::set`).
    pub fn set_watch(&mut self, addr: u64, len: u64, kind: u8, insert: bool) {
        self.watch.set(addr, len, kind, insert);
        self.utlb_flush();
    }

    /// Where `va` is mapped, for a debugger: no permission checks and no TLB changes.
    pub fn debug_translate(&self, bus: &mut dyn Bus, va: u32) -> Option<u32> {
        if self.sctlr & 1 == 0 { return Some(va); }
        self.walk(bus, va).ok().map(|e| e.pa_page | (va & 0xfff))
    }

    /// `translate` that also fills the micro-TLB (and notes device accesses).
    pub(crate) fn translate_fill(
        &mut self,
        bus: &mut dyn Bus,
        va: u32,
        access: Access,
    ) -> Result<u32, (u32, u32)> {
        let pa = self.translate(bus, va, access)?;
        let kind = match access {
            Access::Read => U_READ,
            Access::Write => U_WRITE,
            Access::Execute => U_EXEC,
        };
        self.utlb_fill(va, pa, kind);
        Ok(pa)
    }

    pub(crate) fn mem_read8(
        &mut self,
        bus: &mut dyn Bus,
        va: u32,
        access: Access,
    ) -> Result<u8, (u32, u32)> {
        let kind = if access == Access::Execute { U_EXEC } else { U_READ };
        if let Some(p) = self.host(va, 1, kind) {
            // SAFETY: micro-TLB entries point into the guest RAM mapping
            return Ok(unsafe { p.read() });
        }
        if !self.watch.is_empty() && access == Access::Read { self.watch.check(va as u64, 1, emu_core::debug::WATCH_READ); }
        let physical = self.translate_fill(bus, va, access)?;
        bus.read8(physical as u64).map_err(|_| (va, 0x08))
    }

    fn crosses_page(&self, va: u32, size: u32) -> bool {
        self.sctlr & 1 != 0 && (va & 0xfff) + size > 0x1000
    }

    pub(crate) fn mem_read16(&mut self, bus: &mut dyn Bus, va: u32) -> Result<u16, (u32, u32)> {
        if let Some(p) = self.host(va, 2, U_READ) {
            // SAFETY: as in mem_read8; the two bytes are in the same page
            return Ok(unsafe { (p as *const u16).read_unaligned() });
        }
        if !self.watch.is_empty() { self.watch.check(va as u64, 2, emu_core::debug::WATCH_READ); }
        if self.crosses_page(va, 2) {
            let lo = self.mem_read8(bus, va, Access::Read)?;
            let hi = self.mem_read8(bus, va.wrapping_add(1), Access::Read)?;
            return Ok(u16::from_le_bytes([lo, hi]));
        }
        let physical = self.translate_fill(bus, va, Access::Read)?;
        bus.read16(physical as u64).map_err(|_| (va, 0x08))
    }

    pub(crate) fn mem_read32(
        &mut self,
        bus: &mut dyn Bus,
        va: u32,
        access: Access,
    ) -> Result<u32, (u32, u32)> {
        let kind = if access == Access::Execute { U_EXEC } else { U_READ };
        if let Some(p) = self.host(va, 4, kind) {
            // SAFETY: as in mem_read8; the four bytes are in the same page
            return Ok(unsafe { (p as *const u32).read_unaligned() });
        }
        if !self.watch.is_empty() && access == Access::Read { self.watch.check(va as u64, 4, emu_core::debug::WATCH_READ); }
        if self.crosses_page(va, 4) {
            let mut bytes = [0; 4];
            for (index, byte) in bytes.iter_mut().enumerate() {
                *byte = self.mem_read8(bus, va.wrapping_add(index as u32), access)?;
            }
            return Ok(u32::from_le_bytes(bytes));
        }
        let physical = self.translate_fill(bus, va, access)?;
        bus.read32(physical as u64).map_err(|_| (va, 0x08))
    }

    fn invalidate_exclusive(&mut self, va: u32) {
        if self
            .exclusive_addr
            .is_some_and(|base| va.wrapping_sub(base) < self.exclusive_size.max(4))
        {
            self.exclusive_addr = None;
            self.exclusive_size = 0;
        }
    }

    pub(crate) fn mem_write8(
        &mut self,
        bus: &mut dyn Bus,
        va: u32,
        value: u8,
    ) -> Result<(), (u32, u32)> {
        if let Some(p) = self.host(va, 1, U_WRITE) {
            self.invalidate_exclusive(va);
            // SAFETY: as in mem_read8 (write entries never map pages holding compiled code)
            unsafe { p.write(value) };
            return Ok(());
        }
        if !self.watch.is_empty() { self.watch.check(va as u64, 1, emu_core::debug::WATCH_WRITE); }
        let physical = self.translate_fill(bus, va, Access::Write)?;
        self.invalidate_exclusive(va);
        self.code_written(physical);
        bus.write8(physical as u64, value).map_err(|_| (va, 0x08))
    }

    pub(crate) fn mem_write16(
        &mut self,
        bus: &mut dyn Bus,
        va: u32,
        value: u16,
    ) -> Result<(), (u32, u32)> {
        if let Some(p) = self.host(va, 2, U_WRITE) {
            self.invalidate_exclusive(va);
            // SAFETY: as in mem_write8
            unsafe { (p as *mut u16).write_unaligned(value) };
            return Ok(());
        }
        if !self.watch.is_empty() { self.watch.check(va as u64, 2, emu_core::debug::WATCH_WRITE); }
        if self.crosses_page(va, 2) {
            // Check both pages before writing either byte.
            self.translate(bus, va.wrapping_add(1), Access::Write)?;
            for (index, byte) in value.to_le_bytes().into_iter().enumerate() {
                self.mem_write8(bus, va.wrapping_add(index as u32), byte)?;
            }
            return Ok(());
        }
        let physical = self.translate_fill(bus, va, Access::Write)?;
        self.invalidate_exclusive(va);
        self.code_written(physical);
        bus.write16(physical as u64, value).map_err(|_| (va, 0x08))
    }

    pub(crate) fn mem_write32(
        &mut self,
        bus: &mut dyn Bus,
        va: u32,
        value: u32,
    ) -> Result<(), (u32, u32)> {
        if let Some(p) = self.host(va, 4, U_WRITE) {
            self.invalidate_exclusive(va);
            // SAFETY: as in mem_write8
            unsafe { (p as *mut u32).write_unaligned(value) };
            return Ok(());
        }
        if !self.watch.is_empty() { self.watch.check(va as u64, 4, emu_core::debug::WATCH_WRITE); }
        if self.crosses_page(va, 4) {
            self.translate(bus, va, Access::Write)?;
            self.translate(bus, va.wrapping_add(3), Access::Write)?;
            for (index, byte) in value.to_le_bytes().into_iter().enumerate() {
                self.mem_write8(bus, va.wrapping_add(index as u32), byte)?;
            }
            return Ok(());
        }
        let physical = self.translate_fill(bus, va, Access::Write)?;
        self.invalidate_exclusive(va);
        self.code_written(physical);
        bus.write32(physical as u64, value).map_err(|_| (va, 0x08))
    }
}
