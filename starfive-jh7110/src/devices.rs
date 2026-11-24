//! The JH7110's interrupt and timer blocks (SiFive CLINT and PLIC) and its clock/reset
//! generators and system controller, at the register level Linux and OpenSBI use.

/// Harts in the JH7110: hart 0 is the S7 monitor core, harts 1-4 the U74 application cores.
pub const HARTS: usize = 5;

/// SiFive CLINT (`sifive,clint0`): per-hart MSIP and `mtimecmp`, and the shared `mtime`.
pub struct Clint {
    pub msip: [bool; HARTS],
    pub mtimecmp: [u64; HARTS],
}

impl Default for Clint {
    fn default() -> Self {
        Clint { msip: [false; HARTS], mtimecmp: [u64::MAX; HARTS] }
    }
}

impl Clint {
    pub fn read(&self, off: u64, size: u8, mtime: u64) -> u64 {
        match off {
            0..=0x3fff => {
                let h = (off / 4) as usize;
                self.msip.get(h).map_or(0, |&b| b as u64)
            }
            0x4000..=0xbff7 => {
                let h = ((off - 0x4000) / 8) as usize;
                let v = self.mtimecmp.get(h).copied().unwrap_or(0);
                if off & 4 != 0 { v >> 32 } else if size == 4 { v & 0xffff_ffff } else { v }
            }
            0xbff8 => if size == 4 { mtime & 0xffff_ffff } else { mtime },
            0xbffc => mtime >> 32,
            _ => 0,
        }
    }

    pub fn write(&mut self, off: u64, size: u8, v: u64) {
        match off {
            0..=0x3fff => {
                if let Some(m) = self.msip.get_mut((off / 4) as usize) {
                    *m = v & 1 != 0;
                }
            }
            0x4000..=0xbff7 => {
                let h = ((off - 0x4000) / 8) as usize;
                if let Some(c) = self.mtimecmp.get_mut(h) {
                    *c = match (size, off & 4) {
                        (8, _) => v,
                        (_, 0) => (*c & !0xffff_ffff) | (v & 0xffff_ffff),
                        _ => (*c & 0xffff_ffff) | (v << 32),
                    };
                }
            }
            _ => {} // mtime is read-only here (OpenSBI does not set it)
        }
    }
}

/// Interrupt sources of the PLIC (`riscv,ndev` = 136).
pub const PLIC_SOURCES: usize = 137;
/// PLIC contexts: 0 = hart 0 M; then M and S for each U74.
pub const PLIC_CONTEXTS: usize = 1 + 2 * (HARTS - 1);
const WORDS: usize = PLIC_SOURCES.div_ceil(32);

/// SiFive PLIC with level-triggered gateways.
pub struct Plic {
    priority: [u32; PLIC_SOURCES],
    pending: [u32; WORDS],
    /// claimed and not yet completed
    active: [u32; WORDS],
    /// device input levels
    level: [u32; WORDS],
    enable: [[u32; WORDS]; PLIC_CONTEXTS],
    threshold: [u32; PLIC_CONTEXTS],
}

impl Default for Plic {
    fn default() -> Self {
        Plic {
            priority: [0; PLIC_SOURCES],
            pending: [0; WORDS],
            active: [0; WORDS],
            level: [0; WORDS],
            enable: [[0; WORDS]; PLIC_CONTEXTS],
            threshold: [0; PLIC_CONTEXTS],
        }
    }
}

impl Plic {
    /// The context of `hart`'s machine (`s` false) or supervisor (`s` true) interrupt.
    pub fn context(hart: usize, s: bool) -> Option<usize> {
        match (hart, s) {
            (0, false) => Some(0),
            (0, true) => None,
            (h, s) if h < HARTS => Some(1 + 2 * (h - 1) + s as usize),
            _ => None,
        }
    }

    /// Drive a device interrupt line.
    pub fn set_level(&mut self, src: usize, high: bool) {
        let (w, b) = (src / 32, 1u32 << (src % 32));
        if high {
            self.level[w] |= b;
            if self.active[w] & b == 0 {
                self.pending[w] |= b;
            }
        } else {
            self.level[w] &= !b;
            self.pending[w] &= !b;
        }
    }

    fn best(&self, ctx: usize) -> (usize, u32) {
        let mut best = (0, 0);
        for w in 0..WORDS {
            let mut m = self.pending[w] & self.enable[ctx][w];
            while m != 0 {
                let src = w * 32 + m.trailing_zeros() as usize;
                m &= m - 1;
                let p = self.priority[src];
                if p > best.1 {
                    best = (src, p);
                }
            }
        }
        best
    }

