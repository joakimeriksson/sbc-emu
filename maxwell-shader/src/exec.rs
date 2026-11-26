//! The interpreter: N lanes run a program together (SIMT).
//!
//! All running lanes share one program counter. Lanes part at a branch they disagree about and
//! meet again where the code says: SSY, PBK, PCNT and CAL push a token with the meeting point,
//! and SYNC, BRK, CONT and RET park a lane at the nearest token of its kind. When no lane runs
//! any more, the topmost token is popped and its lanes go on.

use crate::fp;
use crate::host::{Handle, Host, IpaOp, SurfOp, TexOp};
use crate::isa::*;
use crate::program::Program;

/// Why `run` returned.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Exit {
    /// Every lane finished (or only helper lanes are left).
    Done,
    /// At a BAR instruction; call `run` again to go on.
    Barrier,
    /// Out of fuel; call `run` again to go on.
    Timeout,
    /// An unknown instruction or a broken control flow at this code address.
    Fault(u32),
}

#[derive(Clone, Copy, Debug, PartialEq)]
enum Kind {
    Ssy,
    Pbk,
    Pcnt,
    Ret,
    /// Lanes that took the other way at a branch.
    Div,
}

#[derive(Clone, Copy, Debug)]
struct Token {
    kind: Kind,
    pc: u32,
    /// Lanes waiting here.
    lanes: u32,
}

const MAX_DEPTH: usize = 4096;

/// Registers and control state of N lanes. Lane n of a quad is at x = n & 1, y = n >> 1 & 1;
/// with more than 4 lanes, every four lanes form a quad. (Aligned to a cache line, as lanes run on
/// several threads at once.)
#[derive(Clone)]
#[repr(align(128))]
pub struct Lanes<const N: usize> {
    /// General registers by lane. Register 255 must stay zero.
    pub gpr: Box<[[u32; N]; REGS]>,
    /// Predicates, one bit per lane; predicate 7 is all ones.
    pub pred: [u32; 8],
    /// SM70's uniform predicates UP0..UP6 and UPT (7, all ones); `Pred` names them as 8..15.
    pub upred: [u32; 8],
    /// Flags, one bit per lane.
    /// Condition flags, one bit per lane: zero, sign, carry, overflow.
    pub(crate) cc: [u32; 4],
    local: Vec<u8>,
    local_size: usize,
    pub(crate) pc: u32,
    pub(crate) active: u32,
    start: u32,
    exited: u32,
    killed: u32,
    helper: u32,
    stack: Vec<Token>,
    /// Masks SAM saved.
    sam: Vec<u32>,
    /// Registers the last program may have written (the ones `start` clears for the next).
    written: usize,
    /// What the JIT's blocks read (kept to reuse its memory).
    pub(crate) frame: crate::jit::Frame,
}

/// All N lanes.
pub const fn all<const N: usize>() -> u32 {
    if N >= 32 {
        !0
    } else {
        (1 << N) - 1
    }
}

impl<const N: usize> Default for Lanes<N> {
    fn default() -> Self {
        Self::new()
    }
}

impl<const N: usize> Lanes<N> {
    /// The lanes running (for tracing).
    pub fn active_mask(&self) -> u32 {
        self.active
    }

    pub fn new() -> Self {
        assert!((4..=32).contains(&N) && N.is_multiple_of(4), "lanes come in quads, at most 32");
        let gpr: Box<[[u32; N]]> = vec![[0; N]; REGS].into_boxed_slice();
        Lanes {
            gpr: gpr.try_into().unwrap(),
            pred: [0, 0, 0, 0, 0, 0, 0, !0],
            upred: [0, 0, 0, 0, 0, 0, 0, !0],
            cc: [0; 4],
            local: Vec::new(),
            local_size: 0,
            pc: 0,
            active: 0,
            start: 0,
            exited: 0,
            killed: 0,
            helper: 0,
            stack: Vec::with_capacity(16),
            sam: Vec::new(),
            written: 0,
            frame: Default::default(),
        }
    }

    /// Size of the local memory (l[]) of each lane, in bytes; its contents are cleared when the
    /// size changes.
    pub fn set_local_size(&mut self, bytes: usize) {
        if bytes != self.local_size {
            self.local_size = bytes;
            self.local.clear();
            self.local.resize(bytes * N, 0);
        }
    }

    /// Clears the local memory (lanes kept from an earlier launch start like new ones).
    pub fn clear_local(&mut self) {
        self.local.fill(0);
    }

    /// Prepares to run `p` from its entry in `lanes`. Lanes in `helper` (a subset) run only
    /// for the sake of their quad's derivatives: their stores and atomics do nothing. Clears the
    /// registers the program uses, and those the program before wrote.
    pub fn start(&mut self, p: &Program, lanes: u32, helper: u32) {
        // (and those an earlier program wrote: lanes kept from it start as new ones)
        let n = (p.gprs() as usize).min(REGS);
        self.gpr[..n.max(self.written)].fill([0; N]);
        self.written = n;
        self.pred = [0, 0, 0, 0, 0, 0, 0, !0];
        self.upred = [0, 0, 0, 0, 0, 0, 0, !0];
        self.cc = [0; 4];
        self.pc = p.entry;
        self.start = lanes & all::<N>();
        self.active = self.start;
        self.helper = helper & self.start;
        self.exited = 0;
        self.killed = 0;
        self.stack.clear();
        self.sam.clear();
    }

    /// Lanes that executed KIL.
    pub fn killed(&self) -> u32 {
        self.killed
    }

    /// Lanes whose results count: started, not helpers, not killed.
    pub fn live(&self) -> u32 {
        self.start & !self.helper & !self.killed
    }

    /// Lanes running now.
    pub fn active(&self) -> u32 {
        self.active
    }

    pub fn f32(&self, r: Reg, lane: usize) -> f32 {
        f32::from_bits(self.gpr[r as usize][lane])
    }

    #[inline(always)]
    fn put(&mut self, d: Reg, m: u32, v: [u32; N]) {
        if d == RZ {
            return;
        }
        let r = &mut self.gpr[d as usize];
        if m == all::<N>() {
            *r = v;
        } else {
            for i in 0..N {
                if m >> i & 1 != 0 {
                    r[i] = v[i];
                }
            }
        }
    }

    #[inline(always)]
    fn put1(&mut self, d: Reg, lane: usize, v: u32) {
        if d != RZ {
            self.gpr[d as usize][lane] = v;
        }
    }

    #[inline(always)]
    fn putf(&mut self, d: Reg, m: u32, v: [f32; N]) {
        self.put(d, m, v.map(f32::to_bits));
    }

    fn putd(&mut self, d: Reg, m: u32, v: [f64; N]) {
        self.put(d, m, v.map(|x| x.to_bits() as u32));
        self.put(reg2(d), m, v.map(|x| (x.to_bits() >> 32) as u32));
    }

    #[inline(always)]
    fn putp(&mut self, p: u8, m: u32, v: u32) {
        if p & 7 != 7 {
            let r = if p < 8 { &mut self.pred[p as usize] } else { &mut self.upred[p as usize & 7] };
            *r = *r & !m | v & m;
        }
    }

    #[inline(always)]
    fn p(&self, p: Pred) -> u32 {
        let v = if p.idx < 8 { self.pred[p.idx as usize] } else { self.upred[p.idx as usize & 7] };
        if p.not {
            !v
        } else {
            v
        }
    }

    #[inline(always)]
    fn src<H: Host<N>>(&self, h: &H, s: Src) -> [u32; N] {
        match s {
            Src::Reg(r) => self.gpr[r as usize],
            Src::Cb(i, o) => [h.cbuf(i as u32, o); N],
            Src::Imm(v) => [v; N],
        }
    }

    #[inline(always)]
    fn srcf<H: Host<N>>(&self, h: &H, s: Src, m: Mod, ftz: bool) -> [f32; N] {
        modf(self.src(h, s), m, ftz)
    }

    fn src64<H: Host<N>>(&self, h: &H, s: Src) -> [u64; N] {
        match s {
            Src::Reg(r) => {
                let (lo, hi) = (&self.gpr[r as usize], &self.gpr[reg2(r) as usize]);
                std::array::from_fn(|i| lo[i] as u64 | (hi[i] as u64) << 32)
            }
            Src::Cb(i, o) => {
                let lo = h.cbuf(i as u32, o) as u64;
                let hi = h.cbuf(i as u32, o.wrapping_add(4)) as u64;
                [lo | hi << 32; N]
            }
            Src::Imm(v) => [(v as u64) << 32; N],
        }
    }

    fn srcd<H: Host<N>>(&self, h: &H, s: Src, m: Mod) -> [f64; N] {
        self.src64(h, s).map(|x| {
            let x = if m.abs { x & !(1 << 63) } else { x };
            f64::from_bits(x ^ (m.neg as u64) << 63)
        })
    }

    fn flags_f(&mut self, m: u32, v: [f32; N]) {
        let nan = mask::<N>(|i| v[i].is_nan());
        self.flags(m, mask::<N>(|i| v[i] == 0.0) | nan, mask::<N>(|i| v[i] < 0.0) | nan);
    }

    fn flags_d(&mut self, m: u32, v: [f64; N]) {
        let nan = mask::<N>(|i| v[i].is_nan());
        self.flags(m, mask::<N>(|i| v[i] == 0.0) | nan, mask::<N>(|i| v[i] < 0.0) | nan);
    }

    fn flags_i(&mut self, m: u32, v: [u32; N]) {
        self.flags(m, mask::<N>(|i| v[i] == 0), mask::<N>(|i| (v[i] as i32) < 0));
    }

    /// Sets zero and sign, clears carry and overflow.
    fn flags(&mut self, m: u32, z: u32, s: u32) {
        self.cc[0] = self.cc[0] & !m | z & m;
        self.cc[1] = self.cc[1] & !m | s & m;
        self.cc[2] &= !m;
        self.cc[3] &= !m;
    }

    fn flags_co(&mut self, m: u32, c: u32, o: u32) {
        self.cc[2] = self.cc[2] & !m | c & m;
        self.cc[3] = self.cc[3] & !m | o & m;
    }

    /// Lanes whose flags satisfy a flow condition.
    fn test(&self, cond: u8) -> u32 {
        let (z, s) = (self.cc[0], self.cc[1]);
        match cond {
            15 => !0,
            0..=14 => {
                let mut r = 0;
                if cond & 1 != 0 {
                    r |= s & !z;
                }
                if cond & 2 != 0 {
                    r |= z & !s;
                }
                if cond & 4 != 0 {
                    r |= !s & !z;
                }
                if cond & 8 != 0 {
                    r |= s & z;
                }
                r
            }
            16 => !self.cc[3],
            17 => !self.cc[2],
            18 => !s,
            19 => !self.cc[2] | z,
            20 => self.cc[2] & !z,
            21 => s,
            22 => self.cc[2],
            23 => self.cc[3],
            _ => 0,
        }
    }

    /// Parks the lanes `m` at the nearest token of this kind. False if there is none.
    fn park(&mut self, kind: Kind, m: u32) -> bool {
        match self.stack.iter_mut().rev().find(|t| t.kind == kind) {
            Some(t) => {
                t.lanes |= m;
                self.active &= !m;
                true
            }
            None => false,
        }
    }

    /// Continues with lanes that wait, if no lane runs. False if there are none.
    fn unwind(&mut self) -> bool {
        while self.active == 0 {
            match self.stack.pop() {
                Some(t) => {
                    if t.lanes != 0 {
                        self.active = t.lanes;
                        self.pc = t.pc;
                    }
                }
                None => return false,
            }
        }
        true
    }

    /// Whether a lane that counts still runs (or waits).
    fn unfinished(&self) -> bool {
        self.start & !self.exited & !self.helper & !self.killed != 0
    }
}

/// The register after `r`.
#[inline(always)]
fn reg2(r: Reg) -> Reg {
    if r == RZ {
        RZ
    } else {
        r.wrapping_add(1)
    }
}

#[inline(always)]
fn reg_at(r: Reg, n: usize) -> Reg {
    crate::decode::reg_at(r, n as u32)
}

#[inline(always)]
fn mask<const N: usize>(f: impl Fn(usize) -> bool) -> u32 {
    let mut m = 0;
    for i in 0..N {
        m |= (f(i) as u32) << i;
    }
    m
}

#[inline(always)]
fn modf<const N: usize>(v: [u32; N], m: Mod, ftz: bool) -> [f32; N] {
    let (and, xor) = (m.and(), m.xor());
    if ftz {
        v.map(|x| f32::from_bits(fp::ftz(x) & and ^ xor))
    } else {
        v.map(|x| f32::from_bits(x & and ^ xor))
    }
}

