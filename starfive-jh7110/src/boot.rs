//! Direct boot the way the board's firmware boots: OpenSBI's `fw_dynamic` at the start of
//! DRAM (where U-Boot SPL's FIT puts it) on every U74 hart, with a `fw_dynamic_info`
//! naming the kernel as the next stage in S mode, and the distribution's device tree
//! adjusted as U-Boot adjusts it (`/chosen` command line and initrd, `/memory`). Load
//! addresses are U-Boot's `starfive_visionfive2` defaults (`kernel_addr_r`, `fdt_addr_r`,
//! `ramdisk_addr_r`).
use crate::machine::{HartState, Machine, RAM_BASE, SRAM_BASE, SRAM_SIZE};
use emu_core::fdt::Node;

pub const OPENSBI_ADDR: u64 = 0x4000_0000;
pub const KERNEL_ADDR: u64 = 0x4020_0000;
pub const FW_INFO_ADDR: u64 = 0x45ff_f000;
pub const DTB_ADDR: u64 = 0x4600_0000;
pub const INITRD_ADDR: u64 = 0x4610_0000;
pub const DEFAULT_CMDLINE: &str = "console=ttyS0,115200 earlycon rdinit=/init";
/// The firmware framebuffer's slot: the top 16 MiB of RAM below 4 GiB (the DC8200's
/// framebuffer address register is 32 bits).
pub const FB_SLOT: u64 = 16 << 20;

/// A framebuffer the boot firmware set up: (address, width, height), XRGB8888.
pub type Framebuffer = (u64, u32, u32);

/// Where the firmware framebuffer goes for `ram_bytes` of RAM.
pub fn framebuffer_addr(ram_bytes: u64) -> u64 {
    (RAM_BASE + ram_bytes).min(1 << 32) - FB_SLOT
}

/// Whether the device tree describes the display controller (a kernel with its own
/// display driver, such as Xunlong's): then the firmware hands over no framebuffer.
pub fn has_display_node(stock: &[u8]) -> bool {
    stock.windows(7).any(|w| w == b"dc8200@") || stock.windows(17).any(|w| w == b"verisilicon,dc820")
}

/// `struct fw_dynamic_info` (OpenSBI include/sbi/fw_dynamic.h), version 2.
fn fw_dynamic_info(next: u64, boot_hart: u64) -> Vec<u8> {
    [0x4942_534f, 2, next, 1 /* S mode */, 0, boot_hart]
        .iter()
        .flat_map(|v: &u64| v.to_le_bytes())
        .collect()
}

/// Adjust a stock JH7110 board DTB for direct boot.
///
/// With `framebuffer`, the firmware also hands over the HDMI framebuffer it set up, as
/// U-Boot does on boards whose display mainline drives through `simpledrm`: a no-map
/// `/reserved-memory` region and a `/chosen/framebuffer` `simple-framebuffer` node.
pub fn prepare_device_tree(stock: &[u8], ram_bytes: u64, cmdline: &str, initrd: Option<(u64, u64)>, framebuffer: Option<Framebuffer>) -> Result<Vec<u8>, String> {
    let mut root = Node::parse(stock)?;
    let compatible = root.prop("compatible").unwrap_or_default();
    if !compatible.windows(15).any(|w| w == b"starfive,jh7110") {
        return Err("device tree is not for a StarFive JH7110 board".into());
    }
    let memory = root.child_mut("memory");
    memory.set_str("device_type", "memory");
    memory.set_cells("reg", &[0, RAM_BASE as u32, (ram_bytes >> 32) as u32, ram_bytes as u32]);
    let chosen = root.child_mut("chosen");
    chosen.set_str("bootargs", cmdline);
    if let Some((start, end)) = initrd {
        chosen.set_cells("linux,initrd-start", &[(start >> 32) as u32, start as u32]);
        chosen.set_cells("linux,initrd-end", &[(end >> 32) as u32, end as u32]);
    }
    if let Some((addr, width, height)) = framebuffer {
        let reg = [(addr >> 32) as u32, addr as u32, 0, FB_SLOT as u32];
        let name = format!("framebuffer@{addr:x}");
        let phandle = max_phandle(&root) + 1;
        let rsv = root.child_mut("reserved-memory");
        if rsv.prop("#address-cells").is_none() {
            rsv.set_cells("#address-cells", &[2]);
            rsv.set_cells("#size-cells", &[2]);
            rsv.set("ranges", Vec::new());
        }
        let region = rsv.child_mut(&name);
        region.set_cells("reg", &reg);
        region.set("no-map", Vec::new());
        region.set_cells("phandle", &[phandle]);
        let fb = root.child_mut("chosen").child_mut(&name);
        fb.set_str("compatible", "simple-framebuffer");
        fb.set_cells("memory-region", &[phandle]);
        fb.set_cells("width", &[width]);
        fb.set_cells("height", &[height]);
        fb.set_cells("stride", &[width * 4]);
        fb.set_str("format", "x8r8g8b8");
        fb.set_str("status", "okay");
    }
    add_gpu_node(&mut root);
    Ok(root.to_blob())
}

