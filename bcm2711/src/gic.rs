//! GIC-400 (GICv2) distributor and up to 8 CPU interfaces. SGIs and PPIs (IDs 0..31) are banked
//! per CPU; SPIs are shared and reach the CPUs in their ITARGETSR mask. Level inputs come from
//! devices (`set_level`) and the per-core timers (`set_ppi`); SGIs and edge SPIs latch pending;
//! each CPU interface acknowledges its highest-priority pending interrupt above its priority mask.
use std::cell::Cell;

pub const NIRQ: usize = 256;
pub const SPURIOUS: u32 = 1023;

/// The banked (per-CPU) part: SGIs, PPIs and the CPU interface.
#[derive(Clone)]
struct Bank {
    cctlr: u32,
    pmr: u32,
    bpr: u32,
    enabled: [bool; 32],
    pending: [bool; 32],
    active: [bool; 32],
    level: [bool; 32],
    prio: [u8; 32],
    /// pending SGIs: bit n = sent by CPU n
    sgi_src: [u8; 16],
    /// stack of acknowledged interrupts' running priorities
    running: Vec<(u32, u8)>,
    /// banked lines that may be pending (a superset)
    hot: Cell<u32>,
}

impl Bank {
    fn new() -> Self {
        Bank { cctlr: 0, pmr: 0, bpr: 0, enabled: [false; 32], pending: [false; 32], active: [false; 32], level: [false; 32], prio: [0; 32],
               sgi_src: [0; 16], running: Vec::new(), hot: Cell::new(0) }
    }
    fn mark(&self, i: usize) { self.hot.set(self.hot.get() | (1 << i)); }
}

pub struct Gic {
    pub dctlr: u32,
    ncpu: usize,
    banks: Vec<Bank>,
    // shared state; entries below 32 are unused (banked)
    enabled: [bool; NIRQ],
    pending: [bool; NIRQ],
    active: [bool; NIRQ],
    level: [bool; NIRQ],
    edge: [bool; NIRQ],
    prio: [u8; NIRQ],
    target: [u8; NIRQ],
    /// SPI lines that may be pending (a superset): the priority scan only looks at these
    hot: [Cell<u64>; NIRQ / 64],
}

impl Default for Gic {
    fn default() -> Self { Self::new() }
}

impl Gic {
    pub fn new() -> Self { Self::with_cpus(1) }

    pub fn with_cpus(ncpu: usize) -> Self {
        assert!((1..=8).contains(&ncpu));
        let mut edge = [false; NIRQ];
        edge[..16].iter_mut().for_each(|e| *e = true);
        Gic { dctlr: 0, ncpu, banks: vec![Bank::new(); ncpu], enabled: [false; NIRQ], pending: [false; NIRQ], active: [false; NIRQ],
              level: [false; NIRQ], edge, prio: [0; NIRQ], target: [1; NIRQ], hot: Default::default() }
    }

    #[inline]
    fn mark(&self, i: usize) { let w = &self.hot[i / 64]; w.set(w.get() | (1 << (i % 64))); }

    /// A shared peripheral interrupt line (ID >= 32).
    pub fn set_level(&mut self, irq: usize, high: bool) {
        debug_assert!(irq >= 32);
        if self.edge[irq] && high && !self.level[irq] { self.pending[irq] = true; }
        self.level[irq] = high;
        if high { self.mark(irq); }
    }

    /// A private peripheral interrupt of CPU `c` (ID 16..31): the core's timers.
    pub fn set_ppi(&mut self, c: usize, irq: usize, high: bool) {
        let edge = self.edge[irq];
        let b = &mut self.banks[c];
        if edge && high && !b.level[irq] { b.pending[irq] = true; }
        b.level[irq] = high;
        if high { b.mark(irq); }
    }

    fn private_pending(&self, b: &Bank, i: usize) -> bool {
        if i < 16 { b.sgi_src[i] != 0 } else { b.pending[i] || (!self.edge[i] && b.level[i]) }
    }
    fn is_pending(&self, i: usize) -> bool { self.pending[i] || (!self.edge[i] && self.level[i]) }

