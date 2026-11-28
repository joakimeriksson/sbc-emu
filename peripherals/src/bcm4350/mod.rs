//! Broadcom BCM4350 802.11ac Wi-Fi on PCIe (an M.2 card such as the DW1820A), as Linux's
//! `brcmfmac` drives it.
//!
//! The bus side is modelled register by register:
//! - PCI configuration space: Broadcom 14e4:43a3, class 0280 (network, other), BAR0 (32 KiB
//!   of registers) and BAR2 (4 MiB onto the ARM's address space, the TCM at 0x180000), the
//!   PM, MSI and PCIe capabilities where brcmfmac expects them, and Broadcom's own registers
//!   above them: the BAR0 window (0x80) and the to-dongle mailbox (0x98).
//! - BAR0: the 4 KiB window onto the AXI backplane (chipcommon with its chip ID and EROM
//!   pointer, the 802.11 core, the ARM Cortex-R4 with its TCM banks, the PCIe2 core; the
//!   cores' wrappers) and the PCIe2 core's own registers at 0x2000 (mailbox interrupts and
//!   doorbells, the indirect configuration access).
//! - BAR2: the Cortex-R4's TCM (768 KiB), where the driver loads the firmware.
//!
//! The firmware Broadcom ships for the chip (`brcmfmac4350-pcie.bin`) is not run. When the
//! driver takes the Cortex-R4 out of halt, the dongle side starts in its place: it publishes
//! the msgbuf shared structure (version 5, ring indices in TCM) at the end of RAM, and
//! answers the host's message rings — ioctls (through the dongle of `bcm43455`, which
//! knows brcmfmac's `fwil` commands, scans, joins and WPA2), events, flow rings, transmit
//! and receive buffers. Interrupts are MSIs (the PCIe2 core's mailbox interrupt, as
//! brcmfmac masks and acknowledges it).

use crate::bcm43455::dongle::{Dongle, Identity, ToHost};
use crate::bcm43455::Network;
use crate::pci::{self, Config, Function, Master};
use std::collections::{HashMap, VecDeque};

pub const VENDOR: u16 = 0x14e4;
pub const DEVICE: u16 = 0x43a3;
/// Chip 0x4350 revision 8 (brcmfmac loads `brcmfmac4350-pcie.bin`), AXI backplane.
const CHIPID: u32 = 0x4350 | 8 << 16 | 4 << 24 | 1 << 28;

const IDENTITY: Identity = Identity {
    device: DEVICE as u32,
    chip: 0x4350,
    chiprev: 8,
    bus: 2,
    ver: "wl0: Oct 22 2015 06:18:37 version 7.35.180.119 (sbc-emu dongle in place of the firmware) FWID 01-e791c176\n",
};

// Backplane map
const CC_BASE: u32 = 0x1800_0000;
const D11_BASE: u32 = 0x1800_1000;
const ARM_BASE: u32 = 0x1800_2000;
const PCIE2_BASE: u32 = 0x1800_3000;
const WRAP: u32 = 0x10_0000;
const EROM_BASE: u32 = 0x180f_c000;

// Core IDs and revisions
const CORE_CC: u32 = 0x800;
const CORE_80211: u32 = 0x812;
const CORE_ARM_CR4: u32 = 0x83e;
const CORE_PCIE2: u32 = 0x83c;
const CORES: [(u32, u32, u32); 4] = [(CORE_CC, CC_BASE, 49), (CORE_80211, D11_BASE, 42), (CORE_ARM_CR4, ARM_BASE, 7), (CORE_PCIE2, PCIE2_BASE, 11)];

/// The Cortex-R4's view: RAM at 0x180000, 768 KiB in six 128 KiB banks.
pub const RAM_BASE: u32 = 0x18_0000;
const BANKS: u32 = 6;
const BANK_SIZE: u32 = 128 << 10;
pub const RAM_SIZE: u32 = BANKS * BANK_SIZE;
const BAR0_SIZE: u64 = 0x8000;
const BAR2_SIZE: u64 = 4 << 20;

// Wrapper registers
const BCMA_IOCTL: u32 = 0x408;
const BCMA_RESET_CTL: u32 = 0x800;
const IOCTL_CLK: u32 = 1;
const IOCTL_CPUHALT: u32 = 0x20;

// Chipcommon
const CC_WATCHDOG: u32 = 0x80;

// PCIe2 core registers
const PCIE2_INTMASK: u32 = 0x24;
const PCIE2_MAILBOXINT: u32 = 0x48;
const PCIE2_MAILBOXMASK: u32 = 0x4c;
const PCIE2_CONFIGADDR: u32 = 0x120;
const PCIE2_CONFIGDATA: u32 = 0x124;
const PCIE2_H2D_MAILBOX_0: u32 = 0x140;
const MB_INT_FN0_0: u32 = 0x100;
const MB_INT_D2H0_DB0: u32 = 0x1_0000;

