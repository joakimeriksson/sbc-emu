//! The texture unit (TMU): a request is the coordinates a program wrote (S last; T, R, B
//! before it) and the configuration words the TMU took from the program's uniform stream, one
//! per write (P0, P1, then P2/P3):
//!
//! * P0: base address (4 KiB aligned) | cube map | flip Y | type[3:0] | mip levels - 1
//! * P1: type[4] | height | ETC flip | width | mag filter | min filter | wrap T | wrap S
//! * P2/P3 (optional): cube map stride and "bias is the LOD", or child image parameters
//!
//! The result, per lane, is a texel in 8-bit RGBA order (byte 0 = red), which programs unpack
//! from r4 with the PM=1 unpack modes. Mip levels sit below level 0 in memory, smallest first;
//! levels 1.. have power-of-two sizes; each is T-tiled unless it is small enough for LT;
//! RGBA32R is raster. Filtering: nearest, bilinear, the mipmap modes, LOD from the 2x2 quads
//! of a fragment program (level 0 elsewhere unless the LOD is given).

use crate::mem::Mem;
use crate::qpu::{ff, V, ZERO};
use crate::tiling::{self, Layout};

pub const RGBA8888: u32 = 0;
pub const RGBX8888: u32 = 1;
pub const RGBA4444: u32 = 2;
pub const RGBA5551: u32 = 3;
pub const RGB565: u32 = 4;
pub const LUMINANCE: u32 = 5;
pub const ALPHA: u32 = 6;
pub const LUMALPHA: u32 = 7;
pub const ETC1: u32 = 8;
pub const S16F: u32 = 9;
pub const S8: u32 = 10;
pub const S16: u32 = 11;
pub const BW1: u32 = 12;
pub const A4: u32 = 13;
pub const A1: u32 = 14;
pub const RGBA64: u32 = 15;
pub const RGBA32R: u32 = 16;
pub const YUV422R: u32 = 17;

pub const WRAP_REPEAT: u32 = 0;
pub const WRAP_CLAMP: u32 = 1;
pub const WRAP_MIRROR: u32 = 2;
pub const WRAP_BORDER: u32 = 3;

/// Bytes per stored element (ETC1: one 4x4 block of 8 bytes).
pub fn cpp(ty: u32) -> u32 {
    match ty {
        RGBA8888 | RGBX8888 | RGBA32R => 4,
        RGBA4444 | RGBA5551 | RGB565 | LUMALPHA | S16F | S16 => 2,
        ETC1 | RGBA64 => 8,
        _ => 1,
    }
}

/// A decoded texture configuration.
#[derive(Clone, Copy, Debug)]
pub struct Tex {
    pub base: u32,
    pub ty: u32,
    pub levels: u32,
    pub cube: bool,
    pub flipy: bool,
    pub width: u32,
    pub height: u32,
    pub etcflip: bool,
    pub mag_nearest: bool,
    pub minfilt: u32,
    pub wrap_s: u32,
    pub wrap_t: u32,
    pub cube_stride: u32,
    pub bias_is_lod: bool,
}

impl Tex {
    pub fn new(cfg: &[u32]) -> Tex {
        let p0 = cfg.first().copied().unwrap_or(0);
        let p1 = cfg.get(1).copied().unwrap_or(0);
        let mut t = Tex {
            base: p0 & 0xffff_f000,
            ty: (p0 >> 4 & 15) | (p1 >> 31) << 4,
            levels: p0 & 15,
            cube: p0 >> 9 & 1 != 0,
            flipy: p0 >> 8 & 1 != 0,
            height: match p1 >> 20 & 0x7ff { 0 => 2048, h => h },
            etcflip: p1 >> 19 & 1 != 0,
            width: match p1 >> 8 & 0x7ff { 0 => 2048, w => w },
            mag_nearest: p1 >> 7 & 1 != 0,
            minfilt: p1 >> 4 & 7,
            wrap_t: p1 >> 2 & 3,
            wrap_s: p1 & 3,
            cube_stride: 0,
            bias_is_lod: false,
        };
        for &p in cfg.iter().skip(2) {
            if p >> 30 == 1 {
                t.cube_stride = p & 0x3fff_f000;
                t.bias_is_lod = p & 1 != 0;
            }
        }
        t
    }

    /// Level `l`'s size in pixels.
    pub fn level_size(&self, l: u32) -> (u32, u32) {
        if l == 0 { return (self.width, self.height); }
        ((self.width.next_power_of_two() >> l).max(1), (self.height.next_power_of_two() >> l).max(1))
    }