/// The GPU's float arithmetic returns one NaN, 0x7fffffff, whatever the operands' were (which of
/// two NaNs the host's instruction returns depends on their order, which the compiler may swap).
#[inline(always)]
fn canon<const N: usize>(v: [f32; N]) -> [f32; N] {
    // (on the bits: as a float select the compiler may take any NaN for any other)
    v.map(|x| f32::from_bits(fp::canon(x.to_bits())))
}

#[inline(always)]
fn finish<const N: usize>(v: [f32; N], ftz: bool, sat: bool) -> [f32; N] {
    let mut v = canon(v);
    if ftz {
        v = v.map(|x| f32::from_bits(fp::ftz(x.to_bits())));
    }
    if sat {
        v = v.map(fp::sat);
    }
    v
}

#[inline(always)]
fn cmp32<const N: usize>(cond: FCond, a: &[f32; N], b: &[f32; N]) -> u32 {
    let mut m = 0;
    if cond & 1 != 0 {
        m |= mask::<N>(|i| a[i] < b[i]);
    }
    if cond & 2 != 0 {
        m |= mask::<N>(|i| a[i] == b[i]);
    }
    if cond & 4 != 0 {
        m |= mask::<N>(|i| a[i] > b[i]);
    }
    if cond & 8 != 0 {
        m |= mask::<N>(|i| a[i].is_nan() || b[i].is_nan());
    }
    m
}

#[inline(always)]
fn cmpi<const N: usize>(cond: ICond, signed: bool, a: &[u32; N], b: &[u32; N]) -> u32 {
    // An unsigned comparison is a signed one with both sign bits flipped.
    let flip = if signed { 0 } else { 0x8000_0000 };
    let (x, y) = (|i: usize| (a[i] ^ flip) as i32, |i: usize| (b[i] ^ flip) as i32);
    let mut m = 0;
    if cond & 1 != 0 {
        m |= mask::<N>(|i| x(i) < y(i));
    }
    if cond & 2 != 0 {
        m |= mask::<N>(|i| x(i) == y(i));
    }
    if cond & 4 != 0 {
        m |= mask::<N>(|i| x(i) > y(i));
    }
    m
}

/// Source `k` of a packed half-float operation as two floats (low and high result lane).
fn halves<const N: usize, H: Host<N>>(l: &Lanes<N>, h: &H, s: &HalfSrcs, k: usize, ftz: bool) -> [[f32; N]; 2] {
    let raw = match k {
        0 => l.gpr[s.a as usize],
        1 => l.src(h, s.b),
        _ => l.src(h, s.c),
    };
    let m = s.m[k];
    let f = |x: f32| {
        let x = if ftz && x != 0.0 && x.abs() < 6.103_515_6e-5 { 0.0f32.copysign(x) } else { x };
        let x = if m.abs { x.abs() } else { x };
        if m.neg { -x } else { x }
    };
    let half = |v: u32, high: bool| fp::half_to_f32(if high { (v >> 16) as u16 } else { v as u16 });
    let pick = |v: u32, lane: usize| match s.swz[k] {
        1 => f32::from_bits(v),
        2 => half(v, false),
        3 => half(v, true),
        _ => half(v, lane == 1),
    };
    [raw.map(|v| f(pick(v, 0))), raw.map(|v| f(pick(v, 1)))]
}

#[inline(always)]
fn bool_op(op: BoolOp, a: u32, b: u32) -> u32 {
    match op {
        BoolOp::And => a & b,
        BoolOp::Or => a | b,
        BoolOp::Xor => a ^ b,
    }
}

/// All ones or zero (or 1.0 and 0.0) from a mask.
#[inline(always)]
fn bools<const N: usize>(m: u32, float: bool) -> [u32; N] {
    let t = if float { 0x3f80_0000 } else { !0 };
    std::array::from_fn(|i| if m >> i & 1 != 0 { t } else { 0 })
}

fn bfe(a: u32, pos: u32, len: u32, signed: bool) -> u32 {
    if len == 0 {
        return 0;
    }
    let top = (pos + len - 1).min(31);
    let fill = if signed && a >> top & 1 != 0 { !0u32 } else { 0 };
    let v = if pos < 32 { a >> pos } else { fill };
    if len >= 32 {
        return if pos < 32 && pos > 0 { v | fill << (32 - pos) } else { v };
    }
    let m = (1u32 << len) - 1;
    // Bits beyond the source read as its sign.
    let v = if pos > 0 && pos < 32 { v | fill << (32 - pos) } else { v };
    v & m | fill & !m
}

fn bfi(a: u32, pos: u32, len: u32, base: u32) -> u32 {
    if pos >= 32 || len == 0 {
        return base;
    }
    let m = if len >= 32 { !0 } else { (1u32 << len) - 1 } << pos;
    base & !m | a << pos & m
}

fn flo(x: u32, signed: bool, shift: bool) -> u32 {
    let x = if signed && x >> 31 != 0 { !x } else { x };
    if x == 0 {
        !0
    } else if shift {
        x.leading_zeros()
    } else {
        31 - x.leading_zeros()
    }
}

/// PRMT: bytes of `a` (0..3) and `c` (4..7) picked by `sel` and the mode. Shifts on the
/// 64-bit pair, without a byte array: the interpreter runs it for every lane.
#[inline(always)]
pub fn prmt(a: u32, sel: u32, c: u32, mode: u8) -> u32 {
    let x = a as u64 | (c as u64) << 32;
    let s = sel & 3;
    // the source's byte n
    let byte = |n: u32| (x >> (8 * n)) as u32 & 0xff;
    match mode {
        // each result byte by a nibble: bits 0..2 the source byte, bit 3 its sign in all bits
        0 => {
            let mut r = 0;
            for i in 0..4 {
                let n = sel >> (4 * i);
                let b = (x >> (8 * (n & 7))) as u32;
                let b = if n & 8 != 0 { ((b as u8 as i8) >> 7) as u8 as u32 } else { b & 0xff };
                r |= b << (8 * i);
            }
            r
        }
        // forward: bytes s..s + 4
        1 => (x >> (8 * s)) as u32,
        // backward: byte i is source byte i - s, wrapping around the eight
        2 => x.rotate_left(8 * s) as u32,
        // byte s in all four
        3 => byte(s) * 0x0101_0101,
        // edge clamps: byte i is source byte max(i, s) or min(i, s)
        4 => {
            let keep = !0u32 << (8 * s);
            a & keep | byte(s) * 0x0101_0101 & !keep
        }
        5 => {
            let keep = if s == 3 { !0 } else { !(!0u32 << (8 * (s + 1))) };
            a & keep | byte(s) * 0x0101_0101 & !keep
        }
        // half s & 1 in both halves
        _ => {
            let h = a >> (16 * (s & 1)) & 0xffff;
            h | h << 16
        }
    }
}

/// The selector of PRMT's mode 0 (a nibble per result byte) that does what `sel` does in `mode`.
pub fn prmt_bytes(sel: u32, mode: u8) -> u32 {
    let s = sel & 3;
    let n = |i: u32| match mode {
        0 => sel >> (4 * i) & 15,
        1 => i + s,
        2 => (i + 8 - s) & 7,
        3 => s,
        4 => i.max(s),
        5 => i.min(s),
        _ => (i & 1) + (s & 1) * 2,
    };
    (0..4).fold(0, |r, i| r | n(i) << (4 * i))
}

fn shift(v: u32, n: u32, wrap: bool) -> Option<(u32, u32)> {
    if wrap {
        Some((v, n & 31))
    } else if n < 32 {
        Some((v, n))
    } else {
        None
    }
}

/// Reads an integer of a conversion.
fn int_in(lo: u32, hi: u32, ty: IntTy, sel: u8) -> i128 {
    let x = match ty.bytes {
        1 => lo >> (8 * sel as u32) & 0xff,
        2 => lo >> (8 * sel as u32) & 0xffff,
        _ => lo,
    };
    match (ty.bytes, ty.signed) {
        (1, true) => x as u8 as i8 as i128,
        (2, true) => x as u16 as i16 as i128,
        (4, true) => x as i32 as i128,
        (8, true) => (lo as u64 | (hi as u64) << 32) as i64 as i128,
        (8, false) => (lo as u64 | (hi as u64) << 32) as i128,
        _ => x as i128,
    }
}

fn int_range(ty: IntTy) -> (i128, i128) {
    let bits = ty.bytes as u32 * 8;
    if ty.signed {
        (-(1i128 << (bits - 1)), (1i128 << (bits - 1)) - 1)
    } else {
        (0, (1i128 << bits) - 1)
    }
}

/// The register value of an integer of type `ty` (low word first), extended to a word.
fn int_out(v: i128, ty: IntTy) -> u64 {
    match (ty.bytes, ty.signed) {
        (1, false) => v as u8 as u64,
        (1, true) => v as i8 as u32 as u64,
        (2, false) => v as u16 as u64,
        (2, true) => v as i16 as u32 as u64,
        (4, _) => v as u32 as u64,
        _ => v as u64,
    }
}

/// Extends a loaded value to a word.
fn extend(b: &[u8], ty: MemTy) -> u32 {
    match ty {
        MemTy::U8 => b[0] as u32,
        MemTy::S8 => b[0] as i8 as u32,
        MemTy::U16 => u16::from_le_bytes([b[0], b[1]]) as u32,
        MemTy::S16 => i16::from_le_bytes([b[0], b[1]]) as u32,
        _ => u32::from_le_bytes([b[0], b[1], b[2], b[3]]),
    }
}

/// FCHK: whether a / b (as bits) needs the exact division routine. Saying yes too often only
/// costs time; the routine's answer is right either way.
fn fchk(a: u32, b: u32) -> bool {
    let (ea, eb) = ((a >> 23 & 0xff) as i32, (b >> 23 & 0xff) as i32);
    let a_zero = a & 0x7fff_ffff == 0;
    // b zero, denormal, infinite or NaN; a denormal, infinite or NaN
    if eb == 0 || eb == 0xff || ea == 0xff || (ea == 0 && !a_zero) {
        return true;
    }
    // the quotient (and the reciprocal of b) well inside the normal range
    !a_zero && !(-120..=120).contains(&(ea - eb)) || !(3..=252).contains(&eb) || !a_zero && !(3..=252).contains(&ea)
}

/// The program offset of the last memory instruction run (on any thread): a fault report's best guess.
pub static LAST_MEMORY_PC: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);

/// What a fault returns (and prints with SBC_SHADER_FAULT=1).
#[cold]
fn fault(p: &Program, pc: u32) -> Exit {
    if std::env::var_os("SBC_SHADER_FAULT").is_some() {
        let w = p.words.get(pc as usize).copied();
        eprintln!("[shader fault] at {:#x} word {:#018x} {:?}", w.map_or(0, |w| w.0), w.map_or(0, |w| w.1), p.code.get(pc as usize).map(|i| &i.op));
        if w.is_none() {
            let prev = (pc as usize).checked_sub(1).and_then(|k| p.words.get(k).map(|w| (w.0, w.1, p.code[k].op, p.code[k].pred)));
            eprintln!("[shader fault]   pc index {pc} of {} entry {:#x}; before: {prev:x?}", p.code.len(), p.words.get(p.entry as usize).map_or(0, |w| w.0));
        }
    }
    Exit::Fault(p.words.get(pc as usize).or(p.words.last()).map_or(0, |w| w.0))
}

/// Runs the lanes until they finish, reach a barrier or run out of fuel (instructions).
pub fn run<const N: usize, H: Host<N>>(
    p: &Program,
    l: &mut Lanes<N>,
    h: &mut H,
    fuel: &mut u64,
) -> Exit {
    let jit = if crate::jit::lanes_ok(N) { p.jit() } else { None };
    // (after the native code gave the lanes back: the interpreter runs one instruction first)
    let mut left = false;
    loop {
        if *fuel == 0 {
            return Exit::Timeout;
        }
        if let Some(j) = jit {
            if !std::mem::take(&mut left) {
                match j.enter(p, l, h, fuel) {
                    Some(Some(e)) => return e,
                    Some(None) => {
                        left = true;
                        continue;
                    }
                    None => {}
                }
            }
            if let Some((b, len)) = j.block_at(l.pc) {
                if l.active != 0 && *fuel >= len as u64 {
                    j.fill(b, &mut l.frame, |i, o| h.cbuf(i, o));
                    if j.run(b, (&mut l.gpr[..256]).try_into().unwrap(), &mut l.pred, &mut l.cc, &mut l.frame, l.active) {
                        *fuel -= len as u64;
                        l.pc += len;
                        continue;
                    }
                }
            }
        }
        if let Some(e) = step(p, l, h, fuel) {
            return e;
        }
    }
}

