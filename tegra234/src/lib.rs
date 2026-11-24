//! Tegra234 direct-boot machine for early Linux bring-up.
pub mod boot;
mod bpmp;
pub mod gpu;
mod hsp;
mod mc;
mod uart;

use arm_a64::{Cpu, CpuProfile};
use emu_core::{Bus, Core, Fault, Stop, Trap};
use peripherals::{gicv3::Gic, pl011::Pl011};
use uart::UartA;

pub const RAM_BASE: u64 = 0x8000_0000;
pub const DISPLAY_SIZE: (u32, u32) = (640, 360);
const FRAMEBUFFER: u64 = 0x8ff0_0000;
const FRAMEBUFFER_RESERVATION: u64 = 1 << 20;
const APB_MISC: u64 = 0x0010_0000;
const APB_STRAPS: u64 = 0x0010_f000;
const CHIP_ID_A01: u32 = 0x0001_2310;
const GICD: u64 = 0x0f40_0000;
const GICD_SIZE: u64 = 0x1_0000;
const GICR: u64 = 0x0f44_0000;
const GICR_SIZE: u64 = 0x20_0000;
const UART: u64 = 0x031d_0000;
const UART_SIZE: u64 = 0x1_0000;
const UART_SPI: u32 = 285;
const UARTA: u64 = 0x0310_0000;
const UARTA_SPI: u32 = 112;
const COUNTER_HZ: u64 = 31_250_000;

#[derive(Default)]
struct Firmware {
    hsp: hsp::Hsp,
    bpmp: bpmp::Bpmp,
}

struct Soc {
    ram: Vec<u8>,
    gic: Gic,
    uart: Pl011,
    uarta: UartA,
    gpu: gpu::Gpu,
    gpu_node: bool,
    firmware: Option<Firmware>,
    now: u64,
    interrupts_changed: bool,
}

impl Soc {
    fn ram_offset(&self, addr: u64, size: usize) -> Option<usize> {
        let offset = usize::try_from(addr.checked_sub(RAM_BASE)?).ok()?;
        (offset.checked_add(size)? <= self.ram.len()).then_some(offset)
    }

    fn device_read(&mut self, addr: u64) -> Result<u32, Fault> {
        Ok(if (GICD..GICD + GICD_SIZE).contains(&addr) {
            self.gic.dist_read(addr - GICD)
        } else if (GICR..GICR + GICR_SIZE).contains(&addr) {
            self.gic.redist_read(addr - GICR)
        } else if addr == APB_MISC + 4 {
            CHIP_ID_A01
        } else if addr == APB_STRAPS {
            0
        } else if let Some(value) = mc::read(addr) {
            value
        } else if (UART..UART + UART_SIZE).contains(&addr) {
            self.interrupts_changed = true;
            self.uart.read(addr - UART)
        } else if (UARTA..UARTA + UART_SIZE).contains(&addr) {
            self.interrupts_changed = true;
            self.uarta.read(addr - UARTA, self.now)
        } else if addr == uart::FIXTURE_RESET && self.firmware.is_none() {
            self.uarta
                .reset_asserted()
                .map(u32::from)
                .ok_or(Fault::Unmapped)?
        } else if (hsp::BASE..hsp::BASE + hsp::SIZE).contains(&addr) {
            self.firmware
                .as_ref()
                .ok_or(Fault::Unmapped)?
                .hsp
                .read(addr - hsp::BASE)
        } else if self.gpu_bar0_accessible(addr) {
            self.gpu.read(addr - gpu::BAR0, self.now)
        } else {
            return Err(Fault::Unmapped);
        })
    }

    fn device_write(&mut self, addr: u64, value: u32) -> Result<(), Fault> {
        if (GICD..GICD + GICD_SIZE).contains(&addr) {
            self.gic.dist_write(addr - GICD, value);
        } else if (GICR..GICR + GICR_SIZE).contains(&addr) {
            self.gic.redist_write(addr - GICR, value);
        } else if (UART..UART + UART_SIZE).contains(&addr) {
            self.uart.write(addr - UART, value);
        } else if (UARTA..UARTA + UART_SIZE).contains(&addr) {
            self.uarta.write(addr - UARTA, value, self.now);
        } else if addr == uart::FIXTURE_RESET && self.uarta.high_speed() && self.firmware.is_none()
        {
            self.uarta.set_reset(value & 1 != 0);
        } else if (hsp::BASE..hsp::BASE + hsp::SIZE).contains(&addr) {
            let firmware = self.firmware.as_mut().ok_or(Fault::Unmapped)?;
            if firmware.hsp.write(addr - hsp::BASE, value)
                && firmware.bpmp.service(&mut self.uarta, &mut self.gpu)
            {
                firmware.hsp.ring_ccplex();
            }
        } else if self.gpu_bar0_accessible(addr) {
            self.gpu.write(addr - gpu::BAR0, value, &mut self.ram);
        } else {
            return Err(Fault::Unmapped);
        }
        self.interrupts_changed = true;
        Ok(())
    }

