//! Texture instructions on quads: filtered samples, gathers, texel fetches, queries, and
//! surface (image) accesses.
//!
//! Lane order is `x | y << 1`: lane 1 is to the right of lane 0, lane 2 below it. Results are
//! register values: f32 bits, or integers for integer textures.

use crate::desc::{Compare, CubeEdges, Dim, MipFilter, Reduction, Wrap};
use crate::layout::{gob_offset, Mip};
use crate::num::{bits, f32s, srgb8_table, Num};
use crate::texture::{Kind, Sampler, TexCache, Texture};
use crate::Memory;

/// Invocations that are sampled together.
pub const LANES: usize = 4;

/// Where the level of detail comes from.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Lod {
    /// Level 0 (TEX.LZ).
    Zero,
    /// The differences between the quad's coordinates (TEX).
    Auto,
    /// The same, and a bias (TEX.LB).
    Bias([f32; LANES]),
    /// Given (TEX.LL).
    Level([f32; LANES]),
    /// Given derivatives of each coordinate (TXD).
    Grad { dx: [[f32; LANES]; 3], dy: [[f32; LANES]; 3] },
}

/// Operands of a texture instruction on a quad.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Args {
    /// By coordinate, then lane: s, t, r (a cube's direction, which need not be normalised).
    /// 1D textures use s, 2D ones s and t.
    pub coord: [[f32; LANES]; 3],
    /// Array layer, or cube of a cube array. Clamped to the last one.
    pub layer: [u32; LANES],
    pub lod: Lod,
    /// Texels added to each coordinate.
    pub offset: [[i8; LANES]; 3],
    /// Reference for a depth comparison (the instruction's .DC).
    pub compare: Option<[f32; LANES]>,
    /// Lanes to sample; the others give zeros. Derivatives still use all lanes.
    pub active: u8,
}

impl Default for Args {
    fn default() -> Self {
        Self {
            coord: [[0.0; LANES]; 3],
            layer: [0; LANES],
            lod: Lod::Zero,
            offset: [[0; LANES]; 3],
            compare: None,
            active: 15,
        }
    }
}

/// Face of a direction, and the coordinates on its plane: sc, tc and the major component.
/// Faces: +x, -x, +y, -y, +z, -z.
fn cube_face(d: [f32; 3]) -> u32 {
    let a = d.map(f32::abs);
    let axis = if a[0] >= a[1] && a[0] >= a[2] {
        0
    } else if a[1] >= a[2] {
        1
    } else {
        2
    };
    axis as u32 * 2 + (d[axis] < 0.0) as u32
}

fn cube_project(face: u32, [x, y, z]: [f32; 3]) -> (f32, f32, f32) {
    match face {
        0 => (-z, -y, x),
        1 => (z, -y, -x),
        2 => (x, z, y),
        3 => (x, -z, -y),
        4 => (x, -y, z),
        _ => (-x, -y, -z),
    }
}

/// The direction of a point of a face's plane.
fn cube_direction(face: u32, sc: f32, tc: f32) -> [f32; 3] {
    match face {
        0 => [1.0, -tc, -sc],
        1 => [-1.0, -tc, sc],
        2 => [sc, 1.0, tc],
        3 => [sc, -1.0, -tc],
        4 => [sc, -tc, 1.0],
        _ => [-sc, -tc, -1.0],
    }
}

/// The texel (column, row, face) next to a face's edge: one of `x`, `y` is outside 0..n.
fn cube_fold(face: u32, x: i32, y: i32, n: i32) -> (i32, i32, u32) {
    let centre = |i: i32| (2 * i + 1) as f32 / n as f32 - 1.0;
    let (u, v) = (centre(x), centre(y));
    let (over, sc, tc) = if u.abs() > 1.0 { (u.abs() - 1.0, u.signum(), v) } else { (v.abs() - 1.0, u, v.signum()) };
    // Over the edge, then inwards on the next face.
    let mut d = cube_direction(face, sc, tc);
    d[(face >> 1) as usize] *= 1.0 - over;
    let f = cube_face(d);
    let (sc, tc, ma) = cube_project(f, d);
    let texel = |c: f32| (((c / ma + 1.0) * 0.5 * n as f32) as i32).clamp(0, n - 1);
    (texel(sc), texel(tc), f)
}

