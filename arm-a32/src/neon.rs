//! ARMv7 Advanced SIMD (NEON) with the VFPv4 fused multiply-add extension.
//!
//! Decoded from the A32 encodings (`1111 001U ...` data processing, `1111 0100 xxx0 ...`
//! element and structure loads/stores); T32 maps its `111U 1111` and `1111 1001` forms onto
//! these (`thumb.rs`). The scalar transfers in coprocessor space (VMOV core <-> scalar,
//! VDUP from a core register) live with the other CP10/CP11 transfers in `vfp.rs`.
//!
//! Semantics follow the ARMv7 pseudocode: floating-point operations use the Standard
//! FPSCR value (flush-to-zero on inputs and outputs, default NaN, round to nearest)
//! whatever FPSCR holds, VMLA/VMLS/VRECPS/VRSQRTS round the product separately (only
//! VFMA/VFMS fuse), and saturation sets FPSCR.QC. Cumulative floating-point exception flags
//! are not raised, as in `vfp.rs`. Access obeys CPACR (CP10/CP11, ASEDIS) and FPEXC.EN
//! like VFP, so Linux's lazy enable traps the first NEON instruction of a task.
use crate::cpu::Cpu;
use crate::mmu::Access;
use emu_core::{Bus, Trap};

const QC: u32 = 1 << 27;
const FPEXC_EN: u32 = 1 << 30;
const CPACR_ASEDIS: u32 = 1 << 31;
pub(crate) const DEFAULT_NAN: u32 = 0x7fc0_0000;

/// Reason an Advanced SIMD encoding did not execute.
#[derive(Clone, Copy)]
enum Fail {
    Undefined,
    Trap(Trap),
}

type R = Result<(), Fail>;

#[inline]
fn ones(esize: u32) -> u64 {
    u64::MAX >> (64 - esize)
}

#[inline]
fn sext(x: u64, esize: u32) -> i64 {
    ((x << (64 - esize)) as i64) >> (64 - esize)
}

/// Element `i` of a little-endian vector held as consecutive D registers.
#[inline]
fn elem(v: &[u64], i: usize, esize: u32) -> u64 {
    let per = (64 / esize) as usize;
    (v[i / per] >> ((i % per) as u32 * esize)) & ones(esize)
}

#[inline]
fn set_elem(v: &mut [u64], i: usize, esize: u32, x: u64) {
    let per = (64 / esize) as usize;
    let shift = (i % per) as u32 * esize;
    v[i / per] = v[i / per] & !(ones(esize) << shift) | (x & ones(esize)) << shift;
}

fn replicate(x: u64, esize: u32) -> u64 {
    let mut r = 0;
    for i in 0..64 / esize {
        r |= (x & ones(esize)) << (i * esize);
    }
    r
}

/// Saturate to an `esize`-bit signed or unsigned integer, noting saturation in `sat`.
fn saturate(x: i128, esize: u32, unsigned: bool, sat: &mut bool) -> u64 {
    let (min, max) = if unsigned {
        (0, (1i128 << esize) - 1)
    } else {
        (-(1i128 << (esize - 1)), (1i128 << (esize - 1)) - 1)
    };
    if x > max {
        *sat = true;
        max as u64
    } else if x < min {
        *sat = true;
        min as u64
    } else {
        x as u64
    }
}

/// Shift by a signed amount (negative = right), optionally rounding right shifts. Values
/// fit in 65 bits, so left shifts of 64 or more only need to keep the sign for saturation.
fn shift_signed(x: i128, amount: i32, round: bool) -> i128 {
    if amount >= 0 {
        if amount >= 64 {
            x.signum() << 100
        } else {
            x << amount
        }
    } else {
        let right = (-amount).min(100);
        let x = if round { x + (1i128 << (right - 1)) } else { x };
        x >> right
    }
}

/// Carry-less multiply of two 8-bit polynomials.
fn poly_mul(a: u64, b: u64) -> u64 {
    let mut r = 0;
    for i in 0..8 {
        if b >> i & 1 != 0 {
            r ^= a << i;
        }
    }
    r
}

fn count_leading_zeros(x: u64, esize: u32) -> u64 {
    if x == 0 {
        esize as u64
    } else {
        (x.leading_zeros() - (64 - esize)) as u64
    }
}

// ---- Floating point with the Standard FPSCR value -------------------------------------

#[inline]
fn flush(x: u32) -> u32 {
    if x & 0x7f80_0000 == 0 {
        x & 0x8000_0000
    } else {
        x
    }
}

#[inline]
fn fpv(x: u32) -> f32 {
    f32::from_bits(flush(x))
}

/// Round an exact (or exactly representable) result: default NaN, flush tiny values.
fn fp_round(exact: f64) -> u32 {
    if exact.is_nan() {
        DEFAULT_NAN
    } else if exact != 0.0 && exact.abs() < f32::MIN_POSITIVE as f64 {
        (exact.is_sign_negative() as u32) << 31
    } else {
        (exact as f32).to_bits()
    }
}

/// A sum or difference of two floats in the subnormal range is exact, so the host result
/// only needs the default NaN and output flushing.
fn fp_result(r: f32) -> u32 {
    if r.is_nan() {
        DEFAULT_NAN
    } else {
        flush(r.to_bits())
    }
}

fn fp_add(a: u32, b: u32) -> u32 {
    fp_result(fpv(a) + fpv(b))
}

fn fp_sub(a: u32, b: u32) -> u32 {
    fp_result(fpv(a) - fpv(b))
}

fn fp_mul(a: u32, b: u32) -> u32 {
    // Single-precision products are exact in double precision: one rounding.
    fp_round(fpv(a) as f64 * fpv(b) as f64)
}

/// acc + a * b with one rounding (VFMA).
fn fp_mul_add(acc: u32, a: u32, b: u32) -> u32 {
    let (x, y, z) = (fpv(a), fpv(b), fpv(acc));
    let r = x.mul_add(y, z);
    if r.is_nan() {
        return DEFAULT_NAN;
    }
    if r.abs() <= f32::MIN_POSITIVE {
        // Flushing looks at the exact value before rounding. The product of two singles is
        // exact in double precision; TwoSum gives the exact error of the addition, so the
        // test does not depend on the double-precision rounding of the sum.
        let p = x as f64 * y as f64;
        let c = z as f64;
        let sum = p + c;
        let bb = sum - p;
        let err = (p - (sum - bb)) + (c - bb);
        let min = f32::MIN_POSITIVE as f64;
        let below = sum.abs() < min || sum.abs() == min && err != 0.0 && (err < 0.0) != (sum < 0.0);
        if (sum != 0.0 || err != 0.0) && below {
            let negative = if sum != 0.0 { sum < 0.0 } else { err < 0.0 };
            return (negative as u32) << 31;
        }
    }
    r.to_bits()
}

fn fp_neg(x: u32) -> u32 {
    x ^ 0x8000_0000
}

fn fp_max(a: u32, b: u32, max: bool) -> u32 {
    let (x, y) = (fpv(a), fpv(b));
    if x.is_nan() || y.is_nan() {
        return DEFAULT_NAN;
    }
    if x == 0.0 && y == 0.0 {
        let negative = if max {
            x.is_sign_negative() && y.is_sign_negative()
        } else {
            x.is_sign_negative() || y.is_sign_negative()
        };
        return (negative as u32) << 31;
    }
    let pick_x = if max { x > y } else { x < y };
    if pick_x { x.to_bits() } else { y.to_bits() }
}

fn fp_compare(a: u32, b: u32, op: u32) -> bool {
    let (x, y) = (fpv(a), fpv(b));
    match op {
        0 => x == y,
        1 => x >= y,
        _ => x > y,
    }
}

/// VRECPS: 2 - a * b with a separately rounded product (ARMv7 FPRecipStep).
fn fp_recip_step(a: u32, b: u32) -> u32 {
    let (x, y) = (fpv(a), fpv(b));
    if x.is_nan() || y.is_nan() {
        return DEFAULT_NAN;
    }
    let product = if x.is_infinite() && y == 0.0 || x == 0.0 && y.is_infinite() {
        0
    } else {
        fp_mul(a, b)
    };
    fp_sub(0x4000_0000, product)
}

/// VRSQRTS: (3 - a * b) / 2 with a separately rounded product (ARMv7 FPRSqrtStep).
fn fp_rsqrt_step(a: u32, b: u32) -> u32 {
    let (x, y) = (fpv(a), fpv(b));
    if x.is_nan() || y.is_nan() {
        return DEFAULT_NAN;
    }
    let product = if x.is_infinite() && y == 0.0 || x == 0.0 && y.is_infinite() {
        0
    } else {
        fp_mul(a, b)
    };
    fp_result((3.0 - f32::from_bits(product)) * 0.5)
}

