use allwinner_h3::{
    ns_to_ticks, ticks_to_ns, CoreState, Machine, Soc, GICC_BASE, GICD_BASE, PHYSICAL_TIMER_PPI, RAM_BASE, SPURIOUS, UART0_BASE,
};
use emu_core::{Bus, Core, Fault, Stop, Trap};

#[test]
fn a32_program_writes_ok_to_h3_uart0() {
    let instructions: [u32; 7] = [
        0xe308_0000, // movw r0, #0x8000
        0xe340_01c2, // movt r0, #0x01c2: r0 = UART0_BASE
        0xe3a0_104f, // mov r1, #'O'
        0xe5c0_1000, // strb r1, [r0]
        0xe3a0_104b, // mov r1, #'K'
        0xe5c0_1000, // strb r1, [r0]
        0xe120_0070, // bkpt #0
    ];
    let mut machine = Machine::with_ram_size(64 << 10);
    let bytes: Vec<u8> = instructions
        .iter()
        .flat_map(|insn| insn.to_le_bytes())
        .collect();
    machine.load(RAM_BASE, &bytes).unwrap();

    for _ in 0..6 {
        machine.step().unwrap();
    }
    assert_eq!(machine.serial_output(), b"OK");
    assert_eq!(machine.cpu().pc(), RAM_BASE + 24);
    assert_eq!(machine.step(), Err(Trap::Brk(0)));
    assert_eq!(machine.take_serial_output(), b"OK");
    assert!(machine.serial_output().is_empty());
}

#[test]
fn uart_uses_four_byte_register_spacing_and_bounded_input() {
    let mut soc = Soc::new(64 << 10);
    assert_eq!(soc.read32(UART0_BASE + 0x14), Ok(0x60));
    soc.write32(UART0_BASE, b'A' as u32).unwrap();
    assert_eq!(soc.uart.out, b"A");

    assert!(soc.uart.push_rx(b'R'));
    assert_eq!(soc.read32(UART0_BASE + 0x14), Ok(0x61));
    assert_eq!(soc.read8(UART0_BASE), Ok(b'R'));
    assert_eq!(soc.read8(UART0_BASE), Ok(0));
    assert_eq!(soc.read8(UART0_BASE + 1), Err(Fault::Prohibited));
    assert_eq!(soc.read64(UART0_BASE), Err(Fault::Prohibited));
    // Outside the H3 device window nothing answers.
    assert_eq!(soc.read32(0x0200_0000), Err(Fault::Unmapped));

    for byte in 0..64 {
        assert!(soc.uart.push_rx(byte));
    }
    assert!(!soc.uart.push_rx(64));
    assert_eq!(soc.read8(UART0_BASE), Ok(0));
    assert!(soc.uart.push_rx(64));
}

#[test]
fn ram_and_unmapped_addresses_do_not_alias() {
    let mut soc = Soc::new(0x1000);
    soc.write32(RAM_BASE + 0xffc, 0x1234_5678).unwrap();
    assert_eq!(soc.read32(RAM_BASE + 0xffc), Ok(0x1234_5678));
    assert_eq!(soc.read32(RAM_BASE + 0x1000), Err(Fault::Unmapped));
    assert_eq!(soc.read32(RAM_BASE - 4), Err(Fault::Unmapped));
    assert_eq!(soc.read32(u64::MAX - 3), Err(Fault::Unmapped));
}

#[test]
fn gic_acknowledge_and_eoi_track_level_interrupts() {
    let mut soc = Soc::new(64 << 10);
    let timer = PHYSICAL_TIMER_PPI;
    soc.write32(GICD_BASE, 1).unwrap();
    soc.write32(GICD_BASE + 0x100, 1 << timer).unwrap();
    soc.write32(GICD_BASE + 0x41c, 0x8080_8080).unwrap();
    soc.write32(GICC_BASE, 1).unwrap();
    soc.write32(GICC_BASE + 4, 0xff).unwrap();
    soc.gic.set_level(timer, true);

    assert!(soc.gic.irq_out());
    assert_eq!(soc.read32(GICD_BASE + 0x200), Ok(1 << timer));
    assert_eq!(soc.read32(GICC_BASE + 0x18), Ok(timer as u32));
    assert_eq!(soc.read32(GICC_BASE + 0x0c), Ok(timer as u32));
    assert!(!soc.gic.irq_out()); // active interrupts do not preempt at the same priority
    assert_eq!(soc.read32(GICC_BASE + 0x0c), Ok(SPURIOUS));
    assert_eq!(soc.read32(GICD_BASE + 0x300), Ok(1 << timer));

    soc.gic.set_level(timer, false);
    soc.write32(GICC_BASE + 0x10, timer as u32).unwrap();
    assert!(!soc.gic.irq_out());
    soc.gic.set_level(timer, true);
    assert!(soc.gic.irq_out());
}

