//! Arm floating point in software: IEEE 754 binary16/32/64 arithmetic under the FPCR's rounding
//! mode, flush-to-zero (FZ) and default-NaN (DN) controls, raising the FPSR's cumulative
//! exception flags, with the Arm rules for NaN propagation, tininess (detected before rounding)
//! and estimates. Values are raw bit patterns; `es` is the element size (16, 32 or 64).
//!
//! Every operation computes the exact result (wide integers) and rounds once, so results and
//! flags are bit-exact with hardware (checked against the host CPU in tests/diff_fp.rs).

pub const IOC: u32 = 1 << 0;
pub const DZC: u32 = 1 << 1;
pub const OFC: u32 = 1 << 2;
pub const UFC: u32 = 1 << 3;
pub const IXC: u32 = 1 << 4;
pub const IDC: u32 = 1 << 7;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Rnd {
    /// to nearest, ties to even (RN)
    Even,
    /// towards +infinity (RP)
    Up,
    /// towards -infinity (RM)
    Down,
    /// towards zero (RZ)
    Zero,
    /// to nearest, ties away from zero (FRINTA, FCVTA*)
    Away,
    /// round to odd (FCVTXN)
    Odd,
}

impl Rnd {
    /// FPCR.RMode / the rounding field of FRINT* and FCVT* encodings: 0 N, 1 P, 2 M, 3 Z.
    pub fn from_bits(b: u64) -> Rnd {
        match b & 3 { 0 => Rnd::Even, 1 => Rnd::Up, 2 => Rnd::Down, _ => Rnd::Zero }
    }
}

/// Comparison predicates of FCM*/FAC*.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Pred { Eq, Ge, Gt }

#[derive(Clone, Copy)]
struct Fmt { es: u32, mb: u32, eb: u32, bias: i32 }

#[inline]
fn fmt(es: u32) -> Fmt {
    match es {
        16 => Fmt { es: 16, mb: 10, eb: 5, bias: 15 },
        32 => Fmt { es: 32, mb: 23, eb: 8, bias: 127 },
        _ => Fmt { es: 64, mb: 52, eb: 11, bias: 1023 },
    }
}

impl Fmt {
    #[inline] fn sign(self) -> u64 { 1 << (self.es - 1) }
    #[inline] fn emax(self) -> u64 { (1 << self.eb) - 1 }
    #[inline] fn expf(self, x: u64) -> u64 { (x >> self.mb) & self.emax() }
    #[inline] fn frac(self, x: u64) -> u64 { x & ((1 << self.mb) - 1) }
    #[inline] fn neg(self, x: u64) -> bool { x & self.sign() != 0 }
    #[inline] fn is_nan(self, x: u64) -> bool { self.expf(x) == self.emax() && self.frac(x) != 0 }
    #[inline] fn is_snan(self, x: u64) -> bool { self.is_nan(x) && x & (1 << (self.mb - 1)) == 0 }
    #[inline] fn is_qnan(self, x: u64) -> bool { self.is_nan(x) && x & (1 << (self.mb - 1)) != 0 }
    #[inline] fn is_inf(self, x: u64) -> bool { self.expf(x) == self.emax() && self.frac(x) == 0 }
    #[inline] fn is_zero(self, x: u64) -> bool { x & !self.sign() & ((self.sign() << 1).wrapping_sub(1)) == 0 }
    #[inline] fn is_denorm(self, x: u64) -> bool { self.expf(x) == 0 && self.frac(x) != 0 }
    #[inline] fn quiet(self, x: u64) -> u64 { x | (1 << (self.mb - 1)) }
    #[inline] fn dnan(self) -> u64 { (self.emax() << self.mb) | (1 << (self.mb - 1)) }
    #[inline] fn s(self, s: bool) -> u64 { if s { self.sign() } else { 0 } }
    #[inline] fn inf(self, s: bool) -> u64 { self.s(s) | (self.emax() << self.mb) }
    #[inline] fn zero(self, s: bool) -> u64 { self.s(s) }
    #[inline] fn max(self, s: bool) -> u64 { self.s(s) | (((self.emax() - 1) << self.mb) | ((1 << self.mb) - 1)) }
    /// a finite nonzero value as m * 2^e
    #[inline]
    fn unpack(self, x: u64) -> (u64, i32) {
        let ef = self.expf(x);
        if ef == 0 { (self.frac(x), 1 - self.bias - self.mb as i32) } else { (self.frac(x) | (1 << self.mb), ef as i32 - self.bias - self.mb as i32) }
    }
    /// sign-magnitude ordering key with -0 below +0 (for FMAX/FMIN)
    fn key(self, x: u64) -> i128 { let m = (x & !self.sign()) as i128; if self.neg(x) { -m - 1 } else { m } }
    /// numeric ordering key: +0 == -0 (for comparisons)
    fn val(self, x: u64) -> i128 { let m = (x & !self.sign()) as i128; if self.neg(x) { -m } else { m } }
}

