const NIRQ: usize = 256;
pub const NCPU: usize = 4;
pub const SPURIOUS: u32 = 1023;

/// Per-core state: the banked SGI/PPI lines (0-31) and the CPU interface.
#[derive(Clone, Default)]
struct Core {
    enabled: [bool; 32],
    pending: [bool; 32],
    active: [bool; 32],
    level: [bool; 32],
    priority: [u8; 32],
    /// Requesting core of each pending SGI (reported in GICC_IAR bits 12:10). Requests for
    /// the same SGI from several cores merge into one pending interrupt (the last source
    /// is reported); Linux IPI handling does not depend on the source.
    sgi_source: [u8; 16],
    control: u32,
    priority_mask: u8,
    running: Vec<(usize, u8)>,
}

/// GIC-400 (GICv2) distributor and four CPU interfaces, as on the H3.
///
/// SGIs and PPIs are banked per core; SPIs are shared and delivered to the cores in
/// their ITARGETSR byte (1-of-N: whichever targeted core acknowledges first takes it).
/// Register accesses act for core `current`, which the machine sets before running a core.
pub struct Gic {
    dist_enable: bool,
    cores: Vec<Core>,
    enabled: [bool; NIRQ],
    pending: [bool; NIRQ],
    active: [bool; NIRQ],
    level: [bool; NIRQ],
    edge: [bool; NIRQ],
    priority: [u8; NIRQ],
    target: [u8; NIRQ],
    /// The core whose view the register interface presents.
    pub current: usize,
}

impl Default for Gic {
    fn default() -> Self {
        Self {
            dist_enable: false,
            cores: vec![Core::default(); NCPU],
            enabled: [false; NIRQ],
            pending: [false; NIRQ],
            active: [false; NIRQ],
            level: [false; NIRQ],
            edge: [false; NIRQ],
            priority: [0; NIRQ],
            target: [1; NIRQ],
            current: 0,
        }
    }
}

impl Gic {
    /// A core powered on again (PSCI CPU_ON) starts with no interrupt in progress.
    pub fn reset_cpu_interface(&mut self, cpu: usize) {
        let core = &mut self.cores[cpu];
        core.running.clear();
        core.active = [false; 32];
        core.control = 0;
    }

    /// Drive a shared (SPI) line, or a PPI of core 0.
    pub fn set_level(&mut self, irq: usize, high: bool) {
        assert!((16..NIRQ).contains(&irq));
        if irq < 32 {
            self.set_ppi(0, irq, high);
            return;
        }
        if self.edge[irq] && high && !self.level[irq] {
            self.pending[irq] = true;
        }
        self.level[irq] = high;
    }

    /// Drive a private peripheral interrupt (16-31) of one core (level-sensitive).
    pub fn set_ppi(&mut self, cpu: usize, irq: usize, high: bool) {
        assert!((16..32).contains(&irq));
        self.cores[cpu].level[irq] = high;
    }

    fn enabled(&self, cpu: usize, irq: usize) -> bool {
        // GIC-400: SGIs are permanently enabled (their enable bits read as one).
        irq < 16 || if irq < 32 { self.cores[cpu].enabled[irq] } else { self.enabled[irq] }
    }

    fn is_active(&self, cpu: usize, irq: usize) -> bool {
        if irq < 32 { self.cores[cpu].active[irq] } else { self.active[irq] }
    }

    fn is_pending(&self, cpu: usize, irq: usize) -> bool {
        if irq < 32 {
            let core = &self.cores[cpu];
            core.pending[irq] || irq >= 16 && core.level[irq]
        } else {
            self.pending[irq] || (!self.edge[irq] && self.level[irq])
        }
    }

    fn priority_of(&self, cpu: usize, irq: usize) -> u8 {
        if irq < 32 { self.cores[cpu].priority[irq] } else { self.priority[irq] }
    }

    fn best(&self, cpu: usize) -> Option<(usize, u8)> {
        let core = &self.cores[cpu];
        (0..NIRQ)
            .filter(|&irq| {
                self.enabled(cpu, irq)
                    && !self.is_active(cpu, irq)
                    && self.is_pending(cpu, irq)
                    && (irq < 32 || self.target[irq] & (1 << cpu) != 0)
            })
            .map(|irq| (irq, self.priority_of(cpu, irq)))
            .filter(|&(_, priority)| priority < core.priority_mask)
            .min_by_key(|&(irq, priority)| (priority, irq))
    }

    fn visible(&self, cpu: usize) -> Option<(usize, u8)> {
        self.best(cpu).filter(|&(_, priority)| {
            self.cores[cpu].running.last().is_none_or(|&(_, running)| priority < running)
        })
    }

    /// Interrupt output to core 0 (single-core users).
    pub fn irq_out(&self) -> bool {
        self.irq_out_for(0)
    }

    pub fn irq_out_for(&self, cpu: usize) -> bool {
        self.dist_enable && self.cores[cpu].control & 1 != 0 && self.visible(cpu).is_some()
    }

