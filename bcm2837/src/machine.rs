//! The Pi 3 machine: up to four Cortex-A53 cores, their bus, virtual time and devices.
//!
//! Virtual time: one retired instruction = 1 ns (as on the Pi 4). The generic counter runs at
//! 19.2 MHz (the Pi 3's crystal; the Pi 4's is 54 MHz). A core in `wfi` skips to the next timer
//! deadline. The scheduling (cores in turns on one thread, or a thread per core with
//! `--threads multi`) is the Pi 4's; what differs is the interrupt path: each core's timer
//! outputs and mailboxes go into the per-core controller, device interrupts into the ARM-side
//! controller, whose output the per-core controller routes to one core.
use crate::intc::{ArmCtrl, LocalIntc};
use crate::Model;
use arm_a64::{Cpu, CpuProfile};
use bcm2711::mailbox::Mailbox;
use bcm2711::periph::{AuxUart, Pm};
use bcm2711::sdhci::Sdhci;
use emu_core::{Bus, Core, Fault, Trap};
use peripherals::pl011::Pl011;

pub const CNTFRQ: u64 = 19_200_000;
/// The peripheral window (16 MiB), the VideoCore's 0x7e00_0000 bus addresses seen by the ARM.
pub const PERIPH: u64 = 0x3f00_0000;
pub const ASB: u64 = 0x3f00_a000;
pub const ARMCTRL: u64 = 0x3f00_b200;
pub const MBOX: u64 = 0x3f00_b880;
pub const PM: u64 = 0x3f10_0000;
pub const RNG: u64 = 0x3f10_4000;
pub const UART0: u64 = 0x3f20_1000;
pub const THERMAL: u64 = 0x3f21_2000;
pub const AUX: u64 = 0x3f21_5000;
/// the Arasan SD host (`brcm,bcm2835-sdhci`, "mmcnr" in the vendor tree)
pub const SDHCI: u64 = 0x3f30_0000;
/// the ARM-local block: the per-core interrupt controller and mailboxes
pub const LOCAL: u64 = 0x4000_0000;
/// ARM-side interrupt numbers (bank 0)
pub const ARM_MAILBOX: usize = 1;
/// GPU interrupt numbers (banks 1 and 2 of the ARM-side controller)
pub const GPU_AUX: usize = 29;
pub const GPU_UART0: usize = 57;
pub const GPU_RNG: usize = 61;
pub const GPU_SDHCI: usize = 62;

pub struct Soc {
    pub ram: Vec<u8>,
    /// RAM given to Linux; the VideoCore's share (and the framebuffer) is above it
    pub arm_ram: u64,
    /// the core whose access is on the bus
    pub cur: usize,
    pub local: LocalIntc,
    pub armctrl: ArmCtrl,
    /// UART0 (PL011): Bluetooth's on a real 3B+, a spare serial port here (ttyAMA0)
    pub uart: Pl011,
    /// the mini UART: the serial console (ttyS0)
    pub aux: AuxUart,
    pub mbox: Mailbox,
    /// the SD card's host
    pub sd: Sdhci,
    pub rng: crate::periph::Rng,
    pub thermal: crate::periph::Thermal,
    pub pm: Pm,
    /// AXI bridge (ASB) control registers
    asb: [u32; 12],
    pub unknown: Vec<u64>,
    /// a device register was touched: interrupt lines must be re-derived
    pub dirty: bool,
}

impl Soc {
    /// The RAM Linux gets (below the VideoCore's share).
    pub fn arm_ram(&self) -> u64 { self.arm_ram }

    /// Core `k`'s IRQ input.
    pub fn irq_out(&self, k: usize) -> bool { self.local.irq_out(k, self.armctrl.irq()) }

    pub(crate) fn mmio_read(&mut self, a: u64) -> Result<u32, Fault> {
        self.dirty = true;
        Ok(match a {
            UART0..=0x3f20_1fff => self.uart.read(a - UART0),
            AUX..=0x3f21_50ff => self.aux.read(a - AUX),
            MBOX..=0x3f00_b8bf => self.mbox.read(a - MBOX),
            ARMCTRL..=0x3f00_b3ff => self.armctrl.read(a - ARMCTRL),
            SDHCI..=0x3f30_00ff => self.sd.read(a - SDHCI),
            RNG..=0x3f10_400f => self.rng.read(a - RNG),
            THERMAL..=0x3f21_2007 => self.thermal.read(a - THERMAL),
            PM..=0x3f10_0113 => self.pm.read(a - PM),
            // AXI bridges (ASB): the ID register; a stop request (bit 0) is acknowledged (bit 1) at once
            ASB..=0x3f00_a02f => match a & 0xff { 0x20 => 0x6272_6467, o => { let r = self.asb[(o / 4) as usize % 12]; (r & !2) | ((r & 1) << 1) } },
            LOCAL..=0x4000_00ff => self.local.read(a - LOCAL, self.armctrl.irq()),
            PERIPH..=0x3fff_ffff | 0x4000_0100..=0x4003_ffff => { self.note(a); 0 }
            _ => return Err(Fault::Unmapped),
        })
    }

