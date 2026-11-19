//! The USC (unified shading cluster): the Rogue shader core. Programs are sequences of
//! variable-length *instruction groups*; one group issues up to one operation per pipeline
//! phase, all sharing one set of register reads and writes. Encoding from Mesa's
//! `pco_isa.py` (bit layouts), `pco_map.py` (which unit reads which operand) and
//! `pco_binary.c` (group layout).
//!
//! A group is laid out as
//!
//! ```text
//!   header (2-3 bytes) | ops, last phase first | lower srcs | upper srcs | iss | dests | pad
//! ```
//!
//! The header gives the total length (in 16-bit words), the ALU type (main, bitwise,
//! control), which phases are present, the execution condition and the end-of-program flag.
//! Every other part is self-describing: its first bytes carry extension bits that say how
//! many bytes follow.
//!
//! Data flow of a *main* group (per instance):
//! - six source registers s0..s5 (the "lower" and "upper" halves), and a mux `is0` picking
//!   one of them; the "extra" feed-through `fte` starts as `is0`;
//! - phase 0 computes `ft0` (64-bit ops also `fte`) from s0, s1, s2;
//! - phase 1 computes `ft1` from s3;
//! - phase 2: `pck` computes `ft2` from `is3`; `tst` compares `is1` with `is2` into `ftt`
//!   (and optionally the predicate p0); `movc` picks the values written;
//! - the internal source selects `is1..is5` choose among ft0/ft1/ft2/fte; `is4` and `is5`
//!   are what the destinations w0 and w1 receive (unless a `movc` chooses otherwise);
//! - the backend (memory access) reads its operands as registers named by s0..s5.
//!
//! *Bitwise* groups: phase 0 bypasses s0 -> ft0 and s1 (or an immediate, or a bit mask) ->
//! ft1, s2 -> ft2 (optionally shifted) and a bit count of it -> ft3; phase 1 is a logical
//! op into ft4 (else ft4 = ft1); phase 2 a shift of ft4 into ft5 (else ft5 = ft4).
//! w0 receives ft5 and w1 ft3.
//!
//! *Control* groups carry one op: branches, the conditional-execution ops (`cnd*`), data
//! fences, nops.
//!
//! Instances of a task run in lockstep. Each has an execution-enable flag (pe) and a
//! predicate p0. Structured control flow keeps a per-instance nesting counter (the "emc",
//! in an ordinary temp register): `cndst` / `cndend` raise and lower it, and an instance is
//! enabled while its counter is zero. Branches are taken by the whole task: `allinst`
//! branches when no instance is enabled, `anyinst` when some instance is.
use std::collections::HashMap;
use std::fmt;

/// Register banks.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Bank {
    /// constants and special registers
    Special,
    Temp,
    /// per-instance inputs (the unified store, written by the PDS)
    Vtxin,
    /// per-workgroup common store (coefficients; compute: workgroup IDs)
    Coeff,
    /// common store shared by all tasks of a kernel (push constants, descriptors)
    Shared,
    CoeffAlt,
    Idx0,
    Idx1,
}

impl Bank {
    fn from3(v: u8) -> Bank {
        [Bank::Special, Bank::Temp, Bank::Vtxin, Bank::Coeff, Bank::Shared, Bank::CoeffAlt, Bank::Idx0, Bank::Idx1][(v & 7) as usize]
    }
}

/// A register operand.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RegRef {
    pub bank: Bank,
    pub idx: u16,
}

impl RegRef {
    fn new(bank: u8, idx: u16) -> RegRef {
        RegRef { bank: Bank::from3(bank), idx }
    }

    fn offset(self, k: u16) -> RegRef {
        match self.bank {
            Bank::Special => self,
            _ => RegRef { bank: self.bank, idx: self.idx + k },
        }
    }
}

impl fmt::Display for RegRef {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        let p = match self.bank {
            Bank::Special => "sc",
            Bank::Temp => "r",
            Bank::Vtxin => "vi",
            Bank::Coeff => "cf",
            Bank::Shared => "sh",
            Bank::CoeffAlt => "cfa",
            Bank::Idx0 => "idx0.",
            Bank::Idx1 => "idx1.",
        };
        write!(f, "{p}{}", self.idx)
    }
}

/// Execution condition of a group (per instance).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Cc {
    /// enabled instances
    E1Zx,
    /// enabled instances with p0 set
    E1Z1,
    /// all instances, enabled or not
    ExZx,
    /// enabled instances with p0 clear
    E1Z0,
}

impl Cc {
    fn from2(v: u8) -> Cc {
        [Cc::E1Zx, Cc::E1Z1, Cc::ExZx, Cc::E1Z0][(v & 3) as usize]
    }

    fn passes(self, pe: bool, p0: bool) -> bool {
        match self {
            Cc::E1Zx => pe,
            Cc::E1Z1 => pe && p0,
            Cc::ExZx => true,
            Cc::E1Z0 => pe && !p0,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Alu {
    Main,
    Bitwise,
    Control,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Header {
    /// decode assist (offset of the sources; unused here)
    pub da: u8,
    /// total length in bytes
    pub len: u32,
    pub alu: Alu,
    /// main: OPORG (which phases); bitwise: OPCNT bit set (p0 1, p1 2, p2 4)
    pub ops: u8,
    pub w0p: bool,
    pub w1p: bool,
    pub cc: Cc,
    pub end: bool,
    pub atom: bool,
    /// repeat count 1..4
    pub rpt: u8,
    /// control: the op (CTRLOP) and its misc bit (end for nop, drc for wdf)
    pub ctrlop: u8,
    pub miscctl: bool,
}

/// Floating-point source modifiers.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct FMods {
    pub neg: bool,
    pub abs: bool,
    pub flr: bool,
}

/// Single-source ops of the main ALU (SNGL).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Sngl {
    Rcp,
    Rsq,
    Log,
    Exp,
    F16Sop,
    Logcn,
    Gamma,
    Byp,
    Dsx,
    Dsy,
    Dsxf,
    Dsyf,
    Pck,
    Red,
    Sinc,
    Arctanc,
}

/// Integer op of the INT32_64 unit.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum IntOp {
    /// {s1:s0} + s2 -> {fte:ft0}
    Add6432,
    /// {s1:s0} + {is0:s2} -> {fte:ft0}
    Add64,
    /// s0 * s1 + s2 -> ft0
    Madd32,
    /// s0 * s1 + {is0:s2} -> {fte:ft0}
    Madd64,
}

/// A main-ALU operation (phases 0, 1 and 2).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MainOp {
    Fadd { sat: bool, s0: FMods, s1: FMods },
    Fmul { sat: bool, s0: FMods, s1: FMods },
    Fmad { sat: bool, s0: FMods, s1: FMods, s2: FMods },
    Sngl { op: Sngl, s0: FMods },
    /// phase 0: unpack s0 from `fmt` into ft0
    Upck { elem: u8, scale_rtz: bool, fmt: u8 },
    /// phase 2: pack is3 into `fmt` in ft2
    Pck { prog: bool, rtz: bool, scale: bool, fmt: u8 },
    Int { op: IntOp, signed: bool, cin: bool, neg: [bool; 3], abs: [bool; 3] },
    /// phase 2: ftt = is1 OP is2
    Tst { op: u8, ty: u8, elem: u8, pwen: bool, p2end: bool },
    /// phase 2: w0 = ftt ? movw0 : is4; w1 = ftt ? movw1 : is5
    Movc { movw0: u8, movw1: u8, maskw0: u8, aw: bool, p2end: bool },
}

/// Where a backend op's burst length comes from.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Burst {
    Imm(u8),
    /// a register named by source slot n
    Src(u8),
}

/// Backend (memory) ops.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Backend {
    /// load `burst` words from the 64-bit address in the register of slot `addr` into the
    /// registers starting at the one named by s3
    Ld { drc: u8, addr: u8, burst: Burst, cache: u8 },
    /// store `burst` elements of `dsize` (0 8-bit, 1 16-bit, 2 32-bit) from the registers
    /// starting at slot `data` to the address in slot `addr`
    St { drc: u8, addr: u8, burst: Burst, data: u8, dsize: u8, cache: u8, tiled: Option<u8> },
    /// data fence on `drc` for the address in slot `addr`
    Idf { drc: u8, addr: u8 },
    Atomic { drc: u8, op: u8, src: u8, dst: u8 },
}

/// Bitwise phase 0.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct BwP0 {
    /// count source: ft2 (else s2)
    pub csrc_ft2: bool,
    /// 0 count bits set, 1 find top bit, 2 bypass
    pub count_op: u8,
    /// 0 bypass, 1 shuffle, 2 reverse, 3 shift left (s2 by s1)
    pub shift1: u8,
    /// ft1 is a bit mask built from s0 (width) and s1 (offset), else s1
    pub msk: bool,
    pub imm: Option<u32>,
}

/// Bitwise phase 1: ft4 = (mska ? ft2 & ft1 : ft2) OP (mskb ? s3 & !ft1 : s3).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct BwP1 {
    pub mska: bool,
    pub mskb: bool,
    /// 0 or, 1 and, 2 xor, 4 nor, 5 nand, 6 xnor, 7 bypass
    pub op: u8,
}

/// Bitwise phase 2: ft5 = shift(ft4, s4); optionally p0 = test of ft5 / ft3.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct BwP2 {
    pub pwen: bool,
    pub tst_ft3: bool,
    pub tst_nz: bool,
    /// 0 lsl, 1 shr, 2 rol, 3 cps, 4 asr_twb, 5 asr_pwb, 6 asr_mtb, 7 asr_ftb
    pub shift: u8,
}

