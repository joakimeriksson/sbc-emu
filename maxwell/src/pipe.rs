//! The graphics pipeline of a draw: vertices are fetched and shaded, primitives assembled, clipped
//! and rasterized, fragments shaded and written.
use crate::raster;
use crate::rop::{Blend, DepthStencil, Output, Rect, Targets};
use crate::threed::{Cbuf, Threed};
use crate::vertex::{self, Attribute};
use crate::Memory;
use maxwell_shader::isa::{Attr, IpaLoc, IpaMode, LodMode, Pixld, SurfDim, TexKind};
use maxwell_shader::sph::Interp;
use maxwell_shader::{Exit, Handle, Host, IpaOp, Lanes, Program, Sph, SurfOp, TexOp};
use maxwell_tex::{Args, Lod, Sampler, TexCache, Texture};
use std::cell::Cell;
use std::collections::HashMap;
use std::rc::Rc;

/// Invocations that run together.
pub const L: usize = 16;

/// A vertex's attributes: words from address 0x40 to 0x3c0.
pub const WORDS: usize = 224;
const FIRST: u32 = 0x40;

/// The word of an attribute address, if vertices carry it.
#[inline(always)]
pub fn word(addr: u32) -> Option<usize> {
    let w = (addr.wrapping_sub(FIRST) / 4) as usize;
    (w < WORDS).then_some(w)
}
pub const POSITION: usize = (0x70 - 0x40) / 4;
const POINT_SIZE: usize = (0x6c - 0x40) / 4;
const LAYER: usize = (0x64 - 0x40) / 4;
pub const CLIP: usize = (0x2c0 - 0x40) / 4;

pub enum Indices<'a> {
    Range(u32, u32),
    List(&'a [u32]),
}

pub struct Shader {
    pub program: Program,
    pub sph: Sph,
}

/// A constant buffer's words, read when they are first used.
pub struct Words {
    buffer: Cell<Cbuf>,
    words: Vec<Cell<u32>>,
    valid: [Cell<u64>; 16],
}

impl Default for Words {
    fn default() -> Self {
        Self { buffer: Default::default(), words: vec![Cell::new(0); 0x4000], valid: Default::default() }
    }
}

impl Words {
    fn bind(&self, b: Cbuf) {
        if self.buffer.get() != b {
            self.buffer.set(b);
            self.forget();
        }
    }

    fn forget(&self) {
        self.valid.iter().for_each(|v| v.set(0));
    }

    #[inline]
    fn get(&self, offset: u32, mem: &dyn Memory) -> u32 {
        let b = self.buffer.get();
        if offset >= b.size || offset >= 0x1_0000 {
            return 0;
        }
        let w = offset as usize / 4;
        let (block, bit) = (w / 16, 1u64 << (w / 16 % 64));
        if self.valid[block / 64].get() & bit == 0 {
            let mut raw = [0u8; 64];
            mem.read(b.address + 64 * block as u64, &mut raw);
            for (k, c) in raw.chunks_exact(4).enumerate() {
                self.words[16 * block + k].set(u32::from_le_bytes(c.try_into().unwrap()));
            }
            self.valid[block / 64].set(self.valid[block / 64].get() | bit);
        }
        self.words[w].get()
    }
}

pub struct Pipe {
    shaders: HashMap<u64, Option<Rc<Shader>>>,
    textures: HashMap<u32, Rc<Texture>>,
    samplers: HashMap<u32, Rc<Sampler>>,
    tex_cache: TexCache,
    /// constant values of attributes without a stream
    pub constants: [[u32; 4]; 32],
    pub cbufs_changed: bool,
    words: Vec<Rc<[Words; 18]>>,
    lanes: Lanes<L>,
    pub errors: std::collections::BTreeSet<String>,
}

impl Default for Pipe {
    fn default() -> Self {
        Self {
            shaders: HashMap::new(),
            textures: HashMap::new(),
            samplers: HashMap::new(),
            tex_cache: TexCache::new(),
            constants: [[0, 0, 0, 1f32.to_bits()]; 32],
            cbufs_changed: true,
            words: (0..6).map(|_| Rc::new(std::array::from_fn(|_| Words::default()))).collect(),
            lanes: Lanes::new(),
            errors: Default::default(),
        }
    }
}

impl Pipe {
    pub fn invalidate_textures(&mut self) {
        self.textures.clear();
        self.samplers.clear();
        self.tex_cache.invalidate();
    }

    pub fn invalidate_shaders(&mut self) {
        self.shaders.clear();
    }

    /// Memory may have changed behind the engine's back (a new submission).
    pub fn invalidate(&mut self) {
        self.invalidate_textures();
        self.invalidate_shaders();
        self.cbufs_changed = true;
    }

