//! EMMC2: the BCM2711's SD host controller (SDHCI v3 register set, driven by Linux's
//! `sdhci-iproc` as "brcm,bcm2711-emmc2") with an SDHC card behind it.
//!
//! Data moves by PIO through the buffer data port: the capabilities register advertises no
//! SDMA/ADMA, so the driver never programs DMA. Accesses may be 8, 16 or 32 bits wide; each
//! write is applied with byte enables, then its side effects run (command issue on the command
//! register's bytes, W1C on interrupt status, software reset, clock-stable handshake).
//!
//! Card model: SD physical layer 3.0 SDHC/SDXC, block addressed, 3.3 V only (no UHS voltage
//! switch), 4-bit bus, high speed via CMD6. Supported commands: 0, 2, 3, 6, 7, 8, 9, 10, 12, 13,
//! 16, 17, 18, 23, 24, 25, 55 and ACMD 6, 13, 23, 41, 42, 51. Anything else times out, which is
//! how a real SD card answers SDIO/MMC probes (CMD5, CMD1, CMD52).

/// Storage behind the card (512-byte sectors).
pub trait Disk {
    fn sectors(&self) -> u64;
    fn read(&mut self, lba: u64, out: &mut [u8]);
    fn write(&mut self, lba: u64, data: &[u8]);
    /// Sectors written since the image was opened (copy-on-write disks), for snapshots.
    fn overlay(&self) -> Vec<(u64, Vec<u8>)> { Vec::new() }
}

/// A RAM disk (tests, small images).
pub struct MemDisk(pub Vec<u8>);
impl Disk for MemDisk {
    fn sectors(&self) -> u64 { self.0.len() as u64 / 512 }
    fn read(&mut self, lba: u64, out: &mut [u8]) {
        let o = (lba * 512) as usize;
        let end = (o + out.len()).min(self.0.len());
        out.fill(0);
        if o < end { out[..end - o].copy_from_slice(&self.0[o..end]); }
    }
    fn write(&mut self, lba: u64, data: &[u8]) {
        let o = (lba * 512) as usize;
        let end = (o + data.len()).min(self.0.len());
        if o < end { self.0[o..end].copy_from_slice(&data[..end - o]); }
    }
}

// normal interrupt status bits
const INT_CMD_DONE: u32 = 1 << 0;
const INT_XFER_DONE: u32 = 1 << 1;
const INT_BUF_WR: u32 = 1 << 4;
const INT_BUF_RD: u32 = 1 << 5;
const INT_CARD_IN: u32 = 1 << 6;
const INT_ERROR: u32 = 1 << 15;
// error interrupt status bits (in the upper half of the 0x30 word)
const ERR_CMD_TIMEOUT: u32 = 1 << 16;
const ERR_DATA_TIMEOUT: u32 = 1 << 20;

// present state
const PS_CMD_INHIBIT: u32 = 1 << 0;
const PS_DAT_INHIBIT: u32 = 1 << 1;
const PS_DAT_ACTIVE: u32 = 1 << 2;
const PS_WR_ACTIVE: u32 = 1 << 8;
const PS_RD_ACTIVE: u32 = 1 << 9;
const PS_BUF_WR: u32 = 1 << 10;
const PS_BUF_RD: u32 = 1 << 11;
const PS_CARD: u32 = (1 << 16) | (1 << 17) | (1 << 18) | (1 << 19) | (0xf << 20) | (1 << 24);

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum State { Idle, Ready, Ident, Standby, Transfer }

#[derive(Clone, Copy, PartialEq, Eq)]
enum Xfer { None, Read, Write }

pub struct Sdhci {
    regs: [u32; 64],
    pub disk: Option<Box<dyn Disk>>,
    state: State,
    rca: u16,
    app: bool,
    xfer: Xfer,
    /// current block buffer and position
    buf: Vec<u8>,
    pos: usize,
    /// next sector for multi-block transfers; blocks left (None = until CMD12)
    lba: u64,
    left: Option<u32>,
    /// data for non-sector reads (SCR, SD status, CMD6 status)
    special: Option<Vec<u8>>,
    pre_count: Option<u32>,
    pub reads: u64,
    pub writes: u64,
    /// log commands to stderr
    pub trace: bool,
}

