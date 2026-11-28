//! The BCM2835 "sdhost" controller (`brcm,bcm2835-sdhost`, 0x7e20_2000): the SD card's host on
//! a real Pi 3 (the Arasan SDHCI carries the Wi-Fi chip), driven by the Raspberry Pi kernel's
//! `sdhost-bcm2835` (and upstream's `bcm2835` mmc driver, the same register interface).
//!
//! Registers: a command register whose NEW flag the driver polls (clear once the command and its
//! response are done, FAIL set on a timeout), the argument, four response words, the host status
//! (W1C interrupt bits, a live data flag), card power, the "emergency debug mode" register with
//! the data state machine and the FIFO's fill level, the host configuration (interrupt enables),
//! the block size and count, and the 16-word data FIFO.
//!
//! Data moves through the FIFO: by PIO through SDDATA (what this machine's driver uses: the
//! device tree gives it no DMA channel), or by the DMA controller on DREQ 13 through the same
//! `fifo_read`/`fifo_write` and `dreq`. Transfers are instant: a read's blocks are in the FIFO
//! as soon as the command is sent, a written block reaches the card when its last word arrives.
//! Interrupts as the driver expects them: the data flag (FIFO ready) while a transfer runs, a
//! block interrupt after each written block, a busy interrupt when an R1b command's busy ends.
//!
//! The card is the SDHCI model's (`bcm2711::sdhci`: same CID, CSD and command set): SDHC, block
//! addressed, 3.3 V, 4-bit, high speed via CMD6; its SCR also offers CMD23 (SET_BLOCK_COUNT), which
//! the driver then sends before multi-block transfers instead of a CMD12 after them. SDIO and MMC
//! probes (CMD5, CMD52, CMD1) time out.
//!
//! PIO costs the guest one interrupt per 512-byte block (the driver reads a block per data
//! interrupt): about 500 k during Raspberry Pi OS's boot. The vendor tree's DMA channel removes
//! them once the DMA controller is modelled.
use bcm2711::sdhci::Disk;

const SDCMD: u64 = 0x00;
const SDARG: u64 = 0x04;
const SDTOUT: u64 = 0x08;
const SDCDIV: u64 = 0x0c;
const SDRSP0: u64 = 0x10;
const SDHSTS: u64 = 0x20;
const SDVDD: u64 = 0x30;
const SDEDM: u64 = 0x34;
const SDHCFG: u64 = 0x38;
const SDHBCT: u64 = 0x3c;
const SDDATA: u64 = 0x40;
const SDHBLC: u64 = 0x50;

const CMD_NEW: u32 = 0x8000;
const CMD_FAIL: u32 = 0x4000;
const CMD_BUSYWAIT: u32 = 0x800;
const CMD_NO_RESPONSE: u32 = 0x400;
const CMD_LONG_RESPONSE: u32 = 0x200;
const CMD_WRITE: u32 = 0x80;
const CMD_READ: u32 = 0x40;

const HSTS_BUSY_IRPT: u32 = 0x400;
const HSTS_BLOCK_IRPT: u32 = 0x200;
const HSTS_SDIO_IRPT: u32 = 0x100;
const HSTS_CMD_TIME_OUT: u32 = 0x40;
const HSTS_DATA_FLAG: u32 = 0x01;
/// the latched (write-1-to-clear) bits
const HSTS_W1C: u32 = 0x7f8;

const HCFG_BUSY_IRPT_EN: u32 = 1 << 10;
const HCFG_BLOCK_IRPT_EN: u32 = 1 << 8;
const HCFG_SDIO_IRPT_EN: u32 = 1 << 5;
const HCFG_DATA_IRPT_EN: u32 = 1 << 4;

const EDM_FORCE_DATA_MODE: u32 = 1 << 19;
/// the FIFO thresholds the driver may set (bits 9..18); the FSM (0..3) and level (4..8) are live
const EDM_THRESHOLDS: u32 = 0x3ff << 9;
const FSM_IDENTMODE: u32 = 0x0;
const FSM_DATAMODE: u32 = 0x1;
const FSM_READDATA: u32 = 0x2;
const FSM_WRITEDATA: u32 = 0x3;

