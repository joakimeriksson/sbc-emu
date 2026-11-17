//! Interpreter ARMv7-A/A32 core used by the Allwinner H3 and Rockchip RK3288 machines. The
//! identification registers come from a `CpuModel` (Cortex-A7 by default, `CORTEX_A12` for
//! the RK3288).
//!
//! Implemented: the A32 instruction set used by Linux and an ARMv6/ARMv7 hard-float
//! userland (data processing, multiply/divide, DSP/media/saturating/parallel arithmetic,
//! all load/store forms with base-restored aborts, exclusives, SWP), exceptions with
//! FIQ/mode banking, CP15 system state, the physical and virtual architectural timers,
//! ARMv7 short-descriptor translation with a TLB, VFPv4-D32 and Advanced SIMD (NEON).
//! `step_el0` runs 32-bit user programs under an AArch64 kernel (AArch32 EL0, see `el0.rs`).
//! Unimplemented encodings raise the guest's undefined-instruction exception when
//! `Cpu::undef_to_guest` is set. Thumb and Thumb-2 (T32) with IT blocks and interworking. Not implemented: LPAE, TrustZone,
//! hypervisor mode, and multiple cores.

mod cpu;
mod el0;
pub mod jit;
mod media;
mod mmu;
pub mod model;
mod neon;
mod thumb;
mod vfp;

pub use model::{CpuModel, CORTEX_A12, CORTEX_A7};
pub use vfp::Vfp;
pub use jit::CodeWatch;
pub use el0::{Cp15, El0Access, El0Event, El0Exit, El1Host};

pub use cpu::{Cpu, MODE_ABT, MODE_FIQ, MODE_IRQ, MODE_SVC, MODE_SYS, MODE_UND, MODE_USR};
