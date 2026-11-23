//! Random blocks against the interpreter: `check` runs them with the backend where there is
//! one (the browser build exports it for tools/wasm-shader-jit-check.mjs), else with `eval`.
use super::*;
use crate::exec::{run, Exit, Lanes};
use crate::host::Host;
use std::collections::BTreeMap;

pub struct Cb;

impl Host<LANES> for Cb {
    fn cbuf(&self, index: u32, offset: u32) -> u32 {
        word(index as u64 * 7919 + offset as u64)
    }
}

/// A random generator (xorshift).
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }
}

/// Words that make interesting operands: special floats, small integers, halves.
fn word(k: u64) -> u32 {
    const SPECIAL: [u32; 26] = [
        0, 0x8000_0000, 0x3f80_0000, 0xbf80_0000, 0x7f80_0000, 0xff80_0000, 0x7fc0_0000, 0xffc0_0001, 0x7f80_0001, 0xff80_0001, 0x0000_0001, 0x8000_0001,
        0x007f_ffff, 0x0080_0000, 0x4b00_0000, 0x4f00_0000, 0xcf00_0000, 0x3f00_0000, 1, 2, 31, 32, 0xffff, 0x8000, 0x7fff_ffff, 0xffff_ffff,
    ];
    let mut r = Rng(k.wrapping_mul(0x9e37_79b9_7f4a_7c15) | 1);
    r.next();
    match r.next() % 4 {
        0 => SPECIAL[(r.next() % SPECIAL.len() as u64) as usize],
        1 => ((r.next() % 2000) as f32 / 16.0 - 60.0).to_bits(),
        _ => r.next() as u32,
    }
}

/// Code with the scheduling words: `ins`, then EXIT.
fn assemble(ins: &[u64]) -> Vec<u8> {
    const EXIT: u64 = 0xe300_0000_0007_000f;
    let mut w = Vec::new();
    for (n, i) in ins.iter().chain([&EXIT]).enumerate() {
        if n % 3 == 0 {
            w.push(0x001f_8000_fc00_07e0);
        }
        w.push(*i);
    }
    w.iter().flat_map(|x| x.to_le_bytes()).collect()
}

fn kind(op: &Op) -> String {
    let name = format!("{op:?}");
    name[..name.find([' ', '(', '{']).unwrap_or(name.len())].to_string()
}

/// The top 16 bits of words that decode to each kind of instruction the JIT takes.
fn opcodes() -> Vec<Vec<u64>> {
    let mut r = Rng(0x0dd_ba11);
    let mut seen = std::collections::BTreeMap::<String, std::collections::BTreeSet<u64>>::new();
    for _ in 0..400_000 {
        let w = r.next();
        let inst = crate::decode::decode(8, w);
        if lower(&mut Lower::default(), &inst).is_some() && !matches!(inst.op, Op::Nop | Op::Fence) {
            seen.entry(kind(&inst.op)).or_default().insert(w >> 48);
        }
    }
    seen.into_values().map(|s| s.into_iter().collect()).collect()
}

/// A random instruction word that the JIT takes, of a random kind.
fn random_inst(r: &mut Rng, tops: &[Vec<u64>]) -> u64 {
    let top = &tops[(r.next() % tops.len() as u64) as usize];
    let top = top[(r.next() % top.len() as u64) as usize];
    loop {
        let mut w = r.next() & 0xffff_ffff_ffff | top << 48;
        // mostly registers below 24 (so that instructions use each other's results), always
        // predicated on P0..P3 or PT
        let fields = [(0u32, 8u32), (8, 8), (20, 8), (39, 8)];
        for (pos, len) in fields {
            if !r.next().is_multiple_of(8) {
                let v = r.next() % 24;
                w = w & !(((1u64 << len) - 1) << pos) | v << pos;
            }
        }
        // small immediates too (shift amounts below 32)
        if r.next().is_multiple_of(4) {
            w &= !(((1u64 << 14) - 1) << 25);
        }
        let p = [0u64, 1, 2, 3, 7, 7, 7, 7][(r.next() % 8) as usize] | r.next().is_multiple_of(4) as u64 * 8;
        w = w & !(0xf << 16) | p << 16;
        let inst = crate::decode::decode(8, w);
        if lower(&mut Lower::default(), &inst).is_some() && !matches!(inst.op, Op::Nop | Op::Fence) {
            return w;
        }
    }
}

/// A program of these instructions (then EXIT).
pub fn program(ins: &[u64]) -> Program {
    Program::decode(&assemble(ins)).unwrap()
}

/// An instruction the JIT leaves to the interpreter (POPC), to put between blocks.
fn separator(r: &mut Rng) -> u64 {
    loop {
        let w = r.next() & !(0xf << 16) | 7 << 16;
        let inst = crate::decode::decode(8, w);
        if kind(&inst.op) == "PopC" && lower(&mut Lower::default(), &inst).is_none() {
            return w;
        }
    }
}

/// Random registers and predicates.
fn randomize(l: &mut Lanes<LANES>, r: &mut Rng) {
    for k in 0..255 {
        for i in 0..LANES {
            l.gpr[k][i] = word(r.next());
        }
    }
    for k in 0..7 {
        l.pred[k] = r.next() as u32;
    }
}

