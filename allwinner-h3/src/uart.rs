use std::collections::VecDeque;

const RX_CAPACITY: usize = 64;
const LCR_DLAB: u8 = 1 << 7;
const IER_RDA: u8 = 1 << 0;
const IER_THRE: u8 = 1 << 1;
const MCR_LOOP: u8 = 1 << 4;

/// The H3 UART: a Synopsys DesignWare APB UART (16550-compatible, four-byte register
/// spacing). Transmission is instantaneous, so the transmitter is always empty; the
/// THRE interrupt follows 16550 rules (cleared by reading IIR or writing THR).
pub struct Uart {
    pub out: Vec<u8>,
    rx: VecDeque<u8>,
    divisor: u16,
    ier: u8,
    lcr: u8,
    mcr: u8,
    scr: u8,
    fifo_enabled: bool,
    thre_pending: bool,
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
            fifo_enabled: false,
            thre_pending: false,
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

    /// Interrupt output (level).
    pub fn irq(&self) -> bool {
        self.ier & IER_RDA != 0 && !self.rx.is_empty()
            || self.ier & IER_THRE != 0 && self.thre_pending
    }

    fn iir(&mut self) -> u8 {
        let fifo = if self.fifo_enabled { 0xc0 } else { 0 };
        if self.ier & IER_RDA != 0 && !self.rx.is_empty() {
            fifo | 0x04
        } else if self.ier & IER_THRE != 0 && self.thre_pending {
            // Reading IIR with THRE as the source clears it.
            self.thre_pending = false;
            fifo | 0x02
        } else {
            fifo | 0x01
        }
    }

    fn msr(&self) -> u8 {
        if self.mcr & MCR_LOOP != 0 {
            // Loopback: DTR->DSR, RTS->CTS, OUT1->RI, OUT2->DCD.
            let m = self.mcr;
            (m & 1) << 5 | (m & 2) << 3 | (m & 4) << 4 | (m & 8) << 4
        } else {
            0xb0 // DCD, DSR and CTS asserted
        }
    }

    /// `offset` is the byte offset of a naturally aligned, four-byte-spaced register.
    pub fn read(&mut self, offset: u64) -> u32 {
        match offset {
            0x00 if self.lcr & LCR_DLAB != 0 => self.divisor as u8 as u32,
            0x00 => self.rx.pop_front().unwrap_or(0) as u32,
            0x04 if self.lcr & LCR_DLAB != 0 => (self.divisor >> 8) as u32,
            0x04 => self.ier as u32,
            0x08 => self.iir() as u32,
            0x0c => self.lcr as u32,
            0x10 => self.mcr as u32,
            // LSR: data ready, transmitter holding register empty, transmitter empty.
            0x14 => 0x60 | u32::from(!self.rx.is_empty()),
            0x18 => self.msr() as u32,
            0x1c => self.scr as u32,
            // USR: not busy, TX FIFO not full and empty, RX FIFO not empty / full.
            0x7c => {
                0x06 | u32::from(!self.rx.is_empty()) << 3
                    | u32::from(self.rx.len() == RX_CAPACITY) << 4
            }
            0x80 => 0,                    // TFL
            0x84 => self.rx.len() as u32, // RFL
            0xfc => 0x4457_0110,          // CTR: DesignWare peripheral ID
            _ => 0,
        }
    }

    pub fn write(&mut self, offset: u64, value: u32) {
        let value = value as u8;
        match offset {
            0x00 if self.lcr & LCR_DLAB != 0 => {
                self.divisor = (self.divisor & 0xff00) | value as u16;
            }
            0x00 => {
                if self.mcr & MCR_LOOP != 0 {
                    let _ = self.push_rx(value);
                } else {
                    self.out.push(value);
                }
                self.thre_pending = true; // transmitted at once
            }
            0x04 if self.lcr & LCR_DLAB != 0 => {
                self.divisor = (self.divisor & 0x00ff) | ((value as u16) << 8);
            }
            0x04 => {
                let enabling_thre = value & IER_THRE != 0 && self.ier & IER_THRE == 0;
                self.ier = value & 0x0f;
                if enabling_thre {
                    self.thre_pending = true;
                }
            }
            0x08 => {
                self.fifo_enabled = value & 1 != 0;
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