    /// `size`-byte write of `v` at `a` (unshifted value, any alignment within the word).
    fn mmio_write(&mut self, a: u64, v: u32, size: u32) -> Result<(), Fault> {
        self.dirty = true;
        if (SDHCI..=0x3f30_00ff).contains(&a) { self.sd.write(a - SDHCI, v, size, &mut self.ram); return Ok(()); }
        let a = a & !3;
        match a {
            UART0..=0x3f20_1fff => self.uart.write(a - UART0, v),
            AUX..=0x3f21_50ff => self.aux.write(a - AUX, v),
            MBOX..=0x3f00_b8bf => self.mbox.write(a - MBOX, v, &mut self.ram),
            ARMCTRL..=0x3f00_b3ff => self.armctrl.write(a - ARMCTRL, v),
            RNG..=0x3f10_400f => self.rng.write(a - RNG, v),
            THERMAL..=0x3f21_2007 => self.thermal.write(a - THERMAL, v),
            PM..=0x3f10_0113 => self.pm.write(a - PM, v),
            ASB..=0x3f00_a02f => self.asb[((a & 0xff) / 4) as usize % 12] = v & 0x00ff_ffff,
            LOCAL..=0x4000_00ff => self.local.write(a - LOCAL, v),
            PERIPH..=0x3fff_ffff | 0x4000_0100..=0x4003_ffff => self.note(a),
            _ => return Err(Fault::Unmapped),
        }
        Ok(())
    }

    /// Device interrupt lines into the ARM-side controller at virtual time `now` (ns).
    pub(crate) fn device_lines(&mut self, now: u64) {
        self.armctrl.set_gpu(GPU_AUX, self.aux.irq());
        self.armctrl.set_gpu(GPU_UART0, self.uart.irq());
        self.armctrl.set_gpu(GPU_SDHCI, self.sd.irq());
        self.armctrl.set_arm(ARM_MAILBOX, self.mbox.irq());
        self.pm.now = now;
        // nothing reads the spare UART's output yet: keep it from growing without bound
        if self.uart.out.len() > 1 << 16 { self.uart.out.clear(); }
        self.dirty = false;
    }

    /// The next device deadline.
    pub(crate) fn next_event(&self) -> Option<u64> { self.pm.next_event() }

    /// What the display shows, as RGBA: the firmware framebuffer.
    pub fn render(&self, out: &mut Vec<u8>) -> Option<(u32, u32)> { self.mbox.render(&self.ram, out) }
    pub fn shown_size(&self) -> Option<(u32, u32)> { self.mbox.fb.allocated.then(|| self.mbox.fb_size()) }

    fn note(&mut self, a: u64) {
        let page = a & !0xfff;
        if !self.unknown.contains(&page) {
            self.unknown.push(page);
            eprintln!("[pi3] unmodelled peripheral access at {a:#x}");
        }
    }
    #[inline]
    fn ram_slice(&mut self, a: u64, n: usize) -> Option<&mut [u8]> {
        let o = a as usize;
        if a < self.ram.len() as u64 && o + n <= self.ram.len() { Some(&mut self.ram[o..o + n]) } else { None }
    }
}

macro_rules! ram_rw {
    ($r:ident, $w:ident, $t:ty, $n:expr) => {
        #[inline]
        fn $r(&mut self, a: u64) -> Result<$t, Fault> {
            if let Some(s) = self.ram_slice(a, $n) { return Ok(<$t>::from_le_bytes(s.try_into().unwrap())); }
            let v = self.mmio_read(a & !3)? as u64 >> (8 * (a & 3));
            Ok(v as $t)
        }
        #[inline]
        fn $w(&mut self, a: u64, v: $t) -> Result<(), Fault> {
            if let Some(s) = self.ram_slice(a, $n) { s.copy_from_slice(&v.to_le_bytes()); return Ok(()); }
            self.mmio_write(a, v as u32, $n)
        }
    };
}

impl Bus for Soc {
    ram_rw!(read8, write8, u8, 1);
    ram_rw!(read16, write16, u16, 2);
    ram_rw!(read32, write32, u32, 4);
    fn read64(&mut self, a: u64) -> Result<u64, Fault> {
        if let Some(s) = self.ram_slice(a, 8) { return Ok(u64::from_le_bytes(s.try_into().unwrap())); }
        let lo = self.mmio_read(a)? as u64;
        let hi = self.mmio_read(a + 4)? as u64;
        Ok(lo | (hi << 32))
    }
    fn write64(&mut self, a: u64, v: u64) -> Result<(), Fault> {
        if let Some(s) = self.ram_slice(a, 8) { s.copy_from_slice(&v.to_le_bytes()); return Ok(()); }
        self.mmio_write(a, v as u32, 4)?;
        self.mmio_write(a + 4, (v >> 32) as u32, 4)
    }
}

