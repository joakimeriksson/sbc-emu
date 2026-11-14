use bcm2711::gic::Gic;

/// Linux's GICv2 bring-up, then a level SPI and an SGI between two CPUs.
#[test]
fn spi_and_sgi() {
    let mut g = Gic::with_cpus(2);
    let mask = g.dist_read(0, 0x800) & 0xff;
    assert_eq!(mask, 1);
    for i in (32..256).step_by(4) { g.dist_write(0, 0x800 + i as u64, 0x0101_0101); g.dist_write(0, 0x400 + i as u64, 0xa0a0_a0a0); }
    g.dist_write(0, 0x000, 1);
    for c in 0..2 { g.cpu_write(c, 0x04, 0xf0); g.cpu_write(c, 0x00, 1); g.dist_write(c, 0x100, 0xffff); }
    g.dist_write(0, 0x104, 1 << 1); // enable SPI 33 (the mailbox)
    g.set_level(33, true);
    assert!(g.irq_out(0));
    assert!(!g.irq_out(1));
    assert_eq!(g.cpu_read(0, 0x0c), 33);
    g.set_level(33, false);
    g.cpu_write(0, 0x10, 33);
    assert!(!g.irq_out(0));
    // CPU 0 sends SGI 1 to CPU 1
    g.dist_write(0, 0xf00, (2 << 16) | 1);
    assert!(!g.irq_out(0));
    assert!(g.irq_out(1));
    assert_eq!(g.cpu_read(1, 0x0c), 1);
    g.cpu_write(1, 0x10, 1);
    assert!(!g.irq_out(1));
}