    fn acknowledge(&mut self, cpu: usize) -> u32 {
        if !self.dist_enable || self.cores[cpu].control & 1 == 0 {
            return SPURIOUS;
        }
        let Some((irq, priority)) = self.visible(cpu) else {
            return SPURIOUS;
        };
        let core = &mut self.cores[cpu];
        let mut value = irq as u32;
        if irq < 32 {
            core.pending[irq] = false;
            core.active[irq] = true;
            if irq < 16 {
                value |= (core.sgi_source[irq] as u32) << 10;
            }
        } else {
            self.pending[irq] = false;
            self.active[irq] = true;
        }
        core.running.push((irq, priority));
        value
    }

    fn end_of_interrupt(&mut self, cpu: usize, value: u32) {
        let irq = (value & 0x3ff) as usize;
        if irq >= NIRQ {
            return;
        }
        let core = &mut self.cores[cpu];
        if let Some(position) = core.running.iter().rposition(|&(id, _)| id == irq) {
            core.running.remove(position);
        }
        // EOImode zero: priority drop and deactivation happen together.
        if core.control & (1 << 9) == 0 {
            self.deactivate(cpu, irq);
        }
    }

    fn deactivate(&mut self, cpu: usize, irq: usize) {
        if irq < 32 {
            self.cores[cpu].active[irq] = false;
        } else if irq < NIRQ {
            self.active[irq] = false;
        }
    }

    fn send_sgi(&mut self, from: usize, value: u32) {
        let sgi = (value & 0xf) as usize;
        let targets = match (value >> 24) & 3 {
            0 => (value >> 16) & 0xff,
            1 => 0xff & !(1 << from),
            2 => 1 << from,
            _ => 0,
        };
        for cpu in 0..NCPU {
            if targets & (1 << cpu) != 0 {
                self.cores[cpu].pending[sgi] = true;
                self.cores[cpu].sgi_source[sgi] = from as u8;
            }
        }
    }

    fn bits(&self, cpu: usize, base: usize, bit: impl Fn(&Self, usize, usize) -> bool) -> u32 {
        (0..32).fold(0, |value, index| {
            let irq = base + index;
            value | u32::from(irq < NIRQ && bit(self, cpu, irq)) << index
        })
    }

    pub fn dist_read(&self, offset: u64) -> u32 {
        let cpu = self.current;
        let offset = offset as usize;
        match offset {
            0x000 => u32::from(self.dist_enable),
            0x004 => ((NIRQ as u32 / 32) - 1) | ((NCPU as u32 - 1) << 5),
            0x008 => 0x0200_143b, // GIC-400 IIDR
            0x100..=0x17c | 0x180..=0x1fc => {
                self.bits(cpu, (offset & 0x7f) * 8, |gic, c, irq| gic.enabled(c, irq))
            }
            0x200..=0x27c | 0x280..=0x2fc => {
                self.bits(cpu, (offset & 0x7f) * 8, |gic, c, irq| gic.is_pending(c, irq))
            }
            0x300..=0x37c | 0x380..=0x3fc => {
                self.bits(cpu, (offset & 0x7f) * 8, |gic, c, irq| gic.is_active(c, irq))
            }
            0x400..=0x7fc => (0..4).fold(0, |value, index| {
                let irq = offset - 0x400 + index;
                let priority = if irq < NIRQ { self.priority_of(cpu, irq) } else { 0 };
                value | (priority as u32) << (index * 8)
            }),
            0x800..=0xbfc => (0..4).fold(0, |value, index| {
                let irq = offset - 0x800 + index;
                let target = if irq < 32 {
                    1 << cpu
                } else {
                    self.target.get(irq).copied().unwrap_or(0)
                };
                value | (target as u32) << (index * 8)
            }),
            0xc00..=0xcfc => {
                let base = (offset - 0xc00) * 4;
                (0..16).fold(0, |value, index| {
                    let irq = base + index;
                    let edge = irq < 16 || (32..NIRQ).contains(&irq) && self.edge[irq];
                    value | u32::from(edge) << (index * 2 + 1)
                })
            }
            0xfe8 => 0x2b, // ICPIDR2: GICv2
            _ => 0,
        }
    }

