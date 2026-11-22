//! Number formats: normalised integers, small floats, sRGB, and texels packed from channels.

use std::sync::OnceLock;

/// How a channel's bits are a number (the TIC's data types).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Num {
    Snorm,
    #[default]
    Unorm,
    Sint,
    Uint,
    Float,
}

impl Num {
    /// A TIC component's data type. 5 and 6 are SNORM/UNORM with the filter forced to FP16.
    pub fn from_tic(v: u32) -> Option<Self> {
        Some(match v {
            1 | 5 => Self::Snorm,
            2 | 6 => Self::Unorm,
            3 => Self::Sint,
            4 => Self::Uint,
            7 => Self::Float,
            _ => return None,
        })
    }

    pub fn is_int(self) -> bool {
        matches!(self, Self::Sint | Self::Uint)
    }
}

/// One channel of a packed texel: `bits` bits at `shift` of 32-bit word `word`.
/// No bits: the channel is not stored.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Chan {
    pub word: u8,
    pub shift: u8,
    pub bits: u8,
    pub num: Num,
}

impl Chan {
    pub const fn new(word: u8, shift: u8, bits: u8, num: Num) -> Self {
        Self { word, shift, bits, num }
    }

    /// The channel's mask in its word.
    pub const fn mask(self) -> u32 {
        (low_mask(self.bits)) << self.shift
    }

    #[inline]
    pub fn raw(self, words: &[u32; 4]) -> u32 {
        (words[(self.word & 3) as usize] >> self.shift) & low_mask(self.bits)
    }

    /// The channel's value: f32 bits, or an integer (sign extended) for integer channels.
    #[inline]
    pub fn decode(self, words: &[u32; 4]) -> u32 {
        let v = self.raw(words);
        match self.num {
            Num::Unorm => unorm(v, self.bits).to_bits(),
            Num::Snorm => snorm(v, self.bits).to_bits(),
            Num::Uint => v,
            Num::Sint => sext(v, self.bits) as u32,
            Num::Float => small_to_f32(v, self.bits).to_bits(),
        }
    }

    /// The channel's bits for a value as [`Self::decode`] gives it, saturated to the range.
    #[inline]
    pub fn encode_raw(self, v: u32) -> u32 {
        match self.num {
            Num::Unorm => to_unorm(f32::from_bits(v), self.bits),
            Num::Snorm => to_snorm(f32::from_bits(v), self.bits),
            Num::Uint => v.min(low_mask(self.bits)),
            Num::Sint => {
                let max = (low_mask(self.bits) >> 1) as i32;
                (v as i32).clamp(-max - 1, max) as u32 & low_mask(self.bits)
            }
            Num::Float => f32_to_small(f32::from_bits(v), self.bits),
        }
    }

    #[inline]
    pub fn encode(self, v: u32, words: &mut [u32; 4]) {
        if self.bits != 0 {
            words[(self.word & 3) as usize] |= self.encode_raw(v) << self.shift;
        }
    }
}

#[inline]
const fn low_mask(bits: u8) -> u32 {
    if bits >= 32 {
        !0
    } else {
        (1 << bits) - 1
    }
}

#[inline]
fn sext(v: u32, bits: u8) -> i32 {
    let s = 32 - bits.clamp(1, 32) as u32;
    ((v << s) as i32) >> s
}

#[inline]
pub fn unorm(v: u32, bits: u8) -> f32 {
    if bits > 24 {
        (v as f64 / low_mask(bits) as f64) as f32
    } else {
        v as f32 / low_mask(bits) as f32
    }
}

#[inline]
pub fn snorm(v: u32, bits: u8) -> f32 {
    let max = low_mask(bits) >> 1;
    let i = sext(v, bits);
    if bits > 24 {
        ((i as f64 / max as f64) as f32).max(-1.0)
    } else {
        (i as f32 / max as f32).max(-1.0)
    }
}

/// NaN gives 0.
#[inline]
pub fn to_unorm(v: f32, bits: u8) -> u32 {
    let v = if v > 0.0 { v.min(1.0) } else { 0.0 };
    if bits <= 16 {
        (v * low_mask(bits) as f32 + 0.5) as u32
    } else {
        (v as f64 * low_mask(bits) as f64 + 0.5) as u32
    }
}

