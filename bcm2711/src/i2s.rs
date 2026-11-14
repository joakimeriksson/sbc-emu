//! The BCM2835 PCM/I²S controller (`brcm,bcm2835-i2s` at 0xfe203000) as the bit clock and
//! frame master for a PCM5102A DAC (an I²S DAC HAT on GPIO 18-21), so playback ends up as
//! samples on the host.
//!
//! The transmit side consumes frames at the sample rate: bit clock (the firmware clock the
//! driver sets) / frame length (MODE.FLEN + 1). The DMA engine feeds the FIFO through DREQ 2
//! (`Soc::dma_pace`); the samples it delivers are decoded per the channel widths (packed 2 x 16
//! bits per word, or one word per channel) into 16-bit stereo in `out`.

pub const DREQ_TX: u32 = 2;

// CS_A
const EN: u32 = 1 << 0;
const TXON: u32 = 1 << 2;
const TXCLR: u32 = 1 << 3;
const RXCLR: u32 = 1 << 4;
const DMAEN: u32 = 1 << 9;
const TXERR: u32 = 1 << 15;
const RXERR: u32 = 1 << 16;
const TXW: u32 = 1 << 17;
const TXD: u32 = 1 << 19;
const TXE: u32 = 1 << 21;
const SYNC: u32 = 1 << 24;
// MODE_A
const FTXP: u32 = 1 << 24;
const CLKDIS: u32 = 1 << 28;

pub struct I2s {
    cs: u32,
    mode: u32,
    rxc: u32,
    txc: u32,
    dreq: u32,
    inten: u32,
    intstc: u32,
    gray: u32,
    /// the bit clock (Hz), as set through the firmware clock
    pub bclk: u32,
    /// time (ns) up to which frames have been played
    played_to: u64,
    /// fractional frames carried between ticks (in ns * rate units)
    frac: u64,
    /// words of the current frame still to come (non-packed: channel 2 after channel 1)
    half: Option<i16>,
    /// played samples, 16-bit interleaved stereo; the host drains it
    pub out: Vec<i16>,
    /// the sample rate of what is in `out`
    pub rate: u32,
    pub frames: u64,
}

impl Default for I2s { fn default() -> Self { Self::new() } }

impl I2s {
    pub fn new() -> Self {
        I2s { cs: 0, mode: 0, rxc: 0, txc: 0, dreq: 0, inten: 0, intstc: 0, gray: 0, bclk: 0, played_to: 0, frac: 0, half: None, out: Vec::new(), rate: 0, frames: 0 }
    }

    fn running(&self) -> bool { self.cs & (EN | TXON) == EN | TXON && self.mode & CLKDIS == 0 && self.bclk != 0 }

    /// Frames per second while transmitting.
    pub fn sample_rate(&self) -> u32 {
        let flen = ((self.mode >> 10) & 0x3ff) + 1;
        if self.bclk == 0 || flen < 2 { 0 } else { self.bclk / flen }
    }

    /// FIFO words per frame: one if both channels are packed into a word, else one per channel.
    fn words_per_frame(&self) -> u64 {
        if self.mode & FTXP != 0 { 1 } else { ((self.txc >> 30) & 1) as u64 + ((self.txc >> 14) & 1) as u64 }
    }

    fn width(ch: u32) -> u32 { 8 + (ch & 0xf) + if ch & (1 << 15) != 0 { 16 } else { 0 } }

    /// How many FIFO words the transmitter takes between the last call and `now` (the DREQ
    /// the DMA engine paces itself by); `now` moves the play position.
    pub fn tx_demand(&mut self, now: u64) -> u64 {
        if !self.running() || self.cs & DMAEN == 0 {
            self.played_to = now;
            self.frac = 0;
            return 0;
        }
        let rate = self.sample_rate() as u64;
        if now <= self.played_to { return 0; }
        let num = (now - self.played_to) * rate + self.frac;
        let frames = num / 1_000_000_000;
        self.frac = num % 1_000_000_000;
        self.played_to = now;
        frames * self.words_per_frame()
    }

    /// A word for the transmit FIFO (from the DMA engine or the CPU).
    pub fn push(&mut self, w: u32) {
        if self.cs & (EN | TXON) != EN | TXON { return; }
        let rate = self.sample_rate();
        if rate != self.rate { self.rate = rate; }
        let to16 = |v: u32, bits: u32| -> i16 { if bits >= 16 { (v << (32 - bits.min(32)) >> 16) as i16 } else { ((v << (32 - bits)) >> 16) as i16 } };
        if self.mode & FTXP != 0 {
            self.out.push(w as u16 as i16);
            self.out.push((w >> 16) as u16 as i16);
            self.frames += 1;
        } else {
            let (c1, c2) = (self.txc >> 16, self.txc & 0xffff);
            match self.half.take() {
                None if c2 & (1 << 14) != 0 => self.half = Some(to16(w, Self::width(c1))),
                None => { let s = to16(w, Self::width(c1)); self.out.push(s); self.out.push(s); self.frames += 1; }
                Some(l) => { self.out.push(l); self.out.push(to16(w, Self::width(c2))); self.frames += 1; }
            }
        }
        // keep at most ~10 s if nobody drains it
        if self.out.len() > 2 * 48_000 * 10 { self.out.drain(..self.out.len() / 2); }
    }

    pub fn read(&self, off: u64) -> u32 {
        match off {
            0x00 => {
                // the FIFO is always ready for more (the DMA engine paces itself); SYNC reads back
                let mut cs = self.cs & !(TXW | TXD | TXE);
                if self.cs & EN != 0 { cs |= TXW | TXD | TXE; }
                cs
            }
            0x08 => self.mode,
            0x0c => self.rxc,
            0x10 => self.txc,
            0x14 => self.dreq,
            0x18 => self.inten,
            0x1c => self.intstc,
            0x20 => self.gray,
            _ => 0,
        }
    }

    pub fn write(&mut self, off: u64, v: u32) {
        match off {
            0x00 => {
                let keep_err = self.cs & (TXERR | RXERR) & !(v & (TXERR | RXERR)); // write 1 to clear
                if v & TXCLR != 0 { self.half = None; }
                let _ = RXCLR;
                self.cs = (v & !(TXCLR | RXCLR | TXERR | RXERR)) | keep_err;
                let _ = SYNC; // SYNC is plain read/write here: it reads back what was written
            }
            0x04 => self.push(v),
            0x08 => self.mode = v,
            0x0c => self.rxc = v,
            0x10 => self.txc = v,
            0x14 => self.dreq = v,
            0x18 => self.inten = v,
            0x1c => self.intstc &= !v,
            0x20 => self.gray = v,
            _ => {}
        }
    }

    pub fn save_state(&self, w: &mut emu_core::snap::W) {
        w.tag("i2s-v1");
        for v in [self.cs, self.mode, self.rxc, self.txc, self.dreq, self.inten, self.intstc, self.gray, self.bclk] { w.u32(v); }
        w.u64(self.played_to); w.u64(self.frac);
    }
    pub fn load_state(&mut self, r: &mut emu_core::snap::R) -> Result<(), emu_core::snap::SnapError> {
        r.tag("i2s-v1")?;
        for v in [&mut self.cs, &mut self.mode, &mut self.rxc, &mut self.txc, &mut self.dreq, &mut self.inten, &mut self.intstc, &mut self.gray, &mut self.bclk] { *v = r.u32()?; }
        self.played_to = r.u64()?; self.frac = r.u64()?;
        Ok(())
    }
}
