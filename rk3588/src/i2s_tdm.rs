//! The RK3588's I2S/TDM controller (`rockchip,rk3588-i2s-tdm`), as Linux's
//! `rockchip_i2s_tdm` drives it in I2S mode, so what the guest plays ends up as samples on
//! the host. The Orange Pi 5 uses I2S1 for the ES8388 headphone codec and I2S5 for HDMI0.
//!
//! The transmit side consumes frames at the sample rate. As the clock master (CKR.MSS),
//! MCLK / (CLKDIV.TXM + 1) is the bit clock and / (CKR.TSD + 1) the frame rate, snapped to
//! the standard rate it approximates (the MCLK is a fixed clock here, where the real CRU
//! makes the exact one). As a slave, the codec's frame rate is given by the board. Each frame
//! takes one FIFO word per channel (TXCR.CSR: 2-8 channels), except that 16-bit samples come
//! two to a word (left in the low half) unless TXCR.HWT asks for one per word; the first two
//! channels are kept as 16-bit stereo. The receive
//! side produces silent frames at the same rate. The FIFOs are 32 words; DMA requests follow
//! DMACR's watermarks (transmit: level <= TDL, receive: level > RDL).

use std::collections::VecDeque;

pub const SIZE: u64 = 0x1000;

const TXCR: u64 = 0x00;
const RXCR: u64 = 0x04;
const CKR: u64 = 0x08;
const TXFIFOLR: u64 = 0x0c;
const DMACR: u64 = 0x10;
const INTCR: u64 = 0x14;
const INTSR: u64 = 0x18;
const XFER: u64 = 0x1c;
const CLR: u64 = 0x20;
pub const TXDR: u64 = 0x24;
pub const RXDR: u64 = 0x28;
const RXFIFOLR: u64 = 0x2c;
const TDM_TXCR: u64 = 0x30;
const TDM_RXCR: u64 = 0x34;
const CLKDIV: u64 = 0x38;

const XFER_TXS: u32 = 1 << 0;
const XFER_RXS: u32 = 1 << 1;
const DMACR_TDE: u32 = 1 << 8;
const DMACR_RDE: u32 = 1 << 24;
const CKR_MSS_SLAVE: u32 = 1 << 27;
const TXCR_HWT: u32 = 1 << 14;
const FIFO_WORDS: usize = 32;
/// The sample rate assumed when nothing gives one (a slave with no codec clock).
const DEFAULT_RATE: u32 = 48_000;
/// The rates ALSA uses; a master's divided clock is snapped to the nearest.
const RATES: [u32; 18] = [8000, 11025, 12000, 16000, 22050, 24000, 32000, 44100, 48000, 64000, 88200, 96000, 176400, 192000, 352800, 384000, 705600, 768000];

pub struct I2sTdm {
    txcr: u32,
    rxcr: u32,
    ckr: u32,
    dmacr: u32,
    intcr: u32,
    xfer: u32,
    tdm_txcr: u32,
    tdm_rxcr: u32,
    clkdiv: u32,
    tx: VecDeque<u32>,
    rx_level: usize,
    /// Virtual time (ns) up to which transmit and receive frames are accounted for.
    tx_to: u64,
    rx_to: u64,
    /// The frame rate in Hz at the last update.
    pub rate: u32,
    /// What the analogue path after the DAC does to each channel (1.0 for HDMI).
    pub gain: [f32; 2],
    /// Collect the played samples in `out` (16-bit stereo, interleaved).
    pub capture: bool,
    pub out: Vec<i16>,
    pub frames: u64,
    pub underruns: u64,
}

impl Default for I2sTdm {
    fn default() -> Self {
        // the reset values Linux's regmap assumes
        Self {
            txcr: 0x7200_000f,
            rxcr: 0x01c8_000f,
            ckr: 0x0000_1f1f,
            dmacr: 0x001f_0000,
            intcr: 0x01f0_0000,
            xfer: 0,
            tdm_txcr: 0x3eff,
            tdm_rxcr: 0x3eff,
            clkdiv: 0x0707,
            tx: VecDeque::new(),
            rx_level: 0,
            tx_to: 0,
            rx_to: 0,
            rate: 0,
            gain: [1.0; 2],
            capture: false,
            out: Vec::new(),
            frames: 0,
            underruns: 0,
        }
    }
}

impl I2sTdm {
    pub fn read(&mut self, offset: u64) -> u32 {
        match offset {
            TXCR => self.txcr,
            RXCR => self.rxcr,
            CKR => self.ckr,
            TXFIFOLR => self.tx.len().min(FIFO_WORDS) as u32,
            DMACR => self.dmacr,
            INTCR => self.intcr,
            INTSR => 0,
            XFER => self.xfer,
            CLR => 0, // clears complete at once
            RXDR => {
                self.rx_level = self.rx_level.saturating_sub(1);
                0
            }
            RXFIFOLR => self.rx_level.min(FIFO_WORDS) as u32,
            TDM_TXCR => self.tdm_txcr,
            TDM_RXCR => self.tdm_rxcr,
            CLKDIV => self.clkdiv,
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
            TDM_TXCR => self.tdm_txcr = value,
            TDM_RXCR => self.tdm_rxcr = value,
            CLKDIV => self.clkdiv = value,
            _ => {}
        }
    }

    /// A word into the transmit FIFO (from the CPU or the DMA controller).
    pub fn push(&mut self, word: u32) {
        if self.tx.len() < FIFO_WORDS {
            self.tx.push_back(word);
        }
    }

    /// Transmit is running.
    pub fn playing(&self) -> bool {
        self.xfer & XFER_TXS != 0
    }

    /// Transmit or receive is running.
    pub fn active(&self) -> bool {
        self.xfer != 0
    }

