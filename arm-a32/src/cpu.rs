use crate::mmu::{Access, TlbEntry, TLB_ENTRIES};
use emu_core::{Bus, Core, Trap};

pub const MODE_USR: u32 = 0x10;
pub const MODE_FIQ: u32 = 0x11;
pub const MODE_IRQ: u32 = 0x12;
pub const MODE_SVC: u32 = 0x13;
pub const MODE_ABT: u32 = 0x17;
pub const MODE_UND: u32 = 0x1b;
pub const MODE_SYS: u32 = 0x1f;

const N: u32 = 1 << 31;
const Z: u32 = 1 << 30;
const C: u32 = 1 << 29;
const V: u32 = 1 << 28;
const I: u32 = 1 << 7;
const F: u32 = 1 << 6;
const T: u32 = 1 << 5;

/// A Cortex-A7-visible A32 state. The register banks are indexed by exception mode;
/// USR and SYS share the SYS bank. FIQ's r8-r12 banks are not needed by this first slice.
pub struct Cpu {
    pub r: [u32; 16],
    pub cpsr: u32,
    banked_sp: [u32; 32],
    banked_lr: [u32; 32],
    banked_spsr: [u32; 32],
    pub irq: bool,
    pub fiq: bool,
    pub instret: u64,
    pub last_wfe: bool,
    /// Address and size (2 or 4) of the instruction being executed: exception return
    /// addresses are computed from these in both instruction sets.
    pub(crate) cur_pc: u32,
    pub(crate) cur_size: u32,
    /// Set by any PC write while executing a Thumb instruction.
    pub(crate) pc_written: bool,
    /// Set when an instruction restored CPSR from SPSR (exception return).
    pub(crate) psr_restored: bool,
    /// The event register (WFE/SEV).
    pub event: bool,
    /// Set by SEV; the machine delivers the event to the other cores and clears it.
    pub sent_event: bool,
    /// Route SMC to the host as a firmware call (`Trap::HostCall`).
    pub firmware_calls_to_host: bool,
    pub midr: u32,
    pub mpidr: u32,
    pub vbar: u32,
    pub cntfrq: u32,
    pub cntkctl: u32,
    pub cntpct: u64,
    pub cntp_ctl: u32,
    pub cntp_cval: u64,
    pub cntv_ctl: u32,
    pub cntv_cval: u64,
    /// FIQ-banked r8-r12 and the r8-r12 shared by every other mode.
    banked_fiq_r8: [u32; 5],
    banked_usr_r8: [u32; 5],
    pub exclusive_addr: Option<u32>,
    pub exclusive_size: u32,
    pub sctlr: u32,
    pub actlr: u32,
    pub cpacr: u32,
    pub ttbr0: u32,
    pub ttbr1: u32,
    pub ttbcr: u32,
    pub dacr: u32,
    pub dfsr: u32,
    pub ifsr: u32,
    pub dfar: u32,
    pub ifar: u32,
    pub fcseidr: u32,
    pub contextidr: u32,
    pub tpidrurw: u32,
    pub tpidrro: u32,
    pub tpidrprw: u32,
    pub prrr: u32,
    pub nmrr: u32,
    pub csselr: u32,
    pub teecr: u32,
    pub teehbr: u32,
    pub(crate) tlb: Vec<TlbEntry>,
    pub vfp: crate::vfp::Vfp,
    /// Take the architectural undefined-instruction exception for encodings the
    /// emulator does not implement (Linux), instead of stopping with `Trap::Unimplemented`.
    pub undef_to_guest: bool,
    /// The last encoding that was delivered to the guest as UNDEFINED: (pc, insn).
    pub last_undef: Option<(u32, u32)>,
}

impl Default for Cpu {
    fn default() -> Self {
        Self::new()
    }
}

impl Cpu {
    /// Reset in SVC mode with IRQ/FIQ masked and A32 execution selected.
    pub fn new() -> Self {
        Self {
            r: [0; 16],
            cpsr: I | F | MODE_SVC,
            banked_sp: [0; 32],
            banked_lr: [0; 32],
            banked_spsr: [0; 32],
            irq: false,
            fiq: false,
            instret: 0,
            last_wfe: false,
            event: false,
            cur_pc: 0,
            cur_size: 4,
            pc_written: false,
            psr_restored: false,
            sent_event: false,
            firmware_calls_to_host: false,
            midr: 0x410f_c075,
            mpidr: 0x8000_0000,
            vbar: 0,
            cntfrq: 24_000_000,
            cntkctl: 0,
            cntpct: 0,
            cntp_ctl: 0,
            cntp_cval: 0,
            cntv_ctl: 0,
            cntv_cval: 0,
            banked_fiq_r8: [0; 5],
            banked_usr_r8: [0; 5],
            exclusive_addr: None,
            exclusive_size: 0,
            sctlr: 0x00c5_0078,
            actlr: 0,
            cpacr: 0,
            ttbr0: 0,
            ttbr1: 0,
            ttbcr: 0,
            dacr: 0,
            dfsr: 0,
            ifsr: 0,
            dfar: 0,
            ifar: 0,
            fcseidr: 0,
            contextidr: 0,
            tpidrurw: 0,
            tpidrro: 0,
            tpidrprw: 0,
            prrr: 0,
            nmrr: 0,
            csselr: 0,
            teecr: 0,
            teehbr: 0,
            tlb: vec![TlbEntry::INVALID; TLB_ENTRIES],
            vfp: Default::default(),
            undef_to_guest: false,
            last_undef: None,
        }
    }

    /// The SPSR of the current mode (diagnostics).
    pub fn spsr(&self) -> u32 {
        self.banked_spsr[Self::bank(self.cpsr)]
    }

    /// Advance the architectural counter by virtual 24 MHz ticks.
    pub fn advance_time(&mut self, ticks: u64) {
        self.cntpct = self.cntpct.wrapping_add(ticks);
    }

    fn physical_timer_reached(&self) -> bool {
        self.cntpct.wrapping_sub(self.cntp_cval) as i64 >= 0
    }

    fn physical_timer_control(&self) -> u32 {
        self.cntp_ctl | u32::from(self.physical_timer_reached()) << 2
    }

    fn virtual_timer_control(&self) -> u32 {
        let reached = self.cntpct.wrapping_sub(self.cntv_cval) as i64 >= 0;
        self.cntv_ctl | u32::from(reached) << 2
    }

    /// Virtual timer output (CNTVOFF is zero: the virtual count equals the physical one).
    pub fn virtual_timer_irq(&self) -> bool {
        self.cntv_ctl & 1 != 0
            && self.cntv_ctl & 2 == 0
            && self.cntpct.wrapping_sub(self.cntv_cval) as i64 >= 0
    }

    pub fn physical_timer_irq(&self) -> bool {
        self.cntp_ctl & 1 != 0 && self.cntp_ctl & 2 == 0 && self.physical_timer_reached()
    }

    fn ccsidr(&self) -> u32 {
        match self.csselr & 0xf {
            0 => 0x701f_e00a,     // 32 KiB L1 data cache
            1 => 0x201f_e00a,     // 32 KiB L1 instruction cache
            2 | 3 => 0x711f_e07a, // 4 MiB L2 unified cache
            _ => 0,
        }
    }

