//! The JH7110's Gigabit Ethernet MACs (`starfive,dwmac` / `snps,dwmac-5.10a`, Linux
//! `stmmac` + `dwmac-starfive`): GMAC0 at 0x16030000 with the Orange Pi RV's Motorcomm
//! YT8531C PHY at MDIO address 0, and GMAC1 at 0x16040000 (no PHY fitted on this board).
//!
//! Modelled at the level stmmac uses with the features this model advertises (one DMA
//! channel and queue, no checksum offload, no TSO, no PTP, no EEE): the MDIO interface, the
//! MAC address and filter registers as plain storage, the DMA software reset, and channel
//! 0's descriptor rings in the DWMAC4 "normal" format. Owned TX descriptors are sent when
//! the driver moves the tail pointer, received frames fill owned RX descriptors (with the
//! 4-byte FCS the driver strips), and TI/RI/NIS raise the MAC's interrupt.
//! The register and descriptor layout follows sbc-emu's RK3588 GMAC (`rk3588/src/gmac.rs`).
use emu_core::FlatRam;
use std::collections::VecDeque;

// DMA channel 0
const DMA_MODE: usize = 0x1000;
const DMA_STATUS: usize = 0x1008;
const CH_TX_CONTROL: usize = 0x1104;
const CH_RX_CONTROL: usize = 0x1108;
const CH_TX_LIST_HI: usize = 0x1110;
const CH_TX_LIST: usize = 0x1114;
const CH_RX_LIST_HI: usize = 0x1118;
const CH_RX_LIST: usize = 0x111c;
const CH_TX_TAIL: usize = 0x1120;
const CH_RX_TAIL: usize = 0x1128;
const CH_TX_RING_LEN: usize = 0x112c;
const CH_RX_RING_LEN: usize = 0x1130;
const CH_INT_EN: usize = 0x1134;
const CH_CUR_TX: usize = 0x1144;
const CH_CUR_RX: usize = 0x114c;
const CH_STATUS: usize = 0x1160;
// MAC
const MAC_VERSION: usize = 0x110;
const MAC_HW_FEATURE0: usize = 0x11c;
const MAC_HW_FEATURE1: usize = 0x120;
const MAC_HW_FEATURE2: usize = 0x124;
const MAC_HW_FEATURE3: usize = 0x128;
const MAC_MDIO_ADDRESS: usize = 0x200;
const MAC_MDIO_DATA: usize = 0x204;
const MTL_TXQ0_OPERATION_MODE: usize = 0xd00;

// CH_STATUS / CH_INT_EN
const TI: u32 = 1 << 0;
const TBU: u32 = 1 << 2;
const RI: u32 = 1 << 6;
const RBU: u32 = 1 << 7;
const FBE: u32 = 1 << 12;
const AIS: u32 = 1 << 14;
const NIS: u32 = 1 << 15;

// descriptor word 3
const OWN: u32 = 1 << 31;
const CTXT: u32 = 1 << 30;
const FD: u32 = 1 << 29;
const LD: u32 = 1 << 28;

pub struct Gmac {
    regs: Vec<u32>,
    /// the PHY on this MAC's MDIO bus, if the board fits one
    pub phy: Option<(u8, Yt8531)>,
    tx_index: u32,
    rx_index: u32,
    tx_partial: Vec<u8>,
    /// frames sent by the guest, for the network
    pub tx_out: Vec<Vec<u8>>,
    /// frames waiting for an owned RX descriptor
    pub rx_queue: VecDeque<Vec<u8>>,
    pub tx_frames: u64,
    pub rx_frames: u64,
}

impl Gmac {
    pub fn new(phy: Option<(u8, Yt8531)>) -> Self {
        Gmac {
            regs: vec![0; 0x10000 / 4],
            phy,
            tx_index: 0,
            rx_index: 0,
            tx_partial: Vec::new(),
            tx_out: Vec::new(),
            rx_queue: VecDeque::new(),
            tx_frames: 0,
            rx_frames: 0,
        }
    }

    fn r(&self, off: usize) -> u32 {
        self.regs[off / 4]
    }

