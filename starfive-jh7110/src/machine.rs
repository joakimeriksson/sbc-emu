//! The JH7110 machine: four U74 harts (hart 0, the S7 monitor core, is held in reset as
//! the board's firmware leaves it) on one physical bus, scheduled round-robin in virtual
//! time.
use crate::devices::{sys_syscon, Clint, Crg, Plic, Pmu, Regs, TempSensor, HARTS};
use crate::i2c::{At24c04, Axp15060, DwI2c, PowerRequest};
use crate::crypto::Sec;
use crate::display::{Display, Mode, DC_BASE, DC_SIZE, HDMI_BASE, HDMI_SIZE, IRQ_DC, IRQ_HDMI};
use crate::dwmmc::DwMmc;
use crate::pcie::Port;
use emu_core::usb::{DevId, Devices};
use crate::gmac::{Gmac, Yt8531};
use emu_core::{FlatRam, Stop};
use peripherals::Uart;
use riscv64::{irq, Bus, Hart, Identity};
use std::collections::BTreeMap;

pub const RAM_BASE: u64 = 0x4000_0000;
/// The DRAM's uncached alias (`uncached-offset = <0x4 0x0>` in StarFive's jh7110.dtsi): the
/// same memory at +16 GiB, which the vendor kernel maps for DMA-coherent buffers.
pub const DRAM_UNCACHED: u64 = 0x4_0000_0000;
/// The Orange Pi RV's 4 GiB variant.
pub const RAM_SIZE: usize = 4 << 30;

/// `timebase-frequency` of the JH7110 device trees: `mtime` counts at 4 MHz.
pub const MTIME_HZ: u64 = 4_000_000;
const NS_PER_TICK: u64 = 1_000_000_000 / MTIME_HZ;

/// PLIC source numbers.
pub const IRQ_UART0: usize = 32;
pub const IRQ_SEC: usize = 28;
/// The STG syscon: the PCIe ports' link status (DATA_LINK_ACTIVE, bit 5) lives here.
pub const STG_SYSCON: u64 = 0x1024_0000;
const STG_PCIE_LNKSTA: [u64; 2] = [0x48 + 0x170, 0x1f8 + 0x170];
pub const IRQ_TEMP: usize = 81;
pub const TEMP: u64 = 0x120e_0000;
pub const IRQ_GMAC0: usize = 7;
pub const IRQ_GMAC1: usize = 78;
pub const GMAC0: u64 = 0x1603_0000;
pub const GMAC1: u64 = 0x1604_0000;
pub const IRQ_SDIO0: usize = 74;
pub const IRQ_SDIO1: usize = 75;
pub const SEC: u64 = 0x1600_0000;
pub const SDIO0: u64 = 0x1601_0000;
pub const SDIO1: u64 = 0x1602_0000;
/// The microSD slot's card-detect input (`cd-gpios = <&sysgpio 41 GPIO_ACTIVE_HIGH>`).
pub const SD_CD_GPIO: u64 = 41;

/// Instructions a hart runs before the next one gets its turn.
const QUANTUM: u64 = 1000;

pub const CLINT: u64 = 0x0200_0000;
pub const PLIC: u64 = 0x0c00_0000;
pub const UART0: u64 = 0x1000_0000;
/// The seven DesignWare I²C controllers and their PLIC sources; the PMIC and the board
/// EEPROM are on I2C5.
pub const I2C: [(u64, usize); 7] = [
    (0x1003_0000, 35),
    (0x1004_0000, 36),
    (0x1005_0000, 37),
    (0x1203_0000, 48),
    (0x1204_0000, 49),
    (0x1205_0000, 50),
    (0x1206_0000, 51),
];
pub const EEPROM_ADDR: u8 = 0x50;
/// The AXP15060 PMIC's address on I2C5.
pub const PMIC_ADDR: u8 = 0x36;
pub const STGCRG: u64 = 0x1023_0000;
pub const SYSCRG: u64 = 0x1302_0000;
pub const SYS_SYSCON: u64 = 0x1303_0000;
pub const SYSGPIO: u64 = 0x1304_0000;
/// The board's `gpio-restart` line: GPIO35 resets the board through the PMIC.
pub const RESTART_GPIO: u64 = 35;
pub const AONCRG: u64 = 0x1700_0000;
pub const PMU: u64 = 0x1703_0000;
pub const ISPCRG: u64 = 0x1981_0000;
pub const VOUTCRG: u64 = 0x295c_0000;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HartState {
    /// held in reset (the S7) or not yet started
    Off,
    Run,
}

