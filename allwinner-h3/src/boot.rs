//! Direct Linux boot with the distribution's device tree, adjusted the way U-Boot
//! adjusts it on a real Orange Pi One: `/chosen` (command line, initrd), `/memory`
//! (the emulated DRAM size) and a `/psci` node for the host-emulated secure monitor.
//! Load addresses follow U-Boot's sunxi defaults.
use crate::machine::{Machine, RAM_BASE};
use emu_core::fdt::Node;

pub const KERNEL_ADDR: u32 = 0x4200_0000;
pub const DTB_ADDR: u32 = 0x4300_0000;
pub const INITRD_ADDR: u32 = 0x4330_0000;
pub const DEFAULT_CMDLINE: &str = "console=ttyS0,115200 earlycon rdinit=/init nosmp";

/// Patch a stock DTB for direct boot.
pub fn prepare_device_tree(
    stock: &[u8],
    ram_bytes: u64,
    cmdline: &str,
    initrd: Option<(u32, u32)>,
) -> Result<Vec<u8>, String> {
    let mut root = Node::parse(stock)?;
    let compatible = root.prop("compatible").unwrap_or_default();
    if !compatible.windows(17).any(|w| w == b"allwinner,sun8i-h") {
        return Err("device tree is not for an Allwinner sun8i-h3 board".into());
    }
    let memory = root.child_mut("memory");
    memory.set_str("device_type", "memory");
    memory.set_cells("reg", &[RAM_BASE as u32, ram_bytes as u32]);
    let chosen = root.child_mut("chosen");
    chosen.set_str("bootargs", cmdline);
    if let Some((start, end)) = initrd {
        chosen.set_cells("linux,initrd-start", &[start]);
        chosen.set_cells("linux,initrd-end", &[end]);
    }
    let psci = root.child_mut("psci");
    psci.set(
        "compatible",
        b"arm,psci-1.0\0arm,psci-0.2\0".to_vec(),
    );
    psci.set_str("method", "smc");
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
    if (INITRD_ADDR - DTB_ADDR) as usize <= dtb.len() {
        return Err("device tree too large for its load slot".into());
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
        fdt.strs("compatible", &["xunlong,orangepi-one", "allwinner,sun8i-h3"]);
        fdt.begin("chosen");
        fdt.str("stdout-path", "serial0:115200n8");
        fdt.end();
        fdt.begin("memory@40000000");
        fdt.str("device_type", "memory");
        fdt.cells("reg", &[0x4000_0000, 0x2000_0000]);
        fdt.end();
        fdt.end();
        fdt.finish()
    }

    #[test]
    fn patches_chosen_memory_and_psci_like_u_boot() {
        let blob = prepare_device_tree(&stock(), 1 << 30, "console=ttyS0", Some((1, 2))).unwrap();
        let root = Node::parse(&blob).unwrap();
        let chosen = root.children.iter().find(|n| n.name == "chosen").unwrap();
        assert_eq!(chosen.prop("stdout-path"), Some(&b"serial0:115200n8\0"[..]));
        assert_eq!(chosen.prop("bootargs"), Some(&b"console=ttyS0\0"[..]));
        assert_eq!(chosen.prop("linux,initrd-end"), Some(&[0, 0, 0, 2][..]));
        let memory = root.children.iter().find(|n| n.name.starts_with("memory")).unwrap();
        assert_eq!(memory.prop("reg"), Some(&[0x40, 0, 0, 0, 0x40, 0, 0, 0][..]));
        let psci = root.children.iter().find(|n| n.name == "psci").unwrap();
        assert_eq!(psci.prop("method"), Some(&b"smc\0"[..]));
    }

    #[test]
    fn rejects_device_trees_for_other_socs() {
        let mut fdt = Fdt::default();
        fdt.begin("");
        fdt.strs("compatible", &["rockchip,rk3288"]);
        fdt.end();
        assert!(prepare_device_tree(&fdt.finish(), 1 << 30, "", None).is_err());
    }
}
