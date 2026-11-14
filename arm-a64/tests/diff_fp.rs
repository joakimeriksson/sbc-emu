//! Floating-point differential test against the host CPU: every snippet runs natively and on the
//! emulator under a random FPCR (all four rounding modes, flush-to-zero, default NaN) from the
//! same register state, with operands biased towards the hard cases (subnormals, overflow,
//! ties, NaNs, infinities, zeros, integers). v0..v7, x0..x7 and the whole FPSR must match.
#![cfg(all(target_arch = "aarch64", target_os = "macos"))]
mod common;
use arm_a64::Cpu;
use emu_core::{Bus, Core, FlatRam, Trap};
use std::fmt::Write as _;
use std::process::Command;

const SNIPPETS: &[&str] = &[
    // scalar arithmetic
    "fadd s0, s1, s2\n fadd d3, d4, d5",
    "fsub s0, s1, s2\n fsub d3, d4, d5",
    "fmul s0, s1, s2\n fmul d3, d4, d5",
    "fdiv s0, s1, s2\n fdiv d3, d4, d5",
    "fsqrt s0, s1\n fsqrt d3, d4",
    "fmadd s0, s1, s2, s3\n fmadd d4, d5, d6, d7",
    "fmsub s0, s1, s2, s3\n fnmadd d4, d5, d6, d7",
    "fnmsub s0, s1, s2, s3\n fnmul d4, d5, d6",
    "fmax s0, s1, s2\n fmin d3, d4, d5",
    "fmaxnm s0, s1, s2\n fminnm d3, d4, d5",
    "fcmp s1, s2\n cset x0, mi\n cset x1, vs\n fcmpe d3, d4\n cset x2, gt\n cset x3, vs",
    "fcmp s1, #0.0\n cset x0, eq\n fcmpe d3, #0.0\n cset x1, ge\n fccmpe s5, s6, #4, ne\n cset x2, ls",
    // precision conversions
    "fcvt d0, s1\n fcvt s2, d3",
    "fcvt h0, s1\n fcvt h2, d3",
    "fcvt s0, h1\n fcvt d2, h3",
    // round to integral
    "frintn s0, s1\n frintp d2, d3\n frintm s4, s5",
    "frintz d0, d1\n frinta s2, s3\n frintx d4, d5\n frinti s6, s7",
    // to and from integers
    "fcvtzs w0, s1\n fcvtzu x1, d2\n fcvtzs x2, s3\n fcvtzu w3, d4",
    "fcvtns w0, d1\n fcvtnu x1, s2\n fcvtms x2, d3\n fcvtmu w3, s4",
    "fcvtps w0, s1\n fcvtpu x1, d2\n fcvtas x2, s3\n fcvtau w3, d4",
    "scvtf s0, w1\n ucvtf d2, x3\n scvtf d4, x5\n ucvtf s6, w7",
    "scvtf s0, x1\n ucvtf s2, x3\n scvtf d4, w5\n ucvtf d6, w7",
    "scvtf d0, x1, #10\n ucvtf s2, w3, #3\n fcvtzs x4, d5, #20\n fcvtzu w5, s6, #7",
    // vector arithmetic
    "fadd v0.4s, v1.4s, v2.4s\n fsub v3.2d, v4.2d, v5.2d",
    "fmul v0.4s, v1.4s, v2.4s\n fdiv v3.2d, v4.2d, v5.2d",
    "fmulx v0.4s, v1.4s, v2.4s\n fmulx d3, d4, d5",
    "fmla v0.4s, v1.4s, v2.4s\n fmls v3.2d, v4.2d, v5.2d",
    "fmax v0.4s, v1.4s, v2.4s\n fminnm v3.2d, v4.2d, v5.2d\n fabd v6.4s, v7.4s, v1.4s",
    "faddp v0.4s, v1.4s, v2.4s\n fmaxp v3.2d, v4.2d, v5.2d\n fminnmp v6.4s, v7.4s, v1.4s",
    "fcmeq v0.4s, v1.4s, v2.4s\n fcmge v3.2d, v4.2d, v5.2d\n fcmgt v6.4s, v7.4s, v1.4s",
    "facge v0.4s, v1.4s, v2.4s\n facgt v3.2d, v4.2d, v5.2d",
    "fcmeq v0.4s, v1.4s, #0.0\n fcmge v2.2d, v3.2d, #0.0\n fcmle v4.4s, v5.4s, #0.0\n fcmlt v6.2d, v7.2d, #0.0",
    "fcmgt v0.4s, v1.4s, #0.0",
    "fsqrt v0.4s, v1.4s\n fsqrt v2.2d, v3.2d",
    "frecpe v0.4s, v1.4s\n frecpe v2.2d, v3.2d",
    "frsqrte v0.4s, v1.4s\n frsqrte v2.2d, v3.2d",
    "frecps v0.4s, v1.4s, v2.4s\n frsqrts v3.2d, v4.2d, v5.2d",
    "frintn v0.4s, v1.4s\n frintx v2.2d, v3.2d\n frinti v4.4s, v5.4s\n frinta v6.2d, v7.2d",
    "fcvtzs v0.4s, v1.4s\n fcvtzu v2.2d, v3.2d\n fcvtns v4.4s, v5.4s\n fcvtau v6.2d, v7.2d",
    "fcvtms v0.4s, v1.4s\n fcvtpu v2.2d, v3.2d",
    "scvtf v0.4s, v1.4s\n ucvtf v2.2d, v3.2d\n scvtf v4.4s, v5.4s, #12\n fcvtzu v6.2d, v7.2d, #40",
    "fcvtn v0.2s, v1.2d\n fcvtn2 v0.4s, v2.2d\n fcvtl v3.2d, v4.2s\n fcvtl2 v5.2d, v6.4s",
    "fcvtn v0.4h, v1.4s\n fcvtn2 v0.8h, v2.4s\n fcvtl v3.4s, v4.4h\n fcvtl2 v5.4s, v6.8h",
    "fcvtxn v0.2s, v1.2d\n fcvtxn2 v0.4s, v2.2d\n fcvtxn s3, d4",
    "fmaxv s0, v1.4s\n fminnmv s2, v3.4s\n faddp s4, v5.2s\n fmaxnmp d6, v7.2d",
    "fmul v0.4s, v1.4s, v2.s[1]\n fmla v3.2d, v4.2d, v5.d[1]\n fmulx v6.4s, v7.4s, v1.s[3]",
];

