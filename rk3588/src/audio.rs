//! The Orange Pi 5's two audio outputs and what feeds them:
//!
//! - the headphone jack: the ES8388 codec on I2C6 (`es8388`), its I2S on I2S1
//!   (`i2s_tdm`, the codec masters the clocks), fed by DMAC0 (PL330) requests 2 (TX) and 3 (RX);
//! - HDMI0: I2S5 (clock master, from a fixed 98.304 MHz MCLK), fed by DMAC2 request 2. The
//!   HDMI transmitter itself is not modelled (the firmware's framebuffer drives the display):
//!   its I2S input is a plain transmitter codec in the DT, and what I2S5 sends is what the
//!   monitor plays.
//!
//! The DMA controllers run when a peripheral request rises; the I2S FIFOs drain at the
//! sample rate in virtual time.

use crate::es8388::{self, Es8388};
use crate::i2c::Rk3xI2c;
use crate::i2s_tdm::{self, I2sTdm};
use peripherals::pl330::{Pl330, Pl330Bus};

pub const DMAC0: u64 = 0xfea1_0000;
/// DMAC2 (DMAC1, at 0xfea30000, serves no modelled device and is left out).
pub const DMAC2: u64 = 0xfed1_0000;
pub const DMAC_SIZE: u64 = 0x4000;
/// Each DMA controller's two interrupt lines (events 0 and 1).
pub const DMAC_IRQS: [[usize; 2]; 2] = [[32 + 86, 32 + 87], [32 + 90, 32 + 91]];
pub const I2S1: u64 = 0xfe48_0000;
pub const I2S5: u64 = 0xfddf_0000;
pub const I2S1_IRQ: usize = 32 + 181;
pub const I2S5_IRQ: usize = 32 + 185;
pub const I2C6: u64 = 0xfec8_0000;
pub const I2C6_IRQ: usize = 32 + 323;
/// The I2S controllers' MCLK (a fixed clock in the DT): 2048 x 48 kHz, so 44.1 kHz and
/// 48 kHz rates get dividers close enough to tell apart.
pub const I2S_MCLK: u64 = 98_304_000;
/// DMA request numbers.
pub const PERI_I2S1_TX: u8 = 2;
pub const PERI_I2S1_RX: u8 = 3;
pub const PERI_I2S5_TX: u8 = 2;

pub struct Audio {
    /// DMAC0 and DMAC2
    pub dmac: [Pl330; 2],
    /// I2S1 (headphones) and I2S5 (HDMI0)
    pub i2s1: I2sTdm,
    pub i2s5: I2sTdm,
    pub i2c6: Rk3xI2c,
    /// virtual time (ns) of the last update
    now: u64,
}

impl Default for Audio {
    fn default() -> Self {
        let mut i2c6 = Rk3xI2c::default();
        i2c6.attach(es8388::ADDRESS, Box::new(Es8388::default()));
        Self { dmac: [Pl330::new(), Pl330::new()], i2s1: I2sTdm::default(), i2s5: I2sTdm::default(), i2c6, now: 0 }
    }
}

/// What a DMA controller reaches: DRAM and its I2S controller's FIFOs.
struct DmaBus<'a> {
    ram: &'a mut [u8],
    i2s: &'a mut I2sTdm,
    base: u64,
    /// the I2S's request numbers (TX, RX)
    peri: (u8, Option<u8>),
}

impl Pl330Bus for DmaBus<'_> {
    fn read(&mut self, addr: u32, buf: &mut [u8]) -> bool {
        if u64::from(addr) == self.base + i2s_tdm::RXDR {
            for w in buf.chunks_mut(4) {
                let v = self.i2s.read(i2s_tdm::RXDR).to_le_bytes();
                w.copy_from_slice(&v[..w.len()]);
            }
            return true;
        }
        let start = addr as usize;
        match self.ram.get(start..start + buf.len()) {
            Some(s) => {
                buf.copy_from_slice(s);
                true
            }
            None => false,
        }
    }

    fn write(&mut self, addr: u32, data: &[u8]) -> bool {
        if u64::from(addr) == self.base + i2s_tdm::TXDR {
            for w in data.chunks(4) {
                let mut b = [0u8; 4];
                b[..w.len()].copy_from_slice(w);
                self.i2s.push(u32::from_le_bytes(b));
            }
            return true;
        }
        let start = addr as usize;
        match self.ram.get_mut(start..start + data.len()) {
            Some(s) => {
                s.copy_from_slice(data);
                true
            }
            None => false,
        }
    }

    fn request(&mut self, peri: u8) -> bool {
        if peri == self.peri.0 {
            self.i2s.tx_request()
        } else if Some(peri) == self.peri.1 {
            self.i2s.rx_request()
        } else {
            false
        }
    }
}

impl Audio {
    /// The ES8388 on I2C6.
    pub fn codec(&mut self) -> &mut Es8388 {
        self.i2c6.device_mut::<Es8388>(es8388::ADDRESS).expect("ES8388 on I2C6")
    }

    /// Whether `addr` is one of the audio devices' registers.
    pub fn contains(addr: u64) -> bool {
        [(DMAC0, DMAC_SIZE), (DMAC2, DMAC_SIZE), (I2S1, i2s_tdm::SIZE), (I2S5, i2s_tdm::SIZE), (I2C6, 0x1000)]
            .iter()
            .any(|&(b, s)| (b..b + s).contains(&addr))
    }

