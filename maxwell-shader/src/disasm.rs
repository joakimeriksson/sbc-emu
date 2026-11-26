//! Text of decoded instructions, for debugging.

use crate::isa::*;
use crate::program::Program;
use std::fmt::{self, Display, Write};

struct R(Reg);

impl Display for R {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        if self.0 == RZ {
            write!(f, "rz")
        } else {
            write!(f, "r{}", self.0)
        }
    }
}

impl Display for Pred {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        f.write_str(if self.not { "!" } else { "" })?;
        match self.idx {
            7 => write!(f, "pt"),
            15 => write!(f, "upt"),
            n @ 8.. => write!(f, "up{}", n - 8),
            n => write!(f, "p{n}"),
        }
    }
}

struct P(u8);

impl Display for P {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        Pred { idx: self.0, not: false }.fmt(f)
    }
}

/// A source as a float or an integer, with modifiers.
struct S(Src, Mod, bool);

impl Display for S {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        let S(s, m, float) = self;
        f.write_str(if m.neg { "-" } else { "" })?;
        f.write_str(if m.abs { "|" } else { "" })?;
        match *s {
            Src::Reg(r) => R(r).fmt(f)?,
            Src::Cb(i, o) => write!(f, "c{i}[{o:#x}]")?,
            Src::Imm(v) if *float => write!(f, "{:?}f", f32::from_bits(v))?,
            Src::Imm(v) => write!(f, "{v:#x}")?,
        }
        f.write_str(if m.abs { "|" } else { "" })
    }
}

fn i(s: Src) -> S {
    S(s, Mod::default(), false)
}

fn fl(s: Src) -> S {
    S(s, Mod::default(), true)
}

fn neg(s: Src, n: bool) -> S {
    S(s, Mod { abs: false, neg: n }, false)
}

fn r(x: Reg) -> Src {
    Src::Reg(x)
}

fn dot(on: bool, s: &str) -> &str {
    if on {
        s
    } else {
        ""
    }
}

fn rnd(x: Rnd) -> &'static str {
    match x {
        Rnd::N => "",
        Rnd::M => ".rm",
        Rnd::P => ".rp",
        Rnd::Z => ".rz",
    }
}

fn fmz(x: Fmz) -> &'static str {
    if x.dnz {
        ".fmz"
    } else if x.ftz {
        ".ftz"
    } else {
        ""
    }
}

fn cc(x: bool) -> &'static str {
    if x {
        ".cc"
    } else {
        ""
    }
}

const FCOND: [&str; 16] = [
    "f", "lt", "eq", "le", "gt", "ne", "ge", "num", "nan", "ltu", "equ", "leu", "gtu", "neu",
    "geu", "t",
];
const ICOND: [&str; 8] = ["f", "lt", "eq", "le", "gt", "ne", "ge", "t"];

fn bop(b: BoolOp) -> &'static str {
    match b {
        BoolOp::And => ".and",
        BoolOp::Or => ".or",
        BoolOp::Xor => ".xor",
    }
}

fn ty(t: IntTy) -> String {
    format!("{}{}", if t.signed { 's' } else { 'u' }, t.bytes * 8)
}

fn flow(cond: u8) -> String {
    match cond {
        15 => String::new(),
        n if n < 15 => format!(" cc.{}", FCOND[n as usize]),
        n => format!(" cc.{n}"),
    }
}

/// Register list of a memory or texture operand.
fn vec(d: Reg, n: u32) -> String {
    if d == RZ || n <= 1 {
        R(d).to_string()
    } else {
        format!("r{}:{}", d, d as u32 + n - 1)
    }
}

fn words(t: MemTy) -> u32 {
    (t.bytes() as u32).div_ceil(4)
}

fn addr(a: Reg, off: i32) -> String {
    match (a, off) {
        (RZ, o) => format!("[{o:#x}]"),
        (a, 0) => format!("[r{a}]"),
        (a, o) if o < 0 => format!("[r{a}-{:#x}]", -(o as i64)),
        (a, o) => format!("[r{a}+{o:#x}]"),
    }
}

