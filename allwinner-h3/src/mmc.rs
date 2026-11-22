//! Allwinner SD/MMC host controller (`allwinner,sun7i-a20-mmc` on the H3, Linux
//! `sunxi-mmc`) with its internal DMA (IDMAC). Linux always moves data with the IDMAC:
//! a chain of 16-byte descriptors in RAM. Transfers complete when the command is issued;
//! the card protocol is `emu_core::sdcard::SdCard`.
use emu_core::sdcard::{Data, Response, SdCard, OUT_OF_RANGE};
use emu_core::{Bus, FlatRam};

const GCTRL: usize = 0x00;
const BYTCNT: usize = 0x14;
const CMDR: usize = 0x18;
const CMDARG: usize = 0x1c;
const RESP0: usize = 0x20;
const IMASK: usize = 0x30;
const DMAC: usize = 0x80;
const DLBA: usize = 0x84;
const IDIE: usize = 0x8c;

// GCTRL
const SOFT_RESET: u32 = 1 << 0;
const FIFO_RESET: u32 = 1 << 1;
const DMA_RESET: u32 = 1 << 2;
const INT_ENABLE: u32 = 1 << 4;
const DMA_ENABLE: u32 = 1 << 5;
// DMAC
const IDMA_ON: u32 = 1 << 7;
/// Largest transfer Linux issues (the driver's max request size); larger byte counts
/// are refused instead of sizing a host buffer from a guest register.
const MAX_TRANSFER: usize = 16 << 20;
// CMDR
const RESP_EXPIRE: u32 = 1 << 6;
const DATA_EXPIRE: u32 = 1 << 9;
const WRITE: u32 = 1 << 10;
const SEND_AUTO_STOP: u32 = 1 << 12;
const UPCLK_ONLY: u32 = 1 << 21;
const START: u32 = 1 << 31;
// RINTR / MISTA
const COMMAND_DONE: u32 = 1 << 2;
const DATA_OVER: u32 = 1 << 3;
const RESP_TIMEOUT: u32 = 1 << 8;
const DATA_TIMEOUT: u32 = 1 << 9;
const AUTO_COMMAND_DONE: u32 = 1 << 14;
// IDST
const IDMAC_TX: u32 = 1 << 0;
const IDMAC_RX: u32 = 1 << 1;
const IDMAC_NORMAL: u32 = 1 << 8;
// descriptor config
const DESC_LAST: u32 = 1 << 2;
const DESC_OWN: u32 = 1 << 31;

pub struct SunxiMmc {
    pub card: SdCard,
    regs: [u32; 64],
    /// Raw interrupt status (RINTR).
    raw: u32,
    /// IDMAC status (IDST).
    idst: u32,
    /// RAM ranges written by DMA since the machine last looked (compiled code there is
    /// stale).
    pub dma_written: Vec<(u64, usize)>,
}

impl Default for SunxiMmc {
    fn default() -> Self {
        let mut card = SdCard::new(None);
        card.trace = std::env::var_os("SBC_MMC_TRACE").is_some();
        SunxiMmc { card, regs: [0; 64], raw: 0, idst: 0, dma_written: Vec::new() }
    }
}

impl SunxiMmc {
    pub fn irq(&self) -> bool {
        self.regs[GCTRL / 4] & INT_ENABLE != 0 && self.raw & self.regs[IMASK / 4] != 0
            || self.idst & self.regs[IDIE / 4] & 0x3ff != 0
    }

    pub fn read(&mut self, offset: u64) -> u32 {
        match offset as usize {
            0x34 => self.raw & self.regs[IMASK / 4], // MISTA
            0x38 => self.raw,                        // RINTR
            // STAS: FIFO empty, card present, not busy
            0x3c => 1 << 2 | u32::from(self.card.present()) << 8,
            0x88 => self.idst,
            off if off < 0x100 => self.regs[off / 4],
            _ => 0, // FIFO: Linux uses the IDMAC only
        }
    }

