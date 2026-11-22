//! host1x (`nvidia,tegra210-host1x`): the 192 sync points, their threshold interrupts and the 14
//! command channels. Channels run their push buffers at once when the driver moves the put pointer;
//! of what is in them only sync point increments have an effect (no client engine is modelled
//! behind the channels), and waits do not block.
use crate::smmu::{Group, Smmu};
use peripherals::dma::Memory;

pub const BASE: u64 = 0x5000_0000;
pub const SIZE: u64 = 0x3_8000;
/// The sync point interrupt.
pub const IRQ: usize = 32 + 0x41;
pub const POINTS: usize = 192;
const CHANNELS: usize = 14;
const STRIDE: u64 = 0x4000;
const SYNC: u64 = 0x2100;

const DMASTART: usize = 0x14 / 4;
const DMAPUT: usize = 0x18 / 4;
const DMAGET: usize = 0x1c / 4;
const DMACTRL: usize = 0x24 / 4;

pub struct Host1x {
    /// every register, read back
    regs: std::collections::HashMap<u64, u32>,
    channels: [[u32; 64]; CHANNELS],
    class: [u32; CHANNELS],
    pub points: [u32; POINTS],
    threshold: [u32; POINTS],
    enabled: [u32; POINTS / 32],
    status: [u32; POINTS / 32],
}

impl Default for Host1x {
    fn default() -> Self {
        Self { regs: Default::default(), channels: [[0; 64]; CHANNELS], class: [0; CHANNELS], points: [0; POINTS], threshold: [0; POINTS], enabled: [0; POINTS / 32], status: [0; POINTS / 32] }
    }
}

/// The most words one run of a channel takes.
const MAX_WORDS: usize = 1 << 18;

impl Host1x {
    pub fn increment(&mut self, point: usize) {
        if let Some(p) = self.points.get_mut(point) {
            *p = p.wrapping_add(1);
            self.check(point);
        }
    }

    /// The interrupt of a point that has reached its threshold.
    fn check(&mut self, k: usize) {
        let reached = self.points[k].wrapping_sub(self.threshold[k]) as i32 >= 0;
        if reached && self.enabled[k / 32] & 1 << (k % 32) != 0 {
            self.status[k / 32] |= 1 << (k % 32);
        }
    }

    pub fn irq(&self) -> bool {
        self.status.iter().zip(&self.enabled).any(|(s, e)| s & e != 0)
    }

    pub fn read(&self, offset: u64) -> u32 {
        if let Some(o) = offset.checked_sub(SYNC).filter(|&o| o < STRIDE - SYNC) {
            let at = |base: u64, n: usize| o.checked_sub(base).map(|d| (d / 4) as usize).filter(|&k| k < n);
            if let Some(k) = at(0xf80, POINTS) {
                return self.points[k];
            }
            if let Some(k) = at(0xe80, POINTS / 32) {
                return self.status[k];
            }
            if let Some(k) = at(0x1380, POINTS) {
                return self.threshold[k];
            }
        } else if let Some(ch) = self.channels.get((offset / STRIDE) as usize) {
            if offset % STRIDE < 0x100 {
                return ch[(offset % STRIDE) as usize / 4];
            }
        }
        self.regs.get(&offset).copied().unwrap_or(0)
    }

    pub fn write(&mut self, offset: u64, v: u32, smmu: &Smmu, dma: &dyn Memory) {
        if let Some(o) = offset.checked_sub(SYNC).filter(|&o| o < STRIDE - SYNC) {
            let at = |base: u64, n: usize| o.checked_sub(base).map(|d| (d / 4) as usize).filter(|&k| k < n);
            if let Some(k) = at(0xf60, POINTS / 32) {
                (0..32).filter(|b| v >> b & 1 != 0).for_each(|b| self.increment(32 * k + b));
            } else if let Some(k) = at(0xf80, POINTS) {
                self.points[k] = v;
                self.check(k);
            } else if let Some(k) = at(0xe80, POINTS / 32) {
                self.status[k] &= !v;
            } else if let Some(k) = at(0xf00, POINTS / 32) {
                self.enabled[k] &= !v;
            } else if let Some(k) = at(0xf20, POINTS / 32) {
                self.enabled[k] |= v;
                (0..32).filter(|b| v >> b & 1 != 0).for_each(|b| self.check(32 * k + b));
            } else if let Some(k) = at(0x1380, POINTS) {
                self.threshold[k] = v;
                self.check(k);
            } else {
                self.regs.insert(offset, v);
            }
            return;
        }
        let (c, r) = ((offset / STRIDE) as usize, (offset % STRIDE) as usize / 4);
        if c < CHANNELS && r < 64 {
            self.channels[c][r] = v;
            match r {
                // DMAGETRST with DMAINITGET: get = put; without: get = 0
                DMACTRL if v & 2 != 0 => self.channels[c][DMAGET] = if v & 4 != 0 { self.channels[c][DMAPUT] } else { 0 },
                DMACTRL | DMAPUT => self.run(c, smmu, dma),
                _ => {}
            }
        } else {
            self.regs.insert(offset, v);
        }
    }

