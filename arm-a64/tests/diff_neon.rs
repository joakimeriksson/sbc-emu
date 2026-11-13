//! NEON / FP differential test against the host CPU: v0..v7, x0..x7 and FPSR after each snippet
//! must match the native result for random inputs (including edge-value lanes).
#![cfg(all(target_arch = "aarch64", target_os = "macos"))]
mod common;
use arm_a64::Cpu;
use emu_core::{Bus, Core, FlatRam, Trap};
use std::fmt::Write as _;
use std::process::Command;

const SNIPPETS: &[&str] = &[
    // three same, integer
    "add v0.16b, v1.16b, v2.16b\n sub v3.8h, v4.8h, v5.8h\n mul v6.4s, v7.4s, v1.4s\n mla v2.8h, v3.8h, v4.8h",
    "sqadd v0.16b, v1.16b, v2.16b\n uqadd v3.8h, v4.8h, v5.8h\n sqsub v6.4s, v7.4s, v1.4s\n uqsub v2.2d, v3.2d, v4.2d",
    "shadd v0.16b, v1.16b, v2.16b\n uhadd v3.8h, v4.8h, v5.8h\n srhadd v6.4s, v7.4s, v1.4s\n urhadd v2.8b, v3.8b, v4.8b\n uhsub v5.4h, v6.4h, v7.4h",
    "cmgt v0.16b, v1.16b, v2.16b\n cmhi v3.8h, v4.8h, v5.8h\n cmge v6.4s, v7.4s, v1.4s\n cmhs v2.2d, v3.2d, v4.2d\n cmeq v5.16b, v6.16b, v7.16b\n cmtst v1.8h, v2.8h, v3.8h",
    "smax v0.16b, v1.16b, v2.16b\n umin v3.8h, v4.8h, v5.8h\n sabd v6.4s, v7.4s, v1.4s\n uaba v2.16b, v3.16b, v4.16b",
    "smaxp v0.16b, v1.16b, v2.16b\n uminp v3.8h, v4.8h, v5.8h\n addp v6.4s, v7.4s, v1.4s\n addp v2.2d, v3.2d, v4.2d\n umaxp v5.8b, v6.8b, v7.8b",
    "sshl v0.16b, v1.16b, v2.16b\n ushl v3.8h, v4.8h, v5.8h\n srshl v6.4s, v7.4s, v1.4s\n urshl v2.2d, v3.2d, v4.2d",
    "sqshl v0.16b, v1.16b, v2.16b\n uqshl v3.8h, v4.8h, v5.8h\n sqrshl v6.4s, v7.4s, v1.4s",
    "sqdmulh v0.8h, v1.8h, v2.8h\n sqrdmulh v3.4s, v4.4s, v5.4s\n pmul v6.16b, v7.16b, v1.16b",
    "and v0.16b, v1.16b, v2.16b\n bic v3.16b, v4.16b, v5.16b\n orn v6.8b, v7.8b, v1.8b\n bsl v2.16b, v3.16b, v4.16b\n bit v5.16b, v6.16b, v7.16b\n bif v1.16b, v2.16b, v3.16b",
    // three different
    "saddl v0.8h, v1.8b, v2.8b\n uaddl2 v3.4s, v4.8h, v5.8h\n ssubw v6.2d, v7.2d, v1.2s\n usubl v2.8h, v3.8b, v4.8b",
    "umull v0.8h, v1.8b, v2.8b\n smull2 v3.4s, v4.8h, v5.8h\n umlal v6.2d, v7.2s, v1.2s\n smlsl2 v2.8h, v3.16b, v4.16b",
    "addhn v0.8b, v1.8h, v2.8h\n raddhn2 v0.16b, v3.8h, v4.8h\n subhn v5.4h, v6.4s, v7.4s\n rsubhn v1.2s, v2.2d, v3.2d",
    "sabal v0.8h, v1.8b, v2.8b\n uabdl2 v3.4s, v4.8h, v5.8h\n sqdmull v6.4s, v7.4h, v1.4h\n sqdmlal2 v2.2d, v3.4s, v4.4s\n pmull v5.8h, v6.8b, v7.8b",
    // two-register misc
    "rev64 v0.16b, v1.16b\n rev32 v2.8h, v3.8h\n rev16 v4.16b, v5.16b\n cnt v6.8b, v7.8b\n not v1.16b, v2.16b\n rbit v3.16b, v4.16b",
    "cls v0.16b, v1.16b\n clz v2.8h, v3.8h\n clz v4.4s, v5.4s\n abs v6.2d, v7.2d\n neg v1.4s, v2.4s\n sqabs v3.8h, v4.8h\n sqneg v5.16b, v6.16b",
    "cmgt v0.16b, v1.16b, #0\n cmge v2.8h, v3.8h, #0\n cmeq v4.4s, v5.4s, #0\n cmle v6.2d, v7.2d, #0\n cmlt v1.16b, v2.16b, #0",
    "xtn v0.8b, v1.8h\n xtn2 v0.16b, v2.8h\n sqxtn v3.4h, v4.4s\n uqxtn2 v3.8h, v5.4s\n sqxtun v6.2s, v7.2d\n shll v1.8h, v2.8b, #8",
    "saddlp v0.8h, v1.16b\n uaddlp v2.4s, v3.8h\n sadalp v4.2d, v5.4s\n uadalp v6.8h, v7.16b",
    // across lanes
    "addv b0, v1.16b\n uaddlv h2, v3.16b\n saddlv s4, v5.8h\n umaxv h6, v7.8h\n sminv b1, v2.8b\n smaxv s3, v4.4s\n uminv b5, v6.16b",
    // copy, permute, table, ext
    "dup v0.16b, v1.b[5]\n dup v2.8h, w3\n ins v4.s[2], w5\n mov v6.d[1], v7.d[0]\n umov w1, v2.h[3]\n smov x3, v4.b[9]\n mov d5, v6.d[1]",
    "zip1 v0.16b, v1.16b, v2.16b\n zip2 v3.8h, v4.8h, v5.8h\n uzp1 v6.4s, v7.4s, v1.4s\n uzp2 v2.16b, v3.16b, v4.16b\n trn1 v5.8h, v6.8h, v7.8h\n trn2 v1.2d, v2.2d, v3.2d",
    "tbl v0.16b, {v1.16b, v2.16b}, v3.16b\n tbx v4.8b, {v5.16b}, v6.8b\n ext v7.16b, v1.16b, v2.16b, #5\n ext v3.8b, v4.8b, v5.8b, #3",
    // shift by immediate
    "sshr v0.16b, v1.16b, #3\n ushr v2.8h, v3.8h, #15\n srshr v4.4s, v5.4s, #7\n ursra v6.2d, v7.2d, #33\n ssra v1.8b, v2.8b, #8",
    "shl v0.16b, v1.16b, #3\n sli v2.8h, v3.8h, #5\n sri v4.4s, v5.4s, #9\n sqshl v6.2d, v7.2d, #17\n uqshl v1.16b, v2.16b, #2\n sqshlu v3.8h, v4.8h, #4",
    "shrn v0.8b, v1.8h, #3\n rshrn2 v0.16b, v2.8h, #5\n sqshrn v3.4h, v4.4s, #9\n uqrshrn2 v3.8h, v5.4s, #1\n sqrshrun v6.2s, v7.2d, #17",
    "sshll v0.8h, v1.8b, #3\n ushll2 v2.4s, v3.8h, #0\n sxtl v4.2d, v5.2s\n uxtl2 v6.8h, v7.16b",
    "ushr d0, d1, #9\n shl d2, d3, #63\n ssra d4, d5, #1\n add d6, d7, d1\n sub d2, d3, d4\n cmgt d5, d6, d7\n addp d1, v2.2d",
    // by element
    "mul v0.8h, v1.8h, v2.h[3]\n mla v3.4s, v4.4s, v5.s[1]\n mls v6.4h, v7.4h, v1.h[7]\n sqdmulh v2.4s, v3.4s, v4.s[3]",
    "smull v0.4s, v1.4h, v2.h[2]\n umlal2 v3.2d, v4.4s, v5.s[3]\n smlsl v6.4s, v7.4h, v1.h[0]\n sqdmull2 v2.4s, v3.8h, v4.h[5]",
    // floating point vector
    "fadd v0.4s, v1.4s, v2.4s\n fsub v3.2d, v4.2d, v5.2d\n fmul v6.4s, v7.4s, v1.4s\n fdiv v2.2d, v3.2d, v4.2d\n fabd v5.4s, v6.4s, v7.4s",
    "fmla v0.4s, v1.4s, v2.4s\n fmls v3.2d, v4.2d, v5.2d\n fmax v6.4s, v7.4s, v1.4s\n fminnm v2.2d, v3.2d, v4.2d\n faddp v5.4s, v6.4s, v7.4s",
    "fcmeq v0.4s, v1.4s, v2.4s\n fcmge v3.2d, v4.2d, v5.2d\n fcmgt v6.4s, v7.4s, v1.4s\n facgt v2.4s, v3.4s, v4.4s\n fcmlt v5.2d, v6.2d, #0.0",
    "fabs v0.4s, v1.4s\n fneg v2.2d, v3.2d\n fsqrt v4.4s, v5.4s\n frintm v6.2d, v7.2d\n frintn v1.4s, v2.4s\n frinta v3.2d, v4.2d\n frintz v5.4s, v6.4s",
    "scvtf v0.4s, v1.4s\n ucvtf v2.2d, v3.2d\n fcvtzs v4.4s, v5.4s\n fcvtzu v6.2d, v7.2d\n fcvtn v1.2s, v2.2d\n fcvtl v3.2d, v4.2s\n scvtf v5.4s, v6.4s, #8",
    "fmul v0.4s, v1.4s, v2.s[1]\n fmla v3.2d, v4.2d, v5.d[1]\n faddp s6, v7.2s\n fmaxnmp d1, v2.2d\n fmaxv s3, v4.4s",
    // scalar FP
    "fadd s0, s1, s2\n fmul d3, d4, d5\n fmadd s6, s7, s1, s2\n fnmsub d3, d4, d5, d6\n fsqrt s7, s1\n fcvt d2, s3\n fcvt s4, d5",
    "fcmp s1, s2\n cset x0, mi\n fcmpe d3, d4\n cset x1, gt\n fccmp s5, s6, #4, ne\n cset x2, eq\n fcsel d7, d1, d2, lt",
    "fcvtzs w0, s1\n fcvtzu x1, d2\n fcvtms w2, d3\n fcvtps x3, s4\n fcvtas w4, d5\n fcvtns x5, s6\n scvtf d6, x7\n ucvtf s7, w1",
    "frintm s0, s1\n frintp d2, d3\n frintz s4, s5\n frinta d6, d7\n frintn s1, s2\n fabs d3, d4\n fneg s5, s6\n fmax d7, d1, d2\n fminnm s3, s4, s5",
    "ucvtf d0, x3, #10\n scvtf s1, w4, #3\n scvtf d2, x5, #64\n ucvtf s3, x6, #17\n fcvtzs x1, d4, #10\n fcvtzu w2, s5, #5\n fcvtzs w3, d6, #31",
    "movi v0.16b, #0x5a\n movi v1.4s, #0x12, lsl #16\n mvni v2.8h, #0x34, lsl #8\n movi v3.2d, #0xff00ff0000ffff00\n orr v4.4s, #0x7, lsl #8\n bic v5.8h, #0x81\n fmov v6.4s, #1.5\n fmov d7, #-2.25",
];

