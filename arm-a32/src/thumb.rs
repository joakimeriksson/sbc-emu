//! Thumb and Thumb-2 (T32).
//!
//! Most encodings are translated to the equivalent A32 instruction and executed by the
//! A32 path, whose semantics are exercised by booting Linux; the rest (branches, IT,
//! PC-relative and Thumb-only forms: modified immediates, ORN, 12-bit offsets, LDRD with
//! any register pair, TBB/TBH) are executed here. While a Thumb instruction executes,
//! `r[15]` holds its address, so the A32 path's PC reads see address + 4 as Thumb
//! requires; the next PC is set afterwards unless the instruction wrote the PC.
use crate::cpu::Cpu;
use crate::mmu::Access;
use emu_core::{Bus, Trap};

const C: u32 = 1 << 29;
const AL: u32 = 0xe000_0000;

fn sign_extend(value: u32, bits: u32) -> u32 {
    ((value << (32 - bits)) as i32 >> (32 - bits)) as u32
}

/// ThumbExpandImm_C: the 32-bit value of a 12-bit modified immediate and its carry.
pub(crate) fn expand_imm(imm12: u32, carry: bool) -> (u32, bool) {
    if imm12 >> 10 == 0 {
        let imm8 = imm12 & 0xff;
        let value = match (imm12 >> 8) & 3 {
            0 => imm8,
            1 => imm8 << 16 | imm8,
            2 => imm8 << 24 | imm8 << 8,
            _ => imm8 * 0x0101_0101,
        };
        (value, carry)
    } else {
        let value = (0x80 | imm12 & 0x7f).rotate_right(imm12 >> 7);
        (value, value >> 31 != 0)
    }
}

impl Cpu {
    fn it_state(&self) -> u32 {
        (self.cpsr >> 25) & 3 | (self.cpsr >> 8) & 0xfc
    }

    fn set_it_state(&mut self, it: u32) {
        self.cpsr = self.cpsr & !0x0600_fc00 | (it & 3) << 25 | (it & 0xfc) << 8;
    }

    fn in_it_block(&self) -> bool {
        self.it_state() & 0xf != 0
    }

    fn it_advance(&mut self) {
        let it = self.it_state();
        if it & 7 == 0 {
            self.set_it_state(0);
        } else {
            self.set_it_state(it & 0xe0 | (it << 1) & 0x1f);
        }
    }

    fn fetch_pa(&mut self, bus: &mut dyn Bus, va: u32) -> Result<u32, Trap> {
        self.translate_fill(bus, va, Access::Execute)
            .map_err(|(va, status)| self.prefetch_abort(va, status))
    }

    fn read_code16(&mut self, bus: &mut dyn Bus, va: u32, pa: u32) -> Result<u32, Trap> {
        bus.read16(pa as u64)
            .map(u32::from)
            .map_err(|_| self.prefetch_abort(va, 0x08))
    }

    /// Fetch, decode and execute one Thumb instruction at `pc`.
    pub(crate) fn step_thumb(&mut self, bus: &mut dyn Bus, pc: u32) -> Result<(), Trap> {
        self.cur_size = 2;
        let pa = self.fetch_pa(bus, pc)?;
        let hw1 = self.read_code16(bus, pc, pa)?;
        let wide = hw1 >> 11 >= 0b11101;
        let insn = if wide {
            self.cur_size = 4;
            let next = pc.wrapping_add(2);
            // Both halfwords come from one translation unless they straddle a page.
            let pa2 = if pc & 0xfff == 0xffe { self.fetch_pa(bus, next)? } else { pa + 2 };
            hw1 << 16 | self.read_code16(bus, next, pa2)?
        } else {
            hw1
        };
        self.r[15] = pc; // A32-path PC reads see pc + 4
        self.psr_restored = false;
        let it = self.it_state();
        let mut is_it = false;
        let result = if it & 0xf != 0 && !self.condition(it >> 4) {
            Ok(())
        } else if wide {
            self.thumb32(bus, insn >> 16, insn & 0xffff)
        } else {
            self.thumb16(bus, insn, &mut is_it)
        };
        if !self.pc_written {
            self.r[15] = pc.wrapping_add(self.cur_size);
        }
        // No ITAdvance for IT itself, after an exception (SPSR holds the state), or after
        // an exception return (the restored state belongs to the interrupted code).
        if !is_it
            && !self.psr_restored
            && !matches!(result, Err(Trap::Exception(..)) | Err(Trap::Unimplemented(..)))
        {
            self.it_advance();
        }
        self.finish(pc, insn, result)
    }

    /// Run the A32 translation of the current instruction (condition already checked).
    fn a32(&mut self, bus: &mut dyn Bus, word: u32) -> Result<(), Trap> {
        self.execute(bus, self.cur_pc as u64, AL | word & 0x0fff_ffff)
    }

    fn undefined(&self, insn: u32) -> Result<(), Trap> {
        Err(Trap::Unimplemented(self.cur_pc as u64, insn))
    }

    fn pc_value(&self) -> u32 {
        self.cur_pc.wrapping_add(4)
    }

