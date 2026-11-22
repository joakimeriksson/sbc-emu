//! Tegra I²C controller in packet mode (`nvidia,tegra210-i2c`, Linux `i2c-tegra`).
//!
//! The host writes a packet into the TX FIFO: three header words (protocol, payload size - 1,
//! then address and flags), and for a write the payload. Each packet runs at once when it is
//! complete; a read's bytes wait in the RX FIFO. PIO only: the driver uses DMA for transfers
//! longer than the PMIC's.
pub use peripherals::i2c::{I2cDevice, RegisterPointer};
use std::collections::VecDeque;

pub const SIZE: u64 = 0x100;

const CNFG: usize = 0x00;
const TX_FIFO: usize = 0x50;
const RX_FIFO: usize = 0x54;
const FIFO_CONTROL: usize = 0x5c;
const FIFO_STATUS: usize = 0x60;
const INT_MASK: usize = 0x64;
const INT_STATUS: usize = 0x68;
const BUS_CLEAR_CNFG: usize = 0x84;
const BUS_CLEAR_STATUS: usize = 0x88;
const CONFIG_LOAD: usize = 0x8c;

const INT_BUS_CLR_DONE: u32 = 1 << 11;
const INT_XFER_COMPLETE: u32 = 1 << 7;
const INT_NO_ACK: u32 = 1 << 3;
const INT_TX_REQ: u32 = 1 << 1;
const INT_RX_REQ: u32 = 1 << 0;

const HEADER_REPEAT_START: u32 = 1 << 16;
const HEADER_CONTINUE: u32 = 1 << 15;
const HEADER_READ: u32 = 1 << 19;
const HEADER_CONT_ON_NAK: u32 = 1 << 21;
/// FIFO depth in words
const DEPTH: usize = 8;

pub struct TegraI2c {
    regs: [u32; SIZE as usize / 4],
    /// header words of the packet being written, then its payload
    header: Vec<u32>,
    payload: Vec<u8>,
    rx: VecDeque<u8>,
    /// a read whose last bytes have not reached the FIFO yet
    completing: bool,
    devices: Vec<(u8, Box<dyn I2cDevice>)>,
    /// the device a CONTINUE left addressed, until a packet to it goes on or the host gives up
    /// (and its direction; Linux flushes the FIFOs before every packet, so that does not end it)
    current: Option<(usize, bool)>,
}

impl Default for TegraI2c {
    fn default() -> Self {
        Self { regs: [0; SIZE as usize / 4], header: Vec::new(), payload: Vec::new(), rx: VecDeque::new(), completing: false, devices: Vec::new(), current: None }
    }
}

impl TegraI2c {
    /// Attach a device at 7-bit address `address`.
    pub fn attach(&mut self, address: u8, device: Box<dyn I2cDevice>) {
        self.devices.push((address & 0x7f, device));
    }

    /// The clock and reset controller holds the controller in reset.
    pub fn reset(&mut self) {
        self.release();
        self.header.clear();
        self.payload.clear();
        self.rx.clear();
        self.completing = false;
        self.regs.fill(0);
    }

    /// STOP for a device that a CONTINUE left addressed.
    fn release(&mut self) {
        if let Some((i, _)) = self.current.take() {
            self.devices[i].1.stop();
        }
    }

    /// The interrupt status: the FIFO requests are levels.
    fn status(&self) -> u32 {
        let mut s = self.regs[INT_STATUS / 4];
        if !self.rx.is_empty() {
            s |= INT_RX_REQ;
        }
        // always room: a packet runs as soon as it is complete
        s | INT_TX_REQ
    }

    pub fn irq(&self) -> bool {
        self.status() & self.regs[INT_MASK / 4] != 0
    }

    pub fn read(&mut self, offset: u64) -> u32 {
        match offset as usize & 0xfc {
            RX_FIFO => {
                let mut w = [0; 4];
                for b in w.iter_mut() {
                    *b = self.rx.pop_front().unwrap_or(0);
                }
                self.check_complete();
                u32::from_le_bytes(w)
            }
            FIFO_STATUS => (DEPTH as u32) << 4 | self.rx.len().div_ceil(4).min(DEPTH) as u32,
            INT_STATUS => self.status(),
            // done at once
            CONFIG_LOAD => 0,
            BUS_CLEAR_STATUS => 1,
            o => self.regs[o / 4],
        }
    }

    pub fn write(&mut self, offset: u64, value: u32, now_ns: u64) {
        match offset as usize & 0xfc {
            TX_FIFO => self.push(value, now_ns),
            FIFO_CONTROL => {
                if value & 1 != 0 {
                    self.rx.clear();
                    self.completing = false;
                }
                if value & 2 != 0 {
                    self.header.clear();
                    self.payload.clear();
                }
                self.regs[FIFO_CONTROL / 4] = value & !3;
            }
            INT_STATUS => self.regs[INT_STATUS / 4] &= !value,
            BUS_CLEAR_CNFG => {
                self.regs[BUS_CLEAR_CNFG / 4] = value & !1;
                if value & 1 != 0 {
                    self.release();
                    self.regs[INT_STATUS / 4] |= INT_BUS_CLR_DONE;
                }
            }
            CNFG => {
                // leaving packet mode drops a half-written packet
                if value & 1 << 10 == 0 {
                    self.release();
                    self.header.clear();
                    self.payload.clear();
                }
                self.regs[CNFG / 4] = value;
            }
            o => self.regs[o / 4] = value,
        }
    }

