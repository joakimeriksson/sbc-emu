//! The stages between vertices and the rasterizer: tessellation, geometry shaders and transform
//! feedback.
use crate::common_host;
use crate::pipe::{self, Common, Shader, Work, L};
use crate::raster::{Geometry, Verts};
use crate::threed::Threed;
use crate::Memory;
use maxwell_shader::isa::Attr;
use maxwell_shader::sph::Topology;
use maxwell_shader::{Exit, Host, Lanes, SurfOp, TexOp};
use std::sync::Arc;

const FUEL: u64 = 4_000_000;
const PRIMITIVE_ID: u32 = 0x60;

/// Words of a patch's constants.
const PATCH: usize = 256;
type Patch = [u32; PATCH];

/// A control shader run for one patch: each lane is an invocation.
struct Control<'a, 'm> {
    common: &'a mut Common<'m>,
    vertices: &'a Verts,
    inputs: &'a [u32],
    id: u32,
    layout: &'a Verts,
    out: &'a mut [u32],
    patch: &'a mut Patch,
}

impl Host<32> for Control<'_, '_> {
    common_host!(32);

    fn sysval(&mut self, sys: u8) -> [u32; 32] {
        match sys {
            0x10 => [(self.inputs.len() as u32) << 8; 32],
            0x11 => std::array::from_fn(|l| l as u32),
            0x1d => [(self.inputs.len() as u32) << 16; 32],
            _ => [0; 32],
        }
    }

    fn attr_load(&mut self, lane: usize, a: Attr) -> u32 {
        let _ = lane;
        if a.patch {
            return self.patch.get(a.addr as usize / 4).copied().unwrap_or(0);
        }
        if a.output {
            let at = a.vertex as usize * self.layout.stride;
            return pipe::word(a.addr).and_then(|w| self.layout.slot(w)).and_then(|s| self.out.get(at + s)).copied().unwrap_or(0);
        }
        match (a.addr, self.inputs.get(a.vertex as usize)) {
            (PRIMITIVE_ID, _) => self.id,
            (_, Some(&v)) => pipe::word(a.addr).map_or(0, |w| self.vertices.get(v, w)),
            _ => 0,
        }
    }

    fn attr_store(&mut self, lane: usize, a: Attr, v: u32) {
        if a.patch {
            if let Some(p) = self.patch.get_mut(a.addr as usize / 4) {
                *p = v;
            }
        } else if let Some(s) = pipe::word(a.addr).and_then(|w| self.layout.slot(w)) {
            if let Some(o) = self.out.get_mut(lane * self.layout.stride + s) {
                *o = v;
            }
        }
    }
}

/// An evaluation shader's run: lanes are points of one patch.
struct Evaluation<'a, 'm> {
    common: &'a mut Common<'m>,
    vertices: &'a Verts,
    inputs: &'a [u32],
    patch: &'a Patch,
    id: u32,
    points: &'a [[f32; 2]],
    layout: &'a Verts,
    out: &'a mut [u32],
}

impl Host<L> for Evaluation<'_, '_> {
    common_host!(L);

    fn sysval(&mut self, sys: u8) -> [u32; L] {
        match sys {
            0x10 => [(self.inputs.len() as u32) << 8; L],
            0x1d => [(self.inputs.len() as u32) << 16; L],
            _ => [0; L],
        }
    }

    fn attr_load(&mut self, lane: usize, a: Attr) -> u32 {
        match a.addr {
            0x2f0 | 0x2f4 => self.points.get(lane).map_or(0, |p| p[(a.addr >> 2 & 1) as usize].to_bits()),
            0x2f8 | 0x2fc => 0,
            PRIMITIVE_ID if !a.patch => self.id,
            _ if a.patch => self.patch.get(a.addr as usize / 4).copied().unwrap_or(0),
            _ => match self.inputs.get(a.vertex as usize) {
                Some(&v) => pipe::word(a.addr).map_or(0, |w| self.vertices.get(v, w)),
                None => 0,
            },
        }
    }

    fn attr_store(&mut self, lane: usize, a: Attr, v: u32) {
        if let Some(s) = pipe::word(a.addr).and_then(|w| self.layout.slot(w)) {
            if let Some(o) = self.out.get_mut(lane * self.layout.stride + s) {
                *o = v;
            }
        }
    }
}

