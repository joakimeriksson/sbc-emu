//! The Tegra210's PCIe controller (`nvidia,tegra210-pcie`): AFI (address translation, MSI,
//! interrupts), the pads, two root ports and the configuration window. On the Nano port 0
//! (4 lanes) is the M.2 Key E slot, empty here, and port 1 is the RTL8111H Ethernet controller.
//!
//! Configuration space is reached through a 4 KiB page that AFI_FPCI_BAR0 places
//! (`tegra_pcie_map_bus`); memory windows are 1:1. A message-signalled interrupt is the device's
//! write to the page AFI_MSI_FPCI_BAR_ST names, caught by the AFI.
use crate::rtl8168::{self, Rtl8168};
use crate::sdhci::Dma;

pub const IRQ: usize = 32 + 0x62;
pub const MSI_IRQ: usize = 32 + 0x63;

const PORTS: u64 = 0x0100_0000;
const PADS: u64 = 0x0100_3000;
const AFI: u64 = 0x0100_3800;
const CONFIG: u64 = 0x0200_0000;
const IO: u64 = 0x1200_0000;
const END: u64 = 0x4000_0000;

const AFI_FPCI_BAR0: usize = 0x30;
const AFI_MSI_BAR_SZ: usize = 0x60;
const AFI_MSI_FPCI_BAR_ST: usize = 0x64;
const AFI_MSI_VEC: usize = 0x6c;
const AFI_MSI_EN_VEC: usize = 0x8c;
const AFI_INTR_MASK: usize = 0xb4;
const AFI_SM_INTR_ENABLE: usize = 0xc4;
const AFI_PCIE_PME: usize = 0xf0;
const AFI_PCIE_CONFIG: usize = 0xf8;
const AFI_PEX_CTRL: [usize; 2] = [0x110, 0x118];
const PADS_PLL_CTL: usize = 0xb4;

const RP_VEND_XP: usize = 0xf00;
const RP_LINK: usize = 0x90;
const DL_ACTIVE: u32 = 1 << 29;

/// The MSI capability of the Ethernet controller.
const MSI_CAP: usize = 0x50;

struct RootPort {
    regs: Vec<u32>,
}

impl RootPort {
    fn new(index: u32) -> Self {
        let mut regs = vec![0u32; 0x400];
        let mut set = |o: usize, v: u32| regs[o / 4] = v;
        set(0x00, (0x0fae + index) << 16 | 0x10de);
        set(0x04, 0x0010 << 16); // a capability list
        set(0x08, 0x0604_00a1);
        set(0x0c, 0x0001_0000); // a bridge
        set(0x34, 0x48);
        set(0x3c, 0x0100);
        set(0x48, 0x0003 << 16 | 0x80 << 8 | 0x01); // power management, then PCI Express
        set(0x80, 0x0142 << 16 | 0x10); // a root port with a slot
        let lanes = if index == 0 { 4 } else { 1 };
        set(0x8c, (index) << 24 | 1 << 20 | lanes << 4 | 2); // 5 GT/s, reports an active link
        Self { regs }
    }
}

pub struct Pcie {
    afi: [u32; 0x200],
    pads: [u32; 0x200],
    ports: [RootPort; 2],
    /// PERST#, the "pcie_x" reset of the clock controller, as the SoC sees it
    pub in_reset: bool,
    ep: [u32; 64],
    pub nic: Rtl8168,
    nic_level: bool,
    pub msis: u64,
}

impl Pcie {
    pub fn new(mac: [u8; 6]) -> Self {
        Self { afi: [0; 0x200], pads: [0; 0x200], ports: [RootPort::new(0), RootPort::new(1)], in_reset: false, ep: Self::endpoint(), nic: Rtl8168::new(mac), nic_level: false, msis: 0 }
    }

    /// The RTL8111H's configuration space after a reset.
    fn endpoint() -> [u32; 64] {
        let mut ep = [0; 64];
        let mut set = |o: usize, v: u32| ep[o / 4] = v;
        set(0x00, 0x8168_10ec);
        set(0x04, 0x0010 << 16);
        set(0x08, 0x0200_0015);
        set(0x10, 1); // I/O, 256 bytes
        set(0x18, 4); // 64-bit memory, 4 KiB
        set(0x20, 4); // 64-bit memory, 16 KiB (MSI-X, which is not modelled)
        set(0x2c, 0x0123_10ec);
        set(0x34, 0x40);
        set(0x3c, 0x0100);
        set(0x40, 0x0003 << 16 | 0x50 << 8 | 0x01); // power management
        set(MSI_CAP, 0x0080 << 16 | 0x70 << 8 | 0x05); // MSI, 64-bit, one vector
        set(0x70, 0x0002 << 16 | 0x10); // PCI Express endpoint
        set(0x7c, 1 << 4 | 1); // x1, 2.5 GT/s, no ASPM
        set(0x80, (1 << 4 | 1) << 16);
        ep
    }

