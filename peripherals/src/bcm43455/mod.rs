//! Broadcom BCM43455 (CYW43455) 802.11ac Wi-Fi on SDIO, as Linux's `brcmfmac` drives it.
//!
//! The bus side is modelled register by register:
//! - SDIO function 1: the backplane window (SBADDR), the clock CSR, the sleep CSR (KSO),
//!   and 32-bit backplane access with the 2/4-byte flag.
//! - The AXI backplane brcmfmac walks: chipcommon with its chip ID, EROM pointer and
//!   PMU; an enumeration ROM listing chipcommon, the 802.11 core, the ARM Cortex-R4 and
//!   the SDIO device core; the cores' wrappers (IOCTL, RESET_CTL); and the Cortex-R4's
//!   TCM (800 KiB at 0x198000), where the driver loads the firmware and NVRAM.
//! - The SDIO core's interrupt and mailbox registers.
//! - SDIO function 2: the SDPCM frame stream in both directions.
//!
//! The firmware Broadcom ships for the chip's Cortex-R4 is not run. When the driver takes
//! the CPU out of halt, the dongle side (`dongle.rs`) starts in its place: it reports
//! ready through the mailbox and answers the control channel (BCDC ioctls and iovars) and
//! the data channel itself.
//!
//! Interrupts reach the host on the out-of-band line (the `host-wake` GPIO, configured
//! through the Broadcom CCCR register SEPINT) or in band (SDIO DAT1).

mod dongle;

use crate::sdio::SdioFunctions;
use dongle::Dongle;
use std::collections::VecDeque;

pub use dongle::Station;

/// SDIO IDs: Broadcom, BCM43455.
pub const VENDOR: u16 = 0x02d0;
pub const DEVICE: u16 = 0xa9bf;
/// Chip 0x4345 revision 6 (brcmfmac loads `brcmfmac43455-sdio.bin`), AXI backplane.
const CHIPID: u32 = 0x4345 | 6 << 16 | 4 << 24 | 1 << 28;

// Backplane map
const CC_BASE: u32 = 0x1800_0000;
const D11_BASE: u32 = 0x1800_1000;
const ARM_BASE: u32 = 0x1800_2000;
const SDIOD_BASE: u32 = 0x1800_3000;
const WRAP: u32 = 0x10_0000;
const EROM_BASE: u32 = 0x180f_c000;
pub const RAM_BASE: u32 = 0x19_8000;
/// TCM banks: A 160 KiB, B 640 KiB (800 KiB, as the real part).
const BANKS: [u32; 2] = [20, 80];
pub const RAM_SIZE: u32 = (BANKS[0] + BANKS[1]) * 8192;

// Core IDs and revisions
const CORE_CC: u32 = 0x800;
const CORE_80211: u32 = 0x812;
const CORE_SDIOD: u32 = 0x829;
const CORE_ARM_CR4: u32 = 0x83e;
const CORES: [(u32, u32, u32); 4] = [(CORE_CC, CC_BASE, 54), (CORE_80211, D11_BASE, 46), (CORE_ARM_CR4, ARM_BASE, 7), (CORE_SDIOD, SDIOD_BASE, 21)];

// Wrapper registers
const BCMA_IOCTL: u32 = 0x408;
const BCMA_RESET_CTL: u32 = 0x800;
const IOCTL_CLK: u32 = 1;
const IOCTL_CPUHALT: u32 = 0x20;

// Function 1 registers
const F1_SBADDRLOW: u32 = 0x1000a;
const F1_CHIPCLKCSR: u32 = 0x1000e;
const F1_SLEEPCSR: u32 = 0x1001f;
const ACCESS_4B: u32 = 0x8000;
const CLK_ALP_AVAIL: u8 = 0x40;
const CLK_HT_AVAIL: u8 = 0x80;
const CLK_REQUESTS: u8 = 0x01 | 0x02 | 0x08 | 0x10;

// SDIO core registers
const SD_INTSTATUS: u32 = 0x20;
const SD_HOSTINTMASK: u32 = 0x24;
const SD_TOHOSTMAILBOXDATA: u32 = 0x4c;
/// intstatus: frame indication, host mailbox interrupt.
const I_HMB_FRAME_IND: u32 = 1 << 6;
const I_HMB_HOST_INT: u32 = 1 << 7;
const HMB_DATA_FWREADY: u32 = 0x08;
const SDPCM_PROT_VERSION: u32 = 4;

