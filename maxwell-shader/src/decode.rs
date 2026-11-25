//! Decoding one 64-bit instruction word.

use crate::isa::*;

/// Bit fields of an instruction word.
#[derive(Clone, Copy)]
struct W(u64);

impl W {
    #[inline]
    fn f(self, pos: u32, len: u32) -> u32 {
        ((self.0 >> pos) & ((1u64 << len) - 1)) as u32
    }
    #[inline]
    fn b(self, pos: u32) -> bool {
        self.0 >> pos & 1 != 0
    }
    /// Sign-extended field.
    fn s(self, pos: u32, len: u32) -> i32 {
        ((self.f(pos, len) << (32 - len)) as i32) >> (32 - len)
    }
    fn d(self) -> Reg {
        self.f(0, 8) as u8
    }
    fn a(self) -> Reg {
        self.f(8, 8) as u8
    }
    fn rb(self) -> Reg {
        self.f(20, 8) as u8
    }
    fn rc(self) -> Reg {
        self.f(39, 8) as u8
    }
    fn cb(self) -> Src {
        Src::Cb(self.f(34, 5) as u8, self.f(20, 14) << 2)
    }
    /// 20-bit immediate, sign-extended.
    fn imm_i(self) -> Src {
        let v = self.f(20, 19) | self.f(56, 1) << 19;
        Src::Imm(((v << 12) as i32 >> 12) as u32)
    }
    /// 20-bit immediate: the top of a float.
    fn imm_f(self) -> Src {
        Src::Imm((self.f(20, 19) | self.f(56, 1) << 19) << 12)
    }
    fn imm32(self) -> Src {
        Src::Imm(self.f(20, 32))
    }
    fn pred(self, pos: u32, not: u32) -> Pred {
        Pred { idx: self.f(pos, 3) as u8, not: self.b(not) }
    }
    fn p3(self, pos: u32) -> u8 {
        self.f(pos, 3) as u8
    }
    fn m(self, abs: u32, neg: u32) -> Mod {
        Mod { abs: self.b(abs), neg: self.b(neg) }
    }
    fn rnd(self, pos: u32) -> Rnd {
        [Rnd::N, Rnd::M, Rnd::P, Rnd::Z][self.f(pos, 2) as usize]
    }
    fn fmz(self, pos: u32) -> Fmz {
        Fmz { ftz: self.b(pos), dnz: self.b(pos + 1) }
    }
    fn bop(self, pos: u32) -> Option<BoolOp> {
        Some(match self.f(pos, 2) {
            0 => BoolOp::And,
            1 => BoolOp::Or,
            2 => BoolOp::Xor,
            _ => return None,
        })
    }
    fn mem(self, pos: u32) -> Option<MemTy> {
        Some(match self.f(pos, 3) {
            0 => MemTy::U8,
            1 => MemTy::S8,
            2 => MemTy::U16,
            3 => MemTy::S16,
            4 => MemTy::B32,
            5 => MemTy::B64,
            6 => MemTy::B128,
            _ => return None,
        })
    }
    fn atom_op(self, pos: u32) -> Option<AtomOp> {
        Some(match self.f(pos, 4) {
            0 => AtomOp::Add,
            1 => AtomOp::Min,
            2 => AtomOp::Max,
            3 => AtomOp::Inc,
            4 => AtomOp::Dec,
            5 => AtomOp::And,
            6 => AtomOp::Or,
            7 => AtomOp::Xor,
            8 => AtomOp::Exch,
            _ => return None,
        })
    }
    fn atom_ty(self, pos: u32) -> Option<AtomTy> {
        Some(match self.f(pos, 3) {
            0 => AtomTy::U32,
            1 => AtomTy::S32,
            2 => AtomTy::U64,
            3 => AtomTy::F32,
            5 => AtomTy::S64,
            _ => return None,
        })
    }
}

/// The second operand of the common three forms.
#[derive(Clone, Copy, PartialEq)]
enum Form {
    Reg,
    Cb,
    Imm,
}

/// Decodes the instruction at code address `addr`. Jump targets in the result are code
/// addresses; an address of a scheduling word means the instruction after it.
pub fn decode(addr: u32, word: u64) -> Inst {
    let w = W(word);
    let pred = w.pred(16, 19);
    let op = decode_op(addr, w).unwrap_or(Op::Invalid(word));
    // These have no predicate field; nouveau leaves it zero.
    let pred = match op {
        Op::Cal { .. }
        | Op::Pret { .. }
        | Op::Ssy { .. }
        | Op::Pbk { .. }
        | Op::Pcnt { .. }
        | Op::Sam
        | Op::Ram => PT,
        _ => pred,
    };
    Inst { pred, op }
}

/// Skips a scheduling word.
pub fn past_sched(addr: u32) -> u32 {
    if addr & 0x1f == 0 {
        addr.wrapping_add(8)
    } else {
        addr
    }
}