    pub fn fail(&mut self, e: String) {
        self.error(e);
    }

    fn error(&mut self, e: String) {
        if self.errors.len() < 100 && self.errors.insert(e.clone()) {
            eprintln!("[maxwell] {e}");
        }
    }

    /// The shader at `at` (its header first) of the code segment at `base`.
    fn shader(&mut self, base: u64, start: u32, mem: &dyn Memory) -> Option<Rc<Shader>> {
        if let Some(s) = self.shaders.get(&(base + start as u64)) {
            return s.clone();
        }
        let mut header = [0u8; 0x50];
        mem.read(base + start as u64, &mut header);
        let made = match Sph::parse(&header) {
            None => Err(format!("no shader header at {start:#x}")),
            Some(sph) => {
                let entry = start + 0x50;
                // addresses inside a program count from its first instruction's block
                Program::decode_at(entry, &mut |a| {
                    let mut b = [0; 8];
                    mem.read(base + a as u64, &mut b);
                    Some(u64::from_le_bytes(b))
                })
                .map(|program| Shader { program, sph })
            }
        };
        let s = match made {
            Ok(s) => Some(Rc::new(s)),
            Err(e) => {
                self.error(e);
                None
            }
        };
        self.shaders.insert(base + start as u64, s.clone());
        s
    }
}

/// What every stage's host needs.
pub struct Common<'a> {
    pub mem: &'a mut dyn Memory,
    pub words: Rc<[Words; 18]>,
    pub textures: &'a mut HashMap<u32, Rc<Texture>>,
    pub samplers: &'a mut HashMap<u32, Rc<Sampler>>,
    pub cache: &'a mut TexCache,
    pub tic: u64,
    pub tsc: u64,
    pub tex_cb: u32,
    pub stored: bool,
}

