//! GICv3 non-secure subset for up to eight PEs: distributor, one redistributor per PE and the
//! ICC system-register interface of the PE that accesses it (`cur`).
//! Configurable INTID count, Group 1, edge/level pending, priority, acknowledge/EOI/deactivation, SPI
//! routing by affinity (or IRM "any PE") and SGIs by ICC_SGI1R_EL1.
//! No ITS, LPIs, virtualization or security groups.
//!
// System-register encodings shared with the Arm CPU's bus interface.
const fn key(op0: u32, op1: u32, crn: u32, crm: u32, op2: u32) -> u32 {
    (op0 << 14) | (op1 << 11) | (crn << 7) | (crm << 3) | op2
}

pub const NIRQ: usize = 480;
pub const IAR: u32 = key(3, 0, 12, 12, 0);
pub const EOIR: u32 = key(3, 0, 12, 12, 1);
pub const DIR: u32 = key(3, 0, 12, 11, 1);
pub const PMR: u32 = key(3, 0, 4, 6, 0);
pub const SRE: u32 = key(3, 0, 12, 12, 5);
pub const CTLR: u32 = key(3, 0, 12, 12, 4);
pub const GROUP1: u32 = key(3, 0, 12, 12, 7);
pub const SGI1R: u32 = key(3, 0, 12, 11, 5);
/// Size of one redistributor (RD_base + SGI_base frames).
pub const FRAME: u64 = 0x20000;

/// One INTID's configuration and state.
#[derive(Clone, Copy)]
struct Line { group: bool, enabled: bool, pending: bool, level: bool, active: bool, edge: bool, priority: u8 }
impl Default for Line {
    fn default() -> Self { Self { group: false, enabled: false, pending: false, level: false, active: false, edge: false, priority: 0xa0 } }
}
impl Line {
    fn asserted(&self) -> bool { self.pending || (self.level && !self.edge) }
}

/// Per-PE state: the banked SGIs/PPIs (INTIDs 0..32), the redistributor and the CPU interface.
struct Pe {
    private: [Line; 32],
    pmr: u8,
    cpu_ctlr: u64,
    group1: bool,
    waker: u32,
    sre: u64,
    running: Vec<(usize, u8)>,
    cached: std::cell::Cell<Option<Option<usize>>>,
}
impl Default for Pe {
    fn default() -> Self {
        Self { private: [Line::default(); 32], pmr: 0, cpu_ctlr: 0, group1: false, waker: 6, sre: 7,
            running: Vec::new(), cached: std::cell::Cell::new(None) }
    }
}

pub struct Gic {
    ctlr: u32,
    /// SPIs (indices below 32 unused: those live in each PE)
    shared: Vec<Line>,
    route: Vec<u64>,
    pes: Vec<Pe>,
    affinities: Vec<u64>,
    /// SPIs that are enabled, Group 1, asserted and not active (any PE may take them)
    ready: std::cell::RefCell<Option<Vec<u16>>>,
    /// the PE whose ICC registers system-register accesses reach
    pub cur: usize,
    pub acknowledges: u64,
}
impl Default for Gic {
    fn default() -> Self { Self::with_affinities(&[0]) }
}
impl Gic {
    /// Default RK3588 topology for the original callers (Aff1 = core index).
    pub fn with_cpus(n: usize) -> Self {
        let affinities: Vec<_> = (0..n.clamp(1, 8)).map(|core| (core as u64) << 8).collect();
        Self::with_affinities(&affinities)
    }

    /// MPIDR affinity fields for each redistributor, in frame order.
    pub fn with_affinities(affinities: &[u64]) -> Self {
        Self::with_affinities_and_intids(affinities, NIRQ)
    }

    /// Include all SPIs wired by the board, rounded to a GICD_TYPER bank.
    pub fn with_affinities_and_intids(affinities: &[u64], intids: usize) -> Self {
        assert!((32..=512).contains(&intids) && intids.is_multiple_of(32));
        let n = affinities.len();
        assert!((1..=8).contains(&n));
        Self { ctlr: 0x40, shared: vec![Line::default(); intids], route: vec![0; intids], pes: (0..n).map(|_| Pe::default()).collect(), affinities: affinities.to_vec(),
            ready: std::cell::RefCell::new(None), cur: 0, acknowledges: 0 }
    }
    pub fn cpus(&self) -> usize { self.pes.len() }
    /// Something shared changed: every PE's answer may differ.
    fn changed(&self) { self.ready.replace(None); for p in &self.pes { p.cached.set(None); } }
    fn line(&self, pe: usize, id: usize) -> &Line { if id < 32 { &self.pes[pe].private[id] } else { &self.shared[id] } }
    fn line_mut(&mut self, pe: usize, id: usize) -> &mut Line { if id < 32 { &mut self.pes[pe].private[id] } else { &mut self.shared[id] } }

