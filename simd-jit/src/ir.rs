//! The IR: straight-line code over 16-lane vectors of 32-bit words and 16-bit lane masks.
//!
//! A block is a list of nodes; a node's value is its index (`Id`). Vector nodes make one word
//! per lane, scalar nodes a mask of lanes (bit i = lane i, in a `u32`). Nothing moves data
//! between lanes (`Lane` and `Const` make per-lane constants), which lets a backend run the
//! lanes in any grouping. A block reads and writes:
//!
//! - registers: `regs[r]`, 16 words each (the front end numbers its register files and
//!   accumulators into one space);
//! - flags: `flags[f]`, lane masks (the front end's per-lane condition bits);
//! - arguments: `args[k]`, words the same in every lane (uniforms, a core's number), fetched by
//!   the front end before the block runs;
//! - inputs: `inputs[k]`, vectors the front end fetches before the block runs (varyings, texture
//!   results, tile buffer reads: anything the hardware hands a program in order).
//!
//! Every operation has the exact semantics of `unary`/`binary` below, which `eval` uses and the
//! backends reproduce bit for bit. Floating point is IEEE single precision with round to nearest
//! even and no flushing: QPU-style denormal flushing is an explicit `Un::Ftz`.

pub type Id = u16;
/// Lanes of a vector.
pub const LANES: usize = 16;
/// One vector register.
pub type V = [u32; LANES];
/// A block ends before its IR gets longer than this (ids are 16 bits; the passes add nodes).
pub const MAX_NODES: usize = 12288;

/// Operations of one vector.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Un {
    /// Bitwise not, two's complement negation, count of leading zeros (32 for 0).
    Not,
    Neg,
    Clz,
    /// A denormal to zero with its sign (exponent field 0: only the sign bit is kept).
    Ftz,
    /// Signed and unsigned integers to single (rounded to nearest even).
    IToF,
    UToF,
    /// Single to integer, toward zero, saturated, NaN to 0 (Rust's `as i32`, `as u32`).
    FToI,
    FToU,
    FSqrt,
    /// To integral values: nearest even, toward zero, down, up.
    FRound,
    FTrunc,
    FFloor,
    FCeil,
    /// The host library's `f32::exp2` and `f32::log2` (the interpreters' functions), lane by
    /// lane.
    Exp2,
    Log2,
    /// A function the front end registered (`crate::register`), lane by lane: for operations
    /// that must be the interpreter's own to the bit (a sine, its exp2).
    Call(u8),
    /// The half in the low 16 bits to single, exactly (denormal halves too; a NaN is quieted,
    /// its payload kept); a single to a half rounded to nearest even (denormal halves made,
    /// too large to infinity, a NaN to 0x7e00 with its sign), in the low 16 bits.
    HalfToF,
    FToHalf,
    /// Shifts by a constant (0..32).
    Shl(u8),
    ShrU(u8),
    ShrS(u8),
}

/// Operations of two vectors (lane by lane).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Bin {
    And,
    Or,
    Xor,
    Add,
    Sub,
    /// The low 32 bits of the product.
    Mul,
    /// Shifts and rotation by the second operand's low five bits.
    Shl,
    ShrU,
    ShrS,
    Ror,
    /// Signed and unsigned minimum and maximum.
    IMin,
    IMax,
    UMin,
    UMax,
    /// Comparisons: all ones where true, else zero. `ILt` signed, `ULt` unsigned.
    IEq,
    ILt,
    ULt,
    FAdd,
    FSub,
    FMul,
    FDiv,
    /// The QPUs' minimum and maximum: a NaN operand gives the other one (the second if both
    /// are), else the smaller (larger) one, -0 below +0.
    FMin,
    FMax,
    /// The host's `f32::min` and `f32::max` (IEEE minNum/maxNum as the host's library or
    /// instruction does them; AArch64's FMINNM/FMAXNM).
    FMinNum,
    FMaxNum,
    /// Float comparisons (false with a NaN), and "either is a NaN".
    FEq,
    FLt,
    FLe,
    FUnord,
    /// Per byte, unsigned: saturating add and subtract, minimum, maximum, and the VideoCore
    /// IV's multiply (t = a * b + 128; (t + (t >> 8)) >> 8).
    V8AddSat,
    V8SubSat,
    V8Min,
    V8Max,
    V8MulD,
}

/// Operations of two lane masks.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum SOp {
    And,
    Or,
    Xor,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Node {
    // ---- vectors
    /// A register.
    Reg(u16),
    /// The same word in every lane.
    Imm(u32),
    /// A vector constant: `Block::consts[k]`.
    Const(u16),
    /// `args[k]` in every lane.
    Arg(u16),
    /// `inputs[k]`.
    In(u16),
    /// The lane's number (0..15).
    Lane,
    /// Lanes moved: lane i of the result is lane `Block::perms[p][i]` of the vector (quad
    /// derivatives, rotations).
    Shuffle(Id, u16),
    Un(Un, Id),
    Bin(Bin, Id, Id),
    /// Lane by lane: where the first (a vector mask: all ones or zero) is set, the second,
    /// else the third.
    Sel(Id, Id, Id),
    /// A lane mask as a vector mask: all ones in the lanes whose bit is set.
    Expand(Id),
    // ---- lane masks
    /// `flags[f]`.
    Flag(u8),
    /// A constant mask.
    SImm(u32),
    S(SOp, Id, Id),
    /// The lanes of a vector that are not zero.
    Bits(Id),
    // ---- writes
    /// Writes register `r` in the lanes of a mask: (value, mask).
    Store(u16, Id, Id),
    /// Writes register `r` in every lane.
    StoreAll(u16, Id),
    /// Writes flag `f` in the lanes of a mask: (value mask, mask).
    StoreFlag(u8, Id, Id),
    /// Writes `outputs[k]` (all lanes): what the front end does after the block, in order
    /// (writes to the tile buffer, the VPM, ...).
    Out(u16, Id),
}

