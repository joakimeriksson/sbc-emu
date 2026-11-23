//! The JH7110's Cadence QSPI controller (`cdns,qspi-nor`) and the board's 16 MiB SPI NOR flash,
//! which holds the firmware the board boots from: U-Boot SPL at 0, U-Boot's environment at
//! 0xf0000 and the U-Boot + OpenSBI FIT at 1 MiB (the layout of the upstream device tree).
//!
//! The controller, as U-Boot's `cadence_qspi` and Linux's `spi-cadence-quadspi` use it:
//! - **STIG** (`CMDCTRL`): a command of up to 8 bytes in and out, with an optional address,
//!   for RDID, RDSR, WREN, erases and the like.
//! - **Indirect read**: `INDIRECTRDSTARTADDR` and `INDIRECTRDBYTES`, then `INDIRECTRD.START`; the
//!   data comes out of the SRAM FIFO through reads of the AHB window (the trigger address is 0),
//!   `SDRAMLEVEL` counts the words waiting, and `INDIRECTRD.DONE` ends the transfer.
//! - **Indirect write**: the same with `INDIRECTWR`, the data written to the AHB window.
//! - **Direct access**: outside an indirect transfer, reads of the AHB window read the flash.
//! - **Interrupt** 25: indirect completion and the FIFO watermark (Linux waits for them).
//!
//! The model is instant: transfers complete as soon as they start and the controller is always
//! idle. The flash is a GigaDevice GD25LQ128 (JEDEC ID c8 60 18) with its status registers and
//! write-enable latch; erases set bytes to 0xff and programs can only clear bits, as on NOR.
//! Writes stay in memory: the file given with `--spi-flash` is never changed.

/// Controller registers and the AHB window (the flash's direct-access and trigger region).
pub const QSPI_BASE: u64 = 0x1301_0000;
pub const QSPI_SIZE: u64 = 0x1_0000;
pub const QSPI_AHB: u64 = 0x2100_0000;
pub const QSPI_AHB_SIZE: u64 = 0x40_0000;
/// The controller's PLIC source.
pub const IRQ_QSPI: usize = 25;
/// The flash's size: 16 MiB.
pub const FLASH_SIZE: usize = 16 << 20;

const CONFIG: usize = 0x00;
const RD_INSTR: usize = 0x04;
const WR_INSTR: usize = 0x08;
const SRAMPARTITION: usize = 0x18;
const REMAP: usize = 0x24;
const SDRAMLEVEL: usize = 0x2c;
const IRQSTATUS: usize = 0x40;
const IRQMASK: usize = 0x44;
const INDIRECTRD: usize = 0x60;
const INDIRECTRDSTARTADDR: usize = 0x68;
const INDIRECTRDBYTES: usize = 0x6c;
const INDIRECTWR: usize = 0x70;
const INDIRECTWRSTARTADDR: usize = 0x78;
const INDIRECTWRBYTES: usize = 0x7c;
const CMDCTRL: usize = 0x90;
const CMDADDRESS: usize = 0x94;
const CMDREADDATALOWER: usize = 0xa0;
const CMDREADDATAUPPER: usize = 0xa4;
const CMDWRITEDATALOWER: usize = 0xa8;
const CMDWRITEDATAUPPER: usize = 0xac;
const MODULE_ID: usize = 0xfc;

const CONFIG_IDLE: u32 = 1 << 31;
const IND_START: u32 = 1 << 0;
const IND_CANCEL: u32 = 1 << 1;
const IND_INPROGRESS: u32 = 1 << 2;
const IND_DONE: u32 = 1 << 5;
const IRQ_IND_COMP: u32 = 1 << 2;
const IRQ_WATERMARK: u32 = 1 << 6;
/// SRAM words a FIFO partition holds by default (U-Boot writes `fifo-depth / 2` = 128).
const DEFAULT_PARTITION: u32 = 128;
/// The controller's SRAM: 256 words (`cdns,fifo-depth`).
const SRAM_WORDS: u32 = 256;

/// The SPI NOR flash: a GigaDevice GD25LQ128 (16 MiB, 4 KiB sectors, 256-byte pages).
pub struct SpiNor {
    pub data: Vec<u8>,
    /// status register 1 (WIP bit 0 always clear, WEL bit 1, block protection) and 2 (QE)
    sr1: u8,
    sr2: u8,
    /// bytes programmed and sectors erased, for reports
    pub programmed: u64,
    pub erased: u64,
}

impl SpiNor {
    pub const JEDEC_ID: [u8; 3] = [0xc8, 0x60, 0x18];

