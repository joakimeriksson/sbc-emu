//! Smaller BCM2711 blocks: the iproc RNG200 random number generator, the AVS temperature
//! monitor, the power-management watchdog, the BCM2835 system timer and the mini UART (AUX).
//! Times are virtual nanoseconds.
use std::collections::VecDeque;

// ---------------------------------------------------------------------------------- RNG200

/// iproc-rng200: a FIFO of random words the driver polls (no interrupt).
pub struct Rng200 {
    ctrl: u32,
    state: u64,
}

impl Rng200 {
    pub fn new() -> Self {
        // seeded from the host where it can (a deterministic seed on targets without one)
        let seed = {
            use std::hash::{BuildHasher, Hasher};
            let mut h = std::collections::hash_map::RandomState::new().build_hasher();
            h.write_u64(0x5bc0_e0b8);
            h.finish()
        };
        Rng200 { ctrl: 0, state: seed | 1 }
    }
    fn next(&mut self) -> u32 {
        let x = &mut self.state;
        *x ^= *x >> 12;
        *x ^= *x << 25;
        *x ^= *x >> 27;
        (x.wrapping_mul(0x2545_f491_4f6c_dd1d) >> 32) as u32
    }
    pub fn read(&mut self, off: u64) -> u32 {
        let on = self.ctrl & 0x1fff != 0;
        match off {
            0x00 => self.ctrl,
            0x0c => if on { 0xffff_ffff } else { 0 },   // total bit count (warm-up done)
            0x18 => 0,                                  // interrupt status: no errors
            0x20 => if on { self.next() } else { 0 },   // FIFO data
            0x24 => if on { 16 } else { 0 },            // FIFO count (words)
            _ => 0,
        }
    }
    pub fn write(&mut self, off: u64, v: u32) { if off == 0 { self.ctrl = v; } }
}

impl Default for Rng200 { fn default() -> Self { Self::new() } }

// ---------------------------------------------------------------------------------- thermal

/// The AVS monitor's temperature register (`brcm,bcm2711-thermal`): code = (410040 - mC) / 487.
pub struct Avs {
    pub temp_mc: i32,
}

impl Avs {
    pub fn read(&self, off: u64) -> u32 {
        match off {
            0x200 => {
                let code = ((410_040 - self.temp_mc as i64) / 487).clamp(0, 0x3ff) as u32;
                code | (1 << 10) | (1 << 16) // valid
            }
            _ => 0,
        }
    }
}

// ---------------------------------------------------------------------------------- watchdog

const PM_PASSWORD: u32 = 0x5a00_0000;

/// Power management block: the watchdog (PM_RSTC, PM_RSTS, PM_WDOG) at 65536 ticks per second.
pub struct Pm {
    rstc: u32,
    rsts: u32,
    /// virtual time at which the armed watchdog fires
    deadline: Option<u64>,
    wdog_ticks: u32,
    pub now: u64,
}

