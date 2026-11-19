//! The Orange Pi RV's analogue audio: the JH7110's PWM DAC (`starfive,jh7110-pwmdac`,
//! 0x100b0000) fed by the Synopsys DesignWare AXI DMA controller (`starfive,jh7110-axi-dma`,
//! 0x16050000), as Linux's `jh7110_pwmdac` and `dw-axi-dmac` drive them; what the guest plays
//! is kept as 16-bit stereo samples for the host (`--audio-out`).
//!
//! AXI DMAC: four channels, linked-list multi-block transfers. Enabling a channel (CHEN with
//! its write-enable bit) fetches the LLI at CH_LLP; the block's BLOCK_TS+1 items move from
//! SAR to DAR; then CH_LLP takes the LLI's next pointer, and a block marked LLI_LAST ends the
//! transfer: the channel disables itself and raises DMA_TRF (Linux's cyclic audio marks every
//! block last and re-enables the channel from its interrupt handler). Transfers to the PWM
//! DAC run at the DAC's sample rate; memory-to-memory transfers complete at once.
//!
//! PWM DAC: CTRL (enable, CNT_N, ...) and the WDATA FIFO. Its sample rate is the core clock
//! (PLL2 / audio_root / pwmdac_core dividers in the SYSCRG) / (256 * CNT_N), snapped to the
//! standard rate it approximates; 32-bit items are 16-bit stereo frames, 16-bit items mono.

pub const DMA_BASE: u64 = 0x1605_0000;
pub const DMA_SIZE: u64 = 0x1_0000;
pub const IRQ_DMA: usize = 73;
pub const PWMDAC_BASE: u64 = 0x100b_0000;
pub const PWMDAC_SIZE: u64 = 0x1000;

const NCH: usize = 4;
const DMAC_CFG: u64 = 0x10;
const DMAC_CHEN: u64 = 0x18;
const DMAC_INTSTATUS: u64 = 0x30;
const DMAC_RESET: u64 = 0x58;

const CH_SAR: u64 = 0x00;
const CH_DAR: u64 = 0x08;
const CH_BLOCK_TS: u64 = 0x10;
const CH_CTL: u64 = 0x18;
const CH_LLP: u64 = 0x28;
const CH_INTSTATUS_ENA: u64 = 0x80;
const CH_INTSTATUS: u64 = 0x88;
const CH_INTSIGNAL_ENA: u64 = 0x90;
const CH_INTCLEAR: u64 = 0x98;

const IRQ_BLOCK_TRF: u32 = 1 << 0;
const IRQ_DMA_TRF: u32 = 1 << 1;
const IRQ_LLI_RD_DEC_ERR: u32 = 1 << 9;
const LLI_LAST: u32 = 1 << 30;

/// A block being transferred.
#[derive(Clone, Copy, Debug)]
struct Block {
    sar: u64,
    dar: u64,
    items: u64,
    done: u64,
    width: u64,
    last: bool,
    next: u64,
}

#[derive(Default)]
struct Chan {
    regs: [u64; 32],
    enabled: bool,
    block: Option<Block>,
    intstatus: u32,
    /// fraction of a sample owed to the DAC (in 1/1e9 of a sample)
    owed: u64,
}

impl Chan {
    fn reg(&self, off: u64) -> u64 {
        self.regs[(off / 8) as usize & 31]
    }
}

#[derive(Default)]
pub struct Pwmdac {
    ctrl: u32,
    /// samples received (frames)
    pub frames: u64,
    /// keep what is played (16-bit stereo, interleaved) for the host
    pub capture: bool,
    pub samples: Vec<i16>,
    /// the rate of the samples kept
    pub rate: u32,
}

impl Pwmdac {
    pub fn read(&self, off: u64) -> u32 {
        if off == 4 { self.ctrl } else { 0 }
    }

    pub fn write(&mut self, off: u64, v: u32, rate: u32) {
        match off {
            0 => self.play(&v.to_le_bytes(), 4, rate),
            4 => self.ctrl = v,
            _ => {}
        }
    }

    fn enabled(&self) -> bool {
        self.ctrl & 1 != 0
    }