/// Texel number for a wrap mode: -1 is the border.
#[inline(always)]
fn wrap_index(wrap: Wrap, i: i32, n: i32) -> i32 {
    match wrap {
        Wrap::Repeat => {
            if (i as u32) < n as u32 {
                i
            } else {
                i.rem_euclid(n)
            }
        }
        Wrap::ClampToEdge => i.clamp(0, n - 1),
        Wrap::Mirror => {
            let m = i.rem_euclid(2 * n);
            if m < n {
                m
            } else {
                2 * n - 1 - m
            }
        }
        Wrap::Border | Wrap::ClampGl => {
            if (i as u32) < n as u32 {
                i
            } else {
                -1
            }
        }
        Wrap::MirrorOnceClampToEdge => (if i < 0 { -1 - i } else { i }).min(n - 1),
        Wrap::MirrorOnceBorder | Wrap::MirrorOnceClampGl => {
            let m = if i < 0 { -1 - i } else { i };
            if m < n {
                m
            } else {
                -1
            }
        }
    }
}

/// A normalised coordinate brought near 0..1 where the wrap mode repeats, in texels.
#[inline(always)]
fn scale(wrap: Wrap, c: f32, n: i32, normalized: bool) -> f32 {
    if !normalized {
        return c;
    }
    let c = match wrap {
        Wrap::Repeat => c - c.floor(),
        Wrap::Mirror => c - 2.0 * (c * 0.5).floor(),
        _ => c,
    };
    c * n as f32
}

/// Keeps the fixed point below in range.
const FAR: f32 = 4_194_304.0;

/// The two texels a linear filter uses, and the weight of the second in 1/256.
#[inline(always)]
fn axis_linear(wrap: Wrap, c: f32, n: i32, normalized: bool, offset: i32) -> (i32, i32, u32) {
    let mut c = scale(wrap, c, n, normalized) + offset as f32;
    match wrap {
        Wrap::ClampGl => c = c.clamp(0.0, n as f32),
        Wrap::MirrorOnceClampGl => c = c.abs().min(n as f32),
        Wrap::MirrorOnceClampToEdge | Wrap::MirrorOnceBorder => c = c.abs(),
        _ => {}
    }
    let fixed = (c.clamp(-FAR, FAR) * 256.0 - 127.5).floor() as i32;
    let i = fixed >> 8;
    (wrap_index(wrap, i, n), wrap_index(wrap, i + 1, n), (fixed & 255) as u32)
}

/// The texel a nearest filter uses.
#[inline(always)]
fn axis_nearest(wrap: Wrap, c: f32, n: i32, normalized: bool, offset: i32) -> i32 {
    let mut c = scale(wrap, c, n, normalized) + offset as f32;
    let mut wrap = wrap;
    match wrap {
        Wrap::ClampGl => wrap = Wrap::ClampToEdge,
        Wrap::MirrorOnceClampGl => {
            c = c.abs();
            wrap = Wrap::ClampToEdge;
        }
        Wrap::MirrorOnceClampToEdge | Wrap::MirrorOnceBorder => c = c.abs(),
        _ => {}
    }
    wrap_index(wrap, c.clamp(-FAR, FAR).floor() as i32, n)
}

/// One lane's position: coordinates (texels if not normalised), on layer or face `layer`.
#[derive(Clone, Copy)]
struct Pos {
    c: [f32; 3],
    layer: u32,
    /// Face, for cubes.
    face: u32,
    offset: [i32; 3],
    reference: f32,
}

struct Ctx<'a, M: ?Sized> {
    tex: &'a Texture,
    smp: &'a Sampler,
    cache: &'a mut TexCache,
    mem: &'a M,
    srgb: bool,
    normalized: bool,
    compare: Option<Compare>,
}

/// Texels gathered by a filter, with weights of 1/256 or their products.
struct Sum {
    reduction: Reduction,
    v: [f32; 4],
    first: bool,
}

impl Sum {
    fn new(reduction: Reduction) -> Self {
        Self { reduction, v: [0.0; 4], first: true }
    }

    #[inline]
    fn add(&mut self, t: [f32; 4], w: f32) {
        match self.reduction {
            Reduction::Average => {
                for (v, t) in self.v.iter_mut().zip(t) {
                    *v += t * w;
                }
            }
            _ if w == 0.0 => {}
            _ if self.first => {
                self.v = t;
                self.first = false;
            }
            r => {
                for (v, t) in self.v.iter_mut().zip(t) {
                    *v = if r == Reduction::Min { v.min(t) } else { v.max(t) };
                }
            }
        }
    }
}

