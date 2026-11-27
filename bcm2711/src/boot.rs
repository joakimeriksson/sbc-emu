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
    let dtb = fdt::pi4b(&fdt::BootInfo { ram: linux_ram, bootargs: &cmdline, initrd: initrd_range, fb, cpus: m.cpus.len() as u32, bluetooth: m.soc.bt.is_some(), kms: m.soc.vc4.is_some(), v3d: m.soc.v3d.is_some() });
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

/// How a board's firmware reads the boot partition: which `config.txt` sections are its own,
/// and what the `serial0`/`serial1` aliases in `cmdline.txt` name.
pub struct Firmware<'a> {
    /// model filters (`[pi4]`, `[pi3+]`, ...) that apply besides `[all]`; any other filter
    /// (another model, `[none]`, `[tryboot]`, EDID or GPIO filters) skips its section
    pub sections: &'a [&'a str],
    /// the board's firmware loads a 64-bit kernel without `arm_64bit=1` (the Pi 4's does,
    /// the Pi 3's loads the 32-bit `kernel7.img`)
    pub arm64_default: bool,
    /// the Linux devices behind the `serial0` and `serial1` aliases
    pub serial0: &'a str,
    pub serial1: &'a str,
    /// the emulated device tree describes the vc4 KMS pipeline `dtoverlay=vc4-kms-v3d` asks for
    pub kms: bool,
}

/// The Pi 4's firmware: `[pi4]`, serial0 is the PL011 here (the mini UART is the Bluetooth
/// controller's, or `--serial1`).
pub const PI4_FIRMWARE: Firmware<'static> = Firmware {
    sections: &["pi4", "pi400", "board-type=0x11"], arm64_default: true, serial0: "ttyAMA0", serial1: "ttyS0", kms: true,
};

/// Parse config.txt for a Pi 4 (sections [all] and [pi4] apply) and load kernel, initramfs and
/// cmdline from the first FAT partition, as the firmware does.
pub fn read_sd_boot(disk: &mut dyn crate::sdhci::Disk) -> Result<SdBoot, String> { read_sd_boot_for(disk, &PI4_FIRMWARE) }

/// Parse config.txt as board `fw`'s firmware does and load kernel, initramfs and cmdline from
/// the first FAT partition.
/// What `config.txt` says about the boot, for one board.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct BootConfig {
    pub kernel: String,
    /// `kernel=` was given (a 32-bit board's firmware then takes it as is)
    pub kernel_set: bool,
    pub initramfs: Option<String>,
    pub cmdline: String,
    pub os_prefix: String,
    pub auto_initramfs: bool,
    /// `arm_64bit=` if given
    pub arm64: Option<bool>,
    /// human-readable notes on what was applied or ignored
    pub notes: Vec<String>,
}

/// Parse `config.txt` as board `fw`'s firmware does: lines before the first filter and in
/// `[all]` or the board's own model sections apply, lines in every other section do not.
pub fn parse_config(config: &str, fw: &Firmware) -> BootConfig {
    let mut c = BootConfig { kernel: "kernel8.img".into(), cmdline: "cmdline.txt".into(), ..Default::default() };
    let mut applies = true;
    for line in config.lines() {
        let line = line.split('#').next().unwrap_or("").trim();
        if line.is_empty() { continue; }
        if line.starts_with('[') {
            let sect = line.trim_matches(|c| c == '[' || c == ']').to_ascii_lowercase();
            applies = sect == "all" || fw.sections.contains(&sect.as_str());
            continue;
        }
        if !applies { continue; }
        if let Some(rest) = line.strip_prefix("initramfs ") {
            c.initramfs = rest.split_whitespace().next().map(str::to_string);
            continue;
        }
        let Some((k, v)) = line.split_once('=') else { continue };
        match (k.trim(), v.trim()) {
            ("kernel", v) => { c.kernel = v.to_string(); c.kernel_set = true; }
            ("cmdline", v) => c.cmdline = v.to_string(),
            // os_prefix: a directory (or name prefix) in front of the kernel, initramfs and cmdline
            // (Ubuntu keeps them in current/, and new/ for [tryboot], which a normal boot skips)
            ("os_prefix", v) => c.os_prefix = v.to_string(),
            ("auto_initramfs", v) => c.auto_initramfs = v == "1",
            ("arm_64bit", v) => c.arm64 = Some(v != "0"),
            ("dtoverlay", v) if v.starts_with("vc4-kms-v3d") && fw.kms => c.notes.push("dtoverlay=vc4-kms-v3d: HDMI through the vc4 KMS driver (the device tree describes it unless --display fb)".into()),
            ("dtoverlay", v) | ("dtparam", v) => c.notes.push(format!("ignored {k}={v} (reduced device tree)", k = k.trim())),
            _ => {}
        }
    }
    c
}