impl Node {
    /// True if the node makes a lane mask.
    pub fn scalar(&self) -> bool {
        matches!(self, Node::Flag(_) | Node::SImm(_) | Node::S(..) | Node::Bits(_))
    }
    /// True if the node makes nothing (it writes).
    pub fn effect(&self) -> bool {
        matches!(self, Node::Store(..) | Node::StoreAll(..) | Node::StoreFlag(..) | Node::Out(..))
    }
    /// The values it reads.
    pub fn args(&self) -> impl Iterator<Item = Id> {
        let (a, n) = match *self {
            Node::Un(_, a) | Node::Expand(a) | Node::Bits(a) | Node::StoreAll(_, a) | Node::Shuffle(a, _) | Node::Out(_, a) => ([a, 0, 0], 1),
            Node::Bin(_, a, b) | Node::S(_, a, b) | Node::Store(_, a, b) | Node::StoreFlag(_, a, b) => ([a, b, 0], 2),
            Node::Sel(a, b, c) => ([a, b, c], 3),
            _ => ([0; 3], 0),
        };
        a.into_iter().take(n)
    }
}

/// A block of IR, ready for a backend (`Builder::finish`).
#[derive(Clone, Debug, Default)]
pub struct Block {
    pub ir: Vec<Node>,
    /// The vector constants of `Node::Const`.
    pub consts: Vec<V>,
    /// The permutations of `Node::Shuffle`.
    pub perms: Vec<[u8; LANES]>,
    /// Registers, flags, arguments, inputs and outputs it may touch: one more than the highest
    /// index of each (`Code::run` checks the slices are at least this long).
    pub nregs: u16,
    pub nflags: u8,
    pub nargs: u16,
    pub ninputs: u16,
    pub nouts: u16,
}

/// Builds a block, as a front end lowers instructions one after another: reads see the writes
/// before them, as in an interpreter. `finish` then keeps registers and flags in values within
/// the block (each read once, each written once at the end) and makes equal values once.
#[derive(Default, Clone)]
pub struct Builder {
    ir: Vec<Node>,
    consts: Vec<V>,
    perms: Vec<[u8; LANES]>,
}

/// Where a builder was, to go back to (`Builder::mark`, `Builder::reset`).
#[derive(Clone, Copy, Debug)]
pub struct Mark(usize, usize, usize);

impl Builder {
    pub fn new() -> Self {
        Builder::default()
    }
    /// Nodes so far (a front end ends a block before `MAX_NODES`).
    pub fn len(&self) -> usize {
        self.ir.len()
    }
    pub fn is_empty(&self) -> bool {
        self.ir.is_empty()
    }
    pub fn mark(&self) -> Mark {
        Mark(self.ir.len(), self.consts.len(), self.perms.len())
    }
    /// Drops what was built since `m` (an instruction the block cannot take after all).
    pub fn reset(&mut self, m: Mark) {
        self.ir.truncate(m.0);
        self.consts.truncate(m.1);
        self.perms.truncate(m.2);
    }
    pub fn node(&mut self, n: Node) -> Id {
        debug_assert!(n.args().all(|a| (a as usize) < self.ir.len()), "{n:?} reads a later node");
        self.ir.push(n);
        (self.ir.len() - 1) as Id
    }

