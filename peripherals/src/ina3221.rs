//! Texas Instruments INA3221 three-channel shunt and bus voltage monitor on I²C (`ti,ina3221`,
//! Linux `ina3221` hwmon): 16-bit big-endian registers behind a pointer byte. Each channel
//! reports its shunt voltage (40 µV per step, bits 15:3) and bus voltage (8 mV per step,
//! bits 15:3); the critical and warning limits compare against the shunt voltage and raise the
//! flags in Mask/Enable, which also always shows a finished conversion (CVRF).
//!
//! What the chip measures comes from the machine: `set_channel` gives a channel's bus voltage
//! and current, which the shunt resistor (`shunt_uohm`) turns into a shunt voltage.
use crate::i2c::{I2cDevice, RegisterPointer};

const CONFIG: usize = 0x00;
const CRIT1: usize = 0x07;
const WARN1: usize = 0x08;
const SHUNT_SUM: usize = 0x0d;
const CRIT_SUM: usize = 0x0e;
const MASK_ENABLE: usize = 0x0f;
const PV_UPPER: usize = 0x10;
const PV_LOWER: usize = 0x11;
const MANUFACTURER: u8 = 0xfe;
const DIE: u8 = 0xff;

const CONFIG_RESET: u16 = 1 << 15;
const CONFIG_DEFAULT: u16 = 0x7127;
const CVRF: u16 = 1 << 0;
/// summation-control channel enables (SCC1-3)
const SCC_MASK: u16 = 0x7000;

pub struct Ina3221 {
    regs: [u16; 0x12],
    pointer: RegisterPointer,
    /// the first byte of a register write
    high: Option<u8>,
    /// the second byte of a register read comes next
    low_next: bool,
    /// per channel: the shunt resistor (µΩ), bus voltage (mV) and current (mA)
    pub shunt_uohm: [u32; 3],
    bus_mv: [u32; 3],
    current_ma: [u32; 3],
}

impl Ina3221 {
    /// A monitor whose channels sense their current through `shunt_uohm` (µΩ) resistors.
    pub fn new(shunt_uohm: [u32; 3]) -> Self {
        let mut chip = Self {
            regs: [0; 0x12],
            pointer: RegisterPointer::default(),
            high: None,
            low_next: false,
            shunt_uohm,
            bus_mv: [0; 3],
            current_ma: [0; 3],
        };
        chip.reset();
        chip
    }

    fn reset(&mut self) {
        self.regs = [0; 0x12];
        self.regs[CONFIG] = CONFIG_DEFAULT;
        for ch in 0..3 {
            self.regs[CRIT1 + 2 * ch] = 0x7ff8;
            self.regs[WARN1 + 2 * ch] = 0x7ff8;
        }
        self.regs[CRIT_SUM] = 0x7ffe;
        self.regs[MASK_ENABLE] = 0x0002;
        self.regs[PV_UPPER] = 0x2710;
        self.regs[PV_LOWER] = 0x2328;
        self.update();
    }

    /// What channel `ch` (0-2) measures: its bus voltage (mV) and current (mA).
    pub fn set_channel(&mut self, ch: usize, bus_mv: u32, current_ma: u32) {
        self.bus_mv[ch] = bus_mv;
        self.current_ma[ch] = current_ma;
        self.update();
    }

    fn enabled(&self, ch: usize) -> bool {
        self.regs[CONFIG] & (1 << (14 - ch)) != 0
    }

    /// The result registers and alert flags from the inputs.
    fn update(&mut self) {
        let mut sum = 0i32;
        let mut flags = 0u16;
        for ch in 0..3 {
            let (shunt, bus) = if self.enabled(ch) {
                // µV over the shunt, in 40 µV steps; mV on the bus in 8 mV steps
                let uv = u64::from(self.current_ma[ch]) * u64::from(self.shunt_uohm[ch]) / 1000;
                ((uv / 40).min(0xfff) as u16, (self.bus_mv[ch] / 8).min(0xfff) as u16)
            } else {
                (0, 0)
            };
            self.regs[1 + 2 * ch] = shunt << 3;
            self.regs[2 + 2 * ch] = bus << 3;
            if self.regs[MASK_ENABLE] & (1 << (14 - ch)) != 0 {
                sum += i32::from(shunt) << 1; // 40 µV steps in bits 14:1
            }
            // critical (CF1-3: bits 9-7) and warning (WF1-3: bits 5-3) alerts
            if shunt << 3 > self.regs[CRIT1 + 2 * ch] & !7 {
                flags |= 1 << (9 - ch);
            }
            if shunt << 3 > self.regs[WARN1 + 2 * ch] & !7 {
                flags |= 1 << (5 - ch);
            }
        }
        self.regs[SHUNT_SUM] = (sum.min(0x7ffe) as u16) & !1;
        if self.regs[MASK_ENABLE] & SCC_MASK != 0 && self.regs[SHUNT_SUM] > self.regs[CRIT_SUM] & !1 {
            flags |= 1 << 6; // SF
        }
        self.regs[MASK_ENABLE] = (self.regs[MASK_ENABLE] & !0x03f9) | flags | CVRF;
    }

