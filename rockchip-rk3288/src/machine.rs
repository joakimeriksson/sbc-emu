use crate::cru::{Cru, Grf, Pmu, CRU_SIZE, GRF_SIZE, PMU_SIZE};
use crate::devices::Peripherals;
use crate::dsi::{Dsi, DSI_SIZE};
use crate::rga::{Rga, RGA_SIZE};
use crate::vop::{Vop, VOP_SIZE};
use allwinner_h3::{Gic, Uart, NCPU};
use arm_a32::{CodeWatch, Cpu, CORTEX_A12};
use std::sync::Arc;
use emu_core::{Bus, Core, Fault, FlatRam, Stop, Trap};

pub const RAM_BASE: u64 = 0;
/// Default DRAM: 1 GiB (the Tinker Board S has 2 GiB; `with_ram_size` takes up to 0xfe00_0000).
pub const RAM_SIZE: usize = 1024 << 20;
/// MIDR of the RK3288's cores: ARM Cortex-A12 r0p1 (part 0xc0d; Arm later renamed the A12
/// the Cortex-A17, part 0xc0e). The device tree says `arm,cortex-a12`.
pub const RK3288_MIDR: u32 = CORTEX_A12.midr;
/// Cluster 5 in MPIDR (`cpu@500`..`cpu@503`).
pub const RK3288_MPIDR: u32 = 0x8000_0500;
pub const UART2_BASE: u64 = 0xff69_0000;
const UART_SIZE: u64 = 0x100;
/// UART2 interrupt: SPI 57.
pub const UART2_IRQ: usize = 32 + 57;
pub const CRU_BASE: u64 = 0xff76_0000;
pub const GRF_BASE: u64 = 0xff77_0000;
pub const PMU_BASE: u64 = 0xff73_0000;
/// The two display controllers, VOP-big and VOP-little (SPI 15 and 16, each shared with the
/// VOP's IOMMU).
pub const VOP_BASES: [u64; 2] = [0xff93_0000, 0xff94_0000];
pub const VOP_IRQS: [usize; 2] = [32 + 15, 32 + 16];
/// Mali-T760 MP4 GPU and its interrupts (SPI 6, 7, 8: "job", "mmu", "gpu").
pub const GPU_BASE: u64 = 0xffa3_0000;
pub const GPU_IRQS: [(midgard::Irq, usize); 3] =
    [(midgard::Irq::Job, 32 + 6), (midgard::Irq::Mmu, 32 + 7), (midgard::Irq::Gpu, 32 + 8)];
/// RGA 2D accelerator and its interrupt (SPI 18).
pub const RGA_BASE: u64 = 0xff92_0000;
pub const RGA_IRQ: usize = 32 + 18;
/// MIPI DSI host 0 (the panel board's panel).
pub const DSI0_BASE: u64 = 0xff96_0000;
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
/// On-chip SRAM (`bus_intmem`, 96 KiB). Its first 16 bytes are the SMP mailbox the boot ROM
/// polls (`rockchip,rk3066-smp-sram`).
pub const SRAM_BASE: u64 = 0xff70_0000;
pub const SRAM_SIZE: usize = 0x1_8000;
pub const SMP_MAILBOX: u64 = SRAM_BASE;
/// Written to SMP_MAILBOX + 4 by `rockchip_boot_secondary` (the entry is at + 8).
pub const BOOT_MAGIC: u32 = 0xdead_beaf;
/// RK3288 peripheral window (APB/AHB devices, PMU SRAM, GIC, boot ROM). Addresses no model
/// claims behave as plain read-back registers so unmodelled drivers probe without external
/// aborts. Every first touch of a 4 KiB page is recorded in `Soc::stub_pages`.
const DEVICE_WINDOW: (u64, u64) = (0xff00_0000, 0x1_0000_0000);