    fn gpu_bar0_accessible(&self, addr: u64) -> bool {
        (gpu::BAR0..gpu::BAR0 + gpu::SIZE).contains(&addr)
            && self
                .firmware
                .as_ref()
                .is_none_or(|firmware| firmware.bpmp.gpu_available())
    }

    fn sram_offset(addr: u64, size: usize) -> Option<usize> {
        let offset = addr.checked_sub(bpmp::SRAM)?;
        (offset.checked_add(size as u64)? <= bpmp::SRAM_SIZE).then_some(offset as usize)
    }

    fn read<const N: usize>(&mut self, addr: u64) -> Result<[u8; N], Fault> {
        let mut bytes = [0; N];
        if let Some(offset) = self.ram_offset(addr, N) {
            bytes.copy_from_slice(&self.ram[offset..offset + N]);
        } else if let Some(offset) = Self::sram_offset(addr, N) {
            let firmware = self.firmware.as_ref().ok_or(Fault::Unmapped)?;
            bytes.copy_from_slice(&firmware.bpmp.sram[offset..offset + N]);
        } else if N == 8 && addr.is_multiple_of(8) {
            bytes[..4].copy_from_slice(&self.device_read(addr)?.to_le_bytes());
            bytes[4..].copy_from_slice(&self.device_read(addr + 4)?.to_le_bytes());
        } else if N <= 4 && (addr as usize & 3) + N <= 4 {
            let word = self.device_read(addr & !3)?.to_le_bytes();
            let offset = addr as usize & 3;
            bytes.copy_from_slice(&word[offset..offset + N]);
        } else {
            return Err(Fault::Unmapped);
        }
        Ok(bytes)
    }

    fn write<const N: usize>(&mut self, addr: u64, bytes: [u8; N]) -> Result<(), Fault> {
        if let Some(offset) = self.ram_offset(addr, N) {
            self.ram[offset..offset + N].copy_from_slice(&bytes);
        } else if let Some(offset) = Self::sram_offset(addr, N) {
            let firmware = self.firmware.as_mut().ok_or(Fault::Unmapped)?;
            firmware.bpmp.sram[offset..offset + N].copy_from_slice(&bytes);
        } else if N == 8 && addr.is_multiple_of(8) {
            self.device_write(addr, u32::from_le_bytes(bytes[..4].try_into().unwrap()))?;
            self.device_write(addr + 4, u32::from_le_bytes(bytes[4..].try_into().unwrap()))?;
        } else if N <= 4 && (addr as usize & 3) + N <= 4 {
            let aligned = addr & !3;
            let offset = addr as usize & 3;
            let mut word = if N == 4
                || (UART..UART + UART_SIZE).contains(&addr)
                || (UARTA..UARTA + UART_SIZE).contains(&addr)
            {
                [0; 4]
            } else {
                // Linux uses byte writes for the GIC priority registers.
                self.device_read(aligned)?.to_le_bytes()
            };
            word[offset..offset + N].copy_from_slice(&bytes);
            self.device_write(aligned, u32::from_le_bytes(word))?;
        } else {
            return Err(Fault::Unmapped);
        }
        Ok(())
    }
}

macro_rules! bus_access {
    ($read:ident, $write:ident, $ty:ty) => {
        fn $read(&mut self, addr: u64) -> Result<$ty, Fault> {
            self.read(addr).map(<$ty>::from_le_bytes)
        }

        fn $write(&mut self, addr: u64, value: $ty) -> Result<(), Fault> {
            self.write(addr, value.to_le_bytes())
        }
    };
}

impl Bus for Soc {
    bus_access!(read8, write8, u8);
    bus_access!(read16, write16, u16);
    bus_access!(read32, write32, u32);
    bus_access!(read64, write64, u64);

    fn read_sysreg(&mut self, key: u32) -> Option<u64> {
        let result = self.gic.sys_read(key);
        self.interrupts_changed |= result.is_some();
        result
    }

