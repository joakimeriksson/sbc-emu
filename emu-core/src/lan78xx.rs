//! A Microchip LAN7800 USB gigabit Ethernet controller, the Ethernet half of the LAN7515 on a
//! Raspberry Pi 3 Model B+, for Linux's `lan78xx` driver.
//!
//! - Registers through vendor control requests (0xa0 write, 0xa1 read, 0xa2 statistics), with
//!   the self-clearing reset and busy bits and the enable/disabled status pairs the driver
//!   waits on; no EEPROM and an empty OTP, so the MAC address comes from the device tree.
//! - The internal gigabit PHY (a LAN88xx at MDIO address 1) through MII_ACC/MII_DATA: IEEE
//!   registers, paged and MMD registers; the cable is plugged in and auto-negotiation ends at
//!   once with a 1000BASE-T full-duplex partner. The PHY driver polls the link.
//! - Bulk IN (endpoint 1): received frames packed into one transfer, each behind its RX
//!   command words (A, B, C) and with its FCS, padded to the next 4-byte boundary.
//! - Bulk OUT (endpoint 2): frames behind TX command words A and B, 4-byte aligned; the
//!   checksum offload (IPE/TPE), segmentation (LSO with the MSS in word B) and VLAN insertion
//!   the words ask for are done here, so the network sees ordinary frames.
//! - Interrupt IN (endpoint 3): INT_STS where INT_EP_CTL enables it (the PHY interrupt).
//!
//! Frames go out through `tx_out` and come in with `receive`; the machine connects them to
//! `sbc-net`.
use crate::usb::{Speed, Std, UsbDevice};
use std::collections::{BTreeMap, VecDeque};

pub const ID_REV: u32 = 0x000;
pub const INT_STS: u32 = 0x00c;
pub const HW_CFG: u32 = 0x010;
pub const PMT_CTL: u32 = 0x014;
pub const DP_SEL: u32 = 0x024;
pub const E2P_CMD: u32 = 0x040;
pub const E2P_DATA: u32 = 0x044;
pub const INT_EP_CTL: u32 = 0x098;
pub const FCT_RX_CTL: u32 = 0x0c0;
pub const FCT_TX_CTL: u32 = 0x0c4;
pub const MAC_CR: u32 = 0x100;
pub const MAC_RX: u32 = 0x104;
pub const MAC_TX: u32 = 0x108;
pub const RX_ADDRH: u32 = 0x118;
pub const RX_ADDRL: u32 = 0x11c;
pub const MII_ACC: u32 = 0x120;
pub const MII_DATA: u32 = 0x124;
pub const OTP_PWR_DN: u32 = 0x1000;
pub const OTP_CMD_GO: u32 = 0x1028;
pub const OTP_STATUS: u32 = 0x1030;
const OTP_RD_DATA: u32 = 0x1018;
const NREGS: usize = 0x1040 / 4;

pub const INT_STS_PHY_INT: u32 = 1 << 17;
const HW_CFG_LRST: u32 = 1 << 1;
const HW_CFG_SRST: u32 = 1 << 0;
const PMT_CTL_READY: u32 = 1 << 7;
const PMT_CTL_PHY_RST: u32 = 1 << 4;
const E2P_CMD_BUSY: u32 = 1 << 31;
const DP_SEL_DPRDY: u32 = 1 << 31;
const MAC_CR_RST: u32 = 1 << 0;
const MAC_RXTX_EN: u32 = 1 << 0;
const MAC_RXTX_DIS: u32 = 1 << 1;
const FCT_EN: u32 = 1 << 31;
const FCT_RST: u32 = 1 << 30;
const FCT_DIS: u32 = 1 << 20;
const MII_BUSY: u32 = 1 << 0;
const MII_WRITE: u32 = 1 << 1;

pub const TX_CMD_A_LSO: u32 = 0x0800_0000;
pub const TX_CMD_A_IPE: u32 = 0x0400_0000;
pub const TX_CMD_A_TPE: u32 = 0x0200_0000;
pub const TX_CMD_A_IVTG: u32 = 0x0100_0000;
pub const TX_CMD_A_FCS: u32 = 0x0040_0000;
pub const TX_CMD_A_LEN: u32 = 0x000f_ffff;
pub const RX_CMD_A_BAM: u32 = 0x0200_0000;
pub const RX_CMD_A_MAM: u32 = 0x0100_0000;
/// ignore the checksum word: the driver checks the frame's checksums itself
pub const RX_CMD_A_ICSM: u32 = 0x0000_4000;
pub const RX_CMD_A_LEN: u32 = 0x0000_3fff;

/// The largest bulk IN transfer the device builds (BURST_CAP is in 512-byte units).
const BURST_MAX: usize = 16 * 1024;

/// The internal PHY: a LAN88xx (ID 0007:c132) with a 1000BASE-T full-duplex partner.
#[derive(Clone)]
pub struct Phy {
    regs: [u16; 32],
    /// registers 16..30 of the extended pages (register 31 selects the page)
    paged: BTreeMap<(u16, u8), u16>,
    /// MMD registers (device, register), reached through registers 13 and 14
    mmd: BTreeMap<(u8, u16), u16>,
    mmd_addr: u16,
    /// the cable is plugged in
    pub cable: bool,
}