/// The PowerVR BXE-4-32 GPU's registers and interrupt.
pub const GPU_BASE: u64 = 0x1800_0000;
pub const IRQ_GPU: u32 = 82;

/// Mainline's JH7110 device tree has no GPU node yet, so the firmware adds one from the
/// board's resources, as mainline's `img,img-rogue` binding describes it. Xunlong's tree has
/// its own node, for its vendor driver.
fn add_gpu_node(root: &mut Node) {
    let Some(soc) = root.children.iter_mut().find(|c| c.name == "soc") else { return };
    if soc.children.iter().any(|c| c.name.starts_with("gpu@")) {
        return;
    }
    let phandle = |prefix: &str| {
        soc.children
            .iter()
            .find(|c| c.name.starts_with(prefix))
            .and_then(|c| c.prop("phandle"))
            .and_then(|p| p.get(..4))
            .map(|b| u32::from_be_bytes([b[0], b[1], b[2], b[3]]))
    };
    let (Some(syscrg), Some(pmu)) = (phandle("clock-controller@13020000"), phandle("power-controller@17030000")) else {
        return;
    };
    let gpu = soc.child_mut(&format!("gpu@{GPU_BASE:x}"));
    gpu.set("compatible", b"starfive,jh7110-gpu\0img,img-bxe-4-32\0img,img-rogue\0".to_vec());
    gpu.set_cells("reg", &[0, GPU_BASE as u32, 0, 0x10_0000]);
    // JH7110_SYSCLK_GPU_CORE_CLK, _NOC_BUS_GPU_AXI, _GPU_SYS_CLK
    gpu.set_cells("clocks", &[syscrg, 46, syscrg, 50, syscrg, 47]);
    gpu.set("clock-names", b"core\0mem\0sys\0".to_vec());
    gpu.set_cells("interrupts", &[IRQ_GPU]);
    // JH7110_PD_GPUA; JH7110_SYSRST_GPU_DOMA
    gpu.set_cells("power-domains", &[pmu, 2]);
    gpu.set_cells("resets", &[syscrg, 22]);
    gpu.set_str("status", "okay");
}

/// The largest phandle in the tree.
fn max_phandle(node: &Node) -> u32 {
    let own = node.prop("phandle").and_then(|p| p.get(..4)).map_or(0, |b| u32::from_be_bytes([b[0], b[1], b[2], b[3]]));
    node.children.iter().map(max_phandle).fold(own, u32::max)
}

