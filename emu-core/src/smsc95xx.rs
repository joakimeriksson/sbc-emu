//! An SMSC (Microchip) LAN9514's Ethernet controller (USB 0424:ec00, the LAN9512/9514 family
//! of the LAN9500), as on a Raspberry Pi 3 Model B, for Linux's `smsc95xx` driver.
//!
//! - Registers through vendor control requests (0xa0 write, 0xa1 read), as on the LAN78xx;
//!   the self-clearing lite reset, PHY reset and FIFO flush bits complete at once. No EEPROM
//!   (a read times out), so the MAC address comes from the device tree.
//! - The internal 10/100 PHY (ID 0007:c0c3, Linux's "SMSC LAN8700") at MDIO address 1 through
//!   MII_ADDR/MII_DATA: the cable is plugged in and auto-negotiation ends at once with a
//!   100BASE-TX full-duplex partner. Its interrupt (ISF/IM, registers 29 and 30) reaches the
//!   host through INT_STS's PHY bit on the interrupt endpoint.
//! - Bulk IN (endpoint 1): each received frame behind its 32-bit RX status word (length with
//!   the FCS), after the RX data offset HW_CFG asks for, with its FCS and, while COE_CR enables
//!   it, the receive checksum (the ones' complement sum of everything after the Ethernet
//!   header); several frames per transfer only with HW_CFG.MEF, each padded to 4 bytes.
//! - Bulk OUT (endpoint 2): frames behind TX command words A and B; with the checksum bit in
//!   word B, the 32-bit preamble before the frame gives where the sum starts and where it goes.
//! - Interrupt IN (endpoint 3): INT_STS where INT_EP_CTL enables it.
use crate::lan78xx::{crc32, csum_add, csum_fold};
use crate::usb::{Speed, Std, UsbDevice};
use std::collections::VecDeque;

pub const ID_REV: u32 = 0x00;
pub const INT_STS: u32 = 0x08;
pub const RX_CFG: u32 = 0x0c;
pub const TX_CFG: u32 = 0x10;
pub const HW_CFG: u32 = 0x14;
pub const RX_FIFO_INF: u32 = 0x18;
pub const TX_FIFO_INF: u32 = 0x1c;
pub const PM_CTRL: u32 = 0x20;
pub const E2P_CMD: u32 = 0x30;
pub const E2P_DATA: u32 = 0x34;
pub const BURST_CAP: u32 = 0x38;
pub const INT_EP_CTL: u32 = 0x68;
pub const MAC_CR: u32 = 0x100;
pub const ADDRH: u32 = 0x104;
pub const ADDRL: u32 = 0x108;
pub const MII_ADDR: u32 = 0x114;
pub const MII_DATA: u32 = 0x118;
pub const COE_CR: u32 = 0x130;
const NREGS: usize = 0x140 / 4;

pub const INT_STS_PHY_INT: u32 = 1 << 15;
const RX_FIFO_FLUSH: u32 = 1 << 0;
const TX_CFG_ON: u32 = 1 << 2;
const TX_CFG_STOP: u32 = 1 << 1;
const TX_CFG_FIFO_FLUSH: u32 = 1 << 0;
const HW_CFG_RXDOFF: u32 = 3 << 9;
const HW_CFG_MEF: u32 = 1 << 5;
const HW_CFG_LRST: u32 = 1 << 3;
const HW_CFG_SRST: u32 = 1 << 0;
const PM_CTL_DEV_RDY: u32 = 1 << 7;
const PM_CTL_PHY_RST: u32 = 1 << 4;
const E2P_CMD_BUSY: u32 = 1 << 31;
const E2P_CMD_TIMEOUT: u32 = 1 << 10;
const MAC_CR_TXEN: u32 = 1 << 3;
const MAC_CR_RXEN: u32 = 1 << 2;
const MII_BUSY: u32 = 1 << 0;
const MII_WRITE: u32 = 1 << 1;
const TX_COE_EN: u32 = 1 << 16;
const RX_COE_EN: u32 = 1 << 0;

pub const TX_CMD_A_FIRST_SEG: u32 = 1 << 13;
pub const TX_CMD_A_LAST_SEG: u32 = 1 << 12;
pub const TX_CMD_A_BUF_SIZE: u32 = 0x7ff;
pub const TX_CMD_B_CSUM_ENABLE: u32 = 1 << 14;
pub const RX_STS_BF: u32 = 1 << 13;
pub const RX_STS_MF: u32 = 1 << 10;
/// RX status: the frame type bit (an Ethernet II frame, not 802.3 with a length)
pub const RX_STS_FT: u32 = 1 << 5;

