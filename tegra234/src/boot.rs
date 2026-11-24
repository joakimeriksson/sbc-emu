//! A minimal Tegra234 device tree for direct Linux boot.
use crate::{
    Machine, AFFINITIES, GICD, GICD_SIZE, GICR, GICR_SIZE, RAM_BASE, UART, UARTA, UARTA_SPI,
    UART_SIZE, UART_SPI,
};
use emu_core::{arm64_image::kernel_image, fdt::Fdt};

const KERNEL_REGION: u64 = 0x0020_0000;
const DTB_OFFSET: u64 = 0x0800_0000;
const INITRD_OFFSET: u64 = 0x0820_0000;
pub const DEFAULT_CMDLINE: &str =
    "earlycon=pl011,mmio32,0x031d0000 console=ttyAMA0,115200 rdinit=/init";

fn address(value: u64) -> [u32; 2] {
    [(value >> 32) as u32, value as u32]
}

fn range(base: u64, bytes: u64) -> [u32; 4] {
    let [base_hi, base_lo] = address(base);
    let [size_hi, size_lo] = address(bytes);
    [base_hi, base_lo, size_hi, size_lo]
}

fn device_tree(ram_bytes: u64, cores: usize, cmdline: &str, initrd: Option<(u64, u64)>) -> Vec<u8> {
    let mut dt = Fdt::default();
    dt.begin("");
    dt.str("model", "Jetson Orin Nano 4GB (sbc-emu direct boot)");
    dt.strs("compatible", &["nvidia,p3767-0004", "nvidia,tegra234"]);
    dt.cells("#address-cells", &[2]);
    dt.cells("#size-cells", &[2]);
    dt.cells("interrupt-parent", &[1]);

    dt.begin("aliases");
    dt.str("serial0", "/serial@31d0000");
    dt.str("serial1", "/serial@3100000");
    dt.end();

    dt.begin("chosen");
    dt.str("bootargs", cmdline);
    dt.str("stdout-path", "serial0:115200n8");
    if let Some((start, end)) = initrd {
        dt.cells("linux,initrd-start", &address(start));
        dt.cells("linux,initrd-end", &address(end));
    }
    dt.end();

    dt.begin("memory@80000000");
    dt.str("device_type", "memory");
    dt.cells("reg", &range(RAM_BASE, ram_bytes));
    dt.end();

    dt.begin("cpus");
    dt.cells("#address-cells", &[1]);
    dt.cells("#size-cells", &[0]);
    for (core, affinity) in AFFINITIES[..cores].iter().enumerate() {
        dt.begin(&format!("cpu@{affinity:x}"));
        dt.str("device_type", "cpu");
        dt.str("compatible", "arm,cortex-a78ae");
        dt.cells("reg", &[*affinity as u32]);
        dt.cells("phandle", &[10 + core as u32]);
        dt.str("enable-method", "psci");
        dt.end();
    }
    dt.begin("cpu-map");
    for (cluster, members) in [0..cores.min(4), 4..cores].iter().enumerate() {
        if members.is_empty() {
            continue;
        }
        dt.begin(&format!("cluster{cluster}"));
        for (index, core) in members.clone().enumerate() {
            dt.begin(&format!("core{index}"));
            dt.cells("cpu", &[10 + core as u32]);
            dt.end();
        }
        dt.end();
    }
    dt.end();
    dt.end();

    dt.begin("psci");
    dt.str("compatible", "arm,psci-1.0");
    dt.str("method", "smc");
    dt.end();

    dt.begin("interrupt-controller@f400000");
    dt.str("compatible", "arm,gic-v3");
    dt.cells("phandle", &[1]);
    dt.cells("#interrupt-cells", &[3]);
    dt.cells("#redistributor-regions", &[1]);
    dt.empty("interrupt-controller");
    dt.cells(
        "reg",
        &[range(GICD, GICD_SIZE), range(GICR, GICR_SIZE)].concat(),
    );
    dt.end();

    dt.begin("timer");
    dt.str("compatible", "arm,armv8-timer");
    // Tegra234's secure physical, non-secure physical, virtual and hypervisor PPIs.
    dt.cells("interrupts", &[1, 13, 8, 1, 14, 8, 1, 11, 8, 1, 10, 8]);
    dt.empty("always-on");
    dt.end();

    dt.begin("serial@31d0000");
    dt.str("compatible", "arm,sbsa-uart");
    dt.cells("reg", &range(UART, UART_SIZE));
    dt.cells("interrupts", &[0, UART_SPI, 4]);
    dt.cells("current-speed", &[115200]);
    dt.end();

    // The Tegra 8250 driver uses PIO and a fixed reference clock for bring-up.
    dt.begin("serial@3100000");
    dt.strs(
        "compatible",
        &["nvidia,tegra234-uart", "nvidia,tegra20-uart"],
    );
    dt.cells("reg", &range(UARTA, UART_SIZE));
    dt.cells("interrupts", &[0, UARTA_SPI, 4]);
    dt.cells("clock-frequency", &[1_843_200]);
    dt.end();

    dt.end();
    dt.finish()
}

pub fn boot_linux(
    machine: &mut Machine,
    kernel: &[u8],
    initrd: Option<&[u8]>,
    cmdline: &str,
) -> Result<(), String> {
    let image = kernel_image(kernel)?;
    let text_offset = u64::from_le_bytes(image[8..16].try_into().unwrap());
    let image_size = u64::from_le_bytes(image[16..24].try_into().unwrap());
    let entry = KERNEL_REGION
        .checked_add(text_offset)
        .ok_or("kernel text offset overflows")?;
    let image_end = entry
        .checked_add(image_size.max(image.len() as u64))
        .ok_or("kernel size overflows")?;
    if image_end > DTB_OFFSET {
        return Err("kernel overlaps the device tree".into());
    }
    let ram_bytes = machine.soc.ram.len() as u64;
    let initrd_range = initrd
        .map(|bytes| {
            let end = INITRD_OFFSET
                .checked_add(bytes.len() as u64)
                .ok_or("initrd size overflows")?;
            if end > ram_bytes {
                return Err("initrd does not fit in RAM");
            }
            Ok((RAM_BASE + INITRD_OFFSET, RAM_BASE + end))
        })
        .transpose()?;
    let dtb = device_tree(ram_bytes, machine.cpus.len(), cmdline, initrd_range);
    if dtb.len() as u64 > INITRD_OFFSET - DTB_OFFSET {
        return Err("device tree exceeds 2 MiB".into());
    }
    let ram = &mut machine.soc.ram;
    let entry = entry as usize;
    ram[entry..entry + image.len()].copy_from_slice(&image);
    let dtb_offset = DTB_OFFSET as usize;
    ram[dtb_offset..dtb_offset + dtb.len()].copy_from_slice(&dtb);
    if let Some(bytes) = initrd {
        let offset = INITRD_OFFSET as usize;
        ram[offset..offset + bytes.len()].copy_from_slice(bytes);
    }

    let cpu = &mut machine.cpus[0];
    cpu.el = 1;
    cpu.spsel = true;
    cpu.daif = 0x3c0;
    cpu.pc = RAM_BASE + entry as u64;
    cpu.x = [0; 31];
    cpu.x[0] = RAM_BASE + DTB_OFFSET;
    Ok(())
}
