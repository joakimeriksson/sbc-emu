use crate::display::{Display, HDMI_IRQ, TCON0_IRQ};
use crate::emac::{Emac, EMAC_BASE, EMAC_IRQ, EMAC_SIZE};
use peripherals::NCPU;
use crate::usb::{Dma, Ehci, Ohci};
use emu_core::usb::{Keyboard, Tablet};
use crate::{Gic, SunxiMmc, Timer, Uart};
use arm_a32::{CodeWatch, Cpu};
use std::sync::Arc;
use emu_core::{Bus, Core, Fault, FlatRam, Stop, Trap};

pub const RAM_BASE: u64 = 0x4000_0000;
pub const RAM_SIZE: usize = 1024 << 20;
pub const UART0_BASE: u64 = 0x01c2_8000;
pub const GICD_BASE: u64 = 0x01c8_1000;
pub const GICC_BASE: u64 = 0x01c8_2000;
/// Non-secure physical timer (PPI 14). The core has no TrustZone model and runs
/// secure, so CNTP_* also drives the secure physical timer line (PPI 13).
pub const PHYSICAL_TIMER_PPI: usize = 30;
pub const SECURE_PHYSICAL_TIMER_PPI: usize = 29;
/// Virtual timer (PPI 11): Linux's choice when booted without HYP mode.
pub const VIRTUAL_TIMER_PPI: usize = 27;
/// H3 device window. Addresses here that no model claims behave as plain read-back
/// registers, so unmodelled drivers probe without external aborts. Every first touch of
/// a 4 KiB page is recorded in `Soc::stub_pages`.
const DEVICE_WINDOW: (u64, u64) = (0x0100_0000, 0x0200_0000);
const UART_SIZE: u64 = 0x400;
/// UART0 interrupt: SPI 0.
pub const UART0_IRQ: usize = 32;
pub const CCU_BASE: u64 = 0x01c2_0000;
const CCU_SIZE: u64 = 0x400;
pub const TIMER_BASE: u64 = 0x01c2_0c00;
const TIMER_SIZE: u64 = 0x40;
/// SD card controller 0 (`mmc@1c0f000`), interrupt SPI 60.
pub const MMC0_BASE: u64 = 0x01c0_f000;
const MMC_SIZE: u64 = 0x1000;
pub const MMC0_IRQ: usize = 32 + 60;
/// USB host ports 0 (shared with OTG) and 1: EHCI at +0, OHCI at +0x400; interrupts
/// EHCI0 SPI 72, OHCI0 SPI 73, EHCI1 SPI 74, OHCI1 SPI 75.
pub const USB_HOST_BASES: [u64; 2] = [0x01c1_a000, 0x01c1_b000];
const OHCI_OFFSET: u64 = 0x400;
pub const EHCI_IRQS: [usize; 2] = [32 + 72, 32 + 74];
pub const OHCI_IRQS: [usize; 2] = [32 + 73, 32 + 75];
/// Timer 0/1 interrupts: SPI 18/19.
pub const TIMER_IRQS: [usize; 2] = [32 + 18, 32 + 19];
/// Mali-400 MP2 GPU (`gpu@1c40000`); interrupts gp, gpmmu, pp0, ppmmu0, pmu, pp1, ppmmu1
/// are SPI 97-103.
pub const MALI_BASE: u64 = 0x01c4_0000;
pub const MALI_IRQS: [(mali400::Irq, usize); 7] = [
    (mali400::Irq::Gp, 32 + 97),
    (mali400::Irq::GpMmu, 32 + 98),
    (mali400::Irq::Pp0, 32 + 99),
    (mali400::Irq::PpMmu0, 32 + 100),
    (mali400::Irq::Pmu, 32 + 101),
    (mali400::Irq::Pp1, 32 + 102),
    (mali400::Irq::PpMmu1, 32 + 103),
];
const GICD_SIZE: u64 = 0x1000;
const GICC_SIZE: u64 = 0x2000;

/// Physical devices visible to the initial H3 core.
pub struct Soc {
    pub ram: FlatRam,
    pub uart: Uart,
    pub gic: Gic,
    pub timer: Timer,
    pub mmc: SunxiMmc,
    /// DE2 mixer, TCON0 and HDMI.
    pub display: Display,
    pub ehci: [Ehci; 2],
    pub ohci: [Ohci; 2],
    /// USB keyboard on host port 1 (the board's USB-A socket).
    pub kbd: Keyboard,
    /// USB tablet (absolute pointer) on port 0 (the micro-USB OTG socket in host mode).
    pub tablet: Tablet,
    /// Ethernet MAC with its internal PHY.
    pub emac: Emac,
    /// The Mali-400 GPU.
    pub gpu: mali400::Mali400,
    /// The network the Ethernet port is plugged into (None: no cable).
    pub net: Option<sbc_net::net::VirtualNet>,
    /// CCU registers (PLLs, bus gates, resets, module clocks), byte offset / 4.
    ccu: Vec<u32>,
    /// Current 24 MHz time, set by the machine before each step.
    pub now: u64,
    /// A device register was accessed: interrupt lines must be re-evaluated.
    pub dirty: bool,
    stub_regs: std::collections::HashMap<u64, u32>,
    /// Unmodelled device pages touched so far (page address), in first-touch order.
    pub stub_pages: Vec<u64>,
    /// Print each newly touched unmodelled device page to stderr.
    pub log_stub_pages: bool,
    /// RAM pages holding compiled code (shared by the cores' JITs).
    pub code_watch: Arc<CodeWatch>,
}

impl Soc {
    /// What the HDMI output shows, as RGBA (None while the display is off).
    pub fn render(&self, out: &mut Vec<u8>) -> Option<(u32, u32)> {
        self.display.render(&self.ram.mem, self.ram.base, out)
    }

