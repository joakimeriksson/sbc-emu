//! Tegra210 SDMMC controller (`nvidia,tegra210-sdhci`): the SDHCI 3.0 register set with
//! NVIDIA's pad calibration and tuning registers from 0x100. The card protocol is
//! `emu_core::sdcard::SdCard`.
//!
//! Data moves by ADMA2 with 32- or 64-bit descriptors (the Nano's RAM goes past 4 GiB), all of
//! it when the command is issued, or by PIO through the buffer port. No UHS modes: the card
//! does not offer 1.8 V.
use emu_core::sdcard::{Data, Response, SdCard, OUT_OF_RANGE};

pub const SIZE: u64 = 0x200;

const BLKSIZE: usize = 0x04 / 4;
const ARG: usize = 0x08 / 4;
const CMD: usize = 0x0c / 4;
const RESP0: usize = 0x10 / 4;
const DATA: usize = 0x20 / 4;
const PRESENT: usize = 0x24 / 4;
const CTRL: usize = 0x28 / 4;
const CLOCK: usize = 0x2c / 4;
const INTSTAT: usize = 0x30 / 4;
const INTEN: usize = 0x34 / 4;
const SIGEN: usize = 0x38 / 4;
const CAPS: usize = 0x40 / 4;
const CAPS1: usize = 0x44 / 4;
const ADMA_LO: usize = 0x58 / 4;
const ADMA_HI: usize = 0x5c / 4;
const VERSION: usize = 0xfc / 4;
const DLLCAL_STATUS: usize = 0x1bc / 4;
const AUTO_CAL_STATUS: usize = 0x1ec / 4;

const INT_CMD_DONE: u32 = 1 << 0;
const INT_XFER_DONE: u32 = 1 << 1;
const INT_DMA: u32 = 1 << 3;
const INT_BUF_WR: u32 = 1 << 4;
const INT_BUF_RD: u32 = 1 << 5;
const INT_ERROR: u32 = 1 << 15;
const ERR_CMD_TIMEOUT: u32 = 1 << 16;
const ERR_DATA_TIMEOUT: u32 = 1 << 20;
const ERR_ADMA: u32 = 1 << 25;

/// The machine's memory as the controller reaches it.
pub struct Dma<'a> {
    pub ram: &'a mut [u8],
    pub base: u64,
}

