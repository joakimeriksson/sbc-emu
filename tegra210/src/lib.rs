//! NVIDIA Tegra X1 (T210) and the Jetson Nano: four Cortex-A57 cores, a GIC-400, the
//! Tegra timers, clock/reset controller and PMC, UART A, the fuses and chip ID, I²C with the
//! MAX77620 PMIC, GPIO, the SDMMC controllers, PCIe with the RTL8111H Ethernet controller, USB
//! 3.0 (XUSB), the display controllers with HDMI, the SMMU, host1x, and RAM at 0x8000_0000. Direct Linux boot with the stock device tree; PSCI is answered by the host (the
//! role of the secure monitor on a real Nano). Device addresses no model claims behave
//! as read-back registers, and their pages are logged, so drivers probe without faults.
//! Docs: docs/jetson-nano.md (plan: docs/history/jetson-nano-plan.md).
pub mod apbdma;
pub mod boot;
pub mod car;
pub mod dc;
pub mod gpio;
pub mod gpu;
pub mod host1x;
pub mod i2c;
pub mod max77620;
pub mod padctl;
pub mod pcie;
pub mod pmc;
pub mod rtl8168;
pub mod sdhci;
pub mod smmu;
pub mod sor;
pub mod timer;
pub mod xusb;

use arm_a64::{Cpu, CpuProfile};
use emu_core::{Bus, Core, Fault, Stop, Trap};
pub use peripherals::dw_hdmi::Mode;
use peripherals::{Gic, Uart};

pub const RAM_BASE: u64 = 0x8000_0000;
/// The Nano's DRAM is 4 GiB; a smaller emulated amount is reported in /memory.
pub const MAX_RAM: u64 = 4 << 30;
/// The arch timer's frequency (clock M, the 38.4 MHz oscillator / 2).
pub const CNTFRQ: u64 = 19_200_000;
pub const GICD: u64 = 0x5004_1000;
pub const GICC: u64 = 0x5004_2000;
pub const TMR: u64 = 0x6000_5000;
pub const CAR: u64 = 0x6000_6000;
pub const APBMISC: u64 = 0x7000_0800;
pub const UARTA: u64 = 0x7000_6000;
pub const UARTA_IRQ: usize = 32 + 36;
pub const GPIO: u64 = 0x6000_d000;
/// The I²C controllers and their interrupts; the PMIC is on I²C5.
pub const I2C: [(u64, usize); 6] =
    [(0x7000_c000, 32 + 0x26), (0x7000_c400, 32 + 0x54), (0x7000_c500, 32 + 0x5c), (0x7000_c700, 32 + 0x78), (0x7000_d000, 32 + 0x35), (0x7000_d100, 32 + 0x3f)];
/// their numbers in the clock and reset controller
const I2C_RESETS: [usize; 6] = [12, 54, 67, 103, 47, 166];
pub const I2C5: usize = 4;
/// The VI's I2C controller.
const I2C_VI_RESET: usize = 208;
const I2C_VI_IRQ: usize = 32 + 17;
/// The SDMMC controllers, 0x200 apart, and their interrupts; the microSD slot is on SDMMC1.
pub const SDMMC: u64 = 0x700b_0000;
pub const SDMMC_IRQS: [usize; 4] = [32 + 0x0e, 32 + 0x0f, 32 + 0x13, 32 + 0x1f];
/// The slot's card-detect switch: low with a card in.
pub const CARD_DETECT: usize = 0xc9;
/// The XUSB pad controller (the PCIe and USB 3 PHYs).
pub const PADCTL: u64 = 0x7009_f000;
/// The "pcie_x" reset (PERST# and the controller's link side).
const PCIE_X_RESET: usize = 74;
/// The Ethernet controller's address: sbc-emu's locally administered one.
const MAC: [u8; 6] = [0x02, 0x53, 0x42, 0x4e, 0x4e, 0x01];
/// The HDMI connector's hot-plug pin: low with a monitor. Its DDC is on I²C4.
pub const HDMI_HPD: usize = 0xe1;
const DDC: usize = 3;
/// Its registers are four times as far apart as the other I²C controllers'.
const I2C_VI: u64 = 0x546c_0c00;
pub const PMC: u64 = 0x7000_e400;
pub const FUSE: u64 = 0x7000_f800;
/// Every device of the SoC (and PCIe) lies here; RAM starts where it ends.
const DEVICE_WINDOW: (u64, u64) = (0x0100_0000, RAM_BASE);

/// HIDREV: chip 0x21 (Tegra210), major revision 1 (A), minor 2: an A02, as on the Nano.
const HIDREV: u32 = 2 << 16 | 0x21 << 8 | 1 << 4;

/// The Cortex-A57 r1p1 (MIDR 0x411fd071); its GIC is memory-mapped (GICv2).
pub const CORTEX_A57: CpuProfile = CpuProfile { midr: 0x411f_d071, gic_sysregs: false };
pub const NCPU: usize = 4;

/// Fuse values the kernel reads at boot (offset in the fuse block: 0x100 + the fuse's).
fn fuse_preset(offset: u64) -> u32 {
    match offset {
        0x110 => 0x00,  // SKU_INFO: an engineering SKU
        0x130 => 2000, // CPU_SPEEDO_2 (holds the GPU speedo at speedo revision 0)
        0x114 => 2100, // CPU_SPEEDO_0
        0x134 => 1900, // SOC_SPEEDO_0
        0x118 => 10,   // CPU_IDDQ
        0x140 => 10,   // SOC_IDDQ
        0x228 => 10,   // GPU_IDDQ
        _ => 0,
    }
}

pub struct Soc {
    pub ram: Vec<u8>,
    pub uart: Uart,
    pub gic: Gic,
    pub car: car::Car,
    pub pmc: pmc::Pmc,
    pub timers: timer::Timers,
    pub gpio: gpio::Gpio,
    pub i2c: [i2c::TegraI2c; 6],
    pub sd: [sdhci::Sdhci; 4],
    pub pcie: pcie::Pcie,
    pub padctl: padctl::PadCtl,
    pub xusb: xusb::Xusb,
    /// the emulated USB devices: a keyboard and a tablet, and a stick on request
    pub usb: emu_core::usb::Devices,
    pub apbdma: apbdma::ApbDma,
    /// the camera's I²C controller, inside the display's address range
    pub i2c_vi: i2c::TegraI2c,
    /// the monitor's own mode
    pub monitor: Option<(u32, u32)>,
    pub smmu: smmu::Smmu,
    pub host1x: host1x::Host1x,
    pub gpu: gpu::Gpu,
    pub dc: [dc::Dc; 2],
    pub sor: [sor::Sor; 2],
    /// the network behind the Ethernet port, and when to poll it next (ns)
    pub net: Option<sbc_net::net::VirtualNet>,
    net_next: u64,
    fuse: std::collections::HashMap<u64, u32>,
    /// virtual time (ns) at the last device update
    pub now: u64,
    /// a device register was accessed: interrupt lines must be re-evaluated
    pub dirty: bool,
    stub_regs: std::collections::HashMap<u64, u32>,
    /// Unmodelled device pages touched so far (page address), in first-touch order.
    pub stub_pages: Vec<u64>,
    /// Print each newly touched unmodelled device page to stderr.
    pub log_stub_pages: bool,
}

