//! StarFive JH7110 SoC as on the Xunlong Orange Pi RV: four SiFive U74 harts (`riscv64`),
//! the CLINT and PLIC, UART0 (DesignWare 8250), the clock/reset generators and PLL syscon
//! Linux's clock drivers need, read-back stand-ins for other device pages, and a direct
//! boot through OpenSBI (`fw_dynamic`) as the board's U-Boot SPL starts it.
pub mod audio;
pub mod bluetooth;
pub mod boot;
pub mod crypto;
pub mod devices;
pub mod display;
pub mod dwmmc;
pub mod gmac;
pub mod i2c;
pub mod machine;
pub mod nvme;
pub mod pcie;
pub mod qspi;
pub mod sdio;
pub mod wifi;
pub mod xhci;

pub use machine::{AudioSource, HartState, Machine, Soc, RAM_BASE, RAM_SIZE};
