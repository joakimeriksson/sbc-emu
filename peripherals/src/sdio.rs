//! An SDIO card (SDIO 3.0, Physical Layer Simplified Specification part E1) as a host
//! controller sees it: CMD0/5/3/7/52/53, the card common control registers (CCCR), the
//! function basic registers (FBR) and the card information structure (CIS) in function 0's
//! address space. The I/O functions themselves (1 to 7) are a device behind the
//! `SdioFunctions` trait, so any SDIO device (Wi-Fi, Bluetooth, ...) gets the bus protocol
//! from here and any board's SD host controller can carry it.
//!
//! The host controller passes every command to `command` and moves CMD53's data through
//! `read_data` / `write_data`; responses are the 32-bit R4/R5/R6/R1 words.

/// The I/O functions of an SDIO device.
pub trait SdioFunctions {
    /// Number of I/O functions (1 to 7).
    fn functions(&self) -> u8;
    /// SDIO vendor and device ID (the MANFID tuple).
    fn manfid(&self) -> (u16, u16);
    /// Largest block size of function `func` (its CIS FUNCE tuple).
    fn max_block_size(&self, func: u8) -> u16 {
        let _ = func;
        512
    }
    /// A byte register of function `func` (1-7) at `addr` (17 bits).
    fn read(&mut self, func: u8, addr: u32) -> u8;
    fn write(&mut self, func: u8, addr: u32, value: u8);
    /// CMD53 data: `incr` advances the address per byte (OP code 1), otherwise every byte
    /// goes to the same address (a FIFO).
    fn read_block(&mut self, func: u8, addr: u32, incr: bool, out: &mut [u8]) {
        for (i, b) in out.iter_mut().enumerate() {
            *b = self.read(func, if incr { addr + i as u32 } else { addr });
        }
    }
    fn write_block(&mut self, func: u8, addr: u32, incr: bool, data: &[u8]) {
        for (i, &b) in data.iter().enumerate() {
            self.write(func, if incr { addr + i as u32 } else { addr }, b);
        }
    }
    /// The host enabled (IOEx) the functions in `mask` (bit n = function n).
    fn enable(&mut self, mask: u8) {
        let _ = mask;
    }
    /// Functions ready for I/O (IORx), among the enabled ones.
    fn ready(&self) -> u8 {
        0xfe
    }
    /// Functions with an interrupt pending (bit n = function n).
    fn pending(&self) -> u8 {
        0
    }
    /// Vendor-specific CCCR registers 0xf0-0xff.
    fn vendor_read(&mut self, addr: u8) -> u8 {
        let _ = addr;
        0
    }
    fn vendor_write(&mut self, addr: u8, value: u8) {
        let _ = (addr, value);
    }
    /// CCCR RES (I/O reset): every function back to its reset state.
    fn reset(&mut self) {}
    /// The device itself, for the board's wiring (out-of-band interrupts, the network).
    fn as_any_mut(&mut self) -> Option<&mut dyn std::any::Any> {
        None
    }
}

// CCCR registers (function 0)
const CCCR_REV: u32 = 0x00;
const SD_REV: u32 = 0x01;
const IOE: u32 = 0x02;
const IOR: u32 = 0x03;
const IEN: u32 = 0x04;
const INT: u32 = 0x05;
const ABORT: u32 = 0x06;
const BUS_IF: u32 = 0x07;
const CAPS: u32 = 0x08;
const CIS_PTR: u32 = 0x09;
const FN0_BLKSIZE: u32 = 0x10;
const SPEED: u32 = 0x13;
/// SDIO 3.00, CCCR 3.00.
const CCCR_VERSION: u8 = 0x43;
/// SMB: multi-block CMD53.
const CAPS_VALUE: u8 = 0x02;
/// SHS: high speed supported (EHS, bit 1, is the host's).
const SPEED_SHS: u8 = 0x01;
/// The CIS of function n is at CIS_BASE + 0x100 * n.
const CIS_BASE: u32 = 0x1000;

/// R5 flags: CMD state (no data transfer) and TRN state.
const R5_CMD: u32 = 0x1000;
const R5_TRN: u32 = 0x2000;
/// R5 FUNCTION_NUMBER: a function the card does not have.
const R5_FUNCTION_NUMBER: u32 = 1 << 9;

/// What a command needs from the host controller's data phase.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Transfer {
    None,
    /// CMD53 read: the host reads `read_data`.
    Read,
    /// CMD53 write: the host hands the bytes to `write_data`.
    Write,
}