const BMCR: usize = 0;
const BMSR: usize = 1;
const BMCR_RESET: u16 = 1 << 15;
const BMCR_PDOWN: u16 = 1 << 11;
const BMCR_ANENABLE: u16 = 1 << 12;
const BMCR_ANRESTART: u16 = 1 << 9;
const BMSR_LINK: u16 = 1 << 2;
const BMSR_ANEGDONE: u16 = 1 << 5;
const INT_MASK: usize = 25;
const INT_STAT: usize = 26;
const INT_LINK_CHANGE: u16 = 1 << 13;

impl Default for Phy { fn default() -> Self { Self::new() } }

impl Phy {
    pub fn new() -> Self {
        let mut p = Phy { regs: [0; 32], paged: BTreeMap::new(), mmd: BTreeMap::new(), mmd_addr: 0, cable: true };
        p.reset();
        p
    }

    fn reset(&mut self) {
        self.regs = [0; 32];
        self.regs[BMCR] = 0x1140; // auto-negotiation on, full duplex, 1000 Mb/s
        self.regs[2] = 0x0007;
        self.regs[3] = 0xc132;
        self.regs[4] = 0x01e1; // 10/100, half and full duplex
        self.regs[9] = 0x0300; // 1000BASE-T, half and full duplex
        self.paged.clear();
        self.mmd_addr = 0;
    }

    /// The link is up: powered, the cable in, negotiation finished (it finishes at once).
    pub fn link(&self) -> bool { self.cable && self.regs[BMCR] & BMCR_PDOWN == 0 }

    pub fn read(&mut self, r: usize) -> u16 {
        let page = self.regs[31];
        if page != 0 && (16..31).contains(&r) { return self.paged.get(&(page, r as u8)).copied().unwrap_or(0); }
        let link = self.link();
        match r {
            // 100BASE-TX/10BASE-T both duplexes, extended status, preamble suppression, autoneg
            BMSR => 0x796d & !(BMSR_LINK | BMSR_ANEGDONE) | if link { BMSR_LINK | BMSR_ANEGDONE } else { 0 },
            5 => if link { 0x45e1 } else { 0 },  // partner: ack, pause, 10/100 both duplexes
            6 => link as u16,                    // partner can negotiate
            10 => if link { 0x3800 } else { 0 }, // receivers OK, partner 1000BASE-T full duplex
            15 => 0x3000,                        // 1000BASE-T full and half duplex
            14 => {
                let dev = (self.regs[13] & 0x1f) as u8;
                if self.regs[13] >> 14 == 0 { self.mmd_addr } else { self.mmd.get(&(dev, self.mmd_addr)).copied().unwrap_or(0) }
            }
            INT_STAT => std::mem::take(&mut self.regs[INT_STAT]), // read to clear
            _ => self.regs[r & 31],
        }
    }

    pub fn write(&mut self, r: usize, v: u16) {
        let page = self.regs[31];
        if page != 0 && (16..31).contains(&r) { self.paged.insert((page, r as u8), v); return; }
        match r {
            BMCR => {
                if v & BMCR_RESET != 0 { self.reset(); return; }
                let was = self.link();
                self.regs[BMCR] = v & !BMCR_ANRESTART;
                if was != self.link() { self.link_changed(); }
            }
            BMSR | 2 | 3 | 5 | 6 | 10 | 15 => {}
            14 => {
                let dev = (self.regs[13] & 0x1f) as u8;
                if self.regs[13] >> 14 == 0 { self.mmd_addr = v; } else { self.mmd.insert((dev, self.mmd_addr), v); }
            }
            _ => self.regs[r & 31] = v,
        }
    }

    fn link_changed(&mut self) {
        self.regs[INT_STAT] |= INT_LINK_CHANGE | if self.regs[BMCR] & BMCR_ANENABLE != 0 { 1 << 10 } else { 0 };
    }

    /// The PHY's interrupt output (its status masked).
    fn irq(&self) -> bool { self.regs[INT_STAT] & self.regs[INT_MASK] & 0x7fff != 0 }

    /// Plug the cable in or pull it out.
    pub fn set_cable(&mut self, on: bool) {
        if on != self.cable { self.cable = on; self.link_changed(); }
    }
}

pub struct Lan78xx {
    pub st: Std,
    regs: Vec<u32>,
    pub phy: Phy,
    /// the register a vendor write's data stage goes to
    pending_write: Option<u32>,
    /// the interrupt status last sent on endpoint 3 (sent again only when it changes)
    int_sent: u32,
    /// frames from the guest for the network
    pub tx_out: Vec<Vec<u8>>,
    /// frames for the guest, waiting for a bulk IN transfer
    pub rx_queue: VecDeque<Vec<u8>>,
    pub tx_frames: u64,
    pub rx_frames: u64,
    pub rx_dropped: u64,
}

impl Default for Lan78xx { fn default() -> Self { Self::new() } }

impl Lan78xx {
    pub fn new() -> Self {
        let mut l = Lan78xx { st: Std::default(), regs: vec![0; NREGS], phy: Phy::new(), pending_write: None, int_sent: 0,
                              tx_out: Vec::new(), rx_queue: VecDeque::new(), tx_frames: 0, rx_frames: 0, rx_dropped: 0 };
        l.power_on();
        l
    }

