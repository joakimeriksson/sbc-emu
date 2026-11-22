//! Clipping, the viewport transform, the rasterizer and fragment processing.
use crate::pipe::{FragmentHost, Setup, Words, CLIP, L, POSITION, WORDS};
use crate::rop::{compare, Rop, TileSet, TILE};
use crate::threed::Threed;
use crate::Memory;
use maxwell_shader::sph::Interp;
use maxwell_shader::Exit;
use std::rc::Rc;

/// Shaded vertices and the primitives made of them (`size` indices each).
pub struct Geometry {
    pub vertices: Vec<[u32; WORDS]>,
    pub primitives: Vec<u32>,
    pub size: usize,
}

impl Geometry {
    /// Keep some vertices of each primitive.
    pub fn pick(self, which: &[usize]) -> Self {
        let primitives = self.primitives.chunks_exact(self.size).flat_map(|p| which.iter().map(|&k| p[k])).collect();
        Self { vertices: self.vertices, primitives, size: which.len() }
    }
}

/// A value that is linear over the screen.
#[derive(Clone, Copy, Debug, Default)]
pub struct Linear {
    pub value: f32,
    pub dx: f32,
    pub dy: f32,
    pub x0: f32,
    pub y0: f32,
}

impl Linear {
    #[inline(always)]
    pub fn at(&self, x: f32, y: f32) -> f32 {
        self.value + (x - self.x0) * self.dx + (y - self.y0) * self.dy
    }

    fn constant(value: f32) -> Self {
        Self { value, ..Self::default() }
    }
}

#[derive(Clone, Copy, Debug)]
pub enum Plane {
    None,
    Flat(u32),
    Linear(Linear),
}

/// A primitive set up for shading.
pub struct Primitive {
    pub planes: Box<[Plane; WORDS]>,
    /// the provoking vertex's attributes
    pub flat: Box<[u32; WORDS]>,
    pub z: Linear,
    /// 1/w
    pub w: Linear,
    pub front: bool,
    pub is_point: bool,
    pub point: [Linear; 2],
    pub id: u32,
    pub layer: u32,
}

/// A vertex in window coordinates.
#[derive(Clone, Copy)]
struct Win {
    x: f32,
    y: f32,
    z: f32,
    /// 1/w
    w: f32,
}

struct View {
    scale: [f32; 3],
    offset: [f32; 3],
    zero_to_one: bool,
    clamp_near: bool,
    clamp_far: bool,
    planes: u32,
    cull: Option<u32>,
    /// clockwise (in window coordinates with y down) is the front
    front_negative: bool,
    transform: bool,
}