// Broadcom CCCR registers
const CCCR_CARDCTRL: u8 = 0xf1;
const CCCR_SEPINT: u8 = 0xf2;
const SEPINT_MASK: u8 = 1;
const SEPINT_OE: u8 = 2;
const SEPINT_ACT_HI: u8 = 4;

#[derive(Default, Clone, Copy)]
struct Wrapper {
    ioctl: u32,
    reset: u32,
}

pub struct Bcm43455 {
    window: u32,
    clkcsr: u8,
    sleepcsr: u8,
    f1misc: [u8; 0x20],
    cardctrl: u8,
    sepint: u8,
    enabled: u8,
    wrappers: [Wrapper; 4],
    cc: Vec<u32>,
    sdiod: Vec<u32>,
    arm_bankidx: u32,
    erom: Vec<u32>,
    ram: Vec<u8>,
    /// The emulated firmware, once the CPU runs.
    dongle: Option<Dongle>,
    mac: [u8; 6],
    /// Frames to the host (SDPCM, complete) and the one being read.
    rx: VecDeque<Vec<u8>>,
    reading: Option<(Vec<u8>, usize)>,
    /// Bytes of the frame the host is writing.
    tx: Vec<u8>,
    /// A new interrupt cause since the host last looked: the out-of-band line gets a
    /// fresh edge.
    edge: bool,
    pub log: bool,
}

impl Bcm43455 {
    pub fn new(mac: [u8; 6]) -> Self {
        let mut erom = Vec::new();
        for &(id, base, rev) in &CORES {
            // CompIdentA: Broadcom (0x4bf), part number; CompIdentB: revision, one slave
            // wrapper; then the core's registers and its wrapper, 4 KiB each
            erom.push(0x4bf << 20 | id << 8 | 0x1);
            erom.push(rev << 24 | 1 << 19 | 0x1);
            erom.push(base | 0x5);
            erom.push((base + WRAP) | 2 << 6 | 0x5);
        }
        erom.push(0xf); // end of table
        Self {
            window: 0,
            clkcsr: 0,
            sleepcsr: 0,
            f1misc: [0; 0x20],
            cardctrl: 0,
            sepint: 0,
            enabled: 0,
            wrappers: [Wrapper { ioctl: 0, reset: 1 }; 4],
            cc: vec![0; 0x1000 / 4],
            sdiod: vec![0; 0x1000 / 4],
            arm_bankidx: 0,
            erom,
            ram: vec![0; RAM_SIZE as usize],
            dongle: None,
            mac,
            rx: VecDeque::new(),
            reading: None,
            tx: Vec::new(),
            edge: false,
            log: std::env::var_os("SBC_BRCM_LOG").is_some(),
        }
    }

    /// The firmware is running (the CPU out of reset and halt).
    pub fn running(&self) -> bool {
        self.dongle.is_some()
    }

    fn core_index(addr: u32) -> Option<usize> {
        CORES.iter().position(|&(_, base, _)| addr & !0xfff == base || addr & !0xfff == base + WRAP)
    }

    /// A 32-bit backplane read.
    fn read32(&mut self, addr: u32) -> u32 {
        let v = match addr {
            a if (RAM_BASE..RAM_BASE + RAM_SIZE - 3).contains(&a) => {
                let i = (a - RAM_BASE) as usize;
                u32::from_le_bytes(self.ram[i..i + 4].try_into().unwrap())
            }
            a if (EROM_BASE..EROM_BASE + 0x1000).contains(&a) => self.erom.get(((a - EROM_BASE) / 4) as usize).copied().unwrap_or(0xf),
            a if a & WRAP != 0 && Self::core_index(a).is_some() => {
                let w = self.wrappers[Self::core_index(a).unwrap()];
                match a & 0xfff {
                    BCMA_IOCTL => w.ioctl,
                    BCMA_RESET_CTL => w.reset,
                    _ => 0,
                }
            }
            a if a & !0xfff == CC_BASE => match a & 0xfff {
                0x000 => CHIPID,
                0x004 => 0x1000_0000, // capabilities: PMU
                0x0fc => EROM_BASE,
                0x604 => 24, // PMU capabilities: PMU revision 24
                0x608 => 0x0000_0007, // PMU status: ALP/HT available
                o => self.cc[o as usize / 4],
            },
            a if a & !0xfff == ARM_BASE => match a & 0xfff {
                0x04 => 1 | 1 << 4, // one bank A, one bank B
                0x40 => self.arm_bankidx,
                0x44 => BANKS.get(self.arm_bankidx as usize).map_or(0, |b| b - 1),
                _ => 0,
            },
            a if a & !0xfff == SDIOD_BASE => self.sdiod[(a & 0xfff) as usize / 4],
            _ => 0,
        };
        v
    }

