//! Random blocks: `eval` against the backend (natively the AArch64 code). The tests run it, and
//! the browser build exports it for tools/wasm-simd-jit-check.mjs.
use crate::*;

/// A random generator (xorshift).
pub struct Rng(pub u64);

impl Rng {
    #[allow(clippy::should_implement_trait)]
    pub fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }
    pub fn below(&mut self, n: usize) -> usize {
        (self.next() % n as u64) as usize
    }
}

/// Words that make interesting operands: special floats, small integers, bytes.
pub fn word(r: &mut Rng) -> u32 {
    const SPECIAL: [u32; 30] = [
        0, 0x8000_0000, 0x3f80_0000, 0xbf80_0000, 0x7f80_0000, 0xff80_0000, 0x7fc0_0000, 0xffc0_0001, 0x7f80_0001, 0xff80_0001, 0x0000_0001, 0x8000_0001,
        0x007f_ffff, 0x0080_0000, 0x4b00_0000, 0x4f00_0000, 0xcf00_0000, 0x3f00_0000, 1, 2, 31, 32, 0xffff, 0x8000, 0x7fff_ffff, 0xffff_ffff, 0x477f_e000,
        0x3880_0000, 0x3380_0000, 0x0000_7c01,
    ];
    match r.next() % 5 {
        0 => SPECIAL[r.below(SPECIAL.len())],
        1 => ((r.next() % 2000) as f32 / 16.0 - 60.0).to_bits(),
        2 => (r.next() % 64) as u32,
        _ => r.next() as u32,
    }
}

const UNS: [Un; 20] = [
    Un::Not, Un::Neg, Un::Clz, Un::Ftz, Un::IToF, Un::UToF, Un::FToI, Un::FToU, Un::FSqrt, Un::FRound, Un::FTrunc, Un::FFloor, Un::FCeil, Un::Exp2, Un::Log2,
    Un::HalfToF, Un::FToHalf, Un::Shl(5), Un::ShrU(7), Un::ShrS(31),
];
const BINS: [Bin; 34] = [
    Bin::And, Bin::Or, Bin::Xor, Bin::Add, Bin::Sub, Bin::Mul, Bin::Shl, Bin::ShrU, Bin::ShrS, Bin::Ror, Bin::IMin, Bin::IMax, Bin::UMin, Bin::UMax, Bin::IEq,
    Bin::ILt, Bin::ULt, Bin::FAdd, Bin::FSub, Bin::FMul, Bin::FDiv, Bin::FMin, Bin::FMax, Bin::FMinNum, Bin::FMaxNum, Bin::FEq, Bin::FLt, Bin::FLe,
    Bin::FUnord, Bin::V8AddSat, Bin::V8SubSat, Bin::V8Min, Bin::V8Max, Bin::V8MulD,
];

fn sin_pi(x: u32) -> u32 {
    (f32::from_bits(x) * std::f32::consts::PI).sin().to_bits()
}

