//! Render target formats (RT_FORMAT codes) and depth/stencil formats (ZETA_FORMAT codes).

use crate::num::{bits, bytes, f32s, linear_to_srgb8, srgb8_table, to_unorm, words, Chan, Num, Packed};
use std::cmp::Ordering;

/// What a shader writes to a colour target.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Class {
    /// f32: normalised and float formats. Only these blend.
    Float,
    Sint,
    Uint,
}

/// Formats with their own code paths.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Fast {
    No,
    /// 8-bit UNORM, red in byte 0.
    Rgba8,
    /// 8-bit UNORM, blue in byte 0.
    Bgra8,
}

/// A colour target format.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RtFormat {
    pub code: u8,
    /// Bytes per sample: 1, 2, 4, 8 or 16.
    pub bytes: u8,
    pub class: Class,
    /// Red, green and blue are sRGB encoded (SRGB_WRITE has to be enabled as well).
    pub srgb: bool,
    /// Channels that are stored; blending reads a missing alpha as 1.
    pub packed: Packed,
    /// Bits set in formats whose unused bits are written as ones.
    fill: [u32; 4],
    fast: Fast,
}

const NONE: Chan = Chan::new(0, 0, 0, Num::Unorm);

impl RtFormat {
    /// The format of a RT_FORMAT code. None: disabled (0) or reserved.
    #[rustfmt::skip]
    pub fn from_code(code: u8) -> Option<Self> {
        use Num::*;
        // Channels as (word, shift, bits).
        type C = (u8, u8, u8);
        const N: C = (0, 0, 0);
        let f = |n: u8, num: Num, ch: [C; 4]| -> (u8, [Chan; 4], bool, [u32; 4]) {
            (n, ch.map(|(w, s, b)| if b == 0 { NONE } else { Chan::new(w, s, b, num) }), false, [0; 4])
        };
        let srgb = |t: (u8, [Chan; 4], bool, [u32; 4])| (t.0, t.1, true, t.3);
        let fill = |t: (u8, [Chan; 4], bool, [u32; 4]), w0: u32| (t.0, t.1, t.2, [w0, 0, 0, 0]);
        // Common shapes: names give the channels from the low bits up.
        let rgba32 = |num| f(16, num, [(0, 0, 32), (1, 0, 32), (2, 0, 32), (3, 0, 32)]);
        let rgbx32 = |num| f(16, num, [(0, 0, 32), (1, 0, 32), (2, 0, 32), N]);
        let rgba16 = |num| f(8, num, [(0, 0, 16), (0, 16, 16), (1, 0, 16), (1, 16, 16)]);
        let rg32 = |num| f(8, num, [(0, 0, 32), (1, 0, 32), N, N]);
        let rg16 = |num| f(4, num, [(0, 0, 16), (0, 16, 16), N, N]);
        let rgba8 = |num| f(4, num, [(0, 0, 8), (0, 8, 8), (0, 16, 8), (0, 24, 8)]);
        let rgbx8 = || fill(f(4, Unorm, [(0, 0, 8), (0, 8, 8), (0, 16, 8), N]), 0xff00_0000);
        let bgra8 = || f(4, Unorm, [(0, 16, 8), (0, 8, 8), (0, 0, 8), (0, 24, 8)]);
        let bgrx8 = || f(4, Unorm, [(0, 16, 8), (0, 8, 8), (0, 0, 8), N]);
        let bgrx8_ones = || fill(bgrx8(), 0xff00_0000);
        let rgb10a2 = |num| f(4, num, [(0, 0, 10), (0, 10, 10), (0, 20, 10), (0, 30, 2)]);
        let bgr5x1 = || f(2, Unorm, [(0, 10, 5), (0, 5, 5), (0, 0, 5), N]);
        let rg8 = |num| f(2, num, [(0, 0, 8), (0, 8, 8), N, N]);
        let r = |n, num| f(n, num, [(0, 0, n * 8), N, N, N]);
        let a = |n, num| f(n, num, [N, N, N, (0, 0, n * 8)]);
        let (bytes, ch, srgb, fill) = match code {
            0xc0 => rgba32(Float),
            0xc1 => rgba32(Sint),
            0xc2 => rgba32(Uint),
            0xc3 => rgbx32(Float),
            0xc4 => rgbx32(Sint),
            0xc5 => rgbx32(Uint),
            0xc6 => rgba16(Unorm),
            0xc7 => rgba16(Snorm),
            0xc8 => rgba16(Sint),
            0xc9 => rgba16(Uint),
            0xca => rgba16(Float),
            0xcb => rg32(Float),
            0xcc => rg32(Sint),
            0xcd => rg32(Uint),
            0xce => f(8, Float, [(0, 0, 16), (0, 16, 16), (1, 0, 16), N]),
            0xcf => bgra8(),
            0xd0 => srgb(bgra8()),
            0xd1 => rgb10a2(Unorm),
            0xd2 => rgb10a2(Uint),
            0xd5 => rgba8(Unorm),
            0xd6 => srgb(rgba8(Unorm)),
            0xd7 => rgba8(Snorm),
            0xd8 => rgba8(Sint),
            0xd9 => rgba8(Uint),
            0xda => rg16(Unorm),
            0xdb => rg16(Snorm),
            0xdc => rg16(Sint),
            0xdd => rg16(Uint),
            0xde => rg16(Float),
            0xdf => f(4, Unorm, [(0, 20, 10), (0, 10, 10), (0, 0, 10), (0, 30, 2)]),
            0xe0 => f(4, Float, [(0, 0, 11), (0, 11, 11), (0, 22, 10), N]),
            0xe3 => r(4, Sint),
            0xe4 => r(4, Uint),
            0xe5 => r(4, Float),
            0xe6 => bgrx8_ones(),
            0xe7 => srgb(bgrx8_ones()),
            0xe8 => f(2, Unorm, [(0, 11, 5), (0, 5, 6), (0, 0, 5), N]),
            0xe9 => f(2, Unorm, [(0, 10, 5), (0, 5, 5), (0, 0, 5), (0, 15, 1)]),
            0xea => rg8(Unorm),
            0xeb => rg8(Snorm),
            0xec => rg8(Sint),
            0xed => rg8(Uint),
            0xee => r(2, Unorm),
            0xef => r(2, Snorm),
            0xf0 => r(2, Sint),
            0xf1 => r(2, Uint),
            0xf2 => r(2, Float),
            0xf3 => r(1, Unorm),
            0xf4 => r(1, Snorm),
            0xf5 => r(1, Sint),
            0xf6 => r(1, Uint),
            0xf7 => a(1, Unorm),
            0xf8 => fill(bgr5x1(), 0x8000),
            0xf9 => rgbx8(),
            0xfa => srgb(rgbx8()),
            0xfb => bgr5x1(),                                           // unused bit zero
            0xfc => fill(bgr5x1(), 0x8000),                             // and one
            0xfd => bgrx8(),
            0xfe => bgrx8_ones(),
            0xff => r(4, Unorm),
            0x40 => a(2, Unorm),
            0x41 => a(2, Float),
            0x42 => a(4, Float),
            0x43 => f(2, Unorm, [(0, 0, 8), N, N, (0, 8, 8)]),
            0x44 => f(4, Unorm, [(0, 0, 16), N, N, (0, 16, 16)]),
            0x45 => f(4, Float, [(0, 0, 16), N, N, (0, 16, 16)]),
            0x46 => f(8, Float, [(0, 0, 32), N, N, (1, 0, 32)]),
            0x47 => f(4, Unorm, [(0, 8, 8), (0, 16, 8), (0, 24, 8), (0, 0, 8)]),
            _ => return None,
        };
        let packed = Packed { bytes, ch, srgb };
        let num = ch.iter().find(|c| c.bits != 0).map_or(Unorm, |c| c.num);
        let class = match num {
            Sint => Class::Sint,
            Uint => Class::Uint,
            _ => Class::Float,
        };
        let fast = match code {
            0xd5 => Fast::Rgba8,
            0xcf => Fast::Bgra8,
            _ => Fast::No,
        };
        Some(Self { code, bytes, class, srgb, packed, fill, fast })
    }