    fn push(&mut self, word: u32, now: u64) {
        if self.header.len() < 3 {
            self.header.push(word);
        } else {
            self.payload.extend_from_slice(&word.to_le_bytes());
        }
        if self.header.len() < 3 {
            return;
        }
        let (len, flags) = ((self.header[1] & 0xfff) as usize + 1, self.header[2]);
        if flags & HEADER_READ == 0 && self.payload.len() < len {
            return;
        }
        let payload = std::mem::take(&mut self.payload);
        self.header.clear();
        self.packet(len, flags, &payload, now);
    }

    fn packet(&mut self, len: usize, flags: u32, payload: &[u8], now: u64) {
        let (read, address) = (flags & HEADER_READ != 0, (flags >> 1 & 0x7f) as u8);
        // a packet the same way to the device the last one left addressed goes on without a START
        let going_on = self.current.filter(|&(i, r)| self.devices[i].0 == address && r == read);
        if going_on.is_none() {
            self.release();
        }
        self.current = None;
        let ack = match going_on {
            Some((i, _)) => Some(i),
            None => self.devices.iter().position(|(a, _)| *a == address).filter(|&i| {
                let d = &mut self.devices[i].1;
                d.clock(now);
                d.start(read)
            }),
        };
        let Some(i) = ack else {
            if flags & HEADER_CONT_ON_NAK == 0 {
                self.regs[INT_STATUS / 4] |= INT_NO_ACK;
                return;
            }
            if read {
                self.rx.extend(std::iter::repeat_n(0xff, len)); // nobody drives the bus
            }
            self.completing = true;
            return self.check_complete();
        };
        let d = &mut self.devices[i].1;
        let mut ok = true;
        if read {
            for _ in 0..len {
                let b = d.read();
                self.rx.push_back(b);
            }
        } else {
            for &b in &payload[..len] {
                ok &= d.write(b);
                if !ok && flags & HEADER_CONT_ON_NAK == 0 {
                    break;
                }
            }
            ok |= flags & HEADER_CONT_ON_NAK != 0;
        }
        if flags & HEADER_CONTINUE != 0 && ok {
            self.current = Some((i, read));
        } else if flags & HEADER_REPEAT_START == 0 || !ok {
            d.stop();
        }
        if !ok {
            self.regs[INT_STATUS / 4] |= INT_NO_ACK;
            return;
        }
        self.completing = true;
        self.check_complete();
    }

    /// The packet completes when its last bytes are in the FIFO.
    fn check_complete(&mut self) {
        if self.completing && self.rx.len() <= 4 * DEPTH {
            self.completing = false;
            self.regs[INT_STATUS / 4] |= INT_XFER_COMPLETE;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Default)]
    struct Mem {
        p: RegisterPointer,
        data: Vec<u8>,
        stops: usize,
    }
    impl I2cDevice for Mem {
        fn start(&mut self, read: bool) -> bool {
            self.p.start(read);
            true
        }
        fn write(&mut self, byte: u8) -> bool {
            if let Some(r) = self.p.written(byte) {
                self.data[r as usize] = byte;
            }
            true
        }
        fn read(&mut self) -> u8 {
            self.data[self.p.next() as usize]
        }
        fn stop(&mut self) {
            self.stops += 1;
        }
    }

    fn packet(c: &mut TegraI2c, addr: u32, flags: u32, len: u32, data: &[u32]) {
        for w in [0x10 << 0, len - 1, flags | addr << 1].into_iter().chain(data.iter().copied()) {
            c.write(TX_FIFO as u64, w, 0);
        }
    }

    #[test]
    fn write_then_read_with_repeated_start() {
        let mut c = TegraI2c::default();
        c.attach(0x3c, Box::new(Mem { data: (0..=255).collect(), ..Default::default() }));
        c.write(INT_MASK as u64, INT_XFER_COMPLETE | INT_NO_ACK | INT_RX_REQ, 0);
        packet(&mut c, 0x3c, 0, 3, &[0x00bb_aa10]); // reg 0x10 = aa, 0x11 = bb
        assert!(c.irq());
        assert_eq!(c.read(INT_STATUS as u64) & INT_XFER_COMPLETE, INT_XFER_COMPLETE);
        c.write(INT_STATUS as u64, u32::MAX, 0);
        assert!(!c.irq());
        packet(&mut c, 0x3c, HEADER_REPEAT_START, 1, &[0x10]);
        c.write(INT_STATUS as u64, u32::MAX, 0);
        packet(&mut c, 0x3c, HEADER_READ, 5, &[]);
        assert_eq!(c.read(FIFO_STATUS as u64) & 15, 2);
        assert_eq!(c.read(RX_FIFO as u64), 0x1312_bbaa);
        assert_eq!(c.read(RX_FIFO as u64), 0x14);
        assert_eq!(c.read(INT_STATUS as u64) & (INT_XFER_COMPLETE | INT_RX_REQ), INT_XFER_COMPLETE);
    }

