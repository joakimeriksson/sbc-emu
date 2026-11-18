//! RK3288 panel board (`vendor,panel`, Rockchip RK3288) adapter: the vendor's own kernel
//! and device tree from the vendor firmware's /boot, booted directly the way its resident
//! U-Boot's distro boot script does (`bootz` with the board DTB). The firmware is not part
//! of sbc-emu; see docs/panel-board.md for extracting it locally.
//!
//! The board shares the RK3288 machine and run loop with the Tinker Board
//! (`tinkerboard::run_board`); what differs is the device tree and the panel: a MIPI-DSI
//! panel behind VOP-little, mounted rotated (the panel node's `rotation`), which
//! screenshots and VNC show the way the user sees it.
use crate::{arg, tinkerboard::{self, Profile}};
use rockchip_rk3288::{boot, devices::Peripherals};
use emu_core::fdt::Node;
use std::process::ExitCode;

/// The `rotation` of the first panel node in the tree (degrees counter-clockwise).
fn panel_rotation(node: &Node) -> Option<u32> {
    if node.name.starts_with("panel") {
        if let Some(v) = node.prop("rotation").and_then(|v| v.get(..4)) {
            return Some(u32::from_be_bytes(v.try_into().unwrap()));
        }
    }
    node.children.iter().find_map(panel_rotation)
}

/// The panel's mounting rotation from the device tree, if it is one the display can show
/// (a right angle); anything else is reported and shown unturned.
fn mounting_rotation(dtb: &[u8]) -> Result<u32, String> {
    let tree = Node::parse(dtb)?;
    let compatible = tree.prop("compatible").unwrap_or_default();
    if !compatible.split(|&b| b == 0).any(|c| c.starts_with(b"vendor,")) {
        eprintln!("[panel-board] note: the device tree is not a vendor board's");
    }
    Ok(match panel_rotation(&tree).unwrap_or(0) {
        r @ (0 | 90 | 180 | 270) => r,
        r => {
            eprintln!("[panel-board] note: panel rotation {r} is not a right angle; the display is shown unturned");
            0
        }
    })
}

/// The command line for the vendor's userland on the eMMC image `scripts/panel-board-fixture.sh`
/// builds (its root partition's PARTUUID; the vendor's U-Boot environment is not public).
const EMMC_CMDLINE: &str =
    "earlycon=uart8250,mmio32,0xff690000 console=ttyS2,115200 root=PARTUUID=4d504331-0000-4000-8000-000000000001 rootwait ro";

/// the vendor's device tree has the Wi-Fi's SDIO host (dwmmc@ff0d0000, with the BCM43455 under
/// it) disabled; their U-Boot, not public, presumably enables it on boards with Wi-Fi, as
/// the vendor application expects the controller bound. Enable it.
fn enable_wifi(dtb: Vec<u8>) -> Result<Vec<u8>, String> {
    let mut tree = Node::parse(&dtb)?;
    match tree.children.iter_mut().find(|n| n.name == "dwmmc@ff0d0000") {
        Some(sdio) => {
            sdio.set_str("status", "okay");
            Ok(tree.to_blob())
        }
        None => Ok(dtb),
    }
}

pub(crate) fn run(args: &[String]) -> ExitCode {
    // The DTB is read again by the shared loader; it is small.
    let rotation = match arg(args, "--dtb").map(|path| std::fs::read(path).map_err(|e| format!("{path}: {e}"))) {
        Some(Ok(dtb)) => match mounting_rotation(&dtb) {
            Ok(r) => r,
            Err(e) => {
                eprintln!("[panel-board] {e}");
                return ExitCode::FAILURE;
            }
        },
        // A missing or unreadable --dtb is reported by the shared loader.
        _ => 0,
    };
    tinkerboard::run_board(
        args,
        &Profile {
            tag: "panel-board",
            // The panel board has 2 GiB of DRAM.
            default_ram_mb: 2048,
            kernel_hint: "--kernel zImage is required (the firmware's /boot/zImage)",
            dtb_hint: "--dtb rk3288-panel.dtb is required (the firmware's /boot)",
            // With an eMMC and no initramfs, the vendor's own userland from the eMMC.
            cmdline: if arg(args, "--emmc").is_some() && arg(args, "--initrd").is_none() {
                EMMC_CMDLINE
            } else {
                boot::DEFAULT_CMDLINE
            },
            card_boot: false,
            power: Peripherals::attach_panel_board_power,
            banner: "experimental RK3288 panel board (Rockchip RK3288): four Cortex-A12, GIC-400, UART2, CRU/GRF/PMU, \
                     VOP + MIPI DSI panel, SD/eMMC, I2C with the ACT8846 PMIC, SYR827/828 and HYM8563; RGA, \
                     Mali-T760, ILI2116 touch, BCM43455 Wi-Fi (--net local|nat|none), I2S audio through the PL330 \
                     DMA (--audio-out FILE.wav|FIFO); the pads (USB) are not modelled yet",
            setup: &|m| {
                m.soc.display_rotation = rotation;
                m.soc.display_name = "sbc-emu: RK3288 panel board";
            },
            fixup_dtb: enable_wifi,
            // the virtual network only: with `--net nat` the vendor application reaches the
            // internet (and its maker's servers) on its own
            network: Some("local"),
        },
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_right_angle_rotations_are_used() {
        let tree = |rotation: u32| {
            let mut root = Node::new("");
            root.set_str("compatible", "vendor,panel");
            root.child_mut("dsi@ff960000").child_mut("panel@0").set_cells("rotation", &[rotation]);
            root.to_blob()
        };
        assert_eq!(mounting_rotation(&tree(270)).unwrap(), 270);
        assert_eq!(mounting_rotation(&tree(45)).unwrap(), 0);
    }
}
