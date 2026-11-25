//! The JIT's AArch64 backend (macOS and Linux on 64-bit ARM): each block becomes a function
//! with NEON code.
//!
//! The lanes are done four at a time, the width of a NEON register: the block's code runs once
//! for lanes 0..3, again for 4..7, and so on (16 or 32 lanes). Within one such pass every IR
//! value is one register of four lanes, a lane mask included (all ones or zero in each lane):
//! nothing in the IR moves data between lanes, so the passes do not see each other, except
//! through predicates, of which each pass reads and writes its own four bits. Values live in
//! registers v0..v27 (v28..v31 are scratch for operations of several instructions); when there
//! are more, the one used furthest ahead goes to the frame's spill slots.
//!
//! The arithmetic is the interpreter's: IEEE operations as NEON does them (a fused
//! multiply-add is `fmla`, as `fp::fma` is `f32::mul_add` on this host), with the comparisons
//! and selections of `exec` where NEON differs (minimum and maximum, saturation, NaN). MUFU's
//! sine, cosine, powers and logarithms call `unary`, four lanes at a time.
//!
//! Code goes into memory mapped for it: on macOS with MAP_JIT, written with this thread's write
//! protection off (`pthread_jit_write_protect_np`), and the instruction cache is synchronised.
//! Once written it is only run, by any thread.
#![allow(unsafe_code)]

use super::*;
use crate::exec::{Exit, Lanes};
use crate::host::Host;
use crate::program::Program;

/// The code of a program for `lanes` lanes: each block as a function of its own (`run`), and
/// the whole program as one (`enter`).
pub struct Module {
    mem: Exec,
    /// each block's function (offset in `mem`), or None if it has no code
    entry: Vec<Option<usize>>,
    /// the program's function
    program: usize,
    /// for each instruction, where the program's code for it starts (or `leave`)
    table: Vec<u32>,
    leave: u32,
    /// spill slots (16 bytes each) the code may use
    pub slots: usize,
}

/// A compiled block: `f(gpr, pred, cb, active, spill, scratch, flags) -> 1`.
type BlockFn = unsafe extern "C" fn(*mut u32, *mut u32, *const u32, u32, *mut [u32; 4], *mut u32, *mut u32) -> u32;
/// The program: `f(ctx, pc) -> 0` (the lanes go on in the interpreter at `ctx.pc`) or 1 (they
/// stopped: `step` said why).
type ProgramFn = unsafe extern "C" fn(*mut Ctx, u32) -> u32;

/// What the program's code works on, and the interpreter's side of it.
#[repr(C)]
pub struct Ctx {
    gpr: *mut u32,
    pred: *mut u32,
    active: u32,
    pc: u32,
    fuel: u64,
    cb: *const u32,
    spill: *mut [u32; 4],
    scratch: *mut u32,
    /// runs the instruction at `pc` in the interpreter: 0, or 1 if the lanes stopped
    step: unsafe extern "C" fn(*mut Ctx) -> u32,
    env: *mut (),
    base: *const u8,
    table: *const u32,
    len: u32,
    cc: *mut u32,
}

// (offsets in Ctx, for the code)
const C_GPR: u32 = 0;
const C_PRED: u32 = 8;
const C_ACTIVE: u32 = 16;
const C_PC: u32 = 20;
const C_FUEL: u32 = 24;
const C_CB: u32 = 32;
const C_SPILL: u32 = 40;
const C_SCRATCH: u32 = 48;
const C_STEP: u32 = 56;
const C_BASE: u32 = 72;
const C_TABLE: u32 = 80;
const C_LEN: u32 = 88;
const C_CC: u32 = 96;

impl Module {
    pub fn new(code: &Code, lanes: usize) -> Option<Module> {
        let mut a = Asm::default();
        let mut entry = Vec::with_capacity(code.blocks.len());
        let mut slots = 0;
        for b in &code.blocks {
            let at = a.w.len();
            match block(&mut a, b, lanes) {
                Some(n) => {
                    entry.push(Some(at * 4));
                    slots = slots.max(n);
                }
                None => {
                    a.w.truncate(at);
                    a.lits.clear();
                    entry.push(None);
                }
            }
        }
        if entry.iter().all(Option::is_none) {
            return None;
        }
        let at = a.w.len();
        let (table, leave, n) = program(&mut a, code, &entry, lanes)?;
        slots = slots.max(n);
        let mem = Exec::new(&a.w)?;
        Some(Module { mem, entry, program: at * 4, table, leave, slots })
    }

    /// Runs block `b`: false if it has no code (nothing changed then).
    pub fn run(&self, b: u16, gpr: *mut u32, pred: *mut u32, cc: *mut u32, frame: &mut Frame, active: u32) -> bool {
        let Some(Some(at)) = self.entry.get(b as usize) else {
            return false;
        };
        if frame.spill.len() < self.slots {
            frame.spill.resize(self.slots, [0; 4]);
        }
        // SAFETY: `at` is the start of a function written by `block`, of type BlockFn, in memory
        // that stays mapped and unchanged while `self` lives. It reads and writes the lanes'
        // registers (256 of `lanes` words), predicates (8 words) and flags (4) it is given, reads the
        // constant buffer words the block was compiled for (`fill` made the frame hold them all),
        // and uses the frame's spill slots (`slots` of them) and scratch words (4).
        unsafe {
            let f = std::mem::transmute::<*const u8, BlockFn>(self.mem.base.add(*at));
            f(gpr, pred, frame.cb.as_ptr(), active, frame.spill.as_mut_ptr(), frame.scratch.as_mut_ptr(), cc) != 0
        }
    }