#[inline]
fn bitlen(m: u128) -> i32 { 128 - m.leading_zeros() as i32 }

/// `x >> d` and whether nonzero bits were shifted out.
#[inline]
fn shr_sticky(x: u128, d: u32) -> (u128, bool) {
    if d == 0 { (x, false) } else if d >= 128 { (0, x != 0) } else { (x >> d, x & ((1u128 << d) - 1) != 0) }
}

/// Integer square root of `n` and whether it is exact.
fn isqrt(n: u128) -> (u128, bool) {
    let mut x = n;
    let mut r: u128 = 0;
    let mut bit: u128 = 1 << 126;
    while bit > n { bit >>= 2; }
    while bit != 0 {
        if x >= r + bit { x -= r + bit; r = (r >> 1) + bit; } else { r >>= 1; }
        bit >>= 2;
    }
    (r, x == 0)
}

#[derive(Clone, Copy)]
pub struct Fpu {
    pub rnd: Rnd,
    pub fz: bool,
    pub dn: bool,
    /// FPSR cumulative flags raised so far (IOC, DZC, OFC, UFC, IXC, IDC bits)
    pub flags: u32,
}

impl Fpu {
    pub fn new(fpcr: u64) -> Fpu {
        Fpu { rnd: Rnd::from_bits(fpcr >> 22), fz: fpcr >> 24 & 1 != 0, dn: fpcr >> 25 & 1 != 0, flags: 0 }
    }

    /// Flush a denormal input to zero under FZ (half precision is not affected on ARMv8.0).
    #[inline]
    fn input(&mut self, f: Fmt, x: u64) -> u64 {
        if self.fz && f.es != 16 && f.is_denorm(x) { self.flags |= IDC; f.zero(f.neg(x)) } else { x }
    }
    #[inline]
    fn nan_out(&self, f: Fmt, x: u64) -> u64 { if self.dn { f.dnan() } else { x } }
    /// FPProcessNaNs: the first signalling NaN (quietened, IOC), else the first quiet NaN.
    #[inline]
    fn nans(&mut self, f: Fmt, ops: &[u64]) -> Option<u64> {
        if !ops.iter().any(|&x| f.is_nan(x)) { return None; }
        for &x in ops { if f.is_snan(x) { self.flags |= IOC; return Some(self.nan_out(f, f.quiet(x))); } }
        ops.iter().find(|&&x| f.is_nan(x)).map(|&x| self.nan_out(f, x))
    }
    fn invalid(&mut self, f: Fmt) -> u64 { self.flags |= IOC; f.dnan() }

    /// The rounding decision for dropping `sh` low bits of `m` (with `sticky` below them):
    /// the kept (rounded) part and whether the result is inexact.
    fn decide(rnd: Rnd, s: bool, m: u128, sh: i32, sticky: bool) -> (u128, bool) {
        let (kept, gt, eq, inexact) = if sh <= 0 {
            (m << (-sh) as u32, false, false, sticky)
        } else if sh > 128 {
            (0, false, false, m != 0 || sticky)
        } else {
            let sh = sh as u32;
            let kept = if sh == 128 { 0 } else { m >> sh };
            let rem = if sh == 128 { m } else { m & ((1u128 << sh) - 1) };
            let half = 1u128 << (sh - 1);
            (kept, rem > half || (rem == half && sticky), rem == half && !sticky, rem != 0 || sticky)
        };
        let up = match rnd {
            Rnd::Even => gt || (eq && kept & 1 == 1),
            Rnd::Away => gt || eq,
            Rnd::Zero | Rnd::Odd => false,
            Rnd::Up => inexact && !s,
            Rnd::Down => inexact && s,
        };
        let kept = kept + up as u128;
        (if rnd == Rnd::Odd && inexact { kept | 1 } else { kept }, inexact)
    }