const BLKSIZE: usize = 1; // 0x04
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
const VERSION: usize = 0xfc / 4;

impl Default for Sdhci {
    fn default() -> Self { Self::new(None) }
}

impl Sdhci {
    pub fn new(disk: Option<Box<dyn Disk>>) -> Self {
        let mut s = Sdhci { regs: [0; 64], disk, state: State::Idle, rca: 0, app: false, xfer: Xfer::None, buf: Vec::new(), pos: 0,
                            lba: 0, left: None, special: None, pre_count: None, reads: 0, writes: 0, trace: false };
        s.reset_all();
        s
    }

    fn reset_all(&mut self) {
        self.regs = [0; 64];
        // base clock 100 MHz, timeout clock 1 MHz units, 512-byte blocks, high speed, 3.3 V;
        // no SDMA (bit 22) and no ADMA2 (bit 19): the driver uses PIO
        self.regs[CAPS] = (100 << 8) | (1 << 7) | 1 | (1 << 21) | (1 << 24);
        self.regs[CAPS + 1] = 0;
        self.regs[VERSION] = 0x9902_0000; // vendor 0x99, SD host spec 3.00
        self.xfer = Xfer::None;
        if self.disk.is_some() { self.regs[INTSTAT] |= INT_CARD_IN; }
    }

    fn present(&self) -> u32 {
        let card = if self.disk.is_some() { PS_CARD } else { (1 << 18) | (1 << 19) | (0xf << 20) | (1 << 24) };
        let mut p = card;
        match self.xfer {
            Xfer::Read => p |= PS_DAT_INHIBIT | PS_DAT_ACTIVE | PS_RD_ACTIVE | if self.pos < self.buf.len() { PS_BUF_RD } else { 0 },
            Xfer::Write => p |= PS_DAT_INHIBIT | PS_DAT_ACTIVE | PS_WR_ACTIVE | if self.pos < self.buf.len() { PS_BUF_WR } else { 0 },
            Xfer::None => {}
        }
        let _ = PS_CMD_INHIBIT;
        p
    }

    pub fn irq(&self) -> bool {
        let st = self.status();
        st & self.regs[SIGEN] != 0
    }

    fn status(&self) -> u32 {
        let st = self.regs[INTSTAT];
        if st & 0xffff_0000 != 0 { st | INT_ERROR } else { st & !INT_ERROR }
    }

    fn raise(&mut self, bits: u32) {
        if self.trace { eprintln!("[sd]   raise {bits:#x} (enable {:#x} signal {:#x}) -> status {:#x}", self.regs[INTEN], self.regs[SIGEN], self.regs[INTSTAT] | (bits & (self.regs[INTEN] | INT_ERROR))); }
        // only bits enabled in the status-enable register latch
        self.regs[INTSTAT] |= bits & (self.regs[INTEN] | INT_ERROR);
    }

    pub fn read(&mut self, off: u64) -> u32 {
        let r = (off as usize >> 2) & 63;
        if self.trace && self.xfer != Xfer::None && r != DATA { eprintln!("[sd]   rd {:#04x} = {:#x}", r * 4, match r { PRESENT => self.present(), INTSTAT => self.status(), _ => self.regs[r] }); }
        match r {
            DATA => self.data_read(),
            PRESENT => self.present(),
            INTSTAT => self.status(),
            _ => self.regs[r],
        }
    }