pub use emu_core::Stop;

/// What a core is doing between rounds.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CoreState {
    /// powered off (PSCI CPU_OFF, or a secondary never started)
    Off,
    Run,
    /// `wfi`: asleep until its interrupt line rises
    Wfi,
    /// `wfe` with no other core running: asleep until another core runs or time is skipped
    Wfe,
}

/// Virtual time each core runs per round while several cores are runnable (ns = instructions);
/// `SBC_QUANTUM` overrides it (experiments).
fn quantum() -> u64 {
    static Q: std::sync::OnceLock<u64> = std::sync::OnceLock::new();
    *Q.get_or_init(|| std::env::var("SBC_QUANTUM").ok().and_then(|v| v.parse().ok()).unwrap_or(20_000))
}

pub struct Machine {
    pub model: Model,
    pub cpus: Vec<Cpu>,
    pub state: Vec<CoreState>,
    pub soc: Soc,
    /// virtual time in ns: the latest time any core or device has seen
    pub now: u64,
    pub exceptions: u64,
    pub irqs: u64,
    /// the core running now (or last): it holds the shared JIT
    cur: usize,
    /// instruction count of the current core at which interrupt lines must be re-evaluated
    next_check: u64,
    /// virtual time at which the current round ends (u64::MAX: one core runs alone)
    round_end: u64,
    /// a debugger's breakpoints (interpreter only, see `set_debugging`)
    pub breakpoints: emu_core::debug::Breakpoints,
    /// the deadline of the last idle stop (`idle_until` wakes sleeping cores there, not before)
    idle_target: u64,
    /// run the cores on their own host threads (faster with several busy cores, not repeatable);
    /// false: one thread, cores in turns, deterministic
    pub multi: bool,
    #[cfg(not(target_arch = "wasm32"))]
    pub(crate) mt: Option<crate::mt::Mt>,
}

/// Counter ticks to the first virtual ns at which the 19.2 MHz counter reaches them.
pub(crate) fn ns_of_count(c: u64) -> u64 { (c / 12 * 625) + (c % 12 * 625).div_ceil(12) }

/// The earliest enabled timer deadline of a core (ns).
pub(crate) fn timer_deadline(cpu: &Cpu) -> Option<u64> {
    let s = &cpu.sys;
    [(s.vtimer, s.cntvoff), (s.ptimer, 0)].iter().filter(|(t, _)| t.ctl & 3 == 1).map(|(t, off)| ns_of_count(t.cval.wrapping_add(*off))).min()
}

