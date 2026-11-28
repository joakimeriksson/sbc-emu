//! A USB 2.0 hub (the hub class: port status and change bits, port power, reset and suspend,
//! the status-change interrupt endpoint, a single transaction translator) and a tree of hubs
//! and devices behind a host controller's root port (`TreeBus`).
//!
//! The hub is high-speed with one TT, like the SMSC/Microchip USB2514 hubs inside the LAN7515
//! of a Raspberry Pi 3 Model B+. A port reset completes at once (the reset bit is never seen
//! set); its device then goes back to its default state when the tree settles the transaction
//! (`TreeBus::settle`). Full-speed devices behind it are reached by split transactions, which
//! the host controller handles: the tree routes by device address, as the TT does.
use crate::usb::{DevId, Devices, Speed, Std, UsbBus, UsbDevice};

// wPortStatus
pub const PORT_CONNECTION: u16 = 1 << 0;
pub const PORT_ENABLE: u16 = 1 << 1;
pub const PORT_SUSPEND: u16 = 1 << 2;
pub const PORT_RESET: u16 = 1 << 4;
pub const PORT_POWER: u16 = 1 << 8;
pub const PORT_HIGH_SPEED: u16 = 1 << 10;
pub const PORT_INDICATOR: u16 = 1 << 12;
// wPortChange
pub const C_CONNECTION: u16 = 1 << 0;
pub const C_ENABLE: u16 = 1 << 1;
pub const C_SUSPEND: u16 = 1 << 2;
pub const C_RESET: u16 = 1 << 4;

// port features (SET_FEATURE / CLEAR_FEATURE with recipient "other")
const F_ENABLE: u16 = 1;
const F_SUSPEND: u16 = 2;
const F_OVER_CURRENT: u16 = 3;
const F_RESET: u16 = 4;
const F_POWER: u16 = 8;
const F_C_CONNECTION: u16 = 16;
const F_C_RESET: u16 = 20;
const F_TEST: u16 = 21;
const F_INDICATOR: u16 = 22;

/// One downstream port: what is plugged in (fixed at construction) and its state.
#[derive(Default, Clone)]
pub struct Port {
    pub status: u16,
    pub change: u16,
    /// the speed of the device plugged in, if any
    pub attached: Option<Speed>,
}

pub struct Hub {
    pub st: Std,
    pub ports: Vec<Port>,
    vendor: u16,
    product: u16,
    /// DeviceRemovable: bit n set = the device on port n is soldered on
    fixed: u8,
    /// part of a compound device (the LAN7515's hubs are)
    compound: bool,
    /// ports whose change bits the status endpoint has reported (bit n = port n)
    reported: u8,
    /// ports whose reset just finished: their devices go back to the default state
    resets: Vec<u8>,
}

impl Hub {
    /// A hub with `ports` downstream ports (at most 7), USB IDs `vendor:product`, `fixed`
    /// the ports with soldered-on devices (bit n = port n).
    pub fn new(ports: usize, vendor: u16, product: u16, fixed: u8, compound: bool) -> Self {
        Hub { st: Std::default(), ports: vec![Port::default(); ports.clamp(1, 7)], vendor, product, fixed, compound, reported: 0, resets: Vec::new() }
    }

    /// Plug a device of `speed` into port `port` (1-based).
    pub fn attach(&mut self, port: u8, speed: Speed) {
        if let Some(p) = self.ports.get_mut(port as usize - 1) { p.attached = Some(speed); }
    }

    /// Whether the device on port `port` (1-based) can be reached: enabled, not suspended.
    pub fn port_active(&self, port: u8) -> bool {
        self.ports.get((port as usize).wrapping_sub(1)).is_some_and(|p| p.status & (PORT_ENABLE | PORT_SUSPEND) == PORT_ENABLE)
    }

    /// Ports whose reset finished since the last call.
    pub fn take_resets(&mut self) -> Vec<u8> { std::mem::take(&mut self.resets) }

    /// The status-change bitmap: bit 0 the hub, bit n port n.
    fn changes(&self) -> u8 {
        self.ports.iter().enumerate().fold(0, |a, (i, p)| a | (((p.change != 0) as u8) << (i + 1)))
    }

    fn descriptor(&self) -> Vec<u8> {
        // individual port power switching and over-current protection, TT think time 8 FS bit times
        let chars: u16 = 0x0009 | if self.compound { 0x0004 } else { 0 };
        vec![9, 0x29, self.ports.len() as u8, chars as u8, (chars >> 8) as u8, 50, 1, self.fixed, 0xff]
    }