/// Runs the instruction at the lanes' program counter (one of the fuel): Some if the lanes stop.
#[inline(always)]
pub(crate) fn step<const N: usize, H: Host<N>>(p: &Program, l: &mut Lanes<N>, h: &mut H, fuel: &mut u64) -> Option<Exit> {
    let code = &p.code[..];
    *fuel -= 1;
    let pc = l.pc;
    let Some(inst) = code.get(pc as usize) else {
        return Some(fault(p, pc));
    };
    l.pc = pc + 1;
    h.trace(p.words[pc as usize].0, inst, l);
    if interp_stats() {
        count_interpreted(&inst.op);
    }
    let m = l.active & l.p(inst.pred);
    if m == 0 {
        return None;
    }

    match inst.op {
        Op::Nop | Op::Fence => {}
        // (the workgroups of a parallel launch share memory through relaxed atomics)
        Op::Membar => std::sync::atomic::fence(std::sync::atomic::Ordering::SeqCst),
        Op::Invalid(_) | Op::Bpt => return Some(fault(p, pc)),

        Op::FAdd { d, a, am, b, bm, ftz, sat, cc, rnd } => {
            let x = modf(l.gpr[a as usize], am, ftz);
            let y = l.srcf(h, b, bm, ftz);
            let v = if rnd == Rnd::N {
                std::array::from_fn(|i| x[i] + y[i])
            } else {
                std::array::from_fn(|i| fp::add(x[i], y[i], rnd))
            };
            let v = finish(v, ftz, sat);
            if cc {
                l.flags_f(m, v);
            }
            l.putf(d, m, v);
        }
        Op::FMul { d, a, b, neg, fmz, sat, cc, rnd, scale } => {
            let flush = fmz.ftz || fmz.dnz;
            let x = modf(l.gpr[a as usize], Mod { abs: false, neg }, flush);
            let y = l.srcf(h, b, Mod::default(), flush);
            let mut v: [f32; N] = if rnd == Rnd::N {
                std::array::from_fn(|i| x[i] * y[i])
            } else {
                std::array::from_fn(|i| fp::mul(x[i], y[i], rnd))
            };
            if scale != 1.0 {
                v = v.map(|x| x * scale);
            }
            if fmz.dnz {
                v = std::array::from_fn(|i| if x[i] == 0.0 || y[i] == 0.0 { 0.0 } else { v[i] });
            }
            let v = finish(v, flush, sat);
            if cc {
                l.flags_f(m, v);
            }
            l.putf(d, m, v);
        }
        Op::Ffma { d, a, b, c, neg_ab, neg_c, fmz, sat, cc, rnd } => {
            let flush = fmz.ftz || fmz.dnz;
            let x = modf(l.gpr[a as usize], Mod { abs: false, neg: neg_ab }, flush);
            let y = l.srcf(h, b, Mod::default(), flush);
            let z = l.srcf(h, c, Mod { abs: false, neg: neg_c }, flush);
            let mut v: [f32; N] = if rnd == Rnd::N {
                std::array::from_fn(|i| fp::fma(x[i], y[i], z[i]))
            } else {
                std::array::from_fn(|i| fp::fma_rnd(x[i], y[i], z[i], rnd))
            };
            if fmz.dnz {
                v = std::array::from_fn(|i| if x[i] == 0.0 || y[i] == 0.0 { z[i] } else { v[i] });
            }
            let v = finish(v, flush, sat);
            if cc {
                l.flags_f(m, v);
            }
            l.putf(d, m, v);
        }
        Op::FMnMx { d, a, am, b, bm, ftz, cc, p } => {
            let x = modf(l.gpr[a as usize], am, ftz);
            let y = l.srcf(h, b, bm, ftz);
            let sel = l.p(p);
            let v = if sel & m == m {
                std::array::from_fn(|i| fp::min(x[i], y[i]))
            } else if sel & m == 0 {
                std::array::from_fn(|i| fp::max(x[i], y[i]))
            } else {
                std::array::from_fn(|i| {
                    if sel >> i & 1 != 0 {
                        fp::min(x[i], y[i])
                    } else {
                        fp::max(x[i], y[i])
                    }
                })
            };
            if cc {
                l.flags_f(m, v);
            }
            l.putf(d, m, v);
        }
        Op::FCmp { d, a, b, c, cond, ftz } => {
            let z = l.srcf(h, c, Mod::default(), ftz);
            let t = cmp32(cond, &z, &[0.0; N]);
            let (x, y) = (l.gpr[a as usize], l.src(h, b));
            l.put(d, m, std::array::from_fn(|i| if t >> i & 1 != 0 { x[i] } else { y[i] }));
        }
        Op::FSet { d, cmp, bf, cc } => {
            let t = fcmp(l, h, &cmp);
            let r = bool_op(cmp.bop, t, l.p(cmp.p));
            if cc {
                l.flags(m, !r, if bf { 0 } else { r });
            }
            l.put(d, m, bools(r, bf));
        }
        Op::FSetP { pd, pn, cmp } => {
            let t = fcmp(l, h, &cmp);
            let q = l.p(cmp.p);
            l.putp(pd, m, bool_op(cmp.bop, t, q));
            l.putp(pn, m, bool_op(cmp.bop, !t, q));
        }
        Op::FSwzAdd { d, a, b, ops, ftz, cc, rnd } => {
            let x = modf(l.gpr[a as usize], Mod::default(), ftz);
            let y = modf(l.gpr[b as usize], Mod::default(), ftz);
            let v = std::array::from_fn(|i| match ops >> (6 - 2 * (i & 3)) & 3 {
                0 => fp::add(x[i], y[i], rnd),
                1 => fp::add(y[i], -x[i], rnd),
                2 => fp::add(x[i], -y[i], rnd),
                _ => y[i],
            });
            let v = finish(v, ftz, false);
            if cc {
                l.flags_f(m, v);
            }
            l.putf(d, m, v);
        }
        Op::Half { d, op, s, f32, sat, ftz, p } => {
            let ([x0, x1], [y0, y1], [z0, z1]) = (halves(l, h, &s, 0, ftz), halves(l, h, &s, 1, ftz), halves(l, h, &s, 2, ftz));
            let sel = l.p(p);
            let calc = |x: f32, y: f32, z: f32, lane: usize| -> f32 {
                let r = match op {
                    HalfOp::Add => x + y,
                    HalfOp::Mul => x * y,
                    HalfOp::Fma => x.mul_add(y, z),
                    HalfOp::MnMx if sel >> lane & 1 != 0 => fp::min(x, y),
                    HalfOp::MnMx => fp::max(x, y),
                };
                if sat { fp::sat(r) } else { r }
            };
            let v = std::array::from_fn(|i| {
                let lo = calc(x0[i], y0[i], z0[i], i);
                if f32 {
                    return lo.to_bits();
                }
                let hi = calc(x1[i], y1[i], z1[i], i);
                let pack = |v: f32| {
                    let h = fp::f32_to_half(v, Rnd::N);
                    if ftz && h & 0x7c00 == 0 { h & 0x8000 } else { h }
                };
                pack(lo) as u32 | (pack(hi) as u32) << 16
            });
            l.put(d, m, v);
        }
        Op::HSetP { pd, pn, s, cond, ftz, bop, p, hand } => {
            let ([x0, x1], [y0, y1]) = (halves(l, h, &s, 0, ftz), halves(l, h, &s, 1, ftz));
            let lo = cmp32(cond, &x0, &y0);
            let hi = cmp32(cond, &x1, &y1);
            let q = l.p(p);
            if hand {
                l.putp(pd, m, bool_op(bop, lo & hi, q));
                l.putp(pn, m, bool_op(bop, !(lo & hi), q));
            } else {
                l.putp(pd, m, bool_op(bop, lo, q));
                l.putp(pn, m, bool_op(bop, hi, q));
            }
        }
        Op::PackHalves { d, a, b, rnd } => {
            let (x, y) = (l.gpr[a as usize], l.src(h, b));
            let half = |v: u32| fp::f32_to_half(f32::from_bits(v), rnd) as u32;
            l.put(d, m, std::array::from_fn(|i| half(y[i]) | half(x[i]) << 16));
        }
        Op::Mufu { d, a, m: md, op, sat } => {
            let x = l.gpr[a as usize];
            let v = mufu(x, md, op);
            let v = if sat { v.map(|x| fp::sat(f32::from_bits(x)).to_bits()) } else { v };
            l.put(d, m, v);
        }
        Op::Rro { d, b, m: md } => {
            let v = l.srcf(h, b, md, false);
            l.putf(d, m, v);
        }

        Op::DAdd { .. }
        | Op::DMul { .. }
        | Op::Dfma { .. }
        | Op::DMnMx { .. }
        | Op::DSet { .. }
        | Op::DSetP { .. } => double(l, h, m, &inst.op),
        Op::F2F { from: 4, to: 4, d, b, m: md, rnd, int, ftz, sat, cc, .. } => {
            let v = l.srcf(h, b, md, ftz);
            let v = if int { v.map(|x| fp::round_int32(x, rnd)) } else { v };
            let v = finish(v, ftz, sat);
            if cc {
                l.flags_f(m, v);
            }
            l.putf(d, m, v);
        }
        Op::F2I { from: 4, to: IntTy { bytes: 4, signed }, d, b, m: md, rnd, ftz, cc, .. } => {
            let v = l.srcf(h, b, md, ftz);
            let v = if rnd == Rnd::Z { v } else { v.map(|x| fp::round_int32(x, rnd)) };
            // `as` clamps, and NaN becomes 0.
            let v = if signed { v.map(|x| x as i32 as u32) } else { v.map(|x| x as u32) };
            if cc {
                l.flags_i(m, v);
            }
            l.put(d, m, v);
        }
        Op::I2F { from: IntTy { bytes: 4, signed }, to: 4, d, b, m: md, rnd: Rnd::N, cc, .. }
            if !md.abs =>
        {
            let v = l.src(h, b);
            let v: [f32; N] = if md.neg {
                v.map(|x| if signed { -(x as i32 as i64) as f32 } else { -(x as i64) as f32 })
            } else if signed {
                v.map(|x| x as i32 as f32)
            } else {
                v.map(|x| x as f32)
            };
            if cc {
                l.flags_f(m, v);
            }
            l.putf(d, m, v);
        }
        Op::F2F { .. } | Op::F2I { .. } | Op::I2F { .. } | Op::I2I { .. } => {
            convert(l, h, m, &inst.op)
        }

        Op::IAdd { d, a, b, neg_a, neg_b, x, sat, cc } => {
            let (mut u, mut v) = (l.gpr[a as usize], l.src(h, b));
            let po = neg_a && neg_b && !x;
            if neg_a && !po {
                u = u.map(|x| !x);
            }
            if neg_b && !po {
                v = v.map(|x| !x);
            }
            let carry = if x {
                l.cc[2]
            } else if neg_a || neg_b {
                !0
            } else {
                0
            };
            if !cc && !sat {
                let r = std::array::from_fn(|i| {
                    u[i].wrapping_add(v[i]).wrapping_add(carry >> i & 1)
                });
                l.put(d, m, r);
            } else {
                let t: [u64; N] =
                    std::array::from_fn(|i| u[i] as u64 + v[i] as u64 + (carry >> i & 1) as u64);
                let o = mask::<N>(|i| ((u[i] ^ t[i] as u32) & (v[i] ^ t[i] as u32)) >> 31 != 0);
                let mut r = t.map(|x| x as u32);
                if sat {
                    r = std::array::from_fn(|i| {
                        if o >> i & 1 == 0 {
                            r[i]
                        } else if u[i] >> 31 != 0 {
                            0x8000_0000
                        } else {
                            0x7fff_ffff
                        }
                    });
                }
                if cc {
                    l.flags_i(m, r);
                    l.flags_co(m, mask::<N>(|i| t[i] >> 32 != 0), o);
                }
                l.put(d, m, r);
            }
        }
        Op::FChk { pd, a, b } => {
            let (u, v) = (l.gpr[a as usize], l.src(h, b));
            let mut t = 0u32;
            for i in 0..N {
                t |= (fchk(u[i], v[i]) as u32) << i;
            }
            l.putp(pd, m, t);
        }
        Op::IAdd3P { d, a, b, c, neg, x, cin, cout } => {
            let (va, vb, vc) = (l.gpr[a as usize], l.src(h, b), l.src(h, c));
            let (c0, c1) = (l.p(cin[0]), l.p(cin[1]));
            // -v is ~v + 1, except in the high word of a 64-bit add (the low word's carry has it)
            let arg = |v: u32, n: bool| if !n { v as u64 } else if x { !v as u64 } else { !v as u64 + 1 };
            let (mut r, mut k0, mut k1) = ([0u32; N], 0u32, 0u32);
            for i in 0..N {
                let s0 = arg(va[i], neg[0]) + arg(vb[i], neg[1]) + (c0 >> i & 1) as u64;
                let s1 = (s0 & 0xffff_ffff) + arg(vc[i], neg[2]) + (c1 >> i & 1) as u64;
                r[i] = s1 as u32;
                k0 |= ((s0 >> 32 != 0) as u32) << i;
                k1 |= ((s1 >> 32 != 0) as u32) << i;
            }
            l.put(d, m, r);
            l.putp(cout[0], m, k0);
            l.putp(cout[1], m, k1);
        }
        Op::IAdd3 { d, a, b, c, neg, half, shift, x, cc } => {
            let srcs = [l.gpr[a as usize], l.src(h, b), l.gpr[c as usize]];
            let get = |s: usize, i: usize| {
                let v = match half[s] {
                    1 => srcs[s][i] & 0xffff,
                    2 => srcs[s][i] >> 16,
                    _ => srcs[s][i],
                };
                if neg[s] {
                    v.wrapping_neg()
                } else {
                    v
                }
            };
            let mut c_out = 0;
            let r = std::array::from_fn(|i| {
                let ab = get(0, i) as u64 + get(1, i) as u64;
                let ab = match shift {
                    1 => ab >> 16,
                    2 => (ab << 16) & 0xffff_ffff,
                    _ => ab,
                };
                let cin = if x { l.cc[2] >> i & 1 } else { 0 };
                let t = ab + get(2, i) as u64 + cin as u64;
                c_out |= ((t >> 32 != 0) as u32) << i;
                t as u32
            });
            if cc {
                l.flags_i(m, r);
                l.flags_co(m, c_out, 0);
            }
            l.put(d, m, r);
        }
        Op::IScAdd { d, a, b, shift, neg_a, neg_b, cc } => {
            let (u, v) = (l.gpr[a as usize], l.src(h, b));
            let sh = shift as u32 & 31;
            let po = neg_a && neg_b;
            let r = std::array::from_fn(|i| {
                let x = u[i] << sh;
                if po {
                    x.wrapping_add(v[i]).wrapping_add(1)
                } else if neg_a {
                    v[i].wrapping_sub(x)
                } else if neg_b {
                    x.wrapping_sub(v[i])
                } else {
                    x.wrapping_add(v[i])
                }
            });
            if cc {
                l.flags_i(m, r);
                let c = mask::<N>(|i| {
                    let x = (u[i] << sh) as u64;
                    let (x, n) = if neg_a && !po { (!x as u32 as u64, 1) } else { (x, 0) };
                    let y = v[i] as u64;
                    let (y, k) = if neg_b && !po { (!y as u32 as u64, 1) } else { (y, 0) };
                    x + y + (n | k | po as u64) > u32::MAX as u64
                });
                l.flags_co(m, c, 0);
            }
            l.put(d, m, r);
        }
        Op::Lea { d, a, b, hi, shift, neg_a, x, cc, pd } => {
            let (lo, v) = (l.gpr[a as usize], l.src(h, b));
            let up = hi.map(|c| l.gpr[c as usize]);
            let sh = shift as u32 & 31;
            let mut c_out = 0;
            let r = std::array::from_fn(|i| {
                let wide = match up {
                    Some(c) => (c[i] as u64) << 32 | lo[i] as u64,
                    None => lo[i] as u64,
                };
                let wide = if neg_a { wide.wrapping_neg() } else { wide };
                let s = match up {
                    Some(_) => (wide << sh >> 32) as u32,
                    None => (wide << sh) as u32,
                };
                let cin = if x { (l.cc[2] >> i & 1) as u64 } else { 0 };
                let t = s as u64 + v[i] as u64 + cin;
                c_out |= ((t >> 32 != 0) as u32) << i;
                t as u32
            });
            if cc {
                l.flags_i(m, r);
                l.flags_co(m, c_out, 0);
            }
            l.putp(pd, m, c_out);
            l.put(d, m, r);
        }
        Op::Vadd { d, a, a_bits, a_sel, a_signed, b, b_bits, b_sel, b_signed, c, op, d_signed, cc } => {
            let part = |v: u32, bits: u8, sel: u8, signed: bool| -> i64 {
                let v = v >> (8 * sel as u32);
                match (bits, signed) {
                    (8, false) => (v & 0xff) as i64,
                    (8, true) => v as u8 as i8 as i64,
                    (16, false) => (v & 0xffff) as i64,
                    (16, true) => v as u16 as i16 as i64,
                    (_, false) => v as i64,
                    (_, true) => v as i32 as i64,
                }
            };
            let (u, v, w) = (l.gpr[a as usize], l.src(h, b), l.gpr[c as usize]);
            let r = std::array::from_fn(|i| {
                let s = part(u[i], a_bits, a_sel, a_signed) + part(v[i], b_bits, b_sel, b_signed);
                let c = if d_signed { w[i] as i32 as i64 } else { w[i] as i64 };
                match op {
                    0 => w[i] & 0xffff | (s as u32) << 16,
                    1 => w[i] & 0xffff_0000 | s as u32 & 0xffff,
                    2 => w[i] & !0xff | s as u32 & 0xff,
                    3 => w[i] & !0xff_0000 | (s as u32 & 0xff) << 16,
                    4 => (s + c) as u32,
                    5 => s.min(c) as u32,
                    6 => s.max(c) as u32,
                    _ => s as u32,
                }
            });
            if cc {
                l.flags_i(m, r);
            }
            l.put(d, m, r);
        }
        Op::P2r { d, a, b, cc, byte } => {
            let (u, v) = (l.gpr[a as usize], l.src(h, b));
            let sh = 8 * (byte as u32 & 3);
            let flags = l.cc;
            let r = std::array::from_fn(|i| {
                let bits: u32 = if cc {
                    (0..4).map(|k| (flags[k] >> i & 1) << k).sum()
                } else {
                    (0..7).map(|k| (l.pred[k] >> i & 1) << k).sum()
                };
                let mask = (v[i] & 0xff) << sh;
                u[i] & !mask | (bits << sh) & mask
            });
            l.put(d, m, r);
        }
        Op::R2p { a, b, cc, byte } => {
            let (u, v) = (l.gpr[a as usize], l.src(h, b));
            let sh = 8 * (byte as u32 & 3);
            for k in 0..if cc { 4 } else { 7 } {
                let sel = mask::<N>(|i| v[i] >> k & 1 != 0) & m;
                let set = mask::<N>(|i| u[i] >> (sh + k) & 1 != 0);
                let r = if cc {
                    match k {
                        0 => &mut l.cc[0],
                        1 => &mut l.cc[1],
                        2 => &mut l.cc[2],
                        _ => &mut l.cc[3],
                    }
                } else {
                    &mut l.pred[k as usize]
                };
                *r = *r & !sel | set & sel;
            }
        }
        Op::Fchk { pd, a, b, abs_a, neg_a, abs_b, neg_b } => {
            let (u, v) = (l.gpr[a as usize], l.src(h, b));
            let _ = (abs_a, neg_a, abs_b, neg_b); // signs do not change the check
            let slow = mask::<N>(|i| {
                let (ex, ey) = ((u[i] >> 23 & 0xff) as i32, (v[i] >> 23 & 0xff) as i32);
                let x_zero = u[i] & 0x7fff_ffff == 0;
                ey == 0 || ey >= 0xfd || ex == 0xff || (ex == 0 && !x_zero) || (!x_zero && ((ex - ey) >= 126 || (ex - ey) <= -125))
            });
            l.putp(pd, m, slow);
        }
        Op::IMul { d, a, b, sa, sb, hi, cc } => {
            let (u, v) = (l.gpr[a as usize], l.src(h, b));
            let r = std::array::from_fn(|i| {
                let t = widen(u[i], sa).wrapping_mul(widen(v[i], sb));
                (if hi { t >> 32 } else { t }) as u32
            });
            if cc {
                l.flags_i(m, r);
            }
            l.put(d, m, r);
        }
        Op::IMad { .. } => imad(l, h, m, &inst.op),
        Op::Xmad { d, a, b, c, ha, hb, sa, sb, psl, mrg, mode, x, cc } => {
            let (u, v, w) = (l.gpr[a as usize], l.src(h, b), l.src(h, c));
            let half = |x: u32, hi: bool, signed: bool| {
                let x = if hi { x >> 16 } else { x & 0xffff };
                if signed {
                    x as i16 as u32
                } else {
                    x
                }
            };
            let carry = if x { l.cc[2] } else { 0 };
            let mut c_out = 0;
            let r = std::array::from_fn(|i| {
                let mut t = half(u[i], ha, sa).wrapping_mul(half(v[i], hb, sb));
                if psl {
                    t <<= 16;
                }
                let z = match mode {
                    1 => w[i] & 0xffff,
                    2 => w[i] >> 16,
                    3 => {
                        // Corrects an unsigned product into a signed one.
                        let mut z = w[i];
                        if half(u[i], ha, false) & 0x8000 != 0 {
                            z = z.wrapping_sub(v[i] << 16);
                        }
                        if half(v[i], hb, false) & 0x8000 != 0 {
                            z = z.wrapping_sub(u[i] << 16);
                        }
                        z
                    }
                    4 => w[i].wrapping_add(v[i] << 16),
                    _ => w[i],
                };
                let s = t as u64 + z as u64 + (carry >> i & 1) as u64;
                c_out |= ((s >> 32 != 0) as u32) << i;
                if mrg {
                    s as u32 & 0xffff | v[i] << 16
                } else {
                    s as u32
                }
            });
            if cc {
                l.flags_i(m, r);
                l.flags_co(m, c_out, 0);
            }
            l.put(d, m, r);
        }
        Op::IMnMx { d, a, b, signed, part, cc, p } => {
            let (u, v) = (l.gpr[a as usize], l.src(h, b));
            let sel = l.p(p);
            // The low parts of a wide minimum are unsigned and follow the high part's flags.
            let lt = cmpi(1, signed && matches!(part, 0 | 3), &u, &v);
            let eq = cmpi(2, false, &u, &v);
            let (lt, eq) = match part {
                1 | 2 => (l.cc[1] & !l.cc[0] | l.cc[0] & lt, l.cc[0] & eq),
                _ => (lt, eq),
            };
            // lanes that take a
            let take = sel & lt | !sel & !lt & !eq;
            let r = std::array::from_fn(|i| if take >> i & 1 != 0 { u[i] } else { v[i] });
            if cc {
                if part >= 2 {
                    l.flags(m, eq, lt);
                } else {
                    l.flags_i(m, r);
                }
            }
            l.put(d, m, r);
        }
        Op::ICmp { d, a, b, c, cond, signed } => {
            let z = l.src(h, c);
            let t = cmpi(cond, signed, &z, &[0; N]);
            let (x, y) = (l.gpr[a as usize], l.src(h, b));
            l.put(d, m, std::array::from_fn(|i| if t >> i & 1 != 0 { x[i] } else { y[i] }));
        }
        Op::ISet { d, cmp, bf, cc } => {
            let t = icmp(l, h, &cmp);
            let r = bool_op(cmp.bop, t, l.p(cmp.p));
            if cc {
                l.flags(m, !r, if bf { 0 } else { r });
            }
            l.put(d, m, bools(r, bf));
        }
        Op::ISetP { pd, pn, cmp } => {
            let t = icmp(l, h, &cmp);
            let q = l.p(cmp.p);
            l.putp(pd, m, bool_op(cmp.bop, t, q));
            l.putp(pn, m, bool_op(cmp.bop, !t, q));
        }
        Op::Lop { d, a, b, not_a, not_b, op, x, cc, test, pd } => {
            let (u, v) = (l.gpr[a as usize], l.src(h, b));
            let (na, nb) = (if not_a { !0 } else { 0 }, if not_b { !0 } else { 0 });
            let r: [u32; N] = match op {
                0 => std::array::from_fn(|i| (u[i] ^ na) & (v[i] ^ nb)),
                1 => std::array::from_fn(|i| (u[i] ^ na) | (v[i] ^ nb)),
                2 => std::array::from_fn(|i| u[i] ^ na ^ v[i] ^ nb),
                _ => std::array::from_fn(|i| v[i] ^ nb),
            };
            lop_flags(l, m, &r, x, cc, test, pd);
            l.put(d, m, r);
        }
        Op::Lop3 { d, a, b, c, lut, x, cc, test, pd } => {
            let (u, v, w) = (l.gpr[a as usize], l.src(h, b), l.gpr[c as usize]);
            let r: [u32; N] = std::array::from_fn(|i| {
                let mut r = 0;
                for k in 0..8 {
                    if lut >> k & 1 != 0 {
                        let t = |x: u32, bit: u32| if k >> bit & 1 != 0 { x } else { !x };
                        r |= t(u[i], 2) & t(v[i], 1) & t(w[i], 0);
                    }
                }
                r
            });
            lop_flags(l, m, &r, x, cc, test, pd);
            l.put(d, m, r);
        }
        Op::Shl { d, a, b, wrap, x, cc } => {
            let (u, v) = (l.gpr[a as usize], l.src(h, b));
            let r = std::array::from_fn(|i| shift(u[i], v[i], wrap).map_or(0, |(x, n)| x << n));
            if cc {
                l.flags_i(m, r);
                if x {
                    l.cc[0] &= !m | mask::<N>(|i| r[i] == 0);
                }
            }
            l.put(d, m, r);
        }
        Op::Shr { d, a, b, signed, wrap, rev, cc, .. } => {
            let (mut u, v) = (l.gpr[a as usize], l.src(h, b));
            if rev {
                u = u.map(u32::reverse_bits);
            }
            let r = std::array::from_fn(|i| match shift(u[i], v[i], wrap) {
                Some((x, n)) if signed => (x as i32 >> n) as u32,
                Some((x, n)) => x >> n,
                None if signed => (u[i] as i32 >> 31) as u32,
                None => 0,
            });
            if cc {
                l.flags_i(m, r);
            }
            l.put(d, m, r);
        }
        Op::Shf { d, lo, hi, b, right, high, wrap, wide, signed, cc } => {
            let (u, w, v) = (l.gpr[lo as usize], l.gpr[hi as usize], l.src(h, b));
            let bits = if wide { 64 } else { 32 };
            let r = std::array::from_fn(|i| {
                let n = if wrap { v[i] & (bits - 1) } else { v[i].min(bits) };
                let x = u[i] as u64 | (w[i] as u64) << 32;
                let y = if !right {
                    x.checked_shl(n).unwrap_or(0)
                } else if signed {
                    ((x as i64) >> n.min(63)) as u64
                } else {
                    x.checked_shr(n).unwrap_or(0)
                };
                if high || !right {
                    (y >> 32) as u32
                } else {
                    y as u32
                }
            });
            if cc {
                l.flags_i(m, r);
            }
            l.put(d, m, r);
        }
        Op::Bfe { d, a, b, signed, rev, cc } => {
            let (u, v) = (l.gpr[a as usize], l.src(h, b));
            let r = std::array::from_fn(|i| {
                let x = if rev { u[i].reverse_bits() } else { u[i] };
                bfe(x, v[i] & 0xff, v[i] >> 8 & 0xff, signed)
            });
            if cc {
                l.flags_i(m, r);
            }
            l.put(d, m, r);
        }
        Op::Bfi { d, a, b, c, cc } => {
            let (u, v, w) = (l.gpr[a as usize], l.src(h, b), l.src(h, c));
            let r = std::array::from_fn(|i| bfi(u[i], v[i] & 0xff, v[i] >> 8 & 0xff, w[i]));
            if cc {
                l.flags_i(m, r);
            }
            l.put(d, m, r);
        }
        Op::Flo { d, b, not, signed, shift, cc } => {
            let v = l.src(h, b);
            let r = v.map(|x| flo(if not { !x } else { x }, signed, shift));
            if cc {
                l.flags_i(m, r);
            }
            l.put(d, m, r);
        }
        Op::PopC { d, b, not } => {
            let v = l.src(h, b);
            l.put(d, m, v.map(|x| if not { x.count_zeros() } else { x.count_ones() }));
        }
        Op::Prmt { d, a, b, c, mode } => {
            let (u, w) = (l.gpr[a as usize], l.src(h, c));
            let r: [u32; N] = match b {
                Src::Reg(rb) => {
                    let v = l.gpr[rb as usize];
                    // (the mode is known here, so each arm is a loop of shifts)
                    match mode {
                        0 => std::array::from_fn(|i| prmt(u[i], v[i], w[i], 0)),
                        1 => std::array::from_fn(|i| prmt(u[i], v[i], w[i], 1)),
                        2 => std::array::from_fn(|i| prmt(u[i], v[i], w[i], 2)),
                        3 => std::array::from_fn(|i| prmt(u[i], v[i], w[i], 3)),
                        4 => std::array::from_fn(|i| prmt(u[i], v[i], w[i], 4)),
                        5 => std::array::from_fn(|i| prmt(u[i], v[i], w[i], 5)),
                        _ => std::array::from_fn(|i| prmt(u[i], v[i], w[i], 6)),
                    }
                }
                // the same selector in every lane: the shift of each byte, once
                _ => {
                    let sel = prmt_bytes(l.src(h, b)[0], mode);
                    if sel & 0x8888 == 0 {
                        let sh: [u32; 4] = std::array::from_fn(|k| 8 * (sel >> (4 * k) & 7));
                        std::array::from_fn(|i| {
                            let x = u[i] as u64 | (w[i] as u64) << 32;
                            (x >> sh[0]) as u32 & 0xff | ((x >> sh[1]) as u32 & 0xff) << 8 | ((x >> sh[2]) as u32 & 0xff) << 16 | ((x >> sh[3]) as u32) << 24
                        })
                    } else {
                        std::array::from_fn(|i| prmt(u[i], sel, w[i], 0))
                    }
                }
            };
            l.put(d, m, r);
        }
        Op::Sel { d, a, b, p } => {
            let t = l.p(p);
            let (x, y) = (l.gpr[a as usize], l.src(h, b));
            l.put(d, m, std::array::from_fn(|i| if t >> i & 1 != 0 { x[i] } else { y[i] }));
        }
        Op::Mov { d, b, lanes } => {
            let v = l.src(h, b);
            let m = if lanes == 15 { m } else { m & lanes as u32 * 0x1111_1111 };
            l.put(d, m, v);
        }

        Op::PSet { d, a, b, c, op1, op2, bf, cc } => {
            let t = bool_op(op1, l.p(a), l.p(b));
            let r = bool_op(op2, t, l.p(c));
            if cc {
                l.flags(m, !r, if bf { 0 } else { r });
            }
            l.put(d, m, bools(r, bf));
        }
        Op::PSetP { pd, pn, a, b, c, op1, op2 } => {
            let t = bool_op(op1, l.p(a), l.p(b));
            let q = l.p(c);
            l.putp(pd, m, bool_op(op2, t, q));
            l.putp(pn, m, bool_op(op2, !t, q));
        }

        Op::S2R { d, sys } => {
            let v = match sys {
                0x00 => std::array::from_fn(|i| i as u32),
                0x13 => bools(l.helper | l.killed, false),
                0x38 => std::array::from_fn(|i| 1 << i),
                0x39 => std::array::from_fn(|i| (1 << i) - 1),
                0x3a => std::array::from_fn(|i| (1 << i) - 1 | 1 << i),
                0x3b => std::array::from_fn(|i| !((1 << i) - 1 | 1 << i)),
                0x3c => std::array::from_fn(|i| !((1 << i) - 1)),
                _ => h.sysval(sys),
            };
            l.put(d, m, v);
        }
        Op::Vote { d, pd, p, op } => {
            let t = l.p(p) & m;
            let r = match op {
                VoteOp::All => t == m,
                VoteOp::Any => t != 0,
                VoteOp::Eq => t == m || t == 0,
            };
            l.put(d, m, [t; N]);
            l.putp(pd, m, if r { !0 } else { 0 });
        }
        Op::Shfl { d, pd, a, b, c, op } => {
            let (u, v, w) = (l.gpr[a as usize], l.src(h, b), l.src(h, c));
            let mut ok = 0;
            let r = std::array::from_fn(|i| {
                let (lane, n) = (i as u32, v[i] & 31);
                let (last, seg) = (w[i] & 31, w[i] >> 8 & 31);
                let min = lane & seg;
                let max = min | last & !seg;
                let (j, in_range) = match op {
                    ShflOp::Idx => {
                        let j = min | n & !seg & 31;
                        (j, j <= max)
                    }
                    ShflOp::Up => {
                        let j = lane.wrapping_sub(n);
                        (j, j as i32 >= min as i32)
                    }
                    ShflOp::Down => (lane + n, lane + n <= max),
                    ShflOp::Bfly => (lane ^ n, lane ^ n <= max),
                };
                if in_range {
                    ok |= 1 << i;
                    if (j as usize) < N {
                        u[j as usize]
                    } else {
                        0
                    }
                } else {
                    u[i]
                }
            });
            l.put(d, m, r);
            l.putp(pd, m, ok);
        }

        Op::Ldc { .. }
        | Op::Ld { .. }
        | Op::St { .. }
        | Op::Ldl { .. }
        | Op::Stl { .. }
        | Op::Lds { .. }
        | Op::Sts { .. }
        | Op::Ldgsts { .. }
        | Op::Atom { .. }
        | Op::AtomS { .. } => {
            // which instruction a memory fault came from, for the GPU's fault reports
            LAST_MEMORY_PC.store(p.words.get(pc as usize).map_or(0, |w| w.0), std::sync::atomic::Ordering::Relaxed);
            memory(l, h, m, &inst.op)
        }

        Op::Ald { d, a, v, addr, n, patch, output } => {
            let (base, vtx) = (l.gpr[a as usize], l.gpr[v as usize]);
            for i in lanes(m) {
                for c in 0..n as u32 {
                    let at = Attr {
                        addr: base[i].wrapping_add(addr as u32 + 4 * c),
                        vertex: vtx[i],
                        patch,
                        output,
                    };
                    let x = h.attr_load(i, at);
                    l.put1(reg_at(d, c as usize), i, x);
                }
            }
        }
        Op::Ast { r, a, v, addr, n, patch } => {
            let (base, vtx) = (l.gpr[a as usize], l.gpr[v as usize]);
            for i in lanes(m) {
                for c in 0..n as u32 {
                    let at = Attr {
                        addr: base[i].wrapping_add(addr as u32 + 4 * c),
                        vertex: vtx[i],
                        patch,
                        output: true,
                    };
                    h.attr_store(i, at, l.gpr[reg_at(r, c as usize) as usize][i]);
                }
            }
        }
        Op::Al2p { d, a, addr, n, .. } => {
            // Physical attribute addresses are the logical ones here.
            let base = l.gpr[a as usize];
            for c in 0..n as u32 {
                l.put(
                    reg_at(d, c as usize),
                    m,
                    base.map(|x| x.wrapping_add(addr as u32 + 4 * c)),
                );
            }
        }
        Op::Isberd { d, a } => {
            let x = l.gpr[a as usize];
            for i in lanes(m) {
                let v = h.isberd(i, x[i]);
                l.put1(d, i, v);
            }
        }
        Op::Ipa { d, addr, idx, mul, offset, mode, loc, sat } => {
            let off = if loc == IpaLoc::Offset {
                let o = l.gpr[offset as usize];
                [
                    // 12 bits each; the bits above may hold something else
                    o.map(|x| ((x << 20) as i32 >> 20) as f32 / 4096.0),
                    o.map(|x| ((x << 4) as i32 >> 20) as f32 / 4096.0),
                ]
            } else {
                [[0.0; N]; 2]
            };
            let mut op = IpaOp { addr: addr as u32, mode, loc, offset: off };
            let mut v = match idx {
                None => h.interpolate(&op),
                Some(r) => {
                    let base = l.gpr[r as usize];
                    let mut v = [0; N];
                    let mut left = m;
                    while left != 0 {
                        let a = base[left.trailing_zeros() as usize];
                        op.addr = a.wrapping_add(addr as u32);
                        let x = h.interpolate(&op);
                        for i in lanes(left) {
                            if base[i] == a {
                                v[i] = x[i];
                                left &= !(1 << i);
                            }
                        }
                    }
                    v
                }
            };
            // Mesa asks for gl_FragCoord with .MulW too; window coordinates stay as they are.
            let window = idx.is_none() && (0x60..0x80).contains(&addr);
            if mul != RZ && !window && matches!(mode, IpaMode::MulW | IpaMode::State) {
                let w = l.gpr[mul as usize];
                v = std::array::from_fn(|i| {
                    (f32::from_bits(v[i]) * f32::from_bits(w[i])).to_bits()
                });
            }
            if sat {
                v = v.map(|x| fp::sat(f32::from_bits(x)).to_bits());
            }
            l.put(d, m, v);
        }
        Op::Pixld { d, a, what } => {
            let v = h.pixld(what, &l.gpr[a as usize]);
            l.put(d, m, v);
        }
        Op::Out { d, a, b, emit, cut } => {
            let (handle, stream) = (l.gpr[a as usize], l.src(h, b));
            for i in lanes(m) {
                let v = h.emit(i, handle[i], stream[i], emit, cut);
                l.put1(d, i, v);
            }
        }

        Op::Tex(ref t) => texture(l, h, m, t),
        Op::Suld(_) | Op::Sust(_) | Op::Suatom { .. } => surface(l, h, m, &inst.op),

        Op::Exit { cond } => {
            let m = m & l.test(cond);
            l.exited |= m;
            l.active &= !m;
            if !l.unfinished() || !l.unwind() {
                l.active = 0;
                return Some(Exit::Done);
            }
        }
        Op::Kil { cond } => {
            l.killed |= m & l.test(cond);
            if !l.unfinished() {
                l.active = 0;
                return Some(Exit::Done);
            }
        }
        Op::Bra { t, cond, all } => {
            let m = m & l.test(cond);
            let rest = l.active & !m;
            if rest == 0 {
                l.pc = t;
            } else if m != 0 && !all {
                if l.stack.len() >= MAX_DEPTH {
                    return Some(fault(p, pc));
                }
                l.stack.push(Token { kind: Kind::Div, pc: l.pc, lanes: rest });
                l.active = m;
                l.pc = t;
            }
        }
        Op::Brx { a, base, cond } => {
            let m = m & l.test(cond);
            let rest = l.active & !m;
            if rest != 0 && m != 0 {
                l.stack.push(Token { kind: Kind::Div, pc: l.pc, lanes: rest });
            }
            let reg = l.gpr[a as usize];
            let mut left = m;
            while left != 0 {
                let x = reg[left.trailing_zeros() as usize];
                let same = left & mask::<N>(|i| reg[i] == x);
                let Some(t) = p.index_of(base.wrapping_add(x)) else {
                    return Some(fault(p, pc));
                };
                left &= !same;
                if left == 0 {
                    l.active = same;
                    l.pc = t;
                } else {
                    l.stack.push(Token { kind: Kind::Div, pc: t, lanes: same });
                }
            }
            if l.stack.len() >= MAX_DEPTH {
                return Some(fault(p, pc));
            }
        }
        Op::Cal { t } => {
            if l.stack.len() >= MAX_DEPTH {
                return Some(fault(p, pc));
            }
            l.stack.push(Token { kind: Kind::Ret, pc: l.pc, lanes: 0 });
            l.pc = t;
        }
        Op::Pret { t } | Op::Ssy { t } | Op::Pbk { t } | Op::Pcnt { t } => {
            let kind = match inst.op {
                Op::Pret { .. } => Kind::Ret,
                Op::Ssy { .. } => Kind::Ssy,
                Op::Pbk { .. } => Kind::Pbk,
                _ => Kind::Pcnt,
            };
            if l.stack.len() >= MAX_DEPTH {
                return Some(fault(p, pc));
            }
            l.stack.push(Token { kind, pc: t, lanes: 0 });
        }
        Op::Sync { cond } | Op::Brk { cond } | Op::Cont { cond } | Op::Ret { cond } => {
            let kind = match inst.op {
                Op::Sync { .. } => Kind::Ssy,
                Op::Brk { .. } => Kind::Pbk,
                Op::Cont { .. } => Kind::Pcnt,
                _ => Kind::Ret,
            };
            let m = m & l.test(cond);
            if m == 0 {
                return None;
            }
            if !l.park(kind, m) {
                if kind != Kind::Ret {
                    return Some(fault(p, pc));
                }
                // A return from the main program ends it.
                l.exited |= m;
                l.active &= !m;
                if !l.unfinished() {
                    l.active = 0;
                    return Some(Exit::Done);
                }
            }
            if l.stack.len() >= MAX_DEPTH {
                return Some(fault(p, pc));
            }
            if !l.unwind() {
                return Some(Exit::Done);
            }
        }
        Op::Sam => {
            if l.sam.len() >= MAX_DEPTH {
                return Some(fault(p, pc));
            }
            l.sam.push(l.active);
            for q in 0..N / 4 {
                if l.active >> (4 * q) & 15 != 0 {
                    l.active |= 15 << (4 * q);
                }
            }
        }
        Op::Ram => {
            if let Some(a) = l.sam.pop() {
                l.active = a;
            }
        }
        Op::Bar => return Some(Exit::Barrier),
    }
    None
}

