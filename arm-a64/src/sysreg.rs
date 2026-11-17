//! System registers. Identity registers describe a Cortex-A72 (BCM2711) reduced to what this
//! emulator implements; everything else that software may write is stored and read back.
use crate::cpu::Cpu;
use std::collections::BTreeMap;

/// Register key: op0:op1:CRn:CRm:op2, the same 16 bits as instruction bits 20..5.
pub const fn key(op0: u32, op1: u32, crn: u32, crm: u32, op2: u32) -> u32 {
    (op0 << 14) | (op1 << 11) | (crn << 7) | (crm << 3) | op2
}

macro_rules! regs { ($($name:ident = $a:literal,$b:literal,$c:literal,$d:literal,$e:literal;)*) => {
    $(pub const $name: u32 = key($a, $b, $c, $d, $e);)*
} }
regs! {
    MIDR_EL1 = 3,0,0,0,0; MPIDR_EL1 = 3,0,0,0,5; REVIDR_EL1 = 3,0,0,0,6;
    ID_AA64PFR0_EL1 = 3,0,0,4,0; ID_AA64DFR0_EL1 = 3,0,0,5,0; ID_AA64ISAR0_EL1 = 3,0,0,6,0;
    ID_AA64MMFR0_EL1 = 3,0,0,7,0; CCSIDR_EL1 = 3,1,0,0,0; CLIDR_EL1 = 3,1,0,0,1;
    CSSELR_EL1 = 3,2,0,0,0; CTR_EL0 = 3,3,0,0,1; DCZID_EL0 = 3,3,0,0,7;
    SCTLR_EL1 = 3,0,1,0,0; CPACR_EL1 = 3,0,1,0,2; SCTLR_EL2 = 3,4,1,0,0; SCTLR_EL3 = 3,6,1,0,0;
    HCR_EL2 = 3,4,1,1,0; SCR_EL3 = 3,6,1,1,0;
    TTBR0_EL1 = 3,0,2,0,0; TTBR1_EL1 = 3,0,2,0,1; TCR_EL1 = 3,0,2,0,2;
    TTBR0_EL2 = 3,4,2,0,0; TCR_EL2 = 3,4,2,0,2; TTBR0_EL3 = 3,6,2,0,0; TCR_EL3 = 3,6,2,0,2;
    SPSR_EL1 = 3,0,4,0,0; ELR_EL1 = 3,0,4,0,1; SP_EL0 = 3,0,4,1,0; SPSEL = 3,0,4,2,0;
    CURRENTEL = 3,0,4,2,2; NZCV = 3,3,4,2,0; DAIF = 3,3,4,2,1; FPCR = 3,3,4,4,0; FPSR = 3,3,4,4,1;
    SPSR_EL2 = 3,4,4,0,0; ELR_EL2 = 3,4,4,0,1; SP_EL1 = 3,4,4,1,0;
    SPSR_EL3 = 3,6,4,0,0; ELR_EL3 = 3,6,4,0,1; SP_EL2 = 3,6,4,1,0;
    ESR_EL1 = 3,0,5,2,0; ESR_EL2 = 3,4,5,2,0; ESR_EL3 = 3,6,5,2,0;
    FAR_EL1 = 3,0,6,0,0; FAR_EL2 = 3,4,6,0,0; FAR_EL3 = 3,6,6,0,0; PAR_EL1 = 3,0,7,4,0;
    MAIR_EL1 = 3,0,10,2,0;
    TPIDR_EL0 = 3,3,13,0,2; TPIDRRO_EL0 = 3,3,13,0,3; TPIDR_EL1 = 3,0,13,0,4;
    VBAR_EL1 = 3,0,12,0,0; VBAR_EL2 = 3,4,12,0,0; VBAR_EL3 = 3,6,12,0,0;
    CNTFRQ_EL0 = 3,3,14,0,0; CNTPCT_EL0 = 3,3,14,0,1; CNTVCT_EL0 = 3,3,14,0,2;
    CNTP_TVAL_EL0 = 3,3,14,2,0; CNTP_CTL_EL0 = 3,3,14,2,1; CNTP_CVAL_EL0 = 3,3,14,2,2;
    CNTV_TVAL_EL0 = 3,3,14,3,0; CNTV_CTL_EL0 = 3,3,14,3,1; CNTV_CVAL_EL0 = 3,3,14,3,2;
    CNTVOFF_EL2 = 3,4,14,0,3;
}

/// One generic timer (EL1 physical or virtual).
#[derive(Clone, Copy, Debug, Default)]
pub struct Timer {
    pub ctl: u64,
    pub cval: u64,
}
impl Timer {
    /// Level of the timer's interrupt output at counter value `now`.
    pub fn firing(&self, now: u64) -> bool {
        self.ctl & 1 != 0 && self.ctl & 2 == 0 && now >= self.cval
    }
}