/// RecipEstimate on a 9-bit fixed-point input 256..511 (0.5 <= a < 1.0).
fn recip_estimate(a: u32) -> u32 {
    let a = a * 2 + 1;
    let b = (1 << 19) / a;
    b.div_ceil(2)
}

fn fp_recip_estimate(x: u32) -> u32 {
    let x = flush(x);
    let sign = x & 0x8000_0000;
    let exp = (x >> 23) & 0xff;
    if exp == 0xff {
        return if x & 0x7f_ffff != 0 { DEFAULT_NAN } else { sign };
    }
    if exp == 0 {
        return sign | 0x7f80_0000;
    }
    if exp >= 253 {
        return sign; // |x| >= 2^126 underflows to zero with flush-to-zero
    }
    let estimate = recip_estimate(0x100 | (x >> 15) & 0xff);
    sign | (253 - exp) << 23 | (estimate & 0xff) << 15
}

/// RecipSqrtEstimate on a 9-bit fixed-point input 128..511 (0.25 <= a < 1.0).
fn rsqrt_estimate(a: u32) -> u32 {
    let mut a = a;
    if a < 256 {
        a = a * 2 + 1;
    } else {
        a = (a >> 1) << 1;
        a = (a + 1) * 2;
    }
    let mut b: u64 = 512;
    while (a as u64) * (b + 1) * (b + 1) < 1 << 28 {
        b += 1;
    }
    b.div_ceil(2) as u32
}

fn fp_rsqrt_estimate(x: u32) -> u32 {
    let x = flush(x);
    let sign = x & 0x8000_0000;
    let exp = (x >> 23) & 0xff;
    let fraction = x & 0x7f_ffff;
    if exp == 0xff && fraction != 0 {
        return DEFAULT_NAN;
    }
    if exp == 0 {
        return sign | 0x7f80_0000;
    }
    if sign != 0 {
        return DEFAULT_NAN;
    }
    if exp == 0xff {
        return 0;
    }
    let scaled = if exp & 1 == 0 { 0x100 | fraction >> 15 } else { 0x80 | fraction >> 16 };
    let result_exp = (380 - exp) / 2;
    let estimate = rsqrt_estimate(scaled);
    result_exp << 23 | (estimate & 0xff) << 15
}

fn unsigned_recip_estimate(x: u32) -> u32 {
    if x >> 31 == 0 {
        return u32::MAX;
    }
    recip_estimate(x >> 23) << 23
}

fn unsigned_rsqrt_estimate(x: u32) -> u32 {
    if x >> 30 == 0 {
        return u32::MAX;
    }
    rsqrt_estimate(x >> 23) << 23
}

/// Single-precision float to a 32-bit fixed-point integer, rounding toward zero and
/// saturating (VCVT with the Standard FPSCR value).
fn fp_to_fixed(x: u32, fraction_bits: u32, unsigned: bool) -> u32 {
    let value = fpv(x);
    if value.is_nan() {
        return 0;
    }
    let scaled = (value as f64 * (2.0f64).powi(fraction_bits as i32)).trunc();
    if unsigned { scaled as u32 } else { scaled as i32 as u32 }
}

fn fixed_to_fp(x: u32, fraction_bits: u32, unsigned: bool) -> u32 {
    let integer = if unsigned { x as f64 } else { x as i32 as f64 };
    fp_round(integer / (2.0f64).powi(fraction_bits as i32))
}

/// Half precision to single precision (exact). `ahp` selects the alternative format
/// (no infinities or NaNs); `dn` returns the default NaN for NaN inputs.
pub(crate) fn f16_to_f32(h: u16, ahp: bool, dn: bool) -> u32 {
    let sign = (h as u32 & 0x8000) << 16;
    let exp = (h as u32 >> 10) & 0x1f;
    let fraction = h as u32 & 0x3ff;
    if exp == 0x1f && !ahp {
        if fraction == 0 {
            return sign | 0x7f80_0000;
        }
        if dn {
            return DEFAULT_NAN;
        }
        return sign | 0x7fc0_0000 | fraction << 13;
    }
    if exp == 0 {
        return sign | (fraction as f32 * (2.0f32).powi(-24)).to_bits();
    }
    sign | (exp + 112) << 23 | fraction << 13
}

/// Single precision to half precision with FPSCR-style controls: rounding mode `rmode`
/// (0 nearest, 1 +inf, 2 -inf, 3 zero), alternative format, default NaN, flush-to-zero
/// of the single-precision input.
pub(crate) fn f32_to_f16(x: u32, rmode: u32, ahp: bool, dn: bool, fz: bool) -> u16 {
    let x = if fz { flush(x) } else { x };
    let sign = ((x >> 16) & 0x8000) as u16;
    let negative = sign != 0;
    let exp = (x >> 23) & 0xff;
    let fraction = x & 0x7f_ffff;
    if exp == 0xff {
        if fraction != 0 {
            return if ahp {
                sign // no NaNs in the alternative format: a signed zero (Invalid Operation)
            } else if dn {
                0x7e00
            } else {
                sign | 0x7e00 | (fraction >> 13) as u16
            };
        }
        return if ahp { sign | 0x7fff } else { sign | 0x7c00 };
    }
    if exp == 0 && fraction == 0 {
        return sign;
    }
    let (m, e) = if exp == 0 {
        (fraction as u64, -149)
    } else {
        ((fraction | 0x80_0000) as u64, exp as i32 - 150)
    };
    let msb = 63 - m.leading_zeros() as i32 + e;
    let mut q = (msb - 10).max(-24);
    let shift = q - e;
    let mut r = if shift <= 0 {
        m << -shift
    } else {
        let (kept, rest, half) = if shift >= 64 {
            (0, m, None)
        } else {
            (m >> shift, m & ((1u64 << shift) - 1), Some(1u64 << (shift - 1)))
        };
        let up = match rmode {
            0 => match half {
                Some(h) => rest > h || rest == h && kept & 1 != 0,
                None => false,
            },
            1 => rest != 0 && !negative,
            2 => rest != 0 && negative,
            _ => false,
        };
        kept + up as u64
    };
    if r >= 0x800 {
        r >>= 1;
        q += 1;
    }
    if r < 0x400 {
        return sign | r as u16;
    }
    let biased = q + 25;
    let limit = if ahp { 32 } else { 31 };
    if biased >= limit {
        if ahp {
            return sign | 0x7fff;
        }
        let to_infinity = match rmode {
            0 => true,
            1 => !negative,
            2 => negative,
            _ => false,
        };
        return sign | if to_infinity { 0x7c00 } else { 0x7bff };
    }
    sign | (biased as u16) << 10 | (r & 0x3ff) as u16
}

/// AdvSIMDExpandImm; None for the reserved encoding.
fn expand_imm(op: u32, cmode: u32, imm8: u32) -> Option<u64> {
    let imm8 = imm8 as u64;
    Some(match cmode >> 1 {
        0 => replicate(imm8, 32),
        1 => replicate(imm8 << 8, 32),
        2 => replicate(imm8 << 16, 32),
        3 => replicate(imm8 << 24, 32),
        4 => replicate(imm8, 16),
        5 => replicate(imm8 << 8, 16),
        6 => {
            if cmode & 1 == 0 {
                replicate(imm8 << 8 | 0xff, 32)
            } else {
                replicate(imm8 << 16 | 0xffff, 32)
            }
        }
        _ => match (cmode & 1, op) {
            (0, 0) => replicate(imm8, 8),
            (0, _) => {
                let mut r = 0;
                for i in 0..8 {
                    if imm8 >> i & 1 != 0 {
                        r |= 0xff << (i * 8);
                    }
                }
                r
            }
            (_, 0) => {
                let b6 = (imm8 >> 6) & 1;
                let single = (imm8 >> 7) << 31
                    | (b6 ^ 1) << 30
                    | if b6 != 0 { 0x1f << 25 } else { 0 }
                    | (imm8 & 0x3f) << 19;
                replicate(single, 32)
            }
            _ => return None,
        },
    })
}

#[inline]
fn vd(insn: u32) -> usize {
    ((insn >> 18) & 0x10 | (insn >> 12) & 0xf) as usize
}

#[inline]
fn vn(insn: u32) -> usize {
    ((insn >> 3) & 0x10 | (insn >> 16) & 0xf) as usize
}

#[inline]
fn vm(insn: u32) -> usize {
    ((insn >> 1) & 0x10 | insn & 0xf) as usize
}

impl Cpu {
    fn neon_enabled(&self) -> bool {
        self.vfp_accessible() && self.vfp.fpexc & FPEXC_EN != 0 && self.cpacr & CPACR_ASEDIS == 0
    }

