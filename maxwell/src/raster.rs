//! Primitives: clipping, the viewport transform, setup and binning; the rasterizer and the fragment
//! pipeline of a tile.
use crate::pipe::{FragmentHost, Setup, Work, Worker, CLIP, L, POSITION, WORDS};
use crate::rop::{compare, TileSet, TILE};
use crate::threed::{Counters, Threed};
use crate::Memory;
use maxwell_shader::sph::Interp;
use maxwell_shader::Exit;

/// Shaded vertices: the words of them that a shader writes, one after the other.
pub struct Verts {
    map: [u8; WORDS],
    pub stride: usize,
    pub data: Vec<u32>,
    /// the words that clipping interpolates
    lerp: Vec<bool>,
}

const NONE: u8 = 255;

impl Verts {
    /// With these words of the attributes (and the position).
    pub fn new(words: impl Iterator<Item = usize>) -> Self {
        let mut map = [NONE; WORDS];
        let mut stride = 0;
        for w in (POSITION..POSITION + 4).chain(words) {
            if map[w] == NONE {
                map[w] = stride;
                stride += 1;
            }
        }
        Self { map, stride: stride as usize, data: Vec::new(), lerp: Vec::new() }
    }

    /// Append `n` vertices (zeros, with w = 1).
    pub fn grow(&mut self, n: usize) {
        let at = self.data.len();
        self.data.resize(at + n * self.stride, 0);
        let w = self.map[POSITION + 3] as usize;
        self.data[at..].chunks_exact_mut(self.stride).for_each(|v| v[w] = 1f32.to_bits());
    }

    pub fn len(&self) -> usize {
        self.data.len() / self.stride
    }

    pub fn is_empty(&self) -> bool {
        self.data.is_empty()
    }

    /// Where word `w` is in a vertex.
    #[inline(always)]
    pub fn slot(&self, w: usize) -> Option<usize> {
        let s = self.map[w];
        (s != NONE).then_some(s as usize)
    }

    #[inline(always)]
    pub fn get(&self, v: u32, w: usize) -> u32 {
        self.slot(w).map_or(0, |s| self.data[v as usize * self.stride + s])
    }

    #[inline(always)]
    fn f(&self, v: u32, w: usize) -> f32 {
        f32::from_bits(self.get(v, w))
    }

    #[inline(always)]
    fn position(&self, v: u32) -> [f32; 4] {
        // (the position's words come first)
        let p = &self.data[v as usize * self.stride..][..4];
        std::array::from_fn(|k| f32::from_bits(p[k]))
    }

    /// A vertex between two.
    fn mix(&mut self, a: u32, b: u32, t: f32) -> u32 {
        let n = self.len() as u32;
        for s in 0..self.stride {
            let (x, y) = (self.data[a as usize * self.stride + s], self.data[b as usize * self.stride + s]);
            self.data.push(if self.lerp[s] { (f32::from_bits(x) + (f32::from_bits(y) - f32::from_bits(x)) * t).to_bits() } else { x });
        }
        n
    }
}

/// The primitives of a draw: `size` indices each.
pub struct Geometry {
    pub vertices: Verts,
    pub primitives: Vec<u32>,
    pub size: usize,
    /// each primitive's number, if they do not count up from 0
    pub ids: Option<Vec<u32>>,
}