    /// `size` bytes written at `off` (any alignment within a word).
    pub fn write(&mut self, off: u64, v: u32, size: u32) {
        let r = (off as usize >> 2) & 63;
        let shift = (off as u32 & 3) * 8;
        let mask = if size >= 4 { u32::MAX } else { ((1u32 << (8 * size)) - 1) << shift };
        let val = (v << shift) & mask;
        if self.trace && self.xfer != Xfer::None { eprintln!("[sd]   wr {:#04x} <- {val:#x} mask {mask:#x}", r * 4); }
        match r {
            DATA => { self.data_write(v, size); return; }
            INTSTAT => { self.regs[INTSTAT] &= !val; return; }
            PRESENT | CAPS | VERSION => return,
            r if r == CAPS + 1 => return,
            _ => {}
        }
        self.regs[r] = (self.regs[r] & !mask) | val;
        match r {
            CMD if mask & 0xffff_0000 != 0 => self.command(),
            CLOCK => {
                // internal clock enable -> stable; software reset bits self-clear
                if self.regs[CLOCK] & 1 != 0 { self.regs[CLOCK] |= 2; } else { self.regs[CLOCK] &= !2; }
                let rst = (self.regs[CLOCK] >> 24) & 7;
                if rst & 1 != 0 { self.reset_all(); self.state = State::Idle; }
                if rst & 6 != 0 { self.xfer = Xfer::None; self.regs[INTSTAT] &= if rst & 4 != 0 { !(INT_XFER_DONE | INT_BUF_RD | INT_BUF_WR) } else { !INT_CMD_DONE }; }
                self.regs[CLOCK] &= 0x00ff_ffff;
            }
            CTRL => {}
            _ => {}
        }
    }

    // ------------------------------------------------------------------ commands
    fn command(&mut self) {
        let word = self.regs[CMD];
        let xfer_mode = word & 0xffff;
        let idx = (word >> 24) & 0x3f;
        let resp_ty = (word >> 16) & 3;
        let data = word & (1 << 21) != 0;
        let arg = self.regs[ARG];
        let app = std::mem::take(&mut self.app);
        if self.trace { eprintln!("[sd] {}CMD{idx} arg={arg:#010x} mode={xfer_mode:#06x} blk={:#010x} state={:?}", if app { "A" } else { "" }, self.regs[BLKSIZE], self.state); }
        if self.disk.is_none() { return self.timeout(); }
        let r1 = |s: &Self| -> u32 {
            let st = match s.state { State::Idle => 0, State::Ready => 1, State::Ident => 2, State::Standby => 3, State::Transfer => 4 };
            (st << 9) | (1 << 8)
        };
        let resp: u32 = match (app, idx) {
            (_, 0) => { self.state = State::Idle; self.xfer = Xfer::None; 0 }
            (_, 8) => arg & 0xfff,
            (_, 55) => { self.app = true; r1(self) | (1 << 5) }
            (true, 41) => {
                if arg & 0x00ff_8000 != 0 && self.state == State::Idle { self.state = State::Ready; }
                // power-up done, CCS (SDHC), 2.7-3.6 V
                0x8000_0000 | (1 << 30) | 0x00ff_8000
            }
            (_, 2) => {
                self.state = State::Ident;
                return self.done_r2(self.cid());
            }
            (_, 3) => { self.state = State::Standby; self.rca = 0x5bc0; ((self.rca as u32) << 16) | 0x0500 }
            (_, 9) => return self.done_r2(self.csd()),
            (_, 10) => return self.done_r2(self.cid()),
            (_, 7) => {
                let r = r1(self);
                self.state = if (arg >> 16) as u16 == self.rca { State::Transfer } else { State::Standby };
                r
            }
            (true, 6) => r1(self), // bus width
            (true, 23) | (true, 42) => r1(self),
            (true, 51) => { self.special = Some(vec![0x02, 0x35, 0x80, 0x00, 0, 0, 0, 0]); r1(self) }
            (true, 13) => { let mut s = vec![0u8; 64]; s[0] = 0x80; self.special = Some(s); r1(self) }
            (false, 13) => r1(self),
            (_, 16) => r1(self),
            (_, 12) => { self.xfer = Xfer::None; self.raise(INT_XFER_DONE); r1(self) }
            (false, 23) => { self.pre_count = Some(arg); r1(self) }
            (false, 6) => {
                // SWITCH_FUNC status: group 1 supports default + high speed
                let mut s = vec![0u8; 64];
                s[0] = 0x00; s[1] = 0x64; // max current 100 mA
                s[12] = 0x80; s[13] = 0x03; // group 1 support: SDR12, SDR25 (+bit 15)
                s[16] = if arg & 0xf == 1 { 0x01 } else { 0x00 };
                self.special = Some(s);
                r1(self)
            }
            (_, 17) | (_, 18) | (_, 24) | (_, 25) => r1(self),
            _ => return self.timeout(),
        };
        self.regs[RESP0] = resp;
        if data { self.start_data(idx, arg, xfer_mode); }
        self.raise(INT_CMD_DONE);
        let _ = resp_ty;
    }

