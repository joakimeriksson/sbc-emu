//! The JH7110 machine: four U74 harts (hart 0, the S7 monitor core, is held in reset as
//! the board's firmware leaves it) on one physical bus, scheduled round-robin in virtual
//! time.
use crate::devices::{sys_syscon, Clint, Crg, Plic, Pmu, Regs, HARTS};
use crate::i2c::{Axp15060, DwI2c, PowerRequest};
use emu_core::Stop;
use peripherals::Uart;
use riscv64::{irq, Bus, Hart, Identity};
use std::collections::BTreeMap;

pub const RAM_BASE: u64 = 0x4000_0000;
/// The Orange Pi RV's 4 GiB variant.
pub const RAM_SIZE: usize = 4 << 30;

/// `timebase-frequency` of the JH7110 device trees: `mtime` counts at 4 MHz.
pub const MTIME_HZ: u64 = 4_000_000;
const NS_PER_TICK: u64 = 1_000_000_000 / MTIME_HZ;

/// PLIC source numbers.
pub const IRQ_UART0: usize = 32;

/// Instructions a hart runs before the next one gets its turn.
const QUANTUM: u64 = 1000;

pub const CLINT: u64 = 0x0200_0000;
pub const PLIC: u64 = 0x0c00_0000;
pub const UART0: u64 = 0x1000_0000;
pub const I2C5: u64 = 0x1205_0000;
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
    pub ram: Vec<u8>,
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
    pub i2c5: DwI2c,
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
            ram: vec![0; ram_size],
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
            i2c5: {
                let mut bus = DwI2c::default();
                bus.attach(PMIC_ADDR, Box::new(Axp15060::default()));
                bus
            },
            stubs: BTreeMap::new(),
            gpio_reset: false,
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
            _ if (SYSCRG..SYSCRG + 0x10000).contains(&a) => r32(self.syscrg.read(a - SYSCRG)),
            _ if (AONCRG..AONCRG + 0x10000).contains(&a) => r32(self.aoncrg.read(a - AONCRG)),
            _ if (STGCRG..STGCRG + 0x10000).contains(&a) => r32(self.stgcrg.read(a - STGCRG)),
            _ if (ISPCRG..ISPCRG + 0x10000).contains(&a) => r32(self.ispcrg.read(a - ISPCRG)),
            _ if (VOUTCRG..VOUTCRG + 0x10000).contains(&a) => r32(self.voutcrg.read(a - VOUTCRG)),
            _ if (SYS_SYSCON..SYS_SYSCON + 0x1000).contains(&a) => r32(self.sys_syscon.read(a - SYS_SYSCON)),
            _ if (PMU..PMU + 0x1000).contains(&a) => r32(self.pmu.read(a - PMU)),
            _ if (I2C5..I2C5 + 0x10000).contains(&a) => r32(self.i2c5.read(a - I2C5)),
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
            _ if (SYSCRG..SYSCRG + 0x10000).contains(&a) => self.syscrg.write(a - SYSCRG, v32),
            _ if (AONCRG..AONCRG + 0x10000).contains(&a) => self.aoncrg.write(a - AONCRG, v32),
            _ if (STGCRG..STGCRG + 0x10000).contains(&a) => self.stgcrg.write(a - STGCRG, v32),
            _ if (ISPCRG..ISPCRG + 0x10000).contains(&a) => self.ispcrg.write(a - ISPCRG, v32),
            _ if (VOUTCRG..VOUTCRG + 0x10000).contains(&a) => self.voutcrg.write(a - VOUTCRG, v32),
            _ if (SYS_SYSCON..SYS_SYSCON + 0x1000).contains(&a) => self.sys_syscon.write(a - SYS_SYSCON, v32),
            _ if (PMU..PMU + 0x1000).contains(&a) => self.pmu.write(a - PMU, v32),
            _ if (I2C5..I2C5 + 0x10000).contains(&a) => self.i2c5.write(a - I2C5, v32),
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
        &mut self.ram
    }
    fn read(&mut self, addr: u64, size: u8) -> Result<u64, ()> {
        let o = addr.wrapping_sub(RAM_BASE);
        if o + size as u64 <= self.ram.len() as u64 {
            let o = o as usize;
            let mut b = [0u8; 8];
            b[..size as usize].copy_from_slice(&self.ram[o..o + size as usize]);
            return Ok(u64::from_le_bytes(b));
        }
        self.dev_read(addr, size)
    }
    fn write(&mut self, addr: u64, size: u8, v: u64) -> Result<(), ()> {
        let o = addr.wrapping_sub(RAM_BASE);
        if o + size as u64 <= self.ram.len() as u64 {
            let o = o as usize;
            self.ram[o..o + size as usize].copy_from_slice(&v.to_le_bytes()[..size as usize]);
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
        if end > self.soc.ram.len() {
            return Err(format!("{:#x}+{:#x} is beyond RAM", addr, bytes.len()));
        }
        self.soc.ram[o..end].copy_from_slice(bytes);
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
            match self.soc.i2c5.device::<Axp15060>(PMIC_ADDR).map(|p| std::mem::replace(&mut p.request, PowerRequest::None)) {
                Some(PowerRequest::Reset) => return Stop::Reset,
                Some(PowerRequest::Off) => return Stop::PowerOff,
                _ => {}
            }
            if ran == 0 {
                return Stop::Idle(self.next_deadline());
            }
            self.soc.now += ran;
            left = left.saturating_sub(ran);
        }
        Stop::Budget
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