impl Dma<'_> {
    fn slice(&mut self, pa: u64, len: usize) -> Option<&mut [u8]> {
        let o = usize::try_from(pa.checked_sub(self.base)?).ok()?;
        self.ram.get_mut(o..o.checked_add(len)?)
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Xfer {
    None,
    Read,
    Write,
}

pub struct Sdhci {
    pub card: SdCard,
    regs: [u32; SIZE as usize / 4],
    xfer: Xfer,
    /// PIO: the block in the buffer and how far the host is in it
    buf: Vec<u8>,
    pos: usize,
    lba: u64,
    /// blocks left after the one in the buffer
    left: u32,
    pub blocks_read: u64,
    pub blocks_written: u64,
}

impl Default for Sdhci {
    fn default() -> Self {
        let mut s = Self { card: SdCard::new(None), regs: [0; SIZE as usize / 4], xfer: Xfer::None, buf: Vec::new(), pos: 0, lba: 0, left: 0, blocks_read: 0, blocks_written: 0 };
        s.card.trace = std::env::var_os("SBC_MMC_TRACE").is_some();
        s.reset();
        s
    }
}

impl Sdhci {
    fn reset(&mut self) {
        self.regs[..0x100 / 4].fill(0);
        // 3.3 V, high speed, ADMA2, 64-bit, 48 MHz timeout clock base; the base clock comes from
        // the clock controller (the driver does not trust the field on Tegra)
        self.regs[CAPS] = 1 << 28 | 1 << 24 | 1 << 21 | 1 << 19 | 208 << 8 | 1 << 7 | 12;
        self.regs[CAPS1] = 0;
        self.regs[VERSION] = 2 << 16; // SDHCI 3.00
        self.xfer = Xfer::None;
    }

    fn status(&self) -> u32 {
        let s = self.regs[INTSTAT];
        if s & 0xffff_0000 != 0 { s | INT_ERROR } else { s & !INT_ERROR }
    }

    pub fn irq(&self) -> bool {
        self.status() & self.regs[SIGEN] != 0
    }

    fn raise(&mut self, bits: u32) {
        self.regs[INTSTAT] |= bits & self.regs[INTEN];
    }

    fn present(&self) -> u32 {
        let lines = 0xf << 20 | 1 << 24 | 1 << 19 | if self.card.present() { 1 << 16 | 1 << 17 | 1 << 18 } else { 1 << 17 };
        lines | match self.xfer {
            Xfer::None => 0,
            Xfer::Read => 1 << 1 | 1 << 2 | 1 << 9 | if self.pos < self.buf.len() { 1 << 11 } else { 0 },
            Xfer::Write => 1 << 1 | 1 << 2 | 1 << 8 | 1 << 10,
        }
    }

    /// `size` bytes at `offset` (1, 2 or 4, within one register).
    pub fn read_sized(&mut self, offset: u64, size: usize) -> u32 {
        let word = match (offset as usize & (SIZE as usize - 1)) / 4 {
            DATA => return self.pio_read(size),
            PRESENT => self.present(),
            INTSTAT => self.status(),
            // calibrations finish at once
            AUTO_CAL_STATUS | DLLCAL_STATUS => 0,
            r => self.regs[r],
        };
        word >> ((offset & 3) * 8) & if size >= 4 { u32::MAX } else { (1 << (8 * size)) - 1 }
    }

    /// The 32-bit register at `offset`.
    pub fn read(&mut self, offset: u64) -> u32 {
        self.read_sized(offset & !3, 4)
    }

    /// `size` bytes at `offset` (1, 2 or 4, within one register).
    pub fn write(&mut self, offset: u64, value: u32, size: usize, dma: &mut Dma) {
        let r = (offset as usize & (SIZE as usize - 1)) / 4;
        let shift = (offset & 3) * 8;
        let mask = if size >= 4 { u32::MAX } else { ((1u32 << (8 * size)) - 1) << shift };
        let v = value << shift & mask;
        match r {
            DATA => return self.pio_write(value, size),
            INTSTAT => {
                self.regs[INTSTAT] &= !v;
                return;
            }
            PRESENT | CAPS | CAPS1 | VERSION => return,
            _ => {}
        }
        self.regs[r] = self.regs[r] & !mask | v;
        match r {
            CMD if mask & 0xffff_0000 != 0 => self.command(dma),
            CLOCK => {
                let c = self.regs[CLOCK];
                let reset = c >> 24 & 7;
                self.regs[CLOCK] = c & 0x00ff_fffd | (c & 1) << 1; // internal clock: stable at once
                if reset & 1 != 0 {
                    self.reset();
                } else if reset != 0 {
                    if reset & 2 != 0 {
                        self.regs[INTSTAT] &= !INT_CMD_DONE;
                    }
                    if reset & 4 != 0 {
                        self.xfer = Xfer::None;
                        self.regs[INTSTAT] &= !(INT_XFER_DONE | INT_DMA | INT_BUF_RD | INT_BUF_WR);
                    }
                }
            }
            _ => {}
        }
    }

    fn command(&mut self, dma: &mut Dma) {
        let word = self.regs[CMD];
        let (mode, index, busy, with_data) = (word & 0xffff, word >> 24 & 0x3f, word >> 16 & 3 == 3, word & 1 << 21 != 0);
        if index == 12 {
            self.xfer = Xfer::None;
        }
        let reply = self.card.command(index, self.regs[ARG]);
        match reply.response {
            Some(Response::Short(w)) => self.regs[RESP0] = w,
            Some(Response::Long(l)) => {
                // bits 127:8 (no CRC), least significant first
                for k in 0..4 {
                    self.regs[RESP0 + k] = (l >> 8 >> (32 * k)) as u32;
                }
            }
            None if index == 0 => {}
            None => {
                // Complete with the error, as the SDHCI specification allows (its table of the two
                // bits): Linux masks the timeout's signal on Tegra, so nothing else would tell
                // it that a command went unanswered.
                return self.raise(INT_CMD_DONE | ERR_CMD_TIMEOUT);
            }
        }
        let size = ((self.regs[BLKSIZE] & 0xfff) as usize).max(1);
        let blocks = if mode & 1 << 5 == 0 { 1 } else { (self.regs[BLKSIZE] >> 16).max(1) };
        // card blocks are 512 bytes, and the transfer is on the card or not at all
        let refused = match reply.data {
            Data::Read { lba } | Data::Write { lba } if with_data => {
                let outside = !self.card.in_range(lba, blocks as u64);
                if outside {
                    self.regs[RESP0] |= OUT_OF_RANGE;
                }
                outside || size != 512
            }
            _ => false,
        };
        self.raise(INT_CMD_DONE);
        if !with_data {
            if busy {
                self.raise(INT_XFER_DONE); // the card is never busy
            }
            return;
        }
        if refused {
            return self.raise(ERR_DATA_TIMEOUT); // the data phase never comes
        }
        self.pos = 0;
        self.left = blocks - 1;
        match reply.data {
            Data::Bytes(mut b) => {
                b.resize(size, 0);
                self.buf = b;
                self.left = 0;
                self.xfer = Xfer::Read;
            }
            Data::Read { lba } => {
                self.lba = lba;
                self.xfer = Xfer::Read;
                self.load(size);
            }
            Data::Write { lba } => {
                self.lba = lba;
                self.xfer = Xfer::Write;
                self.buf = vec![0; size];
            }
            // refused (out of range): the data phase never comes
            Data::None => return self.raise(ERR_DATA_TIMEOUT),
        }
        let select = self.regs[CTRL] >> 3 & 3;
        if mode & 1 != 0 && select >= 2 {
            self.adma(select == 3, dma);
        } else {
            self.raise(if self.xfer == Xfer::Read { INT_BUF_RD } else { INT_BUF_WR });
        }
    }

    fn load(&mut self, size: usize) {
        self.buf = vec![0; size];
        self.card.read_blocks(self.lba, &mut self.buf);
        self.blocks_read += 1;
        self.lba += 1;
        self.pos = 0;
    }

    fn store(&mut self) {
        self.card.write_blocks(self.lba, &self.buf);
        self.blocks_written += 1;
        self.lba += 1;
        self.pos = 0;
    }

    /// Move the whole transfer through the ADMA2 descriptor table.
    fn adma(&mut self, wide: bool, dma: &mut Dma) {
        let mut d = self.regs[ADMA_LO] as u64 | if wide { (self.regs[ADMA_HI] as u64) << 32 } else { 0 };
        let step = if wide { 12 } else { 8 };
        let mut finished = false;
        // a table is at most as long as RAM; a loop of links ends here
        'table: for _ in 0..1 << 20 {
            let Some(raw) = dma.slice(d, step).map(|s| s.to_vec()) else { break };
            let attr = raw[0];
            let len = match u16::from_le_bytes([raw[2], raw[3]]) { 0 => 0x1_0000, n => n as usize };
            let lo = u32::from_le_bytes(raw[4..8].try_into().unwrap()) as u64;
            let addr = lo | if wide { (u32::from_le_bytes(raw[8..12].try_into().unwrap()) as u64) << 32 } else { 0 };
            if attr & 1 == 0 {
                break; // not valid
            }
            match attr >> 4 & 3 {
                2 => {
                    let Some(mem) = dma.slice(addr, len) else { break };
                    let mut o = 0;
                    while o < len && !finished {
                        let n = (self.buf.len() - self.pos).min(len - o);
                        if self.xfer == Xfer::Read {
                            mem[o..o + n].copy_from_slice(&self.buf[self.pos..self.pos + n]);
                        } else {
                            self.buf[self.pos..self.pos + n].copy_from_slice(&mem[o..o + n]);
                        }
                        self.pos += n;
                        o += n;
                        if self.pos == self.buf.len() {
                            finished = self.block_done();
                        }
                    }
                }
                3 => {
                    d = addr;
                    continue 'table;
                }
                _ => {}
            }
            d += step as u64;
            if attr & 2 != 0 || finished {
                break;
            }
        }
        self.xfer = Xfer::None;
        self.raise(if finished { INT_XFER_DONE } else { ERR_ADMA });
    }

    /// The buffer's block is done with; true: it was the last.
    fn block_done(&mut self) -> bool {
        if self.xfer == Xfer::Write {
            self.store();
        }
        if self.left == 0 {
            return true;
        }
        self.left -= 1;
        if self.xfer == Xfer::Read {
            self.load(self.buf.len());
        }
        false
    }

    fn pio_step(&mut self) {
        if self.pos < self.buf.len() {
            return;
        }
        if self.block_done() {
            self.xfer = Xfer::None;
            self.raise(INT_XFER_DONE);
        } else {
            self.raise(if self.xfer == Xfer::Read { INT_BUF_RD } else { INT_BUF_WR });
        }
    }

    fn pio_read(&mut self, size: usize) -> u32 {
        if self.xfer != Xfer::Read {
            return 0;
        }
        let mut w = [0; 4];
        for b in w[..size.min(4)].iter_mut() {
            *b = self.buf.get(self.pos).copied().unwrap_or(0);
            self.pos += 1;
        }
        self.pio_step();
        u32::from_le_bytes(w)
    }

    fn pio_write(&mut self, value: u32, size: usize) {
        if self.xfer != Xfer::Write {
            return;
        }
        for b in &value.to_le_bytes()[..size.min(4)] {
            if let Some(d) = self.buf.get_mut(self.pos) {
                *d = *b;
            }
            self.pos += 1;
        }
        self.pio_step();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use emu_core::disk::{Disk, MemDisk};

    const BASE: u64 = 0x8000_0000;

    fn ready() -> (Sdhci, Vec<u8>) {
        let mut disk = MemDisk::new(4096);
        for lba in 0..64u64 {
            disk.write(lba, &[lba as u8; 512]);
        }
        let mut s = Sdhci::default();
        s.card = SdCard::new(Some(Box::new(disk)));
        let mut ram = vec![0u8; 0x10_0000];
        let mut d = Dma { ram: &mut ram, base: BASE };
        s.write(0x34, u32::MAX, 4, &mut d);
        s.write(0x38, u32::MAX, 4, &mut d);
        for (i, a, r) in [(0, 0, 0), (8, 0x1aa, 2), (55, 0, 2), (41, 0x40ff_8000, 2), (2, 0, 1), (3, 0, 2), (7, 0x5bc0 << 16, 3)] {
            s.write(0x08, a, 4, &mut d);
            s.write(0x0e, i << 8 | r, 2, &mut d);
        }
        assert!(s.irq());
        s.write(0x30, u32::MAX, 4, &mut d);
        (s, ram)
    }

    fn start(s: &mut Sdhci, d: &mut Dma, index: u32, lba: u32, blocks: u32, mode: u32) {
        s.write(0x04, blocks << 16 | 512, 4, d);
        s.write(0x08, lba, 4, d);
        s.write(0x0c, index << 24 | 0x3a << 16 | mode, 4, d);
    }

    #[test]
    fn adma_reads_and_writes_with_wide_and_narrow_descriptors() {
        for wide in [false, true] {
            let (mut s, mut ram) = ready();
            ram[0x9000..0x9000 + 1024].fill(0xee);
            // two pieces that split a block, then the table's end
            let step = if wide { 12 } else { 8 };
            for (k, (addr, len, attr)) in [(BASE + 0x4000, 700u16, 0x21u16), (BASE + 0x8000, 836, 0x23)].into_iter().enumerate() {
                let o = 0x100 + k * step;
                ram[o..o + 2].copy_from_slice(&attr.to_le_bytes());
                ram[o + 2..o + 4].copy_from_slice(&len.to_le_bytes());
                ram[o + 4..o + step].copy_from_slice(&addr.to_le_bytes()[..step - 4]);
            }
            let mut d = Dma { ram: &mut ram, base: BASE };
            s.write(0x28, if wide { 0x18 } else { 0x10 }, 1, &mut d);
            s.write(0x58, (BASE + 0x100) as u32, 4, &mut d);
            start(&mut s, &mut d, 18, 5, 3, 1 | 1 << 1 | 1 << 4 | 1 << 5);
            assert_eq!(s.read(0x30) & 0xffff_0003, 3, "wide {wide}");
            assert_eq!(d.ram[0x4000..0x4000 + 512], [5; 512]);
            assert_eq!(d.ram[0x4000 + 512..0x4000 + 700], [6; 188]);
            assert_eq!(d.ram[0x8000..0x8000 + 324], [6; 324]);
            assert_eq!(d.ram[0x8000 + 324..0x8000 + 836], [7; 512]);
            // write it back elsewhere
            s.write(0x30, u32::MAX, 4, &mut d);
            s.write(0x58, (BASE + 0x100) as u32, 4, &mut d);
            start(&mut s, &mut d, 25, 100, 3, 1 | 1 << 1 | 1 << 5);
            assert_eq!(s.read(0x30) & 0xffff_0003, 3);
            let mut out = [0; 1536];
            s.card.read_blocks(100, &mut out);
            assert_eq!((out[0], out[512], out[1024], out[1535]), (5, 6, 7, 7));
        }
    }

    #[test]
    fn a_descriptor_outside_ram_is_an_error() {
        let (mut s, mut ram) = ready();
        ram[0x100..0x108].copy_from_slice(&[0x23, 0, 0, 2, 0, 0, 0, 0x40]); // 0x4000_0000
        let mut d = Dma { ram: &mut ram, base: BASE };
        s.write(0x28, 0x10, 1, &mut d);
        s.write(0x58, (BASE + 0x100) as u32, 4, &mut d);
        start(&mut s, &mut d, 17, 0, 1, 1 | 1 << 4);
        assert_ne!(s.read(0x30) & ERR_ADMA, 0);
    }

    #[test]
    fn pio_and_an_absent_card() {
        let (mut s, mut ram) = ready();
        let mut d = Dma { ram: &mut ram, base: BASE };
        start(&mut s, &mut d, 17, 9, 1, 1 << 4);
        assert_ne!(s.read(0x24) & 1 << 11, 0);
        for _ in 0..128 {
            assert_eq!(s.read(0x20), 0x0909_0909);
        }
        assert_ne!(s.read(0x30) & INT_XFER_DONE, 0);
        start(&mut s, &mut d, 24, 200, 1, 0);
        for _ in 0..128 {
            s.write(0x20, 0x1234_5678, 4, &mut d);
        }
        let mut out = [0; 512];
        s.card.read_blocks(200, &mut out);
        assert_eq!(&out[508..], &0x1234_5678u32.to_le_bytes());
        let mut none = Sdhci::default();
        none.write(0x34, u32::MAX, 4, &mut d);
        none.write(0x0e, 8 << 8 | 2, 2, &mut d);
        none.write(0x38, INT_CMD_DONE, 4, &mut d); // as the driver: not the timeout
        assert_ne!(none.read(0x30) & ERR_CMD_TIMEOUT, 0);
        assert!(none.irq());
        assert_eq!(none.read(0x24) & 1 << 16, 0);
    }

    #[test]
    fn a_transfer_past_the_end_fails_whole() {
        let (mut s, mut ram) = ready();
        let mut d = Dma { ram: &mut ram, base: BASE };
        let last = s.card.sectors() as u32 - 1;
        for (index, mode) in [(25, 1 << 5 | 1 << 1), (18, 1 << 5 | 1 << 1 | 1 << 4)] {
            start(&mut s, &mut d, index, last, 8, mode);
            assert_ne!(s.read(0x10) & 1 << 31, 0, "CMD{index}: OUT_OF_RANGE");
            assert_eq!(s.read(0x30) & (INT_XFER_DONE | INT_BUF_RD | INT_BUF_WR), 0, "CMD{index}: no data phase");
            assert_ne!(s.read(0x30) & 0xffff_0000, 0, "CMD{index}: a data error");
            s.write(0x2c, 6 << 24, 4, &mut d);
            s.write(0x30, u32::MAX, 4, &mut d);
        }
    }

    #[test]
    fn card_blocks_are_512_bytes() {
        let (mut s, mut ram) = ready();
        let mut d = Dma { ram: &mut ram, base: BASE };
        for size in [64, 1024] {
            s.write(0x04, 1 << 16 | size, 4, &mut d);
            s.write(0x08, 7, 4, &mut d);
            s.write(0x0c, 24 << 24 | 0x3a << 16, 4, &mut d);
            for _ in 0..256 {
                s.write(0x20, 0, 4, &mut d);
            }
            assert_eq!(s.read(0x30) & INT_XFER_DONE, 0);
            s.write(0x2c, 6 << 24, 4, &mut d);
            s.write(0x30, u32::MAX, 4, &mut d);
        }
        let mut out = [0; 1024];
        s.card.read_blocks(7, &mut out);
        assert_eq!((out[0], out[511], out[512]), (7, 7, 8), "nothing was written");
    }

    #[test]
    fn a_command_reset_leaves_the_data_transfer() {
        let (mut s, mut ram) = ready();
        let mut d = Dma { ram: &mut ram, base: BASE };
        start(&mut s, &mut d, 17, 9, 1, 1 << 4);
        s.read(0x20);
        s.write(0x2f, 2, 1, &mut d); // SW_RST_CMD
        for _ in 1..128 {
            assert_eq!(s.read(0x20), 0x0909_0909);
        }
        assert_ne!(s.read(0x30) & INT_XFER_DONE, 0);
    }
}
