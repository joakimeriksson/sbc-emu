//! System-level behaviour: memory accesses, exceptions, EL changes, MMU, timers.
mod common;
use common::*;
use emu_core::Trap;

#[test]
fn loads_stores_all_addressing_modes() {
    let mut r = run("
        mov x1, #0x100000
        movz x2, #0x8899, lsl #48
        movk x2, #0xaabb, lsl #32
        movk x2, #0xccdd, lsl #16
        movk x2, #0xeeff
        str x2, [x1]
        ldrb w3, [x1, #1]
        ldrsb x4, [x1, #6]
        ldrh w5, [x1, #2]
        ldrsh w6, [x1, #6]
        ldrsw x7, [x1, #4]
        ldur w8, [x1, #3]
        mov x9, x1
        str w2, [x9, #16]!
        ldr w10, [x9], #-8
        mov x11, #2
        ldr x12, [x1, x11, lsl #3]
        stp x2, x9, [x1, #-32]!
        ldp x13, x14, [x1], #32
        ldpsw x15, x16, [x1]
        ldr x17, lit
        brk #0
        .p2align 3
    lit: .quad 0x0123456789abcdef
    ");
    assert_eq!(r.x(3), 0xee);
    assert_eq!(r.x(4), 0xffff_ffff_ffff_ff99); // byte 6
    assert_eq!(r.x(5), 0xccdd);
    assert_eq!(r.x(6), 0xffff_8899);
    assert_eq!(r.x(7), 0xffff_ffff_8899_aabb);
    assert_eq!(r.x(8), 0x99aa_bbcc);
    assert_eq!(r.x(9), 0x100008);
    assert_eq!(r.x(10), 0xccdd_eeff);
    assert_eq!(r.x(12), 0xccdd_eeff); // [x1+16] holds the 32-bit store, upper half zero
    assert_eq!(r.x(13), 0x8899_aabb_ccdd_eeff);
    assert_eq!(r.x(14), 0x100008);
    assert_eq!(r.x(1), 0x100000);
    assert_eq!(r.x(15), 0xffff_ffff_ccdd_eeff);
    assert_eq!(r.x(16), 0xffff_ffff_8899_aabb);
    assert_eq!(r.x(17), 0x0123_4567_89ab_cdef);
    assert_eq!(r.mem64(0x100000 - 32), 0x8899_aabb_ccdd_eeff);
}

#[test]
fn unaligned_and_page_crossing_access() {
    let r = run("
        ldr x1, =0x100ffd
        mov x2, #0x1122
        movk x2, #0x3344, lsl #16
        movk x2, #0x5566, lsl #32
        movk x2, #0x7788, lsl #48
        str x2, [x1]
        ldr x3, [x1]
        ldr w4, [x1, #1]
        brk #0
        .ltorg");
    assert_eq!(r.x(3), 0x7788_5566_3344_1122);
    assert_eq!(r.x(4), 0x6633_4411);
}

#[test]
fn exclusive_pair_and_monitor() {
    let r = run("
        mov x1, #0x100000
        mov x2, #5
        str x2, [x1]
    1:  ldxr x3, [x1]
        add x3, x3, #1
        stxr w4, x3, [x1]
        cbnz w4, 1b
        stxr w5, x3, [x1]      // no reservation: must fail
        ldr x6, [x1]
        ldaxr x7, [x1]
        clrex
        stlxr w8, x7, [x1]     // cleared: must fail
        brk #0");
    assert_eq!(r.x(4), 0);
    assert_eq!(r.x(5), 1);
    assert_eq!(r.x(6), 6);
    assert_eq!(r.x(8), 1);
}

#[test]
fn svc_from_el0_and_eret_back() {
    // EL1 vector table at 0x90000: the lower-EL sync vector (+0x400) increments x20, returns
    let r = run("
        ldr x0, =0x90000
        msr vbar_el1, x0
        adr x1, el0
        msr elr_el1, x1
        msr spsr_el1, xzr        // EL0t, interrupts unmasked
        eret
    el0:
        mov x20, #0
        svc #0x42
        svc #0x43
        mrs x21, nzcv            // EL0-accessible
        brk #0
        .ltorg
        .org 0x10000 + 0x400
        mrs x22, esr_el1
        mrs x23, elr_el1
        add x20, x20, #1
        eret");
    assert_eq!(r.x(20), 2);
    assert_eq!(r.x(22), (0x15 << 26) | (1 << 25) | 0x43);
    assert_eq!(r.cpu.el, 0);
    assert_eq!(r.x(23), r.cpu.pc - 4); // return address = insn after the last svc
}

#[test]
fn el1_register_from_el0_traps() {
    let r = run("
        ldr x0, =0x90000
        msr vbar_el1, x0
        adr x1, el0
        msr elr_el1, x1
        msr spsr_el1, xzr
        eret
    el0:
        mrs x0, sctlr_el1
        brk #1
        .ltorg
        .org 0x10000 + 0x400
        mrs x1, esr_el1
        brk #0");
    assert_eq!(r.cpu.el, 1);
    assert_eq!(r.x(1) >> 26, 0x18);
}

#[test]
fn undefined_instruction_takes_exception() {
    let r = run("
        ldr x0, =0x90000
        msr vbar_el1, x0
        .inst 0x00000000
        .ltorg
        .org 0x10000 + 0x200
        mrs x1, esr_el1
        mrs x2, elr_el1
        brk #0");
    assert_eq!(r.x(1) >> 26, 0);
    assert_eq!(r.x(2), LOAD + 8);
}

#[test]
fn hvc_smc_routing_and_el_drop() {
    let r = run_at(
        "
        ldr x0, =0x90000
        msr vbar_el3, x0
        ldr x0, =0xa0000
        msr vbar_el2, x0
        mov x0, #0x3c9           // EL2h, DAIF masked
        msr spsr_el3, x0
        adr x0, el2
        msr elr_el3, x0
        eret
    el2:
        mrs x10, currentel
        hvc #7
        smc #9
        brk #0
        .ltorg
        .org 0x10000 + 0x200     // EL3, current EL with SPx
        b .
        .org 0x10000 + 0x400     // EL3, from lower EL: SMC
        mrs x12, esr_el3
        eret
        .org 0x20000 + 0x200     // EL2, current EL with SPx: HVC from EL2
        mrs x11, esr_el2
        eret",
        3,
    );
    assert_eq!(r.trap, Trap::Brk(0));
    assert_eq!(r.x(10), 2 << 2);
    assert_eq!(r.x(11) >> 26, 0x16);
    assert_eq!(r.x(11) & 0xffff, 7);
    assert_eq!(r.x(12) >> 26, 0x17);
    assert_eq!(r.x(12) & 0xffff, 9);
}

#[test]
fn mmu_identity_and_high_mapping_with_faults() {
    // L1 table at 0x200000 (TTBR0): 1 GiB identity block for 0..1G.
    // TTBR1 L1 table at 0x201000: VA 0xffff_ff80_0000_0000 + ... maps to L2 at 0x202000,
    // whose entry 0 is a 4 KiB-page L3 table at 0x203000 mapping page 0 -> PA 0x300000 (RO).
    // T0SZ = T1SZ = 25 (39-bit VA, 3 levels starting at L1).
    let r = run("
        ldr x0, =0x200000
        ldr x1, =0x401            // block, AF, AttrIdx 0
        str x1, [x0]
        ldr x0, =0x201000
        ldr x1, =0x202003         // table
        str x1, [x0]
        ldr x0, =0x202000
        ldr x1, =0x203003
        str x1, [x0]
        ldr x0, =0x203000
        ldr x1, =0x3004c3         // page, AF, AP=11 (RO, EL0), 
        str x1, [x0]
        ldr x1, =0x301403         // page 1 -> 0x301000, AF, AP=00 (RW EL1)
        str x1, [x0, #8]
        ldr x0, =0x300000
        mov x1, #0x77
        str x1, [x0]
        ldr x0, =0x200000
        msr ttbr0_el1, x0
        ldr x0, =0x201000
        msr ttbr1_el1, x0
        ldr x0, =0x80190019       // T0SZ=25, T1SZ=25, TG0=4K, TG1=4K
        msr tcr_el1, x0
        ldr x0, =0x90000
        msr vbar_el1, x0
        mrs x0, sctlr_el1
        orr x0, x0, #1
        msr sctlr_el1, x0
        isb
        ldr x2, =0xffffff8000000000
        ldr x3, [x2]              // 0x77 through the high mapping
        mov x4, #0x55
        str x4, [x2, #0x1000]     // writable page
        ldr x5, [x2, #0x1000]
        str x4, [x2]              // read-only: permission fault -> handler sets x6
        ldr x7, =0xffffff8000002000
        ldr x8, [x7]              // unmapped: translation fault
        brk #0
        .ltorg
        .org 0x10000 + 0x200
        mrs x9, esr_el1
        mrs x10, far_el1
        add x6, x6, #1
        mrs x11, elr_el1
        add x11, x11, #4
        msr elr_el1, x11
        mov x12, x9               // remember the first syndrome
        cbz x13, 1f
        b 2f
    1:  mov x13, x9
    2:  eret");
    assert_eq!(r.x(3), 0x77);
    assert_eq!(r.x(5), 0x55);
    assert_eq!(r.x(6), 2);
    // first fault: data abort same EL, write, permission fault level 3
    assert_eq!(r.x(13) >> 26, 0x25);
    assert_eq!(r.x(13) & 0x7f, (1 << 6) | 0b001111);
    // second: translation fault level 3, read
    assert_eq!(r.x(9) & 0x7f, 0b000111);
    assert_eq!(r.x(10), 0xffff_ff80_0000_2000);
}

#[test]
fn generic_timer_compare_and_irq_delivery() {
    use arm_a64::Cpu;
    use emu_core::{Core, FlatRam};
    let code = assemble(
        "
        ldr x0, =0x90000
        msr vbar_el1, x0
        mov x0, #100
        msr cntv_tval_el0, x0
        mov x0, #1
        msr cntv_ctl_el0, x0
        msr daifclr, #2
    1:  wfi
        b 1b
        .ltorg
        .org 0x10000 + 0x280      // IRQ, current EL SPx
        mrs x5, cntv_ctl_el0
        msr cntv_ctl_el0, xzr
        brk #0",
    );
    let mut ram = FlatRam::new(0, RAM);
    ram.load(LOAD, &code).unwrap();
    let mut cpu = Cpu::new();
    cpu.brk_to_host = true;
    cpu.enable_test_jit_from_env();
    cpu.ram = ram.mem.as_mut_ptr();
    cpu.ram_len = ram.mem.len() as u64;
    cpu.el = 1;
    cpu.pc = LOAD;
    cpu.sys.cntfrq = 1_000_000_000; // one count per ns of virtual time
    let mut waits = 0;
    loop {
        // the machine's job: advance virtual time (10 ns extra per step) and drive the PPI level
        cpu.sys.time_base_ns += 10;
        cpu.refresh_counter();
        let now = cpu.sys.cnt.wrapping_sub(cpu.sys.cntvoff);
        cpu.irq = cpu.sys.vtimer.firing(now);
        match cpu.step(&mut ram) {
            Ok(()) | Err(Trap::Interrupt(..)) => {}
            Err(Trap::Wait) => waits += 1,
            Err(Trap::Brk(0)) => break,
            Err(t) => panic!("{t:?} at {:#x}", cpu.pc),
        }
        assert!(cpu.instret < 10_000);
    }
    assert!(waits >= 1);
    assert_eq!(cpu.x[5], 5); // ENABLE | ISTATUS
}
