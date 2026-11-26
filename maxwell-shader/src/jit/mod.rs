//! A JIT for shader programs: each run of plain arithmetic (no control flow, nothing that asks
//! the `Host`) becomes one block of straight-line code.
//!
//! The instructions of a block are first lowered to a small IR whose values are vectors (one
//! 32-bit word per lane) or lane masks (one bit per lane). The lowering follows `exec` exactly,
//! instruction by instruction, so a block leaves the registers and predicates as the
//! interpreter would. Each instruction works on the lanes `active & predicate`; a lane outside
//! them keeps its registers, and an instruction with no lanes changes nothing.
//!
//! The IR then becomes code: WebAssembly with 128-bit SIMD in the browser (`wasm.rs`, one
//! module per program, for 16 lanes), or AArch64 with NEON natively on 64-bit ARM (`a64.rs`, for
//! 16 or 32 lanes, so that compute's 32-lane warps run it too). `eval` runs the IR itself: the
//! tests compare it and the backends with the interpreter.
//! Constant buffer words a block reads are fetched from the `Host` once per `run`, into the
//! frame the code reads them from.

#[cfg(target_arch = "wasm32")]
mod wasm;
#[cfg(target_arch = "wasm32")]
pub use wasm::set_reset_count;
#[cfg(all(target_arch = "aarch64", any(target_os = "macos", target_os = "linux")))]
mod a64;

/// True where there is a native backend (64-bit ARM, macOS or Linux).
pub const NATIVE: bool = cfg!(all(target_arch = "aarch64", any(target_os = "macos", target_os = "linux")));

use crate::isa::*;
use crate::program::Program;

/// Lanes of the browser's code (the GPU's graphics run 16 lanes together; compute's warps are 32,
/// which the native backend runs too).
pub const LANES: usize = 16;
/// Scratch words of a frame.
pub const SCRATCH: usize = 3 * LANES;
/// Instructions only the native backend has code for (carries, PRMT, halves).
const EXT: bool = !cfg!(target_arch = "wasm32");
/// Blocks shorter than this are left to the interpreter (in the browser: natively, a single
/// instruction is cheaper as code than as a call into the interpreter).
const MIN_BLOCK: usize = if EXT { 1 } else { 2 };
/// A block ends before its IR gets longer than this (ids are 16 bits; `forward` adds at most two
/// nodes a write and one a register).
const MAX_NODES: usize = 16384;

