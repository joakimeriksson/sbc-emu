//! GIC-400 (GICv2) distributor and CPU interface for one CPU. Level inputs come from devices
//! (`set_level`); SGIs and edge SPIs latch pending; the CPU interface acknowledges the
//! highest-priority pending interrupt above the priority mask.
pub const NIRQ: usize = 256;
pub const SPURIOUS: u32 = 1023;

pub struct Gic {
    pub dctlr: u32,
    pub cctlr: u32,
    pub pmr: u32,
    pub bpr: u32,
    enabled: [bool; NIRQ],
    pending: [bool; NIRQ],
    active: [bool; NIRQ],
    level: [bool; NIRQ],
    edge: [bool; NIRQ],
    prio: [u8; NIRQ],
    target: [u8; NIRQ],
    /// stack of acknowledged interrupts' running priorities
    running: Vec<(u32, u8)>,
}

impl Default for Gic {
    fn default() -> Self { Self::new() }
}

impl Gic {
    pub fn new() -> Self {
        let mut edge = [false; NIRQ];
        edge[..16].iter_mut().for_each(|e| *e = true);
        Gic { dctlr: 0, cctlr: 0, pmr: 0, bpr: 0, enabled: [false; NIRQ], pending: [false; NIRQ], active: [false; NIRQ],
              level: [false; NIRQ], edge, prio: [0; NIRQ], target: [1; NIRQ], running: Vec::new() }
    }

    pub fn set_level(&mut self, irq: usize, high: bool) {
        if self.edge[irq] && high && !self.level[irq] { self.pending[irq] = true; }
        self.level[irq] = high;
    }

    fn is_pending(&self, i: usize) -> bool { self.pending[i] || (!self.edge[i] && self.level[i]) }

    fn best(&self) -> Option<(usize, u8)> {
        let mut best: Option<(usize, u8)> = None;
        for i in 0..NIRQ {
            if self.enabled[i] && !self.active[i] && self.is_pending(i) && (self.prio[i] as u32) < self.pmr {
                if best.is_none_or(|(_, p)| self.prio[i] < p) { best = Some((i, self.prio[i])); }
            }
        }
        best
    }

    /// The CPU's IRQ input.
    pub fn irq_out(&self) -> bool {
        if self.dctlr & 1 == 0 || self.cctlr & 1 == 0 { return false; }
        match self.best() {
            Some((_, p)) => self.running.last().is_none_or(|&(_, rp)| p < rp),
            None => false,
        }
    }

    fn ack(&mut self) -> u32 {
        if self.dctlr & 1 == 0 || self.cctlr & 1 == 0 { return SPURIOUS; }
        match self.best() {
            Some((i, p)) if self.running.last().is_none_or(|&(_, rp)| p < rp) => {
                self.pending[i] = false;
                self.active[i] = true;
                self.running.push((i as u32, p));
                i as u32
            }
            _ => SPURIOUS,
        }
    }

    fn eoi(&mut self, v: u32) {
        let id = (v & 0x3ff) as usize;
        if id >= NIRQ { return; }
        if let Some(pos) = self.running.iter().rposition(|&(i, _)| i as usize == id) { self.running.remove(pos); }
        // EOImode 0: priority drop and deactivation together
        if self.cctlr & (1 << 9) == 0 { self.active[id] = false; }
    }

    fn bits_read(arr: &[bool; NIRQ], base: usize) -> u32 {
        (0..32).fold(0, |acc, b| acc | ((arr.get(base + b).copied().unwrap_or(false) as u32) << b))
    }

    pub fn dist_read(&self, off: u64) -> u32 {
        let o = off as usize;
        match o {
            0x000 => self.dctlr,
            0x004 => (NIRQ as u32 / 32) - 1, // ITLinesNumber, one CPU
            0x008 => 0x0200_143b,          // IIDR: GIC-400
            0x100..=0x17c => Self::bits_read(&self.enabled, (o - 0x100) * 8),
            0x180..=0x1fc => Self::bits_read(&self.enabled, (o - 0x180) * 8),
            0x200..=0x27c | 0x280..=0x2fc => {
                let base = (o & 0x7f) * 8;
                (0..32).fold(0, |acc, b| acc | ((base + b < NIRQ && self.is_pending(base + b)) as u32) << b)
            }
            0x300..=0x37c | 0x380..=0x3fc => Self::bits_read(&self.active, (o & 0x7f) * 8),
            0x400..=0x7fc => {
                let b = o - 0x400;
                (0..4).fold(0, |acc, k| acc | ((self.prio.get(b + k).copied().unwrap_or(0) as u32) << (8 * k)))
            }
            0x800..=0xbfc => {
                let b = o - 0x800;
                (0..4).fold(0, |acc, k| acc | ((self.target.get(b + k).copied().unwrap_or(0) as u32) << (8 * k)))
            }
            0xc00..=0xcfc => {
                let base = (o - 0xc00) * 4;
                (0..16).fold(0, |acc, k| acc | (if base + k < NIRQ && self.edge[base + k] { 2 } else { 0 }) << (2 * k))
            }
            0xfe8 => 0x2b, // ICPIDR2: GICv2
            _ => 0,
        }
    }

