//! Pixel formats: the TMU's texture data formats (decoded to f32 or integer channels) and
//! the tile buffer's internal types / output image formats used by loads and stores.

use crate::qpu::{f16_to_f32, f32_to_f16};

/// How a texture data format is stored.
#[derive(Clone, Copy, Debug)]
pub struct TexFmt {
    /// bytes per texel, or per block for compressed formats
    pub cpp: u32,
    pub bw: u32,
    pub bh: u32,
    /// returns integer channels (no filtering, no normalisation)
    pub int: bool,
    pub depth: bool,
}

pub fn tex_fmt(t: u32) -> TexFmt {
    let f = |cpp, int, depth| TexFmt { cpp, bw: 1, bh: 1, int, depth };
    let c = |cpp| TexFmt { cpp, bw: 4, bh: 4, int: false, depth: false };
    match t {
        0 | 1 | 25 | 26 => f(1, false, false),
        27 => f(1, true, false),
        2 | 3 | 6 | 7 | 8 | 10 | 11 | 16 | 115 => f(2, false, false),
        28 => f(2, true, false),
        21 => f(2, false, true),
        4 | 5 | 9 | 12 | 13 | 17 | 19 | 20 | 29 => f(4, false, false),
        22 | 23 | 24 => f(4, false, true),
        14 | 15 | 18 | 30 => f(8, false, false),
        31 => f(16, false, false),
        32 | 33 | 34 | 35 | 48 => c(8),
        36 | 37 | 38 | 49 | 50 => c(16),
        64..=77 => {
            const B: [(u32, u32); 14] = [(4, 4), (5, 4), (5, 5), (6, 5), (6, 6), (8, 5), (8, 6), (8, 8), (10, 5), (10, 6), (10, 8), (10, 10), (12, 10), (12, 12)];
            let (bw, bh) = B[(t - 64) as usize];
            TexFmt { cpp: 16, bw, bh, int: false, depth: false }
        }
        96 | 97 => f(1, true, false),
        98 | 99 | 102 | 103 => f(2, true, false),
        100 | 101 | 104 | 105 | 108 | 109 | 114 => f(4, true, false),
        106 | 107 | 110 | 111 => f(8, true, false),
        112 | 113 => f(16, true, false),
        _ => f(4, false, false),
    }
}

#[inline] fn unorm(v: u32, bits: u32) -> f32 { v as f32 / ((1u64 << bits) - 1) as f32 }
#[inline] fn snorm(v: u32, bits: u32) -> f32 {
    let s = ((v << (32 - bits)) as i32) >> (32 - bits);
    (s as f32 / ((1u32 << (bits - 1)) - 1) as f32).max(-1.0)
}
#[inline] fn sext(v: u32, bits: u32) -> u32 { (((v << (32 - bits)) as i32) >> (32 - bits)) as u32 }

/// Unsigned float with `m` mantissa bits and a 5-bit exponent (R11F/G11F/B10F).
fn uf(v: u32, m: u32) -> f32 {
    let e = (v >> m) & 0x1f;
    let mant = v & ((1 << m) - 1);
    if e == 0 { mant as f32 / (1 << m) as f32 * 2f32.powi(-14) }
    else if e == 31 { if mant == 0 { f32::INFINITY } else { f32::NAN } }
    else { (1.0 + mant as f32 / (1 << m) as f32) * 2f32.powi(e as i32 - 15) }
}

/// A decoded texel: f32 channels (normalised/float/depth formats) or 32-bit integers.
#[derive(Clone, Copy, Debug, Default)]
pub struct Texel { pub v: [u32; 4], pub int: bool }

impl Texel {
    pub fn f(r: f32, g: f32, b: f32, a: f32) -> Texel { Texel { v: [r.to_bits(), g.to_bits(), b.to_bits(), a.to_bits()], int: false } }
    pub fn i(r: u32, g: u32, b: u32, a: u32) -> Texel { Texel { v: [r, g, b, a], int: true } }
    pub fn ch(&self, k: usize) -> f32 { f32::from_bits(self.v[k]) }
}

