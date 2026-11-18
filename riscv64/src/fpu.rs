//! F and D: single and double precision on the host's IEEE 754 arithmetic.
//!
//! Results are the host's round-to-nearest-even values, canonicalised to RISC-V's
//! canonical NaN. The other rounding modes and the exception flags are derived from the
//! sign of the exact error of each operation (TwoSum for add, an FMA residual for multiply,
//! divide and square root): a result rounded the wrong way for the requested mode moves
//! one ulp. Single-precision values are NaN-boxed in the 64-bit registers.
use crate::{mstatus, Bus, Exception, Hart, R};

const NX: u64 = 1;
const UF: u64 = 2;
const OF: u64 = 4;
const DZ: u64 = 8;
const NV: u64 = 16;

const RNE: u32 = 0;
const RTZ: u32 = 1;
const RDN: u32 = 2;
const RUP: u32 = 3;
const RMM: u32 = 4;

const BOX: u64 = 0xffff_ffff_0000_0000;
const CNAN32: u32 = 0x7fc0_0000;
const CNAN64: u64 = 0x7ff8_0000_0000_0000;

/// Operations common to binary32 and binary64.
#[allow(dead_code)]
trait Float: Copy + PartialOrd + std::ops::Neg<Output = Self> {
    const MAX: Self;
    const INF: Self;
    const MIN_POS: Self;
    fn is_nan(self) -> bool;
    fn is_snan(self) -> bool;
    fn is_inf(self) -> bool;
    fn zero(self) -> bool;
    fn neg_sign(self) -> bool;
    fn up(self) -> Self;
    fn down(self) -> Self;
    fn abs(self) -> Self;
}

macro_rules! float {
    ($t:ty, $bits:ty, $quiet:expr) => {
        impl Float for $t {
            const MAX: Self = <$t>::MAX;
            const INF: Self = <$t>::INFINITY;
            const MIN_POS: Self = <$t>::MIN_POSITIVE;
            fn is_nan(self) -> bool {
                <$t>::is_nan(self)
            }
            fn is_snan(self) -> bool {
                <$t>::is_nan(self) && self.to_bits() & $quiet == 0
            }
            fn is_inf(self) -> bool {
                <$t>::is_infinite(self)
            }
            fn zero(self) -> bool {
                self == 0.0
            }
            fn neg_sign(self) -> bool {
                self.is_sign_negative()
            }
            fn up(self) -> Self {
                self.next_up()
            }
            fn down(self) -> Self {
                self.next_down()
            }
            fn abs(self) -> Self {
                <$t>::abs(self)
            }
        }
    };
}
float!(f32, u32, 1u32 << 22);
float!(f64, u64, 1u64 << 51);

/// Round `r` (the round-to-nearest-even result) the way `rm` wants, given the sign of
/// `exact - r` (`err`).
fn adjust<F: Float>(r: F, err: i32, rm: u32) -> F {
    if err == 0 || r.is_nan() {
        return r;
    }
    match rm {
        RTZ => {
            if !r.neg_sign() && err < 0 || r.neg_sign() && err > 0 {
                if err < 0 { r.down() } else { r.up() }
            } else {
                r
            }
        }
        RDN => if err < 0 { r.down() } else { r },
        RUP => if err > 0 { r.up() } else { r },
        _ => r,
    }
}

fn sign<F: Float>(e: F) -> i32 {
    if e.is_nan() || e.zero() {
        0
    } else if e.neg_sign() {
        -1
    } else {
        1
    }
}

/// Exception flags for a result `r` with exact-error sign `err` (inputs finite or not).
fn flags<F: Float>(r: F, err: i32, finite_in: bool) -> u64 {
    let mut f = 0;
    if err != 0 {
        f |= NX;
    }
    if r.is_inf() && finite_in {
        f |= OF | NX;
    }
    f
}

