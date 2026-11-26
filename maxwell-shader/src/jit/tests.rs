use super::check::{check, check_lanes};
use super::*;

#[test]
fn random_blocks_match_the_interpreter() {
    let ops = check(0x1234_5678_9abc_def1, if cfg!(debug_assertions) { 3000 } else { 20000 }).unwrap_or_else(|e| panic!("{e}"));
    // every kind of instruction the JIT takes came up
    for k in ["FAdd", "FMul", "Ffma", "FMnMx", "FSet", "FSetP", "Mufu", "F2F", "F2I", "I2F", "IAdd", "Xmad", "Lop", "Lop3", "Shl", "Shr", "ISetP", "Sel", "Mov"] {
        assert!(ops.get(k).is_some_and(|&n| n > 20), "{k}: {ops:?}");
    }
    // (and what only the native code has: PRMT; F2F and IADD are above, with halves and carries)
    if super::EXT {
        assert!(ops.get("Prmt").is_some_and(|&n| n > 20), "{ops:?}");
    }
    // and in 32-lane warps (compute's; natively the AArch64 code). (In this test, not another:
    // the checks turn the JIT on and off for everyone.)
    let ops = check_lanes::<32>(0x5eed_1234_abcd_0001, if cfg!(debug_assertions) { 2000 } else { 20000 }).unwrap_or_else(|e| panic!("{e}"));
    assert!(ops.get("Ffma").is_some_and(|&n| n > 20), "{ops:?}");
}

/// A run longer than a block may be is cut into several, which together do what it does.
#[test]
fn long_runs_are_cut() {
    use crate::exec::{run, Lanes};
    // 4000 LOP3s (36 nodes each): far more than 16-bit ids can count
    let mut x = 0x2545_f491_4f6c_dd1du64;
    let lop3 = loop {
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        let w = x & !(0xf << 16) | 7 << 16;
        let inst = crate::decode::decode(8, w);
        if matches!(inst.op, Op::Lop3 { .. }) && lower(&mut Lower::default(), &inst).is_some() {
            break w;
        }
    };
    let ins: Vec<u64> = (0..4000u64).map(|k| lop3 & !0xff | (k % 20)).collect();
    let p = super::check::program(&ins);
    let code = Code::compile(&p).unwrap();
    assert!(code.blocks.len() > 1);
    assert_eq!(code.blocks.iter().map(|b| b.len).sum::<u32>(), 4000);
    let mut l = Lanes::<LANES>::new();
    l.start(&p, 0xffff, 0);
    for k in 0..255 {
        l.gpr[k] = std::array::from_fn(|i| ((k * 977 + i * 13) as u32).wrapping_mul(0x9e37_79b9));
    }
    let (mut gpr, mut pred, mut cc) = (l.gpr.clone(), l.pred, l.cc);
    let frame = Frame::default();
    for b in &code.blocks {
        eval(b, &mut gpr, &mut pred, &mut cc, &frame.cb, 0xffff);
    }
    let mut fuel = 10_000;
    run(&p, &mut l, &mut super::check::Cb, &mut fuel);
    assert!(*gpr == *l.gpr);
}