/// Run the tessellation stages on patches of `size` vertices each.
pub fn tessellate(t: &mut Threed, vertices: &Verts, list: &[u32], size: usize, control: Option<&Arc<Shader>>, eval: &Arc<Shader>, mem: &mut dyn Memory) -> Geometry {
    let mode = t.r(0x320);
    let mut out = Geometry { vertices: Verts::new(eval.sph.outputs().filter_map(pipe::word)), primitives: Vec::new(), size: 0, ids: Some(Vec::new()) };
    let (control_work, eval_work) = (pipe::work(t, 1), pipe::work(t, 2));
    t.pipe.need_workers(1);
    let mut wide: Option<Box<Lanes<32>>> = None;
    let mut failed = None;
    let levels: [u32; 6] = std::array::from_fn(|k| t.r(0x324 + 4 * k as u32));
    let (mut points, mut triangles) = (Vec::new(), Vec::new());
    for (id, inputs) in list.chunks_exact(size.max(1)).enumerate() {
        let mut patch: Patch = [0; PATCH];
        patch[..6].copy_from_slice(&levels);
        // the control points: the shader's, or the vertices as they are
        let own;
        let control_points: Vec<u32>;
        let (points_of, indices): (&Verts, &[u32]) = match control {
            None => (vertices, inputs),
            Some(c) => {
                t.counters.control += 1;
                let n = (c.sph.threads_per_primitive as usize).clamp(1, 32);
                let mut v = Verts::new(c.sph.outputs().filter_map(pipe::word));
                v.grow(n);
                let mut data = std::mem::take(&mut v.data);
                let w = &mut t.pipe.workers[0];
                let lanes = wide.get_or_insert_with(|| Box::new(Lanes::new()));
                let (_, mut common) = w.parts(&control_work, mem);
                lanes.set_local_size(c.local_size());
                lanes.start(&c.program, (1u64 << n).wrapping_sub(1) as u32, 0);
                let mut host = Control { common: &mut common, vertices, inputs, id: id as u32, layout: &v, out: &mut data, patch: &mut patch };
                let mut fuel = FUEL;
                let e = maxwell_shader::run(&c.program, lanes, &mut host, &mut fuel);
                let stored = common.stored;
                w.stored |= stored;
                if !matches!(e, Exit::Done) {
                    failed = Some(format!("tessellation control shader stopped: {e:?}"));
                }
                v.data = data;
                own = v;
                control_points = (0..n as u32).collect();
                (&own, &control_points)
            }
        };
        let level = |k: usize| f32::from_bits(patch[k]);
        points.clear();
        triangles.clear();
        let corners = domain(mode, std::array::from_fn(level), [level(4), level(5)], &mut points, &mut triangles);
        if points.is_empty() {
            continue;
        }
        let first = out.vertices.len() as u32;
        out.vertices.grow(points.len());
        let stride = out.vertices.stride;
        let mut data = std::mem::take(&mut out.vertices.data);
        let w = &mut t.pipe.workers[0];
        let (lanes, mut common) = w.parts(&eval_work, mem);
        lanes.set_local_size(eval.local_size());
        for (chunk, outs) in points.chunks(L).zip(data[first as usize * stride..].chunks_mut(L * stride)) {
            lanes.start(&eval.program, (1u32 << chunk.len()).wrapping_sub(1), 0);
            let mut host = Evaluation { common: &mut common, vertices: points_of, inputs: indices, patch: &patch, id: id as u32, points: chunk, layout: &out.vertices, out: outs };
            let mut fuel = FUEL;
            let e = maxwell_shader::run(&eval.program, lanes, &mut host, &mut fuel);
            if !matches!(e, Exit::Done) {
                failed = Some(format!("tessellation evaluation shader stopped: {e:?}"));
            }
        }
        let stored = common.stored;
        w.stored |= stored;
        t.counters.evaluations += points.len() as u64;
        out.vertices.data = data;
        out.size = corners;
        out.primitives.extend(triangles.iter().map(|&k| first + k));
        out.ids.as_mut().unwrap().extend(std::iter::repeat_n(id as u32, triangles.len() / corners));
    }
    out.size = out.size.max(1);
    if let Some(e) = failed {
        t.pipe.fail(e);
    }
    out
}

