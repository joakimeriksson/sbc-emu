//! A serial output resource (`nvidia,tegra210-sor1`, HDMI on the Nano; `sor0`, DisplayPort): a
//! register file whose power and lane sequencers finish at once, and whose attach and mode status
//! follow the super state. Audio and infoframe registers read back.
pub const BASE: [u64; 2] = [0x5454_0000, 0x5458_0000];
pub const SIZE: u64 = 0x1000;
pub const RESETS: [usize; 2] = [182, 183];

const SUPER_STATE1: usize = 0x02;
const PWR: usize = 0x15;
const TEST: usize = 0x16;
const LANE_SEQ_CTL: usize = 0x21;
const PWM_CTL: usize = 0x33;
const TRIGGER: u32 = 1 << 31;

pub struct Sor {
    regs: Vec<u32>,
}

impl Default for Sor {
    fn default() -> Self {
        Self { regs: vec![0; SIZE as usize / 4] }
    }
}

impl Sor {
    pub fn read(&self, offset: u64) -> u32 {
        let r = offset as usize / 4 % self.regs.len();
        let v = self.regs[r];
        let state = self.regs[SUPER_STATE1];
        match r {
            PWR => v & !(1 << 28) | if state & 1 << 2 == 0 { 1 << 28 } else { 0 }, // in safe mode
            TEST => v & !(1 << 10 | 3 << 8) | if state & 1 << 3 != 0 { 1 << 10 } else { 0 } | (state & 3) << 8,
            _ => v,
        }
    }

    pub fn write(&mut self, offset: u64, v: u32) {
        let r = offset as usize / 4 % self.regs.len();
        self.regs[r] = match r {
            PWR | LANE_SEQ_CTL | PWM_CTL => v & !(TRIGGER | if r == LANE_SEQ_CTL { 1 << 28 } else { 0 }),
            _ => v,
        };
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sequencers_finish_and_the_status_follows() {
        let mut s = Sor::default();
        s.write(4 * PWR as u64, TRIGGER | 1);
        assert_eq!(s.read(4 * PWR as u64), 1 | 1 << 28);
        s.write(4 * SUPER_STATE1 as u64, 1 << 3 | 1 << 2 | 2);
        assert_eq!(s.read(4 * PWR as u64), 1);
        assert_eq!(s.read(4 * TEST as u64), 1 << 10 | 2 << 8);
        s.write(4 * LANE_SEQ_CTL as u64, TRIGGER | 5);
        assert_eq!(s.read(4 * LANE_SEQ_CTL as u64), 5);
    }
}
