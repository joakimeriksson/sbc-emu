//! Advanced SIMD (NEON) integer and floating-point vector instructions, and their scalar forms.
//! Organised by the Arm ARM encoding groups. Saturation sets FPSR.QC. Floating point goes through
//! `fpu` (FPCR rounding, FZ and DN; FPSR exception flags).
use crate::cpu::Cpu;
use crate::fpu::{Pred, Rnd};
use emu_core::Trap;

#[inline] fn bits(x: u32, hi: u32, lo: u32) -> u32 { (x >> lo) & ((1u32 << (hi - lo + 1)) - 1) }
#[inline] fn bit(x: u32, b: u32) -> bool { (x >> b) & 1 != 0 }
#[inline] fn emask(es: u32) -> u64 { if es >= 64 { u64::MAX } else { (1u64 << es) - 1 } }
#[inline] fn el(v: u128, es: u32, i: u32) -> u64 { ((v >> (es * i)) as u64) & emask(es) }
#[inline] fn sx(x: u64, es: u32) -> i64 { let s = 64 - es; ((x << s) as i64) >> s }
#[inline] fn sel(v: u128, es: u32, i: u32) -> i64 { sx(el(v, es, i), es) }
#[inline] fn set_el(v: &mut u128, es: u32, i: u32, x: u64) {
    let m = (emask(es) as u128) << (es * i);
    *v = (*v & !m) | (((x & emask(es)) as u128) << (es * i));
}
#[inline] fn qmask(q: bool, v: u128) -> u128 { if q { v } else { v & u64::MAX as u128 } }

/// Saturate a wide value to an `es`-bit signed/unsigned element; returns (value, saturated).
fn sat(x: i128, es: u32, signed: bool) -> (u64, bool) {
    let (lo, hi) = if signed { (-(1i128 << (es - 1)), (1i128 << (es - 1)) - 1) } else { (0, (1i128 << es) - 1) };
    if x < lo { ((lo as u64) & emask(es), true) } else if x > hi { ((hi as u64) & emask(es), true) } else { ((x as u64) & emask(es), false) }
}

fn shl_signed(a: u64, es: u32, sh: i64, unsigned: bool, round: bool) -> i128 {
    // shift by a signed amount (negative = right), with optional rounding, in wide arithmetic
    let v: i128 = if unsigned { a as i128 } else { sx(a, es) as i128 };
    if sh >= 0 {
        if sh >= es as i64 { if v == 0 { 0 } else if v > 0 { i128::MAX >> 1 } else { i128::MIN >> 1 } } else { v << sh }
    } else {
        let r = (-sh) as u32;
        if r > 127 { return if round { 0 } else if v < 0 { -1 } else { 0 }; }
        let rnd = if round && r > 0 { 1i128 << (r - 1) } else { 0 };
        (v + rnd) >> r
    }
}

impl Cpu {
    fn qc(&mut self, saturated: bool) { if saturated { self.fpsr |= 1 << 27; } }

    // ================================================================== dispatch
    pub(crate) fn neon(&mut self, i: u32) -> Result<Option<u64>, Trap> {
        let scalar = bit(i, 30) && bits(i, 28, 24) == 0b11110;
        let r = if bits(i, 28, 24) == 0b01110 || scalar {
            if bit(i, 21) && bit(i, 10) { self.three_same(i, scalar) }
            else if bit(i, 21) && bits(i, 20, 17) == 0b0000 && bits(i, 11, 10) == 0b10 { self.two_misc(i, scalar) }
            else if bit(i, 21) && bits(i, 20, 17) == 0b1000 && bits(i, 11, 10) == 0b10 { if scalar { self.scalar_pairwise(i) } else { self.across(i) } }
            else if bit(i, 21) && bits(i, 11, 10) == 0b00 && !scalar { self.three_diff(i) }
            else if !scalar && bits(i, 29, 21) == 0b001110000 && bit(i, 10) && !bit(i, 15) { self.copy(i) }
            else if !scalar && bits(i, 29, 21) == 0b101110000 && !bit(i, 15) && bit(i, 10) { self.ins_elem(i) }
            else if scalar && bits(i, 29, 21) == 0b011110000 && bits(i, 15, 10) == 0b000001 { self.scalar_dup(i) }
            else if !scalar && !bit(i, 21) && !bit(i, 15) && bits(i, 11, 10) == 0b10 && !bit(i, 29) { self.permute(i) }
            else if !scalar && bits(i, 29, 21) & 0b111111001 == 0b101110000 && !bit(i, 10) && !bit(i, 15) { self.ext(i) }
            else if !scalar && !bit(i, 29) && bits(i, 23, 21) == 0 && bits(i, 11, 10) == 0 && !bit(i, 15) { self.tbl(i) }
            else { None }
        } else if bits(i, 28, 24) == 0b01111 || (bit(i, 30) && bits(i, 28, 24) == 0b11111) {
            let scalar = bits(i, 28, 24) == 0b11111;
            if bit(i, 10) && bits(i, 22, 19) != 0 { self.shift_imm(i, scalar) }
            else if !bit(i, 10) { self.by_element(i, scalar) }
            else { None }
        } else { None };
        match r {
            Some(Ok(())) => Ok(None),
            Some(Err(t)) => Err(t),
            None => Err(Trap::Unimplemented(self.pc, i)),
        }
    }

