//! Vertex and tiler jobs (Mesa `genxml/v5.xml`: "Compute Job", "Tiler Job", "Draw").
//!
//! A vertex job runs the vertex shader once per vertex (and instance): attributes come
//! from the attribute records and buffers, results go to the varying buffers through
//! `st_vary`. Mesa's Midgard vertex shaders apply the viewport transform themselves, so
//! the position varying holds (x, y) in framebuffer pixels, z in [0, 1] and 1/w.
//!
//! A tiler job assembles primitives from those positions (indices, draw mode, culling)
//! and appends them to the polygon list of its framebuffer. The polygon list format is
//! internal to the GPU (only the fragment job reads it), so the model keeps the lists on
//! the host, keyed by the framebuffer descriptor.

use crate::format::{Class, PixelFormat};
use crate::isa::{Core, Env, LdSt, LdStArgs, TexWord, Thread};
use crate::tex;
use crate::vm::Vm;
use std::rc::Rc;

/// The Draw descriptor (DCD) shared by compute/vertex and tiler jobs.
#[derive(Clone, Copy, Debug, Default)]
pub struct Dcd {
    pub flags: u32,
    pub offset_start: u32,
    pub position: u64,
    pub ubos: u64,
    pub textures: u64,
    pub samplers: u64,
    pub push: u64,
    pub state: u64,
    pub attr_bufs: u64,
    pub attrs: u64,
    pub vary_bufs: u64,
    pub varyings: u64,
    pub viewport: u64,
    pub occlusion: u64,
    pub fbd: u64,
}

fn padded(v: u32) -> u32 {
    let shift = v & 31;
    let odd = v >> 5;
    (2 * odd + 1) << shift
}

impl Dcd {
    pub fn read(vm: &mut Vm, va: u64) -> Self {
        let w: [u32; 30] = vm.words(va);
        let a = |i: usize| w[i] as u64 | (w[i + 1] as u64) << 32;
        Self {
            flags: w[0],
            offset_start: w[1],
            position: a(4),
            ubos: a(6),
            textures: a(8),
            samplers: a(10),
            push: a(12),
            state: a(14),
            attr_bufs: a(16),
            attrs: a(18),
            vary_bufs: a(20),
            varyings: a(22),
            viewport: a(24),
            occlusion: a(26),
            fbd: a(28),
        }
    }

    pub fn instance_size(&self) -> u32 {
        padded((self.flags >> 16) & 0xff)
    }
    pub fn front_ccw(&self) -> bool {
        self.flags & (1 << 5) != 0
    }
    pub fn cull_front(&self) -> bool {
        self.flags & (1 << 6) != 0
    }
    pub fn cull_back(&self) -> bool {
        self.flags & (1 << 7) != 0
    }
}

/// Renderer state descriptor (RSD) fields the model uses.
#[derive(Clone, Copy, Debug, Default)]
pub struct Rsd {
    pub shader: u64,
    pub props: u32,
    pub depth_units: f32,
    pub depth_factor: f32,
    pub depth_clamp: f32,
    pub ms_misc: u32,
    pub stencil_misc: u32,
    pub stencil_front: u32,
    pub stencil_back: u32,
    pub alpha_ref: f32,
}

impl Rsd {
    pub fn read(vm: &mut Vm, va: u64) -> Self {
        let w: [u32; 16] = vm.words(va);
        Self {
            shader: w[0] as u64 | (w[1] as u64) << 32,
            props: w[4],
            depth_units: f32::from_bits(w[5]),
            depth_factor: f32::from_bits(w[6]),
            depth_clamp: f32::from_bits(w[7]),
            ms_misc: w[8],
            stencil_misc: w[9],
            stencil_front: w[10],
            stencil_back: w[11],
            alpha_ref: f32::from_bits(w[12]),
        }
    }
    pub fn uniform_count(&self) -> u32 {
        (self.props >> 21) & 31
    }
    pub fn pc(&self) -> u64 {
        self.shader & !0xf
    }
}