    fn set(&mut self, off: usize, v: u32) {
        self.regs[off / 4] = v;
    }

    fn status(&self) -> u32 {
        let s = self.r(CH_STATUS) & !(NIS | AIS);
        let mut v = s;
        if s & (TI | TBU | RI) != 0 {
            v |= NIS;
        }
        if s & (RBU | FBE) != 0 {
            v |= AIS;
        }
        v
    }

    /// The MAC's main interrupt (`macirq`).
    pub fn irq(&self) -> bool {
        let s = self.status();
        let en = self.r(CH_INT_EN);
        (en & NIS != 0 && s & en & (TI | TBU | RI) != 0) || (en & AIS != 0 && s & en & (RBU | FBE) != 0)
    }

    pub fn read(&mut self, off: u64) -> u32 {
        let off = off as usize & 0xfffc;
        match off {
            MAC_VERSION => 0x1051, // user version 0x10, Synopsys version 5.10a
            // GMII/MII, half duplex, hash filter, 64 perfect address entries
            MAC_HW_FEATURE0 => 0b111 | (1 << 4) | (0x1f << 18),
            // 16 KiB RX and TX FIFOs (128 << 7), 32-bit addressing
            MAC_HW_FEATURE1 => 7 | (7 << 6),
            // one RX/TX queue and DMA channel
            MAC_HW_FEATURE2 => 0,
            MAC_HW_FEATURE3 => 0,
            DMA_STATUS => self.irq() as u32,
            CH_STATUS => self.status(),
            _ => self.r(off),
        }
    }

    /// A register write; returns true when the DMA should look at its rings.
    pub fn write(&mut self, off: u64, v: u32) -> bool {
        let off = off as usize & 0xfffc;
        match off {
            DMA_MODE => {
                if v & 1 != 0 {
                    // software reset: the DMA and MAC registers return to their defaults
                    let phy = self.phy.take();
                    *self = Gmac::new(phy);
                }
                self.set(DMA_MODE, v & !1);
            }
            MAC_MDIO_ADDRESS => {
                if v & 1 != 0 {
                    let (pa, reg) = (((v >> 21) & 31) as u8, ((v >> 16) & 31) as u8);
                    let goc = (v >> 2) & 3;
                    let data = self.r(MAC_MDIO_DATA) as u16;
                    match &mut self.phy {
                        Some((addr, phy)) if *addr == pa => match goc {
                            1 => phy.write(reg, data),
                            3 | 2 => {
                                let d = phy.read(reg);
                                self.set(MAC_MDIO_DATA, d as u32);
                            }
                            _ => {}
                        },
                        _ => {
                            if goc & 2 != 0 {
                                self.set(MAC_MDIO_DATA, 0xffff); // no PHY: the bus floats high
                            }
                        }
                    }
                }
                self.set(MAC_MDIO_ADDRESS, v & !1);
            }
            MAC_MDIO_DATA => self.set(MAC_MDIO_DATA, v & 0xffff),
            MTL_TXQ0_OPERATION_MODE => self.set(off, v & !1), // TX queue flush completes at once
            CH_STATUS => {
                let s = self.r(CH_STATUS) & !v;
                self.set(CH_STATUS, s);
            }
            CH_TX_LIST => {
                self.set(off, v);
                self.tx_index = 0;
            }
            CH_RX_LIST => {
                self.set(off, v);
                self.rx_index = 0;
            }
            CH_RX_CONTROL => {
                self.set(off, v);
                if v & 1 == 0 {
                    self.rx_queue.clear();
                }
                return true;
            }
            CH_TX_CONTROL | CH_TX_TAIL | CH_RX_TAIL => {
                self.set(off, v);
                return true;
            }
            _ => self.set(off, v),
        }
        false
    }

    pub fn receiving(&self) -> bool {
        self.r(CH_RX_CONTROL) & 1 != 0
    }

    fn ring(&self, list_hi: usize, list: usize, len: usize, index: u32) -> u64 {
        let base = (self.r(list_hi) as u64) << 32 | self.r(list) as u64;
        let count = (self.r(len) & 0x3ff) + 1;
        base + 16 * (index % count) as u64
    }