impl<M: Memory + ?Sized> Ctx<'_, M> {
    fn base(&self, mip: &Mip, layer: u32) -> u64 {
        self.tex.address + mip.offset + self.tex.layer_stride * layer as u64
    }

    /// A texel's result components (after a comparison, if any). Negative: the border.
    #[inline]
    fn texel(&mut self, mip: &Mip, layer: u32, p: [i32; 3], reference: f32) -> [u32; 4] {
        let tex = self.tex;
        let border = p[0] < 0 || p[1] < 0 || p[2] < 0;
        let t = if border {
            self.border()
        } else {
            let base = self.base(mip, layer);
            tex.fetch(self.cache, self.mem, &mip.plane, base, p[0] as u32, p[1] as u32, p[2] as u32, self.srgb)
        };
        if let Some(op) = self.compare {
            let d = f32::from_bits(if border { t[0] } else { t[tex.format.depth as usize] });
            let r = if tex.depth_unorm { reference.clamp(0.0, 1.0) } else { reference };
            return tex.swizzled([(op.test(r, d) as u32 as f32).to_bits(); 4]);
        }
        if border {
            t
        } else {
            tex.swizzled(t)
        }
    }

    /// The border colour, in the range of the format.
    fn border(&self) -> [u32; 4] {
        let b = self.smp.border;
        let c = &self.tex.packed.ch[self.tex.format.depth as usize];
        match c.num {
            _ if self.tex.integer || !self.tex.format.is_plain() => b,
            Num::Unorm => bits(f32s(b).map(|v| v.clamp(0.0, 1.0))),
            Num::Snorm => bits(f32s(b).map(|v| v.clamp(-1.0, 1.0))),
            _ => b,
        }
    }

    /// Coordinates in use beside the layer.
    fn axes(&self) -> usize {
        match self.tex.dim {
            Dim::D1 | Dim::D1Array | Dim::Buffer => 1,
            Dim::D3 => 3,
            _ => 2,
        }
    }

    /// Wrap mode of an axis; a cube's faces clamp unless the sampler says otherwise.
    fn wrap(&self, axis: usize) -> Wrap {
        if self.tex.dim.is_cube() && self.smp.cube_edges != CubeEdges::Wrap {
            Wrap::ClampToEdge
        } else {
            self.smp.wrap[axis]
        }
    }

    fn size(mip: &Mip) -> [i32; 3] {
        [mip.width as i32, mip.height as i32, mip.depth as i32]
    }

    fn nearest(&mut self, mip: &Mip, p: &Pos) -> [u32; 4] {
        let n = Self::size(mip);
        let mut i = [0; 3];
        for (a, v) in i.iter_mut().enumerate().take(self.axes()) {
            *v = axis_nearest(self.wrap(a), p.c[a], n[a], self.normalized, p.offset[a]);
        }
        self.texel(mip, p.layer + p.face, i, p.reference)
    }

    /// A linear filter's texels: columns, rows and slices, and the weights of the second ones.
    fn footprint(&self, mip: &Mip, p: &Pos) -> ([[i32; 2]; 3], [u32; 3]) {
        let n = Self::size(mip);
        let mut t = [[0; 2]; 3];
        let mut w = [0; 3];
        for a in 0..self.axes() {
            let (i0, i1, f) = axis_linear(self.wrap(a), p.c[a], n[a], self.normalized, p.offset[a]);
            t[a] = [i0, i1];
            w[a] = f;
        }
        (t, w)
    }

    /// The four texels of a seamless cube's footprint: (x0,y0), (x1,y0), (x0,y1), (x1,y1).
    /// A missing corner is the average of the others.
    fn cube_quad(&mut self, mip: &Mip, p: &Pos) -> ([[u32; 4]; 4], [u32; 2]) {
        let n = mip.width as i32;
        let fixed = |c: f32| (c.clamp(-2.0, 2.0) * n as f32 * 256.0 - 127.5).floor() as i32;
        let (fx, fy) = (fixed(p.c[0]), fixed(p.c[1]));
        let (x0, y0) = (fx >> 8, fy >> 8);
        let mut out = [[0; 4]; 4];
        let mut corner = None;
        for (k, o) in out.iter_mut().enumerate() {
            let (x, y) = (x0 + (k as i32 & 1), y0 + (k as i32 >> 1));
            let (ox, oy) = (!(0..n).contains(&x), !(0..n).contains(&y));
            let (x, y, face) = match (ox, oy) {
                (false, false) => (x, y, p.face),
                (true, true) => {
                    corner = Some(k);
                    continue;
                }
                _ => cube_fold(p.face, x, y, n),
            };
            *o = self.texel(mip, p.layer + face, [x, y, 0], p.reference);
        }
        if let Some(k) = corner {
            let t = out;
            let mean = |c: usize| {
                let s: f32 = (0..4).filter(|&i| i != k).map(|i| f32::from_bits(t[i][c])).sum();
                (s / 3.0).to_bits()
            };
            // An integer texture's corner takes a neighbour's value.
            out[k] = if self.tex.integer { t[k ^ 1] } else { std::array::from_fn(mean) };
        }
        (out, [(fx & 255) as u32, (fy & 255) as u32])
    }

    fn seamless(&self) -> bool {
        self.tex.dim.is_cube() && self.smp.cube_edges == CubeEdges::Seamless
    }

    fn linear(&mut self, mip: &Mip, p: &Pos) -> [f32; 4] {
        let mut sum = Sum::new(self.smp.reduction);
        if self.seamless() {
            let (t, w) = self.cube_quad(mip, p);
            for (k, t) in t.iter().enumerate() {
                let wx = if k & 1 == 0 { 256 - w[0] } else { w[0] };
                let wy = if k & 2 == 0 { 256 - w[1] } else { w[1] };
                sum.add(f32s(*t), (wx * wy) as f32 * (1.0 / 65536.0));
            }
            return sum.v;
        }
        let axes = self.axes();
        let (t, w) = self.footprint(mip, p);
        for k in 0..1usize << axes {
            let mut weight = 1.0;
            let mut i = [0; 3];
            for a in 0..axes {
                let hi = k >> a & 1;
                i[a] = t[a][hi];
                weight *= (if hi == 1 { w[a] } else { 256 - w[a] }) as f32 * (1.0 / 256.0);
            }
            if weight != 0.0 {
                let t = self.texel(mip, p.layer + p.face, i, p.reference);
                sum.add(f32s(t), weight);
            }
        }
        sum.v
    }

    /// The four texels of a 2D RGBA8 footprint that does not touch the border.
    #[inline]
    fn rgba8_linear(&mut self, mip: &Mip, p: &Pos) -> [f32; 4] {
        let (wrap, norm) = (self.smp.wrap, self.normalized);
        let (x0, x1, fx) = axis_linear(wrap[0], p.c[0], mip.width as i32, norm, p.offset[0]);
        let (y0, y1, fy) = axis_linear(wrap[1], p.c[1], mip.height as i32, norm, p.offset[1]);
        let (x0, x1, y0, y1) = (x0 as u32, x1 as u32, y0 as u32, y1 as u32);
        let base = self.base(mip, p.layer);
        let plane = &mip.plane;
        let t: [[u8; 4]; 4] = if plane.block.is_some() && x0 >> 4 == x1 >> 4 && y0 >> 3 == y1 >> 3 {
            let l = self.cache.line(self.mem, base + plane.gob(x0 * 4, y0, 0));
            let at = |x: u32, y: u32| {
                let o = gob_offset(x * 4, y) as usize & 0x1fc;
                [l[o], l[o + 1], l[o + 2], l[o + 3]]
            };
            [at(x0, y0), at(x1, y0), at(x0, y1), at(x1, y1)]
        } else {
            let mut at = |x: u32, y: u32| self.cache.word(self.mem, base + plane.offset(x * 4, y, 0)).to_le_bytes();
            [at(x0, y0), at(x1, y0), at(x0, y1), at(x1, y1)]
        };
        let w = [(256 - fx) * (256 - fy), fx * (256 - fy), (256 - fx) * fy, fx * fy];
        if self.srgb {
            let table = srgb8_table();
            let mut out = [0.0; 4];
            for (t, w) in t.iter().zip(w) {
                let w = w as f32 * (1.0 / 65536.0);
                let c = [table[t[0] as usize], table[t[1] as usize], table[t[2] as usize], t[3] as f32 * (1.0 / 255.0)];
                for (o, c) in out.iter_mut().zip(c) {
                    *o += c * w;
                }
            }
            return out;
        }
        let mut sum = [0u32; 4];
        for (t, w) in t.iter().zip(w) {
            for (s, t) in sum.iter_mut().zip(t) {
                *s += *t as u32 * w;
            }
        }
        sum.map(|s| s as f32 * (1.0 / (255.0 * 65536.0)))
    }

    /// Filtered value of one level of a non-integer texture.
    fn level(&mut self, mip: &Mip, p: &Pos, linear: bool, rgba8: bool) -> [f32; 4] {
        if !linear {
            f32s(self.nearest(mip, p))
        } else if rgba8 {
            f32s(self.tex.swizzled(bits(self.rgba8_linear(mip, p))))
        } else {
            self.linear(mip, p)
        }
    }

    /// Filtered value at a level of detail (already biased and clamped).
    fn mipmapped(&mut self, lambda: f32, p: &Pos, rgba8: bool) -> [u32; 4] {
        let tex = self.tex;
        let smp = self.smp;
        let top = tex.last - tex.first;
        let linear = if lambda > 0.0 { smp.min_linear } else { smp.mag_linear };
        let mip = |l: u32| &tex.levels[(tex.first + l) as usize];
        match smp.mip {
            MipFilter::Linear if !tex.integer && lambda > 0.0 && top > 0 => {
                let l = lambda.min(top as f32);
                let l0 = (l as u32).min(top);
                let f = ((l - l0 as f32) * 256.0) as u32;
                let a = self.level(mip(l0), p, linear, rgba8);
                if f == 0 {
                    return bits(a);
                }
                let b = self.level(mip(l0 + 1), p, linear, rgba8);
                let f = f as f32 * (1.0 / 256.0);
                bits(match smp.reduction {
                    Reduction::Average => std::array::from_fn(|c| a[c] + (b[c] - a[c]) * f),
                    Reduction::Min => std::array::from_fn(|c| a[c].min(b[c])),
                    Reduction::Max => std::array::from_fn(|c| a[c].max(b[c])),
                })
            }
            MipFilter::None => self.one_level(mip(0), p, linear, rgba8),
            _ => {
                let l = if lambda > 0.0 { ((lambda + 0.5) as u32).min(top) } else { 0 };
                self.one_level(mip(l), p, linear, rgba8)
            }
        }
    }

    #[inline]
    fn one_level(&mut self, mip: &Mip, p: &Pos, linear: bool, rgba8: bool) -> [u32; 4] {
        if self.tex.integer {
            self.nearest(mip, p)
        } else {
            bits(self.level(mip, p, linear, rgba8))
        }
    }
}

