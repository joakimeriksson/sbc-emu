//! The AArch64 backend (macOS and Linux on 64-bit ARM): each block becomes a function with NEON
//! code, `f(regs, flags, args, inputs, outputs)`.
//!
//! The lanes are done four at a time, the width of a NEON register: the block's code runs once
//! for lanes 0..3, again for 4..7, and so on. Within one such pass every IR value is one
//! register of four lanes, a lane mask included (all ones or zero in each lane): the passes do
//! not see each other, except through flags, of which each pass reads and writes its own four
//! bits. A shuffle needs all 16 lanes of its operand: it starts a new phase (a loop of four
//! passes of its own), and values a later phase reads are kept in "wide" stack slots of 16
//! lanes. Values live in v0..v25 (v26 and v27 hold the masks of a float's exponent and sign,
//! v28..v31 are scratch); when there are more, the one used furthest ahead goes to a stack
//! slot.
//!
//! The arithmetic is IEEE as NEON does it with the default FPCR (`ieee` sets it around the
//! call: the guest's may be live when a GPU job runs within a CPU's register write), with the
//! comparisons and selections of `ir::binary` where NEON differs. What NEON has no instruction
//! for (`exp2`, `log2`, registered functions) calls `helper` four lanes at a time.
//!
//! Code goes into an arena of memory mapped for it (on macOS with MAP_JIT, written with this
//! thread's write protection off), never freed: blocks are cached by their front ends.
#![allow(unsafe_code)]

use crate::ir::*;

/// A compiled block.
pub struct Func {
    entry: *const u8,
    pub bytes: usize,
}

// SAFETY: the code is written once, before the value is made, and then only executed
unsafe impl Send for Func {}
unsafe impl Sync for Func {}

type F = unsafe extern "C" fn(*mut u32, *mut u32, *const u32, *const u32, *mut u32);

impl Func {
    pub fn new(b: &Block) -> Option<Func> {
        let words = compile(b)?;
        let entry = arena_put(&words)?;
        Some(Func { entry, bytes: words.len() * 4 })
    }

    /// # Safety
    /// The pointers must cover the block's registers, flags, arguments, inputs and outputs.
    #[inline]
    pub unsafe fn call(&self, regs: *mut u32, flags: *mut u32, args: *const u32, inputs: *const u32, outputs: *mut u32) {
        // SAFETY: `entry` is a function of type F written by `compile` into memory that stays
        // mapped and unchanged
        let f = unsafe { std::mem::transmute::<*const u8, F>(self.entry) };
        ieee(|| unsafe { f(regs, flags, args, inputs, outputs) });
    }
}

/// Runs `f` with the host's floating-point control register in its default state (round to
/// nearest even, IEEE halves, no flushing, NaNs propagated).
#[inline]
fn ieee<T>(f: impl FnOnce() -> T) -> T {
    let old: u64;
    // SAFETY: reading FPCR has no other effect
    unsafe { std::arch::asm!("mrs {}, fpcr", out(reg) old, options(nomem, nostack, preserves_flags)) };
    if old == 0 {
        return f();
    }
    // SAFETY: FPCR's default; the old value is put back before returning (`f` does not unwind:
    // `helper` aborts on a panic)
    unsafe { std::arch::asm!("msr fpcr, {}", in(reg) 0u64, options(nomem, nostack, preserves_flags)) };
    let r = f();
    // SAFETY: as above
    unsafe { std::arch::asm!("msr fpcr, {}", in(reg) old, options(nomem, nostack, preserves_flags)) };
    r
}

// ---------------------------------------------------------------------------- memory

