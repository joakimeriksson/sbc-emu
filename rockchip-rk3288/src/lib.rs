//! Rockchip RK3288 machine (ASUS Tinker Board first, the RK3288 panel board later): four cores
//! with the Cortex-A12 identity on `arm-a32`, GIC-400, UART2, CRU with locked PLLs and core
//! resets, the GRF PLL lock status, PMU power domains, on-chip SRAM with the boot ROM's SMP
//! mailbox and the architectural timers, enough for a mainline ARM32 SMP kernel on the
//! serial console; the DesignWare MMC hosts (SD, SDIO, eMMC), the rk3x I²C controllers with
//! the RK808 PMIC and the GPIO input levels (`devices`), enough for Armbian from an SD card.
//! Other devices in the RK3288 window are read-back register stubs. Networking and display
//! are not modelled.
//!
//! The GIC-400, the DesignWare APB UART and the I²C chips (RK808, ACT8846, SYR827/828,
//! HYM8563, ILI2116) are the shared `peripherals` crate's models.

pub mod boot;
pub mod cru;
pub mod devices;
pub mod dsi;
pub mod dwmmc;
pub mod emmc;
pub mod gpio;
pub mod i2c;
mod machine;
pub mod rga;
pub mod vop;

pub use peripherals::{Gic, Uart, NCPU, SPURIOUS};
// The I²C chips live in the shared `peripherals` crate; re-exported for existing paths.
pub use peripherals::{act8846, hym8563, ili2116, rk808, syr82x};
pub use machine::{
    ns_to_ticks, ticks_to_ns, CoreState, Machine, Soc, BOOT_MAGIC, CRU_BASE, GICC_BASE,
    GICD_BASE, GRF_BASE, PHYSICAL_TIMER_PPI, PMU_BASE, RAM_BASE, RAM_SIZE, RK3288_MIDR,
    RK3288_MPIDR, DSI0_BASE, GPU_BASE, GPU_IRQS, RGA_BASE, RGA_IRQ, VOP_BASES, VOP_IRQS, SECURE_PHYSICAL_TIMER_PPI, SMP_MAILBOX, SRAM_BASE, SRAM_SIZE, UART2_BASE,
    UART2_IRQ, VIRTUAL_TIMER_PPI,
};