    fn write_sysreg(&mut self, key: u32, value: u64) -> bool {
        let handled = self.gic.sys_write(key, value);
        self.interrupts_changed |= handled;
        handled
    }
}

// Observed on an Orin Nano running all six cores. Aff2 selects the cluster.
pub const AFFINITIES: [u64; 6] = [0, 0x100, 0x200, 0x300, 0x10200, 0x10300];
const QUANTUM_NS: u64 = 20_000;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CoreState {
    Off,
    Running,
    Wfi,
    Wfe,
}

pub struct Machine {
    pub cpus: Vec<Cpu>,
    pub state: Vec<CoreState>,
    soc: Soc,
    current: usize,
    now: u64,
    breakpoint: emu_core::debug::Breakpoints,
    exception_streak: Vec<u32>,
    pub interrupts: u64,
    display: bool,
}

impl Machine {
    pub fn new(ram_bytes: usize, cores: usize) -> Result<Self, String> {
        Self::with_uart_mode(ram_bytes, cores, false)
    }

    pub fn with_uart_mode(
        ram_bytes: usize,
        cores: usize,
        high_speed_uart: bool,
    ) -> Result<Self, String> {
        if !(256 << 20..=4096usize << 20).contains(&ram_bytes) {
            return Err("Orin Nano direct boot needs 256–4096 MiB of RAM".into());
        }
        if !(1..=AFFINITIES.len()).contains(&cores) {
            return Err("Orin Nano requires --smp between 1 and 6".into());
        }
        let mut soc = Soc {
            ram: vec![0; ram_bytes],
            gic: Gic::with_affinities(&AFFINITIES[..cores]),
            uart: Pl011::new(),
            uarta: UartA::new(high_speed_uart),
            gpu: gpu::Gpu::default(),
            gpu_node: false,
            firmware: None,
            now: 0,
            interrupts_changed: true,
        };
        let code_pages = Cpu::code_page_map(ram_bytes as u64);
        let cpus = AFFINITIES[..cores]
            .iter()
            .map(|&affinity| {
                let mut cpu = Cpu::new();
                cpu.profile = CpuProfile::CORTEX_A78AE;
                cpu.sys.mpidr = 0x0100_0000 | affinity;
                cpu.sys.cntfrq = COUNTER_HZ;
                cpu.ram = soc.ram.as_mut_ptr();
                cpu.ram_len = ram_bytes as u64;
                cpu.ram_base = RAM_BASE;
                cpu.code_pages = Some(code_pages.clone());
                cpu.firmware_calls_to_host = true;
                cpu
            })
            .collect();
        let mut state = vec![CoreState::Off; cores];
        state[0] = CoreState::Running;
        Ok(Self {
            cpus,
            state,
            soc,
            current: 0,
            now: 0,
            breakpoint: Default::default(),
            exception_streak: vec![0; cores],
            interrupts: 0,
            display: false,
        })
    }

    pub fn enable_bpmp(&mut self) {
        self.soc.firmware.get_or_insert_with(Firmware::default);
    }

    pub fn enable_gpu(&mut self) {
        self.enable_bpmp();
        self.soc.gpu_node = true;
    }

    pub fn enable_display(&mut self) {
        self.display = true;
    }

    pub fn display_size(&self) -> Option<(u32, u32)> {
        self.display.then_some(DISPLAY_SIZE)
    }

    pub fn render(&self, out: &mut Vec<u8>) -> Option<(u32, u32)> {
        let (width, height) = self.display_size()?;
        let start = (FRAMEBUFFER - RAM_BASE) as usize;
        let size = (width * height * 4) as usize;
        out.resize(size, 0);
        for (output, input) in out
            .chunks_exact_mut(4)
            .zip(self.soc.ram[start..start + size].chunks_exact(4))
        {
            output.copy_from_slice(&[input[2], input[1], input[0], 255]);
        }
        Some((width, height))
    }

    pub fn gpu_submissions(&self) -> u64 {
        self.soc.gpu.submissions()
    }

    pub fn set_jit(&mut self, enabled: bool) -> bool {
        for cpu in &mut self.cpus {
            cpu.jit_off = !enabled;
        }
        enabled && self.cpus[self.current].enable_jit()
    }

    pub fn current_core(&self) -> usize {
        self.current
    }

    pub fn now(&self) -> u64 {
        self.now
    }

    pub fn instret(&self) -> u64 {
        self.cpus.iter().map(|cpu| cpu.instret).sum()
    }