    // ---- vectors
    pub fn reg(&mut self, r: u16) -> Id {
        self.node(Node::Reg(r))
    }
    pub fn imm(&mut self, v: u32) -> Id {
        self.node(Node::Imm(v))
    }
    /// A vector constant (a different word per lane).
    pub fn vconst(&mut self, v: V) -> Id {
        if v.iter().all(|&x| x == v[0]) {
            return self.imm(v[0]);
        }
        let k = match self.consts.iter().position(|c| *c == v) {
            Some(k) => k,
            None => {
                self.consts.push(v);
                self.consts.len() - 1
            }
        };
        self.node(Node::Const(k as u16))
    }
    pub fn arg(&mut self, k: u16) -> Id {
        self.node(Node::Arg(k))
    }
    pub fn input(&mut self, k: u16) -> Id {
        self.node(Node::In(k))
    }
    pub fn lane(&mut self) -> Id {
        self.node(Node::Lane)
    }
    /// Lane i of the result is lane `p[i]` of `a` (each below 16).
    pub fn shuffle(&mut self, a: Id, p: [u8; LANES]) -> Id {
        assert!(p.iter().all(|&x| (x as usize) < LANES));
        if p.iter().enumerate().all(|(i, &x)| x as usize == i) {
            return a;
        }
        let k = match self.perms.iter().position(|c| *c == p) {
            Some(k) => k,
            None => {
                self.perms.push(p);
                self.perms.len() - 1
            }
        };
        self.node(Node::Shuffle(a, k as u16))
    }
    /// Lanes rotated up by `n`: lane i of the result is lane (i - n) mod 16 of `a`.
    pub fn rotate(&mut self, a: Id, n: usize) -> Id {
        self.shuffle(a, std::array::from_fn(|i| ((i + LANES - n % LANES) % LANES) as u8))
    }
    /// A function the front end registered (`crate::register`), lane by lane.
    pub fn call(&mut self, f: u8, a: Id) -> Id {
        self.un(Un::Call(f), a)
    }
    pub fn un(&mut self, op: Un, a: Id) -> Id {
        self.node(Node::Un(op, a))
    }
    pub fn bin(&mut self, op: Bin, a: Id, b: Id) -> Id {
        self.node(Node::Bin(op, a, b))
    }
    pub fn bin_imm(&mut self, op: Bin, a: Id, v: u32) -> Id {
        let b = self.imm(v);
        self.bin(op, a, b)
    }
    /// `m` a vector mask.
    pub fn sel(&mut self, m: Id, a: Id, b: Id) -> Id {
        self.node(Node::Sel(m, a, b))
    }
    /// A lane mask as a vector mask.
    pub fn expand(&mut self, s: Id) -> Id {
        self.node(Node::Expand(s))
    }
    /// Shifts by a constant: by 32 or more, zero (or the sign, `ShrS`).
    pub fn shl(&mut self, a: Id, n: u32) -> Id {
        if n >= 32 {
            self.imm(0)
        } else {
            self.un(Un::Shl(n as u8), a)
        }
    }
    pub fn shr(&mut self, a: Id, n: u32) -> Id {
        if n >= 32 {
            self.imm(0)
        } else {
            self.un(Un::ShrU(n as u8), a)
        }
    }
    pub fn sar(&mut self, a: Id, n: u32) -> Id {
        self.un(Un::ShrS(n.min(31) as u8), a)
    }

    // ---- lane masks
    pub fn flag(&mut self, f: u8) -> Id {
        self.node(Node::Flag(f))
    }
    pub fn simm(&mut self, m: u32) -> Id {
        self.node(Node::SImm(m))
    }
    pub fn sop(&mut self, op: SOp, a: Id, b: Id) -> Id {
        self.node(Node::S(op, a, b))
    }
    /// The 16 lanes' complement.
    pub fn snot(&mut self, a: Id) -> Id {
        let all = self.simm(0xffff);
        self.sop(SOp::Xor, a, all)
    }
    /// The lanes where a vector is not zero.
    pub fn bits(&mut self, v: Id) -> Id {
        self.node(Node::Bits(v))
    }

    // ---- writes
    /// Writes register `r` in the lanes of `mask` (a lane mask).
    pub fn store(&mut self, r: u16, v: Id, mask: Id) {
        self.node(Node::Store(r, v, mask));
    }
    pub fn store_all(&mut self, r: u16, v: Id) {
        self.node(Node::StoreAll(r, v));
    }
    /// Writes flag `f` (a lane mask) in the lanes of `mask`.
    pub fn store_flag(&mut self, f: u8, v: Id, mask: Id) {
        self.node(Node::StoreFlag(f, v, mask));
    }
    /// Writes `outputs[k]` (every lane), for the front end to act on after the block.
    pub fn out(&mut self, k: u16, v: Id) {
        self.node(Node::Out(k, v));
    }