    /// Register values at power-on (no EEPROM: the address registers are all ones, which the
    /// driver takes as "no address": it uses the device tree's).
    fn power_on(&mut self) {
        self.regs.iter_mut().for_each(|r| *r = 0);
        self.set(ID_REV, 0x7800_0002);
        self.set(PMT_CTL, PMT_CTL_READY);
        self.set(MAC_RX, 0x05ee_0000 | MAC_RXTX_DIS);
        self.set(MAC_TX, MAC_RXTX_DIS);
        self.set(FCT_RX_CTL, FCT_DIS);
        self.set(FCT_TX_CTL, FCT_DIS);
        self.set(RX_ADDRL, 0xffff_ffff);
        self.set(RX_ADDRH, 0x0000_ffff);
        self.set(OTP_PWR_DN, 1);
        self.set(DP_SEL, DP_SEL_DPRDY);
    }

    fn set(&mut self, r: u32, v: u32) { if let Some(x) = self.regs.get_mut(r as usize / 4) { *x = v; } }
    fn get(&self, r: u32) -> u32 { self.regs.get(r as usize / 4).copied().unwrap_or(0) }

    pub fn rx_enabled(&self) -> bool { self.get(MAC_RX) & MAC_RXTX_EN != 0 && self.get(FCT_RX_CTL) & FCT_EN != 0 }
    pub fn tx_enabled(&self) -> bool { self.get(MAC_TX) & MAC_RXTX_EN != 0 && self.get(FCT_TX_CTL) & FCT_EN != 0 }

    /// The MAC address the driver programmed (RX_ADDRL/H).
    pub fn mac(&self) -> [u8; 6] {
        let (l, h) = (self.get(RX_ADDRL), self.get(RX_ADDRH));
        [l as u8, (l >> 8) as u8, (l >> 16) as u8, (l >> 24) as u8, h as u8, (h >> 8) as u8]
    }

    pub fn read_reg(&mut self, r: u32) -> u32 {
        let v = self.get(r);
        match r {
            // a status bit that follows its enable bit: disabled once the enable is off
            MAC_RX | MAC_TX => (v & !MAC_RXTX_DIS) | if v & MAC_RXTX_EN == 0 { MAC_RXTX_DIS } else { 0 },
            FCT_RX_CTL | FCT_TX_CTL => (v & !FCT_DIS) | if v & FCT_EN == 0 { FCT_DIS } else { 0 } | (1 << 21), // FIFO empty
            INT_STS => v | if self.phy.irq() { INT_STS_PHY_INT } else { 0 },
            _ => v,
        }
    }

    pub fn write_reg(&mut self, r: u32, v: u32) {
        match r {
            ID_REV | OTP_STATUS => {}
            INT_STS => { let old = self.get(INT_STS); self.set(INT_STS, old & !v); }
            HW_CFG => self.set(HW_CFG, v & !(HW_CFG_LRST | HW_CFG_SRST)),
            PMT_CTL => {
                if v & PMT_CTL_PHY_RST != 0 { self.phy.reset(); }
                let wake = self.get(PMT_CTL) & 3 & !v; // wake-up status bits: write 1 to clear
                self.set(PMT_CTL, (v & !(PMT_CTL_PHY_RST | 3)) | PMT_CTL_READY | wake);
            }
            // no EEPROM: a read finds 0xff (no 0xa5 signature)
            E2P_CMD => { self.set(E2P_CMD, v & !E2P_CMD_BUSY); self.set(E2P_DATA, 0xff); }
            // the OTP is blank: reads find 0 (no 0xf3/0xf7 signature)
            OTP_CMD_GO => self.set(OTP_RD_DATA, 0),
            DP_SEL => self.set(DP_SEL, v | DP_SEL_DPRDY),
            MAC_CR => self.set(MAC_CR, v & !MAC_CR_RST),
            FCT_RX_CTL | FCT_TX_CTL => {
                if r == FCT_RX_CTL && v & FCT_RST != 0 { self.rx_queue.clear(); }
                self.set(r, v & !FCT_RST);
            }
            MII_ACC => {
                if v & MII_BUSY != 0 {
                    let (phy, reg) = ((v >> 11) & 0x1f, ((v >> 6) & 0x1f) as usize);
                    if v & MII_WRITE != 0 {
                        if phy == 1 { let d = self.get(MII_DATA) as u16; self.phy.write(reg, d); }
                    } else {
                        let d = if phy == 1 { self.phy.read(reg) } else { 0xffff };
                        self.set(MII_DATA, d as u32);
                    }
                }
                self.set(MII_ACC, v & !MII_BUSY);
            }
            _ => self.set(r, v),
        }
    }

    /// A frame from the network; dropped while the receiver is off or the queue is full.
    pub fn receive(&mut self, frame: &[u8]) {
        if !self.rx_enabled() || !self.phy.link() || self.rx_queue.len() >= 512 || frame.len() < 14 {
            self.rx_dropped += 1;
            return;
        }
        self.rx_queue.push_back(frame.to_vec());
    }