    /// Whether `addr` belongs to the controller.
    pub fn contains(addr: u64) -> bool {
        (PORTS..PORTS + 0x2000).contains(&addr) || (PADS..AFI + 0x800).contains(&addr) || (CONFIG..END).contains(&addr)
    }

    /// The link to the device on port `k`: the Ethernet controller's, while nothing holds it down.
    fn link_up(&self, k: usize) -> bool {
        k == 1 && !self.in_reset && self.afi[AFI_PEX_CTRL[1] / 4] & 1 != 0 && self.afi[AFI_PCIE_CONFIG / 4] & 1 << 2 == 0
    }

    fn port_read(&self, k: usize, o: usize) -> u32 {
        let v = self.ports[k].regs[o / 4];
        let up = self.link_up(k);
        match o {
            RP_VEND_XP => v & !(1 << 30) | if up { 1 << 30 } else { 0 },
            // link status: training done, negotiated x1 at 2.5 GT/s
            RP_LINK => v & 0xffff & !(1 << 5) | if up { DL_ACTIVE | (1 << 4 | 1) << 16 } else { 0 },
            // slot status: presence
            0x98 => v & 0xffff | if up { 1 << 22 } else { 0 },
            _ => v,
        }
    }

    fn port_write(&mut self, k: usize, o: usize, v: u32) {
        let r = &mut self.ports[k].regs[o / 4];
        match o {
            0x00 | 0x08 | 0x10 | 0x14 | 0x34 | 0x48 | 0x80 | 0x84 | 0x8c | 0x94 => {}
            0x04 => *r = *r & 0xffff_0000 | v & 0x0547,
            0x0c => *r = 0x0001_0000 | v & 0xff,
            0x3c => *r = 0x0100 | v & 0xffff_00ff,
            _ => *r = v,
        }
    }

    /// (bus, device, function, register) of a configuration window access.
    fn config_target(&self, a: u64) -> (u32, u32, u32, usize) {
        let page = (self.afi[AFI_FPCI_BAR0 / 4].wrapping_sub(0xfe10_0000) & 0xf_ffff) << 8;
        // (beyond the 4 KiB page the window goes on linearly)
        let offset = (page & !0xfff).wrapping_add((a - CONFIG) as u32);
        let reg = offset & 0xff | offset >> 16 & 0xf00;
        (offset >> 16 & 0xff, offset >> 11 & 31, offset >> 8 & 7, reg as usize)
    }

    fn config_selects_nic(&self, (bus, dev, func, _): (u32, u32, u32, usize)) -> bool {
        self.link_up(1) && bus != 0 && bus == self.ports[1].regs[6] >> 8 & 0xff && dev == 0 && func == 0
    }

    /// The Ethernet controller's registers, when its memory decoding is on.
    fn bar(&self) -> Option<u64> {
        let a = (self.ep[7] as u64) << 32 | (self.ep[6] & !0xf) as u64;
        (self.link_up(1) && self.ep[1] & 2 != 0 && a != 0).then_some(a)
    }

    pub fn read(&mut self, addr: u64, size: usize) -> u32 {
        let pick = |w: u32| w >> ((addr & 3) * 8) & if size >= 4 { u32::MAX } else { (1 << (8 * size)) - 1 };
        if let Some(o) = addr.checked_sub(AFI).filter(|&o| o < 0x800) {
            let o = o as usize & !3;
            let v = match o {
                // the power-off message is acknowledged at once
                AFI_PCIE_PME => {
                    let v = self.afi[o / 4];
                    v | (v & 1) << 5 | (v >> 8 & 1) << 10
                }
                _ => self.afi[o / 4],
            };
            return pick(v);
        }
        if let Some(o) = addr.checked_sub(PADS).filter(|&o| o < 0x800) {
            let o = o as usize & !3;
            // the PLL locks as soon as it is out of reset
            let lock = if o == PADS_PLL_CTL && self.pads[o / 4] & 2 != 0 { 1 << 8 } else { 0 };
            return pick(self.pads[o / 4] & !(if o == PADS_PLL_CTL { 1 << 8 } else { 0 }) | lock);
        }
        if let Some(o) = addr.checked_sub(PORTS).filter(|&o| o < 0x2000) {
            return pick(self.port_read(o as usize >> 12, o as usize & 0xffc));
        }
        if (CONFIG..IO).contains(&addr) {
            let t = self.config_target(addr);
            let w = if self.config_selects_nic(t) { self.ep.get(t.3 / 4).copied().unwrap_or(0) } else { u32::MAX };
            return pick(w);
        }
        match self.bar() {
            Some(b) if (b..b + rtl8168::SIZE).contains(&addr) => self.nic.read(addr - b, size),
            _ => pick(u32::MAX),
        }
    }

