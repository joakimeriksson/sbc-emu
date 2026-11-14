//! Stage-1 address translation, 4 KiB granule, for the EL1&0, EL2 and EL3 regimes (no stage 2,
//! no VHE, no hardware Access-flag/dirty management — as on ARMv8.0). A small direct-mapped
//! software TLB caches walks; it is flushed by TLBI and by writes to SCTLR/TTBR/TCR.
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

/// Cached translation of one 4 KiB page.
#[derive(Clone, Copy, Default)]
pub struct TlbEntry {
    /// VA page number + 1 (0 = empty), tagged with the regime in the top bits
    tag: u64,
    pa_page: u64,
    /// permission bits: 1 EL0 read, 2 EL0 write, 4 EL0 exec, 8 priv read, 16 priv write, 32 priv exec
    perm: u8,
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
        if let Some(t) = self.tlb.as_mut() {
            t.iter_mut().for_each(|e| e.tag = 0);
        }
    }

    /// TLBI: `va` = u64::MAX drops every cached translation, otherwise those of `va`'s page.
    pub fn tlb_invalidate(&mut self, va: u64) {
        if va == u64::MAX { return self.tlb_flush(); }
        let u = (va >> 12) as usize & (UT - 1);
        for t in self.utlb.iter_mut() { t[u] = (0, 0); }
        if let Some(t) = self.tlb.as_mut() { t[(va >> 12) as usize & (TLB_SIZE - 1)].tag = 0; }
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
        if let Some(t) = self.tlb.as_ref() {
            let e = t[idx];
            if e.tag == tag && e.perm & need != 0 {
                return Ok(e.pa_page | (va & 0xfff));
            }
        }
        let (pa_page, perm, level) = self.walk(bus, va, regime)?;
        if perm & need == 0 {
            return Err(permission_fault(level));
        }
        let tlb = self
            .tlb
            .get_or_insert_with(|| vec![TlbEntry::default(); TLB_SIZE].into_boxed_slice());
        tlb[idx] = TlbEntry { tag, pa_page, perm };
        Ok(pa_page | (va & 0xfff))
    }

    /// Walk the tables for `va`; returns (PA of the page, permission bits, final level).
    fn walk(&mut self, bus: &mut dyn Bus, va: u64, regime: u8) -> Result<(u64, u8, u32), u32> {
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
            return Ok((pa_page, perm, level));
        }
    }
}
