//! Ampere GA10B BAR0 registers for early GPU bring-up.
//!
//! NVIDIA's GA10B HAL uses GM20B's BOOT_0 decoder and GK20A's PTIMER
//! read path. Time values are nanoseconds on the SoC virtual clock.

pub const BAR0: u64 = 0x1700_0000;
pub const SIZE: u64 = 0x0100_0000;

/// PMC_BOOT_0 encodes architecture 0x17, implementation 0xb and revision A1.
const BOOT0: u32 = 0x17b0_00a1;
const PTIMER_TIME_0: u64 = 0x009400;
const PTIMER_TIME_1: u64 = 0x009410;

pub struct Gpu;

impl Gpu {
    pub fn read(&self, offset: u64, now: u64) -> Option<u32> {
        Some(match offset {
            0 => BOOT0,
            PTIMER_TIME_0 => now as u32,
            PTIMER_TIME_1 => (now >> 32) as u32,
            _ => return None,
        })
    }
}
