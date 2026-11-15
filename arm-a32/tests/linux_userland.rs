//! Regressions from booting Alpine Linux on the Orange Pi One: abort writeback, CPS,
//! decode-space guards, media/DSP instructions, VFP and TLB maintenance.
//! Encodings come from `arm-none-eabi-as -march=armv7ve -mfpu=vfpv4`.
use arm_a32::{Cpu, MODE_ABT, MODE_SVC, MODE_SYS};
use emu_core::{Bus, Core, FlatRam, Trap};

const Q: u32 = 1 << 27;

fn machine(instructions: &[u32]) -> (Cpu, FlatRam) {
    let mut ram = FlatRam::new(0, 0x5000);
    for (index, instruction) in instructions.iter().enumerate() {
        ram.write32(0x1000 + index as u64 * 4, *instruction).unwrap();
    }
    let mut cpu = Cpu::new();
    cpu.r[15] = 0x1000;
    (cpu, ram)
}

fn run(cpu: &mut Cpu, ram: &mut FlatRam, count: usize) {
    for _ in 0..count {
        cpu.step(ram).unwrap();
    }
}

#[test]
fn aborted_post_indexed_store_leaves_the_base_register_unchanged() {
    let (mut cpu, mut ram) = machine(&[0xe4c0_2001]); // strb r2, [r0], #1
    cpu.r[0] = 0x9000; // outside RAM: external abort
    assert!(matches!(cpu.step(&mut ram), Err(Trap::Exception(..))));
    assert_eq!(cpu.r[0], 0x9000, "base written back despite the abort");
    assert_eq!(cpu.cpsr & 0x1f, MODE_ABT);
    assert_eq!(cpu.r[14], 0x1008);
    assert_eq!(cpu.dfar, 0x9000);
    assert_ne!(cpu.dfsr & (1 << 11), 0, "WnR");
}

#[test]
fn cps_changes_a_i_f_and_mode() {
    let (mut cpu, mut ram) = machine(&[0xf108_0100, 0xf10c_00c0, 0xf102_001f]);
    cpu.cpsr = 1 << 8 | MODE_SVC; // A set, I/F clear
    run(&mut cpu, &mut ram, 1); // cpsie a
    assert_eq!(cpu.cpsr & 0x1c0, 0);
    run(&mut cpu, &mut ram, 1); // cpsid if
    assert_eq!(cpu.cpsr & 0x1c0, 0xc0);
    run(&mut cpu, &mut ram, 1); // cps #0x1f
    assert_eq!(cpu.cpsr & 0x1f, MODE_SYS);
}

#[test]
fn multiply_space_and_misc_space_are_not_data_processing() {
    // mls r0, r1, r2, r3 used to execute as RSB; smc as a flag-setting compare.
    let (mut cpu, mut ram) = machine(&[0xe060_3291, 0xe160_0070]);
    (cpu.r[1], cpu.r[2], cpu.r[3]) = (3, 5, 100);
    run(&mut cpu, &mut ram, 1);
    assert_eq!(cpu.r[0], 85);
    let flags = cpu.cpsr;
    assert!(matches!(cpu.step(&mut ram), Err(Trap::Unimplemented(0x1004, 0xe160_0070))));
    assert_eq!(cpu.cpsr, flags);

    let (mut cpu, mut ram) = machine(&[0xe160_0070]);
    cpu.firmware_calls_to_host = true;
    assert_eq!(cpu.step(&mut ram), Err(Trap::HostCall(0)));
}

#[test]
fn saturating_and_dsp_instructions() {
    let (mut cpu, mut ram) = machine(&[
        0xe106_4055, // qadd r4, r5, r6
        0xe6e8_3011, // usat r3, #8, r1
        0xe6a7_3011, // ssat r3, #8, r1
    ]);
    (cpu.r[5], cpu.r[6], cpu.r[1]) = (0x7fff_ffff, 1, 300);
    run(&mut cpu, &mut ram, 2);
    assert_eq!(cpu.r[4], 0x7fff_ffff);
    assert_ne!(cpu.cpsr & Q, 0);
    assert_eq!(cpu.r[3], 255);
    cpu.r[1] = -300i32 as u32;
    run(&mut cpu, &mut ram, 1);
    assert_eq!(cpu.r[3], -128i32 as u32);

    let (mut cpu, mut ram) = machine(&[
        0xe100_3281, // smlabb r0, r1, r2, r3
        0xe750_f211, // smmul r0, r1, r2
        0xe041_0392, // umaal r0, r1, r2, r3
    ]);
    (cpu.r[1], cpu.r[2], cpu.r[3]) = (0x0001_fffe, 0x0005_0003, 10);
    run(&mut cpu, &mut ram, 1);
    assert_eq!(cpu.r[0], 4); // -2 * 3 + 10
    (cpu.r[1], cpu.r[2]) = (0x4000_0000, 0x4000_0000);
    run(&mut cpu, &mut ram, 1);
    assert_eq!(cpu.r[0], 0x1000_0000);
    (cpu.r[0], cpu.r[1], cpu.r[2], cpu.r[3]) = (1, 2, u32::MAX, u32::MAX);
    run(&mut cpu, &mut ram, 1);
    assert_eq!((cpu.r[0], cpu.r[1]), (4, 0xffff_fffe));
}

#[test]
fn parallel_add_sets_ge_and_sel_uses_it() {
    let (mut cpu, mut ram) = machine(&[0xe655_4f96, 0xe685_7fb6]); // uadd8 r4,r5,r6; sel r7,r5,r6
    (cpu.r[5], cpu.r[6]) = (0x80ff_0102, 0x8001_0203);
    run(&mut cpu, &mut ram, 2);
    assert_eq!(cpu.r[4], 0x0000_0305);
    assert_eq!((cpu.cpsr >> 16) & 0xf, 0b1100);
    assert_eq!(cpu.r[7], 0x80ff_0203);
}

