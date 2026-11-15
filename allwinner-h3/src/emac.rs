//! H3 Ethernet MAC (`allwinner,sun8i-h3-emac`, Linux `dwmac-sun8i` on the stmmac core)
//! with the H3's internal 100 Mbit/s PHY on its MDIO bus.
//!
//! The MAC's DMA walks chained "normal" stmmac descriptors in RAM (the sun8i glue forces
//! chain mode): des0 status with OWN, des1 control and buffer size, des2 the buffer, des3
//! the next descriptor. Transmitted frames (checksums inserted as the descriptor asks)
//! go to `tx_out`; frames in `rx_queue` are written to the receive descriptors.
use crate::usb::Dma;
use std::collections::VecDeque;

pub const EMAC_BASE: u64 = 0x01c3_0000;
pub const EMAC_SIZE: u64 = 0x1_0000;
/// EMAC interrupt: SPI 82.
pub const EMAC_IRQ: usize = 32 + 82;

const BASIC_CTL1: usize = 0x04 / 4;
const INT_STA: usize = 0x08 / 4;
const INT_EN: usize = 0x0c / 4;
const TX_CTL0: usize = 0x10 / 4;
const TX_CTL1: usize = 0x14 / 4;
const TX_DESC_LIST: usize = 0x20 / 4;
const RX_CTL0: usize = 0x24 / 4;
const RX_CTL1: usize = 0x28 / 4;
const RX_DESC_LIST: usize = 0x34 / 4;
const MDIO_CMD: usize = 0x48 / 4;
const MDIO_DATA: usize = 0x4c / 4;
const TX_DMA_STA: usize = 0xb0 / 4;
const TX_CUR_DESC: usize = 0xb4 / 4;
const RX_DMA_STA: usize = 0xc0 / 4;
const RX_CUR_DESC: usize = 0xc4 / 4;
const REGS: usize = 0x100 / 4;

const ENABLE: u32 = 1 << 31; // TX_CTL0 transmitter / RX_CTL0 receiver enable
const DMA_START: u32 = 1 << 31;
const DMA_EN: u32 = 1 << 30;

const TX_INT: u32 = 1 << 0;
const RX_INT: u32 = 1 << 8;
const RX_BUF_UA_INT: u32 = 1 << 9;

const OWN: u32 = 1 << 31;
const BUFFER_SIZE: u32 = 0x7ff;
const TX_FIRST: u32 = 1 << 29;
const TX_LAST: u32 = 1 << 30;
const RX_FIRST: u32 = 1 << 9;
const RX_LAST: u32 = 1 << 8;
/// RDES0 frame type: an Ethernet (length/type >= 0x600) frame.
const RX_FRAME_TYPE: u32 = 1 << 5;

/// MDIO address of the internal PHY.
const PHY_ADDRESS: u32 = 1;
const MAX_FRAME: usize = 9018;
/// Receive frames waiting for descriptors beyond this are dropped (a full FIFO).
const RX_QUEUE_LIMIT: usize = 512;

pub struct Emac {
    regs: [u32; REGS],
    /// Internal PHY registers (clause 22).
    phy: [u16; 32],
    tx_desc: u32,
    rx_desc: u32,
    /// Frames the guest sent.
    pub tx_out: Vec<Vec<u8>>,
    /// Frames for the guest, waiting for receive descriptors.
    pub rx_queue: VecDeque<Vec<u8>>,
    pub tx_frames: u64,
    pub rx_frames: u64,
    /// Received frames too large for the guest's receive buffers.
    pub rx_dropped: u64,
}

impl Default for Emac {
    fn default() -> Self {
        let mut e = Emac {
            regs: [0; REGS],
            phy: [0; 32],
            tx_desc: 0,
            rx_desc: 0,
            tx_out: Vec::new(),
            rx_queue: VecDeque::new(),
            tx_frames: 0,
            rx_frames: 0,
            rx_dropped: 0,
        };
        e.reset();
        e.phy_reset();
        e
    }
}

impl Emac {
    fn reset(&mut self) {
        self.regs = [0; REGS];
        self.tx_desc = 0;
        self.rx_desc = 0;
    }