    fn port(&mut self, index: u16) -> Option<&mut Port> {
        self.ports.get_mut((index as usize).wrapping_sub(1))
    }

    fn set_feature(&mut self, index: u16, f: u16) -> bool {
        let Some(p) = self.port(index) else { return false };
        match f {
            F_POWER => {
                if p.status & PORT_POWER == 0 {
                    p.status |= PORT_POWER;
                    if p.attached.is_some() { p.status |= PORT_CONNECTION; p.change |= C_CONNECTION; }
                }
            }
            F_RESET => {
                if p.status & PORT_CONNECTION != 0 {
                    // the reset ends at once: enabled, at the device's speed
                    p.status = (p.status & !(PORT_SUSPEND | PORT_HIGH_SPEED | PORT_RESET)) | PORT_ENABLE
                        | if p.attached == Some(Speed::High) { PORT_HIGH_SPEED } else { 0 };
                    p.change |= C_RESET;
                    self.resets.push(index as u8);
                }
            }
            F_SUSPEND => if p.status & PORT_ENABLE != 0 { p.status |= PORT_SUSPEND },
            F_INDICATOR => p.status |= PORT_INDICATOR,
            F_ENABLE | F_TEST => {}
            _ => return false,
        }
        true
    }

    fn clear_feature(&mut self, index: u16, f: u16) -> bool {
        let n = index as u8;
        let Some(p) = self.port(index) else { return false };
        match f {
            F_ENABLE => p.status &= !(PORT_ENABLE | PORT_SUSPEND),
            // resume: the device answers again, and the hub says the resume is done
            F_SUSPEND => if p.status & PORT_SUSPEND != 0 { p.status &= !PORT_SUSPEND; p.change |= C_SUSPEND; },
            F_POWER => p.status &= !(PORT_POWER | PORT_CONNECTION | PORT_ENABLE | PORT_SUSPEND | PORT_HIGH_SPEED),
            F_INDICATOR => p.status &= !PORT_INDICATOR,
            F_OVER_CURRENT => {}
            F_C_CONNECTION..=F_C_RESET => p.change &= !(1 << (f - F_C_CONNECTION)),
            _ => return false,
        }
        if p.change == 0 { self.reported &= !(1 << n); }
        true
    }
}

impl UsbDevice for Hub {
    fn std(&mut self) -> &mut Std { &mut self.st }
    fn device_desc(&self) -> Vec<u8> {
        // class hub, protocol 1: high speed with a single TT; bcdDevice 0x0bb3
        let mut d = vec![18, 1, 0x00, 0x02, 9, 0, 1, 64];
        d.extend_from_slice(&self.vendor.to_le_bytes());
        d.extend_from_slice(&self.product.to_le_bytes());
        d.extend_from_slice(&[0xb3, 0x0b, 1, 2, 0, 1]);
        d
    }
    fn config_desc(&self) -> Vec<u8> {
        vec![
            9, 2, 25, 0, 1, 1, 0, 0xe0, 1, // self-powered, remote wakeup, 2 mA
            9, 4, 0, 0, 1, 9, 0, 0, 0,     // interface: hub, one endpoint
            7, 5, 0x81, 3, 1, 0, 12,       // status change: interrupt IN, 1 byte, every 256 ms
        ]
    }
    fn string(&self, i: u8) -> Option<&'static str> { match i { 1 => Some("sbc-emu"), 2 => Some("USB 2.0 Hub"), _ => None } }
    fn speed(&self) -> Speed { Speed::High }
    fn self_powered(&self) -> bool { true }
    fn class_setup(&mut self, s: [u8; 8]) -> Option<Vec<u8>> {
        let value = u16::from_le_bytes([s[2], s[3]]);
        let index = u16::from_le_bytes([s[4], s[5]]);
        let len = u16::from_le_bytes([s[6], s[7]]) as usize;
        match (s[0], s[1]) {
            (0xa0, 6) if value >> 8 == 0x29 => Some(self.descriptor()),
            (0xa0, 0) => Some(vec![0; 4]),                 // hub status: local power good
            (0x20, 1) | (0x20, 3) => Some(Vec::new()),     // hub features (C_HUB_LOCAL_POWER, C_HUB_OVER_CURRENT)
            (0xa3, 0) => {
                let p = self.port(index)?;
                let (st, ch) = (p.status, p.change);
                Some([st.to_le_bytes(), ch.to_le_bytes()].concat())
            }
            (0x23, 3) => self.set_feature(index, value).then(Vec::new),
            (0x23, 1) => self.clear_feature(index, value).then(Vec::new),
            (0x23, 8) | (0x23, 9) | (0x23, 11) => Some(Vec::new()), // CLEAR_TT_BUFFER, RESET_TT, STOP_TT
            (0xa3, 10) => Some(vec![0; len]),                       // GET_TT_STATE
            _ => None,
        }
    }
    fn data_in(&mut self, ep: u8, max: usize) -> Option<Vec<u8>> {
        if ep != 1 || !self.st.configured || max == 0 { return None; }
        let bits = self.changes();
        // a change is reported once; the driver clears it after reading the port's status
        if bits & !self.reported == 0 { return None; }
        self.reported |= bits;
        Some(vec![bits])
    }
    fn has_input(&self) -> bool { self.st.configured && self.changes() & !self.reported != 0 }
    fn bus_reset(&mut self) {
        // a reset hub starts over: ports unpowered, devices behind it unreachable
        self.st = Std::default();
        for p in self.ports.iter_mut() { p.status = 0; p.change = 0; }
        self.reported = 0;
        self.resets.clear();
    }
}