impl Geometry {
    /// Keep some vertices of each primitive.
    pub fn pick(self, which: &[usize]) -> Self {
        let primitives = self.primitives.chunks_exact(self.size).flat_map(|p| which.iter().map(|&k| p[k])).collect();
        Self { primitives, size: which.len(), ..self }
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

/// One attribute of a primitive.
#[derive(Clone, Copy, Debug)]
pub struct Slot {
    /// the provoking vertex's value
    pub flat: u32,
    /// None: the same everywhere
    pub linear: Option<Linear>,
}

type Edge = (i64, i64, i64);

/// A triangle (or half a point or a line) set up for rasterizing.
pub struct Prim {
    edges: [Edge; 3],
    /// pixels [x0, x1) × [y0, y1)
    bounds: (i32, i32, i32, i32),
    pub z: Linear,
    /// 1/w
    pub w: Linear,
    /// its first attribute in `Scene::slots`
    pub slots: u32,
    /// of a point: its coordinates in `Scene::points`
    pub point: Option<u32>,
    pub front: bool,
    pub id: u32,
    pub layer: u32,
}

/// Everything a draw rasterizes.
#[derive(Default)]
pub struct Scene {
    pub prims: Vec<Prim>,
    pub slots: Vec<Slot>,
    pub points: Vec<[Linear; 2]>,
    /// a guess at the pixels
    pub area: u64,
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
    transform: bool,
}

/// Set up every primitive of `g`.
pub fn setup(t: &Threed, g: &mut Geometry, s: &Setup) -> Scene {
    let v = View {
        scale: [t.f(0xa00), t.f(0xa04), t.f(0xa08)],
        offset: [t.f(0xa0c), t.f(0xa10), t.f(0xa14)],
        zero_to_one: t.r(0xd7c) & 1 != 0,
        clamp_near: t.r(0x193c) & 8 != 0,
        clamp_far: t.r(0x193c) & 0x10 != 0,
        planes: t.r(0x1510) & 0xff,
        transform: t.r(0x192c) & 1 != 0,
    };
    let cull = (t.r(0x1918) & 1 != 0).then(|| t.r(0x1920));
    // clockwise (in window coordinates with y down) is the front
    let front_negative = t.r(0x191c) == 0x900;
    let (mode_front, mode_back) = (t.r(0xdac), t.r(0xdb0));
    // glPolygonOffset, for point, line and filled polygons
    let offset_on = [t.r(0xdc0) & 1 != 0, t.r(0xdc4) & 1 != 0, t.r(0xdc8) & 1 != 0];
    let (factor, units, clamp) = (t.f(0x156c), t.f(0x15bc), t.f(0x187c));
    let width = if t.r(0x1570) & 1 != 0 { t.f(0x13b0) } else { t.f(0x13b4) }.max(1.0);
    let (point_size, attribute_size) = (t.f(0x1518), t.r(0x1910) & 1 != 0);
    let first_provokes = t.r(0x1684) & 1 == 0;

    let vs = &mut g.vertices;
    vs.lerp = vec![false; vs.stride];
    let smooth = s.inputs.iter().filter(|i| i.1 != Interp::Constant).map(|i| i.0);
    for w in (POSITION..POSITION + 4).chain(CLIP..CLIP + 8).chain(smooth) {
        if let Some(k) = vs.slot(w) {
            vs.lerp[k] = true;
        }
    }
    let vertices = vs.len();
    let mut b = Build { s, scene: Scene::default(), id: 0, layer: 0 };
    let mut poly: Vec<u32> = Vec::new();
    let mut spare: Vec<u32> = Vec::new();
    let mut wins: Vec<Win> = Vec::new();
    for (id, p) in g.primitives.chunks_exact(g.size).enumerate() {
        b.id = g.ids.as_ref().map_or(id as u32, |i| i[id]);
        b.layer = vs.get(p[0], crate::pipe::LAYER);
        if g.size == 1 {
            let size = if attribute_size { vs.f(p[0], crate::pipe::POINT_SIZE) } else { point_size };
            if outside(&v, vs, p[0]) & !GUARDS == 0 {
                b.point(vs, p[0], window(&v, vs, p[0]), size.max(1.0));
            }
            continue;
        }
        let codes: [u32; 3] = std::array::from_fn(|k| p.get(k).map_or(0, |&i| outside(&v, vs, i)));
        poly.clear();
        poly.extend_from_slice(p);
        if codes[0] | codes[1] | codes[2] != 0 {
            let last = if g.size == 2 { codes[1] } else { codes[2] };
            if codes[0] & codes[1] & last != 0 {
                continue;
            }
            clip(&v, vs, &mut poly, &mut spare, codes[0] | codes[1] | codes[2]);
        }
        if poly.len() < g.size {
            continue;
        }
        let provoking = if first_provokes { p[0] } else { p[g.size - 1] };
        wins.clear();
        wins.extend(poly.iter().map(|&i| window(&v, vs, i)));
        if g.size == 2 {
            b.line(vs, [poly[0], poly[1]], [wins[0], wins[1]], provoking, width);
        } else {
            let area = area_of(&wins);
            if area == 0.0 || !area.is_finite() {
                vs.data.truncate(vertices * vs.stride);
                continue;
            }
            // y goes down in window coordinates: a positive area is clockwise
            let front = (area > 0.0) == front_negative;
            let culled = cull.is_some_and(|c| c == 0x408 || c == 0x404 && front || c == 0x405 && !front);
            if !culled {
                let mode = if front { mode_front } else { mode_back };
                let bias = |on: bool| if on { depth_bias(&wins, factor, units, clamp, s) } else { 0.0 };
                match mode {
                    0x1b00 | 0x1b01 => {
                        let z = bias(offset_on[(mode & 1) as usize]);
                        wins.iter_mut().for_each(|w| w.z += z);
                        for k in 0..poly.len() {
                            if mode == 0x1b00 {
                                b.point(vs, poly[k], wins[k], point_size.max(1.0));
                            } else {
                                let n = (k + 1) % poly.len();
                                b.line(vs, [poly[k], poly[n]], [wins[k], wins[n]], provoking, width);
                            }
                        }
                    }
                    _ => {
                        let z = bias(offset_on[2]);
                        for k in 1..poly.len() - 1 {
                            b.triangle(vs, [poly[0], poly[k], poly[k + 1]], [wins[0], wins[k], wins[k + 1]], provoking, front, z);
                        }
                    }
                }
            }
        }
        // (the vertices clipping made)
        vs.data.truncate(vertices * vs.stride);
    }
    b.scene
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
    let r = match s.targets.zeta {
        Some((_, f)) if f.is_float() => {
            let m = w.iter().map(|w| w.z.abs()).fold(0.0, f32::max);
            2f32.powi((m.to_bits() >> 23) as i32 - 127 - 23)
        }
        Some((_, f)) if f.depth_bits == 16 => 1.0 / 65536.0,
        _ => 1.0 / 16_777_216.0,
    };
    // (the units register holds twice the API's)
    let b = dx.abs().max(dy.abs()) * factor + units * 0.5 * r;
    if clamp > 0.0 {
        b.min(clamp)
    } else if clamp < 0.0 {
        b.max(clamp)
    } else {
        b
    }
}

/// The planes that only keep coordinates small.
const GUARDS: u32 = 0xf << 3;
const GUARD: f32 = 32.0;

/// The distance to a clip plane (inside: not negative).
#[inline]
fn distance(v: &View, vs: &Verts, p: u32, plane: u32) -> f32 {
    let [x, y, z, w] = vs.position(p);
    match plane {
        0 => w - 1e-12,
        1 if v.clamp_near => 1.0,
        1 => if v.zero_to_one { z } else { z + w },
        2 if v.clamp_far => 1.0,
        2 => w - z,
        3 => GUARD * w - x,
        4 => GUARD * w + x,
        5 => GUARD * w - y,
        6 => GUARD * w + y,
        _ if v.planes >> (plane - 7) & 1 != 0 => vs.f(p, CLIP + plane as usize - 7),
        _ => 1.0,
    }
}

/// The planes a vertex is outside of.
#[inline]
fn outside(v: &View, vs: &Verts, p: u32) -> u32 {
    let [x, y, z, w] = vs.position(p);
    let g = GUARD * w;
    let near = if v.clamp_near { 1.0 } else if v.zero_to_one { z } else { z + w };
    let far = if v.clamp_far { 1.0 } else { w - z };
    // (not "less than": NaNs are outside)
    let mut code = 0;
    for (k, d) in [w - 1e-12, near, far, g - x, g + x, g - y, g + y].into_iter().enumerate() {
        code |= u32::from(d.is_nan() || d < 0.0) << k;
    }
    let mut planes = v.planes;
    while planes != 0 {
        let k = planes.trailing_zeros();
        planes &= planes - 1;
        let d = vs.f(p, CLIP + k as usize);
        code |= u32::from(d.is_nan() || d < 0.0) << (7 + k);
    }
    code
}

/// Clip a polygon or a line to the planes in `codes`.
fn clip(v: &View, vs: &mut Verts, poly: &mut Vec<u32>, out: &mut Vec<u32>, codes: u32) {
    let line = poly.len() == 2;
    for plane in (0..15).filter(|k| codes >> k & 1 != 0) {
        out.clear();
        let n = poly.len();
        for k in 0..if line { n.min(2).saturating_sub(1) } else { n } {
            let (p, q) = (poly[k], poly[(k + 1) % n]);
            let (a, b) = (distance(v, vs, p, plane), distance(v, vs, q, plane));
            if a >= 0.0 {
                out.push(p);
            }
            if (a >= 0.0) != (b >= 0.0) {
                // always from the inside out, so that neighbours agree on the point
                let m = if a >= 0.0 { vs.mix(p, q, a / (a - b)) } else { vs.mix(q, p, b / (b - a)) };
                out.push(m);
            }
            if line && b >= 0.0 {
                out.push(q);
            }
        }
        std::mem::swap(poly, out);
        if poly.is_empty() {
            break;
        }
    }
}

#[inline]
fn window(v: &View, vs: &Verts, p: u32) -> Win {
    let [x, y, z, w] = vs.position(p);
    if !v.transform {
        return Win { x, y, z, w };
    }
    let i = 1.0 / w;
    Win { x: x * i * v.scale[0] + v.offset[0], y: y * i * v.scale[1] + v.offset[1], z: z * i * v.scale[2] + v.offset[2], w: i }
}

/// A plane through three values.
struct Plane3 {
    ax: f64,
    ay: f64,
    bx: f64,
    by: f64,
    inv: f64,
    x0: f32,
    y0: f32,
}

impl Plane3 {
    fn new(w: &[Win; 3]) -> Self {
        let (ax, ay) = ((w[1].x - w[0].x) as f64, (w[1].y - w[0].y) as f64);
        let (bx, by) = ((w[2].x - w[0].x) as f64, (w[2].y - w[0].y) as f64);
        Self { ax, ay, bx, by, inv: 1.0 / (ax * by - bx * ay), x0: w[0].x, y0: w[0].y }
    }

    fn through(&self, v: [f32; 3]) -> Linear {
        let (av, bv) = (v[1] as f64 - v[0] as f64, v[2] as f64 - v[0] as f64);
        let dx = (av * self.by - bv * self.ay) * self.inv;
        let dy = (bv * self.ax - av * self.bx) * self.inv;
        Linear { value: v[0], dx: dx as f32, dy: dy as f32, x0: self.x0, y0: self.y0 }
    }
}

struct Build<'s> {
    s: &'s Setup,
    scene: Scene,
    id: u32,
    layer: u32,
}

impl Build<'_> {
    /// The attributes of a triangle; returns where they start.
    fn slots(&mut self, vs: &Verts, w: &[Win; 3], plane: &Plane3, v: [u32; 3], provoking: u32) -> u32 {
        let at = self.scene.slots.len() as u32;
        for &(k, i) in &self.s.inputs {
            let Some(slot) = vs.slot(k) else {
                self.scene.slots.push(Slot { flat: 0, linear: None });
                continue;
            };
            let value = |n: u32| vs.data[n as usize * vs.stride + slot];
            let flat = value(provoking);
            let is_flat = i == Interp::Constant || self.s.flat && (0x280 - 0x40..0x2c0 - 0x40).contains(&(4 * k));
            let linear = (!is_flat).then(|| {
                let over = |n: usize| f32::from_bits(value(v[n])) * if i == Interp::Perspective { w[n].w } else { 1.0 };
                plane.through([over(0), over(1), over(2)])
            });
            self.scene.slots.push(Slot { flat, linear });
        }
        at
    }

