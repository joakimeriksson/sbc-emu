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
    /// config.txt asks for the vc4 KMS driver (`dtoverlay=vc4-kms-v3d`)
    pub kms: bool,
    /// the KMS outputs the overlay's parameters enable (`composite`, `nohdmi`)
    pub outputs: KmsOutputs,
}

/// The outputs `dtoverlay=vc4-kms-v3d` enables: HDMI unless `nohdmi`, the composite encoder
/// (VEC) with `composite` (the overlay's dormant fragment for the VEC node).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct KmsOutputs { pub hdmi: bool, pub composite: bool }

impl Default for KmsOutputs { fn default() -> Self { KmsOutputs { hdmi: true, composite: false } } }

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
    /// ... and its composite output (`dtoverlay=vc4-kms-v3d,composite`), which the firmware
    /// then sets up as `enable_tvout` and `sdtv_mode` say
    pub composite: bool,
}

/// The Pi 4's firmware: `[pi4]`, serial0 is the PL011 here (the mini UART is the Bluetooth
/// controller's, or `--serial1`).
pub const PI4_FIRMWARE: Firmware<'static> = Firmware {
    sections: &["pi4", "pi400", "board-type=0x11"], arm64_default: true, serial0: "ttyAMA0", serial1: "ttyS0", kms: true, composite: false,
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
    /// `dtoverlay=vc4-kms-v3d` (on a board whose emulated tree can describe it)
    pub kms: bool,
    /// the overlay's `composite` and `nohdmi` parameters
    pub outputs: KmsOutputs,
    /// `sdtv_mode=` (the composite TV standard: 0 NTSC, 1 NTSC-J, 2 PAL, 3 PAL-M, 16 and 18
    /// progressive NTSC and PAL), `enable_tvout=`, if given
    pub sdtv_mode: Option<u32>,
    pub enable_tvout: Option<bool>,
    /// `disable_fw_kms_setup=1`: the firmware writes no `video=` options into the command line
    pub disable_fw_kms_setup: bool,
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
            ("dtoverlay", v) if (v == "vc4-kms-v3d" || v.starts_with("vc4-kms-v3d,")) && fw.kms => {
                c.kms = true;
                let params: Vec<&str> = v.split(',').skip(1).map(str::trim).collect();
                c.outputs = KmsOutputs { hdmi: !params.contains(&"nohdmi"), composite: params.contains(&"composite") };
                if c.outputs.composite && !fw.composite {
                    c.outputs.composite = false;
                    c.notes.push("ignored the composite parameter of vc4-kms-v3d (no composite encoder in this board's model)".into());
                }
                let outs = match (c.outputs.hdmi, c.outputs.composite) { (true, true) => "HDMI and composite", (true, false) => "HDMI", (false, true) => "composite", (false, false) => "no output" };
                c.notes.push(format!("dtoverlay={v}: {outs} through the vc4 KMS driver (the device tree describes it unless --display fb)"));
            }
            ("sdtv_mode", v) => c.sdtv_mode = v.parse().ok(),
            ("enable_tvout", v) => c.enable_tvout = Some(v != "0"),
            ("disable_fw_kms_setup", v) => c.disable_fw_kms_setup = v == "1",
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
    let BootConfig { kernel: mut kernel_name, kernel_set, initramfs: mut initramfs_name, cmdline: mut cmdline_name, os_prefix, auto_initramfs, arm64, mut notes, kms,
                     outputs, sdtv_mode, enable_tvout, disable_fw_kms_setup } = parse_config(&config, fw);
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
    let mut cmdline = cmdline.replace("console=serial0", &format!("console={}", fw.serial0)).replace("console=serial1", &format!("console={}", fw.serial1));
    if kms && outputs.composite {
        match composite_video(sdtv_mode, enable_tvout, disable_fw_kms_setup) {
            Ok(video) => { notes.push(format!("composite: {video} added to the command line")); cmdline = format!("{cmdline} {video}"); }
            Err(why) => notes.push(format!("composite: {why}")),
        }
    }
    notes.insert(0, format!("kernel {kernel_name} ({} bytes), initramfs {}, cmdline from {cmdline_name}", kernel.len(),
        initramfs_name.as_deref().unwrap_or("none")));
    Ok(SdBoot { kernel, initramfs, cmdline, notes, kms, outputs })
}