    pub fn write(&mut self, offset: u64, value: u32, ram: &mut FlatRam) {
        match offset as usize {
            GCTRL => {
                // Reset bits complete at once; a soft reset clears the interrupt state.
                if value & SOFT_RESET != 0 {
                    self.raw = 0;
                    self.idst = 0;
                }
                self.regs[GCTRL / 4] = value & !(SOFT_RESET | FIFO_RESET | DMA_RESET);
            }
            0x38 => self.raw &= !value,
            0x88 => self.idst &= !value,
            DMAC => self.regs[DMAC / 4] = value & !1, // soft reset self-clears
            CMDR => {
                self.regs[CMDR / 4] = value & !START;
                if value & START != 0 && value & UPCLK_ONLY == 0 {
                    self.command(value, ram);
                }
            }
            off if off < 0x100 => self.regs[off / 4] = value,
            _ => {}
        }
    }

    fn command(&mut self, cmd: u32, ram: &mut FlatRam) {
        let index = cmd & 0x3f;
        let arg = self.regs[CMDARG / 4];
        let reply = self.card.command(index, arg);
        let expects_response = cmd & RESP_EXPIRE != 0;
        match reply.response {
            None if expects_response => {
                self.raw |= RESP_TIMEOUT | COMMAND_DONE;
                return;
            }
            None => {}
            Some(Response::Short(word)) => self.regs[RESP0 / 4] = word,
            Some(Response::Long(value)) => {
                for i in 0..4 {
                    self.regs[RESP0 / 4 + i] = (value >> (32 * i)) as u32;
                }
            }
        }
        if cmd & DATA_EXPIRE == 0 {
            self.raw |= COMMAND_DONE;
            return;
        }
        let bytes = self.regs[BYTCNT / 4] as usize;
        // Only the internal DMA moves data (Linux enables it; there is no PIO FIFO model).
        let dma = self.regs[GCTRL / 4] & DMA_ENABLE != 0 && self.regs[DMAC / 4] & IDMA_ON != 0;
        let ok = dma && bytes <= MAX_TRANSFER && match reply.data {
            Data::Bytes(mut data) if cmd & WRITE == 0 => {
                data.resize(bytes, 0);
                self.dma_to_memory(&data, ram)
            }
            Data::Read { lba } if cmd & WRITE == 0 && bytes % 512 == 0 => {
                if self.card.in_range(lba, (bytes / 512) as u64) {
                    let mut data = vec![0; bytes];
                    self.card.read_blocks(lba, &mut data);
                    self.dma_to_memory(&data, ram)
                } else {
                    false
                }
            }
            Data::Write { lba } if cmd & WRITE != 0 && bytes % 512 == 0 => {
                if self.card.in_range(lba, (bytes / 512) as u64) {
                    match self.dma_from_memory(bytes, ram) {
                        Some(data) => {
                            self.card.write_blocks(lba, &data);
                            true
                        }
                        None => false,
                    }
                } else {
                    false
                }
            }
            _ => false,
        };
        if !ok {
            self.regs[RESP0 / 4] |= OUT_OF_RANGE;
            self.raw |= COMMAND_DONE | DATA_TIMEOUT;
            return;
        }
        self.raw |= COMMAND_DONE | DATA_OVER;
        if cmd & SEND_AUTO_STOP != 0 {
            // The controller sends CMD12 itself after the data.
            if let Some(Response::Short(word)) = self.card.command(12, 0).response {
                self.regs[RESP0 / 4 + 3] = word; // auto-command response
            }
            self.raw |= AUTO_COMMAND_DONE;
        }
        self.idst |= if cmd & WRITE != 0 { IDMAC_TX } else { IDMAC_RX } | IDMAC_NORMAL;
    }