    fn bank(mode: u32) -> usize {
        match mode & 0x1f {
            MODE_USR => MODE_SYS as usize,
            m @ (MODE_FIQ | MODE_IRQ | MODE_SVC | MODE_ABT | MODE_UND | MODE_SYS) => m as usize,
            _ => MODE_SYS as usize,
        }
    }

    fn change_mode(&mut self, new: u32) {
        let old = self.cpsr & 0x1f;
        if old == new {
            return;
        }
        let (ob, nb) = (Self::bank(old), Self::bank(new));
        if (old == MODE_FIQ) != (new == MODE_FIQ) {
            let (save, load) = if old == MODE_FIQ {
                (&mut self.banked_fiq_r8, &self.banked_usr_r8)
            } else {
                (&mut self.banked_usr_r8, &self.banked_fiq_r8)
            };
            save.copy_from_slice(&self.r[8..13]);
            self.r[8..13].copy_from_slice(load);
        }
        self.banked_sp[ob] = self.r[13];
        self.banked_lr[ob] = self.r[14];
        self.r[13] = self.banked_sp[nb];
        self.r[14] = self.banked_lr[nb];
        self.cpsr = (self.cpsr & !0x1f) | new;
    }

    /// User-mode view of r8-r14 for LDM/STM with the S bit and no PC.
    fn user_reg(&self, n: usize) -> u32 {
        let mode = self.cpsr & 0x1f;
        match n {
            8..=12 if mode == MODE_FIQ => self.banked_usr_r8[n - 8],
            13 if !matches!(mode, MODE_USR | MODE_SYS) => self.banked_sp[MODE_SYS as usize],
            14 if !matches!(mode, MODE_USR | MODE_SYS) => self.banked_lr[MODE_SYS as usize],
            _ => self.r[n],
        }
    }

    fn set_user_reg(&mut self, n: usize, value: u32) {
        let mode = self.cpsr & 0x1f;
        match n {
            8..=12 if mode == MODE_FIQ => self.banked_usr_r8[n - 8] = value,
            13 if !matches!(mode, MODE_USR | MODE_SYS) => self.banked_sp[MODE_SYS as usize] = value,
            14 if !matches!(mode, MODE_USR | MODE_SYS) => self.banked_lr[MODE_SYS as usize] = value,
            _ => self.r[n] = value,
        }
    }

    fn read_reg(&self, n: u32) -> u32 {
        // ARM: execute() has already advanced r15 by four, so reads see the instruction
        // address + 8. Thumb: r15 holds the instruction address while it executes, so
        // reads see address + 4. Never compute PC-relative values from self.r[15] directly.
        if n == 15 {
            self.r[15].wrapping_add(4)
        } else {
            self.r[n as usize]
        }
    }

    pub(crate) fn condition(&self, cond: u32) -> bool {
        let n = self.cpsr & N != 0;
        let z = self.cpsr & Z != 0;
        let c = self.cpsr & C != 0;
        let v = self.cpsr & V != 0;
        match cond {
            0 => z,
            1 => !z,
            2 => c,
            3 => !c,
            4 => n,
            5 => !n,
            6 => v,
            7 => !v,
            8 => c && !z,
            9 => !c || z,
            10 => n == v,
            11 => n != v,
            12 => !z && n == v,
            13 => z || n != v,
            14 => true,
            _ => false,
        }
    }

    fn set_nz(&mut self, value: u32) {
        self.cpsr = (self.cpsr & !(N | Z)) | (value & N) | u32::from(value == 0) * Z;
    }

    pub(crate) fn set_logic_flags(&mut self, value: u32, carry: bool) {
        self.set_nz(value);
        self.cpsr = (self.cpsr & !C) | u32::from(carry) * C;
    }

    pub(crate) fn add_with_carry(a: u32, b: u32, carry: bool) -> (u32, bool, bool) {
        let wide = a as u64 + b as u64 + carry as u64;
        let result = wide as u32;
        let overflow = (a ^ result) & (b ^ result) & N != 0;
        (result, wide >> 32 != 0, overflow)
    }

    pub(crate) fn set_add_flags(&mut self, a: u32, b: u32, result: u32, carry: bool, overflow: bool) {
        let _ = (a, b);
        self.set_nz(result);
        self.cpsr = (self.cpsr & !(C | V)) | u32::from(carry) * C | u32::from(overflow) * V;
    }

    fn write_psr(&mut self, spsr: bool, fields: u32, value: u32) -> bool {
        let mut mask = 0;
        if fields & 1 != 0 {
            mask |= 0x0000_00ff;
        }
        if fields & 2 != 0 {
            mask |= 0x0000_ff00;
        }
        if fields & 4 != 0 {
            mask |= 0x00ff_0000;
        }
        if fields & 8 != 0 {
            mask |= 0xff00_0000;
        }
        if spsr {
            let bank = Self::bank(self.cpsr);
            let old = self.banked_spsr[bank];
            self.banked_spsr[bank] = (old & !mask) | (value & mask);
            return true;
        }
        if self.cpsr & 0x1f == MODE_USR {
            mask &= 0xf80f_0000; // User mode may only write NZCVQ and GE.
        }
        mask &= !(T | 0x0600_fc00); // MSR does not write the execution state bits
        let updated = (self.cpsr & !mask) | (value & mask);
        self.change_mode(updated & 0x1f);
        self.cpsr = updated;
        true
    }

    pub(crate) fn restore_spsr(&mut self) {
        let restored = self.banked_spsr[Self::bank(self.cpsr)];
        self.change_mode(restored & 0x1f);
        self.cpsr = restored;
        self.psr_restored = true;
    }

    /// BranchWritePC: stay in the current instruction set.
    pub(crate) fn branch_to(&mut self, target: u32) {
        self.r[15] = if self.cpsr & T != 0 { target & !1 } else { target & !3 };
        self.pc_written = true;
    }

    /// BXWritePC (BX, BLX, and in ARMv7 also LDR/LDM/POP of the PC and ARM-state ALU
    /// writes): bit 0 selects Thumb.
    pub(crate) fn bx_to(&mut self, target: u32) {
        if target & 1 != 0 {
            self.cpsr |= T;
            self.r[15] = target & !1;
        } else {
            self.cpsr &= !T;
            self.r[15] = target & !3;
        }
        self.pc_written = true;
    }

    /// Return address after this instruction (LR for BL/BLX in either state).
    pub(crate) fn next_pc(&self) -> u32 {
        self.cur_pc.wrapping_add(self.cur_size)
    }

    pub(crate) fn thumb(&self) -> bool {
        self.cpsr & T != 0
    }

    pub(crate) fn shift(value: u32, kind: u32, amount: u32, carry: bool, immediate: bool) -> (u32, bool) {
        if !immediate && amount == 0 {
            return (value, carry);
        }
        match kind {
            0 => match amount {
                0 => (value, carry),
                1..=31 => (value << amount, value & (1 << (32 - amount)) != 0),
                32 => (0, value & 1 != 0),
                _ => (0, false),
            },
            1 => match amount {
                0 if immediate => (0, value & N != 0),
                0 => (value, carry),
                1..=31 => (value >> amount, value & (1 << (amount - 1)) != 0),
                32 => (0, value & N != 0),
                _ => (0, false),
            },
            2 => match amount {
                0 if immediate => (((value as i32) >> 31) as u32, value & N != 0),
                0 => (value, carry),
                1..=31 => (
                    ((value as i32) >> amount) as u32,
                    value & (1 << (amount - 1)) != 0,
                ),
                _ => (((value as i32) >> 31) as u32, value & N != 0),
            },
            _ => {
                if immediate && amount == 0 {
                    ((carry as u32) << 31 | value >> 1, value & 1 != 0)
                } else {
                    let amount = amount & 31;
                    if amount == 0 {
                        (value, value & N != 0)
                    } else {
                        let result = value.rotate_right(amount);
                        (result, result & N != 0)
                    }
                }
            }
        }
    }

