//! The APB DMA controller (`nvidia,tegra210-apbdma`): 32 channels between memory and a
//! peripheral's FIFO. A channel moves words whenever its FIFO has or takes them; the machine
//! offers the FIFOs (the I²C controllers'). Continuous channels start over at their end.
use peripherals::dma::Dma;

pub const BASE: u64 = 0x6002_0000;
pub const SIZE: u64 = 0x2000;
const CHANNELS: usize = 32;

pub fn irq_of(channel: usize) -> usize {
    32 + if channel < 16 { 0x68 + channel } else { 0x80 + channel - 16 }
}

const CSR: usize = 0;
const STATUS: usize = 1;
const CSRE: usize = 3;
const AHBPTR: usize = 4;
const APBPTR: usize = 6;
const WCOUNT: usize = 8;

const ENB: u32 = 1 << 31;
const IE_EOC: u32 = 1 << 30;
const TO_PERIPHERAL: u32 = 1 << 28;
const ONCE: u32 = 1 << 27;
const EOC: u32 = 1 << 30;

#[derive(Clone, Copy, Default)]
struct Channel {
    regs: [u32; 16],
    /// words of the transfer that have moved
    done: u32,
}

pub struct ApbDma {
    general: [u32; 16],
    channels: [Channel; CHANNELS],
}

impl Default for ApbDma {
    fn default() -> Self {
        Self { general: [0; 16], channels: [Channel::default(); CHANNELS] }
    }
}

/// A peripheral's FIFO as the DMA sees it, by its APB address: whether a word can move now (to the
/// peripheral, or from it), and moving it.
pub trait Fifo {
    fn ready(&mut self, apb: u64, to_peripheral: bool) -> bool;
    fn take(&mut self, apb: u64) -> u32;
    fn put(&mut self, apb: u64, v: u32);
}

impl ApbDma {
    pub fn irq(&self, channel: usize) -> bool {
        let c = &self.channels[channel];
        c.regs[STATUS] & EOC != 0 && c.regs[CSR] & IE_EOC != 0
    }

    pub fn read(&self, offset: u64) -> u32 {
        let o = offset as usize;
        if o < 0x1000 {
            return self.general.get(o / 4).copied().unwrap_or(0);
        }
        let (k, r) = ((o - 0x1000) / 0x40, (o & 0x3f) / 4);
        let Some(c) = self.channels.get(k) else { return 0 };
        let left = (c.regs[WCOUNT] & 0xfffc).saturating_sub(4 * c.done);
        match r {
            STATUS => c.regs[STATUS] & EOC | if c.regs[CSR] & ENB != 0 { 1 << 31 } else { 0 } | left,
            9 => left, // WORD_TRANSFER
            _ => c.regs[r],
        }
    }

    pub fn write(&mut self, offset: u64, v: u32) {
        let o = offset as usize;
        if o < 0x1000 {
            if let Some(r) = self.general.get_mut(o / 4) {
                *r = v;
            }
            return;
        }
        let (k, r) = ((o - 0x1000) / 0x40, (o & 0x3f) / 4);
        let Some(c) = self.channels.get_mut(k) else { return };
        match r {
            STATUS => c.regs[STATUS] &= !(v & EOC),
            CSR => {
                if c.regs[CSR] & ENB == 0 && v & ENB != 0 {
                    c.done = 0;
                }
                c.regs[CSR] = v;
            }
            _ => c.regs[r] = v,
        }
    }

    /// Move what can move.
    pub fn run(&mut self, dma: &mut Dma, fifo: &mut dyn Fifo) {
        if self.general[0] & 1 << 31 == 0 {
            return;
        }
        for c in self.channels.iter_mut() {
            let csr = c.regs[CSR];
            if csr & ENB == 0 || c.regs[CSRE] & 1 << 31 != 0 {
                continue;
            }
            let (to, apb) = (csr & TO_PERIPHERAL != 0, c.regs[APBPTR] as u64);
            let words = (c.regs[WCOUNT] & 0xfffc) / 4 + 1;
            while c.done < words && fifo.ready(apb, to) {
                let at = c.regs[AHBPTR] as u64 + 4 * c.done as u64;
                if to {
                    let v = dma.get(at, 4).map_or(0, |b| u32::from_le_bytes(b.try_into().unwrap()));
                    fifo.put(apb, v);
                } else {
                    let v = fifo.take(apb);
                    if let Some(b) = dma.slice(at, 4) {
                        b.copy_from_slice(&v.to_le_bytes());
                    }
                }
                c.done += 1;
            }
            if c.done == words {
                c.regs[STATUS] |= EOC;
                if csr & ONCE != 0 {
                    c.regs[CSR] &= !ENB;
                } else {
                    c.done = 0;
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct F(std::collections::VecDeque<u32>, Vec<u32>);
    impl Fifo for F {
        fn ready(&mut self, _: u64, to: bool) -> bool { to || !self.0.is_empty() }
        fn take(&mut self, _: u64) -> u32 { self.0.pop_front().unwrap() }
        fn put(&mut self, _: u64, v: u32) { self.1.push(v) }
    }

    #[test]
    fn channels_wait_for_the_fifo() {
        let mut ram = vec![0u8; 64];
        let mut d = ApbDma::default();
        let mut f = F([1, 2].into(), vec![]);
        d.write(0, 1 << 31);
        for (r, v) in [(AHBPTR, 0x8000_0010), (APBPTR, 0x7000_c754), (WCOUNT, 8), (CSR, ENB | IE_EOC | ONCE)] {
            d.write(0x1000 + 0x40 * 17 + 4 * r as u64, v);
        }
        let mut m = Dma { ram: &mut ram, base: 0x8000_0000 };
        d.run(&mut m, &mut f);
        assert!(!d.irq(17));
        f.0.push_back(3);
        d.run(&mut m, &mut f);
        assert!(d.irq(17));
        assert_eq!(d.read(0x1000 + 0x40 * 17 + 4) & ENB, 0, "not busy");
        assert_eq!(ram[0x10..0x1c], [1, 0, 0, 0, 2, 0, 0, 0, 3, 0, 0, 0]);
        d.write(0x1000 + 0x40 * 17 + 4, EOC);
        assert!(!d.irq(17));
        // to the peripheral
        for (r, v) in [(AHBPTR, 0x8000_0014), (WCOUNT, 4), (CSR, ENB | ONCE | TO_PERIPHERAL)] {
            d.write(0x1000 + 4 * r as u64, v);
        }
        d.run(&mut Dma { ram: &mut ram, base: 0x8000_0000 }, &mut f);
        assert_eq!(f.1, [2, 3]);
    }
}
