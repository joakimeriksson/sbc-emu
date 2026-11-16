//! Thumb (T16/T32) for the block JIT. Most encodings are the A32 instruction the
//! interpreter's Thumb decoder (`thumb.rs`) maps them to, emitted through the A32 paths with
//! Thumb PC reads; branches, CBZ/CBNZ, modified immediates, ORN, 12-bit offsets, literal
//! loads and LDRD with any register pair are translated here. IT blocks are compiled with
//! their ITSTATE tracked statically: each instruction is guarded by its condition (16-bit
//! ALU operations then do not set flags), and ITSTATE reaches the CPSR only when the
//! interpreter runs one of them or the block leaves inside the IT block.
use super::*;

/// How a Thumb instruction is emitted.
enum T {
    /// the equivalent A32 instruction (condition AL)
    A32(u32),
    /// emitted here; true = the block ends
    Done(bool),
    /// interpreted; true = the block ends after it
    Interp(bool),
}

fn sign_extend(value: u32, bits: u32) -> u32 {
    ((value << (32 - bits)) as i32 >> (32 - bits)) as u32
}

impl Emit<'_> {
    /// Emit the Thumb instruction `hw1`(:`hw2`) at `pc`, the `n`-th of the block. Returns
    /// true when the block ends with it.
    pub(super) fn thumb_insn(&mut self, hw1: u32, hw2: u32, pc: u32, n: u32) -> bool {
        let wide = hw1 >> 11 >= 0b11101;
        self.size = if wide { 4 } else { 2 };
        let it = self.it;
        let is_it = !wide && hw1 & 0xff00 == 0xbf00 && hw1 & 0xf != 0;
        if is_it {
            if it != 0 || (hw1 >> 4) & 0xf == 15 {
                // IT inside an IT block, or firstcond 1111: unpredictable, interpret
                self.call_interp(pc, n, true, 0);
                return true;
            }
            self.it = hw1 & 0xff;
            return false;
        }
        let guard = (it != 0 && it >> 4 != 14).then(|| {
            let l = self.label();
            self.br(0x5400_0000 | ((it >> 4) ^ 1), l); // b.<inverse> skip
            l
        });
        let end = self.thumb_body(hw1, hw2, wide, pc, n);
        if it != 0 {
            // ITAdvance
            self.it = if it & 7 == 0 {
                0
            } else {
                it & 0xe0 | (it << 1) & 0x1f
            };
        }
        match guard {
            Some(l) => {
                self.bind(l);
                false
            }
            None => end,
        }
    }

    fn thumb_body(&mut self, hw1: u32, hw2: u32, wide: bool, pc: u32, n: u32) -> bool {
        let t = if wide {
            self.t32(hw1, hw2, pc, n)
        } else {
            self.t16(hw1, pc, n)
        };
        match t {
            T::A32(word) => match self.native(word, pc, n) {
                Some(end) => end,
                None => {
                    let end = writes_pc(word);
                    self.call_interp(pc, n, end, 0);
                    end
                }
            },
            T::Done(end) => end,
            T::Interp(end) => {
                self.call_interp(pc, n, end, 0);
                end
            }
        }
    }

    /// BranchWritePC in Thumb state: to the address in host `r` (bit 0 ignored).
    fn exit_thumb_branch(&mut self, r: u32, count: u32) {
        self.p(and_imm(r, r, 0xffff_fffe));
        self.p(str_w(r, X_CPU, self.reg_off(15)));
        self.save_flags();
        self.add_instret(count);
        self.leave();
    }

    /// A conditional branch to `target`: leave if taken, otherwise continue the block.
    fn cond_branch(&mut self, cond: u32, target: u32, n: u32) -> T {
        let skip = self.label();
        self.br(0x5400_0000 | (cond ^ 1), skip);
        self.exit_const(target, n + 1);
        self.bind(skip);
        T::Done(false)
    }

    fn t16(&mut self, hw: u32, pc: u32, n: u32) -> T {
        // 16-bit ALU operations set the flags outside IT blocks only
        let s = if self.it == 0 { 1 << 20 } else { 0 };
        let (r0, r3, r6, r8) = (hw & 7, (hw >> 3) & 7, (hw >> 6) & 7, (hw >> 8) & 7);
        let imm8 = hw & 0xff;
        let pc4 = pc.wrapping_add(4);
        match hw >> 11 {
            0..=2 => {
                let imm5 = (hw >> 6) & 31;
                return T::A32(0xe1a0_0000 | s | r0 << 12 | imm5 << 7 | (hw >> 11) << 5 | r3);
            }
            3 => {
                let base = match (hw & (1 << 10) != 0, hw & (1 << 9) != 0) {
                    (false, false) => 0x0080_0000,
                    (false, true) => 0x0040_0000,
                    (true, false) => 0x0280_0000,
                    (true, true) => 0x0240_0000,
                };
                return T::A32(0xe000_0000 | base | s | r3 << 16 | r0 << 12 | r6);
            }
            4 => return T::A32(0xe3a0_0000 | s | r8 << 12 | imm8),
            5 => return T::A32(0xe350_0000 | r8 << 16 | imm8),
            6 => return T::A32(0xe280_0000 | s | r8 << 16 | r8 << 12 | imm8),
            7 => return T::A32(0xe240_0000 | s | r8 << 16 | r8 << 12 | imm8),
            _ => {}
        }
        match hw >> 10 {
            0b010000 => {
                let (rm, rdn) = (r3, r0);
                let word = match (hw >> 6) & 15 {
                    0 => s | rdn << 16 | rdn << 12 | rm,
                    1 => 0x0020_0000 | s | rdn << 16 | rdn << 12 | rm,
                    op @ (2 | 3 | 4 | 7) => {
                        let kind = match op {
                            2 => 0,
                            3 => 1,
                            4 => 2,
                            _ => 3,
                        };
                        0x01a0_0010 | s | rdn << 12 | rm << 8 | kind << 5 | rdn
                    }
                    5 => 0x00a0_0000 | s | rdn << 16 | rdn << 12 | rm,
                    6 => 0x00c0_0000 | s | rdn << 16 | rdn << 12 | rm,
                    8 => 0x0110_0000 | rdn << 16 | rm,
                    9 => 0x0260_0000 | s | rm << 16 | rdn << 12,
                    10 => 0x0150_0000 | rdn << 16 | rm,
                    11 => 0x0170_0000 | rdn << 16 | rm,
                    12 => 0x0180_0000 | s | rdn << 16 | rdn << 12 | rm,
                    13 => 0x0000_0090 | s | rdn << 16 | rdn << 8 | rm,
                    14 => 0x01c0_0000 | s | rdn << 16 | rdn << 12 | rm,
                    _ => 0x01e0_0000 | s | rdn << 12 | rm,
                };
                return T::A32(0xe000_0000 | word);
            }
            0b010001 => {
                let rd = (hw >> 4) & 8 | r0;
                let rm = (hw >> 3) & 15;
                return match (hw >> 8) & 3 {
                    0 if rd == 15 => {
                        if rm == 15 {
                            return T::Interp(true);
                        }
                        self.load(W_DATA, rm, pc);
                        self.mov32(W_OP, pc4);
                        self.p(rrr(0x0b00_0000, W_DATA, W_DATA, W_OP));
                        self.exit_thumb_branch(W_DATA, n + 1);
                        T::Done(true)
                    }
                    0 => T::A32(0xe080_0000 | rd << 16 | rd << 12 | rm),
                    1 => T::A32(0xe150_0000 | rd << 16 | rm),
                    2 if rd == 15 => {
                        self.load(W_DATA, rm, pc);
                        self.exit_thumb_branch(W_DATA, n + 1);
                        T::Done(true)
                    }
                    2 => T::A32(0xe1a0_0000 | rd << 12 | rm),
                    _ => {
                        if rm == 15 {
                            return T::Interp(true);
                        }
                        self.load(W_DATA, rm, pc);
                        if hw & 0x80 != 0 {
                            self.mov32(15, pc.wrapping_add(2) | 1);
                            self.p(str_w(15, X_CPU, self.reg_off(14)));
                        }
                        self.exit_bx(W_DATA, n + 1);
                        T::Done(true)
                    }
                };
            }
            _ => {}
        }
        match hw >> 12 {
            0b0100 => T::A32(0xe59f_0000 | r8 << 12 | imm8 * 4), // LDR literal (aligned PC)
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
                T::A32(0xe000_0000 | word | r3 << 16 | r0 << 12 | r6)
            }
            0b0110 | 0b0111 => {
                let byte = hw & (1 << 12) != 0;
                let imm5 = (hw >> 6) & 31;
                let offset = if byte { imm5 } else { imm5 * 4 };
                let load = (hw >> 11) & 1;
                T::A32(
                    0xe580_0000 | u32::from(byte) << 22 | load << 20 | r3 << 16 | r0 << 12 | offset,
                )
            }
            0b1000 => {
                let offset = ((hw >> 6) & 31) * 2;
                let load = (hw >> 11) & 1;
                T::A32(
                    0xe1c0_00b0
                        | load << 20
                        | r3 << 16
                        | r0 << 12
                        | (offset >> 4) << 8
                        | offset & 15,
                )
            }
            0b1001 => T::A32(0xe58d_0000 | ((hw >> 11) & 1) << 20 | r8 << 12 | imm8 * 4),
            0b1010 => {
                if hw & (1 << 11) == 0 {
                    self.mov32(W_RES, (pc4 & !3).wrapping_add(imm8 * 4)); // ADR
                } else {
                    self.load(W_RN, 13, pc);
                    self.p(addi_w(W_RES, W_RN, imm8 * 4, false));
                }
                self.store(W_RES, r8);
                T::Done(false)
            }
            0b1011 => self.t16_misc(hw, pc, n),
            0b1100 => {
                let load = hw & (1 << 11) != 0;
                let writeback = !load || imm8 & (1 << r8) == 0;
                T::A32(
                    0xe880_0000
                        | u32::from(writeback) << 21
                        | u32::from(load) << 20
                        | r8 << 16
                        | imm8,
                )
            }
            0b1101 => match (hw >> 8) & 15 {
                14 | 15 => T::Interp(true), // UDF, SVC
                cond => self.cond_branch(cond, pc4.wrapping_add(sign_extend(imm8 << 1, 9)), n),
            },
            0b1110 if hw & 0x0800 == 0 => {
                self.exit_const(pc4.wrapping_add(sign_extend((hw & 0x7ff) << 1, 12)), n + 1);
                T::Done(true)
            }
            _ => T::Interp(true),
        }
    }

    fn t16_misc(&mut self, hw: u32, pc: u32, n: u32) -> T {
        let (r0, r3) = (hw & 7, (hw >> 3) & 7);
        match (hw >> 8) & 15 {
            0b0000 => {
                // ADD/SUB SP, SP, #imm7 * 4
                self.load(W_RN, 13, pc);
                self.p(addi_w(W_RN, W_RN, (hw & 0x7f) * 4, hw & 0x80 != 0));
                self.store(W_RN, 13);
                T::Done(false)
            }
            0b0001 | 0b0011 | 0b1001 | 0b1011 => {
                // CBZ/CBNZ: leave for the target if taken
                let offset = (hw >> 3) & 0x1f | (hw >> 4) & 0x20;
                let target = pc.wrapping_add(4).wrapping_add(offset << 1);
                self.load(W_DATA, r0, pc);
                let skip = self.label();
                let nonzero = hw & (1 << 11) != 0;
                // CBZ is taken on zero: skip on non-zero (cbnz), and the reverse for CBNZ
                self.br(
                    if nonzero { 0x3400_0000 } else { 0x3500_0000 } | W_DATA,
                    skip,
                );
                self.exit_const(target, n + 1);
                self.bind(skip);
                T::Done(false)
            }
            0b0010 => {
                let word = match (hw >> 6) & 3 {
                    0 => 0x06bf_0070,
                    1 => 0x06af_0070,
                    2 => 0x06ff_0070,
                    _ => 0x06ef_0070,
                };
                T::A32(0xe000_0000 | word | r0 << 12 | r3)
            }
            0b0100 | 0b0101 => T::A32(0xe92d_0000 | (hw & 0xff) | (hw & 0x100) << 6),
            0b1010 => match (hw >> 6) & 3 {
                0 => T::A32(0xe6bf_0f30 | r0 << 12 | r3),
                1 => T::A32(0xe6bf_0fb0 | r0 << 12 | r3),
                3 => T::A32(0xe6ff_0fb0 | r0 << 12 | r3),
                _ => T::Interp(true),
            },
            0b1100 | 0b1101 => T::A32(0xe8bd_0000 | (hw & 0xff) | (hw & 0x100) << 7),
            0b1111 if hw & 0xf == 0 => T::A32(0xe320_f000 | (hw >> 4) & 0xf), // hints
            // CPS, BKPT, IT (changes ITSTATE: the block ends), undefined
            _ => T::Interp(true),
        }
    }

    fn t32(&mut self, hw1: u32, hw2: u32, pc: u32, n: u32) -> T {
        let insn = hw1 << 16 | hw2;
        let op2 = (hw1 >> 4) & 0x7f;
        match (hw1 >> 11) & 3 {
            1 => {
                if op2 & 0x64 == 0x00 {
                    let (rn, w, l) = (hw1 & 15, (hw1 >> 5) & 1, (hw1 >> 4) & 1);
                    let mode = match (hw1 >> 7) & 3 {
                        1 => 0x0880_0000,
                        2 => 0x0900_0000,
                        _ => return T::Interp(true), // RFE, SRS
                    };
                    return T::A32(0xe000_0000 | mode | w << 21 | l << 20 | rn << 16 | hw2);
                }
                if op2 & 0x64 == 0x04 {
                    return match hw1 & 0xfff0 {
                        // exclusives, TBB/TBH
                        0xe840 | 0xe850 | 0xe8c0 => T::Interp(false),
                        0xe8d0 => T::Interp((hw2 >> 4) & 15 <= 1),
                        _ => self.t_ldrd(hw1, hw2, pc, n),
                    };
                }
                if op2 & 0x60 == 0x20 {
                    let op = (hw1 >> 5) & 15;
                    let (rn, rd, rm) = (hw1 & 15, (hw2 >> 8) & 15, hw2 & 15);
                    let imm5 = ((hw2 >> 12) & 7) << 2 | (hw2 >> 6) & 3;
                    let kind = (hw2 >> 4) & 3;
                    if op == 6 {
                        return T::A32(
                            0xe680_0010 | rn << 16 | rd << 12 | imm5 << 7 | (kind & 2) << 5 | rm,
                        );
                    }
                    // the A32 shifted-register operand form computes the same value and carry
                    let Some(carry) = self.operand2(imm5 << 7 | kind << 5 | rm, pc, W_OP) else {
                        return T::Interp(false);
                    };
                    return self.t_dp(op, hw1 & 0x10 != 0, rn, rd, carry, pc);
                }
                if hw1 & 0x0300 == 0x0300 {
                    return T::Interp(false); // Advanced SIMD
                }
                if hw1 >> 12 == 0xe {
                    T::A32(insn) // coprocessor (CP15 thread registers, cache maintenance)
                } else {
                    T::Interp(false)
                }
            }
            2 => {
                if hw2 & 0x8000 != 0 {
                    return self.t32_branch(hw1, hw2, pc, n);
                }
                let i = (hw1 >> 10) & 1;
                let (imm3, rd, rn, imm8) = ((hw2 >> 12) & 7, (hw2 >> 8) & 15, hw1 & 15, hw2 & 0xff);
                let imm12 = i << 11 | imm3 << 8 | imm8;
                if op2 & 0x20 == 0 {
                    // modified immediate
                    let (value, carry) = crate::thumb::expand_imm(imm12, false);
                    self.mov32(W_OP, value);
                    let carry = if imm12 >> 10 == 0 {
                        Carry::Unchanged
                    } else {
                        Carry::Const(carry)
                    };
                    return self.t_dp((hw1 >> 5) & 15, hw1 & 0x10 != 0, rn, rd, carry, pc);
                }
                let imm16 = (hw1 & 15) << 12 | imm12;
                let shift = imm3 << 2 | (hw2 >> 6) & 3;
                let low5 = hw2 & 31;
                match (hw1 >> 4) & 0x1f {
                    op @ (0x00 | 0x0a) => {
                        // ADDW/SUBW, ADR
                        if rd == 15 {
                            return T::Interp(true);
                        }
                        let sub = op == 0x0a;
                        if rn == 15 {
                            let base = pc.wrapping_add(4) & !3;
                            self.mov32(
                                W_RES,
                                if sub {
                                    base.wrapping_sub(imm12)
                                } else {
                                    base.wrapping_add(imm12)
                                },
                            );
                        } else {
                            self.load(W_RN, rn, pc);
                            self.p(addi_w(W_RES, W_RN, imm12, sub));
                        }
                        self.store(W_RES, rd);
                        T::Done(false)
                    }
                    0x04 => T::A32(0xe300_0000 | (imm16 >> 12) << 16 | rd << 12 | imm16 & 0xfff),
                    0x0c => T::A32(0xe340_0000 | (imm16 >> 12) << 16 | rd << 12 | imm16 & 0xfff),
                    0x14 => T::A32(0xe7a0_0050 | low5 << 16 | rd << 12 | shift << 7 | rn),
                    0x1c => T::A32(0xe7e0_0050 | low5 << 16 | rd << 12 | shift << 7 | rn),
                    0x16 => T::A32(0xe7c0_0010 | low5 << 16 | rd << 12 | shift << 7 | rn),
                    _ => T::Interp(false), // saturation
                }
            }
            _ => {
                if hw1 & 0xff00 == 0xff00 || hw1 & 0xff10 == 0xf900 {
                    return T::Interp(false); // Advanced SIMD
                }
                if op2 & 0x71 == 0x00
                    || op2 & 0x67 == 0x01
                    || op2 & 0x67 == 0x03
                    || op2 & 0x67 == 0x05
                {
                    return self.t_ldst(hw1, hw2, pc, n);
                }
                if op2 & 0x70 == 0x20 {
                    return self.t32_dp_register(hw1, hw2);
                }
                if op2 & 0x78 == 0x30 {
                    return Self::t32_multiply(hw1, hw2);
                }
                if op2 & 0x78 == 0x38 {
                    return Self::t32_long_multiply(hw1, hw2);
                }
                T::Interp(true)
            }
        }
    }

    /// T32 data processing (modified immediate or shifted register) with operand 2 in
    /// `W_OP`. `op` is the T32 opcode; Rd = PC with S selects TST/TEQ/CMN/CMP.
    fn t_dp(&mut self, op: u32, s: bool, rn: u32, rd: u32, carry: Carry, pc: u32) -> T {
        let compare = rd == 15 && s && matches!(op, 0 | 4 | 8 | 13);
        if rd == 15 && !compare || !matches!(op, 0..=4 | 8 | 10 | 11 | 13 | 14) {
            return T::Interp(true);
        }
        if rn == 15 {
            self.p(0x5280_0000 | W_RN); // Rn = PC reads as zero here (ORR/ORN: MOV/MVN)
        } else {
            self.load(W_RN, rn, pc);
        }
        let (a, b) = (W_RN, W_OP);
        let dst = if compare { ZR } else { W_RES };
        let w = match op {
            0 => rrr(0x0a00_0000, W_RES, a, b),
            1 => rrr(0x0a20_0000, W_RES, a, b),
            2 => rrr(0x2a00_0000, W_RES, a, b),
            3 => rrr(0x2a20_0000, W_RES, a, b),
            4 => rrr(0x4a00_0000, W_RES, a, b),
            8 => rrr(if s { 0x2b00_0000 } else { 0x0b00_0000 }, dst, a, b),
            10 => rrr(if s { 0x3a00_0000 } else { 0x1a00_0000 }, dst, a, b),
            11 => rrr(if s { 0x7a00_0000 } else { 0x5a00_0000 }, dst, a, b),
            13 => rrr(if s { 0x6b00_0000 } else { 0x4b00_0000 }, dst, a, b),
            _ => rrr(if s { 0x6b00_0000 } else { 0x4b00_0000 }, dst, b, a),
        };
        self.p(w);
        if s && op <= 4 {
            self.logic_flags(W_RES, carry);
        }
        if !compare {
            self.store(W_RES, rd);
        }
        T::Done(false)
    }

    fn t32_branch(&mut self, hw1: u32, hw2: u32, pc: u32, n: u32) -> T {
        let s = (hw1 >> 10) & 1;
        let (j1, j2) = ((hw2 >> 13) & 1, (hw2 >> 11) & 1);
        let pc4 = pc.wrapping_add(4);
        if (hw2 >> 12) & 5 == 0 {
            let cond = (hw1 >> 6) & 15;
            if cond < 14 {
                let imm = s << 20 | j2 << 19 | j1 << 18 | (hw1 & 0x3f) << 12 | (hw2 & 0x7ff) << 1;
                return self.cond_branch(cond, pc4.wrapping_add(sign_extend(imm, 21)), n);
            }
            return match (hw1 >> 4) & 0x7f {
                // NOP-class hints
                0x3a if hw2 & 0x700 == 0 => T::A32(0xe320_f000 | hw2 & 0xff),
                // DMB/DSB/ISB
                0x3b if matches!((hw2 >> 4) & 15, 4..=6) => T::Done(false),
                0x3b => T::Interp(false), // CLREX
                // MRS
                0x3e | 0x3f => {
                    T::A32(0xe10f_0000 | ((hw1 >> 4) & 1) << 22 | ((hw2 >> 8) & 15) << 12)
                }
                _ => T::Interp(true), // MSR, CPS, SUBS PC, LR, SMC
            };
        }
        let i1 = !(j1 ^ s) & 1;
        let i2 = !(j2 ^ s) & 1;
        let imm = s << 24 | i1 << 23 | i2 << 22 | (hw1 & 0x3ff) << 12 | (hw2 & 0x7ff) << 1;
        let offset = sign_extend(imm, 25);
        match (hw2 >> 12) & 5 {
            1 => {
                self.exit_const(pc4.wrapping_add(offset), n + 1);
                T::Done(true)
            }
            5 => {
                self.mov32(15, pc4 | 1);
                self.p(str_w(15, X_CPU, self.reg_off(14)));
                self.exit_const(pc4.wrapping_add(offset), n + 1);
                T::Done(true)
            }
            _ => {
                if hw2 & 1 != 0 {
                    return T::Interp(true);
                }
                // BLX immediate: to ARM code
                self.mov32(15, pc4 | 1);
                self.p(str_w(15, X_CPU, self.reg_off(14)));
                self.mov32(15, (pc4 & !3).wrapping_add(offset) & !3);
                self.p(str_w(15, X_CPU, self.reg_off(15)));
                self.p(ldr_w(15, X_CPU, self.c.cpsr));
                self.p(and_imm(15, 15, !(1 << 5)));
                self.p(str_w(15, X_CPU, self.c.cpsr));
                self.save_flags();
                self.add_instret(n + 1);
                self.leave();
                T::Done(true)
            }
        }
    }

    /// LDR/STR (byte, halfword, word; signed loads) in the T32 immediate, indexed,
    /// register and literal forms.
    fn t_ldst(&mut self, hw1: u32, hw2: u32, pc: u32, n: u32) -> T {
        let size = match (hw1 >> 5) & 3 {
            0 => 1,
            1 => 2,
            2 => 4,
            _ => return T::Interp(true),
        };
        let (signed, load, rn, rt) = (hw1 & 0x100 != 0, hw1 & 0x10 != 0, hw1 & 15, hw2 >> 12);
        if !load && (signed || rt == 15) {
            return T::Interp(true);
        }
        if load && rt == 15 && size != 4 {
            return T::Done(false); // PLD/PLI
        }
        // (pre-indexed address register, writeback)
        let (a, writeback) = if rn == 15 {
            if !load {
                return T::Interp(true);
            }
            let base = pc.wrapping_add(4) & !3;
            let imm = hw2 & 0xfff;
            self.mov32(
                W_ADDR,
                if hw1 & 0x80 != 0 {
                    base.wrapping_add(imm)
                } else {
                    base.wrapping_sub(imm)
                },
            );
            (W_ADDR, false)
        } else if hw1 & 0x80 != 0 {
            self.load(W_RN, rn, pc);
            self.p(addi_w(W_ADDR, W_RN, hw2 & 0xfff, false));
            (W_ADDR, false)
        } else if hw2 & 0x800 != 0 {
            let (p, u, w) = (hw2 & 0x400 != 0, hw2 & 0x200 != 0, hw2 & 0x100 != 0);
            if !p && !w || w && load && rt == rn {
                return T::Interp(true);
            }
            self.load(W_RN, rn, pc);
            self.p(addi_w(W_ADDR, W_RN, hw2 & 0xff, !u));
            (if p { W_ADDR } else { W_RN }, w)
        } else if hw2 & 0xfc0 == 0 {
            let rm = hw2 & 15;
            if rm == 15 {
                return T::Interp(true);
            }
            self.load(W_RN, rn, pc);
            self.load(W_OP, rm, pc);
            self.p(0x0b00_0000 | ((hw2 >> 4) & 3) << 10 | W_OP << 16 | W_RN << 5 | W_ADDR); // add w12, w9, w10, lsl #sh
            (W_ADDR, false)
        } else {
            return T::Interp(true);
        };
        let (slow, resume) = self.slow_path(pc, n);
        self.probe(a, size, !load, slow);
        if load {
            let op = match (size, signed) {
                (1, false) => 0x3860_4800,
                (1, true) => 0x38e0_4800,
                (2, false) => 0x7860_4800,
                (2, true) => 0x78e0_4800,
                _ => 0xb860_4800,
            };
            self.p(op | a << 16 | 16 << 5 | W_DATA);
            if writeback {
                self.store(W_ADDR, rn);
            }
            if rt == 15 {
                self.exit_bx(W_DATA, n + 1);
                return T::Done(self.after_pc_load(resume, 0xe000_0000, pc, n));
            }
            self.store(W_DATA, rt);
        } else {
            self.load(W_DATA, rt, pc);
            let op = match size {
                1 => 0x3820_4800,
                2 => 0x7820_4800,
                _ => 0xb820_4800,
            };
            self.p(op | a << 16 | 16 << 5 | W_DATA);
            if writeback {
                self.store(W_ADDR, rn);
            }
        }
        self.bind(resume);
        T::Done(false)
    }

    /// LDRD/STRD (immediate) with any register pair.
    fn t_ldrd(&mut self, hw1: u32, hw2: u32, pc: u32, n: u32) -> T {
        let (p, u, w, l) = (
            hw1 & 0x100 != 0,
            hw1 & 0x80 != 0,
            hw1 & 0x20 != 0,
            hw1 & 0x10 != 0,
        );
        let (rn, rt, rt2) = (hw1 & 15, hw2 >> 12, (hw2 >> 8) & 15);
        if !p && !w || rt == 15 || rt2 == 15 || w && rn == 15 || !l && rn == 15 {
            return T::Interp(true);
        }
        let imm = (hw2 & 0xff) << 2;
        let a = self.address(rn, pc, p, u, Some(imm));
        let (slow, resume) = self.slow_path(pc, n);
        self.probe_pair(a, !l, slow);
        self.p(0x8b20_4000 | a << 16 | 16 << 5 | 16); // add x16, x16, wa, uxtw
        if l {
            self.p(0x2940_0000 | W_SRC << 10 | 16 << 5 | W_DATA); // ldp w13, w14, [x16]
            if w {
                self.store(W_ADDR, rn);
            }
            self.store(W_DATA, rt);
            self.store(W_SRC, rt2);
        } else {
            self.load(W_DATA, rt, pc);
            self.load(W_SRC, rt2, pc);
            self.p(0x2900_0000 | W_SRC << 10 | 16 << 5 | W_DATA); // stp w13, w14, [x16]
            if w {
                self.store(W_ADDR, rn);
            }
        }
        self.bind(resume);
        T::Done(false)
    }

    fn t32_dp_register(&mut self, hw1: u32, hw2: u32) -> T {
        let (rn, rd, rm) = (hw1 & 15, (hw2 >> 8) & 15, hw2 & 15);
        let (op1, op2) = ((hw1 >> 4) & 15, (hw2 >> 4) & 15);
        if hw2 & 0xf000 != 0xf000 {
            return T::Interp(true);
        }
        if op1 & 8 == 0 && op2 == 0 {
            let s = (hw1 >> 4) & 1;
            return T::A32(0xe1a0_0010 | s << 20 | rd << 12 | rm << 8 | ((op1 >> 1) & 3) << 5 | rn);
        }
        if op1 & 8 == 0 && op2 & 8 != 0 {
            let kind = match op1 & 7 {
                0 => 0x6b,
                1 => 0x6f,
                2 => 0x68,
                3 => 0x6c,
                4 => 0x6a,
                5 => 0x6e,
                _ => return T::Interp(true),
            };
            return T::A32(
                0xe000_0000 | kind << 20 | 0x70 | rn << 16 | rd << 12 | (op2 & 3) << 10 | rm,
            );
        }
        if op1 & 0xc == 8 && op2 & 0xc == 8 {
            return match (op1 & 3, op2 & 3) {
                (1, 0) => T::A32(0xe6bf_0f30 | rd << 12 | rm),
                (1, 1) => T::A32(0xe6bf_0fb0 | rd << 12 | rm),
                (1, 2) => T::A32(0xe6ff_0f30 | rd << 12 | rm),
                (3, 0) => T::A32(0xe16f_0f10 | rd << 12 | rm),
                _ => T::Interp(false),
            };
        }
        T::Interp(false)
    }

    fn t32_multiply(hw1: u32, hw2: u32) -> T {
        let (rn, ra, rd, rm) = (hw1 & 15, hw2 >> 12, (hw2 >> 8) & 15, hw2 & 15);
        match ((hw1 >> 4) & 7, (hw2 >> 4) & 3) {
            (0, 0) if ra == 15 => T::A32(0xe000_0090 | rd << 16 | rm << 8 | rn),
            (0, 0) => T::A32(0xe020_0090 | rd << 16 | ra << 12 | rm << 8 | rn),
            _ => T::Interp(false),
        }
    }

    fn t32_long_multiply(hw1: u32, hw2: u32) -> T {
        let (rn, lo, hi, rm) = (hw1 & 15, hw2 >> 12, (hw2 >> 8) & 15, hw2 & 15);
        let fields = hi << 16 | lo << 12 | rm << 8 | rn;
        match ((hw1 >> 4) & 7, (hw2 >> 4) & 15) {
            (0, 0) => T::A32(0xe0c0_0090 | fields),
            (1, 15) => T::A32(0xe710_f010 | hi << 16 | rm << 8 | rn),
            (2, 0) => T::A32(0xe080_0090 | fields),
            (3, 15) => T::A32(0xe730_f010 | hi << 16 | rm << 8 | rn),
            (4, 0) => T::A32(0xe0e0_0090 | fields),
            (6, 0) => T::A32(0xe0a0_0090 | fields),
            _ => T::Interp(false),
        }
    }
}