impl Pm {
    pub fn new() -> Self { Pm { rstc: 0, rsts: 0x1000, deadline: None, wdog_ticks: 0, now: 0 } }
    fn ticks_left(&self) -> u32 {
        match self.deadline {
            Some(d) if d > self.now => (((d - self.now) as u128 * 65536 / 1_000_000_000) as u32).min(0xfffff),
            Some(_) => 0,
            None => self.wdog_ticks,
        }
    }
    pub fn read(&mut self, off: u64) -> u32 {
        match off {
            0x1c => self.rstc,
            0x20 => self.rsts,
            0x24 => self.ticks_left(),
            _ => 0,
        }
    }
    pub fn write(&mut self, off: u64, v: u32) {
        if v & 0xff00_0000 != PM_PASSWORD { return; }
        let v = v & 0x00ff_ffff;
        match off {
            0x1c => {
                self.rstc = v;
                // WRCFG = full reset (0x20) arms the watchdog; RESET (0x102) stops it
                self.deadline = if v & 0x30 == 0x20 { Some(self.now + self.wdog_ticks as u64 * 1_000_000_000 / 65536) } else { None };
            }
            0x20 => self.rsts = v,
            0x24 => self.wdog_ticks = v & 0xfffff,
            _ => {}
        }
    }
    /// The watchdog fired: Some(true) = halt (the firmware's "partition 63" power-off), Some(false) = reset.
    pub fn expired(&self) -> Option<bool> {
        self.deadline.filter(|&d| self.now >= d).map(|_| {
            let p = self.rsts & 0x555;
            p == 0x555
        })
    }
    pub fn next_event(&self) -> Option<u64> { self.deadline }
    pub fn save_state(&self, w: &mut emu_core::snap::W) {
        w.tag("pm-v1"); w.u32(self.rstc); w.u32(self.rsts); w.opt_u64(self.deadline); w.u32(self.wdog_ticks);
    }
    pub fn load_state(&mut self, r: &mut emu_core::snap::R) -> Result<(), emu_core::snap::SnapError> {
        r.tag("pm-v1")?; self.rstc = r.u32()?; self.rsts = r.u32()?; self.deadline = r.opt_u64()?; self.wdog_ticks = r.u32()?; Ok(())
    }
}

impl Default for Pm { fn default() -> Self { Self::new() } }

// ---------------------------------------------------------------------------------- system timer

/// BCM2835 system timer: a free-running 1 MHz counter and four compare channels (SPI 64..67).
pub struct SysTimer {
    cs: u32,
    cmp: [u32; 4],
    /// counter value up to which matches have been latched
    last: u64,
    now: u64,
}

impl SysTimer {
    pub fn new() -> Self { SysTimer { cs: 0, cmp: [0; 4], last: 0, now: 0 } }
    fn count(&self) -> u64 { self.now / 1000 }
    /// Advance to virtual time `ns`, latching the compare matches passed on the way.
    pub fn set_now(&mut self, ns: u64) {
        self.now = self.now.max(ns);
        let (a, b) = (self.last, self.count());
        if b > a {
            for (k, &c) in self.cmp.iter().enumerate() {
                let next = (a & !0xffff_ffff) | c as u64;
                let next = if next <= a { next + (1 << 32) } else { next };
                if next <= b { self.cs |= 1 << k; }
            }
            self.last = b;
        }
    }
    pub fn read(&mut self, off: u64) -> u32 {
        match off {
            0x00 => self.cs,
            0x04 => self.count() as u32,
            0x08 => (self.count() >> 32) as u32,
            0x0c..=0x18 => self.cmp[((off - 0x0c) / 4) as usize],
            _ => 0,
        }
    }
    pub fn write(&mut self, off: u64, v: u32) {
        match off {
            0x00 => self.cs &= !(v & 0xf),
            0x0c..=0x18 => self.cmp[((off - 0x0c) / 4) as usize] = v,
            _ => {}
        }
    }
    pub fn irq(&self, k: usize) -> bool { self.cs >> k & 1 != 0 }
    /// Virtual time of the next compare match.
    pub fn next_event(&self) -> Option<u64> {
        let now = self.count();
        self.cmp.iter().map(|&c| {
            let n = (now & !0xffff_ffff) | c as u64;
            let n = if n <= now { n + (1 << 32) } else { n };
            n * 1000
        }).min()
    }
    pub fn save_state(&self, w: &mut emu_core::snap::W) {
        w.tag("systimer-v1"); w.u32(self.cs); for c in self.cmp { w.u32(c); } w.u64(self.last); w.u64(self.now);
    }
    pub fn load_state(&mut self, r: &mut emu_core::snap::R) -> Result<(), emu_core::snap::SnapError> {
        r.tag("systimer-v1")?; self.cs = r.u32()?; for c in self.cmp.iter_mut() { *c = r.u32()?; } self.last = r.u64()?; self.now = r.u64()?; Ok(())
    }
}

