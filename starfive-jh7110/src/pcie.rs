//! The JH7110's two PCIe 2.0 root ports (PLDA XpressRich, Linux `pcie-starfive` +
//! `pcie-plda-host`) and the Orange Pi RV's VIA VL805 USB 3.0 host controller on PCIe0, which
//! drives the board's four USB-A ports. PCIe1 is the M.2 M-key slot: empty, or an NVMe SSD
//! (`--nvme IMAGE`, `nvme.rs`, class 010802, a 16 KiB BAR).
//!
//! Each port has its bridge registers in the "apb" window and an ECAM configuration window
//! ("cfg"): bus 0 device 0 is the root port (a type-1 bridge, whose BARs the driver hides),
//! and the root port's secondary bus device 0 is the VL805 (`1106:3483`, class 0c0330, one
//! 4 KiB 64-bit memory BAR, INTA). Outbound and inbound address translation are 1:1 as the
//! driver programs them, so a BAR address is a CPU address and a device's DMA address is a
//! physical address. The VL805 signals with MSI (one vector, 64-bit address capability at
//! 0x50), as on the board: when the xHCI's interrupt rises, the message write to the PLDA's
//! IMSI_ADDR (PCIe address 0x190, where Linux points it) sets the vector's bit in
//! ISTATUS_MSI. ISTATUS_LOCAL's MSI bit (28) is the summary of ISTATUS_MSI: it stays set
//! until the vectors are acknowledged (Linux's chained handler acks bit 28 before it reads
//! it, so the bit cannot be a latch). Without MSI enabled the xHCI's line is
//! INTA (ISTATUS_LOCAL bit 24). The port's interrupt is ISTATUS_LOCAL & IMASK_LOCAL.
//! Link-up is the STG syscon's DATA_LINK_ACTIVE bit for the port.
use crate::nvme::Nvme;
use crate::xhci::Xhci;
use emu_core::usb::Devices;

const IMASK_LOCAL: u64 = 0x180;
const ISTATUS_LOCAL: u64 = 0x184;
const IMSI_ADDR: u64 = 0x190;
const ISTATUS_MSI: u64 = 0x194;
const INTA: u32 = 1 << 24;
const MSI: u32 = 1 << 28;
/// the VL805's MSI capability (config dwords 0x50..0x5c)
const MSI_CAP: usize = 0x50 / 4;

const VL805_ID: u32 = 0x3483_1106;
const PLDA_ID: u32 = 0x1111_1556;
const XHCI_BAR_SIZE: u64 = 0x1000;
/// sbc-emu's virtual NVMe SSD (no real vendor's identity)
const NVME_ID: u32 = 0x0001_fffd;
const NVME_BAR_SIZE: u64 = 0x4000;

pub struct Port {
    pub apb: u64,
    pub cfg: u64,
    pub irq: usize,
    bridge: Vec<u32>,
    root: [u32; 64],
    ep: [u32; 64],
    /// the VL805 behind this port, if fitted
    pub xhci: Option<Xhci>,
    /// an NVMe SSD in this port's M.2 slot
    pub nvme: Option<Nvme>,
    /// the xHCI's interrupt level at the last update (MSI is sent on its rising edge)
    last_level: bool,
    pub msis: u64,
}

impl Port {
    pub fn new(apb: u64, cfg: u64, irq: usize, vl805: bool) -> Self {
        let mut root = [0u32; 64];
        root[0] = PLDA_ID;
        root[2] = 0x0604_0000; // PCI-to-PCI bridge
        root[3] = 0x0001_0000; // header type 1
        let mut ep = [0u32; 64];
        ep[0] = VL805_ID;
        ep[2] = 0x0c03_3001; // xHCI, revision 1
        ep[4] = 0x4; // BAR0: 64-bit memory
        ep[0x3c / 4] = 0x0100; // INTA
        ep[0x60 / 4] = 0x30; // SBRN: USB 3.0
        ep[1] = 1 << 20; // status: capability list
        ep[0x34 / 4] = 0x50;
        ep[MSI_CAP] = 0x0080_0005; // MSI, 64-bit, one vector
        let mut bridge = vec![0; 0x1000 / 4];
        bridge[(IMSI_ADDR / 4) as usize] = IMSI_ADDR as u32;
        Port { apb, cfg, irq, bridge, root, ep, xhci: vl805.then(Xhci::new), nvme: None, last_level: false, msis: 0 }
    }

    /// Fit an NVMe SSD (the endpoint's identity becomes the NVMe controller's).
    pub fn attach_nvme(&mut self, nvme: Nvme) {
        self.xhci = None;
        self.nvme = Some(nvme);
        self.ep[0] = NVME_ID;
        self.ep[2] = 0x0108_0200; // mass storage, NVM, NVMe
    }

    pub fn link_up(&self) -> bool {
        self.xhci.is_some() || self.nvme.is_some()
    }

    fn bar_size(&self) -> u64 {
        if self.nvme.is_some() { NVME_BAR_SIZE } else { XHCI_BAR_SIZE }
    }