    /// Samples into the DAC: `width` 4 = 16-bit stereo frames, 2 = 16-bit mono.
    fn play(&mut self, data: &[u8], width: u64, rate: u32) {
        self.frames += data.len() as u64 / width.max(1);
        if !self.capture {
            return;
        }
        self.rate = rate;
        let pcm = data.as_chunks::<2>().0.iter().map(|b| i16::from_le_bytes([b[0], b[1]]));
        if width >= 4 {
            self.samples.extend(pcm);
        } else {
            // mono: the same sample on both channels
            self.samples.extend(pcm.flat_map(|s| [s, s]));
        }
    }
}

#[derive(Default)]
pub struct AxiDmac {
    cfg: u32,
    ch: [Chan; NCH],
    /// time of the last update (ns)
    last: u64,
    pub transfers: u64,
}


/// The DAC's sample rate from its clocks: PLL2 / audio_root / pwmdac_core / (256 * CNT_N),
/// snapped to the nearest standard rate.
pub fn sample_rate(pll2_hz: u64, audio_root_div: u32, pwmdac_div: u32, cnt_n: u32) -> u32 {
    let core = pll2_hz / audio_root_div.max(1) as u64 / pwmdac_div.max(1) as u64;
    let raw = core / (256 * cnt_n.max(1) as u64);
    const RATES: [u32; 7] = [8000, 11025, 16000, 22050, 32000, 44100, 48000];
    RATES.into_iter().min_by_key(|&r| (r as i64 - raw as i64).abs()).unwrap()
}

impl AxiDmac {
    pub fn irq(&self) -> bool {
        self.cfg & 3 == 3 && self.ch.iter().any(|c| c.intstatus & c.reg(CH_INTSIGNAL_ENA) as u32 != 0)
    }

    pub fn read(&self, off: u64) -> u32 {
        match off {
            DMAC_CFG => self.cfg,
            DMAC_CHEN => self.ch.iter().enumerate().fold(0, |m, (k, c)| m | (c.enabled as u32) << k),
            DMAC_INTSTATUS => self.ch.iter().enumerate().fold(0, |m, (k, c)| m | ((c.intstatus != 0) as u32) << k),
            DMAC_RESET => 0,
            0x100..=0x4ff => {
                let c = &self.ch[((off - 0x100) / 0x100) as usize];
                let r = off & 0xff;
                match r {
                    CH_INTSTATUS => c.intstatus,
                    _ => {
                        let v = c.reg(r & !7);
                        if r & 4 != 0 { (v >> 32) as u32 } else { v as u32 }
                    }
                }
            }
            _ => 0,
        }
    }

    pub fn write(&mut self, off: u64, v: u32, ram: &mut [u8], ram_base: u64, dac: &mut Pwmdac, rate: u32) {
        match off {
            DMAC_CFG => self.cfg = v,
            DMAC_CHEN => {
                for k in 0..NCH {
                    if v & (1 << (k + 8)) != 0 {
                        let on = v & (1 << k) != 0;
                        if on && !self.ch[k].enabled {
                            self.ch[k].enabled = true;
                            self.fetch(k, ram, ram_base);
                            self.run_now(k, ram, ram_base, dac, rate);
                        } else if !on {
                            self.ch[k].enabled = false;
                            self.ch[k].block = None;
                        }
                    }
                }
            }
            DMAC_RESET => {
                if v & 1 != 0 {
                    *self = AxiDmac { last: self.last, ..AxiDmac::default() };
                }
            }
            0x100..=0x4ff => {
                let c = &mut self.ch[((off - 0x100) / 0x100) as usize];
                let r = off & 0xff;
                match r {
                    CH_INTCLEAR => c.intstatus &= !v,
                    CH_INTSTATUS => {}
                    _ => {
                        let slot = &mut c.regs[((r & !7) / 8) as usize & 31];
                        *slot = if r & 4 != 0 {
                            (*slot & 0xffff_ffff) | (v as u64) << 32
                        } else {
                            (*slot & !0xffff_ffff) | v as u64
                        };
                    }
                }
            }
            _ => {}
        }
    }