    pub fn dist_write(&mut self, off: u64, v: u32) {
        let o = off as usize;
        let each = |arr: &mut [bool; NIRQ], base: usize, val: bool| {
            for b in 0..32 { if v & (1 << b) != 0 && base + b < NIRQ { arr[base + b] = val; } }
        };
        match o {
            0x000 => self.dctlr = v & 1,
            0x100..=0x17c => each(&mut self.enabled, (o - 0x100) * 8, true),
            0x180..=0x1fc => each(&mut self.enabled, (o - 0x180) * 8, false),
            0x200..=0x27c => each(&mut self.pending, (o - 0x200) * 8, true),
            0x280..=0x2fc => each(&mut self.pending, (o - 0x280) * 8, false),
            0x300..=0x37c => each(&mut self.active, (o - 0x300) * 8, true),
            0x380..=0x3fc => each(&mut self.active, (o - 0x380) * 8, false),
            0x400..=0x7fc => for k in 0..4 { if let Some(p) = self.prio.get_mut(o - 0x400 + k) { *p = (v >> (8 * k)) as u8; } },
            0x800..=0xbfc => for k in 0..4 { if o - 0x800 + k >= 32 { if let Some(t) = self.target.get_mut(o - 0x800 + k) { *t = (v >> (8 * k)) as u8; } } },
            0xc00..=0xcfc => {
                let base = (o - 0xc00) * 4;
                for k in 0..16 { let i = base + k; if (16..NIRQ).contains(&i) { self.edge[i] = (v >> (2 * k + 1)) & 1 != 0; } }
            }
            0xf00 => {
                // SGIR: one CPU, so any target list that is non-empty (or "self") reaches us
                let filter = (v >> 24) & 3;
                let list = (v >> 16) & 0xff;
                if filter == 2 || list & 1 != 0 || filter == 1 { self.pending[(v & 15) as usize] = true; }
            }
            _ => {}
        }
    }

    pub fn cpu_read(&mut self, off: u64) -> u32 {
        match off {
            0x00 => self.cctlr,
            0x04 => self.pmr,
            0x08 => self.bpr,
            0x0c => self.ack(),
            0x14 => self.running.last().map(|&(_, p)| p as u32).unwrap_or(0xff),
            0x18 => self.best().map(|(i, _)| i as u32).unwrap_or(SPURIOUS),
            0xfc => 0x0202_143b,
            _ => 0,
        }
    }

    pub fn cpu_write(&mut self, off: u64, v: u32) {
        match off {
            0x00 => self.cctlr = v,
            0x04 => self.pmr = v & 0xff,
            0x08 => self.bpr = v & 7,
            0x10 => self.eoi(v),
            0x1000 => { let id = (v & 0x3ff) as usize; if id < NIRQ { self.active[id] = false; } }
            _ => {}
        }
    }
}

impl Gic {
    pub fn save_state(&self, w: &mut emu_core::snap::W) {
        w.tag("gic-v1");
        w.u32(self.dctlr); w.u32(self.cctlr); w.u32(self.pmr); w.u32(self.bpr);
        for i in 0..NIRQ {
            w.bool(self.enabled[i]); w.bool(self.pending[i]); w.bool(self.active[i]); w.bool(self.level[i]); w.bool(self.edge[i]);
            w.u8(self.prio[i]); w.u8(self.target[i]);
        }
        w.u64(self.running.len() as u64);
        for &(i, p) in &self.running { w.u32(i); w.u8(p); }
    }
    pub fn load_state(&mut self, r: &mut emu_core::snap::R) -> Result<(), emu_core::snap::SnapError> {
        r.tag("gic-v1")?;
        self.dctlr = r.u32()?; self.cctlr = r.u32()?; self.pmr = r.u32()?; self.bpr = r.u32()?;
        for i in 0..NIRQ {
            self.enabled[i] = r.bool()?; self.pending[i] = r.bool()?; self.active[i] = r.bool()?; self.level[i] = r.bool()?; self.edge[i] = r.bool()?;
            self.prio[i] = r.u8()?; self.target[i] = r.u8()?;
        }
        self.running.clear();
        for _ in 0..r.u64()? { let i = r.u32()?; let p = r.u8()?; self.running.push((i, p)); }
        Ok(())
    }
}