    pub fn write(&mut self, addr: u64, value: u32, size: usize, dma: &mut Dma) {
        let shift = (addr & 3) * 8;
        let mask = if size >= 4 { u32::MAX } else { ((1u32 << (8 * size)) - 1) << shift };
        let v = value << shift & mask;
        if let Some(o) = addr.checked_sub(AFI).filter(|&o| o < 0x800) {
            let o = o as usize & !3;
            match o {
                _ if (AFI_MSI_VEC..AFI_MSI_VEC + 32).contains(&o) => self.afi[o / 4] &= !v,
                _ => {
                    let was = self.link_up(1);
                    self.afi[o / 4] = self.afi[o / 4] & !mask | v;
                    if was && !self.link_up(1) {
                        self.reset_device();
                    }
                }
            }
        } else if let Some(o) = addr.checked_sub(PADS).filter(|&o| o < 0x800) {
            let o = o as usize & !3;
            self.pads[o / 4] = self.pads[o / 4] & !mask | v;
        } else if let Some(o) = addr.checked_sub(PORTS).filter(|&o| o < 0x2000) {
            let (k, o) = (o as usize >> 12, o as usize & 0xffc);
            let old = self.ports[k].regs[o / 4] & !mask;
            self.port_write(k, o, old | v);
        } else if (CONFIG..IO).contains(&addr) {
            let t = self.config_target(addr);
            if self.config_selects_nic(t) {
                self.nic_config_write(t.3 & !3, v, mask);
            }
        } else if let Some(b) = self.bar() {
            if (b..b + rtl8168::SIZE).contains(&addr) {
                self.nic.write(addr - b, value, size, dma);
            }
        }
        self.update(); // an edge must not be lost between two writes
    }

    fn nic_config_write(&mut self, o: usize, v: u32, mask: u32) {
        let Some(r) = self.ep.get_mut(o / 4) else { return };
        let w = *r & !mask | v;
        match o {
            0x04 => *r = *r & 0xffff_0000 | w & 0x0547,
            0x0c => *r = w & 0xffff,
            0x10 => *r = w & !0xff | 1,
            0x18 => *r = w & !0xfff | 4,
            0x1c | 0x24 => *r = w,
            0x20 => *r = w & !0x3fff | 4,
            0x3c => *r = 0x0100 | w & 0xff,
            // the power state; PME status and the rest read as zero
            0x44 => *r = w & 3,
            MSI_CAP => *r = *r & !(1 << 16) | w & 1 << 16,
            0x54 => *r = w & !3,
            0x58 | 0x78 => *r = w,
            0x5c => *r = w & 0xffff,
            0x80 => *r = *r & 0xffff_0000 | w & 0x1fb,
            _ => {}
        }
    }

    /// The clock controller drives the "pcie_x" reset.
    pub fn set_reset(&mut self, on: bool) {
        if std::mem::replace(&mut self.in_reset, on) != on && on {
            self.reset_device();
        }
    }

    fn reset_device(&mut self) {
        self.ep = Self::endpoint();
        self.nic.bus_reset();
    }

    /// Send a message when the device's interrupt rises.
    pub fn update(&mut self) {
        let level = self.nic.irq();
        let was = std::mem::replace(&mut self.nic_level, level);
        if level && !was && self.ep[MSI_CAP / 4] & 1 << 16 != 0 && self.ep[1] & 4 != 0 {
            let address = (self.ep[MSI_CAP / 4 + 2] as u64) << 32 | self.ep[MSI_CAP / 4 + 1] as u64;
            let page = (self.afi[AFI_MSI_FPCI_BAR_ST / 4] as u64) << 8;
            let size = (self.afi[AFI_MSI_BAR_SZ / 4] as u64) << 12;
            if (page..page + size).contains(&address) {
                let vector = self.ep[MSI_CAP / 4 + 3] as usize & 0xff;
                self.afi[AFI_MSI_VEC / 4 + vector / 32] |= 1 << (vector % 32);
                self.msis += 1;
            }
        }
    }