    fn drive(line: &mut Line, high: bool) -> bool {
        let changed = line.level != high;
        if line.edge && high && !line.level { line.pending = true; }
        line.level = high;
        changed
    }
    /// Drive an SPI input (or, for INTIDs below 32, PE 0's PPI).
    pub fn set_line(&mut self, id: usize, high: bool) {
        if id < 32 { return self.set_ppi(0, id, high); }
        if Self::drive(&mut self.shared[id], high) { self.changed(); }
    }
    /// Drive PE `pe`'s private peripheral interrupt `id` (16..32).
    pub fn set_ppi(&mut self, pe: usize, id: usize, high: bool) {
        if pe >= self.pes.len() { return; }
        if Self::drive(&mut self.pes[pe].private[id], high) { self.pes[pe].cached.set(None); }
    }

    /// Whether SPI `id` may be delivered to PE `pe`.
    fn routed(&self, id: usize, pe: usize) -> bool {
        let r = self.route[id];
        r & (1 << 31) != 0 || r & 0xff_00ff_ffff == self.affinities[pe]
    }
    fn highest(&self, pe: usize) -> Option<usize> {
        let p = &self.pes[pe];
        if let Some(result) = p.cached.get() { return result; }
        let result = self.find_highest(pe);
        p.cached.set(Some(result));
        result
    }
    fn find_highest(&self, pe: usize) -> Option<usize> {
        let p = &self.pes[pe];
        if self.ctlr & 3 == 0 || !p.group1 || p.waker & 2 != 0 { return None; }
        let running = p.running.last().map_or(256, |(_, pr)| *pr as u16);
        let ok = |l: &Line| l.enabled && l.group && !l.active && l.asserted() && l.priority < p.pmr && (l.priority as u16) < running;
        let private = (0..32).filter(|&i| ok(&p.private[i]));
        let mut ready = self.ready.borrow_mut();
        let ready = ready.get_or_insert_with(|| (32..self.shared.len()).filter(|&i| { let l = &self.shared[i]; l.enabled && l.group && !l.active && l.asserted() }).map(|i| i as u16).collect());
        let shared = ready.iter().map(|&i| i as usize).filter(|&i| ok(&self.shared[i]) && self.routed(i, pe));
        private.chain(shared).min_by_key(|&i| (self.line(pe, i).priority, i))
    }
    /// The IRQ output to PE `pe`.
    pub fn irq_out(&self, pe: usize) -> bool { pe < self.pes.len() && self.highest(pe).is_some() }
    /// The IRQ output to the current PE.
    pub fn irq(&self) -> bool { self.irq_out(self.cur) }

