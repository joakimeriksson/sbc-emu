//! The general register files (GRFs) Linux reaches as syscons: plain registers with
//! Rockchip's write-mask convention (the upper 16 bits of a write select which of the lower 16
//! it changes). The SoC answers the few status registers whose bits come from elsewhere.

pub const SYS_GRF: u64 = 0xfd58_c000;
pub const VOP_GRF: u64 = 0xfd5a_4000;
pub const VO1_GRF: u64 = 0xfd5a_8000;
pub const HDPTXPHY0_GRF: u64 = 0xfd5e_0000;
/// The PMU's registers, a syscon the VOP2 driver looks up (and does not use).
pub const PMU: u64 = 0xfd8d_8000;
/// The GRFs: (base, size).
pub const BLOCKS: [(u64, u64); 5] = [(SYS_GRF, 0x1000), (VOP_GRF, 0x2000), (VO1_GRF, 0x4000), (HDPTXPHY0_GRF, 0x100), (PMU, 0x400)];
/// SYS_GRF SOC_STATUS1: HDMI0's hot-plug level (bit 16).
pub const SYS_GRF_SOC_STATUS1: u64 = 0x384;
pub const HDMI0_LEVEL: u32 = 1 << 16;
/// HDPTXPHY0_GRF CON0 (I_PLL_EN: bit 7) and STATUS (PLL_LOCK_DONE, PHY_CLK_RDY, PHY_RDY:
/// bits 3..1, which the PHY driver polls after enabling the PLL and the lanes).
pub const HDPTX_CON0: u64 = 0x00;
pub const HDPTX_STATUS: u64 = 0x80;
pub const HDPTX_PLL_EN: u32 = 1 << 7;
pub const HDPTX_READY: u32 = 0b1110;
/// The HDMI0 PHY (Samsung HDPTX) registers: kept as written, Linux decides nothing from them.
pub const HDPTXPHY0: u64 = 0xfed6_0000;
pub const HDPTXPHY_SIZE: u64 = 0x2000;

/// A GRF block of `size` bytes.
pub struct Grf {
    regs: Vec<u32>,
}

impl Grf {
    pub fn new(size: u64) -> Self {
        Self { regs: vec![0; (size / 4) as usize] }
    }

    pub fn read(&self, off: u64) -> u32 {
        self.regs.get((off / 4) as usize).copied().unwrap_or(0)
    }

    pub fn write(&mut self, off: u64, v: u32) {
        if let Some(r) = self.regs.get_mut((off / 4) as usize) {
            let mask = v >> 16;
            *r = (*r & !mask) | (v & mask);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn writes_change_only_the_masked_bits() {
        let mut g = Grf::new(0x100);
        g.write(8, 0xffff_00f0);
        g.write(8, 0x0003_0001);
        assert_eq!(g.read(8), 0x00f1);
        g.write(8, 0x0000_ffff);
        assert_eq!(g.read(8), 0x00f1, "no mask, no change");
    }
}