    fn enter_exception(&mut self, vector: u32, return_addr: u32, mode: u32, mask_fiq: bool) {
        let old = self.cpsr;
        self.change_mode(mode);
        self.banked_spsr[Self::bank(mode)] = old;
        self.r[14] = return_addr;
        // Handlers run in Thumb state when SCTLR.TE is set (Thumb-2 kernels).
        let thumb_handlers = if self.sctlr & (1 << 30) != 0 { T } else { 0 };
        self.cpsr = (self.cpsr & !(T | F | 0x0600_fc00)) | I | thumb_handlers
            | if mask_fiq { F } else { 0 };
        self.pc_written = true;
        let base = if self.sctlr & (1 << 13) != 0 {
            0xffff_0000
        } else {
            self.vbar
        };
        self.r[15] = base.wrapping_add(vector);
    }

    pub(crate) fn prefetch_abort(&mut self, va: u32, status: u32) -> Trap {
        self.ifsr = status;
        self.ifar = va;
        self.enter_exception(0x0c, self.cur_pc.wrapping_add(4), MODE_ABT, true);
        Trap::Exception(0x21, 1)
    }

    pub(crate) fn data_abort(&mut self, va: u32, status: u32, write: bool) -> Trap {
        self.dfsr = status | u32::from(write) << 11;
        self.dfar = va;
        self.enter_exception(0x10, self.cur_pc.wrapping_add(8), MODE_ABT, true);
        Trap::Exception(0x25, 1)
    }

