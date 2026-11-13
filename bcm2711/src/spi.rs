//! BCM2835-style SPI0 master (Linux `spi-bcm2835`) and a Microchip MCP3008 10-bit ADC on CE0.
//!
//! Bytes written to the FIFO while TA is set are shifted through the selected device at once
//! (full duplex), the received byte appears in the RX FIFO. Chip select comes from the GPIO
//! controller (the driver drives CE0/CE1 as GPIO8/GPIO7, `cs-gpios` in the device tree).
use std::collections::VecDeque;

const CS_CLEAR: u32 = 3 << 4;
const CS_TA: u32 = 1 << 7;
const CS_INTD: u32 = 1 << 9;
const CS_INTR: u32 = 1 << 10;
const CS_DONE: u32 = 1 << 16;
const CS_RXD: u32 = 1 << 17;
const CS_TXD: u32 = 1 << 18;
const CS_RXR: u32 = 1 << 19;
const CS_RXF: u32 = 1 << 20;

pub struct Spi {
    cs: u32,
    clk: u32,
    dlen: u32,
    rx: VecDeque<u8>,
    pub adc: Mcp3008,
}

impl Default for Spi {
    fn default() -> Self { Self::new() }
}

impl Spi {
    pub fn new() -> Self { Spi { cs: 0x0004_1000, clk: 0, dlen: 0, rx: VecDeque::new(), adc: Mcp3008::new() } }

    fn status(&self) -> u32 {
        let mut s = self.cs & 0x0000_ffff | CS_TXD;
        if self.cs & CS_TA != 0 { s |= CS_DONE; }
        if !self.rx.is_empty() { s |= CS_RXD; }
        if self.rx.len() >= 48 { s |= CS_RXR; }
        if self.rx.len() >= 64 { s |= CS_RXF; }
        s
    }

    pub fn irq(&self) -> bool {
        let s = self.status();
        (self.cs & CS_INTD != 0 && s & CS_DONE != 0) || (self.cs & CS_INTR != 0 && s & CS_RXR != 0)
    }

    pub fn read(&mut self, off: u64) -> u32 {
        match off {
            0x00 => self.status(),
            0x04 => self.rx.pop_front().unwrap_or(0) as u32,
            0x08 => self.clk,
            0x0c => self.dlen,
            _ => 0,
        }
    }

    /// `ce0`: chip-select 0 is asserted (GPIO8 low).
    pub fn write(&mut self, off: u64, v: u32, ce0: bool) {
        match off {
            0x00 => {
                if v & (1 << 5) != 0 { self.rx.clear(); }
                self.cs = v & !CS_CLEAR;
            }
            0x04 => {
                let rx = if self.cs & CS_TA != 0 && ce0 { self.adc.exchange(v as u8) } else { 0xff };
                if self.cs & CS_TA != 0 { self.rx.push_back(rx); }
            }
            0x08 => self.clk = v,
            0x0c => self.dlen = v,
            _ => {}
        }
    }

    /// CE0 released: the device's transaction ends.
    pub fn deselect(&mut self) { self.adc.reset(); }
}

/// MCP3008: 8-channel 10-bit ADC. Bit-serial: wait for the start bit, take SGL/DIFF and D2..D0,
/// spend one clock sampling, output a null bit, then B9..B0 (MSB first).
pub struct Mcp3008 {
    /// channel inputs in volts (VREF = 3.3 V)
    pub volts: [f64; 8],
    pub vref: f64,
    state: u8, // 0 waiting for start, 1..=4 command bits, 5 output
    cmd: u8,
    out: VecDeque<bool>,
}

impl Mcp3008 {
    pub fn new() -> Self { Mcp3008 { volts: [0.0, 0.825, 1.65, 2.475, 3.3, 0.0, 0.0, 0.0], vref: 3.3, state: 0, cmd: 0, out: VecDeque::new() } }

    pub fn reset(&mut self) { self.state = 0; self.cmd = 0; self.out.clear(); }

    pub fn code(&self, ch: usize) -> u16 { ((self.volts[ch] / self.vref).clamp(0.0, 1.0) * 1023.0).round() as u16 }

    fn bit(&mut self, din: bool) -> bool {
        let dout = self.out.pop_front().unwrap_or(false);
        match self.state {
            0 => if din { self.state = 1; self.cmd = 0; },
            1..=4 => {
                self.cmd = (self.cmd << 1) | din as u8;
                self.state += 1;
                if self.state == 5 {
                    let single = self.cmd & 0x8 != 0;
                    let ch = (self.cmd & 7) as usize;
                    // differential: CH(2k) - CH(2k+1) (or swapped), clamped at 0
                    let code = if single { self.code(ch) } else {
                        let (p, n) = (ch & !1 | (ch & 1), ch ^ 1);
                        (((self.volts[p] - self.volts[n]) / self.vref).clamp(0.0, 1.0) * 1023.0).round() as u16
                    };
                    self.out.push_back(false); // sampling clock
                    self.out.push_back(false); // null bit
                    for b in (0..10).rev() { self.out.push_back(code >> b & 1 != 0); }
                }
            }
            _ => {}
        }
        dout
    }

    fn exchange(&mut self, tx: u8) -> u8 {
        let mut rx = 0u8;
        for b in (0..8).rev() { rx |= (self.bit(tx >> b & 1 != 0) as u8) << b; }
        rx
    }
}

impl Default for Mcp3008 {
    fn default() -> Self { Self::new() }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn mcp3008_matches_linux_mcp320x_framing() {
        // mcp320x: tx = start<<6 | single<<5 | ch<<2, then two bytes read: rx0<<2 | rx1>>6
        let mut a = Mcp3008::new();
        for ch in 0..8u8 {
            a.reset();
            let _ = a.exchange((1 << 6) | (1 << 5) | (ch << 2));
            let r0 = a.exchange(0);
            let r1 = a.exchange(0);
            let v = ((r0 as u16) << 2) | (r1 as u16 >> 6);
            assert_eq!(v, a.code(ch as usize), "channel {ch}");
        }
        assert_eq!(a.code(4), 1023);
        assert_eq!(a.code(2), 512);
    }
}