    fn thumb16(&mut self, bus: &mut dyn Bus, hw: u32, is_it: &mut bool) -> Result<(), Trap> {
        let s = u32::from(!self.in_it_block()) << 20;
        let (r0, r3, r6, r8) = (hw & 7, (hw >> 3) & 7, (hw >> 6) & 7, (hw >> 8) & 7);
        let imm8 = hw & 0xff;
        match hw >> 11 {
            0..=2 => {
                // LSL/LSR/ASR (immediate) = MOV{S} Rd, Rm, <shift> #imm5
                let imm5 = (hw >> 6) & 31;
                return self.a32(bus, 0x01a0_0000 | s | r0 << 12 | imm5 << 7 | (hw >> 11) << 5 | r3);
            }
            3 => {
                let sub = hw & (1 << 9) != 0;
                let base = match (hw & (1 << 10) != 0, sub) {
                    (false, false) => 0x0080_0000,
                    (false, true) => 0x0040_0000,
                    (true, false) => 0x0280_0000,
                    (true, true) => 0x0240_0000,
                };
                return self.a32(bus, base | s | r3 << 16 | r0 << 12 | r6);
            }
            4 => return self.a32(bus, 0x03a0_0000 | s | r8 << 12 | imm8), // MOV imm
            5 => return self.a32(bus, 0x0350_0000 | r8 << 16 | imm8),     // CMP imm
            6 => return self.a32(bus, 0x0280_0000 | s | r8 << 16 | r8 << 12 | imm8),
            7 => return self.a32(bus, 0x0240_0000 | s | r8 << 16 | r8 << 12 | imm8),
            _ => {}
        }
        match hw >> 10 {
            0b010000 => {
                let (rm, rdn) = (r3, r0);
                let word = match (hw >> 6) & 15 {
                    0 => 0x0000_0000 | s | rdn << 16 | rdn << 12 | rm,
                    1 => 0x0020_0000 | s | rdn << 16 | rdn << 12 | rm,
                    op @ (2 | 3 | 4 | 7) => {
                        let kind = match op { 2 => 0, 3 => 1, 4 => 2, _ => 3 };
                        0x01a0_0010 | s | rdn << 12 | rm << 8 | kind << 5 | rdn
                    }
                    5 => 0x00a0_0000 | s | rdn << 16 | rdn << 12 | rm,
                    6 => 0x00c0_0000 | s | rdn << 16 | rdn << 12 | rm,
                    8 => 0x0110_0000 | rdn << 16 | rm,
                    9 => 0x0260_0000 | s | rm << 16 | rdn << 12, // RSB Rd, Rn, #0
                    10 => 0x0150_0000 | rdn << 16 | rm,
                    11 => 0x0170_0000 | rdn << 16 | rm,
                    12 => 0x0180_0000 | s | rdn << 16 | rdn << 12 | rm,
                    13 => 0x0000_0090 | s | rdn << 16 | rdn << 8 | rm,
                    14 => 0x01c0_0000 | s | rdn << 16 | rdn << 12 | rm,
                    _ => 0x01e0_0000 | s | rdn << 12 | rm,
                };
                return self.a32(bus, word);
            }
            0b010001 => {
                let rd = (hw >> 4) & 8 | r0;
                let rm = (hw >> 3) & 15;
                return match (hw >> 8) & 3 {
                    0 if rd == 15 => {
                        let target = self.pc_value().wrapping_add(self.read_thumb_reg(rm));
                        self.branch_to(target);
                        Ok(())
                    }
                    0 => self.a32(bus, 0x0080_0000 | rd << 16 | rd << 12 | rm),
                    1 => self.a32(bus, 0x0150_0000 | rd << 16 | rm),
                    2 if rd == 15 => {
                        let target = self.read_thumb_reg(rm);
                        self.branch_to(target);
                        Ok(())
                    }
                    2 => self.a32(bus, 0x01a0_0000 | rd << 12 | rm),
                    _ => {
                        let target = self.read_thumb_reg(rm);
                        if hw & 0x80 != 0 {
                            self.r[14] = self.next_pc() | 1;
                        }
                        self.bx_to(target);
                        Ok(())
                    }
                };
            }
            _ => {}
        }
        match hw >> 12 {
            0b0100 => {
                // LDR (literal)
                let address = (self.pc_value() & !3).wrapping_add(imm8 * 4);
                return self.load(bus, address, 4, false, r8);
            }
            0b0101 => {
                let word = match (hw >> 9) & 7 {
                    0 => 0x0780_0000,
                    1 => 0x0180_00b0,
                    2 => 0x07c0_0000,
                    3 => 0x0190_00d0,
                    4 => 0x0790_0000,
                    5 => 0x0190_00b0,
                    6 => 0x07d0_0000,
                    _ => 0x0190_00f0,
                };
                return self.a32(bus, word | r3 << 16 | r0 << 12 | r6);
            }
            0b0110 | 0b0111 => {
                let byte = hw & (1 << 12) != 0;
                let imm5 = (hw >> 6) & 31;
                let offset = if byte { imm5 } else { imm5 * 4 };
                let load = (hw >> 11) & 1;
                return self.a32(
                    bus,
                    0x0580_0000 | u32::from(byte) << 22 | load << 20 | r3 << 16 | r0 << 12 | offset,
                );
            }
            0b1000 => {
                let offset = ((hw >> 6) & 31) * 2;
                let load = (hw >> 11) & 1;
                return self.a32(
                    bus,
                    0x01c0_00b0 | load << 20 | r3 << 16 | r0 << 12 | (offset >> 4) << 8 | offset & 15,
                );
            }
            0b1001 => {
                let load = (hw >> 11) & 1;
                return self.a32(bus, 0x058d_0000 | load << 20 | r8 << 12 | imm8 * 4);
            }
            0b1010 => {
                self.r[r8 as usize] = if hw & (1 << 11) == 0 {
                    (self.pc_value() & !3).wrapping_add(imm8 * 4) // ADR
                } else {
                    self.r[13].wrapping_add(imm8 * 4)
                };
                return Ok(());
            }
            0b1011 => return self.thumb16_misc(bus, hw, is_it),
            0b1100 => {
                let rn = r8;
                let load = hw & (1 << 11) != 0;
                let writeback = !load || imm8 & (1 << rn) == 0;
                return self.a32(
                    bus,
                    0x0880_0000 | u32::from(writeback) << 21 | u32::from(load) << 20 | rn << 16 | imm8,
                );
            }
            0b1101 => {
                let cond = (hw >> 8) & 15;
                return match cond {
                    14 => self.undefined(hw),
                    15 => {
                        // SVC returns to the next instruction: ITSTATE advances before
                        // the CPSR is saved in SPSR_svc.
                        self.it_advance();
                        *is_it = true;
                        self.a32(bus, 0x0f00_0000 | imm8)
                    }
                    _ => {
                        if self.condition(cond) {
                            let target = self.pc_value().wrapping_add(sign_extend(imm8 << 1, 9));
                            self.branch_to(target);
                        }
                        Ok(())
                    }
                };
            }
            0b1110 => {
                let target = self.pc_value().wrapping_add(sign_extend((hw & 0x7ff) << 1, 12));
                self.branch_to(target);
                return Ok(());
            }
            _ => {}
        }
        self.undefined(hw)
    }

