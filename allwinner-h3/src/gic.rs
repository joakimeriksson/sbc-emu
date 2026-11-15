const NIRQ: usize = 256;
pub const SPURIOUS: u32 = 1023;

/// One-CPU GIC-400/GICv2 distributor and CPU interface.
///
/// SGIs and multi-CPU routing are deliberately omitted from this first H3 slice.
/// PPI/SPI level and edge inputs, priorities, acknowledge and EOI are modelled.
pub struct Gic {
    dist_enable: bool,
    cpu_control: u32,
    priority_mask: u8,
    enabled: [bool; NIRQ],
    pending: [bool; NIRQ],
    active: [bool; NIRQ],
    level: [bool; NIRQ],
    edge: [bool; NIRQ],
    priority: [u8; NIRQ],
    target: [u8; NIRQ],
    running: Vec<(usize, u8)>,
}

impl Default for Gic {
    fn default() -> Self {
        let mut edge = [false; NIRQ];
        edge[..16].fill(true);
        Self {
            dist_enable: false,
            cpu_control: 0,
            priority_mask: 0,
            enabled: [false; NIRQ],
            pending: [false; NIRQ],
            active: [false; NIRQ],
            level: [false; NIRQ],
            edge,
            priority: [0; NIRQ],
            target: [1; NIRQ],
            running: Vec::new(),
        }
    }
}

impl Gic {
    pub fn set_level(&mut self, irq: usize, high: bool) {
        assert!((16..NIRQ).contains(&irq));
        if self.edge[irq] && high && !self.level[irq] {
            self.pending[irq] = true;
        }
        self.level[irq] = high;
    }

    fn is_pending(&self, irq: usize) -> bool {
        self.pending[irq] || (!self.edge[irq] && self.level[irq])
    }

    fn best(&self) -> Option<(usize, u8)> {
        (16..NIRQ)
            .filter(|&irq| {
                self.enabled[irq]
                    && !self.active[irq]
                    && self.is_pending(irq)
                    && self.priority[irq] < self.priority_mask
                    && (irq < 32 || self.target[irq] & 1 != 0)
            })
            .map(|irq| (irq, self.priority[irq]))
            .min_by_key(|&(irq, priority)| (priority, irq))
    }

    fn visible(&self) -> Option<(usize, u8)> {
        self.best().filter(|&(_, priority)| {
            self.running
                .last()
                .is_none_or(|&(_, running)| priority < running)
        })
    }

    pub fn irq_out(&self) -> bool {
        self.dist_enable && self.cpu_control & 1 != 0 && self.visible().is_some()
    }

    fn acknowledge(&mut self) -> u32 {
        if !self.dist_enable || self.cpu_control & 1 == 0 {
            return SPURIOUS;
        }
        let Some((irq, priority)) = self.visible() else {
            return SPURIOUS;
        };
        self.pending[irq] = false;
        self.active[irq] = true;
        self.running.push((irq, priority));
        irq as u32
    }

    fn end_of_interrupt(&mut self, value: u32) {
        let irq = (value & 0x3ff) as usize;
        if irq >= NIRQ {
            return;
        }
        if let Some(position) = self.running.iter().rposition(|&(id, _)| id == irq) {
            self.running.remove(position);
        }
        // EOImode zero: priority drop and deactivation happen together.
        if self.cpu_control & (1 << 9) == 0 {
            self.active[irq] = false;
        }
    }

    fn bits(&self, base: usize, bit: impl Fn(&Self, usize) -> bool) -> u32 {
        (0..32).fold(0, |value, index| {
            let irq = base + index;
            value | u32::from(irq < NIRQ && bit(self, irq)) << index
        })
    }

