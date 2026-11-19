//! The PDS (programmable data sequencer): a small 32-bit processor in front of the USC that
//! a data master (CDM, VDM, PDM) runs for each task. Its programs move data into the USC's
//! register stores (DMA from memory with DOUTD, immediate writes with DOUTW) and then start
//! the USC program (DOUTU). Encoding from Mesa's `pvr_rogue_pds_defs.h` /
//! `pvr_pds_disasm.c`.
//!
//! Registers, as an instruction names them:
//! - const: the program's data segment (read-only), up to 128 words;
//! - temp: the task's temporaries (32 words); the data master preloads the first few with
//!   the task's inputs (for the CDM: workgroup or instance IDs);
//! - ptemp: persistent temporaries (32 words) that keep their values between tasks.
//!
//! 32-bit register fields use the ranges const 0..127, temp 128..159, ptemp 192..223;
//! 64-bit fields name register pairs: const 0..63, temp 64..79, ptemp 96..111.
//!
//! Branches and the "cc" flag work on predicates: P0 (set by CMP), IF0 and IF1 (inputs
//! from the data master; for the CDM, IF1 marks the coefficient-update task) and the
//! condition code. An instruction with cc set runs only if the predicate the last BRA
//! selected with SETC (initially IF0) is true.
use std::fmt;

/// A register as an instruction names it: bank and 32-bit word index.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Reg {
    Const(u8),
    Temp(u8),
    Ptemp(u8),
    /// a field value outside every range (the hardware's behaviour is undefined)
    Bad(u8),
}

impl fmt::Display for Reg {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match self {
            Reg::Const(i) => write!(f, "const{i}"),
            Reg::Temp(i) => write!(f, "temp{i}"),
            Reg::Ptemp(i) => write!(f, "ptemp{i}"),
            Reg::Bad(v) => write!(f, "bad({v:#x})"),
        }
    }
}

/// A 64-bit register operand: shown as the pair's low word.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Reg64(pub Reg);

impl fmt::Display for Reg64 {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "{}:64", self.0)
    }
}

/// 32-bit field, any bank (REGS32): const 0..127, temp 128..159, ptemp 192..223.
fn regs32(v: u32) -> Reg {
    let v = (v & 0xff) as u8;
    match v {
        0..=127 => Reg::Const(v),
        128..=159 => Reg::Temp(v - 128),
        192..=223 => Reg::Ptemp(v - 192),
        _ => Reg::Bad(v),
    }
}

/// 32-bit destination field (REGS32TP): temp 0..31, ptemp 32..63.
fn regs32tp(v: u32) -> Reg {
    let v = (v & 0x3f) as u8;
    if v < 32 { Reg::Temp(v) } else { Reg::Ptemp(v - 32) }
}

/// 32-bit temp-only field (REGS32T).
fn regs32t(v: u32) -> Reg {
    Reg::Temp((v & 0x1f) as u8)
}

/// 64-bit field, any bank (REGS64): const 0..63, temp 64..79, ptemp 96..111.
fn regs64(v: u32) -> Reg64 {
    let v = (v & 0x7f) as u8;
    Reg64(match v {
        0..=63 => Reg::Const(2 * v),
        64..=79 => Reg::Temp(2 * (v - 64)),
        96..=111 => Reg::Ptemp(2 * (v - 96)),
        _ => Reg::Bad(v),
    })
}

/// 64-bit destination (REGS64TP): temp 0..15, ptemp 16..31.
fn regs64tp(v: u32) -> Reg64 {
    let v = (v & 0x1f) as u8;
    Reg64(if v < 16 { Reg::Temp(2 * v) } else { Reg::Ptemp(2 * (v - 16)) })
}

/// 64-bit temp-only field (REGS64T).
fn regs64t(v: u32) -> Reg64 {
    Reg64(Reg::Temp(2 * (v & 0xf) as u8))
}

/// 64-bit const-only field (REGS64C).
fn regs64c(v: u32) -> Reg64 {
    Reg64(Reg::Const(2 * (v & 0x3f) as u8))
}

/// Predicates a BRA tests (SRCC) or selects as the condition code (SETC).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Pred {
    P0,
    If0,
    If1,
    /// stream-out overflow predicates 0..3 and the global one (4)
    SoOverflow(u8),
    /// SRCC: always true; SETC: leave the condition code selection alone
    Keep,
    /// out of bounds (from a DDMAD's bounds test)
    Oob,
    Other(u8),
}

