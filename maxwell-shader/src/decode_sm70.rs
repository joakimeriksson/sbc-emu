//! Volta-and-later (SM70+, here Ampere SM87) shader code into the same internal form as Maxwell's.
//!
//! Instructions are 128 bits with the scheduling controls inside (bits 105 up), so there are no
//! scheduling words. The field layout follows Mesa's NAK encoder (`sm70_encode.rs`).
//!
//! Uniform registers (UR0..UR62) become general registers `UNIFORM_BASE + n`; URZ reads as
//! zero. Every lane holds the same value, which is what a uniform register is. Convergence
//! barriers (BSSY/BSYNC) become SSY/SYNC: the reconvergence label holds the BSYNC, so the SSY's
//! meeting point is the instruction after it (`Program::decode_sm70_at` moves it there).
//! Instructions this decoder does not know decode as `Op::Invalid` with the low 64 bits.

use crate::isa::*;

/// Where uniform registers live among the general ones.
pub const UNIFORM_BASE: u8 = 192;
/// Bytes per instruction.
pub const SIZE: u32 = 16;

#[derive(Clone, Copy)]
struct W(u128);

impl W {
    fn f(self, lo: u32, len: u32) -> u32 {
        ((self.0 >> lo) & ((1u128 << len) - 1)) as u32
    }
    fn b(self, bit: u32) -> bool {
        self.0 >> bit & 1 != 0
    }
    fn sf(self, lo: u32, len: u32) -> i32 {
        let v = self.f(lo, len);
        ((v << (32 - len)) as i32) >> (32 - len)
    }
    fn reg(self, lo: u32) -> Reg {
        self.f(lo, 8) as Reg
    }
    fn ureg(self, lo: u32) -> Reg {
        match self.f(lo, 6) {
            63 => RZ,
            n => UNIFORM_BASE + n as u8,
        }
    }
    fn pred(self, lo: u32, not: u32) -> Pred {
        Pred { idx: self.f(lo, 3) as u8, not: self.b(not) }
    }
    fn pdst(self, lo: u32) -> u8 {
        self.f(lo, 3) as u8
    }
    fn opcode(self) -> u32 {
        self.f(0, 12)
    }
    /// Uniform ALU operations have opcodes 0x80..0x100 (and bit 91 set).
    fn uniform_alu(self) -> bool {
        (0x80..0x100).contains(&(self.opcode() & 0x1ff))
    }
    fn rnd(self, lo: u32) -> Rnd {
        [Rnd::N, Rnd::M, Rnd::P, Rnd::Z][self.f(lo, 2) as usize]
    }
    fn bop(self, lo: u32) -> Option<BoolOp> {
        Some([BoolOp::And, BoolOp::Or, BoolOp::Xor][*[0, 1, 2].get(self.f(lo, 2) as usize)?])
    }
}

/// The operands of an ALU instruction in source order: `a` (bits 24..32), `b` and `c`, wherever
/// the form put them, with their |x| and -x modifiers.
struct Alu {
    a: Reg,
    am: Mod,
    b: Src,
    bm: Mod,
    c: Src,
    cm: Mod,
    /// The form: 1 b and c registers, 2 c immediate, 3 c constant, 4 b immediate,
    /// 5 b constant, 6 b uniform, 7 c uniform.
    form: u32,
}

fn alu(w: W) -> Option<Alu> {
    let uniform = w.uniform_alu();
    let form = w.f(9, 3);
    let reg = |lo| if uniform { w.ureg(lo) } else { w.reg(lo) };
    let m = |abs, neg| Mod { abs: w.b(abs), neg: w.b(neg) };
    let cb = || Src::Cb(w.f(54, 5) as u8, w.f(38, 16));
    let (wide_mod, b_slot) = (m(62, 63), m(74, 75));
    let (b, bm, c, cm) = match form {
        1 => (Src::Reg(reg(32)), wide_mod, Src::Reg(reg(64)), b_slot),
        2 => (Src::Reg(reg(64)), b_slot, Src::Imm(w.f(32, 32)), Mod::default()),
        3 if !w.b(91) => (Src::Reg(reg(64)), b_slot, cb(), wide_mod),
        4 => (Src::Imm(w.f(32, 32)), Mod::default(), Src::Reg(reg(64)), b_slot),
        5 if !w.b(91) => (cb(), wide_mod, Src::Reg(reg(64)), b_slot),
        6 => (Src::Reg(w.ureg(32)), wide_mod, Src::Reg(reg(64)), b_slot),
        7 => (Src::Reg(reg(64)), b_slot, Src::Reg(w.ureg(32)), wide_mod),
        _ => return None,
    };
    Some(Alu { a: reg(24), am: m(73, 72), b, bm, c, cm, form })
}

fn reg_of(s: Src) -> Option<Reg> {
    match s {
        Src::Reg(r) => Some(r),
        _ => None,
    }
}

