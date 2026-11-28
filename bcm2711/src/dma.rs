//! BCM2835 DMA controller (the legacy channels 0..14, `brcm,bcm2835-dma`: at 0xfe007000 on the
//! BCM2711, 0x3f007000 on the BCM2837). A channel set ACTIVE walks its chain of 32-byte control
//! blocks (TI, SOURCE_AD, DEST_AD, TXFR_LEN, STRIDE, NEXTCONBK). A block that waits for a
//! peripheral's DREQ (TI.DEST_DREQ or SRC_DREQ with a PERMAP) moves data only as fast as that
//! peripheral asks for it, with the channel registers showing its progress (`Dma::pace`); other
//! blocks complete at once (`Dma::run`). The SoC's bus (`DmaBus`) performs the accesses and
//! translates the VideoCore bus addresses the control blocks hold.

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

/// The SoC as a DMA channel sees it: memory and peripherals at physical addresses, and how the
/// VideoCore bus addresses in control blocks map to them.
pub trait DmaBus: emu_core::Bus {
    fn phys(&self, bus: u32) -> u64;
    /// Peripheral `dreq` asks for a transfer now (a FIFO with data or room: `Dma::serve`).
    fn dreq(&self, _dreq: u32) -> bool { false }
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

    /// Some channel is active on a block that waits for a DREQ.
    pub fn paced_active(&self) -> bool { self.ch.iter().any(|c| c.cs & ACTIVE != 0 && paced(c.ti).is_some()) }

    /// Load channel `n`'s next control block into its registers; false at the end of the chain.
    fn load_cb(&mut self, n: usize, bus: &mut impl DmaBus) -> bool {
        let cb = self.ch[n].conblk & !31;
        if cb == 0 { return false; }
        let base = bus.phys(cb);
        let mut w = [0u32; 8];
        for (k, x) in w.iter_mut().enumerate() { *x = bus.read32(base + 4 * k as u64).unwrap_or(0); }
        let c = &mut self.ch[n];
        [c.ti, c.src, c.dst, c.len, c.stride, c.next] = [w[0], w[1], w[2], w[3], w[4], w[5]];
        true
    }

    /// The current block of channel `n` is done: interrupt, then on to the next one.
    fn block_done(&mut self, n: usize, bus: &mut impl DmaBus) -> bool {
        let c = &mut self.ch[n];
        if c.ti & TI_INTEN != 0 { c.cs |= INT; }
        c.conblk = c.next;
        if c.conblk == 0 || !self.load_cb(n, bus) {
            let c = &mut self.ch[n];
            c.cs = (c.cs & !ACTIVE) | END;
            return false;
        }
        true
    }

    /// Channel `n` was made active: walk its control blocks now, up to one that waits for a
    /// DREQ. Returns true if it stopped at such a block (`pace` moves it on).
    pub fn run(&mut self, n: usize, bus: &mut impl DmaBus) -> bool {
        for _ in 0..4096 {
            let cb = self.ch[n].conblk & !31;
            if cb == 0 { break; }
            if !self.load_cb(n, bus) { break; }
            let c = self.ch[n];
            if paced(c.ti).is_some() { return true; } // runs as its peripheral asks
            let [ti, src, dst, len, stride, next] = [c.ti, c.src, c.dst, c.len, c.stride, c.next];
            let (xlen, rows) = if ti & TI_TDMODE != 0 { (len & 0xffff, (len >> 16) & 0x3fff) } else { (len & 0x3fff_ffff, 1) };
            let (sstride, dstride) = ((stride & 0xffff) as i16 as i64, (stride >> 16) as i16 as i64);
            let (mut s, mut d) = (bus.phys(src), bus.phys(dst));
            for row in 0..rows {
                let mut off = 0;
                while off < xlen {
                    let n4 = (xlen - off).min(4);
                    if n4 == 4 {
                        let v = if ti & TI_SRC_IGNORE != 0 { 0 } else { bus.read32(s).unwrap_or(0) };
                        if ti & TI_DEST_IGNORE == 0 { let _ = bus.write32(d, v); }
                    } else {
                        for b in 0..n4 as u64 {
                            let v = if ti & TI_SRC_IGNORE != 0 { 0 } else { bus.read8(s + b).unwrap_or(0) };
                            if ti & TI_DEST_IGNORE == 0 { let _ = bus.write8(d + b, v); }
                        }
                    }
                    if ti & TI_SRC_INC != 0 { s += n4 as u64; }
                    if ti & TI_DEST_INC != 0 { d += n4 as u64; }
                    off += n4;
                }
                if ti & TI_TDMODE != 0 && row + 1 < rows { s = s.wrapping_add(sstride as u64); d = d.wrapping_add(dstride as u64); }
            }
            let c = &mut self.ch[n];
            c.ti = ti; c.src = src; c.dst = dst; c.len = 0; c.stride = stride; c.next = next;
            if ti & TI_INTEN != 0 { c.cs |= INT; }
            c.conblk = next;
        }
        let c = &mut self.ch[n];
        c.cs = (c.cs & !ACTIVE) | END;
        false
    }

