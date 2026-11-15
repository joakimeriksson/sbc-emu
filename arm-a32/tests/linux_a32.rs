use arm_a32::{Cpu, MODE_SVC};
use emu_core::{Bus, Core, FlatRam, Trap};

fn coprocessor(cp: u32, read: bool, opc1: u32, crn: u32, rt: u32, crm: u32, opc2: u32) -> u32 {
    0xee00_0010
        | cp << 8
        | u32::from(read) << 20
        | opc1 << 21
        | crn << 16
        | rt << 12
        | opc2 << 5
        | crm
}

fn cp15(read: bool, opc1: u32, crn: u32, rt: u32, crm: u32, opc2: u32) -> u32 {
    coprocessor(15, read, opc1, crn, rt, crm, opc2)
}

fn machine(instructions: &[u32]) -> (Cpu, FlatRam) {
    let mut ram = FlatRam::new(0, 0x5000);
    for (index, instruction) in instructions.iter().enumerate() {
        ram.write32(0x1000 + index as u64 * 4, *instruction)
            .unwrap();
    }
    let mut cpu = Cpu::new();
    cpu.r[15] = 0x1000;
    (cpu, ram)
}

#[test]
fn msr_preserves_unselected_state_and_writes_spsr_fields() {
    let (mut cpu, mut ram) = machine(&[
        0xe10f_0000, // mrs r0, cpsr
        0xe3c0_0080, // bic r0, r0, #0x80
        0xe121_f000, // msr cpsr_c, r0
        0xe10f_1000, // mrs r1, cpsr
        0xe321_f0d3, // msr cpsr_c, #0xd3
        0xe10f_2000, // mrs r2, cpsr
        0xe300_305f, // movw r3, #0x5f
        0xe16f_f003, // msr spsr_cxsf, r3
        0xe14f_4000, // mrs r4, spsr
        0xe120_0070, // bkpt #0
    ]);
    for _ in 0..9 {
        cpu.step(&mut ram).unwrap();
    }
    assert_eq!(cpu.r[1] & 0x80, 0);
    assert_eq!(cpu.r[1] & 0x1f, MODE_SVC);
    assert_eq!(cpu.r[2] & 0xff, 0xd3);
    assert_eq!(cpu.r[4], 0x5f);
}

#[test]
fn load_store_multiple_cover_all_four_addressing_modes() {
    // opcode, load?, r0 address, r3 address, final r2; initial r2 = 0x2200.
    let cases = [
        (0xe922_0009, false, 0x21f8, 0x21fc, 0x21f8), // stmdb
        (0xe822_0009, false, 0x21fc, 0x2200, 0x21f8), // stmda
        (0xe9a2_0009, false, 0x2204, 0x2208, 0x2208), // stmib
        (0xe8a2_0009, false, 0x2200, 0x2204, 0x2208), // stmia
        (0xe932_0009, true, 0x21f8, 0x21fc, 0x21f8),  // ldmdb
        (0xe832_0009, true, 0x21fc, 0x2200, 0x21f8),  // ldmda
        (0xe9b2_0009, true, 0x2204, 0x2208, 0x2208),  // ldmib
        (0xe8b2_0009, true, 0x2200, 0x2204, 0x2208),  // ldmia
    ];
    for &(opcode, load, first, second, final_base) in &cases {
        let (mut cpu, mut ram) = machine(&[opcode, 0xe120_0070]);
        cpu.r[2] = 0x2200;
        if load {
            ram.write32(first, 0x1122_3344).unwrap();
            ram.write32(second, 0x5566_7788).unwrap();
        } else {
            cpu.r[0] = 0x1122_3344;
            cpu.r[3] = 0x5566_7788;
        }
        cpu.step(&mut ram).unwrap();
        if !load {
            assert_eq!(ram.read32(first), Ok(0x1122_3344), "{opcode:#x}");
            assert_eq!(ram.read32(second), Ok(0x5566_7788), "{opcode:#x}");
        }
        assert_eq!(cpu.r[0], 0x1122_3344, "{opcode:#x}");
        assert_eq!(cpu.r[3], 0x5566_7788, "{opcode:#x}");
        assert_eq!(cpu.r[2], final_base, "{opcode:#x}");
    }
}

#[test]
fn ldm_can_load_pc_and_restore_the_stack_pointer() {
    let (mut cpu, mut ram) = machine(&[0xe8bd_80f0, 0xe120_0070]); // pop {r4-r7, pc}
    cpu.r[13] = 0x2000;
    for (index, value) in [4, 5, 6, 7, 0x3000].into_iter().enumerate() {
        ram.write32(0x2000 + index as u64 * 4, value).unwrap();
    }
    ram.write32(0x3000, 0xe120_0070).unwrap();
    cpu.step(&mut ram).unwrap();
    assert_eq!(&cpu.r[4..8], &[4, 5, 6, 7]);
    assert_eq!(cpu.r[13], 0x2014);
    assert_eq!(cpu.pc(), 0x3000);
    assert_eq!(cpu.step(&mut ram), Err(Trap::Brk(0)));
}

