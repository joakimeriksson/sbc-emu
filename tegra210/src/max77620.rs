//! MAX77620 power-management IC (I²C 0x3c): what Linux's MFD, regulator, GPIO and pin
//! drivers use is a file of 8-bit registers. Nothing interrupts, and the rails' voltages only
//! read back. Reset values: the Nano's rails as its boot firmware leaves them.
use crate::i2c::{I2cDevice, RegisterPointer};

pub const ADDRESS: u8 = 0x3c;
pub const RTC_ADDRESS: u8 = 0x68;

const SD0: usize = 0x16;
const SD0_CFG: usize = 0x1d;
const LDO0_CFG: usize = 0x23;
const CID0: usize = 0x58;

pub struct Max77620 {
    pointer: RegisterPointer,
    pub regs: [u8; 256],
}

impl Default for Max77620 {
    fn default() -> Self {
        let mut regs = [0; 256];
        // step-down rails SD0-SD3: 1.0 V core, 1.1 V DDR, 1.35 V, 1.8 V (600 mV + 12.5 mV steps)
        for (k, mv) in [1000, 1125, 1350, 1800].into_iter().enumerate() {
            regs[SD0 + k] = ((mv - 600) * 10 / 125) as u8;
            regs[SD0_CFG + k] = 0x30; // normal power mode
        }
        // LDO0-LDO8: on (power mode 3 in bits 7:6)
        for k in 0..9 {
            regs[LDO0_CFG + 2 * k] = 0xc0;
        }
        // CID: serial, OTP version, ES revision
        regs[CID0..CID0 + 6].copy_from_slice(&[0x5b, 0xc0, 0x00, 0x01, 0x35, 0x80]);
        Self { pointer: RegisterPointer::default(), regs }
    }
}

impl I2cDevice for Max77620 {
    fn start(&mut self, read: bool) -> bool {
        self.pointer.start(read);
        true
    }
    fn write(&mut self, byte: u8) -> bool {
        if let Some(r) = self.pointer.written(byte) {
            let r = r as usize;
            // the identification and the (never pending) interrupt status are read-only
            if !(CID0..CID0 + 6).contains(&r) && !(0x05..=0x0b).contains(&r) {
                self.regs[r] = byte;
            }
        }
        true
    }
    fn read(&mut self) -> u8 {
        self.regs[self.pointer.next() as usize]
    }
}

const RTC_UPDATE0: usize = 0x04;
const RTC_SEC: usize = 0x07;
const UPDATE_WRITE: u8 = 1 << 0;
const UPDATE_READ: u8 = 1 << 4;

/// The PMIC's real-time clock (I²C 0x68, Linux `rtc-max77686`): UPDATE0 copies the running
/// time to the registers (RBUDR) or sets it from them (UDR). No alarms.
pub struct Rtc {
    pointer: RegisterPointer,
    regs: [u8; 256],
    /// time registers were written since the last update (which then sets the time)
    written: bool,
    /// UTC seconds at virtual time 0
    offset: i64,
    now_ns: u64,
}

impl Rtc {
    pub fn new(unix_seconds: i64) -> Self {
        Self { pointer: RegisterPointer::default(), regs: [0; 256], written: false, offset: unix_seconds, now_ns: 0 }
    }

    fn update(&mut self, bits: u8) {
        let up = (self.now_ns / 1_000_000_000) as i64;
        // an update that only applies the control registers leaves the time alone
        if bits & UPDATE_WRITE != 0 && std::mem::take(&mut self.written) {
            let r = &self.regs[RTC_SEC..];
            let days = days_from_civil(2000 + (r[5] & 0x7f) as i64, (r[4] & 15) as i64, (r[6] & 31) as i64);
            let secs = days * 86400 + (r[2] & 31) as i64 * 3600 + (r[1] & 0x7f) as i64 * 60 + (r[0] & 0x7f) as i64;
            self.offset = secs - up;
        }
        if bits & UPDATE_READ != 0 {
            let t = self.offset + up;
            let (days, s) = (t.div_euclid(86400), t.rem_euclid(86400));
            let (y, m, d) = civil_from_days(days);
            let wday = (days + 4).rem_euclid(7); // 1970-01-01 was a Thursday
            let v = [s % 60, s / 60 % 60, s / 3600, 1 << wday, m, (y - 2000).clamp(0, 99), d];
            for (r, v) in self.regs[RTC_SEC..].iter_mut().zip(v) {
                *r = v as u8;
            }
        }
    }
}

/// Days since 1970-01-01 of a proleptic Gregorian date, and back (H. Hinnant's algorithms).
fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let doy = (153 * ((m + 9) % 12) + 2) / 5 + d - 1;
    era * 146_097 + yoe * 365 + yoe / 4 - yoe / 100 + doy - 719_468
}

fn civil_from_days(z: i64) -> (i64, i64, i64) {
    let z = z + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let (d, m) = (doy - (153 * mp + 2) / 5 + 1, if mp < 10 { mp + 3 } else { mp - 9 });
    (yoe + era * 400 + (m <= 2) as i64, m, d)
}

impl I2cDevice for Rtc {
    fn start(&mut self, read: bool) -> bool {
        self.pointer.start(read);
        true
    }
    fn write(&mut self, byte: u8) -> bool {
        match self.pointer.written(byte).map(usize::from) {
            Some(RTC_UPDATE0) => self.update(byte),
            Some(r) if r > 0 => {
                self.regs[r] = byte;
                self.written |= (RTC_SEC..RTC_SEC + 7).contains(&r);
            }
            _ => {} // the pointer, or the read-only interrupt status
        }
        true
    }
    fn read(&mut self) -> u8 {
        self.regs[self.pointer.next() as usize]
    }
    fn clock(&mut self, now_ns: u64) {
        self.now_ns = now_ns;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn get(r: &mut Rtc, reg: u8, n: usize) -> Vec<u8> {
        r.start(false);
        r.write(reg);
        r.start(true);
        (0..n).map(|_| r.read()).collect()
    }

    #[test]
    fn rtc_reads_and_sets_the_time() {
        // 2026-11-22 13:14:15 UTC, a Sunday, plus two seconds of running
        let mut r = Rtc::new(1_795_353_255);
        r.clock(2_000_000_000);
        for (reg, v) in [(3, 2), (RTC_UPDATE0 as u8, UPDATE_WRITE)] {
            r.start(false); // 24-hour mode, applied: the time stays
            r.write(reg);
            r.write(v);
        }
        r.start(false);
        r.write(RTC_UPDATE0 as u8);
        r.write(UPDATE_READ);
        assert_eq!(get(&mut r, RTC_SEC as u8, 7), [17, 14, 13, 1, 11, 26, 22]);
        for (k, v) in [0, 0, 0, 1 << 4, 2, 28, 29].into_iter().enumerate() {
            r.start(false);
            r.write(RTC_SEC as u8 + k as u8);
            r.write(v);
        }
        r.start(false);
        r.write(RTC_UPDATE0 as u8);
        r.write(UPDATE_WRITE);
        r.clock(2_000_000_000 + 86_400_000_000_000);
        r.start(false);
        r.write(RTC_UPDATE0 as u8);
        r.write(UPDATE_READ);
        assert_eq!(get(&mut r, RTC_SEC as u8 + 4, 3), [3, 28, 1], "the day after 2028-02-29");
    }
}