    /// The format with sRGB encoding off (SRGB_WRITE disabled).
    pub fn linear(mut self) -> Self {
        self.srgb = false;
        self.packed.srgb = false;
        self
    }

    /// The shader's output as it is stored, saturated to the format. `v` is f32 bits for
    /// [`Class::Float`], integers otherwise.
    #[inline]
    pub fn encode(&self, v: &[u32; 4]) -> [u8; 16] {
        match self.fast {
            Fast::No => {
                let mut w = self.packed.encode(v);
                for (w, f) in w.iter_mut().zip(self.fill) {
                    *w |= f;
                }
                bytes(&w)
            }
            _ => {
                let mut out = [0; 16];
                out[..4].copy_from_slice(&self.encode_rgba8(f32s(*v)));
                out
            }
        }
    }

    #[inline(always)]
    fn encode_rgba8(&self, v: [f32; 4]) -> [u8; 4] {
        // NaN gives 0.
        let c = v.map(|v| (v.clamp(0.0, 1.0) * 255.0 + 0.5) as u8);
        if self.fast == Fast::Bgra8 {
            [c[2], c[1], c[0], c[3]]
        } else {
            c
        }
    }

    #[inline]
    pub fn encode_f32(&self, v: [f32; 4]) -> [u8; 16] {
        self.encode(&bits(v))
    }

