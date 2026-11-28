//! Tegra234 PCIe controllers: the Synopsys DesignWare core with NVIDIA's application layer, as
//! Linux's `pcie-tegra194` driver (in its Tegra234 form) and the DesignWare host library drive
//! it. Two instances of the Orin Nano developer kit (carrier board P3768) are modelled, each
//! with the addresses, interrupts and BPMP IDs of the module's own device tree
//! (`tegra234-p3768-0000+p3767-0004-nv.dtb`):
//! - [`C1`] (`pcie@14100000`, PCI domain 1, x1): the M.2 Key E slot, where the Wi-Fi card sits;
//! - [`C8`] (`pcie@140a0000`, PCI domain 8, x2 on the GbE UPHY lanes): the carrier's
//!   Realtek RTL8111 Gigabit Ethernet controller (`0008:01:00.0`).
//!
//! Modelled:
//! - APPL (the application layer): pin mux (PERST#), LTSSM enable, the link status and LTSSM
//!   state the driver reads back, the interrupt registers (nothing raises them here).
//! - DBI: the root port's configuration space (NVIDIA 10de:229e on C1, 10de:229c on C8, a PCI bridge with the PM,
//!   MSI and PCIe capabilities and the extended capabilities the driver looks up), the port
//!   logic registers, and the integrated MSI controller (eight banks of 32 vectors; an MSI is
//!   a device's write to the address in `PCIE_MSI_ADDR_LO/HI`, it raises the "msi" SPI).
//! - The unrolled iATU (in the "atu_dma" region): eight outbound and eight inbound regions;
//!   CPU accesses to the config, I/O and memory windows go through the outbound regions
//!   (CFG0 to the card below the port, MEM to its BARs), as on the chip. Inbound, device
//!   addresses are physical addresses (no SMMU here).
//! - The P2U PHY bricks of the lanes (phy-tegra194-p2u): registers only.
//!
//! The link trains as soon as the driver enables the LTSSM with PERST# released and a card is
//! in the slot. The controller's power, clock, resets and UPHY go through the BPMP
//! (`bpmp.rs`).

use peripherals::pci::{Config, Function, Master};
use std::collections::HashMap;

pub const APPL_SIZE: u64 = 0x2_0000;
pub const CONFIG_SIZE: u64 = 0x4_0000;
pub const ATU_DMA_SIZE: u64 = 0x4_0000;
pub const DBI_SIZE: u64 = 0x4_0000;
pub const IO_SIZE: u64 = 0x10_0000;
pub const P2U_SIZE: u64 = 0x1_0000;
/// The 32-bit window's bus address
pub const MEM_BUS: u64 = 0x4000_0000;
pub const MEM_SIZE: u64 = 0x0800_0000;

/// One controller instance: where it is, its interrupts, its BPMP IDs, and its root port.
pub struct Instance {
    /// the device tree node's unit address (its "appl" region)
    pub appl: u64,
    pub config: u64,
    pub atu_dma: u64,
    pub dbi: u64,
    /// the I/O window (bus address = CPU address)
    pub io: u64,
    /// the 64-bit prefetchable window (bus address = CPU address)
    pub pref: u64,
    pub pref_size: u64,
    /// the 32-bit window, at bus address `MEM_BUS`
    pub mem: u64,
    /// the lanes' P2U PHY bricks
    pub p2u: &'static [u64],
    /// "intr" and "msi"
    pub intr_spi: u32,
    pub msi_spi: u32,
    /// the BPMP's controller ID, and the PCI domain (`linux,pci-domain`)
    pub controller: u32,
    pub domain: u32,
    pub lanes: u32,
    /// BPMP clock, resets (APB, core) and power domain, with the domain's name
    pub clock: u32,
    pub apb_reset: u32,
    pub core_reset: u32,
    pub power_domain: u32,
    pub power_name: &'static str,
    /// the root port's device ID
    pub device: u16,
}

/// C1: the M.2 Key E slot (x1).
pub const C1: Instance = Instance {
    appl: 0x1410_0000,
    config: 0x3000_0000,
    atu_dma: 0x3004_0000,
    dbi: 0x3008_0000,
    io: 0x3010_0000,
    pref: 0x20_8000_0000,
    pref_size: 0x2800_0000,
    mem: 0x20_a800_0000,
    p2u: &[0x03e3_0000],
    intr_spi: 45,
    msi_spi: 46,
    controller: 1,
    domain: 1,
    lanes: 1,
    clock: 0xdd,
    apb_reset: 0x7a,
    core_reset: 0x75,
    power_domain: 9,
    power_name: "pcie_x1a",
    device: 0x229e,
};

