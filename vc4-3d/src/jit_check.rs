//! Random programs against the interpreter: QPU instructions the JIT takes (and some it does
//! not, between them), run from the same random state with the JIT and without. The tests run
//! it natively; the browser build exports it for tools/wasm-vc4-jit-check.mjs.

use crate::asm::{bytes, end, I};
use crate::mem::Mem;
use crate::qpu::{decode, sig, Code, Env, Qpu, Stop, V};
use std::collections::BTreeMap;

struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }
    fn below(&mut self, n: u64) -> u64 {
        self.next() % n
    }
    fn pick<T: Copy>(&mut self, v: &[T]) -> T {
        v[self.below(v.len() as u64) as usize]
    }
}

/// Words that make interesting operands: special floats, halves, bytes, small integers.
fn word(r: &mut Rng) -> u32 {
    const SPECIAL: [u32; 24] = [
        0, 0x8000_0000, 0x3f80_0000, 0xbf80_0000, 0x7f80_0000, 0xff80_0000, 0x7fc0_0000, 0xffc0_0001, 0x7f80_0001, 0x0000_0001, 0x8000_0001, 0x007f_ffff,
        0x0080_0000, 0x4b00_0000, 0x3f00_0000, 0x3b80_0000, 1, 31, 0xffff, 0x8000, 0x7fff_ffff, 0xffff_ffff, 0x3c00_7c01, 0x0080_ff7f,
    ];
    match r.below(5) {
        0 => r.pick(&SPECIAL),
        1 => ((r.below(4000) as f32) / 64.0 - 30.0).to_bits(),
        2 => r.below(300) as u32,
        _ => r.next() as u32,
    }
}

fn vector(r: &mut Rng) -> V {
    std::array::from_fn(|_| word(r))
}

/// An environment whose every answer is the next of a sequence (so answers in the wrong order
/// show), with a VPM.
struct SeqEnv {
    seed: u64,
    n: u64,
    vpm: Vec<u32>,
}

impl SeqEnv {
    fn next(&mut self) -> V {
        self.n += 1;
        let mut r = Rng(self.seed ^ self.n.wrapping_mul(0x9e37_79b9_7f4a_7c15) | 1);
        vector(&mut r)
    }
}

impl Env for SeqEnv {
    fn vpm(&mut self) -> (&mut [u32], usize, usize) {
        (&mut self.vpm, 0, 64)
    }
    fn vary(&mut self) -> (V, V) {
        (self.next(), self.next())
    }
    fn pixel_xy(&self) -> (V, V) {
        (std::array::from_fn(|i| i as u32 * 3), std::array::from_fn(|i| 100 - i as u32))
    }
    fn ms_flags(&self) -> V {
        std::array::from_fn(|i| (i as u32 * 5) & 15)
    }
    fn rev_flag(&self) -> V {
        [1; 16]
    }
    fn color_load(&mut self) -> V {
        self.next()
    }
    fn coverage_load(&mut self) -> V {
        self.next()
    }
    fn alpha_mask_load(&mut self) -> V {
        self.next()
    }
    fn tlb_z(&mut self, v: &V, mask: u16) {
        self.n = self.n.wrapping_add(v[0] as u64 ^ mask as u64);
    }
}

const SIGS: [u8; 12] = [
    sig::NONE, sig::NONE, sig::NONE, sig::SMALL_IMM, sig::SMALL_IMM, sig::LOAD_IMM, sig::THRSW, sig::LOAD_TMU0, sig::LOAD_TMU1, sig::COLOR_LOAD, sig::COVERAGE_LOAD,
    sig::ALPHA_MASK_LOAD,
];

/// A random instruction word the JIT takes.
fn random_inst(r: &mut Rng) -> u64 {
    use crate::qpu::{raddr, waddr};
    let waddrs = |r: &mut Rng| -> u64 {
        match r.below(10) {
            0..=4 => r.below(32),
            5 | 6 => 32 + r.below(4),
            7 => waddr::NOP as u64,
            8 => 52 + r.below(4),
            _ => r.pick(&[waddr::TMU_NOSWAP, waddr::QUAD_XY, waddr::MS_FLAGS, waddr::R0]) as u64,
        }
    };
    let raddrs = |r: &mut Rng| -> u64 {
        match r.below(10) {
            0..=5 => r.below(32),
            _ => r.pick(&[raddr::UNIF, raddr::VARY, raddr::ELEM_QPU, raddr::NOP, raddr::XY_PIXEL, raddr::MS_REV, raddr::VPM_BUSY, 36]) as u64,
        }
    };
    loop {
        let s = r.pick(&SIGS) as u64;
        let mut w = s << 60;
        let small_pack = |r: &mut Rng| if r.below(3) == 0 { r.below(16) } else { 0 };
        if s == sig::LOAD_IMM as u64 {
            let mode = r.pick(&[0u64, 0, 1, 3, 2]);
            w |= mode << 57 | (r.below(2)) << 56 | small_pack(r) << 52;
            w |= r.below(8) << 49 | r.below(8) << 46 | ((r.below(3) == 0) as u64) << 45 | r.below(2) << 44;
            w |= waddrs(r) << 38 | waddrs(r) << 32 | word(r) as u64;
        } else {
            let unpack = if r.below(3) == 0 { r.below(8) } else { 0 };
            w |= unpack << 57 | r.below(2) << 56 | small_pack(r) << 52;
            w |= r.pick(&[1u64, 1, 1, 0, 2, 3, 4, 5, 6, 7]) << 49 | r.pick(&[1u64, 1, 1, 0, 2, 3, 4, 5, 6, 7]) << 46;
            w |= ((r.below(3) == 0) as u64) << 45 | r.below(2) << 44;
            w |= waddrs(r) << 38 | waddrs(r) << 32;
            w |= r.below(8) << 29 | r.below(32) << 24;
            w |= raddrs(r) << 18 | if s == sig::SMALL_IMM as u64 { r.below(64) } else { raddrs(r) } << 12;
            w |= r.below(8) << 9 | r.below(8) << 6 | r.below(8) << 3 | r.below(8);
        }
        if crate::jit::lower(&[w]).0 == 1 {
            return w;
        }
    }
}