pub fn draw(t: &mut Threed, g: &mut Geometry, s: &Setup, words: Rc<[Words; 18]>, mem: &mut dyn Memory) {
    let v = View {
        scale: [t.f(0xa00), t.f(0xa04), t.f(0xa08)],
        offset: [t.f(0xa0c), t.f(0xa10), t.f(0xa14)],
        zero_to_one: t.r(0xd7c) & 1 != 0,
        clamp_near: t.r(0x193c) & 8 != 0,
        clamp_far: t.r(0x193c) & 0x10 != 0,
        planes: t.r(0x1510) & 0xff,
        cull: (t.r(0x1918) & 1 != 0).then(|| t.r(0x1920)),
        front_negative: t.r(0x191c) == 0x900,
        transform: t.r(0x192c) & 1 != 0,
    };
    let polygon = |back: bool| t.r(if back { 0xdb0 } else { 0xdac });
    let (mode_front, mode_back) = (polygon(false), polygon(true));
    // glPolygonOffset, for filled, line and point polygons
    let offset_on = [t.r(0xdc0) & 1 != 0, t.r(0xdc4) & 1 != 0, t.r(0xdc8) & 1 != 0];
    let (factor, units, clamp) = (t.f(0x156c), t.f(0x15bc), t.f(0x187c));
    let width = if t.r(0x1570) & 1 != 0 { t.f(0x13b0) } else { t.f(0x13b4) };
    let (point_size, attribute_size) = (t.f(0x1518), t.r(0x1910) & 1 != 0);
    let first_provokes = t.r(0x1684) & 1 == 0;
    let mut r = Raster { s, fill: Fill::default(), id: 0 };
    let size = g.size;
    let mut prims = std::mem::take(&mut g.primitives);
    let (mut common, lanes, rop, counters) = t.fragment_parts(words, mem);
    let mut ctx = Ctx { common: &mut common, lanes, rop, counters, error: None };
    for (id, p) in prims.chunks_exact_mut(size).enumerate() {
        r.id = id as u32;
        let layer = vertex_layer(&g.vertices[p[0] as usize]);
        match size {
            1 => {
                let v0 = g.vertices[p[0] as usize];
                let px = if attribute_size { f32::from_bits(v0[(0x6c - 0x40) / 4]) } else { point_size };
                if inside(&v, &v0) {
                    let w = window(&v, &v0);
                    r.point(&mut ctx, &v0, w, px.max(1.0), layer);
                }
            }
            2 => {
                let (a, b) = (g.vertices[p[0] as usize], g.vertices[p[1] as usize]);
                let provoking = if first_provokes { a } else { b };
                let clipped = clip(&v, &[a, b], s);
                if clipped.len() >= 2 {
                    let wins: Vec<Win> = clipped.iter().map(|c| window(&v, c)).collect();
                    r.line(&mut ctx, &clipped[0], &clipped[1], wins[0], wins[1], &provoking, width.max(1.0), layer);
                }
            }
            _ => {
                let tri = [g.vertices[p[0] as usize], g.vertices[p[1] as usize], g.vertices[p[2] as usize]];
                let provoking = if first_provokes { tri[0] } else { tri[2] };
                let poly = clip(&v, &tri, s);
                if poly.len() < 3 {
                    continue;
                }
                let wins: Vec<Win> = poly.iter().map(|c| window(&v, c)).collect();
                let area = area_of(&wins);
                if area == 0.0 || !area.is_finite() {
                    continue;
                }
                // y goes down in window coordinates: a positive area is clockwise
                let front = (area > 0.0) == v.front_negative;
                if let Some(c) = v.cull {
                    if c == 0x408 || c == 0x404 && front || c == 0x405 && !front {
                        continue;
                    }
                }
                let mode = if front { mode_front } else { mode_back };
                let bias = |on: bool, w: &[Win]| if on { Some(depth_bias(w, factor, units, clamp, s)) } else { None };
                match mode {
                    0x1b01 => {
                        let b = bias(offset_on[1], &wins);
                        for k in 0..poly.len() {
                            let n = (k + 1) % poly.len();
                            let (mut wa, mut wb) = (wins[k], wins[n]);
                            wa.z += b.unwrap_or(0.0);
                            wb.z += b.unwrap_or(0.0);
                            r.line(&mut ctx, &poly[k], &poly[n], wa, wb, &provoking, width.max(1.0), layer);
                        }
                    }
                    0x1b00 => {
                        let b = bias(offset_on[0], &wins);
                        for k in 0..poly.len() {
                            let mut w = wins[k];
                            w.z += b.unwrap_or(0.0);
                            r.point(&mut ctx, &poly[k], w, point_size.max(1.0), layer);
                        }
                    }
                    _ => {
                        let b = bias(offset_on[2], &wins).unwrap_or(0.0);
                        for k in 1..poly.len() - 1 {
                            r.triangle(&mut ctx, [&poly[0], &poly[k], &poly[k + 1]], [wins[0], wins[k], wins[k + 1]], &provoking, front, b, layer);
                        }
                    }
                }
            }
        }
    }
    let stored = ctx.common.stored;
    let error = ctx.error.take();
    if stored {
        t.pipe.invalidate_textures();
        t.pipe.cbufs_changed = true;
    }
    if let Some(e) = error {
        t.pipe.fail(e);
    }
}

