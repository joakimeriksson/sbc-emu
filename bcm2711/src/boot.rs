//! What the Pi 4 firmware does before Linux, done by the emulator ("direct kernel boot"):
//! load the arm64 Image, initramfs and device tree, allocate the framebuffer (as `gpu_mem`
//! firmware would), and start the core at the kernel entry with x0 = DTB.
use crate::fdt;
use crate::machine::{Machine, GPU_MEM};

pub const KERNEL_BASE: u64 = 0x20_0000;
pub const DTB: u64 = 0x3f0_0000;
pub const INITRD: u64 = 0x400_0000;

pub const DEFAULT_CMDLINE: &str =
    "console=ttyAMA0,115200 console=tty1 earlycon=pl011,mmio32,0xfe201000 rdinit=/init loglevel=6";

pub fn boot_linux(m: &mut Machine, kernel: &[u8], initrd: Option<&[u8]>, cmdline: &str, display: bool) -> Result<(), String> {
    if kernel.len() < 64 || &kernel[56..60] != b"ARMd" {
        return Err("not an uncompressed arm64 Linux Image (gunzip kernel8.img first)".into());
    }
    let ram = m.soc.ram.len() as u64;
    let linux_ram = ram - GPU_MEM;
    let text_offset = u64::from_le_bytes(kernel[8..16].try_into().unwrap());
    let image_size = u64::from_le_bytes(kernel[16..24].try_into().unwrap());
    let entry = KERNEL_BASE + text_offset;
    if entry + image_size.max(kernel.len() as u64) > DTB { return Err("kernel too large for the load layout".into()); }
    m.load(entry, kernel);
    let initrd_range = match initrd {
        Some(d) => {
            if INITRD + d.len() as u64 > linux_ram / 2 { return Err("initramfs too large".into()); }
            m.load(INITRD, d);
            Some((INITRD, INITRD + d.len() as u64))
        }
        None => None,
    };
    let fb = if display {
        m.soc.mbox.firmware_alloc();
        let f = &m.soc.mbox.fb;
        Some((f.base, f.width, f.height, f.pitch()))
    } else { None };
    // like the firmware: one 8250 port for the mini UART (Pi kernels allocate none by default)
    let cmdline = if cmdline.contains("8250.nr_uarts") { cmdline.to_string() } else { format!("8250.nr_uarts=1 {cmdline}") };
    let dtb = fdt::pi4b(&fdt::BootInfo { ram: linux_ram, bootargs: &cmdline, initrd: initrd_range, fb, cpus: m.cpus.len() as u32, bluetooth: m.soc.bt.is_some() });
    m.load(DTB, &dtb);
    m.boot_linux(entry, DTB);
    Ok(())
}

/// What the Pi 4 firmware reads from the SD card's FAT boot partition.
pub struct SdBoot {
    pub kernel: Vec<u8>,
    pub initramfs: Option<Vec<u8>>,
    pub cmdline: String,
    /// human-readable notes on how config.txt was interpreted
    pub notes: Vec<String>,
}

fn gunzip_if_needed(data: Vec<u8>) -> Result<Vec<u8>, String> {
    if data.len() > 18 && data[0] == 0x1f && data[1] == 0x8b {
        // gzip member: 10-byte header (+ optional fields), raw deflate, 8-byte trailer
        let flags = data[3];
        let mut p = 10;
        if flags & 4 != 0 { p += 2 + u16::from_le_bytes([data[p], data[p + 1]]) as usize; }
        if flags & 8 != 0 { while data[p] != 0 { p += 1; } p += 1; }
        if flags & 16 != 0 { while data[p] != 0 { p += 1; } p += 1; }
        if flags & 2 != 0 { p += 2; }
        miniz_oxide::inflate::decompress_to_vec(&data[p..data.len() - 8]).map_err(|e| format!("gunzip: {e:?}"))
    } else {
        Ok(data)
    }
}

/// Parse config.txt for a Pi 4 (sections [all] and [pi4] apply) and load kernel, initramfs and
/// cmdline from the first FAT partition, as the firmware does.
pub fn read_sd_boot(disk: &mut dyn crate::sdhci::Disk) -> Result<SdBoot, String> {
    let parts = crate::fat::mbr_partitions(disk);
    let &(_, lba, _) = parts.iter().find(|p| matches!(p.0, 0x0b | 0x0c | 0x0e | 0x06 | 0x04)).ok_or("no FAT boot partition in the MBR")?;
    let mut fat = crate::fat::Fat::open(disk, lba).ok_or("boot partition is not FAT16/FAT32")?;
    let config = fat.read_file("config.txt").map(|c| String::from_utf8_lossy(&c).into_owned()).unwrap_or_default();
    let mut notes = Vec::new();
    let (mut kernel_name, mut initramfs_name, mut cmdline_name, mut auto_initramfs) = ("kernel8.img".to_string(), None::<String>, "cmdline.txt".to_string(), false);
    let mut applies = true;
    for line in config.lines() {
        let line = line.split('#').next().unwrap_or("").trim();
        if line.is_empty() { continue; }
        if line.starts_with('[') {
            let sect = line.trim_matches(|c| c == '[' || c == ']').to_ascii_lowercase();
            applies = matches!(sect.as_str(), "all" | "pi4" | "pi400" | "board-type=0x11");
            continue;
        }
        if !applies { continue; }
        if let Some(rest) = line.strip_prefix("initramfs ") {
            initramfs_name = rest.split_whitespace().next().map(str::to_string);
            continue;
        }
        let Some((k, v)) = line.split_once('=') else { continue };
        match (k.trim(), v.trim()) {
            ("kernel", v) => kernel_name = v.to_string(),
            ("cmdline", v) => cmdline_name = v.to_string(),
            ("auto_initramfs", v) => auto_initramfs = v == "1",
            ("arm_64bit", "0") => return Err("config.txt asks for a 32-bit kernel (arm_64bit=0): not supported".into()),
            ("dtoverlay", v) | ("dtparam", v) => notes.push(format!("ignored {k}={v} (reduced device tree)", k = k.trim())),
            _ => {}
        }
    }
    if initramfs_name.is_none() && auto_initramfs {
        let name = kernel_name.replace("kernel", "initramfs").trim_end_matches(".img").to_string();
        if fat.find(&name).is_some() { initramfs_name = Some(name); }
    }
    let kernel = fat.read_file(&kernel_name).ok_or(format!("{kernel_name} not found on the boot partition"))?;
    let kernel = gunzip_if_needed(kernel)?;
    let initramfs = match &initramfs_name { Some(n) => Some(fat.read_file(n).ok_or(format!("{n} not found"))?), None => None };
    let cmdline = fat.read_file(&cmdline_name).map(|c| String::from_utf8_lossy(&c).trim().to_string()).unwrap_or_default();
    // the firmware resolves the serialN aliases: serial0 is the PL011 here (no Bluetooth UART)
    let cmdline = cmdline.replace("console=serial0", "console=ttyAMA0").replace("console=serial1", "console=ttyS0");
    notes.insert(0, format!("kernel {kernel_name} ({} bytes), initramfs {}, cmdline from {cmdline_name}", kernel.len(),
        initramfs_name.as_deref().unwrap_or("none")));
    Ok(SdBoot { kernel, initramfs, cmdline, notes })
}