const FIFO_WORDS: usize = 16;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum State { Idle, Ready, Ident, Standby, Transfer }

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Xfer { None, Read, Write }

pub struct Sdhost {
    pub disk: Option<Box<dyn Disk>>,
    cmd: u32,
    arg: u32,
    tout: u32,
    cdiv: u32,
    rsp: [u32; 4],
    hsts: u32,
    vdd: u32,
    edm: u32,
    hcfg: u32,
    hbct: u32,
    hblc: u32,
    // the card
    state: State,
    rca: u16,
    app: bool,
    /// block count announced by CMD23 (SET_BLOCK_COUNT)
    pre_count: Option<u32>,
    /// data of the next non-sector read (SCR, SD status, CMD6 status)
    special: Option<Vec<u8>>,
    // the transfer
    xfer: Xfer,
    /// the block passing through the FIFO and the position in it
    buf: Vec<u8>,
    pos: usize,
    /// card sector of `buf`, blocks of the transfer after the current one
    lba: u64,
    left: u32,
    pub reads: u64,
    pub writes: u64,
    /// log commands to stderr (`SBC_SD_TRACE=1`)
    pub trace: bool,
}

impl Default for Sdhost { fn default() -> Self { Self::new(None) } }

impl Sdhost {
    pub fn new(disk: Option<Box<dyn Disk>>) -> Self {
        Sdhost {
            disk, cmd: 0, arg: 0, tout: 0, cdiv: 0, rsp: [0; 4], hsts: 0, vdd: 0, edm: 0, hcfg: 0, hbct: 0, hblc: 0,
            state: State::Idle, rca: 0, app: false, pre_count: None, special: None,
            xfer: Xfer::None, buf: Vec::new(), pos: 0, lba: 0, left: 0, reads: 0, writes: 0, trace: false,
        }
    }

    /// Words the FIFO holds (a read) or has room for (a write).
    fn fifo_level(&self) -> usize {
        match self.xfer {
            Xfer::Read => ((self.buf.len() - self.pos) / 4 + self.left as usize * self.blksz() / 4).min(FIFO_WORDS),
            _ => 0,
        }
    }

    fn fsm(&self) -> u32 {
        match self.xfer {
            Xfer::Read => FSM_READDATA,
            Xfer::Write => FSM_WRITEDATA,
            Xfer::None if self.state == State::Transfer => FSM_DATAMODE,
            Xfer::None => FSM_IDENTMODE,
        }
    }

    /// The data flag: words to read, or room to write, while a transfer runs.
    fn data_flag(&self) -> bool {
        match self.xfer { Xfer::Read => self.fifo_level() > 0, Xfer::Write => true, Xfer::None => false }
    }

    fn status(&self) -> u32 { self.hsts | if self.data_flag() { HSTS_DATA_FLAG } else { 0 } }

    /// The interrupt line: an enabled event, or the data flag with the data interrupt enabled.
    pub fn irq(&self) -> bool {
        let h = self.status();
        (h & HSTS_BUSY_IRPT != 0 && self.hcfg & HCFG_BUSY_IRPT_EN != 0)
            || (h & HSTS_BLOCK_IRPT != 0 && self.hcfg & HCFG_BLOCK_IRPT_EN != 0)
            || (h & HSTS_SDIO_IRPT != 0 && self.hcfg & HCFG_SDIO_IRPT_EN != 0)
            || (h & HSTS_DATA_FLAG != 0 && self.hcfg & HCFG_DATA_IRPT_EN != 0)
    }

    /// The DMA request (DREQ 13): the FIFO has data to take or room to fill.
    pub fn dreq(&self) -> bool { self.data_flag() }