fn vertex_layer(v: &[u32; WORDS]) -> u32 {
    v[(0x64 - 0x40) / 4]
}

fn f(v: &[u32; WORDS], k: usize) -> f32 {
    f32::from_bits(v[k])
}

fn area_of(w: &[Win]) -> f32 {
    let mut a = 0.0;
    for k in 1..w.len() - 1 {
        a += (w[k].x - w[0].x) * (w[k + 1].y - w[0].y) - (w[k + 1].x - w[0].x) * (w[k].y - w[0].y);
    }
    a
}

fn depth_bias(w: &[Win], factor: f32, units: f32, clamp: f32, s: &Setup) -> f32 {
    let (ax, ay, az) = (w[1].x - w[0].x, w[1].y - w[0].y, w[1].z - w[0].z);
    let (bx, by, bz) = (w[2].x - w[0].x, w[2].y - w[0].y, w[2].z - w[0].z);
    let d = ax * by - bx * ay;
    let (dx, dy) = if d != 0.0 { ((az * by - bz * ay) / d, (bz * ax - az * bx) / d) } else { (0.0, 0.0) };
    // (the units register holds twice the API's)
    let r = match s.targets.zeta {
        Some((_, f)) if f.is_float() => {
            let m = w.iter().map(|w| w.z.abs()).fold(0.0, f32::max);
            2f32.powi((m.to_bits() >> 23) as i32 - 127 - 23)
        }
        Some((_, f)) if f.depth_bits == 16 => 1.0 / 65536.0,
        _ => 1.0 / 16_777_216.0,
    };
    let b = dx.abs().max(dy.abs()) * factor + units * 0.5 * r;
    if clamp > 0.0 { b.min(clamp) } else if clamp < 0.0 { b.max(clamp) } else { b }
}

/// Distances to the clip planes (inside: not negative).
fn distances(v: &View, p: &[u32; WORDS]) -> ([f32; 7], [f32; 8]) {
    let (x, y, z, w) = (f(p, POSITION), f(p, POSITION + 1), f(p, POSITION + 2), f(p, POSITION + 3));
    const GUARD: f32 = 32.0;
    let near = if v.clamp_near { 1.0 } else if v.zero_to_one { z } else { z + w };
    let far = if v.clamp_far { 1.0 } else { w - z };
    let user = std::array::from_fn(|k| if v.planes >> k & 1 != 0 { f(p, CLIP + k) } else { 1.0 });
    ([w - 1e-12, near, far, GUARD * w - x, GUARD * w + x, GUARD * w - y, GUARD * w + y], user)
}

fn inside(v: &View, p: &[u32; WORDS]) -> bool {
    let (a, b) = distances(v, p);
    a[..3].iter().chain(&b).all(|&d| d >= 0.0)
}

/// Clip a polygon or a line.
fn clip(v: &View, p: &[[u32; WORDS]], s: &Setup) -> Vec<[u32; WORDS]> {
    let d: Vec<_> = p.iter().map(|p| distances(v, p)).collect();
    if d.iter().all(|(a, b)| a.iter().chain(b).all(|&d| d >= 0.0)) {
        return p.to_vec();
    }
    let mut poly = p.to_vec();
    let line = p.len() == 2;
    for plane in 0..15 {
        if poly.is_empty() {
            break;
        }
        let dist = |p: &[u32; WORDS]| { let (a, b) = distances(v, p); if plane < 7 { a[plane] } else { b[plane - 7] } };
        let ds: Vec<f32> = poly.iter().map(dist).collect();
        if ds.iter().all(|&d| d >= 0.0) {
            continue;
        }
        let mut out = Vec::with_capacity(poly.len() + 1);
        let n = poly.len();
        for k in 0..if line { 1 } else { n } {
            let j = (k + 1) % n;
            let (a, b) = (ds[k], ds[j]);
            if a >= 0.0 {
                out.push(poly[k]);
            }
            if (a >= 0.0) != (b >= 0.0) {
                out.push(mix(&poly[k], &poly[j], a / (a - b), s));
            }
            if line && b >= 0.0 {
                out.push(poly[j]);
            }
        }
        poly = out;
    }
    poly
}

