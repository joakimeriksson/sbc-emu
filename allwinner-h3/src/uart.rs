use std::collections::VecDeque;

const RX_CAPACITY: usize = 32;
const LCR_DLAB: u8 = 1 << 7;

/// The H3 UART's 16550-compatible register subset, with four-byte register spacing.
pub struct Uart {
    pub out: Vec<u8>,
    rx: VecDeque<u8>,
    divisor: u16,
    ier: u8,
    lcr: u8,
    mcr: u8,
    scr: u8,
}

impl Default for Uart {
    fn default() -> Self {
        Self {
            out: Vec::new(),
            rx: VecDeque::with_capacity(RX_CAPACITY),
            divisor: 0,
            ier: 0,
            lcr: 0,
            mcr: 0,
            scr: 0,
        }
    }
}

impl Uart {
    /// Queue host input without dropping a byte when the receive FIFO is full.
    pub fn push_rx(&mut self, byte: u8) -> bool {
        if self.rx.len() == RX_CAPACITY {
            return false;
        }
        self.rx.push_back(byte);
        true
    }

    /// `offset` is the byte offset of a naturally aligned, four-byte-spaced register.
    pub fn read(&mut self, offset: u64) -> u32 {
        let value = match offset {
            0x00 if self.lcr & LCR_DLAB != 0 => self.divisor as u8,
            0x00 => self.rx.pop_front().unwrap_or(0),
            0x04 if self.lcr & LCR_DLAB != 0 => (self.divisor >> 8) as u8,
            0x04 => self.ier,
            0x08 => 0x01, // IIR: no interrupt pending
            0x0c => self.lcr,
            0x10 => self.mcr,
            // LSR: receiver data, transmitter holding register empty, transmitter empty.
            0x14 => 0x60 | u8::from(!self.rx.is_empty()),
            0x18 => 0xb0, // MSR: DCD, DSR and CTS asserted
            0x1c => self.scr,
            _ => 0,
        };
        value as u32
    }

    pub fn write(&mut self, offset: u64, value: u32) {
        let value = value as u8;
        match offset {
            0x00 if self.lcr & LCR_DLAB != 0 => {
                self.divisor = (self.divisor & 0xff00) | value as u16;
            }
            0x00 => self.out.push(value),
            0x04 if self.lcr & LCR_DLAB != 0 => {
                self.divisor = (self.divisor & 0x00ff) | ((value as u16) << 8);
            }
            0x04 => self.ier = value & 0x0f,
            0x08 => {
                // FCR receive-FIFO reset. Output already delivered to the host is retained.
                if value & 0x02 != 0 {
                    self.rx.clear();
                }
            }
            0x0c => self.lcr = value,
            0x10 => self.mcr = value & 0x1f,
            0x1c => self.scr = value,
            _ => {}
        }
    }
}