pub struct Soc {
    pub ram: FlatRam,
    pub clint: Clint,
    pub plic: Plic,
    pub uart: Uart,
    pub syscrg: Crg,
    pub aoncrg: Crg,
    pub stgcrg: Crg,
    pub ispcrg: Crg,
    pub voutcrg: Crg,
    pub sys_syscon: Regs,
    pub pmu: Pmu,
    pub i2c: Vec<DwI2c>,
    /// SDIO0 (the Wi-Fi module's slot, empty here) and SDIO1 (the microSD slot)
    pub sec: Sec,
    /// PCIe0 (the VL805 USB 3.0 controller) and PCIe1 (the empty M.2 slot)
    pub pcie: [Port; 2],
    /// the USB devices: keyboard and tablet on the VL805's ports, an optional USB drive
    pub usb: Devices,
    pub temp: TempSensor,
    /// the DC8200 display controller and the Inno HDMI transmitter with its monitor
    pub display: Display,
    /// GMAC0 with the YT8531C PHY (MDIO address 0) and GMAC1 (no PHY on this board)
    pub gmac0: Gmac,
    pub gmac1: Gmac,
    /// the network GMAC0's port is plugged into (None: no cable)
    pub net: Option<sbc_net::net::VirtualNet>,
    /// the network behind the virtual Wi-Fi access point
    pub wifi_net: Option<sbc_net::net::VirtualNet>,
    /// when the network is polled next (ns)
    net_next: u64,
    pub sdio0: DwMmc,
    pub sdio1: DwMmc,
    /// GPIO35 was driven high as an output (`gpio-restart`)
    pub gpio_reset: bool,
    /// read-back stand-ins for unmodelled device pages
    pub stubs: BTreeMap<u64, Box<[u32; 1024]>>,
    /// virtual time (ns), kept here for `mtime`
    pub now: u64,
    /// the hart whose accesses are being executed
    pub cur: usize,
    irq_dirty: bool,
}

impl Soc {
    pub fn new(ram_size: usize) -> Self {
        Soc {
            ram: FlatRam::new(RAM_BASE, ram_size),
            clint: Clint::default(),
            plic: Plic::default(),
            uart: Uart::default(),
            syscrg: Crg::new(0x10000, 0x2f8, 0x308, 4),
            aoncrg: Crg::new(0x10000, 0x38, 0x3c, 1),
            stgcrg: Crg::new(0x10000, 0x74, 0x78, 1),
            ispcrg: Crg::new(0x10000, 0x38, 0x3c, 1),
            voutcrg: Crg::new(0x10000, 0x48, 0x4c, 1),
            sys_syscon: sys_syscon(),
            pmu: Pmu::default(),
            i2c: (0..I2C.len())
                .map(|k| {
                    let mut bus = DwI2c::default();
                    if k == 5 {
                        bus.attach(PMIC_ADDR, Box::new(Axp15060::default()));
                        let (lo, hi) = At24c04::pair([0xff; 512]);
                        bus.attach(EEPROM_ADDR, Box::new(lo));
                        bus.attach(EEPROM_ADDR + 1, Box::new(hi));
                    }
                    bus
                })
                .collect(),
            stubs: BTreeMap::new(),
            gpio_reset: false,
            sec: Sec::default(),
            pcie: [
                {
                    let mut p = Port::new(0x2b00_0000, 0x9_4000_0000, 56, true);
                    if let Some(x) = p.xhci.as_mut() {
                        x.attach(&[DevId::Keyboard, DevId::Tablet]);
                    }
                    p
                },
                Port::new(0x2c00_0000, 0x9_c000_0000, 57, false),
            ],
            usb: Devices::default(),
            temp: TempSensor::default(),
            display: Display::new(Mode::MODES[0]),
            gmac0: Gmac::new(Some((0, Yt8531::default()))),
            gmac1: Gmac::new(None),
            net: None,
            wifi_net: None,
            net_next: 0,
            sdio0: {
                // the AP6256 Wi-Fi module (BCM43456)
                let mut host = DwMmc::new(SDIO0, IRQ_SDIO0, 32);
                let mut chip = crate::wifi::Bcm43456::new();
                crate::sdio::SdioCard::set_trace(&mut chip, std::env::var_os("SBC_WIFI_TRACE").is_some());
                host.sdio = Some(Box::new(chip));
                host
            },
            sdio1: DwMmc::new(SDIO1, IRQ_SDIO1, 32),
            now: 0,
            cur: 1,
            irq_dirty: false,
        }
    }

