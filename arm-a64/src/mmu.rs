//! Stage-1 address translation, 4 KiB granule, for the EL1&0, EL2 and EL3 regimes (no stage 2,
//! no VHE, no hardware Access-flag/dirty management — as on ARMv8.0). A small direct-mapped
//! software TLB caches walks. As on hardware, EL1&0 entries from non-global (nG) descriptors
//! are tagged with their ASID, so TTBR writes (Linux's context switches, and its software PAN
//! on every kernel entry and user access) keep the cache; TLBI and SCTLR/TCR writes flush it.
use crate::cpu::Cpu;
use emu_core::Bus;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Access {
    Read,
    Write,
    Exec,
    ReadUnpriv,
    WriteUnpriv,
}
impl Access {
    pub fn is_write(self) -> bool {
        matches!(self, Access::Write | Access::WriteUnpriv)
    }
}

pub const TLB_SIZE: usize = 1024;

/// `SBC_TLB_ASID=0`: flush on every TTBR write instead of keeping ASID-tagged entries (A/B tests)
pub(crate) fn asid_tlb() -> bool {
    static A: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *A.get_or_init(|| std::env::var("SBC_TLB_ASID").map_or(true, |v| v != "0"))
}

/// Cached translation of one 4 KiB page.
#[derive(Clone, Copy, Default)]
pub struct TlbEntry {
    /// VA page number + 1 (0 = empty), tagged with the regime in the top bits
    tag: u64,
    pa_page: u64,
    /// permission bits: 1 EL0 read, 2 EL0 write, 4 EL0 exec, 8 priv read, 16 priv write, 32 priv exec
    perm: u8,
    /// a global translation (nG clear, or a regime without ASIDs): valid for every ASID
    global: bool,
    /// the ASID a non-global translation belongs to
    asid: u16,
}

const PA_MASK: u64 = 0x0000_ffff_ffff_f000;

/// Fault status codes (ESR DFSC/IFSC).
fn translation_fault(level: u32) -> u32 {
    0b000100 | level
}
fn access_flag_fault(level: u32) -> u32 {
    0b001000 | level
}
fn permission_fault(level: u32) -> u32 {
    0b001100 | level
}
const SEA_ON_WALK: u32 = 0b010100;

use crate::cpu::UT;

impl Cpu {
    pub fn tlb_flush(&mut self) {
        self.utlb = [[(0, 0); UT]; 3];
        self.tlb_blocks = [(u64::MAX, 0); 2];
        self.utlb_asid = self.asid();
        self.utlb_unpriv = 0;
        if let Some(a) = self.a32.as_mut() { a.utlb_invalidate_all(); }
        if let Some(t) = self.tlb.as_mut() {
            t.iter_mut().for_each(|e| e.tag = 0);
        }
    }

    /// TLBI: `va` = u64::MAX drops every cached translation, otherwise those of `va`'s page.
    pub fn tlb_invalidate(&mut self, va: u64) {
        if va == u64::MAX { return self.tlb_flush(); }
        // inside a block that was cached page by page: the TLBI covers all of its pages
        let (lo, hi) = self.tlb_blocks[(va >> 55 & 1) as usize];
        if va >= lo && va < hi { return self.tlb_flush(); }
        let u = (va >> 12) as usize & (UT - 1);
        for t in self.utlb.iter_mut() { t[u] = (0, 0); }
        if let Some(a) = self.a32.as_mut() { a.utlb_invalidate(va & ((1 << 56) - 1)); }
        if let Some(t) = self.tlb.as_mut() { t[(va >> 12) as usize & (TLB_SIZE - 1)].tag = 0; }
    }

    /// Debugger view of virtual address `va`: its physical address in the current translation
    /// regime, ignoring permissions (EL0 contexts are looked up with EL1 privilege).
    pub fn debug_translate(&mut self, bus: &mut dyn Bus, va: u64) -> Option<u64> {
        let el = self.el;
        if el == 0 { self.el = 1; }
        let r = self.translate(bus, va, Access::Read).or_else(|_| self.translate(bus, va, Access::Exec)).ok();
        self.el = el;
        r
    }

