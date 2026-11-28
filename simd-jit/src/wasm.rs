//! The WebAssembly backend: each block becomes a module with 128-bit SIMD, whose function
//! `f(regs, flags, args, inputs, outputs, scratch)` the page compiles and puts in the function
//! table (`sbc_jit_compile`, as for the CPU's blocks).
//!
//! A vector of 16 lanes is four v128 locals; a value the same in every lane (constants,
//! arguments, what is made of them only) is one local used four times. Lane masks are i32
//! locals. Registers, flags, inputs and outputs are read and written in memory. What WebAssembly
//! has no instruction for (`exp2`, `log2`, registered functions, half floats, minNum/maxNum)
//! goes through `helper`, lane by lane, on the scratch words: the same Rust code `eval` runs.
//!
//! When the CPU's JIT discards its code the page frees every slot, ours too: a block's module
//! is then put in again when it next runs (`set_reset_count` says when).
#![allow(unsafe_code)]

use crate::ir::*;
use std::sync::atomic::{AtomicU32, Ordering};

#[link(wasm_import_module = "env")]
extern "C" {
    /// Provided by the page: compile a module, install its export "f" in the function table,
    /// return the table index (or -1).
    fn sbc_jit_compile(ptr: *const u8, len: usize) -> i32;
}

/// How often the page has freed the JIT's table slots (registered by the machine).
static RESETS: std::sync::OnceLock<fn() -> u32> = std::sync::OnceLock::new();

/// Registers the function that says how often the page has freed the JIT's table slots
/// (`arm_a64::wasm_jit_resets`).
pub fn set_reset_count(f: fn() -> u32) {
    let _ = RESETS.set(f);
}

fn resets() -> u32 {
    RESETS.get().map_or(0, |f| f())
}

pub struct Module {
    bytes: Vec<u8>,
    /// the table slot, and `resets()` when it was filled
    slot: AtomicU32,
    epoch: AtomicU32,
}

const NONE: u32 = u32::MAX;

/// The helper's scratch: two vectors in, the result back in the first.
struct Scratch(std::cell::UnsafeCell<[u32; 2 * LANES]>);
// SAFETY: the browser build has one thread
unsafe impl Sync for Scratch {}
static SCRATCH: Scratch = Scratch(std::cell::UnsafeCell::new([0; 2 * LANES]));

impl Module {
    pub fn new(b: &Block) -> Option<Module> {
        let m = Module { bytes: build(b)?, slot: AtomicU32::new(NONE), epoch: AtomicU32::new(0) };
        m.install()?;
        Some(m)
    }

    fn install(&self) -> Option<u32> {
        let epoch = resets();
        // SAFETY: the import reads `len` bytes of the module and writes nothing
        let idx = unsafe { sbc_jit_compile(self.bytes.as_ptr(), self.bytes.len()) };
        self.epoch.store(epoch, Ordering::Relaxed);
        if idx < 0 {
            self.slot.store(NONE, Ordering::Relaxed);
            return None;
        }
        self.slot.store(idx as u32, Ordering::Relaxed);
        Some(idx as u32)
    }

    /// Runs the block: false if its module could not be put in the table (nothing ran).
    pub fn run(&self, regs: *mut u32, flags: *mut u32, args: *const u32, inputs: *const u32, outputs: *mut u32) -> bool {
        let mut slot = self.slot.load(Ordering::Relaxed);
        let epoch = self.epoch.load(Ordering::Relaxed);
        if slot == NONE && epoch == resets() {
            // (putting it back failed: not again before the table starts over)
            return false;
        }
        if slot == NONE || epoch != resets() {
            match self.install() {
                Some(s) => slot = s,
                None => return false,
            }
        }
        type F = extern "C" fn(u32, u32, u32, u32, u32, u32);
        // SAFETY: a wasm function pointer is its table index; the page put the module's
        // function, of type (i32 x 6) -> (), at this one, and nothing has freed it since. It
        // reads and writes the state as `Code::run` checked it is long enough.
        let f = unsafe { std::mem::transmute::<usize, F>(slot as usize) };
        f(regs as u32, flags as u32, args as u32, inputs as u32, outputs as u32, SCRATCH.0.get() as u32);
        true
    }
}

