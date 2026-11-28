//! The binning thread (CT0) and the primitive tile binner (PTB).
//!
//! The binning control list sets up the tile grid (`TILE_BINNING_MODE_CONFIG`), state, and
//! draws primitives. Each draw's vertices run through the coordinate shader; primitives are
//! assembled, clipped (against the near and far planes, and against the guard band of the
//! 12.4 screen coordinates), culled, and written into the list of every tile their bounding box
//! touches, after whatever state packets that tile's list has not seen yet. Rendering then
//! branches into each tile's list.
//!
//! Tile lists start in the initial blocks (one per tile at the start of the tile allocation
//! memory) and continue in blocks the binner allocates: from the rest of the tile allocation
//! memory, then from the overflow memory the kernel hands out (BPOA/BPOS). When that runs out
//! too, the binner stops with OUTOMEM until the kernel gives it more. `FLUSH` ends every list
//! with a return.
//!
//! The primitive records in tile lists are this model's own (the hardware's compressed formats
//! are not documented in enough detail; nothing but the rendering thread reads them):
//! * `48, n | wide << 7, n vertex indices (u16, or u32 if wide)`: a point, line or triangle;
//! * `49, n, 3 x u32 original indices, n x (x16, y16: i32 screen position from the viewport
//!   centre in 1/16 pixel; zs, 1/wc, b0, b1, b2: f32, the clip-space weights of the original
//!   vertices)`: a primitive the binner clipped.
//!
//! The work splits in two: [`run`] shades and bins (any thread), [`Writer`] lays the lists out
//! in guest memory (the device, which owns the overflow registers).

use crate::cl::{self, GlState, ShaderRec};
use crate::mem::Mem;
use crate::qpu::Code;
use crate::shade;

/// The tile grid and tile memory of `TILE_BINNING_MODE_CONFIG`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct TileCfg {
    pub alloc: u32,
    pub alloc_size: u32,
    pub tsda: u32,
    pub tiles_x: u32,
    pub tiles_y: u32,
    pub ms: bool,
    pub hdr: bool,
    pub init_block: u32,
    pub block: u32,
}

impl TileCfg {
    pub fn parse(p: &[u8]) -> TileCfg {
        let f = p[15];
        TileCfg {
            alloc: cl::le32(&p[1..]), alloc_size: cl::le32(&p[5..]), tsda: cl::le32(&p[9..]),
            tiles_x: p[13] as u32, tiles_y: p[14] as u32, ms: f & 1 != 0, hdr: f & 2 != 0,
            init_block: 32 << (f >> 3 & 3), block: 32 << (f >> 5 & 3),
        }
    }
    pub fn tile_size(&self) -> u32 { if self.ms || self.hdr { 32 } else { 64 } }
    pub fn ntiles(&self) -> usize { (self.tiles_x * self.tiles_y) as usize }
}

/// What a binning job produced: the tile lists (records, each written whole), and how the
/// control list ended.
#[derive(Clone, Debug, Default)]
pub struct BinOutput {
    pub cfg: Option<TileCfg>,
    /// per tile: the bytes of its list, and where each record starts
    pub lists: Vec<Vec<u8>>,
    pub starts: Vec<Vec<u32>>,
    pub flushed: bool,
    /// semaphore increments (they happen once the lists are flushed)
    pub sem: u32,
    pub error: bool,
    pub prims: u64,
}

/// The state packets rendering needs, as they stand.
fn state_packets(st: &GlState) -> [[u8; 9]; 9] {
    let mut p = [[0u8; 9]; 9];
    p[0][..4].copy_from_slice(&[cl::CONFIGURATION_BITS, st.config as u8, (st.config >> 8) as u8, (st.config >> 16) as u8]);
    let w = |op: u8, v: u32| { let mut b = [0u8; 9]; b[0] = op; b[1..5].copy_from_slice(&v.to_le_bytes()); b };
    p[1] = w(cl::FLAT_SHADE_FLAGS, st.flat);
    p[2] = w(cl::POINT_SIZE, st.point_size.to_bits());
    p[3] = w(cl::LINE_WIDTH, st.line_width.to_bits());
    p[4] = w(cl::DEPTH_OFFSET, (st.offset_factor.to_bits() >> 16) | (st.offset_units.to_bits() >> 16) << 16);
    p[5][0] = cl::CLIP_WINDOW;
    for k in 0..4 { p[5][1 + 2 * k..3 + 2 * k].copy_from_slice(&st.clip[k].to_le_bytes()); }
    p[6] = w(cl::VIEWPORT_OFFSET, (st.vp[0] as u16 as u32) | (st.vp[1] as u16 as u32) << 16);
    p[7][0] = cl::Z_CLIPPING;
    p[7][1..5].copy_from_slice(&st.zclip[0].to_bits().to_le_bytes());
    p[7][5..9].copy_from_slice(&st.zclip[1].to_bits().to_le_bytes());
    p[8] = w(st.shader.0, st.shader.1);
    p
}

