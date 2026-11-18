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

/// A hot 32-bit loop (A32 and Thumb, loads and stores, exclusives, an SVC every iteration):
/// compiled by the 32-bit core's JIT, it ends exactly as interpreted.
const LOOP: &str = "
    movw r1, #0
    movt r1, #0x20
    mov r6, #0
    mov r8, #3000
1:  ldr r2, [r1, #16]
    add r2, r2, r8
    eor r2, r2, r8, lsl #3
    str r2, [r1, #16]
    ldrex r3, [r1]
    add r3, r3, #1
    strex r12, r3, [r1]
    cmp r12, #0
    bne 1b
    adr r4, thumb
    orr r4, r4, #1
    blx r4
    mov r7, #2
    svc #0
    subs r8, r8, #1
    bne 1b
    mov r7, #1
    svc #0
    .thumb
thumb:
    adds r6, r6, #3
    lsls r5, r6, #2
    eors r5, r5, r2
    bx lr";

fn run_loop(jit: bool) -> (Cpu, u32, u32) {
    let mut ram = FlatRam::new(0, RAM);
    ram.load(LOAD, &assemble(KERNEL)).unwrap();
    ram.load(USER, &assemble_a32(LOOP)).unwrap();
    let mut cpu = Cpu::new();
    cpu.brk_to_host = true;
    if jit {
        cpu.jit = arm_a64::jit::Jit::with(1 << 20, 1);
        cpu.chain_until = u64::MAX;
    }
    cpu.ram = ram.mem.as_mut_ptr();
    cpu.ram_len = ram.mem.len() as u64;
    cpu.el = 1;
    cpu.pc = LOAD;
    cpu.sp = [0x40_0000, 0x50_0000, 0x60_0000, 0x70_0000];
    cpu.sys.cpacr_el1 = 3 << 20;
    for _ in 0..1_000_000 {
        match cpu.step(&mut ram) {
            Ok(()) | Err(Trap::Exception(..)) => {}
            Err(Trap::Brk(0)) => {
                let (a, b) = (ram.read32(DATA).unwrap(), ram.read32(DATA + 16).unwrap());
                return (cpu, a, b);
            }
            Err(t) => panic!("{t:?} at pc {:#x}", cpu.pc),
        }
    }
    panic!("no brk");
}

#[test]
fn compiled_32_bit_code_ends_as_interpreted() {
    let (want, a, b) = run_loop(false);
    assert_eq!(a, 3000);
    assert_eq!(want.x[0], 7 + 3000 * 100, "an SVC per iteration");
    if !arm_a64::jit::available() {
        return;
    }
    let (got, ga, gb) = run_loop(true);
    assert_eq!((ga, gb), (a, b), "memory");
    assert_eq!(got.x[..15], want.x[..15], "registers");
    assert_eq!(got.instret, want.instret, "instructions");
    let (compiled, runs, _) = got.aarch32_jit_stats().expect("the 32-bit core has a JIT");
    assert!(compiled > 0 && runs > 1000, "{compiled} blocks compiled, {runs} runs");
}

/// Two cores on host threads (`mt`), each a 32-bit program incrementing one counter with
/// LDREX/STREX: the store-exclusive is a compare-and-swap, so no increment is lost.
const SHARED: &str = "
    movw r1, #0
    movt r1, #0x20
    movw r8, #50000
1:  ldrex r3, [r1]
    add r3, r3, #1
    strex r12, r3, [r1]
    cmp r12, #0
    bne 1b
    subs r8, r8, #1
    bne 1b
    mov r7, #1
    svc #0";

/// A bus over the RAM the threads share.
struct SharedRam(usize);
macro_rules! shared_access {($r:ident, $w:ident, $t:ty) => {
    fn $r(&mut self, a: u64) -> Result<$t, emu_core::Fault> {
        if a as usize + std::mem::size_of::<$t>() > RAM { return Err(emu_core::Fault::Unmapped); }
        // SAFETY: inside the shared RAM buffer, which outlives the threads
        Ok(unsafe { ((self.0 + a as usize) as *const $t).read_unaligned() })
    }
    fn $w(&mut self, a: u64, v: $t) -> Result<(), emu_core::Fault> {
        if a as usize + std::mem::size_of::<$t>() > RAM { return Err(emu_core::Fault::Unmapped); }
        // SAFETY: as above
        unsafe { ((self.0 + a as usize) as *mut $t).write_unaligned(v) };
        Ok(())
    }
}}
impl Bus for SharedRam {
    shared_access!(read8, write8, u8);
    shared_access!(read16, write16, u16);
    shared_access!(read32, write32, u32);
    shared_access!(read64, write64, u64);
}

#[test]
fn two_threads_increment_one_counter_with_ldrex_strex() {
    for jit in [false, true] {
        if jit && !arm_a64::jit::available() {
            continue;
        }
        let mut ram = FlatRam::new(0, RAM);
        ram.load(LOAD, &assemble(KERNEL)).unwrap();
        ram.load(USER, &assemble_a32(SHARED)).unwrap();
        let base = ram.mem.as_mut_ptr() as usize;
        let threads: Vec<_> = (0..2u64)
            .map(|t| {
                std::thread::spawn(move || {
                    let mut bus = SharedRam(base);
                    let mut cpu = Cpu::new();
                    cpu.brk_to_host = true;
                    cpu.mt = true;
                    if jit {
                        cpu.jit = arm_a64::jit::Jit::with(1 << 20, 1);
                        cpu.chain_until = u64::MAX;
                    }
                    cpu.ram = base as *mut u8;
                    cpu.ram_len = RAM as u64;
                    cpu.el = 1;
                    cpu.pc = LOAD;
                    cpu.sp = [0x40_0000, 0x50_0000 + t * 0x8000, 0x60_0000, 0x70_0000];
                    cpu.sys.cpacr_el1 = 3 << 20;
                    for _ in 0..10_000_000 {
                        match cpu.step(&mut bus) {
                            Ok(()) | Err(Trap::Exception(..)) => {}
                            Err(Trap::Brk(0)) => return,
                            Err(e) => panic!("{e:?} at pc {:#x}", cpu.pc),
                        }
                    }
                    panic!("no brk, pc {:#x}", cpu.pc);
                })
            })
            .collect();
        for t in threads {
            t.join().unwrap();
        }
        assert_eq!(ram.read32(DATA).unwrap(), 100_000, "jit = {jit}");
    }
}

/// ARMv8's load-acquire/store-release in A32 and T32, every size, and the kernel's
/// `__kuser_cmpxchg` sequence (LDREX, STLEXEQ) that ARMv6 userlands use for atomics.
const ACQ_REL: &str = "
    .arch armv8-a
    movw r1, #0
    movt r1, #0x20
    movw r2, #0x1234
    movt r2, #0x89ab
    stl r2, [r1]
    lda r3, [r1]               // r3 = 0x89ab1234
    stlb r2, [r1]
    ldab r4, [r1]              // 0x34
    add r5, r1, #8
    stlh r2, [r5]
    ldah r6, [r5]              // 0x1234
1:  ldaex r7, [r1]
    add r7, r7, #1
    stlex r8, r7, [r1]
    cmp r8, #0
    bne 1b
    add r9, r1, #16
2:  ldaexd r10, r11, [r9]
    adds r10, r10, #5
    adc r11, r11, #0
    stlexd r8, r10, r11, [r9]
    cmp r8, #0
    bne 2b
    add r12, r1, #32           // kuser_cmpxchg(old r0 = 0, new r1' = 77, ptr r12)
    mov r0, #0
    mov r6, #77
3:  ldrex r3, [r12]
    subs r3, r3, r0
    stlexeq r3, r6, [r12]
    teqeq r3, #1
    beq 3b
    dmb ish
    sevl
    wfe                        // SEVL set the event: no wait
    adr r4, thumb
    orr r4, r4, #1
    blx r4
    mov r7, #1
    svc #0
    .thumb
thumb:
    add r5, r1, #40
    stl r2, [r5]
    lda r0, [r5]
4:  ldaexh r6, [r5]
    adds r6, r6, #1
    stlexh r8, r6, [r5]
    cmp r8, #0
    bne 4b
    ldaexb r9, [r5]
    stlexb r8, r9, [r5]
    bx lr";

#[test]
fn load_acquire_store_release_in_a32_and_t32() {
    for jit in [false, true] {
        if jit && !arm_a64::jit::available() {
            continue;
        }
        let mut ram = FlatRam::new(0, RAM);
        ram.load(LOAD, &assemble(KERNEL)).unwrap();
        ram.load(USER, &assemble_a32(ACQ_REL)).unwrap();
        ram.write64(DATA + 16, 0x1_ffff_fffe).unwrap();
        let mut cpu = Cpu::new();
        cpu.brk_to_host = true;
        if jit {
            cpu.jit = arm_a64::jit::Jit::with(1 << 20, 1);
            cpu.chain_until = u64::MAX;
        }
        cpu.ram = ram.mem.as_mut_ptr();
        cpu.ram_len = ram.mem.len() as u64;
        cpu.el = 1;
        cpu.pc = LOAD;
        cpu.sp = [0x40_0000, 0x50_0000, 0x60_0000, 0x70_0000];
        cpu.sys.cpacr_el1 = 3 << 20;
        run(&mut cpu, &mut ram);
        assert_eq!(cpu.esr[1] >> 26, 0x11, "ended by the final SVC, not UNDEFINED (jit {jit})");
        assert_eq!(cpu.x[0], 0x89ab_1234, "T32 LDA");
        assert_eq!(cpu.x[3], 0, "cmpxchg succeeded");
        assert_eq!(ram.read32(DATA + 32).unwrap(), 77);
        assert_eq!(ram.read64(DATA + 16).unwrap(), 0x2_0000_0003, "LDAEXD/STLEXD with carry");
        assert_eq!(ram.read32(DATA).unwrap(), 0x89ab_1235, "LDAEX/STLEX on the word STLB wrote into");
        assert_eq!(ram.read16(DATA + 8).unwrap(), 0x1234, "STLH");
        assert_eq!(ram.read16(DATA + 40).unwrap(), 0x1235, "T32 LDAEXH/STLEXH");
        assert_eq!(cpu.x[8], 0, "T32 STLEXB");
    }
}