    // ---- compositions the QPUs share (exact, made of the operations above)
    /// Unsigned and float greater-than (the comparisons the other way round).
    pub fn ugt(&mut self, a: Id, b: Id) -> Id {
        self.bin(Bin::ULt, b, a)
    }
    pub fn fgt(&mut self, a: Id, b: Id) -> Id {
        self.bin(Bin::FLt, b, a)
    }
    /// Any NaN to `nan` (a GPU's canonical NaN: 0x7fc0_0000, 0x7fff_ffff, ...).
    pub fn canon(&mut self, a: Id, nan: u32) -> Id {
        let ok = self.bin(Bin::FEq, a, a);
        let n = self.imm(nan);
        self.sel(ok, a, n)
    }
    /// Signed saturating add and subtract.
    pub fn add_sat(&mut self, a: Id, b: Id) -> Id {
        let r = self.bin(Bin::Add, a, b);
        // overflow where both operands' signs differ from the result's
        let ar = self.bin(Bin::Xor, a, r);
        let br = self.bin(Bin::Xor, b, r);
        let o = self.bin(Bin::And, ar, br);
        self.saturate(a, r, o)
    }
    pub fn sub_sat(&mut self, a: Id, b: Id) -> Id {
        let r = self.bin(Bin::Sub, a, b);
        // overflow where the operands' signs differ and the result's differs from a's
        let ab = self.bin(Bin::Xor, a, b);
        let ar = self.bin(Bin::Xor, a, r);
        let o = self.bin(Bin::And, ab, ar);
        self.saturate(a, r, o)
    }
    fn saturate(&mut self, a: Id, r: Id, o: Id) -> Id {
        let zero = self.imm(0);
        let over = self.bin(Bin::ILt, o, zero);
        // a < 0: i32::MIN, else i32::MAX
        let s = self.sar(a, 31);
        let lim = self.bin_imm(Bin::Xor, s, 0x7fff_ffff);
        self.sel(over, lim, r)
    }
    /// Signed clamp to [lo, hi].
    pub fn clamp(&mut self, v: Id, lo: i32, hi: i32) -> Id {
        let l = self.imm(lo as u32);
        let v = self.bin(Bin::IMax, v, l);
        let h = self.imm(hi as u32);
        self.bin(Bin::IMin, v, h)
    }
    /// A half in the low 16 bits to single, as the VideoCore IV does it: denormal halves to
    /// zero with their sign, infinities and NaNs with the mantissa shifted up (no quieting).
    pub fn half_to_f32_ftz(&mut self, h: Id) -> Id {
        let sign = self.bin_imm(Bin::And, h, 0x8000);
        let sign = self.shl(sign, 16);
        let e = self.shr(h, 10);
        let e = self.bin_imm(Bin::And, e, 0x1f);
        let m = self.bin_imm(Bin::And, h, 0x3ff);
        let mag = self.bin_imm(Bin::And, h, 0x7fff);
        let mag = self.shl(mag, 13);
        let normal = self.bin_imm(Bin::Add, mag, 112 << 23);
        let ms = self.shl(m, 13);
        let special = self.bin_imm(Bin::Or, ms, 0x7f80_0000);
        let is31 = self.bin_imm(Bin::IEq, e, 31);
        let v = self.sel(is31, special, normal);
        let is0 = self.bin_imm(Bin::IEq, e, 0);
        let zero = self.imm(0);
        let v = self.sel(is0, zero, v);
        self.bin(Bin::Or, sign, v)
    }
    /// A single to a half (in the low 16 bits) as the VideoCore IV does it: rounded to nearest
    /// even, too small to a signed zero (no denormal halves), too large to infinity, a NaN to
    /// 0x7e00 with its sign (the input as it is: flush it first if need be).
    pub fn f32_to_half_ftz(&mut self, u: Id) -> Id {
        let s = self.shr(u, 16);
        let s = self.bin_imm(Bin::And, s, 0x8000);
        let e = self.shr(u, 23);
        let e = self.bin_imm(Bin::And, e, 0xff);
        let m = self.bin_imm(Bin::And, u, 0x7f_ffff);
        // e == 255: infinity, or a NaN (0x200 in the mantissa)
        let mz = self.bin_imm(Bin::IEq, m, 0);
        let zero = self.imm(0);
        let q = self.imm(0x200);
        let nanbit = self.sel(mz, zero, q);
        let inf = self.bin_imm(Bin::Or, s, 0x7c00);
        let special = self.bin(Bin::Or, inf, nanbit);
        // the rounded half for exponents 113..=142: (e - 112) << 10 | m >> 13, rounded
        let mag = self.bin_imm(Bin::And, u, 0x7fff_ffff);
        let mag = self.shr(mag, 13);
        let r0 = self.bin_imm(Bin::Sub, mag, 112 << 10);
        let rest = self.bin_imm(Bin::And, m, 0x1fff);
        let half = self.imm(0x1000);
        let above = self.bin(Bin::ULt, half, rest);
        let tie = self.bin(Bin::IEq, rest, half);
        let odd = self.bin_imm(Bin::And, r0, 1);
        let odd = self.bin_imm(Bin::IEq, odd, 1);
        let tie = self.bin(Bin::And, tie, odd);
        let inc = self.bin(Bin::Or, above, tie);
        // (inc is all ones: subtracting it adds one)
        let r = self.bin(Bin::Sub, r0, inc);
        let normal = self.bin(Bin::Or, s, r);
        let small_lim = self.imm(113);
        let small = self.bin(Bin::ILt, e, small_lim);
        let v = self.sel(small, s, normal);
        let big_lim = self.imm(142);
        let big = self.bin(Bin::ILt, big_lim, e);
        let v = self.sel(big, inf, v);
        let is255 = self.bin_imm(Bin::IEq, e, 255);
        self.sel(is255, special, v)
    }

    /// The finished block: registers and flags kept in values, written once at the end; equal
    /// values made once.
    pub fn finish(self) -> Block {
        let mut b = Block { ir: Vec::new(), consts: self.consts, perms: self.perms, nregs: 0, nflags: 0, nargs: 0, ninputs: 0, nouts: 0 };
        let ir = fold(&forward(&self.ir));
        for n in &ir {
            match *n {
                Node::Reg(r) | Node::Store(r, ..) | Node::StoreAll(r, _) => b.nregs = b.nregs.max(r + 1),
                Node::Flag(f) | Node::StoreFlag(f, ..) => b.nflags = b.nflags.max(f + 1),
                Node::Arg(k) => b.nargs = b.nargs.max(k + 1),
                Node::In(k) => b.ninputs = b.ninputs.max(k + 1),
                Node::Out(k, _) => b.nouts = b.nouts.max(k + 1),
                _ => {}
            }
        }
        b.ir = ir;
        b
    }
}

