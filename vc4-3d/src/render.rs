//! The rendering thread (CT1): the rendering control list the kernel builds walks the tiles —
//! loads into the tile buffer, a branch into the tile's list from the binner, stores out — and
//! the primitives in each tile list are set up, rasterized within the tile and shaded.
//!
//! For each primitive the vertex shader runs on its vertices (once per job here), giving
//! screen positions (12.4 fixed point), Zs, 1/Wc and the varyings. Triangles are rasterized
//! with the top-left rule at pixel centres (or at four sample positions in 4x multisample
//! mode), lines and points as quads. Fragments go to the fragment shader four 2x2 quads at a
//! time; the shader reads the varyings (perspective-correct: the varying unit gives
//! (v - C) / W-interpolated parts, the shader multiplies by W and adds C from r5), and writes Z,
//! stencil setup and colour to the tile buffer (TLB), which does the depth and stencil tests.
//! There is no fixed-function blending on this GPU: shaders read the tile colour and blend.

use crate::cl::{self, GlState, ShaderRec};
use crate::mem::Mem;
use crate::qpu::{fo, Code, Env, Qpu, Stop, V, ZERO};
use crate::shade;
use crate::tlb::{Frame, TileBuffer};

pub const FS_BUDGET: u64 = 1 << 20;

#[derive(Clone, Copy, Debug, Default)]
pub struct RenderOut {
    pub eof: bool,
    pub error: bool,
    pub fragments: u64,
    pub prims: u64,
}

/// A set-up vertex: absolute screen position in 1/16 pixel, Zs, 1/Wc, and two screen-linear
/// extras (point sprite or line coordinates).
#[derive(Clone, Copy, Debug, Default)]
pub struct SVert {
    pub x: i32,
    pub y: i32,
    pub z: f32,
    pub iw: f32,
    pub aux: [f32; 2],
}

/// What the rasterizer needs about the primitive being drawn.
pub struct Prim<'a> {
    pub v: [SVert; 3],
    /// the vertices' varyings (vertex shader outputs)
    pub vary: [&'a [f32]; 3],
    /// the provoking vertex's varyings (flat shading)
    pub flat_src: &'a [f32],
    /// hardware varyings before the vertex ones: 2 for points, 1 for lines
    pub hw: u32,
    pub cull: bool,
}

/// Four-sample positions in 1/16 pixel (rotated grid).
const SAMPLES4: [(i32, i32); 4] = [(6, 2), (14, 6), (2, 10), (10, 14)];

#[derive(Clone, Copy, Default)]
struct Lane {
    x: u32,
    y: u32,
    cover: u8,
    l: [f32; 3],
    w: f32,
    z24: u32,
}

#[derive(Clone, Copy, Default, Debug)]
struct StencilFace { func: u32, refv: u8, vmask: u8, fail: u32, zfail: u32, zpass: u32, wmask: u8 }

impl StencilFace {
    fn parse(w: u32) -> StencilFace {
        StencilFace { func: w >> 16 & 7, refv: (w >> 8) as u8, vmask: w as u8, fail: w >> 19 & 7, zfail: w >> 25 & 7, zpass: w >> 22 & 7,
                      wmask: [0x1, 0x3, 0xf, 0xff][(w >> 28 & 3) as usize] }
    }
}

pub fn compare(func: u32, a: u32, b: u32) -> bool {
    match func { 0 => false, 1 => a < b, 2 => a == b, 3 => a <= b, 4 => a > b, 5 => a != b, 6 => a >= b, _ => true }
}

fn stencil_op(op: u32, s: u8, r: u8) -> u8 {
    match op { 0 => 0, 1 => s, 2 => r, 3 => s.saturating_add(1), 4 => s.saturating_sub(1), 5 => !s, 6 => s.wrapping_add(1), _ => s.wrapping_sub(1) }
}