    /// A flash holding `image` (padded with erased bytes to 16 MiB).
    pub fn new(image: &[u8]) -> Result<Self, String> {
        if image.len() > FLASH_SIZE {
            return Err(format!("the SPI flash image is {} bytes; the board's flash holds 16 MiB", image.len()));
        }
        let mut data = vec![0xff; FLASH_SIZE];
        data[..image.len()].copy_from_slice(image);
        Ok(SpiNor { data, sr1: 0, sr2: 0, programmed: 0, erased: 0 })
    }

    fn wel(&self) -> bool {
        self.sr1 & 2 != 0
    }

    /// `n` bytes from `addr` (the address wraps at the end of the flash).
    fn read(&self, addr: u32, n: usize) -> Vec<u8> {
        (0..n).map(|k| self.data[(addr as usize + k) % FLASH_SIZE]).collect()
    }

    /// Page program: within the 256-byte page of `addr` (the address wraps in the page), bits
    /// can only go from 1 to 0.
    fn program(&mut self, addr: u32, bytes: &[u8]) {
        if !self.wel() {
            return;
        }
        let page = addr as usize & !0xff & (FLASH_SIZE - 1);
        for (k, b) in bytes.iter().enumerate() {
            let o = page + ((addr as usize + k) & 0xff);
            self.data[o] &= b;
        }
        self.programmed += bytes.len() as u64;
        self.sr1 &= !2;
    }

    fn erase(&mut self, addr: u32, size: usize) {
        if !self.wel() {
            return;
        }
        let start = addr as usize & !(size - 1) & (FLASH_SIZE - 1);
        self.data[start..start + size].fill(0xff);
        self.erased += 1;
        self.sr1 &= !2;
    }

    /// One command (opcode, optional address, the bytes sent after them); returns the `read`
    /// bytes the flash sends back.
    fn command(&mut self, op: u8, addr: Option<u32>, tx: &[u8], read: usize) -> Vec<u8> {
        let a = addr.unwrap_or(0);
        let repeat = |b: &[u8]| -> Vec<u8> { (0..read).map(|k| b[k % b.len()]).collect() };
        match op {
            0x9f => repeat(&Self::JEDEC_ID),
            0x05 => repeat(&[self.sr1]),
            0x35 => repeat(&[self.sr2]),
            0x15 => repeat(&[0]),
            0x06 => {
                self.sr1 |= 2;
                Vec::new()
            }
            0x04 => {
                self.sr1 &= !2;
                Vec::new()
            }
            0x01 => {
                if self.wel() {
                    if let Some(&s) = tx.first() {
                        self.sr1 = s & 0xfc;
                    }
                    if let Some(&s) = tx.get(1) {
                        self.sr2 = s;
                    }
                }
                self.sr1 &= !2;
                Vec::new()
            }
            0x31 => {
                if self.wel() {
                    if let Some(&s) = tx.first() {
                        self.sr2 = s;
                    }
                }
                self.sr1 &= !2;
                Vec::new()
            }
            0x20 => {
                self.erase(a, 4 << 10);
                Vec::new()
            }
            0x52 => {
                self.erase(a, 32 << 10);
                Vec::new()
            }
            0xd8 => {
                self.erase(a, 64 << 10);
                Vec::new()
            }
            0xc7 | 0x60 => {
                if self.wel() {
                    self.data.fill(0xff);
                    self.erased += 1;
                }
                self.sr1 &= !2;
                Vec::new()
            }
            0x02 | 0x32 => {
                self.program(a, tx);
                Vec::new()
            }
            // the read commands: single, fast, dual and quad output and I/O
            0x03 | 0x0b | 0x3b | 0x6b | 0xbb | 0xeb => self.read(a, read),
            // reset, power down and release, SFDP (not provided: erased bytes), others
            _ => vec![0xff; read],
        }
    }
}

pub struct Qspi {
    regs: [u32; 0x40],
    pub flash: Option<SpiNor>,
    /// an indirect read in progress: the bytes not yet read from the SRAM FIFO
    rd: Option<std::collections::VecDeque<u8>>,
    rd_done: bool,
    /// an indirect write in progress: the flash address, the length and the data so far
    wr: Option<(u32, usize, Vec<u8>)>,
    wr_done: bool,
    irqstatus: u32,
}

impl Default for Qspi {
    fn default() -> Self {
        let mut regs = [0u32; 0x40];
        regs[MODULE_ID / 4] = 0x0001_0201;
        regs[SRAMPARTITION / 4] = DEFAULT_PARTITION;
        Qspi { regs, flash: None, rd: None, rd_done: false, wr: None, wr_done: false, irqstatus: 0 }
    }
}