    /// Received frames packed for a bulk IN transfer of at most `max` bytes.
    fn rx_transfer(&mut self, max: usize) -> Option<Vec<u8>> {
        let max = max.min(BURST_MAX);
        let mut out = Vec::new();
        while let Some(f) = self.rx_queue.front() {
            let size = f.len() + 4; // with the FCS
            let pad = (4 - (size + 2) % 4) % 4;
            if out.len() + 10 + size > max { break; }
            let f = self.rx_queue.pop_front().unwrap();
            let mut a = (size as u32 & RX_CMD_A_LEN) | RX_CMD_A_ICSM;
            if f[0] & 1 != 0 { a |= if f[..6] == [0xff; 6] { RX_CMD_A_BAM } else { RX_CMD_A_MAM }; }
            out.extend_from_slice(&a.to_le_bytes());
            out.extend_from_slice(&0u32.to_le_bytes());
            out.extend_from_slice(&0u16.to_le_bytes());
            out.extend_from_slice(&f);
            out.extend_from_slice(&crc32(&f).to_le_bytes());
            self.rx_frames += 1;
            if out.len() + pad <= max { out.resize(out.len() + pad, 0); } else { break; }
        }
        (!out.is_empty()).then_some(out)
    }

    /// A bulk OUT transfer: frames behind their TX command words.
    fn tx_transfer(&mut self, data: &[u8]) {
        let mut off = 0;
        while off + 8 <= data.len() {
            let a = u32::from_le_bytes(data[off..off + 4].try_into().unwrap());
            let b = u32::from_le_bytes(data[off + 4..off + 8].try_into().unwrap());
            let len = (a & TX_CMD_A_LEN) as usize;
            let Some(frame) = data.get(off + 8..off + 8 + len) else { break };
            if self.tx_enabled() && self.phy.link() {
                for f in tx_offload(frame, a, b) { self.tx_frames += 1; self.tx_out.push(f); }
            }
            off += 8 + len;
            off += (4 - off % 4) % 4;
        }
    }

    /// The interrupt endpoint's word: INT_STS bits that INT_EP_CTL lets through.
    fn int_word(&mut self) -> u32 {
        let en = self.get(INT_EP_CTL) & 0x07ff_ffff;
        self.read_reg(INT_STS) & en
    }
}

impl UsbDevice for Lan78xx {
    fn std(&mut self) -> &mut Std { &mut self.st }
    fn device_desc(&self) -> Vec<u8> {
        // vendor-specific class, Microchip's IDs so that `lan78xx` binds
        vec![18, 1, 0x00, 0x02, 0xff, 0x00, 0xff, 64, 0x24, 0x04, 0x00, 0x78, 0x00, 0x03, 1, 2, 0, 1]
    }
    fn config_desc(&self) -> Vec<u8> {
        vec![
            9, 2, 39, 0, 1, 1, 0, 0xa0, 50,       // bus powered, remote wakeup, 100 mA
            9, 4, 0, 0, 3, 0xff, 0x00, 0xff, 0,   // vendor-specific interface, 3 endpoints
            7, 5, 0x81, 2, 0x00, 0x02, 0,         // frames in: bulk, 512 bytes
            7, 5, 0x02, 2, 0x00, 0x02, 0,         // frames out: bulk, 512 bytes
            7, 5, 0x83, 3, 0x10, 0x00, 4,         // interrupts: 16 bytes every 1 ms
        ]
    }
    fn string(&self, i: u8) -> Option<&'static str> { match i { 1 => Some("sbc-emu"), 2 => Some("USB Gigabit Ethernet (LAN7800)"), _ => None } }
    fn speed(&self) -> Speed { Speed::High }
    fn class_setup(&mut self, s: [u8; 8]) -> Option<Vec<u8>> {
        let index = u16::from_le_bytes([s[4], s[5]]) as u32;
        let len = u16::from_le_bytes([s[6], s[7]]) as usize;
        match (s[0], s[1]) {
            (0xc0, 0xa1) => Some(self.read_reg(index).to_le_bytes().to_vec()),
            (0x40, 0xa0) => { self.pending_write = Some(index); Some(Vec::new()) }
            (0xc0, 0xa2) => Some(vec![0; len]), // statistics: all counters zero
            _ => None,
        }
    }
    fn class_out(&mut self, data: &[u8]) {
        if let (Some(r), Some(v)) = (self.pending_write.take(), data.get(..4)) { self.write_reg(r, u32::from_le_bytes(v.try_into().unwrap())); }
    }
    fn data_in(&mut self, ep: u8, max: usize) -> Option<Vec<u8>> {
        if !self.st.configured { return None; }
        match ep {
            1 => self.rx_transfer(max),
            3 => {
                let w = self.int_word();
                if w == 0 || w == self.int_sent { if w == 0 { self.int_sent = 0; } return None; }
                self.int_sent = w;
                Some(w.to_le_bytes()[..max.min(4)].to_vec())
            }
            _ => None,
        }
    }
    fn data_out(&mut self, ep: u8, data: &[u8]) { if ep == 2 { self.tx_transfer(data); } }
    fn has_input(&self) -> bool { self.st.configured && !self.rx_queue.is_empty() }
    fn has_input_ep(&self, ep: u8) -> bool {
        match ep {
            1 => self.has_input(),
            3 => self.st.configured && {
                let en = self.get(INT_EP_CTL) & 0x07ff_ffff;
                let w = (self.get(INT_STS) | if self.phy.irq() { INT_STS_PHY_INT } else { 0 }) & en;
                w != 0 && w != self.int_sent
            },
            _ => false,
        }
    }
    fn bus_reset(&mut self) { self.st = Std::default(); self.pending_write = None; self.int_sent = 0; }
}