/// Invocation dimensions (Mesa `panfrost_pack_work_groups_compute`).
#[derive(Clone, Copy, Debug)]
pub struct Invocation {
    pub size: [u32; 3],
    pub groups: [u32; 3],
}

impl Invocation {
    pub fn read(vm: &mut Vm, va: u64) -> Self {
        let packed = vm.r32(va);
        let w1 = vm.r32(va + 4);
        let shifts = [
            0,
            w1 & 31,
            (w1 >> 5) & 31,
            (w1 >> 10) & 63,
            (w1 >> 16) & 63,
            (w1 >> 22) & 63,
            32,
        ];
        let mut v = [1u32; 6];
        for i in 0..6 {
            let (lo, hi) = (shifts[i].min(32), shifts[i + 1].clamp(shifts[i].min(32), 32));
            if hi > lo {
                let bits = hi - lo;
                let m = if bits >= 32 { u32::MAX } else { (1u32 << bits) - 1 };
                v[i] = ((packed >> lo) & m).wrapping_add(1);
            }
        }
        Self { size: [v[0], v[1], v[2]], groups: [v[3], v[4], v[5]] }
    }

    pub fn count(&self) -> u64 {
        self.size.iter().chain(&self.groups).fold(1u64, |a, &x| a.saturating_mul(x as u64))
    }
}

/// An attribute or varying record and its buffer, resolved.
#[derive(Clone, Copy, Debug)]
pub struct Attr {
    pub format: PixelFormat,
    pub offset: i32,
    pub buf: AttrBuf,
}

#[derive(Clone, Copy, Debug, Default)]
pub struct AttrBuf {
    /// Special input (gl_VertexID, gl_FragCoord, ...), or 0.
    pub special: u32,
    pub ty: u32,
    pub pointer: u64,
    pub stride: u32,
    pub size: u32,
    pub divisor_r: u32,
    pub divisor: u32,
}

pub fn read_attr(vm: &mut Vm, records: u64, bufs: u64, index: u32) -> Attr {
    let w = vm.r64(records + 8 * index as u64);
    let buf_index = (w & 0x1ff) as u32;
    let format = PixelFormat::from_bits(((w >> 10) & 0x3f_ffff) as u32);
    let offset = (w >> 32) as i32;
    let b: [u32; 4] = vm.words(bufs + 16 * buf_index as u64);
    // Bits 0-5 are the type (at most 32, "continuation"); special inputs put their 8-bit
    // code over the type and the pointer's low bits, which are then zero.
    let ty = b[0] & 0x3f;
    let mut buf = AttrBuf { pointer: (b[0] as u64 | (b[1] as u64) << 32) & !63 & ((1 << 56) - 1), stride: b[2], size: b[3], ..Default::default() };
    if ty > 32 {
        buf.special = b[0] & 0xff;
        buf.pointer = 0;
    } else {
        buf.ty = b[0] & 0x3f;
        buf.divisor_r = (b[1] >> 24) & 31;
        buf.divisor = padded((b[1] >> 24) & 0xff);
        if buf.ty == 4 || buf.ty == 12 {
            // NPOT divisor: the divisor is in the continuation record
            let c: [u32; 4] = vm.words(bufs + 16 * (buf_index as u64 + 1));
            buf.divisor = c[3].max(1);
        } else if buf.ty == 3 || buf.ty == 11 {
            buf.divisor = padded((b[1] >> 24) & 0xff);
        }
    }
    Attr { format, offset, buf }
}

impl AttrBuf {
    /// Element index for linear invocation `l` (vertex `l` of a non-instanced draw).
    pub fn element(&self, l: u32, offset_start: u32) -> u32 {
        let i = match self.ty {
            3 | 11 => l % self.divisor.max(1),
            2 | 10 => l >> self.divisor_r,
            4 | 12 => l / self.divisor.max(1),
            _ => l,
        };
        i.wrapping_add(offset_start)
    }
}

