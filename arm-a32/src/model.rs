//! Processor identity: the read-only identification, cache and feature registers a guest
//! uses to recognise the core, and the implementation-defined registers that come with it.
//!
//! `Cpu::new` uses `CORTEX_A7`; a machine replaces it with `Cpu::set_model`. The emulated
//! behaviour is the same for every model; only what the guest reads changes, so a preset
//! advertises exactly the features `arm-a32` implements (no CP14 debug, no PMU event
//! counters, no Advanced SIMD) even where the real core has more. Those deviations are
//! noted next to each value.

/// Identification register values of one processor model.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CpuModel {
    /// MIDR. `Cpu::set_model` copies it to `Cpu::midr`, which a machine may still override.
    pub midr: u32,
    pub revidr: u32,
    pub ctr: u32,
    pub clidr: u32,
    /// CCSIDR for CSSELR = 0 (L1 data), 1 (L1 instruction) and 2 (L2 unified; CSSELR 3
    /// reads the same).
    pub ccsidr: [u32; 3],
    pub id_pfr0: u32,
    pub id_pfr1: u32,
    pub id_dfr0: u32,
    pub id_afr0: u32,
    pub id_mmfr: [u32; 4],
    pub id_isar: [u32; 6],
    /// L2CTLR (`mrc p15, 1, Rt, c9, c0, 2`); bits 25:24 are the number of cores - 1.
    pub l2ctlr: u32,
    /// PMCR as read at reset (IDCODE, number of event counters).
    pub pmcr: u32,
    pub fpsid: u32,
    pub mvfr0: u32,
    pub mvfr1: u32,
    /// The Cortex-A12/A17 diagnostic (`c15, c0, 1`) and internal feature (`c15, c0, 2`)
    /// registers exist (Linux's names; the A17 TRM calls them DGNCTLR1/2, reset UNK; here
    /// read/write storage, reset 0). The kernel's A12/A17 errata workarounds set bits in
    /// them. When false, CP15 c15 is UNDEFINED.
    pub c15_diagnostic: bool,
}

/// ARM Cortex-A7 MPCore r0p5 as the Allwinner H3 has it (the values `arm-a32` always used).
/// Deviations from the silicon: ID_DFR0 reports no CP14 debug, PMCR no event counters,
/// MVFR1 no Advanced SIMD or half-precision, and the L2 geometry is a placeholder.
pub const CORTEX_A7: CpuModel = CpuModel {
    midr: 0x410f_c075,
    revidr: 0,
    ctr: 0x8444_8003,
    clidr: 0x0a20_0023,
    ccsidr: [
        0x701f_e00a, // 32 KiB L1 data cache
        0x201f_e00a, // 32 KiB L1 instruction cache
        0x711f_e07a, // L2 unified cache
    ],
    id_pfr0: 0x0000_1131,
    id_pfr1: 0x0001_1011,
    id_dfr0: 0x0200_0000, // PMUv2, no CP14 debug
    id_afr0: 0,
    id_mmfr: [0x1010_1105, 0x4000_0000, 0x0124_0000, 0x0210_2211],
    id_isar: [0x0210_1110, 0x1311_2111, 0x2123_2041, 0x1111_2131, 0x1001_1142, 0],
    l2ctlr: 0x0300_0000, // four cores
    pmcr: 0x4107_0000,   // Cortex-A7, no event counters
    fpsid: 0x4102_3075,  // VFPv4
    mvfr0: 0x1011_0222,  // 32 double registers, single/double, divide, sqrt
    mvfr1: 0x1000_0011,  // fused multiply-add; no Advanced SIMD
    c15_diagnostic: false,
};

/// ARM Cortex-A12 MPCore r0p1 as the RK3288 has it: four cores, 32 KiB L1 I and D caches,
/// 1 MiB L2. Arm renamed the A12 the Cortex-A17 (part 0xc0e) and publishes no A12 TRM, so
/// the values come from the Cortex-A17 MPCore TRM r1p1 (DDI 0535C, chapter 4 register
/// summary, Table 4-47 for CCSIDR, Table 13-2 for the FP registers), which still lists
/// A12 identity values in places (FPSID variant 0xD, PMCR IDCODE 0x0D), cross-checked with
/// RK3288 boot logs where they show a value. Not verified for the A12 itself: the ID_*,
/// CLIDR and CCSIDR values (A17 r1p1 values; every RK3288-visible consequence agrees:
/// hwcaps, "VIPT aliasing instruction cache", divide instructions) and CTR (see below).
/// Deviations as for the A7: no CP14 debug, no PMU event counters, no Advanced SIMD.
pub const CORTEX_A12: CpuModel = CpuModel {
    midr: 0x410f_c0d1, // RK3288 dmesg: "CPU: ARMv7 Processor [410fc0d1] revision 1"
    revidr: 0,
    // VIPT L1 I-cache, 64-byte lines. The TRM gives 0x84448003 or 0x84448004 depending on
    // the IMINLN input; the RK3288's strap is not verified (IminLine 4 = the 64-byte line).
    ctr: 0x8444_8004,
    clidr: 0x0a20_0023,
    ccsidr: [
        0x700f_e01a, // 32 KiB L1 data: 4-way, 64-byte lines, WB/RA/WA
        0x200f_e01a, // 32 KiB L1 instruction: 4-way, 64-byte lines
        0x707f_e07a, // 1 MiB L2 unified: 16-way, 64-byte lines, WB/RA/WA
    ],
    id_pfr0: 0x0000_1131,
    id_pfr1: 0x0001_1011,
    // TRM: 0x02010555 (v7.1 debug, memory-mapped and CP14); the debug fields read zero
    // because `arm-a32` has no CP14 debug registers.
    id_dfr0: 0x0200_0000,
    id_afr0: 0,
    // NOT verified: ID_MMFR0-3 and ID_ISAR0-3 are the Cortex-A7 preset's values (no A12 or
    // A17 source confirmed them; A15-class cores may report e.g. ID_MMFR0 0x10201105).
    // ID_ISAR4 differs from the A7 only in SWP_frac (0: SWP is not in the core).
    id_mmfr: [0x1010_1105, 0x4000_0000, 0x0124_0000, 0x0210_2211],
    id_isar: [0x0210_1110, 0x1311_2111, 0x2123_2041, 0x1111_2131, 0x0001_1142, 0],
    // Four cores (bits 25:24). Hardware reset has every RAM latency field at 0b111; this
    // is the value U-Boot's rk3288 `configure_l2ctlr` leaves (the model boots as U-Boot).
    l2ctlr: 0x0300_0009,
    // TRM: 0x410D3000 (IDCODE 0x0D, six counters; RK3288 Linux reports "7 counters
    // available" with the cycle counter). N reads zero: no event counters are modelled.
    pmcr: 0x410d_0000,
    // VFPv4: RK3288 dmesg "VFP support v0.3: implementor 41 architecture 3 part 30 variant
    // d rev 0" (TRM Table 13-2).
    fpsid: 0x4103_30d0,
    mvfr0: 0x1011_0222,
    // TRM: 0x11111111. Advanced SIMD and half-precision fields stay "not implemented"
    // until `arm-a32` implements NEON (same value as the A7 preset).
    mvfr1: 0x1000_0011,
    c15_diagnostic: true,
};