fn decode_op(addr: u32, w: W) -> Option<Op> {
    let top = w.f(48, 16);

    // Instructions with 32-bit immediates and other wide fields.
    match top >> 10 {
        0x00 if top & 0xfff0 == 0x0100 => {
            return Some(Op::Mov { d: w.d(), b: w.imm32(), lanes: w.f(12, 4) as u8 });
        }
        0x00 if top & 0xfe00 == 0x0200 => return lop3(w, w.cb(), 48, None, 56),
        0x0f => return lop3(w, w.imm_i(), 48, None, 57),
        0x01 => {
            return Some(Op::Lop {
                d: w.d(),
                a: w.a(),
                b: w.imm32(),
                not_a: w.b(55),
                not_b: w.b(56),
                op: w.f(53, 2) as u8,
                x: w.b(57),
                cc: w.b(52),
                test: 0,
                pd: 7,
            })
        }
        0x02 => {
            return Some(Op::FAdd {
                d: w.d(),
                a: w.a(),
                am: w.m(54, 56),
                b: w.imm32(),
                bm: w.m(57, 53),
                ftz: w.b(55),
                sat: false,
                cc: w.b(52),
                rnd: Rnd::N,
            })
        }
        0x03 => {
            return Some(Op::Ffma {
                d: w.d(),
                a: w.a(),
                b: w.imm32(),
                c: Src::Reg(w.d()),
                neg_ab: w.b(56),
                neg_c: w.b(57),
                fmz: w.fmz(53),
                sat: w.b(55),
                cc: w.b(52),
                rnd: Rnd::N,
            })
        }
        0x05 => {
            return Some(Op::IScAdd {
                d: w.d(),
                a: w.a(),
                b: w.imm32(),
                shift: w.f(53, 5) as u8,
                neg_a: false,
                neg_b: false,
                cc: w.b(52),
            })
        }
        0x07 => {
            return Some(match top >> 8 & 3 {
                0 | 1 => Op::IAdd {
                    d: w.d(),
                    a: w.a(),
                    b: w.imm32(),
                    neg_a: w.b(56),
                    neg_b: false,
                    x: w.b(53),
                    sat: w.b(54),
                    cc: w.b(52),
                },
                2 => Op::FMul {
                    d: w.d(),
                    a: w.a(),
                    b: w.imm32(),
                    neg: false,
                    fmz: w.fmz(53),
                    sat: w.b(55),
                    cc: w.b(52),
                    rnd: Rnd::N,
                    scale: 1.0,
                },
                _ => Op::IMul {
                    d: w.d(),
                    a: w.a(),
                    b: w.imm32(),
                    sa: w.b(54),
                    sb: w.b(55),
                    hi: w.b(53),
                    cc: w.b(52),
                },
            })
        }
        _ => {}
    }

    // VADD
    if top & 0xfc00 == 0x2000 {
        let b_reg = w.b(50);
        let (b, b_bits, b_sel) = if b_reg {
            match w.f(28, 3) {
                k @ 0..=3 => (Src::Reg(w.rb()), 8, k as u8),
                4 => (Src::Reg(w.rb()), 16, 0),
                5 => (Src::Reg(w.rb()), 16, 2),
                _ => (Src::Reg(w.rb()), 32, 0),
            }
        } else {
            let v = w.f(20, 16);
            (Src::Imm(if w.b(49) { v as u16 as i16 as i32 as u32 } else { v }), 32, 0)
        };
        return Some(Op::Vadd {
            d: w.d(),
            a: w.a(),
            a_bits: if w.b(38) { 16 } else { 8 },
            a_sel: w.f(36, 2) as u8,
            a_signed: w.b(48),
            b,
            b_bits,
            b_sel,
            b_signed: w.b(49),
            c: w.rc(),
            op: w.f(51, 3) as u8,
            cc: w.b(47),
        });
    }
    // LEA.HI with a constant: its own layout
    if top & 0xfc00 == 0x1800 {
        return Some(Op::Lea {
            d: w.d(),
            a: w.a(),
            b: w.cb(),
            hi: Some(w.rc()),
            shift: w.f(51, 5) as u8,
            neg_a: w.b(56),
            x: w.b(57),
            cc: w.b(47),
            pd: w.f(48, 3) as u8,
        });
    }
    // Three-source operations: which of the sources is the constant is in the opcode.
    if let Some(op) = three_source(w, top) {
        return Some(op);
    }
    // Two-source operations in three forms.
    let (form, sel) = if top & 0xf000 == 0x5000 {
        (Form::Reg, top & 0x0fff)
    } else if top & 0xf000 == 0x4000 {
        (Form::Cb, top & 0x0fff)
    } else if top & 0xf000 == 0x3000 {
        // 0x58xx pairs with 0x30xx, 0x59xx with 0x32xx, 0x5bxx with 0x36xx, 0x5cxx with 0x38xx
        let sel = top & 0x0eff;
        let hi = match sel & 0x0e00 {
            0x000 => 0x800,
            0x200 => 0x900,
            0x600 => 0xb00,
            0x800 => 0xc00,
            _ => 0,
        };
        (Form::Imm, hi | (sel & 0xff))
    } else {
        return other(addr, w, top);
    };
    match two_source(w, form, sel) {
        Some(op) => Some(op),
        None => other(addr, w, top),
    }
}

fn lop3(w: W, b: Src, lut: u32, pd: Option<u32>, x: u32) -> Option<Op> {
    Some(Op::Lop3 {
        d: w.d(),
        a: w.a(),
        b,
        c: w.rc(),
        lut: w.f(lut, 8) as u8,
        x: w.b(x),
        cc: w.b(47),
        test: if pd.is_some() { w.f(36, 2) as u8 } else { 0 },
        pd: pd.map_or(7, |p| w.p3(p)),
    })
}

/// Integer b, float b, and c of a three-source instruction.
struct Abc {
    ib: Src,
    fb: Src,
    c: Src,
}

fn abc(w: W, kind: u32) -> Abc {
    let rb = Src::Reg(w.rb());
    let rc = Src::Reg(w.rc());
    match kind {
        0 => Abc { ib: rb, fb: rb, c: rc },
        1 => Abc { ib: w.cb(), fb: w.cb(), c: rc },
        2 => Abc { ib: w.imm_i(), fb: w.imm_f(), c: rc },
        // the constant is c, the register takes b's place
        _ => Abc { ib: rc, fb: rc, c: w.cb() },
    }
}