/// The lane numbers in a mask.
#[inline(always)]
fn lanes(m: u32) -> impl Iterator<Item = usize> {
    let mut m = m;
    std::iter::from_fn(move || {
        if m == 0 {
            return None;
        }
        let i = m.trailing_zeros();
        m &= m - 1;
        Some(i as usize)
    })
}

#[inline(always)]
fn widen(x: u32, signed: bool) -> i64 {
    if signed {
        x as i32 as i64
    } else {
        x as i64
    }
}

#[inline(always)]
fn fcmp<const N: usize, H: Host<N>>(l: &Lanes<N>, h: &H, c: &FCmpArgs) -> u32 {
    let a = modf(l.gpr[c.a as usize], c.am, c.ftz);
    let b = l.srcf(h, c.b, c.bm, c.ftz);
    cmp32(c.cond, &a, &b)
}

#[inline(always)]
fn icmp<const N: usize, H: Host<N>>(l: &Lanes<N>, h: &H, c: &ICmpArgs) -> u32 {
    let a = &l.gpr[c.a as usize];
    let b = l.src(h, c.b);
    if let Some(low) = c.ex {
        // SM70: the high halves decide, unless they are equal: then the low halves' result does
        let (lt, eq, gt, low) = (cmpi(1, c.signed, a, &b), cmpi(2, c.signed, a, &b), cmpi(4, c.signed, a, &b), l.p(low));
        return match c.cond & 7 {
            0 => 0,
            1 | 3 => lt | eq & low,
            2 => eq & low,
            4 | 6 => gt | eq & low,
            5 => !eq | low,
            _ => !0,
        };
    }
    if !c.x {
        return cmpi(c.cond, c.signed, a, &b);
    }
    // The high halves decide, unless they are equal: then the low halves do, whose subtraction
    // left "no borrow" in the carry flag and "equal" in the zero flag.
    let lt = cmpi(1, c.signed, a, &b);
    let eq = cmpi(2, c.signed, a, &b);
    let wide_lt = lt | eq & !l.cc[2];
    let wide_eq = eq & l.cc[0];
    let mut r = 0;
    if c.cond & 1 != 0 {
        r |= wide_lt;
    }
    if c.cond & 2 != 0 {
        r |= wide_eq;
    }
    if c.cond & 4 != 0 {
        r |= !wide_lt & !wide_eq;
    }
    r
}