#[derive(Default)]
pub struct SysRegs {
    pub mpidr: u64,
    pub cpacr_el1: u64,
    pub hcr_el2: u64,
    pub scr_el3: u64,
    pub ttbr0: [u64; 4],
    pub ttbr1_el1: u64,
    pub tcr: [u64; 4],
    pub par_el1: u64,
    pub cntfrq: u64,
    /// physical counter value at the last refresh (see `refresh_counter`)
    pub cnt: u64,
    /// virtual time: `time_base_ns` at retired-instruction count `time_base_instret`, then one
    /// nanosecond per retired instruction (the machine moves the base when it skips idle time)
    pub time_base_ns: u64,
    pub time_base_instret: u64,
    /// a timer register was written: the machine must re-evaluate interrupt lines
    pub timers_changed: bool,
    pub cntvoff: u64,
    pub ptimer: Timer,
    pub vtimer: Timer,
    /// TPIDR_EL0, TPIDRRO_EL0, TPIDR_EL1 (thread pointers: hot, read natively by the JIT)
    pub tpidr: [u64; 3],
    pub other: BTreeMap<u32, u64>,
    /// registers read or written that have no model (first access order), for diagnostics
    pub unknown: Vec<u32>,
}

/// The AArch32 identification registers (ID_PFR0_EL1 .. MVFR2_EL1: op0 3, op1 0, CRn 0,
/// CRm 1-3), as EL1 reads them to describe AArch32 EL0. They describe the Cortex-A72's AArch32
/// state reduced to what `arm-a32` implements: ARMv7-A with the virtualization extensions'
/// integer divide, VFPv4-D32 and Advanced SIMD, but none of ARMv8's AArch32 additions
/// (LDA/STL, VSEL/VRINT/VMAXNM, CRC32, SEVL), no SWP and no ThumbEE.
fn aarch32_id(k: u32) -> Option<u64> {
    // CRm (bits 6:3) 1..3 and op2 (bits 2:0): 0x08..0x1f
    if k & !0x7f != key(3, 0, 0, 0, 0) || !(0x08..0x20).contains(&(k & 0x7f)) {
        return None;
    }
    Some(match k & 0x7f {
        0x08 => 0x0000_0131, // ID_PFR0: A32, T32 (Thumb-2), trivial Jazelle
        0x09 => 0x0001_1011, // ID_PFR1: Security, generic timer, ...
        0x0a => 0x0000_0066, // ID_DFR0: ARMv8 debug (CP14), no PMU, no trace
        0x0c => 0x1020_1105, // ID_MMFR0..3 (as the A72)
        0x0d => 0x4000_0000,
        0x0e => 0x0126_0000,
        0x0f => 0x0210_2211,
        0x10 => 0x0210_1110, // ID_ISAR0: SDIV/UDIV in A32 and T32, no SWP
        0x11 => 0x1311_2111,
        0x12 => 0x2123_2041, // ID_ISAR2: LoadStore 1 (no LDA/STL)
        0x13 => 0x0111_2131,
        0x14 => 0x0001_1142,
        0x18 => 0x1011_0222, // MVFR0: VFPv4 double, D32, Advanced SIMD registers
        0x19 => 0x1111_1111, // MVFR1: Advanced SIMD integer/float/FMA, VFP FMA, FTZ, DN
        _ => 0,              // ID_AFR0, ID_ISAR5, ID_MMFR4, MVFR2, reserved
    })
}

/// Counter ticks at `ns` nanoseconds for frequency `frq` (exact; 54 MHz avoids 128-bit math).
pub fn count_at(ns: u64, frq: u64) -> u64 {
    if frq == 54_000_000 { ns / 500 * 27 + ns % 500 * 27 / 500 } else { (ns as u128 * frq as u128 / 1_000_000_000) as u64 }
}

impl Cpu {
    /// Virtual time now, in ns.
    pub fn now_ns(&self) -> u64 {
        if let Some((t0, off)) = self.wall_clock { return (t0.elapsed().as_nanos() as i64 + off).max(0) as u64; }
        self.sys.time_base_ns + (self.instret - self.sys.time_base_instret)
    }

    /// Bring `sys.cnt` up to the current virtual time.
    pub fn refresh_counter(&mut self) { self.sys.cnt = count_at(self.now_ns(), self.sys.cntfrq); }

