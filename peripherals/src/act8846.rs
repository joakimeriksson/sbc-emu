//! Active-semi ACT8846 PMIC on I²C (`active-semi,act8846`, Linux `act8865-regulator`), found on
//! RK3288 boards: twelve regulators (REG1-4 step-down, REG5-12 LDOs), each a
//! voltage selector and an enable bit that read back what Linux writes, and the power-off
//! bits of the global off register (`system-power-controller`).
use crate::i2c::{I2cDevice, RegisterPointer};

/// I²C address (`act8846@5a`).
pub const ADDRESS: u8 = 0x5a;

/// Regulator on (bit 7 of each CTRL register).
const ENA: u8 = 0x80;
const GLB_OFF_CTRL: usize = 0xc3;
const OFF_SYSMASK: u8 = 0x18;

/// (VSET, CTRL) of REG1-REG12; REG2-4 also have a second selector at VSET + 1.
const REGULATORS: [(usize, usize); 12] = [
    (0x10, 0x12),
    (0x20, 0x22),
    (0x30, 0x32),
    (0x40, 0x42),
    (0x50, 0x51),
    (0x58, 0x59),
    (0x60, 0x61),
    (0x68, 0x69),
    (0x70, 0x71),
    (0x80, 0x81),
    (0x90, 0x91),
    (0xa0, 0xa1),
];

/// An RK3288 board's rails as its boot loader leaves them, in microvolts (REG1-12: VCC_DDR,
/// VCC_IO, VDD_LOG, VCC_20, VCCIO_SD, VDD10_LCD, VCCA_CODEC, VCCA_TP, VCCIO_PMU, VDD_10,
/// VCC_18, VCC18_LCD), each inside its device-tree constraints.
const BOARD_MICROVOLTS: [u32; 12] = [
    1_200_000, 3_300_000, 1_100_000, 2_000_000, 3_300_000, 1_000_000, 3_300_000, 3_300_000, 3_300_000, 1_000_000,
    1_800_000, 1_800_000,
];

/// The selector for `microvolts` (Linux's `act8865_voltage_ranges`: 0.6 V + 25 mV steps,
/// 1.2 V + 50 mV, 2.4 V + 100 mV).
pub fn selector(microvolts: u32) -> u8 {
    let s = match microvolts {
        v if v < 1_200_000 => v.saturating_sub(600_000) / 25_000,
        v if v < 2_400_000 => 24 + (v - 1_200_000) / 50_000,
        v => 48 + (v - 2_400_000) / 100_000,
    };
    s.min(63) as u8
}

/// The voltage of a selector, in microvolts.
pub fn microvolts(selector: u8) -> u32 {
    match (selector & 0x3f) as u32 {
        s @ 0..=23 => 600_000 + s * 25_000,
        s @ 24..=47 => 1_200_000 + (s - 24) * 50_000,
        s => 2_400_000 + (s - 48) * 100_000,
    }
}

pub struct Act8846 {
    regs: [u8; 256],
    pointer: RegisterPointer,
    /// Linux wrote the power-off bits: the board turns off.
    pub power_off: bool,
}

impl Default for Act8846 {
    fn default() -> Self {
        let mut regs = [0u8; 256];
        for (&(vset, ctrl), &uv) in REGULATORS.iter().zip(&BOARD_MICROVOLTS) {
            regs[vset] = selector(uv);
            if (0x20..0x50).contains(&vset) {
                regs[vset + 1] = selector(uv);
            }
            regs[ctrl] = ENA;
        }
        Self { regs, pointer: RegisterPointer::default(), power_off: false }
    }
}

impl Act8846 {
    /// Output of REG`n` (1-12) in microvolts, or None while it is off.
    pub fn output(&self, n: usize) -> Option<u32> {
        let (vset, ctrl) = REGULATORS[n - 1];
        (self.regs[ctrl] & ENA != 0).then(|| microvolts(self.regs[vset]))
    }
}

impl I2cDevice for Act8846 {
    fn start(&mut self, read: bool) -> bool {
        self.pointer.start(read);
        true
    }

    fn write(&mut self, byte: u8) -> bool {
        if let Some(reg) = self.pointer.written(byte) {
            self.regs[reg as usize] = byte;
            if reg as usize == GLB_OFF_CTRL && byte & OFF_SYSMASK == OFF_SYSMASK {
                self.power_off = true;
            }
        }
        true
    }

    fn read(&mut self) -> u8 {
        self.regs[self.pointer.next() as usize]
    }

    fn as_any_mut(&mut self) -> Option<&mut dyn std::any::Any> {
        Some(self)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn regulators_start_at_the_board_voltages_and_read_back_writes() {
        let mut pmic = Act8846::default();
        assert_eq!(pmic.output(2), Some(3_300_000)); // VCC_IO
        assert_eq!(pmic.output(4), Some(2_000_000)); // VCC_20
        assert_eq!(pmic.output(11), Some(1_800_000)); // VCC_18
        // VCCIO_SD (REG5) to 1.8 V, then off.
        pmic.start(false);
        pmic.write(0x50);
        pmic.write(selector(1_800_000));
        pmic.write(0);
        assert_eq!(pmic.output(5), None);
        pmic.start(false);
        pmic.write(0x50);
        pmic.start(true);
        assert_eq!(microvolts(pmic.read()), 1_800_000);
        assert!(!pmic.power_off);
        pmic.start(false);
        pmic.write(GLB_OFF_CTRL as u8);
        pmic.write(OFF_SYSMASK);
        assert!(pmic.power_off);
    }

    #[test]
    fn selectors_follow_linuxs_voltage_ranges() {
        for (uv, sel) in [(600_000, 0), (1_175_000, 23), (1_200_000, 24), (2_350_000, 47), (2_400_000, 48), (3_300_000, 57)] {
            assert_eq!(selector(uv), sel);
            assert_eq!(microvolts(sel), uv);
        }
    }
}
