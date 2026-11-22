//! Haoyu HYM8563 real-time clock on I²C (`haoyu,hym8563`, Linux `rtc-hym8563`), a
//! battery-backed clock at 0x51: a PCF8563-style chip with BCD time registers from
//! 0x02 (seconds, minutes, hours, day, weekday, month, year), a STOP bit in CTL1 while the
//! time is set, and alarm/timer/clock-out registers that read back what Linux writes.
//!
//! The clock runs on virtual time from the host's time at power-on, so the "clock not
//! valid" bit (seconds bit 7) is clear, as on a board whose coin cell kept it.
use crate::i2c::{I2cDevice, RegisterPointer};
use crate::rk808::{bcd, date_of, seconds_of, unbcd};

/// I²C address (`hym8563@51`).
pub const ADDRESS: u8 = 0x51;

const CTL1: usize = 0x00;
const CTL1_STOP: u8 = 1 << 5;
const SEC: usize = 0x02;
const YEAR: usize = 0x08;
const UNIX_2000: i64 = 946_684_800;

pub struct Hym8563 {
    regs: [u8; 16],
    pointer: RegisterPointer,
    /// Seconds since 2000-01-01 (Gregorian) at `epoch_ns` of virtual time.
    seconds: i64,
    epoch_ns: u64,
    now_ns: u64,
    /// Time registers written since the clock last took them over.
    time_written: bool,
}

impl Hym8563 {
    /// A clock that shows `unix_seconds` (UTC) at virtual time 0.
    pub fn new(unix_seconds: i64) -> Self {
        let mut rtc = Self {
            regs: [0; 16],
            pointer: RegisterPointer::default(),
            seconds: unix_seconds - UNIX_2000,
            epoch_ns: 0,
            now_ns: 0,
            time_written: false,
        };
        rtc.regs[0x09..=0x0c].fill(0x80); // alarms disabled
        rtc.latch();
        rtc
    }

    fn stopped(&self) -> bool {
        self.regs[CTL1] & CTL1_STOP != 0
    }

    fn now(&self) -> i64 {
        if self.stopped() {
            return self.seconds;
        }
        self.seconds + (self.now_ns.saturating_sub(self.epoch_ns) / 1_000_000_000) as i64
    }

    /// The running time into the time registers.
    fn latch(&mut self) {
        let t = self.now();
        let (year, month, day, hms) = date_of(t, 30);
        let weekday = (t.div_euclid(86400) + 6).rem_euclid(7); // 2000-01-01 was a Saturday
        let fields = [hms % 60, hms / 60 % 60, hms / 3600, day, weekday, month, (year - 2000).clamp(0, 99)];
        for (i, v) in fields.into_iter().enumerate() {
            self.regs[SEC + i] = bcd(v as u8);
        }
    }

    /// The clock takes over the time registers as written.
    fn take_time(&mut self) {
        let r = |i: usize, mask: u8| unbcd(self.regs[i] & mask) as i64;
        let hms = r(SEC + 2, 0x3f) * 3600 + r(SEC + 1, 0x7f) * 60 + r(SEC, 0x7f);
        self.seconds = seconds_of(2000 + r(YEAR, 0xff), r(SEC + 5, 0x1f).clamp(1, 12), r(SEC + 3, 0x3f).max(1), hms, 30);
        self.epoch_ns = self.now_ns;
        self.time_written = false;
    }

    /// The time as Unix seconds.
    pub fn unix_time(&self) -> i64 {
        self.now() + UNIX_2000
    }
}

impl I2cDevice for Hym8563 {
    fn start(&mut self, read: bool) -> bool {
        self.pointer.start(read);
        if read && !self.time_written {
            self.latch();
        }
        true
    }

    fn write(&mut self, byte: u8) -> bool {
        let Some(reg) = self.pointer.written(byte) else { return true };
        let reg = reg as usize & 0xf;
        if reg == CTL1 {
            let (was, is) = (self.stopped(), byte & CTL1_STOP != 0);
            if is && !was {
                self.seconds = self.now();
            }
            self.regs[CTL1] = byte;
            if was && !is {
                if self.time_written {
                    self.take_time();
                } else {
                    self.epoch_ns = self.now_ns;
                }
            }
        } else {
            self.regs[reg] = byte;
            if (SEC..=YEAR).contains(&reg) {
                self.time_written = true;
            }
        }
        true
    }

    fn read(&mut self) -> u8 {
        self.regs[self.pointer.next() as usize & 0xf]
    }

    fn stop(&mut self) {
        // Time written without stopping the clock takes effect at the end of the write.
        if self.time_written && !self.stopped() {
            self.take_time();
        }
    }

    fn clock(&mut self, now_ns: u64) {
        self.now_ns = now_ns;
    }
}

/// Seconds a restored clock may show: `date_of` goes through the years one by one.
const LIMIT: i64 = 1 << 40;

emu_core::snap_struct!(Hym8563 { regs, pointer, seconds, epoch_ns, now_ns, time_written } after |c| {
    if !(-LIMIT..LIMIT).contains(&c.seconds) { return Err(emu_core::snap::SnapError("a clock tens of thousands of years off".into())); }
    Ok(())
});

#[cfg(test)]
mod tests {
    use super::*;

    /// What Linux's `hym8563_rtc_read_time` makes of the registers, as Unix seconds.
    fn linux_reads(rtc: &mut Hym8563) -> i64 {
        rtc.start(false);
        rtc.write(SEC as u8);
        rtc.start(true);
        let buf: [u8; 7] = core::array::from_fn(|_| rtc.read());
        rtc.stop();
        assert_eq!(buf[0] & 0x80, 0, "clock not valid");
        let f = |i: usize, mask: u8| unbcd(buf[i] & mask) as i64;
        let hms = f(2, 0x3f) * 3600 + f(1, 0x7f) * 60 + f(0, 0x7f);
        seconds_of(2000 + f(6, 0xff), f(5, 0x1f), f(3, 0x3f), hms, 30) + UNIX_2000
    }

    #[test]
    fn runs_from_the_host_time_and_takes_a_new_time_like_linux_sets_it() {
        for unix in [1_763_337_600, 1_709_164_800, 1_798_675_199] {
            let mut rtc = Hym8563::new(unix);
            assert_eq!(linux_reads(&mut rtc), unix);
            rtc.clock(3_200_000_000);
            assert_eq!(linux_reads(&mut rtc), unix + 3);
        }
        // hym8563_rtc_set_time: STOP, the seven time registers, then run.
        let mut rtc = Hym8563::new(0);
        rtc.clock(1_000_000_000);
        let target = 1_800_000_000i64; // 2027-01-15 08:00:00 UTC
        let (y, m, d, hms) = date_of(target - UNIX_2000, 30);
        rtc.start(false);
        rtc.write(CTL1 as u8);
        rtc.write(CTL1_STOP);
        rtc.stop();
        rtc.start(false);
        rtc.write(SEC as u8);
        for v in [hms % 60, hms / 60 % 60, hms / 3600, d, 5, m, y - 2000] {
            rtc.write(bcd(v as u8));
        }
        rtc.stop();
        rtc.start(false);
        rtc.write(CTL1 as u8);
        rtc.write(0);
        rtc.stop();
        assert_eq!(linux_reads(&mut rtc), target);
        rtc.clock(61_000_000_000);
        assert_eq!(linux_reads(&mut rtc), target + 60);
    }
}
