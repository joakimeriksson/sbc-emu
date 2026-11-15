//! EMMC2: the BCM2711's SD host controller (SDHCI v3 register set, driven by Linux's
//! `sdhci-iproc` as "brcm,bcm2711-emmc2") with an SDHC card behind it.
//!
//! Data moves by ADMA2 (32-bit descriptors) for the memory card: a data command with DMA
//! enabled walks the descriptor table and completes at once. PIO through the buffer data
//! port works too, and is what the SDIO host (Wi-Fi) uses: ADMA2 is advertised only for a
//! memory card. Accesses may be 8, 16 or 32 bits wide; each write is applied with byte
//! enables, then its side effects run (command issue on the command register's bytes, W1C on
//! interrupt status, software reset, clock-stable handshake).
//!
//! Card model: SD physical layer 3.0 SDHC/SDXC, block addressed, 3.3 V only (no UHS voltage
//! switch), 4-bit bus, high speed via CMD6. Supported commands: 0, 2, 3, 6, 7, 8, 9, 10, 12, 13,
//! 16, 17, 18, 23, 24, 25, 32, 33, 38, 55 and ACMD 6, 13, 23, 41, 42, 51. Anything else times out, which is
//! how a real SD card answers SDIO/MMC probes (CMD5, CMD1, CMD52).

pub use emu_core::disk::Disk;

/// An SDIO card (functions 0..7) behind the host instead of a memory card.
pub trait SdioCard {
    /// number of I/O functions besides function 0
    fn num_functions(&self) -> u8;
    /// CMD52: one register; returns the byte read (or the byte now in the register after a write)
    fn rw_direct(&mut self, write: bool, func: u8, addr: u32, data: u8, raw: bool) -> u8;
    /// CMD53 read/write of `out.len()` bytes (incrementing or fixed address)
    fn read_ext(&mut self, func: u8, addr: u32, incr: bool, out: &mut [u8]);
    fn write_ext(&mut self, func: u8, addr: u32, incr: bool, data: &[u8]);
    /// the card's interrupt line (DAT[1])
    fn irq(&self) -> bool;
    /// time passes (ns): firmware timers, delayed events
    fn tick(&mut self, _now_ns: u64) {}
    /// network frames the card transmitted (Ethernet II)
    fn take_tx(&mut self) -> Vec<Vec<u8>> { Vec::new() }
    /// a network frame for the card
    fn deliver(&mut self, _eth: &[u8]) {}
    fn set_trace(&mut self, _on: bool) {}
    fn save_state(&self, _w: &mut emu_core::snap::W) {}
    fn load_state(&mut self, _r: &mut emu_core::snap::R) -> Result<(), emu_core::snap::SnapError> { Ok(()) }
}

/// A RAM disk (tests, small images): the shared one.
pub use emu_core::disk::MemDisk;

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
    /// an SDIO card instead of a memory card
    pub sdio: Option<Box<dyn SdioCard>>,
    /// pending CMD53 write: (function, address, incrementing)
    sdio_write: Option<(u8, u32, bool)>,
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
const ADMA_ADDR: usize = 0x58 / 4;
const VERSION: usize = 0xfc / 4;
/// capabilities: ADMA2 supported
const CAP_ADMA2: u32 = 1 << 19;
const ERR_ADMA: u32 = 1 << 25;

impl Default for Sdhci {
    fn default() -> Self { Self::new(None) }
}

impl Sdhci {
    pub fn new(disk: Option<Box<dyn Disk>>) -> Self {
        let mut s = Sdhci { regs: [0; 64], disk, state: State::Idle, rca: 0, app: false, xfer: Xfer::None, buf: Vec::new(), pos: 0,
                            lba: 0, left: None, special: None, pre_count: None, reads: 0, writes: 0, trace: false, sdio: None, sdio_write: None };
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
        if self.disk.is_some() || self.sdio.is_some() { self.regs[INTSTAT] |= INT_CARD_IN; }
    }

