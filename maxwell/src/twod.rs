//! The 2D class (902D): blits between surfaces, with scaling and format conversion, and images
//! written through the push buffer (SIFC).
use crate::Memory;
use maxwell_tex::{Class, MsMode, RtFormat, Surface};

#[derive(Clone)]
pub struct Twod {
    regs: Vec<u32>,
    sifc: Vec<u8>,
}

impl Default for Twod {
    fn default() -> Self {
        let mut regs = vec![0; 0x400];
        regs[0x26c / 4] = 1; // always
        Self { regs, sifc: Vec::new() }
    }
}

impl Twod {
    fn r(&self, m: u32) -> u32 {
        self.regs[m as usize / 4]
    }

    /// A 32.32 fixed-point pair (fraction, integer).
    fn fixed(&self, m: u32) -> i64 {
        (self.r(m + 4) as i32 as i64) << 32 | self.r(m) as i64
    }

    fn surface(&self, base: u32) -> Option<(Surface, RtFormat, u32)> {
        let format = RtFormat::from_code(self.r(base) as u8)?;
        let pitch = self.r(base + 4) & 1 != 0;
        let address = ((self.r(base + 0x20) & 0xff) as u64) << 32 | self.r(base + 0x24) as u64;
        let (width, height) = (self.r(base + 0x18), self.r(base + 0x1c));
        let bytes = format.bytes as u32;
        let mode = if pitch { 1 << 12 } else { self.r(base + 8) & 0xfff | if self.r(base + 0xc) > 1 { 1 << 16 } else { 0 } };
        let s = Surface::new(address, if pitch { self.r(base + 0x14) } else { width }, height, mode, 0, bytes, MsMode::X1);
        Some((s, format, if s.is_3d { self.r(base + 0x10) } else { 0 }))
    }

    /// A method; true if memory was written.
    pub fn method(&mut self, m: u32, v: u32, mem: &mut dyn Memory) -> bool {
        if let Some(r) = self.regs.get_mut(m as usize / 4) {
            *r = v;
        }
        match m {
            0x8dc => {
                self.blit(mem);
                true
            }
            0x85c => {
                self.sifc.clear();
                false
            }
            0x860 => {
                self.sifc.extend_from_slice(&v.to_le_bytes());
                self.image(mem)
            }
            _ => false,
        }
    }

    fn clip(&self) -> Option<(i64, i64, i64, i64)> {
        (self.r(0x290) & 1 != 0).then(|| {
            let (x, y) = (self.r(0x280) as i32 as i64, self.r(0x284) as i32 as i64);
            (x, y, x + self.r(0x288) as i64, y + self.r(0x28c) as i64)
        })
    }

