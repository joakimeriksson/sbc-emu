//! The CPU complex's frequency registers (`nvidia,tegra234-ccplex-cluster`, Linux
//! `tegra194-cpufreq`): per core, a scratch register holding the NDIV the core's clock runs at
//! (`CMU_CLKS_BASE + 8 * id`), and an activity-monitor counter pair read as one 64-bit word
//! (`0x30000 + 0x10000 * cluster + 0x9000 + 8 * core`): the reference clock (408 MHz, PLLP)
//! in the high half and the core's clock cycles in the low half. `id` is the physical core,
//! `4 * cluster + core` from the MPIDR (the Orin Nano's cores 4 and 5 are cluster 1's cores 2
//! and 3: ids 6 and 7).
//!
//! The core clock counts at NDIV × 12.8 MHz (the BPMP's `MRQ_CPU_NDIV_LIMITS`), busy or idle
//! (a core here does not gate its clock in WFI).
use crate::bpmp::{CPU_MDIV, CPU_NDIV_MAX, CPU_NDIV_MIN, CPU_PDIV, CPU_REF_CLK_HZ};

pub const BASE: u64 = 0x0e00_0000;
pub const SIZE: u64 = 0x6_0000;
const CMU_CLKS: u64 = 0x2000;
const ACTMON: u64 = 0x3_0000 + 0x9000;
const CLUSTER_STRIDE: u64 = 0x1_0000;
/// three clusters of four cores in the register map
const IDS: usize = 12;
const REF_CLK_MHZ: u64 = 408;

/// The physical core id of an MPIDR affinity (`4 * Aff2 + Aff1`).
pub fn id(affinity: u64) -> usize {
    ((affinity >> 16 & 0xff) * 4 + (affinity >> 8 & 0xff)) as usize
}

pub struct Ccplex {
    ndiv: [u32; IDS],
    /// per core: core clock cycles × 10⁶ counted up to `since` (Σ ns × kHz)
    cycles: [u128; IDS],
    since: u64,
}

impl Default for Ccplex {
    fn default() -> Self {
        // the bootloader leaves the cores at their highest rate
        Self { ndiv: [u32::from(CPU_NDIV_MAX); IDS], cycles: [0; IDS], since: 0 }
    }
}

impl Ccplex {
    /// The core clock of physical core `id` (kHz).
    pub fn khz(&self, id: usize) -> u64 {
        u64::from(CPU_REF_CLK_HZ) / 1000 * u64::from(self.ndiv[id])
            / u64::from(CPU_PDIV * CPU_MDIV)
    }

    /// The highest core clock (kHz).
    pub fn max_khz() -> u64 {
        u64::from(CPU_REF_CLK_HZ) / 1000 * u64::from(CPU_NDIV_MAX) / u64::from(CPU_PDIV * CPU_MDIV)
    }

    fn sync(&mut self, now: u64) {
        let dt = u128::from(now.saturating_sub(self.since));
        for id in 0..IDS {
            self.cycles[id] += dt * u128::from(self.khz(id));
        }
        self.since = self.since.max(now);
    }

    pub fn read(&mut self, offset: u64, now: u64) -> u32 {
        self.sync(now);
        match offset {
            o if (CMU_CLKS..CMU_CLKS + 8 * IDS as u64).contains(&o) && o % 8 == 0 => {
                self.ndiv[((o - CMU_CLKS) / 8) as usize]
            }
            o if (ACTMON..ACTMON + 3 * CLUSTER_STRIDE).contains(&o) => {
                let cluster = (o - ACTMON) / CLUSTER_STRIDE;
                let within = (o - ACTMON) % CLUSTER_STRIDE;
                if within >= 32 {
                    return 0;
                }
                let id = (cluster * 4 + within / 8) as usize;
                if within % 8 == 0 {
                    (self.cycles[id] / 1_000_000) as u32
                } else {
                    (u128::from(now) * u128::from(REF_CLK_MHZ) / 1000) as u32
                }
            }
            _ => 0,
        }
    }

    pub fn write(&mut self, offset: u64, value: u32, now: u64) {
        self.sync(now);
        if (CMU_CLKS..CMU_CLKS + 8 * IDS as u64).contains(&offset) && offset % 8 == 0 {
            let ndiv = (value & 0x1ff).clamp(u32::from(CPU_NDIV_MIN), u32::from(CPU_NDIV_MAX));
            self.ndiv[((offset - CMU_CLKS) / 8) as usize] = ndiv;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn counters_show_the_programmed_frequency() {
        let mut c = Ccplex::default();
        // core 4 is cluster 1's core 2: id 6
        assert_eq!(id(0x10200), 6);
        c.write(CMU_CLKS + 8 * 6, 57, 0); // 57 × 12.8 MHz = 729.6 MHz
        assert_eq!(c.read(CMU_CLKS + 8 * 6, 0), 57);
        let at = |c: &mut Ccplex, now| {
            let reg = ACTMON + CLUSTER_STRIDE + 2 * 8;
            (c.read(reg, now), c.read(reg + 4, now))
        };
        let (core0, ref0) = at(&mut c, 1_000_000);
        let (core1, ref1) = at(&mut c, 1_100_000);
        assert_eq!(ref1 - ref0, 40_800); // 100 µs of 408 MHz
        // the driver's formula: kHz = Δcore × 408 / Δref × 1000
        assert_eq!(u64::from(core1 - core0) * 408 / u64::from(ref1 - ref0) * 1000, 729_000);
        assert_eq!(c.khz(6), 729_600);
        assert_eq!(Ccplex::max_khz(), 1_510_400);
    }
}