fn three_source(w: W, top: u32) -> Option<Op> {
    // FFMA 59/49/32/51, IMAD 5a/4a/34/52: bit 55 and below are fields.
    let kind = match top & 0xff80 {
        0x5980 | 0x5a00 => Some(0),
        0x4980 | 0x4a00 => Some(1),
        0x5180 | 0x5200 => Some(3),
        _ => match top & 0xfe80 {
            0x3280 | 0x3400 => Some(2),
            _ => None,
        },
    };
    if let Some(k) = kind {
        let s = abc(w, k);
        let ffma = matches!(top & 0xff80, 0x5980 | 0x4980 | 0x5180) || top & 0xfe80 == 0x3280;
        return Some(if ffma {
            Op::Ffma {
                d: w.d(),
                a: w.a(),
                b: s.fb,
                c: s.c,
                neg_ab: w.b(48),
                neg_c: w.b(49),
                fmz: w.fmz(53),
                sat: w.b(50),
                cc: w.b(47),
                rnd: w.rnd(51),
            }
        } else {
            Op::IMad {
                d: w.d(),
                a: w.a(),
                b: s.ib,
                c: s.c,
                sa: w.b(48),
                sb: w.b(53),
                hi: w.b(54),
                neg_ab: w.b(51),
                neg_c: w.b(52),
                sat: w.b(50),
                x: w.b(49),
                cc: w.b(47),
            }
        });
    }

    // XMAD: four layouts.
    let xmad = |b: Src, c: Src, hb: bool, psl: bool, mrg: bool, mode: u32, x: bool| Op::Xmad {
        d: w.d(),
        a: w.a(),
        b,
        c,
        ha: w.b(53),
        hb,
        sa: w.b(48),
        sb: w.b(49),
        psl,
        mrg,
        mode: mode as u8,
        x,
        cc: w.b(47),
    };
    let rb = Src::Reg(w.rb());
    let rc = Src::Reg(w.rc());
    if top & 0xffc0 == 0x5b00 {
        return Some(xmad(rb, rc, w.b(35), w.b(36), w.b(37), w.f(50, 3), w.b(38)));
    }
    if top & 0xfec0 == 0x3600 {
        let imm = Src::Imm(w.f(20, 16));
        return Some(xmad(imm, rc, false, w.b(36), w.b(37), w.f(50, 3), w.b(38)));
    }
    if top & 0xfe00 == 0x4e00 {
        return Some(xmad(w.cb(), rc, w.b(52), w.b(55), w.b(56), w.f(50, 2), w.b(54)));
    }
    if top & 0xff80 == 0x5100 {
        return Some(xmad(rc, w.cb(), w.b(52), false, false, w.f(50, 2), w.b(54)));
    }

    // The 5bxx / 4bxx / 36xx / 53xx group.
    let (kind, sel) = match top & 0xff00 {
        0x5b00 => (0, top & 0xff),
        0x4b00 => (1, top & 0xff),
        0x5300 => (3, top & 0xff),
        0x3600 | 0x3700 => (2, top & 0xff),
        _ => return None,
    };
    let s = abc(w, kind);
    Some(match sel {
        0x40..=0x4f => Op::ICmp {
            d: w.d(),
            a: w.a(),
            b: s.ib,
            c: s.c,
            cond: w.f(49, 3) as u8,
            signed: w.b(48),
        },
        0x70..=0x7f => Op::Dfma {
            d: w.d(),
            a: w.a(),
            b: s.fb,
            c: s.c,
            neg_ab: w.b(48),
            neg_c: w.b(49),
            cc: w.b(47),
            rnd: w.rnd(50),
        },
        0xa0..=0xaf => Op::FCmp {
            d: w.d(),
            a: w.a(),
            b: s.fb,
            c: s.c,
            cond: w.f(48, 4) as u8,
            ftz: w.b(47),
        },
        0xc0..=0xc7 => {
            Op::Prmt { d: w.d(), a: w.a(), b: s.ib, c: s.c, mode: w.f(48, 3) as u8 }
        }
        0xf0..=0xf7 => Op::Bfi { d: w.d(), a: w.a(), b: s.ib, c: s.c, cc: w.b(47) },
        _ => return None,
    })
}

