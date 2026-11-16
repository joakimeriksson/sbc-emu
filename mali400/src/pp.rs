//! A pixel processor frame: walk the PP stream's tiles, rasterise the PLBU's primitives
//! into a 16x16 tile buffer (fragment shader, depth/stencil, blending) and write the tile
//! out through the write-back units.

use crate::fs::{self, Inputs, Program, Quad};
use crate::half;
use crate::plbu::*;
use crate::vm::Vm;
use std::collections::HashMap;

/// Frame register indices.
mod frame {
    pub const PLBU_ARRAY: usize = 0;
    pub const FLAGS: usize = 3;
    pub const CLEAR_DEPTH: usize = 4;
    pub const CLEAR_STENCIL: usize = 5;
    pub const CLEAR_COLOR: usize = 6;
    pub const WIDTH: usize = 10;
    pub const HEIGHT: usize = 11;
}

#[derive(Clone, Copy, Default, Debug)]
#[allow(dead_code)]
struct Rsw {
    blend_color: [f32; 4],
    alpha_blend: u32,
    depth_test: u32,
    depth_range: (f32, f32),
    stencil: [u32; 2],
    stencil_test: u32,
    multi_sample: u32,
    shader: u32,
    varying_types: u32,
    uniforms: u32,
    textures: u32,
    aux0: u32,
    aux1: u32,
    varyings: u32,
}

impl Rsw {
    fn read(vm: &mut Vm, va: u32) -> Rsw {
        let w: Vec<u32> = (0..16).map(|k| vm.read_u32(va + k * 4)).collect();
        let bg = w[0];
        let ra = w[1];
        Rsw {
            blend_color: [(ra & 0xff) as f32 / 255.0, (bg >> 16 & 0xff) as f32 / 255.0, (bg & 0xff) as f32 / 255.0, (ra >> 16 & 0xff) as f32 / 255.0],
            alpha_blend: w[2],
            depth_test: w[3],
            depth_range: ((w[4] & 0xffff) as f32 / 65535.0, (w[4] >> 16) as f32 / 65535.0),
            stencil: [w[5], w[6]],
            stencil_test: w[7],
            multi_sample: w[8],
            shader: w[9],
            varying_types: w[10],
            uniforms: w[11],
            textures: w[12],
            aux0: w[13],
            aux1: w[14],
            varyings: w[15],
        }
    }

    fn varying_layout(&self) -> Vec<(u32, bool)> {
        varying_layout(self.aux0, self.varying_types, self.varyings)
    }
}

/// (offset in bytes, fp16?) of each varying vec4 of a render state (aux0, varying types,
/// varyings address words), laid out as Mesa's compiler does.
pub fn varying_layout(aux0: u32, varying_types: u32, varyings: u32) -> Vec<(u32, bool)> {
    let stride = (aux0 & 0x1f) * 8;
    let mut out = Vec::new();
    let mut offset = 0u32;
    let mut k = 0;
    while offset < stride && k < 12 {
        let t = if k < 10 {
            varying_types >> (3 * k) & 7
        } else if k == 10 {
            (varying_types >> 30) | (varyings & 1) << 2
        } else {
            (varyings >> 1) & 7
        };
        let fp16 = t >= 2;
        if !fp16 {
            offset = (offset + 15) & !15;
        }
        if offset >= stride {
            break;
        }
        out.push((offset, fp16));
        offset += if fp16 { 8 } else { 16 };
        k += 1;
    }
    out
}

/// A vertex's varyings (four components each) from memory at `base`.
pub fn read_varyings(vm: &mut Vm, layout: &[(u32, bool)], base: u32) -> Vec<f32> {
    let mut v = Vec::with_capacity(layout.len() * 4);
    for &(off, fp16) in layout {
        for c in 0..4 {
            v.push(if fp16 { half::to_f32(vm.read_u16(base + off + c * 2)) } else { vm.read_f32(base + off + c * 4) });
        }
    }
    v
}

/// A primitive with its vertices fetched.
struct Prim {
    kind: u32,
    state: usize,
    /// screen x, y, z, 1/w
    pos: [[f32; 4]; 4],
    /// varying components per vertex
    vary: [Vec<f32>; 4],
    point_size: f32,
    bbox: [i32; 4],
}

struct DrawState {
    rsw: Rsw,
    program: std::rc::Rc<Program>,
    scissor: [i32; 4],
    uniforms: Vec<[f32; 4]>,
    textures: Vec<Texture>,
}

/// A texture descriptor.
#[derive(Clone, Debug, Default)]
#[allow(dead_code)]
struct Texture {
    format: u32,
    swap_rb: bool,
    stride: u32,
    unnorm: bool,
    dim: u32,
    min_lod: f32,
    max_lod: f32,
    lod_bias: f32,
    mip_linear: bool,
    min_nearest: bool,
    mag_nearest: bool,
    wrap: [u32; 3],
    width: u32,
    height: u32,
    layout: u32,
    levels: Vec<u32>,
    border: [f32; 4],
    cube: bool,
}

fn bits(words: &[u32], at: u32, n: u32) -> u32 {
    let mut v = 0u64;
    for i in 0..n {
        let b = at + i;
        v |= ((words[(b / 32) as usize] >> (b % 32)) as u64 & 1) << i;
    }
    v as u32
}

impl Texture {
    fn read(vm: &mut Vm, va: u32) -> Texture {
        let w: Vec<u32> = (0..16).map(|k| vm.read_u32(va + k * 4)).collect();
        let fixed = |v: u32| -> f32 { v as f32 / 16.0 };
        let bias = bits(&w, 60, 9);
        let lod_bias = if bias > 0xff { -((0x200 - bias) as f32 / 16.0) } else { bias as f32 / 16.0 };
        let mut t = Texture {
            format: bits(&w, 0, 6),
            swap_rb: bits(&w, 7, 1) != 0,
            stride: bits(&w, 16, 15),
            unnorm: bits(&w, 39, 1) != 0,
            dim: bits(&w, 42, 2),
            min_lod: fixed(bits(&w, 44, 8)),
            max_lod: fixed(bits(&w, 52, 8)),
            lod_bias,
            mip_linear: bits(&w, 73, 2) == 3,
            min_nearest: bits(&w, 75, 1) != 0,
            mag_nearest: bits(&w, 76, 1) != 0,
            wrap: [bits(&w, 77, 3), bits(&w, 80, 3), bits(&w, 83, 3)],
            width: bits(&w, 86, 13),
            height: bits(&w, 99, 13),
            layout: bits(&w, 205, 2),
            border: [0.0; 4],
            levels: Vec::new(),
            cube: bits(&w, 41, 1) != 0,
        };
        for c in 0..4 {
            // unorm16 (Mesa packs it with float_to_ushort)
            t.border[c] = bits(&w, 125 + 16 * c as u32, 16) as f32 / 65535.0;
        }
        let levels = (t.max_lod.ceil() as u32 + 1).clamp(1, 12);
        for l in 0..levels {
            let at = 222 + 26 * l;
            if at + 26 > 16 * 32 {
                break;
            }
            t.levels.push(bits(&w, at, 26) << 6);
        }
        t
    }

