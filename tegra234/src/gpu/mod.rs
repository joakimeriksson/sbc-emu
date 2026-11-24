//! The Ampere GA10B register aperture, as driven by Jetson Linux's nvgpu.
//!
//! NVIDIA's GA10B HAL uses GM20B's BOOT_0 decoder and GK20A's PTIMER
//! read path. Time values are nanoseconds on the SoC virtual clock.

use std::collections::HashMap;

mod copy;
mod host;
mod mmu;

pub const BAR0: u64 = 0x1700_0000;
pub const BAR1: u64 = 0x1800_0000;
pub const SIZE: u64 = 0x0100_0000;
pub const FUSE: u64 = 0x03b4_1000;
pub const FUSE_SIZE: u64 = 0x1000;
pub const SPIS: [u32; 4] = [68, 70, 71, 67];

/// PMC_BOOT_0 encodes architecture 0x17, implementation 0xb and revision A1.
const BOOT0: u32 = 0x17b0_00a1;
const PTIMER_TIME_0: u64 = 0x009400;
const PTIMER_TIME_1: u64 = 0x009410;
const RING_COMMAND: u64 = 0x12004c;
const RING_START_RESULTS: u64 = 0x120050;
const RING_COMMAND_MASK: u32 = 0x3f;
const RING_ENUMERATE_AND_START: u32 = 4;
const RING_CONNECTIVITY_PASS: u32 = 1;

pub struct Gpu {
    regs: HashMap<u64, u32>,
    host: host::Host,
    ring_started: bool,
    trace: bool,
}

impl Default for Gpu {
    fn default() -> Self {
        Self {
            regs: HashMap::new(),
            host: host::Host::default(),
            ring_started: false,
            trace: std::env::var_os("SBC_TRACE_GPU").is_some(),
        }
    }
}

impl Gpu {
    pub fn read(&self, offset: u64, now: u64) -> u32 {
        let value = match offset {
            0 => BOOT0,
            PTIMER_TIME_0 => now as u32,
            PTIMER_TIME_1 => (now >> 32) as u32,
            RING_COMMAND => 0, // The supported command completes on write.
            RING_START_RESULTS => u32::from(self.ring_started) * RING_CONNECTIVITY_PASS,
            _ => self.regs.get(&offset).copied().unwrap_or(0),
        };
        if self.trace && !matches!(offset, PTIMER_TIME_0 | PTIMER_TIME_1) {
            eprintln!("[gpu] read {offset:06x} = {value:08x}");
        }
        value
    }

    pub fn write(&mut self, offset: u64, value: u32, ram: &mut [u8]) {
        if self.trace {
            eprintln!("[gpu] write {offset:06x} <- {value:08x}");
        }
        match offset {
            RING_COMMAND => {
                if value & RING_COMMAND_MASK == RING_ENUMERATE_AND_START {
                    self.ring_started = true;
                }
            }
            host::SUBMIT => self.host.submit(
                self.regs.get(&host::BASE_LO).copied().unwrap_or(0),
                self.regs.get(&host::BASE_HI).copied().unwrap_or(0),
                value & 0xffff,
                ram,
            ),
            host::DOORBELL => {
                if self.host.ring(value, ram).is_none() && self.trace {
                    eprintln!("[gpu] doorbell {value:#x} could not complete");
                }
            }
            0 | PTIMER_TIME_0 | PTIMER_TIME_1 | RING_START_RESULTS => {}
            _ => {
                self.regs.insert(offset, value);
            }
        }
    }

    pub fn reset(&mut self) {
        self.regs.clear();
        self.host = host::Host::default();
        self.ring_started = false;
    }

    pub fn submissions(&self) -> u64 {
        self.host.submissions
    }
}
