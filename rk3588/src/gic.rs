//! GICv3 non-secure, single-PE subset: distributor, one redistributor, ICC interface.
//! 480 INTIDs, Group 1, edge/level pending, priority, acknowledge/EOI/deactivation.
//! No ITS, LPIs, virtualization, security groups or multi-PE routing yet.
use arm_a64::sysreg::key;

pub const NIRQ: usize = 480;
pub const IAR: u32 = key(3, 0, 12, 12, 0);
pub const EOIR: u32 = key(3, 0, 12, 12, 1);
pub const DIR: u32 = key(3, 0, 12, 11, 1);
pub const PMR: u32 = key(3, 0, 4, 6, 0);
pub const SRE: u32 = key(3, 0, 12, 12, 5);
pub const CTLR: u32 = key(3, 0, 12, 12, 4);
pub const GROUP1: u32 = key(3, 0, 12, 12, 7);

pub struct Gic {
    ctlr: u32,
    group: [bool; NIRQ],
    enabled: [bool; NIRQ],
    pending: [bool; NIRQ],
    level: [bool; NIRQ],
    active: [bool; NIRQ],
    edge: [bool; NIRQ],
    priority: [u8; NIRQ],
    route: [u64; NIRQ],
    pmr: u8,
    cpu_ctlr: u64,
    group1: bool,
    waker: u32,
    sre: u64,
    running: Vec<(usize, u8)>,
    pub acknowledges: u64,
}
impl Default for Gic {
    fn default() -> Self {
        Self { ctlr: 0x40, group: [false; NIRQ], enabled: [false; NIRQ], pending: [false; NIRQ],
            level: [false; NIRQ], active: [false; NIRQ], edge: [false; NIRQ], priority: [0xa0; NIRQ],
            route: [0; NIRQ], pmr: 0, cpu_ctlr: 0, group1: false, waker: 6, sre: 7,
            running: Vec::new(), acknowledges: 0 }
    }
}
impl Gic {
    pub fn set_line(&mut self, id: usize, high: bool) {
        if self.edge[id] && high && !self.level[id] { self.pending[id] = true; }
        self.level[id] = high;
    }
    fn highest(&self) -> Option<usize> {
        if self.ctlr & 3 == 0 || !self.group1 || self.waker & 2 != 0 { return None; }
        let running = self.running.last().map_or(256, |(_, p)| *p as u16);
        (0..NIRQ).filter(|&i| self.enabled[i] && self.group[i] && !self.active[i]
            && (self.pending[i] || (self.level[i] && !self.edge[i]))
            && self.priority[i] < self.pmr && (self.priority[i] as u16) < running
            && (i < 32 || self.route[i] == 0 || self.route[i] & (1 << 31) != 0))
            .min_by_key(|&i| (self.priority[i], i))
    }
    pub fn irq(&self) -> bool { self.highest().is_some() }
    pub fn sys_read(&mut self, reg: u32) -> Option<u64> {
        Some(match reg {
            IAR => match self.highest() {
                Some(i) => { self.pending[i] = false; self.active[i] = true; self.running.push((i, self.priority[i])); self.acknowledges += 1; i as u64 }
                None => 1023,
            },
            PMR => self.pmr as u64,
            CTLR => (4 << 8) | self.cpu_ctlr, // five priority bits, no RSS/extended INTIDs
            SRE => self.sre,
            GROUP1 => self.group1 as u64,
            r if r == key(3,0,12,12,2) => self.highest().map_or(1023, |i| i as u64), // HPPIR1
            r if r == key(3,0,12,11,3) => self.running.last().map_or(0xff, |(_, p)| *p as u64), // RPR
            r if r == key(3,0,12,12,3) => 0, // BPR1: minimum split
            r if r == key(3,0,12,9,0) => self.running.iter().fold(0, |v,(_,p)| v | (1u64 << (p >> 3))),
            r if (key(3,0,12,8,4)..=key(3,0,12,8,7)).contains(&r) || (key(3,0,12,9,1)..=key(3,0,12,9,3)).contains(&r) => 0,
            r if r == key(3,0,12,8,0) => 1023, // no Group 0 interrupts
            _ => return None,
        })
    }
    pub fn sys_write(&mut self, reg: u32, v: u64) -> bool {
        match reg {
            PMR => self.pmr = v as u8 & 0xf8,
            SRE => self.sre = v | 7,
            CTLR => self.cpu_ctlr = v & 2,
            GROUP1 => self.group1 = v & 1 != 0,
            EOIR => {
                let i = (v & 0xff_ffff) as usize;
                if self.running.last().is_some_and(|&(id, _)| id == i) {
                    self.running.pop();
                    if self.cpu_ctlr & 2 == 0 { self.active[i] = false; }
                }
            }
            DIR => { let i = (v & 0xff_ffff) as usize; if i < NIRQ { self.active[i] = false; } }
            r if r == key(3,0,12,11,5) => { // SGI1R, target PE affinity 0
                let affinity = v & (0xff << 16 | 0xff << 32 | 0xff << 48);
                if v & (1 << 40) == 0 && affinity == 0 && v & 1 != 0 { self.pending[((v >> 24) & 15) as usize] = true; }
            }
            r if r == key(3,0,12,9,0) => self.running.retain(|(_,p)| v & (1u64 << (*p >> 3)) != 0),
            r if r == key(3,0,12,12,3) || (key(3,0,12,8,4)..=key(3,0,12,8,7)).contains(&r) || (key(3,0,12,9,1)..=key(3,0,12,9,3)).contains(&r) => {},
            _ => return false,
        }
        true
    }
    fn bits(&self, base: usize, which: u64) -> u32 {
        (0..32).fold(0, |v, n| { let i = base+n;
            if i >= NIRQ { return v; }
            let bit = match which { 0x80 => self.group[i], 0x100 | 0x180 => self.enabled[i],
                0x200 | 0x280 => self.pending[i] || (self.level[i] && !self.edge[i]),
                0x300 | 0x380 => self.active[i], _ => false };
            v | (u32::from(bit) << n)
        })
    }
    fn bank_read(&self, off: u64) -> u32 {
        match off {
            0x80..=0xbf | 0x100..=0x3ff => { let kind = off & !0x7f; self.bits(((off-kind)/4*32) as usize,kind) }
            0x400..=0x5df => { let i = (off-0x400) as usize; (0..4).fold(0, |v,n| v | ((self.priority[i+n] as u32) << (n*8))) }
            0xc00..=0xc77 => { let i = ((off-0xc00)/4*16) as usize; (0..16).fold(0, |v,n| v | (u32::from(self.edge[i+n]) << (n*2+1))) }
            _ => 0,
        }
    }
    fn bank_write(&mut self, off: u64, v: u32) {
        match off {
            0x80..=0xbf | 0x100..=0x3ff => {
                let kind = off & !0x7f;
                let base = ((off-kind)/4*32) as usize;
                for n in 0..32 { let i=base+n; if i>=NIRQ { break; }
                    let set=v & (1<<n)!=0;
                    match kind { 0x80 => self.group[i]=set,
                        0x100 if set => self.enabled[i]=true, 0x180 if set => self.enabled[i]=false,
                        0x200 if set => self.pending[i]=true, 0x280 if set => self.pending[i]=false,
                        0x300 if set => self.active[i]=true, 0x380 if set => self.active[i]=false, _=>{} }
                }
            }
            0x400..=0x5df => { let i=(off-0x400) as usize; for n in 0..4 {self.priority[i+n]=(v>>(n*8)) as u8 & 0xf8;} }
            0xc00..=0xc77 => { let i=((off-0xc00)/4*16) as usize; for n in 0..16 {self.edge[i+n]=v & (2<<(n*2))!=0;} }
            _=>{}
        }
    }
    pub fn dist_read(&self, off: u64) -> u32 {
        match off {
            0 => self.ctlr,
            4 => (NIRQ as u32 / 32 - 1) | (9 << 19), // 10-bit INTIDs; no LPIs
            8 => 0x0200_043b,
            0xffe8 => 0x30,
            0x6000..=0x6eff => { let i=((off-0x6000)/8) as usize; if i<NIRQ { (self.route[i] >> ((off&4)*8)) as u32 } else {0} }
            _ => self.bank_read(off),
        }
    }
    pub fn dist_write(&mut self, off: u64, v: u32) {
        match off {
            0 => self.ctlr = (v & 0x37) | 0x40,
            0x6000..=0x6eff => { let i=((off-0x6000)/8) as usize; let shift=(off&4)*8; if i<NIRQ {self.route[i]=(self.route[i]&!(0xffff_ffff<<shift))|((v as u64)<<shift);} }
            _ => self.bank_write(off,v),
        }
    }
    pub fn redist_read(&self, off: u64) -> u32 {
        match off { 0=>0, 4=>0x0200_043b, 8=>0x10, 12=>0, 0x14=>self.waker, 0xffe8=>0x30,
            0x10000..=0x1ffff => self.bank_read(off-0x10000), _=>0 }
    }
    pub fn redist_write(&mut self, off: u64,v: u32) {
        match off { 0x14=>self.waker=if v&2!=0 {6}else{0},
            0x10000..=0x1ffff=>self.bank_write(off-0x10000,v), _=>{} }
    }
}
