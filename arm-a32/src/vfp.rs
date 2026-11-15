//! VFPv4-D32 floating point (Cortex-A7 without Advanced SIMD).
//!
//! Arithmetic uses the host's IEEE operations in round-to-nearest; on an AArch64 host
//! these follow the same NaN propagation rules as the guest. FPSCR rounding modes are
//! honoured for float-to-integer conversions (VCVTR). Exception trapping, short vectors
//! and flush-to-zero are not modelled; cumulative flags are not raised.
use crate::cpu::{Cpu, MODE_USR};
use crate::mmu::Access;
use emu_core::{Bus, Trap};

pub const FPSID: u32 = 0x4102_3075; // Cortex-A7 VFPv4
pub const MVFR0: u32 = 0x1011_0222; // 32 double registers, single/double, divide, sqrt
pub const MVFR1: u32 = 0x1000_0011; // fused multiply-add; no Advanced SIMD
const FPEXC_EN: u32 = 1 << 30;

#[derive(Clone)]
pub struct Vfp {
    pub d: [u64; 32],
    pub fpscr: u32,
    pub fpexc: u32,
    pub fpinst: u32,
    pub fpinst2: u32,
}

impl Default for Vfp {
    fn default() -> Self {
        Self { d: [0; 32], fpscr: 0, fpexc: 0, fpinst: 0, fpinst2: 0 }
    }
}

impl Vfp {
    pub fn s(&self, n: usize) -> u32 {
        (self.d[n / 2] >> (32 * (n % 2))) as u32
    }

    pub fn set_s(&mut self, n: usize, value: u32) {
        let shift = 32 * (n % 2);
        self.d[n / 2] = self.d[n / 2] & !(0xffff_ffffu64 << shift) | (value as u64) << shift;
    }

    fn sf(&self, n: usize) -> f32 {
        f32::from_bits(self.s(n))
    }

    fn df(&self, n: usize) -> f64 {
        f64::from_bits(self.d[n])
    }
}

fn round_by_mode(value: f64, mode: u32) -> f64 {
    match mode {
        0 => value.round_ties_even(),
        1 => value.ceil(),
        2 => value.floor(),
        _ => value.trunc(),
    }
}

fn expand_imm(imm8: u32, double: bool) -> u64 {
    let sign = (imm8 >> 7) as u64 & 1;
    let b6 = (imm8 >> 6) & 1;
    let top2 = (imm8 >> 4) & 3;
    let low4 = (imm8 & 0xf) as u64;
    if double {
        let exp = ((b6 ^ 1) << 10 | if b6 != 0 { 0xff << 2 } else { 0 } | top2) as u64;
        sign << 63 | exp << 52 | low4 << 48
    } else {
        let exp = ((b6 ^ 1) << 7 | if b6 != 0 { 0x1f << 2 } else { 0 } | top2) as u64;
        sign << 31 | exp << 23 | low4 << 19
    }
}

fn compare_flags(a: f64, b: f64) -> u32 {
    if a.is_nan() || b.is_nan() {
        0b0011
    } else if a == b {
        0b0110
    } else if a < b {
        0b1000
    } else {
        0b0010
    }
}

impl Cpu {
    /// CPACR grants access to CP10/CP11 at the current privilege level.
    fn vfp_accessible(&self) -> bool {
        let access = (self.cpacr >> 20) & 0xf;
        let privileged = self.cpsr & 0x1f != MODE_USR;
        let cp10 = access & 3;
        let cp11 = (access >> 2) & 3;
        let ok = |a: u32| a == 3 || a == 1 && privileged;
        ok(cp10) && ok(cp11)
    }

