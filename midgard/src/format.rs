//! Mali pixel formats (Mesa `genxml/v5.xml` "Format", packed with a swizzle as a 22-bit
//! "Pixel Format": bits 0-11 the swizzle, 12-19 the format, 20 sRGB).
//!
//! Most formats follow a regular layout: bits 7-5 the type (3 SNORM, 4 UINT, 5 UNORM,
//! 6 SINT), bits 4-3 the channel count minus one, bits 2-0 the channel size (1, 2, 4, 8,
//! 16, 32, 64 bits; 7 is a float type: R64F in the UINT group, R32F in UNORM, R16F in
//! SINT).

use crate::half::{f16_to_f32, f32_to_f16};
use crate::vm::Vm;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PixelFormat {
    pub id: u32,
    pub swizzle: u32,
    pub srgb: bool,
}

impl PixelFormat {
    pub fn from_bits(v: u32) -> Self {
        Self { id: (v >> 12) & 0xff, swizzle: v & 0xfff, srgb: (v >> 20) & 1 != 0 }
    }
}

/// How a format's values reach a shader: normalised/float values as fp32, integers as-is.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Class {
    Float,
    Uint,
    Sint,
}

#[derive(Clone, Copy, Debug)]
#[allow(dead_code)]
enum Layout {
    /// Regular layout: type group, channels, bits per channel (float when `float`).
    Regular { group: u32, chans: u32, bits: u32, float: bool },
    Rgb565,
    Rgb5a1,
    A1bgr5,
    Rgb10a2 { int: Option<bool> },
    Rgba4,
    R11g11b10f,
    Rgb332,
    Z24x8,
    Snap4,
    Constant,
    Unknown,
}

fn layout(id: u32) -> Layout {
    match id {
        0x60..=0xdf => {
            let group = id >> 5;
            let chans = ((id >> 3) & 3) + 1;
            let code = id & 7;
            if code == 7 {
                let bits = match group {
                    4 => 64,
                    5 => 32,
                    6 => 16,
                    _ => return Layout::Unknown,
                };
                Layout::Regular { group, chans, bits, float: true }
            } else {
                Layout::Regular { group, chans, bits: 1 << code, float: false }
            }
        }
        64 => Layout::Rgb565,
        65 => Layout::Rgb5a1,
        66 => Layout::A1bgr5,
        67 | 68 => Layout::Rgb10a2 { int: None },
        71 | 72 => Layout::Rgb10a2 { int: Some(false) },
        73 | 74 => Layout::Rgb10a2 { int: Some(true) },
        75 => Layout::Rgb332,
        77 => Layout::Z24x8,
        89 => Layout::R11g11b10f,
        93 | 94 => Layout::Snap4,
        95 => Layout::Constant,
        // RGBA4 is RGBA4 UNORM in the regular table (186); 0x?? kept for completeness
        _ => Layout::Unknown,
    }
}

impl PixelFormat {
    pub fn class(&self) -> Class {
        match layout(self.id) {
            Layout::Regular { group: 4, float: false, .. } => Class::Uint,
            Layout::Regular { group: 6, float: false, .. } => Class::Sint,
            Layout::Rgb10a2 { int: Some(false) } => Class::Uint,
            Layout::Rgb10a2 { int: Some(true) } => Class::Sint,
            _ => Class::Float,
        }
    }

    /// Bytes per element (0 if not a plain per-element format).
    pub fn size(&self) -> u32 {
        match layout(self.id) {
            Layout::Regular { chans, bits, .. } => (chans * bits).div_ceil(8),
            Layout::Rgb565 | Layout::Rgb5a1 | Layout::A1bgr5 | Layout::Rgba4 => 2,
            Layout::Rgb10a2 { .. } | Layout::R11g11b10f | Layout::Z24x8 => 4,
            Layout::Rgb332 => 1,
            Layout::Snap4 => 16,
            _ => 0,
        }
    }