    pub fn read(&mut self, off: u64) -> u32 {
        match off {
            SDCMD => self.cmd,
            SDARG => self.arg,
            SDTOUT => self.tout,
            SDCDIV => self.cdiv,
            0x10..=0x1c => self.rsp[((off - SDRSP0) / 4) as usize],
            SDHSTS => self.status(),
            SDVDD => self.vdd,
            SDEDM => (self.edm & EDM_THRESHOLDS) | ((self.fifo_level() as u32) << 4) | self.fsm(),
            SDHCFG => self.hcfg,
            SDHBCT => self.hbct,
            SDDATA => self.fifo_read(),
            SDHBLC => self.hblc,
            _ => 0,
        }
    }

    pub fn write(&mut self, off: u64, v: u32) {
        match off {
            SDCMD => {
                self.cmd = v & 0xffff;
                if v & CMD_NEW != 0 { self.command(); }
            }
            SDARG => self.arg = v,
            SDTOUT => self.tout = v,
            SDCDIV => self.cdiv = v & 0x7ff,
            SDHSTS => self.hsts &= !(v & HSTS_W1C),
            SDVDD => {
                // powering the card off resets it
                if v & 1 == 0 && self.vdd & 1 != 0 { self.state = State::Idle; self.rca = 0; self.app = false; self.abort(); }
                self.vdd = v & 1;
            }
            SDEDM => {
                self.edm = v & EDM_THRESHOLDS;
                // out of a read-wait or write-start state: nothing waits here, the transfer is done
                if v & EDM_FORCE_DATA_MODE != 0 && self.xfer != Xfer::None { self.abort(); }
            }
            SDHCFG => self.hcfg = v & 0x7ff,
            SDHBCT => self.hbct = v & 0x3ff,
            SDDATA => self.fifo_write(v),
            SDHBLC => self.hblc = v & 0xffff, // the documented 9 bits would not hold the 512-block requests Linux makes
            _ => {}
        }
    }

    fn blksz(&self) -> usize { (self.hbct as usize).clamp(4, 1024) }

    fn abort(&mut self) { self.xfer = Xfer::None; self.buf.clear(); self.pos = 0; self.left = 0; }

    fn r1(&self) -> u32 {
        let st = match self.state { State::Idle => 0, State::Ready => 1, State::Ident => 2, State::Standby => 3, State::Transfer => 4 };
        (st << 9) | (1 << 8)
    }

    fn command(&mut self) {
        let idx = self.cmd & 0x3f;
        let arg = self.arg;
        let app = std::mem::take(&mut self.app);
        if self.trace { eprintln!("[sdhost] {}CMD{idx} arg={arg:#010x} cmd={:#06x} blk={}x{} state={:?}", if app { "A" } else { "" }, self.cmd, self.hblc, self.hbct, self.state); }
        self.abort();
        if self.disk.is_none() || self.vdd & 1 == 0 { return self.timeout(); }
        let resp: u32 = match (app, idx) {
            (_, 0) => { self.state = State::Idle; 0 }
            (_, 8) => arg & 0xfff,
            (_, 55) => { self.app = true; self.r1() | (1 << 5) }
            (true, 41) => {
                if arg & 0x00ff_8000 != 0 && self.state == State::Idle { self.state = State::Ready; }
                // power-up done, CCS (SDHC), 2.7-3.6 V
                0x8000_0000 | (1 << 30) | 0x00ff_8000
            }
            (_, 2) | (_, 10) => {
                if idx == 2 { self.state = State::Ident; }
                return self.done_r2(bcm2711::sdhci::cid());
            }
            (_, 9) => return self.done_r2(bcm2711::sdhci::csd(self.disk.as_ref().map_or(0, |d| d.sectors()))),
            (_, 3) => { self.state = State::Standby; self.rca = 0x5bc0; ((self.rca as u32) << 16) | 0x0500 }
            (_, 7) => {
                let r = self.r1();
                self.state = if (arg >> 16) as u16 == self.rca { State::Transfer } else { State::Standby };
                r
            }
            (true, 6) | (true, 23) | (true, 42) => self.r1(),
            // SCR: SD 3.0, 1- and 4-bit, CMD23 supported (a block count up front: no CMD12 after each read)
            (true, 51) => { self.special = Some(vec![0x02, 0x35, 0x80, 0x02, 0, 0, 0, 0]); self.r1() }
            (true, 13) => { let mut s = vec![0u8; 64]; s[0] = 0x80; self.special = Some(s); self.r1() }
            (false, 13) | (_, 16) | (false, 32) | (false, 33) | (false, 38) | (_, 12) => self.r1(),
            (false, 23) => { self.pre_count = Some(arg & 0xffff); self.r1() }
            (false, 6) => {
                // SWITCH_FUNC status: group 1 supports default and high speed
                let mut s = vec![0u8; 64];
                s[1] = 0x64; // max current 100 mA
                s[12] = 0x80; s[13] = 0x03;
                s[16] = if arg & 0xf == 1 { 0x01 } else { 0x00 };
                self.special = Some(s);
                self.r1()
            }
            (_, 17) | (_, 18) | (_, 24) | (_, 25) => self.r1(),
            _ => return self.timeout(),
        };
        self.rsp = [resp, 0, 0, 0];
        self.cmd &= !CMD_NEW;
        if self.cmd & (CMD_READ | CMD_WRITE) != 0 { self.start_data(idx); }
        // the end of an R1b command's busy (an erase or a stop after writing: no wait here)
        if self.cmd & CMD_BUSYWAIT != 0 { self.hsts |= HSTS_BUSY_IRPT; }
        let _ = (CMD_NO_RESPONSE, CMD_LONG_RESPONSE);
    }