fn two_source(w: W, form: Form, sel: u32) -> Option<Op> {
    let (ib, fb) = match form {
        Form::Reg => (Src::Reg(w.rb()), Src::Reg(w.rb())),
        Form::Cb => (w.cb(), w.cb()),
        Form::Imm => (w.imm_i(), w.imm_f()),
    };
    // Modifiers of an immediate are not encoded: the bits are shared with other forms but are
    // zero there, so reading them is harmless.
    let fset = || {
        Some(FCmpArgs {
            a: w.a(),
            am: w.m(54, 43),
            b: fb,
            bm: w.m(44, 53),
            cond: w.f(48, 4) as u8,
            ftz: w.b(55),
            bop: w.bop(45)?,
            p: w.pred(39, 42),
        })
    };
    let fsetp = || {
        Some(FCmpArgs {
            a: w.a(),
            am: w.m(7, 43),
            b: fb,
            bm: w.m(44, 6),
            cond: w.f(48, 4) as u8,
            ftz: w.b(47),
            bop: w.bop(45)?,
            p: w.pred(39, 42),
        })
    };
    let icmp = || {
        Some(ICmpArgs {
            a: w.a(),
            b: ib,
            cond: w.f(49, 3) as u8,
            signed: w.b(48),
            x: w.b(43),
            bop: w.bop(45)?,
            p: w.pred(39, 42),
        })
    };
    match sel & 0xf80 {
        0x800 | 0x880 => return Some(Op::FSet { d: w.d(), cmp: fset()?, bf: w.b(52), cc: w.b(47) }),
        0x900 => return Some(Op::DSet { d: w.d(), cmp: fset()?, bf: w.b(52), cc: w.b(47) }),
        _ => {}
    }
    match sel & 0xff0 {
        0xb50 => return Some(Op::ISet { d: w.d(), cmp: icmp()?, bf: w.b(44), cc: w.b(47) }),
        0xb60 => return Some(Op::ISetP { pd: w.p3(3), pn: w.p3(0), cmp: icmp()? }),
        0xb80 => return Some(Op::DSetP { pd: w.p3(3), pn: w.p3(0), cmp: fsetp()? }),
        0xbb0 => return Some(Op::FSetP { pd: w.p3(3), pn: w.p3(0), cmp: fsetp()? }),
        0xcc0 => {
            let reg = form == Form::Reg;
            return Some(Op::IAdd3 {
                d: w.d(),
                a: w.a(),
                b: ib,
                c: w.rc(),
                neg: [w.b(51), w.b(50), w.b(49)],
                half: if reg {
                    [w.f(35, 2) as u8, w.f(33, 2) as u8, w.f(31, 2) as u8]
                } else {
                    [0; 3]
                },
                shift: if reg { w.f(37, 2) as u8 } else { 0 },
                x: w.b(48),
                cc: w.b(47),
            });
        }
        _ => {}
    }
    let d = w.d();
    let a = w.a();
    let cc = w.b(47);
    Some(match sel & 0xff8 {
        0xbe0 if form == Form::Reg => return lop3(w, ib, 28, Some(48), 38),
        0xbf8 | 0xcf8 => {
            if form == Form::Cb {
                return None;
            }
            // The type field sits on top of the immediate.
            let b = if form == Form::Imm { Src::Imm(w.f(20, 17)) } else { ib };
            Op::Shf {
                d,
                lo: a,
                hi: w.rc(),
                b,
                right: sel & 0xf00 == 0xc00,
                high: w.b(48),
                wrap: w.b(50),
                wide: w.f(37, 2) >= 2,
                signed: w.f(37, 2) == 3,
                cc,
            }
        }
        0xc00 => Op::Bfe { d, a, b: ib, signed: w.b(48), rev: w.b(40), cc },
        0xc08 => Op::PopC { d, b: ib, not: w.b(40) },
        0xc10 => Op::IAdd {
            d,
            a,
            b: ib,
            neg_a: w.b(49),
            neg_b: w.b(48),
            x: w.b(43),
            sat: w.b(50),
            cc,
        },
        0xce8 => Op::P2r { d, a, b: ib, cc: w.b(40), byte: w.f(41, 2) as u8 },
        0xcf0 => Op::R2p { a, b: ib, cc: w.b(40), byte: w.f(41, 2) as u8 },
        0xc88 if form == Form::Reg => Op::Fchk {
            pd: w.f(0, 3) as u8,
            a,
            b: ib,
            abs_a: w.b(46),
            neg_a: w.b(48),
            abs_b: w.b(49),
            neg_b: w.b(45),
        },
        // LEA (the low three bits of the selector are the predicate written)
        0xbd0 => Op::Lea { d, a, b: ib, hi: None, shift: w.f(39, 5) as u8, neg_a: w.b(45), x: w.b(46), cc, pd: w.f(48, 3) as u8 },
        0xbd8 if form == Form::Reg => Op::Lea {
            d,
            a,
            b: ib,
            hi: Some(w.rc()),
            shift: w.f(28, 5) as u8,
            neg_a: w.b(37),
            x: w.b(38),
            cc,
            pd: w.f(48, 3) as u8,
        },
        0xc18 => Op::IScAdd {
            d,
            a,
            b: ib,
            shift: w.f(39, 5) as u8,
            neg_a: w.b(49),
            neg_b: w.b(48),
            cc,
        },
        0xc20 => Op::IMnMx {
            d,
            a,
            b: ib,
            signed: w.b(48),
            part: w.f(43, 2) as u8,
            cc,
            p: w.pred(39, 42),
        },
        0xc28 => Op::Shr {
            d,
            a,
            b: ib,
            signed: w.b(48),
            wrap: w.b(39),
            rev: w.b(40),
            x: w.b(44),
            cc,
        },
        0xc30 => Op::Flo { d, b: ib, not: w.b(40), signed: w.b(48), shift: w.b(41), cc },
        0xc38 => Op::IMul { d, a, b: ib, sa: w.b(40), sb: w.b(41), hi: w.b(39), cc },
        0xc40 => Op::Lop {
            d,
            a,
            b: ib,
            not_a: w.b(39),
            not_b: w.b(40),
            op: w.f(41, 2) as u8,
            x: w.b(43),
            cc,
            test: w.f(44, 2) as u8,
            pd: w.p3(48),
        },
        0xc48 => Op::Shl { d, a, b: ib, wrap: w.b(39), x: w.b(43), cc },
        0xc50 => Op::DMnMx {
            d,
            a,
            am: w.m(46, 48),
            b: fb,
            bm: w.m(49, 45),
            cc,
            p: w.pred(39, 42),
        },
        0xc58 => Op::FAdd {
            d,
            a,
            am: w.m(46, 48),
            b: fb,
            bm: w.m(49, 45),
            ftz: w.b(44),
            sat: w.b(50),
            cc,
            rnd: w.rnd(39),
        },
        0xc60 => Op::FMnMx {
            d,
            a,
            am: w.m(46, 48),
            b: fb,
            bm: w.m(49, 45),
            ftz: w.b(44),
            cc,
            p: w.pred(39, 42),
        },
        0xc68 => Op::FMul {
            d,
            a,
            b: fb,
            neg: w.b(48),
            fmz: w.fmz(44),
            sat: w.b(50),
            cc,
            rnd: w.rnd(39),
            scale: [1.0, 0.5, 0.25, 0.125, 8.0, 4.0, 2.0, 1.0][w.f(41, 3) as usize],
        },
        0xc70 => {
            Op::DAdd { d, a, am: w.m(46, 48), b: fb, bm: w.m(49, 45), cc, rnd: w.rnd(39) }
        }
        0xc80 => Op::DMul { d, a, b: fb, neg: w.b(48), cc, rnd: w.rnd(39) },
        0xc90 => Op::Rro { d, b: fb, m: w.m(49, 45) },
        0xc98 => Op::Mov { d, b: ib, lanes: w.f(39, 4) as u8 },
        0xca0 => Op::Sel { d, a, b: ib, p: w.pred(39, 42) },
        0xca8 => Op::F2F {
            d,
            b: fb,
            m: w.m(49, 45),
            from: 1 << w.f(10, 2),
            to: 1 << w.f(8, 2),
            rnd: w.rnd(39),
            int: w.b(42),
            high: w.b(41),
            ftz: w.b(44),
            sat: w.b(50),
            cc,
        },
        0xcb0 => Op::F2I {
            d,
            b: fb,
            m: w.m(49, 45),
            from: 1 << w.f(10, 2),
            to: IntTy { bytes: 1 << w.f(8, 2), signed: w.b(12) },
            rnd: w.rnd(39),
            high: w.b(41),
            ftz: w.b(44),
            cc,
        },
        0xcb8 => Op::I2F {
            d,
            b: ib,
            m: w.m(49, 45),
            from: IntTy { bytes: 1 << w.f(10, 2), signed: w.b(13) },
            to: 1 << w.f(8, 2),
            sel: w.f(41, 2) as u8,
            rnd: w.rnd(39),
            cc,
        },
        0xce0 => Op::I2I {
            d,
            b: ib,
            m: w.m(49, 45),
            from: IntTy { bytes: 1 << w.f(10, 2), signed: w.b(13) },
            to: IntTy { bytes: 1 << w.f(8, 2), signed: w.b(12) },
            sel: w.f(41, 2) as u8,
            sat: w.b(50),
            cc,
        },
        _ => return None,
    })
}