    pub fn msi_irq(&self) -> bool {
        self.afi[AFI_INTR_MASK / 4] & 1 << 8 != 0 && (0..8).any(|k| self.afi[AFI_MSI_VEC / 4 + k] & self.afi[AFI_MSI_EN_VEC / 4 + k] != 0)
    }

    /// The controller's own interrupt: the device's INTA, without MSI.
    pub fn irq(&self) -> bool {
        self.link_up(1)
            && self.nic.irq()
            && self.ep[MSI_CAP / 4] & 1 << 16 == 0
            && self.ep[1] & 1 << 10 == 0
            && self.afi[AFI_INTR_MASK / 4] & 1 != 0
            && self.afi[AFI_SM_INTR_ENABLE / 4] & 1 != 0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn up() -> Pcie {
        let mut p = Pcie::new([2, 0, 0, 0, 0, 1]);
        let mut d = Dma { ram: &mut [], base: 0x8000_0000 };
        p.write(AFI + 0x118, 1, 4, &mut d);
        p
    }

    /// What `tegra_pcie_map_bus` computes.
    fn config(p: &mut Pcie, bus: u32, devfn: u32, reg: u32) -> u64 {
        let offset = (reg & 0xf00) << 16 | bus << 16 | devfn << 8 | reg & 0xff;
        let mut d = Dma { ram: &mut [], base: 0x8000_0000 };
        p.write(AFI + 0x30, 0xfe10_0000 + ((offset & !0xfff) >> 8), 4, &mut d);
        CONFIG + (offset & 0xfff) as u64
    }

    #[test]
    fn enumeration_and_message_interrupts() {
        let mut ram = vec![0; 0x1000];
        let mut p = up();
        assert_eq!(p.read(PORTS + 0x1000, 4), 0x0faf_10de);
        assert_eq!((p.read(PORTS + 0xf00, 4) >> 30 & 1, p.read(PORTS + 0x1f00, 4) >> 30 & 1), (0, 1));
        assert_ne!(p.read(PORTS + 0x1090, 4) & DL_ACTIVE, 0);
        let mut d = Dma { ram: &mut ram, base: 0x8000_0000 };
        p.write(PORTS + 0x1018, 0x0002_0200, 4, &mut d); // secondary bus 2
        let a = config(&mut p, 2, 0, 0);
        assert_eq!((p.read(a, 4), p.read(a + 2, 2)), (0x8168_10ec, 0x8168));
        let a = config(&mut p, 2, 8, 0);
        assert_eq!(p.read(a, 4), u32::MAX);
        let a = config(&mut p, 2, 0, 0x100);
        assert_eq!(p.read(a, 4), 0, "no extended capabilities");
        let mut d = Dma { ram: &mut ram, base: 0x8000_0000 };
        let a = config(&mut p, 2, 0, 0);
        p.write(a + 0x18, u32::MAX, 4, &mut d);
        assert_eq!(p.read(a + 0x18, 4), 0xffff_f004);
        p.write(a + 0x18, 0x1300_0000, 4, &mut d);
        p.write(a + 4, 6, 2, &mut d);
        assert_eq!(p.read(0x1300_0040, 4) >> 20 & 0xfcf, 0x541);
        // MSI vector 5 to the AFI's page
        p.write(a + 0x54, 0x9000_0000, 4, &mut d);
        p.write(a + 0x5c, 5, 2, &mut d);
        p.write(a + 0x52, 0x81, 2, &mut d);
        for (o, v) in [(0x64, 0x90_0000), (0x60, 1), (0x8c, 1 << 5), (0xb4, 0x100)] {
            p.write(AFI + o, v, 4, &mut d);
        }
        p.write(0x1300_003c, 0x20, 2, &mut d);
        p.nic.set_link(true);
        p.update();
        assert!(p.msi_irq() && !p.irq());
        p.write(AFI + 0x6c, 1 << 5, 4, &mut d);
        p.update();
        assert!(!p.msi_irq(), "one message for one rising edge");
        // masked and unmasked with the event still there: a new edge, a new message
        p.write(0x1300_003c, 0, 2, &mut d);
        p.write(0x1300_003c, 0x20, 2, &mut d);
        assert!(p.msi_irq());
        // PERST#: the device is gone, and fresh when it comes back
        p.write(AFI + 0x118, 0, 4, &mut d);
        assert_eq!(p.read(a, 4), u32::MAX);
        p.write(AFI + 0x118, 1, 4, &mut d);
        assert_eq!(p.read(a + 0x18, 4), 4);
    }
}