/// An instruction the JIT leaves to the interpreter (a write to the TLB's Z), to put between
/// blocks.
fn separator() -> u64 {
    crate::asm::nop().add(crate::qpu::op::OR, crate::qpu::waddr::TLB_Z, crate::asm::R1, crate::asm::R1).0
}

fn kind(w: u64) -> String {
    let i = decode(w);
    match i.kind {
        crate::qpu::Kind::Ldi(m) => format!("ldi{m}"),
        _ => {
            let mut s = format!("add{}", i.op_add);
            if i.op_mul != 0 {
                s = format!("mul{}", i.op_mul);
            }
            s
        }
    }
}

/// Runs `cases` random programs both ways; the operations tried, or what differed.
pub fn check(seed: u64, cases: usize) -> Result<BTreeMap<String, usize>, String> {
    let _switch = crate::jit::SWITCH.lock().unwrap_or_else(|e| e.into_inner());
    let was = crate::jit::enabled();
    let mut r = Rng(seed | 1);
    let mut ops = BTreeMap::new();
    let mut ram = vec![0u8; 0x2_0000];
    for case in 0..cases {
        let mut prog: Vec<I> = Vec::new();
        for k in 0..1 + r.below(4) {
            if k > 0 {
                prog.push(I(separator()));
            }
            for _ in 0..2 + r.below(if case % 8 == 7 { 60 } else { 10 }) {
                let w = random_inst(&mut r);
                *ops.entry(kind(w)).or_insert(0) += 1;
                prog.push(I(w));
            }
        }
        prog.extend_from_slice(&end());
        let b = bytes(&prog);
        ram[0x1000..0x1000 + b.len()].copy_from_slice(&b);
        for k in 0..256 {
            let u = word(&mut r);
            ram[0x8000 + 4 * k..0x8000 + 4 * k + 4].copy_from_slice(&u.to_le_bytes());
        }
        let mut q0 = Qpu::new();
        for k in 0..32 {
            q0.ra[k] = vector(&mut r);
            q0.rb[k] = vector(&mut r);
        }
        for k in 0..6 {
            q0.acc[k] = vector(&mut r);
        }
        (q0.z, q0.n, q0.c, q0.num) = (r.next() as u16, r.next() as u16, r.next() as u16, r.below(12) as u32);
        q0.start(0x1000, 0x8000);
        for t in 0..2 {
            for _ in 0..r.below(4) {
                q0.tmu[t].fifo.push_back(vector(&mut r));
            }
        }
        let env_seed = r.next();
        let mut out = Vec::new();
        for on in [true, false] {
            crate::jit::set_enabled(on);
            let mut q = q0.clone();
            let mut env = SeqEnv { seed: env_seed, n: 0, vpm: vec![0; 128 * 16] };
            let mut code = Code::default();
            let mut m = Mem::new(&mut ram);
            let stop = q.run(&mut code, &mut m, &mut env, 100_000);
            if on && simd_jit::available() && code.jit.runs() == 0 {
                return Err(format!("case {case}: no block ran"));
            }
            code.jit.clear();
            out.push((q, env.n, stop));
        }
        crate::jit::set_enabled(was);
        let ((a, na, sa), (b, nb, sb)) = (&out[0], &out[1]);
        let text = || prog.iter().map(|i| format!("{:016x} {}", i.0, crate::disasm::disasm(i.0))).collect::<Vec<_>>().join("\n");
        if *sa != Stop::End || *sb != Stop::End {
            return Err(format!("case {case}: stopped {sa:?} / {sb:?}\n{}", text()));
        }
        for k in 0..32 {
            for (name, x, y) in [("ra", a.ra[k], b.ra[k]), ("rb", a.rb[k], b.rb[k])] {
                if x != y {
                    return Err(format!("case {case}: {name}{k} jit {x:08x?}\n   interpreter {y:08x?}\n{}", text()));
                }
            }
        }
        for k in 0..6 {
            if a.acc[k] != b.acc[k] {
                return Err(format!("case {case}: r{k} jit {:08x?}\n   interpreter {:08x?}\n{}", a.acc[k], b.acc[k], text()));
            }
        }
        if (a.z, a.n, a.c) != (b.z, b.n, b.c) {
            return Err(format!("case {case}: flags z n c jit {:04x} {:04x} {:04x}, interpreter {:04x} {:04x} {:04x}\n{}", a.z, a.n, a.c, b.z, b.n, b.c, text()));
        }
        if (a.unif, a.insns, a.pc, na, a.tmu[0].fifo.len(), a.tmu[1].fifo.len()) != (b.unif, b.insns, b.pc, nb, b.tmu[0].fifo.len(), b.tmu[1].fifo.len()) {
            return Err(format!("case {case}: uniforms, instructions, pc, fetches or TMU results differ\n{}", text()));
        }
    }
    Ok(ops)
}