    pub fn mtime(&self) -> u64 {
        self.now / NS_PER_TICK
    }

    /// Device interrupt levels into the PLIC.
    pub fn update_devices(&mut self) {
        let u = self.uart.irq();
        self.plic.set_level(IRQ_UART0, u);
        let (m0, m1) = (self.sdio0.irq_level(), self.sdio1.irq_level());
        self.plic.set_level(IRQ_SDIO0, m0);
        self.plic.set_level(IRQ_SDIO1, m1);
        let c = self.sec.irq();
        self.plic.set_level(IRQ_SEC, c);
        let (dc, hdmi) = (self.display.irq(), self.display.hdmi_irq());
        self.plic.set_level(IRQ_DC, dc);
        self.plic.set_level(IRQ_HDMI, hdmi);
        for k in 0..2 {
            self.pcie[k].update();
            let l = self.pcie[k].irq_level();
            self.plic.set_level(self.pcie[k].irq, l);
        }
        let (g0, g1) = (self.gmac0.irq(), self.gmac1.irq());
        self.plic.set_level(IRQ_GMAC0, g0);
        self.plic.set_level(IRQ_GMAC1, g1);
        for (k, &(_, irq)) in I2C.iter().enumerate() {
            let l = self.i2c[k].irq();
            self.plic.set_level(irq, l);
        }
    }

    /// Run the VL805's periodic work (retrying IN transfers of devices with input); returns
    /// its next deadline (ns).
    pub fn usb_tick(&mut self) -> u64 {
        let now = self.now;
        let Some(x) = self.pcie[0].xhci.as_mut() else { return u64::MAX };
        x.tick(now, &mut self.ram.mem, &mut self.usb);
        let next = x.next_event(&self.usb).unwrap_or(u64::MAX);
        self.update_devices();
        next
    }

    /// Plug a USB drive into the next free port of the VL805.
    pub fn attach_usb_drive(&mut self, disk: Box<dyn emu_core::disk::Disk + Send>) {
        self.usb.storage = Some(emu_core::usb::Storage::new(disk));
        if let Some(x) = self.pcie[0].xhci.as_mut() {
            x.attach(&[DevId::Keyboard, DevId::Tablet, DevId::Storage]);
        }
    }

    /// The HDMI picture as RGBA.
    pub fn render(&self, out: &mut Vec<u8>) -> Option<(u32, u32)> {
        self.display.render(&self.ram.mem, self.ram.base, out)
    }