/// The exact-error sign of `a + b = s` (TwoSum), for finite operands.
macro_rules! two_sum_err {
    ($a:expr, $b:expr, $s:expr) => {{
        let (a, b, s) = ($a, $b, $s);
        if s.is_infinite() {
            if s > 0.0 { -1 } else { 1 }
        } else {
            let bb = s - a;
            let e = (a - (s - bb)) + (b - bb);
            sign(e)
        }
    }};
}

impl Hart {
    #[inline(always)]
    fn f32_of(&self, r: usize) -> f32 {
        let v = self.f[r];
        if v & BOX == BOX { f32::from_bits(v as u32) } else { f32::from_bits(CNAN32) }
    }
    #[inline(always)]
    fn f64_of(&self, r: usize) -> f64 {
        f64::from_bits(self.f[r])
    }
    fn set_f32(&mut self, r: usize, v: f32) {
        let b = if v.is_nan() { CNAN32 } else { v.to_bits() };
        self.f[r] = BOX | b as u64;
        self.set_fs_dirty();
    }
    fn set_f64(&mut self, r: usize, v: f64) {
        self.f[r] = if v.is_nan() { CNAN64 } else { v.to_bits() };
        self.set_fs_dirty();
    }
    fn raise(&mut self, f: u64) {
        if f != 0 {
            self.fcsr |= f;
            self.set_fs_dirty();
        }
    }

    /// The effective rounding mode of instruction `i`, or illegal.
    fn rm(&self, i: u32) -> R<u32> {
        let rm = (i >> 12) & 7;
        let rm = if rm == 7 { ((self.fcsr >> 5) & 7) as u32 } else { rm };
        if rm > RMM { Err(Exception::illegal(i)) } else { Ok(rm) }
    }

