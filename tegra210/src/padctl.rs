//! The XUSB pad controller (`nvidia,tegra210-xusb-padctl`): the PHYs of PCIe, SATA and USB. A
//! register file whose two UPHY PLLs (PCIe/USB 3 at 0x360, SATA at 0x860) calibrate and lock as
//! soon as they are told to.
pub const SIZE: u64 = 0x1000;

const PLLS: [usize; 2] = [0x360, 0x860];

pub struct PadCtl {
    regs: Vec<u32>,
}

impl Default for PadCtl {
    fn default() -> Self {
        Self { regs: vec![0; SIZE as usize / 4] }
    }
}

impl PadCtl {
    pub fn read(&self, offset: u64) -> u32 {
        let o = offset as usize & (SIZE as usize - 4);
        let v = self.regs[o / 4];
        let status = |enable: u32, done: u32| if v & enable != 0 { v | done } else { v & !done };
        match PLLS.iter().find(|&&p| (p..p + 0x20).contains(&o)).map(|p| o - p) {
            Some(0x00) => status(1 << 3, 1 << 15), // CTL1: enabled, locked
            Some(0x04) => status(1 << 0, 1 << 1),  // CTL2: calibration
            Some(0x1c) => status(1 << 12, 1 << 31), // CTL8: resistor calibration
            _ => v,
        }
    }

    pub fn write(&mut self, offset: u64, value: u32) {
        self.regs[(offset as usize & (SIZE as usize - 4)) / 4] = value;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_plls_calibrate_and_lock() {
        let mut p = PadCtl::default();
        for base in [0x360u64, 0x860] {
            for (o, enable, done) in [(0, 8, 1 << 15), (4, 1, 2), (0x1c, 1 << 12, 1 << 31)] {
                assert_eq!(p.read(base + o), 0);
                p.write(base + o, enable);
                assert_eq!(p.read(base + o), enable | done);
                p.write(base + o, done);
                assert_eq!(p.read(base + o), 0, "done follows the enable");
            }
        }
    }
}