    pub fn serial_input(&mut self, byte: u8) -> bool {
        if !self.soc.uart.rx_room() {
            return false;
        }
        self.soc.uart.push_rx(byte);
        self.soc.interrupts_changed = true;
        true
    }

    pub fn serial_output(&self) -> &[u8] {
        &self.soc.uart.out
    }

    pub fn take_serial_output(&mut self) -> Vec<u8> {
        std::mem::take(&mut self.soc.uart.out)
    }

    /// Deliver a byte to the rover's UART, retaining it at the peer on backpressure.
    pub fn rover_uart_input(&mut self, byte: u8) -> bool {
        let accepted = self.soc.uarta.push_rx(byte, self.now);
        self.soc.interrupts_changed |= accepted;
        accepted
    }

    pub fn take_rover_uart_output(&mut self) -> Vec<u8> {
        self.soc.uarta.take_output()
    }

    fn next_event(&self) -> Option<u64> {
        self.cpus
            .iter()
            .zip(&self.state)
            .filter(|(_, state)| **state != CoreState::Off)
            .flat_map(|(cpu, _)| {
                let sys = &cpu.sys;
                [(sys.ptimer, 0), (sys.vtimer, sys.cntvoff)]
            })
            .filter(|(timer, _)| timer.ctl & 3 == 1)
            // 31.25 MHz is one count per 32 ns.
            .map(|(timer, offset)| timer.cval.wrapping_add(offset).saturating_mul(32))
            .filter(|&deadline| deadline > self.now)
            .chain(self.soc.uarta.next_event(self.now))
            .min()
    }

    fn update_interrupts(&mut self) {
        if let Some(firmware) = &self.soc.firmware {
            self.soc
                .gic
                .set_line((hsp::SPI + 32) as usize, firmware.hsp.irq());
        }
        self.soc
            .gic
            .set_line((UART_SPI + 32) as usize, self.soc.uart.irq());
        self.soc
            .gic
            .set_line((UARTA_SPI + 32) as usize, self.soc.uarta.irq(self.now));
        for (core, (cpu, state)) in self.cpus.iter_mut().zip(&mut self.state).enumerate() {
            if *state == CoreState::Off {
                continue;
            }
            Self::catch_up(cpu, self.now);
            cpu.refresh_counter();
            let sys = &mut cpu.sys;
            sys.timers_changed = false;
            self.soc.gic.set_ppi(core, 30, sys.ptimer.firing(sys.cnt));
            self.soc.gic.set_ppi(
                core,
                27,
                sys.vtimer.firing(sys.cnt.wrapping_sub(sys.cntvoff)),
            );
            let irq = self.soc.gic.irq_out(core);
            cpu.set_irq(irq, false);
            if irq {
                *state = CoreState::Running;
            }
        }
        self.soc.interrupts_changed = false;
    }

    fn catch_up(cpu: &mut Cpu, now: u64) {
        if now > cpu.now_ns() {
            cpu.sys.time_base_ns = now;
            cpu.sys.time_base_instret = cpu.instret;
        }
    }

    pub fn idle_until(&mut self, deadline: u64) {
        self.now = self.now.max(deadline);
        self.update_interrupts();
    }

    fn switch_to(&mut self, core: usize) {
        if self.current != core {
            let previous = self.current;
            let [old, next] = self.cpus.get_disjoint_mut([previous, core]).unwrap();
            old.hand_jit_to(next);
            old.clear_exclusive();
            self.current = core;
            self.soc.gic.cur = core;
        }
        Self::catch_up(&mut self.cpus[core], self.now);
    }