fn wrapper(name: &str, body: &str) -> String {
    let mut s = format!(".p2align 2\n.globl {name}\n{name}:\n stp x29, x30, [sp, #-16]!\n mov x17, x0\n msr fpsr, xzr\n");
    for i in (0..8).step_by(2) { writeln!(s, " ldp q{}, q{}, [x17, #{}]", i, i + 1, 64 + i * 16).unwrap(); }
    for i in (0..8).step_by(2) { writeln!(s, " ldp x{}, x{}, [x17, #{}]", i, i + 1, i * 8).unwrap(); }
    writeln!(s, "{body}").unwrap();
    for i in (0..8).step_by(2) { writeln!(s, " stp x{}, x{}, [x17, #{}]", i, i + 1, i * 8).unwrap(); }
    for i in (0..8).step_by(2) { writeln!(s, " stp q{}, q{}, [x17, #{}]", i, i + 1, 64 + i * 16).unwrap(); }
    s.push_str(" mrs x16, fpsr\n and x16, x16, #0x08000000\n str x16, [x17, #192]\n ldp x29, x30, [sp], #16\n ret\n");
    s
}

const WORDS: usize = 8 + 16 + 1; // x0..x7, v0..v7 (2 words each), fpsr.QC

fn inputs(seed: u64) -> [u64; WORDS] {
    let mut x = seed.wrapping_mul(0x9e37_79b9_7f4a_7c15) | 1;
    let mut next = || { x ^= x >> 12; x ^= x << 25; x ^= x >> 27; x.wrapping_mul(0x2545_f491_4f6c_dd1d) };
    // edge lanes: zero, all ones, sign boundaries, small values, and FP specials (1.0, -0.0, inf, nan, 0.5)
    let edges: [u64; 16] = [0, u64::MAX, 0x7f7f_7f7f_7f7f_7f7f, 0x8080_8080_8080_8080, 0x0001_0001_0001_0001, 0x8000_0000_8000_0000,
        0x7fff_ffff_7fff_ffff, 0x3f80_0000_bf80_0000, 0x3ff0_0000_0000_0000, 0x8000_0000_0000_0000, 0x7ff0_0000_0000_0000,
        0x7fc0_0000_7f80_0000, 0x4049_0fdb_c2f6_e979, 0x4024_0000_0000_0000, 0xc059_0000_0000_0000, 0x0000_0003_ffff_fffd];
    let mut s = [0u64; WORDS];
    for v in s.iter_mut().take(WORDS - 1) {
        let r = next();
        *v = match r % 4 { 0 => edges[(r >> 8) as usize % 16], 1 => next() & 0x0f0f_0f0f_0f0f_0f0f, _ => next() };
    }
    // the x registers feed shift amounts and dup/ins sources: keep some small
    s[3] &= 0x1ff;
    s[7] &= 0xffff_ffff;
    s[WORDS - 1] = 0;
    s
}

