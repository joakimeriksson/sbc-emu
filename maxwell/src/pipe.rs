//! The graphics pipeline of a draw: vertices are fetched and shaded, primitives assembled, clipped
//! and rasterized, fragments shaded and written.
use crate::raster::{self, Geometry, Prim, Scene, Slot, Verts};
use crate::rop::{Blend, DepthStencil, Output, Rect, Targets};
use crate::threed::{Cbuf, Threed};
use crate::vertex::{self, Attribute};
use crate::Memory;
use maxwell_shader::isa::{Attr, IpaLoc, IpaMode, LodMode, Pixld, SurfDim, TexKind};
use maxwell_shader::sph::{FragOutputs, Interp};
use maxwell_shader::{Exit, Handle, Host, IpaOp, Lanes, Program, Sph, SurfOp, TexOp};
use maxwell_tex::{Args, Lod, Sampler, TexCache, Texture};
use std::cell::Cell;
use std::collections::HashMap;
use std::sync::Arc;

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
pub const POINT_SIZE: usize = (0x6c - 0x40) / 4;
pub const LAYER: usize = (0x64 - 0x40) / 4;
pub const CLIP: usize = (0x2c0 - 0x40) / 4;

pub enum Indices<'a> {
    Range(u32, u32),
    List(&'a [u32]),
}

pub struct Shader {
    pub program: Program,
    pub sph: Sph,
}

/// The most local memory a lane gets, in bytes.
pub const MAX_LOCAL: usize = 1 << 19;

impl Shader {
    /// Bytes of local memory of each lane.
    pub fn local_size(&self) -> usize {
        // (the hardware has no more, and neither do we)
        ((self.sph.local_low + self.sph.local_high + self.sph.local_crs) as usize).min(MAX_LOCAL)
    }
}

/// A constant buffer's words, read when they are first used.
#[derive(Default)]
pub struct Words {
    buffer: Cbuf,
    /// what they were read in
    generation: u64,
    words: Box<[Cell<u32>]>,
    valid: [Cell<u64>; 4],
}

impl Words {
    fn bind(&mut self, b: Cbuf, generation: u64) {
        if self.buffer != b || self.generation != generation {
            (self.buffer, self.generation) = (b, generation);
            let n = (b.size.min(0x1_0000) as usize).div_ceil(256) * 64;
            if self.words.len() < n {
                self.words = vec![Cell::new(0); n].into_boxed_slice();
            }
            self.valid.iter().for_each(|v| v.set(0));
        }
    }

    #[inline]
    fn get(&self, offset: u32, mem: &dyn Memory) -> u32 {
        let b = self.buffer;
        if offset >= b.size || offset >= 0x1_0000 {
            return 0;
        }
        // (256 bytes at a time)
        let w = offset as usize / 4;
        let (block, bit) = (w / 64, 1u64 << (w / 64 % 64));
        let valid = &self.valid[block / 64];
        if valid.get() & bit == 0 {
            let mut raw = [0u8; 256];
            mem.read(b.address + 256 * block as u64, &mut raw);
            for (k, c) in raw.chunks_exact(4).enumerate() {
                self.words[64 * block + k].set(u32::from_le_bytes(c.try_into().unwrap()));
            }
            valid.set(valid.get() | bit);
        }
        self.words[w].get()
    }
}

/// Host threads for shading (`SBC_MAXWELL_THREADS`; by default the host's cores minus two, for the
/// CPU cores). The browser build has no threads.
pub fn threads() -> usize {
    #[cfg(target_arch = "wasm32")]
    {
        1
    }
    #[cfg(not(target_arch = "wasm32"))]
    {
        static N: std::sync::OnceLock<usize> = std::sync::OnceLock::new();
        *N.get_or_init(|| match std::env::var("SBC_MAXWELL_THREADS").ok().and_then(|v| v.parse::<usize>().ok()) {
            Some(n) => n.clamp(1, 64),
            None => std::thread::available_parallelism().map_or(1, |n| n.get()).saturating_sub(2).clamp(1, 16),
        })
    }
}

/// Decoded descriptors, by number.
struct Cache<T>(Box<[Option<(u32, Arc<T>)>; 64]>);

impl<T> Default for Cache<T> {
    fn default() -> Self {
        Self(Box::new(std::array::from_fn(|_| None)))
    }
}

impl<T> Cache<T> {
    #[inline]
    fn get(&self, id: u32) -> Option<Arc<T>> {
        self.0[id as usize % 64].as_ref().filter(|e| e.0 == id).map(|e| e.1.clone())
    }

    fn insert(&mut self, id: u32, t: Arc<T>) {
        self.0[id as usize % 64] = Some((id, t));
    }

    fn clear(&mut self) {
        self.0.fill_with(|| None);
    }
}

/// What one thread keeps: shader registers and its own caches of memory.
pub struct Worker {
    pub lanes: Lanes<L>,
    words: Box<[[Words; 18]; 6]>,
    textures: Cache<Texture>,
    samplers: Cache<Sampler>,
    cache: TexCache,
    generation: u64,
    /// a shader wrote memory
    pub stored: bool,
    /// compute's warps, kept from launch to launch (`Vec<Lanes<N>>`), of 16 and of 32 lanes
    pub warps: [Option<Box<dyn std::any::Any + Send>>; 2],
}