/// C8: the carrier's RTL8111 (x2, on the GbE UPHY's lanes 2 and 3).
pub const C8: Instance = Instance {
    appl: 0x140a_0000,
    config: 0x2a00_0000,
    atu_dma: 0x2a04_0000,
    dbi: 0x2a08_0000,
    io: 0x2a10_0000,
    pref: 0x32_4000_0000,
    pref_size: 0x2_e800_0000,
    mem: 0x35_2800_0000,
    p2u: &[0x03f4_0000, 0x03f5_0000],
    intr_spi: 356,
    msi_spi: 357,
    controller: 8,
    domain: 8,
    lanes: 2,
    clock: 0xac,
    apb_reset: 0x1a,
    core_reset: 0x19,
    power_domain: 13,
    power_name: "pcie_x4ca",
    device: 0x229c,
};

impl Instance {
    /// (base, size) of each region the controller decodes.
    fn regions(&self) -> impl Iterator<Item = (u64, u64)> + '_ {
        [
            (self.appl, APPL_SIZE),
            (self.config, CONFIG_SIZE),
            (self.atu_dma, ATU_DMA_SIZE),
            (self.dbi, DBI_SIZE),
            (self.io, IO_SIZE),
            (self.pref, self.pref_size),
            (self.mem, MEM_SIZE),
        ]
        .into_iter()
        .chain(self.p2u.iter().map(|&b| (b, P2U_SIZE)))
    }
}

const APPL_PINMUX: u32 = 0x0;
const APPL_PINMUX_PEX_RST: u32 = 1 << 0;
const APPL_CTRL: u32 = 0x4;
const APPL_CTRL_LTSSM_EN: u32 = 1 << 7;
const APPL_LINK_STATUS: u32 = 0xcc;
const APPL_DEBUG: u32 = 0xd0;
const LTSSM_L0: u32 = 0x11;

const PCIE_CAP: usize = 0x70;
const PORT_DEBUG0: usize = 0x728;
const PORT_DEBUG1: usize = 0x72c;
const MSI_ADDR_LO: usize = 0x820;
const MSI_ADDR_HI: usize = 0x824;
const MSI_CTRL0: usize = 0x828;
const MSI_CTRLS: usize = 8;
const ATU_VIEWPORT: usize = 0x900;

const ATU_REGIONS: usize = 8;
const ATU_CTRL1: usize = 0x00;
const ATU_CTRL2: usize = 0x04;
const ATU_LOWER_BASE: usize = 0x08;
const ATU_UPPER_BASE: usize = 0x0c;
const ATU_LOWER_LIMIT: usize = 0x10;
const ATU_LOWER_TARGET: usize = 0x14;
const ATU_UPPER_TARGET: usize = 0x18;
const ATU_UPPER_LIMIT: usize = 0x20;
const ATU_ENABLE: u32 = 1 << 31;
const ATU_TYPE_MEM: u32 = 0;
const ATU_TYPE_IO: u32 = 2;
const ATU_TYPE_CFG0: u32 = 4;

/// The integrated MSI controller: per bank enable, mask, status.
#[derive(Default)]
struct Msi {
    addr: u64,
    enable: [u32; MSI_CTRLS],
    mask: [u32; MSI_CTRLS],
    status: [u32; MSI_CTRLS],
}

/// What the card reaches as bus master: RAM, and the MSI controller at its address.
struct Port<'a> {
    ram: &'a mut [u8],
    msi: &'a mut Msi,
    log: bool,
    now: u64,
}

impl Port<'_> {
    fn range(&self, addr: u64, len: usize) -> Option<std::ops::Range<usize>> {
        let o = usize::try_from(addr.checked_sub(crate::RAM_BASE)?).ok()?;
        (o.checked_add(len)? <= self.ram.len()).then_some(o..o + len)
    }
}

