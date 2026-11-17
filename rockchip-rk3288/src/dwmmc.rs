//! Synopsys DesignWare Mobile Storage Host (`rockchip,rk3288-dw-mshc`, Linux `dw_mmc` +
//! `dw_mmc-rockchip`), version 2.70a with a 256-word FIFO and the internal DMA controller
//! (IDMAC, 32-bit descriptors, chained or ring). One instance per controller: the RK3288
//! has SDMMC (0xff0c0000), SDIO0 (0xff0d0000), SDIO1 (0xff0e0000) and eMMC (0xff0f0000);
//! each holds an SD card or an eMMC device (`CardKind`).
//!
//! Commands complete when they are written (CMD with START): the response, the data phase
//! and the interrupts (CMD_DONE, DATA_OVER, IDMAC RI/TI) are all raised at once. Linux
//! moves block data with the IDMAC; transfers under 16 bytes (the SD SCR) go through the
//! FIFO at 0x200 (PIO), which the model fills for reads and drains for writes.
//! The card-detect line is CDETECT bit 0 (low = card present); the Tinker Board's SD slot
//! is `broken-cd` (polled), the eMMC `non-removable`.
use crate::emmc::EmmcCard;
use emu_core::disk::Disk;
use emu_core::sdcard::{Data, Reply, Response, SdCard, OUT_OF_RANGE};
use emu_core::{Bus, FlatRam};
use std::collections::VecDeque;

pub const SIZE: u64 = 0x4000;

const CTRL: usize = 0x000;
const BYTCNT: usize = 0x020;
const INTMASK: usize = 0x024;
const CMDARG: usize = 0x028;
const CMD: usize = 0x02c;
const RESP0: usize = 0x030;
const MINTSTS: usize = 0x040;
const RINTSTS: usize = 0x044;
const STATUS: usize = 0x048;
const FIFOTH: usize = 0x04c;
const CDETECT: usize = 0x050;
const WRTPRT: usize = 0x054;
const TCBCNT: usize = 0x05c;
const TBBCNT: usize = 0x060;
const VERID: usize = 0x06c;
const HCON: usize = 0x070;
const BMOD: usize = 0x080;
const DBADDR: usize = 0x088;
const IDSTS: usize = 0x08c;
const IDINTEN: usize = 0x090;
const DSCADDR: usize = 0x094;
const BUFADDR: usize = 0x098;
/// Data FIFO (2.40a and later).
const DATA: usize = 0x200;

// CTRL
const CTRL_RESET: u32 = 1 << 0;
const CTRL_FIFO_RESET: u32 = 1 << 1;
const CTRL_DMA_RESET: u32 = 1 << 2;
const CTRL_INT_ENABLE: u32 = 1 << 4;
const CTRL_USE_IDMAC: u32 = 1 << 25;
// CMD
const CMD_START: u32 = 1 << 31;
const CMD_UPD_CLK: u32 = 1 << 21;
const CMD_SEND_STOP: u32 = 1 << 12;
const CMD_DAT_WR: u32 = 1 << 10;
const CMD_DAT_EXP: u32 = 1 << 9;
const CMD_RESP_EXP: u32 = 1 << 6;
// RINTSTS / INTMASK
const INT_ACD: u32 = 1 << 14;
const INT_EBE: u32 = 1 << 15;
const INT_DRTO: u32 = 1 << 9;
const INT_RTO: u32 = 1 << 8;
const INT_RXDR: u32 = 1 << 5;
const INT_TXDR: u32 = 1 << 4;
const INT_DATA_OVER: u32 = 1 << 3;
const INT_CMD_DONE: u32 = 1 << 2;
// BMOD
const BMOD_SWRESET: u32 = 1 << 0;
const BMOD_ENABLE: u32 = 1 << 7;
// IDSTS / IDINTEN
const IDMAC_TI: u32 = 1 << 0;
const IDMAC_RI: u32 = 1 << 1;
const IDMAC_FBE: u32 = 1 << 2;
const IDMAC_DU: u32 = 1 << 4;
const IDMAC_NI: u32 = 1 << 8;
const IDMAC_AI: u32 = 1 << 9;
// Descriptor DES0
const DES0_LD: u32 = 1 << 2;
const DES0_CH: u32 = 1 << 4;
const DES0_ER: u32 = 1 << 5;
const DES0_OWN: u32 = 1 << 31;

