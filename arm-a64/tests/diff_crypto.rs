//! Cryptographic Extension differential test against the host CPU: v0..v7, x0..x7 and FPSR after each snippet
//! must match the native result for random inputs (including edge-value lanes).
#![cfg(all(target_arch = "aarch64", target_os = "macos"))]
mod common;
use arm_a64::Cpu;
use emu_core::{Bus, Core, FlatRam, Trap};
use std::fmt::Write as _;
use std::process::Command;

const SNIPPETS: &[&str] = &[
    "aese v0.16b, v1.16b\n aesmc v2.16b, v3.16b\n aesd v4.16b, v5.16b\n aesimc v6.16b, v7.16b",
    "aese v0.16b, v1.16b\n aesmc v0.16b, v0.16b\n aesd v2.16b, v3.16b\n aesimc v2.16b, v2.16b",
    "sha1c q0, s1, v2.4s\n sha1p q3, s4, v5.4s\n sha1m q6, s7, v1.4s",
    "sha1h s0, s1\n sha1su0 v2.4s, v3.4s, v4.4s\n sha1su1 v5.4s, v6.4s",
    "sha256h q0, q1, v2.4s\n sha256h2 q3, q4, v5.4s",
    "sha256su0 v0.4s, v1.4s\n sha256su1 v2.4s, v3.4s, v4.4s",
    "pmull v0.1q, v1.1d, v2.1d\n pmull2 v3.1q, v4.2d, v5.2d\n pmull v6.8h, v7.8b, v1.8b",
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
fn crypto_snippets_match_host() {
    let dir = std::env::temp_dir().join(format!("arm-a64-crypto-{}", std::process::id()));
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
            cpu.crypto = true;
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