// ---------------------------------------------------------------------- offloads

fn csum_add(mut sum: u32, data: &[u8]) -> u32 {
    let mut c = data.chunks_exact(2);
    for w in &mut c { sum += u16::from_be_bytes([w[0], w[1]]) as u32; }
    if let [b] = c.remainder() { sum += (*b as u32) << 8; }
    sum
}
fn csum_fold(mut sum: u32) -> u16 {
    while sum >> 16 != 0 { sum = (sum & 0xffff) + (sum >> 16); }
    !(sum as u16)
}

/// Where the IP header starts (after a VLAN tag, if any) and the ethertype.
fn l3(frame: &[u8]) -> Option<(usize, u16)> {
    let mut off = 12;
    let mut et = u16::from_be_bytes([*frame.get(off)?, *frame.get(off + 1)?]);
    if et == 0x8100 { off += 4; et = u16::from_be_bytes([*frame.get(off)?, *frame.get(off + 1)?]); }
    Some((off + 2, et))
}

/// IP version, header length, L4 protocol, L4 length (from the IP header).
fn ip_info(f: &[u8], ip: usize, et: u16) -> Option<(u8, usize, u8, usize)> {
    match et {
        0x0800 => {
            let ihl = (*f.get(ip)? as usize & 15) * 4;
            let total = u16::from_be_bytes([*f.get(ip + 2)?, *f.get(ip + 3)?]) as usize;
            Some((4, ihl, *f.get(ip + 9)?, total.checked_sub(ihl)?))
        }
        0x86dd => {
            let plen = u16::from_be_bytes([*f.get(ip + 4)?, *f.get(ip + 5)?]) as usize;
            Some((6, 40, *f.get(ip + 6)?, plen)) // no extension headers
        }
        _ => None,
    }
}

/// Fill in the IPv4 header checksum and the TCP/UDP checksum of a frame.
fn fill_checksums(f: &mut [u8], ip_csum: bool, l4_csum: bool) {
    let Some((ip, et)) = l3(f) else { return };
    let Some((ver, ihl, proto, l4len)) = ip_info(f, ip, et) else { return };
    if f.len() < ip + ihl + l4len { return; }
    if ver == 4 && ip_csum {
        f[ip + 10] = 0; f[ip + 11] = 0;
        let c = csum_fold(csum_add(0, &f[ip..ip + ihl]));
        f[ip + 10..ip + 12].copy_from_slice(&c.to_be_bytes());
    }
    let at = match proto { 6 => 16, 17 => 6, _ => return };
    if !l4_csum { return; }
    let l4 = ip + ihl;
    f[l4 + at] = 0; f[l4 + at + 1] = 0;
    let mut sum = if ver == 4 { csum_add(0, &f[ip + 12..ip + 20]) } else { csum_add(0, &f[ip + 8..ip + 40]) };
    sum += proto as u32 + l4len as u32;
    sum = csum_add(sum, &f[l4..l4 + l4len]);
    let mut c = csum_fold(sum);
    if proto == 17 && c == 0 { c = 0xffff; }
    f[l4 + at..l4 + at + 2].copy_from_slice(&c.to_be_bytes());
}

/// What the TX command words ask for: VLAN insertion, checksums, TCP segmentation. Returns the
/// frames as they go on the wire (without FCS).
pub fn tx_offload(frame: &[u8], a: u32, b: u32) -> Vec<Vec<u8>> {
    let mut f = frame.to_vec();
    if a & TX_CMD_A_IVTG != 0 && f.len() >= 12 {
        let tag = [0x81, 0x00, (b >> 8) as u8, b as u8];
        f.splice(12..12, tag);
    }
    let csum = a & (TX_CMD_A_IPE | TX_CMD_A_TPE) != 0;
    if a & TX_CMD_A_LSO != 0 {
        let mss = ((b >> 16) & 0x3fff) as usize;
        if let Some(segs) = segment(&f, mss.max(8)) { return segs; }
    }
    if csum { fill_checksums(&mut f, a & TX_CMD_A_IPE != 0, a & TX_CMD_A_TPE != 0); }
    vec![f]
}