/// A clip-space vertex during clipping, with its weights of the original vertices.
#[derive(Clone, Copy, Debug)]
struct Cv { c: [f32; 4], b: [f32; 3] }

fn lerp(a: &Cv, b: &Cv, t: f32) -> Cv {
    Cv { c: std::array::from_fn(|k| a.c[k] + (b.c[k] - a.c[k]) * t), b: std::array::from_fn(|k| a.b[k] + (b.b[k] - a.b[k]) * t) }
}

/// Guard band: screen coordinates must fit the 16-bit 12.4 format.
const GUARD: f32 = 2047.0 * 16.0;

/// Signed distances to the clip planes (inside: >= 0): w > 0, Zs >= 0, Zs <= 1, the guard band.
fn planes(st: &GlState, c: &[f32; 4]) -> [f32; 7] {
    let (x, y, z, w) = (c[0], c[1], c[2], c[3]);
    let zs = st.clip_z[0] * z + st.clip_z[1] * w;
    let (hw, hh) = (st.clip_xy[0].abs(), st.clip_xy[1].abs());
    [w - 1e-6 * w.abs().max(1e-30), zs, w - zs, GUARD * w - hw * x, GUARD * w + hw * x, GUARD * w - hh * y, GUARD * w + hh * y]
}

/// Clip a polygon (or a segment) against the planes some vertex is outside of.
fn clip_poly(st: &GlState, mut poly: Vec<Cv>, mask: u32) -> Vec<Cv> {
    let closed = poly.len() > 2;
    for p in 0..7 {
        if mask >> p & 1 == 0 || poly.is_empty() { continue; }
        let d: Vec<f32> = poly.iter().map(|v| planes(st, &v.c)[p]).collect();
        let mut out = Vec::with_capacity(poly.len() + 2);
        let n = poly.len();
        let edges = if closed { n } else { n - 1 };
        if !closed {
            // a segment: keep the inside part
            let (a, b) = (poly[0], poly[1]);
            let (da, db) = (d[0], d[1]);
            if da < 0.0 && db < 0.0 { return Vec::new(); }
            let a2 = if da < 0.0 { lerp(&a, &b, da / (da - db)) } else { a };
            let b2 = if db < 0.0 { lerp(&a, &b, da / (da - db)) } else { b };
            poly = vec![a2, b2];
            continue;
        }
        for i in 0..edges {
            let j = (i + 1) % n;
            let (a, b) = (&poly[i], &poly[j]);
            if d[i] >= 0.0 { out.push(*a); }
            if (d[i] >= 0.0) != (d[j] >= 0.0) { out.push(lerp(a, b, d[i] / (d[i] - d[j]))); }
        }
        poly = out;
    }
    poly
}

/// Screen position (1/16 pixel from the viewport centre, as the shaders' FTOI gives it) and
/// Zs, 1/Wc of a clip-space position.
fn project(st: &GlState, c: &[f32; 4]) -> (i32, i32, f32, f32) {
    let iw = 1.0 / c[3];
    let x = (c[0] * st.clip_xy[0] * iw) as i32;
    let y = (c[1] * st.clip_xy[1] * iw) as i32;
    (x, y, c[2] * st.clip_z[0] * iw + st.clip_z[1], iw)
}