impl Common<'_> {
    #[inline(always)]
    pub fn cbuf(&self, index: u32, offset: u32) -> u32 {
        self.words.get(index as usize).map_or(0, |w| w.get(offset, &*self.mem))
    }

    fn texture_at(&mut self, id: u32) -> Rc<Texture> {
        if let Some(t) = self.textures.get(&id) {
            return t.clone();
        }
        let mut raw = [0u8; 32];
        self.mem.read(self.tic + 32 * id as u64, &mut raw);
        let w: [u32; 8] = std::array::from_fn(|k| u32::from_le_bytes(raw[4 * k..4 * k + 4].try_into().unwrap()));
        let t = Rc::new(Texture::from_words(&w).unwrap_or_else(|_| Texture::empty()));
        self.textures.insert(id, t.clone());
        t
    }

    fn sampler_at(&mut self, id: u32) -> Rc<Sampler> {
        if let Some(s) = self.samplers.get(&id) {
            return s.clone();
        }
        let mut raw = [0u8; 32];
        self.mem.read(self.tsc + 32 * id as u64, &mut raw);
        let w: [u32; 8] = std::array::from_fn(|k| u32::from_le_bytes(raw[4 * k..4 * k + 4].try_into().unwrap()));
        let s = Rc::new(Sampler::from_words(&w));
        self.samplers.insert(id, s.clone());
        s
    }

    pub fn texture(&mut self, op: &TexOp<L>) -> [[u32; L]; 4] {
        let mut out = [[0; L]; 4];
        for q in 0..L / 4 {
            let active = (op.lanes >> (4 * q) & 15) as u8;
            if active == 0 {
                continue;
            }
            let lanes: [usize; 4] = std::array::from_fn(|k| 4 * q + k);
            let first = lanes[active.trailing_zeros() as usize];
            // (a handle is the same for a quad's lanes, or the lanes are sampled by themselves)
            match op.handle {
                Handle::Bindless(h) if lanes.iter().any(|&l| active >> (l - 4 * q) & 1 != 0 && h[l] != h[first]) => {
                    for (k, &l) in lanes.iter().enumerate().filter(|(k, _)| active >> k & 1 != 0) {
                        self.quad(op, q, h[l], 1 << k, &mut out);
                    }
                }
                Handle::Bindless(h) => self.quad(op, q, h[first], active, &mut out),
                Handle::Bound(w) => {
                    let h = self.cbuf(self.tex_cb, 4 * w);
                    self.quad(op, q, h, active, &mut out);
                }
            }
        }
        out
    }

    fn quad(&mut self, op: &TexOp<L>, q: usize, handle: u32, active: u8, out: &mut [[u32; L]; 4]) {
        let tex = self.texture_at(handle & 0xf_ffff);
        let at = |v: &[u32; L]| -> [u32; 4] { std::array::from_fn(|k| v[4 * q + k]) };
        let fl = |v: &[u32; L]| at(v).map(f32::from_bits);
        let mut put = |r: [[u32; 4]; 4]| {
            for (k, t) in r.iter().enumerate() {
                if active >> k & 1 != 0 {
                    (0..4).for_each(|c| out[c][4 * q + k] = t[c]);
                }
            }
        };
        let offset: [[i8; 4]; 3] = std::array::from_fn(|c| std::array::from_fn(|k| op.offset[c][4 * q + k] as i8));
        match op.kind {
            TexKind::Query => {
                let lod = at(&op.lod);
                let r = std::array::from_fn(|k| match op.query {
                    2 => maxwell_tex::sample::query_type(&tex),
                    _ => maxwell_tex::sample::query_dimensions(&tex, lod[k]),
                });
                put(r);
            }
            TexKind::Fetch => {
                let p: [[u32; 4]; 3] = std::array::from_fn(|c| at(&op.coord[c]));
                let (layer, lod, sample) = (at(&op.layer), at(&op.lod), at(&op.sample));
                let mem = &*self.mem;
                let cache = &mut *self.cache;
                let r = std::array::from_fn(|k| {
                    if active >> k & 1 == 0 {
                        return [0; 4];
                    }
                    let level = if op.lod_mode == LodMode::Level { lod[k] } else { 0 };
                    let pos = std::array::from_fn(|c| p[c][k] as i32 + offset[c][k] as i32);
                    maxwell_tex::sample::fetch(&tex, cache, mem, pos, if op.array { layer[k] } else { 0 }, level, op.ms.then_some(sample[k]))
                });
                put(r);
            }
            _ => {
                let smp = self.sampler_at(handle >> 20);
                let lod = match op.lod_mode {
                    _ if op.kind == TexKind::Grad => {
                        let g = |v: &[[f32; L]; 2]| -> [[f32; 4]; 3] { std::array::from_fn(|c| std::array::from_fn(|k| v.get(c).map_or(0.0, |v| v[4 * q + k]))) };
                        Lod::Grad { dx: g(&op.ddx), dy: g(&op.ddy) }
                    }
                    LodMode::Auto => Lod::Auto,
                    LodMode::Zero => Lod::Zero,
                    LodMode::Bias => Lod::Bias(fl(&op.lod)),
                    LodMode::Level => Lod::Level(fl(&op.lod)),
                };
                let a = Args {
                    coord: std::array::from_fn(|c| fl(&op.coord[c])),
                    layer: if op.array { at(&op.layer) } else { [0; 4] },
                    lod,
                    offset,
                    compare: op.shadow.then(|| std::array::from_fn(|k| op.dref[4 * q + k])),
                    active,
                };
                let r = match op.kind {
                    TexKind::QueryLod => maxwell_tex::sample::query_lod(&tex, &smp, &a),
                    TexKind::Gather => {
                        let per = op.per_texel.is_some().then(|| {
                            std::array::from_fn::<[[i8; 2]; 4], 4, _>(|k| std::array::from_fn(|t| op.gather_offset(4 * q + k, t).map_or([0, 0], |(x, y)| [x as i8, y as i8])))
                        });
                        maxwell_tex::sample::gather(&tex, &smp, self.cache, &*self.mem, &a, op.comp as u32, per.as_ref())
                    }
                    _ => maxwell_tex::sample::sample(&tex, &smp, self.cache, &*self.mem, &a),
                };
                put(r);
            }
        }
    }

    fn surface(&mut self, op: &SurfOp<L>, l: usize) -> (Rc<Texture>, [i32; 3], u32) {
        let h = match op.handle {
            Handle::Bindless(h) => h[l],
            Handle::Bound(w) => self.cbuf(self.tex_cb, 4 * w),
        };
        let t = self.texture_at(h & 0xf_ffff);
        let c = |k: usize| op.coord[k][l] as i32;
        let (p, layer) = match op.dim {
            SurfDim::D1 | SurfDim::Buffer => ([c(0), 0, 0], 0),
            SurfDim::D1Array => ([c(0), 0, 0], c(1) as u32),
            SurfDim::D2 => ([c(0), c(1), 0], 0),
            SurfDim::D2Array => ([c(0), c(1), 0], c(2) as u32),
            SurfDim::D3 => ([c(0), c(1), c(2)], 0),
        };
        (t, p, layer)
    }

    pub fn surface_load(&mut self, op: &SurfOp<L>) -> [[u32; L]; 4] {
        let mut out = [[0; L]; 4];
        for l in (0..L).filter(|l| op.lanes >> l & 1 != 0) {
            let (t, p, layer) = self.surface(op, l);
            let v = if op.raw {
                let mut b = [0u8; 16];
                if let Some(a) = maxwell_tex::sample::image_address(&t, p, layer) {
                    self.mem.read(a, &mut b[..op.ty.bytes()]);
                }
                std::array::from_fn(|k| u32::from_le_bytes(b[4 * k..4 * k + 4].try_into().unwrap()))
            } else {
                maxwell_tex::sample::image_load(&t, &*self.mem, p, layer)
            };
            (0..4).for_each(|k| out[k][l] = v[k]);
        }
        out
    }

    pub fn surface_store(&mut self, op: &SurfOp<L>, data: &[[u32; L]; 4]) {
        self.stored = true;
        for l in (0..L).filter(|l| op.lanes >> l & 1 != 0) {
            let (t, p, layer) = self.surface(op, l);
            let v: [u32; 4] = std::array::from_fn(|k| data[k][l]);
            if op.raw {
                if let Some(a) = maxwell_tex::sample::image_address(&t, p, layer) {
                    let b: Vec<u8> = v.iter().flat_map(|w| w.to_le_bytes()).collect();
                    self.mem.write(a, &b[..op.ty.bytes()]);
                }
            } else {
                maxwell_tex::sample::image_store(&t, self.mem, p, layer, v);
            }
        }
        self.cache.invalidate();
    }

    pub fn surface_atomic(&mut self, op: &SurfOp<L>, atom: maxwell_shader::isa::AtomOp, ty: maxwell_shader::isa::AtomTy, a: &[u64; L], b: &[u64; L]) -> [u64; L] {
        self.stored = true;
        let mut out = [0; L];
        for l in (0..L).filter(|l| op.lanes >> l & 1 != 0) {
            let (t, p, layer) = self.surface(op, l);
            if let Some(at) = maxwell_tex::sample::image_address(&t, p, layer) {
                let n = ty.bytes();
                let mut w = [0; 8];
                self.mem.read(at, &mut w[..n]);
                out[l] = u64::from_le_bytes(w);
                let new = maxwell_shader::atomic_apply(atom, ty, out[l], a[l], b[l]);
                self.mem.write(at, &new.to_le_bytes()[..n]);
            }
        }
        self.cache.invalidate();
        out
    }
}

