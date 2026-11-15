use emu_core::{Bus, Fault, Trap};
use rockchip_rk3288::{
    cru, Machine, Soc, CRU_BASE, GICC_BASE, GICD_BASE, GRF_BASE, PMU_BASE, RAM_BASE,
    RK3288_MIDR, SECURE_PHYSICAL_TIMER_PPI, UART2_BASE,
};

fn program(machine: &mut Machine, instructions: &[u32]) {
    let bytes: Vec<u8> = instructions.iter().flat_map(|insn| insn.to_le_bytes()).collect();
    machine.load(RAM_BASE, &bytes).unwrap();
}

#[test]
fn a32_program_reads_cortex_a12_identity_and_writes_uart2() {
    let mut machine = Machine::with_ram_size(64 << 10);
    program(
        &mut machine,
        &[
            0xe300_0000, // movw r0, #0
            0xe34f_0f69, // movt r0, #0xff69: r0 = UART2_BASE
            0xe3a0_104f, // mov r1, #'O'
            0xe580_1000, // str r1, [r0]
            0xe3a0_104b, // mov r1, #'K'
            0xe580_1000, // str r1, [r0]
            0xee10_2f10, // mrc p15, 0, r2, c0, c0, 0 (MIDR)
            0xee10_3fb0, // mrc p15, 0, r3, c0, c0, 5 (MPIDR)
            0xee39_4f50, // mrc p15, 1, r4, c9, c0, 2 (L2CTLR: core count for platsmp)
            0xe120_0070, // bkpt #0
        ],
    );
    for _ in 0..9 {
        machine.step().unwrap();
    }
    assert_eq!(machine.take_serial_output(), b"OK");
    assert_eq!(machine.cpu.r[2], RK3288_MIDR);
    assert_eq!(machine.cpu.r[2] >> 4 & 0xfff, 0xc0d, "Cortex-A12 part number");
    assert_eq!(machine.cpu.r[3] & 0xfff, 0x500, "cpu@500: cluster 5, core 0");
    assert_eq!((machine.cpu.r[4] >> 24 & 3) + 1, 4, "four cores");
    assert_eq!(machine.step(), Err(Trap::Brk(0)));
}

#[test]
fn uart2_is_word_only_and_device_window_is_bounded() {
    let mut soc = Soc::new(64 << 10);
    assert_eq!(soc.read32(UART2_BASE + 0x14), Ok(0x60));
    assert_eq!(soc.read8(UART2_BASE), Err(Fault::Prohibited));
    assert_eq!(soc.read32(UART2_BASE + 0xfc), Ok(0x4457_0110)); // DesignWare CTR
    // DRAM ends where the model says; the gap up to the device window is unmapped.
    assert_eq!(soc.read32(64 << 10), Err(Fault::Unmapped));
    assert_eq!(soc.read32(0xfeff_fffc), Err(Fault::Unmapped));
    // Unmodelled devices and on-chip SRAM read back what was written.
    soc.write32(0xff70_0008, 0x0030_0000).unwrap();
    soc.write8(0xff70_0004, 0xaf).unwrap();
    assert_eq!(soc.read32(0xff70_0008), Ok(0x0030_0000));
    assert_eq!(soc.read8(0xff70_0004), Ok(0xaf));
    assert_eq!(soc.stub_pages, [0xff70_0000]);
}

#[test]
fn clock_registers_are_hiword_and_pll_lock_is_visible_in_grf() {
    let mut soc = Soc::new(64 << 10);
    let mode = CRU_BASE + cru::MODE_CON;
    assert_eq!(soc.read32(GRF_BASE + cru::GRF_SOC_STATUS1), Ok(0x3e0));
    soc.write32(mode, 0x0003_0000).unwrap(); // APLL to slow mode
    assert_eq!(soc.read32(mode).unwrap() & 3, 0);
    assert_eq!(soc.cru.pll_rate(0), 24_000_000);
    soc.write32(CRU_BASE + 0x0c, 0x0002_0002).unwrap(); // APLL power-down
    assert_eq!(soc.read32(GRF_BASE + cru::GRF_SOC_STATUS1), Ok(0x3a0));
    // GRF writes are masked too.
    soc.write32(GRF_BASE + 0x244, 0xffff_1234).unwrap();
    soc.write32(GRF_BASE + 0x244, 0x00f0_0000).unwrap();
    assert_eq!(soc.read32(GRF_BASE + 0x244), Ok(0x1204));
    assert_eq!(soc.read16(CRU_BASE), Err(Fault::Prohibited));
}

#[test]
fn power_domains_and_global_reset() {
    let mut machine = Machine::with_ram_size(64 << 10);
    let soc = &mut machine.soc;
    soc.write32(PMU_BASE + cru::PMU_PWRDN_CON, 1 << 9).unwrap();
    assert_eq!(soc.read32(PMU_BASE + cru::PMU_PWRDN_ST), Ok(1 << 9));
    soc.write32(CRU_BASE + cru::GLB_SRST_FST, 0xfdb9).unwrap();
    program(&mut machine, &[0xe320_f000]); // nop
    assert_eq!(machine.run(10), emu_core::Stop::Reset);
}

#[test]
fn secure_physical_timer_reaches_the_gic() {
    let mut machine = Machine::with_ram_size(64 << 10);
    let ppi = SECURE_PHYSICAL_TIMER_PPI;
    let soc = &mut machine.soc;
    soc.write32(GICD_BASE, 1).unwrap();
    soc.write32(GICD_BASE + 0x100, 1 << ppi).unwrap();
    soc.write32(GICC_BASE, 1).unwrap();
    soc.write32(GICC_BASE + 4, 0xff).unwrap();
    machine.cpu.cntp_cval = 10;
    machine.cpu.cntp_ctl = 1;
    machine.advance_time(20);
    assert!(machine.soc.gic.irq_out());
    assert_eq!(machine.soc.read32(GICC_BASE + 0x0c), Ok(ppi as u32));
}

#[test]
fn boot_rejects_non_zimage_kernels() {
    let mut machine = Machine::with_ram_size(64 << 20);
    let error = rockchip_rk3288::boot::boot_linux(&mut machine, &[0; 64], &[], None, "")
        .unwrap_err();
    assert!(error.contains("zImage"));
}