impl Pred {
    fn decode(v: u32) -> Pred {
        match v & 0xf {
            0 => Pred::P0,
            1 => Pred::If0,
            2 => Pred::If1,
            v @ 3..=7 => Pred::SoOverflow(v as u8 - 3),
            8 => Pred::Keep,
            9 => Pred::Oob,
            v => Pred::Other(v as u8),
        }
    }
}

impl fmt::Display for Pred {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match self {
            Pred::P0 => write!(f, "p0"),
            Pred::If0 => write!(f, "if0"),
            Pred::If1 => write!(f, "if1"),
            Pred::SoOverflow(n) => write!(f, "so{n}"),
            Pred::Keep => write!(f, "keep"),
            Pred::Oob => write!(f, "oob"),
            Pred::Other(n) => write!(f, "pred{n}"),
        }
    }
}

/// The DOUT interfaces.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DoutKind {
    /// DMA from memory into the unified or common store
    D,
    /// direct write of a 32/64-bit value into a store
    W,
    /// start the USC program
    U,
    /// vertex data (VDM)
    V,
    /// iterator (PDM)
    I,
    /// fence / completion
    C,
    Other(u8),
}

/// Logical op of SFTLP.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Lop {
    None,
    Not,
    And,
    Or,
    Xor,
    Xnor,
    Nand,
    Nor,
}

impl Lop {
    fn decode(v: u32) -> Lop {
        [Lop::None, Lop::Not, Lop::And, Lop::Or, Lop::Xor, Lop::Xnor, Lop::Nand, Lop::Nor][(v & 7) as usize]
    }

    fn apply(self, a: u64, b: u64) -> u64 {
        match self {
            Lop::None => a,
            Lop::Not => !a,
            Lop::And => a & b,
            Lop::Or => a | b,
            Lop::Xor => a ^ b,
            Lop::Xnor => !(a ^ b),
            Lop::Nand => !(a & b),
            Lop::Nor => !(a | b),
        }
    }
}

/// Comparison of CMP.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Cop {
    Eq,
    Gt,
    Lt,
    Ne,
}

/// Shift amount of SFTLP: an immediate (positive left, negative right) or a register.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Shift {
    Imm(i8),
    Reg(Reg),
}

/// Second operand of CMP.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CmpSrc {
    Imm(u16),
    Reg(Reg64),
}

/// A decoded PDS instruction.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Instr {
    /// dst = src0 + src1 (or src0 - src1); 32-bit
    Add32 { cc: bool, signed: bool, sub: bool, src0: Reg, src1: Reg, dst: Reg },
    Add64 { cc: bool, signed: bool, sub: bool, src0: Reg64, src1: Reg64, dst: Reg64 },
    /// dst64 = src0 * src1 + src2 (or - src2)
    Mad { cc: bool, signed: bool, sub: bool, src0: Reg, src1: Reg, src2: Reg64, dst: Reg64 },
    /// dst = (src0 LOP src1) shifted by src2
    Sftlp32 { cc: bool, lop: Lop, src0: Reg, src1: Reg, src2: Shift, dst: Reg },
    Sftlp64 { cc: bool, lop: Lop, src0: Reg64, src1: Reg64, src2: Shift, dst: Reg64 },
    /// P0 = src0 COP src1 (unsigned 64-bit)
    Cmp { cc: bool, cop: Cop, src0: Reg64, src1: CmpSrc },
    /// if (SRCC != neg) branch by `offset` instructions; then select SETC as the condition code
    Bra { srcc: Pred, neg: bool, setc: Pred, offset: i32 },
    /// load from memory into temps (src0 holds address, count and destination)
    Ld { cc: bool, src0: Reg64 },
    /// store temps to memory
    St { cc: bool, src0: Reg64 },
    /// dst = 16-bit immediate (zero-extended)
    Limm { cc: bool, global: bool, imm: u16, dst: Reg },
    Stmc { cc: bool, mask: u8 },
    /// wait for outstanding data (DMA) to land
    Wdf { cc: bool },
    Lock { cc: bool },
    Release { cc: bool },
    Halt { cc: bool },
    Nop { cc: bool },
    /// DOUTD with address src2 + src0 * src1 and control word from src3
    Ddmad { cc: bool, end: bool, src0: Reg, src1: Reg, src2: Reg64, src3: Reg64 },
    Dout { cc: bool, end: bool, kind: DoutKind, src0: Reg64, src1: Reg },
    Unknown(u32),
}