    /// Run the USB frames due by tick `now`.
    fn usb_update(&mut self, now: u64) {
        let mut dma = Dma { mem: &mut self.ram.mem, base: self.ram.base };
        self.ohci[0].update(now, &mut dma, Some(&mut self.tablet));
        self.ohci[1].update(now, &mut dma, Some(&mut self.kbd));
    }

    /// Exchange frames between the Ethernet MAC and the network; true while the port
    /// has to be polled: frames wait for descriptors, or the NAT has connections to serve.
    fn pump_network(&mut self, now: u64) -> bool {
        let now_us = crate::machine::ticks_to_ns(now) / 1000;
        match self.net.as_mut() {
            Some(net) => {
                for frame in std::mem::take(&mut self.emac.tx_out) {
                    self.emac.deliver(net.handle(&frame, now_us));
                }
                self.emac.deliver(net.poll(now_us));
            }
            None => self.emac.tx_out.clear(),
        }
        self.emac.receive(&mut Dma { mem: &mut self.ram.mem, base: self.ram.base });
        self.emac.rx_pending() || self.net.as_ref().is_some_and(|net| net.nat.is_some())
    }

    /// The USB controller and register offset at `addr`: (port, is OHCI, offset).
    fn usb_register(addr: u64) -> Option<(usize, bool, u64)> {
        USB_HOST_BASES.iter().enumerate().find_map(|(port, &base)| {
            if Self::in_region(addr, 4, base, Ehci::SIZE) {
                Some((port, false, addr - base))
            } else if Self::in_region(addr, 4, base + OHCI_OFFSET, Ohci::SIZE) {
                Some((port, true, addr - base - OHCI_OFFSET))
            } else {
                None
            }
        })
    }

    pub fn new(ram_size: usize) -> Self {
        Self {
            ram: FlatRam::new(RAM_BASE, ram_size),
            uart: Uart::default(),
            gic: Gic::default(),
            timer: Timer::default(),
            mmc: SunxiMmc::default(),
            display: Display::default(),
            ehci: Default::default(),
            ohci: [Ohci::new(true), Ohci::new(true)],
            kbd: Keyboard::default(),
            tablet: Tablet::default(),
            emac: Emac::default(),
            gpu: mali400::Mali400::new(std::env::var("SBC_MALI_LOG").is_ok_and(|v| v != "0")),
            net: None,
            ccu: Self::ccu_reset(),
            now: 0,
            dirty: true,
            stub_regs: Default::default(),
            stub_pages: Vec::new(),
            log_stub_pages: false,
            code_watch: CodeWatch::new(RAM_BASE as u32, ram_size as u32),
        }
    }

    /// CCU state as the boot firmware leaves it: PLL_CPUX at 1008 MHz and PLL_PERIPH0
    /// at 600 MHz (both enabled), AHB1/APB1/APB2 from PLL_PERIPH0/OSC24M.
    fn ccu_reset() -> Vec<u32> {
        let mut ccu = vec![0; (CCU_SIZE / 4) as usize];
        ccu[0x000 / 4] = 0x8000_1410; // PLL_CPUX: N=21, K=2 -> 1008 MHz
        ccu[0x028 / 4] = 0x9004_1811; // PLL_PERIPH0: 600 MHz
        ccu[0x050 / 4] = 0x0002_0000; // CPUX/AXI: CPUX from PLL_CPUX
        ccu[0x054 / 4] = 0x0000_3180; // AHB1/APB1: AHB1 from PLL_PERIPH0 / 3
        ccu[0x058 / 4] = 0x0100_0000; // APB2 from OSC24M (UART clock 24 MHz)
        ccu
    }

    fn ccu_read(&self, offset: u64) -> u32 {
        let value = self.ccu[(offset / 4) as usize];
        // PLL registers (0x00-0x4c): report LOCK (bit 28) whenever the PLL is enabled.
        if offset < 0x50 && value & (1 << 31) != 0 {
            value | 1 << 28
        } else {
            value
        }
    }

    fn stub_access(&mut self, addr: u64, write: Option<(u64, u32)>) -> Result<u32, Fault> {
        if !(DEVICE_WINDOW.0..DEVICE_WINDOW.1).contains(&addr) {
            return Err(Fault::Unmapped);
        }
        let page = addr & !0xfff;
        if !self.stub_pages.contains(&page) {
            self.stub_pages.push(page);
            if self.log_stub_pages {
                eprintln!("[h3] unmodelled device page {page:#010x} (first access {addr:#010x})");
            }
        }
        let word = addr & !3;
        let shift = (addr & 3) * 8;
        let old = self.stub_regs.get(&word).copied().unwrap_or(0);
        match write {
            Some((width, value)) => {
                let mask = if width >= 4 { u32::MAX } else { ((1u32 << (width * 8)) - 1) << shift };
                self.stub_regs.insert(word, old & !mask | (value << shift) & mask);
                Ok(0)
            }
            None => Ok(old >> shift),
        }
    }

    fn has_ram(&self, addr: u64, width: u64) -> bool {
        Self::in_region(addr, width, self.ram.base, self.ram.mem.len() as u64)
    }

    fn in_region(addr: u64, width: u64, base: u64, size: u64) -> bool {
        addr.checked_sub(base)
            .is_some_and(|offset| offset.checked_add(width).is_some_and(|end| end <= size))
    }

    /// UART register offset; misaligned accesses inside the UART are prohibited.
    fn uart_offset(addr: u64, width: u64) -> Result<u64, Fault> {
        let offset = addr.checked_sub(UART0_BASE).ok_or(Fault::Unmapped)?;
        let end = offset.checked_add(width).ok_or(Fault::Unmapped)?;
        if end > UART_SIZE {
            Err(Fault::Unmapped)
        } else if offset & 3 != 0 || width > 4 {
            Err(Fault::Prohibited)
        } else {
            Ok(offset)
        }
    }