// Configuration space: capabilities and Broadcom's registers
const CAP_PM: usize = 0x48;
const CAP_MSI: usize = 0x58;
const CAP_EXP: usize = 0xac;
const CFG_BAR0_WINDOW: usize = 0x80;
const CFG_SBMBX: usize = 0x98;

// msgbuf: the shared structure and what it points at, near the end of RAM (above the
// firmware image, below any NVRAM)
const SHARED_VERSION: u32 = 5;
const SHARED: u32 = RAM_BASE + RAM_SIZE - 0x1_0000;
const RING_INFO: u32 = SHARED + 0x100;
const RING_MEM: u32 = SHARED + 0x200;
const MAX_FLOWRINGS: u32 = 16;
/// H2D rings: control submit, receive buffer post, then the flow rings
const H2D_RINGS: u32 = 2 + MAX_FLOWRINGS;
/// D2H rings: control complete, transmit complete, receive complete
const D2H_RINGS: u32 = 3;
const H2D_W: u32 = SHARED + 0x300;
const H2D_R: u32 = H2D_W + 4 * H2D_RINGS;
const D2H_W: u32 = H2D_R + 4 * H2D_RINGS;
const D2H_R: u32 = D2H_W + 4 * D2H_RINGS;
const HTOD_MB: u32 = SHARED + 0x600;
const DTOH_MB: u32 = SHARED + 0x604;
const CONSOLE: u32 = SHARED + 0x700;
const CONSOLE_BUF: u32 = SHARED + 0x800;
const CONSOLE_SIZE: u32 = 0x400;
const MAX_RXBUFPOST: u16 = 255;

// Ring IDs
const CONTROL_SUBMIT: u32 = 0;
const RXPOST_SUBMIT: u32 = 1;
const CONTROL_COMPLETE: u32 = 2;
const TX_COMPLETE: u32 = 3;
const RX_COMPLETE: u32 = 4;

// Message types
const MSG_FLOW_RING_CREATE: u8 = 0x3;
const MSG_FLOW_RING_CREATE_CMPLT: u8 = 0x4;
const MSG_FLOW_RING_DELETE: u8 = 0x5;
const MSG_FLOW_RING_DELETE_CMPLT: u8 = 0x6;
const MSG_FLOW_RING_FLUSH: u8 = 0x7;
const MSG_FLOW_RING_FLUSH_CMPLT: u8 = 0x8;
const MSG_IOCTLPTR_REQ: u8 = 0x9;
const MSG_IOCTLRESP_BUF_POST: u8 = 0xb;
const MSG_IOCTL_CMPLT: u8 = 0xc;
const MSG_EVENT_BUF_POST: u8 = 0xd;
const MSG_WL_EVENT: u8 = 0xe;
const MSG_TX_POST: u8 = 0xf;
const MSG_TX_STATUS: u8 = 0x10;
const MSG_RXBUF_POST: u8 = 0x11;
const MSG_RX_CMPLT: u8 = 0x12;
const PKT_FLAGS_FRAME_802_3: u16 = 0x01;

// Host <-> dongle mailbox data
const H2D_HOST_D3_INFORM: u32 = 0x1;
const D2H_DEV_D3_ACK: u32 = 0x1;

/// How long a scan takes: 13 channels at 2.4 GHz, active (the firmware's dwell times), before
/// its results come. A scan that finished at once would beat clients that subscribe to the
/// scan events only after triggering one (`iw scan`).
const SCAN_NS: u64 = 1_500_000_000;
const E_ESCAN_RESULT: u32 = 69;

/// Frames kept for the host while it has no receive buffer posted.
const MAX_PENDING_RX: usize = 256;

#[derive(Default, Clone, Copy)]
struct Wrapper {
    ioctl: u32,
    reset: u32,
}

/// A buffer the host posted: its packet ID, address and length.
#[derive(Clone, Copy, Debug)]
struct HostBuf {
    id: u32,
    addr: u64,
    len: usize,
}

#[derive(Clone, Copy, Debug)]
struct FlowRing {
    addr: u64,
    items: u16,
    item_len: u16,
    ifidx: u8,
}

/// An ioctl's answer waiting for a response buffer.
struct IoctlAnswer {
    cmd: u32,
    trans_id: u16,
    status: i32,
    data: Vec<u8>,
}

/// The emulated firmware once it runs: the dongle and the msgbuf rings.
struct Firmware {
    dongle: Dongle,
    flowrings: HashMap<u16, FlowRing>,
    ioctl_bufs: VecDeque<HostBuf>,
    event_bufs: VecDeque<HostBuf>,
    rx_bufs: VecDeque<HostBuf>,
    answers: VecDeque<IoctlAnswer>,
    events: VecDeque<Vec<u8>>,
    rx: VecDeque<Vec<u8>>,
    /// completion items not yet in their ring (it was full), per D2H ring
    completions: [VecDeque<Vec<u8>>; D2H_RINGS as usize],
    event_seq: u16,
    /// a scan's result events and when the scan ends
    scan: Option<(u64, Vec<Vec<u8>>)>,
}

