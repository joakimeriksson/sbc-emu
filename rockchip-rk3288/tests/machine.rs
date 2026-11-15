use emu_core::{Bus, Fault, Trap};
use rockchip_rk3288::{
    cru, CoreState, Machine, Soc, BOOT_MAGIC, CRU_BASE, GICC_BASE, GICD_BASE, GRF_BASE,
    PMU_BASE, RAM_BASE, RK3288_MIDR, SECURE_PHYSICAL_TIMER_PPI, SMP_MAILBOX, SRAM_BASE,
    SRAM_SIZE, UART2_BASE,
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
    assert_eq!(machine.cpu().r[2], RK3288_MIDR);
    assert_eq!(machine.cpu().r[2] >> 4 & 0xfff, 0xc0d, "Cortex-A12 part number");
    assert_eq!(machine.cpu().r[3] & 0xfff, 0x500, "cpu@500: cluster 5, core 0");
    assert_eq!((machine.cpu().r[4] >> 24 & 3) + 1, 4, "four cores");
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
    // On-chip SRAM is memory (any width); unmodelled devices (here the PMU SRAM) read back
    // what was written and are recorded.
    soc.write32(SMP_MAILBOX + 8, 0x0030_0000).unwrap();
    soc.write8(SMP_MAILBOX + 4, 0xaf).unwrap();
    soc.write64(SRAM_BASE + SRAM_SIZE as u64 - 8, 0x1122_3344_5566_7788).unwrap();
    assert_eq!(soc.read32(SMP_MAILBOX + 8), Ok(0x0030_0000));
    assert_eq!(soc.read8(SMP_MAILBOX + 4), Ok(0xaf));
    assert_eq!(soc.read16(SRAM_BASE + SRAM_SIZE as u64 - 2), Ok(0x1122));
    soc.write32(0xff72_0010, 7).unwrap();
    assert_eq!(soc.read32(0xff72_0010), Ok(7));
    assert_eq!(soc.stub_pages, [0xff72_0000]);
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
    machine.cpu_mut().cntp_cval = 10;
    machine.cpu_mut().cntp_ctl = 1;
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

#[test]
fn cortex_a12_identity_and_errata_registers() {
    let mut machine = Machine::with_ram_size(64 << 10);
    program(
        &mut machine,
        &[
            0xee10_1f30, // mrc p15, 0, r1, c0, c0, 1 (CTR)
            0xee30_2f30, // mrc p15, 1, r2, c0, c0, 1 (CLIDR)
            0xe3a0_0001, // mov r0, #1
            0xee40_0f10, // mcr p15, 2, r0, c0, c0, 0 (CSSELR: L1 instruction cache)
            0xee30_3f10, // mrc p15, 1, r3, c0, c0, 0 (CCSIDR)
            0xe3a0_060f, // mov r0, #0xf00000
            0xee01_0f50, // mcr p15, 0, r0, c1, c0, 2 (CPACR: cp10/cp11 access)
            0xeef0_4a10, // vmrs r4, fpsid
            0xeef7_5a10, // vmrs r5, mvfr0
            // __ca12_errata (ARM_ERRATA_818325_852422, 821420): read-modify-write of the
            // diagnostic and internal feature registers.
            0xee1f_6f30, // mrc p15, 0, r6, c15, c0, 1
            0xe386_6a01, // orr r6, r6, #0x1000
            0xee0f_6f30, // mcr p15, 0, r6, c15, c0, 1
            0xee1f_7f50, // mrc p15, 0, r7, c15, c0, 2
            0xe387_7002, // orr r7, r7, #2
            0xee0f_7f50, // mcr p15, 0, r7, c15, c0, 2
            0xee1f_8f30, // mrc p15, 0, r8, c15, c0, 1
            0xee1f_9f50, // mrc p15, 0, r9, c15, c0, 2
            0xee19_af1c, // mrc p15, 0, r10, c9, c12, 0 (PMCR)
            0xe120_0070, // bkpt #0
        ],
    );
    for _ in 0..18 {
        machine.step().unwrap();
    }
    let r = machine.cpu().r;
    assert_eq!(r[1], 0x8444_8004, "CTR: VIPT L1 I-cache, 64-byte lines");
    assert_eq!(r[2], 0x0a20_0023, "CLIDR");
    assert_eq!(r[3], 0x200f_e01a, "CCSIDR: 32 KiB 4-way L1 I-cache");
    assert_eq!(r[4], 0x4103_30d0, "FPSID: VFPv4, variant 0xd as on RK3288");
    assert_eq!(r[5], 0x1011_0222, "MVFR0");
    assert_eq!((r[8], r[9]), (1 << 12, 1 << 1), "c15 registers keep what was written");
    assert_eq!(r[10] >> 16 & 0xff, 0x0d, "PMCR IDCODE: Cortex-A12");
    assert_eq!(machine.step(), Err(Trap::Brk(0)));
    // Every core has the same identity; MPIDR is cluster 5, core n.
    for (n, cpu) in machine.cpus.iter().enumerate() {
        assert_eq!((cpu.midr, cpu.mpidr), (RK3288_MIDR, 0x8000_0500 | n as u32));
        assert_eq!(cpu.model, arm_a32::CORTEX_A12);
    }
}

/// Power a secondary core up the way `rockchip_boot_secondary` does it and see the boot ROM
/// wait for the mailbox.
#[test]
fn secondary_core_follows_pmu_power_cru_reset_and_the_rom_mailbox() {
    let mut machine = Machine::with_ram_size(64 << 10);
    let release = |machine: &mut Machine, core: u32, on: bool| {
        let soc = &mut machine.soc;
        let reset = CRU_BASE + cru::SOFTRST_CON0;
        if !on {
            soc.write32(reset, 0x1_0001 << core).unwrap();
        }
        let con = soc.read32(PMU_BASE + cru::PMU_PWRDN_CON).unwrap();
        let con = if on { con & !(1 << core) } else { con | 1 << core };
        soc.write32(PMU_BASE + cru::PMU_PWRDN_CON, con).unwrap();
        assert_eq!(soc.read32(PMU_BASE + cru::PMU_PWRDN_ST).unwrap() >> core & 1, u32::from(!on));
        if on {
            soc.write32(reset, 0x1_0000 << core).unwrap();
        }
        machine.advance_time(0);
    };
    assert_eq!(machine.state, [CoreState::Run, CoreState::Off, CoreState::Off, CoreState::Off]);
    // Powered but held in reset: still off.
    machine.soc.write32(CRU_BASE + cru::SOFTRST_CON0, 0x0004_0004).unwrap();
    machine.soc.write32(PMU_BASE + cru::PMU_PWRDN_CON, 0b1010).unwrap();
    machine.advance_time(0);
    assert_eq!(machine.state[2], CoreState::Off);
    release(&mut machine, 2, true);
    assert_eq!(machine.state[2], CoreState::BootRom, "no magic yet: the ROM waits");
    // The ROM sleeps in WFE: the mailbox is looked at again only on an event.
    machine.soc.write32(SMP_MAILBOX + 8, 0x4000).unwrap();
    machine.soc.write32(SMP_MAILBOX + 4, BOOT_MAGIC).unwrap();
    machine.advance_time(0);
    assert_eq!(machine.state[2], CoreState::BootRom);
    // Powering down another core (reset asserted first, as Linux does) leaves it off.
    release(&mut machine, 3, false);
    assert_eq!(machine.state[3], CoreState::Off);
    // With the magic already present, a newly released core starts at once.
    release(&mut machine, 1, true);
    assert_eq!(machine.state[1], CoreState::Run);
    assert_eq!(machine.cpus[1].r[15], 0x4000);
    // CPU hotplug: cpu_kill powers the domain down.
    release(&mut machine, 1, false);
    assert_eq!(machine.state[1], CoreState::Off);
}

/// A bare-metal two-core program: core 0 starts core 1 through the PMU, the CRU and the SRAM
/// mailbox (the `rockchip,rk3066-smp` sequence) and sleeps in WFE until core 1 reports in.
#[test]
fn boot_rom_mailbox_starts_a_second_core_and_sev_wakes_wfe() {
    let mut machine = Machine::with_ram_size(64 << 10);
    let mut words = vec![
        0xe300_0000, // movw r0, #0
        0xe34f_0f76, // movt r0, #0xff76: CRU
        0xe300_1002, // movw r1, #2
        0xe340_1002, // movt r1, #2
        0xe580_11b8, // str r1, [r0, #0x1b8]: SOFTRST_CON0, assert core 1 reset
        0xe300_2000, // movw r2, #0
        0xe34f_2f73, // movt r2, #0xff73: PMU
        0xe592_3008, // ldr r3, [r2, #8]: PWRDN_CON
        0xe3c3_3002, // bic r3, r3, #2
        0xe582_3008, // str r3, [r2, #8]: power core 1 up
        0xe300_1000, // movw r1, #0
        0xe340_1002, // movt r1, #2
        0xe580_11b8, // str r1, [r0, #0x1b8]: release core 1 reset
        0xe300_4000, // movw r4, #0
        0xe34f_4f70, // movt r4, #0xff70: SRAM mailbox
        0xe3a0_5c01, // mov r5, #0x100: core 1 entry
        0xe584_5008, // str r5, [r4, #8]
        0xe30b_5eaf, // movw r5, #0xbeaf
        0xe34d_5ead, // movt r5, #0xdead
        0xe584_5004, // str r5, [r4, #4]
        0xf57f_f04f, // dsb sy
        0xe320_f004, // sev
        0xe3a0_7c02, // mov r7, #0x200
        0xe320_f002, // 0x5c: wfe
        0xe597_6000, // ldr r6, [r7]
        0xe356_0000, // cmp r6, #0
        0x0aff_fffb, // beq 0x5c
        0xe120_0071, // bkpt #1
    ];
    words.resize(0x100 / 4, 0);
    words.extend([
        0xee10_0fb0, // 0x100: mrc p15, 0, r0, c0, c0, 5 (MPIDR)
        0xe3a0_7c02, // mov r7, #0x200
        0xe587_0004, // str r0, [r7, #4]
        0xe10f_1000, // mrs r1, cpsr
        0xe587_1008, // str r1, [r7, #8]
        0xe3a0_1001, // mov r1, #1
        0xe587_1000, // str r1, [r7]
        0xf57f_f04f, // dsb sy
        0xe320_f004, // sev
        0xe320_f003, // 0x124: wfi
        0xeaff_fffd, // b 0x124
    ]);
    program(&mut machine, &words);
    assert_eq!(machine.run(100_000), emu_core::Stop::Brk(1));
    assert_eq!(machine.last_stop_core, 0);
    let soc = &mut machine.soc;
    assert_eq!(soc.read32(RAM_BASE + 0x204), Ok(0x8000_0501), "core 1 ran with its own MPIDR");
    assert_eq!(soc.read32(RAM_BASE + 0x208).unwrap() & 0xff, 0xd3, "ARM, SVC, IRQ/FIQ masked");
    assert_eq!(machine.cpus[1].sctlr & 1, 0, "MMU off");
    assert_eq!(machine.state[1], CoreState::Wait);
    assert_eq!(machine.state[2..], [CoreState::Off, CoreState::Off]);
}

#[test]
fn ipis_reach_the_target_core_only() {
    let mut machine = Machine::with_ram_size(64 << 10);
    let soc = &mut machine.soc;
    soc.write32(GICD_BASE, 1).unwrap();
    for core in 0..2 {
        soc.gic.current = core;
        soc.write32(GICC_BASE, 1).unwrap();
        soc.write32(GICC_BASE + 4, 0xff).unwrap();
    }
    soc.gic.current = 0;
    // GICD_SGIR: SGI 3 to the CPU target list {core 1}.
    soc.write32(GICD_BASE + 0xf00, 1 << 17 | 3).unwrap();
    assert!(!soc.gic.irq_out_for(0));
    assert!(soc.gic.irq_out_for(1));
    soc.gic.current = 1;
    assert_eq!(soc.read32(GICC_BASE + 0x0c), Ok(3), "SGI 3 from core 0");
}
