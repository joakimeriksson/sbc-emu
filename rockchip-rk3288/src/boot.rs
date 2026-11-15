//! Direct Linux boot with a stock RK3288 device tree, adjusted the way U-Boot's distro boot
//! adjusts it on a real board: `/chosen` (command line, initrd) and `/memory` (the emulated
//! DRAM size). No `/psci` node: mainline RK3288 has no secure monitor. Load addresses follow
//! U-Boot's rk3288 defaults (`kernel_addr_r`, `fdt_addr_r`, `ramdisk_addr_r`).
use crate::machine::{Machine, RAM_BASE};
use emu_core::fdt::Node;

pub const KERNEL_ADDR: u32 = 0x0200_0000;
pub const DTB_ADDR: u32 = 0x01f0_0000;
pub const INITRD_ADDR: u32 = 0x0400_0000;
pub const DEFAULT_CMDLINE: &str =
    "earlycon=uart8250,mmio32,0xff690000 console=ttyS2,115200 rdinit=/init";

/// Patch a stock DTB for direct boot.
pub fn prepare_device_tree(
    stock: &[u8],
    ram_bytes: u64,
    cmdline: &str,
    initrd: Option<(u32, u32)>,
) -> Result<Vec<u8>, String> {
    let mut root = Node::parse(stock)?;
    let compatible = root.prop("compatible").unwrap_or_default();
    if !compatible.split(|&b| b == 0).any(|c| c == b"rockchip,rk3288") {
        return Err("device tree is not for a Rockchip RK3288 board".into());
    }
    // rk3288.dtsi uses two address and two size cells at the root.
    let cells = |name| {
        root.prop(name)
            .and_then(|v| v.get(..4))
            .map_or(1, |v| u32::from_be_bytes(v.try_into().unwrap()))
    };
    let (address_cells, size_cells) = (cells("#address-cells"), cells("#size-cells"));
    let mut reg = Vec::new();
    for (value, count) in [(RAM_BASE, address_cells), (ram_bytes, size_cells)] {
        if count == 2 {
            reg.push((value >> 32) as u32);
        }
        reg.push(value as u32);
    }
    let memory = root.child_mut("memory");
    memory.set_str("device_type", "memory");
    memory.set_cells("reg", &reg);
    let chosen = root.child_mut("chosen");
    chosen.set_str("bootargs", cmdline);
    if let Some((start, end)) = initrd {
        chosen.set_cells("linux,initrd-start", &[start]);
        chosen.set_cells("linux,initrd-end", &[end]);
    }
    Ok(root.to_blob())
}

/// Load a zImage, an optional initrd and the patched DTB, and enter the kernel.
pub fn boot_linux(
    machine: &mut Machine,
    kernel: &[u8],
    stock_dtb: &[u8],
    initrd: Option<&[u8]>,
    cmdline: &str,
) -> Result<Vec<u8>, String> {
    if kernel.get(0x24..0x28) != Some(&[0x18, 0x28, 0x6f, 0x01]) {
        return Err("kernel is not an ARM zImage (magic 0x016f2818 missing)".into());
    }
    let ram_bytes = machine.soc.ram.mem.len() as u64;
    let initrd_range = initrd.map(|data| (INITRD_ADDR, INITRD_ADDR + data.len() as u32));
    let dtb = prepare_device_tree(stock_dtb, ram_bytes, cmdline, initrd_range)?;
    if (KERNEL_ADDR - DTB_ADDR) as usize <= dtb.len() {
        return Err("device tree too large for its load slot".into());
    }
    if (INITRD_ADDR - KERNEL_ADDR) as usize <= kernel.len() {
        return Err("kernel too large for its load slot".into());
    }
    let load = |m: &mut Machine, addr: u32, data: &[u8], what: &str| {
        m.load(addr as u64, data)
            .map_err(|_| format!("{what} does not fit in {} MiB of RAM", ram_bytes >> 20))
    };
    load(machine, KERNEL_ADDR, kernel, "kernel")?;
    load(machine, DTB_ADDR, &dtb, "device tree")?;
    if let Some(data) = initrd {
        load(machine, INITRD_ADDR, data, "initrd")?;
    }
    machine.boot_linux(KERNEL_ADDR, DTB_ADDR);
    Ok(dtb)
}

#[cfg(test)]
mod tests {
    use super::*;
    use emu_core::fdt::Fdt;

    fn stock() -> Vec<u8> {
        let mut fdt = Fdt::default();
        fdt.begin("");
        fdt.cells("#address-cells", &[2]);
        fdt.cells("#size-cells", &[2]);
        fdt.strs("compatible", &["asus,rk3288-tinker-s", "rockchip,rk3288"]);
        fdt.begin("chosen");
        fdt.str("stdout-path", "serial2:115200n8");
        fdt.end();
        fdt.begin("memory");
        fdt.str("device_type", "memory");
        fdt.cells("reg", &[0, 0, 0, 0x8000_0000]);
        fdt.end();
        fdt.end();
        fdt.finish()
    }

    #[test]
    fn patches_chosen_and_two_cell_memory_without_psci() {
        let blob = prepare_device_tree(&stock(), 1 << 30, "console=ttyS2", Some((1, 2))).unwrap();
        let root = Node::parse(&blob).unwrap();
        let chosen = root.children.iter().find(|n| n.name == "chosen").unwrap();
        assert_eq!(chosen.prop("stdout-path"), Some(&b"serial2:115200n8\0"[..]));
        assert_eq!(chosen.prop("bootargs"), Some(&b"console=ttyS2\0"[..]));
        assert_eq!(chosen.prop("linux,initrd-start"), Some(&[0, 0, 0, 1][..]));
        let memories: Vec<_> = root.children.iter().filter(|n| n.name.starts_with("memory")).collect();
        assert_eq!(memories.len(), 1);
        assert_eq!(memories[0].prop("reg"), Some(&[0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0x40, 0, 0, 0][..]));
        assert!(root.children.iter().all(|n| n.name != "psci"));
    }

    #[test]
    fn rejects_device_trees_for_other_socs() {
        let mut fdt = Fdt::default();
        fdt.begin("");
        fdt.strs("compatible", &["rockchip,rk3288w-board", "allwinner,sun8i-h3"]);
        fdt.end();
        assert!(prepare_device_tree(&fdt.finish(), 1 << 30, "", None).is_err());
    }
}
