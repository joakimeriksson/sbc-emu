//! The RK3288's I2S controller (`rockchip,rk3288-i2s` at 0xff890000), as Linux's
//! `rockchip_i2s` drives it in master mode, so what the guest plays ends up as samples on
//! the host.
//!
//! The transmit side consumes frames at the sample rate. As the clock master (CKR.MSS),
//! MCLK (the CRU's `i2s_pre`) / (CKR.MDIV + 1) is the bit clock, / (CKR.TSD + 1) the frame
//! rate. As a slave, the codec makes the clocks from the MCLK it is given (`i2s0_clkout`,
//! the same clock): the panel board's codec (the vendor's `codec`, clock and frame master) runs at
//! 256 fs, so the kernel sets 11.2896 MHz for 44.1 kHz. Each frame takes one
//! FIFO word per channel (TXCR.CSR: 2-8 channels), or one word per two channels for 16-bit
//! samples with TXCR.HWT; the first two channels are kept as 16-bit stereo. The receive
//! side produces silent frames at the same rate. The FIFOs are 32 words; DMA requests
//! follow DMACR's watermarks (transmit: level <= TDL, receive: level > RDL).

use std::collections::VecDeque;

pub const SIZE: u64 = 0x1_0000;

const TXCR: u64 = 0x00;
const RXCR: u64 = 0x04;
const CKR: u64 = 0x08;
const FIFOLR: u64 = 0x0c;
const DMACR: u64 = 0x10;
const INTCR: u64 = 0x14;
const INTSR: u64 = 0x18;
const XFER: u64 = 0x1c;
const CLR: u64 = 0x20;
pub const TXDR: u64 = 0x24;
pub const RXDR: u64 = 0x28;

const XFER_TXS: u32 = 1 << 0;
const XFER_RXS: u32 = 1 << 1;
const DMACR_TDE: u32 = 1 << 8;
const DMACR_RDE: u32 = 1 << 24;
const CKR_MSS_SLAVE: u32 = 1 << 27;
/// MCLK / frame rate of the codec when it masters the clocks.
const CODEC_MCLK_FS: u64 = 256;
const TXCR_HWT: u32 = 1 << 14;
const FIFO_WORDS: usize = 32;
/// The sample rate assumed when the clocks give none (an external MCLK).
const DEFAULT_RATE: u32 = 48_000;

pub struct I2s {
    txcr: u32,
    rxcr: u32,
    ckr: u32,
    dmacr: u32,
    intcr: u32,
    xfer: u32,
    tx: VecDeque<u32>,
    rx_level: usize,
    /// Virtual time (ns) up to which transmit and receive frames are accounted for.
    tx_to: u64,
    rx_to: u64,
    /// The frame rate in Hz, from the clocks at the last update.
    pub rate: u32,
    /// Collect the played samples in `out` (16-bit stereo, interleaved).
    pub capture: bool,
    pub out: Vec<i16>,
    pub frames: u64,
    pub underruns: u64,
}

impl Default for I2s {
    fn default() -> Self {
        Self {
            txcr: 0x0000_000f,
            rxcr: 0x0000_000f,
            ckr: 0x0007_1f1f,
            dmacr: 0x001f_0000,
            intcr: 0x01f0_0000,
            xfer: 0,
            tx: VecDeque::new(),
            rx_level: 0,
            tx_to: 0,
            rx_to: 0,
            rate: 0,
            capture: false,
            out: Vec::new(),
            frames: 0,
            underruns: 0,
        }
    }
}

impl I2s {
    pub fn read(&mut self, offset: u64) -> u32 {
        match offset {
            TXCR => self.txcr,
            RXCR => self.rxcr,
            CKR => self.ckr,
            FIFOLR => self.tx.len().min(FIFO_WORDS) as u32 | (self.rx_level.min(FIFO_WORDS) as u32) << 24,
            DMACR => self.dmacr,
            INTCR => self.intcr,
            INTSR => 0,
            XFER => self.xfer,
            CLR => 0, // clears complete at once
            RXDR => {
                self.rx_level = self.rx_level.saturating_sub(1);
                0
            }
            _ => 0,
        }
    }

    /// A register write at virtual time `now` (ns).
    pub fn write(&mut self, offset: u64, value: u32, now: u64) {
        match offset {
            TXCR => self.txcr = value,
            RXCR => self.rxcr = value,
            CKR => self.ckr = value,
            DMACR => self.dmacr = value,
            INTCR => self.intcr = value,
            XFER => {
                if value & XFER_TXS != 0 && self.xfer & XFER_TXS == 0 {
                    self.tx_to = now;
                }
                if value & XFER_RXS != 0 && self.xfer & XFER_RXS == 0 {
                    self.rx_to = now;
                }
                self.xfer = value & (XFER_TXS | XFER_RXS);
            }
            CLR => {
                if value & 1 != 0 {
                    self.tx.clear();
                }
                if value & 2 != 0 {
                    self.rx_level = 0;
                }
            }
            TXDR => self.push(value),
            _ => {}
        }
    }

    /// A word into the transmit FIFO (from the CPU or the DMA controller).
    pub fn push(&mut self, word: u32) {
        if self.tx.len() < FIFO_WORDS {
            self.tx.push_back(word);
        }
    }

    /// The frame rate for MCLK `mclk` Hz.
    fn frame_rate(&self, mclk: u64) -> u32 {
        if mclk == 0 {
            return DEFAULT_RATE;
        }
        if self.ckr & CKR_MSS_SLAVE != 0 {
            return (mclk / CODEC_MCLK_FS) as u32;
        }
        let bclk_div = u64::from(self.ckr >> 16 & 0xff) + 1;
        let lrck_div = u64::from(self.ckr & 0xff) + 1;
        (mclk / bclk_div / lrck_div) as u32
    }