    pub fn dist_read(&self, offset: u64) -> u32 {
        let offset = offset as usize;
        match offset {
            0x000 => u32::from(self.dist_enable),
            0x004 => (NIRQ as u32 / 32) - 1, // 256 lines, one CPU
            0x008 => 0x0200_143b,            // GIC-400 IIDR
            0x100..=0x17c | 0x180..=0x1fc => {
                self.bits((offset & 0x7f) * 8, |gic, irq| gic.enabled[irq])
            }
            0x200..=0x27c | 0x280..=0x2fc => {
                self.bits((offset & 0x7f) * 8, |gic, irq| gic.is_pending(irq))
            }
            0x300..=0x37c | 0x380..=0x3fc => {
                self.bits((offset & 0x7f) * 8, |gic, irq| gic.active[irq])
            }
            0x400..=0x7fc => (0..4).fold(0, |value, index| {
                let irq = offset - 0x400 + index;
                value | (self.priority.get(irq).copied().unwrap_or(0) as u32) << (index * 8)
            }),
            0x800..=0xbfc => (0..4).fold(0, |value, index| {
                let irq = offset - 0x800 + index;
                let target = if irq < 32 {
                    1
                } else {
                    self.target.get(irq).copied().unwrap_or(0)
                };
                value | (target as u32) << (index * 8)
            }),
            0xc00..=0xcfc => {
                let base = (offset - 0xc00) * 4;
                (0..16).fold(0, |value, index| {
                    let irq = base + index;
                    value | u32::from(irq < NIRQ && self.edge[irq]) << (index * 2 + 1)
                })
            }
            0xfe8 => 0x2b, // ICPIDR2: GICv2
            _ => 0,
        }
    }

    pub fn dist_write(&mut self, offset: u64, value: u32) {
        let offset = offset as usize;
        let base = match offset {
            0x000 => {
                self.dist_enable = value & 1 != 0;
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
                if irq < 16 || irq >= NIRQ || value & (1 << index) == 0 {
                    continue;
                }
                match operation {
                    0 => self.enabled[irq] = true,
                    1 => self.enabled[irq] = false,
                    2 => self.pending[irq] = true,
                    3 => self.pending[irq] = false,
                    4 => self.active[irq] = true,
                    5 => self.active[irq] = false,
                    _ => unreachable!(),
                }
            }
            return;
        }
        match offset {
            0x400..=0x7fc => {
                for index in 0..4 {
                    let irq = offset - 0x400 + index;
                    if let Some(priority) = self.priority.get_mut(irq) {
                        *priority = (value >> (index * 8)) as u8;
                    }
                }
            }
            0x800..=0xbfc => {
                for index in 0..4 {
                    let irq = offset - 0x800 + index;
                    if irq >= 32 {
                        if let Some(target) = self.target.get_mut(irq) {
                            *target = (value >> (index * 8)) as u8;
                        }
                    }
                }
            }
            0xc00..=0xcfc => {
                let base = (offset - 0xc00) * 4;
                for index in 0..16 {
                    let irq = base + index;
                    if (16..NIRQ).contains(&irq) {
                        self.edge[irq] = value & (1 << (index * 2 + 1)) != 0;
                    }
                }
            }
            _ => {}
        }
    }

    pub fn cpu_read(&mut self, offset: u64) -> u32 {
        match offset {
            0x00 => self.cpu_control,
            0x04 => self.priority_mask as u32,
            0x0c => self.acknowledge(),
            0x14 => self
                .running
                .last()
                .map(|&(_, priority)| priority as u32)
                .unwrap_or(0xff),
            0x18 => self.best().map(|(irq, _)| irq as u32).unwrap_or(SPURIOUS),
            0xfc => 0x0202_143b,
            _ => 0,
        }
    }

    pub fn cpu_write(&mut self, offset: u64, value: u32) {
        match offset {
            0x00 => self.cpu_control = value & 0x201,
            0x04 => self.priority_mask = value as u8,
            0x10 => self.end_of_interrupt(value),
            0x1000 => {
                let irq = (value & 0x3ff) as usize;
                if irq < NIRQ {
                    self.active[irq] = false;
                }
            }
            _ => {}
        }
    }
}