    fn triangle(&mut self, vs: &Verts, v: [u32; 3], w: [Win; 3], provoking: u32, front: bool, bias: f32) {
        let Some((edges, bounds)) = self.edges(&w) else { return };
        let plane = Plane3::new(&w);
        let slots = self.slots(vs, &w, &plane, v, provoking);
        let prim = Prim {
            edges,
            bounds,
            z: plane.through([w[0].z + bias, w[1].z + bias, w[2].z + bias]),
            w: plane.through([w[0].w, w[1].w, w[2].w]),
            slots,
            point: None,
            front,
            id: self.id,
            layer: self.layer,
        };
        self.push(prim);
    }

    /// A point: a square (or a sprite) of `size` pixels.
    fn point(&mut self, vs: &Verts, v: u32, w: Win, size: f32) {
        let h = size * 0.5;
        let corner = |x: f32, y: f32| Win { x: w.x + x, y: w.y + y, ..w };
        let quad = [corner(-h, -h), corner(h, -h), corner(h, h), corner(-h, h)];
        // nothing varies over a point
        let slots = self.scene.slots.len() as u32;
        for &(k, i) in &self.s.inputs {
            let flat = vs.get(v, k);
            let linear = match i {
                Interp::Constant => None,
                Interp::Perspective => Some(Linear::constant(f32::from_bits(flat) * w.w)),
                _ => Some(Linear::constant(f32::from_bits(flat))),
            };
            self.scene.slots.push(Slot { flat, linear });
        }
        let upper_left = self.s.sprite >> 2 & 1 != 0;
        let across = Linear { value: 0.0, dx: 1.0 / size, dy: 0.0, x0: w.x - h, y0: 0.0 };
        let (value, dy) = if upper_left { (0.0, 1.0 / size) } else { (1.0, -1.0 / size) };
        self.scene.points.push([across, Linear { value, dx: 0.0, dy, x0: 0.0, y0: w.y - h }]);
        let point = Some(self.scene.points.len() as u32 - 1);
        for t in [[quad[0], quad[1], quad[2]], [quad[0], quad[2], quad[3]]] {
            if let Some((edges, bounds)) = self.edges(&t) {
                let prim = Prim { edges, bounds, z: Linear::constant(w.z), w: Linear::constant(w.w), slots, point, front: true, id: self.id, layer: self.layer };
                self.push(prim);
            }
        }
    }

