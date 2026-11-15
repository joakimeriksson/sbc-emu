//! Rockchip RK3288 machine (ASUS Tinker Board first, the RK3288 panel board later): one core with
//! the Cortex-A12/A17 identity on `arm-a32`, GIC-400, UART2, CRU with locked PLLs, the GRF
//! PLL lock status, PMU power domains and the architectural timers, enough for a mainline
//! ARM32 kernel on the serial console. Other devices in the RK3288 window are read-back
//! register stubs. SMP (boot ROM mailbox), storage, networking and display are not modelled.
//!
//! The GIC-400 and the DesignWare APB UART are the Allwinner H3 crate's models (same IP);
//! they move to a shared crate once both boards are merged.

pub mod boot;
pub mod cru;
mod machine;

pub use allwinner_h3::{Gic, Uart, SPURIOUS};
pub use machine::{
    ns_to_ticks, ticks_to_ns, Machine, Soc, CRU_BASE, GICC_BASE, GICD_BASE, GRF_BASE,
    PHYSICAL_TIMER_PPI, PMU_BASE, RAM_BASE, RAM_SIZE, RK3288_MIDR, RK3288_MPIDR,
    SECURE_PHYSICAL_TIMER_PPI, UART2_BASE, UART2_IRQ, VIRTUAL_TIMER_PPI,
};