pub struct Bcm4350 {
    cfg: Config,
    wrappers: [Wrapper; 4],
    cc: Vec<u32>,
    pcie2: Vec<u32>,
    arm_bankidx: u32,
    erom: Vec<u32>,
    ram: Vec<u8>,
    /// BAR2 below the RAM: the reset vector the driver writes at 0
    rom: [u8; 8],
    fw: Option<Firmware>,
    mac: [u8; 6],
    networks: Vec<Network>,
    /// the MSI was sent for the mailbox interrupt now pending
    signalled: bool,
    pub log: bool,
}

impl Bcm4350 {
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

        let mut cfg = Config::endpoint(VENDOR, DEVICE, 0x02_8000, 8);
        cfg.set16(0x06, 0x0010); // capability list
        cfg.memory_bar(0x10, BAR0_SIZE, true, false);
        cfg.memory_bar(0x18, BAR2_SIZE, true, false);
        // subsystem: Dell (DW1820A)
        cfg.set16(0x2c, 0x1028);
        cfg.set16(0x2e, 0x0023);
        cfg.set8(0x34, CAP_PM as u8);
        cfg.set8(0x3d, 1); // INTA
        // power management: version 3, D0..D3hot; PMCSR state and PME bits writable
        cfg.capability(CAP_PM, 0x01, CAP_MSI as u8);
        cfg.set16(CAP_PM + 2, 0x0003);
        cfg.writable(CAP_PM + 4, 2, 0x0103);
        // MSI: one vector, 64-bit address
        cfg.capability(CAP_MSI, 0x05, CAP_EXP as u8);
        cfg.set16(CAP_MSI + 2, 0x0080);
        cfg.writable(CAP_MSI + 2, 2, 0x0071);
        cfg.writable(CAP_MSI + 4, 4, 0xffff_fffc);
        cfg.writable(CAP_MSI + 8, 4, 0xffff_ffff);
        cfg.writable(CAP_MSI + 12, 2, 0xffff);
        // PCI Express v2 endpoint, x1 at 5 GT/s, link up
        cfg.capability(CAP_EXP, 0x10, 0);
        cfg.set16(CAP_EXP + 2, 0x0002);
        cfg.set32(CAP_EXP + 4, 0x0000_8000 | 2); // DEVCAP: role-based error, 512-byte payload
        cfg.writable(CAP_EXP + 8, 2, 0x7fff);
        cfg.clearable(CAP_EXP + 10, 2, 0x000f);
        cfg.set32(CAP_EXP + 12, 0x0004_0c12); // LNKCAP: 5 GT/s, x1, ASPM L0s/L1
        cfg.writable(CAP_EXP + 16, 2, 0x0fc3);
        cfg.set16(CAP_EXP + 18, 0x1012); // LNKSTA: 5 GT/s x1, common clock
        cfg.writable(CAP_EXP + 0x28, 2, 0x04ff); // DEVCTL2
        cfg.set32(CAP_EXP + 0x2c, 0x0000_0006); // LNKCAP2: 2.5 and 5 GT/s
        cfg.writable(CAP_EXP + 0x30, 2, 0x0f7f); // LNKCTL2
        cfg.set16(CAP_EXP + 0x30, 0x0002);
        // Broadcom's registers
        for o in [0x70, CFG_BAR0_WINDOW, CFG_SBMBX, 0x228, 0x248, 0x4e0, 0x4f4] {
            cfg.writable(o, 4, 0xffff_ffff);
        }
        cfg.set32(CFG_BAR0_WINDOW, CC_BASE);