    /// Execute an Advanced SIMD data-processing or element/structure load/store
    /// instruction (A32 encoding); None if `insn` is not one.
    pub(crate) fn execute_neon(
        &mut self,
        bus: &mut dyn Bus,
        pc: u64,
        insn: u32,
    ) -> Option<Result<(), Trap>> {
        let data = insn & 0xfe00_0000 == 0xf200_0000;
        if !data && insn & 0xff10_0000 != 0xf400_0000 {
            return None;
        }
        if !self.neon_enabled() {
            return Some(Err(Trap::Unimplemented(pc, insn)));
        }
        let result = if data { self.neon_data(insn) } else { self.neon_load_store(bus, insn) };
        Some(match result {
            Ok(()) => Ok(()),
            Err(Fail::Undefined) => Err(Trap::Unimplemented(pc, insn)),
            Err(Fail::Trap(trap)) => Err(trap),
        })
    }

    fn dregs(&self, first: usize, count: usize) -> [u64; 4] {
        let mut v = [0; 4];
        for (i, slot) in v.iter_mut().enumerate().take(count) {
            *slot = self.vfp.d[(first + i) & 31];
        }
        v
    }

    fn set_dregs(&mut self, first: usize, count: usize, v: &[u64; 4]) {
        for (i, &value) in v.iter().enumerate().take(count) {
            self.vfp.d[(first + i) & 31] = value;
        }
    }

    fn set_qc(&mut self, sat: bool) {
        if sat {
            self.vfp.fpscr |= QC;
        }
    }

    /// Elementwise `f(n, m, d)` over `regs` D registers of `esize`-bit lanes.
    #[allow(clippy::too_many_arguments)]
    fn lanes3(
        &mut self,
        d: usize,
        n: usize,
        m: usize,
        regs: usize,
        esize: u32,
        mut f: impl FnMut(u64, u64, u64) -> u64,
    ) {
        let (a, b, c) = (self.dregs(n, regs), self.dregs(m, regs), self.dregs(d, regs));
        let mut out = [0u64; 4];
        for i in 0..regs * (64 / esize) as usize {
            set_elem(&mut out, i, esize, f(elem(&a, i, esize), elem(&b, i, esize), elem(&c, i, esize)));
        }
        self.set_dregs(d, regs, &out);
    }

    /// Pairwise `f` over the lanes of Dn then Dm, into Dd.
    fn pairwise(&mut self, d: usize, n: usize, m: usize, esize: u32, mut f: impl FnMut(u64, u64) -> u64) {
        let (a, b) = ([self.vfp.d[n]], [self.vfp.d[m]]);
        let half = (32 / esize) as usize;
        let mut out = [0u64];
        for i in 0..half {
            set_elem(&mut out, i, esize, f(elem(&a, 2 * i, esize), elem(&a, 2 * i + 1, esize)));
            set_elem(&mut out, half + i, esize, f(elem(&b, 2 * i, esize), elem(&b, 2 * i + 1, esize)));
        }
        self.vfp.d[d] = out[0];
    }

    fn neon_data(&mut self, insn: u32) -> R {
        let u = insn >> 24 & 1 != 0;
        let a = (insn >> 19) & 0x1f;
        let b = (insn >> 8) & 0xf;
        let c = (insn >> 4) & 0xf;
        if a & 0x10 == 0 {
            return self.neon_three_same(insn);
        }
        if c & 1 != 0 {
            if c & 8 == 0 && a & 0x7 == 0 {
                return self.neon_modified_imm(insn);
            }
            return self.neon_shift_imm(insn);
        }
        if a & 0x6 == 0x6 {
            if !u {
                return self.neon_ext(insn);
            }
            if b & 0x8 == 0 {
                return self.neon_misc(insn);
            }
            if b & 0xc == 0x8 {
                return self.neon_table(insn);
            }
            if b == 0xc && c & 0x8 == 0 {
                return self.neon_dup_scalar(insn);
            }
            return Err(Fail::Undefined);
        }
        if c & 0x4 == 0 {
            self.neon_three_diff(insn)
        } else {
            self.neon_scalar(insn)
        }
    }

    // ---- Three registers of the same length ----------------------------------------

    fn neon_three_same(&mut self, insn: u32) -> R {
        let u = insn >> 24 & 1 != 0;
        let size = (insn >> 20) & 3;
        let op = (insn >> 8) & 0xf;
        let bit4 = insn >> 4 & 1 != 0;
        let q = insn >> 6 & 1 != 0;
        let (d, n, m) = (vd(insn), vn(insn), vm(insn));
        if q && (d | n | m) & 1 != 0 {
            return Err(Fail::Undefined);
        }
        let regs = if q { 2 } else { 1 };
        let esize = 8 << size;
        let val = move |x: u64| if u { x as i128 } else { sext(x, esize) as i128 };
        let mask = ones(esize);
        let mut sat = false;
        if op >= 12 && !(op == 12 && !bit4) {
            return self.neon_three_same_float(insn, u, op, bit4, q, d, n, m);
        }
        let no64 = || if size == 3 { Err(Fail::Undefined) } else { Ok(()) };
        match (op, bit4) {
            (0, false) => {
                no64()?;
                self.lanes3(d, n, m, regs, esize, |a, b, _| ((val(a) + val(b)) >> 1) as u64)
            }
            (0, true) => self.lanes3(d, n, m, regs, esize, |a, b, _| saturate(val(a) + val(b), esize, u, &mut sat)),
            (1, false) => {
                no64()?;
                self.lanes3(d, n, m, regs, esize, |a, b, _| ((val(a) + val(b) + 1) >> 1) as u64)
            }
            (1, true) => self.lanes3(d, n, m, regs, 64, |a, b, dd| match (u, size) {
                (false, 0) => a & b,
                (false, 1) => a & !b,
                (false, 2) => a | b,
                (false, _) => a | !b,
                (true, 0) => a ^ b,
                (true, 1) => dd & a | !dd & b,
                (true, 2) => dd & !b | a & b,
                (true, _) => dd & b | a & !b,
            }),
            (2, false) => {
                no64()?;
                self.lanes3(d, n, m, regs, esize, |a, b, _| ((val(a) - val(b)) >> 1) as u64)
            }
            (2, true) => self.lanes3(d, n, m, regs, esize, |a, b, _| saturate(val(a) - val(b), esize, u, &mut sat)),
            (3, _) => {
                no64()?;
                self.lanes3(d, n, m, regs, esize, |a, b, _| {
                    let r = if bit4 { val(a) >= val(b) } else { val(a) > val(b) };
                    if r { mask } else { 0 }
                })
            }
            (4 | 5, _) => {
                // VSHL/VQSHL/VRSHL/VQRSHL (register): Vm shifted by the low byte of Vn.
                let round = op == 5;
                self.lanes3(d, n, m, regs, esize, |shift, value, _| {
                    let r = shift_signed(val(value), sext(shift & 0xff, 8) as i32, round);
                    if bit4 { saturate(r, esize, u, &mut sat) } else { r as u64 }
                })
            }
            (6, _) => {
                no64()?;
                self.lanes3(d, n, m, regs, esize, |a, b, _| {
                    let pick_a = if bit4 { val(a) <= val(b) } else { val(a) >= val(b) };
                    if pick_a { a } else { b }
                })
            }
            (7, _) => {
                no64()?;
                self.lanes3(d, n, m, regs, esize, |a, b, dd| {
                    let diff = (val(a) - val(b)).unsigned_abs() as u64;
                    if bit4 { dd.wrapping_add(diff) } else { diff }
                })
            }
            (8, false) => self.lanes3(d, n, m, regs, esize, |a, b, _| if u { a.wrapping_sub(b) } else { a.wrapping_add(b) }),
            (8, true) => {
                no64()?;
                self.lanes3(d, n, m, regs, esize, |a, b, _| {
                    let r = if u { a == b } else { a & b != 0 };
                    if r { mask } else { 0 }
                })
            }
            (9, false) => {
                no64()?;
                self.lanes3(d, n, m, regs, esize, |a, b, dd| {
                    let p = a.wrapping_mul(b);
                    if u { dd.wrapping_sub(p) } else { dd.wrapping_add(p) }
                })
            }
            (9, true) => {
                no64()?;
                if u && size != 0 {
                    return Err(Fail::Undefined);
                }
                self.lanes3(d, n, m, regs, esize, |a, b, _| if u { poly_mul(a, b) } else { a.wrapping_mul(b) })
            }
            (10, _) => {
                no64()?;
                if q {
                    return Err(Fail::Undefined);
                }
                self.pairwise(d, n, m, esize, |a, b| {
                    let pick_a = if bit4 { val(a) <= val(b) } else { val(a) >= val(b) };
                    if pick_a { a } else { b }
                })
            }
            (11, false) => {
                if size == 0 || size == 3 {
                    return Err(Fail::Undefined);
                }
                let s = move |x: u64| sext(x, esize) as i128;
                self.lanes3(d, n, m, regs, esize, |a, b, _| {
                    let round = if u { 1i128 << (esize - 1) } else { 0 };
                    saturate((2 * s(a) * s(b) + round) >> esize, esize, false, &mut sat)
                })
            }
            (11, true) => {
                if u || q || size == 3 {
                    return Err(Fail::Undefined);
                }
                self.pairwise(d, n, m, esize, |a, b| a.wrapping_add(b))
            }
            _ => return Err(Fail::Undefined),
        }
        self.set_qc(sat);
        Ok(())
    }

