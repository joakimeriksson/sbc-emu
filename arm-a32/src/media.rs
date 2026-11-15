//! ARMv7 A32 multiply, saturating, parallel and packing instructions.
//!
//! `execute_media` is called after the condition check and before the general decoders;
//! it returns `None` for encodings it does not own.
use crate::cpu::Cpu;
use emu_core::{Bus, Trap};

const Q: u32 = 1 << 27;

fn signed_saturate(value: i64, bits: u32) -> (u32, bool) {
    let max = (1i64 << (bits - 1)) - 1;
    let min = -(1i64 << (bits - 1));
    if value > max {
        (max as u32, true)
    } else if value < min {
        (min as u32, true)
    } else {
        (value as u32, false)
    }
}

fn unsigned_saturate(value: i64, bits: u32) -> (u32, bool) {
    let max = if bits == 32 { u32::MAX as i64 } else { (1i64 << bits) - 1 };
    if value > max {
        (max as u32, true)
    } else if value < 0 {
        (0, true)
    } else {
        (value as u32, false)
    }
}

fn lo16(v: u32) -> i32 {
    v as u16 as i16 as i32
}

fn hi16(v: u32) -> i32 {
    (v >> 16) as u16 as i16 as i32
}

impl Cpu {
    fn set_q(&mut self, saturated: bool) {
        if saturated {
            self.cpsr |= Q;
        }
    }

    fn set_ge(&mut self, ge: u32) {
        self.cpsr = (self.cpsr & !(0xf << 16)) | (ge & 0xf) << 16;
    }

