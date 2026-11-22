//! Clock and reset controller (CAR, `nvidia,tegra210-car`) as the bootloader leaves it.
//!
//! Linux reads the oscillator frequency and the clock-M divider here, programs the PLLs and
//! the peripheral clock sources itself, and waits (with a timeout) for each PLL's lock bit.
//! The model is a register file with: the oscillator at 38.4 MHz and clock M at 19.2 MHz
//! (the Jetson Nano's), lock bits that read as set while their PLL is enabled, and the
//! set/clear registers that change the clock-enable and reset registers.

pub const SIZE: u64 = 0x1000;

const OSC_CTRL: usize = 0x50;
/// OSC_FREQ = 5 (38.4 MHz), PLL_REF_DIV = 0: the PLLs' reference is the oscillator itself
/// (PLLE's frequency table has 38.4 MHz, not 19.2)
const OSC_CTRL_38M4: u32 = 5 << 28;
/// CLK_M_DIVISOR (bits 3:2) = 1: clock M is the oscillator / 2
const SPARE_REG0: usize = 0x55c;
const PLL_ENABLE: u32 = 1 << 30;
const SCLK_BURST_POLICY: usize = 0x28;
const CCLKG_BURST_POLICY: usize = 0x368;
const CCLKLP_BURST_POLICY: usize = 0x370;
const ACLK_BURST_POLICY: usize = 0x6e0;
const PLL_BASE_LOCK: u32 = 1 << 27;
const PLLCX_BASE_LOCK: u32 = 1 << 26;

/// (base register, lock bit) for the PLLs whose lock bit is in the base register.
const BASE_LOCKS: [(usize, u32); 13] = [
    (0x80, PLL_BASE_LOCK),   // PLLC
    (0x90, PLL_BASE_LOCK),   // PLLM
    (0xa0, PLL_BASE_LOCK),   // PLLP
    (0xb0, PLL_BASE_LOCK),   // PLLA
    (0xc0, PLL_BASE_LOCK),   // PLLU
    (0xd0, PLL_BASE_LOCK),   // PLLD
    (0xe0, PLL_BASE_LOCK),   // PLLX
    (0x4b8, PLL_BASE_LOCK),  // PLLD2
    (0x4e8, PLLCX_BASE_LOCK), // PLLC2
    (0x4fc, PLLCX_BASE_LOCK), // PLLC3
    (0x590, PLL_BASE_LOCK),  // PLLDP
    (0x5a4, PLL_BASE_LOCK),  // PLLC4
    (0x6a4, PLLCX_BASE_LOCK), // PLLA1
];
const PLLMB_BASE: usize = 0x5e8;
/// PLLE: lock in MISC0 bit 11, enabled by BASE bit 31
const PLLE_BASE: usize = 0xe8;
const PLLE_MISC0: usize = 0xec;
/// PLLRE: lock in MISC0 bit 27
const PLLRE_BASE: usize = 0x4c4;
const PLLRE_MISC0: usize = 0x4c8;
/// UTMI PLL: lock in HW_PWRDN_CFG0 bit 31
const UTMIPLL_HW_PWRDN_CFG0: usize = 0x52c;

/// (state register, set register, clear register) for the clock-enable and reset banks.
const SET_CLEAR: [(usize, usize, usize); 14] = [
    (0x010, 0x320, 0x324), // CLK_OUT_ENB_L
    (0x014, 0x328, 0x32c), // CLK_OUT_ENB_H
    (0x018, 0x330, 0x334), // CLK_OUT_ENB_U
    (0x360, 0x440, 0x444), // CLK_OUT_ENB_V
    (0x364, 0x448, 0x44c), // CLK_OUT_ENB_W
    (0x280, 0x284, 0x288), // CLK_OUT_ENB_X
    (0x298, 0x29c, 0x2a0), // CLK_OUT_ENB_Y
    (0x004, 0x300, 0x304), // RST_DEVICES_L
    (0x008, 0x308, 0x30c), // RST_DEVICES_H
    (0x00c, 0x310, 0x314), // RST_DEVICES_U
    (0x358, 0x430, 0x434), // RST_DEVICES_V
    (0x35c, 0x438, 0x43c), // RST_DEVICES_W
    (0x28c, 0x290, 0x294), // RST_DEVICES_X
    (0x2a4, 0x2a8, 0x2ac), // RST_DEVICES_Y
];

pub struct Car {
    regs: Vec<u32>,
}