impl Qspi {
    pub fn irq(&self) -> bool {
        self.irqstatus & self.regs[IRQMASK / 4] != 0
    }

    /// Words the read FIFO holds: what is left, at most one SRAM partition.
    fn read_level(&self) -> u32 {
        let words = self.rd.as_ref().map_or(0, |q| q.len().div_ceil(4)) as u32;
        let part = match self.regs[SRAMPARTITION / 4] {
            0 => DEFAULT_PARTITION,
            p => p,
        };
        words.min(part).min(0xffff)
    }

    pub fn read(&mut self, offset: u64) -> u32 {
        let o = offset as usize & 0xfffc;
        match o {
            CONFIG => self.regs[0] | CONFIG_IDLE,
            SDRAMLEVEL => self.read_level(),
            IRQSTATUS => self.irqstatus,
            INDIRECTRD => {
                (self.regs[o / 4] & !(IND_START | IND_INPROGRESS | IND_DONE))
                    | if self.rd.is_some() { IND_INPROGRESS } else { 0 }
                    | if self.rd_done { IND_DONE } else { 0 }
            }
            INDIRECTWR => {
                (self.regs[o / 4] & !(IND_START | IND_INPROGRESS | IND_DONE))
                    | if self.wr.is_some() { IND_INPROGRESS } else { 0 }
                    | if self.wr_done { IND_DONE } else { 0 }
            }
            CMDCTRL => self.regs[o / 4] & !3,
            _ if o < 0x100 => self.regs[o / 4],
            _ => 0,
        }
    }

    pub fn write(&mut self, offset: u64, v: u32) {
        let o = offset as usize & 0xfffc;
        match o {
            IRQSTATUS => self.irqstatus &= !v,
            CMDCTRL => {
                self.regs[o / 4] = v & !1;
                if v & 1 != 0 {
                    self.stig(v);
                }
            }
            INDIRECTRD => {
                if v & IND_DONE != 0 {
                    self.rd_done = false;
                }
                if v & IND_CANCEL != 0 {
                    self.rd = None;
                }
                if v & IND_START != 0 {
                    let (addr, len) = (self.regs[INDIRECTRDSTARTADDR / 4], self.regs[INDIRECTRDBYTES / 4] as usize);
                    let op = self.regs[RD_INSTR / 4] as u8;
                    let data = self.flash.as_mut().map_or_else(|| vec![0xff; len], |f| f.command(op, Some(addr), &[], len));
                    self.rd_done = data.is_empty();
                    self.rd = (!data.is_empty()).then(|| data.into());
                    self.irqstatus |= IRQ_IND_COMP | IRQ_WATERMARK;
                }
                self.regs[o / 4] = v & !(IND_START | IND_CANCEL | IND_DONE);
            }
            INDIRECTWR => {
                if v & IND_DONE != 0 {
                    self.wr_done = false;
                }
                if v & IND_CANCEL != 0 {
                    self.wr = None;
                }
                if v & IND_START != 0 {
                    let (addr, len) = (self.regs[INDIRECTWRSTARTADDR / 4], self.regs[INDIRECTWRBYTES / 4] as usize);
                    self.wr = Some((addr, len, Vec::with_capacity(len)));
                    self.wr_done = false;
                    if len == 0 {
                        self.finish_write();
                    }
                }
                self.regs[o / 4] = v & !(IND_START | IND_CANCEL | IND_DONE);
            }
            // the read partition's size in words, at most the SRAM's 256 (Linux finds the FIFO
            // depth by writing all ones and reading back depth - 1)
            SRAMPARTITION => self.regs[o / 4] = v & (SRAM_WORDS - 1),
            _ if o < 0x100 => self.regs[o / 4] = v,
            _ => {}
        }
    }

    /// A STIG command from CMDCTRL: opcode, address, up to 8 bytes out and in.
    fn stig(&mut self, ctrl: u32) {
        let op = (ctrl >> 24) as u8;
        let rd = if ctrl & 1 << 23 != 0 { ((ctrl >> 20) & 7) as usize + 1 } else { 0 };
        let addr = (ctrl & 1 << 19 != 0).then(|| {
            let bytes = ((ctrl >> 16) & 3) + 1;
            let a = self.regs[CMDADDRESS / 4];
            if bytes >= 4 {
                a
            } else {
                a & ((1u32 << (8 * bytes)) - 1)
            }
        });
        let wr = if ctrl & 1 << 15 != 0 { ((ctrl >> 12) & 7) as usize + 1 } else { 0 };
        let out = (self.regs[CMDWRITEDATALOWER / 4] as u64 | (self.regs[CMDWRITEDATAUPPER / 4] as u64) << 32).to_le_bytes();
        let back = match self.flash.as_mut() {
            Some(f) => f.command(op, addr, &out[..wr], rd),
            // no flash: the data lines float high
            None => vec![0xff; rd],
        };
        let mut word = [0u8; 8];
        word[..back.len()].copy_from_slice(&back);
        self.regs[CMDREADDATALOWER / 4] = u32::from_le_bytes(word[..4].try_into().unwrap());
        self.regs[CMDREADDATAUPPER / 4] = u32::from_le_bytes(word[4..].try_into().unwrap());
    }