    pub(crate) fn execute_vfp(
        &mut self,
        bus: &mut dyn Bus,
        pc: u64,
        insn: u32,
    ) -> Option<Result<(), Trap>> {
        // Coprocessor 10/11 instructions only.
        if insn & 0x0c00_0e00 != 0x0c00_0a00 || insn & 0x0f00_0000 == 0x0f00_0000 {
            return None;
        }
        let undefined = Some(Err(Trap::Unimplemented(pc, insn)));
        if !self.vfp_accessible() {
            return undefined;
        }
        let enabled = self.vfp.fpexc & FPEXC_EN != 0;

        // VMRS/VMSR: system registers. FPSID, FPEXC and MVFRx stay accessible with
        // FPEXC.EN clear (privileged), which is how Linux probes and enables VFP lazily.
        if insn & 0x0fe0_0fff == 0x0ee0_0a10 {
            let read = insn & (1 << 20) != 0;
            let reg = (insn >> 16) & 0xf;
            let rt = ((insn >> 12) & 0xf) as usize;
            let privileged = self.cpsr & 0x1f != MODE_USR;
            let allowed = match reg {
                1 => enabled,
                0 | 6 | 7 | 8 => privileged,
                9 | 10 => privileged && enabled,
                _ => false,
            };
            if !allowed {
                return undefined;
            }
            if read {
                let value = match reg {
                    0 => FPSID,
                    1 => self.vfp.fpscr,
                    6 => MVFR1,
                    7 => MVFR0,
                    8 => self.vfp.fpexc,
                    9 => self.vfp.fpinst,
                    10 => self.vfp.fpinst2,
                    _ => return undefined,
                };
                if rt == 15 {
                    if reg != 1 {
                        return undefined;
                    }
                    self.cpsr = (self.cpsr & 0x0fff_ffff) | (value & 0xf000_0000);
                } else {
                    self.r[rt] = value;
                }
            } else {
                if rt == 15 {
                    return undefined;
                }
                let value = self.r[rt];
                match reg {
                    1 => self.vfp.fpscr = value & 0xfff7_9f9f,
                    8 => self.vfp.fpexc = value & (FPEXC_EN | 1 << 31),
                    9 => self.vfp.fpinst = value,
                    10 => self.vfp.fpinst2 = value,
                    0 | 6 | 7 => {}
                    _ => return undefined,
                }
            }
            return Some(Ok(()));
        }
        if !enabled {
            return undefined;
        }

        let double = insn & (1 << 8) != 0;
        let d_bit = (insn >> 22) & 1;
        let vd = (insn >> 12) & 0xf;
        let vn = (insn >> 16) & 0xf;
        let vm = insn & 0xf;
        let n_bit = (insn >> 7) & 1;
        let m_bit = (insn >> 5) & 1;
        let sreg = |v: u32, b: u32| (v << 1 | b) as usize;
        let dreg = |v: u32, b: u32| (b << 4 | v) as usize;

        // VMOV between a core register and a single-precision register.
        if insn & 0x0fe0_0f7f == 0x0e00_0a10 {
            let rt = ((insn >> 12) & 0xf) as usize;
            if rt == 15 {
                return undefined;
            }
            let s = sreg(vn, n_bit);
            if insn & (1 << 20) != 0 {
                self.r[rt] = self.vfp.s(s);
            } else {
                self.vfp.set_s(s, self.r[rt]);
            }
            return Some(Ok(()));
        }

        // VMOV between a core register and half of a double register (32-bit scalar).
        if insn & 0x0f90_0f7f == 0x0e00_0b10 || insn & 0x0f90_0f7f == 0x0e10_0b10 {
            let to_core = insn & (1 << 20) != 0;
            if insn & (1 << 22) != 0 || insn & 0x60 != 0 {
                return undefined; // 8/16-bit scalars are Advanced SIMD
            }
            let rt = ((insn >> 12) & 0xf) as usize;
            if rt == 15 {
                return undefined;
            }
            let reg = dreg(vn, n_bit);
            let half = (insn >> 21) & 1;
            let s = reg * 2 + half as usize;
            if to_core {
                self.r[rt] = self.vfp.s(s);
            } else {
                self.vfp.set_s(s, self.r[rt]);
            }
            return Some(Ok(()));
        }

        // VMOV between two core registers and two singles / one double.
        if insn & 0x0fe0_0ed0 == 0x0c40_0a10 {
            let to_core = insn & (1 << 20) != 0;
            let (rt, rt2) = (((insn >> 12) & 0xf) as usize, ((insn >> 16) & 0xf) as usize);
            if rt == 15 || rt2 == 15 || to_core && rt == rt2 {
                return undefined;
            }
            if double {
                let reg = dreg(vm, m_bit);
                if to_core {
                    self.r[rt] = self.vfp.d[reg] as u32;
                    self.r[rt2] = (self.vfp.d[reg] >> 32) as u32;
                } else {
                    self.vfp.d[reg] = self.r[rt] as u64 | (self.r[rt2] as u64) << 32;
                }
            } else {
                let s = sreg(vm, m_bit);
                if s == 31 {
                    return undefined;
                }
                if to_core {
                    self.r[rt] = self.vfp.s(s);
                    self.r[rt2] = self.vfp.s(s + 1);
                } else {
                    self.vfp.set_s(s, self.r[rt]);
                    self.vfp.set_s(s + 1, self.r[rt2]);
                }
            }
            return Some(Ok(()));
        }

        // Loads and stores: VLDR/VSTR and VLDM/VSTM (including FLDMX/FSTMX).
        if insn & 0x0e00_0000 == 0x0c00_0000 {
            return Some(self.vfp_transfer(bus, insn, double, sreg(vd, d_bit), dreg(vd, d_bit)));
        }

        // Data processing.
        if insn & 0x0f00_0010 != 0x0e00_0000 {
            return undefined;
        }
        let opc1 = (insn >> 20) & 0xb; // bits 23, 21, 20
        let op = (insn >> 6) & 1;
        let (d, n, m) = if double {
            (dreg(vd, d_bit), dreg(vn, n_bit), dreg(vm, m_bit))
        } else {
            (sreg(vd, d_bit), sreg(vn, n_bit), sreg(vm, m_bit))
        };
        macro_rules! binary {
            (|$a:ident, $b:ident, $acc:ident| $e:expr) => {{
                if double {
                    let ($a, $b, $acc): (f64, f64, f64) =
                        (self.vfp.df(n), self.vfp.df(m), self.vfp.df(d));
                    let _ = (&$a, &$b, &$acc);
                    self.vfp.d[d] = ($e).to_bits();
                } else {
                    let ($a, $b, $acc): (f32, f32, f32) =
                        (self.vfp.sf(n), self.vfp.sf(m), self.vfp.sf(d));
                    let _ = (&$a, &$b, &$acc);
                    self.vfp.set_s(d, ($e).to_bits());
                }
            }};
        }
        match (opc1, op) {
            (0x0, 0) => binary!(|a, b, acc| acc + a * b),     // VMLA
            (0x0, 1) => binary!(|a, b, acc| acc - a * b),     // VMLS
            (0x1, 0) => binary!(|a, b, acc| -acc + a * b),    // VNMLS
            (0x1, _) => binary!(|a, b, acc| -acc - a * b),    // VNMLA
            (0x2, 0) => binary!(|a, b, acc| a * b),          // VMUL
            (0x2, _) => binary!(|a, b, acc| -(a * b)),       // VNMUL
            (0x3, 0) => binary!(|a, b, acc| a + b),          // VADD
            (0x3, _) => binary!(|a, b, acc| a - b),          // VSUB
            (0x8, 0) => binary!(|a, b, acc| a / b),          // VDIV
            (0xa, 0) => {
                // VFMA
                if double {
                    let r = self.vfp.df(n).mul_add(self.vfp.df(m), self.vfp.df(d));
                    self.vfp.d[d] = r.to_bits();
                } else {
                    let r = self.vfp.sf(n).mul_add(self.vfp.sf(m), self.vfp.sf(d));
                    self.vfp.set_s(d, r.to_bits());
                }
            }
            (0xa, _) => {
                // VFMS
                if double {
                    let r = (-self.vfp.df(n)).mul_add(self.vfp.df(m), self.vfp.df(d));
                    self.vfp.d[d] = r.to_bits();
                } else {
                    let r = (-self.vfp.sf(n)).mul_add(self.vfp.sf(m), self.vfp.sf(d));
                    self.vfp.set_s(d, r.to_bits());
                }
            }
            (0x9, _) => {
                // VFNMA (op 1): -d - n*m ; VFNMS (op 0): -d + n*m
                let negate_product = op == 1;
                if double {
                    let a = if negate_product { -self.vfp.df(n) } else { self.vfp.df(n) };
                    let r = a.mul_add(self.vfp.df(m), -self.vfp.df(d));
                    self.vfp.d[d] = r.to_bits();
                } else {
                    let a = if negate_product { -self.vfp.sf(n) } else { self.vfp.sf(n) };
                    let r = a.mul_add(self.vfp.sf(m), -self.vfp.sf(d));
                    self.vfp.set_s(d, r.to_bits());
                }
            }
            (0xb, _) => return Some(self.vfp_other(pc, insn, double, d, m, vd, d_bit, vm, m_bit)),
            _ => return undefined,
        }
        Some(Ok(()))
    }

