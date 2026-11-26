//! The internal instruction form: operands resolved, one variant per operation.
//!
//! Register 255 reads as zero and drops writes; predicate 7 is true.

/// A general register number; 255 is zero.
pub type Reg = u8;

/// "No register": reads zero, writes are dropped.
pub const RZ: Reg = 255;

/// An operand that may come from a register, a constant buffer or the instruction.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Src {
    Reg(Reg),
    /// c[index][byte offset]
    Cb(u8, u32),
    /// For 64-bit floats this is the high word; the low word is zero.
    Imm(u32),
}

/// Floating-point source modifier, |x| first, then -x.
#[derive(Clone, Copy, Debug, PartialEq, Default)]
pub struct Mod {
    pub abs: bool,
    pub neg: bool,
}

impl Mod {
    #[inline(always)]
    pub fn and(self) -> u32 {
        if self.abs {
            0x7fff_ffff
        } else {
            !0
        }
    }
    #[inline(always)]
    pub fn xor(self) -> u32 {
        (self.neg as u32) << 31
    }
}

/// A predicate source.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Pred {
    pub idx: u8,
    pub not: bool,
}

pub const PT: Pred = Pred { idx: 7, not: false };

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Rnd {
    /// To nearest, ties to even.
    N,
    /// Towards minus infinity.
    M,
    /// Towards plus infinity.
    P,
    /// Towards zero.
    Z,
}

/// Denormal handling of multiplications.
#[derive(Clone, Copy, Debug, PartialEq, Default)]
pub struct Fmz {
    /// Flush denormals (inputs and result) to zero.
    pub ftz: bool,
    /// Like `ftz`, and 0 * x = +0 for every x (`fmulz`).
    pub dnz: bool,
}

/// Comparison of floats, 4 bits: 1 lt, 2 eq, 4 gt, 8 true when unordered. 7 is "both are
/// numbers", 8 "one is a NaN". With flags (`cc`) the same codes test the zero and sign flags.
pub type FCond = u8;
/// Comparison of integers, 3 bits: 1 lt, 2 eq, 4 gt.
pub type ICond = u8;

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum BoolOp {
    And,
    Or,
    Xor,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Mufu {
    Cos,
    Sin,
    Ex2,
    Lg2,
    Rcp,
    Rsq,
    Rcp64h,
    Rsq64h,
    Sqrt,
}

/// Size (and sign) of a memory access.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum MemTy {
    U8,
    S8,
    U16,
    S16,
    B32,
    B64,
    B128,
}