    fn raise(&mut self, k: usize, bits: u32) {
        let c = &mut self.ch[k];
        c.intstatus |= bits & (c.reg(CH_INTSTATUS_ENA) as u32 | IRQ_DMA_TRF);
    }

    /// Load the LLI at CH_LLP into the channel.
    fn fetch(&mut self, k: usize, ram: &[u8], ram_base: u64) {
        let llp = self.ch[k].reg(CH_LLP) & !0x3f;
        let o = llp.wrapping_sub(ram_base) as usize;
        let Some(lli) = ram.get(o..o.wrapping_add(64)) else {
            self.ch[k].enabled = false;
            self.raise(k, IRQ_LLI_RD_DEC_ERR);
            return;
        };
        let q = |i: usize| u64::from_le_bytes(lli[i..i + 8].try_into().unwrap());
        let (sar, dar, block_ts, next) = (q(0), q(8), q(16) & 0x3f_ffff, q(24));
        let (ctl_lo, ctl_hi) = (q(32) as u32, (q(32) >> 32) as u32);
        let width = 1u64 << ((ctl_lo >> 8) & 7); // source width
        let c = &mut self.ch[k];
        c.regs[(CH_SAR / 8) as usize] = sar;
        c.regs[(CH_DAR / 8) as usize] = dar;
        c.regs[(CH_BLOCK_TS / 8) as usize] = block_ts;
        c.regs[(CH_CTL / 8) as usize] = q(32);
        c.block = Some(Block { sar, dar, items: block_ts + 1, done: 0, width, last: ctl_hi & LLI_LAST != 0, next });
    }

    fn to_dac(b: &Block) -> bool {
        (PWMDAC_BASE..PWMDAC_BASE + PWMDAC_SIZE).contains(&b.dar)
    }

    /// Memory-to-memory blocks complete as soon as the channel starts.
    fn run_now(&mut self, k: usize, ram: &mut [u8], ram_base: u64, dac: &mut Pwmdac, rate: u32) {
        while let Some(b) = self.ch[k].block {
            if Self::to_dac(&b) {
                return;
            }
            let n = (b.items * b.width) as usize;
            let (s, d) = (b.sar.wrapping_sub(ram_base) as usize, b.dar.wrapping_sub(ram_base) as usize);
            if s.checked_add(n).is_some_and(|e| e <= ram.len()) && d.checked_add(n).is_some_and(|e| e <= ram.len()) {
                ram.copy_within(s..s + n, d);
            }
            self.finish_block(k, ram, ram_base, dac, rate);
        }
    }

    fn finish_block(&mut self, k: usize, ram: &mut [u8], ram_base: u64, dac: &mut Pwmdac, rate: u32) {
        let Some(b) = self.ch[k].block.take() else { return };
        self.transfers += 1;
        self.ch[k].regs[(CH_LLP / 8) as usize] = b.next;
        self.raise(k, IRQ_BLOCK_TRF);
        if b.last {
            self.ch[k].enabled = false;
            self.raise(k, IRQ_DMA_TRF);
        } else {
            self.fetch(k, ram, ram_base);
            self.run_now(k, ram, ram_base, dac, rate);
        }
    }

