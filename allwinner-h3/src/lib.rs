//! Allwinner H3 (Orange Pi One) machine: one Cortex-A7 (`arm-a32`), GIC-400, UART0,
//! CCU, the H3 timers and the architectural timers, enough for a mainline ARM32 kernel
//! and a BusyBox initramfs on the serial console. Other H3 devices in the distribution
//! device tree are read-back register stubs. Storage, networking, display and SMP are
//! not modelled yet.

pub mod boot;
mod gic;
mod machine;
mod timer;
mod uart;

pub use gic::{Gic, NCPU, SPURIOUS};
pub use timer::Timer;
pub use machine::{
    ns_to_ticks, ticks_to_ns, CoreState, Machine, Soc, GICC_BASE, GICD_BASE, PHYSICAL_TIMER_PPI,
    RAM_BASE, RAM_SIZE, UART0_BASE,
};
pub use uart::Uart;
