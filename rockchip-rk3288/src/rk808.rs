//! Rockchip RK808 PMIC on I²C (`rockchip,rk808`, Linux `rk8xx-i2c`, `rk808-regulator`,
//! `rtc-rk808`, `clk-rk808`): a 256-byte register file that reads back what Linux writes
//! (regulator enables and voltage selectors, interrupt masks, clock output), with
//! U-Boot-like regulator settings at power-on, a running real-time clock and the
//! power-off bit of the device control register (`rockchip,system-power-controller`).
//!
//! The RTC counts in the RK808's own calendar, which gives November 31 days; Linux
//! converts to and from the Gregorian calendar with 2016-01-01 as the common day
//! (`rockchip_to_gregorian`), so the model keeps the hardware's view and lets Linux do
//! that conversion.
use crate::i2c::I2cDevice;

/// I²C address on the Tinker Board (`pmic@1b`).
pub const ADDRESS: u8 = 0x1b;

const SECONDS: usize = 0x00;
const WEEKS: usize = 0x06;
const RTC_CTRL: usize = 0x10;
const RTC_STATUS: usize = 0x11;
const DCDC_EN: usize = 0x23;
const LDO_EN: usize = 0x24;
const DEVCTRL: usize = 0x4b;
const INT_STS1: usize = 0x4c;
const INT_STS2: usize = 0x4e;
// RTC_CTRL
const STOP_RTC: u8 = 1 << 0;
const GET_TIME: u8 = 1 << 6;
// DEVCTRL
const DEV_OFF: u8 = 1 << 0;
const DEV_OFF_RST: u8 = 1 << 3;

pub struct Rk808 {
    regs: [u8; 256],
    /// Register pointer (the first byte of a write transaction selects it).
    pointer: u8,
    /// The next written byte is the register address.
    expect_pointer: bool,
    /// RTC: seconds since 2000-01-01 00:00:00 in the RK808 calendar at `rtc_epoch_ns` of
    /// virtual time.
    rtc_seconds: i64,
    rtc_epoch_ns: u64,
    /// Virtual time of the current access.
    now_ns: u64,
    /// Linux wrote DEV_OFF or DEV_OFF_RST: the board turns off.
    pub power_off: bool,
}

impl Rk808 {
    /// A PMIC whose RTC shows `unix_seconds` (UTC) at virtual time 0.
    pub fn new(unix_seconds: i64) -> Self {
        let mut regs = [0u8; 256];
        // As U-Boot leaves the Tinker Board: every regulator on, at its board voltage.
        regs[DCDC_EN] = 0x6f; // BUCK1-4 and both switches
        regs[LDO_EN] = 0xff;
        for (reg, value) in [
            (0x2f, 31), // BUCK1 vdd_arm 1.1 V (0.7125 V + 12.5 mV steps)
            (0x33, 31), // BUCK2 vdd_gpu 1.1 V
            (0x38, 15), // BUCK4 vcc_io 3.3 V (1.8 V + 100 mV steps)
            (0x3b, 0),  // LDO1 1.8 V
            (0x3d, 15), // LDO2 3.3 V
            (0x3f, 2),  // LDO3 1.0 V (0.8 V + 100 mV steps)
            (0x41, 0),  // LDO4 1.8 V
            (0x43, 15), // LDO5 vccio_sd 3.3 V
            (0x45, 2),  // LDO6 1.0 V
            (0x47, 10), // LDO7 1.8 V
            (0x49, 0),  // LDO8 1.8 V
        ] {
            regs[reg] = value;
        }
        regs[0x17] = 0x00; // ID_MSB / ID_LSB: RK808 (0x0000)
        regs[0x18] = 0x00;
        regs[0x4d] = 0xff; // interrupts masked at power-on
        regs[0x4f] = 0xff;
        let mut pmic = Rk808 {
            regs,
            pointer: 0,
            expect_pointer: false,
            rtc_seconds: rockchip_seconds_for_unix(unix_seconds),
            rtc_epoch_ns: 0,
            now_ns: 0,
            power_off: false,
        };
        pmic.latch_time();
        pmic
    }

    /// The RTC's current value (RK808 calendar seconds since 2000).
    fn rtc_now(&self) -> i64 {
        if self.regs[RTC_CTRL] & STOP_RTC != 0 {
            return self.rtc_seconds;
        }
        self.rtc_seconds + (self.now_ns.saturating_sub(self.rtc_epoch_ns) / 1_000_000_000) as i64
    }