    pub fn dist_write(&mut self, offset: u64, value: u32) {
        let cpu = self.current;
        let offset = offset as usize;
        let base = match offset {
            0x000 => {
                self.dist_enable = value & 1 != 0;
                return;
            }
            0xf00 => {
                self.send_sgi(cpu, value);
                return;
            }
            0x100..=0x17c => Some(((offset - 0x100) * 8, 0)),
            0x180..=0x1fc => Some(((offset - 0x180) * 8, 1)),
            0x200..=0x27c => Some(((offset - 0x200) * 8, 2)),
            0x280..=0x2fc => Some(((offset - 0x280) * 8, 3)),
            0x300..=0x37c => Some(((offset - 0x300) * 8, 4)),
            0x380..=0x3fc => Some(((offset - 0x380) * 8, 5)),
            _ => None,
        };
        if let Some((base, operation)) = base {
            for index in 0..32 {
                let irq = base + index;
                if irq >= NIRQ || value & (1 << index) == 0 {
                    continue;
                }
                // SGIs are always enabled on the GIC-400; pending SGIs come from GICD_SGIR.
                if irq < 16 && operation < 4 {
                    continue;
                }
                let (enabled, pending, active) = if irq < 32 {
                    let core = &mut self.cores[cpu];
                    (&mut core.enabled[irq], &mut core.pending[irq], &mut core.active[irq])
                } else {
                    (&mut self.enabled[irq], &mut self.pending[irq], &mut self.active[irq])
                };
                match operation {
                    0 => *enabled = true,
                    1 => *enabled = false,
                    2 => *pending = true,
                    3 => *pending = false,
                    4 => *active = true,
                    _ => *active = false,
                }
            }
            return;
        }
        match offset {
            0x400..=0x7fc => {
                for index in 0..4 {
                    let irq = offset - 0x400 + index;
                    let priority = (value >> (index * 8)) as u8;
                    if irq < 32 {
                        self.cores[cpu].priority[irq] = priority;
                    } else if irq < NIRQ {
                        self.priority[irq] = priority;
                    }
                }
            }
            0x800..=0xbfc => {
                for index in 0..4 {
                    let irq = offset - 0x800 + index;
                    if (32..NIRQ).contains(&irq) {
                        self.target[irq] = (value >> (index * 8)) as u8 & ((1 << NCPU) - 1);
                    }
                }
            }
            0xc00..=0xcfc => {
                let base = (offset - 0xc00) * 4;
                for index in 0..16 {
                    let irq = base + index;
                    if (32..NIRQ).contains(&irq) {
                        self.edge[irq] = value & (1 << (index * 2 + 1)) != 0;
                    }
                }
            }
            _ => {}
        }
    }

    pub fn cpu_read(&mut self, offset: u64) -> u32 {
        let cpu = self.current;
        match offset {
            0x00 => self.cores[cpu].control,
            0x04 => self.cores[cpu].priority_mask as u32,
            0x0c => self.acknowledge(cpu),
            0x14 => self.cores[cpu].running.last().map(|&(_, p)| p as u32).unwrap_or(0xff),
            0x18 => self.best(cpu).map(|(irq, _)| irq as u32).unwrap_or(SPURIOUS),
            0xfc => 0x0202_143b,
            _ => 0,
        }
    }

    pub fn cpu_write(&mut self, offset: u64, value: u32) {
        let cpu = self.current;
        match offset {
            0x00 => self.cores[cpu].control = value & 0x201,
            0x04 => self.cores[cpu].priority_mask = value as u8,
            0x10 => self.end_of_interrupt(cpu, value),
            0x1000 => self.deactivate(cpu, (value & 0x3ff) as usize),
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn enable_cpu(gic: &mut Gic, cpu: usize) {
        gic.current = cpu;
        gic.cpu_write(0x00, 1);
        gic.cpu_write(0x04, 0xff);
    }

    #[test]
    fn sgis_reach_the_listed_cores_with_their_source() {
        let mut gic = Gic::default();
        gic.dist_write(0x000, 1);
        for cpu in 0..NCPU {
            enable_cpu(&mut gic, cpu);
        }
        gic.current = 0;
        gic.dist_write(0xf00, 0b0110 << 16 | 3); // SGI 3 to cores 1 and 2
        assert!(!gic.irq_out_for(0) && gic.irq_out_for(1) && gic.irq_out_for(2));
        gic.current = 2;
        assert_eq!(gic.cpu_read(0x0c), 3); // from core 0
        gic.cpu_write(0x10, 3);
        assert!(!gic.irq_out_for(2) && gic.irq_out_for(1));
        gic.current = 3;
        gic.dist_write(0xf00, 1 << 24 | 5); // all but self
        assert!(gic.irq_out_for(0) && !gic.irq_out_for(3));
        gic.current = 0;
        assert_eq!(gic.cpu_read(0x0c), 5 | 3 << 10);
    }

    #[test]
    fn ppis_are_banked_and_spis_follow_targets() {
        let mut gic = Gic::default();
        gic.dist_write(0x000, 1);
        for cpu in 0..NCPU {
            enable_cpu(&mut gic, cpu);
            gic.dist_write(0x100, 1 << 27); // each core enables its own PPI 27
        }
        gic.set_ppi(2, 27, true);
        assert!(gic.irq_out_for(2) && !gic.irq_out_for(0));
        gic.dist_write(0x104, 1 << (40 - 32));
        gic.dist_write(0x828, 1 << 3); // SPI 40 -> core 3
        gic.set_level(40, true);
        assert!(gic.irq_out_for(3) && !gic.irq_out_for(1));
    }
}