/// Control ops.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Ctrl {
    /// bpred: 0 exec condition, 1 all instances off, 2 any instance on
    Branch { link: bool, bpred: u8, abs: bool, offset: i32 },
    /// cndinst: 0 st, 1 ef, 2 sm, 3 lt, 4 end, 5 setl_b, 6 lpc, 7 setl_a;
    /// pcnd: 0 always, 1 p0 true, 2 never, 3 p0 false
    Cnd { adjust: u8, pcnd: u8, inst: u8 },
    Wdf,
    Wop,
    Nop,
    Mutex { lr: u8, id: u8 },
}

/// One decoded instruction group.
#[derive(Clone, Debug)]
pub struct Group {
    pub hdr: Header,
    pub p0: Option<MainOp>,
    pub p1: Option<MainOp>,
    pub pck: Option<MainOp>,
    pub tst: Option<MainOp>,
    pub mov: Option<MainOp>,
    pub backend: Option<Backend>,
    pub bw0: Option<BwP0>,
    pub bw1: Option<BwP1>,
    pub bw2: Option<BwP2>,
    pub ctrl: Option<Ctrl>,
    /// s0..s5
    pub src: [Option<RegRef>; 6],
    /// is0 mux: 0 s0, 1 s3, 2 s4, 3 s5, 4 s1, 5 s2
    pub is0: u8,
    /// is1 (0 ft0, 1 fte), is2 (0 ft1, 1 fte), is3 (0 ft0, 1 ft1, 3 fte), is4/is5 (ft0, ft1, ft2, fte)
    pub iss: [u8; 6],
    pub dst: [Option<RegRef>; 2],
}

/// Why a group could not be decoded.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DecodeError(pub String);

struct Rd<'a> {
    b: &'a [u8],
    pos: usize,
}

impl Rd<'_> {
    fn at(&self, i: usize) -> u8 {
        self.b.get(self.pos + i).copied().unwrap_or(0)
    }
}

fn err<T>(s: impl Into<String>) -> Result<T, DecodeError> {
    Err(DecodeError(s.into()))
}

/// Decode a source half (three slots and, for the lower half, the is0 mux). Returns the
/// slots, the mux and the length.
fn decode_srcs(r: &Rd) -> ([Option<RegRef>; 3], Option<u8>, usize) {
    let (b0, b1, b2, b3, b4, b5) = (r.at(0), r.at(1), r.at(2), r.at(3), r.at(4), r.at(5));
    let r6 = |bank: u8, idx: u16| Some(RegRef::new(bank, idx));
    if b0 & 0x80 == 0 {
        return ([r6((b0 >> 6) & 1, (b0 & 0x3f) as u16), None, None], None, 1);
    }
    let (sel, ext1) = (b1 >> 7, (b1 >> 6) & 1);
    let ext2 = b2 >> 7;
    // fields shared by the wider variants
    let sba2 = ((b2 >> 4) & 1) << 1 | (b0 >> 6) & 1;
    let sa7 = (((b2 >> 2) & 1) as u16) << 6 | (b0 & 0x3f) as u16;
    let sbb2 = ((b2 >> 3) & 1) << 1 | (b1 >> 5) & 1;
    let sb7 = ((b2 & 3) as u16) << 5 | (b1 & 0x1f) as u16;
    match (sel, ext1, ext2) {
        (0, 0, _) => {
            let bank = ((b1 >> 2) & 3) << 1 | (b0 >> 6) & 1;
            let idx = ((b2 & 7) as u16) << 8 | ((b1 & 3) as u16) << 6 | (b0 & 0x3f) as u16;
            ([r6(bank, idx), None, None], Some((b1 >> 4) & 3), 3)
        }
        (1, 0, _) => ([r6((b0 >> 6) & 1, (b0 & 0x3f) as u16), r6((b1 >> 5) & 1, (b1 & 0x1f) as u16), None], None, 2),
        (1, 1, 0) => ([r6(sba2, sa7), r6(sbb2, sb7), None], Some((b2 >> 5) & 3), 3),
        (1, 1, _) => {
            let bank = ((b3 >> 3) & 1) << 2 | sba2;
            let idx = ((b3 >> 5) as u16) << 8 | (((b3 >> 1) & 1) as u16) << 7 | sa7;
            let sb8 = ((b3 & 1) as u16) << 7 | sb7;
            let mux = ((b3 >> 4) & 1) << 2 | (b2 >> 5) & 3;
            ([r6(bank, idx), r6(sbb2, sb8), None], Some(mux), 4)
        }
        (0, 1, 0) => ([r6(sba2, sa7), r6(sbb2, sb7), r6(b3 >> 6, (b3 & 0x3f) as u16)], Some((b2 >> 5) & 3), 4),
        _ => {
            let ext4 = (b4 >> 2) & 1;
            let sba3 = ((b4 >> 3) & 1) << 2 | sba2;
            let sb8 = ((b4 & 1) as u16) << 7 | sb7;
            let sbc3 = (b4 >> 7) << 2 | (b3 >> 6) & 3;
            let mux = ((b4 >> 4) & 1) << 2 | (b2 >> 5) & 3;
            let sa8 = (((b4 >> 1) & 1) as u16) << 7 | sa7;
            let sc8 = (((b4 >> 5) & 3) as u16) << 6 | (b3 & 0x3f) as u16;
            if ext4 == 0 {
                ([r6(sba3, sa8), r6(sbb2, sb8), r6(sbc3, sc8)], Some(mux), 5)
            } else {
                let sa11 = ((b5 & 7) as u16) << 8 | sa8;
                let sc11 = (((b5 >> 3) & 7) as u16) << 8 | sc8;
                ([r6(sba3, sa11), r6(sbb2, sb8), r6(sbc3, sc11)], Some(mux), 6)
            }
        }
    }
}

