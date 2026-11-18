//! RK3288 clock and reset unit (CRU, 0xff76_0000, `rockchip,rk3288-cru`) and the two syscons
//! whose status the clock and power code polls: the GRF PLL lock bits and the PMU power
//! domains. Register layouts follow mainline `drivers/clk/rockchip/clk-rk3288.c`,
//! `clk-pll.c` (rk3066-type PLL) and `drivers/pmdomain/rockchip/pm-domains.c`.
//!
//! Rockchip "hiword" registers take a write-enable mask in bits 31:16: only the low bits
//! whose mask bit is set change, and the upper half reads as zero.

/// Apply a hiword-masked write to a 16-bit register value.
pub fn hiword_write(old: u32, value: u32) -> u32 {
    let mask = value >> 16;
    (old & !mask | value & mask) & 0xffff
}

pub const CRU_SIZE: u64 = 0x1000;
/// `RK3288_PLL_CON(4 * pll)`: APLL, DPLL, CPLL, GPLL, NPLL, four registers each.
pub const PLL_NAMES: [&str; 5] = ["apll", "dpll", "cpll", "gpll", "npll"];
pub const MODE_CON: u64 = 0x50;
/// MODE_CON field shift of each PLL (2 bits: 0 slow/24 MHz, 1 normal, 2 deep slow).
const MODE_SHIFT: [u32; 5] = [0, 4, 8, 12, 14];
pub const fn clksel_con(n: u64) -> u64 {
    0x60 + 4 * n
}
pub const GLB_SRST_FST: u64 = 0x1b0;
pub const GLB_SRST_SND: u64 = 0x1b4;
/// `RK3288_SOFTRST_CON(0)` (hiword): bits 0-3 hold cores 0-3 in reset (`SRST_CORE0..3`,
/// the `resets` of the cpu nodes; set = asserted).
pub const SOFTRST_CON0: u64 = 0x1b8;
/// Fractional dividers (`COMPOSITE_FRACMUX`): plain 32-bit numerator << 16 | denominator.
const FRAC_CLKSEL: [u64; 8] = [7, 8, 9, 17, 18, 19, 20, 41];
const OSC_HZ: u64 = 24_000_000;

pub struct Cru {
    regs: Vec<u32>,
    /// The guest wrote a global soft-reset magic (0xfdb9 to FST or 0xeca8 to SND).
    pub reset_requested: bool,
}

impl Default for Cru {
    fn default() -> Self {
        Self::new()
    }
}

impl Cru {
    /// The state U-Boot leaves on a Tinker Board: all PLLs locked in normal mode with
    /// APLL 1008 MHz, DPLL 532 MHz, CPLL 400 MHz, GPLL 594 MHz and NPLL 500 MHz; UART2 on
    /// the 24 MHz oscillator.
    pub fn new() -> Self {
        let mut regs = vec![0; (CRU_SIZE / 4) as usize];
        // (nr, nf, no): rate = 24 MHz * nf / (nr * no).
        let plls = [(1, 84, 2), (1, 133, 6), (1, 50, 3), (1, 99, 4), (1, 125, 6)];
        for (index, (nr, nf, no)) in plls.into_iter().enumerate() {
            let base = index * 4;
            regs[base] = (nr - 1) << 8 | (no - 1);
            regs[base + 1] = nf - 1;
            regs[base + 2] = nf / 2 - 1; // bandwidth adjust, nb = nf / 2
        }
        regs[(MODE_CON / 4) as usize] =
            MODE_SHIFT.iter().fold(0, |mode, shift| mode | 1 << shift);
        for n in FRAC_CLKSEL {
            regs[(clksel_con(n) / 4) as usize] = 0x0bb8_ea60; // 3000 / 60000
        }
        // CLKSEL_CON15[9:8]: uart2 clock from xin24m.
        regs[(clksel_con(15) / 4) as usize] = 2 << 8;
        Self { regs, reset_requested: false }
    }

    fn is_plain(offset: u64) -> bool {
        offset == GLB_SRST_FST
            || offset == GLB_SRST_SND
            || FRAC_CLKSEL.iter().any(|&n| clksel_con(n) == offset)
    }

    pub fn read(&self, offset: u64) -> u32 {
        self.regs[(offset / 4) as usize]
    }

    pub fn write(&mut self, offset: u64, value: u32) {
        let index = (offset / 4) as usize;
        match offset {
            GLB_SRST_FST if value == 0xfdb9 => self.reset_requested = true,
            GLB_SRST_SND if value == 0xeca8 => self.reset_requested = true,
            _ => {}
        }
        self.regs[index] = if Self::is_plain(offset) {
            value
        } else {
            hiword_write(self.regs[index], value)
        };
    }