#[test]
fn halfword_and_signed_transfers_use_split_offsets_and_writeback() {
    let (mut cpu, mut ram) = machine(&[
        0xe1c1_20b4, // strh r2, [r1, #4]
        0xe0d1_30b4, // ldrh r3, [r1], #4
        0xe151_30d4, // ldrsb r3, [r1, #-4]
        0xe1f1_30f4, // ldrsh r3, [r1, #4]!
    ]);
    cpu.r[1] = 0x2000;
    cpu.r[2] = 0x1122_3344;
    cpu.step(&mut ram).unwrap();
    assert_eq!(ram.read16(0x2004), Ok(0x3344));

    ram.write16(0x2000, 0x3344).unwrap();
    cpu.step(&mut ram).unwrap();
    assert_eq!(cpu.r[3], 0x3344);
    assert_eq!(cpu.r[1], 0x2004);

    cpu.r[1] = 0x2000;
    ram.write8(0x1ffc, 0x80).unwrap();
    cpu.step(&mut ram).unwrap();
    assert_eq!(cpu.r[3], 0xffff_ff80);
    assert_eq!(cpu.r[1], 0x2000);

    ram.write16(0x2004, 0x8001).unwrap();
    cpu.step(&mut ram).unwrap();
    assert_eq!(cpu.r[3], 0xffff_8001);
    assert_eq!(cpu.r[1], 0x2004);
}

#[test]
fn bitfield_extend_reverse_hints_and_blx_are_executable() {
    let (mut cpu, mut ram) = machine(&[
        0xe30c_1def, // movw r1, #0xcdef
        0xe348_19ab, // movt r1, #0x89ab
        0xe16f_0f11, // clz r0, r1
        0xe7e7_0251, // ubfx r0, r1, #4, #8
        0xe7a7_0c51, // sbfx r0, r1, #24, #8
        0xe6bf_0f31, // rev r0, r1
        0xe6bf_0fb1, // rev16 r0, r1
        0xe6ef_0071, // uxtb r0, r1
        0xe6ff_0071, // uxth r0, r1
        0xe6af_0071, // sxtb r0, r1
        0xe6bf_0071, // sxth r0, r1
        0xf57f_f05f, // dmb sy
        0xf57f_f04f, // dsb sy
        0xf57f_f06f, // isb sy
        0xf5d0_f010, // pld [r0, #16]
        0xf550_f010, // pld [r0, #-16]
        0xf590_f000, // pldw [r0]
        0xf590_f010, // pldw [r0, #16]
        0xe12f_ff32, // blx r2
        0xe120_0070, // not reached
    ]);
    ram.write32(0x3000, 0xe120_0070).unwrap();
    cpu.step(&mut ram).unwrap();
    cpu.step(&mut ram).unwrap();
    assert_eq!(cpu.r[1], 0x89ab_cdef);

    for expected in [
        0,
        0xde,
        0xffff_ff89,
        0xefcd_ab89,
        0xab89_efcd,
        0xef,
        0xcdef,
        0xffff_ffef,
        0xffff_cdef,
    ] {
        cpu.step(&mut ram).unwrap();
        assert_eq!(cpu.r[0], expected);
    }
    for _ in 0..7 {
        cpu.step(&mut ram).unwrap();
    }
    cpu.r[2] = 0x3000;
    let link = cpu.pc() + 4;
    cpu.step(&mut ram).unwrap();
    assert_eq!(cpu.pc(), 0x3000);
    assert_eq!(cpu.r[14], link as u32);
    assert_eq!(cpu.step(&mut ram), Err(Trap::Brk(0)));
}

#[test]
fn long_multiply_accumulate_and_divide_match_armv7_results() {
    let (mut cpu, mut ram) = machine(&[
        0xe081_0392, // umull r0, r1, r2, r3
        0xe0a1_0392, // umlal r0, r1, r2, r3
        0xe0c1_0392, // smull r0, r1, r2, r3
        0xe0e1_0392, // smlal r0, r1, r2, r3
        0xe710_f211, // sdiv r0, r1, r2
        0xe730_f211, // udiv r0, r1, r2
    ]);
    cpu.r[2] = u32::MAX;
    cpu.r[3] = 2;
    cpu.step(&mut ram).unwrap();
    assert_eq!((cpu.r[1], cpu.r[0]), (1, 0xffff_fffe));
    cpu.step(&mut ram).unwrap();
    assert_eq!((cpu.r[1], cpu.r[0]), (3, 0xffff_fffc));
    cpu.step(&mut ram).unwrap();
    assert_eq!((cpu.r[1], cpu.r[0]), (u32::MAX, 0xffff_fffe));
    cpu.step(&mut ram).unwrap();
    assert_eq!((cpu.r[1], cpu.r[0]), (u32::MAX, 0xffff_fffc));

    cpu.r[1] = -42i32 as u32;
    cpu.r[2] = 5;
    cpu.step(&mut ram).unwrap();
    assert_eq!(cpu.r[0], -8i32 as u32);
    cpu.step(&mut ram).unwrap();
    assert_eq!(cpu.r[0], (-42i32 as u32) / 5);

    cpu.r[2] = 0;
    ram.write32(cpu.pc(), 0xe710_f211).unwrap();
    cpu.step(&mut ram).unwrap();
    assert_eq!(cpu.r[0], 0);
    cpu.r[1] = i32::MIN as u32;
    cpu.r[2] = -1i32 as u32;
    ram.write32(cpu.pc(), 0xe710_f211).unwrap();
    cpu.step(&mut ram).unwrap();
    assert_eq!(cpu.r[0], i32::MIN as u32);
}