/// Decode the destinations: one (into w0 or w1) or two.
fn decode_dests(r: &Rd, w0p: bool, w1p: bool) -> ([Option<RegRef>; 2], usize) {
    let (b0, b1, b2, b3) = (r.at(0), r.at(1), r.at(2), r.at(3));
    if !w0p && !w1p {
        return ([None, None], 0);
    }
    if w0p != w1p {
        let (d, n) = if b0 & 0x80 == 0 {
            (RegRef::new((b0 >> 6) & 1, (b0 & 0x3f) as u16), 1)
        } else {
            let bank = ((b1 >> 2) & 3) << 1 | (b0 >> 6) & 1;
            let idx = (((b1 >> 4) & 7) as u16) << 8 | ((b1 & 3) as u16) << 6 | (b0 & 0x3f) as u16;
            (RegRef::new(bank, idx), 2)
        };
        return (if w0p { [Some(d), None] } else { [None, Some(d)] }, n);
    }
    if b1 & 0x80 == 0 {
        let d0 = RegRef::new(b0 >> 7, (b0 & 0x7f) as u16);
        let d1 = RegRef::new((b1 >> 6) & 1, (b1 & 0x3f) as u16);
        return ([Some(d0), Some(d1)], 2);
    }
    let db0 = ((b2 >> 1) & 3) << 1 | b0 >> 7;
    let db1 = ((b2 >> 5) & 3) << 1 | (b1 >> 6) & 1;
    let d0 = ((b2 & 1) as u16) << 7 | (b0 & 0x7f) as u16;
    let d1 = (((b2 >> 3) & 3) as u16) << 6 | (b1 & 0x3f) as u16;
    if b2 & 0x80 == 0 {
        ([Some(RegRef::new(db0, d0)), Some(RegRef::new(db1, d1))], 3)
    } else {
        let d0 = ((b3 & 7) as u16) << 8 | d0;
        let d1 = (((b3 >> 3) & 7) as u16) << 8 | d1;
        ([Some(RegRef::new(db0, d0)), Some(RegRef::new(db1, d1))], 4)
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Phase {
    P0,
    P1,
    Pck,
    Tst,
    Mov,
}

/// Decode one main-ALU op in `phase`; returns it and its length.
fn decode_main(r: &Rd, phase: Phase) -> Result<(MainOp, usize), DecodeError> {
    let (b0, b1) = (r.at(0), r.at(1));
    let ext = b0 & 0x10 != 0;
    let bit = |b: u8, n: u8| b & (1 << n) != 0;
    Ok(match b0 >> 5 {
        op @ 0..=3 => {
            let s0 = FMods { neg: bit(b0, 3), abs: bit(b0, 2), flr: bit(b0, 0) };
            let s1 = FMods { abs: bit(b0, 1), ..Default::default() };
            let sat = bit(b0, 4);
            (if op < 2 { MainOp::Fadd { sat, s0, s1 } } else { MainOp::Fmul { sat, s0, s1 } }, 1)
        }
        4 => {
            let sop = b0 & 0xf;
            if sop == 12 {
                if !ext {
                    return err("pck/upck without its extension byte");
                }
                return Ok(match phase {
                    Phase::Pck => (MainOp::Pck { prog: bit(b1, 7), rtz: bit(b1, 6), scale: bit(b1, 5), fmt: b1 & 0x1f }, 2),
                    _ => (MainOp::Upck { elem: b1 >> 6, scale_rtz: bit(b1, 5), fmt: b1 & 0x1f }, 2),
                });
            }
            let op = [
                Sngl::Rcp,
                Sngl::Rsq,
                Sngl::Log,
                Sngl::Exp,
                Sngl::F16Sop,
                Sngl::Logcn,
                Sngl::Gamma,
                Sngl::Byp,
                Sngl::Dsx,
                Sngl::Dsy,
                Sngl::Dsxf,
                Sngl::Dsyf,
                Sngl::Pck,
                Sngl::Red,
                Sngl::Sinc,
                Sngl::Arctanc,
            ][sop as usize];
            if ext {
                (MainOp::Sngl { op, s0: FMods { neg: bit(b1, 1), abs: bit(b1, 0), flr: false } }, 2)
            } else {
                (MainOp::Sngl { op, s0: FMods::default() }, 1)
            }
        }
        5 => return err("int8_16 ops are not implemented"),
        6 if phase == Phase::Mov => {
            let (maskw0, aw, p2end) = if ext { ((b1 >> 2) & 0xf, bit(b1, 1), bit(b1, 0)) } else { (0xf, false, false) };
            (MainOp::Movc { movw0: b0 & 3, movw1: (b0 >> 2) & 3, maskw0, aw, p2end }, if ext { 2 } else { 1 })
        }
        6 => {
            let mut s0 = FMods { neg: bit(b0, 3), abs: bit(b0, 2), flr: false };
            let mut s1 = FMods::default();
            let mut s2 = FMods { neg: bit(b0, 1), ..Default::default() };
            if ext {
                s1 = FMods { neg: bit(b1, 2), abs: bit(b1, 3), flr: false };
                s2.flr = bit(b1, 1);
                s2.abs = bit(b1, 0);
            }
            s0.flr = false;
            (MainOp::Fmad { sat: bit(b0, 0), s0, s1, s2 }, if ext { 2 } else { 1 })
        }
        _ if phase == Phase::Tst => {
            if ext {
                let op = (b1 & 1) << 3 | (b0 >> 1) & 7;
                (MainOp::Tst { op, ty: b1 >> 5, elem: (b1 >> 2) & 3, pwen: bit(b0, 0), p2end: bit(b1, 4) }, 2)
            } else {
                (MainOp::Tst { op: (b0 >> 1) & 7, ty: 0, elem: 0, pwen: bit(b0, 0), p2end: false }, 1)
            }
        }
        _ => {
            let op = [IntOp::Add6432, IntOp::Add64, IntOp::Madd32, IntOp::Madd64][(b0 & 3) as usize];
            let mut neg = [false, false, bit(b0, 2)];
            let mut abs = [false; 3];
            let mut cin = false;
            if ext {
                cin = bit(b1, 6);
                neg[1] = bit(b1, 5);
                neg[0] = bit(b1, 4);
                abs[0] = bit(b1, 2);
                abs[1] = bit(b1, 1);
                abs[2] = bit(b1, 0);
            }
            (MainOp::Int { op, signed: bit(b0, 3), cin, neg, abs }, if ext { 2 } else { 1 })
        }
    })
}

/// Decode a backend op; returns it and its length.
fn decode_backend(r: &Rd) -> Result<(Backend, usize), DecodeError> {
    let (b0, b1, b2, b3) = (r.at(0), r.at(1), r.at(2), r.at(3));
    if b0 >> 5 != 7 {
        return err(format!("backend op {} (uvsw/msk/phas/setl/vistest/fitr/emit) not implemented", b0 >> 5));
    }
    let drc = (b0 >> 3) & 1;
    let immbl = b0 & 0x10 != 0;
    let burst = |b2: u8| {
        if immbl {
            let n = (b2 & 1) << 3 | (b1 >> 2) & 7;
            Burst::Imm(if n == 0 { 16 } else { n })
        } else {
            Burst::Src((b1 >> 2) & 7)
        }
    };
    Ok(match b0 & 7 {
        0 => (Backend::Idf { drc, addr: b1 & 7 }, 2),
        1 => (Backend::Ld { drc, addr: b1 >> 5, burst: burst(b2), cache: b1 & 3 }, if immbl { 3 } else { 2 }),
        2 => {
            let tiled = b2 & 0x80 != 0;
            let st = Backend::St {
                drc,
                addr: b1 >> 5,
                burst: burst(b2),
                data: (b2 >> 4) & 7,
                dsize: (b2 >> 2) & 3,
                cache: b1 & 3,
                tiled: if tiled { Some(b3 & 7) } else { None },
            };
            (st, if tiled { 4 } else { 3 })
        }
        5 => (Backend::Atomic { drc, op: b1 >> 4, src: b1 & 7, dst: b2 & 7 }, 3),
        4 => return err("smp (texture sample) not implemented"),
        op => return err(format!("dma op {op} not implemented")),
    })
}

/// Decode the control op `ctrlop` (from the header); returns it and its length.
fn decode_ctrl(r: &Rd, ctrlop: u8) -> Result<(Ctrl, usize), DecodeError> {
    let (b0, b1, b2, b3, b4) = (r.at(0), r.at(1), r.at(2), r.at(3), r.at(4));
    Ok(match ctrlop {
        0 => {
            let off = u32::from_le_bytes([b1 & 0xfe, b2, b3, b4]) as i32;
            (Ctrl::Branch { link: b0 & 0x10 != 0, bpred: (b0 >> 2) & 3, abs: b0 & 2 != 0, offset: off }, 5)
        }
        3 => (Ctrl::Cnd { adjust: (b0 >> 5) & 3, pcnd: (b0 >> 3) & 3, inst: b0 & 7 }, 1),
        4 => (Ctrl::Wop, 0),
        5 => (Ctrl::Wdf, 0),
        6 => (Ctrl::Mutex { lr: b0 >> 6, id: b0 & 0xf }, 1),
        7 => (Ctrl::Nop, 1),
        op => {
            let name = match op {
                1 => "lapc",
                2 => "savl",
                8 => "itrsmp",
                11 => "sbo",
                12 => "ditr",
                _ => "reserved",
            };
            return err(format!("control op {name} ({op}) not implemented"));
        }
    })
}

/// The bytes of the group at `va`, if readable.
fn fetch(mem: &mut dyn Memory, va: u64) -> Option<Vec<u8>> {
    let byte = |mem: &mut dyn Memory, a: u64| mem.read32(a & !3).map(|w| (w >> (8 * (a & 3))) as u8);
    let b0 = byte(mem, va)?;
    let len = match (b0 & 0xf) as u64 {
        0 => 32,
        n => 2 * n,
    };
    (0..len).map(|i| byte(mem, va + i)).collect()
}

impl Group {
    /// Decode the group at the start of `bytes`.
    pub fn decode(bytes: &[u8]) -> Result<Group, DecodeError> {
        let (b0, b1) = (bytes.first().copied().unwrap_or(0), bytes.get(1).copied().unwrap_or(0));
        let ext = b1 & 0x80 != 0;
        let b2 = if ext { bytes.get(2).copied().unwrap_or(0) } else { 0 };
        let len = match b0 & 0xf {
            0 => 32,
            n => 2 * n as u32,
        };
        let alu = if !ext {
            Alu::Main
        } else {
            match (b2 >> 5) & 3 {
                0 => Alu::Main,
                2 => Alu::Bitwise,
                3 => Alu::Control,
                _ => return err("reserved ALU type"),
            }
        };
        let hdr = Header {
            da: b0 >> 4,
            len,
            alu,
            ops: (b1 >> 4) & 7,
            w0p: b1 & 2 != 0,
            w1p: b1 & 4 != 0,
            cc: Cc::from2((b2 & 1) << 1 | b1 & 1),
            end: alu != Alu::Control && b2 & 0x80 != 0,
            atom: alu != Alu::Control && b2 & 8 != 0,
            rpt: if alu == Alu::Control { 1 } else { ((b2 >> 1) & 3) + 1 },
            ctrlop: (b2 >> 1) & 0xf,
            miscctl: alu == Alu::Control && b2 & 0x80 != 0,
        };
        let mut g = Group {
            hdr,
            p0: None,
            p1: None,
            pck: None,
            tst: None,
            mov: None,
            backend: None,
            bw0: None,
            bw1: None,
            bw2: None,
            ctrl: None,
            src: [None; 6],
            is0: 0,
            iss: [0; 6],
            dst: [None; 2],
        };
        let mut r = Rd { b: bytes, pos: if ext { 3 } else { 2 } };
        let mut has_srcs = true;
        match alu {
            Alu::Main => {
                // OPORG: p0, p2, be, p0_p1, p0_p2, p0_p1_p2, p0_p2_be, p0_p1_p2_be
                let (p0, p1, p2, be) = match hdr.ops {
                    0 => (true, false, false, false),
                    1 => (false, false, true, false),
                    2 => (false, false, false, true),
                    3 => (true, true, false, false),
                    4 => (true, false, true, false),
                    5 => (true, true, true, false),
                    6 => (true, false, true, true),
                    _ => (true, true, true, true),
                };
                if be {
                    let (op, n) = decode_backend(&r)?;
                    g.backend = Some(op);
                    r.pos += n;
                }
                if p2 {
                    // mov, tst, pck in that order; a tst or mov with p2end (or a pck) ends phase 2
                    loop {
                        let phase = match r.at(0) >> 5 {
                            6 => Phase::Mov,
                            7 => Phase::Tst,
                            4 => Phase::Pck,
                            op => return err(format!("main op {op} in phase 2")),
                        };
                        let (op, n) = decode_main(&r, phase)?;
                        r.pos += n;
                        let done = match op {
                            MainOp::Movc { p2end, .. } if g.mov.is_none() && g.tst.is_none() && g.pck.is_none() => {
                                g.mov = Some(op);
                                p2end
                            }
                            MainOp::Tst { p2end, .. } if g.tst.is_none() && g.pck.is_none() => {
                                g.tst = Some(op);
                                p2end
                            }
                            MainOp::Pck { .. } => {
                                g.pck = Some(op);
                                true
                            }
                            _ => return err("phase 2 ops out of order"),
                        };
                        if done {
                            break;
                        }
                    }
                }
                if p1 {
                    let (op, n) = decode_main(&r, Phase::P1)?;
                    g.p1 = Some(op);
                    r.pos += n;
                }
                if p0 {
                    let (op, n) = decode_main(&r, Phase::P0)?;
                    g.p0 = Some(op);
                    r.pos += n;
                }
            }
            Alu::Bitwise => {
                let bit = |b: u8, n: u8| b & (1 << n) != 0;
                if hdr.ops & 4 != 0 {
                    let b = r.at(0);
                    g.bw2 = Some(BwP2 { pwen: bit(b, 5), tst_ft3: bit(b, 4), tst_nz: bit(b, 3), shift: b & 7 });
                    r.pos += 1;
                }
                if hdr.ops & 2 != 0 {
                    let b = r.at(0);
                    g.bw1 = Some(BwP1 { mskb: bit(b, 5), mska: bit(b, 3), op: b & 7 });
                    r.pos += 1;
                }
                if hdr.ops & 1 != 0 {
                    let b = r.at(0);
                    let count_op = (b >> 1 & 1) << 1 | (b >> 5) & 1;
                    let mut p = BwP0 { csrc_ft2: bit(b, 6), count_op, shift1: (b >> 2) & 3, msk: false, imm: None };
                    let mut n = 1;
                    if bit(b, 4) {
                        if bit(b, 0) {
                            p.imm = Some(u32::from_le_bytes([r.at(1), r.at(2), r.at(3), r.at(4)]));
                            n = 5;
                        } else {
                            p.imm = Some(u16::from_le_bytes([r.at(1), r.at(2)]) as u32);
                            n = 3;
                        }
                    } else {
                        p.msk = bit(b, 0);
                    }
                    g.bw0 = Some(p);
                    r.pos += n;
                }
            }
            Alu::Control => {
                let (op, n) = decode_ctrl(&r, hdr.ctrlop)?;
                g.ctrl = Some(op);
                r.pos += n;
                // decode assist 0: no sources or destinations
                has_srcs = hdr.da != 0;
                if has_srcs && !matches!(op, Ctrl::Cnd { .. }) {
                    return err("control op with sources");
                }
            }
        }
        if has_srcs {
            let (lo, mux, n) = decode_srcs(&r);
            g.src[..3].copy_from_slice(&lo);
            g.is0 = mux.unwrap_or(0);
            r.pos += n;
            let (up, _, n) = decode_srcs(&r);
            g.src[3..].copy_from_slice(&up);
            r.pos += n;
            if alu == Alu::Main {
                let b = r.at(0);
                g.iss = [0, b & 1, (b >> 1) & 1, (b >> 2) & 3, (b >> 4) & 3, b >> 6];
                r.pos += 1;
            }
        }
        let (d, n) = decode_dests(&r, hdr.w0p, hdr.w1p);
        g.dst = d;
        r.pos += n;
        if r.pos as u32 > len {
            return err(format!("group overruns its length ({} > {len})", r.pos));
        }
        Ok(g)
    }
}

const FT: [&str; 4] = ["ft0", "ft1", "ft2", "fte"];

fn fmods(m: FMods, s: &str) -> String {
    let mut v = s.to_string();
    if m.flr {
        v = format!("flr({v})");
    }
    if m.abs {
        v = format!("abs({v})");
    }
    if m.neg {
        v = format!("-{v}");
    }
    v
}

impl fmt::Display for MainOp {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        let sat = |s: bool| if s { ".sat" } else { "" };
        match *self {
            MainOp::Fadd { sat: s, s0, s1 } => write!(f, "fadd{} ft0, {}, {}", sat(s), fmods(s0, "s0"), fmods(s1, "s1")),
            MainOp::Fmul { sat: s, s0, s1 } => write!(f, "fmul{} ft0, {}, {}", sat(s), fmods(s0, "s0"), fmods(s1, "s1")),
            MainOp::Fmad { sat: s, s0, s1, s2 } => {
                write!(f, "fmad{} ft0, {}, {}, {}", sat(s), fmods(s0, "s0"), fmods(s1, "s1"), fmods(s2, "s2"))
            }
            MainOp::Sngl { op, s0 } => write!(f, "{} {}", format!("{op:?}").to_lowercase(), fmods(s0, "src")),
            MainOp::Upck { elem, scale_rtz, fmt } => write!(f, "upck.{} ft0, s0.e{elem}{}", pck_name(fmt), if scale_rtz { ".scale" } else { "" }),
            MainOp::Pck { prog, rtz, scale, fmt } => write!(
                f,
                "pck.{}{}{}{} ft2, is3",
                pck_name(fmt),
                if prog { ".prog" } else { "" },
                if rtz { ".rtz" } else { "" },
                if scale { ".scale" } else { "" }
            ),
            MainOp::Int { op, signed, cin, neg, abs } => {
                let m = |i: usize, s: &str| fmods(FMods { neg: neg[i], abs: abs[i], flr: false }, s);
                let s = if signed { ".s" } else { "" };
                let c = if cin { ", p0" } else { "" };
                match op {
                    IntOp::Add6432 => write!(f, "add64_32{s} ft0, fte, {}, {}, {}{c}", m(0, "s0"), m(1, "s1"), m(2, "s2")),
                    IntOp::Add64 => write!(f, "add64{s} ft0, fte, {}, {}, {}, is0{c}", m(0, "s0"), m(1, "s1"), m(2, "s2")),
                    IntOp::Madd32 => write!(f, "imadd32{s} ft0, {}, {}, {}{c}", m(0, "s0"), m(1, "s1"), m(2, "s2")),
                    IntOp::Madd64 => write!(f, "imadd64{s} ft0, fte, {}, {}, {}, is0{c}", m(0, "s0"), m(1, "s1"), m(2, "s2")),
                }
            }
            MainOp::Tst { op, ty, elem, pwen, p2end: _ } => {
                let ops = ["z", "gz", "gez", "c", "e", "g", "ge", "ne", "l", "le"];
                let tys = ["f32", "u16", "s16", "u8", "s8", "u32", "s32", "?"];
                write!(
                    f,
                    "tst.{}.{} ftt{}, is1, is2{}",
                    tys[ty as usize & 7],
                    ops.get(op as usize).unwrap_or(&"?"),
                    if pwen { ", p0" } else { "" },
                    if elem != 0 { format!(" (e{elem})") } else { String::new() }
                )
            }
            MainOp::Movc { movw0, movw1, maskw0, aw, p2end: _ } => write!(
                f,
                "movc{} w0 = ftt ? {} : is4, w1 = ftt ? {} : is5{}",
                if aw { ".aw" } else { "" },
                FT[movw0 as usize],
                FT[movw1 as usize],
                if maskw0 != 0xf { format!(" (mask {maskw0:#x})") } else { String::new() }
            ),
        }
    }
}

fn pck_name(fmt: u8) -> &'static str {
    match fmt {
        0 => "u8888",
        1 => "s8888",
        2 => "o8888",
        3 => "u1616",
        4 => "s1616",
        5 => "o1616",
        6 => "u32",
        7 => "s32",
        8 => "u1010102",
        9 => "s1010102",
        10 => "u111110",
        11 => "s111110",
        12 => "f111110",
        14 => "f16f16",
        15 => "f32",
        16 => "cov",
        17 => "u565u565",
        18 => "d24s8",
        19 => "s8d24",
        20 => "f32_mask",
        21 => "2f10f10f10",
        22 => "s8888ogl",
        23 => "s1616ogl",
        30 => "zero",
        31 => "one",
        _ => "?",
    }
}