fn other(addr: u32, w: W, top: u32) -> Option<Op> {
    let rel = || past_sched(addr.wrapping_add(8).wrapping_add(w.s(20, 24) as u32));
    let abs = || past_sched(w.f(20, 32));
    let cond = w.f(0, 5) as u8;
    let d = w.d();
    let a = w.a();

    match top & 0xfff0 {
        0xe200 if !w.b(5) => return Some(Op::Brx { a, base: w.f(20, 32), cond }),
        0xe210 if !w.b(5) => return Some(Op::Bra { t: abs(), cond, all: w.b(7) }),
        0xe220 if !w.b(5) => return Some(Op::Cal { t: abs() }),
        0xe240 if !w.b(5) => return Some(Op::Bra { t: rel(), cond, all: w.b(7) }),
        0xe250 if !w.b(5) => {
            let base = addr.wrapping_add(8).wrapping_add(w.s(20, 24) as u32);
            return Some(Op::Brx { a, base, cond });
        }
        0xe260 if !w.b(5) => return Some(Op::Cal { t: rel() }),
        0xe270 if !w.b(5) => return Some(Op::Pret { t: rel() }),
        0xe290 if !w.b(5) => return Some(Op::Ssy { t: rel() }),
        0xe2a0 if !w.b(5) => return Some(Op::Pbk { t: rel() }),
        0xe2b0 if !w.b(5) => return Some(Op::Pcnt { t: rel() }),
        0xe300 => return Some(Op::Exit { cond }),
        0xe320 => return Some(Op::Ret { cond }),
        0xe330 => return Some(Op::Kil { cond }),
        0xe340 => return Some(Op::Brk { cond }),
        0xe350 => return Some(Op::Cont { cond }),
        0xe370 => return Some(Op::Sam),
        0xe380 => return Some(Op::Ram),
        0xe3a0 => return Some(Op::Bpt),
        0xeef0 => {
            let ty = if w.b(49) { AtomTy::U64 } else { AtomTy::U32 };
            return Some(Op::Atom {
                d,
                a,
                b: w.rb(),
                off: w.s(28, 20),
                op: AtomOp::Cas,
                ty,
                wide: w.b(48),
            });
        }
        0xee40 | 0xee50 => {
            let ty = if w.b(52) { AtomTy::U64 } else { AtomTy::U32 };
            return Some(Op::AtomS { d, a, b: w.rb(), off: w.s(30, 22) << 2, op: AtomOp::Cas, ty });
        }
        _ => {}
    }

    match top & 0xfff8 {
        0x5080 => {
            let op = match w.f(20, 4) {
                0 => Mufu::Cos,
                1 => Mufu::Sin,
                2 => Mufu::Ex2,
                3 => Mufu::Lg2,
                4 => Mufu::Rcp,
                5 => Mufu::Rsq,
                6 => Mufu::Rcp64h,
                7 => Mufu::Rsq64h,
                8 => Mufu::Sqrt,
                _ => return None,
            };
            return Some(Op::Mufu { d, a, m: w.m(46, 48), op, sat: w.b(50) });
        }
        0x5088 | 0x5090 => {
            let (x, y, z) = (w.pred(12, 15), w.pred(29, 32), w.pred(39, 42));
            let (op1, op2) = (w.bop(24)?, w.bop(45)?);
            return Some(if top & 0xfff8 == 0x5088 {
                Op::PSet { d, a: x, b: y, c: z, op1, op2, bf: w.b(44), cc: w.b(47) }
            } else {
                Op::PSetP { pd: w.p3(3), pn: w.p3(0), a: x, b: y, c: z, op1, op2 }
            });
        }
        0x50b0 => return Some(Op::Nop),
        0x50c8 | 0xf0c8 => return Some(Op::S2R { d, sys: w.f(20, 8) as u8 }),
        0x50d8 => {
            let op = match w.f(48, 2) {
                0 => VoteOp::All,
                1 => VoteOp::Any,
                2 => VoteOp::Eq,
                _ => return None,
            };
            return Some(Op::Vote { d, pd: w.p3(45), p: w.pred(39, 42), op });
        }
        0x50f8 => {
            return Some(Op::FSwzAdd {
                d,
                a,
                b: w.rb(),
                ops: w.f(28, 8) as u8,
                ftz: w.b(44),
                cc: w.b(47),
                rnd: w.rnd(39),
            })
        }
        0xef10 => {
            let op = [ShflOp::Idx, ShflOp::Up, ShflOp::Down, ShflOp::Bfly][w.f(30, 2) as usize];
            let b = if w.b(28) { Src::Imm(w.f(20, 5)) } else { Src::Reg(w.rb()) };
            let c = if w.b(29) { Src::Imm(w.f(34, 13)) } else { Src::Reg(w.rc()) };
            return Some(Op::Shfl { d, pd: w.p3(48), a, b, c, op });
        }
        0xeed0 => return Some(Op::Ld { d, a, off: w.s(20, 24), ty: w.mem(48)?, wide: w.b(45) }),
        0xeed8 => {
            return Some(Op::St { r: d, a, off: w.s(20, 24), ty: w.mem(48)?, wide: w.b(45) })
        }
        0xef40 => return Some(Op::Ldl { d, a, off: w.s(20, 24), ty: w.mem(48)? }),
        0xef48 => return Some(Op::Lds { d, a, off: w.s(20, 24), ty: w.mem(48)? }),
        0xef50 => return Some(Op::Stl { r: d, a, off: w.s(20, 24), ty: w.mem(48)? }),
        0xef58 => return Some(Op::Sts { r: d, a, off: w.s(20, 24), ty: w.mem(48)? }),
        0xef80 | 0xef98 | 0xf0f0 => return Some(Op::Fence),
        0xef90 => {
            return Some(Op::Ldc {
                d,
                idx: w.f(36, 5) as u8,
                a,
                off: w.s(20, 16),
                ty: w.mem(48)?,
                mode: w.f(44, 2) as u8,
            })
        }
        0xefa0 => {
            return Some(Op::Al2p {
                d,
                a,
                addr: w.f(20, 11) as u16,
                n: w.f(47, 2) as u8 + 1,
                output: w.b(32),
            })
        }
        0xefd0 => return Some(Op::Isberd { d, a }),
        0xefd8 => {
            return Some(Op::Ald {
                d,
                a,
                v: w.rc(),
                addr: w.f(20, 10) as u16,
                n: w.f(47, 2) as u8 + 1,
                patch: w.b(31),
                output: w.b(32),
            })
        }
        0xefe8 => {
            let what = match w.f(31, 3) {
                0 => Pixld::Count,
                1 => Pixld::CovMask,
                2 => Pixld::Covered,
                3 => Pixld::Offset,
                4 => Pixld::CentroidOffset,
                5 => Pixld::SampleId,
                n => Pixld::Other(n as u8),
            };
            return Some(Op::Pixld { d, a, what });
        }
        0xeff0 => {
            return Some(Op::Ast {
                r: d,
                a,
                v: w.rc(),
                addr: w.f(20, 10) as u16,
                n: w.f(47, 2) as u8 + 1,
                patch: w.b(31),
            })
        }
        0xf0a8 => return Some(Op::Bar),
        0xf0f8 => return Some(Op::Sync { cond }),
        0xebf8 => {
            let op = match w.f(23, 3) {
                0 => AtomOp::Add,
                1 => AtomOp::Min,
                2 => AtomOp::Max,
                3 => AtomOp::Inc,
                4 => AtomOp::Dec,
                5 => AtomOp::And,
                6 => AtomOp::Or,
                _ => AtomOp::Xor,
            };
            // RED: the destination field holds the operand.
            return Some(Op::Atom {
                d: RZ,
                a,
                b: d,
                off: w.s(28, 20),
                op,
                ty: w.atom_ty(20)?,
                wide: w.b(48),
            });
        }
        0xfbe0 | 0xebe0 => {
            let b = if top & 0xfff8 == 0xfbe0 { Src::Reg(w.rb()) } else { w.cb() };
            return Some(Op::Out { d, a, b, emit: w.b(39), cut: w.b(40) });
        }
        0xf6e0 | 0xf7e0 => {
            return Some(Op::Out { d, a, b: w.imm_i(), emit: w.b(39), cut: w.b(40) })
        }
        _ => {}
    }

    match top & 0xffe0 {
        // CCTL: the offset reaches into the opcode's low bits.
        0xef60 => return Some(Op::Fence),
        0xeb00 | 0xeb20 => {
            let store = top & 0x20 != 0;
            let raw = w.b(52);
            let ty = if raw && !store { w.mem(20)? } else { MemTy::B128 };
            let mask = if raw && !store { 0xf } else { w.f(20, 4) as u8 };
            let s = Surf { dim: surf_dim(w)?, raw, ty, mask, coord: a, handle: surf_handle(w), r: d };
            return Some(if store { Op::Sust(s) } else { Op::Suld(s) });
        }
        0xea60 | 0xeac0 => {
            let cas = top & 0xffe0 == 0xeac0;
            let ty = w.atom_ty(36)?;
            let op = if cas { AtomOp::Cas } else { w.atom_op(29)? };
            let mem = if ty.bytes() == 8 { MemTy::B64 } else { MemTy::B32 };
            let s = Surf {
                dim: surf_dim(w)?,
                raw: true,
                ty: mem,
                mask: 1,
                coord: a,
                handle: surf_handle(w),
                r: d,
            };
            return Some(Op::Suatom { s, b: w.rb(), op, ty });
        }
        _ => {}
    }

    match top >> 8 {
        0xe0 => {
            let mode = [IpaMode::Pass, IpaMode::MulW, IpaMode::Constant, IpaMode::State]
                [w.f(54, 2) as usize];
            let loc = match w.f(52, 2) {
                0 => IpaLoc::Default,
                1 => IpaLoc::Centroid,
                2 => IpaLoc::Offset,
                _ => return None,
            };
            return Some(Op::Ipa {
                d,
                addr: w.f(28, 10) as u16,
                idx: w.b(38).then_some(a),
                mul: w.rb(),
                offset: w.rc(),
                mode,
                loc,
                sat: w.b(51),
            });
        }
        0xec => {
            let ty = [AtomTy::U32, AtomTy::S32, AtomTy::U64, AtomTy::S64][w.f(28, 2) as usize];
            return Some(Op::AtomS {
                d,
                a,
                b: w.rb(),
                off: w.s(30, 22) << 2,
                op: w.atom_op(52)?,
                ty,
            });
        }
        0xed => {
            return Some(Op::Atom {
                d,
                a,
                b: w.rb(),
                off: w.s(28, 20),
                op: w.atom_op(52)?,
                ty: w.atom_ty(49)?,
                wide: w.b(48),
            })
        }
        _ => {}
    }
    match top >> 13 {
        4 => {
            return Some(Op::Ld {
                d,
                a,
                off: w.f(20, 32) as i32,
                ty: w.mem(53)?,
                wide: w.b(52),
            })
        }
        5 => {
            return Some(Op::St {
                r: d,
                a,
                off: w.f(20, 32) as i32,
                ty: w.mem(53)?,
                wide: w.b(52),
            })
        }
        _ => {}
    }
    tex(w, top).map(Op::Tex)
}