/// The firmware's `video=` option for the composite output (its format string is
/// `video=Composite-1:%ux%u@%u%s`), from `sdtv_mode`: the mode Linux's `vc4` then prefers on
/// `Composite-1`, and with it the TV standard (`vc4.tv_norm` defaults to PAL for a 50 Hz mode,
/// to NTSC otherwise). None with `disable_fw_kms_setup=1` (the kernel's defaults apply) or
/// `enable_tvout=0` (composite off in the firmware).
pub fn composite_video(sdtv_mode: Option<u32>, enable_tvout: Option<bool>, disable_fw_kms_setup: bool) -> Result<String, String> {
    if enable_tvout == Some(false) { return Err("enable_tvout=0: the firmware leaves the composite output unconfigured (no video= option)".into()); }
    if disable_fw_kms_setup { return Err("disable_fw_kms_setup=1: no video= option from the firmware; Linux's defaults apply (NTSC unless vc4.tv_norm= says otherwise)".into()); }
    let (h, rate, i) = match sdtv_mode.unwrap_or(0) {
        0 | 1 | 3 => (480, 60, "i"), // NTSC, NTSC-J, PAL-M: 525 lines
        2 => (576, 50, "i"),         // PAL
        16 => (240, 60, ""),         // progressive NTSC
        18 => (288, 50, ""),         // progressive PAL
        n => return Err(format!("sdtv_mode={n} is not a TV standard the firmware knows; no video= option")),
    };
    Ok(format!("video=Composite-1:720x{h}@{rate}{i}"))
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
        let pi3 = Firmware { sections: &["pi3", "pi3+"], arm64_default: false, serial0: "ttyS0", serial1: "ttyAMA0", kms: true, composite: true };
        let c = parse_config(CONFIG, &pi3);
        assert!(!c.kms);
        assert!(parse_config("[pi3]\ndtoverlay=vc4-kms-v3d,cma-128\n", &pi3).kms);
        assert!(!parse_config("[pi4]\ndtoverlay=vc4-kms-v3d\n", &pi3).kms);
        assert_eq!((c.kernel.as_str(), c.cmdline.as_str(), c.initramfs.as_deref()), ("pi3.img", "plus.txt", Some("extra.cpio")));
        assert!(c.kernel_set);
    }

    #[test]
    fn composite_from_the_overlay_and_sdtv_mode() {
        let pi3 = Firmware { sections: &["pi3"], arm64_default: false, serial0: "ttyS0", serial1: "ttyAMA0", kms: true, composite: true };
        let c = parse_config("dtoverlay=vc4-kms-v3d\ndisable_fw_kms_setup=1\n", &pi3);
        assert_eq!((c.outputs, c.disable_fw_kms_setup), (KmsOutputs { hdmi: true, composite: false }, true));
        let c = parse_config("dtoverlay=vc4-kms-v3d,cma-128,composite\nsdtv_mode=2\n", &pi3);
        assert_eq!((c.outputs, c.sdtv_mode), (KmsOutputs { hdmi: true, composite: true }, Some(2)));
        assert_eq!(parse_config("dtoverlay=vc4-kms-v3d,composite,nohdmi\n", &pi3).outputs, KmsOutputs { hdmi: false, composite: true });
        // the Pi 4 model has no VEC: the parameter is reported and ignored
        assert!(!parse_config("dtoverlay=vc4-kms-v3d,composite\n", &PI4_FIRMWARE).outputs.composite);
        assert_eq!(composite_video(Some(2), None, false).as_deref(), Ok("video=Composite-1:720x576@50i"));
        assert_eq!(composite_video(None, Some(true), false).as_deref(), Ok("video=Composite-1:720x480@60i"));
        assert!(composite_video(Some(2), None, true).is_err() && composite_video(Some(2), Some(false), false).is_err());
    }
}