fn lop_flags<const N: usize>(
    l: &mut Lanes<N>,
    m: u32,
    r: &[u32; N],
    x: bool,
    cc: bool,
    test: u8,
    pd: u8,
) {
    if pd != 7 {
        let z = mask::<N>(|i| r[i] == 0);
        let v = match test {
            0 => 0,
            1 => !0,
            2 => z,
            _ => !z,
        };
        l.putp(pd, m, v);
    }
    if cc {
        // .X: the zero flag covers a wider value.
        let z = mask::<N>(|i| r[i] == 0) & if x { l.cc[0] } else { !0 };
        l.flags(m, z, mask::<N>(|i| (r[i] as i32) < 0));
    }
}

#[inline(never)]
fn mufu<const N: usize>(x: [u32; N], md: Mod, op: Mufu) -> [u32; N] {
    let v = x.map(|x| f32::from_bits(x & md.and() ^ md.xor()));
    let wide = |x: f32| f64::from_bits((x.to_bits() as u64) << 32);
    let f = |r: [f32; N]| canon(r).map(f32::to_bits);
    match op {
        Mufu::Cos => f(v.map(f32::cos)),
        Mufu::Sin => f(v.map(f32::sin)),
        Mufu::Ex2 => f(v.map(f32::exp2)),
        Mufu::Lg2 => f(v.map(f32::log2)),
        Mufu::Rcp => f(v.map(|x| 1.0 / x)),
        Mufu::Rsq => f(v.map(|x| (1.0 / (x as f64).sqrt()) as f32)),
        Mufu::Sqrt => f(v.map(f32::sqrt)),
        // (the high halves of doubles: left as the host makes them; the JIT computes them with
        // the same code, so both agree, though a NaN's sign may differ between hosts)
        Mufu::Rcp64h => v.map(|x| ((1.0 / wide(x)).to_bits() >> 32) as u32),
        Mufu::Rsq64h => v.map(|x| ((1.0 / wide(x).sqrt()).to_bits() >> 32) as u32),
    }
}