    fn mmio_read(&mut self, addr: u64, width: u64) -> Result<u32, Fault> {
        self.dirty = true;
        if let Some(value) = self.display.read(addr, width) {
            return Ok(value);
        }
        if width == 4 && addr & 3 == 0 {
            if Self::in_region(addr, 4, CCU_BASE, CCU_SIZE) {
                return Ok(self.ccu_read(addr - CCU_BASE));
            }
            if Self::in_region(addr, 4, TIMER_BASE, TIMER_SIZE) {
                return Ok(self.timer.read(addr - TIMER_BASE, self.now));
            }
            if Self::in_region(addr, 4, MMC0_BASE, MMC_SIZE) {
                return Ok(self.mmc.read(addr - MMC0_BASE));
            }
            if Self::in_region(addr, 4, EMAC_BASE, EMAC_SIZE) {
                return Ok(self.emac.read(addr - EMAC_BASE));
            }
            if Self::in_region(addr, 4, MALI_BASE, mali400::SIZE) {
                return Ok(self.gpu.read(addr - MALI_BASE));
            }
            if let Some((port, ohci, offset)) = Self::usb_register(addr) {
                return Ok(if ohci { self.ohci[port].read(offset) } else { self.ehci[port].read(offset) });
            }
        }
        match Self::uart_offset(addr, width) {
            Ok(offset) => return Ok(self.uart.read(offset)),
            Err(Fault::Prohibited) => return Err(Fault::Prohibited),
            Err(Fault::Unmapped) => {}
        }
        let distributor = Self::in_region(addr, width, GICD_BASE, GICD_SIZE);
        let interface = Self::in_region(addr, width, GICC_BASE, GICC_SIZE);
        if distributor || interface {
            // GICv2 allows byte accesses to the distributor (priority and target bytes
            // in particular); the CPU interface is word-only.
            if width > 4 || addr & (width - 1) != 0 || interface && width != 4 {
                return Err(Fault::Prohibited);
            }
            let shift = (addr & 3) * 8;
            return Ok(if distributor {
                self.gic.dist_read((addr - GICD_BASE) & !3) >> shift
            } else {
                self.gic.cpu_read(addr - GICC_BASE)
            });
        }
        self.stub_access(addr, None)
    }

    fn mmio_write(&mut self, addr: u64, width: u64, value: u32) -> Result<(), Fault> {
        self.dirty = true;
        if self.display.write(addr, width, value) {
            return Ok(());
        }
        if width == 4 && addr & 3 == 0 {
            if Self::in_region(addr, 4, CCU_BASE, CCU_SIZE) {
                self.ccu[((addr - CCU_BASE) / 4) as usize] = value & !(1 << 28);
                return Ok(());
            }
            if Self::in_region(addr, 4, TIMER_BASE, TIMER_SIZE) {
                self.timer.write(addr - TIMER_BASE, value, self.now);
                return Ok(());
            }
            if Self::in_region(addr, 4, MMC0_BASE, MMC_SIZE) {
                self.mmc.write(addr - MMC0_BASE, value, &mut self.ram);
                for (address, len) in self.mmc.dma_written.drain(..) {
                    self.code_watch.dma_write(address, len);
                }
                return Ok(());
            }
            if Self::in_region(addr, 4, EMAC_BASE, EMAC_SIZE) {
                let mut dma = Dma { mem: &mut self.ram.mem, base: self.ram.base };
                self.emac.write(addr - EMAC_BASE, value, &mut dma);
                return Ok(());
            }
            if Self::in_region(addr, 4, MALI_BASE, mali400::SIZE) {
                self.gpu.now = self.now;
                let mut mem = GpuRam { ram: &mut self.ram, written: Vec::new() };
                self.gpu.write(addr - MALI_BASE, value, &mut mem);
                for (address, len) in mem.written {
                    self.code_watch.dma_write(address, len);
                }
                return Ok(());
            }
            if let Some((port, ohci, offset)) = Self::usb_register(addr) {
                if ohci {
                    self.ohci[port].write(offset, value, self.now);
                } else {
                    self.ehci[port].write(offset, value);
                }
                return Ok(());
            }
        }
        match Self::uart_offset(addr, width) {
            Ok(offset) => {
                self.uart.write(offset, value);
                return Ok(());
            }
            Err(Fault::Prohibited) => return Err(Fault::Prohibited),
            Err(Fault::Unmapped) => {}
        }
        let distributor = Self::in_region(addr, width, GICD_BASE, GICD_SIZE);
        let interface = Self::in_region(addr, width, GICC_BASE, GICC_SIZE);
        if distributor || interface {
            if width > 4 || addr & (width - 1) != 0 || interface && width != 4 {
                return Err(Fault::Prohibited);
            }
            if distributor {
                let offset = addr - GICD_BASE;
                let word = offset & !3;
                let value = if width == 4 {
                    value
                } else {
                    let shift = (offset & 3) * 8;
                    let mask = ((1u32 << (width * 8)) - 1) << shift;
                    // Set/clear-enable, pending and active registers are write-one:
                    // the other bytes of the word must be written as zero.
                    let old = if (0x100..0x400).contains(&word) { 0 } else { self.gic.dist_read(word) };
                    old & !mask | (value << shift) & mask
                };
                self.gic.dist_write(word, value);
            } else {
                self.gic.cpu_write(addr - GICC_BASE, value);
            }
            return Ok(());
        }
        self.stub_access(addr, Some((width, value))).map(|_| ())
    }
}

/// RAM as the GPU sees it (physical addresses); records the pages it writes so compiled
/// code there is dropped.
struct GpuRam<'a> {
    ram: &'a mut FlatRam,
    written: Vec<(u64, usize)>,
}