    pub fn run(&mut self, budget: u64) -> Stop {
        let mut remaining = budget;
        let mut attempts = budget;
        let debugging = !self.breakpoint.at.is_empty() || !self.cpus[0].watch.is_empty();
        while remaining != 0 && attempts != 0 {
            self.update_interrupts();
            // WFE is also allowed to return spuriously. Giving a parked core a
            // new turn lets it recheck the condition after another core runs.
            for state in &mut self.state {
                if *state == CoreState::Wfe {
                    *state = CoreState::Running;
                }
            }
            let count = self.cpus.len();
            let next = (1..=count)
                .map(|offset| (self.current + offset) % count)
                .find(|&core| {
                    self.state[core] == CoreState::Running && self.breakpoint.may_run(core)
                });
            let Some(core) = next else {
                let deadline = self
                    .next_event()
                    .unwrap_or(self.now.saturating_add(10_000_000));
                return Stop::Idle(deadline);
            };
            self.switch_to(core);
            let slice_end = self.now.saturating_add(QUANTUM_NS);
            let mut deadline = self.next_event().unwrap_or(u64::MAX);
            while self.state[core] == CoreState::Running
                && self.now < slice_end
                && remaining != 0
                && attempts != 0
            {
                if self.soc.interrupts_changed
                    || self.cpus[core].sys.timers_changed
                    || self.now >= deadline
                {
                    self.update_interrupts();
                    deadline = self.next_event().unwrap_or(u64::MAX);
                }
                let cpu = &mut self.cpus[core];
                if self.breakpoint.hit(core, cpu.pc) {
                    return Stop::Breakpoint(core);
                }
                let before = cpu.instret;
                let max_instructions = deadline.min(slice_end).saturating_sub(self.now);
                cpu.chain_until = before.saturating_add(remaining.min(max_instructions));
                attempts -= 1;
                let stop = self.execute(core, debugging);
                remaining = remaining.saturating_sub(self.cpus[core].instret - before);
                if let Some(stop) = stop {
                    return stop;
                }
            }
        }
        Stop::Budget
    }

    fn execute(&mut self, core: usize, interpreting: bool) -> Option<Stop> {
        let jit_off = self.cpus[core].jit_off;
        self.cpus[core].jit_off |= interpreting;
        self.soc.now = self.now;
        let result = self.cpus[core].step(&mut self.soc);
        let watch_hit = self.cpus[core].watch_hit.take();
        self.cpus[core].jit_off = jit_off;
        self.now = self.now.max(self.cpus[core].now_ns());
        if let Some(op) = self.cpus[core].tlbi_bcast.take() {
            for (index, cpu) in self.cpus.iter_mut().enumerate() {
                if index != core {
                    cpu.tlb_invalidate(op);
                }
            }
        }
        if let Some(op) = self.cpus[core].ic_bcast.take() {
            for (index, cpu) in self.cpus.iter_mut().enumerate() {
                if index != core {
                    cpu.jit_invalidate(op);
                }
            }
        }
        match result {
            Ok(()) => {
                self.exception_streak[core] = 0;
                if let Some((addr, kind)) = watch_hit {
                    return Some(Stop::Watchpoint(core, addr, kind));
                }
            }
            Err(Trap::Interrupt(..)) => {
                self.interrupts += 1;
                self.soc.interrupts_changed = true;
            }
            Err(Trap::Exception(..)) => {
                self.exception_streak[core] += 1;
                if self.exception_streak[core] >= 1_000_000 {
                    self.exception_streak[core] = 0;
                    let el = self.cpus[core].el as usize;
                    return Some(Stop::ExceptionStorm {
                        pc: self.cpus[core].pc,
                        esr: self.cpus[core].esr[el],
                        elr: self.cpus[core].elr[el],
                        far: self.cpus[core].far[el],
                        el: self.cpus[core].el,
                    });
                }
            }
            Err(Trap::Wait) if !self.cpus[core].irq => {
                self.state[core] = if self.cpus[core].last_wfe {
                    CoreState::Wfe
                } else {
                    CoreState::Wfi
                };
            }
            Err(Trap::Wait) => {}
            Err(Trap::HostCall(_)) => return self.psci(core),
            Err(Trap::Unimplemented(pc, word)) => return Some(Stop::Unimplemented(pc, word)),
            Err(Trap::Hlt(code)) => return Some(Stop::Hlt(code)),
            Err(Trap::Brk(code)) => return Some(Stop::Brk(code)),
        }
        None
    }

