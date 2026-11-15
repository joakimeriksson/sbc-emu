use arm_a32::{Cpu, CORTEX_A12, CORTEX_A7};
use emu_core::{Bus, Core, FlatRam, Trap};

fn run(cpu: &mut Cpu, words: &[u32]) -> FlatRam {
    let mut ram = FlatRam::new(0, 0x4000);
    for (n, word) in words.iter().enumerate() {
        ram.write32(0x1000 + 4 * n as u64, *word).unwrap();
    }
    cpu.r[15] = 0x1000;
    ram
}

const READ_IDS: [u32; 7] = [
    0xee10_0f10, // mrc p15, 0, r0, c0, c0, 0 (MIDR)
    0xee10_1f30, // mrc p15, 0, r1, c0, c0, 1 (CTR)
    0xee10_2f12, // mrc p15, 0, r2, c0, c2, 0 (ID_ISAR0)
    0xee10_3f92, // mrc p15, 0, r3, c0, c2, 4 (ID_ISAR4)
    0xee30_4f10, // mrc p15, 1, r4, c0, c0, 0 (CCSIDR, CSSELR = 0: L1 data)
    0xee39_5f50, // mrc p15, 1, r5, c9, c0, 2 (L2CTLR)
    0xeef0_6a10, // vmrs r6, fpsid
];

#[test]
fn default_model_is_the_cortex_a7() {
    let mut cpu = Cpu::new();
    assert_eq!(cpu.model, CORTEX_A7);
    let words = READ_IDS;
    cpu.cpacr = 0xf << 20;
    let mut ram = run(&mut cpu, &words);
    for _ in 0..words.len() {
        cpu.step(&mut ram).unwrap();
    }
    assert_eq!(
        cpu.r[..7],
        [0x410f_c075, 0x8444_8003, 0x0210_1110, 0x1001_1142, 0x701f_e00a, 0x0300_0000, 0x4102_3075]
    );
}

#[test]
fn set_model_switches_every_identity_register() {
    let mut cpu = Cpu::new();
    cpu.set_model(CORTEX_A12);
    assert_eq!(cpu.midr, 0x410f_c0d1);
    let words = READ_IDS;
    cpu.cpacr = 0xf << 20;
    let mut ram = run(&mut cpu, &words);
    for _ in 0..words.len() {
        cpu.step(&mut ram).unwrap();
    }
    assert_eq!(
        cpu.r[..7],
        [0x410f_c0d1, 0x8444_8004, 0x0210_1110, 0x0001_1142, 0x700f_e01a, 0x0300_0009, 0x4103_30d0]
    );
}

#[test]
fn c15_diagnostic_registers_exist_only_on_the_a12() {
    let words = [
        0xe3a0_0a01, // mov r0, #0x1000
        0xee0f_0f30, // mcr p15, 0, r0, c15, c0, 1 (diagnostic)
        0xe3a0_0002, // mov r0, #2
        0xee0f_0f50, // mcr p15, 0, r0, c15, c0, 2 (internal feature)
        0xee1f_1f30, // mrc p15, 0, r1, c15, c0, 1
        0xee1f_2f50, // mrc p15, 0, r2, c15, c0, 2
    ];
    let mut a12 = Cpu::new();
    a12.set_model(CORTEX_A12);
    let mut ram = run(&mut a12, &words);
    for _ in 0..words.len() {
        a12.step(&mut ram).unwrap();
    }
    assert_eq!((a12.r[1], a12.r[2]), (0x1000, 2));

    let mut a7 = Cpu::new();
    let mut ram = run(&mut a7, &words);
    a7.step(&mut ram).unwrap();
    assert_eq!(a7.step(&mut ram), Err(Trap::Unimplemented(0x1004, words[1])));
}