    fn thumb16_misc(&mut self, bus: &mut dyn Bus, hw: u32, is_it: &mut bool) -> Result<(), Trap> {
        let (r0, r3) = (hw & 7, (hw >> 3) & 7);
        match (hw >> 8) & 15 {
            0b0000 => {
                let imm = (hw & 0x7f) * 4;
                self.r[13] = if hw & 0x80 != 0 {
                    self.r[13].wrapping_sub(imm)
                } else {
                    self.r[13].wrapping_add(imm)
                };
                Ok(())
            }
            0b0001 | 0b0011 | 0b1001 | 0b1011 => {
                // CBZ/CBNZ
                let offset = (hw >> 3) & 0x1f | (hw >> 4) & 0x20;
                if (self.r[r0 as usize] == 0) != (hw & (1 << 11) != 0) {
                    let target = self.pc_value().wrapping_add(offset << 1);
                    self.branch_to(target);
                }
                Ok(())
            }
            0b0010 => {
                let word = match (hw >> 6) & 3 {
                    0 => 0x06bf_0070, // SXTH
                    1 => 0x06af_0070, // SXTB
                    2 => 0x06ff_0070, // UXTH
                    _ => 0x06ef_0070, // UXTB
                };
                self.a32(bus, word | r0 << 12 | r3)
            }
            0b0100 | 0b0101 => self.a32(bus, 0x092d_0000 | (hw & 0xff) | (hw & 0x100) << 6),
            0b0110 if hw & 0xffe8 == 0xb660 => {
                let imod = if hw & 0x10 != 0 { 3 } else { 2 };
                let flags = (hw & 4) << 6 | (hw & 2) << 6 | (hw & 1) << 6;
                self.execute(bus, self.cur_pc as u64, 0xf100_0000 | imod << 18 | flags)
            }
            0b1010 => {
                let word = match (hw >> 6) & 3 {
                    0 => 0x06bf_0f30,
                    1 => 0x06bf_0fb0,
                    3 => 0x06ff_0fb0,
                    _ => return self.undefined(hw),
                };
                self.a32(bus, word | r0 << 12 | r3)
            }
            0b1100 | 0b1101 => self.a32(bus, 0x08bd_0000 | (hw & 0xff) | (hw & 0x100) << 7),
            0b1110 => Err(Trap::Brk((hw & 0xff) as u16)),
            0b1111 => {
                if hw & 0xf != 0 {
                    // IT: the state takes effect from the next instruction.
                    self.set_it_state(hw & 0xff);
                    *is_it = true;
                    Ok(())
                } else {
                    self.a32(bus, 0x0320_f000 | (hw >> 4) & 0xf)
                }
            }
            _ => self.undefined(hw),
        }
    }

    /// A register read as a Thumb instruction sees it (PC = address + 4).
    fn read_thumb_reg(&self, n: u32) -> u32 {
        if n == 15 { self.pc_value() } else { self.r[n as usize] }
    }

    fn load(&mut self, bus: &mut dyn Bus, address: u32, size: u32, signed: bool, rt: u32) -> Result<(), Trap> {
        let value = match size {
            1 => self.mem_read8(bus, address, Access::Read).map(|v| if signed { v as i8 as u32 } else { v as u32 }),
            2 => self.mem_read16(bus, address).map(|v| if signed { v as i16 as u32 } else { v as u32 }),
            _ => self.mem_read32(bus, address, Access::Read),
        };
        match value {
            Ok(value) if rt == 15 => {
                self.bx_to(value);
                Ok(())
            }
            Ok(value) => {
                self.r[rt as usize] = value;
                Ok(())
            }
            Err((va, status)) => Err(self.data_abort(va, status, false)),
        }
    }

    fn store(&mut self, bus: &mut dyn Bus, address: u32, size: u32, value: u32) -> Result<(), Trap> {
        let result = match size {
            1 => self.mem_write8(bus, address, value as u8),
            2 => self.mem_write16(bus, address, value as u16),
            _ => self.mem_write32(bus, address, value),
        };
        result.map_err(|(va, status)| self.data_abort(va, status, true))
    }