    /// Exchange frames between GMAC0 and the network. Returns when to poll next (ns): soon
    /// while frames wait or the NAT relays connections, otherwise in 10 ms.
    pub fn pump_network(&mut self) {
        if self.now < self.net_next {
            return;
        }
        let now_us = self.now / 1000;
        match self.net.as_mut() {
            Some(net) => {
                for frame in std::mem::take(&mut self.gmac0.tx_out) {
                    let replies = net.handle(&frame, now_us);
                    self.gmac0.deliver(replies);
                }
                let polled = net.poll(now_us);
                self.gmac0.deliver(polled);
            }
            None => self.gmac0.tx_out.clear(),
        }
        self.gmac0.service(&mut self.ram);
        // Wi-Fi: the associated interface's frames go to the network behind the access point
        if let (Some(card), Some(wnet)) = (self.sdio0.sdio.as_mut(), self.wifi_net.as_mut()) {
            let mut replies = Vec::new();
            for f in card.take_tx() {
                replies.extend(wnet.handle(&f, now_us));
            }
            replies.extend(wnet.poll(now_us));
            for r in replies {
                card.deliver(&r);
            }
        }
        let busy = !self.gmac0.rx_queue.is_empty() || self.net.as_ref().is_some_and(|n| n.nat.is_some()) || self.wifi_net.is_some();
        self.net_next = self.now + if busy { 1_000_000 } else { 10_000_000 };
        self.update_devices();
    }

    /// The next network poll, while a network is connected (ns).
    pub fn network_deadline(&self) -> u64 {
        if self.net.is_some() && (self.gmac0.receiving() || !self.gmac0.tx_out.is_empty()) {
            self.net_next
        } else {
            u64::MAX
        }
    }

    /// The interrupt lines of hart `h`.
    pub fn lines(&self, h: usize) -> u64 {
        let mut l = 0;
        if self.clint.msip[h] {
            l |= irq::MSIP;
        }
        if self.mtime() >= self.clint.mtimecmp[h] {
            l |= irq::MTIP;
        }
        if Plic::context(h, false).is_some_and(|c| self.plic.asserted(c)) {
            l |= irq::MEIP;
        }
        if Plic::context(h, true).is_some_and(|c| self.plic.asserted(c)) {
            l |= irq::SEIP;
        }
        l
    }

    fn stub(&mut self, addr: u64) -> &mut u32 {
        let page = self.stubs.entry(addr & !0xfff).or_insert_with(|| Box::new([0; 1024]));
        &mut page[((addr & 0xfff) / 4) as usize]
    }