/// The parts of `Host` that are the same for every stage.
#[macro_export]
macro_rules! common_host {
    () => {
        #[inline(always)]
        fn cbuf(&self, index: u32, offset: u32) -> u32 {
            self.common.cbuf(index, offset)
        }
        fn load(&mut self, addr: u64, out: &mut [u8]) {
            self.common.mem.read(addr, out);
        }
        fn store(&mut self, addr: u64, data: &[u8]) {
            self.common.stored = true;
            self.common.mem.write(addr, data);
        }
        fn texture(&mut self, op: &TexOp<L>) -> [[u32; L]; 4] {
            self.common.texture(op)
        }
        fn surface_load(&mut self, op: &SurfOp<L>) -> [[u32; L]; 4] {
            self.common.surface_load(op)
        }
        fn surface_store(&mut self, op: &SurfOp<L>, data: &[[u32; L]; 4]) {
            self.common.surface_store(op, data)
        }
        fn surface_atomic(&mut self, op: &SurfOp<L>, atom: maxwell_shader::isa::AtomOp, ty: maxwell_shader::isa::AtomTy, a: &[u64; L], b: &[u64; L]) -> [u64; L] {
            self.common.surface_atomic(op, atom, ty, a, b)
        }
    };
}

/// A vertex shader's surroundings: its attributes are fetched already.
struct VertexHost<'a, 'm> {
    common: &'a mut Common<'m>,
    inputs: &'a [[[u32; 4]; 32]; L],
    vertex: [u32; L],
    instance: u32,
    out: &'a mut [[u32; WORDS]],
}

impl Host<L> for VertexHost<'_, '_> {
    common_host!();

    fn attr_load(&mut self, lane: usize, a: Attr) -> u32 {
        match a.addr {
            0x80..=0x27c => self.inputs[lane][(a.addr as usize - 0x80) / 16][(a.addr as usize / 4) & 3],
            0x2f8 => self.instance,
            0x2fc => self.vertex[lane],
            _ => 0,
        }
    }

    fn attr_store(&mut self, lane: usize, a: Attr, v: u32) {
        if let (Some(w), Some(o)) = (word(a.addr), self.out.get_mut(lane)) {
            o[w] = v;
        }
    }
}

/// A stream's state.
#[derive(Clone, Copy, Default)]
struct Stream {
    on: bool,
    at: u64,
    limit: u64,
    stride: u64,
    /// per instance, every so many
    divisor: Option<u32>,
}

const FUEL: u64 = 4_000_000;