/// Uniform buffer descriptor: entries and pointer.
pub fn ubo_pointer(vm: &mut Vm, ubos: u64, index: u32) -> u64 {
    let d = vm.r64(ubos + 8 * index as u64);
    (d >> 12) << 4
}

/// Load an attribute element as RGBA in the class a shader load asks for.
pub fn load_attr(vm: &mut Vm, a: &Attr, element: u32) -> [u32; 4] {
    let va = a.buf.pointer.wrapping_add(element as u64 * a.buf.stride as u64).wrapping_add(a.offset as i64 as u64);
    a.format.load(vm, va)
}

/// Convert a loaded value to what an `ld_attr`/`ld_vary` variant returns.
pub fn convert_for(op_kind: u32, class: Class, v: [u32; 4]) -> [u32; 4] {
    // op_kind: 0 float32, 1 float16 (returned as fp32 here), 2 uint, 3 int
    match (op_kind, class) {
        (0 | 1, Class::Uint) => v.map(|x| (x as f32).to_bits()),
        (0 | 1, Class::Sint) => v.map(|x| (x as i32 as f32).to_bits()),
        (2 | 3, Class::Float) => v.map(|x| {
            let f = f32::from_bits(x);
            if op_kind == 2 { f as u32 } else { f as i32 as u32 }
        }),
        _ => v,
    }
}

pub fn to_u128(v: [u32; 4]) -> u128 {
    v[0] as u128 | (v[1] as u128) << 32 | (v[2] as u128) << 64 | (v[3] as u128) << 96
}

pub fn from_u128(v: u128) -> [u32; 4] {
    [v as u32, (v >> 32) as u32, (v >> 64) as u32, (v >> 96) as u32]
}

/// Pack fp32 lanes as fp16 into the low 64 bits.
pub fn to_half_lanes(v: [u32; 4]) -> u128 {
    let mut o = 0u128;
    for c in 0..4 {
        o |= (crate::half::f32_to_f16(f32::from_bits(v[c])) as u128) << (16 * c);
    }
    o
}

/// Common load/store handling: UBOs, global memory, specials. Returns None for ops the
/// caller handles (attributes, varyings).
pub fn common_ldst(vm: &mut Vm, dcd: &Dcd, ins: &LdSt, a: LdStArgs, data: u128, tls: u64) -> Option<u128> {
    let op = ins.op;
    match op {
        0xa0..=0xb3 => {
            // The buffer index is an immediate in the argument fields (Mesa's
            // midgard_pack_ubo_index_imm); Mesa never selects it by register.
            let index = ins.arg_comp | ins.arg_reg << 2 | (ins.bitsize_toggle as u32) << 5 | ins.index_format << 6;
            let base = ubo_pointer(vm, dcd.ubos, index);
            let off = ((ins.offset >> 2) as i64 as u64).wrapping_add(a.index << ins.index_shift);
            let len = match op & 0x1c {
                0x00 => 1,
                0x04 => 2,
                0x08 => 4,
                0x0c => 8,
                _ => 16,
            };
            let mut b = [0u8; 16];
            vm.read_bytes(base.wrapping_add(off), &mut b[..len]);
            Some(u128::from_le_bytes(b))
        }
        0x80..=0x93 => {
            let addr = mem_addr(ins, a, tls);
            let len = match op {
                0x80 | 0x81 => 1,
                0x84..=0x87 => 2,
                0x88..=0x8b => 4,
                0x8c..=0x8f => 8,
                _ => 16,
            };
            let mut b = [0u8; 16];
            vm.read_bytes(addr, &mut b[..len]);
            let v = u128::from_le_bytes(b);
            Some(match op {
                0x81 => (v as u8 as i8 as i32 as u32) as u128,
                0x85 => (v as u16 as i16 as i32 as u32) as u128,
                _ => v,
            })
        }
        0xc0..=0xd3 => {
            let addr = mem_addr(ins, a, tls);
            let b = data.to_le_bytes();
            match op {
                0xc0 | 0xc1 => vm.write_bytes(addr, &b[..1]),
                0xc4..=0xc7 => vm.write_bytes(addr, &b[..2]),
                0xc8..=0xcb => {
                    for i in 0..4 {
                        if ins.mask & (1 << i) != 0 {
                            vm.write_bytes(addr + i, &b[i as usize..i as usize + 1]);
                        }
                    }
                }
                0xcc..=0xcf => {
                    for i in 0..4 {
                        if ins.mask & (1 << i) != 0 {
                            vm.write_bytes(addr + 2 * i, &b[2 * i as usize..2 * i as usize + 2]);
                        }
                    }
                }
                _ => {
                    for i in 0..4 {
                        if ins.mask & (1 << i) != 0 {
                            vm.write_bytes(addr + 4 * i, &b[4 * i as usize..4 * i as usize + 4]);
                        }
                    }
                }
            }
            Some(0)
        }
        0x9c..=0x9f => Some(match ins.offset >> 9 {
            96 => 1, // sample mask: single-sampled
            _ => 0,
        }),
        _ => None,
    }
}