    /// Feed DREQ-paced channels what their peripherals asked for: `demands` holds (DREQ, words
    /// wanted) and is decremented by what was moved; the words read, with their DREQ, are
    /// appended to `out` in order for the caller to hand to the peripherals. A chain that
    /// reaches a block without a DREQ runs it at once.
    pub fn pace(&mut self, demands: &mut [(u32, u64)], bus: &mut impl DmaBus, out: &mut Vec<(u32, u32)>) {
        for n in 0..CHANNELS {
            let Some(dreq) = paced(self.ch[n].ti) else { continue };
            if self.ch[n].cs & ACTIVE == 0 { continue; }
            let Some(slot) = demands.iter().position(|d| d.0 == dreq) else { continue };
            let mut demand = demands[slot].1;
            let mut guard = 0;
            while demand > 0 && guard < 1 << 20 {
                guard += 1;
                let c = self.ch[n];
                if c.len < 4 {
                    if !self.block_done(n, bus) { break; }
                    if paced(self.ch[n].ti).is_none() { self.run(n, bus); break; }
                    continue;
                }
                let v = if c.ti & TI_SRC_IGNORE != 0 { 0 } else { bus.read32(bus.phys(c.src)).unwrap_or(0) };
                out.push((dreq, v));
                let c = &mut self.ch[n];
                if c.ti & TI_SRC_INC != 0 { c.src = c.src.wrapping_add(4); }
                c.len -= 4;
                demand -= 1;
            }
            // the block that just ran dry completes now, not at the next demand
            if self.ch[n].cs & ACTIVE != 0 && self.ch[n].len < 4 { self.block_done(n, bus); }
            demands[slot].1 = demand;
        }
    }

    /// Move data for the channels waiting on `dreq` while the peripheral asserts it
    /// (`DmaBus::dreq`): a FIFO in either direction (TI.SRC_DREQ reads it, TI.DEST_DREQ fills
    /// it), word by word through the bus. A block that runs dry completes (its interrupt, then
    /// the next block); the chain goes on while the peripheral keeps asking. Returns whether
    /// anything moved.
    pub fn serve(&mut self, dreq: u32, bus: &mut impl DmaBus) -> bool {
        let mut moved = false;
        for n in 0..CHANNELS {
            let mut guard = 0u32;
            while self.ch[n].cs & ACTIVE != 0 && paced(self.ch[n].ti) == Some(dreq) && guard < 1 << 24 {
                guard += 1;
                let c = self.ch[n];
                if c.len < 4 {
                    moved = true;
                    if !self.block_done(n, bus) { break; }
                    if paced(self.ch[n].ti).is_none() { self.run(n, bus); }
                    continue;
                }
                if !bus.dreq(dreq) { break; }
                let v = if c.ti & TI_SRC_IGNORE != 0 { 0 } else { bus.read32(bus.phys(c.src)).unwrap_or(0) };
                if c.ti & TI_DEST_IGNORE == 0 { let _ = bus.write32(bus.phys(c.dst), v); }
                let c = &mut self.ch[n];
                if c.ti & TI_SRC_INC != 0 { c.src = c.src.wrapping_add(4); }
                if c.ti & TI_DEST_INC != 0 { c.dst = c.dst.wrapping_add(4); }
                c.len -= 4;
                moved = true;
            }
        }
        moved
    }

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

    /// RAM at 0..0x1000, a peripheral FIFO register at 0x2000 that hands out `fifo` (DREQ 13).
    struct FifoBus { ram: Vec<u8>, fifo: std::collections::VecDeque<u32> }
    impl emu_core::Bus for FifoBus {
        fn read8(&mut self, a: u64) -> Result<u8, emu_core::Fault> { Ok(self.ram[a as usize]) }
        fn read16(&mut self, _: u64) -> Result<u16, emu_core::Fault> { Ok(0) }
        fn read32(&mut self, a: u64) -> Result<u32, emu_core::Fault> {
            if a == 0x2000 { return Ok(self.fifo.pop_front().unwrap_or(0)); }
            Ok(u32::from_le_bytes(self.ram[a as usize..a as usize + 4].try_into().unwrap()))
        }
        fn read64(&mut self, _: u64) -> Result<u64, emu_core::Fault> { Ok(0) }
        fn write8(&mut self, a: u64, v: u8) -> Result<(), emu_core::Fault> { self.ram[a as usize] = v; Ok(()) }
        fn write16(&mut self, _: u64, _: u16) -> Result<(), emu_core::Fault> { Ok(()) }
        fn write32(&mut self, a: u64, v: u32) -> Result<(), emu_core::Fault> { self.ram[a as usize..a as usize + 4].copy_from_slice(&v.to_le_bytes()); Ok(()) }
        fn write64(&mut self, _: u64, _: u64) -> Result<(), emu_core::Fault> { Ok(()) }
    }
    impl DmaBus for FifoBus {
        fn phys(&self, bus: u32) -> u64 { bus as u64 }
        fn dreq(&self, dreq: u32) -> bool { dreq == 13 && !self.fifo.is_empty() }
    }

    #[test]
    fn serve_reads_a_fifo_while_its_dreq_is_up() {
        use emu_core::Bus;
        let mut bus = FifoBus { ram: vec![0; 0x1000], fifo: (1..=6).collect() };
        // one block: 8 words from the FIFO (SRC_DREQ, PERMAP 13) to RAM at 0x100, interrupt at the end
        let cb = [TI_INTEN | TI_SRC_DREQ | TI_DEST_INC | (13 << 16), 0x2000, 0x100, 32, 0, 0];
        for (k, w) in cb.iter().enumerate() { bus.write32(0x40 + 4 * k as u64, *w).unwrap(); }
        let mut d = Dma::new();
        d.write(0x404, 0x40);
        let n = d.write(0x400, ACTIVE).unwrap();
        assert!(d.run(n, &mut bus), "the block waits for its DREQ");
        assert!(d.serve(13, &mut bus));
        assert_eq!(d.ch[4].len, 8, "six words moved, the FIFO ran dry");
        assert_eq!(bus.read32(0x114).unwrap(), 6);
        bus.fifo.extend([7, 8]);
        d.serve(13, &mut bus);
        assert_eq!(bus.read32(0x11c).unwrap(), 8);
        assert_eq!(d.ch[4].cs & (ACTIVE | END | INT), END | INT, "done, with its interrupt");
    }
}