    /// Run a channel's push buffer.
    fn run(&mut self, c: usize, smmu: &Smmu, dma: &dyn Memory) {
        if self.channels[c][DMACTRL] & 1 != 0 {
            return; // stopped
        }
        let start = self.channels[c][DMASTART] as u64;
        let (mut get, put) = (self.channels[c][DMAGET], self.channels[c][DMAPUT]);
        let word = |a: u64| smmu.read_u32(Group::Host1x, a, dma);
        // two words per slot; the opcode's data words follow it in the stream
        let mut fifo = Vec::new();
        for _ in 0..1 << 16 {
            if get == put {
                break;
            }
            let w = word(start + get as u64);
            get = get.wrapping_add(4);
            match w >> 28 {
                5 => get = (w & 0xfff_ffff) << 4, // restart
                6 => {
                    // gather: the words are elsewhere
                    let a = word(start + get as u64) as u64;
                    get = get.wrapping_add(4);
                    let n = (w as usize & 0x3fff).min(MAX_WORDS - fifo.len().min(MAX_WORDS));
                    fifo.extend((0..n as u64).map(|k| word(a + 4 * k)));
                }
                _ => fifo.push(w),
            }
        }
        self.channels[c][DMAGET] = get;
        self.execute(c, &fifo);
    }

    fn execute(&mut self, c: usize, words: &[u32]) {
        let mut it = words.iter().copied();
        while let Some(w) = it.next() {
            let (reg, low) = (w >> 16 & 0xfff, w & 0xffff);
            match w >> 28 {
                0 => {
                    self.class[c] = w >> 6 & 0x3ff;
                    for b in (0..6).filter(|b| w >> b & 1 != 0) {
                        let Some(v) = it.next() else { return };
                        self.method(reg + b, v);
                    }
                }
                1 => for k in 0..low {
                    let Some(v) = it.next() else { return };
                    self.method(reg + k, v);
                },
                2 => for _ in 0..low {
                    let Some(v) = it.next() else { return };
                    self.method(reg, v);
                },
                3 => for b in (0..16).filter(|b| low >> b & 1 != 0) {
                    let Some(v) = it.next() else { return };
                    self.method(reg + b, v);
                },
                4 => self.method(reg, low),
                _ => {}
            }
        }
    }

    /// Method 0 of every class is INCR_SYNCPT.
    fn method(&mut self, reg: u32, v: u32) {
        if reg == 0 {
            self.increment(v as usize & 0xff);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use peripherals::dma::Dma;

    #[test]
    fn points_thresholds_and_channels() {
        let mut ram = vec![0u8; 0x4000];
        // a push buffer: set class with INCR_SYNCPT 7; then a gather of an INCR_SYNCPT 9
        for (o, w) in [(0x1000, 0x5d << 6 | 1u32), (0x1004, 7), (0x1008, 6 << 28 | 2), (0x100c, 0x8000_2000), (0x2000, 2 << 28 | 1), (0x2004, 9)] {
            ram[o..o + 4].copy_from_slice(&w.to_le_bytes());
        }
        let d = Dma { ram: &mut ram, base: 0x8000_0000 };
        let s = Smmu::default();
        let mut h = Host1x::default();
        h.write(SYNC + 0x1380 + 4 * 7, 1, &s, &d);
        h.write(SYNC + 0xf20, 1 << 7, &s, &d);
        assert!(!h.irq());
        let ch = 2 * STRIDE;
        for (o, v) in [(0x24, 1), (0x14, 0x8000_1000), (0x18, 0), (0x24, 7), (0x24, 0), (0x18, 16)] {
            h.write(ch + o, v, &s, &d);
        }
        assert_eq!((h.points[7], h.points[9], h.read(ch + 0x1c)), (1, 1, 16));
        assert!(h.irq());
        assert_eq!(h.read(SYNC + 0xe80), 1 << 7);
        h.write(SYNC + 0xf00, 1 << 7, &s, &d);
        h.write(SYNC + 0xe80, 1 << 7, &s, &d);
        assert!(!h.irq());
        h.write(SYNC + 0xf64, 1 << 1, &s, &d);
        assert_eq!(h.read(SYNC + 0xf80 + 4 * 33), 1, "a CPU increment");
    }
}
