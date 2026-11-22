//! ETC2 and EAC blocks (GM20B is one of the two NVIDIA chips that decode them). Blocks are
//! big endian, texels run down the columns.

use crate::bc::{rgba8, Texels};

const MODIFIER: [[i32; 2]; 8] =
    [[2, 8], [5, 17], [9, 29], [13, 42], [18, 60], [24, 80], [33, 106], [47, 183]];

/// Distances of the T and H modes.
const DISTANCE: [i32; 8] = [3, 6, 11, 16, 23, 32, 41, 64];

#[rustfmt::skip]
const EAC: [[i32; 8]; 16] = [
    [-3, -6, -9, -15, 2, 5, 8, 14], [-3, -7, -10, -13, 2, 6, 9, 12],
    [-2, -5, -8, -13, 1, 4, 7, 12], [-2, -4, -6, -13, 1, 3, 5, 12],
    [-3, -6, -8, -12, 2, 5, 7, 11], [-3, -7, -9, -11, 2, 6, 8, 10],
    [-4, -7, -8, -11, 3, 6, 7, 10], [-3, -5, -8, -11, 2, 4, 7, 10],
    [-2, -6, -8, -10, 1, 5, 7, 9], [-2, -5, -8, -10, 1, 4, 7, 9],
    [-2, -4, -8, -10, 1, 3, 7, 9], [-2, -5, -7, -10, 1, 4, 6, 9],
    [-3, -4, -7, -10, 2, 3, 6, 9], [-1, -2, -3, -10, 0, 1, 2, 9],
    [-4, -6, -8, -9, 3, 5, 7, 8], [-3, -5, -7, -9, 2, 4, 6, 8],
];

fn be64(b: &[u8]) -> u64 {
    let mut v = [0u8; 8];
    v.copy_from_slice(&b[..8]);
    u64::from_be_bytes(v)
}

/// Bits `hi..=lo` of a block.
fn f(b: u64, hi: u32, lo: u32) -> i32 {
    (b >> lo) as i32 & ((1 << (hi - lo + 1)) - 1)
}

fn add(c: [i32; 3], d: i32) -> [u8; 4] {
    let s = |v: i32| (v + d).clamp(0, 255) as u8;
    [s(c[0]), s(c[1]), s(c[2]), 255]
}

fn x4(c: [i32; 3]) -> [i32; 3] {
    c.map(|v| v * 17)
}

fn x5(v: i32) -> i32 {
    v << 3 | v >> 2
}

