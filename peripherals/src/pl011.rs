//! ARM PL011 UART (UART0). Transmission is instantaneous (the TX FIFO is always empty), received
//! bytes queue in a 32-entry FIFO. RIS bits: RX 4, TX 5, RT 6.
use std::collections::VecDeque;

pub struct Pl011 {
    pub out: Vec<u8>,
    rx: VecDeque<u8>,
    imsc: u32,
    cr: u32,
    lcr: u32,
    ibrd: u32,
    fbrd: u32,
    ifls: u32,
    /// TX interrupt latched (set when the FIFO drains, cleared by ICR or a write)
    tx_ris: bool,
}

impl Default for Pl011 {
    fn default() -> Self { Self::new() }
}

impl Pl011 {
    pub fn new() -> Self {
        Pl011 { out: Vec::new(), rx: VecDeque::new(), imsc: 0, cr: 0x300, lcr: 0, ibrd: 0, fbrd: 0, ifls: 0x12, tx_ris: true }
    }

    pub fn push_rx(&mut self, b: u8) { if self.rx.len() < 32 { self.rx.push_back(b); } }
    pub fn rx_room(&self) -> bool { self.rx.len() < 32 }

    fn ris(&self) -> u32 {
        let rx = if self.rx.is_empty() { 0 } else { (1 << 4) | (1 << 6) };
        rx | ((self.tx_ris as u32) << 5)
    }

    pub fn irq(&self) -> bool { self.ris() & self.imsc != 0 }

    pub fn read(&mut self, off: u64) -> u32 {
        match off {
            0x00 => self.rx.pop_front().map(|b| b as u32).unwrap_or(0),
            0x18 => (1 << 7) | if self.rx.is_empty() { 1 << 4 } else { 0 }, // TXFE, RXFE
            0x24 => self.ibrd,
            0x28 => self.fbrd,
            0x2c => self.lcr,
            0x30 => self.cr,
            0x34 => self.ifls,
            0x38 => self.imsc,
            0x3c => self.ris(),
            0x40 => self.ris() & self.imsc,
            0xfe0 => 0x11,
            0xfe4 => 0x10,
            0xfe8 => 0x34,
            0xfec => 0x00,
            0xff0 => 0x0d,
            0xff4 => 0xf0,
            0xff8 => 0x05,
            0xffc => 0xb1,
            _ => 0,
        }
    }

    pub fn write(&mut self, off: u64, v: u32) {
        match off {
            0x00 => { self.out.push(v as u8); self.tx_ris = true; }
            0x24 => self.ibrd = v & 0xffff,
            0x28 => self.fbrd = v & 0x3f,
            0x2c => self.lcr = v & 0xff,
            0x30 => self.cr = v & 0xffff,
            0x34 => self.ifls = v & 0x3f,
            0x38 => self.imsc = v & 0x7ff,
            0x44 => { if v & (1 << 5) != 0 { self.tx_ris = false; } }
            _ => {}
        }
    }
}

impl Pl011 {
    pub fn save_state(&self, w: &mut emu_core::snap::W) {
        w.tag("pl011-v1");
        w.bytes(&self.rx.iter().copied().collect::<Vec<u8>>());
        for v in [self.imsc, self.cr, self.lcr, self.ibrd, self.fbrd, self.ifls] { w.u32(v); }
        w.bool(self.tx_ris);
    }
    pub fn load_state(&mut self, r: &mut emu_core::snap::R) -> Result<(), emu_core::snap::SnapError> {
        r.tag("pl011-v1")?;
        self.rx = r.bytes()?.iter().copied().collect();
        for v in [&mut self.imsc, &mut self.cr, &mut self.lcr, &mut self.ibrd, &mut self.fbrd, &mut self.ifls] { *v = r.u32()?; }
        self.tx_ris = r.bool()?;
        Ok(())
    }
}