    fn present(&self) -> u32 {
        let card = if self.disk.is_some() || self.sdio.is_some() { PS_CARD } else { (1 << 18) | (1 << 19) | (0xf << 20) | (1 << 24) };
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
        // the SDIO card interrupt (bit 8) is a level, visible while enabled in the status-enable register
        let card_int = if self.sdio.as_ref().is_some_and(|c| c.irq()) && self.regs[INTEN] & (1 << 8) != 0 { 1 << 8 } else { 0 };
        let st = self.regs[INTSTAT] | card_int;
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
            CAPS => self.regs[CAPS] | if self.sdio.is_none() { CAP_ADMA2 } else { 0 },
            _ => self.regs[r],
        }
    }

    /// `size` bytes written at `off` (any alignment within a word).
    pub fn write(&mut self, off: u64, v: u32, size: u32, ram: &mut [u8]) {
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
            CMD if mask & 0xffff_0000 != 0 => { self.command(); self.run_dma(ram); }
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
        if self.sdio.is_some() { return self.sdio_command(idx, arg, data, xfer_mode); }
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
            // erase: CMD32/CMD33 set the first and last block, CMD38 (R1b) erases. Linux uses it
            // for discard only (the queue's write_zeroes_max_bytes is 0), after which the data is
            // undefined: the blocks keep their contents. Sent with a busy response (R1b), the end
            // of busy is the transfer-complete interrupt, at once; as plain R1, Linux polls CMD13.
            (false, 32) | (false, 33) => r1(self),
            (false, 38) => { if resp_ty == 3 { self.raise(INT_XFER_DONE); } r1(self) }
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

    /// Commands to an SDIO card (SD Specifications Part E1: CMD5, CMD52, CMD53).
    fn sdio_command(&mut self, idx: u32, arg: u32, data: bool, mode: u32) {
        let selected = self.state == State::Transfer;
        let r5 = |sel: bool, d: u8| -> u32 { ((if sel { 0x20 } else { 0x10 }) << 8) | d as u32 };
        let resp = match idx {
            0 => { self.state = State::Idle; return self.raise(INT_CMD_DONE); }
            5 => {
                let n = self.sdio.as_ref().map(|c| c.num_functions()).unwrap_or(0) as u32;
                if arg & 0x00ff_ffff != 0 { self.state = State::Ready; }
                // R4: ready, I/O functions, no memory, 2.7-3.6 V
                (1 << 31) | ((n & 7) << 28) | 0x00ff_8000
            }
            3 => { self.state = State::Standby; self.rca = 0x0001; (self.rca as u32) << 16 }
            7 => { self.state = if (arg >> 16) as u16 == self.rca { State::Transfer } else { State::Standby }; 0x0000_0700 }
            52 => {
                let write = arg >> 31 != 0;
                let func = ((arg >> 28) & 7) as u8;
                let raw = arg & (1 << 27) != 0;
                let addr = (arg >> 9) & 0x1_ffff;
                let card = self.sdio.as_mut().unwrap();
                let d = card.rw_direct(write, func, addr, arg as u8, raw);
                if self.trace { eprintln!("[sdio] CMD52 {} f{func} {addr:#07x} {} {d:#04x}", if write { "W" } else { "R" }, if write { format!("{:#04x} ->", arg as u8) } else { "->".into() }); }
                r5(selected, d)
            }
            53 => {
                let write = arg >> 31 != 0;
                let func = ((arg >> 28) & 7) as u8;
                let incr = arg & (1 << 26) != 0;
                let addr = (arg >> 9) & 0x1_ffff;
                let blksz = (self.regs[BLKSIZE] & 0xfff) as usize;
                let blkcnt = (self.regs[BLKSIZE] >> 16) as usize;
                let len = if mode & (1 << 5) != 0 { blksz * blkcnt.max(1) } else { blksz.max(1) };
                if self.trace { eprintln!("[sdio] CMD53 {} f{func} {addr:#07x} len {len} {}", if write { "W" } else { "R" }, if incr { "incr" } else { "fixed" }); }
                if data {
                    self.buf = vec![0; len];
                    self.pos = 0;
                    self.left = Some(0);
                    if write {
                        self.sdio_write = Some((func, addr, incr));
                        self.xfer = Xfer::Write;
                        self.raise(INT_BUF_WR);
                    } else {
                        self.sdio.as_mut().unwrap().read_ext(func, addr, incr, &mut self.buf);
                        self.xfer = Xfer::Read;
                        self.raise(INT_BUF_RD);
                    }
                }
                r5(selected, 0)
            }
            _ => return self.timeout(),
        };
        self.regs[RESP0] = resp;
        self.raise(INT_CMD_DONE);
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

    /// A data transfer just started with DMA enabled (transfer mode bit 0, ADMA2 selected in
    /// host control): move all of it through the descriptor table now.
    fn run_dma(&mut self, ram: &mut [u8]) {
        let mode = self.regs[CMD] & 0xffff;
        let adma2 = (self.regs[CTRL] >> 3) & 3 == 2;
        if self.sdio.is_some() || mode & 1 == 0 || !adma2 || self.xfer == Xfer::None { return; }
        let blksz = ((self.regs[BLKSIZE] & 0xfff) as usize).max(1);
        // descriptors: (host address, length) of each transfer chunk, in order
        let mut chunks: Vec<(usize, usize)> = Vec::new();
        let mut d = self.regs[ADMA_ADDR] as usize;
        let mut ok = true;
        for _ in 0..65536 {
            let Some(b) = ram.get(d..d + 8) else { ok = false; break };
            let attr = u16::from_le_bytes([b[0], b[1]]);
            let len = match u16::from_le_bytes([b[2], b[3]]) { 0 => 65536, l => l as usize };
            let addr = u32::from_le_bytes([b[4], b[5], b[6], b[7]]) as usize;
            if attr & 1 == 0 { ok = false; break; } // not valid
            match attr & 0x30 {
                0x20 => { chunks.push((addr, len)); d += 8; }
                0x30 => { d = addr; continue; }
                _ => d += 8,
            }
            if attr & 2 != 0 { break; } // end
        }
        let total: usize = chunks.iter().map(|c| c.1).sum();
        if !ok || chunks.iter().any(|&(a, l)| a + l > ram.len()) {
            self.xfer = Xfer::None;
            self.raise(ERR_ADMA);
            return;
        }
        match self.xfer {
            Xfer::Read => {
                // the data: this buffer (a special read or the first block), then more blocks
                let mut data = std::mem::take(&mut self.buf);
                data.truncate(blksz.min(data.len()));
                while data.len() < total {
                    match self.left {
                        Some(0) => break,
                        left => {
                            if let Some(l) = left { self.left = Some(l - 1); }
                            self.lba += 1;
                            self.buf = vec![0; 512];
                            self.load_block();
                            data.extend_from_slice(&self.buf);
                        }
                    }
                }
                let mut o = 0;
                for (a, l) in chunks {
                    let n = l.min(data.len().saturating_sub(o));
                    ram[a..a + n].copy_from_slice(&data[o..o + n]);
                    o += n;
                }
            }
            Xfer::Write => {
                let mut data = Vec::with_capacity(total);
                for (a, l) in chunks { data.extend_from_slice(&ram[a..a + l]); }
                for blk in data.chunks(512) {
                    let mut b = blk.to_vec();
                    b.resize(512, 0);
                    let lba = self.lba;
                    if let Some(d) = self.disk.as_mut() { if lba < d.sectors() { d.write(lba, &b); } }
                    self.writes += 1;
                    self.lba += 1;
                    if let Some(l) = self.left { if l == 0 { break; } self.left = Some(l - 1); }
                }
            }
            Xfer::None => {}
        }
        self.xfer = Xfer::None;
        self.pos = 0;
        self.buf.clear();
        self.regs[INTSTAT] &= !(INT_BUF_RD | INT_BUF_WR);
        self.raise(INT_XFER_DONE);
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
            if let Some((func, addr, incr)) = self.sdio_write.take() {
                if let Some(c) = self.sdio.as_mut() { c.write_ext(func, addr, incr, &self.buf); }
                self.regs[INTSTAT] &= !INT_BUF_WR;
                self.xfer = Xfer::None;
                self.raise(INT_XFER_DONE);
                return;
            }
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
        if let Some(c) = self.sdio.as_ref() { c.save_state(w); }
        if self.disk.as_ref().is_some_and(|d| !d.snapshots_disk()) {
            eprintln!("[sd] snapshot does not include the SD card: --sd-persist writes go to the image");
        }
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
        if let Some(c) = self.sdio.as_mut() { c.load_state(r)?; }
        let n = r.u64()?;
        // `n` comes from the snapshot: no pre-allocation from an untrusted count.
        let mut sectors = Vec::new();
        for _ in 0..n {
            let lba = r.u64()?;
            sectors.push((lba, r.bytes()?.to_vec()));
        }
        // Exactly the snapshot's sectors: writes made after it was taken are dropped.
        if let Some(d) = self.disk.as_mut() {
            if !d.snapshots_disk() {
                eprintln!("[sd] restored snapshot without the SD card's state: --sd-persist writes go to the image");
            }
            d.restore_overlay(sectors);
        }
        Ok(())
    }
}