impl mali400::Memory for GpuRam<'_> {
    fn read_u32(&self, pa: u32) -> Option<u32> {
        let offset = (pa as u64).checked_sub(self.ram.base)? as usize;
        let bytes = self.ram.mem.get(offset..offset + 4)?;
        Some(u32::from_le_bytes(bytes.try_into().unwrap()))
    }

    fn write_u32(&mut self, pa: u32, value: u32) -> bool {
        let Some(offset) = (pa as u64).checked_sub(self.ram.base).map(|o| o as usize) else { return false };
        let Some(bytes) = self.ram.mem.get_mut(offset..offset + 4) else { return false };
        bytes.copy_from_slice(&value.to_le_bytes());
        match self.written.last_mut() {
            Some((start, len)) if *start + *len as u64 == pa as u64 => *len += 4,
            _ => self.written.push((pa as u64, 4)),
        }
        true
    }
}

/// PSCI function identifiers (SMC32 calling convention) and return codes.
mod psci {
    pub const VERSION: u32 = 0x8400_0000;
    pub const CPU_OFF: u32 = 0x8400_0002;
    pub const CPU_ON: u32 = 0x8400_0003;
    pub const AFFINITY_INFO: u32 = 0x8400_0004;
    pub const MIGRATE_INFO_TYPE: u32 = 0x8400_0006;
    pub const SYSTEM_OFF: u32 = 0x8400_0008;
    pub const SYSTEM_RESET: u32 = 0x8400_0009;
    pub const FEATURES: u32 = 0x8400_000a;
    pub const SUCCESS: u32 = 0;
    pub const NOT_SUPPORTED: u32 = -1i32 as u32;
    pub const INVALID_PARAMETERS: u32 = -2i32 as u32;
    pub const ALREADY_ON: u32 = -4i32 as u32;
}

/// Instructions a core runs before the next core gets its turn.
const QUANTUM: u64 = 1000;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CoreState {
    /// Powered down (before PSCI CPU_ON, or after CPU_OFF).
    Off,
    Run,
    /// In WFI/WFE until an interrupt.
    Wait,
}

/// The H3: four Cortex-A7 cores, GIC-400, UART0, CCU, timers.
///
/// Time is one 24 MHz architectural count shared by the cores. Each executed instruction
/// is 1 ns of one core's time; with several cores online they run in round-robin quanta
/// and time advances as if they ran in parallel. `step` (single core 0) fast-forwards idle
/// time; `run` returns `Stop::Idle` so a host loop can pace idle time against the wall
/// clock. Boot firmware is emulated by the host: PSCI over SMC, as U-Boot's secure
/// monitor provides it on real boards (CPU_ON brings up cores 1-3).
pub struct Machine {
    pub cpus: Vec<Cpu>,
    pub state: Vec<CoreState>,
    pub soc: Soc,
    /// The core whose turn it is.
    cur: usize,
    /// Tick at which interrupt state must next be re-evaluated.
    next_event: u64,
    timer_state: Vec<(u32, u64, u32, u64)>,
    pub irqs: u64,
    pub exceptions: u64,
    /// Time since the counter last advanced, in 1/12 ns. An instruction is 12 units
    /// divided by the number of running cores (they run in parallel on real hardware).
    subtick: u64,
    /// The core that caused the last non-budget, non-idle stop (diagnostics).
    pub last_stop_core: usize,
    /// a debugger's breakpoints (`debug_breakpoint`), and whether they turned the JIT off
    pub breakpoints: emu_core::debug::Breakpoints,
    debug_jit: bool,
    /// Compile hot code (the A32 JIT) on hosts that can run it.
    jit: bool,
    /// Debugging: print this many exceptions (vector, LR, SPSR, fault registers) to
    /// stderr once `trace_from` instructions have run (env `H3_TRACE_EXC`, `H3_TRACE_FROM`).
    pub trace_exceptions: u32,
    pub trace_from: u64,
}

impl Machine {
    pub fn new() -> Self {
        Self::with_ram_size(RAM_SIZE)
    }

    pub fn with_ram_size(ram_size: usize) -> Self {
        let cpus = (0..NCPU)
            .map(|index| {
                let mut cpu = Cpu::new();
                cpu.mpidr = 0x8000_0000 | index as u32;
                cpu.r[15] = RAM_BASE as u32;
                cpu
            })
            .collect();
        let mut state = vec![CoreState::Off; NCPU];
        state[0] = CoreState::Run;
        let mut machine = Self {
            cpus,
            state,
            soc: Soc::new(ram_size),
            cur: 0,
            next_event: 0,
            timer_state: vec![(0, 0, 0, 0); NCPU],
            irqs: 0,
            exceptions: 0,
            subtick: 0,
            last_stop_core: 0,
            breakpoints: Default::default(),
            debug_jit: false,
            jit: arm_a32::jit::available() && std::env::var("SBC_JIT").map_or(true, |v| v != "0"),
            trace_exceptions: std::env::var("H3_TRACE_EXC").ok().and_then(|v| v.parse().ok()).unwrap_or(0),
            trace_from: std::env::var("H3_TRACE_FROM").ok().and_then(|v| v.parse().ok()).unwrap_or(0),
        };
        for k in 0..NCPU {
            machine.attach(k);
        }
        machine
    }

    /// Give core `k` direct access to RAM and, if enabled, its JIT.
    fn attach(&mut self, k: usize) {
        let ram = &mut self.soc.ram.mem;
        // SAFETY: the RAM vector is allocated once with the machine and never resized; the
        // cores are dropped with it
        unsafe { self.cpus[k].set_ram(ram.as_mut_ptr(), RAM_BASE as u32, ram.len() as u32) };
        if self.jit {
            self.cpus[k].enable_jit(self.soc.code_watch.clone());
        }
    }

    /// Turn the JIT on or off (it is on by default where the host supports it).
    pub fn set_jit(&mut self, on: bool) {
        self.jit = on && arm_a32::jit::available();
        for k in 0..NCPU {
            if self.jit {
                self.cpus[k].enable_jit(self.soc.code_watch.clone());
            } else {
                self.cpus[k].disable_jit();
            }
        }
    }

    pub fn jit_enabled(&self) -> bool {
        self.jit
    }