fn wrapper(name: &str, body: &str) -> String {
    let mut s = format!(".p2align 2\n.globl {name}\n{name}:\n stp x29, x30, [sp, #-16]!\n mov x17, x0\n msr fpsr, xzr\n ldr x16, [x17, #200]\n msr fpcr, x16\n");
    for i in (0..8).step_by(2) { writeln!(s, " ldp q{}, q{}, [x17, #{}]", i, i + 1, 64 + i * 16).unwrap(); }
    for i in (0..8).step_by(2) { writeln!(s, " ldp x{}, x{}, [x17, #{}]", i, i + 1, i * 8).unwrap(); }
    writeln!(s, "{body}").unwrap();
    for i in (0..8).step_by(2) { writeln!(s, " stp x{}, x{}, [x17, #{}]", i, i + 1, i * 8).unwrap(); }
    for i in (0..8).step_by(2) { writeln!(s, " stp q{}, q{}, [x17, #{}]", i, i + 1, 64 + i * 16).unwrap(); }
    s.push_str(" mrs x16, fpsr\n str x16, [x17, #192]\n msr fpcr, xzr\n ldp x29, x30, [sp], #16\n ret\n");
    s
}

/// x0..x7, v0..v7 (2 words each), FPSR (out), FPCR (in)
const WORDS: usize = 8 + 16 + 2;

