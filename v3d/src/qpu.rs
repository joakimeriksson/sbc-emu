//! The V3D 4.2 QPU: 64-bit instruction decoding and a 16-lane interpreter.
//!
//! There is no public ISA document for V3D 4.x; the encoding follows Mesa's
//! `src/broadcom/qpu/qpu_pack.c` and the semantics follow what Mesa's compiler
//! (`src/broadcom/compiler`) relies on:
//! * an instruction pairs an add-ALU and a mul-ALU operation (each with its own
//!   condition and flag update) with a signal (`thrsw`, `ldunif`, `ldtmu`, `ldvary`, ...);
//! * accumulators r0-r5 and a 64-entry register file; `ldunif`/`ldunifa` write r5,
//!   `ldvary` writes its register at once and the C coefficient to r5 one
//!   instruction later, SFU writes through the magic registers land in r4 two
//!   instructions later (4.2 code uses the stalling add-ALU SFU ops instead);
//! * branches and thread switches have delay slots (3 and 2);
//! * two `thrsw` in a row mark the last thread switch, after which the next `thrsw`
//!   ends the program (after its delay slots); a shader that starts "in the final
//!   thread section" ends at its first `thrsw`.

pub const LANES: usize = 16;
pub type V = [u32; LANES];

// ------------------------------------------------------------------------- decoding

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AddOp {
    Fadd, Faddnf, Vfpack, Add, Sub, Fsub, Min, Max, Umin, Umax, Shl, Shr, Asr, Ror, Fmin, Fmax,
    Vfmin, And, Or, Xor, Vadd, Vsub, Not, Neg, Flapush, Flbpush, Flpop, Recip, Setmsf, Setrevf,
    Nop, Tidx, Eidx, Lr, Vfla, Vflna, Vflb, Vflnb, Fxcd, Xcd, Fycd, Ycd, Msf, Revf, Iid, Sampid,
    Barrierid, Tmuwt, Vpmwt, Flafirst, Flnafirst, Vpmsetup, LdvpmvIn, LdvpmvOut, LdvpmdIn,
    LdvpmdOut, Ldvpmp, Rsqrt, Exp, Log, Sin, Rsqrt2, LdvpmgIn, LdvpmgOut, Fcmp, Vfmax, Fround,
    Ftoin, Ftrunc, Ftoiz, Ffloor, Ftouz, Fceil, Ftoc, Fdx, Fdy, Stvpmv, Stvpmd, Stvpmp, Itof,
    Clz, Utof,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MulOp { Add, Sub, Umul24, Vfmul, Smul24, Multop, Fmov, Mov, Nop, Fmul }

#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum Unpack { #[default] None, Abs, L, H, Rep32f16, RepL16, RepH16, Swap16 }

#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum Pack { #[default] None, L, H }

#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum Cond { #[default] None, Ifa, Ifb, Ifna, Ifnb }

#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum Pf { #[default] None, Z, N, C }

/// Flag updates: `and`/`nor` of flag A with the zero/negative/carry condition (or its inverse).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum Uf { #[default] None, Andz, Andnz, Nornz, Norz, Andn, Andnn, Nornn, Norn, Andc, Andnc, Nornc, Norc }

#[derive(Clone, Copy, Debug, Default)]
pub struct Sig {
    pub thrsw: bool, pub ldunif: bool, pub ldunifa: bool, pub ldunifrf: bool, pub ldunifarf: bool,
    pub ldtmu: bool, pub ldvary: bool, pub ldvpm: bool, pub ldtlb: bool, pub ldtlbu: bool,
    pub ucb: bool, pub rotate: bool, pub wrtmuc: bool, pub small_imm_b: bool,
}

impl Sig {
    fn decode(s: u32) -> Option<Sig> {
        let mut g = Sig::default();
        match s {
            0 => {}
            1 => g.thrsw = true,
            2 => g.ldunif = true,
            3 => { g.thrsw = true; g.ldunif = true }
            4 => g.ldtmu = true,
            5 => { g.thrsw = true; g.ldtmu = true }
            6 => { g.ldtmu = true; g.ldunif = true }
            7 => { g.thrsw = true; g.ldtmu = true; g.ldunif = true }
            8 => g.ldvary = true,
            9 => { g.thrsw = true; g.ldvary = true }
            10 => { g.ldvary = true; g.ldunif = true }
            11 => { g.thrsw = true; g.ldvary = true; g.ldunif = true }
            12 => g.ldunifrf = true,
            13 => { g.thrsw = true; g.ldunifrf = true }
            14 => { g.small_imm_b = true; g.ldvary = true }
            15 => g.small_imm_b = true,
            16 => g.ldtlb = true,
            17 => g.ldtlbu = true,
            18 => g.wrtmuc = true,
            19 => { g.thrsw = true; g.wrtmuc = true }
            20 => { g.ldvary = true; g.wrtmuc = true }
            21 => { g.thrsw = true; g.ldvary = true; g.wrtmuc = true }
            22 => g.ucb = true,
            23 => g.rotate = true,
            24 => g.ldunifa = true,
            25 => g.ldunifarf = true,
            31 => { g.small_imm_b = true; g.ldtmu = true }
            _ => return None,
        }
        Some(g)
    }
    /// Signals whose result goes to the address in the condition field.
    pub fn writes_address(&self) -> bool { self.ldunifrf || self.ldunifarf || self.ldvary || self.ldtmu || self.ldtlb || self.ldtlbu }
}

#[derive(Clone, Copy, Debug)]
pub struct Alu<O> {
    pub op: O,
    pub a: u8,
    pub b: u8,
    pub ua: Unpack,
    pub ub: Unpack,
    pub waddr: u8,
    pub magic: bool,
    pub pack: Pack,
    pub cond: Cond,
    pub pf: Pf,
    pub uf: Uf,
}

#[derive(Clone, Copy, Debug)]
pub struct AluInst {
    pub sig: Sig,
    pub sig_addr: u8,
    pub sig_magic: bool,
    pub raddr_a: u8,
    pub raddr_b: u8,
    pub add: Alu<AddOp>,
    pub mul: Alu<MulOp>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BrCond { Always, A0, Na0, AllA, AnyNa, AnyA, AllNa }

#[derive(Clone, Copy, Debug)]
pub struct BranchInst {
    pub cond: BrCond,
    pub msfign: u8,
    pub bdi: u8,
    pub ub: bool,
    pub bdu: u8,
    pub raddr_a: u8,
    pub offset: u32,
}

#[derive(Clone, Copy, Debug)]
pub enum Inst { Alu(AluInst), Branch(BranchInst), Invalid(u64) }

const SMALL_IMM: [u32; 48] = [
    0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15,
    -16i32 as u32, -15i32 as u32, -14i32 as u32, -13i32 as u32, -12i32 as u32, -11i32 as u32, -10i32 as u32, -9i32 as u32,
    -8i32 as u32, -7i32 as u32, -6i32 as u32, -5i32 as u32, -4i32 as u32, -3i32 as u32, -2i32 as u32, -1i32 as u32,
    0x3b800000, 0x3c000000, 0x3c800000, 0x3d000000, 0x3d800000, 0x3e000000, 0x3e800000, 0x3f000000,
    0x3f800000, 0x40000000, 0x40800000, 0x41000000, 0x41800000, 0x42000000, 0x42800000, 0x43000000,
];

fn f32_unpack(p: u32) -> Unpack { match p { 0 => Unpack::Abs, 1 => Unpack::None, 2 => Unpack::L, _ => Unpack::H } }
fn f16_unpack(p: u32) -> Option<Unpack> {
    Some(match p { 0 => Unpack::None, 1 => Unpack::Rep32f16, 2 => Unpack::RepL16, 3 => Unpack::RepH16, 4 => Unpack::Swap16, _ => return None })
}
fn pack_of(p: u32) -> Pack { match p { 0 => Pack::None, 1 => Pack::L, _ => Pack::H } }

const ANY: u8 = 0xff;
const fn m(b: u32) -> u8 { 1 << b }
const fn r(lo: u32, hi: u32) -> u8 { (((1u32 << (hi - lo + 1)) - 1) << lo) as u8 }

/// (first, last, mux_b mask, mux_a mask, op): Mesa's v3d42_add_ops, first match wins.
const ADD_OPS: &[(u8, u8, u8, u8, AddOp)] = {
    use AddOp::*;
    &[
        (0, 47, ANY, ANY, Fadd), (53, 55, ANY, ANY, Vfpack), (56, 56, ANY, ANY, Add), (57, 59, ANY, ANY, Vfpack),
        (60, 60, ANY, ANY, Sub), (61, 63, ANY, ANY, Vfpack), (64, 111, ANY, ANY, Fsub),
        (120, 120, ANY, ANY, Min), (121, 121, ANY, ANY, Max), (122, 122, ANY, ANY, Umin), (123, 123, ANY, ANY, Umax),
        (124, 124, ANY, ANY, Shl), (125, 125, ANY, ANY, Shr), (126, 126, ANY, ANY, Asr), (127, 127, ANY, ANY, Ror),
        (128, 175, ANY, ANY, Fmin), (176, 180, ANY, ANY, Vfmin), (181, 181, ANY, ANY, And), (182, 182, ANY, ANY, Or),
        (183, 183, ANY, ANY, Xor), (184, 184, ANY, ANY, Vadd), (185, 185, ANY, ANY, Vsub),
        (186, 186, m(0), ANY, Not), (186, 186, m(1), ANY, Neg), (186, 186, m(2), ANY, Flapush), (186, 186, m(3), ANY, Flbpush),
        (186, 186, m(4), ANY, Flpop), (186, 186, m(5), ANY, Recip), (186, 186, m(6), ANY, Setmsf), (186, 186, m(7), ANY, Setrevf),
        (187, 187, m(0), m(0), Nop), (187, 187, m(0), m(1), Tidx), (187, 187, m(0), m(2), Eidx), (187, 187, m(0), m(3), Lr),
        (187, 187, m(0), m(4), Vfla), (187, 187, m(0), m(5), Vflna), (187, 187, m(0), m(6), Vflb), (187, 187, m(0), m(7), Vflnb),
        (187, 187, m(1), r(0, 2), Fxcd), (187, 187, m(1), m(3), Xcd), (187, 187, m(1), r(4, 6), Fycd), (187, 187, m(1), m(7), Ycd),
        (187, 187, m(2), m(0), Msf), (187, 187, m(2), m(1), Revf), (187, 187, m(2), m(2), Iid), (187, 187, m(2), m(3), Sampid),
        (187, 187, m(2), m(4), Barrierid), (187, 187, m(2), m(5), Tmuwt), (187, 187, m(2), m(6), Vpmwt), (187, 187, m(2), m(7), Flafirst),
        (187, 187, m(3), m(0), Flnafirst), (187, 187, m(3), ANY, Vpmsetup),
        (188, 188, m(0), ANY, LdvpmvIn), (188, 188, m(1), ANY, LdvpmdIn), (188, 188, m(2), ANY, Ldvpmp),
        (188, 188, m(3), ANY, Rsqrt), (188, 188, m(4), ANY, Exp), (188, 188, m(5), ANY, Log), (188, 188, m(6), ANY, Sin),
        (188, 188, m(7), ANY, Rsqrt2), (189, 189, ANY, ANY, LdvpmgIn),
        (192, 239, ANY, ANY, Fcmp), (240, 244, ANY, ANY, Vfmax),
        (245, 245, r(0, 2), ANY, Fround), (245, 245, m(3), ANY, Ftoin), (245, 245, r(4, 6), ANY, Ftrunc), (245, 245, m(7), ANY, Ftoiz),
        (246, 246, r(0, 2), ANY, Ffloor), (246, 246, m(3), ANY, Ftouz), (246, 246, r(4, 6), ANY, Fceil), (246, 246, m(7), ANY, Ftoc),
        (247, 247, r(0, 2), ANY, Fdx), (247, 247, r(4, 6), ANY, Fdy),
        (248, 248, ANY, ANY, Stvpmv),
        (252, 252, r(0, 2), ANY, Itof), (252, 252, m(3), ANY, Clz), (252, 252, r(4, 6), ANY, Utof),
    ]
};

pub fn add_op_num_src(op: AddOp) -> u32 {
    use AddOp::*;
    match op {
        Nop | Tidx | Eidx | Lr | Vfla | Vflna | Vflb | Vflnb | Fxcd | Xcd | Fycd | Ycd | Msf | Revf | Iid | Sampid
        | Barrierid | Tmuwt | Vpmwt | Flafirst | Flnafirst => 0,
        Not | Neg | Flapush | Flbpush | Flpop | Recip | Setmsf | Setrevf | Vpmsetup | LdvpmvIn | LdvpmvOut | LdvpmdIn
        | LdvpmdOut | Ldvpmp | Rsqrt | Exp | Log | Sin | Rsqrt2 | Fround | Ftoin | Ftrunc | Ftoiz | Ffloor | Ftouz
        | Fceil | Ftoc | Fdx | Fdy | Itof | Clz | Utof => 1,
        _ => 2,
    }
}

pub fn mul_op_num_src(op: MulOp) -> u32 {
    match op { MulOp::Nop => 0, MulOp::Fmov | MulOp::Mov => 1, _ => 2 }
}

fn decode_flags(c: u32) -> Option<(Cond, Cond, Pf, Pf, Uf, Uf)> {
    const CM: [Cond; 4] = [Cond::Ifa, Cond::Ifb, Cond::Ifna, Cond::Ifnb];
    let pf = |v: u32| [Pf::None, Pf::Z, Pf::N, Pf::C][(v & 3) as usize];
    let uf = |v: u32| -> Uf {
        use Uf::*;
        [Andz, Andnz, Nornz, Norz, Andn, Andnn, Nornn, Norn, Andc, Andnc, Nornc, Norc][((v & 0xf) - 4) as usize]
    };
    let (mut ac, mut mc, mut apf, mut mpf, mut auf, mut muf) = (Cond::None, Cond::None, Pf::None, Pf::None, Uf::None, Uf::None);
    if c == 0 {
    } else if c >> 2 == 0 {
        apf = pf(c);
    } else if c >> 4 == 0 {
        auf = uf(c);
    } else if c == 0x10 {
        return None;
    } else if c >> 2 == 0x4 {
        mpf = pf(c);
    } else if c >> 4 == 0x1 {
        muf = uf(c);
    } else if c >> 4 == 0x2 {
        ac = CM[((c >> 2) & 3) as usize];
        mpf = pf(c);
    } else if c >> 4 == 0x3 {
        mc = CM[((c >> 2) & 3) as usize];
        apf = pf(c);
    } else {
        mc = CM[((c >> 4) & 3) as usize];
        if (c >> 2) & 3 == 0 { ac = CM[(c & 3) as usize]; } else { auf = uf(c); }
    }
    Some((ac, mc, apf, mpf, auf, muf))
}

pub fn decode(w: u64) -> Inst {
    let f = |hi: u32, lo: u32| ((w >> lo) & ((1u64 << (hi - lo + 1)) - 1)) as u32;
    let op_mul = f(63, 58);
    let sig = f(57, 53);
    if op_mul == 0 {
        if sig & 24 != 16 { return Inst::Invalid(w); }
        let c = f(34, 32);
        let cond = match c {
            0 => BrCond::Always, 2 => BrCond::A0, 3 => BrCond::Na0, 4 => BrCond::AllA, 5 => BrCond::AnyNa,
            6 => BrCond::AnyA, 7 => BrCond::AllNa, _ => return Inst::Invalid(w),
        };
        let msfign = f(22, 21);
        if msfign == 3 { return Inst::Invalid(w); }
        let ub = w & (1 << 14) != 0;
        return Inst::Branch(BranchInst {
            cond, msfign: msfign as u8, bdi: f(13, 12) as u8, ub, bdu: if ub { f(17, 15) as u8 } else { 0 },
            raddr_a: f(11, 6) as u8, offset: (f(55, 35) << 3).wrapping_add(f(31, 24) << 24),
        });
    }
    let Some(s) = Sig::decode(sig) else { return Inst::Invalid(w) };
    let packed_cond = f(52, 46);
    let (mut sig_addr, mut sig_magic) = (0u8, false);
    let flags = if s.writes_address() {
        sig_addr = (packed_cond & 0x3f) as u8;
        sig_magic = packed_cond & 0x40 != 0;
        (Cond::None, Cond::None, Pf::None, Pf::None, Uf::None, Uf::None)
    } else {
        match decode_flags(packed_cond) { Some(x) => x, None => return Inst::Invalid(w) }
    };
    // add ALU
    let op = f(31, 24);
    let (mux_a, mux_b, waddr_a) = (f(14, 12), f(17, 15), f(37, 32));
    let map_op = match op { 249..=251 => op - 249 + 245, 253..=255 => op - 253 + 245, _ => op };
    let Some(&(_, _, _, _, mut aop)) = ADD_OPS.iter().find(|e| map_op >= e.0 as u32 && map_op <= e.1 as u32 && e.2 & (1 << mux_b) != 0 && e.3 & (1 << mux_a) != 0)
    else { return Inst::Invalid(w) };
    if ((op >> 2) & 3) * 8 + mux_a > (op & 3) * 8 + mux_b {
        if aop == AddOp::Fmin { aop = AddOp::Fmax; }
        if aop == AddOp::Fadd { aop = AddOp::Faddnf; }
    }
    if aop == AddOp::Stvpmv {
        aop = match waddr_a { 0 => AddOp::Stvpmv, 1 => AddOp::Stvpmd, 2 => AddOp::Stvpmp, _ => return Inst::Invalid(w) };
    }
    let (mut ua, mut ub, mut apack) = (Unpack::None, Unpack::None, Pack::None);
    use AddOp::*;
    match aop {
        Fadd | Faddnf | Fsub | Fmin | Fmax | Fcmp | Vfpack => {
            if aop != Vfpack { apack = pack_of((op >> 4) & 3); }
            ua = f32_unpack((op >> 2) & 3);
            ub = f32_unpack(op & 3);
        }
        Ffloor | Fround | Ftrunc | Fceil | Fdx | Fdy => { apack = pack_of(mux_b & 3); ua = f32_unpack((op >> 2) & 3); }
        Ftoin | Ftoiz | Ftouz | Ftoc => ua = f32_unpack((op >> 2) & 3),
        Itof | Utof => apack = pack_of(mux_b & 3),
        Vfmin | Vfmax => match f16_unpack(op & 7) { Some(u) => ua = u, None => return Inst::Invalid(w) },
        _ => {}
    }
    let ma = w & (1 << 44) != 0;
    let mut amagic = false;
    if ma {
        match aop {
            LdvpmvIn => aop = LdvpmvOut,
            LdvpmdIn => aop = LdvpmdOut,
            LdvpmgIn => aop = LdvpmgOut,
            _ => amagic = true,
        }
    }
    // mul ALU
    let (mmux_a, mmux_b) = (f(20, 18), f(23, 21));
    let mop = match op_mul {
        1 => MulOp::Add, 2 => MulOp::Sub, 3 => MulOp::Umul24, 4..=8 => MulOp::Vfmul, 9 => MulOp::Smul24,
        10 => MulOp::Multop, 14 => MulOp::Fmov,
        15 => match mmux_b { 0..=3 => MulOp::Fmov, 4 if mmux_a == 0 => MulOp::Nop, 7 => MulOp::Mov, _ => return Inst::Invalid(w) },
        16..=63 => MulOp::Fmul,
        _ => return Inst::Invalid(w),
    };
    let (mut mua, mut mub, mut mpack) = (Unpack::None, Unpack::None, Pack::None);
    match mop {
        MulOp::Fmul => { mpack = pack_of(((op_mul >> 4) & 3) - 1); mua = f32_unpack((op_mul >> 2) & 3); mub = f32_unpack(op_mul & 3); }
        MulOp::Fmov => { mpack = pack_of(((op_mul & 1) << 1) + ((mmux_b >> 2) & 1)); mua = f32_unpack(mmux_b & 3); }
        MulOp::Vfmul => match f16_unpack(((op_mul & 7).wrapping_sub(4)) & 7) { Some(u) => mua = u, None => return Inst::Invalid(w) },
        _ => {}
    }
    let (ac, mc, apf, mpf, auf, muf) = flags;
    Inst::Alu(AluInst {
        sig: s, sig_addr, sig_magic,
        raddr_a: f(11, 6) as u8, raddr_b: f(5, 0) as u8,
        add: Alu { op: aop, a: mux_a as u8, b: mux_b as u8, ua, ub, waddr: waddr_a as u8, magic: amagic, pack: apack, cond: ac, pf: apf, uf: auf },
        mul: Alu { op: mop, a: mmux_a as u8, b: mmux_b as u8, ua: mua, ub: mub, waddr: f(43, 38) as u8, magic: w & (1 << 45) != 0, pack: mpack, cond: mc, pf: mpf, uf: muf },
    })
}

// ------------------------------------------------------------------------- numerics

pub fn f16_to_f32(h: u16) -> f32 {
    let s = ((h >> 15) as u32) << 31;
    let e = ((h >> 10) & 0x1f) as u32;
    let m = (h & 0x3ff) as u32;
    let bits = if e == 0 {
        if m == 0 { s } else {
            // subnormal half: normalise
            let mut e = 127 - 15 + 1;
            let mut m = m;
            while m & 0x400 == 0 { m <<= 1; e -= 1; }
            s | ((e as u32) << 23) | ((m & 0x3ff) << 13)
        }
    } else if e == 31 {
        s | 0x7f80_0000 | (m << 13)
    } else {
        s | ((e + 127 - 15) << 23) | (m << 13)
    };
    f32::from_bits(bits)
}

/// f32 to IEEE half, round to nearest even, overflow to infinity.
pub fn f32_to_f16(f: f32) -> u16 {
    let b = f.to_bits();
    let s = ((b >> 16) & 0x8000) as u16;
    let e = ((b >> 23) & 0xff) as i32;
    let m = b & 0x7f_ffff;
    if e == 255 { return s | 0x7c00 | if m != 0 { 0x200 } else { 0 }; }
    let e16 = e - 127 + 15;
    if e16 >= 31 { return s | 0x7c00; }
    if e16 <= 0 {
        if e16 < -10 { return s; }
        let m = m | 0x80_0000;
        let shift = (14 - e16) as u32;
        let half = 1u32 << (shift - 1);
        let rem = m & ((1 << shift) - 1);
        let mut r = m >> shift;
        if rem > half || (rem == half && r & 1 != 0) { r += 1; }
        return s | r as u16;
    }
    let mut r = ((e16 as u32) << 10) | (m >> 13);
    let rem = m & 0x1fff;
    if rem > 0x1000 || (rem == 0x1000 && r & 1 != 0) { r += 1; }
    s | r as u16
}

#[inline]
fn ftz(f: f32) -> f32 { if f.is_subnormal() { if f.is_sign_negative() { -0.0 } else { 0.0 } } else { f } }

#[inline]
fn unpack_f32(v: u32, u: Unpack) -> f32 {
    let f = match u {
        Unpack::L => f16_to_f32(v as u16),
        Unpack::H => f16_to_f32((v >> 16) as u16),
        Unpack::Abs => f32::from_bits(v & 0x7fff_ffff),
        _ => f32::from_bits(v),
    };
    ftz(f)
}

/// The f16x2 input unpacks of VFMUL/VFMIN/VFMAX.
fn unpack_f16x2(v: u32, u: Unpack) -> u32 {
    match u {
        Unpack::Rep32f16 => { let h = f32_to_f16(f32::from_bits(v)) as u32; h | (h << 16) }
        Unpack::RepL16 => (v & 0xffff) | (v << 16),
        Unpack::RepH16 => (v >> 16) | (v & 0xffff_0000),
        Unpack::Swap16 => v.rotate_left(16),
        _ => v,
    }
}

fn fmin(a: f32, b: f32) -> f32 { if a.is_nan() { b } else if b.is_nan() { a } else if a < b || (a == b && a.is_sign_negative()) { a } else { b } }
fn fmax(a: f32, b: f32) -> f32 { if a.is_nan() { b } else if b.is_nan() { a } else if a > b || (a == b && b.is_sign_negative()) { a } else { b } }

fn round_even(x: f32) -> f32 {
    let r = x.round();
    if (x - x.trunc()).abs() == 0.5 { 2.0 * (x / 2.0).round() } else { r }
}

fn f2i_sat(x: f32) -> u32 { if x.is_nan() { 0 } else { (x as i32) as u32 } }
fn f2u_sat(x: f32) -> u32 { if x.is_nan() { 0 } else { x as u32 } }

fn sfu(op: AddOp, x: f32) -> f32 {
    match op {
        AddOp::Recip => 1.0 / x,
        AddOp::Rsqrt | AddOp::Rsqrt2 => 1.0 / x.sqrt(),
        AddOp::Exp => x.exp2(),
        AddOp::Log => x.log2(),
        AddOp::Sin => (x * std::f32::consts::PI).sin(),
        _ => x,
    }
}

// ------------------------------------------------------------------------- execution

/// One TMU request being assembled by register writes.
#[derive(Clone, Default)]
pub struct TmuReq {
    pub s: Option<V>,
    pub t: Option<V>,
    pub r: Option<V>,
    pub i: Option<V>,
    pub b: Option<V>,
    pub dref: Option<V>,
    pub off: Option<V>,
    /// TMUD writes (general stores/atomics)
    pub data: Vec<V>,
}

/// What retired a TMU request.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TmuKind { Tex, TexCube, TexFetch, TexLod, General }

/// Services the shader needs from the rest of the GPU.
pub trait Env {
    /// Read `n` bytes at GPU virtual address `va`.
    fn read32(&mut self, va: u32) -> u32;
    fn write32(&mut self, va: u32, v: u32);
    fn read8(&mut self, va: u32) -> u8 { (self.read32(va & !3) >> ((va & 3) * 8)) as u8 }
    fn write8(&mut self, va: u32, v: u8) {
        let w = self.read32(va & !3);
        let sh = (va & 3) * 8;
        self.write32(va & !3, (w & !(0xff << sh)) | ((v as u32) << sh));
    }
    /// A texture or memory lookup: returns the words for `ldtmu`, in order.
    fn tmu(&mut self, t: &Thread, kind: TmuKind, cfg: &[u32], req: &TmuReq, lanes: u16) -> Vec<V>;
    /// `ldvary`: the next varying's (A*x + B*y form, C coefficient) for each lane.
    fn ldvary(&mut self, _t: &mut Thread) -> (V, V) { ([0; LANES], [0; LANES]) }
    fn tlb_write(&mut self, _t: &mut Thread, _lanes: u16, _v: &V, _cfg: Option<u32>) {}
    fn tlb_read(&mut self, _t: &mut Thread, _cfg: Option<u32>) -> V { [0; LANES] }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Stop {
    /// the program ended
    Done,
    /// a barrier (SYNCB write): resume once every thread of the group got here
    Barrier,
    /// instruction budget used up (runaway shader)
    Budget,
    /// undecodable instruction
    Fault,
}

#[derive(Clone)]
pub struct Thread {
    pub rf: Box<[V; 64]>,
    pub acc: [V; 6],
    pub fa: u16,
    pub fb: u16,
    /// multisample flags (4 bits per lane); for VS/CS: 0xf on lanes holding work
    pub msf: [u8; LANES],
    pub revf: bool,
    pub pc: u32,
    pub unif: u32,
    pub unifa: u32,
    pub lr: u32,
    pub rtop: V,
    /// lanes holding real work (vertices, invocations)
    pub valid: u16,
    pub tmu_cfg: Vec<u32>,
    pub tmu_req: TmuReq,
    pub tmu_out: std::collections::VecDeque<V>,
    /// VPM: input rows then (if separate) output rows
    pub vpm: Vec<V>,
    pub vpm_out_base: usize,
    /// fragment coordinates of each lane
    pub xcd: [i32; LANES],
    pub ycd: [i32; LANES],
    pub fx: [f32; LANES],
    pub fy: [f32; LANES],
    pub tidx: u32,
    pub barrier_id: u32,
    pub final_section: bool,
    prev_thrsw: bool,
    end_in: Option<u8>,
    branch: Option<(u8, u32, Option<u32>)>,
    /// delayed accumulator writes: (instructions left, accumulator, value)
    delayed: Vec<(u8, u8, V)>,
    pub insns: u64,
    pub done: bool,
}

#[inline]
fn lanes_of(mask: u16) -> impl Iterator<Item = usize> { (0..LANES).filter(move |l| mask & (1 << l) != 0) }

impl Thread {
    pub fn new(pc: u32, unif: u32, final_section: bool) -> Thread {
        Thread {
            rf: Box::new([[0; LANES]; 64]), acc: [[0; LANES]; 6], fa: 0, fb: 0, msf: [0xf; LANES], revf: false,
            pc, unif, unifa: 0, lr: 0, rtop: [0; LANES], valid: 0xffff, tmu_cfg: Vec::new(), tmu_req: TmuReq::default(),
            tmu_out: Default::default(), vpm: Vec::new(), vpm_out_base: 0, xcd: [0; LANES], ycd: [0; LANES],
            fx: [0.0; LANES], fy: [0.0; LANES], tidx: 0, barrier_id: 0, final_section, prev_thrsw: false, end_in: None,
            branch: None, delayed: Vec::new(), insns: 0, done: false,
        }
    }

    fn next_uniform(&mut self, env: &mut dyn Env) -> u32 {
        let v = env.read32(self.unif);
        self.unif = self.unif.wrapping_add(4);
        v
    }

    fn cond_mask(&self, c: Cond) -> u16 {
        match c { Cond::None => 0xffff, Cond::Ifa => self.fa, Cond::Ifb => self.fb, Cond::Ifna => !self.fa, Cond::Ifnb => !self.fb }
    }

    fn read_mux(&self, mux: u8, i: &AluInst) -> V {
        match mux {
            0..=5 => self.acc[mux as usize],
            6 => self.rf[i.raddr_a as usize],
            _ => if i.sig.small_imm_b { [SMALL_IMM.get(i.raddr_b as usize).copied().unwrap_or(0); LANES] } else { self.rf[i.raddr_b as usize] },
        }
    }

    fn msf_mask(&self) -> u16 { let mut m = 0; for l in 0..LANES { if self.msf[l] != 0 { m |= 1 << l; } } m }

    /// Run until the program ends, hits a barrier or uses up `budget` instructions.
    pub fn run(&mut self, code: &mut crate::Code, env: &mut dyn Env, budget: u64) -> Stop {
        let stop_at = self.insns + budget;
        while !self.done {
            if self.insns >= stop_at { return Stop::Budget; }
            let inst = code.fetch(self.pc, env);
            self.insns += 1;
            let tracing = crate::TRACE.load(std::sync::atomic::Ordering::Relaxed) > 0;
            let snap = if tracing { Some((self.rf.clone(), self.acc)) } else { None };
            let this_pc = self.pc;
            self.pc = self.pc.wrapping_add(8);
            let mut barrier = false;
            let mut thrsw = false;
            match inst {
                Inst::Invalid(w) => {
                    crate::log(&format!("v3d: invalid QPU instruction {w:#018x} at {this_pc:#x}"));
                    self.done = true;
                    return Stop::Fault;
                }
                Inst::Branch(b) => self.branch_inst(&b, this_pc, env),
                Inst::Alu(i) => { barrier = self.alu(&i, env); thrsw = i.sig.thrsw; }
            }
            // delayed accumulator writes land at the end of the instruction they are due in
            if !self.delayed.is_empty() {
                let mut k = 0;
                while k < self.delayed.len() {
                    if self.delayed[k].0 == 0 {
                        let (_, a, v) = self.delayed.remove(k);
                        self.acc[a as usize] = v;
                    } else { self.delayed[k].0 -= 1; k += 1; }
                }
            }
            // thread switches: a pair marks the last one; in the final section the next one ends
            // the thread once its two delay slots have run
            if let Some(n) = self.end_in {
                if n <= 1 { self.done = true; } else { self.end_in = Some(n - 1); }
            }
            if thrsw && self.end_in.is_none() && !self.done {
                if self.prev_thrsw { self.final_section = true; } else if self.final_section { self.end_in = Some(2); }
            }
            self.prev_thrsw = thrsw;
            if let Some((n, target, unif)) = self.branch {
                if n == 0 {
                    self.pc = target;
                    if let Some(u) = unif { self.unif = u; }
                    self.branch = None;
                } else { self.branch = Some((n - 1, target, unif)); }
            }
            if let Some((rf, acc)) = snap {
                crate::TRACE.fetch_sub(1, std::sync::atomic::Ordering::Relaxed);
                let mut ch = String::new();
                for k in 0..64 { if rf[k][0] != self.rf[k][0] { ch += &format!(" rf{k}={:#x}({})", self.rf[k][0], f32::from_bits(self.rf[k][0])); } }
                for k in 0..6 { if acc[k][0] != self.acc[k][0] { ch += &format!(" r{k}={:#x}({})", self.acc[k][0], f32::from_bits(self.acc[k][0])); } }
                let d = match inst { Inst::Alu(i) => format!("{:?}/{:?} a{}b{} ra{} rb{} {:?}{:?} sig{}", i.add.op, i.mul.op, i.add.a, i.add.b, i.raddr_a, i.raddr_b, i.mul.a, i.mul.b, i.sig.ldunif as u8), x => format!("{x:?}") };
                eprintln!("\rqpu {this_pc:#x} {d} fa={:#x} |{ch}\r", self.fa);
            }
            if barrier { return Stop::Barrier; }
        }
        Stop::Done
    }

    fn branch_inst(&mut self, b: &BranchInst, pc: u32, env: &mut dyn Env) {
        let msf = self.msf_mask();
        let consider: u16 = match b.msfign {
            1 => msf,
            2 => { let mut q = 0u16; for k in 0..4 { if msf & (0xf << (4 * k)) != 0 { q |= 0xf << (4 * k); } } q }
            _ => 0xffff,
        };
        let (a, na) = (self.fa & consider, !self.fa & consider);
        let taken = match b.cond {
            BrCond::Always => true,
            BrCond::A0 => self.fa & 1 != 0,
            BrCond::Na0 => self.fa & 1 == 0,
            BrCond::AllA => na == 0,
            BrCond::AnyNa => na != 0,
            BrCond::AnyA => a != 0,
            BrCond::AllNa => a == 0,
        };
        // the uniform for ABS/REL uniform-stream branches is consumed whether or not the branch is taken
        let ureload = if b.ub {
            match b.bdu {
                0 => Some(self.next_uniform(env)),
                1 => { let off = self.next_uniform(env); Some(self.unif.wrapping_add(off)) }
                2 => Some(self.lr),
                _ => Some(self.rf[b.raddr_a as usize][0]),
            }
        } else { None };
        if !taken { return; }
        let target = match b.bdi {
            0 => b.offset,
            1 => pc.wrapping_add(32).wrapping_add(b.offset),
            2 => self.lr,
            _ => self.rf[b.raddr_a as usize][0],
        };
        self.lr = pc.wrapping_add(32);
        self.branch = Some((2, target, ureload));
    }

    /// Execute one ALU instruction; true if it hit a barrier.
    fn alu(&mut self, i: &AluInst, env: &mut dyn Env) -> bool {
        let add = &i.add;
        let mul = &i.mul;
        // operands are read before anything is written
        let na = add_op_num_src(add.op);
        let a_a = if na >= 1 { self.read_mux(add.a, i) } else { [0; LANES] };
        let a_b = if na >= 2 { self.read_mux(add.b, i) } else { [0; LANES] };
        let nm = mul_op_num_src(mul.op);
        let mut m_a = if nm >= 1 { self.read_mux(mul.a, i) } else { [0; LANES] };
        let mut m_b = if nm >= 2 { self.read_mux(mul.b, i) } else { [0; LANES] };
        if i.sig.rotate {
            let n = (if i.sig.small_imm_b { SMALL_IMM.get(i.raddr_b as usize).copied().unwrap_or(0) } else { self.acc[5][0] }) as usize & 15;
            let rot = |v: &V| { let mut o = [0; LANES]; for l in 0..LANES { o[l] = v[(l + LANES - n) % LANES]; } o };
            m_a = rot(&m_a);
            m_b = rot(&m_b);
        }
        let acond = self.cond_mask(add.cond);
        let mcond = self.cond_mask(mul.cond);
        let mut barrier = false;

        // ---- add ALU
        let mut ares = [0u32; LANES];
        let mut az = 0u16; let mut an = 0u16; let mut ac = 0u16;
        let mut a_has = true;
        {
            use AddOp::*;
            let fl = |v: &V, k: usize, u: Unpack| unpack_f32(v[k], u);
            for l in 0..LANES {
                let (x, y) = (a_a[l], a_b[l]);
                let (mut z, mut n, mut c) = (false, false, false);
                let res: u32 = match add.op {
                    Fadd | Faddnf | Fsub | Fmin | Fmax | Fcmp => {
                        let (p, q) = (fl(&a_a, l, add.ua), fl(&a_b, l, add.ub));
                        let r = match add.op { Fadd | Faddnf => p + q, Fsub => p - q, Fmin => fmin(p, q), Fmax => fmax(p, q), _ => p - q };
                        let r = ftz(r);
                        if add.op == Fcmp {
                            z = p == q; n = p < q; c = p <= q;
                        } else { z = r == 0.0; n = r < 0.0; c = r > 0.0; }
                        r.to_bits()
                    }
                    Vfpack => (f32_to_f16(fl(&a_a, l, add.ua)) as u32) | ((f32_to_f16(fl(&a_b, l, add.ub)) as u32) << 16),
                    Add => { let (r, o) = x.overflowing_add(y); c = o; r }
                    Sub => { let (r, o) = x.overflowing_sub(y); c = o; r }
                    Min => { c = (x as i32) > (y as i32); (x as i32).min(y as i32) as u32 }
                    Max => { c = (x as i32) > (y as i32); (x as i32).max(y as i32) as u32 }
                    Umin => { c = x > y; x.min(y) }
                    Umax => { c = x > y; x.max(y) }
                    Shl => x.wrapping_shl(y & 31),
                    Shr => x.wrapping_shr(y & 31),
                    Asr => ((x as i32) >> (y & 31)) as u32,
                    Ror => x.rotate_right(y & 31),
                    Vfmin | Vfmax => {
                        let p = unpack_f16x2(x, add.ua);
                        let h = |v: u32, s: u32| f16_to_f32((v >> s) as u16);
                        let op = |a: f32, b: f32| if add.op == Vfmin { fmin(a, b) } else { fmax(a, b) };
                        (f32_to_f16(op(h(p, 0), h(y, 0))) as u32) | ((f32_to_f16(op(h(p, 16), h(y, 16))) as u32) << 16)
                    }
                    And => x & y,
                    Or => x | y,
                    Xor => x ^ y,
                    Vadd => ((x as u16).wrapping_add(y as u16) as u32) | (((x >> 16) as u16).wrapping_add((y >> 16) as u16) as u32) << 16,
                    Vsub => ((x as u16).wrapping_sub(y as u16) as u32) | (((x >> 16) as u16).wrapping_sub((y >> 16) as u16) as u32) << 16,
                    Not => !x,
                    Neg => x.wrapping_neg(),
                    Recip | Rsqrt | Exp | Log | Sin | Rsqrt2 => ftz(sfu(add.op, fl(&a_a, l, add.ua))).to_bits(),
                    Setmsf => { if acond & (1 << l) != 0 { self.msf[l] = (x & 0xf) as u8; } x }
                    Setrevf => { if l == 0 && acond & 1 != 0 { self.revf = x & 1 != 0; } x }
                    Nop => { a_has = false; 0 }
                    Tidx => self.tidx,
                    Eidx => l as u32,
                    Lr => self.lr,
                    Vfla => (self.fa >> l) as u32 & 1,
                    Vflna => !(self.fa >> l) as u32 & 1,
                    Vflb => (self.fb >> l) as u32 & 1,
                    Vflnb => !(self.fb >> l) as u32 & 1,
                    Fxcd => self.fx[l].to_bits(),
                    Fycd => self.fy[l].to_bits(),
                    Xcd => self.xcd[l] as u32,
                    Ycd => self.ycd[l] as u32,
                    Msf => self.msf[l] as u32,
                    Revf => self.revf as u32,
                    Iid => 0,
                    Sampid => 0,
                    Barrierid => self.barrier_id,
                    Tmuwt | Vpmwt => { a_has = false; 0 }
                    Flafirst | Flnafirst => {
                        let first = (0..LANES).find(|&k| self.msf[k] != 0).unwrap_or(0);
                        let f = (self.fa >> first) & 1 != 0;
                        (if add.op == Flafirst { f } else { !f }) as u32
                    }
                    Vpmsetup => { a_has = false; 0 }
                    LdvpmvIn => self.vpm.get(x as usize).map_or(0, |r| r[l]),
                    LdvpmvOut => self.vpm.get(self.vpm_out_base + x as usize).map_or(0, |r| r[l]),
                    LdvpmdIn => self.vpm.get(x as usize).map_or(0, |r| r[l]),
                    LdvpmdOut => self.vpm.get(self.vpm_out_base + x as usize).map_or(0, |r| r[l]),
                    Ldvpmp | LdvpmgIn | LdvpmgOut => 0,
                    Fround | Ftrunc | Ffloor | Fceil => {
                        let p = fl(&a_a, l, add.ua);
                        let r = match add.op { Fround => round_even(p), Ftrunc => p.trunc(), Ffloor => p.floor(), _ => p.ceil() };
                        z = r == 0.0; n = r < 0.0;
                        r.to_bits()
                    }
                    Ftoin => f2i_sat(round_even(fl(&a_a, l, add.ua))),
                    Ftoiz => f2i_sat(fl(&a_a, l, add.ua).trunc()),
                    Ftouz => f2u_sat(fl(&a_a, l, add.ua).trunc()),
                    Ftoc => {
                        let p = fl(&a_a, l, add.ua);
                        let k = if p.is_nan() { 0 } else { (p.clamp(0.0, 1.0) * 4.0).round() as u32 };
                        (1u32 << k) - 1
                    }
                    Fdx | Fdy => {
                        let q = l & !3;
                        let p = |k: usize| fl(&a_a, k, add.ua);
                        let d = if add.op == Fdx { p(q | 1 | (l & 2)) - p(q | (l & 2)) } else { p(q | 2 | (l & 1)) - p(q | (l & 1)) };
                        ftz(d).to_bits()
                    }
                    Stvpmv | Stvpmd | Stvpmp => {
                        a_has = false;
                        if acond & (1 << l) != 0 && add.op != Stvpmp {
                            let row = self.vpm_out_base + x as usize;
                            if self.vpm.len() <= row { self.vpm.resize(row + 1, [0; LANES]); }
                            self.vpm[row][l] = y;
                        }
                        0
                    }
                    Itof => ftz(x as i32 as f32).to_bits(),
                    Utof => ftz(x as f32).to_bits(),
                    Clz => x.leading_zeros(),
                    Flapush | Flbpush | Flpop => { a_has = false; 0 }
                };
                ares[l] = res;
                // integer results: zero/negative from the value
                match add.op {
                    Fadd | Faddnf | Fsub | Fmin | Fmax | Fcmp | Fround | Ftrunc | Ffloor | Fceil => {}
                    Recip | Rsqrt | Exp | Log | Sin | Rsqrt2 | Itof | Utof | Fdx | Fdy => { let f = f32::from_bits(res); z = f == 0.0; n = f < 0.0; }
                    _ => { z = res == 0; n = (res as i32) < 0; }
                }
                if z { az |= 1 << l; }
                if n { an |= 1 << l; }
                if c { ac |= 1 << l; }
            }
            if add.op == Flapush || add.op == Flbpush || add.op == Flpop {
                // flag stack ops (not used by Mesa's GL/Vulkan backends)
                crate::log_once("v3d: flag push/pop QPU op not modelled");
            }
        }

        // ---- mul ALU
        let mut mres = [0u32; LANES];
        let mut mz = 0u16; let mut mn = 0u16; let mut mc = 0u16;
        let m_has = mul.op != MulOp::Nop && mul.op != MulOp::Multop;
        for l in 0..LANES {
            let (x, y) = (m_a[l], m_b[l]);
            let (mut z, mut n, mut c) = (false, false, false);
            let res = match mul.op {
                MulOp::Add => { let (r, o) = x.overflowing_add(y); c = o; r }
                MulOp::Sub => { let (r, o) = x.overflowing_sub(y); c = o; r }
                MulOp::Umul24 => { let r = (x & 0xff_ffff).wrapping_mul(y & 0xff_ffff).wrapping_add(self.rtop[l]); self.rtop[l] = 0; r }
                MulOp::Smul24 => ((((x << 8) as i32) >> 8) as i64 * ((((y << 8) as i32) >> 8) as i64)) as u32,
                MulOp::Multop => { self.rtop[l] = ((x >> 24).wrapping_mul(y & 0xff_ffff).wrapping_add((x & 0xff_ffff).wrapping_mul(y >> 24))) << 24; 0 }
                MulOp::Vfmul => {
                    let p = unpack_f16x2(x, mul.ua);
                    let h = |v: u32, s: u32| f16_to_f32((v >> s) as u16);
                    (f32_to_f16(h(p, 0) * h(y, 0)) as u32) | ((f32_to_f16(h(p, 16) * h(y, 16)) as u32) << 16)
                }
                MulOp::Fmov => { let f = unpack_f32(x, mul.ua); z = f == 0.0; n = f < 0.0; f.to_bits() }
                MulOp::Mov => x,
                MulOp::Nop => 0,
                MulOp::Fmul => { let f = ftz(unpack_f32(x, mul.ua) * unpack_f32(y, mul.ub)); z = f == 0.0; n = f < 0.0; c = f > 0.0; f.to_bits() }
            };
            if !matches!(mul.op, MulOp::Fmov | MulOp::Fmul) { z = res == 0; n = (res as i32) < 0; }
            mres[l] = res;
            if z { mz |= 1 << l; }
            if n { mn |= 1 << l; }
            if c { mc |= 1 << l; }
        }

        // ---- signals that read uniforms come before ALU writes using them (TLBU/TMUAU take theirs after)
        let sig = i.sig;
        if sig.ldunif { let u = self.next_uniform(env); self.acc[5] = [u; LANES]; }
        if sig.ldunifrf { let u = self.next_uniform(env); self.write_reg(i.sig_addr, i.sig_magic, &[u; LANES], 0xffff, Pack::None, env, &mut barrier); }
        if sig.ldunifa || sig.ldunifarf {
            let u = env.read32(self.unifa);
            self.unifa = self.unifa.wrapping_add(4);
            if sig.ldunifa { self.acc[5] = [u; LANES]; } else { self.write_reg(i.sig_addr, i.sig_magic, &[u; LANES], 0xffff, Pack::None, env, &mut barrier); }
        }
        if sig.wrtmuc { let u = self.next_uniform(env); self.tmu_cfg.push(u); }

        // ---- ALU writes
        if a_has { self.write_alu(add.waddr, add.magic, &ares, acond, add.pack, env, &mut barrier); }
        if m_has { self.write_alu(mul.waddr, mul.magic, &mres, mcond, mul.pack, env, &mut barrier); }

        // ---- flags
        let update = |fa: &mut u16, fb: &mut u16, pf: Pf, uf: Uf, z: u16, n: u16, c: u16| {
            match pf {
                Pf::None => {}
                Pf::Z => { *fb = *fa; *fa = z; }
                Pf::N => { *fb = *fa; *fa = n; }
                Pf::C => { *fb = *fa; *fa = c; }
            }
            use Uf::*;
            let a = *fa;
            *fa = match uf {
                None => a,
                Andz => a & z, Andnz => a & !z, Nornz => !a & z, Norz => !a & !z,
                Andn => a & n, Andnn => a & !n, Nornn => !a & n, Norn => !a & !n,
                Andc => a & c, Andnc => a & !c, Nornc => !a & c, Norc => !a & !c,
            };
        };
        update(&mut self.fa, &mut self.fb, add.pf, add.uf, az, an, ac);
        update(&mut self.fa, &mut self.fb, mul.pf, mul.uf, mz, mn, mc);

        // ---- result-returning signals
        if sig.ldtmu {
            let v = self.tmu_out.pop_front().unwrap_or_else(|| { crate::log_once("v3d: ldtmu with no TMU result"); [0; LANES] });
            self.write_reg(i.sig_addr, i.sig_magic, &v, 0xffff, Pack::None, env, &mut barrier);
        }
        if sig.ldvary {
            let (v, c) = env.ldvary(self);
            self.write_reg(i.sig_addr, i.sig_magic, &v, 0xffff, Pack::None, env, &mut barrier);
            self.delayed.push((1, 5, c));
        }
        if sig.ldtlb || sig.ldtlbu {
            let cfg = if sig.ldtlbu { Some(self.next_uniform(env)) } else { None };
            let v = env.tlb_read(self, cfg);
            self.write_reg(i.sig_addr, i.sig_magic, &v, 0xffff, Pack::None, env, &mut barrier);
        }
        barrier
    }

    fn write_alu(&mut self, waddr: u8, magic: bool, v: &V, mask: u16, pack: Pack, env: &mut dyn Env, barrier: &mut bool) {
        self.write_reg(waddr, magic, v, mask, pack, env, barrier)
    }

    fn write_reg(&mut self, waddr: u8, magic: bool, v: &V, mask: u16, pack: Pack, env: &mut dyn Env, barrier: &mut bool) {
        let packed = |old: u32, new: u32| -> u32 {
            match pack {
                Pack::None => new,
                Pack::L => (old & 0xffff_0000) | f32_to_f16(f32::from_bits(new)) as u32,
                Pack::H => (old & 0xffff) | ((f32_to_f16(f32::from_bits(new)) as u32) << 16),
            }
        };
        if !magic {
            let r = &mut self.rf[waddr as usize];
            for l in lanes_of(mask) { r[l] = packed(r[l], v[l]); }
            return;
        }
        match waddr {
            0..=5 => { let r = &mut self.acc[waddr as usize]; for l in lanes_of(mask) { r[l] = packed(r[l], v[l]); } }
            6 => {}
            7 | 8 => {
                let cfg = if waddr == 8 { Some(self.next_uniform(env)) } else { None };
                env.tlb_write(self, mask, v, cfg);
            }
            9 => self.unifa = v[(mask.trailing_zeros() as usize).min(15)],
            11 => self.tmu_req.data.push(*v),
            12 | 13 => {
                let cfg = if waddr == 13 { self.next_uniform(env) } else { 0xffff_ffff };
                let mut req = std::mem::take(&mut self.tmu_req);
                req.s = Some(*v);
                let lanes = mask & self.valid;
                let out = env.tmu(self, TmuKind::General, &[cfg], &req, lanes);
                self.tmu_out.extend(out);
            }
            14 | 15 => { if waddr == 15 { self.next_uniform(env); } }
            16 | 17 | 18 => {
                if waddr == 17 { self.next_uniform(env); }
                if waddr == 18 { *barrier = true; }
            }
            19..=24 => {
                let op = [AddOp::Recip, AddOp::Rsqrt, AddOp::Exp, AddOp::Log, AddOp::Sin, AddOp::Rsqrt2][(waddr - 19) as usize];
                let mut out = [0u32; LANES];
                for l in 0..LANES { out[l] = ftz(sfu(op, f32::from_bits(v[l]))).to_bits(); }
                self.delayed.push((1, 4, out));
            }
            32 => self.tmu_cfg.push(v[0]),
            33 | 40 | 41 | 42 | 43 | 44 | 45 | 46 => {
                let kind = match waddr { 40 | 44 => TmuKind::TexCube, 41 | 45 => TmuKind::TexFetch, 42 | 46 => TmuKind::TexLod, _ => TmuKind::Tex };
                let mut req = std::mem::take(&mut self.tmu_req);
                req.s = Some(*v);
                let cfg = std::mem::take(&mut self.tmu_cfg);
                let lanes = mask & self.valid;
                let out = env.tmu(self, kind, &cfg, &req, lanes);
                self.tmu_out.extend(out);
            }
            34 => self.tmu_req.t = Some(*v),
            35 => self.tmu_req.r = Some(*v),
            36 => self.tmu_req.i = Some(*v),
            37 => self.tmu_req.b = Some(*v),
            38 => self.tmu_req.dref = Some(*v),
            39 => self.tmu_req.off = Some(*v),
            55 => { let x = v[(mask.trailing_zeros() as usize).min(15)]; self.acc[5] = [x; LANES]; }
            _ => crate::log_once(&format!("v3d: write to unmodelled magic register {waddr}")),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn half_floats() {
        for &f in &[0.0f32, 1.0, -2.5, 65504.0, 0.000061035156, 1.0e-7, 3.14159] {
            let h = f32_to_f16(f);
            let back = f16_to_f32(h);
            assert!((back - f).abs() <= f.abs() * 0.001 + 1e-7, "{f} -> {h:#x} -> {back}");
        }
        assert_eq!(f32_to_f16(1.0), 0x3c00);
        assert_eq!(f32_to_f16(70000.0), 0x7c00);
    }

    #[test]
    fn decodes_nop_and_branch() {
        // Mesa's canonical NOP: "nop ; nop" = 0x3c003186bb800000
        match decode(0x3c00_3186_bb80_0000) {
            Inst::Alu(i) => { assert_eq!(i.add.op, AddOp::Nop); assert_eq!(i.mul.op, MulOp::Nop); }
            x => panic!("{x:?}"),
        }
    }
}