/// The largest bulk IN transfer the device builds (the driver's turbo-mode URB is 18944 bytes).
const BURST_MAX: usize = 18944;

/// The internal 10/100 PHY with a 100BASE-TX full-duplex partner.
#[derive(Clone)]
pub struct Phy {
    regs: [u16; 32],
    /// the cable is plugged in
    pub cable: bool,
}

const BMCR: usize = 0;
const BMSR: usize = 1;
const BMCR_RESET: u16 = 1 << 15;
const BMCR_ANENABLE: u16 = 1 << 12;
const BMCR_PDOWN: u16 = 1 << 11;
const BMCR_ANRESTART: u16 = 1 << 9;
const BMSR_LINK: u16 = 1 << 2;
const BMSR_ANEGDONE: u16 = 1 << 5;
const CTRL_STATUS: usize = 17;
const ENERGYON: u16 = 1 << 1;
const ISF: usize = 29;
const IM: usize = 30;
/// ISF: link down, auto-negotiation complete
const ISF_LINK_DOWN: u16 = 1 << 4;
const ISF_ANEG_COMPLETE: u16 = 1 << 6;

impl Default for Phy { fn default() -> Self { Self::new() } }

impl Phy {
    pub fn new() -> Self {
        let mut p = Phy { regs: [0; 32], cable: true };
        p.reset();
        p
    }

    fn reset(&mut self) {
        self.regs = [0; 32];
        self.regs[BMCR] = 0x3100; // 100 Mb/s, auto-negotiation on, full duplex
        self.regs[2] = 0x0007;
        self.regs[3] = 0xc0c3;
        self.regs[4] = 0x01e1; // 10/100, half and full duplex
        self.regs[18] = 0x00e1; // special modes: all capable, PHY address 1
    }

    /// The link is up: powered, the cable in, negotiation finished (it finishes at once).
    pub fn link(&self) -> bool { self.cable && self.regs[BMCR] & BMCR_PDOWN == 0 }

    pub fn read(&mut self, r: usize) -> u16 {
        let link = self.link();
        match r & 31 {
            // 100BASE-TX and 10BASE-T in both duplexes, preamble suppression, auto-negotiation
            BMSR => 0x7849 | if link { BMSR_LINK | BMSR_ANEGDONE } else { 0 },
            5 => if link { 0x45e1 } else { 0 }, // partner: ack, pause, 10/100 both duplexes
            6 => link as u16,                   // partner can negotiate
            CTRL_STATUS => self.regs[CTRL_STATUS] & !ENERGYON | if link { ENERGYON } else { 0 },
            ISF => std::mem::take(&mut self.regs[ISF]), // read to clear
            // special control/status: auto-negotiation done, 100BASE-TX full duplex
            31 => (self.regs[31] & !0x101c) | if link { 0x1018 } else { 0 },
            r => self.regs[r],
        }
    }

    pub fn write(&mut self, r: usize, v: u16) {
        match r & 31 {
            BMCR => {
                if v & BMCR_RESET != 0 { self.reset(); return; }
                let was = self.link();
                self.regs[BMCR] = v & !BMCR_ANRESTART;
                if was != self.link() || (v & BMCR_ANRESTART != 0 && self.link()) { self.link_changed(); }
            }
            BMSR | 2 | 3 | 5 | 6 | ISF => {}
            r => self.regs[r] = v,
        }
    }

    fn link_changed(&mut self) {
        self.regs[ISF] |= if self.link() { if self.regs[BMCR] & BMCR_ANENABLE != 0 { ISF_ANEG_COMPLETE } else { 0 } } else { ISF_LINK_DOWN };
    }

    /// The PHY's interrupt output (its flags masked).
    fn irq(&self) -> bool { self.regs[ISF] & self.regs[IM] != 0 }

    /// Plug the cable in or pull it out.
    pub fn set_cable(&mut self, on: bool) {
        if on != self.cable { self.cable = on; self.link_changed(); }
    }
}

