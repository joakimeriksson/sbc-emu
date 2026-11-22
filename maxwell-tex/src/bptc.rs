//! BC6H and BC7 (BPTC) blocks.

use crate::bc::{rgba8, Texels};
use crate::num::half_to_f32;
use std::sync::OnceLock;

/// Subset of each texel (bit i: texel i) of the two-subset partitions.
#[rustfmt::skip]
const PART2: [u16; 64] = [
    0xcccc, 0x8888, 0xeeee, 0xecc8, 0xc880, 0xfeec, 0xfec8, 0xec80,
    0xc800, 0xffec, 0xfe80, 0xe800, 0xffe8, 0xff00, 0xfff0, 0xf000,
    0xf710, 0x008e, 0x7100, 0x08ce, 0x008c, 0x7310, 0x3100, 0x8cce,
    0x088c, 0x3110, 0x6666, 0x366c, 0x17e8, 0x0ff0, 0x718e, 0x399c,
    0xaaaa, 0xf0f0, 0x5a5a, 0x33cc, 0x3c3c, 0x55aa, 0x9696, 0xa55a,
    0x73ce, 0x13c8, 0x324c, 0x3bdc, 0x6996, 0xc33c, 0x9966, 0x0660,
    0x0272, 0x04e4, 0x4e40, 0x2720, 0xc936, 0x936c, 0x39c6, 0x639c,
    0x9336, 0x9cc6, 0x817e, 0xe718, 0xccf0, 0x0fcc, 0x7744, 0xee22,
];

/// Two bits per texel: the three-subset partitions.
#[rustfmt::skip]
const PART3: [u32; 64] = [
    0xaa685050, 0x6a5a5040, 0x5a5a4200, 0x5450a0a8, 0xa5a50000, 0xa0a05050, 0x5555a0a0, 0x5a5a5050,
    0xaa550000, 0xaa555500, 0xaaaa5500, 0x90909090, 0x94949494, 0xa4a4a4a4, 0xa9a59450, 0x2a0a4250,
    0xa5945040, 0x0a425054, 0xa5a5a500, 0x55a0a0a0, 0xa8a85454, 0x6a6a4040, 0xa4a45000, 0x1a1a0500,
    0x0050a4a4, 0xaaa59090, 0x14696914, 0x69691400, 0xa08585a0, 0xaa821414, 0x50a4a450, 0x6a5a0200,
    0xa9a58000, 0x5090a0a8, 0xa8a09050, 0x24242424, 0x00aa5500, 0x24924924, 0x24499224, 0x50a50a50,
    0x500aa550, 0xaaaa4444, 0x66660000, 0xa5a0a5a0, 0x50a050a0, 0x69286928, 0x44aaaa44, 0x66666600,
    0xaa444444, 0x54a854a8, 0x95809580, 0x96969600, 0xa85454a8, 0x80959580, 0xaa141414, 0x96960000,
    0xaaaa1414, 0xa05050a0, 0xa0a5a5a0, 0x96000000, 0x40804080, 0xa9a8a9a8, 0xaaaaaa44, 0x2a4a5254,
];

/// The texel of the second subset whose index lacks its top bit (two subsets).
#[rustfmt::skip]
const ANCHOR2: [u8; 64] = [
    15, 15, 15, 15, 15, 15, 15, 15, 15, 15, 15, 15, 15, 15, 15, 15,
    15,  2,  8,  2,  2,  8,  8, 15,  2,  8,  2,  2,  8,  8,  2,  2,
    15, 15,  6,  8,  2,  8, 15, 15,  2,  8,  2,  2,  2, 15, 15,  6,
     6,  2,  6,  8, 15, 15,  2,  2, 15, 15, 15, 15, 15,  2,  2, 15,
];

/// The same of the second and third of three subsets.
#[rustfmt::skip]
const ANCHOR3A: [u8; 64] = [
     3,  3, 15, 15,  8,  3, 15, 15,  8,  8,  6,  6,  6,  5,  3,  3,
     3,  3,  8, 15,  3,  3,  6, 10,  5,  8,  8,  6,  8,  5, 15, 15,
     8, 15,  3,  5,  6, 10,  8, 15, 15,  3, 15,  5, 15, 15, 15, 15,
     3, 15,  5,  5,  5,  8,  5, 10,  5, 10,  8, 13, 15, 12,  3,  3,
];
#[rustfmt::skip]
const ANCHOR3B: [u8; 64] = [
    15,  8,  8,  3, 15, 15,  3,  8, 15, 15, 15, 15, 15, 15, 15,  8,
    15,  8, 15,  3, 15,  8, 15,  8,  3, 15,  6, 10, 15, 15, 10,  8,
    15,  3, 15, 10, 10,  8,  9, 10,  6, 15,  8, 15,  3,  6,  6,  8,
    15,  3, 15, 15, 15, 15, 15, 15, 15, 15, 15, 15,  3, 15, 15,  8,
];