fn mem_ty(v: u32) -> Option<MemTy> {
    Some(match v {
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

/// A relative branch target: in 4-byte units from the next instruction.
fn rel(addr: u32, w: W) -> u32 {
    let offset = (w.0 >> 34) as i64 & ((1 << 48) - 1);
    let offset = (offset << 16) >> 16;
    (addr as i64 + SIZE as i64 + offset * 4) as u32
}

/// Swaps the b and c inputs of a LOP3 lookup table.
fn lut_swap_bc(lut: u8) -> u8 {
    (0..8).fold(0, |out, i| {
        let (a, b, c) = (i >> 2 & 1, i >> 1 & 1, i & 1);
        let j = a << 2 | c << 1 | b;
        out | (lut >> j & 1) << i
    })
}

/// Decodes the instruction at code address `addr`: usually one operation, two where SM70 does in
/// one instruction what takes two here. Jump targets are code addresses.
pub fn decode(addr: u32, raw: u128) -> Vec<Inst> {
    let w = W(raw);
    let pred = w.pred(12, 15);
    let ops = decode_ops(addr, w).unwrap_or_else(|| vec![Op::Invalid(raw as u64)]);
    ops.into_iter().map(|op| Inst { pred, op }).collect()
}

fn one(op: Op) -> Option<Vec<Op>> {
    Some(vec![op])
}

fn decode_ops(addr: u32, w: W) -> Option<Vec<Op>> {
    let opcode = w.opcode();
    // Whole-opcode instructions first: control flow, memory, attributes.
    match opcode {
        0x918 => return one(Op::Nop),
        // Convergence-barrier bookkeeping, warp synchronization, dependency waits.
        0x355 | 0x356 | 0x956 | 0xf55 | 0x148 | 0x948 | 0x91a => return one(Op::Nop),
        0x947 => {
            if w.b(32) {
                return None;
            }
            let p = w.pred(87, 90);
            // A predicated branch: the guard predicate is the condition.
            if p.idx != 7 || p.not {
                return None;
            }
            return one(Op::Bra { t: rel(addr, w), cond: 15, all: false });
        }
        0x94d => {
            let p = w.pred(87, 90);
            return (p.idx == 7 && !p.not).then(|| vec![Op::Exit { cond: 15 }]);
        }
        0x945 => return one(Op::Ssy { t: rel(addr, w) }),
        0x941 => return one(Op::Sync { cond: 15 }),
        0x95b => return one(Op::Kil { cond: 15 }),
        0xb1d => return one(Op::Bar),
        0x992 | 0x98f | 0x9af => return one(Op::Fence),
        0x919 => return one(Op::S2R { d: w.reg(16), sys: w.f(72, 8) as u8 }),
        0x9c3 => return one(Op::S2R { d: w.ureg(16), sys: w.f(72, 8) as u8 }),
        0x805 => {
            // CS2R: a system register pair; SRZ (255) zeroes two registers.
            let d = w.reg(16);
            let sys = w.f(72, 8) as u8;
            let wide = w.b(80);
            let read = |d: Reg, s: u8| if s == 255 { Op::Mov { d, b: Src::Reg(RZ), lanes: 15 } } else { Op::S2R { d, sys: s } };
            return Some(if wide && d != RZ {
                vec![read(d, sys), read(d + 1, if sys == 255 { 255 } else { sys + 1 })]
            } else {
                vec![read(d, sys)]
            });
        }
        0x3c2 => return one(Op::Mov { d: w.ureg(16), b: Src::Reg(w.reg(24)), lanes: 15 }),
        0x882 => return one(Op::Mov { d: w.ureg(16), b: Src::Imm(w.f(32, 32)), lanes: 15 }),
        0xc82 => return one(Op::Mov { d: w.ureg(16), b: Src::Reg(w.ureg(32)), lanes: 15 }),
        0xab9 => {
            // ULDC: constant buffer words into uniform registers.
            let ty = mem_ty(w.f(73, 3))?;
            return one(Op::Ldc { d: w.ureg(16), idx: w.f(54, 5) as u8, a: RZ, off: w.f(38, 16) as i32, ty, mode: 0 });
        }
        0xb82 => {
            let ty = mem_ty(w.f(73, 3))?;
            return one(Op::Ldc { d: w.reg(16), idx: w.f(54, 5) as u8, a: w.reg(24), off: w.sf(38, 16), ty, mode: w.f(78, 2) as u8 });
        }
        0x981 | 0x986 | 0x983 | 0x987 | 0x984 | 0x988 => {
            let ty = mem_ty(w.f(73, 3))?;
            let a = w.reg(24);
            let off = w.sf(40, 24);
            let uniform = if matches!(opcode, 0x981 | 0x983 | 0x984) { w.ureg(32) } else { w.ureg(64) };
            if w.b(91) && uniform != RZ {
                return None;
            }
            let data = w.reg(32);
            let d = w.reg(16);
            let wide = w.b(90);
            return one(match opcode {
                0x981 => Op::Ld { d, a, off, ty, wide },
                0x986 => Op::St { r: data, a, off, ty, wide },
                0x983 => Op::Ldl { d, a, off, ty },
                0x987 => Op::Stl { r: data, a, off, ty },
                0x984 => Op::Lds { d, a, off, ty },
                _ => Op::Sts { r: data, a, off, ty },
            });
        }
        0x321 => {
            return one(Op::Ald {
                d: w.reg(16),
                a: w.reg(24),
                v: w.reg(32),
                addr: w.f(40, 10) as u16,
                n: w.f(74, 2) as u8 + 1,
                patch: w.b(76),
                output: w.b(79),
            })
        }
        0x322 => {
            return one(Op::Ast {
                r: w.reg(32),
                a: w.reg(24),
                v: w.reg(64),
                addr: w.f(40, 10) as u16,
                n: w.f(74, 2) as u8 + 1,
                patch: w.b(76),
            })
        }
        0x920 => return one(Op::Al2p { d: w.reg(16), a: w.reg(24), addr: w.sf(40, 11) as u16, n: w.f(74, 2) as u8 + 1, output: w.b(79) }),
        0x326 => {
            let loc = [IpaLoc::Default, IpaLoc::Centroid, IpaLoc::Offset][*[0, 1, 2].get(w.f(76, 2) as usize)?];
            let mode = [IpaMode::Pass, IpaMode::Constant, IpaMode::State][*[0, 1, 2].get(w.f(78, 2) as usize)?];
            return one(Op::Ipa { d: w.reg(16), addr: (w.f(64, 8) << 2) as u16, idx: None, mul: RZ, offset: w.reg(32), mode, loc, sat: false });
        }
        0x925 => {
            let what = match w.f(78, 3) {
                0 => Pixld::Count,
                1 => Pixld::CovMask,
                2 => Pixld::CentroidOffset,
                3 => Pixld::SampleId,
                4 => Pixld::Covered,
                n => Pixld::Other(n as u8),
            };
            return one(Op::Pixld { d: w.reg(16), a: w.reg(24), what });
        }
        0x806 | 0x886 => {
            let op = [VoteOp::All, VoteOp::Any, VoteOp::Eq][*[0, 1, 2].get(w.f(72, 2) as usize)?];
            let d = if opcode == 0x886 { w.ureg(16) } else { w.reg(16) };
            return one(Op::Vote { d, pd: w.pdst(81), p: w.pred(87, 90), op });
        }
        0x389 | 0x589 | 0x989 | 0xf89 => {
            let op = [ShflOp::Idx, ShflOp::Up, ShflOp::Down, ShflOp::Bfly][w.f(58, 2) as usize];
            let lane = if opcode & 0x800 != 0 { Src::Imm(w.f(53, 5)) } else { Src::Reg(w.reg(32)) };
            let c = if opcode & 0x200 != 0 && opcode != 0x989 || opcode == 0xf89 { Src::Imm(w.f(40, 13)) } else { Src::Reg(w.reg(64)) };
            return one(Op::Shfl { d: w.reg(16), pd: w.pdst(81), a: w.reg(24), b: lane, c, op });
        }
        0x81c => {
            // PLOP3 of three predicates; the second operation's table is in bits 16..24.
            let lut0 = (w.f(64, 3) | w.f(72, 5) << 3) as u8;
            if w.b(67) {
                return None;
            }
            return Some(plop3(w.pdst(81), w.pred(87, 90), w.pred(77, 80), w.pred(68, 71), lut0)?);
        }
        _ => {}
    }

    // Packed half floats: like the ALU forms, with a swizzle per source and the third
    // slot's modifiers in bits 83 and 84.
    if matches!(opcode & 0x1ff, 0x030..=0x035 | 0x037 | 0x040) {
        return half(w).map(|op| vec![op]);
    }
    match opcode {
        // R2P PR, R.Bn, mask: predicates from the bits of a register's byte n (bits 76..78).
        0x804 => {
            let (a, mask, byte) = (w.reg(24), w.f(32, 7), w.f(76, 2));
            return Some(
                (0..7)
                    .filter(|i| mask >> i & 1 != 0)
                    .map(|i| Op::Lop { d: RZ, a, b: Src::Imm(1 << (8 * byte + i)), not_a: false, not_b: false, op: 0, x: false, cc: false, test: 3, pd: i as u8 })
                    .collect(),
            );
        }
        0x822 => {
            return one(Op::FSwzAdd { d: w.reg(16), a: w.reg(24), b: w.reg(64), ops: w.f(32, 8) as u8, ftz: w.b(80), cc: false, rnd: w.rnd(78) });
        }
        0xb60 | 0x361 | 0xb66 | 0x367 | 0xb63 | 0x364 | 0xb69 | 0x36a | 0xb6c | 0x36d | 0xb6f | 0x370 => {
            return texture(w).map(|t| vec![Op::Tex(t)]);
        }
        _ => {}
    }

    // ALU instructions: the low 9 bits name the operation, bits 9..12 the operand form.
    let op9 = opcode & 0x1ff;
    let uniform = op9 >= 0x80 && op9 < 0x100;
    let base = if uniform { op9 - 0x80 } else { op9 };
    let s = alu(w)?;
    let d = if uniform { w.ureg(16) } else { w.reg(16) };
    let ftz = w.b(80);
    let op = match (base, uniform) {
        // MOV
        (0x002, false) => Op::Mov { d, b: s.b, lanes: w.f(72, 4) as u8 },
        // FADD: the second operand is b, or c in the immediate/constant/uniform forms.
        (0x021, false) => {
            let (b, bm) = if matches!(s.form, 2 | 3 | 7) { (s.c, s.cm) } else { (s.b, s.bm) };
            Op::FAdd { d, a: s.a, am: s.am, b, bm, ftz, sat: w.b(77), cc: false, rnd: w.rnd(78) }
        }
        (0x020, false) => {
            if s.am.abs || s.bm.abs {
                return None;
            }
            let scale = match w.f(84, 3) {
                0 | 4 => 1.0,
                n => [1.0, 0.5, 0.25, 0.125, 1.0, 8.0, 4.0, 2.0][n as usize],
            };
            Op::FMul { d, a: s.a, b: s.b, neg: s.am.neg ^ s.bm.neg, fmz: Fmz { ftz, dnz: w.b(76) }, sat: w.b(77), cc: false, rnd: w.rnd(78), scale }
        }
        (0x023, false) => {
            if s.am.abs || s.bm.abs || s.cm.abs {
                return None;
            }
            Op::Ffma { d, a: s.a, b: s.b, c: s.c, neg_ab: s.am.neg ^ s.bm.neg, neg_c: s.cm.neg, fmz: Fmz { ftz, dnz: w.b(76) }, sat: w.b(77), cc: false, rnd: w.rnd(78) }
        }
        (0x009, false) => Op::FMnMx { d, a: s.a, am: s.am, b: s.b, bm: s.bm, ftz, cc: false, p: w.pred(87, 90) },
        (0x00a, false) => Op::FSet {
            d,
            cmp: FCmpArgs { a: s.a, am: s.am, b: s.b, bm: s.bm, cond: w.f(76, 4) as u8, ftz, bop: w.bop(74)?, p: w.pred(87, 90) },
            bf: true,
            cc: false,
        },
        (0x00b, false) => Op::FSetP {
            pd: w.pdst(81),
            pn: w.pdst(84),
            cmp: FCmpArgs { a: s.a, am: s.am, b: s.b, bm: s.bm, cond: w.f(76, 4) as u8, ftz, bop: w.bop(74)?, p: w.pred(87, 90) },
        },
        (0x108, false) => {
            let a = reg_of(s.b)?;
            let op = [Mufu::Cos, Mufu::Sin, Mufu::Ex2, Mufu::Lg2, Mufu::Rcp, Mufu::Rsq, Mufu::Rcp64h, Mufu::Rsq64h, Mufu::Sqrt]
                [*[0, 1, 2, 3, 4, 5, 6, 7, 8].get(w.f(74, 6) as usize)?];
            if w.b(73) {
                return None; // F16
            }
            // SM70 has no RRO: SIN and COS take turns, the compiler having divided by 2 pi
            if matches!(op, Mufu::Sin | Mufu::Cos) {
                let turn = std::f32::consts::TAU.to_bits();
                return Some(vec![
                    Op::FMul { d, a, b: Src::Imm(turn), neg: false, fmz: Fmz::default(), sat: false, cc: false, rnd: Rnd::N, scale: 1.0 },
                    Op::Mufu { d, a: d, m: s.bm, op, sat: false },
                ]);
            }
            Op::Mufu { d, a, m: s.bm, op, sat: false }
        }
        // IADD3 (.X adds the carry); the carry and overflow predicates become the carry flag.
        (0x010, _) => {
            let x = w.b(74);
            let carry_out = w.pdst(81) != 7;
            let (mut b, mut c, mut nb, mut nc) = (s.b, s.c, s.bm.neg, s.cm.neg);
            if reg_of(c).is_none() {
                std::mem::swap(&mut b, &mut c);
                std::mem::swap(&mut nb, &mut nc);
            }
            Op::IAdd3 { d, a: s.a, b, c: reg_of(c)?, neg: [s.am.neg, nb, nc], half: [0; 3], shift: 0, x, cc: carry_out }
        }
        (0x024, _) => {
            let signed = w.b(73);
            Op::IMad { d, a: s.a, b: s.b, c: s.c, sa: signed, sb: signed, hi: false, neg_ab: false, neg_c: s.cm.neg, sat: false, x: false, cc: false }
        }
        (0x027, _) => {
            let signed = w.b(73);
            Op::IMad { d, a: s.a, b: s.b, c: s.c, sa: signed, sb: signed, hi: true, neg_ab: false, neg_c: s.cm.neg, sat: false, x: false, cc: false }
        }
        (0x017, _) => Op::IMnMx { d, a: s.a, b: s.b, signed: w.b(73), part: 0, cc: false, p: w.pred(87, 90) },
        (0x00c, _) => {
            if w.b(72) {
                return None; // .EX: a 64-bit compare continued from a predicate
            }
            Op::ISetP {
                pd: w.pdst(81),
                pn: w.pdst(84),
                cmp: ICmpArgs { a: s.a, b: s.b, cond: w.f(76, 3) as u8, signed: w.b(73), x: false, bop: w.bop(74)?, p: w.pred(87, 90) },
            }
        }
        (0x012, _) => {
            let (mut b, mut c, mut lut) = (s.b, s.c, w.f(72, 8) as u8);
            if reg_of(c).is_none() {
                std::mem::swap(&mut b, &mut c);
                lut = lut_swap_bc(lut);
            }
            let pd = w.pdst(81);
            Op::Lop3 { d, a: s.a, b, c: reg_of(c)?, lut, x: false, cc: false, test: if pd == 7 { 0 } else { 3 }, pd }
        }
        (0x019, _) => {
            let ty = w.f(73, 2);
            Op::Shf { d, lo: s.a, hi: reg_of(s.c)?, b: s.b, right: w.b(76), high: w.b(80), wrap: w.b(75), wide: ty < 2, signed: ty == 0 || ty == 2, cc: false }
        }
        (0x007, _) => Op::Sel { d, a: s.a, b: s.b, p: w.pred(87, 90) },
        // FSEL: SEL of floats
        (0x008, false) => {
            if s.am != Mod::default() || s.bm != Mod::default() {
                return None;
            }
            Op::Sel { d, a: s.a, b: s.b, p: w.pred(87, 90) }
        }
        (0x016, _) => Op::Prmt { d, a: s.a, b: s.b, c: s.c, mode: w.f(72, 3) as u8 },
        (0x011, _) => {
            if w.b(80) || w.b(74) {
                return None; // LEA.HI and LEA.X
            }
            Op::IScAdd { d, a: s.a, b: s.b, shift: w.f(75, 5) as u8, neg_a: w.b(72), neg_b: false, cc: false }
        }
        (0x013, false) => Op::I2I { d, b: s.b, m: Mod { abs: true, neg: false }, from: IntTy { bytes: 4, signed: true }, to: IntTy { bytes: 4, signed: true }, sel: 0, sat: false, cc: false },
        (0x100, false) => Op::Flo { d, b: s.b, not: s.bm.neg, signed: w.b(73), shift: w.b(74), cc: false },
        (0x109, false) => Op::PopC { d, b: s.b, not: s.bm.neg },
        (0x104 | 0x110, false) => Op::F2F {
            d,
            b: s.b,
            m: s.bm,
            from: 1 << w.f(84, 2),
            to: 1 << w.f(75, 2),
            rnd: w.rnd(78),
            int: false,
            high: false,
            ftz,
            sat: false,
            cc: false,
        },
        (0x107 | 0x113, false) => Op::F2F {
            d,
            b: s.b,
            m: s.bm,
            from: 1 << w.f(84, 2),
            to: 1 << w.f(75, 2),
            rnd: w.rnd(78),
            int: true,
            high: false,
            ftz,
            sat: false,
            cc: false,
        },
        (0x105 | 0x111, false) => Op::F2I {
            d,
            b: s.b,
            m: s.bm,
            from: 1 << w.f(84, 2),
            to: IntTy { bytes: 1 << w.f(75, 2), signed: w.b(72) },
            rnd: w.rnd(78),
            high: false,
            ftz,
            cc: false,
        },
        // I2FP: the ALU pipe's integer to float, with I2F's fields
        (0x106 | 0x112 | 0x045, false) => Op::I2F {
            d,
            b: s.b,
            m: Mod::default(),
            from: IntTy { bytes: 1 << w.f(84, 2), signed: w.b(74) },
            to: 1 << w.f(75, 2),
            sel: 0,
            rnd: w.rnd(78),
            cc: false,
        },
        _ => return None,
    };
    one(op)
}

fn half(w: W) -> Option<Op> {
    let form = w.f(9, 3);
    let m = |abs, neg| Mod { abs: w.b(abs), neg: w.b(neg) };
    let cb = || Src::Cb(w.f(54, 5) as u8, w.f(38, 16));
    // (source, modifiers, swizzle) of the 32-bit slot and of the slot at bits 64..72
    let wide = |src: Src| (src, m(62, 63), w.f(60, 2) as u8);
    let low = || (Src::Reg(w.reg(64)), m(83, 84), w.f(81, 2) as u8);
    let imm = |src: Src| (src, Mod::default(), 0);
    let (b, c) = match form {
        1 => (wide(Src::Reg(w.reg(32))), low()),
        2 => (low(), imm(Src::Imm(w.f(32, 32)))),
        3 if !w.b(91) => (low(), wide(cb())),
        4 => (imm(Src::Imm(w.f(32, 32))), low()),
        5 if !w.b(91) => (wide(cb()), low()),
        6 => (wide(Src::Reg(w.ureg(32))), low()),
        7 => (low(), wide(Src::Reg(w.ureg(32)))),
        _ => return None,
    };
    let a = w.reg(24);
    let srcs = HalfSrcs { a, b: b.0, c: c.0, swz: [w.f(74, 2) as u8, b.2, c.2], m: [m(73, 72), b.1, c.1] };
    let (d, ftz, sat, f32) = (w.reg(16), w.b(80), w.b(77), w.b(78));
    let op9 = w.opcode() & 0x1ff;
    if w.b(85) {
        return None; // bfloat16
    }
    // HADD2, HMUL2 and HSETP2 take their second operand from b, or from c in forms 2, 3 and 7.
    let s2 = if matches!(form, 2 | 3 | 7) {
        HalfSrcs { b: srcs.c, swz: [srcs.swz[0], srcs.swz[2], 0], m: [srcs.m[0], srcs.m[2], Mod::default()], ..srcs }
    } else {
        srcs
    };
    Some(match op9 {
        0x030 | 0x032 | 0x037 => {
            let op = if op9 == 0x030 { HalfOp::Add } else { HalfOp::Mul };
            Op::Half { d, op, s: s2, f32, sat, ftz, p: PT }
        }
        0x031 | 0x035 => Op::Half { d, op: HalfOp::Fma, s: srcs, f32, sat, ftz, p: PT },
        0x040 => Op::Half { d, op: HalfOp::MnMx, s: srcs, f32: false, sat: false, ftz, p: w.pred(87, 90) },
        0x034 => Op::HSetP { pd: w.pdst(81), pn: w.pdst(84), s: s2, cond: w.f(76, 4) as u8, ftz, bop: w.bop(69)?, p: w.pred(87, 90), hand: w.b(71) },
        _ => return None,
    })
}

/// The texture instructions: sampling, fetches, gathers, queries and gradients. Their sources
/// are two register vectors (bits 24 and 32) as Mesa's NAK packs them; the bound forms name a
/// texture handle in a constant buffer, which goes in `index` as buffer << 14 | word.
fn texture(w: W) -> Option<Tex> {
    let opcode = w.opcode();
    let bindless = opcode & 0x800 == 0;
    let kind = match opcode & 0x7ff {
        0x360 | 0x361 => TexKind::Sample,
        0x366 | 0x367 => TexKind::Fetch,
        0x363 | 0x364 => TexKind::Gather,
        0x369 | 0x36a => TexKind::QueryLod,
        0x36c | 0x36d => TexKind::Grad,
        _ => TexKind::Query,
    };
    let (dim, array) = match w.f(61, 3) {
        0 => (TexDim::D1, false),
        1 => (TexDim::D2, false),
        2 => (TexDim::D3, false),
        3 => (TexDim::Cube, false),
        4 => (TexDim::D1, true),
        5 => (TexDim::D2, true),
        7 => (TexDim::Cube, true),
        _ => return None,
    };
    let index = if bindless { 0 } else { (w.f(54, 5) << 14 | w.f(40, 14)) as u16 };
    let mut t = Tex {
        kind,
        dim,
        array,
        mask: w.f(72, 4) as u8,
        d0: w.reg(16),
        d1: w.reg(64),
        scalar: true,
        handle: RZ,
        bindless,
        index,
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
        ndv: w.b(77),
        f16: w.b(79),
        live_only: false,
    };
    if kind == TexKind::Query {
        t.dim = TexDim::D2;
        t.array = false;
        t.ndv = false;
        t.f16 = false;
        t.comp = [1, 2, 5, 0][w.f(62, 2) as usize];
        if bindless {
            return None;
        }
        t.lod = w.reg(24);
        t.lod_mode = LodMode::Level;
        return Some(t);
    }
    let coords = match dim {
        TexDim::D1 => 1,
        TexDim::D2 => 2,
        TexDim::D3 | TexDim::Cube => 3,
    };
    // The logical sources in order, then where they live.
    let mut first: Vec<u8> = Vec::new(); // what goes in the first vector
    let mut second: Vec<u8> = Vec::new();
    const LAYER: u8 = 10;
    const HANDLE: u8 = 11;
    const LOD: u8 = 12;
    const OFFSET: u8 = 13;
    const OFFSET2: u8 = 14;
    const DREF: u8 = 15;
    const SAMPLE: u8 = 16;
    if kind == TexKind::Grad {
        if bindless {
            first.push(HANDLE);
        }
        first.extend(0..coords as u8);
        if array || w.b(76) {
            first.push(LAYER);
            if w.b(76) {
                t.offset_kind = OffsetKind::NibblesHigh;
            }
        }
        t.ndv = true;
    } else {
        if array {
            first.push(LAYER);
        }
        first.extend(0..coords as u8);
        if bindless {
            second.push(HANDLE);
        }
        match kind {
            TexKind::Sample => {
                t.lod_mode = match w.f(87, 3) {
                    0 | 4 => LodMode::Auto,
                    1 => LodMode::Zero,
                    2 | 5 => LodMode::Bias,
                    3 => LodMode::Level,
                    _ => return None,
                };
                if matches!(t.lod_mode, LodMode::Bias | LodMode::Level) {
                    second.push(LOD);
                }
                if w.b(76) {
                    second.push(OFFSET);
                    t.offset_kind = OffsetKind::Nibbles;
                }
                t.shadow = w.b(78);
            }
            TexKind::Fetch => {
                t.ndv = false;
                t.lod_mode = match w.f(87, 3) {
                    1 => LodMode::Zero,
                    3 => LodMode::Level,
                    _ => return None,
                };
                if t.lod_mode == LodMode::Level {
                    second.push(LOD);
                }
                if w.b(76) {
                    second.push(OFFSET);
                    t.offset_kind = OffsetKind::Nibbles;
                }
                t.ms = w.b(78);
            }
            TexKind::Gather => {
                t.lod_mode = LodMode::Zero;
                t.comp = w.f(87, 2) as u8;
                match w.f(76, 2) {
                    1 => {
                        second.push(OFFSET);
                        t.offset_kind = OffsetKind::Bytes;
                    }
                    2 => {
                        second.extend([OFFSET, OFFSET2]);
                        t.offset_kind = OffsetKind::PerTexel;
                    }
                    _ => {}
                }
                t.shadow = w.b(78);
            }
            _ => {}
        }
        if t.shadow {
            second.push(DREF);
        }
        if t.ms {
            second.push(SAMPLE);
        }
    }
    // Short lists go across both vectors (bit 60).
    let rebalanced = w.b(60) && second.is_empty() && kind != TexKind::Grad;
    let place = |list: &[u8], a: Reg, t: &mut Tex| {
        for (n, &what) in list.iter().enumerate() {
            let r = crate::decode::reg_at(a, n as u32);
            match what {
                0..=2 => t.coord[what as usize] = r,
                LAYER => {
                    if array {
                        t.layer = r
                    } else {
                        t.offset = r
                    }
                }
                HANDLE => t.handle = r,
                LOD => t.lod = r,
                OFFSET => t.offset = r,
                OFFSET2 => {}
                DREF => t.dref = r,
                _ => t.sample = r,
            }
        }
    };
    let (ra, rb) = (w.reg(24), w.reg(32));
    if rebalanced {
        let split = match first.len() {
            4 | 3 => 2,
            _ => 1,
        };
        place(&first[..split], ra, &mut t);
        place(&first[split..], rb, &mut t);
    } else {
        place(&first, ra, &mut t);
        place(&second, rb, &mut t);
    }
    if kind == TexKind::Grad {
        t.deriv = rb;
    }
    Some(t)
}

/// PLOP3 into one or two predicates, from three. The internal form has PSETP (two operations on
/// three predicates), so only tables that are such a combination decode: the common AND, OR,
/// XOR and copies.
fn plop3(pd: u8, a: Pred, b: Pred, c: Pred, lut: u8) -> Option<Vec<Op>> {
    let table = |f: &dyn Fn(bool, bool, bool) -> bool| -> u8 {
        (0..8).fold(0, |t, i| t | (f(i & 4 != 0, i & 2 != 0, i & 1 != 0) as u8) << i)
    };
    for op1 in [BoolOp::And, BoolOp::Or, BoolOp::Xor] {
        for op2 in [BoolOp::And, BoolOp::Or, BoolOp::Xor] {
            for (na, nb, nc) in (0..8).map(|k| (k & 4 != 0, k & 2 != 0, k & 1 != 0)) {
                let apply = |o: BoolOp, x: bool, y: bool| match o {
                    BoolOp::And => x & y,
                    BoolOp::Or => x | y,
                    BoolOp::Xor => x ^ y,
                };
                let t = table(&|x, y, z| apply(op2, apply(op1, x ^ na, y ^ nb), z ^ nc));
                if t == lut {
                    let flip = |p: Pred, n: bool| Pred { idx: p.idx, not: p.not ^ n };
                    return Some(vec![Op::PSetP { pd, pn: 7, a: flip(a, na), b: flip(b, nb), c: flip(c, nc), op1, op2 }]);
                }
            }
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::program::Program;

    /// NVIDIA's GLSL compiler's fragment shader for `gl_FragColor = shade` (SM87), as
    /// nvdisasm reads it.
    const FRAGMENT: &[(u64, u64)] = &[
        (0x00000002ff007c0c, 0x040fda000bf05070), // ISETP.NE.U32.AND P0, PT, RZ, UR2, PT
        (0x0000012000000947, 0x000fea0003800000), // @P0 BRA 0x140
        (0xffffffff00007956, 0x000fe80000000000), // BMOV.32 B0, 0xffffffff
        (0x0000000000017f55, 0x000fe80000100000), // BMOV.32.CLEAR B1, B0
        (0x0000000001027f55, 0x000fe80000100000),
        (0x0000000002037f55, 0x000fe80000100000),
        (0x0000000003047f55, 0x000fe80000100000),
        (0x0000000004057f55, 0x000fe80000100000),
        (0x0000000005067f55, 0x000fe80000100000),
        (0x0000000006077f55, 0x000fe80000100000),
        (0x0000000007087f55, 0x000fe80000100000),
        (0x0000000008097f55, 0x000fe80000100000),
        (0x00000000090a7f55, 0x000fe80000100000),
        (0x000000000a0b7f55, 0x000fe80000100000),
        (0x000000000b0c7f55, 0x000fe80000100000),
        (0x000000000c0d7f55, 0x000fe80000100000),
        (0x000000000d0e7f55, 0x000fe80000100000),
        (0x000000000e0f7f55, 0x000fe80000100000),
        (0x000000000f007956, 0x000fe80000000000), // BMOV.32 B15, 0x0
        (0x0000000100027882, 0x000fe40000000000), // UMOV UR2, 0x1
        (0x000000ff00047326, 0x000e2200000e001f), // IPA.PASS R4, a[0x7c]
        (0x000000ff00017326, 0x000e6200000e0020), // IPA.PASS R1, a[0x80]
        (0x000000ff00037326, 0x000ea200000e0021),
        (0x000000ff00057326, 0x000ee200000e0022),
        (0x000000ff00077326, 0x000f2200000e0023),
        (0x0000000400047308, 0x001e640000001000), // MUFU.RCP R4, R4
        (0x0000000104007220, 0x042fe2000041c000), // FMUL.FTZ.RZ R0, R4, R1
        (0x0000000304017220, 0x044fe2000041c000),
        (0x0000000504027220, 0x048fe2000041c000),
        (0x0000000704037220, 0x010fe2000041c000),
        (0x000000000000794d, 0x000fec0003800000), // EXIT
        (0xfffffff000007947, 0x000fc0000383ffff), // BRA 0x1f0
    ];

    fn program(words: &[(u64, u64)]) -> Program {
        Program::decode_sm70_at(0, &mut |a| {
            let (lo, hi) = *words.get(a as usize / 16)?;
            Some(if a % 16 == 0 { lo } else { hi })
        })
        .unwrap()
    }

    #[test]
    fn the_stock_compilers_fragment_shader_decodes() {
        let p = program(FRAGMENT);
        let ops: Vec<Op> = p.insts().iter().map(|i| i.op).collect();
        assert!(ops.iter().all(|op| !matches!(op, Op::Invalid(_))), "{ops:?}");
        let ur2 = UNIFORM_BASE + 2;
        assert_eq!(
            p.insts()[0],
            Inst {
                pred: PT,
                op: Op::ISetP {
                    pd: 0,
                    pn: 7,
                    cmp: ICmpArgs { a: RZ, b: Src::Reg(ur2), cond: 5, signed: false, x: false, bop: BoolOp::And, p: PT },
                },
            }
        );
        assert_eq!(p.insts()[1].pred, Pred { idx: 0, not: false });
        assert!(matches!(p.insts()[1].op, Op::Bra { t: 20, cond: 15, all: false }));
        assert_eq!(p.insts()[19].op, Op::Mov { d: ur2, b: Src::Imm(1), lanes: 15 });
        assert!(matches!(
            p.insts()[20].op,
            Op::Ipa { d: 4, addr: 0x7c, idx: None, mode: IpaMode::Pass, loc: IpaLoc::Default, .. }
        ));
        assert!(matches!(p.insts()[25].op, Op::Mufu { d: 4, a: 4, op: Mufu::Rcp, .. }));
        assert!(matches!(
            p.insts()[26].op,
            Op::FMul { d: 0, a: 4, b: Src::Reg(1), neg: false, rnd: Rnd::Z, fmz: Fmz { ftz: true, dnz: false }, .. }
        ));
        assert!(matches!(p.insts()[30].op, Op::Exit { cond: 15 }));
    }

    #[test]
    fn lop3_tables_follow_swapped_operands() {
        // a & b swapped to a & c, and so on.
        assert_eq!(lut_swap_bc(0xf0 & 0xcc), 0xf0 & 0xaa);
        assert_eq!(lut_swap_bc(0xcc ^ 0xaa), 0xcc ^ 0xaa);
        assert_eq!(lut_swap_bc(0xaa), 0xcc);
    }
}