    pub(crate) fn read_sysreg(&mut self, k: u32) -> u64 {
        if (k >> 7) & 15 == 14 { self.refresh_counter(); }
        let s = &self.sys;
        match k {
            MIDR_EL1 => self.profile.midr,
            MPIDR_EL1 => 0x8000_0000 | s.mpidr,
            REVIDR_EL1 => 0,
            // EL0 AArch64 and AArch32 (32-bit programs, see aarch32.rs), EL1..EL3 AArch64 only,
            // FP and AdvSIMD implemented, no GIC system registers
            ID_AA64PFR0_EL1 => 0x1112 | if self.profile.gic_sysregs { 1 << 24 } else { 0 },
            ID_AA64DFR0_EL1 => 0x0000_0006,
            // no AES/SHA/CRC32/atomics
            // CRC32 (as the Pi 4's Cortex-A72; no crypto extension on the BCM2711)
            ID_AA64ISAR0_EL1 => (1 << 16) | if self.crypto { (1 << 12) | (1 << 8) | (2 << 4) } else { 0 },
            // 40-bit PA, 16-bit ASID, 4 KiB granule only (16K and 64K not supported)
            ID_AA64MMFR0_EL1 => 0x0f00_0022 | (0xf << 24),
            CTR_EL0 => 0x8444_c004,
            DCZID_EL0 => 4,
            CLIDR_EL1 => 0x0a20_0023,
            CCSIDR_EL1 => 0x701f_e01a,
            SCTLR_EL1 => self.sctlr[1],
            SCTLR_EL2 => self.sctlr[2],
            SCTLR_EL3 => self.sctlr[3],
            CPACR_EL1 => s.cpacr_el1,
            HCR_EL2 => s.hcr_el2,
            SCR_EL3 => s.scr_el3,
            TTBR0_EL1 => s.ttbr0[1],
            TTBR0_EL2 => s.ttbr0[2],
            TTBR0_EL3 => s.ttbr0[3],
            TTBR1_EL1 => s.ttbr1_el1,
            TCR_EL1 => s.tcr[1],
            TCR_EL2 => s.tcr[2],
            TCR_EL3 => s.tcr[3],
            PAR_EL1 => s.par_el1,
            SPSR_EL1 => self.spsr[1],
            SPSR_EL2 => self.spsr[2],
            SPSR_EL3 => self.spsr[3],
            ELR_EL1 => self.elr[1],
            ELR_EL2 => self.elr[2],
            ELR_EL3 => self.elr[3],
            SP_EL0 => self.sp[0],
            TPIDR_EL0 => s.tpidr[0],
            TPIDRRO_EL0 => s.tpidr[1],
            TPIDR_EL1 => s.tpidr[2],
            SP_EL1 => self.sp[1],
            SP_EL2 => self.sp[2],
            SPSEL => self.spsel as u64,
            CURRENTEL => (self.el as u64) << 2,
            NZCV => self.nzcv as u64,
            DAIF => self.daif as u64,
            FPCR => self.fpcr,
            FPSR => self.fpsr,
            ESR_EL1 => self.esr[1],
            ESR_EL2 => self.esr[2],
            ESR_EL3 => self.esr[3],
            FAR_EL1 => self.far[1],
            FAR_EL2 => self.far[2],
            FAR_EL3 => self.far[3],
            VBAR_EL1 => self.vbar[1],
            VBAR_EL2 => self.vbar[2],
            VBAR_EL3 => self.vbar[3],
            CNTFRQ_EL0 => s.cntfrq,
            CNTPCT_EL0 => s.cnt,
            CNTVCT_EL0 => s.cnt.wrapping_sub(s.cntvoff),
            CNTVOFF_EL2 => s.cntvoff,
            CNTP_CTL_EL0 => timer_ctl(s.ptimer, s.cnt),
            CNTP_CVAL_EL0 => s.ptimer.cval,
            CNTP_TVAL_EL0 => s.ptimer.cval.wrapping_sub(s.cnt) as u32 as u64,
            CNTV_CTL_EL0 => timer_ctl(s.vtimer, s.cnt.wrapping_sub(s.cntvoff)),
            CNTV_CVAL_EL0 => s.vtimer.cval,
            CNTV_TVAL_EL0 => {
                s.vtimer.cval.wrapping_sub(s.cnt.wrapping_sub(s.cntvoff)) as u32 as u64
            }
            _ if aarch32_id(k).is_some() => aarch32_id(k).unwrap_or(0),
            _ => {
                if !self.sys.other.contains_key(&k) && !self.sys.unknown.contains(&k) {
                    self.sys.unknown.push(k);
                }
                self.sys.other.get(&k).copied().unwrap_or(0)
            }
        }
    }

