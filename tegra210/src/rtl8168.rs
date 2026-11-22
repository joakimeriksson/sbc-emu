//! Realtek RTL8111H Gigabit Ethernet (`10ec:8168` revision 0x15, MAC version XID 0x541) as Linux's
//! `r8169` drives it: the 256-byte register window, the indirect spaces behind it (PHY and MAC
//! OCP, ERI, CSI, EPHY), the normal-priority transmit ring and the receive ring with 16-byte
//! descriptors, transmit checksum and segmentation offload, the receive checksum flags, and the
//! interrupt status. The PHY is the chip's own (`0x001cc800`), with a Gigabit link partner.
//!
//! Not modelled: the high-priority ring, wake-on-LAN, VLAN tagging, jumbo frames, flow control.
use crate::sdhci::Dma;
use std::collections::{HashMap, VecDeque};

pub const SIZE: u64 = 0x1000;

const COUNTER_LO: usize = 0x10;
const COUNTER_HI: usize = 0x14;
const TX_LO: usize = 0x20;
const TX_HI: usize = 0x24;
const CHIP_CMD: usize = 0x37;
const TX_POLL: usize = 0x38;
const INTR_MASK: usize = 0x3c;
const INTR_STATUS: usize = 0x3e;
const TX_CONFIG: usize = 0x40;
const RX_CONFIG: usize = 0x44;
const CSIDR: usize = 0x64;
const CSIAR: usize = 0x68;
const PHY_STATUS: usize = 0x6c;
const ERIDR: usize = 0x70;
const ERIAR: usize = 0x74;
const EPHYAR: usize = 0x80;
const OCPDR: usize = 0xb0;
const GPHY_OCP: usize = 0xb8;
const MCU: usize = 0xd3;
const EFUSEAR: usize = 0xdc;
const RX_LO: usize = 0xe4;
const RX_HI: usize = 0xe8;

const FLAG: u32 = 1 << 31;
/// TxConfig: XID 0x541, and the transmit FIFO is empty
const TX_CONFIG_FIXED: u32 = 0x541 << 20 | 1 << 11;
const TX_CONFIG_XID: u32 = 0xfcf << 20 | 1 << 11;

const RX_OK: u16 = 1 << 0;
const TX_OK: u16 = 1 << 2;
const RX_OVERFLOW: u16 = 1 << 4;
const LINK_CHANGE: u16 = 1 << 5;

const OWN: u32 = 1 << 31;
const RING_END: u32 = 1 << 30;
const FIRST: u32 = 1 << 29;
const LAST: u32 = 1 << 28;

/// The PHY's Clause 22 registers in OCP space.
const PHY_BASE: u32 = 0xa400;
const PHY_ID: u32 = 0x001c_c800;

pub struct Rtl8168 {
    regs: [u8; 256],
    /// OCP, ERI, CSI and EPHY registers by (space, address)
    indirect: HashMap<(u8, u32), u32>,
    phy: [u16; 32],
    cable: bool,
    mac: [u8; 6],
    tx_index: u64,
    rx_index: u64,
    partial: Vec<u8>,
    /// frames sent by the guest, for the network
    pub tx_out: Vec<Vec<u8>>,
    /// frames waiting for a free receive descriptor
    pub rx_queue: VecDeque<Vec<u8>>,
    pub tx_frames: u64,
    pub rx_frames: u64,
}

impl Rtl8168 {
    pub fn new(mac: [u8; 6]) -> Self {
        let mut s = Self { regs: [0; 256], indirect: HashMap::new(), phy: [0; 32], cable: false, mac, tx_index: 0, rx_index: 0, partial: Vec::new(), tx_out: Vec::new(), rx_queue: VecDeque::new(), tx_frames: 0, rx_frames: 0 };
        s.power_on();
        s
    }

    fn power_on(&mut self) {
        self.regs = [0; 256];
        self.regs[..6].copy_from_slice(&self.mac);
        self.put32(TX_CONFIG, TX_CONFIG_FIXED);
        self.indirect.clear();
        self.phy = [0; 32];
        self.phy[0] = 0x1140;
        self.phy[4] = 0x01e1;
        self.phy[9] = 0x0300;
        self.reset();
    }