/// An IR value: the index of the node that makes it.
pub type Id = u16;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Un {
    /// Single-precision square root, rounding to integral values.
    Sqrt,
    Nearest,
    Floor,
    Ceil,
    Trunc,
    /// Signed and unsigned integers to single.
    SToF,
    UToF,
    /// Single to integer, truncated, saturated, NaN to 0 (`as i32`, `as u32`).
    FToS,
    FToU,
    /// 1 / sqrt(x), in double, rounded to single.
    Rsq,
    /// Denormals to zero with their sign.
    Ftz,
    /// Clamp to [0, 1], NaN to 0.
    Sat,
    /// Any NaN to the GPU's own (`fp::NAN`).
    Canon,
    /// MUFU's sine, cosine, powers and logarithms of two, and the double-precision halves: by
    /// the host's library, lane by lane.
    Sin,
    Cos,
    Ex2,
    Lg2,
    Rcp64h,
    Rsq64h,
    /// A half (in the low or high 16 bits) to single, exactly; a single to a half rounded to
    /// nearest even (NaN to 0x7e00 with the sign), in the low 16 bits. (Natively only.)
    HalfLo,
    HalfHi,
    ToHalf,
    /// Shifts by a constant (0..32).
    Shl(u8),
    ShrU(u8),
    ShrS(u8),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Bin {
    And,
    Or,
    Xor,
    Add,
    Sub,
    /// The low 32 bits of the product.
    Mul,
    FAdd,
    FMul,
    FDiv,
    /// minNum and maxNum as `fp::min` and `fp::max`.
    FMin,
    FMax,
    /// Lane by lane shifts, by the second operand's low five bits.
    Shl,
    ShrU,
    ShrS,
    /// Comparisons: all ones in a lane where true, else zero.
    FLt,
    FEq,
    FGt,
    /// Either is a NaN.
    FUnord,
    /// Signed.
    ILt,
    IEq,
    IGt,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum SOp {
    And,
    Or,
    Xor,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Node {
    /// A general register.
    Reg(Reg),
    /// The same word in every lane.
    Imm(u32),
    /// A constant buffer word: an index into `Code::cbs`.
    Cb(u16),
    Un(Un, Id),
    Bin(Bin, Id, Id),
    /// Lane by lane: where the first (a mask of all ones or zero) is set, the second, else the
    /// third.
    Sel(Id, Id, Id),
    /// a * b + c with one rounding.
    Fma(Id, Id, Id),
    /// PRMT of a and b with a selector of mode 0 (`exec::prmt_bytes`). (Natively only.)
    Prmt(Id, Id, u16),
    /// A mask of lanes (scalar) as a vector: all ones in the lanes whose bit is set.
    Expand(Id),
    /// Scalars: the lanes running, a predicate (8..11: the flags zero, sign, carry, overflow), a
    /// constant, operations.
    Active,
    Pred(u8),
    SImm(u32),
    S(SOp, Id, Id),
    /// The lanes of a vector that are not zero.
    Bits(Id),
    /// Writes a register in the lanes of the mask.
    Store(Reg, Id, Id),
    /// Writes a register in every lane.
    StoreAll(Reg, Id),
    /// Writes a predicate or a flag (value, mask).
    StoreP(u8, Id, Id),
}

impl Node {
    /// True if the node makes a scalar (a mask of lanes).
    pub fn scalar(&self) -> bool {
        matches!(self, Node::Active | Node::Pred(_) | Node::SImm(_) | Node::S(..) | Node::Bits(_))
    }
    /// True if the node makes nothing (it writes).
    pub fn effect(&self) -> bool {
        matches!(self, Node::Store(..) | Node::StoreAll(..) | Node::StoreP(..))
    }
    /// The values it reads.
    pub fn args(&self) -> impl Iterator<Item = Id> {
        let (a, n) = match *self {
            Node::Un(_, a) | Node::Expand(a) | Node::Bits(a) | Node::StoreAll(_, a) => ([a, 0, 0], 1),
            Node::Bin(_, a, b) | Node::S(_, a, b) | Node::Store(_, a, b) | Node::StoreP(_, a, b) | Node::Prmt(a, b, _) => ([a, b, 0], 2),
            Node::Sel(a, b, c) | Node::Fma(a, b, c) => ([a, b, c], 3),
            _ => ([0; 3], 0),
        };
        a.into_iter().take(n)
    }
}

/// One block: `len` instructions as IR.
#[derive(Clone, Debug)]
pub struct Block {
    pub len: u32,
    pub ir: Vec<Node>,
    /// The constant buffer words it reads (indices into `Code::cbs`).
    pub cbs: Vec<u16>,
}

/// The compiled blocks of a program.
pub struct Code {
    /// For each instruction: 1 + the block that starts there, or 0.
    start: Vec<u16>,
    pub blocks: Vec<Block>,
    /// The constant buffer words the blocks read: (buffer, byte offset).
    pub cbs: Vec<(u8, u32)>,
    #[cfg(target_arch = "wasm32")]
    wasm: Option<wasm::Module>,
    /// the native code for 16 and for 32 lanes, made when first run
    #[cfg(all(target_arch = "aarch64", any(target_os = "macos", target_os = "linux")))]
    native: [std::sync::OnceLock<Option<a64::Module>>; 2],
}

impl std::fmt::Debug for Code {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        write!(f, "Code({} blocks)", self.blocks.len())
    }
}

/// What a block reads besides the lanes: the constant buffer words of `Code::cbs`, and room
/// for the host's functions. Laid out for the generated code.
#[derive(Clone)]
#[repr(C, align(16))]
pub struct Frame {
    /// For the host's functions: three vectors in, the result back in the first.
    pub scratch: [u32; SCRATCH],
    pub cb: Vec<u32>,
    /// Where the native code keeps values it has no register for (four lanes each).
    pub spill: Vec<[u32; 4]>,
}

impl Default for Frame {
    fn default() -> Self {
        Frame { scratch: [0; SCRATCH], cb: Vec::new(), spill: Vec::new() }
    }
}

/// Held by whatever turns the JIT off and on to compare it with the interpreter (the tests
/// run at once).
pub static SWITCH: std::sync::Mutex<()> = std::sync::Mutex::new(());

static ENABLED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(true);
static BLOCKS: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);

/// Blocks compiled so far, by every program.
pub fn blocks() -> u32 {
    BLOCKS.load(std::sync::atomic::Ordering::Relaxed)
}

/// Turns the JIT on or off (it is on where there is a backend). Programs compiled before keep
/// their code but do not use it while it is off.
pub fn set_enabled(on: bool) {
    ENABLED.store(on, std::sync::atomic::Ordering::Relaxed);
}

/// On if there is a backend, not turned off, and not SBC_SHADER_JIT=0.
pub fn enabled() -> bool {
    static ENV: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    (cfg!(target_arch = "wasm32") || NATIVE)
        && ENABLED.load(std::sync::atomic::Ordering::Relaxed)
        && *ENV.get_or_init(|| std::env::var_os("SBC_SHADER_JIT").is_none_or(|v| v != "0"))
}

/// Whether there is code for `n` lanes (16 in the browser; 16 or 32 natively).
pub const fn lanes_ok(n: usize) -> bool {
    if cfg!(target_arch = "wasm32") {
        n == LANES
    } else {
        NATIVE && (n == 16 || n == 32)
    }
}

impl Code {
    /// The blocks of a program, or None if it has none worth it.
    pub fn compile(p: &Program) -> Option<Code> {
        let code = &p.code;
        // where control can arrive other than from the instruction before
        let mut entry = vec![false; code.len() + 1];
        for (k, inst) in code.iter().enumerate() {
            if let Op::Bra { t, .. } | Op::Cal { t } | Op::Pret { t } | Op::Ssy { t } | Op::Pbk { t } | Op::Pcnt { t } = inst.op {
                entry[t as usize] = true;
            }
            if lower(&mut Lower::default(), inst).is_none() {
                entry[k + 1] = true;
            }
        }
        let mut c = Code {
            start: vec![0; code.len()],
            blocks: Vec::new(),
            cbs: Vec::new(),
            #[cfg(target_arch = "wasm32")]
            wasm: None,
            #[cfg(all(target_arch = "aarch64", any(target_os = "macos", target_os = "linux")))]
            native: Default::default(),
        };
        let mut k = 0;
        while k < code.len() {
            let mut low = Lower { cbs: std::mem::take(&mut c.cbs), ..Lower::default() };
            let mut n = 0;
            while k + n < code.len() && (n == 0 || !entry[k + n]) && low.ir.len() < MAX_NODES && lower(&mut low, &code[k + n]).is_some() {
                n += 1;
            }
            if n >= MIN_BLOCK && c.blocks.len() < u16::MAX as usize - 1 {
                c.start[k] = c.blocks.len() as u16 + 1;
                let ir = forward(&low.ir);
                // (as long as its ids fit)
                let ir = if EXT { fold(&ir).filter(|f| f.len() < Id::MAX as usize).unwrap_or(ir) } else { ir };
                let mut cbs: Vec<u16> = ir.iter().filter_map(|n| if let Node::Cb(i) = n { Some(*i) } else { None }).collect();
                cbs.sort_unstable();
                cbs.dedup();
                c.blocks.push(Block { len: n as u32, ir, cbs });
            }
            c.cbs = low.cbs;
            k += n.max(1);
        }
        if c.blocks.is_empty() {
            return None;
        }
        #[cfg(target_arch = "wasm32")]
        {
            c.wasm = Some(wasm::Module::new(&c)?);
        }
        BLOCKS.fetch_add(c.blocks.len() as u32, std::sync::atomic::Ordering::Relaxed);
        Some(c)
    }

    /// The block that starts at instruction `pc`.
    #[inline(always)]
    pub fn block_at(&self, pc: u32) -> Option<(u16, u32)> {
        match self.start.get(pc as usize) {
            Some(&b) if b != 0 => Some((b - 1, self.blocks[b as usize - 1].len)),
            _ => None,
        }
    }

    /// Fetches the constant buffer words block `b` reads into the frame (when the block is
    /// entered, as the interpreter would read them).
    pub fn fill(&self, b: u16, frame: &mut Frame, cbuf: impl Fn(u32, u32) -> u32) {
        frame.cb.resize(self.cbs.len(), 0);
        for &k in &self.blocks[b as usize].cbs {
            let (i, o) = self.cbs[k as usize];
            frame.cb[k as usize] = cbuf(i as u32, o);
        }
    }

    /// Fetches the words of every block (constant buffers do not change while a program runs).
    pub fn fill_all(&self, frame: &mut Frame, cbuf: impl Fn(u32, u32) -> u32) {
        frame.cb.clear();
        frame.cb.extend(self.cbs.iter().map(|&(i, o)| cbuf(i as u32, o)));
    }

    /// Runs the program's native code from the lanes' instruction, if it has code there: until
    /// the lanes stop (Some(Some(why))), or until they reach what is left to the interpreter
    /// (Some(None): it goes on from the lanes' instruction). None: no native code here.
    pub fn enter<const N: usize, H: crate::host::Host<N>>(&self, p: &Program, l: &mut crate::exec::Lanes<N>, h: &mut H, fuel: &mut u64) -> Option<Option<crate::exec::Exit>> {
        #[cfg(all(target_arch = "aarch64", any(target_os = "macos", target_os = "linux")))]
        {
            let k = match N {
                16 => 0,
                32 => 1,
                _ => return None,
            };
            if let Some(m) = self.native[k].get_or_init(|| a64::Module::new(self, N)) {
                let r = m.enter(self, p, l, h, fuel);
                if r.is_some() {
                    RUNS.with(|r| r.set(r.get() + 1));
                }
                return r;
            }
        }
        let _ = (p, l, h, fuel);
        None
    }

    /// Runs block `b` on the lanes (`fill` first). False if it cannot be run here (then nothing
    /// changed).
    pub fn run<const N: usize>(&self, b: u16, gpr: &mut [[u32; N]; 256], pred: &mut [u32; 8], cc: &mut [u32; 4], frame: &mut Frame, active: u32) -> bool {
        #[cfg(target_arch = "wasm32")]
        if let Some(w) = &self.wasm {
            let Some(gpr) = (gpr as &mut dyn std::any::Any).downcast_mut::<[[u32; LANES]; 256]>() else {
                return false;
            };
            let ran = w.run(b, gpr, pred, frame, active);
            if ran {
                RUNS.with(|r| r.set(r.get() + 1));
            }
            return ran;
        }
        #[cfg(all(target_arch = "aarch64", any(target_os = "macos", target_os = "linux")))]
        {
            let k = match N {
                16 => 0,
                32 => 1,
                _ => return false,
            };
            if let Some(m) = self.native[k].get_or_init(|| a64::Module::new(self, N)) {
                let ran = m.run(b, gpr.as_mut_ptr() as *mut u32, pred.as_mut_ptr(), cc.as_mut_ptr(), frame, active);
                if ran {
                    RUNS.with(|r| r.set(r.get() + 1));
                }
                return ran;
            }
        }
        let _ = (b, gpr, pred, cc, frame, active);
        false
    }
}

thread_local! {
    /// (per thread: the GPU's threads would all write one line)
    static RUNS: std::cell::Cell<u64> = const { std::cell::Cell::new(0) };
}

/// Blocks run by compiled code so far on this thread (natively: entries into a program's code).
pub fn runs() -> u64 {
    RUNS.with(|r| r.get())
}

/// Programs compiled, by their code: the GPU decodes its programs again after each submission
/// (memory may have changed), and the same code must not be compiled (and given a table slot)
/// again each time.
type Key = (u32, Vec<(u32, u64)>, Vec<(u32, u64)>);
static CACHE: std::sync::Mutex<Option<std::collections::HashMap<Key, Option<std::sync::Arc<Code>>>>> = std::sync::Mutex::new(None);
/// Programs kept; past that the cache starts again (the page frees the slots of what was
/// dropped when the CPU's JIT next starts over).
const CACHE_MAX: usize = 4096;

/// The code of a program: from the cache, or compiled.
pub fn code_for(p: &Program) -> Option<std::sync::Arc<Code>> {
    // the whole of each word: SM70 instructions differing only in their upper half (a
    // register, a compare, a LOP3 table) must not share code
    let key = (p.entry, p.words.clone(), p.high.clone());
    let mut cache = CACHE.lock().unwrap_or_else(|e| e.into_inner());
    let map = cache.get_or_insert_with(Default::default);
    if let Some(c) = map.get(&key) {
        return c.clone();
    }
    if map.len() >= CACHE_MAX {
        map.clear();
    }
    let c = Code::compile(p).map(std::sync::Arc::new);
    map.insert(key, c.clone());
    c
}

/// The IR of instructions, being built.
#[derive(Default)]
struct Lower {
    ir: Vec<Node>,
    cbs: Vec<(u8, u32)>,
    /// The block's `Active` node, once made.
    active: Option<Id>,
}

impl Lower {
    fn node(&mut self, n: Node) -> Id {
        self.ir.push(n);
        (self.ir.len() - 1) as Id
    }
    fn imm(&mut self, v: u32) -> Id {
        self.node(Node::Imm(v))
    }
    fn reg(&mut self, r: Reg) -> Id {
        // (RZ too: the interpreter reads its register, which stays zero)
        self.node(Node::Reg(r))
    }
    fn src(&mut self, s: Src) -> Id {
        match s {
            Src::Reg(r) => self.reg(r),
            Src::Imm(v) => self.imm(v),
            Src::Cb(i, o) => {
                let k = match self.cbs.iter().position(|&c| c == (i, o)) {
                    Some(k) => k,
                    None => {
                        self.cbs.push((i, o));
                        self.cbs.len() - 1
                    }
                };
                self.node(Node::Cb(k as u16))
            }
        }
    }
    fn un(&mut self, op: Un, a: Id) -> Id {
        self.node(Node::Un(op, a))
    }
    fn bin(&mut self, op: Bin, a: Id, b: Id) -> Id {
        self.node(Node::Bin(op, a, b))
    }
    fn bin_imm(&mut self, op: Bin, a: Id, v: u32) -> Id {
        let b = self.imm(v);
        self.bin(op, a, b)
    }
    fn sel(&mut self, m: Id, a: Id, b: Id) -> Id {
        self.node(Node::Sel(m, a, b))
    }
    fn sop(&mut self, op: SOp, a: Id, b: Id) -> Id {
        self.node(Node::S(op, a, b))
    }
    fn snot(&mut self, a: Id) -> Id {
        let all = self.node(Node::SImm(!0));
        self.sop(SOp::Xor, a, all)
    }
    fn simm(&mut self, v: u32) -> Id {
        self.node(Node::SImm(v))
    }
    /// A predicate as a mask of lanes.
    fn p(&mut self, p: Pred) -> Id {
        let v = if p.idx & 7 == 7 { self.simm(!0) } else { self.node(Node::Pred(p.idx & 7)) };
        if p.not {
            self.snot(v)
        } else {
            v
        }
    }
    /// Operand modifiers of a float: flush, absolute value, negation.
    fn modf(&mut self, v: Id, m: Mod, ftz: bool) -> Id {
        let mut v = if ftz { self.un(Un::Ftz, v) } else { v };
        if m.and() != !0 {
            v = self.bin_imm(Bin::And, v, m.and());
        }
        if m.xor() != 0 {
            v = self.bin_imm(Bin::Xor, v, m.xor());
        }
        v
    }
    fn finish(&mut self, v: Id, ftz: bool, sat: bool) -> Id {
        let v = self.un(Un::Canon, v);
        let v = if ftz { self.un(Un::Ftz, v) } else { v };
        if sat {
            self.un(Un::Sat, v)
        } else {
            v
        }
    }
    fn put(&mut self, d: Reg, m: Id, v: Id) {
        if d != RZ {
            self.node(Node::Store(d, v, m));
        }
    }
    /// Writes a predicate (or, 8..11, a flag).
    fn putp(&mut self, p: u8, m: Id, v: Id) {
        if p != 7 {
            self.node(Node::StoreP(if p < 8 { p & 7 } else { p }, v, m));
        }
    }
    fn expand(&mut self, s: Id) -> Id {
        self.node(Node::Expand(s))
    }
    /// All ones or zero (or 1.0 and 0.0) from a mask.
    fn bools(&mut self, r: Id, float: bool) -> Id {
        let m = self.expand(r);
        let t = self.imm(if float { 0x3f80_0000 } else { !0 });
        let z = self.imm(0);
        self.sel(m, t, z)
    }
    fn bool_op(&mut self, op: BoolOp, a: Id, b: Id) -> Id {
        let op = match op {
            BoolOp::And => SOp::And,
            BoolOp::Or => SOp::Or,
            BoolOp::Xor => SOp::Xor,
        };
        self.sop(op, a, b)
    }
    /// The lanes where a float comparison holds.
    fn cmp32(&mut self, cond: FCond, a: Id, b: Id) -> Id {
        let mut m = None;
        for (bit, op) in [(1, Bin::FLt), (2, Bin::FEq), (4, Bin::FGt), (8, Bin::FUnord)] {
            if cond & bit != 0 {
                let t = self.bin(op, a, b);
                m = Some(match m {
                    Some(x) => self.bin(Bin::Or, x, t),
                    None => t,
                });
            }
        }
        match m {
            Some(v) => self.node(Node::Bits(v)),
            None => self.simm(0),
        }
    }
    /// The lanes where an integer comparison holds.
    fn cmpi(&mut self, cond: ICond, signed: bool, a: Id, b: Id) -> Id {
        let (a, b) = if signed {
            (a, b)
        } else {
            (self.bin_imm(Bin::Xor, a, 0x8000_0000), self.bin_imm(Bin::Xor, b, 0x8000_0000))
        };
        let mut m = None;
        for (bit, op) in [(1, Bin::ILt), (2, Bin::IEq), (4, Bin::IGt)] {
            if cond & bit != 0 {
                let t = self.bin(op, a, b);
                m = Some(match m {
                    Some(x) => self.bin(Bin::Or, x, t),
                    None => t,
                });
            }
        }
        match m {
            Some(v) => self.node(Node::Bits(v)),
            None => self.simm(0),
        }
    }
    /// Lanes where `v` is zero, as a vector mask.
    fn is_zero(&mut self, v: Id) -> Id {
        let z = self.imm(0);
        self.bin(Bin::IEq, v, z)
    }
    /// Shift by an operand, as `exec::shift`: wrapped to five bits, or zero (`fill`) from 32 on.
    fn shift(&mut self, op: Bin, u: Id, b: Src, wrap: bool, fill: Id) -> Id {
        if let Src::Imm(n) = b {
            let n = if wrap { n & 31 } else { n };
            if n >= 32 {
                return fill;
            }
            let c = match op {
                Bin::Shl => Un::Shl(n as u8),
                Bin::ShrU => Un::ShrU(n as u8),
                _ => Un::ShrS(n as u8),
            };
            return self.un(c, u);
        }
        let v = self.src(b);
        let r = self.bin(op, u, v);
        if wrap {
            return r;
        }
        let high = self.un(Un::ShrU(5), v);
        let small = self.is_zero(high);
        self.sel(small, r, fill)
    }
    /// A 16-bit half of a word for XMAD, sign-extended if `signed`.
    fn half(&mut self, x: Id, hi: bool, signed: bool) -> Id {
        match (hi, signed) {
            (true, true) => self.un(Un::ShrS(16), x),
            (true, false) => self.un(Un::ShrU(16), x),
            (false, true) => {
                let t = self.un(Un::Shl(16), x);
                self.un(Un::ShrS(16), t)
            }
            (false, false) => self.bin_imm(Bin::And, x, 0xffff),
        }
    }
}

/// Lowers one instruction (appended to `l`), or None if blocks cannot have it (then `l` may
/// have nodes of it: the caller drops the block's IR from there, or, in `compile`, stops).
fn lower(l: &mut Lower, inst: &Inst) -> Option<()> {
    let mark = (l.ir.len(), l.cbs.len());
    let r = lower_op(l, inst);
    if r.is_none() {
        l.ir.truncate(mark.0);
        l.cbs.truncate(mark.1);
        l.active = l.active.filter(|&a| (a as usize) < mark.0);
    }
    r
}

fn lower_op(l: &mut Lower, inst: &Inst) -> Option<()> {
    // the lanes of the instruction
    let m = {
        let a = match l.active {
            Some(a) => a,
            None => {
                let a = l.node(Node::Active);
                l.active = Some(a);
                a
            }
        };
        if inst.pred.idx & 7 == 7 && !inst.pred.not {
            a
        } else {
            let p = l.p(inst.pred);
            l.sop(SOp::And, a, p)
        }
    };
    let f0 = 0f32.to_bits();
    match inst.op {
        Op::Nop | Op::Fence => {}
        Op::FAdd { d, a, am, b, bm, ftz, sat, cc: false, rnd: Rnd::N } => {
            let x = l.reg(a);
            let x = l.modf(x, am, ftz);
            let y = l.src(b);
            let y = l.modf(y, bm, ftz);
            let v = l.bin(Bin::FAdd, x, y);
            let v = l.finish(v, ftz, sat);
            l.put(d, m, v);
        }
        Op::FMul { d, a, b, neg, fmz, sat, cc: false, rnd: Rnd::N, scale } => {
            let flush = fmz.ftz || fmz.dnz;
            let x = l.reg(a);
            let x = l.modf(x, Mod { abs: false, neg }, flush);
            let y = l.src(b);
            let y = l.modf(y, Mod::default(), flush);
            let mut v = l.bin(Bin::FMul, x, y);
            if scale != 1.0 {
                v = l.bin_imm(Bin::FMul, v, scale.to_bits());
            }
            if fmz.dnz {
                let zx = l.bin_imm(Bin::FEq, x, f0);
                let zy = l.bin_imm(Bin::FEq, y, f0);
                let z = l.bin(Bin::Or, zx, zy);
                let zero = l.imm(0);
                v = l.sel(z, zero, v);
            }
            let v = l.finish(v, flush, sat);
            l.put(d, m, v);
        }
        Op::Ffma { d, a, b, c, neg_ab, neg_c, fmz, sat, cc: false, rnd: Rnd::N } => {
            let flush = fmz.ftz || fmz.dnz;
            let x = l.reg(a);
            let x = l.modf(x, Mod { abs: false, neg: neg_ab }, flush);
            let y = l.src(b);
            let y = l.modf(y, Mod::default(), flush);
            let z = l.src(c);
            let z = l.modf(z, Mod { abs: false, neg: neg_c }, flush);
            let mut v = l.node(Node::Fma(x, y, z));
            if fmz.dnz {
                let zx = l.bin_imm(Bin::FEq, x, f0);
                let zy = l.bin_imm(Bin::FEq, y, f0);
                let t = l.bin(Bin::Or, zx, zy);
                v = l.sel(t, z, v);
            }
            let v = l.finish(v, flush, sat);
            l.put(d, m, v);
        }
        Op::FMnMx { d, a, am, b, bm, ftz, cc: false, p } => {
            let x = l.reg(a);
            let x = l.modf(x, am, ftz);
            let y = l.src(b);
            let y = l.modf(y, bm, ftz);
            let s = l.p(p);
            let s = l.expand(s);
            let lo = l.bin(Bin::FMin, x, y);
            let hi = l.bin(Bin::FMax, x, y);
            let v = l.sel(s, lo, hi);
            l.put(d, m, v);
        }
        Op::FCmp { d, a, b, c, cond, ftz } => {
            let z = l.src(c);
            let z = l.modf(z, Mod::default(), ftz);
            let zero = l.imm(f0);
            let t = l.cmp32(cond, z, zero);
            let t = l.expand(t);
            let x = l.reg(a);
            let y = l.src(b);
            let v = l.sel(t, x, y);
            l.put(d, m, v);
        }
        Op::FSet { d, cmp, bf, cc: false } => {
            let t = fcmp(l, &cmp);
            let q = l.p(cmp.p);
            let r = l.bool_op(cmp.bop, t, q);
            let v = l.bools(r, bf);
            l.put(d, m, v);
        }
        Op::FSetP { pd, pn, cmp } => {
            let t = fcmp(l, &cmp);
            let q = l.p(cmp.p);
            let a = l.bool_op(cmp.bop, t, q);
            let nt = l.snot(t);
            let b = l.bool_op(cmp.bop, nt, q);
            l.putp(pd, m, a);
            l.putp(pn, m, b);
        }
        Op::Mufu { d, a, m: md, op, sat } => {
            let x = l.reg(a);
            let x = l.modf(x, md, false);
            let v = match op {
                Mufu::Rcp => {
                    let one = l.imm(1f32.to_bits());
                    l.bin(Bin::FDiv, one, x)
                }
                Mufu::Sqrt => l.un(Un::Sqrt, x),
                Mufu::Rsq => l.un(Un::Rsq, x),
                Mufu::Sin => l.un(Un::Sin, x),
                Mufu::Cos => l.un(Un::Cos, x),
                Mufu::Ex2 => l.un(Un::Ex2, x),
                Mufu::Lg2 => l.un(Un::Lg2, x),
                Mufu::Rcp64h => l.un(Un::Rcp64h, x),
                Mufu::Rsq64h => l.un(Un::Rsq64h, x),
            };
            let v = if matches!(op, Mufu::Rcp64h | Mufu::Rsq64h) { v } else { l.un(Un::Canon, v) };
            let v = if sat { l.un(Un::Sat, v) } else { v };
            l.put(d, m, v);
        }
        Op::Rro { d, b, m: md } => {
            let v = l.src(b);
            let v = l.modf(v, md, false);
            l.put(d, m, v);
        }
        Op::F2F { from: 4, to: 4, d, b, m: md, rnd, int, ftz, sat, cc: false, .. } => {
            let v = l.src(b);
            let v = l.modf(v, md, ftz);
            let v = if int { round(l, v, rnd) } else { v };
            let v = l.finish(v, ftz, sat);
            l.put(d, m, v);
        }
        Op::F2I { from: 4, to: IntTy { bytes: 4, signed }, d, b, m: md, rnd, ftz, cc: false, .. } => {
            let v = l.src(b);
            let v = l.modf(v, md, ftz);
            let v = if rnd == Rnd::Z { v } else { round(l, v, rnd) };
            let v = l.un(if signed { Un::FToS } else { Un::FToU }, v);
            l.put(d, m, v);
        }
        Op::I2F { from: IntTy { bytes: 4, signed }, to: 4, d, b, m: Mod { abs: false, neg: false }, rnd: Rnd::N, cc: false, .. } => {
            let v = l.src(b);
            let v = l.un(if signed { Un::SToF } else { Un::UToF }, v);
            l.put(d, m, v);
        }
        Op::IAdd { d, a, b, neg_a, neg_b, x, sat: false, cc } if EXT && (x || cc) => {
            // as exec: the carry in is the flag (.X), or one for a negation; with .CC the flags
            // say zero, sign, the carry out of the 33-bit sum, and signed overflow
            let po = neg_a && neg_b && !x;
            let mut u = l.reg(a);
            let mut v = l.src(b);
            if neg_a && !po {
                u = l.bin_imm(Bin::Xor, u, !0);
            }
            if neg_b && !po {
                v = l.bin_imm(Bin::Xor, v, !0);
            }
            let s = l.bin(Bin::Add, u, v);
            let r = if x {
                let c = l.node(Node::Pred(10));
                let c = l.expand(c);
                let one = l.bin_imm(Bin::And, c, 1);
                l.bin(Bin::Add, s, one)
            } else if neg_a || neg_b {
                l.bin_imm(Bin::Add, s, 1)
            } else {
                s
            };
            if cc {
                let below = |l: &mut Lower, a: Id, b: Id| {
                    let a = l.bin_imm(Bin::Xor, a, 0x8000_0000);
                    let b = l.bin_imm(Bin::Xor, b, 0x8000_0000);
                    l.bin(Bin::ILt, a, b)
                };
                let c1 = below(l, s, u);
                let c2 = below(l, r, s);
                let carry = l.bin(Bin::Or, c1, c2);
                let ur = l.bin(Bin::Xor, u, r);
                let vr = l.bin(Bin::Xor, v, r);
                let o = l.bin(Bin::And, ur, vr);
                let zero = l.imm(0);
                let o = l.bin(Bin::ILt, o, zero);
                let z = l.bin(Bin::IEq, r, zero);
                let sg = l.bin(Bin::ILt, r, zero);
                for (f, t) in [(8, z), (9, sg), (10, carry), (11, o)] {
                    let b = l.node(Node::Bits(t));
                    l.putp(f, m, b);
                }
            }
            l.put(d, m, r);
        }
        Op::Prmt { d, a, b: Src::Imm(sel), c, mode } if EXT => {
            let x = l.reg(a);
            let y = l.src(c);
            let v = l.node(Node::Prmt(x, y, crate::exec::prmt_bytes(sel, mode) as u16));
            l.put(d, m, v);
        }
        Op::F2F { from: 2, to: 4, d, b, m: md, int: false, high, ftz, sat, cc: false, .. } if EXT => {
            let v = l.src(b);
            let v = l.un(if high { Un::HalfHi } else { Un::HalfLo }, v);
            let v = l.modf(v, md, false);
            let v = if sat { l.un(Un::Sat, v) } else { v };
            let v = l.finish(v, ftz, false);
            l.put(d, m, v);
        }
        Op::F2F { from: 4, to: 2, d, b, m: md, rnd: Rnd::N, int: false, high, ftz, sat, cc: false } if EXT => {
            let v = l.src(b);
            let v = l.modf(v, md, ftz);
            let v = if sat { l.un(Un::Sat, v) } else { v };
            let v = l.un(Un::ToHalf, v);
            let v = if high { l.un(Un::Shl(16), v) } else { v };
            l.put(d, m, v);
        }
        Op::IAdd { d, a, b, neg_a, neg_b, x: false, sat: false, cc: false } => {
            let po = neg_a && neg_b;
            let mut u = l.reg(a);
            let mut v = l.src(b);
            if neg_a && !po {
                u = l.bin_imm(Bin::Xor, u, !0);
            }
            if neg_b && !po {
                v = l.bin_imm(Bin::Xor, v, !0);
            }
            let mut r = l.bin(Bin::Add, u, v);
            if neg_a || neg_b {
                r = l.bin_imm(Bin::Add, r, 1);
            }
            l.put(d, m, r);
        }
        Op::IScAdd { d, a, b, shift, neg_a, neg_b, cc: false } => {
            let u = l.reg(a);
            let v = l.src(b);
            let x = l.un(Un::Shl(shift & 31), u);
            let r = if neg_a && neg_b {
                let s = l.bin(Bin::Add, x, v);
                l.bin_imm(Bin::Add, s, 1)
            } else if neg_a {
                l.bin(Bin::Sub, v, x)
            } else if neg_b {
                l.bin(Bin::Sub, x, v)
            } else {
                l.bin(Bin::Add, x, v)
            };
            l.put(d, m, r);
        }
        Op::IMul { d, a, b, hi: false, cc: false, .. } => {
            let u = l.reg(a);
            let v = l.src(b);
            let r = l.bin(Bin::Mul, u, v);
            l.put(d, m, r);
        }
        Op::Xmad { d, a, b, c, ha, hb, sa, sb, psl, mrg, mode, x: false, cc: false } => {
            let u = l.reg(a);
            let v = l.src(b);
            let w = l.src(c);
            let hu = l.half(u, ha, sa);
            let hv = l.half(v, hb, sb);
            let mut t = l.bin(Bin::Mul, hu, hv);
            if psl {
                t = l.un(Un::Shl(16), t);
            }
            let z = match mode {
                1 => l.bin_imm(Bin::And, w, 0xffff),
                2 => l.un(Un::ShrU(16), w),
                3 => {
                    // corrects an unsigned product into a signed one
                    let mut z = w;
                    for (x, hx, y) in [(u, ha, v), (v, hb, u)] {
                        let h = l.half(x, hx, false);
                        let top = l.bin_imm(Bin::And, h, 0x8000);
                        let clear = l.is_zero(top);
                        let ys = l.un(Un::Shl(16), y);
                        let less = l.bin(Bin::Sub, z, ys);
                        z = l.sel(clear, z, less);
                    }
                    z
                }
                4 => {
                    let vs = l.un(Un::Shl(16), v);
                    l.bin(Bin::Add, w, vs)
                }
                _ => w,
            };
            let mut r = l.bin(Bin::Add, t, z);
            if mrg {
                let lo = l.bin_imm(Bin::And, r, 0xffff);
                let vs = l.un(Un::Shl(16), v);
                r = l.bin(Bin::Or, lo, vs);
            }
            l.put(d, m, r);
        }
        Op::ICmp { d, a, b, c, cond, signed } => {
            let z = l.src(c);
            let zero = l.imm(0);
            let t = l.cmpi(cond, signed, z, zero);
            let t = l.expand(t);
            let x = l.reg(a);
            let y = l.src(b);
            let v = l.sel(t, x, y);
            l.put(d, m, v);
        }
        Op::ISet { d, cmp: cmp @ ICmpArgs { x: false, .. }, bf, cc: false } => {
            let a = l.reg(cmp.a);
            let b = l.src(cmp.b);
            let t = l.cmpi(cmp.cond, cmp.signed, a, b);
            let q = l.p(cmp.p);
            let r = l.bool_op(cmp.bop, t, q);
            let v = l.bools(r, bf);
            l.put(d, m, v);
        }
        Op::ISetP { pd, pn, cmp: cmp @ ICmpArgs { x: false, .. } } => {
            let a = l.reg(cmp.a);
            let b = l.src(cmp.b);
            let t = l.cmpi(cmp.cond, cmp.signed, a, b);
            let q = l.p(cmp.p);
            let r = l.bool_op(cmp.bop, t, q);
            let nt = l.snot(t);
            let s = l.bool_op(cmp.bop, nt, q);
            l.putp(pd, m, r);
            l.putp(pn, m, s);
        }
        Op::Lop { d, a, b, not_a, not_b, op, cc: false, test, pd, .. } => {
            let mut u = l.reg(a);
            let mut v = l.src(b);
            if not_b {
                v = l.bin_imm(Bin::Xor, v, !0);
            }
            let r = match op {
                0..=2 => {
                    if not_a {
                        u = l.bin_imm(Bin::Xor, u, !0);
                    }
                    l.bin([Bin::And, Bin::Or, Bin::Xor][op as usize], u, v)
                }
                _ => v,
            };
            lop_pred(l, m, r, test, pd);
            l.put(d, m, r);
        }
        Op::Lop3 { d, a, b, c, lut, cc: false, test, pd, .. } => {
            let u = l.reg(a);
            let v = l.src(b);
            let w = l.reg(c);
            let not = [u, v, w].map(|x| l.bin_imm(Bin::Xor, x, !0));
            let mut r = l.imm(0);
            for k in 0..8u8 {
                if lut >> k & 1 != 0 {
                    let t = |x: usize| if k >> (2 - x) & 1 != 0 { [u, v, w][x] } else { not[x] };
                    let (x, y, z) = (t(0), t(1), t(2));
                    let xy = l.bin(Bin::And, x, y);
                    let xyz = l.bin(Bin::And, xy, z);
                    r = l.bin(Bin::Or, r, xyz);
                }
            }
            lop_pred(l, m, r, test, pd);
            l.put(d, m, r);
        }
        Op::Shl { d, a, b, wrap, cc: false, .. } => {
            let u = l.reg(a);
            let zero = l.imm(0);
            let r = l.shift(Bin::Shl, u, b, wrap, zero);
            l.put(d, m, r);
        }
        Op::Shr { d, a, b, signed, wrap, rev: false, cc: false, .. } => {
            let u = l.reg(a);
            let fill = if signed { l.un(Un::ShrS(31), u) } else { l.imm(0) };
            let r = l.shift(if signed { Bin::ShrS } else { Bin::ShrU }, u, b, wrap, fill);
            l.put(d, m, r);
        }
        Op::Sel { d, a, b, p } => {
            let t = l.p(p);
            let t = l.expand(t);
            let x = l.reg(a);
            let y = l.src(b);
            let v = l.sel(t, x, y);
            l.put(d, m, v);
        }
        Op::Mov { d, b, lanes } => {
            let v = l.src(b);
            let m = if lanes == 15 {
                m
            } else {
                let q = l.simm(lanes as u32 * 0x1111_1111);
                l.sop(SOp::And, m, q)
            };
            l.put(d, m, v);
        }
        Op::PSet { d, a, b, c, op1, op2, bf, cc: false } => {
            let (pa, pb, pc) = (l.p(a), l.p(b), l.p(c));
            let t = l.bool_op(op1, pa, pb);
            let r = l.bool_op(op2, t, pc);
            let v = l.bools(r, bf);
            l.put(d, m, v);
        }
        Op::PSetP { pd, pn, a, b, c, op1, op2 } => {
            let (pa, pb, q) = (l.p(a), l.p(b), l.p(c));
            let t = l.bool_op(op1, pa, pb);
            let r = l.bool_op(op2, t, q);
            let nt = l.snot(t);
            let s = l.bool_op(op2, nt, q);
            l.putp(pd, m, r);
            l.putp(pn, m, s);
        }
        _ => return None,
    }
    Some(())
}

fn fcmp(l: &mut Lower, c: &FCmpArgs) -> Id {
    let a = l.reg(c.a);
    let a = l.modf(a, c.am, c.ftz);
    let b = l.src(c.b);
    let b = l.modf(b, c.bm, c.ftz);
    l.cmp32(c.cond, a, b)
}

fn round(l: &mut Lower, v: Id, rnd: Rnd) -> Id {
    let op = match rnd {
        Rnd::N => Un::Nearest,
        Rnd::M => Un::Floor,
        Rnd::P => Un::Ceil,
        Rnd::Z => Un::Trunc,
    };
    l.un(op, v)
}

/// The predicate LOP and LOP3 may write: never, always, where zero, where not zero.
fn lop_pred(l: &mut Lower, m: Id, r: Id, test: u8, pd: u8) {
    if pd == 7 {
        return;
    }
    let v = match test {
        0 => l.simm(0),
        1 => l.simm(!0),
        _ => {
            let z = l.is_zero(r);
            let z = l.node(Node::Bits(z));
            if test == 2 {
                z
            } else {
                l.snot(z)
            }
        }
    };
    l.putp(pd, m, v);
}

/// The host's part of the unary operations that are not instructions: a lane at a time.
pub fn unary(op: Un, x: u32) -> u32 {
    let f = f32::from_bits(x);
    let wide = |x: f32| f64::from_bits((x.to_bits() as u64) << 32);
    match op {
        Un::Sqrt => f.sqrt().to_bits(),
        Un::Nearest => f.round_ties_even().to_bits(),
        Un::Floor => f.floor().to_bits(),
        Un::Ceil => f.ceil().to_bits(),
        Un::Trunc => f.trunc().to_bits(),
        Un::SToF => (x as i32 as f32).to_bits(),
        Un::UToF => (x as f32).to_bits(),
        Un::FToS => f as i32 as u32,
        Un::FToU => f as u32,
        Un::Rsq => ((1.0 / (f as f64).sqrt()) as f32).to_bits(),
        Un::Ftz => crate::fp::ftz(x),
        Un::Sat => crate::fp::sat(f).to_bits(),
        Un::Canon => crate::fp::canon(x),
        Un::Sin => f.sin().to_bits(),
        Un::Cos => f.cos().to_bits(),
        Un::Ex2 => f.exp2().to_bits(),
        Un::Lg2 => f.log2().to_bits(),
        Un::Rcp64h => ((1.0 / wide(f)).to_bits() >> 32) as u32,
        Un::Rsq64h => ((1.0 / wide(f).sqrt()).to_bits() >> 32) as u32,
        Un::HalfLo => crate::fp::half_to_f32(x as u16).to_bits(),
        Un::HalfHi => crate::fp::half_to_f32((x >> 16) as u16).to_bits(),
        Un::ToHalf => crate::fp::f32_to_half(f, Rnd::N) as u32,
        Un::Shl(n) => x << n,
        Un::ShrU(n) => x >> n,
        Un::ShrS(n) => (x as i32 >> n) as u32,
    }
}

/// The same for the binary ones.
pub fn binary(op: Bin, x: u32, y: u32) -> u32 {
    let (f, g) = (f32::from_bits(x), f32::from_bits(y));
    let t = |b: bool| if b { !0 } else { 0 };
    match op {
        Bin::And => x & y,
        Bin::Or => x | y,
        Bin::Xor => x ^ y,
        Bin::Add => x.wrapping_add(y),
        Bin::Sub => x.wrapping_sub(y),
        Bin::Mul => x.wrapping_mul(y),
        Bin::FAdd => (f + g).to_bits(),
        Bin::FMul => (f * g).to_bits(),
        Bin::FDiv => (f / g).to_bits(),
        Bin::FMin => crate::fp::min(f, g).to_bits(),
        Bin::FMax => crate::fp::max(f, g).to_bits(),
        Bin::Shl => x << (y & 31),
        Bin::ShrU => x >> (y & 31),
        Bin::ShrS => (x as i32 >> (y & 31)) as u32,
        Bin::FLt => t(f < g),
        Bin::FEq => t(f == g),
        Bin::FGt => t(f > g),
        Bin::FUnord => t(f.is_nan() || g.is_nan()),
        Bin::ILt => t((x as i32) < y as i32),
        Bin::IEq => t(x == y),
        Bin::IGt => t(x as i32 > y as i32),
    }
}

/// Runs a block's IR (for the tests, and as the reference of the backends).
pub fn eval<const N: usize>(b: &Block, gpr: &mut [[u32; N]; 256], pred: &mut [u32; 8], cc: &mut [u32; 4], cb: &[u32], active: u32) {
    let mut val = vec![[0u32; N]; b.ir.len()];
    let mut s = vec![0u32; b.ir.len()];
    for (k, n) in b.ir.iter().enumerate() {
        let v = |i: Id| val[i as usize];
        match *n {
            Node::Reg(r) => val[k] = gpr[r as usize],
            Node::Imm(x) => val[k] = [x; N],
            Node::Cb(i) => val[k] = [cb[i as usize]; N],
            Node::Un(op, a) => val[k] = v(a).map(|x| unary(op, x)),
            Node::Bin(op, a, c) => {
                let (x, y) = (v(a), v(c));
                val[k] = std::array::from_fn(|i| binary(op, x[i], y[i]));
            }
            Node::Sel(m, a, c) => {
                let (m, x, y) = (v(m), v(a), v(c));
                val[k] = std::array::from_fn(|i| x[i] & m[i] | y[i] & !m[i]);
            }
            Node::Prmt(a, c, sel) => {
                let (x, y) = (v(a), v(c));
                val[k] = std::array::from_fn(|i| crate::exec::prmt(x[i], sel as u32, y[i], 0));
            }
            Node::Fma(a, c, d) => {
                let (x, y, z) = (v(a), v(c), v(d));
                val[k] = std::array::from_fn(|i| crate::fp::fma(f32::from_bits(x[i]), f32::from_bits(y[i]), f32::from_bits(z[i])).to_bits());
            }
            Node::Expand(a) => val[k] = std::array::from_fn(|i| if s[a as usize] >> i & 1 != 0 { !0 } else { 0 }),
            Node::Active => s[k] = active,
            Node::Pred(p) => s[k] = if p < 8 { pred[p as usize] } else { cc[p as usize & 3] },
            Node::SImm(x) => s[k] = x,
            Node::S(op, a, c) => {
                let (x, y) = (s[a as usize], s[c as usize]);
                s[k] = match op {
                    SOp::And => x & y,
                    SOp::Or => x | y,
                    SOp::Xor => x ^ y,
                };
            }
            Node::Bits(a) => s[k] = (0..N).fold(0, |m, i| m | ((v(a)[i] != 0) as u32) << i),
            Node::Store(r, a, m) => {
                let (x, m) = (v(a), s[m as usize]);
                for i in 0..N {
                    if m >> i & 1 != 0 {
                        gpr[r as usize][i] = x[i];
                    }
                }
            }
            Node::StoreAll(r, a) => gpr[r as usize] = v(a),
            Node::StoreP(p, a, m) => {
                let (x, m) = (s[a as usize], s[m as usize]);
                let r = if p < 8 { &mut pred[p as usize] } else { &mut cc[p as usize & 3] };
                *r = *r & !m | x & m;
            }
        }
    }
}

/// (Natively) predicates and flags kept in values within a block as `forward` keeps registers,
/// each written once at the end; and every value made once (the same node of the same
/// arguments is the same value).
fn fold(ir: &[Node]) -> Option<Vec<Node>> {
    let mut out: Vec<Node> = Vec::with_capacity(ir.len());
    let mut map = vec![0 as Id; ir.len()];
    let mut seen: std::collections::HashMap<Node, Id> = std::collections::HashMap::new();
    let mut cur: [Option<Id>; 12] = [None; 12];
    let mut dirty = [false; 12];
    let mut stores = Vec::new();
    fn make(out: &mut Vec<Node>, seen: &mut std::collections::HashMap<Node, Id>, n: Node) -> Id {
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
            Node::Pred(p) => match cur[p as usize] {
                Some(c) => c,
                None => {
                    let c = make(&mut out, &mut seen, Node::Pred(p));
                    cur[p as usize] = Some(c);
                    c
                }
            },
            Node::StoreP(p, v, m) => {
                let old = match cur[p as usize] {
                    Some(c) => c,
                    None => make(&mut out, &mut seen, Node::Pred(p)),
                };
                let all = make(&mut out, &mut seen, Node::SImm(!0));
                let not_m = make(&mut out, &mut seen, Node::S(SOp::Xor, r(m), all));
                let keep = make(&mut out, &mut seen, Node::S(SOp::And, old, not_m));
                let put = make(&mut out, &mut seen, Node::S(SOp::And, r(v), r(m)));
                let new = make(&mut out, &mut seen, Node::S(SOp::Or, keep, put));
                cur[p as usize] = Some(new);
                dirty[p as usize] = true;
                new
            }
            Node::StoreAll(g, a) => {
                stores.push(Node::StoreAll(g, r(a)));
                0
            }
            Node::Store(..) => unreachable!("forward made registers whole"),
            Node::Reg(_) | Node::Imm(_) | Node::Cb(_) | Node::Active | Node::SImm(_) => make(&mut out, &mut seen, *n),
            Node::Un(op, a) => make(&mut out, &mut seen, Node::Un(op, r(a))),
            Node::Bin(op, a, b) => make(&mut out, &mut seen, Node::Bin(op, r(a), r(b))),
            Node::Sel(a, b, c) => make(&mut out, &mut seen, Node::Sel(r(a), r(b), r(c))),
            Node::Fma(a, b, c) => make(&mut out, &mut seen, Node::Fma(r(a), r(b), r(c))),
            Node::Prmt(a, b, x) => make(&mut out, &mut seen, Node::Prmt(r(a), r(b), x)),
            Node::Expand(a) => make(&mut out, &mut seen, Node::Expand(r(a))),
            Node::S(op, a, b) => make(&mut out, &mut seen, Node::S(op, r(a), r(b))),
            Node::Bits(a) => make(&mut out, &mut seen, Node::Bits(r(a))),
        };
    }
    out.extend(stores);
    for p in 0..12 {
        if let (true, Some(c)) = (dirty[p], cur[p]) {
            let all = make(&mut out, &mut seen, Node::SImm(!0));
            out.push(Node::StoreP(p as u8, c, all));
        }
    }
    Some(out)
}