    /// Decode one element from its bytes into RGBA (before the swizzle): fp32 bits for
    /// float classes, raw integers otherwise.
    pub fn decode(&self, b: &[u8]) -> [u32; 4] {
        let f = |x: f32| x.to_bits();
        let mut out = [0u32, 0, 0, f(1.0)];
        let class = self.class();
        if class != Class::Float {
            out[3] = 1;
        }
        let word = |i: usize| u32::from_le_bytes([b[i], b[i + 1], b[i + 2], b[i + 3]]);
        match layout(self.id) {
            Layout::Regular { group, chans, bits, float } => {
                for c in 0..chans as usize {
                    let raw: u64 = match bits {
                        1 | 2 | 4 => {
                            let bit = c as u32 * bits;
                            ((b[(bit / 8) as usize] >> (bit % 8)) as u64) & ((1 << bits) - 1)
                        }
                        8 => b[c] as u64,
                        16 => u16::from_le_bytes([b[2 * c], b[2 * c + 1]]) as u64,
                        32 => word(4 * c) as u64,
                        _ => word(8 * c) as u64 | (word(8 * c + 4) as u64) << 32,
                    };
                    out[c] = if float {
                        match bits {
                            16 => f(f16_to_f32(raw as u16)),
                            32 => raw as u32,
                            _ => f(f64::from_bits(raw) as f32),
                        }
                    } else {
                        let max = if bits == 64 { u64::MAX } else { (1u64 << bits) - 1 };
                        match group {
                            3 => {
                                let s = ((raw << (64 - bits)) as i64 >> (64 - bits)) as f64;
                                f((s / (max >> 1) as f64).max(-1.0) as f32)
                            }
                            4 => raw as u32,
                            5 => f((raw as f64 / max as f64) as f32),
                            _ => ((raw << (64 - bits)) as i64 >> (64 - bits)) as u32,
                        }
                    };
                }
                if self.srgb {
                    for c in 0..3.min(chans as usize) {
                        out[c] = f(srgb_to_linear(f32::from_bits(out[c])));
                    }
                }
            }
            Layout::Rgb565 => {
                let v = u16::from_le_bytes([b[0], b[1]]) as u32;
                out[0] = f((v & 31) as f32 / 31.0);
                out[1] = f(((v >> 5) & 63) as f32 / 63.0);
                out[2] = f((v >> 11) as f32 / 31.0);
            }
            Layout::Rgb5a1 => {
                let v = u16::from_le_bytes([b[0], b[1]]) as u32;
                out[0] = f((v & 31) as f32 / 31.0);
                out[1] = f(((v >> 5) & 31) as f32 / 31.0);
                out[2] = f(((v >> 10) & 31) as f32 / 31.0);
                out[3] = f((v >> 15) as f32);
            }
            Layout::A1bgr5 => {
                let v = u16::from_le_bytes([b[0], b[1]]) as u32;
                out[3] = f((v & 1) as f32);
                out[2] = f(((v >> 1) & 31) as f32 / 31.0);
                out[1] = f(((v >> 6) & 31) as f32 / 31.0);
                out[0] = f(((v >> 11) & 31) as f32 / 31.0);
            }
            Layout::Rgba4 => {
                let v = u16::from_le_bytes([b[0], b[1]]) as u32;
                for c in 0..4 {
                    out[c] = f(((v >> (4 * c)) & 15) as f32 / 15.0);
                }
            }
            Layout::Rgb10a2 { int } => {
                let v = word(0);
                let ch = [v & 1023, (v >> 10) & 1023, (v >> 20) & 1023, v >> 30];
                for c in 0..4 {
                    let max = if c == 3 { 3.0 } else { 1023.0 };
                    out[c] = match int {
                        None => f(ch[c] as f32 / max),
                        Some(false) => ch[c],
                        Some(true) => {
                            let bits = if c == 3 { 2 } else { 10 };
                            ((ch[c] << (32 - bits)) as i32 >> (32 - bits)) as u32
                        }
                    };
                }
            }
            Layout::R11g11b10f => {
                let v = word(0);
                out[0] = f(small_float(v & 0x7ff, 6));
                out[1] = f(small_float((v >> 11) & 0x7ff, 6));
                out[2] = f(small_float(v >> 22, 5));
            }
            Layout::Rgb332 => {
                let v = b[0] as u32;
                out[0] = f((v & 7) as f32 / 7.0);
                out[1] = f(((v >> 3) & 7) as f32 / 7.0);
                out[2] = f((v >> 6) as f32 / 3.0);
            }
            Layout::Z24x8 => {
                out[0] = f((word(0) & 0xff_ffff) as f32 / 16_777_215.0);
            }
            Layout::Snap4 => {
                for c in 0..4 {
                    out[c] = word(4 * c);
                }
            }
            Layout::Constant | Layout::Unknown => {
                out = [0, 0, 0, 0];
            }
        }
        out
    }