#[test]
fn physical_timer_wakes_wfi_and_enters_irq_through_gic() {
    let instructions: [u32; 7] = [
        0xee1e_0f10, // mrc p15, 0, r0, c14, c0, 0 (CNTFRQ)
        0xe3a0_1005, // mov r1, #5
        0xee0e_1f12, // mcr p15, 0, r1, c14, c2, 0 (CNTP_TVAL)
        0xe3a0_1001, // mov r1, #1
        0xee0e_1f32, // mcr p15, 0, r1, c14, c2, 1 (CNTP_CTL)
        0xe320_f003, // wfi
        0xe120_0070, // bkpt #0
    ];
    let bytes: Vec<u8> = instructions
        .iter()
        .flat_map(|insn| insn.to_le_bytes())
        .collect();
    let mut machine = Machine::with_ram_size(64 << 10);
    machine.load(RAM_BASE, &bytes).unwrap();
    machine.cpu_mut().vbar = (RAM_BASE + 0x100) as u32;
    machine
        .load(RAM_BASE + 0x118, &0xe120_0070u32.to_le_bytes())
        .unwrap();
    machine.cpu_mut().cpsr &= !(1 << 7);

    machine.soc.write32(GICD_BASE, 1).unwrap();
    machine
        .soc
        .write32(GICD_BASE + 0x100, 1 << PHYSICAL_TIMER_PPI)
        .unwrap();
    machine.soc.write32(GICD_BASE + 0x41c, 0x8080_8080).unwrap();
    machine.soc.write32(GICC_BASE, 1).unwrap();
    machine.soc.write32(GICC_BASE + 4, 0xff).unwrap();

    for _ in 0..5 {
        machine.step().unwrap();
    }
    assert_eq!(machine.cpu().r[0], 24_000_000);
    assert_eq!(machine.step(), Err(Trap::Wait));
    assert_eq!(machine.step(), Err(Trap::Wait));
    assert_eq!(machine.step(), Err(Trap::Interrupt(0, 1)));
    assert_eq!(machine.cpu().pc(), RAM_BASE + 0x118);
    assert_eq!(machine.cpu().r[14], (RAM_BASE + 28) as u32);
    assert_eq!(
        machine.soc.read32(GICC_BASE + 0x0c),
        Ok(PHYSICAL_TIMER_PPI as u32)
    );

    machine.cpu_mut().cntp_ctl = 0;
    machine.advance_time(0);
    machine
        .soc
        .write32(GICC_BASE + 0x10, PHYSICAL_TIMER_PPI as u32)
        .unwrap();
    machine.advance_time(0);
    assert!(!machine.soc.gic.irq_out());
    assert!(!machine.cpu().irq);
    assert_eq!(machine.step(), Err(Trap::Brk(0)));
}

/// Core 0 starts core 1 through PSCI CPU_ON (entry 0x4000_0100, context id 0x55) and
/// waits for its flag, then SEV wakes core 1 from WFE.
#[test]
fn psci_cpu_on_starts_a_second_core_and_sev_wakes_wfe() {
    let core0 = [
        0xe300_0003, 0xe348_0400, // movw/movt r0, #0x84000003 (CPU_ON)
        0xe3a0_1001,              // mov r1, #1
        0xe300_2100, 0xe344_2000, // movw/movt r2, #0x40000100
        0xe3a0_3055,              // mov r3, #0x55
        0xe160_0070,              // smc #0
        0xe1a0_5000,              // mov r5, r0
        0xe300_4200, 0xe344_4000, // movw/movt r4, #0x40000200
        0xe594_6000,              // 1: ldr r6, [r4]
        0xe356_0000,              // cmp r6, #0
        0x0aff_fffc,              // beq 1b
        0xe320_f004,              // sev
        0xeaff_fffe,              // b .
    ];
    let core1 = [
        0xe300_4200, 0xe344_4000, // movw/movt r4, #0x40000200
        0xe584_0000,              // str r0, [r4]
        0xe320_f002,              // wfe
        0xe3a0_7077,              // mov r7, #0x77
        0xe584_7004,              // str r7, [r4, #4]
        0xeaff_fffe,              // b .
    ];
    let mut machine = Machine::with_ram_size(64 << 10);
    for (base, code) in [(RAM_BASE, &core0[..]), (RAM_BASE + 0x100, &core1[..])] {
        for (i, word) in code.iter().enumerate() {
            machine.soc.write32(base + i as u64 * 4, *word).unwrap();
        }
    }
    machine.cpu_mut().firmware_calls_to_host = true;
    assert_eq!(machine.state[1], CoreState::Off);
    machine.run(100_000);
    assert_eq!(machine.cpus[0].r[5], 0, "PSCI SUCCESS");
    assert_eq!(machine.state[1], CoreState::Run);
    assert_eq!(machine.cpus[1].mpidr & 0xff, 1);
    assert_eq!(machine.soc.read32(RAM_BASE + 0x200), Ok(0x55), "context id in r0");
    assert_eq!(machine.soc.read32(RAM_BASE + 0x204), Ok(0x77), "SEV woke WFE");
}