fn slot_name(n: u8) -> &'static str {
    ["s0", "s1", "s2", "s3", "s4", "s5", "?", "?"][n as usize & 7]
}

impl fmt::Display for Group {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        let h = &self.hdr;
        let mut parts = Vec::new();
        let cc = match h.cc {
            Cc::E1Zx => "",
            Cc::E1Z1 => "if(p0) ",
            Cc::ExZx => "(ignorepe) ",
            Cc::E1Z0 => "if(!p0) ",
        };
        let rpt = if h.rpt > 1 { format!("rpt{} ", h.rpt) } else { String::new() };
        if let Some(op) = self.p0 {
            parts.push(match op {
                MainOp::Sngl { op, s0 } => format!("p0: {} ft0, {}", format!("{op:?}").to_lowercase(), fmods(s0, "s0")),
                op => format!("p0: {op}"),
            });
        }
        if let Some(op) = self.p1 {
            parts.push(match op {
                MainOp::Sngl { op, s0 } => format!("p1: {} ft1, {}", format!("{op:?}").to_lowercase(), fmods(s0, "s3")),
                op => format!("p1: {op}"),
            });
        }
        for op in [self.pck, self.tst, self.mov].into_iter().flatten() {
            parts.push(format!("p2: {op}"));
        }
        if let Some(b) = self.backend {
            parts.push(match b {
                Backend::Ld { drc, addr, burst, .. } => {
                    let dst = self.src[3].map_or("?".into(), |r| r.to_string());
                    format!("ld {dst} <- [{}], {burst:?}, drc{drc}", slot_name(addr))
                }
                Backend::St { drc, addr, burst, data, dsize, tiled, .. } => format!(
                    "st{} [{}] <- {}, {burst:?}, dsize {dsize}, drc{drc}",
                    if tiled.is_some() { ".tiled" } else { "" },
                    slot_name(addr),
                    slot_name(data)
                ),
                Backend::Idf { drc, addr } => format!("idf drc{drc}, {}", slot_name(addr)),
                Backend::Atomic { drc, op, src, dst } => format!("atomic op{op} {} <- [{}], drc{drc}", slot_name(dst), slot_name(src)),
            });
        }
        if let Some(p) = self.bw0 {
            let s1 = match p.imm {
                Some(i) => format!("{i:#x}"),
                None if p.msk => "mask(s0,s1)".into(),
                None => "s1".into(),
            };
            parts.push(format!("p0: ft0 = s0, ft1 = {s1}, shift1 {}, count {}", p.shift1, p.count_op));
        }
        if let Some(p) = self.bw1 {
            let ops = ["or", "and", "xor", "?", "nor", "nand", "xnor", "byp"];
            let a = if p.mska { "ft1&ft2" } else { "ft2" };
            let b = if p.mskb { "~ft1&s3" } else { "s3" };
            parts.push(format!("p1: ft4 = {a} {} {b}", ops[p.op as usize]));
        }
        if let Some(p) = self.bw2 {
            let ops = ["lsl", "shr", "rol", "cps", "asr_twb", "asr_pwb", "asr_mtb", "asr_ftb"];
            parts.push(format!("p2: ft5 = {} ft4, s4{}", ops[p.shift as usize], if p.pwen { ", p0 = tst" } else { "" }));
        }
        if let Some(c) = self.ctrl {
            parts.push(match c {
                Ctrl::Branch { link, bpred, abs, offset } => format!(
                    "br{}{} {}{offset:+#x}",
                    ["", ".allinst", ".anyinst", ".?"][bpred as usize],
                    if link { ".link" } else { "" },
                    if abs { "abs " } else { "" }
                ),
                Ctrl::Cnd { adjust, pcnd, inst } => format!(
                    "cnd{}.{} pe, w0, s0, {adjust}",
                    ["st", "ef", "sm", "lt", "end", "setl_b", "lpc", "setl_a"][inst as usize],
                    ["always", "p0", "never", "!p0"][pcnd as usize]
                ),
                Ctrl::Wdf => format!("wdf drc{}", h.miscctl as u8),
                Ctrl::Wop => "wop".into(),
                Ctrl::Nop => "nop".into(),
                Ctrl::Mutex { lr, id } => format!("mutex lr{lr} id{id}"),
            });
        }
        let srcs: Vec<String> =
            self.src.iter().enumerate().filter_map(|(i, s)| s.map(|r| format!("s{i}={r}"))).collect();
        let mut io = srcs.join(" ");
        if h.alu == Alu::Main {
            let is0 = ["s0", "s3", "s4", "s5", "s1", "s2", "?", "?"][self.is0 as usize & 7];
            io += &format!(" is0={is0} is4={} is5={}", FT[self.iss[4] as usize], FT[self.iss[5] as usize]);
        }
        for (i, d) in self.dst.iter().enumerate() {
            if let Some(d) = d {
                io += &format!(" w{i}={d}");
            }
        }
        write!(
            f,
            "{cc}{rpt}{}{} {{ {} }} {}",
            format!("{:?}", h.alu).to_lowercase(),
            if h.end || (h.alu == Alu::Control && h.miscctl && matches!(self.ctrl, Some(Ctrl::Nop))) { ".end" } else { "" },
            parts.join("; "),
            io.trim()
        )
    }
}