    fn blit(&self, mem: &mut dyn Memory) {
        if !self.rendering(mem) {
            return;
        }
        let (Some((src, sf, sz)), Some((dst, df, dz))) = (self.surface(0x230), self.surface(0x200)) else { return };
        // a raster operation of source, destination and the pattern's colour (Mesa's pattern is solid)
        let rop = (self.r(0x2ac) == 4).then(|| {
            let p = self.r(0x2f4).to_be_bytes().map(|b| b as f32 / 255.0);
            (self.r(0x2a0) as u8, df.encode_f32([p[1], p[2], p[3], p[0]]))
        });
        let (dx0, dy0) = (self.r(0x8b0) as i32 as i64, self.r(0x8b4) as i32 as i64);
        let (w, h) = (self.r(0x8b8) as i64, self.r(0x8bc) as i64);
        let (du, dv) = (self.fixed(0x8c0), self.fixed(0x8c8));
        let (u0, v0) = (self.fixed(0x8d0), self.fixed(0x8d8));
        let bilinear = self.r(0x88c) >> 4 & 1 != 0;
        // where the first sample is: the pixel's corner, or its centre
        let centre = self.r(0x88c) & 1 == 0;
        let (half_u, half_v) = if centre { ((du.unsigned_abs() / 2) as i64, (dv.unsigned_abs() / 2) as i64) } else { (0, 0) };
        let (mut cx0, mut cy0, mut cx1, mut cy1) = (0, 0, dst.width as i64, dst.height as i64);
        if let Some((x0, y0, x1, y1)) = self.clip() {
            (cx0, cy0, cx1, cy1) = (cx0.max(x0), cy0.max(y0), cx1.min(x1), cy1.min(y1));
        }
        let (x0, x1) = (dx0.max(cx0), (dx0 + w).min(cx1));
        let (y0, y1) = (dy0.max(cy0), (dy0 + h).min(cy1));
        if x0 >= x1 || y0 >= y1 || src.width == 0 || src.height == 0 {
            return;
        }
        const ONE: i64 = 1 << 32;
        let (sb, db) = (sf.bytes as usize, df.bytes as usize);
        let (src_base, sz) = src.layer(sz);
        let (dst_base, dz) = dst.layer(dz);
        let mut line = vec![0u8; src.width as usize * sb];
        let mut line2 = line.clone();
        let mut out = vec![0u8; (x1 - x0) as usize * db];
        let same = sf == df;
        let (mut at, mut at2) = (-1i64, -1i64);
        let load = |l: &mut [u8], y: i64, mem: &dyn Memory| src.plane.read_rect(mem, src_base, rect(0, y as u32, l.len() as u32, 1), sz, l, l.len());
        for y in y0..y1 {
            let v = v0 + (y - dy0) * dv + half_v;
            if !bilinear {
                let sy = (v >> 32).clamp(0, src.height as i64 - 1);
                if sy != at {
                    at = sy;
                    load(&mut line, sy, mem);
                }
                // a plain copy of a run
                if same && du == ONE && rop.is_none() {
                    let sx = (u0 + (x0 - dx0) * du + half_u) >> 32;
                    if sx >= 0 && sx + (x1 - x0) <= src.width as i64 {
                        out.copy_from_slice(&line[sx as usize * sb..][..(x1 - x0) as usize * sb]);
                        dst.plane.write_rect(mem, dst_base, rect(x0 as u32 * db as u32, y as u32, out.len() as u32, 1), dz, &out, out.len());
                        continue;
                    }
                }
                for x in x0..x1 {
                    let sx = ((u0 + (x - dx0) * du + half_u) >> 32).clamp(0, src.width as i64 - 1) as usize;
                    let p = &line[sx * sb..(sx + 1) * sb];
                    let o = &mut out[(x - x0) as usize * db..][..db];
                    if same {
                        o.copy_from_slice(p);
                    } else {
                        o.copy_from_slice(&convert(&sf, &df, p)[..db]);
                    }
                }
            } else {
                let vv = v - ONE / 2;
                let (ya, yb) = ((vv >> 32).clamp(0, src.height as i64 - 1), ((vv >> 32) + 1).clamp(0, src.height as i64 - 1));
                if ya != at {
                    at = ya;
                    load(&mut line, ya, mem);
                }
                if yb != at2 {
                    at2 = yb;
                    load(&mut line2, yb, mem);
                }
                let fy = (vv & (ONE - 1)) as f32 / ONE as f32;
                for x in x0..x1 {
                    let uu = u0 + (x - dx0) * du + half_u - ONE / 2;
                    let c = |k: i64| (((uu >> 32) + k).clamp(0, src.width as i64 - 1) as usize) * sb;
                    let fx = (uu & (ONE - 1)) as f32 / ONE as f32;
                    let t = |l: &[u8], k: i64| sf.decode_f32(&l[c(k)..c(k) + sb]);
                    let (a, b, p, q) = (t(&line, 0), t(&line, 1), t(&line2, 0), t(&line2, 1));
                    let mix: [f32; 4] = std::array::from_fn(|i| {
                        let (m, n) = (a[i] + (b[i] - a[i]) * fx, p[i] + (q[i] - p[i]) * fx);
                        m + (n - m) * fy
                    });
                    out[(x - x0) as usize * db..][..db].copy_from_slice(&df.encode_f32(mix)[..db]);
                }
            }
            let at = rect(x0 as u32 * db as u32, y as u32, out.len() as u32, 1);
            if let Some((op, pattern)) = &rop {
                let mut old = vec![0u8; out.len()];
                let n = old.len();
                dst.plane.read_rect(mem, dst_base, at, dz, &mut old, n);
                for (k, (o, d)) in out.iter_mut().zip(&old).enumerate() {
                    *o = rop3(*op, pattern[k % db], *o, *d);
                }
            }
            dst.plane.write_rect(mem, dst_base, at, dz, &out, out.len());
        }
    }

    /// Conditional rendering.
    fn rendering(&self, mem: &dyn Memory) -> bool {
        crate::threed::condition(self.r(0x26c), ((self.r(0x264) & 0xff) as u64) << 32 | self.r(0x268) as u64, mem)
    }

