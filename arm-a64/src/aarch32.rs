//! AArch32 at EL0: 32-bit user programs under the 64-bit kernel, as the Cortex-A72 runs them.
//!
//! An exception return to EL0 with SPSR.M[4] set hands the user registers to an `arm-a32` core
//! (`enter_aarch32`). It runs the program, translating through this core's EL1&0 regime and
//! asking it for the CP15 registers EL0 may use. Every exception leaves AArch32 again: the
//! registers come back (`leave_aarch32`, the architectural mapping: R0-R14 in W0-W14, the D
//! registers in V0-V15, FPSCR split into FPSR and FPCR) and the exception is taken to EL1 at
//! the "lower EL using AArch32" vectors (offset 0x600) with an AArch32 syndrome.
use crate::cpu::Cpu;
use crate::mmu::Access;
use crate::sysreg::{self, key};
use arm_a32::{Cp15, El0Access, El0Event, El1Host};
use emu_core::{Bus, Trap};

/// Exception classes of AArch32 EL0.
const EC_CP15_32: u32 = 0x03;
const EC_CP15_64: u32 = 0x04;
const EC_SVC32: u32 = 0x11;
const EC_BKPT32: u32 = 0x38;

const CNTKCTL_EL1: u32 = key(3, 0, 14, 1, 0);

/// Snapshot key (not a system register encoding: those fit in 16 bits) for the AArch32 CPSR
/// while a 32-bit program runs; its other registers are saved in their AArch64 places.
pub(crate) const SNAP_AARCH32_CPSR: u32 = 0x1_0000;

/// SPSR bits an AArch32 return restores into the CPSR: everything but J, SS and IL.
const CPSR_FROM_SPSR: u32 = !((1 << 24) | (1 << 21) | (1 << 20));

/// The 64-bit core as the 32-bit core's EL1.
struct El1<'a>(&'a mut Cpu);

impl El1Host for El1<'_> {
    fn translate(&mut self, bus: &mut dyn Bus, va: u32, access: El0Access) -> Result<u64, u32> {
        let a = match access {
            El0Access::Read => Access::Read,
            El0Access::Write => Access::Write,
            El0Access::Exec => Access::Exec,
        };
        self.0.translate(bus, va as u64, a)
    }

    fn cp15(&mut self, insn: u32, value: u64) -> Cp15 {
        let c = &mut *self.0;
        let read = insn & (1 << 20) != 0;
        let cntkctl = c.sys.other.get(&CNTKCTL_EL1).copied().unwrap_or(0);
        // CNTKCTL_EL1: EL0PCTEN (0), EL0VCTEN (1), EL0VTEN (8), EL0PTEN (9)
        let (pct, vct, vt, pt) = (cntkctl & 1 != 0, cntkctl & 2 != 0, cntkctl >> 8 & 1 != 0, cntkctl >> 9 & 1 != 0);
        let timer = |allowed: bool, c: &mut Cpu, k: u32| -> Cp15 {
            if !allowed {
                return Cp15::Trap;
            }
            if read {
                Cp15::Read(c.read_sysreg(k))
            } else {
                c.write_sysreg(k, value);
                Cp15::Done
            }
        };
        if insn & 0x0fe0_0000 == 0x0c40_0000 {
            // MRRC/MCRR: opc1, CRm
            return match ((insn >> 4) & 0xf, insn & 0xf) {
                (0, 14) if read => timer(pct, c, sysreg::CNTPCT_EL0),
                (1, 14) if read => timer(vct, c, sysreg::CNTVCT_EL0),
                (2, 14) => timer(pt, c, sysreg::CNTP_CVAL_EL0),
                (3, 14) => timer(vt, c, sysreg::CNTV_CVAL_EL0),
                _ => Cp15::Undefined,
            };
        }
        let (opc1, crn, crm, opc2) = ((insn >> 21) & 7, (insn >> 16) & 0xf, insn & 0xf, (insn >> 5) & 7);
        match (opc1, crn, crm, opc2) {
            // TPIDRURW is TPIDR_EL0[31:0]; TPIDRURO is TPIDRRO_EL0[31:0], read-only at EL0
            (0, 13, 0, 2) if read => Cp15::Read(c.sys.tpidr[0] & 0xffff_ffff),
            (0, 13, 0, 2) => {
                c.sys.tpidr[0] = (c.sys.tpidr[0] & !0xffff_ffff) | (value & 0xffff_ffff);
                Cp15::Done
            }
            (0, 13, 0, 3) if read => Cp15::Read(c.sys.tpidr[1] & 0xffff_ffff),
            (0, 14, 0, 0) if read => timer(pct || vct, c, sysreg::CNTFRQ_EL0),
            (0, 14, 2, 0) => timer(pt, c, sysreg::CNTP_TVAL_EL0),
            (0, 14, 2, 1) => timer(pt, c, sysreg::CNTP_CTL_EL0),
            (0, 14, 3, 0) => timer(vt, c, sysreg::CNTV_TVAL_EL0),
            (0, 14, 3, 1) => timer(vt, c, sysreg::CNTV_CTL_EL0),
            // CP15ISB, CP15DSB, CP15DMB: only while SCTLR_EL1.CP15BEN allows them
            (0, 7, 5, 4) | (0, 7, 10, 4) | (0, 7, 10, 5) if !read => {
                if c.sctlr[1] & (1 << 5) == 0 {
                    return Cp15::Undefined;
                }
                if c.mt {
                    std::sync::atomic::fence(std::sync::atomic::Ordering::SeqCst);
                }
                Cp15::Done
            }
            _ => Cp15::Undefined,
        }
    }
}