/// Timing of a sequence like the hot loop of ncnn's convolutions on NVK (run by hand).
#[test]
#[ignore]
fn timing_hot_sequence() {
    let _switch = SWITCH.lock().unwrap_or_else(|e| e.into_inner());
    use crate::exec::{run, Lanes};
    let seq: [u64; 12] = [
        0x3810000000271618, 0x4c10800400071818, 0x4c1008040017ff19, 0x381000000037161a, 0x4b62038400271a07, 0x5c98078001a7001d,
        0x5c9807800197001c, 0x36c07f8441071919, 0x5ca8000001770617, 0x5ca8000001870618, 0x59a00e0001770303, 0x3848008000370316,
    ];
    let alu: [u64; 6] = [0x3810000000271618, 0x381000000037161a, 0x4b62038400271a07, 0x5c98078001a7001d, 0x5c9807800197001c, 0x3848008000370316];
    let pairs = [0x3810000000271618u64, 0x4b62038400271a07, 0x36c07f8441071919];
    for (what, ins) in [("hot", (0..50).flat_map(|_| seq).collect::<Vec<u64>>()), ("alu", (0..50).flat_map(|_| alu).collect()), ("pairs", (0..100).flat_map(|_| pairs).collect())] {
    let p = super::check::program(&ins);
    eprintln!("{what}:");
    for (name, on) in [("interpreter", false), ("jit", true)] {
        set_enabled(on);
        let mut l = Lanes::<32>::new();
        let t = std::time::Instant::now();
        let reps = 20_000;
        for _ in 0..reps {
            l.start(&p, !0, 0);
            let mut fuel = 1_000_000;
            run(&p, &mut l, &mut super::check::Cb, &mut fuel);
        }
        let ns = t.elapsed().as_nanos() as f64 / (reps * ins.len()) as f64;
        eprintln!("{name}: {ns:.1} ns per warp instruction");
    }
    }
    set_enabled(true);
}

/// The interpreter's time for each instruction of that sequence (run by hand:
/// `cargo test --release -p maxwell-shader timing -- --ignored --nocapture`).
#[test]
#[ignore]
fn timing_each() {
    let _switch = SWITCH.lock().unwrap_or_else(|e| e.into_inner());
    use crate::exec::{run, Lanes};
    set_enabled(false);
    for (name, w) in [("iadd.cc", 0x4c10800400071818u64), ("iadd.x", 0x4c1008040017ff19), ("prmt", 0x36c07f8441071919), ("f2f.f32.f16", 0x5ca8000001770617), ("ffma", 0x59a00e0001770303), ("mov", 0x5c98078001a7001d), ("isetp", 0x4b62038400271a07), ("nop", 0x50b0000000070f00), ("iadd", 0x3810000000271618)] {
        let ins: Vec<u64> = vec![w; 300];
        let p = super::check::program(&ins);
        let mut l = Lanes::<32>::new();
        let t = std::time::Instant::now();
        let reps = 5_000;
        for _ in 0..reps {
            l.start(&p, !0, 0);
            let mut fuel = 1_000_000;
            run(&p, &mut l, &mut super::check::Cb, &mut fuel);
        }
        let ns = t.elapsed().as_nanos() as f64 / (reps * ins.len()) as f64;
        eprintln!("{name}: {ns:.1} ns {:?}", p.code[0].op);
    }
    set_enabled(true);
}