    #[allow(clippy::too_many_arguments)]
    fn neon_three_same_float(
        &mut self,
        insn: u32,
        u: bool,
        op: u32,
        bit4: bool,
        q: bool,
        d: usize,
        n: usize,
        m: usize,
    ) -> R {
        if insn & (1 << 20) != 0 {
            return Err(Fail::Undefined); // half precision is ARMv8.2
        }
        let regs = if q { 2 } else { 1 };
        let alt = insn & (1 << 21) != 0;
        let f = |a: u64| a as u32;
        match (op, bit4, u, alt) {
            (12, true, false, _) => self.lanes3(d, n, m, regs, 32, |a, b, dd| {
                let a = if alt { fp_neg(f(a)) } else { f(a) };
                fp_mul_add(f(dd), a, f(b)) as u64
            }),
            (13, false, false, false) => self.lanes3(d, n, m, regs, 32, |a, b, _| fp_add(f(a), f(b)) as u64),
            (13, false, false, true) => self.lanes3(d, n, m, regs, 32, |a, b, _| fp_sub(f(a), f(b)) as u64),
            (13, false, true, false) => {
                if q {
                    return Err(Fail::Undefined);
                }
                self.pairwise(d, n, m, 32, |a, b| fp_add(f(a), f(b)) as u64)
            }
            (13, false, true, true) => {
                self.lanes3(d, n, m, regs, 32, |a, b, _| (fp_sub(f(a), f(b)) & 0x7fff_ffff) as u64)
            }
            (13, true, false, _) => self.lanes3(d, n, m, regs, 32, |a, b, dd| {
                let product = fp_mul(f(a), f(b));
                if alt { fp_sub(f(dd), product) as u64 } else { fp_add(f(dd), product) as u64 }
            }),
            (13, true, true, false) => self.lanes3(d, n, m, regs, 32, |a, b, _| fp_mul(f(a), f(b)) as u64),
            (14, false, false, false) => {
                self.lanes3(d, n, m, regs, 32, |a, b, _| if fp_compare(f(a), f(b), 0) { 0xffff_ffff } else { 0 })
            }
            (14, false, true, _) => {
                let op = if alt { 2 } else { 1 };
                self.lanes3(d, n, m, regs, 32, |a, b, _| if fp_compare(f(a), f(b), op) { 0xffff_ffff } else { 0 })
            }
            (14, true, true, _) => {
                let op = if alt { 2 } else { 1 };
                self.lanes3(d, n, m, regs, 32, |a, b, _| {
                    let (a, b) = (flush(f(a)) & 0x7fff_ffff, flush(f(b)) & 0x7fff_ffff);
                    if fp_compare(a, b, op) { 0xffff_ffff } else { 0 }
                })
            }
            (15, false, false, _) => self.lanes3(d, n, m, regs, 32, |a, b, _| fp_max(f(a), f(b), !alt) as u64),
            (15, false, true, _) => {
                if q {
                    return Err(Fail::Undefined);
                }
                self.pairwise(d, n, m, 32, |a, b| fp_max(f(a), f(b), !alt) as u64)
            }
            (15, true, false, _) => self.lanes3(d, n, m, regs, 32, |a, b, _| {
                if alt { fp_rsqrt_step(f(a), f(b)) as u64 } else { fp_recip_step(f(a), f(b)) as u64 }
            }),
            _ => return Err(Fail::Undefined),
        }
        Ok(())
    }

    // ---- Three registers of different lengths ---------------------------------------

    fn neon_three_diff(&mut self, insn: u32) -> R {
        let u = insn >> 24 & 1 != 0;
        let size = (insn >> 20) & 3;
        let op = (insn >> 8) & 0xf;
        let (d, n, m) = (vd(insn), vn(insn), vm(insn));
        let esize = 8 << size;
        let wide = 2 * esize;
        let val = move |x: u64| if u { x as i128 } else { sext(x, esize) as i128 };
        let wval = move |x: u64| if u { x as i128 } else { sext(x, wide) as i128 };
        let lanes = (64 / esize) as usize;
        let mut sat = false;
        let mut out = [0u64; 4];
        match op {
            0..=3 | 5 | 7..=14 => {
                // Long (Qd) results: VADDL/W, VSUBL/W, VABAL, VABDL, VMLAL/VMLSL,
                // VQDMLAL/VQDMLSL, VMULL, VQDMULL, VMULL.P8.
                let widen_n = op <= 3 && op & 1 != 0;
                if d & 1 != 0 || widen_n && n & 1 != 0 {
                    return Err(Fail::Undefined);
                }
                match op {
                    9 | 11 | 13 if u || size == 0 => return Err(Fail::Undefined),
                    14 if u || size != 0 => return Err(Fail::Undefined),
                    _ => {}
                }
                let an = self.dregs(n, if widen_n { 2 } else { 1 });
                let bm = [self.vfp.d[m]];
                let acc = self.dregs(d, 2);
                for i in 0..lanes {
                    let x = if widen_n { wval(elem(&an, i, wide)) } else { val(elem(&an, i, esize)) };
                    let y = val(elem(&bm, i, esize));
                    let dd = elem(&acc, i, wide);
                    let r: u64 = match op {
                        0 | 1 => (x + y) as u64,
                        2 | 3 => (x - y) as u64,
                        5 => dd.wrapping_add((x - y).unsigned_abs() as u64),
                        7 => (x - y).unsigned_abs() as u64,
                        8 => dd.wrapping_add((x * y) as u64),
                        10 => dd.wrapping_sub((x * y) as u64),
                        9 | 11 => {
                            let product = saturate(2 * x * y, wide, false, &mut sat);
                            let p = sext(product, wide) as i128;
                            let acc = sext(dd, wide) as i128;
                            saturate(if op == 9 { acc + p } else { acc - p }, wide, false, &mut sat)
                        }
                        12 => (x * y) as u64,
                        13 => saturate(2 * x * y, wide, false, &mut sat),
                        _ => {
                            // VMULL.P8
                            let (a, b) = (elem(&an, i, 8), elem(&bm, i, 8));
                            poly_mul(a, b)
                        }
                    };
                    set_elem(&mut out, i, wide, r);
                }
                self.set_dregs(d, 2, &out);
            }
            4 | 6 => {
                // VADDHN/VRADDHN, VSUBHN/VRSUBHN: Dd = high half of Qn +/- Qm.
                if n & 1 != 0 || m & 1 != 0 {
                    return Err(Fail::Undefined);
                }
                let (an, bm) = (self.dregs(n, 2), self.dregs(m, 2));
                let round = if u { 1u64 << (esize - 1) } else { 0 };
                for i in 0..lanes {
                    let (x, y) = (elem(&an, i, wide), elem(&bm, i, wide));
                    let r = if op == 4 { x.wrapping_add(y) } else { x.wrapping_sub(y) }.wrapping_add(round);
                    set_elem(&mut out, i, esize, (r & ones(wide)) >> esize);
                }
                self.vfp.d[d] = out[0];
            }
            _ => return Err(Fail::Undefined),
        }
        self.set_qc(sat);
        Ok(())
    }

    // ---- Two registers and a scalar -------------------------------------------------