    /// Level `l`'s stored size (ETC1 in blocks) and layout.
    fn level_layout(&self, l: u32) -> (u32, u32, Layout) {
        let c = cpp(self.ty);
        let (w, h) = if self.ty == ETC1 {
            let (bw, bh) = (self.width.div_ceil(4), self.height.div_ceil(4));
            if l == 0 { (bw, bh) } else { ((bw.next_power_of_two() >> l).max(1), (bh.next_power_of_two() >> l).max(1)) }
        } else { self.level_size(l) };
        let tiling = if self.ty == RGBA32R { tiling::RASTER } else if tiling::size_is_lt(w, h, c) { tiling::LT } else { tiling::T };
        let (aw, ah) = tiling::aligned(w, h, c, tiling);
        (aw, ah, Layout::new(tiling, c, aw))
    }

    /// Base addresses of the levels (level 0 at `base`, smaller ones below it).
    pub fn level_bases(&self) -> Vec<(u32, Layout)> {
        let mut v = Vec::with_capacity(self.levels as usize + 1);
        let (_, _, l0) = self.level_layout(0);
        v.push((self.base, l0));
        let mut a = self.base;
        for l in 1..=self.levels {
            let (aw, ah, lay) = self.level_layout(l);
            a = a.wrapping_sub(aw * ah * cpp(self.ty));
            v.push((a, lay));
        }
        v
    }
}

/// The texel at (x, y) of a level at `base` as RGBA bytes.
pub fn texel(t: &Tex, base: u32, lay: &Layout, x: u32, y: u32, mem: &mut Mem) -> [u8; 4] {
    let n4 = |v: u32| (v * 17) as u8;
    let e5 = |v: u32| ((v << 3) | (v >> 2)) as u8;
    let e6 = |v: u32| ((v << 2) | (v >> 4)) as u8;
    match t.ty {
        ETC1 => {
            let a = base.wrapping_add(lay.offset(x / 4, y / 4));
            let mut b = [0u8; 8];
            mem.read(a, &mut b);
            etc1_pixel(&b, x % 4, y % 4)
        }
        _ => {
            let a = base.wrapping_add(lay.offset(x, y));
            match t.ty {
                RGBA8888 | RGBA32R => mem.r32(a).to_le_bytes(),
                RGBX8888 => { let mut b = mem.r32(a).to_le_bytes(); b[3] = 255; b }
                RGBA4444 => { let h = mem.r16(a) as u32; [n4(h >> 12), n4(h >> 8 & 15), n4(h >> 4 & 15), n4(h & 15)] }
                RGBA5551 => { let h = mem.r16(a) as u32; [e5(h >> 11), e5(h >> 6 & 31), e5(h >> 1 & 31), ((h & 1) * 255) as u8] }
                RGB565 => { let h = mem.r16(a) as u32; [e5(h >> 11), e6(h >> 5 & 63), e5(h & 31), 255] }
                LUMINANCE => { let l = mem.r8(a); [l, l, l, 255] }
                ALPHA => [0, 0, 0, mem.r8(a)],
                LUMALPHA => { let h = mem.r16(a); let l = h as u8; [l, l, l, (h >> 8) as u8] }
                S8 => [mem.r8(a), 0, 0, 0],
                S16 | S16F => { let h = mem.r16(a).to_le_bytes(); [h[0], h[1], 0, 0] }
                _ => { crate::log_first(&format!("vc4: texture type {} not modelled", t.ty)); [0; 4] }
            }
        }
    }
}

