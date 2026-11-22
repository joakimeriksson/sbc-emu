//! Tegra210 GPIO controller (`nvidia,tegra210-gpio`): eight banks of four ports of eight pins,
//! each bank with one interrupt. Register blocks are 0x100 apart; at +0x80 the masked writes
//! (bits 15:8 select the pins bits 7:0 set).
pub const SIZE: u64 = 0x1000;
pub const BANKS: usize = 8;
/// GIC interrupts of the banks.
pub const IRQS: [usize; BANKS] = [32 + 0x20, 32 + 0x21, 32 + 0x22, 32 + 0x23, 32 + 0x37, 32 + 0x57, 32 + 0x59, 32 + 0x7d];

const CNF: usize = 0;
const OE: usize = 1;
const OUT: usize = 2;
const IN: usize = 3;
const INT_STA: usize = 4;
const INT_ENB: usize = 5;
const INT_LVL: usize = 6;
const INT_CLR: usize = 7;

#[derive(Clone, Copy, Default)]
struct Port {
    /// CNF, OE, OUT, -, INT_STA, INT_ENB, INT_LVL, -
    regs: [u32; 8],
    /// levels the board drives on the pins
    input: u8,
}

pub struct Gpio {
    ports: [[Port; 4]; BANKS],
    other: std::collections::HashMap<u64, u32>,
}

impl Default for Gpio {
    fn default() -> Self {
        // undriven inputs read high (the pads' pull-ups)
        let port = Port { input: 0xff, ..Default::default() };
        Self { ports: [[port; 4]; BANKS], other: Default::default() }
    }
}

impl Port {
    fn pins(&self) -> u8 {
        let driven = (self.regs[OE] & self.regs[CNF]) as u8;
        self.regs[OUT] as u8 & driven | self.input & !driven
    }

    /// Latch the interrupts of a change of the pins from `old`, and the levels held.
    fn changed(&mut self, old: u8) {
        let (new, lvl) = (self.pins(), self.regs[INT_LVL]);
        let (level, edge, both) = (lvl as u8, (lvl >> 8) as u8, (lvl >> 16) as u8);
        let (rose, fell) = (new & !old, old & !new);
        let edges = edge & (both & (rose | fell) | !both & (level & rose | !level & fell));
        let levels = !edge & (level & new | !level & !new);
        self.regs[INT_STA] |= ((edges | levels) & self.regs[INT_ENB] as u8) as u32;
    }
}

impl Gpio {
    /// Drive input pin `n` (the number in the device tree).
    pub fn set_input(&mut self, n: usize, high: bool) {
        let port = &mut self.ports[n / 32][n / 8 % 4];
        let old = port.pins();
        port.input = port.input & !(1 << (n % 8)) | (high as u8) << (n % 8);
        port.changed(old);
    }

    /// What the board sees on pin `n`.
    pub fn pin(&self, n: usize) -> bool {
        self.ports[n / 32][n / 8 % 4].pins() >> (n % 8) & 1 != 0
    }

    pub fn irq(&self, bank: usize) -> bool {
        self.ports[bank].iter().any(|p| p.regs[INT_STA] & p.regs[INT_ENB] != 0)
    }

    fn decode(offset: u64) -> Option<(usize, usize, usize, bool)> {
        let (bank, low) = ((offset >> 8) as usize, (offset & 0xff) as usize);
        let masked = low & 0x80 != 0;
        (bank < BANKS).then_some((bank, low >> 2 & 3, low >> 4 & 7, masked))
    }

    pub fn read(&self, offset: u64) -> u32 {
        match Self::decode(offset) {
            Some((b, p, IN, false)) => self.ports[b][p].pins() as u32,
            Some((b, p, r, false)) => self.ports[b][p].regs[r],
            _ => self.other.get(&offset).copied().unwrap_or(0),
        }
    }

    pub fn write(&mut self, offset: u64, value: u32) {
        let Some((b, p, r, masked)) = Self::decode(offset) else { return };
        // masked: CNF, OE, OUT, debounce enable (stored), INT_STA, INT_ENB, INT_LVL
        if masked && r == IN || masked && r == INT_CLR {
            self.other.insert(offset, value);
            return;
        }
        let port = &mut self.ports[b][p];
        let old = port.pins();
        let v = if masked {
            let m = value >> 8 & 0xff;
            port.regs[r] & !m | value & m
        } else {
            value
        };
        match r {
            IN => {}
            INT_CLR => port.regs[INT_STA] &= !value,
            INT_STA => port.regs[INT_STA] &= if masked { v } else { !value },
            _ => port.regs[r] = v,
        }
        port.changed(old); // and levels that still hold
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn masked_writes_drive_pins() {
        let mut g = Gpio::default();
        // pin 203 (bank 6, port 1, bit 3): the SD card's supply
        g.write(0x604, 1 << 3);
        g.write(0x694, 1 << 11 | 1 << 3); // MSK_OE
        g.write(0x6a4, 1 << 11 | 1 << 3); // MSK_OUT high
        assert!(g.pin(203));
        g.write(0x6a4, 1 << 11);
        assert!(!g.pin(203));
        assert_eq!(g.read(0x624) & 0xff, 0);
    }

    #[test]
    fn edge_and_level_interrupts() {
        let mut g = Gpio::default();
        // pin 201 (bank 6, port 1, bit 1), both edges
        g.write(0x664, 0x01_0100 << 1);
        g.write(0x6d4, 1 << 9 | 1 << 1); // MSK_INT_ENB
        assert!(!g.irq(6));
        g.set_input(201, false);
        assert!(g.irq(6) && !g.irq(5));
        assert_eq!(g.read(0x634) >> 1 & 1, 0);
        g.write(0x674, 1 << 1); // INT_CLR
        assert!(!g.irq(6));
        g.set_input(201, true);
        assert!(g.irq(6));
        g.write(0x674, 1 << 1);
        // low level: stays until the level goes
        g.write(0x664, 0);
        g.set_input(201, false);
        g.write(0x674, 1 << 1);
        assert!(g.irq(6));
        g.set_input(201, true);
        g.write(0x674, 1 << 1);
        assert!(!g.irq(6));
    }
}