    fn neon_scalar(&mut self, insn: u32) -> R {
        let u = insn >> 24 & 1 != 0;
        let size = (insn >> 20) & 3;
        let op = (insn >> 8) & 0xf;
        let (d, n) = (vd(insn), vn(insn));
        let esize = 8 << size;
        let (m, index) = match size {
            1 => ((insn & 7) as usize, ((insn >> 4) & 2 | (insn >> 3) & 1) as usize),
            2 => ((insn & 0xf) as usize, ((insn >> 5) & 1) as usize),
            _ => return Err(Fail::Undefined),
        };
        let scalar = elem(&[self.vfp.d[m]], index, esize);
        let float = matches!(op, 1 | 5 | 9);
        if float && size != 2 {
            return Err(Fail::Undefined);
        }
        let mut sat = false;
        match op {
            0 | 1 | 4 | 5 | 8 | 9 | 12 | 13 => {
                // Q is bit 24 for these.
                let q = u;
                if q && (d | n) & 1 != 0 {
                    return Err(Fail::Undefined);
                }
                let regs = if q { 2 } else { 1 };
                let s = move |x: u64| sext(x, esize) as i128;
                match op {
                    0 | 4 => self.lanes3(d, n, n, regs, esize, |a, _, dd| {
                        let p = a.wrapping_mul(scalar);
                        if op == 4 { dd.wrapping_sub(p) } else { dd.wrapping_add(p) }
                    }),
                    1 | 5 => self.lanes3(d, n, n, regs, 32, |a, _, dd| {
                        let product = fp_mul(a as u32, scalar as u32);
                        if op == 5 { fp_sub(dd as u32, product) as u64 } else { fp_add(dd as u32, product) as u64 }
                    }),
                    8 => self.lanes3(d, n, n, regs, esize, |a, _, _| a.wrapping_mul(scalar)),
                    9 => self.lanes3(d, n, n, regs, 32, |a, _, _| fp_mul(a as u32, scalar as u32) as u64),
                    _ => self.lanes3(d, n, n, regs, esize, |a, _, _| {
                        let round = if op == 13 { 1i128 << (esize - 1) } else { 0 };
                        saturate((2 * s(a) * s(scalar) + round) >> esize, esize, false, &mut sat)
                    }),
                }
            }
            2 | 3 | 6 | 7 | 10 | 11 => {
                // Long: VMLAL/VMLSL, VQDMLAL/VQDMLSL, VMULL, VQDMULL by scalar.
                if d & 1 != 0 || u && matches!(op, 3 | 7 | 11) {
                    return Err(Fail::Undefined);
                }
                let wide = 2 * esize;
                let val = move |x: u64| if u { x as i128 } else { sext(x, esize) as i128 };
                let an = [self.vfp.d[n]];
                let acc = self.dregs(d, 2);
                let y = val(scalar);
                let mut out = [0u64; 4];
                for i in 0..(64 / esize) as usize {
                    let x = val(elem(&an, i, esize));
                    let dd = elem(&acc, i, wide);
                    let r = match op {
                        2 => dd.wrapping_add((x * y) as u64),
                        6 => dd.wrapping_sub((x * y) as u64),
                        10 => (x * y) as u64,
                        11 => saturate(2 * x * y, wide, false, &mut sat),
                        _ => {
                            let product = sext(saturate(2 * x * y, wide, false, &mut sat), wide) as i128;
                            let acc = sext(dd, wide) as i128;
                            saturate(if op == 3 { acc + product } else { acc - product }, wide, false, &mut sat)
                        }
                    };
                    set_elem(&mut out, i, wide, r);
                }
                self.set_dregs(d, 2, &out);
            }
            _ => return Err(Fail::Undefined),
        }
        self.set_qc(sat);
        Ok(())
    }

    // ---- Two registers and a shift amount -------------------------------------------

    fn neon_shift_imm(&mut self, insn: u32) -> R {
        let u = insn >> 24 & 1 != 0;
        let op = (insn >> 8) & 0xf;
        let l = insn >> 7 & 1;
        let bit6 = insn >> 6 & 1 != 0;
        let imm6 = (insn >> 16) & 0x3f;
        let limm = l << 6 | imm6;
        let esize = if l != 0 {
            64
        } else if imm6 & 0x20 != 0 {
            32
        } else if imm6 & 0x10 != 0 {
            16
        } else {
            8
        };
        let right = 2 * esize - limm; // 1..=esize
        let left = limm - esize; // 0..esize-1
        let (d, m) = (vd(insn), vm(insn));
        let val = move |x: u64| if u { x as i128 } else { sext(x, esize) as i128 };
        let mut sat = false;
        let mask = ones(esize);
        match op {
            0..=7 => {
                let q = bit6;
                if q && (d | m) & 1 != 0 {
                    return Err(Fail::Undefined);
                }
                let regs = if q { 2 } else { 1 };
                match op {
                    0..=3 => self.lanes3(d, m, m, regs, esize, |x, _, dd| {
                        let r = shift_signed(val(x), -(right as i32), op & 2 != 0) as u64;
                        if op & 1 != 0 { dd.wrapping_add(r) } else { r }
                    }),
                    4 => {
                        if !u {
                            return Err(Fail::Undefined);
                        }
                        // VSRI
                        let keep = if right == 64 { u64::MAX } else { !(mask >> right) & mask };
                        self.lanes3(d, m, m, regs, esize, |x, _, dd| {
                            let shifted = if right == 64 { 0 } else { x >> right };
                            dd & keep | shifted
                        })
                    }
                    5 => {
                        // VSHL (immediate) / VSLI
                        let keep = !(mask << left) & mask;
                        self.lanes3(d, m, m, regs, esize, |x, _, dd| {
                            let shifted = x << left & mask;
                            if u { dd & keep | shifted } else { shifted }
                        })
                    }
                    6 | 7 => {
                        // VQSHLU (U=1, op=6) and VQSHL signed/unsigned (op=7)
                        if !u && op == 6 {
                            return Err(Fail::Undefined);
                        }
                        let signed_in = op == 6 || !u;
                        let unsigned_out = op == 6 || u;
                        self.lanes3(d, m, m, regs, esize, |x, _, _| {
                            let v = if signed_in { sext(x, esize) as i128 } else { x as i128 };
                            saturate(v << left, esize, unsigned_out, &mut sat)
                        })
                    }
                    _ => unreachable!(),
                }
            }
            8 | 9 => {
                // Narrowing right shifts: Dd = Qm >> shift (esize is the result size).
                if l != 0 || m & 1 != 0 {
                    return Err(Fail::Undefined);
                }
                let wide = 2 * esize;
                let src = self.dregs(m, 2);
                let mut out = [0u64; 4];
                let round = bit6;
                for i in 0..(64 / esize) as usize {
                    let x = elem(&src, i, wide);
                    let r = match (op, u) {
                        (8, false) => shift_signed(x as i128, -(right as i32), round) as u64 & mask,
                        (8, true) => {
                            let v = shift_signed(sext(x, wide) as i128, -(right as i32), round);
                            saturate(v, esize, true, &mut sat)
                        }
                        _ => {
                            let v = if u { x as i128 } else { sext(x, wide) as i128 };
                            saturate(shift_signed(v, -(right as i32), round), esize, u, &mut sat)
                        }
                    };
                    set_elem(&mut out, i, esize, r);
                }
                self.vfp.d[d] = out[0];
            }
            10 => {
                // VSHLL (VMOVL when the shift is zero).
                if l != 0 || bit6 || d & 1 != 0 {
                    return Err(Fail::Undefined);
                }
                let wide = 2 * esize;
                let src = [self.vfp.d[m]];
                let mut out = [0u64; 4];
                for i in 0..(64 / esize) as usize {
                    set_elem(&mut out, i, wide, (val(elem(&src, i, esize)) << left) as u64);
                }
                self.set_dregs(d, 2, &out);
            }
            14 | 15 => {
                // VCVT between floating point and fixed point.
                if l != 0 || imm6 & 0x20 == 0 {
                    return Err(Fail::Undefined);
                }
                let q = bit6;
                if q && (d | m) & 1 != 0 {
                    return Err(Fail::Undefined);
                }
                let fraction_bits = 64 - imm6;
                let to_fixed = op == 15;
                self.lanes3(d, m, m, if q { 2 } else { 1 }, 32, |x, _, _| {
                    let x = x as u32;
                    (if to_fixed { fp_to_fixed(x, fraction_bits, u) } else { fixed_to_fp(x, fraction_bits, u) }) as u64
                });
            }
            _ => return Err(Fail::Undefined),
        }
        self.set_qc(sat);
        Ok(())
    }

    // ---- One register and a modified immediate --------------------------------------

    fn neon_modified_imm(&mut self, insn: u32) -> R {
        let q = insn >> 6 & 1 != 0;
        let op = insn >> 5 & 1;
        let cmode = (insn >> 8) & 0xf;
        let imm8 = (insn >> 17) & 0x80 | (insn >> 12) & 0x70 | insn & 0xf;
        let d = vd(insn);
        if q && d & 1 != 0 {
            return Err(Fail::Undefined);
        }
        let imm = expand_imm(op, cmode, imm8).ok_or(Fail::Undefined)?;
        let orr_bic = cmode & 1 != 0 && cmode < 12;
        for r in 0..if q { 2 } else { 1 } {
            let reg = &mut self.vfp.d[d + r];
            *reg = match (orr_bic, op) {
                (true, 0) => *reg | imm,
                (true, _) => *reg & !imm,
                (false, 0) => imm,
                (false, _) if cmode == 14 => imm,
                (false, _) => !imm,
            };
        }
        Ok(())
    }