/// Decode the texel at byte slice `p` (cpp bytes) of an uncompressed format.
pub fn decode_texel(t: u32, p: &[u8]) -> Texel {
    let b = |k: usize| *p.get(k).unwrap_or(&0) as u32;
    let u16_ = |k: usize| b(k) | b(k + 1) << 8;
    let u32_ = |k: usize| u16_(k) | u16_(k + 2) << 16;
    let fl = |k: usize| f32::from_bits(u32_(k));
    let h = |k: usize| f16_to_f32(u16_(k) as u16);
    match t {
        0 => Texel::f(unorm(b(0), 8), 0.0, 0.0, 1.0),
        1 => Texel::f(snorm(b(0), 8), 0.0, 0.0, 1.0),
        2 => Texel::f(unorm(b(0), 8), unorm(b(1), 8), 0.0, 1.0),
        3 => Texel::f(snorm(b(0), 8), snorm(b(1), 8), 0.0, 1.0),
        4 => Texel::f(unorm(b(0), 8), unorm(b(1), 8), unorm(b(2), 8), unorm(b(3), 8)),
        5 => Texel::f(snorm(b(0), 8), snorm(b(1), 8), snorm(b(2), 8), snorm(b(3), 8)),
        6 => { let v = u16_(0); Texel::f(unorm(v >> 11, 5), unorm((v >> 5) & 63, 6), unorm(v & 31, 5), 1.0) }
        7 => { let v = u16_(0); Texel::f(unorm(v >> 12, 4), unorm((v >> 8) & 15, 4), unorm((v >> 4) & 15, 4), unorm(v & 15, 4)) }
        8 => { let v = u16_(0); Texel::f(unorm(v >> 11, 5), unorm((v >> 6) & 31, 5), unorm((v >> 1) & 31, 5), (v & 1) as f32) }
        115 => { let v = u16_(0); Texel::f(unorm((v >> 10) & 31, 5), unorm((v >> 5) & 31, 5), unorm(v & 31, 5), (v >> 15) as f32) }
        9 => { let v = u32_(0); Texel::f(unorm(v & 1023, 10), unorm((v >> 10) & 1023, 10), unorm((v >> 20) & 1023, 10), unorm(v >> 30, 2)) }
        10 => Texel::f(unorm(u16_(0), 16), 0.0, 0.0, 1.0),
        11 => Texel::f(snorm(u16_(0), 16), 0.0, 0.0, 1.0),
        12 => Texel::f(unorm(u16_(0), 16), unorm(u16_(2), 16), 0.0, 1.0),
        13 => Texel::f(snorm(u16_(0), 16), snorm(u16_(2), 16), 0.0, 1.0),
        14 => Texel::f(unorm(u16_(0), 16), unorm(u16_(2), 16), unorm(u16_(4), 16), unorm(u16_(6), 16)),
        15 => Texel::f(snorm(u16_(0), 16), snorm(u16_(2), 16), snorm(u16_(4), 16), snorm(u16_(6), 16)),
        16 => Texel::f(h(0), 0.0, 0.0, 1.0),
        17 => Texel::f(h(0), h(2), 0.0, 1.0),
        18 => Texel::f(h(0), h(2), h(4), h(6)),
        19 => { let v = u32_(0); Texel::f(uf(v & 0x7ff, 6), uf((v >> 11) & 0x7ff, 6), uf(v >> 22, 5), 1.0) }
        20 => {
            let v = u32_(0);
            let e = (v >> 27) as i32;
            let s = 2f32.powi(e - 15 - 9);
            Texel::f((v & 511) as f32 * s, ((v >> 9) & 511) as f32 * s, ((v >> 18) & 511) as f32 * s, 1.0)
        }
        21 => { let d = unorm(u16_(0), 16); Texel::f(d, d, d, 1.0) }
        22 => { let d = unorm(u32_(0) & 0xff_ffff, 24); Texel::f(d, d, d, 1.0) }
        23 => { let d = fl(0); Texel::f(d, d, d, 1.0) }
        24 => { let d = unorm(u32_(0) >> 8, 24); Texel::f(d, d, d, 1.0) }
        25 => Texel::f(unorm(b(0) & 15, 4), 0.0, 0.0, 1.0),
        26 => Texel::f((b(0) & 1) as f32, 0.0, 0.0, 1.0),
        27 => Texel::i(b(0), 0, 0, 1),
        28 => Texel::i(u16_(0), 0, 0, 1),
        29 => Texel::f(fl(0), 0.0, 0.0, 1.0),
        30 => Texel::f(fl(0), fl(4), 0.0, 1.0),
        31 => Texel::f(fl(0), fl(4), fl(8), fl(12)),
        96 => Texel::i(sext(b(0), 8), 0, 0, 1),
        97 => Texel::i(b(0), 0, 0, 1),
        98 => Texel::i(sext(b(0), 8), sext(b(1), 8), 0, 1),
        99 => Texel::i(b(0), b(1), 0, 1),
        100 => Texel::i(sext(b(0), 8), sext(b(1), 8), sext(b(2), 8), sext(b(3), 8)),
        101 => Texel::i(b(0), b(1), b(2), b(3)),
        102 => Texel::i(sext(u16_(0), 16), 0, 0, 1),
        103 => Texel::i(u16_(0), 0, 0, 1),
        104 => Texel::i(sext(u16_(0), 16), sext(u16_(2), 16), 0, 1),
        105 => Texel::i(u16_(0), u16_(2), 0, 1),
        106 => Texel::i(sext(u16_(0), 16), sext(u16_(2), 16), sext(u16_(4), 16), sext(u16_(6), 16)),
        107 => Texel::i(u16_(0), u16_(2), u16_(4), u16_(6)),
        108 | 109 => Texel::i(u32_(0), 0, 0, 1),
        110 | 111 => Texel::i(u32_(0), u32_(4), 0, 1),
        112 | 113 => Texel::i(u32_(0), u32_(4), u32_(8), u32_(12)),
        114 => { let v = u32_(0); Texel::i(v & 1023, (v >> 10) & 1023, (v >> 20) & 1023, v >> 30) }
        _ => { crate::log_once(&format!("v3d: texture data format {t} not modelled")); Texel::f(1.0, 0.0, 1.0, 1.0) }
    }
}