fn mem_addr(ins: &LdSt, a: LdStArgs, _tls: u64) -> u64 {
    let idx = if ins.index_format == 3 { a.index as u32 as i32 as i64 as u64 } else { a.index };
    a.arg.wrapping_add(idx << ins.index_shift).wrapping_add(ins.offset as i64 as u64)
}

/// The vertex shader's environment.
struct VertexEnv<'a, 'b> {
    vm: &'a mut Vm<'b>,
    dcd: Dcd,
    /// Linear invocation (attribute addressing) and varying index of each thread.
    linear: [u32; 4],
    vary_index: [u32; 4],
    instance: [u32; 4],
}

impl Env for VertexEnv<'_, '_> {
    fn spend(&mut self, n: u64) -> bool {
        self.vm.spend(n)
    }

    fn code(&mut self, va: u64) -> [u8; 64] {
        let mut b = [0u8; 64];
        self.vm.read_bytes(va, &mut b);
        b
    }

    fn ldst(&mut self, t: usize, ins: &LdSt, a: LdStArgs, data: u128) -> u128 {
        let op = ins.op;
        let index = ((ins.offset >> 9) as i64 as u64).wrapping_add(a.index << ins.index_shift) as u32;
        let secondary = ins.index_format & 2 != 0;
        match op {
            0x94..=0x97 => {
                let (recs, bufs) = if secondary { (self.dcd.varyings, self.dcd.vary_bufs) } else { (self.dcd.attrs, self.dcd.attr_bufs) };
                let at = read_attr(self.vm, recs, bufs, index);
                let v = match at.buf.special {
                    34 => {
                        let n = self.dcd.instance_size().max(1);
                        let inst = self.instance[t];
                        [self.linear[t].wrapping_sub(inst.wrapping_mul(if inst > 0 { n } else { 0 })).wrapping_add(self.dcd.offset_start), 0, 0, 1]
                    }
                    36 => [self.instance[t], 0, 0, 1],
                    0 => {
                        let e = if ins.bitsize_toggle {
                            a.arg as u32
                        } else {
                            at.buf.element(self.linear[t], self.dcd.offset_start)
                        };
                        let v = load_attr(self.vm, &at, e);
                        convert_for((op - 0x94) & 3, at.format.class(), v)
                    }
                    _ => [0, 0, 0, 0],
                };
                let kind = [0u32, 1, 2, 3][(op - 0x94) as usize];
                if kind == 1 { to_half_lanes(v) } else { to_u128(v) }
            }
            0xd4..=0xd7 => {
                let (recs, bufs) = if secondary || op >= 0xd4 { (self.dcd.varyings, self.dcd.vary_bufs) } else { (self.dcd.attrs, self.dcd.attr_bufs) };
                let at = read_attr(self.vm, recs, bufs, index);
                if at.buf.special == 0 && at.format.id != 95 {
                    let mut v = from_u128(data);
                    if op == 0xd5 {
                        // st_vary_16: fp16 lanes in the low half
                        let h = data;
                        for c in 0..4 {
                            v[c] = crate::half::f16_to_f32((h >> (16 * c)) as u16).to_bits();
                        }
                    }
                    let v = match (op, at.format.class()) {
                        (0xd4 | 0xd5, Class::Uint) => v.map(|x| f32::from_bits(x) as u32),
                        (0xd4 | 0xd5, Class::Sint) => v.map(|x| f32::from_bits(x) as i32 as u32),
                        (0xd6 | 0xd7, Class::Float) => v.map(|x| if op == 0xd6 { (x as f32).to_bits() } else { (x as i32 as f32).to_bits() }),
                        _ => v,
                    };
                    let mut b = [0u8; 32];
                    let n = at.format.encode(v, &mut b);
                    let va = at.buf.pointer
                        .wrapping_add(self.vary_index[t] as u64 * at.buf.stride as u64)
                        .wrapping_add(at.offset as i64 as u64);
                    self.vm.write_bytes(va, &b[..n]);
                }
                0
            }
            _ => common_ldst(self.vm, &self.dcd, ins, a, data, 0).unwrap_or(0),
        }
    }