    /// Round (-1)^s * (m + sticky) * 2^e (m != 0) to format `f`: FPRound.
    fn round(&mut self, f: Fmt, s: bool, e: i32, m: u128, sticky: bool) -> u64 {
        let p = f.mb as i32 + 1;
        let n = bitlen(m);
        let mut be = e + n - 1 + f.bias;
        let tiny = be < 1;
        if tiny && self.fz && f.es != 16 {
            self.flags |= UFC;
            return f.zero(s);
        }
        let sh = if tiny { n - p + (1 - be) } else { n - p };
        let (kept, inexact) = Self::decide(self.rnd, s, m, sh, sticky);
        let mut kept = kept as u64;
        if inexact { self.flags |= IXC; if tiny { self.flags |= UFC; } }
        if tiny {
            // subnormal, or rounded up to the smallest normal (the carry sets the exponent field)
            return f.s(s) | kept;
        }
        if kept >> p != 0 { kept >>= 1; be += 1; }
        if be >= f.emax() as i32 {
            self.flags |= OFC | IXC;
            let to_inf = match self.rnd { Rnd::Even | Rnd::Away => true, Rnd::Zero | Rnd::Odd => false, Rnd::Up => !s, Rnd::Down => s };
            return if to_inf { f.inf(s) } else { f.max(s) };
        }
        f.s(s) | ((be as u64) << f.mb) | (kept & ((1 << f.mb) - 1))
    }

    /// Round the exact sum of two signed terms m * 2^e (either m may be 0), times 2^adj.
    #[allow(clippy::too_many_arguments)]
    fn sum(&mut self, f: Fmt, s1: bool, e1: i32, m1: u128, s2: bool, e2: i32, m2: u128, adj: i32) -> u64 {
        if m2 == 0 { return self.round(f, s1, e1 + adj, m1, false); }
        if m1 == 0 { return self.round(f, s2, e2 + adj, m2, false); }
        // both normalized to bit 125 (headroom for the carry), the smaller exponent shifted
        // right with its lost bits jammed into bit 0 (enough guard bits stay above it)
        let (n1, n2) = (125 - bitlen(m1), 125 - bitlen(m2));
        let (x, ex, y, ey) = (m1 << n1, e1 - n1, m2 << n2, e2 - n2);
        let (x, ex, sx, y, ey, sy) = if ex >= ey { (x, ex, s1, y, ey, s2) } else { (y, ey, s2, x, ex, s1) };
        let (y, st) = shr_sticky(y, (ex - ey).min(200) as u32);
        let y = y | st as u128;
        if sx == sy { self.round(f, sx, ex + adj, x + y, false) }
        else if x > y { self.round(f, sx, ex + adj, x - y, false) }
        else if x < y { self.round(f, sy, ex + adj, y - x, false) }
        else { f.zero(self.rnd == Rnd::Down) }
    }

    // ------------------------------------------------------------------ arithmetic

    pub fn add(&mut self, es: u32, a: u64, b: u64) -> u64 { self.addsub(fmt(es), a, b, false) }
    pub fn sub(&mut self, es: u32, a: u64, b: u64) -> u64 { self.addsub(fmt(es), a, b, true) }

    fn addsub(&mut self, f: Fmt, a: u64, b: u64, negb: bool) -> u64 {
        let (a, b) = (self.input(f, a), self.input(f, b));
        if let Some(n) = self.nans(f, &[a, b]) { return n; }
        let b = if negb { b ^ f.sign() } else { b };
        let (sa, sb) = (f.neg(a), f.neg(b));
        match (f.is_inf(a), f.is_inf(b)) {
            (true, true) => return if sa == sb { a } else { self.invalid(f) },
            (true, false) => return a,
            (false, true) => return b,
            _ => {}
        }
        match (f.is_zero(a), f.is_zero(b)) {
            (true, true) => return f.zero(if sa == sb { sa } else { self.rnd == Rnd::Down }),
            (true, false) => return self.exact(f, b),
            (false, true) => return self.exact(f, a),
            _ => {}
        }
        let ((ma, ea), (mb, eb)) = (f.unpack(a), f.unpack(b));
        self.sum(f, sa, ea, ma as u128, sb, eb, mb as u128, 0)
    }

