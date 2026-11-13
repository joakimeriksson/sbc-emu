//! Raspberry Pi 4B (BCM2711) machine: RAM, GIC-400, PL011 UART0, the Cortex-A72 generic timer.
//! Peripherals sit at their "low peripheral mode" ARM physical addresses (0xfc00_0000+), as the
//! Pi 4 firmware sets up by default. Only what the reduced device tree (`fdt.rs`) describes is
//! modelled; everything else in the peripheral window reads as zero and is logged once.
pub mod boot;
pub mod dwc2;
pub mod fat;
pub mod fdt;
pub mod genet;
pub mod gic;
pub mod mailbox;
pub mod png;
pub mod sdhci;
pub mod machine;
pub mod pl011;

pub use machine::{Machine, Stop};
