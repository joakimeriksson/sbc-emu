use arm_a64::sysreg::key;
use emu_core::{Bus, Stop};
use rk3588::{gic::{Gic, IAR, EOIR, DIR, PMR, CTLR, GROUP1}, uart::Uart, Machine, UART_IRQ, GICD, GICR};

fn enabled_gic() -> Gic {
    let mut g=Gic::default();
    g.dist_write(0,0x12);
    g.redist_write(0x14,0);
    g.redist_write(0x10080,u32::MAX);
    g.redist_write(0x10100,u32::MAX);
    g.dist_write(0x80+((UART_IRQ/32)*4) as u64,u32::MAX);
    g.dist_write(0x100+((UART_IRQ/32)*4) as u64,1<<(UART_IRQ%32));
    g.sys_write(PMR,0xf0);g.sys_write(GROUP1,1);
    g
}

#[test]
fn gic_level_irq_requires_deactivation_and_respects_masks() {
    let mut g=enabled_gic();
    g.sys_write(CTLR,2); // split EOI/deactivate as used by Linux
    g.set_line(UART_IRQ,true);
    assert!(g.irq());
    assert_eq!(g.sys_read(IAR),Some(UART_IRQ as u64));
    assert!(!g.irq());
    g.sys_write(EOIR,UART_IRQ as u64);
    assert!(!g.irq(),"EOI alone must not deactivate in split mode");
    g.sys_write(DIR,UART_IRQ as u64);
    assert!(g.irq(),"asserted level must re-pend");
    g.sys_write(PMR,0x80);assert!(!g.irq());
    g.sys_write(PMR,0xf0);assert!(g.irq());
    g.dist_write(0x180+((UART_IRQ/32)*4) as u64,1<<(UART_IRQ%32));
    assert!(!g.irq());
}

#[test]
fn gic_priority_preemption_and_edge_latching() {
    let mut g=enabled_gic();
    g.redist_write(0x10418,0x40a0_a0a0); // PPI27 priority 0x40
    g.set_line(UART_IRQ,true);
    assert_eq!(g.sys_read(IAR),Some(UART_IRQ as u64));
    g.set_line(27,true);
    assert_eq!(g.sys_read(IAR),Some(27));
    g.set_line(27,false);
    g.sys_write(EOIR,27);assert!(!g.irq());
    g.set_line(UART_IRQ,false);g.sys_write(EOIR,UART_IRQ as u64);
    g.redist_write(0x10c04,2 << ((27-16)*2));
    g.set_line(27,true);g.set_line(27,false);
    assert_eq!(g.sys_read(IAR),Some(27),"short edge remains latched");
    g.sys_write(EOIR,27);assert_eq!(g.sys_read(IAR),Some(1023));
    g.sys_write(key(3,0,12,11,5),(3<<24)|1); // SGI3 to affinity 0
    assert_eq!(g.sys_read(IAR),Some(3));
}

#[test]
fn uart_divisor_fifo_loopback_and_interrupt_acknowledge() {
    let mut u=Uart::default();
    u.write(12,0x80);u.write(0,13);u.write(4,1);
    assert_eq!(u.read(0),13);assert_eq!(u.read(4),1);assert!(u.out.is_empty());
    u.write(12,3);u.write(8,1);u.write(4,3);
    assert!(u.irq());assert_eq!(u.read(8),0xc2);assert!(!u.irq());
    for b in 0..64 {assert!(u.input(b));}
    assert!(!u.input(64));assert!(u.irq());assert_eq!(u.read(8),0xc4);
    for b in 0..64 {assert_eq!(u.read(0),b);}
    assert!(!u.irq());
    u.write(16,0x1f);assert_eq!(u.read(24),0xf0);
    u.write(0,b'x' as u32);assert_eq!(u.read(0),b'x' as u32);assert!(u.out.is_empty());
    u.write(16,0);u.write(0,b'y' as u32);assert_eq!(u.out,b"y");
    u.write(8,7);assert_eq!(u.read(0x84),0);
}

