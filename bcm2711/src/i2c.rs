//! BCM2711 I2C (BSC) controller, as driven by Linux `i2c-bcm2835`, and a Bosch BME280 sensor.
//!
//! Controller: a transfer starts when C is written with ST. Writes complete when DLEN bytes have
//! been pushed into the FIFO (TXW asks for them); reads complete at once, with the bytes in the
//! RX FIFO. DONE, TXW and RXR are reported in S and raise the interrupt when enabled (INTD,
//! INTT, INTR). No device at the address: ERR (NACK) with DONE.
use std::collections::VecDeque;

const C_READ: u32 = 1 << 0;
const C_CLEAR: u32 = 3 << 4;
const C_ST: u32 = 1 << 7;
const C_INTD: u32 = 1 << 8;
const C_INTT: u32 = 1 << 9;
const C_INTR: u32 = 1 << 10;
const S_TA: u32 = 1 << 0;
const S_DONE: u32 = 1 << 1;
const S_TXW: u32 = 1 << 2;
const S_RXR: u32 = 1 << 3;
const S_TXD: u32 = 1 << 4;
const S_RXD: u32 = 1 << 5;
const S_TXE: u32 = 1 << 6;
const S_ERR: u32 = 1 << 8;
const S_CLKT: u32 = 1 << 9;

pub trait I2cDevice {
    fn address(&self) -> u8;
    fn write(&mut self, data: &[u8]);
    fn read(&mut self, out: &mut [u8]);
}

pub struct I2c {
    c: u32,
    s: u32,
    dlen: u32,
    a: u32,
    div: u32,
    del: u32,
    clkt: u32,
    rx: VecDeque<u8>,
    /// a write in progress: bytes collected so far
    wr: Option<Vec<u8>>,
    /// the board's BME280 at 0x76
    pub bme280: Bme280,
}

impl I2c {
    pub fn new() -> Self {
        I2c { c: 0, s: 0, dlen: 0, a: 0, div: 0x5dc, del: 0x0030_0030, clkt: 0x40, rx: VecDeque::new(), wr: None, bme280: Bme280::new() }
    }

    fn device(&mut self) -> Option<&mut dyn I2cDevice> {
        let a = (self.a & 0x7f) as u8;
        if self.bme280.address() == a { Some(&mut self.bme280) } else { None }
    }

    fn status(&self) -> u32 {
        let mut s = self.s & (S_DONE | S_ERR | S_CLKT) | S_TXD;
        if let Some(w) = &self.wr { s |= S_TA; if (w.len() as u32) < self.dlen { s |= S_TXW; } }
        if !self.rx.is_empty() { s |= S_RXD | S_RXR; }
        if self.wr.is_none() { s |= S_TXE; }
        s
    }

    pub fn irq(&self) -> bool {
        let s = self.status();
        (self.c & C_INTD != 0 && s & S_DONE != 0) || (self.c & C_INTT != 0 && s & S_TXW != 0) || (self.c & C_INTR != 0 && s & S_RXR != 0)
    }

    pub fn read(&mut self, off: u64) -> u32 {
        match off {
            0x00 => self.c,
            0x04 => self.status(),
            0x08 => self.dlen,
            0x0c => self.a,
            0x10 => self.rx.pop_front().unwrap_or(0) as u32,
            0x14 => self.div,
            0x18 => self.del,
            0x1c => self.clkt,
            _ => 0,
        }
    }

    pub fn write(&mut self, off: u64, v: u32) {
        match off {
            0x00 => {
                if v & C_CLEAR != 0 { self.rx.clear(); self.wr = None; }
                self.c = v & !(C_ST | C_CLEAR);
                if v & C_ST != 0 { self.start(v & C_READ != 0); }
            }
            0x04 => self.s &= !(v & (S_DONE | S_ERR | S_CLKT)),
            0x08 => self.dlen = v & 0xffff,
            0x0c => self.a = v & 0x7f,
            0x10 => {
                if let Some(w) = self.wr.as_mut() {
                    w.push(v as u8);
                    if w.len() as u32 >= self.dlen { self.finish_write(); }
                }
            }
            0x14 => self.div = v,
            0x18 => self.del = v,
            0x1c => self.clkt = v,
            _ => {}
        }
    }

    fn start(&mut self, read: bool) {
        self.wr = None;
        if self.device().is_none() {
            self.s |= S_ERR | S_DONE; // no acknowledge from the address
            return;
        }
        if read {
            let mut buf = vec![0u8; self.dlen as usize];
            self.device().unwrap().read(&mut buf);
            self.rx.extend(buf);
            self.s |= S_DONE;
        } else {
            self.wr = Some(Vec::with_capacity(self.dlen as usize));
            if self.dlen == 0 { self.finish_write(); }
        }
    }

