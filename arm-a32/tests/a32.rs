use arm_a32::{Cpu, MODE_IRQ, MODE_SVC};
use emu_core::{Bus, Core, FlatRam, Trap};

fn machine(words: &[u32]) -> (Cpu, FlatRam) {
    let mut ram = FlatRam::new(0, 0x4000);
    for (n, word) in words.iter().enumerate() {
        ram.write32(0x1000 + 4 * n as u64, *word).unwrap();
    }
    let mut cpu = Cpu::new();
    cpu.r[15] = 0x1000;
    (cpu, ram)
}

#[test]
fn data_processing_conditions_and_multiply() {
    let (mut cpu, mut ram) = machine(&[
        0xe3a0_0001, // mov r0, #1
        0xe280_1002, // add r1, r0, #2
        0xe351_0003, // cmp r1, #3
        0x03a0_2007, // moveq r2, #7
        0x13a0_2009, // movne r2, #9 (not executed)
        0xe003_0190, // mul r3, r1, r0
        0xe120_0070, // bkpt #0
    ]);
    for _ in 0..6 {
        cpu.step(&mut ram).unwrap();
    }
    assert_eq!(cpu.r[1], 3);
    assert_eq!(cpu.r[2], 7);
    assert_eq!(cpu.r[3], 3);
    assert_eq!(cpu.step(&mut ram), Err(Trap::Brk(0)));
    assert_eq!(cpu.instret(), 7);
}

#[test]
fn arithmetic_sets_signed_overflow_and_comparison_flags() {
    let (mut cpu, mut ram) = machine(&[
        0xe300_0000, // movw r0, #0
        0xe344_0000, // movt r0, #0x4000
        0xe090_1000, // adds r1, r0, r0
        0xe151_0001, // cmp r1, r1
        0xe120_0070, // bkpt #0
    ]);
    cpu.step(&mut ram).unwrap();
    cpu.step(&mut ram).unwrap();
    assert_eq!(cpu.r[0], 0x4000_0000);
    cpu.step(&mut ram).unwrap();
    assert_eq!(cpu.r[1], 0x8000_0000);
    assert_ne!(cpu.cpsr & (1 << 31), 0, "negative");
    assert_ne!(cpu.cpsr & (1 << 28), 0, "overflow");
    assert_eq!(cpu.cpsr & (1 << 29), 0, "no carry");
    cpu.step(&mut ram).unwrap();
    assert_ne!(cpu.cpsr & (1 << 30), 0, "zero");
    assert_ne!(cpu.cpsr & (1 << 29), 0, "carry");
    assert_eq!(cpu.step(&mut ram), Err(Trap::Brk(0)));
}

#[test]
fn word_byte_memory_and_branch() {
    let (mut cpu, mut ram) = machine(&[
        0xe300_0100, // movw r0, #0x100
        0xe3a0_102a, // mov r1, #42
        0xe580_1004, // str r1, [r0, #4]
        0xe590_2004, // ldr r2, [r0, #4]
        0xe5c0_1007, // strb r1, [r0, #7]
        0xe5d0_3007, // ldrb r3, [r0, #7]
        0xea00_0000, // b +0: skip one instruction
        0xe3a0_4063, // mov r4, #99 (skipped)
        0xe3a0_4005, // mov r4, #5
        0xe120_0070, // bkpt #0
    ]);
    for _ in 0..8 {
        cpu.step(&mut ram).unwrap();
    }
    assert_eq!(cpu.r[2], 42);
    assert_eq!(cpu.r[3], 42);
    assert_eq!(cpu.r[4], 5);
    assert_eq!(ram.read32(0x104).unwrap(), 0x2a00_002a);
    assert_eq!(ram.read8(0x107).unwrap(), 42);
    assert_eq!(cpu.step(&mut ram), Err(Trap::Brk(0)));
}

#[test]
fn svc_and_irq_enter_their_vector_and_banked_link_register() {
    let (mut cpu, mut ram) = machine(&[0xef00_0000]);
    cpu.vbar = 0x2000;
    ram.write32(0x2008, 0xe120_0070).unwrap();
    assert_eq!(cpu.step(&mut ram), Err(Trap::Exception(0x15, 1)));
    assert_eq!(cpu.pc(), 0x2008);
    assert_eq!(cpu.r[14], 0x1004);
    assert_eq!(cpu.cpsr & 0x1f, MODE_SVC);
    assert_eq!(cpu.step(&mut ram), Err(Trap::Brk(0)));

    cpu.r[15] = 0x1000;
    cpu.cpsr &= !(1 << 7);
    cpu.set_irq(true, false);
    ram.write32(0x2018, 0xe120_0070).unwrap();
    assert_eq!(cpu.step(&mut ram), Err(Trap::Interrupt(0, 1)));
    assert_eq!(cpu.pc(), 0x2018);
    assert_eq!(cpu.r[14], 0x1004);
    assert_eq!(cpu.cpsr & 0x1f, MODE_IRQ);
    assert_ne!(cpu.cpsr & (1 << 7), 0);
}