/// The position of each lane, and for cubes the quad's coordinates on lane 0's face.
fn positions(tex: &Texture, a: &Args, normalized: bool) -> [Pos; LANES] {
    std::array::from_fn(|l| {
        let mut p = Pos {
            c: [a.coord[0][l], a.coord[1][l], a.coord[2][l]],
            layer: a.layer[l].min(tex.layers.saturating_sub(1)),
            face: 0,
            offset: [a.offset[0][l] as i32, a.offset[1][l] as i32, a.offset[2][l] as i32],
            reference: a.compare.map_or(0.0, |c| c[l]),
        };
        if !tex.dim.is_array() && !tex.dim.is_cube() {
            p.layer = 0;
        }
        if tex.dim.is_cube() {
            let face = cube_face(p.c);
            let (sc, tc, ma) = cube_project(face, p.c);
            let half = |v: f32| (v / ma + 1.0) * 0.5;
            let s = if normalized { 1.0 } else { tex.levels[tex.first as usize].width as f32 };
            p.c = [half(sc) * s, half(tc) * s, 0.0];
            p.face = face;
            p.layer *= 6;
        }
        p
    })
}

/// Change of the texture coordinates, in texels of the view's first level, across and down.
fn derivatives(tex: &Texture, a: &Args, normalized: bool, lane: usize) -> ([f32; 3], [f32; 3]) {
    let mip = &tex.levels[tex.first as usize];
    let size = if normalized { [mip.width as f32, mip.height as f32, mip.depth as f32] } else { [1.0; 3] };
    let axes = match tex.dim {
        Dim::D1 | Dim::D1Array | Dim::Buffer => 1,
        Dim::D3 => 3,
        _ => 2,
    };
    let at = |l: usize| [a.coord[0][l], a.coord[1][l], a.coord[2][l]];
    let mut d = ([0.0; 3], [0.0; 3]);
    if tex.dim.is_cube() {
        // On the plane of the lane's face.
        let l0 = if let Lod::Grad { .. } = a.lod { lane } else { 0 };
        let p = at(l0);
        let face = cube_face(p);
        let plane = |p: [f32; 3]| {
            let (sc, tc, ma) = cube_project(face, p);
            [sc / ma * 0.5 * mip.width as f32, tc / ma * 0.5 * mip.width as f32, 0.0]
        };
        let (px, py) = match &a.lod {
            Lod::Grad { dx, dy } => {
                (std::array::from_fn(|i| p[i] + dx[i][lane]), std::array::from_fn(|i| p[i] + dy[i][lane]))
            }
            _ => (at(1), at(2)),
        };
        let (o, x, y) = (plane(p), plane(px), plane(py));
        for i in 0..2 {
            d.0[i] = x[i] - o[i];
            d.1[i] = y[i] - o[i];
        }
        return d;
    }
    for i in 0..axes {
        match &a.lod {
            Lod::Grad { dx, dy } => {
                d.0[i] = dx[i][lane] * size[i];
                d.1[i] = dy[i][lane] * size[i];
            }
            _ => {
                d.0[i] = (a.coord[i][1] - a.coord[i][0]) * size[i];
                d.1[i] = (a.coord[i][2] - a.coord[i][0]) * size[i];
            }
        }
    }
    d
}