    fn write32(&mut self, addr: u32, value: u32) {
        match addr {
            a if (RAM_BASE..RAM_BASE + RAM_SIZE - 3).contains(&a) => {
                let i = (a - RAM_BASE) as usize;
                self.ram[i..i + 4].copy_from_slice(&value.to_le_bytes());
            }
            a if a & WRAP != 0 && Self::core_index(a).is_some() => {
                let k = Self::core_index(a).unwrap();
                match a & 0xfff {
                    BCMA_IOCTL => self.wrappers[k].ioctl = value,
                    BCMA_RESET_CTL => self.wrappers[k].reset = value & 1,
                    _ => {}
                }
                if CORES[k].0 == CORE_ARM_CR4 {
                    self.arm_state();
                }
            }
            a if a & !0xfff == CC_BASE => self.cc[(a & 0xfff) as usize / 4] = value,
            a if a & !0xfff == ARM_BASE => {
                if a & 0xfff == 0x40 {
                    self.arm_bankidx = value;
                }
            }
            a if a & !0xfff == SDIOD_BASE => {
                let o = (a & 0xfff) as usize / 4;
                match a & 0xfff {
                    SD_INTSTATUS => self.sdiod[o] &= !value,
                    _ => self.sdiod[o] = value,
                }
            }
            _ => {}
        }
    }

    /// The Cortex-R4 runs when its wrapper has it out of reset, clocked and not halted:
    /// the firmware starts (or stops when the driver halts it again).
    fn arm_state(&mut self) {
        let k = CORES.iter().position(|c| c.0 == CORE_ARM_CR4).unwrap();
        let w = self.wrappers[k];
        let run = w.reset == 0 && w.ioctl & IOCTL_CLK != 0 && w.ioctl & IOCTL_CPUHALT == 0;
        match (run, self.dongle.is_some()) {
            (true, false) => self.boot(),
            (false, true) => {
                self.dongle = None;
                self.rx.clear();
                self.reading = None;
            }
            _ => {}
        }
    }

    /// The firmware's start-up: ready on the mailbox, the shared-structure pointer in the
    /// last word of RAM (where the driver left the NVRAM length token).
    fn boot(&mut self) {
        if self.log {
            let nvram = self.ram_word(RAM_SIZE - 4);
            eprintln!("[brcm] firmware started (reset vector {:#x}, NVRAM token {nvram:#x})", self.ram_word(0));
        }
        // sdpcm_shared: version 3, no console, no trap
        let shared = RAM_SIZE - 0x100;
        self.set_ram_word(shared, 3);
        for k in 1..8 {
            self.set_ram_word(shared + 4 * k, 0);
        }
        self.set_ram_word(RAM_SIZE - 4, RAM_BASE + shared);
        self.dongle = Some(Dongle::new(self.mac, self.log));
        self.sdiod[SD_TOHOSTMAILBOXDATA as usize / 4] = HMB_DATA_FWREADY | SDPCM_PROT_VERSION << 16;
        self.interrupt(I_HMB_HOST_INT);
    }

    fn ram_word(&self, off: u32) -> u32 {
        let i = off as usize;
        u32::from_le_bytes(self.ram[i..i + 4].try_into().unwrap())
    }

    fn set_ram_word(&mut self, off: u32, v: u32) {
        let i = off as usize;
        self.ram[i..i + 4].copy_from_slice(&v.to_le_bytes());
    }

    fn interrupt(&mut self, bits: u32) {
        self.sdiod[SD_INTSTATUS as usize / 4] |= bits;
        self.edge = true;
    }

    fn host_pending(&self) -> bool {
        self.sdiod[SD_INTSTATUS as usize / 4] & self.sdiod[SD_HOSTINTMASK as usize / 4] != 0
    }