    fn level_size(&self, level: u32) -> (u32, u32) {
        ((self.width >> level).max(1), (self.height >> level).max(1))
    }

    fn bpp(&self) -> u32 {
        match self.format {
            0x09..=0x0b => 1,
            0x0e..=0x14 => 2,
            0x15 => 3,
            0x16 | 0x17 | 0x2c | 0x32 => 4,
            0x22..=0x24 => 2,
            0x25 => 4,
            0x26 => 8,
            0x2f => 6,
            _ => 4,
        }
    }

    /// Bytes between the faces / layers of a level (Mesa aligns each to 16x16).
    fn layer_stride(&self, level: u32) -> u32 {
        let (w, h) = self.level_size(level);
        let (aw, ah) = (w.div_ceil(16) * 16, h.div_ceil(16) * 16);
        if self.format == 0x20 {
            aw * ah / 2
        } else {
            aw * ah * self.bpp()
        }
    }

    fn texel(&self, vm: &mut Vm, level: u32, face: u32, x: u32, y: u32) -> [f32; 4] {
        let base = self.levels[level as usize] + face * self.layer_stride(level);
        let (w, _) = self.level_size(level);
        if self.format == 0x20 {
            return self.etc1_texel(vm, base, w, x, y);
        }
        let bpp = self.bpp();
        let addr = if self.layout == 3 {
            let tiles_per_row = w.div_ceil(16);
            let tile = (y / 16) * tiles_per_row + x / 16;
            base + (tile * 256 + utile_index(x & 15, y & 15)) * bpp
        } else {
            let stride = if self.stride != 0 { self.stride } else { w * bpp };
            base + y * stride + x * bpp
        };
        let n8 = |v: u32| v as f32 / 255.0;
        let mut c = match self.format {
            0x09 => {
                let l = n8(vm.read_u8(addr) as u32);
                [l, l, l, 1.0]
            }
            0x0a => [0.0, 0.0, 0.0, n8(vm.read_u8(addr) as u32)],
            0x0b => [n8(vm.read_u8(addr) as u32); 4],
            0x0e => {
                let v = vm.read_u16(addr) as u32;
                [(v >> 11) as f32 / 31.0, (v >> 5 & 0x3f) as f32 / 63.0, (v & 0x1f) as f32 / 31.0, 1.0]
            }
            0x0f => {
                let v = vm.read_u16(addr) as u32;
                [(v >> 10 & 0x1f) as f32 / 31.0, (v >> 5 & 0x1f) as f32 / 31.0, (v & 0x1f) as f32 / 31.0, (v >> 15) as f32]
            }
            0x10 => {
                let v = vm.read_u16(addr) as u32;
                [(v >> 8 & 0xf) as f32 / 15.0, (v >> 4 & 0xf) as f32 / 15.0, (v & 0xf) as f32 / 15.0, (v >> 12) as f32 / 15.0]
            }
            0x11 => {
                let v = vm.read_u16(addr) as u32;
                let l = n8(v & 0xff);
                [l, l, l, n8(v >> 8)]
            }
            0x12 => {
                let l = vm.read_u16(addr) as f32 / 65535.0;
                [l, l, l, 1.0]
            }
            0x13 => [0.0, 0.0, 0.0, vm.read_u16(addr) as f32 / 65535.0],
            0x14 => [vm.read_u16(addr) as f32 / 65535.0; 4],
            0x15 => {
                let (b, g, r) = (vm.read_u8(addr), vm.read_u8(addr + 1), vm.read_u8(addr + 2));
                [n8(r as u32), n8(g as u32), n8(b as u32), 1.0]
            }
            0x16 | 0x17 => {
                let v = vm.read_u32(addr);
                let a = if self.format == 0x17 { 1.0 } else { n8(v >> 24) };
                [n8(v >> 16 & 0xff), n8(v >> 8 & 0xff), n8(v & 0xff), a]
            }
            0x2c | 0x32 => {
                let v = vm.read_u32(addr);
                let d = (v & 0xff_ffff) as f32 / 16777215.0;
                [d, (v >> 24) as f32, 0.0, 1.0]
            }
            0x22 => {
                let l = half::to_f32(vm.read_u16(addr));
                [l, l, l, 1.0]
            }
            0x23 => [0.0, 0.0, 0.0, half::to_f32(vm.read_u16(addr))],
            0x24 => [half::to_f32(vm.read_u16(addr)); 4],
            0x25 => {
                let l = half::to_f32(vm.read_u16(addr));
                [l, l, l, half::to_f32(vm.read_u16(addr + 2))]
            }
            0x26 => {
                let mut h = |k: u32| half::to_f32(vm.read_u16(addr + k * 2));
                [h(2), h(1), h(0), h(3)]
            }
            0x2f => {
                let mut h = |k: u32| half::to_f32(vm.read_u16(addr + k * 2));
                [h(2), h(1), h(0), 1.0]
            }
            _ => [1.0, 0.0, 1.0, 1.0],
        };
        if self.swap_rb {
            c.swap(0, 2);
        }
        c
    }

    fn wrap_coord(mode: u32, i: i32, size: u32) -> Option<u32> {
        let n = size as i32;
        Some(match mode {
            0 => i.rem_euclid(n),
            4 => {
                let m = i.rem_euclid(2 * n);
                if m >= n {
                    2 * n - 1 - m
                } else {
                    m
                }
            }
            3 | 7 if i < 0 || i >= n => return None,
            5 | 6 => {
                let m = if i < 0 { -i - 1 } else { i };
                m.min(n - 1)
            }
            _ => i.clamp(0, n - 1),
        } as u32)
    }

    /// An ETC1 texel: 4x4 blocks of 8 bytes, tiled 16x16 blocks at a time.
    fn etc1_texel(&self, vm: &mut Vm, base: u32, w: u32, x: u32, y: u32) -> [f32; 4] {
        let (bx, by) = (x / 4, y / 4);
        let blocks_w = w.div_ceil(4);
        let addr = if self.layout == 3 {
            let tile = (by / 16) * blocks_w.div_ceil(16) + bx / 16;
            base + (tile * 256 + utile_index(bx & 15, by & 15)) * 8
        } else {
            let stride = if self.stride != 0 { self.stride } else { blocks_w * 8 };
            base + by * stride + bx * 8
        };
        let b: Vec<u32> = (0..8).map(|k| vm.read_u8(addr + k) as u32).collect();
        let c = etc1_decode(&b, x & 3, y & 3);
        [c[0], c[1], c[2], 1.0]
    }