/// Load OpenSBI, the kernel `Image`, the patched DTB and an optional initrd, and start the
/// U74 harts in OpenSBI. Returns the DTB handed to OpenSBI.
pub fn boot_linux(m: &mut Machine, opensbi: &[u8], kernel: &[u8], stock_dtb: &[u8], initrd: Option<&[u8]>, cmdline: &str) -> Result<Vec<u8>, String> {
    if kernel.get(0x30..0x38) != Some(b"RISCV\0\0\0") {
        return Err("kernel is not a RISC-V Linux Image (magic \"RISCV\" at 0x30 missing)".into());
    }
    let image_size = u64::from_le_bytes(kernel[0x10..0x18].try_into().unwrap());
    if KERNEL_ADDR + image_size.max(kernel.len() as u64) > FW_INFO_ADDR {
        return Err("kernel image too large for its load slot".into());
    }
    if opensbi.len() as u64 > KERNEL_ADDR - OPENSBI_ADDR {
        return Err("OpenSBI firmware too large for its load slot".into());
    }
    let ram_bytes = m.soc.ram.mem.len() as u64;
    let initrd_range = initrd.map(|d| (INITRD_ADDR, INITRD_ADDR + d.len() as u64));
    // a kernel without a display driver gets the firmware's framebuffer on HDMI
    let framebuffer = (!has_display_node(stock_dtb)).then(|| {
        let addr = framebuffer_addr(ram_bytes);
        m.soc.display.firmware_scanout(addr as u32);
        (addr, m.soc.display.mode.width, m.soc.display.mode.height)
    });
    let dtb = prepare_device_tree(stock_dtb, ram_bytes, cmdline, initrd_range, framebuffer)?;
    if (INITRD_ADDR - DTB_ADDR) as usize <= dtb.len() {
        return Err("device tree too large for its load slot".into());
    }
    let ram_mb = ram_bytes >> 20;
    let fit = |what: &'static str| move |_| format!("{what} does not fit in {ram_mb} MiB of RAM");
    m.load(OPENSBI_ADDR, opensbi).map_err(fit("OpenSBI"))?;
    m.load(KERNEL_ADDR, kernel).map_err(fit("kernel"))?;
    m.load(FW_INFO_ADDR, &fw_dynamic_info(KERNEL_ADDR, 1)).map_err(fit("fw_dynamic_info"))?;
    m.load(DTB_ADDR, &dtb).map_err(fit("device tree"))?;
    if let Some(d) = initrd {
        m.load(INITRD_ADDR, d).map_err(fit("initrd"))?;
    }
    for h in 1..m.harts.len() {
        let hart = &mut m.harts[h];
        hart.reset(OPENSBI_ADDR);
        hart.x[10] = h as u64;
        hart.x[11] = DTB_ADDR;
        hart.x[12] = FW_INFO_ADDR;
        m.state[h] = HartState::Run;
    }
    Ok(dtb)
}

/// The board's 24C04 EEPROM in StarFive's HAT-style format, which U-Boot reads to tell the
/// board (`XOPIRV`) and its DRAM size: a header (`SFVF`), the vendor atom with the product
/// string, and the custom atom with the PCB revision and MAC addresses, each with U-Boot's
/// CRC-16. The rest reads as erased.
pub fn board_eeprom(ram_bytes: u64) -> [u8; 512] {
    let mut e = [0xffu8; 512];
    // U-Boot knows the board's DRAM sizes only (2, 4, 8 or 16 GiB): the largest that fits
    let gib = eeprom_ram_gib(ram_bytes);
    let pstr = format!("XOPIRVA1-2450-D{gib:03X}E000-00000001");
    let mut out = Vec::new();
    // header: signature, format version (2, as U-Boot writes it), reserved, number of
    // atoms, total length
    out.extend_from_slice(b"SFVF");
    out.extend_from_slice(&[2, 0]);
    out.extend_from_slice(&2u16.to_le_bytes());
    out.extend_from_slice(&136u32.to_le_bytes());
    // atom 1, vendor info: uuid, product id/version, string lengths, vendor and product
    let mut a1 = Vec::new();
    a1.extend_from_slice(&[0u8; 16]);
    a1.extend_from_slice(&0u16.to_le_bytes());
    a1.extend_from_slice(&0u16.to_le_bytes());
    a1.extend_from_slice(&[32, 32]);
    let mut vstr = [0u8; 32];
    let vendor = b"Shenzhen Xunlong Software Co.";
    vstr[..vendor.len()].copy_from_slice(vendor);
    a1.extend_from_slice(&vstr);
    let mut p = [0u8; 32];
    p[..pstr.len()].copy_from_slice(pstr.as_bytes());
    a1.extend_from_slice(&p);
    // atom 4, custom data: format version, PCB and BOM revision, MAC0/MAC1, Wi-Fi/BT module
    let mut a4 = Vec::new();
    a4.extend_from_slice(&2u16.to_le_bytes());
    a4.extend_from_slice(&[0xa1, b'A']);
    a4.extend_from_slice(&[0x6c, 0xcf, 0x39, 0x6c, 0xde, 0xad]);
    a4.extend_from_slice(&[0x6c, 0xcf, 0x39, 0x6c, 0xde, 0xae]);
    a4.extend_from_slice(&[1, 0]);
    for (ty, count, data) in [(1u16, 1u16, a1), (4, 2, a4)] {
        let start = out.len();
        out.extend_from_slice(&ty.to_le_bytes());
        out.extend_from_slice(&count.to_le_bytes());
        out.extend_from_slice(&((data.len() + 2) as u32).to_le_bytes());
        out.extend_from_slice(&data);
        let crc = crc16(&out[start..]);
        out.extend_from_slice(&crc.to_le_bytes());
    }
    e[..out.len()].copy_from_slice(&out);
    e
}