/// Where an edge of level `f` is divided, as positions from 0 to 1.
fn divide(f: f32, spacing: u32, out: &mut Vec<f32>) {
    out.clear();
    let f = if f.is_nan() { 1.0 } else { f };
    let (f, n) = match spacing {
        1 => {
            let f = f.clamp(1.0, 63.0);
            let n = f.ceil() as u32;
            (f, if n.is_multiple_of(2) { n + 1 } else { n })
        }
        2 => {
            let f = f.clamp(2.0, 64.0);
            let n = f.ceil() as u32;
            (f, if n % 2 == 1 { n + 1 } else { n })
        }
        _ => {
            let n = f.clamp(1.0, 64.0).ceil();
            (n, n as u32)
        }
    };
    if n < 3 || f == n as f32 {
        out.extend((0..=n).map(|k| k as f32 / n as f32));
        return;
    }
    // n - 2 segments of 1/f, and two shorter ones: the first and the last
    let short = (1.0 - (n - 2) as f32 / f) * 0.5;
    out.push(0.0);
    out.extend((0..n - 1).map(|k| short + k as f32 / f));
    out.push(1.0);
    // (the middle exactly)
    if n % 2 == 0 {
        out[n as usize / 2] = 0.5;
    }
}

/// Generate a patch's points (u, v) and primitives for `mode`; returns the vertices of each.
fn domain(mode: u32, outer: [f32; 4], inner: [f32; 2], points: &mut Vec<[f32; 2]>, prims: &mut Vec<u32>) -> usize {
    let spacing = mode >> 4 & 3;
    let kind = mode & 3;
    // a level of zero or less (or NaN) drops the patch
    let used = [2, 3, 4, 4][kind as usize];
    if outer[..used].iter().any(|&l| l.is_nan() || l <= 0.0) {
        return 1;
    }
    if kind == 0 {
        let (mut lines, mut along) = (Vec::new(), Vec::new());
        divide(outer[0], 0, &mut lines);
        divide(outer[1], spacing, &mut along);
        let n = along.len() as u32;
        for (k, &v) in lines[..lines.len() - 1].iter().enumerate() {
            points.extend(along.iter().map(|&u| [u, v]));
            if mode & 0x100 != 0 {
                prims.extend((0..n - 1).flat_map(|i| [k as u32 * n + i, k as u32 * n + i + 1]));
            }
        }
        if mode & 0x100 == 0 {
            prims.extend(0..points.len() as u32);
            return 1;
        }
        return 2;
    }
    let mut b = Builder { points, prims, mode };
    if kind == 1 {
        b.triangle([outer[0], outer[1], outer[2]], inner[0], spacing);
    } else {
        b.quad(outer, inner, spacing);
    }
    if mode & 0x200 == 0 {
        // points: each one once
        b.prims.clear();
        let n = b.points.len() as u32;
        b.prims.extend(0..n);
        return 1;
    }
    3
}

struct Builder<'a> {
    points: &'a mut Vec<[f32; 2]>,
    prims: &'a mut Vec<u32>,
    mode: u32,
}