    // ================================================================== three same
    fn three_same(&mut self, i: u32, scalar: bool) -> Option<Result<(), Trap>> {
        let q = bit(i, 30) || scalar;
        let u = bit(i, 29);
        let size = bits(i, 23, 22);
        let opc = bits(i, 15, 11);
        let (rd, rn, rm) = (bits(i, 4, 0) as usize, bits(i, 9, 5) as usize, bits(i, 20, 16) as usize);
        let (n, m, d) = (self.v[rn], self.v[rm], self.v[rd]);
        if opc >= 0b11000 {
            return Some(self.three_same_fp(i, scalar));
        }
        if opc == 0b00011 {
            if scalar { return None; }
            let r = match (u, size) {
                (false, 0) => n & m, (false, 1) => n & !m, (false, 2) => n | m, (false, _) => n | !m,
                (true, 0) => n ^ m, (true, 1) => (d & n) | (!d & m), (true, 2) => (d & !m) | (n & m), (true, _) => (d & m) | (n & !m),
            };
            self.v[rd] = qmask(q, r);
            return Some(Ok(()));
        }
        let es = 8u32 << size;
        let lanes = if scalar { 1 } else if q { 128 / es } else { 64 / es };
        let mut out = 0u128;
        let mut satd = false;
        for k in 0..lanes {
            let (a, b) = (el(n, es, k), el(m, es, k));
            let (sa, sb) = (sx(a, es) as i128, sx(b, es) as i128);
            let (ua, ub) = (a as i128, b as i128);
            let (wa, wb) = if u { (ua, ub) } else { (sa, sb) };
            let ones = emask(es);
            let pair = |k: u32| -> (u64, u64) {
                let half = lanes / 2;
                let (src, j) = if k < half { (n, 2 * k) } else { (m, 2 * (k - half)) };
                (el(src, es, j), el(src, es, j + 1))
            };
            let v: u64 = match opc {
                0b00000 => ((wa + wb) >> 1) as u64,
                0b00001 => { let (r, s) = sat(wa + wb, es, !u); satd |= s; r }
                0b00010 => ((wa + wb + 1) >> 1) as u64,
                0b00100 => ((wa - wb) >> 1) as u64,
                0b00101 => { let (r, s) = sat(wa - wb, es, !u); satd |= s; r }
                0b00110 => if wa > wb { ones } else { 0 },
                0b00111 => if wa >= wb { ones } else { 0 },
                0b01000 | 0b01010 => {
                    let sh = sx(b & 0xff, 8);
                    if sh >= es as i64 { 0 } else { (shl_signed(a, es, sh, u, opc == 0b01010) as u64) & ones }
                }
                0b01001 | 0b01011 => {
                    let sh = sx(b & 0xff, 8);
                    let (r, s) = sat(shl_signed(a, es, sh, u, opc == 0b01011), es, !u);
                    satd |= s;
                    r
                }
                0b01100 => if wa >= wb { a } else { b },
                0b01101 => if wa <= wb { a } else { b },
                0b01110 => (wa - wb).unsigned_abs() as u64,
                0b01111 => el(d, es, k).wrapping_add((wa - wb).unsigned_abs() as u64),
                0b10000 => if u { a.wrapping_sub(b) } else { a.wrapping_add(b) },
                0b10001 => if u { if a == b { ones } else { 0 } } else if a & b != 0 { ones } else { 0 },
                0b10010 => { let p = a.wrapping_mul(b); if u { el(d, es, k).wrapping_sub(p) } else { el(d, es, k).wrapping_add(p) } }
                0b10011 => if u {
                    // PMUL (bytes): carry-less multiply
                    let mut r = 0u64;
                    for bb in 0..8 { if b & (1 << bb) != 0 { r ^= a << bb; } }
                    r
                } else { a.wrapping_mul(b) },
                0b10100 | 0b10101 => {
                    if scalar { return None; }
                    let (x, y) = pair(k);
                    let (wx, wy) = if u { (x as i128, y as i128) } else { (sx(x, es) as i128, sx(y, es) as i128) };
                    if (opc == 0b10100) == (wx >= wy) { x } else { y }
                }
                0b10110 => {
                    // SQDMULH / SQRDMULH
                    let p = 2 * sa * sb + if u { 1i128 << (es - 1) } else { 0 };
                    let (r, s) = sat(p >> es, es, true);
                    satd |= s;
                    r
                }
                0b10111 if !u => {
                    if scalar { return None; }
                    let (x, y) = pair(k);
                    x.wrapping_add(y)
                }
                _ => return None,
            };
            set_el(&mut out, es, k, v);
        }
        self.qc(satd);
        self.v[rd] = if scalar { out & emask(es) as u128 } else { qmask(q, out) };
        Some(Ok(()))
    }