    fn word(ram: &FlatRam, addr: u64) -> Option<u32> {
        let o = addr.checked_sub(ram.base)? as usize;
        Some(u32::from_le_bytes(ram.mem.get(o..o.checked_add(4)?)?.try_into().ok()?))
    }

    fn put_word(ram: &mut FlatRam, addr: u64, v: u32) -> bool {
        ram.load(addr, &v.to_le_bytes()).is_ok()
    }

    fn bus_error(&mut self) {
        let s = self.r(CH_STATUS) | FBE;
        self.set(CH_STATUS, s);
        let (tx, rx) = (self.r(CH_TX_CONTROL) & !1, self.r(CH_RX_CONTROL) & !1);
        self.set(CH_TX_CONTROL, tx);
        self.set(CH_RX_CONTROL, rx);
        self.rx_queue.clear();
    }

    /// Send owned TX descriptors and fill owned RX descriptors with queued frames.
    pub fn service(&mut self, ram: &mut FlatRam) {
        if self.r(CH_TX_CONTROL) & 1 != 0 {
            let count = (self.r(CH_TX_RING_LEN) & 0x3ff) + 1;
            for _ in 0..count {
                let d = self.ring(CH_TX_LIST_HI, CH_TX_LIST, CH_TX_RING_LEN, self.tx_index);
                let Some(flags) = Self::word(ram, d + 12) else { self.bus_error(); break };
                if flags & OWN == 0 {
                    break;
                }
                self.set(CH_CUR_TX, d as u32);
                if flags & CTXT == 0 {
                    let (Some(b1), Some(b2), Some(lens)) = (Self::word(ram, d), Self::word(ram, d + 4), Self::word(ram, d + 8)) else {
                        self.bus_error();
                        break;
                    };
                    if flags & FD != 0 {
                        self.tx_partial.clear();
                    }
                    for (addr, len) in [(b1, lens & 0x3fff), (b2, (lens >> 16) & 0x3fff)] {
                        if len == 0 {
                            continue;
                        }
                        let o = (addr as u64).wrapping_sub(ram.base) as usize;
                        match ram.mem.get(o..o.saturating_add(len as usize)) {
                            Some(data) if self.tx_partial.len() + data.len() <= 65536 => self.tx_partial.extend_from_slice(data),
                            Some(_) => self.tx_partial.clear(),
                            None => {
                                self.bus_error();
                                return;
                            }
                        }
                    }
                    if flags & LD != 0 && !self.tx_partial.is_empty() {
                        self.tx_out.push(std::mem::take(&mut self.tx_partial));
                        self.tx_frames = self.tx_frames.wrapping_add(1);
                    }
                    Self::put_word(ram, d + 12, flags & (FD | LD));
                } else {
                    Self::put_word(ram, d + 12, CTXT);
                }
                self.tx_index = (self.tx_index + 1) % count;
                let s = self.r(CH_STATUS) | TI;
                self.set(CH_STATUS, s);
            }
        }
        if self.r(CH_RX_CONTROL) & 1 != 0 {
            let count = (self.r(CH_RX_RING_LEN) & 0x3ff) + 1;
            while let Some(frame) = self.rx_queue.front() {
                let d = self.ring(CH_RX_LIST_HI, CH_RX_LIST, CH_RX_RING_LEN, self.rx_index);
                let Some(flags) = Self::word(ram, d + 12) else { self.bus_error(); break };
                if flags & OWN == 0 {
                    let s = self.r(CH_STATUS) | RBU;
                    self.set(CH_STATUS, s);
                    break;
                }
                let Some(buf) = Self::word(ram, d) else { self.bus_error(); break };
                let capacity = ((self.r(CH_RX_CONTROL) >> 1) & 0x3fff) as usize;
                let len = frame.len() + 4;
                if len > capacity {
                    self.rx_queue.pop_front(); // larger than one buffer: dropped
                    continue;
                }
                let mut data = frame.clone();
                data.extend_from_slice(&[0; 4]); // FCS, stripped by the driver
                if ram.load(buf as u64, &data).is_err() {
                    self.bus_error();
                    break;
                }
                self.set(CH_CUR_RX, d as u32);
                Self::put_word(ram, d, 0);
                Self::put_word(ram, d + 4, 0);
                Self::put_word(ram, d + 8, 0);
                Self::put_word(ram, d + 12, FD | LD | len as u32);
                self.rx_queue.pop_front();
                self.rx_frames = self.rx_frames.wrapping_add(1);
                self.rx_index = (self.rx_index + 1) % count;
                let s = self.r(CH_STATUS) | RI;
                self.set(CH_STATUS, s);
            }
        }
    }