/// The colour block. `punch`: one bit of alpha, bit 33 says whether the block is opaque.
/// The result is row by row.
fn color(b: u64, punch: bool) -> [[u8; 4]; 16] {
    let opaque = !punch || f(b, 33, 33) != 0;
    // 2-bit index of the texel in column x, row y.
    let index = |x: usize, y: usize| {
        let i = x * 4 + y;
        ((b >> (i + 16) & 1) << 1 | (b >> i & 1)) as usize
    };
    let paint = |pal: [[u8; 4]; 4]| -> [[u8; 4]; 16] {
        let mut pal = pal;
        if !opaque {
            pal[2] = [0; 4];
        }
        std::array::from_fn(|i| pal[index(i & 3, i >> 2)])
    };

    let (c0, c1);
    if punch || f(b, 33, 33) != 0 {
        let base = [f(b, 63, 59), f(b, 55, 51), f(b, 47, 43)];
        let delta = [f(b, 58, 56), f(b, 50, 48), f(b, 42, 40)].map(|v| (v << 29) >> 29);
        let second: [i32; 3] = std::array::from_fn(|i| base[i] + delta[i]);
        let out = |v: i32| !(0..32).contains(&v);
        if out(second[0]) {
            // T mode.
            let a = x4([f(b, 60, 59) << 2 | f(b, 57, 56), f(b, 55, 52), f(b, 51, 48)]);
            let c = x4([f(b, 47, 44), f(b, 43, 40), f(b, 39, 36)]);
            let d = DISTANCE[(f(b, 35, 34) << 1 | f(b, 32, 32)) as usize];
            return paint([add(a, 0), add(c, d), add(c, 0), add(c, -d)]);
        }
        if out(second[1]) {
            // H mode; the order of the colours is a bit of the distance.
            let a4 = [f(b, 62, 59), f(b, 58, 56) << 1 | f(b, 52, 52), f(b, 51, 51) << 3 | f(b, 49, 47)];
            let c4 = [f(b, 46, 43), f(b, 42, 39), f(b, 38, 35)];
            let order = |c: [i32; 3]| c[0] << 8 | c[1] << 4 | c[2];
            let lsb = (order(a4) >= order(c4)) as i32;
            let d = DISTANCE[(f(b, 34, 34) << 2 | f(b, 32, 32) << 1 | lsb) as usize];
            let (a, c) = (x4(a4), x4(c4));
            return paint([add(a, d), add(a, -d), add(c, d), add(c, -d)]);
        }
        if out(second[2]) {
            // Planar mode, always opaque.
            let x6 = |v: i32| v << 2 | v >> 4;
            let x7 = |v: i32| v << 1 | v >> 6;
            let o = [
                x6(f(b, 62, 57)),
                x7(f(b, 56, 56) << 6 | f(b, 54, 49)),
                x6(f(b, 48, 48) << 5 | f(b, 44, 43) << 3 | f(b, 41, 39)),
            ];
            let h = [x6(f(b, 38, 34) << 1 | f(b, 32, 32)), x7(f(b, 31, 25)), x6(f(b, 24, 19))];
            let v = [x6(f(b, 18, 13)), x7(f(b, 12, 6)), x6(f(b, 5, 0))];
            return std::array::from_fn(|i| {
                let (x, y) = (i as i32 & 3, i as i32 >> 2);
                let c = |k: usize| {
                    ((x * (h[k] - o[k]) + y * (v[k] - o[k]) + 4 * o[k] + 2) >> 2).clamp(0, 255) as u8
                };
                [c(0), c(1), c(2), 255]
            });
        }
        c0 = base.map(x5);
        c1 = second.map(x5);
    } else {
        c0 = x4([f(b, 63, 60), f(b, 55, 52), f(b, 47, 44)]);
        c1 = x4([f(b, 59, 56), f(b, 51, 48), f(b, 43, 40)]);
    }
    let tables = [MODIFIER[f(b, 39, 37) as usize], MODIFIER[f(b, 36, 34) as usize]];
    let flip = f(b, 32, 32) != 0;
    std::array::from_fn(|i| {
        let (x, y) = (i & 3, i >> 2);
        let half = if flip { y >> 1 } else { x >> 1 };
        let (c, t) = (if half == 0 { c0 } else { c1 }, tables[half]);
        let idx = index(x, y);
        let small = if opaque { t[0] } else { 0 };
        match idx {
            0 => add(c, small),
            1 => add(c, t[1]),
            2 if !opaque => [0; 4],
            2 => add(c, -small),
            _ => add(c, -t[1]),
        }
    })
}

/// EAC modifiers: base, multiplier and the 16 table entries, row by row.
fn eac(b: u64) -> (i32, i32, [i32; 16]) {
    let t = &EAC[f(b, 51, 48) as usize];
    let m = std::array::from_fn(|i| {
        let n = (i & 3) * 4 + (i >> 2);
        t[(b >> (45 - 3 * n)) as usize & 7]
    });
    (f(b, 63, 56), f(b, 55, 52), m)
}

fn eac11(b: u64, signed: bool) -> [f32; 16] {
    let (base, mul, m) = eac(b);
    let scale = |m: i32| if mul == 0 { m } else { m * mul * 8 };
    if signed {
        let base = (base as u8 as i8 as i32).max(-127) * 8;
        m.map(|m| (base + scale(m)).clamp(-1023, 1023) as f32 / 1023.0)
    } else {
        m.map(|m| (base * 8 + 4 + scale(m)).clamp(0, 2047) as f32 / 2047.0)
    }
}

pub fn etc2_rgb(b: &[u8], srgb: bool) -> Texels {
    color(be64(b), false).map(|c| rgba8(c, srgb))
}

pub fn etc2_rgb8a1(b: &[u8], srgb: bool) -> Texels {
    color(be64(b), true).map(|c| rgba8(c, srgb))
}

pub fn etc2_rgba(b: &[u8], srgb: bool) -> Texels {
    let (base, mul, m) = eac(be64(b));
    let mut t = color(be64(&b[8..]), false);
    for (c, m) in t.iter_mut().zip(m) {
        c[3] = (base + m * mul).clamp(0, 255) as u8;
    }
    t.map(|c| rgba8(c, srgb))
}

pub fn eac_r(b: &[u8], signed: bool) -> Texels {
    eac11(be64(b), signed).map(|r| [r, 0.0, 0.0, 1.0])
}

pub fn eac_rg(b: &[u8], signed: bool) -> Texels {
    let (r, g) = (eac11(be64(b), signed), eac11(be64(&b[8..]), signed));
    std::array::from_fn(|i| [r[i], g[i], 0.0, 1.0])
}
