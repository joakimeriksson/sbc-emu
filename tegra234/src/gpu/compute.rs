//! Ampere Compute B (C7C0), using QMD v3.0 dispatch descriptors.
//! Register and descriptor layouts follow NVIDIA's clc7c0 and clc7c0qmd headers.
use super::{
    mmu::View,
    sass::{self, CONSTANT_BINDINGS, ConstantBuffer},
};

pub(super) const CLASS: u32 = 0xc7c0;
const QMD_ADDRESS: u32 = 0x2b4;
const SIGNAL_PCAS2: u32 = 0x2c0;
const MAX_WORKGROUP_SIZE: u32 = 1024;
const MAX_LAUNCH_THREADS: u32 = 16384;

#[derive(Default)]
pub(super) struct Compute {
    qmd_address: Option<u64>,
}

struct Launch {
    program: u64,
    groups: [u32; 3],
    threads: [u32; 3],
    constants: [Option<ConstantBuffer>; CONSTANT_BINDINGS],
}

impl Launch {
    fn read(memory: &View, address: u64) -> Option<Self> {
        let word = |index: u64| memory.load(address + index * 4);
        let version_and_x = word(18)?;
        if version_and_x & 0xff != 0x30 {
            return None;
        }
        let yz = word(19)?;
        let program = u64::from(word(48)?) | u64::from(word(49)? & 0x1ffff) << 32;
        if program & 15 != 0 {
            return None;
        }
        let valid = word(20)?;
        let mut constants = [None; CONSTANT_BINDINGS];
        for (index, entry) in constants.iter_mut().enumerate() {
            if valid & (1 << index) != 0 {
                let descriptor = 32 + index as u64 * 2;
                let low = word(descriptor)?;
                let high = word(descriptor + 1)?;
                *entry = Some(ConstantBuffer {
                    address: u64::from(low) | u64::from(high & 0x1ffff) << 32,
                    size: (high >> 19) << 4,
                });
            }
        }
        Some(Self {
            program,
            constants,
            groups: [word(12)?, word(13)? & 0xffff, word(14)? & 0xffff],
            threads: [version_and_x >> 16, yz & 0xffff, yz >> 16],
        })
    }

    fn run(&self, memory: &mut View) -> Option<()> {
        let count = |dimensions: [u32; 3]| dimensions.into_iter().try_fold(1_u32, u32::checked_mul);
        let threads = count(self.threads)?;
        if threads > MAX_WORKGROUP_SIZE
            || threads.checked_mul(count(self.groups)?)? > MAX_LAUNCH_THREADS
        {
            return None;
        }
        let coordinates = |n: u32, [x, y, _]: [u32; 3]| [n % x, n / x % y, n / x / y];
        let shader = sass::Program::read(memory, self.program)?;
        for group in 0..count(self.groups)? {
            let group = coordinates(group, self.groups);
            for local in 0..threads {
                shader.run(
                    memory,
                    coordinates(local, self.threads),
                    group,
                    &self.constants,
                )?;
            }
        }
        Some(())
    }
}

impl Compute {
    pub fn method(&mut self, method: u32, data: u32, memory: &mut View) -> Option<()> {
        match method {
            0x100 | 0x110 => {} // NOP and WAIT_FOR_IDLE on the synchronous engine.
            QMD_ADDRESS => self.qmd_address = Some(u64::from(data) << 8),
            SIGNAL_PCAS2 if matches!(data, 2 | 3) => {
                Launch::read(memory, self.qmd_address?)?.run(memory)?;
            }
            _ => return None,
        }
        Some(())
    }
}