/// `helper_apply` on the scratch's 16 lanes.
extern "C" fn helper(code: u32, scratch: u32) {
    // SAFETY: the code passes the scratch, 32 words
    let s = unsafe { &mut *(scratch as *mut [u32; 2 * LANES]) };
    for i in 0..LANES {
        s[i] = crate::helper_apply(code, s[i], s[LANES + i]);
    }
}

// ---------------------------------------------------------------------------- encoding

fn uleb(out: &mut Vec<u8>, mut v: u64) {
    loop {
        let b = (v & 0x7f) as u8;
        v >>= 7;
        if v == 0 {
            out.push(b);
            break;
        }
        out.push(b | 0x80);
    }
}

fn sleb(out: &mut Vec<u8>, mut v: i64) {
    loop {
        let b = (v & 0x7f) as u8;
        v >>= 7;
        let done = (v == 0 && b & 0x40 == 0) || (v == -1 && b & 0x40 != 0);
        out.push(if done { b } else { b | 0x80 });
        if done {
            break;
        }
    }
}

// parameters
const P_REGS: u32 = 0;
const P_FLAGS: u32 = 1;
const P_ARGS: u32 = 2;
const P_INPUTS: u32 = 3;
const P_OUTPUTS: u32 = 4;
const P_SCRATCH: u32 = 5;
const PARAMS: u32 = 6;
/// i32 locals, then v128 locals.
const I32S: u32 = 64;
const V128S: u32 = 2048;

/// A vector: four v128 locals (the same one four times if uniform).
#[derive(Clone, Copy)]
struct Vec4 {
    l: [u32; 4],
    uniform: bool,
}

#[derive(Clone, Copy)]
enum Val {
    V(Vec4),
    S(u32),
    None,
}

struct W {
    c: Vec<u8>,
    free_v: Vec<u32>,
    free_s: Vec<u32>,
    used_v: u32,
    used_s: u32,
    failed: bool,
}

impl W {
    fn op(&mut self, b: u8) {
        self.c.push(b);
    }
    fn simd(&mut self, op: u32) {
        self.c.push(0xfd);
        uleb(&mut self.c, op as u64);
    }
    fn get(&mut self, l: u32) {
        self.op(0x20);
        uleb(&mut self.c, l as u64);
    }
    fn set(&mut self, l: u32) {
        self.op(0x21);
        uleb(&mut self.c, l as u64);
    }
    fn tee(&mut self, l: u32) {
        self.op(0x22);
        uleb(&mut self.c, l as u64);
    }
    fn i32c(&mut self, v: i32) {
        self.op(0x41);
        sleb(&mut self.c, v as i64);
    }
    fn v128c(&mut self, lanes: [u32; 4]) {
        self.simd(0x0c);
        for x in lanes {
            self.c.extend_from_slice(&x.to_le_bytes());
        }
    }
    fn memarg(&mut self, align: u32, offset: u32) {
        uleb(&mut self.c, align as u64);
        uleb(&mut self.c, offset as u64);
    }
    fn load_v(&mut self, base: u32, offset: u32) {
        self.get(base);
        self.simd(0x00);
        self.memarg(4, offset);
    }
    fn lane_op(&mut self, op: u32, lane: u8) {
        self.simd(op);
        self.op(lane);
    }
    fn new_v(&mut self) -> u32 {
        if let Some(l) = self.free_v.pop() {
            return l;
        }
        if self.used_v == V128S {
            self.failed = true;
            return PARAMS + I32S;
        }
        self.used_v += 1;
        PARAMS + I32S + self.used_v - 1
    }
    fn new_s(&mut self) -> u32 {
        if let Some(l) = self.free_s.pop() {
            return l;
        }
        if self.used_s == I32S {
            self.failed = true;
            return PARAMS;
        }
        self.used_s += 1;
        PARAMS + self.used_s - 1
    }
    fn new_vec(&mut self, uniform: bool) -> Vec4 {
        if uniform {
            let l = self.new_v();
            Vec4 { l: [l; 4], uniform }
        } else {
            Vec4 { l: [0; 4].map(|_| self.new_v()), uniform }
        }
    }
    fn free(&mut self, v: Val) {
        match v {
            Val::V(v) => {
                let n = if v.uniform { 1 } else { 4 };
                self.free_v.extend_from_slice(&v.l[..n]);
            }
            Val::S(l) => self.free_s.push(l),
            Val::None => {}
        }
    }
}