    /// A line as a parallelogram, `width` pixels wide across its minor axis.
    fn line(&mut self, vs: &Verts, v: [u32; 2], [a, b]: [Win; 2], provoking: u32, width: f32) {
        let (dx, dy) = (b.x - a.x, b.y - a.y);
        if dx == 0.0 && dy == 0.0 {
            return;
        }
        let h = width * 0.5;
        let (ox, oy) = if dx.abs() >= dy.abs() { (0.0, h) } else { (h, 0.0) };
        let shift = |w: Win, s: f32| Win { x: w.x + ox * s, y: w.y + oy * s, ..w };
        let q = [shift(a, -1.0), shift(b, -1.0), shift(b, 1.0), shift(a, 1.0)];
        let first = [q[0], q[1], q[2]];
        let plane = Plane3::new(&first);
        let slots = self.slots(vs, &first, &plane, [v[0], v[1], v[1]], provoking);
        let (z, w) = (plane.through([a.z, b.z, b.z]), plane.through([a.w, b.w, b.w]));
        for t in [first, [q[0], q[2], q[3]]] {
            if let Some((edges, bounds)) = self.edges(&t) {
                let prim = Prim { edges, bounds, z, w, slots, point: None, front: true, id: self.id, layer: self.layer };
                self.push(prim);
            }
        }
    }