/// FIFO depth in 32-bit words (`fifo-depth = <0x100>` in rk3288.dtsi).
const FIFO_DEPTH: usize = 256;
/// Largest transfer accepted (Linux's request limit is far below); larger byte counts are
/// refused instead of sizing a host buffer from a guest register.
const MAX_TRANSFER: usize = 16 << 20;
/// VERID: 2.70a (Rockchip parts report 0x5342270a).
const VERSION: u32 = 0x5342_270a;
/// HCON: one SD/MMC slot, AHB, 32-bit host data, 32-bit addresses, internal DMA.
const HCONFIG: u32 = 1 | 1 << 6 | 1 << 7 | 0x1f << 10;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CardKind {
    /// Removable SD card (SDHC/SDXC model of `emu_core::sdcard`).
    Sd,
    /// Soldered eMMC device.
    Emmc,
}

/// The card in a slot.
pub enum Card {
    Sd(SdCard),
    Emmc(EmmcCard),
}

impl Card {
    pub fn new(kind: CardKind, disk: Option<Box<dyn Disk>>) -> Self {
        let trace = std::env::var_os("SBC_MMC_TRACE").is_some();
        match kind {
            CardKind::Sd => {
                let mut card = SdCard::new(disk);
                card.trace = trace;
                Card::Sd(card)
            }
            CardKind::Emmc => {
                let mut card = EmmcCard::new(disk);
                card.trace = trace;
                Card::Emmc(card)
            }
        }
    }

    pub fn kind(&self) -> CardKind {
        match self {
            Card::Sd(_) => CardKind::Sd,
            Card::Emmc(_) => CardKind::Emmc,
        }
    }

    pub fn present(&self) -> bool {
        match self {
            Card::Sd(c) => c.present(),
            Card::Emmc(c) => c.present(),
        }
    }

    fn command(&mut self, index: u32, arg: u32) -> Reply {
        match self {
            Card::Sd(c) => c.command(index, arg),
            Card::Emmc(c) => c.command(index, arg),
        }
    }

    fn in_range(&self, lba: u64, blocks: u64) -> bool {
        match self {
            Card::Sd(c) => c.in_range(lba, blocks),
            Card::Emmc(c) => c.in_range(lba, blocks),
        }
    }

    fn read_blocks(&mut self, lba: u64, out: &mut [u8]) {
        match self {
            Card::Sd(c) => c.read_blocks(lba, out),
            Card::Emmc(c) => c.read_blocks(lba, out),
        }
    }

    fn write_blocks(&mut self, lba: u64, data: &[u8]) {
        match self {
            Card::Sd(c) => c.write_blocks(lba, data),
            Card::Emmc(c) => c.write_blocks(lba, data),
        }
    }

    /// The backing disk (the boot firmware reads `/boot` through it).
    pub fn disk_mut(&mut self) -> Option<&mut Box<dyn Disk>> {
        match self {
            Card::Sd(c) => c.disk.as_mut(),
            Card::Emmc(c) => c.disk.as_mut(),
        }
    }
}

/// A PIO write in progress: the FIFO collects `bytes` before they go to the card.
struct PioWrite {
    lba: Option<u64>,
    bytes: usize,
    data: Vec<u8>,
}

pub struct DwMmc {
    /// Physical base address and GIC interrupt number (SPI + 32), for the machine.
    pub base: u64,
    pub irq: usize,
    pub card: Card,
    regs: [u32; 0x40],
    rintsts: u32,
    idsts: u32,
    fifo: VecDeque<u32>,
    pio_write: Option<PioWrite>,
    /// RAM ranges written by DMA since the machine last looked (compiled code there is
    /// stale for a JIT).
    pub dma_written: Vec<(u64, usize)>,
}

impl DwMmc {
    pub fn new(base: u64, irq: usize, kind: CardKind) -> Self {
        let mut regs = [0u32; 0x40];
        regs[FIFOTH / 4] = ((FIFO_DEPTH as u32) - 1) << 16;
        regs[0x14 / 4] = 0xffff_ff40; // TMOUT
        DwMmc {
            base,
            irq,
            card: Card::new(kind, None),
            regs,
            rintsts: 0,
            idsts: 0,
            fifo: VecDeque::new(),
            pio_write: None,
            dma_written: Vec::new(),
        }
    }