    fn texture(&mut self, w: &TexWord, coords: &[[f32; 4]; 4], lod: &[f32; 4], active: u8) -> [[u32; 4]; 4] {
        tex::sample(self.vm, &self.dcd, w, coords, lod, active, false)
    }

    fn writeout(&mut self, _t: usize, _r0: u128, _half: bool, _r1: u128, _w: u8) {}
}

/// Preload uniform registers: push uniform n into r(23 - n).
pub fn preload_uniforms(vm: &mut Vm, dcd: &Dcd, rsd: &Rsd, t: &mut Thread) {
    for n in 0..rsd.uniform_count().min(16) {
        let mut b = [0u8; 16];
        vm.read_bytes(dcd.push + 16 * n as u64, &mut b);
        t.r[(23 - n) as usize] = u128::from_le_bytes(b);
    }
}

/// Run a vertex (or compute) job: returns GPU cycles.
pub fn vertex_job(vm: &mut Vm, job: u64) -> u64 {
    let inv = Invocation::read(vm, job + 32);
    let dcd = Dcd::read(vm, job + 64);
    let rsd = Rsd::read(vm, dcd.state);
    if rsd.pc() == 0 {
        return 10;
    }
    let total = inv.count().min(1 << 22) as u32;
    let instanced = inv.groups[2] > 1 && inv.groups[1] > 1;
    let isize = dcd.instance_size().max(1);
    let mut uniforms = Thread::new(rsd.pc());
    preload_uniforms(vm, &dcd, &rsd, &mut uniforms);
    let mut env = VertexEnv { vm, dcd, linear: [0; 4], vary_index: [0; 4], instance: [0; 4] };
    let mut core = Core::new(&mut env);
    let mut l = 0;
    while l < total {
        let n = (total - l).min(4) as usize;
        let mut threads = [uniforms; 4];
        for k in 0..n {
            let i = l + k as u32;
            let (vertex, instance) = if instanced { (i % inv.groups[1], i / inv.groups[1]) } else { (i, 0) };
            let linear = instance.wrapping_mul(isize).wrapping_add(vertex);
            core.env.linear[k] = linear;
            core.env.vary_index[k] = linear;
            core.env.instance[k] = instance;
        }
        core.run(&mut threads[..n]);
        l += n as u32;
        if !core.env.vm.spend(0) {
            break;
        }
    }
    core.cycles * 4 / 4 + total as u64
}

/// Most primitives one polygon list holds (about 160 MB); a draw that needs more fails its
/// chain.
pub const MAX_PRIMS: usize = 1 << 21;

/// A primitive in a polygon list.
#[derive(Clone, Debug)]
pub struct Prim {
    pub kind: u8,
    /// Varying indices of the vertices.
    pub v: [u32; 3],
    /// Screen positions (x, y, z, 1/w) of the corners.
    pub pos: [[f32; 4]; 3],
    /// Each corner as clip-space weights of the vertices `v` (the identity unless the
    /// triangle was clipped at the near plane).
    pub corner: [[f32; 3]; 3],
    pub draw: Rc<DrawState>,
}