    fn psci(&mut self, core: usize) -> Option<Stop> {
        const NOT_SUPPORTED: u64 = u64::MAX;
        const INVALID_PARAMETERS: u64 = -2i64 as u64;
        const ALREADY_ON: u64 = -4i64 as u64;
        fn supported(id: u32) -> bool {
            matches!(
                id,
                0x8000_0000
                    | 0x8400_0000
                    | 0x8400_0002
                    | 0x8400_0003
                    | 0xc400_0003
                    | 0x8400_0004
                    | 0xc400_0004
                    | 0x8400_0008
                    | 0x8400_0009
                    | 0x8400_000a
            )
        }
        let [id, arg, entry, context, ..] = self.cpus[core].x;
        let affinity = arg & 0xff_00ff_ffff;
        let target = AFFINITIES[..self.cpus.len()]
            .iter()
            .position(|&value| value == affinity);
        self.cpus[core].x[0] = match id as u32 {
            0x8000_0000 | 0x8400_0000 => 0x1_0001, // SMCCC / PSCI 1.1
            0x8400_0002 => {
                self.state[core] = CoreState::Off;
                self.cpus[core].set_irq(false, false);
                self.cpus[core].clear_exclusive();
                return None;
            }
            0x8400_0003 | 0xc400_0003 => match target {
                None => INVALID_PARAMETERS,
                Some(_) if self.soc.ram_offset(entry, 4).is_none() || entry % 4 != 0 => {
                    INVALID_PARAMETERS
                }
                Some(target) if self.state[target] != CoreState::Off => ALREADY_ON,
                Some(target) => {
                    self.start_core(target, entry, context);
                    0
                }
            },
            0x8400_0004 | 0xc400_0004 if entry == 0 => target
                .map_or(INVALID_PARAMETERS, |target| {
                    u64::from(self.state[target] == CoreState::Off)
                }),
            0x8400_0004 | 0xc400_0004 => INVALID_PARAMETERS,
            0x8400_0008 => return Some(Stop::PowerOff),
            0x8400_0009 => return Some(Stop::Reset),
            0x8400_000a if supported(arg as u32) => 0,
            _ => NOT_SUPPORTED,
        };
        None
    }

    fn start_core(&mut self, core: usize, entry: u64, context: u64) {
        let old = &mut self.cpus[core];
        let mut cpu = Cpu::new();
        cpu.profile = old.profile;
        cpu.ram = old.ram;
        cpu.ram_base = old.ram_base;
        cpu.ram_len = old.ram_len;
        cpu.code_pages = old.code_pages.clone();
        cpu.jit_off = old.jit_off;
        cpu.watch = std::mem::take(&mut old.watch);
        cpu.instret = old.instret;
        cpu.sys.mpidr = old.sys.mpidr;
        cpu.sys.cntfrq = COUNTER_HZ;
        cpu.sys.time_base_ns = self.now;
        cpu.sys.time_base_instret = cpu.instret;
        cpu.firmware_calls_to_host = true;
        cpu.el = 1;
        cpu.spsel = true;
        cpu.daif = 0x3c0;
        cpu.x[0] = context;
        cpu.pc = entry;
        *old = cpu;
        self.exception_streak[core] = 0;
        self.state[core] = CoreState::Running;
        self.soc.interrupts_changed = true;
    }

    pub fn debug_breakpoint(&mut self, addr: u64, insert: bool) {
        self.breakpoint.set(addr, insert);
    }

    pub fn debug_watch(&mut self, addr: u64, len: u64, kind: u8, insert: bool) {
        for cpu in &mut self.cpus {
            cpu.watch.retain(|&watch| watch != (addr, len, kind));
            if insert {
                cpu.watch.push((addr, len, kind));
            }
        }
    }

    pub fn debug_step(&mut self, core: usize) -> Option<Stop> {
        if self.state[core] == CoreState::Off {
            return None;
        }
        self.switch_to(core);
        self.state[core] = CoreState::Running;
        self.update_interrupts();
        self.execute(core, true)
    }

    fn debug_pages(&mut self, core: usize, addr: u64, len: usize) -> Option<Vec<(usize, usize)>> {
        let mut pages = Vec::new();
        let mut done = 0;
        while done < len {
            let va = addr.checked_add(done as u64)?;
            let pa = self.cpus[core].debug_translate(&mut self.soc, va)?;
            let size = (len - done).min(0x1000 - (va as usize & 0xfff));
            pages.push((self.soc.ram_offset(pa, size)?, size));
            done += size;
        }
        Some(pages)
    }

    pub fn debug_read(&mut self, core: usize, addr: u64, len: usize) -> Option<Vec<u8>> {
        let mut data = Vec::new();
        for (offset, size) in self.debug_pages(core, addr, len)? {
            data.extend_from_slice(&self.soc.ram[offset..offset + size]);
        }
        Some(data)
    }

