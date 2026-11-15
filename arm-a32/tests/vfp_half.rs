//! VFP half-precision conversions (VCVTB/VCVTT, advertised by MVFR1.FPHP) against the
//! host's FCVT under the same FPSCR controls: all four rounding modes, flush-to-zero,
//! default NaN and the alternative half-precision format.
#![cfg(target_arch = "aarch64")]
use arm_a32::{Cpu, MODE_SVC};
use emu_core::{Bus, Core, FlatRam};

const VCVTB_F16_F32: u32 = 0xeeb3_0a41; // vcvtb.f16.f32 s0, s2
const VCVTT_F16_F32: u32 = 0xeeb3_0ac1; // vcvtt.f16.f32 s0, s2
const VCVTB_F32_F16: u32 = 0xeeb2_0a41; // vcvtb.f32.f16 s0, s2
const VCVTT_F32_F16: u32 = 0xeeb2_0ac1; // vcvtt.f32.f16 s0, s2

fn host_to_half(x: u32, fpcr: u64) -> u16 {
    let out: u64;
    unsafe {
        core::arch::asm!(
            "mrs x9, fpcr",
            "msr fpcr, {c}",
            "fmov s1, {x:w}",
            "fcvt h0, s1",
            "umov {o:w}, v0.h[0]",
            "msr fpcr, x9",
            c = in(reg) fpcr, x = in(reg) x as u64, o = out(reg) out,
            out("x9") _, out("v0") _, out("v1") _,
        );
    }
    out as u16
}

fn host_to_single(h: u16, fpcr: u64) -> u32 {
    let out: u64;
    unsafe {
        core::arch::asm!(
            "mrs x9, fpcr",
            "msr fpcr, {c}",
            "fmov s1, {x:w}",
            "fcvt s0, h1",
            "fmov {o:w}, s0",
            "msr fpcr, x9",
            c = in(reg) fpcr, x = in(reg) h as u64, o = out(reg) out,
            out("x9") _, out("v0") _, out("v1") _,
        );
    }
    out as u32
}

fn emulate(word: u32, s2: u32, s0: u32, fpscr: u32) -> u32 {
    let mut ram = FlatRam::new(0, 0x2000);
    ram.write32(0x1000, word).unwrap();
    let mut cpu = Cpu::new();
    cpu.cpsr = MODE_SVC | 0xc0;
    cpu.cpacr = 0xf << 20;
    cpu.vfp.fpexc = 1 << 30;
    cpu.vfp.fpscr = fpscr;
    cpu.vfp.set_s(0, s0);
    cpu.vfp.set_s(2, s2);
    cpu.r[15] = 0x1000;
    cpu.step(&mut ram).expect("executes");
    cpu.vfp.s(0)
}

#[test]
fn half_precision_conversions_match_host() {
    let mut x: u64 = 0x5851_f42d_4c95_7f2d;
    let mut next = || {
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        x
    };
    let mut checked = 0;
    for i in 0..40_000u32 {
        let r = next();
        // Singles around the half-precision range (including its subnormals and overflow),
        // plus specials.
        let single = match i % 4 {
            0 => r as u32,
            1 => (r as u32 & 0x807f_ffff) | ((100 + (r >> 32) % 50) as u32) << 23,
            2 => [0, 0x8000_0000, 0x7f80_0000, 0xff80_0000, 0x7fc0_1234, 0x7f80_0001, 0x0000_0001, 0x477f_f000,
                  0x477f_efff, 0x3380_0000, 0x3300_0001, 0x387f_c000][(r % 12) as usize],
            _ => (r as u32 & 0x807f_ffff) | ((r >> 40) as u32 & 0xff) << 23,
        };
        let half = (r >> 48) as u16;
        let controls = (next() & 0xf) as u32; // rmode (2 bits), FZ, DN, AHP drawn below
        let rmode = controls & 3;
        let fz = controls >> 2 & 1;
        let dn = (next() & 1) as u32;
        let ahp = (next() % 4 == 0) as u32;
        let fpscr = ahp << 26 | dn << 25 | fz << 24 | rmode << 22;
        let fpcr = fpscr as u64;
        let old = next() as u32;

        let want = host_to_half(single, fpcr) as u32;
        let got = emulate(VCVTB_F16_F32, single, old, fpscr);
        assert_eq!(got, old & 0xffff_0000 | want, "vcvtb.f16.f32 {single:#010x} fpscr {fpscr:#x}");
        let got = emulate(VCVTT_F16_F32, single, old, fpscr);
        assert_eq!(got, old & 0xffff | want << 16, "vcvtt.f16.f32 {single:#010x} fpscr {fpscr:#x}");

        let want = host_to_single(half, fpcr);
        let got = emulate(VCVTB_F32_F16, half as u32 | 0xabcd_0000, old, fpscr);
        assert_eq!(got, want, "vcvtb.f32.f16 {half:#06x} fpscr {fpscr:#x}");
        let got = emulate(VCVTT_F32_F16, (half as u32) << 16 | 0x1234, old, fpscr);
        assert_eq!(got, want, "vcvtt.f32.f16 {half:#06x} fpscr {fpscr:#x}");
        checked += 4;
    }
    eprintln!("{checked} half-precision conversions checked");
}