    fn thumb32(&mut self, bus: &mut dyn Bus, hw1: u32, hw2: u32) -> Result<(), Trap> {
        let insn = hw1 << 16 | hw2;
        let op2 = (hw1 >> 4) & 0x7f;
        match (hw1 >> 11) & 3 {
            1 => {
                if op2 & 0x64 == 0x00 {
                    // LDM/STM (IA, DB)
                    let rn = hw1 & 15;
                    let (w, l) = ((hw1 >> 5) & 1, (hw1 >> 4) & 1);
                    let mode = match (hw1 >> 7) & 3 {
                        1 => 0x0880_0000,
                        2 => 0x0900_0000,
                        op => {
                            // RFEDB/RFEIA (L = 1) and SRSDB/SRSIA (L = 0): op 00 is DB, 11 IA.
                            let increment = op == 3;
                            let writeback = w != 0;
                            return if l != 0 && hw2 == 0xc000 {
                                self.return_from_exception(bus, insn, rn as usize, increment, !increment, writeback)
                            } else if l == 0 && rn == 13 && hw2 & 0xffe0 == 0xc000 {
                                self.store_return_state(bus, insn, hw2 & 0x1f, increment, !increment, writeback)
                            } else {
                                self.undefined(insn)
                            };
                        }
                    };
                    return self.a32(bus, mode | w << 21 | l << 20 | rn << 16 | hw2);
                }
                if op2 & 0x64 == 0x04 {
                    return self.thumb32_dual_exclusive(bus, hw1, hw2);
                }
                if op2 & 0x60 == 0x20 {
                    return self.thumb32_dp_shifted(bus, hw1, hw2);
                }
                // Coprocessor space (VFP, CP15, CP14): the A32 encoding with an AL
                // condition. 1110 1111 is Advanced SIMD data processing (U=0), which must
                // not reach the A32 path as 0xEFxxxxxx (SVC): it becomes 1111 0010.
                if hw1 & 0x0300 == 0x0300 {
                    return self.thumb32_simd(bus, hw1, hw2);
                }
                self.a32(bus, insn)
            }
            2 => {
                if hw2 & 0x8000 != 0 {
                    return self.thumb32_branch_misc(bus, hw1, hw2);
                }
                let i = (hw1 >> 10) & 1;
                let imm3 = (hw2 >> 12) & 7;
                let rd = (hw2 >> 8) & 15;
                let rn = hw1 & 15;
                let imm8 = hw2 & 0xff;
                let imm12 = i << 11 | imm3 << 8 | imm8;
                if op2 & 0x20 == 0 {
                    let (operand, carry) = expand_imm(imm12, self.cpsr & C != 0);
                    return self.thumb32_dp(insn, (hw1 >> 5) & 15, hw1 & 0x10 != 0, rn, rd, operand, carry);
                }
                let imm16 = (hw1 & 15) << 12 | imm12;
                let shift = imm3 << 2 | (hw2 >> 6) & 3;
                let low5 = hw2 & 31;
                match (hw1 >> 4) & 0x1f {
                    0x00 | 0x0a => {
                        let base = if rn == 15 { self.pc_value() & !3 } else { self.r[rn as usize] };
                        let sub = (hw1 >> 4) & 0x1f == 0x0a;
                        if rd == 15 {
                            return self.undefined(insn);
                        }
                        self.r[rd as usize] =
                            if sub { base.wrapping_sub(imm12) } else { base.wrapping_add(imm12) };
                        Ok(())
                    }
                    0x04 => self.a32(bus, 0x0300_0000 | (imm16 >> 12) << 16 | rd << 12 | imm16 & 0xfff),
                    0x0c => self.a32(bus, 0x0340_0000 | (imm16 >> 12) << 16 | rd << 12 | imm16 & 0xfff),
                    op @ (0x10 | 0x12 | 0x18 | 0x1a) => {
                        let unsigned = op >= 0x18;
                        let sh = (op >> 1) & 1;
                        if sh == 1 && shift == 0 {
                            // SSAT16/USAT16
                            let base = if unsigned { 0x06e0_0f30 } else { 0x06a0_0f30 };
                            return self.a32(bus, base | (low5 & 15) << 16 | rd << 12 | rn);
                        }
                        let base = if unsigned { 0x06e0_0010 } else { 0x06a0_0010 };
                        self.a32(bus, base | low5 << 16 | rd << 12 | shift << 7 | sh << 6 | rn)
                    }
                    0x14 => self.a32(bus, 0x07a0_0050 | low5 << 16 | rd << 12 | shift << 7 | rn),
                    0x1c => self.a32(bus, 0x07e0_0050 | low5 << 16 | rd << 12 | shift << 7 | rn),
                    0x16 => self.a32(bus, 0x07c0_0010 | low5 << 16 | rd << 12 | shift << 7 | rn),
                    _ => self.undefined(insn),
                }
            }
            _ => {
                // Advanced SIMD: 1111 1111 data processing (U=1), 1111 1001 xxx0 element
                // and structure loads/stores.
                if hw1 & 0xff00 == 0xff00 || hw1 & 0xff10 == 0xf900 {
                    return self.thumb32_simd(bus, hw1, hw2);
                }
                if op2 & 0x71 == 0x00 || op2 & 0x67 == 0x01 || op2 & 0x67 == 0x03 || op2 & 0x67 == 0x05 {
                    return self.thumb32_load_store(bus, hw1, hw2);
                }
                if op2 & 0x70 == 0x20 {
                    return self.thumb32_dp_register(bus, hw1, hw2);
                }
                if op2 & 0x78 == 0x30 {
                    return self.thumb32_multiply(bus, hw1, hw2);
                }
                if op2 & 0x78 == 0x38 {
                    return self.thumb32_long_multiply(bus, hw1, hw2);
                }
                self.undefined(insn)
            }
        }
    }

    /// Advanced SIMD: the T32 forms are the A32 ones with 111U 1111 in place of 1111 001U
    /// (data processing) and 1111 1001 in place of 1111 0100 (loads/stores).
    fn thumb32_simd(&mut self, bus: &mut dyn Bus, hw1: u32, hw2: u32) -> Result<(), Trap> {
        let low = (hw1 & 0xff) << 16 | hw2;
        let word = if hw1 & 0xef00 == 0xef00 {
            0xf200_0000 | (hw1 >> 12 & 1) << 24 | low
        } else {
            0xf400_0000 | low
        };
        self.execute(bus, self.cur_pc as u64, word)
    }

