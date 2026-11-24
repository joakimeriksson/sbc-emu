//! Tegra234 direct-boot machine for early Linux bring-up.
pub mod boot;

use arm_a64::{Cpu, CpuProfile};
use emu_core::{Bus, Core, Fault, Stop, Trap};
use peripherals::{gicv3::Gic, pl011::Pl011};

pub const RAM_BASE: u64 = 0x8000_0000;
const GICD: u64 = 0x0f40_0000;
const GICD_SIZE: u64 = 0x1_0000;
const GICR: u64 = 0x0f44_0000;
const GICR_SIZE: u64 = 0x20_0000;
const UART: u64 = 0x031d_0000;
const UART_SIZE: u64 = 0x1_0000;
const UART_SPI: u32 = 285;
const COUNTER_HZ: u64 = 31_250_000;

struct Soc {
    ram: Vec<u8>,
    gic: Gic,
    uart: Pl011,
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
        } else if (UART..UART + UART_SIZE).contains(&addr) {
            self.interrupts_changed = true;
            self.uart.read(addr - UART)
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
        } else {
            return Err(Fault::Unmapped);
        }
        self.interrupts_changed = true;
        Ok(())
    }

    fn read<const N: usize>(&mut self, addr: u64) -> Result<[u8; N], Fault> {
        let mut bytes = [0; N];
        if let Some(offset) = self.ram_offset(addr, N) {
            bytes.copy_from_slice(&self.ram[offset..offset + N]);
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
        } else if N == 8 && addr.is_multiple_of(8) {
            self.device_write(addr, u32::from_le_bytes(bytes[..4].try_into().unwrap()))?;
            self.device_write(addr + 4, u32::from_le_bytes(bytes[4..].try_into().unwrap()))?;
        } else if N <= 4 && (addr as usize & 3) + N <= 4 {
            let aligned = addr & !3;
            let offset = addr as usize & 3;
            let mut word = if N == 4 || (UART..UART + UART_SIZE).contains(&addr) {
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

pub struct Machine {
    pub cpu: Cpu,
    soc: Soc,
    waiting: bool,
    breakpoint: emu_core::debug::Breakpoints,
    exception_streak: u32,
    pub interrupts: u64,
}

impl Machine {
    pub fn new(ram_bytes: usize) -> Result<Self, String> {
        if !(256 << 20..=4096usize << 20).contains(&ram_bytes) {
            return Err("Orin Nano direct boot needs 256–4096 MiB of RAM".into());
        }
        let mut soc = Soc {
            ram: vec![0; ram_bytes],
            gic: Gic::with_affinities(&[0]),
            uart: Pl011::new(),
            interrupts_changed: true,
        };
        let mut cpu = Cpu::new();
        cpu.profile = CpuProfile::CORTEX_A78AE;
        cpu.sys.cntfrq = COUNTER_HZ;
        cpu.ram = soc.ram.as_mut_ptr();
        cpu.ram_len = soc.ram.len() as u64;
        cpu.ram_base = RAM_BASE;
        cpu.firmware_calls_to_host = true;
        Ok(Self {
            cpu,
            soc,
            waiting: false,
            breakpoint: Default::default(),
            exception_streak: 0,
            interrupts: 0,
        })
    }

    pub fn now(&self) -> u64 {
        self.cpu.now_ns()
    }

    pub fn instret(&self) -> u64 {
        self.cpu.instret
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

    fn next_timer(&self) -> Option<u64> {
        let sys = &self.cpu.sys;
        [(sys.ptimer, 0), (sys.vtimer, sys.cntvoff)]
            .into_iter()
            .filter(|(timer, _)| timer.ctl & 3 == 1)
            // 31.25 MHz is one count per 32 ns.
            .map(|(timer, offset)| timer.cval.wrapping_add(offset).saturating_mul(32))
            .filter(|&deadline| deadline > self.now())
            .min()
    }

    fn update_interrupts(&mut self) {
        self.cpu.refresh_counter();
        let sys = &mut self.cpu.sys;
        sys.timers_changed = false;
        self.soc.gic.set_ppi(0, 30, sys.ptimer.firing(sys.cnt));
        self.soc
            .gic
            .set_ppi(0, 27, sys.vtimer.firing(sys.cnt.wrapping_sub(sys.cntvoff)));
        self.soc
            .gic
            .set_line((UART_SPI + 32) as usize, self.soc.uart.irq());
        let irq = self.soc.gic.irq_out(0);
        self.cpu.set_irq(irq, false);
        self.waiting &= !irq;
        self.soc.interrupts_changed = false;
    }

    pub fn idle_until(&mut self, deadline: u64) {
        if deadline > self.now() {
            self.cpu.sys.time_base_ns = deadline;
            self.cpu.sys.time_base_instret = self.cpu.instret;
        }
        self.update_interrupts();
    }

    pub fn run(&mut self, budget: u64) -> Stop {
        let end = self.instret().saturating_add(budget);
        let mut deadline = 0;
        let debugging = !self.breakpoint.at.is_empty() || !self.cpu.watch.is_empty();
        for _ in 0..budget {
            if self.instret() >= end {
                break;
            }
            if self.soc.interrupts_changed || self.cpu.sys.timers_changed || self.now() >= deadline
            {
                self.update_interrupts();
                deadline = self.next_timer().unwrap_or(u64::MAX);
            }
            if self.waiting {
                return Stop::Idle(deadline.min(self.now().saturating_add(10_000_000)));
            }
            if self.breakpoint.hit(0, self.cpu.pc) {
                return Stop::Breakpoint(0);
            }
            self.cpu.chain_until = end.min(
                self.instret()
                    .saturating_add(deadline.saturating_sub(self.now())),
            );
            if let Some(stop) = self.execute(debugging) {
                return stop;
            }
        }
        Stop::Budget
    }

    fn execute(&mut self, interpreting: bool) -> Option<Stop> {
        let jit_off = self.cpu.jit_off;
        self.cpu.jit_off |= interpreting;
        let result = self.cpu.step(&mut self.soc);
        let watch_hit = self.cpu.watch_hit.take();
        self.cpu.jit_off = jit_off;
        match result {
            Ok(()) => {
                self.exception_streak = 0;
                if let Some((addr, kind)) = watch_hit {
                    return Some(Stop::Watchpoint(0, addr, kind));
                }
            }
            Err(Trap::Interrupt(..)) => {
                self.interrupts += 1;
                self.soc.interrupts_changed = true;
            }
            Err(Trap::Exception(..)) => {
                self.exception_streak += 1;
                if self.exception_streak >= 1_000_000 {
                    self.exception_streak = 0;
                    let el = self.cpu.el as usize;
                    return Some(Stop::ExceptionStorm {
                        pc: self.cpu.pc,
                        esr: self.cpu.esr[el],
                        elr: self.cpu.elr[el],
                        far: self.cpu.far[el],
                        el: self.cpu.el,
                    });
                }
            }
            Err(Trap::Wait) => self.waiting = !self.cpu.irq,
            Err(Trap::HostCall(_)) => return self.psci(),
            Err(Trap::Unimplemented(pc, word)) => return Some(Stop::Unimplemented(pc, word)),
            Err(Trap::Hlt(code)) => return Some(Stop::Hlt(code)),
            Err(Trap::Brk(code)) => return Some(Stop::Brk(code)),
        }
        None
    }

    fn psci(&mut self) -> Option<Stop> {
        const NOT_SUPPORTED: u64 = u64::MAX;
        fn supported(id: u32) -> bool {
            matches!(
                id,
                0x8000_0000 | 0x8400_0000 | 0x8400_0008 | 0x8400_0009 | 0x8400_000a
            )
        }
        self.cpu.x[0] = match self.cpu.x[0] as u32 {
            0x8000_0000 | 0x8400_0000 => 0x1_0001, // SMCCC / PSCI 1.1
            0x8400_0008 => return Some(Stop::PowerOff),
            0x8400_0009 => return Some(Stop::Reset),
            0x8400_000a if supported(self.cpu.x[1] as u32) => 0,
            _ => NOT_SUPPORTED,
        };
        None
    }

    pub fn debug_breakpoint(&mut self, addr: u64, insert: bool) {
        self.breakpoint.set(addr, insert);
    }

    pub fn debug_watch(&mut self, addr: u64, len: u64, kind: u8, insert: bool) {
        self.cpu.watch.retain(|&watch| watch != (addr, len, kind));
        if insert {
            self.cpu.watch.push((addr, len, kind));
        }
    }

    pub fn debug_step(&mut self) -> Option<Stop> {
        self.waiting = false;
        self.update_interrupts();
        self.execute(true)
    }

    fn debug_pages(&mut self, addr: u64, len: usize) -> Option<Vec<(usize, usize)>> {
        let mut pages = Vec::new();
        let mut done = 0;
        while done < len {
            let va = addr.checked_add(done as u64)?;
            let pa = self.cpu.debug_translate(&mut self.soc, va)?;
            let size = (len - done).min(0x1000 - (va as usize & 0xfff));
            pages.push((self.soc.ram_offset(pa, size)?, size));
            done += size;
        }
        Some(pages)
    }

    pub fn debug_read(&mut self, addr: u64, len: usize) -> Option<Vec<u8>> {
        let mut data = Vec::new();
        for (offset, size) in self.debug_pages(addr, len)? {
            data.extend_from_slice(&self.soc.ram[offset..offset + size]);
        }
        Some(data)
    }

    pub fn debug_write(&mut self, addr: u64, data: &[u8]) -> bool {
        let Some(pages) = self.debug_pages(addr, data.len()) else {
            return false;
        };
        let mut written = 0;
        for (offset, size) in pages {
            self.soc.ram[offset..offset + size].copy_from_slice(&data[written..written + size]);
            written += size;
        }
        self.cpu.jit_invalidate(u64::MAX);
        true
    }
}