    #[allow(clippy::too_many_arguments)]
    fn vfp_other(
        &mut self,
        pc: u64,
        insn: u32,
        double: bool,
        d: usize,
        m: usize,
        vd: u32,
        d_bit: u32,
        vm: u32,
        m_bit: u32,
    ) -> Result<(), Trap> {
        let undefined = Err(Trap::Unimplemented(pc, insn));
        let opc2 = (insn >> 16) & 0xf;
        let opc3 = (insn >> 6) & 3;
        if opc3 & 1 == 0 {
            // VMOV immediate.
            let imm8 = ((insn >> 16) & 0xf) << 4 | insn & 0xf;
            let value = expand_imm(imm8, double);
            if double {
                self.vfp.d[d] = value;
            } else {
                self.vfp.set_s(d, value as u32);
            }
            return Ok(());
        }
        let sreg = |v: u32, b: u32| (v << 1 | b) as usize;
        match (opc2, opc3) {
            (0, 1) | (0, 3) | (1, 1) | (1, 3) => {
                // VMOV register, VABS, VNEG, VSQRT
                if double {
                    let a = self.vfp.d[m];
                    self.vfp.d[d] = match (opc2, opc3) {
                        (0, 1) => a,
                        (0, _) => a & !(1 << 63),
                        (1, 1) => a ^ 1 << 63,
                        _ => f64::from_bits(a).sqrt().to_bits(),
                    };
                } else {
                    let a = self.vfp.s(m);
                    let r = match (opc2, opc3) {
                        (0, 1) => a,
                        (0, _) => a & !(1 << 31),
                        (1, 1) => a ^ 1 << 31,
                        _ => f32::from_bits(a).sqrt().to_bits(),
                    };
                    self.vfp.set_s(d, r);
                }
            }
            (4, _) | (5, _) => {
                // VCMP/VCMPE against a register or +0.0
                let (a, b) = if double {
                    (self.vfp.df(d), if opc2 == 5 { 0.0 } else { self.vfp.df(m) })
                } else {
                    (
                        self.vfp.sf(d) as f64,
                        if opc2 == 5 { 0.0 } else { self.vfp.sf(m) as f64 },
                    )
                };
                self.vfp.fpscr = (self.vfp.fpscr & 0x0fff_ffff) | compare_flags(a, b) << 28;
            }
            (7, 3) => {
                // VCVT between double and single precision.
                if double {
                    let source = self.vfp.df(m);
                    self.vfp.set_s(sreg(vd, d_bit), (source as f32).to_bits());
                } else {
                    let source = self.vfp.sf(sreg(vm, m_bit));
                    self.vfp.d[(d_bit << 4 | vd) as usize] = (source as f64).to_bits();
                }
            }
            (8, _) => {
                // VCVT integer (in a single register) to floating point.
                let raw = self.vfp.s(sreg(vm, m_bit));
                let value = if insn & (1 << 7) != 0 { raw as i32 as f64 } else { raw as f64 };
                if double {
                    self.vfp.d[d] = value.to_bits();
                } else {
                    self.vfp.set_s(d, (value as f32).to_bits());
                }
            }
            (12, _) | (13, _) => {
                // VCVT/VCVTR floating point to integer (result in a single register).
                let source = if double { self.vfp.df(m) } else { self.vfp.sf(m) as f64 };
                let mode = if insn & (1 << 7) != 0 { 3 } else { (self.vfp.fpscr >> 22) & 3 };
                let rounded = round_by_mode(source, mode);
                let value = if opc2 & 1 != 0 {
                    rounded as i32 as u32
                } else {
                    rounded as u32
                };
                self.vfp.set_s(sreg(vd, d_bit), value);
            }
            (10, _) | (11, _) | (14, _) | (15, _) => {
                // VCVT between floating point and fixed point, in place.
                let to_fixed = opc2 & 4 != 0;
                let unsigned = opc2 & 1 != 0;
                let size = if insn & (1 << 7) != 0 { 32 } else { 16 };
                let imm = ((insn & 0xf) << 1 | (insn >> 5) & 1) as i32;
                let fraction_bits = size - imm;
                if fraction_bits < 0 {
                    return undefined;
                }
                let scale = (2.0f64).powi(fraction_bits);
                let raw = if double { self.vfp.d[d] } else { self.vfp.s(d) as u64 };
                if to_fixed {
                    let source = if double { f64::from_bits(raw) } else { f32::from_bits(raw as u32) as f64 };
                    let scaled = (source * scale).trunc();
                    let bits = match (unsigned, size) {
                        (true, 32) => scaled as u32 as u64,
                        (false, 32) => scaled as i32 as u32 as u64,
                        (true, _) => (scaled.clamp(0.0, 65535.0) as u32) as u64,
                        (false, _) => (scaled.clamp(-32768.0, 32767.0) as i32 as u32 & 0xffff) as i16 as i32 as u32 as u64,
                    };
                    if double {
                        self.vfp.d[d] = bits | if unsigned || bits & 0x8000_0000 == 0 { 0 } else { 0xffff_ffff << 32 };
                    } else {
                        self.vfp.set_s(d, bits as u32);
                    }
                } else {
                    let integer = match (unsigned, size) {
                        (true, 32) => raw as u32 as f64,
                        (false, 32) => raw as u32 as i32 as f64,
                        (true, _) => (raw as u16) as f64,
                        (false, _) => (raw as u16 as i16) as f64,
                    };
                    let value = integer / scale;
                    if double {
                        self.vfp.d[d] = value.to_bits();
                    } else {
                        self.vfp.set_s(d, (value as f32).to_bits());
                    }
                }
            }
            _ => return undefined, // half-precision conversions are not advertised
        }
        Ok(())
    }