    /// Queue frames from the network (only while the receiver runs; bounded).
    pub fn deliver(&mut self, frames: Vec<Vec<u8>>) {
        if !self.receiving() {
            return;
        }
        for f in frames {
            if self.rx_queue.len() < 256 {
                self.rx_queue.push_back(f);
            }
        }
    }
}

/// Motorcomm YT8531C RGMII Gigabit PHY (Clause 22): identity, control/status, a
/// 1000BASE-T full-duplex link partner, and the extended-register window (0x1e/0x1f)
/// the motorcomm driver programs. Link follows the virtual cable.
pub struct Yt8531 {
    regs: [u16; 32],
    ext: std::collections::BTreeMap<u16, u16>,
    page: u16,
    pub cable: bool,
    latched_low: bool,
}

pub const YT8531_ID: u32 = 0x4f51_e91b;

impl Default for Yt8531 {
    fn default() -> Self {
        let mut p = Yt8531 { regs: [0; 32], ext: Default::default(), page: 0, cable: false, latched_low: true };
        p.reset();
        p
    }
}

impl Yt8531 {
    fn reset(&mut self) {
        self.regs = [0; 32];
        self.regs[0] = 0x1140;
        self.regs[4] = 0x01e1;
        self.regs[9] = 0x0300;
        self.ext.clear();
        // chip configuration: the RGMII LDO strapped to 1.8 V (bits 5:4 = 2), as the board's
        // device tree drive strengths (motorcomm,rx-clk-drv-microamp = 3970) require
        self.ext.insert(0xa001, 0x0020);
        self.page = 0;
    }

    fn link(&self) -> bool {
        self.cable && self.regs[0] & (1 << 11) == 0
    }

    pub fn set_link(&mut self, up: bool) {
        if self.link() && !up {
            self.latched_low = true;
        }
        self.cable = up;
    }

    pub fn read(&mut self, reg: u8) -> u16 {
        let link = self.link();
        match reg {
            1 => {
                let latched = std::mem::take(&mut self.latched_low);
                0x7909 | if link && !latched { 1 << 2 } else { 0 } | if link { 1 << 5 } else { 0 }
            }
            2 => (YT8531_ID >> 16) as u16,
            3 => YT8531_ID as u16,
            5 => if link { 0xc1e1 } else { 0 },
            10 => if link { 0x3c00 } else { 0 },
            15 => 0x3000,
            // PHY-specific status: 1000 Mbit/s, full duplex, resolved, link
            17 => if link { 0x8000 | 0x2000 | 0x0800 | 0x0400 } else { 0 },
            30 => self.page,
            31 => self.ext.get(&self.page).copied().unwrap_or(0),
            _ => self.regs[reg as usize & 31],
        }
    }