    fn dev_read(&mut self, a: u64, size: u8) -> Result<u64, ()> {
        let r32 = |v: u32| Ok(v as u64);
        for k in 0..2 {
            let p = &mut self.pcie[k];
            if p.in_bar(a) {
                let v = p.mem_read(a & !3);
                self.update_devices();
                self.irq_dirty = true;
                return r32(v);
            }
            if (p.apb..p.apb + 0x100_0000).contains(&a) {
                return r32(p.bridge_read(a - p.apb));
            }
            if (p.cfg..p.cfg + 0x1000_0000).contains(&a) {
                let v = p.cfg_read((a - p.cfg) & !3);
                return Ok(((v as u64) >> (8 * (a & 3))) & (u64::MAX >> (64 - 8 * size.min(4) as u64)));
            }
        }
        if let Some(k) = STG_PCIE_LNKSTA.iter().position(|&o| a == STG_SYSCON + o) {
            let link = (self.pcie[k].link_up() as u32) << 5;
            return r32(*self.stub(a) | link);
        }
        match a {
            _ if (CLINT..CLINT + 0x10000).contains(&a) => {
                let t = self.mtime();
                Ok(self.clint.read(a - CLINT, size, t))
            }
            _ if (PLIC..PLIC + 0x400_0000).contains(&a) => {
                let v = self.plic.read(a - PLIC);
                self.irq_dirty = true;
                r32(v)
            }
            _ if (UART0..UART0 + 0x10000).contains(&a) => {
                let v = self.uart.read(a - UART0);
                self.update_devices();
                self.irq_dirty = true;
                r32(v)
            }
            _ if self.sdio1.contains(a) => {
                let v = self.sdio1.read(a - SDIO1);
                self.update_devices();
                self.irq_dirty = true;
                r32(v)
            }
            _ if self.sdio0.contains(a) => r32(self.sdio0.read(a - SDIO0)),
            _ if (SEC..SEC + 0x4000).contains(&a) => r32(self.sec.read(a - SEC)),
            _ if (TEMP..TEMP + 0x10000).contains(&a) => r32(self.temp.read(a - TEMP)),
            _ if (GMAC0..GMAC0 + 0x10000).contains(&a) => r32(self.gmac0.read(a - GMAC0)),
            _ if (DC_BASE..DC_BASE + DC_SIZE).contains(&a) => {
                let v = self.display.dc_read(a - DC_BASE);
                self.update_devices();
                self.irq_dirty = true;
                r32(v)
            }
            _ if (HDMI_BASE..HDMI_BASE + HDMI_SIZE).contains(&a) => {
                let v = self.display.hdmi_read(a - HDMI_BASE);
                self.update_devices();
                self.irq_dirty = true;
                r32(v)
            }
            _ if (GMAC1..GMAC1 + 0x10000).contains(&a) => r32(self.gmac1.read(a - GMAC1)),
            // GPIOIN: the inputs the board wires (the SD card-detect switch)
            _ if a & !4 == SYSGPIO + 0x118 => {
                let inputs = (self.sdio1.card.present() as u64) << SD_CD_GPIO;
                r32((inputs >> (8 * (a - SYSGPIO - 0x118))) as u32)
            }
            _ if (SYSCRG..SYSCRG + 0x10000).contains(&a) => r32(self.syscrg.read(a - SYSCRG)),
            _ if (AONCRG..AONCRG + 0x10000).contains(&a) => r32(self.aoncrg.read(a - AONCRG)),
            _ if (STGCRG..STGCRG + 0x10000).contains(&a) => r32(self.stgcrg.read(a - STGCRG)),
            _ if (ISPCRG..ISPCRG + 0x10000).contains(&a) => r32(self.ispcrg.read(a - ISPCRG)),
            _ if (VOUTCRG..VOUTCRG + 0x10000).contains(&a) => r32(self.voutcrg.read(a - VOUTCRG)),
            _ if (SYS_SYSCON..SYS_SYSCON + 0x1000).contains(&a) => r32(self.sys_syscon.read(a - SYS_SYSCON)),
            _ if (PMU..PMU + 0x1000).contains(&a) => r32(self.pmu.read(a - PMU)),
            _ if I2C.iter().any(|&(b, _)| (b..b + 0x10000).contains(&a)) => {
                let k = I2C.iter().position(|&(b, _)| (b..b + 0x10000).contains(&a)).unwrap();
                let v = self.i2c[k].read(a - I2C[k].0);
                self.update_devices();
                self.irq_dirty = true;
                r32(v)
            }
            _ if a < RAM_BASE || (0x9_0000_0000..0xa_0000_0000).contains(&a) => {
                let lo = *self.stub(a & !3) as u64;
                Ok(match size {
                    8 => lo | (*self.stub((a & !3) + 4) as u64) << 32,
                    _ => (lo >> (8 * (a & 3))) & (u64::MAX >> (64 - 8 * size as u64)),
                })
            }
            _ => Err(()),
        }
    }

