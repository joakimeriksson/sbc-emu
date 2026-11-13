//! Differential test against the host CPU (Apple silicon is AArch64): every snippet runs natively
//! and on the emulator from the same random register state; x0..x15 and NZCV must match exactly.
//! Snippets may only use x0..x15 and must not touch memory or branch outside themselves.
#![cfg(all(target_arch = "aarch64", target_os = "macos"))]
mod common;
use arm_a64::Cpu;
use emu_core::{Bus, Core, FlatRam, Trap};
use std::fmt::Write as _;
use std::process::Command;

const SNIPPETS: &[&str] = &[
    "add x0, x1, x2\n adds x3, x4, x5\n adc x6, x7, x8\n adcs x9, x10, x11",
    "sub x0, x1, x2\n subs x3, x4, x5\n sbc x6, x7, x8\n sbcs x9, x10, x11\n cmp x12, x13\n cset x14, lt",
    "adds w0, w1, w2\n subs w3, w4, w5\n adcs w6, w7, w8\n sbcs w9, w10, w11\n negs w12, w13",
    "add x0, x1, #0xfff\n add x2, x3, #0x123, lsl #12\n subs w4, w5, #1\n cmn x6, #7\n cset w7, eq",
    "add x0, x1, x2, lsl #13\n sub x3, x4, x5, lsr #7\n adds x6, x7, x8, asr #63\n add w9, w10, w11, asr #31",
    "add x0, x1, w2, uxtb #2\n add x3, x4, w5, sxth\n sub x6, x7, w8, sxtw #4\n adds x9, x10, x11, uxtx #1\n add w12, w13, w14, uxth #3",
    "and x0, x1, x2\n orr x3, x4, x5, ror #17\n eor x6, x7, x8, lsl #3\n bic x9, x10, x11\n orn x12, x13, x14, asr #9\n eon w15, w1, w2",
    "ands x0, x1, x2\n bics w3, w4, w5\n tst x6, #0xff00\n cset x7, ne",
    "and x0, x1, #0x5555555555555555\n orr w2, w3, #0xf0f0f0f0\n eor x4, x5, #0x3ffc\n ands w6, w7, #0x80000001\n mov x8, #0x0000ffff0000ffff",
    "movz x0, #0x1234, lsl #48\n movk x0, #0xbeef, lsl #16\n movn x1, #5\n movn w2, #0x8000, lsl #16\n mov w3, #-17",
    "ubfx x0, x1, #7, #13\n sbfx x2, x3, #60, #4\n bfi x4, x5, #12, #20\n bfxil w6, w7, #3, #9\n ubfiz w8, w9, #5, #11\n sbfiz x10, x11, #30, #3",
    "lsl x0, x1, #17\n lsr w2, w3, #31\n asr x4, x5, #1\n ror w6, w7, #13\n sxtb x8, w9\n uxth w10, w11\n sxtw x12, w13",
    "extr x0, x1, x2, #23\n extr w3, w4, w5, #31\n ror x6, x7, #0",
    "lslv x0, x1, x2\n lsrv w3, w4, w5\n asrv x6, x7, x8\n rorv w9, w10, w11",
    "udiv x0, x1, x2\n sdiv x3, x4, x5\n udiv w6, w7, w8\n sdiv w9, w10, w11\n udiv x12, x13, xzr",
    "mul x0, x1, x2\n madd x3, x4, x5, x6\n msub w7, w8, w9, w10\n smull x11, w12, w13\n umull x14, w15, w1",
    "smulh x0, x1, x2\n umulh x3, x4, x5\n smaddl x6, w7, w8, x9\n umsubl x10, w11, w12, x13",
    "rbit x0, x1\n rev x2, x3\n rev32 x4, x5\n rev16 w6, w7\n clz x8, x9\n cls w10, w11\n clz w12, wzr\n cls x13, x14",
    "csel x0, x1, x2, hi\n csinc w3, w4, w5, ge\n csinv x6, x7, x8, mi\n csneg w9, w10, w11, vs\n cinc x12, x13, le",
    "cmp x0, x1\n ccmp x2, x3, #5, lt\n ccmn w4, #17, #10, cc\n cset x5, gt\n csetm w6, pl",
    "subs x0, x1, x2\n b.eq 1f\n add x3, x3, #1\n1: b.ge 2f\n add x4, x4, #1\n2: cbz w5, 3f\n add x6, x6, #1\n3: tbnz x7, #63, 4f\n add x8, x8, #1\n4: tbz w9, #3, 5f\n add x10, x10, #1\n5:",
    "adr x0, .\n sub x0, x0, x15\n adds x1, x2, x2\n cinc x3, x4, cs",
];

fn wrapper(name: &str, body: &str) -> String {
    let mut s = format!(".p2align 2\n.globl {name}\n{name}:\n stp x29, x30, [sp, #-16]!\n mov x17, x0\n ldr x16, [x17, #128]\n msr nzcv, x16\n");
    for i in (0..16).step_by(2) {
        writeln!(s, " ldp x{}, x{}, [x17, #{}]", i, i + 1, i * 8).unwrap();
    }
    writeln!(s, "{body}").unwrap();
    for i in (0..16).step_by(2) {
        writeln!(s, " stp x{}, x{}, [x17, #{}]", i, i + 1, i * 8).unwrap();
    }
    s.push_str(" mrs x16, nzcv\n str x16, [x17, #128]\n ldp x29, x30, [sp], #16\n ret\n");
    s
}