/// The DRAM size the EEPROM names: U-Boot accepts 2, 4, 8 and 16 GiB.
pub fn eeprom_ram_gib(ram_bytes: u64) -> u64 {
    [16, 8, 4, 2].into_iter().find(|&g| ram_bytes >= g << 30).unwrap_or(2)
}

/// Whether U-Boot can run with this much RAM (it takes the size from the EEPROM).
pub fn uboot_ram_ok(ram_bytes: u64) -> bool {
    [2u64, 4, 8, 16].iter().any(|&g| ram_bytes == g << 30)
}

/// U-Boot's `crc16`: polynomial 0x8005, reflected, initial value 0.
fn crc16(data: &[u8]) -> u16 {
    let mut crc = 0u16;
    for &b in data {
        crc ^= b as u16;
        for _ in 0..8 {
            crc = if crc & 1 != 0 { (crc >> 1) ^ 0xa001 } else { crc >> 1 };
        }
    }
    crc
}

fn crc32(data: &[u8]) -> u32 {
    let mut crc = !0u32;
    for &b in data {
        crc ^= b as u32;
        for _ in 0..8 {
            crc = if crc & 1 != 0 { (crc >> 1) ^ 0xedb8_8320 } else { crc >> 1 };
        }
    }
    !crc
}

/// Where the boot ROM looks for SPL on an SD card: at the start of the card, then at the
/// header's backup offset (2 MiB, the start of the `spl` partition on StarFive's layout).
const SPL_COPIES: [u64; 2] = [0, 0x20_0000];
const SPL_VERSION: u32 = 0x0101_0101;

