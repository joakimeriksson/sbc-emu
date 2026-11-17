//! Texture sampling (Mesa `genxml/v5.xml` "Texture" and "Sampler").
//!
//! On v5 the draw's texture pointer is an array of 64-bit pointers to texture
//! descriptors: a 32-byte header (size, format, dimension, layout, levels, swizzle)
//! followed by one surface per (layer, level, face) in that nesting order, each a 64-bit
//! pointer with row and surface strides.

use crate::draw::Dcd;
use crate::format::{u_interleaved_offset, Class, PixelFormat};
use crate::isa::TexWord;
use crate::vm::Vm;

#[derive(Clone, Copy, Debug)]
pub struct Texture {
    pub width: u32,
    pub height: u32,
    pub depth: u32,
    pub array: u32,
    pub format: PixelFormat,
    pub dim: u32,
    pub layout: u32,
    pub surface_type: u32,
    pub levels: u32,
    pub swizzle: u32,
    pub payload: u64,
}

impl Texture {
    pub fn read(vm: &mut Vm, va: u64) -> Self {
        let w: [u32; 8] = vm.words(va);
        Self {
            width: (w[0] & 0xffff) + 1,
            height: (w[0] >> 16) + 1,
            depth: (w[1] & 0xffff) + 1,
            array: (w[1] >> 16) + 1,
            format: PixelFormat::from_bits(w[2] & 0x3f_ffff),
            dim: (w[2] >> 22) & 3,
            layout: (w[2] >> 24) & 15,
            surface_type: (w[2] >> 28) & 3,
            levels: (w[3] >> 24) + 1,
            swizzle: w[4] & 0xfff,
            payload: va + 32,
        }
    }

    fn faces(&self) -> u32 {
        if self.dim == 0 { 6 } else { 1 }
    }

    /// Surface (pointer, row stride, surface stride) for a layer, level and face.
    pub fn surface(&self, vm: &mut Vm, layer: u32, level: u32, face: u32) -> (u64, u32, u32) {
        let i = ((layer * self.levels + level) * self.faces() + face) as u64;
        match self.surface_type {
            0 => (vm.r32(self.payload + 4 * i) as u64, 0, 0),
            1 => (vm.r64(self.payload + 8 * i), 0, 0),
            2 => (vm.r32(self.payload + 8 * i) as u64, vm.r32(self.payload + 8 * i + 4), 0),
            _ => (vm.r64(self.payload + 16 * i), vm.r32(self.payload + 16 * i + 8), vm.r32(self.payload + 16 * i + 12)),
        }
    }
}

#[derive(Clone, Copy, Debug, Default)]
pub struct Sampler {
    pub mag_nearest: bool,
    pub min_nearest: bool,
    pub mip_mode: u32,
    pub normalized: bool,
    pub lod_bias: f32,
    pub min_lod: f32,
    pub max_lod: f32,
    pub wrap: [u32; 3],
    pub compare: u32,
    pub border: [u32; 4],
}

impl Sampler {
    pub fn read(vm: &mut Vm, va: u64) -> Self {
        let w: [u32; 8] = vm.words(va);
        Self {
            mag_nearest: w[0] & 1 != 0,
            min_nearest: w[0] & 2 != 0,
            mip_mode: (w[0] >> 3) & 3,
            normalized: w[0] & (1 << 5) != 0,
            lod_bias: ((w[0] >> 16) as i16) as f32 / 256.0,
            min_lod: (w[1] & 0xffff) as f32 / 256.0,
            max_lod: (w[1] >> 16) as f32 / 256.0,
            wrap: [w[2] & 15, (w[2] >> 4) & 15, (w[2] >> 8) & 15],
            compare: (w[2] >> 12) & 7,
            border: [w[4], w[5], w[6], w[7]],
        }
    }
}

/// Wrap an integer texel coordinate; None for the border colour.
fn wrap(i: i64, size: u32, mode: u32) -> Option<u32> {
    let n = size as i64;
    let v = match mode {
        8 => i.rem_euclid(n),
        9 | 10 => i.clamp(0, n - 1),
        11 => {
            if i < 0 || i >= n {
                return None;
            }
            i
        }
        12 => {
            let p = i.rem_euclid(2 * n);
            if p >= n { 2 * n - 1 - p } else { p }
        }
        _ => {
            let m = if i < 0 { -1 - i } else { i };
            m.clamp(0, n - 1)
        }
    };
    Some(v as u32)
}

