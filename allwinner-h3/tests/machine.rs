use allwinner_h3::{
    Machine, Soc, GICC_BASE, GICD_BASE, PHYSICAL_TIMER_PPI, RAM_BASE, SPURIOUS, UART0_BASE,
};
use emu_core::{Bus, Core, Fault, Trap};

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
    assert_eq!(machine.cpu.pc(), RAM_BASE + 24);
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
    assert_eq!(soc.read8(UART0_BASE + 1), Err(Fault::Unmapped));
    assert_eq!(soc.read64(UART0_BASE), Err(Fault::Prohibited));
    assert_eq!(soc.read32(UART0_BASE - 4), Err(Fault::Unmapped));

    for byte in 0..32 {
        assert!(soc.uart.push_rx(byte));
    }
    assert!(!soc.uart.push_rx(32));
    assert_eq!(soc.read8(UART0_BASE), Ok(0));
    assert!(soc.uart.push_rx(32));
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
    machine.cpu.vbar = (RAM_BASE + 0x100) as u32;
    machine
        .load(RAM_BASE + 0x118, &0xe120_0070u32.to_le_bytes())
        .unwrap();
    machine.cpu.cpsr &= !(1 << 7);

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
    assert_eq!(machine.cpu.r[0], 24_000_000);
    assert_eq!(machine.step(), Err(Trap::Wait));
    assert_eq!(machine.step(), Err(Trap::Wait));
    assert_eq!(machine.step(), Err(Trap::Interrupt(0, 1)));
    assert_eq!(machine.cpu.pc(), RAM_BASE + 0x118);
    assert_eq!(machine.cpu.r[14], (RAM_BASE + 28) as u32);
    assert_eq!(
        machine.soc.read32(GICC_BASE + 0x0c),
        Ok(PHYSICAL_TIMER_PPI as u32)
    );

    machine.cpu.cntp_ctl = 0;
    machine.advance_time(0);
    machine
        .soc
        .write32(GICC_BASE + 0x10, PHYSICAL_TIMER_PPI as u32)
        .unwrap();
    machine.advance_time(0);
    assert!(!machine.soc.gic.irq_out());
    assert!(!machine.cpu.irq);
    assert_eq!(machine.step(), Err(Trap::Brk(0)));
}