    fn finish_write(&mut self) {
        if let Some(w) = self.wr.take() {
            if let Some(d) = self.device() { d.write(&w); }
            self.s |= S_DONE;
        }
    }
}

// ---------------------------------------------------------------------------- BME280

/// Bosch BME280 (temperature, pressure, humidity) at I2C address 0x76. Calibration constants are
/// the datasheet's example values; the raw ADC words are chosen so that the datasheet's integer
/// compensation formulas (which Linux `bmp280` implements) give the physical values the host sets.
pub struct Bme280 {
    pub temp_c: f64,
    pub pressure_pa: f64,
    pub humidity: f64,
    regs: [u8; 256],
    ptr: u8,
}

const T1: i64 = 27504; const T2: i64 = 26435; const T3: i64 = -1000;
const P: [i64; 9] = [36477, -10685, 3024, 2855, 140, -7, 15500, -14600, 6000];
const H1: i64 = 75; const H2: i64 = 362; const H3: i64 = 0; const H4: i64 = 313; const H5: i64 = 50; const H6: i64 = 30;

impl Default for I2c {
    fn default() -> Self { Self::new() }
}

impl Default for Bme280 {
    fn default() -> Self { Self::new() }
}

impl Bme280 {
    pub fn new() -> Self {
        let mut b = Bme280 { temp_c: 21.5, pressure_pa: 101_325.0, humidity: 45.0, regs: [0; 256], ptr: 0 };
        let put16 = |r: &mut [u8; 256], a: usize, v: i64| { r[a] = v as u8; r[a + 1] = (v >> 8) as u8; };
        put16(&mut b.regs, 0x88, T1); put16(&mut b.regs, 0x8a, T2); put16(&mut b.regs, 0x8c, T3);
        for (k, v) in P.iter().enumerate() { put16(&mut b.regs, 0x8e + 2 * k, *v); }
        b.regs[0xa1] = H1 as u8;
        put16(&mut b.regs, 0xe1, H2);
        b.regs[0xe3] = H3 as u8;
        b.regs[0xe4] = (H4 >> 4) as u8;
        b.regs[0xe5] = ((H4 & 0xf) | ((H5 & 0xf) << 4)) as u8;
        b.regs[0xe6] = (H5 >> 4) as u8;
        b.regs[0xe7] = H6 as u8;
        b.regs[0xd0] = 0x60; // chip id
        b
    }

    fn t_fine(adc_t: i64) -> i64 {
        let v1 = (((adc_t >> 3) - (T1 << 1)) * T2) >> 11;
        let v2 = (((((adc_t >> 4) - T1) * ((adc_t >> 4) - T1)) >> 12) * T3) >> 14;
        v1 + v2
    }
    fn temp(adc_t: i64) -> i64 { (Self::t_fine(adc_t) * 5 + 128) >> 8 } // 0.01 degC

    /// Pressure in Pa * 256 (datasheet 64-bit integer formula).
    fn press(adc_p: i64, t_fine: i64) -> i64 {
        let mut v1 = t_fine - 128000;
        let mut v2 = v1 * v1 * P[5];
        v2 += (v1 * P[4]) << 17;
        v2 += P[3] << 35;
        v1 = ((v1 * v1 * P[2]) >> 8) + ((v1 * P[1]) << 12);
        v1 = (((1i64 << 47) + v1) * P[0]) >> 33;
        if v1 == 0 { return 0; }
        let mut p = 1048576 - adc_p;
        p = (((p << 31) - v2) * 3125) / v1;
        v1 = (P[8] * (p >> 13) * (p >> 13)) >> 25;
        v2 = (P[7] * p) >> 19;
        ((p + v1 + v2) >> 8) + (P[6] << 4)
    }

    /// Humidity in %RH * 1024.
    fn hum(adc_h: i64, t_fine: i64) -> i64 {
        // datasheet 4.2.3: both factors use the same (t_fine - 76800)
        let x = t_fine - 76800;
        let a = (((adc_h << 14) - (H4 << 20) - (H5 * x)) + 16384) >> 15;
        let b = ((((((x * H6) >> 10) * (((x * H3) >> 11) + 32768)) >> 10) + 2097152) * H2 + 8192) >> 14;
        let mut v = a * b;
        v -= ((((v >> 15) * (v >> 15)) >> 7) * H1) >> 4;
        v.clamp(0, 419430400) >> 12
    }