// ---------------------------------------------------------------------- the tree

/// What sits in one place of the tree.
pub enum Node {
    Hub(Hub),
    /// one of the shared devices (`Devices`: keyboard, tablet, ...)
    Dev(DevId),
    /// a LAN78xx USB Ethernet adapter (the 3B+'s LAN7515)
    Lan(Box<crate::lan78xx::Lan78xx>),
    /// a LAN95xx USB Ethernet adapter (the 3B's LAN9514)
    Smsc(Box<crate::smsc95xx::Smsc95xx>),
}

/// A USB Ethernet adapter in the tree, as the machine connects it to the network.
pub trait UsbNic {
    /// Frames the guest sent since the last call.
    fn take_tx(&mut self) -> Vec<Vec<u8>>;
    /// A frame from the network for the guest.
    fn receive(&mut self, frame: &[u8]);
    /// Frames sent, received, dropped.
    fn counters(&self) -> (u64, u64, u64);
}

impl UsbNic for crate::lan78xx::Lan78xx {
    fn take_tx(&mut self) -> Vec<Vec<u8>> { std::mem::take(&mut self.tx_out) }
    fn receive(&mut self, frame: &[u8]) { crate::lan78xx::Lan78xx::receive(self, frame) }
    fn counters(&self) -> (u64, u64, u64) { (self.tx_frames, self.rx_frames, self.rx_dropped) }
}

impl UsbNic for crate::smsc95xx::Smsc95xx {
    fn take_tx(&mut self) -> Vec<Vec<u8>> { std::mem::take(&mut self.tx_out) }
    fn receive(&mut self, frame: &[u8]) { crate::smsc95xx::Smsc95xx::receive(self, frame) }
    fn counters(&self) -> (u64, u64, u64) { (self.tx_frames, self.rx_frames, self.rx_dropped) }
}

pub struct Slot {
    pub node: Node,
    /// the hub (index in the tree) and its port (1-based) this is plugged into; None: the root port
    pub up: Option<(usize, u8)>,
}

/// Hubs and devices behind one root port. The first node added without a parent is on the root port.
#[derive(Default)]
pub struct UsbTree {
    pub slots: Vec<Slot>,
}

impl UsbTree {
    pub fn new() -> Self { Self::default() }

    /// Add `node` behind port `up.1` of the hub at `up.0` (None: on the root port); returns its index.
    pub fn add(&mut self, node: Node, up: Option<(usize, u8)>, devs: &mut Devices) -> usize {
        let speed = match &node {
            Node::Hub(_) | Node::Lan(_) | Node::Smsc(_) => Speed::High,
            Node::Dev(d) => devs.get(*d).map_or(Speed::Full, |d| d.speed()),
        };
        if let Some((h, p)) = up {
            if let Some(Slot { node: Node::Hub(hub), .. }) = self.slots.get_mut(h) { hub.attach(p, speed); }
        }
        self.slots.push(Slot { node, up });
        self.slots.len() - 1
    }