    pub(crate) fn fp<B: Bus>(&mut self, bus: &mut B, i: u32, len: u64) -> R<()> {
        let ill = Exception::illegal(if len == 2 { 0 } else { i });
        if self.mstatus & mstatus::FS == 0 {
            return Err(ill);
        }
        let rd = ((i >> 7) & 31) as usize;
        let r1 = ((i >> 15) & 31) as usize;
        let r2 = ((i >> 20) & 31) as usize;
        let r3 = (i >> 27) as usize;
        match i & 0x7f {
            0x07 => {
                let a = self.x[r1].wrapping_add(((i as i32) >> 20) as i64 as u64);
                match (i >> 12) & 7 {
                    2 => {
                        let v = self.load(bus, a, 4)?;
                        self.f[rd] = BOX | v;
                    }
                    3 => self.f[rd] = self.load(bus, a, 8)?,
                    _ => return Err(ill),
                }
                self.set_fs_dirty();
            }
            0x27 => {
                let imm = ((((i as i32) >> 25) << 5) | ((i >> 7) & 31) as i32) as i64 as u64;
                let a = self.x[r1].wrapping_add(imm);
                match (i >> 12) & 7 {
                    2 => self.store(bus, a, 4, self.f[r2] & 0xffff_ffff)?,
                    3 => self.store(bus, a, 8, self.f[r2])?,
                    _ => return Err(ill),
                }
            }
            0x43 | 0x47 | 0x4b | 0x4f => {
                let rm = self.rm(i)?;
                let (neg_prod, neg_add) = match i & 0x7f {
                    0x43 => (false, false),
                    0x47 => (false, true),
                    0x4b => (true, false),
                    _ => (true, true),
                };
                match (i >> 25) & 3 {
                    0 => {
                        let (mut a, b, mut c) = (self.f32_of(r1), self.f32_of(r2), self.f32_of(r3));
                        if neg_prod { a = -a; }
                        if neg_add { c = -c; }
                        let mut fl = 0;
                        if a.is_snan() || b.is_snan() || c.is_snan()
                            || (a.is_inf() && b == 0.0 || a == 0.0 && b.is_inf())
                        {
                            fl |= NV;
                        }
                        let p = a as f64 * b as f64; // exact
                        let s = p + c as f64;
                        let r = s as f32;
                        let r = if r.is_nan() {
                            if !(a.is_nan() || b.is_nan() || c.is_nan()) { fl |= NV; }
                            r
                        } else {
                            let err = if s.is_infinite() || p.is_infinite() { 0 } else {
                                let e1: i32 = two_sum_err!(p, c as f64, s);
                                let d = s - r as f64;
                                if d != 0.0 { sign(d) } else { e1 }
                            };
                            let finite = a.is_finite() && b.is_finite() && c.is_finite();
                            let r2 = adjust(r, err, rm);
                            fl |= flags(r2, err, finite);
                            r2
                        };
                        self.raise(fl);
                        self.set_f32(rd, r);
                    }
                    1 => {
                        let (mut a, b, mut c) = (self.f64_of(r1), self.f64_of(r2), self.f64_of(r3));
                        if neg_prod { a = -a; }
                        if neg_add { c = -c; }
                        let mut fl = 0;
                        if a.is_snan() || b.is_snan() || c.is_snan()
                            || (a.is_inf() && b == 0.0 || a == 0.0 && b.is_inf())
                        {
                            fl |= NV;
                        }
                        let r = a.mul_add(b, c);
                        let r = if r.is_nan() {
                            if !(a.is_nan() || b.is_nan() || c.is_nan()) { fl |= NV; }
                            r
                        } else if !(a.is_finite() && b.is_finite() && c.is_finite()) {
                            r
                        } else if r.is_infinite() {
                            let r2 = adjust(r, if r > 0.0 { -1 } else { 1 }, rm);
                            fl |= OF | NX;
                            r2
                        } else {
                            // exact = p + e + c with p + e = a*b; residual = (s - r) + t + e
                            let p = a * b;
                            let e = if p.is_finite() { a.mul_add(b, -p) } else { 0.0 };
                            let s = p + c;
                            let bb = s - p;
                            let t = (p - (s - bb)) + (c - bb);
                            let err = sign(((s - r) + t) + e);
                            let r2 = adjust(r, err, rm);
                            fl |= flags(r2, err, true);
                            if err != 0 && r2.abs() < f64::MIN_POSITIVE { fl |= UF; }
                            r2
                        };
                        self.raise(fl);
                        self.set_f64(rd, r);
                    }
                    _ => return Err(ill),
                }
            }
            0x53 => self.fp_op(i, rd, r1, r2, ill)?,
            _ => return Err(ill),
        }
        self.pc = self.pc.wrapping_add(len);
        Ok(())
    }