    /// Move the samples due by `now` (ns) into the DAC. Returns the next deadline while
    /// a transfer to the DAC runs.
    pub fn update(&mut self, now: u64, ram: &mut [u8], ram_base: u64, dac: &mut Pwmdac, rate: u32) -> Option<u64> {
        let elapsed = now.saturating_sub(self.last);
        self.last = now;
        let mut next = None;
        for k in 0..NCH {
            let Some(b) = self.ch[k].block else { continue };
            if !self.ch[k].enabled || !Self::to_dac(&b) || !dac.enabled() {
                continue;
            }
            let c = &mut self.ch[k];
            c.owed += elapsed * rate as u64;
            let due = c.owed / 1_000_000_000;
            c.owed %= 1_000_000_000;
            let n = due.min(b.items - b.done);
            if n > 0 {
                let s = (b.sar + b.done * b.width).wrapping_sub(ram_base) as usize;
                let len = (n * b.width) as usize;
                if let Some(data) = ram.get(s..s + len) {
                    let data = data.to_vec();
                    dac.play(&data, b.width, rate);
                }
                self.ch[k].block.as_mut().unwrap().done += n;
            }
            if self.ch[k].block.is_some_and(|b| b.done >= b.items) {
                self.finish_block(k, ram, ram_base, dac, rate);
            }
            if let Some(b) = self.ch[k].block.filter(|_| self.ch[k].enabled) {
                let left = b.items - b.done;
                next = Some(next.unwrap_or(u64::MAX).min(now + (left * 1_000_000_000).div_ceil(rate as u64)));
            }
        }
        next
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const BASE: u64 = 0x4000_0000;

    fn lli(ram: &mut [u8], at: u64, sar: u64, dar: u64, items: u64, next: u64, last: bool) {
        let o = (at - BASE) as usize;
        ram[o..o + 8].copy_from_slice(&sar.to_le_bytes());
        ram[o + 8..o + 16].copy_from_slice(&dar.to_le_bytes());
        ram[o + 16..o + 24].copy_from_slice(&(items - 1).to_le_bytes());
        ram[o + 24..o + 32].copy_from_slice(&next.to_le_bytes());
        let ctl = (2u64 << 8) | (2 << 11) | ((1u64 << 31) | if last { 1u64 << 30 } else { 0 }) << 32;
        ram[o + 32..o + 40].copy_from_slice(&ctl.to_le_bytes());
    }

    #[test]
    fn sample_rates_from_the_clock_dividers() {
        assert_eq!(sample_rate(1_188_000_000, 1, 97, 1), 48000);
        assert_eq!(sample_rate(1_188_000_000, 1, 106, 1), 44100);
        assert_eq!(sample_rate(1_188_000_000, 1, 194, 3), 8000);
    }

    #[test]
    fn cyclic_blocks_to_the_dac_at_the_sample_rate() {
        let mut ram = vec![0u8; 0x10000];
        let mut dac = Pwmdac::default();
        dac.write(4, 1 | (1 << 4), 48000);
        let mut d = AxiDmac::default();
        // two 480-frame periods, each block marked last, chained in a ring
        lli(&mut ram, BASE + 0x100, BASE + 0x1000, PWMDAC_BASE, 480, BASE + 0x140, true);
        lli(&mut ram, BASE + 0x140, BASE + 0x1000 + 1920, PWMDAC_BASE, 480, BASE + 0x100, true);
        let ch0 = 0x100;
        d.write(DMAC_CFG, 3, &mut ram, BASE, &mut dac, 48000);
        d.write(ch0 + CH_LLP, (BASE + 0x100) as u32, &mut ram, BASE, &mut dac, 48000);
        d.write(ch0 + CH_LLP + 4, 0, &mut ram, BASE, &mut dac, 48000);
        d.write(ch0 + CH_INTSIGNAL_ENA, IRQ_DMA_TRF, &mut ram, BASE, &mut dac, 48000);
        d.write(DMAC_CHEN, 0x101, &mut ram, BASE, &mut dac, 48000);
        assert_eq!(d.read(DMAC_CHEN) & 1, 1);
        // 5 ms: 240 frames, half a period
        assert!(d.update(5_000_000, &mut ram, BASE, &mut dac, 48000).is_some());
        assert_eq!(dac.frames, 240);
        assert!(!d.irq());
        // 10 ms: the first period is done; the channel stopped with DMA_TRF
        d.update(10_000_000, &mut ram, BASE, &mut dac, 48000);
        assert_eq!(dac.frames, 480);
        assert!(d.irq());
        assert_eq!(d.read(DMAC_CHEN) & 1, 0);
        assert_eq!(d.read(ch0 + CH_LLP) as u64, BASE + 0x140);
        // the driver acknowledges and re-enables: the second period follows
        d.write(ch0 + CH_INTCLEAR, !0, &mut ram, BASE, &mut dac, 48000);
        d.write(DMAC_CHEN, 0x101, &mut ram, BASE, &mut dac, 48000);
        d.update(20_000_000, &mut ram, BASE, &mut dac, 48000);
        assert_eq!(dac.frames, 960);
    }
}