impl Cpu {
    /// Exception return from EL1 to AArch32 EL0: `spsr` (M = User, bit 4 set) and `elr`.
    pub(crate) fn enter_aarch32(&mut self, spsr: u64, elr: u64) {
        let (ram, len, mt) = (self.ram, self.ram_len, self.mt);
        let a = self.a32.get_or_insert_with(|| Box::new(arm_a32::Cpu::new_el0()));
        // SAFETY: the machine's guarantee for `ram` (see the field) covers this core too
        unsafe { a.el0_setup(ram, len, mt) };
        for (r, x) in a.r.iter_mut().zip(self.x.iter()).take(15) {
            *r = *x as u32;
        }
        let cpsr = spsr as u32 & CPSR_FROM_SPSR;
        // the T bit selects the instruction set; the address is aligned to it
        a.r[15] = elr as u32 & if cpsr & (1 << 5) != 0 { !1 } else { !3 };
        a.cpsr = cpsr;
        for n in 0..16 {
            a.vfp.d[2 * n] = self.v[n] as u64;
            a.vfp.d[2 * n + 1] = (self.v[n] >> 64) as u64;
        }
        a.vfp.fpscr = (self.fpsr | self.fpcr) as u32;
        a.exclusive_addr = None;
        self.pc = a.r[15] as u64;
        self.aarch32 = true;
        self.el = 0;
        self.spsel = false;
        self.nzcv = cpsr & 0xf000_0000;
        self.daif = cpsr & 0x1c0;
        self.chain_stop = true;
        self.interp_run = false;
    }

    /// Back to AArch64 for an exception from AArch32 EL0: the registers return to their
    /// AArch64 places; the result is the SPSR to save.
    pub(crate) fn leave_aarch32(&mut self) -> u64 {
        let a = self.a32.as_ref().expect("AArch32 state");
        for (x, r) in self.x.iter_mut().zip(a.r.iter()).take(15) {
            *x = *r as u64;
        }
        for n in 0..16 {
            self.v[n] = a.vfp.d[2 * n] as u128 | (a.vfp.d[2 * n + 1] as u128) << 64;
        }
        let fpscr = a.vfp.fpscr as u64;
        self.fpsr = fpscr & 0xf800_009f;
        self.fpcr = fpscr & 0x07ff_9f00;
        self.pc = a.r[15] as u64;
        self.nzcv = a.cpsr & 0xf000_0000;
        self.aarch32 = false;
        (a.cpsr & CPSR_FROM_SPSR) as u64
    }

    /// One instruction of the 32-bit program (interrupts were checked by `step`).
    pub(crate) fn step_aarch32(&mut self, bus: &mut dyn Bus) -> Result<(), Trap> {
        let mut a = self.a32.take().expect("AArch32 state");
        let before = a.instret;
        let r = a.step_el0(bus, &mut El1(self));
        self.instret += a.instret.wrapping_sub(before);
        self.pc = a.r[15] as u64;
        let exit = a.take_el0_exit();
        self.last_wfe = a.last_wfe;
        self.a32 = Some(a);
        let Some(exit) = exit else {
            return r;
        };
        let pc = exit.pc as u64;
        let (ec, iss, ret, far) = match exit.event {
            El0Event::Undefined => (crate::cpu::Exc::UNKNOWN, 0, pc, None),
            El0Event::Svc(imm) => (EC_SVC32, imm & 0xffff, pc + exit.size as u64, None),
            El0Event::Bkpt(imm) => {
                if self.brk_to_host {
                    return Err(Trap::Brk(imm));
                }
                (EC_BKPT32, imm as u32, pc, None)
            }
            El0Event::PrefetchAbort { va, fsc } => (crate::cpu::Exc::IABT_LOWER, fsc, pc, Some(va as u64)),
            El0Event::DataAbort { va, fsc, write } => {
                (crate::cpu::Exc::DABT_LOWER, fsc | u32::from(write) << 6, pc, Some(va as u64))
            }
            El0Event::Cp15Trap(insn) => {
                // CV = 1 with the condition, then the operands and the direction (1 = read)
                let (rt, rt2, read) = ((insn >> 12) & 0xf, (insn >> 16) & 0xf, (insn >> 20) & 1);
                let head = 1 << 24 | (insn >> 28) << 20;
                if insn & 0x0fe0_0000 == 0x0c40_0000 {
                    let opc1 = (insn >> 4) & 0xf;
                    (EC_CP15_64, head | opc1 << 16 | rt2 << 10 | rt << 5 | (insn & 0xf) << 1 | read, pc, None)
                } else {
                    let (opc1, crn, crm, opc2) = ((insn >> 21) & 7, (insn >> 16) & 0xf, insn & 0xf, (insn >> 5) & 7);
                    (EC_CP15_32, head | opc2 << 17 | opc1 << 14 | crn << 10 | rt << 5 | crm << 1 | read, pc, None)
                }
            }
        };
        // the 32-bit core already moved its PC on for a completed SVC; ELR says where to return
        let t = self.sync_exception_to(1, ec, iss, ret, far);
        if exit.size == 2 && matches!(exit.event, El0Event::Svc(_) | El0Event::Bkpt(_)) {
            self.esr[1] &= !(1 << 25); // IL = 0: a 16-bit instruction
        }
        Err(t)
    }
}
