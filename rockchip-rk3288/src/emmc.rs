//! An eMMC device (JEDEC eMMC 5.1 command set, the subset Linux's MMC core uses):
//! CMD1 power-up with the access mode in the OCR, CID/CSD, EXT_CSD (read with CMD8,
//! written with CMD6 SWITCH), block reads and writes, erase. It answers the controller
//! through the same `Reply` type as `emu_core::sdcard::SdCard`, and ignores the SD and
//! SDIO probe commands (CMD5, CMD8 before selection, CMD55) as a real eMMC does, so the
//! core's SDIO -> SD -> MMC probe order ends at MMC.
//!
//! Devices above 2 GiB are sector addressed; smaller ones are byte addressed and report
//! their capacity in the CSD (Linux reads `EXT_CSD_SEC_CNT` only for > 2 GiB).
//! Only HS52 is advertised (no HS200/DDR), so Linux needs no tuning or 1.8 V switch.
//! There are no boot or RPMB partitions.
use emu_core::disk::Disk;
use emu_core::sdcard::{CardState, Data, Reply, Response, OUT_OF_RANGE, READY_FOR_DATA};

/// R1 SWITCH_ERROR.
const SWITCH_ERROR: u32 = 1 << 7;

// EXT_CSD byte indexes (Linux include/linux/mmc/mmc.h).
#[cfg(test)]
const EXT_CSD_BUS_WIDTH: usize = 183;
const EXT_CSD_REV: usize = 192;
const EXT_CSD_STRUCTURE: usize = 194;
const EXT_CSD_CARD_TYPE: usize = 196;
const EXT_CSD_DRIVER_STRENGTH: usize = 197;
const EXT_CSD_SEC_CNT: usize = 212;
const EXT_CSD_S_A_TIMEOUT: usize = 217;
const EXT_CSD_REL_WR_SEC_C: usize = 222;
const EXT_CSD_HC_WP_GRP_SIZE: usize = 221;
const EXT_CSD_ERASE_TIMEOUT_MULT: usize = 223;
const EXT_CSD_HC_ERASE_GRP_SIZE: usize = 224;
const EXT_CSD_POWER_OFF_LONG_TIME: usize = 247;
const EXT_CSD_GENERIC_CMD6_TIME: usize = 248;
const EXT_CSD_PRE_EOL_INFO: usize = 267;
const EXT_CSD_S_CMD_SET: usize = 504;

pub struct EmmcCard {
    pub disk: Option<Box<dyn Disk>>,
    state: CardState,
    rca: u16,
    ext_csd: [u8; 512],
    erase: (u32, u32),
    /// Print every command to stderr.
    pub trace: bool,
}

impl EmmcCard {
    pub fn new(disk: Option<Box<dyn Disk>>) -> Self {
        let mut ext_csd = [0u8; 512];
        ext_csd[EXT_CSD_REV] = 8; // eMMC 5.1
        ext_csd[EXT_CSD_STRUCTURE] = 2;
        ext_csd[EXT_CSD_CARD_TYPE] = 0x03; // HS 26 and 52 MHz
        ext_csd[EXT_CSD_DRIVER_STRENGTH] = 1;
        ext_csd[EXT_CSD_S_A_TIMEOUT] = 0x10;
        ext_csd[EXT_CSD_REL_WR_SEC_C] = 1;
        ext_csd[EXT_CSD_HC_WP_GRP_SIZE] = 1;
        ext_csd[EXT_CSD_ERASE_TIMEOUT_MULT] = 1;
        ext_csd[EXT_CSD_HC_ERASE_GRP_SIZE] = 1; // 512 KiB
        ext_csd[EXT_CSD_POWER_OFF_LONG_TIME] = 10;
        ext_csd[EXT_CSD_GENERIC_CMD6_TIME] = 10; // 100 ms
        ext_csd[EXT_CSD_PRE_EOL_INFO] = 1; // normal
        ext_csd[EXT_CSD_S_CMD_SET] = 1;
        let mut card = EmmcCard { disk, state: CardState::Idle, rca: 0, ext_csd, erase: (0, 0), trace: false };
        card.update_capacity();
        card
    }

    /// Re-read the disk size into EXT_CSD (after `disk` changed).
    pub fn update_capacity(&mut self) {
        let sectors = self.sectors().min(u32::MAX as u64) as u32;
        self.ext_csd[EXT_CSD_SEC_CNT..EXT_CSD_SEC_CNT + 4].copy_from_slice(&sectors.to_le_bytes());
    }

    pub fn present(&self) -> bool {
        self.disk.is_some()
    }

    pub fn sectors(&self) -> u64 {
        self.disk.as_ref().map_or(0, |d| d.sectors())
    }

    /// Sector (block) addressing: devices larger than 2 GiB.
    fn block_addressed(&self) -> bool {
        self.sectors() > (2 << 30) / 512
    }

