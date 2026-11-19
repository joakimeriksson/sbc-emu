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
    m.cpus[0].pc=0x80000;
    m.soc.ram[0x80000..0x80004].copy_from_slice(&0xd503207fu32.to_le_bytes()); // WFI
    m.cpus[0].sys.vtimer.ctl=1;m.cpus[0].sys.vtimer.cval=24_000;
    let Stop::Idle(deadline)=m.run(100) else {panic!("expected WFI idle")};
    assert_eq!(deadline,1_000_000);
    m.idle_until(deadline-1);assert!(!m.cpus[0].irq);
    m.idle_until(deadline);assert!(m.cpus[0].irq);
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
    m.cpus[0].pc=0;
    let Stop::Idle(deadline)=m.run(10) else {panic!("expected WFI")};
    assert!(deadline-m.now()>1_000_000);
}

/// Enable PE `pe`'s CPU interface and redistributor and all SGIs/PPIs on it.
fn enable_pe(g: &mut Gic, pe: usize) {
    let f = pe as u64 * rk3588::gic::FRAME;
    g.redist_write(f+0x14,0);
    g.redist_write(f+0x10080,u32::MAX);
    g.redist_write(f+0x10100,u32::MAX);
    g.cur=pe; g.sys_write(PMR,0xf0); g.sys_write(GROUP1,1);
}

#[test]
fn eight_redistributors_report_core_affinities() {
    let g=Gic::with_cpus(8);
    for pe in 0..8u64 {
        let f=pe*rk3588::gic::FRAME;
        let typer=g.redist_read(f+8) as u64 | (g.redist_read(f+12) as u64) << 32;
        assert_eq!(typer>>32,pe<<8,"Aff1 = core");
        assert_eq!((typer>>8)&0xffff,pe,"processor number");
        assert_eq!(typer&0x10!=0,pe==7,"Last only on the final frame");
    }
    assert_eq!((g.dist_read(4)>>5)&7,7,"GICD_TYPER.CPUNumber");
}

#[test]
fn sgis_reach_only_their_targets() {
    let mut g=Gic::with_cpus(8);
    g.dist_write(0,0x12);
    for pe in 0..8 { enable_pe(&mut g,pe); }
    g.cur=0;
    g.sys_write(rk3588::gic::SGI1R,(1<<24)|(5<<16)|1); // SGI1 to Aff1=5 (core 5)
    assert_eq!((0..8).filter(|&k|g.irq_out(k)).collect::<Vec<_>>(),[5]);
    g.cur=5; assert_eq!(g.sys_read(IAR),Some(1)); g.sys_write(EOIR,1);
    g.cur=2;
    g.sys_write(rk3588::gic::SGI1R,(2<<24)|(1<<40)); // IRM: everyone but core 2
    assert_eq!((0..8).filter(|&k|g.irq_out(k)).collect::<Vec<_>>(),[0,1,3,4,5,6,7]);
}

#[test]
fn spis_follow_their_affinity_route() {
    let mut g=Gic::with_cpus(8);
    g.dist_write(0,0x12);
    for pe in 0..8 { enable_pe(&mut g,pe); }
    g.dist_write(0x80+((UART_IRQ/32)*4) as u64,u32::MAX);
    g.dist_write(0x100+((UART_IRQ/32)*4) as u64,1<<(UART_IRQ%32));
    g.dist_write(0x6000+UART_IRQ as u64*8,6<<8); // to core 6
    g.set_line(UART_IRQ,true);
    assert_eq!((0..8).filter(|&k|g.irq_out(k)).collect::<Vec<_>>(),[6]);
    g.dist_write(0x6000+UART_IRQ as u64*8,1<<31); // IRM: any core; the first to acknowledge takes it
    assert!((0..8).all(|k|g.irq_out(k)));
    g.cur=3; assert_eq!(g.sys_read(IAR),Some(UART_IRQ as u64));
    assert!((0..8).all(|k|!g.irq_out(k)),"active on core 3");
}