    /// Copy the running time into the shadow registers Linux reads.
    fn latch_time(&mut self) {
        let fields = rockchip_fields(self.rtc_now());
        self.regs[SECONDS..=WEEKS].copy_from_slice(&fields.map(bcd));
    }

    fn write_reg(&mut self, reg: usize, value: u8) {
        let old = self.regs[reg];
        match reg {
            RTC_CTRL => {
                if value & STOP_RTC != 0 && old & STOP_RTC == 0 {
                    // Stopped: freeze the counter.
                    self.rtc_seconds = self.rtc_now();
                    self.regs[reg] = value;
                } else if value & STOP_RTC == 0 && old & STOP_RTC != 0 {
                    // Started again: the time registers hold the value set while stopped.
                    let fields = core::array::from_fn(|i| unbcd(self.regs[SECONDS + i]));
                    self.rtc_seconds = rockchip_seconds(fields);
                    self.rtc_epoch_ns = self.now_ns;
                    self.regs[reg] = value;
                } else {
                    self.regs[reg] = value;
                }
                if value & GET_TIME != 0 && old & GET_TIME == 0 {
                    self.latch_time();
                }
            }
            RTC_STATUS | INT_STS1 | INT_STS2 => self.regs[reg] &= !value,
            DEVCTRL => {
                self.regs[reg] = value;
                if value & (DEV_OFF | DEV_OFF_RST) != 0 {
                    self.power_off = true;
                }
            }
            _ => self.regs[reg] = value,
        }
    }
}

impl I2cDevice for Rk808 {
    fn start(&mut self, read: bool) -> bool {
        self.expect_pointer = !read;
        true
    }

    fn write(&mut self, byte: u8) -> bool {
        if self.expect_pointer {
            self.pointer = byte;
            self.expect_pointer = false;
        } else {
            self.write_reg(self.pointer as usize, byte);
            self.pointer = self.pointer.wrapping_add(1);
        }
        true
    }

    fn read(&mut self) -> u8 {
        let value = self.regs[self.pointer as usize];
        self.pointer = self.pointer.wrapping_add(1);
        value
    }

    fn clock(&mut self, now_ns: u64) {
        self.now_ns = now_ns;
    }

    fn as_any_mut(&mut self) -> Option<&mut dyn std::any::Any> {
        Some(self)
    }
}

pub(crate) fn bcd(value: u8) -> u8 {
    value / 10 << 4 | value % 10
}

pub(crate) fn unbcd(value: u8) -> u8 {
    (value >> 4) * 10 + (value & 0xf)
}

fn leap(year: i64) -> bool {
    year % 4 == 0 && (year % 100 != 0 || year % 400 == 0)
}

/// Days in `month` (1-12); `november` is 30 (Gregorian) or 31 (RK808).
fn month_days(year: i64, month: i64, november: i64) -> i64 {
    match month {
        2 if leap(year) => 29,
        2 => 28,
        4 | 6 | 9 => 30,
        11 => november,
        _ => 31,
    }
}

/// Seconds since 2000-01-01 of a date in a calendar whose November has `november` days.
pub(crate) fn seconds_of(year: i64, month: i64, day: i64, hms: i64, november: i64) -> i64 {
    let mut days = 0;
    if year >= 2000 {
        for y in 2000..year {
            days += (1..=12).map(|m| month_days(y, m, november)).sum::<i64>();
        }
    } else {
        for y in year..2000 {
            days -= (1..=12).map(|m| month_days(y, m, november)).sum::<i64>();
        }
    }
    days += (1..month).map(|m| month_days(year, m, november)).sum::<i64>();
    (days + day - 1) * 86400 + hms
}

/// (year, month, day, seconds of day) of `seconds` since 2000-01-01.
pub(crate) fn date_of(seconds: i64, november: i64) -> (i64, i64, i64, i64) {
    let mut days = seconds.div_euclid(86400);
    let hms = seconds.rem_euclid(86400);
    let mut year = 2000;
    loop {
        let length: i64 = (1..=12).map(|m| month_days(year, m, november)).sum();
        if days < 0 {
            year -= 1;
            days += (1..=12).map(|m| month_days(year, m, november)).sum::<i64>();
        } else if days >= length {
            days -= length;
            year += 1;
        } else {
            break;
        }
    }
    let mut month = 1;
    while days >= month_days(year, month, november) {
        days -= month_days(year, month, november);
        month += 1;
    }
    (year, month, days + 1, hms)
}