    fn best(&self, c: usize) -> Option<(usize, u8)> {
        let b = &self.banks[c];
        let mut best: Option<(usize, u8)> = None;
        let mut bits = b.hot.get();
        while bits != 0 {
            let i = bits.trailing_zeros() as usize;
            bits &= bits - 1;
            if !self.private_pending(b, i) { b.hot.set(b.hot.get() & !(1 << i)); continue; }
            if b.enabled[i] && !b.active[i] && (b.prio[i] as u32) < b.pmr && best.is_none_or(|(_, p)| b.prio[i] < p) {
                best = Some((i, b.prio[i]));
            }
        }
        let me = 1u8 << c;
        for (wi, w) in self.hot.iter().enumerate() {
            let mut bits = w.get();
            while bits != 0 {
                let bit = bits.trailing_zeros() as usize;
                bits &= bits - 1;
                let i = wi * 64 + bit;
                if !self.is_pending(i) { w.set(w.get() & !(1 << bit)); continue; }
                if self.target[i] & me != 0 && self.enabled[i] && !self.active[i] && (self.prio[i] as u32) < b.pmr
                    && best.is_none_or(|(_, p)| self.prio[i] < p) {
                    best = Some((i, self.prio[i]));
                }
            }
        }
        best
    }

    /// CPU `c`'s IRQ input.
    pub fn irq_out(&self, c: usize) -> bool {
        let b = &self.banks[c];
        if self.dctlr & 1 == 0 || b.cctlr & 1 == 0 { return false; }
        match self.best(c) {
            Some((_, p)) => b.running.last().is_none_or(|&(_, rp)| p < rp),
            None => false,
        }
    }

    fn ack(&mut self, c: usize) -> u32 {
        if self.dctlr & 1 == 0 || self.banks[c].cctlr & 1 == 0 { return SPURIOUS; }
        match self.best(c) {
            Some((i, p)) if self.banks[c].running.last().is_none_or(|&(_, rp)| p < rp) => {
                let b = &mut self.banks[c];
                let iar = if i < 16 {
                    // the lowest-numbered sender; the SGI stays pending for the others
                    let src = b.sgi_src[i].trailing_zeros();
                    b.sgi_src[i] &= !(1 << src);
                    b.active[i] = true;
                    i as u32 | (src << 10)
                } else if i < 32 {
                    b.pending[i] = false;
                    b.active[i] = true;
                    i as u32
                } else {
                    self.pending[i] = false;
                    self.active[i] = true;
                    i as u32
                };
                self.banks[c].running.push((i as u32, p));
                iar
            }
            _ => SPURIOUS,
        }
    }

    fn deactivate(&mut self, c: usize, id: usize) {
        if id < 32 { self.banks[c].active[id] = false; } else { self.active[id] = false; }
    }

    fn eoi(&mut self, c: usize, v: u32) {
        let id = (v & 0x3ff) as usize;
        if id >= NIRQ { return; }
        let b = &mut self.banks[c];
        if let Some(pos) = b.running.iter().rposition(|&(i, _)| i as usize == id) { b.running.remove(pos); }
        // EOImode 0: priority drop and deactivation together
        if b.cctlr & (1 << 9) == 0 { self.deactivate(c, id); }
    }

    /// 32 one-bit flags of interrupts `base..base+32` as CPU `c` sees them.
    fn bits(&self, c: usize, base: usize, f: impl Fn(&Self, &Bank, usize) -> bool) -> u32 {
        let b = &self.banks[c];
        (0..32).fold(0, |acc, k| acc | ((base + k < NIRQ && f(self, b, base + k)) as u32) << k)
    }

    pub fn dist_read(&self, c: usize, off: u64) -> u32 {
        let o = off as usize;
        match o {
            0x000 => self.dctlr,
            0x004 => ((NIRQ as u32 / 32) - 1) | ((self.ncpu as u32 - 1) << 5), // ITLinesNumber, CPUNumber
            0x008 => 0x0200_143b,                                              // IIDR: GIC-400
            0x100..=0x17c | 0x180..=0x1fc => self.bits(c, (o & 0x7f) * 8, |g, b, i| if i < 32 { b.enabled[i] } else { g.enabled[i] }),
            0x200..=0x27c | 0x280..=0x2fc => self.bits(c, (o & 0x7f) * 8, |g, b, i| if i < 32 { g.private_pending(b, i) } else { g.is_pending(i) }),
            0x300..=0x37c | 0x380..=0x3fc => self.bits(c, (o & 0x7f) * 8, |g, b, i| if i < 32 { b.active[i] } else { g.active[i] }),
            0x400..=0x7fc => {
                let base = o - 0x400;
                (0..4).fold(0, |acc, k| {
                    let i = base + k;
                    let p = if i < 32 { self.banks[c].prio[i] } else { self.prio.get(i).copied().unwrap_or(0) };
                    acc | ((p as u32) << (8 * k))
                })
            }
            0x800..=0xbfc => {
                let base = o - 0x800;
                (0..4).fold(0, |acc, k| {
                    let i = base + k;
                    // banked IDs read as the reading CPU's own bit
                    let t = if i < 32 { 1u8 << c } else { self.target.get(i).copied().unwrap_or(0) };
                    acc | ((t as u32) << (8 * k))
                })
            }
            0xc00..=0xcfc => {
                let base = (o - 0xc00) * 4;
                (0..16).fold(0, |acc, k| acc | (if base + k < NIRQ && self.edge[base + k] { 2 } else { 0 }) << (2 * k))
            }
            0xf10..=0xf1f | 0xf20..=0xf2f => {
                // SPENDSGIR/CPENDSGIR: one byte of sender bits per SGI
                let base = o & 0xc;
                (0..4).fold(0, |acc, k| acc | ((self.banks[c].sgi_src[base + k] as u32) << (8 * k)))
            }
            0xfe8 => 0x2b, // ICPIDR2: GICv2
            _ => 0,
        }
    }