/// A random block of `n` operations over 12 registers, 3 flags, 4 arguments, 3 inputs and 2
/// outputs.
pub fn random_block(r: &mut Rng, n: usize) -> Block {
    let f = register(sin_pi);
    let mut b = Builder::new();
    let mut vecs: Vec<Id> = Vec::new();
    let mut masks: Vec<Id> = Vec::new();
    for _ in 0..n {
        let pick = |r: &mut Rng, v: &Vec<Id>| v[r.below(v.len())];
        match r.below(if vecs.len() < 2 { 4 } else { 24 }) {
            0 => vecs.push(b.reg(r.below(12) as u16)),
            1 => vecs.push(match r.below(5) {
                0 => b.arg(r.below(4) as u16),
                1 => b.input(r.below(3) as u16),
                2 => b.lane(),
                3 => b.vconst(std::array::from_fn(|_| word(r))),
                _ => b.imm(word(r)),
            }),
            2 => masks.push(b.flag(r.below(3) as u8)),
            3 => masks.push(b.simm(r.next() as u32 & 0xffff)),
            4..=9 => {
                let x = pick(r, &vecs);
                let op = if r.below(4) == 0 && r.below(3) == 0 { Un::Call(f) } else { UNS[r.below(UNS.len())] };
                let op = match op {
                    Un::Shl(_) => Un::Shl(r.below(32) as u8),
                    Un::ShrU(_) => Un::ShrU(r.below(32) as u8),
                    Un::ShrS(_) => Un::ShrS(r.below(32) as u8),
                    o => o,
                };
                vecs.push(b.un(op, x));
            }
            10..=16 => {
                let (x, y) = (pick(r, &vecs), pick(r, &vecs));
                vecs.push(b.bin(BINS[r.below(BINS.len())], x, y));
            }
            17 => {
                let (m, x, y) = (pick(r, &vecs), pick(r, &vecs), pick(r, &vecs));
                vecs.push(b.sel(m, x, y));
            }
            18 if !masks.is_empty() => {
                let m = pick(r, &masks);
                vecs.push(b.expand(m));
            }
            19 => {
                let x = pick(r, &vecs);
                masks.push(b.bits(x));
                if !masks.is_empty() && r.below(2) == 0 {
                    let (p, q) = (pick(r, &masks), pick(r, &masks));
                    masks.push(b.sop([SOp::And, SOp::Or, SOp::Xor][r.below(3)], p, q));
                }
            }
            20 if r.below(3) == 0 => {
                let x = pick(r, &vecs);
                let p: [u8; 16] = if r.below(2) == 0 { std::array::from_fn(|i| (i ^ 1) as u8) } else { std::array::from_fn(|_| r.below(16) as u8) };
                vecs.push(b.shuffle(x, p));
            }
            21 if !masks.is_empty() => {
                let (v, m) = (pick(r, &vecs), pick(r, &masks));
                b.store(r.below(12) as u16, v, m);
            }
            22 if !masks.is_empty() => {
                let (v, m) = (pick(r, &masks), pick(r, &masks));
                b.store_flag(r.below(3) as u8, v, m);
            }
            23 => {
                let v = pick(r, &vecs);
                if r.below(4) == 0 {
                    b.out(r.below(2) as u16, v);
                } else {
                    b.store_all(r.below(12) as u16, v);
                }
            }
            _ => {}
        }
    }
    // the last values somewhere
    for (k, &v) in vecs.iter().rev().take(3).enumerate() {
        b.store_all(k as u16, v);
    }
    if let Some(&m) = masks.last() {
        b.store_flag(0, m, m);
    }
    b.finish()
}

/// Runs `cases` random blocks with `eval` and with `Code::run`; the number of blocks that had
/// code, or what differed.
pub fn check(seed: u64, cases: usize) -> Result<usize, String> {
    let mut r = Rng(seed | 1);
    let mut compiled = 0;
    for case in 0..cases {
        let n = 4 + r.below(if case % 16 == 15 { 400 } else { 40 });
        let b = random_block(&mut r, n);
        validate(&b).map_err(|e| format!("case {case}: {e}"))?;
        let regs: Vec<V> = (0..12).map(|_| std::array::from_fn(|_| word(&mut r))).collect();
        let flags: Vec<u32> = (0..3).map(|_| r.next() as u32 & 0xffff).collect();
        let args: Vec<u32> = (0..4).map(|_| word(&mut r)).collect();
        let inputs: Vec<V> = (0..3).map(|_| std::array::from_fn(|_| word(&mut r))).collect();
        let (mut ra, mut fa, mut oa) = (regs.clone(), flags.clone(), vec![[0u32; 16]; 2]);
        let (mut rb, mut fb, mut ob) = (regs.clone(), flags.clone(), vec![[0u32; 16]; 2]);
        eval(&b, &mut ra, &mut fa, &args, &inputs, &mut oa);
        let code = Code::new(b);
        compiled += code.compiled() as usize;
        if available() && !code.compiled() {
            return Err(format!("case {case}: no code for\n{:?}", code.block.ir));
        }
        code.run(&mut rb, &mut fb, &args, &inputs, &mut ob);
        if ra != rb || fa != fb || oa != ob {
            let k = (0..12).find(|&k| ra[k] != rb[k]);
            return Err(format!(
                "case {case}: reg {k:?} eval {:08x?}\n code {:08x?}\n flags {fa:04x?} {fb:04x?}\n outputs same: {}\n{:#?}",
                k.map(|k| ra[k]),
                k.map(|k| rb[k]),
                oa == ob,
                code.block.ir.iter().enumerate().collect::<Vec<_>>()
            ));
        }
    }
    Ok(compiled)
}