    fn register(&self, reg: u8) -> u16 {
        match reg {
            MANUFACTURER => 0x5449, // "TI"
            DIE => 0x3220,
            r => self.regs.get(r as usize).copied().unwrap_or(0),
        }
    }

    fn store(&mut self, reg: u8, value: u16) {
        let r = reg as usize;
        match r {
            CONFIG if value & CONFIG_RESET != 0 => return self.reset(),
            CONFIG => self.regs[CONFIG] = value,
            // the result registers are read-only
            0x01..=0x06 | SHUNT_SUM => return,
            CRIT1..=0x0c | PV_UPPER | PV_LOWER => self.regs[r] = value & !7,
            CRIT_SUM => self.regs[r] = value & !1,
            // the enables are writable, the flags are the chip's
            MASK_ENABLE => self.regs[r] = (self.regs[r] & 0x03ff) | (value & 0xfc00),
            _ => return,
        }
        self.update();
    }
}

impl I2cDevice for Ina3221 {
    fn start(&mut self, read: bool) -> bool {
        self.pointer.start(read);
        self.high = None;
        self.low_next = false;
        true
    }

    fn write(&mut self, byte: u8) -> bool {
        if self.pointer.written(byte).is_some() {
            match self.high.take() {
                None => self.high = Some(byte),
                Some(high) => {
                    let reg = self.pointer.pointer.wrapping_sub(2);
                    self.store(reg, u16::from(high) << 8 | u16::from(byte));
                    // the pointer stays on the register
                    self.pointer.pointer = reg;
                }
            }
        }
        true
    }

    fn read(&mut self) -> u8 {
        // the pointer does not advance: reads repeat the same register, high byte first
        let value = self.register(self.pointer.pointer);
        self.low_next = !self.low_next;
        if self.low_next { (value >> 8) as u8 } else { value as u8 }
    }

    fn as_any_mut(&mut self) -> Option<&mut dyn std::any::Any> {
        Some(self)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write(chip: &mut Ina3221, reg: u8, value: u16) {
        chip.start(false);
        for b in [reg, (value >> 8) as u8, value as u8] {
            assert!(chip.write(b));
        }
        chip.stop();
    }

    fn read(chip: &mut Ina3221, reg: u8) -> u16 {
        chip.start(false);
        chip.write(reg);
        chip.start(true);
        let v = u16::from(chip.read()) << 8 | u16::from(chip.read());
        chip.stop();
        v
    }

    #[test]
    fn identifies_and_resets_to_the_datasheet_defaults() {
        let mut chip = Ina3221::new([5000; 3]);
        assert_eq!(read(&mut chip, 0xfe), 0x5449);
        assert_eq!(read(&mut chip, 0xff), 0x3220);
        write(&mut chip, 0x00, 0x7000);
        assert_eq!(read(&mut chip, 0x00), 0x7000);
        write(&mut chip, 0x00, 0x8000);
        assert_eq!(read(&mut chip, 0x00), 0x7127);
        assert_eq!(read(&mut chip, 0x0f) & 1, 1, "conversion ready");
    }

    #[test]
    fn measures_bus_voltage_and_current_through_the_shunt() {
        let mut chip = Ina3221::new([5000; 3]);
        chip.set_channel(0, 5016, 1000);
        // 1 A through 5 mΩ: 5 mV = 125 steps of 40 µV; 5016 mV = 627 steps of 8 mV
        assert_eq!(read(&mut chip, 0x01), 125 << 3);
        assert_eq!(read(&mut chip, 0x02), 627 << 3);
        // a disabled channel reads zero
        write(&mut chip, 0x00, 0x3127);
        assert_eq!(read(&mut chip, 0x02), 0);
    }

    #[test]
    fn limits_raise_the_alert_flags() {
        let mut chip = Ina3221::new([5000; 3]);
        chip.set_channel(0, 5000, 2000); // 10 mV = 250 steps
        assert_eq!(read(&mut chip, 0x0f) & (1 << 9 | 1 << 5), 0);
        write(&mut chip, 0x07, 200 << 3);
        write(&mut chip, 0x08, 100 << 3);
        assert_eq!(read(&mut chip, 0x0f) & (1 << 9 | 1 << 5), 1 << 9 | 1 << 5);
    }
}