    fn fp_op(&mut self, i: u32, rd: usize, r1: usize, r2: usize, ill: Exception) -> R<()> {
        let funct7 = i >> 25;
        let dbl = funct7 & 1 == 1;
        let f3 = (i >> 12) & 7;
        match funct7 >> 2 {
            // fadd, fsub, fmul, fdiv
            0..=3 => {
                let rm = self.rm(i)?;
                let op = funct7 >> 2;
                if dbl {
                    let (a, b) = (self.f64_of(r1), self.f64_of(r2));
                    let (r, fl) = arith(op, a, b, rm);
                    self.raise(fl);
                    self.set_f64(rd, r);
                } else {
                    let (a, b) = (self.f32_of(r1), self.f32_of(r2));
                    let (r, fl) = arith32(op, a, b, rm);
                    self.raise(fl);
                    self.set_f32(rd, r);
                }
            }
            // fsqrt
            0x0b => {
                if r2 != 0 {
                    return Err(ill);
                }
                let rm = self.rm(i)?;
                if dbl {
                    let a = self.f64_of(r1);
                    let (r, fl) = sqrt(a, rm);
                    self.raise(fl);
                    self.set_f64(rd, r);
                } else {
                    let a = self.f32_of(r1);
                    let (r, fl) = sqrt(a as f64, RNE);
                    let mut fl = fl;
                    let r32 = r as f32;
                    // sqrt in f64 of an f32 is exact enough to round once to f32
                    let err = sign(r - r32 as f64);
                    let r32 = if r32.is_nan() { r32 } else { adjust(r32, err, rm) };
                    if err != 0 { fl |= NX; }
                    self.raise(fl);
                    self.set_f32(rd, r32);
                }
            }
            // fsgnj, fsgnjn, fsgnjx
            0x04 => {
                if dbl {
                    let (a, b) = (self.f[r1], self.f[r2]);
                    let s = match f3 {
                        0 => b,
                        1 => !b,
                        2 => a ^ b,
                        _ => return Err(ill),
                    } & (1 << 63);
                    self.f[rd] = (a & !(1 << 63)) | s;
                } else {
                    let a = self.f32_of(r1).to_bits();
                    let b = self.f32_of(r2).to_bits();
                    let s = match f3 {
                        0 => b,
                        1 => !b,
                        2 => a ^ b,
                        _ => return Err(ill),
                    } & (1 << 31);
                    self.f[rd] = BOX | ((a & !(1 << 31)) | s) as u64;
                }
                self.set_fs_dirty();
            }
            // fmin, fmax
            0x05 => {
                if f3 > 1 {
                    return Err(ill);
                }
                if dbl {
                    let (a, b) = (self.f64_of(r1), self.f64_of(r2));
                    let (r, fl) = minmax(a, b, f3 == 1);
                    self.raise(fl);
                    self.set_f64(rd, r);
                } else {
                    let (a, b) = (self.f32_of(r1), self.f32_of(r2));
                    let (r, fl) = minmax(a, b, f3 == 1);
                    self.raise(fl);
                    self.set_f32(rd, r);
                }
            }
            // fcvt.s.d / fcvt.d.s
            0x08 => {
                let rm = self.rm(i)?;
                match (dbl, r2) {
                    (false, 1) => {
                        let a = self.f64_of(r1);
                        let r = a as f32;
                        let mut fl = if a.is_snan() { NV } else { 0 };
                        let r = if a.is_nan() {
                            r
                        } else {
                            let err = if r.is_infinite() && a.is_finite() {
                                if a > 0.0 { -1 } else { 1 }
                            } else {
                                sign(a - r as f64)
                            };
                            let r2 = adjust(r, err, rm);
                            fl |= flags(r2, err, a.is_finite());
                            if err != 0 && r2.abs() < f32::MIN_POSITIVE {
                                fl |= UF;
                            }
                            r2
                        };
                        self.raise(fl);
                        self.set_f32(rd, r);
                    }
                    (true, 0) => {
                        let a = self.f32_of(r1);
                        if a.is_snan() {
                            self.raise(NV);
                        }
                        self.set_f64(rd, a as f64);
                    }
                    _ => return Err(ill),
                }
            }
            // feq, flt, fle
            0x14 => {
                let (r, fl) = if dbl {
                    compare(self.f64_of(r1), self.f64_of(r2), f3)
                } else {
                    compare(self.f32_of(r1), self.f32_of(r2), f3)
                };
                let r = r.ok_or(ill)?;
                self.raise(fl);
                self.set(rd, r as u64);
            }
            // fcvt.int.fp
            0x18 => {
                let rm = self.rm(i)?;
                let a = if dbl { self.f64_of(r1) } else { self.f32_of(r1) as f64 };
                let (v, fl) = to_int(a, rm, r2 as u32).ok_or(ill)?;
                self.raise(fl);
                self.set(rd, v);
            }
            // fcvt.fp.int
            0x1a => {
                let rm = self.rm(i)?;
                let x = self.x[r1];
                let v: i128 = match r2 {
                    0 => x as i32 as i128,
                    1 => x as u32 as i128,
                    2 => x as i64 as i128,
                    3 => x as i128,
                    _ => return Err(ill),
                };
                if dbl {
                    let r = v as f64;
                    let err = (v - r as i128).signum() as i32;
                    let r = adjust(r, err, rm);
                    self.raise(if err != 0 { NX } else { 0 });
                    self.set_f64(rd, r);
                } else {
                    let r = v as f32;
                    let err = (v - r as i128).signum() as i32;
                    let r = adjust(r, err, rm);
                    self.raise(if err != 0 { NX } else { 0 });
                    self.set_f32(rd, r);
                }
            }
            // fmv.x.w / fmv.x.d, fclass
            0x1c => {
                if r2 != 0 {
                    return Err(ill);
                }
                let v = match (f3, dbl) {
                    (0, false) => self.f[r1] as u32 as i32 as i64 as u64,
                    (0, true) => self.f[r1],
                    (1, false) => classify(self.f32_of(r1).to_bits() as u64, 32),
                    (1, true) => classify(self.f[r1], 64),
                    _ => return Err(ill),
                };
                self.set(rd, v);
            }
            // fmv.w.x / fmv.d.x
            0x1e => {
                if r2 != 0 || f3 != 0 {
                    return Err(ill);
                }
                self.f[rd] = if dbl { self.x[r1] } else { BOX | (self.x[r1] & 0xffff_ffff) };
                self.set_fs_dirty();
            }
            _ => return Err(ill),
        }
        Ok(())
    }
}