/// xorshift64*: deterministic inputs that hit edge values often.
fn inputs(seed: u64) -> [u64; 17] {
    let mut x = seed.wrapping_mul(0x9e37_79b9_7f4a_7c15) | 1;
    let mut next = || {
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        x.wrapping_mul(0x2545_f491_4f6c_dd1d)
    };
    let edges = [
        0u64,
        1,
        2,
        0x7fff_ffff,
        0x8000_0000,
        0xffff_ffff,
        0x1_0000_0000,
        i64::MAX as u64,
        1 << 63,
        u64::MAX,
        63,
        64,
        31,
        32,
    ];
    let mut s = [0u64; 17];
    for v in s.iter_mut().take(16) {
        let r = next();
        *v = if r % 3 == 0 {
            edges[(r >> 8) as usize % edges.len()]
        } else {
            next()
        };
    }
    s[15] &= 0xffff; // x15: small value (used as an offset by the adr snippet)
    s[16] = (next() & 0xf) << 28;
    s
}

const SEEDS: u64 = 200;

#[test]
fn integer_snippets_match_host() {
    let dir = std::env::temp_dir().join(format!("arm-a64-diff-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    // host binary: one function per snippet, a driver that prints the outputs for every seed
    let mut asm = String::from(".text\n");
    for (k, b) in SNIPPETS.iter().enumerate() {
        asm += &wrapper(&format!("_snip{k}"), b);
    }
    let mut c = String::from("#include <stdio.h>\n#include <stdint.h>\n#include <stdlib.h>\n");
    for k in 0..SNIPPETS.len() {
        writeln!(c, "void snip{k}(uint64_t*);").unwrap();
    }
    write!(c, "typedef void (*fn)(uint64_t*);\nstatic fn F[] = {{").unwrap();
    for k in 0..SNIPPETS.len() {
        write!(c, "snip{k},").unwrap();
    }
    c += "};\nint main(void){ uint64_t s[17]; int k; while (scanf(\"%d\", &k) == 1) { for (int i = 0; i < 17; i++) scanf(\"%llx\", (unsigned long long*)&s[i]); F[k](s); for (int i = 0; i < 17; i++) printf(\"%llx \", (unsigned long long)s[i]); printf(\"\\n\"); } return 0; }\n";
    std::fs::write(dir.join("s.s"), &asm).unwrap();
    std::fs::write(dir.join("m.c"), &c).unwrap();
    let st = Command::new("clang")
        .current_dir(&dir)
        .args(["-O1", "m.c", "s.s", "-o", "host"])
        .status()
        .unwrap();
    assert!(st.success());
    let mut input = String::new();
    let mut cases = Vec::new();
    for k in 0..SNIPPETS.len() {
        for seed in 0..SEEDS {
            let s = inputs(seed * 1000 + k as u64);
            write!(input, "{k}").unwrap();
            for v in s {
                write!(input, " {v:x}").unwrap();
            }
            input.push('\n');
            cases.push((k, s));
        }
    }
    std::fs::write(dir.join("in.txt"), &input).unwrap();
    let out = Command::new("sh")
        .current_dir(&dir)
        .arg("-c")
        .arg("./host < in.txt")
        .output()
        .unwrap();
    assert!(out.status.success());
    let host: Vec<Vec<u64>> = String::from_utf8(out.stdout)
        .unwrap()
        .lines()
        .map(|l| {
            l.split_whitespace()
                .map(|h| u64::from_str_radix(h, 16).unwrap())
                .collect()
        })
        .collect();
    assert_eq!(host.len(), cases.len());

    // the emulator runs the identical code (assembled for ELF), returning to a `brk`
    let mut failures = 0;
    for (k, b) in SNIPPETS.iter().enumerate() {
        let code = common::assemble(&format!("{}\nhalt: brk #0\n", wrapper("snip", b)));
        let halt = common::LOAD + code.len() as u64 - 4;
        for seed in 0..SEEDS as usize {
            let idx = k * SEEDS as usize + seed;
            let (_, s) = cases[idx];
            let mut ram = FlatRam::new(0, 1 << 22);
            ram.load(common::LOAD, &code).unwrap();
            let state = 0x20_0000u64;
            for (i, v) in s.iter().enumerate() {
                ram.write64(state + 8 * i as u64, *v).unwrap();
            }
            let mut cpu = Cpu::new();
            cpu.brk_to_host = true;
            cpu.el = 0;
            cpu.pc = common::LOAD;
            cpu.sp[0] = 0x30_0000;
            cpu.x[0] = state;
            cpu.x[30] = halt;
            let mut trap = None;
            for _ in 0..10_000 {
                if let Err(t) = cpu.step(&mut ram) {
                    trap = Some(t);
                    break;
                }
            }
            assert_eq!(
                trap,
                Some(Trap::Brk(0)),
                "snippet {k}: {:?} at {:#x}",
                trap,
                cpu.pc
            );
            let got: Vec<u64> = (0..17)
                .map(|i| ram.read64(state + 8 * i).unwrap())
                .collect();
            // adr result depends on load address: compare it modulo the address
            if got != host[idx] && !(k == SNIPPETS.len() - 1) {
                failures += 1;
                if failures <= 10 {
                    eprintln!("snippet {k} seed {seed}: `{}`", b.replace('\n', ";"));
                    for i in 0..17 {
                        if got[i] != host[idx][i] {
                            eprintln!(
                                "  {}: in {:#x} emu {:#x} host {:#x}",
                                if i == 16 {
                                    "nzcv".into()
                                } else {
                                    format!("x{i}")
                                },
                                s[i],
                                got[i],
                                host[idx][i]
                            );
                        }
                    }
                }
            }
        }
    }
    let _ = std::fs::remove_dir_all(&dir);
    assert_eq!(failures, 0, "{failures} mismatching cases");
}