/// One pixel of an ETC1 block (standard big-endian layout).
pub fn etc1_pixel(b: &[u8; 8], x: u32, y: u32) -> [u8; 4] {
    let hi = u32::from_be_bytes([b[0], b[1], b[2], b[3]]);
    let lo = u32::from_be_bytes([b[4], b[5], b[6], b[7]]);
    let diff = hi & 2 != 0;
    let flip = hi & 1 != 0;
    let sub = if flip { (y >= 2) as u32 } else { (x >= 2) as u32 };
    let (r, g, bb) = if diff {
        let base = |sh: u32| (hi >> sh >> 3) & 31;
        let delta = |sh: u32| { let d = (hi >> sh) & 7; (d << 29) as i32 >> 29 };
        let c5 = |sh: u32| -> u32 {
            let v = base(sh) as i32 + if sub == 1 { delta(sh) } else { 0 };
            let v = v.clamp(0, 31) as u32;
            (v << 3) | (v >> 2)
        };
        (c5(24), c5(16), c5(8))
    } else {
        let c4 = |sh: u32| -> u32 { let v = (hi >> (sh + if sub == 1 { 0 } else { 4 })) & 15; v * 17 };
        (c4(24), c4(16), c4(8))
    };
    const TABLES: [[i32; 4]; 8] = [[2, 8, -2, -8], [5, 17, -5, -17], [9, 29, -9, -29], [13, 42, -13, -42],
                                   [18, 60, -18, -60], [24, 80, -24, -80], [33, 106, -33, -106], [47, 183, -47, -183]];
    let tbl = if sub == 1 { hi >> 2 & 7 } else { hi >> 5 & 7 };
    let idx = x * 4 + y;
    let msb = lo >> (16 + idx) & 1;
    let lsb = lo >> idx & 1;
    let m = TABLES[tbl as usize][(msb << 1 | lsb) as usize];
    let c = |v: u32| (v as i32 + m).clamp(0, 255) as u8;
    [c(r), c(g), c(bb), 255]
}

fn wrap(i: i32, n: i32, mode: u32) -> Option<i32> {
    match mode {
        WRAP_REPEAT => Some(i.rem_euclid(n)),
        WRAP_MIRROR => { let p = i.rem_euclid(2 * n); Some(if p < n { p } else { 2 * n - 1 - p }) }
        WRAP_BORDER => if i < 0 || i >= n { None } else { Some(i) },
        _ => Some(i.clamp(0, n - 1)),
    }
}

/// Sample one level at normalised (s, t): nearest or bilinear.
#[allow(clippy::too_many_arguments)]
fn sample_level(t: &Tex, lv: u32, bases: &[(u32, Layout)], face_off: u32, s: f32, tt: f32, linear: bool, border: [u8; 4], mem: &mut Mem) -> [f32; 4] {
    let (w, h) = t.level_size(lv);
    let (base, lay) = bases[lv as usize];
    let base = base.wrapping_add(face_off);
    let fetch = |x: i32, y: i32, mem: &mut Mem| -> [u8; 4] {
        match (wrap(x, w as i32, t.wrap_s), wrap(y, h as i32, t.wrap_t)) {
            (Some(x), Some(y)) => texel(t, base, &lay, x as u32, y as u32, mem),
            _ => border,
        }
    };
    let u = s * w as f32;
    let v = tt * h as f32;
    if !linear {
        let p = fetch(u.floor() as i32, v.floor() as i32, mem);
        return p.map(|c| c as f32);
    }
    let (u, v) = (u - 0.5, v - 0.5);
    let (x0, y0) = (u.floor(), v.floor());
    let (fx, fy) = (u - x0, v - y0);
    let (x0, y0) = (x0 as i32, y0 as i32);
    let p00 = fetch(x0, y0, mem);
    let p10 = fetch(x0 + 1, y0, mem);
    let p01 = fetch(x0, y0 + 1, mem);
    let p11 = fetch(x0 + 1, y0 + 1, mem);
    std::array::from_fn(|c| {
        let a = p00[c] as f32 * (1.0 - fx) + p10[c] as f32 * fx;
        let b = p01[c] as f32 * (1.0 - fx) + p11[c] as f32 * fx;
        a * (1.0 - fy) + b * fy
    })
}

/// Cube map face and face coordinates for direction (s, t, r) (OpenGL's rules).
pub fn cube_face(s: f32, t: f32, r: f32) -> (u32, f32, f32) {
    let (ax, ay, az) = (s.abs(), t.abs(), r.abs());
    let (face, sc, tc, ma) = if ax >= ay && ax >= az {
        if s >= 0.0 { (0, -r, -t, ax) } else { (1, r, -t, ax) }
    } else if ay >= az {
        if t >= 0.0 { (2, s, r, ay) } else { (3, s, -r, ay) }
    } else if r >= 0.0 { (4, s, -t, az) } else { (5, -s, -t, az) };
    let ma = if ma == 0.0 { 1.0 } else { ma };
    (face, (sc / ma + 1.0) * 0.5, (tc / ma + 1.0) * 0.5)
}