    /// The bus reset (PERST#).
    pub fn bus_reset(&mut self) {
        self.power_on();
    }

    /// CmdReset: the rings and the interrupts; the MAC address and the PHY stay.
    fn reset(&mut self) {
        self.regs[CHIP_CMD] = 0;
        self.put32(INTR_MASK, 0);
        self.tx_index = 0;
        self.rx_index = 0;
        self.partial.clear();
        self.rx_queue.clear();
    }

    /// Plug a cable to a Gigabit link partner in, or out.
    pub fn set_link(&mut self, up: bool) {
        if std::mem::replace(&mut self.cable, up) != up {
            self.raise(LINK_CHANGE);
        }
    }

    fn link(&self) -> bool {
        self.cable && self.phy[0] & 1 << 11 == 0 && self.phy[0] & 1 << 10 == 0
    }

    fn get32(&self, r: usize) -> u32 {
        u32::from_le_bytes(self.regs[r..r + 4].try_into().unwrap())
    }

    fn put32(&mut self, r: usize, v: u32) {
        self.regs[r..r + 4].copy_from_slice(&v.to_le_bytes());
    }

    fn status(&self) -> u16 {
        u16::from_le_bytes([self.regs[INTR_STATUS], self.regs[INTR_STATUS + 1]])
    }

    fn set_status(&mut self, v: u16) {
        self.regs[INTR_STATUS..INTR_STATUS + 2].copy_from_slice(&v.to_le_bytes());
    }

    fn raise(&mut self, bits: u16) {
        let v = self.status() | bits;
        self.set_status(v);
    }

    /// The interrupt's level (a message is sent when it rises).
    pub fn irq(&self) -> bool {
        self.status() & u16::from_le_bytes([self.regs[INTR_MASK], self.regs[INTR_MASK + 1]]) != 0
    }

    /// The negotiated (or forced) speed in Mbit/s and whether it is full duplex; the partner offers
    /// everything.
    fn speed(&self) -> (u32, bool) {
        let c = self.phy[0];
        if c & 1 << 12 == 0 {
            let speed = match (c >> 6 & 1, c >> 13 & 1) { (1, 0) => 1000, (_, 1) => 100, _ => 10 };
            return (speed, c & 1 << 8 != 0);
        }
        let (a, g) = (self.phy[4], self.phy[9]);
        match () {
            _ if g & 1 << 9 != 0 => (1000, true),
            _ if g & 1 << 8 != 0 => (1000, false),
            _ if a & 1 << 8 != 0 => (100, true),
            _ if a & 1 << 7 != 0 => (100, false),
            _ => (10, a & 1 << 6 != 0),
        }
    }

    fn phy_read(&self, reg: usize) -> u16 {
        let link = self.link();
        match reg {
            1 => 0x7949 | if link { 1 << 5 | 1 << 2 } else { 0 },
            2 => (PHY_ID >> 16) as u16,
            3 => PHY_ID as u16,
            5 if link => 0xc1e1,
            6 if link => 0x0d,
            10 if link => 0x3c00,
            5 | 6 | 10 => 0,
            15 => 0x2000, // 1000BASE-T full duplex
            _ => self.phy[reg],
        }
    }

    fn phy_write(&mut self, reg: usize, v: u16) {
        match reg {
            0 => {
                let was = self.link();
                let restarted = v & 1 << 15 != 0 || v & 1 << 9 != 0;
                self.phy[0] = if v & 1 << 15 != 0 { 0x1140 } else { v & !(1 << 9) };
                if restarted && self.link() || was != self.link() {
                    self.raise(LINK_CHANGE); // negotiated at once
                }
            }
            1..=3 | 5 | 6 | 10 | 15 => {}
            _ => self.phy[reg] = v,
        }
    }

    fn ocp_read(&self, reg: u32) -> u16 {
        match reg {
            // page 0xa43: the speed and duplex
            0xa434 if self.link() => {
                let (speed, full) = self.speed();
                (match speed { 1000 => 0x20, 100 => 0x10, _ => 0 }) | 4 | (full as u16) << 3
            }
            0xa434 => 0,
            r if (PHY_BASE..PHY_BASE + 0x40).contains(&r) => self.phy_read((r - PHY_BASE) as usize / 2),
            r => self.indirect.get(&(b'o', r)).copied().unwrap_or(0) as u16,
        }
    }

