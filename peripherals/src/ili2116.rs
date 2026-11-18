//! Ilitek ILI2116 touch controller on the panel board's I²C4 (address 0x41), as the vendor's
//! `ili2116` driver uses it (read from the driver in the vendor's kernel):
//!
//! | register | read | |
//! |---|---|---|
//! | 0x42 | 2 | protocol version |
//! | 0x40 | 4 | firmware version (the vendor's `touch-fw-upd` flashes when its file is newer) |
//! | 0xC0 | 1 | mode, 0x5A when running |
//! | 0x61 | 5 | MCU version |
//! | 0x20 | 10 | panel: maximum X and Y (16-bit little-endian), then channel counts |
//! | 0x10 | 1 | number of touch records waiting |
//! | 0x11 | 5 | the next record: bit 7 of byte 0 is "down", bits 0-6 the slot + 1, then X and Y (16-bit big-endian) |
//!
//! Each register read is a write of the register number and a repeated-start read. The
//! controller pulls its interrupt line (GPIO7_A6, falling edge) low while records wait.

use crate::i2c::{I2cDevice, RegisterPointer};

pub const ADDRESS: u8 = 0x41;
/// Touch coordinates are the LCD's own (800x1280 portrait): the touch layer sits on the
/// panel, which the panel board mounts turned by 270 degrees; the application turns them.
pub const MAX_X: u16 = 799;
pub const MAX_Y: u16 = 1279;
/// Matches the firmware the vendor ships (`/lib/firmware/ilitek/ili2116.fw`, version 0.0.4.0), so
/// its updater finds it up to date.
const FIRMWARE: [u8; 4] = [0, 0, 4, 0];

#[derive(Default)]
pub struct Ili2116 {
    reg: RegisterPointer,
    /// Bytes of the register being read.
    out: Vec<u8>,
    at: usize,
    /// Records reported by the next 0x10/0x11 sequence.
    queue: Vec<[u8; 5]>,
    /// Records the host still has to read in the current report.
    reading: Vec<[u8; 5]>,
    down: bool,
}

/// Touch records in one report: the driver rejects a count above its 10 slots.
const MAX_REPORT: usize = 10;

impl Ili2116 {
    /// A touch on the panel (from the VNC pointer): `down` and the position.
    pub fn touch(&mut self, down: bool, x: u16, y: u16) {
        if !down && !self.down {
            return;
        }
        self.down = down;
        let (x, y) = (x.min(MAX_X), y.min(MAX_Y));
        let mut rec = [if down { 0x81 } else { 0x01 }, 0, 0, 0, 0];
        rec[1..3].copy_from_slice(&x.to_be_bytes());
        rec[3..5].copy_from_slice(&y.to_be_bytes());
        // keep only the newest position of a held touch
        if let Some(last) = self.queue.last_mut() {
            if last[0] == rec[0] && down {
                *last = rec;
                return;
            }
        }
        self.queue.push(rec);
    }

    /// The interrupt line is low (asserted) while records wait.
    pub fn irq_asserted(&self) -> bool {
        !self.queue.is_empty() || !self.reading.is_empty()
    }

    fn select(&mut self, reg: u8) {
        self.at = 0;
        self.out = match reg {
            0x42 => vec![0x02, 0x00],
            0x40 => FIRMWARE.to_vec(),
            0xc0 => vec![0x5a],
            0x61 => vec![0, 0, 0, 0, 0],
            0x20 => {
                let mut v = vec![0u8; 10];
                v[0..2].copy_from_slice(&MAX_X.to_le_bytes());
                v[2..4].copy_from_slice(&MAX_Y.to_le_bytes());
                v[4] = 20;
                v[5] = 12;
                v[6] = 1; // one contact
                v
            }
            0x10 => {
                // Records the host has not read from the last report come first; a report
                // holds at most MAX_REPORT, the rest wait (the line stays asserted).
                let mut all: Vec<[u8; 5]> = self.reading.drain(..).rev().collect();
                all.append(&mut self.queue);
                let n = all.len().min(MAX_REPORT);
                self.queue = all.split_off(n);
                all.reverse();
                self.reading = all;
                vec![n as u8]
            }
            0x11 => self.reading.pop().map(|r| r.to_vec()).unwrap_or_else(|| vec![0; 5]),
            _ => vec![0],
        };
    }
}

impl I2cDevice for Ili2116 {
    fn start(&mut self, read: bool) -> bool {
        self.reg.start(read);
        true
    }

    fn write(&mut self, byte: u8) -> bool {
        if self.reg.written(byte).is_none() {
            let reg = self.reg.pointer;
            self.select(reg);
        }
        true
    }

    fn read(&mut self) -> u8 {
        let b = self.out.get(self.at).copied().unwrap_or(0);
        self.at += 1;
        b
    }

    fn as_any_mut(&mut self) -> Option<&mut dyn std::any::Any> {
        Some(self)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn read_reg(t: &mut Ili2116, reg: u8, n: usize) -> Vec<u8> {
        t.start(false);
        t.write(reg);
        t.start(true);
        let v = (0..n).map(|_| t.read()).collect();
        t.stop();
        v
    }

    #[test]
    fn identification_and_a_touch() {
        let mut t = Ili2116::default();
        assert_eq!(read_reg(&mut t, 0xc0, 1), [0x5a]);
        assert_eq!(read_reg(&mut t, 0x40, 4), FIRMWARE);
        assert!(!t.irq_asserted());
        t.touch(true, 532, 770);
        assert!(t.irq_asserted());
        assert_eq!(read_reg(&mut t, 0x10, 1), [1]);
        assert_eq!(read_reg(&mut t, 0x11, 5), [0x81, 0x02, 0x14, 0x03, 0x02]);
        assert!(!t.irq_asserted());
        t.touch(false, 532, 770);
        assert_eq!(read_reg(&mut t, 0x10, 1), [1]);
        assert_eq!(read_reg(&mut t, 0x11, 5)[0], 0x01);
    }

    /// Many records are reported ten at a time, and records the host skipped are reported
    /// again first rather than lost.
    #[test]
    fn reports_are_bounded_and_lossless() {
        let mut t = Ili2116::default();
        for k in 0..15u16 {
            t.touch(true, k, 0);
            t.touch(false, k, 0);
        }
        assert_eq!(read_reg(&mut t, 0x10, 1), [10]);
        assert_eq!(read_reg(&mut t, 0x11, 5), [0x81, 0, 0, 0, 0]);
        // the host reads only one record, then asks again: 9 unread + 20 queued
        assert_eq!(read_reg(&mut t, 0x10, 1), [10]);
        assert_eq!(read_reg(&mut t, 0x11, 5), [0x01, 0, 0, 0, 0]);
        for _ in 0..9 {
            read_reg(&mut t, 0x11, 5);
        }
        assert!(t.irq_asserted());
        assert_eq!(read_reg(&mut t, 0x10, 1), [10]);
        for _ in 0..10 {
            read_reg(&mut t, 0x11, 5);
        }
        assert_eq!(read_reg(&mut t, 0x10, 1), [9]);
        for _ in 0..8 {
            read_reg(&mut t, 0x11, 5);
        }
        assert_eq!(read_reg(&mut t, 0x11, 5), [0x01, 0, 14, 0, 0]);
        assert!(!t.irq_asserted());
    }
}