    /// The edge functions and the bounds of a triangle: None if it covers nothing.
    #[allow(clippy::type_complexity)]
    fn edges(&self, w: &[Win; 3]) -> Option<([Edge; 3], (i32, i32, i32, i32))> {
        let fix = |v: f32| (v * 256.0).round() as i64;
        let mut p: [(i64, i64); 3] = std::array::from_fn(|k| (fix(w[k].x), fix(w[k].y)));
        let area = (p[1].0 - p[0].0) * (p[2].1 - p[0].1) - (p[2].0 - p[0].0) * (p[1].1 - p[0].1);
        if area == 0 {
            return None;
        }
        if area < 0 {
            p.swap(1, 2);
        }
        // edge k: from vertex k to the next; inside is positive. Pixels exactly on a top or left
        // edge belong to the triangle
        let edges = std::array::from_fn(|k| {
            let (a, b) = (p[k], p[(k + 1) % 3]);
            let (dx, dy) = (b.0 - a.0, b.1 - a.1);
            let top_left = dy < 0 || dy == 0 && dx > 0;
            // e(x, y) = dx (y - ay) - dy (x - ax)
            (-dy, dx, dy * a.0 - dx * a.1 - if top_left { 0 } else { 1 })
        });
        let r = self.s.rect;
        let min = |f: fn(&(i64, i64)) -> i64| p.iter().map(f).min().unwrap();
        let max = |f: fn(&(i64, i64)) -> i64| p.iter().map(f).max().unwrap();
        let x0 = (min(|p| p.0) >> 8).max(r.x0 as i64) as i32;
        let y0 = (min(|p| p.1) >> 8).max(r.y0 as i64) as i32;
        let x1 = ((max(|p| p.0) + 255) >> 8).min(r.x1 as i64) as i32;
        let y1 = ((max(|p| p.1) + 255) >> 8).min(r.y1 as i64) as i32;
        (x0 < x1 && y0 < y1).then_some((edges, (x0, y0, x1, y1)))
    }

    fn push(&mut self, p: Prim) {
        let (x0, y0, x1, y1) = p.bounds;
        self.scene.area += (x1 - x0) as u64 * (y1 - y0) as u64 / 2 + 16;
        self.scene.prims.push(p);
    }
}

/// Whether a square of pixels is outside (None), partly or wholly inside (true) the triangle.
#[inline]
fn square(edges: &[Edge; 3], x: i64, y: i64, n: i64) -> Option<bool> {
    let mut whole = true;
    for &(a, b, e) in edges {
        let at = |x: i64, y: i64| a * x + b * y + e;
        let (lx, hx) = (x << 8, (x + n) << 8);
        let (ly, hy) = (y << 8, (y + n) << 8);
        if at(if a > 0 { hx } else { lx }, if b > 0 { hy } else { ly }) < 0 {
            return None;
        }
        whole &= at(if a > 0 { lx } else { hx }, if b > 0 { ly } else { hy }) >= 0;
    }
    Some(whole)
}

const T: i32 = TILE as i32;