#[inline(never)]
fn imad<const N: usize, H: Host<N>>(l: &mut Lanes<N>, h: &H, m: u32, op: &Op) {
    let Op::IMad { d, a, b, c, sa, sb, hi, neg_ab, neg_c, sat, x, cc } = *op else {
        return;
    };
    let (u, v, w) = (l.gpr[a as usize], l.src(h, b), l.src(h, c));
    let carry = if x { l.cc[2] } else { 0 };
    let po = neg_ab && neg_c;
    let mut c_out = 0;
    let mut o_out = 0;
    let r = std::array::from_fn(|i| {
        let mut t = widen(u[i], sa).wrapping_mul(widen(v[i], sb));
        if neg_ab && !po {
            t = t.wrapping_neg();
        }
        let p = (if hi { t >> 32 } else { t }) as u32;
        let (z, n) = if neg_c && !po { (!w[i], 1) } else { (w[i], po as u64) };
        // With .X the negation is a complement.
        let cin = if x { (carry >> i & 1) as u64 } else { n };
        let s = p as u64 + z as u64 + cin;
        c_out |= ((s >> 32 != 0) as u32) << i;
        let y = s as u32;
        let o = (p ^ y) & (z ^ y) & 0x8000_0000 != 0;
        o_out |= (o as u32) << i;
        if sat && o {
            if p >> 31 != 0 {
                0x8000_0000
            } else {
                0x7fff_ffff
            }
        } else {
            y
        }
    });
    if cc {
        l.flags_i(m, r);
        l.flags_co(m, c_out, o_out);
    }
    l.put(d, m, r);
}

#[inline(never)]
fn double<const N: usize, H: Host<N>>(l: &mut Lanes<N>, h: &H, m: u32, op: &Op) {
    match *op {
        Op::DAdd { d, a, am, b, bm, cc, rnd } => {
            let (x, y) = (l.srcd(h, Src::Reg(a), am), l.srcd(h, b, bm));
            let v = std::array::from_fn(|i| dadd(x[i], y[i], rnd));
            if cc {
                l.flags_d(m, v);
            }
            l.putd(d, m, v);
        }
        Op::DMul { d, a, b, neg, cc, rnd } => {
            let x = l.srcd(h, Src::Reg(a), Mod { abs: false, neg });
            let y = l.srcd(h, b, Mod::default());
            let v = std::array::from_fn(|i| {
                let r = x[i] * y[i];
                directed(r, || x[i].mul_add(y[i], -r), rnd)
            });
            if cc {
                l.flags_d(m, v);
            }
            l.putd(d, m, v);
        }
        Op::Dfma { d, a, b, c, neg_ab, neg_c, cc, .. } => {
            let x = l.srcd(h, Src::Reg(a), Mod { abs: false, neg: neg_ab });
            let y = l.srcd(h, b, Mod::default());
            let z = l.srcd(h, c, Mod { abs: false, neg: neg_c });
            let v = std::array::from_fn(|i| x[i].mul_add(y[i], z[i]));
            if cc {
                l.flags_d(m, v);
            }
            l.putd(d, m, v);
        }
        Op::DMnMx { d, a, am, b, bm, cc, p } => {
            let (x, y) = (l.srcd(h, Src::Reg(a), am), l.srcd(h, b, bm));
            let sel = l.p(p);
            let v = std::array::from_fn(|i| {
                if sel >> i & 1 != 0 {
                    fp::min64(x[i], y[i])
                } else {
                    fp::max64(x[i], y[i])
                }
            });
            if cc {
                l.flags_d(m, v);
            }
            l.putd(d, m, v);
        }
        Op::DSet { d, cmp, bf, cc } => {
            let r = bool_op(cmp.bop, dcmp(l, h, &cmp), l.p(cmp.p));
            if cc {
                l.flags(m, !r, if bf { 0 } else { r });
            }
            l.put(d, m, bools(r, bf));
        }
        Op::DSetP { pd, pn, cmp } => {
            let t = dcmp(l, h, &cmp);
            let q = l.p(cmp.p);
            l.putp(pd, m, bool_op(cmp.bop, t, q));
            l.putp(pn, m, bool_op(cmp.bop, !t, q));
        }
        _ => {}
    }
}

fn dcmp<const N: usize, H: Host<N>>(l: &Lanes<N>, h: &H, c: &FCmpArgs) -> u32 {
    let a = l.srcd(h, Src::Reg(c.a), c.am);
    let b = l.srcd(h, c.b, c.bm);
    mask::<N>(|i| fp::cmp(c.cond, a[i], b[i]) != 0)
}

