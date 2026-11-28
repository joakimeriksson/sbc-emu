//! Tegra234 HSP TOP0. The first model exposes the CCPLEX and BPMP doorbells.
pub const BASE: u64 = 0x03c0_0000;
pub const SIZE: u64 = 0x000a_0000;
pub const SPI: u32 = 176;
pub const BPMP_MASTER: u32 = 19;
const CCPLEX_MASTER: u32 = 17;
const DIMENSIONING: u64 = 0x380;
// The shared mailboxes and shared interrupts (TCU receive) are modelled in tcu.rs;
// this file is the doorbell bank.
const SHARED_MAILBOXES: u32 = 8;
const SHARED_INTERRUPTS: u32 = 8;
const DOORBELL_BANK: u64 = (1 + SHARED_MAILBOXES as u64 / 2) * 0x10000;
const CCPLEX: u64 = DOORBELL_BANK + 0x100;
const BPMP: u64 = DOORBELL_BANK + 0x300;

#[derive(Default)]
pub struct Hsp {
    enable: u32,
    pending: u32,
}

impl Hsp {
    pub fn read(&self, offset: u64) -> u32 {
        match offset {
            DIMENSIONING => (SHARED_INTERRUPTS << 16) | (7 << 12) | SHARED_MAILBOXES,
            o if o == CCPLEX + 4 => self.enable,
            o if o == CCPLEX + 8 => self.pending,
            o if o == CCPLEX + 12 => self.pending & self.enable,
            o if o == BPMP + 4 => 1 << CCPLEX_MASTER,
            _ => 0,
        }
    }

    /// Returns true when the host CPU has rung the firmware doorbell.
    pub fn write(&mut self, offset: u64, value: u32) -> bool {
        match offset {
            o if o == CCPLEX + 4 => self.enable = value,
            o if o == CCPLEX + 12 => self.pending &= !value,
            BPMP => return value & 1 != 0,
            _ => {}
        }
        false
    }

    pub fn ring_ccplex(&mut self) {
        self.pending |= 1 << BPMP_MASTER;
    }

    pub fn irq(&self) -> bool {
        self.pending & self.enable != 0
    }
}