    /// Walk the IDMAC descriptor chain, calling `chunk(buffer address, length)` for each
    /// owned descriptor until `bytes` are covered; descriptors are handed back to the CPU.
    fn walk(&mut self, mut bytes: usize, ram: &mut FlatRam, mut chunk: impl FnMut(&mut FlatRam, u64, usize) -> bool) -> bool {
        let mut desc = self.regs[DLBA / 4] as u64;
        for _ in 0..4096 {
            if bytes == 0 {
                return true;
            }
            let Ok(config) = ram.read32(desc) else { return false };
            if config & DESC_OWN == 0 {
                return false;
            }
            let size = match ram.read32(desc + 4).map(|v| v & 0xffff) {
                Ok(0) => 0x1_0000,
                Ok(n) => n as usize,
                Err(_) => return false,
            };
            let (Ok(buffer), Ok(next)) = (ram.read32(desc + 8), ram.read32(desc + 12)) else {
                return false;
            };
            let n = size.min(bytes);
            if !chunk(ram, buffer as u64, n) {
                return false;
            }
            bytes -= n;
            let _ = ram.write32(desc, config & !DESC_OWN);
            if config & DESC_LAST != 0 {
                return bytes == 0;
            }
            desc = next as u64;
        }
        false
    }

    fn dma_to_memory(&mut self, data: &[u8], ram: &mut FlatRam) -> bool {
        let mut pos = 0;
        let mut written = std::mem::take(&mut self.dma_written);
        let ok = self.walk(data.len(), ram, |ram, address, n| {
            let ok = ram.load(address, &data[pos..pos + n]).is_ok();
            written.push((address, n));
            pos += n;
            ok
        });
        self.dma_written = written;
        ok
    }

    fn dma_from_memory(&mut self, bytes: usize, ram: &mut FlatRam) -> Option<Vec<u8>> {
        let mut data = Vec::with_capacity(bytes);
        let ok = self.walk(bytes, ram, |ram, address, n| {
            let start = address.checked_sub(ram.base).map(|o| o as usize);
            match start.filter(|&s| s + n <= ram.mem.len()) {
                Some(s) => {
                    data.extend_from_slice(&ram.mem[s..s + n]);
                    true
                }
                None => false,
            }
        });
        ok.then_some(data)
    }
}

emu_core::snap_struct!(SunxiMmc { card, regs, raw, idst, dma_written });

#[cfg(test)]
mod tests {
    use super::*;
    use emu_core::disk::MemDisk;

    const RAM: u64 = 0x4000_0000;

    fn setup() -> (SunxiMmc, FlatRam) {
        let mut mmc = SunxiMmc::default();
        let mut disk = MemDisk::new(2048);
        disk.0[512..516].copy_from_slice(b"SECT");
        mmc.card.disk = Some(Box::new(disk));
        mmc.regs[GCTRL / 4] = INT_ENABLE | DMA_ENABLE;
        mmc.regs[DMAC / 4] = IDMA_ON;
        mmc.regs[IMASK / 4] = u32::MAX;
        mmc.regs[IDIE / 4] = IDMAC_RX | IDMAC_TX | IDMAC_NORMAL;
        (mmc, FlatRam::new(RAM, 0x10000))
    }

    fn issue(mmc: &mut SunxiMmc, ram: &mut FlatRam, cmd: u32, arg: u32) {
        mmc.write(CMDARG as u64, arg, ram);
        mmc.write(CMDR as u64, START | RESP_EXPIRE | cmd, ram);
    }

    /// Two descriptors of 512 bytes each at RAM+0x100, buffers at RAM+0x1000 / RAM+0x1200.
    fn descriptors(ram: &mut FlatRam) {
        for (i, (buffer, last)) in [(0x1000u32, false), (0x1200, true)].into_iter().enumerate() {
            let d = RAM + 0x100 + i as u64 * 16;
            ram.write32(d, DESC_OWN | if last { DESC_LAST } else { 0 }).unwrap();
            ram.write32(d + 4, 512).unwrap();
            ram.write32(d + 8, RAM as u32 + buffer).unwrap();
            ram.write32(d + 12, (d + 16) as u32).unwrap();
        }
    }