    fn three_same_fp(&mut self, i: u32, scalar: bool) -> Result<(), Trap> {
        let q = bit(i, 30) || scalar;
        let u = bit(i, 29);
        let a_bit = bit(i, 23);
        let es = if bit(i, 22) { 64 } else { 32 };
        let opc = bits(i, 15, 11);
        let (rd, rn, rm) = (bits(i, 4, 0) as usize, bits(i, 9, 5) as usize, bits(i, 20, 16) as usize);
        let (n, m, d) = (self.v[rn], self.v[rm], self.v[rd]);
        let lanes = if scalar { 1 } else if q { 128 / es } else { 64 / es };
        if !scalar && !q && es == 64 { return Err(self.undefined()); }
        let mut out = 0u128;
        let mut fp = self.fpu();
        for k in 0..lanes {
            let (a, b) = (el(n, es, k), el(m, es, k));
            let pair = |k: u32| -> (u64, u64) {
                let half = lanes / 2;
                let (src, j) = if k < half { (n, 2 * k) } else { (m, 2 * (k - half)) };
                (el(src, es, j), el(src, es, j + 1))
            };
            let all = emask(es);
            let cmp = |t: bool| if t { all } else { 0 };
            let sign = 1u64 << (es - 1);
            let v = match (u, opc, a_bit) {
                (false, 0b11000, false) => fp.maxnm(es, a, b),
                (false, 0b11000, true) => fp.minnm(es, a, b),
                (false, 0b11001, _) => fp.fma(es, el(d, es, k), if a_bit { a ^ sign } else { a }, b),
                (false, 0b11010, false) => fp.add(es, a, b),
                (false, 0b11010, true) => fp.sub(es, a, b),
                (false, 0b11011, false) => fp.mulx(es, a, b),
                (false, 0b11100, false) => cmp(fp.cmp_pred(es, a, b, Pred::Eq)),
                (false, 0b11110, false) => fp.max(es, a, b),
                (false, 0b11110, true) => fp.min(es, a, b),
                (false, 0b11111, false) => fp.recip_step(es, a, b, false), // FRECPS
                (false, 0b11111, true) => fp.recip_step(es, a, b, true),   // FRSQRTS
                (true, 0b11000, false) if !scalar => { let (x, y) = pair(k); fp.maxnm(es, x, y) }
                (true, 0b11000, true) if !scalar => { let (x, y) = pair(k); fp.minnm(es, x, y) }
                (true, 0b11010, false) if !scalar => { let (x, y) = pair(k); fp.add(es, x, y) }
                (true, 0b11010, true) => fp.sub(es, a, b) & !sign, // FABD = |a - b|, NaNs too
                (true, 0b11011, false) => fp.mul(es, a, b),
                (true, 0b11100, false) => cmp(fp.cmp_pred(es, a, b, Pred::Ge)),
                (true, 0b11100, true) => cmp(fp.cmp_pred(es, a, b, Pred::Gt)),
                (true, 0b11101, false) => cmp(fp.cmp_pred(es, a & !sign, b & !sign, Pred::Ge)),
                (true, 0b11101, true) => cmp(fp.cmp_pred(es, a & !sign, b & !sign, Pred::Gt)),
                (true, 0b11110, false) if !scalar => { let (x, y) = pair(k); fp.max(es, x, y) }
                (true, 0b11110, true) if !scalar => { let (x, y) = pair(k); fp.min(es, x, y) }
                (true, 0b11111, false) => fp.div(es, a, b),
                _ => return Err(Trap::Unimplemented(self.pc, i)),
            };
            set_el(&mut out, es, k, v);
        }
        self.fpu_done(fp);
        self.v[rd] = if scalar { out & emask(es) as u128 } else { qmask(q, out) };
        Ok(())
    }

    // ================================================================== three different
    fn three_diff(&mut self, i: u32) -> Option<Result<(), Trap>> {
        let q = bit(i, 30);
        let u = bit(i, 29);
        let es = 8u32 << bits(i, 23, 22);
        if es == 64 { return None; }
        let opc = bits(i, 15, 12);
        let (rd, rn, rm) = (bits(i, 4, 0) as usize, bits(i, 9, 5) as usize, bits(i, 20, 16) as usize);
        let (n, m, d) = (self.v[rn], self.v[rm], self.v[rd]);
        let half = 64 / es;
        let off = if q { half } else { 0 };
        let we = es * 2;
        let ext = |v: u128, k: u32| -> i128 { if u { el(v, es, k) as i128 } else { sel(v, es, k) as i128 } };
        let wide = |v: u128, k: u32| -> i128 { if u { el(v, we, k) as i128 } else { sel(v, we, k) as i128 } };
        let mut out = if matches!(opc, 0b0100 | 0b0110) { if q { d & u64::MAX as u128 } else { 0 } } else { 0 };
        let mut satd = false;
        for k in 0..half {
            match opc {
                0b0100 | 0b0110 => {
                    // ADDHN/RADDHN/SUBHN/RSUBHN: wide inputs, high half of the result, narrowed
                    let (a, b) = (el(n, we, k) as i128, el(m, we, k) as i128);
                    let r = if opc == 0b0100 { a + b } else { a - b } + if u { 1i128 << (es - 1) } else { 0 };
                    set_el(&mut out, es, k + off, ((r >> es) as u64) & emask(es));
                }
                _ => {
                    let (a, b) = (ext(n, k + off), ext(m, k + off));
                    let acc = wide(d, k);
                    let v: i128 = match opc {
                        0b0000 => a + b,
                        0b0001 => wide(n, k) + b,
                        0b0010 => a - b,
                        0b0011 => wide(n, k) - b,
                        0b0101 => acc + (a - b).abs(),
                        0b0111 => (a - b).abs(),
                        0b1000 => acc + a * b,
                        0b1010 => acc - a * b,
                        0b1100 => a * b,
                        0b1001 | 0b1011 | 0b1101 if !u => {
                            // SQDMLAL / SQDMLSL / SQDMULL
                            let (p, s1) = sat(2 * a * b, we, true);
                            satd |= s1;
                            let p = sx(p, we) as i128;
                            let r = match opc { 0b1001 => acc + p, 0b1011 => acc - p, _ => p };
                            let (r, s2) = sat(r, we, true);
                            satd |= s2;
                            sx(r, we) as i128
                        }
                        0b1110 if !u && es == 8 => {
                            // PMULL (8-bit polynomial)
                            let (x, y) = (el(n, 8, k + off), el(m, 8, k + off));
                            let mut r = 0u64;
                            for bb in 0..8 { if y & (1 << bb) != 0 { r ^= x << bb; } }
                            r as i128
                        }
                        _ => return None,
                    };
                    set_el(&mut out, we, k, (v as u64) & emask(we));
                }
            }
        }
        self.qc(satd);
        self.v[rd] = out;
        Some(Ok(()))
    }