fn surf_dim(w: W) -> Option<SurfDim> {
    Some(match w.f(33, 3) {
        0 => SurfDim::D1,
        1 => SurfDim::Buffer,
        2 => SurfDim::D1Array,
        3 => SurfDim::D2,
        4 => SurfDim::D2Array,
        5 => SurfDim::D3,
        _ => return None,
    })
}

fn surf_handle(w: W) -> Src {
    if w.b(51) {
        Src::Imm(w.f(36, 13))
    } else {
        Src::Reg(w.rc())
    }
}

/// The register `n` after `r`, in a vector.
pub fn reg_at(r: Reg, n: u32) -> Reg {
    if r == RZ {
        RZ
    } else {
        r.wrapping_add(n as u8)
    }
}

/// Hands out consecutive registers of a vector operand.
struct Cursor(Reg);

impl Cursor {
    fn next(&mut self) -> Reg {
        let r = self.0;
        self.0 = reg_at(r, 1);
        r
    }
}

const NO_TEX: Tex = Tex {
    kind: TexKind::Sample,
    dim: TexDim::D2,
    array: false,
    mask: 0xf,
    d0: RZ,
    d1: RZ,
    scalar: false,
    handle: RZ,
    bindless: false,
    index: 0,
    coord: [RZ; 3],
    layer: RZ,
    lod_mode: LodMode::Auto,
    lod: RZ,
    shadow: false,
    dref: RZ,
    ms: false,
    sample: RZ,
    offset: RZ,
    offset_kind: OffsetKind::None,
    deriv: RZ,
    comp: 0,
    ndv: false,
    live_only: false,
};