pub struct Smsc95xx {
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

impl Default for Smsc95xx { fn default() -> Self { Self::new() } }

impl Smsc95xx {
    pub fn new() -> Self {
        let mut l = Smsc95xx { st: Std::default(), regs: vec![0; NREGS], phy: Phy::new(), pending_write: None, int_sent: 0,
                               tx_out: Vec::new(), rx_queue: VecDeque::new(), tx_frames: 0, rx_frames: 0, rx_dropped: 0 };
        l.power_on();
        l
    }

    /// Register values at power-on (no EEPROM: the address registers are all ones).
    fn power_on(&mut self) {
        self.regs.iter_mut().for_each(|r| *r = 0);
        self.set(ID_REV, 0xec00_0002);
        self.set(PM_CTRL, PM_CTL_DEV_RDY);
        self.set(ADDRL, 0xffff_ffff);
        self.set(ADDRH, 0x0000_ffff);
        self.set(TX_FIFO_INF, 0x0000_1200); // the TX data FIFO's free space
        self.set(MAC_CR, 0x0010_0000);      // full duplex
    }

    fn set(&mut self, r: u32, v: u32) { if let Some(x) = self.regs.get_mut(r as usize / 4) { *x = v; } }
    fn get(&self, r: u32) -> u32 { self.regs.get(r as usize / 4).copied().unwrap_or(0) }

    pub fn rx_enabled(&self) -> bool { self.get(MAC_CR) & MAC_CR_RXEN != 0 }
    pub fn tx_enabled(&self) -> bool { self.get(MAC_CR) & MAC_CR_TXEN != 0 && self.get(TX_CFG) & TX_CFG_ON != 0 }

    /// The MAC address the driver programmed (ADDRL/H).
    pub fn mac(&self) -> [u8; 6] {
        let (l, h) = (self.get(ADDRL), self.get(ADDRH));
        [l as u8, (l >> 8) as u8, (l >> 16) as u8, (l >> 24) as u8, h as u8, (h >> 8) as u8]
    }

    pub fn read_reg(&mut self, r: u32) -> u32 {
        let v = self.get(r);
        match r {
            INT_STS => v | if self.phy.irq() { INT_STS_PHY_INT } else { 0 },
            RX_FIFO_INF => self.rx_queue.iter().map(|f| f.len() as u32 + 8).sum::<u32>().min(0xffff),
            _ => v,
        }
    }