/// The constant buffers of stage `s`, ready.
fn stage_words(t: &mut Threed, s: usize) -> Rc<[Words; 18]> {
    let w = t.pipe.words[s].clone();
    for (k, b) in t.cbufs[s].iter().enumerate() {
        w[k].bind(*b);
    }
    w
}

pub fn draw(t: &mut Threed, indices: Indices, mem: &mut dyn Memory) {
    if std::mem::take(&mut t.pipe.cbufs_changed) {
        t.pipe.words.iter().for_each(|s| s.iter().for_each(|w| w.forget()));
    }
    // split at the restart index
    let restart = (t.r(0x1644) & 1 != 0).then(|| t.r(0x1648));
    match indices {
        Indices::Range(first, n) => {
            let v: Vec<u32> = (0..n).map(|k| first.wrapping_add(k)).collect();
            strip(t, &v, true, mem);
        }
        Indices::List(list) => match restart {
            Some(r) => list.split(|&i| i == r).filter(|s| !s.is_empty()).for_each(|s| strip(t, s, false, mem)),
            None => strip(t, list, false, mem),
        },
    }
}

/// Draw one run of vertices.
fn strip(t: &mut Threed, list: &[u32], arrays: bool, mem: &mut dyn Memory) {
    let base = t.address(0x1608);
    let stage = |t: &Threed, k: u32| (t.r(0x2000 + 0x40 * k) & 1 != 0).then(|| t.r(0x2004 + 0x40 * k));
    let Some(vs) = stage(t, 1).and_then(|s| t.pipe.shader(base, s, mem)) else { return };
    let fs = stage(t, 5).and_then(|s| t.pipe.shader(base, s, mem));
    let (verts, remap) = shade_vertices(t, &vs, list, arrays, mem);
    t.counters.vertices += list.len() as u64;
    // (tessellation and geometry shaders change what is rasterized)
    let mut out = raster::Geometry { vertices: verts, primitives: Vec::new(), size: 0 };
    let first = t.r(0x1684) & 1 == 0;
    let op = t.batch.op;
    vertex::assemble(op, list.len(), first, |p| {
        out.size = p.len();
        out.primitives.extend(p.iter().map(|&k| remap[k as usize]));
    });
    if out.primitives.is_empty() {
        return;
    }
    // adjacency is for geometry shaders: without one it is dropped
    let mut out = match op {
        10 | 11 => out.pick(&[1, 2]),
        12 | 13 => out.pick(&[0, 2, 4]),
        _ => out,
    };
    if op == 14 {
        return; // patches: not yet
    }
    t.counters.primitives += (out.primitives.len() / out.size.max(1)) as u64;
    if t.r(0x37c) & 1 == 0 {
        return;
    }
    // quads, strips of them and polygons are triangles here
    rasterize(t, &mut out, fs, mem);
}