mod sys {
    use std::ffi::{c_int, c_void};
    extern "C" {
        pub fn mmap(addr: *mut c_void, len: usize, prot: c_int, flags: c_int, fd: c_int, off: i64) -> *mut c_void;
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
    /// read, write and execute (macOS: writes switched per thread)
    pub const PROT: c_int = 7;
}

struct Arena {
    at: usize,
    left: usize,
}

const CHUNK: usize = 4 << 20;

/// Copies code into executable memory; its address.
fn arena_put(words: &[u32]) -> Option<*const u8> {
    static ARENA: std::sync::Mutex<Arena> = std::sync::Mutex::new(Arena { at: 0, left: 0 });
    let bytes = (words.len() * 4).next_multiple_of(64);
    if bytes > CHUNK {
        return None;
    }
    let mut a = ARENA.lock().unwrap_or_else(|e| e.into_inner());
    if a.left < bytes {
        // SAFETY: an anonymous mapping the system places; failure is checked
        let p = unsafe { sys::mmap(std::ptr::null_mut(), CHUNK, sys::PROT, sys::FLAGS, -1, 0) };
        if p as isize == -1 || p.is_null() {
            static ONCE: std::sync::Once = std::sync::Once::new();
            ONCE.call_once(|| eprintln!("[simd-jit] the system gives no memory for code; shaders are interpreted"));
            return None;
        }
        a.at = p as usize;
        a.left = CHUNK;
    }
    let p = a.at as *mut u8;
    a.at += bytes;
    a.left -= bytes;
    // SAFETY: `p` has `bytes` bytes of the mapping nobody else uses (the arena hands each out
    // once, under the lock); written with this thread's write protection off (macOS), and the
    // instruction cache is synchronised before anyone runs them
    unsafe {
        #[cfg(target_os = "macos")]
        sys::pthread_jit_write_protect_np(0);
        std::ptr::copy_nonoverlapping(words.as_ptr() as *const u8, p, words.len() * 4);
        #[cfg(target_os = "macos")]
        {
            sys::pthread_jit_write_protect_np(1);
            sys::sys_icache_invalidate(p as *mut _, bytes);
        }
        #[cfg(target_os = "linux")]
        sys::__clear_cache(p as *mut _, p.add(bytes) as *mut _);
    }
    Some(p)
}

// ---------------------------------------------------------------------------- encoding

/// A literal's words, and the instructions that load it (true: an ADR).
type Lit = (Vec<u32>, Vec<(usize, bool)>);

#[derive(Default)]
struct Asm {
    w: Vec<u32>,
    /// literals (4 or 16 words) and the instructions that point at them (true: an ADR)
    lits: Vec<Lit>,
    /// a branch or literal out of its instruction's reach
    far: bool,
}

// general registers
const XG: u32 = 19; // this pass's lanes of the registers
const XF: u32 = 20; // flags
const XA: u32 = 21; // arguments
const XI: u32 = 22; // this pass's lanes of the inputs
const XS: u32 = 23; // the stack frame
const XK: u32 = 24; // 4 * pass (the first lane's bit)
const XG0: u32 = 25;
const XI0: u32 = 26;
const XO0: u32 = 27;
const XO: u32 = 28; // this pass's lanes of the outputs
const X9: u32 = 9;
const X10: u32 = 10;
const X11: u32 = 11;
// vector registers: 0..ALLOC for values, two constants, then scratch
const ALLOC: usize = 26;
/// 0x7f80_0000 (a float's exponent) and 0x8000_0000 (its sign) in every lane, for `Un::Ftz`
const VEXP: u32 = 26;
const VSIGN: u32 = 27;
const T0: u32 = 28;
const T1: u32 = 29;
const T2: u32 = 30;
const T3: u32 = 31;

/// The frame: helper scratch (two vectors of four lanes), then the wide slots, then the spill
/// slots.
const WBASE: usize = 32;

impl Asm {
    fn put(&mut self, w: u32) {
        self.w.push(w);
    }
    fn v3(&mut self, op: u32, d: u32, n: u32, m: u32) {
        self.put(op | m << 16 | n << 5 | d);
    }
    fn v2(&mut self, op: u32, d: u32, n: u32) {
        self.put(op | n << 5 | d);
    }
    fn mov(&mut self, d: u32, n: u32) {
        if d != n {
            self.v3(ORR, d, n, n);
        }
    }
    fn lit_ref(&mut self, v: Vec<u32>, adr: bool) {
        let at = self.w.len();
        match self.lits.iter_mut().find(|l| l.0 == v) {
            Some(l) => l.1.push((at, adr)),
            None => self.lits.push((v, vec![(at, adr)])),
        }
    }
    /// A literal of four words into `d`.
    fn lit(&mut self, d: u32, v: [u32; 4]) {
        self.lit_ref(v.to_vec(), false);
        self.put(0x9c00_0000 | d); // ldr qd, literal
    }
    /// This pass's four words of a 16-word literal into `d` (x9 is overwritten).
    fn lit16(&mut self, d: u32, v: [u32; 16]) {
        self.lit_ref(v.to_vec(), true);
        self.put(0x1000_0000 | X9); // adr x9, literal
        self.put(0x8b00_0800 | XK << 16 | X9 << 5 | X9); // add x9, x9, x24, lsl #2
        self.put(0x3dc0_0000 | X9 << 5 | d); // ldr qd, [x9]
    }
    fn splat(&mut self, d: u32, x: u32) {
        match x {
            0 => self.put(0x6f00_e400 | d),           // movi d.2d, #0
            u32::MAX => self.put(0x6f07_e7e0 | d),    // movi d.2d, #-1
            0x7fff_ffff => self.put(0x6f04_6400 | d), // mvni d.4s, #0x80, lsl #24
            0x8000_0000 => self.put(0x4f04_6400 | d), // movi d.4s, #0x80, lsl #24
            0x3f80_0000 => self.put(0x4f03_f600 | d), // fmov d.4s, #1.0
            _ if x < 256 => self.put(0x4f00_0400 | (x >> 5) << 16 | (x & 31) << 5 | d), // movi d.4s, #x
            _ => self.lit(d, [x; 4]),
        }
    }
    fn mov_w(&mut self, d: u32, x: u32) {
        self.put(0x5280_0000 | (x & 0xffff) << 5 | d);
        if x >> 16 != 0 {
            self.put(0x72a0_0000 | (x >> 16) << 5 | d); // movk wd, #hi, lsl #16
        }
    }
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
        if !off.is_multiple_of(16) || off / 16 >= 4096 {
            self.far = true;
            return false;
        }
        self.put(if load { 0x3dc0_0000 } else { 0x3d80_0000 } | ((off / 16) as u32) << 10 | n << 5 | t);
        true
    }
    fn ldst_w(&mut self, load: bool, t: u32, n: u32, off: u32) {
        debug_assert!(off.is_multiple_of(4) && off / 4 < 4096);
        self.put(if load { 0xb940_0000 } else { 0xb900_0000 } | (off / 4) << 10 | n << 5 | t);
    }
    fn branch_cond(&mut self, cond: u32, to: usize) {
        let d = to as i64 - self.w.len() as i64;
        self.far |= d.unsigned_abs() >= 1 << 18;
        self.put(0x5400_0000 | (d as u32 & 0x7_ffff) << 5 | cond);
    }
    /// The lane mask of this pass's four bits of `w x` (x is overwritten, T3 too).
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
    /// Literals after the code, and the instructions pointed at them.
    fn pool(&mut self) {
        while !self.w.len().is_multiple_of(4) {
            self.put(0xd503_201f); // nop
        }
        for (v, at) in std::mem::take(&mut self.lits) {
            let here = self.w.len();
            for (a, adr) in at {
                let off = (here - a) as u32;
                if adr {
                    let bytes = off * 4;
                    self.far |= bytes >= 1 << 20;
                    self.w[a] |= (bytes & 3) << 29 | (bytes >> 2 & 0x7_ffff) << 5;
                } else {
                    self.far |= off >= 1 << 18;
                    self.w[a] |= (off & 0x7_ffff) << 5;
                }
            }
            self.w.extend_from_slice(&v);
        }
    }
}