    pub(crate) fn translate(
        &mut self,
        bus: &mut dyn Bus,
        va: u64,
        acc: Access,
    ) -> Result<u64, u32> {
        let regime = self.el.max(1);
        if self.sctlr[regime as usize] & 1 == 0 {
            return Ok(va);
        }
        let unpriv = self.el == 0 || matches!(acc, Access::ReadUnpriv | Access::WriteUnpriv);
        let need: u8 = match (acc, unpriv) {
            (Access::Exec, true) => 4,
            (Access::Exec, false) => 32,
            (a, true) if a.is_write() => 2,
            (_, true) => 1,
            (a, false) if a.is_write() => 16,
            _ => 8,
        };
        let tag = ((va >> 12) + 1) | ((regime as u64) << 62);
        let idx = ((va >> 12) as usize) & (TLB_SIZE - 1);
        let asid = if regime == 1 { self.asid() } else { 0 };
        if let Some(t) = self.tlb.as_ref() {
            let e = t[idx];
            if e.tag == tag && e.perm & need != 0 && (e.global || e.asid == asid) {
                return Ok(e.pa_page | (va & 0xfff));
            }
        }
        let (pa_page, perm, level, ng) = self.walk(bus, va, regime)?;
        if perm & need == 0 {
            return Err(permission_fault(level));
        }
        if level < 3 {
            // a block (1 GiB at level 1, 2 MiB at level 2): remember the range it covers
            let size = 1u64 << (12 + 9 * (3 - level));
            let base = va & !(size - 1);
            let b = &mut self.tlb_blocks[(va >> 55 & 1) as usize];
            *b = (b.0.min(base), b.1.max(base.saturating_add(size)));
        }
        let tlb = self
            .tlb
            .get_or_insert_with(|| vec![TlbEntry::default(); TLB_SIZE].into_boxed_slice());
        tlb[idx] = TlbEntry { tag, pa_page, perm, global: regime != 1 || !ng, asid };
        Ok(pa_page | (va & 0xfff))
    }

    /// The current EL1&0 ASID: from TTBR1 or TTBR0 as TCR_EL1.A1 says, 8 or 16 bits (TCR.AS).
    pub(crate) fn asid(&self) -> u16 {
        let tcr = self.sys.tcr[1];
        let ttbr = if tcr >> 22 & 1 != 0 { self.sys.ttbr1_el1 } else { self.sys.ttbr0[1] };
        let a = (ttbr >> 48) as u16;
        if tcr >> 36 & 1 != 0 { a } else { a & 0xff }
    }

    /// TTBR0_EL1 or TTBR1_EL1 was written. The main TLB is ASID-tagged and keeps everything.
    /// The micro-TLBs are untagged (the JIT probes them inline), so their lower-half pages go,
    /// except EL0 entries while the ASID stays that of their address space: Linux's software
    /// PAN switches TTBR0 and the ASID (to the reserved 0) on every kernel entry and user
    /// access, and EL0 entries cannot be used at EL1 anyway (the tag holds the EL). Kernel
    /// (upper-half, global) pages always stay.
    pub(crate) fn utlb_ttbr_written(&mut self) {
        let a = self.asid();
        let other_space = a != 0 && a != self.utlb_asid;
        if other_space {
            self.utlb_asid = a;
            // a 32-bit program's cached pages belong to the address space that was left
            if let Some(a) = self.a32.as_mut() { a.utlb_invalidate_all(); }
        }
        self.utlb_unpriv = if a == self.utlb_asid { 0 } else { 1 << 59 };
        for t in self.utlb.iter_mut() {
            for e in t.iter_mut() {
                // tag = (va >> 12) + 1 | el << 60: bit 43 is VA bit 55, which selects TTBR1
                // (also for user pointers with a top-byte tag)
                if e.0 & (1 << 43) == 0 && (other_space || e.0 >> 60 != 0) { *e = (0, 0); }
            }
        }
    }