/// GPU memory as the USC (and the PDS) see it: GPU virtual addresses.
pub trait Memory {
    fn read32(&mut self, va: u64) -> Option<u32>;
    fn write32(&mut self, va: u64, v: u32) -> bool;
}

/// One instance (one invocation): its private registers and execution state.
#[derive(Clone, Debug)]
pub struct Instance {
    pub temps: Vec<u32>,
    pub vtxin: Vec<u32>,
    /// execution enabled
    pub pe: bool,
    pub p0: bool,
}

impl Instance {
    pub fn new(temps: usize, vtxins: usize) -> Instance {
        Instance { temps: vec![0; temps], vtxin: vec![0; vtxins], pe: true, p0: false }
    }
}

/// An execution fault: the program is abandoned.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Fault {
    pub pc: u64,
    pub msg: String,
}

/// Statistics of one run.
#[derive(Clone, Copy, Debug, Default)]
pub struct Stats {
    pub groups: u64,
    pub stores: u64,
    pub loads: u64,
}

/// Per-workgroup / per-kernel register stores and memory a task runs against.
pub struct Machine<'a> {
    pub mem: &'a mut dyn Memory,
    pub coeffs: &'a mut [u32],
    pub shareds: &'a mut [u32],
    /// decoded groups by address (code does not change during a job)
    pub cache: &'a mut HashMap<u64, Result<Group, DecodeError>>,
    pub trace: bool,
}

/// The constant registers (special bank 0..31 hold their index; others from Mesa's
/// `pco_const_imms.c`).
fn const_reg(idx: u16) -> Option<u32> {
    if idx < 32 {
        return Some(idx as u32);
    }
    const TABLE: &[(u16, u32)] = &[
        (64, 0x3f800000),
        (65, 0x40000000),
        (66, 0x40800000),
        (67, 0x41000000),
        (68, 0x41800000),
        (69, 0x42000000),
        (70, 0x42800000),
        (71, 0x43000000),
        (72, 0x43800000),
        (73, 0x44000000),
        (74, 0x44800000),
        (75, 0x3f000000),
        (76, 0x3e800000),
        (77, 0x3e000000),
        (78, 0x3d800000),
        (79, 0x3d000000),
        (80, 0x3c800000),
        (81, 0x3c000000),
        (82, 0x3b800000),
        (83, 0x3b000000),
        (84, 0x3a800000),
        (85, 0x3a000000),
        (86, 0x39800000),
        (87, 0x39000000),
        (88, 0x38800000),
        (89, 0x402df854),
        (90, 0x3ebc5ab2),
        (91, 0x3fb504f3),
        (92, 0x3f3504f3),
        (93, 0x3f490fdb),
        (94, 0x3fc90fdb),
        (95, 0x40490fdb),
        (128, 0x3ea2f983),
        (129, 0x3f22f983),
        (130, 0x3fa2f983),
        (131, 0x40c90fdb),
        (132, 0x41490fdb),
        (133, 0x41c90fdb),
        (134, 0x37800000),
        (135, 0x38000000),
        (136, 0x3b4d2e1c),
        (137, 0x414eb852),
        (138, 0x3ed55555),
        (139, 0x3f870a3d),
        (140, 0x3d6147ae),
        (141, 0x80000000),
        (142, 0x7f800000),
        (143, 0xffffffff),
        (144, 0x7fff7fff),
        (145, 0x3e9a209b),
        (146, 0x3f317218),
        (147, 0x0000007f),
        (148, 0x7f7fffff),
        (149, 0x4b000000),
        (150, 0x4b800000),
        (151, 0x3f860a92),
        (152, 0x3eaaaaab),
        (153, 0x3e2aaaab),
        (154, 0x40549a78),
        (155, 0x3fb8aa3b),
        (156, 0x3d25aee6),
        (157, 0x3d9e8391),
        (158, 0x3f72a76f),
        (159, 0x4019999a),
    ];
    TABLE.iter().find(|(i, _)| *i == idx).map(|(_, v)| *v)
}

/// Special register numbers (`pco_isa.py` SR).
const SR_INST_NUM: u16 = 51;

fn f(v: u32) -> f32 {
    f32::from_bits(v)
}

fn fmod(v: u32, m: FMods) -> f32 {
    let mut x = f(v);
    if m.flr {
        x = x.floor();
    }
    if m.abs {
        x = x.abs();
    }
    if m.neg {
        x = -x;
    }
    x
}

fn sat(x: f32, s: bool) -> f32 {
    if s { x.clamp(0.0, 1.0) } else { x }
}

/// The value a group computes for one instance, before it is written.
struct Lanes {
    ft: [u32; 4],
    ftt: bool,
}