struct Rng(u64);
impl Rng {
    fn next(&mut self) -> u64 { let x = &mut self.0; *x ^= *x >> 12; *x ^= *x << 25; *x ^= *x >> 27; x.wrapping_mul(0x2545_f491_4f6c_dd1d) }
}

/// A float bit pattern of `es` bits (32 or 64) from the interesting classes.
fn fp_value(r: &mut Rng, es: u32) -> u64 {
    let (mb, eb) = if es == 32 { (23, 8) } else { (52, 11) };
    let emax = (1u64 << eb) - 1;
    let bias = emax >> 1;
    let sign = (r.next() & 1) << (es - 1);
    let frac = r.next() & ((1 << mb) - 1);
    let exp = match r.next() % 12 {
        0 => 0,                                // zero / subnormal
        1 => emax,                             // inf / NaN
        2 => 1 + r.next() % 3,                 // just above the subnormals
        3 => emax - 1 - r.next() % 3,          // near overflow
        4 => bias + r.next() % 3,              // 1 .. 8 (ties, integers)
        5 => bias - 1 - r.next() % 3,          // fractions
        6 => bias + (r.next() % 66),           // integers and values near the integer limits
        _ => r.next() % emax,
    };
    let frac = match r.next() % 6 {
        0 => 0,
        1 => frac & !((1 << (mb / 2)) - 1),     // few low bits (exact operations, ties)
        2 if exp == emax => 1 << (mb - 1),     // a quiet NaN with no payload
        3 if exp == emax => frac | 1,          // a NaN with a payload
        _ => frac,
    };
    sign | (exp << mb) | frac
}

fn inputs(seed: u64) -> [u64; WORDS] {
    let mut r = Rng(seed.wrapping_mul(0x9e37_79b9_7f4a_7c15) | 1);
    let mut s = [0u64; WORDS];
    for (i, v) in s.iter_mut().enumerate().take(24) {
        *v = match r.next() % 8 {
            // two single-precision lanes or one double
            0..=2 => fp_value(&mut r, 32) | (fp_value(&mut r, 32) << 32),
            3..=5 => fp_value(&mut r, 64),
            // integers for the conversions (some small, some huge)
            6 => if i < 8 { r.next() >> (r.next() % 64) } else { r.next() },
            _ => (fp_value(&mut r, 32) & 0xffff) | (fp_value(&mut r, 32) & 0xffff) << 16 | (fp_value(&mut r, 64) & 0xffff_ffff) << 32,
        };
    }
    // FPCR: RMode (23:22), FZ (24), DN (25)
    let m = r.next();
    s[WORDS - 1] = ((m & 3) << 22) | (((m >> 2) & 1) << 24) | (((m >> 3) & 1) << 25);
    s[WORDS - 2] = 0;
    s
}

const SEEDS: u64 = 400;