/// add (0), sub (1), mul (2), div (3) in binary64.
fn arith(op: u32, a: f64, b: f64, rm: u32) -> (f64, u64) {
    let mut fl = 0;
    if a.is_snan() || b.is_snan() {
        fl |= NV;
    }
    let r = match op {
        0 => a + b,
        1 => a - b,
        2 => a * b,
        _ => a / b,
    };
    if r.is_nan() {
        if !(a.is_nan() || b.is_nan()) {
            fl |= NV;
        }
        return (r, fl);
    }
    if op == 3 && b == 0.0 && a.is_finite() && a != 0.0 {
        return (r, fl | DZ);
    }
    let finite = a.is_finite() && b.is_finite();
    if !finite {
        return (r, fl);
    }
    let err = match op {
        0 => two_sum_err!(a, b, r),
        1 => two_sum_err!(a, -b, r),
        2 => {
            if r.is_infinite() {
                if r > 0.0 { -1 } else { 1 }
            } else {
                sign(a.mul_add(b, -r))
            }
        }
        _ => {
            if r.is_infinite() {
                if r > 0.0 { -1 } else { 1 }
            } else {
                // a - r*b, scaled by the sign of b
                let res = (-r).mul_add(b, a);
                sign(res) * if b < 0.0 { -1 } else { 1 }
            }
        }
    };
    let mut r2 = adjust(r, err, rm);
    if op <= 1 && r2 == 0.0 && err == 0 && rm == RDN && !(a == 0.0 && b == 0.0) {
        r2 = -0.0;
    }
    fl |= flags(r2, err, true);
    if err != 0 && r2.abs() < f64::MIN_POSITIVE {
        fl |= UF;
    }
    (r2, fl)
}

