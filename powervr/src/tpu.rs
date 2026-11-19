//! The texture unit (TPU), as the USC's `smp` uses it: texture and sampler state words
//! (Mesa's `csbgen/rogue/texstate.xml`) name an image in memory; a sample fetches a texel
//! and returns its channels.
//!
//! Implemented: 2D twiddled and strided (linear) images, mip level 0, point sampling,
//! normalised or texel coordinates, repeat / clamp-to-edge addressing, swizzles, and the
//! formats Mesa's copies and simple textures use. Anything else is reported, not guessed.
use crate::usc::Memory;

/// Twiddled (Morton) offset of texel (x, y), in texels, in an image of `w` x `h` (rounded
/// up to powers of two): the low bits of x and y interleave with y in the even positions;
/// the remaining high bits of the longer side go above them (Mesa's `usclib_twiddle2d`).
pub fn twiddle(x: u32, y: u32, w: u32, h: u32) -> u64 {
    let (w, h) = (w.max(1).next_power_of_two(), h.max(1).next_power_of_two());
    let common = w.min(h).trailing_zeros();
    let mut t = 0u64;
    for i in 0..common {
        t |= (((y >> i) & 1) as u64) << (2 * i);
        t |= (((x >> i) & 1) as u64) << (2 * i + 1);
    }
    let rest = ((x >> common) | (y >> common)) as u64;
    t | rest << (2 * common)
}

/// Bytes per texel and how channels are stored, for a TPU texture format.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Chan {
    /// unsigned normalised (or integer, when sampled as integer) 8-bit channels
    Unorm8(u32),
    /// signed normalised 8-bit channels
    Snorm8(u32),
    Unorm16(u32),
    F16(u32),
    /// 32-bit channels returned as stored (integers or floats)
    Raw32(u32),
}

fn format(fmt: u32) -> Result<Chan, String> {
    Ok(match fmt {
        0 => Chan::Unorm8(1),
        7 => Chan::Unorm8(2),
        12 => Chan::Unorm8(4),
        58 => Chan::Unorm8(3),
        1 => Chan::Snorm8(1),
        8 => Chan::Snorm8(2),
        13 => Chan::Snorm8(4),
        9 => Chan::Unorm16(1),
        15 => Chan::Unorm16(2),
        29 => Chan::Unorm16(4),
        11 => Chan::F16(1),
        17 => Chan::F16(2),
        28 => Chan::F16(4),
        18 | 24 | 25 => Chan::Raw32(1),
        34..=36 => Chan::Raw32(2),
        64..=66 => Chan::Raw32(3),
        61..=63 => Chan::Raw32(4),
        f => return Err(format!("texture format {f} not implemented")),
    })
}

/// Integer (raw) formats return 1 for a missing alpha; the others 1.0.
fn is_integer_format(fmt: u32) -> bool {
    matches!(fmt, 24 | 25 | 35 | 36 | 62 | 63 | 65 | 66)
}

fn f16_to_f32(h: u16) -> f32 {
    let sign = if h & 0x8000 != 0 { -1.0 } else { 1.0 };
    let e = ((h >> 10) & 0x1f) as i32;
    let m = (h & 0x3ff) as f32;
    sign * match e {
        0 => m * (2f32).powi(-24),
        0x1f => {
            if m == 0.0 {
                f32::INFINITY
            } else {
                f32::NAN
            }
        }
        e => (1.0 + m / 1024.0) * (2f32).powi(e - 15),
    }
}

/// Apply an address mode to texel coordinate `c` in [0, n).
fn address(c: i64, n: u32, mode: u64) -> Result<u32, String> {
    let n = n as i64;
    Ok(match mode {
        0 => c.rem_euclid(n) as u32,
        2 | 5 => c.clamp(0, n - 1) as u32,
        m => return Err(format!("texture address mode {m} not implemented")),
    })
}