    /// An indirect space's command register was written: `write` bit 31 asks for a write, which
    /// clears the bit when it is done; a read sets it.
    fn indirect(&mut self, space: u8, v: u32, address: u32, data: usize, in_command: bool) -> u32 {
        if v & FLAG != 0 {
            let d = if in_command { v & 0xffff } else { self.get32(data) };
            self.indirect.insert((space, address), d);
            return v & !FLAG;
        }
        let d = self.indirect.get(&(space, address)).copied().unwrap_or(0);
        if in_command {
            return v & 0xffff_0000 | d & 0xffff | FLAG;
        }
        self.put32(data, d);
        v | FLAG
    }

    pub fn read(&mut self, offset: u64, size: usize) -> u32 {
        let o = offset as usize;
        if o + size > 256 {
            return 0;
        }
        let mut w = [0; 4];
        for (k, b) in w[..size].iter_mut().enumerate() {
            *b = match o + k {
                // always ready, both FIFOs empty, link list ready
                MCU => self.regs[MCU] & !(1 << 7) | 1 << 5 | 1 << 4 | 1 << 1,
                PHY_STATUS if self.link() => {
                    let (speed, full) = self.speed();
                    0x62 | (match speed { 1000 => 0x10, 100 => 0x08, _ => 0x04 }) | full as u8
                }
                PHY_STATUS => 0,
                // commands are done at once; the receive buffer is empty
                CHIP_CMD => self.regs[CHIP_CMD] | 1,
                r => self.regs[r],
            };
        }
        u32::from_le_bytes(w)
    }

    pub fn write(&mut self, offset: u64, value: u32, size: usize, dma: &mut Dma) {
        let o = offset as usize;
        if o + size > 256 {
            return;
        }
        let bytes = value.to_le_bytes();
        let touches = |r: usize, n: usize| o < r + n && r < o + size;
        // the bytes that act on being written
        for k in 0..size {
            let (r, b) = (o + k, bytes[k]);
            match r {
                _ if r == INTR_STATUS || r == INTR_STATUS + 1 => self.regs[r] &= !b,
                CHIP_CMD => {
                    if b & 0x10 != 0 {
                        self.reset();
                    } else {
                        self.regs[CHIP_CMD] = b & !0x10;
                    }
                }
                TX_POLL => self.regs[TX_POLL] = b & !0xc0,
                MCU | PHY_STATUS => self.regs[r] = b,
                _ => self.regs[r] = b,
            }
        }
        if touches(TX_CONFIG, 4) {
            let v = self.get32(TX_CONFIG) & !TX_CONFIG_XID | TX_CONFIG_FIXED;
            self.put32(TX_CONFIG, v);
        }
        if touches(TX_LO, 4) {
            self.tx_index = 0;
        }
        if touches(RX_LO, 4) {
            self.rx_index = 0;
        }
        if size == 4 {
            match o {
                GPHY_OCP => {
                    let reg = value >> 15 & 0xfffe;
                    let done = if value & FLAG != 0 {
                        if (PHY_BASE..PHY_BASE + 0x40).contains(&reg) && reg != 0xa434 {
                            self.phy_write((reg - PHY_BASE) as usize / 2, value as u16);
                        } else {
                            self.indirect.insert((b'o', reg), value & 0xffff);
                        }
                        value & !FLAG
                    } else {
                        value & 0xffff_0000 | self.ocp_read(reg) as u32 | FLAG
                    };
                    self.put32(GPHY_OCP, done);
                }
                OCPDR => {
                    let reg = value >> 15 & 0xfffe;
                    if value & FLAG != 0 {
                        self.indirect.insert((b'm', reg), value & 0xffff);
                    } else {
                        let d = self.indirect.get(&(b'm', reg)).copied().unwrap_or(0);
                        self.put32(OCPDR, d);
                    }
                }
                ERIAR => {
                    // the byte enables (15:12) select which of the word's bytes change
                    let (address, enables) = (value & 0x30fff, value >> 12 & 15);
                    if value & FLAG != 0 {
                        let mask = (0..4).filter(|k| enables >> k & 1 != 0).fold(0u32, |m, k| m | 0xff << (8 * k));
                        let old = self.indirect.get(&(b'e', address)).copied().unwrap_or(0);
                        let d = old & !mask | self.get32(ERIDR) & mask;
                        self.indirect.insert((b'e', address), d);
                        self.put32(ERIAR, value & !FLAG);
                    } else {
                        let d = self.indirect(b'e', value, address, ERIDR, false);
                        self.put32(ERIAR, d);
                    }
                }
                CSIAR => {
                    let d = self.indirect(b'c', value, value & 0xfff, CSIDR, false);
                    self.put32(CSIAR, d);
                }
                EPHYAR => {
                    let d = self.indirect(b'p', value, value >> 16 & 0x7f, 0, true);
                    self.put32(EPHYAR, d);
                }
                EFUSEAR => self.put32(EFUSEAR, if value & FLAG != 0 { value & !FLAG } else { value | FLAG | 0xff }),
                COUNTER_LO => {
                    if value & 8 != 0 {
                        let a = (self.get32(COUNTER_HI) as u64) << 32 | (value & !63) as u64;
                        let mut dump = [0; 64];
                        dump[..8].copy_from_slice(&self.tx_frames.to_le_bytes());
                        dump[8..16].copy_from_slice(&self.rx_frames.to_le_bytes());
                        if let Some(m) = dma.slice(a, 64) {
                            m.copy_from_slice(&dump);
                        }
                    }
                    self.put32(COUNTER_LO, value & !9);
                }
                _ => {}
            }
        }
        if touches(CHIP_CMD, 1) || touches(TX_POLL, 1) {
            self.service(dma);
        }
    }