    fn fetch(&self, vm: &mut Vm, level: u32, face: u32, x: i32, y: i32) -> [f32; 4] {
        let (w, h) = self.level_size(level);
        match (Self::wrap_coord(self.wrap[0], x, w), Self::wrap_coord(self.wrap[1], y, h)) {
            (Some(x), Some(y)) => self.texel(vm, level, face, x, y),
            _ => self.border,
        }
    }

    fn sample_level(&self, vm: &mut Vm, level: u32, face: u32, s: f32, t: f32, nearest: bool) -> [f32; 4] {
        let (w, h) = self.level_size(level);
        let (u, v) = if self.unnorm { (s, t) } else { (s * w as f32, t * h as f32) };
        if nearest {
            return self.fetch(vm, level, face, u.floor() as i32, v.floor() as i32);
        }
        let (u, v) = (u - 0.5, v - 0.5);
        let (x0, y0) = (u.floor(), v.floor());
        let (fx, fy) = (u - x0, v - y0);
        let (x0, y0) = (x0 as i32, y0 as i32);
        let a = self.fetch(vm, level, face, x0, y0);
        let b = self.fetch(vm, level, face, x0 + 1, y0);
        let c = self.fetch(vm, level, face, x0, y0 + 1);
        let d = self.fetch(vm, level, face, x0 + 1, y0 + 1);
        [0, 1, 2, 3].map(|k| (a[k] * (1.0 - fx) + b[k] * fx) * (1.0 - fy) + (c[k] * (1.0 - fx) + d[k] * fx) * fy)
    }

    fn sample(&self, vm: &mut Vm, face: u32, s: f32, t: f32, lod: f32) -> [f32; 4] {
        if self.levels.is_empty() || self.width == 0 {
            return [0.0, 0.0, 0.0, 1.0];
        }
        // magnification or minification is decided before the LOD is clamped (without
        // mipmaps min_lod = max_lod = 0)
        let raw = lod + self.lod_bias;
        let lod = raw.clamp(self.min_lod, self.max_lod.max(self.min_lod));
        let max_level = self.levels.len() as u32 - 1;
        if raw <= 0.0 {
            return self.sample_level(vm, 0, face, s, t, self.mag_nearest);
        }
        if lod <= 0.0 {
            return self.sample_level(vm, 0, face, s, t, self.min_nearest);
        }
        if !self.mip_linear || max_level == 0 {
            let level = (lod.round() as u32).min(max_level);
            let level = if max_level == 0 { 0 } else { level };
            return self.sample_level(vm, level, face, s, t, self.min_nearest);
        }
        let l0 = (lod.floor() as u32).min(max_level);
        let l1 = (l0 + 1).min(max_level);
        let f = lod - lod.floor();
        let a = self.sample_level(vm, l0, face, s, t, self.min_nearest);
        let b = self.sample_level(vm, l1, face, s, t, self.min_nearest);
        [0, 1, 2, 3].map(|k| a[k] * (1.0 - f) + b[k] * f)
    }
}

/// Decode texel (x, y) of an ETC1 block (8 bytes) to RGB.
fn etc1_decode(b: &[u32], x: u32, y: u32) -> [f32; 3] {
    const MODIFIERS: [[i32; 2]; 8] = [[2, 8], [5, 17], [9, 29], [13, 42], [18, 60], [24, 80], [33, 106], [47, 183]];
    let diff = b[3] & 2 != 0;
    let flip = b[3] & 1 != 0;
    let sub = if flip { (y >= 2) as usize } else { (x >= 2) as usize };
    let table = if sub == 0 { b[3] >> 5 } else { (b[3] >> 2) & 7 } as usize;
    let base = |k: usize| -> i32 {
        let v = b[k];
        if diff {
            if sub == 0 {
                ((v & 0xf8) | (v >> 5)) as i32
            } else {
                // the second colour in 8-bit arithmetic, as Mesa decodes it (an overflow
                // is undefined in ETC1)
                let d = ((v & 7) as i32) << 29 >> 29;
                let c = ((v >> 3) as i32 + d) as u8;
                (c << 3 | c >> 2) as i32
            }
        } else {
            let c = if sub == 0 { v >> 4 } else { v & 15 } as i32;
            c << 4 | c
        }
    };
    let lo = b[4] << 24 | b[5] << 16 | b[6] << 8 | b[7];
    let p = x * 4 + y;
    let index = ((lo >> (16 + p)) & 1) << 1 | ((lo >> p) & 1);
    let m = MODIFIERS[table];
    let delta = match index {
        0 => m[0],
        1 => m[1],
        2 => -m[0],
        _ => -m[1],
    };
    [0, 1, 2].map(|k| (base(k) + delta).clamp(0, 255) as f32 / 255.0)
}

/// Cube map face (+x, -x, +y, -y, +z, -z) and (s, t) for a direction.
fn cube_face(d: [f32; 4]) -> (u32, f32, f32) {
    let (x, y, z) = (d[0], d[1], d[2]);
    let (ax, ay, az) = (x.abs(), y.abs(), z.abs());
    let (face, sc, tc, ma) = if ax >= ay && ax >= az {
        if x >= 0.0 {
            (0, -z, -y, ax)
        } else {
            (1, z, -y, ax)
        }
    } else if ay >= az {
        if y >= 0.0 {
            (2, x, z, ay)
        } else {
            (3, x, -z, ay)
        }
    } else if z >= 0.0 {
        (4, x, -y, az)
    } else {
        (5, -x, -y, az)
    };
    let ma = if ma == 0.0 { 1.0 } else { ma };
    (face, (sc / ma + 1.0) / 2.0, (tc / ma + 1.0) / 2.0)
}

/// Position of (x, y) inside a 16x16 tile in the u-interleaved layout.
pub fn utile_index(x: u32, y: u32) -> u32 {
    let mut r = 0;
    for i in 0..4 {
        let xb = x >> i & 1;
        let yb = y >> i & 1;
        r |= (xb ^ yb) << (2 * i) | yb << (2 * i + 1);
    }
    r
}

/// One fragment quad's view of a primitive, for the shader.
struct QuadInputs<'a, 'v, 'm> {
    vm: std::cell::RefCell<&'a mut Vm<'v>>,
    state: &'a DrawState,
    prim: &'a Prim,
    /// per lane: perspective-correct barycentrics, screen-space barycentrics, pixel
    bary: [[f32; 3]; 4],
    lin: [[f32; 3]; 4],
    px: [(i32, i32); 4],
    front: bool,
    /// render target height (gl_FragCoord.y is counted from the bottom)
    height: u32,
    color: [[f32; 4]; 4],
    depth: [f32; 4],
    _m: std::marker::PhantomData<&'m ()>,
}