    /// Data processing with a modified immediate or shifted register (native: ORN and
    /// Thumb immediates have no A32 equivalent).
    #[allow(clippy::too_many_arguments)]
    fn thumb32_dp(
        &mut self,
        insn: u32,
        op: u32,
        s: bool,
        rn: u32,
        rd: u32,
        operand: u32,
        shifter_carry: bool,
    ) -> Result<(), Trap> {
        let left = if rn == 15 { 0 } else { self.r[rn as usize] };
        let carry_in = self.cpsr & C != 0;
        let (result, arithmetic) = match op {
            0 => (left & operand, None),
            1 => (left & !operand, None),
            2 => (left | operand, None),
            3 => (left | !operand, None),
            4 => (left ^ operand, None),
            8 => {
                let r = Self::add_with_carry(left, operand, false);
                (r.0, Some((left, operand, r)))
            }
            10 => {
                let r = Self::add_with_carry(left, operand, carry_in);
                (r.0, Some((left, operand, r)))
            }
            11 => {
                let r = Self::add_with_carry(left, !operand, carry_in);
                (r.0, Some((left, !operand, r)))
            }
            13 => {
                let r = Self::add_with_carry(left, !operand, true);
                (r.0, Some((left, !operand, r)))
            }
            14 => {
                let r = Self::add_with_carry(operand, !left, true);
                (r.0, Some((operand, !left, r)))
            }
            _ => return self.undefined(insn),
        };
        let compare = rd == 15 && s && matches!(op, 0 | 4 | 8 | 13);
        if rd == 15 && !compare {
            return self.undefined(insn);
        }
        if !compare {
            self.r[rd as usize] = result;
        }
        if s {
            match arithmetic {
                Some((a, b, (value, carry, overflow))) => self.set_add_flags(a, b, value, carry, overflow),
                None => self.set_logic_flags(result, shifter_carry),
            }
        }
        Ok(())
    }

    fn thumb32_dp_shifted(&mut self, bus: &mut dyn Bus, hw1: u32, hw2: u32) -> Result<(), Trap> {
        let op = (hw1 >> 5) & 15;
        let rn = hw1 & 15;
        let rd = (hw2 >> 8) & 15;
        let rm = hw2 & 15;
        let imm5 = ((hw2 >> 12) & 7) << 2 | (hw2 >> 6) & 3;
        let kind = (hw2 >> 4) & 3;
        if op == 6 {
            // PKHBT/PKHTB
            return self.a32(bus, 0x0680_0010 | rn << 16 | rd << 12 | imm5 << 7 | (kind & 2) << 5 | rm);
        }
        let (operand, carry) = Self::shift(self.read_thumb_reg(rm), kind, imm5, self.cpsr & C != 0, true);
        self.thumb32_dp(hw1 << 16 | hw2, op, hw1 & 0x10 != 0, rn, rd, operand, carry)
    }