impl Worker {
    fn new() -> Self {
        Self {
            warps: [None, None],
            lanes: Lanes::new(),
            words: Box::new(std::array::from_fn(|_| std::array::from_fn(|_| Words::default()))),
            textures: Cache::default(),
            samplers: Cache::default(),
            cache: TexCache::new(),
            generation: 0,
            stored: false,
        }
    }

    /// The registers and a shader's surroundings.
    pub fn parts<'a>(&'a mut self, work: &Work, mem: &'a mut dyn Memory) -> (&'a mut Lanes<L>, Common<'a>) {
        if self.generation != work.textures {
            self.generation = work.textures;
            self.textures.clear();
            self.samplers.clear();
            self.cache.invalidate();
        }
        let words = &mut self.words[work.stage];
        for (w, b) in words.iter_mut().zip(&work.cbufs) {
            w.bind(*b, work.constants);
        }
        let common = Common { mem, words, textures: &mut self.textures, samplers: &mut self.samplers, cache: &mut self.cache, tic: work.tic, tsc: work.tsc, tex_cb: work.tex_cb, linked: work.linked, stored: false };
        (&mut self.lanes, common)
    }
}

/// A stage's resources in a draw.
#[derive(Clone)]
pub struct Work {
    stage: usize,
    cbufs: [Cbuf; 18],
    /// generations of the textures' and the constants' contents
    textures: u64,
    constants: u64,
    tic: u64,
    tsc: u64,
    tex_cb: u32,
    /// SET_SAMPLER_BINDING VIA_HEADER_BINDING: a texture's sampler has its number
    linked: bool,
}

pub struct Pipe {
    shaders: HashMap<u64, Option<Arc<Shader>>>,
    programs: HashMap<u64, Option<Arc<Program>>>,
    /// constant values of attributes without a stream
    pub constants: [[u32; 4]; 32],
    pub cbufs_changed: bool,
    constant_generation: u64,
    texture_generation: u64,
    pub workers: Vec<Worker>,
    /// primitives by tile, empty between draws
    pub bins: Vec<Vec<u32>>,
    pub errors: std::collections::BTreeSet<String>,
    /// Volta-and-later shaders (Tegra234's Ampere): 128-bit SM70 code after a 128-byte header,
    /// each stage at its own 64-bit address, and buffer sizes instead of limit addresses.
    pub sm70: bool,
}

impl Default for Pipe {
    fn default() -> Self {
        Self {
            shaders: HashMap::new(),
            programs: HashMap::new(),
            constants: [[0, 0, 0, 1f32.to_bits()]; 32],
            cbufs_changed: true,
            constant_generation: 1,
            texture_generation: 1,
            workers: Vec::new(),
            bins: Vec::new(),
            errors: Default::default(),
            sm70: false,
        }
    }
}

impl Pipe {
    pub fn invalidate_textures(&mut self) {
        self.texture_generation += 1;
    }

    pub fn need_workers(&mut self, n: usize) {
        while self.workers.len() < n {
            self.workers.push(Worker::new());
        }
    }

    pub fn invalidate_shaders(&mut self) {
        self.shaders.clear();
        self.programs.clear();
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

    /// The compute shader at `entry` of the code segment at `base`.
    pub fn program(&mut self, base: u64, entry: u32, mem: &dyn Memory) -> Option<Arc<Program>> {
        if let Some(p) = self.programs.get(&(base + entry as u64)) {
            return p.clone();
        }
        let mut fetch = |a: u32| {
            let mut b = [0; 8];
            mem.read(base + a as u64, &mut b);
            Some(u64::from_le_bytes(b))
        };
        let made = if self.sm70 { Program::decode_sm70_at(entry, &mut fetch) } else { Program::decode_at(entry, &mut fetch) };
        if let (true, Ok(p)) = (self.sm70, &made) {
            report_sm70(p, base, mem);
        }
        let p = match made {
            Ok(p) => {
                static SEEN: std::sync::Mutex<Vec<u64>> = std::sync::Mutex::new(Vec::new());
                if std::env::var_os("SBC_SHADER_DUMP").is_some() {
                    let mut seen = SEEN.lock().unwrap();
                    if !seen.contains(&(base + entry as u64)) {
                        seen.push(base + entry as u64);
                        eprintln!("[shader dump] compute program at {:#x}:\n{p}[shader dump] end", base + entry as u64);
                    }
                }
                Some(Arc::new(p))
            }
            Err(e) => {
                self.error(e);
                None
            }
        };
        self.programs.insert(base + entry as u64, p.clone());
        p
    }

    /// The shader at `at` (its header first) of the code segment at `base`.
    fn shader(&mut self, base: u64, start: u32, mem: &dyn Memory) -> Option<Arc<Shader>> {
        if let Some(s) = self.shaders.get(&(base + start as u64)) {
            return s.clone();
        }
        let mut header = [0u8; 0x50];
        mem.read(base + start as u64, &mut header);
        let sm70 = self.sm70;
        let made = match Sph::parse(&header) {
            None => Err(format!("no shader header at {start:#x}")),
            Some(sph) => {
                let mut fetch = |a: u32| {
                    let mut b = [0; 8];
                    mem.read(base + a as u64, &mut b);
                    Some(u64::from_le_bytes(b))
                };
                // addresses inside a program count from its first instruction's block
                if sm70 {
                    let made = Program::decode_sm70_at(start + 0x80, &mut fetch);
                    if let Ok(p) = &made {
                        report_sm70(p, base + start as u64, mem);
                    }
                    made
                } else {
                    Program::decode_at(start + 0x50, &mut fetch)
                }
                .map(|program| Shader { program, sph })
            }
        };
        let s = match made {
            Ok(s) => Some(Arc::new(s)),
            Err(e) => {
                self.error(e);
                None
            }
        };
        self.shaders.insert(base + start as u64, s.clone());
        s
    }
}

/// SM70 code at `base`: instructions the decoder does not know, whole, for a disassembler; and
/// with SBC_DUMP_SHADERS=DIR the code as far as it was decoded (a graphics shader's header first).
fn report_sm70(p: &Program, base: u64, mem: &dyn Memory) {
    let read = |at: u64| {
        let mut b = [0; 8];
        mem.read(at, &mut b);
        u64::from_le_bytes(b)
    };
    for (inst, &(at, _)) in p.insts().iter().zip(p.words()) {
        if let maxwell_shader::isa::Op::Invalid(_) = inst.op {
            let (lo, hi) = (read(base + at as u64), read(base + at as u64 + 8));
            eprintln!("[maxwell] sm70 unknown at {base:#x}+{at:#x}: {hi:016x}{lo:016x}");
        }
    }
    if let Some(dir) = std::env::var_os("SBC_DUMP_SHADERS") {
        let end = p.words().iter().map(|w| w.0 + 16).max().unwrap_or(0);
        let mut bytes = vec![0; end as usize];
        mem.read(base, &mut bytes);
        let _ = std::fs::write(std::path::Path::new(&dir).join(format!("{base:x}.bin")), bytes);
    }
}

/// What every stage's host needs.
pub struct Common<'a> {
    pub mem: &'a mut dyn Memory,
    pub words: &'a [Words; 18],
    textures: &'a mut Cache<Texture>,
    samplers: &'a mut Cache<Sampler>,
    pub cache: &'a mut TexCache,
    pub tic: u64,
    pub tsc: u64,
    pub tex_cb: u32,
    pub linked: bool,
    pub stored: bool,
}