const IDENTITY: [[f32; 3]; 3] = [[1.0, 0.0, 0.0], [0.0, 1.0, 0.0], [0.0, 0.0, 1.0]];

/// Clip a triangle at the near plane (window z = `znear`). Positions hold window x, y, z
/// and 1/w; in clip space (x w, y w, (z - znear) w, w) is linear along edges, and
/// (z - znear) w is the distance to the near plane. Returns the corners of the clipped
/// polygon with their clip-space weights of the original vertices.
pub fn clip_near(pos: [[f32; 4]; 3], znear: f32) -> Vec<([f32; 4], [f32; 3])> {
    let h: Vec<([f32; 4], [f32; 3])> = (0..3)
        .map(|i| {
            let w = 1.0 / pos[i][3];
            ([pos[i][0] * w, pos[i][1] * w, (pos[i][2] - znear) * w, w], IDENTITY[i])
        })
        .collect();
    if h.iter().all(|(c, _)| c[2] >= 0.0 && c[3] > 0.0 && c[3].is_finite()) {
        return (0..3).map(|i| (pos[i], IDENTITY[i])).collect();
    }
    let mut out: Vec<([f32; 4], [f32; 3])> = Vec::new();
    for i in 0..3 {
        let (a, wa) = h[i];
        let (b, wb) = h[(i + 1) % 3];
        let (ia, ib) = (a[2] >= 0.0, b[2] >= 0.0);
        if ia {
            out.push((a, wa));
        }
        if ia != ib {
            let t = a[2] / (a[2] - b[2]);
            let mut c = [0f32; 4];
            let mut wc = [0f32; 3];
            for k in 0..4 {
                c[k] = a[k] + t * (b[k] - a[k]);
            }
            for k in 0..3 {
                wc[k] = wa[k] + t * (wb[k] - wa[k]);
            }
            c[2] = 0.0;
            out.push((c, wc));
        }
    }
    out.into_iter()
        .filter(|(c, _)| c[3] > 0.0)
        .map(|(c, w)| ([c[0] / c[3], c[1] / c[3], c[2] / c[3] + znear, 1.0 / c[3]], w))
        .collect()
}

/// Per-draw state a fragment job needs.
#[derive(Clone, Debug)]
pub struct DrawState {
    pub dcd: Dcd,
    pub rsd_va: u64,
    pub first_provoking: bool,
    pub point_size: f32,
    pub line_width: f32,
}

/// Primitive modes.
pub const POINTS: u8 = 1;
pub const LINES: u8 = 2;
pub const TRIS: u8 = 3;