/// The module of a block, or None if it needs more locals than a function may have.
fn build(b: &Block) -> Option<Vec<u8>> {
    let mut w = W { c: Vec::new(), free_v: Vec::new(), free_s: Vec::new(), used_v: 0, used_s: 0, failed: false };
    // the last node that reads each value
    let mut last = vec![0usize; b.ir.len()];
    for (k, n) in b.ir.iter().enumerate() {
        for a in n.args() {
            last[a as usize] = k;
        }
    }
    let mut vals = vec![Val::None; b.ir.len()];
    // the expansion of a lane mask into vector masks, kept while that mask lives
    let mut expanded: Vec<Option<Vec4>> = vec![None; b.ir.len()];
    for (k, n) in b.ir.iter().enumerate() {
        vals[k] = node(&mut w, b, n, &vals, &mut expanded);
        if w.failed {
            return None;
        }
        // values whose last reader this was are free; so are values nobody reads
        for a in n.args().chain(std::iter::once(k as Id)) {
            let a = a as usize;
            if last[a] <= k && !matches!(vals[a], Val::None) {
                w.free(vals[a]);
                vals[a] = Val::None;
                if let Some(e) = expanded[a].take() {
                    w.free(Val::V(e));
                }
            }
        }
    }
    w.op(0x0b); // end of the function
    Some(assemble(&w.c, w.used_v))
}

fn vec(v: Val) -> Vec4 {
    match v {
        Val::V(v) => v,
        _ => unreachable!("not a vector"),
    }
}

fn scal(v: Val) -> u32 {
    match v {
        Val::S(l) => l,
        _ => unreachable!("not a lane mask"),
    }
}

/// The vector masks of the lanes of a lane mask (4 lanes in each of 4 locals).
fn expand(w: &mut W, s: u32) -> Vec4 {
    let r = w.new_vec(false);
    for c in 0..4 {
        w.get(s);
        w.simd(0x11); // i32x4.splat
        w.v128c([1, 2, 4, 8].map(|b| b << (4 * c)));
        w.simd(0x4e); // and
        w.v128c([0; 4]);
        w.simd(0x38); // i32x4.ne
        w.set(r.l[c]);
    }
    r
}

/// One SIMD operation on each of the four parts (once if uniform).
fn each(w: &mut W, uniform: bool, mut f: impl FnMut(&mut W, usize)) -> Vec4 {
    let d = w.new_vec(uniform);
    for c in 0..if uniform { 1 } else { 4 } {
        f(w, c);
        w.set(d.l[c]);
    }
    d
}