const W2: [u16; 4] = [0, 21, 43, 64];
const W3: [u16; 8] = [0, 9, 18, 27, 37, 46, 55, 64];
const W4: [u16; 16] = [0, 4, 9, 13, 17, 21, 26, 30, 34, 38, 43, 47, 51, 55, 60, 64];

fn weight(bits: u32, i: u32) -> i32 {
    (match bits {
        2 => W2[i as usize & 3],
        3 => W3[i as usize & 7],
        _ => W4[i as usize & 15],
    }) as i32
}

/// Bits from the low end of a block.
struct Bits(u128);

impl Bits {
    fn new(b: &[u8]) -> Self {
        let mut v = [0u8; 16];
        v.copy_from_slice(&b[..16]);
        Self(u128::from_le_bytes(v))
    }

    fn take(&mut self, n: u32) -> u32 {
        let v = (self.0 & ((1 << n) - 1)) as u32;
        self.0 >>= n;
        v
    }
}

fn subset(subsets: u32, part: u32, texel: usize) -> usize {
    match subsets {
        1 => 0,
        2 => (PART2[part as usize] >> texel) as usize & 1,
        _ => (PART3[part as usize] >> (2 * texel)) as usize & 3,
    }
}

fn is_anchor(subsets: u32, part: u32, texel: usize) -> bool {
    let p = part as usize;
    texel == 0
        || (subsets == 2 && texel == ANCHOR2[p] as usize)
        || (subsets == 3 && (texel == ANCHOR3A[p] as usize || texel == ANCHOR3B[p] as usize))
}

struct Bc7Mode {
    subsets: u32,
    part: u32,
    rotation: u32,
    selector: u32,
    color: u32,
    alpha: u32,
    /// p bits per end point, or per subset.
    p_end: bool,
    p_subset: bool,
    index: u32,
    index2: u32,
}

#[rustfmt::skip]
const BC7: [Bc7Mode; 8] = {
    #[allow(clippy::too_many_arguments)]
    const fn m(
        subsets: u32, part: u32, rotation: u32, selector: u32, color: u32, alpha: u32,
        p_end: bool, p_subset: bool, index: u32, index2: u32,
    ) -> Bc7Mode {
        Bc7Mode { subsets, part, rotation, selector, color, alpha, p_end, p_subset, index, index2 }
    }
    [
        m(3, 4, 0, 0, 4, 0, true, false, 3, 0),
        m(2, 6, 0, 0, 6, 0, false, true, 3, 0),
        m(3, 6, 0, 0, 5, 0, false, false, 2, 0),
        m(2, 6, 0, 0, 7, 0, true, false, 2, 0),
        m(1, 0, 2, 1, 5, 6, false, false, 2, 3),
        m(1, 0, 2, 0, 7, 8, false, false, 2, 2),
        m(1, 0, 0, 0, 7, 7, true, false, 4, 0),
        m(2, 6, 0, 0, 5, 5, true, false, 2, 0),
    ]
};

/// Reserved blocks (no mode bit) are transparent black.
pub fn bc7(b: &[u8], srgb: bool) -> Texels {
    let mode = b[0].trailing_zeros();
    if mode > 7 {
        return [[0.0; 4]; 16];
    }
    let m = &BC7[mode as usize];
    let mut s = Bits::new(b);
    s.take(mode + 1);
    let part = s.take(m.part);
    let rotation = s.take(m.rotation);
    let selector = s.take(m.selector);
    // End points: [subset * 2 + end][component].
    let mut e = [[0u32; 4]; 6];
    let ends = m.subsets as usize * 2;
    for c in 0..3 {
        for p in &mut e[..ends] {
            p[c] = s.take(m.color);
        }
    }
    if m.alpha != 0 {
        for p in &mut e[..ends] {
            p[3] = s.take(m.alpha);
        }
    }
    let mut p = [0u32; 6];
    if m.p_end {
        for v in &mut p[..ends] {
            *v = s.take(1);
        }
    } else if m.p_subset {
        for i in 0..m.subsets as usize {
            let v = s.take(1);
            p[2 * i] = v;
            p[2 * i + 1] = v;
        }
    }
    let has_p = m.p_end || m.p_subset;
    for (ep, &pb) in e[..ends].iter_mut().zip(&p) {
        for (c, v) in ep.iter_mut().enumerate() {
            let mut bits = if c == 3 { m.alpha } else { m.color };
            if bits == 0 {
                *v = 255;
                continue;
            }
            if has_p {
                *v = *v << 1 | pb;
                bits += 1;
            }
            *v <<= 8 - bits;
            *v |= *v >> bits;
        }
    }
    let mut i1 = [0u32; 16];
    let mut i2 = [0u32; 16];
    for (t, i) in i1.iter_mut().enumerate() {
        *i = s.take(m.index - is_anchor(m.subsets, part, t) as u32);
    }
    if m.index2 != 0 {
        for (t, i) in i2.iter_mut().enumerate() {
            *i = s.take(m.index2 - (t == 0) as u32);
        }
    }
    std::array::from_fn(|t| {
        let n = 2 * subset(m.subsets, part, t);
        let (wc, wa) = if m.index2 == 0 {
            let w = weight(m.index, i1[t]);
            (w, w)
        } else {
            let (a, b) = (weight(m.index, i1[t]), weight(m.index2, i2[t]));
            if selector == 0 { (a, b) } else { (b, a) }
        };
        let mut c: [u8; 4] = std::array::from_fn(|k| {
            let w = if k == 3 { wa } else { wc };
            ((e[n][k] as i32 * (64 - w) + e[n + 1][k] as i32 * w + 32) >> 6) as u8
        });
        if rotation != 0 {
            c.swap(rotation as usize - 1, 3);
        }
        rgba8(c, srgb)
    })
}