    // ================================================================== two-register misc
    fn two_misc(&mut self, i: u32, scalar: bool) -> Option<Result<(), Trap>> {
        let q = bit(i, 30) || scalar;
        let u = bit(i, 29);
        let size = bits(i, 23, 22);
        let opc = bits(i, 16, 12);
        let (rd, rn) = (bits(i, 4, 0) as usize, bits(i, 9, 5) as usize);
        let n = self.v[rn];
        let d = self.v[rd];
        let es = 8u32 << size;
        let lanes = if scalar { 1 } else if q { 128 / es } else { 64 / es };
        // floating point forms (opcode with bit 4 set or the FP compare/abs block)
        let fp = (opc >= 0b11000 && opc != 0b11100) || (size >= 2 && matches!(opc, 0b01100..=0b01111)) || matches!(opc, 0b10110 | 0b10111 | 0b11100 | 0b11101);
        if fp {
            return Some(self.two_misc_fp(i, scalar));
        }
        let mut out = 0u128;
        let mut satd = false;
        let ones = emask(es);
        match opc {
            0b00000 | 0b00001 if !scalar => {
                let container = match (u, opc) { (false, 0) => 64, (false, _) => 16, _ => 32 };
                if es >= container { return Some(Err(self.undefined())); }
                let per = container / es;
                for k in 0..(128 / es) {
                    let base = k / per * per;
                    set_el(&mut out, es, k, el(n, es, base + (per - 1 - (k - base))));
                }
            }
            0b00010 | 0b00110 if !scalar => {
                // SADDLP/UADDLP (+ SADALP/UADALP accumulate)
                let we = es * 2;
                for k in 0..lanes / 2 {
                    let (a, b) = if u { (el(n, es, 2 * k) as i128, el(n, es, 2 * k + 1) as i128) } else { (sel(n, es, 2 * k) as i128, sel(n, es, 2 * k + 1) as i128) };
                    let acc = if opc == 0b00110 { el(d, we, k) as i128 } else { 0 };
                    set_el(&mut out, we, k, ((acc + a + b) as u64) & emask(we));
                }
            }
            0b00100 => for k in 0..lanes {
                let a = el(n, es, k);
                let v = if u { (a << (64 - es)).leading_zeros().min(es) as u64 } else {
                    let s = sx(a, es);
                    ((s ^ (s >> 63)) as u64).leading_zeros().saturating_sub(64 - es + 1).min(es - 1) as u64
                };
                set_el(&mut out, es, k, v);
            },
            0b00101 if !scalar => match (u, size) {
                (false, 0) => for k in 0..16 { set_el(&mut out, 8, k, el(n, 8, k).count_ones() as u64); },
                (true, 0) => out = !n,
                (true, 1) => for k in 0..16 { set_el(&mut out, 8, k, (el(n, 8, k) as u8).reverse_bits() as u64); },
                _ => return None,
            },
            0b00111 => for k in 0..lanes {
                let a = sel(n, es, k) as i128;
                let (v, s) = sat(if u { -a } else { a.abs() }, es, true);
                satd |= s;
                set_el(&mut out, es, k, v);
            },
            0b01000 | 0b01001 | 0b01010 => for k in 0..lanes {
                let s = sel(n, es, k);
                let t = match (u, opc) { (false, 0b01000) => s > 0, (true, 0b01000) => s >= 0, (false, 0b01001) => s == 0, (true, 0b01001) => s <= 0, (false, _) => s < 0, _ => return None };
                set_el(&mut out, es, k, if t { ones } else { 0 });
            },
            0b01011 => for k in 0..lanes {
                let s = sel(n, es, k);
                set_el(&mut out, es, k, if u { s.wrapping_neg() as u64 } else { s.unsigned_abs() } & ones);
            },
            0b10010 | 0b10100 => {
                // XTN / SQXTUN (opc 10010) and SQXTN / UQXTN (10100): narrow 2*es -> es
                let we = es * 2;
                if we > 64 { return None; }
                let nl = if scalar { 1 } else { 64 / es };
                let off = if q && !scalar { nl } else { 0 };
                out = if q && !scalar { d & u64::MAX as u128 } else { 0 };
                for k in 0..nl {
                    let x = el(n, we, k);
                    let v = match (opc, u) {
                        (0b10010, false) => x & ones,
                        (0b10010, true) => { let (r, s) = sat(sx(x, we) as i128, es, false); satd |= s; r }
                        (_, false) => { let (r, s) = sat(sx(x, we) as i128, es, true); satd |= s; r }
                        (_, true) => { let (r, s) = sat(x as i128, es, false); satd |= s; r }
                    };
                    set_el(&mut out, es, k + off, v);
                }
                self.qc(satd);
                self.v[rd] = out;
                return Some(Ok(()));
            }
            0b10011 if u && !scalar => {
                // SHLL / SHLL2
                let half = 64 / es;
                let off = if q { half } else { 0 };
                for k in 0..half { set_el(&mut out, es * 2, k, el(n, es, k + off) << es); }
                self.v[rd] = out;
                return Some(Ok(()));
            }
            _ => return None,
        }
        self.qc(satd);
        self.v[rd] = if scalar { out & ones as u128 } else { qmask(q, out) };
        Some(Ok(()))
    }