    pub fn write_reg(&mut self, r: u32, v: u32) {
        match r {
            ID_REV | RX_FIFO_INF | TX_FIFO_INF => {}
            INT_STS => { let old = self.get(INT_STS); self.set(INT_STS, old & !v); }
            RX_CFG => {
                if v & RX_FIFO_FLUSH != 0 { self.rx_queue.clear(); }
                self.set(RX_CFG, v & !RX_FIFO_FLUSH);
            }
            TX_CFG => {
                // stopping the transmitter turns it off
                let v = if v & TX_CFG_STOP != 0 { v & !TX_CFG_ON } else { v };
                self.set(TX_CFG, v & !(TX_CFG_STOP | TX_CFG_FIFO_FLUSH));
            }
            HW_CFG => {
                if v & HW_CFG_LRST != 0 { self.rx_queue.clear(); self.set(MAC_CR, 0x0010_0000); self.set(TX_CFG, 0); }
                self.set(HW_CFG, v & !(HW_CFG_LRST | HW_CFG_SRST));
            }
            PM_CTRL => {
                if v & PM_CTL_PHY_RST != 0 { self.phy.reset(); }
                let wake = self.get(PM_CTRL) & 3 & !v; // wake-up status bits: write 1 to clear
                self.set(PM_CTRL, (v & !(PM_CTL_PHY_RST | 3)) | PM_CTL_DEV_RDY | wake);
            }
            // no EEPROM: every command times out
            E2P_CMD => { self.set(E2P_CMD, (v & !E2P_CMD_BUSY) | E2P_CMD_TIMEOUT); self.set(E2P_DATA, 0xff); }
            MII_ADDR => {
                if v & MII_BUSY != 0 {
                    let (phy, reg) = ((v >> 11) & 0x1f, ((v >> 6) & 0x1f) as usize);
                    if v & MII_WRITE != 0 {
                        if phy == 1 { let d = self.get(MII_DATA) as u16; self.phy.write(reg, d); }
                    } else {
                        let d = if phy == 1 { self.phy.read(reg) } else { 0xffff };
                        self.set(MII_DATA, d as u32);
                    }
                }
                self.set(MII_ADDR, v & !MII_BUSY);
            }
            MAC_CR => {
                if v & MAC_CR_RXEN == 0 { self.rx_queue.clear(); }
                self.set(MAC_CR, v);
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
        let hw = self.get(HW_CFG);
        let (offset, several) = (((hw & HW_CFG_RXDOFF) >> 9) as usize, hw & HW_CFG_MEF != 0);
        let coe = self.get(COE_CR) & RX_COE_EN != 0;
        let mut out = Vec::new();
        while let Some(f) = self.rx_queue.front() {
            let size = f.len() + 4 + if coe { 2 } else { 0 }; // with the FCS and the checksum
            if out.len() + 4 + offset + size > max && !out.is_empty() { break; }
            let f = self.rx_queue.pop_front().unwrap();
            let mut sts = (size as u32) << 16;
            if f[0] & 1 != 0 { sts |= if f[..6] == [0xff; 6] { RX_STS_BF } else { RX_STS_MF }; }
            if u16::from_be_bytes([f[12], f[13]]) >= 0x0600 { sts |= RX_STS_FT; }
            out.extend_from_slice(&sts.to_le_bytes());
            out.resize(out.len() + offset, 0);
            out.extend_from_slice(&f);
            out.extend_from_slice(&crc32(&f).to_le_bytes());
            if coe {
                // the sum of the bytes after the Ethernet header, folded, in network order
                let mut s = csum_add(0, &f[14..]);
                while s >> 16 != 0 { s = (s & 0xffff) + (s >> 16); }
                out.extend_from_slice(&(s as u16).to_be_bytes());
            }
            self.rx_frames += 1;
            if !several { break; }
            let pad = (4 - (size + offset) % 4) % 4;
            if self.rx_queue.is_empty() { break; }
            out.resize(out.len() + pad, 0);
        }
        (!out.is_empty()).then_some(out)
    }

    /// A bulk OUT transfer: frames behind their TX command words.
    fn tx_transfer(&mut self, data: &[u8]) {
        let mut off = 0;
        while off + 8 <= data.len() {
            let a = u32::from_le_bytes(data[off..off + 4].try_into().unwrap());
            let b = u32::from_le_bytes(data[off + 4..off + 8].try_into().unwrap());
            let start = off + 8 + ((a >> 16) & 0x1f) as usize;
            let len = (a & TX_CMD_A_BUF_SIZE) as usize;
            let Some(buf) = data.get(start..start + len) else { break };
            if self.tx_enabled() && self.phy.link() {
                if let Some(f) = tx_checksum(buf, b, self.get(COE_CR) & TX_COE_EN != 0) { self.tx_frames += 1; self.tx_out.push(f); }
            }
            off = start + len;
            off += (4 - off % 4) % 4;
        }
    }

    /// The interrupt endpoint's word: INT_STS bits that INT_EP_CTL lets through.
    fn int_word(&mut self) -> u32 {
        let en = self.get(INT_EP_CTL) & 0x000f_ffff;
        self.read_reg(INT_STS) & en
    }
}

/// A frame as it goes on the wire, from one TX buffer: with the checksum bit in word B (and
/// the engine on), the buffer starts with the preamble (where the sum starts, where it goes);
/// the sum covers the rest of the frame, the pseudo-header sum the stack left in place included.
pub fn tx_checksum(buf: &[u8], b: u32, coe: bool) -> Option<Vec<u8>> {
    if b & TX_CMD_B_CSUM_ENABLE == 0 || !coe {
        return Some(buf.to_vec());
    }
    let pre = u32::from_le_bytes(buf.get(..4)?.try_into().ok()?);
    let mut f = buf[4..].to_vec();
    let (start, at) = ((pre & 0xffff) as usize, (pre >> 16) as usize);
    if start < f.len() && at + 2 <= f.len() {
        let c = csum_fold(csum_add(0, &f[start..]));
        f[at..at + 2].copy_from_slice(&c.to_be_bytes());
    }
    Some(f)
}

impl UsbDevice for Smsc95xx {
    fn std(&mut self) -> &mut Std { &mut self.st }
    fn device_desc(&self) -> Vec<u8> {
        // vendor-specific class, SMSC's IDs for the LAN9514's Ethernet so that `smsc95xx` binds
        vec![18, 1, 0x00, 0x02, 0xff, 0x00, 0xff, 64, 0x24, 0x04, 0x00, 0xec, 0x00, 0x02, 0, 0, 0, 1]
    }
    fn config_desc(&self) -> Vec<u8> {
        vec![
            9, 2, 39, 0, 1, 1, 0, 0xe0, 1,        // self-powered, remote wakeup, 2 mA
            9, 4, 0, 0, 3, 0xff, 0x00, 0xff, 0,   // vendor-specific interface, 3 endpoints
            7, 5, 0x81, 2, 0x00, 0x02, 0,         // frames in: bulk, 512 bytes
            7, 5, 0x02, 2, 0x00, 0x02, 0,         // frames out: bulk, 512 bytes
            7, 5, 0x83, 3, 0x10, 0x00, 4,         // interrupts: 16 bytes every 1 ms
        ]
    }
    fn string(&self, _i: u8) -> Option<&'static str> { None }
    fn speed(&self) -> Speed { Speed::High }
    fn self_powered(&self) -> bool { true }
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
                let en = self.get(INT_EP_CTL) & 0x000f_ffff;
                let w = (self.get(INT_STS) | if self.phy.irq() { INT_STS_PHY_INT } else { 0 }) & en;
                w != 0 && w != self.int_sent
            },
            _ => false,
        }
    }
    fn bus_reset(&mut self) { self.st = Std::default(); self.pending_write = None; self.int_sent = 0; }
}

