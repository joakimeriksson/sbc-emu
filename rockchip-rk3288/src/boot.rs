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

/// What `boot_from_card` loaded, for the run banner.
#[derive(Debug)]
pub struct CardBoot {
    /// The controller the boot files came from (`devices::SDMMC` or `devices::EMMC`).
    pub controller: usize,
    pub dtb: String,
    pub cmdline: String,
    pub kernel_bytes: usize,
    pub initrd_bytes: usize,
    pub notes: Vec<String>,
}

/// Default device tree: the board this machine models (the Tinker Board S).
pub const DEFAULT_FDTFILE: &str = "rk3288-tinker-s.dtb";

/// Emulated boot firmware for an Armbian (or similar) card: what U-Boot and the image's
/// `boot.scr` (Armbian's `boot-rockchip.cmd`) do, then a direct kernel boot. Reads
/// `armbianEnv.txt`, `zImage`, `dtb/<fdtfile>` and `uInitrd` (legacy header stripped) from
/// `/boot` of the first Linux partition holding a zImage, on the SD card if one is
/// inserted, otherwise on the eMMC.
pub fn boot_from_card(machine: &mut Machine, extra_args: &str) -> Result<CardBoot, String> {
    use crate::devices::{EMMC, SDMMC};
    use emu_core::ext4::Ext4;
    let controller = [SDMMC, EMMC]
        .into_iter()
        .find(|&c| machine.soc.periph.mmc[c].card.present())
        .ok_or("no SD card or eMMC image (--sd IMAGE or --emmc IMAGE)")?;
    let disk = machine.soc.periph.mmc[controller].card.disk_mut().expect("present card");
    let mut mbr = [0u8; 512];
    disk.read(0, &mut mbr);
    if mbr[510..512] != [0x55, 0xaa] {
        return Err("the card has no MBR partition table".into());
    }
    let disk_id = u32::from_le_bytes(mbr[440..444].try_into().unwrap());
    let mut found = None;
    for slot in 0..4 {
        let entry = &mbr[446 + 16 * slot..462 + 16 * slot];
        let lba = u32::from_le_bytes(entry[8..12].try_into().unwrap()) as u64;
        if entry[4] != 0x83 {
            continue;
        }
        let Ok(mut fs) = Ext4::open(disk.as_mut(), lba) else { continue };
        if let Some(prefix) = ["/boot/", "/"].into_iter().find(|p| fs.exists(&format!("{p}zImage"))) {
            found = Some((slot, lba, prefix));
            break;
        }
    }
    let (slot, lba, prefix) = found.ok_or("no ext2/3/4 partition with zImage in / or /boot on the card")?;
    let mut fs = Ext4::open(disk.as_mut(), lba)?;
    let mut notes = Vec::new();
    if fs.needs_recovery {
        notes.push("the card's filesystem was not cleanly unmounted; its journal is not replayed, recent changes to /boot may be missing".into());
    }
    let mut env = std::collections::HashMap::new();
    match fs.read_file(&format!("{prefix}armbianEnv.txt")) {
        Ok(text) => {
            for line in String::from_utf8_lossy(&text).lines() {
                if let Some((key, value)) = line.split_once('=') {
                    env.insert(key.trim().to_string(), value.trim().to_string());
                }
            }
        }
        Err(_) => notes.push("no armbianEnv.txt: defaults".into()),
    }
    let get = |key: &str, default: &str| env.get(key).cloned().unwrap_or_else(|| default.to_string());
    if ["overlays", "user_overlays"].iter().any(|k| env.get(*k).is_some_and(|o| !o.is_empty())) {
        notes.push(format!("device tree overlays are not applied (overlays={})", get("overlays", "")));
    }
    let kernel = fs.read_file(&format!("{prefix}zImage"))?;
    let fdtfile = get("fdtfile", DEFAULT_FDTFILE);
    let fdtfile = fdtfile.trim_start_matches("rockchip/").to_string();
    let dtb = fs
        .read_file(&format!("{prefix}dtb/{fdtfile}"))
        .or_else(|_| fs.read_file(&format!("{prefix}dtb/rockchip/{fdtfile}")))?;
    let mut initrd = fs.read_file(&format!("{prefix}uInitrd")).ok();
    if let Some(data) = initrd.as_mut() {
        // U-Boot legacy image: 64-byte header (magic 0x27051956), then the payload.
        if data.len() > 64 && data[..4] == [0x27, 0x05, 0x19, 0x56] {
            let size = u32::from_be_bytes(data[12..16].try_into().unwrap()) as usize;
            *data = data[64..(64 + size).min(data.len())].to_vec();
        }
    }
    // boot.cmd: `console=both` gives "console=ttyS2,115200n8 console=tty1". The serial
    // console goes last here so /dev/console is the serial port (no display model yet).
    let consoles = match get("console", "both").as_str() {
        "serial" | "ttyS2,115200n8" => "console=ttyS2,115200n8",
        other => {
            if other == "display" {
                notes.push("console=display: the serial console is added (no display model)".into());
            }
            "console=tty1 console=ttyS2,115200n8"
        }
    };
    let earlycon = if get("earlycon", "off") == "on" { "earlycon" } else { "" };
    let splash = if get("bootlogo", "false") == "true" { "splash plymouth.ignore-serial-consoles" } else { "splash=verbose" };
    let docker = if get("docker_optimizations", "on") == "on" {
        "cgroup_enable=cpuset cgroup_memory=1 cgroup_enable=memory"
    } else {
        ""
    };
    let cmdline = format!(
        "earlyprintk root={} rootwait rootfstype={} {earlycon} {splash} {consoles} consoleblank=0 loglevel={} \
         ubootpart={disk_id:08x}-{:02x} usb-storage.quirks={} {} {} {docker} {extra_args}",
        get("rootdev", &format!("UUID={}", fs.uuid_string())),
        get("rootfstype", "ext4"),
        get("verbosity", "1"),
        slot + 1,
        get("usbstoragequirks", ""),
        get("extraargs", ""),
        get("extraboardargs", ""),
    )
    .split_whitespace()
    .collect::<Vec<_>>()
    .join(" ");
    let boot = CardBoot {
        controller,
        dtb: fdtfile,
        cmdline: cmdline.clone(),
        kernel_bytes: kernel.len(),
        initrd_bytes: initrd.as_ref().map_or(0, |d| d.len()),
        notes,
    };
    boot_linux(machine, &kernel, &dtb, initrd.as_deref(), &cmdline)?;
    Ok(boot)
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