    /// An image through the push buffer: written line by line as it comes.
    fn image(&mut self, mem: &mut dyn Memory) -> bool {
        let (w, h) = (self.r(0x838) as usize, self.r(0x83c) as usize);
        // (bitmaps are not drivers' business any more)
        let target = (self.r(0x800) & 1 == 0 && w != 0 && w <= 1 << 16 && h <= 1 << 16).then(|| RtFormat::from_code(self.r(0x804) as u8).zip(self.surface(0x200))).flatten();
        let Some((sf, (dst, df, dz))) = target else {
            self.sifc.clear();
            return false;
        };
        let stride = (w * sf.bytes as usize).next_multiple_of(4);
        if self.sifc.len() < stride * h {
            return false;
        }
        let (x0, y0) = (self.r(0x854), self.r(0x85c));
        let (base, z) = dst.layer(dz);
        let db = df.bytes as usize;
        for y in 0..h {
            let line = &self.sifc[y * stride..][..w * sf.bytes as usize];
            let out: Vec<u8> = if sf == df { line.to_vec() } else { line.chunks_exact(sf.bytes as usize).flat_map(|p| convert(&sf, &df, p)[..db].to_vec()).collect() };
            dst.plane.write_rect(mem, base, rect(x0.wrapping_mul(db as u32) & 0xff_ffff, y0.wrapping_add(y as u32) & 0xff_ffff, out.len() as u32, 1), z, &out, out.len());
        }
        self.sifc.clear();
        true
    }
}

/// A raster operation: bit (pattern, source, destination) of its number, for every bit.
fn rop3(op: u8, p: u8, s: u8, d: u8) -> u8 {
    (0..8).fold(0, |v, k| {
        let i = (p >> k & 1) << 2 | (s >> k & 1) << 1 | d >> k & 1;
        v | (op >> i & 1) << k
    })
}

fn rect(x: u32, y: u32, w: u32, h: u32) -> maxwell_tex::layout::Rect {
    maxwell_tex::layout::Rect { x, y, w, h }
}

fn convert(from: &RtFormat, to: &RtFormat, p: &[u8]) -> [u8; 16] {
    let mut v = from.decode(p);
    if from.class == Class::Float && to.class != Class::Float {
        v = v.map(|b| f32::from_bits(b) as u32);
    }
    to.encode(&v)
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Ram(Vec<u8>);
    impl Memory for Ram {
        fn read(&self, va: u64, out: &mut [u8]) { out.copy_from_slice(&self.0[va as usize..va as usize + out.len()]) }
        fn write(&mut self, va: u64, d: &[u8]) { self.0[va as usize..va as usize + d.len()].copy_from_slice(d) }
    }

    #[test]
    fn blits_copy_convert_and_scale() {
        let mut ram = Ram(vec![0; 0x2000]);
        for k in 0..16u8 {
            ram.0[k as usize * 4..][..4].copy_from_slice(&[k, 100 + k, 200, 255]);
        }
        let mut t = Twod::default();
        let mut set = |m: &[(u32, u32)], ram: &mut Ram| m.iter().for_each(|&(m, v)| { t.method(m, v, ram); });
        // source: 4 x 4 A8B8G8R8 pitch at 0; destination: A8R8G8B8 pitch at 0x1000
        set(&[(0x230, 0xd5), (0x234, 1), (0x244, 16), (0x248, 4), (0x24c, 4), (0x254, 0)], &mut ram);
        set(&[(0x200, 0xcf), (0x204, 1), (0x214, 32), (0x218, 8), (0x21c, 8), (0x224, 0x1000)], &mut ram);
        // scaled up by two
        set(&[(0x88c, 0), (0x8b0, 0), (0x8b4, 0), (0x8b8, 8), (0x8bc, 8), (0x8c0, 1 << 31), (0x8c4, 0), (0x8c8, 1 << 31), (0x8cc, 0), (0x8d0, 0), (0x8d4, 0), (0x8d8, 0), (0x8dc, 0)], &mut ram);
        let px = |x: usize, y: usize| ram.0[0x1000 + 32 * y + 4 * x..][..4].to_vec();
        assert_eq!((px(0, 0), px(1, 1), px(7, 7), px(2, 0)), (vec![200, 100, 0, 255], vec![200, 100, 0, 255], vec![200, 115, 15, 255], vec![200, 101, 1, 255]));
    }
}