pub fn srgb_to_linear(c: f32) -> f32 { if c <= 0.04045 { c / 12.92 } else { ((c + 0.055) / 1.055).powf(2.4) } }
pub fn linear_to_srgb(c: f32) -> f32 {
    let c = c.clamp(0.0, 1.0);
    if c <= 0.003_130_8 { c * 12.92 } else { 1.055 * c.powf(1.0 / 2.4) - 0.055 }
}

// ------------------------------------------------------------------------- tile buffer

/// Internal types of a render target in the tile buffer.
pub mod it {
    pub const I8: u32 = 0;
    pub const UI8: u32 = 1;
    pub const UNORM8: u32 = 2;
    pub const I16: u32 = 4;
    pub const UI16: u32 = 5;
    pub const F16: u32 = 6;
    pub const I32: u32 = 8;
    pub const UI32: u32 = 9;
    pub const F32: u32 = 10;
}

pub fn internal_is_int(t: u32) -> bool { matches!(t, it::I8 | it::UI8 | it::I16 | it::UI16 | it::I32 | it::UI32) }

/// Store a channel value into the tile buffer's internal precision (floats as f32 bits).
pub fn quantize(t: u32, v: u32) -> u32 {
    match t {
        it::UNORM8 => { let f = f32::from_bits(v); let f = if f.is_nan() { 0.0 } else { f.clamp(0.0, 1.0) }; ((f * 255.0).round() / 255.0).to_bits() }
        it::F16 => f16_to_f32(f32_to_f16(f32::from_bits(v))).to_bits(),
        it::I8 => (v as i32).clamp(-128, 127) as u32,
        it::UI8 => v.min(255),
        it::I16 => (v as i32).clamp(-32768, 32767) as u32,
        it::UI16 => v.min(65535),
        _ => v,
    }
}

/// Bytes per pixel of an output image format in memory (0 = not modelled).
pub fn out_cpp(f: u32) -> u32 {
    match f {
        0 | 2 | 3 | 8 | 11 | 14 | 17 | 19 | 22 | 25 | 27 | 40 | 41 | 43 => 4,
        4 | 5 | 6 | 7 | 20 | 23 | 26 | 29 | 32 | 35 | 42 | 45 => 2,
        9 | 12 | 15 => 16,
        10 | 13 | 16 | 18 | 21 | 24 => 8,
        1 | 28 => 3,
        30 | 33 | 36 | 44 => 1,
        31 | 34 => 4,
        _ => 0,
    }
}

#[inline] fn to_unorm(f: f32, bits: u32) -> u32 { let m = ((1u64 << bits) - 1) as f32; (if f.is_nan() { 0.0 } else { f.clamp(0.0, 1.0) } * m).round() as u32 }
fn to_uf(f: f32, m: u32) -> u32 {
    if !(f > 0.0) { return 0; }
    let h = f32_to_f16(f) as u32; // reuse half: 5-bit exponent, 10-bit mantissa
    let e = (h >> 10) & 0x1f;
    let mant = (h & 0x3ff) >> (10 - m);
    (e << m) | mant
}