impl Instr {
    pub fn decode(w: u32) -> Instr {
        let cc = w & (1 << 27) != 0;
        if w & (1 << 31) != 0 {
            match (w >> 28) & 0xf {
                0x8 => Instr::Add64 {
                    cc,
                    signed: w & (1 << 26) != 0,
                    sub: w & (1 << 24) != 0,
                    src0: regs64(w >> 12),
                    src1: regs64(w >> 5),
                    dst: regs64tp(w),
                },
                0x9 => Instr::Add32 {
                    cc,
                    signed: w & (1 << 26) != 0,
                    sub: w & (1 << 24) != 0,
                    src0: regs32(w >> 14),
                    src1: regs32(w >> 6),
                    dst: regs32tp(w),
                },
                0xa => {
                    let im = w & (1 << 23) != 0;
                    let src2 = if im {
                        Shift::Imm(((w >> 5) & 0xff) as u8 as i8)
                    } else {
                        Shift::Reg(regs32(w >> 5))
                    };
                    Instr::Sftlp64 { cc, lop: Lop::decode(w >> 24), src0: regs64tp(w >> 18), src1: regs64tp(w >> 13), src2, dst: regs64tp(w) }
                }
                0xb => {
                    let cop = [Cop::Eq, Cop::Gt, Cop::Lt, Cop::Ne][((w >> 25) & 3) as usize];
                    let src1 = if w & (1 << 23) != 0 { CmpSrc::Imm((w >> 2) as u16) } else { CmpSrc::Reg(regs64(w >> 2)) };
                    Instr::Cmp { cc, cop, src0: regs64tp(w >> 18), src1 }
                }
                0xc => {
                    let a = w & 0x7ffff;
                    let offset = if a & 0x40000 != 0 { a as i32 - 0x80000 } else { a as i32 };
                    Instr::Bra { srcc: Pred::decode(w >> 24), neg: w & (1 << 23) != 0, setc: Pred::decode(w >> 19), offset }
                }
                0xd => match (w >> 23) & 0xf {
                    0 => Instr::Ld { cc, src0: regs64(w) },
                    1 => Instr::St { cc, src0: regs64(w) },
                    2 => Instr::Wdf { cc },
                    3 => Instr::Limm { cc, global: w & 2 != 0, imm: (w >> 2) as u16, dst: regs32t(w >> 18) },
                    4 => Instr::Lock { cc },
                    5 => Instr::Release { cc },
                    6 => Instr::Halt { cc },
                    7 => Instr::Stmc { cc, mask: (w & 0xf) as u8 },
                    0xf => Instr::Nop { cc },
                    _ => Instr::Unknown(w),
                },
                0xe => Instr::Ddmad {
                    cc,
                    end: w & (1 << 26) != 0,
                    src0: regs32(w >> 18),
                    src1: regs32t(w >> 13),
                    src2: regs64(w >> 6),
                    src3: regs64c(w),
                },
                0xf => {
                    let kind = match w & 7 {
                        0 => DoutKind::D,
                        1 => DoutKind::W,
                        2 => DoutKind::U,
                        3 => DoutKind::V,
                        4 => DoutKind::I,
                        5 => DoutKind::C,
                        k => DoutKind::Other(k as u8),
                    };
                    Instr::Dout { cc, end: w & (1 << 26) != 0, kind, src0: regs64(w >> 8), src1: regs32(w >> 16) }
                }
                _ => Instr::Unknown(w),
            }
        } else if w & (1 << 30) != 0 {
            match (w >> 29) & 7 {
                2 => {
                    let src2 = if w & (1 << 28) != 0 {
                        // a 6-bit signed immediate in bits 10:5
                        Shift::Imm(((((w >> 5) & 0x3f) << 2) as u8 as i8) >> 2)
                    } else {
                        Shift::Reg(regs32tp(w >> 5))
                    };
                    Instr::Sftlp32 { cc, lop: Lop::decode(w >> 24), src0: regs32t(w >> 19), src1: regs32(w >> 11), src2, dst: regs32t(w) }
                }
                _ => Instr::Unknown(w),
            }
        } else {
            Instr::Mad {
                cc,
                signed: w & (1 << 28) != 0,
                sub: w & (1 << 29) != 0,
                src0: regs32(w >> 19),
                src1: regs32(w >> 11),
                src2: regs64(w >> 4),
                dst: regs64t(w),
            }
        }
    }
}

