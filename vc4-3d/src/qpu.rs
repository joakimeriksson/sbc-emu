//! The QPU: VideoCore IV's 16-way SIMD shader processor (Mesa's `vc4_qpu_defines.h` names).
//!
//! Each instruction is 64 bits: an add and a multiply operation issued together, reading two
//! register-file ports (A and B, 32 registers each, plus memory-mapped I/O such as uniforms,
//! varyings and the VPM) and the accumulators r0–r5, writing one destination each; or a load
//! immediate, a semaphore operation or a branch. A signal field adds a side action: thread
//! switch, program end, TMU or tile-buffer loads into r4, small immediates.
//!
//! The interpreter runs one QPU's 16 lanes per instruction. What a program reaches outside its
//! registers (VPM, tile buffer, varyings, interrupts, semaphores) goes through [`Env`], which
//! the binner, the rasterizer and the user-program scheduler implement; memory (uniforms,
//! code, the TMU, VPM DMA) through [`Mem`].

use crate::mem::Mem;
use std::collections::VecDeque;

pub type V = [u32; 16];
pub const ZERO: V = [0; 16];
pub const ALL: u16 = 0xffff;

pub mod sig {
    pub const BREAK: u8 = 0;
    pub const NONE: u8 = 1;
    pub const THRSW: u8 = 2;
    pub const PROG_END: u8 = 3;
    pub const WAIT_SCORE: u8 = 4;
    pub const UNLOCK_SCORE: u8 = 5;
    pub const LAST_THRSW: u8 = 6;
    pub const COVERAGE_LOAD: u8 = 7;
    pub const COLOR_LOAD: u8 = 8;
    pub const COLOR_LOAD_END: u8 = 9;
    pub const LOAD_TMU0: u8 = 10;
    pub const LOAD_TMU1: u8 = 11;
    pub const ALPHA_MASK_LOAD: u8 = 12;
    pub const SMALL_IMM: u8 = 13;
    pub const LOAD_IMM: u8 = 14;
    pub const BRANCH: u8 = 15;
}

pub mod raddr {
    pub const UNIF: u8 = 32;
    pub const VARY: u8 = 35;
    pub const ELEM_QPU: u8 = 38;
    pub const NOP: u8 = 39;
    pub const XY_PIXEL: u8 = 41;
    pub const MS_REV: u8 = 42;
    pub const VPM: u8 = 48;
    pub const VPM_BUSY: u8 = 49;
    pub const VPM_WAIT: u8 = 50;
    pub const MUTEX: u8 = 51;
}

pub mod waddr {
    pub const R0: u8 = 32;
    pub const R1: u8 = 33;
    pub const R2: u8 = 34;
    pub const R3: u8 = 35;
    pub const TMU_NOSWAP: u8 = 36;
    pub const R5: u8 = 37;
    pub const HOST_INT: u8 = 38;
    pub const NOP: u8 = 39;
    pub const UNIFORMS_ADDRESS: u8 = 40;
    pub const QUAD_XY: u8 = 41;
    pub const MS_FLAGS: u8 = 42;
    pub const TLB_STENCIL: u8 = 43;
    pub const TLB_Z: u8 = 44;
    pub const TLB_COLOR_MS: u8 = 45;
    pub const TLB_COLOR_ALL: u8 = 46;
    pub const TLB_ALPHA_MASK: u8 = 47;
    pub const VPM: u8 = 48;
    pub const VPM_SETUP: u8 = 49;
    pub const VPM_ADDR: u8 = 50;
    pub const MUTEX_RELEASE: u8 = 51;
    pub const SFU_RECIP: u8 = 52;
    pub const SFU_RECIPSQRT: u8 = 53;
    pub const SFU_EXP: u8 = 54;
    pub const SFU_LOG: u8 = 55;
    pub const TMU0_S: u8 = 56;
    pub const TMU0_T: u8 = 57;
    pub const TMU0_R: u8 = 58;
    pub const TMU0_B: u8 = 59;
    pub const TMU1_S: u8 = 60;
    pub const TMU1_B: u8 = 63;
}