    /// The stored sample as f32 bits or integers; a missing alpha is 1, other missing
    /// channels 0.
    #[inline]
    pub fn decode(&self, b: &[u8]) -> [u32; 4] {
        match self.fast {
            Fast::No => self.packed.decode(&words(&b[..self.bytes as usize])),
            _ => {
                let v = [b[0], b[1], b[2], b[3]].map(|v| v as f32 / 255.0);
                bits(if self.fast == Fast::Bgra8 { [v[2], v[1], v[0], v[3]] } else { v })
            }
        }
    }

    #[inline]
    pub fn decode_f32(&self, b: &[u8]) -> [f32; 4] {
        f32s(self.decode(b))
    }

    /// The bits a write mask (red, green, blue, alpha) leaves writable. Unused bits are
    /// written when any channel is.
    pub fn write_mask(&self, mask: [bool; 4]) -> [u8; 16] {
        let n = self.bytes as usize;
        if mask == [false; 4] {
            return [0; 16];
        }
        let mut w = [0u32; 4];
        for (c, m) in self.packed.ch.iter().zip(mask) {
            if c.bits != 0 && !m {
                w[c.word as usize & 3] |= c.mask();
            }
        }
        let mut out = bytes(&w.map(|w| !w));
        out[n..].fill(0);
        out
    }

    /// Blending and the fixed clamps of the format: NaN-free normalised values.
    pub fn is_normalized(&self) -> bool {
        self.packed.ch.iter().any(|c| c.bits != 0 && matches!(c.num, Num::Unorm | Num::Snorm))
    }

    /// 8-bit sRGB code of a linear value, as stored in sRGB formats.
    pub fn to_srgb8(v: f32) -> u8 {
        linear_to_srgb8(v)
    }

    pub fn from_srgb8(v: u8) -> f32 {
        srgb8_table()[v as usize]
    }
}

/// A depth/stencil format.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ZetaFormat {
    pub code: u8,
    /// Bytes per sample: 1, 2, 4 or 8.
    pub bytes: u8,
    /// Size of the depth (0: none); 32 is float.
    pub depth_bits: u8,
    depth_shift: u8,
    /// Where the 8 bits of stencil are, if any.
    pub stencil_shift: Option<u8>,
}

