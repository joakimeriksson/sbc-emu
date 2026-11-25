//! Tegra234 Host1x syncpoint counters and threshold interrupts.
use std::collections::HashMap;

pub const COMMON: u64 = 0x13e0_0000;
pub const HYPERVISOR: u64 = 0x13e1_0000;
pub const VM: u64 = 0x13e4_0000;
pub const ACTMON: u64 = 0x13ef_0000;
pub const WINDOW: u64 = 0x10000;
pub const ACTMON_SIZE: u64 = 0x60000;
pub const FIRST_IRQ: usize = 32 + 448;
pub const IRQ_COUNT: usize = 8;

// GPU aperture: the first word in each 64 KiB slot increments a syncpoint.
const GPU_SYNCPOINT_BASE: u64 = 0x6000_0000;
const GPU_SYNCPOINT_STRIDE: u64 = 0x10000;

const POINTS: usize = 1024;
const BANKS: usize = POINTS / 32;
const CPU_INCREMENT: u64 = VM + 0x6400;
const STATUS: u64 = VM + 0x6600;
const IRQ_DEST: u64 = VM + 0x6684;
const IRQ_ENABLE: u64 = VM + 0x770c;
const IRQ_DISABLE: u64 = VM + 0x7790;
const VALUE: u64 = VM + 0x8080;
const THRESHOLD: u64 = VM + 0xa088;

pub fn contains(address: u64) -> bool {
    [COMMON, HYPERVISOR, VM]
        .into_iter()
        .any(|base| (base..base + WINDOW).contains(&address))
        || (ACTMON..ACTMON + ACTMON_SIZE).contains(&address)
}

pub struct Host1x {
    regs: HashMap<u64, u32>,
    values: [u32; POINTS],
    thresholds: [u32; POINTS],
    destinations: [u8; POINTS],
    enabled: [u32; BANKS],
    pending: [u32; BANKS],
}

impl Default for Host1x {
    fn default() -> Self {
        Self {
            regs: HashMap::new(),
            values: [0; POINTS],
            thresholds: [0; POINTS],
            destinations: [0; POINTS],
            enabled: [0; BANKS],
            pending: [0; BANKS],
        }
    }
}

fn bits(mut mask: u32) -> impl Iterator<Item = usize> {
    std::iter::from_fn(move || {
        if mask == 0 {
            return None;
        }
        let bit = mask.trailing_zeros();
        mask &= mask - 1;
        Some(bit as usize)
    })
}

fn register_index(address: u64, base: u64, count: usize) -> Option<usize> {
    let offset = address.checked_sub(base)?;
    (offset < 4 * count as u64).then_some((offset / 4) as usize)
}

impl Host1x {
    pub fn read(&self, address: u64) -> u32 {
        if let Some(point) = register_index(address, VALUE, POINTS) {
            self.values[point]
        } else if let Some(point) = register_index(address, THRESHOLD, POINTS) {
            self.thresholds[point]
        } else if let Some(point) = register_index(address, IRQ_DEST, POINTS) {
            u32::from(self.destinations[point])
        } else if let Some(bank) = register_index(address, STATUS, BANKS) {
            self.pending[bank]
        } else if let Some(bank) = register_index(address, IRQ_ENABLE, BANKS) {
            self.enabled[bank]
        } else {
            self.regs.get(&address).copied().unwrap_or(0)
        }
    }

    pub fn write(&mut self, address: u64, value: u32) {
        if let Some(point) = register_index(address, VALUE, POINTS) {
            self.values[point] = value;
            self.check_threshold(point);
        } else if let Some(point) = register_index(address, THRESHOLD, POINTS) {
            self.thresholds[point] = value;
            self.check_threshold(point);
        } else if let Some(point) = register_index(address, IRQ_DEST, POINTS) {
            self.destinations[point] = (value & 7) as u8;
        } else if let Some(bank) = register_index(address, STATUS, BANKS) {
            self.pending[bank] &= !value;
        } else if let Some(bank) = register_index(address, IRQ_DISABLE, BANKS) {
            self.enabled[bank] &= !value;
        } else if let Some(bank) = register_index(address, IRQ_ENABLE, BANKS) {
            self.enabled[bank] |= value;
            for bit in bits(value) {
                self.check_threshold(bank * 32 + bit);
            }
        } else if let Some(bank) = register_index(address, CPU_INCREMENT, BANKS) {
            for bit in bits(value) {
                let point = bank * 32 + bit;
                self.increment(point);
            }
        } else {
            self.regs.insert(address, value);
        }
    }

    pub fn gpu_increment(&mut self, physical: u64) -> bool {
        let Some(offset) = physical.checked_sub(GPU_SYNCPOINT_BASE) else {
            return false;
        };
        let point = offset / GPU_SYNCPOINT_STRIDE;
        if point >= POINTS as u64 || !offset.is_multiple_of(GPU_SYNCPOINT_STRIDE) {
            return false;
        }
        self.increment(point as usize);
        true
    }

    fn increment(&mut self, point: usize) {
        self.values[point] = self.values[point].wrapping_add(1);
        self.check_threshold(point);
    }

    fn check_threshold(&mut self, point: usize) {
        let bit = 1 << (point % 32);
        let bank = point / 32;
        if self.enabled[bank] & bit != 0
            && self.values[point].wrapping_sub(self.thresholds[point]) as i32 >= 0
        {
            self.pending[bank] |= bit;
        }
    }

    pub fn irq_mask(&self) -> u8 {
        let mut lines = 0;
        for (bank, (&pending, &enabled)) in self.pending.iter().zip(&self.enabled).enumerate() {
            for bit in bits(pending & enabled) {
                lines |= 1 << self.destinations[bank * 32 + bit];
            }
        }
        lines
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cpu_and_gpu_increment_the_same_wrapping_counter_and_route_threshold() {
        let mut host = Host1x::default();
        let point = 1023u64;
        let bank = point / 32 * 4;
        let bit = 1 << (point % 32);
        host.write(VALUE + 4 * point, u32::MAX - 1);
        host.write(THRESHOLD + 4 * point, 0);
        host.write(IRQ_DEST + 4 * point, 7);
        host.write(IRQ_ENABLE + bank, bit);
        host.write(CPU_INCREMENT + bank, bit);
        assert_eq!(host.read(VALUE + 4 * point), u32::MAX);
        assert_eq!(host.irq_mask(), 0);

        let gpu_register = GPU_SYNCPOINT_BASE + GPU_SYNCPOINT_STRIDE * point;
        assert!(!host.gpu_increment(gpu_register + 4));
        assert!(host.gpu_increment(gpu_register));
        assert_eq!(host.read(VALUE + 4 * point), 0);
        assert_eq!(host.read(STATUS + bank), bit);
        assert_eq!(host.irq_mask(), 1 << 7);
        host.write(IRQ_DISABLE + bank, bit);
        host.write(STATUS + bank, bit);
        assert_eq!(host.irq_mask(), 0);
        assert_eq!(host.read(STATUS + bank), 0);
    }
}