#[test]
fn byte_priorities_and_64bit_affinity_registers() {
    let mut m=Machine::new(128<<20).unwrap();
    m.soc.write32(GICD+0x420,0xa0a0_a0a0).unwrap();
    m.soc.write8(GICD+0x421,0x47).unwrap();
    assert_eq!(m.soc.read32(GICD+0x420).unwrap(),0xa0a0_40a0);
    assert_eq!(m.soc.read64(GICR+8).unwrap(),0x10); // affinity 0, last redistributor
    m.soc.write64(GICD+0x6000+UART_IRQ as u64*8,0x100_0000_0000).unwrap();
    assert_eq!(m.soc.read64(GICD+0x6000+UART_IRQ as u64*8).unwrap(),0x100_0000_0000);
}

#[test]
fn timer_wakes_waiting_core_at_24mhz() {
    let mut m=Machine::new(128<<20).unwrap();m.soc.gic=enabled_gic();
    m.cpu.pc=0x80000;
    m.soc.ram[0x80000..0x80004].copy_from_slice(&0xd503207fu32.to_le_bytes()); // WFI
    m.cpu.sys.vtimer.ctl=1;m.cpu.sys.vtimer.cval=24_000;
    let Stop::Idle(deadline)=m.run(100) else {panic!("expected WFI idle")};
    assert_eq!(deadline,1_000_000);
    m.idle_until(deadline-1);assert!(!m.cpu.irq);
    m.idle_until(deadline);assert!(m.cpu.irq);
    assert_eq!(m.soc.gic.sys_read(IAR),Some(27));
}

#[test]
fn boot_rejects_invalid_layout_without_panicking() {
    assert!(Machine::new(1).is_err());
    let mut m=Machine::new(128<<20).unwrap();
    let mut image=vec![0;64];image[56..60].copy_from_slice(b"ARMd");
    image[8..16].copy_from_slice(&u64::MAX.to_le_bytes());
    assert!(rk3588::boot::boot_linux(&mut m,&image,None,"").is_err());
}

#[test]
fn private_banks_are_isolated_and_byte_clear_is_write_one() {
    let mut m=Machine::new(128<<20).unwrap();
    m.soc.write32(GICR+0x10100,0xffff).unwrap();
    m.soc.write8(GICR+0x10180,1).unwrap();
    assert_eq!(m.soc.read32(GICR+0x10100).unwrap(),0xfffe);
    m.soc.write32(GICD+0x100,u32::MAX).unwrap();
    assert_eq!(m.soc.read32(GICD+0x100).unwrap(),0);
    assert_eq!(m.soc.read32(GICR+0x10100).unwrap(),0xfffe);
    m.soc.write32(GICD+0x420,0x20202020).unwrap();
    m.soc.write32(GICR+0x10420,0xffffffff).unwrap();
    assert_eq!(m.soc.read32(GICD+0x420).unwrap(),0x20202020);
}
#[test]
fn uart_reset_preserves_unread_output() {
    let mut u=Uart::default();u.write(0,b'A' as u32);u.write(0x88,1);u.write(0,b'B' as u32);
    assert_eq!(u.out,b"AB");
}

#[test]
fn gmac_partial_writes_preserve_neighbors_and_idle_does_not_poll_local_network() {
    let mut m=Machine::new(128<<20).unwrap();
    m.soc.write32(rk3588::GMAC+0x1134,0x12345678).unwrap();
    m.soc.write8(rk3588::GMAC+0x1135,0xab).unwrap();
    assert_eq!(m.soc.read32(rk3588::GMAC+0x1134).unwrap(),0x1234ab78);
    m.connect_network(false,false);
    m.soc.ram[..4].copy_from_slice(&0xd503207fu32.to_le_bytes());
    m.cpu.pc=0;
    let Stop::Idle(deadline)=m.run(10) else {panic!("expected WFI")};
    assert!(deadline-m.now()>1_000_000);
}