const SEEDS: u64 = 150;

#[test]
fn neon_snippets_match_host() {
    let dir = std::env::temp_dir().join(format!("arm-a64-neon-{}", std::process::id()));
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
    let mut failing = std::collections::BTreeSet::new();
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
    cpu.ram = ram.mem.as_mut_ptr();
    cpu.ram_len = ram.mem.len() as u64;
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
                if failing.insert(k) { eprintln!("snippet {k}: stopped with {trap:?} at {:#x}: `{}`", cpu.pc, b.replace('\n', ";")); }
                continue;
            }
            let got: Vec<u64> = (0..WORDS as u64).map(|i| ram.read64(state + 8 * i).unwrap()).collect();
            if got != host[idx] {
                failures += 1;
                if failing.insert(k) {
                    eprintln!("snippet {k} seed {seed}: `{}`", b.replace('\n', ";"));
                    for i in 0..WORDS {
                        if got[i] != host[idx][i] {
                            let name = if i < 8 { format!("x{i}") } else if i < 24 { format!("v{}.{}", (i - 8) / 2, if i % 2 == 0 { "lo" } else { "hi" }) } else { "fpsr.qc".into() };
                            eprintln!("  {name}: in {:#018x} emu {:#018x} host {:#018x}", s[i], got[i], host[idx][i]);
                        }
                    }
                }
            }
        }
    }
    let _ = std::fs::remove_dir_all(&dir);
    assert_eq!(failures, 0, "{failures} mismatching cases in snippets {failing:?}");
}