    // ---- Two registers, miscellaneous ----------------------------------------------

    fn neon_misc(&mut self, insn: u32) -> R {
        let size = (insn >> 18) & 3;
        let a = (insn >> 16) & 3;
        let b = (insn >> 6) & 0x1f;
        let q = insn >> 6 & 1 != 0;
        let (d, m) = (vd(insn), vm(insn));
        let esize = 8 << size;
        let mask = ones(esize);
        let regs = if q { 2 } else { 1 };
        let mut sat = false;
        let qcheck = |extra: bool| {
            if q && (d | m) & 1 != 0 || extra { Err(Fail::Undefined) } else { Ok(()) }
        };
        match (a, b >> 1) {
            (0, 0..=2) => {
                // VREV64/VREV32/VREV16: reverse esize-bit lanes within each container.
                let op = b >> 1;
                qcheck(op + size >= 3)?;
                let container = 64 >> op;
                let per = (container / esize) as usize;
                let src = self.dregs(m, regs);
                let mut out = [0u64; 4];
                for i in 0..regs * (64 / esize) as usize {
                    let base = i / per * per;
                    let j = base + (per - 1 - (i - base));
                    set_elem(&mut out, j, esize, elem(&src, i, esize));
                }
                self.set_dregs(d, regs, &out);
            }
            (0, 4 | 5 | 12 | 13) => {
                // VPADDL / VPADAL
                qcheck(size == 3)?;
                let unsigned = b & 2 != 0;
                let accumulate = b >> 1 >= 12;
                let wide = 2 * esize;
                let val = move |x: u64| if unsigned { x as i128 } else { sext(x, esize) as i128 };
                let (src, acc) = (self.dregs(m, regs), self.dregs(d, regs));
                let mut out = [0u64; 4];
                for i in 0..regs * (32 / esize) as usize {
                    let sum = (val(elem(&src, 2 * i, esize)) + val(elem(&src, 2 * i + 1, esize))) as u64;
                    let r = if accumulate { elem(&acc, i, wide).wrapping_add(sum) } else { sum };
                    set_elem(&mut out, i, wide, r);
                }
                self.set_dregs(d, regs, &out);
            }
            (0, 8) => {
                qcheck(size == 3)?;
                self.lanes3(d, m, m, regs, esize, |x, _, _| {
                    let s = sext(x, esize) as u64 & mask;
                    count_leading_zeros((s ^ s >> 1) & (mask >> 1), esize - 1)
                })
            }
            (0, 9) => {
                qcheck(size == 3)?;
                self.lanes3(d, m, m, regs, esize, |x, _, _| count_leading_zeros(x, esize))
            }
            (0, 10) => {
                qcheck(size != 0)?;
                self.lanes3(d, m, m, regs, 8, |x, _, _| x.count_ones() as u64)
            }
            (0, 11) => {
                qcheck(size != 0)?;
                self.lanes3(d, m, m, regs, 64, |x, _, _| !x)
            }
            (0, 14 | 15) => {
                // VQABS / VQNEG
                qcheck(size == 3)?;
                let negate = b >> 1 == 15;
                self.lanes3(d, m, m, regs, esize, |x, _, _| {
                    let v = sext(x, esize) as i128;
                    let r = if negate { -v } else { v.abs() };
                    saturate(r, esize, false, &mut sat)
                })
            }
            (1, op) if op & 7 != 5 => {
                // Compare with zero, VABS, VNEG (bit 10 selects floating point).
                let float = op & 8 != 0;
                qcheck(size == 3 || float && size != 2)?;
                let kind = op & 7;
                self.lanes3(d, m, m, regs, esize, |x, _, _| {
                    if kind == 6 {
                        return if float {
                            x & 0x7fff_ffff
                        } else {
                            (sext(x, esize) as i128).unsigned_abs() as u64
                        };
                    }
                    if kind == 7 {
                        return if float { x ^ 0x8000_0000 } else { x.wrapping_neg() };
                    }
                    let r = if float {
                        let x = x as u32;
                        match kind {
                            0 => fp_compare(x, 0, 2),
                            1 => fp_compare(x, 0, 1),
                            2 => fp_compare(x, 0, 0),
                            3 => fp_compare(0, x, 1),
                            _ => fp_compare(0, x, 2),
                        }
                    } else {
                        let v = sext(x, esize);
                        match kind {
                            0 => v > 0,
                            1 => v >= 0,
                            2 => v == 0,
                            3 => v <= 0,
                            _ => v < 0,
                        }
                    };
                    if r { mask } else { 0 }
                })
            }
            (2, 0) => {
                // VSWP
                qcheck(size != 0)?;
                let (x, y) = (self.dregs(d, regs), self.dregs(m, regs));
                self.set_dregs(d, regs, &y);
                self.set_dregs(m, regs, &x);
            }
            (2, 1..=3) => {
                let op = b >> 1;
                qcheck(size == 3 || op != 1 && !q && size == 2)?;
                let (x, y) = (self.dregs(d, regs), self.dregs(m, regs));
                let lanes = regs * (64 / esize) as usize;
                let (mut nd, mut nm) = (x, y);
                match op {
                    1 => {
                        // VTRN
                        for i in (0..lanes).step_by(2) {
                            set_elem(&mut nd, i + 1, esize, elem(&y, i, esize));
                            set_elem(&mut nm, i, esize, elem(&x, i + 1, esize));
                        }
                    }
                    2 => {
                        // VUZP: Vd gets the even lanes of Vd:Vm, Vm the odd ones.
                        for i in 0..lanes {
                            let pick = |k: usize| if k < lanes { elem(&x, k, esize) } else { elem(&y, k - lanes, esize) };
                            set_elem(&mut nd, i, esize, pick(2 * i));
                            set_elem(&mut nm, i, esize, pick(2 * i + 1));
                        }
                    }
                    _ => {
                        // VZIP: interleave Vd and Vm; low half to Vd, high half to Vm.
                        for i in 0..lanes {
                            let k = 2 * i;
                            let (lo, hi) = (elem(&x, i, esize), elem(&y, i, esize));
                            for (slot, value) in [(k, lo), (k + 1, hi)] {
                                if slot < lanes {
                                    set_elem(&mut nd, slot, esize, value);
                                } else {
                                    set_elem(&mut nm, slot - lanes, esize, value);
                                }
                            }
                        }
                    }
                }
                self.set_dregs(d, regs, &nd);
                self.set_dregs(m, regs, &nm);
            }
            (2, 4) => {
                // VMOVN (b=01000), VQMOVUN (01001)
                if size == 3 || m & 1 != 0 {
                    return Err(Fail::Undefined);
                }
                let src = self.dregs(m, 2);
                let wide = 2 * esize;
                let mut out = [0u64; 4];
                for i in 0..(64 / esize) as usize {
                    let x = elem(&src, i, wide);
                    let r = if b & 1 == 0 { x } else { saturate(sext(x, wide) as i128, esize, true, &mut sat) };
                    set_elem(&mut out, i, esize, r);
                }
                self.vfp.d[d] = out[0];
            }
            (2, 5) => {
                // VQMOVN signed (b=01010) / unsigned (01011)
                if size == 3 || m & 1 != 0 {
                    return Err(Fail::Undefined);
                }
                let unsigned = b & 1 != 0;
                let src = self.dregs(m, 2);
                let wide = 2 * esize;
                let mut out = [0u64; 4];
                for i in 0..(64 / esize) as usize {
                    let x = elem(&src, i, wide);
                    let v = if unsigned { x as i128 } else { sext(x, wide) as i128 };
                    set_elem(&mut out, i, esize, saturate(v, esize, unsigned, &mut sat));
                }
                self.vfp.d[d] = out[0];
            }
            (2, 6) if b == 0x0c => {
                // VSHLL #esize
                if size == 3 || d & 1 != 0 {
                    return Err(Fail::Undefined);
                }
                let wide = 2 * esize;
                let src = [self.vfp.d[m]];
                let mut out = [0u64; 4];
                for i in 0..(64 / esize) as usize {
                    set_elem(&mut out, i, wide, elem(&src, i, esize) << esize);
                }
                self.set_dregs(d, 2, &out);
            }
            (2, 12) if b & 1 == 0 => {
                // VCVT.F16.F32 Dd, Qm
                if size != 1 || m & 1 != 0 {
                    return Err(Fail::Undefined);
                }
                let src = self.dregs(m, 2);
                let mut out = [0u64];
                for i in 0..4 {
                    set_elem(&mut out, i, 16, f32_to_f16(elem(&src, i, 32) as u32, 0, self.vfp.fpscr & (1 << 26) != 0, true, true) as u64);
                }
                self.vfp.d[d] = out[0];
            }
            (2, 14) if b & 1 == 0 => {
                // VCVT.F32.F16 Qd, Dm
                if size != 1 || d & 1 != 0 {
                    return Err(Fail::Undefined);
                }
                let src = [self.vfp.d[m]];
                let mut out = [0u64; 4];
                for i in 0..4 {
                    set_elem(&mut out, i, 32, f16_to_f32(elem(&src, i, 16) as u16, self.vfp.fpscr & (1 << 26) != 0, true) as u64);
                }
                self.set_dregs(d, 2, &out);
            }
            (3, 8..=11) => {
                // VRECPE / VRSQRTE (bit 8: floating point)
                qcheck(size != 2)?;
                let float = b & 4 != 0;
                let rsqrt = b & 2 != 0;
                self.lanes3(d, m, m, regs, 32, |x, _, _| {
                    let x = x as u32;
                    (match (float, rsqrt) {
                        (false, false) => unsigned_recip_estimate(x),
                        (false, true) => unsigned_rsqrt_estimate(x),
                        (true, false) => fp_recip_estimate(x),
                        (true, true) => fp_rsqrt_estimate(x),
                    }) as u64
                })
            }
            (3, 12..=15) => {
                // VCVT between floating point and integer (round toward zero).
                qcheck(size != 2)?;
                let op = (b >> 1) & 3;
                let unsigned = op & 1 != 0;
                self.lanes3(d, m, m, regs, 32, |x, _, _| {
                    let x = x as u32;
                    (if op & 2 != 0 { fp_to_fixed(x, 0, unsigned) } else { fixed_to_fp(x, 0, unsigned) }) as u64
                })
            }
            _ => return Err(Fail::Undefined),
        }
        self.set_qc(sat);
        Ok(())
    }