/// Parse config.txt as board `fw`'s firmware does and load kernel, initramfs and cmdline from
/// the first FAT partition.
pub fn read_sd_boot_for(disk: &mut dyn crate::sdhci::Disk, fw: &Firmware) -> Result<SdBoot, String> {
    let parts = crate::fat::mbr_partitions(disk);
    let &(_, lba, _) = parts.iter().find(|p| matches!(p.0, 0x0b | 0x0c | 0x0e | 0x06 | 0x04)).ok_or("no FAT boot partition in the MBR")?;
    let mut fat = crate::fat::Fat::open(disk, lba).ok_or("boot partition is not FAT16/FAT32")?;
    let config = fat.read_file("config.txt").map(|c| String::from_utf8_lossy(&c).into_owned()).unwrap_or_default();
    let BootConfig { kernel: mut kernel_name, kernel_set, initramfs: mut initramfs_name, cmdline: mut cmdline_name, os_prefix, auto_initramfs, arm64, mut notes } = parse_config(&config, fw);
    if arm64 == Some(false) {
        return Err("config.txt asks for a 32-bit kernel (arm_64bit=0): not supported".into());
    }
    if arm64.is_none() && !fw.arm64_default && !kernel_set {
        return Err("config.txt does not ask for a 64-bit kernel (arm_64bit=1); this board's firmware would start the 32-bit kernel7.img: not supported".into());
    }
    if initramfs_name.is_none() && auto_initramfs {
        let name = kernel_name.replace("kernel", "initramfs").trim_end_matches(".img").to_string();
        if fat.find(&format!("{os_prefix}{name}")).is_some() { initramfs_name = Some(name); }
    }
    if !os_prefix.is_empty() {
        notes.push(format!("os_prefix={os_prefix}"));
        kernel_name = format!("{os_prefix}{kernel_name}");
        cmdline_name = format!("{os_prefix}{cmdline_name}");
        initramfs_name = initramfs_name.map(|n| format!("{os_prefix}{n}"));
    }
    let kernel = fat.read_file(&kernel_name).ok_or(format!("{kernel_name} not found on the boot partition"))?;
    let kernel = gunzip_if_needed(kernel)?;
    let initramfs = match &initramfs_name { Some(n) => Some(fat.read_file(n).ok_or(format!("{n} not found"))?), None => None };
    let cmdline = fat.read_file(&cmdline_name).map(|c| String::from_utf8_lossy(&c).trim().to_string()).unwrap_or_default();
    // the firmware resolves the serialN aliases
    let cmdline = cmdline.replace("console=serial0", &format!("console={}", fw.serial0)).replace("console=serial1", &format!("console={}", fw.serial1));
    notes.insert(0, format!("kernel {kernel_name} ({} bytes), initramfs {}, cmdline from {cmdline_name}", kernel.len(),
        initramfs_name.as_deref().unwrap_or("none")));
    Ok(SdBoot { kernel, initramfs, cmdline, notes })
}

#[cfg(test)]
mod tests {
    use super::*;

    const CONFIG: &str = "dtparam=audio=on\nauto_initramfs=1\narm_64bit=1\n[pi4]\nkernel=pi4.img\n[pi3]\nkernel=pi3.img\n[pi3+]\ncmdline=plus.txt\n[cm4]\notg_mode=1\n[all]\ninitramfs extra.cpio\n";

    #[test]
    fn config_sections_follow_the_board() {
        let pi4 = parse_config(CONFIG, &PI4_FIRMWARE);
        assert_eq!((pi4.kernel.as_str(), pi4.cmdline.as_str(), pi4.initramfs.as_deref()), ("pi4.img", "cmdline.txt", Some("extra.cpio")));
        assert!(pi4.auto_initramfs && pi4.arm64 == Some(true));
        let pi3 = Firmware { sections: &["pi3", "pi3+"], arm64_default: false, serial0: "ttyS0", serial1: "ttyAMA0", kms: false };
        let c = parse_config(CONFIG, &pi3);
        assert_eq!((c.kernel.as_str(), c.cmdline.as_str(), c.initramfs.as_deref()), ("pi3.img", "plus.txt", Some("extra.cpio")));
        assert!(c.kernel_set);
    }
}