    fn thumb32_dual_exclusive(&mut self, bus: &mut dyn Bus, hw1: u32, hw2: u32) -> Result<(), Trap> {
        let insn = hw1 << 16 | hw2;
        let rn = hw1 & 15;
        let rt = hw2 >> 12;
        let rd = (hw2 >> 8) & 15;
        match hw1 & 0xfff0 {
            // LDREX/STREX Rt, [Rn, #imm8*4]: T32 has an offset (Linux's Thumb-2 exception
            // exit clears the monitor with `strex r2, r1, [sp, #S_LR]`).
            0xe840 | 0xe850 if rt == 15 || rn == 15 || hw1 & 0xfff0 == 0xe840 && rd == 15 => self.undefined(insn),
            0xe850 => {
                let address = self.r[rn as usize].wrapping_add((hw2 & 0xff) << 2);
                match self.mem_read32(bus, address, Access::Read) {
                    Ok(value) => {
                        self.r[rt as usize] = value;
                        self.mark_exclusive(address, 4, value as u64);
                        Ok(())
                    }
                    Err((va, status)) => Err(self.data_abort(va, status, false)),
                }
            }
            0xe840 => {
                let address = self.r[rn as usize].wrapping_add((hw2 & 0xff) << 2);
                let value = self.r[rt as usize] as u64;
                match self.store_exclusive(bus, address, 4, value) {
                    Ok(status) => self.r[rd as usize] = status,
                    Err((va, status)) => return Err(self.data_abort(va, status, true)),
                }
                Ok(())
            }
            // T32 load-acquire/store-release: LDA/STL{B,H,} (op 8-10) and LDAEX/STLEX{B,H,,D}
            // (op 12-15), as A32 encodings
            0xe8c0 | 0xe8d0 if (hw2 >> 4) & 8 != 0 => {
                let (op, load) = ((hw2 >> 4) & 15, hw1 & 0x10 != 0);
                let size = [0b10, 0b11, 0b00, 0b01][(op & 3) as usize] << 21;
                let excl = op & 4 != 0;
                if op & 7 == 3 && !excl || (op & 3 == 3) && (rt == 15 || rd == 15) {
                    return self.undefined(insn);
                }
                let status = hw2 & 15;
                if op == 15 {
                    // LDAEXD/STLEXD: T32 allows any Rt/Rt2 pair, so as LDREXD/STREXD above
                    self.barrier();
                    let r = self.thumb32_dual_exclusive(bus, hw1, hw2 & !0x80);
                    self.barrier();
                    return r;
                }
                let a32 = 0xe180_0c90 | size | u32::from(load) << 20 | rn << 16 | u32::from(excl) << 9;
                let a32 = if load { a32 | rt << 12 | 0xf } else { a32 | (if excl { status } else { 15 }) << 12 | rt };
                self.a32(bus, a32)
            }
            0xe8c0 => {
                let status = hw2 & 15;
                match (hw2 >> 4) & 15 {
                    4 => self.a32(bus, 0x01c0_0f90 | rn << 16 | status << 12 | rt),
                    5 => self.a32(bus, 0x01e0_0f90 | rn << 16 | status << 12 | rt),
                    7 => {
                        // STREXD: T32 allows any Rt/Rt2 pair (A32 needs an even pair).
                        let address = self.r[rn as usize];
                        let value = self.r[rt as usize] as u64 | (self.r[rd as usize] as u64) << 32;
                        match self.store_exclusive(bus, address, 8, value) {
                            Ok(s) => self.r[status as usize] = s,
                            Err((va, s)) => return Err(self.data_abort(va, s, true)),
                        }
                        Ok(())
                    }
                    _ => self.undefined(insn),
                }
            }
            0xe8d0 => match (hw2 >> 4) & 15 {
                0 | 1 => {
                    // TBB/TBH
                    let rm = hw2 & 15;
                    let base = self.read_thumb_reg(rn);
                    let half = hw2 & 0x10 != 0;
                    let entry = if half {
                        self.mem_read16(bus, base.wrapping_add(self.r[rm as usize] << 1)).map(u32::from)
                    } else {
                        self.mem_read8(bus, base.wrapping_add(self.r[rm as usize]), Access::Read).map(u32::from)
                    };
                    match entry {
                        Ok(offset) => {
                            let target = self.pc_value().wrapping_add(offset << 1);
                            self.branch_to(target);
                            Ok(())
                        }
                        Err((va, status)) => Err(self.data_abort(va, status, false)),
                    }
                }
                4 => self.a32(bus, 0x01d0_0f9f | rn << 16 | rt << 12),
                5 => self.a32(bus, 0x01f0_0f9f | rn << 16 | rt << 12),
                7 => {
                    // LDREXD: any Rt/Rt2 pair.
                    let address = self.r[rn as usize];
                    let first = self.mem_read32(bus, address, Access::Read);
                    let second =
                        first.and_then(|_| self.mem_read32(bus, address.wrapping_add(4), Access::Read));
                    match (first, second) {
                        (Ok(a), Ok(b)) => {
                            self.r[rt as usize] = a;
                            self.r[rd as usize] = b;
                            self.mark_exclusive(address, 8, a as u64 | (b as u64) << 32);
                            Ok(())
                        }
                        (Err((va, status)), _) | (_, Err((va, status))) => {
                            Err(self.data_abort(va, status, false))
                        }
                    }
                }
                _ => self.undefined(insn),
            },
            _ => {
                // LDRD/STRD (immediate), any register pair.
                let (p, u, w, l) = (hw1 & 0x100 != 0, hw1 & 0x80 != 0, hw1 & 0x20 != 0, hw1 & 0x10 != 0);
                if !p && !w {
                    return self.undefined(insn);
                }
                let rt2 = rd;
                let imm = (hw2 & 0xff) << 2;
                let base = if rn == 15 { self.pc_value() & !3 } else { self.r[rn as usize] };
                let offset_address = if u { base.wrapping_add(imm) } else { base.wrapping_sub(imm) };
                let address = if p { offset_address } else { base };
                if l {
                    let first = self.mem_read32(bus, address, Access::Read);
                    let second = first.and_then(|_| self.mem_read32(bus, address.wrapping_add(4), Access::Read));
                    match (first, second) {
                        (Ok(a), Ok(b)) => {
                            if w {
                                self.r[rn as usize] = offset_address;
                            }
                            self.r[rt as usize] = a;
                            self.r[rt2 as usize] = b;
                            Ok(())
                        }
                        (Err((va, status)), _) | (_, Err((va, status))) => Err(self.data_abort(va, status, false)),
                    }
                } else {
                    let (a, b) = (self.r[rt as usize], self.r[rt2 as usize]);
                    self.store(bus, address, 4, a)?;
                    self.store(bus, address.wrapping_add(4), 4, b)?;
                    if w {
                        self.r[rn as usize] = offset_address;
                    }
                    Ok(())
                }
            }
        }
    }

    fn thumb32_load_store(&mut self, bus: &mut dyn Bus, hw1: u32, hw2: u32) -> Result<(), Trap> {
        let insn = hw1 << 16 | hw2;
        let size = match (hw1 >> 5) & 3 {
            0 => 1,
            1 => 2,
            2 => 4,
            _ => return self.undefined(insn),
        };
        let signed = hw1 & 0x100 != 0;
        let load = hw1 & 0x10 != 0;
        let rn = hw1 & 15;
        let rt = hw2 >> 12;
        let (address, writeback) = if rn == 15 {
            if !load {
                return self.undefined(insn);
            }
            let base = self.pc_value() & !3;
            let imm = hw2 & 0xfff;
            (if hw1 & 0x80 != 0 { base.wrapping_add(imm) } else { base.wrapping_sub(imm) }, None)
        } else if hw1 & 0x80 != 0 {
            (self.r[rn as usize].wrapping_add(hw2 & 0xfff), None)
        } else if hw2 & 0x800 != 0 {
            let (p, u, w) = (hw2 & 0x400 != 0, hw2 & 0x200 != 0, hw2 & 0x100 != 0);
            if !p && !w {
                return self.undefined(insn);
            }
            let base = self.r[rn as usize];
            let imm = hw2 & 0xff;
            let offset_address = if u { base.wrapping_add(imm) } else { base.wrapping_sub(imm) };
            (if p { offset_address } else { base }, w.then_some(offset_address))
        } else if hw2 & 0xfc0 == 0 {
            let rm = hw2 & 15;
            (self.r[rn as usize].wrapping_add(self.r[rm as usize] << ((hw2 >> 4) & 3)), None)
        } else {
            return self.undefined(insn);
        };
        if load {
            if rt == 15 && size != 4 {
                return Ok(()); // PLD/PLI hints
            }
            self.load(bus, address, size, signed, rt)?;
            // Base-restored aborts: write back only after the access succeeded. With
            // Rt == Rn the result is UNPREDICTABLE; the loaded value is kept.
            if let Some(value) = writeback.filter(|_| rt != rn) {
                self.r[rn as usize] = value;
            }
            Ok(())
        } else {
            if signed || rt == 15 {
                return self.undefined(insn);
            }
            let value = self.r[rt as usize];
            self.store(bus, address, size, value)?;
            if let Some(value) = writeback {
                self.r[rn as usize] = value;
            }
            Ok(())
        }
    }