    /// Apply the format's swizzle (3 bits per output channel: R, G, B, A, 0, 1).
    pub fn swizzled(&self, v: [u32; 4]) -> [u32; 4] {
        apply_swizzle(self.swizzle, v, self.class() == Class::Float)
    }

    /// Load and swizzle one element.
    pub fn load(&self, vm: &mut Vm, va: u64) -> [u32; 4] {
        let n = self.size().max(1) as usize;
        let mut b = [0u8; 32];
        vm.read_bytes(va, &mut b[..n]);
        self.swizzled(self.decode(&b))
    }

    /// Encode RGBA (fp32 bits for float classes) into the format's bytes; returns the size.
    pub fn encode(&self, v: [u32; 4], out: &mut [u8; 32]) -> usize {
        let fv = |c: usize| f32::from_bits(v[c]);
        match layout(self.id) {
            Layout::Regular { group, chans, bits, float } => {
                for c in 0..chans as usize {
                    let raw: u64 = if float {
                        match bits {
                            16 => f32_to_f16(fv(c)) as u64,
                            32 => v[c] as u64,
                            _ => (fv(c) as f64).to_bits(),
                        }
                    } else {
                        let max = if bits == 64 { u64::MAX } else { (1u64 << bits) - 1 };
                        match group {
                            3 => {
                                let m = (max >> 1) as f32;
                                ((fv(c).clamp(-1.0, 1.0) * m).round() as i64 as u64) & max
                            }
                            4 => (v[c] as u64).min(max),
                            5 => {
                                let x = if self.srgb && c < 3 { linear_to_srgb(fv(c)) } else { fv(c) };
                                (x.clamp(0.0, 1.0) as f64 * max as f64).round() as u64
                            }
                            _ => (v[c] as i32 as i64 as u64) & max,
                        }
                    };
                    match bits {
                        1 | 2 | 4 => {
                            let bit = c as u32 * bits;
                            let i = (bit / 8) as usize;
                            out[i] = (out[i] & !(((1u8 << bits) - 1) << (bit % 8))) | ((raw as u8) << (bit % 8));
                        }
                        8 => out[c] = raw as u8,
                        16 => out[2 * c..2 * c + 2].copy_from_slice(&(raw as u16).to_le_bytes()),
                        32 => out[4 * c..4 * c + 4].copy_from_slice(&(raw as u32).to_le_bytes()),
                        _ => out[8 * c..8 * c + 8].copy_from_slice(&raw.to_le_bytes()),
                    }
                }
            }
            Layout::Snap4 => {
                for c in 0..4 {
                    out[4 * c..4 * c + 4].copy_from_slice(&v[c].to_le_bytes());
                }
            }
            Layout::Rgb565 => {
                let q = |c: usize, m: f32| (fv(c).clamp(0.0, 1.0) * m).round() as u16;
                let w = q(0, 31.0) | q(1, 63.0) << 5 | q(2, 31.0) << 11;
                out[..2].copy_from_slice(&w.to_le_bytes());
            }
            Layout::Rgb10a2 { int: None } => {
                let q = |c: usize, m: f32| (fv(c).clamp(0.0, 1.0) * m).round() as u32;
                let w = q(0, 1023.0) | q(1, 1023.0) << 10 | q(2, 1023.0) << 20 | q(3, 3.0) << 30;
                out[..4].copy_from_slice(&w.to_le_bytes());
            }
            _ => {}
        }
        self.size() as usize
    }
}