/// Shade every vertex of the list once; returns them and where each list entry is.
fn shade_vertices(t: &mut Threed, vs: &Rc<Shader>, list: &[u32], arrays: bool, mem: &mut dyn Memory) -> (Vec<[u32; WORDS]>, Vec<u32>) {
    let mut unique: Vec<u32> = Vec::new();
    let remap: Vec<u32> = if arrays {
        unique.extend_from_slice(list);
        (0..list.len() as u32).collect()
    } else {
        let mut seen: HashMap<u32, u32> = HashMap::with_capacity(list.len());
        list.iter()
            .map(|&i| {
                *seen.entry(i).or_insert_with(|| {
                    unique.push(i);
                    unique.len() as u32 - 1
                })
            })
            .collect()
    };
    let streams: [Stream; 32] = std::array::from_fn(|j| {
        let j = j as u32;
        let f = t.r(0x1c00 + 16 * j);
        Stream {
            on: f >> 12 & 1 != 0,
            at: t.address(0x1c04 + 16 * j),
            limit: t.address(0x1f00 + 8 * j),
            stride: (f & 0xfff) as u64,
            divisor: (t.r(0x1880 + 4 * j) & 1 != 0).then(|| t.r(0x1c0c + 16 * j)),
        }
    });
    let attributes: Vec<(usize, Attribute)> = (0..32).filter(|&k| (0..4).any(|c| vs.sph.reads(0x80 + 16 * k as u32 + 4 * c))).map(|k| (k, Attribute::new(t.r(0x1160 + 4 * k as u32)))).collect();
    let (element_base, id_base) = (if arrays { 0 } else { t.r(0x1434) }, if arrays { 0 } else { t.r(0x1434) });
    let instance_base = t.r(0x1438);
    let instance = t.batch.instance;
    let words = stage_words(t, 0);
    let mut out = vec![[0u32; WORDS]; unique.len()];
    for v in out.iter_mut() {
        v[POSITION + 3] = 1f32.to_bits();
    }
    let (tic, tsc, tex_cb) = (t.address(0x1574), t.address(0x155c), t.r(0x2608));
    let p = &mut t.pipe;
    let constants = p.constants;
    p.lanes.set_local_size((vs.sph.local_low + vs.sph.local_high + vs.sph.local_crs) as usize);
    let mut common = Common { mem, words, textures: &mut p.textures, samplers: &mut p.samplers, cache: &mut p.tex_cache, tic, tsc, tex_cb, stored: false };
    let mut inputs = [[[0u32; 4]; 32]; L];
    let mut failed = None;
    for (chunk, outs) in unique.chunks(L).zip(out.chunks_mut(L)) {
        for (lane, &i) in chunk.iter().enumerate() {
            for &(k, a) in &attributes {
                let s = &streams[a.stream];
                inputs[lane][k] = if a.constant || !s.on {
                    constants[k]
                } else {
                    let e = match s.divisor {
                        Some(0) => instance_base as u64,
                        Some(d) => instance_base as u64 + (instance / d) as u64,
                        None => i.wrapping_add(element_base) as u64,
                    };
                    let at = s.at + e * s.stride + a.offset as u64;
                    if at > s.limit { [0; 4] } else { a.fetch(at, &*common.mem) }
                };
            }
        }
        let vertex = std::array::from_fn(|l| chunk.get(l).copied().unwrap_or(0).wrapping_add(id_base));
        p.lanes.start(&vs.program, (1u32 << chunk.len()).wrapping_sub(1), 0);
        let mut host = VertexHost { common: &mut common, inputs: &inputs, vertex, instance, out: outs };
        let mut fuel = FUEL;
        let e = maxwell_shader::run(&vs.program, &mut p.lanes, &mut host, &mut fuel);
        if !matches!(e, Exit::Done) {
            failed = Some(format!("vertex shader stopped: {e:?}"));
        }
    }
    if let Some(e) = failed {
        t.pipe.error(e);
    }
    (out, remap)
}

/// What the rasterizer needs to know about a fragment shader's surroundings.
pub struct Setup {
    pub targets: Targets,
    pub rect: Rect,
    pub ds: DepthStencil,
    pub outputs: Vec<(usize, Output)>,
    pub constant: [f32; 4],
    pub logic: Option<u32>,
    pub fs: Option<Rc<Shader>>,
    /// depth and stencil are tested before shading
    pub early: bool,
    pub alpha_test: Option<(u32, f32)>,
    pub sample_mask: u32,
    pub samples: u32,
    pub positions: [(i32, i32); 16],
    pub per_sample: bool,
    pub alpha_to_coverage: bool,
    pub dual: bool,
    pub two_sided: bool,
    pub clamp_z: bool,
    pub zrange: (f32, f32),
    /// the attribute words the shader interpolates, and how
    pub inputs: Vec<(usize, Interp)>,
    pub need: u32,
    pub write: u32,
    pub sprite: u32,
    pub flat: bool,
}