impl QuadInputs<'_, '_, '_> {
    fn interp(&self, lane: usize, f: impl Fn(usize) -> f32) -> f32 {
        let b = self.bary[lane];
        b[0] * f(0) + b[1] * f(1) + b[2] * f(2)
    }
}

impl Inputs for QuadInputs<'_, '_, '_> {
    fn varying(&self, lane: usize, index: u32) -> f32 {
        let i = index as usize;
        let p = self.prim;
        if p.vary[0].len() <= i {
            return 0.0;
        }
        if p.kind == KIND_POINT {
            return p.vary[0][i];
        }
        self.interp(lane, |v| p.vary[v][i])
    }

    fn frag_coord(&self, lane: usize) -> [f32; 4] {
        let (x, y) = self.px[lane];
        let l = self.lin[lane];
        let p = &self.prim.pos;
        let z = l[0] * p[0][2] + l[1] * p[1][2] + l[2] * p[2][2];
        let w = l[0] * p[0][3] + l[1] * p[1][3] + l[2] * p[2][3];
        // the hardware counts y from the bottom edge (Mesa flips it for the API)
        [x as f32 + 0.5, self.height as f32 - (y as f32 + 0.5), z, w]
    }

    fn front_facing(&self) -> bool {
        self.front
    }

    fn point_coord(&self, lane: usize) -> [f32; 2] {
        let (x, y) = self.px[lane];
        let p = &self.prim.pos[0];
        let s = self.prim.point_size.max(1.0);
        [((x as f32 + 0.5 - p[0]) / s + 0.5).clamp(0.0, 1.0), ((y as f32 + 0.5 - p[1]) / s + 0.5).clamp(0.0, 1.0)]
    }

    fn uniform(&self, index: i32) -> [f32; 4] {
        self.state.uniforms.get(index.max(0) as usize).copied().unwrap_or([0.0; 4])
    }

    fn sample(&self, sampler: u32, coords: &[[f32; 4]; 4], live: u8, bias: f32, explicit: Option<[f32; 4]>, cube: bool) -> [[f32; 4]; 4] {
        let Some(tex) = self.state.textures.get(sampler as usize) else { return [[0.0, 0.0, 0.0, 1.0]; 4] };
        // cube maps: pick the face and project the direction onto it
        let mut faces = [0u32; 4];
        let mut projected = *coords;
        if cube || tex.cube {
            for l in 0..4 {
                let (f, s, t) = cube_face(coords[l]);
                faces[l] = f;
                projected[l] = [s, t, 0.0, 1.0];
            }
        }
        let coords = &projected;
        let (w, h) = (tex.width.max(1) as f32, tex.height.max(1) as f32);
        // LOD from the quad's coordinate derivatives
        let scale = if tex.unnorm { (1.0, 1.0) } else { (w, h) };
        let dx = ((coords[1][0] - coords[0][0]) * scale.0, (coords[1][1] - coords[0][1]) * scale.1);
        let dy = ((coords[2][0] - coords[0][0]) * scale.0, (coords[2][1] - coords[0][1]) * scale.1);
        let rho = (dx.0 * dx.0 + dx.1 * dx.1).max(dy.0 * dy.0 + dy.1 * dy.1).sqrt();
        let lod = if live == 0xf && rho > 0.0 { rho.log2() } else { 0.0 };
        let mut vm = self.vm.borrow_mut();
        let mut out = [[0.0f32; 4]; 4];
        for l in 0..4 {
            if live >> l & 1 == 0 {
                continue;
            }
            let lod = explicit.map_or(lod + bias, |e| e[l]);
            out[l] = tex.sample(&mut vm, faces[l], coords[l][0], coords[l][1], lod);
        }
        out
    }

    fn fb_color(&self, lane: usize) -> [f32; 4] {
        self.color[lane]
    }

    fn fb_depth(&self, lane: usize) -> f32 {
        self.depth[lane]
    }
}

/// The 16x16 tile buffer.
struct Tile {
    height: u32,
    /// an fp16 render target (no clamping or 8-bit quantisation)
    fp16: bool,
    color: [[f32; 4]; 256],
    depth: [u32; 256],
    stencil: [u8; 256],
}

thread_local! {
    /// GPU clock cycles the current frame has cost (a rough model: one shader
    /// instruction per fragment per cycle, one cycle per pixel written back).
    static CYCLES: std::cell::Cell<u64> = const { std::cell::Cell::new(0) };
}

fn spend(cycles: u64) {
    CYCLES.with(|c| c.set(c.get() + cycles));
}

/// Charge geometry work to the same counter.
pub fn spend_gp(cycles: u64) {
    spend(cycles);
}

/// Cycles spent since the last call.
pub fn take_cycles() -> u64 {
    CYCLES.with(|c| c.replace(0))
}

/// Render a frame for one PP from its registers. Returns the tiles rendered.
pub fn render(vm: &mut Vm, regs: &[u32; 23], wb: &[[u32; 12]; 3], programs: &mut HashMap<u32, std::rc::Rc<Program>>, log: &mut dyn FnMut(String)) -> u32 {
    let width = regs[frame::WIDTH] + 1;
    let height = regs[frame::HEIGHT] + 1;
    let fp16 = regs[frame::FLAGS] & 1 != 0;
    let clear_color = if fp16 {
        let lo = regs[frame::CLEAR_COLOR];
        let hi = regs[frame::CLEAR_COLOR + 1];
        [lo & 0xffff, lo >> 16, hi & 0xffff, hi >> 16].map(|v| v as f32 / 65535.0)
    } else {
        let c = regs[frame::CLEAR_COLOR];
        [c & 0xff, c >> 8 & 0xff, c >> 16 & 0xff, c >> 24].map(|v| v as f32 / 255.0)
    };
    let clear_depth = regs[frame::CLEAR_DEPTH] & 0xff_ffff;
    let clear_stencil = regs[frame::CLEAR_STENCIL] as u8;

    // the PP stream: tile positions and the polygon list each one starts at
    let mut tiles = Vec::new();
    let mut pc = regs[frame::PLBU_ARRAY];
    let mut cur = (0u32, 0u32);
    let mut list = 0u32;
    for _ in 0..1 << 20 {
        let w = vm.read_u32(pc);
        pc += 4;
        match w >> 24 {
            0xb8 => cur = (w & 0xff, w >> 8 & 0xff),
            0xe0..=0xff if w >> 29 == 7 => list = (w & 0x1fff_fffc) << 3,
            0xb0 => tiles.push((cur, list)),
            0xbc => break,
            _ => {}
        }
    }

    // the polygon lists (one per GP job; usually every tile shares it)
    let mut lists: HashMap<u32, (Vec<Prim>, Vec<DrawState>)> = HashMap::new();
    let mut rendered = 0;
    for &((tx, ty), list) in &tiles {
        let (prims, states) = &*lists.entry(list).or_insert_with(|| load_list(vm, list, programs, log));
        let mut tile = Tile { height, fp16, color: [clear_color; 256], depth: [clear_depth; 256], stencil: [clear_stencil; 256] };
        let (x0, y0) = (tx as i32 * 16, ty as i32 * 16);
        for p in prims {
            if p.bbox[0] >= x0 + 16 || p.bbox[1] <= x0 || p.bbox[2] >= y0 + 16 || p.bbox[3] <= y0 {
                continue;
            }
            raster(vm, &mut tile, x0, y0, p, &states[p.state]);
        }
        for unit in wb {
            write_back(vm, unit, &tile, tx, ty, width, height);
        }
        spend(256);
        rendered += 1;
    }
    rendered
}