    #[test]
    fn absent_device_is_not_acknowledged() {
        let mut c = TegraI2c::default();
        packet(&mut c, 0x50, 0, 1, &[0]);
        assert_eq!(c.read(INT_STATUS as u64) & (INT_NO_ACK | INT_XFER_COMPLETE), INT_NO_ACK);
    }

    #[test]
    fn a_long_read_completes_when_its_tail_fits_the_fifo() {
        let mut c = TegraI2c::default();
        c.attach(0x3c, Box::new(Mem { data: (0..=255).collect(), ..Default::default() }));
        packet(&mut c, 0x3c, HEADER_READ, 40, &[]);
        assert_eq!(c.read(INT_STATUS as u64) & INT_XFER_COMPLETE, 0);
        c.read(RX_FIFO as u64);
        c.read(RX_FIFO as u64);
        assert_ne!(c.read(INT_STATUS as u64) & INT_XFER_COMPLETE, 0);
    }

    #[test]
    fn an_abandoned_continue_does_not_reach_the_next_transfer() {
        for end in [(FIFO_CONTROL, 3), (CNFG, 0), (BUS_CLEAR_CNFG, 1), (INT_STATUS, u32::MAX)] {
            let mut c = TegraI2c::default();
            c.attach(0x3c, Box::new(Mem { data: vec![0; 256], ..Default::default() }));
            c.attach(0x68, Box::new(Mem { data: vec![0x77; 256], ..Default::default() }));
            packet(&mut c, 0x3c, HEADER_READ | HEADER_CONTINUE, 1, &[]);
            c.read(RX_FIFO as u64);
            c.write(end.0 as u64, end.1, 0); // the host gives up (the last: it just goes on)
            c.write(INT_STATUS as u64, u32::MAX, 0);
            packet(&mut c, 0x68, 0, 2, &[0x1004]); // register 4 = 0x10 of the other device
            packet(&mut c, 0x68, HEADER_REPEAT_START, 1, &[4]);
            packet(&mut c, 0x68, HEADER_READ, 1, &[]);
            assert_eq!(c.read(RX_FIFO as u64), 0x10, "{end:x?}");
        }
    }

    #[test]
    fn a_read_that_ignores_the_missing_ack_gets_ones() {
        let mut c = TegraI2c::default();
        packet(&mut c, 0x50, HEADER_READ | HEADER_CONT_ON_NAK, 3, &[]);
        assert_eq!(c.read(INT_STATUS as u64) & (INT_XFER_COMPLETE | INT_RX_REQ | INT_NO_ACK), INT_XFER_COMPLETE | INT_RX_REQ);
        assert_eq!(c.read(RX_FIFO as u64), 0x00ff_ffff);
    }

    #[test]
    fn a_continued_read_goes_on_and_a_write_starts_anew() {
        let mut c = TegraI2c::default();
        c.attach(0x3c, Box::new(Mem { data: (0..=255).collect(), ..Default::default() }));
        packet(&mut c, 0x3c, HEADER_REPEAT_START, 1, &[0x20]);
        packet(&mut c, 0x3c, HEADER_READ | HEADER_CONTINUE, 1, &[]);
        assert_eq!(c.read(RX_FIFO as u64), 0x20);
        c.write(FIFO_CONTROL as u64, 3, 0); // as Linux does before the rest
        packet(&mut c, 0x3c, HEADER_READ, 2, &[]);
        assert_eq!(c.read(RX_FIFO as u64), 0x2221);
        // given up after the first byte; then register 0x30 of the same device is read
        packet(&mut c, 0x3c, HEADER_READ | HEADER_CONTINUE, 1, &[]);
        c.read(RX_FIFO as u64);
        packet(&mut c, 0x3c, HEADER_REPEAT_START, 1, &[0x30]);
        packet(&mut c, 0x3c, HEADER_READ, 1, &[]);
        assert_eq!(c.read(RX_FIFO as u64), 0x30);
    }

    #[test]
    fn a_bus_clear_ends_an_abandoned_transfer_to_the_same_device() {
        for by_reset in [false, true] {
            let mut c = TegraI2c::default();
            c.attach(0x3c, Box::new(Mem { data: vec![0; 256], ..Default::default() }));
            packet(&mut c, 0x3c, HEADER_CONTINUE, 2, &[0xaa10]);
            if by_reset { c.reset() } else { c.write(BUS_CLEAR_CNFG as u64, 1, 0) }
            packet(&mut c, 0x3c, 0, 2, &[0x0120]); // register 0x20 = 1
            packet(&mut c, 0x3c, HEADER_REPEAT_START, 1, &[0x20]);
            packet(&mut c, 0x3c, HEADER_READ, 1, &[]);
            assert_eq!(c.read(RX_FIFO as u64), 1, "reset {by_reset}");
        }
    }
}