    /// Runs the program's code from the lanes' instruction (see `Code::enter`).
    pub fn enter<const N: usize, H: Host<N>>(&self, code: &Code, p: &Program, l: &mut Lanes<N>, h: &mut H, fuel: &mut u64) -> Option<Option<Exit>> {
        if *self.table.get(l.pc as usize)? == self.leave {
            return None;
        }
        code.fill_all(&mut l.frame, |i, o| h.cbuf(i, o));
        if l.frame.spill.len() < self.slots {
            l.frame.spill.resize(self.slots, [0; 4]);
        }
        let lp: *mut Lanes<N> = l;
        let mut env = Env::<N, H> { p, l: lp, h, exit: None, panic: None };
        // SAFETY: everything the code and `step` use is reached through `lp` and `env`, which
        // outlive the call; the code runs on this thread and gives everything back when it
        // returns. Its accesses are those `run` describes, with the registers, predicates and
        // frame of `l`, plus `ctx` and the table, which it only reads past `len` checks.
        unsafe {
            let mut ctx = Ctx {
                gpr: (*lp).gpr.as_mut_ptr() as *mut u32,
                pred: (*lp).pred.as_mut_ptr(),
                active: (*lp).active,
                pc: (*lp).pc,
                fuel: *fuel,
                cb: (*lp).frame.cb.as_ptr(),
                spill: (*lp).frame.spill.as_mut_ptr(),
                scratch: (*lp).frame.scratch.as_mut_ptr(),
                step: step::<N, H>,
                env: &mut env as *mut Env<N, H> as *mut (),
                base: self.mem.base,
                table: self.table.as_ptr(),
                len: self.table.len() as u32,
                cc: (*lp).cc.as_mut_ptr(),
            };
            let f = std::mem::transmute::<*const u8, ProgramFn>(self.mem.base.add(self.program));
            let stopped = f(&mut ctx, (*lp).pc) != 0;
            *fuel = ctx.fuel;
            if let Some(p) = env.panic.take() {
                std::panic::resume_unwind(p);
            }
            if stopped {
                return Some(Some(env.exit.unwrap_or(Exit::Timeout)));
            }
            (*lp).pc = ctx.pc;
            Some(None)
        }
    }
}

/// The interpreter's side of a program's run.
struct Env<'a, const N: usize, H> {
    p: &'a Program,
    l: *mut Lanes<N>,
    h: *mut H,
    exit: Option<Exit>,
    panic: Option<Box<dyn std::any::Any + Send>>,
}

/// The interpreter runs the instruction at `ctx.pc`: 0 when the code goes on (from `ctx.pc`,
/// with what the lanes are now), 1 when the lanes stop (why is in the environment).
unsafe extern "C" fn step<const N: usize, H: Host<N>>(ctx: *mut Ctx) -> u32 {
    // SAFETY: the code passes the context `enter` made, whose environment is an Env<N, H>; the
    // lanes and host are not otherwise in use while the code runs
    let ctx = unsafe { &mut *ctx };
    let env = unsafe { &mut *(ctx.env as *mut Env<N, H>) };
    let (l, h) = unsafe { (&mut *env.l, &mut *env.h) };
    l.pc = ctx.pc;
    if ctx.fuel == 0 {
        env.exit = Some(Exit::Timeout);
        return 1;
    }
    let p = env.p;
    match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| crate::exec::step(p, l, h, &mut ctx.fuel))) {
        Ok(None) => {
            ctx.pc = l.pc;
            ctx.active = l.active;
            0
        }
        Ok(Some(e)) => {
            env.exit = Some(e);
            1
        }
        Err(e) => {
            env.panic = Some(e);
            1
        }
    }
}

// ---------------------------------------------------------------------------- memory

/// Executable memory holding a module's code.
struct Exec {
    base: *mut u8,
    len: usize,
}

// SAFETY: the code is written once, before the value is shared, and then only executed
unsafe impl Send for Exec {}
unsafe impl Sync for Exec {}

mod sys {
    use std::ffi::{c_int, c_void};
    extern "C" {
        pub fn mmap(addr: *mut c_void, len: usize, prot: c_int, flags: c_int, fd: c_int, off: i64) -> *mut c_void;
        pub fn munmap(addr: *mut c_void, len: usize) -> c_int;
        #[cfg(target_os = "macos")]
        pub fn pthread_jit_write_protect_np(enabled: c_int);
        #[cfg(target_os = "macos")]
        pub fn sys_icache_invalidate(start: *mut c_void, len: usize);
        #[cfg(target_os = "linux")]
        pub fn __clear_cache(start: *mut c_void, end: *mut c_void);
    }
    #[cfg(target_os = "macos")]
    pub const FLAGS: c_int = 0x0002 | 0x1000 | 0x0800; // PRIVATE | ANON | JIT
    #[cfg(target_os = "linux")]
    pub const FLAGS: c_int = 0x0002 | 0x0020; // PRIVATE | ANONYMOUS
}

impl Exec {
    fn new(words: &[u32]) -> Option<Exec> {
        let bytes = words.len() * 4;
        let len = bytes.next_multiple_of(1 << 14);
        // SAFETY: an anonymous mapping the system places; failure is checked. The words are
        // copied into it with this thread's write protection off (macOS), and the instruction
        // cache is synchronised for them before anyone runs them.
        unsafe {
            let p = sys::mmap(std::ptr::null_mut(), len, 7, sys::FLAGS, -1, 0);
            if p as isize == -1 || p.is_null() {
                return None;
            }
            let base = p as *mut u8;
            #[cfg(target_os = "macos")]
            sys::pthread_jit_write_protect_np(0);
            std::ptr::copy_nonoverlapping(words.as_ptr() as *const u8, base, bytes);
            #[cfg(target_os = "macos")]
            {
                sys::pthread_jit_write_protect_np(1);
                sys::sys_icache_invalidate(p, bytes);
            }
            #[cfg(target_os = "linux")]
            sys::__clear_cache(p, base.add(bytes) as *mut std::ffi::c_void);
            Some(Exec { base, len })
        }
    }
}

impl Drop for Exec {
    fn drop(&mut self) {
        // SAFETY: the mapping made in `new`; nothing runs its code any more (its module is gone)
        unsafe {
            sys::munmap(self.base as *mut std::ffi::c_void, self.len);
        }
    }
}

// ---------------------------------------------------------------------------- encoding

/// Instruction words, with the literals (four words each) the code loads.
#[derive(Default)]
struct Asm {
    w: Vec<u32>,
    /// a literal and the `ldr q` instructions that load it
    lits: Vec<([u32; 4], Vec<usize>)>,
}