    /// The internal PHY after reset: 100BASE-TX full duplex, link up, autonegotiation done
    /// with a 10/100 full/half-duplex partner.
    fn phy_reset(&mut self) {
        self.phy = [0; 32];
        self.phy[0] = 0x3100; // BMCR: autonegotiation, 100 Mbit/s, full duplex
        self.phy[1] = 0x786d; // BMSR: 10/100 capable, autoneg complete, link up
        self.phy[2] = 0x0044; // PHY ID (the H3 EPHY)
        self.phy[3] = 0x1400;
        self.phy[4] = 0x01e1; // advertise 10/100 half/full
        self.phy[5] = 0x45e1; // partner: same, acknowledged
        self.phy[6] = 0x0001;
    }

    pub fn read(&self, offset: u64) -> u32 {
        let index = (offset / 4) as usize;
        match index {
            TX_CUR_DESC => self.tx_desc,
            RX_CUR_DESC => self.rx_desc,
            TX_DMA_STA => (self.regs[TX_CTL1] & DMA_EN != 0) as u32 * 6, // suspended: waiting for descriptors
            RX_DMA_STA => (self.regs[RX_CTL1] & DMA_EN != 0) as u32 * 3, // waiting for frames
            _ if index < REGS => self.regs[index],
            _ => 0,
        }
    }

    pub fn write(&mut self, offset: u64, value: u32, dma: &mut Dma) {
        let index = (offset / 4) as usize;
        match index {
            BASIC_CTL1 => {
                if value & 1 != 0 {
                    self.reset(); // soft reset, done at once
                }
                self.regs[index] = value & !1;
            }
            INT_STA => self.regs[index] &= !value,
            TX_DESC_LIST => {
                self.regs[index] = value;
                self.tx_desc = value;
            }
            RX_DESC_LIST => {
                self.regs[index] = value;
                self.rx_desc = value;
            }
            MDIO_CMD => {
                self.regs[index] = value & !1;
                if value & 1 != 0 {
                    self.mdio(value);
                }
            }
            TX_CTL1 => {
                self.regs[index] = value & !DMA_START;
                if value & DMA_START != 0 {
                    self.transmit(dma);
                }
            }
            RX_CTL1 => {
                self.regs[index] = value & !DMA_START;
                if value & DMA_START != 0 {
                    self.receive(dma);
                }
            }
            TX_DMA_STA | TX_CUR_DESC | RX_DMA_STA | RX_CUR_DESC => {}
            _ if index < REGS => self.regs[index] = value,
            _ => {}
        }
    }

    /// One MDIO transaction (`MDIO_CMD`: PHY address 16:12, register 8:4, write bit 1).
    fn mdio(&mut self, cmd: u32) {
        let address = cmd >> 12 & 0x1f;
        let reg = (cmd >> 4 & 0x1f) as usize;
        let write = cmd & 2 != 0;
        if address != PHY_ADDRESS {
            if !write {
                self.regs[MDIO_DATA] = 0xffff; // nobody answers
            }
            return;
        }
        if write {
            let value = self.regs[MDIO_DATA] as u16;
            match reg {
                0 if value & 0x8000 != 0 => self.phy_reset(),
                0 => self.phy[0] = value & !0x0200, // restart autonegotiation completes at once
                1 | 2 | 3 | 5 => {}
                _ => self.phy[reg] = value,
            }
        } else {
            self.regs[MDIO_DATA] = self.phy[reg] as u32;
        }
    }

    pub fn irq(&self) -> bool {
        self.regs[INT_STA] & self.regs[INT_EN] != 0
    }

    fn tx_running(&self) -> bool {
        self.regs[TX_CTL0] & ENABLE != 0 && self.regs[TX_CTL1] & DMA_EN != 0
    }

    /// Frames are waiting for the guest's receive descriptors.
    pub fn rx_pending(&self) -> bool {
        self.rx_running() && !self.rx_queue.is_empty()
    }

    fn rx_running(&self) -> bool {
        self.regs[RX_CTL0] & ENABLE != 0 && self.regs[RX_CTL1] & DMA_EN != 0
    }