    fn dev_write(&mut self, a: u64, size: u8, v: u64) -> Result<(), ()> {
        let v32 = v as u32;
        for k in 0..2 {
            if self.pcie[k].in_bar(a) {
                self.pcie[k].mem_write(a & !3, v32, &mut self.ram.mem, &mut self.usb);
                self.update_devices();
                self.irq_dirty = true;
                return Ok(());
            }
            let (apb, cfg) = (self.pcie[k].apb, self.pcie[k].cfg);
            if (apb..apb + 0x100_0000).contains(&a) {
                self.pcie[k].bridge_write(a - apb, v32);
                self.update_devices();
                self.irq_dirty = true;
                return Ok(());
            }
            if (cfg..cfg + 0x1000_0000).contains(&a) {
                let o = (a - cfg) & !3;
                let v = if size >= 4 {
                    v32
                } else {
                    let sh = 8 * (a & 3);
                    let mask = ((1u64 << (8 * size as u64)) - 1) as u32;
                    (self.pcie[k].cfg_read(o) & !(mask << sh)) | ((v32 & mask) << sh)
                };
                self.pcie[k].cfg_write(o, v);
                return Ok(());
            }
        }
        match a {
            _ if (CLINT..CLINT + 0x10000).contains(&a) => {
                self.clint.write(a - CLINT, size, v);
                self.irq_dirty = true;
            }
            _ if (PLIC..PLIC + 0x400_0000).contains(&a) => {
                self.plic.write(a - PLIC, v32);
                self.irq_dirty = true;
            }
            _ if (UART0..UART0 + 0x10000).contains(&a) => {
                self.uart.write(a - UART0, v32);
                self.update_devices();
                self.irq_dirty = true;
            }
            _ if self.sdio1.contains(a) => {
                self.sdio1.write(a - SDIO1, v32, &mut self.ram);
                self.update_devices();
                self.irq_dirty = true;
            }
            _ if (TEMP..TEMP + 0x10000).contains(&a) => {
                if self.temp.write(a - TEMP, v32) {
                    self.plic.trigger(IRQ_TEMP);
                    self.irq_dirty = true;
                }
            }
            _ if (DC_BASE..DC_BASE + DC_SIZE).contains(&a) => {
                self.display.dc_write(a - DC_BASE, v32);
                self.update_devices();
                self.irq_dirty = true;
            }
            _ if (HDMI_BASE..HDMI_BASE + HDMI_SIZE).contains(&a) => {
                self.display.hdmi_write(a - HDMI_BASE, v32);
                self.update_devices();
                self.irq_dirty = true;
            }
            _ if (GMAC0..GMAC0 + 0x10000).contains(&a) => {
                if self.gmac0.write(a - GMAC0, v32) {
                    self.gmac0.service(&mut self.ram);
                    if !self.gmac0.tx_out.is_empty() {
                        self.net_next = self.now; // hand the frames to the network soon
                    }
                }
                self.update_devices();
                self.irq_dirty = true;
            }
            _ if (GMAC1..GMAC1 + 0x10000).contains(&a) => {
                if self.gmac1.write(a - GMAC1, v32) {
                    self.gmac1.service(&mut self.ram);
                    self.gmac1.tx_out.clear(); // nothing is connected to GMAC1
                }
                self.update_devices();
                self.irq_dirty = true;
            }
            _ if (SEC..SEC + 0x4000).contains(&a) => {
                self.sec.write(a - SEC, v32);
                self.update_devices();
                self.irq_dirty = true;
            }
            _ if self.sdio0.contains(a) => {
                self.sdio0.write(a - SDIO0, v32, &mut self.ram);
                self.update_devices();
                self.irq_dirty = true;
            }
            _ if (SYSCRG..SYSCRG + 0x10000).contains(&a) => self.syscrg.write(a - SYSCRG, v32),
            _ if (AONCRG..AONCRG + 0x10000).contains(&a) => self.aoncrg.write(a - AONCRG, v32),
            _ if (STGCRG..STGCRG + 0x10000).contains(&a) => self.stgcrg.write(a - STGCRG, v32),
            _ if (ISPCRG..ISPCRG + 0x10000).contains(&a) => self.ispcrg.write(a - ISPCRG, v32),
            _ if (VOUTCRG..VOUTCRG + 0x10000).contains(&a) => self.voutcrg.write(a - VOUTCRG, v32),
            _ if (SYS_SYSCON..SYS_SYSCON + 0x1000).contains(&a) => self.sys_syscon.write(a - SYS_SYSCON, v32),
            _ if (PMU..PMU + 0x1000).contains(&a) => self.pmu.write(a - PMU, v32),
            _ if I2C.iter().any(|&(b, _)| (b..b + 0x10000).contains(&a)) => {
                let k = I2C.iter().position(|&(b, _)| (b..b + 0x10000).contains(&a)).unwrap();
                self.i2c[k].write(a - I2C[k].0, v32);
                self.update_devices();
                self.irq_dirty = true;
            }
            _ if a < RAM_BASE || (0x9_0000_0000..0xa_0000_0000).contains(&a) => match size {
                8 => {
                    *self.stub(a & !3) = v as u32;
                    *self.stub((a & !3) + 4) = (v >> 32) as u32;
                }
                4 => *self.stub(a & !3) = v32,
                _ => {
                    let sh = 8 * (a & 3);
                    let mask = ((1u64 << (8 * size as u64)) - 1) as u32;
                    let w = self.stub(a & !3);
                    *w = (*w & !(mask << sh)) | ((v32 & mask) << sh);
                }
            },
            _ => return Err(()),
        }
        if a & !3 == SYSGPIO + 0x40 + 4 * (RESTART_GPIO / 4) || a & !3 == SYSGPIO + 4 * (RESTART_GPIO / 4) {
            // the pin's output signal (DOUT, 7 bits) is 1 (high) and its output enable
            // (DOEN, 6 bits) is 0 (enabled)
            let shift = 8 * (RESTART_GPIO % 4);
            let dout = (*self.stub(SYSGPIO + 0x40 + 4 * (RESTART_GPIO / 4)) >> shift) & 0x7f;
            let doen = (*self.stub(SYSGPIO + 4 * (RESTART_GPIO / 4)) >> shift) & 0x3f;
            if dout == 1 && doen == 0 {
                self.gpio_reset = true;
            }
        }
        Ok(())
    }
}