struct Bc6Mode {
    /// Mode bits: 2 or 5 of them.
    code: u32,
    subsets: u32,
    /// Bits of the base end point, and of the others.
    base: u32,
    delta: [u32; 3],
    /// The others are differences from the base.
    transformed: bool,
    /// The fields in order: component, end point (w, x, y, z), bits from-to.
    fields: &'static str,
}

#[rustfmt::skip]
const BC6: [Bc6Mode; 14] = {
    const fn m(code: u32, subsets: u32, base: u32, delta: [u32; 3], transformed: bool, fields: &'static str) -> Bc6Mode {
        Bc6Mode { code, subsets, base, delta, transformed, fields }
    }
    [
        m(0x00, 2, 10, [5, 5, 5], true, "gy4 by4 bz4 rw0-9 gw0-9 bw0-9 rx0-4 gz4 gy0-3 gx0-4 bz0 gz0-3 bx0-4 bz1 by0-3 ry0-4 bz2 rz0-4 bz3"),
        m(0x01, 2, 7, [6, 6, 6], true, "gy5 gz4 gz5 rw0-6 bz0 bz1 by4 gw0-6 by5 bz2 gy4 bw0-6 bz3 bz5 bz4 rx0-5 gy0-3 gx0-5 gz0-3 bx0-5 by0-3 ry0-5 rz0-5"),
        m(0x02, 2, 11, [5, 4, 4], true, "rw0-9 gw0-9 bw0-9 rx0-4 rw10 gy0-3 gx0-3 gw10 bz0 gz0-3 bx0-3 bw10 bz1 by0-3 ry0-4 bz2 rz0-4 bz3"),
        m(0x06, 2, 11, [4, 5, 4], true, "rw0-9 gw0-9 bw0-9 rx0-3 rw10 gz4 gy0-3 gx0-4 gw10 gz0-3 bx0-3 bw10 bz1 by0-3 ry0-3 bz0 bz2 rz0-3 gy4 bz3"),
        m(0x0a, 2, 11, [4, 4, 5], true, "rw0-9 gw0-9 bw0-9 rx0-3 rw10 by4 gy0-3 gx0-3 gw10 bz0 gz0-3 bx0-4 bw10 by0-3 ry0-3 bz1 bz2 rz0-3 bz4 bz3"),
        m(0x0e, 2, 9, [5, 5, 5], true, "rw0-8 by4 gw0-8 gy4 bw0-8 bz4 rx0-4 gz4 gy0-3 gx0-4 bz0 gz0-3 bx0-4 bz1 by0-3 ry0-4 bz2 rz0-4 bz3"),
        m(0x12, 2, 8, [6, 5, 5], true, "rw0-7 gz4 by4 gw0-7 bz2 gy4 bw0-7 bz3 bz4 rx0-5 gy0-3 gx0-4 bz0 gz0-3 bx0-4 bz1 by0-3 ry0-5 rz0-5"),
        m(0x16, 2, 8, [5, 6, 5], true, "rw0-7 bz0 by4 gw0-7 gy5 gy4 bw0-7 gz5 bz4 rx0-4 gz4 gy0-3 gx0-5 gz0-3 bx0-4 bz1 by0-3 ry0-4 bz2 rz0-4 bz3"),
        m(0x1a, 2, 8, [5, 5, 6], true, "rw0-7 bz1 by4 gw0-7 by5 gy4 bw0-7 bz5 bz4 rx0-4 gz4 gy0-3 gx0-4 bz0 gz0-3 bx0-5 by0-3 ry0-4 bz2 rz0-4 bz3"),
        m(0x1e, 2, 6, [6, 6, 6], false, "rw0-5 gz4 bz0 bz1 by4 gw0-5 gy5 by5 bz2 gy4 bw0-5 gz5 bz3 bz5 bz4 rx0-5 gy0-3 gx0-5 gz0-3 bx0-5 by0-3 ry0-5 rz0-5"),
        m(0x03, 1, 10, [10, 10, 10], false, "rw0-9 gw0-9 bw0-9 rx0-9 gx0-9 bx0-9"),
        m(0x07, 1, 11, [9, 9, 9], true, "rw0-9 gw0-9 bw0-9 rx0-8 rw10 gx0-8 gw10 bx0-8 bw10"),
        m(0x0b, 1, 12, [8, 8, 8], true, "rw0-9 gw0-9 bw0-9 rx0-7 rw11-10 gx0-7 gw11-10 bx0-7 bw11-10"),
        m(0x0f, 1, 16, [4, 4, 4], true, "rw0-9 gw0-9 bw0-9 rx0-3 rw15-10 gx0-3 gw15-10 bx0-3 bw15-10"),
    ]
};