        Self {
            cfg,
            wrappers: [Wrapper { ioctl: 0, reset: 1 }; 4],
            cc: vec![0; 0x1000 / 4],
            pcie2: vec![0; 0x1000 / 4],
            arm_bankidx: 0,
            erom,
            ram: vec![0; RAM_SIZE as usize],
            rom: [0; 8],
            fw: None,
            mac,
            networks: Network::defaults(),
            signalled: false,
            log: std::env::var_os("SBC_BRCM_LOG").is_some(),
        }
    }

    /// The firmware is running (the CPU out of reset and halt).
    pub fn running(&self) -> bool {
        self.fw.is_some()
    }

    /// The access points scans find (from the next firmware start).
    pub fn set_networks(&mut self, networks: Vec<Network>) {
        self.networks = networks;
    }

    /// The SSID of the network joined, if any.
    pub fn joined(&self) -> Option<&str> {
        self.fw.as_ref()?.dongle.joined()
    }

    /// The SSID of the soft access point, when the host made the card one.
    pub fn soft_ap(&self) -> Option<String> {
        self.fw.as_ref()?.dongle.ap_ssid().map(|s| String::from_utf8_lossy(s).into_owned())
    }

    /// Ethernet frames for the network (from the host's flow rings).
    pub fn take_transmitted(&mut self) -> Vec<Vec<u8>> {
        self.fw.as_mut().map(|f| std::mem::take(&mut f.dongle.to_air)).unwrap_or_default()
    }

    /// An Ethernet frame from the network for the host.
    pub fn receive(&mut self, frame: &[u8], master: &mut dyn Master) {
        if let Some(f) = self.fw.as_mut() {
            f.dongle.receive(frame);
        }
        self.flush(master);
    }

    // ------------------------------------------------------------------ backplane

    fn core_index(addr: u32) -> Option<usize> {
        CORES.iter().position(|&(_, base, _)| addr & !0xfff == base || addr & !0xfff == base + WRAP)
    }

    fn bp_read(&mut self, addr: u32) -> u32 {
        match addr {
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
                0x604 => 24,          // PMU capabilities: PMU revision 24
                0x608 => 0x0000_0007, // PMU status: ALP/HT available
                o => self.cc[o as usize / 4],
            },
            a if a & !0xfff == ARM_BASE => match a & 0xfff {
                0x04 => BANKS, // CAP: six A (TCM) banks, no B
                0x40 => self.arm_bankidx,
                0x44 if self.arm_bankidx < BANKS => BANK_SIZE / 8192 - 1,
                _ => 0,
            },
            a if a & !0xfff == PCIE2_BASE => self.pcie2_read(a & 0xfff),
            _ => 0,
        }
    }

    fn bp_write(&mut self, addr: u32, value: u32, master: &mut dyn Master) {
        match addr {
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
            a if a & !0xfff == CC_BASE => {
                self.cc[(a & 0xfff) as usize / 4] = value;
                if a & 0xfff == CC_WATCHDOG && value != 0 {
                    self.watchdog_reset();
                }
            }
            a if a & !0xfff == ARM_BASE => {
                if a & 0xfff == 0x40 {
                    self.arm_bankidx = value;
                }
            }
            a if a & !0xfff == PCIE2_BASE => self.pcie2_write(a & 0xfff, value, master),
            _ => {}
        }
    }

    fn pcie2_read(&mut self, o: u32) -> u32 {
        match o {
            PCIE2_CONFIGDATA => self.cfg.read(self.pcie2[PCIE2_CONFIGADDR as usize / 4] as usize & 0xffc),
            _ => self.pcie2[o as usize / 4],
        }
    }

    fn pcie2_write(&mut self, o: u32, value: u32, master: &mut dyn Master) {
        let i = o as usize / 4;
        match o {
            PCIE2_MAILBOXINT => self.pcie2[i] &= !value,
            PCIE2_CONFIGDATA => {
                let at = self.pcie2[PCIE2_CONFIGADDR as usize / 4] as usize & 0xffc;
                self.cfg.write(at, value, 4);
            }
            PCIE2_H2D_MAILBOX_0 => {
                // the host rang the doorbell: its rings have new items
                self.service(master);
                return;
            }
            _ => self.pcie2[i] = value,
        }
        if matches!(o, PCIE2_MAILBOXINT | PCIE2_MAILBOXMASK | PCIE2_INTMASK) {
            self.signal(master);
        }
    }

    /// The chipcommon watchdog resets the chip: the cores go back to reset and the
    /// firmware stops (the PCIe core and its configuration stay).
    fn watchdog_reset(&mut self) {
        if self.log && self.fw.is_some() {
            eprintln!("[brcm] watchdog reset: firmware stopped");
        }
        self.wrappers = [Wrapper { ioctl: 0, reset: 1 }; 4];
        self.fw = None;
        self.pcie2[PCIE2_MAILBOXINT as usize / 4] = 0;
        self.signalled = false;
    }

    /// The Cortex-R4 runs when its wrapper has it out of reset, clocked and not halted:
    /// the firmware starts (or stops when the driver halts it again).
    fn arm_state(&mut self) {
        let k = CORES.iter().position(|c| c.0 == CORE_ARM_CR4).unwrap();
        let w = self.wrappers[k];
        let run = w.reset == 0 && w.ioctl & IOCTL_CLK != 0 && w.ioctl & IOCTL_CPUHALT == 0;
        match (run, self.fw.is_some()) {
            (true, false) => self.boot(),
            (false, true) => self.fw = None,
            _ => {}
        }
    }

    /// The firmware's start-up: the msgbuf shared structure, its ring information and
    /// console, then its address in the last word of RAM, which the driver polls.
    fn boot(&mut self) {
        if self.log {
            eprintln!("[brcm] firmware started (reset vector {:#x})", u32::from_le_bytes(self.rom[..4].try_into().unwrap()));
        }
        for a in (SHARED..CONSOLE_BUF + CONSOLE_SIZE).step_by(4) {
            self.set_tcm32(a, 0);
        }
        self.set_tcm32(SHARED, SHARED_VERSION);
        self.set_tcm32(SHARED + 20, CONSOLE);
        self.set_tcm16(SHARED + 34, MAX_RXBUFPOST);
        self.set_tcm32(SHARED + 36, 0); // rx data offset
        self.set_tcm32(SHARED + 40, HTOD_MB);
        self.set_tcm32(SHARED + 44, DTOH_MB);
        self.set_tcm32(SHARED + 48, RING_INFO);
        self.set_tcm32(RING_INFO, RING_MEM);
        self.set_tcm32(RING_INFO + 4, H2D_W);
        self.set_tcm32(RING_INFO + 8, H2D_R);
        self.set_tcm32(RING_INFO + 12, D2H_W);
        self.set_tcm32(RING_INFO + 16, D2H_R);
        // version 5: max_flowrings counts every submission ring
        self.set_tcm16(RING_INFO + 52, H2D_RINGS as u16);
        let banner = b"sbc-emu: BCM4350 dongle up (msgbuf, emulated in place of the firmware)\n";
        for (k, b) in banner.iter().enumerate() {
            self.ram[(CONSOLE_BUF - RAM_BASE) as usize + k] = *b;
        }
        self.set_tcm32(CONSOLE + 8, CONSOLE_BUF);
        self.set_tcm32(CONSOLE + 12, CONSOLE_SIZE);
        self.set_tcm32(CONSOLE + 16, banner.len() as u32);
        self.set_tcm32(RAM_BASE + RAM_SIZE - 4, SHARED);
        let dongle = Dongle::new(self.mac, self.networks.clone(), self.log).with_identity(IDENTITY);
        self.fw = Some(Firmware {
            dongle,
            flowrings: HashMap::new(),
            ioctl_bufs: VecDeque::new(),
            event_bufs: VecDeque::new(),
            rx_bufs: VecDeque::new(),
            answers: VecDeque::new(),
            events: VecDeque::new(),
            rx: VecDeque::new(),
            completions: Default::default(),
            event_seq: 0,
            scan: None,
        });
    }

    // ------------------------------------------------------------------ TCM

    fn tcm_byte(&self, a: u32) -> u8 {
        if (RAM_BASE..RAM_BASE + RAM_SIZE).contains(&a) {
            self.ram[(a - RAM_BASE) as usize]
        } else {
            self.rom.get(a as usize).copied().unwrap_or(0)
        }
    }

    fn set_tcm_byte(&mut self, a: u32, v: u8) {
        if (RAM_BASE..RAM_BASE + RAM_SIZE).contains(&a) {
            self.ram[(a - RAM_BASE) as usize] = v;
        } else if let Some(b) = self.rom.get_mut(a as usize) {
            *b = v;
        }
    }

    fn tcm16(&self, a: u32) -> u16 {
        u16::from_le_bytes([self.tcm_byte(a), self.tcm_byte(a + 1)])
    }

    fn tcm32(&self, a: u32) -> u32 {
        u32::from_le_bytes([0, 1, 2, 3].map(|k| self.tcm_byte(a + k)))
    }

    fn tcm64(&self, a: u32) -> u64 {
        self.tcm32(a) as u64 | (self.tcm32(a + 4) as u64) << 32
    }

    fn set_tcm16(&mut self, a: u32, v: u16) {
        for (k, b) in v.to_le_bytes().into_iter().enumerate() {
            self.set_tcm_byte(a + k as u32, b);
        }
    }

    fn set_tcm32(&mut self, a: u32, v: u32) {
        for (k, b) in v.to_le_bytes().into_iter().enumerate() {
            self.set_tcm_byte(a + k as u32, b);
        }
    }

    // ------------------------------------------------------------------ interrupts

    /// The mailbox interrupt as an MSI: one message when it becomes pending and unmasked
    /// (brcmfmac masks it in its handler and unmasks it after, which sends the next).
    fn signal(&mut self, master: &mut dyn Master) {
        let pending = self.pcie2[PCIE2_MAILBOXINT as usize / 4] & self.pcie2[PCIE2_MAILBOXMASK as usize / 4] != 0;
        if !pending {
            self.signalled = false;
            return;
        }
        if self.signalled {
            return;
        }
        self.signalled = true;
        if self.cfg.command() & pci::CMD_MASTER == 0 {
            return;
        }
        match pci::msi_target(&self.cfg, CAP_MSI) {
            Some((addr, data)) => {
                master.write(addr, &data.to_le_bytes());
            }
            None if self.log => eprintln!("[brcm] interrupt pending but MSI disabled"),
            None => {}
        }
    }

    /// The legacy INTA line (when the host did not enable MSI).
    pub fn intx(&self) -> bool {
        pci::msi_target(&self.cfg, CAP_MSI).is_none()
            && self.pcie2[PCIE2_MAILBOXINT as usize / 4] & self.pcie2[PCIE2_MAILBOXMASK as usize / 4] != 0
    }

    fn raise(&mut self, bits: u32, master: &mut dyn Master) {
        self.pcie2[PCIE2_MAILBOXINT as usize / 4] |= bits;
        self.signal(master);
    }

    // ------------------------------------------------------------------ msgbuf

    /// A common ring (0..5, in `brcmf_pcie_init_ringbuffers` order): its host address, item
    /// count and size, as the host wrote them into the ring memory.
    fn common_ring(&self, k: u32) -> Option<(u64, u16, u16)> {
        let d = RING_MEM + 16 * k;
        let (items, len, addr) = (self.tcm16(d + 4), self.tcm16(d + 6), self.tcm64(d + 8));
        (items != 0 && len != 0 && addr != 0).then_some((addr, items, len))
    }

    /// A submission ring (the two common ones, then the flow rings by ID).
    fn ring(&self, id: u32) -> Option<(u64, u16, u16)> {
        if id < 2 {
            return self.common_ring(id);
        }
        let f = self.fw.as_ref()?.flowrings.get(&(id as u16))?;
        Some((f.addr, f.items, f.item_len))
    }

    /// The host rang the doorbell: run what its submission rings hold.
    fn service(&mut self, master: &mut dyn Master) {
        if self.fw.is_none() {
            return;
        }
        let mut ids = vec![CONTROL_SUBMIT, RXPOST_SUBMIT];
        let mut flows: Vec<u32> = self.fw.as_ref().unwrap().flowrings.keys().map(|&k| k as u32).collect();
        flows.sort_unstable();
        ids.extend(flows);
        for id in ids {
            let Some((addr, items, len)) = self.ring(id) else { continue };
            let w = self.tcm16(H2D_W + 4 * id) % items;
            let mut r = self.tcm16(H2D_R + 4 * id) % items;
            while r != w {
                let mut item = vec![0u8; len as usize];
                master.read(addr + r as u64 * len as u64, &mut item);
                self.message(id, &item, master);
                r = (r + 1) % items;
                // a flow ring deleted by its own message
                if self.ring(id).is_none() {
                    break;
                }
            }
            self.set_tcm16(H2D_R + 4 * id, r);
        }
        self.flush(master);
    }

    fn message(&mut self, ring: u32, m: &[u8], master: &mut dyn Master) {
        if m.len() < 8 {
            return;
        }
        let u16_at = |o: usize| u16::from_le_bytes([m[o], m[o + 1]]);
        let u32_at = |o: usize| u32::from_le_bytes(m[o..o + 4].try_into().unwrap());
        let u64_at = |o: usize| u32_at(o) as u64 | (u32_at(o + 4) as u64) << 32;
        let (kind, ifidx, request) = (m[0], m[1], u32_at(4));
        let log = self.log;
        let Some(fw) = self.fw.as_mut() else { return };
        match kind {
            MSG_IOCTLPTR_REQ if m.len() >= 32 => {
                let (cmd, trans_id, inlen, outlen) = (u32_at(8), u16_at(12), u16_at(14) as usize, u16_at(16) as usize);
                let mut input = vec![0u8; inlen];
                master.read(u64_at(24), &mut input);
                let (status, data) = fw.dongle.ioctl_request(cmd, &input, outlen);
                fw.answers.push_back(IoctlAnswer { cmd, trans_id, status, data });
            }
            MSG_IOCTLRESP_BUF_POST | MSG_EVENT_BUF_POST if m.len() >= 24 => {
                let buf = HostBuf { id: request, addr: u64_at(16), len: u16_at(8) as usize };
                if kind == MSG_IOCTLRESP_BUF_POST { fw.ioctl_bufs.push_back(buf) } else { fw.event_bufs.push_back(buf) }
            }
            MSG_RXBUF_POST if m.len() >= 32 => {
                fw.rx_bufs.push_back(HostBuf { id: request, addr: u64_at(24), len: u16_at(10) as usize });
            }
            MSG_FLOW_RING_CREATE if m.len() >= 40 => {
                let id = u16_at(22);
                let ring = FlowRing { addr: u64_at(32), items: u16_at(28), item_len: u16_at(30), ifidx };
                if log {
                    eprintln!("[brcm] flow ring {id}: {} items of {} bytes", ring.items, ring.item_len);
                }
                let ok = (2..H2D_RINGS as u16).contains(&id) && ring.items != 0 && ring.item_len >= 44;
                if ok {
                    fw.flowrings.insert(id, ring);
                }
                fw.completions[0].push_back(completion(MSG_FLOW_RING_CREATE_CMPLT, ifidx, request, if ok { 0 } else { 1 }, id, 24));
            }
            MSG_FLOW_RING_DELETE | MSG_FLOW_RING_FLUSH if m.len() >= 12 => {
                let id = u16_at(8);
                let reply = if kind == MSG_FLOW_RING_DELETE {
                    fw.flowrings.remove(&id);
                    MSG_FLOW_RING_DELETE_CMPLT
                } else {
                    MSG_FLOW_RING_FLUSH_CMPLT
                };
                fw.completions[0].push_back(completion(reply, ifidx, request, 0, id, 24));
            }
            MSG_TX_POST if m.len() >= 44 => {
                let len = u16_at(42) as usize;
                let mut eth = m[8..22].to_vec();
                let at = eth.len();
                eth.resize(at + len, 0);
                master.read(u64_at(32), &mut eth[at..]);
                fw.dongle.host_data(&eth);
                let ifidx = fw.flowrings.get(&(ring as u16)).map_or(ifidx, |f| f.ifidx);
                fw.completions[(TX_COMPLETE - CONTROL_COMPLETE) as usize].push_back(completion(MSG_TX_STATUS, ifidx, request, 0, ring as u16, 16));
            }
            k if log => eprintln!("[brcm] ring {ring}: message type {k:#x} ignored"),
            _ => {}
        }
    }

    /// Move answers, events and received frames into the host's buffers and the completion
    /// rings; ring the host's doorbell when anything went in.
    fn flush(&mut self, master: &mut dyn Master) {
        let log = self.log;
        let Some(fw) = self.fw.as_mut() else { return };
        let now = master.now();
        for m in fw.dongle.take_messages() {
            match m {
                ToHost::Event(e) if event_code(&e) == E_ESCAN_RESULT => {
                    fw.scan.get_or_insert_with(|| (now + SCAN_NS, Vec::new())).1.push(e);
                }
                ToHost::Event(e) => fw.events.push_back(e),
                ToHost::Data(d) if fw.rx.len() < MAX_PENDING_RX => fw.rx.push_back(d),
                ToHost::Data(_) => {}
            }
        }
        while !fw.answers.is_empty() && !fw.ioctl_bufs.is_empty() {
            let a = fw.answers.pop_front().unwrap();
            let b = fw.ioctl_bufs.pop_front().unwrap();
            let n = a.data.len().min(b.len);
            master.write(b.addr, &a.data[..n]);
            let mut c = completion(MSG_IOCTL_CMPLT, 0, b.id, a.status as i16 as u16, 0, 24);
            c[12..14].copy_from_slice(&(n as u16).to_le_bytes());
            c[14..16].copy_from_slice(&a.trans_id.to_le_bytes());
            c[16..20].copy_from_slice(&a.cmd.to_le_bytes());
            fw.completions[0].push_back(c);
        }
        while !fw.events.is_empty() && !fw.event_bufs.is_empty() {
            let e = fw.events.pop_front().unwrap();
            let b = fw.event_bufs.pop_front().unwrap();
            if log {
                let code = event_code(&e);
                let status = e.get(32..36).map_or(0, |c| u32::from_be_bytes(c.try_into().unwrap()));
                eprintln!("[brcm] event {code} status {status} to buffer {}", b.id);
            }
            let n = e.len().min(b.len);
            master.write(b.addr, &e[..n]);
            let mut c = completion(MSG_WL_EVENT, 0, b.id, 0, 0, 24);
            c[12..14].copy_from_slice(&(n as u16).to_le_bytes());
            c[14..16].copy_from_slice(&fw.event_seq.to_le_bytes());
            fw.event_seq = fw.event_seq.wrapping_add(1);
            fw.completions[0].push_back(c);
        }
        while !fw.rx.is_empty() && !fw.rx_bufs.is_empty() {
            let d = fw.rx.pop_front().unwrap();
            let b = fw.rx_bufs.pop_front().unwrap();
            let n = d.len().min(b.len);
            master.write(b.addr, &d[..n]);
            let mut c = completion(MSG_RX_CMPLT, 0, b.id, 0, RX_COMPLETE as u16, 32);
            c[14..16].copy_from_slice(&(n as u16).to_le_bytes());
            c[18..20].copy_from_slice(&PKT_FLAGS_FRAME_802_3.to_le_bytes());
            fw.completions[(RX_COMPLETE - CONTROL_COMPLETE) as usize].push_back(c);
        }
        let mut wrote = false;
        for d in 0..D2H_RINGS {
            let Some((addr, items, len)) = self.common_ring(CONTROL_COMPLETE + d) else { continue };
            let fw = self.fw.as_mut().unwrap();
            if fw.completions[d as usize].is_empty() {
                continue;
            }
            let r = self.tcm16(D2H_R + 4 * d) % items;
            let mut w = self.tcm16(D2H_W + 4 * d) % items;
            let fw = self.fw.as_mut().unwrap();
            while (w + 1) % items != r {
                let Some(mut c) = fw.completions[d as usize].pop_front() else { break };
                c.resize(len as usize, 0);
                master.write(addr + w as u64 * len as u64, &c);
                w = (w + 1) % items;
                wrote = true;
            }
            self.set_tcm16(D2H_W + 4 * d, w);
        }
        if wrote {
            self.raise(MB_INT_D2H0_DB0, master);
        }
    }

    /// Mailbox data from the host (written to TCM, announced through `SBMBX`).
    fn host_mailbox(&mut self, master: &mut dyn Master) {
        if self.fw.is_none() {
            return;
        }
        let data = self.tcm32(HTOD_MB);
        self.set_tcm32(HTOD_MB, 0);
        if data & H2D_HOST_D3_INFORM != 0 {
            self.set_tcm32(DTOH_MB, D2H_DEV_D3_ACK);
            self.raise(MB_INT_FN0_0, master);
        }
    }

    // ------------------------------------------------------------------ BARs

    fn bar0_read32(&mut self, o: u32) -> u32 {
        let window = self.cfg.get32(CFG_BAR0_WINDOW) & !0xfff;
        match o {
            0..=0xfff => self.bp_read(window | o),
            0x1000..=0x1fff => self.bp_read((window + WRAP) | (o & 0xfff)),
            0x2000..=0x2fff => self.pcie2_read(o & 0xfff),
            _ => 0,
        }
    }

    fn bar0_write32(&mut self, o: u32, v: u32, master: &mut dyn Master) {
        let window = self.cfg.get32(CFG_BAR0_WINDOW) & !0xfff;
        match o {
            0..=0xfff => self.bp_write(window | o, v, master),
            0x1000..=0x1fff => self.bp_write((window + WRAP) | (o & 0xfff), v, master),
            0x2000..=0x2fff => self.pcie2_write(o & 0xfff, v, master),
            _ => {}
        }
    }
}