pub mod op {
    pub const A_NOP: u8 = 0;
    pub const FADD: u8 = 1;
    pub const FSUB: u8 = 2;
    pub const FMIN: u8 = 3;
    pub const FMAX: u8 = 4;
    pub const FMINABS: u8 = 5;
    pub const FMAXABS: u8 = 6;
    pub const FTOI: u8 = 7;
    pub const ITOF: u8 = 8;
    pub const ADD: u8 = 12;
    pub const SUB: u8 = 13;
    pub const SHR: u8 = 14;
    pub const ASR: u8 = 15;
    pub const ROR: u8 = 16;
    pub const SHL: u8 = 17;
    pub const MIN: u8 = 18;
    pub const MAX: u8 = 19;
    pub const AND: u8 = 20;
    pub const OR: u8 = 21;
    pub const XOR: u8 = 22;
    pub const NOT: u8 = 23;
    pub const CLZ: u8 = 24;
    pub const A_V8ADDS: u8 = 30;
    pub const A_V8SUBS: u8 = 31;
    pub const M_NOP: u8 = 0;
    pub const FMUL: u8 = 1;
    pub const MUL24: u8 = 2;
    pub const V8MULD: u8 = 3;
    pub const V8MIN: u8 = 4;
    pub const V8MAX: u8 = 5;
    pub const M_V8ADDS: u8 = 6;
    pub const M_V8SUBS: u8 = 7;
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum Kind {
    #[default]
    Alu,
    /// load immediate: 32-bit, or per-element 2-bit signed (mode 1) / unsigned (mode 3)
    Ldi(u8),
    /// semaphore: number, decrement (acquire)
    Sem(u8, bool),
    Branch,
}

/// A decoded instruction.
#[derive(Clone, Copy, Debug, Default)]
pub struct Inst {
    pub raw: u64,
    pub kind: Kind,
    pub sig: u8,
    pub unpack: u8,
    pub pm: bool,
    pub pack: u8,
    pub cond_add: u8,
    pub cond_mul: u8,
    pub sf: bool,
    pub ws: bool,
    pub waddr_add: u8,
    pub waddr_mul: u8,
    pub op_mul: u8,
    pub op_add: u8,
    pub raddr_a: u8,
    pub raddr_b: u8,
    pub add_a: u8,
    pub add_b: u8,
    pub mul_a: u8,
    pub mul_b: u8,
    pub imm: u32,
    pub bcond: u8,
    pub rel: bool,
    pub reg: bool,
    /// pre-decoded: an ALU instruction without unpack, pack or vector rotation, which
    /// [`Qpu::alu_fast`] runs (`imm` then holds the small immediate's value)
    pub fast: bool,
}

#[inline]
fn f(w: u64, hi: u32, lo: u32) -> u8 { ((w >> lo) & ((1u64 << (hi - lo + 1)) - 1)) as u8 }

pub fn decode(w: u64) -> Inst {
    let sig = f(w, 63, 60);
    let mut i = Inst { raw: w, sig, ..Default::default() };
    i.waddr_add = f(w, 43, 38);
    i.waddr_mul = f(w, 37, 32);
    i.ws = w >> 44 & 1 != 0;
    match sig {
        sig::BRANCH => {
            i.kind = Kind::Branch;
            i.bcond = f(w, 55, 52);
            i.rel = w >> 51 & 1 != 0;
            i.reg = w >> 50 & 1 != 0;
            i.raddr_a = f(w, 49, 45);
            i.imm = w as u32;
        }
        sig::LOAD_IMM => {
            let mode = f(w, 59, 57);
            i.kind = if mode == 4 { Kind::Sem(w as u8 & 15, w >> 4 & 1 != 0) } else { Kind::Ldi(mode) };
            i.pm = w >> 56 & 1 != 0;
            i.pack = f(w, 55, 52);
            i.cond_add = f(w, 51, 49);
            i.cond_mul = f(w, 48, 46);
            i.sf = w >> 45 & 1 != 0;
            i.imm = w as u32;
        }
        _ => {
            i.unpack = f(w, 59, 57);
            i.pm = w >> 56 & 1 != 0;
            i.pack = f(w, 55, 52);
            i.cond_add = f(w, 51, 49);
            i.cond_mul = f(w, 48, 46);
            i.sf = w >> 45 & 1 != 0;
            i.op_mul = f(w, 31, 29);
            i.op_add = f(w, 28, 24);
            i.raddr_a = f(w, 23, 18);
            i.raddr_b = f(w, 17, 12);
            i.add_a = f(w, 11, 9);
            i.add_b = f(w, 8, 6);
            i.mul_a = f(w, 5, 3);
            i.mul_b = f(w, 2, 0);
            if sig == sig::SMALL_IMM { i.imm = small_imm(i.raddr_b); }
            i.fast = fast_path() && i.unpack == 0 && i.pack == 0 && !(sig == sig::SMALL_IMM && i.raddr_b >= 48);
        }
    }
    i
}

/// Whether ALU instructions take the fast path (`SBC_VC4_QPU_FAST=0`: all take the general one).
fn fast_path() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| std::env::var("SBC_VC4_QPU_FAST").map_or(true, |v| v != "0"))
}

/// The value of a small immediate (raddr_b with the small-immediate signal); 48..63 are the
/// multiply pipe's vector rotations and read as 0.
pub fn small_imm(n: u8) -> u32 {
    match n {
        0..=15 => n as u32,
        16..=31 => (n as i32 - 32) as u32,
        32..=39 => 2f32.powi(n as i32 - 32).to_bits(),
        40..=47 => 2f32.powi(n as i32 - 48).to_bits(),
        _ => 0,
    }
}

/// Add-pipe operations that read floats (unpack converts to float for them).
#[inline]
pub fn add_float_in(o: u8) -> bool { (op::FADD..=op::FTOI).contains(&o) }
/// Add-pipe operations that produce floats (pack converts from float for them).
#[inline]
pub fn add_float_out(o: u8) -> bool { (op::FADD..=op::FMAXABS).contains(&o) || o == op::ITOF }

/// Flush a denormal to a signed zero (the QPU's float unit does not keep denormals).
#[inline(always)]
pub fn ff(u: u32) -> f32 { if u & 0x7f80_0000 == 0 { f32::from_bits(u & 0x8000_0000) } else { f32::from_bits(u) } }
#[inline(always)]
pub fn fo(x: f32) -> u32 { let u = x.to_bits(); if u & 0x7f80_0000 == 0 { u & 0x8000_0000 } else { u } }

pub fn add_op(o: u8, a: u32, b: u32) -> u32 {
    match o {
        op::FADD => fo(ff(a) + ff(b)),
        op::FSUB => fo(ff(a) - ff(b)),
        op::FMIN => { let (x, y) = (ff(a), ff(b)); fo(if x.is_nan() { y } else if y.is_nan() { x } else if x < y || (x == y && x.is_sign_negative()) { x } else { y }) }
        op::FMAX => { let (x, y) = (ff(a), ff(b)); fo(if x.is_nan() { y } else if y.is_nan() { x } else if x > y || (x == y && y.is_sign_negative()) { x } else { y }) }
        op::FMINABS => fo(ff(a).abs().min(ff(b).abs())),
        op::FMAXABS => fo(ff(a).abs().max(ff(b).abs())),
        op::FTOI => ff(a) as i32 as u32,
        op::ITOF => fo(a as i32 as f32),
        op::ADD => a.wrapping_add(b),
        op::SUB => a.wrapping_sub(b),
        op::SHR => a >> (b & 31),
        op::ASR => ((a as i32) >> (b & 31)) as u32,
        op::ROR => a.rotate_right(b & 31),
        op::SHL => a << (b & 31),
        op::MIN => (a as i32).min(b as i32) as u32,
        op::MAX => (a as i32).max(b as i32) as u32,
        op::AND => a & b,
        op::OR => a | b,
        op::XOR => a ^ b,
        op::NOT => !a,
        op::CLZ => a.leading_zeros(),
        op::A_V8ADDS => v8(a, b, |x, y| (x + y).min(255)),
        op::A_V8SUBS => v8(a, b, |x, y| x.saturating_sub(y)),
        _ => 0,
    }
}

pub fn mul_op(o: u8, a: u32, b: u32) -> u32 {
    match o {
        op::FMUL => fo(ff(a) * ff(b)),
        op::MUL24 => (a & 0xff_ffff).wrapping_mul(b & 0xff_ffff),
        op::V8MULD => v8(a, b, |x, y| { let t = x * y + 128; (t + (t >> 8)) >> 8 }),
        op::V8MIN => v8(a, b, |x, y| x.min(y)),
        op::V8MAX => v8(a, b, |x, y| x.max(y)),
        op::M_V8ADDS => v8(a, b, |x, y| (x + y).min(255)),
        op::M_V8SUBS => v8(a, b, |x, y| x.saturating_sub(y)),
        _ => 0,
    }
}