impl Master for Port<'_> {
    fn read(&mut self, addr: u64, buf: &mut [u8]) -> bool {
        match self.range(addr, buf.len()) {
            Some(r) => {
                buf.copy_from_slice(&self.ram[r]);
                true
            }
            None => {
                if self.log {
                    eprintln!("[pcie] DMA read of {:#x} (+{}) outside RAM", addr, buf.len());
                }
                buf.fill(0xff);
                false
            }
        }
    }

    fn now(&self) -> u64 {
        self.now
    }

    fn ram(&mut self) -> Option<peripherals::dma::Dma<'_>> {
        Some(peripherals::dma::Dma { ram: &mut *self.ram, base: crate::RAM_BASE })
    }

    fn write(&mut self, addr: u64, data: &[u8]) -> bool {
        if addr == self.msi.addr && self.msi.addr != 0 {
            let vector = data.first().copied().unwrap_or(0) as usize;
            let (bank, bit) = (vector / 32 % MSI_CTRLS, vector % 32);
            if self.msi.enable[bank] & 1 << bit != 0 {
                self.msi.status[bank] |= 1 << bit;
            }
            return true;
        }
        match self.range(addr, data.len()) {
            Some(r) => {
                self.ram[r].copy_from_slice(data);
                true
            }
            None => {
                if self.log {
                    eprintln!("[pcie] DMA write to {:#x} (+{}) outside RAM", addr, data.len());
                }
                false
            }
        }
    }
}

pub struct Pcie {
    pub inst: &'static Instance,
    appl: Vec<u32>,
    /// the root port's configuration space and port logic (DBI 0..4 KiB)
    rp: Config,
    /// the rest of the DBI region
    dbi: HashMap<u32, u32>,
    msi: Msi,
    /// iATU regions: outbound then inbound, 0x200 bytes apart, the inbound ones at +0x100
    atu: Vec<u32>,
    /// the P2U bricks' registers by address
    p2u: HashMap<u64, u32>,
    /// LNKSTA's speed and width once the link is up: the lower of the port's and the card's
    link: u16,
    card: Option<Box<dyn Function>>,
    /// the BPMP has the controller powered, out of reset and its UPHY on
    pub powered: bool,
    /// the time (ns), as the SoC last set it
    pub now: u64,
    pub log: bool,
}

impl Pcie {
    pub fn new(inst: &'static Instance, mut card: Option<Box<dyn Function>>) -> Self {
        let mut rp = Config::new();
        rp.set16(0x00, 0x10de);
        rp.set16(0x02, inst.device);
        rp.set32(0x08, 0x0604_00a1);
        rp.set8(0x0e, 0x01); // type 1 header
        rp.set16(0x06, 0x0010);
        rp.writable(0x04, 2, 0x0547);
        rp.clearable(0x06, 2, 0xf900);
        rp.writable(0x0c, 2, 0xffff);
        rp.writable(0x18, 4, 0xffff_ffff); // bus numbers, secondary latency
        rp.writable(0x1c, 2, 0xf0f0); // I/O base and limit (16-bit)
        rp.clearable(0x1e, 2, 0xf900);
        rp.writable(0x20, 4, 0xfff0_fff0);
        rp.set32(0x24, 0x0001_0001); // prefetchable, 64-bit
        rp.writable(0x24, 4, 0xfff0_fff0);
        rp.writable(0x28, 4, 0xffff_ffff);
        rp.writable(0x2c, 4, 0xffff_ffff);
        rp.writable(0x30, 4, 0xffff_ffff);
        rp.set8(0x34, 0x40);
        rp.writable(0x3c, 1, 0xff);
        rp.set8(0x3d, 1);
        rp.writable(0x3e, 2, 0x0fff);
        // power management, MSI, PCI Express
        rp.capability(0x40, 0x01, 0x50);
        rp.set16(0x42, 0x5bc3);
        rp.writable(0x44, 2, 0x8103);
        rp.capability(0x50, 0x05, PCIE_CAP as u8);
        rp.set16(0x52, 0x018a);
        rp.writable(0x52, 2, 0x0071);
        rp.writable(0x54, 4, 0xffff_fffc);
        rp.writable(0x58, 4, 0xffff_ffff);
        rp.writable(0x5c, 2, 0xffff);
        rp.capability(PCIE_CAP, 0x10, 0);
        rp.set16(PCIE_CAP + 2, 0x0042); // v2, root port
        rp.set32(PCIE_CAP + 4, 0x0000_8002);
        rp.writable(PCIE_CAP + 8, 2, 0x7fff);
        rp.clearable(PCIE_CAP + 10, 2, 0x000f);
        rp.set32(PCIE_CAP + 0x0c, 0x0010_0c04 | inst.lanes << 4); // LNKCAP: 16 GT/s, ASPM, DLLLA reporting
        rp.writable(PCIE_CAP + 0x0c, 4, 0xffff_ffff);
        rp.writable(PCIE_CAP + 0x10, 2, 0x0fff);
        rp.writable(PCIE_CAP + 0x1c, 2, 0x001f); // root control
        rp.clearable(PCIE_CAP + 0x20, 4, 0x0001_0000);
        rp.set32(PCIE_CAP + 0x24, 0x0000_0800 | 0x1f); // DEVCAP2: LTR, timeouts
        rp.writable(PCIE_CAP + 0x28, 2, 0xffff);
        rp.set32(PCIE_CAP + 0x2c, 0x0000_001e); // LNKCAP2: 2.5 to 16 GT/s
        rp.writable(PCIE_CAP + 0x30, 2, 0xffff);
        rp.set16(PCIE_CAP + 0x30, 0x0004);
        // extended capabilities, as the driver finds them: AER, secondary PCIe (its lane
        // equalization control at 0x154), 16 GT/s, L1 PM substates, data link feature, the
        // RAS DES vendor capability (event counters)
        let ext = [(0x100usize, 0x0001u32, 1u32, 0x148usize), (0x148, 0x0019, 1, 0x168), (0x168, 0x0026, 1, 0x190), (0x190, 0x001e, 1, 0x1a0), (0x1a0, 0x0025, 1, 0x1b0), (0x1b0, 0x000b, 1, 0)];
        for (at, id, ver, next) in ext {
            rp.set32(at, id | ver << 16 | (next as u32) << 20);
        }
        for (start, end) in [(0x104, 0x148), (0x14c, 0x168), (0x16c, 0x190), (0x194, 0x1a0), (0x1a4, 0x1b0), (0x1b4, 0x2b0), (0x700, 0x1000)] {
            for o in (start..end).step_by(4) {
                rp.writable(o, 4, u32::MAX);
            }
        }
        rp.set32(0x194, 0x0000_001f); // L1SS capabilities
        let link = card.as_mut().map_or(0, |c| {
            let lnkcap = card_lnkcap(c.config()).unwrap_or(0x11);
            let speed = (lnkcap & 0xf).min(4);
            let width = (lnkcap >> 4 & 0x3f).min(inst.lanes);
            (speed | width << 4) as u16
        });
        Self {
            inst,
            link,
            appl: vec![0; (APPL_SIZE / 4) as usize],
            rp,
            dbi: HashMap::new(),
            msi: Msi::default(),
            atu: vec![0; ATU_REGIONS * 0x200 / 4],
            p2u: HashMap::new(),
            card,
            powered: true,
            now: 0,
            log: std::env::var_os("SBC_PCIE_LOG").is_some(),
        }
    }