    fn thumb32_dp_register(&mut self, bus: &mut dyn Bus, hw1: u32, hw2: u32) -> Result<(), Trap> {
        let insn = hw1 << 16 | hw2;
        let rn = hw1 & 15;
        let rd = (hw2 >> 8) & 15;
        let rm = hw2 & 15;
        let op1 = (hw1 >> 4) & 15;
        let op2 = (hw2 >> 4) & 15;
        if hw2 & 0xf000 != 0xf000 {
            return self.undefined(insn);
        }
        if op1 & 8 == 0 && op2 == 0 {
            // LSL/LSR/ASR/ROR (register) = MOV{S} Rd, Rn, <shift> Rm
            let s = (hw1 >> 4) & 1;
            return self.a32(bus, 0x01a0_0010 | s << 20 | rd << 12 | rm << 8 | ((op1 >> 1) & 3) << 5 | rn);
        }
        if op1 & 8 == 0 && op2 & 8 != 0 {
            let kind = match op1 & 7 {
                0 => 0x6b,
                1 => 0x6f,
                2 => 0x68,
                3 => 0x6c,
                4 => 0x6a,
                5 => 0x6e,
                _ => return self.undefined(insn),
            };
            return self.a32(bus, kind << 20 | 0x70 | rn << 16 | rd << 12 | (op2 & 3) << 10 | rm);
        }
        if op1 & 8 != 0 && op2 & 8 == 0 {
            // Parallel add/subtract.
            let op = match op1 & 7 {
                1 => 0,
                2 => 1,
                6 => 2,
                5 => 3,
                0 => 4,
                4 => 7,
                _ => return self.undefined(insn),
            };
            let prefix = match op2 & 7 {
                0 => 1,
                1 => 2,
                2 => 3,
                4 => 5,
                5 => 6,
                6 => 7,
                _ => return self.undefined(insn),
            };
            return self.a32(bus, 0x0600_0f10 | prefix << 20 | rn << 16 | rd << 12 | op << 5 | rm);
        }
        if op1 & 0xc == 8 && op2 & 0xc == 8 {
            let word = match ((op1 >> 0) & 3, op2 & 3) {
                (0, b) => {
                    let op = match b { 0 => 0, 1 => 2, 2 => 1, _ => 3 };
                    0x0100_0050 | op << 21 | rn << 16 | rd << 12 | rm
                }
                (1, 0) => 0x06bf_0f30 | rd << 12 | rm,
                (1, 1) => 0x06bf_0fb0 | rd << 12 | rm,
                (1, 2) => 0x06ff_0f30 | rd << 12 | rm,
                (1, 3) => 0x06ff_0fb0 | rd << 12 | rm,
                (2, 0) => 0x0680_0fb0 | rn << 16 | rd << 12 | rm,
                (3, 0) => 0x016f_0f10 | rd << 12 | rm,
                _ => return self.undefined(insn),
            };
            return self.a32(bus, word);
        }
        self.undefined(insn)
    }

    fn thumb32_multiply(&mut self, bus: &mut dyn Bus, hw1: u32, hw2: u32) -> Result<(), Trap> {
        let insn = hw1 << 16 | hw2;
        let rn = hw1 & 15;
        let ra = hw2 >> 12;
        let rd = (hw2 >> 8) & 15;
        let rm = hw2 & 15;
        let (n, m) = ((hw2 >> 5) & 1, (hw2 >> 4) & 1);
        let fields = rd << 16 | ra << 12 | rm << 8 | rn;
        let op2 = (hw2 >> 4) & 3;
        let word = match ((hw1 >> 4) & 7, op2) {
            (0, 0) if ra == 15 => 0x0000_0090 | rd << 16 | rm << 8 | rn,
            (0, 0) => 0x0020_0090 | fields,
            (0, 1) => 0x0060_0090 | fields,
            (1, _) if ra == 15 => 0x0160_0080 | n << 5 | m << 6 | rd << 16 | rm << 8 | rn,
            (1, _) => 0x0100_0080 | n << 5 | m << 6 | fields,
            (2, 0 | 1) => 0x0700_0010 | m << 5 | fields,
            (3, 0 | 1) if ra == 15 => 0x0120_00a0 | m << 6 | rd << 16 | rm << 8 | rn,
            (3, 0 | 1) => 0x0120_0080 | m << 6 | fields,
            (4, 0 | 1) => 0x0700_0050 | m << 5 | fields,
            (5, 0 | 1) => 0x0750_0010 | m << 5 | fields,
            (6, 0 | 1) => 0x0750_00d0 | m << 5 | fields,
            (7, 0) => 0x0780_0010 | fields,
            _ => return self.undefined(insn),
        };
        self.a32(bus, word)
    }