fn dot(v: [f32; 3]) -> f32 {
    v[0] * v[0] + v[1] * v[1] + v[2] * v[2]
}

/// Level of detail of each lane before bias and clamping, and for anisotropic samplers the
/// number of probes and the vector they are spread along (texels of the first level).
fn lambdas(tex: &Texture, a: &Args, normalized: bool, aniso: u32) -> ([f32; LANES], [(u32, [f32; 3]); LANES]) {
    let mut probes = [(1, [0.0; 3]); LANES];
    let mut of = |lane: usize| {
        let (dx, dy) = derivatives(tex, a, normalized, lane);
        let (x2, y2) = (dot(dx), dot(dy));
        let (long, short, axis) = if x2 >= y2 { (x2, y2, dx) } else { (y2, x2, dy) };
        if aniso > 1 && long > 0.0 {
            let n = ((long / short.max(1e-12)).sqrt().ceil()).clamp(1.0, aniso as f32);
            probes[lane] = (n as u32, axis);
            // The probes cover the long axis, unless that takes too many.
            return 0.5 * (long / (n * n)).max(short).log2();
        }
        0.5 * long.log2()
    };
    let l = match a.lod {
        Lod::Zero => [0.0; LANES],
        Lod::Level(l) => l,
        Lod::Auto => [of(0); LANES],
        Lod::Bias(b) => {
            let l = of(0);
            b.map(|b| l + b)
        }
        Lod::Grad { .. } => std::array::from_fn(of),
    };
    if matches!(a.lod, Lod::Auto | Lod::Bias(_)) {
        probes = [probes[0]; LANES];
    }
    (l, probes)
}