impl fmt::Display for Instr {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        let q = |cc: bool| if cc { "? " } else { "" };
        match *self {
            Instr::Add32 { cc, signed, sub, src0, src1, dst } => {
                write!(f, "{}{}32{} {dst}, {src0}, {src1}", q(cc), if sub { "sub" } else { "add" }, if signed { ".s" } else { "" })
            }
            Instr::Add64 { cc, signed, sub, src0, src1, dst } => {
                write!(f, "{}{}64{} {dst}, {src0}, {src1}", q(cc), if sub { "sub" } else { "add" }, if signed { ".s" } else { "" })
            }
            Instr::Mad { cc, signed, sub, src0, src1, src2, dst } => {
                write!(f, "{}mad{}{} {dst}, {src0}, {src1}, {src2}", q(cc), if signed { ".s" } else { "" }, if sub { ".sub" } else { "" })
            }
            Instr::Sftlp32 { cc, lop, src0, src1, src2, dst } => write!(f, "{}sftlp32 {dst}, {src0} {lop:?} {src1}, {src2:?}", q(cc)),
            Instr::Sftlp64 { cc, lop, src0, src1, src2, dst } => write!(f, "{}sftlp64 {dst}, {src0} {lop:?} {src1}, {src2:?}", q(cc)),
            Instr::Cmp { cc, cop, src0, src1 } => match src1 {
                CmpSrc::Imm(i) => write!(f, "{}cmp.{cop:?} p0, {src0}, #{i:#x}", q(cc)),
                CmpSrc::Reg(r) => write!(f, "{}cmp.{cop:?} p0, {src0}, {r}", q(cc)),
            },
            Instr::Bra { srcc, neg, setc, offset } => write!(f, "bra {}{srcc}, setc {setc}, {offset:+}", if neg { "!" } else { "" }),
            Instr::Ld { cc, src0 } => write!(f, "{}ld {src0}", q(cc)),
            Instr::St { cc, src0 } => write!(f, "{}st {src0}", q(cc)),
            Instr::Limm { cc, global, imm, dst } => write!(f, "{}limm{} {dst}, #{imm:#x}", q(cc), if global { ".gr" } else { "" }),
            Instr::Stmc { cc, mask } => write!(f, "{}stmc #{mask:#x}", q(cc)),
            Instr::Wdf { cc } => write!(f, "{}wdf", q(cc)),
            Instr::Lock { cc } => write!(f, "{}lock", q(cc)),
            Instr::Release { cc } => write!(f, "{}release", q(cc)),
            Instr::Halt { cc } => write!(f, "{}halt", q(cc)),
            Instr::Nop { cc } => write!(f, "{}nop", q(cc)),
            Instr::Ddmad { cc, end, src0, src1, src2, src3 } => {
                write!(f, "{}ddmad{} {src0}, {src1}, {src2}, {src3}", q(cc), if end { ".end" } else { "" })
            }
            Instr::Dout { cc, end, kind, src0, src1 } => {
                let e = if end { ".end" } else { "" };
                match kind {
                    DoutKind::U => write!(f, "{}doutu{e} {src0}", q(cc)),
                    DoutKind::C => write!(f, "{}doutc{e}", q(cc)),
                    k => write!(f, "{}dout{}{e} {src0}, {src1}", q(cc), format!("{k:?}").to_lowercase()),
                }
            }
            Instr::Unknown(w) => write!(f, "unknown {w:#010x}"),
        }
    }
}

/// Disassemble a code segment (one instruction per line, with its index).
pub fn disassemble(code: &[u32]) -> String {
    code.iter().enumerate().map(|(i, &w)| format!("{i:3}: {w:08x}  {}\n", Instr::decode(w))).collect()
}

/// The length of the program at the start of `code`: up to the first unconditional end
/// (HALT, or a DOUT/DDMAD with END) that no branch jumps past.
pub fn program_len(code: &[u32]) -> usize {
    let mut reach = 0i64;
    for (i, &w) in code.iter().enumerate() {
        let i = i as i64;
        match Instr::decode(w) {
            Instr::Bra { offset, .. } => reach = reach.max(i + offset as i64),
            Instr::Halt { cc: false } | Instr::Dout { cc: false, end: true, .. } | Instr::Ddmad { cc: false, end: true, .. } if i >= reach => {
                return i as usize + 1;
            }
            _ => {}
        }
    }
    code.len()
}

/// A DOUTD (DMA) request, decoded from its two source words.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DoutD {
    /// source GPU virtual address
    pub addr: u64,
    /// per-instance data offset in words (vertex attributes)
    pub doffset: u32,
    /// words to transfer
    pub size: u32,
    /// destination register offset in the store
    pub ao: u32,
    /// common store (else the unified store)
    pub common: bool,
    /// the last DMA of the task
    pub last: bool,
}

