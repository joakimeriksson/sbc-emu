//! The BCM2837's own small blocks, older than the Pi 4's: the BCM2835 random number generator
//! (`brcm,bcm2835-rng`) and the BCM2835-family thermal sensor (`brcm,bcm2837-thermal`).

/// The BCM2835 RNG: a control register, a status register whose top byte counts the words
/// ready, and a data register the driver polls (its interrupt stays masked).
pub struct Rng {
    ctrl: u32,
    int_mask: u32,
    state: u64,
}

impl Default for Rng { fn default() -> Self { Self::new() } }

impl Rng {
    /// A fixed seed: runs on one thread stay repeatable (`reseed` changes it).
    pub fn new() -> Self { Rng { ctrl: 0, int_mask: 0, state: 0x5bc0_e0b8_9e37_79b9 } }
    pub fn reseed(&mut self, seed: u64) { self.state = seed.wrapping_mul(0x9e37_79b9_7f4a_7c15) | 1; }
    fn next(&mut self) -> u32 {
        let x = &mut self.state;
        *x ^= *x >> 12;
        *x ^= *x << 25;
        *x ^= *x >> 27;
        (x.wrapping_mul(0x2545_f491_4f6c_dd1d) >> 32) as u32
    }
    pub fn read(&mut self, off: u64) -> u32 {
        let on = self.ctrl & 1 != 0;
        match off {
            0x0 => self.ctrl,
            // RNG_STATUS: words available in bits 31..24 (the FIFO holds four)
            0x4 => if on { 4 << 24 } else { 0 },
            0x8 => if on { self.next() } else { 0 },
            0x10 => self.int_mask,
            _ => 0,
        }
    }
    pub fn write(&mut self, off: u64, v: u32) {
        match off {
            0x0 => self.ctrl = v,
            0x10 => self.int_mask = v,
            _ => {}
        }
    }
    pub fn save_state(&self, w: &mut emu_core::snap::W) { w.tag("rng2835-v1"); w.u32(self.ctrl); w.u32(self.int_mask); w.u64(self.state); }
    pub fn load_state(&mut self, r: &mut emu_core::snap::R) -> Result<(), emu_core::snap::SnapError> {
        r.tag("rng2835-v1")?;
        self.ctrl = r.u32()?; self.int_mask = r.u32()?; self.state = r.u64()?;
        Ok(())
    }
}

/// The thermal sensor's conversion, as the device tree's thermal zone gives it to the driver:
/// millidegrees = OFFSET + SLOPE * code (the values of the vendor `bcm2837.dtsi`).
pub const THERMAL_SLOPE: i32 = -538;
pub const THERMAL_OFFSET: i32 = 412_000;

/// The BCM2835-family temperature sensor: TSENSCTL (set up "by the firmware": RSTB high) and
/// TSENSSTAT (a valid 10-bit code).
pub struct Thermal {
    ctl: u32,
    /// the SoC temperature it measures (millidegrees C)
    pub temp_mc: i32,
}

impl Default for Thermal { fn default() -> Self { Self::new() } }

impl Thermal {
    pub fn new() -> Self { Thermal { ctl: 1 << 1, temp_mc: 45_000 } }
    pub fn read(&self, off: u64) -> u32 {
        match off {
            0x0 => self.ctl,
            0x4 => {
                let code = ((THERMAL_OFFSET - self.temp_mc) / -THERMAL_SLOPE).clamp(0, 0x3ff) as u32;
                code | (1 << 10) // VALID
            }
            _ => 0,
        }
    }
    pub fn write(&mut self, off: u64, v: u32) { if off == 0 { self.ctl = v; } }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rng_needs_enable() {
        let mut r = Rng::new();
        assert_eq!(r.read(0x4), 0);
        r.write(0x4, 0x40000); // the driver's warm-up count (ignored)
        r.write(0x0, 1);
        assert_eq!(r.read(0x4) >> 24, 4);
        let (a, b) = (r.read(0x8), r.read(0x8));
        assert_ne!(a, b);
    }

    #[test]
    fn thermal_code_matches_the_zone() {
        let mut t = Thermal::new();
        t.temp_mc = 52_000;
        let s = t.read(0x4);
        assert_ne!(s & (1 << 10), 0);
        let back = THERMAL_OFFSET + THERMAL_SLOPE * (s & 0x3ff) as i32;
        assert!((back - 52_000).abs() <= -THERMAL_SLOPE, "{back}");
        assert_ne!(t.read(0x0) & 2, 0, "set up as the firmware leaves it");
    }
}