impl Builder<'_> {
    /// A triangle given counter-clockwise in (u, v).
    fn emit(&mut self, a: u32, b: u32, c: u32) {
        if self.mode & 0x100 != 0 {
            self.prims.extend([a, c, b]);
        } else {
            self.prims.extend([a, b, c]);
        }
    }

    fn point(&mut self, u: f32, v: f32) -> u32 {
        self.points.push([u, v]);
        self.points.len() as u32 - 1
    }

    /// Fill between an outer and an inner line of points; the inside is to the left of the outer
    /// line's direction.
    fn stitch(&mut self, outer: &[u32], inner: &[u32]) {
        let (n, m) = (outer.len() - 1, inner.len() - 1);
        let (mut i, mut j) = (0, 0);
        while i < n || j < m {
            // advance the line that is behind
            if j == m || i < n && (2 * i + 1) * m <= (2 * j + 1) * n {
                self.emit(outer[i], outer[i + 1], inner[j]);
                i += 1;
            } else {
                self.emit(outer[i], inner[j + 1], inner[j]);
                j += 1;
            }
        }
    }

    fn triangle(&mut self, outer: [f32; 3], inner: f32, spacing: u32) {
        let mut edges: [Vec<f32>; 3] = Default::default();
        for k in 0..3 {
            divide(outer[k], spacing, &mut edges[k]);
        }
        let mut t = Vec::new();
        divide(inner, spacing, &mut t);
        if t.len() == 2 {
            if edges.iter().all(|e| e.len() == 2) {
                let (a, b, c) = (self.point(1.0, 0.0), self.point(0.0, 1.0), self.point(0.0, 0.0));
                self.emit(a, b, c);
                return;
            }
            divide(1.0 + f32::EPSILON, spacing, &mut t);
        }
        let n = t.len() - 1;
        // corners by barycentric coordinates: u = 1, v = 1, w = 1. Edge k runs from corner k to the
        // next; the levels belong to the edges u = 0 (from v to w), v = 0 (from w to u) and w = 0
        let corner = |k: usize, d: f32| -> [f32; 3] { std::array::from_fn(|i| if i == k { 1.0 - 2.0 * d } else { d }) };
        let levels = [2, 0, 1];
        // a ring's points, edge by edge (an edge ends where the next begins)
        let mut ring: Vec<Vec<u32>> = Vec::new();
        for (k, &l) in levels.iter().enumerate() {
            let (a, b) = (corner(k, 0.0), corner((k + 1) % 3, 0.0));
            let e = edges[l].clone();
            ring.push(e[..e.len() - 1].iter().map(|&s| self.point(a[0] + (b[0] - a[0]) * s, a[1] + (b[1] - a[1]) * s)).collect());
        }
        close(&mut ring);
        for r in 1..=n / 2 {
            let d = t[r] * 2.0 / 3.0;
            let segments = n - 2 * r;
            let mut next: Vec<Vec<u32>> = Vec::new();
            if segments == 0 {
                let c = self.point(1.0 / 3.0, 1.0 / 3.0);
                next = vec![vec![c]; 3];
            } else {
                for k in 0..3 {
                    let (a, b) = (corner(k, d), corner((k + 1) % 3, d));
                    next.push(
                        (r..n - r)
                            .map(|i| {
                                let s = (t[i] - t[r]) / (1.0 - 2.0 * t[r]);
                                self.point(a[0] + (b[0] - a[0]) * s, a[1] + (b[1] - a[1]) * s)
                            })
                            .collect(),
                    );
                }
                close(&mut next);
            }
            for k in 0..3 {
                self.stitch(&ring[k], &next[k]);
            }
            if segments == 1 {
                let (a, b, c) = (next[0][0], next[1][0], next[2][0]);
                self.emit(a, b, c);
            }
            ring = next;
        }
    }

    fn quad(&mut self, outer: [f32; 4], inner: [f32; 2], spacing: u32) {
        let mut edges: [Vec<f32>; 4] = Default::default();
        for k in 0..4 {
            divide(outer[k], spacing, &mut edges[k]);
        }
        let (mut s, mut t) = (Vec::new(), Vec::new());
        divide(inner[0], spacing, &mut s);
        divide(inner[1], spacing, &mut t);
        if s.len() == 2 && t.len() == 2 && edges.iter().all(|e| e.len() == 2) {
            let p = [self.point(0.0, 0.0), self.point(1.0, 0.0), self.point(1.0, 1.0), self.point(0.0, 1.0)];
            self.emit(p[0], p[1], p[2]);
            self.emit(p[0], p[2], p[3]);
            return;
        }
        for d in [&mut s, &mut t] {
            if d.len() == 2 {
                divide(1.0 + f32::EPSILON, spacing, d);
            }
        }
        let (n, m) = (s.len() - 1, t.len() - 1);
        // the inner grid
        let grid: Vec<Vec<u32>> = (1..m).map(|j| (1..n).map(|i| self.point(s[i], t[j])).collect()).collect();
        for j in 0..m.saturating_sub(2) {
            for i in 0..n.saturating_sub(2) {
                let (a, b, c, d) = (grid[j][i], grid[j][i + 1], grid[j + 1][i + 1], grid[j + 1][i]);
                self.emit(a, b, c);
                self.emit(a, c, d);
            }
        }
        // the border, counter-clockwise: v = 0, u = 1, v = 1, u = 0
        let mut ring: Vec<Vec<u32>> = Vec::new();
        for (k, l) in [1, 2, 3, 0].into_iter().enumerate() {
            let e = edges[l].clone();
            ring.push(
                e[..e.len() - 1]
                    .iter()
                    .map(|&x| match k {
                        0 => self.point(x, 0.0),
                        1 => self.point(1.0, x),
                        2 => self.point(1.0 - x, 1.0),
                        _ => self.point(0.0, 1.0 - x),
                    })
                    .collect(),
            );
        }
        close(&mut ring);
        let (w, h) = (n - 1, m - 1);
        let sides: [Vec<u32>; 4] = [
            (0..w).map(|i| grid[0][i]).collect(),
            (0..h).map(|j| grid[j][w - 1]).collect(),
            (0..w).rev().map(|i| grid[h - 1][i]).collect(),
            (0..h).rev().map(|j| grid[j][0]).collect(),
        ];
        for k in 0..4 {
            self.stitch(&ring[k], &sides[k]);
        }
    }
}