/// Cut a TCP frame into segments of at most `mss` payload bytes, with headers and checksums
/// fixed up as the hardware does (IPv4 ID counting up, FIN/PSH on the last, CWR on the first).
fn segment(f: &[u8], mss: usize) -> Option<Vec<Vec<u8>>> {
    let (ip, et) = l3(f)?;
    let (ver, ihl, proto, _) = ip_info(f, ip, et)?;
    if proto != 6 { return None; }
    let tcp = ip + ihl;
    let thl = (*f.get(tcp + 12)? as usize >> 4) * 4;
    let hdr = tcp + thl;
    let payload = f.get(hdr..)?;
    let seq0 = u32::from_be_bytes(f.get(tcp + 4..tcp + 8)?.try_into().ok()?);
    let id0 = if ver == 4 { u16::from_be_bytes([f[ip + 4], f[ip + 5]]) } else { 0 };
    let flags = f[tcp + 13];
    let chunks: Vec<&[u8]> = if payload.is_empty() { vec![payload] } else { payload.chunks(mss).collect() };
    let n = chunks.len();
    let mut out = Vec::with_capacity(n);
    for (k, chunk) in chunks.into_iter().enumerate() {
        let mut s = f[..hdr].to_vec();
        s.extend_from_slice(chunk);
        let l4len = thl + chunk.len();
        if ver == 4 {
            s[ip + 2..ip + 4].copy_from_slice(&((ihl + l4len) as u16).to_be_bytes());
            s[ip + 4..ip + 6].copy_from_slice(&id0.wrapping_add(k as u16).to_be_bytes());
        } else {
            s[ip + 4..ip + 6].copy_from_slice(&(l4len as u16).to_be_bytes());
        }
        s[tcp + 4..tcp + 8].copy_from_slice(&seq0.wrapping_add((k * mss) as u32).to_be_bytes());
        let last = k + 1 == n;
        let mut fl = flags;
        if !last { fl &= !(0x01 | 0x08); } // FIN, PSH
        if k > 0 { fl &= !0x80; }          // CWR
        s[tcp + 13] = fl;
        fill_checksums(&mut s, true, true);
        out.push(s);
    }
    Some(out)
}

/// The Ethernet FCS (CRC-32, reflected, as it goes on the wire, little-endian).
pub fn crc32(data: &[u8]) -> u32 {
    let mut c = 0xffff_ffffu32;
    for &b in data {
        c ^= b as u32;
        for _ in 0..8 { c = if c & 1 != 0 { (c >> 1) ^ 0xedb8_8320 } else { c >> 1 }; }
    }
    !c
}

// ---------------------------------------------------------------------- snapshots