pub struct SdioCard {
    pub dev: Box<dyn SdioFunctions>,
    rca: u16,
    selected: bool,
    ioe: u8,
    ien: u8,
    bus_if: u8,
    speed: u8,
    fn0_blksize: u16,
    blksize: [u16; 8],
    cis: Vec<u8>,
    /// The CMD53 argument of the transfer in progress.
    ext: u32,
    pub trace: bool,
}

impl SdioCard {
    pub fn new(dev: Box<dyn SdioFunctions>) -> Self {
        let cis = build_cis(dev.as_ref());
        let trace = std::env::var_os("SBC_SDIO_TRACE").is_some();
        Self { dev, rca: 0, selected: false, ioe: 0, ien: 0, bus_if: 0, speed: 0, fn0_blksize: 0, blksize: [0; 8], cis, ext: 0, trace }
    }

    fn functions(&self) -> u8 {
        self.dev.functions().clamp(1, 7)
    }

    /// A command; the response word (None: the card does not answer) and the data phase.
    pub fn command(&mut self, index: u32, arg: u32) -> (Option<u32>, Transfer) {
        let r = match index {
            0 => {
                self.selected = false;
                (None, Transfer::None)
            }
            // IO_SEND_OP_COND (R4): ready, the number of functions, no memory, 2.7-3.6 V
            5 => (Some(1 << 31 | (self.functions() as u32) << 28 | 0x00ff_8000), Transfer::None),
            // SEND_RELATIVE_ADDR (R6)
            3 => {
                self.rca = 0x0001;
                (Some((self.rca as u32) << 16), Transfer::None)
            }
            // SELECT/DESELECT_CARD (R1b)
            7 => {
                self.selected = arg >> 16 == self.rca as u32;
                (Some(if self.selected { 0x0f00 } else { 0 }), Transfer::None)
            }
            52 => (Some(self.direct(arg)), Transfer::None),
            53 => {
                let func = (arg >> 28 & 7) as u8;
                if func > self.functions() {
                    return (Some(R5_CMD | R5_FUNCTION_NUMBER), Transfer::None);
                }
                self.ext = arg;
                (Some(R5_TRN), if arg & 1 << 31 != 0 { Transfer::Write } else { Transfer::Read })
            }
            // SD memory commands (CMD8 SEND_IF_COND, ACMD41...): not a memory card
            _ => (None, Transfer::None),
        };
        if self.trace && index != 52 && index != 53 {
            eprintln!("[sdio] CMD{index} {arg:#010x} -> {:?}", r.0);
        }
        r
    }

    /// CMD52 IO_RW_DIRECT: one byte. Returns the R5 response.
    fn direct(&mut self, arg: u32) -> u32 {
        let write = arg & 1 << 31 != 0;
        let func = (arg >> 28 & 7) as u8;
        let raw = arg & 1 << 27 != 0;
        let addr = arg >> 9 & 0x1ffff;
        let data = arg as u8;
        if func > self.functions() {
            return R5_CMD | R5_FUNCTION_NUMBER;
        }
        if write {
            if func == 0 {
                self.write0(addr, data);
            } else {
                self.dev.write(func, addr, data);
            }
            if !raw {
                return R5_CMD | data as u32;
            }
        }
        let value = if func == 0 { self.read0(addr) } else { self.dev.read(func, addr) };
        if self.trace && func == 0 && addr < 0x100 {
            eprintln!("[sdio] CMD52 {} f0 {addr:#x} = {value:#04x}", if write { "W" } else { "R" });
        }
        R5_CMD | value as u32
    }