struct Binner<'a> {
    cfg: TileCfg,
    st: GlState,
    out: &'a mut BinOutput,
    /// state packets as of now, a version for them, and per tile the version and packets its
    /// list has
    cur: [[u8; 9]; 9],
    ver: u32,
    tile_ver: Vec<u32>,
    tile_state: Vec<[[u8; 9]; 9]>,
}

impl Binner<'_> {
    fn emit(&mut self, t: usize, rec: &[u8]) {
        if self.tile_ver[t] != self.ver {
            for k in 0..9 {
                if self.tile_state[t][k] != self.cur[k] && self.cur[k][0] != 0 {
                    let n = cl::size(self.cur[k][0]).unwrap_or(1) as usize;
                    self.out.starts[t].push(self.out.lists[t].len() as u32);
                    self.out.lists[t].extend_from_slice(&self.cur[k][..n]);
                    self.tile_state[t][k] = self.cur[k];
                }
            }
            self.tile_ver[t] = self.ver;
        }
        self.out.starts[t].push(self.out.lists[t].len() as u32);
        self.out.lists[t].extend_from_slice(rec);
    }

    /// Put a record into the tiles a pixel rectangle (inclusive, screen pixels) touches.
    fn bin(&mut self, x0: i32, y0: i32, x1: i32, y1: i32, rec: &[u8]) {
        let c = self.st.clip;
        let (cx0, cy0, cx1, cy1) = (c[0] as i32, c[1] as i32, c[0] as i32 + c[2] as i32 - 1, c[1] as i32 + c[3] as i32 - 1);
        let (x0, y0, x1, y1) = (x0.max(cx0).max(0), y0.max(cy0).max(0), x1.min(cx1), y1.min(cy1));
        if x0 > x1 || y0 > y1 { return; }
        let ts = self.cfg.tile_size() as i32;
        let (tx0, ty0) = (x0 / ts, y0 / ts);
        let (tx1, ty1) = ((x1 / ts).min(self.cfg.tiles_x as i32 - 1), (y1 / ts).min(self.cfg.tiles_y as i32 - 1));
        if tx0 > tx1 || ty0 > ty1 { return; }
        self.out.prims += 1;
        for ty in ty0..=ty1 {
            for tx in tx0..=tx1 { self.emit((ty * self.cfg.tiles_x as i32 + tx) as usize, rec); }
        }
    }

    fn set_state(&mut self, st: GlState) {
        self.st = st;
        let p = state_packets(&st);
        if p != self.cur { self.cur = p; self.ver += 1; }
    }
}

/// Screen-pixel bounding box (inclusive) of positions in 1/16 pixel, grown by `pad` pixels.
fn bbox(pts: &[(i32, i32)], pad: f32) -> (i32, i32, i32, i32) {
    let p16 = (pad * 16.0).ceil() as i32 + 16;
    let x0 = pts.iter().map(|p| p.0).min().unwrap() - p16;
    let x1 = pts.iter().map(|p| p.0).max().unwrap() + p16;
    let y0 = pts.iter().map(|p| p.1).min().unwrap() - p16;
    let y1 = pts.iter().map(|p| p.1).max().unwrap() + p16;
    (x0.div_euclid(16), y0.div_euclid(16), x1.div_euclid(16), y1.div_euclid(16))
}

/// Whether a triangle is front facing: `area2` is its doubled signed area in screen space
/// (y down), positive when it runs clockwise on screen. The hardware's "clockwise" is the
/// other way round (y up, as GL's window coordinates): Mesa sets the CW bit for GL's
/// counter-clockwise front faces with its y-flipped viewport, and those have negative area here.
pub fn front_facing(config: u32, area2: i64) -> bool { (area2 < 0) == (config & cl::config::CW != 0) }

/// Whether a triangle (screen positions) is drawn under the face culling configuration.
pub fn facing_enabled(config: u32, area2: i64) -> bool {
    if area2 == 0 { return false; }
    let front = front_facing(config, area2);
    if front { config & cl::config::ENABLE_FRONT != 0 } else { config & cl::config::ENABLE_BACK != 0 }
}


fn idx_record(idx: &[u32]) -> Vec<u8> {
    let wide = idx.iter().any(|&v| v > 0xffff);
    let mut r = vec![cl::COMPRESSED_PRIMITIVE, idx.len() as u8 | (wide as u8) << 7];
    for &v in idx { if wide { r.extend_from_slice(&v.to_le_bytes()) } else { r.extend_from_slice(&(v as u16).to_le_bytes()) } }
    r
}