    /// The Ethernet adapter, if the tree has one.
    pub fn lan(&mut self) -> Option<&mut dyn UsbNic> {
        self.slots.iter_mut().find_map(|s| match &mut s.node {
            Node::Lan(l) => Some(l.as_mut() as &mut dyn UsbNic),
            Node::Smsc(l) => Some(l.as_mut() as &mut dyn UsbNic),
            _ => None,
        })
    }
    pub fn lan_ref(&self) -> Option<&dyn UsbNic> {
        self.slots.iter().find_map(|s| match &s.node {
            Node::Lan(l) => Some(l.as_ref() as &dyn UsbNic),
            Node::Smsc(l) => Some(l.as_ref() as &dyn UsbNic),
            _ => None,
        })
    }

    fn root(&self) -> Option<usize> { self.slots.iter().position(|s| s.up.is_none()) }

    /// Every hub port between node `i` and the root port is enabled.
    fn reachable(&self, mut i: usize) -> bool {
        for _ in 0..8 {
            match self.slots[i].up {
                None => return true,
                Some((h, p)) => match &self.slots[h].node {
                    Node::Hub(hub) if hub.port_active(p) => i = h,
                    _ => return false,
                },
            }
        }
        false
    }

    fn get<'a>(&'a mut self, i: usize, devs: &'a mut Devices) -> Option<&'a mut dyn UsbDevice> {
        match &mut self.slots.get_mut(i)?.node {
            Node::Hub(h) => Some(h),
            Node::Lan(l) => Some(l.as_mut()),
            Node::Smsc(l) => Some(l.as_mut()),
            Node::Dev(d) => devs.get(*d),
        }
    }
}

/// The tree as a root port's bus, with the shared devices it refers to.
pub struct TreeBus<'a> {
    pub tree: &'a mut UsbTree,
    pub devs: &'a mut Devices,
}

impl UsbBus for TreeBus<'_> {
    fn root_speed(&mut self) -> Option<Speed> {
        let r = self.tree.root()?;
        self.tree.get(r, self.devs).map(|d| d.speed())
    }
    fn root_reset(&mut self) {
        if let Some(r) = self.tree.root() { if let Some(d) = self.tree.get(r, self.devs) { d.bus_reset(); } }
    }
    fn device(&mut self, addr: u8) -> Option<&mut dyn UsbDevice> {
        let n = self.tree.slots.len();
        let i = (0..n).find(|&i| self.tree.reachable(i) && self.tree.get(i, self.devs).is_some_and(|d| d.address() == addr))?;
        self.tree.get(i, self.devs)
    }
    fn settle(&mut self) {
        for h in 0..self.tree.slots.len() {
            let resets = match &mut self.tree.slots[h].node { Node::Hub(hub) => hub.take_resets(), _ => continue };
            for p in resets {
                if let Some(c) = self.tree.slots.iter().position(|s| s.up == Some((h, p))) {
                    if let Some(d) = self.tree.get(c, self.devs) { d.bus_reset(); }
                }
            }
        }
    }
}

// ---------------------------------------------------------------------- snapshots

impl crate::snap::Snap for Hub {
    fn save(&self, w: &mut crate::snap::W) {
        self.st.save(w);
        w.u8(self.ports.len() as u8);
        for p in &self.ports { w.u16(p.status); w.u16(p.change); }
        w.u8(self.reported);
    }
    fn load(&mut self, r: &mut crate::snap::R) -> Result<(), crate::snap::SnapError> {
        self.st.load(r)?;
        if r.u8()? as usize != self.ports.len() { return Err(crate::snap::SnapError("a USB hub with another number of ports".into())); }
        for p in self.ports.iter_mut() { p.status = r.u16()?; p.change = r.u16()?; }
        self.reported = r.u8()?;
        self.resets.clear();
        Ok(())
    }
}