/// The JH7110 boot ROM's SD path. The U74s start in the mask ROM, which reads U-Boot SPL
/// (`u-boot-spl.bin.normal.out`: a 0x400-byte header with version, size and CRC-32 at
/// 0x284) from the card, copies it into the L2 LIM and jumps there on every U74.
pub fn boot_rom(m: &mut Machine) -> Result<(), String> {
    let disk = m.soc.sdio1.card.disk_mut().ok_or("no SD card: the boot ROM boots from --sd (or give --kernel)")?;
    let mut found = None;
    for off in SPL_COPIES {
        let mut hdr = [0u8; 0x400];
        if off / 512 + 2 > disk.sectors() {
            continue;
        }
        disk.read(off / 512, &mut hdr);
        let word = |o: usize| u32::from_le_bytes(hdr[o..o + 4].try_into().unwrap());
        let (version, size, hdr_size, crc) = (word(0x284), word(0x288) as usize, word(0x28c) as u64, word(0x290));
        if version != SPL_VERSION || size == 0 || size > SRAM_SIZE || hdr_size % 512 != 0 {
            continue;
        }
        let mut body = vec![0u8; size.div_ceil(512) * 512];
        if (off + hdr_size) / 512 + body.len() as u64 / 512 > disk.sectors() {
            continue;
        }
        disk.read((off + hdr_size) / 512, &mut body);
        body.truncate(size);
        if crc32(&body) == crc {
            found = Some(body);
            break;
        }
    }
    let spl = found.ok_or("no valid U-Boot SPL on the SD card (boot ROM header at 0 or 2 MiB)")?;
    m.soc.sram.mem[..spl.len()].copy_from_slice(&spl);
    for h in 1..m.harts.len() {
        m.harts[h].reset(SRAM_BASE);
        m.harts[h].x[10] = h as u64;
        m.state[h] = HartState::Run;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use emu_core::disk::MemDisk;

    /// A `u-boot-spl.bin.normal.out`-style image of `body`.
    fn spl_image(body: &[u8]) -> Vec<u8> {
        let mut img = vec![0u8; 0x400];
        img[0..4].copy_from_slice(&0x240u32.to_le_bytes());
        img[4..8].copy_from_slice(&0x20_0000u32.to_le_bytes());
        img[0x284..0x288].copy_from_slice(&SPL_VERSION.to_le_bytes());
        img[0x288..0x28c].copy_from_slice(&(body.len() as u32).to_le_bytes());
        img[0x28c..0x290].copy_from_slice(&0x400u32.to_le_bytes());
        img[0x290..0x294].copy_from_slice(&crc32(body).to_le_bytes());
        img.extend_from_slice(body);
        img
    }

    fn machine_with_card(image: &[u8], at: usize) -> Machine {
        let mut disk = MemDisk::new(8192);
        disk.0[at..at + image.len()].copy_from_slice(image);
        let mut m = Machine::new(128 << 20);
        m.soc.sdio1.insert(Some(Box::new(disk)));
        m
    }

    #[test]
    fn boot_rom_loads_spl_from_the_backup_copy() {
        let body: Vec<u8> = (0..3000u32).map(|i| i as u8).collect();
        // sector 0 holds a protective MBR, as on a GPT card: the ROM falls back to 2 MiB
        let mut img = vec![0u8; 512];
        img[446 + 4] = 0xee; // one partition of type 0xEE (GPT protective) covering the disk
        img[446 + 8] = 1;
        img[446 + 12..446 + 16].copy_from_slice(&0xffff_ffffu32.to_le_bytes());
        img[510] = 0x55;
        img[511] = 0xaa;
        img.resize(0x20_0000, 0);
        img.extend_from_slice(&spl_image(&body));
        let mut m = machine_with_card(&img, 0);
        boot_rom(&mut m).unwrap();
        assert_eq!(&m.soc.sram.mem[..body.len()], &body[..]);
        for h in 1..m.harts.len() {
            assert_eq!(m.harts[h].pc, SRAM_BASE);
            assert_eq!(m.harts[h].x[10], h as u64);
            assert_eq!(m.state[h], HartState::Run);
        }
        assert_eq!(m.state[0], HartState::Off, "the S7 stays in reset");
    }

    #[test]
    fn boot_rom_rejects_a_bad_crc() {
        let mut img = spl_image(&[1, 2, 3, 4]);
        img[0x400] ^= 0xff;
        let mut m = machine_with_card(&img, 0);
        assert!(boot_rom(&mut m).is_err());
    }

    #[test]
    fn eeprom_names_a_size_uboot_knows() {
        assert_eq!(eeprom_ram_gib(4 << 30), 4);
        assert_eq!(eeprom_ram_gib(3 << 30), 2);
        assert_eq!(eeprom_ram_gib(1 << 30), 2);
        assert!(uboot_ram_ok(8 << 30) && !uboot_ram_ok(3 << 30));
    }

    #[test]
    fn eeprom_identifies_the_board() {
        let e = board_eeprom(4 << 30);
        assert_eq!(&e[0..4], b"SFVF");
        // atom 1's product string starts at 12 + 8 + 16 + 2 + 2 + 2 + 32
        let pstr = &e[74..74 + 31];
        assert!(pstr.starts_with(b"XOPIRV"));
        // U-Boot's DDR size: hex digits at pstr[14..], bits 23:16
        let hex = std::str::from_utf8(&pstr[14..22]).unwrap();
        assert_eq!((u32::from_str_radix(hex, 16).unwrap() >> 16) & 0xff, 4);
        // each atom's CRC-16 covers its header and data
        assert_eq!(u16::from_le_bytes([e[106], e[107]]), crc16(&e[12..106]));
        assert_eq!(u16::from_le_bytes([e[134], e[135]]), crc16(&e[108..134]));
    }
}