/// An add-pipe operation on all 16 lanes (one dispatch, then a loop the compiler vectorises).
pub fn add_vec(o: u8, a: &V, b: &V) -> V {
    macro_rules! each { ($f:expr) => {{ let f = $f; let mut r = ZERO; for l in 0..16 { r[l] = f(a[l], b[l]); } r }} }
    match o {
        op::FADD => each!(|x: u32, y: u32| fo(ff(x) + ff(y))),
        op::FSUB => each!(|x: u32, y: u32| fo(ff(x) - ff(y))),
        op::ADD => each!(|x: u32, y: u32| x.wrapping_add(y)),
        op::SUB => each!(|x: u32, y: u32| x.wrapping_sub(y)),
        op::AND => each!(|x: u32, y: u32| x & y),
        op::OR => each!(|x: u32, y: u32| x | y),
        op::XOR => each!(|x: u32, y: u32| x ^ y),
        op::SHL => each!(|x: u32, y: u32| x << (y & 31)),
        op::SHR => each!(|x: u32, y: u32| x >> (y & 31)),
        op::ASR => each!(|x: u32, y: u32| ((x as i32) >> (y & 31)) as u32),
        op::MIN => each!(|x: u32, y: u32| (x as i32).min(y as i32) as u32),
        op::MAX => each!(|x: u32, y: u32| (x as i32).max(y as i32) as u32),
        _ => each!(|x, y| add_op(o, x, y)),
    }
}

/// A multiply-pipe operation on all 16 lanes.
pub fn mul_vec(o: u8, a: &V, b: &V) -> V {
    macro_rules! each { ($f:expr) => {{ let f = $f; let mut r = ZERO; for l in 0..16 { r[l] = f(a[l], b[l]); } r }} }
    match o {
        op::FMUL => each!(|x: u32, y: u32| fo(ff(x) * ff(y))),
        op::MUL24 => each!(|x: u32, y: u32| (x & 0xff_ffff).wrapping_mul(y & 0xff_ffff)),
        op::V8MIN if a == b => *a,
        _ => each!(|x, y| mul_op(o, x, y)),
    }
}

#[inline]
fn v8(a: u32, b: u32, g: impl Fn(u32, u32) -> u32) -> u32 {
    let mut r = 0;
    for k in 0..4 { r |= g(a >> (8 * k) & 0xff, b >> (8 * k) & 0xff) << (8 * k); }
    r
}

/// Unpack of a regfile-A read (PM=0) or an r4 read (PM=1), as the consuming operation sees it.
pub fn unpack(mode: u8, v: u32, float_in: bool) -> u32 {
    match mode {
        0 => v,
        1 | 2 => {
            let h = if mode == 1 { v & 0xffff } else { v >> 16 } as u16;
            if float_in { f16_to_f32(h).to_bits() } else { h as i16 as i32 as u32 }
        }
        3 => { let b = v >> 24; if float_in { (b as f32 / 255.0).to_bits() } else { b * 0x0101_0101 } }
        _ => {
            let b = v >> (8 * (mode - 4)) & 0xff;
            if float_in { (b as f32 / 255.0).to_bits() } else { b }
        }
    }
}

/// A float in [0, 1] as an 8-bit unorm value (the multiply pipe's colour packing).
#[inline]
pub fn unorm8(u: u32) -> u32 {
    let x = ff(u);
    if !(x > 0.0) { return 0; }
    if x >= 1.0 { return 255; }
    (x * 255.0 + 0.5) as u32
}

/// Regfile-A pack (PM=0) of `v` into `old`; `wide` is the exact result of an integer add/sub
/// (for the 32-bit saturating mode).
pub fn pack_a(mode: u8, old: u32, v: u32, float_out: bool, wide: i64) -> u32 {
    let sat = mode >= 8;
    match mode {
        0 => v,
        8 => wide.clamp(i32::MIN as i64, i32::MAX as i64) as i32 as u32,
        1 | 2 | 9 | 10 => {
            let h = if float_out { f32_to_f16(ff(v)) as u32 }
                    else if sat { (v as i32).clamp(-32768, 32767) as u32 & 0xffff } else { v & 0xffff };
            if mode == 1 || mode == 9 { (old & 0xffff_0000) | h } else { (old & 0xffff) | h << 16 }
        }
        _ => {
            let b = if float_out { unorm8(v) } else if sat { (v as i32).clamp(0, 255) as u32 } else { v & 0xff };
            match mode & 7 {
                3 => b * 0x0101_0101,
                m => { let s = 8 * (m as u32 - 4); (old & !(0xff << s)) | b << s }
            }
        }
    }
}

/// Multiply-pipe pack (PM=1): the float result as 8-bit colour, replicated or into one byte.
pub fn pack_mul(mode: u8, old: u32, v: u32) -> u32 {
    match mode {
        0 => v,
        3 => unorm8(v) * 0x0101_0101,
        4..=7 => { let s = 8 * (mode as u32 - 4); (old & !(0xff << s)) | unorm8(v) << s }
        _ => v,
    }
}

pub fn f16_to_f32(h: u16) -> f32 {
    let s = (h as u32 & 0x8000) << 16;
    let e = (h >> 10 & 0x1f) as u32;
    let m = (h & 0x3ff) as u32;
    let bits = match e {
        0 => s, // denormal halves flush to zero
        31 => s | 0x7f80_0000 | m << 13,
        _ => s | (e + 112) << 23 | m << 13,
    };
    f32::from_bits(bits)
}

pub fn f32_to_f16(x: f32) -> u16 {
    let u = x.to_bits();
    let s = (u >> 16 & 0x8000) as u16;
    let e = (u >> 23 & 0xff) as i32;
    let m = u & 0x7f_ffff;
    if e == 255 { return s | 0x7c00 | if m != 0 { 0x200 } else { 0 }; }
    let e2 = e - 112;
    if e2 >= 31 { return s | 0x7c00; }
    if e2 <= 0 { return s; }
    // round to nearest even
    let mut r = (e2 as u32) << 10 | m >> 13;
    let rest = m & 0x1fff;
    if rest > 0x1000 || (rest == 0x1000 && r & 1 != 0) { r += 1; }
    s | r as u16
}

