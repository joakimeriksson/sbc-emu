//! Device models that are not tied to one SoC, shared by the board crates:
//!
//! | module | device | used by |
//! |---|---|---|
//! | `gic` | Arm GIC-400 (distributor and CPU interfaces, SGIs) | Allwinner H3, Rockchip RK3288 |
//! | `uart` | Synopsys DesignWare APB UART (16550-style) | Allwinner H3, Rockchip RK3288 |
//! | `i2c` | the device side of an I²C bus (`I2cDevice`, `RegisterPointer`) | every I²C controller model |
//! | `rk808` | Rockchip RK808 PMIC and RTC | Tinker Board |
//! | `act8846` | Active-Semi ACT8846 PMIC | panel board |
//! | `syr82x` | Silergy SYR827/SYR828 regulators | panel board |
//! | `hym8563` | Haoyu HYM8563 RTC | panel board |
//! | `ili2116` | Ilitek ILI2116 touch controller | panel board |
//! | `timebase` | 24 MHz architectural timer ticks against nanoseconds | Allwinner H3, Rockchip RK3288 |
//!
//! A board attaches an I²C chip to its controller's bus at the chip's address; the
//! controller model (an SoC's) drives START, bytes and STOP.

pub mod act8846;
pub mod gic;
pub mod hym8563;
pub mod i2c;
pub mod ili2116;
pub mod rk808;
pub mod syr82x;
pub mod timebase;
pub mod uart;

pub use gic::{Gic, NCPU, SPURIOUS};
pub use uart::Uart;