/// The event code of a firmware event frame (Ethernet, Broadcom header, event message).
fn event_code(e: &[u8]) -> u32 {
    e.get(28..32).map_or(0, |c| u32::from_be_bytes(c.try_into().unwrap()))
}

/// A completion item: header, completion header (status, ring), zeros after.
fn completion(kind: u8, ifidx: u8, request: u32, status: u16, ring: u16, len: usize) -> Vec<u8> {
    let mut c = vec![0u8; len];
    c[0] = kind;
    c[1] = ifidx;
    c[4..8].copy_from_slice(&request.to_le_bytes());
    c[8..10].copy_from_slice(&status.to_le_bytes());
    c[10..12].copy_from_slice(&ring.to_le_bytes());
    c
}

impl Function for Bcm4350 {
    fn config(&mut self) -> &mut Config {
        &mut self.cfg
    }

    fn config_written(&mut self, offset: usize, master: &mut dyn Master) {
        if offset & !3 == CFG_SBMBX {
            self.host_mailbox(master);
        }
        // MSI or bus mastering switched on with an interrupt pending
        if offset < 0x08 || (CAP_MSI..CAP_MSI + 0x10).contains(&offset) {
            self.signalled = false;
            self.signal(master);
        }
    }

    fn bar_size(&self, bar: usize) -> u64 {
        match bar {
            0 => BAR0_SIZE,
            2 => BAR2_SIZE,
            _ => 0,
        }
    }