impl Machine<'_> {
    fn reg(&self, inst: &Instance, lane: usize, r: RegRef) -> Result<u32, String> {
        let i = r.idx as usize;
        let v = match r.bank {
            Bank::Special => {
                if let Some(c) = const_reg(r.idx) {
                    Some(c)
                } else if r.idx == SR_INST_NUM {
                    Some(lane as u32)
                } else {
                    return Err(format!("special register sr{} not implemented", r.idx));
                }
            }
            Bank::Temp => inst.temps.get(i).copied(),
            Bank::Vtxin => inst.vtxin.get(i).copied(),
            Bank::Coeff => self.coeffs.get(i).copied(),
            Bank::Shared => self.shareds.get(i).copied(),
            b => return Err(format!("register bank {b:?} not implemented")),
        };
        v.ok_or_else(|| format!("register {r} out of range"))
    }

    fn set_reg(&mut self, inst: &mut Instance, r: RegRef, v: u32) -> Result<(), String> {
        let i = r.idx as usize;
        let slot = match r.bank {
            Bank::Temp => inst.temps.get_mut(i),
            Bank::Vtxin => inst.vtxin.get_mut(i),
            Bank::Coeff => self.coeffs.get_mut(i),
            Bank::Shared => self.shareds.get_mut(i),
            b => return Err(format!("write to register bank {b:?} not implemented")),
        };
        *slot.ok_or_else(|| format!("register {r} out of range"))? = v;
        Ok(())
    }

    /// Run a task from `entry` (a GPU virtual address) until a group with the end flag.
    pub fn run(&mut self, entry: u64, insts: &mut [Instance]) -> Result<Stats, Fault> {
        let mut pc = entry;
        let mut stats = Stats::default();
        for _ in 0..1_000_000 {
            if !self.cache.contains_key(&pc) {
                let g = match fetch(self.mem, pc) {
                    Some(bytes) => Group::decode(&bytes).map_err(|e| {
                        DecodeError(format!("{} [{}]", e.0, bytes.iter().map(|b| format!("{b:02x}")).collect::<Vec<_>>().join(" ")))
                    }),
                    None => Err(DecodeError("code fetch from unmapped memory".into())),
                };
                if self.trace {
                    match &g {
                        Ok(g) => eprintln!("[gpu usc] {pc:#x}: {g}"),
                        Err(e) => eprintln!("[gpu usc] {pc:#x}: undecodable group: {}", e.0),
                    }
                }
                self.cache.insert(pc, g);
            }
            let g = match &self.cache[&pc] {
                Ok(g) => g.clone(),
                Err(e) => return Err(Fault { pc, msg: e.0.clone() }),
            };
            stats.groups += 1;
            let next = self.exec(&g, insts, &mut stats).map_err(|msg| Fault { pc, msg })?;
            let end = g.hdr.end || (g.hdr.alu == Alu::Control && g.hdr.miscctl && matches!(g.ctrl, Some(Ctrl::Nop)));
            if end {
                return Ok(stats);
            }
            pc = match next {
                Some(target) => target.wrapping_add(pc),
                None => pc + g.hdr.len as u64,
            };
        }
        Err(Fault { pc, msg: "runaway program".into() })
    }

    /// Execute one group on every instance; returns a taken branch's offset.
    fn exec(&mut self, g: &Group, insts: &mut [Instance], stats: &mut Stats) -> Result<Option<u64>, String> {
        match g.hdr.alu {
            Alu::Control => return self.exec_ctrl(g, insts),
            Alu::Main | Alu::Bitwise => {}
        }
        for (lane, inst) in insts.iter_mut().enumerate() {
            if !g.hdr.cc.passes(inst.pe, inst.p0) {
                continue;
            }
            for k in 0..g.hdr.rpt as u16 {
                if g.hdr.alu == Alu::Main {
                    self.exec_main(g, inst, lane, k, stats)?;
                } else {
                    self.exec_bitwise(g, inst, lane, k)?;
                }
            }
        }
        Ok(None)
    }

    fn srcs(&self, g: &Group, inst: &Instance, lane: usize, k: u16) -> Result<[u32; 6], String> {
        let mut s = [0u32; 6];
        for (v, r) in s.iter_mut().zip(g.src) {
            if let Some(r) = r {
                *v = self.reg(inst, lane, r.offset(k))?;
            }
        }
        Ok(s)
    }

    fn exec_main(&mut self, g: &Group, inst: &mut Instance, lane: usize, k: u16, stats: &mut Stats) -> Result<(), String> {
        let s = self.srcs(g, inst, lane, k)?;
        let is0 = s[[0, 3, 4, 5, 1, 2, 0, 0][g.is0 as usize & 7]];
        let mut l = Lanes { ft: [0, 0, 0, is0], ftt: false };
        if let Some(op) = g.p0 {
            self.unit(op, &mut l, s[0], s[1], s[2], is0, inst.p0, 0)?;
        }
        if let Some(op) = g.p1 {
            self.unit(op, &mut l, s[3], 0, 0, is0, inst.p0, 1)?;
        }
        if let Some(MainOp::Pck { prog, rtz, scale, fmt }) = g.pck {
            let is3 = l.ft[[0, 1, 3, 3][g.iss[3] as usize & 3]];
            if prog || scale {
                return Err(format!("pck.{}{} not implemented", pck_name(fmt), if prog { ".prog" } else { ".scale" }));
            }
            l.ft[2] = pack(fmt, is3, rtz)?;
        }
        if let Some(MainOp::Tst { op, ty, elem, pwen, .. }) = g.tst {
            let is1 = l.ft[[0, 3][g.iss[1] as usize & 1]];
            let is2 = l.ft[[1, 3][g.iss[2] as usize & 1]];
            l.ftt = test(op, ty, elem, is1, is2)?;
            if pwen {
                inst.p0 = l.ftt;
            }
        }
        let mut w = [l.ft[g.iss[4] as usize & 3], l.ft[g.iss[5] as usize & 3]];
        let mut mask = 0xf;
        if let Some(MainOp::Movc { movw0, movw1, maskw0, aw, .. }) = g.mov {
            if aw || l.ftt {
                w[0] = l.ft[movw0 as usize];
            }
            if l.ftt {
                w[1] = l.ft[movw1 as usize];
            }
            mask = maskw0;
        }
        if let Some(b) = g.backend {
            self.backend(g, b, inst, lane, k, stats)?;
        }
        for (i, d) in g.dst.iter().enumerate() {
            if let Some(d) = d {
                let d = d.offset(k);
                let mut v = w[i];
                if i == 0 && mask != 0xf {
                    // byte-masked write into the existing value
                    let old = self.reg(inst, lane, d)?;
                    let m = (0..4).filter(|b| mask & (1 << b) != 0).fold(0u32, |m, b| m | 0xff << (8 * b));
                    v = (old & !m) | (v & m);
                }
                self.set_reg(inst, d, v)?;
            }
        }
        Ok(())
    }

    /// One main-ALU unit (phase 0 or 1): computes ft0 (phase 0) or ft1 (phase 1).
    #[allow(clippy::too_many_arguments)]
    fn unit(&self, op: MainOp, l: &mut Lanes, a: u32, b: u32, c: u32, is0: u32, p0: bool, phase: usize) -> Result<(), String> {
        let out = match op {
            MainOp::Fadd { sat: s, s0, s1 } => sat(fmod(a, s0) + fmod(b, s1), s).to_bits(),
            MainOp::Fmul { sat: s, s0, s1 } => sat(fmod(a, s0) * fmod(b, s1), s).to_bits(),
            MainOp::Fmad { sat: s, s0, s1, s2 } => sat(fmod(a, s0) * fmod(b, s1) + fmod(c, s2), s).to_bits(),
            MainOp::Sngl { op, s0 } => {
                let x = fmod(a, s0);
                match op {
                    Sngl::Byp if s0 == FMods::default() => a,
                    Sngl::Byp => x.to_bits(),
                    Sngl::Rcp => (1.0 / x).to_bits(),
                    Sngl::Rsq => (1.0 / x.sqrt()).to_bits(),
                    Sngl::Log => x.log2().to_bits(),
                    Sngl::Exp => x.exp2().to_bits(),
                    Sngl::Logcn => x.max(f32::MIN_POSITIVE).log2().to_bits(),
                    op => return Err(format!("sngl op {op:?} not implemented")),
                }
            }
            MainOp::Upck { elem, scale_rtz, fmt } => unpack(fmt, a, elem, scale_rtz)?,
            MainOp::Int { op, signed, cin, neg, abs } => {
                let ext = |v: u32, i: usize| -> i128 {
                    let mut x = if signed { v as i32 as i128 } else { v as i128 };
                    if abs[i] {
                        x = x.abs();
                    }
                    if neg[i] {
                        x = -x;
                    }
                    x
                };
                let carry = (cin && p0) as i128;
                let r: i128 = match op {
                    IntOp::Add6432 => {
                        let mut x = ((b as u64) << 32 | a as u64) as i128;
                        if neg[0] {
                            x = -x;
                        }
                        x + ext(c, 2) + carry
                    }
                    IntOp::Add64 => ((b as u64) << 32 | a as u64) as i128 + ((is0 as u64) << 32 | c as u64) as i128 + carry,
                    IntOp::Madd32 => ext(a, 0) * ext(b, 1) + ext(c, 2) + carry,
                    IntOp::Madd64 => ext(a, 0) * ext(b, 1) + ((is0 as u64) << 32 | c as u64) as i128 + carry,
                };
                l.ft[3] = (r >> 32) as u32;
                r as u32
            }
            op => return Err(format!("{op} in phase {phase}")),
        };
        l.ft[phase] = out;
        Ok(())
    }

    fn exec_bitwise(&mut self, g: &Group, inst: &mut Instance, lane: usize, k: u16) -> Result<(), String> {
        let s = self.srcs(g, inst, lane, k)?;
        let mut ft = [0u32; 6];
        let mask = |w: u32, o: u32| -> u32 {
            let m = if w >= 32 { u32::MAX } else { (1u32 << w) - 1 };
            m.checked_shl(o).unwrap_or(0)
        };
        if let Some(p) = g.bw0 {
            ft[0] = s[0];
            ft[1] = match p.imm {
                Some(i) => i,
                None if p.msk => mask(s[0], s[1]),
                None => s[1],
            };
            ft[2] = match p.shift1 {
                0 => s[2],
                2 => s[2].reverse_bits(),
                3 => s[2].checked_shl(s[1]).unwrap_or(0),
                _ => return Err("bitwise shuffle not implemented".into()),
            };
            let c = if p.csrc_ft2 { ft[2] } else { s[2] };
            ft[3] = match p.count_op {
                0 => c.count_ones(),
                1 => {
                    if c == 0 {
                        u32::MAX
                    } else {
                        31 - c.leading_zeros()
                    }
                }
                _ => c,
            };
        }
        ft[4] = match g.bw1 {
            Some(p) => {
                let a = if p.mska { ft[2] & ft[1] } else { ft[2] };
                let b = if p.mskb { s[3] & !ft[1] } else { s[3] };
                match p.op {
                    0 => a | b,
                    1 => a & b,
                    2 => a ^ b,
                    4 => !(a | b),
                    5 => !(a & b),
                    6 => !(a ^ b),
                    7 => a,
                    op => return Err(format!("logical op {op} not implemented")),
                }
            }
            None => ft[1],
        };
        ft[5] = match g.bw2 {
            Some(p) => {
                let (x, n) = (ft[4], s[4]);
                let v = match p.shift {
                    0 => x.checked_shl(n).unwrap_or(0),
                    1 => x.checked_shr(n).unwrap_or(0),
                    2 => x.rotate_left(n & 31),
                    4 => (x as i32).checked_shr(n).unwrap_or(if (x as i32) < 0 { -1 } else { 0 }) as u32,
                    6 => {
                        // shift right, then sign-extend from the bit count in ft0
                        let v = x.checked_shr(n).unwrap_or(0);
                        let bits = ft[0];
                        if bits == 0 || bits >= 32 {
                            v
                        } else {
                            (((v << (32 - bits)) as i32) >> (32 - bits)) as u32
                        }
                    }
                    op => return Err(format!("shift op {op} not implemented")),
                };
                if p.pwen {
                    let t = if p.tst_ft3 { ft[3] } else { v };
                    inst.p0 = (t != 0) == p.tst_nz;
                }
                v
            }
            None => ft[4],
        };
        let w = [ft[5], ft[3]];
        for (i, d) in g.dst.iter().enumerate() {
            if let Some(d) = d {
                self.set_reg(inst, d.offset(k), w[i])?;
            }
        }
        Ok(())
    }

    fn backend(&mut self, g: &Group, b: Backend, inst: &mut Instance, lane: usize, k: u16, stats: &mut Stats) -> Result<(), String> {
        let slot = |n: u8| g.src.get(n as usize).copied().flatten().map(|r| r.offset(k)).ok_or(format!("backend source s{n} missing"));
        let addr = |m: &Self, inst: &Instance, n: u8| -> Result<u64, String> {
            let r = slot(n)?;
            Ok(m.reg(inst, lane, r)? as u64 | (m.reg(inst, lane, r.offset(1))? as u64) << 32)
        };
        let burst = |m: &Self, inst: &Instance, bl: Burst| -> Result<u32, String> {
            match bl {
                Burst::Imm(n) => Ok(n as u32),
                Burst::Src(n) => m.reg(inst, lane, slot(n)?),
            }
        };
        match b {
            Backend::Ld { addr: a, burst: bl, .. } => {
                let va = addr(self, inst, a)?;
                let n = burst(self, inst, bl)?;
                let dst = slot(3)?;
                for i in 0..n {
                    let v = self.mem.read32(va + 4 * i as u64).ok_or(format!("load from unmapped {:#x}", va + 4 * i as u64))?;
                    self.set_reg(inst, dst.offset(i as u16), v)?;
                }
                stats.loads += 1;
            }
            Backend::St { addr: a, burst: bl, data, dsize, tiled: None, .. } => {
                let va = addr(self, inst, a)?;
                let n = burst(self, inst, bl)?;
                let src = slot(data)?;
                for i in 0..n {
                    let v = self.reg(inst, lane, src.offset(i as u16))?;
                    match dsize {
                        2 => {
                            if !self.mem.write32(va + 4 * i as u64, v) {
                                return Err(format!("store to unmapped {:#x}", va + 4 * i as u64));
                            }
                        }
                        0 | 1 => {
                            let bytes = if dsize == 0 { 1 } else { 2 };
                            let a = va + (bytes * i) as u64;
                            let word = self.mem.read32(a & !3).ok_or(format!("store to unmapped {a:#x}"))?;
                            let sh = 8 * (a & 3) as u32;
                            let m = if bytes == 1 { 0xffu32 } else { 0xffff } << sh;
                            self.mem.write32(a & !3, (word & !m) | ((v << sh) & m));
                        }
                        _ => return Err(format!("store data size {dsize}")),
                    }
                }
                stats.stores += 1;
            }
            // the memory model is synchronous: fences have nothing to wait for
            Backend::Idf { .. } => {}
            b => return Err(format!("backend op {b:?} not implemented")),
        }
        Ok(())
    }

    fn exec_ctrl(&mut self, g: &Group, insts: &mut [Instance]) -> Result<Option<u64>, String> {
        let cc = g.hdr.cc;
        match g.ctrl {
            Some(Ctrl::Branch { link, bpred, abs, offset }) => {
                if link || abs {
                    return Err("branch with link / absolute target not implemented".into());
                }
                let taken = match bpred {
                    0 => insts.iter().any(|i| cc.passes(i.pe, i.p0)),
                    1 => !insts.iter().any(|i| i.pe),
                    2 => insts.iter().any(|i| i.pe),
                    _ => return Err("reserved branch predicate".into()),
                };
                Ok(if taken { Some(offset as i64 as u64) } else { None })
            }
            Some(Ctrl::Cnd { adjust, pcnd, inst: op }) => {
                // the loop test needs to know whether any instance goes round again
                let any_loop = op == 3 && {
                    let mut any = false;
                    for (lane, i) in insts.iter().enumerate() {
                        if cc.passes(i.pe, i.p0) && self.reg(i, lane, g.src[0].ok_or("cnd without s0")?)? == 0 {
                            any = true;
                        }
                    }
                    any
                };
                for (lane, i) in insts.iter_mut().enumerate() {
                    if !cc.passes(i.pe, i.p0) {
                        continue;
                    }
                    let emc = self.reg(i, lane, g.src[0].ok_or("cnd without s0")?)?;
                    let c = match pcnd {
                        0 => true,
                        1 => i.p0,
                        2 => false,
                        _ => !i.p0,
                    };
                    let adj = adjust as u32;
                    let out = match op {
                        // start: an enabled instance stays enabled if the condition holds;
                        // every other one goes one level deeper
                        0 => {
                            if emc == 0 && c {
                                0
                            } else {
                                emc + adj
                            }
                        }
                        // else: swap the enabled instances of the innermost level
                        1 => match emc {
                            0 => adj,
                            e if e == adj && c => 0,
                            e => e,
                        },
                        // set mask (break / continue): enabled instances take the value in s2
                        2 => {
                            if emc == 0 && c {
                                self.reg(i, lane, g.src[2].ok_or("cndsm without s2")?)?
                            } else {
                                emc
                            }
                        }
                        // loop test: while any instance loops, the rest stay where they are
                        3 => {
                            if any_loop {
                                i.p0 = emc == 0;
                                emc
                            } else {
                                i.p0 = false;
                                emc.saturating_sub(adj)
                            }
                        }
                        // end: one level up
                        4 => emc.saturating_sub(adj),
                        op => return Err(format!("cnd op {op} not implemented")),
                    };
                    i.pe = out == 0;
                    if let Some(d) = g.dst[0] {
                        self.set_reg(i, d, out)?;
                    }
                }
                Ok(None)
            }
            // memory accesses complete synchronously; there are no other tasks to wait for
            Some(Ctrl::Wdf) | Some(Ctrl::Wop) | Some(Ctrl::Nop) | Some(Ctrl::Mutex { .. }) => Ok(None),
            None => Err("control group without an op".into()),
        }
    }
}