fn load_list(vm: &mut Vm, list: u32, programs: &mut HashMap<u32, std::rc::Rc<Program>>, log: &mut dyn FnMut(String)) -> (Vec<Prim>, Vec<DrawState>) {
    let mut prims = Vec::new();
    let mut states: Vec<DrawState> = Vec::new();
    let mut cur_state: Option<(u32, u32, u32)> = None;
    let mut point_size = 1.0f32;
    let mut at = list;
    let mut layout: Vec<(u32, bool)> = Vec::new();
    for _ in 0..1 << 22 {
        let w0 = vm.read_u32(at);
        let (w1, w2, w3) = (vm.read_u32(at + 4), vm.read_u32(at + 8), vm.read_u32(at + 12));
        at += 16;
        match w0 & TAG_MASK {
            TAG_LINK => at = w1,
            TAG_END => break,
            TAG_STATE => {
                cur_state = Some((w1, w2, w3));
                let rsw = Rsw::read(vm, w1);
                let shader_va = rsw.shader & !0x1f;
                let program = programs
                    .entry(shader_va)
                    .or_insert_with(|| {
                        // read up to the last stop bit that covers every branch target (code
                        // such as a discard block can follow the main program's stop)
                        let mut words: Vec<u32> = Vec::new();
                        let mut need = 0u32;
                        loop {
                            let at = words.len() as u32;
                            let ctrl = vm.read_u32(shader_va + at * 4);
                            let n = (ctrl & 0x1f).max(1);
                            for k in 0..n {
                                words.push(vm.read_u32(shader_va + (at + k) * 4));
                            }
                            let i = fs::Instr::decode(&words[at as usize..]);
                            if let Some(fs::Branch::Jump { target, .. }) = i.branch {
                                need = need.max((at as i32 + target).max(0) as u32);
                            }
                            if (i.stop && words.len() as u32 > need) || words.len() > 4096 {
                                break;
                            }
                        }
                        std::rc::Rc::new(Program::decode(&words))
                    })
                    .clone();
                // fp16 uniforms through a one-entry pointer array
                let mut uniforms = Vec::new();
                if rsw.uniforms & !0xf != 0 {
                    let buf = vm.read_u32(rsw.uniforms & !0xf);
                    let bits = rsw.uniforms & 0xf;
                    // Mesa stores ceil(log2(vec4 count))
                    let count = (1u32 << bits).min(4096);
                    for k in 0..count {
                        let a = buf + k * 8;
                        uniforms.push([0, 1, 2, 3].map(|c| half::to_f32(vm.read_u16(a + c * 2))));
                    }
                }
                let mut textures = Vec::new();
                if rsw.textures != 0 {
                    let n = (rsw.aux0 >> 14) & 0x1f;
                    for k in 0..n.max(1) {
                        let d = vm.read_u32(rsw.textures + k * 4);
                        textures.push(Texture::read(vm, d));
                    }
                }
                layout = rsw.varying_layout();
                states.push(DrawState { rsw, program, scissor: [0, 4096, 0, 4096], uniforms, textures });
            }
            TAG_STATE2 => {
                if let Some(s) = states.last_mut() {
                    s.scissor = [(w1 & 0xffff) as i32, (w1 >> 16) as i32, (w2 & 0xffff) as i32, (w2 >> 16) as i32];
                }
                point_size = f32::from_bits(w3);
            }
            TAG_PRIM | TAG_PRIM_ABS => {
                let Some((_, gl_pos, ptsize)) = cur_state else { continue };
                let kind = w0 & 0xff;
                let state = states.len() - 1;
                let rsw = states[state].rsw;
                let idx = [w1, w2, w3];
                let mut pos = [[0.0f32; 4]; 4];
                let mut vary: [Vec<f32>; 4] = Default::default();
                let absolute = w0 & TAG_MASK == TAG_PRIM_ABS;
                for v in 0..3 {
                    if absolute {
                        // a vertex the PLBU made by clipping: position, then f32 varyings
                        let a = idx[v];
                        pos[v] = [0, 1, 2, 3].map(|c| vm.read_f32(a + c * 4));
                        vary[v] = (0..layout.len() as u32 * 4).map(|k| vm.read_f32(a + 16 + k * 4)).collect();
                        continue;
                    }
                    let a = gl_pos + idx[v] * 16;
                    pos[v] = [0, 1, 2, 3].map(|c| vm.read_f32(a + c * 4));
                    let stride = (rsw.aux0 & 0x1f) * 8;
                    vary[v] = read_varyings(vm, &layout, (rsw.varyings & !0xf) + idx[v] * stride);
                }
                let size = if kind == KIND_POINT && ptsize != 0 { vm.read_f32(ptsize + w1 * 4) } else { point_size };
                if kind == KIND_RECT {
                    // the fourth corner: v0 + v2 - v1
                    for c in 0..4 {
                        pos[3][c] = pos[0][c] + pos[2][c] - pos[1][c];
                    }
                    vary[3] = (0..vary[0].len()).map(|k| vary[0][k] + vary[2][k] - vary[1][k]).collect();
                }
                let n = if kind == KIND_RECT { 4 } else if kind == KIND_TRIANGLE { 3 } else if kind == KIND_LINE { 2 } else { 1 };
                let half_size = if kind == KIND_TRIANGLE || kind == KIND_RECT { 0.0 } else { size.max(1.0) / 2.0 + 1.0 };
                let mut bbox = [i32::MAX, i32::MIN, i32::MAX, i32::MIN];
                for p in pos.iter().take(n) {
                    bbox[0] = bbox[0].min((p[0] - half_size).floor() as i32);
                    bbox[1] = bbox[1].max((p[0] + half_size).ceil() as i32 + 1);
                    bbox[2] = bbox[2].min((p[1] - half_size).floor() as i32);
                    bbox[3] = bbox[3].max((p[1] + half_size).ceil() as i32 + 1);
                }
                let sc = states[state].scissor;
                bbox = [bbox[0].max(sc[0]), bbox[1].min(sc[1]), bbox[2].max(sc[2]), bbox[3].min(sc[3])];
                if bbox[0] >= bbox[1] || bbox[2] >= bbox[3] {
                    continue;
                }
                if std::env::var("SBC_MALI_DUMP").is_ok() && prims.is_empty() {
                    eprintln!("\r[mali] rsw {:x?}\r", states[state].rsw);
                    eprintln!("\r[mali] layout {:?} uniforms {:?}\r", layout, states[state].uniforms);
                    for v in 0..3 {
                        eprintln!("\r[mali] v{v} pos {:?} vary {:?}\r", pos[v], vary[v]);
                    }
                    for (k, i) in states[state].program.instrs.iter().enumerate() {
                        eprintln!("\r[mali] fs{k}: {i:?}\r");
                    }
                }
                prims.push(Prim { kind, state, pos, vary, point_size: size, bbox });
            }
            _ => {
                log(format!("[mali] PP: bad polygon list record {w0:08x} at {:#x}", at - 16));
                break;
            }
        }
    }
    (prims, states)
}