/// Checks that every node reads values of the right kind (vectors or lane masks), earlier
/// nodes, constants that exist. (`Code::new` asserts it in debug builds.)
pub fn validate(b: &Block) -> Result<(), String> {
    let kind = |k: Id| -> Option<bool> { b.ir.get(k as usize).map(|n| n.scalar()) };
    for (k, n) in b.ir.iter().enumerate() {
        for a in n.args() {
            if a as usize >= k {
                return Err(format!("node {k} {n:?} reads a later node {a}"));
            }
            if b.ir[a as usize].effect() {
                return Err(format!("node {k} {n:?} reads a write"));
            }
        }
        let want: &[bool] = match *n {
            Node::Un(..) | Node::Shuffle(..) | Node::Out(..) => &[false],
            Node::Bin(..) => &[false, false],
            Node::Sel(..) => &[false, false, false],
            Node::Expand(_) => &[true],
            Node::S(..) => &[true, true],
            Node::Bits(_) => &[false],
            Node::Store(..) => &[false, true],
            Node::StoreAll(..) => &[false],
            Node::StoreFlag(..) => &[true, true],
            _ => &[],
        };
        for (a, &s) in n.args().zip(want) {
            if kind(a) != Some(s) {
                return Err(format!("node {k} {n:?}: argument {a} should be {}", if s { "a lane mask" } else { "a vector" }));
            }
        }
        if let Node::Const(c) = *n {
            if c as usize >= b.consts.len() {
                return Err(format!("node {k}: no constant {c}"));
            }
        }
        if let Node::Shuffle(_, p) = *n {
            if b.perms.get(p as usize).is_none_or(|p| p.iter().any(|&x| x as usize >= LANES)) {
                return Err(format!("node {k}: no permutation {p}"));
            }
        }
        if let Node::Un(Un::Shl(s) | Un::ShrU(s) | Un::ShrS(s), _) = *n {
            if s >= 32 {
                return Err(format!("node {k}: shift by {s}"));
            }
        }
    }
    if b.ir.len() >= Id::MAX as usize {
        return Err("too many nodes".into());
    }
    Ok(())
}

// ---------------------------------------------------------------------------- semantics

#[inline]
fn f(x: u32) -> f32 {
    f32::from_bits(x)
}

/// A unary operation on one lane.
pub fn unary(op: Un, x: u32) -> u32 {
    match op {
        Un::Not => !x,
        Un::Neg => x.wrapping_neg(),
        Un::Clz => x.leading_zeros(),
        Un::Ftz => {
            if x & 0x7f80_0000 == 0 {
                x & 0x8000_0000
            } else {
                x
            }
        }
        Un::IToF => (x as i32 as f32).to_bits(),
        Un::UToF => (x as f32).to_bits(),
        Un::FToI => f(x) as i32 as u32,
        Un::FToU => f(x) as u32,
        Un::FSqrt => f(x).sqrt().to_bits(),
        Un::FRound => f(x).round_ties_even().to_bits(),
        Un::FTrunc => f(x).trunc().to_bits(),
        Un::FFloor => f(x).floor().to_bits(),
        Un::FCeil => f(x).ceil().to_bits(),
        Un::Exp2 => f(x).exp2().to_bits(),
        Un::Log2 => f(x).log2().to_bits(),
        Un::Call(k) => crate::call(k, x),
        Un::HalfToF => half_to_f32(x as u16),
        Un::FToHalf => f32_to_half(x) as u32,
        Un::Shl(n) => x << n,
        Un::ShrU(n) => x >> n,
        Un::ShrS(n) => (x as i32 >> n) as u32,
    }
}

/// A half to single, exactly, a NaN quieted (as AArch64's FCVTL does it).
pub fn half_to_f32(h: u16) -> u32 {
    let s = (h as u32 & 0x8000) << 16;
    let e = (h >> 10 & 0x1f) as u32;
    let m = (h & 0x3ff) as u32;
    match (e, m) {
        (0, 0) => s,
        // a denormal half is m * 2^-24: normalised for single
        (0, _) => {
            let sh = m.leading_zeros() - 21;
            s | (113 - sh) << 23 | (m << sh & 0x3ff) << 13
        }
        (31, 0) => s | 0x7f80_0000,
        (31, _) => s | 0x7fc0_0000 | m << 13,
        _ => s | (e + 112) << 23 | m << 13,
    }
}

/// A single to half, rounded to nearest even; a NaN to 0x7e00 with its sign.
pub fn f32_to_half(u: u32) -> u16 {
    let s = (u >> 16 & 0x8000) as u16;
    let e = (u >> 23 & 0xff) as i32;
    let m = u & 0x7f_ffff;
    if e == 255 {
        return s | if m != 0 { 0x7e00 } else { 0x7c00 };
    }
    // the significand with its leading one, and the shift that leaves units of the half's last
    // place (2^-24 for denormal halves); below 2^-25 everything rounds to zero
    if e < 102 {
        return s;
    }
    let sig = m | 0x80_0000;
    let sh = if e >= 113 { 13 } else { (126 - e) as u32 };
    let q = sig >> sh;
    let rest = sig & ((1u32 << sh) - 1);
    let half = 1u32 << (sh - 1);
    let q = q + (rest > half || (rest == half && q & 1 != 0)) as u32;
    if e >= 113 {
        // (q has the leading one at bit 10: the exponent's carry when rounding up is right)
        let v = (((e - 112) as u32) << 10) + (q - 0x400);
        if v >= 0x7c00 {
            s | 0x7c00
        } else {
            s | v as u16
        }
    } else {
        s | q as u16
    }
}

#[inline]
fn v8(a: u32, b: u32, g: impl Fn(u32, u32) -> u32) -> u32 {
    let mut r = 0;
    for k in 0..4 {
        r |= g(a >> (8 * k) & 0xff, b >> (8 * k) & 0xff) << (8 * k);
    }
    r
}

