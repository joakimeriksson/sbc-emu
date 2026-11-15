use crate::cru::{Cru, Grf, Pmu, CRU_SIZE, GRF_SIZE, PMU_SIZE};
use allwinner_h3::{Gic, Uart};
use arm_a32::Cpu;
use emu_core::{Bus, Core, Fault, FlatRam, Stop, Trap};

pub const RAM_BASE: u64 = 0;
/// Default DRAM: 1 GiB (the Tinker Board S has 2 GiB; `with_ram_size` takes up to 0xfe00_0000).
pub const RAM_SIZE: usize = 1024 << 20;
/// MIDR of the RK3288's cores: ARM Cortex-A12 r0p1 (part 0xc0d; Arm later renamed the A12
/// the Cortex-A17, part 0xc0e). The device tree says `arm,cortex-a12`.
pub const RK3288_MIDR: u32 = 0x410f_c0d1;
/// Cluster 5 in MPIDR (`cpu@500`..`cpu@503`).
pub const RK3288_MPIDR: u32 = 0x8000_0500;
pub const UART2_BASE: u64 = 0xff69_0000;
const UART_SIZE: u64 = 0x100;
/// UART2 interrupt: SPI 57.
pub const UART2_IRQ: usize = 32 + 57;
pub const CRU_BASE: u64 = 0xff76_0000;
pub const GRF_BASE: u64 = 0xff77_0000;
pub const PMU_BASE: u64 = 0xff73_0000;
pub const GICD_BASE: u64 = 0xffc0_1000;
pub const GICC_BASE: u64 = 0xffc0_2000;
const GICD_SIZE: u64 = 0x1000;
const GICC_SIZE: u64 = 0x2000;
/// Architectural timer PPIs. The RK3288 tree marks the timer `arm,cpu-registers-not-fw-
/// configured`, so ARM32 Linux uses the secure physical timer (PPI 13). The core has no
/// TrustZone model, so CNTP_* drives both physical lines.
pub const SECURE_PHYSICAL_TIMER_PPI: usize = 29;
pub const PHYSICAL_TIMER_PPI: usize = 30;
pub const VIRTUAL_TIMER_PPI: usize = 27;
/// RK3288 peripheral window (APB/AHB devices, on-chip SRAM, GIC, boot ROM). Addresses no
/// model claims behave as plain read-back registers so unmodelled drivers probe without
/// external aborts; on-chip SRAM (0xff70_0000, 0xff72_0000) works through the same store.
/// Every first touch of a 4 KiB page is recorded in `Soc::stub_pages`.
const DEVICE_WINDOW: (u64, u64) = (0xff00_0000, 0x1_0000_0000);