    pub fn read(&mut self, addr: u64) -> u32 {
        match addr {
            a if (DMAC0..DMAC0 + DMAC_SIZE).contains(&a) => self.dmac[0].read(a - DMAC0),
            a if (DMAC2..DMAC2 + DMAC_SIZE).contains(&a) => self.dmac[1].read(a - DMAC2),
            a if (I2S1..I2S1 + i2s_tdm::SIZE).contains(&a) => self.i2s1.read(a - I2S1),
            a if (I2S5..I2S5 + i2s_tdm::SIZE).contains(&a) => self.i2s5.read(a - I2S5),
            a if (I2C6..I2C6 + 0x1000).contains(&a) => self.i2c6.read(a - I2C6),
            _ => 0,
        }
    }

    pub fn write(&mut self, addr: u64, value: u32, ram: &mut [u8]) {
        let now = self.now;
        match addr {
            a if (DMAC0..DMAC0 + DMAC_SIZE).contains(&a) => {
                let Audio { dmac, i2s1, .. } = self;
                dmac[0].write(a - DMAC0, value, &mut DmaBus { ram, i2s: i2s1, base: I2S1, peri: (PERI_I2S1_TX, Some(PERI_I2S1_RX)) });
            }
            a if (DMAC2..DMAC2 + DMAC_SIZE).contains(&a) => {
                let Audio { dmac, i2s5, .. } = self;
                dmac[1].write(a - DMAC2, value, &mut DmaBus { ram, i2s: i2s5, base: I2S5, peri: (PERI_I2S5_TX, None) });
            }
            a if (I2S1..I2S1 + i2s_tdm::SIZE).contains(&a) => self.i2s1.write(a - I2S1, value, now),
            a if (I2S5..I2S5 + i2s_tdm::SIZE).contains(&a) => self.i2s5.write(a - I2S5, value, now),
            a if (I2C6..I2C6 + 0x1000).contains(&a) => {
                self.i2c6.now_ns = now;
                self.i2c6.write(a - I2C6, value);
            }
            _ => {}
        }
    }

    /// Play up to virtual time `now` (ns), serving each DMA request when it rises (so a
    /// late call, as from the threaded machine's I/O loop, does not starve the FIFOs).
    pub fn service(&mut self, now: u64, ram: &mut [u8]) {
        self.now = self.now.max(now);
        let now = self.now;
        let mut codec_rate = None;
        if self.i2s1.active() {
            let playing = self.i2s1.playing();
            let codec = self.codec();
            // the DAC's rate while playing, else the ADC's (the two share LRCK on this board)
            let rate = if playing { codec.dac_rate(es8388::MCLK) } else { codec.adc_rate(es8388::MCLK) };
            codec_rate = codec.master().then_some(rate);
            let gain = codec.headphone_gain();
            self.i2s1.gain = gain;
        }
        let Audio { dmac, i2s1, i2s5, .. } = self;
        let [d0, d1] = dmac;
        play(d0, &mut DmaBus { ram, i2s: i2s1, base: I2S1, peri: (PERI_I2S1_TX, Some(PERI_I2S1_RX)) }, now, codec_rate);
        play(d1, &mut DmaBus { ram, i2s: i2s5, base: I2S5, peri: (PERI_I2S5_TX, None) }, now, None);
    }

    /// The interrupt lines: (GIC interrupt, level).
    pub fn lines(&self) -> impl Iterator<Item = (usize, bool)> + '_ {
        (0..2).flat_map(move |c| (0..2).map(move |l| (DMAC_IRQS[c][l], self.dmac[c].irq(l)))).chain([(I2C6_IRQ, self.i2c6.irq()), (I2S1_IRQ, false), (I2S5_IRQ, false)])
    }

    /// When an I2S FIFO next asks for DMA (virtual ns).
    pub fn next_event(&self) -> Option<u64> {
        [self.i2s1.next_deadline(), self.i2s5.next_deadline()].into_iter().flatten().min()
    }

    /// Keep what each output plays (for `--audio-out` and `--hdmi-audio-out`).
    pub fn capture(&mut self, headphones: bool, hdmi: bool) {
        self.i2s1.capture |= headphones;
        self.i2s5.capture |= hdmi;
    }
}

/// Run an I2S controller up to `now`, stopping at each of its DMA requests to let the
/// controller's waiting thread move data.
fn play(dmac: &mut Pl330, bus: &mut DmaBus, now: u64, codec: Option<u32>) {
    if bus.i2s.active() {
        bus.i2s.clocks(I2S_MCLK, codec);
    }
    loop {
        if dmac.waiting() {
            dmac.run(bus);
        }
        if !bus.i2s.active() {
            return;
        }
        let next = if dmac.waiting() { bus.i2s.next_deadline() } else { None };
        // A DMA program that has ended with its interrupt pending waits for the guest's
        // handler to start the next period. The threaded machine's I/O loop sees that
        // restart only on a later call, so rather than underrun, the I2S clock holds when
        // its FIFO runs dry (as an elastic buffer would).
        let pending = !dmac.waiting() && (0..2).any(|l| dmac.irq(l));
        let hold = if pending { bus.i2s.tx_dry_at() } else { None };
        let t = next.map_or(now, |d| d.min(now));
        let t = hold.map_or(t, |h| h.min(t));
        bus.i2s.update(t, I2S_MCLK, codec);
        if t >= now || hold.is_some_and(|h| h <= t) {
            if dmac.waiting() {
                dmac.run(bus);
            }
            return;
        }
    }
}

emu_core::snap_struct!(Audio { dmac, i2s1, i2s5, i2c6, now });