// general registers
const XG: u32 = 19; // the registers of this pass's lanes
const XP: u32 = 20; // predicates
const XC: u32 = 21; // constant buffer words
const WA: u32 = 22; // the lanes running
const XS: u32 = 23; // spill slots
const XK: u32 = 24; // 4 * pass (the first lane's bit)
const XF: u32 = 25; // flags
const XH: u32 = 26; // scratch words, for the helper
const X9: u32 = 9;
const X10: u32 = 10;
const X11: u32 = 11;
// vector registers: 0..ALLOC for values, then scratch
const ALLOC: usize = 28;
const T0: u32 = 28;
const T1: u32 = 29;
const T2: u32 = 30;
const T3: u32 = 31;

impl Asm {
    fn put(&mut self, w: u32) {
        self.w.push(w);
    }
    /// Three vector registers.
    fn v3(&mut self, op: u32, d: u32, n: u32, m: u32) {
        self.put(op | m << 16 | n << 5 | d);
    }
    /// Two.
    fn v2(&mut self, op: u32, d: u32, n: u32) {
        self.put(op | n << 5 | d);
    }
    fn mov(&mut self, d: u32, n: u32) {
        if d != n {
            self.v3(ORR, d, n, n);
        }
    }
    /// A literal of four words into `d`.
    fn lit(&mut self, d: u32, v: [u32; 4]) {
        let at = self.w.len();
        match self.lits.iter_mut().find(|l| l.0 == v) {
            Some(l) => l.1.push(at),
            None => self.lits.push((v, vec![at])),
        }
        self.put(0x9c00_0000 | d);
    }
    /// The same word in every lane.
    fn splat(&mut self, d: u32, x: u32) {
        match x {
            0 => self.put(0x6f00_e400 | d),             // movi d.2d, #0
            u32::MAX => self.put(0x6f07_e7e0 | d),      // movi d.2d, #-1
            0x7fff_ffff => self.put(0x6f04_6400 | d),   // mvni d.4s, #0x80, lsl #24
            0x8000_0000 => self.put(0x4f04_6400 | d),   // movi d.4s, #0x80, lsl #24
            0x3f80_0000 => self.put(0x4f03_f600 | d),   // fmov d.4s, #1.0
            _ => self.lit(d, [x; 4]),
        }
    }
    /// `mov wd, #x`.
    fn mov_w(&mut self, d: u32, x: u32) {
        self.put(0x5280_0000 | (x & 0xffff) << 5 | d);
        if x >> 16 != 0 {
            self.put(0x72a0_0000 | (x >> 16) << 5 | d); // movk wd, #hi, lsl #16
        }
    }
    /// `mov xd, #x`.
    fn mov_x(&mut self, d: u32, x: u64) {
        self.put(0xd280_0000 | ((x & 0xffff) as u32) << 5 | d);
        for hw in 1..4 {
            let part = (x >> (16 * hw)) as u32 & 0xffff;
            if part != 0 {
                self.put(0xf280_0000 | hw << 21 | part << 5 | d);
            }
        }
    }
    /// `ldr/str q t, [x n, #off]`, or false if the offset does not fit.
    fn ldst_q(&mut self, load: bool, t: u32, n: u32, off: usize) -> bool {
        if off % 16 != 0 || off / 16 >= 4096 {
            return false;
        }
        self.put(if load { 0x3dc0_0000 } else { 0x3d80_0000 } | ((off / 16) as u32) << 10 | n << 5 | t);
        true
    }
    /// `ldr/str x t, [x n, #off]`.
    fn ldst_x(&mut self, load: bool, t: u32, n: u32, off: u32) {
        debug_assert!(off % 8 == 0 && off / 8 < 4096);
        self.put(if load { 0xf940_0000 } else { 0xf900_0000 } | (off / 8) << 10 | n << 5 | t);
    }
    /// `mov wd, #x` in two instructions whatever x is (code that is skipped over counts them).
    fn mov_w_fixed(&mut self, d: u32, x: u32) {
        self.put(0x5280_0000 | (x & 0xffff) << 5 | d);
        self.put(0x72a0_0000 | (x >> 16) << 5 | d);
    }
    /// `b to`.
    fn branch(&mut self, to: usize) {
        let off = (to as i64 - self.w.len() as i64) as u32 & 0x3ff_ffff;
        self.put(0x1400_0000 | off);
    }
    /// `b.cond to`.
    fn branch_cond(&mut self, cond: u32, to: usize) {
        let off = (to as i64 - self.w.len() as i64) as u32 & 0x7_ffff;
        self.put(0x5400_0000 | off << 5 | cond);
    }
    /// Points the `b` at `at` to `to`.
    fn patch_branch(&mut self, at: usize, to: usize) {
        let off = (to as i64 - at as i64) as u32 & 0x3ff_ffff;
        self.w[at] = 0x1400_0000 | off;
    }
    /// `ldr/str w t, [x n, #off]`.
    fn ldst_w(&mut self, load: bool, t: u32, n: u32, off: u32) {
        debug_assert!(off % 4 == 0 && off / 4 < 4096);
        self.put(if load { 0xb940_0000 } else { 0xb900_0000 } | (off / 4) << 10 | n << 5 | t);
    }
    /// The lane mask of this pass's four bits of `w x` (in `T3`'s help: it is overwritten).
    fn mask_of(&mut self, d: u32, x: u32) {
        self.put(0x1ac0_2400 | XK << 16 | x << 5 | x); // lsr wx, wx, w24
        self.v2(0x4e04_0c00, d, x); // dup d.4s, wx
        self.lit(T3, [1, 2, 4, 8]);
        self.v3(CMTST, d, d, T3);
    }
    /// The four bits of a lane mask, at this pass's place, into `w x`.
    fn bits_of(&mut self, x: u32, v: u32, t: u32) {
        self.lit(t, [1, 2, 4, 8]);
        self.v3(AND, t, v, t);
        self.v2(0x4eb1_b800, t, t); // addv st, t.4s
        self.v2(0x0e04_3c00, x, t); // umov wx, t.s[0]
        self.put(0x1ac0_2000 | XK << 16 | x << 5 | x); // lsl wx, wx, w24
    }
    /// Literals after the code, and the loads pointed at them.
    fn pool(&mut self) {
        while self.w.len() % 4 != 0 {
            self.put(0xd503_201f); // nop
        }
        for (v, at) in std::mem::take(&mut self.lits) {
            let here = self.w.len();
            for a in at {
                let off = (here - a) as u32;
                self.w[a] |= (off & 0x7_ffff) << 5;
            }
            self.w.extend_from_slice(&v);
        }
    }
}