fn dadd(a: f64, b: f64, rnd: Rnd) -> f64 {
    let s = a + b;
    directed(
        s,
        || {
            let t = s - a;
            (a - (s - t)) + (b - t)
        },
        rnd,
    )
}

/// Moves the nearest result `r` one step if the rounding mode asks for it; `err` is the exact
/// value minus `r`.
fn directed(r: f64, err: impl Fn() -> f64, rnd: Rnd) -> f64 {
    if rnd == Rnd::N || !r.is_finite() {
        return r;
    }
    let e = err();
    match rnd {
        Rnd::M if e < 0.0 => r.next_down(),
        Rnd::P if e > 0.0 => r.next_up(),
        Rnd::Z if r > 0.0 && e < 0.0 => r.next_down(),
        Rnd::Z if r < 0.0 && e > 0.0 => r.next_up(),
        _ => r,
    }
}

#[inline(never)]
fn convert<const N: usize, H: Host<N>>(l: &mut Lanes<N>, h: &H, m: u32, op: &Op) {
    // Float operands as doubles (every half and single is one).
    let float = |l: &Lanes<N>, b: Src, md: Mod, from: u8, high: bool, ftz: bool| -> [f64; N] {
        match from {
            2 => l.src(h, b).map(|x| {
                let x = fp::half_to_f32((if high { x >> 16 } else { x }) as u16);
                let x = x.to_bits() & md.and() ^ md.xor();
                f32::from_bits(x) as f64
            }),
            4 => l.srcf(h, b, md, ftz).map(|x| x as f64),
            _ => l.srcd(h, b, md),
        }
    };
    match *op {
        Op::F2F { d, b, m: md, from, to, rnd, int, high, ftz, sat, cc } => {
            let mut v = float(l, b, md, from, high && from == 2, ftz);
            let mut out_rnd = rnd;
            if int {
                v = v.map(|x| fp::round_int(x, rnd));
                out_rnd = Rnd::N;
            }
            if sat {
                v = v.map(|x| if x > 0.0 { x.min(1.0) } else { 0.0 });
            }
            if cc {
                l.flags_d(m, v);
            }
            match to {
                2 => {
                    let half = |x: f64| {
                        // to odd first, so that the second rounding is right
                        let y = fp::to_f32(x, Rnd::Z);
                        let y = if y as f64 != x && y.to_bits() & 1 == 0 && y.is_finite() {
                            f32::from_bits(y.to_bits() | 1)
                        } else {
                            y
                        };
                        (fp::f32_to_half(y, out_rnd) as u32) << if high { 16 } else { 0 }
                    };
                    l.put(d, m, v.map(half));
                }
                4 => {
                    let r = finish(v.map(|x| fp::to_f32(x, out_rnd)), ftz, false);
                    l.putf(d, m, r);
                }
                _ => l.putd(d, m, v),
            }
        }
        Op::F2I { d, b, m: md, from, to, rnd, high, ftz, cc } => {
            let v = float(l, b, md, from, high && from == 2, ftz);
            let (lo, hi) = int_range(to);
            // `as` clamps, and NaN becomes 0.
            let r = v.map(|x| int_out((fp::round_int(x, rnd) as i128).clamp(lo, hi), to));
            let w = r.map(|x| x as u32);
            if cc {
                l.flags_i(m, w);
            }
            l.put(d, m, w);
            if to.bytes == 8 {
                l.put(reg2(d), m, r.map(|x| (x >> 32) as u32));
            }
        }
        Op::I2F { d, b, m: md, from, to, sel, rnd, cc } => {
            let v = int_src(l, h, b, md, from, sel);
            let f: [f64; N] = v.map(|x| x as f64);
            if cc {
                l.flags_d(m, f);
            }
            match to {
                2 => l.put(d, m, f.map(|x| fp::f32_to_half(x as f32, rnd) as u32)),
                4 if rnd == Rnd::N => l.putf(d, m, v.map(|x| x as f32)),
                4 => l.putf(d, m, f.map(|x| fp::to_f32(x, rnd))),
                _ => l.putd(d, m, f),
            }
        }
        Op::I2I { d, b, m: md, from, to, sel, sat, cc } => {
            let mut v = int_src(l, h, b, md, from, sel);
            if sat {
                let (lo, hi) = int_range(to);
                v = v.map(|x| x.clamp(lo, hi));
            }
            let r = v.map(|x| int_out(x, to));
            let w = r.map(|x| x as u32);
            if cc {
                l.flags_i(m, w);
            }
            l.put(d, m, w);
            if to.bytes == 8 {
                l.put(reg2(d), m, r.map(|x| (x >> 32) as u32));
            }
        }
        _ => {}
    }
}

fn int_src<const N: usize, H: Host<N>>(
    l: &Lanes<N>,
    h: &H,
    b: Src,
    md: Mod,
    from: IntTy,
    sel: u8,
) -> [i128; N] {
    let (lo, hi) = if from.bytes == 8 {
        let v = match b {
            // A 64-bit integer immediate is sign-extended.
            Src::Imm(x) => [x as i32 as u64; N],
            _ => l.src64(h, b),
        };
        (v.map(|x| x as u32), v.map(|x| (x >> 32) as u32))
    } else {
        (l.src(h, b), [0; N])
    };
    std::array::from_fn(|i| {
        let x = int_in(lo[i], hi[i], from, sel);
        let x = if md.abs { x.abs() } else { x };
        if md.neg {
            -x
        } else {
            x
        }
    })
}

#[inline(never)]
fn memory<const N: usize, H: Host<N>>(l: &mut Lanes<N>, h: &mut H, m: u32, op: &Op) {
    let quiet = l.helper | l.killed;
    let global = |l: &Lanes<N>, a: Reg, off: i32, wide: bool, i: usize| -> u64 {
        let lo = l.gpr[a as usize][i];
        if wide {
            let hi = l.gpr[reg2(a) as usize][i];
            (lo as u64 | (hi as u64) << 32).wrapping_add(off as i64 as u64)
        } else {
            lo.wrapping_add(off as u32) as u64
        }
    };
    let near =
        |l: &Lanes<N>, a: Reg, off: i32, i: usize| l.gpr[a as usize][i].wrapping_add(off as u32);
    // Writes a loaded value to its registers.
    let load = |l: &mut Lanes<N>, d: Reg, ty: MemTy, i: usize, buf: &[u8; 16]| {
        if ty.bytes() <= 4 {
            l.put1(d, i, extend(buf, ty));
        } else {
            for k in 0..ty.bytes() / 4 {
                l.put1(reg_at(d, k), i, extend(&buf[4 * k..], MemTy::B32));
            }
        }
    };
    let data = |l: &Lanes<N>, r: Reg, ty: MemTy, i: usize| -> [u8; 16] {
        let mut buf = [0; 16];
        for k in 0..ty.bytes().div_ceil(4) {
            let v = l.gpr[reg_at(r, k) as usize][i];
            buf[4 * k..4 * k + 4].copy_from_slice(&v.to_le_bytes());
        }
        buf
    };
    let operand = |l: &Lanes<N>, r: Reg, ty: AtomTy, i: usize| -> u64 {
        let lo = l.gpr[r as usize][i] as u64;
        if ty.bytes() == 8 {
            lo | (l.gpr[reg2(r) as usize][i] as u64) << 32
        } else {
            lo
        }
    };

    match *op {
        Op::Ldc { d, idx, a, off, ty, mode } => {
            let base = l.gpr[a as usize];
            for i in lanes(m) {
                // Indexed-segmented: the upper half of the register selects the buffer.
                let (seg, o) = if mode >= 2 {
                    (base[i] >> 16, base[i] & 0xffff)
                } else {
                    (0, base[i])
                };
                let idx = idx as u32 + seg;
                let o = o.wrapping_add(off as u32) & if mode >= 2 { 0xffff } else { u32::MAX };
                match ty {
                    MemTy::B64 | MemTy::B128 => {
                        for k in 0..ty.bytes() / 4 {
                            let v = h.cbuf(idx, o.wrapping_add(4 * k as u32));
                            l.put1(reg_at(d, k), i, v);
                        }
                    }
                    MemTy::B32 => {
                        let v = h.cbuf(idx, o & !3);
                        l.put1(d, i, v);
                    }
                    _ => {
                        let w = h.cbuf(idx, o & !3) >> (8 * (o & 3));
                        l.put1(d, i, extend(&w.to_le_bytes(), ty));
                    }
                }
            }
        }
        Op::Ld { d, a, off, ty, wide } => {
            // the warp's addresses, then one call for all of them
            let mut at = [0u64; N];
            for (i, x) in at.iter_mut().enumerate() {
                *x = global(l, a, off, wide, i);
            }
            let mut buf = [[0u8; 16]; N];
            h.load_lanes(&at, m, ty.bytes(), &mut buf);
            for i in lanes(m) {
                load(l, d, ty, i, &buf[i]);
            }
        }
        Op::St { r, a, off, ty, wide } => {
            let m = m & !quiet;
            let mut at = [0u64; N];
            for (i, x) in at.iter_mut().enumerate() {
                *x = global(l, a, off, wide, i);
            }
            let buf: [[u8; 16]; N] = std::array::from_fn(|i| if m >> i & 1 != 0 { data(l, r, ty, i) } else { [0; 16] });
            h.store_lanes(&at, m, ty.bytes(), &buf);
        }
        Op::Ldl { d, a, off, ty } => {
            for i in lanes(m) {
                let at = near(l, a, off, i) as usize;
                let n = ty.bytes();
                let mut buf = [0; 16];
                if at.checked_add(n).is_some_and(|e| e <= l.local_size) {
                    let at = i * l.local_size + at;
                    buf[..n].copy_from_slice(&l.local[at..at + n]);
                }
                load(l, d, ty, i, &buf);
            }
        }
        Op::Stl { r, a, off, ty } => {
            for i in lanes(m) {
                let at = near(l, a, off, i) as usize;
                let n = ty.bytes();
                if at.checked_add(n).is_some_and(|e| e <= l.local_size) {
                    let buf = data(l, r, ty, i);
                    let at = i * l.local_size + at;
                    l.local[at..at + n].copy_from_slice(&buf[..n]);
                }
            }
        }
        Op::Lds { d, a, off, ty } => {
            for i in lanes(m) {
                let at = near(l, a, off, i) as usize;
                let n = ty.bytes();
                let mut buf = [0; 16];
                if let Some(s) = at.checked_add(n).and_then(|e| h.shared().get(at..e)) {
                    buf[..n].copy_from_slice(s);
                }
                load(l, d, ty, i, &buf);
            }
        }
        Op::Sts { r, a, off, ty } => {
            for i in lanes(m & !quiet) {
                let at = near(l, a, off, i) as usize;
                let n = ty.bytes();
                let buf = data(l, r, ty, i);
                if let Some(s) = at.checked_add(n).and_then(|e| h.shared().get_mut(at..e)) {
                    s.copy_from_slice(&buf[..n]);
                }
            }
        }
        Op::Ldgsts { s, soff, g, goff, ty, p } => {
            let valid = l.p(p);
            for i in lanes(m & !quiet) {
                let n = ty.bytes();
                let mut buf = [0; 16];
                if valid >> i & 1 != 0 {
                    h.load(global(l, g, goff, true, i), &mut buf[..n]);
                }
                let at = near(l, s, soff, i) as usize;
                if let Some(s) = at.checked_add(n).and_then(|e| h.shared().get_mut(at..e)) {
                    s.copy_from_slice(&buf[..n]);
                }
            }
        }
        Op::Atom { d, a, b, off, op, ty, wide } => {
            for i in lanes(m) {
                let mut old = 0;
                if quiet >> i & 1 == 0 {
                    let x = operand(l, b, ty, i);
                    let y = operand(l, reg_at(b, ty.bytes() / 4), ty, i);
                    old = h.atomic(global(l, a, off, wide, i), op, ty, x, y);
                }
                l.put1(d, i, old as u32);
                if ty.bytes() == 8 {
                    l.put1(reg2(d), i, (old >> 32) as u32);
                }
            }
        }
        Op::AtomS { d, a, b, off, op, ty } => {
            for i in lanes(m) {
                let at = near(l, a, off, i) as usize;
                let n = ty.bytes();
                let x = operand(l, b, ty, i);
                let y = operand(l, reg_at(b, n / 4), ty, i);
                let mut buf = [0; 8];
                if let Some(s) = at.checked_add(n).and_then(|e| h.shared().get_mut(at..e)) {
                    buf[..n].copy_from_slice(s);
                    if quiet >> i & 1 == 0 {
                        let old = u64::from_le_bytes(buf);
                        let new = crate::host::atomic_apply(op, ty, old, x, y);
                        s.copy_from_slice(&new.to_le_bytes()[..n]);
                    }
                }
                let old = u64::from_le_bytes(buf);
                l.put1(d, i, old as u32);
                if n == 8 {
                    l.put1(reg2(d), i, (old >> 32) as u32);
                }
            }
        }
        _ => {}
    }
}

