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

fn device_tree(
    ram_bytes: u64,
    cores: usize,
    high_speed_uart: bool,
    bpmp: bool,
    cmdline: &str,
    initrd: Option<(u64, u64)>,
) -> Vec<u8> {
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

    dt.begin("misc@100000");
    dt.str("compatible", "nvidia,tegra234-misc");
    dt.cells(
        "reg",
        &[
            0,
            crate::APB_MISC as u32,
            0,
            0xf000,
            0,
            crate::APB_STRAPS as u32,
            0,
            0x1000,
        ],
    );
    dt.end();

    if bpmp {
        bpmp_nodes(&mut dt);
    }
    if high_speed_uart && !bpmp {
        dt.begin("uarta-clock-parent");
        dt.str("compatible", "fixed-clock");
        dt.cells("#clock-cells", &[0]);
        dt.cells("phandle", &[4]);
        dt.cells("clock-frequency", &[crate::uart::CLOCK_HZ]);
        dt.end();

        // serial-tegra computes its termios baud bounds from the parent.
        dt.begin("uarta-clock");
        dt.str("compatible", "fixed-factor-clock");
        dt.cells("#clock-cells", &[0]);
        dt.cells("phandle", &[2]);
        dt.cells("clocks", &[4]);
        dt.cells("clock-mult", &[1]);
        dt.cells("clock-div", &[1]);
        dt.end();

        dt.begin("reset-controller@70000000");
        dt.str("compatible", "snps,dw-high-reset");
        dt.cells("reg", &range(crate::uart::FIXTURE_RESET, 4));
        dt.cells("#reset-cells", &[1]);
        dt.cells("phandle", &[3]);
        dt.end();
    }

    dt.begin("serial@3100000");
    dt.cells("reg", &range(UARTA, UART_SIZE));
    dt.cells("interrupts", &[0, UARTA_SPI, 4]);
    if high_speed_uart {
        dt.str("compatible", "nvidia,tegra194-hsuart");
        if bpmp {
            dt.cells("clocks", &[BPMP_HANDLE, crate::bpmp::CLK_UARTA]);
            dt.cells("resets", &[BPMP_HANDLE, crate::bpmp::RESET_UARTA]);
        } else {
            dt.cells("clocks", &[2]);
            dt.cells("resets", &[3, 0]);
        }
        dt.str("reset-names", "serial");
    } else {
        dt.strs(
            "compatible",
            &["nvidia,tegra234-uart", "nvidia,tegra20-uart"],
        );
        dt.cells("clock-frequency", &[crate::uart::CLOCK_HZ]);
    }
    dt.end();

    dt.end();
    dt.finish()
}

const HSP_HANDLE: u32 = 20;
const TX_HANDLE: u32 = 21;
const RX_HANDLE: u32 = 22;
const BPMP_HANDLE: u32 = 23;

fn bpmp_nodes(dt: &mut Fdt) {
    use crate::{bpmp, hsp};
    dt.begin("mailbox@3c00000");
    dt.strs(
        "compatible",
        &["nvidia,tegra234-hsp", "nvidia,tegra194-hsp"],
    );
    dt.cells("reg", &range(hsp::BASE, hsp::SIZE));
    dt.cells("interrupts", &[0, hsp::SPI, 4]);
    dt.str("interrupt-names", "doorbell");
    dt.cells("#mbox-cells", &[2]);
    dt.cells("phandle", &[HSP_HANDLE]);
    dt.end();

    dt.begin("sram@40000000");
    dt.strs("compatible", &["nvidia,tegra234-sysram", "mmio-sram"]);
    dt.cells("reg", &range(bpmp::SRAM, bpmp::SRAM_SIZE));
    dt.cells("#address-cells", &[1]);
    dt.cells("#size-cells", &[1]);
    dt.cells("ranges", &[0, 0, bpmp::SRAM as u32, bpmp::SRAM_SIZE as u32]);
    dt.empty("no-memory-wc");
    for (offset, handle, name) in [
        (bpmp::TX_POOL, TX_HANDLE, "cpu-bpmp-tx"),
        (bpmp::RX_POOL, RX_HANDLE, "cpu-bpmp-rx"),
    ] {
        dt.begin(&format!("sram@{offset:x}"));
        dt.cells("reg", &[offset as u32, bpmp::POOL_SIZE as u32]);
        dt.str("label", name);
        dt.empty("pool");
        dt.cells("phandle", &[handle]);
        dt.end();
    }
    dt.end();

    dt.begin("bpmp");
    dt.strs(
        "compatible",
        &["nvidia,tegra234-bpmp", "nvidia,tegra186-bpmp"],
    );
    dt.cells("mboxes", &[HSP_HANDLE, 0, hsp::BPMP_MASTER]);
    dt.cells("shmem", &[TX_HANDLE, RX_HANDLE]);
    dt.cells("#clock-cells", &[1]);
    dt.cells("#reset-cells", &[1]);
    dt.cells("#power-domain-cells", &[1]);
    dt.cells("phandle", &[BPMP_HANDLE]);
    dt.end();
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
    let dtb = device_tree(
        ram_bytes,
        machine.cpus.len(),
        machine.soc.uarta.high_speed(),
        machine.soc.firmware.is_some(),
        cmdline,
        initrd_range,
    );
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