/// Vertices of one draw the model takes (Mesa splits draws far below this).
const MAX_DRAW: u32 = 1 << 20;

/// Run a binning control list from `start` to `end`.
pub fn run(start: u32, end: u32, mem: &mut Mem, code: &mut Code, par: &mut crate::par::Par, insns: &mut u64) -> BinOutput {
    let mut out = BinOutput::default();
    let mut cache = shade::Cache::default();
    // with several workers, every draw's vertices go through the coordinate shader first, all
    // workers taking chunks of them; binning then finds them shaded
    if par.threads > 1 {
        let want = draws(start, end, mem);
        shade::shade_parallel(want, true, &mut cache, mem, code, par, insns);
    }
    let mut st = GlState::default();
    let mut pc = start;
    let mut stack: Vec<u32> = Vec::new();
    let mut rec: Option<ShaderRec> = None;
    let mut steps = 0u64;
    // the binner borrows `out` mutably; keep its pieces here and move them in when configured
    let mut cfg: Option<TileCfg> = None;
    let mut cur = [[0u8; 9]; 9];
    let mut ver = 0u32;
    let mut tile_ver: Vec<u32> = Vec::new();
    let mut tile_state: Vec<[[u8; 9]; 9]> = Vec::new();
    while pc != end {
        steps += 1;
        if steps > 50_000_000 { crate::log_first("vc4: binning list does not end"); out.error = true; break; }
        let op = mem.r8(pc);
        let Some(n) = cl::size(op) else {
            crate::log_first(&format!("vc4: binning list: unknown packet {op} at {pc:#x}"));
            out.error = true;
            break;
        };
        let mut p = [0u8; 16];
        mem.read(pc, &mut p[..n as usize]);
        let next = pc.wrapping_add(n);
        if crate::cl_log_enabled() { eprintln!("\rvc4 bin {pc:#010x}: {} {:02x?}\r", cl::name(op), &p[1..n as usize]); }
        match op {
            cl::HALT => break,
            cl::NOP | cl::WAIT_ON_SEMAPHORE | cl::START_TILE_BINNING => {
                if op == cl::START_TILE_BINNING { ver += 1; for t in tile_state.iter_mut() { *t = [[0; 9]; 9]; } }
            }
            cl::FLUSH | cl::FLUSH_ALL => out.flushed = true,
            cl::INCREMENT_SEMAPHORE => out.sem += 1,
            cl::BRANCH => { pc = cl::le32(&p[1..]); continue; }
            cl::BRANCH_TO_SUB_LIST => {
                if stack.len() < 2 { stack.push(next); pc = cl::le32(&p[1..]); continue; }
                crate::log_first("vc4: binning list: sub-lists nested too deep");
            }
            cl::RETURN_FROM_SUB_LIST => { if let Some(r) = stack.pop() { pc = r; continue; } }
            cl::TILE_BINNING_MODE_CONFIG => {
                let c = TileCfg::parse(&p);
                cfg = Some(c);
                out.cfg = Some(c);
                out.lists = vec![Vec::new(); c.ntiles()];
                out.starts = vec![Vec::new(); c.ntiles()];
                tile_ver = vec![u32::MAX; c.ntiles()];
                tile_state = vec![[[0; 9]; 9]; c.ntiles()];
            }
            cl::GL_INDEXED_PRIMITIVE | cl::GL_ARRAY_PRIMITIVE => {
                let Some(c) = cfg else { crate::log_first("vc4: primitive before TILE_BINNING_MODE_CONFIG"); pc = next; continue; };
                if st.shader.0 == 0 { crate::log_first("vc4: primitive before shader state"); pc = next; continue; }
                if rec.as_ref().map(|r| r.addr) != Some(st.shader.1 & !0xf) {
                    rec = Some(ShaderRec::read(st.shader.0, st.shader.1, mem));
                }
                let r = rec.clone().unwrap();
                let mode = p[1] & 15;
                let idx: Vec<u32> = if op == cl::GL_INDEXED_PRIMITIVE {
                    let (len, addr, max) = (cl::le32(&p[2..]), cl::le32(&p[6..]), cl::le32(&p[10..]));
                    let wide = p[1] >> 4 != 0;
                    {
                        let _ = max;
                        (0..len.min(MAX_DRAW)).map(|k| if wide { mem.r16(addr.wrapping_add(2 * k)) as u32 } else { mem.r8(addr.wrapping_add(k)) as u32 }).collect()
                    }
                } else {
                    let (len, first) = (cl::le32(&p[2..]), cl::le32(&p[6..]));
                    (0..len.min(MAX_DRAW)).map(|k| first.wrapping_add(k)).collect()
                };
                let mut b = Binner { cfg: c, st, out: &mut out, cur, ver, tile_ver: std::mem::take(&mut tile_ver), tile_state: std::mem::take(&mut tile_state) };
                b.set_state(st);
                draw(&mut b, &r, mode, &idx, &mut cache, mem, code, insns);
                cur = b.cur;
                ver = b.ver;
                tile_ver = std::mem::take(&mut b.tile_ver);
                tile_state = std::mem::take(&mut b.tile_state);
            }
            _ => {
                if !st.apply(&p[..n as usize]) {
                    crate::log_first(&format!("vc4: binning list: packet {} ({op}) ignored", cl::name(op)));
                }
            }
        }
        pc = next;
    }
    out
}