/// Encode a tile-buffer pixel (4 channels, f32 bits or ints) into `out` in output format `f`.
pub fn encode(f: u32, c: [u32; 4], out: &mut [u8]) {
    let fl = |k: usize| f32::from_bits(c[k]);
    let mut put = |bytes: &[u8]| { let n = bytes.len().min(out.len()); out[..n].copy_from_slice(&bytes[..n]); };
    match f {
        0 => put(&[to_unorm(linear_to_srgb(fl(0)), 8) as u8, to_unorm(linear_to_srgb(fl(1)), 8) as u8, to_unorm(linear_to_srgb(fl(2)), 8) as u8, to_unorm(fl(3), 8) as u8]),
        1 => put(&[to_unorm(linear_to_srgb(fl(0)), 8) as u8, to_unorm(linear_to_srgb(fl(1)), 8) as u8, to_unorm(linear_to_srgb(fl(2)), 8) as u8]),
        27 => put(&[to_unorm(fl(0), 8) as u8, to_unorm(fl(1), 8) as u8, to_unorm(fl(2), 8) as u8, to_unorm(fl(3), 8) as u8]),
        28 => put(&[to_unorm(fl(0), 8) as u8, to_unorm(fl(1), 8) as u8, to_unorm(fl(2), 8) as u8]),
        29 => put(&[to_unorm(fl(0), 8) as u8, to_unorm(fl(1), 8) as u8]),
        30 => put(&[to_unorm(fl(0), 8) as u8]),
        2 => put(&((c[0] & 1023) | (c[1] & 1023) << 10 | (c[2] & 1023) << 20 | (c[3] & 3) << 30).to_le_bytes()),
        3 => put(&(to_unorm(fl(0), 10) | to_unorm(fl(1), 10) << 10 | to_unorm(fl(2), 10) << 20 | to_unorm(fl(3), 2) << 30).to_le_bytes()),
        4 | 5 => put(&((to_unorm(fl(3), 1) | to_unorm(fl(2), 5) << 1 | to_unorm(fl(1), 5) << 6 | to_unorm(fl(0), 5) << 11) as u16).to_le_bytes()),
        6 => put(&((to_unorm(fl(3), 4) | to_unorm(fl(2), 4) << 4 | to_unorm(fl(1), 4) << 8 | to_unorm(fl(0), 4) << 12) as u16).to_le_bytes()),
        7 => put(&((to_unorm(fl(2), 5) | to_unorm(fl(1), 6) << 5 | to_unorm(fl(0), 5) << 11) as u16).to_le_bytes()),
        45 => put(&((to_unorm(fl(0), 5) | to_unorm(fl(1), 5) << 5 | to_unorm(fl(2), 5) << 10 | to_unorm(fl(3), 1) << 15) as u16).to_le_bytes()),
        8 => put(&(to_uf(fl(0), 6) | to_uf(fl(1), 6) << 11 | to_uf(fl(2), 5) << 22).to_le_bytes()),
        9 | 12 | 15 => { let mut b = [0u8; 16]; for k in 0..4 { b[k * 4..k * 4 + 4].copy_from_slice(&c[k].to_le_bytes()); } put(&b) }
        10 | 13 | 16 => { let mut b = [0u8; 8]; for k in 0..2 { b[k * 4..k * 4 + 4].copy_from_slice(&c[k].to_le_bytes()); } put(&b) }
        11 | 14 | 17 => put(&c[0].to_le_bytes()),
        18 | 19 | 20 => { let n = [4usize, 2, 1][(f - 18) as usize]; let mut b = [0u8; 8]; for k in 0..n { b[k * 2..k * 2 + 2].copy_from_slice(&f32_to_f16(fl(k)).to_le_bytes()); } put(&b[..n * 2]) }
        21..=26 => { let n = [4usize, 2, 1][((f - 21) % 3) as usize]; let mut b = [0u8; 8]; for k in 0..n { b[k * 2..k * 2 + 2].copy_from_slice(&(c[k] as u16).to_le_bytes()); } put(&b[..n * 2]) }
        31..=36 => { let n = [4usize, 2, 1][((f - 31) % 3) as usize]; let b: Vec<u8> = (0..n).map(|k| c[k] as u8).collect(); put(&b) }
        _ => crate::log_once(&format!("v3d: store to output image format {f} not modelled")),
    }
}