/// A binary operation on one lane.
pub fn binary(op: Bin, x: u32, y: u32) -> u32 {
    let t = |b: bool| if b { !0 } else { 0 };
    let (a, b) = (f(x), f(y));
    match op {
        Bin::And => x & y,
        Bin::Or => x | y,
        Bin::Xor => x ^ y,
        Bin::Add => x.wrapping_add(y),
        Bin::Sub => x.wrapping_sub(y),
        Bin::Mul => x.wrapping_mul(y),
        Bin::Shl => x << (y & 31),
        Bin::ShrU => x >> (y & 31),
        Bin::ShrS => (x as i32 >> (y & 31)) as u32,
        Bin::Ror => x.rotate_right(y & 31),
        Bin::IMin => (x as i32).min(y as i32) as u32,
        Bin::IMax => (x as i32).max(y as i32) as u32,
        Bin::UMin => x.min(y),
        Bin::UMax => x.max(y),
        Bin::IEq => t(x == y),
        Bin::ILt => t((x as i32) < y as i32),
        Bin::ULt => t(x < y),
        Bin::FAdd => (a + b).to_bits(),
        Bin::FSub => (a - b).to_bits(),
        Bin::FMul => (a * b).to_bits(),
        Bin::FDiv => (a / b).to_bits(),
        Bin::FMin => {
            if a.is_nan() {
                y
            } else if b.is_nan() || a < b || (a == b && a.is_sign_negative()) {
                x
            } else {
                y
            }
        }
        Bin::FMax => {
            if a.is_nan() {
                y
            } else if b.is_nan() || a > b || (a == b && b.is_sign_negative()) {
                x
            } else {
                y
            }
        }
        Bin::FMinNum => a.min(b).to_bits(),
        Bin::FMaxNum => a.max(b).to_bits(),
        Bin::FEq => t(a == b),
        Bin::FLt => t(a < b),
        Bin::FLe => t(a <= b),
        Bin::FUnord => t(a.is_nan() || b.is_nan()),
        Bin::V8AddSat => v8(x, y, |p, q| (p + q).min(255)),
        Bin::V8SubSat => v8(x, y, |p, q| p.saturating_sub(q)),
        Bin::V8Min => v8(x, y, |p, q| p.min(q)),
        Bin::V8Max => v8(x, y, |p, q| p.max(q)),
        Bin::V8MulD => v8(x, y, |p, q| {
            let t = p * q + 128;
            (t + (t >> 8)) >> 8
        }),
    }
}

/// Runs a block's IR: the reference the backends are tested against (and what runs where there
/// is no backend).
pub fn eval(b: &Block, regs: &mut [V], flags: &mut [u32], args: &[u32], inputs: &[V], outputs: &mut [V]) {
    let mut val = vec![[0u32; LANES]; b.ir.len()];
    let mut s = vec![0u32; b.ir.len()];
    for (k, n) in b.ir.iter().enumerate() {
        let v = |i: Id| val[i as usize];
        match *n {
            Node::Reg(r) => val[k] = regs[r as usize],
            Node::Imm(x) => val[k] = [x; LANES],
            Node::Const(c) => val[k] = b.consts[c as usize],
            Node::Arg(i) => val[k] = [args[i as usize]; LANES],
            Node::In(i) => val[k] = inputs[i as usize],
            Node::Lane => val[k] = std::array::from_fn(|i| i as u32),
            Node::Shuffle(a, p) => {
                let (x, p) = (v(a), b.perms[p as usize]);
                val[k] = std::array::from_fn(|i| x[p[i] as usize]);
            }
            Node::Un(op, a) => val[k] = v(a).map(|x| unary(op, x)),
            Node::Bin(op, a, c) => {
                let (x, y) = (v(a), v(c));
                val[k] = std::array::from_fn(|i| binary(op, x[i], y[i]));
            }
            Node::Sel(m, a, c) => {
                let (m, x, y) = (v(m), v(a), v(c));
                val[k] = std::array::from_fn(|i| x[i] & m[i] | y[i] & !m[i]);
            }
            Node::Expand(a) => val[k] = std::array::from_fn(|i| if s[a as usize] >> i & 1 != 0 { !0 } else { 0 }),
            Node::Flag(i) => s[k] = flags[i as usize],
            Node::SImm(x) => s[k] = x,
            Node::S(op, a, c) => {
                let (x, y) = (s[a as usize], s[c as usize]);
                s[k] = match op {
                    SOp::And => x & y,
                    SOp::Or => x | y,
                    SOp::Xor => x ^ y,
                };
            }
            Node::Bits(a) => s[k] = (0..LANES).fold(0, |m, i| m | ((v(a)[i] != 0) as u32) << i),
            Node::Store(r, a, m) => {
                let (x, m) = (v(a), s[m as usize]);
                for i in 0..LANES {
                    if m >> i & 1 != 0 {
                        regs[r as usize][i] = x[i];
                    }
                }
            }
            Node::StoreAll(r, a) => regs[r as usize] = v(a),
            Node::StoreFlag(f, a, m) => {
                let (x, m) = (s[a as usize], s[m as usize]);
                let r = &mut flags[f as usize];
                *r = *r & !m | x & m;
            }
            Node::Out(o, a) => outputs[o as usize] = v(a),
        }
    }
}

// ---------------------------------------------------------------------------- passes

