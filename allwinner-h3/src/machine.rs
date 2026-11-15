use crate::{Gic, Timer, Uart};
use arm_a32::Cpu;
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
/// Timer 0/1 interrupts: SPI 18/19.
pub const TIMER_IRQS: [usize; 2] = [32 + 18, 32 + 19];
const GICD_SIZE: u64 = 0x1000;
const GICC_SIZE: u64 = 0x2000;

/// Physical devices visible to the initial H3 core.
pub struct Soc {
    pub ram: FlatRam,
    pub uart: Uart,
    pub gic: Gic,
    pub timer: Timer,
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
}

impl Soc {
    pub fn new(ram_size: usize) -> Self {
        Self {
            ram: FlatRam::new(RAM_BASE, ram_size),
            uart: Uart::default(),
            gic: Gic::default(),
            timer: Timer::default(),
            ccu: Self::ccu_reset(),
            now: 0,
            dirty: true,
            stub_regs: Default::default(),
            stub_pages: Vec::new(),
            log_stub_pages: false,
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
        if width == 4 && addr & 3 == 0 {
            if Self::in_region(addr, 4, CCU_BASE, CCU_SIZE) {
                return Ok(self.ccu_read(addr - CCU_BASE));
            }
            if Self::in_region(addr, 4, TIMER_BASE, TIMER_SIZE) {
                return Ok(self.timer.read(addr - TIMER_BASE, self.now));
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
        if width == 4 && addr & 3 == 0 {
            if Self::in_region(addr, 4, CCU_BASE, CCU_SIZE) {
                self.ccu[((addr - CCU_BASE) / 4) as usize] = value & !(1 << 28);
                return Ok(());
            }
            if Self::in_region(addr, 4, TIMER_BASE, TIMER_SIZE) {
                self.timer.write(addr - TIMER_BASE, value, self.now);
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

/// PSCI function identifiers (SMC32 calling convention).
mod psci {
    pub const VERSION: u32 = 0x8400_0000;
    pub const CPU_OFF: u32 = 0x8400_0002;
    pub const CPU_ON: u32 = 0x8400_0003;
    pub const AFFINITY_INFO: u32 = 0x8400_0004;
    pub const MIGRATE_INFO_TYPE: u32 = 0x8400_0006;
    pub const SYSTEM_OFF: u32 = 0x8400_0008;
    pub const SYSTEM_RESET: u32 = 0x8400_0009;
    pub const FEATURES: u32 = 0x8400_000a;
    pub const NOT_SUPPORTED: u32 = -1i32 as u32;
    pub const INVALID_PARAMETERS: u32 = -2i32 as u32;
    pub const ALREADY_ON: u32 = -4i32 as u32;
}

/// One-core H3 machine: Cortex-A7, GIC-400, UART0, CCU, timers.
///
/// Time is the 24 MHz architectural count. Each executed instruction takes 1 ns of
/// virtual time (about a 1 GHz Cortex-A7), so the count advances 3 ticks every 125
/// instructions (deterministic). `step` fast-forwards idle time to the next timer event; `run`
/// instead returns `Stop::Idle` so a host loop can pace idle time against the wall clock.
/// Boot firmware is emulated by the host: PSCI over SMC as U-Boot's secure monitor
/// provides it on real boards.
pub struct Machine {
    pub cpu: Cpu,
    pub soc: Soc,
    waiting: bool,
    /// Tick at which interrupt state must next be re-evaluated.
    next_event: u64,
    timer_state: (u32, u64, u32, u64),
    pub irqs: u64,
    pub exceptions: u64,
    /// Instructions executed since the counter last advanced (0..125).
    subtick: u32,
}

impl Machine {
    pub fn new() -> Self {
        Self::with_ram_size(RAM_SIZE)
    }

    pub fn with_ram_size(ram_size: usize) -> Self {
        let mut cpu = Cpu::new();
        cpu.r[15] = RAM_BASE as u32;
        Self {
            cpu,
            soc: Soc::new(ram_size),
            waiting: false,
            next_event: 0,
            timer_state: (0, 0, 0, 0),
            irqs: 0,
            exceptions: 0,
            subtick: 0,
        }
    }

    pub fn load(&mut self, addr: u64, bytes: &[u8]) -> Result<(), Fault> {
        self.soc.ram.load(addr, bytes)
    }

    /// ARM32 Linux zImage entry: SVC/A32, MMU and caches off, IRQ/FIQ masked,
    /// r0 = 0, r1 = ~0 and r2 = the DTB physical address. SMC goes to host PSCI.
    pub fn boot_linux(&mut self, entry: u32, dtb: u32) {
        self.cpu = Cpu::new();
        self.cpu.r[0] = 0;
        self.cpu.r[1] = u32::MAX;
        self.cpu.r[2] = dtb;
        self.cpu.r[15] = entry;
        self.cpu.sctlr &= !(1 | 1 << 2 | 1 << 12);
        self.cpu.undef_to_guest = true;
        self.cpu.firmware_calls_to_host = true;
        self.waiting = false;
        self.update_irqs();
    }

    /// Virtual time in nanoseconds.
    pub fn now(&self) -> u64 {
        ticks_to_ns(self.cpu.cntpct) + self.subtick as u64
    }

    /// One instruction's worth of virtual time (1 ns).
    fn tick(&mut self) {
        self.subtick += 1;
        if self.subtick == 125 {
            self.subtick = 0;
            self.cpu.cntpct += 3;
        }
    }

    fn timer_state(&self) -> (u32, u64, u32, u64) {
        (self.cpu.cntp_ctl, self.cpu.cntp_cval, self.cpu.cntv_ctl, self.cpu.cntv_cval)
    }

    fn update_irqs(&mut self) {
        let now = self.cpu.cntpct;
        self.soc.now = now;
        self.soc.dirty = false;
        self.timer_state = self.timer_state();
        self.soc.timer.update(now);
        for (index, &irq) in TIMER_IRQS.iter().enumerate() {
            let level = self.soc.timer.irq(index);
            self.soc.gic.set_level(irq, level);
        }
        let uart = self.soc.uart.irq();
        self.soc.gic.set_level(UART0_IRQ, uart);
        let physical = self.cpu.physical_timer_irq();
        self.soc.gic.set_level(PHYSICAL_TIMER_PPI, physical);
        self.soc.gic.set_level(SECURE_PHYSICAL_TIMER_PPI, physical);
        self.soc
            .gic
            .set_level(VIRTUAL_TIMER_PPI, self.cpu.virtual_timer_irq());
        let irq = self.soc.gic.irq_out();
        self.cpu.set_irq(irq, false);
        if irq {
            self.waiting = false;
        }
        self.next_event = self.next_timer_event(now);
    }

    /// Earliest future tick at which a timer output can change.
    fn next_timer_event(&self, now: u64) -> u64 {
        let mut next = u64::MAX;
        for (ctl, cval) in [
            (self.cpu.cntp_ctl, self.cpu.cntp_cval),
            (self.cpu.cntv_ctl, self.cpu.cntv_cval),
        ] {
            if ctl & 1 != 0 && cval > now {
                next = next.min(cval);
            }
        }
        if let Some(deadline) = self.soc.timer.next_deadline(now) {
            next = next.min(deadline.max(now + 1));
        }
        next
    }

    fn sync_irqs(&mut self) {
        if self.soc.dirty
            || self.cpu.cntpct >= self.next_event
            || self.timer_state != self.timer_state()
        {
            self.update_irqs();
        }
    }

    pub fn advance_time(&mut self, ticks: u64) {
        self.cpu.advance_time(ticks);
        self.update_irqs();
    }

    /// Advance idle virtual time toward an absolute deadline in nanoseconds.
    pub fn idle_until(&mut self, time_ns: u64) {
        let ticks = ns_to_ticks(time_ns);
        if ticks > self.cpu.cntpct {
            self.cpu.cntpct = ticks;
            self.subtick = 0;
        }
        self.update_irqs();
    }

    /// Execute one instruction, fast-forwarding idle time to the next timer event.
    pub fn step(&mut self) -> Result<(), Trap> {
        self.sync_irqs();
        if self.waiting && !self.cpu.irq {
            let ticks = match self.next_event {
                u64::MAX => 1,
                event => event.saturating_sub(self.cpu.cntpct).max(1),
            };
            self.cpu.advance_time(ticks);
            self.update_irqs();
            return Err(Trap::Wait);
        }
        let result = self.cpu.step(&mut self.soc);
        self.tick();
        if result == Err(Trap::Wait) {
            self.waiting = true;
            self.update_irqs();
        }
        result
    }

    /// Run up to `budget` instructions. Returns `Stop::Idle(deadline_ns)` when the core
    /// waits for an interrupt and nothing is due before the deadline.
    pub fn run(&mut self, budget: u64) -> Stop {
        let mut storm = 0u32;
        for _ in 0..budget {
            self.sync_irqs();
            if self.waiting {
                return Stop::Idle(match self.next_event {
                    u64::MAX => u64::MAX,
                    event => ticks_to_ns(event),
                });
            }
            let result = self.cpu.step(&mut self.soc);
            self.tick();
            match result {
                Ok(()) => storm = 0,
                Err(Trap::Wait) => {
                    self.waiting = true;
                    self.update_irqs();
                }
                Err(Trap::Interrupt(..)) => {
                    self.irqs += 1;
                    storm = 0;
                }
                Err(Trap::Exception(..)) => {
                    self.exceptions += 1;
                    storm += 1;
                    if storm > 10_000 {
                        let c = &self.cpu;
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
                    if let Some(stop) = self.psci() {
                        return stop;
                    }
                }
                Err(Trap::Unimplemented(pc, word)) => return Stop::Unimplemented(pc, word),
                Err(Trap::Brk(n)) => return Stop::Brk(n),
                Err(Trap::Hlt(n)) => return Stop::Hlt(n),
            }
        }
        Stop::Budget
    }

    /// Host PSCI 1.0 for the single modelled core.
    fn psci(&mut self) -> Option<Stop> {
        let function = self.cpu.r[0];
        let ret = match function {
            psci::VERSION => 0x0001_0000,
            psci::SYSTEM_OFF => return Some(Stop::PowerOff),
            psci::SYSTEM_RESET => return Some(Stop::Reset),
            psci::MIGRATE_INFO_TYPE => 2, // trusted OS not present
            psci::AFFINITY_INFO => u32::from(self.cpu.r[1] & 0xff != 0), // 0 = on, 1 = off
            psci::CPU_ON => {
                if self.cpu.r[1] & 0xff == 0 {
                    psci::ALREADY_ON
                } else {
                    psci::INVALID_PARAMETERS // secondary cores are not modelled yet
                }
            }
            psci::FEATURES => match self.cpu.r[1] {
                psci::VERSION | psci::SYSTEM_OFF | psci::SYSTEM_RESET | psci::FEATURES
                | psci::AFFINITY_INFO | psci::MIGRATE_INFO_TYPE => 0,
                _ => psci::NOT_SUPPORTED,
            },
            psci::CPU_OFF => psci::NOT_SUPPORTED,
            _ => psci::NOT_SUPPORTED,
        };
        self.cpu.r[0] = ret;
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

/// 24 MHz ticks to nanoseconds and back.
pub fn ticks_to_ns(ticks: u64) -> u64 {
    (ticks as u128 * 125 / 3) as u64
}

pub fn ns_to_ticks(ns: u64) -> u64 {
    (ns as u128 * 3 / 125) as u64
}

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