/// What a program reaches outside its registers. Defaults: nothing there (zeros).
pub trait Env {
    /// The VPM (16 words per row), and the rows that generic reads and writes start from.
    fn vpm(&mut self) -> (&mut [u32], usize, usize) { (&mut [], 0, 0) }
    /// The next varying: its value as the varying unit gives it, and the C coefficient (r5).
    fn vary(&mut self) -> (V, V) { (ZERO, ZERO) }
    fn pixel_xy(&self) -> (V, V) { (ZERO, ZERO) }
    fn ms_flags(&self) -> V { ZERO }
    fn set_ms_flags(&mut self, _v: &V, _mask: u16) {}
    fn rev_flag(&self) -> V { ZERO }
    fn tlb_stencil(&mut self, _v: u32) {}
    fn tlb_z(&mut self, _v: &V, _mask: u16) {}
    fn tlb_color(&mut self, _v: &V, _mask: u16, _all_samples: bool) {}
    fn tlb_alpha_mask(&mut self, _v: &V, _mask: u16) {}
    fn color_load(&mut self) -> V { ZERO }
    fn coverage_load(&mut self) -> V { ZERO }
    fn alpha_mask_load(&mut self) -> V { ZERO }
    /// Fragment shading: texture LOD from the quads' derivatives.
    fn fragment(&self) -> bool { false }
    /// Semaphore `n`: true if the operation can go ahead (a decrement at zero waits).
    fn semaphore(&mut self, _n: u8, _down: bool) -> bool { true }
    /// Mutex acquire (true: acquired) or release.
    fn mutex(&mut self, _acquire: bool) -> bool { true }
    fn host_int(&mut self) {}
}

/// A TMU request being set up, and the results waiting for `LOAD_TMUn`.
#[derive(Clone, Default, Debug)]
pub struct Tmu {
    /// coordinates written so far: S, T, R, B (lane values, lanes written)
    pub coord: [Option<V>; 4],
    /// configuration words taken from the uniform stream, in order of the writes
    pub cfg: [u32; 4],
    pub ncfg: usize,
    pub writes: usize,
    pub fifo: VecDeque<V>,
}

/// A generic VPM read or write setup and where it has got to.
#[derive(Clone, Copy, Default, Debug)]
pub struct VpmGen {
    pub setup: u32,
    pub addr: u32,
    /// vectors still to read (reads only)
    pub left: u32,
}

impl VpmGen {
    fn set(&mut self, s: u32, read: bool) {
        self.setup = s;
        self.addr = s & 0xff;
        self.left = if read { let n = s >> 20 & 15; if n == 0 { 16 } else { n } } else { u32::MAX };
    }
    fn stride(&self) -> u32 { let s = self.setup >> 12 & 0x3f; if s == 0 { 64 } else { s } }
    /// The word index (row * 16 + column) lane `i` accesses, and the byte/half lane within it
    /// (shift, width in bits: 32 for whole words).
    fn word(&self, i: usize) -> (usize, u32, u32) {
        let (horiz, laned, size, a) = (self.setup >> 11 & 1 != 0, self.setup >> 10 & 1 != 0, self.setup >> 8 & 3, self.addr as usize);
        match (size, horiz) {
            (2, true) | (3, true) => (a * 16 + i, 0, 32),
            (2, false) | (3, false) => (((a >> 4 << 4) + i) * 16 + (a & 15), 0, 32),
            (1, true) => ((a >> 1) * 16 + i, if laned { 16 * (a as u32 & 1) } else { 0 }, if laned { 16 } else { 32 }),
            (1, false) => (((a >> 5 << 4) + i) * 16 + (a >> 1 & 15), if laned { 16 * (a as u32 & 1) } else { 0 }, if laned { 16 } else { 32 }),
            (0, true) => ((a >> 2) * 16 + i, if laned { 8 * (a as u32 & 3) } else { 0 }, if laned { 8 } else { 32 }),
            _ => (((a >> 6 << 4) + i) * 16 + (a >> 2 & 15), if laned { 8 * (a as u32 & 3) } else { 0 }, if laned { 8 } else { 32 }),
        }
    }
}

/// Why a program stopped running.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Stop {
    /// program end (its delay slots executed)
    End,
    /// waiting for a semaphore or the mutex: run it again later
    Blocked,
    /// the instruction budget ran out (an endless loop)
    Budget,
}

/// Decoded-instruction cache (direct-mapped by address), valid for one job.
pub struct Code {
    tags: Vec<u32>,
    insts: Vec<Inst>,
    /// a shader of this job ran out of its instruction budget (an endless loop): the rest of
    /// the job skips its shaders, as the kernel's reset would end it on the hardware, instead
    /// of spending the budget again on every batch
    pub runaway: bool,
}

const CODE_SLOTS: usize = 8192;

impl Default for Code {
    fn default() -> Self { Code { tags: vec![u32::MAX; CODE_SLOTS], insts: vec![Inst::default(); CODE_SLOTS], runaway: false } }
}

impl Code {
    #[inline]
    pub fn fetch(&mut self, pc: u32, mem: &mut Mem) -> Inst {
        let k = (pc >> 3) as usize & (CODE_SLOTS - 1);
        if self.tags[k] != pc {
            let w = mem.r32(pc) as u64 | (mem.r32(pc.wrapping_add(4)) as u64) << 32;
            self.insts[k] = decode(w);
            self.tags[k] = pc;
        }
        self.insts[k]
    }
    pub fn clear(&mut self) { self.tags.iter_mut().for_each(|t| *t = u32::MAX); self.runaway = false; }
}

/// One QPU thread's state.
#[derive(Clone)]
pub struct Qpu {
    pub ra: [V; 32],
    pub rb: [V; 32],
    /// r0..r5
    pub acc: [V; 6],
    /// per-lane flags (bit i = lane i)
    pub z: u16,
    pub n: u16,
    pub c: u16,
    pub pc: u32,
    /// address of the next uniform
    pub unif: u32,
    /// the QPU's number (0..11)
    pub num: u32,
    /// instructions left before the program stops (after a program-end signal)
    end: u8,
    /// a taken branch: target and delay slots left
    br: Option<(u32, u8)>,
    pub tmu: [Tmu; 2],
    pub vrd: VpmGen,
    pub vwr: VpmGen,
    /// VPM DMA setups: load (VDR) and its extended pitch, store (VDW) and its stride
    pub vdr: u32,
    pub vdr_pitch: u32,
    pub vdw: u32,
    pub vdw_stride: u32,
    pub insns: u64,
}