#[test]
fn cortex_a7_cache_identity_and_thumbee_setup_are_consistent() {
    let instructions = [
        cp15(true, 0, 0, 0, 0, 1),              // CTR -> r0
        cp15(true, 1, 0, 1, 0, 1),              // CLIDR -> r1
        cp15(true, 1, 0, 2, 0, 0),              // CCSIDR (L1 data) -> r2
        0xe3a0_3001,                            // mov r3, #1
        cp15(false, 2, 0, 3, 0, 0),             // CSSELR = instruction
        cp15(true, 1, 0, 4, 0, 0),              // CCSIDR (L1 instruction) -> r4
        0xe3a0_3002,                            // mov r3, #2
        cp15(false, 2, 0, 3, 0, 0),             // CSSELR = L2 unified
        cp15(true, 1, 0, 5, 0, 0),              // CCSIDR (L2) -> r5
        cp15(true, 0, 0, 6, 1, 0),              // ID_PFR0 -> r6
        cp15(true, 0, 0, 7, 2, 0),              // ID_ISAR0 -> r7
        cp15(true, 0, 0, 8, 0, 5),              // MPIDR -> r8
        0xe3a0_a001,                            // mov r10, #1
        coprocessor(14, false, 6, 0, 10, 0, 0), // TEECR = 1
        coprocessor(14, true, 6, 0, 9, 0, 0),   // TEECR -> r9
        coprocessor(14, false, 6, 1, 10, 0, 0), // TEEHBR = 1
        coprocessor(14, true, 6, 1, 11, 0, 0),  // TEEHBR -> r11
    ];
    let (mut cpu, mut ram) = machine(&instructions);
    for _ in 0..instructions.len() {
        cpu.step(&mut ram).unwrap();
    }
    assert_eq!(cpu.r[0], 0x8444_8003);
    assert_eq!(cpu.r[1], 0x0a20_0023);
    assert_eq!(cpu.r[2], 0x701f_e00a);
    assert_eq!(cpu.r[4], 0x201f_e00a);
    assert_eq!(cpu.r[5], 0x711f_e07a);
    assert_eq!(cpu.r[6], 0x1131);
    assert_eq!(cpu.r[7], 0x0210_1110);
    assert_eq!(cpu.r[8], 0x8000_0000);
    assert_eq!((cpu.r[9], cpu.r[11]), (1, 1));
}

#[test]
fn cp15_system_registers_round_trip_and_maintenance_retires() {
    let cases = [
        (0, 1, 0, 0, 0x1234),  // SCTLR
        (0, 1, 0, 1, 0x1234),  // ACTLR
        (0, 1, 0, 2, 0x1234),  // CPACR
        (0, 2, 0, 0, 0x1234),  // TTBR0
        (0, 2, 0, 1, 0x1234),  // TTBR1
        (0, 2, 0, 2, 0x1234),  // TTBCR
        (0, 3, 0, 0, 0x1234),  // DACR
        (0, 5, 0, 0, 0x1234),  // DFSR
        (0, 5, 0, 1, 0x1234),  // IFSR
        (0, 6, 0, 0, 0x1234),  // DFAR
        (0, 6, 0, 2, 0x1234),  // IFAR
        (0, 10, 2, 0, 0x1234), // PRRR
        (0, 10, 2, 1, 0x1234), // NMRR
        (0, 12, 0, 0, 0x1220), // VBAR alignment
        (0, 13, 0, 0, 0x1234), // FCSEIDR
        (0, 13, 0, 1, 0x1234), // CONTEXTIDR
        (0, 13, 0, 2, 0x1234), // TPIDRURW
        (0, 13, 0, 3, 0x1234), // TPIDRURO
        (0, 13, 0, 4, 0x1234), // TPIDRPRW
    ];
    for &(opc1, crn, crm, opc2, expected) in &cases {
        let (mut cpu, mut ram) = machine(&[
            0xe301_0234, // movw r0, #0x1234
            cp15(false, opc1, crn, 0, crm, opc2),
            cp15(true, opc1, crn, 1, crm, opc2),
        ]);
        for _ in 0..3 {
            cpu.step(&mut ram).unwrap();
        }
        assert_eq!(cpu.r[1], expected, "c{crn},c{crm},{opc2}");
    }

    let (mut cpu, mut ram) = machine(&[
        cp15(false, 0, 7, 0, 14, 2), // clean/invalidate D-cache by set/way
        cp15(false, 0, 8, 0, 7, 0),  // invalidate TLBs
    ]);
    cpu.step(&mut ram).unwrap();
    cpu.step(&mut ram).unwrap();
    assert_eq!(cpu.instret(), 2);
}