    pub fn contains(&self, addr: u64) -> bool {
        self.inst.regions().any(|(base, size)| (base..base + size).contains(&addr))
    }

    fn p2u_register(&self, addr: u64) -> Option<u64> {
        self.inst.p2u.iter().any(|&b| (b..b + P2U_SIZE).contains(&addr)).then_some(addr & !3)
    }

    /// Run `f` on the card with its bus-master port onto `ram`.
    pub fn with_card<T: 'static, R>(&mut self, ram: &mut [u8], f: impl FnOnce(&mut T, &mut dyn Master) -> R) -> Option<R> {
        let card = self.card.as_mut()?.as_any().downcast_mut::<T>()?;
        let mut port = Port { ram, msi: &mut self.msi, log: self.log, now: self.now };
        Some(f(card, &mut port))
    }

    /// The card's timed work at `now`.
    pub fn tick(&mut self, now: u64, ram: &mut [u8]) {
        self.now = now;
        let log = self.log;
        if let Some(card) = self.card.as_mut() {
            let mut port = Port { ram, msi: &mut self.msi, log, now };
            card.tick(now, &mut port);
        }
    }

    pub fn next_event(&self) -> Option<u64> {
        self.card.as_ref()?.next_event()
    }

    pub fn link_up(&self) -> bool {
        self.card.is_some()
            && self.powered
            && self.appl[(APPL_CTRL / 4) as usize] & APPL_CTRL_LTSSM_EN != 0
            && self.appl[(APPL_PINMUX / 4) as usize] & APPL_PINMUX_PEX_RST != 0
    }

    /// The MSI controller's interrupt ("msi").
    pub fn msi_irq(&self) -> bool {
        (0..MSI_CTRLS).any(|k| self.msi.status[k] & !self.msi.mask[k] != 0)
    }

    pub fn read(&mut self, addr: u64, len: usize, ram: &mut [u8]) -> u64 {
        let i = self.inst;
        let v = if (i.appl..i.appl + APPL_SIZE).contains(&addr) {
            self.appl_read((addr - i.appl) as u32 & !3) as u64
        } else if (i.dbi..i.dbi + DBI_SIZE).contains(&addr) {
            self.dbi_read((addr - i.dbi) as usize & !3) as u64
        } else if (i.atu_dma..i.atu_dma + ATU_DMA_SIZE).contains(&addr) {
            let o = (addr - i.atu_dma) as usize & !3;
            self.atu.get(o / 4).copied().unwrap_or(0) as u64
        } else if let Some(r) = self.p2u_register(addr) {
            self.p2u.get(&r).copied().unwrap_or(0) as u64
        } else {
            return self.window_read(addr, len, ram);
        };
        let shift = 8 * (addr & 3);
        let mask = if len >= 4 { u32::MAX as u64 } else { (1u64 << (8 * len)) - 1 };
        (v >> shift) & mask
    }

    pub fn write(&mut self, addr: u64, value: u64, len: usize, ram: &mut [u8]) {
        let i = self.inst;
        if (i.appl..i.appl + APPL_SIZE).contains(&addr) {
            let o = (addr - i.appl) as u32 & !3;
            let v = merge(self.appl_read(o), addr, value, len);
            self.appl[(o / 4) as usize] = v;
        } else if (i.dbi..i.dbi + DBI_SIZE).contains(&addr) {
            self.dbi_write((addr - i.dbi) as usize, value as u32, len.min(4));
        } else if (i.atu_dma..i.atu_dma + ATU_DMA_SIZE).contains(&addr) {
            let o = (addr - i.atu_dma) as usize & !3;
            if let Some(w) = self.atu.get_mut(o / 4) {
                *w = merge(*w, addr, value, len);
            }
        } else if let Some(r) = self.p2u_register(addr) {
            let old = self.p2u.get(&r).copied().unwrap_or(0);
            self.p2u.insert(r, merge(old, addr, value, len));
        } else {
            self.window_write(addr, value, len, ram);
        }
    }

    fn appl_read(&self, o: u32) -> u32 {
        let up = self.link_up();
        match o {
            APPL_LINK_STATUS => u32::from(up),
            APPL_DEBUG if up => LTSSM_L0 << 3,
            _ => self.appl.get((o / 4) as usize).copied().unwrap_or(0),
        }
    }

    // ------------------------------------------------------------------ DBI

    fn dbi_read(&self, o: usize) -> u32 {
        let up = self.link_up();
        match o {
            _ if o == PCIE_CAP + 0x10 => {
                // LNKSTA: what the link trained at (the card's speed and width, at most the
                // port's), common clock, DLL active
                let sta = if up { 0x3000 | self.link as u32 } else { 0x1000 };
                self.rp.get16(o) as u32 | sta << 16
            }
            PORT_DEBUG0 if up => LTSSM_L0,
            PORT_DEBUG1 if up => 1 << 4,
            MSI_ADDR_LO => self.msi.addr as u32,
            MSI_ADDR_HI => (self.msi.addr >> 32) as u32,
            o if (MSI_CTRL0..MSI_CTRL0 + 12 * MSI_CTRLS).contains(&o) => {
                let (k, r) = ((o - MSI_CTRL0) / 12, (o - MSI_CTRL0) % 12);
                [self.msi.enable[k], self.msi.mask[k], self.msi.status[k]][r / 4]
            }
            // the viewport reads as all ones: the iATU is unrolled
            ATU_VIEWPORT => u32::MAX,
            o if o < 0x1000 => self.rp.read(o),
            o => self.dbi.get(&(o as u32)).copied().unwrap_or(0),
        }
    }

    fn dbi_write(&mut self, o: usize, value: u32, len: usize) {
        let aligned = o & !3;
        match aligned {
            MSI_ADDR_LO | MSI_ADDR_HI => {
                let v = merge(self.dbi_read(aligned), o as u64, value as u64, len);
                if aligned == MSI_ADDR_LO {
                    self.msi.addr = self.msi.addr & !0xffff_ffff | v as u64;
                } else {
                    self.msi.addr = self.msi.addr & 0xffff_ffff | (v as u64) << 32;
                }
            }
            a if (MSI_CTRL0..MSI_CTRL0 + 12 * MSI_CTRLS).contains(&a) => {
                let (k, r) = ((a - MSI_CTRL0) / 12, (a - MSI_CTRL0) % 12);
                let v = merge(self.dbi_read(a), o as u64, value as u64, len);
                match r {
                    0 => self.msi.enable[k] = v,
                    4 => self.msi.mask[k] = v,
                    _ => self.msi.status[k] &= !(value << (8 * (o & 3))),
                }
            }
            a if a < 0x1000 => self.rp.write(o, value, len),
            a => {
                let v = merge(self.dbi_read(a), o as u64, value as u64, len);
                self.dbi.insert(a as u32, v);
            }
        }
    }

    // ------------------------------------------------------------------ iATU and windows

    /// The outbound region an address hits: its type and the bus address.
    fn translate(&self, addr: u64) -> Option<(u32, u64)> {
        (0..ATU_REGIONS).find_map(|i| {
            let r = |o: usize| self.atu[(i * 0x200 + o) / 4] as u64;
            if r(ATU_CTRL2) as u32 & ATU_ENABLE == 0 {
                return None;
            }
            let base = r(ATU_UPPER_BASE) << 32 | r(ATU_LOWER_BASE);
            let upper_limit = if r(ATU_UPPER_LIMIT) != 0 { r(ATU_UPPER_LIMIT) } else { r(ATU_UPPER_BASE) };
            let limit = upper_limit << 32 | r(ATU_LOWER_LIMIT);
            let target = r(ATU_UPPER_TARGET) << 32 | r(ATU_LOWER_TARGET);
            (base..=limit).contains(&addr).then(|| (r(ATU_CTRL1) as u32 & 0x1f, target + (addr - base)))
        })
    }

    /// The card, when a type 0 configuration transaction with `target` (bus, device,
    /// function in its high bits) addresses it.
    fn config_target(&self, target: u64) -> bool {
        let (bus, dev, func) = ((target >> 24) & 0xff, (target >> 19) & 0x1f, (target >> 16) & 7);
        self.link_up() && bus == self.rp.get8(0x19) as u64 && dev == 0 && func == 0
    }

    fn window_read(&mut self, addr: u64, len: usize, ram: &mut [u8]) -> u64 {
        let ones = if len >= 8 { u64::MAX } else { (1u64 << (8 * len)) - 1 };
        let Some((kind, bus)) = self.translate(addr) else {
            if self.log {
                eprintln!("[pcie] read of {addr:#x}: no iATU region");
            }
            return ones;
        };
        match kind {
            ATU_TYPE_CFG0 if self.config_target(bus) => {
                let o = (addr & 0xfff) as usize;
                let card = self.card.as_mut().unwrap();
                let v = card.config().read(o);
                (v as u64 >> (8 * (o & 3))) & ones
            }
            ATU_TYPE_MEM => {
                let log = self.log;
                let Some(card) = self.card.as_mut() else { return ones };
                match card.decode(bus) {
                    Some((bar, off)) => {
                        let mut port = Port { ram, msi: &mut self.msi, log, now: self.now };
                        card.bar_read(bar, off, len, &mut port)
                    }
                    None => ones,
                }
            }
            _ => ones,
        }
    }

    fn window_write(&mut self, addr: u64, value: u64, len: usize, ram: &mut [u8]) {
        let Some((kind, bus)) = self.translate(addr) else {
            if self.log {
                eprintln!("[pcie] write to {addr:#x}: no iATU region");
            }
            return;
        };
        let log = self.log;
        match kind {
            ATU_TYPE_CFG0 if self.config_target(bus) => {
                let o = (addr & 0xfff) as usize;
                let card = self.card.as_mut().unwrap();
                card.config().write(o, value as u32, len.min(4));
                let mut port = Port { ram, msi: &mut self.msi, log, now: self.now };
                card.config_written(o, &mut port);
            }
            ATU_TYPE_MEM => {
                let Some(card) = self.card.as_mut() else { return };
                if let Some((bar, off)) = card.decode(bus) {
                    let mut port = Port { ram, msi: &mut self.msi, log, now: self.now };
                    card.bar_write(bar, off, value, len, &mut port);
                }
            }
            ATU_TYPE_IO => {}
            _ => {}
        }
    }
}