/// Keeps registers in values within a block: a register is read from the lanes once, a write
/// makes its new value (the old one outside the mask), and each register written is stored
/// once, at the end, whole.
fn forward(ir: &[Node]) -> Vec<Node> {
    let mut out: Vec<Node> = Vec::with_capacity(ir.len());
    let mut map = vec![0 as Id; ir.len()];
    let mut cur: [Option<Id>; 256] = [None; 256];
    let mut dirty = [false; 256];
    // the vector mask of each scalar mask, made once
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
                dirty[g as usize] = true;
                s
            }
            Node::Un(op, a) => push(&mut out, Node::Un(op, r(a))),
            Node::Bin(op, a, b) => push(&mut out, Node::Bin(op, r(a), r(b))),
            Node::Sel(a, b, c) => push(&mut out, Node::Sel(r(a), r(b), r(c))),
            Node::Fma(a, b, c) => push(&mut out, Node::Fma(r(a), r(b), r(c))),
            Node::Prmt(a, b, s) => push(&mut out, Node::Prmt(r(a), r(b), s)),
            Node::S(op, a, b) => push(&mut out, Node::S(op, r(a), r(b))),
            Node::Bits(a) => push(&mut out, Node::Bits(r(a))),
            Node::StoreAll(g, a) => {
                cur[g as usize] = Some(r(a));
                dirty[g as usize] = true;
                r(a)
            }
            Node::StoreP(p, a, m) => push(&mut out, Node::StoreP(p, r(a), r(m))),
            n @ (Node::Imm(_) | Node::Cb(_) | Node::Active | Node::Pred(_) | Node::SImm(_)) => push(&mut out, n),
        };
    }
    for g in 0..256 {
        if let (true, Some(c)) = (dirty[g], cur[g]) {
            out.push(Node::StoreAll(g as Reg, c));
        }
    }
    out
}

pub mod check;
#[cfg(test)]
mod tests;