impl Bus for Soc {
    #[inline(always)]
    fn ram_base(&self) -> u64 {
        RAM_BASE
    }
    #[inline(always)]
    fn ram(&mut self) -> &mut [u8] {
        &mut self.ram.mem
    }
    fn read(&mut self, addr: u64, size: u8) -> Result<u64, ()> {
        let mut o = addr.wrapping_sub(RAM_BASE);
        if o >= self.ram.mem.len() as u64 {
            o = addr.wrapping_sub(RAM_BASE + DRAM_UNCACHED);
        }
        if o + size as u64 <= self.ram.mem.len() as u64 {
            let o = o as usize;
            let mut b = [0u8; 8];
            b[..size as usize].copy_from_slice(&self.ram.mem[o..o + size as usize]);
            return Ok(u64::from_le_bytes(b));
        }
        self.dev_read(addr, size)
    }
    fn write(&mut self, addr: u64, size: u8, v: u64) -> Result<(), ()> {
        let mut o = addr.wrapping_sub(RAM_BASE);
        if o >= self.ram.mem.len() as u64 {
            o = addr.wrapping_sub(RAM_BASE + DRAM_UNCACHED);
        }
        if o + size as u64 <= self.ram.mem.len() as u64 {
            let o = o as usize;
            self.ram.mem[o..o + size as usize].copy_from_slice(&v.to_le_bytes()[..size as usize]);
            return Ok(());
        }
        self.dev_write(addr, size, v)
    }
    fn mtime(&mut self) -> u64 {
        Soc::mtime(self)
    }
    #[inline(always)]
    fn irq_update(&mut self) -> Option<u64> {
        if self.irq_dirty {
            self.irq_dirty = false;
            Some(self.lines(self.cur))
        } else {
            None
        }
    }
}

pub struct Machine {
    pub harts: Vec<Hart>,
    pub state: [HartState; HARTS],
    pub soc: Soc,
    /// virtual nanoseconds not yet added to `soc.now` (1 ns per instruction per hart)
    rr: usize,
}

impl Machine {
    pub fn new(ram_size: usize) -> Self {
        let harts = (0..HARTS as u64).map(|h| Hart::new(Identity::sifive_u74(h))).collect();
        Machine { harts, state: [HartState::Off; HARTS], soc: Soc::new(ram_size), rr: 0 }
    }

    pub fn load(&mut self, addr: u64, bytes: &[u8]) -> Result<(), String> {
        let o = addr.checked_sub(RAM_BASE).ok_or("below RAM")? as usize;
        let end = o + bytes.len();
        if end > self.soc.ram.mem.len() {
            return Err(format!("{:#x}+{:#x} is beyond RAM", addr, bytes.len()));
        }
        self.soc.ram.mem[o..end].copy_from_slice(bytes);
        Ok(())
    }