    fn two_misc_fp(&mut self, i: u32, scalar: bool) -> Result<(), Trap> {
        let q = bit(i, 30) || scalar;
        let u = bit(i, 29);
        let a_bit = bit(i, 23);
        let sz = bit(i, 22);
        let opc = bits(i, 16, 12);
        let (rd, rn) = (bits(i, 4, 0) as usize, bits(i, 9, 5) as usize);
        let n = self.v[rn];
        let es = if sz { 64 } else { 32 };
        let mut fp = self.fpu();
        // FCVTN/FCVTXN (narrow, into the low or high half) and FCVTL (widen, from either half)
        if !a_bit && (opc == 0b10110 || opc == 0b10111) {
            let (wide, narrow) = if sz { (64, 32) } else { (32, 16) };
            if opc == 0b10110 {
                if u && !sz { return Err(self.undefined()); }
                if u { fp.rnd = Rnd::Odd; } // FCVTXN
                let lanes = if scalar { 1 } else { 64 / narrow };
                let off = if q && !scalar { lanes } else { 0 };
                let mut out = if off != 0 { self.v[rd] & u64::MAX as u128 } else { 0 };
                for k in 0..lanes { set_el(&mut out, narrow, k + off, fp.convert(wide, narrow, el(n, wide, k))); }
                self.v[rd] = out;
            } else {
                if scalar || u { return Err(self.undefined()); }
                let lanes = 128 / wide;
                let off = if q { lanes } else { 0 };
                let mut out = 0u128;
                for k in 0..lanes { set_el(&mut out, wide, k, fp.convert(narrow, wide, el(n, narrow, k + off))); }
                self.v[rd] = out;
            }
            self.fpu_done(fp);
            return Ok(());
        }
        let lanes = if scalar { 1 } else if q { 128 / es } else { 64 / es };
        if !scalar && !q && es == 64 { return Err(self.undefined()); }
        let mut out = 0u128;
        let all = emask(es);
        let sign = 1u64 << (es - 1);
        let cur = fp.rnd;
        for k in 0..lanes {
            let x = el(n, es, k);
            let t = |b: bool| if b { all } else { 0 };
            let v = match (a_bit, opc, u) {
                (true, 0b01100, false) => t(fp.cmp_pred(es, x, 0, Pred::Gt)), // FCMGT #0
                (true, 0b01100, true) => t(fp.cmp_pred(es, x, 0, Pred::Ge)),  // FCMGE #0
                (true, 0b01101, false) => t(fp.cmp_pred(es, x, 0, Pred::Eq)), // FCMEQ #0
                (true, 0b01101, true) => t(fp.cmp_pred(es, 0, x, Pred::Ge)),  // FCMLE #0
                (true, 0b01110, false) => t(fp.cmp_pred(es, 0, x, Pred::Gt)), // FCMLT #0
                (true, 0b01111, false) => x & !sign,                           // FABS
                (true, 0b01111, true) => x ^ sign,                             // FNEG
                (true, 0b11111, true) => fp.sqrt(es, x),
                (false, 0b11000, false) => fp.round_int(es, x, Rnd::Even, false), // FRINTN
                (false, 0b11000, true) => fp.round_int(es, x, Rnd::Away, false),  // FRINTA
                (false, 0b11001, false) => fp.round_int(es, x, Rnd::Down, false), // FRINTM
                (false, 0b11001, true) => fp.round_int(es, x, cur, true),         // FRINTX
                (true, 0b11000, false) => fp.round_int(es, x, Rnd::Up, false),    // FRINTP
                (true, 0b11001, false) => fp.round_int(es, x, Rnd::Zero, false),  // FRINTZ
                (true, 0b11001, true) => fp.round_int(es, x, cur, false),         // FRINTI
                (false, 0b11010, _) => fp.to_int(es, x, Rnd::Even, u, es, 0),     // FCVTN[SU]
                (false, 0b11011, _) => fp.to_int(es, x, Rnd::Down, u, es, 0),     // FCVTM[SU]
                (false, 0b11100, _) => fp.to_int(es, x, Rnd::Away, u, es, 0),     // FCVTA[SU]
                (false, 0b11101, _) => fp.from_int(es, x, !u, es, 0),             // [SU]CVTF
                (true, 0b11010, _) => fp.to_int(es, x, Rnd::Up, u, es, 0),        // FCVTP[SU]
                (true, 0b11011, _) => fp.to_int(es, x, Rnd::Zero, u, es, 0),      // FCVTZ[SU]
                (true, 0b11101, false) => fp.recip_estimate(es, x),               // FRECPE
                (true, 0b11101, true) => fp.rsqrt_estimate(es, x),                // FRSQRTE
                _ => return Err(Trap::Unimplemented(self.pc, i)),
            };
            set_el(&mut out, es, k, v);
        }
        self.fpu_done(fp);
        self.v[rd] = if scalar { out & all as u128 } else { qmask(q, out) };
        Ok(())
    }