    /// Core `core` is held in soft reset (SOFTRST_CON0).
    pub fn core_in_reset(&self, core: usize) -> bool {
        self.regs[(SOFTRST_CON0 / 4) as usize] & (1 << core) != 0
    }

    /// Output rate of a PLL in Hz as Linux computes it (mode mux included).
    pub fn pll_rate(&self, pll: usize) -> u64 {
        let mode = self.regs[(MODE_CON / 4) as usize] >> MODE_SHIFT[pll] & 3;
        match mode {
            0 => return OSC_HZ,
            2 => return 32_768,
            _ => {}
        }
        let con = &self.regs[pll * 4..pll * 4 + 4];
        if con[3] & 1 != 0 {
            return OSC_HZ; // bypass
        }
        let nr = u64::from((con[0] >> 8 & 0x3f) + 1);
        let no = u64::from((con[0] & 0xf) + 1);
        let nf = u64::from((con[1] & 0x1fff) + 1);
        OSC_HZ * nf / (nr * no)
    }

    /// I2S0's MCLK as Linux computes it (`clk-rk3288.c`): `i2s_src` is CPLL or GPLL
    /// (CLKSEL_CON4[15]) over CLKSEL_CON4[6:0] + 1; `i2s_frac` scales it by CLKSEL_CON8
    /// (numerator << 16 | denominator); `i2s_pre` picks one (CLKSEL_CON4[9:8]: src, frac,
    /// the external clock, 12 MHz). 0 for the external clock (not modelled).
    pub fn i2s_mclk(&self) -> u64 {
        let sel4 = self.regs[(clksel_con(4) / 4) as usize];
        let pll = if sel4 >> 15 & 1 == 0 { 2 } else { 3 };
        let src = self.pll_rate(pll) / (u64::from(sel4 & 0x7f) + 1);
        match sel4 >> 8 & 3 {
            0 => src,
            1 => {
                let frac = self.regs[(clksel_con(8) / 4) as usize];
                let (num, den) = (u64::from(frac >> 16), u64::from(frac & 0xffff));
                (src * num).checked_div(den).unwrap_or(0)
            }
            2 => 0,
            _ => OSC_HZ / 2,
        }
    }

    /// PLLs that are powered (not in power-down) report lock.
    pub fn pll_locked(&self, pll: usize) -> bool {
        self.regs[pll * 4 + 3] & (1 << 1 | 1 << 5) == 0
    }
}

pub const GRF_SIZE: u64 = 0x1000;
/// `RK3288_GRF_SOC_STATUS1`: PLL lock bits APLL 6, DPLL 5, CPLL 7, GPLL 8, NPLL 9.
pub const GRF_SOC_STATUS1: u64 = 0x284;
const GRF_LOCK_SHIFT: [u32; 5] = [6, 5, 7, 8, 9];
/// GRF_SOC_STATUS0..21 are read-only.
const GRF_STATUS: (u64, u64) = (0x280, 0x2d8);

/// General register files: pin muxing, IO voltage, SoC control (hiword registers) and
/// read-only status. Only the PLL lock status is modelled; other status reads as zero.
pub struct Grf {
    regs: Vec<u32>,
}

impl Default for Grf {
    fn default() -> Self {
        Self { regs: vec![0; (GRF_SIZE / 4) as usize] }
    }
}

impl Grf {
    pub fn read(&self, offset: u64, cru: &Cru) -> u32 {
        if offset == GRF_SOC_STATUS1 {
            return (0..5)
                .filter(|&pll| cru.pll_locked(pll))
                .fold(0, |status, pll| status | 1 << GRF_LOCK_SHIFT[pll]);
        }
        self.regs[(offset / 4) as usize]
    }

    pub fn write(&mut self, offset: u64, value: u32) {
        if (GRF_STATUS.0..GRF_STATUS.1).contains(&offset) {
            return;
        }
        let index = (offset / 4) as usize;
        self.regs[index] = hiword_write(self.regs[index], value);
    }
}

pub const PMU_SIZE: u64 = 0x100;
pub const PMU_PWRDN_CON: u64 = 0x08;
pub const PMU_PWRDN_ST: u64 = 0x0c;
pub const PMU_IDLE_REQ: u64 = 0x10;
pub const PMU_IDLE_ST: u64 = 0x14;

/// PMU power domains (`rockchip,rk3288-pmu`). Power and idle transitions complete at once:
/// PWRDN_ST follows PWRDN_CON (bit 14, HEVC, reports in bit 10) and IDLE_ST reports each
/// IDLE_REQ bit as both idle (same bit) and acknowledged (bit + 16). Plain 32-bit registers.
/// PWRDN_CON bits 0-3 are the A12 cores (`platsmp.c` powers cores 1-3 through them).
pub struct Pmu {
    regs: Vec<u32>,
}