    fn timeout(&mut self) {
        if self.trace { eprintln!("[sdhost]   -> timeout"); }
        self.rsp = [0; 4];
        self.hsts |= HSTS_CMD_TIME_OUT;
        self.cmd = (self.cmd & !CMD_NEW) | CMD_FAIL;
        if self.cmd & CMD_BUSYWAIT != 0 { self.hsts |= HSTS_BUSY_IRPT; }
    }

    /// A 136-bit response: the driver reads SDRSP3..0 as the register's bits 127..0.
    fn done_r2(&mut self, v: u128) {
        for k in 0..4 { self.rsp[k] = (v >> (32 * k)) as u32; }
        self.cmd &= !CMD_NEW;
    }

    fn start_data(&mut self, idx: u32) {
        let blksz = self.blksz();
        let blocks = match self.pre_count.take() { Some(n) if n > 0 => n, _ => (self.hblc).max(1) };
        if let Some(mut sp) = self.special.take() {
            sp.truncate(blksz);
            sp.resize(blksz.max(4) & !3, 0);
            self.buf = sp;
            self.pos = 0;
            self.left = 0;
            self.xfer = Xfer::Read;
            return;
        }
        if !matches!(idx, 17 | 18 | 24 | 25) { return; }
        let multi = idx == 18 || idx == 25;
        self.lba = self.arg as u64;
        self.left = if multi { blocks - 1 } else { 0 };
        self.pos = 0;
        if idx == 17 || idx == 18 {
            self.xfer = Xfer::Read;
            self.load_block();
        } else {
            self.xfer = Xfer::Write;
            self.buf = Vec::with_capacity(blksz);
        }
    }

    fn load_block(&mut self) {
        let lba = self.lba;
        self.buf = vec![0; self.blksz()];
        match self.disk.as_mut() {
            Some(d) if lba < d.sectors() => d.read(lba, &mut self.buf),
            _ => {}
        }
        self.reads += 1;
        self.pos = 0;
    }

    /// A word out of the FIFO (SDDATA, or a DMA read).
    pub fn fifo_read(&mut self) -> u32 {
        if self.xfer != Xfer::Read || self.pos >= self.buf.len() { return 0; }
        let mut w = [0u8; 4];
        let n = (self.buf.len() - self.pos).min(4);
        w[..n].copy_from_slice(&self.buf[self.pos..self.pos + n]);
        self.pos += n;
        if self.pos >= self.buf.len() {
            if self.left == 0 {
                self.abort();
            } else {
                self.left -= 1;
                self.lba += 1;
                self.load_block();
            }
        }
        u32::from_le_bytes(w)
    }