    fn msi_enabled(&self) -> bool {
        self.ep[MSI_CAP] & (1 << 16) != 0
    }

    /// The level-derived ISTATUS_LOCAL bits: INTx from the device, the MSI summary.
    fn intx(&self) -> u32 {
        let mut v = if self.bridge[(ISTATUS_MSI / 4) as usize] != 0 { MSI } else { 0 };
        if !self.msi_enabled() && self.xhci.as_ref().is_some_and(|x| x.irq()) {
            v |= INTA;
        }
        v
    }

    /// Follow the device's interrupt: on a rising edge with MSI enabled, the message write.
    pub fn update(&mut self) {
        if let Some(n) = self.nvme.as_mut() {
            if std::mem::take(&mut n.irq_request) && self.ep[MSI_CAP] & (1 << 16) != 0 {
                self.send_msi();
            }
            return;
        }
        let level = self.xhci.as_ref().is_some_and(|x| x.irq());
        if level && self.msi_enabled()
            && self.send_msi() {
                if let Some(x) = self.xhci.as_mut() {
                    x.msi_sent();
                }
            }
        self.last_level = self.xhci.as_ref().is_some_and(|x| x.irq());
    }

    /// The port's interrupt to the PLIC.
    pub fn irq_level(&self) -> bool {
        (self.bridge[(ISTATUS_LOCAL / 4) as usize] | self.intx()) & self.bridge[(IMASK_LOCAL / 4) as usize] != 0
    }

    pub fn bridge_read(&self, off: u64) -> u32 {
        match off {
            ISTATUS_LOCAL => self.bridge[(off / 4) as usize] | self.intx(),
            _ => self.bridge.get((off / 4) as usize).copied().unwrap_or(0),
        }
    }

    pub fn bridge_write(&mut self, off: u64, v: u32) {
        match off {
            // write-1-to-clear; INTx follows the device's line
            ISTATUS_LOCAL | ISTATUS_MSI => self.bridge[(off / 4) as usize] &= !v,
            _ => {
                if let Some(r) = self.bridge.get_mut((off / 4) as usize) {
                    *r = v;
                }
            }
        }
    }

    /// (bus, device, function, register) of a configuration-window offset (ECAM).
    fn decode(off: u64) -> (u32, u32, u32, usize) {
        ((off >> 20) as u32 & 0xff, (off >> 15) as u32 & 0x1f, (off >> 12) as u32 & 7, ((off & 0xfff) / 4) as usize)
    }

    fn secondary(&self) -> u32 {
        (self.root[6] >> 8) & 0xff
    }

    pub fn cfg_read(&self, off: u64) -> u32 {
        let (bus, dev, func, reg) = Self::decode(off);
        if bus == 0 && dev == 0 && func == 0 {
            return self.root.get(reg).copied().unwrap_or(0);
        }
        if self.link_up() && bus != 0 && bus == self.secondary() && dev == 0 && func == 0 {
            return self.ep.get(reg).copied().unwrap_or(0);
        }
        u32::MAX
    }

    pub fn cfg_write(&mut self, off: u64, v: u32) {
        let (bus, dev, func, reg) = Self::decode(off);
        if bus == 0 && dev == 0 && func == 0 {
            // identity, class and header type are read-only; the driver hides the BARs
            if !matches!(reg, 0 | 2 | 3 | 4 | 5) && reg < 64 {
                self.root[reg] = v;
            }
            return;
        }
        if self.link_up() && bus != 0 && bus == self.secondary() && dev == 0 && func == 0 {
            match reg {
                0 | 2 | 3 => {}
                1 => self.ep[1] = (self.ep[1] & 0xffff_0000) | (v & 0x0547),
                // BAR0: 64-bit, non-prefetchable, 4 KiB (xHCI) or 16 KiB (NVMe)
                4 => self.ep[4] = (v & !(self.bar_size() as u32 - 1)) | 0x4,
                5 => self.ep[5] = v,
                6..=9 => {}
                15 => self.ep[15] = (self.ep[15] & 0xffff_ff00) | (v & 0xff),
                13 => {}
                MSI_CAP => self.ep[MSI_CAP] = (self.ep[MSI_CAP] & !0x0071_0000) | (v & 0x0001_0000),
                r if r < 64 => self.ep[r] = v,
                _ => {}
            }
        }
    }

    /// The endpoint's MSI write, when it goes to the PLDA's MSI address.
    fn send_msi(&mut self) -> bool {
        let addr = (self.ep[MSI_CAP + 2] as u64) << 32 | self.ep[MSI_CAP + 1] as u64;
        if addr != self.bridge[(IMSI_ADDR / 4) as usize] as u64 {
            return false;
        }
        let vector = self.ep[MSI_CAP + 3] & 31;
        self.bridge[(ISTATUS_MSI / 4) as usize] |= 1 << vector;
        self.msis = self.msis.wrapping_add(1);
        true
    }