/// The vertices the draws of a binning list use, by shader record (what [`run`] will shade, so
/// it can be shaded ahead in parallel). Records with pre-shaded vertices are left out.
fn draws(start: u32, end: u32, mem: &mut Mem) -> Vec<(ShaderRec, Vec<u32>)> {
    let mut want: Vec<(ShaderRec, Vec<u32>)> = Vec::new();
    let mut st = GlState::default();
    let mut pc = start;
    let mut stack: Vec<u32> = Vec::new();
    let mut steps = 0u64;
    while pc != end && steps < 50_000_000 {
        steps += 1;
        let op = mem.r8(pc);
        let Some(n) = cl::size(op) else { break };
        let mut p = [0u8; 16];
        mem.read(pc, &mut p[..n as usize]);
        let next = pc.wrapping_add(n);
        match op {
            cl::HALT => break,
            cl::BRANCH => { pc = cl::le32(&p[1..]); continue; }
            cl::BRANCH_TO_SUB_LIST if stack.len() < 2 => { stack.push(next); pc = cl::le32(&p[1..]); continue; }
            cl::RETURN_FROM_SUB_LIST => { if let Some(r) = stack.pop() { pc = r; continue; } }
            cl::GL_INDEXED_PRIMITIVE | cl::GL_ARRAY_PRIMITIVE if st.shader.0 != 0 => {
                let addr = st.shader.1 & !0xf;
                let k = match want.iter().position(|w| w.0.addr == addr) {
                    Some(k) => k,
                    None => { want.push((ShaderRec::read(st.shader.0, st.shader.1, mem), Vec::new())); want.len() - 1 }
                };
                if want[k].0.nv.is_some() { pc = next; continue; }
                let idx = &mut want[k].1;
                if op == cl::GL_INDEXED_PRIMITIVE {
                    let (len, addr) = (cl::le32(&p[2..]), cl::le32(&p[6..]));
                    let wide = p[1] >> 4 != 0;
                    idx.extend((0..len.min(MAX_DRAW)).map(|k| if wide { mem.r16(addr.wrapping_add(2 * k)) as u32 } else { mem.r8(addr.wrapping_add(k)) as u32 }));
                } else {
                    let (len, first) = (cl::le32(&p[2..]), cl::le32(&p[6..]));
                    idx.extend((0..len.min(MAX_DRAW)).map(|k| first.wrapping_add(k)));
                }
            }
            _ => { st.apply(&p[..n as usize]); }
        }
        pc = next;
    }
    want
}