const ADD: u32 = 0x4ea0_8400;
const SUB: u32 = 0x6ea0_8400;
const MUL: u32 = 0x4ea0_9c00;
const CMGT: u32 = 0x4ea0_3400;
const CMEQ: u32 = 0x6ea0_8c00;
const CMTST: u32 = 0x4ea0_8c00;
const SSHL: u32 = 0x4ea0_4400;
const USHL: u32 = 0x6ea0_4400;
const AND: u32 = 0x4e20_1c00;
const ORR: u32 = 0x4ea0_1c00;
const ORN: u32 = 0x4ee0_1c00;
const EOR: u32 = 0x6e20_1c00;
const BSL: u32 = 0x6e60_1c00;
const FADD: u32 = 0x4e20_d400;
const FMUL: u32 = 0x6e20_dc00;
const FDIV: u32 = 0x6e20_fc00;
const FMLA: u32 = 0x4e20_cc00;
const FCMEQ: u32 = 0x4e20_e400;
const FCMGT: u32 = 0x6ea0_e400;
const CMEQ0: u32 = 0x4ea0_9800;
const CMLT0: u32 = 0x4ea0_a800;
const FCMGT0: u32 = 0x4ea0_c800;
const NOT: u32 = 0x6e20_5800;
const NEG: u32 = 0x6ea0_b800;

// ---------------------------------------------------------------------------- registers

/// Where IR values are while a block's code is made.
struct Ra {
    /// the lanes (a register's stride in the lanes' registers is four times as many bytes)
    lanes: usize,
    /// the positions that read each value, in order, and how many of them are behind
    uses: Vec<Vec<u32>>,
    seen: Vec<usize>,
    reg: Vec<Option<u8>>,
    slot: Vec<Option<u16>>,
    holder: [Option<u16>; ALLOC],
    /// registers the node being made reads or writes
    pinned: u32,
    slots: usize,
    failed: bool,
}

impl Ra {
    fn next_use(&mut self, v: usize, pos: u32) -> u32 {
        let u = &self.uses[v];
        while self.seen[v] < u.len() && u[self.seen[v]] <= pos {
            self.seen[v] += 1;
        }
        u.get(self.seen[v]).copied().unwrap_or(u32::MAX)
    }

    /// Writes register `r`'s value to its slot (once) and frees `r`.
    fn evict(&mut self, a: &mut Asm, r: usize) {
        let Some(v) = self.holder[r].take() else { return };
        let v = v as usize;
        if self.slot[v].is_none() {
            let s = self.slots;
            self.slots += 1;
            if !a.ldst_q(false, r as u32, XS, 16 * s) {
                self.failed = true;
            }
            self.slot[v] = Some(s as u16);
        }
        self.reg[v] = None;
    }

    /// A free register (a value goes to its slot if need be).
    fn free_reg(&mut self, a: &mut Asm, pos: u32) -> u32 {
        if let Some(r) = (0..ALLOC).find(|&r| self.holder[r].is_none() && self.pinned >> r & 1 == 0) {
            self.pinned |= 1 << r;
            return r as u32;
        }
        let mut best = None;
        let pinned = self.pinned;
        for r in (0..ALLOC).filter(|&r| pinned >> r & 1 == 0) {
            let n = self.next_use(self.holder[r].unwrap() as usize, pos);
            if best.is_none_or(|(_, b)| n > b) {
                best = Some((r, n));
            }
        }
        let Some((r, _)) = best else {
            self.failed = true;
            return T0;
        };
        self.evict(a, r);
        self.pinned |= 1 << r;
        r as u32
    }

    /// The register of value `v`, loaded from its slot if it is not in one.
    fn get(&mut self, a: &mut Asm, v: Id, pos: u32) -> u32 {
        let v = v as usize;
        if let Some(r) = self.reg[v] {
            self.pinned |= 1 << r;
            return r as u32;
        }
        let r = self.free_reg(a, pos);
        match self.slot[v] {
            Some(s) => {
                a.ldst_q(true, r, XS, 16 * s as usize);
            }
            None => self.failed = true,
        }
        self.reg[v] = Some(r as u8);
        self.holder[r as usize] = Some(v as u16);
        r
    }

    /// A register for the value node `k` makes.
    fn def(&mut self, a: &mut Asm, k: usize) -> u32 {
        let r = self.free_reg(a, k as u32);
        self.reg[k] = Some(r as u8);
        self.holder[r as usize] = Some(k as u16);
        r
    }

    /// Every value to its slot (before a call).
    fn spill_all(&mut self, a: &mut Asm) {
        for r in 0..ALLOC {
            self.evict(a, r);
        }
    }

    /// After node `k`: values it was the last to read are dropped, and so is its own if nothing
    /// reads it.
    fn done(&mut self, k: usize, args: &[Id]) {
        self.pinned = 0;
        for &v in args.iter().chain(std::iter::once(&(k as Id))) {
            let v = v as usize;
            if self.uses[v].last().is_none_or(|&l| l as usize <= k) {
                if let Some(r) = self.reg[v].take() {
                    self.holder[r as usize] = None;
                }
            }
        }
    }
}

// ---------------------------------------------------------------------------- blocks

/// The code of one block as a function, or None if it cannot be made. The spill slots it uses.
fn block(a: &mut Asm, b: &Block, lanes: usize) -> Option<usize> {
    prologue(a);
    for (d, n) in [(XG, 0), (XP, 1), (XC, 2), (XS, 4), (XH, 5), (XF, 6)] {
        a.put(0xaa00_03e0 | n << 16 | d); // mov xd, xn
    }
    a.put(0x2a00_03e0 | 3 << 16 | WA); // mov w22, w3
    let slots = body(a, b, lanes)?;
    a.mov_w(0, 1);
    epilogue(a);
    a.pool();
    Some(slots)
}