#[test]
fn fp_snippets_match_host() {
    let dir = std::env::temp_dir().join(format!("arm-a64-fp-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let mut asm = String::from(".text\n");
    for (k, b) in SNIPPETS.iter().enumerate() { asm += &wrapper(&format!("_snip{k}"), b); }
    let mut c = format!("#include <stdio.h>\n#include <stdint.h>\n#define W {WORDS}\n");
    for k in 0..SNIPPETS.len() { writeln!(c, "void snip{k}(uint64_t*);").unwrap(); }
    write!(c, "typedef void (*fn)(uint64_t*);\nstatic fn F[] = {{").unwrap();
    for k in 0..SNIPPETS.len() { write!(c, "snip{k},").unwrap(); }
    c += "};\nint main(void){ uint64_t s[W]; int k; while (scanf(\"%d\", &k) == 1) { for (int i = 0; i < W; i++) scanf(\"%llx\", (unsigned long long*)&s[i]); F[k](s); for (int i = 0; i < W; i++) printf(\"%llx \", (unsigned long long)s[i]); printf(\"\\n\"); } return 0; }\n";
    std::fs::write(dir.join("s.s"), &asm).unwrap();
    std::fs::write(dir.join("m.c"), &c).unwrap();
    let out = Command::new("clang").current_dir(&dir).args(["-O1", "m.c", "s.s", "-o", "host"]).output().unwrap();
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    let mut input = String::new();
    let mut cases = Vec::new();
    for k in 0..SNIPPETS.len() {
        for seed in 0..SEEDS {
            let s = inputs(seed * 7919 + k as u64 * 104_729);
            write!(input, "{k}").unwrap();
            for v in s { write!(input, " {v:x}").unwrap(); }
            input.push('\n');
            cases.push(s);
        }
    }
    std::fs::write(dir.join("in.txt"), &input).unwrap();
    let out = Command::new("sh").current_dir(&dir).arg("-c").arg("./host < in.txt").output().unwrap();
    assert!(out.status.success());
    let host: Vec<Vec<u64>> = String::from_utf8(out.stdout).unwrap().lines()
        .map(|l| l.split_whitespace().map(|h| u64::from_str_radix(h, 16).unwrap()).collect()).collect();
    let mut failures = 0;
    let mut failing = std::collections::BTreeMap::new();
    for (k, b) in SNIPPETS.iter().enumerate() {
        let code = common::assemble(&format!("{}\nhalt: brk #0\n", wrapper("snip", b)));
        let halt = common::LOAD + code.len() as u64 - 4;
        for seed in 0..SEEDS as usize {
            let idx = k * SEEDS as usize + seed;
            let s = cases[idx];
            let mut ram = FlatRam::new(0, 1 << 22);
            ram.load(common::LOAD, &code).unwrap();
            let state = 0x20_0000u64;
            for (i, v) in s.iter().enumerate() { ram.write64(state + 8 * i as u64, *v).unwrap(); }
            let mut cpu = Cpu::new();
            cpu.brk_to_host = true;
            cpu.enable_test_jit_from_env();
            cpu.ram = ram.mem.as_mut_ptr();
            cpu.ram_len = ram.mem.len() as u64;
            cpu.el = 0;
            cpu.pc = common::LOAD;
            cpu.sp[0] = 0x30_0000;
            cpu.sys.cpacr_el1 = 3 << 20;
            cpu.x[0] = state;
            cpu.x[30] = halt;
            let mut trap = None;
            for _ in 0..10_000 { if let Err(t) = cpu.step(&mut ram) { trap = Some(t); break; } }
            if trap != Some(Trap::Brk(0)) {
                failures += 1;
                if failing.insert(k, 1).is_none() { eprintln!("snippet {k}: stopped with {trap:?} at {:#x}: `{}`", cpu.pc, b.replace('\n', ";")); }
                continue;
            }
            let got: Vec<u64> = (0..WORDS as u64).map(|i| ram.read64(state + 8 * i).unwrap()).collect();
            if got != host[idx] {
                failures += 1;
                let n = failing.entry(k).or_insert(0);
                *n += 1;
                if *n <= 3 {
                    eprintln!("snippet {k} seed {seed} fpcr {:#x}: `{}`", s[WORDS - 1], b.replace('\n', ";"));
                    for i in 0..WORDS {
                        if got[i] != host[idx][i] {
                            let name = if i < 8 { format!("x{i}") } else if i < 24 { format!("v{}.{}", (i - 8) / 2, if i % 2 == 0 { "lo" } else { "hi" }) } else { "fpsr".into() };
                            eprintln!("  {name}: in {:#018x} emu {:#018x} host {:#018x}", if i < 24 { s[i] } else { 0 }, got[i], host[idx][i]);
                        }
                    }
                }
            }
        }
    }
    let _ = std::fs::remove_dir_all(&dir);
    assert_eq!(failures, 0, "{failures} mismatching cases (snippet: count) {failing:?}");
}