    // ================================================================== across lanes / scalar pairwise
    fn across(&mut self, i: u32) -> Option<Result<(), Trap>> {
        let q = bit(i, 30);
        let u = bit(i, 29);
        let size = bits(i, 23, 22);
        let opc = bits(i, 16, 12);
        let (rd, rn) = (bits(i, 4, 0) as usize, bits(i, 9, 5) as usize);
        let n = self.v[rn];
        if opc == 0b01100 || opc == 0b01111 {
            // FMAXNMV / FMINNMV / FMAXV / FMINV (single precision, Q=1): Reduce() pairs (0,1),(2,3)
            let es = 32;
            let mut fp = self.fpu();
            let mut op = |a: u64, b: u64| match (opc, bit(i, 23)) {
                (0b01100, false) => fp.maxnm(es, a, b),
                (0b01100, true) => fp.minnm(es, a, b),
                (_, false) => fp.max(es, a, b),
                (_, true) => fp.min(es, a, b),
            };
            let (x, y) = (op(el(n, es, 0), el(n, es, 1)), op(el(n, es, 2), el(n, es, 3)));
            let r = op(x, y);
            self.fpu_done(fp);
            self.v[rd] = r as u128;
            return Some(Ok(()));
        }
        let es = 8u32 << size;
        let lanes = if q { 128 / es } else { 64 / es };
        let vals: Vec<i128> = (0..lanes).map(|k| if u { el(n, es, k) as i128 } else { sel(n, es, k) as i128 }).collect();
        let (r, out_es): (i128, u32) = match opc {
            0b11011 if !u => (vals.iter().sum(), es),
            0b00011 => (vals.iter().sum(), es * 2),
            0b01010 => (*vals.iter().max().unwrap(), es),
            0b11010 => (*vals.iter().min().unwrap(), es),
            _ => return None,
        };
        self.v[rd] = ((r as u64) & emask(out_es)) as u128;
        Some(Ok(()))
    }

    fn scalar_pairwise(&mut self, i: u32) -> Option<Result<(), Trap>> {
        let u = bit(i, 29);
        let opc = bits(i, 16, 12);
        let (rd, rn) = (bits(i, 4, 0) as usize, bits(i, 9, 5) as usize);
        let n = self.v[rn];
        if !u && opc == 0b11011 && bits(i, 23, 22) == 3 {
            self.v[rd] = el(n, 64, 0).wrapping_add(el(n, 64, 1)) as u128; // ADDP d
            return Some(Ok(()));
        }
        if u {
            let es = if bit(i, 22) { 64 } else { 32 };
            let (a, b) = (el(n, es, 0), el(n, es, 1));
            let mut fp = self.fpu();
            let v = match (opc, bit(i, 23)) {
                (0b01101, false) => fp.add(es, a, b),
                (0b01100, false) => fp.maxnm(es, a, b),
                (0b01100, true) => fp.minnm(es, a, b),
                (0b01111, false) => fp.max(es, a, b),
                (0b01111, true) => fp.min(es, a, b),
                _ => return None,
            };
            self.fpu_done(fp);
            self.v[rd] = v as u128;
            return Some(Ok(()));
        }
        None
    }

    // ================================================================== copy / permute / table
    fn copy(&mut self, i: u32) -> Option<Result<(), Trap>> {
        let q = bit(i, 30);
        let imm5 = bits(i, 20, 16);
        let imm4 = bits(i, 14, 11);
        let (rd, rn) = (bits(i, 4, 0) as usize, bits(i, 9, 5) as usize);
        if imm5 & 0xf == 0 { return Some(Err(self.undefined())); }
        let sz = imm5.trailing_zeros();
        let es = 8u32 << sz;
        let idx = imm5 >> (sz + 1);
        let n = self.v[rn];
        let rep = |x: u64| { let mut v = 0u128; for k in 0..128 / es { set_el(&mut v, es, k, x); } v };
        match imm4 {
            0b0000 => self.v[rd] = qmask(q, rep(el(n, es, idx))),
            0b0001 => { let x = self.xr(rn as u32); self.v[rd] = qmask(q, rep(x)); }
            0b0011 => { let x = self.xr(rn as u32); set_el(&mut self.v[rd], es, idx, x); }
            0b0101 => { let x = sx(el(n, es, idx), es) as u64; self.set_x(rd as u32, if q { x } else { x & 0xffff_ffff }); }
            0b0111 => { let x = el(n, es, idx); self.set_x(rd as u32, x); }
            _ => return None,
        }
        Some(Ok(()))
    }

    fn ins_elem(&mut self, i: u32) -> Option<Result<(), Trap>> {
        let imm5 = bits(i, 20, 16);
        let imm4 = bits(i, 14, 11);
        let (rd, rn) = (bits(i, 4, 0) as usize, bits(i, 9, 5) as usize);
        if imm5 & 0xf == 0 { return Some(Err(self.undefined())); }
        let sz = imm5.trailing_zeros();
        let es = 8u32 << sz;
        let x = el(self.v[rn], es, imm4 >> sz);
        set_el(&mut self.v[rd], es, imm5 >> (sz + 1), x);
        Some(Ok(()))
    }

    fn scalar_dup(&mut self, i: u32) -> Option<Result<(), Trap>> {
        let imm5 = bits(i, 20, 16);
        let (rd, rn) = (bits(i, 4, 0) as usize, bits(i, 9, 5) as usize);
        if imm5 & 0xf == 0 { return Some(Err(self.undefined())); }
        let sz = imm5.trailing_zeros();
        let es = 8u32 << sz;
        self.v[rd] = el(self.v[rn], es, imm5 >> (sz + 1)) as u128;
        Some(Ok(()))
    }

    fn permute(&mut self, i: u32) -> Option<Result<(), Trap>> {
        let q = bit(i, 30);
        let es = 8u32 << bits(i, 23, 22);
        let opc = bits(i, 14, 12);
        let (rd, rn, rm) = (bits(i, 4, 0) as usize, bits(i, 9, 5) as usize, bits(i, 20, 16) as usize);
        let (n, m) = (self.v[rn], self.v[rm]);
        let lanes = if q { 128 / es } else { 64 / es };
        let half = lanes / 2;
        let mut out = 0u128;
        for k in 0..lanes {
            let v = match opc {
                0b001 | 0b101 => { let j = 2 * k + (opc >> 2); if j < lanes { el(n, es, j) } else { el(m, es, j - lanes) } }
                0b010 | 0b110 => { let p = opc >> 2; if k % 2 == 0 { el(n, es, k + p) } else { el(m, es, k - 1 + p) } }
                0b011 | 0b111 => { let base = if opc == 0b111 { half } else { 0 }; if k % 2 == 0 { el(n, es, base + k / 2) } else { el(m, es, base + k / 2) } }
                _ => return None,
            };
            set_el(&mut out, es, k, v);
        }
        self.v[rd] = qmask(q, out);
        Some(Ok(()))
    }