impl Default for Pmu {
    fn default() -> Self {
        let mut regs = vec![0; (PMU_SIZE / 4) as usize];
        // Cores 1-3 powered down until Linux starts them; every other domain on.
        regs[(PMU_PWRDN_CON / 4) as usize] = 0b1110;
        Self { regs }
    }
}

impl Pmu {
    pub fn read(&self, offset: u64) -> u32 {
        let reg = |offset: u64| self.regs[(offset / 4) as usize];
        match offset {
            PMU_PWRDN_ST => {
                let con = reg(PMU_PWRDN_CON);
                con & !(1 << 14 | 1 << 10) | (con >> 14 & 1) << 10
            }
            PMU_IDLE_ST => {
                let req = reg(PMU_IDLE_REQ) & 0xffff;
                req | req << 16
            }
            _ => reg(offset),
        }
    }

    /// The power domain of core `core` is on.
    pub fn core_powered(&self, core: usize) -> bool {
        self.regs[(PMU_PWRDN_CON / 4) as usize] & (1 << core) == 0
    }

    pub fn write(&mut self, offset: u64, value: u32) {
        if offset != PMU_PWRDN_ST && offset != PMU_IDLE_ST {
            self.regs[(offset / 4) as usize] = value;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hiword_writes_change_only_enabled_bits() {
        assert_eq!(hiword_write(0x00ff, 0x0f00_0a05), 0x0aff);
        assert_eq!(hiword_write(0x00ff, 0x00f0_0005), 0x000f);
        assert_eq!(hiword_write(0x1234, 0x0000_ffff), 0x1234);
        assert_eq!(hiword_write(0x1234, 0xffff_0000), 0);
    }

    #[test]
    fn plls_report_u_boot_rates_and_lock() {
        let cru = Cru::new();
        let mhz: Vec<u64> = (0..5).map(|pll| cru.pll_rate(pll) / 1_000_000).collect();
        assert_eq!(mhz, [1008, 532, 400, 594, 500]);
        let grf = Grf::default();
        assert_eq!(grf.read(GRF_SOC_STATUS1, &cru), 0x3e0);
    }

    #[test]
    fn linux_pll_reprogramming_sequence_takes_effect() {
        // clk-pll.c rk3066 set_params for GPLL = 297 MHz (nr 2, nf 99, no 4) via slow mode.
        let mut cru = Cru::new();
        let gpll = 0x30;
        cru.write(MODE_CON, 0x3000_0000); // GPLL mode: slow
        assert_eq!(cru.pll_rate(3), 24_000_000);
        cru.write(gpll + 12, 0x0020_0020); // reset
        assert!(!cru.pll_locked(3));
        cru.write(gpll, 0x3f00_0100 | 0x000f_0003);
        cru.write(gpll + 4, 0x1fff_0062);
        cru.write(gpll + 12, 0x0020_0000); // out of reset
        assert!(cru.pll_locked(3));
        cru.write(MODE_CON, 0x3000_1000); // normal
        assert_eq!(cru.pll_rate(3), 297_000_000);
        assert_eq!(cru.read(gpll) >> 16, 0, "hiword registers read back without the mask");
    }

    #[test]
    fn fractional_dividers_are_plain_and_global_reset_is_reported() {
        let mut cru = Cru::new();
        cru.write(clksel_con(19), 0x0001_0002);
        assert_eq!(cru.read(clksel_con(19)), 0x0001_0002);
        assert!(!cru.reset_requested);
        cru.write(GLB_SRST_FST, 0xfdb9);
        assert!(cru.reset_requested);
    }

    #[test]
    fn pmu_domains_complete_power_and_idle_requests() {
        let mut pmu = Pmu::default();
        assert_eq!(pmu.read(PMU_PWRDN_ST) & 0xf, 0b1110);
        pmu.write(PMU_PWRDN_CON, 1 << 9 | 1 << 14); // GPU and HEVC off
        assert_eq!(pmu.read(PMU_PWRDN_ST), 1 << 9 | 1 << 10);
        pmu.write(PMU_IDLE_REQ, 1 << 2); // GPU idle request
        assert_eq!(pmu.read(PMU_IDLE_ST), 1 << 2 | 1 << 18);
        pmu.write(PMU_IDLE_ST, 0);
        assert_eq!(pmu.read(PMU_IDLE_ST), 1 << 2 | 1 << 18);
    }
}
