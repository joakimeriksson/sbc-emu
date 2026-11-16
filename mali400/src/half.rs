//! IEEE 754 binary16 conversions.

pub fn to_f32(h: u16) -> f32 {
    let sign = (h as u32 & 0x8000) << 16;
    let exp = (h >> 10) & 0x1f;
    let mant = (h & 0x3ff) as u32;
    let bits = match exp {
        0 if mant == 0 => sign,
        0 => {
            // subnormal: normalise
            let shift = mant.leading_zeros() - 21;
            sign | (113 - shift) << 23 | (mant << (shift + 13)) & 0x7f_ffff
        }
        0x1f => sign | 0x7f80_0000 | mant << 13,
        _ => sign | ((exp as u32 + 112) << 23) | mant << 13,
    };
    f32::from_bits(bits)
}

pub fn from_f32(f: f32) -> u16 {
    let bits = f.to_bits();
    let sign = ((bits >> 16) & 0x8000) as u16;
    let exp = ((bits >> 23) & 0xff) as i32;
    let mant = bits & 0x7f_ffff;
    if exp == 0xff {
        return sign | 0x7c00 | if mant != 0 { 0x200 } else { 0 };
    }
    let e = exp - 127 + 15;
    if e >= 0x1f {
        return sign | 0x7c00;
    }
    if e <= 0 {
        if e < -10 {
            return sign;
        }
        let m = mant | 0x80_0000;
        let shift = (14 - e) as u32;
        let half = m >> shift;
        let rest = m & ((1 << shift) - 1);
        let round = rest > (1 << (shift - 1)) || (rest == 1 << (shift - 1) && half & 1 != 0);
        return sign | (half + round as u32) as u16;
    }
    let half = (e as u32) << 10 | mant >> 13;
    let rest = mant & 0x1fff;
    let round = rest > 0x1000 || (rest == 0x1000 && half & 1 != 0);
    sign | (half + round as u32) as u16
}

#[cfg(test)]
mod tests {
    #[test]
    fn round_trip() {
        for h in 0..=0xffffu16 {
            let f = super::to_f32(h);
            if f.is_nan() {
                continue;
            }
            assert_eq!(super::from_f32(f), h, "{h:04x} {f}");
        }
        assert_eq!(super::from_f32(1.0), 0x3c00);
        assert_eq!(super::from_f32(-2.5), 0xc100);
    }
}