impl Soc {
    fn ram_offset(&self, addr: u64, size: usize) -> Option<usize> {
        let off = addr.checked_sub(RAM_BASE)?;
        (off + size as u64 <= self.ram.len() as u64).then_some(off as usize)
    }

    fn stub_read(&mut self, word: u64) -> u32 {
        self.note_stub(word);
        self.stub_regs.get(&word).copied().unwrap_or(0)
    }

    fn note_stub(&mut self, word: u64) {
        let page = word & !0xfff;
        if !self.stub_pages.contains(&page) {
            self.stub_pages.push(page);
            if self.log_stub_pages {
                eprintln!("[jetson-nano] unmodelled device page {page:#010x}");
            }
        }
    }

    /// The 32-bit register at `word` (4-byte aligned), or None outside the device window.
    fn reg_read(&mut self, word: u64) -> Option<u32> {
        let at = |base: u64, size: u64| (base..base + size).contains(&word);
        Some(if at(GICD, 0x1000) {
            self.gic.dist_read(word - GICD)
        } else if at(GICC, 0x2000) {
            self.gic.cpu_read(word - GICC)
        } else if at(UARTA, 0x40) {
            self.uart.read(word - UARTA)
        } else if at(TMR, timer::SIZE) {
            self.timers.read(word - TMR, self.now)
        } else if at(CAR, car::SIZE) {
            self.car.read(word - CAR)
        } else if at(PMC, 0xc00) {
            self.pmc.read(word - PMC)
        } else if at(PADCTL, padctl::SIZE) {
            self.padctl.read(word - PADCTL)
        } else if at(xusb::BASE, xusb::SIZE) {
            self.xusb.read(word - xusb::BASE)
        } else if at(apbdma::BASE, apbdma::SIZE) {
            self.apbdma.read(word - apbdma::BASE)
        } else if let Some(o) = word.checked_sub(I2C_VI).filter(|&o| o < 0x400) {
            self.i2c_vi.read(o >> 2)
        } else if at(smmu::BASE, smmu::SIZE) {
            self.smmu.read(word - smmu::BASE)
        } else if at(host1x::BASE, host1x::SIZE) {
            self.host1x.read(word - host1x::BASE)
        } else if at(gpu::BAR0, gpu::SIZE) {
            self.gpu.tick(self.now);
            self.gpu.read(word - gpu::BAR0)
        } else if at(gpu::BAR1, gpu::SIZE) {
            self.gpu.tick(self.now);
            self.gpu.bar1_read(word - gpu::BAR1, &self.smmu, &mut self.ram)
        } else if let Some(k) = dc::BASE.iter().position(|&b| at(b, dc::SIZE)) {
            self.dc[k].read(word - dc::BASE[k])
        } else if let Some(k) = sor::BASE.iter().position(|&b| at(b, sor::SIZE)) {
            self.sor[k].read(word - sor::BASE[k])
        } else if at(GPIO, gpio::SIZE) {
            self.gpio.read(word - GPIO)
        } else if let Some(k) = I2C.iter().position(|&(b, _)| at(b, i2c::SIZE)) {
            self.i2c[k].read(word - I2C[k].0)
        } else if at(FUSE, 0x400) {
            let o = word - FUSE;
            self.fuse.get(&o).copied().unwrap_or_else(|| fuse_preset(o))
        } else if word == APBMISC + 4 {
            HIDREV
        } else if (DEVICE_WINDOW.0..DEVICE_WINDOW.1).contains(&word) {
            self.stub_read(word)
        } else {
            return None;
        })
    }

    fn reg_write(&mut self, word: u64, value: u32) -> bool {
        let at = |base: u64, size: u64| (base..base + size).contains(&word);
        if at(GICD, 0x1000) {
            self.gic.dist_write(word - GICD, value);
        } else if at(GICC, 0x2000) {
            self.gic.cpu_write(word - GICC, value);
        } else if at(UARTA, 0x40) {
            self.uart.write(word - UARTA, value);
        } else if at(TMR, timer::SIZE) {
            self.timers.write(word - TMR, value);
        } else if at(CAR, car::SIZE) {
            let usb_reset = self.car.in_reset(xusb::RESET);
            let dc_reset = dc::RESETS.map(|id| self.car.in_reset(id));
            self.car.write(word - CAR, value);
            if !usb_reset && self.car.in_reset(xusb::RESET) {
                self.xusb.reset(&mut self.usb);
            }
            self.pcie.set_reset(self.car.in_reset(PCIE_X_RESET));
            for ((d, id), was) in self.dc.iter_mut().zip(dc::RESETS).zip(dc_reset) {
                if !was && self.car.in_reset(id) {
                    d.reset();
                }
            }
            for (s, id) in self.sor.iter_mut().zip(sor::RESETS) {
                if self.car.in_reset(id) {
                    *s = Default::default();
                }
            }
            if self.car.in_reset(I2C_VI_RESET) {
                self.i2c_vi.reset();
            }
            for (c, &id) in self.i2c.iter_mut().zip(&I2C_RESETS) {
                if self.car.in_reset(id) {
                    c.reset();
                }
            }
        } else if at(PMC, 0xc00) {
            self.pmc.write(word - PMC, value);
        } else if at(PADCTL, padctl::SIZE) {
            self.padctl.write(word - PADCTL, value);
        } else if at(xusb::BASE, xusb::SIZE) {
            let mut dma = sdhci::Dma { ram: &mut self.ram, base: RAM_BASE };
            self.xusb.write(word - xusb::BASE, value, &mut dma, &mut self.usb);
        } else if at(apbdma::BASE, apbdma::SIZE) {
            self.apbdma.write(word - apbdma::BASE, value);
        } else if let Some(o) = word.checked_sub(I2C_VI).filter(|&o| o < 0x400) {
            self.i2c_vi.write(o >> 2, value, self.now);
        } else if at(smmu::BASE, smmu::SIZE) {
            self.smmu.write(word - smmu::BASE, value);
            self.gpu.mmu.invalidate();
        } else if at(host1x::BASE, host1x::SIZE) {
            let dma = sdhci::Dma { ram: &mut self.ram, base: RAM_BASE };
            self.host1x.write(word - host1x::BASE, value, &self.smmu, &dma);
        } else if at(gpu::BAR0, gpu::SIZE) {
            self.gpu.tick(self.now);
            self.gpu.write(word - gpu::BAR0, value, &self.smmu, &mut self.ram);
        } else if at(gpu::BAR1, gpu::SIZE) {
            self.gpu.tick(self.now);
            self.gpu.bar1_write(word - gpu::BAR1, value, &self.smmu, &mut self.ram);
        } else if let Some(k) = dc::BASE.iter().position(|&b| at(b, dc::SIZE)) {
            self.dc[k].write(word - dc::BASE[k], value, self.now);
        } else if let Some(k) = sor::BASE.iter().position(|&b| at(b, sor::SIZE)) {
            self.sor[k].write(word - sor::BASE[k], value);
        } else if at(GPIO, gpio::SIZE) {
            self.gpio.write(word - GPIO, value);
        } else if let Some(k) = I2C.iter().position(|&(b, _)| at(b, i2c::SIZE)) {
            self.i2c[k].write(word - I2C[k].0, value, self.now);
        } else if at(FUSE, 0x400) {
            self.fuse.insert(word - FUSE, value);
        } else if word == APBMISC + 4 {
        } else if (DEVICE_WINDOW.0..DEVICE_WINDOW.1).contains(&word) {
            self.note_stub(word);
            self.stub_regs.insert(word, value);
        } else {
            return false;
        }
        true
    }

