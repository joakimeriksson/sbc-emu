//! The JH7110's Synopsys DesignWare I²C controllers (`snps,designware-i2c`) in master
//! mode, as OpenSBI's `dw_i2c` driver uses them (polled): each command written to
//! IC_DATA_CMD runs on the bus at once, so the TX FIFO is always empty, read data lands
//! in the RX FIFO, and a command with STOP raises STOP_DET. A NACK raises TX_ABRT.
//! The Orange Pi RV's X-Powers AXP15060 PMIC sits on I2C5 at 0x36.
use peripherals::i2c::{I2cDevice, RegisterPointer};
use std::collections::VecDeque;

const RAW_TX_ABRT: u32 = 1 << 6;
const RAW_STOP_DET: u32 = 1 << 9;
const CMD_READ: u32 = 1 << 8;
/// IC_TAR's index in `regs`
const TAR: usize = 1;
const CMD_STOP: u32 = 1 << 9;

pub struct DwI2c {
    pub devices: Vec<(u8, Box<dyn I2cDevice>)>,
    regs: [u32; 64],
    rx: VecDeque<u8>,
    raw: u32,
    /// direction of the transaction in progress (`None` = bus idle)
    active: Option<bool>,
    nacked: bool,
}

impl Default for DwI2c {
    fn default() -> Self {
        DwI2c { devices: Vec::new(), regs: [0; 64], rx: VecDeque::new(), raw: 0, active: None, nacked: false }
    }
}

impl DwI2c {
    pub fn attach(&mut self, addr: u8, dev: Box<dyn I2cDevice>) {
        self.devices.push((addr, dev));
    }

    fn target(&mut self) -> Option<&mut Box<dyn I2cDevice>> {
        let tar = (self.regs[TAR] & 0x7f) as u8;
        self.devices.iter_mut().find(|(a, _)| *a == tar).map(|(_, d)| d)
    }

    fn command(&mut self, v: u32) {
        let read = v & CMD_READ != 0;
        if self.active != Some(read) {
            let ok = match self.target() {
                Some(d) => d.start(read),
                None => false,
            };
            self.active = Some(read);
            self.nacked = !ok;
            if !ok {
                self.raw |= RAW_TX_ABRT;
            }
        }
        if !self.nacked {
            if read {
                let b = self.target().map_or(0xff, |d| d.read());
                self.rx.push_back(b);
            } else if !self.target().is_some_and(|d| d.write(v as u8)) {
                self.nacked = true;
                self.raw |= RAW_TX_ABRT;
            }
        }
        if v & CMD_STOP != 0 {
            if let Some(d) = self.target() {
                d.stop();
            }
            self.active = None;
            self.nacked = false;
            self.raw |= RAW_STOP_DET;
        }
    }

    pub fn read(&mut self, off: u64) -> u32 {
        match off {
            0x10 => self.rx.pop_front().unwrap_or(0) as u32,
            0x2c => self.raw & self.regs[0x30 / 4],
            0x34 => self.raw,
            0x40 => {
                self.raw = 0;
                0
            }
            0x54 => {
                self.raw &= !RAW_TX_ABRT;
                0
            }
            0x60 => {
                self.raw &= !RAW_STOP_DET;
                0
            }
            0x44..=0x68 => 0,
            // STATUS: TX FIFO not full and empty; RX FIFO not empty
            0x70 => 0b110 | ((!self.rx.is_empty() as u32) << 3),
            0x74 => 0,
            0x78 => self.rx.len() as u32,
            0x80 => 0,
            0x9c => self.regs[0x6c / 4] & 1,
            // 8-deep FIFOs, 32-bit APB, fast/high-speed capable
            0xf4 => (7 << 16) | (7 << 8) | (1 << 7) | (1 << 4) | (2 << 2) | 2,
            0xf8 => 0x3230_312a,
            0xfc => 0x4457_0140,
            _ => self.regs.get((off / 4) as usize).copied().unwrap_or(0),
        }
    }

    pub fn write(&mut self, off: u64, v: u32) {
        match off {
            0x10 => self.command(v),
            0x6c => {
                if v & 1 == 0 {
                    self.active = None;
                    self.rx.clear();
                }
                self.regs[0x6c / 4] = v;
            }
            _ => {
                if let Some(r) = self.regs.get_mut((off / 4) as usize) {
                    *r = v;
                }
            }
        }
    }

    /// The device at `addr` as its concrete type.
    pub fn device<T: 'static>(&mut self, addr: u8) -> Option<&mut T> {
        self.devices
            .iter_mut()
            .find(|(a, _)| *a == addr)
            .and_then(|(_, d)| d.as_any_mut())
            .and_then(|a| a.downcast_mut::<T>())
    }
}

/// What the PMIC was asked to do through its power register.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PowerRequest {
    None,
    Reset,
    Off,
}

/// X-Powers AXP15060 PMIC: a register file; register 0x32 bit 7 powers the board off and
/// bit 6 (with bit 7, as OpenSBI writes it) resets it.
pub struct Axp15060 {
    regs: [u8; 256],
    ptr: RegisterPointer,
    pub request: PowerRequest,
}

impl Default for Axp15060 {
    fn default() -> Self {
        let mut regs = [0u8; 256];
        regs[0x03] = 0x50; // chip ID
        Axp15060 { regs, ptr: RegisterPointer::default(), request: PowerRequest::None }
    }
}

impl I2cDevice for Axp15060 {
    fn start(&mut self, read: bool) -> bool {
        self.ptr.start(read);
        true
    }
    fn write(&mut self, byte: u8) -> bool {
        if let Some(reg) = self.ptr.written(byte) {
            self.regs[reg as usize] = byte;
            if reg == 0x32 {
                if byte & 0x40 != 0 {
                    self.request = PowerRequest::Reset;
                } else if byte & 0x80 != 0 {
                    self.request = PowerRequest::Off;
                }
            }
        }
        true
    }
    fn read(&mut self) -> u8 {
        let reg = self.ptr.next();
        self.regs[reg as usize]
    }
    fn as_any_mut(&mut self) -> Option<&mut dyn std::any::Any> {
        Some(self)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn opensbi_style_pmic_reset_over_dw_i2c() {
        let mut bus = DwI2c::default();
        bus.attach(0x36, Box::new(Axp15060::default()));
        // read register 0x32 (dw_i2c_adapter_read)
        bus.write(0x6c, 0);
        bus.write(0x04, 0x36);
        bus.write(0x6c, 1);
        assert_ne!(bus.read(0x70) & 4, 0);
        bus.write(0x10, 0x32);
        bus.write(0x10, CMD_READ | CMD_STOP);
        assert_ne!(bus.read(0x70) & 8, 0);
        let v = bus.read(0x10);
        assert_eq!(v, 0);
        // write it back with power-off and reset bits
        bus.write(0x10, 0x32);
        bus.write(0x10, 0xc0 | CMD_STOP);
        assert_ne!(bus.read(0x34) & RAW_STOP_DET, 0);
        assert_eq!(bus.device::<Axp15060>(0x36).unwrap().request, PowerRequest::Reset);
        // nobody at 0x20: TX_ABRT
        bus.write(0x04, 0x20);
        bus.write(0x10, 0x00 | CMD_STOP);
        assert_ne!(bus.read(0x34) & RAW_TX_ABRT, 0);
    }
}