fn coords(dim: TexDim) -> usize {
    match dim {
        TexDim::D1 => 1,
        TexDim::D2 => 2,
        TexDim::D3 | TexDim::Cube => 3,
    }
}

fn tex(w: W, top: u32) -> Option<Tex> {
    let dim = [TexDim::D1, TexDim::D2, TexDim::D3, TexDim::Cube][w.f(29, 2) as usize];
    let array = w.b(28);
    let mut t = Tex {
        dim,
        array,
        mask: w.f(31, 4) as u8,
        d0: w.d(),
        index: w.f(36, 13) as u16,
        live_only: w.b(49),
        ndv: w.b(35),
        ..NO_TEX
    };
    let mut a = Cursor(w.a());
    let mut b = Cursor(w.rb());

    // Array index, then coordinates.
    let base = |t: &mut Tex, a: &mut Cursor| {
        if array {
            t.layer = a.next();
        }
        for c in t.coord.iter_mut().take(coords(dim)) {
            *c = a.next();
        }
    };

    let lod_modes = [LodMode::Auto, LodMode::Zero, LodMode::Bias, LodMode::Level];
    if top & 0xfe38 == 0xc038 || top & 0xfff8 == 0xdeb8 {
        t.bindless = top & 0xff00 == 0xde00;
        let (lodm, aoffi) = if t.bindless { (37, 36) } else { (55, 54) };
        t.lod_mode = lod_modes[w.f(lodm, 2) as usize];
        t.shadow = w.b(50);
        base(&mut t, &mut a);
        if t.bindless {
            t.handle = b.next();
        }
        if matches!(t.lod_mode, LodMode::Bias | LodMode::Level) {
            t.lod = b.next();
        }
        if w.b(aoffi) {
            t.offset = b.next();
            t.offset_kind = OffsetKind::Nibbles;
        }
        if t.shadow {
            t.dref = b.next();
        }
        return Some(t);
    }
    if top & 0xff38 == 0xdc38 || top & 0xff38 == 0xdd38 {
        t.kind = TexKind::Fetch;
        t.bindless = top & 0xff00 == 0xdd00;
        t.ndv = false;
        t.ms = w.b(50);
        t.lod_mode = if w.b(55) { LodMode::Level } else { LodMode::Zero };
        base(&mut t, &mut a);
        if t.bindless {
            t.handle = b.next();
        }
        if t.lod_mode == LodMode::Level {
            t.lod = b.next();
        }
        if w.b(35) {
            t.offset = b.next();
            t.offset_kind = OffsetKind::Nibbles;
        }
        if t.ms {
            t.sample = b.next();
        }
        return Some(t);
    }
    if top & 0xfc38 == 0xc838 || top & 0xfff8 == 0xdef8 {
        t.kind = TexKind::Gather;
        t.bindless = top & 0xff00 == 0xde00;
        let (comp, ptp, aoffi) = if t.bindless { (38, 37, 36) } else { (56, 55, 54) };
        t.comp = w.f(comp, 2) as u8;
        t.shadow = w.b(50);
        t.lod_mode = LodMode::Zero;
        base(&mut t, &mut a);
        if t.bindless {
            t.handle = b.next();
        }
        if w.b(ptp) {
            t.offset = b.next();
            b.next();
            t.offset_kind = OffsetKind::PerTexel;
        } else if w.b(aoffi) {
            t.offset = b.next();
            t.offset_kind = OffsetKind::Bytes;
        }
        if t.shadow {
            t.dref = b.next();
        }
        return Some(t);
    }
    match top & 0xfff8 {
        0xde38 | 0xde78 => {
            t.kind = TexKind::Grad;
            t.bindless = top & 0xfff8 == 0xde78;
            t.ndv = true;
            if t.bindless {
                t.handle = a.next();
            }
            for c in t.coord.iter_mut().take(coords(dim)) {
                *c = a.next();
            }
            if array || w.b(35) {
                let r = a.next();
                if array {
                    t.layer = r;
                }
                if w.b(35) {
                    t.offset = r;
                    t.offset_kind = OffsetKind::NibblesHigh;
                }
            }
            t.deriv = b.0;
            return Some(t);
        }
        0xdf48 | 0xdf50 => {
            t.kind = TexKind::Query;
            t.bindless = top & 0xfff8 == 0xdf50;
            t.comp = w.f(22, 6) as u8;
            t.array = false;
            t.ndv = false;
            if t.bindless {
                t.handle = a.next();
            }
            t.lod = a.next();
            t.lod_mode = LodMode::Level;
            return Some(t);
        }
        0xdf58 | 0xdf60 => {
            t.kind = TexKind::QueryLod;
            t.bindless = top & 0xfff8 == 0xdf60;
            base(&mut t, &mut a);
            if t.bindless {
                t.handle = b.next();
            }
            return Some(t);
        }
        _ => {}
    }

    // The scalar forms: up to two registers per operand, two destinations of up to two.
    t.scalar = true;
    t.ndv = false;
    t.array = false;
    t.d1 = w.f(28, 8) as u8;
    let (ra, rb) = (w.a(), w.rb());
    let (a1, b1) = (reg_at(ra, 1), reg_at(rb, 1));
    let mask3 = w.f(50, 3) as usize;
    let mask = || -> Option<u8> {
        let m = if t.d1 == RZ {
            [0x1, 0x2, 0x4, 0x8, 0x3, 0x9, 0xa, 0xc][mask3]
        } else {
            [0x7, 0xb, 0xd, 0xe, 0xf, 0, 0, 0][mask3]
        };
        (m != 0).then_some(m)
    };
    match top & 0xfe00 {
        0xd800 => {
            t.mask = mask()?;
            let target = w.f(53, 4);
            use LodMode::*;
            use TexDim::*;
            (t.dim, t.lod_mode) = match target {
                0 => (D1, Zero),
                1 | 4 | 7 => (D2, Auto),
                2 | 6 | 8 | 9 => (D2, Zero),
                3 | 5 => (D2, Level),
                10 => (D3, Auto),
                11 => (D3, Zero),
                12 => (Cube, Auto),
                13 => (Cube, Level),
                _ => return None,
            };
            t.array = matches!(target, 7..=9);
            t.shadow = matches!(target, 4..=6 | 9);
            match target {
                0 => t.coord[0] = ra,
                1 | 2 => t.coord = [ra, rb, RZ],
                3 => (t.coord, t.lod) = ([ra, a1, RZ], rb),
                4 | 6 => (t.coord, t.dref) = ([ra, a1, RZ], rb),
                5 => (t.coord, t.lod, t.dref) = ([ra, a1, RZ], rb, b1),
                7 | 8 => (t.layer, t.coord) = (ra, [a1, rb, RZ]),
                9 => (t.layer, t.coord, t.dref) = (ra, [a1, rb, RZ], b1),
                10..=12 => t.coord = [ra, a1, rb],
                _ => (t.coord, t.lod) = ([ra, a1, rb], b1),
            }
            Some(t)
        }
        0xda00 => {
            t.kind = TexKind::Fetch;
            t.mask = mask()?;
            let target = w.f(53, 4);
            t.lod_mode = LodMode::Zero;
            match target {
                0 => (t.dim, t.coord[0]) = (TexDim::D1, ra),
                1 => {
                    (t.dim, t.coord[0], t.lod) = (TexDim::D1, ra, rb);
                    t.lod_mode = LodMode::Level;
                }
                2 => (t.dim, t.coord) = (TexDim::D2, [ra, rb, RZ]),
                4 => {
                    (t.dim, t.coord, t.offset) = (TexDim::D2, [ra, a1, RZ], rb);
                    t.offset_kind = OffsetKind::Nibbles;
                }
                5 => {
                    (t.dim, t.coord, t.lod) = (TexDim::D2, [ra, a1, RZ], rb);
                    t.lod_mode = LodMode::Level;
                }
                6 => {
                    (t.dim, t.coord, t.sample) = (TexDim::D2, [ra, a1, RZ], rb);
                    t.ms = true;
                }
                7 => (t.dim, t.coord) = (TexDim::D3, [ra, a1, rb]),
                8 => {
                    (t.dim, t.layer, t.coord) = (TexDim::D2, ra, [rb, b1, RZ]);
                    t.array = true;
                }
                12 => {
                    (t.dim, t.coord, t.lod, t.offset) = (TexDim::D2, [ra, a1, RZ], rb, b1);
                    t.lod_mode = LodMode::Level;
                    t.offset_kind = OffsetKind::Nibbles;
                }
                _ => return None,
            }
            Some(t)
        }
        _ if top & 0xffc0 == 0xdf00 => {
            t.kind = TexKind::Gather;
            t.dim = TexDim::D2;
            t.lod_mode = LodMode::Zero;
            t.mask = if t.d1 == RZ { 0x3 } else { 0xf };
            t.comp = w.f(52, 2) as u8;
            t.shadow = w.b(50);
            let aoffi = w.b(51);
            if !aoffi && !t.shadow {
                t.coord = [ra, rb, RZ];
            } else {
                t.coord = [ra, a1, RZ];
                let mut b = Cursor(rb);
                if aoffi {
                    t.offset = b.next();
                    t.offset_kind = OffsetKind::Bytes;
                }
                if t.shadow {
                    t.dref = b.next();
                }
            }
            Some(t)
        }
        _ => None,
    }
}
