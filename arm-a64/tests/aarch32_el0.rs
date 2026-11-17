//! 32-bit programs at EL0 under a 64-bit EL1 (AArch32 EL0, as a Cortex-A72 runs a 32-bit
//! userland): the exception return into AArch32, the register mapping both ways, and the
//! syndromes of SVC (A32 and 16-bit Thumb), UNDEFINED, a data abort and a trapped CP15 timer
//! read, taken at the "lower EL using AArch32" vectors.
mod common;
use arm_a64::Cpu;
use common::{assemble, assemble_a32, LOAD, RAM};
use emu_core::{Bus, Core, FlatRam, Trap};

const USER: u64 = 0x10_0000;
const DATA: u64 = 0x20_0000;
const LOG: u64 = 0x30_0000;

/// EL1: vectors, then ERET to the 32-bit program. Every exception from it is logged (ESR,
/// FAR); SVC with r7 = 2 returns r0 + 100, r7 = 1 ends the run; other exceptions skip the
/// (32-bit) instruction.
const KERNEL: &str = "
    adr x0, vectors
    msr vbar_el1, x0
    mov x0, #0x2000
    msr tpidrro_el0, x0
    mov x1, #0x10              // SPSR: AArch32 User mode, A32
    msr spsr_el1, x1
    mov x1, #0x100000
    msr elr_el1, x1
    mov x0, #7
    mov x15, #0x1515           // not an AArch32 register: must survive
    fmov d0, #1.5
    mov x2, #0x1616
    fmov d16, x2
    mov x21, #0
    eret
    .balign 2048
vectors:
    .skip 0x600
    b sync32
    .balign 0x80
    brk #1                     // IRQ from AArch32 (not expected in this run)