impl MemTy {
    pub fn bytes(self) -> usize {
        match self {
            MemTy::U8 | MemTy::S8 => 1,
            MemTy::U16 | MemTy::S16 => 2,
            MemTy::B32 => 4,
            MemTy::B64 => 8,
            MemTy::B128 => 16,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum AtomOp {
    Add,
    Min,
    Max,
    Inc,
    Dec,
    And,
    Or,
    Xor,
    Exch,
    /// `a` is the expected value, `b` the new one.
    Cas,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum AtomTy {
    U32,
    S32,
    U64,
    S64,
    F32,
}

impl AtomTy {
    pub fn bytes(self) -> usize {
        match self {
            AtomTy::U64 | AtomTy::S64 => 8,
            _ => 4,
        }
    }
}

/// How IPA combines the interpolated value.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum IpaMode {
    /// The value as the rasterizer interpolates it.
    Pass,
    /// Multiplied with a register (w, to undo the division by w).
    MulW,
    /// Flat: the provoking vertex.
    Constant,
    /// Flat or smooth as the shade model says (two-sided colours).
    State,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum IpaLoc {
    Default,
    Centroid,
    /// At an offset from the pixel centre.
    Offset,
}

/// PIXLD queries.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Pixld {
    Count,
    CovMask,
    Covered,
    Offset,
    CentroidOffset,
    SampleId,
    Other(u8),
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum TexDim {
    D1,
    D2,
    D3,
    Cube,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum LodMode {
    /// From the derivatives of the quad.
    Auto,
    Zero,
    /// Automatic plus a bias.
    Bias,
    /// Given.
    Level,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum TexKind {
    /// TEX, TEXS
    Sample,
    /// TXD: explicit derivatives.
    Grad,
    /// TLD, TLDS: integer coordinates, no filtering.
    Fetch,
    /// TLD4, TLD4S
    Gather,
    /// TMML: the level of detail.
    QueryLod,
    /// TXQ
    Query,
}

/// Where the operands of a texture instruction are; computed when decoding.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Tex {
    pub kind: TexKind,
    pub dim: TexDim,
    pub array: bool,
    /// Components wanted (for `Query`, of the query's result).
    pub mask: u8,
    /// First and second destination; the first holds two components when `scalar`.
    pub d0: Reg,
    pub d1: Reg,
    pub scalar: bool,
    /// Register of the handle (bindless), or an index into the texture constant buffer.
    pub handle: Reg,
    pub bindless: bool,
    pub index: u16,
    pub coord: [Reg; 3],
    pub layer: Reg,
    pub lod_mode: LodMode,
    pub lod: Reg,
    pub shadow: bool,
    pub dref: Reg,
    pub ms: bool,
    pub sample: Reg,
    /// Register with packed offsets.
    pub offset: Reg,
    pub offset_kind: OffsetKind,
    /// Derivatives (TXD): du/dx, du/dy, dv/dx, dv/dy.
    pub deriv: Reg,
    /// Component TLD4 gathers, or the number of a TXQ.
    pub comp: u8,
    /// .NDV: derivatives from all lanes of the quad.
    pub ndv: bool,
    /// Only live lanes need a result.
    pub live_only: bool,
}

/// How texel offsets are packed.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum OffsetKind {
    None,
    /// 4 bits per axis, starting at bit 0.
    Nibbles,
    /// 4 bits per axis, starting at bit 16 (TXD).
    NibblesHigh,
    /// 8 bits per axis (TLD4).
    Bytes,
    /// Four pairs of bytes in two registers (TLD4, one offset per texel).
    PerTexel,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum SurfDim {
    D1,
    Buffer,
    D1Array,
    D2,
    D2Array,
    D3,
}

/// Operands of a surface (image) instruction.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Surf {
    pub dim: SurfDim,
    /// Raw texel bits instead of converted components.
    pub raw: bool,
    pub ty: MemTy,
    pub mask: u8,
    pub coord: Reg,
    pub handle: Src,
    /// Destination or data.
    pub r: Reg,
}

/// Address of a vertex or patch attribute.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Attr {
    /// Byte address, 4 bytes per component (0x70: position.x, 0x80: generic 0 x, ...).
    pub addr: u32,
    /// Vertex within the primitive or patch (for geometry shaders: the handle ISBERD made, or
    /// the one OUT made).
    pub vertex: u32,
    /// Per patch instead of per vertex.
    pub patch: bool,
    /// The shader's own outputs (tessellation control).
    pub output: bool,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum ShflOp {
    Idx,
    Up,
    Down,
    Bfly,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum VoteOp {
    All,
    Any,
    Eq,
}

/// Integer type of a conversion.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct IntTy {
    pub bytes: u8,
    pub signed: bool,
}

/// Arguments of the two-source compares.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct FCmpArgs {
    pub a: Reg,
    pub am: Mod,
    pub b: Src,
    pub bm: Mod,
    pub cond: FCond,
    pub ftz: bool,
    pub bop: BoolOp,
    pub p: Pred,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ICmpArgs {
    pub a: Reg,
    pub b: Src,
    pub cond: ICond,
    pub signed: bool,
    /// Continue a 64-bit comparison: use the flags of the low half's subtraction.
    pub x: bool,
    pub bop: BoolOp,
    pub p: Pred,
}

/// Jump target: a code address while decoding, an index into `Program` once linked.
pub type Target = u32;

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Op {
    Nop,
    /// Not an instruction this crate knows; running it is a fault.
    Invalid(u64),

    // 32-bit float
    FAdd { d: Reg, a: Reg, am: Mod, b: Src, bm: Mod, ftz: bool, sat: bool, cc: bool, rnd: Rnd },
    FMul { d: Reg, a: Reg, b: Src, neg: bool, fmz: Fmz, sat: bool, cc: bool, rnd: Rnd, scale: f32 },
    Ffma { d: Reg, a: Reg, b: Src, c: Src, neg_ab: bool, neg_c: bool, fmz: Fmz, sat: bool, cc: bool, rnd: Rnd },
    FMnMx { d: Reg, a: Reg, am: Mod, b: Src, bm: Mod, ftz: bool, cc: bool, p: Pred },
    FCmp { d: Reg, a: Reg, b: Src, c: Src, cond: FCond, ftz: bool },
    FSet { d: Reg, cmp: FCmpArgs, bf: bool, cc: bool },
    FSetP { pd: u8, pn: u8, cmp: FCmpArgs },
    /// Per-lane add/subtract; `ops` holds 2 bits per lane of the quad, lane 0 on top.
    FSwzAdd { d: Reg, a: Reg, b: Reg, ops: u8, ftz: bool, cc: bool, rnd: Rnd },
    Mufu { d: Reg, a: Reg, m: Mod, op: Mufu, sat: bool },
    /// Range reduction before MUFU: modelled as the identity.
    Rro { d: Reg, b: Src, m: Mod },

    // 64-bit float
    DAdd { d: Reg, a: Reg, am: Mod, b: Src, bm: Mod, cc: bool, rnd: Rnd },
    DMul { d: Reg, a: Reg, b: Src, neg: bool, cc: bool, rnd: Rnd },
    Dfma { d: Reg, a: Reg, b: Src, c: Src, neg_ab: bool, neg_c: bool, cc: bool, rnd: Rnd },
    DMnMx { d: Reg, a: Reg, am: Mod, b: Src, bm: Mod, cc: bool, p: Pred },
    DSet { d: Reg, cmp: FCmpArgs, bf: bool, cc: bool },
    DSetP { pd: u8, pn: u8, cmp: FCmpArgs },

    // conversions; sizes in bytes
    F2F { d: Reg, b: Src, m: Mod, from: u8, to: u8, rnd: Rnd, int: bool, high: bool, ftz: bool, sat: bool, cc: bool },
    F2I { d: Reg, b: Src, m: Mod, from: u8, to: IntTy, rnd: Rnd, high: bool, ftz: bool, cc: bool },
    I2F { d: Reg, b: Src, m: Mod, from: IntTy, to: u8, sel: u8, rnd: Rnd, cc: bool },
    I2I { d: Reg, b: Src, m: Mod, from: IntTy, to: IntTy, sel: u8, sat: bool, cc: bool },

    // integer
    IAdd { d: Reg, a: Reg, b: Src, neg_a: bool, neg_b: bool, x: bool, sat: bool, cc: bool },
    /// `shift`: 0 plain, 1 (a + b) >> 16, 2 (a + b) << 16; `half`: 0 word, 1 H0, 2 H1 per source.
    IAdd3 { d: Reg, a: Reg, b: Src, c: Reg, neg: [bool; 3], half: [u8; 3], shift: u8, x: bool, cc: bool },
    IScAdd { d: Reg, a: Reg, b: Src, shift: u8, neg_a: bool, neg_b: bool, cc: bool },
    /// LEA: d = (a << shift) + b; LEA.HI (`hi`: the high word's register c): d = the high word of
    /// (c:a << shift) + b. X adds the carry; the carry out goes to CC (with `cc`) and to `pd`.
    Lea { d: Reg, a: Reg, b: Src, hi: Option<Reg>, shift: u8, neg_a: bool, x: bool, cc: bool, pd: u8 },
    /// VADD (video add): an 8/16-bit part of a (`a_bits`, at byte `a_sel`) plus an 8/16/32-bit
    /// part of b, then with c: 0..=3 merged into its high half, low half, byte 0, byte 2; 4 added;
    /// 5 min; 6 max; 7 alone.
    /// `d_signed`: c and the result are signed (for accumulate, min and max).
    Vadd { d: Reg, a: Reg, a_bits: u8, a_sel: u8, a_signed: bool, b: Src, b_bits: u8, b_sel: u8, b_signed: bool, c: Reg, op: u8, d_signed: bool, cc: bool },
    /// P2R: predicates P0..P6 (or the condition code) where `b`'s low byte has bits, into byte
    /// `byte` of a copy of a.
    P2r { d: Reg, a: Reg, b: Src, cc: bool, byte: u8 },
    /// R2P: the bits of byte `byte` of a into the predicates (or the condition code) `b` selects.
    R2p { a: Reg, b: Src, cc: bool, byte: u8 },
    /// FCHK.DIVIDE: pd = whether a / b needs the exact (slow) path.
    Fchk { pd: u8, a: Reg, b: Src, abs_a: bool, neg_a: bool, abs_b: bool, neg_b: bool },
    IMul { d: Reg, a: Reg, b: Src, sa: bool, sb: bool, hi: bool, cc: bool },
    IMad { d: Reg, a: Reg, b: Src, c: Src, sa: bool, sb: bool, hi: bool, neg_ab: bool, neg_c: bool, sat: bool, x: bool, cc: bool },
    Xmad { d: Reg, a: Reg, b: Src, c: Src, ha: bool, hb: bool, sa: bool, sb: bool, psl: bool, mrg: bool, mode: u8, x: bool, cc: bool },
    /// `part`: 0 whole, 1 low, 2 middle, 3 high word of a wider minimum/maximum.
    IMnMx { d: Reg, a: Reg, b: Src, signed: bool, part: u8, cc: bool, p: Pred },
    ICmp { d: Reg, a: Reg, b: Src, c: Src, cond: ICond, signed: bool },
    ISet { d: Reg, cmp: ICmpArgs, bf: bool, cc: bool },
    ISetP { pd: u8, pn: u8, cmp: ICmpArgs },
    /// op 0 and, 1 or, 2 xor, 3 b. `test`: 0 false, 1 true, 2 zero, 3 not zero into `pd`.
    Lop { d: Reg, a: Reg, b: Src, not_a: bool, not_b: bool, op: u8, x: bool, cc: bool, test: u8, pd: u8 },
    Lop3 { d: Reg, a: Reg, b: Src, c: Reg, lut: u8, x: bool, cc: bool, test: u8, pd: u8 },
    Shl { d: Reg, a: Reg, b: Src, wrap: bool, x: bool, cc: bool },
    Shr { d: Reg, a: Reg, b: Src, signed: bool, wrap: bool, rev: bool, x: bool, cc: bool },
    Shf { d: Reg, lo: Reg, hi: Reg, b: Src, right: bool, high: bool, wrap: bool, wide: bool, signed: bool, cc: bool },
    Bfe { d: Reg, a: Reg, b: Src, signed: bool, rev: bool, cc: bool },
    Bfi { d: Reg, a: Reg, b: Src, c: Src, cc: bool },
    Flo { d: Reg, b: Src, not: bool, signed: bool, shift: bool, cc: bool },
    PopC { d: Reg, b: Src, not: bool },
    Prmt { d: Reg, a: Reg, b: Src, c: Src, mode: u8 },
    Sel { d: Reg, a: Reg, b: Src, p: Pred },
    /// `lanes`: lanes of the quad that are written.
    Mov { d: Reg, b: Src, lanes: u8 },

    // predicates
    PSet { d: Reg, a: Pred, b: Pred, c: Pred, op1: BoolOp, op2: BoolOp, bf: bool, cc: bool },
    PSetP { pd: u8, pn: u8, a: Pred, b: Pred, c: Pred, op1: BoolOp, op2: BoolOp },

    S2R { d: Reg, sys: u8 },
    Vote { d: Reg, pd: u8, p: Pred, op: VoteOp },
    Shfl { d: Reg, pd: u8, a: Reg, b: Src, c: Src, op: ShflOp },

    // memory
    Ldc { d: Reg, idx: u8, a: Reg, off: i32, ty: MemTy, mode: u8 },
    Ld { d: Reg, a: Reg, off: i32, ty: MemTy, wide: bool },
    St { r: Reg, a: Reg, off: i32, ty: MemTy, wide: bool },
    Ldl { d: Reg, a: Reg, off: i32, ty: MemTy },
    Stl { r: Reg, a: Reg, off: i32, ty: MemTy },
    Lds { d: Reg, a: Reg, off: i32, ty: MemTy },
    Sts { r: Reg, a: Reg, off: i32, ty: MemTy },
    Atom { d: Reg, a: Reg, b: Reg, off: i32, op: AtomOp, ty: AtomTy, wide: bool },
    AtomS { d: Reg, a: Reg, b: Reg, off: i32, op: AtomOp, ty: AtomTy },
    Ald { d: Reg, a: Reg, v: Reg, addr: u16, n: u8, patch: bool, output: bool },
    Ast { r: Reg, a: Reg, v: Reg, addr: u16, n: u8, patch: bool },
    Al2p { d: Reg, a: Reg, addr: u16, n: u8, output: bool },
    Isberd { d: Reg, a: Reg },
    Ipa { d: Reg, addr: u16, idx: Option<Reg>, mul: Reg, offset: Reg, mode: IpaMode, loc: IpaLoc, sat: bool },
    Pixld { d: Reg, a: Reg, what: Pixld },
    Out { d: Reg, a: Reg, b: Src, emit: bool, cut: bool },

    Tex(Tex),
    Suld(Surf),
    Sust(Surf),
    Suatom { s: Surf, b: Reg, op: AtomOp, ty: AtomTy },

    // control flow; `cond` is a flag test, 15 is "always"
    Exit { cond: u8 },
    Kil { cond: u8 },
    /// `all`: only when every running lane takes it.
    Bra { t: Target, cond: u8, all: bool },
    /// Indirect: register plus constant (code address).
    Brx { a: Reg, base: u32, cond: u8 },
    Cal { t: Target },
    Ret { cond: u8 },
    Pret { t: Target },
    Ssy { t: Target },
    Sync { cond: u8 },
    Pbk { t: Target },
    Brk { cond: u8 },
    Pcnt { t: Target },
    Cont { cond: u8 },
    /// All lanes of the quad run until `Ram`.
    Sam,
    Ram,
    Bar,
    /// Cache control and DEPBAR: nothing to do here.
    Fence,
    /// MEMBAR: the memory accesses before it are seen before those after it, by every thread.
    Membar,
    Bpt,
}

/// One decoded instruction: runs in the lanes where `pred` holds.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Inst {
    pub pred: Pred,
    pub op: Op,
}