    /// An edge-triggered request (a pulse): pending until claimed.
    pub fn trigger(&mut self, src: usize) {
        let (w, b) = (src / 32, 1u32 << (src % 32));
        if self.active[w] & b == 0 {
            self.pending[w] |= b;
        }
    }

    /// Whether context `ctx` has an interrupt above its threshold.
    pub fn asserted(&self, ctx: usize) -> bool {
        let (src, p) = self.best(ctx);
        src != 0 && p > self.threshold[ctx]
    }

    pub fn read(&mut self, off: u64) -> u32 {
        match off {
            0..=0xfff => self.priority.get((off / 4) as usize).copied().unwrap_or(0),
            0x1000..=0x1fff => self.pending.get(((off - 0x1000) / 4) as usize).copied().unwrap_or(0),
            0x2000..=0x1f_ffff => {
                let ctx = ((off - 0x2000) / 0x80) as usize;
                let w = ((off - 0x2000) % 0x80 / 4) as usize;
                self.enable.get(ctx).and_then(|e| e.get(w)).copied().unwrap_or(0)
            }
            _ => {
                let ctx = ((off - 0x20_0000) / 0x1000) as usize;
                if ctx >= PLIC_CONTEXTS {
                    return 0;
                }
                match off & 0xfff {
                    0 => self.threshold[ctx],
                    4 => {
                        // claim
                        let (src, p) = self.best(ctx);
                        if src == 0 || p <= self.threshold[ctx] {
                            return 0;
                        }
                        let (w, b) = (src / 32, 1u32 << (src % 32));
                        self.pending[w] &= !b;
                        self.active[w] |= b;
                        src as u32
                    }
                    _ => 0,
                }
            }
        }
    }

    pub fn write(&mut self, off: u64, v: u32) {
        match off {
            0..=0xfff => {
                if let Some(p) = self.priority.get_mut((off / 4) as usize) {
                    *p = v & 7;
                }
            }
            0x1000..=0x1fff => {}
            0x2000..=0x1f_ffff => {
                let ctx = ((off - 0x2000) / 0x80) as usize;
                let w = ((off - 0x2000) % 0x80 / 4) as usize;
                if let Some(e) = self.enable.get_mut(ctx).and_then(|e| e.get_mut(w)) {
                    *e = if w == 0 { v & !1 } else { v };
                }
            }
            _ => {
                let ctx = ((off - 0x20_0000) / 0x1000) as usize;
                if ctx >= PLIC_CONTEXTS {
                    return;
                }
                match off & 0xfff {
                    0 => self.threshold[ctx] = v & 7,
                    4 => {
                        // complete
                        let src = v as usize;
                        if src > 0 && src < PLIC_SOURCES {
                            let (w, b) = (src / 32, 1u32 << (src % 32));
                            if self.active[w] & b != 0 {
                                self.active[w] &= !b;
                                if self.level[w] & b != 0 {
                                    self.pending[w] |= b;
                                }
                            }
                        }
                    }
                    _ => {}
                }
            }
        }
    }
}

/// A JH7110 clock and reset generator (`syscrg`, `aoncrg`, `stgcrg`, ...): one 32-bit
/// register per clock (bit 31 gate, bits 29:24 mux, low bits divider) and a bank of reset
/// assert registers whose status registers read back the inverse (1 = out of reset), which
/// is what Linux's `reset-starfive-jh71x0` polls for.
pub struct Crg {
    regs: Vec<u32>,
    assert_off: u64,
    status_off: u64,
    banks: u64,
}

impl Crg {
    pub fn new(size: usize, assert_off: u64, status_off: u64, banks: u64) -> Self {
        let mut regs = vec![0; size / 4];
        // clock registers below the reset bank: divider 1, gate open (the boot ROM and
        // SPL leave the clocks Linux's early devices need running)
        for r in regs.iter_mut().take((assert_off / 4) as usize) {
            *r = 0x8000_0001;
        }
        Crg { regs, assert_off, status_off, banks }
    }

    /// Reset values for clock dividers: (clock index, divisor), gate open.
    pub fn with_dividers(mut self, dividers: &[(usize, u32)]) -> Self {
        for &(i, d) in dividers {
            self.regs[i] = 0x8000_0000 | d;
        }
        self
    }

    pub fn read(&self, off: u64) -> u32 {
        if off >= self.status_off && off < self.status_off + 4 * self.banks {
            let a = self.regs[((off - self.status_off + self.assert_off) / 4) as usize];
            return !a;
        }
        self.regs.get((off / 4) as usize).copied().unwrap_or(0)
    }

    pub fn write(&mut self, off: u64, v: u32) {
        if off >= self.status_off && off < self.status_off + 4 * self.banks {
            return;
        }
        if let Some(r) = self.regs.get_mut((off / 4) as usize) {
            *r = v;
        }
    }
}