    /// An input passed through unchanged (exact): a denormal is still subject to FPRound.
    fn exact(&mut self, f: Fmt, x: u64) -> u64 {
        if f.is_denorm(x) { let (m, e) = f.unpack(x); self.round(f, f.neg(x), e, m as u128, false) } else { x }
    }

    pub fn mul(&mut self, es: u32, a: u64, b: u64) -> u64 { self.mul_x(fmt(es), a, b, false) }
    /// FMULX: infinity times zero is 2.0 (signed) instead of invalid.
    pub fn mulx(&mut self, es: u32, a: u64, b: u64) -> u64 { self.mul_x(fmt(es), a, b, true) }

    fn mul_x(&mut self, f: Fmt, a: u64, b: u64, x: bool) -> u64 {
        let (a, b) = (self.input(f, a), self.input(f, b));
        if let Some(n) = self.nans(f, &[a, b]) { return n; }
        let s = f.neg(a) != f.neg(b);
        let (ia, ib, za, zb) = (f.is_inf(a), f.is_inf(b), f.is_zero(a), f.is_zero(b));
        if (ia && zb) || (za && ib) { return if x { f.s(s) | ((f.bias as u64 + 1) << f.mb) } else { self.invalid(f) }; }
        if ia || ib { return f.inf(s); }
        if za || zb { return f.zero(s); }
        let ((ma, ea), (mb, eb)) = (f.unpack(a), f.unpack(b));
        self.round(f, s, ea + eb, ma as u128 * mb as u128, false)
    }

    pub fn div(&mut self, es: u32, a: u64, b: u64) -> u64 {
        let f = fmt(es);
        let (a, b) = (self.input(f, a), self.input(f, b));
        if let Some(n) = self.nans(f, &[a, b]) { return n; }
        let s = f.neg(a) != f.neg(b);
        let (ia, ib, za, zb) = (f.is_inf(a), f.is_inf(b), f.is_zero(a), f.is_zero(b));
        if (ia && ib) || (za && zb) { return self.invalid(f); }
        if ia { return f.inf(s); }
        if ib { return f.zero(s); }
        if zb { self.flags |= DZC; return f.inf(s); }
        if za { return f.zero(s); }
        let ((ma, ea), (mb, eb)) = (f.unpack(a), f.unpack(b));
        let sh = 125 - bitlen(ma as u128);
        let n = (ma as u128) << sh;
        let (q, r) = (n / mb as u128, n % mb as u128);
        self.round(f, s, ea - sh - eb, q, r != 0)
    }

    pub fn sqrt(&mut self, es: u32, a: u64) -> u64 {
        let f = fmt(es);
        let a = self.input(f, a);
        if let Some(n) = self.nans(f, &[a]) { return n; }
        if f.is_zero(a) { return a; }
        if f.neg(a) { return self.invalid(f); }
        if f.is_inf(a) { return a; }
        let (m, e) = f.unpack(a);
        let mut sh = 120 - bitlen(m as u128);
        if (e - sh) & 1 != 0 { sh += 1; }
        let (r, exact) = isqrt((m as u128) << sh);
        self.round(f, false, (e - sh) / 2, r, !exact)
    }