    #[test]
    fn multi_block_read_through_the_idmac_with_auto_stop() {
        let (mut mmc, mut ram) = setup();
        for (cmd, arg) in [(0, 0), (8, 0x1aa), (55, 0), (41, 0x40ff_8000), (2, 0), (3, 0), (7, 0x5bc0 << 16)] {
            issue(&mut mmc, &mut ram, cmd, arg);
        }
        descriptors(&mut ram);
        mmc.write(DLBA as u64, RAM as u32 + 0x100, &mut ram);
        mmc.write(BYTCNT as u64, 1024, &mut ram);
        mmc.write(0x38, u32::MAX, &mut ram);
        issue(&mut mmc, &mut ram, 18 | DATA_EXPIRE | SEND_AUTO_STOP, 1);
        assert_eq!(&ram.mem[0x1000..0x1004], b"SECT");
        assert_eq!(mmc.read(0x38) & (COMMAND_DONE | DATA_OVER | AUTO_COMMAND_DONE), COMMAND_DONE | DATA_OVER | AUTO_COMMAND_DONE);
        assert_ne!(mmc.read(0x88) & IDMAC_RX, 0);
        assert_eq!(ram.read32(RAM + 0x100).unwrap() & DESC_OWN, 0, "descriptor returned");
        assert!(mmc.irq());
        assert_eq!(mmc.read(CMDR as u64) & START, 0);
    }

    #[test]
    fn no_transfer_without_dma_or_with_an_oversized_byte_count() {
        let (mut mmc, mut ram) = setup();
        descriptors(&mut ram);
        mmc.write(DLBA as u64, RAM as u32 + 0x100, &mut ram);
        mmc.write(BYTCNT as u64, 1024, &mut ram);
        mmc.regs[DMAC / 4] = 0; // IDMAC off
        issue(&mut mmc, &mut ram, 18 | DATA_EXPIRE, 1);
        assert_ne!(mmc.read(0x38) & DATA_TIMEOUT, 0);
        assert_eq!(&ram.mem[0x1000..0x1004], &[0; 4], "no DMA into guest memory");
        mmc.regs[DMAC / 4] = IDMA_ON;
        mmc.write(0x38, u32::MAX, &mut ram);
        mmc.write(BYTCNT as u64, 0xffff_fe00, &mut ram);
        issue(&mut mmc, &mut ram, 18 | DATA_EXPIRE, 1);
        assert_ne!(mmc.read(0x38) & DATA_TIMEOUT, 0, "refused, no 4 GiB host buffer");
    }

    #[test]
    fn write_then_out_of_range_and_absent_card() {
        let (mut mmc, mut ram) = setup();
        descriptors(&mut ram);
        ram.mem[0x1000..0x1004].copy_from_slice(b"WRIT");
        mmc.write(DLBA as u64, RAM as u32 + 0x100, &mut ram);
        mmc.write(BYTCNT as u64, 1024, &mut ram);
        issue(&mut mmc, &mut ram, 25 | DATA_EXPIRE | WRITE, 10);
        let mut out = [0u8; 512];
        mmc.card.read_blocks(10, &mut out);
        assert_eq!(&out[..4], b"WRIT");
        mmc.write(0x38, u32::MAX, &mut ram);
        issue(&mut mmc, &mut ram, 18 | DATA_EXPIRE, 2047);
        assert_ne!(mmc.read(0x38) & DATA_TIMEOUT, 0, "two blocks from the last sector");
        mmc.card.disk = None;
        mmc.write(0x38, u32::MAX, &mut ram);
        issue(&mut mmc, &mut ram, 8, 0x1aa);
        assert_eq!(mmc.read(0x38) & (RESP_TIMEOUT | COMMAND_DONE), RESP_TIMEOUT | COMMAND_DONE);
        assert_eq!(mmc.read(0x3c) & (1 << 8), 0);
    }
}