    fn ext(&mut self, i: u32) -> Option<Result<(), Trap>> {
        let q = bit(i, 30);
        let pos = bits(i, 14, 11) as usize;
        let (rd, rn, rm) = (bits(i, 4, 0) as usize, bits(i, 9, 5) as usize, bits(i, 20, 16) as usize);
        let w = if q { 16 } else { 8 };
        let mut b = [0u8; 32];
        b[..16].copy_from_slice(&self.v[rn].to_le_bytes());
        b[w..w + 16].copy_from_slice(&self.v[rm].to_le_bytes());
        let mut out = [0u8; 16];
        out[..w].copy_from_slice(&b[pos..pos + w]);
        self.v[rd] = u128::from_le_bytes(out);
        Some(Ok(()))
    }

    fn tbl(&mut self, i: u32) -> Option<Result<(), Trap>> {
        let q = bit(i, 30);
        let len = bits(i, 14, 13) as usize + 1;
        let tbx = bit(i, 12);
        let (rd, rn, rm) = (bits(i, 4, 0) as usize, bits(i, 9, 5) as usize, bits(i, 20, 16) as usize);
        let mut table = Vec::with_capacity(16 * len);
        for r in 0..len { table.extend_from_slice(&self.v[(rn + r) % 32].to_le_bytes()); }
        let idx = self.v[rm].to_le_bytes();
        let d = self.v[rd].to_le_bytes();
        let mut out = [0u8; 16];
        for k in 0..if q { 16 } else { 8 } {
            let j = idx[k] as usize;
            out[k] = if j < table.len() { table[j] } else if tbx { d[k] } else { 0 };
        }
        self.v[rd] = u128::from_le_bytes(out);
        Some(Ok(()))
    }

    // ================================================================== shift by immediate
    fn shift_imm(&mut self, i: u32, scalar: bool) -> Option<Result<(), Trap>> {
        let q = bit(i, 30) || scalar;
        let u = bit(i, 29);
        let immh = bits(i, 22, 19);
        let immhb = bits(i, 22, 16);
        let opc = bits(i, 15, 11);
        let (rd, rn) = (bits(i, 4, 0) as usize, bits(i, 9, 5) as usize);
        let n = self.v[rn];
        let d = self.v[rd];
        let hs = 31 - immh.leading_zeros();
        let es = 8u32 << hs;
        let narrow = matches!(opc, 0b10000..=0b10011);
        let long = opc == 0b10100;
        let lanes = if scalar { 1 } else if q { 128 / es } else { 64 / es };
        let shr = 2 * es - immhb; // right shift amount (for es; narrowing uses the narrow es)
        let shl = immhb - es;
        let mut satd = false;
        if long {
            if scalar { return None; }
            let half = 64 / es;
            let off = if q { half } else { 0 };
            let mut out = 0u128;
            for k in 0..half {
                let a = el(n, es, k + off);
                let a = if u { a } else { sx(a, es) as u64 };
                set_el(&mut out, es * 2, k, a << shl);
            }
            self.v[rd] = out;
            return Some(Ok(()));
        }
        if narrow {
            // es is the destination element size; source elements are 2*es wide
            let we = es * 2;
            if we > 64 { return None; }
            let nl = if scalar { 1 } else { 64 / es };
            let off = if q && !scalar { nl } else { 0 };
            let mut out = if q && !scalar { d & u64::MAX as u128 } else { 0 };
            let round = opc & 1 != 0;
            for k in 0..nl {
                let x = el(n, we, k);
                let signed_src = !(opc >> 1 == 0b1000 && !u) && !(opc >> 1 == 0b1001 && u);
                let src = if opc >> 1 == 0b1000 && !u { x as i128 } else if signed_src { sx(x, we) as i128 } else { x as i128 };
                let v = (src + if round { 1i128 << (shr - 1) } else { 0 }) >> shr;
                let r = match (opc >> 1, u) {
                    (0b1000, false) => (v as u64) & emask(es),                                  // SHRN / RSHRN
                    (0b1000, true) => { let (r, s) = sat(v, es, false); satd |= s; r }          // SQSHRUN / SQRSHRUN
                    (0b1001, false) => { let (r, s) = sat(v, es, true); satd |= s; r }          // SQSHRN / SQRSHRN
                    (_, _) => { let (r, s) = sat(v, es, false); satd |= s; r }                  // UQSHRN / UQRSHRN
                };
                set_el(&mut out, es, k + off, r);
            }
            self.qc(satd);
            self.v[rd] = out;
            return Some(Ok(()));
        }
        let mut out = 0u128;
        for k in 0..lanes {
            let a = el(n, es, k);
            let acc = el(d, es, k);
            let wide: i128 = if u { a as i128 } else { sx(a, es) as i128 };
            let rshift = |round: bool| -> u64 {
                let r = shr;
                let rnd = if round { 1i128 << (r - 1) } else { 0 };
                (((wide + rnd) >> r.min(127)) as u64) & emask(es)
            };
            let v = match (opc, u) {
                (0b00000, _) => rshift(false),
                (0b00010, _) => acc.wrapping_add(rshift(false)),
                (0b00100, _) => rshift(true),
                (0b00110, _) => acc.wrapping_add(rshift(true)),
                (0b01000, true) => { let msk = if shr >= es { 0 } else { emask(es) >> shr }; (acc & !msk) | (rshift(false) & msk) }
                (0b01010, false) => (a << shl) & emask(es),
                (0b01010, true) => { let msk = (emask(es) << shl) & emask(es); (acc & !msk) | ((a << shl) & msk) }
                (0b01100, true) => { let (r, s) = sat((sx(a, es) as i128) << shl, es, false); satd |= s; r } // SQSHLU
                (0b01110, _) => { let (r, s) = sat(wide << shl, es, !u); satd |= s; r }                      // SQSHL/UQSHL
                (0b11100, _) if es >= 32 => { let mut fp = self.fpu(); let v = fp.from_int(es, a, !u, es, shr); self.fpu_done(fp); v } // [SU]CVTF fixed
                (0b11111, _) if es >= 32 => { let mut fp = self.fpu(); let v = fp.to_int(es, a, Rnd::Zero, u, es, shr); self.fpu_done(fp); v } // FCVTZ[SU] fixed
                _ => return None,
            };
            set_el(&mut out, es, k, v);
        }
        self.qc(satd);
        self.v[rd] = if scalar { out & emask(es) as u128 } else { qmask(q, out) };
        Some(Ok(()))
    }