    pub fn sys_read(&mut self, reg: u32) -> Option<u64> {
        let c = self.cur;
        Some(match reg {
            IAR => match self.highest(c) {
                Some(i) => {
                    let pr = self.line(c, i).priority;
                    let l = self.line_mut(c, i);
                    l.pending = false; l.active = true;
                    self.pes[c].running.push((i, pr));
                    self.acknowledges += 1;
                    if i < 32 { self.pes[c].cached.set(None); } else { self.changed(); }
                    i as u64
                }
                None => 1023,
            },
            PMR => self.pes[c].pmr as u64,
            CTLR => (4 << 8) | self.pes[c].cpu_ctlr, // five priority bits, no RSS/extended INTIDs
            SRE => self.pes[c].sre,
            GROUP1 => self.pes[c].group1 as u64,
            r if r == key(3,0,12,12,2) => self.highest(c).map_or(1023, |i| i as u64), // HPPIR1
            r if r == key(3,0,12,11,3) => self.pes[c].running.last().map_or(0xff, |(_, p)| *p as u64), // RPR
            r if r == key(3,0,12,12,3) => 0, // BPR1: minimum split
            r if r == key(3,0,12,9,0) => self.pes[c].running.iter().fold(0, |v,(_,p)| v | (1u64 << (p >> 3))),
            r if (key(3,0,12,8,4)..=key(3,0,12,8,7)).contains(&r) || (key(3,0,12,9,1)..=key(3,0,12,9,3)).contains(&r) => 0,
            r if r == key(3,0,12,8,0) => 1023, // no Group 0 interrupts
            _ => return None,
        })
    }
    pub fn sys_write(&mut self, reg: u32, v: u64) -> bool {
        let c = self.cur;
        // CPU-interface state is the current PE's own; EOI/DIR of an SPI and SGIs reach others
        let shared = match reg { EOIR | DIR => v & 0xff_ffff >= 32, SGI1R => true, _ => false };
        if shared { self.changed(); } else { self.pes[c].cached.set(None); }
        match reg {
            PMR => self.pes[c].pmr = v as u8 & 0xf8,
            SRE => self.pes[c].sre = v | 7,
            CTLR => self.pes[c].cpu_ctlr = v & 2,
            GROUP1 => self.pes[c].group1 = v & 1 != 0,
            EOIR => {
                let i = (v & 0xff_ffff) as usize;
                if self.pes[c].running.last().is_some_and(|&(id, _)| id == i) {
                    self.pes[c].running.pop();
                    if self.pes[c].cpu_ctlr & 2 == 0 { self.line_mut(c, i).active = false; }
                }
            }
            DIR => { let i = (v & 0xff_ffff) as usize; if i < self.shared.len() { self.line_mut(c, i).active = false; } }
            SGI1R => self.sgi(v),
            r if r == key(3,0,12,9,0) => self.pes[c].running.retain(|(_,p)| v & (1u64 << (*p >> 3)) != 0),
            r if r == key(3,0,12,12,3) || (key(3,0,12,8,4)..=key(3,0,12,8,7)).contains(&r) || (key(3,0,12,9,1)..=key(3,0,12,9,3)).contains(&r) => {},
            _ => return false,
        }
        true
    }
    /// ICC_SGI1R_EL1 from the current PE: INTID in 27:24; IRM (bit 40) targets every PE but the
    /// sender, otherwise Aff3/Aff2/Aff1 in 55:48/39:32/23:16 name a cluster, RS (47:44) a block of
    /// 16 Aff0 values and bits 15:0 the targets in it.
    fn sgi(&mut self, v: u64) {
        let id = ((v >> 24) & 15) as usize;
        let cluster = ((v >> 48) & 0xff) << 32 | ((v >> 32) & 0xff) << 16 | ((v >> 16) & 0xff) << 8;
        for pe in 0..self.pes.len() {
            let a = self.affinities[pe];
            let aff0 = a & 0xff;
            let hit = if v & (1 << 40) != 0 { pe != self.cur }
                else { a & !0xff == cluster && (v >> 44) & 15 == aff0 / 16 && v & (1 << (aff0 % 16)) != 0 };
            if hit { self.pes[pe].private[id].pending = true; }
        }
    }

