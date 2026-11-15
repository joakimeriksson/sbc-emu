//! Interpreter-first ARMv7-A/A32 core for the Allwinner H3 bring-up.
//!
//! The implemented subset is intentionally explicit: A32 data processing, multiply/divide,
//! word/byte/halfword and block transfers, branches, basic exceptions, CP15 system state,
//! the physical architectural timer and ARMv7 short-descriptor translation. Thumb,
//! VFP/NEON, LPAE and SMP/exclusive synchronization are not implemented yet.

mod cpu;
mod media;
mod mmu;
mod vfp;

pub use vfp::Vfp;

pub use cpu::{Cpu, MODE_ABT, MODE_FIQ, MODE_IRQ, MODE_SVC, MODE_SYS, MODE_UND, MODE_USR};