impl Default for Qpu { fn default() -> Self { Qpu::new() } }

impl Qpu {
    pub fn new() -> Qpu {
        Qpu { ra: [ZERO; 32], rb: [ZERO; 32], acc: [ZERO; 6], z: 0, n: 0, c: 0, pc: 0, unif: 0, num: 0, end: 0, br: None,
              tmu: Default::default(), vrd: Default::default(), vwr: Default::default(), vdr: 0, vdr_pitch: 0, vdw: 0, vdw_stride: 0, insns: 0 }
    }

    /// Start a program at `pc` with uniforms at `unif` (registers keep whatever the caller set).
    pub fn start(&mut self, pc: u32, unif: u32) {
        self.pc = pc;
        self.unif = unif;
        self.end = 0;
        self.br = None;
        for t in self.tmu.iter_mut() { *t = Tmu::default(); }
    }

    fn uniform(&mut self, mem: &mut Mem) -> u32 {
        let v = mem.r32(self.unif);
        self.unif = self.unif.wrapping_add(4);
        v
    }

    #[inline]
    fn cond_mask(&self, c: u8) -> u16 {
        match c {
            0 => 0,
            1 => ALL,
            2 => self.z,
            3 => !self.z,
            4 => self.n,
            5 => !self.n,
            6 => self.c,
            _ => !self.c,
        }
    }

    fn branch_taken(&self, c: u8) -> bool {
        match c {
            0 => self.z == ALL,
            1 => self.z == 0,
            2 => self.z != 0,
            3 => self.z != ALL,
            4 => self.n == ALL,
            5 => self.n == 0,
            6 => self.n != 0,
            7 => self.n != ALL,
            8 => self.c == ALL,
            9 => self.c == 0,
            10 => self.c != 0,
            11 => self.c != ALL,
            15 => true,
            _ => false,
        }
    }

    /// Read port A or B (`b`) at raddr `r`: the value and, for a varying read, the C
    /// coefficient for r5.
    fn read(&mut self, r: u8, b: bool, mem: &mut Mem, env: &mut dyn Env, r5: &mut Option<V>) -> V {
        match r {
            0..=31 => if b { self.rb[r as usize] } else { self.ra[r as usize] },
            raddr::UNIF => [self.uniform(mem); 16],
            raddr::VARY => { let (v, c) = env.vary(); *r5 = Some(c); v }
            raddr::ELEM_QPU => if b { [self.num; 16] } else { std::array::from_fn(|i| i as u32) },
            raddr::XY_PIXEL => { let (x, y) = env.pixel_xy(); if b { y } else { x } }
            raddr::MS_REV => if b { env.rev_flag() } else { env.ms_flags() },
            raddr::VPM => self.vpm_read(env),
            _ => ZERO,
        }
    }

    fn vpm_read(&mut self, env: &mut dyn Env) -> V {
        let mut out = ZERO;
        if self.vrd.left == 0 { crate::log_first("vc4: VPM read with no read setup left"); return out; }
        let g = self.vrd;
        let (vpm, base, _) = env.vpm();
        if vpm.is_empty() { return out; }
        let n = vpm.len();
        for (i, o) in out.iter_mut().enumerate() {
            let (w, sh, bits) = g.word(i);
            let v = vpm[(base * 16 + w) % n];
            *o = if bits == 32 { v } else { v >> sh & ((1u32 << bits) - 1) };
        }
        self.vrd.addr = self.vrd.addr.wrapping_add(self.vrd.stride());
        self.vrd.left -= 1;
        out
    }

    fn vpm_write(&mut self, v: &V, mask: u16, env: &mut dyn Env) {
        let g = self.vwr;
        let (vpm, _, base) = env.vpm();
        if vpm.is_empty() { return; }
        let n = vpm.len();
        for i in 0..16 {
            if mask >> i & 1 == 0 { continue; }
            let (w, sh, bits) = g.word(i);
            let slot = &mut vpm[(base * 16 + w) % n];
            *slot = if bits == 32 { v[i] } else { let m = ((1u32 << bits) - 1) << sh; (*slot & !m) | (v[i] << sh & m) };
        }
        self.vwr.addr = self.vwr.addr.wrapping_add(self.vwr.stride());
    }

    /// VPM DMA load (VDR) from memory at `addr`.
    fn vdr_load(&mut self, addr: u32, mem: &mut Mem, env: &mut dyn Env) {
        let s = self.vdr;
        let modew = s >> 28 & 7;
        let mpitch = s >> 24 & 15;
        let rowlen = match s >> 20 & 15 { 0 => 16, n => n };
        let nrows = match s >> 16 & 15 { 0 => 16, n => n };
        let vpitch = match s >> 12 & 15 { 0 => 16, n => n };
        let vert = s >> 11 & 1 != 0;
        let (y, x) = (s >> 4 & 0x7f, s & 15);
        let pitch = if mpitch == 0 { self.vdr_pitch } else { 8 << mpitch };
        if modew != 0 { crate::log_first(&format!("vc4: VPM DMA load in mode {modew} (only 32-bit modelled)")); }
        let (vpm, _, _) = env.vpm();
        if vpm.is_empty() { return; }
        let n = vpm.len();
        for r in 0..nrows {
            for c in 0..rowlen {
                let w = mem.r32(addr.wrapping_add(r * pitch + c * 4));
                let (row, col) = if vert { (y + c, x + r * vpitch) } else { (y + r * vpitch, x + c) };
                vpm[((row as usize) * 16 + (col as usize & 15)) % n] = w;
            }
        }
    }

    /// VPM DMA store (VDW) to memory at `addr`.
    fn vdw_store(&mut self, addr: u32, mem: &mut Mem, env: &mut dyn Env) {
        let s = self.vdw;
        let units = match s >> 23 & 0x7f { 0 => 128, n => n };
        let depth = match s >> 16 & 0x7f { 0 => 128, n => n };
        let horiz = s >> 14 & 1 != 0;
        let base = s >> 3 & 0x7ff;
        let modew = s & 7;
        let (y, x) = (base >> 4, base & 15);
        let stride = self.vdw_stride & 0x1fff;
        if modew != 0 { crate::log_first(&format!("vc4: VPM DMA store in mode {modew} (only 32-bit modelled)")); }
        let (vpm, _, _) = env.vpm();
        if vpm.is_empty() { return; }
        let n = vpm.len();
        let mut a = addr;
        for u in 0..units {
            for d in 0..depth {
                let (row, col) = if horiz { (y + u, x + d) } else { (y + d, x + u) };
                mem.w32(a, vpm[((row as usize) * 16 + (col as usize & 15)) % n]);
                a = a.wrapping_add(4);
            }
            a = a.wrapping_add(stride);
        }
    }