/// End each of a ring's lines at the start of the next.
fn close(ring: &mut [Vec<u32>]) {
    let starts: Vec<u32> = ring.iter().map(|r| r[0]).collect();
    let n = ring.len();
    for (k, r) in ring.iter_mut().enumerate() {
        r.push(starts[(k + 1) % n]);
    }
}

/// A geometry shader's run: each lane is an invocation for a primitive.
struct Geometric<'a, 'm> {
    common: &'a mut Common<'m>,
    vertices: &'a Verts,
    /// each lane's primitive: its vertices, its number, the invocation
    inputs: [&'a [u32]; L],
    ids: [u32; L],
    invocations: [u32; L],
    per: u32,
    layout: &'a Verts,
    current: &'a mut [Vec<u32>],
    /// emitted vertices, and where strips end
    out: &'a mut [Vec<u32>],
    cuts: &'a mut [Vec<u32>],
    max: usize,
}

impl Host<L> for Geometric<'_, '_> {
    common_host!(L);

    fn sysval(&mut self, sys: u8) -> [u32; L] {
        match sys {
            0x10 => [self.per << 8; L],
            0x11 => self.invocations,
            0x1d => [self.per << 16; L],
            _ => [0; L],
        }
    }

    fn attr_load(&mut self, lane: usize, a: Attr) -> u32 {
        if a.addr == PRIMITIVE_ID {
            return self.ids[lane];
        }
        match self.inputs[lane].get(a.vertex as usize) {
            Some(&v) => pipe::word(a.addr).map_or(0, |w| self.vertices.get(v, w)),
            None => 0,
        }
    }

    fn attr_store(&mut self, lane: usize, a: Attr, v: u32) {
        if let Some(s) = pipe::word(a.addr).and_then(|w| self.layout.slot(w)) {
            self.current[lane][s] = v;
        }
    }