/// A block's code for every four lanes (the registers set up). The spill slots it uses.
fn body(a: &mut Asm, b: &Block, lanes: usize) -> Option<usize> {
    if lanes % 4 != 0 || !(4..=32).contains(&lanes) {
        return None;
    }
    // an expansion of a mask is the mask itself here
    let mut alias: Vec<Id> = (0..b.ir.len() as Id).collect();
    for (k, n) in b.ir.iter().enumerate() {
        if let Node::Expand(s) = *n {
            alias[k] = alias[s as usize];
        }
    }
    let args = |n: &Node| -> Vec<Id> { n.args().map(|x| alias[x as usize]).collect() };
    let mut uses = vec![Vec::new(); b.ir.len()];
    for (k, n) in b.ir.iter().enumerate() {
        if !matches!(n, Node::Expand(_)) {
            for x in args(n) {
                uses[x as usize].push(k as u32);
            }
        }
    }
    let mut ra = Ra {
        lanes,
        seen: vec![0; b.ir.len()],
        uses,
        reg: vec![None; b.ir.len()],
        slot: vec![None; b.ir.len()],
        holder: [None; ALLOC],
        pinned: 0,
        slots: 0,
        failed: false,
    };
    a.put(0xd280_0000 | XK); // mov x24, #0
    let top = a.w.len();
    for (k, n) in b.ir.iter().enumerate() {
        if matches!(n, Node::Expand(_)) {
            continue;
        }
        let xs = args(n);
        node(a, &mut ra, k, n, &xs)?;
        ra.done(k, &xs);
        if ra.failed {
            return None;
        }
    }
    // the next four lanes
    a.put(0x9100_0000 | 16 << 10 | XG << 5 | XG); // add x19, x19, #16
    a.put(0x9100_0000 | 4 << 10 | XK << 5 | XK); // add x24, x24, #4
    a.put(0xf100_001f | (lanes as u32) << 10 | XK << 5); // cmp x24, #lanes
    let back = (top as i64 - a.w.len() as i64) as u32 & 0x7_ffff;
    a.put(0x5400_000b | back << 5); // b.lt top
    Some(ra.slots)
}

/// The code of a whole program: `f(ctx, pc)`. Each block's code is there (with its own
/// literals); every other instruction is run by `ctx.step`, after which the code goes on at the
/// next instruction, or where the table says. The table (each instruction's code, or `leave`),
/// the offset of `leave`, and the spill slots.
fn program(a: &mut Asm, code: &Code, entry: &[Option<usize>], lanes: usize) -> Option<(Vec<u32>, u32, usize)> {
    let n = code.start.len();
    let mut slots = 0;
    // the function: x27 the context, x28 the code's base
    prologue(a);
    a.put(0xaa00_03e0 | 0 << 16 | 27); // mov x27, x0
    a.ldst_x(true, 28, 27, C_BASE);
    a.put(0x2a00_03e0 | 1 << 16 | X9); // mov w9, w1
    let to_dispatch = a.w.len();
    a.put(0x1400_0000); // b dispatch
    // leave: the lanes go on in the interpreter at ctx.pc (= w9)
    let leave = a.w.len();
    a.ldst_w(false, X9, 27, C_PC);
    a.mov_w(0, 0);
    epilogue(a);
    // stop: `step` said the lanes stopped
    let stop = a.w.len();
    a.mov_w(0, 1);
    epilogue(a);
    // dispatch: to the code of instruction w9, or leave
    let dispatch = a.w.len();
    a.ldst_w(true, X10, 27, C_LEN);
    a.put(0x6b00_001f | X10 << 16 | X9 << 5); // cmp w9, w10
    a.branch_cond(0x2, leave); // b.hs leave
    a.ldst_x(true, X11, 27, C_TABLE);
    a.put(0xb860_7800 | X9 << 16 | X11 << 5 | X10); // ldr w10, [x11, x9, lsl #2]
    a.put(0x8b00_0000 | X10 << 16 | 28 << 5 | X10); // add x10, x28, x10
    a.put(0xd61f_0000 | X10 << 5); // br x10
    a.patch_branch(to_dispatch, dispatch);
    // (offsets from the start of the module's code, which x28 holds)
    let mut table = vec![(leave * 4) as u32; n];
    let mut k = 0;
    while k < n {
        table[k] = (a.w.len() * 4) as u32;
        if let Some((b, len)) = code.block_at(k as u32).filter(|&(b, _)| entry[b as usize].is_some()) {
            // fuel for the block, or leave
            a.ldst_x(true, X9, 27, C_FUEL);
            a.mov_w(X10, len);
            a.put(0xeb00_001f | X10 << 16 | X9 << 5); // cmp x9, x10
            a.put(0x5400_0082); // b.hs +4 (over the way out)
            a.mov_w_fixed(X9, k as u32);
            a.branch(leave);
            a.put(0xcb00_0000 | X10 << 16 | X9 << 5 | X9); // sub x9, x9, x10
            a.ldst_x(false, X9, 27, C_FUEL);
            // (a block that writes nothing, NOPs, is only its fuel)
            let block = &code.blocks[b as usize];
            if block.ir.iter().any(Node::effect) {
                for (d, off) in [(XG, C_GPR), (XP, C_PRED), (XC, C_CB), (XS, C_SPILL), (XH, C_SCRATCH), (XF, C_CC)] {
                    a.ldst_x(true, d, 27, off);
                }
                a.ldst_w(true, WA, 27, C_ACTIVE);
                slots = slots.max(body(a, block, lanes)?);
            }
            // (its literals, jumped over)
            if !a.lits.is_empty() {
                let over = a.w.len();
                a.put(0x1400_0000);
                a.pool();
                let here = a.w.len();
                a.patch_branch(over, here);
            }
            k += len as usize;
            continue;
        }
        // the interpreter runs it
        a.mov_w_fixed(X9, k as u32);
        a.ldst_w(false, X9, 27, C_PC);
        a.put(0xaa00_03e0 | 27 << 16); // mov x0, x27
        a.ldst_x(true, X9, 27, C_STEP);
        a.put(0xd63f_0000 | X9 << 5); // blr x9
        a.put(0x3400_0040); // cbz w0, +2
        a.branch(stop);
        a.ldst_w(true, X9, 27, C_PC);
        a.mov_w_fixed(X10, k as u32 + 1);
        a.put(0x6b00_001f | X10 << 16 | X9 << 5); // cmp w9, w10
        a.put(0x5400_0040); // b.eq +2
        a.branch(dispatch);
        k += 1;
    }
    // past the end: the interpreter faults there
    a.mov_w_fixed(X9, n as u32);
    a.branch(leave);
    Some((table, (leave * 4) as u32, slots))
}