/// Keeps registers in values within a block: a register is read once, a write makes its new
/// value (the old one outside the mask), and each register written is stored once, at the end,
/// whole.
fn forward(ir: &[Node]) -> Vec<Node> {
    let nregs = ir.iter().filter_map(|n| match *n {
        Node::Reg(r) | Node::Store(r, ..) | Node::StoreAll(r, _) => Some(r as usize + 1),
        _ => None,
    });
    let nregs = nregs.max().unwrap_or(0);
    let mut out: Vec<Node> = Vec::with_capacity(ir.len());
    let mut map = vec![0 as Id; ir.len()];
    let mut cur: Vec<Option<Id>> = vec![None; nregs];
    let mut dirty = vec![false; nregs];
    let mut order: Vec<u16> = Vec::new();
    // the vector mask of each lane mask, made once
    let mut expanded: Vec<Option<Id>> = vec![None; ir.len()];
    let push = |out: &mut Vec<Node>, n: Node| {
        out.push(n);
        (out.len() - 1) as Id
    };
    for (k, n) in ir.iter().enumerate() {
        let r = |a: Id| map[a as usize];
        map[k] = match *n {
            Node::Reg(g) => match cur[g as usize] {
                Some(c) => c,
                None => {
                    let c = push(&mut out, Node::Reg(g));
                    cur[g as usize] = Some(c);
                    c
                }
            },
            Node::Expand(s) => match expanded[s as usize] {
                Some(e) => e,
                None => {
                    let e = push(&mut out, Node::Expand(r(s)));
                    expanded[s as usize] = Some(e);
                    e
                }
            },
            Node::Store(g, v, m) => {
                let old = match cur[g as usize] {
                    Some(c) => c,
                    None => push(&mut out, Node::Reg(g)),
                };
                let e = match expanded[m as usize] {
                    Some(e) => e,
                    None => {
                        let e = push(&mut out, Node::Expand(r(m)));
                        expanded[m as usize] = Some(e);
                        e
                    }
                };
                let s = push(&mut out, Node::Sel(e, r(v), old));
                cur[g as usize] = Some(s);
                if !dirty[g as usize] {
                    dirty[g as usize] = true;
                    order.push(g);
                }
                s
            }
            Node::StoreAll(g, a) => {
                cur[g as usize] = Some(r(a));
                if !dirty[g as usize] {
                    dirty[g as usize] = true;
                    order.push(g);
                }
                r(a)
            }
            Node::Un(op, a) => push(&mut out, Node::Un(op, r(a))),
            Node::Bin(op, a, b) => push(&mut out, Node::Bin(op, r(a), r(b))),
            Node::Sel(a, b, c) => push(&mut out, Node::Sel(r(a), r(b), r(c))),
            Node::S(op, a, b) => push(&mut out, Node::S(op, r(a), r(b))),
            Node::Bits(a) => push(&mut out, Node::Bits(r(a))),
            Node::StoreFlag(f, a, m) => push(&mut out, Node::StoreFlag(f, r(a), r(m))),
            Node::Shuffle(a, p) => push(&mut out, Node::Shuffle(r(a), p)),
            Node::Out(o, a) => push(&mut out, Node::Out(o, r(a))),
            n @ (Node::Imm(_) | Node::Const(_) | Node::Arg(_) | Node::In(_) | Node::Lane | Node::Flag(_) | Node::SImm(_)) => push(&mut out, n),
        };
    }
    order.sort_unstable();
    for g in order {
        out.push(Node::StoreAll(g, cur[g as usize].unwrap()));
    }
    out
}

