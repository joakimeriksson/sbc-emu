//! The JIT against the interpreter: random straight-line A32 and Thumb code
//! (`common/jit_gen.rs`) runs once interpreted and once compiled from the same state;
//! registers, flags, memory and the instruction count must match.
#[path = "common/jit_gen.rs"]
mod jit_gen;
use arm_a32::{CodeWatch, Cpu};
use emu_core::{Bus, Core, FlatRam};
use jit_gen::*;

#[test]
fn random_blocks_match_the_interpreter() {
    if !arm_a32::jit::available() {
        return;
    }
    let mut rng = Rng(0x9e37_79b9_7f4a_7c15);
    let cases = std::env::var("JIT_DIFF_CASES").ok().and_then(|v| v.parse().ok()).unwrap_or(3000);
    for case in 0..cases {
        let len = 1 + rng.below(40) as usize;
        let mut words: Vec<u32> = (0..len).map(|_| gen(&mut rng)).collect();
        words.push(0xe120_0070); // bkpt #0
        let seed = rng.next() as u64 | 1;
        let (mut a, mut ram_a) = setup(&mut Rng(seed), &words);
        let (mut b, mut ram_b) = setup(&mut Rng(seed), &words);
        run(&mut a, &mut ram_a);
        // SAFETY: ram_b outlives b
        unsafe { b.set_ram(ram_b.mem.as_mut_ptr(), 0, RAM as u32) };
        assert!(b.enable_jit(CodeWatch::new(0, RAM as u32)));
        b.jit_set_hot(1);
        run(&mut b, &mut ram_b);
        let (compiled, _, _) = b.jit_stats().unwrap();
        assert!(compiled >= 1, "case {case}: nothing compiled");
        let dis: Vec<String> = words.iter().map(|w| format!("{w:08x}")).collect();
        assert_eq!(a.r, b.r, "case {case} registers; code {}", dis.join(" "));
        assert_eq!(a.cpsr, b.cpsr, "case {case} cpsr; code {}", dis.join(" "));
        assert_eq!(a.instret(), b.instret(), "case {case} instret; code {}", dis.join(" "));
        assert!(ram_a.mem == ram_b.mem, "case {case} memory; code {}", dis.join(" "));
    }
}

#[test]
fn random_thumb_blocks_match_the_interpreter() {
    if !arm_a32::jit::available() {
        return;
    }
    let mut rng = Rng(0x2545_f491_4f6c_dd1d);
    let cases = std::env::var("JIT_DIFF_CASES").ok().and_then(|v| v.parse().ok()).unwrap_or(3000);
    for case in 0..cases {
        let len = 1 + rng.below(30) as usize;
        let mut code: Vec<u16> = Vec::new();
        for _ in 0..len {
            if rng.below(6) == 0 {
                // an IT block of 1-4 single instructions
                let firstcond = rng.below(14);
                let k = 1 + rng.below(4);
                let mut mask = 1 << (4 - k);
                for i in 1..k {
                    let then = rng.below(2) == 0;
                    let bit = (firstcond & 1) ^ u32::from(!then);
                    mask |= bit << (4 - i);
                }
                code.push((0xbf00 | firstcond << 4 | mask) as u16);
                let mut done = 0;
                while done < k {
                    let v = gen_thumb(&mut rng);
                    let single = v.len() == 1 || v.len() == 2 && v[0] >> 11 >= 0b11101;
                    if single {
                        code.extend(v);
                        done += 1;
                    }
                }
            } else {
                code.extend(gen_thumb(&mut rng));
            }
        }
        code.push(0xbe00); // bkpt #0
        let seed = rng.next() as u64 | 1;
        let (mut a, mut ram_a) = setup_thumb(&mut Rng(seed), &code);
        let (mut b, mut ram_b) = setup_thumb(&mut Rng(seed), &code);
        if std::env::var_os("JIT_DIFF_TRACE").is_some() {
            let dis: Vec<String> = code.iter().map(|h| format!("{h:04x}")).collect();
            eprintln!("case {case}: {}", dis.join(" "));
        }
        run(&mut a, &mut ram_a);
        // SAFETY: ram_b outlives b
        unsafe { b.set_ram(ram_b.mem.as_mut_ptr(), 0, RAM as u32) };
        assert!(b.enable_jit(CodeWatch::new(0, RAM as u32)));
        b.jit_set_hot(1);
        run(&mut b, &mut ram_b);
        let dis: Vec<String> = code.iter().map(|h| format!("{h:04x}")).collect();
        assert!(b.jit_stats().unwrap().0 >= 1, "case {case}: nothing compiled");
        assert_eq!(a.r, b.r, "case {case} registers; code {}", dis.join(" "));
        assert_eq!(a.cpsr, b.cpsr, "case {case} cpsr; code {}", dis.join(" "));
        assert_eq!(a.instret(), b.instret(), "case {case} instret; code {}", dis.join(" "));
        assert!(ram_a.mem == ram_b.mem, "case {case} memory; code {}", dis.join(" "));
    }
}