    fn bits(&self, pe: usize, base: usize, which: u64) -> u32 {
        (0..32).fold(0, |v, n| { let i = base+n;
            if i >= self.shared.len() { return v; }
            let l = self.line(pe, i);
            let bit = match which { 0x80 => l.group, 0x100 | 0x180 => l.enabled,
                0x200 | 0x280 => l.asserted(), 0x300 | 0x380 => l.active, _ => false };
            v | (u32::from(bit) << n)
        })
    }
    fn bank_read(&self, pe: usize, off: u64) -> u32 {
        match off {
            0x80..=0xbf | 0x100..=0x3ff => { let kind = off & !0x7f; self.bits(pe, ((off-kind)/4*32) as usize,kind) }
            0x400..=0x5ff if (off-0x400) as usize + 4 <= self.shared.len() => { let i = (off-0x400) as usize; (0..4).fold(0, |v,n| v | ((self.line(pe, i+n).priority as u32) << (n*8))) }
            0xc00..=0xc7f if ((off-0xc00)/4*16) as usize + 16 <= self.shared.len() => { let i = ((off-0xc00)/4*16) as usize; (0..16).fold(0, |v,n| v | (u32::from(self.line(pe, i+n).edge) << (n*2+1))) }
            _ => 0,
        }
    }
    fn bank_write(&mut self, pe: usize, off: u64, v: u32) {
        match off {
            0x80..=0xbf | 0x100..=0x3ff => {
                let kind = off & !0x7f;
                let base = ((off-kind)/4*32) as usize;
                for n in 0..32 { let i=base+n; if i>=self.shared.len() { break; }
                    let set=v & (1<<n)!=0;
                    let l = self.line_mut(pe, i);
                    match kind { 0x80 => l.group=set,
                        0x100 if set => l.enabled=true, 0x180 if set => l.enabled=false,
                        0x200 if set => l.pending=true, 0x280 if set => l.pending=false,
                        0x300 if set => l.active=true, 0x380 if set => l.active=false, _=>{} }
                }
            }
            0x400..=0x5ff if (off-0x400) as usize + 4 <= self.shared.len() => { let i=(off-0x400) as usize; for n in 0..4 {self.line_mut(pe, i+n).priority=(v>>(n*8)) as u8 & 0xf8;} }
            0xc00..=0xc7f if ((off-0xc00)/4*16) as usize + 16 <= self.shared.len() => { let i=((off-0xc00)/4*16) as usize; for n in 0..16 {self.line_mut(pe, i+n).edge=v & (2<<(n*2))!=0;} }
            _=>{}
        }
    }
    /// Register offsets that cover INTIDs 0..31 (owned by the redistributors when ARE is set).
    fn private_offset(off: u64) -> bool {
        matches!(off, 0x80..=0x83 | 0x100..=0x103 | 0x180..=0x183 | 0x200..=0x203 | 0x280..=0x283 | 0x300..=0x303 | 0x380..=0x383 | 0x400..=0x41f | 0xc00..=0xc07)
    }
    pub fn dist_read(&self, off: u64) -> u32 {
        match off {
            0 => self.ctlr,
            4 => (self.shared.len() as u32 / 32 - 1) | ((self.pes.len() as u32 - 1) << 5) | (9 << 19), // 10-bit INTIDs; no LPIs
            8 => 0x0200_043b,
            0xffe8 => 0x30,
            0x6000..=0x6fff => { let i=((off-0x6000)/8) as usize; if i<self.shared.len() { (self.route[i] >> ((off&4)*8)) as u32 } else {0} }
            o if Self::private_offset(o) => 0,
            _ => self.bank_read(0, off),
        }
    }
    pub fn dist_write(&mut self, off: u64, v: u32) {
        self.changed();
        match off {
            0 => self.ctlr = (v & 0x37) | 0x40,
            0x6000..=0x6fff => { let i=((off-0x6000)/8) as usize; let shift=(off&4)*8; if i<self.shared.len() {self.route[i]=(self.route[i]&!(0xffff_ffff<<shift))|((v as u64)<<shift);} }
            o if Self::private_offset(o) => {},
            _ => self.bank_write(0, off,v),
        }
    }
    /// `off` from the start of the redistributor region: PE `off / FRAME`.
    pub fn redist_read(&self, off: u64) -> u32 {
        let (pe, off) = ((off / FRAME) as usize, off % FRAME);
        if pe >= self.pes.len() { return 0; }
        let last = pe + 1 == self.pes.len();
        let affinity = self.affinities[pe];
        let packed = (affinity & 0xff_ffff) | ((affinity >> 8) & 0xff00_0000);
        let typer = packed << 32 | (pe as u64) << 8 | u64::from(last) << 4;
        match off { 0=>0, 4=>0x0200_043b, 8=>typer as u32, 12=>(typer >> 32) as u32, 0x14=>self.pes[pe].waker, 0xffe8=>0x30,
            o if o >= 0x10000 && Self::private_offset(o - 0x10000) => self.bank_read(pe, o-0x10000), _=>0 }
    }
    pub fn redist_write(&mut self, off: u64, v: u32) {
        let (pe, off) = ((off / FRAME) as usize, off % FRAME);
        if pe >= self.pes.len() { return; }
        self.pes[pe].cached.set(None); // the redistributor holds only its PE's interrupts
        match off { 0x14=>self.pes[pe].waker=if v&2!=0 {6}else{0},
            o if o >= 0x10000 && Self::private_offset(o - 0x10000) => self.bank_write(pe, o-0x10000,v), _=>{} }
    }
}

emu_core::snap_struct!(Line { group, enabled, pending, level, active, edge, priority });
emu_core::snap_struct!(Pe { private, pmr, cpu_ctlr, group1, waker, sre, running } skip { cached });
emu_core::snap_struct!(Gic { ctlr, cur, acknowledges } fixed { shared, pes, route } skip { ready, affinities } after |s| {
    s.changed();
    if s.cur >= s.pes.len() { return Err(emu_core::snap::SnapError(format!("GIC: PE {} of {}", s.cur, s.pes.len()))); }
    Ok(())
});

#[cfg(test)]
mod topology_test {
    use super::*;

    #[test]
    fn sparse_affinities_identify_redistributors_and_route_sgis() {
        let mut gic = Gic::with_affinities(&[0, 0x1_0100, 0x1_0200]);
        gic.dist_write(0, 0x12);
        for pe in 0..3 {
            let base = pe as u64 * FRAME;
            gic.redist_write(base + 0x14, 0);
            gic.redist_write(base + 0x10080, 1 << 1);
            gic.redist_write(base + 0x10100, 1 << 1);
            gic.cur = pe;
            gic.sys_write(PMR, 0xf0);
            gic.sys_write(GROUP1, 1);
        }
        assert_eq!(gic.redist_read(FRAME + 12), 0x1_0100);
        assert_eq!(gic.redist_read(2 * FRAME + 12), 0x1_0200);

        gic.cur = 0;
        // SGI1, Aff2=1, Aff1=2, target Aff0=0.
        gic.sys_write(SGI1R, (1 << 32) | (1 << 24) | (2 << 16) | 1);
        assert!(!gic.irq_out(0));
        assert!(!gic.irq_out(1));
        assert!(gic.irq_out(2));
    }
}