    pub(crate) fn write_sysreg(&mut self, k: u32, v: u64) {
        if (k >> 7) & 15 == 14 { self.refresh_counter(); self.sys.timers_changed = true; }
        let flush = matches!(
            k,
            SCTLR_EL1
                | SCTLR_EL2
                | SCTLR_EL3
                | TTBR0_EL2
                | TTBR0_EL3
                | TCR_EL1
                | TCR_EL2
                | TCR_EL3
        );
        let s = &mut self.sys;
        match k {
            MIDR_EL1 | MPIDR_EL1 | REVIDR_EL1 | ID_AA64PFR0_EL1 | ID_AA64DFR0_EL1
            | ID_AA64ISAR0_EL1 | ID_AA64MMFR0_EL1 | CTR_EL0 | DCZID_EL0 | CLIDR_EL1
            | CCSIDR_EL1 | CURRENTEL | CNTPCT_EL0 | CNTVCT_EL0 => {}
            SCTLR_EL1 => self.sctlr[1] = v,
            SCTLR_EL2 => self.sctlr[2] = v,
            SCTLR_EL3 => self.sctlr[3] = v,
            CPACR_EL1 => s.cpacr_el1 = v,
            HCR_EL2 => s.hcr_el2 = v,
            SCR_EL3 => s.scr_el3 = v,
            TTBR0_EL1 => s.ttbr0[1] = v,
            TTBR0_EL2 => s.ttbr0[2] = v,
            TTBR0_EL3 => s.ttbr0[3] = v,
            TTBR1_EL1 => s.ttbr1_el1 = v,
            TCR_EL1 => s.tcr[1] = v,
            TCR_EL2 => s.tcr[2] = v,
            TCR_EL3 => s.tcr[3] = v,
            PAR_EL1 => s.par_el1 = v,
            SPSR_EL1 => self.spsr[1] = v,
            SPSR_EL2 => self.spsr[2] = v,
            SPSR_EL3 => self.spsr[3] = v,
            ELR_EL1 => self.elr[1] = v,
            ELR_EL2 => self.elr[2] = v,
            ELR_EL3 => self.elr[3] = v,
            SP_EL0 => self.sp[0] = v,
            TPIDR_EL0 => s.tpidr[0] = v,
            TPIDRRO_EL0 => s.tpidr[1] = v,
            TPIDR_EL1 => s.tpidr[2] = v,
            SP_EL1 => self.sp[1] = v,
            SP_EL2 => self.sp[2] = v,
            SPSEL => self.spsel = v & 1 != 0,
            NZCV => self.nzcv = (v as u32) & 0xf000_0000,
            DAIF => self.daif = (v as u32) & 0x3c0,
            FPCR => self.fpcr = v,
            FPSR => self.fpsr = v,
            ESR_EL1 => self.esr[1] = v,
            ESR_EL2 => self.esr[2] = v,
            ESR_EL3 => self.esr[3] = v,
            FAR_EL1 => self.far[1] = v,
            FAR_EL2 => self.far[2] = v,
            FAR_EL3 => self.far[3] = v,
            VBAR_EL1 => self.vbar[1] = v & !0x7ff,
            VBAR_EL2 => self.vbar[2] = v & !0x7ff,
            VBAR_EL3 => self.vbar[3] = v & !0x7ff,
            CNTFRQ_EL0 => s.cntfrq = v & 0xffff_ffff,
            CNTVOFF_EL2 => s.cntvoff = v,
            CNTP_CTL_EL0 => s.ptimer.ctl = v & 3,
            CNTP_CVAL_EL0 => s.ptimer.cval = v,
            CNTP_TVAL_EL0 => s.ptimer.cval = s.cnt.wrapping_add(v as u32 as i32 as i64 as u64),
            CNTV_CTL_EL0 => s.vtimer.ctl = v & 3,
            CNTV_CVAL_EL0 => s.vtimer.cval = v,
            CNTV_TVAL_EL0 => {
                s.vtimer.cval = s
                    .cnt
                    .wrapping_sub(s.cntvoff)
                    .wrapping_add(v as u32 as i32 as i64 as u64)
            }
            _ => {
                if !s.other.contains_key(&k) && !s.unknown.contains(&k) {
                    s.unknown.push(k);
                }
                s.other.insert(k, v);
            }
        }
        if flush || (matches!(k, TTBR0_EL1 | TTBR1_EL1) && !crate::mmu::asid_tlb()) {
            self.tlb_flush();
        } else if matches!(k, TTBR0_EL1 | TTBR1_EL1) {
            // the TLB is ASID-tagged (mmu.rs); only the untagged micro-TLBs drop user pages
            self.utlb_ttbr_written();
        }
    }
}

fn timer_ctl(t: Timer, now: u64) -> u64 {
    t.ctl
        | if t.ctl & 1 != 0 && now >= t.cval {
            4
        } else {
            0
        }
}