    pub fn receiving(&self) -> bool {
        self.regs[CHIP_CMD] & 8 != 0
    }

    /// Queue frames from the network (only while the receiver runs; bounded).
    pub fn deliver(&mut self, frames: Vec<Vec<u8>>) {
        if self.receiving() && self.link() {
            for f in frames {
                if self.rx_queue.len() < 256 {
                    self.rx_queue.push_back(f);
                }
            }
        }
    }

    fn accepts(&self, f: &[u8]) -> bool {
        let config = self.get32(RX_CONFIG);
        if f.len() < 14 {
            return false;
        }
        config & 1 != 0
            || config & 2 != 0 && f[..6] == self.regs[..6]
            || config & 8 != 0 && f[..6] == [0xff; 6]
            || config & 4 != 0 && f[0] & 1 != 0 && f[..6] != [0xff; 6]
    }

    /// Run both rings.
    pub fn service(&mut self, dma: &mut Dma) {
        self.transmit(dma);
        self.receive(dma);
    }

    fn descriptor(&self, lo: usize, hi: usize, index: u64) -> u64 {
        ((self.get32(hi) as u64) << 32 | (self.get32(lo) & !0xff) as u64) + 16 * index
    }

    fn transmit(&mut self, dma: &mut Dma) {
        if self.regs[CHIP_CMD] & 4 == 0 {
            return;
        }
        for _ in 0..1024 {
            let at = self.descriptor(TX_LO, TX_HI, self.tx_index);
            let Some(d) = dma.slice(at, 16) else { return };
            let word = |k: usize| u32::from_le_bytes(d[k..k + 4].try_into().unwrap());
            let (opts1, opts2, addr) = (word(0), word(4), word(8) as u64 | (word(12) as u64) << 32);
            if opts1 & OWN == 0 {
                return;
            }
            d[..4].copy_from_slice(&(opts1 & !OWN).to_le_bytes());
            self.tx_index = if opts1 & RING_END != 0 { 0 } else { self.tx_index + 1 };
            if opts1 & FIRST != 0 {
                self.partial.clear();
            }
            if let Some(data) = dma.slice(addr, (opts1 & 0xffff) as usize) {
                if self.partial.len() + data.len() <= 0x1_0000 + 256 {
                    self.partial.extend_from_slice(data);
                }
            }
            if opts1 & LAST != 0 {
                let frame = std::mem::take(&mut self.partial);
                if self.link() {
                    self.tx_out.extend(offload(frame, opts1, opts2));
                    self.tx_frames += 1;
                }
                self.raise(TX_OK);
            }
        }
    }