/// Decode a pixel of output format `f` into tile-buffer channels.
pub fn decode(f: u32, p: &[u8]) -> [u32; 4] {
    let b = |k: usize| *p.get(k).unwrap_or(&0) as u32;
    let u16_ = |k: usize| b(k) | b(k + 1) << 8;
    let u32_ = |k: usize| u16_(k) | u16_(k + 2) << 16;
    let fb = |v: f32| v.to_bits();
    let one = 1f32.to_bits();
    match f {
        0 => [fb(srgb_to_linear(unorm(b(0), 8))), fb(srgb_to_linear(unorm(b(1), 8))), fb(srgb_to_linear(unorm(b(2), 8))), fb(unorm(b(3), 8))],
        1 => [fb(srgb_to_linear(unorm(b(0), 8))), fb(srgb_to_linear(unorm(b(1), 8))), fb(srgb_to_linear(unorm(b(2), 8))), one],
        27 => [fb(unorm(b(0), 8)), fb(unorm(b(1), 8)), fb(unorm(b(2), 8)), fb(unorm(b(3), 8))],
        28 => [fb(unorm(b(0), 8)), fb(unorm(b(1), 8)), fb(unorm(b(2), 8)), one],
        29 => [fb(unorm(b(0), 8)), fb(unorm(b(1), 8)), 0, one],
        30 => [fb(unorm(b(0), 8)), 0, 0, one],
        2 => { let v = u32_(0); [v & 1023, (v >> 10) & 1023, (v >> 20) & 1023, v >> 30] }
        3 => { let v = u32_(0); [fb(unorm(v & 1023, 10)), fb(unorm((v >> 10) & 1023, 10)), fb(unorm((v >> 20) & 1023, 10)), fb(unorm(v >> 30, 2))] }
        4 | 5 => { let v = u16_(0); [fb(unorm(v >> 11, 5)), fb(unorm((v >> 6) & 31, 5)), fb(unorm((v >> 1) & 31, 5)), fb((v & 1) as f32)] }
        6 => { let v = u16_(0); [fb(unorm(v >> 12, 4)), fb(unorm((v >> 8) & 15, 4)), fb(unorm((v >> 4) & 15, 4)), fb(unorm(v & 15, 4))] }
        7 => { let v = u16_(0); [fb(unorm(v >> 11, 5)), fb(unorm((v >> 5) & 63, 6)), fb(unorm(v & 31, 5)), one] }
        45 => { let v = u16_(0); [fb(unorm(v & 31, 5)), fb(unorm((v >> 5) & 31, 5)), fb(unorm((v >> 10) & 31, 5)), fb((v >> 15) as f32)] }
        8 => { let v = u32_(0); [fb(uf(v & 0x7ff, 6)), fb(uf((v >> 11) & 0x7ff, 6)), fb(uf(v >> 22, 5)), one] }
        9 | 12 | 15 => [u32_(0), u32_(4), u32_(8), u32_(12)],
        10 | 13 | 16 => [u32_(0), u32_(4), 0, if f == 10 { one } else { 1 }],
        11 | 14 | 17 => [u32_(0), 0, 0, if f == 11 { one } else { 1 }],
        18 => [fb(f16_to_f32(u16_(0) as u16)), fb(f16_to_f32(u16_(2) as u16)), fb(f16_to_f32(u16_(4) as u16)), fb(f16_to_f32(u16_(6) as u16))],
        19 => [fb(f16_to_f32(u16_(0) as u16)), fb(f16_to_f32(u16_(2) as u16)), 0, one],
        20 => [fb(f16_to_f32(u16_(0) as u16)), 0, 0, one],
        21 | 24 => [u16_(0), u16_(2), u16_(4), u16_(6)],
        22 | 25 => [u16_(0), u16_(2), 0, 1],
        23 | 26 => [u16_(0), 0, 0, 1],
        31 | 34 => [b(0), b(1), b(2), b(3)],
        32 | 35 => [b(0), b(1), 0, 1],
        33 | 36 => [b(0), 0, 0, 1],
        _ => { crate::log_once(&format!("v3d: load from output image format {f} not modelled")); [0; 4] }
    }
}

/// Encode f32 channels as a texel of an uncompressed texture data format (for TFU mipmaps).
pub fn encode_texel(t: u32, c: [f32; 4], out: &mut [u8]) {
    let un = |f: f32, bits: u32| to_unorm(f, bits);
    let sn = |f: f32, bits: u32| { let m = ((1u32 << (bits - 1)) - 1) as f32; ((if f.is_nan() { 0.0 } else { f.clamp(-1.0, 1.0) } * m).round() as i32) as u32 };
    let mut put = |b: &[u8]| { let n = b.len().min(out.len()); out[..n].copy_from_slice(&b[..n]); };
    match t {
        0 => put(&[un(c[0], 8) as u8]),
        1 => put(&[sn(c[0], 8) as u8]),
        2 => put(&[un(c[0], 8) as u8, un(c[1], 8) as u8]),
        3 => put(&[sn(c[0], 8) as u8, sn(c[1], 8) as u8]),
        4 => put(&[un(c[0], 8) as u8, un(c[1], 8) as u8, un(c[2], 8) as u8, un(c[3], 8) as u8]),
        5 => put(&[sn(c[0], 8) as u8, sn(c[1], 8) as u8, sn(c[2], 8) as u8, sn(c[3], 8) as u8]),
        6 => put(&((un(c[2], 5) | un(c[1], 6) << 5 | un(c[0], 5) << 11) as u16).to_le_bytes()),
        7 => put(&((un(c[3], 4) | un(c[2], 4) << 4 | un(c[1], 4) << 8 | un(c[0], 4) << 12) as u16).to_le_bytes()),
        8 => put(&((un(c[3], 1) | un(c[2], 5) << 1 | un(c[1], 5) << 6 | un(c[0], 5) << 11) as u16).to_le_bytes()),
        9 => put(&(un(c[0], 10) | un(c[1], 10) << 10 | un(c[2], 10) << 20 | un(c[3], 2) << 30).to_le_bytes()),
        10 | 12 | 14 => { let n = [1usize, 2, 4][((t - 10) / 2) as usize]; let b: Vec<u8> = (0..n).flat_map(|k| (un(c[k], 16) as u16).to_le_bytes()).collect(); put(&b) }
        11 | 13 | 15 => { let n = [1usize, 2, 4][((t - 11) / 2) as usize]; let b: Vec<u8> = (0..n).flat_map(|k| (sn(c[k], 16) as u16).to_le_bytes()).collect(); put(&b) }
        16 | 17 | 18 => { let n = [1usize, 2, 4][(t - 16) as usize]; let b: Vec<u8> = (0..n).flat_map(|k| f32_to_f16(c[k]).to_le_bytes()).collect(); put(&b) }
        19 => put(&(to_uf(c[0], 6) | to_uf(c[1], 6) << 11 | to_uf(c[2], 5) << 22).to_le_bytes()),
        25 => put(&[un(c[0], 4) as u8]),
        29 => put(&c[0].to_bits().to_le_bytes()),
        30 => { let mut b = [0u8; 8]; for k in 0..2 { b[k * 4..k * 4 + 4].copy_from_slice(&c[k].to_bits().to_le_bytes()); } put(&b) }
        31 => { let mut b = [0u8; 16]; for k in 0..4 { b[k * 4..k * 4 + 4].copy_from_slice(&c[k].to_bits().to_le_bytes()); } put(&b) }
        _ => crate::log_once(&format!("v3d: TFU encode of texture format {t} not modelled")),
    }
}