    fn read(&mut self, addr: u64, size: usize) -> Result<u64, Fault> {
        if let Some(o) = self.ram_offset(addr, size) {
            return Ok(self.ram[o..o + size].iter().rev().fold(0, |v, b| v << 8 | *b as u64));
        }
        if size == 8 {
            return Ok(self.read(addr, 4)? | self.read(addr + 4, 4)? << 32);
        }
        self.dirty = true;
        if (SDMMC..SDMMC + 4 * sdhci::SIZE).contains(&addr) {
            let k = (addr - SDMMC) / sdhci::SIZE;
            return Ok(self.sd[k as usize].read_sized(addr - SDMMC - k * sdhci::SIZE, size) as u64);
        }
        if pcie::Pcie::contains(addr) {
            return Ok(self.pcie.read(addr, size) as u64);
        }
        let word = self.reg_read(addr & !3).ok_or(Fault::Unmapped)?;
        Ok((word >> ((addr & 3) * 8)) as u64 & ((1u64 << (size * 8)) - 1))
    }

    fn write(&mut self, addr: u64, size: usize, value: u64) -> Result<(), Fault> {
        if let Some(o) = self.ram_offset(addr, size) {
            self.ram[o..o + size].copy_from_slice(&value.to_le_bytes()[..size]);
            return Ok(());
        }
        if size == 8 {
            self.write(addr, 4, value as u32 as u64)?;
            return self.write(addr + 4, 4, value >> 32);
        }
        self.dirty = true;
        let word = addr & !3;
        if (SDMMC..SDMMC + 4 * sdhci::SIZE).contains(&word) {
            let k = (word - SDMMC) / sdhci::SIZE;
            // SDHCI registers are 8, 16 and 32 bits wide, and commands start from part of a word
            let mut dma = sdhci::Dma { ram: &mut self.ram, base: RAM_BASE };
            self.sd[k as usize].write(addr - SDMMC - k * sdhci::SIZE, value as u32, size, &mut dma);
            return Ok(());
        }
        if pcie::Pcie::contains(addr) {
            let mut dma = sdhci::Dma { ram: &mut self.ram, base: RAM_BASE };
            self.pcie.write(addr, value as u32, size, &mut dma);
            return Ok(());
        }
        let v = if size == 4 || (UARTA..UARTA + 0x40).contains(&word) && addr & 3 == 0 {
            // the 8250 driver writes its byte registers as bytes at the word's address
            value as u32
        } else {
            let shift = (addr & 3) * 8;
            let mask = ((1u32 << (size * 8)) - 1) << shift;
            // the GIC's set/clear-enable, pending and active registers are write-one: the
            // other bytes of the word are written as zero
            let write_one = (GICD + 0x100..GICD + 0x400).contains(&word);
            let old = if write_one { 0 } else { self.reg_read(word).unwrap_or(0) };
            old & !mask | (value as u32) << shift & mask
        };
        if self.reg_write(word, v) { Ok(()) } else { Err(Fault::Unmapped) }
    }

    /// Bring the devices up to `now` and drive the interrupt lines.
    fn service(&mut self, now: u64) {
        self.now = self.now.max(now);
        self.timers.service(self.now);
        self.gic.set_level(UARTA_IRQ, self.uart.irq());
        self.apbdma.run(&mut sdhci::Dma { ram: &mut self.ram, base: RAM_BASE }, &mut I2cFifos { i2c: &mut self.i2c, now: self.now });
        for k in 0..32 {
            self.gic.set_level(apbdma::irq_of(k), self.apbdma.irq(k));
        }
        for (c, &(_, irq)) in self.i2c.iter().zip(&I2C) {
            self.gic.set_level(irq, c.irq());
        }
        self.gic.set_level(I2C_VI_IRQ, self.i2c_vi.irq());
        for (c, &irq) in self.sd.iter().zip(&SDMMC_IRQS) {
            self.gic.set_level(irq, c.irq());
        }
        self.xusb.xhci.tick(self.now, &mut sdhci::Dma { ram: &mut self.ram, base: RAM_BASE }, &mut self.usb);
        self.gic.set_level(xusb::IRQ, self.xusb.irq());
        for (d, &irq) in self.dc.iter_mut().zip(&dc::IRQS) {
            if let Some((point, n)) = d.tick(self.now) {
                (0..n).for_each(|_| self.host1x.increment(point));
            }
            self.gic.set_level(irq, d.irq());
        }
        self.gic.set_level(host1x::IRQ, self.host1x.irq());
        self.gpu.tick(self.now);
        self.gpu.poll(&self.smmu, &mut self.ram);
        self.gic.set_level(gpu::IRQ, self.gpu.irq());
        self.pump_network();
        self.pcie.update();
        self.gic.set_level(pcie::IRQ, self.pcie.irq());
        self.gic.set_level(pcie::MSI_IRQ, self.pcie.msi_irq());
        for (b, &irq) in gpio::IRQS.iter().enumerate() {
            self.gic.set_level(irq, self.gpio.irq(b));
        }
        for i in 0..timer::TIMERS {
            if let Some(irq) = timer::irq_of(i) {
                self.gic.set_level(irq, self.timers.irq(i));
            }
        }
    }

    /// Exchange frames between the Ethernet controller and the network: what the guest sends goes
    /// out at once, the network is polled.
    fn pump_network(&mut self) {
        let nic = &mut self.pcie.nic;
        if nic.tx_out.is_empty() && self.now < self.net_next {
            return;
        }
        let now_us = self.now / 1000;
        let sent = std::mem::take(&mut nic.tx_out);
        if let Some(net) = self.net.as_mut() {
            for frame in sent {
                nic.deliver(net.handle(&frame, now_us));
            }
            if self.now >= self.net_next {
                nic.deliver(net.poll(now_us));
                // soon while frames wait or the NAT relays connections
                let busy = net.nat.is_some() || !nic.rx_queue.is_empty();
                self.net_next = self.now + if busy { 1_000_000 } else { 10_000_000 };
            }
        }
        self.pcie.service(&mut sdhci::Dma { ram: &mut self.ram, base: RAM_BASE });
    }

