//! Direct Linux boot with the distribution's device tree, adjusted the way U-Boot
//! adjusts it on a real Orange Pi One: `/chosen` (command line, initrd), `/memory`
//! (the emulated DRAM size) and a `/psci` node for the host-emulated secure monitor.
//! Load addresses follow U-Boot's sunxi defaults.
use crate::machine::{Machine, RAM_BASE};
use emu_core::fdt::Node;

pub const KERNEL_ADDR: u32 = 0x4200_0000;
pub const DTB_ADDR: u32 = 0x4300_0000;
pub const INITRD_ADDR: u32 = 0x4330_0000;
pub const DEFAULT_CMDLINE: &str = "console=ttyS0,115200 earlycon rdinit=/init";

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

/// What `boot_from_card` loaded, for the run banner.
#[derive(Debug)]
pub struct CardBoot {
    pub cmdline: String,
    pub kernel_bytes: usize,
    pub initrd_bytes: usize,
    pub notes: Vec<String>,
}

/// Emulated boot firmware for an Armbian (or similar) SD card: do what U-Boot and the
/// image's `boot.scr` do, then boot the kernel directly. Reads `/boot/armbianEnv.txt`,
/// `/boot/zImage`, `/boot/dtb/sun8i-h3-orangepi-one.dtb` and `/boot/uInitrd` (U-Boot
/// legacy header stripped) from the first Linux partition of the card in mmc0.
pub fn boot_from_card(machine: &mut Machine, extra_args: &str) -> Result<CardBoot, String> {
    use emu_core::ext4::Ext4;
    let disk = machine.soc.mmc.card.disk.as_mut().ok_or("no SD card (--sd IMAGE)")?;
    let mut mbr = [0u8; 512];
    disk.read(0, &mut mbr);
    if mbr[510..512] != [0x55, 0xaa] {
        return Err("the SD card has no MBR partition table".into());
    }
    let disk_id = u32::from_le_bytes(mbr[440..444].try_into().unwrap());
    // Linux partitions by MBR slot; the boot files are in /boot of the root partition, or
    // at the root of a separate boot partition.
    let slots = (0..4).filter_map(|slot| {
        let e = &mbr[446 + 16 * slot..462 + 16 * slot];
        let lba = u32::from_le_bytes(e[8..12].try_into().unwrap()) as u64;
        (e[4] == 0x83).then_some((slot, lba))
    });
    let mut found = None;
    for (slot, lba) in slots {
        let Ok(mut fs) = Ext4::open(disk.as_mut(), lba) else { continue };
        for prefix in ["/boot/", "/"] {
            if fs.exists(&format!("{prefix}zImage")) {
                found = Some((slot, lba, prefix));
                break;
            }
        }
        if found.is_some() {
            break;
        }
    }
    let (slot, lba, prefix) = found.ok_or("no ext2/3/4 partition with zImage in / or /boot on the SD card")?;
    let mut fs = Ext4::open(disk.as_mut(), lba)?;
    let mut notes = Vec::new();
    if fs.needs_recovery {
        notes.push("the card's filesystem was not cleanly unmounted; its journal is not replayed, recent changes to /boot may be missing".into());
    }
    let mut env = std::collections::HashMap::new();
    if let Ok(text) = fs.read_file(&format!("{prefix}armbianEnv.txt")) {
        for line in String::from_utf8_lossy(&text).lines() {
            if let Some((key, value)) = line.split_once('=') {
                env.insert(key.trim().to_string(), value.trim().to_string());
            }
        }
    } else {
        notes.push("no armbianEnv.txt: defaults".into());
    }
    let get = |key: &str, default: &str| env.get(key).cloned().unwrap_or_else(|| default.to_string());
    if env.get("overlays").is_some_and(|o| !o.is_empty()) || env.get("user_overlays").is_some_and(|o| !o.is_empty()) {
        notes.push("device tree overlays in armbianEnv.txt are not applied".into());
    }
    let kernel = fs.read_file(&format!("{prefix}zImage"))?;
    if kernel.len() > (DTB_ADDR - KERNEL_ADDR) as usize {
        return Err(format!("zImage of {} bytes does not fit below the device tree (16 MiB)", kernel.len()));
    }
    // boot.cmd: fdtfile (U-Boot's default for the board), with or without a vendor dir.
    let fdtfile = get("fdtfile", "sun8i-h3-orangepi-one.dtb");
    let fdtfile = fdtfile.trim_start_matches("allwinner/");
    let dtb = fs
        .read_file(&format!("{prefix}dtb/{fdtfile}"))
        .or_else(|_| fs.read_file(&format!("{prefix}dtb/allwinner/{fdtfile}")))?;
    let mut initrd = fs.read_file(&format!("{prefix}uInitrd")).ok();
    if let Some(data) = initrd.as_mut() {
        // U-Boot legacy image: 64-byte header (magic 0x27051956), then the payload.
        if data.len() > 64 && data[..4] == [0x27, 0x05, 0x19, 0x56] {
            let size = u32::from_be_bytes(data[12..16].try_into().unwrap()) as usize;
            *data = data[64..(64 + size).min(data.len())].to_vec();
        }
    }
    // boot.cmd's consoles, with the serial console always present and last (so
    // /dev/console is the serial port: there is no display model).
    let consoles = match get("console", "both").as_str() {
        "serial" => "console=ttyS0,115200",
        other => {
            if other == "display" {
                notes.push("console=display: the serial console is added (no display model)".into());
            }
            "console=tty1 console=ttyS0,115200"
        }
    };
    let docker = if get("docker_optimizations", "on") == "on" { "cgroup_enable=memory" } else { "" };
    let cmdline = format!(
        "root={} rootwait rootfstype={} splash=verbose {} consoleblank=0 loglevel={} ubootpart={:08x}-{:02x} ubootsource=mmc usb-storage.quirks={} {} {} {} {}",
        get("rootdev", &format!("UUID={}", fs.uuid_string())),
        get("rootfstype", "ext4"),
        consoles,
        get("verbosity", "1"),
        disk_id,
        slot + 1,
        get("usbstoragequirks", ""),
        get("extraargs", ""),
        get("extraboardargs", ""),
        docker,
        extra_args
    )
    .split_whitespace()
    .collect::<Vec<_>>()
    .join(" ");
    let boot = CardBoot {
        cmdline: cmdline.clone(),
        kernel_bytes: kernel.len(),
        initrd_bytes: initrd.as_ref().map_or(0, |d| d.len()),
        notes,
    };
    boot_linux(machine, &kernel, &dtb, initrd.as_deref(), &cmdline)?;
    Ok(boot)
}