/// A bit of an end point: component, end point, bit.
type FieldBit = (u8, u8, u8);

fn bc6_fields() -> &'static [Vec<FieldBit>; 14] {
    static T: OnceLock<[Vec<FieldBit>; 14]> = OnceLock::new();
    T.get_or_init(|| {
        std::array::from_fn(|i| {
            let mut v = Vec::new();
            for f in BC6[i].fields.split(' ') {
                let b = f.as_bytes();
                let comp = b"rgb".iter().position(|&c| c == b[0]).unwrap_or(0) as u8;
                let end = b"wxyz".iter().position(|&c| c == b[1]).unwrap_or(0) as u8;
                let mut range = f[2..].split('-').map(|n| n.parse::<u8>().unwrap_or(0));
                let from = range.next().unwrap_or(0);
                let to = range.next().unwrap_or(from);
                if from <= to {
                    v.extend((from..=to).map(|b| (comp, end, b)));
                } else {
                    v.extend((to..=from).rev().map(|b| (comp, end, b)));
                }
            }
            v
        })
    })
}

fn sext(v: i32, bits: u32) -> i32 {
    (v << (32 - bits)) >> (32 - bits)
}

fn unquantize(v: i32, bits: u32, signed: bool) -> i32 {
    if signed {
        let a = v.abs();
        let u = if bits >= 16 || a == 0 {
            return v;
        } else if a >= (1 << (bits - 1)) - 1 {
            0x7fff
        } else {
            ((a << 15) + 0x4000) >> (bits - 1)
        };
        if v < 0 { -u } else { u }
    } else if bits >= 15 || v == 0 {
        v
    } else if v == (1 << bits) - 1 {
        0xffff
    } else {
        ((v << 15) + 0x4000) >> (bits - 1)
    }
}

/// Reserved modes are black.
pub fn bc6h(b: &[u8], signed: bool) -> Texels {
    let mut s = Bits::new(b);
    let mut code = s.take(2);
    if code > 1 {
        code |= s.take(3) << 2;
    }
    let Some(n) = BC6.iter().position(|m| m.code == code) else {
        return [[0.0, 0.0, 0.0, 1.0]; 16];
    };
    let m = &BC6[n];
    let mut e = [[0i32; 3]; 4];
    for &(comp, end, bit) in &bc6_fields()[n] {
        e[end as usize][comp as usize] |= (s.take(1) as i32) << bit;
    }
    let part = if m.subsets == 2 { s.take(5) } else { 0 };
    let ends = m.subsets as usize * 2;
    for c in 0..3 {
        if signed {
            e[0][c] = sext(e[0][c], m.base);
        }
        for k in 1..ends {
            let mut v = e[k][c];
            if m.transformed {
                v = e[0][c] + sext(v, m.delta[c]);
                v &= ((1u32 << m.base) - 1) as i32;
            }
            if signed {
                v = sext(v, m.base);
            }
            e[k][c] = v;
        }
        for p in &mut e[..ends] {
            p[c] = unquantize(p[c], m.base, signed);
        }
    }
    let bits = if m.subsets == 2 { 3 } else { 4 };
    std::array::from_fn(|t| {
        let w = weight(bits, s.take(bits - is_anchor(m.subsets, part, t) as u32));
        let n = 2 * subset(m.subsets, part, t);
        let mut out = [0.0, 0.0, 0.0, 1.0];
        for (c, o) in out.iter_mut().enumerate().take(3) {
            let v = (e[n][c] * (64 - w) + e[n + 1][c] * w + 32) >> 6;
            *o = half_to_f32(if !signed {
                ((v * 31) >> 6) as u16
            } else if v < 0 {
                ((-v * 31) >> 5) as u16 | 0x8000
            } else {
                ((v * 31) >> 5) as u16
            });
        }
        out
    })
}