/// The fragment shader's view of the tile buffer and the primitive.
struct FragEnv<'a> {
    tb: &'a mut TileBuffer,
    origin: (u32, u32),
    lanes: [Lane; 16],
    active: u16,
    prim: &'a Prim<'a>,
    nvary: u32,
    flat: u32,
    next_vary: u32,
    rev: bool,
    config: u32,
    stencil: [Option<StencilFace>; 2],
    /// per lane: samples that passed the Z/stencil test (once a Z write has happened)
    zpass: Option<[u8; 16]>,
    ms_writes: u32,
    color_loads: u32,
    pub fragments: u64,
}

impl FragEnv<'_> {
    /// The depth and stencil test of lane `i` with fragment depth `fz`, updating the buffers;
    /// returns the samples that pass.
    fn ztest(&mut self, i: usize, fz: u32) -> u8 {
        let ln = self.lanes[i];
        let face = self.stencil[self.rev as usize];
        let func = cl::config::depth_func(self.config);
        let zupd = self.config & cl::config::Z_UPDATE != 0;
        let mut pass = 0u8;
        for s in 0..self.tb.samples {
            if ln.cover >> s & 1 == 0 { continue; }
            let k = self.tb.idx(ln.x, ln.y, s);
            let zs = self.tb.zs[k];
            let (bz, bs) = (zs >> 8, zs as u8);
            let mut ns = bs;
            let ok;
            if let Some(f) = face {
                if !compare(f.func, (f.refv & f.vmask) as u32, (bs & f.vmask) as u32) {
                    ns = stencil_op(f.fail, bs, f.refv);
                    ok = false;
                } else if !compare(func, fz, bz) {
                    ns = stencil_op(f.zfail, bs, f.refv);
                    ok = false;
                } else {
                    ns = stencil_op(f.zpass, bs, f.refv);
                    ok = true;
                }
                ns = (bs & !f.wmask) | (ns & f.wmask);
            } else {
                ok = compare(func, fz, bz);
            }
            let nz = if ok && zupd { fz } else { bz };
            self.tb.zs[k] = nz << 8 | ns as u32;
            if ok { pass |= 1 << s; }
        }
        pass
    }
}

