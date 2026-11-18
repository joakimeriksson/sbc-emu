//! The JH7110's Synopsys DesignWare I²C controllers (`snps,designware-i2c`) in master
//! mode, as OpenSBI's `dw_i2c` driver (polled) and Linux's `i2c-designware` (interrupts)
//! use them: each command written to IC_DATA_CMD runs on the bus at once, so the TX FIFO
//! is always empty (TX_EMPTY), read data lands in the RX FIFO (RX_FULL above RX_TL), and
//! a command with STOP raises STOP_DET. A NACK raises TX_ABRT (with its source in
//! IC_TX_ABRT_SOURCE) and drops the rest of the transfer until the abort is cleared.
//! The Orange Pi RV's X-Powers AXP15060 PMIC sits on I2C5 at 0x36.
use peripherals::i2c::{I2cDevice, RegisterPointer};
use std::collections::VecDeque;

const RAW_RX_FULL: u32 = 1 << 2;
const RAW_TX_EMPTY: u32 = 1 << 4;
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
    /// TX_ABRT pending: commands are dropped until it is cleared
    aborted: bool,
    abort_source: u32,
}

impl Default for DwI2c {
    fn default() -> Self {
        DwI2c { devices: Vec::new(), regs: [0; 64], rx: VecDeque::new(), raw: 0, active: None, nacked: false, aborted: false, abort_source: 0 }
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

    fn abort(&mut self, source: u32) {
        if let Some(d) = self.target() {
            d.stop();
        }
        self.raw |= RAW_TX_ABRT | RAW_STOP_DET;
        self.abort_source |= source;
        self.aborted = true;
        self.active = None;
        self.nacked = false;
    }

    fn command(&mut self, v: u32) {
        if self.aborted || self.regs[0x6c / 4] & 1 == 0 {
            return;
        }
        let read = v & CMD_READ != 0;
        if self.active != Some(read) {
            let ok = match self.target() {
                Some(d) => d.start(read),
                None => false,
            };
            self.active = Some(read);
            if !ok {
                self.abort(1); // ABRT_7B_ADDR_NOACK
                return;
            }
        }
        if read {
            let b = self.target().map_or(0xff, |d| d.read());
            self.rx.push_back(b);
        } else if !self.target().is_some_and(|d| d.write(v as u8)) {
            self.abort(1 << 3); // ABRT_TXDATA_NOACK
            return;
        }
        if v & CMD_STOP != 0 {
            if let Some(d) = self.target() {
                d.stop();
            }
            self.active = None;
            self.raw |= RAW_STOP_DET;
        }
    }

    /// Raw interrupt status, with the FIFO-level bits.
    fn raw_status(&self) -> u32 {
        let mut r = self.raw;
        if self.regs[0x6c / 4] & 1 != 0 {
            r |= RAW_TX_EMPTY;
        }
        if self.rx.len() as u32 > self.regs[0x38 / 4] {
            r |= RAW_RX_FULL;
        }
        r
    }

    /// The controller's interrupt line.
    pub fn irq(&self) -> bool {
        self.raw_status() & self.regs[0x30 / 4] != 0
    }

    fn clear_abort(&mut self) {
        self.raw &= !RAW_TX_ABRT;
        self.aborted = false;
        self.abort_source = 0;
    }

    pub fn read(&mut self, off: u64) -> u32 {
        match off {
            0x10 => self.rx.pop_front().unwrap_or(0) as u32,
            0x2c => self.raw_status() & self.regs[0x30 / 4],
            0x34 => self.raw_status(),
            0x40 => {
                self.raw = 0;
                self.clear_abort();
                0
            }
            0x54 => {
                self.clear_abort();
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
            0x80 => self.abort_source,
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
                    self.raw = 0;
                    self.aborted = false;
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

/// A 24C04 serial EEPROM (512 bytes as two 256-byte blocks at consecutive addresses):
/// the board ID EEPROM at 0x50 on I2C5. `half` selects the block this address serves.
pub struct At24c04 {
    pub data: std::sync::Arc<std::sync::Mutex<[u8; 512]>>,
    half: usize,
    ptr: RegisterPointer,
}

impl At24c04 {
    /// The two bus devices (0x50 and 0x51) of one EEPROM.
    pub fn pair(contents: [u8; 512]) -> (At24c04, At24c04) {
        let data = std::sync::Arc::new(std::sync::Mutex::new(contents));
        (
            At24c04 { data: data.clone(), half: 0, ptr: RegisterPointer::default() },
            At24c04 { data, half: 256, ptr: RegisterPointer::default() },
        )
    }
}

impl I2cDevice for At24c04 {
    fn start(&mut self, read: bool) -> bool {
        self.ptr.start(read);
        true
    }
    fn write(&mut self, byte: u8) -> bool {
        if let Some(reg) = self.ptr.written(byte) {
            self.data.lock().unwrap()[self.half + reg as usize] = byte;
        }
        true
    }
    fn read(&mut self) -> u8 {
        let reg = self.ptr.next();
        self.data.lock().unwrap()[self.half + reg as usize]
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
