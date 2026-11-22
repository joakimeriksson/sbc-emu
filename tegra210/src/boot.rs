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

/// An arm64 Image: as given, or unpacked from an EFI zboot image ("zimg" at offset 4, the
/// payload's offset and size at 8 and 12, its compression at 24).
pub fn kernel_image(kernel: &[u8]) -> Result<Vec<u8>, String> {
    if kernel.len() >= 64 && &kernel[56..60] == b"ARMd" {
        return Ok(kernel.to_vec());
    }
    if kernel.len() >= 32 && &kernel[4..8] == b"zimg" {
        let u32_at = |o: usize| u32::from_le_bytes(kernel[o..o + 4].try_into().unwrap()) as usize;
        let (offset, size) = (u32_at(8), u32_at(12));
        let payload = kernel.get(offset..offset.checked_add(size).ok_or("zboot: bad payload")?).ok_or("zboot: truncated image")?;
        let kind = &kernel[24..32];
        if !kind.starts_with(b"gzip") {
            return Err(format!("zboot: {} compression is not supported (only gzip)", String::from_utf8_lossy(kind).trim_end_matches('\0')));
        }
        let image = gunzip(payload)?;
        if image.len() < 64 || &image[56..60] != b"ARMd" {
            return Err("zboot: the payload is not an arm64 Image".into());
        }
        return Ok(image);
    }
    if kernel.starts_with(&[0x1f, 0x8b]) {
        return kernel_image(&gunzip(kernel)?);
    }
    Err("expected an arm64 Image, a gzip'd Image or an EFI zboot vmlinuz".into())
}

fn gunzip(data: &[u8]) -> Result<Vec<u8>, String> {
    if data.len() < 18 || data[0] != 0x1f || data[1] != 0x8b || data[2] != 8 {
        return Err("not gzip".into());
    }
    let flags = data[3];
    let bad = || "gzip: truncated or bad header".to_string();
    let mut p = 10;
    if flags & 4 != 0 {
        let xlen = data.get(p..p + 2).ok_or_else(bad)?;
        p += 2 + u16::from_le_bytes([xlen[0], xlen[1]]) as usize;
    }
    for bit in [8, 16] {
        if flags & bit != 0 {
            p += data.get(p..).ok_or_else(bad)?.iter().position(|&b| b == 0).ok_or_else(bad)? + 1;
        }
    }
    if flags & 2 != 0 {
        p += 2;
    }
    let (body, trailer) = data.split_at(data.len() - 8);
    let out = miniz_oxide::inflate::decompress_to_vec(body.get(p..).ok_or_else(bad)?).map_err(|e| format!("gunzip: {e:?}"))?;
    if u32::from_le_bytes(trailer[4..].try_into().unwrap()) != out.len() as u32 {
        return Err("gzip: the unpacked size does not match (truncated file?)".into());
    }
    Ok(out)
}

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
    fn broken_gzip_is_an_error() {
        let mut d = vec![0u8; 18];
        d[..4].copy_from_slice(&[0x1f, 0x8b, 8, 4]);
        d[10..12].copy_from_slice(&[0xff, 0xff]); // an extra field longer than the file
        assert!(kernel_image(&d).is_err());
        d[3] = 8; // a name that never ends
        d[10..].fill(b'x');
        assert!(kernel_image(&d).is_err());
        assert!(kernel_image(&d[..12]).is_err());
        // a whole stream whose length trailer is wrong
        let mut z = vec![0x1f, 0x8b, 8, 0, 0, 0, 0, 0, 0, 0];
        z.extend(miniz_oxide::deflate::compress_to_vec(&[7; 100], 6));
        z.extend([0, 0, 0, 0, 99, 0, 0, 0]);
        assert!(gunzip(&z).unwrap_err().contains("size"));
        let n = z.len();
        z[n - 4] = 100;
        assert_eq!(gunzip(&z).unwrap(), vec![7; 100]);
    }

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