/// Bin one draw: shade its vertices, assemble, clip, cull, bin.
#[allow(clippy::too_many_arguments)]
fn draw(b: &mut Binner, rec: &ShaderRec, mode: u8, idx: &[u32], cache: &mut shade::Cache, mem: &mut Mem, code: &mut Code, insns: &mut u64) {
    let nv = rec.nv.is_some();
    // shade all vertices of the draw first, in batches of 16
    let mut uniq: Vec<u32> = idx.to_vec();
    uniq.sort_unstable();
    uniq.dedup();
    cache.ensure(rec, !nv, &uniq, false, mem, code, insns);
    let st = b.st;
    let vp = (st.vp[0] as i32, st.vp[1] as i32);
    let ps_flag = rec.point_size();
    let get = |v: u32| -> &[u32] { let o = cache.get(rec.addr, v, !nv).unwrap(); &cache.data()[o..o + shade::outputs(rec, !nv)] };
    // screen position (1/16 pixel, absolute) of a shaded vertex
    let screen = |o: &[u32]| -> (i32, i32) {
        let xy = if nv { o[0] } else { o[4] };
        ((xy as u16 as i16) as i32 + vp.0, ((xy >> 16) as u16 as i16) as i32 + vp.1)
    };
    let clipping = !nv;
    assemble_and_bin(b, rec, mode, idx, |b, prim: &[usize]| {
        let vs: Vec<&[u32]> = prim.iter().map(|&k| get(idx[k])).collect();
        let pidx: Vec<u32> = prim.iter().map(|&k| idx[k]).collect();
        match prim.len() {
            1 => {
                let o = vs[0];
                if clipping {
                    let d = planes(&st, &[f32::from_bits(o[0]), f32::from_bits(o[1]), f32::from_bits(o[2]), f32::from_bits(o[3])]);
                    if d[0] < 0.0 || d[1] < 0.0 || d[2] < 0.0 { return; }
                }
                let size = if ps_flag { f32::from_bits(o[if nv { 3 } else { 7 }]) } else { st.point_size };
                let p = screen(o);
                let (x0, y0, x1, y1) = bbox(&[p], size * 0.5);
                b.bin(x0, y0, x1, y1, &idx_record(&pidx));
            }
            n => {
                let cv: Vec<Cv> = (0..n).map(|k| {
                    let o = vs[k];
                    let mut bw = [0.0; 3];
                    bw[k] = 1.0;
                    Cv { c: [f32::from_bits(o[0]), f32::from_bits(o[1]), f32::from_bits(o[2]), f32::from_bits(o[3])], b: bw }
                }).collect();
                let (mut any_out, mut all_out) = (0u32, 0x7fu32);
                if clipping {
                    for v in &cv {
                        let d = planes(&st, &v.c);
                        let mut m = 0u32;
                        for (p, &x) in d.iter().enumerate() { if x < 0.0 { m |= 1 << p; } }
                        any_out |= m;
                        all_out &= m;
                    }
                    if all_out != 0 { return; }
                }
                let pad = if n == 2 { st.line_width * 0.5 + 1.0 } else { 0.0 };
                if any_out == 0 {
                    let pts: Vec<(i32, i32)> = vs.iter().map(|o| screen(o)).collect();
                    if n == 3 {
                        let area2 = (pts[1].0 - pts[0].0) as i64 * (pts[2].1 - pts[0].1) as i64 - (pts[2].0 - pts[0].0) as i64 * (pts[1].1 - pts[0].1) as i64;
                        if !facing_enabled(st.config, area2) { return; }
                    }
                    let (x0, y0, x1, y1) = bbox(&pts, pad);
                    b.bin(x0, y0, x1, y1, &idx_record(&pidx));
                    return;
                }
                let poly = clip_poly(&st, cv, any_out);
                if poly.len() < n { return; }
                let proj: Vec<(i32, i32, f32, f32)> = poly.iter().map(|v| project(&st, &v.c)).collect();
                let emit = |b: &mut Binner, ks: &[usize]| {
                    let pts: Vec<(i32, i32)> = ks.iter().map(|&k| (proj[k].0 + vp.0, proj[k].1 + vp.1)).collect();
                    if ks.len() == 3 {
                        let area2 = (pts[1].0 - pts[0].0) as i64 * (pts[2].1 - pts[0].1) as i64 - (pts[2].0 - pts[0].0) as i64 * (pts[1].1 - pts[0].1) as i64;
                        if !facing_enabled(st.config, area2) { return; }
                    }
                    let mut r = vec![cl::CLIPPED_PRIMITIVE, ks.len() as u8];
                    for k in 0..3 { r.extend_from_slice(&pidx.get(k).copied().unwrap_or(pidx[0]).to_le_bytes()); }
                    for &k in ks {
                        let (x, y, z, iw) = proj[k];
                        for w in [x as u32, y as u32, z.to_bits(), iw.to_bits(), poly[k].b[0].to_bits(), poly[k].b[1].to_bits(), poly[k].b[2].to_bits()] {
                            r.extend_from_slice(&w.to_le_bytes());
                        }
                    }
                    let (x0, y0, x1, y1) = bbox(&pts, pad);
                    b.bin(x0, y0, x1, y1, &r);
                };
                if n == 2 { emit(b, &[0, 1]); } else { for k in 1..poly.len() - 1 { emit(b, &[0, k, k + 1]); } }
            }
        }
    });
}