/// Rasterise one primitive into the tile at (x0, y0).
fn raster(vm: &mut Vm, tile: &mut Tile, x0: i32, y0: i32, p: &Prim, st: &DrawState) {
    match p.kind {
        KIND_TRIANGLE => raster_tri(vm, tile, x0, y0, p, st, [0, 1, 2]),
        KIND_RECT => {
            raster_tri(vm, tile, x0, y0, p, st, [0, 1, 2]);
            raster_tri(vm, tile, x0, y0, p, st, [0, 2, 3]);
        }
        KIND_POINT => {
            let s = p.point_size.max(1.0) / 2.0;
            let c = p.pos[0];
            let bounds = [c[0] - s, c[0] + s, c[1] - s, c[1] + s];
            raster_rect(vm, tile, x0, y0, p, st, bounds);
        }
        _ => raster_line(vm, tile, x0, y0, p, st),
    }
}

/// A line as GL rasterises it: along the major axis one pixel column (or row) per pixel
/// centre the segment crosses (half-open at the end), `width` pixels across the minor
/// axis, attributes interpolated along the line.
fn raster_line(vm: &mut Vm, tile: &mut Tile, x0: i32, y0: i32, p: &Prim, st: &DrawState) {
    let (a, b) = (p.pos[0], p.pos[1]);
    let (dx, dy) = (b[0] - a[0], b[1] - a[1]);
    if dx == 0.0 && dy == 0.0 {
        return;
    }
    let x_major = dx.abs() >= dy.abs();
    let width = p.point_size.max(1.0).round();
    let sc = st.scissor;
    let mut cover = [[false; 16]; 16];
    let mut lin = [[0.0f32; 3]; 256];
    let mut bary = [[0.0f32; 3]; 256];
    let mut any = false;
    for y in 0..16 {
        for x in 0..16 {
            let (px, py) = (x0 + x, y0 + y);
            let (cx, cy) = (px as f32 + 0.5, py as f32 + 0.5);
            let (major, minor, a_major, d_major, a_minor, d_minor) =
                if x_major { (cx, cy, a[0], dx, a[1], dy) } else { (cy, cx, a[1], dy, a[0], dx) };
            let t = (major - a_major) / d_major;
            if !(0.0..1.0).contains(&t) {
                continue;
            }
            let centre = a_minor + t * d_minor;
            let lo = (centre - width / 2.0).round();
            if minor < lo || minor >= lo + width {
                continue;
            }
            let k = (y * 16 + x) as usize;
            let l = [1.0 - t, t, 0.0];
            lin[k] = l;
            let pw = [l[0] * a[3], l[1] * b[3]];
            let sum = pw[0] + pw[1];
            bary[k] = if sum != 0.0 { [pw[0] / sum, pw[1] / sum, 0.0] } else { l };
            if px >= sc[0] && px < sc[1] && py >= sc[2] && py < sc[3] {
                cover[y as usize][x as usize] = true;
                any = true;
            }
        }
    }
    if any {
        shade(vm, tile, x0, y0, p, st, &cover, &bary, &lin, [0, 1, 1], true);
    }
}

fn raster_rect(vm: &mut Vm, tile: &mut Tile, x0: i32, y0: i32, p: &Prim, st: &DrawState, b: [f32; 4]) {
    let mut cover = [[false; 16]; 16];
    let mut any = false;
    for y in 0..16 {
        for x in 0..16 {
            let (cx, cy) = ((x0 + x) as f32 + 0.5, (y0 + y) as f32 + 0.5);
            if cx >= b[0] && cx < b[1] && cy >= b[2] && cy < b[3] {
                cover[y as usize][x as usize] = true;
                any = true;
            }
        }
    }
    if any {
        let one = [[1.0, 0.0, 0.0]; 256];
        shade(vm, tile, x0, y0, p, st, &cover, &one, &one, [0, 0, 0], true);
    }
}