    /// Function 0: CCCR, FBRs, CIS.
    fn read0(&mut self, addr: u32) -> u8 {
        let fbr = |a: u32| (a >> 8, a & 0xff);
        match addr {
            CCCR_REV => CCCR_VERSION,
            SD_REV => 0x03,
            IOE => self.ioe,
            IOR => self.ioe & self.dev.ready(),
            IEN => self.ien,
            INT => self.pending(),
            BUS_IF => self.bus_if,
            CAPS => CAPS_VALUE,
            CIS_PTR => CIS_BASE as u8,
            0x0a => (CIS_BASE >> 8) as u8,
            0x0b => (CIS_BASE >> 16) as u8,
            FN0_BLKSIZE => self.fn0_blksize as u8,
            0x11 => (self.fn0_blksize >> 8) as u8,
            SPEED => SPEED_SHS | self.speed,
            0xf0..=0xff => self.dev.vendor_read(addr as u8),
            0x100..=0x7ff => {
                let (n, reg) = fbr(addr);
                let cis = CIS_BASE + 0x100 * n;
                match reg {
                    0x09 => cis as u8,
                    0x0a => (cis >> 8) as u8,
                    0x0b => (cis >> 16) as u8,
                    0x10 => self.blksize[n as usize] as u8,
                    0x11 => (self.blksize[n as usize] >> 8) as u8,
                    _ => 0,
                }
            }
            a if a >= CIS_BASE => self.cis.get((a - CIS_BASE) as usize).copied().unwrap_or(0xff),
            _ => 0,
        }
    }

    fn write0(&mut self, addr: u32, value: u8) {
        match addr {
            IOE => {
                self.ioe = value & 0xfe;
                self.dev.enable(self.ioe);
            }
            IEN => self.ien = value,
            ABORT if value & 0x08 != 0 => {
                // RES: I/O reset
                self.ioe = 0;
                self.ien = 0;
                self.bus_if = 0;
                self.speed = 0;
                self.dev.reset();
            }
            BUS_IF => self.bus_if = value,
            FN0_BLKSIZE => self.fn0_blksize = self.fn0_blksize & 0xff00 | value as u16,
            0x11 => self.fn0_blksize = self.fn0_blksize & 0xff | (value as u16) << 8,
            SPEED => self.speed = value & 0x0e,
            0xf0..=0xff => self.dev.vendor_write(addr as u8, value),
            0x100..=0x7ff => {
                let n = (addr >> 8) as usize;
                match addr & 0xff {
                    0x10 => self.blksize[n] = self.blksize[n] & 0xff00 | value as u16,
                    0x11 => self.blksize[n] = self.blksize[n] & 0xff | (value as u16) << 8,
                    _ => {}
                }
            }
            _ => {}
        }
    }

    /// Functions with an interrupt pending that the host enabled (CCCR INTx).
    pub fn pending(&self) -> u8 {
        if self.ien & 1 == 0 {
            return 0;
        }
        self.dev.pending() & self.ien & 0xfe
    }

    /// The card's interrupt (DAT1) for the host controller's SDIO interrupt.
    pub fn irq(&self) -> bool {
        self.pending() != 0
    }

    /// The device behind the card, as its concrete type.
    pub fn device_mut<T: 'static>(&mut self) -> Option<&mut T> {
        self.dev.as_any_mut()?.downcast_mut::<T>()
    }

    /// CMD53 read data (`len` bytes, as the host's byte count gives them).
    pub fn read_data(&mut self, len: usize) -> Vec<u8> {
        let (func, addr, incr) = self.ext_target();
        let mut out = vec![0; len];
        if func == 0 {
            for (i, b) in out.iter_mut().enumerate() {
                *b = self.read0(if incr { addr + i as u32 } else { addr });
            }
        } else {
            self.dev.read_block(func, addr, incr, &mut out);
        }
        if self.trace {
            eprintln!("[sdio] CMD53 R f{func} {addr:#x} {len} bytes{}", if incr { "" } else { " (fixed)" });
        }
        out
    }

    /// CMD53 write data.
    pub fn write_data(&mut self, data: &[u8]) {
        let (func, addr, incr) = self.ext_target();
        if self.trace {
            eprintln!("[sdio] CMD53 W f{func} {addr:#x} {} bytes{}", data.len(), if incr { "" } else { " (fixed)" });
        }
        if func == 0 {
            for (i, &b) in data.iter().enumerate() {
                self.write0(if incr { addr + i as u32 } else { addr }, b);
            }
        } else {
            self.dev.write_block(func, addr, incr, data);
        }
    }

    fn ext_target(&self) -> (u8, u32, bool) {
        let a = self.ext;
        ((a >> 28 & 7) as u8, a >> 9 & 0x1ffff, a & 1 << 26 != 0)
    }
}