    fn finish_write(&mut self) {
        if let Some((addr, _, data)) = self.wr.take() {
            let op = self.regs[WR_INSTR / 4] as u8;
            if let Some(f) = self.flash.as_mut() {
                // a write instruction register left at 0 means page program
                f.command(if op == 0 { 0x02 } else { op }, Some(addr), &data, 0);
            }
        }
        self.wr_done = true;
        self.irqstatus |= IRQ_IND_COMP;
    }

    /// A read of `size` bytes at offset `off` of the AHB window: the indirect read FIFO while a
    /// transfer runs, otherwise the flash itself (direct access, with the remap offset).
    pub fn ahb_read(&mut self, off: u64, size: u8) -> u64 {
        let mut b = [0u8; 8];
        let n = size.min(8) as usize;
        if let Some(q) = self.rd.as_mut() {
            for byte in b.iter_mut().take(n) {
                *byte = q.pop_front().unwrap_or(0);
            }
            if q.is_empty() {
                self.rd = None;
                self.rd_done = true;
                self.irqstatus |= IRQ_IND_COMP;
            }
        } else if let Some(f) = self.flash.as_ref() {
            let a = (off as u32).wrapping_add(self.regs[REMAP / 4]);
            b[..n].copy_from_slice(&f.read(a, n));
        } else {
            b = [0xff; 8];
        }
        u64::from_le_bytes(b)
    }

    /// A write to the AHB window: data for an indirect write (ignored otherwise).
    pub fn ahb_write(&mut self, size: u8, v: u64) {
        let Some((_, len, data)) = self.wr.as_mut() else { return };
        let n = (size as usize).min(*len - data.len());
        let before = data.len();
        data.extend_from_slice(&v.to_le_bytes()[..n]);
        // Linux waits for an interrupt after each page it hands over
        if data.len() / 256 != before / 256 {
            self.irqstatus |= IRQ_WATERMARK;
        }
        if data.len() >= *len {
            self.finish_write();
        }
    }
}

// Snapshots: what the flash holds now (writes stay in memory), whatever `--spi-flash` held.
impl emu_core::snap::Snap for SpiNor {
    fn save(&self, w: &mut emu_core::snap::W) {
        let Self { data, sr1, sr2, programmed, erased } = self;
        w.bytes(data);
        for v in [sr1, sr2] { v.save(w); }
        for v in [programmed, erased] { v.save(w); }
    }
    fn load(&mut self, r: &mut emu_core::snap::R) -> Result<(), emu_core::snap::SnapError> {
        let Self { data, sr1, sr2, programmed, erased } = self;
        let b = r.bytes()?;
        if b.len() != data.len() {
            return Err(emu_core::snap::SnapError(format!("the snapshot's SPI flash has {} bytes, this one {}", b.len(), data.len())));
        }
        data.copy_from_slice(b);
        for v in [sr1, sr2] { v.load(r)?; }
        for v in [programmed, erased] { v.load(r)?; }
        Ok(())
    }
}

emu_core::snap_struct!(Qspi { regs, rd, rd_done, wr, wr_done, irqstatus } fitted { flash: "an SPI flash (--spi-flash)" }
after |q| {
    // an indirect write ends when its data is complete
    if q.wr.as_ref().is_some_and(|(_, len, data)| data.len() > *len) {
        return Err(emu_core::snap::SnapError("a QSPI indirect write with more data than its length".into()));
    }
    Ok(())
});

#[cfg(test)]
mod tests {
    use super::*;