    pub(crate) fn execute(&mut self, bus: &mut dyn Bus, pc: u64, insn: u32) -> Result<(), Trap> {
        let unimplemented = || Trap::Unimplemented(pc, insn);
        if insn >> 28 == 15 {
            if insn == 0xf57f_f01f {
                self.exclusive_addr = None;
                self.exclusive_size = 0;
                return Ok(());
            }
            // CPS/CPSIE/CPSID: cccc=1111 0001 0000 imod M 0 0000000 A I F 0 mode.
            if insn & 0x0ff1_fe20 == 0x0100_0000 {
                let imod = (insn >> 18) & 3;
                let change_mode = insn & (1 << 17) != 0;
                let mode = insn & 0x1f;
                if imod == 1
                    || imod == 0 && !change_mode
                    || change_mode
                        && !matches!(
                            mode,
                            MODE_USR | MODE_FIQ | MODE_IRQ | MODE_SVC | MODE_ABT | MODE_UND | MODE_SYS
                        )
                {
                    return Err(unimplemented());
                }
                if self.cpsr & 0x1f == MODE_USR {
                    return Ok(()); // CPS is a NOP in User mode.
                }
                let mask = insn & 0x1c0; // A, I, F map onto CPSR bits 8:6
                if imod == 2 {
                    self.cpsr &= !mask;
                } else if imod == 3 {
                    self.cpsr |= mask;
                }
                if change_mode {
                    self.change_mode(mode);
                }
                return Ok(());
            }
            // BLX immediate: call into Thumb code.
            if insn & 0x0e00_0000 == 0x0a00_0000 {
                let offset = ((((insn & 0x00ff_ffff) << 8) as i32 >> 6) as u32) | (insn >> 23) & 2;
                let target = self.read_reg(15).wrapping_add(offset);
                self.r[14] = self.next_pc();
                self.bx_to(target | 1);
                return Ok(());
            }
            // DMB/DSB/ISB are no-ops with synchronous devices and direct instruction fetch.
            if insn & 0x0fff_fff0 == 0x057f_f040
                || insn & 0x0fff_fff0 == 0x057f_f050
                || insn == 0xf57f_f06f
            {
                return Ok(());
            }
            // Immediate PLD/PLDW hints need no cache model.
            if insn & 0x0f70_f000 == 0x0550_f000 || insn & 0x0f70_f000 == 0x0510_f000 {
                return Ok(());
            }
            // Register PLD/PLDW and both PLI forms.
            if matches!(insn & 0x0f70_f010, 0x0750_f000 | 0x0710_f000 | 0x0650_f000)
                || insn & 0x0f70_f000 == 0x0450_f000
            {
                return Ok(());
            }
            return Err(unimplemented());
        }
        if !self.condition(insn >> 28) {
            return Ok(());
        }

        if insn & 0x0ff0_00f0 == 0x07f0_00f0 {
            return Err(unimplemented()); // permanently undefined (UDF)
        }
        if let Some(result) = self.execute_media(bus, pc, insn) {
            return result;
        }
        if let Some(result) = self.execute_vfp(bus, pc, insn) {
            return result;
        }

        // Hints and wait instructions.
        if insn & 0x0fff_fff8 == 0x0320_f000 {
            match insn & 0xf {
                2 => {
                    // WFE completes at once, consuming the event, if one is pending.
                    if std::mem::take(&mut self.event) {
                        return Ok(());
                    }
                    self.last_wfe = true;
                    return Err(Trap::Wait);
                }
                3 => {
                    self.last_wfe = false;
                    return Err(Trap::Wait);
                }
                4 => {
                    // SEV: the machine broadcasts the event to every core.
                    self.event = true;
                    self.sent_event = true;
                    return Ok(());
                }
                _ => return Ok(()),
            }
        }
        // SMC: a firmware (PSCI) call answered by the host, as on boards where the boot
        // loader installs a secure monitor. Without host firmware it is UNDEFINED.
        if insn & 0x0fff_fff0 == 0x0160_0070 {
            if self.firmware_calls_to_host && self.cpsr & 0x1f != MODE_USR {
                return Err(Trap::HostCall((insn & 0xf) as u16));
            }
            return Err(unimplemented());
        }
        if insn & 0x0ff0_00f0 == 0x0120_0070 {
            return Err(Trap::Brk((((insn >> 4) & 0xfff0) | (insn & 0xf)) as u16));
        }
        if insn & 0x0ff0_00f0 == 0x0100_0070 {
            return Err(Trap::Hlt((((insn >> 4) & 0xfff0) | (insn & 0xf)) as u16));
        }
        if insn & 0x0fff_fff0 == 0x012f_ff30 {
            // BLX register: the return address keeps the caller's instruction set in bit 0.
            let target = self.read_reg(insn & 0xf);
            self.r[14] = self.next_pc() | u32::from(self.thumb());
            self.bx_to(target);
            return Ok(());
        }
        if insn & 0x0fff_fff0 == 0x012f_ff10 {
            let target = self.read_reg(insn & 0xf);
            self.bx_to(target);
            return Ok(());
        }

        // MOVW/MOVT.
        if insn & 0x0fb0_0000 == 0x0300_0000 || insn & 0x0fb0_0000 == 0x0340_0000 {
            let rd = (insn >> 12) & 0xf;
            if rd == 15 {
                return Err(unimplemented());
            }
            let imm = (insn >> 4) & 0xf000 | insn & 0xfff;
            if insn & 0x0040_0000 == 0 {
                self.r[rd as usize] = imm;
            } else {
                self.r[rd as usize] = self.r[rd as usize] & 0xffff | imm << 16;
            }
            return Ok(());
        }

        // Long multiply and accumulate.
        if insn & 0x0f80_00f0 == 0x0080_0090 {
            let (rd_hi, rd_lo, rs, rm) = (
                (insn >> 16) & 0xf,
                (insn >> 12) & 0xf,
                (insn >> 8) & 0xf,
                insn & 0xf,
            );
            if [rd_hi, rd_lo, rs, rm].contains(&15) || rd_hi == rd_lo {
                return Err(unimplemented());
            }
            let left = self.r[rm as usize];
            let right = self.r[rs as usize];
            let mut result = if insn & (1 << 22) != 0 {
                (left as i32 as i64).wrapping_mul(right as i32 as i64) as u64
            } else {
                (left as u64).wrapping_mul(right as u64)
            };
            if insn & (1 << 21) != 0 {
                let accumulate =
                    self.r[rd_lo as usize] as u64 | (self.r[rd_hi as usize] as u64) << 32;
                result = result.wrapping_add(accumulate);
            }
            self.r[rd_lo as usize] = result as u32;
            self.r[rd_hi as usize] = (result >> 32) as u32;
            if insn & (1 << 20) != 0 {
                self.cpsr =
                    (self.cpsr & !(N | Z)) | (result >> 32) as u32 & N | u32::from(result == 0) * Z;
            }
            return Ok(());
        }

        // ARM-state signed/unsigned divide. Divide-by-zero returns zero.
        if matches!(insn & 0x0ff0_f0f0, 0x0710_f010 | 0x0730_f010) {
            let (rd, divisor, dividend) = ((insn >> 16) & 0xf, (insn >> 8) & 0xf, insn & 0xf);
            if [rd, divisor, dividend].contains(&15) {
                return Err(unimplemented());
            }
            let left = self.r[dividend as usize];
            let right = self.r[divisor as usize];
            self.r[rd as usize] = if right == 0 {
                0
            } else if insn & (1 << 21) == 0 {
                (left as i32).wrapping_div(right as i32) as u32
            } else {
                left / right
            };
            return Ok(());
        }

        // MUL/MLA.
        if insn & 0x0fc0_00f0 == 0x0000_0090 {
            let (rd, rn, rs, rm) = (
                (insn >> 16) & 0xf,
                (insn >> 12) & 0xf,
                (insn >> 8) & 0xf,
                insn & 0xf,
            );
            if rd == 15 || rn == 15 || rs == 15 || rm == 15 {
                return Err(unimplemented());
            }
            let mut result = self.r[rm as usize].wrapping_mul(self.r[rs as usize]);
            if insn & (1 << 21) != 0 {
                result = result.wrapping_add(self.r[rn as usize]);
            }
            self.r[rd as usize] = result;
            if insn & (1 << 20) != 0 {
                self.set_nz(result);
            }
            return Ok(());
        }

        // MSR (field-masked CPSR/SPSR writes from a register or immediate).
        let register_msr = matches!(insn & 0x0ff0_fff0, 0x0120_f000 | 0x0160_f000);
        let immediate_msr = matches!(insn & 0x0ff0_f000, 0x0320_f000 | 0x0360_f000);
        if register_msr || immediate_msr {
            let value = if register_msr {
                let rm = insn & 0xf;
                if rm == 15 {
                    return Err(unimplemented());
                }
                self.r[rm as usize]
            } else {
                (insn & 0xff).rotate_right(2 * ((insn >> 8) & 0xf))
            };
            if !self.write_psr(insn & (1 << 22) != 0, (insn >> 16) & 0xf, value) {
                return Err(unimplemented());
            }
            return Ok(());
        }

        // MRS (CPSR/SPSR to a general register).
        if insn & 0x0fbf_0fff == 0x010f_0000 {
            let rd = (insn >> 12) & 0xf;
            if rd == 15 {
                return Err(unimplemented());
            }
            self.r[rd as usize] = if insn & (1 << 22) == 0 {
                self.cpsr
            } else {
                self.banked_spsr[Self::bank(self.cpsr)]
            };
            return Ok(());
        }

        // Count leading zeros.
        if insn & 0x0fff_0ff0 == 0x016f_0f10 {
            let (rd, rm) = ((insn >> 12) & 0xf, insn & 0xf);
            if rd == 15 || rm == 15 {
                return Err(unimplemented());
            }
            self.r[rd as usize] = self.r[rm as usize].leading_zeros();
            return Ok(());
        }

        // Bitfield insert/clear. Rn = 15 encodes BFC with a zero source.
        if insn & 0x0fe0_0070 == 0x07c0_0010 {
            let (rd, rn) = ((insn >> 12) & 0xf, insn & 0xf);
            let msb = (insn >> 16) & 0x1f;
            let lsb = (insn >> 7) & 0x1f;
            if rd == 15 || msb < lsb {
                return Err(unimplemented());
            }
            let width = msb - lsb + 1;
            let mask = if width == 32 {
                u32::MAX
            } else {
                (1 << width) - 1
            };
            let source = if rn == 15 { 0 } else { self.r[rn as usize] };
            let old = self.r[rd as usize];
            self.r[rd as usize] = (old & !(mask << lsb)) | (source & mask) << lsb;
            return Ok(());
        }

        // Unsigned/signed bitfield extracts.
        if matches!(insn & 0x0fe0_0070, 0x07e0_0050 | 0x07a0_0050) {
            let signed = insn & 0x0fe0_0070 == 0x07a0_0050;
            let (rd, rm) = ((insn >> 12) & 0xf, insn & 0xf);
            let width = ((insn >> 16) & 0x1f) + 1;
            let lsb = (insn >> 7) & 0x1f;
            if rd == 15 || rm == 15 || lsb + width > 32 {
                return Err(unimplemented());
            }
            let mask = if width == 32 {
                u32::MAX
            } else {
                (1 << width) - 1
            };
            let value = (self.r[rm as usize] >> lsb) & mask;
            self.r[rd as usize] = if signed {
                ((value << (32 - width)) as i32 >> (32 - width)) as u32
            } else {
                value
            };
            return Ok(());
        }

        // Byte reversals.
        if matches!(insn & 0x0fff_0ff0, 0x06bf_0f30 | 0x06bf_0fb0) {
            let (rd, rm) = ((insn >> 12) & 0xf, insn & 0xf);
            if rd == 15 || rm == 15 {
                return Err(unimplemented());
            }
            let value = self.r[rm as usize];
            self.r[rd as usize] = if insn & 0x80 == 0 {
                value.swap_bytes()
            } else {
                (value & 0x00ff_00ff) << 8 | (value & 0xff00_ff00) >> 8
            };
            return Ok(());
        }

        // Zero/sign-extending byte and halfword operations, with optional rotation.
        let extend = match insn & 0x0ff0_03f0 {
            0x06e0_0070 => Some(0),
            0x06f0_0070 => Some(1),
            0x06a0_0070 => Some(2),
            0x06b0_0070 => Some(3),
            _ => None,
        };
        if let Some(kind) = extend {
            let (rd, rn, rm) = ((insn >> 12) & 0xf, (insn >> 16) & 0xf, insn & 0xf);
            if rd == 15 || rm == 15 {
                return Err(unimplemented());
            }
            let value = self.r[rm as usize].rotate_right(((insn >> 10) & 3) * 8);
            let extended = match kind {
                0 => value & 0xff,
                1 => value & 0xffff,
                2 => value as u8 as i8 as i32 as u32,
                _ => value as u16 as i16 as i32 as u32,
            };
            self.r[rd as usize] = if rn == 15 {
                extended
            } else {
                self.r[rn as usize].wrapping_add(extended)
            };
            return Ok(());
        }

        // B/BL. A32 r15 is the current instruction address plus eight.
        if insn & 0x0e00_0000 == 0x0a00_0000 {
            let offset = (((insn & 0x00ff_ffff) << 8) as i32 >> 6) as u32;
            if insn & (1 << 24) != 0 {
                self.r[14] = self.next_pc() | u32::from(self.thumb());
            }
            let target = self.read_reg(15).wrapping_add(offset);
            self.branch_to(target);
            return Ok(());
        }

        // SVC always enters the guest's supervisor call handler.
        if insn & 0x0f00_0000 == 0x0f00_0000 {
            self.enter_exception(0x08, self.next_pc(), MODE_SVC, false);
            return Err(Trap::Exception(0x15, 1));
        }

        // CP15 64-bit architectural timer registers (MCRR/MRRC).
        if insn & 0x0fe0_0000 == 0x0c40_0000 && (insn >> 8) & 0xf == 15 {
            let read = insn & (1 << 20) != 0;
            let (opc1, rt, rt2, crm) = (
                (insn >> 4) & 0xf,
                (insn >> 12) & 0xf,
                (insn >> 16) & 0xf,
                insn & 0xf,
            );
            if rt == 15 || rt2 == 15 {
                return Err(unimplemented());
            }
            let value = match (read, opc1, crm) {
                (true, 0 | 1, 14) => Some(self.cntpct), // CNTPCT, CNTVCT (CNTVOFF = 0)
                (true, 2, 14) => Some(self.cntp_cval),
                (true, 3, 14) => Some(self.cntv_cval),
                (true, 4, 14) => Some(0), // CNTVOFF
                (true, 0, 2) => Some(self.ttbr0 as u64),
                (true, 1, 2) => Some(self.ttbr1 as u64),
                _ => None,
            };
            if read {
                let Some(value) = value else {
                    return Err(unimplemented());
                };
                self.r[rt as usize] = value as u32;
                self.r[rt2 as usize] = (value >> 32) as u32;
            } else {
                let value = self.r[rt as usize] as u64 | (self.r[rt2 as usize] as u64) << 32;
                match (opc1, crm) {
                    (2, 14) => self.cntp_cval = value,
                    (3, 14) => self.cntv_cval = value,
                    (4, 14) => {} // CNTVOFF: fixed at zero
                    _ => return Err(unimplemented()),
                }
            }
            return Ok(());
        }

        // CP15 identity, MMU control, faults, cache/TLB maintenance and thread state.
        if insn & 0x0f00_0f10 == 0x0e00_0f10 {
            let read = insn & (1 << 20) != 0;
            let (opc1, crn, rd, crm, opc2) = (
                (insn >> 21) & 7,
                (insn >> 16) & 0xf,
                (insn >> 12) & 0xf,
                insn & 0xf,
                (insn >> 5) & 7,
            );
            if rd == 15 {
                return Err(unimplemented());
            }
            let value = match (opc1, crn, crm, opc2) {
                (0, 0, 0, 0) => Some(self.midr),
                (0, 0, 0, 1) => Some(0x8444_8003), // CTR
                (0, 0, 0, 5) => Some(self.mpidr),
                (0, 0, 0, 6) => Some(0), // REVIDR
                (0, 0, 1, 0) => Some(0x0000_1131), // ID_PFR0
                (0, 0, 1, 1) => Some(0x0001_1011), // ID_PFR1
                (0, 0, 1, 2) => Some(0x0200_0000), // ID_DFR0: PMUv2, no CP14 debug
                (0, 0, 0, 2 | 3 | 4 | 7) => Some(0), // TCMTR, TLBTR, aliases
                (0, 0, 1, 3) => Some(0),           // ID_AFR0
                (0, 0, 1, 4) => Some(0x1010_1105), // ID_MMFR0
                (0, 0, 1, 5) => Some(0x4000_0000), // ID_MMFR1
                (0, 0, 1, 6) => Some(0x0124_0000), // ID_MMFR2
                (0, 0, 1, 7) => Some(0x0210_2211), // ID_MMFR3
                (0, 0, 2, 0) => Some(0x0210_1110), // ID_ISAR0, including ARM divide
                (0, 0, 2, 1) => Some(0x1311_2111), // ID_ISAR1
                (0, 0, 2, 2) => Some(0x2123_2041), // ID_ISAR2
                (0, 0, 2, 3) => Some(0x1111_2131), // ID_ISAR3
                (0, 0, 2, 4) => Some(0x1001_1142), // ID_ISAR4
                (0, 0, 2, 5 | 6 | 7) => Some(0),   // ID_ISAR5, ID_MMFR4, ID_ISAR6
                (0, 0, 3, 4) => Some(0),           // ID_PFR2
                (1, 0, 0, 0) => Some(self.ccsidr()),
                (1, 0, 0, 1) => Some(0x0a20_0023), // CLIDR
                (2, 0, 0, 0) => Some(self.csselr),
                (0, 1, 0, 0) => Some(self.sctlr),
                (0, 1, 0, 1) => Some(self.actlr),
                (0, 1, 0, 2) => Some(self.cpacr),
                (0, 2, 0, 0) => Some(self.ttbr0),
                (0, 2, 0, 1) => Some(self.ttbr1),
                (0, 2, 0, 2) => Some(self.ttbcr),
                (0, 3, 0, 0) => Some(self.dacr),
                (0, 5, 0, 0) => Some(self.dfsr),
                (0, 5, 0, 1) => Some(self.ifsr),
                (0, 6, 0, 0) => Some(self.dfar),
                (0, 6, 0, 2) => Some(self.ifar),
                (0, 10, 2, 0) => Some(self.prrr),
                (0, 10, 2, 1) => Some(self.nmrr),
                (0, 12, 0, 0) => Some(self.vbar),
                (0, 13, 0, 0) => Some(self.fcseidr),
                (0, 13, 0, 1) => Some(self.contextidr),
                (0, 13, 0, 2) => Some(self.tpidrurw),
                (0, 13, 0, 3) => Some(self.tpidrro),
                (0, 13, 0, 4) => Some(self.tpidrprw),
                (0, 14, 0, 0) => Some(self.cntfrq),
                (0, 14, 1, 0) => Some(self.cntkctl),
                (0, 14, 2, 0) => Some(self.cntp_cval.wrapping_sub(self.cntpct) as u32),
                (0, 14, 2, 1) => Some(self.physical_timer_control()),
                (0, 14, 3, 0) => Some(self.cntv_cval.wrapping_sub(self.cntpct) as u32),
                (0, 14, 3, 1) => Some(self.virtual_timer_control()),
                (0, 1, 1, 0..=2) => Some(0), // SCR, SDER, NSACR (secure configuration)
                (0, 9, 12, 0) => Some(0x4107_0000), // PMCR: Cortex-A7, no event counters
                (0, 9, 12..=14, _) => Some(0),       // other PMU registers read as zero
                (1, 9, 0, 2) => Some(0x0300_0000),   // L2CTLR: four cores
                (0, 12, 1, 0) => Some(u32::from(self.irq) << 7 | u32::from(self.fiq) << 6), // ISR
                _ => None,
            };
            if read {
                let Some(value) = value else {
                    return Err(unimplemented());
                };
                self.r[rd as usize] = value;
            } else {
                let value = self.r[rd as usize];
                match (opc1, crn, crm, opc2) {
                    (0, 1, 0, 0) => {
                        self.sctlr = value;
                        self.tlb_flush();
                    }
                    (0, 1, 0, 1) => self.actlr = value,
                    (0, 1, 0, 2) => self.cpacr = value,
                    (0, 2, 0, 0) => {
                        self.ttbr0 = value;
                        self.tlb_flush();
                    }
                    (0, 2, 0, 1) => {
                        self.ttbr1 = value;
                        self.tlb_flush();
                    }
                    (0, 2, 0, 2) => {
                        self.ttbcr = value;
                        self.tlb_flush();
                    }
                    (0, 3, 0, 0) => self.dacr = value,
                    (0, 5, 0, 0) => self.dfsr = value,
                    (0, 5, 0, 1) => self.ifsr = value,
                    (0, 6, 0, 0) => self.dfar = value,
                    (0, 6, 0, 2) => self.ifar = value,
                    (0, 7, 1 | 5 | 6 | 10 | 11 | 14, _) => {}
                    (0, 8, 3 | 5 | 6 | 7, _) => self.tlb_flush(),
                    (0, 10, 2, 0) => self.prrr = value,
                    (0, 10, 2, 1) => self.nmrr = value,
                    (0, 12, 0, 0) => self.vbar = value & 0xffff_ffe0,
                    (0, 13, 0, 0) => self.fcseidr = value,
                    (0, 13, 0, 1) => {
                        self.contextidr = value;
                        self.tlb_flush();
                    }
                    (0, 13, 0, 2) => self.tpidrurw = value,
                    (0, 13, 0, 3) => self.tpidrro = value,
                    (0, 13, 0, 4) => self.tpidrprw = value,
                    (0, 14, 0, 0) => self.cntfrq = value,
                    (0, 14, 1, 0) => self.cntkctl = value,
                    (0, 14, 2, 0) => {
                        let delta = value as i32 as i64 as u64;
                        self.cntp_cval = self.cntpct.wrapping_add(delta);
                    }
                    (0, 14, 2, 1) => self.cntp_ctl = value & 3,
                    (0, 14, 3, 0) => {
                        let delta = value as i32 as i64 as u64;
                        self.cntv_cval = self.cntpct.wrapping_add(delta);
                    }
                    (0, 14, 3, 1) => self.cntv_ctl = value & 3,
                    (0, 1, 1, 0..=2) => {}         // secure configuration: no TrustZone model
                    (0, 9, 12..=14, _) => {}       // PMU writes ignored
                    (0, 7, 0, 4) | (0, 7, 13, 1) => {} // legacy WFI/prefetch encodings
                    (2, 0, 0, 0) => self.csselr = value & 0xf,
                    _ => return Err(unimplemented()),
                }
            }
            return Ok(());
        }

        // ThumbEE control registers touched by the Cortex-A7 early CPU setup.
        if insn & 0x0f00_0f10 == 0x0e00_0e10 {
            let read = insn & (1 << 20) != 0;
            let (opc1, crn, rd, crm, opc2) = (
                (insn >> 21) & 7,
                (insn >> 16) & 0xf,
                (insn >> 12) & 0xf,
                insn & 0xf,
                (insn >> 5) & 7,
            );
            if rd == 15 {
                return Err(unimplemented());
            }
            if opc1 <= 1 {
                // CP14 debug (opc1 0) and trace (opc1 1): no debug architecture is
                // advertised, so DBGDIDR and the rest read as zero and ignore writes.
                if read {
                    self.r[rd as usize] = 0;
                }
                return Ok(());
            }
            if opc1 != 6 || crm != 0 || opc2 != 0 {
                return Err(unimplemented());
            }
            let value = match crn {
                0 => Some(self.teecr),
                1 => Some(self.teehbr),
                _ => None,
            };
            if read {
                let Some(value) = value else {
                    return Err(unimplemented());
                };
                self.r[rd as usize] = value;
            } else {
                match (crn, crm) {
                    (0, 0) => self.teecr = self.r[rd as usize],
                    (1, 0) => self.teehbr = self.r[rd as usize],
                    _ => return Err(unimplemented()),
                }
            }
            return Ok(());
        }

        // Load/store exclusive and the local reservation monitor.
        let load_exclusive = match insn & 0x0ff0_0fff {
            0x0190_0f9f => Some(4),
            0x01b0_0f9f => Some(8),
            0x01d0_0f9f => Some(1),
            0x01f0_0f9f => Some(2),
            _ => None,
        };
        if let Some(size) = load_exclusive {
            let (rn, rd) = ((insn >> 16) & 0xf, (insn >> 12) & 0xf);
            if rn == 15 || rd == 15 || size == 8 && (rd & 1 != 0 || rd >= 14) {
                return Err(unimplemented());
            }
            let address = self.r[rn as usize];
            let first = match size {
                1 => self.mem_read8(bus, address, Access::Read).map(u32::from),
                2 => self.mem_read16(bus, address).map(u32::from),
                _ => self.mem_read32(bus, address, Access::Read),
            };
            let first = match first {
                Ok(value) => value,
                Err((va, status)) => return Err(self.data_abort(va, status, false)),
            };
            self.r[rd as usize] = first;
            if size == 8 {
                match self.mem_read32(bus, address.wrapping_add(4), Access::Read) {
                    Ok(value) => self.r[rd as usize + 1] = value,
                    Err((va, status)) => return Err(self.data_abort(va, status, false)),
                }
            }
            self.exclusive_addr = Some(address);
            self.exclusive_size = size;
            return Ok(());
        }
        let store_exclusive = match insn & 0x0ff0_0ff0 {
            0x0180_0f90 => Some(4),
            0x01a0_0f90 => Some(8),
            0x01c0_0f90 => Some(1),
            0x01e0_0f90 => Some(2),
            _ => None,
        };
        if let Some(size) = store_exclusive {
            let (rn, rd, rm) = (
                (insn >> 16) & 0xf,
                (insn >> 12) & 0xf,
                insn & 0xf,
            );
            if rn == 15
                || rd == 15
                || rm == 15
                || size == 8 && (rm & 1 != 0 || rm >= 14)
            {
                return Err(unimplemented());
            }
            let address = self.r[rn as usize];
            let reserved = self.exclusive_addr == Some(address) && self.exclusive_size == size;
            if !reserved {
                self.r[rd as usize] = 1;
                self.exclusive_addr = None;
                self.exclusive_size = 0;
                return Ok(());
            }
            let value = self.r[rm as usize];
            let result = match size {
                1 => self.mem_write8(bus, address, value as u8),
                2 => self.mem_write16(bus, address, value as u16),
                _ => self.mem_write32(bus, address, value),
            };
            if let Err((va, status)) = result {
                return Err(self.data_abort(va, status, true));
            }
            if size == 8 {
                if let Err((va, status)) = self.mem_write32(
                    bus,
                    address.wrapping_add(4),
                    self.r[rm as usize + 1],
                ) {
                    return Err(self.data_abort(va, status, true));
                }
            }
            self.r[rd as usize] = 0;
            self.exclusive_addr = None;
            self.exclusive_size = 0;
            return Ok(());
        }

        // Load/store multiple. Registers always transfer in architectural order.
        if insn & 0x0e00_0000 == 0x0800_0000 {
            let (pre, up, spsr, writeback, load) = (
                insn & (1 << 24) != 0,
                insn & (1 << 23) != 0,
                insn & (1 << 22) != 0,
                insn & (1 << 21) != 0,
                insn & (1 << 20) != 0,
            );
            let rn = (insn >> 16) & 0xf;
            let list = insn & 0xffff;
            let loads_pc = load && list & 0x8000 != 0;
            if rn == 15
                || list == 0
                || (!load && list & 0x8000 != 0)
                || (spsr && !loads_pc && writeback)
                || (load && writeback && list & (1 << rn) != 0)
            {
                return Err(unimplemented());
            }
            if spsr && loads_pc && self.banked_spsr[Self::bank(self.cpsr)] & T != 0 {
                return Err(unimplemented());
            }
            let base = self.r[rn as usize];
            let bytes = list.count_ones() * 4;
            let mut address = match (pre, up) {
                (false, false) => base.wrapping_sub(bytes.wrapping_sub(4)),
                (false, true) => base,
                (true, false) => base.wrapping_sub(bytes),
                (true, true) => base.wrapping_add(4),
            };
            let new_base = if up {
                base.wrapping_add(bytes)
            } else {
                base.wrapping_sub(bytes)
            };
            if load {
                let mut values = [0u32; 16];
                for (register, value) in values.iter_mut().enumerate() {
                    if list & (1 << register) != 0 {
                        *value = match self.mem_read32(bus, address, Access::Read) {
                            Ok(value) => value,
                            Err((va, status)) => return Err(self.data_abort(va, status, false)),
                        };
                        address = address.wrapping_add(4);
                    }
                }
                if writeback {
                    self.r[rn as usize] = new_base;
                }
                let user_bank = spsr && !loads_pc;
                for register in 0..15 {
                    if list & (1 << register) != 0 {
                        if user_bank {
                            self.set_user_reg(register, values[register]);
                        } else {
                            self.r[register] = values[register];
                        }
                    }
                }
                if loads_pc && spsr {
                    // Exception return: CPSR from SPSR, then a plain branch.
                    self.restore_spsr();
                    self.branch_to(values[15]);
                } else if loads_pc {
                    self.bx_to(values[15]);
                }
            } else {
                for register in 0..15 {
                    if list & (1 << register) != 0 {
                        let value = if spsr { self.user_reg(register) } else { self.r[register] };
                        if let Err((va, status)) = self.mem_write32(bus, address, value)
                        {
                            return Err(self.data_abort(va, status, true));
                        }
                        address = address.wrapping_add(4);
                    }
                }
                if writeback {
                    self.r[rn as usize] = new_base;
                }
            }
            return Ok(());
        }

        // Halfword and signed-byte/halfword transfers (split immediate or register offset).
        if insn & 0x0e00_0090 == 0x0000_0090 && insn & 0x60 != 0 {
            let (rn, rd) = ((insn >> 16) & 0xf, (insn >> 12) & 0xf);
            let (load, signed, halfword) = (
                insn & (1 << 20) != 0,
                insn & (1 << 6) != 0,
                insn & (1 << 5) != 0,
            );
            let pair = !load && signed;
            if pair && (rd & 1 != 0 || rd >= 14) || !pair && load && rd == 15 {
                return Err(unimplemented());
            }
            let offset = if insn & (1 << 22) != 0 {
                (insn >> 4) & 0xf0 | insn & 0xf
            } else {
                if insn & 0xf00 != 0 {
                    return Err(unimplemented());
                }
                self.read_reg(insn & 0xf)
            };
            let base = self.read_reg(rn);
            let adjusted = if insn & (1 << 23) != 0 {
                base.wrapping_add(offset)
            } else {
                base.wrapping_sub(offset)
            };
            let pre = insn & (1 << 24) != 0;
            let address = if pre { adjusted } else { base };
            let result = if pair {
                if halfword {
                    // STRD stores the even register first, then its odd partner.
                    if let Err((va, status)) =
                        self.mem_write32(bus, address, self.r[rd as usize])
                    {
                        return Err(self.data_abort(va, status, true));
                    }
                    if let Err((va, status)) = self.mem_write32(
                        bus,
                        address.wrapping_add(4),
                        self.r[rd as usize + 1],
                    ) {
                        return Err(self.data_abort(va, status, true));
                    }
                    Ok(())
                } else {
                    // LDRD commits both registers after both reads succeed.
                    let low = match self.mem_read32(bus, address, Access::Read) {
                        Ok(value) => value,
                        Err((va, status)) => return Err(self.data_abort(va, status, false)),
                    };
                    let high = match self.mem_read32(
                        bus,
                        address.wrapping_add(4),
                        Access::Read,
                    ) {
                        Ok(value) => value,
                        Err((va, status)) => return Err(self.data_abort(va, status, false)),
                    };
                    self.r[rd as usize] = low;
                    self.r[rd as usize + 1] = high;
                    Ok(())
                }
            } else if load {
                let value = match (signed, halfword) {
                    (false, true) => self.mem_read16(bus, address).map(u32::from),
                    (true, false) => self
                        .mem_read8(bus, address, Access::Read)
                        .map(|value| value as i8 as i32 as u32),
                    (true, true) => self
                        .mem_read16(bus, address)
                        .map(|value| value as i16 as i32 as u32),
                    _ => return Err(unimplemented()),
                };
                match value {
                    Ok(value) => {
                        self.r[rd as usize] = value;
                        Ok(())
                    }
                    Err((va, status)) => Err(self.data_abort(va, status, false)),
                }
            } else {
                match self.mem_write16(bus, address, self.read_reg(rd) as u16) {
                    Ok(()) => Ok(()),
                    Err((va, status)) => Err(self.data_abort(va, status, true)),
                }
            };
            // Base-restored abort model: an aborted transfer leaves the base unchanged.
            if result.is_ok() && (!pre || insn & (1 << 21) != 0) {
                if rn == 15 || load && (rn == rd || pair && rn == rd + 1) {
                    return Err(unimplemented());
                }
                self.r[rn as usize] = adjusted;
            }
            return result;
        }

        // Single word/byte data transfer.
        if insn & 0x0c00_0000 == 0x0400_0000 {
            let (rn, rd) = ((insn >> 16) & 0xf, (insn >> 12) & 0xf);
            let base = self.read_reg(rn);
            let offset = if insn & (1 << 25) == 0 {
                insn & 0xfff
            } else {
                if insn & (1 << 4) != 0 {
                    return Err(unimplemented());
                }
                let (value, _) = Self::shift(
                    self.read_reg(insn & 0xf),
                    (insn >> 5) & 3,
                    (insn >> 7) & 0x1f,
                    self.cpsr & C != 0,
                    true,
                );
                value
            };
            let adjusted = if insn & (1 << 23) != 0 {
                base.wrapping_add(offset)
            } else {
                base.wrapping_sub(offset)
            };
            let pre = insn & (1 << 24) != 0;
            let addr = if pre { adjusted } else { base };
            let load = insn & (1 << 20) != 0;
            let byte = insn & (1 << 22) != 0;
            let result = if load {
                let value = if byte {
                    self.mem_read8(bus, addr, Access::Read).map(u32::from)
                } else {
                    self.mem_read32(bus, addr, Access::Read)
                };
                match value {
                    Ok(value) => {
                        if rd == 15 {
                            if byte {
                                return Err(unimplemented());
                            }
                            self.bx_to(value);
                        } else {
                            self.r[rd as usize] = value;
                        }
                        Ok(())
                    }
                    Err((va, status)) => Err(self.data_abort(va, status, false)),
                }
            } else {
                let value = self.read_reg(rd);
                let result = if byte {
                    self.mem_write8(bus, addr, value as u8)
                } else {
                    self.mem_write32(bus, addr, value)
                };
                match result {
                    Ok(()) => Ok(()),
                    Err((va, status)) => Err(self.data_abort(va, status, true)),
                }
            };
            // Base-restored abort model: an aborted transfer leaves the base unchanged.
            if result.is_ok() && (!pre || insn & (1 << 21) != 0) {
                if rn == 15 || load && rn == rd {
                    return Err(unimplemented());
                }
                self.r[rn as usize] = adjusted;
            }
            return result;
        }

        // A32 data processing with immediate or shifted-register operand two.
        if insn & 0x0c00_0000 == 0 {
            let immediate = insn & (1 << 25) != 0;
            // Not data processing: multiply/extra load-store space (bits 7 and 4 set) and
            // the miscellaneous space (TST/TEQ/CMP/CMN encodings without S).
            if !immediate && insn & 0x90 == 0x90
                || insn & 0x0190_0000 == 0x0100_0000
            {
                return Err(unimplemented());
            }
            let (opcode, set_flags, rn, rd) = (
                (insn >> 21) & 0xf,
                insn & (1 << 20) != 0,
                (insn >> 16) & 0xf,
                (insn >> 12) & 0xf,
            );
            let (operand, shifter_carry) = if immediate {
                let rotation = (insn >> 8) & 0xf;
                let value = (insn & 0xff).rotate_right(2 * rotation);
                (
                    value,
                    if rotation == 0 {
                        self.cpsr & C != 0
                    } else {
                        value & N != 0
                    },
                )
            } else {
                let rm = insn & 0xf;
                let by_register = insn & (1 << 4) != 0;
                let amount = if by_register {
                    let rs = (insn >> 8) & 0xf;
                    if rs == 15 || rm == 15 {
                        return Err(unimplemented());
                    }
                    self.read_reg(rs) & 0xff
                } else {
                    (insn >> 7) & 0x1f
                };
                Self::shift(
                    self.read_reg(rm),
                    (insn >> 5) & 3,
                    amount,
                    self.cpsr & C != 0,
                    !by_register,
                )
            };
            let left = self.read_reg(rn);
            let carry_in = self.cpsr & C != 0;
            let mut arithmetic = None;
            let result = match opcode {
                0 => operand & left,
                1 => operand ^ left,
                2 | 10 => {
                    let r = Self::add_with_carry(left, !operand, true);
                    arithmetic = Some((left, !operand, r));
                    r.0
                }
                3 => {
                    let r = Self::add_with_carry(operand, !left, true);
                    arithmetic = Some((operand, !left, r));
                    r.0
                }
                4 | 11 => {
                    let r = Self::add_with_carry(left, operand, false);
                    arithmetic = Some((left, operand, r));
                    r.0
                }
                5 => {
                    let r = Self::add_with_carry(left, operand, carry_in);
                    arithmetic = Some((left, operand, r));
                    r.0
                }
                6 => {
                    let r = Self::add_with_carry(left, !operand, carry_in);
                    arithmetic = Some((left, !operand, r));
                    r.0
                }
                7 => {
                    let r = Self::add_with_carry(operand, !left, carry_in);
                    arithmetic = Some((operand, !left, r));
                    r.0
                }
                8 => left & operand,
                9 => left ^ operand,
                12 => left | operand,
                13 => operand,
                14 => left & !operand,
                15 => !operand,
                _ => return Err(unimplemented()),
            };
            if matches!(opcode, 8..=11) {
                if let Some((a, b, (value, carry, overflow))) = arithmetic {
                    if matches!(opcode, 10 | 11) {
                        self.set_add_flags(a, b, value, carry, overflow);
                    }
                }
                if matches!(opcode, 8 | 9) {
                    self.set_logic_flags(result, shifter_carry);
                }
                return Ok(());
            }
            if rd == 15 {
                if set_flags {
                    // SUBS PC, LR etc.: exception return.
                    self.restore_spsr();
                    self.branch_to(result);
                } else {
                    self.bx_to(result); // ARMv7 ALUWritePC interworks in ARM state
                }
                return Ok(());
            }
            self.r[rd as usize] = result;
            if set_flags {
                if let Some((a, b, (value, carry, overflow))) = arithmetic {
                    self.set_add_flags(a, b, value, carry, overflow);
                } else {
                    self.set_logic_flags(result, shifter_carry);
                }
            }
            return Ok(());
        }

        Err(unimplemented())
    }
}