impl Env for FragEnv<'_> {
    fn fragment(&self) -> bool { true }

    fn vary(&mut self) -> (V, V) {
        let k = self.next_vary;
        self.next_vary += 1;
        let (mut v, mut c) = (ZERO, ZERO);
        if k >= self.nvary { crate::log_first("vc4: fragment shader reads more varyings than the shader record has"); }
        let p = self.prim;
        for i in 0..16 {
            if self.active >> i & 1 == 0 { continue; }
            let ln = &self.lanes[i];
            if k < p.hw {
                // point or line coordinate: linear in screen space
                let val = ln.l[0] * p.v[0].aux[k as usize] + ln.l[1] * p.v[1].aux[k as usize] + ln.l[2] * p.v[2].aux[k as usize];
                v[i] = fo(val / ln.w);
                continue;
            }
            let vk = (k - p.hw) as usize;
            if self.flat >> k & 1 != 0 {
                c[i] = fo(p.flat_src.get(vk).copied().unwrap_or(0.0));
                continue;
            }
            let get = |j: usize| p.vary[j].get(vk).copied().unwrap_or(0.0);
            let cc = get(2);
            let s = ln.l[0] * (get(0) - cc) * p.v[0].iw + ln.l[1] * (get(1) - cc) * p.v[1].iw + ln.l[2] * 0.0;
            v[i] = fo(s);
            c[i] = fo(cc);
        }
        (v, c)
    }

    fn pixel_xy(&self) -> (V, V) {
        (std::array::from_fn(|i| self.origin.0 + self.lanes[i].x), std::array::from_fn(|i| self.origin.1 + self.lanes[i].y))
    }
    fn ms_flags(&self) -> V { std::array::from_fn(|i| self.lanes[i].cover as u32) }
    fn set_ms_flags(&mut self, v: &V, mask: u16) {
        for i in 0..16 { if mask >> i & 1 != 0 { self.lanes[i].cover = v[i] as u8 & 0xf; } }
    }
    fn rev_flag(&self) -> V { [self.rev as u32; 16] }

    fn tlb_stencil(&mut self, w: u32) {
        match w >> 30 {
            0 => {
                for (k, f) in self.stencil.iter_mut().enumerate() { if let Some(f) = f { f.wmask = (w >> (8 * k)) as u8; } }
            }
            sel => {
                let f = StencilFace::parse(w);
                if sel & 1 != 0 { self.stencil[0] = Some(f); }
                if sel & 2 != 0 { self.stencil[1] = Some(f); }
            }
        }
    }

    fn tlb_z(&mut self, v: &V, mask: u16) {
        let mut zp = self.zpass.unwrap_or([0; 16]);
        for i in 0..16 {
            if (self.active & mask) >> i & 1 == 0 { continue; }
            zp[i] = self.ztest(i, v[i] & 0xff_ffff);
        }
        self.zpass = Some(zp);
    }

    fn tlb_color(&mut self, v: &V, mask: u16, all: bool) {
        let m = self.active & mask;
        if self.zpass.is_none() {
            // no Z written: test with the interpolated depth
            let mut zp = [0u8; 16];
            for i in 0..16 { if self.active >> i & 1 != 0 { zp[i] = self.ztest(i, self.lanes[i].z24); } }
            self.zpass = Some(zp);
        }
        let zp = self.zpass.unwrap();
        let sample = self.ms_writes;
        if !all { self.ms_writes += 1; }
        for i in 0..16 {
            if m >> i & 1 == 0 { continue; }
            let ln = self.lanes[i];
            for s in 0..self.tb.samples {
                if zp[i] >> s & 1 == 0 || (!all && s != sample) { continue; }
                let k = self.tb.idx(ln.x, ln.y, s);
                self.tb.color[k] = v[i];
            }
            self.fragments += 1;
        }
    }

    fn color_load(&mut self) -> V {
        let s = self.color_loads % self.tb.samples;
        self.color_loads += 1;
        std::array::from_fn(|i| { let ln = self.lanes[i]; self.tb.color[self.tb.idx(ln.x, ln.y, s)] })
    }
    fn coverage_load(&mut self) -> V { self.ms_flags() }
    fn alpha_mask_load(&mut self) -> V {
        std::array::from_fn(|i| { let ln = self.lanes[i]; self.tb.vg[self.tb.idx(ln.x, ln.y, 0)] as u32 })
    }
}

/// The rendering state of a job.
pub struct Render<'a, 'm> {
    pub tb: &'a mut TileBuffer,
    pub mem: &'a mut Mem<'m>,
    pub code: &'a mut Code,
    pub insns: &'a mut u64,
    pub frame: Frame,
    pub tile: (u32, u32),
    pub st: GlState,
    rec: Option<ShaderRec>,
    cache: shade::Cache,
    pub out: RenderOut,
}

impl<'a, 'm> Render<'a, 'm> {
    pub fn new(tb: &'a mut TileBuffer, mem: &'a mut Mem<'m>, code: &'a mut Code, insns: &'a mut u64) -> Self {
        Render { tb, mem, code, insns, frame: Frame::default(), tile: (0, 0), st: GlState::default(), rec: None, cache: shade::Cache::default(), out: RenderOut::default() }
    }

    fn shader(&mut self) -> Option<ShaderRec> {
        if self.st.shader.0 == 0 { crate::log_first("vc4: primitive in a tile list before any shader state"); return None; }
        if self.rec.as_ref().map(|r| r.addr) != Some(self.st.shader.1 & !0xf) {
            self.rec = Some(ShaderRec::read(self.st.shader.0, self.st.shader.1, self.mem));
        }
        self.rec.clone()
    }

