//! GICv3 with the RK3588's CPU affinity mapping.
pub use peripherals::gicv3::*;

/// MPIDR affinity fields for the RK3588's DynamIQ cores.
pub fn affinity(core: usize) -> u64 { (core as u64) << 8 }