    /// The boot core (core 0).
    pub fn cpu(&self) -> &Cpu {
        &self.cpus[0]
    }

    pub fn cpu_mut(&mut self) -> &mut Cpu {
        &mut self.cpus[0]
    }

    /// Instructions retired by all cores.
    pub fn instret(&self) -> u64 {
        self.cpus.iter().map(|c| c.instret).sum()
    }

    pub fn load(&mut self, addr: u64, bytes: &[u8]) -> Result<(), Fault> {
        self.soc.code_watch.dma_write(addr, bytes.len());
        self.soc.ram.load(addr, bytes)
    }

    /// Reset state of a core entering the kernel or a PSCI entry point: SVC/A32, MMU and
    /// caches off, IRQ/FIQ masked; SMC goes to host PSCI.
    fn reset_core(&mut self, index: usize, entry: u32) {
        let now = self.cpus[0].cntpct.max(self.cpus[index].cntpct);
        let instret = self.cpus[index].instret;
        let mut cpu = Cpu::new();
        cpu.instret = instret;
        cpu.mpidr = 0x8000_0000 | index as u32;
        // Bit 0 of an entry point selects Thumb state (Thumb-2 kernels pass a Thumb
        // secondary_startup).
        cpu.set_entry(entry);
        cpu.sctlr &= !(1 | 1 << 2 | 1 << 12);
        cpu.undef_to_guest = true;
        cpu.firmware_calls_to_host = true;
        cpu.cntpct = now;
        cpu.watch = self.cpus[index].watch.clone();
        self.cpus[index] = cpu;
        self.attach(index);
        self.state[index] = CoreState::Run;
        self.soc.gic.reset_cpu_interface(index);
    }

    /// Plug the Ethernet port into the virtual 10.0.2.0/24 network (DHCP, DNS, ICMP);
    /// `nat` also relays guest TCP/UDP to the host's network (native builds only).
    pub fn connect_network(&mut self, nat: bool, log: bool) {
        let mut net = sbc_net::net::VirtualNet::new(log);
        #[cfg(not(target_arch = "wasm32"))]
        if nat {
            net.nat = Some(sbc_net::nat::Nat::new(log));
        }
        let _ = nat;
        self.soc.net = Some(net);
    }

    /// ARM32 Linux zImage entry on core 0: r0 = 0, r1 = ~0, r2 = the DTB address.
    /// Cores 1-3 stay off until PSCI CPU_ON.
    pub fn boot_linux(&mut self, entry: u32, dtb: u32) {
        for index in 0..NCPU {
            self.state[index] = CoreState::Off;
        }
        self.reset_core(0, entry);
        let cpu = &mut self.cpus[0];
        cpu.r[0] = 0;
        cpu.r[1] = u32::MAX;
        cpu.r[2] = dtb;
        self.cur = 0;
        self.update_irqs();
    }

    /// Virtual time in nanoseconds.
    pub fn now(&self) -> u64 {
        ticks_to_ns(self.cpus[0].cntpct) + self.subtick / 12
    }

    fn set_time(&mut self, ticks: u64) {
        for cpu in &mut self.cpus {
            cpu.cntpct = ticks;
        }
    }

    /// One instruction of one core: 1 ns divided among the running cores. Cores asleep
    /// in WFI/WFE do not count, so a lone busy core still runs at 1 ns per instruction.
    fn tick(&mut self, instructions: u64) {
        let running = self.state.iter().filter(|&&s| s == CoreState::Run).count().max(1) as u64;
        self.advance_subtick(instructions * (12 / running));
    }

    /// Advance virtual time by `units` of 1/12 ns.
    fn advance_subtick(&mut self, units: u64) {
        self.subtick += units;
        if self.subtick >= 125 * 12 {
            let periods = self.subtick / (125 * 12);
            self.subtick -= periods * 125 * 12;
            let ticks = self.cpus[0].cntpct + 3 * periods;
            self.set_time(ticks);
        }
    }

    /// Instructions core `k` may run before the next device deadline (compiled blocks
    /// chain until then).
    fn instructions_to_deadline(&self) -> u64 {
        let running = self.state.iter().filter(|&&s| s == CoreState::Run).count().max(1) as u64;
        let now = self.cpus[0].cntpct;
        if self.next_event <= now {
            return 1;
        }
        let units = (self.next_event - now).saturating_mul(500).saturating_sub(self.subtick);
        units.div_ceil(12 / running).max(1)
    }

    fn timer_state_of(cpu: &Cpu) -> (u32, u64, u32, u64) {
        (cpu.cntp_ctl, cpu.cntp_cval, cpu.cntv_ctl, cpu.cntv_cval)
    }

