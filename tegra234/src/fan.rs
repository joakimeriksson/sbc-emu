//! The module's fan: driven by PWM3 (`nvidia,tegra194-pwm`, Linux `pwm-tegra`, through
//! `pwm-fan`), measured by the fan tachometer (`nvidia,pwm-tegra234-tachometer`, NVIDIA's
//! `pwm-tegra-tachometer`).
//!
//! - PWM: one channel, one register: enable (bit 31), duty in 256ths (bits 24:16, 256 = on all
//!   the time) and the clock divider (bits 12:0).
//! - Fan: a 4-wire 5 V fan of the Orin Nano developer kit's kind, 6000 rpm at full duty (the
//!   speed nvfancontrol's profiles expect at PWM 255), two tachometer pulses per revolution;
//!   it spins up and down with a time constant of about a second and stops below 10 % duty.
//! - Tachometer: TACH_FAN_TACH0 holds the time `2^WIN_LENGTH` pulses took in ticks of its
//!   1.010526 MHz counter, minus one (bits 18:0, all ones for a stopped fan); WIN_LENGTH (bits
//!   26:25) is the driver's. TACH1 is the high time of a pulse (half of it).
pub const PWM_BASE: u64 = 0x032a_0000;
pub const PWM_SIZE: u64 = 0x1_0000;
pub const TACH_BASE: u64 = 0x039c_0000;
pub const TACH_SIZE: u64 = 0x10;

const PWM_ENABLE: u32 = 1 << 31;
const TACH0: u64 = 0x0;
const TACH1: u64 = 0x4;
const PERIOD_MASK: u32 = 0x7_ffff;
const WIN_SHIFT: u32 = 25;
const OVERFLOW: u32 = 1 << 24;
const TACH_HZ: f64 = 1_010_526.0;
pub const PULSES_PER_REV: u32 = 2;
pub const MAX_RPM: f64 = 6000.0;
const SPIN_TAU_S: f64 = 1.0;

#[derive(Default)]
pub struct Fan {
    pwm: u32,
    /// the tachometer's registers that hold what the driver wrote (TACH0's WIN_LENGTH, the
    /// thresholds, interrupt enable, control)
    tach: [u32; 8],
    pub rpm: f64,
    since: u64,
}

impl Fan {
    /// The duty cycle the fan sees (0-1).
    pub fn duty(&self) -> f64 {
        if self.pwm & PWM_ENABLE == 0 {
            // pwm-fan's pin idles low: a 4-wire fan's PWM input pulled to 0 stops it
            return 0.0;
        }
        f64::from((self.pwm >> 16 & 0x1ff).min(256)) / 256.0
    }

    fn target_rpm(&self) -> f64 {
        let duty = self.duty();
        if duty < 0.1 { 0.0 } else { MAX_RPM * duty }
    }

    /// The rotor's speed follows the duty cycle.
    pub fn advance(&mut self, now: u64) {
        let dt = now.saturating_sub(self.since) as f64 / 1e9;
        self.since = self.since.max(now);
        let k = 1.0 - (-dt / SPIN_TAU_S).exp();
        self.rpm += (self.target_rpm() - self.rpm) * k;
        if self.rpm < 1.0 {
            self.rpm = 0.0;
        }
    }

    pub fn pwm_read(&self, offset: u64) -> u32 {
        if offset == 0 { self.pwm } else { 0 }
    }

    pub fn pwm_write(&mut self, offset: u64, value: u32, now: u64) {
        if offset == 0 {
            self.advance(now);
            self.pwm = value & (PWM_ENABLE | 0x1ff << 16 | 0x1fff);
        }
    }

    fn window(&self) -> u32 {
        1 << (self.tach[0] >> WIN_SHIFT & 3)
    }

    pub fn tach_read(&mut self, offset: u64, now: u64) -> u32 {
        self.advance(now);
        let pulse_s = if self.rpm >= 1.0 { 60.0 / (self.rpm * f64::from(PULSES_PER_REV)) } else { f64::INFINITY };
        match offset {
            TACH0 => {
                let ticks = pulse_s * f64::from(self.window()) * TACH_HZ;
                let period = if ticks >= f64::from(PERIOD_MASK) { PERIOD_MASK } else { (ticks as u32).saturating_sub(1).max(1) };
                let overflow = if period == PERIOD_MASK { OVERFLOW } else { 0 };
                (self.tach[0] & (3 << WIN_SHIFT)) | overflow | period
            }
            TACH1 => {
                let high = pulse_s / 2.0 * TACH_HZ;
                if high >= f64::from(PERIOD_MASK) { PERIOD_MASK } else { high as u32 }
            }
            o if o < TACH_SIZE => self.tach[(o / 4) as usize],
            _ => 0,
        }
    }

    pub fn tach_write(&mut self, offset: u64, value: u32) {
        match offset {
            // writing 1 to the overflow bit clears it; the period is the counter's
            TACH0 => self.tach[0] = value & (3 << WIN_SHIFT),
            TACH1 => {}
            o if o < TACH_SIZE => self.tach[(o / 4) as usize] = value,
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The driver's rpm: 60 s / period, the period from TACH0 as `pwm_tegra_tacho_capture`
    /// computes it (µs).
    fn driver_rpm(fan: &mut Fan, now: u64) -> u64 {
        let tach0 = fan.tach_read(TACH0, now);
        let period = tach0 & PERIOD_MASK;
        if period == 0 || period == PERIOD_MASK {
            return 0;
        }
        let win = u64::from(fan.window());
        let us = (u64::from(period) + 1) * u64::from(PULSES_PER_REV) * 1_000_000 / (win * 1_010_526);
        60_000_000 / us
    }

    #[test]
    fn the_fan_spins_up_to_the_duty_cycle_and_the_tachometer_sees_it() {
        let mut fan = Fan::default();
        fan.tach_write(TACH0, 1 << WIN_SHIFT); // a window of two pulses
        assert_eq!(driver_rpm(&mut fan, 0), 0);
        assert_ne!(fan.tach_read(TACH0, 0) & OVERFLOW, 0);
        // pwm-fan's 255 of 255 at a 45334 ns period: duty 256/256
        fan.pwm_write(0, PWM_ENABLE | 256 << 16, 0);
        let rpm = driver_rpm(&mut fan, 10_000_000_000);
        assert!((5900..=6100).contains(&rpm), "{rpm}");
        fan.pwm_write(0, PWM_ENABLE | 128 << 16, 10_000_000_000);
        let rpm = driver_rpm(&mut fan, 20_000_000_000);
        assert!((2900..=3100).contains(&rpm), "{rpm}");
        fan.pwm_write(0, 128 << 16, 20_000_000_000);
        assert_eq!(driver_rpm(&mut fan, 40_000_000_000), 0);
    }
}