/// Saves what the code uses of what a call must keep (x19..x28, d8..d15).
fn prologue(a: &mut Asm) {
    let stp = |a: &mut Asm, t: u32, t2: u32| a.put(0xa9bf_03e0 | t2 << 10 | t);
    let stp_d = |a: &mut Asm, t: u32, t2: u32| a.put(0x6dbf_03e0 | t2 << 10 | t);
    stp(a, 29, 30);
    a.put(0x9100_03fd); // mov x29, sp
    for (t, t2) in [(19, 20), (21, 22), (23, 24), (25, 26), (27, 28)] {
        stp(a, t, t2);
    }
    for t in [8, 10, 12, 14] {
        stp_d(a, t, t + 1);
    }
}

fn epilogue(a: &mut Asm) {
    let ldp = |a: &mut Asm, t: u32, t2: u32| a.put(0xa8c1_03e0 | t2 << 10 | t);
    let ldp_d = |a: &mut Asm, t: u32, t2: u32| a.put(0x6cc1_03e0 | t2 << 10 | t);
    for t in [14, 12, 10, 8] {
        ldp_d(a, t, t + 1);
    }
    for (t, t2) in [(27, 28), (25, 26), (23, 24), (21, 22), (19, 20)] {
        ldp(a, t, t2);
    }
    ldp(a, 29, 30);
    a.put(0xd65f_03c0); // ret
}

/// The code of node `k` (its arguments `xs`, after aliasing).
fn node(a: &mut Asm, ra: &mut Ra, k: usize, n: &Node, xs: &[Id]) -> Option<()> {
    let pos = k as u32;
    match *n {
        Node::Reg(r) => {
            let d = ra.def(a, k);
            if !a.ldst_q(true, d, XG, r as usize * 4 * ra.lanes) {
                return None;
            }
        }
        Node::Imm(x) => {
            let d = ra.def(a, k);
            a.splat(d, x);
        }
        Node::Cb(i) => {
            let d = ra.def(a, k);
            if i >= 4096 {
                return None;
            }
            a.put(0xbd40_0000 | (i as u32) << 10 | XC << 5 | d); // ldr sd, [x21, #4i]
            a.v2(0x4e04_0400, d, d); // dup d.4s, vd.s[0]
        }
        Node::Un(op, _) => {
            if matches!(op, Un::Sin | Un::Cos | Un::Ex2 | Un::Lg2 | Un::Rcp64h | Un::Rsq64h) {
                return call(a, ra, k, op, xs[0]);
            }
            let x = ra.get(a, xs[0], pos);
            let d = ra.def(a, k);
            un(a, op, d, x);
        }
        Node::Bin(op, ..) => {
            let x = ra.get(a, xs[0], pos);
            let y = ra.get(a, xs[1], pos);
            let d = ra.def(a, k);
            bin(a, op, d, x, y);
        }
        Node::Sel(..) => {
            let m = ra.get(a, xs[0], pos);
            let x = ra.get(a, xs[1], pos);
            let y = ra.get(a, xs[2], pos);
            let d = ra.def(a, k);
            a.mov(d, m);
            a.v3(BSL, d, x, y);
        }
        Node::Prmt(_, _, sel) => {
            // a byte table lookup in the two (with their bytes' signs, four) registers
            let x = ra.get(a, xs[0], pos);
            let y = ra.get(a, xs[1], pos);
            let d = ra.def(a, k);
            let sign = sel & 0x8888 != 0;
            let mut idx = [0u8; 16];
            for lane in 0..4u8 {
                for i in 0..4 {
                    let n = (sel >> (4 * i) & 15) as u8;
                    let at = if n & 7 < 4 { 4 * lane + (n & 3) } else { 16 + 4 * lane + (n & 3) };
                    idx[4 * lane as usize + i] = at + if n & 8 != 0 { 32 } else { 0 };
                }
            }
            a.mov(T0, x);
            a.mov(T1, y);
            if sign {
                a.v2(0x4f09_0400, T2, T0); // sshr t2.16b, t0.16b, #7
                a.v2(0x4f09_0400, T3, T1);
            }
            a.lit(d, std::array::from_fn(|w| u32::from_le_bytes(idx[4 * w..4 * w + 4].try_into().unwrap())));
            a.v3(if sign { 0x4e00_6000 } else { 0x4e00_2000 }, d, T0, d); // tbl d, {t0..}, d
        }
        Node::Fma(..) => {
            let x = ra.get(a, xs[0], pos);
            let y = ra.get(a, xs[1], pos);
            let z = ra.get(a, xs[2], pos);
            let d = ra.def(a, k);
            a.mov(d, z);
            a.v3(FMLA, d, x, y);
        }
        Node::Expand(_) => {}
        Node::Active => {
            let d = ra.def(a, k);
            a.put(0x2a00_03e0 | WA << 16 | X9); // mov w9, w22
            a.mask_of(d, X9);
        }
        Node::Pred(p) => {
            let d = ra.def(a, k);
            a.ldst_w(true, X9, if p < 8 { XP } else { XF }, 4 * (p as u32 & if p < 8 { 7 } else { 3 }));
            a.mask_of(d, X9);
        }
        Node::SImm(x) => {
            let d = ra.def(a, k);
            match x {
                0 | u32::MAX => a.splat(d, x),
                _ => {
                    a.mov_w(X9, x);
                    a.mask_of(d, X9);
                }
            }
        }
        Node::S(op, ..) => {
            let x = ra.get(a, xs[0], pos);
            let y = ra.get(a, xs[1], pos);
            let d = ra.def(a, k);
            a.v3(
                match op {
                    SOp::And => AND,
                    SOp::Or => ORR,
                    SOp::Xor => EOR,
                },
                d,
                x,
                y,
            );
        }
        Node::Bits(_) => {
            let x = ra.get(a, xs[0], pos);
            let d = ra.def(a, k);
            a.v3(CMTST, d, x, x);
        }
        Node::StoreAll(r, _) => {
            let x = ra.get(a, xs[0], pos);
            if !a.ldst_q(false, x, XG, r as usize * 4 * ra.lanes) {
                return None;
            }
        }
        Node::StoreP(p, ..) => {
            // pred[p] = pred[p] & !m | v & m, in this pass's four bits
            let v = ra.get(a, xs[0], pos);
            let m = ra.get(a, xs[1], pos);
            a.v3(AND, T0, v, m);
            a.bits_of(X10, T0, T1);
            a.bits_of(X11, m, T1);
            let (base, at) = if p < 8 { (XP, 4 * p as u32) } else { (XF, 4 * (p as u32 & 3)) };
            a.ldst_w(true, X9, base, at);
            a.put(0x0a20_0000 | X11 << 16 | X9 << 5 | X9); // bic w9, w9, w11
            a.put(0x2a00_0000 | X10 << 16 | X9 << 5 | X9); // orr w9, w9, w10
            a.ldst_w(false, X9, base, at);
        }
        // (registers are written whole at the end of a block once `forward` has run)
        Node::Store(..) => return None,
    }
    Some(())
}