/// A function's LNKCAP, from the PCI Express capability in its list.
fn card_lnkcap(c: &mut Config) -> Option<u32> {
    let mut at = c.get8(0x34) as usize & !3;
    for _ in 0..48 {
        if at < 0x40 {
            return None;
        }
        if c.get8(at) == 0x10 {
            return Some(c.get32(at + 0x0c));
        }
        at = c.get8(at + 1) as usize & !3;
    }
    None
}

/// `value` (`len` bytes at `addr`) merged into the 32-bit register `old`.
fn merge(old: u32, addr: u64, value: u64, len: usize) -> u32 {
    if len >= 4 {
        return value as u32;
    }
    let shift = 8 * (addr & 3) as u32;
    let mask = ((1u64 << (8 * len)) - 1) as u32;
    old & !(mask << shift) | ((value as u32) & mask) << shift
}

#[cfg(test)]
mod tests {
    use super::*;
    use peripherals::bcm4350::Bcm4350;

    const APPL: u64 = C1.appl;
    const CONFIG: u64 = C1.config;
    const ATU_DMA: u64 = C1.atu_dma;
    const DBI: u64 = C1.dbi;
    const MEM: u64 = C1.mem;

    fn rc() -> (Pcie, Vec<u8>) {
        (Pcie::new(&C1, Some(Box::new(Bcm4350::new([2, 0, 0, 0, 0, 1])))), vec![0; 1 << 20])
    }

