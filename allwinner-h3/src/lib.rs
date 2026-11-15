//! Initial Allwinner H3 machine: Cortex-A7/A32 execution, based RAM, UART0 and GICv2.
//!
//! This is a bare-metal bring-up slice, not a Linux-capable H3 implementation yet.
//! The physical architectural timer is connected to the one-CPU GIC. Storage,
//! networking, display and SMP are not modelled.

mod gic;
mod machine;
mod uart;

pub use gic::{Gic, SPURIOUS};
pub use machine::{
    Machine, Soc, GICC_BASE, GICD_BASE, PHYSICAL_TIMER_PPI, RAM_BASE, RAM_SIZE, UART0_BASE,
};
pub use uart::Uart;