    fn timeout(&mut self) {
        if self.trace { eprintln!("[sd]   -> timeout"); }
        self.regs[RESP0] = 0;
        self.raise(ERR_CMD_TIMEOUT);
        self.regs[INTSTAT] |= ERR_CMD_TIMEOUT;
    }

    fn done_r2(&mut self, v: u128) {
        // SDHCI keeps R2 bits 127:8 (no CRC) in RESP0..3
        let v = v >> 8;
        for k in 0..4 { self.regs[RESP0 + k] = (v >> (32 * k)) as u32; }
        self.raise(INT_CMD_DONE);
    }

    fn start_data(&mut self, idx: u32, arg: u32, mode: u32) {
        let blksz = (self.regs[BLKSIZE] & 0xfff) as usize;
        let blkcnt = self.regs[BLKSIZE] >> 16;
        if let Some(sp) = self.special.take() {
            self.buf = sp;
            self.buf.truncate(blksz.max(1));
            self.pos = 0;
            self.left = Some(0);
            self.xfer = Xfer::Read;
            self.raise(INT_BUF_RD);
            return;
        }
        let multi = mode & (1 << 5) != 0;
        self.lba = arg as u64;
        self.left = if !multi { Some(0) } else if mode & 2 != 0 { Some(blkcnt.saturating_sub(1)) } else { self.pre_count.take().map(|c| c.saturating_sub(1)) };
        self.pre_count = None;
        self.buf = vec![0; 512];
        self.pos = 0;
        if idx == 17 || idx == 18 {
            self.xfer = Xfer::Read;
            self.load_block();
            self.raise(INT_BUF_RD);
        } else {
            self.xfer = Xfer::Write;
            self.raise(INT_BUF_WR);
        }
    }

    fn load_block(&mut self) {
        let lba = self.lba;
        let ok = self.disk.as_ref().is_some_and(|d| lba < d.sectors());
        if let (true, Some(d)) = (ok, self.disk.as_mut()) { d.read(lba, &mut self.buf); } else { self.buf.fill(0); }
        self.reads += 1;
        self.pos = 0;
    }

    fn data_read(&mut self) -> u32 {
        if self.trace && self.pos == 0 { eprintln!("[sd]   data read starts ({} bytes)", self.buf.len()); }
        if self.xfer != Xfer::Read || self.pos >= self.buf.len() { return 0; }
        let mut w = [0u8; 4];
        let n = (self.buf.len() - self.pos).min(4);
        w[..n].copy_from_slice(&self.buf[self.pos..self.pos + n]);
        self.pos += n;
        if self.pos >= self.buf.len() {
            self.regs[INTSTAT] &= !INT_BUF_RD;
            match self.left {
                Some(0) => { self.xfer = Xfer::None; self.raise(INT_XFER_DONE); }
                left => {
                    if let Some(l) = left { self.left = Some(l - 1); }
                    self.lba += 1;
                    self.load_block();
                    self.raise(INT_BUF_RD);
                }
            }
        }
        u32::from_le_bytes(w)
    }