pub fn apply_swizzle(sw: u32, v: [u32; 4], float: bool) -> [u32; 4] {
    let one = if float { 1.0f32.to_bits() } else { 1 };
    let mut o = [0u32; 4];
    for c in 0..4 {
        o[c] = match (sw >> (3 * c)) & 7 {
            s @ 0..=3 => v[s as usize],
            5 => one,
            _ => 0,
        };
    }
    o
}

fn small_float(v: u32, mant_bits: u32) -> f32 {
    let e = (v >> mant_bits) as i32;
    let m = (v & ((1 << mant_bits) - 1)) as f32 / (1 << mant_bits) as f32;
    match e {
        0 => m * 2f32.powi(-14),
        31 => if m == 0.0 { f32::INFINITY } else { f32::NAN },
        _ => (1.0 + m) * 2f32.powi(e - 15),
    }
}

pub fn srgb_to_linear(c: f32) -> f32 {
    if c <= 0.04045 { c / 12.92 } else { ((c + 0.055) / 1.055).powf(2.4) }
}

pub fn linear_to_srgb(c: f32) -> f32 {
    let c = c.clamp(0.0, 1.0);
    if c <= 0.0031308 { c * 12.92 } else { 1.055 * c.powf(1.0 / 2.4) - 0.055 }
}

/// Offset of pixel (x, y) in a 16x16 u-interleaved tiled surface: `row_stride` bytes per
/// row of tiles, `bpp` bytes per pixel. Within a tile, bit 2i of the index is x_i ^ y_i
/// and bit 2i+1 is y_i.
pub fn u_interleaved_offset(x: u32, y: u32, row_stride: u32, bpp: u32) -> u64 {
    let (tx, ty) = (x >> 4, y >> 4);
    let (lx, ly) = (x & 15, y & 15);
    let mut i = 0;
    for b in 0..4 {
        let xb = (lx >> b) & 1;
        let yb = (ly >> b) & 1;
        i |= ((xb ^ yb) << (2 * b)) | (yb << (2 * b + 1));
    }
    ty as u64 * row_stride as u64 + (tx as u64 * 256 + i as u64) * bpp as u64
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn regular_formats() {
        let rgba8 = PixelFormat { id: 187, swizzle: 0b011_010_001_000, srgb: false };
        let v = rgba8.decode(&[255, 0, 128, 255, 0, 0, 0, 0]);
        assert_eq!(f32::from_bits(v[0]), 1.0);
        assert!((f32::from_bits(v[2]) - 128.0 / 255.0).abs() < 1e-6);
        let rgba32f = PixelFormat { id: 191, swizzle: 0b011_010_001_000, srgb: false };
        let mut b = [0u8; 32];
        let n = rgba32f.encode([1.5f32.to_bits(), 0, 0, 0], &mut b);
        assert_eq!(n, 16);
        assert_eq!(f32::from_bits(rgba32f.decode(&b)[0]), 1.5);
        assert_eq!(PixelFormat { id: 133, swizzle: 0, srgb: false }.class(), Class::Uint);
        // R16F lives in the SINT group with code 7
        let r16f = PixelFormat { id: 199, swizzle: 0b101_100_100_000, srgb: false };
        let v = r16f.load_bytes(&0x3c00u16.to_le_bytes());
        assert_eq!(f32::from_bits(v[0]), 1.0);
        assert_eq!(f32::from_bits(v[3]), 1.0);
    }

    impl PixelFormat {
        fn load_bytes(&self, b: &[u8]) -> [u32; 4] {
            let mut x = [0u8; 32];
            x[..b.len()].copy_from_slice(b);
            self.swizzled(self.decode(&x))
        }
    }

    #[test]
    fn u_interleave() {
        assert_eq!(u_interleaved_offset(0, 0, 0, 4), 0);
        assert_eq!(u_interleaved_offset(1, 0, 0, 1), 1);
        assert_eq!(u_interleaved_offset(0, 1, 0, 1), 3);
        assert_eq!(u_interleaved_offset(16, 0, 0, 1), 256);
        assert_eq!(u_interleaved_offset(0, 16, 4096, 1), 4096);
    }
}