    pub fn dist_write(&mut self, c: usize, off: u64, v: u32) {
        let o = off as usize;
        let set = |g: &mut Self, base: usize, f: &dyn Fn(&mut Self, usize)| {
            for k in 0..32 { if v & (1 << k) != 0 && base + k < NIRQ { f(g, base + k); } }
        };
        match o {
            0x000 => self.dctlr = v & 1,
            0x100..=0x17c => set(self, (o - 0x100) * 8, &|g, i| if i < 32 { g.banks[c].enabled[i] = true; g.banks[c].mark(i) } else { g.enabled[i] = true }),
            0x180..=0x1fc => set(self, (o - 0x180) * 8, &|g, i| if i < 32 { g.banks[c].enabled[i] = false } else { g.enabled[i] = false }),
            0x200..=0x27c => set(self, (o - 0x200) * 8, &|g, i| {
                if i < 16 {
                } else if i < 32 { g.banks[c].pending[i] = true; g.banks[c].mark(i) } else { g.pending[i] = true; g.mark(i) }
            }),
            0x280..=0x2fc => set(self, (o - 0x280) * 8, &|g, i| if i < 16 { g.banks[c].sgi_src[i] = 0 } else if i < 32 { g.banks[c].pending[i] = false } else { g.pending[i] = false }),
            0x300..=0x37c => set(self, (o - 0x300) * 8, &|g, i| if i < 32 { g.banks[c].active[i] = true } else { g.active[i] = true }),
            0x380..=0x3fc => set(self, (o - 0x380) * 8, &|g, i| g.deactivate(c, i)),
            0x400..=0x7fc => for k in 0..4 {
                let i = o - 0x400 + k;
                let p = (v >> (8 * k)) as u8;
                if i < 32 { self.banks[c].prio[i] = p } else if let Some(x) = self.prio.get_mut(i) { *x = p }
            },
            0x800..=0xbfc => for k in 0..4 {
                let i = o - 0x800 + k;
                if i >= 32 { if let Some(t) = self.target.get_mut(i) { *t = (v >> (8 * k)) as u8; } }
            },
            0xc00..=0xcfc => {
                let base = (o - 0xc00) * 4;
                for k in 0..16 { let i = base + k; if (16..NIRQ).contains(&i) { self.edge[i] = (v >> (2 * k + 1)) & 1 != 0; } }
            }
            0xf00 => {
                // SGIR: 0 = the CPUs in the target list, 1 = all but the sender, 2 = the sender
                let id = (v & 15) as usize;
                let list = match (v >> 24) & 3 { 0 => (v >> 16) & 0xff, 1 => 0xff & !(1 << c), 2 => 1 << c, _ => 0 };
                for t in 0..self.ncpu {
                    if list & (1 << t) != 0 { self.banks[t].sgi_src[id] |= 1 << c; self.banks[t].mark(id); }
                }
            }
            0xf10..=0xf1f => for k in 0..4 { let i = (o & 0xc) + k; self.banks[c].sgi_src[i] |= (v >> (8 * k)) as u8; self.banks[c].mark(i); },
            0xf20..=0xf2f => for k in 0..4 { let i = (o & 0xc) + k; self.banks[c].sgi_src[i] &= !((v >> (8 * k)) as u8); },
            _ => {}
        }
    }

    pub fn cpu_read(&mut self, c: usize, off: u64) -> u32 {
        match off {
            0x00 => self.banks[c].cctlr,
            0x04 => self.banks[c].pmr,
            0x08 => self.banks[c].bpr,
            0x0c => self.ack(c),
            0x14 => self.banks[c].running.last().map(|&(_, p)| p as u32).unwrap_or(0xff),
            0x18 => self.best(c).map(|(i, _)| i as u32).unwrap_or(SPURIOUS),
            0xfc => 0x0202_143b,
            _ => 0,
        }
    }