    fn receive(&mut self, dma: &mut Dma) {
        while self.receiving() {
            let Some(frame) = self.rx_queue.front() else { return };
            if !self.accepts(frame) {
                self.rx_queue.pop_front();
                continue;
            }
            let at = self.descriptor(RX_LO, RX_HI, self.rx_index);
            let Some(d) = dma.slice(at, 16) else { return };
            let word = |k: usize| u32::from_le_bytes(d[k..k + 4].try_into().unwrap());
            let (opts1, addr) = (word(0), word(8) as u64 | (word(12) as u64) << 32);
            if opts1 & OWN == 0 {
                self.raise(RX_OVERFLOW);
                return;
            }
            let frame = self.rx_queue.pop_front().unwrap();
            let len = frame.len().max(60);
            if len + 4 > (opts1 & 0x3fff) as usize {
                continue; // larger than one buffer: dropped
            }
            let Some(buf) = dma.slice(addr, len + 4) else { continue };
            buf[..frame.len()].copy_from_slice(&frame);
            buf[frame.len()..].fill(0); // padding and the FCS, which the driver strips
            // the protocol seen, with no checksum failure
            let proto = match (frame.get(12..14), frame.get(23), frame.get(20)) {
                (Some([8, 0]), Some(6), _) => 1 << 17,
                (Some([8, 0]), Some(17), _) => 1 << 18,
                (Some([0x86, 0xdd]), _, Some(6)) => 1 << 17,
                (Some([0x86, 0xdd]), _, Some(17)) => 1 << 18,
                (Some([8, 0]), ..) => 3 << 17,
                _ => 0,
            };
            let d = dma.slice(at, 16).unwrap();
            d[..4].copy_from_slice(&(opts1 & RING_END | FIRST | LAST | proto | (len as u32 + 4)).to_le_bytes());
            d[4..8].fill(0);
            self.rx_index = if opts1 & RING_END != 0 { 0 } else { self.rx_index + 1 };
            self.rx_frames += 1;
            self.raise(RX_OK);
        }
    }
}

fn sum(mut s: u32, data: &[u8]) -> u32 {
    for c in data.chunks(2) {
        s += (c[0] as u32) << 8 | *c.get(1).unwrap_or(&0) as u32;
        s = (s & 0xffff) + (s >> 16);
    }
    s
}

/// The Internet checksum of a transport segment with its pseudo header; `ip` is the IP header.
fn transport_checksum(ip: &[u8], v4: bool, proto: u8, seg: &[u8]) -> u16 {
    let s = if v4 { sum(0, &ip[12..20]) } else { sum(0, &ip[8..40]) };
    !(sum(s + proto as u32 + seg.len() as u32, seg) as u16)
}

/// Where the IP header starts and whether it is IPv4 (None: not IP, or cut short); an 802.1Q tag
/// may be in front of it.
fn ip_header(f: &[u8]) -> Option<(usize, bool)> {
    let tagged = f.get(12..14)? == [0x81, 0];
    let at = if tagged { 18 } else { 14 };
    let v4 = match f.get(at - 2..at)? {
        [8, 0] => true,
        [0x86, 0xdd] => false,
        _ => return None,
    };
    let header = f.get(at..at + if v4 { 20 } else { 40 })?;
    (header[0] >> 4 == if v4 { 4 } else { 6 }).then_some((at, v4))
}