    fn next_event(&self) -> Option<u64> {
        let net = (self.net.is_some() && self.pcie.nic.receiving()).then_some(self.net_next);
        self.timers.next_event().into_iter().chain(net).chain(self.xusb.xhci.next_event(&self.usb)).chain(self.dc.iter().filter_map(|d| d.next_event())).chain(self.gpu.next_event()).min()
    }
}

/// The I²C controllers' FIFOs, as the APB DMA reaches them.
struct I2cFifos<'a> {
    i2c: &'a mut [i2c::TegraI2c; 6],
    now: u64,
}

impl I2cFifos<'_> {
    fn at(&mut self, apb: u64) -> Option<(&mut i2c::TegraI2c, u64)> {
        let k = I2C.iter().position(|&(b, _)| (b..b + i2c::SIZE).contains(&apb))?;
        Some((&mut self.i2c[k], apb - I2C[k].0))
    }
}

impl apbdma::Fifo for I2cFifos<'_> {
    fn ready(&mut self, apb: u64, to_peripheral: bool) -> bool {
        self.at(apb).is_some_and(|(c, _)| if to_peripheral { c.has_room() } else { c.has_data() })
    }
    fn take(&mut self, apb: u64) -> u32 {
        self.at(apb).map_or(0, |(c, o)| c.read(o))
    }
    fn put(&mut self, apb: u64, v: u32) {
        let now = self.now;
        if let Some((c, o)) = self.at(apb) {
            c.write(o, v, now);
        }
    }
}

macro_rules! bus_access {
    ($read:ident, $write:ident, $ty:ty, $size:expr) => {
        fn $read(&mut self, a: u64) -> Result<$ty, Fault> { self.read(a, $size).map(|v| v as $ty) }
        fn $write(&mut self, a: u64, v: $ty) -> Result<(), Fault> { self.write(a, $size, v as u64) }
    };
}

impl Bus for Soc {
    bus_access!(read8, write8, u8, 1);
    bus_access!(read16, write16, u16, 2);
    bus_access!(read32, write32, u32, 4);
    bus_access!(read64, write64, u64, 8);
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum CoreState {
    Off,
    Run,
    Wfi,
    Wfe,
}

/// Instructions (ns of virtual time) a core runs before the next core gets its turn.
fn quantum() -> u64 {
    static Q: std::sync::OnceLock<u64> = std::sync::OnceLock::new();
    *Q.get_or_init(|| std::env::var("SBC_QUANTUM").ok().and_then(|v| v.parse().ok()).unwrap_or(20_000))
}

/// MPIDR of core `k`: one cluster, Aff0 = the core.
pub fn core_mpidr(k: usize) -> u64 {
    0x8000_0000 | k as u64
}

pub struct Machine {
    pub cpus: Vec<Cpu>,
    pub state: Vec<CoreState>,
    pub soc: Soc,
    pub now: u64,
    cur: usize,
    next_check: u64,
    round_end: u64,
    pub irqs: u64,
    pub exceptions: u64,
    /// A debugger's breakpoints (interpreter only, as its watchpoints: see `set_debugging`).
    pub breakpoints: emu_core::debug::Breakpoints,
    debugging: bool,
    /// each core's `jit_off` before a debugger set something
    jit_off_before: Vec<bool>,
}

impl Machine {
    /// A Jetson Nano with `ram_bytes` of DRAM (up to 4 GiB) and `ncpu` cores (1-4), its clock at
    /// the start of 2026.
    pub fn with_cpus(ram_bytes: usize, ncpu: usize) -> Result<Self, String> {
        Self::with_clock(ram_bytes, ncpu, 1_767_225_600)
    }

    /// As `with_cpus`, with the real-time clock at `unix_seconds`.
    pub fn with_clock(ram_bytes: usize, ncpu: usize, unix_seconds: i64) -> Result<Self, String> {
        if !(64 << 20..=MAX_RAM).contains(&(ram_bytes as u64)) {
            return Err("the Jetson Nano takes 64..4096 MiB of RAM".into());
        }
        if !(1..=NCPU).contains(&ncpu) {
            return Err(format!("the Tegra X1 has 1..{NCPU} cores"));
        }
        let soc = Soc {
            ram: vec![0; ram_bytes],
            uart: Uart::default(),
            gic: Gic::default(),
            car: car::Car::default(),
            pmc: pmc::Pmc::default(),
            timers: timer::Timers::default(),
            gpio: gpio::Gpio::default(),
            i2c: {
                let mut buses: [i2c::TegraI2c; 6] = Default::default();
                buses[I2C5].attach(max77620::ADDRESS, Box::new(max77620::Max77620::default()));
                buses[I2C5].attach(max77620::RTC_ADDRESS, Box::new(max77620::Rtc::new(unix_seconds)));
                buses
            },
            sd: Default::default(),
            pcie: pcie::Pcie::new(MAC),
            padctl: Default::default(),
            xusb: Default::default(),
            usb: Default::default(),
            apbdma: Default::default(),
            i2c_vi: Default::default(),
            monitor: None,
            smmu: Default::default(),
            host1x: Default::default(),
            gpu: Default::default(),
            dc: Default::default(),
            sor: Default::default(),
            net: None,
            net_next: 0,
            fuse: Default::default(),
            now: 0,
            dirty: true,
            stub_regs: Default::default(),
            stub_pages: Vec::new(),
            log_stub_pages: false,
        };
        let mut soc = soc;
        soc.gpio.set_input(HDMI_HPD, true);
        let mut m = Self { cpus: Vec::new(), state: vec![CoreState::Off; ncpu], soc, now: 0, cur: 0, next_check: 0, round_end: u64::MAX, irqs: 0, exceptions: 0, breakpoints: Default::default(), debugging: false, jit_off_before: vec![false; ncpu] };
        for k in 0..ncpu {
            let c = m.fresh_cpu(k);
            m.cpus.push(c);
        }
        m.state[0] = CoreState::Run;
        Ok(m)
    }

    fn fresh_cpu(&mut self, k: usize) -> Cpu {
        let mut cpu = Cpu::new();
        cpu.profile = CORTEX_A57;
        cpu.sys.cntfrq = CNTFRQ;
        cpu.sys.mpidr = core_mpidr(k);
        cpu.ram = self.soc.ram.as_mut_ptr();
        cpu.ram_len = self.soc.ram.len() as u64;
        cpu.ram_base = RAM_BASE;
        cpu.firmware_calls_to_host = true;
        cpu.code_pages = Some(match self.cpus.first().and_then(|c| c.code_pages.clone()) {
            Some(m) => m,
            None => Cpu::code_page_map(cpu.ram_len),
        });
        cpu
    }

    pub fn cpu(&self) -> &Cpu { &self.cpus[0] }
    pub fn cpu_mut(&mut self) -> &mut Cpu { &mut self.cpus[0] }
    pub fn instret(&self) -> u64 { self.cpus.iter().map(|c| c.instret).sum() }
    pub fn now(&self) -> u64 { self.now.max(self.cpus[self.cur].now_ns()) }
    pub fn set_jit(&mut self, on: bool) {
        for c in self.cpus.iter_mut() {
            c.jit_off = !on;
        }
    }
    pub fn enable_jit(&mut self) -> bool { self.cpus[self.cur].enable_jit() }

