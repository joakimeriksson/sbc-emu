//! The Midgard shader core: an interpreter for the VLIW bundles Mesa's Midgard compiler
//! emits (`src/panfrost/midgard/midgard.h`, `disassemble.c`, `midgard_emit.c`).
//!
//! A shader is a sequence of bundles, each starting with a 4-bit tag (its type and size)
//! and the next bundle's tag:
//!
//! - ALU bundles (1-4 quadwords): a control word enabling up to five units, one register
//!   word per unit, the unit words, an optional branch and optional 128-bit constants
//!   (read as register r26). VMUL and SADD run first, then VADD, SMUL and VLUT see their
//!   results (pipeline registers r24/r25, the condition register r31).
//! - Load/store bundles (1 quadword): two 60-bit load/store instructions.
//! - Texture bundles (1 quadword): one texture instruction, reading coordinates from
//!   r28/r29 and writing results there.
//!
//! Registers are 128 bits, read as 2x64, 4x32, 8x16 or 16x8-bit lanes. r0-r23 are work and
//! uniform registers (uniform n is preloaded into r(23-n)), r26/r27 are the load/store
//! unit's argument registers, r28/r29 the texture unit's.
//!
//! Threads run in groups of up to four (a 2x2 pixel quad for fragment shaders, so
//! derivatives and texture LOD see their neighbours). Divergent threads are scheduled by
//! lowest PC first, so they reconverge where their paths join.

use crate::half::{f16_to_f32, f32_to_f16};

pub const TAG_BREAK: u32 = 1;

/// Bundles a thread may run before it is stopped (a shader stuck in a loop).
const MAX_STEPS: u32 = 1 << 20;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum State {
    Run,
    Done,
    Discarded,
}

#[derive(Clone, Copy)]
pub struct Thread {
    pub r: [u128; 32],
    pub pc: u64,
    pub state: State,
    /// The condition in r31 was written at 16-bit width.
    r31_half: bool,
    /// The last write to r0 was 16-bit (the colour is fp16 at write-out).
    pub r0_half: bool,
    steps: u32,
}

impl Thread {
    pub fn new(pc: u64) -> Self {
        Self { r: [0; 32], pc, state: State::Run, r31_half: false, r0_half: false, steps: 0 }
    }
}

/// Load/store instruction (60 bits, see `midgard_load_store_word`).
#[derive(Clone, Copy, Debug)]
pub struct LdSt {
    pub op: u32,
    pub reg: u32,
    pub mask: u32,
    pub swizzle: u32,
    pub arg_comp: u32,
    pub arg_reg: u32,
    pub bitsize_toggle: bool,
    pub index_format: u32,
    pub index_comp: u32,
    pub index_reg: u32,
    pub index_shift: u32,
    pub offset: i32,
}

impl LdSt {
    fn decode(w: u64) -> Self {
        let f = |s: u32, n: u32| ((w >> s) & ((1u64 << n) - 1)) as u32;
        let off = f(42, 18);
        Self {
            op: f(0, 8),
            reg: f(8, 5),
            mask: f(13, 4),
            swizzle: f(17, 8),
            arg_comp: f(25, 2),
            arg_reg: f(27, 3),
            bitsize_toggle: f(30, 1) != 0,
            index_format: f(31, 2),
            index_comp: f(33, 2),
            index_reg: f(35, 3),
            index_shift: f(38, 4),
            offset: ((off << 14) as i32) >> 14,
        }
    }
}

/// Texture instruction (`midgard_texture_word`).
#[derive(Clone, Copy, Debug)]
pub struct TexWord {
    pub op: u32,
    pub mode: u32,
    pub format: u32,
    pub sampler_register: bool,
    pub texture_register: bool,
    pub lod_register: bool,
    pub offset_register: bool,
    pub in_full: bool,
    pub in_select: u32,
    pub in_upper: bool,
    pub in_swizzle: u32,
    pub out_full: bool,
    pub sampler_type: u32,
    pub out_select: u32,
    pub out_upper: bool,
    pub mask: u32,
    pub outmod: u32,
    pub swizzle: u32,
    pub offset: u32,
    pub bias: u32,
    pub bias_int: i32,
    pub sampler_handle: u32,
    pub texture_handle: u32,
}