    /// A primitive record from a tile list (`p`: the record).
    fn primitive(&mut self, p: &[u8]) {
        let Some(rec) = self.shader() else { return };
        self.out.prims += 1;
        let clipped = p[0] == cl::CLIPPED_PRIMITIVE;
        let (n, idx): (usize, Vec<u32>) = if clipped {
            (p[1] as usize, (0..3).map(|k| cl::le32(&p[2 + 4 * k..])).collect())
        } else {
            let n = (p[1] & 0x7f) as usize;
            let wide = p[1] >> 7 != 0;
            (n, (0..n).map(|k| if wide { cl::le32(&p[2 + 4 * k..]) } else { cl::le16(&p[2 + 2 * k..]) as u32 }).collect())
        };
        if n == 0 || n > 3 { return; }
        self.cache.ensure(&rec, false, &idx, true, self.mem, self.code, self.insns);
        let nout = shade::outputs(&rec, false);
        let ps = rec.point_size() as usize;
        let vp = (self.st.vp[0] as i32, self.st.vp[1] as i32);
        let outs: Vec<Vec<u32>> = idx.iter().map(|&v| {
            let o = self.cache.get(rec.addr, v, false).unwrap();
            self.cache.data()[o..o + nout].to_vec()
        }).collect();
        let nvv = nout - 3 - ps;
        let vary_of = |o: &[u32]| -> Vec<f32> { o[3 + ps..].iter().map(|&w| f32::from_bits(w)).collect() };
        let sv_of = |o: &[u32]| SVert {
            x: (o[0] as u16 as i16) as i32 + vp.0, y: ((o[0] >> 16) as u16 as i16) as i32 + vp.1,
            z: f32::from_bits(o[1]), iw: f32::from_bits(o[2]), aux: [0.0; 2],
        };
        let mut verts: Vec<SVert> = Vec::new();
        let mut varys: Vec<Vec<f32>> = Vec::new();
        if clipped {
            let ov: Vec<Vec<f32>> = outs.iter().map(|o| vary_of(o)).collect();
            for k in 0..n {
                let b = &p[14 + 28 * k..];
                let w = |j: usize| cl::le32(&b[4 * j..]);
                verts.push(SVert { x: w(0) as i32 + vp.0, y: w(1) as i32 + vp.1, z: f32::from_bits(w(2)), iw: f32::from_bits(w(3)), aux: [0.0; 2] });
                let bw = [f32::from_bits(w(4)), f32::from_bits(w(5)), f32::from_bits(w(6))];
                varys.push((0..nvv).map(|j| bw[0] * ov[0][j] + bw[1] * ov[1][j] + bw[2] * ov[2][j]).collect());
            }
        } else {
            for o in &outs { verts.push(sv_of(o)); varys.push(vary_of(o)); }
        }
        // the provoking vertex: the primitive's last original vertex
        let flat_src = vary_of(&outs[if clipped { 2 } else { n - 1 }]);
        match n {
            3 => {
                let prim = Prim { v: [verts[0], verts[1], verts[2]], vary: [&varys[0], &varys[1], &varys[2]], flat_src: &flat_src, hw: 0, cull: true };
                self.triangle(&rec, &prim);
            }
            2 => {
                let (a, b) = (verts[0], verts[1]);
                let (dx, dy) = ((b.x - a.x) as f32, (b.y - a.y) as f32);
                if dx == 0.0 && dy == 0.0 { return; }
                let hw = self.st.line_width.max(1.0) * 8.0;
                let (nx, ny) = if dx.abs() >= dy.abs() { (0.0, hw) } else { (hw, 0.0) };
                let c = |v: SVert, sx: f32, t: f32| SVert { x: v.x + (sx * nx) as i32, y: v.y + (sx * ny) as i32, aux: [t, 0.0], ..v };
                let q = [c(a, -1.0, 0.0), c(b, -1.0, 1.0), c(b, 1.0, 1.0), c(a, 1.0, 0.0)];
                let vv = [&varys[0], &varys[1], &varys[1], &varys[0]];
                for tri in [[0usize, 1, 2], [0, 2, 3]] {
                    let prim = Prim { v: tri.map(|k| q[k]), vary: tri.map(|k| vv[k].as_slice()), flat_src: &flat_src, hw: 1, cull: false };
                    self.triangle(&rec, &prim);
                }
            }
            _ => {
                let a = verts[0];
                let size = if ps != 0 { f32::from_bits(outs[0][3]) } else { self.st.point_size };
                let h = (size.max(0.125) * 8.0) as i32;
                let c = |sx: i32, sy: i32| SVert { x: a.x + sx * h, y: a.y + sy * h, aux: [(sx + 1) as f32 * 0.5, (sy + 1) as f32 * 0.5], ..a };
                let q = [c(-1, -1), c(1, -1), c(1, 1), c(-1, 1)];
                for tri in [[0usize, 1, 2], [0, 2, 3]] {
                    let prim = Prim { v: tri.map(|k| q[k]), vary: [&varys[0], &varys[0], &varys[0]], flat_src: &flat_src, hw: 2, cull: false };
                    self.triangle(&rec, &prim);
                }
            }
        }
    }