    pub fn cpu_write(&mut self, c: usize, off: u64, v: u32) {
        match off {
            0x00 => self.banks[c].cctlr = v,
            0x04 => self.banks[c].pmr = v & 0xff,
            0x08 => self.banks[c].bpr = v & 7,
            0x10 => self.eoi(c, v),
            0x1000 => { let id = (v & 0x3ff) as usize; if id < NIRQ { self.deactivate(c, id); } }
            _ => {}
        }
    }
}

impl Gic {
    pub fn save_state(&self, w: &mut emu_core::snap::W) {
        w.tag("gic-v2");
        w.u32(self.dctlr);
        w.u32(self.ncpu as u32);
        for b in &self.banks {
            w.u32(b.cctlr); w.u32(b.pmr); w.u32(b.bpr);
            for i in 0..32 { w.bool(b.enabled[i]); w.bool(b.pending[i]); w.bool(b.active[i]); w.bool(b.level[i]); w.u8(b.prio[i]); }
            for s in b.sgi_src { w.u8(s); }
            w.u64(b.running.len() as u64);
            for &(i, p) in &b.running { w.u32(i); w.u8(p); }
        }
        for i in 0..NIRQ {
            w.bool(self.enabled[i]); w.bool(self.pending[i]); w.bool(self.active[i]); w.bool(self.level[i]); w.bool(self.edge[i]);
            w.u8(self.prio[i]); w.u8(self.target[i]);
        }
    }

    pub fn load_state(&mut self, r: &mut emu_core::snap::R) -> Result<(), emu_core::snap::SnapError> {
        let p0 = r.p;
        if r.tag("gic-v2").is_err() { r.p = p0; return self.load_v1(r); }
        self.dctlr = r.u32()?;
        let n = r.u32()? as usize;
        if n != self.ncpu { return Err(emu_core::snap::SnapError(format!("snapshot GIC has {n} CPU interfaces, machine has {}", self.ncpu))); }
        for b in self.banks.iter_mut() {
            b.cctlr = r.u32()?; b.pmr = r.u32()?; b.bpr = r.u32()?;
            for i in 0..32 { b.enabled[i] = r.bool()?; b.pending[i] = r.bool()?; b.active[i] = r.bool()?; b.level[i] = r.bool()?; b.prio[i] = r.u8()?; }
            for s in b.sgi_src.iter_mut() { *s = r.u8()?; }
            b.running.clear();
            for _ in 0..r.u64()? { let i = r.u32()?; let p = r.u8()?; b.running.push((i, p)); }
            b.hot.set(u32::MAX);
        }
        for i in 0..NIRQ {
            self.enabled[i] = r.bool()?; self.pending[i] = r.bool()?; self.active[i] = r.bool()?; self.level[i] = r.bool()?; self.edge[i] = r.bool()?;
            self.prio[i] = r.u8()?; self.target[i] = r.u8()?;
        }
        for w in &self.hot { w.set(u64::MAX); }
        Ok(())
    }

    /// Snapshots from the single-CPU GIC model: everything banked belongs to CPU 0.
    fn load_v1(&mut self, r: &mut emu_core::snap::R) -> Result<(), emu_core::snap::SnapError> {
        r.tag("gic-v1")?;
        self.dctlr = r.u32()?;
        for b in self.banks.iter_mut() { *b = Bank::new(); }
        let b = &mut self.banks[0];
        b.cctlr = r.u32()?; b.pmr = r.u32()?; b.bpr = r.u32()?;
        for i in 0..NIRQ {
            let (en, pend, act, lvl, edge) = (r.bool()?, r.bool()?, r.bool()?, r.bool()?, r.bool()?);
            let (prio, target) = (r.u8()?, r.u8()?);
            if i < 32 {
                let b = &mut self.banks[0];
                b.enabled[i] = en; b.active[i] = act; b.level[i] = lvl; b.prio[i] = prio;
                if i < 16 { b.sgi_src[i] = pend as u8; } else { b.pending[i] = pend; }
            } else {
                // the single-CPU model ignored targets (and stored byte writes to them wrongly):
                // every SPI went to CPU 0
                let _ = target;
                self.enabled[i] = en; self.pending[i] = pend; self.active[i] = act; self.level[i] = lvl; self.prio[i] = prio; self.target[i] = 1;
            }
            self.edge[i] = edge;
        }
        let b = &mut self.banks[0];
        b.running.clear();
        for _ in 0..r.u64()? { let i = r.u32()?; let p = r.u8()?; b.running.push((i, p)); }
        b.hot.set(u32::MAX);
        for w in &self.hot { w.set(u64::MAX); }
        Ok(())
    }
}
