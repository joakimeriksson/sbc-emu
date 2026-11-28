//! UART A, with the Tegra high-speed driver's PIO interrupt and FIFO behavior.
use peripherals::Uart;
use std::collections::VecDeque;

pub const CLOCK_HZ: u32 = 16 * 115_200;
// Bring-up reset controller used by the generated DTB until BPMP supplies resets.
pub const FIXTURE_RESET: u64 = 0x7000_0000;
const FIFO_SIZE: usize = 32;
const IER_RX: u8 = 1 << 0;
const IER_TX: u8 = 1 << 1;
const IER_TIMEOUT: u8 = 1 << 4;
const DLAB: u8 = 1 << 7;

pub(super) enum UartA {
    Legacy(Uart),
    HighSpeed(HighSpeed),
}

impl UartA {
    pub fn new(high_speed: bool) -> Self {
        if high_speed {
            Self::HighSpeed(HighSpeed::default())
        } else {
            Self::Legacy(Uart::default())
        }
    }

    pub fn high_speed(&self) -> bool {
        matches!(self, Self::HighSpeed(_))
    }

    pub fn push_rx(&mut self, byte: u8, now: u64) -> bool {
        match self {
            Self::Legacy(uart) => uart.push_rx(byte),
            Self::HighSpeed(uart) => uart.push_rx(byte, now),
        }
    }

    /// Bytes received and not read by the guest yet.
    #[cfg(not(target_arch = "wasm32"))]
    pub fn rx_len(&self) -> usize {
        match self {
            Self::Legacy(uart) => uart.rx_len(),
            Self::HighSpeed(uart) => uart.rx.len(),
        }
    }

    /// Whether the guest sent bytes that `take_output` has not taken.
    #[cfg(not(target_arch = "wasm32"))]
    pub fn has_output(&self) -> bool {
        match self {
            Self::Legacy(uart) => !uart.out.is_empty(),
            Self::HighSpeed(uart) => !uart.out.is_empty(),
        }
    }

    pub fn take_output(&mut self) -> Vec<u8> {
        match self {
            Self::Legacy(uart) => std::mem::take(&mut uart.out),
            Self::HighSpeed(uart) => std::mem::take(&mut uart.out),
        }
    }

    pub fn read(&mut self, offset: u64, now: u64) -> u32 {
        match self {
            Self::Legacy(uart) => uart.read(offset),
            Self::HighSpeed(uart) => uart.read(offset, now),
        }
    }

    pub fn write(&mut self, offset: u64, value: u32, now: u64) {
        match self {
            Self::Legacy(uart) => uart.write(offset, value),
            Self::HighSpeed(uart) => uart.write(offset, value as u8, now),
        }
    }

    pub fn irq(&self, now: u64) -> bool {
        match self {
            Self::Legacy(uart) => uart.irq(),
            Self::HighSpeed(uart) => uart.interrupt_id(now) != 1,
        }
    }

    pub fn next_event(&self, now: u64) -> Option<u64> {
        match self {
            Self::Legacy(_) => None,
            Self::HighSpeed(uart) => uart.timeout().filter(|&deadline| deadline > now),
        }
    }

    pub fn reset_asserted(&self) -> Option<bool> {
        match self {
            Self::Legacy(_) => None,
            Self::HighSpeed(uart) => Some(uart.reset_asserted),
        }
    }

    pub fn set_clock_rate(&mut self, rate: u64) {
        if let Self::HighSpeed(uart) = self {
            uart.clock_rate = Some(rate);
        }
    }

    pub fn set_reset(&mut self, asserted: bool) {
        if let Self::HighSpeed(uart) = self {
            if asserted && !uart.reset_asserted {
                let out = std::mem::take(&mut uart.out);
                *uart = HighSpeed {
                    out,
                    clock_rate: uart.clock_rate,
                    ..Default::default()
                };
            }
            uart.reset_asserted = asserted;
        }
    }
}

#[derive(Default)]
pub(super) struct HighSpeed {
    out: Vec<u8>,
    rx: VecDeque<u8>,
    last_rx: u64,
    clock_rate: Option<u64>,
    divisor: u16,
    ier: u8,
    lcr: u8,
    mcr: u8,
    scratch: u8,
    fcr: u8,
    tx_pending: bool,
    reset_asserted: bool,
}