    pub(crate) fn execute_media(
        &mut self,
        bus: &mut dyn Bus,
        pc: u64,
        insn: u32,
    ) -> Option<Result<(), Trap>> {
        let unimplemented = Err(Trap::Unimplemented(pc, insn));
        let r = |n: u32| (n & 0xf) as usize;
        let uses_pc = |regs: &[u32]| regs.iter().any(|&n| n & 0xf == 15);

        // MLS and UMAAL (multiply space, op1 = 0x6 / 0x4).
        match insn & 0x0ff0_00f0 {
            0x0060_0090 => {
                let (rd, ra, rm, rn) = (insn >> 16, insn >> 12, insn >> 8, insn);
                if uses_pc(&[rd, ra, rm, rn]) {
                    return Some(unimplemented);
                }
                let product = self.r[r(rn)].wrapping_mul(self.r[r(rm)]);
                self.r[r(rd)] = self.r[r(ra)].wrapping_sub(product);
                return Some(Ok(()));
            }
            0x0040_0090 => {
                let (hi, lo, rm, rn) = (insn >> 16, insn >> 12, insn >> 8, insn);
                if uses_pc(&[hi, lo, rm, rn]) || hi & 0xf == lo & 0xf {
                    return Some(unimplemented);
                }
                let result = self.r[r(rn)] as u64 * self.r[r(rm)] as u64
                    + self.r[r(hi)] as u64
                    + self.r[r(lo)] as u64;
                self.r[r(lo)] = result as u32;
                self.r[r(hi)] = (result >> 32) as u32;
                return Some(Ok(()));
            }
            _ => {}
        }

        // SWP/SWPB (enabled by SCTLR.SW on ARMv7).
        if insn & 0x0fb0_0ff0 == 0x0100_0090 {
            let (rn, rt, rt2) = (insn >> 16, insn >> 12, insn);
            if uses_pc(&[rn, rt, rt2]) || self.sctlr & (1 << 10) == 0 {
                return Some(unimplemented);
            }
            let address = self.r[r(rn)];
            let store = self.r[r(rt2)];
            let byte = insn & (1 << 22) != 0;
            let result = if byte {
                self.mem_read8(bus, address, crate::mmu::Access::Read).map(u32::from)
            } else {
                self.mem_read32(bus, address, crate::mmu::Access::Read)
            };
            let result = result.and_then(|old| {
                if byte {
                    self.mem_write8(bus, address, store as u8)
                } else {
                    self.mem_write32(bus, address, store)
                }
                .map(|()| old)
            });
            return Some(match result {
                Ok(old) => {
                    self.r[r(rt)] = old;
                    Ok(())
                }
                Err((va, status)) => Err(self.data_abort(va, status, false)),
            });
        }

        // QADD/QSUB/QDADD/QDSUB.
        if insn & 0x0f90_0ff0 == 0x0100_0050 {
            let (rn, rd, rm) = (insn >> 16, insn >> 12, insn);
            if uses_pc(&[rn, rd, rm]) {
                return Some(unimplemented);
            }
            let (a, mut b) = (self.r[r(rm)] as i32 as i64, self.r[r(rn)] as i32 as i64);
            let op = (insn >> 21) & 3;
            if op & 2 != 0 {
                let (doubled, sat) = signed_saturate(2 * b, 32);
                self.set_q(sat);
                b = doubled as i32 as i64;
            }
            let (value, sat) = signed_saturate(if op & 1 == 0 { a + b } else { a - b }, 32);
            self.set_q(sat);
            self.r[r(rd)] = value;
            return Some(Ok(()));
        }

        // Signed 16-bit multiplies: SMLAxy, SMLAWy/SMULWy, SMLALxy, SMULxy.
        if insn & 0x0f90_0090 == 0x0100_0080 {
            let (rd, ra, rm, rn) = (insn >> 16, insn >> 12, insn >> 8, insn);
            let op = (insn >> 21) & 3;
            let n_top = insn & (1 << 5) != 0;
            let m_top = insn & (1 << 6) != 0;
            if uses_pc(&[rd, rm, rn]) || op != 3 && op != 1 && ra & 0xf == 15 {
                return Some(unimplemented);
            }
            let (vn, vm) = (self.r[r(rn)], self.r[r(rm)]);
            let operand_n = if n_top { hi16(vn) } else { lo16(vn) } as i64;
            let operand_m = if m_top { hi16(vm) } else { lo16(vm) } as i64;
            match op {
                0 => {
                    let sum = operand_n * operand_m + self.r[r(ra)] as i32 as i64;
                    self.set_q(sum != sum as i32 as i64);
                    self.r[r(rd)] = sum as u32;
                }
                1 => {
                    let product = (vn as i32 as i64 * operand_m) >> 16;
                    if n_top {
                        // SMULWy
                        self.r[r(rd)] = product as u32;
                    } else {
                        if ra & 0xf == 15 {
                            return Some(unimplemented);
                        }
                        let sum = product + self.r[r(ra)] as i32 as i64;
                        self.set_q(sum != sum as i32 as i64);
                        self.r[r(rd)] = sum as u32;
                    }
                }
                2 => {
                    let (hi, lo) = (rd, ra);
                    let acc = (self.r[r(hi)] as u64) << 32 | self.r[r(lo)] as u64;
                    let result = (acc as i64).wrapping_add(operand_n * operand_m) as u64;
                    self.r[r(lo)] = result as u32;
                    self.r[r(hi)] = (result >> 32) as u32;
                }
                _ => self.r[r(rd)] = (operand_n * operand_m) as u32,
            }
            return Some(Ok(()));
        }

        // Media space: cond 011x xxxx ... xxx1.
        if insn & 0x0e00_0010 != 0x0600_0010 {
            return None;
        }

        // Parallel add/subtract.
        if insn & 0x0f80_0f10 == 0x0600_0f10 {
            let (rn, rd, rm) = (insn >> 16, insn >> 12, insn);
            let prefix = (insn >> 20) & 7;
            let op = (insn >> 5) & 7;
            if uses_pc(&[rn, rd, rm]) || matches!(prefix, 0 | 4) || matches!(op, 5 | 6) {
                return Some(unimplemented);
            }
            let (a, b) = (self.r[r(rn)], self.r[r(rm)]);
            let signed = prefix < 4;
            let kind = prefix & 3; // 1 plain (GE), 2 saturating, 3 halving
            let lane = |x: u32, i: u32, width: u32| -> i64 {
                let v = (x >> (i * width)) & ((1 << width) - 1);
                if signed {
                    ((v << (32 - width)) as i32 >> (32 - width)) as i64
                } else {
                    v as i64
                }
            };
            let (width, lanes): (u32, Vec<(i64, i64, bool)>) = match op {
                0 => (16, vec![(lane(a, 0, 16), lane(b, 0, 16), true), (lane(a, 1, 16), lane(b, 1, 16), true)]),
                1 => (16, vec![(lane(a, 0, 16), lane(b, 1, 16), false), (lane(a, 1, 16), lane(b, 0, 16), true)]),
                2 => (16, vec![(lane(a, 0, 16), lane(b, 1, 16), true), (lane(a, 1, 16), lane(b, 0, 16), false)]),
                3 => (16, vec![(lane(a, 0, 16), lane(b, 0, 16), false), (lane(a, 1, 16), lane(b, 1, 16), false)]),
                4 => (8, (0..4).map(|i| (lane(a, i, 8), lane(b, i, 8), true)).collect()),
                _ => (8, (0..4).map(|i| (lane(a, i, 8), lane(b, i, 8), false)).collect()),
            };
            let mut result = 0u32;
            let mut ge = 0u32;
            let ge_bits = if width == 16 { 3 } else { 1 };
            for (i, (x, y, add)) in lanes.into_iter().enumerate() {
                let sum = if add { x + y } else { x - y };
                let value = match kind {
                    1 => {
                        let set = if signed {
                            sum >= 0
                        } else if add {
                            sum >= 1 << width
                        } else {
                            sum >= 0
                        };
                        if set {
                            ge |= ge_bits << (i as u32 * if width == 16 { 2 } else { 1 });
                        }
                        sum as u32
                    }
                    2 => {
                        if signed {
                            signed_saturate(sum, width).0
                        } else {
                            unsigned_saturate(sum, width).0
                        }
                    }
                    _ => (sum >> 1) as u32,
                };
                result |= (value & ((1 << width) - 1)) << (i as u32 * width);
            }
            if kind == 1 {
                self.set_ge(ge);
            }
            self.r[r(rd)] = result;
            return Some(Ok(()));
        }

        // SEL.
        if insn & 0x0ff0_0ff0 == 0x0680_0fb0 {
            let (rn, rd, rm) = (insn >> 16, insn >> 12, insn);
            if uses_pc(&[rn, rd, rm]) {
                return Some(unimplemented);
            }
            let ge = (self.cpsr >> 16) & 0xf;
            let mut value = 0;
            for i in 0..4 {
                let source = if ge & (1 << i) != 0 { self.r[r(rn)] } else { self.r[r(rm)] };
                value |= source & (0xff << (8 * i));
            }
            self.r[r(rd)] = value;
            return Some(Ok(()));
        }

        // PKHBT/PKHTB.
        if insn & 0x0ff0_0030 == 0x0680_0010 {
            let (rn, rd, rm) = (insn >> 16, insn >> 12, insn);
            if uses_pc(&[rn, rd, rm]) {
                return Some(unimplemented);
            }
            let imm = (insn >> 7) & 0x1f;
            let (vn, vm) = (self.r[r(rn)], self.r[r(rm)]);
            self.r[r(rd)] = if insn & (1 << 6) == 0 {
                vn & 0xffff | (vm << imm) & 0xffff_0000
            } else {
                let shifted = ((vm as i32) >> if imm == 0 { 31 } else { imm }) as u32;
                vn & 0xffff_0000 | shifted & 0xffff
            };
            return Some(Ok(()));
        }

        // SSAT16/USAT16.
        if matches!(insn & 0x0ff0_0ff0, 0x06a0_0f30 | 0x06e0_0f30) {
            let (rd, rn) = (insn >> 12, insn);
            if uses_pc(&[rd, rn]) {
                return Some(unimplemented);
            }
            let unsigned = insn & (1 << 22) != 0;
            let bits = (insn >> 16) & 0xf;
            let value = self.r[r(rn)];
            let mut result = 0;
            let mut any = false;
            for (i, half) in [lo16(value), hi16(value)].into_iter().enumerate() {
                let (v, sat) = if unsigned {
                    unsigned_saturate(half as i64, bits)
                } else {
                    signed_saturate(half as i64, bits + 1)
                };
                any |= sat;
                result |= (v & 0xffff) << (16 * i);
            }
            self.set_q(any);
            self.r[r(rd)] = result;
            return Some(Ok(()));
        }

        // SSAT/USAT.
        if matches!(insn & 0x0fe0_0030, 0x06a0_0010 | 0x06e0_0010) {
            let (rd, rn) = (insn >> 12, insn);
            if uses_pc(&[rd, rn]) {
                return Some(unimplemented);
            }
            let unsigned = insn & (1 << 22) != 0;
            let bits = (insn >> 16) & 0x1f;
            let imm = (insn >> 7) & 0x1f;
            let value = self.r[r(rn)];
            let operand = if insn & (1 << 6) == 0 {
                (value << imm) as i32 as i64
            } else {
                ((value as i32) >> if imm == 0 { 31 } else { imm }) as i64
            };
            let (v, sat) = if unsigned {
                unsigned_saturate(operand, bits)
            } else {
                signed_saturate(operand, bits + 1)
            };
            self.set_q(sat);
            self.r[r(rd)] = v;
            return Some(Ok(()));
        }

        // SXTAB16/SXTB16/UXTAB16/UXTB16.
        if matches!(insn & 0x0ff0_03f0, 0x0680_0070 | 0x06c0_0070) {
            let (rn, rd, rm) = (insn >> 16, insn >> 12, insn);
            if uses_pc(&[rd, rm]) {
                return Some(unimplemented);
            }
            let value = self.r[r(rm)].rotate_right(((insn >> 10) & 3) * 8);
            let signed = insn & (1 << 22) == 0;
            let extend = |byte: u32| if signed { byte as u8 as i8 as i32 as u32 } else { byte & 0xff };
            let (lo, hi) = (extend(value), extend(value >> 16));
            let (base_lo, base_hi) = if rn & 0xf == 15 {
                (0, 0)
            } else {
                (self.r[r(rn)], self.r[r(rn)] >> 16)
            };
            self.r[r(rd)] = base_lo.wrapping_add(lo) & 0xffff | base_hi.wrapping_add(hi) << 16;
            return Some(Ok(()));
        }

        // RBIT/REVSH.
        if matches!(insn & 0x0fff_0ff0, 0x06ff_0f30 | 0x06ff_0fb0) {
            let (rd, rm) = (insn >> 12, insn);
            if uses_pc(&[rd, rm]) {
                return Some(unimplemented);
            }
            let value = self.r[r(rm)];
            self.r[r(rd)] = if insn & 0x80 == 0 {
                value.reverse_bits()
            } else {
                (value as u16).swap_bytes() as i16 as i32 as u32
            };
            return Some(Ok(()));
        }

        // USAD8/USADA8.
        if insn & 0x0ff0_00f0 == 0x0780_0010 {
            let (rd, ra, rm, rn) = (insn >> 16, insn >> 12, insn >> 8, insn);
            if uses_pc(&[rd, rm, rn]) {
                return Some(unimplemented);
            }
            let (a, b) = (self.r[r(rn)], self.r[r(rm)]);
            let mut sum = (0..4)
                .map(|i| ((a >> (8 * i)) & 0xff).abs_diff((b >> (8 * i)) & 0xff))
                .sum::<u32>();
            if ra & 0xf != 15 {
                sum = sum.wrapping_add(self.r[r(ra)]);
            }
            self.r[r(rd)] = sum;
            return Some(Ok(()));
        }

        // Signed dual and most-significant-word multiplies.
        let op1 = (insn >> 20) & 7;
        if insn & 0x0f80_0010 == 0x0700_0010 && matches!(op1, 0 | 4 | 5) {
            let (rd, ra, rm, rn) = (insn >> 16, insn >> 12, insn >> 8, insn);
            let op2 = (insn >> 5) & 7;
            if uses_pc(&[rd, rm, rn]) {
                return Some(unimplemented);
            }
            let (vn, vm) = (self.r[r(rn)], self.r[r(rm)]);
            let swapped = if op2 & 1 != 0 { vm.rotate_right(16) } else { vm };
            let p1 = lo16(vn) as i64 * lo16(swapped) as i64;
            let p2 = hi16(vn) as i64 * hi16(swapped) as i64;
            match (op1, op2 >> 1) {
                (0, 0) | (0, 1) => {
                    // SMLAD/SMUAD, SMLSD/SMUSD
                    let mut sum = if op2 >> 1 == 0 { p1 + p2 } else { p1 - p2 };
                    if ra & 0xf != 15 {
                        sum += self.r[r(ra)] as i32 as i64;
                    }
                    self.set_q(sum != sum as i32 as i64);
                    self.r[r(rd)] = sum as u32;
                }
                (4, 0) | (4, 1) => {
                    // SMLALD/SMLSLD
                    let (hi, lo) = (rd, ra);
                    if lo & 0xf == 15 {
                        return Some(unimplemented);
                    }
                    let acc = ((self.r[r(hi)] as u64) << 32 | self.r[r(lo)] as u64) as i64;
                    let product = if op2 >> 1 == 0 { p1 + p2 } else { p1 - p2 };
                    let result = acc.wrapping_add(product) as u64;
                    self.r[r(lo)] = result as u32;
                    self.r[r(hi)] = (result >> 32) as u32;
                }
                (5, 0) | (5, 3) => {
                    // SMMLA/SMMUL, SMMLS (op2 bit 0 = round)
                    let product = vn as i32 as i64 * vm as i32 as i64;
                    let acc = if ra & 0xf == 15 {
                        if op2 >> 1 == 3 {
                            return Some(unimplemented);
                        }
                        0
                    } else {
                        (self.r[r(ra)] as i32 as i64) << 32
                    };
                    let mut value = if op2 >> 1 == 0 { acc.wrapping_add(product) } else { acc.wrapping_sub(product) };
                    if op2 & 1 != 0 {
                        value = value.wrapping_add(0x8000_0000);
                    }
                    self.r[r(rd)] = (value >> 32) as u32;
                }
                _ => return Some(unimplemented),
            }
            return Some(Ok(()));
        }
        None
    }
}