    // ---- VEXT, VTBL/VTBX, VDUP (scalar) ---------------------------------------------

    fn neon_ext(&mut self, insn: u32) -> R {
        let q = insn >> 6 & 1 != 0;
        let imm4 = ((insn >> 8) & 0xf) as usize;
        let (d, n, m) = (vd(insn), vn(insn), vm(insn));
        if q && (d | n | m) & 1 != 0 || !q && imm4 > 7 {
            return Err(Fail::Undefined);
        }
        let regs = if q { 2 } else { 1 };
        let bytes = 8 * regs;
        let (x, y) = (self.dregs(n, regs), self.dregs(m, regs));
        let mut out = [0u64; 4];
        for i in 0..bytes {
            let k = i + imm4;
            let value = if k < bytes { elem(&x, k, 8) } else { elem(&y, k - bytes, 8) };
            set_elem(&mut out, i, 8, value);
        }
        self.set_dregs(d, regs, &out);
        Ok(())
    }

    fn neon_table(&mut self, insn: u32) -> R {
        let (d, n, m) = (vd(insn), vn(insn), vm(insn));
        let length = ((insn >> 8) & 3) as usize + 1;
        let extension = insn & (1 << 6) != 0;
        if n + length > 32 {
            return Err(Fail::Undefined);
        }
        let table = self.dregs(n, length);
        let index = [self.vfp.d[m]];
        let mut out = [self.vfp.d[d]];
        for i in 0..8 {
            let k = elem(&index, i, 8) as usize;
            if k < 8 * length {
                set_elem(&mut out, i, 8, elem(&table, k, 8));
            } else if !extension {
                set_elem(&mut out, i, 8, 0);
            }
        }
        self.vfp.d[d] = out[0];
        Ok(())
    }

    fn neon_dup_scalar(&mut self, insn: u32) -> R {
        let imm4 = (insn >> 16) & 0xf;
        let q = insn >> 6 & 1 != 0;
        let (d, m) = (vd(insn), vm(insn));
        let (esize, index) = if imm4 & 1 != 0 {
            (8, imm4 >> 1)
        } else if imm4 & 2 != 0 {
            (16, imm4 >> 2)
        } else if imm4 & 4 != 0 {
            (32, imm4 >> 3)
        } else {
            return Err(Fail::Undefined);
        };
        if q && d & 1 != 0 {
            return Err(Fail::Undefined);
        }
        let value = replicate(elem(&[self.vfp.d[m]], index as usize, esize), esize);
        for r in 0..if q { 2 } else { 1 } {
            self.vfp.d[d + r] = value;
        }
        Ok(())
    }

    // ---- Element and structure loads and stores -------------------------------------

    /// Read `bytes.len()` bytes in `unit`-byte accesses (1, 2 or 4).
    fn neon_read(&mut self, bus: &mut dyn Bus, va: u32, bytes: &mut [u8], unit: usize) -> R {
        for offset in (0..bytes.len()).step_by(unit) {
            let address = va.wrapping_add(offset as u32);
            let value = match unit {
                4 => self.mem_read32(bus, address, Access::Read),
                2 => self.mem_read16(bus, address).map(u32::from),
                _ => self.mem_read8(bus, address, Access::Read).map(u32::from),
            };
            let value = value.map_err(|(va, status)| Fail::Trap(self.data_abort(va, status, false)))?;
            bytes[offset..offset + unit].copy_from_slice(&value.to_le_bytes()[..unit]);
        }
        Ok(())
    }

    fn neon_write(&mut self, bus: &mut dyn Bus, va: u32, bytes: &[u8], unit: usize) -> R {
        for offset in (0..bytes.len()).step_by(unit) {
            let address = va.wrapping_add(offset as u32);
            let chunk = &bytes[offset..offset + unit];
            let result = match unit {
                4 => self.mem_write32(bus, address, u32::from_le_bytes(chunk.try_into().unwrap())),
                2 => self.mem_write16(bus, address, u16::from_le_bytes([chunk[0], chunk[1]])),
                _ => self.mem_write8(bus, address, chunk[0]),
            };
            result.map_err(|(va, status)| Fail::Trap(self.data_abort(va, status, true)))?;
        }
        Ok(())
    }