    fn tx_channels(&self) -> usize {
        2 * (((self.txcr >> 15) & 3) as usize + 1)
    }

    fn sample_bits(&self) -> u32 {
        (self.txcr & 0x1f) + 1
    }

    /// FIFO words per transmitted frame.
    fn tx_words(&self) -> usize {
        if self.txcr & TXCR_HWT != 0 && self.sample_bits() <= 16 { self.tx_channels() / 2 } else { self.tx_channels() }
    }

    fn rx_words(&self) -> usize {
        2 * (((self.rxcr >> 15) & 3) as usize + 1)
    }

    /// Play and record frames up to virtual time `now` (ns) with MCLK `mclk` Hz.
    pub fn update(&mut self, now: u64, mclk: u64) {
        self.rate = self.frame_rate(mclk);
        let period = 1_000_000_000 / u64::from(self.rate.max(1));
        if self.xfer & XFER_TXS != 0 {
            let words = self.tx_words();
            while self.tx_to + period <= now {
                self.tx_to += period;
                if self.tx.len() < words {
                    // an underrun: the frame goes out as silence, and is not recorded
                    self.underruns += 1;
                    continue;
                }
                let frame: Vec<u32> = self.tx.drain(..words).collect();
                self.frames += 1;
                if self.capture {
                    let (l, r) = self.decode(&frame);
                    self.out.extend_from_slice(&[l, r]);
                }
            }
        }
        if self.xfer & XFER_RXS != 0 {
            let words = self.rx_words();
            while self.rx_to + period <= now {
                self.rx_to += period;
                self.rx_level = (self.rx_level + words).min(FIFO_WORDS);
            }
        }
    }

    /// The first two channels of a frame, as 16-bit samples.
    fn decode(&self, frame: &[u32]) -> (i16, i16) {
        let bits = self.sample_bits();
        if self.txcr & TXCR_HWT != 0 && bits <= 16 {
            let w = frame[0];
            return (w as u16 as i16, (w >> 16) as u16 as i16);
        }
        let to16 = |w: u32| ((w << (32 - bits)) as i32 >> 16) as i16;
        (to16(frame[0]), to16(frame.get(1).copied().unwrap_or(0)))
    }

    /// The transmit FIFO asks the DMA controller for data.
    pub fn tx_request(&self) -> bool {
        self.dmacr & DMACR_TDE != 0 && self.tx.len() <= (self.dmacr & 0x1f) as usize
    }

    /// The receive FIFO asks the DMA controller to take data.
    pub fn rx_request(&self) -> bool {
        self.dmacr & DMACR_RDE != 0 && self.rx_level > (self.dmacr >> 16 & 0x1f) as usize
    }

    /// When the next DMA request rises (virtual ns), while a direction runs.
    pub fn next_deadline(&self) -> Option<u64> {
        let period = 1_000_000_000 / u64::from(self.rate.max(1));
        let mut next = None::<u64>;
        if self.xfer & XFER_TXS != 0 && self.dmacr & DMACR_TDE != 0 {
            let tdl = (self.dmacr & 0x1f) as usize;
            let frames = self.tx.len().saturating_sub(tdl).div_ceil(self.tx_words()).max(1) as u64;
            next = Some(self.tx_to + frames * period);
        }
        if self.xfer & XFER_RXS != 0 && self.dmacr & DMACR_RDE != 0 {
            let rdl = (self.dmacr >> 16 & 0x1f) as usize;
            let frames = (rdl + 1).saturating_sub(self.rx_level).div_ceil(self.rx_words()).max(1) as u64;
            let t = self.rx_to + frames * period;
            next = Some(next.map_or(t, |n| n.min(t)));
        }
        next
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 48 kHz from a 12.288 MHz MCLK (bclk = 64 fs: MDIV 3, TSD 63); 16-bit stereo packed
    /// in one word (HWT); frames leave at the sample rate and DMA requests follow TDL.
    #[test]
    fn plays_at_the_sample_rate() {
        let mut i = I2s { capture: true, ..I2s::default() };
        i.write(CKR, 3 << 16 | 63 << 8 | 63, 0);
        i.write(TXCR, TXCR_HWT | 15, 0);
        i.write(DMACR, DMACR_TDE | 16, 0);
        assert!(i.tx_request());
        for k in 0..32u32 {
            i.write(TXDR, (k << 16) | k, 0);
        }
        assert!(!i.tx_request());
        i.write(XFER, XFER_TXS, 0);
        i.update(1_000_000_000 / 48_000 * 10 + 1, 12_288_000);
        assert_eq!(i.rate, 48_000);
        assert_eq!(i.frames, 10);
        assert_eq!(&i.out[..4], &[0, 0, 1, 1]);
        assert_eq!(i.read(FIFOLR) & 0x3f, 22);
        assert!(!i.tx_request());
        let t = i.next_deadline().unwrap();
        i.update(t, 12_288_000);
        assert!(i.tx_request(), "at the watermark");
    }

    /// With the codec as clock master, 256 fs from the MCLK it is given.
    #[test]
    fn slave_mode_follows_the_codec() {
        let mut i = I2s::default();
        i.write(CKR, CKR_MSS_SLAVE | 0x00071f1f, 0);
        i.update(0, 11_289_600);
        assert_eq!(i.rate, 44_100);
    }
}