    /// Virtual time in nanoseconds.
    pub fn now(&self) -> u64 {
        self.soc.now
    }

    pub fn instret(&self) -> u64 {
        self.harts.iter().map(|h| h.instret).sum()
    }

    /// The earliest timer deadline of a running hart, in ns.
    fn next_deadline(&self) -> u64 {
        (0..HARTS)
            .filter(|&h| self.state[h] == HartState::Run)
            .map(|h| self.soc.clint.mtimecmp[h])
            .min()
            .map_or(u64::MAX, |t| t.saturating_mul(NS_PER_TICK))
    }

    pub fn idle_until(&mut self, time_ns: u64) {
        if time_ns > self.soc.now && time_ns != u64::MAX {
            self.soc.now = time_ns;
        }
    }

    /// Run up to `budget` instructions across the harts. `Stop::Idle(deadline)` when every
    /// running hart waits for an interrupt that nothing raises before `deadline`.
    pub fn run(&mut self, budget: u64) -> Stop {
        let mut left = budget;
        while left > 0 {
            let mut ran = 0;
            for _ in 0..HARTS {
                let h = self.rr;
                self.rr = (self.rr + 1) % HARTS;
                if self.state[h] != HartState::Run {
                    continue;
                }
                self.soc.update_devices();
                let lines = self.soc.lines(h);
                let hart = &mut self.harts[h];
                hart.irq_lines = lines;
                if hart.wfi && !hart.interrupt_pending() {
                    continue;
                }
                self.soc.cur = h;
                // another hart may have written the reserved location since
                hart.reservation = u64::MAX;
                let n = hart.run(&mut self.soc, QUANTUM.min(left));
                ran = ran.max(n);
            }
            if std::mem::take(&mut self.soc.gpio_reset) {
                return Stop::Reset;
            }
            match self.soc.i2c[5].device::<Axp15060>(PMIC_ADDR).map(|p| std::mem::replace(&mut p.request, PowerRequest::None)) {
                Some(PowerRequest::Reset) => return Stop::Reset,
                Some(PowerRequest::Off) => return Stop::PowerOff,
                _ => {}
            }
            if self.soc.net.is_some() {
                self.soc.pump_network();
            }
            let vblank = self.soc.display.update(self.soc.now).unwrap_or(u64::MAX);
            let usb = self.soc.usb_tick();
            if ran == 0 {
                return Stop::Idle(self.next_deadline().min(self.soc.network_deadline()).min(vblank).min(usb));
            }
            self.soc.now += ran;
            left = left.saturating_sub(ran);
        }
        Stop::Budget
    }

    /// Plug GMAC0's port into the virtual 10.0.2.0/24 network (DHCP, DNS, ICMP); `nat` also
    /// relays guest TCP/UDP to the host's network (native builds only).
    pub fn connect_network(&mut self, nat: bool, log: bool) {
        let mut net = sbc_net::net::VirtualNet::new(log);
        #[cfg(not(target_arch = "wasm32"))]
        if nat {
            net.nat = Some(sbc_net::nat::Nat::new(log));
        }
        let _ = nat;
        self.soc.net = Some(net);
        // the Wi-Fi access point's network (isolated: DHCP, DNS and local services)
        self.soc.wifi_net = Some(sbc_net::net::VirtualNet::new(log));
        if let Some((_, phy)) = self.soc.gmac0.phy.as_mut() {
            phy.set_link(true);
        }
    }

    pub fn serial_input(&mut self, byte: u8) -> bool {
        let ok = self.soc.uart.push_rx(byte);
        self.soc.update_devices();
        ok
    }

    pub fn serial_output(&self) -> &[u8] {
        &self.soc.uart.out
    }

    pub fn take_serial_output(&mut self) -> Vec<u8> {
        std::mem::take(&mut self.soc.uart.out)
    }
}