    pub fn in_range(&self, lba: u64, blocks: u64) -> bool {
        lba.checked_add(blocks).is_some_and(|end| end <= self.sectors())
    }

    pub fn read_blocks(&mut self, lba: u64, out: &mut [u8]) {
        match self.disk.as_mut() {
            Some(disk) => disk.read(lba, out),
            None => out.fill(0),
        }
    }

    pub fn write_blocks(&mut self, lba: u64, data: &[u8]) {
        if let Some(disk) = self.disk.as_mut() {
            disk.write(lba, data);
        }
    }

    pub fn status(&self) -> u32 {
        let state = match self.state {
            CardState::Idle => 0,
            CardState::Ready => 1,
            CardState::Ident => 2,
            CardState::Standby => 3,
            CardState::Transfer => 4,
        };
        state << 9 | READY_FOR_DATA
    }

    pub fn cid(&self) -> u128 {
        // MID 0x5b, CBX 1 (BGA), OID 0x53, PNM "SBCEMU", PRV 1.0, PSN, MDT, CRC
        0x5b01_5353_4243_454d_5510_5bc0_0001_1a01
    }

    pub fn csd(&self) -> u128 {
        // CSD_STRUCTURE 3 (version in EXT_CSD), SPEC_VERS 4. The CSD capacity is what
        // Linux uses for byte-addressed devices: (C_SIZE + 1) << (C_SIZE_MULT + 2)
        // blocks of 2^READ_BL_LEN bytes; sector-addressed devices report 0xfff.
        let sectors = self.sectors();
        let (read_bl_len, c_size) = if self.block_addressed() {
            (9u128, 0xfffu128)
        } else {
            let read_bl_len = if sectors > 1 << 21 { 10 } else { 9 };
            let unit = 1u64 << (7 + 2 + read_bl_len - 9); // sectors per C_SIZE step
            (read_bl_len as u128, (sectors / unit).clamp(1, 4096) as u128 - 1)
        };
        3 << 126
            | 4 << 122
            | 0x0e << 112 // TAAC
            | 0x32 << 96 // TRAN_SPEED 26 MHz
            | 0x8f5 << 84 // CCC
            | read_bl_len << 80
            | c_size << 62
            | 7 << 47 // C_SIZE_MULT
            | 0x1f << 42 // ERASE_GRP_SIZE
            | 0x1f << 37 // ERASE_GRP_MULT
            | 2 << 26 // R2W_FACTOR
            | 9 << 22 // WRITE_BL_LEN
            | 1
    }

    fn short(word: u32) -> Reply {
        Reply { response: Some(Response::Short(word)), data: Data::None }
    }

    fn timeout() -> Reply {
        Reply { response: None, data: Data::None }
    }

    /// The block a data command's argument addresses.
    fn block(&self, arg: u32) -> u64 {
        if self.block_addressed() {
            arg as u64
        } else {
            arg as u64 / 512
        }
    }

    pub fn command(&mut self, index: u32, arg: u32) -> Reply {
        if self.trace {
            eprintln!("[emmc] CMD{index} arg={arg:#010x} state={:?}", self.state);
        }
        if self.disk.is_none() {
            return Self::timeout();
        }
        let r1 = self.status();
        match index {
            0 => {
                self.state = CardState::Idle;
                Self::timeout()
            }
            1 => {
                if self.state == CardState::Idle {
                    self.state = CardState::Ready;
                }
                // R3: power-up done, access mode, 1.70-1.95 V and 2.7-3.6 V.
                let mode = if self.block_addressed() { 2 << 29 } else { 0 };
                Self::short(0x8000_0000 | mode | 0x00ff_8080)
            }
            2 if matches!(self.state, CardState::Ready) => {
                self.state = CardState::Ident;
                Reply { response: Some(Response::Long(self.cid())), data: Data::None }
            }
            3 if matches!(self.state, CardState::Ident) => {
                self.rca = (arg >> 16) as u16;
                self.state = CardState::Standby;
                Self::short(r1)
            }
            9 if self.state == CardState::Standby => Reply { response: Some(Response::Long(self.csd())), data: Data::None },
            10 if self.state == CardState::Standby => Reply { response: Some(Response::Long(self.cid())), data: Data::None },
            7 => {
                self.state = if (arg >> 16) as u16 == self.rca && self.rca != 0 {
                    CardState::Transfer
                } else {
                    CardState::Standby
                };
                Self::short(r1)
            }
            8 if self.state == CardState::Transfer => {
                Reply { response: Some(Response::Short(r1)), data: Data::Bytes(self.ext_csd.to_vec()) }
            }
            6 if self.state == CardState::Transfer => {
                let (access, index, value) = ((arg >> 24) & 3, (arg >> 16) as usize & 0xff, (arg >> 8) as u8);
                // The modes segment (index < 192) is writable; the properties segment is not.
                if index >= 192 || access == 0 {
                    return Self::short(r1 | SWITCH_ERROR);
                }
                let byte = &mut self.ext_csd[index];
                match access {
                    1 => *byte |= value,
                    2 => *byte &= !value,
                    _ => *byte = value,
                }
                if self.trace {
                    eprintln!("[emmc] SWITCH EXT_CSD[{index}] = {:#x}", self.ext_csd[index]);
                }
                Self::short(r1)
            }
            13 if self.state != CardState::Idle => Self::short(r1),
            12 | 16 | 23 if self.state == CardState::Transfer => Self::short(r1),
            // Erase group start/end (CMD35/36) and erase/trim/discard (CMD38). The blocks
            // keep their contents (Linux uses this for discard).
            35 if self.state == CardState::Transfer => {
                self.erase.0 = arg;
                Self::short(r1)
            }
            36 if self.state == CardState::Transfer => {
                self.erase.1 = arg;
                Self::short(r1)
            }
            38 if self.state == CardState::Transfer => {
                let (start, end) = (self.block(self.erase.0), self.block(self.erase.1));
                if start > end || end >= self.sectors() {
                    return Self::short(r1 | OUT_OF_RANGE);
                }
                Self::short(r1)
            }
            17 | 18 | 24 | 25 if self.state == CardState::Transfer => {
                let lba = self.block(arg);
                if lba >= self.sectors() {
                    return Self::short(r1 | OUT_OF_RANGE);
                }
                let data = if index <= 18 { Data::Read { lba } } else { Data::Write { lba } };
                Reply { response: Some(Response::Short(r1)), data }
            }
            _ => Self::timeout(),
        }
    }
}