    fn atu(p: &mut Pcie, ram: &mut [u8], i: u64, kind: u32, cpu: u64, size: u64, target: u64) {
        let r = p.inst.atu_dma + i * 0x200;
        let limit = cpu + size - 1;
        for (o, v) in [
            (ATU_LOWER_BASE, cpu as u32),
            (ATU_UPPER_BASE, (cpu >> 32) as u32),
            (ATU_LOWER_LIMIT, limit as u32),
            (ATU_UPPER_LIMIT, (limit >> 32) as u32),
            (ATU_LOWER_TARGET, target as u32),
            (ATU_UPPER_TARGET, (target >> 32) as u32),
            (ATU_CTRL1, kind),
            (ATU_CTRL2, ATU_ENABLE),
        ] {
            p.write(r + o as u64, v as u64, 4, ram);
        }
    }

    #[test]
    fn root_port_and_card_through_the_iatu() {
        let (mut p, mut ram) = rc();
        assert_eq!(p.read(DBI, 4, &mut ram), 0x229e_10de);
        assert_eq!(p.read(DBI + ATU_VIEWPORT as u64, 4, &mut ram), u32::MAX as u64);
        // iATU region detection: eight outbound regions keep what is written
        p.write(ATU_DMA + 7 * 0x200 + 0x14, 0x1111_0000, 4, &mut ram);
        assert_eq!(p.read(ATU_DMA + 7 * 0x200 + 0x14, 4, &mut ram), 0x1111_0000);
        p.write(ATU_DMA + 8 * 0x200 + 0x14, 0x1111_0000, 4, &mut ram);
        assert_eq!(p.read(ATU_DMA + 8 * 0x200 + 0x14, 4, &mut ram), 0);
        // link down until the LTSSM runs with PERST# released
        assert_eq!(p.read(DBI + 0x82, 2, &mut ram) & 0x2000, 0);
        p.write(APPL + APPL_CTRL as u64, APPL_CTRL_LTSSM_EN as u64, 4, &mut ram);
        p.write(APPL + APPL_PINMUX as u64, APPL_PINMUX_PEX_RST as u64, 4, &mut ram);
        assert_eq!(p.read(DBI + 0x82, 2, &mut ram) & 0x2000, 0x2000);
        // bus 1 below the port; a CFG0 region onto it
        p.write(DBI + 0x18, 0x00ff_0100, 4, &mut ram);
        atu(&mut p, &mut ram, 0, ATU_TYPE_CFG0, CONFIG, CONFIG_SIZE, 1 << 24);
        assert_eq!(p.read(CONFIG, 4, &mut ram), 0x43a3_14e4);
        assert_eq!(p.read(CONFIG + 0x0a, 2, &mut ram), 0x0280);
        atu(&mut p, &mut ram, 0, ATU_TYPE_CFG0, CONFIG, CONFIG_SIZE, 1 << 24 | 1 << 19);
        assert_eq!(p.read(CONFIG, 4, &mut ram), u32::MAX as u64, "only device 0");
        // BAR0 at bus address 0x40000000 through the 32-bit window
        atu(&mut p, &mut ram, 0, ATU_TYPE_CFG0, CONFIG, CONFIG_SIZE, 1 << 24);
        p.write(CONFIG + 0x10, MEM_BUS, 4, &mut ram);
        p.write(CONFIG + 0x04, 0x6, 2, &mut ram);
        atu(&mut p, &mut ram, 1, ATU_TYPE_MEM, MEM, MEM_SIZE, MEM_BUS);
        // the BAR0 window starts on chipcommon: the chip ID
        assert_eq!(p.read(MEM, 4, &mut ram) & 0xffff, 0x4350);
    }