/// Rasterize a scene.
pub fn draw(t: &mut Threed, scene: &Scene, s: &Setup, work: &Work, mem: &mut dyn Memory) {
    let mut layers: Vec<u32> = Vec::new();
    for p in &scene.prims {
        if layers.last() != Some(&p.layer) && !layers.contains(&p.layer) {
            layers.push(p.layer);
        }
    }
    let threads = if s.serial { 1 } else { crate::pipe::threads().min((scene.area / 6000) as usize).max(1) };
    let (tx0, ty0) = (s.rect.x0 as i32 / T, s.rect.y0 as i32 / T);
    let (w, h) = ((s.rect.x1 as i32 + T - 1) / T - tx0, (s.rect.y1 as i32 + T - 1) / T - ty0);
    let mut bins = std::mem::take(&mut t.pipe.bins);
    if bins.len() < (w * h) as usize {
        bins.resize((w * h) as usize, Vec::new());
    }
    for layer in layers {
        let mut used: Vec<u32> = Vec::new();
        for (k, p) in scene.prims.iter().enumerate().filter(|(_, p)| p.layer == layer) {
            let (x0, y0, x1, y1) = p.bounds;
            let one = x0 / T == (x1 - 1) / T && y0 / T == (y1 - 1) / T;
            for ty in y0 / T..=(y1 - 1) / T {
                for tx in x0 / T..=(x1 - 1) / T {
                    if one || square(&p.edges, (tx * T) as i64, (ty * T) as i64, T as i64).is_some() {
                        let at = (ty - ty0) * w + tx - tx0;
                        let bin = &mut bins[at as usize];
                        if bin.is_empty() {
                            used.push(at as u32);
                        }
                        bin.push(k as u32);
                    }
                }
            }
        }
        let tiles: Vec<(u32, u32, u32)> = used.iter().map(|&at| (at, (tx0 + at as i32 % w) as u32, (ty0 + at as i32 / w) as u32)).collect();
        run(t, scene, s, work, mem, layer, &tiles, &bins, threads);
        for at in used {
            bins[at as usize].clear();
        }
    }
    t.pipe.bins = bins;
}

/// Run the tiles of a layer.
#[allow(clippy::too_many_arguments)]
fn run(t: &mut Threed, scene: &Scene, s: &Setup, work: &Work, mem: &mut dyn Memory, layer: u32, tiles: &[(u32, u32, u32)], bins: &[Vec<u32>], threads: usize) {
    let threads = threads.min(tiles.len());
    t.pipe.need_workers(threads.max(1));
    #[cfg(not(target_arch = "wasm32"))]
    if threads > 1 && mem.share().is_some() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        use std::sync::Mutex;
        let sets: Vec<Mutex<Option<Box<TileSet>>>> = tiles.iter().map(|&(_, x, y)| Mutex::new(Some(t.rop.take(x, y, layer)))).collect();
        let next = AtomicUsize::new(0);
        let (rop, pipe) = (&t.rop, &mut t.pipe);
        let memory = &*mem;
        let results: Vec<(Counters, Option<String>)> = std::thread::scope(|scope| {
            let handles: Vec<_> = pipe.workers[..threads]
                .iter_mut()
                .map(|w| {
                    let (sets, next) = (&sets, &next);
                    let mut mine = memory.share().unwrap();
                    scope.spawn(move || {
                        let mut out = (Counters::default(), None);
                        loop {
                            let k = next.fetch_add(1, Ordering::Relaxed);
                            let Some(&(at, x, y)) = tiles.get(k) else { break };
                            let mut set = sets[k].lock().unwrap().take().unwrap();
                            rop.load(&mut set, x, y, layer, s.need, s.write, &*mine);
                            tile(w, work, scene, s, &mut *mine, &mut set, (x as i32 * T, y as i32 * T), &bins[at as usize], &mut out);
                            *sets[k].lock().unwrap() = Some(set);
                        }
                        out
                    })
                })
                .collect();
            handles.into_iter().map(|h| h.join().unwrap_or_else(|e| std::panic::resume_unwind(e))).collect()
        });
        for (set, &(_, x, y)) in sets.into_iter().zip(tiles) {
            t.rop.put(x, y, layer, set.into_inner().unwrap().unwrap());
        }
        for (c, e) in results {
            t.counters.add(&c);
            if let Some(e) = e {
                t.pipe.fail(e);
            }
        }
        return;
    }
    let mut out = (Counters::default(), None);
    for &(at, x, y) in tiles {
        let mut set = t.rop.take(x, y, layer);
        t.rop.load(&mut set, x, y, layer, s.need, s.write, mem);
        tile(&mut t.pipe.workers[0], work, scene, s, mem, &mut set, (x as i32 * T, y as i32 * T), &bins[at as usize], &mut out);
        t.rop.put(x, y, layer, set);
    }
    t.counters.add(&out.0);
    if let Some(e) = out.1 {
        t.pipe.fail(e);
    }
}

