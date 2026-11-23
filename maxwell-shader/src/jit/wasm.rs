//! The JIT's WebAssembly backend: a program's blocks become one module with 128-bit SIMD.
//!
//! The module's function `f(gpr, pred, scratch, cb, active, block)` jumps to the block's code
//! (`br_table`) and returns 1 (0 for a block that needed more locals than a function has: the
//! interpreter runs that one). A vector of 16 lanes is four v128 locals; a value that is the same in every
//! lane (constants, constant buffer words, what is made of them) is one local used four times.
//! Registers and predicates are read from and written to the `Lanes` in memory; a write keeps
//! the lanes outside its mask (`v128.bitselect`). What wasm has no instruction for (sine,
//! cosine, powers, logarithms; a fused multiply-add whose double-precision sum might round
//! twice) goes through `helper`, on the frame's scratch words, exactly as the interpreter
//! computes it.
//!
//! The page compiles the module (`sbc_jit_compile`, as for the CPU's blocks) and puts its
//! function in the table. When the CPU's JIT discards its code the page frees every slot,
//! ours too: the module is then put in again (`set_reset_count` says when).
#![allow(unsafe_code)]

use super::*;
use std::sync::atomic::{AtomicU32, Ordering};

#[link(wasm_import_module = "env")]
extern "C" {
    /// Provided by the page: compile a module, install its export "f" in the function table,
    /// return the table index (or -1).
    fn sbc_jit_compile(ptr: *const u8, len: usize) -> i32;
}

/// How often the page has freed the JIT's table slots (registered by the machine).
static RESETS: std::sync::OnceLock<fn() -> u32> = std::sync::OnceLock::new();

pub fn set_reset_count(f: fn() -> u32) {
    let _ = RESETS.set(f);
}

fn resets() -> u32 {
    RESETS.get().map_or(0, |f| f())
}

pub struct Module {
    bytes: Vec<u8>,
    /// The table slot, and `resets()` when it was filled.
    slot: AtomicU32,
    epoch: AtomicU32,
}

const NONE: u32 = u32::MAX;

impl Module {
    pub fn new(c: &Code) -> Option<Module> {
        let m = Module { bytes: build(c)?, slot: AtomicU32::new(NONE), epoch: AtomicU32::new(0) };
        m.install()?;
        Some(m)
    }

    fn install(&self) -> Option<u32> {
        let epoch = resets();
        // SAFETY: the import reads `len` bytes of the module and writes nothing
        let idx = unsafe { sbc_jit_compile(self.bytes.as_ptr(), self.bytes.len()) };
        if idx < 0 {
            self.slot.store(NONE, Ordering::Relaxed);
            return None;
        }
        self.slot.store(idx as u32, Ordering::Relaxed);
        self.epoch.store(epoch, Ordering::Relaxed);
        Some(idx as u32)
    }

    pub fn run(&self, b: u16, gpr: &mut [[u32; LANES]; 256], pred: &mut [u32; 8], frame: &mut Frame, active: u32) -> bool {
        let mut slot = self.slot.load(Ordering::Relaxed);
        if slot == NONE || self.epoch.load(Ordering::Relaxed) != resets() {
            match self.install() {
                Some(s) => slot = s,
                None => return false,
            }
        }
        type F = extern "C" fn(u32, u32, u32, u32, u32, u32) -> u32;
        // SAFETY: a wasm function pointer is its table index; the page put the module's
        // function, of type (i32 x 6) -> i32, at this one, and nothing has freed it since
        let f = unsafe { std::mem::transmute::<usize, F>(slot as usize) };
        // (0: that block has no code, too big for the locals)
        f(gpr.as_mut_ptr() as u32, pred.as_mut_ptr() as u32, frame.scratch.as_mut_ptr() as u32, frame.cb.as_ptr() as u32, active, b as u32) != 0
    }
}


const H_SIN: i32 = 0;
const H_COS: i32 = 1;
const H_EX2: i32 = 2;
const H_LG2: i32 = 3;
const H_RCP64H: i32 = 4;
const H_RSQ64H: i32 = 5;
const H_FMA: i32 = 6;