    #[test]
    fn c8_enumerates_the_rtl8111_and_its_msix_reaches_the_msi_controller() {
        use peripherals::rtl8168::Rtl8111;
        let mut p = Pcie::new(&C8, Some(Box::new(Rtl8111::new([2, 0, 0, 0, 0, 1]))));
        // RAM_BASE..+1 MiB stands for the guest's memory
        let mut ram = vec![0; 1 << 20];
        let (dbi, config, mem) = (C8.dbi, C8.config, C8.mem);
        assert!(p.contains(C8.appl) && p.contains(0x03f5_0000) && !p.contains(C1.appl));
        assert_eq!(p.read(dbi, 4, &mut ram), 0x229c_10de);
        assert_eq!(p.read(dbi + PCIE_CAP as u64 + 0x0c, 4, &mut ram) >> 4 & 0x3f, 2, "x2 port");
        p.write(C8.appl + APPL_CTRL as u64, APPL_CTRL_LTSSM_EN as u64, 4, &mut ram);
        p.write(C8.appl + APPL_PINMUX as u64, APPL_PINMUX_PEX_RST as u64, 4, &mut ram);
        // trained at the card's 2.5 GT/s x1
        assert_eq!(p.read(dbi + PCIE_CAP as u64 + 0x12, 2, &mut ram), 0x3011);
        p.write(dbi + 0x18, 0x00ff_0100, 4, &mut ram);
        atu(&mut p, &mut ram, 0, ATU_TYPE_CFG0, config, CONFIG_SIZE, 1 << 24);
        assert_eq!(p.read(config, 4, &mut ram), 0x8168_10ec);
        assert_eq!(p.read(config + 8, 4, &mut ram), 0x0200_0015);
        // BAR2 (registers) and BAR4 (MSI-X) in the 32-bit window
        p.write(config + 0x18, MEM_BUS, 4, &mut ram);
        p.write(config + 0x20, MEM_BUS + 0x4000, 4, &mut ram);
        p.write(config + 0x04, 0x6, 2, &mut ram);
        atu(&mut p, &mut ram, 1, ATU_TYPE_MEM, mem, MEM_SIZE, MEM_BUS);
        assert_eq!(p.read(mem + 0x40, 4, &mut ram) >> 20 & 0xfcf, 0x541, "TxConfig: RTL8168H");
        assert_eq!(p.read(mem, 4, &mut ram), 0x0000_0002, "MAC0..3");
        // MSI-X vector 0 to the DesignWare MSI controller's address, vector 5
        let msi = RAM_BASE_FOR_TEST + 0x8000;
        p.write(dbi + MSI_ADDR_LO as u64, msi, 4, &mut ram);
        p.write(dbi + MSI_CTRL0 as u64, 1 << 5, 4, &mut ram);
        p.write(mem + 0x4000, msi, 4, &mut ram);
        p.write(mem + 0x4008, 5, 4, &mut ram);
        p.write(mem + 0x400c, 0, 4, &mut ram);
        p.write(config + 0xb2, 0x8000, 2, &mut ram);
        // a link change, unmasked: the message arrives
        p.write(mem + 0x3c, 0x20, 2, &mut ram);
        p.with_card(&mut ram, |c: &mut Rtl8111, port| {
            c.nic.set_link(false);
            c.nic.set_link(true);
            c.service(port);
        });
        assert!(p.msi_irq());
    }

    const RAM_BASE_FOR_TEST: u64 = crate::RAM_BASE;

    #[test]
    fn msi_write_sets_status_and_the_line() {
        let (mut p, mut ram) = rc();
        p.write(DBI + MSI_ADDR_LO as u64, 0x8010_0000, 4, &mut ram);
        p.write(DBI + MSI_CTRL0 as u64, 1 << 3, 4, &mut ram);
        let mut port = Port { ram: &mut ram, msi: &mut p.msi, log: false, now: 0 };
        port.write(0x8010_0000, &3u32.to_le_bytes());
        assert!(p.msi_irq());
        p.write(DBI + MSI_CTRL0 as u64 + 4, 1 << 3, 4, &mut ram);
        assert!(!p.msi_irq(), "masked");
        p.write(DBI + MSI_CTRL0 as u64 + 4, 0, 4, &mut ram);
        p.write(DBI + MSI_CTRL0 as u64 + 8, 1 << 3, 4, &mut ram);
        assert!(!p.msi_irq(), "acknowledged");
    }
}