fn rasterize(t: &mut Threed, g: &mut raster::Geometry, fs: Option<Rc<Shader>>, mem: &mut dyn Memory) {
    let targets = Targets::new(t);
    let mut rect = targets.rect();
    if t.r(0xe00) & 1 != 0 {
        rect = rect.and(t.r(0xe04), t.r(0xe08));
    }
    if rect.empty() {
        return;
    }
    let ds = DepthStencil::new(t);
    let srgb = t.r(0x15b8) & 1 != 0;
    let mut outputs = Vec::new();
    let (mut need, mut write) = (0, 0);
    for (k, c) in targets.color.iter().enumerate() {
        let Some((_, format)) = c else { continue };
        let blend = Blend::new(t, k);
        if blend.mask & 0x1111 == 0 {
            continue;
        }
        let format = if srgb { *format } else { format.linear() };
        outputs.push((k, Output { format, blend, srgb_write: srgb }));
        need |= 1 << k;
        write |= 1 << k;
    }
    if targets.zeta.is_some() && ds.used() {
        need |= crate::rop::NEED_ZETA;
        write |= crate::rop::NEED_ZETA;
    }
    let samples = targets.ms.samples();
    let positions: [(i32, i32); 16] = std::array::from_fn(|s| {
        let p = t.r(0x11e0 + 4 * (s as u32 / 4)) >> (8 * (s as u32 % 4));
        if samples == 1 {
            return (128, 128);
        }
        let (x, y) = targets.ms.positions().get(s).copied().unwrap_or((8, 8));
        let _ = p;
        (x as i32 * 16, y as i32 * 16)
    });
    let sph = fs.as_ref().map(|f| &f.sph);
    let out = sph.map(|s| s.frag_outputs());
    let alpha_test = (t.r(0x12ec) & 1 != 0).then(|| (t.r(0x1314), t.f(0x1310)));
    let alpha_to_coverage = t.r(0x153c) & 1 != 0 && samples > 1;
    let forced = t.r(0x210) & 1 != 0;
    let changes = sph.is_some_and(|s| s.kills_pixels) || out.as_ref().is_some_and(|o| o.depth.is_some() || o.sample_mask.is_some()) || alpha_test.is_some() || alpha_to_coverage;
    let mut sample_mask = 0;
    for s in 0..samples {
        // one word per pixel of a 2 × 2 block; the first serves all
        sample_mask |= t.r(0xfbc) & 1 << s;
    }
    if samples == 1 || t.r(0x1534) & 1 == 0 {
        sample_mask = u32::MAX;
    }
    let inputs = match &fs {
        None => Vec::new(),
        Some(f) => (0..WORDS).filter_map(|w| {
            let addr = FIRST + 4 * w as u32;
            let i = f.sph.interp(addr);
            (f.sph.reads(addr) && addr >= 0x80).then_some((w, i))
        }).collect(),
    };
    let dual = outputs.iter().any(|(_, o)| o.blend.on && o.blend.rgb[1..].iter().chain(&o.blend.alpha[1..]).any(|f| matches!(f & 0xfff, 0x900..=0x903)));
    let s = Setup {
        rect,
        early: forced || !changes,
        ds,
        outputs,
        constant: std::array::from_fn(|k| t.f(0x131c + 4 * k as u32)),
        logic: (t.r(0x19c4) & 1 != 0).then(|| t.r(0x19c8)),
        alpha_test,
        sample_mask,
        samples,
        positions,
        per_sample: t.r(0x754) & 0x10 != 0 && samples > 1,
        alpha_to_coverage,
        dual,
        two_sided: t.r(0x1688) & 1 != 0,
        clamp_z: t.r(0x193c) & 0x18 != 0,
        zrange: { let (a, b) = (t.f(0xc08), t.f(0xc0c)); (a.min(b), a.max(b)) },
        inputs,
        need,
        write,
        sprite: if t.r(0x1520) & 1 != 0 { t.r(0x1604) } else { 0 },
        flat: t.r(0x12d4) == 0x1d00,
        targets,
        fs,
    };
    if s.need == 0 && s.fs.as_ref().is_none_or(|f| !f.sph.global_store) && t.r(0x1514) & 1 == 0 {
        return;
    }
    t.rop.begin(&s.targets);
    let words = stage_words(t, 4);
    raster::draw(t, g, &s, words, mem);
    let _ = (LAYER, POINT_SIZE);
}

impl Threed {
    /// The pieces a fragment shader's host needs, apart from the render targets.
    pub(crate) fn fragment_parts<'a>(&'a mut self, words: Rc<[Words; 18]>, mem: &'a mut dyn Memory) -> (Common<'a>, &'a mut Lanes<L>, &'a mut crate::rop::Rop, &'a mut crate::threed::Counters) {
        let (tic, tsc, tex_cb) = (self.address(0x1574), self.address(0x155c), self.r(0x2608));
        let p = &mut self.pipe;
        (Common { mem, words, textures: &mut p.textures, samplers: &mut p.samplers, cache: &mut p.tex_cache, tic, tsc, tex_cb, stored: false }, &mut p.lanes, &mut self.rop, &mut self.counters)
    }
}

/// A fragment shader's surroundings: quads of one primitive.
pub struct FragmentHost<'a, 'm> {
    pub common: &'a mut Common<'m>,
    pub setup: &'a Setup,
    pub prim: &'a raster::Primitive,
    /// each quad's first pixel
    pub quads: [(i32, i32); L / 4],
    /// each lane's covered samples
    pub coverage: [u32; L],
    /// the sample being shaded, with per-sample shading
    pub sample: Option<u32>,
}

impl FragmentHost<'_, '_> {
    /// A lane's sample point, in pixels.
    #[inline(always)]
    fn at(&self, l: usize, offset: (f32, f32)) -> (f32, f32) {
        let (qx, qy) = self.quads[l / 4];
        let (cx, cy) = match self.sample {
            Some(s) => {
                let (x, y) = self.setup.positions[s as usize];
                (x as f32 / 256.0, y as f32 / 256.0)
            }
            None => (0.5, 0.5),
        };
        ((qx + (l & 1) as i32) as f32 + cx + offset.0, (qy + (l >> 1 & 1) as i32) as f32 + cy + offset.1)
    }
}

impl Host<L> for FragmentHost<'_, '_> {
    common_host!();