/// Flags kept in values within a block as `forward` keeps registers, each written once at the
/// end; every value made once (the same node of the same arguments is the same value);
/// operations of constants folded where that is plainly exact; and what nothing reads dropped.
fn fold(ir: &[Node]) -> Vec<Node> {
    use std::collections::HashMap;
    let nflags = ir.iter().filter_map(|n| match *n {
        Node::Flag(f) | Node::StoreFlag(f, ..) => Some(f as usize + 1),
        _ => None,
    });
    let nflags = nflags.max().unwrap_or(0);
    let mut out: Vec<Node> = Vec::with_capacity(ir.len());
    let mut map = vec![0 as Id; ir.len()];
    let mut seen: HashMap<Node, Id> = HashMap::new();
    let mut cur: Vec<Option<Id>> = vec![None; nflags];
    let mut dirty = vec![false; nflags];
    let mut stores = Vec::new();
    fn make(out: &mut Vec<Node>, seen: &mut HashMap<Node, Id>, n: Node) -> Id {
        // constants: integer operations of immediates, lane masks of constants
        let n = match n {
            Node::Bin(op @ (Bin::And | Bin::Or | Bin::Xor | Bin::Add | Bin::Sub | Bin::Mul | Bin::IEq | Bin::ILt | Bin::ULt), a, b) => {
                match (out[a as usize], out[b as usize]) {
                    (Node::Imm(x), Node::Imm(y)) => Node::Imm(binary(op, x, y)),
                    _ => n,
                }
            }
            Node::Un(op @ (Un::Not | Un::Neg | Un::Shl(_) | Un::ShrU(_) | Un::ShrS(_)), a) => match out[a as usize] {
                Node::Imm(x) => Node::Imm(unary(op, x)),
                _ => n,
            },
            Node::S(op, a, b) => match (out[a as usize], out[b as usize]) {
                (Node::SImm(x), Node::SImm(y)) => Node::SImm(match op {
                    SOp::And => x & y,
                    SOp::Or => x | y,
                    SOp::Xor => x ^ y,
                }),
                _ => n,
            },
            Node::Sel(_, a, b) if a == b => return a,
            Node::Sel(m, a, b) => match out[m as usize] {
                Node::Imm(0) => return b,
                Node::Imm(u32::MAX) => return a,
                Node::Expand(s) => match out[s as usize] {
                    Node::SImm(x) if x & 0xffff == 0 => return b,
                    Node::SImm(x) if x & 0xffff == 0xffff => return a,
                    _ => Node::Sel(m, a, b),
                },
                _ => n,
            },
            _ => n,
        };
        if let Some(&k) = seen.get(&n) {
            return k;
        }
        out.push(n);
        let k = (out.len() - 1) as Id;
        seen.insert(n, k);
        k
    }
    for (k, n) in ir.iter().enumerate() {
        let r = |a: Id| map[a as usize];
        map[k] = match *n {
            Node::Flag(f) => match cur[f as usize] {
                Some(c) => c,
                None => {
                    let c = make(&mut out, &mut seen, Node::Flag(f));
                    cur[f as usize] = Some(c);
                    c
                }
            },
            Node::StoreFlag(f, v, m) => {
                let old = match cur[f as usize] {
                    Some(c) => c,
                    None => make(&mut out, &mut seen, Node::Flag(f)),
                };
                let new = match out[r(m) as usize] {
                    // (a constant mask: all lanes, or none)
                    Node::SImm(x) if x & 0xffff == 0xffff => r(v),
                    Node::SImm(x) if x & 0xffff == 0 => old,
                    _ => {
                        let all = make(&mut out, &mut seen, Node::SImm(!0));
                        let not_m = make(&mut out, &mut seen, Node::S(SOp::Xor, r(m), all));
                        let keep = make(&mut out, &mut seen, Node::S(SOp::And, old, not_m));
                        let put = make(&mut out, &mut seen, Node::S(SOp::And, r(v), r(m)));
                        make(&mut out, &mut seen, Node::S(SOp::Or, keep, put))
                    }
                };
                cur[f as usize] = Some(new);
                dirty[f as usize] = true;
                new
            }
            Node::StoreAll(g, a) => {
                stores.push(Node::StoreAll(g, r(a)));
                0
            }
            Node::Store(..) => unreachable!("forward made registers whole"),
            Node::Reg(_) | Node::Imm(_) | Node::Const(_) | Node::Arg(_) | Node::In(_) | Node::Lane | Node::SImm(_) => make(&mut out, &mut seen, *n),
            Node::Un(op, a) => make(&mut out, &mut seen, Node::Un(op, r(a))),
            Node::Bin(op, a, b) => make(&mut out, &mut seen, Node::Bin(op, r(a), r(b))),
            Node::Sel(a, b, c) => make(&mut out, &mut seen, Node::Sel(r(a), r(b), r(c))),
            Node::Expand(a) => make(&mut out, &mut seen, Node::Expand(r(a))),
            Node::S(op, a, b) => make(&mut out, &mut seen, Node::S(op, r(a), r(b))),
            Node::Bits(a) => make(&mut out, &mut seen, Node::Bits(r(a))),
            Node::Shuffle(a, p) => make(&mut out, &mut seen, Node::Shuffle(r(a), p)),
            // (outputs keep their order among themselves: each one is a node of its own)
            Node::Out(o, a) => {
                out.push(Node::Out(o, r(a)));
                (out.len() - 1) as Id
            }
        };
    }
    // a store of a register's own unchanged value is no store
    let stores: Vec<Node> = stores.into_iter().filter(|s| !matches!(*s, Node::StoreAll(g, a) if out[a as usize] == Node::Reg(g))).collect();
    out.extend(stores);
    for f in 0..nflags {
        if let (true, Some(c)) = (dirty[f], cur[f]) {
            if out[c as usize] != Node::Flag(f as u8) {
                let all = make(&mut out, &mut seen, Node::SImm(!0));
                out.push(Node::StoreFlag(f as u8, c, all));
            }
        }
    }
    prune(out)
}

/// Drops the nodes nothing reads (and that write nothing), renumbering the rest.
fn prune(ir: Vec<Node>) -> Vec<Node> {
    let mut live = vec![false; ir.len()];
    for k in (0..ir.len()).rev() {
        if ir[k].effect() {
            live[k] = true;
        }
        if live[k] {
            for a in ir[k].args() {
                live[a as usize] = true;
            }
        }
    }
    let mut map = vec![0 as Id; ir.len()];
    let mut out = Vec::with_capacity(ir.len());
    for (k, n) in ir.into_iter().enumerate() {
        if !live[k] {
            continue;
        }
        let r = |a: Id| map[a as usize];
        let n = match n {
            Node::Un(op, a) => Node::Un(op, r(a)),
            Node::Bin(op, a, b) => Node::Bin(op, r(a), r(b)),
            Node::Sel(a, b, c) => Node::Sel(r(a), r(b), r(c)),
            Node::Expand(a) => Node::Expand(r(a)),
            Node::S(op, a, b) => Node::S(op, r(a), r(b)),
            Node::Bits(a) => Node::Bits(r(a)),
            Node::Store(g, a, b) => Node::Store(g, r(a), r(b)),
            Node::StoreAll(g, a) => Node::StoreAll(g, r(a)),
            Node::StoreFlag(f, a, b) => Node::StoreFlag(f, r(a), r(b)),
            Node::Shuffle(a, p) => Node::Shuffle(r(a), p),
            Node::Out(o, a) => Node::Out(o, r(a)),
            n => n,
        };
        out.push(n);
        map[k] = (out.len() - 1) as Id;
    }
    out
}