impl Common<'_> {
    /// A bound texture's handle.
    fn bound(&self, w: u32) -> u32 {
        if self.tex_cb == u32::MAX {
            self.cbuf(w >> 14, 4 * (w & 0x3fff))
        } else {
            self.cbuf(self.tex_cb, 4 * w)
        }
    }

    #[inline(always)]
    pub fn cbuf(&self, index: u32, offset: u32) -> u32 {
        self.words.get(index as usize).map_or(0, |w| w.get(offset, &*self.mem))
    }

    fn texture_at(&mut self, id: u32) -> Arc<Texture> {
        if let Some(t) = self.textures.get(id) {
            return t;
        }
        let mut raw = [0u8; 32];
        self.mem.read(self.tic + 32 * id as u64, &mut raw);
        let w: [u32; 8] = std::array::from_fn(|k| u32::from_le_bytes(raw[4 * k..4 * k + 4].try_into().unwrap()));
        let t = Arc::new(Texture::from_words(&w).unwrap_or_else(|_| Texture::empty()));
        self.textures.insert(id, t.clone());
        t
    }

    fn sampler_at(&mut self, id: u32) -> Arc<Sampler> {
        if let Some(s) = self.samplers.get(id) {
            return s;
        }
        let mut raw = [0u8; 32];
        self.mem.read(self.tsc + 32 * id as u64, &mut raw);
        let w: [u32; 8] = std::array::from_fn(|k| u32::from_le_bytes(raw[4 * k..4 * k + 4].try_into().unwrap()));
        let s = Arc::new(Sampler::from_words(&w));
        self.samplers.insert(id, s.clone());
        s
    }

    pub fn texture<const N: usize>(&mut self, op: &TexOp<N>) -> [[u32; N]; 4] {
        let mut out = [[0; N]; 4];
        for q in 0..N / 4 {
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
                    let h = self.bound(w);
                    self.quad(op, q, h, active, &mut out);
                }
            }
        }
        out
    }

    fn quad<const N: usize>(&mut self, op: &TexOp<N>, q: usize, handle: u32, active: u8, out: &mut [[u32; N]; 4]) {
        let tex = self.texture_at(handle & 0xf_ffff);
        let at = |v: &[u32; N]| -> [u32; 4] { std::array::from_fn(|k| v[4 * q + k]) };
        let fl = |v: &[u32; N]| at(v).map(f32::from_bits);
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
                    let pos = std::array::from_fn(|c| (p[c][k] as i32).wrapping_add(offset[c][k] as i32));
                    maxwell_tex::sample::fetch(&tex, cache, mem, pos, if op.array { layer[k] } else { 0 }, level, op.ms.then_some(sample[k]))
                });
                put(r);
            }
            _ => {
                let smp = self.sampler_at(if self.linked { handle & 0xfffff } else { handle >> 20 });
                let lod = match op.lod_mode {
                    _ if op.kind == TexKind::Grad => {
                        let g = |v: &[[f32; N]; 2]| -> [[f32; 4]; 3] { std::array::from_fn(|c| std::array::from_fn(|k| v.get(c).map_or(0.0, |v| v[4 * q + k]))) };
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

    fn surface<const N: usize>(&mut self, op: &SurfOp<N>, l: usize) -> (Arc<Texture>, [i32; 3], u32) {
        let h = match op.handle {
            Handle::Bindless(h) => h[l],
            Handle::Bound(w) => self.bound(w),
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

    pub fn surface_load<const N: usize>(&mut self, op: &SurfOp<N>) -> [[u32; N]; 4] {
        let mut out = [[0; N]; 4];
        for l in (0..N).filter(|l| op.lanes >> l & 1 != 0) {
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

    pub fn surface_store<const N: usize>(&mut self, op: &SurfOp<N>, data: &[[u32; N]; 4]) {
        self.stored = true;
        for l in (0..N).filter(|l| op.lanes >> l & 1 != 0) {
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

    pub fn surface_atomic<const N: usize>(&mut self, op: &SurfOp<N>, atom: maxwell_shader::isa::AtomOp, ty: maxwell_shader::isa::AtomTy, a: &[u64; N], b: &[u64; N]) -> [u64; N] {
        self.stored = true;
        let mut out = [0; N];
        for l in (0..N).filter(|l| op.lanes >> l & 1 != 0) {
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
    ($n:expr) => {
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
        fn load_lanes(&mut self, at: &[u64; $n], mask: u32, n: usize, out: &mut [[u8; 16]; $n]) {
            self.common.mem.read_lanes(at, mask, n, out);
        }
        fn store_lanes(&mut self, at: &[u64; $n], mask: u32, n: usize, data: &[[u8; 16]; $n]) {
            self.common.stored = true;
            self.common.mem.write_lanes(at, mask, n, data);
        }
        fn texture(&mut self, op: &TexOp<{ $n }>) -> [[u32; $n]; 4] {
            self.common.texture(op)
        }
        fn surface_load(&mut self, op: &SurfOp<{ $n }>) -> [[u32; $n]; 4] {
            self.common.surface_load(op)
        }
        fn surface_store(&mut self, op: &SurfOp<{ $n }>, data: &[[u32; $n]; 4]) {
            self.common.surface_store(op, data)
        }
        fn surface_atomic(&mut self, op: &SurfOp<{ $n }>, atom: maxwell_shader::isa::AtomOp, ty: maxwell_shader::isa::AtomTy, a: &[u64; $n], b: &[u64; $n]) -> [u64; $n] {
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
    layout: &'a Verts,
    out: &'a mut [u32],
}

impl Host<L> for VertexHost<'_, '_> {
    common_host!(L);

    fn attr_load(&mut self, lane: usize, a: Attr) -> u32 {
        match a.addr {
            0x80..=0x27c => self.inputs[lane][(a.addr as usize - 0x80) / 16][(a.addr as usize / 4) & 3],
            0x2f8 => self.instance,
            0x2fc => self.vertex[lane],
            _ => 0,
        }
    }

    fn attr_store(&mut self, lane: usize, a: Attr, v: u32) {
        if let Some(s) = word(a.addr).and_then(|w| self.layout.slot(w)) {
            if let Some(o) = self.out.get_mut(lane * self.layout.stride + s) {
                *o = v;
            }
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

impl Work {
    /// Compute's resources.
    pub fn compute(pipe: &mut Pipe, cbufs: [Cbuf; 18], tic: u64, tsc: u64, tex_cb: u32, linked: bool) -> Self {
        if std::mem::take(&mut pipe.cbufs_changed) {
            pipe.constant_generation += 1;
        }
        Self { stage: 5, cbufs, textures: pipe.texture_generation, constants: pipe.constant_generation, tic, tsc, tex_cb, linked }
    }
}

/// Stage `s`'s resources.
pub fn work(t: &Threed, s: usize) -> Work {
    Work {
        stage: s,
        cbufs: t.cbufs[s],
        textures: t.pipe.texture_generation,
        constants: t.pipe.constant_generation,
        tic: t.address(0x1574),
        tsc: t.address(0x155c),
        // SM70 bound textures name their constant buffer: `index` is buffer << 14 | word
        tex_cb: if t.pipe.sm70 { u32::MAX } else { t.r(0x2608) },
        linked: t.r(0x1234) & 1 != 0,
    }
}

pub fn draw(t: &mut Threed, indices: Indices, mem: &mut dyn Memory) {
    if std::mem::take(&mut t.pipe.cbufs_changed) {
        t.pipe.constant_generation += 1;
    }
    // split at the restart index; the primitives' numbers go on
    let restart = (t.r(0x1644) & 1 != 0).then(|| t.r(0x1648));
    let mut id = 0;
    match indices {
        Indices::Range(first, n) => {
            // points, lines, triangles and quads in parts; other lists have their limit
            let part = if matches!(t.batch.op, 0 | 1 | 4 | 7) { 3 << 20 } else { 1 << 25 };
            let mut at = 0;
            while at < n {
                let m = part.min(n - at);
                let v: Vec<u32> = (0..m).map(|k| first.wrapping_add(at + k)).collect();
                strip(t, &v, true, &mut id, mem);
                at += m;
                if part == 1 << 25 {
                    break;
                }
            }
        }
        Indices::List(list) => match restart {
            Some(r) => list.split(|&i| i == r).for_each(|s| strip(t, s, false, &mut id, mem)),
            None => strip(t, list, false, &mut id, mem),
        },
    }
}

/// Draw one run of vertices.
fn strip(t: &mut Threed, list: &[u32], arrays: bool, id: &mut u32, mem: &mut dyn Memory) {
    if list.is_empty() {
        return;
    }
    let base = t.address(0x1608);
    let stage = |t: &Threed, k: u32| (t.r(0x2000 + 0x40 * k) & 1 != 0).then(|| t.r(0x2004 + 0x40 * k));
    // SM70: SET_PIPELINE_PROGRAM_ADDRESS_A/B per stage
    let shader = |t: &mut Threed, k: u32| {
        if t.pipe.sm70 {
            let on = t.r(0x2000 + 0x40 * k) & 1 != 0;
            on.then(|| t.address(0x2014 + 0x40 * k)).and_then(|at| t.pipe.shader(at, 0, mem))
        } else {
            stage(t, k).and_then(|s| t.pipe.shader(base, s, mem))
        }
    };
    let Some(vs) = shader(t, 1) else { return };
    let (control, eval, geometry, fs) = (shader(t, 2), shader(t, 3), shader(t, 4), shader(t, 5));
    let (vertices, remap) = shade_vertices(t, &vs, list, arrays, mem);
    t.counters.vertices += list.len() as u64;
    let op = t.batch.op;
    let mut out;
    if op == 14 {
        let Some(eval) = eval else { return };
        out = crate::geom::tessellate(t, &vertices, &remap, t.r(0xdcc).clamp(1, 32) as usize, control.as_ref(), &eval, *id, mem);
        *id = id.wrapping_add(remap.len() as u32 / t.r(0xdcc).clamp(1, 32));
    } else {
        out = Geometry { vertices, primitives: Vec::new(), size: 0, first_id: *id, ids: None };
        let first = t.r(0x1684) & 1 == 0;
        vertex::assemble(op, list.len(), first, |p| {
            out.size = p.len();
            out.primitives.extend(p.iter().map(|&k| remap[k as usize]));
        });
        // quads are two triangles with one number
        if matches!(op, 7 | 8) {
            out.ids = Some((0..(out.primitives.len() / 3) as u32).map(|k| id.wrapping_add(k / 2)).collect());
            *id = id.wrapping_add(out.primitives.len() as u32 / 6);
        } else {
            *id = id.wrapping_add((out.primitives.len() / out.size.max(1)) as u32);
        }
    }
    if out.primitives.is_empty() {
        written(t);
        return;
    }
    t.counters.assembled += (out.primitives.len() / out.size) as u64;
    if let Some(g) = geometry {
        out = crate::geom::geometry(t, &out, &g, mem);
    } else {
        // adjacency is for geometry shaders
        out = match op {
            10 | 11 => out.pick(&[1, 2]),
            12 | 13 => out.pick(&[0, 2, 4]),
            _ => out,
        };
    }
    t.counters.primitives += (out.primitives.len() / out.size) as u64;
    if t.r(0x744) & 1 != 0 {
        crate::geom::feedback(t, &out, mem);
        t.pipe.invalidate_textures();
        t.pipe.cbufs_changed = true;
    }
    if t.r(0x37c) & 1 != 0 && !out.primitives.is_empty() {
        rasterize(t, &mut out, fs, mem);
    }
    written(t);
}

/// A vertex shader's inputs in a draw.
struct VertexInputs {
    streams: [Stream; 32],
    attributes: Vec<(usize, Attribute)>,
    constants: [[u32; 4]; 32],
    element_base: u32,
    instance_base: u32,
    instance: u32,
}

impl VertexInputs {
    /// Shade vertices: their outputs go to `out`, `layout.stride` words each.
    fn run(&self, vs: &Shader, work: &Work, w: &mut Worker, mem: &mut dyn Memory, layout: &Verts, vertices: &[u32], out: &mut [u32]) -> Option<String> {
        let mut inputs = [[[0u32; 4]; 32]; L];
        let (lanes, mut common) = w.parts(work, mem);
        lanes.set_local_size(vs.local_size());
        let mut failed = None;
        for (chunk, outs) in vertices.chunks(L).zip(out.chunks_mut(L * layout.stride)) {
            for (lane, &i) in chunk.iter().enumerate() {
                for &(k, a) in &self.attributes {
                    let s = &self.streams[a.stream];
                    inputs[lane][k] = if a.constant || !s.on {
                        self.constants[k]
                    } else {
                        let e = match s.divisor {
                            Some(0) => self.instance_base as u64,
                            Some(d) => self.instance_base as u64 + (self.instance / d) as u64,
                            None => i.wrapping_add(self.element_base) as u64,
                        };
                        let at = s.at + e * s.stride + a.offset as u64;
                        if at > s.limit { [0; 4] } else { a.fetch(at, &*common.mem) }
                    };
                }
            }
            let vertex = std::array::from_fn(|l| chunk.get(l).copied().unwrap_or(0).wrapping_add(self.element_base));
            lanes.start(&vs.program, (1u32 << chunk.len()).wrapping_sub(1), 0);
            let mut host = VertexHost { common: &mut common, inputs: &inputs, vertex, instance: self.instance, layout, out: outs };
            let mut fuel = FUEL;
            let e = maxwell_shader::run(&vs.program, lanes, &mut host, &mut fuel);
            if !matches!(e, Exit::Done) {
                failed = Some(format!("vertex shader stopped: {e:?}"));
            }
        }
        let stored = common.stored;
        w.stored |= stored;
        failed
    }
}

/// Vertices a thread shades at a time.
const CHUNK: usize = 16 * L;

/// Shade every vertex of the list once; returns them and where each list entry is.
fn shade_vertices(t: &mut Threed, vs: &Arc<Shader>, list: &[u32], arrays: bool, mem: &mut dyn Memory) -> (Verts, Vec<u32>) {
    let mut unique: Vec<u32> = Vec::new();
    let remap: Vec<u32> = if arrays {
        unique.extend_from_slice(list);
        (0..list.len() as u32).collect()
    } else {
        let (low, high) = list.iter().fold((u32::MAX, 0), |(a, b), &i| (a.min(i), b.max(i)));
        if (high - low) as usize <= 4 * list.len() + 1024 {
            // a table serves
            let mut seen = vec![u32::MAX; (high - low) as usize + 1];
            list.iter()
                .map(|&i| {
                    let s = &mut seen[(i - low) as usize];
                    if *s == u32::MAX {
                        *s = unique.len() as u32;
                        unique.push(i);
                    }
                    *s
                })
                .collect()
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
        }
    };
    let streams: [Stream; 32] = std::array::from_fn(|j| {
        let j = j as u32;
        let f = t.r(0x1c00 + 16 * j);
        Stream {
            on: f >> 12 & 1 != 0,
            at: t.address(0x1c04 + 16 * j),
            // the last byte of the stream
            limit: if t.pipe.sm70 {
                (t.address(0x1c04 + 16 * j) + t.address(0x600 + 8 * j)).wrapping_sub(1)
            } else {
                t.address(0x1f00 + 8 * j)
            },
            stride: (f & 0xfff) as u64,
            divisor: (t.r(0x1880 + 4 * j) & 1 != 0).then(|| t.r(0x1c0c + 16 * j)),
        }
    });
    let attributes = (0..32).filter(|&k| (0..4).any(|c| vs.sph.reads(0x80 + 16 * k as u32 + 4 * c))).map(|k| (k, Attribute::new(t.r(0x1160 + 4 * k as u32)))).collect();
    let inputs = VertexInputs {
        streams,
        attributes,
        constants: t.pipe.constants,
        element_base: if arrays { 0 } else { t.r(0x1434) },
        instance_base: t.r(0x1438),
        instance: t.batch.instance,
    };
    let work = work(t, 0);
    let mut out = Verts::new(vs.sph.outputs().filter_map(word));
    out.grow(unique.len());
    let mut data = std::mem::take(&mut out.data);
    let workers = if vs.program.writes_memory() { 1 } else { threads().min(unique.len() / (2 * CHUNK)).max(1) };
    t.pipe.need_workers(workers);
    let mut failed = None;
    #[cfg(not(target_arch = "wasm32"))]
    if workers > 1 && mem.share().is_some() {
        use std::sync::Mutex;
        let jobs = Mutex::new(unique.chunks(CHUNK).zip(data.chunks_mut(CHUNK * out.stride)));
        let memory = &*mem;
        type Slot<'a> = Mutex<(&'a mut Worker, Box<dyn Memory + Send + 'a>, Option<String>)>;
        let slots: Vec<Slot> = t.pipe.workers[..workers].iter_mut().map(|w| Mutex::new((w, memory.share().unwrap(), None))).collect();
        crate::pool::run(workers, &|k| {
            let (w, mine, failed) = &mut *slots[k].lock().unwrap();
            loop {
                let Some((v, o)) = jobs.lock().unwrap().next() else { break };
                if let Some(e) = inputs.run(vs, &work, w, &mut **mine, &out, v, o) {
                    *failed = Some(e);
                }
            }
        });
        failed = slots.into_iter().filter_map(|s| s.into_inner().unwrap().2).last();
        out.data = data;
        if let Some(e) = failed {
            t.pipe.error(e);
        }
        return (out, remap);
    }
    failed = inputs.run(vs, &work, &mut t.pipe.workers[0], mem, &out, &unique, &mut data).or(failed);
    out.data = data;
    if let Some(e) = failed {
        t.pipe.error(e);
    }
    written(t);
    (out, remap)
}

/// A shader may have written memory: what was read from it is stale.
fn written(t: &mut Threed) {
    if t.pipe.workers.iter_mut().fold(false, |s, w| s | std::mem::take(&mut w.stored)) {
        t.pipe.invalidate_textures();
        t.pipe.cbufs_changed = true;
    }
}

/// What the rasterizer needs to know about a fragment shader's surroundings.
pub struct Setup {
    /// gl_FragCoord.y counts up from the bottom of a window this high (`Threed::window_flip`)
    pub flip: Option<f32>,
    pub targets: Targets,
    pub rect: Rect,
    pub ds: DepthStencil,
    pub outputs: Vec<(usize, Output)>,
    pub constant: [f32; 4],
    pub logic: Option<u32>,
    pub fs: Option<Arc<Shader>>,
    /// depth and stencil are tested before shading
    pub early: bool,
    pub alpha_test: Option<(u32, f32)>,
    pub sample_mask: u32,
    pub samples: u32,
    pub positions: [(i32, i32); 16],
    pub per_sample: bool,
    pub alpha_to_coverage: bool,
    pub alpha_to_one: bool,
    /// the layer of everything, unless vertices choose theirs
    pub layer: Option<u32>,
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
    /// the shader's results
    pub frag: FragOutputs,
    /// the index in `inputs` of each attribute word
    pub slots: [u8; WORDS],
    /// the shader writes memory: no threads
    pub serial: bool,
}

fn rasterize(t: &mut Threed, g: &mut Geometry, fs: Option<Arc<Shader>>, mem: &mut dyn Memory) {
    let targets = Targets::new(t);
    let mut rect = targets.rect();
    if t.r(0xe00) & 1 != 0 {
        rect = rect.and(t.r(0xe04), t.vertical(t.r(0xe08)));
    }
    if rect.empty() {
        return;
    }
    let ds = DepthStencil::new(t);
    let mut outputs = Vec::new();
    let (mut need, mut write) = (0, 0);
    for (k, c) in targets.color.iter().enumerate() {
        let Some((_, format)) = c else { continue };
        let blend = Blend::new(t, k);
        if blend.mask & 0x1111 == 0 {
            continue;
        }
        outputs.push((k, Output { format: *format, blend, clamp: t.r(0x13a8) >> (4 * k) & 1 != 0 }));
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
    let stores = fs.as_ref().is_some_and(|f| f.sph.global_store || f.program.writes_memory());
    let changes = stores || sph.is_some_and(|s| s.kills_pixels) || out.as_ref().is_some_and(|o| o.depth.is_some() || o.sample_mask.is_some()) || alpha_test.is_some() || alpha_to_coverage;
    let mut sample_mask = 0;
    for s in 0..samples {
        // one word per pixel of a 2 × 2 block; the first serves all
        sample_mask |= t.r(0xfbc) & 1 << s;
    }
    if samples == 1 || t.r(0x1534) & 1 == 0 {
        sample_mask = u32::MAX;
    }
    let two_sided = t.r(0x1688) & 1 != 0;
    let mut inputs: Vec<(usize, Interp)> = Vec::new();
    if let Some(f) = &fs {
        for a in (0x80..0x3c0).step_by(4).filter(|&a| f.sph.reads(a)) {
            let Some(w) = word(a) else { continue };
            inputs.push((w, f.sph.interp(a)));
            // back faces' colours
            if two_sided && (0x280..0x2a0).contains(&a) {
                inputs.push((w + 8, f.sph.interp(a)));
            }
        }
    }
    let mut slots = [u8::MAX; WORDS];
    inputs.iter().enumerate().for_each(|(k, i)| slots[i.0] = k as u8);
    let dual = outputs.iter().any(|(_, o)| o.blend.on && o.blend.rgb[1..].iter().chain(&o.blend.alpha[1..]).any(|f| matches!(f & 0xfff, 0x900..=0x903)));
    let s = Setup {
        flip: t.window_flip().map(|h| h as f32),
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
        alpha_to_one: t.r(0x153c) >> 4 & 1 != 0 && samples > 1,
        dual,
        layer: (t.r(0x15cc) >> 16 & 1 == 0).then(|| t.r(0x15cc) & 0xffff),
        two_sided,
        clamp_z: t.r(0x193c) & 0x18 != 0,
        zrange: {
            let (a, b) = (t.f(0xc08), t.f(0xc0c));
            if a.is_nan() || b.is_nan() { (0.0, 1.0) } else { (a.min(b), a.max(b)) }
        },
        inputs,
        need,
        write,
        sprite: if t.r(0x1520) & 1 != 0 { t.r(0x1604) } else { 0 },
        flat: t.r(0x12d4) == 0x1d00,
        frag: out.unwrap_or_default(),
        slots,
        serial: fs.as_ref().is_some_and(|f| f.program.writes_memory()),
        targets,
        fs,
    };
    if s.need == 0 && s.fs.as_ref().is_none_or(|f| !f.sph.global_store) && t.r(0x1514) & 1 == 0 {
        return;
    }
    t.rop.begin(&s.targets);
    if t.r(0x2600) & 1 != 0 {
        g.vertices.clamp_colors();
    }
    let work = work(t, 4);
    // (a part at a time, for the memory it takes)
    let total = g.primitives.len() / g.size;
    let vertices = g.vertices.data.len();
    for from in (0..total).step_by(PART) {
        let scene = raster::setup(t, g, &s, from, (from + PART).min(total));
        if !scene.prims.is_empty() {
            raster::draw(t, &g.vertices, &scene, &s, &work, mem);
        }
        g.vertices.data.truncate(vertices);
    }
    written(t);
}

/// Primitives set up before they are drawn.
const PART: usize = 1 << 18;

/// A fragment shader's surroundings: four quads.
pub struct FragmentHost<'a, 'm> {
    common: &'a mut Common<'m>,
    setup: &'a Setup,
    scene: &'a Scene,
    /// each quad's primitive and first pixel
    prims: [&'a Prim; L / 4],
    slots: [&'a [Slot]; L / 4],
    quads: [(i32, i32); L / 4],
    /// each lane's covered samples
    coverage: [u32; L],
    /// the sample being shaded, with per-sample shading
    sample: Option<u32>,
    /// each lane's sample point
    x: [f32; L],
    y: [f32; L],
}

impl<'a, 'm> FragmentHost<'a, 'm> {
    #[allow(clippy::too_many_arguments)]
    pub fn new(common: &'a mut Common<'m>, setup: &'a Setup, scene: &'a Scene, prims: [&'a Prim; L / 4], slots: [&'a [Slot]; L / 4], quads: [(i32, i32); L / 4], coverage: [u32; L], sample: Option<u32>) -> Self {
        let (cx, cy) = match sample {
            Some(s) => {
                let (x, y) = setup.positions[s as usize];
                (x as f32 / 256.0, y as f32 / 256.0)
            }
            None => (0.5, 0.5),
        };
        let x = std::array::from_fn(|l| (quads[l / 4].0 + (l & 1) as i32) as f32 + cx);
        let y = std::array::from_fn(|l| (quads[l / 4].1 + (l >> 1 & 1) as i32) as f32 + cy);
        Self { common, setup, scene, prims, slots, quads, coverage, sample, x, y }
    }

    /// Where an instruction interpolates.
    fn points(&self, op: &IpaOp<L>) -> ([f32; L], [f32; L]) {
        let pixel = |l: usize| ((self.quads[l / 4].0 + (l & 1) as i32) as f32, (self.quads[l / 4].1 + (l >> 1 & 1) as i32) as f32);
        let all = (1u32 << self.setup.samples) - 1;
        let at = |l: usize| match op.loc {
            IpaLoc::Offset => (pixel(l).0 + 0.5 + op.offset[0][l], pixel(l).1 + 0.5 + op.offset[1][l]),
            // a partly covered pixel: at a sample that is
            IpaLoc::Centroid if self.sample.is_none() && self.coverage[l] & all != all && self.coverage[l] & all != 0 => {
                let (x, y) = self.setup.positions[self.coverage[l].trailing_zeros() as usize];
                (pixel(l).0 + x as f32 / 256.0, pixel(l).1 + y as f32 / 256.0)
            }
            _ => (self.x[l], self.y[l]),
        };
        let p: [(f32, f32); L] = std::array::from_fn(at);
        (p.map(|p| p.0), p.map(|p| p.1))
    }
}

impl Host<L> for FragmentHost<'_, '_> {
    common_host!(L);

    fn sysval(&mut self, sys: u8) -> [u32; L] {
        match sys {
            0x12 => [1f32.to_bits(); L], // the direction of y
            _ => [0; L],
        }
    }

    fn interpolate(&mut self, op: &IpaOp<L>) -> [u32; L] {
        let moved;
        let (x, y) = if op.loc == IpaLoc::Default {
            (&self.x, &self.y)
        } else {
            moved = self.points(op);
            (&moved.0, &moved.1)
        };
        let mul_w = op.mode == IpaMode::MulW;
        let mut out = [0u32; L];
        let of = |f: &dyn Fn(&Prim, usize) -> f32| std::array::from_fn(|l| f(self.prims[l / 4], l).to_bits());
        match op.addr {
            0x70 => return x.map(f32::to_bits),
            0x74 => return y.map(|y| self.setup.flip.map_or(y, |h| h - y).to_bits()),
            0x78 => return of(&|p, l| p.z.at(x[l], y[l])),
            0x7c => return of(&|p, l| p.w.at(x[l], y[l])),
            0x3fc => return std::array::from_fn(|l| if self.prims[l / 4].front { u32::MAX } else { 0 }),
            0x60 => return std::array::from_fn(|l| self.prims[l / 4].id),
            0x64 => return std::array::from_fn(|l| self.prims[l / 4].layer),
            0x68 => return [0; L],
            0x2e0 | 0x2e4 => {
                return of(&|p, l| {
                    let v = p.point.map_or(0.0, |k| self.scene.points[k as usize][(op.addr >> 2 & 1) as usize].at(x[l], y[l]));
                    if mul_w { v * p.w.at(x[l], y[l]) } else { v }
                });
            }
            _ => {}
        }
        let Some(w) = word(op.addr) else { return out };
        for (q, o) in out.chunks_exact_mut(4).enumerate() {
            let p = self.prims[q];
            let at = 4 * q..4 * q + 4;
            let (x, y) = (&x[at.clone()], &y[at]);
            // a point sprite's coordinate takes a generic's place
            if let (Some(k), 0x80..=0xfc) = (p.point, op.addr) {
                if self.setup.sprite >> (3 + (op.addr - 0x80) / 16) & 1 != 0 {
                    let c = (op.addr >> 2 & 3) as usize;
                    for l in 0..4 {
                        let v = match c {
                            0 | 1 => self.scene.points[k as usize][c].at(x[l], y[l]),
                            2 => 0.0,
                            _ => 1.0,
                        };
                        o[l] = if mul_w { v * p.w.at(x[l], y[l]) } else { v }.to_bits();
                    }
                    continue;
                }
            }
            // two-sided colours
            let w = if self.setup.two_sided && !p.front && (0x280..0x2a0).contains(&op.addr) { w + 8 } else { w };
            let k = self.setup.slots[w];
            if k == u8::MAX {
                continue;
            }
            let slot = &self.slots[q][k as usize];
            match (&slot.linear, op.mode) {
                (None, IpaMode::MulW) => {
                    // the shader will multiply by w
                    let v = f32::from_bits(slot.flat);
                    (0..4).for_each(|l| o[l] = (v * p.w.at(x[l], y[l])).to_bits());
                }
                (None, _) | (_, IpaMode::Constant) => o.fill(slot.flat),
                (Some(v), _) => (0..4).for_each(|l| o[l] = v.at(x[l], y[l]).to_bits()),
            }
        }
        out
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