/// Run a tiler job: append its primitives to `list`.
pub fn tiler_job(vm: &mut Vm, job: u64, list: &mut Vec<Prim>) -> u64 {
    let inv = Invocation::read(vm, job + 32);
    let p: [u32; 6] = vm.words(job + 40);
    let dcd = Dcd::read(vm, job + 64);
    let psize = f32::from_bits(vm.r32(job + 184));
    let mode = p[0] & 0xff;
    let index_type = (p[0] >> 8) & 7;
    let first_provoking = p[0] & (1 << 15) != 0;
    let restart = (p[0] >> 19) & 3;
    let base_vertex = p[1] as i32;
    let restart_index = p[2];
    let count = p[3].wrapping_add(1).min(1 << 24);
    let indices = p[4] as u64 | (p[5] as u64) << 32;
    let instances = if inv.groups[2] > 1 && inv.groups[1] > 1 { inv.groups[2] } else { 1 };
    let isize = dcd.instance_size().max(1);
    let draw = Rc::new(DrawState { dcd, rsd_va: dcd.state, first_provoking, point_size: if psize > 0.0 { psize } else { 1.0 }, line_width: 1.0 });
    let restart_value = match (restart, index_type) {
        (2, 1) => Some(0xff),
        (2, 2) => Some(0xffff),
        (2, 3) => Some(0xffff_ffff),
        (3, _) => Some(restart_index),
        _ => None,
    };
    let mut prims = 0u64;
    let znear = if dcd.viewport != 0 { vm.rf32(dcd.viewport + 16) } else { 0.0 };
    for inst in 0..instances {
        if !vm.spend(count as u64) || list.len() >= MAX_PRIMS {
            // out of time, or more primitives than a polygon list holds: the chain fails
            vm.exhaust();
            break;
        }
        let mut idx: Vec<Option<u32>> = Vec::with_capacity(count as usize);
        for k in 0..count {
            let raw = match index_type {
                1 => vm.r8(indices + k as u64) as u32,
                2 => vm.r16(indices + 2 * k as u64) as u32,
                3 => vm.r32(indices + 4 * k as u64),
                _ => k,
            };
            if index_type != 0 && restart_value == Some(raw) {
                idx.push(None);
                continue;
            }
            let v = if index_type != 0 { (raw as i64 + base_vertex as i64) as u32 } else { raw };
            idx.push(Some(v.wrapping_add(inst.wrapping_mul(isize))));
        }
        // split at restarts
        for run in idx.split(|x| x.is_none()) {
            let run: Vec<u32> = run.iter().map(|x| x.unwrap()).collect();
            let mut emit = |kind: u8, v: [u32; 3]| {
                if list.len() >= MAX_PRIMS {
                    return;
                }
                let n = match kind {
                    POINTS => 1,
                    LINES => 2,
                    _ => 3,
                };
                let mut pos = [[0f32; 4]; 3];
                for i in 0..n {
                    let w: [u32; 4] = vm.words(dcd.position + 16 * v[i] as u64);
                    pos[i] = w.map(f32::from_bits);
                }
                if kind == TRIS {
                    let poly = clip_near(pos, znear);
                    for k in 2..poly.len() {
                        let c = [poly[0], poly[k - 1], poly[k]];
                        list.push(Prim {
                            kind,
                            v,
                            pos: [c[0].0, c[1].0, c[2].0],
                            corner: [c[0].1, c[1].1, c[2].1],
                            draw: draw.clone(),
                        });
                        prims += 1;
                    }
                    return;
                }
                list.push(Prim { kind, v, pos, corner: IDENTITY, draw: draw.clone() });
                prims += 1;
            };
            let n = run.len();
            match mode {
                1 => run.iter().for_each(|&a| emit(POINTS, [a, a, a])),
                2 => run.as_chunks::<2>().0.iter().for_each(|c| emit(LINES, [c[0], c[1], c[1]])),
                4 | 6 => {
                    for i in 1..n {
                        emit(LINES, [run[i - 1], run[i], run[i]]);
                    }
                    if mode == 6 && n > 2 {
                        emit(LINES, [run[n - 1], run[0], run[0]]);
                    }
                }
                8 => run.as_chunks::<3>().0.iter().for_each(|c| emit(TRIS, [c[0], c[1], c[2]])),
                10 => {
                    for i in 2..n {
                        // keep the winding: odd triangles swap their first two vertices
                        if i % 2 == 0 {
                            emit(TRIS, [run[i - 2], run[i - 1], run[i]]);
                        } else {
                            emit(TRIS, [run[i - 1], run[i - 2], run[i]]);
                        }
                    }
                }
                12 | 13 => {
                    for i in 2..n {
                        emit(TRIS, [run[0], run[i - 1], run[i]]);
                    }
                }
                14 => run.as_chunks::<4>().0.iter().for_each(|c| {
                    emit(TRIS, [c[0], c[1], c[2]]);
                    emit(TRIS, [c[0], c[2], c[3]]);
                }),
                15 => {
                    let mut i = 3;
                    while i < n {
                        emit(TRIS, [run[i - 3], run[i - 2], run[i]]);
                        emit(TRIS, [run[i - 3], run[i], run[i - 1]]);
                        i += 2;
                    }
                }
                _ => {}
            }
        }
    }
    20 + prims * 4 + count as u64
}
