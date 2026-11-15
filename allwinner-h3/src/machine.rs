use crate::{Gic, Uart};
use arm_a32::Cpu;
use emu_core::{Bus, Core, Fault, FlatRam, Trap};

pub const RAM_BASE: u64 = 0x4000_0000;
pub const RAM_SIZE: usize = 1024 << 20;
pub const UART0_BASE: u64 = 0x01c2_8000;
pub const GICD_BASE: u64 = 0x01c8_1000;
pub const GICC_BASE: u64 = 0x01c8_2000;
pub const PHYSICAL_TIMER_PPI: usize = 30;
const UART_SIZE: u64 = 0x20;
const GICD_SIZE: u64 = 0x1000;
const GICC_SIZE: u64 = 0x2000;

/// Physical devices visible to the initial H3 core.
pub struct Soc {
    pub ram: FlatRam,
    pub uart: Uart,
    pub gic: Gic,
}

impl Soc {
    pub fn new(ram_size: usize) -> Self {
        Self {
            ram: FlatRam::new(RAM_BASE, ram_size),
            uart: Uart::default(),
            gic: Gic::default(),
        }
    }

    fn has_ram(&self, addr: u64, width: u64) -> bool {
        Self::in_region(addr, width, self.ram.base, self.ram.mem.len() as u64)
    }

    fn in_region(addr: u64, width: u64, base: u64, size: u64) -> bool {
        addr.checked_sub(base)
            .is_some_and(|offset| offset.checked_add(width).is_some_and(|end| end <= size))
    }

    fn uart_offset(addr: u64, width: u64) -> Result<u64, Fault> {
        let offset = addr.checked_sub(UART0_BASE).ok_or(Fault::Unmapped)?;
        let end = offset.checked_add(width).ok_or(Fault::Unmapped)?;
        if offset & 3 != 0 || end > UART_SIZE {
            Err(Fault::Unmapped)
        } else {
            Ok(offset)
        }
    }

    fn mmio_read(&mut self, addr: u64, width: u64) -> Result<u32, Fault> {
        if let Ok(offset) = Self::uart_offset(addr, width) {
            if width > 4 {
                return Err(Fault::Prohibited);
            }
            return Ok(self.uart.read(offset));
        }
        let distributor = Self::in_region(addr, width, GICD_BASE, GICD_SIZE);
        let interface = Self::in_region(addr, width, GICC_BASE, GICC_SIZE);
        if distributor || interface {
            if width != 4 || addr & 3 != 0 {
                return Err(Fault::Prohibited);
            }
            return Ok(if distributor {
                self.gic.dist_read(addr - GICD_BASE)
            } else {
                self.gic.cpu_read(addr - GICC_BASE)
            });
        }
        Err(Fault::Unmapped)
    }

    fn mmio_write(&mut self, addr: u64, width: u64, value: u32) -> Result<(), Fault> {
        if let Ok(offset) = Self::uart_offset(addr, width) {
            if width > 4 {
                return Err(Fault::Prohibited);
            }
            self.uart.write(offset, value);
            return Ok(());
        }
        let distributor = Self::in_region(addr, width, GICD_BASE, GICD_SIZE);
        let interface = Self::in_region(addr, width, GICC_BASE, GICC_SIZE);
        if distributor || interface {
            if width != 4 || addr & 3 != 0 {
                return Err(Fault::Prohibited);
            }
            if distributor {
                self.gic.dist_write(addr - GICD_BASE, value);
            } else {
                self.gic.cpu_write(addr - GICC_BASE, value);
            }
            return Ok(());
        }
        Err(Fault::Unmapped)
    }
}

/// One-core bare-metal H3 machine used to grow the SoC implementation incrementally.
///
/// The architectural counter advances one virtual tick per machine step. This is
/// deterministic instruction-driven time, not yet wall-clock pacing at 24 MHz.
pub struct Machine {
    pub cpu: Cpu,
    pub soc: Soc,
    waiting: bool,
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
        }
    }

    pub fn load(&mut self, addr: u64, bytes: &[u8]) -> Result<(), Fault> {
        self.soc.ram.load(addr, bytes)
    }

    /// ARM32 Linux zImage entry: SVC/A32, MMU and caches off, IRQ/FIQ masked,
    /// r0 = 0, r1 = ~0 and r2 = the DTB physical address.
    pub fn boot_linux(&mut self, entry: u32, dtb: u32) {
        self.cpu = Cpu::new();
        self.cpu.r[0] = 0;
        self.cpu.r[1] = u32::MAX;
        self.cpu.r[2] = dtb;
        self.cpu.r[15] = entry;
        self.cpu.sctlr &= !(1 | 1 << 2 | 1 << 12);
        self.waiting = false;
        self.update_irqs();
    }

    fn update_irqs(&mut self) {
        self.soc
            .gic
            .set_level(PHYSICAL_TIMER_PPI, self.cpu.physical_timer_irq());
        let irq = self.soc.gic.irq_out();
        self.cpu.set_irq(irq, false);
        if irq {
            self.waiting = false;
        }
    }

    pub fn advance_time(&mut self, ticks: u64) {
        self.cpu.advance_time(ticks);
        self.update_irqs();
    }

    pub fn step(&mut self) -> Result<(), Trap> {
        self.update_irqs();
        if self.waiting && !self.cpu.irq {
            let ticks = if self.cpu.cntp_ctl & 3 == 1 && self.cpu.cntp_cval > self.cpu.cntpct {
                self.cpu.cntp_cval - self.cpu.cntpct
            } else {
                1
            };
            self.cpu.advance_time(ticks);
            self.update_irqs();
            return Err(Trap::Wait);
        }
        let result = self.cpu.step(&mut self.soc);
        self.cpu.advance_time(1);
        if result == Err(Trap::Wait) {
            self.waiting = true;
        }
        self.update_irqs();
        result
    }

    pub fn serial_output(&self) -> &[u8] {
        &self.soc.uart.out
    }

    pub fn take_serial_output(&mut self) -> Vec<u8> {
        std::mem::take(&mut self.soc.uart.out)
    }

    pub fn serial_input(&mut self, byte: u8) -> bool {
        self.soc.uart.push_rx(byte)
    }
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