    /// The out-of-band interrupt line's level, when the driver has routed it there
    /// (SEPINT MASK and OE); None while the pin is not driven.
    pub fn oob_level(&self) -> Option<bool> {
        if self.sepint & (SEPINT_MASK | SEPINT_OE) != SEPINT_MASK | SEPINT_OE {
            return None;
        }
        let active = self.host_pending();
        Some(active == (self.sepint & SEPINT_ACT_HI != 0))
    }

    /// A new interrupt cause since the last call: the line should show a fresh edge.
    pub fn take_edge(&mut self) -> bool {
        std::mem::take(&mut self.edge)
    }

    /// Ethernet frames the dongle sends to the air (from the host's data channel).
    pub fn take_transmitted(&mut self) -> Vec<Vec<u8>> {
        self.dongle.as_mut().map(|d| std::mem::take(&mut d.to_air)).unwrap_or_default()
    }

    /// An Ethernet frame from the air for the host.
    pub fn receive(&mut self, frame: &[u8]) {
        if let Some(d) = self.dongle.as_mut() {
            d.receive(frame);
        }
        self.flush();
    }

    /// Advance the dongle's own work (scans, association) to `now_ms`.
    pub fn tick(&mut self, now_ms: u64) {
        if let Some(d) = self.dongle.as_mut() {
            d.tick(now_ms);
        }
        self.flush();
    }

    /// Move the dongle's outgoing messages into SDPCM frames for the host.
    fn flush(&mut self) {
        let Some(d) = self.dongle.as_mut() else { return };
        let frames = d.take_frames();
        if frames.is_empty() {
            return;
        }
        self.rx.extend(frames);
        self.interrupt(I_HMB_FRAME_IND);
    }

    /// A complete SDPCM frame from the host.
    fn host_frame(&mut self, frame: &[u8]) {
        if let Some(d) = self.dongle.as_mut() {
            d.host_frame(frame);
        } else if self.log {
            eprintln!("[brcm] frame before the firmware runs, dropped");
        }
        self.flush();
    }

    /// Function 1 byte register / backplane access.
    fn f1_read(&mut self, addr: u32) -> u8 {
        match addr {
            F1_CHIPCLKCSR => {
                let avail = if self.clkcsr & CLK_REQUESTS != 0 { CLK_ALP_AVAIL | CLK_HT_AVAIL } else { 0 };
                self.clkcsr | avail
            }
            F1_SLEEPCSR => self.sleepcsr | (self.sleepcsr & 1) << 1,
            0x10000..=0x1001f => self.f1misc[(addr - 0x10000) as usize],
            a if a < 0x10000 => {
                let bp = self.window | (a & 0x7fff);
                let word = self.read32(bp & !3);
                (word >> (8 * (bp & 3))) as u8
            }
            _ => 0,
        }
    }

    fn f1_write(&mut self, addr: u32, value: u8) {
        match addr {
            F1_CHIPCLKCSR => self.clkcsr = value & 0x3f,
            F1_SLEEPCSR => self.sleepcsr = value & 1,
            0x10000..=0x1001f => {
                self.f1misc[(addr - 0x10000) as usize] = value;
                if (F1_SBADDRLOW..F1_SBADDRLOW + 3).contains(&addr) {
                    let m = &self.f1misc[0x0a..0x0d];
                    self.window = ((m[0] as u32) << 8 | (m[1] as u32) << 16 | (m[2] as u32) << 24) & 0xffff_8000;
                }
            }
            a if a < 0x10000 => {
                let bp = self.window | (a & 0x7fff);
                if (RAM_BASE..RAM_BASE + RAM_SIZE).contains(&bp) {
                    self.ram[(bp - RAM_BASE) as usize] = value;
                } else {
                    let shift = 8 * (bp & 3);
                    let word = self.read32(bp & !3) & !(0xff << shift) | (value as u32) << shift;
                    self.write32(bp & !3, word);
                }
            }
            _ => {}
        }
    }
}

impl SdioFunctions for Bcm43455 {
    fn functions(&self) -> u8 {
        2
    }

    fn manfid(&self) -> (u16, u16) {
        (VENDOR, DEVICE)
    }

    fn max_block_size(&self, func: u8) -> u16 {
        match func {
            1 => 64,
            _ => 512,
        }
    }

    fn read(&mut self, func: u8, addr: u32) -> u8 {
        match func {
            1 => self.f1_read(addr),
            _ => {
                let mut b = [0];
                self.read_block(func, addr, false, &mut b);
                b[0]
            }
        }
    }