/// Draw a bin's primitives into a tile.
#[allow(clippy::too_many_arguments)]
fn tile(w: &mut Worker, work: &Work, scene: &Scene, s: &Setup, mem: &mut dyn Memory, set: &mut TileSet, (tx, ty): (i32, i32), bin: &[u32], out: &mut (Counters, Option<String>)) {
    let mut sh = Shade { w, work, scene, s, mem, set, out, batch: Batch::default() };
    let all = (1u32 << s.samples) - 1;
    let mask = all & s.sample_mask;
    for &k in bin {
        let p = &scene.prims[k as usize];
        let x0 = p.bounds.0.max(tx);
        let y0 = p.bounds.1.max(ty);
        let x1 = p.bounds.2.min(tx + T);
        let y1 = p.bounds.3.min(ty + T);
        let mut by = y0 & !7;
        while by < y1 {
            let mut bx = x0 & !7;
            while bx < x1 {
                if let Some(whole) = square(&p.edges, bx as i64, by as i64, 8) {
                    let whole = whole && bx >= x0 && by >= y0 && bx + 8 <= x1 && by + 8 <= y1;
                    for qy in (by..by + 8).step_by(2) {
                        for qx in (bx..bx + 8).step_by(2) {
                            let mut cov = [0u32; 4];
                            if whole {
                                cov = [mask; 4];
                            } else {
                                for (l, m) in cov.iter_mut().enumerate() {
                                    let (x, y) = (qx + (l as i32 & 1), qy + (l as i32 >> 1));
                                    if x < x0 || x >= x1 || y < y0 || y >= y1 {
                                        continue;
                                    }
                                    for k in 0..s.samples as usize {
                                        let (sx, sy) = s.positions[k];
                                        let (px, py) = (((x as i64) << 8) + sx as i64, ((y as i64) << 8) + sy as i64);
                                        if p.edges.iter().all(|&(a, b, e)| a * px + b * py + e >= 0) {
                                            *m |= 1 << k;
                                        }
                                    }
                                    *m &= mask;
                                }
                            }
                            if cov != [0; 4] {
                                sh.quad(k, (qx, qy), cov);
                            }
                        }
                    }
                }
                bx += 8;
            }
            by += 8;
        }
    }
    sh.flush();
}

/// Quads waiting to be shaded.
#[derive(Default)]
struct Batch {
    n: usize,
    quads: [(i32, i32); L / 4],
    prims: [u32; L / 4],
    coverage: [u32; L],
}

struct Shade<'a, 'm> {
    w: &'a mut Worker,
    work: &'a Work,
    scene: &'a Scene,
    s: &'a Setup,
    mem: &'m mut dyn Memory,
    set: &'a mut TileSet,
    out: &'a mut (Counters, Option<String>),
    batch: Batch,
}

impl Shade<'_, '_> {
    #[inline]
    fn quad(&mut self, prim: u32, at: (i32, i32), cov: [u32; 4]) {
        let b = &mut self.batch;
        b.quads[b.n] = at;
        b.prims[b.n] = prim;
        b.coverage[4 * b.n..4 * b.n + 4].copy_from_slice(&cov);
        b.n += 1;
        if b.n == L / 4 {
            self.flush();
        }
    }