    fn stig(q: &mut Qspi, op: u8, rd: u32, addr: Option<u32>, wr: &[u8]) -> u64 {
        let mut ctrl = (op as u32) << 24;
        if rd > 0 {
            ctrl |= 1 << 23 | (rd - 1) << 20;
        }
        if let Some(a) = addr {
            q.write(CMDADDRESS as u64, a);
            ctrl |= 1 << 19 | 2 << 16;
        }
        if !wr.is_empty() {
            let mut w = [0u8; 8];
            w[..wr.len()].copy_from_slice(wr);
            q.write(CMDWRITEDATALOWER as u64, u32::from_le_bytes(w[..4].try_into().unwrap()));
            q.write(CMDWRITEDATAUPPER as u64, u32::from_le_bytes(w[4..].try_into().unwrap()));
            ctrl |= 1 << 15 | (wr.len() as u32 - 1) << 12;
        }
        q.write(CMDCTRL as u64, ctrl | 1);
        q.read(CMDREADDATALOWER as u64) as u64 | (q.read(CMDREADDATAUPPER as u64) as u64) << 32
    }

    fn indirect_read(q: &mut Qspi, addr: u32, len: u32) -> Vec<u8> {
        q.write(RD_INSTR as u64, 0x03);
        q.write(INDIRECTRDSTARTADDR as u64, addr);
        q.write(INDIRECTRDBYTES as u64, len);
        q.write(INDIRECTRD as u64, IND_START);
        let mut out = Vec::new();
        while out.len() < len as usize {
            let level = q.read(SDRAMLEVEL as u64);
            assert!(level > 0);
            for _ in 0..level {
                out.extend_from_slice(&(q.ahb_read(0, 4) as u32).to_le_bytes());
            }
        }
        out.truncate(len as usize);
        assert_ne!(q.read(INDIRECTRD as u64) & IND_DONE, 0);
        q.write(INDIRECTRD as u64, IND_DONE);
        assert_eq!(q.read(INDIRECTRD as u64) & IND_DONE, 0);
        out
    }

    #[test]
    fn jedec_id_indirect_read_erase_and_program() {
        let image: Vec<u8> = (0..0x3000u32).map(|i| (i * 7) as u8).collect();
        let mut q = Qspi { flash: Some(SpiNor::new(&image).unwrap()), ..Default::default() };
        assert_eq!(stig(&mut q, 0x9f, 3, None, &[]) & 0xff_ffff, 0x18_60c8);
        assert_eq!(indirect_read(&mut q, 0x1000, 0x900), image[0x1000..0x1900]);
        // erase needs the write-enable latch
        stig(&mut q, 0x20, 0, Some(0x1000), &[]);
        assert_eq!(indirect_read(&mut q, 0x1000, 4), image[0x1000..0x1004]);
        stig(&mut q, 0x06, 0, None, &[]);
        assert_eq!(stig(&mut q, 0x05, 1, None, &[]) & 2, 2);
        stig(&mut q, 0x20, 0, Some(0x1234), &[]);
        assert!(indirect_read(&mut q, 0x1000, 0x1000).iter().all(|&b| b == 0xff));
        assert_eq!(stig(&mut q, 0x05, 1, None, &[]) & 2, 0);
        // an indirect page program of 8 bytes through the AHB window
        stig(&mut q, 0x06, 0, None, &[]);
        q.write(WR_INSTR as u64, 0x02);
        q.write(INDIRECTWRSTARTADDR as u64, 0x1100);
        q.write(INDIRECTWRBYTES as u64, 8);
        q.write(IRQSTATUS as u64, u32::MAX);
        q.write(IRQMASK as u64, IRQ_IND_COMP);
        q.write(INDIRECTWR as u64, IND_START);
        q.ahb_write(4, 0x4433_2211);
        assert!(!q.irq());
        q.ahb_write(4, 0x8877_6655);
        assert!(q.irq());
        assert_ne!(q.read(INDIRECTWR as u64) & IND_DONE, 0);
        assert_eq!(indirect_read(&mut q, 0x1100, 8), [0x11, 0x22, 0x33, 0x44, 0x55, 0x66, 0x77, 0x88]);
        // direct access reads the flash
        assert_eq!(q.ahb_read(0x2000, 4) as u32, u32::from_le_bytes(image[0x2000..0x2004].try_into().unwrap()));
    }
}

#[cfg(test)]
mod snapshot_tests {
    /// An indirect write holding more data than its length is refused on restore (the next AHB
    /// write would compute a negative remainder).
    #[test]
    fn a_snapshot_with_an_overfull_indirect_write_is_refused() {
        let mut m = crate::Machine::new(8 << 20);
        m.soc.qspi.wr = Some((0x1000, 256, vec![0; 256]));
        crate::Machine::new(8 << 20).restore(&m.save().unwrap()).unwrap();
        assert!(crate::machine::refused(|m| m.soc.qspi.wr = Some((0x1000, 4, vec![0; 8]))).contains("QSPI"));
    }
}
