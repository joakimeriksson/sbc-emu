//! ARMv8.0-A AArch64 core (Cortex-A72 identity): integer ISA, system registers, exceptions
//! EL0–EL3, stage-1 MMU (4 KiB granule), FP/SIMD load/store. Interpreter first; a block cache
//! and JIT can follow esp32sim's pattern once the interpreter is the oracle.
mod cpu;
mod exec;
pub mod jit;
pub mod mmu;
mod neon;
mod simd;
pub mod sysreg;

pub use cpu::{Cpu, Exc};