    /// Shade and write the quads collected so far.
    fn flush(&mut self) {
        let Batch { n, quads, prims, coverage: mut cov } = std::mem::take(&mut self.batch);
        if n == 0 {
            return;
        }
        cov[4 * n..].fill(0);
        let (s, scene) = (self.s, self.scene);
        let prims: [&Prim; L / 4] = std::array::from_fn(|q| &scene.prims[prims[if q < n { q } else { 0 }] as usize]);
        let z_at = |l: usize, sample: usize| {
            let (qx, qy) = quads[l / 4];
            let (sx, sy) = if s.samples == 1 { (128, 128) } else { s.positions[sample] };
            let (x, y) = ((qx + (l as i32 & 1)) as f32 + sx as f32 / 256.0, (qy + (l as i32 >> 1 & 1)) as f32 + sy as f32 / 256.0);
            let z = prims[l / 4].z.at(x, y);
            if s.clamp_z { z.clamp(s.zrange.0, s.zrange.1) } else { z.clamp(0.0, 1.0) }
        };
        if s.early {
            depth_test(s, self.set, &prims, &quads, &mut cov, n, &z_at);
            if cov.iter().all(|&m| m == 0) {
                return;
            }
        }
        let Some(fs) = &s.fs else {
            if !s.early {
                depth_test(s, self.set, &prims, &quads, &mut cov, n, &z_at);
            }
            self.out.0.samples += cov.iter().map(|m| m.count_ones() as u64).sum::<u64>();
            return;
        };
        let out = &s.frag;
        for pass in 0..if s.per_sample { s.samples } else { 1 } {
            let sample = s.per_sample.then_some(pass);
            let mut live = cov;
            if let Some(k) = sample {
                live.iter_mut().for_each(|m| *m &= 1 << k);
            }
            let (mut helper, mut run) = (0u32, 0u32);
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
            self.out.0.fragments += (run & !helper).count_ones() as u64;
            let (lanes, mut common) = self.w.parts(self.work, self.mem);
            lanes.set_local_size((fs.sph.local_low + fs.sph.local_high + fs.sph.local_crs) as usize);
            lanes.start(&fs.program, run, helper);
            let mut host = FragmentHost::new(&mut common, s, scene, prims, quads, live, sample);
            let mut fuel = 4_000_000;
            let e = maxwell_shader::run(&fs.program, lanes, &mut host, &mut fuel);
            let stored = common.stored;
            if !matches!(e, Exit::Done) && self.out.1.is_none() {
                self.out.1 = Some(format!("fragment shader stopped: {e:?}"));
            }
            self.w.stored |= stored;
            let lanes = &self.w.lanes;
            let alive = lanes.live();
            for (l, m) in live.iter_mut().enumerate() {
                if alive >> l & 1 == 0 {
                    *m = 0;
                }
            }
            if let Some(r) = out.sample_mask {
                (0..4 * n).for_each(|l| live[l] &= lanes.gpr[r as usize][l]);
            }
            let alpha = |l: usize| out.color[0][3].map_or(0.0, |r| f32::from_bits(lanes.gpr[r as usize][l]));
            if let Some((func, reference)) = s.alpha_test {
                for (l, m) in live.iter_mut().enumerate().take(4 * n) {
                    if !compare(func, alpha(l), reference) {
                        *m = 0;
                    }
                }
            }
            if s.alpha_to_coverage {
                for (l, m) in live.iter_mut().enumerate().take(4 * n) {
                    let on = (alpha(l).clamp(0.0, 1.0) * s.samples as f32 + 0.5) as u32;
                    *m &= (1u32 << on).wrapping_sub(1);
                }
            }
            if !s.early {
                let gpr: Option<[u32; L]> = out.depth.map(|r| lanes.gpr[r as usize]);
                let z = |l: usize, k: usize| match &gpr {
                    Some(g) => f32::from_bits(g[l]).clamp(0.0, 1.0),
                    None => z_at(l, k),
                };
                depth_test(s, self.set, &prims, &quads, &mut live, n, &z);
            }
            self.out.0.samples += live.iter().map(|m| m.count_ones() as u64).sum::<u64>();
            for (rt, o) in &s.outputs {
                let regs = &out.color[if fs.sph.color0_broadcast { 0 } else { *rt }];
                let color: [[u32; L]; 4] = std::array::from_fn(|i| regs[i].map_or([if i == 3 { 1f32.to_bits() } else { 0 }; L], |r| lanes.gpr[r as usize]));
                for (l, &m) in live.iter().enumerate().take(4 * n) {
                    let mut mask = m;
                    if mask == 0 {
                        continue;
                    }
                    let (qx, qy) = quads[l / 4];
                    let (x, y) = (((qx + (l as i32 & 1)) % T) as u32, ((qy + (l as i32 >> 1 & 1)) % T) as u32);
                    let c = std::array::from_fn(|i| color[i][l]);
                    let src1 = if s.dual { std::array::from_fn(|i| out.color[1][i].map_or(0.0, |r| f32::from_bits(lanes.gpr[r as usize][l]))) } else { [0.0; 4] };
                    while mask != 0 {
                        let k = mask.trailing_zeros();
                        mask &= mask - 1;
                        o.write(self.set.color[*rt].sample_mut(x, y, k), c, src1, s.constant, s.logic);
                    }
                }
            }
        }
    }
}

/// Test the covered samples; those that fail are taken out of the coverage.
fn depth_test(s: &Setup, set: &mut TileSet, prims: &[&Prim; L / 4], quads: &[(i32, i32); L / 4], cov: &mut [u32; L], n: usize, z: &dyn Fn(usize, usize) -> f32) {
    let Some((_, format)) = s.targets.zeta.filter(|_| s.ds.used()) else { return };
    for (l, c) in cov.iter_mut().enumerate().take(4 * n) {
        let (qx, qy) = quads[l / 4];
        let (x, y) = (((qx + (l as i32 & 1)) % T) as u32, ((qy + (l as i32 >> 1 & 1)) % T) as u32);
        let mut m = *c;
        while m != 0 {
            let k = m.trailing_zeros();
            m &= m - 1;
            if !s.ds.test(&format, set.zeta.sample_mut(x, y, k), z(l, k as usize), !prims[l / 4].front) {
                *c &= !(1 << k);
            }
        }
    }
}