    /// acc + a*b with one rounding (FMADD family; callers negate operands for the variants).
    pub fn fma(&mut self, es: u32, acc: u64, a: u64, b: u64) -> u64 {
        let f = fmt(es);
        let (c, a, b) = (self.input(f, acc), self.input(f, a), self.input(f, b));
        let inf_zero = (f.is_inf(a) && f.is_zero(b)) || (f.is_zero(a) && f.is_inf(b));
        if let Some(n) = self.nans(f, &[c, a, b]) {
            // a quiet-NaN addend does not hide the invalid infinity * zero
            return if f.is_qnan(c) && inf_zero { self.invalid(f) } else { n };
        }
        let sp = f.neg(a) != f.neg(b);
        let (ip, zp) = (f.is_inf(a) || f.is_inf(b), f.is_zero(a) || f.is_zero(b));
        let sc = f.neg(c);
        if inf_zero || (f.is_inf(c) && ip && sc != sp) { return self.invalid(f); }
        if f.is_inf(c) { return c; }
        if ip { return f.inf(sp); }
        if zp && f.is_zero(c) { return f.zero(if sc == sp { sc } else { self.rnd == Rnd::Down }); }
        if zp { return self.exact(f, c); }
        let ((ma, ea), (mb, eb)) = (f.unpack(a), f.unpack(b));
        let (mc, ec) = if f.is_zero(c) { (0, 0) } else { f.unpack(c) };
        self.sum(f, sp, ea + eb, ma as u128 * mb as u128, sc, ec, mc as u128, 0)
    }

    /// FRECPS (2 - a*b) and FRSQRTS ((3 - a*b) / 2), fused.
    pub fn recip_step(&mut self, es: u32, a: u64, b: u64, rsqrt: bool) -> u64 {
        let f = fmt(es);
        let (a, b) = (self.input(f, a), self.input(f, b));
        let a = a ^ f.sign();
        if let Some(n) = self.nans(f, &[a, b]) { return n; }
        let (ia, ib, za, zb) = (f.is_inf(a), f.is_inf(b), f.is_zero(a), f.is_zero(b));
        let sp = f.neg(a) != f.neg(b);
        if (ia && zb) || (za && ib) {
            // 2.0 or 1.5
            return if rsqrt { ((f.bias as u64) << f.mb) | (1 << (f.mb - 1)) } else { (f.bias as u64 + 1) << f.mb };
        }
        if ia || ib { return f.inf(sp); }
        let (k, adj) = if rsqrt { (3u128, -1) } else { (2u128, 0) };
        if za || zb { return self.round(f, false, adj, k, false); }
        let ((ma, ea), (mb, eb)) = (f.unpack(a), f.unpack(b));
        self.sum(f, sp, ea + eb, ma as u128 * mb as u128, false, 0, k, adj)
    }

    // ------------------------------------------------------------------ min / max / compare

    pub fn max(&mut self, es: u32, a: u64, b: u64) -> u64 { self.minmax(fmt(es), a, b, true, false) }
    pub fn min(&mut self, es: u32, a: u64, b: u64) -> u64 { self.minmax(fmt(es), a, b, false, false) }
    pub fn maxnm(&mut self, es: u32, a: u64, b: u64) -> u64 { self.minmax(fmt(es), a, b, true, true) }
    pub fn minnm(&mut self, es: u32, a: u64, b: u64) -> u64 { self.minmax(fmt(es), a, b, false, true) }

    fn minmax(&mut self, f: Fmt, a: u64, b: u64, is_max: bool, nm: bool) -> u64 {
        let (a, b) = (self.input(f, a), self.input(f, b));
        if nm {
            // a single quiet NaN loses to a number
            if f.is_qnan(a) && !f.is_nan(b) { return self.exact(f, b); }
            if f.is_qnan(b) && !f.is_nan(a) { return self.exact(f, a); }
        }
        if let Some(n) = self.nans(f, &[a, b]) { return n; }
        let (ka, kb) = (f.key(a), f.key(b));
        let r = if is_max == (ka >= kb) { a } else { b };
        self.exact(f, r)
    }

    /// FCMP / FCMPE: NZCV (in bits 31..28). `signal` (FCMPE) raises IOC for quiet NaNs too.
    pub fn cmp(&mut self, es: u32, a: u64, b: u64, signal: bool) -> u32 {
        let f = fmt(es);
        let (a, b) = (self.input(f, a), self.input(f, b));
        if f.is_nan(a) || f.is_nan(b) {
            if signal || f.is_snan(a) || f.is_snan(b) { self.flags |= IOC; }
            return 0b0011 << 28;
        }
        let (x, y) = (f.val(a), f.val(b));
        if x == y { 0b0110 << 28 } else if x < y { 0b1000 << 28 } else { 0b0010 << 28 }
    }