/// Run a texture request: coordinates S, T, R, B as written (lanes in `mask`), the
/// configuration words, whether LODs come from fragment quads.
pub fn lookup(coord: &[Option<V>; 4], cfg: &[u32], mask: u16, fragment: bool, mem: &mut Mem) -> V {
    let t = Tex::new(cfg);
    if cfg.len() < 2 { crate::log_first("vc4: TMU request with fewer than two configuration words"); }
    let s_in = coord[0].unwrap_or(ZERO);
    let t_in = coord[1].unwrap_or(ZERO);
    let r_in = coord[2];
    let b_in = coord[3];
    let bases = t.level_bases();
    let mut ss = [0f32; 16];
    let mut ts = [0f32; 16];
    let mut faces = [0u32; 16];
    for i in 0..16 {
        let (mut s, mut tt) = (ff(s_in[i]), ff(t_in[i]));
        if t.cube {
            let (f, a, b) = cube_face(s, tt, ff(r_in.map_or(0, |r| r[i])));
            faces[i] = f;
            s = a;
            tt = b;
        }
        if t.flipy { tt = 1.0 - tt; }
        ss[i] = s;
        ts[i] = tt;
    }
    let mut out = ZERO;
    let (w0, h0) = (t.width as f32, t.height as f32);
    for q in 0..4 {
        let l0 = q * 4;
        let lod_quad = if fragment {
            let dsdx = (ss[l0 + 1] - ss[l0]) * w0;
            let dtdx = (ts[l0 + 1] - ts[l0]) * h0;
            let dsdy = (ss[l0 + 2] - ss[l0]) * w0;
            let dtdy = (ts[l0 + 2] - ts[l0]) * h0;
            let rho = (dsdx * dsdx + dtdx * dtdx).max(dsdy * dsdy + dtdy * dtdy).sqrt();
            if rho > 0.0 { rho.log2() } else { -1000.0 }
        } else { 0.0 };
        for i in l0..l0 + 4 {
            if mask >> i & 1 == 0 { continue; }
            let mut lod = lod_quad;
            if let Some(b) = b_in {
                let b = ff(b[i]);
                if t.bias_is_lod { lod = b } else { lod += b }
            }
            let border = if t.cube { [0; 4] } else { r_in.map_or([0; 4], |r| r[i].to_le_bytes()) };
            let face_off = faces[i].wrapping_mul(t.cube_stride);
            let maxl = t.levels as f32;
            let px: [f32; 4] = if lod <= 0.0 {
                sample_level(&t, 0, &bases, face_off, ss[i], ts[i], !t.mag_nearest, border, mem)
            } else {
                let lod = lod.min(maxl);
                match t.minfilt {
                    0 | 1 => sample_level(&t, 0, &bases, face_off, ss[i], ts[i], t.minfilt == 0, border, mem),
                    2 | 4 => {
                        let l = ((lod + 0.5).ceil() - 1.0).clamp(0.0, maxl) as u32;
                        sample_level(&t, l, &bases, face_off, ss[i], ts[i], t.minfilt == 4, border, mem)
                    }
                    _ => {
                        let lin = t.minfilt == 5;
                        let l = lod.floor() as u32;
                        let fr = lod - l as f32;
                        let a = sample_level(&t, l, &bases, face_off, ss[i], ts[i], lin, border, mem);
                        if l >= t.levels || fr == 0.0 { a } else {
                            let b = sample_level(&t, l + 1, &bases, face_off, ss[i], ts[i], lin, border, mem);
                            std::array::from_fn(|c| a[c] * (1.0 - fr) + b[c] * fr)
                        }
                    }
                }
            };
            let b = px.map(|c| (c + 0.5).clamp(0.0, 255.0) as u8);
            out[i] = u32::from_le_bytes(b);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg(base: u32, ty: u32, w: u32, h: u32, levels: u32, minfilt: u32, mag_nearest: bool, wrap: u32) -> Vec<u32> {
        vec![base | (ty & 15) << 4 | levels, (ty >> 4) << 31 | (h & 0x7ff) << 20 | (w & 0x7ff) << 8 | (mag_nearest as u32) << 7 | minfilt << 4 | wrap << 2 | wrap]
    }

    fn put_image(ram: &mut [u8], base: u32, lay: &Layout, w: u32, h: u32, px: impl Fn(u32, u32) -> u32) {
        for y in 0..h { for x in 0..w {
            let o = (base + lay.offset(x, y)) as usize;
            ram[o..o + 4].copy_from_slice(&px(x, y).to_le_bytes());
        } }
    }

    fn coords(s: f32, t: f32) -> [Option<V>; 4] { [Some([s.to_bits(); 16]), Some([t.to_bits(); 16]), None, None] }

    #[test]
    fn nearest_t_tiled_rgba() {
        let mut ram = vec![0u8; 1 << 20];
        let (w, h) = (64, 64);
        let base = 0x1_0000;
        let lay = Layout::new(tiling::T, 4, w);
        put_image(&mut ram, base, &lay, w, h, |x, y| x | y << 8 | 0xff00_0000);
        let mut m = Mem::new(&mut ram);
        let c = cfg(base, RGBA8888, w, h, 0, 1, true, WRAP_REPEAT);
        let r = lookup(&coords((10.5) / 64.0, (37.5) / 64.0), &c, ALL_LANES, false, &mut m);
        assert_eq!(r[0], 10 | 37 << 8 | 0xff00_0000);
        // repeat wraps
        let r = lookup(&coords(1.0 + 3.5 / 64.0, -0.5 / 64.0), &c, ALL_LANES, false, &mut m);
        assert_eq!(r[5], 3 | 63 << 8 | 0xff00_0000);
    }

    const ALL_LANES: u16 = 0xffff;

    #[test]
    fn bilinear_and_clamp() {
        let mut ram = vec![0u8; 1 << 20];
        let base = 0x1_0000;
        let lay = Layout::new(tiling::LT, 4, 4);
        put_image(&mut ram, base, &lay, 4, 4, |x, _| if x < 2 { 0 } else { 0xffff_ffff });
        let mut m = Mem::new(&mut ram);
        let c = cfg(base, RGBA8888, 4, 4, 0, 0, false, WRAP_CLAMP);
        // halfway between texel 1 and 2
        let r = lookup(&coords(0.5, 0.5), &c, ALL_LANES, false, &mut m);
        assert_eq!(r[0] & 0xff, 128);
        let r = lookup(&coords(-3.0, 0.5), &c, ALL_LANES, false, &mut m);
        assert_eq!(r[0], 0);
        let r = lookup(&coords(7.0, 0.5), &c, ALL_LANES, false, &mut m);
        assert_eq!(r[0], 0xffff_ffff);
    }

    #[test]
    fn formats_expand_to_rgba8() {
        let mut ram = vec![0u8; 1 << 16];
        ram[0x1000..0x1002].copy_from_slice(&0xf81fu16.to_le_bytes()); // 565: red 31, green 0, blue 31
        let mut m = Mem::new(&mut ram);
        let c = cfg(0x1000, RGB565, 1, 1, 0, 1, true, WRAP_CLAMP);
        assert_eq!(lookup(&coords(0.5, 0.5), &c, 1, false, &mut m)[0], 0xffff_00ff);
        let c = cfg(0x1000, RGBA4444, 1, 1, 0, 1, true, WRAP_CLAMP);
        assert_eq!(lookup(&coords(0.5, 0.5), &c, 1, false, &mut m)[0].to_le_bytes(), [0xff, 0x88, 0x11, 0xff]);
        let c = cfg(0x1000, ALPHA, 1, 1, 0, 1, true, WRAP_CLAMP);
        assert_eq!(lookup(&coords(0.5, 0.5), &c, 1, false, &mut m)[0], 0x1f00_0000);
    }

    #[test]
    fn mip_levels_sit_below_the_base() {
        let t = Tex::new(&cfg(0x10_0000, RGBA8888, 64, 64, 2, 2, false, 0));
        let b = t.level_bases();
        assert_eq!(b[1].0, 0x10_0000 - 32 * 32 * 4);
        assert_eq!(b[2].0, b[1].0 - 16 * 16 * 4);
        assert_eq!(b[1].1.tiling, tiling::T);
        assert_eq!(b[2].1.tiling, tiling::LT);
    }

    #[test]
    fn etc1_block() {
        // individual mode, both sub-blocks base colour 8 (0x88), table 0, all indices 0 (+2)
        let b = [0x88, 0x88, 0x88, 0x00, 0, 0, 0, 0];
        assert_eq!(etc1_pixel(&b, 0, 0), [0x8a, 0x8a, 0x8a, 255]);
    }

    #[test]
    fn cube_faces() {
        assert_eq!(cube_face(1.0, 0.0, 0.0).0, 0);
        assert_eq!(cube_face(0.0, -1.0, 0.1).0, 3);
        assert_eq!(cube_face(0.0, 0.2, -1.0).0, 5);
    }
}