fn un(a: &mut Asm, op: Un, d: u32, x: u32) {
    let misc = |op: Un| -> Option<u32> {
        Some(match op {
            Un::Sqrt => 0x6ea1_f800,
            Un::Nearest => 0x4e21_8800,
            Un::Floor => 0x4e21_9800,
            Un::Ceil => 0x4ea1_8800,
            Un::Trunc => 0x4ea1_9800,
            Un::SToF => 0x4e21_d800,
            Un::UToF => 0x6e21_d800,
            Un::FToS => 0x4ea1_b800,
            Un::FToU => 0x6ea1_b800,
            _ => return None,
        })
    };
    if let Some(code) = misc(op) {
        a.v2(code, d, x);
        return;
    }
    match op {
        Un::Shl(s) => a.v2(0x4f00_5400 | (32 + s as u32) << 16, d, x),
        Un::ShrU(0) | Un::ShrS(0) => a.mov(d, x),
        Un::ShrU(s) => a.v2(0x6f00_0400 | (64 - s as u32) << 16, d, x),
        Un::ShrS(s) => a.v2(0x4f00_0400 | (64 - s as u32) << 16, d, x),
        Un::HalfLo | Un::HalfHi => {
            // the four halves (low or high 16 bits of each lane) widened: exact, as half_to_f32
            if op == Un::HalfLo {
                a.v2(0x0e61_2800, T0, x); // xtn t0.4h, x.4s
            } else {
                a.v2(0x0f10_8400, T0, x); // shrn t0.4h, x.4s, #16
            }
            a.v2(0x0e21_7800, d, T0); // fcvtl d.4s, t0.4h
        }
        Un::ToHalf => {
            // rounded to nearest even (FPCR's mode), zero-extended; a NaN is the sign and 0x7e00
            a.v2(0x0e21_6800, T0, x); // fcvtn t0.4h, x.4s
            a.v2(0x2f10_a400, T0, T0); // ushll t0.4s, t0.4h, #0
            a.v2(0x6f00_0400 | (64 - 31) << 16, T1, x); // ushr t1, x, #31
            a.v2(0x4f00_5400 | (32 + 15) << 16, T1, T1); // shl t1, t1, #15
            a.lit(T2, [0x7e00; 4]);
            a.v3(ORR, T1, T1, T2);
            a.v3(FCMEQ, T2, x, x);
            a.mov(d, T2);
            a.v3(BSL, d, T0, T1);
        }
        Un::Ftz => {
            // the sign alone where the exponent is zero
            a.v2(0x4f00_5400 | 33 << 16, T0, x); // shl t0, x, #1
            a.v2(0x6f00_0400 | (64 - 24) << 16, T0, T0); // ushr t0, t0, #24
            a.v2(CMEQ0, T0, T0);
            a.splat(T1, 0x8000_0000);
            a.v3(AND, T1, x, T1);
            a.mov(d, T0);
            a.v3(BSL, d, T1, x);
        }
        Un::Sat => {
            // x > 0 ? (x < 1 ? x : 1) : 0, NaN to 0
            a.splat(T1, 0x3f80_0000);
            a.v3(FCMGT, T0, T1, x); // 1 > x
            a.v3(BSL, T0, x, T1);
            a.v2(FCMGT0, T2, x); // x > 0
            a.v3(AND, d, T0, T2);
        }
        Un::Canon => {
            // x where it equals itself, else the GPU's NaN
            a.v3(FCMEQ, T0, x, x);
            a.splat(T1, crate::fp::NAN);
            a.mov(d, T0);
            a.v3(BSL, d, x, T1);
        }
        Un::Rsq => {
            // 1 / sqrt(x) in double, rounded to single
            a.v2(0x0e61_7800, T0, x); // fcvtl t0.2d, x.2s
            a.v2(0x4e61_7800, T1, x); // fcvtl2 t1.2d, x.4s
            a.v2(0x6ee1_f800, T0, T0); // fsqrt .2d
            a.v2(0x6ee1_f800, T1, T1);
            a.put(0x6f03_f600 | T2); // fmov t2.2d, #1.0
            a.v3(0x6e60_fc00, T0, T2, T0); // fdiv .2d
            a.v3(0x6e60_fc00, T1, T2, T1);
            a.v2(0x0e61_6800, d, T0); // fcvtn d.2s, t0.2d
            a.v2(0x4e61_6800, d, T1); // fcvtn2 d.4s, t1.2d
        }
        _ => unreachable!("{op:?}"),
    }
}