fn fcmp(c: &FCmpArgs) -> String {
    format!(
        "{}{}{} {} {} {}",
        FCOND[c.cond as usize & 15],
        dot(c.ftz, ".ftz"),
        bop(c.bop),
        S(r(c.a), c.am, true),
        S(c.b, c.bm, true),
        c.p
    )
}

fn icmp(c: &ICmpArgs) -> String {
    format!(
        "{}{}{}{} {} {} {}{}",
        ICOND[c.cond as usize & 7],
        dot(!c.signed, ".u32"),
        dot(c.x, ".x"),
        bop(c.bop),
        R(c.a),
        i(c.b),
        c.p,
        c.ex.map_or(String::new(), |p| format!(" ex {p}"))
    )
}

fn surf(s: &Surf) -> String {
    let raw = if s.raw { format!(".{:?}", s.ty) } else { format!(".{:x}", s.mask) };
    format!("{raw}.{:?} coord {} handle {}", s.dim, R(s.coord), i(s.handle))
}

fn tex(t: &Tex) -> String {
    let mut o = String::new();
    let name = match (t.kind, t.scalar) {
        (TexKind::Sample, false) => "tex",
        (TexKind::Sample, true) => "texs",
        (TexKind::Grad, _) => "txd",
        (TexKind::Fetch, false) => "tld",
        (TexKind::Fetch, true) => "tlds",
        (TexKind::Gather, false) => "tld4",
        (TexKind::Gather, true) => "tld4s",
        (TexKind::QueryLod, _) => "tmml",
        (TexKind::Query, _) => "txq",
    };
    let _ = write!(o, "{name}.{:?}{}", t.dim, dot(t.array, ".array"));
    let _ = match t.lod_mode {
        LodMode::Auto => Ok(()),
        LodMode::Zero => write!(o, ".lz"),
        LodMode::Bias => write!(o, ".lb"),
        LodMode::Level => write!(o, ".ll"),
    };
    let _ = write!(o, "{}{}", dot(t.shadow, ".dc"), dot(t.ndv, ".ndv"));
    if matches!(t.kind, TexKind::Gather | TexKind::Query) {
        let _ = write!(o, ".{}", t.comp);
    }
    let _ = write!(o, " {}", R(t.d0));
    if t.scalar {
        let _ = write!(o, " {}", R(t.d1));
    }
    let _ = write!(o, " mask {:x}", t.mask);
    let _ = match t.bindless {
        true => write!(o, " handle {}", R(t.handle)),
        false => write!(o, " handle #{:#x}", t.index),
    };
    if t.kind != TexKind::Query {
        let _ = write!(o, " (");
        for (n, c) in t.coord.iter().enumerate() {
            if *c != RZ || n == 0 {
                let _ = write!(o, "{}{}", if n > 0 { " " } else { "" }, R(*c));
            }
        }
        let _ = write!(o, ")");
    }
    for (on, n, x) in [
        (t.array, "layer", t.layer),
        (matches!(t.lod_mode, LodMode::Bias | LodMode::Level), "lod", t.lod),
        (t.shadow, "ref", t.dref),
        (t.ms, "sample", t.sample),
        (t.offset_kind != OffsetKind::None, "offset", t.offset),
        (t.kind == TexKind::Grad, "deriv", t.deriv),
    ] {
        if on {
            let _ = write!(o, " {n} {}", R(x));
        }
    }
    o
}