fn raster_tri(vm: &mut Vm, tile: &mut Tile, x0: i32, y0: i32, p: &Prim, st: &DrawState, vi: [usize; 3]) {
    let v = vi.map(|k| p.pos[k]);
    let area = (v[1][0] - v[0][0]) * (v[2][1] - v[0][1]) - (v[2][0] - v[0][0]) * (v[1][1] - v[0][1]);
    if area == 0.0 || !area.is_finite() {
        return;
    }
    let ccw = area < 0.0;
    let front_ccw = st.rsw.aux1 & 0x1000 != 0;
    let front = ccw == front_ccw;
    // Coverage with exact fixed-point edge functions (8 subpixel bits), so a pixel on an
    // edge two triangles share belongs to exactly one of them (top-left rule).
    const SUB: f32 = 256.0;
    let fx = |c: f32| (c * SUB).round() as i64;
    let q = v.map(|p| (fx(p[0]), fx(p[1])));
    let iarea = (q[1].0 - q[0].0) * (q[2].1 - q[0].1) - (q[2].0 - q[0].0) * (q[1].1 - q[0].1);
    if iarea == 0 {
        return;
    }
    let isign: i64 = if iarea > 0 { 1 } else { -1 };
    let iedge = |a: (i64, i64), b: (i64, i64), x: i64, y: i64| ((b.0 - a.0) * (y - a.1) - (b.1 - a.1) * (x - a.0)) * isign;
    // an edge owns the pixels on it if it is a left edge (the inside is to its right) or
    // a top edge (horizontal, inside below, y pointing down)
    let owns = |a: (i64, i64), b: (i64, i64)| {
        let (dx, dy) = ((b.0 - a.0) * isign, (b.1 - a.1) * isign);
        dy < 0 || (dy == 0 && dx > 0)
    };
    let edges = [(q[1], q[2]), (q[2], q[0]), (q[0], q[1])];
    let tl = edges.map(|(a, b)| owns(a, b));
    // barycentrics for interpolation, in floating point
    let sign = if area > 0.0 { 1.0 } else { -1.0 };
    let edge = |a: [f32; 4], b: [f32; 4], x: f32, y: f32| ((b[0] - a[0]) * (y - a[1]) - (b[1] - a[1]) * (x - a[0])) * sign;
    let sc = st.scissor;
    let mut cover = [[false; 16]; 16];
    let mut lin = [[0.0f32; 3]; 256];
    let mut bary = [[0.0f32; 3]; 256];
    let mut any = false;
    let area_abs = area.abs();
    let half = (SUB / 2.0) as i64;
    for y in 0..16 {
        for x in 0..16 {
            let (px, py) = (x0 + x, y0 + y);
            let (cx, cy) = (px as f32 + 0.5, py as f32 + 0.5);
            let e = [edge(v[1], v[2], cx, cy), edge(v[2], v[0], cx, cy), edge(v[0], v[1], cx, cy)];
            let k = (y * 16 + x) as usize;
            let l = [e[0] / area_abs, e[1] / area_abs, e[2] / area_abs];
            lin[k] = l;
            let pw = [l[0] * v[0][3], l[1] * v[1][3], l[2] * v[2][3]];
            let sum = pw[0] + pw[1] + pw[2];
            bary[k] = if sum != 0.0 { [pw[0] / sum, pw[1] / sum, pw[2] / sum] } else { l };
            let (ix, iy) = (px as i64 * SUB as i64 + half, py as i64 * SUB as i64 + half);
            let inside = (0..3).all(|i| {
                let e = iedge(edges[i].0, edges[i].1, ix, iy);
                e > 0 || (e == 0 && tl[i])
            });
            if inside && px >= sc[0] && px < sc[1] && py >= sc[2] && py < sc[3] {
                cover[y as usize][x as usize] = true;
                any = true;
            }
        }
    }
    if any {
        shade(vm, tile, x0, y0, p, st, &cover, &bary, &lin, [vi[0], vi[1], vi[2]], front);
    }
}