fn bin(a: &mut Asm, op: Bin, d: u32, x: u32, y: u32) {
    let simple = match op {
        Bin::And => Some((AND, false)),
        Bin::Or => Some((ORR, false)),
        Bin::Xor => Some((EOR, false)),
        Bin::Add => Some((ADD, false)),
        Bin::Sub => Some((SUB, false)),
        Bin::Mul => Some((MUL, false)),
        Bin::FAdd => Some((FADD, false)),
        Bin::FMul => Some((FMUL, false)),
        Bin::FDiv => Some((FDIV, false)),
        Bin::FEq => Some((FCMEQ, false)),
        Bin::FGt => Some((FCMGT, false)),
        Bin::FLt => Some((FCMGT, true)),
        Bin::IEq => Some((CMEQ, false)),
        Bin::IGt => Some((CMGT, false)),
        Bin::ILt => Some((CMGT, true)),
        _ => None,
    };
    if let Some((code, swap)) = simple {
        if swap {
            a.v3(code, d, y, x);
        } else {
            a.v3(code, d, x, y);
        }
        return;
    }
    match op {
        Bin::FUnord => {
            a.v3(FCMEQ, T0, x, x);
            a.v3(FCMEQ, T1, y, y);
            a.v3(AND, T0, T0, T1);
            a.v2(NOT, d, T0);
        }
        Bin::FMin | Bin::FMax => {
            // x where x < y (x > y), y is NaN, or they are equal and x (y) is negative
            let min = op == Bin::FMin;
            if min {
                a.v3(FCMGT, T0, y, x);
            } else {
                a.v3(FCMGT, T0, x, y);
            }
            a.v3(FCMEQ, T1, y, y);
            a.v3(ORN, T0, T0, T1);
            a.v3(FCMEQ, T1, x, y);
            a.v2(CMLT0, T2, if min { x } else { y });
            a.v3(AND, T1, T1, T2);
            a.v3(ORR, T0, T0, T1);
            a.mov(d, T0);
            a.v3(BSL, d, x, y);
        }
        Bin::Shl | Bin::ShrU | Bin::ShrS => {
            // each lane by its own amount, modulo 32 (to the right: a negative left shift)
            a.lit(T0, [31; 4]);
            a.v3(AND, T0, y, T0);
            if op != Bin::Shl {
                a.v2(NEG, T0, T0);
            }
            a.v3(if op == Bin::ShrS { SSHL } else { USHL }, d, x, T0);
        }
        _ => unreachable!("{op:?}"),
    }
}

/// MUFU's operations NEON does not have: `unary` on this pass's four lanes, through the
/// scratch words. Every value goes to its slot first (a call keeps no vector register).
fn call(a: &mut Asm, ra: &mut Ra, k: usize, op: Un, x: Id) -> Option<()> {
    let r = ra.get(a, x, k as u32);
    if !a.ldst_q(false, r, XH, 0) {
        return None;
    }
    ra.spill_all(a);
    let code = match op {
        Un::Sin => 0,
        Un::Cos => 1,
        Un::Ex2 => 2,
        Un::Lg2 => 3,
        Un::Rcp64h => 4,
        _ => 5,
    };
    a.mov_w(0, code);
    a.put(0xaa00_03e0 | XH << 16 | 1); // mov x1, x26
    a.mov_x(X9, helper as extern "C" fn(u32, *mut [u32; 4]) as usize as u64);
    a.put(0xd63f_0000 | X9 << 5); // blr x9
    ra.pinned = 0;
    let d = ra.def(a, k);
    a.ldst_q(true, d, XH, 0);
    Some(())
}

/// `unary` on four lanes, in place.
extern "C" fn helper(op: u32, v: *mut [u32; 4]) {
    let op = match op {
        0 => Un::Sin,
        1 => Un::Cos,
        2 => Un::Ex2,
        3 => Un::Lg2,
        4 => Un::Rcp64h,
        _ => Un::Rsq64h,
    };
    // SAFETY: the code passes the frame's scratch words, at least four
    let v = unsafe { &mut *v };
    for x in v.iter_mut() {
        *x = unary(op, *x);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Encodings against the system assembler's (clang, arm64).
    #[test]
    fn encodings() {
        let mut a = Asm::default();
        a.v3(ADD, 1, 2, 3);
        a.v3(SUB, 1, 2, 3);
        a.v3(MUL, 1, 2, 3);
        a.v3(CMGT, 1, 2, 3);
        a.v3(CMEQ, 1, 2, 3);
        a.v3(CMTST, 1, 2, 3);
        a.v3(SSHL, 1, 2, 3);
        a.v3(USHL, 1, 2, 3);
        a.v3(AND, 1, 2, 3);
        a.v3(ORR, 1, 2, 3);
        a.v3(ORN, 1, 2, 3);
        a.v3(EOR, 1, 2, 3);
        a.v3(BSL, 1, 2, 3);
        a.v3(FADD, 1, 2, 3);
        a.v3(FMUL, 1, 2, 3);
        a.v3(FDIV, 1, 2, 3);
        a.v3(FMLA, 1, 2, 3);
        a.v3(FCMEQ, 1, 2, 3);
        a.v3(FCMGT, 1, 2, 3);
        a.v2(CMEQ0, 1, 2);
        a.v2(CMLT0, 1, 2);
        a.v2(FCMGT0, 1, 2);
        a.v2(NOT, 1, 2);
        a.v2(NEG, 1, 2);
        un(&mut a, Un::Shl(5), 1, 2);
        un(&mut a, Un::ShrU(5), 1, 2);
        un(&mut a, Un::ShrS(5), 1, 2);
        a.splat(1, 0);
        a.splat(1, u32::MAX);
        a.splat(1, 0x7fff_ffff);
        a.splat(1, 0x8000_0000);
        a.splat(1, 0x3f80_0000);
        a.ldst_q(true, 1, 2, 32);
        a.ldst_q(false, 1, 2, 32);
        a.ldst_w(true, 1, 2, 8);
        a.ldst_w(false, 1, 2, 8);
        a.mov_w(1, 0x1234_0000);
        assert_eq!(
            a.w,
            [
                0x4ea38441, 0x6ea38441, 0x4ea39c41, 0x4ea33441, 0x6ea38c41, 0x4ea38c41, 0x4ea34441, 0x6ea34441, 0x4e231c41, 0x4ea31c41, 0x4ee31c41,
                0x6e231c41, 0x6e631c41, 0x4e23d441, 0x6e23dc41, 0x6e23fc41, 0x4e23cc41, 0x4e23e441, 0x6ea3e441, 0x4ea09841, 0x4ea0a841, 0x4ea0c841,
                0x6e205841, 0x6ea0b841, 0x4f255441, 0x6f3b0441, 0x4f3b0441, 0x6f00e401, 0x6f07e7e1, 0x6f046401, 0x4f046401, 0x4f03f601, 0x3dc00841,
                0x3d800841, 0xb9400841, 0xb9000841, 0x52800001, 0x72a24681,
            ]
        );
    }
}