/// The binary32 operations, through binary64 (exact for add, sub and mul; the error of
/// the final rounding gives the flags).
fn arith32(op: u32, a: f32, b: f32, rm: u32) -> (f32, u64) {
    let mut fl = 0;
    if a.is_snan() || b.is_snan() {
        fl |= NV;
    }
    let r = match op {
        0 => a + b,
        1 => a - b,
        2 => a * b,
        _ => a / b,
    };
    if r.is_nan() {
        if !(a.is_nan() || b.is_nan()) {
            fl |= NV;
        }
        return (r, fl);
    }
    if op == 3 && b == 0.0 && a.is_finite() && a != 0.0 {
        return (r, fl | DZ);
    }
    if !(a.is_finite() && b.is_finite()) {
        return (r, fl);
    }
    let (a64, b64) = (a as f64, b as f64);
    let err = if r.is_infinite() {
        if r > 0.0 { -1 } else { 1 }
    } else {
        match op {
            0 | 1 => {
                let b64 = if op == 1 { -b64 } else { b64 };
                let s = a64 + b64;
                let e: i32 = two_sum_err!(a64, b64, s);
                let d = s - r as f64;
                if d != 0.0 { sign(d) } else { e }
            }
            2 => sign(a64 * b64 - r as f64),
            _ => {
                let res = (-(r as f64)).mul_add(b64, a64);
                sign(res) * if b < 0.0 { -1 } else { 1 }
            }
        }
    };
    let mut r2 = adjust(r, err, rm);
    if op <= 1 && r2 == 0.0 && err == 0 && rm == RDN && !(a == 0.0 && b == 0.0) {
        r2 = -0.0;
    }
    fl |= flags(r2, err, true);
    if err != 0 && r2.abs() < f32::MIN_POSITIVE {
        fl |= UF;
    }
    (r2, fl)
}

fn sqrt(a: f64, rm: u32) -> (f64, u64) {
    if a.is_nan() {
        return (a, if a.is_snan() { NV } else { 0 });
    }
    if a < 0.0 {
        return (f64::NAN, NV);
    }
    let r = a.sqrt();
    if !a.is_finite() || a == 0.0 {
        return (r, 0);
    }
    let err = sign((-r).mul_add(r, a));
    let r2 = adjust(r, err, rm);
    (r2, if err != 0 { NX } else { 0 })
}

fn minmax<F: Float>(a: F, b: F, max: bool) -> (F, u64) {
    let fl = if a.is_snan() || b.is_snan() { NV } else { 0 };
    let r = if a.is_nan() && b.is_nan() {
        a // canonicalised by the caller
    } else if a.is_nan() {
        b
    } else if b.is_nan() {
        a
    } else if a.zero() && b.zero() {
        if a.neg_sign() == max { b } else { a }
    } else if max == (a < b) {
        b
    } else {
        a
    };
    (r, fl)
}

fn compare<F: Float>(a: F, b: F, f3: u32) -> (Option<bool>, u64) {
    let any_nan = a.is_nan() || b.is_nan();
    match f3 {
        2 => (Some(!any_nan && a == b), if a.is_snan() || b.is_snan() { NV } else { 0 }),
        1 => (Some(!any_nan && a < b), if any_nan { NV } else { 0 }),
        0 => (Some(!any_nan && a <= b), if any_nan { NV } else { 0 }),
        _ => (None, 0),
    }
}

/// fcvt to an integer: `kind` 0 = w, 1 = wu, 2 = l, 3 = lu.
fn to_int(a: f64, rm: u32, kind: u32) -> Option<(u64, u64)> {
    let (lo, hi): (f64, f64) = match kind {
        0 => (-2147483648.0, 2147483647.0),
        1 => (0.0, 4294967295.0),
        2 => (-9223372036854775808.0, 9223372036854775807.0),
        3 => (0.0, 18446744073709551615.0),
        _ => return None,
    };
    let sat = |high: bool| -> u64 {
        match (kind, high) {
            (0, true) => i32::MAX as i64 as u64,
            (0, false) => i32::MIN as i64 as u64,
            (1, true) => u64::MAX, // 0xffffffff sign-extended
            (1, false) => 0,
            (2, true) => i64::MAX as u64,
            (2, false) => i64::MIN as u64,
            (_, true) => u64::MAX,
            (_, false) => 0,
        }
    };
    if a.is_nan() {
        return Some((sat(true), NV));
    }
    let r = match rm {
        RTZ => a.trunc(),
        RDN => a.floor(),
        RUP => a.ceil(),
        RMM => a.round(),
        _ => a.round_ties_even(),
    };
    // for l/lu the f64 upper bound rounds up to 2^63 / 2^64: compare with >=
    let too_high = if kind >= 2 { r >= hi } else { r > hi };
    if too_high {
        return Some((sat(true), NV));
    }
    if r < lo {
        return Some((sat(false), NV));
    }
    let nx = if r != a { NX } else { 0 };
    let v = match kind {
        0 => r as i32 as i64 as u64,
        1 => r as u32 as i32 as i64 as u64,
        2 => r as i64 as u64,
        _ => r as u64,
    };
    Some((v, nx))
}