    /// Plug the Ethernet port into the virtual 10.0.2.0/24 network (DHCP, DNS, ICMP); `nat` also
    /// relays guest TCP/UDP to the host's network (native builds only).
    pub fn connect_network(&mut self, nat: bool, log: bool) {
        #[allow(unused_mut)]
        let mut net = sbc_net::net::VirtualNet::new(log);
        #[cfg(not(target_arch = "wasm32"))]
        if nat {
            net.nat = Some(sbc_net::nat::Nat::new(log));
        }
        let _ = nat;
        self.soc.net = Some(net);
        self.soc.pcie.nic.set_link(true);
        self.soc.dirty = true;
    }

    /// Connect a monitor to HDMI.
    pub fn connect_monitor(&mut self, mode: &Mode) {
        self.soc.i2c[DDC].attach(0x50, Box::new(peripherals::dw_hdmi::EdidEeprom::new(mode)));
        let totals = (mode.width + mode.hfp + mode.hsync + mode.hbp, mode.height + mode.vfp + mode.vsync + mode.vbp, mode.clock_khz);
        self.soc.dc.iter_mut().for_each(|d| d.mode = Some(totals));
        self.soc.monitor = Some((mode.width, mode.height));
        self.soc.gpio.set_input(HDMI_HPD, false);
        self.soc.dirty = true;
    }

    /// The picture on HDMI as RGBA.
    pub fn render(&self, out: &mut Vec<u8>) -> Option<(u32, u32)> {
        let s = &self.soc;
        let dma = peripherals::dma::Ram { ram: &s.ram, base: RAM_BASE };
        [smmu::Group::Dc, smmu::Group::DcB].into_iter().zip(&s.dc).find_map(|(g, d)| d.render(g, &s.smmu, &dma, out))
    }

    /// The size of the picture on HDMI.
    pub fn screen_size(&self) -> Option<(u32, u32)> {
        self.soc.dc.iter().find(|d| d.running())?.size()
    }

    /// Plug a USB stick in.
    pub fn attach_usb_drive(&mut self, disk: Box<dyn emu_core::disk::Disk + Send>) {
        self.soc.usb.storage = Some(emu_core::usb::Storage::new(disk));
        self.soc.xusb.attach(true);
    }

    /// Put a card in the microSD slot.
    pub fn insert_sd(&mut self, disk: Box<dyn emu_core::disk::Disk>) {
        self.soc.sd[0].card.disk = Some(disk);
        self.soc.gpio.set_input(CARD_DETECT, false);
        self.soc.dirty = true;
    }

    pub fn serial_input(&mut self, b: u8) -> bool {
        self.soc.dirty = true;
        self.soc.uart.push_rx(b)
    }
    pub fn serial_output(&self) -> &[u8] { &self.soc.uart.out }
    pub fn take_serial_output(&mut self) -> Vec<u8> { std::mem::take(&mut self.soc.uart.out) }

    fn ns_of_count(c: u64) -> u64 {
        ((c as u128 * 1_000_000_000).div_ceil(CNTFRQ as u128)).min(u64::MAX as u128) as u64
    }

    fn catch_up(&mut self, k: usize, t: u64) {
        let c = &mut self.cpus[k];
        if c.now_ns() < t {
            c.sys.time_base_ns = t;
            c.sys.time_base_instret = c.instret;
        }
    }

    fn switch_to(&mut self, k: usize) {
        if k != self.cur {
            let (a, b) = (self.cur, k);
            let (lo, hi) = self.cpus.split_at_mut(a.max(b));
            let (pa, pb) = if a < b { (&mut lo[a], &mut hi[0]) } else { (&mut hi[0], &mut lo[b]) };
            pa.hand_jit_to(pb);
            pa.clear_exclusive();
            self.cur = k;
        }
        self.soc.gic.current = k;
        self.next_check = 0;
    }

    fn timer_deadlines(cpu: &Cpu) -> impl Iterator<Item = u64> + '_ {
        let s = &cpu.sys;
        [(s.ptimer, 0), (s.vtimer, s.cntvoff)]
            .into_iter()
            .filter(|(t, _)| t.ctl & 3 == 1)
            .map(|(t, off)| Self::ns_of_count(t.cval.wrapping_add(off)))
    }

    fn deadline_after(&self, t: u64, c: usize) -> Option<u64> {
        let mut d = self.soc.next_event().filter(|&x| x > t);
        for (k, cpu) in self.cpus.iter().enumerate() {
            if self.state[k] == CoreState::Off || (k != c && self.state[k] == CoreState::Run) {
                continue;
            }
            for ns in Self::timer_deadlines(cpu) {
                if ns > t {
                    d = Some(d.map_or(ns, |x| x.min(ns)));
                }
            }
        }
        d
    }

    fn next_deadline(&self) -> Option<u64> {
        self.cpus
            .iter()
            .enumerate()
            .filter(|(k, _)| self.state[*k] != CoreState::Off)
            .flat_map(|(_, c)| Self::timer_deadlines(c))
            .chain(self.soc.next_event())
            // an expired timer whose interrupt is off wakes nobody: it must not stop time
            .filter(|&t| t > self.now)
            .min()
    }

    fn update(&mut self) {
        let c = self.cur;
        let t = self.cpus[c].now_ns();
        if t > self.now {
            self.now = t;
        }
        let now = self.now;
        for k in 0..self.cpus.len() {
            if self.state[k] == CoreState::Off {
                continue;
            }
            if k != c && self.state[k] != CoreState::Run {
                self.catch_up(k, now);
            }
            let cpu = &mut self.cpus[k];
            cpu.refresh_counter();
            cpu.sys.timers_changed = false;
            let s = &cpu.sys;
            let (p, v) = (s.ptimer.firing(s.cnt), s.vtimer.firing(s.cnt.wrapping_sub(s.cntvoff)));
            // the physical timer drives both the secure (29) and non-secure (30) PPIs: the
            // core has no TrustZone
            self.soc.gic.set_ppi(k, 30, p);
            self.soc.gic.set_ppi(k, 29, p);
            self.soc.gic.set_ppi(k, 27, v);
        }
        self.soc.service(now);
        for k in 0..self.cpus.len() {
            if self.state[k] == CoreState::Off {
                continue;
            }
            let irq = self.soc.gic.irq_out_for(k);
            self.cpus[k].set_irq(irq, false);
            if irq && matches!(self.state[k], CoreState::Wfi | CoreState::Wfe) {
                self.state[k] = CoreState::Run;
                self.round_end = self.round_end.min(t + quantum());
            }
        }
        self.soc.dirty = false;
        let horizon = t + 1_000_000;
        let due = self.deadline_after(t, c).unwrap_or(horizon).min(horizon);
        self.next_check = self.cpus[c].instret + (due.saturating_sub(t));
    }

