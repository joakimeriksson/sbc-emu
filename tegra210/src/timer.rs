//! The Tegra210 timers (`nvidia,tegra210-timer`): TIMERUS, a free-running microsecond
//! counter, and the countdown timers TMR1-TMR14.
//!
//! Linux ticks on TMR10-TMR13, one per core (clockevent rating 460, above the arch timer's:
//! on this SoC the A57s' arch timer does not survive a core power cycle). It counts them at
//! the timer clock's rate and writes TIMERUS_USEC_CFG for that rate ((low + 1) / (high + 1)
//! MHz, 0x045f for 19.2 MHz), so the model takes its count rate from that register.
//!
//! A timer register pair: PTV (+0: bit 31 enable, bit 30 periodic, bits 28:0 count - 1) and
//! PCR (+4: reads the count left and bit 31 the interrupt; writing bit 30 clears it).

pub const SIZE: u64 = 0x400;

const USEC_CNTR: u64 = 0x10;
const USEC_CFG: u64 = 0x14;
const PTV_EN: u32 = 1 << 31;
const PTV_PER: u32 = 1 << 30;
const PCR_INTR: u32 = 1 << 31;
const PCR_INTR_CLR: u32 = 1 << 30;

/// Index of the timer whose PTV/PCR pair holds `offset`: TMR1 (0x00), TMR2 (0x08), then
/// one every 8 bytes from 0x50 to 0xa8 (Linux's per-core timers are at 0x90-0xa8).
fn timer_index(offset: u64) -> Option<usize> {
    match offset & !7 {
        0x00 => Some(0),
        0x08 => Some(1),
        o @ 0x50..=0xa8 => Some(2 + ((o - 0x50) / 8) as usize),
        _ => None,
    }
}

/// GIC interrupt of each timer: TMR10-TMR13 (0x90-0xa8, the per-core clockevents) are SPIs
/// 176-179 in the device tree; the others are not used by Linux.
pub fn irq_of(index: usize) -> Option<usize> {
    match index {
        10..=13 => Some(32 + 176 + index - 10),
        _ => None,
    }
}
pub const TIMERS: usize = 14;

#[derive(Clone, Copy, Default)]
struct Tmr {
    ptv: u32,
    /// virtual time (ns) the count expires
    deadline: Option<u64>,
    /// period of a periodic timer (ns)
    period: u64,
    irq: bool,
}

pub struct Timers {
    usec_cfg: u32,
    tmr: [Tmr; TIMERS],
    other: std::collections::HashMap<u64, u32>,
}

impl Default for Timers {
    fn default() -> Self {
        Self { usec_cfg: 0, tmr: [Tmr::default(); TIMERS], other: Default::default() }
    }
}

impl Timers {
    /// Nanoseconds per count of the TMR timers, from TIMERUS_USEC_CFG (1 MHz until set).
    fn ns_of(&self, counts: u64) -> u64 {
        let (lo, hi) = ((self.usec_cfg & 0xff) as u64 + 1, ((self.usec_cfg >> 8) & 0xff) as u64 + 1);
        // rate = lo / hi MHz: ns = counts * 1000 * hi / lo
        (counts as u128 * 1000 * hi as u128 / lo as u128).min(u64::MAX as u128) as u64
    }

    pub fn read(&self, offset: u64, now: u64) -> u32 {
        let o = offset & !3;
        match o {
            USEC_CNTR => (now / 1000) as u32,
            USEC_CFG => self.usec_cfg,
            _ => match timer_index(o) {
                Some(i) if o & 4 == 0 => self.tmr[i].ptv,
                Some(i) => {
                    let t = &self.tmr[i];
                    let left = t.deadline.map_or(0, |d| {
                        let (lo, hi) = ((self.usec_cfg & 0xff) as u64 + 1, ((self.usec_cfg >> 8) & 0xff) as u64 + 1);
                        (d.saturating_sub(now) as u128 * lo as u128 / (1000 * hi as u128)) as u32
                    });
                    left & 0x1fff_ffff | if t.irq { PCR_INTR } else { 0 }
                }
                None => self.other.get(&o).copied().unwrap_or(0),
            },
        }
    }

    pub fn write(&mut self, offset: u64, value: u32, now: u64) {
        let o = offset & !3;
        match o {
            USEC_CNTR => {}
            USEC_CFG => self.usec_cfg = value,
            _ => match timer_index(o) {
                Some(i) if o & 4 == 0 => {
                    let ticks = (value & 0x1fff_ffff) as u64 + 1;
                    let ns = self.ns_of(ticks).max(1);
                    let t = &mut self.tmr[i];
                    t.ptv = value;
                    t.period = if value & PTV_PER != 0 { ns } else { 0 };
                    t.deadline = (value & PTV_EN != 0).then_some(now + ns);
                }
                Some(i) => {
                    if value & PCR_INTR_CLR != 0 {
                        self.tmr[i].irq = false;
                    }
                }
                None => {
                    self.other.insert(o, value);
                }
            },
        }
    }

    /// Expire the timers due by `now`.
    pub fn service(&mut self, now: u64) {
        for t in self.tmr.iter_mut() {
            while let Some(d) = t.deadline.filter(|&d| d <= now) {
                t.irq = true;
                if t.period > 0 {
                    // a periodic timer that fell far behind skips the missed periods
                    let next = d + t.period;
                    t.deadline = Some(if next <= now { now + t.period } else { next });
                } else {
                    t.deadline = None;
                    t.ptv &= !PTV_EN;
                }
            }
        }
    }

    pub fn irq(&self, index: usize) -> bool {
        self.tmr[index].irq
    }

    pub fn next_event(&self) -> Option<u64> {
        self.tmr.iter().filter_map(|t| t.deadline).min()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn oneshot_and_periodic_at_the_configured_rate() {
        let mut t = Timers::default();
        t.write(USEC_CFG, 0x045f, 0); // 19.2 MHz
        // TMR10 (0x90): 1920 counts = 100 us
        t.write(0x90, PTV_EN | (1920 - 1), 0);
        t.service(99_999);
        assert!(!t.irq(10));
        t.service(100_000);
        assert!(t.irq(10));
        assert_eq!(t.next_event(), None);
        t.write(0x94, PCR_INTR_CLR, 100_000);
        assert!(!t.irq(10));
        t.write(0x90, PTV_EN | PTV_PER | (192 - 1), 200_000); // every 10 us
        t.service(210_000);
        assert!(t.irq(10));
        assert_eq!(t.next_event(), Some(220_000));
        assert_eq!(t.read(USEC_CNTR, 5_000_000), 5000);
        assert_eq!(irq_of(10), Some(32 + 176));
    }
}