    /// Send every frame the guest handed over (descriptors it owns).
    pub fn transmit(&mut self, dma: &mut Dma) {
        if !self.tx_running() {
            return;
        }
        let mut frame = Vec::new();
        let mut checksum = 0;
        for _ in 0..4096 {
            let desc = self.tx_desc;
            let status = dma.read32(desc);
            if status & OWN == 0 || desc == 0 {
                break;
            }
            let control = dma.read32(desc + 4);
            let buffer = dma.read32(desc + 8);
            let next = dma.read32(desc + 12);
            if control & TX_FIRST != 0 {
                frame.clear();
                checksum = control >> 27 & 3;
            }
            let len = (control & BUFFER_SIZE) as usize;
            if frame.len() + len <= MAX_FRAME {
                let start = frame.len();
                frame.resize(start + len, 0);
                dma.read(buffer, &mut frame[start..]);
            }
            dma.write32(desc, 0); // done, no errors
            self.tx_desc = next;
            if control & TX_LAST != 0 {
                let mut out = std::mem::take(&mut frame);
                if checksum != 0 {
                    insert_checksums(&mut out, checksum);
                }
                self.tx_out.push(out);
                self.tx_frames += 1;
                self.regs[INT_STA] |= TX_INT;
            }
        }
    }

    /// Write waiting frames into the receive descriptors the guest owns.
    pub fn receive(&mut self, dma: &mut Dma) {
        if self.rx_queue.len() > RX_QUEUE_LIMIT {
            let excess = self.rx_queue.len() - RX_QUEUE_LIMIT;
            self.rx_queue.drain(..excess);
        }
        if !self.rx_running() {
            return;
        }
        while let Some(frame) = self.rx_queue.front() {
            let desc = self.rx_desc;
            if desc == 0 || dma.read32(desc) & OWN == 0 {
                self.regs[INT_STA] |= RX_BUF_UA_INT;
                break;
            }
            // The frame and its FCS (stmmac strips four bytes from the reported length)
            // go into one buffer: Linux sizes its buffers for the MTU and passes the
            // pieces of a frame spread over several descriptors up as separate packets,
            // so a frame that does not fit is dropped, as the MAC drops oversized frames.
            let size = (dma.read32(desc + 4) & BUFFER_SIZE) as usize;
            if frame.len() + 4 > size {
                self.rx_queue.pop_front();
                self.rx_dropped += 1;
                continue;
            }
            let fcs = crc32(frame).to_le_bytes();
            let buffer = dma.read32(desc + 8);
            dma.write(buffer, frame);
            dma.write(buffer.wrapping_add(frame.len() as u32), &fcs);
            dma.write32(desc, RX_FRAME_TYPE | RX_FIRST | RX_LAST | ((frame.len() + 4) as u32) << 16);
            self.rx_desc = dma.read32(desc + 12);
            self.rx_queue.pop_front();
            self.rx_frames += 1;
            self.regs[INT_STA] |= RX_INT;
        }
    }
}