impl Default for SysTimer { fn default() -> Self { Self::new() } }

// ---------------------------------------------------------------------------------- mini UART

/// AUX block with the mini UART, programmed by Linux as a 16550 (8250_bcm2835aux, reg-shift 2).
/// Transmission is instantaneous; received bytes wait in an 8-byte FIFO.
pub struct AuxUart {
    pub out: Vec<u8>,
    rx: VecDeque<u8>,
    enables: u32,
    ier: u32,
    lcr: u32,
    mcr: u32,
    scratch: u32,
    cntl: u32,
    baud: u32,
}

impl AuxUart {
    pub fn new() -> Self { AuxUart { out: Vec::new(), rx: VecDeque::new(), enables: 0, ier: 0, lcr: 0, mcr: 0, scratch: 0, cntl: 3, baud: 0 } }
    pub fn push_rx(&mut self, b: u8) { if self.rx.len() < 8 { self.rx.push_back(b); } }
    fn dlab(&self) -> bool { self.lcr & 0x80 != 0 }
    fn rx_irq(&self) -> bool { self.ier & 1 != 0 && !self.rx.is_empty() }
    fn tx_irq(&self) -> bool { self.ier & 2 != 0 }
    pub fn irq(&self) -> bool { self.enables & 1 != 0 && (self.rx_irq() || self.tx_irq()) }
    pub fn read(&mut self, off: u64) -> u32 {
        match off {
            0x00 => self.irq() as u32,                       // AUX_IRQ
            0x04 => self.enables,                            // AUX_ENABLES
            0x40 => if self.dlab() { self.baud & 0xff } else { self.rx.pop_front().unwrap_or(0) as u32 },
            0x44 => if self.dlab() { self.baud >> 8 & 0xff } else { self.ier },
            0x48 => 0xc0 | if self.rx_irq() { 0x04 } else if self.tx_irq() { 0x02 } else { 0x01 }, // IIR
            0x4c => self.lcr,
            0x50 => self.mcr,
            0x54 => 0x60 | !self.rx.is_empty() as u32,      // LSR: THRE | TEMT | DR
            0x58 => 0x20,                                    // MSR: CTS
            0x5c => self.scratch,
            0x60 => self.cntl,
            0x64 => 0x30c | (!self.rx.is_empty() as u32) | ((self.rx.len() as u32) << 16), // STAT
            0x68 => self.baud,
            _ => 0,
        }
    }
    pub fn write(&mut self, off: u64, v: u32) {
        match off {
            0x04 => self.enables = v & 7,
            0x40 => if self.dlab() { self.baud = (self.baud & 0xff00) | (v & 0xff) } else if self.enables & 1 != 0 { self.out.push(v as u8) },
            0x44 => if self.dlab() { self.baud = (self.baud & 0xff) | ((v & 0xff) << 8) } else { self.ier = v & 0xf },
            0x48 => if v & 2 != 0 { self.rx.clear() }, // FCR: clear RX FIFO
            0x4c => self.lcr = v & 0xff,
            0x50 => self.mcr = v & 0xff,
            0x5c => self.scratch = v & 0xff,
            0x60 => self.cntl = v,
            0x68 => self.baud = v & 0xffff,
            _ => {}
        }
    }
}

impl AuxUart {
    pub fn save_state(&self, w: &mut emu_core::snap::W) {
        w.tag("aux-v1");
        for v in [self.enables, self.ier, self.lcr, self.mcr, self.scratch, self.cntl, self.baud] { w.u32(v); }
    }
    pub fn load_state(&mut self, r: &mut emu_core::snap::R) -> Result<(), emu_core::snap::SnapError> {
        r.tag("aux-v1")?;
        for v in [&mut self.enables, &mut self.ier, &mut self.lcr, &mut self.mcr, &mut self.scratch, &mut self.cntl, &mut self.baud] { *v = r.u32()?; }
        Ok(())
    }
}

impl Default for AuxUart { fn default() -> Self { Self::new() } }
