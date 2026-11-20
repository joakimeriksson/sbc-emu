//! Device models that are not tied to one SoC, shared by the board crates:
//!
//! | module | device | used by |
//! |---|---|---|
//! | `gic` | Arm GIC-400 (distributor and CPU interfaces, SGIs) | Allwinner H3, Rockchip RK3288 |
//! | `uart` | Synopsys DesignWare APB UART (16550-style) | Allwinner H3, Rockchip RK3288 |
//! | `i2c` | the device side of an I²C bus (`I2cDevice`, `RegisterPointer`) | every I²C controller model |
//! | `rk808` | Rockchip RK808 PMIC and RTC | Tinker Board |
//! | `act8846` | Active-Semi ACT8846 PMIC | not attached by a board yet |
//! | `syr82x` | Silergy SYR827/SYR828 regulators | not attached by a board yet |
//! | `hym8563` | Haoyu HYM8563 RTC | not attached by a board yet |
//! | `bcm43455` | Broadcom BCM43455 Wi-Fi on SDIO (brcmfmac): backplane, cores, TCM, SDPCM; an emulated FullMAC dongle in place of the firmware (scans, open and WPA2-PSK joins, Ethernet to a network) | not attached by a board yet |
//! | `dw_hdmi` | Synopsys DesignWare HDMI TX with a connected monitor: IDs, hot plug, DDC and PHY I²C masters, interrupts; the EDID (`Mode`, `edid`, `EdidEeprom`) | Allwinner H3, Rockchip RK3288 |
//! | `pl330` | Arm DMA-330 DMA controller: the channel threads run the driver's microcode | Rockchip RK3288 (audio) |
//! | `sdio` | an SDIO card: CMD5/52/53, CCCR, FBR, CIS; the functions behind a trait | any SD host controller |
//! | `timebase` | 24 MHz architectural timer ticks against nanoseconds | Allwinner H3, Rockchip RK3288 |
//!
//! A board attaches an I²C chip to its controller's bus at the chip's address; the
//! controller model (an SoC's) drives START, bytes and STOP.

pub mod act8846;
pub mod bcm43455;
pub mod dw_hdmi;
pub mod gic;
pub mod hym8563;
pub mod i2c;
pub mod pl330;
pub mod rk808;
pub mod sdio;
pub mod syr82x;
pub mod timebase;
pub mod uart;

pub use gic::{Gic, NCPU, SPURIOUS};
pub use uart::Uart;