/// Sample image `img` (IMAGE_WORD0/1 as four dwords) with sampler `smp` (SAMPLER_WORD0/1)
/// at `coords`; returns four channels as the USC receives them (floats, or raw values for
/// integer formats and integer sampling).
pub fn sample(mem: &mut dyn Memory, img: [u32; 4], smp: [u32; 4], coords: &[f32], nncoords: bool, integer: bool) -> Result<[u32; 4], String> {
    let w0 = img[0] as u64 | (img[1] as u64) << 32;
    let w1 = img[2] as u64 | (img[3] as u64) << 32;
    let s0 = smp[0] as u64 | (smp[1] as u64) << 32;
    let textype = w0 & 7;
    let fmt = ((w0 >> 27) & 0x7f) as u32;
    let width = ((w0 >> 34) & 0x3fff) as u32 + 1;
    let height = ((w0 >> 48) & 0x3fff) as u32 + 1;
    let addr = ((w1 >> 16) & ((1 << 38) - 1)) << 2;
    let (magf, minf) = ((s0 >> 36) & 3, (s0 >> 38) & 3);
    if magf != 0 || minf != 0 {
        return Err("linear / bicubic texture filtering not implemented".into());
    }
    let nn = nncoords || s0 & (1 << 49) != 0;
    let (u, v) = (coords.first().copied().unwrap_or(0.0), coords.get(1).copied().unwrap_or(0.0));
    let (u, v) = if nn { (u, v) } else { (u * width as f32, v * height as f32) };
    let x = address(u.floor() as i64, width, (s0 >> 41) & 7)?;
    let y = if textype == 0 { 0 } else { address(v.floor() as i64, height, (s0 >> 44) & 7)? };
    let chan = format(fmt)?;
    let bpp = match chan {
        Chan::Unorm8(n) | Chan::Snorm8(n) => n,
        Chan::Unorm16(n) | Chan::F16(n) => 2 * n,
        Chan::Raw32(n) => 4 * n,
    } as u64;
    let offset = match textype {
        // 1D / 2D: twiddled
        0 | 1 => twiddle(x, y, width, height) * bpp,
        4 => {
            let stride = (w1 & 0x7fff) + 1;
            (y as u64 * stride + x as u64) * bpp
        }
        t => return Err(format!("texture type {t} not implemented")),
    };
    let mut bytes = [0u8; 16];
    for i in 0..bpp {
        let a = addr + offset + i;
        let word = mem.read32(a & !3).ok_or(format!("texture read from unmapped {a:#x}"))?;
        bytes[i as usize] = (word >> (8 * (a & 3))) as u8;
    }
    let one = if is_integer_format(fmt) || integer { 1 } else { 1.0f32.to_bits() };
    let mut c = [0u32, 0, 0, one];
    match chan {
        Chan::Unorm8(n) => {
            for i in 0..n as usize {
                c[i] = if integer { bytes[i] as u32 } else { (bytes[i] as f32 / 255.0).to_bits() };
            }
        }
        Chan::Snorm8(n) => {
            for i in 0..n as usize {
                let b = bytes[i] as i8;
                c[i] = if integer { b as i32 as u32 } else { (b as f32 / 127.0).max(-1.0).to_bits() };
            }
        }
        Chan::Unorm16(n) => {
            for i in 0..n as usize {
                let h = u16::from_le_bytes([bytes[2 * i], bytes[2 * i + 1]]);
                c[i] = if integer { h as u32 } else { (h as f32 / 65535.0).to_bits() };
            }
        }
        Chan::F16(n) => {
            for i in 0..n as usize {
                c[i] = f16_to_f32(u16::from_le_bytes([bytes[2 * i], bytes[2 * i + 1]])).to_bits();
            }
        }
        Chan::Raw32(n) => {
            for i in 0..n as usize {
                c[i] = u32::from_le_bytes(bytes[4 * i..4 * i + 4].try_into().unwrap());
            }
        }
    }
    // SWIZ: 0..3 source channel, 4 one, 5 zero
    let swz = |sh: u64| -> u32 {
        match (w0 >> sh) & 7 {
            s @ 0..=3 => c[s as usize],
            4 => one,
            _ => 0,
        }
    };
    Ok([swz(14), swz(11), swz(8), swz(5)])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn twiddle_interleaves_y_in_even_bits() {
        assert_eq!(twiddle(0, 0, 64, 64), 0);
        assert_eq!(twiddle(0, 1, 64, 64), 1);
        assert_eq!(twiddle(1, 0, 64, 64), 2);
        assert_eq!(twiddle(1, 1, 64, 64), 3);
        assert_eq!(twiddle(2, 0, 64, 64), 8);
        assert_eq!(twiddle(63, 63, 64, 64), 4095);
        // non-square: the extra bits of the long side go on top
        assert_eq!(twiddle(4, 0, 8, 4), 16);
        assert_eq!(twiddle(5, 3, 8, 4), 16 + twiddle(1, 3, 4, 4));
    }
}