    fn update_irqs(&mut self) {
        let now = self.cpus[0].cntpct;
        self.soc.now = now;
        self.soc.dirty = false;
        self.soc.timer.update(now);
        for (index, &irq) in TIMER_IRQS.iter().enumerate() {
            let level = self.soc.timer.irq(index);
            self.soc.gic.set_level(irq, level);
        }
        let uart = self.soc.uart.irq();
        self.soc.gic.set_level(UART0_IRQ, uart);
        let mmc = self.soc.mmc.irq();
        self.soc.gic.set_level(MMC0_IRQ, mmc);
        self.soc.display.update(now);
        let (tcon, hdmi) = (self.soc.display.tcon_irq(), self.soc.display.hdmi_irq());
        self.soc.gic.set_level(TCON0_IRQ, tcon);
        self.soc.gic.set_level(HDMI_IRQ, hdmi);
        let mut next = self.soc.display.next_deadline().unwrap_or(u64::MAX);
        self.soc.usb_update(now);
        if self.soc.pump_network(now) {
            // poll the network every millisecond while the interface receives
            next = next.min(now + 24_000);
        }
        let emac = self.soc.emac.irq();
        self.soc.gic.set_level(EMAC_IRQ, emac);
        self.soc.gpu.update(now);
        if let Some(due) = self.soc.gpu.next_deadline() {
            next = next.min(due.max(now + 1));
        }
        for (line, irq) in MALI_IRQS {
            let level = self.soc.gpu.irq(line);
            self.soc.gic.set_level(irq, level);
        }
        for port in 0..2 {
            let (ehci, ohci) = (self.soc.ehci[port].irq(), self.soc.ohci[port].irq());
            self.soc.gic.set_level(EHCI_IRQS[port], ehci);
            self.soc.gic.set_level(OHCI_IRQS[port], ohci);
            if let Some(frame) = self.soc.ohci[port].next_deadline() {
                next = next.min(frame);
            }
        }
        for k in 0..NCPU {
            let cpu = &self.cpus[k];
            self.timer_state[k] = Self::timer_state_of(cpu);
            let physical = cpu.physical_timer_irq();
            let virtual_ = cpu.virtual_timer_irq();
            if self.state[k] != CoreState::Off {
                for (ctl, cval) in [(cpu.cntp_ctl, cpu.cntp_cval), (cpu.cntv_ctl, cpu.cntv_cval)] {
                    if ctl & 1 != 0 && cval > now {
                        next = next.min(cval);
                    }
                }
            }
            self.soc.gic.set_ppi(k, PHYSICAL_TIMER_PPI, physical);
            self.soc.gic.set_ppi(k, SECURE_PHYSICAL_TIMER_PPI, physical);
            self.soc.gic.set_ppi(k, VIRTUAL_TIMER_PPI, virtual_);
        }
        for k in 0..NCPU {
            let irq = self.soc.gic.irq_out_for(k);
            self.cpus[k].set_irq(irq, false);
            if irq && self.state[k] == CoreState::Wait {
                self.state[k] = CoreState::Run;
            }
        }
        if let Some(deadline) = self.soc.timer.next_deadline(now) {
            next = next.min(deadline.max(now + 1));
        }
        self.next_event = next;
    }

    fn sync_irqs(&mut self) {
        let changed = self
            .cpus
            .iter()
            .zip(&self.timer_state)
            .any(|(cpu, state)| Self::timer_state_of(cpu) != *state);
        if self.soc.dirty || changed || self.cpus[0].cntpct >= self.next_event {
            self.update_irqs();
        }
    }

    pub fn advance_time(&mut self, ticks: u64) {
        let now = self.cpus[0].cntpct + ticks;
        self.set_time(now);
        self.update_irqs();
    }

    /// Advance idle virtual time toward an absolute deadline in nanoseconds.
    pub fn idle_until(&mut self, time_ns: u64) {
        let ticks = ns_to_ticks(time_ns);
        if ticks > self.cpus[0].cntpct {
            self.set_time(ticks);
            self.subtick = 0;
        }
        self.update_irqs();
    }

    /// Hand the machine to core `k`: the core that stops running loses its exclusive
    /// reservation (another core may now write the location), and the GIC's banked
    /// registers present core `k`'s view.
    fn switch_to(&mut self, k: usize) {
        if k != self.cur {
            self.cpus[self.cur].exclusive_addr = None;
            self.cur = k;
        }
        self.soc.gic.current = k;
    }

    /// Execute one instruction on core 0, fast-forwarding idle time to the next timer
    /// event (single-core tests and probes).
    pub fn step(&mut self) -> Result<(), Trap> {
        self.switch_to(0);
        self.sync_irqs();
        if self.state[0] == CoreState::Wait {
            let ticks = match self.next_event {
                u64::MAX => 1,
                event => event.saturating_sub(self.cpus[0].cntpct).max(1),
            };
            self.advance_time(ticks);
            return Err(Trap::Wait);
        }
        let i0 = self.cpus[0].instret;
        self.cpus[0].chain_until = 0;
        let result = self.cpus[0].step(&mut self.soc);
        // step() runs core 0 alone, so each instruction is a full 1 ns.
        self.advance_subtick(12 * (self.cpus[0].instret - i0).max(1));
        if self.cpus[0].sent_event {
            self.send_event(0);
        }
        if self.cpus[0].tlb_broadcast {
            self.broadcast_tlb_flush(0);
        }
        if result == Err(Trap::Wait) {
            self.state[0] = CoreState::Wait;
            self.update_irqs();
        }
        result
    }

    fn set_debugging(&mut self) {
        let on = !self.breakpoints.at.is_empty() || !self.cpus[0].watch.is_empty();
        if on && self.jit_enabled() {
            self.set_jit(false);
            self.debug_jit = true;
        } else if !on && std::mem::take(&mut self.debug_jit) {
            self.set_jit(true);
        }
    }

    /// A debugger's breakpoint at `addr` (the cores are interpreted while any is set).
    pub fn debug_breakpoint(&mut self, addr: u64, insert: bool) {
        self.breakpoints.set(addr, insert);
        self.set_debugging();
    }

    /// Watch [addr, addr + len) for accesses of `kind` (`emu_core::debug::WATCH_*`) on every core.
    pub fn debug_watch(&mut self, addr: u64, len: u64, kind: u8, insert: bool) {
        for c in &mut self.cpus {
            c.set_watch(addr, len, kind, insert);
        }
        self.set_debugging();
    }

    /// Execute one instruction on core `k` (interpreted; a sleeping core is woken); a stop it
    /// causes, if any. `run` does it, giving the other cores no turn meanwhile.
    #[cold]
    pub fn debug_step(&mut self, k: usize) -> Option<Stop> {
        if !matches!(self.state[k], CoreState::Run | CoreState::Wait) {
            return None;
        }
        let jit = self.jit_enabled();
        if jit {
            self.set_jit(false);
        }
        self.state[k] = CoreState::Run;
        self.breakpoints.resume = Some((k, self.cpus[k].r[15] as u64));
        self.breakpoints.only = Some(k);
        let stop = self.run(1);
        self.breakpoints.only = None;
        self.breakpoints.resume = None;
        if jit {
            self.set_jit(true);
        }
        (!matches!(stop, Stop::Budget | Stop::Idle(_))).then_some(stop)
    }