/// Decode texel (x, y) of a compressed block (ETC2/EAC, BC1-3); ASTC is not modelled.
pub fn decode_block(t: u32, b: &[u8], x: u32, y: u32) -> Texel {
    match t {
        32 | 33 | 38 => {
            let (rgb_block, alpha) = if t == 38 { (&b[8..16], Some(eac(&b[0..8], x, y, false, 8))) } else { (&b[0..8], None) };
            let (r, g, bb, a) = etc2_rgb(rgb_block, x, y, t == 33);
            let a = alpha.map_or(a, |v| v);
            Texel::f(r, g, bb, a)
        }
        34 | 35 => Texel::f(eac(&b[0..8], x, y, t == 35, 11), 0.0, 0.0, 1.0),
        36 | 37 => Texel::f(eac(&b[0..8], x, y, t == 37, 11), eac(&b[8..16], x, y, t == 37, 11), 0.0, 1.0),
        48 | 49 | 50 => {
            let (cb, ab) = if t == 48 { (&b[0..8], None) } else { (&b[8..16], Some(&b[0..8])) };
            let (r, g, bl, mut a) = bc1(cb, x, y, t == 48);
            if let Some(ab) = ab {
                a = if t == 49 { ((ab[(y * 2 + x / 2) as usize] >> (4 * (x & 1))) & 15) as f32 / 15.0 } else { bc3_alpha(ab, x, y) };
            }
            Texel::f(r, g, bl, a)
        }
        _ => { crate::log_once(&format!("v3d: compressed texture format {t} not modelled")); Texel::f(1.0, 0.0, 1.0, 1.0) }
    }
}

fn bc1(b: &[u8], x: u32, y: u32, alpha_mode: bool) -> (f32, f32, f32, f32) {
    let c0 = u16::from_le_bytes([b[0], b[1]]) as u32;
    let c1 = u16::from_le_bytes([b[2], b[3]]) as u32;
    let rgb = |c: u32| [((c >> 11) & 31) as f32 / 31.0, ((c >> 5) & 63) as f32 / 63.0, (c & 31) as f32 / 31.0];
    let (p0, p1) = (rgb(c0), rgb(c1));
    let idx = (u32::from_le_bytes([b[4], b[5], b[6], b[7]]) >> (2 * (y * 4 + x))) & 3;
    let mix = |a: f32, w: f32| a * w;
    let c = if c0 > c1 || !alpha_mode {
        match idx { 0 => (p0, 1.0), 1 => (p1, 1.0),
            2 => ([mix(p0[0], 2.0 / 3.0) + mix(p1[0], 1.0 / 3.0), mix(p0[1], 2.0 / 3.0) + mix(p1[1], 1.0 / 3.0), mix(p0[2], 2.0 / 3.0) + mix(p1[2], 1.0 / 3.0)], 1.0),
            _ => ([mix(p0[0], 1.0 / 3.0) + mix(p1[0], 2.0 / 3.0), mix(p0[1], 1.0 / 3.0) + mix(p1[1], 2.0 / 3.0), mix(p0[2], 1.0 / 3.0) + mix(p1[2], 2.0 / 3.0)], 1.0) }
    } else {
        match idx { 0 => (p0, 1.0), 1 => (p1, 1.0), 2 => ([(p0[0] + p1[0]) / 2.0, (p0[1] + p1[1]) / 2.0, (p0[2] + p1[2]) / 2.0], 1.0), _ => ([0.0; 3], 0.0) }
    };
    (c.0[0], c.0[1], c.0[2], c.1)
}