/// What the transmit descriptor asks for: checksums filled in, or the frame cut into segments. A
/// frame that is not what the descriptor says goes out as it is.
fn offload(mut f: Vec<u8>, opts1: u32, opts2: u32) -> Vec<Vec<u8>> {
    let gso = opts1 & (1 << 25 | 1 << 26) != 0;
    let tcp = gso || opts2 & 1 << 30 != 0;
    let want_v4 = if gso { opts1 & 1 << 26 != 0 } else { opts2 & 1 << 29 != 0 };
    if !gso && opts2 & (0xf << 28) == 0 {
        return vec![f];
    }
    let Some((ip, v4)) = ip_header(&f).filter(|&(_, v4)| v4 == want_v4) else { return vec![f] };
    let transport = if gso { opts1 >> 18 & 0x7f } else { opts2 >> 18 & 0x3ff } as usize;
    // the IPv4 header with its options
    let header = if v4 { ip + 4 * (f[ip] & 15) as usize } else { ip + 40 };
    if header < ip + 20 || header > f.len() {
        return vec![f];
    }
    // the datagram ends where its header says (Ethernet padding may follow; a frame to be cut is
    // whole, whatever its length field says)
    let length = u16::from_be_bytes(if v4 { [f[ip + 2], f[ip + 3]] } else { [f[ip + 4], f[ip + 5]] }) as usize;
    let datagram = if gso { f.len() } else { ip + length + if v4 { 0 } else { 40 } };
    if !gso && opts2 & (3 << 30) == 0 {
        if v4 {
            fill_ipv4(&mut f, ip, header); // the IPv4 header's checksum alone
        }
        return vec![f];
    }
    if transport < header || datagram > f.len() || datagram < transport + if tcp { 20 } else { 8 } {
        return vec![f];
    }
    let end = ip + if v4 { 20 } else { 40 };
    if !gso {
        if v4 {
            fill_ipv4(&mut f, ip, header);
        }
        let (proto, at) = if tcp { (6, transport + 16) } else { (17, transport + 6) };
        f[at..at + 2].fill(0);
        let mut c = transport_checksum(&f[ip..end], v4, proto, &f[transport..datagram]);
        if proto == 17 && c == 0 {
            c = 0xffff;
        }
        f[at..at + 2].copy_from_slice(&c.to_be_bytes());
        return vec![f];
    }
    let mss = (opts2 >> 18 & 0x7ff) as usize;
    let start = transport + 4 * (f[transport + 12] >> 4) as usize;
    if mss == 0 || start > f.len() || start < transport + 20 {
        return vec![f];
    }
    let seq = u32::from_be_bytes(f[transport + 4..transport + 8].try_into().unwrap());
    let id = u16::from_be_bytes([f[ip + 4], f[ip + 5]]);
    let count = f[start..].len().div_ceil(mss).max(1);
    (0..count)
        .map(|k| {
            let payload = &f[start..][k * mss..((k + 1) * mss).min(f.len() - start)];
            let mut s = [&f[..start], payload].concat();
            s[transport + 4..transport + 8].copy_from_slice(&seq.wrapping_add((k * mss) as u32).to_be_bytes());
            if k + 1 < count {
                s[transport + 13] &= !(1 | 8); // FIN and PSH belong to the last one
            }
            let n = s.len();
            if v4 {
                s[ip + 2..ip + 4].copy_from_slice(&((n - ip) as u16).to_be_bytes());
                s[ip + 4..ip + 6].copy_from_slice(&id.wrapping_add(k as u16).to_be_bytes());
                fill_ipv4(&mut s, ip, header);
            } else {
                s[ip + 4..ip + 6].copy_from_slice(&((n - ip - 40) as u16).to_be_bytes());
            }
            s[transport + 16..transport + 18].fill(0);
            let c = transport_checksum(&s[ip..end], v4, 6, &s[transport..]);
            s[transport + 16..transport + 18].copy_from_slice(&c.to_be_bytes());
            s
        })
        .collect()
}

fn fill_ipv4(f: &mut [u8], ip: usize, end: usize) {
    f[ip + 10..ip + 12].fill(0);
    let c = !(sum(0, &f[ip..end]) as u16);
    f[ip + 10..ip + 12].copy_from_slice(&c.to_be_bytes());
}

#[cfg(test)]
mod tests {
    use super::*;

    const BASE: u64 = 0x8000_0000;
    const MAC: [u8; 6] = [2, 0x53, 0x49, 0x4d, 0, 1];

    fn started(ram: &mut [u8]) -> Rtl8168 {
        let mut n = Rtl8168::new(MAC);
        let mut d = Dma { ram, base: BASE };
        n.set_link(true);
        n.write(INTR_STATUS as u64, 0xffff, 2, &mut d);
        n.write(INTR_MASK as u64, 0x3f, 2, &mut d);
        n.write(TX_LO as u64, BASE as u32 + 0x1000, 4, &mut d);
        n.write(RX_LO as u64, BASE as u32 + 0x2000, 4, &mut d);
        n.write(RX_CONFIG as u64, 0xe, 4, &mut d);
        n.write(CHIP_CMD as u64, 0xc, 1, &mut d);
        n
    }