fn aniso_of(tex: &Texture, smp: &Sampler) -> u32 {
    if smp.min_linear && !tex.integer && tex.dim != Dim::D3 {
        smp.max_aniso
    } else {
        1
    }
}

/// Add the sampler's bias and clamp.
fn finish_lambda(tex: &Texture, smp: &Sampler, l: f32) -> f32 {
    let l = l + smp.lod_bias + tex.lod_bias;
    // NaN becomes the lower limit.
    l.max(smp.min_lod).min(smp.max_lod).max(tex.min_lod_clamp)
}

/// TEX, TXD and their variants: a filtered sample for each lane.
pub fn sample<M: Memory + ?Sized>(
    tex: &Texture,
    smp: &Sampler,
    cache: &mut TexCache,
    mem: &M,
    a: &Args,
) -> [[u32; 4]; LANES] {
    let mut out = [[0; 4]; LANES];
    if tex.null || tex.is_buffer {
        return out;
    }
    let normalized = tex.normalized && !smp.force_unnormalized;
    let aniso = aniso_of(tex, smp);
    // Nothing depends on the level of detail of a single level with equal filters.
    let one_level = smp.mip == MipFilter::None || tex.first == tex.last;
    let (lambda, probes) = if one_level && smp.mag_linear == smp.min_linear && aniso == 1 {
        ([0.0; LANES], [(1, [0.0; 3]); LANES])
    } else {
        let (l, p) = lambdas(tex, a, normalized, aniso);
        (l.map(|l| finish_lambda(tex, smp, l)), p)
    };
    let pos = positions(tex, a, normalized);
    let compare = if a.compare.is_some() { smp.compare } else { None };
    let rgba8 = tex.format.kind == Kind::Rgba8
        && matches!(tex.dim, Dim::D2 | Dim::D2Array | Dim::D2NoMip)
        && smp.simple_2d
        && compare.is_none();
    let mut ctx = Ctx { tex, smp, cache, mem, srgb: tex.srgb && smp.srgb, normalized, compare };
    for (l, o) in out.iter_mut().enumerate() {
        if a.active >> l & 1 == 0 {
            continue;
        }
        let (n, axis) = probes[l];
        *o = if n > 1 && lambda[l] > 0.0 {
            let mip = &tex.levels[tex.first as usize];
            let size = if normalized { [mip.width as f32, mip.height as f32, 1.0] } else { [1.0; 3] };
            let mut sum = [0.0; 4];
            for k in 0..n {
                let at = (k as f32 + 0.5) / n as f32 - 0.5;
                let mut p = pos[l];
                for i in 0..2 {
                    p.c[i] += axis[i] / size[i] * at;
                }
                let t = f32s(ctx.mipmapped(lambda[l], &p, rgba8));
                for (s, t) in sum.iter_mut().zip(t) {
                    *s += t / n as f32;
                }
            }
            bits(sum)
        } else {
            ctx.mipmapped(lambda[l], &pos[l], rgba8)
        };
    }
    out
}