    // ================================================================== by element
    fn by_element(&mut self, i: u32, scalar: bool) -> Option<Result<(), Trap>> {
        let q = bit(i, 30) || scalar;
        let u = bit(i, 29);
        let size = bits(i, 23, 22);
        let opc = bits(i, 15, 12);
        let (l, m_hi, h) = (bits(i, 21, 21), bits(i, 20, 20), bits(i, 11, 11));
        let (rd, rn) = (bits(i, 4, 0) as usize, bits(i, 9, 5) as usize);
        let fp = matches!(opc, 0b0001 | 0b0101 | 0b1001) && size >= 2;
        let (es, idx, rm) = if fp {
            if size == 3 { (64, h, bits(i, 20, 16)) } else { (32, (h << 1) | l, bits(i, 20, 16)) }
        } else {
            match size {
                1 => (16, (h << 2) | (l << 1) | m_hi, bits(i, 19, 16)),
                2 => (32, (h << 1) | l, bits(i, 20, 16)),
                _ => return None,
            }
        };
        let (n, d) = (self.v[rn], self.v[rd]);
        let e = el(self.v[rm as usize], es, idx);
        let lanes = if scalar { 1 } else if q { 128 / es } else { 64 / es };
        let mut out = 0u128;
        let mut satd = false;
        if !fp && matches!(opc, 0b0010 | 0b0110 | 0b1010 | 0b0011 | 0b0111 | 0b1011) {
            // long forms: [SU]MLAL, [SU]MLSL, [SU]MULL, SQDMLAL, SQDMLSL, SQDMULL (by element)
            let half = if scalar { 1 } else { 64 / es };
            let off = if bit(i, 30) && !scalar { half } else { 0 };
            let we = es * 2;
            let eb: i128 = if u { e as i128 } else { sx(e, es) as i128 };
            for k in 0..half {
                let a: i128 = if u { el(n, es, k + off) as i128 } else { sel(n, es, k + off) as i128 };
                let acc = if u { el(d, we, k) as i128 } else { sel(d, we, k) as i128 };
                let v = match opc {
                    0b0010 => acc + a * eb,
                    0b0110 => acc - a * eb,
                    0b1010 => a * eb,
                    _ if u => return None,
                    _ => {
                        let (p, s1) = sat(2 * a * eb, we, true);
                        satd |= s1;
                        let p = sx(p, we) as i128;
                        let r = match opc { 0b0011 => acc + p, 0b0111 => acc - p, _ => p };
                        let (r, s2) = sat(r, we, true);
                        satd |= s2;
                        sx(r, we) as i128
                    }
                };
                set_el(&mut out, we, k, (v as u64) & emask(we));
            }
            self.qc(satd);
            self.v[rd] = out;
            return Some(Ok(()));
        }
        for k in 0..lanes {
            let a = el(n, es, k);
            let acc = el(d, es, k);
            let v = match (opc, u, fp) {
                (0b0000, true, false) => acc.wrapping_add(a.wrapping_mul(e)),
                (0b0100, true, false) => acc.wrapping_sub(a.wrapping_mul(e)),
                (0b1000, false, false) => a.wrapping_mul(e),
                (0b1100, false, false) | (0b1101, false, false) => {
                    let p = 2 * (sx(a, es) as i128) * (sx(e, es) as i128) + if opc == 0b1101 { 1i128 << (es - 1) } else { 0 };
                    let (r, s) = sat(p >> es, es, true);
                    satd |= s;
                    r
                }
                (0b0001, false, true) | (0b0101, false, true) => { let mut f = self.fpu(); let v = f.fma(es, acc, if opc == 0b0101 { a ^ (1u64 << (es - 1)) } else { a }, e); self.fpu_done(f); v }
                (0b1001, false, true) => { let mut f = self.fpu(); let v = f.mul(es, a, e); self.fpu_done(f); v }
                (0b1001, true, true) => { let mut f = self.fpu(); let v = f.mulx(es, a, e); self.fpu_done(f); v } // FMULX
                _ => return None,
            };
            set_el(&mut out, es, k, v);
        }
        self.qc(satd);
        self.v[rd] = if scalar { out & emask(es) as u128 } else { qmask(q, out) };
        Some(Ok(()))
    }
}