/// Physical devices shared by the cores.
pub struct Soc {
    pub ram: FlatRam,
    pub sram: FlatRam,
    pub uart: Uart,
    pub gic: Gic,
    pub cru: Cru,
    pub grf: Grf,
    pub pmu: Pmu,
    /// SD/eMMC/SDIO hosts, I²C controllers and GPIO banks (`devices.rs`).
    pub periph: Peripherals,
    /// VOP-big and VOP-little.
    pub vop: [Vop; 2],
    pub dsi: Dsi,
    /// RAM pages holding compiled code (shared by the cores' JITs): devices that write RAM
    /// report it here.
    pub code_watch: Arc<CodeWatch>,
    /// RGA 2D accelerator (rotates the vendor application's frames onto the panel).
    pub rga: Rga,
    /// Mali-T760 GPU (`midgard`).
    pub gpu: midgard::MaliT760,
    /// How the panel is mounted, in degrees counter-clockwise (the panel node's `rotation`):
    /// the picture is turned the same way for screenshots and VNC, as the user sees it.
    /// 0, 90, 180 or 270; other values show the picture unturned.
    pub display_rotation: u32,
    /// Desktop name for VNC viewers.
    pub display_name: &'static str,
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
            sram: FlatRam::new(SRAM_BASE, SRAM_SIZE),
            uart: Uart::default(),
            gic: Gic::default(),
            cru: Cru::new(),
            grf: Grf::default(),
            pmu: Pmu::default(),
            periph: Peripherals::default(),
            vop: Default::default(),
            dsi: Dsi::default(),
            code_watch: CodeWatch::new(RAM_BASE as u32, ram_size as u32),
            rga: Rga::default(),
            gpu: midgard::MaliT760::new(std::env::var("SBC_MALI_LOG").is_ok_and(|v| v != "0")),
            display_rotation: 0,
            display_name: "sbc-emu: RK3288",
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

    fn has_sram(addr: u64, width: u64) -> bool {
        Self::in_region(addr, width, SRAM_BASE, SRAM_SIZE as u64)
    }

    /// The running VOP (VOP-little first: it drives the panel board's panel).
    fn active_vop(&self) -> Option<&Vop> {
        [&self.vop[1], &self.vop[0]].into_iter().find(|v| v.shown_size().is_some())
    }

    /// What the display shows, as the user sees it (after the panel's mounting rotation).
    pub fn shown_size(&self) -> Option<(u32, u32)> {
        let (w, h) = self.active_vop()?.shown_size()?;
        Some(if self.display_rotation % 180 == 90 { (h, w) } else { (w, h) })
    }

    /// The display as RGBA, turned by the panel's mounting rotation.
    pub fn render(&self, out: &mut Vec<u8>) -> Option<(u32, u32)> {
        self.active_vop()?.render(&self.ram.mem, self.ram.base, self.display_rotation, out)
    }

