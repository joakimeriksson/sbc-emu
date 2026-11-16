//! RK3288 panel board (`vendor,panel`, Rockchip RK3288) adapter: the vendor's own kernel
//! and device tree from the vendor firmware's /boot, booted directly the way its resident
//! U-Boot's distro boot script does (`bootz` with the board DTB). The firmware is not part
//! of sbc-emu; see docs/panel-board.md for extracting it locally.
//!
//! The board shares the RK3288 machine with the Tinker Board; what differs is the device
//! tree and the panel: a MIPI-DSI panel behind VOP-little, mounted rotated (the panel node's
//! `rotation`), which screenshots and VNC show the way the user sees it.
use crate::{arg, serial_board::{self, RunConfig}};
use emu_core::fdt::Node;
use rockchip_rk3288::{boot, Machine};
use std::process::ExitCode;

pub(crate) fn run(args: &[String]) -> ExitCode {
    match execute(args) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("[panel-board] {e}");
            ExitCode::FAILURE
        }
    }
}

/// The `rotation` of the first panel node in the tree (degrees counter-clockwise).
fn panel_rotation(node: &Node) -> Option<u32> {
    if node.name.starts_with("panel") {
        if let Some(v) = node.prop("rotation").and_then(|v| v.get(..4)) {
            return Some(u32::from_be_bytes(v.try_into().unwrap()));
        }
    }
    node.children.iter().find_map(panel_rotation)
}

fn execute(args: &[String]) -> Result<(), String> {
    serial_board::validate_options(args, &["--dtb", "--dump-dtb", "--screenshot", "--vnc"])?;
    let read = |path: &str| std::fs::read(path).map_err(|e| format!("{path}: {e}"));
    let kernel = read(arg(args, "--kernel").ok_or("--kernel zImage is required (the firmware's /boot/zImage)")?)?;
    let dtb = read(arg(args, "--dtb").ok_or("--dtb rk3288-panel.dtb is required (the firmware's /boot)")?)?;
    let tree = Node::parse(&dtb)?;
    let compatible = tree.prop("compatible").unwrap_or_default();
    if !compatible.split(|&b| b == 0).any(|c| c.starts_with(b"vendor,")) {
        eprintln!("[panel-board] note: the device tree is not a vendor board's");
    }
    let rotation = panel_rotation(&tree).unwrap_or(0);
    let initrd = arg(args, "--initrd").map(read).transpose()?;
    let cmdline = arg(args, "--append").unwrap_or(boot::DEFAULT_CMDLINE);
    // The panel board has 2 GiB of DRAM.
    let ram = serial_board::ram_bytes(args, 2048)?;
    if ram == 0 || ram > 0xfe00_0000 {
        return Err("--ram-mb must be between 1 and 4064 (DRAM ends below the RK3288 device window)".into());
    }
    let config = RunConfig::from_args("panel-board", args)?;
    let make_machine = || -> Result<Machine, String> {
        let mut m = Machine::with_ram_size(ram);
        m.soc.display_rotation = rotation;
        m.soc.display_name = "sbc-emu: RK3288 panel board";
        boot::boot_linux(&mut m, &kernel, &dtb, initrd.as_deref(), cmdline)?;
        Ok(m)
    };
    if let Some(path) = arg(args, "--dump-dtb") {
        let range = initrd.as_ref().map(|d| (boot::INITRD_ADDR, boot::INITRD_ADDR + d.len() as u32));
        let blob = boot::prepare_device_tree(&dtb, ram as u64, cmdline, range)?;
        std::fs::write(path, blob).map_err(|e| e.to_string())?;
    }
    eprintln!(
        "[panel-board] experimental RK3288 panel board (Rockchip RK3288): four Cortex-A12 (interpreter), GIC-400, UART2, \
         CRU/GRF/PMU, VOP + MIPI DSI panel (rotation {rotation}); storage, audio, touch, pads and the GPU are not modelled yet"
    );
    serial_board::run(config, make_machine)
}