    /// The RAM behind [va, va + len) as core `k` maps it, a page at a time.
    fn debug_pages(&mut self, k: usize, va: u64, len: usize) -> Option<Vec<(usize, usize)>> {
        let (base, size) = (self.soc.ram.base, self.soc.ram.mem.len());
        let (mut pages, mut done) = (Vec::new(), 0);
        while done < len {
            let a = (va as u32).wrapping_add(done as u32);
            let pa = self.cpus[k].debug_translate(&mut self.soc, a)? as u64;
            let n = (len - done).min(0x1000 - (a & 0xfff) as usize);
            let o = pa.checked_sub(base)? as usize;
            if o + n > size {
                return None;
            }
            pages.push((o, n));
            done += n;
        }
        Some(pages)
    }

    /// Guest RAM at virtual address `va` as core `k` sees it.
    pub fn debug_read(&mut self, k: usize, va: u64, len: usize) -> Option<Vec<u8>> {
        let pages = self.debug_pages(k, va, len)?;
        Some(pages.into_iter().flat_map(|(o, n)| self.soc.ram.mem[o..o + n].to_vec()).collect())
    }

    /// Write guest RAM at virtual address `va`; compiled code is dropped.
    pub fn debug_write(&mut self, k: usize, va: u64, data: &[u8]) -> bool {
        let Some(pages) = self.debug_pages(k, va, data.len()) else { return false };
        let mut done = 0;
        for (o, n) in pages {
            self.soc.ram.mem[o..o + n].copy_from_slice(&data[done..done + n]);
            done += n;
        }
        if self.jit_enabled() {
            self.set_jit(false);
            self.set_jit(true);
        }
        true
    }

    /// Run up to `budget` instructions across the cores. Returns `Stop::Idle(deadline_ns)`
    /// when every online core waits for an interrupt and nothing is due before then.
    pub fn run(&mut self, budget: u64) -> Stop {
        let mut left = budget;
        // Exception-storm detection per core: one core's progress must not hide another's
        // abort loop.
        let mut storm = [0u32; NCPU];
        while left > 0 {
            self.sync_irqs();
            let Some(k) = (1..=NCPU)
                .map(|offset| (self.cur + offset) % NCPU)
                .find(|&k| self.state[k] == CoreState::Run && self.breakpoints.may_run(k))
            else {
                return Stop::Idle(match self.next_event {
                    u64::MAX => u64::MAX,
                    event => ticks_to_ns(event),
                });
            };
            self.switch_to(k);
            let mut quantum = left.min(QUANTUM);
            // what a debugger has set only changes between runs
            let (breaks, watches) = (!self.breakpoints.at.is_empty(), !self.cpus[k].watch.is_empty());
            while quantum > 0 {
                if breaks && self.breakpoints.hit(k, self.cpus[k].r[15] as u64) {
                    return Stop::Breakpoint(k);
                }
                let i0 = self.cpus[k].instret;
                self.cpus[k].chain_until = i0 + quantum.min(self.instructions_to_deadline());
                let result = self.cpus[k].step(&mut self.soc);
                // a compiled block runs several instructions; a trap that retired nothing
                // still takes a tick
                let n = (self.cpus[k].instret - i0).max(1);
                quantum = quantum.saturating_sub(n);
                left = left.saturating_sub(n);
                self.tick(n);
                if self.cpus[k].sent_event {
                    self.send_event(k);
                }
                if self.cpus[k].tlb_broadcast {
                    self.broadcast_tlb_flush(k);
                }
                match result {
                    Ok(()) => storm[k] = 0,
                    Err(Trap::Wait) => {
                        self.state[k] = CoreState::Wait;
                        self.update_irqs();
                        break;
                    }
                    Err(Trap::Interrupt(..)) => {
                        self.irqs += 1;
                        storm[k] = 0;
                    }
                    Err(Trap::Exception(class, _)) => {
                        self.exceptions += 1;
                        if self.trace_exceptions > 0 && self.cpus[k].instret >= self.trace_from {
                            self.trace_exceptions -= 1;
                            let c = &self.cpus[k];
                            let va = if class == 0x21 { c.ifar } else { c.dfar };
                            let n = c.ttbcr & 7;
                            let table = if n != 0 && (va as u64) >= (1u64 << 32 >> n) { c.ttbr1 & !0x3fff } else { c.ttbr0 & !((1u32 << (14 - n)) - 1) };
                            let l1 = self.soc.ram.read32(table as u64 + (va >> 20) as u64 * 4).unwrap_or(0xdead);
                            eprintln!(
                                "[h3 exc] core {k} class {class:#x} vector {:#010x} lr {:#010x} spsr {:#010x} dfsr {:#x} dfar {:#010x} ifsr {:#x} ifar {:#010x} insns {} ttbcr {:#x} l1 {l1:#010x} dacr {:#x} sctlr {:#x}",
                                c.r[15], c.r[14], c.spsr(), c.dfsr, c.dfar, c.ifsr, c.ifar, c.instret, c.ttbcr, c.dacr, c.sctlr
                            );
                        }
                        storm[k] += 1;
                        if storm[k] > 10_000 {
                            self.last_stop_core = k;
                            let c = &self.cpus[k];
                            return Stop::ExceptionStorm {
                                pc: c.r[15] as u64,
                                esr: (c.dfsr as u64) << 32 | c.ifsr as u64,
                                elr: c.r[14] as u64,
                                far: c.dfar as u64,
                                el: 1,
                            };
                        }
                    }
                    Err(Trap::HostCall(_)) => {
                        if let Some(stop) = self.psci(k) {
                            self.last_stop_core = k;
                            return stop;
                        }
                        if self.state[k] != CoreState::Run {
                            break;
                        }
                    }
                    Err(Trap::Unimplemented(pc, word)) => {
                        self.last_stop_core = k;
                        return Stop::Unimplemented(pc, word);
                    }
                    Err(Trap::Brk(n)) => {
                        self.last_stop_core = k;
                        return Stop::Brk(n);
                    }
                    Err(Trap::Hlt(n)) => {
                        self.last_stop_core = k;
                        return Stop::Hlt(n);
                    }
                }
                if watches {
                    if let Some((address, access)) = self.cpus[k].watch.hit.take() {
                        self.update_irqs();
                        return Stop::Watchpoint(k, address, access);
                    }
                }
                // A device access (e.g. an IPI), a timer deadline or this core's own timer
                // registers may change an interrupt line.
                if self.soc.dirty
                    || self.cpus[0].cntpct >= self.next_event
                    || Self::timer_state_of(&self.cpus[k]) != self.timer_state[k]
                {
                    self.update_irqs();
                }
            }
        }
        Stop::Budget
    }