    pub fn debug_write(&mut self, core: usize, addr: u64, data: &[u8]) -> bool {
        let Some(pages) = self.debug_pages(core, addr, data.len()) else {
            return false;
        };
        let mut written = 0;
        for (offset, size) in pages {
            self.soc.ram[offset..offset + size].copy_from_slice(&data[written..written + size]);
            written += size;
        }
        for cpu in &mut self.cpus {
            cpu.jit_invalidate(u64::MAX);
        }
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct BpmpClient {
        tx: u64,
        rx: u64,
        trigger: u64,
        pending: u64,
        count: u32,
    }

    impl BpmpClient {
        fn new(soc: &mut Soc, channel: u64) -> Self {
            let dim = soc.read32(hsp::BASE + 0x380).unwrap();
            assert_ne!(dim & 15, 0); // The Linux mailbox core requires a nonempty controller.
            let bank = (1 + (dim & 15) / 2 + ((dim >> 4) & 15) + ((dim >> 8) & 15)) as u64;
            let ccplex = hsp::BASE + bank * 0x10000 + 0x100;
            let bpmp_db = ccplex + 0x200;
            assert_ne!(soc.read32(bpmp_db + 4).unwrap() & (1 << 17), 0);
            soc.write32(ccplex + 4, 1 << hsp::BPMP_MASTER).unwrap();
            let offset = channel * 256;
            let tx = bpmp::SRAM + bpmp::TX_POOL as u64 + offset;
            let rx = bpmp::SRAM + bpmp::RX_POOL as u64 + offset;

            // Publish SYNC, see the remote ACK, then publish ESTABLISHED.
            soc.write32(tx + 4, 1).unwrap();
            soc.write32(bpmp_db, 1).unwrap();
            assert_eq!(soc.read32(rx + 4).unwrap(), 2);
            assert_eq!(soc.read32(ccplex + 12).unwrap(), 1 << hsp::BPMP_MASTER);
            soc.write32(ccplex + 12, 1 << hsp::BPMP_MASTER).unwrap();
            soc.write32(tx + 4, 0).unwrap();
            soc.write32(bpmp_db, 1).unwrap();
            assert_eq!(soc.read32(rx + 4).unwrap(), 0);
            Self {
                tx,
                rx,
                trigger: bpmp_db,
                pending: ccplex + 12,
                count: 0,
            }
        }

        fn call(&mut self, soc: &mut Soc, mrq: u32, words: &[u32], irq: bool) -> i32 {
            soc.write32(self.tx + 128, mrq).unwrap();
            soc.write32(self.tx + 132, 1 | if irq { 2 } else { 0 })
                .unwrap();
            for (index, value) in words.iter().enumerate() {
                soc.write32(self.tx + 136 + 4 * index as u64, *value)
                    .unwrap();
            }

            self.count = self.count.wrapping_add(1);
            soc.write32(self.tx, self.count).unwrap();
            soc.write32(self.trigger, 1).unwrap();
            assert_eq!(soc.read32(self.tx + 64).unwrap(), self.count);
            assert_eq!(soc.read32(self.rx).unwrap(), self.count);
            assert_eq!(
                soc.read32(self.pending).unwrap(),
                if irq { 1 << hsp::BPMP_MASTER } else { 0 }
            );
            soc.write32(self.pending, 1 << hsp::BPMP_MASTER).unwrap();
            let status = soc.read32(self.rx + 128).unwrap() as i32;
            soc.write32(self.rx + 64, self.count).unwrap();
            status
        }
    }

    #[test]
    fn bpmp_routes_atomic_and_threaded_messages_through_hsp_and_ivc() {
        let mut machine = Machine::with_uart_mode(256 << 20, 1, true).unwrap();
        machine.enable_bpmp();
        let mut atomic = BpmpClient::new(&mut machine.soc, 3);

        // Restart the receive queue and check that its doorbell reaches the GIC.
        let receive_state = bpmp::SRAM + bpmp::TX_POOL as u64 + 13 * 256 + 4;
        let intid = hsp::SPI + 32;
        let gic_pending = GICD + 0x200 + u64::from(intid / 32) * 4;
        let gic_mask = 1 << (intid % 32);
        machine.soc.write32(receive_state, 1).unwrap();
        machine.soc.write32(atomic.trigger, 1).unwrap();
        machine.update_interrupts();
        assert_ne!(machine.soc.read32(gic_pending).unwrap() & gic_mask, 0);
        machine
            .soc
            .write32(atomic.pending, 1 << hsp::BPMP_MASTER)
            .unwrap();
        machine.soc.write32(receive_state, 0).unwrap();
        machine.soc.write32(atomic.trigger, 1).unwrap();
        machine.update_interrupts();
        assert_eq!(machine.soc.read32(gic_pending).unwrap() & gic_mask, 0);

        let soc = &mut machine.soc;
        assert_eq!(atomic.call(soc, 0, &[41], false), 0);
        assert_eq!(soc.read32(atomic.rx + 136).unwrap(), 82);
        assert_eq!(atomic.call(soc, 23, &[71], false), 0);
        assert_eq!(soc.read32(atomic.rx + 136).unwrap(), 0);
        assert_eq!(atomic.call(soc, 71, &[], false), 0);
        assert_eq!(soc.read32(atomic.rx + 136).unwrap().to_le_bytes(), *b"sbc-");

        let mut threaded = BpmpClient::new(soc, 0);
        // Program UART A for a 9600-baud clock and verify the same rate is returned.
        assert_eq!(
            threaded.call(soc, 22, &[(2 << 24) | bpmp::CLK_UARTA, 0, 153_600, 0], true),
            0
        );
        assert_eq!(soc.read64(threaded.rx + 136).unwrap(), 153_600);
        assert_eq!(
            threaded.call(soc, 22, &[(1 << 24) | bpmp::CLK_UARTA], true),
            0
        );
        assert_eq!(soc.read64(threaded.rx + 136).unwrap(), 153_600);

        // Check that the firmware reset changes the physical UART block state.
        soc.write32(UARTA + 0x0c, 3).unwrap();
        assert_eq!(atomic.call(soc, 20, &[1, bpmp::RESET_UARTA], false), 0);
        assert_eq!(soc.read32(UARTA + 0x0c).unwrap(), 0);
        assert!(!soc.uarta.push_rx(b'a', 0));
        assert_eq!(atomic.call(soc, 20, &[2, bpmp::RESET_UARTA], false), 0);
        assert!(soc.uarta.push_rx(b'a', 0));
        assert_eq!(soc.read32(UARTA).unwrap(), b'a' as u32);
        assert_eq!(atomic.call(soc, 0xffff, &[], false), -3);

        // The GPU register aperture is available exactly while its domain is on
        // and the firmware has released its reset.
        assert_eq!(soc.read32(gpu::BAR0), Err(Fault::Unmapped));
        assert_eq!(threaded.call(soc, 66, &[1, 35, 1], true), 0);
        assert_eq!(soc.read32(gpu::BAR0), Ok(0x17b0_00a1));
        let ring_command = gpu::BAR0 + 0x12004c;
        let ring_result = gpu::BAR0 + 0x120050;
        assert_eq!(soc.read32(ring_result), Ok(0));
        soc.write32(ring_command, 4).unwrap();
        assert_eq!(soc.read32(ring_command), Ok(0));
        assert_eq!(soc.read32(ring_result), Ok(1));
        assert_eq!(atomic.call(soc, 20, &[1, 19], false), 0);
        assert_eq!(soc.read32(gpu::BAR0), Err(Fault::Unmapped));
        assert_eq!(atomic.call(soc, 20, &[2, 19], false), 0);
        assert_eq!(soc.read32(gpu::BAR0), Ok(0x17b0_00a1));
        assert_eq!(soc.read32(ring_result), Ok(0));
        soc.write32(ring_command, 4).unwrap();
        assert_eq!(atomic.call(soc, 20, &[3, 19], false), 0);
        assert_eq!(soc.read32(ring_result), Ok(0));
        assert_eq!(threaded.call(soc, 66, &[1, 35, 0], true), 0);
        assert_eq!(soc.read32(gpu::BAR0), Err(Fault::Unmapped));
    }

    #[test]
    fn guest_reads_ampere_identity_and_virtual_timer() {
        let mut machine = Machine::new(256 << 20, 1).unwrap();
        for (offset, word) in [
            0xd2a2_e001, // movz x1, #0x1700, lsl #16
            0xb940_0020, // ldr  w0, [x1]
            0xf292_8001, // movk x1, #0x9400
            0xb940_0022, // ldr  w2, [x1]
            0xb940_1023, // ldr  w3, [x1, #16]
            0xd440_0000, // hlt  #0
        ]
        .into_iter()
        .enumerate()
        {
            machine
                .soc
                .write32(RAM_BASE + (offset * 4) as u64, word)
                .unwrap();
        }
        machine.cpus[0].pc = RAM_BASE;
        machine.idle_until(1 << 32);
        assert_eq!(machine.run(12), Stop::Hlt(0));
        assert_eq!(machine.cpus[0].x[0], 0x17b0_00a1);
        assert_eq!(machine.cpus[0].x[2], 3);
        assert_eq!(machine.cpus[0].x[3], 1);
    }
}