fn node(w: &mut W, b: &Block, n: &Node, vals: &[Val], expanded: &mut [Option<Vec4>]) -> Val {
    let v = |i: Id| vec(vals[i as usize]);
    let s = |i: Id| scal(vals[i as usize]);
    if let Some(code) = crate::helper_code(n) {
        let xs: Vec<Vec4> = n.args().map(v).collect();
        return Val::V(call(w, code, &xs));
    }
    match *n {
        Node::Reg(r) => Val::V(each(w, false, |w, c| w.load_v(P_REGS, r as u32 * 64 + c as u32 * 16))),
        Node::In(i) => Val::V(each(w, false, |w, c| w.load_v(P_INPUTS, i as u32 * 64 + c as u32 * 16))),
        Node::Imm(x) => Val::V(each(w, true, |w, _| w.v128c([x; 4]))),
        Node::Const(k) => {
            let cv = b.consts[k as usize];
            Val::V(each(w, false, |w, c| w.v128c(std::array::from_fn(|i| cv[4 * c + i]))))
        }
        Node::Lane => Val::V(each(w, false, |w, c| w.v128c(std::array::from_fn(|i| (4 * c + i) as u32)))),
        Node::Arg(i) => Val::V(each(w, true, |w, _| {
            w.get(P_ARGS);
            w.op(0x28); // i32.load
            w.memarg(2, i as u32 * 4);
            w.simd(0x11); // i32x4.splat
        })),
        Node::Un(op, a) => Val::V(emit_un(w, op, v(a))),
        Node::Bin(op, a, c) => Val::V(emit_bin(w, op, v(a), v(c))),
        Node::Sel(m, a, c) => {
            let (m, a, c) = (v(m), v(a), v(c));
            Val::V(each(w, m.uniform && a.uniform && c.uniform, |w, k| {
                w.get(a.l[k]);
                w.get(c.l[k]);
                w.get(m.l[k]);
                w.simd(0x52); // bitselect
            }))
        }
        Node::Shuffle(a, p) => Val::V(shuffle(w, v(a), &b.perms[p as usize])),
        Node::Expand(a) => Val::V(expand(w, s(a))),
        Node::Flag(f) => {
            let d = w.new_s();
            w.get(P_FLAGS);
            w.op(0x28);
            w.memarg(2, f as u32 * 4);
            w.set(d);
            Val::S(d)
        }
        Node::SImm(x) => {
            let d = w.new_s();
            w.i32c(x as i32);
            w.set(d);
            Val::S(d)
        }
        Node::S(op, a, c) => {
            let d = w.new_s();
            w.get(s(a));
            w.get(s(c));
            w.op(match op {
                SOp::And => 0x71,
                SOp::Or => 0x72,
                SOp::Xor => 0x73,
            });
            w.set(d);
            Val::S(d)
        }
        Node::Bits(a) => {
            let a = v(a);
            let d = w.new_s();
            for c in 0..4 {
                w.get(a.l[c]);
                w.v128c([0; 4]);
                w.simd(0x38); // i32x4.ne
                w.simd(0xa4); // i32x4.bitmask
                if c > 0 {
                    w.i32c(4 * c as i32);
                    w.op(0x74); // i32.shl
                    w.op(0x72); // i32.or
                }
            }
            w.set(d);
            Val::S(d)
        }
        Node::Store(r, a, m) => {
            let a = v(a);
            let e = match expanded[m as usize] {
                Some(e) => e,
                None => {
                    let e = expand(w, s(m));
                    expanded[m as usize] = Some(e);
                    e
                }
            };
            for c in 0..4 {
                let at = r as u32 * 64 + c as u32 * 16;
                w.get(P_REGS);
                w.get(a.l[c]);
                w.load_v(P_REGS, at);
                w.get(e.l[c]);
                w.simd(0x52); // bitselect: the new value in the mask's lanes
                w.simd(0x0b); // v128.store
                w.memarg(4, at);
            }
            Val::None
        }
        Node::StoreAll(r, a) | Node::Out(r, a) => {
            let base = if matches!(n, Node::Out(..)) { P_OUTPUTS } else { P_REGS };
            let a = v(a);
            for c in 0..4 {
                w.get(base);
                w.get(a.l[c]);
                w.simd(0x0b);
                w.memarg(4, r as u32 * 64 + c as u32 * 16);
            }
            Val::None
        }
        Node::StoreFlag(f, a, m) => {
            // flags[f] = flags[f] & !m | a & m
            w.get(P_FLAGS);
            w.get(P_FLAGS);
            w.op(0x28);
            w.memarg(2, f as u32 * 4);
            w.get(s(m));
            w.i32c(-1);
            w.op(0x73);
            w.op(0x71);
            w.get(s(a));
            w.get(s(m));
            w.op(0x71);
            w.op(0x72);
            w.op(0x36); // i32.store
            w.memarg(2, f as u32 * 4);
            Val::None
        }
    }
}

