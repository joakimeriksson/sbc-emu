//! Rockchip GPIO bank (`rockchip,gpio-bank`, register layout v1 of Linux `gpio-rockchip`):
//! 32 pins with output data and direction registers, interrupt configuration that reads
//! back, and the input level register EXT_PORT. Outputs read back what is driven; inputs
//! read the level the board puts on them, high by default (pull-ups, released buttons,
//! inactive-low interrupt lines such as the RK808's). Inputs the board changes (a touch
//! controller's interrupt line) raise edge or level interrupts as INTEN/INTMASK/INTTYPE_LEVEL/
//! INT_POLARITY configure them; PORTA_EOI clears edge interrupts.
//!
//! The level matters: the Tinker Board's power button (`gpio-keys`, GPIO0_A5, active low)
//! read as pressed at probe makes systemd-logind power the board off.

pub const SIZE: u64 = 0x1000;

/// GPIO0 (PMU domain), then GPIO1-8 at 0xff780000 + 0x10000 * (n - 1).
pub const BANKS: [u64; 9] = [
    0xff75_0000, 0xff78_0000, 0xff79_0000, 0xff7a_0000, 0xff7b_0000, 0xff7c_0000, 0xff7d_0000,
    0xff7e_0000, 0xff7f_0000,
];

const SWPORT_DR: usize = 0x00;
const SWPORT_DDR: usize = 0x04;
const INTEN: usize = 0x30;
const INTMASK: usize = 0x34;
const INTTYPE_LEVEL: usize = 0x38;
const INT_POLARITY: usize = 0x3c;
const INT_STATUS: usize = 0x40;
const INT_RAWSTATUS: usize = 0x44;
const PORTA_EOI: usize = 0x4c;
const EXT_PORT: usize = 0x50;

pub struct GpioBank {
    regs: [u32; 0x20],
    /// Levels the board drives onto pins configured as inputs.
    pub inputs: u32,
    /// Latched edge interrupts.
    edges: u32,
}

impl Default for GpioBank {
    fn default() -> Self {
        Self { regs: [0; 0x20], inputs: u32::MAX, edges: 0 }
    }
}

impl GpioBank {
    pub fn read(&self, offset: u64) -> u32 {
        let offset = offset as usize;
        match offset {
            EXT_PORT => {
                let output = self.regs[SWPORT_DDR / 4];
                self.regs[SWPORT_DR / 4] & output | self.inputs & !output
            }
            INT_RAWSTATUS => self.raw(),
            INT_STATUS => self.raw() & !self.regs[INTMASK / 4],
            PORTA_EOI => 0,
            _ if offset < 0x80 => self.regs[offset / 4],
            _ => 0,
        }
    }

    pub fn write(&mut self, offset: u64, value: u32) {
        let offset = offset as usize;
        match offset {
            PORTA_EOI => self.edges &= !value,
            INT_STATUS | INT_RAWSTATUS | EXT_PORT => {}
            _ if offset < 0x80 => self.regs[offset / 4] = value,
            _ => {}
        }
    }
}

impl GpioBank {
    /// Pending interrupts before masking: latched edges, and level-triggered pins at their
    /// active level.
    fn raw(&self) -> u32 {
        let en = self.regs[INTEN / 4];
        let edge = self.regs[INTTYPE_LEVEL / 4];
        let high = self.regs[INT_POLARITY / 4];
        let level = !(self.inputs ^ high) & !edge;
        (self.edges & edge | level) & en
    }

    /// The bank's interrupt line.
    pub fn irq(&self) -> bool {
        self.raw() & !self.regs[INTMASK / 4] != 0
    }

    /// The board drives input `pin` to `level`; an edge in the configured direction latches
    /// an edge interrupt.
    pub fn set_input(&mut self, pin: u32, level: bool) {
        let bit = 1u32 << pin;
        let old = self.inputs & bit != 0;
        if old == level {
            return;
        }
        if level { self.inputs |= bit } else { self.inputs &= !bit }
        let rising = self.regs[INT_POLARITY / 4] & bit != 0;
        if self.regs[INTEN / 4] & self.regs[INTTYPE_LEVEL / 4] & bit != 0 && level == rising {
            self.edges |= bit;
        }
    }
}

emu_core::snap_struct!(GpioBank { regs, inputs, edges });

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn inputs_read_high_and_outputs_read_back() {
        let mut bank = GpioBank::default();
        assert_eq!(bank.read(EXT_PORT as u64) >> 5 & 1, 1, "power button released");
        bank.write(SWPORT_DDR as u64, 1 << 11);
        bank.write(SWPORT_DR as u64, 0);
        assert_eq!(bank.read(EXT_PORT as u64) & (1 << 11 | 1 << 5), 1 << 5);
        bank.inputs &= !(1 << 5);
        assert_eq!(bank.read(EXT_PORT as u64) & 1 << 5, 0, "pressed");
    }

    #[test]
    fn falling_edge_interrupt() {
        let mut bank = GpioBank::default();
        bank.write(INTTYPE_LEVEL as u64, 1 << 6);
        bank.write(INTEN as u64, 1 << 6);
        assert!(!bank.irq());
        bank.set_input(6, false);
        assert!(bank.irq());
        bank.write(PORTA_EOI as u64, 1 << 6);
        assert!(!bank.irq());
        bank.set_input(6, true);
        assert!(!bank.irq(), "rising edge with falling polarity");
    }
}