impl emu_core::snap::Snap for EmmcCard {
    fn save(&self, w: &mut emu_core::snap::W) {
        let Self { disk, state, rca, ext_csd, erase, trace: _ } = self;
        state.save(w);
        rca.save(w);
        ext_csd.save(w);
        erase.save(w);
        emu_core::snap::save_disk(w, disk.as_deref());
    }
    fn load(&mut self, r: &mut emu_core::snap::R) -> Result<(), emu_core::snap::SnapError> {
        let Self { disk, state, rca, ext_csd, erase, trace: _ } = self;
        state.load(r)?;
        rca.load(r)?;
        ext_csd.load(r)?;
        erase.load(r)?;
        emu_core::snap::load_disk(r, disk.as_mut().map(|d| d.as_mut() as &mut dyn Disk), "an eMMC (--emmc)")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use emu_core::disk::MemDisk;

    fn selected(sectors: u64) -> EmmcCard {
        let mut card = EmmcCard::new(Some(Box::new(MemDisk::new(sectors))));
        assert_eq!(card.command(8, 0x1aa).response, None, "no SD CMD8 answer");
        assert_eq!(card.command(55, 0).response, None);
        card.command(0, 0);
        assert!(matches!(card.command(1, 0x40ff_8080).response, Some(Response::Short(ocr)) if ocr >> 31 == 1));
        card.command(2, 0);
        card.command(3, 1 << 16);
        card.command(7, 1 << 16);
        card
    }

    #[test]
    fn identification_ext_csd_and_switch() {
        let mut card = selected(4096);
        let Reply { data: Data::Bytes(ext), .. } = card.command(8, 0) else { panic!("EXT_CSD") };
        assert_eq!(ext.len(), 512);
        assert_eq!(u32::from_le_bytes(ext[212..216].try_into().unwrap()), 4096);
        card.command(6, 3 << 24 | (EXT_CSD_BUS_WIDTH as u32) << 16 | 2 << 8);
        let Reply { data: Data::Bytes(ext), .. } = card.command(8, 0) else { panic!("EXT_CSD") };
        assert_eq!(ext[EXT_CSD_BUS_WIDTH], 2);
        assert!(matches!(card.command(6, 3 << 24 | 200 << 16).response, Some(Response::Short(s)) if s & SWITCH_ERROR != 0));
    }

    #[test]
    fn small_devices_are_byte_addressed_with_the_csd_capacity() {
        let mut card = selected(2048 * 1024); // 1 GiB
        assert_eq!(card.command(17, 3 * 512).data, Data::Read { lba: 3 });
        let csd = card.csd();
        let (bl, c_size, mult) = ((csd >> 80) & 0xf, (csd >> 62) & 0xfff, (csd >> 47) & 7);
        assert_eq!((c_size + 1) << (mult + 2) << (bl - 9), 2048 * 1024);
        let mut big = selected((4 << 30) / 512);
        assert_eq!(big.command(18, 3).data, Data::Read { lba: 3 });
        assert_eq!((big.csd() >> 62) & 0xfff, 0xfff);
    }
}