/// Physical devices visible to the boot core.
pub struct Soc {
    pub ram: FlatRam,
    pub uart: Uart,
    pub gic: Gic,
    pub cru: Cru,
    pub grf: Grf,
    pub pmu: Pmu,
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
        assert!(ram_size as u64 <= DEVICE_WINDOW.0, "DRAM overlaps the device window");
        Self {
            ram: FlatRam::new(RAM_BASE, ram_size),
            uart: Uart::default(),
            gic: Gic::default(),
            cru: Cru::new(),
            grf: Grf::default(),
            pmu: Pmu::default(),
            now: 0,
            dirty: true,
            stub_regs: Default::default(),
            stub_pages: Vec::new(),
            log_stub_pages: false,
        }
    }

    fn stub_access(&mut self, addr: u64, width: u64, write: Option<u32>) -> Result<u32, Fault> {
        if !(DEVICE_WINDOW.0..DEVICE_WINDOW.1).contains(&addr) {
            return Err(Fault::Unmapped);
        }
        let page = addr & !0xfff;
        if !self.stub_pages.contains(&page) {
            self.stub_pages.push(page);
            if self.log_stub_pages {
                eprintln!("[rk3288] unmodelled device page {page:#010x} (first access {addr:#010x})");
            }
        }
        let word = addr & !3;
        let shift = (addr & 3) * 8;
        let old = self.stub_regs.get(&word).copied().unwrap_or(0);
        match write {
            Some(value) => {
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

    /// Word-only register block: Ok(Some(offset)) inside it, Prohibited for other widths.
    fn word_offset(addr: u64, width: u64, base: u64, size: u64) -> Result<Option<u64>, Fault> {
        if !Self::in_region(addr, width, base, size) {
            Ok(None)
        } else if width != 4 || addr & 3 != 0 {
            Err(Fault::Prohibited)
        } else {
            Ok(Some(addr - base))
        }
    }

    fn mmio_read(&mut self, addr: u64, width: u64) -> Result<u32, Fault> {
        self.dirty = true;
        if let Some(offset) = Self::word_offset(addr, width, UART2_BASE, UART_SIZE)? {
            return Ok(self.uart.read(offset));
        }
        if let Some(offset) = Self::word_offset(addr, width, CRU_BASE, CRU_SIZE)? {
            return Ok(self.cru.read(offset));
        }
        if let Some(offset) = Self::word_offset(addr, width, GRF_BASE, GRF_SIZE)? {
            return Ok(self.grf.read(offset, &self.cru));
        }
        if let Some(offset) = Self::word_offset(addr, width, PMU_BASE, PMU_SIZE)? {
            return Ok(self.pmu.read(offset));
        }
        let distributor = Self::in_region(addr, width, GICD_BASE, GICD_SIZE);
        let interface = Self::in_region(addr, width, GICC_BASE, GICC_SIZE);
        if distributor || interface {
            // GICv2 allows byte accesses to the distributor; the CPU interface is word-only.
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
        self.stub_access(addr, width, None)
    }

    fn mmio_write(&mut self, addr: u64, width: u64, value: u32) -> Result<(), Fault> {
        self.dirty = true;
        if let Some(offset) = Self::word_offset(addr, width, UART2_BASE, UART_SIZE)? {
            self.uart.write(offset, value);
            return Ok(());
        }
        if let Some(offset) = Self::word_offset(addr, width, CRU_BASE, CRU_SIZE)? {
            self.cru.write(offset, value);
            return Ok(());
        }
        if let Some(offset) = Self::word_offset(addr, width, GRF_BASE, GRF_SIZE)? {
            self.grf.write(offset, value);
            return Ok(());
        }
        if let Some(offset) = Self::word_offset(addr, width, PMU_BASE, PMU_SIZE)? {
            self.pmu.write(offset, value);
            return Ok(());
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
                    // Set/clear-enable, pending and active registers are write-one: the other
                    // bytes of the word must be written as zero.
                    let old = if (0x100..0x400).contains(&word) { 0 } else { self.gic.dist_read(word) };
                    old & !mask | (value << shift) & mask
                };
                self.gic.dist_write(word, value);
            } else {
                self.gic.cpu_write(addr - GICC_BASE, value);
            }
            return Ok(());
        }
        self.stub_access(addr, width, Some(value)).map(|_| ())
    }
}

/// One-core RK3288 machine: Cortex-A12/A17 identity on `arm-a32`, GIC-400, UART2, CRU,
/// GRF PLL lock, PMU power domains and the architectural timers.
///
/// Time is the 24 MHz architectural count; each instruction takes 1 ns of virtual time, so
/// the count advances 3 ticks every 125 instructions (deterministic). `step` fast-forwards
/// idle time to the next timer event; `run` returns `Stop::Idle` so a host loop can pace idle
/// time against the wall clock. The RK3288 has no secure monitor under mainline Linux (no
/// PSCI); secondary cores start through the boot ROM mailbox, which is not modelled yet, so
/// the kernel runs with `nosmp`.
pub struct Machine {
    pub cpu: Cpu,
    pub soc: Soc,
    waiting: bool,
    next_event: u64,
    timer_state: (u32, u64, u32, u64),
    pub irqs: u64,
    pub exceptions: u64,
    subtick: u32,
}

impl Machine {
    pub fn new() -> Self {
        Self::with_ram_size(RAM_SIZE)
    }

    pub fn with_ram_size(ram_size: usize) -> Self {
        let mut machine = Self {
            cpu: Self::new_cpu(),
            soc: Soc::new(ram_size),
            waiting: false,
            next_event: 0,
            timer_state: (0, 0, 0, 0),
            irqs: 0,
            exceptions: 0,
            subtick: 0,
        };
        machine.cpu.r[15] = RAM_BASE as u32;
        machine
    }

    fn new_cpu() -> Cpu {
        let mut cpu = Cpu::new();
        cpu.midr = RK3288_MIDR;
        cpu.mpidr = RK3288_MPIDR;
        cpu
    }

    pub fn load(&mut self, addr: u64, bytes: &[u8]) -> Result<(), Fault> {
        self.soc.ram.load(addr, bytes)
    }

    /// ARM32 Linux zImage entry: SVC/A32, MMU and caches off, IRQ/FIQ masked, r0 = 0,
    /// r1 = ~0 (device-tree boot) and r2 = the DTB physical address.
    pub fn boot_linux(&mut self, entry: u32, dtb: u32) {
        self.cpu = Self::new_cpu();
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
        let uart = self.soc.uart.irq();
        self.soc.gic.set_level(UART2_IRQ, uart);
        let physical = self.cpu.physical_timer_irq();
        self.soc.gic.set_level(PHYSICAL_TIMER_PPI, physical);
        self.soc.gic.set_level(SECURE_PHYSICAL_TIMER_PPI, physical);
        self.soc.gic.set_level(VIRTUAL_TIMER_PPI, self.cpu.virtual_timer_irq());
        let irq = self.soc.gic.irq_out();
        self.cpu.set_irq(irq, false);
        if irq {
            self.waiting = false;
        }
        self.next_event = self.next_timer_event(now);
    }

    fn next_timer_event(&self, now: u64) -> u64 {
        [(self.cpu.cntp_ctl, self.cpu.cntp_cval), (self.cpu.cntv_ctl, self.cpu.cntv_cval)]
            .into_iter()
            .filter(|&(ctl, cval)| ctl & 1 != 0 && cval > now)
            .map(|(_, cval)| cval)
            .min()
            .unwrap_or(u64::MAX)
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
            if self.soc.cru.reset_requested {
                self.soc.cru.reset_requested = false;
                return Stop::Reset;
            }
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
                // No secure firmware on the RK3288: SMC returns PSCI NOT_SUPPORTED.
                Err(Trap::HostCall(_)) => self.cpu.r[0] = -1i32 as u32,
                Err(Trap::Unimplemented(pc, word)) => return Stop::Unimplemented(pc, word),
                Err(Trap::Brk(n)) => return Stop::Brk(n),
                Err(Trap::Hlt(n)) => return Stop::Hlt(n),
            }
        }
        Stop::Budget
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

// Shared with the H3: rounds idle deadlines up so ns -> ticks reaches the compare value.
pub use allwinner_h3::{ns_to_ticks, ticks_to_ns};

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

    /// Doubleword device accesses (LDRD/STRD to on-chip SRAM) are two word accesses.
    fn read64(&mut self, addr: u64) -> Result<u64, Fault> {
        if self.has_ram(addr, 8) {
            self.ram.read64(addr)
        } else {
            let low = self.mmio_read(addr, 4)?;
            let high = self.mmio_read(addr + 4, 4)?;
            Ok(u64::from(low) | u64::from(high) << 32)
        }
    }

    fn write64(&mut self, addr: u64, value: u64) -> Result<(), Fault> {
        if self.has_ram(addr, 8) {
            self.ram.write64(addr, value)
        } else {
            self.mmio_write(addr, 4, value as u32)?;
            self.mmio_write(addr + 4, 4, (value >> 32) as u32)
        }
    }
}