/// TLD4: the component `component` of the four texels a linear filter of the view's first
/// level would use, in the order (x0,y1), (x1,y1), (x1,y0), (x0,y0). With `offsets` (the .PTP
/// form; by lane and result component: x, y), each is the (x0,y0) of its own offset.
#[allow(clippy::too_many_arguments)]
pub fn gather<M: Memory + ?Sized>(
    tex: &Texture,
    smp: &Sampler,
    cache: &mut TexCache,
    mem: &M,
    a: &Args,
    component: u32,
    offsets: Option<&[[[i8; 2]; 4]; LANES]>,
) -> [[u32; 4]; LANES] {
    let mut out = [[0; 4]; LANES];
    if tex.null || tex.is_buffer {
        return out;
    }
    let normalized = tex.normalized && !smp.force_unnormalized;
    let pos = positions(tex, a, normalized);
    let compare = if a.compare.is_some() { smp.compare } else { None };
    let c = if compare.is_some() { 0 } else { (component & 3) as usize };
    let mut ctx = Ctx { tex, smp, cache, mem, srgb: tex.srgb && smp.srgb, normalized, compare };
    let mip = &tex.levels[tex.first as usize];
    for (l, o) in out.iter_mut().enumerate() {
        if a.active >> l & 1 == 0 {
            continue;
        }
        let p = &pos[l];
        if let Some(offsets) = offsets {
            for (o, off) in o.iter_mut().zip(offsets[l]) {
                let p = Pos { offset: [off[0] as i32, off[1] as i32, 0], ..*p };
                let (t, _) = ctx.footprint(mip, &p);
                *o = ctx.texel(mip, p.layer + p.face, [t[0][0], t[1][0], t[2][0]], p.reference)[c];
            }
        } else if ctx.seamless() {
            let (t, _) = ctx.cube_quad(mip, p);
            *o = [t[2][c], t[3][c], t[1][c], t[0][c]];
        } else {
            let (t, _) = ctx.footprint(mip, p);
            for (o, (x, y)) in o.iter_mut().zip([(0, 1), (1, 1), (1, 0), (0, 0)]) {
                *o = ctx.texel(mip, p.layer + p.face, [t[0][x], t[1][y], t[2][0]], p.reference)[c];
            }
        }
    }
    out
}

/// TMML: the level of detail (signed 8.8 fixed point, biased, not clamped) and the level that
/// would be used (unsigned 8.8, from the view's first), in 16 bits each.
pub fn query_lod(tex: &Texture, smp: &Sampler, a: &Args) -> [[u32; 4]; LANES] {
    if tex.null || tex.is_buffer {
        return [[0; 4]; LANES];
    }
    let normalized = tex.normalized && !smp.force_unnormalized;
    let (l, _) = lambdas(tex, a, normalized, aniso_of(tex, smp));
    l.map(|l| {
        let raw = l + smp.lod_bias + tex.lod_bias;
        let top = (tex.last - tex.first) as f32;
        let level = finish_lambda(tex, smp, l).clamp(0.0, top);
        let level = match smp.mip {
            MipFilter::None => 0.0,
            MipFilter::Nearest => (level + 0.5).floor().min(top),
            MipFilter::Linear => level,
        };
        let fixed = |v: f32| (v * 256.0).round() as i32 as u32 & 0xffff;
        [fixed(raw.clamp(-128.0, 127.99)), fixed(level), 0, 0]
    })
}

/// TXQ of the dimensions: width, height, depth or array layers, levels. A 1D array's layers
/// come second. Level `lod` is from the view's first; a level outside gives zero sizes.
pub fn query_dimensions(tex: &Texture, lod: u32) -> [u32; 4] {
    if tex.null {
        return [0; 4];
    }
    let levels = tex.last - tex.first + 1;
    let Some(m) = tex.level(lod) else {
        return [0, 0, 0, levels];
    };
    match tex.dim {
        Dim::D3 => [m.width, m.height, m.depth, levels],
        Dim::D1Array => [m.width, tex.layers, tex.layers, levels],
        _ => [m.width, m.height, tex.layers, levels],
    }
}

