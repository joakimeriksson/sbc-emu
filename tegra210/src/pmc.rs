//! Power management controller (`nvidia,tegra210-pmc`): power partitions and I/O pads.
//!
//! Linux turns a partition on or off by writing its number with START to PWRGATE_TOGGLE and
//! polling PWRGATE_STATUS; an I/O pad goes into or out of deep power-down by a request to
//! IO_DPD(2)_REQ and polling IO_DPD(2)_STATUS. Both take effect at once here. Everything else
//! (scratch registers, the strapping register, wake configuration) reads back.

pub const SIZE: u64 = 0x1000;

const PWRGATE_TOGGLE: usize = 0x30;
const PWRGATE_TOGGLE_START: u32 = 1 << 8;
const PWRGATE_STATUS: usize = 0x38;
const IO_DPD_REQ: usize = 0x1b8;
const IO_DPD_STATUS: usize = 0x1bc;
const IO_DPD2_REQ: usize = 0x1c0;
const IO_DPD2_STATUS: usize = 0x1c4;

/// The partitions powered at hand-over: the CPU cluster (CRAIL 0, CPU0 14, C0NC 15) and
/// the other cores' partitions (CPU1-3: 9-11), as the bootloader leaves them.
const INITIAL_STATUS: u32 = 1 << 0 | 1 << 14 | 1 << 15 | 1 << 9 | 1 << 10 | 1 << 11;

pub struct Pmc {
    regs: Vec<u32>,
}

impl Default for Pmc {
    fn default() -> Self {
        let mut regs = vec![0; SIZE as usize / 4];
        regs[PWRGATE_STATUS / 4] = INITIAL_STATUS;
        Self { regs }
    }
}

impl Pmc {
    pub fn read(&self, offset: u64) -> u32 {
        let o = offset as usize & (SIZE as usize - 4);
        match o {
            // the toggle is done at once: START reads back clear
            PWRGATE_TOGGLE => self.regs[o / 4] & !PWRGATE_TOGGLE_START,
            _ => self.regs[o / 4],
        }
    }

    pub fn write(&mut self, offset: u64, value: u32) {
        let o = offset as usize & (SIZE as usize - 4);
        match o {
            PWRGATE_TOGGLE => {
                if value & PWRGATE_TOGGLE_START != 0 {
                    self.regs[PWRGATE_STATUS / 4] ^= 1 << (value & 0x1f);
                }
                self.regs[o / 4] = value & !PWRGATE_TOGGLE_START;
            }
            PWRGATE_STATUS => {} // read-only
            IO_DPD_REQ | IO_DPD2_REQ => {
                let status = if o == IO_DPD_REQ { IO_DPD_STATUS } else { IO_DPD2_STATUS };
                let pads = value & 0x3fff_ffff;
                match value >> 30 {
                    1 => self.regs[status / 4] &= !pads, // DPD off
                    2 => self.regs[status / 4] |= pads,  // DPD on
                    _ => {}
                }
                self.regs[o / 4] = value;
            }
            _ => self.regs[o / 4] = value,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn powergate_toggle_and_io_pads() {
        let mut p = Pmc::default();
        assert_eq!(p.read(0x38) & 1 << 1, 0); // 3D off
        p.write(0x30, PWRGATE_TOGGLE_START | 1);
        assert_ne!(p.read(0x38) & 1 << 1, 0);
        assert_eq!(p.read(0x30) & PWRGATE_TOGGLE_START, 0);
        p.write(0x1b8, 2 << 30 | 1 << 3);
        assert_ne!(p.read(0x1bc) & 1 << 3, 0);
        p.write(0x1b8, 1 << 30 | 1 << 3);
        assert_eq!(p.read(0x1bc) & 1 << 3, 0);
    }
}