    /// Insert a card backed by `disk` (or empty the slot).
    pub fn insert(&mut self, disk: Option<Box<dyn Disk>>) {
        self.card = Card::new(self.card.kind(), disk);
    }

    pub fn contains(&self, addr: u64) -> bool {
        addr.wrapping_sub(self.base) < SIZE
    }

    pub fn irq_level(&self) -> bool {
        self.regs[CTRL / 4] & CTRL_INT_ENABLE != 0 && self.rintsts & self.regs[INTMASK / 4] != 0
            || self.idsts & self.regs[IDINTEN / 4] & 0x337 != 0
    }

    fn status(&self) -> u32 {
        let count = self.fifo.len() as u32;
        let mut status = count << 17;
        if count == 0 {
            status |= 1 << 2; // FIFO empty
        }
        if count as usize >= FIFO_DEPTH {
            status |= 1 << 3; // FIFO full
        }
        if self.card.present() {
            status |= 1 << 8; // DAT3: card present
        }
        status
    }

    pub fn read(&mut self, offset: u64) -> u32 {
        let offset = offset as usize;
        match offset {
            MINTSTS => self.rintsts & self.regs[INTMASK / 4],
            RINTSTS => self.rintsts,
            STATUS => self.status(),
            CDETECT => u32::from(!self.card.present()),
            WRTPRT => 0,
            VERID => VERSION,
            HCON => HCONFIG,
            IDSTS => self.idsts,
            DATA.. => self.fifo.pop_front().unwrap_or(0),
            _ if offset < 0x100 => self.regs[offset / 4],
            _ => 0,
        }
    }

    pub fn write(&mut self, offset: u64, value: u32, ram: &mut FlatRam) {
        let offset = offset as usize;
        match offset {
            CTRL => {
                // Resets complete at once.
                if value & CTRL_RESET != 0 {
                    self.rintsts = 0;
                    self.pio_write = None;
                }
                if value & (CTRL_RESET | CTRL_FIFO_RESET) != 0 {
                    self.fifo.clear();
                }
                self.regs[CTRL / 4] = value & !(CTRL_RESET | CTRL_FIFO_RESET | CTRL_DMA_RESET);
            }
            RINTSTS => self.rintsts &= !value,
            IDSTS => self.idsts &= !(value & 0x3ff),
            BMOD => self.regs[BMOD / 4] = value & !BMOD_SWRESET,
            CMD => {
                self.regs[CMD / 4] = value & !CMD_START;
                if value & CMD_START != 0 && value & CMD_UPD_CLK == 0 {
                    self.command(value, ram);
                }
            }
            MINTSTS | STATUS | CDETECT | WRTPRT | TCBCNT | TBBCNT | VERID | HCON | DSCADDR | BUFADDR => {}
            DATA.. => self.fifo_write(value),
            _ if offset < 0x100 => self.regs[offset / 4] = value,
            _ => {}
        }
    }

    fn uses_idmac(&self) -> bool {
        self.regs[CTRL / 4] & CTRL_USE_IDMAC != 0 && self.regs[BMOD / 4] & BMOD_ENABLE != 0
    }