    /// A word into the FIFO (SDDATA, or a DMA write): a full block goes to the card.
    pub fn fifo_write(&mut self, v: u32) {
        if self.xfer != Xfer::Write { return; }
        self.buf.extend_from_slice(&v.to_le_bytes());
        if self.buf.len() < self.blksz() { return; }
        let mut block = std::mem::take(&mut self.buf);
        block.resize(512, 0);
        let lba = self.lba;
        if let Some(d) = self.disk.as_mut() { if lba < d.sectors() { d.write(lba, &block); } }
        self.writes += 1;
        self.hsts |= HSTS_BLOCK_IRPT;
        if self.left == 0 {
            self.abort();
        } else {
            self.left -= 1;
            self.lba += 1;
        }
    }

    pub fn save_state(&self, w: &mut emu_core::snap::W) {
        w.tag("sdhost-v1");
        for v in [self.cmd, self.arg, self.tout, self.cdiv, self.rsp[0], self.rsp[1], self.rsp[2], self.rsp[3], self.hsts, self.vdd, self.edm, self.hcfg, self.hbct, self.hblc] { w.u32(v); }
        w.u8(self.state as u8); w.u16(self.rca); w.bool(self.app); w.opt_u32(self.pre_count);
        w.bool(self.special.is_some()); w.bytes(self.special.as_deref().unwrap_or(&[]));
        w.u8(self.xfer as u8); w.bytes(&self.buf); w.u64(self.pos as u64); w.u64(self.lba); w.u32(self.left);
        bcm2711::sdhci::save_card(&self.disk, w);
    }