    fn thumb32_long_multiply(&mut self, bus: &mut dyn Bus, hw1: u32, hw2: u32) -> Result<(), Trap> {
        let insn = hw1 << 16 | hw2;
        let rn = hw1 & 15;
        let lo = hw2 >> 12;
        let hi = (hw2 >> 8) & 15;
        let rm = hw2 & 15;
        let op2 = (hw2 >> 4) & 15;
        let fields = hi << 16 | lo << 12 | rm << 8 | rn;
        let word = match ((hw1 >> 4) & 7, op2) {
            (0, 0) => 0x00c0_0090 | fields,
            (1, 15) => 0x0710_f010 | hi << 16 | rm << 8 | rn,
            (2, 0) => 0x0080_0090 | fields,
            (3, 15) => 0x0730_f010 | hi << 16 | rm << 8 | rn,
            (4, 0) => 0x00e0_0090 | fields,
            (4, 8..=11) => 0x0140_0080 | ((op2 >> 1) & 1) << 5 | (op2 & 1) << 6 | fields,
            (4, 12 | 13) => 0x0740_0010 | (op2 & 1) << 5 | fields,
            (5, 12 | 13) => 0x0740_0050 | (op2 & 1) << 5 | fields,
            (6, 0) => 0x00a0_0090 | fields,
            (6, 6) => 0x0040_0090 | fields,
            _ => return self.undefined(insn),
        };
        self.a32(bus, word)
    }

    fn thumb32_branch_misc(&mut self, bus: &mut dyn Bus, hw1: u32, hw2: u32) -> Result<(), Trap> {
        let insn = hw1 << 16 | hw2;
        let s = (hw1 >> 10) & 1;
        let (j1, j2) = ((hw2 >> 13) & 1, (hw2 >> 11) & 1);
        match (hw2 >> 12) & 5 {
            0 => {
                let cond = (hw1 >> 6) & 15;
                if cond < 14 {
                    // B<cond>.W
                    let imm = s << 20 | j2 << 19 | j1 << 18 | (hw1 & 0x3f) << 12 | (hw2 & 0x7ff) << 1;
                    if self.condition(cond) {
                        let target = self.pc_value().wrapping_add(sign_extend(imm, 21));
                        self.branch_to(target);
                    }
                    return Ok(());
                }
                self.thumb32_misc_control(bus, hw1, hw2)
            }
            _ => {
                let i1 = !(j1 ^ s) & 1;
                let i2 = !(j2 ^ s) & 1;
                let imm = s << 24 | i1 << 23 | i2 << 22 | (hw1 & 0x3ff) << 12 | (hw2 & 0x7ff) << 1;
                let offset = sign_extend(imm, 25);
                match (hw2 >> 12) & 5 {
                    1 => {
                        let target = self.pc_value().wrapping_add(offset);
                        self.branch_to(target);
                    }
                    5 => {
                        self.r[14] = self.next_pc() | 1;
                        let target = self.pc_value().wrapping_add(offset);
                        self.branch_to(target);
                    }
                    _ => {
                        // BLX immediate: to ARM code.
                        if hw2 & 1 != 0 {
                            return self.undefined(insn);
                        }
                        self.r[14] = self.next_pc() | 1;
                        let target = (self.pc_value() & !3).wrapping_add(offset);
                        self.bx_to(target & !3);
                    }
                }
                Ok(())
            }
        }
    }

    fn thumb32_misc_control(&mut self, bus: &mut dyn Bus, hw1: u32, hw2: u32) -> Result<(), Trap> {
        let insn = hw1 << 16 | hw2;
        match (hw1 >> 4) & 0x7f {
            0x38 | 0x39 => {
                // MSR (register)
                let r = (hw1 >> 4) & 1;
                self.a32(bus, 0x0120_f000 | r << 22 | ((hw2 >> 8) & 15) << 16 | hw1 & 15)
            }
            0x3a => {
                if hw2 & 0x700 == 0 {
                    self.a32(bus, 0x0320_f000 | hw2 & 0xff)
                } else {
                    // CPS.W
                    let imod = (hw2 >> 9) & 3;
                    let m = (hw2 >> 8) & 1;
                    self.execute(
                        bus,
                        self.cur_pc as u64,
                        0xf100_0000 | imod << 18 | m << 17 | (hw2 & 0xe0) << 1 | hw2 & 0x1f,
                    )
                }
            }
            0x3b => {
                let option = hw2 & 15;
                let word = match (hw2 >> 4) & 15 {
                    2 => 0xf57f_f01f,
                    4 => 0xf57f_f040 | option,
                    5 => 0xf57f_f050 | option,
                    6 => 0xf57f_f060 | option,
                    _ => return self.undefined(insn),
                };
                self.execute(bus, self.cur_pc as u64, word)
            }
            0x3d if hw1 & 15 == 14 && hw2 & 0x0f00 == 0x0f00 => {
                // SUBS PC, LR, #imm8: exception return.
                let target = self.r[14].wrapping_sub(hw2 & 0xff);
                self.restore_spsr();
                self.branch_to(target);
                Ok(())
            }
            0x7f if hw2 & 0xf000 == 0x8000 => {
                // SMC #imm4 (T32): the same firmware call as the A32 encoding.
                self.a32(bus, 0x0160_0070 | hw1 & 0xf)
            }
            0x3e | 0x3f => {
                // MRS
                let r = (hw1 >> 4) & 1;
                self.a32(bus, 0x010f_0000 | r << 22 | ((hw2 >> 8) & 15) << 12)
            }
            _ => self.undefined(insn),
        }
    }
}
