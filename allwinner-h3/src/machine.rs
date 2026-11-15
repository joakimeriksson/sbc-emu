use crate::gic::NCPU;
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
        Self {
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
        }
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
        cpu.r[15] = entry;
        cpu.sctlr &= !(1 | 1 << 2 | 1 << 12);
        cpu.undef_to_guest = true;
        cpu.firmware_calls_to_host = true;
        cpu.cntpct = now;
        self.cpus[index] = cpu;
        self.state[index] = CoreState::Run;
        self.soc.gic.reset_cpu_interface(index);
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
    fn tick(&mut self) {
        let running = self.state.iter().filter(|&&s| s == CoreState::Run).count().max(1) as u64;
        self.subtick += 12 / running;
        if self.subtick >= 125 * 12 {
            self.subtick -= 125 * 12;
            let ticks = self.cpus[0].cntpct + 3;
            self.set_time(ticks);
        }
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
        let mut next = u64::MAX;
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
        let result = self.cpus[0].step(&mut self.soc);
        self.tick();
        if self.cpus[0].sent_event {
            self.send_event(0);
        }
        if result == Err(Trap::Wait) {
            self.state[0] = CoreState::Wait;
            self.update_irqs();
        }
        result
    }

    /// Run up to `budget` instructions across the cores. Returns `Stop::Idle(deadline_ns)`
    /// when every online core waits for an interrupt and nothing is due before then.
    pub fn run(&mut self, budget: u64) -> Stop {
        let mut left = budget;
        let mut storm = 0u32;
        while left > 0 {
            self.sync_irqs();
            let Some(k) = (1..=NCPU)
                .map(|offset| (self.cur + offset) % NCPU)
                .find(|&k| self.state[k] == CoreState::Run)
            else {
                return Stop::Idle(match self.next_event {
                    u64::MAX => u64::MAX,
                    event => ticks_to_ns(event),
                });
            };
            self.switch_to(k);
            let quantum = left.min(QUANTUM);
            for _ in 0..quantum {
                left -= 1;
                let result = self.cpus[k].step(&mut self.soc);
                self.tick();
                if self.cpus[k].sent_event {
                    self.send_event(k);
                }
                match result {
                    Ok(()) => storm = 0,
                    Err(Trap::Wait) => {
                        self.state[k] = CoreState::Wait;
                        self.update_irqs();
                        break;
                    }
                    Err(Trap::Interrupt(..)) => {
                        self.irqs += 1;
                        storm = 0;
                    }
                    Err(Trap::Exception(..)) => {
                        self.exceptions += 1;
                        storm += 1;
                        if storm > 10_000 {
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

/// 24 MHz ticks to nanoseconds (rounded up) and back (rounded down), so that
/// `ns_to_ticks(ticks_to_ns(t)) >= t`: an idle deadline converted to nanoseconds and back
/// always reaches the timer compare value.
pub fn ticks_to_ns(ticks: u64) -> u64 {
    (ticks as u128 * 125).div_ceil(3) as u64
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