    /// A write to `w` on the A side (`a`) or B side, lanes in `mask`.
    fn write(&mut self, w: u8, a: bool, v: &V, mask: u16, mem: &mut Mem, env: &mut dyn Env) {
        if mask == 0 && w != waddr::NOP { return; }
        let first = mask.trailing_zeros() as usize;
        match w {
            0..=31 => {
                let r = if a { &mut self.ra[w as usize] } else { &mut self.rb[w as usize] };
                if mask == ALL { *r = *v } else { for i in 0..16 { if mask >> i & 1 != 0 { r[i] = v[i]; } } }
            }
            waddr::R0..=waddr::R3 => {
                let r = &mut self.acc[(w - waddr::R0) as usize];
                if mask == ALL { *r = *v } else { for i in 0..16 { if mask >> i & 1 != 0 { r[i] = v[i]; } } }
            }
            waddr::R5 => {
                let src: V = if a { std::array::from_fn(|i| v[i & !3]) } else { [v[0]; 16] };
                for i in 0..16 { if mask >> i & 1 != 0 { self.acc[5][i] = src[i]; } }
            }
            waddr::TMU_NOSWAP | waddr::NOP | waddr::QUAD_XY => {}
            waddr::HOST_INT => env.host_int(),
            waddr::UNIFORMS_ADDRESS => self.unif = v[first],
            waddr::MS_FLAGS => if a { env.set_ms_flags(v, mask) },
            waddr::TLB_STENCIL => env.tlb_stencil(v[first]),
            waddr::TLB_Z => env.tlb_z(v, mask),
            waddr::TLB_COLOR_MS => env.tlb_color(v, mask, false),
            waddr::TLB_COLOR_ALL => env.tlb_color(v, mask, true),
            waddr::TLB_ALPHA_MASK => env.tlb_alpha_mask(v, mask),
            waddr::VPM => self.vpm_write(v, mask, env),
            waddr::VPM_SETUP => {
                let s = v[first];
                if a {
                    if s >> 31 == 0 { self.vrd.set(s, true) } else if s >> 28 == 9 { self.vdr_pitch = s & 0x1fff } else { self.vdr = s }
                } else {
                    match s >> 30 { 0 => self.vwr.set(s, false), 2 => self.vdw = s, 3 => self.vdw_stride = s, _ => {} }
                }
            }
            waddr::VPM_ADDR => if a { self.vdr_load(v[first], mem, env) } else { self.vdw_store(v[first], mem, env) },
            waddr::MUTEX_RELEASE => { env.mutex(false); }
            waddr::SFU_RECIP..=waddr::SFU_LOG => {
                for i in 0..16 {
                    if mask >> i & 1 == 0 { continue; }
                    let x = ff(v[i]);
                    let y = match w { waddr::SFU_RECIP => 1.0 / x, waddr::SFU_RECIPSQRT => 1.0 / x.sqrt(), waddr::SFU_EXP => x.exp2(), _ => x.log2() };
                    self.acc[4][i] = fo(y);
                }
            }
            waddr::TMU0_S..=waddr::TMU1_B => self.tmu_write(w, v, mask, mem, env),
            _ => {}
        }
    }

    fn tmu_write(&mut self, w: u8, v: &V, mask: u16, mem: &mut Mem, env: &mut dyn Env) {
        let (t, reg) = (((w - waddr::TMU0_S) / 4) as usize, ((w - waddr::TMU0_S) % 4) as usize);
        let vals: V = std::array::from_fn(|i| if mask >> i & 1 != 0 { v[i] } else { 0 });
        if reg == 0 && self.tmu[t].writes == 0 {
            // only S written: a direct memory read (32-bit word per lane)
            let r: V = std::array::from_fn(|i| if mask >> i & 1 != 0 { mem.r32(vals[i] & !3) } else { 0 });
            self.tmu[t].fifo.push_back(r);
            return;
        }
        let u = self.uniform(mem);
        let tm = &mut self.tmu[t];
        if tm.ncfg < 4 { tm.cfg[tm.ncfg] = u; tm.ncfg += 1; }
        tm.coord[reg] = Some(vals);
        tm.writes += 1;
        if reg == 0 {
            let req = std::mem::take(&mut tm.coord);
            let (cfg, n) = (tm.cfg, tm.ncfg);
            tm.writes = 0;
            tm.ncfg = 0;
            let r = crate::tmu::lookup(&req, &cfg[..n], mask, env.fragment(), mem);
            self.tmu[t].fifo.push_back(r);
        }
    }