impl Cpu {
    /// Retire an instruction or turn an unimplemented encoding into UNDEFINED.
    pub(crate) fn finish(&mut self, pc: u32, insn: u32, result: Result<(), Trap>) -> Result<(), Trap> {
        match result {
            Ok(()) | Err(Trap::Wait | Trap::HostCall(_) | Trap::Brk(_) | Trap::Hlt(_)) => {
                self.instret = self.instret.wrapping_add(1);
            }
            Err(Trap::Unimplemented(..)) if self.undef_to_guest => {
                self.last_undef = Some((pc, insn));
                // LR_und = instruction + 4 (ARM) or + 2 (Thumb, also for 32-bit
                // encodings: Linux reads the halfwords at LR - 2 and LR).
                let lr = pc.wrapping_add(if self.thumb() { 2 } else { 4 });
                self.enter_exception(0x04, lr, MODE_UND, false);
                return Err(Trap::Exception(0x00, 1));
            }
            Err(Trap::Unimplemented(..)) => self.r[15] = pc,
            _ => {}
        }
        result
    }
}

impl Core for Cpu {
    fn step(&mut self, bus: &mut dyn Bus) -> Result<(), Trap> {
        if self.irq && self.cpsr & I == 0 {
            self.enter_exception(0x18, self.r[15].wrapping_add(4), MODE_IRQ, true);
            return Err(Trap::Interrupt(0, 1));
        }
        if self.fiq && self.cpsr & F == 0 {
            self.enter_exception(0x1c, self.r[15].wrapping_add(4), MODE_FIQ, true);
            return Err(Trap::Interrupt(1, 1));
        }
        let pc = self.r[15];
        self.cur_pc = pc;
        self.pc_written = false;
        if self.thumb() {
            return self.step_thumb(bus, pc);
        }
        self.cur_size = 4;
        let insn = match self.mem_read32(bus, pc, Access::Execute) {
            Ok(insn) => insn,
            Err((va, status)) => return Err(self.prefetch_abort(va, status)),
        };
        self.r[15] = pc.wrapping_add(4);
        let result = self.execute(bus, pc as u64, insn);
        self.finish(pc, insn, result)
    }

    fn pc(&self) -> u64 {
        self.r[15] as u64
    }

    fn instret(&self) -> u64 {
        self.instret
    }

    fn set_irq(&mut self, irq: bool, fiq: bool) {
        self.irq = irq;
        self.fiq = fiq;
    }
}