#[test]
fn psci_cpu_on_starts_a_big_core_with_its_identity() {
    let mut m=Machine::with_cpus(128<<20,8).unwrap();
    // core 0: CPU_ON(0x400, entry 0x90000, context 0x1234) by SMC, then WFI
    let code=[0xd2800060u32 /* movz x0,#3 */, 0xf2b08000 /* movk x0,#0x8400,lsl16 */, 0xd2808001 /* movz x1,#0x400 */,
        0xd2920002 /* movz x2,#0x9000 */, 0xd37cec42 /* lsl x2,x2,#4 */, 0xd2824683 /* movz x3,#0x1234 */, 0xd4000003 /* smc */, 0xd503207f /* wfi */];
    for (i,w) in code.iter().enumerate() { m.soc.ram[0x80000+i*4..0x80004+i*4].copy_from_slice(&w.to_le_bytes()); }
    // core 4: read MPIDR_EL1 and MIDR_EL1 into x1/x2, then WFI
    for (i,w) in [0xd53800a1u32 /* mrs x1,mpidr_el1 */, 0xd5380002 /* mrs x2,midr_el1 */, 0xd503207f].iter().enumerate() { m.soc.ram[0x90000+i*4..0x90004+i*4].copy_from_slice(&w.to_le_bytes()); }
    m.cpus[0].pc=0x80000; m.cpus[0].el=1;
    let _=m.run(1000);
    assert_eq!(m.cpus[0].x[0],0,"CPU_ON succeeded");
    assert_eq!(m.state[4],rk3588::CoreState::Wfi);
    assert_eq!(m.cpus[4].x[0],0x1234);
    assert_eq!(m.cpus[4].x[1],0x8100_0400);
    assert_eq!(m.cpus[4].x[2],0x414f_d0b0,"Cortex-A76");
    assert!(m.state.iter().enumerate().all(|(k,s)|k==0||k==4||*s==rk3588::CoreState::Off));
}

#[test]
fn hdmi_framebuffer_is_handed_over_and_reserved() {
    use rk3588::display::{framebuffer_addr, Mode};
    let mut m=Machine::new(256<<20).unwrap();
    m.enable_hdmi(Mode{width:1920,height:1080});
    let mut image=vec![0;4096];image[56..60].copy_from_slice(b"ARMd");
    rk3588::boot::boot_linux(&mut m,&image,None,"").unwrap();
    let addr=framebuffer_addr(256<<20);
    let has=|s:&[u8]|m.dtb.windows(s.len()).any(|w|w==s);
    assert!(has(b"simple-framebuffer") && has(b"x8r8g8b8") && has(b"no-map"));
    assert!(has(format!("framebuffer@{addr:x}").as_bytes()));
    assert_eq!(m.soc.display.shown_size(),Some((1920,1080)));
    let mut plain=Machine::new(256<<20).unwrap();
    rk3588::boot::boot_linux(&mut plain,&image,None,"").unwrap();
    assert!(!plain.dtb.windows(18).any(|w|w==b"simple-framebuffer"),"no HDMI, no handoff");
}

#[test]
fn usb_host_is_a_platform_xhci_with_keyboard_and_tablet() {
    let mut m=Machine::new(128<<20).unwrap();
    assert_eq!(m.soc.read32(rk3588::XHCI).unwrap()>>16,0x0100,"HCIVERSION 1.0");
    assert_eq!(m.soc.xhci.attached(),[emu_core::usb::DevId::Keyboard,emu_core::usb::DevId::Tablet]);
    let dt=rk3588::boot::device_tree(128<<20,"",None,false);
    assert!(dt.windows(12).any(|w|w==b"generic-xhci"));
}

#[test]
fn m2_slot_is_an_ecam_host_with_an_intx_nvme() {
    let mut m=Machine::new(128<<20).unwrap();
    assert_eq!(m.soc.read32(rk3588::pcie::ECAM).unwrap(),u32::MAX,"empty slot");
    m.attach_nvme(Box::new(emu_core::disk::MemDisk(vec![0u8;1<<20])));
    assert_eq!(m.soc.read32(rk3588::pcie::ECAM).unwrap(),0x0001_fffd);
    let dt=rk3588::boot::device_tree(128<<20,"",None,false);
    assert!(dt.windows(21).any(|w|w==b"pci-host-ecam-generic"));
}
