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
            card_boot: false,
            rk808: false,
            banner: "experimental RK3288 panel board (Rockchip RK3288): four Cortex-A12 (interpreter), GIC-400, UART2, \
                     CRU/GRF/PMU, VOP + MIPI DSI panel; the power chips, audio, touch, pads and the GPU are not modelled yet",
            setup: &|m| {
                m.soc.display_rotation = rotation;
                m.soc.display_name = "sbc-emu: RK3288 panel board";
            },
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