/// Lane i of the result is lane p[i] of `a`.
fn shuffle(w: &mut W, a: Vec4, p: &[u8; LANES]) -> Vec4 {
    if a.uniform {
        // (the same in every lane: moving lanes keeps it)
        return each(w, true, |w, _| w.get(a.l[0]));
    }
    let d = w.new_vec(false);
    for c in 0..4 {
        let src: Vec<usize> = (0..4).map(|i| p[4 * c + i] as usize).collect();
        let mut chunks: Vec<usize> = src.iter().map(|s| s / 4).collect();
        chunks.sort_unstable();
        chunks.dedup();
        if chunks.len() <= 2 {
            let (x, y) = (chunks[0], *chunks.last().unwrap());
            w.get(a.l[x]);
            w.get(a.l[y]);
            w.simd(0x0d); // i8x16.shuffle
            for &s in &src {
                let base = if s / 4 == x { 0 } else { 16 };
                for byte in 0..4 {
                    w.op((base + 4 * (s % 4) + byte) as u8);
                }
            }
        } else {
            w.get(a.l[0]);
            for (i, &s) in src.iter().enumerate() {
                w.get(a.l[s / 4]);
                w.lane_op(0x1b, (s % 4) as u8); // i32x4.extract_lane
                w.lane_op(0x1c, i as u8); // i32x4.replace_lane
            }
        }
        w.set(d.l[c]);
    }
    d
}

fn emit_un(w: &mut W, op: Un, a: Vec4) -> Vec4 {
    let simple = match op {
        Un::Not => Some(0x4d),
        Un::Neg => Some(0xa1),
        Un::IToF => Some(0xfa),
        Un::UToF => Some(0xfb),
        Un::FToI => Some(0xf8),
        Un::FToU => Some(0xf9),
        Un::FSqrt => Some(0xe3),
        Un::FRound => Some(0x6a),
        Un::FTrunc => Some(0x69),
        Un::FFloor => Some(0x68),
        Un::FCeil => Some(0x67),
        _ => None,
    };
    if let Some(code) = simple {
        return each(w, a.uniform, |w, c| {
            w.get(a.l[c]);
            w.simd(code);
        });
    }
    match op {
        Un::Shl(n) | Un::ShrU(n) | Un::ShrS(n) => each(w, a.uniform, |w, c| {
            w.get(a.l[c]);
            w.i32c(n as i32);
            w.simd(match op {
                Un::Shl(_) => 0xab,
                Un::ShrS(_) => 0xac,
                _ => 0xad,
            });
        }),
        Un::Clz => each(w, a.uniform, |w, c| {
            w.get(a.l[c]);
            for lane in 0..4u8 {
                w.get(a.l[c]);
                w.lane_op(0x1b, lane);
                w.op(0x67); // i32.clz
                w.lane_op(0x1c, lane);
            }
        }),
        Un::Ftz => each(w, a.uniform, |w, c| {
            // the sign alone where the exponent is zero
            w.get(a.l[c]);
            w.v128c([0x8000_0000; 4]);
            w.simd(0x4e);
            w.get(a.l[c]);
            w.get(a.l[c]);
            w.v128c([0x7f80_0000; 4]);
            w.simd(0x4e);
            w.v128c([0; 4]);
            w.simd(0x37); // i32x4.eq
            w.simd(0x52);
        }),
        _ => unreachable!("{op:?}"),
    }
}