/// The state of the hubs and adapters (the shared devices are saved with `Devices`).
impl crate::snap::Snap for UsbTree {
    fn save(&self, w: &mut crate::snap::W) {
        w.tag("usbtree-v1");
        w.u32(self.slots.len() as u32);
        for s in &self.slots {
            match &s.node {
                Node::Hub(h) => { w.u8(1); h.save(w); }
                Node::Lan(l) => { w.u8(2); l.save(w); }
                Node::Dev(_) => w.u8(3),
                Node::Smsc(l) => { w.u8(4); l.save(w); }
            }
        }
    }
    fn load(&mut self, r: &mut crate::snap::R) -> Result<(), crate::snap::SnapError> {
        r.tag("usbtree-v1")?;
        if r.u32()? as usize != self.slots.len() { return Err(crate::snap::SnapError("another USB topology".into())); }
        for s in self.slots.iter_mut() {
            match (r.u8()?, &mut s.node) {
                (1, Node::Hub(h)) => h.load(r)?,
                (2, Node::Lan(l)) => l.load(r)?,
                (3, Node::Dev(_)) => {}
                (4, Node::Smsc(l)) => l.load(r)?,
                _ => return Err(crate::snap::SnapError("another USB topology".into())),
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::snap::Snap;

    fn setup(rt: u8, req: u8, value: u16, index: u16, len: u16) -> [u8; 8] {
        let mut s = [rt, req, 0, 0, 0, 0, 0, 0];
        s[2..4].copy_from_slice(&value.to_le_bytes());
        s[4..6].copy_from_slice(&index.to_le_bytes());
        s[6..8].copy_from_slice(&len.to_le_bytes());
        s
    }

    /// A control request with an IN data stage (or none): the reply, None on STALL.
    fn ctrl(d: &mut dyn UsbDevice, s: [u8; 8]) -> Option<Vec<u8>> {
        if !d.setup(s) { return None; }
        if s[0] & 0x80 == 0 { d.control_in(0); return Some(Vec::new()); } // status stage IN
        let r = d.control_in(u16::from_le_bytes([s[6], s[7]]) as usize);
        d.control_out(&[]);
        Some(r)
    }

    fn port_status(d: &mut dyn UsbDevice, p: u16) -> (u16, u16) {
        let r = ctrl(d, setup(0xa3, 0, 0, p, 4)).unwrap();
        (u16::from_le_bytes([r[0], r[1]]), u16::from_le_bytes([r[2], r[3]]))
    }

    /// Enumerate a device at address 0 to `addr` and configure it.
    fn enumerate(bus: &mut dyn UsbBus, addr: u8) {
        let d = bus.device(0).expect("a device at address 0");
        let desc = ctrl(d, setup(0x80, 6, 0x0100, 0, 18)).unwrap();
        assert_eq!(desc[1], 1);
        assert!(d.setup(setup(0x00, 5, addr as u16, 0, 0)));
        assert!(d.control_in(0).is_empty());
        let d = bus.device(addr).expect("the device at its new address");
        assert!(ctrl(d, setup(0x00, 9, 1, 0, 0)).is_some());
    }

    #[test]
    fn hub_ports_power_reset_and_report_changes() {
        let mut h = Hub::new(4, 0x0424, 0x2514, 1 << 1, true);
        h.attach(1, Speed::High);
        h.attach(2, Speed::Full);
        let desc = ctrl(&mut h, setup(0xa0, 6, 0x2900, 0, 71)).unwrap();
        assert_eq!((desc[1], desc[2], desc[7]), (0x29, 4, 1 << 1));
        let st = ctrl(&mut h, setup(0x80, 0, 0, 0, 2)).unwrap();
        assert_eq!(st[0] & 1, 1, "self-powered");
        assert!(ctrl(&mut h, setup(0x00, 9, 1, 0, 0)).is_some());
        // nothing to report until ports are powered
        assert_eq!(h.data_in(1, 1), None);
        for p in 1..=4 { assert!(ctrl(&mut h, setup(0x23, 3, F_POWER, p, 0)).is_some()); }
        assert_eq!(port_status(&mut h, 1), (PORT_POWER | PORT_CONNECTION, C_CONNECTION));
        assert_eq!(port_status(&mut h, 3), (PORT_POWER, 0));
        // ports 1 and 2 changed; reported once
        assert!(h.has_input());
        assert_eq!(h.data_in(1, 1), Some(vec![0b110]));
        assert_eq!(h.data_in(1, 1), None);
        assert!(ctrl(&mut h, setup(0x23, 1, F_C_CONNECTION, 1, 0)).is_some());
        // reset: enabled at once, high speed, C_RESET; the device behind it is to be reset
        assert!(ctrl(&mut h, setup(0x23, 3, F_RESET, 1, 0)).is_some());
        assert_eq!(port_status(&mut h, 1), (PORT_POWER | PORT_CONNECTION | PORT_ENABLE | PORT_HIGH_SPEED, C_RESET));
        assert_eq!(h.take_resets(), vec![1]);
        assert!(h.port_active(1));
        assert!(ctrl(&mut h, setup(0x23, 3, F_RESET, 2, 0)).is_some());
        assert_eq!(port_status(&mut h, 2).0, PORT_POWER | PORT_CONNECTION | PORT_ENABLE, "full speed: neither speed bit");
        // suspend and resume
        assert!(ctrl(&mut h, setup(0x23, 3, F_SUSPEND, 1, 0)).is_some());
        assert!(!h.port_active(1));
        assert!(ctrl(&mut h, setup(0x23, 1, F_SUSPEND, 1, 0)).is_some());
        assert!(h.port_active(1));
        assert_eq!(port_status(&mut h, 1).1 & C_SUSPEND, C_SUSPEND);
        // a reset on an empty port does nothing; an unknown port stalls
        assert!(ctrl(&mut h, setup(0x23, 3, F_RESET, 3, 0)).is_some());
        assert_eq!(port_status(&mut h, 3), (PORT_POWER, 0));
        assert!(ctrl(&mut h, setup(0xa3, 0, 0, 5, 4)).is_none());
    }

    /// The keyboard behind two hubs is enumerated through the tree and its reports come through.
    #[test]
    fn keyboard_behind_two_hubs() {
        let mut devs = Devices::default();
        let mut tree = UsbTree::new();
        let h1 = tree.add(Node::Hub(Hub::new(4, 0x0424, 0x2514, 0, true)), None, &mut devs);
        let h2 = tree.add(Node::Hub(Hub::new(3, 0x0424, 0x2514, 0, true)), Some((h1, 1)), &mut devs);
        tree.add(Node::Dev(DevId::Keyboard), Some((h2, 2)), &mut devs);
        let mut bus = TreeBus { tree: &mut tree, devs: &mut devs };
        assert_eq!(bus.root_speed(), Some(Speed::High));
        bus.root_reset();
        enumerate(&mut bus, 1);
        // hub 1, port 1: power, reset
        for (hub, port, addr) in [(1u8, 1u16, 2u8), (2, 2, 3)] {
            let h = bus.device(hub).unwrap();
            assert!(ctrl(h, setup(0x23, 3, F_POWER, port, 0)).is_some());
            assert!(bus.device(0).is_none(), "not reachable before the port is reset");
            let h = bus.device(hub).unwrap();
            assert!(ctrl(h, setup(0x23, 3, F_RESET, port, 0)).is_some());
            bus.settle();
            enumerate(&mut bus, addr);
        }
        bus.devs.kbd.type_byte(b'a');
        let kbd = bus.device(3).unwrap();
        assert!(kbd.has_input());
        assert_eq!(kbd.data_in(1, 8), Some(vec![0, 0, 4, 0, 0, 0, 0, 0]));
        // a reset of the hub's port puts the keyboard back at address 0
        let h = bus.device(2).unwrap();
        assert!(ctrl(h, setup(0x23, 3, F_RESET, 2, 0)).is_some());
        bus.settle();
        assert!(bus.device(3).is_none());
        assert!(bus.device(0).is_some());
        // disabling hub 1's port hides everything behind it
        let h = bus.device(1).unwrap();
        assert!(ctrl(h, setup(0x23, 1, F_ENABLE, 1, 0)).is_some());
        assert!(bus.device(2).is_none());
        assert!(bus.device(0).is_none());
    }

    #[test]
    fn tree_snapshot_round_trip() {
        let mut devs = Devices::default();
        let mut tree = UsbTree::new();
        let h = tree.add(Node::Hub(Hub::new(4, 0x0424, 0x2514, 0, true)), None, &mut devs);
        tree.add(Node::Dev(DevId::Keyboard), Some((h, 2)), &mut devs);
        if let Node::Hub(hub) = &mut tree.slots[0].node { hub.set_feature(2, F_POWER); hub.set_feature(2, F_RESET); }
        let mut w = crate::snap::W::default();
        tree.save(&mut w);
        let mut other = UsbTree::new();
        other.add(Node::Hub(Hub::new(4, 0x0424, 0x2514, 0, true)), None, &mut devs);
        other.add(Node::Dev(DevId::Keyboard), Some((h, 2)), &mut devs);
        other.load(&mut crate::snap::R::new(&w.0)).unwrap();
        let Node::Hub(hub) = &other.slots[0].node else { panic!() };
        assert!(hub.port_active(2));
    }
}