/// Conversions between halves and singles, for many values (every half), with the JIT and without.
#[test]
fn half_conversions_match_the_interpreter() {
    use crate::exec::{run, Lanes};
    let find = |want: &dyn Fn(&Op) -> bool| {
        let mut x = 0x1234_5678_9abc_def1u64;
        loop {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            let w = x & !(0xf << 16) | 7 << 16;
            let inst = crate::decode::decode(8, w);
            if want(&inst.op) && lower(&mut Lower::default(), &inst).is_some() {
                break w;
            }
        }
    };
    let words = [
        find(&|op| matches!(op, Op::F2F { from: 4, to: 2, b: Src::Reg(_), high: false, sat: false, ftz: false, m: Mod { abs: false, neg: false }, .. })),
        find(&|op| matches!(op, Op::F2F { from: 2, to: 4, b: Src::Reg(_), high: false, sat: false, ftz: false, m: Mod { abs: false, neg: false }, .. })),
        find(&|op| matches!(op, Op::F2F { from: 2, to: 4, b: Src::Reg(_), high: true, .. })),
        find(&|op| matches!(op, Op::F2F { from: 4, to: 2, b: Src::Reg(_), ftz: true, .. })),
    ];
    let mut r = 0x9e37_79b9_7f4a_7c15u64;
    let _switch = SWITCH.lock().unwrap_or_else(|e| e.into_inner());
    for w in words {
        let p = super::check::program(&[w, w]);
        let Op::F2F { b: Src::Reg(src), .. } = p.code[0].op else { unreachable!() };
        for chunk in 0..(1 << 16) / 16 + 4096 {
            let mut a = Lanes::<LANES>::new();
            a.start(&p, 0xffff, 0);
            for k in 0..255 {
                a.gpr[k] = std::array::from_fn(|i| {
                    if chunk < (1 << 16) / 16 {
                        // every half, and every single with those top bits
                        ((chunk * 16 + i) as u32) << if k % 2 == 0 { 0 } else { 16 }
                    } else {
                        r ^= r << 13;
                        r ^= r >> 7;
                        r ^= r << 17;
                        r as u32
                    }
                });
            }
            let _ = src;
            let mut b = a.clone();
            set_enabled(true);
            let (mut f1, mut f2) = (100, 100);
            run(&p, &mut a, &mut super::check::Cb, &mut f1);
            set_enabled(false);
            run(&p, &mut b, &mut super::check::Cb, &mut f2);
            set_enabled(true);
            for k in 0..256 {
                assert_eq!(a.gpr[k], b.gpr[k], "R{k} {:?} chunk {chunk}", p.code[0].op);
            }
        }
    }
}

/// The native code gives the interpreter's results whatever rounding mode (and half format) the
/// host's FPCR says, as a guest's may be live when the GPU runs.
#[cfg(all(target_arch = "aarch64", any(target_os = "macos", target_os = "linux")))]
#[test]
#[allow(unsafe_code)]
fn native_code_ignores_the_hosts_fpcr() {
    use crate::exec::{run, Lanes};
    let _switch = SWITCH.lock().unwrap_or_else(|e| e.into_inner());
    // F2F single to half, then FFMA: rounding shows in both
    let mut x = 0x1234_5678_9abc_def1u64;
    let mut find = |want: &dyn Fn(&Op) -> bool| loop {
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        let w = x & !(0xf << 16) | 7 << 16;
        let inst = crate::decode::decode(8, w);
        if want(&inst.op) && lower(&mut Lower::default(), &inst).is_some() {
            break w;
        }
    };
    let ins = [
        find(&|op| matches!(op, Op::F2F { from: 4, to: 2, b: Src::Reg(_), .. })),
        find(&|op| matches!(op, Op::Ffma { b: Src::Reg(_), c: Src::Reg(_), .. })),
    ];
    let p = super::check::program(&ins);
    let mut a = Lanes::<LANES>::new();
    a.start(&p, 0xffff, 0);
    let mut r = 0x9e37_79b9u64;
    for k in 0..255 {
        a.gpr[k] = std::array::from_fn(|_| {
            r ^= r << 13;
            r ^= r >> 7;
            r ^= r << 17;
            (r as u32 & 0x3fff_ffff) | 0x3000_0000
        });
    }
    let mut b = a.clone();
    set_enabled(false);
    let (mut f1, mut f2) = (100, 100);
    run(&p, &mut b, &mut super::check::Cb, &mut f2);
    set_enabled(true);
    // round toward zero, alternative half precision
    let old: u64;
    // SAFETY: FPCR is put back right after the run, which panics in no way that skips it here
    unsafe {
        std::arch::asm!("mrs {}, fpcr", out(reg) old);
        std::arch::asm!("msr fpcr, {}", in(reg) old | 3 << 22 | 1 << 26);
    }
    run(&p, &mut a, &mut super::check::Cb, &mut f1);
    // SAFETY: the host's own value back
    unsafe { std::arch::asm!("msr fpcr, {}", in(reg) old) };
    for k in 0..256 {
        assert_eq!(a.gpr[k], b.gpr[k], "R{k}");
    }
}