#[inline]
pub fn to_snorm(v: f32, bits: u8) -> u32 {
    let v = if v.is_nan() { 0.0 } else { v.clamp(-1.0, 1.0) } as f64;
    let max = (low_mask(bits) >> 1) as f64;
    ((v * max).round() as i32) as u32 & low_mask(bits)
}

/// 32, 16 (half), 11 or 10 bit (unsigned) floats.
#[inline]
fn small_to_f32(v: u32, bits: u8) -> f32 {
    match bits {
        32 => f32::from_bits(v),
        16 => half_to_f32(v as u16),
        11 => ufloat_to_f32(v, 6),
        _ => ufloat_to_f32(v, 5),
    }
}

#[inline]
fn f32_to_small(v: f32, bits: u8) -> u32 {
    match bits {
        32 => v.to_bits(),
        16 => f32_to_half(v) as u32,
        11 => f32_to_ufloat(v, 6),
        _ => f32_to_ufloat(v, 5),
    }
}

#[inline]
pub fn half_to_f32(h: u16) -> f32 {
    let sign = (h as u32 & 0x8000) << 16;
    let em = h as u32 & 0x7fff;
    let v = if em >= 0x7c00 {
        0x7f80_0000 | (em & 0x3ff) << 13
    } else if em >= 0x400 {
        (em << 13) + (112 << 23)
    } else {
        (em as f32 * (1.0 / 16_777_216.0)).to_bits()
    };
    f32::from_bits(sign | v)
}

/// Rounds to nearest, ties to even; overflow gives infinity.
pub fn f32_to_half(f: f32) -> u16 {
    let b = f.to_bits();
    let sign = ((b >> 16) & 0x8000) as u16;
    let a = b & 0x7fff_ffff;
    if a > 0x7f80_0000 {
        return sign | 0x7e00;
    }
    if a >= 0x477f_f000 {
        return sign | 0x7c00;
    }
    if a < 0x3880_0000 {
        // Below 2^-14: a multiple of 2^-24.
        let v = f32::from_bits(a) * 16_777_216.0;
        return sign | v.round_ties_even() as u16;
    }
    let m = a - (112 << 23);
    let round = (m >> 13) + (((m & 0x1fff) + ((m >> 13) & 1) > 0x1000) as u32);
    sign | round as u16
}

/// Unsigned float with a 5-bit exponent and `mant` mantissa bits.
#[inline]
pub fn ufloat_to_f32(v: u32, mant: u32) -> f32 {
    let e = (v >> mant) & 31;
    let m = v & ((1 << mant) - 1);
    let up = 23 - mant;
    match e {
        31 => f32::from_bits(0x7f80_0000 | m << up),
        0 => m as f32 / (1u32 << (14 + mant)) as f32,
        _ => f32::from_bits((e + 112) << 23 | m << up),
    }
}

/// Negative values give 0, values above the largest finite one that value (but infinity is
/// kept). Rounds to nearest.
pub fn f32_to_ufloat(f: f32, mant: u32) -> u32 {
    let inf = 31 << mant;
    if f.is_nan() {
        return inf | 1;
    }
    if f <= 0.0 {
        return 0;
    }
    if f.is_infinite() {
        return inf;
    }
    let b = f.to_bits();
    if b < 113 << 23 {
        return (f * (1u32 << (14 + mant)) as f32).round_ties_even() as u32;
    }
    let m = b - (112 << 23);
    let up = 23 - mant;
    let half = 1 << (up - 1);
    let r = (m >> up) + (((m & (2 * half - 1)) + ((m >> up) & 1) > half) as u32);
    r.min(inf - 1)
}

/// Shared exponent RGB9E5: red in the low bits.
pub fn rgb9e5_to_f32(v: u32) -> [f32; 3] {
    let scale = f32::from_bits((((v >> 27) as i32 - 15 - 9 + 127) as u32) << 23);
    [(v & 511) as f32 * scale, ((v >> 9) & 511) as f32 * scale, ((v >> 18) & 511) as f32 * scale]
}

fn srgb_to_linear(c: f64) -> f64 {
    if c <= 0.04045 {
        c / 12.92
    } else {
        ((c + 0.055) / 1.055).powf(2.4)
    }
}