#[test]
fn cp15_identity_wait_and_unknown_instruction() {
    let (mut cpu, mut ram) = machine(&[
        0xee10_0f10, // mrc p15, 0, r0, c0, c0, 0 (MIDR)
        0xe320_f003, // wfi
        0xe3a0_0000, // mov r0, #0
    ]);
    cpu.cpsr |= 1 << 30;
    cpu.step(&mut ram).unwrap();
    assert_eq!(cpu.r[0], 0x410f_c075);
    assert_eq!(cpu.step(&mut ram), Err(Trap::Wait));
    assert_eq!(cpu.pc(), 0x1008);
    cpu.step(&mut ram).unwrap();

    let udf_pc = cpu.pc();
    ram.write32(udf_pc, 0xe7f0_00f0).unwrap(); // udf #0
    let retired = cpu.instret();
    assert_eq!(
        cpu.step(&mut ram),
        Err(Trap::Unimplemented(udf_pc, 0xe7f0_00f0))
    );
    assert_eq!(cpu.pc(), udf_pc);
    assert_eq!(cpu.instret(), retired);
}

#[test]
fn physical_timer_cp15_accesses_counter_compare_and_mask() {
    let (mut cpu, mut ram) = machine(&[
        0xee1e_0f10, // mrc p15, 0, r0, c14, c0, 0 (CNTFRQ)
        0xe3a0_1005, // mov r1, #5
        0xee0e_1f12, // mcr p15, 0, r1, c14, c2, 0 (CNTP_TVAL)
        0xe3a0_1001, // mov r1, #1
        0xee0e_1f32, // mcr p15, 0, r1, c14, c2, 1 (CNTP_CTL)
        0xec53_2f0e, // mrrc p15, 0, r2, r3, c14 (CNTPCT)
        0xec55_4f2e, // mrrc p15, 2, r4, r5, c14 (CNTP_CVAL)
        0xee1e_8f32, // mrc p15, 0, r8, c14, c2, 1 (CNTP_CTL)
        0xee1e_7f12, // mrc p15, 0, r7, c14, c2, 0 (CNTP_TVAL)
        0xe120_0070, // bkpt #0
    ]);
    cpu.cntpct = 0x1_0000_0064;
    for _ in 0..9 {
        cpu.step(&mut ram).unwrap();
    }
    assert_eq!(cpu.r[0], 24_000_000);
    assert_eq!((cpu.r[3], cpu.r[2]), (1, 0x64));
    assert_eq!((cpu.r[5], cpu.r[4]), (1, 0x69));
    assert_eq!(cpu.r[8], 1); // enabled, ISTATUS clear
    assert_eq!(cpu.r[7], 5);
    assert!(!cpu.physical_timer_irq());

    cpu.advance_time(5);
    assert!(cpu.physical_timer_irq());
    ram.write32(cpu.pc(), 0xee1e_8f32).unwrap();
    cpu.step(&mut ram).unwrap();
    assert_eq!(cpu.r[8], 5); // enabled and ISTATUS set

    ram.write32(cpu.pc(), 0xe3a0_1003).unwrap();
    cpu.step(&mut ram).unwrap();
    ram.write32(cpu.pc(), 0xee0e_1f32).unwrap();
    cpu.step(&mut ram).unwrap();
    assert!(!cpu.physical_timer_irq());
    ram.write32(cpu.pc(), 0xee1e_8f32).unwrap();
    cpu.step(&mut ram).unwrap();
    assert_eq!(cpu.r[8], 7); // masking the output does not clear ISTATUS

    cpu.r[4] = 0x80;
    cpu.r[5] = 0;
    ram.write32(cpu.pc(), 0xec45_4f2e).unwrap(); // mcrr p15, 2, r4, r5, c14
    cpu.step(&mut ram).unwrap();
    assert_eq!(cpu.cntp_cval, 0x80);
    ram.write32(cpu.pc(), 0xec57_6f2e).unwrap(); // mrrc p15, 2, r6, r7, c14
    cpu.step(&mut ram).unwrap();
    assert_eq!((cpu.r[7], cpu.r[6]), (0, 0x80));
}

#[test]
fn stm_with_the_pc_in_the_list_stores_the_instruction_address_plus_8() {
    // APCS frame prologue of Linux kernels built with frame pointers.
    let (mut cpu, mut ram) = machine(&[
        0xe1a0_c00d, // mov ip, sp
        0xe92d_d800, // push {fp, ip, lr, pc}
        0xe120_0070, // bkpt #0
    ]);
    cpu.r[13] = 0x3000;
    cpu.r[11] = 0x11;
    cpu.r[14] = 0x14;
    cpu.step(&mut ram).unwrap();
    cpu.step(&mut ram).unwrap();
    assert_eq!(cpu.r[13], 0x3000 - 16);
    assert_eq!(ram.read32(0x2ff0).unwrap(), 0x11, "fp");
    assert_eq!(ram.read32(0x2ff4).unwrap(), 0x3000, "ip");
    assert_eq!(ram.read32(0x2ff8).unwrap(), 0x14, "lr");
    assert_eq!(ram.read32(0x2ffc).unwrap(), 0x1004 + 8, "pc");
    assert_eq!(cpu.step(&mut ram), Err(Trap::Brk(0)));
}