/// Debugging aid: `JIT_REPRO="f83f 1033 ..." cargo test --test jit_diff thumb_repro -- --ignored`
#[test]
#[ignore]
fn thumb_repro() {
    let code: Vec<u16> = std::env::var("JIT_REPRO").unwrap().split_whitespace().map(|h| u16::from_str_radix(h, 16).unwrap()).collect();
    let seed = std::env::var("JIT_SEED").ok().and_then(|v| v.parse().ok()).unwrap_or(1);
    let (mut a, mut ram_a) = setup_thumb(&mut Rng(seed), &code);
    let (mut b, mut ram_b) = setup_thumb(&mut Rng(seed), &code);
    run(&mut a, &mut ram_a);
    unsafe { b.set_ram(ram_b.mem.as_mut_ptr(), 0, RAM as u32) };
    assert!(b.enable_jit(CodeWatch::new(0, RAM as u32)));
    b.jit_set_hot(1);
    run(&mut b, &mut ram_b);
    assert_eq!(a.r, b.r);
    assert_eq!(a.cpsr, b.cpsr);
}

/// Interworking in compiled code: Thumb calls ARM code with BLX (register) and returns with
/// BX LR; ARM calls Thumb with BLX (immediate); POP {pc} and LDR pc switch state.
#[test]
fn interworking_calls() {
    if !arm_a32::jit::available() {
        return;
    }
    let mut ram = FlatRam::new(0, RAM);
    // Thumb at 0x1000: r4 counts down; each round calls the ARM routine at 0x2000 (blx r3),
    // which calls the Thumb routine at 0x3000 (blx #imm) and returns with bx lr
    let thumb: [u16; 9] = [
        0x2405, // movs r4, #5
        0x4798, // loop: blx r3
        0x3c01, // subs r4, #1
        0xd1fc, // bne loop
        0xbe00, // bkpt
        0, 0, 0, 0,
    ];
    for (i, h) in thumb.iter().enumerate() {
        ram.write16(0x1000 + 2 * i as u64, *h).unwrap();
    }
    let arm: [u32; 5] = [
        0xe92d_4010, // push {r4, lr}
        0xe280_0001, // add r0, r0, #1
        0xfa00_03fc, // blx 0x3000 (Thumb)
        0xe8bd_4010, // pop {r4, lr}
        0xe12f_ff1e, // bx lr
    ];
    for (i, w) in arm.iter().enumerate() {
        ram.write32(0x2000 + 4 * i as u64, *w).unwrap();
    }
    let callee: [u16; 4] = [
        0xb500, // push {lr}
        0x3102, // adds r1, #2
        0xbd00, // pop {pc}
        0,
    ];
    for (i, h) in callee.iter().enumerate() {
        ram.write16(0x3000 + 2 * i as u64, *h).unwrap();
    }
    let mut ram_b = FlatRam::new(0, RAM);
    ram_b.mem.copy_from_slice(&ram.mem);
    let init = |cpu: &mut Cpu| {
        cpu.r[15] = 0x1000;
        cpu.cpsr |= 1 << 5;
        cpu.r[3] = 0x2000;
        cpu.r[13] = 0x9000;
    };
    let (mut a, mut b) = (Cpu::new(), Cpu::new());
    init(&mut a);
    init(&mut b);
    run(&mut a, &mut ram);
    unsafe { b.set_ram(ram_b.mem.as_mut_ptr(), 0, RAM as u32) };
    assert!(b.enable_jit(CodeWatch::new(0, RAM as u32)));
    b.jit_set_hot(1);
    run(&mut b, &mut ram_b);
    assert_eq!((a.r[0], a.r[1]), (5, 10));
    assert_eq!(a.r, b.r);
    assert_eq!(a.cpsr, b.cpsr);
    assert_eq!(a.instret(), b.instret());
}

/// The same code in loops: compiled blocks branch back into themselves (the browser build's
/// regions loop inside one function while `chain_until` allows).
#[test]
fn random_loops_match_the_interpreter() {
    if !arm_a32::jit::available() {
        return;
    }
    let mut rng = Rng(0x6a09_e667_f3bc_c909);
    let cases = std::env::var("JIT_DIFF_CASES").ok().and_then(|v| v.parse().ok()).unwrap_or(1000);
    for case in 0..cases {
        let thumb = case % 2 == 1;
        let ((arm, code16), seed) = if thumb {
            let (c, s) = thumb_loop_case(&mut rng);
            ((Vec::new(), c), s)
        } else {
            let (c, s) = arm_loop_case(&mut rng);
            ((c, Vec::new()), s)
        };
        if let Err(e) = compare_case(thumb, &arm, &code16, seed, u64::MAX) {
            let dis: Vec<String> = if thumb { code16.iter().map(|h| format!("{h:04x}")).collect() } else { arm.iter().map(|w| format!("{w:08x}")).collect() };
            panic!("case {case} ({}): {e}; code {}", if thumb { "Thumb" } else { "A32" }, dis.join(" "));
        }
    }
}