impl DoutD {
    pub fn decode(src0: u64, src1: u32) -> DoutD {
        DoutD {
            addr: src0 & 0xff_ffff_ffff,
            doffset: ((src0 >> 40) & 0x1fff) as u32,
            size: src1 & 0xfff,
            ao: (src1 >> 13) & 0x1fff,
            common: src1 & (1 << 28) != 0,
            last: src1 & (1 << 31) != 0,
        }
    }
}

/// A DOUTW (direct write) request: `data` goes to `ao` (and `ao + 1`) of a store.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DoutW {
    pub ao: u32,
    /// which words of the 64-bit source to write: (word index, value)
    pub words: [Option<u32>; 2],
    pub common: bool,
    pub last: bool,
}

impl DoutW {
    pub fn decode(data: u64, src1: u32) -> DoutW {
        let (lo, hi) = (data as u32, (data >> 32) as u32);
        // BSIZE: 0 lower 32 bits, 1 upper 32 bits, 2 all 64, 3 none
        let words = match src1 & 3 {
            0 => [Some(lo), None],
            1 => [Some(hi), None],
            2 => [Some(lo), Some(hi)],
            _ => [None, None],
        };
        DoutW { ao: (src1 >> 13) & 0x1fff, words, common: src1 & (1 << 28) != 0, last: src1 & (1 << 31) != 0 }
    }
}

/// A DOUTU: start the USC program at `exe_offset` (in the USC heap) with `temps` temporaries
/// per instance.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DoutU {
    pub exe_offset: u32,
    pub temps: u32,
    pub sample_rate: u32,
    pub dual_phase: bool,
}

impl DoutU {
    pub fn decode(src0: u64) -> DoutU {
        DoutU {
            exe_offset: (src0 as u32) & !3,
            temps: ((src0 >> 35) & 0x3f) as u32 * 4,
            sample_rate: ((src0 >> 33) & 3) as u32,
            dual_phase: src0 & (1 << 41) != 0,
        }
    }
}

/// What a PDS program asks of the data master and the memory system.
pub trait Host {
    fn read32(&mut self, va: u64) -> Option<u32>;
    fn write32(&mut self, va: u64, v: u32) -> bool;
    fn doutd(&mut self, d: DoutD);
    fn doutw(&mut self, w: DoutW);
    fn doutu(&mut self, u: DoutU);
    /// DOUTC: the task (or the kernel, for a fence) is complete
    fn doutc(&mut self) {}
    /// DOUTV / DOUTI / anything else the host does not model
    fn dout_other(&mut self, kind: DoutKind, src0: u64, src1: u32);
}

/// Inputs of one PDS task.
#[derive(Clone, Debug, Default)]
pub struct Task {
    pub if0: bool,
    pub if1: bool,
    /// the first temps, preloaded by the data master
    pub temps: Vec<u32>,
}

/// The PDS: persistent temps survive across tasks.
#[derive(Default)]
pub struct Pds {
    pub ptemps: [u32; 32],
    pub trace: bool,
}

/// The program faulted or ran away.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Error {
    Unimplemented(u32, String),
    CodeFetch(u64),
    Runaway,
}

struct Regs<'a> {
    consts: &'a [u32],
    temps: [u32; 32],
    ptemps: &'a mut [u32; 32],
}

impl Regs<'_> {
    fn get(&self, r: Reg) -> u32 {
        match r {
            Reg::Const(i) => self.consts.get(i as usize).copied().unwrap_or(0),
            Reg::Temp(i) => self.temps[(i & 31) as usize],
            Reg::Ptemp(i) => self.ptemps[(i & 31) as usize],
            Reg::Bad(_) => 0,
        }
    }

    fn set(&mut self, r: Reg, v: u32) {
        match r {
            Reg::Temp(i) => self.temps[(i & 31) as usize] = v,
            Reg::Ptemp(i) => self.ptemps[(i & 31) as usize] = v,
            Reg::Const(_) | Reg::Bad(_) => {}
        }
    }

    fn get64(&self, r: Reg64) -> u64 {
        let lo = self.get(r.0) as u64;
        let hi = match r.0 {
            Reg::Const(i) => self.get(Reg::Const(i + 1)),
            Reg::Temp(i) => self.get(Reg::Temp(i + 1)),
            Reg::Ptemp(i) => self.get(Reg::Ptemp(i + 1)),
            Reg::Bad(_) => 0,
        } as u64;
        hi << 32 | lo
    }

    fn set64(&mut self, r: Reg64, v: u64) {
        self.set(r.0, v as u32);
        match r.0 {
            Reg::Temp(i) => self.set(Reg::Temp(i + 1), (v >> 32) as u32),
            Reg::Ptemp(i) => self.set(Reg::Ptemp(i + 1), (v >> 32) as u32),
            _ => {}
        }
    }
}