fn assemble_and_bin(b: &mut Binner, _rec: &ShaderRec, mode: u8, idx: &[u32], mut f: impl FnMut(&mut Binner, &[usize])) {
    cl::assemble(mode, idx.len(), |p| f(b, p));
}

/// Lays a job's tile lists out in guest memory; resumable when the memory runs out.
#[derive(Clone, Debug, Default)]
pub struct Writer {
    pub tile: usize,
    pub rec: usize,
    /// where the current tile's list continues, and the room left in its block
    pub cur: u32,
    pub room: u32,
    /// the block pool: tile allocation memory, then overflow memory
    pub pool: u32,
    pub pool_left: u32,
    pub started: bool,
    /// overflow memory was used
    pub spilled: bool,
}

pub enum Written { Done, OutOfMemory }

impl Writer {
    /// Write (or go on writing) `out`'s lists. `bpo` is the overflow memory (BPOA, BPOS): taken
    /// whole when the pool runs dry, leaving BPOS zero.
    pub fn write(&mut self, out: &BinOutput, bpo: &mut (u32, u32), mem: &mut Mem) -> Written {
        let Some(cfg) = out.cfg else { return Written::Done };
        let nt = cfg.ntiles();
        if !self.started {
            self.started = true;
            let init = (nt as u32 * cfg.init_block).div_ceil(256) * 256;
            self.pool = cfg.alloc.wrapping_add(init);
            self.pool_left = cfg.alloc_size.saturating_sub(init);
            self.tile = 0;
            self.rec = 0;
            self.cur = cfg.alloc;
            self.room = cfg.init_block;
        }
        while self.tile < nt {
            let t = self.tile;
            let (list, starts) = (&out.lists[t], &out.starts[t]);
            while self.rec < starts.len() {
                let s = starts[self.rec] as usize;
                let e = starts.get(self.rec + 1).map_or(list.len(), |&x| x as usize);
                let need = (e - s) as u32;
                if self.room < need + 5 {
                    let blocks = (need + 5).div_ceil(cfg.block);
                    let size = blocks * cfg.block;
                    if self.pool_left < size {
                        if bpo.1 >= size {
                            self.pool = bpo.0;
                            self.pool_left = bpo.1;
                            bpo.0 = bpo.0.wrapping_add(bpo.1);
                            bpo.1 = 0;
                            self.spilled = true;
                        } else {
                            return Written::OutOfMemory;
                        }
                    }
                    let blk = self.pool;
                    self.pool = self.pool.wrapping_add(size);
                    self.pool_left -= size;
                    let mut br = [cl::BRANCH, 0, 0, 0, 0];
                    br[1..].copy_from_slice(&blk.to_le_bytes());
                    mem.write(self.cur, &br);
                    self.cur = blk;
                    self.room = size;
                }
                mem.write(self.cur, &list[s..e]);
                self.cur = self.cur.wrapping_add(need);
                self.room -= need;
                self.rec += 1;
            }
            if out.flushed { mem.w8(self.cur, cl::RETURN_FROM_SUB_LIST); }
            self.tile += 1;
            self.rec = 0;
            self.cur = cfg.alloc.wrapping_add(self.tile as u32 * cfg.init_block);
            self.room = cfg.init_block;
        }
        Written::Done
    }
}