/// Linear value of each 8-bit sRGB code.
pub fn srgb8_table() -> &'static [f32; 256] {
    static T: OnceLock<[f32; 256]> = OnceLock::new();
    T.get_or_init(|| std::array::from_fn(|i| srgb_to_linear(i as f64 / 255.0) as f32))
}

struct SrgbEncode {
    /// Code of the low end of each of 4096 equal parts of 0..1.
    guess: [u8; 4097],
    /// Smallest linear value that rounds to each code.
    least: [f32; 257],
}

fn srgb_encode_table() -> &'static SrgbEncode {
    static T: OnceLock<SrgbEncode> = OnceLock::new();
    T.get_or_init(|| {
        let mut least = [0f32; 257];
        for (i, l) in least.iter_mut().enumerate().skip(1) {
            *l = srgb_to_linear((i as f64 - 0.5) / 255.0) as f32;
        }
        least[256] = f32::INFINITY;
        let mut guess = [0u8; 4097];
        let mut code = 0;
        for (i, g) in guess.iter_mut().enumerate() {
            let v = i as f32 / 4096.0;
            while least[code + 1] <= v {
                code += 1;
            }
            *g = code as u8;
        }
        SrgbEncode { guess, least }
    })
}

/// The 8-bit sRGB code nearest a linear value, saturated. NaN gives 0.
pub fn linear_to_srgb8(v: f32) -> u8 {
    let t = srgb_encode_table();
    let v = if v > 0.0 { v.min(1.0) } else { 0.0 };
    let mut code = t.guess[((v * 4096.0) as usize).min(4096)] as usize;
    while t.least[code + 1] <= v {
        code += 1;
    }
    code as u8
}

/// A texel of up to 16 bytes made of channels in 32-bit words: red, green, blue, alpha.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Packed {
    pub bytes: u8,
    pub ch: [Chan; 4],
    /// Red, green and blue are sRGB encoded (8 bits each).
    pub srgb: bool,
}

impl Packed {
    /// Channels not stored read as 0, alpha as 1 (an integer 1 if the others are integers).
    pub fn decode(&self, words: &[u32; 4]) -> [u32; 4] {
        let mut out = [0; 4];
        for (i, (o, c)) in out.iter_mut().zip(&self.ch).enumerate() {
            *o = if c.bits == 0 {
                if i < 3 {
                    0
                } else if self.is_int() {
                    1
                } else {
                    1f32.to_bits()
                }
            } else if self.srgb && i < 3 {
                srgb8_table()[(c.raw(words) & 255) as usize].to_bits()
            } else {
                c.decode(words)
            };
        }
        out
    }

    pub fn encode(&self, v: &[u32; 4]) -> [u32; 4] {
        let mut words = [0; 4];
        for (i, c) in self.ch.iter().enumerate() {
            if self.srgb && i < 3 && c.bits != 0 {
                words[(c.word & 3) as usize] |= (linear_to_srgb8(f32::from_bits(v[i])) as u32) << c.shift;
            } else {
                c.encode(v[i], &mut words);
            }
        }
        words
    }

    pub fn is_int(&self) -> bool {
        self.ch.iter().any(|c| c.bits != 0 && c.num.is_int())
    }
}

/// Little-endian words of up to 16 bytes.
#[inline]
pub fn words(bytes: &[u8]) -> [u32; 4] {
    let mut b = [0u8; 16];
    let n = bytes.len().min(16);
    b[..n].copy_from_slice(&bytes[..n]);
    std::array::from_fn(|i| u32::from_le_bytes([b[4 * i], b[4 * i + 1], b[4 * i + 2], b[4 * i + 3]]))
}

#[inline]
pub fn bytes(words: &[u32; 4]) -> [u8; 16] {
    let mut b = [0u8; 16];
    for (i, w) in words.iter().enumerate() {
        b[4 * i..4 * i + 4].copy_from_slice(&w.to_le_bytes());
    }
    b
}

#[inline]
pub fn f32s(v: [u32; 4]) -> [f32; 4] {
    v.map(f32::from_bits)
}

#[inline]
pub fn bits(v: [f32; 4]) -> [u32; 4] {
    v.map(f32::to_bits)
}