    /// Rasterize a triangle within the current tile and shade its fragments.
    fn triangle(&mut self, rec: &ShaderRec, p: &Prim) {
        let st = self.st;
        let mut v = p.v;
        let area = (v[1].x - v[0].x) as i64 * (v[2].y - v[0].y) as i64 - (v[2].x - v[0].x) as i64 * (v[1].y - v[0].y) as i64;
        if area == 0 { return; }
        let cw = area > 0;
        let front = cw == (st.config & cl::config::CW != 0);
        if p.cull && !crate::bin::facing_enabled(st.config, area) { return; }
        let rev = p.cull && !front;
        // positive orientation; `map[k]` is the original vertex at position k
        let mut map = [0usize, 1, 2];
        if area < 0 { v.swap(1, 2); map.swap(1, 2); }
        let area = area.abs();
        let ts = self.tb.size;
        let (ox, oy) = (self.tile.0 * ts, self.tile.1 * ts);
        let c = st.clip;
        let (cx0, cy0, cx1, cy1) = (c[0] as i32, c[1] as i32, c[0] as i32 + c[2] as i32, c[1] as i32 + c[3] as i32);
        let minx = (v.iter().map(|q| q.x).min().unwrap() >> 4).max(ox as i32).max(cx0);
        let maxx = ((v.iter().map(|q| q.x).max().unwrap() + 15) >> 4).min((ox + ts) as i32 - 1).min(cx1 - 1);
        let miny = (v.iter().map(|q| q.y).min().unwrap() >> 4).max(oy as i32).max(cy0);
        let maxy = ((v.iter().map(|q| q.y).max().unwrap() + 15) >> 4).min((oy + ts) as i32 - 1).min(cy1 - 1);
        if minx > maxx || miny > maxy { return; }
        // edges: e0 = v1->v2, e1 = v2->v0, e2 = v0->v1; inside where all >= 0 (top-left ties)
        let edges: [(i64, i64, i64, i64, bool); 3] = std::array::from_fn(|k| {
            let (a, b) = (v[(k + 1) % 3], v[(k + 2) % 3]);
            let (dx, dy) = ((b.x - a.x) as i64, (b.y - a.y) as i64);
            (a.x as i64, a.y as i64, dx, dy, (dy == 0 && dx > 0) || dy < 0)
        });
        let edge = |k: usize, px: i64, py: i64| -> (i64, bool) {
            let (ax, ay, dx, dy, tl) = edges[k];
            let e = dx * (py - ay) - dy * (px - ax);
            (e, e > 0 || (e == 0 && tl))
        };
        let samples = self.tb.samples;
        let offs: &[(i32, i32)] = if samples == 4 { &SAMPLES4 } else { &[(8, 8)] };
        // depth offset
        let zoff = if st.config & cl::config::DEPTH_OFFSET != 0 {
            let (x0, y0) = (v[0].x as f64 / 16.0, v[0].y as f64 / 16.0);
            let (x1, y1) = ((v[1].x as f64 / 16.0) - x0, (v[1].y as f64 / 16.0) - y0);
            let (x2, y2) = ((v[2].x as f64 / 16.0) - x0, (v[2].y as f64 / 16.0) - y0);
            let (z1, z2) = ((v[1].z - v[0].z) as f64, (v[2].z - v[0].z) as f64);
            let det = x1 * y2 - x2 * y1;
            let (a, b) = if det != 0.0 { ((z1 * y2 - z2 * y1) / det, (x1 * z2 - x2 * z1) / det) } else { (0.0, 0.0) };
            st.offset_factor as f64 * a.abs().max(b.abs()) + st.offset_units as f64 * (1.0 / (1u64 << 25) as f64)
        } else { 0.0 };
        let mut lanes = [Lane::default(); 16];
        let mut nl = 0usize;
        let mut active = 0u16;
        let (y0, x0) = (miny & !1, minx & !1);
        let mut qy = y0;
        while qy <= maxy {
            let mut qx = x0;
            while qx <= maxx {
                let mut any = false;
                for k in 0..4 {
                    let (px, py) = (qx + (k & 1) as i32, qy + (k >> 1) as i32);
                    let lane = &mut lanes[nl + k];
                    *lane = Lane { x: (px - ox as i32).max(0) as u32, y: (py - oy as i32).max(0) as u32, ..Default::default() };
                    if px < minx || px > maxx || py < miny || py > maxy { continue; }
                    let mut cover = 0u8;
                    for (s, &(sx, sy)) in offs.iter().enumerate() {
                        let (sx, sy) = ((px * 16 + sx) as i64, (py * 16 + sy) as i64);
                        if (0..3).all(|e| edge(e, sx, sy).1) { cover |= 1 << s; }
                    }
                    if cover == 0 { continue; }
                    let (cx, cy) = ((px * 16 + 8) as i64, (py * 16 + 8) as i64);
                    let l: [f64; 3] = std::array::from_fn(|e| edge(e, cx, cy).0 as f64 / area as f64);
                    let iw = l[0] * v[0].iw as f64 + l[1] * v[1].iw as f64 + l[2] * v[2].iw as f64;
                    let z = l[0] * v[0].z as f64 + l[1] * v[1].z as f64 + l[2] * v[2].z as f64 + zoff;
                    lane.cover = cover;
                    // barycentrics by original vertex
                    let mut lo = [0f32; 3];
                    for k2 in 0..3 { lo[map[k2]] = l[k2] as f32; }
                    lane.l = lo;
                    lane.w = (1.0 / iw) as f32;
                    lane.z24 = (z.clamp(0.0, 1.0) * 16_777_215.0).round() as u32;
                    active |= 1 << (nl + k);
                    any = true;
                }
                if any { nl += 4; }
                if nl == 16 {
                    self.shade(rec, p, &lanes, active, rev);
                    nl = 0;
                    active = 0;
                }
                qx += 2;
            }
            qy += 2;
        }
        if nl > 0 { self.shade(rec, p, &lanes, active, rev); }
    }