    /// Advance idle virtual time to `t` (ns).
    pub fn idle_until(&mut self, t: u64) {
        if t > self.now {
            self.now = t;
        }
        let now = self.now;
        for k in 0..self.cpus.len() {
            if self.state[k] != CoreState::Off {
                self.catch_up(k, now);
            }
        }
        self.update();
    }

    /// Run up to `budget` instructions over all cores.
    pub fn run(&mut self, budget: u64) -> Stop {
        let end = self.instret().saturating_add(budget);
        let mut total = self.instret();
        let mut steps = budget;
        let mut storm = 0u32;
        let stop = 'run: loop {
            if total >= end || steps == 0 {
                break Stop::Budget;
            }
            self.update();
            if self.state.contains(&CoreState::Run) {
                for s in self.state.iter_mut() {
                    if *s == CoreState::Wfe {
                        *s = CoreState::Run;
                    }
                }
            }
            let runnable = (0..self.cpus.len()).filter(|&k| self.state[k] == CoreState::Run && self.breakpoints.may_run(k)).count();
            if runnable == 0 {
                let until = self.next_deadline().unwrap_or(self.now + 10_000_000);
                break Stop::Idle(until.max(self.now));
            }
            let round_start = self.now;
            self.round_end = if runnable == 1 { u64::MAX } else { round_start + quantum() };
            let (last, n) = (self.breakpoints.last_turn(self.cur), self.cpus.len());
            let watching = !self.cpus[0].watch.is_empty();
            for i in 0..n {
                let k = self.breakpoints.turn(i, last, n, watching);
                if self.state[k] != CoreState::Run || !self.breakpoints.may_run(k) {
                    continue;
                }
                if let Some(stop) = self.slice(k, round_start, &mut total, end, &mut steps, &mut storm) {
                    break 'run stop;
                }
                if total >= end || steps == 0 {
                    break;
                }
            }
        };
        let t = self.cpus[self.cur].now_ns();
        if t > self.now {
            self.now = t;
        }
        stop
    }

    fn slice(&mut self, k: usize, round_start: u64, total: &mut u64, end: u64, steps: &mut u64, storm: &mut u32) -> Option<Stop> {
        self.switch_to(k);
        self.catch_up(k, round_start);
        // what a debugger has set only changes between runs
        let (breaks, watches) = (!self.breakpoints.at.is_empty(), !self.cpus[k].watch.is_empty());
        loop {
            let t = self.cpus[k].now_ns();
            if t >= self.round_end || *total >= end || *steps == 0 {
                return None;
            }
            if breaks && self.breakpoints.hit(k, self.cpus[k].pc) {
                return Some(Stop::Breakpoint(k));
            }
            *steps -= 1;
            // what a device read sees as the time (a block that has run a while sees its start)
            self.soc.now = self.soc.now.max(t);
            if self.soc.dirty || self.cpus[k].sys.timers_changed || self.cpus[k].instret >= self.next_check {
                self.update();
            }
            let cpu = &mut self.cpus[k];
            let i0 = cpu.instret;
            cpu.chain_until = i0.saturating_add(self.round_end - t).min(self.next_check).min(i0 + (end - *total));
            let r = cpu.step(&mut self.soc);
            *total += self.cpus[k].instret - i0;
            if let Some(op) = self.cpus[k].tlbi_bcast.take() {
                for (j, o) in self.cpus.iter_mut().enumerate() {
                    if j != k {
                        o.tlb_invalidate(op);
                    }
                }
            }
            if let Some(op) = self.cpus[k].ic_bcast.take() {
                for (j, o) in self.cpus.iter_mut().enumerate() {
                    if j != k {
                        o.jit_invalidate(op);
                    }
                }
            }
            if watches {
                if let Some((a, kind)) = self.cpus[k].watch_hit.take() {
                    if r.is_ok() {
                        return Some(Stop::Watchpoint(k, a, kind));
                    }
                }
            }
            match r {
                Ok(()) => *storm = 0,
                Err(Trap::Exception(..)) => {
                    self.exceptions += 1;
                    *storm += 1;
                    if *storm >= 1_000_000 {
                        let c = &self.cpus[k];
                        let e = c.el as usize;
                        return Some(Stop::ExceptionStorm { pc: c.pc, esr: c.esr[e], elr: c.elr[e], far: c.far[e], el: c.el });
                    }
                }
                Err(Trap::Interrupt(..)) => {
                    self.irqs += 1;
                    self.soc.dirty = true;
                }
                Err(Trap::Wait) => {
                    self.update();
                    if self.cpus[k].irq {
                        continue;
                    }
                    let others = (0..self.cpus.len()).any(|j| j != k && self.state[j] == CoreState::Run);
                    if self.cpus[k].last_wfe && others {
                        if self.round_end != u64::MAX {
                            let r = self.round_end;
                            self.catch_up(k, r);
                        }
                        return None;
                    }
                    self.state[k] = if self.cpus[k].last_wfe { CoreState::Wfe } else { CoreState::Wfi };
                    return None;
                }
                Err(Trap::HostCall(_)) => {
                    if let Some(s) = self.psci(k) {
                        return Some(s);
                    }
                    if self.state[k] != CoreState::Run {
                        return None;
                    }
                }
                Err(Trap::Unimplemented(pc, word)) => return Some(Stop::Unimplemented(pc, word)),
                Err(Trap::Brk(n)) => return Some(Stop::Brk(n)),
                Err(Trap::Hlt(n)) => return Some(Stop::Hlt(n)),
            }
        }
    }

    /// Cores run interpreted while a debugger has something set; `--no-jit` stays as it was.
    fn set_debugging(&mut self) {
        let on = !self.breakpoints.at.is_empty() || !self.cpus[0].watch.is_empty();
        if on != self.debugging {
            self.debugging = on;
            for (c, off) in self.cpus.iter_mut().zip(self.jit_off_before.iter_mut()) {
                if on {
                    *off = std::mem::replace(&mut c.jit_off, true);
                } else {
                    c.jit_off = *off;
                }
            }
        }
    }

    pub fn debug_breakpoint(&mut self, addr: u64, insert: bool) {
        self.breakpoints.set(addr, insert);
        self.set_debugging();
    }

    /// Watch [addr, addr + len) for accesses of `kind` (1 write, 2 read, 3 either) on every core.
    pub fn debug_watch(&mut self, addr: u64, len: u64, kind: u8, insert: bool) {
        for c in self.cpus.iter_mut() {
            c.watch.retain(|&w| w != (addr, len, kind));
            if insert {
                c.watch.push((addr, len, kind));
            }
        }
        self.set_debugging();
    }

    /// Execute one instruction on core `k` (interpreter; a sleeping core is woken); a stop it
    /// causes, if any. `run` does it, giving the other cores no turn meanwhile.
    #[cold]
    pub fn debug_step(&mut self, k: usize) -> Option<Stop> {
        if self.state[k] == CoreState::Off {
            return None;
        }
        self.state[k] = CoreState::Run;
        let jit_off = std::mem::replace(&mut self.cpus[k].jit_off, true);
        let turn = self.breakpoints.last_turn(self.cur);
        self.breakpoints.begin_step(k, self.cpus[k].pc);
        let stop = self.run(1);
        self.breakpoints.end_step(k);
        self.breakpoints.keep_turn(turn);
        self.cpus[k].jit_off = jit_off;
        (!matches!(stop, Stop::Budget | Stop::Idle(_))).then_some(stop)
    }

    /// The pieces of guest RAM behind [va, va + len) as core `k` sees them: (offset, length)
    /// each. None where it is not RAM: device registers are never read.
    fn debug_pages(&mut self, k: usize, va: u64, len: usize) -> Option<Vec<(usize, usize)>> {
        let mut out = Vec::new();
        let mut done = 0;
        while done < len {
            let a = va.wrapping_add(done as u64);
            let pa = self.cpus[k].debug_translate(&mut self.soc, a)?;
            let n = (len - done).min(0x1000 - (a & 0xfff) as usize);
            out.push((self.soc.ram_offset(pa, n)?, n));
            done += n;
        }
        Some(out)
    }

    /// Guest RAM at virtual address `va` as core `k` sees it.
    pub fn debug_read(&mut self, k: usize, va: u64, len: usize) -> Option<Vec<u8>> {
        let pages = self.debug_pages(k, va, len)?;
        Some(pages.into_iter().flat_map(|(o, n)| self.soc.ram[o..o + n].iter().copied()).collect())
    }

    /// Write guest RAM at virtual address `va` (all of it or nothing); compiled code is dropped.
    pub fn debug_write(&mut self, k: usize, va: u64, data: &[u8]) -> bool {
        let Some(pages) = self.debug_pages(k, va, data.len()) else { return false };
        let mut done = 0;
        for (o, n) in pages {
            self.soc.ram[o..o + n].copy_from_slice(&data[done..done + n]);
            done += n;
        }
        for c in self.cpus.iter_mut() {
            c.jit_invalidate(u64::MAX); // the 32-bit core's code too
        }
        true
    }

    fn target(&self, mpidr: u64) -> Option<usize> {
        let a = mpidr & 0xff_00ff_ffff;
        (0..self.cpus.len()).find(|&k| core_mpidr(k) & 0xff_00ff_ffff == a)
    }

    /// PSCI 1.0 from the host, as the Nano's secure monitor (TF-A) provides it.
    fn psci(&mut self, k: usize) -> Option<Stop> {
        const NOT_SUPPORTED: u64 = -1i64 as u64;
        const INVALID_PARAMETERS: u64 = -2i64 as u64;
        const ALREADY_ON: u64 = -4i64 as u64;
        let x = self.cpus[k].x;
        let supported = |f: u32| matches!(f, 0x8400_0000 | 0x8400_0002 | 0x8400_0003 | 0xc400_0003 | 0x8400_0004 | 0xc400_0004 | 0x8400_0006 | 0x8400_0008 | 0x8400_0009 | 0x8400_000a);
        let ret = match x[0] as u32 {
            0x8400_0000 => 0x10000, // PSCI 1.0
            0x8400_0002 => {
                self.state[k] = CoreState::Off;
                self.cpus[k].set_irq(false, false);
                return None;
            }
            0x8400_0003 | 0xc400_0003 => match self.target(x[1]) {
                None => INVALID_PARAMETERS,
                Some(t) if self.state[t] != CoreState::Off => ALREADY_ON,
                Some(t) => {
                    self.cpu_on(k, t, x[2], x[3]);
                    0
                }
            },
            0x8400_0004 | 0xc400_0004 => match self.target(x[1]) {
                None => INVALID_PARAMETERS,
                Some(t) => (self.state[t] == CoreState::Off) as u64,
            },
            0x8400_0006 => 2, // no trusted OS to migrate
            0x8400_0008 => return Some(Stop::PowerOff),
            0x8400_0009 => return Some(Stop::Reset),
            0x8400_000a => if supported(x[1] as u32) { 0 } else { NOT_SUPPORTED },
            _ => NOT_SUPPORTED,
        };
        self.cpus[k].x[0] = ret;
        None
    }

    fn cpu_on(&mut self, caller: usize, t: usize, entry: u64, context: u64) {
        let (el, now) = (self.cpus[caller].el, self.cpus[caller].now_ns());
        let c = &mut self.cpus[t];
        let mut f = Cpu::new();
        f.profile = CORTEX_A57;
        f.sys.cntfrq = CNTFRQ;
        f.sys.mpidr = core_mpidr(t);
        f.firmware_calls_to_host = true;
        f.ram = c.ram;
        f.ram_len = c.ram_len;
        f.ram_base = c.ram_base;
        f.code_pages = c.code_pages.clone();
        f.instret = c.instret;
        f.sys.time_base_ns = now;
        f.sys.time_base_instret = c.instret;
        f.el = el;
        f.spsel = true;
        f.daif = 0xf << 6;
        f.pc = entry;
        f.x[0] = context;
        f.sys.cpacr_el1 = 3 << 20;
        f.jit_off = c.jit_off;
        f.watch = c.watch.clone();
        let mut old = std::mem::replace(c, f);
        old.hand_jit_to(c);
        self.soc.gic.reset_cpu_interface(t);
        self.state[t] = CoreState::Run;
        self.round_end = self.round_end.min(now + quantum());
        self.soc.dirty = true;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ram_at_its_base_and_devices_below() {
        let mut m = Machine::with_cpus(64 << 20, 1).unwrap();
        // MOVZ X1,#0x8000,LSL#16; MOV X0,#0x2a; STR X0,[X1,#0x100]; LDR X2,[X1,#0x100]; LDR W3,[X4]
        m.soc.write(UARTA, 1, b'J' as u64).unwrap();
        assert_eq!(m.soc.uart.out, b"J");
        assert_eq!(m.soc.read(APBMISC + 4, 4).unwrap() >> 8 & 0xff, 0x21);
        m.soc.write(RAM_BASE + 0x100, 8, 0x1234_5678_9abc_def0).unwrap();
        assert_eq!(m.soc.read(RAM_BASE + 0x100, 8).unwrap(), 0x1234_5678_9abc_def0);
        assert_eq!(m.soc.ram[0x100], 0xf0);
        // an unmodelled register reads back what was written
        m.soc.write(0x6000_c000, 4, 0x55).unwrap();
        assert_eq!(m.soc.read(0x6000_c000, 4).unwrap(), 0x55);
        assert_eq!(m.soc.stub_pages, vec![0x6000_c000]);
        assert!(m.soc.read(0x8000_0000 + (64 << 20), 4).is_err());
    }

    #[test]
    fn the_core_runs_from_ram_at_0x80000000() {
        let mut m = Machine::with_cpus(64 << 20, 1).unwrap();
        // MOV X0,#0x2a; MOVZ X1,#0x8000,LSL#16; STR X0,[X1,#0x1000]; WFI
        let code: [u32; 4] = [0xd280_0540, 0xd2b0_0001, 0xf908_0020, 0xd503_207f];
        for (i, w) in code.iter().enumerate() {
            m.soc.ram[i * 4..i * 4 + 4].copy_from_slice(&w.to_le_bytes());
        }
        m.cpus[0].pc = RAM_BASE;
        m.cpus[0].el = 1;
        let stop = m.run(100);
        assert!(matches!(stop, Stop::Idle(_)), "{stop:?}");
        assert_eq!(u64::from_le_bytes(m.soc.ram[0x1000..0x1008].try_into().unwrap()), 0x2a);
    }

    fn load(m: &mut Machine, code: &[u32]) {
        for (i, w) in code.iter().enumerate() {
            m.soc.ram[i * 4..i * 4 + 4].copy_from_slice(&w.to_le_bytes());
        }
        m.cpus[0].pc = RAM_BASE;
        m.cpus[0].el = 1;
    }

    #[test]
    fn narrow_writes_keep_the_other_bytes() {
        let mut m = Machine::with_cpus(64 << 20, 1).unwrap();
        for (addr, size, value, want) in [(0, 1, 0x55, 0x1122_3355u64), (0, 2, 0xabcd, 0x1122_abcd), (1, 1, 0x55, 0x1122_5544), (2, 2, 0xabcd, 0xabcd_3344)] {
            m.soc.write(0x6000_c000, 4, 0x1122_3344).unwrap();
            m.soc.write(0x6000_c000 + addr, size, value).unwrap();
            assert_eq!(m.soc.read(0x6000_c000, 4).unwrap(), want, "{size} bytes at {addr}");
        }
    }

    /// A timer started after a long stretch without a device access counts from the write.
    #[test]
    fn a_timer_counts_from_when_it_was_started() {
        for jit in [false, true] {
            let mut m = Machine::with_cpus(64 << 20, 1).unwrap();
            if jit {
                m.enable_jit();
            }
            m.soc.timers.write(0x14, 0x045f); // 19.2 MHz
            load(&mut m, &[
                0xd2ac_0000, // mov x0, #0x60000000
                0xf28a_1200, // movk x0, #0x5090 (TMR10)
                0x5280_efe1, // mov w1, #0x77f (100 us)
                0x72b0_0001, // movk w1, #0x8000, lsl #16 (enable)
                0xd29b_bd02, // mov x2, #0xdde8
                0xf2a0_00c2, // movk x2, #6, lsl #16 (450,024 loops, about 0.9 ms)
                0xf100_0442, // subs x2, x2, #1
                0x54ff_ffe1, // b.ne
                0xb900_0001, // str w1, [x0]
                0xd503_207f, // wfi
            ]);
            let stop = m.run(10_000_000);
            assert!(matches!(stop, Stop::Idle(_)), "{stop:?}");
            assert!(!m.soc.timers.irq(10), "not yet (jit {jit})");
            let due = m.soc.timers.next_event().unwrap();
            let left = due - m.now();
            assert!((99_000..=100_000).contains(&left), "{left} ns left (jit {jit})");
            m.idle_until(due);
            assert!(m.soc.timers.irq(10));
        }
    }

    /// An expired arch timer whose interrupt is off does not keep time from moving on.
    #[test]
    fn an_expired_masked_timer_does_not_stop_time() {
        let mut m = Machine::with_cpus(64 << 20, 1).unwrap();
        load(&mut m, &[0xd503_207f, 0xd503_207f]); // wfi; wfi
        m.cpus[0].sys.vtimer.ctl = 1;
        m.cpus[0].sys.vtimer.cval = 0;
        m.soc.timers.write(0x14, 0x045f);
        m.soc.timers.write(0x90, 1 << 31 | 95_999); // 5 ms
        for _ in 0..4 {
            if let Stop::Idle(t) = m.run(1000) {
                m.idle_until(t);
            }
        }
        assert!(m.soc.timers.irq(10), "time stopped at {} ns", m.now());
    }

    /// A debugger's write into a 32-bit program's compiled code takes effect.
    #[test]
    fn debug_write_reaches_compiled_32_bit_code() {
        let mut m = Machine::with_cpus(64 << 20, 1).unwrap();
        m.enable_jit();
        load(&mut m, &[
            0xd280_0200, // mov x0, #0x10 (AArch32 user mode)
            0xd518_4000, // msr spsr_el1, x0
            0xd2b0_0201, // mov x1, #0x80100000
            0xd518_4021, // msr elr_el1, x1
            0xd2b0_0402, // mov x2, #0x80200000
            0xd69f_03e0, // eret
        ]);
        // 1: mov r0, #1; str r0, [r2]; b 1b
        for (i, w) in [0xe3a0_0001u32, 0xe582_0000, 0xeaff_fffc].iter().enumerate() {
            m.soc.ram[0x10_0000 + i * 4..0x10_0004 + i * 4].copy_from_slice(&w.to_le_bytes());
        }
        m.run(1_000_000);
        assert_eq!(m.soc.ram[0x20_0000], 1);
        assert!(m.debug_write(0, RAM_BASE + 0x10_0000, &0xe3a0_0002u32.to_le_bytes()));
        m.run(1_000_000);
        assert_eq!(m.soc.ram[0x20_0000], 2);
    }

    #[test]
    fn narrow_reads_of_the_sd_buffer_take_what_they_read() {
        let mut m = Machine::with_cpus(64 << 20, 1).unwrap();
        let mut disk = emu_core::disk::MemDisk::new(2048);
        emu_core::disk::Disk::write(&mut disk, 3, &(0..512u32).map(|i| i as u8).collect::<Vec<_>>());
        m.insert_sd(Box::new(disk));
        let mut cmd = |index: u64, arg: u64, flags: u64| {
            m.soc.write(SDMMC + 8, 4, arg).unwrap();
            m.soc.write(SDMMC + 0xe, 2, index << 8 | flags).unwrap();
        };
        for (i, a, f) in [(0, 0, 0), (8, 0x1aa, 2), (55, 0, 2), (41, 0x40ff_8000, 2), (2, 0, 1), (3, 0, 2), (7, 0x5bc0 << 16, 3)] {
            cmd(i, a, f);
        }
        m.soc.write(SDMMC + 4, 4, 1 << 16 | 512).unwrap();
        m.soc.write(SDMMC + 8, 4, 3).unwrap();
        m.soc.write(SDMMC + 0xc, 4, 17 << 24 | 0x3a << 16 | 1 << 4).unwrap();
        let mut got = Vec::new();
        for size in [1usize, 1, 2, 4, 2, 1, 1] {
            got.extend_from_slice(&m.soc.read(SDMMC + 0x20, size).unwrap().to_le_bytes()[..size]);
        }
        assert_eq!(got, (0..12).collect::<Vec<u8>>());
        assert_eq!(m.soc.read(SDMMC + 0xfe, 2).unwrap(), 2, "a register's upper half");
    }
}
