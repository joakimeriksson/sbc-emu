//! Floating-point helpers: flushing, rounding modes, halves.

use crate::isa::Rnd;

/// Flushes a denormal to zero, keeping the sign.
#[inline(always)]
pub fn ftz(x: u32) -> u32 {
    if x & 0x7f80_0000 == 0 {
        x & 0x8000_0000
    } else {
        x
    }
}

/// Clamp to 0..1; NaN becomes 0.
#[inline(always)]
pub fn sat(x: f32) -> f32 {
    if x > 0.0 {
        if x < 1.0 {
            x
        } else {
            1.0
        }
    } else {
        0.0
    }
}

/// `x` as a single, rounded as `rnd` says.
pub fn to_f32(x: f64, rnd: Rnd) -> f32 {
    let y = x as f32;
    let back = y as f64;
    if rnd == Rnd::N || back == x || x.is_nan() {
        return y;
    }
    let up = back < x;
    match rnd {
        Rnd::M if !up => y.next_down(),
        Rnd::P if up => y.next_up(),
        Rnd::Z if up != (x > 0.0) => {
            if x > 0.0 {
                y.next_down()
            } else {
                y.next_up()
            }
        }
        _ => y,
    }
}

/// a + b rounded to odd, so that a later rounding to single is exact.
fn add_odd(a: f64, b: f64) -> f64 {
    let s = a + b;
    if !s.is_finite() {
        return s;
    }
    let t = s - a;
    let e = (a - (s - t)) + (b - t);
    if e == 0.0 || s.to_bits() & 1 == 1 {
        s
    } else if e > 0.0 {
        s.next_up()
    } else {
        s.next_down()
    }
}

/// a + b in single precision.
#[inline(always)]
pub fn add(a: f32, b: f32, rnd: Rnd) -> f32 {
    if rnd == Rnd::N {
        a + b
    } else {
        to_f32(add_odd(a as f64, b as f64), rnd)
    }
}

/// a * b in single precision.
#[inline(always)]
pub fn mul(a: f32, b: f32, rnd: Rnd) -> f32 {
    if rnd == Rnd::N {
        a * b
    } else {
        to_f32(a as f64 * b as f64, rnd)
    }
}

/// a * b + c in single precision with one rounding.
#[inline(always)]
pub fn fma(a: f32, b: f32, c: f32) -> f32 {
    #[cfg(any(target_arch = "aarch64", target_feature = "fma"))]
    {
        a.mul_add(b, c)
    }
    #[cfg(not(any(target_arch = "aarch64", target_feature = "fma")))]
    {
        fma_double(a, b, c)
    }
}

/// The same without a fused instruction. The product is exact in double. Rounding the sum twice
/// goes wrong only when the double lies halfway between two singles (or is a denormal single).
#[allow(dead_code)]
#[inline(always)]
fn fma_double(a: f32, b: f32, c: f32) -> f32 {
    let p = a as f64 * b as f64;
    let s = p + c as f64;
    let halfway = s.to_bits() & 0x1fff_ffff == 0x1000_0000;
    if halfway || s.abs() < 2e-38 {
        add_odd(p, c as f64) as f32
    } else {
        s as f32
    }
}

/// a * b + c with another rounding mode.
pub fn fma_rnd(a: f32, b: f32, c: f32, rnd: Rnd) -> f32 {
    to_f32(add_odd(a as f64 * b as f64, c as f64), rnd)
}

/// Rounds to an integral value.
#[inline(always)]
pub fn round_int(x: f64, rnd: Rnd) -> f64 {
    match rnd {
        Rnd::N => x.round_ties_even(),
        Rnd::M => x.floor(),
        Rnd::P => x.ceil(),
        Rnd::Z => x.trunc(),
    }
}

#[inline(always)]
pub fn round_int32(x: f32, rnd: Rnd) -> f32 {
    match rnd {
        Rnd::N => x.round_ties_even(),
        Rnd::M => x.floor(),
        Rnd::P => x.ceil(),
        Rnd::Z => x.trunc(),
    }
}

/// minNum/maxNum: a NaN loses, -0 is less than +0.
#[inline(always)]
pub fn min(a: f32, b: f32) -> f32 {
    if a < b || b.is_nan() || (a == b && a.is_sign_negative()) {
        a
    } else {
        b
    }
}

#[inline(always)]
pub fn max(a: f32, b: f32) -> f32 {
    if a > b || b.is_nan() || (a == b && b.is_sign_negative()) {
        a
    } else {
        b
    }
}

pub fn min64(a: f64, b: f64) -> f64 {
    if a < b || b.is_nan() || (a == b && a.is_sign_negative()) {
        a
    } else {
        b
    }
}

pub fn max64(a: f64, b: f64) -> f64 {
    -min64(-a, -b)
}