impl Default for Car {
    fn default() -> Self {
        let mut regs = vec![0; SIZE as usize / 4];
        regs[OSC_CTRL / 4] = OSC_CTRL_38M4;
        regs[SPARE_REG0 / 4] = 1 << 2;
        // PLLP runs (408 MHz) and so does PLLX for the CPU, as the bootloader leaves them
        regs[0xa0 / 4] = PLL_ENABLE;
        regs[0xe0 / 4] = PLL_ENABLE;
        // the super-clock muxes in the RUN state (bit 29), source in bits 7:4: the CPU
        // clusters on PLLX (8), the system clock on PLLP (3)
        regs[CCLKG_BURST_POLICY / 4] = 1 << 29 | 8 << 4;
        regs[CCLKLP_BURST_POLICY / 4] = 1 << 29 | 8 << 4;
        regs[SCLK_BURST_POLICY / 4] = 1 << 29 | 3 << 4;
        // the audio DSP's clock (aclk) has the same format; RUN on its first source
        regs[ACLK_BURST_POLICY / 4] = 1 << 29;
        Self { regs }
    }
}

impl Car {
    pub fn read(&self, offset: u64) -> u32 {
        let o = offset as usize & (SIZE as usize - 4);
        let v = self.regs[o / 4];
        let enabled = |base: usize| self.regs[base / 4] & PLL_ENABLE != 0;
        // the lock bit follows the enable, whatever a driver wrote back
        let locked = |on: bool, bit: u32| if on { v | bit } else { v & !bit };
        match o {
            _ if BASE_LOCKS.iter().any(|&(b, _)| b == o) => {
                let lock = BASE_LOCKS.iter().find(|&&(b, _)| b == o).unwrap().1;
                locked(v & PLL_ENABLE != 0, lock)
            }
            PLLMB_BASE => locked(v & PLL_ENABLE != 0, PLL_BASE_LOCK),
            PLLE_MISC0 => locked(self.regs[PLLE_BASE / 4] & 1 << 31 != 0, 1 << 11),
            PLLRE_MISC0 => locked(enabled(PLLRE_BASE), 1 << 27),
            UTMIPLL_HW_PWRDN_CFG0 => v | 1 << 31,
            _ => v,
        }
    }

    /// Whether the peripheral with this clock/reset number (the device tree's) is in reset.
    pub fn in_reset(&self, id: usize) -> bool {
        const BANKS: [usize; 7] = [0x4, 0x8, 0xc, 0x358, 0x35c, 0x28c, 0x2a4]; // L H U V W X Y
        BANKS.get(id / 32).is_some_and(|b| self.regs[b / 4] & 1 << (id % 32) != 0)
    }

    pub fn write(&mut self, offset: u64, value: u32) {
        let o = offset as usize & (SIZE as usize - 4);
        if let Some(&(state, set, _)) = SET_CLEAR.iter().find(|&&(_, s, c)| s == o || c == o) {
            if o == set { self.regs[state / 4] |= value } else { self.regs[state / 4] &= !value }
            return;
        }
        self.regs[o / 4] = value;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn set_clear_and_lock() {
        let mut c = Car::default();
        c.write(0x320, 1 << 6); // CLK_ENB_L_SET: UARTA
        assert_eq!(c.read(0x10) & 1 << 6, 1 << 6);
        c.write(0x324, 1 << 6);
        assert_eq!(c.read(0x10) & 1 << 6, 0);
        c.write(0x80, PLL_ENABLE);
        assert_ne!(c.read(0x80) & PLL_BASE_LOCK, 0);
        c.write(0x80, 0);
        assert_eq!(c.read(0x80) & PLL_BASE_LOCK, 0);
        assert_eq!(c.read(0x50) >> 28, 5);
        // PLLE, PLLRE, PLLMB: a lock bit written back does not outlive the PLL
        for (base, on, reg, bit) in [(0xe8, 1 << 31, 0xec, 11), (0x4c4, PLL_ENABLE, 0x4c8, 27), (0x5e8, PLL_ENABLE, 0x5e8, 27)] {
            c.write(base, on);
            let v = c.read(reg);
            assert_ne!(v & 1 << bit, 0, "{base:#x}");
            c.write(reg, v);
            c.write(base, if reg == base { v & !on } else { 0 });
            assert_eq!(c.read(reg) & 1 << bit, 0, "{base:#x}");
        }
    }
}