/// TXQ of the type: texture type and (third) samples.
pub fn query_type(tex: &Texture) -> [u32; 4] {
    [tex.dim as u32, 0, tex.ms.samples(), 0]
}

/// The address of a texel of level `lod`: None outside. `sample`: an access to a multisampled
/// texture by pixel and sample; without it one is an image of its samples.
fn locate(tex: &Texture, p: [i32; 3], layer: u32, lod: u32, sample: Option<u32>) -> Option<(u64, [u32; 3])> {
    let mip = if tex.is_buffer { &tex.levels[0] } else { tex.level(lod)? };
    let inside = |v: i32, n: u32| (v >= 0 && (v as u32) < n).then_some(v as u32);
    let x = inside(p[0], mip.width)?;
    let y = if tex.dim.is_1d() { 0 } else { inside(p[1], mip.height)? };
    let z = if tex.dim == Dim::D3 { inside(p[2], mip.depth)? } else { 0 };
    let layers = tex.layers * if tex.dim.is_cube() { 6 } else { 1 };
    let layer = if tex.dim.is_array() || tex.dim.is_cube() { layer } else { 0 };
    if layer >= layers {
        return None;
    }
    if tex.is_buffer {
        return Some((tex.address + x as u64 * tex.format.bytes as u64, [0; 3]));
    }
    if let Some(s) = sample {
        let (mx, my) = tex.ms.shift();
        let (sx, sy) = tex.ms.sample_offset(s);
        let base = tex.address + layer as u64 * tex.ms_layer_stride;
        return Some((base, [(x << mx) + sx, (y << my) + sy, z]));
    }
    Some((tex.address + mip.offset + layer as u64 * tex.layer_stride, [x, y, z]))
}

/// TLD: one texel by its position, without a sampler. `layer` counts faces of cubes. Outside
/// the texture the stored components are zeros.
pub fn fetch<M: Memory + ?Sized>(
    tex: &Texture,
    cache: &mut TexCache,
    mem: &M,
    p: [i32; 3],
    layer: u32,
    lod: u32,
    sample: Option<u32>,
) -> [u32; 4] {
    if tex.null {
        return [0; 4];
    }
    let Some((base, [x, y, z])) = locate(tex, p, layer, lod, sample) else {
        return tex.swizzled([0; 4]);
    };
    let plane = if sample.is_some() && !tex.is_buffer {
        &tex.ms_plane
    } else if tex.is_buffer {
        &tex.levels[0].plane
    } else {
        &tex.levels[(tex.first + lod) as usize].plane
    };
    tex.swizzled(tex.fetch(cache, mem, plane, base, x, y, z, tex.srgb))
}

/// The address of a texel of an image (the view's first level): None outside, where loads
/// give zeros and stores are dropped. For raw surface accesses and atomics.
pub fn image_address(tex: &Texture, p: [i32; 3], layer: u32) -> Option<u64> {
    if tex.null || !tex.format.is_plain() {
        return None;
    }
    let (base, [x, y, z]) = locate(tex, p, layer, 0, None)?;
    if tex.is_buffer {
        return Some(base);
    }
    Some(base + tex.levels[tex.first as usize].plane.offset(x * tex.format.bytes, y, z))
}

/// SULD.P: a formatted image load. Image accesses do not go through the texture cache.
pub fn image_load<M: Memory + ?Sized>(tex: &Texture, mem: &M, p: [i32; 3], layer: u32) -> [u32; 4] {
    let Some(addr) = image_address(tex, p, layer) else {
        return tex.swizzled([0; 4]);
    };
    let mut b = [0; 16];
    let n = tex.format.bytes.min(16) as usize;
    mem.read(addr, &mut b[..n]);
    tex.swizzled(tex.packed.decode(&crate::num::words(&b)))
}

/// SUST.P: a formatted image store.
pub fn image_store<M: Memory + ?Sized>(tex: &Texture, mem: &mut M, p: [i32; 3], layer: u32, v: [u32; 4]) {
    let Some(addr) = image_address(tex, p, layer) else {
        return;
    };
    // The stored component of each result component.
    let mut stored = [0; 4];
    for (i, &s) in tex.swizzle.iter().enumerate().rev() {
        if s < 4 {
            stored[s as usize] = v[i];
        }
    }
    let b = crate::num::bytes(&tex.packed.encode(&stored));
    mem.write(addr, &b[..tex.format.bytes.min(16) as usize]);
}
