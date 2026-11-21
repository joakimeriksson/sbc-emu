//! RK3588 UART2: DesignWare APB UART, 32-bit registers at four-byte spacing.
//! Immediate TX, 64-byte RX FIFO; no baud timing, DMA or external modem.
use std::collections::VecDeque;

#[derive(Default)]
pub struct Uart {
    pub out: Vec<u8>,
    rx: VecDeque<u8>,
    ier: u8,
    lcr: u8,
    mcr: u8,
    fcr: u8,
    dll: u8,
    dlm: u8,
    scratch: u8,
    tx_pending: bool,
}

impl Uart {
    /// Whether the RX FIFO can take another byte.
    pub fn rx_room(&self) -> bool { self.rx.len() < 64 }
    pub fn input(&mut self, b: u8) -> bool {
        if !self.rx_room() { return false; }
        self.rx.push_back(b);
        true
    }
    fn iir(&self) -> u8 {
        let fifo = if self.fcr & 1 != 0 { 0xc0 } else { 0 };
        fifo | if self.ier & 1 != 0 && !self.rx.is_empty() { 4 }
        else if self.ier & 2 != 0 && self.tx_pending { 2 } else { 1 }
    }
    pub fn irq(&self) -> bool { self.iir() & 1 == 0 }
    pub fn read(&mut self, off: u64) -> u32 {
        match off {
            0 if self.lcr & 0x80 != 0 => self.dll as u32,
            4 if self.lcr & 0x80 != 0 => self.dlm as u32,
            0 => self.rx.pop_front().unwrap_or(0) as u32,
            4 => self.ier as u32,
            8 => { let iir = self.iir(); if iir & 0xf == 2 { self.tx_pending = false; } iir as u32 }
            12 => self.lcr as u32,
            16 => self.mcr as u32,
            20 => 0x60 | u32::from(!self.rx.is_empty()),
            24 => if self.mcr & 0x10 != 0 {
                ((self.mcr as u32 & 1) << 5) | ((self.mcr as u32 & 2) << 3) |
                ((self.mcr as u32 & 4) << 4) | ((self.mcr as u32 & 8) << 4)
            } else { 0xb0 },
            28 => self.scratch as u32,
            0x7c => 6 | (u32::from(!self.rx.is_empty()) << 3) | (u32::from(self.rx.len() == 64) << 4),
            0x80 => 0, // TX FIFO level
            0x84 => self.rx.len() as u32,
            0xf4 => (4 << 16) | (1 << 12) | (1 << 4), // 64-byte FIFO, FIFO_STAT, AFCE
            0xf8 => 0x3430_302a,
            0xfc => 0x4457_0110,
            _ => 0,
        }
    }
    pub fn write(&mut self, off: u64, value: u32) {
        let v = value as u8;
        match off {
            0 if self.lcr & 0x80 != 0 => self.dll = v,
            4 if self.lcr & 0x80 != 0 => self.dlm = v,
            0 => { if self.mcr & 0x10 != 0 { self.input(v); } else { self.out.push(v); } self.tx_pending = true; }
            4 => { if v & 2 != 0 && self.ier & 2 == 0 { self.tx_pending = true; } self.ier = v & 0xf; }
            8 => { self.fcr = v & 0xc9; if v & 2 != 0 { self.rx.clear(); } if v & 4 != 0 { self.tx_pending = true; } }
            12 => self.lcr = v,
            16 => self.mcr = v,
            28 => self.scratch = v,
            0x88 => { if v & 1 != 0 { let out=std::mem::take(&mut self.out); *self = Self::default(); self.out=out; } else { if v & 2 != 0 { self.rx.clear(); } if v & 4 != 0 { self.tx_pending = true; } } }
            _ => {}
        }
    }
}