/// What the code cannot do itself, lane by lane on the scratch words: the result goes to the
/// first 16.
extern "C" fn helper(op: u32, scratch: u32) {
    // SAFETY: the code passes the frame's scratch words, which are SCRATCH long
    let s = unsafe { &mut *(scratch as *mut [u32; SCRATCH]) };
    let un = |op| match op {
        H_SIN => Un::Sin,
        H_COS => Un::Cos,
        H_EX2 => Un::Ex2,
        H_LG2 => Un::Lg2,
        H_RCP64H => Un::Rcp64h,
        _ => Un::Rsq64h,
    };
    for i in 0..LANES {
        s[i] = if op as i32 == H_FMA {
            crate::fp::fma(f32::from_bits(s[i]), f32::from_bits(s[LANES + i]), f32::from_bits(s[2 * LANES + i])).to_bits()
        } else {
            unary(un(op as i32), s[i])
        };
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
const P_GPR: u32 = 0;
const P_PRED: u32 = 1;
const P_SCRATCH: u32 = 2;
const P_CB: u32 = 3;
const P_ACTIVE: u32 = 4;
const P_BLOCK: u32 = 5;
const PARAMS: u32 = 6;
/// i32 locals, then v128 locals.
const I32S: u32 = 64;
const V128S: u32 = 1024;

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
    fn v128c64(&mut self, lanes: [u64; 2]) {
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
    fn shuffle(&mut self, lanes: [u8; 16]) {
        self.simd(0x0d);
        self.c.extend_from_slice(&lanes);
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

/// The module of a program's blocks.
fn build(code: &Code) -> Option<Vec<u8>> {
    let mut w = W { c: Vec::new(), free_v: Vec::new(), free_s: Vec::new(), used_v: 0, used_s: 0, failed: false };
    let n = code.blocks.len() as u32;
    // block $exit, then one block per compiled block, innermost first
    for _ in 0..=n {
        w.op(0x02);
        w.op(0x40);
    }
    w.get(P_BLOCK);
    w.op(0x0e);
    uleb(&mut w.c, n as u64);
    for k in 0..=n {
        uleb(&mut w.c, k as u64);
    }
    for b in &code.blocks {
        w.op(0x0b);
        let (at, used_v, used_s) = (w.c.len(), w.used_v, w.used_s);
        block(&mut w, b);
        if w.failed {
            w.c.truncate(at);
            (w.used_v, w.used_s, w.failed) = (used_v, used_s, false);
            w.i32c(0);
        } else {
            w.i32c(1);
        }
        w.op(0x0f);
    }
    w.op(0x0b); // end of $exit
    w.i32c(0);
    w.op(0x0b); // end of the function
    Some(assemble(&w.c, w.used_s, w.used_v))
}

/// The code of one block.
fn block(w: &mut W, b: &Block) {
    w.free_v.clear();
    w.free_s.clear();
    let (used_v, used_s) = (w.used_v, w.used_s);
    // every local is free again: start from the bottom
    w.free_v.extend((0..used_v).rev().map(|k| PARAMS + I32S + k));
    w.free_s.extend((0..used_s).rev().map(|k| PARAMS + k));
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
        vals[k] = node(w, n, &vals, &mut expanded);
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
        _ => unreachable!("not a scalar"),
    }
}

/// The vector masks of the lanes of a scalar mask (4 lanes in each of 4 locals).
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

fn node(w: &mut W, n: &Node, vals: &[Val], expanded: &mut [Option<Vec4>]) -> Val {
    let v = |i: Id| vec(vals[i as usize]);
    let s = |i: Id| scal(vals[i as usize]);
    match *n {
        Node::Reg(r) => {
            let d = w.new_vec(false);
            for c in 0..4 {
                w.load_v(P_GPR, r as u32 * 64 + c as u32 * 16);
                w.set(d.l[c]);
            }
            Val::V(d)
        }
        Node::Imm(x) => {
            let d = w.new_vec(true);
            w.v128c([x; 4]);
            w.set(d.l[0]);
            Val::V(d)
        }
        Node::Cb(i) => {
            let d = w.new_vec(true);
            w.get(P_CB);
            w.op(0x28); // i32.load
            w.memarg(2, i as u32 * 4);
            w.simd(0x11); // i32x4.splat
            w.set(d.l[0]);
            Val::V(d)
        }
        Node::Un(op, a) => Val::V(emit_un(w, op, v(a))),
        Node::Bin(op, a, b) => Val::V(emit_bin(w, op, v(a), v(b))),
        Node::Sel(m, a, b) => {
            let (m, a, b) = (v(m), v(a), v(b));
            let d = w.new_vec(m.uniform && a.uniform && b.uniform);
            for c in 0..if d.uniform { 1 } else { 4 } {
                w.get(a.l[c]);
                w.get(b.l[c]);
                w.get(m.l[c]);
                w.simd(0x52); // bitselect
                w.set(d.l[c]);
            }
            Val::V(d)
        }
        Node::Fma(a, b, c) => Val::V(fma(w, v(a), v(b), v(c))),
        Node::Expand(a) => {
            let e = expand(w, s(a));
            Val::V(e)
        }
        Node::Active => {
            let d = w.new_s();
            w.get(P_ACTIVE);
            w.set(d);
            Val::S(d)
        }
        Node::Pred(p) => {
            let d = w.new_s();
            w.get(P_PRED);
            w.op(0x28);
            w.memarg(2, p as u32 * 4);
            w.set(d);
            Val::S(d)
        }
        Node::SImm(x) => {
            let d = w.new_s();
            w.i32c(x as i32);
            w.set(d);
            Val::S(d)
        }
        Node::S(op, a, b) => {
            let d = w.new_s();
            w.get(s(a));
            w.get(s(b));
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
                w.get(P_GPR);
                w.get(a.l[c]);
                w.load_v(P_GPR, at);
                w.get(e.l[c]);
                w.simd(0x52); // bitselect: the new value in the mask's lanes
                w.simd(0x0b); // v128.store
                w.memarg(4, at);
            }
            Val::None
        }
        Node::StoreAll(r, a) => {
            let a = v(a);
            for c in 0..4 {
                w.get(P_GPR);
                w.get(a.l[c]);
                w.simd(0x0b);
                w.memarg(4, r as u32 * 64 + c as u32 * 16);
            }
            Val::None
        }
        Node::StoreP(p, a, m) => {
            // pred[p] = pred[p] & !m | a & m
            w.get(P_PRED);
            w.get(P_PRED);
            w.op(0x28);
            w.memarg(2, p as u32 * 4);
            w.get(s(m));
            w.i32c(-1);
            w.op(0x73);
            w.op(0x71);
            w.get(s(a));
            w.get(s(m));
            w.op(0x71);
            w.op(0x72);
            w.op(0x36); // i32.store
            w.memarg(2, p as u32 * 4);
            Val::None
        }
    }
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

fn emit_un(w: &mut W, op: Un, a: Vec4) -> Vec4 {
    let simple = |op: Un| -> Option<u32> {
        Some(match op {
            Un::Sqrt => 0xe3,
            Un::Nearest => 0x6a,
            Un::Floor => 0x68,
            Un::Ceil => 0x67,
            Un::Trunc => 0x69,
            Un::SToF => 0xfa,
            Un::UToF => 0xfb,
            Un::FToS => 0xf8,
            Un::FToU => 0xf9,
            _ => return None,
        })
    };
    if let Some(code) = simple(op) {
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
        Un::Sat => each(w, a.uniform, |w, c| {
            // x > 0 ? (x < 1 ? x : 1) : 0, NaN to 0
            w.get(a.l[c]);
            w.v128c([1f32.to_bits(); 4]);
            w.get(a.l[c]);
            w.v128c([1f32.to_bits(); 4]);
            w.simd(0x43); // f32x4.lt
            w.simd(0x52);
            w.v128c([0; 4]);
            w.get(a.l[c]);
            w.v128c([0; 4]);
            w.simd(0x44); // f32x4.gt
            w.simd(0x52);
        }),
        Un::Rsq => each(w, a.uniform, |w, c| {
            // in double: the low two lanes, then the high two
            for half in 0..2 {
                w.v128c64([1f64.to_bits(); 2]);
                w.get(a.l[c]);
                if half == 1 {
                    w.get(a.l[c]);
                    w.shuffle([8, 9, 10, 11, 12, 13, 14, 15, 8, 9, 10, 11, 12, 13, 14, 15]);
                }
                w.simd(0x5f); // f64x2.promote_low_f32x4
                w.simd(0xef); // f64x2.sqrt
                w.simd(0xf3); // f64x2.div
                w.simd(0x5e); // f32x4.demote_f64x2_zero
            }
            w.shuffle([0, 1, 2, 3, 4, 5, 6, 7, 16, 17, 18, 19, 20, 21, 22, 23]);
        }),
        Un::Sin | Un::Cos | Un::Ex2 | Un::Lg2 | Un::Rcp64h | Un::Rsq64h => {
            let h = match op {
                Un::Sin => H_SIN,
                Un::Cos => H_COS,
                Un::Ex2 => H_EX2,
                Un::Lg2 => H_LG2,
                Un::Rcp64h => H_RCP64H,
                _ => H_RSQ64H,
            };
            call(w, h, &[a])
        }
        _ => unreachable!(),
    }
}

/// Calls `helper(h)` on vectors put in the scratch words; the result comes back from there.
fn call(w: &mut W, h: i32, args: &[Vec4]) -> Vec4 {
    for (k, a) in args.iter().enumerate() {
        for c in 0..4 {
            w.get(P_SCRATCH);
            w.get(a.l[c]);
            w.simd(0x0b);
            w.memarg(4, (k * 64 + c * 16) as u32);
        }
    }
    w.i32c(h);
    w.get(P_SCRATCH);
    w.i32c(helper as extern "C" fn(u32, u32) as usize as i32);
    w.op(0x11); // call_indirect
    uleb(&mut w.c, 1); // type 1: (i32, i32) -> ()
    w.op(0x00); // table 0
    let d = w.new_vec(false);
    for c in 0..4 {
        w.load_v(P_SCRATCH, c as u32 * 16);
        w.set(d.l[c]);
    }
    d
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
        Bin::FAdd => Some(0xe4),
        Bin::FMul => Some(0xe6),
        Bin::FDiv => Some(0xe7),
        Bin::FLt => Some(0x43),
        Bin::FEq => Some(0x41),
        Bin::FGt => Some(0x44),
        Bin::ILt => Some(0x39),
        Bin::IEq => Some(0x37),
        Bin::IGt => Some(0x3b),
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
            // a where a < b (a > b), b is NaN, or they are equal and a (b) is negative
            let (x, y) = (a.l[c], b.l[c]);
            w.get(x);
            w.get(y);
            w.get(x);
            w.get(y);
            w.simd(if op == Bin::FMin { 0x43 } else { 0x44 });
            w.get(y);
            w.get(y);
            w.simd(0x42);
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
        Bin::Shl | Bin::ShrU | Bin::ShrS => {
            // each lane by its own amount (i32 shifts take it modulo 32)
            let sop = match op {
                Bin::Shl => 0x74,
                Bin::ShrS => 0x75,
                _ => 0x76,
            };
            each(w, uniform, |w, c| {
                w.get(a.l[c]);
                for lane in 0..4u8 {
                    w.get(a.l[c]);
                    w.simd(0x1b);
                    w.op(lane);
                    w.get(b.l[c]);
                    w.simd(0x1b);
                    w.op(lane);
                    w.op(sop);
                    w.simd(0x1c);
                    w.op(lane);
                }
            })
        }
        _ => unreachable!(),
    }
}

/// a * b + c rounded once, as `fp::fma` (which is `fma_double` here): the exact product plus
/// c in double, then to single; where that might round twice (the double is halfway between
/// two singles, or tiny), every lane goes through the helper.
fn fma(w: &mut W, a: Vec4, b: Vec4, c: Vec4) -> Vec4 {
    let uniform = a.uniform && b.uniform && c.uniform;
    let d = w.new_vec(uniform);
    let slow = w.new_s();
    w.i32c(0);
    w.set(slow);
    let t = w.new_v();
    for part in 0..if uniform { 1 } else { 4 } {
        for half in 0..2 {
            // s = a * b + c in double for two lanes
            for x in [a, b] {
                w.get(x.l[part]);
                if half == 1 {
                    w.get(x.l[part]);
                    w.shuffle([8, 9, 10, 11, 12, 13, 14, 15, 8, 9, 10, 11, 12, 13, 14, 15]);
                }
                w.simd(0x5f);
            }
            w.simd(0xf2); // f64x2.mul
            w.get(c.l[part]);
            if half == 1 {
                w.get(c.l[part]);
                w.shuffle([8, 9, 10, 11, 12, 13, 14, 15, 8, 9, 10, 11, 12, 13, 14, 15]);
            }
            w.simd(0x5f);
            w.simd(0xf0); // f64x2.add
            w.op(0x22); // local.tee
            uleb(&mut w.c, t as u64);
            // halfway: the bits below a single's are exactly 1000...
            w.v128c64([0x1fff_ffff; 2]);
            w.simd(0x4e);
            w.v128c64([0x1000_0000; 2]);
            w.simd(0xd6); // i64x2.eq
            w.get(t);
            w.simd(0xec); // f64x2.abs
            w.v128c64([2e-38f64.to_bits(); 2]);
            w.simd(0x49); // f64x2.lt
            w.simd(0x50);
            w.simd(0x53); // any_true
            w.get(slow);
            w.op(0x72);
            w.set(slow);
            w.get(t);
            w.simd(0x5e); // f32x4.demote_f64x2_zero
        }
        w.shuffle([0, 1, 2, 3, 4, 5, 6, 7, 16, 17, 18, 19, 20, 21, 22, 23]);
        w.set(d.l[part]);
    }
    w.free(Val::V(Vec4 { l: [t; 4], uniform: true }));
    w.get(slow);
    w.op(0x04); // if
    w.op(0x40);
    let r = call(w, H_FMA, &[a, b, c]);
    for part in 0..if uniform { 1 } else { 4 } {
        w.get(r.l[part]);
        w.set(d.l[part]);
    }
    w.free(Val::V(r));
    w.op(0x0b);
    w.free(Val::S(slow));
    d
}

/// The module around the function's body.
fn assemble(body: &[u8], i32s: u32, v128s: u32) -> Vec<u8> {
    let mut m = b"\0asm\x01\0\0\0".to_vec();
    let section = |m: &mut Vec<u8>, id: u8, payload: &[u8]| {
        m.push(id);
        uleb(m, payload.len() as u64);
        m.extend_from_slice(payload);
    };
    // types: 0 = (i32 x 6) -> i32, the function; 1 = (i32, i32) -> (), the helper
    section(&mut m, 1, &[2, 0x60, 6, 0x7f, 0x7f, 0x7f, 0x7f, 0x7f, 0x7f, 1, 0x7f, 0x60, 2, 0x7f, 0x7f, 0]);
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
    // locals: I32S i32s (only those used are needed, but their numbers are fixed), v128s
    let _ = i32s;
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
    // a name for profiles: function 0 is "gpu_shader"
    let mut names = vec![4];
    names.extend_from_slice(b"name");
    let mut sub = vec![1, 0, 10];
    sub.extend_from_slice(b"gpu_shader");
    names.push(1);
    uleb(&mut names, sub.len() as u64);
    names.extend_from_slice(&sub);
    section(&mut m, 0, &names);
    m
}