fn compare(func: u32, a: u32, b: u32) -> bool {
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

fn compare_f(func: u32, a: f32, b: f32) -> bool {
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

fn stencil_op(op: u32, s: u8, reference: u8) -> u8 {
    match op {
        1 => reference,
        2 => 0,
        3 => !s,
        4 => s.wrapping_add(1),
        5 => s.wrapping_sub(1),
        6 => s.saturating_add(1),
        7 => s.saturating_sub(1),
        _ => s,
    }
}

/// Run the fragment shader on the covered pixels (as 2x2 quads) and merge the results.
#[allow(clippy::too_many_arguments)]
fn shade(vm: &mut Vm, tile: &mut Tile, x0: i32, y0: i32, p: &Prim, st: &DrawState, cover: &[[bool; 16]; 16], bary: &[[f32; 3]; 256], lin: &[[f32; 3]; 256], vi: [usize; 3], front: bool) {
    let rsw = &st.rsw;
    // a primitive whose vertices are not 0, 1, 2 (rect halves, line quads): remap
    let remapped;
    let prim: &Prim = if vi == [0, 1, 2] {
        p
    } else {
        remapped = Prim {
            kind: p.kind,
            state: p.state,
            pos: [p.pos[vi[0]], p.pos[vi[1]], p.pos[vi[2]], [0.0; 4]],
            vary: [p.vary[vi[0]].clone(), p.vary[vi[1]].clone(), p.vary[vi[2]].clone(), Vec::new()],
            point_size: p.point_size,
            bbox: p.bbox,
        };
        &remapped
    };
    let depth_func = (rsw.depth_test >> 1) & 7;
    let depth_write = rsw.depth_test & 1 != 0;
    let shader_depth = rsw.depth_test & 0x800 != 0;
    // gl_FragDepth: the .x of the vec4 register in bits 6-9 (0x800 says it is written)
    let depth_reg = ((rsw.depth_test >> 6) & 0xf) as usize;
    let face = if front { 0 } else { 1 };
    let sfront = rsw.stencil[face];
    let stencil_on = !(sfront & 7 == 7 && (sfront >> 3) & 0x1ff == 0);
    let swrite = ((rsw.stencil_test >> (8 * face)) & 0xff) as u8;
    let alpha_func = rsw.multi_sample & 7;
    let alpha_ref = ((rsw.stencil_test >> 16) & 0xff) as f32 / 255.0;
    let color_reg = ((rsw.multi_sample >> 28) & 0xf) as usize;
    let colormask = rsw.alpha_blend >> 28;
    let offset_units = ((rsw.depth_test >> 24) & 0xff) as i8 as f32 / 2.0;

    for qy in (0..16).step_by(2) {
        for qx in (0..16).step_by(2) {
            let lanes = [(qx, qy), (qx + 1, qy), (qx, qy + 1), (qx + 1, qy + 1)];
            let covered: u8 = lanes.iter().enumerate().fold(0, |m, (l, &(x, y))| m | (cover[y][x] as u8) << l);
            if covered == 0 {
                continue;
            }
            let ks = lanes.map(|(x, y)| y * 16 + x);
            let mut quad = Quad::default();
            let inputs = QuadInputs {
                vm: std::cell::RefCell::new(vm),
                state: st,
                prim,
                bary: ks.map(|k| bary[k]),
                lin: ks.map(|k| lin[k]),
                px: lanes.map(|(x, y)| (x0 + x as i32, y0 + y as i32)),
                front,
                height: tile.height,
                color: ks.map(|k| tile.color[k]),
                depth: ks.map(|k| tile.depth[k] as f32 / 16777215.0),
                _m: std::marker::PhantomData,
            };
            quad.run(&st.program, &inputs, 0, 0xf);
            spend(st.program.instrs.len() as u64 * 4);
            let frag_z = [0, 1, 2, 3].map(|l| inputs.frag_coord(l)[2]);
            for l in 0..4 {
                if covered >> l & 1 == 0 || quad.killed >> l & 1 != 0 {
                    continue;
                }
                let k = ks[l];
                let regs = &quad.regs[l];
                let mut z = if shader_depth { regs[depth_reg][0] } else { frag_z[l] };
                z += offset_units / 16777215.0;
                if rsw.depth_test & 0x10 == 0 && z < 0.0 || rsw.depth_test & 0x20 == 0 && z > 1.0 {
                    continue;
                }
                let zi = (z.clamp(0.0, 1.0) * 16777215.0).round() as u32;
                let color = regs[color_reg];
                if !compare_f(alpha_func, color[3], alpha_ref) {
                    continue;
                }
                // stencil and depth
                let s = tile.stencil[k];
                if stencil_on {
                    let reference = ((sfront >> 16) & 0xff) as u8;
                    let vmask = ((sfront >> 24) & 0xff) as u8;
                    let pass = compare(sfront & 7, (reference & vmask) as u32, (s & vmask) as u32);
                    if !pass {
                        let ns = stencil_op((sfront >> 3) & 7, s, reference);
                        tile.stencil[k] = s & !swrite | ns & swrite;
                        continue;
                    }
                }
                let dpass = compare(depth_func, zi, tile.depth[k]);
                if stencil_on {
                    let reference = ((sfront >> 16) & 0xff) as u8;
                    let op = if dpass { (sfront >> 9) & 7 } else { (sfront >> 6) & 7 };
                    let ns = stencil_op(op, s, reference);
                    tile.stencil[k] = s & !swrite | ns & swrite;
                }
                if !dpass {
                    continue;
                }
                if depth_write {
                    tile.depth[k] = zi;
                }
                let out = blend(rsw, color, tile.color[k], tile.fp16);
                for c in 0..4 {
                    if colormask >> c & 1 != 0 {
                        // fp16 targets keep their range; 8-bit ones are quantised
                        tile.color[k][c] = if tile.fp16 { out[c] } else { (out[c].clamp(0.0, 1.0) * 255.0).round() / 255.0 };
                    }
                }
            }
        }
    }
}

fn blend_factor(f: u32, src: [f32; 4], dst: [f32; 4], constant: [f32; 4], c: usize, alpha_only: bool) -> f32 {
    let use_alpha = alpha_only || f & 0x10 != 0;
    let comp = if use_alpha { 3 } else { c };
    let v = match f & 7 {
        0 => src[comp],
        1 => dst[comp],
        2 => constant[comp],
        3 => 0.0,
        4 => {
            if c == 3 {
                1.0
            } else {
                src[3].min(1.0 - dst[3])
            }
        }
        _ => src[comp],
    };
    if f & 8 != 0 {
        1.0 - v
    } else {
        v
    }
}

fn blend(rsw: &Rsw, src: [f32; 4], dst: [f32; 4], fp16: bool) -> [f32; 4] {
    let ab = rsw.alpha_blend;
    let (rgb_func, a_func) = (ab & 7, (ab >> 3) & 7);
    let (rgb_s, rgb_d) = ((ab >> 6) & 0x1f, (ab >> 11) & 0x1f);
    let (a_s, a_d) = ((ab >> 16) & 0xf, (ab >> 20) & 0xf);
    let src = if fp16 { src } else { src.map(|v| v.clamp(0.0, 1.0)) };
    let mut out = [0.0f32; 4];
    for c in 0..4 {
        let (func, sf, df) = if c == 3 { (a_func, a_s, a_d) } else { (rgb_func, rgb_s, rgb_d) };
        let s = src[c] * blend_factor(sf, src, dst, rsw.blend_color, c, c == 3);
        let d = dst[c] * blend_factor(df, src, dst, rsw.blend_color, c, c == 3);
        out[c] = match func {
            0 => s - d,
            1 => d - s,
            4 => src[c].min(dst[c]),
            5 => src[c].max(dst[c]),
            _ => s + d,
        };
    }
    out
}

/// Write the tile through one write-back unit.
fn write_back(vm: &mut Vm, wb: &[u32; 12], tile: &Tile, tx: u32, ty: u32, width: u32, height: u32) {
    let kind = wb[0];
    if kind == 0 || wb[1] == 0 {
        return;
    }
    let (address, format, layout, pitch, flags) = (wb[1], wb[2], wb[4], wb[5], wb[6]);
    let swap = flags & 4 != 0;
    let bpp = match format {
        0x00..=0x02 | 0x0e => 2,
        0x04 => 1,
        0x05 | 0x07 => 2,
        0x06 => 8,
        0x08 => 4,
        _ => 4,
    };
    let q8 = |v: f32| (v.clamp(0.0, 1.0) * 255.0).round() as u32;
    let q = |v: f32, max: u32| (v.clamp(0.0, 1.0) * max as f32).round() as u32;
    for y in 0..16u32 {
        for x in 0..16u32 {
            let (px, py) = (tx * 16 + x, ty * 16 + y);
            let addr = if layout == 2 {
                address + ((ty * pitch + tx) * 256 + utile_index(x, y)) * bpp
            } else {
                if px >= width || py >= height {
                    continue;
                }
                address + py * pitch * 8 + px * bpp
            };
            let k = (y * 16 + x) as usize;
            let mut c = tile.color[k];
            if swap {
                c.swap(0, 2);
            }
            // pixel formats are named from the low bits: B first unless swapped
            let (r, g, b, a) = (c[0], c[1], c[2], c[3]);
            if kind == 1 {
                let (d, s) = (tile.depth[k], tile.stencil[k] as u32);
                match format {
                    0x0e => vm.write_u16(addr, (d >> 8) as u16),
                    _ => vm.write_u32(addr, d | s << 24),
                }
                continue;
            }
            match format {
                0x00 => vm.write_u16(addr, (q(b, 31) | q(g, 63) << 5 | q(r, 31) << 11) as u16),
                0x01 => vm.write_u16(addr, (q(b, 31) | q(g, 31) << 5 | q(r, 31) << 10 | q(a, 1) << 15) as u16),
                0x02 => vm.write_u16(addr, (q(b, 15) | q(g, 15) << 4 | q(r, 15) << 8 | q(a, 15) << 12) as u16),
                0x04 => vm.write_u8(addr, q8(b) as u8),
                0x05 => vm.write_u16(addr, (q8(b) | q8(g) << 8) as u16),
                0x06 => {
                    for (i, v) in [b, g, r, a].iter().enumerate() {
                        vm.write_u16(addr + i as u32 * 2, half::from_f32(*v));
                    }
                }
                0x07 => vm.write_u16(addr, half::from_f32(b)),
                0x08 => vm.write_u32(addr, half::from_f32(b) as u32 | (half::from_f32(g) as u32) << 16),
                _ => vm.write_u32(addr, q8(b) | q8(g) << 8 | q8(r) << 16 | q8(a) << 24),
            }
        }
    }
    let _ = fs::NREG;
}

#[cfg(test)]
mod tests {
    use super::utile_index;

    #[test]
    fn u_interleaved_tile_order() {
        // panfrost's u-interleaved 16x16 layout: bit 2i = x_i ^ y_i, bit 2i+1 = y_i
        assert_eq!([utile_index(0, 0), utile_index(1, 0), utile_index(0, 1), utile_index(1, 1)], [0, 1, 3, 2]);
        assert_eq!(utile_index(2, 0), 4);
        assert_eq!(utile_index(15, 15), 0xaa);
        let mut seen = [false; 256];
        for y in 0..16 {
            for x in 0..16 {
                seen[utile_index(x, y) as usize] = true;
            }
        }
        assert!(seen.iter().all(|&s| s));
    }
}
