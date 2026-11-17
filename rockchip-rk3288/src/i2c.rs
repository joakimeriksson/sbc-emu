//! Rockchip rk3x I²C controller (`rockchip,rk3288-i2c`, Linux `i2c-rk3x`) with a bus of
//! devices addressed by their 7-bit address. The controller is the "v0" Rockchip design:
//! the host writes up to 32 bytes into TXDATA and their count into MTXCNT (TX mode), or a
//! count into MRXCNT and reads RXDATA (RX modes; REGISTER_TX first sends the address in
//! MRXADDR and the register bytes in MRXRADDR, then a repeated start for the read). Each
//! step completes at once and raises its interrupt (START, MBTF, MBRF, STOP; NAKRCV when
//! no device answers the address).

pub use peripherals::i2c::{I2cDevice, RegisterPointer};

pub const SIZE: u64 = 0x1000;

const CON: usize = 0x00;
const CLKDIV: usize = 0x04;
const MRXADDR: usize = 0x08;
const MRXRADDR: usize = 0x0c;
const MTXCNT: usize = 0x10;
const MRXCNT: usize = 0x14;
const IEN: usize = 0x18;
const IPD: usize = 0x1c;
const FCNT: usize = 0x20;
const TXDATA: usize = 0x100;
const RXDATA: usize = 0x200;

// CON
const CON_EN: u32 = 1 << 0;
const CON_START: u32 = 1 << 3;
const CON_STOP: u32 = 1 << 4;
const CON_ACTACK: u32 = 1 << 6;
const MODE_TX: u32 = 0;
const MODE_REGISTER_TX: u32 = 1;
// IPD / IEN
const INT_BTF: u32 = 1 << 0;
const INT_BRF: u32 = 1 << 1;
const INT_MBTF: u32 = 1 << 2;
const INT_MBRF: u32 = 1 << 3;
const INT_START: u32 = 1 << 4;
const INT_STOP: u32 = 1 << 5;
const INT_NAKRCV: u32 = 1 << 6;

/// One rk3x controller and the devices on its bus.
pub struct Rk3xI2c {
    con: u32,
    clkdiv: u32,
    mrxaddr: u32,
    mrxraddr: u32,
    mtxcnt: u32,
    mrxcnt: u32,
    ien: u32,
    ipd: u32,
    fcnt: u32,
    tx: [u32; 8],
    rx: [u32; 8],
    /// The device addressed in the current transaction (index into `devices`).
    current: Option<usize>,
    /// The address byte of the current message has been sent.
    addressed: bool,
    devices: Vec<(u8, Box<dyn I2cDevice>)>,
    /// Virtual time in nanoseconds, set by the machine before each access.
    pub now_ns: u64,
}

impl Default for Rk3xI2c {
    fn default() -> Self {
        Self {
            con: 0,
            clkdiv: 0x0006_0006,
            mrxaddr: 0,
            mrxraddr: 0,
            mtxcnt: 0,
            mrxcnt: 0,
            ien: 0,
            ipd: 0,
            fcnt: 0,
            tx: [0; 8],
            rx: [0; 8],
            current: None,
            addressed: false,
            devices: Vec::new(),
            now_ns: 0,
        }
    }
}

impl Rk3xI2c {
    /// Attach a device at 7-bit address `address`.
    pub fn attach(&mut self, address: u8, device: Box<dyn I2cDevice>) {
        self.devices.push((address & 0x7f, device));
    }