    fn vfp_transfer(
        &mut self,
        bus: &mut dyn Bus,
        insn: u32,
        double: bool,
        sd: usize,
        dd: usize,
    ) -> Result<(), Trap> {
        let (pre, up, writeback, load) = (
            insn & (1 << 24) != 0,
            insn & (1 << 23) != 0,
            insn & (1 << 21) != 0,
            insn & (1 << 20) != 0,
        );
        let rn = ((insn >> 16) & 0xf) as usize;
        let imm8 = insn & 0xff;
        let undefined = Err(Trap::Unimplemented(self.r[15] as u64 - 4, insn));
        let base = if rn == 15 { self.r[15].wrapping_add(4) & !3 } else { self.r[rn] };

        // VLDR/VSTR
        if pre && !writeback {
            let address = if up { base.wrapping_add(imm8 * 4) } else { base.wrapping_sub(imm8 * 4) };
            let words = if double { 2 } else { 1 };
            for word in 0..words {
                let va = address.wrapping_add(word * 4);
                let index = if double { dd * 2 + word as usize } else { sd };
                if load {
                    match self.mem_read32(bus, va, Access::Read) {
                        Ok(value) => self.vfp.set_s(index, value),
                        Err((va, status)) => return Err(self.data_abort(va, status, false)),
                    }
                } else if let Err((va, status)) = self.mem_write32(bus, va, self.vfp.s(index)) {
                    return Err(self.data_abort(va, status, true));
                }
            }
            return Ok(());
        }

        // VLDM/VSTM: IA (P=0,U=1) with optional writeback, DB (P=1,U=0) with writeback.
        if pre == up || rn == 15 && writeback {
            return undefined;
        }
        let words = imm8;
        let (first, count) = if double {
            (dd * 2, (words & !1) as usize) // FLDMX/FSTMX transfer one extra unused word
        } else {
            (sd, words as usize)
        };
        if words == 0 || first + count > 64 {
            return undefined;
        }
        let start = if up { base } else { base.wrapping_sub(words * 4) };
        for index in 0..count {
            let va = start.wrapping_add(index as u32 * 4);
            if load {
                match self.mem_read32(bus, va, Access::Read) {
                    Ok(value) => self.vfp.set_s(first + index, value),
                    Err((va, status)) => return Err(self.data_abort(va, status, false)),
                }
            } else if let Err((va, status)) = self.mem_write32(bus, va, self.vfp.s(first + index)) {
                return Err(self.data_abort(va, status, true));
            }
        }
        if writeback {
            self.r[rn] = if up { base.wrapping_add(words * 4) } else { base.wrapping_sub(words * 4) };
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn immediate_expansion_matches_the_architecture() {
        assert_eq!(f32::from_bits(expand_imm(0x70, false) as u32), 1.0);
        assert_eq!(f64::from_bits(expand_imm(0x70, true)), 1.0);
        assert_eq!(f32::from_bits(expand_imm(0x00, false) as u32), 2.0);
        assert_eq!(f64::from_bits(expand_imm(0xf0, true)), -1.0);
        assert_eq!(f32::from_bits(expand_imm(0x60, false) as u32), 0.5);
    }

    #[test]
    fn single_registers_alias_double_halves() {
        let mut vfp = Vfp::default();
        vfp.set_s(2, 0x1111_1111);
        vfp.set_s(3, 0x2222_2222);
        assert_eq!(vfp.d[1], 0x2222_2222_1111_1111);
    }
}