    /// A word of on-chip SRAM (the boot ROM's view of the SMP mailbox).
    pub fn sram_word(&mut self, addr: u64) -> u32 {
        self.sram.read32(addr).unwrap_or(0)
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
        for (k, &base) in VOP_BASES.iter().enumerate() {
            if let Some(offset) = Self::word_offset(addr, width, base, VOP_SIZE)? {
                return Ok(self.vop[k].read(offset));
            }
        }
        if let Some(offset) = Self::word_offset(addr, width, DSI0_BASE, DSI_SIZE)? {
            return Ok(self.dsi.read(offset));
        }
        if let Some(offset) = Self::word_offset(addr, width, GPU_BASE, midgard::SIZE)? {
            return Ok(self.gpu.read(offset));
        }
        if let Some(offset) = Self::word_offset(addr, width, RGA_BASE, RGA_SIZE)? {
            return Ok(self.rga.read(offset));
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
        if let Some(result) = self.periph.read(addr, width, ticks_to_ns(self.now)) {
            return result;
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
        for (k, &base) in VOP_BASES.iter().enumerate() {
            if let Some(offset) = Self::word_offset(addr, width, base, VOP_SIZE)? {
                self.vop[k].write(offset, value, self.now);
                return Ok(());
            }
        }
        if let Some(offset) = Self::word_offset(addr, width, DSI0_BASE, DSI_SIZE)? {
            self.dsi.write(offset, value);
            return Ok(());
        }
        if let Some(offset) = Self::word_offset(addr, width, GPU_BASE, midgard::SIZE)? {
            self.gpu.write(offset, value, &mut self.ram.mem, self.ram.base);
            for page in self.gpu.written_pages.drain(..) {
                self.code_watch.dma_write(page, 0x1000);
            }
            return Ok(());
        }
        if let Some(offset) = Self::word_offset(addr, width, RGA_BASE, RGA_SIZE)? {
            self.rga.write(offset, value, &mut self.ram.mem, self.ram.base);
            for (pa, len) in self.rga.written.drain(..) {
                self.code_watch.dma_write(pa, len);
            }
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
        if let Some(result) = self.periph.write(addr, width, value, &mut self.ram, ticks_to_ns(self.now)) {
            for (pa, len) in self.periph.dma_written.drain(..) {
                self.code_watch.dma_write(pa, len);
            }
            return result;
        }
        self.stub_access(addr, width, Some(value)).map(|_| ())
    }
}

/// Instructions a core runs before the next core gets its turn.
const QUANTUM: u64 = 1000;

/// SMC without secure firmware: PSCI NOT_SUPPORTED in r0.
const PSCI_NOT_SUPPORTED: u32 = -1i32 as u32;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CoreState {
    /// Power domain off or held in soft reset.
    Off,
    /// Powered and out of reset, in the boot ROM's wait loop: the ROM sleeps in WFE until
    /// the SMP mailbox (SRAM+4) holds `BOOT_MAGIC`, then jumps to the address at SRAM+8.
    /// The host emulates the loop; the core executes no instructions.
    BootRom,
    Run,
    /// In WFI/WFE until an interrupt or event.
    Wait,
}

/// The RK3288: four Cortex-A12 cores on `arm-a32`, GIC-400, UART2, CRU, GRF PLL lock,
/// PMU power domains and the architectural timers.
///
/// Time is the 24 MHz architectural count shared by the cores. Each executed instruction is
/// 1 ns of one core's time; with several cores running they take round-robin quanta and
/// time advances as if they ran in parallel (cores asleep in WFI/WFE do not count).
/// `step` (core 0 only) fast-forwards idle time to the next timer event; `run` returns
/// `Stop::Idle` so a host loop can pace idle time against the wall clock.
///
/// Secondary cores start as on real hardware under mainline Linux (`rockchip,rk3066-smp`,
/// no PSCI): the kernel powers a core up in the PMU (PWRDN_CON), releases its CRU soft
/// reset, writes the entry point to SRAM+8 and `BOOT_MAGIC` to SRAM+4, and executes SEV.
/// The boot ROM part (wait for the magic, jump to the entry in ARM state, SVC mode, MMU
/// off) is emulated by the host.
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
    /// Compile hot code (the A32 JIT to AArch64) on hosts that can run it.
    jit: bool,
}

impl Machine {
    pub fn new() -> Self {
        Self::with_ram_size(RAM_SIZE)
    }

    pub fn with_ram_size(ram_size: usize) -> Self {
        let cpus = (0..NCPU)
            .map(|index| {
                let mut cpu = Self::new_cpu(index);
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
            jit: arm_a32::jit::available() && std::env::var("SBC_JIT").map_or(true, |v| v != "0"),
        };
        for k in 0..NCPU {
            machine.attach(k);
        }
        machine.update_irqs();
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

    fn new_cpu(index: usize) -> Cpu {
        let mut cpu = Cpu::new();
        cpu.set_model(CORTEX_A12);
        cpu.mpidr = RK3288_MPIDR | index as u32;
        cpu
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

    /// Reset state of a core entering the kernel or leaving the boot ROM: SVC/A32, MMU and
    /// caches off, IRQ/FIQ masked.
    fn reset_core(&mut self, index: usize, entry: u32) {
        let now = self.cpus[0].cntpct.max(self.cpus[index].cntpct);
        let instret = self.cpus[index].instret;
        let mut cpu = Self::new_cpu(index);
        cpu.instret = instret;
        // Bit 0 of an entry point selects Thumb state.
        cpu.set_entry(entry);
        cpu.sctlr &= !(1 | 1 << 2 | 1 << 12);
        cpu.undef_to_guest = true;
        cpu.firmware_calls_to_host = true;
        cpu.cntpct = now;
        self.cpus[index] = cpu;
        self.attach(index);
        self.state[index] = CoreState::Run;
        self.soc.gic.reset_cpu_interface(index);
    }

    /// ARM32 Linux zImage entry on core 0: r0 = 0, r1 = ~0 (device-tree boot) and r2 = the
    /// DTB physical address. Cores 1-3 stay off until Linux powers them up.
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

    /// One instruction of one core: 1 ns divided among the running cores. Cores asleep in
    /// WFI/WFE (or in the boot ROM) do not count, so a lone busy core runs at 1 ns per
    /// instruction.
    fn tick(&mut self, n: u64) {
        let running = self.state.iter().filter(|&&s| s == CoreState::Run).count().max(1) as u64;
        self.advance_subtick(n * (12 / running));
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

    /// Instructions a core may run before the next device deadline (compiled blocks chain
    /// until then).
    fn instructions_to_deadline(&self) -> u64 {
        let running = self.state.iter().filter(|&&s| s == CoreState::Run).count().max(1) as u64;
        let now = self.cpus[0].cntpct;
        if self.next_event <= now {
            return 1;
        }
        let units = (self.next_event - now).saturating_mul(500).saturating_sub(self.subtick);
        units.div_ceil(12 / running).max(1)
    }

    /// Inner-shareable TLB maintenance on core `k` reaches every core (DVM broadcast);
    /// see the H3 machine.
    fn broadcast_tlb_flush(&mut self, k: usize) {
        self.cpus[k].tlb_broadcast = false;
        for (j, cpu) in self.cpus.iter_mut().enumerate() {
            if j != k {
                cpu.tlb_flush();
            }
        }
    }

    fn timer_state_of(cpu: &Cpu) -> (u32, u64, u32, u64) {
        (cpu.cntp_ctl, cpu.cntp_cval, cpu.cntv_ctl, cpu.cntv_cval)
    }

    /// Follow the PMU core power domains and the CRU core resets: a core whose domain is
    /// off or whose reset is asserted stops; one that is powered and released enters the
    /// boot ROM. Core 0 is the boot core and is not switched (Linux never powers it down).
    fn sync_power(&mut self) {
        for k in 1..NCPU {
            let up = self.soc.pmu.core_powered(k) && !self.soc.cru.core_in_reset(k);
            match self.state[k] {
                CoreState::Off if up => {
                    self.state[k] = CoreState::BootRom;
                    self.boot_rom_check(k);
                }
                CoreState::Off => {}
                _ if !up => {
                    self.state[k] = CoreState::Off;
                    self.cpus[k].set_irq(false, false);
                }
                _ => {}
            }
        }
    }

    /// The boot ROM's mailbox check on core `k` (run when it enters the ROM and on every
    /// event): with `BOOT_MAGIC` at SRAM+4 it jumps to the address at SRAM+8.
    fn boot_rom_check(&mut self, k: usize) {
        if self.soc.sram_word(SMP_MAILBOX + 4) == BOOT_MAGIC {
            let entry = self.soc.sram_word(SMP_MAILBOX + 8);
            self.reset_core(k, entry);
        }
    }

    fn update_irqs(&mut self) {
        self.sync_power();
        let now = self.cpus[0].cntpct;
        self.soc.now = now;
        self.soc.dirty = false;
        let uart = self.soc.uart.irq();
        self.soc.gic.set_level(UART2_IRQ, uart);
        let Soc { periph, gic, .. } = &mut self.soc;
        for (irq, level) in periph.irq_lines() {
            gic.set_level(irq, level);
        }
        let mut next = u64::MAX;
        for k in 0..2 {
            self.soc.vop[k].update(now);
            let irq = self.soc.vop[k].irq();
            self.soc.gic.set_level(VOP_IRQS[k], irq);
            if let Some(due) = self.soc.vop[k].next_deadline() {
                next = next.min(due);
            }
        }
        {
            let Soc { gpu, ram, gic, rga, code_watch, .. } = &mut self.soc;
            gic.set_level(RGA_IRQ, rga.irq());
            gpu.update(now, &mut ram.mem, ram.base);
            for page in gpu.written_pages.drain(..) {
                code_watch.dma_write(page, 0x1000);
            }
            for (irq, line) in GPU_IRQS {
                gic.set_level(line, gpu.irq(irq));
            }
            if let Some(due) = gpu.next_deadline() {
                next = next.min(due);
            }
        }
        for k in 0..NCPU {
            let cpu = &self.cpus[k];
            self.timer_state[k] = Self::timer_state_of(cpu);
            let physical = cpu.physical_timer_irq();
            let virtual_ = cpu.virtual_timer_irq();
            if matches!(self.state[k], CoreState::Run | CoreState::Wait) {
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
            if !matches!(self.state[k], CoreState::Run | CoreState::Wait) {
                continue;
            }
            let irq = self.soc.gic.irq_out_for(k);
            self.cpus[k].set_irq(irq, false);
            if irq && self.state[k] == CoreState::Wait {
                self.state[k] = CoreState::Run;
            }
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
        self.cpus[0].chain_until = i0 + 1;
        let result = self.cpus[0].step(&mut self.soc);
        // step() runs core 0 alone, so each instruction is a full 1 ns.
        self.advance_subtick(12 * (self.cpus[0].instret - i0).max(1));
        if self.cpus[0].sent_event {
            self.send_event(0);
        }
        if self.cpus[0].tlb_broadcast {
            self.broadcast_tlb_flush(0);
        }
        match result {
            Err(Trap::Wait) => {
                self.state[0] = CoreState::Wait;
                self.update_irqs();
            }
            Err(Trap::HostCall(_)) => self.cpus[0].r[0] = PSCI_NOT_SUPPORTED,
            _ => {}
        }
        result
    }

    /// Run up to `budget` instructions across the cores. Returns `Stop::Idle(deadline_ns)`
    /// when every running core waits for an interrupt and nothing is due before then.
    pub fn run(&mut self, budget: u64) -> Stop {
        let mut left = budget;
        // Exception-storm detection per core: one core's progress must not hide another's
        // abort loop.
        let mut storm = [0u32; NCPU];
        while left > 0 {
            self.sync_irqs();
            if self.soc.cru.reset_requested {
                self.soc.cru.reset_requested = false;
                return Stop::Reset;
            }
            if self.soc.periph.power_off_requested() {
                return Stop::PowerOff;
            }
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
            let mut quantum = left.min(QUANTUM);
            while quantum > 0 {
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
                    Err(Trap::Exception(..)) => {
                        self.exceptions += 1;
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
                    // No secure firmware on the RK3288: SMC returns PSCI NOT_SUPPORTED.
                    Err(Trap::HostCall(_)) => self.cpus[k].r[0] = PSCI_NOT_SUPPORTED,
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
                // A device access (an IPI, a core power or reset change, a global reset), a
                // timer deadline or this core's own timer registers may change the machine.
                if self.soc.dirty
                    || self.cpus[0].cntpct >= self.next_event
                    || Self::timer_state_of(&self.cpus[k]) != self.timer_state[k]
                {
                    self.update_irqs();
                    if self.soc.cru.reset_requested || self.state[k] != CoreState::Run {
                        break;
                    }
                }
            }
        }
        Stop::Budget
    }

    /// SEV from core `k`: every other core's event register is set; cores sleeping in WFE
    /// wake (consuming the event), and cores in the boot ROM check the mailbox.
    fn send_event(&mut self, k: usize) {
        self.cpus[k].sent_event = false;
        for j in 0..NCPU {
            if j == k {
                continue;
            }
            match self.state[j] {
                CoreState::Off => {}
                CoreState::BootRom => self.boot_rom_check(j),
                CoreState::Wait if self.cpus[j].last_wfe => self.state[j] = CoreState::Run,
                CoreState::Run | CoreState::Wait => self.cpus[j].event = true,
            }
        }
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
            } else if Self::has_sram(addr, $width) {
                self.sram.$name(addr)
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
            } else if Self::has_sram(addr, $width) {
                self.sram.$name(addr, value)
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

    /// Doubleword device accesses (LDRD/STRD) are two word accesses.
    fn read64(&mut self, addr: u64) -> Result<u64, Fault> {
        if self.has_ram(addr, 8) {
            self.ram.read64(addr)
        } else if Self::has_sram(addr, 8) {
            self.sram.read64(addr)
        } else {
            let low = self.mmio_read(addr, 4)?;
            let high = self.mmio_read(addr + 4, 4)?;
            Ok(u64::from(low) | u64::from(high) << 32)
        }
    }

    fn write64(&mut self, addr: u64, value: u64) -> Result<(), Fault> {
        if self.has_ram(addr, 8) {
            self.ram.write64(addr, value)
        } else if Self::has_sram(addr, 8) {
            self.sram.write64(addr, value)
        } else {
            self.mmio_write(addr, 4, value as u32)?;
            self.mmio_write(addr + 4, 4, (value >> 32) as u32)
        }
    }
}