    fn descriptor(ram: &mut [u8], at: usize, opts1: u32, opts2: u32, addr: u64) {
        ram[at..at + 4].copy_from_slice(&opts1.to_le_bytes());
        ram[at + 4..at + 8].copy_from_slice(&opts2.to_le_bytes());
        ram[at + 8..at + 16].copy_from_slice(&addr.to_le_bytes());
    }

    #[test]
    fn identity_phy_and_indirect_registers() {
        let mut ram = vec![0; 16];
        let mut n = Rtl8168::new(MAC);
        let mut d = Dma { ram: &mut ram, base: BASE };
        n.write(TX_CONFIG as u64, 0x0300_0700, 4, &mut d);
        assert_eq!(n.read(TX_CONFIG as u64, 4) >> 20 & 0xfcf, 0x541);
        assert_eq!(n.read(0, 4).to_le_bytes(), MAC[..4]);
        let mut phy = |n: &mut Rtl8168, reg: u32, write: Option<u16>| {
            n.write(GPHY_OCP as u64, reg << 15 | write.map_or(0, |v| FLAG | v as u32), 4, &mut d);
            n.read(GPHY_OCP as u64, 4)
        };
        assert_eq!(phy(&mut n, 0xa404, None), FLAG | 0xa404 << 15 | 0x1c);
        assert_eq!(phy(&mut n, 0xa406, None) & 0xffff, 0xc800);
        assert_eq!(phy(&mut n, 0xa402, None) & 4, 0, "no link without a cable");
        n.set_link(true);
        assert_eq!(phy(&mut n, 0xa402, None) & 0x24, 0x24);
        assert_eq!(phy(&mut n, 0xa434, None) & 0x630, 0x20, "1000 Mbit/s");
        assert_eq!(phy(&mut n, 0xa400, Some(0x0800)) & FLAG, 0, "a write clears the flag");
        assert_eq!(phy(&mut n, 0xa402, None) & 4, 0, "powered down");
        // ERI byte enables
        let mut d = Dma { ram: &mut ram, base: BASE };
        n.write(ERIDR as u64, 0x1122_3344, 4, &mut d);
        n.write(ERIAR as u64, FLAG | 0xf << 12 | 0xd0, 4, &mut d);
        n.write(ERIDR as u64, 0xffff_ffff, 4, &mut d);
        n.write(ERIAR as u64, FLAG | 1 << 12 | 0xd0, 4, &mut d);
        assert_eq!(n.read(ERIAR as u64, 4) & FLAG, 0);
        n.write(ERIAR as u64, 0xf << 12 | 0xd0, 4, &mut d);
        assert_ne!(n.read(ERIAR as u64, 4) & FLAG, 0);
        assert_eq!(n.read(ERIDR as u64, 4), 0x1122_33ff);
    }

    #[test]
    fn frames_go_both_ways() {
        let mut ram = vec![0; 0x10000];
        let mut n = started(&mut ram);
        ram[0x4000..0x4006].copy_from_slice(b"hello ");
        ram[0x4100..0x4105].copy_from_slice(b"world");
        descriptor(&mut ram, 0x1000, OWN | FIRST | 6, 0, BASE + 0x4000);
        descriptor(&mut ram, 0x1010, OWN | LAST | RING_END | 5, 0, BASE + 0x4100);
        n.write(TX_POLL as u64, 0x40, 1, &mut Dma { ram: &mut ram, base: BASE });
        assert_eq!(n.tx_out, vec![b"hello world".to_vec()]);
        assert_eq!((ram[0x1003] >> 7, ram[0x1013] >> 7), (0, 0));
        assert_eq!(n.tx_index, 0, "wrapped at the ring's end");
        assert!(n.irq());
        n.write(INTR_STATUS as u64, TX_OK as u32, 2, &mut Dma { ram: &mut ram, base: BASE });
        assert!(!n.irq());
        // receive: one for us, one for another station, then no descriptor left
        descriptor(&mut ram, 0x2000, OWN | RING_END | 0x3fff, 0, BASE + 0x5000);
        let frame = [&MAC[..], &[2; 6], &[8, 0x45], &[0x55; 50]].concat();
        n.deliver(vec![[&[4; 6][..], &[2; 60]].concat(), frame.clone(), frame.clone()]);
        n.service(&mut Dma { ram: &mut ram, base: BASE });
        let opts1 = u32::from_le_bytes(ram[0x2000..0x2004].try_into().unwrap());
        assert_eq!(opts1, RING_END | FIRST | LAST | 68);
        assert_eq!(ram[0x5000..0x5040], frame[..]);
        assert_eq!(n.status() & (RX_OK | RX_OVERFLOW), RX_OK | RX_OVERFLOW);
        assert_eq!((n.rx_frames, n.rx_queue.len()), (1, 1));
    }

