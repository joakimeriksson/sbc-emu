//! Silergy SYR827/SYR828 step-down regulators on I²C (`silergy,syr827`/`syr828`, Linux
//! `fan53555`): the panel board's vdd_cpu (0x40) and vdd_gpu (0x41). Two voltage selectors
//! (0.7125 V + 12.5 mV steps, bit 7 enables the output), a control register and the chip ID
//! Linux checks (die 8, `SILERGY_SYR82X`).
use crate::i2c::{I2cDevice, RegisterPointer};

/// I²C addresses on the panel board.
pub const CPU_ADDRESS: u8 = 0x40;
pub const GPU_ADDRESS: u8 = 0x41;

const VSEL0: usize = 0x00;
const VSEL1: usize = 0x01;
const ID1: usize = 0x03;
const ID2: usize = 0x04;
const MONITOR: usize = 0x05;
const BUCK_EN: u8 = 1 << 7;

pub struct Syr82x {
    regs: [u8; 8],
    pointer: RegisterPointer,
}

impl Default for Syr82x {
    /// On at 1.1 V in both selectors, as the boot loader leaves the CPU and GPU rails.
    fn default() -> Self {
        let mut regs = [0u8; 8];
        regs[VSEL0] = BUCK_EN | selector(1_100_000);
        regs[VSEL1] = BUCK_EN | selector(1_100_000);
        regs[ID1] = 0x80 | 8; // vendor bits, die ID 8 (SYR82x)
        regs[ID2] = 0x01;
        regs[MONITOR] = 0x80; // power good
        Self { regs, pointer: RegisterPointer::default() }
    }
}

/// The selector for `microvolts`.
pub fn selector(microvolts: u32) -> u8 {
    (microvolts.saturating_sub(712_500) / 12_500).min(63) as u8
}

impl Syr82x {
    /// Output in microvolts from selector `n` (0 or 1; which one is live is the board's
    /// VSEL pin, the other one is the suspend voltage), or None while it is off.
    pub fn output(&self, n: usize) -> Option<u32> {
        let v = self.regs[VSEL0 + n];
        (v & BUCK_EN != 0).then(|| 712_500 + (v & 0x3f) as u32 * 12_500)
    }
}

impl I2cDevice for Syr82x {
    fn start(&mut self, read: bool) -> bool {
        self.pointer.start(read);
        true
    }

    fn write(&mut self, byte: u8) -> bool {
        if let Some(reg) = self.pointer.written(byte) {
            match reg as usize {
                ID1 | ID2 | MONITOR => {}
                r if r < self.regs.len() => self.regs[r] = byte,
                _ => {}
            }
        }
        true
    }

    fn read(&mut self) -> u8 {
        self.regs.get(self.pointer.next() as usize).copied().unwrap_or(0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reports_a_syr82x_and_reads_back_the_voltage() {
        let mut buck = Syr82x::default();
        buck.start(false);
        buck.write(ID1 as u8);
        buck.start(true);
        assert_eq!(buck.read() & 0x0f, 8);
        assert_eq!(buck.output(0), Some(1_100_000));
        buck.start(false);
        buck.write(VSEL0 as u8);
        buck.write(BUCK_EN | selector(1_250_000));
        assert_eq!(buck.output(0), Some(1_250_000));
    }
}