impl Machine {
    /// A Pi 3 of `model` with `ram` bytes of SDRAM (the board's own size: `model.ram_mb()`; at
    /// most 1 GiB, all the SoC addresses) and `ncpu` cores (the board has 4); only core 0 starts.
    pub fn new(model: Model, ram: usize, ncpu: usize) -> Self {
        let ncpu = ncpu.clamp(1, 4);
        let ram = ram.min(1 << 30);
        // the ARM sees RAM up to the peripheral window; the VideoCore's share is at the top
        let ram_arm_visible = ram.min(PERIPH as usize);
        let gpu = (model.gpu_mb() << 20).min(ram / 4);
        let arm_ram = (ram - gpu) as u64;
        let fb_max = (ram_arm_visible as u64).saturating_sub(arm_ram);
        let mut mbox = Mailbox::new(arm_ram, fb_max, 1024, 768);
        mbox.revision = model.revision();
        let soc = Soc {
            ram: vec![0; ram_arm_visible], arm_ram, cur: 0, local: LocalIntc::new(), armctrl: ArmCtrl::new(), uart: Pl011::new(), aux: AuxUart::new(),
            mbox, sd: Sdhci::new(None), rng: crate::periph::Rng::new(), thermal: crate::periph::Thermal::new(), pm: Pm::new(), asb: [0; 12],
            unknown: Vec::new(), dirty: true,
        };
        let mut m = Machine { model, cpus: Vec::new(), state: vec![CoreState::Off; ncpu], soc, now: 0, exceptions: 0, irqs: 0, cur: 0, next_check: 0,
                              round_end: u64::MAX, breakpoints: Default::default(), idle_target: 0, multi: false,
                              #[cfg(not(target_arch = "wasm32"))] mt: None };
        for k in 0..ncpu { let c = m.fresh_cpu(k); m.cpus.push(c); }
        m.state[0] = CoreState::Run;
        m
    }

    fn fresh_cpu(&mut self, k: usize) -> Cpu {
        let mut cpu = Cpu::new();
        cpu.profile = CpuProfile::CORTEX_A53;
        cpu.sys.cntfrq = CNTFRQ;
        cpu.sys.mpidr = k as u64;
        cpu.firmware_calls_to_host = true;
        cpu.ram = self.soc.ram.as_mut_ptr();
        cpu.ram_len = self.soc.ram.len() as u64;
        // one compiled-pages map for all cores (IC IVAU broadcasts only where code was compiled)
        cpu.code_pages = Some(match self.cpus.first().and_then(|c| c.code_pages.clone()) {
            Some(m) => m,
            None => Cpu::code_page_map(cpu.ram_len),
        });
        cpu
    }

    /// Core 0 (status displays; the boot core).
    pub fn cpu(&self) -> &Cpu { &self.cpus[0] }

    /// Instructions retired by all cores.
    pub fn instret(&self) -> u64 { self.cpus.iter().map(|c| c.instret).sum() }

    /// Turn the block JIT on (one code cache shared by all cores); returns whether it is active.
    pub fn enable_jit(&mut self) -> bool { self.cpus[self.cur].enable_jit() }
    pub fn disable_jit(&mut self) { for c in self.cpus.iter_mut() { c.jit = None; } }

    pub fn load(&mut self, addr: u64, data: &[u8]) {
        self.soc.ram[addr as usize..addr as usize + data.len()].copy_from_slice(data);
    }

    /// The serial console's input (the mini UART); false if its FIFO is full.
    pub fn serial_input(&mut self, b: u8) -> bool {
        if !self.soc.aux.rx_room() { return false; }
        self.soc.aux.push_rx(b);
        self.soc.dirty = true;
        true
    }
    pub fn serial_output(&self) -> &[u8] { &self.soc.aux.out }
    pub fn take_serial_output(&mut self) -> Vec<u8> { std::mem::take(&mut self.soc.aux.out) }

    /// Physical inputs: the SoC temperature (`cpu-temp`, degrees C).
    pub fn set_sensor(&mut self, what: &str, v: f64) {
        if matches!(what, "cpu-temp" | "soc-temp") { self.soc.thermal.temp_mc = (v * 1000.0) as i32; self.soc.mbox.temp_mc = (v * 1000.0).max(0.0) as u32; }
    }

    /// Linux arm64 boot protocol: enter at EL1h (as on the Pi 4 machine), MMU off, interrupts
    /// masked, x0 = DTB.
    pub fn boot_linux(&mut self, entry: u64, dtb: u64) {
        let cpu = &mut self.cpus[0];
        cpu.el = 1;
        cpu.spsel = true;
        cpu.daif = 0xf << 6;
        cpu.pc = entry;
        cpu.x = [0; 31];
        cpu.x[0] = dtb;
        cpu.sys.cpacr_el1 = 3 << 20;
    }

    /// Move core `k`'s clock to `t` if it is behind (a sleeping core's time follows the machine).
    fn catch_up(&mut self, k: usize, t: u64) {
        let c = &mut self.cpus[k];
        if c.now_ns() < t { c.sys.time_base_ns = t; c.sys.time_base_instret = c.instret; }
    }

    /// Make core `k` the running one: it takes the shared JIT; the core that stops running
    /// loses its exclusive reservation.
    fn switch_to(&mut self, k: usize) {
        if k != self.cur {
            let (a, b) = (self.cur, k);
            let (lo, hi) = self.cpus.split_at_mut(a.max(b));
            let (pa, pb) = if a < b { (&mut lo[a], &mut hi[0]) } else { (&mut hi[0], &mut lo[b]) };
            pa.hand_jit_to(pb);
            pa.clear_exclusive();
            self.cur = k;
        }
        self.soc.cur = k;
        self.next_check = 0;
    }

    fn update_irqs(&mut self) {
        let c = self.cur;
        let t = self.cpus[c].now_ns();
        if t > self.now { self.now = t; }
        let now = self.now;
        for k in 0..self.cpus.len() {
            if self.state[k] == CoreState::Off { continue; }
            if k != c && self.state[k] != CoreState::Run { self.catch_up(k, now); }
            let cpu = &mut self.cpus[k];
            cpu.refresh_counter();
            cpu.sys.timers_changed = false;
            let cnt = cpu.sys.cnt;
            let v = cpu.sys.vtimer.firing(cnt.wrapping_sub(cpu.sys.cntvoff));
            let p = cpu.sys.ptimer.firing(cnt);
            self.soc.local.set_timers(k, p, v);
        }
        self.soc.device_lines(now);
        for k in 0..self.cpus.len() {
            if self.state[k] == CoreState::Off { continue; }
            let irq = self.soc.irq_out(k);
            self.cpus[k].set_irq(irq, false);
            if irq && matches!(self.state[k], CoreState::Wfi | CoreState::Wfe) {
                // woken: from now on the cores take turns
                self.state[k] = CoreState::Run;
                self.round_end = self.round_end.min(t + quantum());
            }
        }
        self.soc.dirty = false;
        // next forced re-evaluation: the earliest deadline still ahead, at most 1 ms ahead
        let horizon = t + 1_000_000;
        let due = self.deadline_after(t, c).unwrap_or(horizon).min(horizon);
        self.next_check = self.cpus[c].instret + (due - t);
    }

    /// The earliest deadline after `t` that core `c` running alone must notice: its own timers,
    /// those of sleeping cores, devices.
    fn deadline_after(&self, t: u64, c: usize) -> Option<u64> {
        let mut d: Option<u64> = self.soc.next_event().filter(|&x| x > t);
        for (k, cpu) in self.cpus.iter().enumerate() {
            if self.state[k] == CoreState::Off || (k != c && self.state[k] == CoreState::Run) { continue; }
            if let Some(ns) = timer_deadline(cpu).filter(|&ns| ns > t) { d = Some(d.map_or(ns, |x| x.min(ns))); }
        }
        d
    }

    /// Next timer (of any core that is on) or device deadline in ns.
    fn next_deadline(&self) -> Option<u64> {
        let mut d = self.soc.next_event();
        for (k, cpu) in self.cpus.iter().enumerate() {
            if self.state[k] == CoreState::Off { continue; }
            let s = &cpu.sys;
            for (t, off) in [(s.vtimer, s.cntvoff), (s.ptimer, 0)] {
                if t.ctl & 3 == 1 {
                    let ns = ns_of_count(t.cval.wrapping_add(off));
                    // an expired timer whose interrupt is off wakes nobody: it must not stop time
                    if ns > self.now { d = Some(d.map_or(ns, |x: u64| x.min(ns))); }
                }
            }
        }
        d
    }

    /// Run until the cores have retired `budget` instructions between them (see the Pi 4's
    /// `Machine::run`: rounds of 20 µs while several cores run, one core alone without rounds).
    pub fn run(&mut self, budget: u64) -> Stop {
        #[cfg(not(target_arch = "wasm32"))]
        if self.multi && self.breakpoints.at.is_empty() && !self.cpus.iter().any(|c| c.jit_off || !c.watch.is_empty()) {
            return self.run_threaded(std::time::Duration::from_millis(10));
        }
        if self.cpus[0].mt {
            // back on one thread (a debugger is attached): turn-taking semantics again
            for c in self.cpus.iter_mut() { c.mt = false; }
        }
        let end = self.instret().saturating_add(budget);
        let mut total = self.instret();
        // steps, not retired instructions, bound the call: faults do not retire
        let mut steps = budget;
        let mut storm = 0u32;
        let stop = 'run: loop {
            if total >= end || steps == 0 { break Stop::Budget; }
            self.update_irqs();
            if let Some(halt) = self.soc.pm.expired() { break if halt { Stop::PowerOff } else { Stop::Reset }; }
            let any_run = self.state.contains(&CoreState::Run);
            if any_run { for s in self.state.iter_mut() { if *s == CoreState::Wfe { *s = CoreState::Run; } } }
            let runnable = self.state.iter().filter(|s| **s == CoreState::Run).count();
            if runnable == 0 {
                let until = self.next_deadline().unwrap_or(self.now + 10_000_000).max(self.now);
                self.idle_target = until;
                break Stop::Idle(until);
            }
            let round_start = self.now;
            self.round_end = if runnable == 1 { u64::MAX } else { round_start + quantum() };
            let (last, n) = (self.breakpoints.last_turn(self.cur), self.cpus.len());
            let watching = !self.cpus[0].watch.is_empty();
            for i in 0..n {
                let k = self.breakpoints.turn(i, last, n, watching);
                if self.state[k] != CoreState::Run { continue; }
                if let Some(stop) = self.slice(k, round_start, &mut total, end, &mut steps, &mut storm) { break 'run stop; }
                if total >= end || steps == 0 { break; }
            }
        };
        let t = self.cpus[self.cur].now_ns();
        if t > self.now { self.now = t; }
        stop
    }

    /// Run core `k` from `round_start` until the round ends, it sleeps, or the budget is used.
    fn slice(&mut self, k: usize, round_start: u64, total: &mut u64, end: u64, steps: &mut u64, storm: &mut u32) -> Option<Stop> {
        self.switch_to(k);
        self.catch_up(k, round_start);
        let breaks = !self.breakpoints.at.is_empty();
        loop {
            let t = self.cpus[k].now_ns();
            if t >= self.round_end || *total >= end || *steps == 0 { return None; }
            if breaks && self.breakpoints.hit(k, self.cpus[k].pc) { return Some(Stop::Breakpoint(k)); }
            *steps -= 1;
            if self.soc.dirty || self.cpus[k].sys.timers_changed || self.cpus[k].instret >= self.next_check {
                self.update_irqs();
                if let Some(halt) = self.soc.pm.expired() { return Some(if halt { Stop::PowerOff } else { Stop::Reset }); }
            }
            let cpu = &mut self.cpus[k];
            let i0 = cpu.instret;
            // compiled code may chain blocks until the slice or budget ends or a device needs a look
            cpu.chain_until = i0.saturating_add(self.round_end - t).min(self.next_check).min(i0 + (end - *total));
            let r = cpu.step(&mut self.soc);
            *total += self.cpus[k].instret - i0;
            if let Some((a, kind)) = self.cpus[k].watch_hit.take() { if r.is_ok() { return Some(Stop::Watchpoint(k, a, kind)); } }
            if let Some(op) = self.cpus[k].tlbi_bcast.take() {
                for (j, o) in self.cpus.iter_mut().enumerate() { if j != k { o.tlb_invalidate(op); } }
            }
            if let Some(op) = self.cpus[k].ic_bcast.take() {
                for (j, o) in self.cpus.iter_mut().enumerate() { if j != k { o.jit_invalidate(op); } }
            }
            match r {
                Ok(()) => *storm = 0,
                Err(Trap::Exception(_, el)) => {
                    self.exceptions += 1;
                    *storm += 1;
                    if *storm >= 1_000_000 {
                        let (e, cpu) = (el as usize, &self.cpus[k]);
                        return Some(Stop::ExceptionStorm { pc: cpu.pc, esr: cpu.esr[e], elr: cpu.elr[e], far: cpu.far[e], el });
                    }
                }
                Err(Trap::Interrupt(..)) => { self.irqs += 1; self.soc.dirty = true; }
                Err(Trap::Wait) => {
                    self.update_irqs();
                    if self.cpus[k].irq { continue; }
                    let others = (0..self.cpus.len()).any(|j| j != k && self.state[j] == CoreState::Run);
                    if self.cpus[k].last_wfe && others {
                        // waiting for another core (a lock, a flag): give the rest of the round away
                        if self.round_end != u64::MAX { let r = self.round_end; self.catch_up(k, r); }
                        return None;
                    }
                    self.state[k] = if self.cpus[k].last_wfe { CoreState::Wfe } else { CoreState::Wfi };
                    return None;
                }
                Err(Trap::HostCall(_)) => {
                    if let Some(stop) = self.psci(k) { return Some(stop); }
                    if self.state[k] != CoreState::Run { return None; }
                }
                Err(Trap::Unimplemented(pc, i)) => return Some(Stop::Unimplemented(pc, i)),
                Err(Trap::Hlt(n)) => return Some(Stop::Hlt(n)),
                Err(Trap::Brk(n)) => return Some(Stop::Brk(n)),
            }
        }
    }

    /// PSCI 1.1 (the emulated firmware's), called by core `k`.
    fn psci(&mut self, k: usize) -> Option<Stop> {
        const NOT_SUPPORTED: u64 = -1i64 as u64;
        const INVALID_PARAMETERS: u64 = -2i64 as u64;
        const ALREADY_ON: u64 = -4i64 as u64;
        let x = self.cpus[k].x;
        let target = |m: &Self| { let t = (x[1] & 0xff_00ff_ffff) as usize; (t < m.cpus.len()).then_some(t) };
        let r = match x[0] as u32 {
            0x8400_0000 => 0x0001_0001,                 // PSCI_VERSION 1.1
            0x8400_0002 => {                             // CPU_OFF
                self.state[k] = CoreState::Off;
                self.cpus[k].set_irq(false, false);
                return None;
            }
            0x8400_0003 | 0xc400_0003 => match target(self) { // CPU_ON(target, entry, context)
                None => INVALID_PARAMETERS,
                Some(t) if self.state[t] != CoreState::Off => ALREADY_ON,
                Some(t) => { self.cpu_on(k, t, x[2], x[3]); 0 }
            },
            0x8400_0004 | 0xc400_0004 => match target(self) { // AFFINITY_INFO: 0 on, 1 off
                None => INVALID_PARAMETERS,
                Some(t) => (self.state[t] == CoreState::Off) as u64,
            },
            0x8400_0006 => 2,                            // MIGRATE_INFO_TYPE: no migration needed
            0x8400_0008 => return Some(Stop::PowerOff),  // SYSTEM_OFF
            0x8400_0009 => return Some(Stop::Reset),     // SYSTEM_RESET
            0x8400_000a => match x[1] as u32 {           // PSCI_FEATURES
                0x8400_0000 | 0x8400_0002 | 0x8400_0003 | 0xc400_0003 | 0x8400_0004 | 0xc400_0004 | 0x8400_0006 | 0x8400_0008 | 0x8400_0009 | 0x8400_000a => 0,
                _ => NOT_SUPPORTED,
            },
            _ => NOT_SUPPORTED,
        };
        self.cpus[k].x[0] = r;
        None
    }

    /// Start core `t` at `entry` (MMU off, interrupts masked, x0 = `context`) in the caller's EL.
    fn cpu_on(&mut self, caller: usize, t: usize, entry: u64, context: u64) {
        let (el, now) = (self.cpus[caller].el, self.cpus[caller].now_ns());
        let jit_off = self.cpus[caller].jit_off;
        let c = &mut self.cpus[t];
        Self::reset_core(c, t, el, entry, context, now);
        c.jit_off = jit_off;
        self.state[t] = CoreState::Run;
        self.round_end = self.round_end.min(now + quantum());
        self.soc.dirty = true;
    }

    /// Core `k` out of reset at `entry` (PSCI CPU_ON): RAM, retired count and JIT are kept.
    pub(crate) fn reset_core(c: &mut Cpu, k: usize, el: u8, entry: u64, context: u64, now: u64) {
        let mut f = Cpu::new();
        f.profile = CpuProfile::CORTEX_A53;
        f.sys.cntfrq = CNTFRQ;
        f.sys.mpidr = k as u64;
        f.firmware_calls_to_host = true;
        f.ram = c.ram;
        f.ram_len = c.ram_len;
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
        f.mt = c.mt;
        f.wall_clock = c.wall_clock;
        let mut old = std::mem::replace(c, f);
        old.hand_jit_to(c);
    }

    /// Debugging: interpreter only on all cores (needed while breakpoints are set).
    pub fn set_debugging(&mut self, on: bool) { for c in self.cpus.iter_mut() { c.jit_off = on; } }

    /// Debugging: set or clear a breakpoint at `addr` (every core).
    pub fn breakpoint(&mut self, addr: u64, insert: bool) {
        self.breakpoints.set(addr, insert);
        let on = !self.breakpoints.at.is_empty() || !self.cpus[0].watch.is_empty();
        self.set_debugging(on);
    }

    /// Debugging: set or clear (`insert` false) a watchpoint on [addr, addr + len) for
    /// accesses of `kind` (1 write, 2 read, 3 either), on every core.
    pub fn watchpoint(&mut self, addr: u64, len: u64, kind: u8, insert: bool) {
        for c in self.cpus.iter_mut() {
            c.watch.retain(|&w| w != (addr, len, kind));
            if insert { c.watch.push((addr, len, kind)); }
        }
        let on = !self.breakpoints.at.is_empty() || !self.cpus[0].watch.is_empty();
        self.set_debugging(on);
    }

    /// Debugging: execute one instruction on core `k` (interpreter); a stop it causes, if any.
    pub fn debug_step(&mut self, k: usize) -> Option<Stop> {
        if self.state[k] == CoreState::Off { return None; }
        self.state[k] = CoreState::Run;
        let jit_off = self.cpus[k].jit_off;
        self.cpus[k].jit_off = true;
        let turn = self.breakpoints.last_turn(self.cur);
        self.breakpoints.begin_step(k, self.cpus[k].pc);
        self.round_end = u64::MAX;
        let (mut total, mut steps, mut storm) = (self.instret(), 1, 0);
        let t = self.cpus[k].now_ns().max(self.now);
        let r = self.slice(k, t, &mut total, u64::MAX, &mut steps, &mut storm);
        self.breakpoints.end_step(k);
        self.breakpoints.keep_turn(turn);
        self.cpus[k].jit_off = jit_off;
        let t = self.cpus[k].now_ns();
        if t > self.now { self.now = t; }
        r
    }

    /// Debugging: read guest memory at virtual address `va` as core `k` sees it (RAM only).
    pub fn debug_read(&mut self, k: usize, va: u64, len: usize) -> Option<Vec<u8>> {
        let mut out = Vec::with_capacity(len);
        while out.len() < len {
            let a = va.wrapping_add(out.len() as u64);
            let pa = self.cpus[k].debug_translate(&mut self.soc, a)?;
            let n = (len - out.len()).min(0x1000 - (a & 0xfff) as usize);
            out.extend_from_slice(self.soc.ram.get(pa as usize..pa as usize + n)?);
        }
        Some(out)
    }

    /// Debugging: write guest memory at virtual address `va` (RAM only); compiled code is dropped.
    pub fn debug_write(&mut self, k: usize, va: u64, data: &[u8]) -> bool {
        let mut done = 0;
        while done < data.len() {
            let a = va.wrapping_add(done as u64);
            let Some(pa) = self.cpus[k].debug_translate(&mut self.soc, a) else { return false };
            let n = (data.len() - done).min(0x1000 - (a & 0xfff) as usize);
            let Some(dst) = self.soc.ram.get_mut(pa as usize..pa as usize + n) else { return false };
            dst.copy_from_slice(&data[done..done + n]);
            done += n;
        }
        for c in self.cpus.iter_mut() { c.jit_invalidate(u64::MAX); }
        true
    }

    /// Advance idle time to `t` (after an `Idle` stop) and re-evaluate interrupts.
    pub fn idle_until(&mut self, t: u64) {
        if t > self.now { self.now = t; }
        let now = self.now;
        // waking happens at the idle stop's own deadline only (runs stay repeatable)
        let reached = now >= self.idle_target;
        for k in 0..self.cpus.len() {
            if self.state[k] == CoreState::Off { continue; }
            self.catch_up(k, now);
            if reached && self.state[k] == CoreState::Wfe { self.state[k] = CoreState::Run; }
        }
        self.update_irqs();
        if !reached { return; }
        // an idle stop ends at a deadline, and `wfi` may wake spuriously: a core whose own
        // timer deadline has passed resumes
        for k in 0..self.cpus.len() {
            if self.state[k] != CoreState::Wfi { continue; }
            if timer_deadline(&self.cpus[k]).is_some_and(|d| d <= now) { self.state[k] = CoreState::Run; }
        }
    }
}

const SNAP_MAGIC: &[u8; 8] = b"SBCPI3S1";

impl Machine {
    /// Serialize the whole machine (deflate-compressed; RAM keeps only non-zero pages).
    pub fn save(&self) -> Result<Vec<u8>, String> {
        let mut w = emu_core::snap::W::default();
        w.tag("pi3-machine-v1");
        w.u8(self.model as u8);
        w.u64(self.now); w.u64(self.exceptions); w.u64(self.irqs);
        w.u32(self.cpus.len() as u32);
        for (c, st) in self.cpus.iter().zip(&self.state) {
            w.u8(match st { CoreState::Off => 0, CoreState::Run => 1, CoreState::Wfi => 2, CoreState::Wfe => 3 });
            c.save_state(&mut w);
        }
        self.soc.local.save_state(&mut w);
        self.soc.armctrl.save_state(&mut w);
        self.soc.uart.save_state(&mut w);
        self.soc.aux.save_state(&mut w);
        self.soc.mbox.save_state(&mut w);
        self.soc.sd.save_state(&mut w);
        self.soc.rng.save_state(&mut w);
        self.soc.pm.save_state(&mut w);
        for r in self.soc.asb { w.u32(r); }
        emu_core::snap::save_ram(&mut w, &self.soc.ram);
        let mut out = SNAP_MAGIC.to_vec();
        out.extend_from_slice(&miniz_oxide::deflate::compress_to_vec(&w.0, 1));
        Ok(out)
    }