    fn sysval(&mut self, sys: u8) -> [u32; L] {
        match sys {
            0x12 => [1f32.to_bits(); L], // the direction of y
            _ => [0; L],
        }
    }

    fn interpolate(&mut self, op: &IpaOp<L>) -> [u32; L] {
        let p = self.prim;
        let centroid = op.loc == IpaLoc::Centroid && self.sample.is_none() && self.setup.samples > 1;
        let at = |l: usize, h: &Self| -> (f32, f32) {
            if op.loc == IpaLoc::Offset {
                let (qx, qy) = h.quads[l / 4];
                return ((qx + (l & 1) as i32) as f32 + 0.5 + op.offset[0][l], (qy + (l >> 1 & 1) as i32) as f32 + 0.5 + op.offset[1][l]);
            }
            let all = (1u32 << h.setup.samples) - 1;
            if centroid && h.coverage[l] & all != all && h.coverage[l] & all != 0 {
                let s = h.coverage[l].trailing_zeros() as usize;
                let (x, y) = h.setup.positions[s];
                let (qx, qy) = h.quads[l / 4];
                return ((qx + (l & 1) as i32) as f32 + x as f32 / 256.0, (qy + (l >> 1 & 1) as i32) as f32 + y as f32 / 256.0);
            }
            h.at(l, (0.0, 0.0))
        };
        match op.addr {
            0x70 => return std::array::from_fn(|l| at(l, self).0.to_bits()),
            0x74 => return std::array::from_fn(|l| at(l, self).1.to_bits()),
            0x78 => return std::array::from_fn(|l| { let (x, y) = at(l, self); p.z.at(x, y).to_bits() }),
            0x7c => return std::array::from_fn(|l| { let (x, y) = at(l, self); p.w.at(x, y).to_bits() }),
            0x3fc => return [if p.front { u32::MAX } else { 0 }; L],
            0x60 => return [p.id; L],
            0x64 => return [p.layer; L],
            0x2e0 | 0x2e4 => {
                let pl = if op.addr == 0x2e0 { &p.point[0] } else { &p.point[1] };
                return std::array::from_fn(|l| {
                    let (x, y) = at(l, self);
                    (pl.at(x, y) * if op.mode == IpaMode::MulW { p.w.at(x, y) } else { 1.0 }).to_bits()
                });
            }
            _ => {}
        }
        // two-sided colours
        let mut addr = op.addr;
        if self.setup.two_sided && !p.front && (0x280..0x2a0).contains(&addr) {
            addr += 0x20;
        }
        let Some(w) = word(addr) else { return [0; L] };
        // a point sprite's coordinate takes a generic's place
        if p.is_point && (0x80..0x100).contains(&addr) {
            let g = (addr - 0x80) / 16;
            if self.setup.sprite >> (3 + g) & 1 != 0 {
                return match addr >> 2 & 3 {
                    c @ (0 | 1) => std::array::from_fn(|l| {
                        let (x, y) = at(l, self);
                        (p.point[c as usize].at(x, y) * if op.mode == IpaMode::MulW { p.w.at(x, y) } else { 1.0 }).to_bits()
                    }),
                    3 => std::array::from_fn(|l| { let (x, y) = at(l, self); (if op.mode == IpaMode::MulW { p.w.at(x, y) } else { 1.0 }).to_bits() }),
                    _ => [0; L],
                };
            }
        }
        match &p.planes[w] {
            raster::Plane::None => [0; L],
            raster::Plane::Flat(v) => {
                // a shader that multiplies with w gets the value over w
                if op.mode == IpaMode::MulW {
                    let v = f32::from_bits(*v);
                    return std::array::from_fn(|l| { let (x, y) = at(l, self); (v * p.w.at(x, y)).to_bits() });
                }
                [*v; L]
            }
            raster::Plane::Linear(pl) => {
                if op.mode == IpaMode::Constant {
                    return [p.flat[w]; L];
                }
                std::array::from_fn(|l| { let (x, y) = at(l, self); pl.at(x, y).to_bits() })
            }
        }
    }

    fn pixld(&mut self, what: Pixld, arg: &[u32; L]) -> [u32; L] {
        match what {
            Pixld::Count => [self.setup.samples; L],
            Pixld::CovMask | Pixld::Covered => std::array::from_fn(|l| match self.sample {
                Some(s) => self.coverage[l] & 1 << s,
                None => self.coverage[l],
            }),
            Pixld::SampleId => [self.sample.unwrap_or(0); L],
            Pixld::Offset => std::array::from_fn(|l| {
                let (x, y) = self.setup.positions[arg[l] as usize & 15];
                (x as u32 >> 4 & 15) | (y as u32 >> 4 & 15) << 16
            }),
            _ => [0; L],
        }
    }
}