impl Pds {
    /// Run the program at `code_va` with data segment `data` for one task.
    pub fn run(&mut self, host: &mut dyn Host, code_va: u64, data: &[u32], task: &Task) -> Result<(), Error> {
        let mut regs = Regs { consts: data, temps: [0; 32], ptemps: &mut self.ptemps };
        for (i, &v) in task.temps.iter().take(32).enumerate() {
            regs.temps[i] = v;
        }
        let mut p0 = false;
        let mut cc_sel = Pred::If0;
        let mut pc: i64 = 0;
        let pred = |p: Pred, p0: bool| match p {
            Pred::P0 => p0,
            Pred::If0 => task.if0,
            Pred::If1 => task.if1,
            Pred::Keep => true,
            _ => false,
        };
        for _ in 0..100_000 {
            let va = code_va.wrapping_add(4 * pc as u64);
            let w = host.read32(va).ok_or(Error::CodeFetch(va))?;
            let ins = Instr::decode(w);
            if self.trace {
                eprintln!("[gpu pds]   {pc:3}: {w:08x}  {ins}");
            }
            let cc_ok = pred(cc_sel, p0);
            let run = |cc: bool| !cc || cc_ok;
            pc += 1;
            match ins {
                Instr::Add32 { cc, signed: _, sub, src0, src1, dst } if run(cc) => {
                    let (a, b) = (regs.get(src0), regs.get(src1));
                    regs.set(dst, if sub { a.wrapping_sub(b) } else { a.wrapping_add(b) });
                }
                Instr::Add64 { cc, signed: _, sub, src0, src1, dst } if run(cc) => {
                    let (a, b) = (regs.get64(src0), regs.get64(src1));
                    regs.set64(dst, if sub { a.wrapping_sub(b) } else { a.wrapping_add(b) });
                }
                Instr::Mad { cc, signed, sub, src0, src1, src2, dst } if run(cc) => {
                    let (a, b) = (regs.get(src0), regs.get(src1));
                    let m = if signed { (a as i32 as i64).wrapping_mul(b as i32 as i64) as u64 } else { a as u64 * b as u64 };
                    let c = regs.get64(src2);
                    regs.set64(dst, if sub { m.wrapping_sub(c) } else { m.wrapping_add(c) });
                }
                Instr::Sftlp32 { cc, lop, src0, src1, src2, dst } if run(cc) => {
                    let v = lop.apply(regs.get(src0) as u64, regs.get(src1) as u64) as u32;
                    let sh = match src2 {
                        Shift::Imm(s) => s as i32,
                        Shift::Reg(r) => regs.get(r) as i32,
                    };
                    let v = if sh >= 0 { v.checked_shl(sh as u32).unwrap_or(0) } else { v.checked_shr(sh.unsigned_abs()).unwrap_or(0) };
                    regs.set(dst, v);
                }
                Instr::Sftlp64 { cc, lop, src0, src1, src2, dst } if run(cc) => {
                    let v = lop.apply(regs.get64(src0), regs.get64(src1));
                    let sh = match src2 {
                        Shift::Imm(s) => (s as i32).clamp(-63, 63),
                        Shift::Reg(r) => regs.get(r) as i32,
                    };
                    let v = if sh >= 0 { v.checked_shl(sh as u32).unwrap_or(0) } else { v.checked_shr(sh.unsigned_abs()).unwrap_or(0) };
                    regs.set64(dst, v);
                }
                Instr::Cmp { cc, cop, src0, src1 } if run(cc) => {
                    let a = regs.get64(src0);
                    let b = match src1 {
                        CmpSrc::Imm(i) => i as u64,
                        CmpSrc::Reg(r) => regs.get64(r),
                    };
                    p0 = match cop {
                        Cop::Eq => a == b,
                        Cop::Gt => a > b,
                        Cop::Lt => a < b,
                        Cop::Ne => a != b,
                    };
                }
                Instr::Bra { srcc, neg, setc, offset } => {
                    let taken = pred(srcc, p0) != neg;
                    if setc != Pred::Keep {
                        cc_sel = setc;
                    }
                    if taken {
                        pc += offset as i64 - 1;
                    }
                }
                Instr::Limm { cc, global, imm, dst } if run(cc) => {
                    if global {
                        return Err(Error::Unimplemented(w, "limm to a global register".into()));
                    }
                    regs.set(dst, imm as u32);
                }
                Instr::Ld { cc, src0 } if run(cc) => {
                    // src0: address >> 2 in bits 39:2, count of 64-bit words in 43:41,
                    // destination (REGS64TP) in 51:47
                    let s = regs.get64(src0);
                    let addr = ((s >> 2) & 0x3f_ffff_ffff) << 2;
                    let count = ((s >> 41) & 7) as u32;
                    let dst = regs64tp((s >> 47) as u32);
                    for i in 0..count {
                        let lo = host.read32(addr + 8 * i as u64).unwrap_or(0) as u64;
                        let hi = host.read32(addr + 8 * i as u64 + 4).unwrap_or(0) as u64;
                        let r = match dst.0 {
                            Reg::Temp(t) => Reg64(Reg::Temp(t + 2 * i as u8)),
                            Reg::Ptemp(t) => Reg64(Reg::Ptemp(t + 2 * i as u8)),
                            other => Reg64(other),
                        };
                        regs.set64(r, hi << 32 | lo);
                    }
                }
                Instr::St { cc, src0 } if run(cc) => {
                    // src0: address >> 2 in bits 39:2, count of 32-bit words in 43:40,
                    // source (REGS32TP) in 51:46
                    let s = regs.get64(src0);
                    let addr = ((s >> 2) & 0x3f_ffff_ffff) << 2;
                    let count = match ((s >> 40) & 0xf) as u32 {
                        0 => 16,
                        n => n,
                    };
                    let src = regs32tp((s >> 46) as u32);
                    for i in 0..count {
                        let r = match src {
                            Reg::Temp(t) => Reg::Temp(t + i as u8),
                            Reg::Ptemp(t) => Reg::Ptemp(t + i as u8),
                            other => other,
                        };
                        host.write32(addr + 4 * i as u64, regs.get(r));
                    }
                }
                Instr::Halt { cc } if run(cc) => return Ok(()),
                Instr::Ddmad { cc, end, src0, src1, src2, src3 } if run(cc) => {
                    let a = regs.get(src0) as u64 * regs.get(src1) as u64;
                    let base = regs.get64(src2);
                    let ctl = regs.get64(src3);
                    let addr = (base & 0xff_ffff_ffff).wrapping_add(a);
                    host.doutd(DoutD::decode((base & !0xff_ffff_ffff) | (addr & 0xff_ffff_ffff), ctl as u32));
                    if end {
                        return Ok(());
                    }
                }
                Instr::Dout { cc, end, kind, src0, src1 } if run(cc) => {
                    let (s0, s1) = (regs.get64(src0), regs.get(src1));
                    match kind {
                        DoutKind::D => host.doutd(DoutD::decode(s0, s1)),
                        DoutKind::W => host.doutw(DoutW::decode(s0, s1)),
                        DoutKind::U => host.doutu(DoutU::decode(s0)),
                        DoutKind::C => host.doutc(),
                        k => host.dout_other(k, s0, s1),
                    }
                    if end {
                        return Ok(());
                    }
                }
                // the DMA and register writes complete before the next instruction here
                Instr::Wdf { .. } | Instr::Nop { .. } | Instr::Stmc { .. } | Instr::Lock { .. } | Instr::Release { .. } => {}
                Instr::Unknown(w) => return Err(Error::Unimplemented(w, format!("{ins}"))),
                // a cc-gated instruction whose condition is false
                _ => {}
            }
        }
        Err(Error::Runaway)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    /// Test vectors captured from the guest: Mesa's compute programs for vkcompute.
    const SHARED_DATA: [u32; 12] =
        [0x00043040, 0x80, 0x00043080, 0x80, 0x0004b0c0, 0x80, 0x10000004, 0x10008002, 0x9000c003, 0, 0, 0];
    const SHARED_CODE: [u32; 4] = [0xf0060000, 0xf0070100, 0xf4080200, 0];
    const KERNEL_DATA: [u32; 8] = [0x00007a00, 0x10, 0x10000002, 0x90004001, 0x80000000, 0, 0, 0];
    const KERNEL_CODE: [u32; 12] =
        [0xc2c00007, 0x90016000, 0x9001a041, 0x9001e0c3, 0xf0024001, 0xf0034101, 0xd3000000, 0xf0044001, 0xfc000002, 0xd3000000, 0xa1, 0];
    const FENCE_CODE: [u32; 5] = [0xc2c00002, 0xd3000000, 0x80000010, 0xf0000005, 0xd3000000];

    #[derive(Default)]
    struct Rec {
        mem: HashMap<u64, u32>,
        out: Vec<String>,
    }

    impl Host for Rec {
        fn read32(&mut self, va: u64) -> Option<u32> {
            self.mem.get(&va).copied()
        }
        fn write32(&mut self, va: u64, v: u32) -> bool {
            self.mem.insert(va, v);
            true
        }
        fn doutd(&mut self, d: DoutD) {
            self.out.push(format!("D {:#x} n{} ao{} c{} l{}", d.addr, d.size, d.ao, d.common as u8, d.last as u8));
        }
        fn doutw(&mut self, w: DoutW) {
            self.out.push(format!("W ao{} {:?} c{} l{}", w.ao, w.words, w.common as u8, w.last as u8));
        }
        fn doutu(&mut self, u: DoutU) {
            self.out.push(format!("U {:#x} t{}", u.exe_offset, u.temps));
        }
        fn doutc(&mut self) {
            self.out.push("C".into());
        }
        fn dout_other(&mut self, kind: DoutKind, _: u64, _: u32) {
            self.out.push(format!("{kind:?}"));
        }
    }

    fn load(code: &[u32]) -> Rec {
        let mut r = Rec::default();
        for (i, &w) in code.iter().enumerate() {
            r.mem.insert(0x1000 + 4 * i as u64, w);
        }
        r
    }

    #[test]
    fn disassembles_the_compute_programs() {
        assert_eq!(Instr::decode(0xc2c00007).to_string(), "bra !if1, setc keep, +7");
        assert_eq!(Instr::decode(0x90016000).to_string(), "add32 temp0, const5, temp0");
        assert_eq!(Instr::decode(0x9001e0c3).to_string(), "add32 temp3, const7, temp3");
        assert_eq!(Instr::decode(0xf0024001).to_string(), "doutw temp0:64, const2");
        assert_eq!(Instr::decode(0xfc000002).to_string(), "? doutu.end const0:64");
        assert_eq!(Instr::decode(0xd3000000).to_string(), "halt");
        assert_eq!(Instr::decode(0xf4080200).to_string(), "doutd.end const4:64, const8");
        assert_eq!(Instr::decode(0x80000010).to_string(), "add64 ptemp0:64, const0:64, const0:64");
        assert_eq!(Instr::decode(0xf0000005).to_string(), "doutc");
    }

    #[test]
    fn program_lengths_follow_branches() {
        assert_eq!(program_len(&SHARED_CODE), 3);
        assert_eq!(program_len(&KERNEL_CODE), 10);
        assert_eq!(program_len(&FENCE_CODE), 5);
    }

    #[test]
    fn shared_register_program_dmas_three_blocks() {
        let mut h = load(&SHARED_CODE);
        let mut pds = Pds::default();
        pds.run(&mut h, 0x1000, &SHARED_DATA, &Task { if0: true, ..Default::default() }).unwrap();
        assert_eq!(h.out, ["D 0x8000043040 n4 ao0 c1 l0", "D 0x8000043080 n2 ao4 c1 l0", "D 0x800004b0c0 n3 ao6 c1 l1"]);
    }

    #[test]
    fn compute_program_coefficient_and_instance_tasks() {
        let mut pds = Pds::default();
        // coefficient update task: workgroup IDs (5, 0, 0) into the common store
        let mut h = load(&KERNEL_CODE);
        pds.run(&mut h, 0x1000, &KERNEL_DATA, &Task { if0: true, if1: true, temps: vec![5, 0, 0, 0] }).unwrap();
        assert_eq!(h.out, ["W ao0 [Some(5), Some(0)] c1 l0", "W ao2 [Some(0), None] c1 l1"]);
        // instance task: local index into the unified store, then the USC kick
        let mut h = load(&KERNEL_CODE);
        pds.run(&mut h, 0x1000, &KERNEL_DATA, &Task { if0: true, if1: false, temps: vec![17, 0] }).unwrap();
        assert_eq!(h.out, ["W ao0 [Some(17), None] c0 l1", "U 0x7a00 t8"]);
    }

    #[test]
    fn fence_program_clears_ptemp_and_signals() {
        let mut pds = Pds::default();
        pds.ptemps[0] = 0x1234;
        let mut h = load(&FENCE_CODE);
        pds.run(&mut h, 0x1000, &[0; 4], &Task { if0: true, ..Default::default() }).unwrap();
        assert_eq!(h.out, ["C"]);
        assert_eq!(pds.ptemps[0], 0);
        // the coefficient task of the fence does nothing
        let mut h = load(&FENCE_CODE);
        pds.run(&mut h, 0x1000, &[0; 4], &Task { if0: true, if1: true, ..Default::default() }).unwrap();
        assert!(h.out.is_empty());
    }
}