/// Cores asleep in WFI do not slow the clock: a lone busy core runs at 1 ns per
/// instruction; two busy cores share it.
#[test]
fn virtual_time_counts_only_running_cores() {
    let mut machine = Machine::with_ram_size(64 << 10);
    machine.soc.write32(RAM_BASE, 0xeaff_fffe).unwrap(); // b .
    for core in 1..4 {
        machine.state[core] = CoreState::Wait;
    }
    machine.run(12_000);
    assert_eq!(machine.now(), 12_000);
    machine.state[1] = CoreState::Run;
    machine.cpus[1].r[15] = RAM_BASE as u32;
    machine.run(12_000);
    assert_eq!(machine.now(), 18_000, "two running cores: 0.5 ns per instruction");
}

/// An idle deadline for a compare value not divisible by 3 must still be reached:
/// converting ticks -> ns -> ticks with truncation both ways stopped one tick short and
/// the guest never woke (found in review of the RK3288 copy of this code).
#[test]
fn idle_until_the_reported_deadline_fires_the_timer() {
    for cval in [1000u64, 1001, 1002, 999_999] {
        assert!(ns_to_ticks(ticks_to_ns(cval)) >= cval, "cval {cval}");
    }
    // WFI loop with the physical timer due at 1000 ticks.
    let mut machine = Machine::with_ram_size(64 << 10);
    machine.soc.write32(RAM_BASE, 0xe320_f003).unwrap(); // wfi
    machine.soc.write32(RAM_BASE + 4, 0xeaff_fffd).unwrap(); // b wfi
    machine.cpu_mut().cntp_cval = 1000;
    machine.cpu_mut().cntp_ctl = 1;
    let mut woke = false;
    for _ in 0..10 {
        match machine.run(1000) {
            Stop::Idle(deadline) => machine.idle_until(deadline),
            _ => {}
        }
        if machine.cpu().physical_timer_irq() {
            woke = true;
            break;
        }
    }
    assert!(woke, "timer never fired: count {}", machine.cpu().cntpct);
}

/// TLBIALLIS on one core invalidates every core's TLB: core 1 must not keep using a
/// translation that core 0 removed (stale entries let Linux write into freed vmalloc pages).
#[test]
fn inner_shareable_tlb_invalidate_reaches_other_cores() {
    let mut machine = Machine::with_ram_size(64 << 10);
    let table = RAM_BASE + 0x4000;
    // VA 1 MiB -> RAM (the page under test) and an identity map for the code.
    machine.soc.write32(table + 4, 0x4000_0c02).unwrap();
    machine.soc.write32(table + 0x400 * 4, 0x4000_0c02).unwrap();
    machine.soc.write32(RAM_BASE, 0xee08_0f13).unwrap(); // mcr p15, 0, r0, c8, c3, 0
    machine.soc.write32(RAM_BASE + 0x100, 0xe591_0000).unwrap(); // ldr r0, [r1]
    machine.soc.write32(RAM_BASE + 0x104, 0xe591_0000).unwrap(); // ldr r0, [r1]
    machine.soc.write32(RAM_BASE + 0x200, 0x1234_5678).unwrap();
    for (k, pc) in [(0, RAM_BASE), (1, RAM_BASE + 0x100)] {
        let cpu = &mut machine.cpus[k];
        cpu.ttbr0 = table as u32;
        cpu.dacr = 1;
        cpu.sctlr |= 1;
        cpu.tlb_flush();
        cpu.r[15] = pc as u32;
        cpu.r[1] = 0x0010_0200;
    }
    machine.state[1] = CoreState::Run;
    machine.cpus[1].step(&mut machine.soc).unwrap();
    assert_eq!(machine.cpus[1].r[0], 0x1234_5678);
    machine.soc.write32(table + 4, 0).unwrap(); // unmap
    machine.step().unwrap(); // core 0: TLBIALLIS
    assert!(matches!(machine.cpus[1].step(&mut machine.soc), Err(Trap::Exception(..))));
    assert_eq!(machine.cpus[1].dfsr & 0x40f, 0x05, "translation fault, not a stale hit");
}