impl TexWord {
    fn decode(w: u128) -> Self {
        let f = |s: u32, n: u32| ((w >> s) & ((1u128 << n) - 1)) as u32;
        Self {
            op: f(8, 4),
            mode: f(12, 4),
            format: f(18, 2),
            sampler_register: f(20, 1) != 0,
            texture_register: f(21, 1) != 0,
            lod_register: f(22, 1) != 0,
            offset_register: f(23, 1) != 0,
            in_full: f(24, 1) != 0,
            in_select: f(25, 1),
            in_upper: f(26, 1) != 0,
            in_swizzle: f(27, 8),
            out_full: f(37, 1) != 0,
            sampler_type: f(38, 2),
            out_select: f(40, 1),
            out_upper: f(41, 1) != 0,
            mask: f(42, 4),
            outmod: f(46, 2),
            swizzle: f(48, 8),
            offset: f(68, 12),
            bias: f(80, 8),
            bias_int: (f(88, 8) as u8) as i8 as i32,
            sampler_handle: f(96, 16),
            texture_handle: f(112, 16),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Unit {
    VMul,
    SAdd,
    VAdd,
    SMul,
    Lut,
}

#[derive(Clone, Copy, Debug)]
pub struct AluIns {
    unit: Unit,
    /// `midgard_reg_info`: src1 reg, src2 reg, out reg, src2 immediate.
    src1_reg: u32,
    src2_reg: u32,
    out_reg: u32,
    src2_imm: bool,
    /// The unit word (48 bits vector, 32 bits scalar).
    word: u64,
}

#[derive(Clone, Copy, Debug)]
pub struct Branch {
    op: u32,
    cond: u32,
    /// Destination in quadwords relative to the next bundle.
    offset: i32,
    call_mode: u32,
}

#[derive(Clone, Debug)]
pub enum Bundle {
    Alu { ins: Vec<AluIns>, branch: Option<Branch>, consts: u128, writeout: bool, next_tag: u32, size: u64 },
    LdSt { ins: [LdSt; 2], next_tag: u32 },
    Tex { word: TexWord, next_tag: u32 },
    Invalid,
}

impl Bundle {
    /// Decode the bundle whose first bytes are `q` (up to four quadwords).
    pub fn decode(q: &[u8; 64]) -> Bundle {
        let w0 = u32::from_le_bytes(q[0..4].try_into().unwrap());
        let tag = w0 & 0xf;
        let next_tag = (w0 >> 4) & 0xf;
        match tag {
            2..=4 => Bundle::Tex { word: TexWord::decode(u128::from_le_bytes(q[0..16].try_into().unwrap())), next_tag },
            5 => {
                let v = u128::from_le_bytes(q[0..16].try_into().unwrap());
                let w1 = ((v >> 8) & ((1u128 << 60) - 1)) as u64;
                let w2 = ((v >> 68) & ((1u128 << 60) - 1)) as u64;
                Bundle::LdSt { ins: [LdSt::decode(w1), LdSt::decode(w2)], next_tag }
            }
            8..=15 => {
                let quads = ((tag & 3) + 1) as usize;
                let half = |i: usize| u16::from_le_bytes([q[2 * i], q[2 * i + 1]]) as u64;
                let units: Vec<Unit> = [(17, Unit::VMul), (19, Unit::SAdd), (21, Unit::VAdd), (23, Unit::SMul), (25, Unit::Lut)]
                    .into_iter()
                    .filter(|&(b, _)| w0 >> b & 1 != 0)
                    .map(|(_, u)| u)
                    .collect();
                // Halfwords: [0,1] control word, then one register word per unit, then fields.
                let mut regs = 2;
                let mut p = 2 + units.len();
                let mut ins = Vec::with_capacity(units.len());
                for &u in &units {
                    let reg = half(regs);
                    regs += 1;
                    let n = if matches!(u, Unit::SAdd | Unit::SMul) { 2 } else { 3 };
                    let mut word = 0u64;
                    for k in 0..n {
                        word |= half(p + k) << (16 * k);
                    }
                    p += n;
                    ins.push(AluIns {
                        unit: u,
                        src1_reg: (reg & 31) as u32,
                        src2_reg: ((reg >> 5) & 31) as u32,
                        out_reg: ((reg >> 10) & 31) as u32,
                        src2_imm: reg >> 15 != 0,
                        word,
                    });
                }
                let mut branch = None;
                if w0 >> 26 & 1 != 0 {
                    let b = half(p) as u32;
                    p += 1;
                    let op = b & 7;
                    branch = Some(if op == 1 {
                        Branch { op, cond: 3, offset: ((b >> 9) as i32) << 25 >> 25, call_mode: (b >> 7) & 3 }
                    } else {
                        Branch { op, cond: (b >> 14) & 3, offset: (((b >> 7) & 0x7f) as i32) << 25 >> 25, call_mode: 1 }
                    });
                }
                if w0 >> 27 & 1 != 0 {
                    let b = half(p) | half(p + 1) << 16 | half(p + 2) << 32;
                    p += 3;
                    let lut = ((b >> 32) & 0xffff) as u32;
                    // Mesa duplicates a single 2-bit condition across the LUT.
                    branch = Some(Branch {
                        op: (b & 7) as u32,
                        cond: lut & 3,
                        offset: (((b >> 9) & 0x7f_ffff) as i32) << 9 >> 9,
                        call_mode: ((b >> 7) & 3) as u32,
                    });
                }
                let used_quads = (p * 2).div_ceil(16);
                let consts = if quads > used_quads {
                    let o = (quads - 1) * 16;
                    u128::from_le_bytes(q[o..o + 16].try_into().unwrap())
                } else {
                    0
                };
                Bundle::Alu { ins, branch, consts, writeout: tag >= 12, next_tag, size: quads as u64 * 16 }
            }
            _ => Bundle::Invalid,
        }
    }

    pub fn size(&self) -> u64 {
        match self {
            Bundle::Alu { size, .. } => *size,
            _ => 16,
        }
    }

    pub fn next_tag(&self) -> u32 {
        match self {
            Bundle::Alu { next_tag, .. } | Bundle::LdSt { next_tag, .. } | Bundle::Tex { next_tag, .. } => *next_tag,
            Bundle::Invalid => TAG_BREAK,
        }
    }
}

/// What a shader asks of its job: memory, attributes, varyings, textures and write-out.
pub trait Env {
    /// Fetch 64 bytes of code at `va`.
    fn code(&mut self, va: u64) -> [u8; 64];
    /// A load/store instruction that touches memory or job state, for thread `t`.
    /// Returns the value to write to the destination register (before swizzle/mask) for
    /// loads; `data` is the (swizzled) source register for stores.
    fn ldst(&mut self, t: usize, ins: &LdSt, addr: LdStArgs, data: u128) -> u128;
    /// A texture instruction for the active threads of the group: `coords[t]` are the
    /// coordinates (as fp32), `lod[t]` the register LOD/bias if any. Returns 4 lanes of
    /// raw 32-bit results per thread (fp32, or integers for integer samplers).
    fn texture(&mut self, w: &TexWord, coords: &[[f32; 4]; 4], lod: &[f32; 4], active: u8) -> [[u32; 4]; 4];
    /// Fragment write-out of thread `t`: colour in r0 (fp16 if `half`), r1 parameters
    /// (depth in x, stencil in y, render target << 8 | sample in z).
    fn writeout(&mut self, t: usize, r0: u128, half: bool, r1: u128, r1_written: u8);
}

/// Resolved address operands of a load/store instruction.
#[derive(Clone, Copy, Debug, Default)]
pub struct LdStArgs {
    pub arg: u64,
    pub index: u64,
}

#[inline]
fn lane(r: u128, bits: u32, i: u32) -> u64 {
    ((r >> (bits * i)) & ((1u128 << bits) - 1)) as u64
}

#[inline]
fn set_lane(r: &mut u128, bits: u32, i: u32, v: u64) {
    let m = ((1u128 << bits) - 1) << (bits * i);
    *r = (*r & !m) | (((v as u128) << (bits * i)) & m);
}

fn mode_bits(mode: u32) -> u32 {
    8 << mode
}

fn is_int_op(op: u32) -> bool {
    (0x40..=0x7e).contains(&op) || (0xa0..=0xc1).contains(&op)
}

fn is_conversion(op: u32) -> bool {
    matches!(op, 0x80..=0x83 | 0x88..=0x8b | 0x90..=0x93 | 0x98..=0x9f | 0xb8..=0xbf)
}

fn is_int_out(op: u32) -> bool {
    is_int_op(op) ^ is_conversion(op)
}

fn sext(v: u64, bits: u32) -> i64 {
    ((v << (64 - bits)) as i64) >> (64 - bits)
}

fn mask_of(bits: u32) -> u64 {
    if bits == 64 { u64::MAX } else { (1u64 << bits) - 1 }
}

/// Float value of a lane of `bits` width.
#[inline]
fn to_f(v: u64, bits: u32) -> f64 {
    match bits {
        16 => f16_to_f32(v as u16) as f64,
        32 => f32::from_bits(v as u32) as f64,
        64 => f64::from_bits(v),
        _ => 0.0,
    }
}

#[inline]
fn from_f(f: f64, bits: u32) -> u64 {
    match bits {
        16 => f32_to_f16(f as f32) as u64,
        32 => (f as f32).to_bits() as u64,
        64 => f.to_bits(),
        _ => 0,
    }
}

/// Operands of one vector instruction, expanded to the op's lane width.
struct Operand {
    lanes: [u64; 16],
}

/// Read a vector source (13-bit `midgard_vector_alu_src`) from register value `r`.
fn vector_source(r: u128, src: u32, mode: u32, int_op: bool) -> Operand {
    let modifier = src & 3;
    let expand = (src >> 2) & 7;
    let sw = (src >> 5) & 0xff;
    let sel = |i: u32| (sw >> (2 * (i & 3))) & 3;
    let bits = mode_bits(mode);
    let n = 128 / bits;
    let mut lanes = [0u64; 16];
    let expands = expand >= 4;
    if !expands {
        let (first, second) = match expand {
            1 => (0, 0),
            2 => (1, 1),
            3 => (1, 0),
            _ => (0, 1),
        };
        for i in 0..n {
            lanes[i as usize] = match bits {
                32 => lane(r, 32, sel(i)),
                16 => lane(r, 16, sel(i) + 4 * if i < 4 { first } else { second }),
                8 => {
                    let j = i % 8;
                    let c = 2 * sel(j / 2) + (j & 1) + 8 * if i < 8 { first } else { second };
                    lane(r, 8, c)
                }
                _ => lane(r, 64, ((sw >> (4 * i)) & 3) >> 1),
            };
        }
    } else {
        let half = bits / 2;
        let high = expand == 5 || expand == 7;
        for i in 0..n {
            let raw = match bits {
                32 => lane(r, 16, sel(i) + if high { 4 } else { 0 }),
                16 => {
                    let (first, second) = match expand {
                        4 => (0, 4),
                        5 => (8, 12),
                        6 => (4, 0),
                        _ => (12, 8),
                    };
                    lane(r, 8, sel(i) + if i < 4 { first } else { second })
                }
                64 => lane(r, 32, (((sw >> (4 * i)) & 3) >> 1) + if high { 2 } else { 0 }),
                _ => 0,
            };
            lanes[i as usize] = if int_op {
                match modifier {
                    0 => sext(raw, half) as u64 & mask_of(bits),
                    1 => raw,
                    2 => raw | raw << half,
                    _ => raw << half,
                }
            } else {
                match bits {
                    32 => (f16_to_f32(raw as u16)).to_bits() as u64,
                    64 => (f32::from_bits(raw as u32) as f64).to_bits(),
                    _ => raw,
                }
            };
        }
    }
    if !int_op {
        for v in lanes.iter_mut().take(n as usize) {
            *v = float_mod(*v, bits, modifier);
        }
    }
    Operand { lanes }
}

fn float_mod(v: u64, bits: u32, m: u32) -> u64 {
    let sign = 1u64 << (bits - 1);
    let mut v = v;
    if m & 1 != 0 {
        v &= !sign;
    }
    if m & 2 != 0 {
        v ^= sign;
    }
    v
}

fn float_outmod(f: f64, outmod: u32) -> f64 {
    match outmod {
        1 => if f.is_nan() { 0.0 } else { f.max(0.0) },
        2 => if f.is_nan() { -1.0 } else { f.clamp(-1.0, 1.0) },
        3 => if f.is_nan() { 0.0 } else { f.clamp(0.0, 1.0) },
        _ => f,
    }
}

fn round_mode(f: f64, mode: u32) -> f64 {
    match mode {
        1 => f.trunc(),
        2 => f.floor(),
        3 => f.ceil(),
        _ => {
            let r = f.round();
            if (f - f.trunc()).abs() == 0.5 { 2.0 * (f / 2.0).round() } else { r }
        }
    }
}

/// One lane of a lane-wise op. `a`/`b` are raw lane bits at width `bits`.
fn lane_op(op: u32, a: u64, b: u64, bits: u32) -> u64 {
    let m = mask_of(bits);
    let fa = || to_f(a, bits);
    let fb = || to_f(b, bits);
    let ia = sext(a, bits);
    let ib = sext(b, bits);
    let t = |c: bool| if c { m } else { 0 };
    match op {
        0x10..=0x13 => from_f(fa() + fb(), bits),
        0x14..=0x17 => from_f(fa() * fb(), bits),
        0x28 | 0x29 => from_f(if op == 0x29 && (fa().is_nan() || fb().is_nan()) { f64::NAN } else { fa().min(fb()) }, bits),
        0x2a | 0x2b => from_f(fa().abs().min(fb().abs()), bits),
        0x2c | 0x2d => from_f(if op == 0x2d && (fa().is_nan() || fb().is_nan()) { f64::NAN } else { fa().max(fb()) }, bits),
        0x2e | 0x2f => from_f(fa().abs().max(fb().abs()), bits),
        0x30..=0x33 => b, // fmov: value from src2
        0x34 => from_f(round_mode(fa(), 0), bits),
        0x35 => from_f(fa().trunc(), bits),
        0x36 => from_f(fa().floor(), bits),
        0x37 => from_f(fa().ceil(), bits),
        0xc6 => from_f(fa().round(), bits),
        0x40 => a.wrapping_add(b) & m,
        0x41 => (a << 1).wrapping_add(b) & m,
        0x46 => a.wrapping_sub(b) & m,
        0x47 => (a << 1).wrapping_sub(b) & m,
        0x48 => {
            let max = (m >> 1) as i64;
            (ia.saturating_add(ib).clamp(-max - 1, max) as u64) & m
        }
        0x49 => (a + b).min(m),
        0x4e => {
            let max = (m >> 1) as i64;
            (ia.saturating_sub(ib).clamp(-max - 1, max) as u64) & m
        }
        0x4f => a.saturating_sub(b),
        0x58 => a.wrapping_mul(b) & m,
        0x60 => (ia.min(ib) as u64) & m,
        0x61 => a.min(b),
        0x62 => (ia.max(ib) as u64) & m,
        0x63 => a.max(b),
        0x64 => (((ia + ib) >> 1) as u64) & m,
        0x65 => (a + b) >> 1,
        0x66 => (((ia + ib + 1) >> 1) as u64) & m,
        0x67 => (a + b + 1) >> 1,
        0x68 => ((ia >> (b & (bits as u64 - 1))) as u64) & m,
        0x69 => a >> (b & (bits as u64 - 1)),
        0x6c | 0x6e => (a << (b & (bits as u64 - 1))) & m,
        0x6d => (a << (b & (bits as u64 - 1))) & m,
        0x70 => a & b,
        0x71 => a | b,
        0x72 => !(a & b) & m,
        0x73 => !(a | b) & m,
        0x74 => a & !b & m,
        0x75 => (a | !b) & m,
        0x76 => a ^ b,
        0x77 => !(a ^ b) & m,
        0x78 => (a.leading_zeros() - (64 - bits)) as u64,
        0x7a => a.count_ones() as u64,
        0x7b => b, // imov
        0x7c => ia.abs_diff(ib) & m,
        0x7d => a.abs_diff(b),
        0x80 => t(fa() == fb()),
        0x81 => t(fa() != fb()),
        0x82 => t(fa() < fb()),
        0x83 => t(fa() <= fb()),
        0x98..=0x9b => {
            let f = round_mode(fa(), op - 0x98);
            let max = (m >> 1) as i64;
            if f.is_nan() { 0 } else { ((f.clamp(-(max as f64) - 1.0, max as f64)) as i64 as u64) & m }
        }
        0x9c..=0x9f => {
            let f = round_mode(fa(), op - 0x9c);
            if f.is_nan() || f <= 0.0 { 0 } else { (f.min(m as f64) as u64) & m }
        }
        0xa0 => t(a == b),
        0xa1 => t(a != b),
        0xa2 => t(a < b),
        0xa3 => t(a <= b),
        0xa4 => t(ia < ib),
        0xa5 => t(ia <= ib),
        0xb8..=0xbb => from_f(ia as f64, bits),
        0xbc..=0xbf => from_f(a as f64, bits),
        0xf0 => from_f(1.0 / fa(), bits),
        0xf2 => from_f(1.0 / fa().sqrt(), bits),
        0xf3 => from_f(fa().sqrt(), bits),
        0xf4 => from_f(fa().exp2(), bits),
        0xf5 => from_f(fa().log2(), bits),
        0xf6 => from_f((std::f64::consts::PI * fa()).sin(), bits),
        0xf7 => from_f((std::f64::consts::PI * fa()).cos(), bits),
        _ => 0,
    }
}

fn is_lut_unary(op: u32) -> bool {
    matches!(op, 0xf0..=0xf7 | 0x34..=0x37 | 0xc6 | 0x78 | 0x7a | 0x98..=0x9f | 0xb8..=0xbf)
}

pub struct Core<'e, E: Env> {
    pub env: &'e mut E,
    cache: std::collections::HashMap<u64, std::rc::Rc<Bundle>>,
    /// Bundles executed (all threads), for timing.
    pub cycles: u64,
    /// Special load/store registers: 3 local storage pointer, 4 local thread id, 5 group
    /// id, 6 global thread id (per thread, set by the job), 2 the TLS stack pointer.
    pub special: [[u64; 8]; 4],
    /// Print each bundle and the registers it changes for the first thread
    /// (`SBC_MALI_ISA=1`).
    pub trace: bool,
}

impl<'e, E: Env> Core<'e, E> {
    pub fn new(env: &'e mut E) -> Self {
        Self { env, cache: Default::default(), cycles: 0, special: [[0; 8]; 4], trace: std::env::var("SBC_MALI_ISA").is_ok() }
    }

    fn bundle(&mut self, pc: u64) -> std::rc::Rc<Bundle> {
        if let Some(b) = self.cache.get(&pc) {
            return b.clone();
        }
        let bytes = self.env.code(pc);
        let b = std::rc::Rc::new(Bundle::decode(&bytes));
        self.cache.insert(pc, b.clone());
        b
    }

    /// Run a group of up to four threads to completion.
    pub fn run(&mut self, threads: &mut [Thread]) {
        loop {
            let Some(pc) = threads.iter().filter(|t| t.state == State::Run).map(|t| t.pc).min() else {
                return;
            };
            let mut group = 0u8;
            for (k, t) in threads.iter_mut().enumerate() {
                if t.state == State::Run && t.pc == pc {
                    group |= 1 << k;
                    t.steps += 1;
                    if t.steps > MAX_STEPS {
                        t.state = State::Done;
                        group &= !(1 << k);
                    }
                }
            }
            if group == 0 {
                continue;
            }
            let b = self.bundle(pc);
            self.cycles += 1;
            let before = if self.trace && group & 1 != 0 { Some(threads[0].r) } else { None };
            if before.is_some() {
                eprintln!("[isa] {pc:#x} {:?}", b);
            }
            let next = pc + b.size();
            let mut target: [Option<u64>; 4] = [None; 4];
            match &*b {
                Bundle::Alu { ins, branch, consts, writeout, .. } => {
                    for k in 0..threads.len() {
                        if group & (1 << k) == 0 {
                            continue;
                        }
                        let t = &mut threads[k];
                        let r1_before = t.r[1];
                        exec_alu(t, ins, *consts);
                        if let Some(br) = branch {
                            target[k] = self.branch(k, t, br, next, *writeout, r1_before);
                        }
                    }
                }
                Bundle::LdSt { ins, .. } => {
                    for k in 0..threads.len() {
                        if group & (1 << k) == 0 {
                            continue;
                        }
                        for i in ins.iter() {
                            if i.op != 0x03 && i.op != 0 {
                                self.exec_ldst(k, &mut threads[k], i);
                            }
                        }
                    }
                }
                Bundle::Tex { word, .. } => self.exec_tex(threads, group, word),
                Bundle::Invalid => {
                    for k in 0..threads.len() {
                        if group & (1 << k) != 0 {
                            threads[k].state = State::Done;
                        }
                    }
                    continue;
                }
            }
            if let Some(before) = before {
                for (i, (a, n)) in before.iter().zip(threads[0].r.iter()).enumerate() {
                    if a != n {
                        let l = |v: u128| (0..4).map(|c| format!("{:08x}", (v >> (32 * c)) as u32)).collect::<Vec<_>>().join(" ");
                        eprintln!("[isa]   r{i} = {} ({:?})", l(*n), (0..4).map(|c| f32::from_bits((*n >> (32 * c)) as u32)).collect::<Vec<_>>());
                    }
                }
            }
            let end = b.next_tag() == TAG_BREAK;
            for k in 0..threads.len() {
                if group & (1 << k) == 0 || threads[k].state != State::Run {
                    continue;
                }
                match target[k] {
                    Some(t) => threads[k].pc = t,
                    None if end => threads[k].state = State::Done,
                    None => threads[k].pc = next,
                }
            }
        }
    }

    /// The branch of an ALU bundle: returns the new PC if taken.
    fn branch(&mut self, k: usize, t: &mut Thread, br: &Branch, next: u64, _writeout: bool, r1_before: u128) -> Option<u64> {
        let c = if t.r31_half { lane(t.r[31], 16, 3) != 0 } else { lane(t.r[31], 32, 3) != 0 };
        let taken = match br.cond {
            1 => !c,
            2 => c,
            3 => true,
            _ => false,
        };
        let dest = (next as i64 + br.offset as i64 * 16) as u64;
        match br.op {
            1 => {
                if br.call_mode == 3 {
                    // return from a blend shader: not modelled, the thread ends
                    t.state = State::Done;
                    return None;
                }
                Some(dest)
            }
            2 => taken.then_some(dest),
            4 => {
                if taken {
                    t.state = State::Discarded;
                }
                None
            }
            7 => {
                // r1 lanes written by this bundle carry the write-out parameters
                let mut written = 0u8;
                for i in 0..4 {
                    if lane(t.r[1], 32, i) != lane(r1_before, 32, i) {
                        written |= 1 << i;
                    }
                }
                self.env.writeout(k, t.r[0], t.r0_half, t.r[1], written);
                None
            }
            _ => None, // tilebuffer wait: always ready
        }
    }

    fn ldst_reg(&self, k: usize, t: &Thread, reg: u32, comp: u32, wide: bool) -> u64 {
        match reg {
            0 | 1 => {
                let r = t.r[26 + reg as usize];
                if wide {
                    lane(r, 32, comp) | lane(r, 32, (comp + 1) & 3) << 32
                } else {
                    lane(r, 32, comp)
                }
            }
            7 => 0,
            n => self.special[k][n as usize],
        }
    }

    fn exec_ldst(&mut self, k: usize, t: &mut Thread, i: &LdSt) {
        let op = i.op;
        let sw = |v: u128, s: u32| -> u128 {
            let mut o = 0u128;
            for c in 0..4 {
                set_lane(&mut o, 32, c, lane(v, 32, (s >> (2 * c)) & 3));
            }
            o
        };
        let write_dest = |t: &mut Thread, v: u128, mask: u32| {
            let r = &mut t.r[i.reg as usize];
            for c in 0..4 {
                if mask & (1 << c) != 0 {
                    set_lane(r, 32, c, lane(v, 32, c));
                }
            }
            if i.reg == 0 {
                t.r0_half = false;
            }
        };
        let is_store = (0xc0..=0xdf).contains(&op) || (0xe8..=0xea).contains(&op);
        match op {
            // register-to-register ops
            0x10 => {
                let v = sw(t.r[26 + (i.arg_reg & 1) as usize], i.swizzle);
                write_dest(t, v, i.mask);
            }
            0x11..=0x13 => {
                let src = t.r[26 + (i.arg_reg & 1) as usize];
                let get = |c: u32| -> f32 {
                    if i.bitsize_toggle { f32::from_bits(lane(src, 32, c) as u32) } else { f16_to_f32(lane(src, 16, c) as u16) }
                };
                let d = get(op - 0x10);
                let out = [get(0) / d, get(1) / d, get(2) / d, 1.0];
                let mut v = 0u128;
                for c in 0..4 {
                    set_lane(&mut v, 32, c, out[c as usize].to_bits() as u64);
                }
                write_dest(t, v, i.mask);
            }
            0x0e => {
                // cube map coordinates: select the major axis, project to the face
                let src = t.r[26 + (i.arg_reg & 1) as usize];
                let f = |c: u32| f32::from_bits(lane(src, 32, (i.swizzle >> (2 * c)) & 3) as u32);
                let (x, y, z) = (f(0), f(1), f(2));
                let (ax, ay, az) = (x.abs(), y.abs(), z.abs());
                let (face, sc, tc, ma) = if ax >= ay && ax >= az {
                    if x > 0.0 { (0, -z, -y, ax) } else { (1, z, -y, ax) }
                } else if ay >= az {
                    if y > 0.0 { (2, x, z, ay) } else { (3, x, -z, ay) }
                } else if z > 0.0 {
                    (4, x, -y, az)
                } else {
                    (5, -x, -y, az)
                };
                // Face and s share x (face * 4 + s), as the texture unit decodes them: the
                // compiler keeps only x and y of the result.
                let s = (0.5 * sc / ma + 0.5).clamp(0.0, 1.0);
                let tt = 0.5 * tc / ma + 0.5;
                let mut v = 0u128;
                set_lane(&mut v, 32, 0, (face as f32 * 4.0 + s).to_bits() as u64);
                set_lane(&mut v, 32, 1, tt.to_bits() as u64);
                write_dest(t, v, i.mask);
            }
            0x0c => {
                // lea: effective address
                let a = self.addr(k, t, i);
                let mut v = 0u128;
                set_lane(&mut v, 64, 0, a);
                write_dest(t, v, i.mask);
            }
            _ => {
                let wide = i.bitsize_toggle;
                let args = LdStArgs {
                    arg: self.ldst_reg(k, t, i.arg_reg, i.arg_comp, wide),
                    index: self.ldst_reg(k, t, i.index_reg, i.index_comp, false),
                };
                if is_store {
                    let data = sw(t.r[26 + (i.reg & 1) as usize], i.swizzle);
                    self.env.ldst(k, i, args, data);
                } else {
                    let v = self.env.ldst(k, i, args, 0);
                    let v = if (0x04..=0x13).contains(&op) { v } else { sw(v, i.swizzle) };
                    write_dest(t, v, i.mask);
                    if op == 0x99 || op == 0x95 || op == 0xb9 || op == 0x9d {
                        if i.reg == 0 {
                            t.r0_half = true;
                        }
                    }
                }
            }
        }
    }

    /// Address of a memory load/store: arg + (index << shift) + offset.
    fn addr(&self, k: usize, t: &Thread, i: &LdSt) -> u64 {
        let arg = self.ldst_reg(k, t, i.arg_reg, i.arg_comp, i.bitsize_toggle);
        let idx = self.ldst_reg(k, t, i.index_reg, i.index_comp, i.index_format == 1);
        let idx = if i.index_format == 3 { idx as u32 as i32 as i64 as u64 } else { idx };
        arg.wrapping_add(idx << i.index_shift).wrapping_add(i.offset as i64 as u64)
    }

    fn exec_tex(&mut self, threads: &mut [Thread], group: u8, w: &TexWord) {
        if w.op == 11 {
            return; // barrier
        }
        let mut coords = [[0f32; 4]; 4];
        let mut lod = [0f32; 4];
        for (k, t) in threads.iter().enumerate() {
            let r = t.r[28 + w.in_select as usize];
            for c in 0..4u32 {
                let s = (w.in_swizzle >> (2 * c)) & 3;
                coords[k][c as usize] = if w.in_full {
                    f32::from_bits(lane(r, 32, s) as u32)
                } else {
                    f16_to_f32(lane(r, 16, s + if w.in_upper { 4 } else { 0 }) as u16)
                };
            }
            if w.lod_register {
                let sel = w.bias;
                let reg = t.r[28 + ((sel >> 1) & 1) as usize];
                let comp = (sel >> 3) & 3;
                lod[k] = if sel & 1 != 0 {
                    f32::from_bits(lane(reg, 32, comp) as u32)
                } else {
                    f16_to_f32(lane(reg, 16, comp + if sel & 4 != 0 { 4 } else { 0 }) as u16)
                };
            }
        }
        let res: [[u32; 4]; 4] = match w.op {
            13 => {
                // derivative across the 2x2 quad (thread k = (x & 1) | (y & 1) << 1)
                let mut o = [[0u32; 4]; 4];
                for k in 0..threads.len().min(4) {
                    for c in 0..4 {
                        let d = if threads.len() < 4 {
                            0.0
                        } else if w.mode == 0 {
                            let row = k & 2;
                            coords[row | 1][c] - coords[row][c]
                        } else {
                            let col = k & 1;
                            coords[col | 2][c] - coords[col][c]
                        };
                        o[k][c] = d.to_bits();
                    }
                }
                o
            }
            10 => {
                let mut o = [[0u32; 4]; 4];
                for k in 0..threads.len().min(4) {
                    for c in 0..4 {
                        o[k][c] = coords[k][c].to_bits();
                    }
                }
                o
            }
            _ => self.env.texture(w, &coords, &lod, group),
        };
        for (k, t) in threads.iter_mut().enumerate() {
            if group & (1 << k) == 0 {
                continue;
            }
            let r = &mut t.r[28 + w.out_select as usize];
            for c in 0..4u32 {
                if w.mask & (1 << c) == 0 {
                    continue;
                }
                let v = res[k][((w.swizzle >> (2 * c)) & 3) as usize];
                let float_out = w.sampler_type == 1 || w.op == 13 || w.op == 10;
                let v = if float_out && w.outmod != 0 {
                    (float_outmod(f32::from_bits(v) as f64, w.outmod) as f32).to_bits()
                } else {
                    v
                };
                if w.out_full {
                    set_lane(r, 32, c, v as u64);
                } else {
                    let h = if float_out { f32_to_f16(f32::from_bits(v)) as u64 } else { v as u64 & 0xffff };
                    set_lane(r, 16, c + if w.out_upper { 4 } else { 0 }, h);
                }
            }
        }
        self.cycles += 4;
    }
}

/// Execute the ALU part of a bundle on one thread.
fn exec_alu(t: &mut Thread, ins: &[AluIns], consts: u128) {
    // Stage 1: VMUL and SADD read the registers as they were; stage 2 sees stage 1.
    let mut pending: [(u32, u128, u128, bool); 2] = [(0, 0, 0, false); 2];
    let mut n = 0;
    for i in ins.iter().filter(|i| matches!(i.unit, Unit::VMul | Unit::SAdd)) {
        if let Some(w) = alu_one(t, i, consts) {
            pending[n] = w;
            n += 1;
        }
    }
    for &(reg, value, mask, half) in &pending[..n] {
        commit(t, reg, value, mask, half);
    }
    let mut n = 0;
    let mut pending2: [(u32, u128, u128, bool); 3] = [(0, 0, 0, false); 3];
    for i in ins.iter().filter(|i| matches!(i.unit, Unit::VAdd | Unit::SMul | Unit::Lut)) {
        if let Some(w) = alu_one(t, i, consts) {
            pending2[n] = w;
            n += 1;
        }
    }
    for &(reg, value, mask, half) in &pending2[..n] {
        commit(t, reg, value, mask, half);
    }
}

fn commit(t: &mut Thread, reg: u32, value: u128, mask: u128, half: bool) {
    let r = &mut t.r[reg as usize];
    *r = (*r & !mask) | (value & mask);
    if reg == 31 {
        t.r31_half = half;
    }
    if reg == 0 {
        t.r0_half = half;
    }
}

fn read_reg(t: &Thread, reg: u32, consts: u128) -> u128 {
    if reg == 26 { consts } else { t.r[reg as usize] }
}

/// Run one ALU instruction; returns (register, value, write mask, 16-bit write).
fn alu_one(t: &Thread, i: &AluIns, consts: u128) -> Option<(u32, u128, u128, bool)> {
    if matches!(i.unit, Unit::SAdd | Unit::SMul) {
        return Some(scalar(t, i, consts));
    }
    let w = i.word;
    let op = (w & 0xff) as u32;
    let mode = ((w >> 8) & 3) as u32;
    let src1 = ((w >> 10) & 0x1fff) as u32;
    let src2 = ((w >> 23) & 0x1fff) as u32;
    let shrink = ((w >> 36) & 3) as u32;
    let outmod = ((w >> 38) & 3) as u32;
    let mask = ((w >> 40) & 0xff) as u32;
    let bits = mode_bits(mode);
    let n = 128 / bits;
    let int_op = is_int_op(op);
    let a = vector_source(read_reg(t, i.src1_reg, consts), src1, mode, int_op);
    let b = if i.src2_imm {
        let imm = (i.src2_reg << 11 | ((src2 >> 2) & 7) << 8 | ((src2 >> 2) >> 3) & 0xff) as u64;
        let v = if int_op {
            sext(imm, 16) as u64 & mask_of(bits)
        } else {
            match bits {
                16 => imm,
                32 => f16_to_f32(imm as u16).to_bits() as u64,
                64 => (f16_to_f32(imm as u16) as f64).to_bits(),
                _ => imm,
            }
        };
        Operand { lanes: [v; 16] }
    } else {
        vector_source(read_reg(t, i.src2_reg, consts), src2, mode, int_op)
    };
    let mut res = [0u64; 16];
    match op {
        0x3c | 0x3d | 0x3e => {
            let k = if op == 0x3e { 4 } else { 3 };
            let d: f64 = (0..k).map(|c| to_f(a.lanes[c], bits) * to_f(b.lanes[c], bits)).sum();
            res = [from_f(d, bits); 16];
        }
        0x88..=0x8b | 0x90..=0x93 => {
            let cmp = 0x80 + (op & 3);
            let all = op < 0x90;
            let r: Vec<bool> = (0..4).map(|c| lane_op(cmp, a.lanes[c], b.lanes[c], bits) != 0).collect();
            let v = if all { r.iter().all(|&x| x) } else { r.iter().any(|&x| x) };
            res = [if v { mask_of(bits) } else { 0 }; 16];
        }
        0xa8..=0xad | 0xb0..=0xb5 => {
            let base = op & 0xf7;
            let cmp = match base & 7 {
                0 => 0xa0,
                1 => 0xa1,
                2 => 0xa2,
                3 => 0xa3,
                4 => 0xa4,
                _ => 0xa5,
            };
            let all = op < 0xb0;
            let r: Vec<bool> = (0..4).map(|c| lane_op(cmp, a.lanes[c], b.lanes[c], bits) != 0).collect();
            let v = if all { r.iter().all(|&x| x) } else { r.iter().any(|&x| x) };
            res = [if v { mask_of(bits) } else { 0 }; 16];
        }
        0xc0 | 0xc1 | 0xc4 | 0xc5 => {
            let cond = t.r[31];
            for c in 0..n {
                let sel = if op == 0xc0 || op == 0xc4 {
                    lane(cond, bits, c)
                } else if t.r31_half {
                    if lane(cond, 16, 3) != 0 { mask_of(bits) } else { 0 }
                } else if lane(cond, 32, 3) != 0 {
                    mask_of(bits)
                } else {
                    0
                };
                res[c as usize] = (a.lanes[c as usize] & sel) | (b.lanes[c as usize] & !sel & mask_of(bits));
            }
        }
        0x38..=0x3b => {
            // ffma: src1.x * src1.y + src2 (per lane pairs)
            for c in 0..n {
                let x = to_f(a.lanes[0], bits);
                let y = to_f(a.lanes[1], bits);
                res[c as usize] = from_f(x.mul_add(y, to_f(b.lanes[c as usize], bits)), bits);
            }
        }
        _ => {
            let unary = is_lut_unary(op);
            for c in 0..n as usize {
                let (x, y) = if matches!(op, 0x30..=0x33 | 0x7b) { (a.lanes[c], b.lanes[c]) } else { (a.lanes[c], b.lanes[c]) };
                res[c] = lane_op(op, x, if unary { 0 } else { y }, bits);
            }
        }
    }
    // output modifiers and write
    let float_out = !is_int_out(op);
    let mut value = 0u128;
    let mut wmask = 0u128;
    let lane_enabled = |c: u32| -> bool {
        if bits >= 16 { mask & (1 << (c * bits / 16)) != 0 } else { mask & (1 << (c / 2)) != 0 }
    };
    let half_write;
    if shrink == 2 || bits == 8 {
        for c in 0..n {
            if !lane_enabled(c) {
                continue;
            }
            let mut v = res[c as usize];
            if float_out && outmod != 0 {
                v = from_f(float_outmod(to_f(v, bits), outmod), bits);
            }
            set_lane(&mut value, bits, c, v);
            set_lane(&mut wmask, bits, c, mask_of(bits));
        }
        half_write = bits == 16;
    } else {
        let hb = bits / 2;
        for c in 0..n {
            if !lane_enabled(c) {
                continue;
            }
            let v = res[c as usize];
            let h = if float_out {
                let f = float_outmod(to_f(v, bits), outmod);
                from_f(f, hb)
            } else {
                match outmod {
                    0 => {
                        let s = sext(v, bits);
                        let max = (1i64 << (hb - 1)) - 1;
                        (s.clamp(-max - 1, max) as u64) & mask_of(hb)
                    }
                    1 => v.min(mask_of(hb)),
                    3 => v >> hb,
                    _ => v & mask_of(hb),
                }
            };
            let dst = c + if shrink == 1 { n } else { 0 };
            set_lane(&mut value, hb, dst, h);
            set_lane(&mut wmask, hb, dst, mask_of(hb));
        }
        half_write = hb == 16;
    }
    Some((i.out_reg, value, wmask, half_write))
}

/// Scalar ALU (SADD/SMUL): 32-bit lanes, sources by component.
fn scalar(t: &Thread, i: &AluIns, consts: u128) -> (u32, u128, u128, bool) {
    let w = i.word;
    let op = (w & 0xff) as u32;
    let src1 = ((w >> 8) & 0x3f) as u32;
    let src2 = ((w >> 14) & 0x7ff) as u32;
    let outmod = ((w >> 26) & 3) as u32;
    let full = (w >> 28) & 1 != 0;
    let comp = ((w >> 29) & 7) as u32;
    let int_op = is_int_op(op);
    let read = |reg: u32, s: u32| -> u64 {
        let r = read_reg(t, reg, consts);
        let modifier = s & 3;
        let sfull = (s >> 2) & 1 != 0;
        let c = s >> 3;
        let v = if sfull {
            lane(r, 32, c >> 1)
        } else {
            let h = lane(r, 16, c);
            if int_op {
                if modifier == 1 { h } else if modifier == 3 { h << 16 } else { sext(h, 16) as u32 as u64 }
            } else {
                f16_to_f32(h as u16).to_bits() as u64
            }
        };
        if int_op { v } else { float_mod(v, 32, modifier) }
    };
    let a = read(i.src1_reg, src1);
    let b = if i.src2_imm {
        let imm = src2;
        let v = (i.src2_reg << 11) | ((imm & 3) << 9) | ((imm & 4) << 6) | ((imm & 0x38) << 2) | (imm >> 6);
        if int_op { sext(v as u64, 16) as u32 as u64 } else { f16_to_f32(v as u16).to_bits() as u64 }
    } else {
        read(i.src2_reg, src2)
    };
    let r = match op {
        0xc1 | 0xc5 | 0xc0 | 0xc4 => {
            let c = if t.r31_half { lane(t.r[31], 16, 3) != 0 } else { lane(t.r[31], 32, 3) != 0 };
            if c { a } else { b }
        }
        _ => lane_op(op, a, if is_lut_unary(op) { 0 } else { b }, 32),
    };
    let float_out = !is_int_out(op);
    let mut value = 0u128;
    let mut wmask = 0u128;
    if full {
        let v = if float_out && outmod != 0 { from_f(float_outmod(to_f(r, 32), outmod), 32) } else { r };
        set_lane(&mut value, 32, comp >> 1, v);
        set_lane(&mut wmask, 32, comp >> 1, 0xffff_ffff);
    } else {
        let h = if float_out {
            from_f(float_outmod(to_f(r, 32), outmod), 16)
        } else {
            match outmod {
                0 => (sext(r, 32).clamp(-32768, 32767) as u64) & 0xffff,
                1 => r.min(0xffff),
                3 => r >> 16,
                _ => r & 0xffff,
            }
        };
        set_lane(&mut value, 16, comp, h);
        set_lane(&mut wmask, 16, comp, 0xffff);
    }
    (i.out_reg, value, wmask, !full)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn swizzled_sources() {
        let mut r = 0u128;
        for c in 0..4 {
            set_lane(&mut r, 32, c, (c as f32 + 1.0).to_bits() as u64);
        }
        // swizzle wzyx (3,2,1,0), passthrough, no modifier
        let src = (0b00_01_10_11u32 << 5) | (0 << 2);
        let o = vector_source(r, src, 2, false);
        assert_eq!(f32::from_bits(o.lanes[0] as u32), 4.0);
        assert_eq!(f32::from_bits(o.lanes[3] as u32), 1.0);
        // negate
        let o = vector_source(r, (0xe4 << 5) | 2, 2, false);
        assert_eq!(f32::from_bits(o.lanes[1] as u32), -2.0);
        // 16-bit source expanded to 32-bit (fp16 1.0 in lane 0)
        let mut h = 0u128;
        set_lane(&mut h, 16, 0, 0x3c00);
        set_lane(&mut h, 16, 5, 0x4000);
        let o = vector_source(h, (0xe4 << 5) | (4 << 2), 2, false);
        assert_eq!(f32::from_bits(o.lanes[0] as u32), 1.0);
        let o = vector_source(h, (0xe4 << 5) | (5 << 2), 2, false);
        assert_eq!(f32::from_bits(o.lanes[1] as u32), 2.0);
    }

    #[test]
    fn lane_ops() {
        let f = |x: f32| x.to_bits() as u64;
        assert_eq!(lane_op(0x10, f(1.5), f(2.0), 32), f(3.5));
        assert_eq!(lane_op(0x82, f(1.0), f(2.0), 32), 0xffff_ffff);
        assert_eq!(lane_op(0x99, f(-2.7), 0, 32), (-2i64) as u64 & 0xffff_ffff);
        assert_eq!(lane_op(0xb8, 5, 0, 32), f(5.0));
        assert_eq!(lane_op(0x40, 0xffff, 1, 16), 0);
        assert_eq!(lane_op(0x68, 0x8000_0000, 4, 32), 0xf800_0000);
    }
}