    /// Run the fragment shader on up to four quads.
    fn shade(&mut self, rec: &ShaderRec, p: &Prim, lanes: &[Lane; 16], active: u16, rev: bool) {
        // varying interpolation uses barycentrics by the primitive's original vertex order;
        // vertex 2 is C's source, so the weights are applied as given
        let mut q = Qpu::new();
        for i in 0..16 {
            q.ra[15][i] = fo(lanes[i].w);
            q.rb[15][i] = lanes[i].z24;
        }
        q.start(rec.fs_code, rec.fs_unif);
        let origin = (self.tile.0 * self.tb.size, self.tile.1 * self.tb.size);
        let mut env = FragEnv {
            tb: self.tb, origin, lanes: *lanes, active, prim: p, nvary: rec.fs_nvary, flat: self.st.flat, next_vary: 0, rev,
            config: self.st.config, stencil: [None; 2], zpass: None, ms_writes: 0, color_loads: 0, fragments: 0,
        };
        let stop = q.run(self.code, self.mem, &mut env, FS_BUDGET);
        self.out.fragments += env.fragments;
        *self.insns += q.insns;
        crate::progress(q.insns);
        if stop != Stop::End { crate::log_first(&format!("vc4: fragment shader at {:#x} did not end ({stop:?})", rec.fs_code)); }
    }