/// One instruction. `at` gives the code address of a jump target (identity for an instruction
/// that is not linked into a program).
pub fn inst(x: &Inst, at: &dyn Fn(Target) -> u32) -> String {
    let t = |t: Target| format!("{:#x}", at(t));
    let s = match x.op {
        Op::Nop => "nop".into(),
        Op::Invalid(w) => format!("??? {w:#018x}"),
        Op::FAdd { d, a, am, b, bm, ftz, sat, cc: c, rnd: rn } => format!(
            "fadd{}{}{}{} {} {} {}",
            dot(ftz, ".ftz"),
            rnd(rn),
            dot(sat, ".sat"),
            cc(c),
            R(d),
            S(r(a), am, true),
            S(b, bm, true)
        ),
        Op::FMul { d, a, b, neg, fmz: z, sat, cc: c, rnd: rn, scale } => {
            let sc = if scale == 1.0 { String::new() } else { format!(".x{scale}") };
            let m = Mod { abs: false, neg };
            format!("fmul{}{}{sc}{}{} {} {} {}", fmz(z), rnd(rn), dot(sat, ".sat"), cc(c), R(d), S(r(a), m, true), fl(b))
        }
        Op::Ffma { d, a, b, c, neg_ab, neg_c, fmz: z, sat, cc: f, rnd: rn } => format!(
            "ffma{}{}{}{} {} {} {} {}",
            fmz(z),
            rnd(rn),
            dot(sat, ".sat"),
            cc(f),
            R(d),
            S(r(a), Mod { abs: false, neg: neg_ab }, true),
            fl(b),
            S(c, Mod { abs: false, neg: neg_c }, true)
        ),
        Op::FMnMx { d, a, am, b, bm, ftz, cc: c, p } => {
            let m = match p {
                PT => "fmin".to_string(),
                Pred { idx: 7, .. } => "fmax".to_string(),
                _ => format!("fmnmx({p})"),
            };
            format!("{m}{}{} {} {} {}", dot(ftz, ".ftz"), cc(c), R(d), S(r(a), am, true), S(b, bm, true))
        }
        Op::FCmp { d, a, b, c, cond, ftz } => format!(
            "fcmp.{}{} {} {} {} {}",
            FCOND[cond as usize & 15],
            dot(ftz, ".ftz"),
            R(d),
            R(a),
            fl(b),
            fl(c)
        ),
        Op::FSet { d, ref cmp, bf, cc: c } => {
            format!("fset{}{} {} {}", dot(bf, ".bf"), cc(c), R(d), fcmp(cmp))
        }
        Op::FSetP { pd, pn, ref cmp } => format!("fsetp {} {} {}", P(pd), P(pn), fcmp(cmp)),
        Op::FSwzAdd { d, a, b, ops, ftz, .. } => {
            format!("fswzadd{} {} {} {} {ops:#04x}", dot(ftz, ".ftz"), R(d), R(a), R(b))
        }
        Op::Mufu { d, a, m, op, sat } => {
            format!("mufu.{op:?}{} {} {}", dot(sat, ".sat"), R(d), S(r(a), m, true)).to_lowercase()
        }
        Op::Rro { d, b, m } => format!("rro {} {}", R(d), S(b, m, true)),
        Op::DAdd { d, a, am, b, bm, .. } => {
            format!("dadd {} {} {}", R(d), S(r(a), am, true), S(b, bm, true))
        }
        Op::DMul { d, a, b, neg, .. } => {
            format!("dmul {} {} {}", R(d), S(r(a), Mod { abs: false, neg }, true), fl(b))
        }
        Op::Dfma { d, a, b, c, neg_ab, neg_c, .. } => format!(
            "dfma {} {} {} {}",
            R(d),
            S(r(a), Mod { abs: false, neg: neg_ab }, true),
            fl(b),
            S(c, Mod { abs: false, neg: neg_c }, true)
        ),
        Op::DMnMx { d, a, am, b, bm, p, .. } => {
            format!("dmnmx({p}) {} {} {}", R(d), S(r(a), am, true), S(b, bm, true))
        }
        Op::DSet { d, ref cmp, bf, .. } => {
            format!("dset{} {} {}", dot(bf, ".bf"), R(d), fcmp(cmp))
        }
        Op::DSetP { pd, pn, ref cmp } => format!("dsetp {} {} {}", P(pd), P(pn), fcmp(cmp)),
        Op::F2F { d, b, m, from, to, rnd: rn, int, high, ftz, sat, cc: c } => format!(
            "f2f.f{}.f{}{}{}{}{}{}{} {} {}",
            to * 8,
            from * 8,
            dot(int, ".int"),
            rnd(rn),
            dot(high, ".h1"),
            dot(ftz, ".ftz"),
            dot(sat, ".sat"),
            cc(c),
            R(d),
            S(b, m, true)
        ),
        Op::F2I { d, b, m, from, to, rnd: rn, high, ftz, cc: c } => format!(
            "f2i.{}.f{}{}{}{}{} {} {}",
            ty(to),
            from * 8,
            rnd(rn),
            dot(high, ".h1"),
            dot(ftz, ".ftz"),
            cc(c),
            R(d),
            S(b, m, true)
        ),
        Op::I2F { d, b, m, from, to, sel, rnd: rn, .. } => {
            format!("i2f.f{}.{}.b{sel}{} {} {}", to * 8, ty(from), rnd(rn), R(d), S(b, m, false))
        }
        Op::I2I { d, b, m, from, to, sel, sat, cc: c } => format!(
            "i2i.{}.{}.b{sel}{}{} {} {}",
            ty(to),
            ty(from),
            dot(sat, ".sat"),
            cc(c),
            R(d),
            S(b, m, false)
        ),
        Op::IAdd { d, a, b, neg_a, neg_b, x: e, sat, cc: c } => format!(
            "iadd{}{}{} {} {} {}",
            dot(e, ".x"),
            dot(sat, ".sat"),
            cc(c),
            R(d),
            neg(r(a), neg_a),
            neg(b, neg_b)
        ),
        Op::IAdd3P { d, a, b, c, neg: n, x: e, cin, cout } => format!(
            "iadd3{} {} {} {}, {}{}, {}{}, {}{} {} {}",
            dot(e, ".x"),
            R(d),
            Pred { idx: cout[0], not: false },
            Pred { idx: cout[1], not: false },
            if n[0] { "-" } else { "" },
            R(a),
            if n[1] { "-" } else { "" },
            i(b),
            if n[2] { "-" } else { "" },
            i(c),
            cin[0],
            cin[1]
        ),
        Op::IAdd3 { d, a, b, c, neg: n, half, shift, x: e, cc: f } => format!(
            "iadd3{}{} {} {} {} {} half {half:?} shift {shift}",
            dot(e, ".x"),
            cc(f),
            R(d),
            neg(r(a), n[0]),
            neg(b, n[1]),
            neg(r(c), n[2])
        ),
        Op::IScAdd { d, a, b, shift, neg_a, neg_b, cc: c } => {
            format!("iscadd{} {} {} {} {shift}", cc(c), R(d), neg(r(a), neg_a), neg(b, neg_b))
        }
        Op::Vadd { d, a, b, c, op, .. } => format!("vadd.op{op} {} {} {} {}", R(d), R(a), i(b), R(c)),
        Op::P2r { d, a, b, cc: c, byte } => format!("p2r{} {} {} {} b{byte}", dot(c, ".cc"), R(d), R(a), i(b)),
        Op::R2p { a, b, cc: c, byte } => format!("r2p{} {} {} b{byte}", dot(c, ".cc"), R(a), i(b)),
        Op::Fchk { pd, a, b, .. } => format!("fchk.divide p{pd} {} {}", R(a), i(b)),
        Op::Lea { d, a, b, hi, shift, neg_a, x, cc: c, pd } => format!(
            "lea{}{}{} p{pd} {} {} {}{} {shift}",
            dot(hi.is_some(), ".hi"),
            dot(x, ".x"),
            cc(c),
            R(d),
            neg(r(a), neg_a),
            i(b),
            hi.map_or(String::new(), |c| format!(" {}", R(c)))
        ),
        Op::IMul { d, a, b, sa, sb, hi, .. } => {
            format!("imul.{}{}{} {} {} {}", sign(sa), sign(sb), dot(hi, ".hi"), R(d), R(a), i(b))
        }
        Op::IMad { d, a, b, c, sa, sb, hi, neg_ab, neg_c, x: e, cc: f, .. } => format!(
            "imad.{}{}{}{}{} {} {} {} {}",
            sign(sa),
            sign(sb),
            dot(hi, ".hi"),
            dot(e, ".x"),
            cc(f),
            R(d),
            neg(r(a), neg_ab),
            i(b),
            neg(c, neg_c)
        ),
        Op::Xmad { d, a, b, c, ha, hb, sa, sb, psl, mrg, mode, x: e, cc: f } => format!(
            "xmad.{}{}{}{}{}{}{} {} {}{} {}{} {}",
            sign(sa),
            sign(sb),
            dot(psl, ".psl"),
            dot(mrg, ".mrg"),
            ["", ".clo", ".chi", ".csfu", ".cbcc", ".5", ".6", ".7"][mode as usize & 7],
            dot(e, ".x"),
            cc(f),
            R(d),
            R(a),
            dot(ha, ".h1"),
            i(b),
            dot(hb, ".h1"),
            i(c)
        ),
        Op::IMnMx { d, a, b, signed, part, p, .. } => {
            format!("imnmx({p}).{}{} {} {} {}", sign(signed), ["", ".xlo", ".xmed", ".xhi"][part as usize & 3], R(d), R(a), i(b))
        }
        Op::ICmp { d, a, b, c, cond, signed } => format!(
            "icmp.{}.{} {} {} {} {}",
            ICOND[cond as usize & 7],
            sign(signed),
            R(d),
            R(a),
            i(b),
            i(c)
        ),
        Op::ISet { d, ref cmp, bf, cc: c } => {
            format!("iset{}{} {} {}", dot(bf, ".bf"), cc(c), R(d), icmp(cmp))
        }
        Op::ISetP { pd, pn, ref cmp } => format!("isetp {} {} {}", P(pd), P(pn), icmp(cmp)),
        Op::Lop { d, a, b, not_a, not_b, op, test, pd, cc: c, .. } => format!(
            "lop.{}{} {}{} {}{} {}{}",
            ["and", "or", "xor", "b"][op as usize & 3],
            cc(c),
            R(d),
            if pd == 7 { String::new() } else { format!(" {} test {test}", P(pd)) },
            dot(not_a, "~"),
            R(a),
            dot(not_b, "~"),
            i(b)
        ),
        Op::Lop3 { d, a, b, c, lut, .. } => {
            format!("lop3 {} {} {} {} {lut:#04x}", R(d), R(a), i(b), R(c))
        }
        Op::Shl { d, a, b, wrap, cc: c, .. } => {
            format!("shl{}{} {} {} {}", dot(wrap, ".w"), cc(c), R(d), R(a), i(b))
        }
        Op::Shr { d, a, b, signed, wrap, rev, .. } => format!(
            "shr.{}{}{} {} {} {}",
            sign(signed),
            dot(wrap, ".w"),
            dot(rev, ".rev"),
            R(d),
            R(a),
            i(b)
        ),
        Op::Shf { d, lo, hi, b, right, high, wrap, wide, signed, .. } => format!(
            "shf.{}{}{}{}{} {} {} {} {}",
            if right { "r" } else { "l" },
            dot(wide, if signed { ".s64" } else { ".u64" }),
            dot(high, ".hi"),
            dot(wrap, ".w"),
            "",
            R(d),
            R(lo),
            i(b),
            R(hi)
        ),
        Op::Bfe { d, a, b, signed, rev, .. } => {
            format!("bfe.{}{} {} {} {}", sign(signed), dot(rev, ".rev"), R(d), R(a), i(b))
        }
        Op::Bfi { d, a, b, c, .. } => format!("bfi {} {} {} {}", R(d), R(a), i(b), i(c)),
        Op::Flo { d, b, not, signed, shift, .. } => format!(
            "flo.{}{} {} {}{}",
            sign(signed),
            dot(shift, ".sh"),
            R(d),
            dot(not, "~"),
            i(b)
        ),
        Op::PopC { d, b, not } => format!("popc {} {}{}", R(d), dot(not, "~"), i(b)),
        Op::Prmt { d, a, b, c, mode } => {
            format!("prmt.{mode} {} {} {} {}", R(d), R(a), i(b), i(c))
        }
        Op::Sel { d, a, b, p } => format!("sel {} {} {} {p}", R(d), R(a), i(b)),
        Op::Mov { d, b, lanes } => {
            let l = if lanes == 15 { String::new() } else { format!(" lanes {lanes:x}") };
            format!("mov {} {}{l}", R(d), i(b))
        }
        Op::PSet { d, a, b, c, op1, op2, bf, .. } => {
            format!("pset{}{}{} {} {a} {b} {c}", dot(bf, ".bf"), bop(op1), bop(op2), R(d))
        }
        Op::PSetP { pd, pn, a, b, c, op1, op2 } => {
            format!("psetp{}{} {} {} {a} {b} {c}", bop(op1), bop(op2), P(pd), P(pn))
        }
        Op::S2R { d, sys } => format!("s2r {} sr{sys:#04x}", R(d)),
        Op::Vote { d, pd, p, op } => format!("vote.{op:?} {} {} {p}", R(d), P(pd)),
        Op::Shfl { d, pd, a, b, c, op } => {
            format!("shfl.{op:?} {} {} {} {} {}", R(d), P(pd), R(a), i(b), i(c))
        }
        Op::Ldc { d, idx, a, off, ty, mode } => {
            format!("ldc.{ty:?} {} c{idx}{} mode {mode}", vec(d, words(ty)), addr(a, off))
        }
        Op::Ld { d, a, off, ty, wide } => {
            format!("ld.{ty:?}{} {} g{}", dot(wide, ".e"), vec(d, words(ty)), addr(a, off))
        }
        Op::St { r: v, a, off, ty, wide } => {
            format!("st.{ty:?}{} g{} {}", dot(wide, ".e"), addr(a, off), vec(v, words(ty)))
        }
        Op::Ldl { d, a, off, ty } => format!("ldl.{ty:?} {} l{}", vec(d, words(ty)), addr(a, off)),
        Op::Stl { r: v, a, off, ty } => {
            format!("stl.{ty:?} l{} {}", addr(a, off), vec(v, words(ty)))
        }
        Op::Lds { d, a, off, ty } => format!("lds.{ty:?} {} s{}", vec(d, words(ty)), addr(a, off)),
        Op::Sts { r: v, a, off, ty } => {
            format!("sts.{ty:?} s{} {}", addr(a, off), vec(v, words(ty)))
        }
        Op::Ldgsts { s, soff, g, goff, ty, p } => format!("ldgsts.{ty:?} s{} g{} {p}", addr(s, soff), addr(g, goff)),
        Op::Atom { d, a, b, off, op, ty, wide } => format!(
            "atom.{op:?}.{ty:?}{} {} g{} {}",
            dot(wide, ".e"),
            R(d),
            addr(a, off),
            R(b)
        ),
        Op::AtomS { d, a, b, off, op, ty } => {
            format!("atoms.{op:?}.{ty:?} {} s{} {}", R(d), addr(a, off), R(b))
        }
        Op::Ald { d, a, v, addr: at, n, patch, output } => format!(
            "ald{}{} {} a{} vertex {}",
            dot(patch, ".p"),
            dot(output, ".o"),
            vec(d, n as u32),
            addr(a, at as i32),
            R(v)
        ),
        Op::Ast { r: v, a, v: vtx, addr: at, n, patch } => format!(
            "ast{} a{} {} vertex {}",
            dot(patch, ".p"),
            addr(a, at as i32),
            vec(v, n as u32),
            R(vtx)
        ),
        Op::Al2p { d, a, addr: at, output, .. } => {
            format!("al2p{} {} a{}", dot(output, ".o"), R(d), addr(a, at as i32))
        }
        Op::Isberd { d, a } => format!("isberd {} {}", R(d), R(a)),
        Op::Ipa { d, addr: at, idx, mul, offset, mode, loc, sat } => format!(
            "ipa.{mode:?}.{loc:?}{} {} a{} {} {}",
            dot(sat, ".sat"),
            R(d),
            addr(idx.unwrap_or(RZ), at as i32),
            R(mul),
            R(offset)
        ),
        Op::Pixld { d, a, what } => format!("pixld.{what:?} {} {}", R(d), R(a)),
        Op::Out { d, a, b, emit, cut } => {
            format!("out{}{} {} {} {}", dot(emit, ".emit"), dot(cut, ".cut"), R(d), R(a), i(b))
        }
        Op::Tex(ref t) => tex(t),
        Op::Suld(ref s) => format!("suld{} -> {}", surf(s), R(s.r)),
        Op::Sust(ref s) => format!("sust{} <- {}", surf(s), R(s.r)),
        Op::Suatom { ref s, b, op, ty } => {
            format!("suatom.{op:?}.{ty:?}{} {} {}", surf(s), R(s.r), R(b))
        }
        Op::Exit { cond } => format!("exit{}", flow(cond)),
        Op::Kil { cond } => format!("kil{}", flow(cond)),
        Op::Bra { t: to, cond, all } => format!("bra{}{} {}", dot(all, ".u"), flow(cond), t(to)),
        Op::Brx { a, base, cond } => format!("brx{} {}+{base:#x}", flow(cond), R(a)),
        Op::Cal { t: to } => format!("cal {}", t(to)),
        Op::Ret { cond } => format!("ret{}", flow(cond)),
        Op::Pret { t: to } => format!("pret {}", t(to)),
        Op::Ssy { t: to } => format!("ssy {}", t(to)),
        Op::Sync { cond } => format!("sync{}", flow(cond)),
        Op::Pbk { t: to } => format!("pbk {}", t(to)),
        Op::Brk { cond } => format!("brk{}", flow(cond)),
        Op::Pcnt { t: to } => format!("pcnt {}", t(to)),
        Op::Cont { cond } => format!("cont{}", flow(cond)),
        Op::Sam => "sam".into(),
        Op::Ram => "ram".into(),
        Op::Bar => "bar".into(),
        Op::Fence => "fence".into(),
        Op::Membar => "membar".into(),
        Op::Bpt => "bpt".into(),
        Op::Half { d, op, s, f32, .. } => format!("half.{op:?}{} r{d}, r{}, {:?}, {:?} {:?}", if f32 { ".f32" } else { "" }, s.a, s.b, s.c, s.swz),
        Op::PackHalves { d, a, b, .. } => format!("f2fp.pack_ab r{d}, r{a}, {b:?}"),
        Op::HSetP { pd, pn, s, cond, .. } => format!("hsetp p{pd}, p{pn}, r{}, {:?} cond {cond}", s.a, s.b),
    };
    if x.pred == PT {
        s
    } else {
        format!("({}) {s}", x.pred)
    }
}

fn sign(s: bool) -> &'static str {
    if s {
        "s"
    } else {
        "u"
    }
}

impl Display for Program {
    /// One line per instruction: code address, word, text.
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        let at = |t: Target| self.words.get(t as usize).map_or(!0, |w| w.0);
        for (n, (x, (a, w))) in self.code.iter().zip(&self.words).enumerate() {
            let mark = if n as u32 == self.entry { '>' } else { ' ' };
            writeln!(f, "{mark}{a:06x}: {w:016x}  {}", inst(x, &at))?;
        }
        Ok(())
    }
}
