//! Ampere Compute B (C7C0), using QMD v3.0 dispatch descriptors.
//! Register and descriptor layouts follow NVIDIA's clc7c0 and clc7c0qmd headers.
use super::{
    mmu::View,
    sass::{self, CONSTANT_BINDINGS, ConstantBuffer},
};

pub(super) const CLASS: u32 = 0xc7c0;
const QMD_ADDRESS: u32 = 0x2b4;
const SIGNAL_PCAS2: u32 = 0x2c0;
const REPORT_SEMAPHORE_A: u32 = 0x1b00;
const REPORT_SEMAPHORE_D: u32 = 0x1b0c;
const REPORT_ONE_WORD: u32 = 1 << 28;
const MAX_WORKGROUP_SIZE: u32 = 1024;
const MAX_LAUNCH_THREADS: u32 = 16384;

#[derive(Default)]
pub(super) struct Compute {
    qmd_address: Option<u64>,
    report: [u32; 3],
    upload: maxwell::inline::Inline,
    pub timestamp: u64,
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
            // State for features outside the scalar path. The in-order interpreter
            // has no shader caches, texture access, or asynchronous work distributor.
            0x200..=0x208
            | 0x248
            | 0x2a0
            | 0x2a4
            | 0x2e4..=0x2ec
            | 0x310
            | 0x790
            | 0x794
            | 0x7b0
            | 0x7b4
            | 0x155c..=0x1564
            | 0x1574..=0x157c => {}
            0x21c | 0x1698 => {} // Unified guest memory needs no cache invalidation.
            0x510 | 0x3400..=0x3408 => {} // Firmware and MME shadow setup.
            0x25f8 | 0x25fc => {} // Trap handler setup, unused by supported instructions.
            0x23c if data == 1 => {} // Software-reserved initialization method.
            0x1b0 if data & !0x51 != 0 || data & 1 == 0 => return None,
            method if maxwell::inline::Inline::owns(method) => {
                self.upload
                    .upload(method, data, |address, bytes| memory.write(address, bytes))?;
            }
            REPORT_SEMAPHORE_A..=0x1b08 => {
                self.report[((method - REPORT_SEMAPHORE_A) / 4) as usize] = data;
            }
            REPORT_SEMAPHORE_D if data & !(REPORT_ONE_WORD | 4) == 0 => {
                let [hi, lo, payload] = self.report;
                let address = (u64::from(hi & 0xff) << 32) | u64::from(lo);
                memory.release(
                    address,
                    payload,
                    (data & REPORT_ONE_WORD == 0).then_some(self.timestamp),
                )?;
            }
            _ => return None,
        }
        Some(())
    }
}
