//! The backend against `eval` (`check`), the half conversions, the host's FPCR.
use crate::check::*;
use crate::*;

#[test]
fn random_blocks_match_eval() {
    let n = check(0x1234_5678_9abc_def1, if cfg!(debug_assertions) { 3000 } else { 20000 }).unwrap_or_else(|e| panic!("{e}"));
    if available() {
        assert_eq!(n, if cfg!(debug_assertions) { 3000 } else { 20000 }, "every block compiled");
    }
}

#[test]
fn halves_both_ways() {
    // every half, to single and back
    for h in 0..=0xffffu32 {
        let f = ir::half_to_f32(h as u16);
        let e = h >> 10 & 31;
        if e == 31 && h & 0x3ff != 0 {
            assert_eq!(ir::f32_to_half(f), (h & 0x8000) as u16 | 0x7e00, "{h:04x}");
        } else {
            assert_eq!(ir::f32_to_half(f), h as u16, "{h:04x} {f:08x}");
        }
    }
    // rounding: halfway between two halves goes to the even one
    assert_eq!(ir::f32_to_half(0x3f80_1000), 0x3c00); // 1 + 2^-11
    assert_eq!(ir::f32_to_half(0x3f80_3000), 0x3c02);
    assert_eq!(ir::f32_to_half(0x477f_f000), 0x7c00); // 65520: rounds to infinity
    assert_eq!(ir::f32_to_half(0x477f_efff), 0x7bff);
    assert_eq!(ir::f32_to_half(0x3300_0000), 0x0000); // 2^-25: a tie, to zero
    assert_eq!(ir::f32_to_half(0x3300_0001), 0x0001);
    assert_eq!(ir::f32_to_half(0x3380_0000), 0x0001); // 2^-24
    assert_eq!(ir::f32_to_half(0x3840_0000), 0x0300); // 3 * 2^-15: a denormal half
}

/// The native conversions against the Rust ones, for every half and many singles.
#[test]
fn half_conversions_match_eval() {
    let mut b = Builder::new();
    let x = b.reg(0);
    let h = b.un(Un::HalfToF, x);
    b.store_all(1, h);
    let s = b.un(Un::FToHalf, x);
    b.store_all(2, s);
    let code = Code::new(b.finish());
    let mut r = Rng(77);
    for chunk in 0..(1 << 16) / 16 + 20000 {
        let mut regs = vec![[0u32; 16]; 3];
        regs[0] = std::array::from_fn(|i| if chunk < 4096 { (chunk * 16 + i) as u32 | (r.next() as u32) << 16 } else { r.next() as u32 >> (r.below(8)) });
        let mut want = regs.clone();
        eval(&code.block, &mut want, &mut [], &[], &[], &mut []);
        code.run(&mut regs, &mut [], &[], &[], &mut []);
        assert_eq!(regs, want, "chunk {chunk}");
    }
}

/// Code is the same whatever the host's FPCR says (round toward zero, flush, default NaN).
#[cfg(all(target_arch = "aarch64", any(target_os = "macos", target_os = "linux")))]
#[test]
#[allow(unsafe_code)]
fn native_code_ignores_the_hosts_fpcr() {
    let mut r = Rng(5);
    for _ in 0..200 {
        let b = random_block(&mut r, 30);
        let code = Code::new(b);
        let regs: Vec<V> = (0..12).map(|_| std::array::from_fn(|_| word(&mut r))).collect();
        let (mut ra, mut rb) = (regs.clone(), regs);
        let (mut fa, mut fb) = (vec![0x1234u32; 3], vec![0x1234u32; 3]);
        let args = [word(&mut r), 1, 2, 3];
        let inputs = vec![[0x3f80_0001u32; 16]; 3];
        eval(&code.block, &mut ra, &mut fa, &args, &inputs, &mut [[0; 16]; 2]);
        let old: u64;
        // SAFETY: FPCR is put back right after the run
        unsafe {
            std::arch::asm!("mrs {}, fpcr", out(reg) old);
            std::arch::asm!("msr fpcr, {}", in(reg) old | 3 << 22 | 1 << 24 | 1 << 25);
        }
        code.run(&mut rb, &mut fb, &args, &inputs, &mut [[0; 16]; 2]);
        // SAFETY: the host's own value back
        unsafe { std::arch::asm!("msr fpcr, {}", in(reg) old) };
        assert_eq!(ra, rb);
    }
}

#[test]
#[ignore]
fn find_small_failure() {
    let mut best: Option<(usize, String)> = None;
    for seed in 1..4000u64 {
        let mut r = Rng(seed * 7919 | 1);
        let n = 4 + r.below(20);
        let b = random_block(&mut r, n);
        let regs: Vec<V> = (0..12).map(|_| std::array::from_fn(|_| word(&mut r))).collect();
        let flags: Vec<u32> = (0..3).map(|_| r.next() as u32 & 0xffff).collect();
        let args: Vec<u32> = (0..4).map(|_| word(&mut r)).collect();
        let inputs: Vec<V> = (0..3).map(|_| std::array::from_fn(|_| word(&mut r))).collect();
        let (mut ra, mut fa, mut oa) = (regs.clone(), flags.clone(), vec![[0u32; 16]; 2]);
        let (mut rb, mut fb, mut ob) = (regs.clone(), flags.clone(), vec![[0u32; 16]; 2]);
        eval(&b, &mut ra, &mut fa, &args, &inputs, &mut oa);
        let len = b.ir.len();
        let code = Code::new(b);
        code.run(&mut rb, &mut fb, &args, &inputs, &mut ob);
        if (ra != rb || fa != fb || oa != ob) && best.as_ref().is_none_or(|b| len < b.0) {
            best = Some((len, format!("{:#?}\nregs eval {:08x?}\ncode {:08x?}\nflags {fa:x?} {fb:x?}\nargs {args:08x?}", code.block.ir.iter().enumerate().collect::<Vec<_>>(), ra, rb)));
        }
    }
    if let Some(b) = best { panic!("{}", b.1) }
}