sync32:
    mrs x9, esr_el1
    mov x22, #0x300000
    str x9, [x22, x21, lsl #3]
    mrs x11, far_el1
    add x23, x22, #0x100
    str x11, [x23, x21, lsl #3]
    add x21, x21, #1
    lsr x10, x9, #26
    cmp x10, #0x11
    b.eq svc
    mrs x12, elr_el1
    add x12, x12, #4
    msr elr_el1, x12
    eret
svc:
    cmp x7, #1
    b.eq done
    add x0, x0, #100
    eret
done:
    mrs x24, spsr_el1
    mrs x25, tpidr_el0
    brk #0";

const PROGRAM: &str = "
    add r0, r0, #1             // 8
    mov r7, #2
    svc #0x42                  // 108
    mov r5, r0
    movw r1, #0
    movt r1, #0x20
    str r5, [r1]
    ldrb r6, [r1]
    mrc p15, 0, r2, c13, c0, 3 // TPIDRURO
    mcr p15, 0, r5, c13, c0, 2 // TPIDRURW
    vmov.f64 d1, #2.0
    vadd.f64 d2, d0, d1        // 3.5
    adr r4, thumb
    orr r4, r4, #1
    blx r4                     // 109, then 209
    udf #0
    movw r8, #0
    movt r8, #0x9000
    ldr r9, [r8]               // no memory there: external abort
    mrrc p15, 1, r10, r11, c14 // CNTVCT while CNTKCTL_EL1 denies it: trapped
    ldrex r3, [r1]
    add r3, r3, #1
    strex r12, r3, [r1]
    dmb ish
    mcr p15, 0, r0, c7, c10, 5 // CP15DMB (SCTLR_EL1.CP15BEN is set)
    mov r7, #1
    svc #0
    .thumb
thumb:
    adds r0, r0, #1
    svc #3
    bx lr";

fn machine() -> (Cpu, FlatRam) {
    let mut ram = FlatRam::new(0, RAM);
    ram.load(LOAD, &assemble(KERNEL)).unwrap();
    ram.load(USER, &assemble_a32(PROGRAM)).unwrap();
    let mut cpu = Cpu::new();
    cpu.brk_to_host = true;
    cpu.enable_test_jit_from_env();
    cpu.ram = ram.mem.as_mut_ptr();
    cpu.ram_len = ram.mem.len() as u64;
    cpu.el = 1;
    cpu.pc = LOAD;
    cpu.sp = [0x40_0000, 0x50_0000, 0x60_0000, 0x70_0000];
    cpu.sys.cpacr_el1 = 3 << 20;
    (cpu, ram)
}

fn run(cpu: &mut Cpu, ram: &mut FlatRam) {
    for _ in 0..10_000 {
        match cpu.step(ram) {
            Ok(()) | Err(Trap::Exception(..)) => {}
            Err(Trap::Brk(0)) => return,
            Err(t) => panic!("{t:?} at pc {:#x}", cpu.pc),
        }
    }
    panic!("no brk, pc {:#x}", cpu.pc);
}

fn check(cpu: &Cpu, ram: &mut FlatRam) {
    assert_eq!(cpu.x[0], 209, "r0 through two SVCs and Thumb code");
    assert_eq!((cpu.x[5], cpu.x[6], cpu.x[2]), (108, 108, 0x2000));
    assert_eq!(ram.read32(DATA).unwrap(), 109, "exclusive increment");
    assert_eq!(cpu.x[12], 0, "store-exclusive succeeded");
    assert_eq!(cpu.x[25], 108, "TPIDRURW is TPIDR_EL0");
    assert_eq!(cpu.v[0] >> 64, 2.0f64.to_bits() as u128, "D1 is the upper half of V0");
    assert_eq!(cpu.v[1] as u64, 3.5f64.to_bits(), "D2 is the lower half of V1");
    assert_eq!(cpu.x[15], 0x1515);
    assert_eq!(cpu.v[16] as u64, 0x1616);
    assert_eq!(cpu.x[24] & 0x3f, 0x10, "SPSR: AArch32 User mode, A32");
    let esr: Vec<u64> = (0..6).map(|i| ram.read64(LOG + i * 8).unwrap()).collect();
    let far = ram.read64(LOG + 0x100 + 3 * 8).unwrap();
    let cntvct = 1 << 24 | 0xe << 20 | 1 << 16 | 11 << 10 | 10 << 5 | 14 << 1 | 1;
    assert_eq!(
        esr,
        [
            0x11 << 26 | 1 << 25 | 0x42, // SVC (A32)
            0x11 << 26 | 3,              // SVC (16-bit Thumb: IL = 0)
            1 << 25,                     // UNDEFINED
            0x24 << 26 | 1 << 25 | 0x10, // data abort, synchronous external abort
            0x04 << 26 | 1 << 25 | cntvct, // MRRC p15 trapped
            0x11 << 26 | 1 << 25,        // the final SVC
        ]
    );
    assert_eq!(far, 0x9000_0000);
}

#[test]
fn a_32_bit_program_runs_at_el0_and_its_exceptions_reach_el1() {
    let (mut cpu, mut ram) = machine();
    run(&mut cpu, &mut ram);
    check(&cpu, &mut ram);
}

#[test]
fn a_snapshot_taken_inside_the_32_bit_program_resumes_it() {
    let (mut cpu, mut ram) = machine();
    // into the program, past the first SVC
    while !(cpu.aarch32 && cpu.x[21] == 1) {
        let _ = cpu.step(&mut ram);
    }
    for _ in 0..8 {
        cpu.step(&mut ram).unwrap();
    }
    let mut w = emu_core::snap::W(Vec::new());
    cpu.save_state(&mut w);
    let bytes = w.0;
    let (mut restored, _other_ram) = machine();
    restored.ram = ram.mem.as_mut_ptr();
    restored.load_state(&mut emu_core::snap::R::new(&bytes)).unwrap();
    assert!(restored.aarch32);
    run(&mut restored, &mut ram);
    check(&restored, &mut ram);
}

#[test]
fn an_interrupt_leaves_the_32_bit_program_at_the_aarch32_vector() {
    let (mut cpu, mut ram) = machine();
    while !cpu.aarch32 {
        let _ = cpu.step(&mut ram);
    }
    cpu.step(&mut ram).unwrap(); // add r0, r0, #1
    cpu.set_irq(true, false);
    assert!(matches!(cpu.step(&mut ram), Err(Trap::Interrupt(0, 1))));
    assert!(!cpu.aarch32);
    assert_eq!(cpu.pc, cpu.vbar[1] + 0x680);
    assert_eq!(cpu.elr[1], USER + 4, "returns to the next instruction");
    assert_eq!(cpu.spsr[1] & 0x3f, 0x10);
    assert_eq!(cpu.x[0], 8);
}