    /// A one-line state summary for run reports.
    pub fn summary(&self) -> String {
        format!(
            "MSI cap {:#010x} addr {:#x}:{:#x} data {:#x}, {} MSIs, IMASK_LOCAL {:#x} ISTATUS_LOCAL {:#x} ISTATUS_MSI {:#x}, xHCI irq {}",
            self.ep[MSI_CAP], self.ep[MSI_CAP + 2], self.ep[MSI_CAP + 1], self.ep[MSI_CAP + 3], self.msis,
            self.bridge[(IMASK_LOCAL / 4) as usize], self.bridge_read(ISTATUS_LOCAL), self.bridge[(ISTATUS_MSI / 4) as usize],
            self.xhci.as_ref().is_some_and(|x| x.irq())
        )
    }

    /// The VL805's register window, when its memory decoding is on.
    pub fn bar(&self) -> Option<u64> {
        if !self.link_up() || self.ep[1] & 2 == 0 {
            return None;
        }
        let addr = (self.ep[5] as u64) << 32 | (self.ep[4] & !0xf) as u64;
        (addr != 0).then_some(addr)
    }

    pub fn in_bar(&self, a: u64) -> bool {
        self.bar().is_some_and(|b| (b..b + self.bar_size()).contains(&a))
    }

    pub fn mem_read(&mut self, a: u64) -> u32 {
        match (self.bar(), self.xhci.as_mut(), self.nvme.as_mut()) {
            (Some(b), Some(x), _) => x.read(a - b),
            (Some(b), _, Some(n)) => n.read(a - b),
            _ => u32::MAX,
        }
    }

    pub fn mem_write(&mut self, a: u64, v: u32, ram: &mut [u8], devs: &mut Devices) {
        match (self.bar(), self.xhci.as_mut(), self.nvme.as_mut()) {
            (Some(b), Some(x), _) => x.write(a - b, v, ram, devs),
            (Some(b), _, Some(n)) => n.write(a - b, v, ram, crate::machine::RAM_BASE),
            _ => {}
        }
    }
}

// Snapshots. The port's addresses and interrupt are the board's.
emu_core::snap_struct!(Port { root, ep, last_level, msis } fixed { bridge } fitted { xhci: "the VL805 USB controller", nvme: "an NVMe SSD (--nvme)" } skip { apb, cfg, irq });

#[cfg(test)]
mod tests {
    use super::*;

    fn ecam(bus: u64, dev: u64, reg: u64) -> u64 {
        bus << 20 | dev << 15 | reg
    }

    #[test]
    fn enumeration_finds_the_root_port_and_the_vl805() {
        let mut p = Port::new(0x2b00_0000, 0x9_4000_0000, 56, true);
        assert_eq!(p.cfg_read(ecam(0, 0, 0)), PLDA_ID);
        assert_eq!(p.cfg_read(ecam(0, 1, 0)), u32::MAX);
        assert_eq!(p.cfg_read(ecam(1, 0, 0)), u32::MAX, "no secondary bus yet");
        p.cfg_write(ecam(0, 0, 0x18), 0x0001_0100); // primary 0, secondary 1, subordinate 1
        assert_eq!(p.cfg_read(ecam(1, 0, 0)), VL805_ID);
        assert_eq!(p.cfg_read(ecam(1, 0, 8)) >> 8, 0x0c0330);
        // BAR sizing
        p.cfg_write(ecam(1, 0, 0x10), u32::MAX);
        assert_eq!(p.cfg_read(ecam(1, 0, 0x10)), 0xffff_f004);
        p.cfg_write(ecam(1, 0, 0x10), 0x3000_0000);
        p.cfg_write(ecam(1, 0, 0x14), 0);
        assert_eq!(p.bar(), None, "memory decoding off");
        p.cfg_write(ecam(1, 0, 4), 0x6);
        assert_eq!(p.bar(), Some(0x3000_0000));
        assert!(p.in_bar(0x3000_0010));
        // xHCI capability registers: CAPLENGTH 0x20
        assert_eq!(p.mem_read(0x3000_0000) & 0xff, 0x20);
        // MSI to IMSI_ADDR, vector 3
        assert_eq!(p.cfg_read(ecam(1, 0, 0x34)) & 0xff, 0x50);
        p.cfg_write(ecam(1, 0, 0x54), 0x190);
        p.cfg_write(ecam(1, 0, 0x58), 0);
        p.cfg_write(ecam(1, 0, 0x5c), 3);
        p.cfg_write(ecam(1, 0, 0x50), 1 << 16);
        p.bridge_write(IMASK_LOCAL, MSI);
        // USBCMD.INTE + RS, then IMAN.IE: no event yet, no interrupt
        p.mem_write(0x3000_0020, 0x5, &mut [], &mut Devices::default());
        p.update();
        assert!(!p.irq_level());
        let empty = Port::new(0x2c00_0000, 0x9_c000_0000, 57, false);
        assert!(!empty.link_up());
        assert_eq!(empty.cfg_read(ecam(1, 0, 0)), u32::MAX);
    }
}