    fn command(&mut self, cmd: u32, ram: &mut FlatRam) {
        let index = cmd & 0x3f;
        let arg = self.regs[CMDARG / 4];
        let reply = self.card.command(index, arg);
        match reply.response {
            None if cmd & CMD_RESP_EXP != 0 => {
                self.rintsts |= INT_RTO | INT_CMD_DONE;
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
        self.rintsts |= INT_CMD_DONE;
        if cmd & CMD_DAT_EXP == 0 {
            return;
        }
        let bytes = self.regs[BYTCNT / 4] as usize;
        let write = cmd & CMD_DAT_WR != 0;
        let ok = bytes <= MAX_TRANSFER
            && match reply.data {
                Data::Bytes(mut data) if !write => {
                    data.resize(bytes, 0);
                    self.to_host(&data, ram)
                }
                Data::Read { lba } if !write && bytes % 512 == 0 && self.card.in_range(lba, (bytes / 512) as u64) => {
                    let mut data = vec![0; bytes];
                    self.card.read_blocks(lba, &mut data);
                    self.to_host(&data, ram)
                }
                Data::Write { lba } if write && bytes % 512 == 0 && self.card.in_range(lba, (bytes / 512) as u64) => {
                    self.from_host(Some(lba), bytes, ram)
                }
                _ => false,
            };
        if !ok {
            self.regs[RESP0 / 4] |= OUT_OF_RANGE;
            self.rintsts |= INT_DRTO | if write { INT_EBE } else { 0 };
            if self.uses_idmac() {
                self.idsts |= IDMAC_AI | IDMAC_FBE;
            }
            return;
        }
        if cmd & CMD_SEND_STOP != 0 && self.pio_write.is_none() {
            self.auto_stop();
        }
    }

    /// CMD12 sent by the controller after the data (SEND_STOP).
    fn auto_stop(&mut self) {
        if let Some(Response::Short(word)) = self.card.command(12, 0).response {
            self.regs[RESP0 / 4 + 1] = word;
        }
        self.rintsts |= INT_ACD;
    }

    /// Card -> host: through the IDMAC, or into the FIFO for PIO.
    fn to_host(&mut self, data: &[u8], ram: &mut FlatRam) -> bool {
        if self.uses_idmac() {
            let mut pos = 0;
            let mut written = std::mem::take(&mut self.dma_written);
            let ok = self.walk(data.len(), ram, |ram, address, n| {
                let ok = ram.load(address, &data[pos..pos + n]).is_ok();
                written.push((address, n));
                pos += n;
                ok
            });
            self.dma_written = written;
            if ok {
                self.idsts |= IDMAC_RI | IDMAC_NI;
                self.rintsts |= INT_DATA_OVER;
            }
            return ok;
        }
        if data.len() > FIFO_DEPTH * 4 {
            return false; // Linux uses PIO for transfers under 16 bytes only
        }
        self.fifo.clear();
        for chunk in data.chunks(4) {
            let mut word = [0u8; 4];
            word[..chunk.len()].copy_from_slice(chunk);
            self.fifo.push_back(u32::from_le_bytes(word));
        }
        self.regs[TCBCNT / 4] = data.len() as u32;
        self.rintsts |= INT_RXDR | INT_DATA_OVER;
        true
    }

    /// Host -> card: through the IDMAC at once, or collected from the FIFO (PIO).
    fn from_host(&mut self, lba: Option<u64>, bytes: usize, ram: &mut FlatRam) -> bool {
        if !self.uses_idmac() {
            self.pio_write = Some(PioWrite { lba, bytes, data: Vec::with_capacity(bytes) });
            self.rintsts |= INT_TXDR;
            return true;
        }
        let mut data = Vec::with_capacity(bytes);
        let ok = self.walk(bytes, ram, |ram, address, n| {
            let start = address.checked_sub(ram.base).map(|o| o as usize);
            match start.filter(|&s| s.checked_add(n).is_some_and(|e| e <= ram.mem.len())) {
                Some(s) => {
                    data.extend_from_slice(&ram.mem[s..s + n]);
                    true
                }
                None => false,
            }
        });
        if !ok {
            return false;
        }
        if let Some(lba) = lba {
            self.card.write_blocks(lba, &data);
        }
        self.idsts |= IDMAC_TI | IDMAC_NI;
        self.rintsts |= INT_DATA_OVER;
        true
    }

    fn fifo_write(&mut self, value: u32) {
        let Some(pio) = self.pio_write.as_mut() else { return };
        pio.data.extend_from_slice(&value.to_le_bytes());
        if pio.data.len() >= pio.bytes {
            let pio = self.pio_write.take().expect("PIO write");
            if let Some(lba) = pio.lba {
                self.card.write_blocks(lba, &pio.data[..pio.bytes]);
            }
            self.regs[TCBCNT / 4] = pio.bytes as u32;
            self.rintsts |= INT_DATA_OVER;
            if self.regs[CMD / 4] & CMD_SEND_STOP != 0 {
                self.auto_stop();
            }
        }
    }

    /// Walk the IDMAC descriptor list from DBADDR, calling `chunk(buffer address, length)`
    /// for each owned buffer until `bytes` are covered; descriptors are handed back to the
    /// host (OWN cleared). Chained (CH) descriptors follow DES3; ring descriptors follow
    /// each other, back to DBADDR after the one marked end-of-ring (ER).
    fn walk(&mut self, mut bytes: usize, ram: &mut FlatRam, mut chunk: impl FnMut(&mut FlatRam, u64, usize) -> bool) -> bool {
        let base = self.regs[DBADDR / 4] as u64;
        let mut desc = base;
        for _ in 0..4096 {
            if bytes == 0 {
                self.regs[TCBCNT / 4] = self.regs[BYTCNT / 4];
                return true;
            }
            let Ok(des0) = ram.read32(desc) else { return false };
            if des0 & DES0_OWN == 0 {
                self.idsts |= IDMAC_DU | IDMAC_AI;
                return false;
            }
            let Ok(des1) = ram.read32(desc + 4) else { return false };
            let (Ok(buffer1), Ok(des3)) = (ram.read32(desc + 8), ram.read32(desc + 12)) else {
                return false;
            };
            // Buffer 1 (bits 12:0); in ring mode buffer 2 (bits 25:13) at DES3.
            let mut buffers = vec![(buffer1 as u64, (des1 & 0x1fff) as usize)];
            if des0 & DES0_CH == 0 {
                buffers.push((des3 as u64, ((des1 >> 13) & 0x1fff) as usize));
            }
            for (address, size) in buffers {
                let n = size.min(bytes);
                if n > 0 {
                    if !chunk(ram, address, n) {
                        return false;
                    }
                    bytes -= n;
                }
            }
            let _ = ram.write32(desc, des0 & !DES0_OWN);
            self.regs[DSCADDR / 4] = desc as u32;
            if des0 & DES0_LD != 0 {
                if bytes == 0 {
                    self.regs[TCBCNT / 4] = self.regs[BYTCNT / 4];
                }
                return bytes == 0;
            }
            desc = if des0 & DES0_CH != 0 {
                des3 as u64
            } else if des0 & DES0_ER != 0 {
                base
            } else {
                desc + 16
            };
        }
        false
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use emu_core::disk::MemDisk;

    const RAM: u64 = 0;

    fn setup(kind: CardKind) -> (DwMmc, FlatRam) {
        let mut mmc = DwMmc::new(0xff0c_0000, 32 + 32, kind);
        let mut disk = MemDisk::new(2048);
        disk.0[512..516].copy_from_slice(b"SECT");
        mmc.insert(Some(Box::new(disk)));
        let mut ram = FlatRam::new(RAM, 0x10000);
        mmc.write(INTMASK as u64, 0xffff, &mut ram);
        mmc.write(CTRL as u64, CTRL_INT_ENABLE | CTRL_USE_IDMAC, &mut ram);
        mmc.write(BMOD as u64, BMOD_ENABLE, &mut ram);
        mmc.write(IDINTEN as u64, IDMAC_NI | IDMAC_RI | IDMAC_TI, &mut ram);
        (mmc, ram)
    }

    #[test]
    fn registers_above_the_array_read_as_zero() {
        // CDTHRCTL (0x100), UHS_REG_EXT (0x108) and the rest of 0x100..0x200 must not panic.
        let (mut mmc, _) = setup(CardKind::Sd);
        for offset in (0x100..0x200).step_by(4) {
            assert_eq!(mmc.read(offset), 0);
        }
    }

    fn issue(mmc: &mut DwMmc, ram: &mut FlatRam, cmd: u32, arg: u32) {
        mmc.write(CMDARG as u64, arg, ram);
        mmc.write(CMD as u64, CMD_START | CMD_RESP_EXP | cmd, ram);
    }

    fn init_sd(mmc: &mut DwMmc, ram: &mut FlatRam) {
        for (cmd, arg) in [(0, 0), (8, 0x1aa), (55, 0), (41, 0x40ff_8000), (2, 0), (3, 0), (7, 0x5bc0 << 16)] {
            issue(mmc, ram, cmd, arg);
        }
    }

    /// Two chained descriptors of 512 bytes at 0x100, buffers at 0x1000 and 0x1200.
    fn descriptors(ram: &mut FlatRam) {
        for (i, (buffer, last)) in [(0x1000u32, false), (0x1200, true)].into_iter().enumerate() {
            let d = RAM + 0x100 + i as u64 * 16;
            ram.write32(d, DES0_OWN | DES0_CH | if last { DES0_LD } else { 0 }).unwrap();
            ram.write32(d + 4, 512).unwrap();
            ram.write32(d + 8, buffer).unwrap();
            ram.write32(d + 12, (d + 16) as u32).unwrap();
        }
    }

    #[test]
    fn idmac_multi_block_read_and_write() {
        let (mut mmc, mut ram) = setup(CardKind::Sd);
        init_sd(&mut mmc, &mut ram);
        descriptors(&mut ram);
        mmc.write(DBADDR as u64, 0x100, &mut ram);
        mmc.write(BYTCNT as u64, 1024, &mut ram);
        mmc.write(RINTSTS as u64, u32::MAX, &mut ram);
        issue(&mut mmc, &mut ram, 18 | CMD_DAT_EXP, 1);
        assert_eq!(&ram.mem[0x1000..0x1004], b"SECT");
        assert_eq!(mmc.read(RINTSTS as u64) & (INT_CMD_DONE | INT_DATA_OVER), INT_CMD_DONE | INT_DATA_OVER);
        assert_ne!(mmc.read(IDSTS as u64) & IDMAC_RI, 0);
        assert_eq!(ram.read32(0x100).unwrap() & DES0_OWN, 0, "descriptor returned");
        assert!(mmc.irq_level());
        assert_eq!(mmc.read(CMD as u64) & CMD_START, 0);
        // Write back two blocks at 10.
        descriptors(&mut ram);
        ram.mem[0x1200..0x1204].copy_from_slice(b"WRIT");
        issue(&mut mmc, &mut ram, 25 | CMD_DAT_EXP | CMD_DAT_WR, 10);
        let Card::Sd(card) = &mut mmc.card else { panic!() };
        let mut out = [0u8; 512];
        card.read_blocks(11, &mut out);
        assert_eq!(&out[..4], b"WRIT");
    }

    #[test]
    fn pio_read_of_the_scr_and_response_timeouts() {
        let (mut mmc, mut ram) = setup(CardKind::Sd);
        init_sd(&mut mmc, &mut ram);
        mmc.write(CTRL as u64, CTRL_INT_ENABLE, &mut ram); // no IDMAC
        mmc.write(BYTCNT as u64, 8, &mut ram);
        issue(&mut mmc, &mut ram, 55, 0x5bc0 << 16);
        issue(&mut mmc, &mut ram, 51 | CMD_DAT_EXP, 0);
        assert_eq!(mmc.read(STATUS as u64) >> 17 & 0x1fff, 2);
        assert_eq!(mmc.read(DATA as u64), 0x0080_3502);
        assert_ne!(mmc.read(RINTSTS as u64) & INT_DATA_OVER, 0);
        mmc.write(RINTSTS as u64, u32::MAX, &mut ram);
        issue(&mut mmc, &mut ram, 5, 0); // SDIO probe: no answer
        assert_eq!(mmc.read(RINTSTS as u64) & (INT_RTO | INT_CMD_DONE), INT_RTO | INT_CMD_DONE);
        mmc.insert(None);
        assert_eq!(mmc.read(CDETECT as u64) & 1, 1, "no card");
    }

    #[test]
    fn emmc_identification_and_ext_csd_by_dma() {
        let (mut mmc, mut ram) = setup(CardKind::Emmc);
        mmc.write(CMD as u64, CMD_START, &mut ram); // CMD0, no response
        for (cmd, arg) in [(1, 0x40ff_8080), (2, 0), (3, 1 << 16), (7, 1 << 16)] {
            issue(&mut mmc, &mut ram, cmd, arg);
            assert_eq!(mmc.read(RINTSTS as u64) & INT_RTO, 0, "CMD{cmd}");
        }
        ram.write32(0x100, DES0_OWN | DES0_CH | DES0_LD).unwrap();
        ram.write32(0x104, 512).unwrap();
        ram.write32(0x108, 0x2000).unwrap();
        mmc.write(DBADDR as u64, 0x100, &mut ram);
        mmc.write(BYTCNT as u64, 512, &mut ram);
        issue(&mut mmc, &mut ram, 8 | CMD_DAT_EXP, 0);
        assert_eq!(ram.read32(0x2000 + 212).unwrap(), 2048, "SEC_COUNT");
        // Byte addressing (small device): block 1 at byte 512.
        ram.write32(0x100, DES0_OWN | DES0_CH | DES0_LD).unwrap();
        issue(&mut mmc, &mut ram, 17 | CMD_DAT_EXP, 512);
        assert_eq!(&ram.mem[0x2000..0x2004], b"SECT");
    }
}