impl crate::snap::Snap for Smsc95xx {
    fn save(&self, w: &mut crate::snap::W) {
        w.tag("smsc95xx-v1");
        self.st.save(w);
        for r in &self.regs { w.u32(*r); }
        for r in self.phy.regs { w.u16(r); }
        w.bool(self.phy.cable);
        w.opt_u32(self.pending_write);
        w.u32(self.int_sent);
        self.rx_queue.save(w);
    }
    fn load(&mut self, r: &mut crate::snap::R) -> Result<(), crate::snap::SnapError> {
        r.tag("smsc95xx-v1")?;
        self.st.load(r)?;
        for x in self.regs.iter_mut() { *x = r.u32()?; }
        for x in self.phy.regs.iter_mut() { *x = r.u16()?; }
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

    fn wr(l: &mut Smsc95xx, r: u32, v: u32) {
        let mut s = [0x40, 0xa0, 0, 0, 0, 0, 4, 0];
        s[4..6].copy_from_slice(&(r as u16).to_le_bytes());
        assert!(l.setup(s));
        l.control_out(&v.to_le_bytes());
        assert!(l.control_in(0).is_empty());
    }
    fn rd(l: &mut Smsc95xx, r: u32) -> u32 {
        let mut s = [0xc0, 0xa1, 0, 0, 0, 0, 4, 0];
        s[4..6].copy_from_slice(&(r as u16).to_le_bytes());
        assert!(l.setup(s));
        let v = l.control_in(4);
        l.control_out(&[]);
        u32::from_le_bytes(v.try_into().unwrap())
    }
    fn mdio(l: &mut Smsc95xx, reg: u32) -> u16 {
        wr(l, MII_ADDR, (1 << 11) | (reg << 6) | MII_BUSY);
        assert_eq!(rd(l, MII_ADDR) & MII_BUSY, 0);
        rd(l, MII_DATA) as u16
    }
    fn mdio_wr(l: &mut Smsc95xx, reg: u32, v: u16) {
        wr(l, MII_DATA, v as u32);
        wr(l, MII_ADDR, (1 << 11) | (reg << 6) | MII_WRITE | MII_BUSY);
    }
    /// What `smsc95xx_reset` leaves: turbo mode, IP alignment, checksum engines, both paths on.
    fn up(l: &mut Smsc95xx) {
        l.st.configured = true;
        wr(l, HW_CFG, HW_CFG_LRST);
        assert_eq!(rd(l, HW_CFG) & HW_CFG_LRST, 0, "lite reset completes at once");
        wr(l, HW_CFG, HW_CFG_MEF | 2 | (2 << 9));
        wr(l, COE_CR, TX_COE_EN | RX_COE_EN);
        let cr = rd(l, MAC_CR) | MAC_CR_TXEN | MAC_CR_RXEN;
        wr(l, MAC_CR, cr);
        wr(l, TX_CFG, TX_CFG_ON);
    }
    /// An IPv4/TCP frame with `payload` bytes of data and correct checksums.
    fn tcp_frame(payload: usize) -> Vec<u8> {
        let mut f = vec![0x02, 0x5b, 0xc0, 0, 3, 1, 0x02, 0, 0, 0, 0, 2, 0x08, 0x00];
        let total = 20 + 20 + payload;
        f.extend_from_slice(&[0x45, 0, (total >> 8) as u8, total as u8, 0x12, 0x34, 0x40, 0, 64, 6, 0, 0, 10, 0, 2, 2, 10, 0, 2, 15]);
        f.extend_from_slice(&[0x00, 0x50, 0x9c, 0x40, 0, 0, 0x10, 0, 0, 0, 0, 1, 0x50, 0x18, 0xff, 0xff, 0, 0, 0, 0]);
        f.extend((0..payload).map(|i| i as u8));
        let c = csum_fold(csum_add(0, &f[14..34]));
        f[24..26].copy_from_slice(&c.to_be_bytes());
        let mut sum = csum_add(0, &f[26..34]) + 6 + (20 + payload) as u32;
        sum = csum_add(sum, &f[34..]);
        let c = csum_fold(sum);
        f[50..52].copy_from_slice(&c.to_be_bytes());
        f
    }

    #[test]
    fn registers_and_the_lan8700_phy() {
        let mut l = Smsc95xx::new();
        assert_eq!(rd(&mut l, ID_REV) >> 16, 0xec00);
        assert_eq!(rd(&mut l, HW_CFG) & (1 << 2), 0, "the internal PHY (PSEL clear)");
        assert_eq!(l.mac(), [0xff; 6], "no EEPROM: the driver takes the device tree's address");
        wr(&mut l, E2P_CMD, E2P_CMD_BUSY | 1);
        assert_eq!(rd(&mut l, E2P_CMD) & (E2P_CMD_BUSY | E2P_CMD_TIMEOUT), E2P_CMD_TIMEOUT);
        wr(&mut l, PM_CTRL, PM_CTL_PHY_RST);
        assert_eq!(rd(&mut l, PM_CTRL) & (PM_CTL_PHY_RST | PM_CTL_DEV_RDY), PM_CTL_DEV_RDY);
        assert_eq!((mdio(&mut l, 2), mdio(&mut l, 3)), (0x0007, 0xc0c3));
        let bmsr = mdio(&mut l, 1);
        assert_eq!(bmsr & (BMSR_LINK | BMSR_ANEGDONE), BMSR_LINK | BMSR_ANEGDONE);
        assert_eq!(bmsr & 0x0100, 0, "no extended status: a 10/100 PHY");
        // another PHY address floats high
        wr(&mut l, MII_ADDR, (2 << 11) | (2 << 6) | MII_BUSY);
        assert_eq!(rd(&mut l, MII_DATA), 0xffff);
        // INT_STS: write 1 to clear
        l.set(INT_STS, 0x30);
        wr(&mut l, INT_STS, 0x10);
        assert_eq!(rd(&mut l, INT_STS), 0x20);
    }

    #[test]
    fn phy_interrupt_reaches_the_endpoint() {
        let mut l = Smsc95xx::new();
        l.st.configured = true;
        wr(&mut l, INT_EP_CTL, INT_STS_PHY_INT);
        mdio_wr(&mut l, IM as u32, ISF_LINK_DOWN | ISF_ANEG_COMPLETE);
        assert_eq!(l.data_in(3, 16), None);
        // a restarted negotiation completes at once and says so
        mdio_wr(&mut l, 0, BMCR_ANENABLE | BMCR_ANRESTART);
        assert!(l.has_input_ep(3));
        assert_eq!(l.data_in(3, 16), Some(INT_STS_PHY_INT.to_le_bytes().to_vec()));
        assert_eq!(l.data_in(3, 16), None, "reported once");
        assert_eq!(mdio(&mut l, ISF as u32) & ISF_ANEG_COMPLETE, ISF_ANEG_COMPLETE);
        assert_eq!(mdio(&mut l, ISF as u32), 0, "read to clear");
        assert_eq!(l.data_in(3, 16), None);
        l.phy.set_cable(false);
        assert_eq!(l.data_in(3, 16), Some(INT_STS_PHY_INT.to_le_bytes().to_vec()));
        assert_eq!(mdio(&mut l, 1) & BMSR_LINK, 0);
    }

    #[test]
    fn rx_frames_with_status_offset_fcs_and_checksum() {
        let mut l = Smsc95xx::new();
        l.receive(&[0xff; 60]);
        assert!(l.rx_queue.is_empty(), "receiver off: dropped");
        up(&mut l);
        let a = tcp_frame(5);
        let b = vec![0xffu8; 42];
        l.receive(&a);
        l.receive(&b);
        let t = l.data_in(1, 18944).unwrap();
        let sts = u32::from_le_bytes(t[0..4].try_into().unwrap());
        let size = (sts >> 16) as usize;
        assert_eq!(size, a.len() + 4 + 2, "frame, FCS, checksum");
        assert_ne!(sts & RX_STS_FT, 0);
        assert_eq!(&t[6..6 + a.len()], &a[..], "after the 2-byte offset");
        assert_eq!(u32::from_le_bytes(t[6 + a.len()..10 + a.len()].try_into().unwrap()), crc32(&a));
        // the hardware's sum over the IP packet: with the TCP pseudo header it checks out
        let hw = u16::from_be_bytes([t[10 + a.len()], t[11 + a.len()]]);
        let pseudo = csum_add(0, &a[26..34]) + 6 + (a.len() - 34) as u32;
        // the IP header sums to 0xffff, so the packet's sum is the TCP segment's
        assert_eq!(csum_fold(pseudo + hw as u32), 0);
        // the second frame after the padding (the driver's align_count)
        let next = 4 + 2 + size + (4 - (size + 2) % 4) % 4;
        let sts1 = u32::from_le_bytes(t[next..next + 4].try_into().unwrap());
        assert_eq!((sts1 >> 16) as usize, b.len() + 6);
        assert_ne!(sts1 & RX_STS_BF, 0);
        assert_eq!(&t[next + 6..next + 6 + b.len()], &b[..]);
        assert_eq!(t.len(), next + 6 + b.len() + 6);
        assert_eq!(l.data_in(1, 18944), None);
        // without MEF one frame per transfer
        wr(&mut l, HW_CFG, 2 << 9);
        l.receive(&b);
        l.receive(&b);
        assert_eq!(l.data_in(1, 18944).unwrap().len(), 4 + 2 + b.len() + 6);
        assert_eq!(l.rx_queue.len(), 1);
    }

    #[test]
    fn tx_frames_with_and_without_checksum_offload() {
        let mut l = Smsc95xx::new();
        up(&mut l);
        let good = tcp_frame(40);
        // the stack leaves the pseudo-header sum in the TCP checksum field
        let mut partial = good.clone();
        let pseudo = csum_add(0, &good[26..34]) + 6 + (good.len() - 34) as u32;
        let mut p = pseudo;
        while p >> 16 != 0 { p = (p & 0xffff) + (p >> 16); }
        partial[50..52].copy_from_slice(&(p as u16).to_be_bytes());
        let mut buf = Vec::new();
        let len = partial.len() as u32 + 4;
        buf.extend_from_slice(&(len | TX_CMD_A_FIRST_SEG | TX_CMD_A_LAST_SEG).to_le_bytes());
        buf.extend_from_slice(&(len | TX_CMD_B_CSUM_ENABLE).to_le_bytes());
        buf.extend_from_slice(&((50u32 << 16) | 34).to_le_bytes());
        buf.extend_from_slice(&partial);
        while buf.len() % 4 != 0 { buf.push(0); }
        let arp = vec![0xffu8; 42];
        buf.extend_from_slice(&(42 | TX_CMD_A_FIRST_SEG | TX_CMD_A_LAST_SEG).to_le_bytes());
        buf.extend_from_slice(&42u32.to_le_bytes());
        buf.extend_from_slice(&arp);
        l.data_out(2, &buf);
        assert_eq!(l.tx_out, vec![good, arp]);
    }

    #[test]
    fn snapshot_round_trip() {
        use crate::snap::Snap;
        let mut l = Smsc95xx::new();
        up(&mut l);
        wr(&mut l, ADDRL, 0x00c0_5b02);
        l.receive(&[1; 60]);
        let mut w = crate::snap::W::default();
        l.save(&mut w);
        let mut m = Smsc95xx::new();
        m.load(&mut crate::snap::R::new(&w.0)).unwrap();
        assert!(m.rx_enabled() && m.tx_enabled());
        assert_eq!(m.rx_queue.len(), 1);
        assert_eq!(m.mac()[..4], [0x02, 0x5b, 0xc0, 0x00]);
    }
}