fn bc3_alpha(b: &[u8], x: u32, y: u32) -> f32 {
    let (a0, a1) = (b[0] as f32 / 255.0, b[1] as f32 / 255.0);
    let bits = u64::from_le_bytes([b[2], b[3], b[4], b[5], b[6], b[7], 0, 0]);
    let i = ((bits >> (3 * (y * 4 + x))) & 7) as f32;
    if b[0] > b[1] {
        match i as u32 { 0 => a0, 1 => a1, k => ((8 - k) as f32 * a0 + (k - 1) as f32 * a1) / 7.0 }
    } else {
        match i as u32 { 0 => a0, 1 => a1, 6 => 0.0, 7 => 1.0, k => ((6 - k) as f32 * a0 + (k - 1) as f32 * a1) / 5.0 }
    }
}

/// ETC1/ETC2 RGB block (big-endian 64 bits), texel (x, y); `punch` for RGB8 punchthrough alpha.
fn etc2_rgb(b: &[u8], x: u32, y: u32, punch: bool) -> (f32, f32, f32, f32) {
    let w = u64::from_be_bytes(b[0..8].try_into().unwrap());
    let hi = (w >> 32) as u32;
    let lo = w as u32;
    let k = x * 4 + y;
    let msb = (lo >> (16 + k)) & 1;
    let lsb = (lo >> k) & 1;
    let idx = (msb << 1) | lsb;
    let diff = hi & 2 != 0;
    let flip = hi & 1 != 0;
    let ext4 = |v: u32| (v << 4) | v;
    let ext5 = |v: u32| (v << 3) | (v >> 2);
    let clamp = |v: i32| v.clamp(0, 255) as f32 / 255.0;
    const MOD: [[i32; 4]; 8] = [[2, 8, -2, -8], [5, 17, -5, -17], [9, 29, -9, -29], [13, 42, -13, -42], [18, 60, -18, -60], [24, 80, -24, -80], [33, 106, -33, -106], [47, 183, -47, -183]];
    let opaque = !punch || diff;
    if !diff && !punch || (punch && !diff && false) {
        // individual mode
        let (r1, r2) = (ext4(hi >> 28), ext4((hi >> 24) & 15));
        let (g1, g2) = (ext4((hi >> 20) & 15), ext4((hi >> 16) & 15));
        let (b1, b2) = (ext4((hi >> 12) & 15), ext4((hi >> 8) & 15));
        let second = if flip { y >= 2 } else { x >= 2 };
        let (r, g, bb) = if second { (r2, g2, b2) } else { (r1, g1, b1) };
        let tbl = if second { (hi >> 2) & 7 } else { (hi >> 5) & 7 };
        let m = MOD[tbl as usize][idx as usize];
        return (clamp(r as i32 + m), clamp(g as i32 + m), clamp(bb as i32 + m), 1.0);
    }
    let r = (hi >> 27) & 31;
    let dr = (((hi >> 24) & 7) << 29) as i32 >> 29;
    let g = (hi >> 19) & 31;
    let dg = (((hi >> 16) & 7) << 29) as i32 >> 29;
    let bl = (hi >> 11) & 31;
    let db = (((hi >> 8) & 7) << 29) as i32 >> 29;
    let (r2, g2, b2) = (r as i32 + dr, g as i32 + dg, bl as i32 + db);
    if !(0..32).contains(&r2) {
        // T mode
        let r1 = ext4(((hi >> 27) & 3) << 2 | ((hi >> 24) & 3));
        let g1 = ext4((hi >> 20) & 15);
        let b1 = ext4((hi >> 16) & 15);
        let (rr2, gg2, bb2) = (ext4((hi >> 12) & 15), ext4((hi >> 8) & 15), ext4((hi >> 4) & 15));
        let d = [3, 6, 11, 16, 23, 32, 41, 64][(((hi >> 2) & 3) << 1 | (hi & 1)) as usize];
        let p = match idx { 0 => (r1 as i32, g1 as i32, b1 as i32), 1 => (rr2 as i32 + d, gg2 as i32 + d, bb2 as i32 + d), 2 => (rr2 as i32, gg2 as i32, bb2 as i32), _ => (rr2 as i32 - d, gg2 as i32 - d, bb2 as i32 - d) };
        if punch && !opaque && idx == 2 { return (0.0, 0.0, 0.0, 0.0); }
        return (clamp(p.0), clamp(p.1), clamp(p.2), 1.0);
    }
    if !(0..32).contains(&g2) {
        // H mode
        let r1 = ext4((hi >> 27) & 15);
        let g1 = ext4(((hi >> 24) & 7) << 1 | ((hi >> 20) & 1));
        let b1 = ext4(((hi >> 19) & 1) << 3 | ((hi >> 15) & 7));
        let rr2 = ext4((hi >> 11) & 15);
        let gg2 = ext4((hi >> 7) & 15);
        let bb2 = ext4((hi >> 3) & 15);
        let c1 = (r1 << 16) | (g1 << 8) | b1;
        let c2 = (rr2 << 16) | (gg2 << 8) | bb2;
        let di = (((hi >> 2) & 1) << 2) | ((hi & 1) << 1) | (c1 >= c2) as u32;
        let d = [3, 6, 11, 16, 23, 32, 41, 64][di as usize];
        let p = match idx { 0 => (r1 as i32 + d, g1 as i32 + d, b1 as i32 + d), 1 => (r1 as i32 - d, g1 as i32 - d, b1 as i32 - d), 2 => (rr2 as i32 + d, gg2 as i32 + d, bb2 as i32 + d), _ => (rr2 as i32 - d, gg2 as i32 - d, bb2 as i32 - d) };
        if punch && !opaque && idx == 2 { return (0.0, 0.0, 0.0, 0.0); }
        return (clamp(p.0), clamp(p.1), clamp(p.2), 1.0);
    }
    if !(0..32).contains(&b2) {
        // planar mode
        let ro = ext6((hi >> 25) & 63);
        let go = ext7((((hi >> 24) & 1) << 6) | ((hi >> 17) & 63));
        let bo = ext6((((hi >> 16) & 1) << 5) | (((hi >> 11) & 3) << 3) | ((hi >> 7) & 7));
        let rh = ext6((((hi >> 2) & 31) << 1) | (hi & 1));
        let gh = ext7(lo >> 25);
        let bh = ext6((lo >> 19) & 63);
        let rv = ext6((lo >> 13) & 63);
        let gv = ext7((lo >> 6) & 127);
        let bv = ext6(lo & 63);
        let f = |o: u32, h: u32, v: u32| clamp(((x as i32) * (h as i32 - o as i32) + (y as i32) * (v as i32 - o as i32) + 4 * o as i32 + 2) >> 2);
        return (f(ro, rh, rv), f(go, gh, gv), f(bo, bh, bv), 1.0);
    }
    // differential mode
    let second = if flip { y >= 2 } else { x >= 2 };
    let (rr, gg, bbb) = if second { (r2 as u32, g2 as u32, b2 as u32) } else { (r, g, bl) };
    let tbl = if second { (hi >> 2) & 7 } else { (hi >> 5) & 7 };
    if punch && !opaque && idx == 2 { return (0.0, 0.0, 0.0, 0.0); }
    let m = if punch && !opaque && (idx == 0) { 0 } else { MOD[tbl as usize][idx as usize] };
    (clamp(ext5(rr) as i32 + m), clamp(ext5(gg) as i32 + m), clamp(ext5(bbb) as i32 + m), 1.0)
}
fn ext6(v: u32) -> u32 { (v << 2) | (v >> 4) }
fn ext7(v: u32) -> u32 { (v << 1) | (v >> 6) }