/// Calls `helper(code)` on vectors put in the scratch words; the result comes back from there.
fn call(w: &mut W, code: u32, args: &[Vec4]) -> Vec4 {
    for (k, a) in args.iter().enumerate() {
        for c in 0..4 {
            w.get(P_SCRATCH);
            w.get(a.l[c]);
            w.simd(0x0b);
            w.memarg(4, (k * 64 + c * 16) as u32);
        }
    }
    w.i32c(code as i32);
    w.get(P_SCRATCH);
    w.i32c(helper as extern "C" fn(u32, u32) as usize as i32);
    w.op(0x11); // call_indirect
    uleb(&mut w.c, 1); // type 1: (i32, i32) -> ()
    w.op(0x00); // table 0
    each(w, false, |w, c| w.load_v(P_SCRATCH, c as u32 * 16))
}

fn emit_bin(w: &mut W, op: Bin, a: Vec4, b: Vec4) -> Vec4 {
    let uniform = a.uniform && b.uniform;
    let simple = match op {
        Bin::And => Some(0x4e),
        Bin::Or => Some(0x50),
        Bin::Xor => Some(0x51),
        Bin::Add => Some(0xae),
        Bin::Sub => Some(0xb1),
        Bin::Mul => Some(0xb5),
        Bin::IMin => Some(0xb6),
        Bin::UMin => Some(0xb7),
        Bin::IMax => Some(0xb8),
        Bin::UMax => Some(0xb9),
        Bin::IEq => Some(0x37),
        Bin::ILt => Some(0x39),
        Bin::ULt => Some(0x3a),
        Bin::FAdd => Some(0xe4),
        Bin::FSub => Some(0xe5),
        Bin::FMul => Some(0xe6),
        Bin::FDiv => Some(0xe7),
        Bin::FEq => Some(0x41),
        Bin::FLt => Some(0x43),
        Bin::FLe => Some(0x45),
        Bin::V8AddSat => Some(0x70),
        Bin::V8SubSat => Some(0x73),
        Bin::V8Min => Some(0x77),
        Bin::V8Max => Some(0x79),
        _ => None,
    };
    if let Some(code) = simple {
        return each(w, uniform, |w, c| {
            w.get(a.l[c]);
            w.get(b.l[c]);
            w.simd(code);
        });
    }
    match op {
        Bin::FUnord => each(w, uniform, |w, c| {
            for x in [a, b] {
                w.get(x.l[c]);
                w.get(x.l[c]);
                w.simd(0x42); // f32x4.ne
            }
            w.simd(0x50);
        }),
        Bin::FMin | Bin::FMax => each(w, uniform, |w, c| {
            // x where x is a number and y is not, x < y (x > y), or they are equal and x (y)
            // is negative; else y
            let (x, y) = (a.l[c], b.l[c]);
            w.get(x);
            w.get(y);
            w.get(x);
            w.get(x);
            w.simd(0x41); // x == x
            w.get(y);
            w.get(y);
            w.simd(0x42); // y != y
            w.simd(0x4e);
            w.get(x);
            w.get(y);
            w.simd(if op == Bin::FMin { 0x43 } else { 0x44 });
            w.simd(0x50);
            w.get(x);
            w.get(y);
            w.simd(0x41);
            w.get(if op == Bin::FMin { x } else { y });
            w.v128c([0; 4]);
            w.simd(0x39); // i32x4.lt_s: the sign bit
            w.simd(0x4e);
            w.simd(0x50);
            w.simd(0x52);
        }),
        Bin::V8MulD => {
            // per byte, in 16 bits: t = a * b + 128; (t + (t >> 8)) >> 8
            let t = w.new_v();
            let d = each(w, uniform, |w, c| {
                for ext in [0x9e, 0x9f] {
                    w.get(a.l[c]);
                    w.get(b.l[c]);
                    w.simd(ext); // i16x8.extmul_{low,high}_i8x16_u
                    w.v128c([0x0080_0080; 4]);
                    w.simd(0x8e); // i16x8.add
                    w.tee(t);
                    w.get(t);
                    w.i32c(8);
                    w.simd(0x8d); // i16x8.shr_u
                    w.simd(0x8e);
                    w.i32c(8);
                    w.simd(0x8d);
                }
                w.simd(0x66); // i8x16.narrow_i16x8_u
            });
            w.free(Val::V(Vec4 { l: [t; 4], uniform: true }));
            d
        }
        Bin::Shl | Bin::ShrU | Bin::ShrS | Bin::Ror => {
            if b.uniform {
                // by the same amount in every lane: the vector shifts (modulo 32, as wanted)
                let n = w.new_s();
                w.get(b.l[0]);
                w.lane_op(0x1b, 0);
                w.set(n);
                let d = each(w, a.uniform, |w, c| {
                    let sh = |w: &mut W, op: u32, left: bool| {
                        w.get(a.l[c]);
                        if left {
                            w.i32c(32);
                            w.get(n);
                            w.op(0x6b); // i32.sub
                        } else {
                            w.get(n);
                        }
                        w.simd(op);
                    };
                    match op {
                        Bin::Shl => sh(w, 0xab, false),
                        Bin::ShrS => sh(w, 0xac, false),
                        Bin::ShrU => sh(w, 0xad, false),
                        _ => {
                            sh(w, 0xad, false);
                            sh(w, 0xab, true);
                            w.simd(0x50);
                        }
                    }
                });
                w.free(Val::S(n));
                d
            } else {
                let sop = match op {
                    Bin::Shl => 0x74,
                    Bin::ShrS => 0x75,
                    Bin::ShrU => 0x76,
                    _ => 0x78, // i32.rotr
                };
                each(w, uniform, |w, c| {
                    w.get(a.l[c]);
                    for lane in 0..4u8 {
                        w.get(a.l[c]);
                        w.lane_op(0x1b, lane);
                        w.get(b.l[c]);
                        w.lane_op(0x1b, lane);
                        w.op(sop);
                        w.lane_op(0x1c, lane);
                    }
                })
            }
        }
        _ => unreachable!("{op:?}"),
    }
}