impl HighSpeed {
    fn push_rx(&mut self, byte: u8, now: u64) -> bool {
        if self.reset_asserted || self.rx.len() >= FIFO_SIZE {
            return false;
        }
        self.rx.push_back(byte);
        self.last_rx = now;
        true
    }

    fn rx_trigger(&self) -> usize {
        if self.fcr & 1 == 0 {
            return 1;
        }
        // The Tegra driver programs R_TRIG_11 for a four-byte PIO threshold.
        [1, 4, 8, 4][(self.fcr >> 6) as usize]
    }

    fn timeout(&self) -> Option<u64> {
        if self.ier & IER_TIMEOUT == 0 || self.rx.is_empty() {
            return None;
        }
        let bits = 1
            + (5 + (self.lcr & 3) as u64)
            + u64::from(self.lcr & 8 != 0)
            + 1
            + u64::from(self.lcr & 4 != 0);
        let ticks = bits * 4 * 16 * u64::from(self.divisor.max(1));
        Some(self.last_rx.saturating_add(
            (ticks * 1_000_000_000).div_ceil(self.clock_rate.unwrap_or(u64::from(CLOCK_HZ))),
        ))
    }

    fn interrupt_id(&self, now: u64) -> u8 {
        if self.reset_asserted {
            1
        } else if self.ier & IER_RX != 0 && self.rx.len() >= self.rx_trigger() {
            4
        } else if self.timeout().is_some_and(|deadline| now >= deadline) {
            0x0c
        } else if self.ier & IER_TX != 0 && self.tx_pending {
            2
        } else {
            1
        }
    }

    fn read(&mut self, offset: u64, now: u64) -> u32 {
        u32::from(match offset {
            0 if self.lcr & DLAB != 0 => self.divisor as u8,
            0 => {
                self.last_rx = now;
                self.rx.pop_front().unwrap_or(0)
            }
            4 if self.lcr & DLAB != 0 => (self.divisor >> 8) as u8,
            4 => self.ier,
            8 => {
                let id = self.interrupt_id(now);
                if id == 2 {
                    self.tx_pending = false;
                }
                id | if self.fcr & 1 != 0 { 0xc0 } else { 0 }
            }
            0x0c => self.lcr,
            0x10 => self.mcr,
            0x14 => 0x60 | u8::from(!self.rx.is_empty()), // TEMT, THRE, DR
            0x18 => 0xb0,                                 // CTS, DSR, DCD
            0x1c => self.scratch,
            _ => 0,
        })
    }

    fn write(&mut self, offset: u64, value: u8, now: u64) {
        if self.reset_asserted {
            return;
        }
        match offset {
            0 if self.lcr & DLAB != 0 => self.divisor = (self.divisor & 0xff00) | u16::from(value),
            0 => {
                if self.mcr & 0x10 != 0 {
                    self.push_rx(value, now);
                } else {
                    self.out.push(value);
                }
                self.tx_pending = true;
            }
            4 if self.lcr & DLAB != 0 => {
                self.divisor = (self.divisor & 0xff) | (u16::from(value) << 8)
            }
            4 => {
                self.tx_pending |= value & !self.ier & IER_TX != 0;
                self.ier = value & 0x3f;
            }
            8 => {
                self.fcr = value & !6;
                if value & 2 != 0 {
                    self.rx.clear();
                }
            }
            0x0c => self.lcr = value,
            0x10 => self.mcr = value & 0x7f,
            0x1c => self.scratch = value,
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn receive_timeout_and_threshold_use_distinct_iir_codes() {
        let mut uart = UartA::new(true);
        uart.write(8, 0xc1, 0);
        uart.write(4, IER_RX as u32 | IER_TIMEOUT as u32, 0);
        uart.write(0x0c, 3, 0); // 8N1
        assert!(uart.push_rx(b'a', 0));
        let deadline = uart.next_event(0).unwrap();
        assert_eq!(deadline, 347_223);
        assert!(!uart.irq(deadline - 1));
        assert_eq!(uart.read(8, deadline), 0xcc);
        assert_eq!(uart.read(0, deadline), b'a' as u32);
        assert!(!uart.irq(deadline));

        for byte in b"bcde" {
            assert!(uart.push_rx(*byte, deadline));
        }
        assert_eq!(uart.read(8, deadline), 0xc4);
        uart.set_reset(true);
        assert!(!uart.push_rx(b'f', deadline));
        uart.set_reset(false);
        assert_eq!(uart.read(0x14, deadline), 0x60);
    }
}