fn mix(a: &[u32; WORDS], b: &[u32; WORDS], t: f32, s: &Setup) -> [u32; WORDS] {
    let mut o = *a;
    let m = |k: usize| (f(a, k) + (f(b, k) - f(a, k)) * t).to_bits();
    for k in POSITION..POSITION + 4 {
        o[k] = m(k);
    }
    for k in CLIP..CLIP + 8 {
        o[k] = m(k);
    }
    for &(k, i) in &s.inputs {
        if i != Interp::Constant {
            o[k] = m(k);
        }
    }
    // (two-sided colours)
    if s.two_sided {
        for k in (0x2a0 - 0x40) / 4..(0x2c0 - 0x40) / 4 {
            o[k] = m(k);
        }
    }
    o
}

fn window(v: &View, p: &[u32; WORDS]) -> Win {
    let (x, y, z, w) = (f(p, POSITION), f(p, POSITION + 1), f(p, POSITION + 2), f(p, POSITION + 3));
    if !v.transform {
        return Win { x, y, z, w };
    }
    let i = 1.0 / w;
    Win { x: x * i * v.scale[0] + v.offset[0], y: y * i * v.scale[1] + v.offset[1], z: z * i * v.scale[2] + v.offset[2], w: i }
}

struct Ctx<'a, 'm> {
    common: &'a mut crate::pipe::Common<'m>,
    lanes: &'a mut maxwell_shader::Lanes<L>,
    rop: &'a mut Rop,
    counters: &'a mut crate::threed::Counters,
    error: Option<String>,
}

/// Quads waiting to be shaded: they belong to one primitive.
#[derive(Default)]
struct Fill {
    n: usize,
    quads: [(i32, i32); L / 4],
    coverage: [u32; L],
}

struct Raster<'s> {
    s: &'s Setup,
    fill: Fill,
    id: u32,
}

/// A plane through three values.
fn plane(w: &[Win; 3], v: [f32; 3]) -> Linear {
    let (ax, ay) = ((w[1].x - w[0].x) as f64, (w[1].y - w[0].y) as f64);
    let (bx, by) = ((w[2].x - w[0].x) as f64, (w[2].y - w[0].y) as f64);
    let d = ax * by - bx * ay;
    let (av, bv) = (v[1] as f64 - v[0] as f64, v[2] as f64 - v[0] as f64);
    Linear { value: v[0], dx: ((av * by - bv * ay) / d) as f32, dy: ((bv * ax - av * bx) / d) as f32, x0: w[0].x, y0: w[0].y }
}