/// A float comparison (codes as in `isa::FCond`), as one bit.
#[inline(always)]
pub fn cmp(cond: u8, a: f64, b: f64) -> u32 {
    let k = (a < b) as u8 | ((a == b) as u8) << 1 | ((a > b) as u8) << 2;
    let k = if k == 0 { 8 } else { k };
    (cond & k != 0) as u32
}

pub fn half_to_f32(h: u16) -> f32 {
    let sign = (h as u32 & 0x8000) << 16;
    let exp = (h >> 10) & 0x1f;
    let man = (h & 0x3ff) as u32;
    let bits = match exp {
        0 => {
            let v = man as f32 * (1.0 / 16_777_216.0);
            return f32::from_bits(sign | v.to_bits());
        }
        31 => 0x7f80_0000 | man << 13,
        _ => ((exp as u32 + 112) << 23) | man << 13,
    };
    f32::from_bits(sign | bits)
}

pub fn f32_to_half(x: f32, rnd: Rnd) -> u16 {
    let bits = x.to_bits();
    let sign = (bits >> 16 & 0x8000) as u16;
    let a = f32::from_bits(bits & 0x7fff_ffff);
    if a.is_nan() {
        return sign | 0x7e00;
    }
    if a.is_infinite() {
        return sign | 0x7c00;
    }
    // Work in units of the result's last place.
    let exp = ((bits >> 23 & 0xff) as i32 - 127).max(-14);
    let ulp = exp - 10;
    let scaled = a as f64 * 2f64.powi(-ulp);
    let rnd = match (rnd, sign != 0) {
        (Rnd::N, _) => Rnd::N,
        (Rnd::M, false) | (Rnd::P, true) | (Rnd::Z, _) => Rnd::Z,
        _ => Rnd::P,
    };
    let m = round_int(scaled, rnd) as u64;
    // m may have carried into the next binade; both cases come out right by adding.
    let e = exp + 14;
    let v = (e as u64) * 1024 + m;
    if v >= 0x7c00 {
        // overflow: the largest number when rounding towards zero
        return sign | if rnd == Rnd::Z { 0x7bff } else { 0x7c00 };
    }
    sign | v as u16
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn halves_round_trip() {
        for h in 0..=0xffffu16 {
            let f = half_to_f32(h);
            let back = f32_to_half(f, Rnd::N);
            if f.is_nan() {
                assert_eq!(back & 0x7e00, 0x7e00);
            } else {
                assert_eq!(back, h, "{h:#x} {f}");
            }
        }
        assert_eq!(f32_to_half(65519.0, Rnd::N), 0x7bff);
        assert_eq!(f32_to_half(65520.0, Rnd::N), 0x7c00);
        assert_eq!(f32_to_half(65535.0, Rnd::Z), 0x7bff);
        assert_eq!(f32_to_half(1.0 + 1.0 / 2048.0, Rnd::N), 0x3c00);
        assert_eq!(f32_to_half(1.0 + 1.0 / 2048.0, Rnd::P), 0x3c01);
        assert_eq!(f32_to_half(-1.0 - 1.0 / 2048.0, Rnd::M), 0xbc01);
        assert_eq!(f32_to_half(1.9999, Rnd::N), 0x4000);
    }

    #[test]
    fn directed_rounding() {
        let third = 1.0f64 / 3.0;
        assert!((to_f32(third, Rnd::M) as f64) < third);
        assert!((to_f32(third, Rnd::P) as f64) > third);
        assert!((to_f32(-third, Rnd::Z) as f64) > -third);
        assert_eq!(add(1.0, 1e-30, Rnd::P), 1.0f32.next_up());
        assert_eq!(add(1.0, -1e-30, Rnd::Z), 1.0f32.next_down());
        assert_eq!(add(1.0, 1e-30, Rnd::M), 1.0);
    }

    #[test]
    fn fma_is_fused() {
        let mut s = 12345u32;
        for _ in 0..200_000 {
            let mut r = || {
                s ^= s << 13;
                s ^= s >> 17;
                s ^= s << 5;
                f32::from_bits(s)
            };
            let (a, b, c) = (r(), r(), r());
            let y = a.mul_add(b, c);
            for x in [fma(a, b, c), fma_double(a, b, c)] {
                assert!(x.to_bits() == y.to_bits() || (x.is_nan() && y.is_nan()), "{a} {b} {c}");
            }
            // and close to halfway: 24-bit mantissas of moderate size
            let (a, b) = (f32::from_bits(0x3f80_0000 | s >> 9), f32::from_bits(0x3f80_0000 | s & 0xfff));
            let c = f32::from_bits((s & 0x8000_0000) | 0x3300_0000 + (s >> 20 & 0x7f) * 0x80_000);
            assert_eq!(fma_double(a, b, c).to_bits(), a.mul_add(b, c).to_bits());
        }
    }
}