/// Fill in the checksums a transmit descriptor asks for (CIC 1: IPv4 header; 2-3: also
/// the TCP/UDP checksum including the pseudo-header).
fn insert_checksums(frame: &mut [u8], cic: u32) {
    let mut l3 = 14;
    if frame.len() < l3 {
        return;
    }
    let mut ethertype = u16::from_be_bytes([frame[12], frame[13]]);
    if ethertype == 0x8100 && frame.len() >= 18 {
        ethertype = u16::from_be_bytes([frame[16], frame[17]]);
        l3 = 18;
    }
    let (proto, l4, pseudo) = match ethertype {
        0x0800 => {
            let Some(&vihl) = frame.get(l3) else { return };
            let ihl = ((vihl & 0xf) as usize) * 4;
            if ihl < 20 || frame.len() < l3 + ihl {
                return;
            }
            frame[l3 + 10] = 0;
            frame[l3 + 11] = 0;
            let sum = !fold(sum16(&frame[l3..l3 + ihl], 0));
            frame[l3 + 10..l3 + 12].copy_from_slice(&sum.to_be_bytes());
            let total = (u16::from_be_bytes([frame[l3 + 2], frame[l3 + 3]]) as usize).min(frame.len() - l3);
            if total < ihl {
                return;
            }
            let length = (total - ihl) as u32;
            let proto = frame[l3 + 9];
            let pseudo = sum16(&frame[l3 + 12..l3 + 20], 0) + proto as u32 + length;
            (proto, (l3 + ihl, l3 + total), pseudo)
        }
        0x86dd => {
            if frame.len() < l3 + 40 {
                return;
            }
            let length = (u16::from_be_bytes([frame[l3 + 4], frame[l3 + 5]]) as usize).min(frame.len() - l3 - 40);
            let proto = frame[l3 + 6];
            let pseudo = sum16(&frame[l3 + 8..l3 + 40], 0) + proto as u32 + length as u32;
            (proto, (l3 + 40, l3 + 40 + length), pseudo)
        }
        _ => return,
    };
    if cic < 2 {
        return;
    }
    let (start, end) = l4;
    let field = match proto {
        6 => 16,
        17 => 6,
        _ => return,
    };
    if end < start + field + 2 {
        return;
    }
    frame[start + field] = 0;
    frame[start + field + 1] = 0;
    let mut sum = !fold(sum16(&frame[start..end], pseudo));
    if proto == 17 && sum == 0 {
        sum = 0xffff;
    }
    frame[start + field..start + field + 2].copy_from_slice(&sum.to_be_bytes());
}

fn sum16(data: &[u8], init: u32) -> u32 {
    let mut sum = init;
    for pair in data.chunks(2) {
        sum += (pair[0] as u32) << 8 | pair.get(1).copied().unwrap_or(0) as u32;
        sum = (sum & 0xffff) + (sum >> 16);
    }
    sum
}

fn fold(mut sum: u32) -> u16 {
    while sum >> 16 != 0 {
        sum = (sum & 0xffff) + (sum >> 16);
    }
    sum as u16
}

/// Ethernet FCS (CRC-32, reflected, as transmitted little-endian).
fn crc32(data: &[u8]) -> u32 {
    let mut crc = !0u32;
    for &b in data {
        crc ^= b as u32;
        for _ in 0..8 {
            crc = if crc & 1 != 0 { crc >> 1 ^ 0xedb8_8320 } else { crc >> 1 };
        }
    }
    !crc
}

#[cfg(test)]
mod tests {
    use super::*;

    const BASE: u64 = 0x4000_0000;