    /// Execute one instruction.
    pub fn step(&mut self, code: &mut Code, mem: &mut Mem, env: &mut dyn Env) -> Option<Stop> {
        let i = code.fetch(self.pc, mem);
        // operations that wait: nothing of the instruction happens until they can go ahead
        match i.kind {
            Kind::Sem(n, down) => if !env.semaphore(n, down) { return Some(Stop::Blocked) },
            Kind::Alu => {
                if (i.raddr_a == raddr::MUTEX || (i.raddr_b == raddr::MUTEX && i.sig != sig::SMALL_IMM)) && !env.mutex(true) {
                    return Some(Stop::Blocked);
                }
            }
            _ => {}
        }
        let pc = self.pc;
        self.pc = pc.wrapping_add(8);
        self.insns += 1;
        let tracing = crate::TRACE.load(std::sync::atomic::Ordering::Relaxed) > 0 && crate::TRACE.fetch_sub(1, std::sync::atomic::Ordering::Relaxed) > 0;
        match i.kind {
            Kind::Branch => {
                if self.branch_taken(i.bcond) {
                    let mut t = i.imm;
                    if i.rel { t = t.wrapping_add(pc.wrapping_add(32)); }
                    if i.reg { t = t.wrapping_add(self.ra[i.raddr_a as usize][0]); }
                    self.br = Some((t, 4));
                }
                let link = [pc.wrapping_add(32); 16];
                let (wa, wm) = (i.waddr_add, i.waddr_mul);
                self.write(wa, !i.ws, &link, ALL, mem, env);
                self.write(wm, i.ws, &link, ALL, mem, env);
            }
            Kind::Ldi(_) | Kind::Sem(_, _) => {
                let mode = if let Kind::Ldi(m) = i.kind { m } else { 0 };
                let v: V = match mode {
                    1 => std::array::from_fn(|l| { let x = (i.imm >> (16 + l) & 1) << 1 | (i.imm >> l & 1); ((x as i32) << 30 >> 30) as u32 }),
                    3 => std::array::from_fn(|l| (i.imm >> (16 + l) & 1) << 1 | (i.imm >> l & 1)),
                    _ => [i.imm; 16],
                };
                let (ma, mm) = (self.cond_mask(i.cond_add), self.cond_mask(i.cond_mul));
                self.write_result(i.waddr_add, !i.ws, &v, ma, &i, false, true, None, mem, env);
                self.write_result(i.waddr_mul, i.ws, &v, mm, &i, false, false, None, mem, env);
                if i.sf { self.set_flags(&v, false, None, ma); }
            }
            _ => if i.fast { self.alu_fast(&i, mem, env) } else { self.alu(&i, mem, env) },
        }
        if tracing {
            let show = |w: u8, a: bool| -> String {
                if w < 32 || (waddr::R0..=waddr::R3).contains(&w) || w == waddr::R5 {
                    let v = self.peek(w, a)[0];
                    format!(" {}={v:#x}({})", crate::disasm::waddr(w, a), f32::from_bits(v))
                } else { String::new() }
            };
            let (wa, wm) = if let Kind::Branch = i.kind { (String::new(), String::new()) } else { (show(i.waddr_add, !i.ws), show(i.waddr_mul, i.ws)) };
            eprintln!("\rqpu{} {pc:#010x}: {}  |{wa}{wm}\r", self.num, crate::disasm::disasm(i.raw));
        }
        // signals
        match i.sig {
            sig::PROG_END => self.end = 3,
            sig::COLOR_LOAD => self.acc[4] = env.color_load(),
            sig::COLOR_LOAD_END => { self.acc[4] = env.color_load(); self.end = 3; }
            sig::LOAD_TMU0 | sig::LOAD_TMU1 => {
                let t = (i.sig - sig::LOAD_TMU0) as usize;
                match self.tmu[t].fifo.pop_front() {
                    Some(v) => self.acc[4] = v,
                    None => { crate::log_first("vc4: TMU load with no request outstanding"); self.acc[4] = ZERO; }
                }
            }
            sig::COVERAGE_LOAD => self.acc[4] = env.coverage_load(),
            sig::ALPHA_MASK_LOAD => self.acc[4] = env.alpha_mask_load(),
            _ => {}
        }
        if self.end > 0 {
            self.end -= 1;
            if self.end == 0 { return Some(Stop::End); }
        }
        if let Some((t, n)) = self.br {
            if n <= 1 { self.pc = t; self.br = None; } else { self.br = Some((t, n - 1)); }
        }
        None
    }

    /// Set the flags from `r` in the lanes of `mask` (the condition of the operation they come
    /// from: a conditional instruction sets flags only where it executes).
    fn set_flags(&mut self, r: &V, float: bool, carry: Option<&V>, mask: u16) {
        let (mut z, mut n, mut c) = (0u16, 0u16, 0u16);
        for i in 0..16 {
            let b = 1 << i;
            if float {
                let x = ff(r[i]);
                if x == 0.0 { z |= b; }
                if x < 0.0 { n |= b; }
                if x > 0.0 { c |= b; }
            } else {
                if r[i] == 0 { z |= b; }
                if (r[i] as i32) < 0 { n |= b; }
                if let Some(cv) = carry { if cv[i] != 0 { c |= b; } }
            }
        }
        self.z = (self.z & !mask) | (z & mask);
        self.n = (self.n & !mask) | (n & mask);
        self.c = (self.c & !mask) | (c & mask);
    }

    /// Write an ALU (or load-immediate) result: packing, then the destination.
    #[allow(clippy::too_many_arguments)]
    fn write_result(&mut self, w: u8, file_a: bool, v: &V, mask: u16, i: &Inst, float_out: bool, is_add: bool, wide: Option<&[i64; 16]>, mem: &mut Mem, env: &mut dyn Env) {
        if w == waddr::NOP || mask == 0 { return; }
        if i.pm {
            if !is_add && i.pack != 0 {
                let old = self.peek(w, file_a);
                let p: V = std::array::from_fn(|l| pack_mul(i.pack, old[l], v[l]));
                return self.write(w, file_a, &p, mask, mem, env);
            }
        } else if i.pack != 0 && file_a && w < 32 {
            let old = self.ra[w as usize];
            let p: V = std::array::from_fn(|l| pack_a(i.pack, old[l], v[l], float_out, wide.map_or(v[l] as i32 as i64, |w| w[l])));
            return self.write(w, file_a, &p, mask, mem, env);
        }
        self.write(w, file_a, v, mask, mem, env)
    }

    /// The current value of a destination (for partial-word packs).
    fn peek(&self, w: u8, file_a: bool) -> V {
        match w {
            0..=31 => if file_a { self.ra[w as usize] } else { self.rb[w as usize] },
            waddr::R0..=waddr::R3 => self.acc[(w - waddr::R0) as usize],
            waddr::R5 => self.acc[5],
            _ => ZERO,
        }
    }