    /// The device of type `T` at `address`, for the machine to inspect.
    pub fn device_mut<T: 'static>(&mut self, address: u8) -> Option<&mut T> {
        let (_, device) = self.devices.iter_mut().find(|(a, _)| *a == address)?;
        device.as_any_mut()?.downcast_mut::<T>()
    }

    pub fn irq(&self) -> bool {
        self.ipd & self.ien != 0
    }

    pub fn read(&mut self, offset: u64) -> u32 {
        let offset = offset as usize;
        match offset {
            CON => self.con,
            CLKDIV => self.clkdiv,
            MRXADDR => self.mrxaddr,
            MRXRADDR => self.mrxraddr,
            MTXCNT => self.mtxcnt,
            MRXCNT => self.mrxcnt,
            IEN => self.ien,
            IPD => self.ipd,
            FCNT => self.fcnt,
            TXDATA..=0x11c => self.tx[(offset - TXDATA) / 4],
            RXDATA..=0x21c => self.rx[(offset - RXDATA) / 4],
            _ => 0,
        }
    }

    pub fn write(&mut self, offset: u64, value: u32) {
        let offset = offset as usize;
        match offset {
            CON => self.write_con(value),
            CLKDIV => self.clkdiv = value,
            MRXADDR => self.mrxaddr = value,
            MRXRADDR => self.mrxraddr = value,
            MTXCNT => {
                self.mtxcnt = value & 0x3f;
                self.transmit();
            }
            MRXCNT => {
                self.mrxcnt = value & 0x3f;
                self.receive();
            }
            IEN => self.ien = value & 0x7f,
            IPD => self.ipd &= !value,
            TXDATA..=0x11c => self.tx[(offset - TXDATA) / 4] = value,
            _ => {}
        }
    }

    fn mode(&self) -> u32 {
        (self.con >> 1) & 3
    }

    fn write_con(&mut self, value: u32) {
        let old = self.con;
        self.con = value;
        if value & CON_EN == 0 {
            // Disabled: the controller's state machine is reset.
            self.end_transaction();
            return;
        }
        if value & CON_START != 0 && old & CON_START == 0 {
            // A START (or the emulated repeated start): a new message begins.
            self.addressed = false;
            self.ipd |= INT_START;
        }
        if value & CON_STOP != 0 && old & CON_STOP == 0 {
            self.end_transaction();
            self.ipd |= INT_STOP;
        }
    }

    fn end_transaction(&mut self) {
        if let Some(index) = self.current.take() {
            self.devices[index].1.stop();
        }
        self.addressed = false;
    }

    fn nak(&mut self) {
        if self.con & CON_ACTACK != 0 {
            self.ipd |= INT_NAKRCV;
        }
    }

    /// Address `byte >> 1` for a read (bit 0 set) or a write.
    fn address(&mut self, byte: u8) -> bool {
        let address = byte >> 1;
        let read = byte & 1 != 0;
        let Some(index) = self.devices.iter().position(|(a, _)| *a == address) else {
            self.current = None;
            return false;
        };
        self.current = Some(index);
        let device = &mut self.devices[index].1;
        device.clock(self.now_ns);
        device.start(read)
    }

    fn transmit(&mut self) {
        let count = (self.mtxcnt as usize).min(32);
        if self.mode() != MODE_TX || count == 0 {
            return;
        }
        let bytes: Vec<u8> = (0..count).map(|i| (self.tx[i / 4] >> (8 * (i % 4))) as u8).collect();
        let mut data = &bytes[..];
        if !self.addressed {
            self.addressed = true;
            if !self.address(data[0]) {
                self.nak();
                return;
            }
            data = &data[1..];
        }
        for &byte in data {
            let ok = match self.current {
                Some(index) => self.devices[index].1.write(byte),
                None => false,
            };
            if !ok {
                self.nak();
                return;
            }
        }
        self.fcnt = count as u32;
        self.ipd |= INT_MBTF | INT_BTF;
    }

    fn receive(&mut self) {
        let count = (self.mrxcnt as usize).min(32);
        if self.mode() == MODE_TX {
            return;
        }
        if !self.addressed {
            self.addressed = true;
            let address = self.mrxaddr as u8;
            if self.mode() == MODE_REGISTER_TX && self.mrxraddr & 0x0700_0000 != 0 {
                // Write phase: address (write), register bytes, then a repeated start.
                if !self.address(address & !1) {
                    self.nak();
                    return;
                }
                for i in 0..3 {
                    if self.mrxraddr & (1 << (24 + i)) != 0 {
                        let byte = (self.mrxraddr >> (8 * i)) as u8;
                        let index = self.current.expect("addressed");
                        if !self.devices[index].1.write(byte) {
                            self.nak();
                            return;
                        }
                    }
                }
            }
            if !self.address(address | 1) {
                self.nak();
                return;
            }
        }
        let Some(index) = self.current else {
            self.nak();
            return;
        };
        self.rx = [0; 8];
        for i in 0..count {
            let byte = self.devices[index].1.read();
            self.rx[i / 4] |= u32::from(byte) << (8 * (i % 4));
        }
        self.fcnt = count as u32;
        self.ipd |= INT_MBRF | INT_BRF;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A register file: the first byte written selects the register, later bytes write
    /// it and reads continue from it.
    #[derive(Default)]
    struct Regs {
        regs: Vec<u8>,
        pointer: u8,
        selected: bool,
    }

    impl I2cDevice for Regs {
        fn start(&mut self, _read: bool) -> bool {
            self.selected = false;
            true
        }
        fn write(&mut self, byte: u8) -> bool {
            if self.selected {
                self.regs[self.pointer as usize] = byte;
                self.pointer = self.pointer.wrapping_add(1);
            } else {
                self.pointer = byte;
                self.selected = true;
            }
            true
        }
        fn read(&mut self) -> u8 {
            let value = self.regs[self.pointer as usize];
            self.pointer = self.pointer.wrapping_add(1);
            value
        }
    }

    fn con(mode: u32, bits: u32) -> u32 {
        CON_EN | mode << 1 | CON_ACTACK | bits
    }

    #[test]
    fn write_then_combined_register_read_as_linux_does() {
        let mut i2c = Rk3xI2c::default();
        i2c.attach(0x1b, Box::new(Regs { regs: vec![0; 256], ..Default::default() }));
        i2c.write(IEN as u64, 0x7f);
        // Write 0x55, 0xaa to registers 0x10, 0x11.
        i2c.write(CON as u64, con(MODE_TX, CON_START));
        assert!(i2c.irq() && i2c.read(IPD as u64) & INT_START != 0);
        i2c.write(IPD as u64, INT_START);
        i2c.write(CON as u64, con(MODE_TX, 0));
        i2c.write(TXDATA as u64, 0x1b << 1 | 0x10 << 8 | 0x55 << 16 | 0xaa << 24);
        i2c.write(MTXCNT as u64, 4);
        assert_eq!(i2c.read(IPD as u64) & (INT_MBTF | INT_NAKRCV), INT_MBTF);
        i2c.write(IPD as u64, 0x7f);
        i2c.write(CON as u64, con(MODE_TX, CON_STOP));
        assert_ne!(i2c.read(IPD as u64) & INT_STOP, 0);
        i2c.write(IPD as u64, 0x7f);
        // Combined: register 0x10, read two bytes.
        i2c.write(MRXADDR as u64, 0x1b << 1 | 1 << 24);
        i2c.write(MRXRADDR as u64, 0x10 | 1 << 24);
        i2c.write(CON as u64, con(MODE_REGISTER_TX, CON_START));
        i2c.write(CON as u64, con(MODE_REGISTER_TX, 0));
        i2c.write(MRXCNT as u64, 2);
        assert_ne!(i2c.read(IPD as u64) & INT_MBRF, 0);
        assert_eq!(i2c.read(RXDATA as u64) & 0xffff, 0xaa55);
    }

    #[test]
    fn absent_device_naks() {
        let mut i2c = Rk3xI2c::default();
        i2c.write(IEN as u64, 0x7f);
        i2c.write(CON as u64, con(MODE_TX, CON_START));
        i2c.write(CON as u64, con(MODE_TX, 0));
        i2c.write(TXDATA as u64, 0x50 << 1);
        i2c.write(MTXCNT as u64, 1);
        assert_ne!(i2c.read(IPD as u64) & INT_NAKRCV, 0);
        assert_eq!(i2c.read(IPD as u64) & INT_MBTF, 0);
    }
}
