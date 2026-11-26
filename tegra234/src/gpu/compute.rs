//! Ampere Compute B (C7C0). Launches are QMD v3 descriptors, given by address
//! (SEND_SIGNALING_PCAS2_B) or written through the push buffer (the inline QMD that CUDA uses).
//! The work runs on the Tegra210 compute engine (`maxwell::compute`) with its SM70 switch on:
//! the descriptor layout follows NVIDIA's clc7c0qmd.h and programs are Volta-and-later code,
//! run by the same interpreter as the graphics shaders, with shared memory and barriers.
use super::mmu::{Guest, View};

pub(super) const CLASS: u32 = 0xc7c0;
const QMD_ADDRESS: u32 = 0x2b4;
const SIGNAL_PCAS2: u32 = 0x2c0;
const INLINE_ADDRESS_A: u32 = 0x318;
const INLINE_ADDRESS_B: u32 = 0x31c;
const INLINE_DATA: u32 = 0x320;
const INLINE_LAST_WORD: u32 = INLINE_DATA + 63 * 4;

pub(super) struct Compute {
    engine: maxwell::compute::Compute,
    pipe: maxwell::pipe::Pipe,
    qmd_address: Option<u64>,
    inline_address: [u32; 2],
    pub timestamp: u64,
}

impl Default for Compute {
    fn default() -> Self {
        let mut engine = maxwell::compute::Compute::default();
        engine.sm70 = true;
        let mut pipe = maxwell::pipe::Pipe::default();
        pipe.sm70 = true;
        Self {
            engine,
            pipe,
            qmd_address: None,
            inline_address: [0; 2],
            timestamp: 0,
        }
    }
}

impl Compute {
    /// A new submission: guest memory may have changed under cached programs.
    pub fn begin(&mut self) {
        self.pipe.invalidate();
    }

    fn launch(&mut self, qmd: u64, memory: &mut View) -> Option<()> {
        let mut guest = Guest::new(memory);
        self.engine
            .launch_at(qmd, &mut self.pipe, &mut guest, self.timestamp);
        if let Some((address, write, pc)) = guest.first_fault.get() {
            eprintln!(
                "[gpu] compute launch at {qmd:#x}: GPU page fault, {} {address:#x} (program {:#x} at +{:#x}, groups {:?}, threads {:?})",
                if write { "write" } else { "read" },
                self.engine.last.0,
                pc,
                self.engine.last.1,
                self.engine.last.2
            );
        }
        guest.ok()
    }

    pub fn method(&mut self, method: u32, data: u32, memory: &mut View) -> Option<()> {
        match method {
            INLINE_ADDRESS_A => self.inline_address[0] = data,
            INLINE_ADDRESS_B => self.inline_address[1] = data,
            INLINE_DATA..=INLINE_LAST_WORD => {
                let [hi, lo] = self.inline_address;
                let address = (u64::from(hi) << 40) | (u64::from(lo) << 8);
                if hi >> 8 != 0 {
                    return None;
                }
                memory.store(address + u64::from(method - INLINE_DATA), data)?;
                // Writing the final word submits the descriptor to the scheduler.
                if method == INLINE_LAST_WORD {
                    self.launch(address, memory)?;
                }
            }
            QMD_ADDRESS => self.qmd_address = Some(u64::from(data) << 8),
            SIGNAL_PCAS2 if matches!(data, 2 | 3) => self.launch(self.qmd_address?, memory)?,
            SIGNAL_PCAS2 => {}
            _ => {
                let mut guest = Guest::new(memory);
                self.engine
                    .method(method, data, &mut self.pipe, &mut guest, self.timestamp);
                guest.ok()?;
            }
        }
        Some(())
    }
}