    /// FCMEQ (quiet: only signalling NaNs raise IOC), FCMGE/FCMGT (any NaN raises IOC).
    pub fn cmp_pred(&mut self, es: u32, a: u64, b: u64, p: Pred) -> bool {
        let f = fmt(es);
        let (a, b) = (self.input(f, a), self.input(f, b));
        if f.is_nan(a) || f.is_nan(b) {
            if p != Pred::Eq || f.is_snan(a) || f.is_snan(b) { self.flags |= IOC; }
            return false;
        }
        let (x, y) = (f.val(a), f.val(b));
        match p { Pred::Eq => x == y, Pred::Ge => x >= y, Pred::Gt => x > y }
    }

    // ------------------------------------------------------------------ conversions

    /// FRINT*: round to an integral value in format `es`; `exact` (FRINTX) raises IXC.
    pub fn round_int(&mut self, es: u32, a: u64, rnd: Rnd, exact: bool) -> u64 {
        let f = fmt(es);
        let a = self.input(f, a);
        if let Some(n) = self.nans(f, &[a]) { return n; }
        if f.is_inf(a) || f.is_zero(a) { return a; }
        let (m, e) = f.unpack(a);
        if e >= 0 { return a; }
        let s = f.neg(a);
        let (k, inexact) = Self::decide(rnd, s, m as u128, -e, false);
        if inexact && exact { self.flags |= IXC; }
        if k == 0 { return f.zero(s); }
        let save = self.flags;
        let r = self.round(f, s, 0, k, false);
        self.flags = save;
        r
    }

    /// FCVT*: to a `bits`-bit integer (with `fbits` fraction bits), saturating (IOC).
    pub fn to_int(&mut self, es: u32, a: u64, rnd: Rnd, unsigned: bool, bits: u32, fbits: u32) -> u64 {
        let f = fmt(es);
        let a = self.input(f, a);
        let mask = if bits == 64 { u64::MAX } else { (1 << bits) - 1 };
        let (imax, imin) = if unsigned { (mask, 0) } else { (mask >> 1, (mask >> 1) + 1) };
        if f.is_nan(a) { self.flags |= IOC; return 0; }
        let s = f.neg(a);
        let sat = |me: &mut Self| { me.flags |= IOC; if s { imin } else { imax } };
        if f.is_inf(a) { return sat(self); }
        if f.is_zero(a) { return 0; }
        let (m, e) = f.unpack(a);
        let e = e + fbits as i32;
        let (k, inexact) = if e >= 0 {
            if e > 70 { return sat(self); }
            ((m as u128) << e, false)
        } else {
            Self::decide(rnd, s, m as u128, -e, false)
        };
        let over = if unsigned { (s && k != 0) || k > mask as u128 } else if s { k > imin as u128 } else { k > imax as u128 };
        if over { return sat(self); }
        if inexact { self.flags |= IXC; }
        (if s { (k as u64).wrapping_neg() } else { k as u64 }) & mask
    }

    /// [SU]CVTF: a `bits`-bit integer (with `fbits` fraction bits) to format `es`.
    pub fn from_int(&mut self, es: u32, x: u64, signed: bool, bits: u32, fbits: u32) -> u64 {
        let x = if bits == 64 { x } else { x & ((1 << bits) - 1) };
        let (s, mag) = if signed {
            let v = ((x << (64 - bits)) as i64) >> (64 - bits);
            (v < 0, v.unsigned_abs())
        } else { (false, x) };
        let f = fmt(es);
        if mag == 0 { return f.zero(false); }
        self.round(f, s, -(fbits as i32), mag as u128, false)
    }

    /// FCVT between precisions.
    pub fn convert(&mut self, from: u32, to: u32, a: u64) -> u64 {
        let (fi, fo) = (fmt(from), fmt(to));
        let a = self.input(fi, a);
        let s = fi.neg(a);
        if fi.is_nan(a) {
            if fi.is_snan(a) { self.flags |= IOC; }
            if self.dn { return fo.dnan(); }
            let fr = fi.frac(a);
            let fr = if fo.mb >= fi.mb { fr << (fo.mb - fi.mb) } else { fr >> (fi.mb - fo.mb) };
            return fo.s(s) | (fo.emax() << fo.mb) | fr | (1 << (fo.mb - 1));
        }
        if fi.is_inf(a) { return fo.inf(s); }
        if fi.is_zero(a) { return fo.zero(s); }
        let (m, e) = fi.unpack(a);
        self.round(fo, s, e, m as u128, false)
    }