/// The module around the function's body.
fn assemble(body: &[u8], v128s: u32) -> Vec<u8> {
    let mut m = b"\0asm\x01\0\0\0".to_vec();
    let section = |m: &mut Vec<u8>, id: u8, payload: &[u8]| {
        m.push(id);
        uleb(m, payload.len() as u64);
        m.extend_from_slice(payload);
    };
    // types: 0 = (i32 x 6) -> (), the function; 1 = (i32, i32) -> (), the helper
    section(&mut m, 1, &[2, 0x60, 6, 0x7f, 0x7f, 0x7f, 0x7f, 0x7f, 0x7f, 0, 0x60, 2, 0x7f, 0x7f, 0]);
    // imports: env.memory, env.table
    let mut imp = vec![2];
    for (name, desc) in [("memory", &[0x02u8, 0x00, 0x00][..]), ("table", &[0x01u8, 0x70, 0x00, 0x00][..])] {
        imp.push(3);
        imp.extend_from_slice(b"env");
        imp.push(name.len() as u8);
        imp.extend_from_slice(name.as_bytes());
        imp.extend_from_slice(desc);
    }
    section(&mut m, 2, &imp);
    section(&mut m, 3, &[1, 0]);
    section(&mut m, 7, &[1, 1, b'f', 0x00, 0]);
    // locals: I32S i32s (their numbers are fixed), then the v128s used
    let mut f = vec![2];
    uleb(&mut f, I32S as u64);
    f.push(0x7f);
    uleb(&mut f, v128s.max(1) as u64);
    f.push(0x7b);
    f.extend_from_slice(body);
    let mut code = vec![1];
    uleb(&mut code, f.len() as u64);
    code.extend_from_slice(&f);
    section(&mut m, 10, &code);
    // a name for profiles: function 0 is "qpu_block"
    let mut names = vec![4];
    names.extend_from_slice(b"name");
    let mut sub = vec![1, 0, 9];
    sub.extend_from_slice(b"qpu_block");
    names.push(1);
    uleb(&mut names, sub.len() as u64);
    names.extend_from_slice(&sub);
    section(&mut m, 0, &names);
    m
}