impl crate::snap::Snap for Lan78xx {
    fn save(&self, w: &mut crate::snap::W) {
        w.tag("lan78xx-v1");
        self.st.save(w);
        for r in &self.regs { w.u32(*r); }
        for r in self.phy.regs { w.u16(r); }
        self.phy.paged.save(w);
        self.phy.mmd.save(w);
        w.u16(self.phy.mmd_addr);
        w.bool(self.phy.cable);
        w.opt_u32(self.pending_write);
        w.u32(self.int_sent);
        self.rx_queue.save(w);
    }
    fn load(&mut self, r: &mut crate::snap::R) -> Result<(), crate::snap::SnapError> {
        r.tag("lan78xx-v1")?;
        self.st.load(r)?;
        for x in self.regs.iter_mut() { *x = r.u32()?; }
        for x in self.phy.regs.iter_mut() { *x = r.u16()?; }
        self.phy.paged.load(r)?;
        self.phy.mmd.load(r)?;
        self.phy.mmd_addr = r.u16()?;
        self.phy.cable = r.bool()?;
        self.pending_write = r.opt_u32()?;
        self.int_sent = r.u32()?;
        self.rx_queue.load(r)?;
        self.tx_out.clear();
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn wr(l: &mut Lan78xx, r: u32, v: u32) {
        let mut s = [0x40, 0xa0, 0, 0, 0, 0, 4, 0];
        s[4..6].copy_from_slice(&(r as u16).to_le_bytes());
        assert!(l.setup(s));
        l.control_out(&v.to_le_bytes());
        assert!(l.control_in(0).is_empty());
    }
    fn rd(l: &mut Lan78xx, r: u32) -> u32 {
        let mut s = [0xc0, 0xa1, 0, 0, 0, 0, 4, 0];
        s[4..6].copy_from_slice(&(r as u16).to_le_bytes());
        assert!(l.setup(s));
        let v = l.control_in(4);
        l.control_out(&[]);
        u32::from_le_bytes(v.try_into().unwrap())
    }
    fn mdio(l: &mut Lan78xx, reg: u32) -> u16 {
        wr(l, MII_ACC, (1 << 11) | (reg << 6) | MII_BUSY);
        assert_eq!(rd(l, MII_ACC) & MII_BUSY, 0);
        rd(l, MII_DATA) as u16
    }
    fn up(l: &mut Lan78xx) {
        l.st.configured = true;
        let v = rd(l, MAC_RX) | 1;
        wr(l, MAC_RX, v);
        wr(l, FCT_RX_CTL, FCT_EN);
        wr(l, MAC_TX, 1);
        wr(l, FCT_TX_CTL, FCT_EN);
    }

    /// An IPv4/TCP frame with `payload` bytes of data (checksums left zero, as with offload).
    fn tcp_frame(payload: usize, flags: u8) -> Vec<u8> {
        let mut f = vec![0x02, 0, 0, 0, 0, 2, 0x02, 0x5b, 0xc0, 0, 3, 1, 0x08, 0x00];
        let total = 20 + 20 + payload;
        f.extend_from_slice(&[0x45, 0, (total >> 8) as u8, total as u8, 0x12, 0x34, 0x40, 0, 64, 6, 0, 0, 10, 0, 2, 15, 10, 0, 2, 2]);
        f.extend_from_slice(&[0x9c, 0x40, 0x00, 0x50, 0, 0, 0x10, 0, 0, 0, 0, 1, 0x50, flags, 0xff, 0xff, 0, 0, 0, 0]);
        f.extend((0..payload).map(|i| i as u8));
        f
    }
    fn ip_ok(f: &[u8]) -> bool { csum_fold(csum_add(0, &f[14..34])) == 0 }
    fn tcp_ok(f: &[u8]) -> bool {
        let l4len = f.len() - 34;
        let mut sum = csum_add(0, &f[26..34]) + 6 + l4len as u32;
        sum = csum_add(sum, &f[34..]);
        csum_fold(sum) == 0
    }

    #[test]
    fn registers_resets_and_status_bits() {
        let mut l = Lan78xx::new();
        assert_eq!(rd(&mut l, ID_REV) >> 16, 0x7800);
        wr(&mut l, HW_CFG, HW_CFG_LRST);
        assert_eq!(rd(&mut l, HW_CFG) & HW_CFG_LRST, 0, "lite reset completes at once");
        wr(&mut l, PMT_CTL, PMT_CTL_PHY_RST);
        let p = rd(&mut l, PMT_CTL);
        assert_eq!(p & (PMT_CTL_PHY_RST | PMT_CTL_READY), PMT_CTL_READY);
        // the MAC address registers say "none": the driver takes the device tree's
        assert_eq!(l.mac(), [0xff; 6]);
        // enable/disable status pairs
        assert_ne!(rd(&mut l, MAC_RX) & MAC_RXTX_DIS, 0);
        let v = rd(&mut l, MAC_RX) | MAC_RXTX_EN;
        wr(&mut l, MAC_RX, v);
        assert_eq!(rd(&mut l, MAC_RX) & MAC_RXTX_DIS, 0);
        let v = rd(&mut l, MAC_RX) & !MAC_RXTX_EN;
        wr(&mut l, MAC_RX, v);
        assert_ne!(rd(&mut l, MAC_RX) & MAC_RXTX_DIS, 0);
        // no EEPROM signature, no OTP signature
        wr(&mut l, E2P_CMD, E2P_CMD_BUSY);
        assert_eq!(rd(&mut l, E2P_CMD) & E2P_CMD_BUSY, 0);
        assert_eq!(rd(&mut l, E2P_DATA), 0xff);
        wr(&mut l, OTP_CMD_GO, 1);
        assert_eq!(rd(&mut l, OTP_STATUS) & 1, 0);
        assert_eq!(rd(&mut l, OTP_RD_DATA), 0);
        // INT_STS: write 1 to clear
        l.set(INT_STS, 0x30);
        wr(&mut l, INT_STS, 0x10);
        assert_eq!(rd(&mut l, INT_STS), 0x20);
    }

    #[test]
    fn phy_identifies_and_links_at_gigabit() {
        let mut l = Lan78xx::new();
        assert_eq!((mdio(&mut l, 2), mdio(&mut l, 3)), (0x0007, 0xc132));
        let bmsr = mdio(&mut l, 1);
        assert_ne!(bmsr & BMSR_LINK, 0);
        assert_ne!(bmsr & BMSR_ANEGDONE, 0);
        assert_ne!(mdio(&mut l, 10) & 0x0800, 0, "partner: 1000BASE-T full duplex");
        // a write to another PHY address does nothing; reads float high
        wr(&mut l, MII_ACC, (2 << 11) | (2 << 6) | MII_BUSY);
        assert_eq!(rd(&mut l, MII_DATA), 0xffff);
        // power down: the link goes, and comes back on power up
        wr(&mut l, MII_DATA, 0x1940);
        wr(&mut l, MII_ACC, (1 << 11) | MII_WRITE | MII_BUSY);
        assert_eq!(mdio(&mut l, 1) & BMSR_LINK, 0);
        wr(&mut l, MII_DATA, BMCR_RESET as u32);
        wr(&mut l, MII_ACC, (1 << 11) | MII_WRITE | MII_BUSY);
        assert_eq!(mdio(&mut l, 0), 0x1140);
        assert_ne!(mdio(&mut l, 1) & BMSR_LINK, 0);
        // MMD indirect access and extended pages
        for (r, v) in [(13u32, 0x0003u32), (14, 0x8010), (13, 0x4003), (14, 0x0bad)] {
            wr(&mut l, MII_DATA, v);
            wr(&mut l, MII_ACC, (1 << 11) | (r << 6) | MII_WRITE | MII_BUSY);
        }
        assert_eq!(mdio(&mut l, 14), 0x0bad);
        assert_eq!(l.phy.mmd.get(&(3, 0x8010)), Some(&0x0bad));
    }

    #[test]
    fn rx_frames_are_packed_with_command_words_and_fcs() {
        let mut l = Lan78xx::new();
        l.receive(&[0xff; 60]);
        assert!(l.rx_queue.is_empty(), "receiver off: dropped");
        up(&mut l);
        let a = vec![0xffu8; 60];
        let mut b = vec![0x02, 0x5b, 0xc0, 0, 3, 1];
        b.extend((0..55).map(|i| i as u8));
        l.receive(&a);
        l.receive(&b);
        assert!(l.has_input_ep(1));
        let t = l.data_in(1, 16384).unwrap();
        // frame 1: 10 header bytes, 64 bytes with FCS, padded so (64 + 2) rounds up to 68
        let a0 = u32::from_le_bytes(t[0..4].try_into().unwrap());
        assert_eq!(a0 & RX_CMD_A_LEN, 64);
        assert_ne!(a0 & RX_CMD_A_BAM, 0);
        assert_ne!(a0 & RX_CMD_A_ICSM, 0);
        assert_eq!(&t[10..70], &a[..]);
        assert_eq!(u32::from_le_bytes(t[70..74].try_into().unwrap()), crc32(&a));
        let next = 10 + 64 + 2;
        let a1 = u32::from_le_bytes(t[next..next + 4].try_into().unwrap());
        assert_eq!(a1 & RX_CMD_A_LEN, 65);
        assert_eq!(&t[next + 10..next + 10 + 61], &b[..]);
        assert_eq!(t.len(), next + 10 + 65 + 1);
        assert_eq!(l.data_in(1, 16384), None);
        // a transfer that holds only one frame leaves the other queued
        l.receive(&a);
        l.receive(&a);
        assert_eq!(l.data_in(1, 100).unwrap().len(), 76);
        assert_eq!(l.rx_queue.len(), 1);
        assert_eq!(crc32(b"123456789"), 0xcbf4_3926);
    }

    #[test]
    fn tx_frames_with_checksum_offload() {
        let mut l = Lan78xx::new();
        up(&mut l);
        let f1 = tcp_frame(10, 0x18);
        let f2 = vec![0xffu8; 42];
        let mut buf = Vec::new();
        buf.extend_from_slice(&(f1.len() as u32 | TX_CMD_A_FCS | TX_CMD_A_IPE | TX_CMD_A_TPE).to_le_bytes());
        buf.extend_from_slice(&0u32.to_le_bytes());
        buf.extend_from_slice(&f1);
        while buf.len() % 4 != 0 { buf.push(0); }
        buf.extend_from_slice(&(f2.len() as u32 | TX_CMD_A_FCS).to_le_bytes());
        buf.extend_from_slice(&0u32.to_le_bytes());
        buf.extend_from_slice(&f2);
        l.data_out(2, &buf);
        assert_eq!(l.tx_out.len(), 2);
        assert!(ip_ok(&l.tx_out[0]) && tcp_ok(&l.tx_out[0]));
        assert_eq!(l.tx_out[0][14 + 20 + 20..], f1[54..]);
        assert_eq!(l.tx_out[1], f2);
    }

    #[test]
    fn tx_segmentation() {
        let f = tcp_frame(3000, 0x19); // PSH, FIN, ACK
        let segs = tx_offload(&f, f.len() as u32 | TX_CMD_A_LSO | TX_CMD_A_IPE | TX_CMD_A_TPE, 1400 << 16);
        assert_eq!(segs.len(), 3);
        let mut data = Vec::new();
        for (k, s) in segs.iter().enumerate() {
            assert!(ip_ok(s) && tcp_ok(s), "segment {k}");
            assert_eq!(u16::from_be_bytes([s[18], s[19]]), 0x1234 + k as u16);
            assert_eq!(u32::from_be_bytes(s[38..42].try_into().unwrap()), 0x1000 + (k * 1400) as u32);
            assert_eq!(s[47] & 0x09 != 0, k == 2, "FIN/PSH only on the last");
            assert_eq!(u16::from_be_bytes([s[16], s[17]]) as usize, s.len() - 14);
            data.extend_from_slice(&s[54..]);
        }
        assert_eq!(segs[0].len(), 54 + 1400);
        assert_eq!(data, f[54..]);
        // VLAN insertion
        let v = tx_offload(&f2(), 42 | TX_CMD_A_IVTG, 0x0005);
        assert_eq!(&v[0][12..16], &[0x81, 0x00, 0x00, 0x05]);
        fn f2() -> Vec<u8> { vec![0xffu8; 42] }
    }

    #[test]
    fn interrupt_endpoint_reports_the_phy_once() {
        let mut l = Lan78xx::new();
        l.st.configured = true;
        assert_eq!(l.data_in(3, 16), None);
        // the PHY interrupt unmasked on both sides, then the cable comes out
        wr(&mut l, INT_EP_CTL, INT_STS_PHY_INT);
        l.phy.write(INT_MASK, INT_LINK_CHANGE);
        l.phy.set_cable(false);
        assert!(l.has_input_ep(3));
        assert_eq!(l.data_in(3, 16), Some(INT_STS_PHY_INT.to_le_bytes().to_vec()));
        assert_eq!(l.data_in(3, 16), None);
        assert_eq!(l.phy.read(INT_STAT) & INT_LINK_CHANGE, INT_LINK_CHANGE);
        assert_eq!(l.data_in(3, 16), None);
    }

    #[test]
    fn snapshot_round_trip() {
        use crate::snap::Snap;
        let mut l = Lan78xx::new();
        up(&mut l);
        wr(&mut l, RX_ADDRL, 0x00c0_5b02);
        l.receive(&[1; 60]);
        let mut w = crate::snap::W::default();
        l.save(&mut w);
        let mut m = Lan78xx::new();
        m.load(&mut crate::snap::R::new(&w.0)).unwrap();
        assert!(m.rx_enabled());
        assert_eq!(m.rx_queue.len(), 1);
        assert_eq!(m.mac()[..4], [0x02, 0x5b, 0xc0, 0x00]);
    }
}