    fn data_write(&mut self, v: u32, size: u32) {
        if self.xfer != Xfer::Write { return; }
        for k in 0..size as usize {
            if self.pos < self.buf.len() { self.buf[self.pos] = (v >> (8 * k)) as u8; self.pos += 1; }
        }
        if self.pos >= self.buf.len() {
            let lba = self.lba;
            if let Some(d) = self.disk.as_mut() { if lba < d.sectors() { d.write(lba, &self.buf); } }
            self.writes += 1;
            self.regs[INTSTAT] &= !INT_BUF_WR;
            match self.left {
                Some(0) => { self.xfer = Xfer::None; self.raise(INT_XFER_DONE); }
                left => {
                    if let Some(l) = left { self.left = Some(l - 1); }
                    self.lba += 1;
                    self.pos = 0;
                    self.raise(INT_BUF_WR);
                }
            }
        }
        let _ = ERR_DATA_TIMEOUT;
    }

    fn cid(&self) -> u128 {
        // MID 0x5b, OID "SE", PNM "SBCEM", PRV 1.0, PSN, MDT 2026-11
        let mut v: u128 = 0x5b << 120;
        v |= (u16::from_be_bytes(*b"SE") as u128) << 104;
        for (k, c) in b"SBCEM".iter().enumerate() { v |= (*c as u128) << (96 - 8 * k); }
        v |= 0x10 << 56;
        v |= 0x5bc0_0001u128 << 24;
        v |= ((26u128 << 4) | 11) << 8;
        v | 1
    }

    fn csd(&self) -> u128 {
        let sectors = self.disk.as_ref().map(|d| d.sectors()).unwrap_or(0);
        let c_size = (sectors / 1024).saturating_sub(1) as u128 & 0x3f_ffff;
        let mut v: u128 = 1 << 126; // CSD 2.0
        v |= 0x0e << 112; // TAAC
        v |= 0x32 << 96; // TRAN_SPEED 25 MHz
        v |= 0x5b5 << 84; // CCC
        v |= 9 << 80; // READ_BL_LEN 512
        v |= c_size << 48;
        v |= 0x7f << 39; // SECTOR_SIZE
        v |= 2 << 26; // R2W_FACTOR
        v |= 9 << 22; // WRITE_BL_LEN
        v | 1
    }
}

impl Sdhci {
    pub fn save_state(&self, w: &mut emu_core::snap::W) {
        w.tag("sdhci-v1");
        for v in self.regs { w.u32(v); }
        w.u8(match self.state { State::Idle => 0, State::Ready => 1, State::Ident => 2, State::Standby => 3, State::Transfer => 4 });
        w.u16(self.rca); w.bool(self.app);
        w.u8(match self.xfer { Xfer::None => 0, Xfer::Read => 1, Xfer::Write => 2 });
        w.bytes(&self.buf); w.u64(self.pos as u64); w.u64(self.lba); w.opt_u32(self.left);
        w.bool(self.special.is_some()); w.bytes(self.special.as_deref().unwrap_or(&[]));
        w.opt_u32(self.pre_count);
        let ov = self.disk.as_ref().map(|d| d.overlay()).unwrap_or_default();
        w.u64(ov.len() as u64);
        for (lba, data) in ov { w.u64(lba); w.bytes(&data); }
    }
    pub fn load_state(&mut self, r: &mut emu_core::snap::R) -> Result<(), emu_core::snap::SnapError> {
        r.tag("sdhci-v1")?;
        for v in self.regs.iter_mut() { *v = r.u32()?; }
        self.state = match r.u8()? { 0 => State::Idle, 1 => State::Ready, 2 => State::Ident, 3 => State::Standby, _ => State::Transfer };
        self.rca = r.u16()?; self.app = r.bool()?;
        self.xfer = match r.u8()? { 0 => Xfer::None, 1 => Xfer::Read, _ => Xfer::Write };
        self.buf = r.bytes()?.to_vec(); self.pos = r.u64()? as usize; self.lba = r.u64()?; self.left = r.opt_u32()?;
        let has = r.bool()?; let sp = r.bytes()?.to_vec(); self.special = has.then_some(sp);
        self.pre_count = r.opt_u32()?;
        let n = r.u64()?;
        for _ in 0..n {
            let lba = r.u64()?;
            let data = r.bytes()?;
            if let Some(d) = self.disk.as_mut() { d.write(lba, data); }
        }
        Ok(())
    }
}