#[inline(never)]
fn texture<const N: usize, H: Host<N>>(l: &mut Lanes<N>, h: &mut H, m: u32, t: &Tex) {
    let g = |r: Reg| l.gpr[r as usize];
    let gf = |r: Reg| l.gpr[r as usize].map(f32::from_bits);
    let offs = g(t.offset);
    let (bits, first) = match t.offset_kind {
        OffsetKind::Nibbles => (4, 0),
        OffsetKind::NibblesHigh => (4, 16),
        OffsetKind::Bytes => (8, 0),
        _ => (0, 0),
    };
    let offset = std::array::from_fn(|c| {
        if bits == 0 {
            [0; N]
        } else {
            let sh = first + bits * c as u32;
            offs.map(|x| ((x >> sh) << (32 - bits)) as i32 >> (32 - bits))
        }
    });
    let grad = t.kind == TexKind::Grad;
    let deriv = |n: usize| if grad { gf(reg_at(t.deriv, n)) } else { [0.0; N] };
    let op = TexOp {
        kind: t.kind,
        dim: t.dim,
        array: t.array,
        handle: if t.bindless { Handle::Bindless(g(t.handle)) } else { Handle::Bound(t.index as u32) },
        coord: t.coord.map(g),
        layer: if t.array { g(t.layer).map(|x| x & 0xffff) } else { [0; N] },
        lod_mode: t.lod_mode,
        lod: g(t.lod),
        shadow: t.shadow,
        dref: gf(t.dref),
        ms: t.ms,
        sample: g(t.sample),
        offset,
        per_texel: (t.offset_kind == OffsetKind::PerTexel)
            .then(|| [g(t.offset), g(reg_at(t.offset, 1))]),
        ddx: [deriv(0), deriv(2)],
        ddy: [deriv(1), deriv(3)],
        comp: t.comp,
        query: if t.kind == TexKind::Query { t.comp } else { 0 },
        mask: t.mask,
        lanes: m,
        ndv: t.ndv,
    };
    let mut r = h.texture(&op);
    if t.kind == TexKind::QueryLod {
        // 8.8 fixed point: signed before clamping, unsigned after.
        let fixed = |x: u32| f32::from_bits(x) as f64 * 256.0;
        r[0] = r[0].map(|x| fixed(x).clamp(-32768.0, 32767.0) as i32 as u32 & 0xffff);
        r[1] = r[1].map(|x| fixed(x).clamp(0.0, 65535.0) as u32);
    }
    if t.f16 {
        // two components to a register: the first pair in d0, the second in d1
        let wanted: Vec<[u32; N]> = (0..4).filter(|c| t.mask >> c & 1 != 0).map(|c| r[c]).collect();
        let half = |v: u32| fp::f32_to_half(f32::from_bits(v), Rnd::N) as u32;
        for (k, pair) in wanted.chunks(2).enumerate() {
            let v = std::array::from_fn(|i| half(pair[0][i]) | pair.get(1).map_or(0, |p| half(p[i]) << 16));
            l.put(if k == 0 { t.d0 } else { t.d1 }, m, v);
        }
        return;
    }
    let mut n = 0;
    for (c, v) in r.iter().enumerate() {
        if t.mask >> c & 1 != 0 {
            let d = match (t.scalar, n) {
                (true, 2..) => reg_at(t.d1, n - 2),
                _ => reg_at(t.d0, n),
            };
            l.put(d, m, *v);
            n += 1;
        }
    }
}

#[inline(never)]
fn surface<const N: usize, H: Host<N>>(l: &mut Lanes<N>, h: &mut H, m: u32, op: &Op) {
    let s = match op {
        Op::Suld(s) | Op::Sust(s) | Op::Suatom { s, .. } => s,
        _ => return,
    };
    let quiet = l.helper | l.killed;
    let handle = match s.handle {
        Src::Imm(v) => Handle::Bound(v),
        Src::Reg(r) => Handle::Bindless(l.gpr[r as usize]),
        Src::Cb(..) => return,
    };
    let n = match s.dim {
        SurfDim::D1 | SurfDim::Buffer => 1,
        SurfDim::D1Array | SurfDim::D2 => 2,
        SurfDim::D2Array | SurfDim::D3 => 3,
    };
    let coord =
        std::array::from_fn(|c| if c < n { l.gpr[reg_at(s.coord, c) as usize] } else { [0; N] });
    let so = SurfOp { dim: s.dim, handle, coord, raw: s.raw, ty: s.ty, mask: s.mask, lanes: m };
    match *op {
        Op::Suld(s) => {
            let mut v = h.surface_load(&so);
            if s.raw && s.ty.bytes() < 4 {
                v[0] = v[0].map(|x| extend(&x.to_le_bytes(), s.ty));
            }
            if s.raw {
                for k in 0..s.ty.bytes().div_ceil(4) {
                    l.put(reg_at(s.r, k), m, v[k]);
                }
            } else {
                let mut k = 0;
                for (c, x) in v.iter().enumerate() {
                    if s.mask >> c & 1 != 0 {
                        l.put(reg_at(s.r, k), m, *x);
                        k += 1;
                    }
                }
            }
        }
        Op::Sust(s) => {
            let data = std::array::from_fn(|c| l.gpr[reg_at(s.r, c) as usize]);
            let so = SurfOp { lanes: m & !quiet, ..so };
            if so.lanes != 0 {
                h.surface_store(&so, &data);
            }
        }
        Op::Suatom { s, b, op, ty } => {
            let so = SurfOp { lanes: m & !quiet, ..so };
            let wide = ty.bytes() == 8;
            let val = |l: &Lanes<N>, r: Reg| -> [u64; N] {
                let (lo, hi) = (l.gpr[r as usize], l.gpr[reg2(r) as usize]);
                std::array::from_fn(|i| lo[i] as u64 | if wide { (hi[i] as u64) << 32 } else { 0 })
            };
            let x = val(l, b);
            let y = val(l, reg_at(b, ty.bytes() / 4));
            let old =
                if so.lanes != 0 { h.surface_atomic(&so, op, ty, &x, &y) } else { [0; N] };
            let old: [u64; N] = std::array::from_fn(|i| if quiet >> i & 1 != 0 { 0 } else { old[i] });
            l.put(s.r, m, old.map(|x| x as u32));
            if wide {
                l.put(reg2(s.r), m, old.map(|x| (x >> 32) as u32));
            }
        }
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::prmt;

    /// PRMT byte by byte, as the PTX manual describes it.
    pub(super) fn reference(a: u32, sel: u32, c: u32, mode: u8) -> u32 {
        let src = (a as u64 | (c as u64) << 32).to_le_bytes();
        let s = (sel & 3) as usize;
        let r: [u8; 4] = match mode {
            0 => std::array::from_fn(|i| {
                let n = sel >> (4 * i) & 15;
                let b = src[(n & 7) as usize];
                if n & 8 != 0 {
                    (b as i8 >> 7) as u8
                } else {
                    b
                }
            }),
            1 => std::array::from_fn(|i| src[i + s]),
            2 => std::array::from_fn(|i| src[if i >= s { i - s } else { 8 + i - s }]),
            3 => [src[s]; 4],
            4 => std::array::from_fn(|i| src[i.max(s)]),
            5 => std::array::from_fn(|i| src[i.min(s)]),
            _ => std::array::from_fn(|i| src[(i & 1) + (s & 1) * 2]),
        };
        u32::from_le_bytes(r)
    }

    #[test]
    fn prmt_matches_the_bytewise_definition() {
        let (a, c) = (0x8382_0180, 0x8786_8584);
        assert_eq!(prmt(a, 0x3210, c, 0), a);
        assert_eq!(prmt(a, 0x7654, c, 0), c);
        assert_eq!(prmt(a, 0x9810, c, 0), 0x00ff_0180, "sign bytes");
        assert_eq!(prmt(a, 0x1, c, 1), 0x8483_8201);
        assert_eq!(prmt(a, 0x1, c, 2), 0x8201_8087);
        assert_eq!(prmt(a, 0x2, c, 3), 0x8282_8282);
        assert_eq!(prmt(a, 0x2, c, 4), 0x8382_8282);
        assert_eq!(prmt(a, 0x1, c, 5), 0x0101_0180);
        assert_eq!(prmt(a, 0x1, c, 6), 0x8382_8382);
        let mut x = 0x2545_f491_4f6c_dd1du64;
        let mut next = || {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            x
        };
        for _ in 0..200_000 {
            let (a, c, sel) = (next() as u32, next() as u32, next() as u32);
            for mode in 0..8 {
                assert_eq!(prmt(a, sel, c, mode), reference(a, sel, c, mode), "{a:#x} {sel:#x} {c:#x} mode {mode}");
                assert_eq!(prmt(a, super::prmt_bytes(sel, mode), c, 0), reference(a, sel, c, mode), "as mode 0: {a:#x} {sel:#x} {c:#x} mode {mode}");
            }
        }
    }
}

#[cfg(test)]
mod run_tests {
    use crate::isa::*;
    use crate::jit::check::{program, Cb};
    use crate::{run, Host, Lanes};

    /// A word that decodes to an instruction `want` accepts.
    fn find(want: impl Fn(&Op) -> bool) -> u64 {
        let mut x = 0x2545_f491_4f6c_dd1du64;
        loop {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            let w = x & !(0xf << 16) | 7 << 16;
            if want(&crate::decode::decode(8, w).op) {
                return w;
            }
        }
    }

    /// PRMT with a selector that is the same in every lane (immediate and constant buffer forms),
    /// through `run`, against the byte-wise definition. (Where the JIT runs, it runs those.)
    #[test]
    fn prmt_with_one_selector() {
        for form in 0..2 {
            for mode in 0..8u8 {
                let w = find(|op| match *op {
                    Op::Prmt { b: Src::Imm(_), mode: m, .. } => form == 0 && m == mode,
                    Op::Prmt { b: Src::Cb(..), mode: m, .. } => form == 1 && m == mode,
                    _ => false,
                });
                let p = program(&[w]);
                let Op::Prmt { d, a, b, c, mode } = p.code[0].op else { unreachable!() };
                let mut l = Lanes::<16>::new();
                l.start(&p, 0xffff, 0);
                for k in 0..255 {
                    l.gpr[k] = std::array::from_fn(|i| ((k * 977 + i * 131) as u32).wrapping_mul(0x9e37_79b9));
                }
                let before = l.gpr.clone();
                let mut fuel = 10;
                run(&p, &mut l, &mut Cb, &mut fuel);
                let value = |s: Src, i: usize| match s {
                    Src::Reg(r) => before[r as usize][i],
                    Src::Imm(v) => v,
                    Src::Cb(k, o) => Host::<16>::cbuf(&Cb, k as u32, o),
                };
                for i in 0..16 {
                    let want = super::tests::reference(before[a as usize][i], value(b, i), value(c, i), mode);
                    assert_eq!(l.gpr[d as usize][i], want, "{:?} lane {i}", p.code[0].op);
                }
            }
        }
    }

    #[test]
    fn membar_decodes() {
        assert_eq!(crate::decode::decode(8, 0xef98_0000_0007_000f).op, Op::Membar);
    }
}

/// SBC_INTERP_STATS: count the instructions the interpreter runs, by kind, and print the most
/// common ones every 50 million (to see what keeps code out of the JIT).
fn interp_stats() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("SBC_INTERP_STATS").is_some())
}

#[cold]
fn count_interpreted(op: &Op) {
    use std::collections::HashMap;
    thread_local! {
        static COUNTS: std::cell::RefCell<(u64, HashMap<String, u64>)> = Default::default();
    }
    COUNTS.with(|c| {
        let mut c = c.borrow_mut();
        let name = format!("{op:?}");
        let kind = name[..name.find([' ', '(', '{']).unwrap_or(name.len())].to_string();
        *c.1.entry(kind).or_insert(0) += 1;
        c.0 += 1;
        if c.0 % 50_000_000 == 0 {
            let mut v: Vec<_> = c.1.iter().map(|(k, n)| (*n, k.clone())).collect();
            v.sort_unstable_by(|a, b| b.cmp(a));
            let top: Vec<String> = v.iter().take(14).map(|(n, k)| format!("{k} {:.1}%", 100.0 * *n as f64 / c.0 as f64)).collect();
            eprintln!("[interp] {}M instructions: {}", c.0 / 1_000_000, top.join(", "));
        }
    });
}