/// Integer value of a float, rounded (or truncated with `rtz`) and saturated.
fn f2i(x: f32, rtz: bool) -> f32 {
    if rtz { x.trunc() } else { x.round_ties_even() }
}

fn pack(fmt: u8, v: u32, rtz: bool) -> Result<u32, String> {
    Ok(match fmt {
        30 => 0,
        31 => 1.0f32.to_bits(),
        15 => v,
        // float to integer conversions saturate; NaN becomes 0
        6 => f2i(f(v), rtz) as u32,
        7 => f2i(f(v), rtz) as i32 as u32,
        fmt => return Err(format!("pck.{} not implemented", pck_name(fmt))),
    })
}

fn unpack(fmt: u8, v: u32, elem: u8, _scale_rtz: bool) -> Result<u32, String> {
    Ok(match fmt {
        6 => (v as f32).to_bits(),
        7 => (v as i32 as f32).to_bits(),
        15 => v,
        0 => (((v >> (8 * elem)) & 0xff) as f32).to_bits(),
        fmt => return Err(format!("upck.{} not implemented", pck_name(fmt))),
    })
}

/// The TST unit: compare is1 with is2 (or with zero for the one-operand tests).
fn test(op: u8, ty: u8, elem: u8, a: u32, b: u32) -> Result<bool, String> {
    use std::cmp::Ordering;
    let sext = |v: u32, bits: u32| ((v << (32 - bits)) as i32 >> (32 - bits)) as i64;
    let field = |v: u32, bits: u32| (v >> (bits * elem as u32)) & ((1u64 << bits) - 1) as u32;
    let ord = |x: Option<Ordering>| x;
    let (cmp, cmp0) = match ty {
        0 => (f(a).partial_cmp(&f(b)), f(a).partial_cmp(&0.0)),
        1 => (Some(field(a, 16).cmp(&field(b, 16))), Some(field(a, 16).cmp(&0))),
        2 => (Some(sext(field(a, 16), 16).cmp(&sext(field(b, 16), 16))), Some(sext(field(a, 16), 16).cmp(&0))),
        3 => (Some(field(a, 8).cmp(&field(b, 8))), Some(field(a, 8).cmp(&0))),
        4 => (Some(sext(field(a, 8), 8).cmp(&sext(field(b, 8), 8))), Some(sext(field(a, 8), 8).cmp(&0))),
        5 => (Some(a.cmp(&b)), Some(a.cmp(&0))),
        6 => (Some((a as i32).cmp(&(b as i32))), Some((a as i32).cmp(&0))),
        t => return Err(format!("tst type {t}")),
    };
    let (cmp, cmp0) = (ord(cmp), ord(cmp0));
    use Ordering::*;
    Ok(match op {
        0 => cmp0 == Some(Equal),
        1 => cmp0 == Some(Greater),
        2 => matches!(cmp0, Some(Greater | Equal)),
        4 => cmp == Some(Equal),
        5 => cmp == Some(Greater),
        6 => matches!(cmp, Some(Greater | Equal)),
        // unordered (NaN) compares not-equal
        7 => cmp != Some(Equal),
        8 => cmp == Some(Less),
        9 => matches!(cmp, Some(Less | Equal)),
        op => return Err(format!("tst op {op} not implemented")),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The vkcompute fill shader (`tests/gpu/pvr/fill.comp`) as Mesa 26.2.6's pco compiled
    /// it for the BXE-4-32 (from PCO_DEBUG_PRINT=binary).
    pub const FILL: [u8; 224] = [
        0x86, 0x92, 0x40, 0x13, 0x40, 0x00, 0x00, 0x00, 0x00, 0x00, 0x40, 0xFF, 0x99, 0xD2, 0x00, 0xD3, //
        0x3C, 0xF0, 0xC1, 0x9C, 0x1E, 0x87, 0x87, 0x80, 0xCF, 0x90, 0x11, 0x40, 0x20, 0x40, 0x44, 0x82, //
        0x67, 0x20, 0x00, 0x00, 0x45, 0xFF, 0x98, 0xC0, 0x00, 0xD3, 0x3C, 0xF1, 0xA0, 0x9C, 0x1E, 0x87, //
        0xC0, 0xCF, 0x80, 0x11, 0x00, 0x20, 0x44, 0x82, 0x67, 0x38, 0x45, 0x00, 0x45, 0xFF, 0x04, 0x80, //
        0x60, 0x04, 0x92, 0x00, 0x00, 0x00, 0x35, 0x82, 0x00, 0x87, 0xC0, 0x04, 0x00, 0x00, 0x00, 0x40, //
        0x55, 0xD2, 0x40, 0x00, 0x02, 0x80, 0xA0, 0x80, 0x86, 0x40, 0x36, 0x82, 0x00, 0xE2, 0xC0, 0x41, //
        0x00, 0x80, 0x00, 0x00, 0x43, 0xFF, 0x35, 0x82, 0x00, 0x87, 0x84, 0x08, 0x00, 0x00, 0x00, 0x40, //
        0x36, 0x82, 0x00, 0xE2, 0xC3, 0x60, 0x00, 0x00, 0x00, 0x00, 0x40, 0xFF, 0x35, 0x82, 0x00, 0x87, //
        0x85, 0x08, 0x00, 0x00, 0x00, 0x41, 0x36, 0x82, 0x00, 0xE2, 0xC0, 0x41, 0x00, 0x41, 0x00, 0x00, //
        0x42, 0xFF, 0x55, 0xD2, 0x40, 0x00, 0x02, 0x80, 0xA3, 0x80, 0x82, 0x43, 0x35, 0x82, 0x00, 0x87, //
        0x81, 0x08, 0x00, 0x00, 0x00, 0x44, 0x37, 0x86, 0x00, 0xE8, 0x80, 0x64, 0x80, 0x43, 0x08, 0x00, //
        0xC0, 0x83, 0x44, 0xFF, 0x37, 0x86, 0x00, 0xE8, 0xC3, 0x64, 0x80, 0x03, 0x80, 0x00, 0xC0, 0x80, //
        0x41, 0xFF, 0x65, 0xA0, 0x00, 0xF2, 0x66, 0x08, 0x42, 0x40, 0x00, 0xFF, 0x02, 0x80, 0x6A, 0xFF, //
        0x44, 0x82, 0x67, 0x24, 0x45, 0x00, 0x45, 0xFF, 0x04, 0x80, 0xEE, 0x00, 0xF2, 0xFF, 0xFF, 0xFF, //
    ];

    fn groups() -> Vec<(usize, Group)> {
        let mut out = Vec::new();
        let mut pos = 0;
        while pos < FILL.len() {
            let g = Group::decode(&FILL[pos..]).unwrap_or_else(|e| panic!("group at {pos:#x}: {}", e.0));
            let len = g.hdr.len as usize;
            out.push((pos, g));
            pos += len;
        }
        out
    }

    #[test]
    fn decodes_the_fill_shader_layout() {
        let gs = groups();
        let offsets: Vec<usize> = gs.iter().map(|(o, _)| *o).collect();
        // offsets of the 21 groups, as pco printed them
        assert_eq!(
            offsets,
            [0x00, 0x0c, 0x1e, 0x26, 0x36, 0x3e, 0x46, 0x50, 0x5a, 0x66, 0x70, 0x7c, 0x86, 0x92, 0x9c, 0xa6, 0xb4, 0xc2, 0xcc, 0xd0, 0xd8]
        );
        let g = |off: usize| &gs.iter().find(|(o, _)| *o == off).unwrap().1;
        // movi32 %1, 64
        assert_eq!(g(0x00).bw0.unwrap().imm, Some(64));
        // bcmp.s32.l: s0 = vi0, s1 = sc143 (true), s3 = r0, is0 = s1, w0 = r0
        let b = g(0x0c);
        assert_eq!(b.src[0], Some(RegRef { bank: Bank::Vtxin, idx: 0 }));
        assert_eq!(b.src[1], Some(RegRef { bank: Bank::Special, idx: 143 }));
        assert_eq!(b.src[3], Some(RegRef { bank: Bank::Temp, idx: 0 }));
        assert_eq!(b.is0, 4);
        assert!(matches!(b.tst, Some(MainOp::Tst { op: 8, ty: 6, .. })));
        assert!(matches!(b.pck, Some(MainOp::Pck { fmt: 30, .. })));
        assert!(matches!(b.mov, Some(MainOp::Movc { movw0: 3, .. })));
        // cndst.if(!p0) pe, r5, r5, 1 with ignorepe
        let c = g(0x36);
        assert_eq!(c.hdr.cc, Cc::ExZx);
        assert_eq!(c.ctrl, Some(Ctrl::Cnd { adjust: 1, pcnd: 3, inst: 0 }));
        assert_eq!(c.dst[0], Some(RegRef { bank: Bank::Temp, idx: 5 }));
        // br.allinst to the epilogue at 0xd0
        assert_eq!(g(0x3e).ctrl, Some(Ctrl::Branch { link: false, bpred: 1, abs: false, offset: 0xd0 - 0x3e }));
        // s0 = cf0
        assert_eq!(g(0x46).src[0], Some(RegRef { bank: Bank::Coeff, idx: 0 }));
        // add64_32.s: s0 = sh0, s1 = r4, s2 = r3, w0 = r3, w1 = r4
        let a = g(0xa6);
        let r = |bank, idx| Some(RegRef { bank, idx });
        assert_eq!(a.src[..3], [r(Bank::Shared, 0), r(Bank::Temp, 4), r(Bank::Temp, 3)]);
        assert_eq!(a.dst, [Some(RegRef { bank: Bank::Temp, idx: 3 }), Some(RegRef { bank: Bank::Temp, idx: 4 })]);
        assert!(matches!(a.p0, Some(MainOp::Int { op: IntOp::Add6432, signed: true, .. })));
        // st.lazywriteback s0 (r2), 1 word, address in s3 (r0:r1)
        let s = g(0xc2);
        assert_eq!(s.backend, Some(Backend::St { drc: 0, addr: 3, burst: Burst::Imm(1), data: 0, dsize: 2, cache: 2, tiled: None }));
        assert_eq!(s.src[3], Some(RegRef { bank: Bank::Temp, idx: 0 }));
        // the last group ends the program
        assert!(g(0xd8).hdr.miscctl && g(0xd8).ctrl == Some(Ctrl::Nop));
    }

    struct Mem(HashMap<u64, u32>);

    impl Memory for Mem {
        fn read32(&mut self, va: u64) -> Option<u32> {
            self.0.get(&va).copied()
        }
        fn write32(&mut self, va: u64, v: u32) -> bool {
            self.0.insert(va, v);
            true
        }
    }

    #[test]
    fn runs_the_fill_shader() {
        const CODE: u64 = 0xe0_0000_7a00;
        const BUF: u64 = 0x80_0010_0000;
        let mut mem = Mem(HashMap::new());
        for (i, w) in FILL.chunks(4).enumerate() {
            mem.0.insert(CODE + 4 * i as u64, u32::from_le_bytes(w.try_into().unwrap()));
        }
        // shared registers: buffer address (sh0:sh1), offset (sh3), push constants (sh4, sh5)
        let mut shareds = vec![0u32; 64];
        shareds[0] = BUF as u32;
        shareds[1] = (BUF >> 32) as u32;
        shareds[3] = 0x40;
        shareds[4] = 3;
        shareds[5] = 7;
        let mut cache = HashMap::new();
        for wg in 0..2u32 {
            let mut coeffs = vec![0u32; 16];
            coeffs[0] = wg;
            let mut insts: Vec<Instance> = (0..64)
                .map(|i| {
                    let mut inst = Instance::new(8, 4);
                    inst.vtxin[0] = i;
                    inst
                })
                .collect();
            let mut m = Machine { mem: &mut mem, coeffs: &mut coeffs, shareds: &mut shareds, cache: &mut cache, trace: false };
            let st = m.run(CODE, &mut insts).unwrap();
            assert_eq!(st.stores, 64);
        }
        for i in 0..128u64 {
            assert_eq!(mem.0.get(&(BUF + 0x40 + 4 * i)), Some(&(i as u32 * 3 + 7)), "v[{i}]");
        }
    }

    #[test]
    fn disabled_instances_skip_the_body() {
        const CODE: u64 = 0x1000;
        let mut mem = Mem(HashMap::new());
        for (i, w) in FILL.chunks(4).enumerate() {
            mem.0.insert(CODE + 4 * i as u64, u32::from_le_bytes(w.try_into().unwrap()));
        }
        let mut shareds = vec![0u32; 16];
        shareds[0] = 0x8000;
        let mut coeffs = vec![0u32; 16];
        let mut cache = HashMap::new();
        // local indices >= 64 fail the shader's bounds check: nothing is stored
        let mut insts: Vec<Instance> = (64..96)
            .map(|i| {
                let mut inst = Instance::new(8, 4);
                inst.vtxin[0] = i;
                inst
            })
            .collect();
        let mut m = Machine { mem: &mut mem, coeffs: &mut coeffs, shareds: &mut shareds, cache: &mut cache, trace: false };
        let st = m.run(CODE, &mut insts).unwrap();
        assert_eq!(st.stores, 0);
        // and the branch skipped the body
        assert!(st.groups < 10);
    }
}