    /// The frame rate: from the dividers of MCLK `mclk` Hz as the master, else `codec` (the
    /// rate of the codec that masters the clocks).
    fn frame_rate(&self, mclk: u64, codec: Option<u32>) -> u32 {
        if self.ckr & CKR_MSS_SLAVE != 0 {
            return codec.unwrap_or(DEFAULT_RATE);
        }
        let bclk_div = u64::from(self.clkdiv & 0xff) + 1;
        let lrck_div = u64::from(self.ckr & 0xff) + 1;
        let raw = (mclk / bclk_div / lrck_div) as u32;
        RATES.into_iter().min_by_key(|r| r.abs_diff(raw)).unwrap_or(DEFAULT_RATE)
    }

    fn tx_channels(&self) -> usize {
        2 * (((self.txcr >> 15) & 3) as usize + 1)
    }

    fn sample_bits(&self) -> u32 {
        (self.txcr & 0x1f) + 1
    }

    /// 16-bit samples are packed two to a word (HWT clear: Linux's S16_LE frames as the DMA
    /// moves them).
    fn packed(&self) -> bool {
        self.txcr & TXCR_HWT == 0 && self.sample_bits() <= 16
    }

    /// FIFO words per transmitted frame.
    fn tx_words(&self) -> usize {
        if self.packed() { self.tx_channels() / 2 } else { self.tx_channels() }
    }

    fn rx_words(&self) -> usize {
        let channels = 2 * (((self.rxcr >> 15) & 3) as usize + 1);
        if self.rxcr & TXCR_HWT == 0 && (self.rxcr & 0x1f) < 16 { channels / 2 } else { channels }
    }

    /// Take the frame rate from the clocks (before asking for `next_deadline`).
    pub fn clocks(&mut self, mclk: u64, codec: Option<u32>) {
        self.rate = self.frame_rate(mclk, codec);
    }

    /// Play and record frames up to virtual time `now` (ns), with MCLK `mclk` Hz or the
    /// codec's frame rate `codec`.
    pub fn update(&mut self, now: u64, mclk: u64, codec: Option<u32>) {
        self.clocks(mclk, codec);
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
                    let scale = |s: i16, g: f32| (f32::from(s) * g).round().clamp(-32768.0, 32767.0) as i16;
                    self.out.extend_from_slice(&[scale(l, self.gain[0]), scale(r, self.gain[1])]);
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
        if self.packed() {
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

    /// When the transmit FIFO runs dry (virtual ns), while it plays from DMA: the time its
    /// last whole frame has gone out.
    pub fn tx_dry_at(&self) -> Option<u64> {
        if self.xfer & XFER_TXS == 0 || self.dmacr & DMACR_TDE == 0 {
            return None;
        }
        let period = 1_000_000_000 / u64::from(self.rate.max(1));
        Some(self.tx_to + (self.tx.len() / self.tx_words()) as u64 * period)
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

    /// Master at 48 kHz from 98.304 MHz (bclk = 64 fs: TXM 31, TSD 63), 16-bit stereo two
    /// samples to a word as Linux sets it up; frames leave at the sample rate, DMA requests
    /// follow TDL and the gain applies.
    #[test]
    fn plays_at_the_sample_rate() {
        let mut i = I2sTdm { capture: true, gain: [1.0, 0.5], ..I2sTdm::default() };
        i.write(CLKDIV, 31, 0);
        i.write(CKR, 63, 0);
        i.write(TXCR, 15, 0);
        i.write(DMACR, DMACR_TDE | 16, 0);
        assert!(i.tx_request());
        for k in 0..32u32 {
            i.write(TXDR, (200 * k) << 16 | 100 * k, 0);
        }
        assert!(!i.tx_request());
        i.write(XFER, XFER_TXS, 0);
        i.update(1_000_000_000 / 48_000 * 10 + 1, 98_304_000, None);
        assert_eq!(i.rate, 48_000);
        assert_eq!(i.frames, 10);
        assert_eq!(&i.out[..4], &[0, 0, 100, 100]);
        assert_eq!(i.read(TXFIFOLR), 22);
        assert!(!i.tx_request());
        let t = i.next_deadline().unwrap();
        i.update(t, 98_304_000, None);
        assert!(i.tx_request(), "at the watermark");
        // 24-bit samples: a word per channel, the top 16 bits kept
        i.write(TXCR, 23, 0);
        i.write(CLR, 1, 0);
        i.write(TXDR, 0x12_3456, 0);
        i.write(TXDR, 0xff_ff00, 0);
        i.out.clear();
        i.update(t + 1_000_000_000 / 48_000, 98_304_000, None);
        assert_eq!(&i.out[..], &[0x1234, -1]);
        i.write(XFER, 0, 0);
        i.write(CLR, 1, 0);
        assert_eq!(i.read(CLR), 0);
        assert_eq!(i.read(TXFIFOLR), 0);
    }

    /// 44.1 kHz does not divide 98.304 MHz: the nearest divider is snapped back to it.
    #[test]
    fn master_rate_snaps_to_the_standard_rate() {
        let mut i = I2sTdm::default();
        i.write(CLKDIV, 34, 0); // round(98.304 MHz / (64 * 44.1 kHz)) = 35
        i.write(CKR, 63, 0);
        i.update(0, 98_304_000, None);
        assert_eq!(i.rate, 44_100);
    }

    /// With the codec as clock master, its rate.
    #[test]
    fn slave_mode_follows_the_codec() {
        let mut i = I2sTdm::default();
        i.write(CKR, CKR_MSS_SLAVE | 0x1f1f, 0);
        i.update(0, 98_304_000, Some(16_000));
        assert_eq!(i.rate, 16_000);
    }
}