    /// Walk the tables for `va`; returns (PA of the page, permission bits, final level, nG).
    fn walk(&mut self, bus: &mut dyn Bus, va: u64, regime: u8) -> Result<(u64, u8, u32, bool), u32> {
        let tcr = self.sys.tcr[regime as usize];
        let (ttbr, tsz) = if regime == 1 {
            let upper = va >> 63 != 0;
            let (tsz, epd, tg) = if upper {
                (((tcr >> 16) & 63) as u32, (tcr >> 23) & 1, (tcr >> 30) & 3)
            } else {
                ((tcr & 63) as u32, (tcr >> 7) & 1, (tcr >> 14) & 3)
            };
            let tg_ok = if upper { tg == 2 } else { tg == 0 };
            if epd != 0 || !tg_ok {
                return Err(translation_fault(0));
            }
            // the bits above the input range must all equal bit 63
            let top = va >> (64 - tsz);
            let expect = if upper { (1u64 << tsz) - 1 } else { 0 };
            if tsz != 0 && top != expect {
                return Err(translation_fault(0));
            }
            (
                if upper {
                    self.sys.ttbr1_el1
                } else {
                    self.sys.ttbr0[1]
                },
                tsz,
            )
        } else {
            let tsz = (tcr & 63) as u32;
            if (tcr >> 14) & 3 != 0 {
                return Err(translation_fault(0));
            }
            if tsz != 0 && va >> (64 - tsz) != 0 {
                return Err(translation_fault(0));
            }
            (self.sys.ttbr0[regime as usize], tsz)
        };
        let insize = 64 - tsz.clamp(16, 39);
        let levels = (insize - 12).div_ceil(9);
        let mut level = 4 - levels;
        let mut table = ttbr & PA_MASK & !1;
        loop {
            let shift = 12 + 9 * (3 - level);
            let idx = (va >> shift) & 0x1ff;
            let desc = bus
                .read64(table + idx * 8)
                .map_err(|_| SEA_ON_WALK | level)?;
            let valid = desc & 1 != 0;
            let is_table = desc & 2 != 0;
            if !valid {
                return Err(translation_fault(level));
            }
            if level < 3 && is_table {
                table = desc & PA_MASK;
                level += 1;
                continue;
            }
            if level == 3 && !is_table {
                return Err(translation_fault(level));
            }
            if level == 0 {
                return Err(translation_fault(level));
            }
            if desc & (1 << 10) == 0 {
                return Err(access_flag_fault(level));
            }
            let block_mask = (1u64 << shift) - 1;
            let pa_page = ((desc & PA_MASK & !block_mask) | (va & block_mask)) & !0xfff;
            let ap = (desc >> 6) & 3;
            let uxn = desc & (1 << 54) != 0;
            let pxn = desc & (1 << 53) != 0;
            let ro = ap & 2 != 0;
            let el0 = ap & 1 != 0;
            let wxn = self.sctlr[regime as usize] & (1 << 19) != 0;
            let mut perm = 0u8;
            if regime == 1 {
                if el0 {
                    perm |= 1;
                    if !ro {
                        perm |= 2;
                    }
                    if !uxn && !(wxn && !ro) {
                        perm |= 4;
                    }
                }
                perm |= 8;
                if !ro {
                    perm |= 16;
                }
                // memory writable at EL0 is never executable at EL1
                if !pxn && !(el0 && !ro) && !(wxn && !ro) {
                    perm |= 32;
                }
            } else {
                perm |= 8;
                if !ro {
                    perm |= 16;
                }
                if !uxn && !(wxn && !ro) {
                    perm |= 32;
                }
            }
            return Ok((pa_page, perm, level, desc & (1 << 11) != 0));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use emu_core::{Bus, FlatRam};

    /// EL1&0 with 4 KiB granule, 39-bit VAs (3 levels), the tables at 0x10000.
    fn machine() -> (Cpu, FlatRam) {
        let mut cpu = Cpu::new();
        let mut ram = FlatRam::new(0, 0x80_0000);
        cpu.el = 1;
        cpu.sctlr[1] = 1;
        cpu.sys.tcr[1] = 25 | (1 << 23); // T0SZ = 25, no TTBR1 walks
        cpu.sys.ttbr0[1] = 0x10000;
        // L1[0] -> L2 at 0x11000; L2[1] = 2 MiB block VA 0x20_0000 -> PA 0x40_0000 (AF, EL1 RW)
        ram.write64(0x10000, 0x11000 | 3).unwrap();
        ram.write64(0x11008, 0x40_0000 | (1 << 10) | 1).unwrap();
        (cpu, ram)
    }

    #[test]
    fn tlbi_of_a_block_drops_all_its_pages() {
        let (mut cpu, mut ram) = machine();
        // two pages of the block, cached as separate entries
        assert_eq!(cpu.translate(&mut ram, 0x20_0000, Access::Read), Ok(0x40_0000));
        assert_eq!(cpu.translate(&mut ram, 0x20_5000, Access::Read), Ok(0x40_5000));
        // remap the block to PA 0x60_0000 and invalidate it with one TLBI at its base, as
        // Linux does for a huge page
        ram.write64(0x11008, 0x60_0000 | (1 << 10) | 1).unwrap();
        cpu.tlb_invalidate(0x20_0000);
        assert_eq!(cpu.translate(&mut ram, 0x20_5000, Access::Read), Ok(0x60_5000));
    }

    #[test]
    fn asid_tagged_entries_survive_ttbr_writes() {
        let (mut cpu, mut ram) = machine();
        // non-global page mappings: L2[2] -> L3 at 0x12000, L3[0] = VA 0x40_0000 -> PA 0x50_0000
        ram.write64(0x11010, 0x12000 | 3).unwrap();
        ram.write64(0x12000, 0x50_0000 | (1 << 11) | (1 << 10) | 3).unwrap();
        cpu.sys.ttbr0[1] = 0x10000 | (5 << 48); // ASID 5 (TCR.A1 = 0: ASID from TTBR0)
        assert_eq!(cpu.translate(&mut ram, 0x40_0000, Access::Read), Ok(0x50_0000));
        // another address space (ASID 6) maps nothing there: its walk faults, ASID 5's entry
        // must not answer for it
        cpu.write_sysreg(crate::sysreg::TTBR0_EL1, 0x20000 | (6 << 48));
        assert!(cpu.translate(&mut ram, 0x40_0000, Access::Read).is_err());
        // back to ASID 5 without a TLBI: the entry is still valid
        ram.write64(0x12000, 0).unwrap(); // a walk would fault now
        cpu.write_sysreg(crate::sysreg::TTBR0_EL1, 0x10000 | (5 << 48));
        assert_eq!(cpu.translate(&mut ram, 0x40_0000, Access::Read), Ok(0x50_0000));
    }
}