struct Level {
    base: u64,
    row_stride: u32,
    surface_stride: u32,
    w: u32,
    h: u32,
    d: u32,
}

fn texel(vm: &mut Vm, t: &Texture, l: &Level, x: u32, y: u32, z: u32) -> [u32; 4] {
    let bpp = t.format.size().max(1);
    let off = if t.layout == 1 {
        u_interleaved_offset(x, y, l.row_stride, bpp)
    } else {
        let rs = if l.row_stride != 0 { l.row_stride } else { l.w * bpp };
        y as u64 * rs as u64 + x as u64 * bpp as u64
    };
    let off = off + z as u64 * l.surface_stride as u64;
    let mut b = [0u8; 32];
    vm.read_bytes(l.base + off, &mut b[..bpp as usize]);
    t.format.decode(&b)
}

fn compare(func: u32, a: f32, b: f32) -> bool {
    match func {
        0 => false,
        1 => a < b,
        2 => a == b,
        3 => a <= b,
        4 => a > b,
        5 => a != b,
        6 => a >= b,
        _ => true,
    }
}

/// Sample for each active thread of a quad. `frag` enables implicit LOD from the quad.
pub fn sample(vm: &mut Vm, dcd: &Dcd, w: &TexWord, coords: &[[f32; 4]; 4], lod: &[f32; 4], active: u8, frag: bool) -> [[u32; 4]; 4] {
    let mut out = [[0u32; 4]; 4];
    let tptr = vm.r64(dcd.textures + 8 * w.texture_handle as u64);
    let t = Texture::read(vm, tptr);
    let s = Sampler::read(vm, dcd.samplers + 32 * w.sampler_handle as u64);
    let class = t.format.class();
    let is_cube = t.dim == 0;
    // coordinates per thread: (s, t, r, layer/face)
    let mut uv = [[0f32; 3]; 4];
    let mut layer = [0u32; 4];
    let mut face = [0u32; 4];
    for k in 0..4 {
        let c = coords[k];
        if is_cube {
            let f = (c[0] / 4.0).floor().clamp(0.0, 5.0);
            face[k] = f as u32;
            uv[k] = [c[0] - 4.0 * f, c[1], 0.0];
            if t.array > 1 && c[3].is_finite() {
                layer[k] = (c[3].round().max(0.0) as u32).min(t.array - 1);
            }
        } else {
            uv[k] = [c[0], c[1], c[2]];
            if t.array > 1 {
                layer[k] = (c[3].round().max(0.0) as u32).min(t.array - 1);
            }
        }
    }
    let imm_lod = w.bias_int as f32 + w.bias as f32 / 256.0;
    for k in 0..4 {
        if active & (1 << k) == 0 {
            continue;
        }
        // level of detail
        let level_f = match w.op {
            4 => {
                if w.lod_register { lod[k] } else { w.bias_int as f32 }
            }
            2 => {
                if w.lod_register { lod[k] } else { imm_lod }
            }
            _ => {
                let mut l = 0.0;
                if frag {
                    let ox = k ^ 1;
                    let oy = k ^ 2;
                    let sz = [t.width as f32, t.height as f32, t.depth as f32];
                    let dims = if t.dim == 3 { 3 } else if t.dim == 1 { 1 } else { 2 };
                    let mut mx = 0f32;
                    let mut my = 0f32;
                    for d in 0..dims {
                        let scale = if s.normalized { sz[d] } else { 1.0 };
                        mx += ((uv[ox][d] - uv[k][d]) * scale).powi(2);
                        my += ((uv[oy][d] - uv[k][d]) * scale).powi(2);
                    }
                    let rho = mx.max(my).sqrt();
                    l = if rho > 0.0 { rho.log2() } else { -100.0 };
                }
                l + if w.lod_register { lod[k] } else { imm_lod } + s.lod_bias
            }
        };
        let max_level = (t.levels - 1) as f32;
        let level_f = if w.op == 4 { level_f } else { level_f.clamp(s.min_lod, s.max_lod.max(s.min_lod)) };
        let magnify = level_f <= 0.0;
        let nearest = if magnify { s.mag_nearest } else { s.min_nearest } || w.op == 4;
        let (l0, l1, frac) = if t.levels == 1 || magnify {
            (0, 0, 0.0)
        } else if s.mip_mode == 0 || w.op == 4 {
            let l = level_f.round().clamp(0.0, max_level) as u32;
            (l, l, 0.0)
        } else {
            let lf = level_f.clamp(0.0, max_level);
            let l0 = lf.floor() as u32;
            ((l0), (l0 + 1).min(t.levels - 1), lf - lf.floor())
        };
        let fetch = |vm: &mut Vm, level: u32| -> [f32; 4] {
            let (base, rs, ss) = t.surface(vm, layer[k], level, face[k]);
            let lv = Level {
                base,
                row_stride: rs,
                surface_stride: ss,
                w: (t.width >> level).max(1),
                h: (t.height >> level).max(1),
                d: (t.depth >> level).max(1),
            };
            let dims = [lv.w, lv.h, lv.d];
            if w.op == 4 {
                // texel fetch: integer coordinates
                let ci = |d: usize| coords[k][d].to_bits() as i32 as i64;
                let x = wrap(ci(0), lv.w, 9).unwrap_or(0);
                let y = if t.dim >= 2 { wrap(ci(1), lv.h, 9).unwrap_or(0) } else { 0 };
                let z = if t.dim == 3 { wrap(ci(2), lv.d, 9).unwrap_or(0) } else { 0 };
                return texel(vm, &t, &lv, x, y, z).map(f32::from_bits);
            }
            let ndim = match t.dim {
                1 => 1,
                3 => 3,
                _ => 2,
            };
            let mut pos = [0f32; 3];
            for d in 0..ndim {
                pos[d] = if s.normalized { uv[k][d] * dims[d] as f32 } else { uv[k][d] };
            }
            let border = |c: Class| -> [f32; 4] {
                if c == Class::Float { s.border.map(f32::from_bits) } else { s.border.map(|x| f32::from_bits(x)) }
            };
            if nearest {
                let mut idx = [0u32; 3];
                for d in 0..ndim {
                    match wrap(pos[d].floor() as i64, dims[d], s.wrap[d]) {
                        Some(v) => idx[d] = v,
                        None => return border(class),
                    }
                }
                texel(vm, &t, &lv, idx[0], idx[1], idx[2]).map(f32::from_bits)
            } else {
                let mut i0 = [0i64; 3];
                let mut fr = [0f32; 3];
                for d in 0..ndim {
                    let p = pos[d] - 0.5;
                    i0[d] = p.floor() as i64;
                    fr[d] = p - p.floor();
                }
                let corners = 1 << ndim;
                let mut acc = [0f32; 4];
                for c in 0..corners {
                    let mut wgt = 1.0;
                    let mut idx = [0u32; 3];
                    let mut is_border = false;
                    for d in 0..ndim {
                        let hi = (c >> d) & 1 == 1;
                        wgt *= if hi { fr[d] } else { 1.0 - fr[d] };
                        match wrap(i0[d] + hi as i64, dims[d], s.wrap[d]) {
                            Some(v) => idx[d] = v,
                            None => is_border = true,
                        }
                    }
                    if wgt == 0.0 {
                        continue;
                    }
                    let v = if is_border { border(class) } else { texel(vm, &t, &lv, idx[0], idx[1], idx[2]).map(f32::from_bits) };
                    for ch in 0..4 {
                        acc[ch] += wgt * v[ch];
                    }
                }
                acc
            }
        };
        let mut v = fetch(vm, l0);
        if frac > 0.0 && l1 != l0 && class == Class::Float {
            let v1 = fetch(vm, l1);
            for ch in 0..4 {
                v[ch] = v[ch] * (1.0 - frac) + v1[ch] * frac;
            }
        }
        let raw = v.map(f32::to_bits);
        let raw = if class == Class::Float { raw } else { v.map(|x| x.to_bits()) };
        let sw = crate::format::apply_swizzle(t.format.swizzle, raw, class == Class::Float);
        let mut res = crate::format::apply_swizzle(t.swizzle, sw, class == Class::Float);
        if w.mode == 5 || w.mode == 6 {
            // shadow: compare the texel's first channel with the reference in z
            let pass = compare(s.compare, f32::from_bits(res[0]), coords[k][2]);
            let r = if pass { 1.0f32 } else { 0.0 };
            res = [r.to_bits(), r.to_bits(), r.to_bits(), 1.0f32.to_bits()];
        }
        out[k] = match (w.sampler_type, class) {
            (2, Class::Float) => res.map(|x| f32::from_bits(x) as u32),
            (3, Class::Float) => res.map(|x| f32::from_bits(x) as i32 as u32),
            (1, Class::Uint) => res.map(|x| (x as f32).to_bits()),
            (1, Class::Sint) => res.map(|x| (x as i32 as f32).to_bits()),
            _ => res,
        };
    }
    out
}