    fn dma(mem: &mut [u8]) -> Dma<'_> {
        Dma { mem, base: BASE }
    }

    #[test]
    fn mdio_reads_the_internal_phy() {
        let mut mem = vec![0u8; 0x100];
        let mut e = Emac::default();
        let read = |e: &mut Emac, mem: &mut [u8], phy: u32, reg: u32| {
            e.write(0x48, phy << 12 | reg << 4 | 1, &mut dma(mem));
            assert_eq!(e.read(0x48) & 1, 0, "not busy");
            e.read(0x4c)
        };
        assert_eq!(read(&mut e, &mut mem, 1, 2) << 16 | read(&mut e, &mut mem, 1, 3), 0x0044_1400);
        assert_ne!(read(&mut e, &mut mem, 1, 1) & 4, 0, "link up");
        assert_eq!(read(&mut e, &mut mem, 3, 2), 0xffff, "no PHY at 3");
    }

    fn start(e: &mut Emac, mem: &mut [u8]) {
        let mut d = dma(mem);
        e.write(0x10, ENABLE, &mut d);
        e.write(0x24, ENABLE, &mut d);
        e.write(0x0c, TX_INT | RX_INT, &mut d);
    }

    #[test]
    fn transmit_gathers_a_two_descriptor_frame_and_inserts_checksums() {
        let mut mem = vec![0u8; 0x4000];
        let mut e = Emac::default();
        start(&mut e, &mut mem);
        // an IPv4/UDP frame from 10.0.2.15:68 to 10.0.2.2:67 with zeroed checksums
        let mut frame = vec![0xffu8; 6];
        frame.extend_from_slice(&[2, 0, 0, 0, 0, 1, 0x08, 0x00]);
        let payload = b"hello";
        let ip_len = 20 + 8 + payload.len();
        frame.extend_from_slice(&[0x45, 0, 0, ip_len as u8, 0, 0, 0, 0, 64, 17, 0, 0, 10, 0, 2, 15, 10, 0, 2, 2]);
        frame.extend_from_slice(&[0, 68, 0, 67, 0, (8 + payload.len()) as u8, 0, 0]);
        frame.extend_from_slice(payload);
        let (d0, d1, b0, b1) = (0x4000_1000u32, 0x4000_1010u32, 0x4000_2000u32, 0x4000_3000u32);
        let off = |a: u32| (a as u64 - BASE) as usize;
        mem[off(b0)..off(b0) + 20].copy_from_slice(&frame[..20]);
        mem[off(b1)..off(b1) + frame.len() - 20].copy_from_slice(&frame[20..]);
        {
            let mut d = dma(&mut mem);
            d.write32(d0, OWN);
            d.write32(d0 + 4, TX_FIRST | 3 << 27 | 20);
            d.write32(d0 + 8, b0);
            d.write32(d0 + 12, d1);
            d.write32(d1, OWN);
            d.write32(d1 + 4, TX_LAST | (frame.len() as u32 - 20));
            d.write32(d1 + 8, b1);
            d.write32(d1 + 12, 0x4000_1020);
            e.write(0x20, d0, &mut d);
            e.write(0x14, DMA_EN | DMA_START, &mut d);
        }
        assert_eq!(e.tx_out.len(), 1);
        let sent = &e.tx_out[0];
        assert_eq!(sent.len(), frame.len());
        assert_eq!(fold(sum16(&sent[14..34], 0)), 0xffff, "IPv4 header checksum");
        let pseudo = sum16(&sent[26..34], 0) + 17 + (8 + payload.len()) as u32;
        assert_eq!(fold(sum16(&sent[34..], pseudo)), 0xffff, "UDP checksum");
        assert!(e.irq());
        assert_eq!(dma(&mut mem).read32(d0) & OWN, 0, "descriptor returned");
        assert_eq!(e.read(0xb4), 0x4000_1020);
    }

    #[test]
    fn receive_fills_owned_descriptors_with_frame_and_fcs() {
        let mut mem = vec![0u8; 0x4000];
        let mut e = Emac::default();
        start(&mut e, &mut mem);
        let (d0, d1, b0) = (0x4000_1000u32, 0x4000_1010u32, 0x4000_2000u32);
        {
            let mut d = dma(&mut mem);
            for (desc, next, buf) in [(d0, d1, b0), (d1, d0, b0 + 0x800)] {
                d.write32(desc, OWN);
                d.write32(desc + 4, 1536 | 1 << 24);
                d.write32(desc + 8, buf);
                d.write32(desc + 12, next);
            }
            e.write(0x34, d0, &mut d);
            e.write(0x28, DMA_EN | DMA_START, &mut d);
        }
        let frame: Vec<u8> = (0..60u8).collect();
        e.rx_queue.push_back(frame.clone());
        e.rx_queue.push_back(frame.clone());
        e.rx_queue.push_back(frame.clone());
        e.receive(&mut dma(&mut mem));
        let status = dma(&mut mem).read32(d0);
        assert_eq!(status & OWN, 0);
        assert_eq!(status >> 16 & 0x3fff, 64, "length includes the FCS");
        assert_eq!(status & (RX_FIRST | RX_LAST), RX_FIRST | RX_LAST);
        let at = (b0 as u64 - BASE) as usize;
        assert_eq!(&mem[at..at + 60], &frame[..]);
        assert_eq!(e.rx_queue.len(), 1, "third frame waits for a descriptor");
        assert_ne!(e.read(0x08) & RX_BUF_UA_INT, 0);
        // a frame larger than the buffer is dropped, not split
        dma(&mut mem).write32(d0, OWN);
        e.rx_queue.clear();
        e.rx_queue.push_back(vec![0; 1600]);
        e.receive(&mut dma(&mut mem));
        assert_eq!((e.rx_dropped, e.rx_queue.len()), (1, 0));
        assert_ne!(dma(&mut mem).read32(d0) & OWN, 0, "descriptor still the guest's");
        assert_eq!(crc32(b"123456789"), 0xcbf4_3926);
    }
}