#[test]
fn bit_and_pack_operations() {
    let (mut cpu, mut ram) = machine(&[
        0xe6ff_0f31, // rbit r0, r1
        0xe681_0812, // pkhbt r0, r1, r2, lsl #16
        0xe6cf_0071, // uxtb16 r0, r1
    ]);
    cpu.r[1] = 1;
    run(&mut cpu, &mut ram, 1);
    assert_eq!(cpu.r[0], 0x8000_0000);
    (cpu.r[1], cpu.r[2]) = (0x1111_aaaa, 0x0000_bbbb);
    run(&mut cpu, &mut ram, 1);
    assert_eq!(cpu.r[0], 0xbbbb_aaaa);
    cpu.r[1] = 0x1122_3344;
    run(&mut cpu, &mut ram, 1);
    assert_eq!(cpu.r[0], 0x0022_0044);
}

#[test]
fn vfp_arithmetic_conversion_compare_and_transfers() {
    let (mut cpu, mut ram) = machine(&[
        0xeee8_0a10, // vmsr fpexc, r0
        0xeeb7_0b00, // vmov.f64 d0, #1.0
        0xeeb0_2a04, // vmov.f32 s4, #2.5
        0xee30_1b00, // vadd.f64 d1, d0, d0
        0xeeb7_3ac2, // vcvt.f64.f32 d3, s4
        0xee21_4b03, // vmul.f64 d4, d1, d3
        0xeebd_5bc4, // vcvt.s32.f64 s10, d4
        0xee15_1a10, // vmov r1, s10
        0xeeb4_4b41, // vcmp.f64 d4, d1
        0xeef1_fa10, // vmrs APSR_nzcv, fpscr
        0xed82_4b00, // vstr d4, [r2]
        0xed92_5b00, // vldr d5, [r2]
        0xeea0_5b00, // vfma.f64 d5, d0, d0
        0xec54_3b15, // vmov r3, r4, d5
    ]);
    cpu.cpacr = 0xf << 20;
    (cpu.r[0], cpu.r[2]) = (0x4000_0000, 0x3000);
    run(&mut cpu, &mut ram, 14);
    assert_eq!(cpu.r[1], 5);
    assert_eq!(cpu.cpsr >> 28, 0b0010, "5.0 > 2.0: C only");
    assert_eq!(ram.read64(0x3000).unwrap(), 5.0f64.to_bits());
    assert_eq!((cpu.r[3], cpu.r[4]), (0, (6.0f64.to_bits() >> 32) as u32));
}

#[test]
fn vfp_is_undefined_until_enabled_so_linux_can_enable_it_lazily() {
    let (mut cpu, mut ram) = machine(&[0xee30_1b00]); // vadd.f64 d1, d0, d0
    cpu.cpacr = 0xf << 20;
    assert!(matches!(cpu.step(&mut ram), Err(Trap::Unimplemented(..))));
    cpu.undef_to_guest = true;
    cpu.r[15] = 0x1000;
    assert!(matches!(cpu.step(&mut ram), Err(Trap::Exception(0, _))));
    assert_eq!(cpu.r[15], 0x04, "undefined-instruction vector");
}

#[test]
fn ldm_with_s_bit_and_no_pc_loads_user_registers() {
    // ldm sp, {sp, lr}^ ; cps #0x1f
    let (mut cpu, mut ram) = machine(&[0xe8dd_6000, 0xf102_001f]);
    cpu.r[13] = 0x3000;
    ram.write32(0x3000, 0xaaaa_0000).unwrap();
    ram.write32(0x3004, 0xbbbb_0000).unwrap();
    run(&mut cpu, &mut ram, 1);
    assert_eq!(cpu.r[13], 0x3000, "SVC stack pointer untouched");
    run(&mut cpu, &mut ram, 1);
    assert_eq!((cpu.r[13], cpu.r[14]), (0xaaaa_0000, 0xbbbb_0000));
}

#[test]
fn translations_are_cached_until_tlb_maintenance() {
    // ldr r0, [r1]; ldr r0, [r1]; mcr p15, 0, r0, c8, c7, 0 (TLBIALL); ldr r0, [r1]
    let (mut cpu, mut ram) = machine(&[0xe591_0000, 0xe591_0000, 0xee08_0f17, 0xe591_0000]);
    ram.write32(0x4000, 0x0000_0c02).unwrap(); // VA 0 MiB -> PA 0, section, full access
    ram.write32(0x4004, 0x0000_0c02).unwrap(); // VA 1 MiB -> PA 0
    ram.write32(0x0100, 0x1234_5678).unwrap();
    cpu.ttbr0 = 0x4000;
    cpu.dacr = 1;
    cpu.sctlr |= 1;
    cpu.tlb_flush();
    cpu.r[1] = 0x0010_0100;
    run(&mut cpu, &mut ram, 1);
    assert_eq!(cpu.r[0], 0x1234_5678);
    ram.write32(0x4004, 0).unwrap(); // unmap without maintenance: stale entry still used
    run(&mut cpu, &mut ram, 1);
    assert_eq!(cpu.r[0], 0x1234_5678);
    run(&mut cpu, &mut ram, 1); // TLBIALL
    assert!(matches!(cpu.step(&mut ram), Err(Trap::Exception(..))));
    assert_eq!(cpu.dfsr & 0x40f, 0x05, "section translation fault");
}