    fn write(&mut self, func: u8, addr: u32, value: u8) {
        match func {
            1 => self.f1_write(addr, value),
            _ => self.write_block(func, addr, false, &[value]),
        }
    }

    fn read_block(&mut self, func: u8, addr: u32, incr: bool, out: &mut [u8]) {
        if func == 2 {
            // the receive FIFO: the frame being read continues, else the next one; past
            // a frame's end (padding) and with no frame, zeros (a null header)
            if self.reading.is_none() {
                self.reading = self.rx.pop_front().map(|f| (f, 0));
            }
            if let Some((f, pos)) = self.reading.as_mut() {
                for b in out.iter_mut() {
                    *b = f.get(*pos).copied().unwrap_or(0);
                    *pos += 1;
                }
                if *pos >= f.len() {
                    self.reading = None;
                }
            } else {
                out.fill(0);
            }
            return;
        }
        // function 1: 32-bit backplane access, RAM in bulk
        let bp = self.window | (addr & 0x7fff);
        if addr < 0x10000 && (RAM_BASE..RAM_BASE + RAM_SIZE).contains(&bp) && incr {
            let start = (bp - RAM_BASE) as usize;
            let n = out.len().min(self.ram.len() - start);
            out[..n].copy_from_slice(&self.ram[start..start + n]);
            return;
        }
        if addr < 0x10000 && addr & ACCESS_4B != 0 && out.len() == 4 && bp & 3 == 0 {
            out.copy_from_slice(&self.read32(bp).to_le_bytes());
            return;
        }
        for (i, b) in out.iter_mut().enumerate() {
            *b = self.f1_read(if incr { addr + i as u32 } else { addr });
        }
    }

    fn write_block(&mut self, func: u8, addr: u32, incr: bool, data: &[u8]) {
        if func == 2 {
            // the transmit FIFO: a frame may come in several transfers; what follows its
            // end in the last one is padding
            self.tx.extend_from_slice(data);
            if self.tx.len() >= 4 {
                let len = u16::from_le_bytes([self.tx[0], self.tx[1]]) as usize;
                let check = u16::from_le_bytes([self.tx[2], self.tx[3]]) as usize;
                if len == 0 || len ^ check != 0xffff {
                    self.tx.clear();
                } else if self.tx.len() >= len {
                    let frame: Vec<u8> = self.tx[..len].to_vec();
                    self.tx.clear();
                    self.host_frame(&frame);
                }
            }
            return;
        }
        let bp = self.window | (addr & 0x7fff);
        if addr < 0x10000 && (RAM_BASE..RAM_BASE + RAM_SIZE).contains(&bp) && incr {
            let start = (bp - RAM_BASE) as usize;
            let n = data.len().min(self.ram.len() - start);
            self.ram[start..start + n].copy_from_slice(&data[..n]);
            return;
        }
        if addr < 0x10000 && addr & ACCESS_4B != 0 && data.len() == 4 && bp & 3 == 0 {
            self.write32(bp, u32::from_le_bytes(data.try_into().unwrap()));
            return;
        }
        for (i, &b) in data.iter().enumerate() {
            self.f1_write(if incr { addr + i as u32 } else { addr }, b);
        }
    }

    fn enable(&mut self, mask: u8) {
        self.enabled = mask;
    }

    fn ready(&self) -> u8 {
        // function 2 is ready once the firmware runs
        0x02 | if self.running() { 0x04 } else { 0 }
    }

    fn pending(&self) -> u8 {
        if self.host_pending() {
            0x06
        } else {
            0
        }
    }

    fn vendor_read(&mut self, addr: u8) -> u8 {
        match addr {
            CCCR_CARDCTRL => self.cardctrl,
            CCCR_SEPINT => self.sepint,
            _ => 0,
        }
    }

    fn vendor_write(&mut self, addr: u8, value: u8) {
        match addr {
            CCCR_CARDCTRL => self.cardctrl = value,
            CCCR_SEPINT => self.sepint = value,
            _ => {}
        }
    }

    fn reset(&mut self) {
        let (mac, log) = (self.mac, self.log);
        *self = Self::new(mac);
        self.log = log;
    }

    fn as_any_mut(&mut self) -> Option<&mut dyn std::any::Any> {
        Some(self)
    }
}

#[cfg(test)]
mod tests;