/// Shadow register values (seconds, minutes, hours, day, month, year - 2000, weekday) of
/// an RK808 calendar time.
fn rockchip_fields(seconds: i64) -> [u8; 7] {
    let (year, month, day, hms) = date_of(seconds, 31);
    let weekday = (seconds.div_euclid(86400) + 6).rem_euclid(7); // 2000-01-01 was a Saturday
    [
        (hms % 60) as u8,
        (hms / 60 % 60) as u8,
        (hms / 3600) as u8,
        day as u8,
        month as u8,
        (year - 2000).clamp(0, 99) as u8,
        weekday as u8,
    ]
}

fn rockchip_seconds(fields: [u8; 7]) -> i64 {
    let [s, min, h, day, month, year, _] = fields.map(i64::from);
    seconds_of(2000 + year, month.clamp(1, 12), day.max(1), h * 3600 + min * 60 + s, 31)
}

/// Linux's `nov2dec_transitions` for a date.
fn transitions(year: i64, month: i64) -> i64 {
    year - 2016 + i64::from(month > 11)
}

/// What the RK808 shows for a Unix time once Linux's `gregorian_to_rockchip` has set it.
fn rockchip_seconds_for_unix(unix: i64) -> i64 {
    const UNIX_2000: i64 = 946_684_800;
    let t = unix - UNIX_2000; // Gregorian seconds since 2000
    let (year, month, ..) = date_of(t, 30);
    let extra = transitions(year, month);
    let (mut y, mut m, mut d, hms) = date_of(t - extra * 86400, 30);
    if transitions(y, m) < extra {
        if m == 11 {
            d += 1; // November 31st
        } else {
            (y, m, d, _) = date_of(t - (extra - 1) * 86400, 30);
        }
    }
    seconds_of(y, m, d, hms, 31)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Linux's `rockchip_to_gregorian`: the Unix time Linux reads from shadow registers.
    fn linux_reads(fields: [u8; 7]) -> i64 {
        let [s, min, h, day, month, year, _] = fields.map(i64::from);
        // rtc_tm_to_time64 treats the date as Gregorian (Nov 31 counts as Dec 1).
        let gregorian = seconds_of(2000 + year, month, 1, h * 3600 + min * 60 + s, 30) + (day - 1) * 86400;
        gregorian + 946_684_800 + transitions(2000 + year, month) * 86400
    }

    fn read_time(pmic: &mut Rk808) -> [u8; 7] {
        pmic.start(false);
        pmic.write(RTC_CTRL as u8);
        pmic.write(GET_TIME);
        pmic.start(false);
        pmic.write(RTC_CTRL as u8);
        pmic.write(0);
        pmic.start(false);
        pmic.write(SECONDS as u8);
        pmic.start(true);
        core::array::from_fn(|_| unbcd(pmic.read()))
    }

    #[test]
    fn rtc_reads_back_the_host_time_through_linuxs_calendar_conversion() {
        for unix in [1_763_337_600, 1_480_464_000, 1_480_550_400, 1_451_606_400, 1_798_675_199] {
            let mut pmic = Rk808::new(unix);
            assert_eq!(linux_reads(read_time(&mut pmic)), unix, "unix {unix}");
            pmic.clock(5_500_000_000);
            assert_eq!(linux_reads(read_time(&mut pmic)), unix + 5);
        }
    }

    #[test]
    fn rtc_set_while_stopped_then_runs() {
        let mut pmic = Rk808::new(0);
        pmic.clock(1_000_000_000);
        pmic.start(false);
        pmic.write(RTC_CTRL as u8);
        pmic.write(STOP_RTC);
        pmic.start(false);
        pmic.write(SECONDS as u8);
        for value in [0x30, 0x15, 0x12, 0x31, 0x11, 0x26, 0x01] {
            pmic.write(value); // 2026-11-31 12:15:30 in the RK808 calendar
        }
        pmic.start(false);
        pmic.write(RTC_CTRL as u8);
        pmic.write(0);
        pmic.clock(3_000_000_000);
        assert_eq!(read_time(&mut pmic)[..6], [32, 15, 12, 31, 11, 26]);
    }

    #[test]
    fn regulators_read_back_and_power_off() {
        let mut pmic = Rk808::new(0);
        pmic.start(false);
        pmic.write(0x43);
        pmic.start(true);
        assert_eq!(pmic.read(), 15, "LDO5 at 3.3 V");
        pmic.start(false);
        pmic.write(0x43);
        pmic.write(0);
        pmic.start(false);
        pmic.write(0x43);
        pmic.start(true);
        assert_eq!(pmic.read(), 0);
        assert!(!pmic.power_off);
        pmic.start(false);
        pmic.write(DEVCTRL as u8);
        pmic.write(DEV_OFF_RST);
        assert!(pmic.power_off);
    }
}