    /// Inner-shareable TLB maintenance on core `k` reaches every core (DVM broadcast).
    /// Without it, another core kept stale translations of unmapped vmalloc pages and
    /// wrote into freed memory.
    fn broadcast_tlb_flush(&mut self, k: usize) {
        self.cpus[k].tlb_broadcast = false;
        for (j, cpu) in self.cpus.iter_mut().enumerate() {
            if j != k {
                cpu.tlb_flush();
            }
        }
    }

    /// SEV from core `k`: every core's event register is set; cores sleeping in WFE
    /// wake (consuming the event).
    fn send_event(&mut self, k: usize) {
        self.cpus[k].sent_event = false;
        for j in 0..NCPU {
            if j == k || self.state[j] == CoreState::Off {
                continue;
            }
            if self.state[j] == CoreState::Wait && self.cpus[j].last_wfe {
                self.state[j] = CoreState::Run;
            } else {
                self.cpus[j].event = true;
            }
        }
    }

    /// Host PSCI 1.0, called by core `k`.
    fn psci(&mut self, k: usize) -> Option<Stop> {
        let [function, target, entry, context] = [0, 1, 2, 3].map(|n| self.cpus[k].r[n]);
        let core_of = |mpidr: u32| -> Option<usize> {
            let index = (mpidr & 0xff) as usize;
            (mpidr & 0xff_ff00 == 0 && index < NCPU).then_some(index)
        };
        let ret = match function {
            psci::VERSION => 0x0001_0000,
            psci::SYSTEM_OFF => return Some(Stop::PowerOff),
            psci::SYSTEM_RESET => return Some(Stop::Reset),
            psci::MIGRATE_INFO_TYPE => 2, // trusted OS not present
            psci::AFFINITY_INFO => match core_of(target) {
                Some(index) => u32::from(self.state[index] == CoreState::Off),
                None => psci::INVALID_PARAMETERS,
            },
            psci::CPU_ON => match core_of(target) {
                Some(index) if self.state[index] != CoreState::Off => psci::ALREADY_ON,
                Some(index) => {
                    self.reset_core(index, entry);
                    self.cpus[index].r[0] = context;
                    self.update_irqs();
                    psci::SUCCESS
                }
                None => psci::INVALID_PARAMETERS,
            },
            psci::CPU_OFF => {
                self.state[k] = CoreState::Off;
                self.cpus[k].set_irq(false, false);
                return None;
            }
            psci::FEATURES => match target {
                psci::VERSION | psci::SYSTEM_OFF | psci::SYSTEM_RESET | psci::FEATURES
                | psci::AFFINITY_INFO | psci::MIGRATE_INFO_TYPE | psci::CPU_ON
                | psci::CPU_OFF => 0,
                _ => psci::NOT_SUPPORTED,
            },
            _ => psci::NOT_SUPPORTED,
        };
        self.cpus[k].r[0] = ret;
        None
    }

    pub fn serial_output(&self) -> &[u8] {
        &self.soc.uart.out
    }

    pub fn take_serial_output(&mut self) -> Vec<u8> {
        std::mem::take(&mut self.soc.uart.out)
    }

    pub fn serial_input(&mut self, byte: u8) -> bool {
        let accepted = self.soc.uart.push_rx(byte);
        self.soc.dirty = true;
        accepted
    }
}

pub use peripherals::timebase::{ns_to_ticks, ticks_to_ns};

impl Default for Machine {
    fn default() -> Self {
        Self::new()
    }
}

macro_rules! mmio_read {
    ($name:ident, $type:ty, $width:expr) => {
        fn $name(&mut self, addr: u64) -> Result<$type, Fault> {
            if self.has_ram(addr, $width) {
                self.ram.$name(addr)
            } else {
                self.mmio_read(addr, $width).map(|value| value as $type)
            }
        }
    };
}

macro_rules! mmio_write {
    ($name:ident, $type:ty, $width:expr) => {
        fn $name(&mut self, addr: u64, value: $type) -> Result<(), Fault> {
            if self.has_ram(addr, $width) {
                self.ram.$name(addr, value)
            } else {
                self.mmio_write(addr, $width, value as u32)
            }
        }
    };
}

impl Bus for Soc {
    mmio_read!(read8, u8, 1);
    mmio_read!(read16, u16, 2);
    mmio_read!(read32, u32, 4);
    mmio_write!(write8, u8, 1);
    mmio_write!(write16, u16, 2);
    mmio_write!(write32, u32, 4);

    fn read64(&mut self, addr: u64) -> Result<u64, Fault> {
        if self.has_ram(addr, 8) {
            self.ram.read64(addr)
        } else {
            self.mmio_read(addr, 8).map(u64::from)
        }
    }

    fn write64(&mut self, addr: u64, value: u64) -> Result<(), Fault> {
        if self.has_ram(addr, 8) {
            self.ram.write64(addr, value)
        } else {
            self.mmio_write(addr, 8, value as u32)
        }
    }
}