    fn emit(&mut self, lane: usize, handle: u32, stream: u32, emit: bool, cut: bool) -> u32 {
        if stream != 0 {
            return handle;
        }
        let stride = self.layout.stride;
        if emit && self.out[lane].len() < self.max * stride {
            let v = &self.current[lane];
            self.out[lane].extend_from_slice(v);
        }
        if cut {
            self.cuts[lane].push((self.out[lane].len() / stride) as u32);
        }
        handle
    }
}

/// Run a geometry shader on primitives.
pub fn geometry(t: &mut Threed, g: &Geometry, shader: &Arc<Shader>, mem: &mut dyn Memory) -> Geometry {
    let invocations = shader.sph.threads_per_primitive.max(1);
    let work: Work = pipe::work(t, 3);
    let words: Vec<usize> = shader.sph.outputs().filter_map(pipe::word).collect();
    let writes_id = shader.sph.writes(PRIMITIVE_ID);
    let mut out = Geometry { vertices: Verts::new(words.into_iter()), primitives: Vec::new(), size: 1, ids: Some(Vec::new()) };
    let stride = out.vertices.stride;
    let topology = shader.sph.topology.unwrap_or(Topology::TriangleStrip);
    out.size = match topology {
        Topology::Points => 1,
        Topology::LineStrip => 2,
        Topology::TriangleStrip => 3,
    };
    let jobs: Vec<(usize, u32)> = (0..g.primitives.len() / g.size).flat_map(|p| (0..invocations).map(move |i| (p, i))).collect();
    let first = t.r(0x1684) & 1 == 0;
    t.pipe.need_workers(1);
    t.counters.geometry += jobs.len() as u64;
    let mut failed = None;
    let w = &mut t.pipe.workers[0];
    let (lanes, mut common) = w.parts(&work, mem);
    lanes.set_local_size(shader.local_size());
    let mut blank = Verts::new(std::iter::empty());
    blank.grow(1);
    let mut current: Vec<Vec<u32>> = vec![vec![0; stride]; L];
    let mut outs: Vec<Vec<u32>> = vec![Vec::new(); L];
    let mut cuts: Vec<Vec<u32>> = vec![Vec::new(); L];
    for chunk in jobs.chunks(L) {
        let of = |l: usize| &g.primitives[chunk.get(l).map_or(0, |j| j.0) * g.size..][..g.size];
        let mut host = Geometric {
            common: &mut common,
            vertices: &g.vertices,
            inputs: std::array::from_fn(of),
            ids: std::array::from_fn(|l| chunk.get(l).map_or(0, |j| g.ids.as_ref().map_or(j.0 as u32, |i| i[j.0]))),
            invocations: std::array::from_fn(|l| chunk.get(l).map_or(0, |j| j.1)),
            per: g.size as u32,
            layout: &out.vertices,
            current: &mut current,
            out: &mut outs,
            cuts: &mut cuts,
            max: shader.sph.max_vertices as usize,
        };
        lanes.start(&shader.program, (1u32 << chunk.len()).wrapping_sub(1), 0);
        let mut fuel = FUEL;
        let e = maxwell_shader::run(&shader.program, lanes, &mut host, &mut fuel);
        if !matches!(e, Exit::Done) {
            failed = Some(format!("geometry shader stopped: {e:?}"));
        }
        for (l, job) in chunk.iter().enumerate() {
            let base = out.vertices.len() as u32;
            let n = (outs[l].len() / stride) as u32;
            out.vertices.data.append(&mut outs[l]);
            cuts[l].push(n);
            let mut start = 0;
            for end in cuts[l].drain(..) {
                let op = match topology {
                    Topology::Points => 0,
                    Topology::LineStrip => 3,
                    Topology::TriangleStrip => 5,
                };
                let before = out.primitives.len();
                crate::vertex::assemble(op, end.saturating_sub(start) as usize, first, |p| out.primitives.extend(p.iter().map(|&k| base + start + k)));
                let made = (out.primitives.len() - before) / out.size;
                for k in 0..made {
                    let v = out.primitives[before + k * out.size];
                    let id = if writes_id { out.vertices.get(v, pipe::word(PRIMITIVE_ID).unwrap()) } else { g.ids.as_ref().map_or(job.0 as u32, |i| i[job.0]) };
                    out.ids.as_mut().unwrap().push(id);
                }
                start = start.max(end);
            }
        }
    }
    let stored = common.stored;
    w.stored |= stored;
    t.counters.geometry_out += (out.primitives.len() / out.size) as u64;
    if let Some(e) = failed {
        t.pipe.fail(e);
    }
    out
}

/// Write primitives' vertices to the transform feedback buffers.
pub fn feedback(t: &mut Threed, g: &Geometry, mem: &mut dyn Memory) {
    struct Buffer {
        at: u64,
        size: u32,
        offset: u32,
        stride: u32,
        words: Vec<Option<usize>>,
    }
    let mut buffers: Vec<Option<Buffer>> = Vec::new();
    for b in 0..4u32 {
        let base = 0x380 + 0x20 * b;
        let count = t.r(0x704 + 0x10 * b);
        // (only the first vertex stream)
        let on = t.r(base) & 1 != 0 && count != 0 && t.r(0x700 + 0x10 * b) == 0;
        buffers.push(on.then(|| Buffer {
            at: t.address(base + 4),
            size: t.r(base + 0xc),
            offset: t.r(base + 0x10),
            stride: t.r(0x708 + 0x10 * b),
            words: (0..count.min(128))
                .map(|k| {
                    let a = t.r(0x2800 + 0x80 * b + (k & !3)) >> (8 * (k & 3)) & 0xff;
                    pipe::word(4 * a)
                })
                .collect(),
        }));
    }
    let mut line = Vec::new();
    for p in g.primitives.chunks_exact(g.size) {
        t.counters.feedback_needed += 1;
        // all of a primitive, in every buffer, or nothing
        let room = buffers.iter().flatten().all(|b| b.offset as u64 + b.stride as u64 * g.size as u64 <= b.size as u64);
        if !room || buffers.iter().all(|b| b.is_none()) {
            continue;
        }
        t.counters.feedback += 1;
        for &v in p {
            for b in buffers.iter_mut().flatten() {
                let start = b.at + b.offset as u64;
                line.clear();
                for w in &b.words {
                    line.extend_from_slice(&w.map_or(0, |w| g.vertices.get(v, w)).to_le_bytes());
                }
                // (words that are skipped keep what is there)
                let mut k = 0;
                while k < b.words.len() {
                    let n = b.words[k..].iter().take_while(|w| w.is_some() == b.words[k].is_some()).count();
                    if b.words[k].is_some() {
                        mem.write(start + 4 * k as u64, &line[4 * k..4 * (k + n)]);
                    }
                    k += n;
                }
                b.offset += b.stride;
            }
        }
    }
    for (k, b) in buffers.iter().enumerate() {
        if let Some(b) = b {
            t.regs[(0x390 + 0x20 * k) / 4] = b.offset;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn area(p: &[[f32; 2]], t: &[u32]) -> Vec<f32> {
        t.chunks_exact(3)
            .map(|t| {
                let [a, b, c] = [t[0], t[1], t[2]].map(|k| p[k as usize]);
                ((b[0] - a[0]) * (c[1] - a[1]) - (c[0] - a[0]) * (b[1] - a[1])) / 2.0
            })
            .collect()
    }

    #[test]
    fn domains_are_covered_once() {
        for spacing in 0..3 {
            for outer in [[1.0, 1.0, 1.0, 1.0], [1.0, 4.0, 3.0, 7.0], [5.5, 2.2, 8.0, 1.0], [64.0, 1.0, 2.0, 63.5]] {
                for inner in [[1.0, 1.0], [2.0, 5.0], [3.3, 1.0], [8.0, 7.5]] {
                    for (kind, whole) in [(1, 0.5), (2, 1.0)] {
                        let (mut p, mut t) = (Vec::new(), Vec::new());
                        assert_eq!(domain(kind | spacing << 4 | 0x200, outer, inner, &mut p, &mut t), 3);
                        let a = area(&p, &t);
                        let case = format!("{kind} {spacing} {outer:?} {inner:?}");
                        assert!(a.iter().all(|&x| x > -1e-6), "{case}: all counter-clockwise");
                        assert!((a.iter().sum::<f32>() - whole).abs() < 1e-3, "{case}: {}", a.iter().sum::<f32>());
                        assert!(p.iter().all(|p| p.iter().all(|&x| (-1e-6..=1.0 + 1e-6).contains(&x))), "{case}");
                        // clockwise
                        t.clear();
                        p.clear();
                        domain(kind | spacing << 4 | 0x300, outer, inner, &mut p, &mut t);
                        assert!(area(&p, &t).iter().all(|&x| x < 1e-6), "{case}");
                    }
                }
            }
        }
    }

    #[test]
    fn levels_and_special_cases() {
        let (mut p, mut t) = (Vec::new(), Vec::new());
        domain(0x201, [1.0; 4], [1.0; 2], &mut p, &mut t);
        assert_eq!((p.len(), t.len()), (3, 3));
        (p, t) = Default::default();
        assert_eq!(domain(0x201, [1.0, 0.0, 1.0, 1.0], [1.0; 2], &mut p, &mut t), 1);
        assert!(p.is_empty(), "a level of zero drops the patch");
        // three lines of four segments
        assert_eq!(domain(0x100, [3.0, 4.0, 0.0, 0.0], [0.0; 2], &mut p, &mut t), 2);
        assert_eq!((p.len(), t.len()), (15, 24));
        (p, t) = Default::default();
        // equal spacing: an n-level triangle with equal levels
        domain(0x201, [3.0; 4], [3.0; 2], &mut p, &mut t);
        assert_eq!((p.len(), t.len() / 3), (12, 13));
        let mut d = Vec::new();
        divide(4.5, 1, &mut d);
        assert_eq!(d.len(), 6);
        assert!(d.windows(2).all(|w| w[1] > w[0]) && (0..6).all(|k| (d[k] + d[5 - k] - 1.0).abs() < 1e-6));
    }
}