impl ZetaFormat {
    /// The format of a ZETA_FORMAT code.
    pub fn from_code(code: u8) -> Option<Self> {
        let (bytes, depth_bits, depth_shift, stencil_shift) = match code {
            0x0a => (4, 32, 0, None),
            0x13 => (2, 16, 0, None),
            0x14 => (4, 24, 8, Some(0)),
            0x15 | 0x18 => (4, 24, 0, None),
            0x16 => (4, 24, 0, Some(24)),
            0x17 => (1, 0, 0, Some(0)),
            0x19 | 0x1f => (8, 32, 0, Some(32)),
            0x1d => (8, 24, 0, Some(32)),
            0x1e => (8, 32, 0, None),
            _ => return None,
        };
        Some(Self { code, bytes, depth_bits, depth_shift, stencil_shift })
    }

    pub fn has_depth(self) -> bool {
        self.depth_bits != 0
    }

    pub fn has_stencil(self) -> bool {
        self.stencil_shift.is_some()
    }

    pub fn is_float(self) -> bool {
        self.depth_bits == 32
    }

    fn depth_mask(self) -> u64 {
        ((1u64 << self.depth_bits) - 1) << self.depth_shift
    }

    /// A stored sample.
    #[inline]
    pub fn load(self, b: &[u8]) -> u64 {
        let mut v = [0u8; 8];
        let n = self.bytes as usize;
        v[..n].copy_from_slice(&b[..n]);
        u64::from_le_bytes(v)
    }

    #[inline]
    pub fn store(self, v: u64, b: &mut [u8]) {
        let n = self.bytes as usize;
        b[..n].copy_from_slice(&v.to_le_bytes()[..n]);
    }

    /// The depth of a sample as stored: an integer, or f32 bits.
    #[inline]
    pub fn depth(self, sample: u64) -> u32 {
        ((sample & self.depth_mask()) >> self.depth_shift) as u32
    }

    #[inline]
    pub fn stencil(self, sample: u64) -> u8 {
        self.stencil_shift.map_or(0, |s| (sample >> s) as u8)
    }

    #[inline]
    pub fn with_depth(self, sample: u64, depth: u32) -> u64 {
        let m = self.depth_mask();
        (sample & !m) | (((depth as u64) << self.depth_shift) & m)
    }

    /// Replace the bits of `mask` of the stencil.
    #[inline]
    pub fn with_stencil(self, sample: u64, stencil: u8, mask: u8) -> u64 {
        match self.stencil_shift {
            Some(s) => {
                let m = (mask as u64) << s;
                (sample & !m) | (((stencil as u64) << s) & m)
            }
            None => sample,
        }
    }

    /// A depth as it is stored: rounded to the nearest integer of 0..1, or f32 bits.
    #[inline]
    pub fn quantize(self, z: f32) -> u32 {
        if self.is_float() {
            z.to_bits()
        } else {
            to_unorm(z, self.depth_bits)
        }
    }

    #[inline]
    pub fn to_f32(self, depth: u32) -> f32 {
        if self.is_float() {
            f32::from_bits(depth)
        } else if self.depth_bits == 0 {
            0.0
        } else {
            (depth as f64 / ((1u64 << self.depth_bits) - 1) as f64) as f32
        }
    }

    /// Order of two stored depths. None if one is NaN.
    #[inline]
    pub fn compare(self, a: u32, b: u32) -> Option<Ordering> {
        if self.is_float() {
            f32::from_bits(a).partial_cmp(&f32::from_bits(b))
        } else {
            Some(a.cmp(&b))
        }
    }

    /// Depth and stencil of a sample.
    pub fn decode(self, b: &[u8]) -> (f32, u8) {
        let v = self.load(b);
        (self.to_f32(self.depth(v)), self.stencil(v))
    }

    /// A sample of a depth and a stencil, other bits zero.
    pub fn encode(self, z: f32, stencil: u8) -> u64 {
        let d = if self.has_depth() { self.with_depth(0, self.quantize(z)) } else { 0 };
        self.with_stencil(d, stencil, 0xff)
    }
}