impl Raster<'_> {
    fn primitive(&self, w: [Win; 3], v: [&[u32; WORDS]; 3], provoking: &[u32; WORDS], front: bool, bias: f32, layer: u32) -> Primitive {
        let mut planes: Box<[Plane; WORDS]> = Box::new([Plane::None; WORDS]);
        for &(k, i) in &self.s.inputs {
            let flat = i == Interp::Constant || self.s.flat && k >= (0x280 - 0x40) / 4 && k < (0x2c0 - 0x40) / 4;
            let mut make = |k: usize, i: Interp| {
                planes[k] = if flat {
                    Plane::Flat(provoking[k])
                } else {
                    let scale = |n: usize| if i == Interp::Perspective { f(v[n], k) * w[n].w } else { f(v[n], k) };
                    Plane::Linear(plane(&w, [scale(0), scale(1), scale(2)]))
                };
            };
            make(k, i);
            if self.s.two_sided && k >= (0x280 - 0x40) / 4 && k < (0x2a0 - 0x40) / 4 {
                make(k + 8, i);
            }
        }
        Primitive {
            planes,
            flat: Box::new(*provoking),
            z: plane(&w, [w[0].z + bias, w[1].z + bias, w[2].z + bias]),
            w: plane(&w, [w[0].w, w[1].w, w[2].w]),
            front,
            is_point: false,
            point: Default::default(),
            id: self.id,
            layer,
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn triangle(&mut self, c: &mut Ctx, v: [&[u32; WORDS]; 3], w: [Win; 3], provoking: &[u32; WORDS], front: bool, bias: f32, layer: u32) {
        let area = area_of(&w);
        if area == 0.0 {
            return;
        }
        let prim = self.primitive(w, v, provoking, front, bias, layer);
        self.scan(c, &prim, w);
    }

    /// Rasterize a triangle.
    fn scan(&mut self, c: &mut Ctx, prim: &Primitive, w: [Win; 3]) {
        let fix = |v: f32| (v * 256.0).round() as i64;
        let mut p: [(i64, i64); 3] = std::array::from_fn(|k| (fix(w[k].x), fix(w[k].y)));
        let area = (p[1].0 - p[0].0) * (p[2].1 - p[0].1) - (p[2].0 - p[0].0) * (p[1].1 - p[0].1);
        if area == 0 {
            return;
        }
        if area < 0 {
            p.swap(1, 2);
        }
        // edge k: from vertex k to the next; inside is positive. Pixels exactly on a top or left
        // edge belong to the triangle
        let edges: [(i64, i64, i64); 3] = std::array::from_fn(|k| {
            let (a, b) = (p[k], p[(k + 1) % 3]);
            let (dx, dy) = (b.0 - a.0, b.1 - a.1);
            let top_left = dy < 0 || dy == 0 && dx > 0;
            // e(x, y) = dx (y - ay) - dy (x - ax)
            (-dy, dx, dy * a.0 - dx * a.1 - if top_left { 0 } else { 1 })
        });
        let r = self.s.rect;
        let min = |f: fn(&(i64, i64)) -> i64| p.iter().map(f).min().unwrap();
        let max = |f: fn(&(i64, i64)) -> i64| p.iter().map(f).max().unwrap();
        let x0 = (min(|p| p.0) >> 8).max(r.x0 as i64);
        let y0 = (min(|p| p.1) >> 8).max(r.y0 as i64);
        let x1 = ((max(|p| p.0) + 255) >> 8).min(r.x1 as i64);
        let y1 = ((max(|p| p.1) + 255) >> 8).min(r.y1 as i64);
        if x0 >= x1 || y0 >= y1 {
            return;
        }
        const B: i64 = 8;
        let mut by = y0 & !(B - 1);
        while by < y1 {
            let mut bx = x0 & !(B - 1);
            while bx < x1 {
                // the block's extremes of each edge
                let (mut out, mut whole) = (false, true);
                for &(a, b, e) in &edges {
                    let at = |x: i64, y: i64| a * x + b * y + e;
                    let (lx, hx) = (bx << 8, (bx + B) << 8);
                    let (ly, hy) = (by << 8, (by + B) << 8);
                    let top = at(if a > 0 { hx } else { lx }, if b > 0 { hy } else { ly });
                    let low = at(if a > 0 { lx } else { hx }, if b > 0 { ly } else { hy });
                    out |= top < 0;
                    whole &= low >= 0;
                }
                whole &= bx >= x0 && by >= y0 && bx + B <= x1 && by + B <= y1;
                if !out {
                    self.block(c, prim, &edges, (bx, by), whole, (x0, y0, x1, y1));
                }
                bx += B;
            }
            by += B;
        }
        self.flush(c, prim);
    }

    fn block(&mut self, c: &mut Ctx, prim: &Primitive, edges: &[(i64, i64, i64); 3], (bx, by): (i64, i64), whole: bool, (x0, y0, x1, y1): (i64, i64, i64, i64)) {
        let all = (1u32 << self.s.samples) - 1;
        let mask = all & self.s.sample_mask;
        for qy in (by..by + 8).step_by(2) {
            for qx in (bx..bx + 8).step_by(2) {
                let mut cov = [0u32; 4];
                if whole {
                    cov = [mask; 4];
                } else {
                    for (l, m) in cov.iter_mut().enumerate() {
                        let (x, y) = (qx + (l as i64 & 1), qy + (l as i64 >> 1));
                        if x < x0 || x >= x1 || y < y0 || y >= y1 {
                            continue;
                        }
                        for s in 0..self.s.samples as usize {
                            let (sx, sy) = self.s.positions[s];
                            let (px, py) = ((x << 8) + sx as i64, (y << 8) + sy as i64);
                            if edges.iter().all(|&(a, b, e)| a * px + b * py + e >= 0) {
                                *m |= 1 << s;
                            }
                        }
                        *m &= mask;
                    }
                }
                if cov != [0; 4] {
                    self.quad(c, prim, (qx as i32, qy as i32), cov);
                }
            }
        }
    }

    fn quad(&mut self, c: &mut Ctx, prim: &Primitive, at: (i32, i32), cov: [u32; 4]) {
        let n = self.fill.n;
        self.fill.quads[n] = at;
        self.fill.coverage[4 * n..4 * n + 4].copy_from_slice(&cov);
        self.fill.n += 1;
        if self.fill.n == L / 4 {
            self.flush(c, prim);
        }
    }

    /// A point: a square (or a sprite) of `size` pixels.
    fn point(&mut self, c: &mut Ctx, v: &[u32; WORDS], w: Win, size: f32, layer: u32) {
        let h = size * 0.5;
        let corner = |x: f32, y: f32| Win { x: w.x + x, y: w.y + y, ..w };
        let quad = [corner(-h, -h), corner(h, -h), corner(h, h), corner(-h, h)];
        let mut prim = self.primitive([quad[0], quad[1], quad[2]], [v, v, v], v, true, 0.0, layer);
        // nothing varies over a point
        prim.z = Linear::constant(w.z);
        prim.w = Linear::constant(w.w);
        for &(k, i) in &self.s.inputs {
            if let Plane::Linear(_) = prim.planes[k] {
                let val = if i == Interp::Perspective { f(v, k) * w.w } else { f(v, k) };
                prim.planes[k] = Plane::Linear(Linear::constant(val));
            }
        }
        prim.is_point = true;
        let upper_left = self.s.sprite >> 2 & 1 != 0;
        prim.point[0] = Linear { value: 0.0, dx: 1.0 / size, dy: 0.0, x0: w.x - h, y0: 0.0 };
        prim.point[1] = if upper_left { Linear { value: 0.0, dx: 0.0, dy: 1.0 / size, x0: 0.0, y0: w.y - h } } else { Linear { value: 1.0, dx: 0.0, dy: -1.0 / size, x0: 0.0, y0: w.y - h } };
        self.scan(c, &prim, [quad[0], quad[1], quad[2]]);
        self.scan(c, &prim, [quad[0], quad[2], quad[3]]);
    }

    /// A line as a rectangle around it, `width` pixels wide across its minor axis.
    #[allow(clippy::too_many_arguments)]
    fn line(&mut self, c: &mut Ctx, va: &[u32; WORDS], vb: &[u32; WORDS], a: Win, b: Win, provoking: &[u32; WORDS], width: f32, layer: u32) {
        let (dx, dy) = (b.x - a.x, b.y - a.y);
        if dx == 0.0 && dy == 0.0 {
            return;
        }
        let h = width * 0.5;
        let (ox, oy) = if dx.abs() >= dy.abs() { (0.0, h) } else { (h, 0.0) };
        let shift = |w: Win, s: f32| Win { x: w.x + ox * s, y: w.y + oy * s, ..w };
        let q = [shift(a, -1.0), shift(b, -1.0), shift(b, 1.0), shift(a, 1.0)];
        let prim = self.primitive([q[0], q[1], q[2]], [va, vb, vb], provoking, true, 0.0, layer);
        self.scan(c, &prim, [q[0], q[1], q[2]]);
        self.scan(c, &prim, [q[0], q[2], q[3]]);
    }

    /// Shade and write the quads collected so far.
    fn flush(&mut self, c: &mut Ctx, prim: &Primitive) {
        let n = std::mem::take(&mut self.fill.n);
        if n == 0 {
            return;
        }
        let s = self.s;
        let quads = self.fill.quads;
        let mut cov = self.fill.coverage;
        cov[4 * n..].fill(0);
        let z_at = |l: usize, sample: usize| {
            let (qx, qy) = quads[l / 4];
            let (sx, sy) = if s.samples == 1 { (128, 128) } else { s.positions[sample] };
            let (x, y) = ((qx + (l as i32 & 1)) as f32 + sx as f32 / 256.0, (qy + (l as i32 >> 1 & 1)) as f32 + sy as f32 / 256.0);
            let z = prim.z.at(x, y);
            if s.clamp_z { z.clamp(s.zrange.0, s.zrange.1) } else { z.clamp(0.0, 1.0) }
        };
        if s.early {
            depth_test(s, c, prim, &quads, &mut cov, n, &z_at);
        }
        if cov.iter().all(|&m| m == 0) {
            return;
        }
        let Some(fs) = &s.fs else {
            if !s.early {
                depth_test(s, c, prim, &quads, &mut cov, n, &z_at);
            }
            c.counters.samples += cov.iter().map(|m| m.count_ones() as u64).sum::<u64>();
            return;
        };
        let passes = if s.per_sample { s.samples } else { 1 };
        let out = fs.sph.frag_outputs();
        for pass in 0..passes {
            let sample = s.per_sample.then_some(pass);
            let mut live = cov;
            if let Some(k) = sample {
                live.iter_mut().for_each(|m| *m &= 1 << k);
            }
            let mut helper = 0u32;
            let mut run = 0u32;
            for q in 0..n {
                let m = (0..4).fold(0, |m, l| m | u32::from(live[4 * q + l] != 0) << l);
                if m != 0 {
                    run |= 15 << (4 * q);
                    helper |= (15 & !m) << (4 * q);
                }
            }
            if run == 0 {
                continue;
            }
            c.counters.fragments += (run & !helper).count_ones() as u64;
            c.lanes.set_local_size((fs.sph.local_low + fs.sph.local_high + fs.sph.local_crs) as usize);
            c.lanes.start(&fs.program, run, helper);
            let mut host = FragmentHost { common: c.common, setup: s, prim, quads, coverage: live, sample };
            let mut fuel = 4_000_000;
            let e = maxwell_shader::run(&fs.program, c.lanes, &mut host, &mut fuel);
            if !matches!(e, Exit::Done) && c.error.is_none() {
                c.error = Some(format!("fragment shader stopped: {e:?}"));
            }
            let alive = c.lanes.live();
            for (l, m) in live.iter_mut().enumerate() {
                if alive >> l & 1 == 0 {
                    *m = 0;
                }
            }
            let reg = |r: Option<u8>, l: usize, c: &Ctx| r.map(|r| c.lanes.gpr[r as usize][l]);
            if let Some(r) = out.sample_mask {
                (0..4 * n).for_each(|l| live[l] &= c.lanes.gpr[r as usize][l]);
            }
            if let Some((func, reference)) = s.alpha_test {
                for (l, m) in live.iter_mut().enumerate().take(4 * n) {
                    let a = f32::from_bits(reg(out.color[0][3], l, c).unwrap_or(0));
                    if !compare(func, a, reference) {
                        *m = 0;
                    }
                }
            }
            if s.alpha_to_coverage {
                for (l, m) in live.iter_mut().enumerate().take(4 * n) {
                    let a = f32::from_bits(reg(out.color[0][3], l, c).unwrap_or(0)).clamp(0.0, 1.0);
                    let on = (a * s.samples as f32 + 0.5) as u32;
                    *m &= (1u32 << on).wrapping_sub(1);
                }
            }
            if !s.early {
                let depth = out.depth;
                let gpr: Option<[u32; L]> = depth.map(|r| c.lanes.gpr[r as usize]);
                let z = |l: usize, k: usize| match &gpr {
                    Some(g) => f32::from_bits(g[l]).clamp(0.0, 1.0),
                    None => z_at(l, k),
                };
                depth_test(s, c, prim, &quads, &mut live, n, &z);
            }
            c.counters.samples += live.iter().map(|m| m.count_ones() as u64).sum::<u64>();
            if s.outputs.is_empty() {
                continue;
            }
            // colours
            for q in 0..n {
                if live[4 * q..4 * q + 4] == [0; 4] {
                    continue;
                }
                let (qx, qy) = (quads[q].0 as u32, quads[q].1 as u32);
                let tile = c.rop.tile(qx / TILE, qy / TILE, prim.layer, s.need, s.write, &*c.common.mem);
                for (rt, o) in &s.outputs {
                    let regs = &out.color[if fs.sph.color0_broadcast { 0 } else { *rt }];
                    for k in 0..4 {
                        let l = 4 * q + k;
                        if live[l] == 0 {
                            continue;
                        }
                        let color: [u32; 4] = std::array::from_fn(|i| regs[i].map_or(if i == 3 { 1f32.to_bits() } else { 0 }, |r| c.lanes.gpr[r as usize][l]));
                        let src1: [f32; 4] = if s.dual { std::array::from_fn(|i| out.color[1][i].map_or(0.0, |r| f32::from_bits(c.lanes.gpr[r as usize][l]))) } else { [0.0; 4] };
                        let (x, y) = ((qx + (k as u32 & 1)) % TILE, (qy + (k as u32 >> 1)) % TILE);
                        write_samples(tile, *rt, o, (x, y), live[l], color, src1, s);
                    }
                }
            }
        }
    }
}

#[inline]
#[allow(clippy::too_many_arguments)]
fn write_samples(tile: &mut TileSet, rt: usize, o: &crate::rop::Output, (x, y): (u32, u32), mut mask: u32, color: [u32; 4], src1: [f32; 4], s: &Setup) {
    while mask != 0 {
        let k = mask.trailing_zeros();
        mask &= mask - 1;
        let p = tile.color[rt].sample_mut(x, y, k);
        o.write(p, color, src1, s.constant, s.logic);
    }
}

/// Test the covered samples; those that fail are taken out of the coverage.
fn depth_test(s: &Setup, c: &mut Ctx, prim: &Primitive, quads: &[(i32, i32); L / 4], cov: &mut [u32; L], n: usize, z: &dyn Fn(usize, usize) -> f32) {
    let Some((_, format)) = s.targets.zeta.filter(|_| s.ds.used()) else { return };
    for q in 0..n {
        if cov[4 * q..4 * q + 4] == [0; 4] {
            continue;
        }
        let (qx, qy) = (quads[q].0 as u32, quads[q].1 as u32);
        let tile = c.rop.tile(qx / TILE, qy / TILE, prim.layer, s.need, s.write, &*c.common.mem);
        for k in 0..4 {
            let l = 4 * q + k;
            let (x, y) = ((qx + (k as u32 & 1)) % TILE, (qy + (k as u32 >> 1)) % TILE);
            let mut m = cov[l];
            while m != 0 {
                let smp = m.trailing_zeros();
                m &= m - 1;
                let p = tile.zeta.sample_mut(x, y, smp);
                if !s.ds.test(&format, p, z(l, smp as usize), !prim.front) {
                    cov[l] &= !(1 << smp);
                }
            }
        }
    }
}