fn classify(bits: u64, width: u32) -> u64 {
    let (sign, exp, frac, emax, quiet) = if width == 32 {
        (bits >> 31 & 1, (bits >> 23) & 0xff, bits & 0x7f_ffff, 0xff, 1 << 22)
    } else {
        (bits >> 63, (bits >> 52) & 0x7ff, bits & 0xf_ffff_ffff_ffff, 0x7ff, 1 << 51)
    };
    let neg = sign == 1;
    1 << if exp == emax {
        if frac == 0 {
            if neg { 0 } else { 7 }
        } else if frac & quiet != 0 {
            9
        } else {
            8
        }
    } else if exp == 0 {
        if frac == 0 {
            if neg { 3 } else { 4 }
        } else if neg {
            2
        } else {
            5
        }
    } else if neg {
        1
    } else {
        6
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn conversions_saturate_and_round() {
        assert_eq!(to_int(1.5, RNE, 0), Some((2, NX)));
        assert_eq!(to_int(2.5, RNE, 0), Some((2, NX)));
        assert_eq!(to_int(-1.5, RTZ, 0), Some(((-1i64) as u64, NX)));
        assert_eq!(to_int(-1.5, RDN, 2), Some(((-2i64) as u64, NX)));
        assert_eq!(to_int(f64::NAN, RNE, 0), Some((i32::MAX as u64, NV)));
        assert_eq!(to_int(-1.0, RNE, 1), Some((0, NV)));
        assert_eq!(to_int(-0.25, RTZ, 3), Some((0, NX)));
        assert_eq!(to_int(4294967295.0, RNE, 1), Some((u64::MAX, 0)));
        assert_eq!(to_int(9.3e18, RNE, 2), Some((i64::MAX as u64, NV)));
        assert_eq!(to_int(1e19, RNE, 3), Some((10000000000000000000, 0)));
    }

    #[test]
    fn rounding_modes_and_flags() {
        let (r, fl) = arith(3, 1.0, 3.0, RNE);
        assert_eq!(fl, NX);
        assert_eq!(r, 1.0 / 3.0);
        let (down, _) = arith(3, 1.0, 3.0, RDN);
        let (up, _) = arith(3, 1.0, 3.0, RUP);
        assert!(down < up && (down == r || up == r));
        assert_eq!(arith(3, 1.0, 0.0, RNE), (f64::INFINITY, DZ));
        assert_eq!(arith(0, f64::MAX, f64::MAX, RTZ).0, f64::MAX);
        assert_eq!(arith(0, 1.0, 2.0, RNE), (3.0, 0));
        let (r, fl) = arith32(0, 1.0, 1e-10, RUP);
        assert_eq!(r, 1.0f32.next_up());
        assert_eq!(fl, NX);
        assert_eq!(arith(1, 1.0, 1.0, RDN).0.to_bits(), (-0.0f64).to_bits());
        assert_eq!(classify(0x7ff8_0000_0000_0000, 64), 1 << 9);
        assert_eq!(classify(0x8000_0000_0000_0000, 64), 1 << 3);
        assert_eq!(minmax(-0.0f64, 0.0, false).0.to_bits(), (-0.0f64).to_bits());
        assert_eq!(minmax(-0.0f64, 0.0, true).0.to_bits(), 0);
    }
}