/// EAC alpha/R11 block: 8 bits (`bits` = 8, normalised to [0,1]) or 11-bit (signed or not).
fn eac(b: &[u8], x: u32, y: u32, signed: bool, bits: u32) -> f32 {
    let w = u64::from_be_bytes(b[0..8].try_into().unwrap());
    let base = (w >> 56) as u32;
    let mult = ((w >> 52) & 15) as i32;
    let tbl = ((w >> 48) & 15) as usize;
    let k = x * 4 + y;
    let idx = ((w >> (45 - 3 * k)) & 7) as usize;
    const T: [[i32; 8]; 16] = [[-3, -6, -9, -15, 2, 5, 8, 14], [-3, -7, -10, -13, 2, 6, 9, 12], [-2, -5, -8, -13, 1, 4, 7, 12], [-2, -4, -6, -13, 1, 3, 5, 12],
        [-3, -6, -8, -12, 2, 5, 7, 11], [-3, -7, -9, -11, 2, 6, 8, 10], [-4, -7, -8, -11, 3, 6, 7, 10], [-3, -5, -8, -11, 2, 4, 7, 10],
        [-2, -6, -8, -10, 1, 5, 7, 9], [-2, -5, -8, -10, 1, 4, 7, 9], [-2, -4, -8, -10, 1, 3, 7, 9], [-2, -5, -7, -10, 1, 4, 6, 9],
        [-3, -4, -7, -10, 2, 3, 6, 9], [-1, -2, -3, -10, 0, 1, 2, 9], [-4, -6, -8, -9, 3, 5, 7, 8], [-3, -5, -7, -9, 2, 4, 6, 8]];
    let m = T[tbl][idx];
    if bits == 8 { return (base as i32 + m * mult).clamp(0, 255) as f32 / 255.0; }
    if signed {
        let b0 = base as i8 as i32;
        let mul = if mult == 0 { 1 } else { mult * 8 };
        let v = (b0 * 8 + m * mul).clamp(-1023, 1023);
        v as f32 / 1023.0
    } else {
        let mul = if mult == 0 { 1 } else { mult * 8 };
        let v = (base as i32 * 8 + 4 + m * mul).clamp(0, 2047);
        v as f32 / 2047.0
    }
}