    // ------------------------------------------------------------------ estimates

    /// FRECPE: Arm's 8-bit reciprocal estimate.
    pub fn recip_estimate(&mut self, es: u32, a: u64) -> u64 {
        let f = fmt(es);
        let a = self.input(f, a);
        if let Some(n) = self.nans(f, &[a]) { return n; }
        let s = f.neg(a);
        if f.is_inf(a) { return f.zero(s); }
        if f.is_zero(a) { self.flags |= DZC; return f.inf(s); }
        let mag = a & !f.sign();
        // too small: the reciprocal overflows
        let tiny = match es { 16 => mag < 0x0100, 32 => mag < 0x0020_0000, _ => mag < 0x0004_0000_0000_0000 };
        if tiny {
            self.flags |= OFC | IXC;
            let to_inf = match self.rnd { Rnd::Even | Rnd::Away => true, Rnd::Zero | Rnd::Odd => false, Rnd::Up => !s, Rnd::Down => s };
            return if to_inf { f.inf(s) } else { f.max(s) };
        }
        let exp = f.expf(a) as i64;
        if self.fz && es != 16 && exp >= f.emax() as i64 - 2 {
            // the reciprocal underflows: flushed to zero
            self.flags |= UFC;
            return f.zero(s);
        }
        let mut fraction = f.frac(a) << (52 - f.mb);
        let mut exp = exp;
        if exp == 0 {
            if fraction >> 51 & 1 == 0 { exp = -1; fraction = (fraction & ((1 << 50) - 1)) << 2; } else { fraction = (fraction & ((1 << 51) - 1)) << 1; }
        }
        let scaled = 0x100 | (fraction >> 44) & 0xff;
        let mut result_exp = 2 * f.bias as i64 - 1 - exp;
        let est = {
            let a = scaled * 2 + 1;
            let b = (1u64 << 19) / a;
            (b + 1) / 2
        };
        let mut fraction = (est & 0xff) << 44;
        if result_exp == 0 { fraction = (1 << 51) | (fraction >> 1); } else if result_exp == -1 { fraction = (1 << 50) | (fraction >> 2); result_exp = 0; }
        f.s(s) | ((result_exp as u64 & f.emax()) << f.mb) | (fraction >> (52 - f.mb))
    }

    /// FRSQRTE: Arm's 8-bit reciprocal square root estimate.
    pub fn rsqrt_estimate(&mut self, es: u32, a: u64) -> u64 {
        let f = fmt(es);
        let a = self.input(f, a);
        if let Some(n) = self.nans(f, &[a]) { return n; }
        let s = f.neg(a);
        if f.is_zero(a) { self.flags |= DZC; return f.inf(s); }
        if s { return self.invalid(f); }
        if f.is_inf(a) { return f.zero(false); }
        let mut fraction = f.frac(a) << (52 - f.mb);
        let mut exp = f.expf(a) as i64;
        if exp == 0 {
            while fraction >> 51 & 1 == 0 { fraction = (fraction << 1) & ((1 << 52) - 1); exp -= 1; }
            fraction = (fraction << 1) & ((1 << 52) - 1);
        }
        let scaled = if exp & 1 == 0 { 0x100 | (fraction >> 44) & 0xff } else { 0x80 | (fraction >> 45) & 0x7f };
        let result_exp = (3 * f.bias as i64 - 1 - exp).div_euclid(2);
        let est = {
            let mut a = scaled;
            if a < 256 { a = a * 2 + 1; } else { a = (a >> 1) << 1; a = (a + 1) * 2; }
            let mut b = 512u64;
            while a * (b + 1) * (b + 1) < 1 << 28 { b += 1; }
            (b + 1) / 2
        };
        ((result_exp as u64 & f.emax()) << f.mb) | ((est & 0xff) << (f.mb - 8))
    }
}
