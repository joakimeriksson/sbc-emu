//! BCM2835 DMA controller (the legacy channels 0..14 at 0xfe007000, `brcm,bcm2835-dma`).
//! A channel set ACTIVE walks its chain of 32-byte control blocks (TI, SOURCE_AD, DEST_AD,
//! TXFR_LEN, STRIDE, NEXTCONBK). A block that waits for a peripheral's DREQ (TI.DEST_DREQ or
//! SRC_DREQ with a PERMAP) moves data only as fast as that peripheral asks for it, with the
//! channel registers showing its progress (`Soc::dma_pace`); other blocks complete at once.
//! The SoC performs the memory accesses (`Soc::dma_run`); this holds the register state.

pub const CHANNELS: usize = 15;

// CS bits
pub const ACTIVE: u32 = 1 << 0;
pub const END: u32 = 1 << 1;
pub const INT: u32 = 1 << 2;
pub const RESET: u32 = 1 << 31;
pub const ABORT: u32 = 1 << 30;

// TI bits
pub const TI_INTEN: u32 = 1 << 0;
pub const TI_TDMODE: u32 = 1 << 1;
pub const TI_DEST_INC: u32 = 1 << 4;
pub const TI_DEST_WIDTH: u32 = 1 << 5;
pub const TI_DEST_IGNORE: u32 = 1 << 7;
pub const TI_SRC_INC: u32 = 1 << 8;
pub const TI_SRC_WIDTH: u32 = 1 << 9;
pub const TI_SRC_IGNORE: u32 = 1 << 11;
pub const TI_DEST_DREQ: u32 = 1 << 6;
pub const TI_SRC_DREQ: u32 = 1 << 10;

/// The peripheral a block waits for (TI.PERMAP), if it waits for one.
pub fn paced(ti: u32) -> Option<u32> {
    let permap = (ti >> 16) & 0x1f;
    (ti & (TI_DEST_DREQ | TI_SRC_DREQ) != 0 && permap != 0).then_some(permap)
}

#[derive(Clone, Copy, Default)]
pub struct Chan {
    pub cs: u32,
    pub conblk: u32,
    pub ti: u32,
    pub src: u32,
    pub dst: u32,
    pub len: u32,
    pub stride: u32,
    pub next: u32,
    pub debug: u32,
}

pub struct Dma {
    pub ch: [Chan; CHANNELS],
    pub enable: u32,
}

impl Default for Dma { fn default() -> Self { Self::new() } }

impl Dma {
    pub fn new() -> Self { Dma { ch: [Chan::default(); CHANNELS], enable: 0x7fff } }

    pub fn read(&self, off: u64) -> u32 {
        match off {
            0xfe0 => self.ch.iter().enumerate().fold(0, |a, (n, c)| a | (((c.cs & INT != 0) as u32) << n)), // INT_STATUS
            0xff0 => self.enable,
            o if (o as usize) < CHANNELS * 0x100 => {
                let c = &self.ch[o as usize / 0x100];
                match o & 0xff {
                    0x00 => c.cs,
                    0x04 => c.conblk,
                    0x08 => c.ti,
                    0x0c => c.src,
                    0x10 => c.dst,
                    0x14 => c.len,
                    0x18 => c.stride,
                    0x1c => c.next,
                    0x20 => c.debug,
                    _ => 0,
                }
            }
            _ => 0,
        }
    }

    /// Register write; returns the channel to run if it was made active.
    pub fn write(&mut self, off: u64, v: u32) -> Option<usize> {
        match off {
            0xff0 => { self.enable = v; None }
            o if (o as usize) < CHANNELS * 0x100 => {
                let n = o as usize / 0x100;
                let c = &mut self.ch[n];
                match o & 0xff {
                    0x00 => {
                        if v & RESET != 0 { *c = Chan::default(); return None; }
                        if v & ABORT != 0 { c.cs &= !ACTIVE; c.conblk = c.next; }
                        // END and INT are write-one-to-clear; priorities and the debug bits are kept
                        const KEEP: u32 = 0x30ff_0000;
                        c.cs &= !(v & (END | INT));
                        c.cs = (c.cs & !KEEP) | (v & KEEP);
                        // ACTIVE written to a channel that is already running (as Linux does when it clears
                        // INT) leaves it running: the block is not loaded again
                        let was_active = c.cs & ACTIVE != 0;
                        if v & ACTIVE != 0 && c.conblk != 0 {
                            c.cs |= ACTIVE;
                            return (!was_active).then_some(n);
                        }
                        if v & ACTIVE == 0 { c.cs &= !ACTIVE; }
                        None
                    }
                    0x04 => { c.conblk = v; None }
                    0x20 => { c.debug &= !(v & 7); None }
                    _ => None,
                }
            }
            _ => None,
        }
    }

    pub fn irq(&self, n: usize) -> bool { self.ch[n].cs & INT != 0 }

    pub fn save_state(&self, w: &mut emu_core::snap::W) {
        w.tag("dma-v1");
        w.u32(self.enable);
        for c in &self.ch { for v in [c.cs, c.conblk, c.ti, c.src, c.dst, c.len, c.stride, c.next, c.debug] { w.u32(v); } }
    }
    pub fn load_state(&mut self, r: &mut emu_core::snap::R) -> Result<(), emu_core::snap::SnapError> {
        r.tag("dma-v1")?;
        self.enable = r.u32()?;
        for c in self.ch.iter_mut() {
            for v in [&mut c.cs, &mut c.conblk, &mut c.ti, &mut c.src, &mut c.dst, &mut c.len, &mut c.stride, &mut c.next, &mut c.debug] { *v = r.u32()?; }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn active_written_to_a_running_channel_does_not_restart_it() {
        let mut d = Dma::new();
        d.write(0x204, 0x1000);
        assert_eq!(d.write(0x200, ACTIVE), Some(2), "a stopped channel starts");
        d.ch[2].cs |= INT;
        // Linux's interrupt handler: INT to clear it, with ACTIVE kept set
        assert_eq!(d.write(0x200, INT | ACTIVE), None, "a running one goes on where it is");
        assert_eq!(d.ch[2].cs & (INT | ACTIVE), ACTIVE);
        d.write(0x200, 0);
        assert_eq!(d.write(0x200, ACTIVE), Some(2), "paused, it starts again");
    }
}