    fn tcp_frame(payload: usize) -> Vec<u8> {
        let mut f = [&[2; 6][..], &MAC, &[8, 0, 0x45, 0]].concat();
        f.extend(((40 + payload) as u16).to_be_bytes());
        f.extend([0, 7, 0x40, 0, 64, 6, 0xde, 0xad, 10, 0, 2, 15, 1, 1, 1, 1]);
        f.extend([0x30, 0, 0, 80]);
        f.extend(1000u32.to_be_bytes());
        f.extend([0, 0, 0, 1, 0x50, 0x19, 0xff, 0xff, 0xde, 0xad, 0, 0]);
        f.extend((0..payload).map(|k| k as u8));
        f
    }

    fn valid(f: &[u8]) -> bool {
        sum(0, &f[14..34]) == 0xffff && transport_checksum(&f[14..34], true, 6, &f[34..]) == 0
    }

    #[test]
    fn checksum_and_segmentation_offload() {
        let out = offload(tcp_frame(99), 0, 1 << 29 | 1 << 30 | 34 << 18);
        assert!(out.len() == 1 && valid(&out[0]));
        let out = offload(tcp_frame(2500), 1 << 26 | 34 << 18, 1000 << 18);
        assert_eq!(out.iter().map(Vec::len).collect::<Vec<_>>(), [1054, 1054, 554]);
        for (k, f) in out.iter().enumerate() {
            assert!(valid(f), "segment {k}");
            assert_eq!(u32::from_be_bytes(f[38..42].try_into().unwrap()), 1000 + 1000 * k as u32);
            assert_eq!(f[47] & 9, if k == 2 { 9 } else { 0 });
            assert_eq!(f[54], (k * 1000) as u8);
        }
        // nonsense offsets leave the frame alone
        assert_eq!(offload(tcp_frame(10), 1 << 26 | 2 << 18, 1000 << 18), [tcp_frame(10)]);
        assert_eq!(offload(vec![1, 2, 3], 0, 1 << 29 | 1 << 30 | 34 << 18), [vec![1, 2, 3]]);
        // a frame with an inline VLAN tag
        let mut f = tcp_frame(40);
        f.splice(12..12, [0x81, 0, 0, 100]);
        let out = offload(f.clone(), 0, 1 << 29 | 1 << 30 | 38 << 18).remove(0);
        assert_eq!(sum(0, &out[18..38]), 0xffff);
        assert_eq!(transport_checksum(&out[18..38], true, 6, &out[38..]), 0);
        // padding after the datagram is not part of it
        let mut p = tcp_frame(3);
        p.resize(60, 0x55);
        let out = offload(p, 0, 1 << 29 | 1 << 30 | 34 << 18).remove(0);
        assert_eq!(transport_checksum(&out[14..34], true, 6, &out[34..57]), 0);
        // any descriptor, any frame: no panic
        let mut x = 1u32;
        let mut next = || { x ^= x << 13; x ^= x >> 17; x ^= x << 5; x };
        for k in 0..20_000 {
            let mut g = if k % 3 == 0 { f.clone() } else { tcp_frame(k % 90) };
            for _ in 0..k % 5 {
                let at = next() as usize % g.len();
                g[at] = next() as u8;
            }
            g.truncate(next() as usize % (g.len() + 1));
            let _ = offload(g, next(), next());
        }
    }
}