    /// Smallest raw value in [0, 2^20) whose compensated value reaches `target` (f monotonic).
    fn invert(target: i64, increasing: bool, f: impl Fn(i64) -> i64) -> i64 {
        let (mut lo, mut hi) = (0i64, (1 << 20) - 1);
        while lo < hi {
            let mid = (lo + hi) / 2;
            let v = f(mid);
            if (v < target) == increasing { lo = mid + 1 } else { hi = mid }
        }
        lo
    }

    /// Refresh the data registers from the physical values.
    fn convert(&mut self) {
        let adc_t = Self::invert((self.temp_c * 100.0).round() as i64, true, Self::temp);
        let tf = Self::t_fine(adc_t);
        let adc_p = Self::invert((self.pressure_pa * 256.0).round() as i64, false, |a| Self::press(a, tf));
        let adc_h = Self::invert((self.humidity * 1024.0).round() as i64, true, |a| Self::hum(a, tf)).min(0xffff);
        let put20 = |r: &mut [u8; 256], a: usize, v: i64| { r[a] = (v >> 12) as u8; r[a + 1] = (v >> 4) as u8; r[a + 2] = ((v & 0xf) << 4) as u8; };
        put20(&mut self.regs, 0xf7, adc_p);
        put20(&mut self.regs, 0xfa, adc_t);
        self.regs[0xfd] = (adc_h >> 8) as u8;
        self.regs[0xfe] = adc_h as u8;
    }
}

impl I2cDevice for Bme280 {
    fn address(&self) -> u8 { 0x76 }
    fn write(&mut self, data: &[u8]) {
        let Some((&reg, rest)) = data.split_first() else { return };
        self.ptr = reg;
        for &b in rest {
            match self.ptr {
                0xe0 => { if b == 0xb6 { self.regs[0xf2] = 0; self.regs[0xf4] = 0; self.regs[0xf5] = 0; } }
                0xf2 | 0xf4 | 0xf5 => self.regs[self.ptr as usize] = b,
                _ => {}
            }
            self.ptr = self.ptr.wrapping_add(1);
        }
    }
    fn read(&mut self, out: &mut [u8]) {
        if (0xf7..=0xfe).contains(&self.ptr) || self.ptr == 0xf3 { self.convert(); }
        for b in out.iter_mut() {
            *b = match self.ptr { 0xf3 => 0, p => self.regs[p as usize] }; // status: never measuring
            self.ptr = self.ptr.wrapping_add(1);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn bme280_compensation_round_trip() {
        // the datasheet example: adc_T 519888 -> 25.08 degC
        assert_eq!(Bme280::temp(519888), 2508);
        let mut b = Bme280::new();
        b.temp_c = 23.25; b.pressure_pa = 98_700.0; b.humidity = 61.0;
        b.write(&[0xf7]);
        let mut d = [0u8; 8];
        b.read(&mut d);
        let raw = |h: u8, m: u8, l: u8| ((h as i64) << 12) | ((m as i64) << 4) | (l as i64 >> 4);
        let (ap, at, ah) = (raw(d[0], d[1], d[2]), raw(d[3], d[4], d[5]), ((d[6] as i64) << 8) | d[7] as i64);
        let tf = Bme280::t_fine(at);
        assert!((Bme280::temp(at) - 2325).abs() <= 1);
        assert!(((Bme280::press(ap, tf) as f64 / 256.0) - 98_700.0).abs() < 2.0);
        assert!(((Bme280::hum(ah, tf) as f64 / 1024.0) - 61.0).abs() < 0.1);
        // independent reference: the Linux bmp280 driver's expression, transcribed with s32 math
        let linux_hum = |adc: i32, t_fine: i32| -> u32 {
            let (h1, h2, h3, h4, h5, h6) = (H1 as i32, H2 as i32, H3 as i32, H4 as i32, H5 as i32, H6 as i32);
            let mut var = t_fine - 76800;
            var = ((((adc << 14) - (h4 << 20) - (h5 * var)) + 16384) >> 15)
                * (((((((var * h6) >> 10) * (((var * h3) >> 11) + 32768)) >> 10) + 2097152) * h2 + 8192) >> 14);
            var -= ((((var >> 15) * (var >> 15)) >> 7) * h1) >> 4;
            (var.clamp(0, 419430400) >> 12) as u32
        };
        for adc in [20000, 30000, 40000] { assert_eq!(Bme280::hum(adc, tf) as u32, linux_hum(adc as i32, tf as i32)); }
    }
}
