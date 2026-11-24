//! Direct Linux boot with the stock Jetson Nano device tree (`tegra210-p3450-0000.dtb`),
//! adjusted the way NVIDIA's bootloader (or U-Boot) adjusts it on a real board: `/chosen`
//! (command line, initrd) and `/memory` (the emulated DRAM size). The kernel is an arm64
//! `Image`, or a distribution's EFI zboot `vmlinuz` (a gzip'd Image), which is unpacked here
//! as the EFI stub would.
use crate::{Machine, RAM_BASE};
use emu_core::fdt::Node;

/// Offsets from the start of RAM: the kernel at 2 MiB (arm64's alignment), the device tree
/// and the initrd above the largest kernel image we expect (`image_size` is checked).
pub const KERNEL_OFFSET: u64 = 0x0020_0000;
pub const DTB_OFFSET: u64 = 0x0800_0000;
pub const INITRD_OFFSET: u64 = 0x0820_0000;
const MAX_DTB: u64 = INITRD_OFFSET - DTB_OFFSET;
pub const DEFAULT_CMDLINE: &str = "earlycon=uart8250,mmio32,0x70006000 console=ttyS0,115200 rdinit=/init";

pub use emu_core::arm64_image::kernel_image;

/// Patch the stock DTB for direct boot.
pub fn prepare_device_tree(stock: &[u8], ram_bytes: u64, cmdline: &str, initrd: Option<(u64, u64)>) -> Result<Vec<u8>, String> {
    let mut root = Node::parse(stock)?;
    let compatible = root.prop("compatible").unwrap_or_default();
    if !compatible.split(|&b| b == 0).any(|c| c == b"nvidia,tegra210") {
        return Err("device tree is not for an NVIDIA Tegra210 board".into());
    }
    // tegra210.dtsi has two address and two size cells at the root
    let memory = root.child_mut("memory");
    memory.set_str("device_type", "memory");
    memory.set_cells("reg", &[(RAM_BASE >> 32) as u32, RAM_BASE as u32, (ram_bytes >> 32) as u32, ram_bytes as u32]);
    let chosen = root.child_mut("chosen");
    chosen.set_str("bootargs", cmdline);
    if let Some((start, end)) = initrd {
        chosen.set_cells("linux,initrd-start", &[(start >> 32) as u32, start as u32]);
        chosen.set_cells("linux,initrd-end", &[(end >> 32) as u32, end as u32]);
    }
    Ok(root.to_blob())
}

/// Load the kernel, an optional initrd and the patched DTB, and set core 0 at the kernel's
/// entry (EL1, MMU off, x0 = the device tree), as arm64 Linux's boot protocol asks.
pub fn boot_linux(m: &mut Machine, kernel: &[u8], dtb: &[u8], initrd: Option<&[u8]>, cmdline: &str) -> Result<(), String> {
    let image = kernel_image(kernel)?;
    let text_offset = u64::from_le_bytes(image[8..16].try_into().unwrap());
    let image_size = u64::from_le_bytes(image[16..24].try_into().unwrap()).max(image.len() as u64);
    let entry = KERNEL_OFFSET + text_offset;
    if entry + image_size > DTB_OFFSET {
        return Err(format!("the kernel ({} MiB with its bss) does not fit below the device tree", image_size >> 20));
    }
    let ram_len = m.soc.ram.len() as u64;
    if DTB_OFFSET + MAX_DTB > ram_len {
        return Err(format!("{} MiB of RAM is too little for the boot layout; give --ram-mb 256 or more", ram_len >> 20));
    }
    m.soc.ram[entry as usize..entry as usize + image.len()].copy_from_slice(&image);
    let range = match initrd {
        Some(data) => {
            let end = INITRD_OFFSET + data.len() as u64;
            if end > ram_len {
                return Err(format!("the initrd ({} MiB) does not fit in RAM; give more --ram-mb", data.len() >> 20));
            }
            m.soc.ram[INITRD_OFFSET as usize..end as usize].copy_from_slice(data);
            Some((RAM_BASE + INITRD_OFFSET, RAM_BASE + end))
        }
        None => None,
    };
    let dt = prepare_device_tree(dtb, ram_len, cmdline, range)?;
    if DTB_OFFSET + dt.len() as u64 > INITRD_OFFSET {
        return Err("the device tree is larger than 2 MiB".into());
    }
    m.soc.ram[DTB_OFFSET as usize..DTB_OFFSET as usize + dt.len()].copy_from_slice(&dt);
    let c = m.cpu_mut();
    c.el = 1;
    c.spsel = true;
    c.daif = 0x3c0;
    c.pc = RAM_BASE + entry;
    c.x = [0; 31];
    c.x[0] = RAM_BASE + DTB_OFFSET;
    c.sys.cpacr_el1 = 3 << 20;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn too_little_ram_is_an_error() {
        let mut image = vec![0u8; 4096];
        image[56..60].copy_from_slice(b"ARMd");
        for mb in [64, 128, 129] {
            let mut m = Machine::with_cpus(mb << 20, 1).unwrap();
            assert!(boot_linux(&mut m, &image, &[], None, "").unwrap_err().contains("RAM"), "{mb} MiB");
        }
    }
}
