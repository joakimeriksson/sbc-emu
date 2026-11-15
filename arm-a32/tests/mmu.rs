use arm_a32::{Cpu, MODE_USR};
use emu_core::{Bus, Core, FlatRam, Trap};

const L1: u32 = 0x10000;
const CODE_VA: u32 = 0x0040_0000;
const DATA_VA: u32 = 0x0060_0000;
const PROGRAM: [u32; 4] = [
    0xe3a0_002a, // mov r0, #42
    0xe581_0000, // str r0, [r1]
    0xe591_2000, // ldr r2, [r1]
    0xe120_0070, // bkpt #0
];

fn machine() -> (Cpu, FlatRam) {
    let mut ram = FlatRam::new(0, 0x800000);
    for (index, instruction) in PROGRAM.iter().enumerate() {
        ram.write32(0x200000 + index as u64 * 4, *instruction)
            .unwrap();
    }
    let mut cpu = Cpu::new();
    cpu.r[15] = CODE_VA;
    cpu.ttbr0 = L1;
    cpu.dacr = 1; // domain zero client
    cpu.sctlr = 1;
    cpu.vbar = 0x8000;
    (cpu, ram)
}

fn section(physical: u32, ap: u32) -> u32 {
    physical | ap << 10 | 2
}

fn small_page(physical: u32, ap: u32) -> u32 {
    physical | ap << 4 | 2
}

#[test]
fn section_mappings_execute_and_access_physical_ram() {
    let (mut cpu, mut ram) = machine();
    ram.write32(L1 as u64 + 4 * 4, section(0x200000, 3))
        .unwrap();
    ram.write32(L1 as u64 + 6 * 4, section(0x400000, 3))
        .unwrap();
    cpu.r[1] = DATA_VA;

    for _ in 0..3 {
        cpu.step(&mut ram).unwrap();
    }
    assert_eq!(cpu.r[0], 42);
    assert_eq!(cpu.r[2], 42);
    assert_eq!(cpu.pc(), CODE_VA as u64 + 12);
    assert_eq!(ram.read32(0x400000), Ok(42));
    assert_eq!(cpu.step(&mut ram), Err(Trap::Brk(0)));
}

#[test]
fn small_pages_translate_each_side_of_a_page_crossing_access() {
    let (mut cpu, mut ram) = machine();
    ram.write32(L1 as u64 + 4 * 4, 0x20001).unwrap();
    ram.write32(L1 as u64 + 6 * 4, 0x21001).unwrap();
    ram.write32(0x20000, small_page(0x200000, 3)).unwrap();
    ram.write32(0x21000, small_page(0x400000, 3)).unwrap();
    ram.write32(0x21004, small_page(0x500000, 3)).unwrap();
    cpu.r[1] = DATA_VA + 0xffe;

    for _ in 0..3 {
        cpu.step(&mut ram).unwrap();
    }
    assert_eq!(cpu.r[2], 42);
    assert_eq!(ram.read16(0x400ffe), Ok(42));
    assert_eq!(ram.read16(0x500000), Ok(0));
    assert_eq!(cpu.step(&mut ram), Err(Trap::Brk(0)));
}

#[test]
fn mmu_faults_record_address_status_and_abort_vector() {
    let (mut cpu, mut ram) = machine();
    ram.write32(L1 as u64 + 4 * 4, section(0x200000, 3))
        .unwrap();
    cpu.r[1] = DATA_VA;
    cpu.r[15] = CODE_VA + 4; // STR into an unmapped section
    assert_eq!(cpu.step(&mut ram), Err(Trap::Exception(0x25, 1)));
    assert_eq!(cpu.pc(), 0x8010);
    assert_eq!(cpu.dfsr, 0x05 | 1 << 11);
    assert_eq!(cpu.dfar, DATA_VA);

    let (mut cpu, mut ram) = machine();
    ram.write32(L1 as u64 + 4 * 4, section(0x200000, 3))
        .unwrap();
    ram.write32(L1 as u64 + 6 * 4, section(0x400000, 1))
        .unwrap();
    cpu.cpsr = (cpu.cpsr & !0x1f) | MODE_USR;
    cpu.r[1] = DATA_VA;
    cpu.r[15] = CODE_VA + 4;
    assert_eq!(cpu.step(&mut ram), Err(Trap::Exception(0x25, 1)));
    assert_eq!(cpu.dfsr, 0x0d | 1 << 11);
    assert_eq!(cpu.dfar, DATA_VA);

    let (mut cpu, mut ram) = machine();
    ram.write32(
        L1 as u64 + 4 * 4,
        section(0x200000, 3) | 1 << 4, // XN
    )
    .unwrap();
    assert_eq!(cpu.step(&mut ram), Err(Trap::Exception(0x21, 1)));
    assert_eq!(cpu.pc(), 0x800c);
    assert_eq!(cpu.ifsr, 0x0d);
    assert_eq!(cpu.ifar, CODE_VA);
}
