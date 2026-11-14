//! The BCM2711's PCIe root complex (`brcm,bcm2711-pcie`, Linux's `pcie-brcmstb`) with the
//! VL805 USB 3.0 host controller on bus 1, as wired on a Pi 4.
//!
//! The controller registers sit at 0xfd500000; offsets below 0x1000 are the root port's own
//! configuration space, and other buses are reached through the EXT_CFG index/data window.
//! The link is always up (x1, 5 GT/s). Memory space: CPU 0x6_0000_0000 is PCI 0xf8000000
//! (64 MiB, the vendor tree's `ranges`); inbound DMA is identity-mapped below 3 GiB. INTA
//! of the VL805 is GIC SPI 143. No MSI: the tree names no MSI controller, so `xhci-pci`
//! falls back to the legacy interrupt.
use crate::usbdev::Devices;
use crate::xhci::Xhci;

pub const BASE: u64 = 0xfd50_0000;
pub const SIZE: u64 = 0x9310;
pub const MEM_CPU: u64 = 0x6_0000_0000;
pub const MEM_PCI: u64 = 0xf800_0000;
pub const MEM_SIZE: u64 = 0x0400_0000;

const EXT_CFG_DATA: u64 = 0x8000;
const EXT_CFG_INDEX: u64 = 0x9000;
const MISC_PCIE_STATUS: u64 = 0x4068;
const MISC_REVISION: u64 = 0x406c;
const PRIV1_ID_VAL3: u64 = 0x043c;
const MDIO_WR_DATA: u64 = 0x1104;
const MDIO_RD_DATA: u64 = 0x1108;

/// The root port's PCI Express capability.
const RP_CAP: u64 = 0xac;
/// The VL805's.
const EP_CAP: u64 = 0x80;
const BAR_SIZE: u64 = 0x1000;

pub struct Pcie {
    regs: Vec<u32>,
    /// the VL805's configuration header (256 bytes)
    ep: [u32; 64],
    pub xhci: Xhci,
}

impl Default for Pcie { fn default() -> Self { Self::new() } }

impl Pcie {
    pub fn new() -> Self {
        let mut p = Pcie { regs: vec![0; (SIZE / 4) as usize], ep: [0; 64], xhci: Xhci::new() };
        p.reset_config();
        p
    }

    fn reset_config(&mut self) {
        let r = &mut self.regs;
        r[(PRIV1_ID_VAL3 / 4) as usize] = 0x0008_0000; // class code: endpoint until the driver sets the bridge class
        r[(0x04dc / 4) as usize] = 0x0000_0c12;          // PRIV1_LINK_CAPABILITY: x1, 5 GT/s, ASPM L0s/L1
        r[(0x4204 / 4) as usize] = 1 << 27;              // HARD_DEBUG: SerDes in IDDQ until enabled
        r[(0x9210 / 4) as usize] = 3;                    // RGR1_SW_INIT_1: bridge and PERST# in reset
        let e = &mut self.ep;
        *e = [0; 64];
        e[0x00 / 4] = 0x3483_1106;                       // VIA VL805
        e[0x04 / 4] = 0x0010_0000;                       // status: capability list
        e[0x08 / 4] = 0x0c03_3001;                       // xHCI, revision 1
        e[0x10 / 4] = 0x4;                               // BAR0: 64-bit memory
        e[0x2c / 4] = 0x3483_1106;                       // subsystem
        e[0x34 / 4] = EP_CAP as u32;
        e[0x3c / 4] = 0x0000_0100;                       // INTA
        e[0x50 / 4] = 0x0001_38c0;                       // VL805 firmware version
        let c = (EP_CAP / 4) as usize;
        e[c] = 0x0002_0010;                              // PCIe v2, endpoint
        e[c + 1] = 0x0000_8001;                          // DEVCAP: 256-byte payloads
        e[c + 3] = 0x0000_0012;                          // LNKCAP: x1, 5 GT/s
        e[c + 4] = 0x1012_0000;                          // LNKSTA: x1, 5 GT/s, slot clock
    }

    fn rc_mem_enabled(&self) -> bool { self.ep[1] & 2 != 0 }

    fn bar(&self) -> u64 { (self.ep[4] as u64 & !0xf) | (self.ep[5] as u64) << 32 }

    /// INTA of the VL805, if it is not masked in its command register.
    pub fn irq(&self) -> bool { self.ep[1] & (1 << 10) == 0 && self.xhci.irq() }

    fn in_reset(&self) -> bool { self.regs[(0x9210 / 4) as usize] & 1 != 0 }

    // ------------------------------------------------------------------ controller registers

    pub fn read(&mut self, off: u64) -> u32 {
        let off = off & !3;
        match off {
            MISC_PCIE_STATUS => if self.in_reset() { 0x80 } else { 0xb0 }, // RC mode; PHY link up, DL active
            MISC_REVISION => 0x0304,
            MDIO_RD_DATA => 0x8000_0000,
            MDIO_WR_DATA => self.regs[(off / 4) as usize] & !0x8000_0000,
            EXT_CFG_DATA..=0x8fff => self.cfg_read(self.regs[(EXT_CFG_INDEX / 4) as usize], off & 0xffc),
            o if o < 0x1000 => self.rp_read(o),
            o if o < SIZE => self.regs[(o / 4) as usize],
            _ => 0,
        }
    }