const ADD: u32 = 0x4ea0_8400;
const SUB: u32 = 0x6ea0_8400;
const MUL: u32 = 0x4ea0_9c00;
const CMGT: u32 = 0x4ea0_3400;
const CMHI: u32 = 0x6ea0_3400;
const CMEQ: u32 = 0x6ea0_8c00;
const CMTST: u32 = 0x4ea0_8c00;
const SSHL: u32 = 0x4ea0_4400;
const USHL: u32 = 0x6ea0_4400;
const AND: u32 = 0x4e20_1c00;
const BIC: u32 = 0x4e60_1c00;
const ORR: u32 = 0x4ea0_1c00;
const EOR: u32 = 0x6e20_1c00;
const BSL: u32 = 0x6e60_1c00;
const FADD: u32 = 0x4e20_d400;
const FSUB: u32 = 0x4ea0_d400;
const FMUL: u32 = 0x6e20_dc00;
const FDIV: u32 = 0x6e20_fc00;
const FCMEQ: u32 = 0x4e20_e400;
const FCMGT: u32 = 0x6ea0_e400;
const FCMGE: u32 = 0x6e20_e400;
const FMINNM: u32 = 0x4ea0_c400;
const FMAXNM: u32 = 0x4e20_c400;
const SMIN: u32 = 0x4ea0_6c00;
const SMAX: u32 = 0x4ea0_6400;
const UMIN: u32 = 0x6ea0_6c00;
const UMAX: u32 = 0x6ea0_6400;
const UQADD8: u32 = 0x6e20_0c00;
const UQSUB8: u32 = 0x6e20_2c00;
const UMIN8: u32 = 0x6e20_6c00;
const UMAX8: u32 = 0x6e20_6400;
const CMLT0: u32 = 0x4ea0_a800;
const NOT: u32 = 0x6e20_5800;
const NEG: u32 = 0x6ea0_b800;
const CLZ: u32 = 0x6ea0_4800;
const UMULL8: u32 = 0x2e20_c000;
const UMULL8_2: u32 = 0x6e20_c000;
const ADD16: u32 = 0x4e60_8400;
const TBL4: u32 = 0x4e00_6000;

