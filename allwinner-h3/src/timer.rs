//! H3 timer block at 0x01c2_0c00 (`allwinner,sun8i-a23-timer`): two 32-bit down-counters
//! clocked from OSC24M through a power-of-two prescaler, with per-timer interrupt status.
//! Time is the CPU's 24 MHz architectural count, so both views stay in step.

#[derive(Clone, Copy, Default)]
struct Channel {
    control: u32,
    interval: u32,
    /// Counter value at `start`.
    base: u32,
    start: u64,
    /// One-shot timer that reached zero and stopped.
    expired: bool,
}

impl Channel {
    fn enabled(&self) -> bool {
        self.control & 1 != 0
    }

    fn one_shot(&self) -> bool {
        self.control & (1 << 7) != 0
    }

    fn prescale(&self) -> u64 {
        1 << ((self.control >> 4) & 7)
    }

    fn elapsed(&self, now: u64) -> u64 {
        now.saturating_sub(self.start) / self.prescale()
    }

    fn current(&self, now: u64) -> u32 {
        if !self.enabled() || self.expired {
            return self.base;
        }
        let elapsed = self.elapsed(now);
        if elapsed <= self.base as u64 {
            return self.base - elapsed as u32;
        }
        if self.one_shot() {
            0
        } else {
            // Continuous mode reloads the interval after reaching zero.
            let period = self.interval as u64 + 1;
            let after = (elapsed - self.base as u64 - 1) % period;
            (self.interval as u64 - after) as u32
        }
    }

    /// The tick at which the counter next reaches zero, if it is running.
    fn next_expiry(&self, now: u64) -> Option<u64> {
        if !self.enabled() || self.expired {
            return None;
        }
        let elapsed = self.elapsed(now);
        let ticks = if elapsed <= self.base as u64 {
            self.base as u64 - elapsed
        } else if self.one_shot() {
            return None;
        } else {
            self.current(now) as u64
        };
        Some(now + ticks.max(1) * self.prescale())
    }
}

#[derive(Default)]
pub struct Timer {
    irq_enable: u32,
    irq_status: u32,
    channels: [Channel; 2],
    /// Last time `update` observed each channel, to latch expiries between reads.
    seen: [u64; 2],
}

impl Timer {
    /// Latch interrupt status for counters that reached zero by `now`.
    pub fn update(&mut self, now: u64) {
        for index in 0..2 {
            let channel = &mut self.channels[index];
            if !channel.enabled() || channel.expired {
                self.seen[index] = now;
                continue;
            }
            let start = self.seen[index].max(channel.start);
            let crossings_before = Self::crossings(channel, start);
            let crossings_now = Self::crossings(channel, now);
            if crossings_now > crossings_before {
                self.irq_status |= 1 << index;
                if channel.one_shot() {
                    channel.expired = true;
                    channel.base = 0;
                    channel.control &= !1;
                }
            }
            self.seen[index] = now;
        }
    }

    /// Number of times the counter has passed zero since it was (re)started.
    fn crossings(channel: &Channel, now: u64) -> u64 {
        let elapsed = channel.elapsed(now);
        if elapsed <= channel.base as u64 {
            0
        } else {
            1 + (elapsed - channel.base as u64 - 1) / (channel.interval as u64 + 1)
        }
    }

    pub fn irq(&self, index: usize) -> bool {
        self.irq_enable & self.irq_status & (1 << index) != 0
    }

    /// Earliest tick at which an enabled timer interrupt could fire.
    pub fn next_deadline(&self, now: u64) -> Option<u64> {
        (0..2)
            .filter(|&i| self.irq_enable & (1 << i) != 0)
            .filter_map(|i| self.channels[i].next_expiry(now))
            .min()
    }

    pub fn read(&mut self, offset: u64, now: u64) -> u32 {
        self.update(now);
        match offset {
            0x00 => self.irq_enable,
            0x04 => self.irq_status,
            0x10 | 0x20 => self.channels[(offset as usize - 0x10) / 0x10].control,
            0x14 | 0x24 => self.channels[(offset as usize - 0x14) / 0x10].interval,
            0x18 | 0x28 => self.channels[(offset as usize - 0x18) / 0x10].current(now),
            _ => 0,
        }
    }

    pub fn write(&mut self, offset: u64, value: u32, now: u64) {
        self.update(now);
        match offset {
            0x00 => self.irq_enable = value & 3,
            0x04 => self.irq_status &= !value,
            0x10 | 0x20 => {
                let index = (offset as usize - 0x10) / 0x10;
                let channel = &mut self.channels[index];
                let was_enabled = channel.enabled();
                let current = channel.current(now);
                channel.control = value & !2; // RELOAD self-clears
                if value & 2 != 0 || value & 1 != 0 && !was_enabled {
                    channel.base = if value & 2 != 0 { channel.interval } else { current };
                    channel.start = now;
                    channel.expired = false;
                } else if was_enabled && value & 1 == 0 {
                    channel.base = current;
                }
                self.seen[index] = now;
            }
            0x14 | 0x24 => self.channels[(offset as usize - 0x14) / 0x10].interval = value,
            0x18 | 0x28 => {
                let channel = &mut self.channels[(offset as usize - 0x18) / 0x10];
                channel.base = value;
                channel.start = now;
            }
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn free_running_counter_counts_down_and_wraps() {
        let mut timer = Timer::default();
        timer.write(0x24, u32::MAX, 0);
        timer.write(0x20, 0b0110, 0); // reload, OSC24M
        timer.write(0x20, 0b0101, 0); // enable
        assert_eq!(timer.read(0x28, 0), u32::MAX);
        assert_eq!(timer.read(0x28, 10), u32::MAX - 10);
    }

    #[test]
    fn one_shot_raises_status_once() {
        let mut timer = Timer::default();
        timer.write(0x00, 1, 0);
        timer.write(0x14, 100, 0);
        timer.write(0x10, 0x86, 0); // one-shot, reload
        timer.write(0x10, 0x85, 0); // enable
        assert_eq!(timer.next_deadline(0), Some(100));
        timer.update(50);
        assert!(!timer.irq(0));
        timer.update(101);
        assert!(timer.irq(0));
        assert_eq!(timer.read(0x18, 200), 0);
        timer.write(0x04, 1, 200);
        assert!(!timer.irq(0));
    }

    #[test]
    fn periodic_timer_reloads_interval() {
        let mut timer = Timer::default();
        timer.write(0x00, 1, 0);
        timer.write(0x14, 9, 0);
        timer.write(0x10, 0x06, 0);
        timer.write(0x10, 0x05, 0);
        timer.update(10);
        assert!(timer.irq(0));
        assert_eq!(timer.read(0x18, 15), 4);
    }
}