    fn neon_load_store(&mut self, bus: &mut dyn Bus, insn: u32) -> R {
        let single = insn & (1 << 23) != 0;
        let load = insn & (1 << 21) != 0;
        let rn = ((insn >> 16) & 0xf) as usize;
        let rm = (insn & 0xf) as usize;
        let d = vd(insn);
        let kind = (insn >> 8) & 0xf;
        if rn == 15 {
            return Err(Fail::Undefined);
        }
        let address = self.r[rn];
        // (registers in order of the memory layout, element size, structure size,
        //  alignment, transfer length in bytes)
        let undefined = Err(Fail::Undefined);
        let transfer: usize;
        if !single {
            let size = (insn >> 6) & 3;
            let align = (insn >> 4) & 3;
            let (structure, regs, inc) = match kind {
                7 => (1, 1, 1),
                10 => (1, 2, 1),
                6 => (1, 3, 1),
                2 => (1, 4, 1),
                8 => (2, 1, 1),
                9 => (2, 1, 2),
                3 => (2, 2, 2),
                4 => (3, 1, 1),
                5 => (3, 1, 2),
                0 => (4, 1, 1),
                1 => (4, 1, 2),
                _ => return undefined,
            };
            let bad_align = match (structure, regs) {
                (1, 1) | (1, 3) | (3, _) => align & 2 != 0,
                (1, 2) | (2, 1) => align == 3,
                _ => false,
            };
            if bad_align || structure > 1 && size == 3 {
                return undefined;
            }
            let ebytes = 1usize << size;
            let elements = 8 / ebytes;
            // Register of structure member `s`, register group `r`.
            let reg = |r: usize, s: usize| d + r + s * inc;
            let last = if structure == 1 { d + regs - 1 } else { reg(regs - 1, structure - 1) };
            if last > 31 {
                return undefined;
            }
            let alignment = if align == 0 { 1 } else { 4 << align };
            if !address.is_multiple_of(alignment) {
                return Err(Fail::Trap(self.data_abort(address, 0x01, !load)));
            }
            transfer = 8 * regs * structure;
            let mut buffer = [0u8; 32];
            let buffer = &mut buffer[..transfer];
            // Memory position of each register byte: element e of member s of group r is at
            // ((r * elements + e) * structure + s) * ebytes.
            let place = |r: usize, s: usize, e: usize| ((r * elements + e) * structure + s) * ebytes;
            if load {
                // Element-sized accesses, as the hardware issues them (device memory).
                self.neon_read(bus, address, buffer, ebytes.min(4))?;
                for r in 0..regs {
                    for s in 0..structure {
                        let mut value = [0u8; 8];
                        for e in 0..elements {
                            let at = place(r, s, e);
                            value[e * ebytes..(e + 1) * ebytes].copy_from_slice(&buffer[at..at + ebytes]);
                        }
                        self.vfp.d[reg(r, s)] = u64::from_le_bytes(value);
                    }
                }
            } else {
                for r in 0..regs {
                    for s in 0..structure {
                        let value = self.vfp.d[reg(r, s)].to_le_bytes();
                        for e in 0..elements {
                            let at = place(r, s, e);
                            buffer[at..at + ebytes].copy_from_slice(&value[e * ebytes..(e + 1) * ebytes]);
                        }
                    }
                }
                self.neon_write(bus, address, buffer, ebytes.min(4))?;
            }
        } else {
            let size = (insn >> 10) & 3;
            let structure = ((insn >> 8) & 3) as usize + 1;
            if size == 3 {
                // Single element to all lanes (loads only).
                if !load {
                    return undefined;
                }
                let size = (insn >> 6) & 3;
                let t = insn & (1 << 5) != 0;
                let a = insn & (1 << 4) != 0;
                let (ebytes, alignment, regs, inc) = match structure {
                    1 => {
                        if size == 3 || size == 0 && a {
                            return undefined;
                        }
                        let ebytes = 1usize << size;
                        (ebytes, if a { ebytes } else { 1 }, if t { 2 } else { 1 }, 1)
                    }
                    2 => {
                        if size == 3 {
                            return undefined;
                        }
                        let ebytes = 1usize << size;
                        (ebytes, if a { 2 * ebytes } else { 1 }, 1, if t { 2 } else { 1 })
                    }
                    3 => {
                        if size == 3 || a {
                            return undefined;
                        }
                        (1usize << size, 1, 1, if t { 2 } else { 1 })
                    }
                    _ => {
                        if size == 3 && !a {
                            return undefined;
                        }
                        let (ebytes, alignment) = match size {
                            0 => (1, 4),
                            1 => (2, 8),
                            2 => (4, 8),
                            _ => (4, 16),
                        };
                        (ebytes, if a { alignment } else { 1 }, 1, if t { 2 } else { 1 })
                    }
                };
                let last = d + (regs - 1) + (structure - 1) * inc;
                if last > 31 {
                    return undefined;
                }
                if !(address as usize).is_multiple_of(alignment) {
                    return Err(Fail::Trap(self.data_abort(address, 0x01, false)));
                }
                transfer = ebytes * structure;
                let mut buffer = [0u8; 16];
                self.neon_read(bus, address, &mut buffer[..transfer], ebytes.min(4))?;
                let esize = 8 * ebytes as u32;
                for s in 0..structure {
                    let mut value = [0u8; 8];
                    value[..ebytes].copy_from_slice(&buffer[s * ebytes..(s + 1) * ebytes]);
                    let lane = replicate(u64::from_le_bytes(value), esize);
                    for r in 0..regs {
                        self.vfp.d[d + r + s * inc] = lane;
                    }
                }
            } else {
                // Single element to or from one lane.
                let ia = (insn >> 4) & 0xf;
                let ebytes = 1usize << size;
                let (index, inc, alignment) = match (structure, size) {
                    (1, 0) => {
                        if ia & 1 != 0 {
                            return undefined;
                        }
                        (ia >> 1, 1, 1)
                    }
                    (1, 1) => {
                        if ia & 2 != 0 {
                            return undefined;
                        }
                        (ia >> 2, 1, if ia & 1 != 0 { 2 } else { 1 })
                    }
                    (1, _) => {
                        if ia & 4 != 0 || ia & 3 == 1 || ia & 3 == 2 {
                            return undefined;
                        }
                        (ia >> 3, 1, if ia & 3 == 3 { 4 } else { 1 })
                    }
                    (2, 0) => (ia >> 1, 1, if ia & 1 != 0 { 2 } else { 1 }),
                    (2, 1) => (ia >> 2, if ia & 2 != 0 { 2 } else { 1 }, if ia & 1 != 0 { 4 } else { 1 }),
                    (2, _) => {
                        if ia & 2 != 0 {
                            return undefined;
                        }
                        (ia >> 3, if ia & 4 != 0 { 2 } else { 1 }, if ia & 1 != 0 { 8 } else { 1 })
                    }
                    (3, 0) => {
                        if ia & 1 != 0 {
                            return undefined;
                        }
                        (ia >> 1, 1, 1)
                    }
                    (3, 1) => {
                        if ia & 1 != 0 {
                            return undefined;
                        }
                        (ia >> 2, if ia & 2 != 0 { 2 } else { 1 }, 1)
                    }
                    (3, _) => {
                        if ia & 3 != 0 {
                            return undefined;
                        }
                        (ia >> 3, if ia & 4 != 0 { 2 } else { 1 }, 1)
                    }
                    (_, 0) => (ia >> 1, 1, if ia & 1 != 0 { 4 } else { 1 }),
                    (_, 1) => (ia >> 2, if ia & 2 != 0 { 2 } else { 1 }, if ia & 1 != 0 { 8 } else { 1 }),
                    (_, _) => {
                        if ia & 3 == 3 {
                            return undefined;
                        }
                        let alignment = if ia & 3 == 0 { 1 } else { 4 << (ia & 3) };
                        (ia >> 3, if ia & 4 != 0 { 2 } else { 1 }, alignment)
                    }
                };
                let last = d + (structure - 1) * inc;
                if last > 31 {
                    return undefined;
                }
                if !(address as usize).is_multiple_of(alignment) {
                    return Err(Fail::Trap(self.data_abort(address, 0x01, !load)));
                }
                transfer = ebytes * structure;
                let esize = 8 * ebytes as u32;
                let mut buffer = [0u8; 16];
                if load {
                    self.neon_read(bus, address, &mut buffer[..transfer], ebytes.min(4))?;
                    for s in 0..structure {
                        let mut value = [0u8; 8];
                        value[..ebytes].copy_from_slice(&buffer[s * ebytes..(s + 1) * ebytes]);
                        let reg = d + s * inc;
                        let mut v = [self.vfp.d[reg]];
                        set_elem(&mut v, index as usize, esize, u64::from_le_bytes(value));
                        self.vfp.d[reg] = v[0];
                    }
                } else {
                    for s in 0..structure {
                        let value = elem(&[self.vfp.d[d + s * inc]], index as usize, esize).to_le_bytes();
                        buffer[s * ebytes..(s + 1) * ebytes].copy_from_slice(&value[..ebytes]);
                    }
                    self.neon_write(bus, address, &buffer[..transfer], ebytes.min(4))?;
                }
            }
        }
        match rm {
            15 => {}
            13 => self.r[rn] = address.wrapping_add(transfer as u32),
            _ => self.r[rn] = address.wrapping_add(self.r[rm]),
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn half_precision_round_trips() {
        for h in 0..=0xffffu32 {
            let h = h as u16;
            let single = f16_to_f32(h, false, false);
            if h & 0x7c00 == 0x7c00 && h & 0x3ff != 0 {
                continue; // NaNs are quietened
            }
            assert_eq!(f32_to_f16(single, 0, false, false, false), h, "{h:#06x}");
        }
        assert_eq!(f32_to_f16(1.0f32.to_bits(), 0, false, false, false), 0x3c00);
        assert_eq!(f32_to_f16(65520.0f32.to_bits(), 0, false, false, false), 0x7c00);
        assert_eq!(f32_to_f16(65519.0f32.to_bits(), 0, false, false, false), 0x7bff);
        assert_eq!(f32_to_f16(65520.0f32.to_bits(), 3, false, false, false), 0x7bff);
        assert_eq!(f32_to_f16((2.0f32).powi(-25).to_bits(), 0, false, false, false), 0);
        assert_eq!(f32_to_f16((1.5f32 * (2.0f32).powi(-25)).to_bits(), 0, false, false, false), 1);
    }

    #[test]
    fn modified_immediates_expand() {
        assert_eq!(expand_imm(0, 0b1110, 0xab), Some(0xabab_abab_abab_abab));
        assert_eq!(expand_imm(1, 0b1110, 0x81), Some(0xff00_0000_0000_00ff));
        assert_eq!(expand_imm(0, 0b1111, 0x70), Some(0x3f80_0000_3f80_0000));
        assert_eq!(expand_imm(0, 0b1100, 0x12), Some(0x0000_12ff_0000_12ff));
        assert_eq!(expand_imm(1, 0b1111, 0x12), None);
    }
}