/// Runs `cases` random blocks both ways; the kinds of instruction tried, or what differed.
/// Every fourth case is a program of several blocks run by `exec::run` with the JIT on and off
/// (on where there is a backend).
pub fn check(seed: u64, cases: usize) -> Result<BTreeMap<String, usize>, String> {
    let mut r = Rng(seed | 1);
    let tops = opcodes();
    let mut ops = BTreeMap::new();
    let was = ENABLED.load(std::sync::atomic::Ordering::Relaxed);
    let sep = separator(&mut r);
    for case in 0..cases {
        if case % 4 == 3 {
            several(&mut r, &tops, sep, case, was)?;
            continue;
        }
        let n = 2 + (r.next() % 8) as usize;
        let ins: Vec<u64> = (0..n).map(|_| random_inst(&mut r, &tops)).collect();
        let p = Program::decode(&assemble(&ins)).map_err(|e| format!("case {case}: {e}"))?;
        let code = Code::compile(&p).ok_or(format!("case {case}: no block"))?;
        if code.block_at(p.entry) != Some((0, n as u32)) {
            return Err(format!("case {case}: block {:?}", code.block_at(p.entry)));
        }
        for inst in &p.code[..n] {
            *ops.entry(kind(&inst.op)).or_insert(0) += 1;
        }
        let mut l = Lanes::<LANES>::new();
        let lanes = if r.next().is_multiple_of(3) { r.next() as u32 & 0xffff | 1 } else { 0xffff };
        l.start(&p, lanes, 0);
        randomize(&mut l, &mut r);
        let (mut gpr, mut pred) = (l.gpr.clone(), l.pred);
        let mut frame = Frame::default();
        code.fill(0, &mut frame, |i, o| Host::<LANES>::cbuf(&Cb, i, o));
        if !code.run(0, &mut gpr, &mut pred, &mut frame, lanes) {
            if cfg!(target_arch = "wasm32") {
                return Err(format!("case {case}: the compiled code did not run"));
            }
            eval(&code.blocks[0], &mut gpr, &mut pred, &frame.cb, lanes);
        }
        // the interpreter alone
        set_enabled(false);
        let mut fuel = 1000;
        let exit = run(&p, &mut l, &mut Cb, &mut fuel);
        set_enabled(was);
        if exit != Exit::Done {
            return Err(format!("case {case}: {exit:?}"));
        }
        let text = || p.code[..n].iter().map(|i| format!("{i:?}")).collect::<Vec<_>>().join("\n");
        for k in 0..256 {
            if gpr[k] != l.gpr[k] {
                return Err(format!("case {case}: R{k} {:08x?}\n    interpreter {:08x?}\n{}", gpr[k], l.gpr[k], text()));
            }
        }
        // (bits of lanes that do not exist are left as they were by both)
        if pred.map(|p| p & 0xffff) != l.pred.map(|p| p & 0xffff) {
            return Err(format!("case {case}: predicates {pred:08x?}\n    interpreter {:08x?}\n{}", l.pred, text()));
        }
    }
    Ok(ops)
}

/// A program of two to four blocks with POPC between them, run whole with the JIT and without.
fn several(r: &mut Rng, tops: &[Vec<u64>], sep: u64, case: usize, was: bool) -> Result<(), String> {
    let mut ins = Vec::new();
    for k in 0..2 + r.next() % 3 {
        if k > 0 {
            ins.push(sep);
        }
        for _ in 0..2 + r.next() % 6 {
            ins.push(random_inst(r, tops));
        }
    }
    let p = Program::decode(&assemble(&ins)).map_err(|e| format!("case {case}: {e}"))?;
    let mut a = Lanes::<LANES>::new();
    let lanes = if r.next().is_multiple_of(3) { r.next() as u32 & 0xffff | 1 } else { 0xffff };
    a.start(&p, lanes, 0);
    randomize(&mut a, r);
    let mut b = a.clone();
    let before = runs();
    set_enabled(true);
    let (mut f1, mut f2) = (1000, 1000);
    let e1 = run(&p, &mut a, &mut Cb, &mut f1);
    set_enabled(false);
    let e2 = run(&p, &mut b, &mut Cb, &mut f2);
    set_enabled(was);
    if cfg!(target_arch = "wasm32") && runs() == before {
        return Err(format!("case {case}: no compiled block ran"));
    }
    let text = || p.code.iter().map(|i| format!("{i:?}")).collect::<Vec<_>>().join("\n");
    if (e1, f1) != (e2, f2) {
        return Err(format!("case {case}: {e1:?} fuel {f1}, interpreter {e2:?} fuel {f2}\n{}", text()));
    }
    for k in 0..256 {
        if a.gpr[k] != b.gpr[k] {
            return Err(format!("case {case}: R{k} {:08x?}\n    interpreter {:08x?}\n{}", a.gpr[k], b.gpr[k], text()));
        }
    }
    if a.pred.map(|p| p & 0xffff) != b.pred.map(|p| p & 0xffff) {
        return Err(format!("case {case}: predicates {:08x?}\n    interpreter {:08x?}\n{}", a.pred, b.pred, text()));
    }
    Ok(())
}