/// CISTPL_MANFID and FUNCE (function 0) for the common CIS, then MANFID and the long
/// FUNCE (function n) for each function's CIS, each 0x100 bytes apart.
fn build_cis(dev: &dyn SdioFunctions) -> Vec<u8> {
    let (vendor, device) = dev.manfid();
    let mut cis = vec![0xff; 0x100 * (dev.functions().clamp(1, 7) as usize + 1)];
    let manfid = [0x20, 4, vendor as u8, (vendor >> 8) as u8, device as u8, (device >> 8) as u8];
    let common = dev.max_block_size(0);
    // FUNCE type 0: function 0's largest block, 25 MHz (TPLFE_MAX_TRAN_SPEED 0x32)
    let mut t = manfid.to_vec();
    t.extend_from_slice(&[0x22, 4, 0x00, common as u8, (common >> 8) as u8, 0x32, 0xff]);
    cis[..t.len()].copy_from_slice(&t);
    for n in 1..=dev.functions().clamp(1, 7) {
        let mut funce = [0u8; 42];
        funce[0] = 0x01; // type 1: a function's extension
        let blk = dev.max_block_size(n);
        funce[12] = blk as u8;
        funce[13] = (blk >> 8) as u8;
        funce[28] = 100; // enable timeout: 1 s in 10 ms units
        let mut t = manfid.to_vec();
        t.extend_from_slice(&[0x22, funce.len() as u8]);
        t.extend_from_slice(&funce);
        t.push(0xff);
        let at = 0x100 * n as usize;
        cis[at..at + t.len()].copy_from_slice(&t);
    }
    cis
}

#[cfg(test)]
mod tests {
    use super::*;

    /// One function of 16 bytes of registers.
    struct Regs([u8; 16]);

    impl SdioFunctions for Regs {
        fn functions(&self) -> u8 {
            1
        }
        fn manfid(&self) -> (u16, u16) {
            (0x02d0, 0xa9bf)
        }
        fn read(&mut self, _func: u8, addr: u32) -> u8 {
            self.0[addr as usize & 15]
        }
        fn write(&mut self, _func: u8, addr: u32, value: u8) {
            self.0[addr as usize & 15] = value;
        }
    }

    fn cmd52(card: &mut SdioCard, write: bool, func: u32, addr: u32, data: u8) -> u8 {
        let arg = (write as u32) << 31 | func << 28 | addr << 9 | data as u32;
        let (r, t) = card.command(52, arg);
        assert_eq!(t, Transfer::None);
        r.unwrap() as u8
    }

    #[test]
    fn identification_cccr_cis_and_io() {
        let mut card = SdioCard::new(Box::new(Regs([0; 16])));
        assert_eq!(card.command(8, 0x1aa).0, None, "no memory card");
        assert_eq!(card.command(5, 0).0, Some(0x9000_0000 | 0x00ff_8000), "ready, one function");
        assert_eq!(card.command(3, 0).0, Some(0x0001_0000));
        assert!(card.command(7, 0x0001_0000).0.is_some());
        assert_eq!(cmd52(&mut card, false, 0, 0, 0), CCCR_VERSION);
        // the common CIS: MANFID first
        let cis = cmd52(&mut card, false, 0, 9, 0) as u32 | (cmd52(&mut card, false, 0, 10, 0) as u32) << 8;
        let tuple: Vec<u8> = (0..6).map(|i| cmd52(&mut card, false, 0, cis + i, 0)).collect();
        assert_eq!(tuple, [0x20, 4, 0xd0, 0x02, 0xbf, 0xa9]);
        // enable function 1: ready at once
        cmd52(&mut card, true, 0, IOE, 0x02);
        assert_eq!(cmd52(&mut card, false, 0, IOR, 0), 0x02);
        // a function register, written with read-after-write
        let arg = 1 << 31 | 1 << 28 | 1 << 27 | 5 << 9 | 0x5a;
        assert_eq!(card.command(52, arg).0, Some(R5_CMD | 0x5a));
        // CMD53: 4 bytes from address 4, incrementing
        let (_, t) = card.command(53, 1 << 28 | 1 << 26 | 4 << 9 | 4);
        assert_eq!(t, Transfer::Read);
        assert_eq!(card.read_data(4), [0, 0x5a, 0, 0]);
        let (_, t) = card.command(53, 1 << 31 | 1 << 28 | 1 << 26 | 8 << 9 | 2);
        assert_eq!(t, Transfer::Write);
        card.write_data(&[7, 8]);
        assert_eq!(cmd52(&mut card, false, 1, 9, 0), 8);
        // a function the card does not have
        assert_ne!(card.command(52, 3 << 28).0.unwrap() & R5_FUNCTION_NUMBER, 0);
    }
}