    pub(crate) fn alu(&mut self, i: &Inst, mem: &mut Mem, env: &mut dyn Env) {
        let mut r5_vary = None;
        let small = i.sig == sig::SMALL_IMM;
        let va = if i.raddr_a != raddr::NOP { self.read(i.raddr_a, false, mem, env, &mut r5_vary) } else { ZERO };
        let vb = if small { [small_imm(i.raddr_b); 16] } else if i.raddr_b != raddr::NOP { self.read(i.raddr_b, true, mem, env, &mut r5_vary) } else { ZERO };
        let (af, mf) = (add_float_in(i.op_add), i.op_mul == op::FMUL);
        let operand = |s: &Self, mux: u8, float_in: bool| -> V {
            let raw = match mux { 0..=5 => s.acc[mux as usize], 6 => va, _ => vb };
            if i.unpack != 0 && ((!i.pm && mux == 6) || (i.pm && mux == 4)) {
                std::array::from_fn(|l| unpack(i.unpack, raw[l], float_in))
            } else { raw }
        };
        let sat32 = !i.pm && i.pack == 8;
        let add_res = if i.op_add != op::A_NOP {
            let (a, b) = (operand(self, i.add_a, af), operand(self, i.add_b, af));
            let r = add_vec(i.op_add, &a, &b);
            let carry: Option<V> = if !i.sf { None } else {
                match i.op_add {
                    op::ADD => Some(std::array::from_fn(|l| (a[l] as u64 + b[l] as u64 > u32::MAX as u64) as u32)),
                    op::SUB => Some(std::array::from_fn(|l| (a[l] < b[l]) as u32)),
                    _ => None,
                }
            };
            let wide: Option<[i64; 16]> = if !sat32 { None } else {
                Some(match i.op_add {
                    op::ADD => std::array::from_fn(|l| a[l] as i32 as i64 + b[l] as i32 as i64),
                    op::SUB => std::array::from_fn(|l| a[l] as i32 as i64 - b[l] as i32 as i64),
                    _ => std::array::from_fn(|l| r[l] as i32 as i64),
                })
            };
            Some((r, carry, wide))
        } else { None };
        let mul_res = if i.op_mul != op::M_NOP {
            let (mut a, mut b) = (operand(self, i.mul_a, mf), operand(self, i.mul_b, mf));
            if small && i.raddr_b >= 48 {
                let n = if i.raddr_b == 48 { self.acc[5][0] & 15 } else { (i.raddr_b - 48) as u32 } as usize;
                a = std::array::from_fn(|l| a[(l + 16 - n) & 15]);
                b = std::array::from_fn(|l| b[(l + 16 - n) & 15]);
            }
            Some(mul_vec(i.op_mul, &a, &b))
        } else { None };
        if let Some((r, _, wide)) = &add_res {
            let m = self.cond_mask(i.cond_add);
            self.write_result(i.waddr_add, !i.ws, r, m, i, add_float_out(i.op_add), true, wide.as_ref(), mem, env);
        }
        if let Some(r) = &mul_res {
            let m = self.cond_mask(i.cond_mul);
            self.write_result(i.waddr_mul, i.ws, r, m, i, i.op_mul == op::FMUL, false, None, mem, env);
        }
        if let Some(c) = r5_vary { self.acc[5] = c; }
        if i.sf {
            if let Some((r, carry, _)) = &add_res { self.set_flags(r, add_float_out(i.op_add), carry.as_ref(), self.cond_mask(i.cond_add)); }
            else if let Some(r) = &mul_res { self.set_flags(r, i.op_mul == op::FMUL, None, self.cond_mask(i.cond_mul)); }
        }
    }

    /// An operand by its mux: an accumulator or a port's value.
    #[inline(always)]
    fn mux<'s>(&'s self, m: u8, va: &'s V, vb: &'s V) -> &'s V {
        match m { 0..=5 => &self.acc[m as usize], 6 => va, _ => vb }
    }

    /// A result to a destination: registers and accumulators here, the rest through `write`.
    #[inline(always)]
    fn put(&mut self, w: u8, a: bool, v: &V, mask: u16, mem: &mut Mem, env: &mut dyn Env) {
        if w == waddr::NOP || mask == 0 { return; }
        let r = match w {
            0..=31 => if a { &mut self.ra[w as usize] } else { &mut self.rb[w as usize] },
            waddr::R0..=waddr::R3 => &mut self.acc[(w - waddr::R0) as usize],
            _ => return self.write(w, a, v, mask, mem, env),
        };
        if mask == ALL { *r = *v } else { for l in 0..16 { if mask >> l & 1 != 0 { r[l] = v[l]; } } }
    }

    /// [`Qpu::alu`] for instructions without unpack, pack or rotation (`Inst::fast`): the same
    /// reads, operations, writes and flags in the same order, without the general path's
    /// per-operand unpacking and packing.
    #[inline(always)]
    pub(crate) fn alu_fast(&mut self, i: &Inst, mem: &mut Mem, env: &mut dyn Env) {
        let mut r5_vary = None;
        let va = match i.raddr_a { r @ 0..=31 => self.ra[r as usize], raddr::NOP => ZERO, r => self.read(r, false, mem, env, &mut r5_vary) };
        let vb = if i.sig == sig::SMALL_IMM { [i.imm; 16] } else {
            match i.raddr_b { r @ 0..=31 => self.rb[r as usize], raddr::NOP => ZERO, r => self.read(r, true, mem, env, &mut r5_vary) }
        };
        let (has_add, has_mul) = (i.op_add != op::A_NOP, i.op_mul != op::M_NOP);
        let add = if has_add { add_vec(i.op_add, self.mux(i.add_a, &va, &vb), self.mux(i.add_b, &va, &vb)) } else { ZERO };
        let carry: Option<V> = if i.sf && has_add && (i.op_add == op::ADD || i.op_add == op::SUB) {
            let (a, b) = (self.mux(i.add_a, &va, &vb), self.mux(i.add_b, &va, &vb));
            Some(if i.op_add == op::ADD { std::array::from_fn(|l| a[l].overflowing_add(b[l]).1 as u32) } else { std::array::from_fn(|l| (a[l] < b[l]) as u32) })
        } else { None };
        let mul = if has_mul { mul_vec(i.op_mul, self.mux(i.mul_a, &va, &vb), self.mux(i.mul_b, &va, &vb)) } else { ZERO };
        let (ma, mm) = (self.cond_mask(i.cond_add), self.cond_mask(i.cond_mul));
        if has_add { self.put(i.waddr_add, !i.ws, &add, ma, mem, env); }
        if has_mul { self.put(i.waddr_mul, i.ws, &mul, mm, mem, env); }
        if let Some(c) = r5_vary { self.acc[5] = c; }
        if i.sf {
            if has_add { self.set_flags(&add, add_float_out(i.op_add), carry.as_ref(), ma); }
            else if has_mul { self.set_flags(&mul, i.op_mul == op::FMUL, None, mm); }
        }
    }

    /// Run until the program ends, waits, or `budget` instructions have run.
    pub fn run(&mut self, code: &mut Code, mem: &mut Mem, env: &mut dyn Env, budget: u64) -> Stop {
        let start = self.insns;
        loop {
            if let Some(s) = self.step(code, mem, env) { return s; }
            if self.insns - start >= budget { return Stop::Budget; }
        }
    }
}
