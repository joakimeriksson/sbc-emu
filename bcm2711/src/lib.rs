//! Raspberry Pi 4B (BCM2711) machine: four Cortex-A72 cores, RAM, GIC-400 and the peripherals.
//! Peripherals sit at their "low peripheral mode" ARM physical addresses (0xfc00_0000+), as the
//! Pi 4 firmware sets up by default. Only what the reduced device tree (`fdt.rs`) describes is
//! modelled; everything else in the peripheral window reads as zero and is logged once.
pub mod boot;
pub mod dma;
pub mod dwc2;
pub mod fat;
pub mod fdt;
pub mod genet;
pub mod gic;
pub mod gpio;
pub mod i2c;
pub mod mailbox;
pub mod periph;
pub mod png;
pub mod sdhci;
pub mod spi;
pub mod wifi;
pub mod machine;
#[cfg(not(target_arch = "wasm32"))]
pub(crate) mod mt;
pub mod pl011;

pub use machine::{Machine, Stop};