    pub fn load_state(&mut self, r: &mut emu_core::snap::R) -> Result<(), emu_core::snap::SnapError> {
        r.tag("sdhost-v1")?;
        let mut v = [0u32; 14];
        for x in v.iter_mut() { *x = r.u32()?; }
        [self.cmd, self.arg, self.tout, self.cdiv, self.rsp[0], self.rsp[1], self.rsp[2], self.rsp[3], self.hsts, self.vdd, self.edm, self.hcfg, self.hbct, self.hblc] = v;
        self.state = match r.u8()? { 0 => State::Idle, 1 => State::Ready, 2 => State::Ident, 3 => State::Standby, _ => State::Transfer };
        self.rca = r.u16()?; self.app = r.bool()?; self.pre_count = r.opt_u32()?;
        let has = r.bool()?; let sp = r.bytes()?.to_vec(); self.special = has.then_some(sp);
        self.xfer = match r.u8()? { 1 => Xfer::Read, 2 => Xfer::Write, _ => Xfer::None };
        self.buf = r.bytes()?.to_vec(); self.pos = r.u64()? as usize; self.lba = r.u64()?; self.left = r.u32()?;
        bcm2711::sdhci::load_card(&mut self.disk, r)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use emu_core::disk::MemDisk;

    fn host() -> Sdhost {
        let mut img = vec![0u8; 4096 * 512];
        for (k, b) in img.iter_mut().enumerate() { *b = (k / 512) as u8 ^ (k as u8); }
        let mut h = Sdhost::new(Some(Box::new(MemDisk(img))));
        h.write(SDVDD, 1);
        h
    }

    fn send(h: &mut Sdhost, idx: u32, arg: u32, flags: u32) -> u32 {
        h.write(SDARG, arg);
        h.write(SDCMD, idx | flags | CMD_NEW);
        let c = h.read(SDCMD);
        assert_eq!(c & CMD_NEW, 0, "CMD{idx} done at once");
        c
    }

    fn init(h: &mut Sdhost) {
        send(h, 0, 0, CMD_NO_RESPONSE);
        assert_eq!(send(h, 5, 0, 0) & CMD_FAIL, CMD_FAIL, "no SDIO");
        assert_ne!(h.read(SDHSTS) & HSTS_CMD_TIME_OUT, 0);
        h.write(SDHSTS, 0x7f8);
        send(h, 8, 0x1aa, 0);
        assert_eq!(h.read(SDRSP0), 0x1aa);
        send(h, 55, 0, 0);
        send(h, 41, 0x40ff_8000, 0);
        assert_eq!(h.read(SDRSP0) >> 30, 3, "ready, SDHC");
        send(h, 2, 0, CMD_LONG_RESPONSE);
        assert_eq!(h.read(0x1c) >> 24, 0x5b, "CID's manufacturer in SDRSP3's top byte");
        send(h, 3, 0, 0);
        let rca = h.read(SDRSP0) >> 16;
        send(h, 7, rca << 16, CMD_BUSYWAIT);
        assert_ne!(h.read(SDHSTS) & HSTS_BUSY_IRPT, 0);
        h.write(SDHSTS, HSTS_BUSY_IRPT);
        assert_eq!(h.read(SDEDM) & 0xf, FSM_DATAMODE);
    }

    #[test]
    fn multi_block_read_through_the_fifo() {
        let mut h = host();
        init(&mut h);
        h.write(SDHCFG, HCFG_DATA_IRPT_EN | HCFG_BUSY_IRPT_EN);
        h.write(SDHBCT, 512);
        h.write(SDHBLC, 3);
        send(&mut h, 18, 10, CMD_READ);
        assert!(h.irq(), "data flag with the data interrupt on");
        let mut got = Vec::new();
        while h.read(SDEDM) >> 4 & 0x1f > 0 { got.extend_from_slice(&h.read(SDDATA).to_le_bytes()); }
        assert_eq!(got.len(), 3 * 512);
        assert_eq!(got[512], (11u8) ^ 0);
        assert!(!h.irq());
        assert_eq!(h.read(SDEDM) & 0xf, FSM_DATAMODE);
        send(&mut h, 12, 0, CMD_BUSYWAIT);
        assert!(h.irq(), "busy interrupt after the stop");
    }

    #[test]
    fn write_raises_a_block_interrupt_per_block() {
        let mut h = host();
        init(&mut h);
        h.write(SDHBCT, 512);
        h.write(SDHBLC, 2);
        h.write(SDHCFG, HCFG_DATA_IRPT_EN);
        send(&mut h, 25, 100, CMD_WRITE);
        assert!(h.irq());
        h.write(SDHCFG, HCFG_BLOCK_IRPT_EN);
        assert_eq!(16 - (h.read(SDEDM) >> 4 & 0x1f), 16, "room in the FIFO");
        for k in 0..128 { h.write(SDDATA, 0xa5a5_0000 | k); }
        assert!(h.irq(), "block interrupt");
        h.write(SDHSTS, HSTS_BLOCK_IRPT);
        for k in 0..128 { h.write(SDDATA, k); }
        assert_eq!(h.writes, 2);
        assert_eq!(h.read(SDEDM) & 0xf, FSM_DATAMODE);
        let mut b = [0u8; 512];
        h.disk.as_mut().unwrap().read(100, &mut b);
        assert_eq!(&b[..4], &0xa5a5_0000u32.to_le_bytes());
    }

    #[test]
    fn scr_read_and_csd_capacity() {
        let mut h = host();
        init(&mut h);
        send(&mut h, 9, 0, CMD_LONG_RESPONSE);
        let csd = (0..4).fold(0u128, |a, k| a | (h.read(SDRSP0 + 4 * k) as u128) << (32 * k));
        assert_eq!(((csd >> 48) & 0x3f_ffff) + 1, 4, "C_SIZE + 1 = 4 units of 512 KiB");
        h.write(SDHBCT, 8);
        h.write(SDHBLC, 1);
        send(&mut h, 55, 0x5bc0 << 16, 0);
        send(&mut h, 51, 0, CMD_READ);
        assert_eq!(h.read(SDEDM) >> 4 & 0x1f, 2);
        assert_eq!(h.read(SDDATA) & 0xff, 0x02);
        h.read(SDDATA);
        assert_eq!(h.read(SDHSTS) & HSTS_DATA_FLAG, 0);
    }
}
