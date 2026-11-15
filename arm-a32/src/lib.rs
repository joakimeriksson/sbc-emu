//! Interpreter ARMv7-A/A32 core (Cortex-A7 identity) used by the Allwinner H3 machine.
//!
//! Implemented: the A32 instruction set used by Linux and an ARMv6/ARMv7 hard-float
//! userland (data processing, multiply/divide, DSP/media/saturating/parallel arithmetic,
//! all load/store forms with base-restored aborts, exclusives, SWP), exceptions with
//! FIQ/mode banking, CP15 system state, the physical and virtual architectural timers,
//! ARMv7 short-descriptor translation with a TLB, and VFPv4-D32 (no Advanced SIMD).
//! Unimplemented encodings raise the guest's undefined-instruction exception when
//! `Cpu::undef_to_guest` is set. Thumb and Thumb-2 (T32) with IT blocks and interworking. Not implemented: NEON, LPAE, TrustZone,
//! hypervisor mode, and multiple cores.

mod cpu;
mod media;
mod mmu;
mod thumb;
mod vfp;

pub use vfp::Vfp;

pub use cpu::{Cpu, MODE_ABT, MODE_FIQ, MODE_IRQ, MODE_SVC, MODE_SYS, MODE_UND, MODE_USR};