    /// Run a rendering control list from `start` to `end`.
    pub fn run(&mut self, start: u32, end: u32, sem: &mut u32) {
        let mut pc = start;
        let mut stack: Vec<u32> = Vec::new();
        let mut loads: Vec<[u8; 7]> = Vec::new();
        let mut steps = 0u64;
        let mut buf = vec![0u8; 256];
        while pc != end {
            steps += 1;
            if steps > 100_000_000 { crate::log_first("vc4: rendering list does not end"); self.out.error = true; break; }
            let op = self.mem.r8(pc);
            let n = match op {
                cl::COMPRESSED_PRIMITIVE => { let b = self.mem.r8(pc.wrapping_add(1)); 2 + (b & 0x7f) as u32 * if b >> 7 != 0 { 4 } else { 2 } }
                cl::CLIPPED_PRIMITIVE => 14 + self.mem.r8(pc.wrapping_add(1)) as u32 * 28,
                _ => match cl::size(op) {
                    Some(n) => n,
                    None => {
                        crate::log_first(&format!("vc4: rendering list: unknown packet {op} at {pc:#x}"));
                        self.out.error = true;
                        break;
                    }
                },
            };
            let p = &mut buf[..n as usize];
            self.mem.read(pc, p);
            let p: Vec<u8> = p.to_vec();
            let next = pc.wrapping_add(n);
            match op {
                cl::HALT => break,
                cl::NOP | cl::FLUSH | cl::FLUSH_ALL | cl::START_TILE_BINNING | cl::INCREMENT_SEMAPHORE | cl::PRIMITIVE_LIST_FORMAT => {}
                cl::WAIT_ON_SEMAPHORE => {
                    if *sem > 0 { *sem -= 1 } else { crate::log_first("vc4: rendering waits on a semaphore nothing raised (going on)"); }
                }
                cl::BRANCH => { pc = cl::le32(&p[1..]); continue; }
                cl::BRANCH_TO_SUB_LIST => {
                    if stack.len() < 2 { stack.push(next); pc = cl::le32(&p[1..]); continue; }
                    crate::log_first("vc4: rendering list: sub-lists nested too deep");
                }
                cl::RETURN_FROM_SUB_LIST => { if let Some(r) = stack.pop() { pc = r; continue; } }
                cl::TILE_RENDERING_MODE_CONFIG => {
                    self.frame = Frame::parse(&p);
                    self.tb.configure(&self.frame);
                }
                cl::CLEAR_COLORS => self.tb.set_clear(&p),
                cl::TILE_COORDINATES => {
                    self.tile = (p[1] as u32, p[2] as u32);
                    for l in loads.drain(..) {
                        if l[0] == cl::LOAD_FULL_RES_TILE_BUFFER { self.tb.load_full(&l, self.mem) } else { self.tb.load_general(&self.frame, self.tile, &l, self.mem) }
                    }
                }
                cl::LOAD_TILE_BUFFER_GENERAL | cl::LOAD_FULL_RES_TILE_BUFFER => {
                    let mut l = [0u8; 7];
                    l[..n as usize].copy_from_slice(&p);
                    loads.push(l);
                }
                cl::STORE_TILE_BUFFER_GENERAL => { if self.tb.store_general(&self.frame, self.tile, &p, self.mem) { self.out.eof = true; } }
                cl::STORE_MS_TILE_BUFFER | cl::STORE_MS_TILE_BUFFER_AND_EOF => {
                    self.tb.store_ms(&self.frame, self.tile, self.mem);
                    if op == cl::STORE_MS_TILE_BUFFER_AND_EOF { self.out.eof = true; }
                }
                cl::STORE_FULL_RES_TILE_BUFFER => { if self.tb.store_full(&p, self.mem) { self.out.eof = true; } }
                cl::COMPRESSED_PRIMITIVE | cl::CLIPPED_PRIMITIVE => self.primitive(&p),
                _ => {
                    if !self.st.apply(&p) {
                        crate::log_first(&format!("vc4: rendering list: packet {} ({op}) ignored", cl::name(op)));
                    }
                }
            }
            pc = next;
        }
    }
}