    /// Restore a snapshot into a machine made with the same options (model, RAM, cores, card).
    pub fn restore(&mut self, snap: &[u8]) -> Result<(), String> {
        if snap.len() < 8 || &snap[..8] != SNAP_MAGIC { return Err("not a Raspberry Pi 3 snapshot".into()); }
        let raw = miniz_oxide::inflate::decompress_to_vec(&snap[8..]).map_err(|e| format!("the snapshot is damaged ({:?})", e.status))?;
        let e = |e: emu_core::snap::SnapError| e.0;
        let mut r = emu_core::snap::R::new(&raw);
        r.tag("pi3-machine-v1").map_err(e)?;
        let model = r.u8().map_err(e)?;
        if model != self.model as u8 { return Err(format!("the snapshot is of another model (this machine: --model {})", self.model.id())); }
        let (now, exceptions, irqs) = (r.u64().map_err(e)?, r.u64().map_err(e)?, r.u64().map_err(e)?);
        let n = r.u32().map_err(e)? as usize;
        if n != self.cpus.len() { return Err(format!("snapshot has {n} cores, machine has {} (use --smp {n})", self.cpus.len())); }
        let mut jit = self.cpus[self.cur].jit.take();
        if let Some(j) = jit.as_mut() { j.flush(); }
        (self.now, self.exceptions, self.irqs) = (now, exceptions, irqs);
        self.cur = 0;
        self.soc.cur = 0;
        for k in 0..n {
            let mut c = self.fresh_cpu(k);
            c.jit_off = self.cpus[k].jit_off;
            c.watch = std::mem::take(&mut self.cpus[k].watch);
            self.cpus[k] = c;
            self.state[k] = match r.u8().map_err(e)? { 0 => CoreState::Off, 2 => CoreState::Wfi, 3 => CoreState::Wfe, _ => CoreState::Run };
            self.cpus[k].load_state(&mut r).map_err(e)?;
        }
        self.cpus[0].jit = jit;
        self.soc.local.load_state(&mut r).map_err(e)?;
        self.soc.armctrl.load_state(&mut r).map_err(e)?;
        self.soc.uart.load_state(&mut r).map_err(e)?;
        self.soc.aux.load_state(&mut r).map_err(e)?;
        self.soc.mbox.load_state(&mut r).map_err(e)?;
        self.soc.sd.load_state(&mut r).map_err(e)?;
        self.soc.rng.load_state(&mut r).map_err(e)?;
        self.soc.pm.load_state(&mut r).map_err(e)?;
        for x in self.soc.asb.iter_mut() { *x = r.u32().map_err(e)?; }
        emu_core::snap::load_ram(&mut r, &mut self.soc.ram).map_err(e)?;
        for c in self.cpus.iter_mut() { c.ram = self.soc.ram.as_mut_ptr(); c.ram_len = self.soc.ram.len() as u64; }
        self.next_check = 0;
        self.soc.dirty = true;
        Ok(())
    }
}