/// Plain read-back registers with reset values (system controllers).
pub struct Regs {
    pub regs: Vec<u32>,
}

impl Regs {
    pub fn new(size: usize, init: &[(u64, u32)]) -> Self {
        let mut regs = vec![0; size / 4];
        for &(off, v) in init {
            regs[(off / 4) as usize] = v;
        }
        Regs { regs }
    }
    pub fn read(&self, off: u64) -> u32 {
        self.regs.get((off / 4) as usize).copied().unwrap_or(0)
    }
    pub fn write(&mut self, off: u64, v: u32) {
        if let Some(r) = self.regs.get_mut((off / 4) as usize) {
            *r = v;
        }
    }
}

/// `sys_syscon` reset values for the three PLLs, as the SPL leaves them: integer mode
/// (DACPD = DSMPD = 1) from the 24 MHz oscillator. PLL0 (CPU) 1 GHz = 24 * 125 / 3,
/// PLL1 (DDR) 1064 MHz = 24 * 133 / 3, PLL2 (peripherals) 1188 MHz = 24 * 99 / 2.
pub fn sys_syscon() -> Regs {
    Regs::new(
        0x1000,
        &[
            (0x18, (1 << 24) | (1 << 25)),
            (0x1c, 125),
            (0x24, 3 | (1 << 15) | (1 << 16) | (133 << 17)),
            (0x2c, 3 | (1 << 15) | (1 << 16) | (99 << 17)),
            (0x34, 2),
        ],
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plic_claim_complete_level() {
        let mut p = Plic::default();
        let ctx = Plic::context(1, true).unwrap();
        assert_eq!(ctx, 2);
        p.write(32 * 4, 1);
        p.write(0x2000 + 0x80 * ctx as u64 + 4, 1); // enable source 32
        p.set_level(32, true);
        assert!(p.asserted(ctx));
        assert!(!p.asserted(1));
        let claim = 0x20_0000 + 0x1000 * ctx as u64 + 4;
        assert_eq!(p.read(claim), 32);
        assert!(!p.asserted(ctx));
        p.write(claim, 32); // still high: pending again
        assert!(p.asserted(ctx));
        p.set_level(32, false);
        assert!(!p.asserted(ctx));
        p.write(0x20_0000 + 0x1000 * ctx as u64, 1); // threshold 1 masks priority 1
        p.set_level(32, true);
        assert!(!p.asserted(ctx));
    }

    /// An interrupt pending from a source the PLIC does not have is refused on restore (claiming
    /// it would index past the priorities).
    #[test]
    fn a_snapshot_with_a_source_beyond_the_plic_is_refused() {
        let mut p = Plic::default();
        p.set_level(PLIC_SOURCES - 1, true);
        let mut m = crate::Machine::new(8 << 20);
        m.soc.plic = p;
        crate::Machine::new(8 << 20).restore(&m.save().unwrap()).unwrap();
        for w in 0..3 {
            let e = crate::machine::refused(|m| match w {
                0 => m.soc.plic.pending[WORDS - 1] |= 1 << 31,
                1 => m.soc.plic.active[WORDS - 1] |= 1 << (PLIC_SOURCES % 32),
                _ => m.soc.plic.level[WORDS - 1] = u32::MAX,
            });
            assert!(e.contains("PLIC"), "case {w}: {e}");
        }
    }

    #[test]
    fn crg_reset_status_inverts_assert() {
        let mut c = Crg::new(0x400, 0x2f8, 0x308, 4);
        c.write(0x2f8, 0x5);
        assert_eq!(c.read(0x308), !0x5);
        assert_eq!(c.read(0x2f8), 0x5);
    }
}

/// The JH7110 power management unit (`starfive,jh7110-pmu`), as Linux's `jh71xx-pmu`
/// drives it: a domain mask written to SW_TURN_ON/OFF_POWER takes effect at once (the
/// "encourage" handshake that follows is accepted), and CURR_POWER_MODE reports the
/// domains that are on. The always-on SYSTOP and CPU domains start on.
pub struct Pmu {
    pub regs: Regs,
    pub on: u32,
}

impl Default for Pmu {
    fn default() -> Self {
        Pmu { regs: Regs::new(0x1000, &[]), on: 0b11 }
    }
}

impl Pmu {
    pub fn read(&self, off: u64) -> u32 {
        match off {
            0x80 => self.on,
            0x88 | 0x8c => 0,
            _ => self.regs.read(off),
        }
    }
    pub fn write(&mut self, off: u64, v: u32) {
        match off {
            0x0c => self.on |= v & 0x7f,
            0x10 => self.on &= !(v & 0x7c), // SYSTOP and CPU cannot be switched off
            _ => self.regs.write(off, v),
        }
    }
}

/// The JH7110 temperature sensor (`starfive,jh7110-temp`, 0x120e0000): one register with
/// power-down, reset and run controls; a run converts at once and pulses the sensor's
/// interrupt, and DOUT (bits 27:16) reads about 45 °C
/// (`dout * 237500 / 4094 - 81100` m°C, Linux's `sfctemp` formula).
#[derive(Default)]
pub struct TempSensor {
    ctrl: u32,
    pub conversions: u64,
}

impl TempSensor {
    const DOUT: u32 = 2173;

    pub fn read(&self, off: u64) -> u32 {
        if off == 0 { (self.ctrl & 7) | (Self::DOUT << 16) } else { 0 }
    }

    /// A register write; true when a conversion completed (an interrupt pulse).
    pub fn write(&mut self, off: u64, v: u32) -> bool {
        if off != 0 {
            return false;
        }
        let start = v & 4 != 0 && self.ctrl & 4 == 0 && v & 1 != 0 && v & 2 == 0;
        self.ctrl = v;
        if start {
            self.conversions = self.conversions.wrapping_add(1);
        }
        start
    }
}

/// The security block's true random number generator (`starfive,jh7110-trng`): seeded and
/// idle; a generate command fills RAND0..7 and raises RAND_RDY, a reseed raises SEED_DONE.
/// The numbers come from a deterministic generator (runs stay reproducible): they are not
/// entropy.
pub struct Trng {
    ie: u32,
    istat: u32,
    mode: u32,
    smode: u32,
    rand: [u32; 8],
    state: u64,
}

impl Default for Trng {
    fn default() -> Self {
        Trng { ie: 0, istat: 0, mode: 0, smode: 0, rand: [0; 8], state: 0x5bc0_e000_7110_2026 }
    }
}

impl Trng {
    const STAT_SEEDED: u32 = 1 << 9;
    const ISTAT_RAND_RDY: u32 = 1 << 0;
    const ISTAT_SEED_DONE: u32 = 1 << 1;
    const IE_GLBL_EN: u32 = 1 << 31;

    pub fn read(&mut self, off: u64) -> u32 {
        match off {
            // STAT: seeded, never busy; nonce/mission/256-bit modes as set in MODE/SMODE
            0x04 => Self::STAT_SEEDED | (self.mode & (1 << 3)) | (self.smode & (1 << 2 | 1 << 8)),
            0x08 => self.mode,
            0x0c => self.smode,
            0x10 => self.ie,
            0x14 => self.istat,
            0x20..=0x3c => self.rand[((off - 0x20) / 4) as usize],
            _ => 0,
        }
    }

    pub fn write(&mut self, off: u64, v: u32) {
        match off {
            0x00 => match v {
                1 => {
                    for r in self.rand.iter_mut() {
                        // xorshift64*
                        self.state ^= self.state >> 12;
                        self.state ^= self.state << 25;
                        self.state ^= self.state >> 27;
                        *r = (self.state.wrapping_mul(0x2545_f491_4f6c_dd1d) >> 32) as u32;
                    }
                    self.istat |= Self::ISTAT_RAND_RDY;
                }
                2 => self.istat |= Self::ISTAT_SEED_DONE,
                _ => {}
            },
            0x08 => self.mode = v,
            0x0c => self.smode = v,
            0x10 => self.ie = v,
            0x14 => self.istat &= !v,
            _ => {}
        }
    }

    pub fn irq(&self) -> bool {
        self.ie & Self::IE_GLBL_EN != 0 && self.istat & self.ie & 0x13 != 0
    }
}

// Snapshots. A clock generator's register offsets and banks are the board's.
emu_core::snap_struct!(Clint { msip, mtimecmp });
emu_core::snap_struct!(Plic { priority, pending, active, level, enable, threshold } after |p| {
    // only the sources there are can be pending, claimed or high (`best` looks up the priority
    // of a pending one)
    let none_above = |words: &[u32; WORDS]| words.iter().enumerate().all(|(w, &v)| {
        let first = w * 32;
        first + 32 <= PLIC_SOURCES || v >> (PLIC_SOURCES - first) == 0
    });
    if ![&p.pending, &p.active, &p.level].into_iter().all(none_above) {
        return Err(emu_core::snap::SnapError(format!("a PLIC interrupt source beyond its {PLIC_SOURCES}")));
    }
    Ok(())
});
emu_core::snap_struct!(Crg {} fixed { regs } skip { assert_off, status_off, banks });
emu_core::snap_struct!(Regs {} fixed { regs });
emu_core::snap_struct!(Pmu { regs, on });
emu_core::snap_struct!(TempSensor { ctrl, conversions });
emu_core::snap_struct!(Trng { ie, istat, mode, smode, rand, state });