    pub fn write(&mut self, reg: u8, v: u16) {
        match reg {
            0 if v & 0x8000 != 0 => self.reset(),
            0 => self.regs[0] = v & !(1 << 9),
            1 | 2 | 3 | 5 | 10 | 15 | 17 => {}
            30 => self.page = v,
            31 => {
                self.ext.insert(self.page, v);
            }
            _ => self.regs[reg as usize & 31] = v,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const BASE: u64 = 0x4000_0000;

    fn put(ram: &mut FlatRam, a: u64, v: u32) {
        ram.load(a, &v.to_le_bytes()).unwrap();
    }

    #[test]
    fn mdio_reaches_the_phy_and_an_empty_bus_reads_high() {
        let mut g = Gmac::new(Some((0, Yt8531::default())));
        g.write(MAC_MDIO_ADDRESS as u64, (2 << 16) | (3 << 2) | 1);
        assert_eq!(g.read(MAC_MDIO_DATA as u64), (YT8531_ID >> 16));
        g.write(MAC_MDIO_ADDRESS as u64, (1 << 21) | (2 << 16) | (3 << 2) | 1);
        assert_eq!(g.read(MAC_MDIO_DATA as u64), 0xffff);
        assert_eq!(g.read(MAC_MDIO_ADDRESS as u64) & 1, 0);
    }

    #[test]
    fn transmit_and_receive_through_the_rings() {
        let mut ram = FlatRam::new(BASE, 0x10000);
        let mut g = Gmac::new(Some((0, Yt8531::default())));
        g.write(CH_INT_EN as u64, NIS | TI | RI);
        // TX ring of 4 at +0x100, one frame in two descriptors
        g.write(CH_TX_RING_LEN as u64, 3);
        g.write(CH_TX_LIST as u64, (BASE + 0x100) as u32);
        ram.load(BASE + 0x1000, b"hello ").unwrap();
        ram.load(BASE + 0x1100, b"world").unwrap();
        for (k, (buf, len, f)) in [(0x1000u64, 6u32, FD), (0x1100, 5, LD)].into_iter().enumerate() {
            let d = BASE + 0x100 + 16 * k as u64;
            put(&mut ram, d, (BASE + buf) as u32);
            put(&mut ram, d + 8, len);
            put(&mut ram, d + 12, OWN | f);
        }
        g.write(CH_TX_CONTROL as u64, 1);
        g.service(&mut ram);
        assert_eq!(g.tx_out, vec![b"hello world".to_vec()]);
        assert!(g.irq());
        assert_eq!(Gmac::word(&ram, BASE + 0x10c).unwrap() & OWN, 0);
        g.write(CH_STATUS as u64, TI);
        assert!(!g.irq());
        // RX ring of 2 at +0x200, 1536-byte buffers
        g.write(CH_RX_RING_LEN as u64, 1);
        g.write(CH_RX_LIST as u64, (BASE + 0x200) as u32);
        for k in 0..2u64 {
            put(&mut ram, BASE + 0x200 + 16 * k, (BASE + 0x2000 + 0x800 * k) as u32);
            put(&mut ram, BASE + 0x20c + 16 * k, OWN | (1 << 24));
        }
        g.write(CH_RX_CONTROL as u64, 1 | (1536 << 1));
        g.deliver(vec![vec![0xaa; 60]]);
        g.service(&mut ram);
        let d3 = Gmac::word(&ram, BASE + 0x20c).unwrap();
        assert_eq!(d3, FD | LD | 64);
        assert_eq!(ram.mem[0x2000], 0xaa);
        assert!(g.irq());
    }
}

// Snapshots
emu_core::snap_struct!(Yt8531 { regs, ext, page, cable, latched_low });
emu_core::snap_struct!(Gmac { tx_index, rx_index, tx_partial, tx_out, rx_queue, tx_frames, rx_frames } fixed { regs } fitted { phy: "a PHY" }
after |g| {
    // a descriptor ring has at most 1024 entries (the ring length is 10 bits)
    if g.tx_index >= 1024 || g.rx_index >= 1024 {
        return Err(emu_core::snap::SnapError(format!("Ethernet DMA descriptors {} and {} of at most 1024", g.tx_index, g.rx_index)));
    }
    Ok(())
});

#[cfg(test)]
mod snapshot_tests {
    /// A descriptor index past the longest ring is refused on restore (its increment would
    /// overflow).
    #[test]
    fn a_snapshot_with_an_impossible_ring_index_is_refused() {
        let mut m = crate::Machine::new(8 << 20);
        (m.soc.gmac0.tx_index, m.soc.gmac0.rx_index) = (1023, 7);
        crate::Machine::new(8 << 20).restore(&m.save().unwrap()).unwrap();
        assert!(crate::machine::refused(|m| m.soc.gmac0.tx_index = u32::MAX).contains("Ethernet DMA"));
        assert!(crate::machine::refused(|m| m.soc.gmac1.rx_index = 1024).contains("Ethernet DMA"));
    }
}