    fn bar_read(&mut self, bar: usize, offset: u64, len: usize, _master: &mut dyn Master) -> u64 {
        let o = offset as u32;
        match bar {
            0 => {
                let word = self.bar0_read32(o & !3) as u64 | if len == 8 { (self.bar0_read32((o & !3) + 4) as u64) << 32 } else { 0 };
                let shift = 8 * (o & 3);
                let mask = if len >= 8 { u64::MAX } else { (1u64 << (8 * len)) - 1 };
                (word >> shift) & mask
            }
            2 => (0..len).fold(0u64, |v, k| v | (self.tcm_byte(o + k as u32) as u64) << (8 * k)),
            _ => 0,
        }
    }

    fn bar_write(&mut self, bar: usize, offset: u64, value: u64, len: usize, master: &mut dyn Master) {
        let o = offset as u32;
        match bar {
            0 if len >= 4 => {
                self.bar0_write32(o, value as u32, master);
                if len == 8 {
                    self.bar0_write32(o + 4, (value >> 32) as u32, master);
                }
            }
            0 => {
                let shift = 8 * (o & 3);
                let mask = ((1u64 << (8 * len)) - 1) as u32;
                let old = self.bar0_read32(o & !3);
                self.bar0_write32(o & !3, old & !(mask << shift) | ((value as u32) & mask) << shift, master);
            }
            2 => {
                for k in 0..len {
                    self.set_tcm_byte(o + k as u32, (value >> (8 * k)) as u8);
                }
            }
            _ => {}
        }
    }

    /// A scan's results, once it has run its time.
    fn tick(&mut self, now: u64, master: &mut dyn Master) {
        let Some(fw) = self.fw.as_mut() else { return };
        if fw.scan.as_ref().is_some_and(|(due, _)| *due <= now) {
            let (_, events) = fw.scan.take().unwrap();
            fw.events.extend(events);
            self.flush(master);
        }
    }

    fn next_event(&self) -> Option<u64> {
        self.fw.as_ref()?.scan.as_ref().map(|(due, _)| *due)
    }

    fn as_any(&mut self) -> &mut dyn std::any::Any {
        self
    }
}

#[cfg(test)]
mod tests;
