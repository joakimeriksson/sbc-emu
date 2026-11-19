//! The texture unit (TPU), as the USC's `smp` uses it: texture and sampler state words
//! (Mesa's `csbgen/rogue/texstate.xml`) name an image in memory; a sample fetches a texel
//! and returns its channels.
//!
//! Implemented: 2D twiddled and strided (linear) images, mip levels (twiddled), point and
//! bilinear filtering with nearest or linear mip selection from quad gradients, normalised
//! or texel coordinates, repeat / clamp-to-edge addressing, sRGB decode, swizzles, and the
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

/// IEEE half to f32.
pub fn f16_to_f32(h: u16) -> f32 {
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

/// An image as the texture state words describe it.
struct Image {
    textype: u64,
    fmt: u32,
    chan: Chan,
    bpp: u64,
    width: u32,
    height: u32,
    addr: u64,
    stride: u64,
    levels: u32,
    base: u32,
    srgb: bool,
}

impl Image {
    fn decode(img: [u32; 4]) -> Result<(Image, u64), String> {
        let w0 = img[0] as u64 | (img[1] as u64) << 32;
        let w1 = img[2] as u64 | (img[3] as u64) << 32;
        let textype = w0 & 7;
        let fmt = ((w0 >> 27) & 0x7f) as u32;
        let chan = format(fmt)?;
        let bpp = match chan {
            Chan::Unorm8(n) | Chan::Snorm8(n) => n,
            Chan::Unorm16(n) | Chan::F16(n) => 2 * n,
            Chan::Raw32(n) => 4 * n,
        } as u64;
        // mip levels: IMAGE_WORD1 num_mip_levels 0..3 / baselevel 60..63 (twiddled),
        // STRIDE_IMAGE_WORD1 num_mip_levels 60..63; mipmaps_present 15
        let mipmaps = w1 & (1 << 15) != 0;
        let (levels, base, stride) = match textype {
            4 => (if mipmaps { (w1 >> 60) as u32 } else { 1 }, 0, (w1 & 0x7fff) + 1),
            _ => (if mipmaps { (w1 & 0xf) as u32 } else { 1 }, (w1 >> 60) as u32, 0),
        };
        if textype == 4 && levels > 1 {
            return Err("mipmapped strided textures not implemented".into());
        }
        let image = Image {
            textype,
            fmt,
            chan,
            bpp,
            width: ((w0 >> 34) & 0x3fff) as u32 + 1,
            height: ((w0 >> 48) & 0x3fff) as u32 + 1,
            addr: ((w1 >> 16) & ((1 << 38) - 1)) << 2,
            stride,
            levels: levels.max(1),
            base,
            // gamma: sRGB-encoded colour channels
            srgb: (w0 >> 3) & 1 != 0,
        };
        Ok((image, w0))
    }

    /// Size and byte offset of mip level `l`: levels follow each other, each twiddled
    /// over its power-of-two extent (Mesa's `pvr_image_plane_setup_mip_levels`).
    fn level(&self, l: u32) -> (u32, u32, u64) {
        let (mut w, mut h, mut off) = (self.width, self.height, 0u64);
        for _ in 0..l {
            off += w.next_power_of_two() as u64 * h.next_power_of_two() as u64 * self.bpp;
            w = (w / 2).max(1);
            h = (h / 2).max(1);
        }
        (w, h, off)
    }

    /// The raw channels of texel (x, y) of level `l` (coordinates already addressed).
    fn texel(&self, mem: &mut dyn Memory, l: u32, x: u32, y: u32) -> Result<[u32; 4], String> {
        let (w, h, level_off) = self.level(l);
        let offset = level_off
            + match self.textype {
                0 | 1 => twiddle(x, y, w, h) * self.bpp,
                4 => (y as u64 * self.stride + x as u64) * self.bpp,
                t => return Err(format!("texture type {t} not implemented")),
            };
        let mut bytes = [0u8; 16];
        for i in 0..self.bpp {
            let a = self.addr + offset + i;
            let word = mem.read32(a & !3).ok_or(format!("texture read from unmapped {a:#x}"))?;
            bytes[i as usize] = (word >> (8 * (a & 3))) as u8;
        }
        let mut c = [0u32; 4];
        match self.chan {
            Chan::Unorm8(n) | Chan::Snorm8(n) => (0..n as usize).for_each(|i| c[i] = bytes[i] as u32),
            Chan::Unorm16(n) | Chan::F16(n) => (0..n as usize).for_each(|i| c[i] = u16::from_le_bytes([bytes[2 * i], bytes[2 * i + 1]]) as u32),
            Chan::Raw32(n) => (0..n as usize).for_each(|i| c[i] = u32::from_le_bytes(bytes[4 * i..4 * i + 4].try_into().unwrap())),
        }
        Ok(c)
    }

    fn channels(&self) -> usize {
        match self.chan {
            Chan::Unorm8(n) | Chan::Snorm8(n) | Chan::Unorm16(n) | Chan::F16(n) | Chan::Raw32(n) => n as usize,
        }
    }

    /// Raw channels as floats (normalised formats scaled, F16/F32 converted).
    fn to_float(&self, c: [u32; 4]) -> Result<[f32; 4], String> {
        let mut out = [0f32; 4];
        for i in 0..self.channels() {
            out[i] = match self.chan {
                Chan::Unorm8(_) => {
                    let v = c[i] as f32 / 255.0;
                    if self.srgb && i < 3 {
                        srgb_to_linear(v)
                    } else {
                        v
                    }
                }
                Chan::Snorm8(_) => (c[i] as u8 as i8 as f32 / 127.0).max(-1.0),
                Chan::Unorm16(_) => c[i] as f32 / 65535.0,
                Chan::F16(_) => f16_to_f32(c[i] as u16),
                Chan::Raw32(_) if self.fmt == 18 || self.fmt == 34 || self.fmt == 61 || self.fmt == 64 => f32::from_bits(c[i]),
                Chan::Raw32(_) => return Err("filtering an integer texture".into()),
            };
        }
        Ok(out)
    }
}

fn srgb_to_linear(c: f32) -> f32 {
    if c <= 0.04045 {
        c / 12.92
    } else {
        ((c + 0.055) / 1.055).powf(2.4)
    }
}

/// Sample image `img` (IMAGE_WORD0/1 as four dwords) with sampler `smp` (SAMPLER_WORD0/1)
/// at `coords`; returns four channels as the USC receives them (floats, or raw values for
/// integer formats and integer sampling).
///
/// The level of detail comes from the coordinate gradients across the pixel quad
/// (du/dx, dv/dx, du/dy, dv/dy, normalised): lambda = log2 of the longer of the two
/// gradient vectors in texels of the base level, plus the sampler's bias, clamped to its
/// min/max LOD. Magnification (lambda <= 0) uses the mag filter on the base level;
/// minification the min filter on the nearest level, or a blend of the two nearest with a
/// linear mip filter. Without gradients (texel coordinates, the transfer copies) the base
/// level is sampled with the mag filter.
pub fn sample(mem: &mut dyn Memory, img: [u32; 4], smp: [u32; 4], coords: &[f32], nncoords: bool, integer: bool, grads: Option<[f32; 4]>) -> Result<[u32; 4], String> {
    let (image, w0) = Image::decode(img)?;
    let s0 = smp[0] as u64 | (smp[1] as u64) << 32;
    let (magf, minf, mipf) = ((s0 >> 36) & 3, (s0 >> 38) & 3, s0 & (1 << 40) != 0);
    let nn = nncoords || s0 & (1 << 49) != 0;
    let (u, v) = (coords.first().copied().unwrap_or(0.0), coords.get(1).copied().unwrap_or(0.0));
    let modes = ((s0 >> 41) & 7, (s0 >> 44) & 7);
    let raw = is_integer_format(image.fmt) || integer;
    let one = if raw { 1 } else { 1.0f32.to_bits() };
    // level of detail
    let mut lambda = 0f32;
    if let (Some(g), false) = (grads, nn) {
        let (w, h) = (image.width as f32, image.height as f32);
        let rx = ((g[0] * w).powi(2) + (g[1] * h).powi(2)).sqrt();
        let ry = ((g[2] * w).powi(2) + (g[3] * h).powi(2)).sqrt();
        let bias = (((s0 & 0x1fff) as f32) - 4095.0) / 256.0;
        lambda = rx.max(ry).max(f32::MIN_POSITIVE).log2() + bias;
        let (minlod, maxlod) = (((s0 >> 13) & 0x3ff) as f32 / 64.0, ((s0 >> 23) & 0x3ff) as f32 / 64.0);
        lambda = lambda.max(minlod).min(maxlod);
    }
    let last = image.levels - 1;
    let (filter, levels): (u64, Vec<(u32, f32)>) = if lambda <= 0.0 {
        (magf, vec![(image.base, 1.0)])
    } else if image.levels == 1 {
        (minf, vec![(image.base, 1.0)])
    } else if mipf {
        let l0 = (lambda.floor() as u32).min(last);
        let l1 = (l0 + 1).min(last);
        let t = lambda - lambda.floor();
        (minf, vec![(image.base + l0, 1.0 - t), (image.base + l1, t)])
    } else {
        (minf, vec![(image.base + ((lambda + 0.5).floor() as u32).min(last), 1.0)])
    };
    if filter > 1 {
        return Err("bicubic texture filtering not implemented".into());
    }
    let mut c = [0u32, 0, 0, one];
    if filter == 0 || raw {
        // point sampling (integer formats are never filtered)
        let (l, _) = levels[0];
        let (w, h, _) = image.level(l);
        let (x, y) = if nn { (u, v) } else { (u * w as f32, v * h as f32) };
        let x = address(x.floor() as i64, w, modes.0)?;
        let y = if image.textype == 0 { 0 } else { address(y.floor() as i64, h, modes.1)? };
        let t = image.texel(mem, l, x, y)?;
        let n = image.channels();
        if raw {
            c[..n].copy_from_slice(&t[..n]);
        } else {
            let fl = image.to_float(t)?;
            for i in 0..n {
                c[i] = fl[i].to_bits();
            }
        }
    } else {
        // bilinear on each level, blended across levels
        let mut acc = [0f32; 4];
        for &(l, weight) in &levels {
            let (w, h, _) = image.level(l);
            let (x, y) = if nn { (u - 0.5, v - 0.5) } else { (u * w as f32 - 0.5, v * h as f32 - 0.5) };
            let (x0, y0) = (x.floor(), y.floor());
            let (fx, fy) = (x - x0, y - y0);
            for (dx, dy, wt) in [(0, 0, (1.0 - fx) * (1.0 - fy)), (1, 0, fx * (1.0 - fy)), (0, 1, (1.0 - fx) * fy), (1, 1, fx * fy)] {
                let tx = address((x0 as i64).saturating_add(dx), w, modes.0)?;
                let ty = if image.textype == 0 { 0 } else { address((y0 as i64).saturating_add(dy), h, modes.1)? };
                let t = image.to_float(image.texel(mem, l, tx, ty)?)?;
                for i in 0..4 {
                    acc[i] += t[i] * wt * weight;
                }
            }
        }
        for i in 0..image.channels() {
            c[i] = acc[i].to_bits();
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