// ---------------------------------------------------------------------------- registers

/// Where IR values are while a block's code is made.
struct Ra {
    /// the positions that read each value, in order, and how many of them are behind
    uses: Vec<Vec<u32>>,
    seen: Vec<usize>,
    reg: Vec<Option<u8>>,
    slot: Vec<Option<u16>>,
    /// the wide slot of each value a later phase reads
    wide: Vec<Option<u16>>,
    nwide: usize,
    holder: [Option<u16>; ALLOC],
    /// registers the node being made reads or writes
    pinned: u32,
    slots: usize,
    /// the last node of the current phase
    phase_end: u32,
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

    fn slot_off(&self, s: usize) -> usize {
        WBASE + 64 * self.nwide + 16 * s
    }

    /// Frees register `r`, its value to a slot first if it has no other place.
    fn evict(&mut self, a: &mut Asm, r: usize) {
        let Some(v) = self.holder[r].take() else { return };
        let v = v as usize;
        if self.slot[v].is_none() && self.wide[v].is_none() {
            let s = self.slots;
            self.slots += 1;
            let off = self.slot_off(s);
            if !a.ldst_q(false, r as u32, XS, off) {
                self.failed = true;
            }
            self.slot[v] = Some(s as u16);
        }
        self.reg[v] = None;
    }

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
        if let Some(w) = self.wide[v] {
            a.put(0x8b00_0800 | XK << 16 | XS << 5 | X10); // add x10, x23, x24, lsl #2
            a.ldst_q(true, r, X10, WBASE + 64 * w as usize);
        } else if let Some(s) = self.slot[v] {
            let off = self.slot_off(s as usize);
            a.ldst_q(true, r, XS, off);
        } else {
            self.failed = true;
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

    /// Every value out of the registers (before a call).
    fn spill_all(&mut self, a: &mut Asm) {
        for r in 0..ALLOC {
            self.evict(a, r);
        }
    }

    /// After node `k`: values not read again in this phase leave their registers (they are in
    /// their wide slots if a later phase reads them).
    fn done(&mut self, k: usize, args: &[Id]) {
        self.pinned = 0;
        for &v in args.iter().chain(std::iter::once(&(k as Id))) {
            let v = v as usize;
            if self.next_use(v, k as u32) > self.phase_end {
                if let Some(r) = self.reg[v].take() {
                    self.holder[r as usize] = None;
                }
            }
        }
    }

    /// A new phase: nothing in registers.
    fn new_phase(&mut self, end: u32) {
        for r in 0..ALLOC {
            if let Some(v) = self.holder[r].take() {
                self.reg[v as usize] = None;
            }
        }
        self.phase_end = end;
    }
}

// ---------------------------------------------------------------------------- blocks

/// The function of a block, as instruction words.
fn compile(b: &Block) -> Option<Vec<u32>> {
    let n = b.ir.len();
    // an expansion of a mask is the mask itself here
    let mut alias: Vec<Id> = (0..n as Id).collect();
    for (k, node) in b.ir.iter().enumerate() {
        if let Node::Expand(s) = *node {
            alias[k] = alias[s as usize];
        }
    }
    let args = |node: &Node| -> Vec<Id> { node.args().map(|x| alias[x as usize]).collect() };
    // phases: a shuffle of a value of the current phase starts a new one
    let mut phase = vec![0u32; n];
    let mut cur = 0;
    for (k, node) in b.ir.iter().enumerate() {
        if let Node::Shuffle(a, _) = *node {
            if phase[alias[a as usize] as usize] == cur {
                cur += 1;
            }
        }
        phase[k] = cur;
    }
    let mut uses = vec![Vec::new(); n];
    let mut wide = vec![None; n];
    let mut nwide = 0;
    for (k, node) in b.ir.iter().enumerate() {
        if matches!(node, Node::Expand(_)) {
            continue;
        }
        for x in args(node) {
            uses[x as usize].push(k as u32);
            if (phase[x as usize] < phase[k] || matches!(node, Node::Shuffle(..))) && wide[x as usize].is_none() {
                wide[x as usize] = Some(nwide as u16);
                nwide += 1;
            }
        }
    }
    let mut ra = Ra {
        seen: vec![0; n],
        uses,
        reg: vec![None; n],
        slot: vec![None; n],
        wide,
        nwide,
        holder: [None; ALLOC],
        pinned: 0,
        slots: 0,
        phase_end: 0,
        failed: false,
    };
    let mut a = Asm::default();
    prologue(&mut a);
    for (d, s) in [(XG0, 0), (XF, 1), (XA, 2), (XI0, 3), (XO0, 4)] {
        a.put(0xaa00_03e0 | s << 16 | d); // mov xd, xs
    }
    let frame_at = a.w.len();
    a.put(0xd140_03ff); // sub sp, sp, #hi, lsl #12 (patched)
    a.put(0xd100_03ff); // sub sp, sp, #lo (patched)
    a.put(0x9100_03e0 | XS); // mov x23, sp
    consts(&mut a);
    let mut k = 0;
    while k < n {
        // one phase: nodes k..end, a loop of four passes
        let p = phase[k];
        let end = (k..n).find(|&j| phase[j] != p).unwrap_or(n);
        ra.new_phase(end as u32 - 1);
        a.put(0xd280_0000 | XK); // mov x24, #0
        let top = a.w.len();
        for (d, base) in [(XG, XG0), (XI, XI0), (XO, XO0)] {
            a.put(0x8b00_0800 | XK << 16 | base << 5 | d); // add xd, xbase, x24, lsl #2
        }
        for j in k..end {
            let node = &b.ir[j];
            if matches!(node, Node::Expand(_)) {
                continue;
            }
            let xs = args(node);
            emit(&mut a, &mut ra, b, j, node, &xs)?;
            // a value later phases read goes to its wide slot as it is made
            if let (Some(w), Some(r)) = (ra.wide[j], ra.reg[j]) {
                a.put(0x8b00_0800 | XK << 16 | XS << 5 | X10); // add x10, x23, x24, lsl #2
                a.ldst_q(false, r as u32, X10, WBASE + 64 * w as usize);
            }
            ra.done(j, &xs);
            if ra.failed || a.far {
                return None;
            }
        }
        a.put(0x9100_0000 | 4 << 10 | XK << 5 | XK); // add x24, x24, #4
        a.put(0xf100_001f | (LANES as u32) << 10 | XK << 5); // cmp x24, #16
        a.branch_cond(0xb, top); // b.lt top
        k = end;
    }
    let frame = (WBASE + 64 * ra.nwide + 16 * ra.slots).next_multiple_of(16);
    if frame >= 1 << 16 {
        return None;
    }
    a.w[frame_at] |= ((frame >> 12) as u32) << 10;
    a.w[frame_at + 1] |= ((frame & 0xfff) as u32) << 10;
    a.put(0xd100_0000 | 144 << 10 | 29 << 5 | 31); // sub sp, x29, #144
    epilogue(&mut a);
    a.pool();
    if a.far || ra.failed {
        return None;
    }
    Some(a.w)
}

/// The constant registers (again after a call, which may change them).
fn consts(a: &mut Asm) {
    a.lit(VEXP, [0x7f80_0000; 4]);
    a.put(0x4f04_6400 | VSIGN); // movi v27.4s, #0x80, lsl #24
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
fn emit(a: &mut Asm, ra: &mut Ra, b: &Block, k: usize, n: &Node, xs: &[Id]) -> Option<()> {
    let pos = k as u32;
    if let Some(code) = crate::helper_code(n) {
        return call(a, ra, k, code, xs);
    }
    match *n {
        Node::Reg(r) => {
            let d = ra.def(a, k);
            a.ldst_q(true, d, XG, r as usize * 64);
        }
        Node::Imm(x) => {
            let d = ra.def(a, k);
            a.splat(d, x);
        }
        Node::Const(c) => {
            let d = ra.def(a, k);
            a.lit16(d, b.consts[c as usize]);
        }
        Node::Arg(i) => {
            let d = ra.def(a, k);
            if i >= 4096 {
                return None;
            }
            a.put(0xbd40_0000 | (i as u32) << 10 | XA << 5 | d); // ldr sd, [x21, #4i]
            a.v2(0x4e04_0400, d, d); // dup d.4s, vd.s[0]
        }
        Node::In(i) => {
            let d = ra.def(a, k);
            a.ldst_q(true, d, XI, i as usize * 64);
        }
        Node::Lane => {
            let d = ra.def(a, k);
            a.v2(0x4e04_0c00, d, XK); // dup d.4s, w24
            a.lit(T0, [0, 1, 2, 3]);
            a.v3(ADD, d, d, T0);
        }
        Node::Shuffle(_, p) => {
            // the operand's 16 lanes (its wide slot) in t0..t3, a byte table lookup
            let w = ra.wide[xs[0] as usize]? as usize;
            for (i, t) in [T0, T1, T2, T3].into_iter().enumerate() {
                a.ldst_q(true, t, XS, WBASE + 64 * w + 16 * i);
            }
            let d = ra.def(a, k);
            let perm = b.perms[p as usize];
            let idx: [u32; 16] = std::array::from_fn(|lane| {
                let s = 4 * perm[lane] as u32;
                s | (s + 1) << 8 | (s + 2) << 16 | (s + 3) << 24
            });
            a.lit16(d, idx);
            a.v3(TBL4, d, T0, d);
        }
        Node::Un(op, _) => {
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
        Node::Expand(_) => {}
        Node::Flag(f) => {
            let d = ra.def(a, k);
            a.ldst_w(true, X9, XF, 4 * f as u32);
            a.mask_of(d, X9);
        }
        Node::SImm(x) => {
            let d = ra.def(a, k);
            match x & 0xffff {
                0 => a.splat(d, 0),
                0xffff => a.splat(d, u32::MAX),
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
            let code = match op {
                SOp::And => AND,
                SOp::Or => ORR,
                SOp::Xor => EOR,
            };
            a.v3(code, d, x, y);
        }
        Node::Bits(_) => {
            let x = ra.get(a, xs[0], pos);
            let d = ra.def(a, k);
            a.v3(CMTST, d, x, x);
        }
        Node::StoreAll(r, _) => {
            let x = ra.get(a, xs[0], pos);
            a.ldst_q(false, x, XG, r as usize * 64);
        }
        Node::Out(o, _) => {
            let x = ra.get(a, xs[0], pos);
            a.ldst_q(false, x, XO, o as usize * 64);
        }
        Node::Store(r, ..) => {
            // (`forward` leaves none, but a block may be made by hand)
            let v = ra.get(a, xs[0], pos);
            let m = ra.get(a, xs[1], pos);
            a.ldst_q(true, T0, XG, r as usize * 64);
            a.mov(T1, m);
            a.v3(BSL, T1, v, T0);
            a.ldst_q(false, T1, XG, r as usize * 64);
        }
        Node::StoreFlag(f, ..) => {
            // flags[f] = flags[f] & !m | v & m, in this pass's four bits
            let v = ra.get(a, xs[0], pos);
            let m = ra.get(a, xs[1], pos);
            a.v3(AND, T0, v, m);
            a.bits_of(X10, T0, T1);
            a.bits_of(X11, m, T1);
            a.ldst_w(true, X9, XF, 4 * f as u32);
            a.put(0x0a20_0000 | X11 << 16 | X9 << 5 | X9); // bic w9, w9, w11
            a.put(0x2a00_0000 | X10 << 16 | X9 << 5 | X9); // orr w9, w9, w10
            a.ldst_w(false, X9, XF, 4 * f as u32);
        }
    }
    Some(())
}

fn un(a: &mut Asm, op: Un, d: u32, x: u32) {
    let simple = match op {
        Un::Not => Some(NOT),
        Un::Neg => Some(NEG),
        Un::Clz => Some(CLZ),
        Un::IToF => Some(0x4e21_d800),
        Un::UToF => Some(0x6e21_d800),
        Un::FToI => Some(0x4ea1_b800),
        Un::FToU => Some(0x6ea1_b800),
        Un::FSqrt => Some(0x6ea1_f800),
        Un::FRound => Some(0x4e21_8800),
        Un::FTrunc => Some(0x4ea1_9800),
        Un::FFloor => Some(0x4e21_9800),
        Un::FCeil => Some(0x4ea1_8800),
        _ => None,
    };
    if let Some(code) = simple {
        a.v2(code, d, x);
        return;
    }
    match op {
        Un::Shl(0) | Un::ShrU(0) | Un::ShrS(0) => a.mov(d, x),
        Un::Shl(s) => a.v2(0x4f00_5400 | (32 + s as u32) << 16, d, x),
        Un::ShrU(s) => a.v2(0x6f00_0400 | (64 - s as u32) << 16, d, x),
        Un::ShrS(s) => a.v2(0x4f00_0400 | (64 - s as u32) << 16, d, x),
        Un::Ftz => {
            // the sign alone where the exponent is zero
            a.v3(CMTST, T0, x, VEXP);
            a.v3(ORR, T0, T0, VSIGN);
            a.v3(AND, d, x, T0);
        }
        Un::HalfToF => {
            a.v2(0x0e61_2800, T0, x); // xtn t0.4h, x.4s
            a.v2(0x0e21_7800, d, T0); // fcvtl d.4s, t0.4h
        }
        Un::FToHalf => {
            // rounded to nearest even, zero-extended; a NaN is its sign and 0x7e00
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
        Bin::IMin => Some((SMIN, false)),
        Bin::IMax => Some((SMAX, false)),
        Bin::UMin => Some((UMIN, false)),
        Bin::UMax => Some((UMAX, false)),
        Bin::IEq => Some((CMEQ, false)),
        Bin::ILt => Some((CMGT, true)),
        Bin::ULt => Some((CMHI, true)),
        Bin::FAdd => Some((FADD, false)),
        Bin::FSub => Some((FSUB, false)),
        Bin::FMul => Some((FMUL, false)),
        Bin::FDiv => Some((FDIV, false)),
        Bin::FMinNum => Some((FMINNM, false)),
        Bin::FMaxNum => Some((FMAXNM, false)),
        Bin::FEq => Some((FCMEQ, false)),
        Bin::FLt => Some((FCMGT, true)),
        Bin::FLe => Some((FCMGE, true)),
        Bin::V8AddSat => Some((UQADD8, false)),
        Bin::V8SubSat => Some((UQSUB8, false)),
        Bin::V8Min => Some((UMIN8, false)),
        Bin::V8Max => Some((UMAX8, false)),
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
            // x where x is a number and y is not, x < y (x > y), or they are equal and x (y)
            // is negative; else y
            let min = op == Bin::FMin;
            a.v3(FCMEQ, T0, x, x);
            a.v3(FCMEQ, T1, y, y);
            a.v3(BIC, T0, T0, T1);
            if min {
                a.v3(FCMGT, T1, y, x);
            } else {
                a.v3(FCMGT, T1, x, y);
            }
            a.v3(ORR, T0, T0, T1);
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
        Bin::Ror => {
            // x >> n | x << (32 - n): a shift by 32 gives zero
            a.lit(T0, [31; 4]);
            a.v3(AND, T0, y, T0);
            a.v2(NEG, T1, T0);
            a.v3(USHL, T1, x, T1);
            a.lit(T2, [32; 4]);
            a.v3(SUB, T2, T2, T0);
            a.v3(USHL, T2, x, T2);
            a.v3(ORR, d, T1, T2);
        }
        Bin::V8MulD => {
            // per byte: t = a * b + 128 in 16 bits; (t + (t >> 8)) >> 8
            a.v3(UMULL8, T0, x, y);
            a.v3(UMULL8_2, T1, x, y);
            a.put(0x4f04_8400 | T2); // movi t2.8h, #0x80
            a.v3(ADD16, T0, T0, T2);
            a.v3(ADD16, T1, T1, T2);
            a.v2(0x6f18_1400, T0, T0); // usra t0.8h, t0.8h, #8
            a.v2(0x6f18_1400, T1, T1);
            a.v2(0x0f08_8400, d, T0); // shrn d.8b, t0.8h, #8
            a.v2(0x4f08_8400, d, T1); // shrn2 d.16b, t1.8h, #8
        }
        _ => unreachable!("{op:?}"),
    }
}

/// What NEON has no instruction for: `helper` on this pass's four lanes, through the frame's
/// scratch. Every value leaves the registers first (a call keeps no vector register).
fn call(a: &mut Asm, ra: &mut Ra, k: usize, code: u32, xs: &[Id]) -> Option<()> {
    for (i, &x) in xs.iter().enumerate() {
        let r = ra.get(a, x, k as u32);
        a.ldst_q(false, r, XS, 16 * i);
    }
    ra.spill_all(a);
    a.mov_w(0, code);
    a.put(0x9100_0000 | XS << 5 | 1); // add x1, x23, #0
    a.mov_x(X9, helper as extern "C" fn(u32, *mut [u32; 8]) as usize as u64);
    a.put(0xd63f_0000 | X9 << 5); // blr x9
    consts(a);
    ra.pinned = 0;
    let d = ra.def(a, k);
    a.ldst_q(true, d, XS, 0);
    Some(())
}

/// An operation on four lanes, in place: `v[0..4]` the first operand and the result, `v[4..8]`
/// the second.
extern "C" fn helper(code: u32, v: *mut [u32; 8]) {
    // SAFETY: the code passes its frame's scratch, eight words
    let v = unsafe { &mut *v };
    let r = std::panic::catch_unwind(|| std::array::from_fn::<u32, 4, _>(|i| crate::helper_apply(code, v[i], v[4 + i])));
    match r {
        Ok(r) => v[..4].copy_from_slice(&r),
        // (unwinding through the generated code is not possible)
        Err(_) => std::process::abort(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Encodings against the system assembler's (clang, arm64).
    #[test]
    fn encodings() {
        let mut a = Asm::default();
        for op in [ADD, SUB, MUL, CMGT, CMHI, CMEQ, CMTST, SSHL, USHL, AND, BIC, ORR, 0x4ee0_1c00, EOR, BSL, FADD, FSUB, FMUL, FDIV, FCMEQ, FCMGT, FCMGE, FMINNM, FMAXNM, SMIN, SMAX, UMIN, UMAX, UQADD8, UQSUB8, UMIN8, UMAX8, UMULL8, UMULL8_2, ADD16, TBL4] {
            a.v3(op, 1, 2, 3);
        }
        for op in [0x4ea0_9800, CMLT0, 0x4ea0_c800, NOT, NEG, CLZ, 0x6f18_1400, 0x0f08_8400, 0x4f08_8400] {
            a.v2(op, 1, 2);
        }
        a.put(0x4f04_8400 | 1);
        a.splat(1, 200);
        assert_eq!(
            a.w,
            [
                0x4ea38441, 0x6ea38441, 0x4ea39c41, 0x4ea33441, 0x6ea33441, 0x6ea38c41, 0x4ea38c41, 0x4ea34441, 0x6ea34441, 0x4e231c41, 0x4e631c41, 0x4ea31c41,
                0x4ee31c41, 0x6e231c41, 0x6e631c41, 0x4e23d441, 0x4ea3d441, 0x6e23dc41, 0x6e23fc41, 0x4e23e441, 0x6ea3e441, 0x6e23e441, 0x4ea3c441, 0x4e23c441,
                0x4ea36c41, 0x4ea36441, 0x6ea36c41, 0x6ea36441, 0x6e230c41, 0x6e232c41, 0x6e236c41, 0x6e236441, 0x2e23c041, 0x6e23c041, 0x4e638441, 0x4e036041,
                0x4ea09841, 0x4ea0a841, 0x4ea0c841, 0x6e205841, 0x6ea0b841, 0x6ea04841, 0x6f181441, 0x0f088441, 0x4f088441, 0x4f048401, 0x4f060501,
            ]
        );
    }
}
