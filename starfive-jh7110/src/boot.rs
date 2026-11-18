//! Direct boot the way the board's firmware boots: OpenSBI's `fw_dynamic` at the start of
//! DRAM (where U-Boot SPL's FIT puts it) on every U74 hart, with a `fw_dynamic_info`
//! naming the kernel as the next stage in S mode, and the distribution's device tree
//! adjusted as U-Boot adjusts it (`/chosen` command line and initrd, `/memory`). Load
//! addresses are U-Boot's `starfive_visionfive2` defaults (`kernel_addr_r`, `fdt_addr_r`,
//! `ramdisk_addr_r`).
use crate::machine::{HartState, Machine, RAM_BASE};
use emu_core::fdt::Node;

pub const OPENSBI_ADDR: u64 = 0x4000_0000;
pub const KERNEL_ADDR: u64 = 0x4020_0000;
pub const FW_INFO_ADDR: u64 = 0x45ff_f000;
pub const DTB_ADDR: u64 = 0x4600_0000;
pub const INITRD_ADDR: u64 = 0x4610_0000;
pub const DEFAULT_CMDLINE: &str = "console=ttyS0,115200 earlycon rdinit=/init";

/// `struct fw_dynamic_info` (OpenSBI include/sbi/fw_dynamic.h), version 2.
fn fw_dynamic_info(next: u64, boot_hart: u64) -> Vec<u8> {
    [0x4942_534f, 2, next, 1 /* S mode */, 0, boot_hart]
        .iter()
        .flat_map(|v: &u64| v.to_le_bytes())
        .collect()
}

/// Adjust a stock JH7110 board DTB for direct boot.
pub fn prepare_device_tree(stock: &[u8], ram_bytes: u64, cmdline: &str, initrd: Option<(u64, u64)>) -> Result<Vec<u8>, String> {
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
    Ok(root.to_blob())
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
    let ram_bytes = m.soc.ram.len() as u64;
    let initrd_range = initrd.map(|d| (INITRD_ADDR, INITRD_ADDR + d.len() as u64));
    let dtb = prepare_device_tree(stock_dtb, ram_bytes, cmdline, initrd_range)?;
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