    pub fn write(&mut self, off: u64, v: u32, size: u32) {
        // narrower config writes merge into the dword
        let (a, sh) = (off & !3, (off & 3) * 8);
        let mask = if size >= 4 { u32::MAX } else { ((1u32 << (8 * size)) - 1) << sh };
        let merge = |old: u32| (old & !mask) | ((v << sh) & mask);
        match a {
            EXT_CFG_DATA..=0x8fff => {
                let idx = self.regs[(EXT_CFG_INDEX / 4) as usize];
                let old = self.cfg_read(idx, a & 0xffc);
                self.cfg_write(idx, a & 0xffc, merge(old));
            }
            o if o < 0x1000 => { let old = self.regs[(o / 4) as usize]; self.rp_write(o, merge(old)); }
            o if o < SIZE => { let old = self.regs[(o / 4) as usize]; self.regs[(o / 4) as usize] = merge(old); }
            _ => {}
        }
    }

    /// The root port's configuration space (bus 0, device 0).
    fn rp_read(&self, o: u64) -> u32 {
        let r = self.regs[(o / 4) as usize];
        match o {
            0x00 => 0x2711_14e4,
            0x04 => (r & 0xffff) | 0x0010 << 16,
            0x08 => (self.regs[(PRIV1_ID_VAL3 / 4) as usize] & 0xff_ffff) << 8 | 0x20,
            0x0c => (r & 0xffff) | 0x0001_0000, // header type 1: a bridge
            0x10 | 0x14 | 0x38 => 0,
            0x34 => RP_CAP as u32,
            0x3c => r & 0xffff_00ff, // no interrupt pin
            RP_CAP => 0x0042_0010,                         // PCIe v2, root port
            o if o == RP_CAP + 4 => 0x0000_8001,           // DEVCAP
            o if o == RP_CAP + 0x0c => 0x0010_0012,        // LNKCAP: x1, 5 GT/s, DL active reporting
            o if o == RP_CAP + 0x10 => (r & 0xffff) | 0x3012 << 16, // LNKSTA: 5 GT/s x1, DL active, slot clock
            o if o == RP_CAP + 0x2c => 0x6,                // LNKCAP2: 2.5 and 5 GT/s
            0x100 => 0,                                    // no extended capabilities
            _ => r,
        }
    }

    fn rp_write(&mut self, o: u64, v: u32) {
        match o {
            0x00 | 0x08 | 0x10 | 0x14 | 0x34 | 0x38 | RP_CAP => {}
            0x04 => self.regs[1] = v & 0xffff,
            _ => self.regs[(o / 4) as usize] = v,
        }
    }

    // ------------------------------------------------------------------ devices behind the port

    fn ep_present(&self, idx: u32) -> bool {
        // bus 1 (the root port's secondary bus), device 0, function 0; the link must be up
        let sec = self.regs[0x18 / 4] >> 8 & 0xff;
        !self.in_reset() && sec != 0 && idx >> 20 == sec && (idx >> 12) & 0xff == 0
    }

    fn cfg_read(&self, idx: u32, o: u64) -> u32 {
        if !self.ep_present(idx) { return u32::MAX; }
        self.ep.get((o / 4) as usize).copied().unwrap_or(0)
    }

    fn cfg_write(&mut self, idx: u32, o: u64, v: u32) {
        if !self.ep_present(idx) { return; }
        let e = &mut self.ep;
        match o {
            0x04 => e[1] = (e[1] & 0xffff_0000 & !(v & 0xf900_0000)) | (v & 0x0547), // command; RW1C status
            0x0c => e[3] = v & 0xffff,
            0x10 => e[4] = (v & !(BAR_SIZE as u32 - 1)) | 0x4,
            0x14 => e[5] = v,
            0x3c => e[0xf] = (e[0xf] & !0xff) | (v & 0xff),
            o if o == EP_CAP + 8 => e[(o / 4) as usize] = v & 0xffff,  // DEVCTL
            o if o == EP_CAP + 0x10 => e[(o / 4) as usize] = (e[(o / 4) as usize] & 0xffff_0000) | (v & 0xffff), // LNKCTL
            _ => {}
        }
    }

    /// The xHCI registers, if CPU address `a` hits the VL805's BAR.
    fn bar_off(&self, a: u64) -> Option<u64> {
        let pci = a.checked_sub(MEM_CPU)? + MEM_PCI;
        let bar = self.bar();
        (self.rc_mem_enabled() && bar != 0 && (bar..bar + BAR_SIZE).contains(&pci)).then(|| pci - bar)
    }

    pub fn mem_read(&mut self, a: u64) -> u32 {
        match self.bar_off(a) { Some(o) => self.xhci.read(o & !3), None => u32::MAX }
    }

    pub fn mem_write(&mut self, a: u64, v: u32, ram: &mut [u8], devs: &mut Devices) {
        if let Some(o) = self.bar_off(a) { self.xhci.write(o & !3, v, ram, devs); }
    }

    pub fn save_state(&self, w: &mut emu_core::snap::W) {
        w.tag("pcie-v1");
        w.u32(self.regs.len() as u32);
        for &v in &self.regs { w.u32(v); }
        for &v in &self.ep { w.u32(v); }
        self.xhci.save_state(w);
    }
    pub fn load_state(&mut self, r: &mut emu_core::snap::R) -> Result<(), emu_core::snap::SnapError> {
        r.tag("pcie-v1")?;
        let n = r.u32()? as usize;
        self.regs = (0..n).map(|_| r.u32()).collect::<Result<_, _>>()?;
        for v in self.ep.iter_mut() { *v = r.u32()?; }
        self.xhci.load_state(r)
    }
}
