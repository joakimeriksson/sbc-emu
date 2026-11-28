//! A minimal Tegra234 device tree for direct Linux boot.
use crate::{
    AFFINITIES, GICD, GICD_SIZE, GICR, GICR_SIZE, Machine, RAM_BASE, UART, UART_SIZE, UART_SPI,
    UARTA, UARTA_SPI,
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

fn device_tree(machine: &Machine, cmdline: &str, initrd: Option<(u64, u64)>) -> Vec<u8> {
    let ram_bytes = machine.soc.ram.len() as u64;
    let cores = machine.cpus.len();
    let high_speed_uart = machine.soc.uarta.high_speed();
    let bpmp = machine.soc.firmware.is_some();
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
    if let Some((width, height)) = machine.display_size() {
        dt.cells("#address-cells", &[2]);
        dt.cells("#size-cells", &[2]);
        dt.empty("ranges");
        dt.begin(&format!("framebuffer@{:x}", crate::FRAMEBUFFER));
        dt.str("compatible", "simple-framebuffer");
        dt.cells(
            "reg",
            &range(crate::FRAMEBUFFER, u64::from(width * height * 4)),
        );
        dt.cells("width", &[width]);
        dt.cells("height", &[height]);
        dt.cells("stride", &[width * 4]);
        dt.str("format", "x8r8g8b8");
        dt.end();
    }
    dt.end();

    dt.begin("memory@80000000");
    dt.str("device_type", "memory");
    dt.cells("reg", &range(RAM_BASE, ram_bytes));
    dt.end();

    if machine.display_size().is_some() {
        dt.begin("reserved-memory");
        dt.cells("#address-cells", &[2]);
        dt.cells("#size-cells", &[2]);
        dt.empty("ranges");
        dt.begin(&format!("framebuffer@{:x}", crate::FRAMEBUFFER));
        dt.cells(
            "reg",
            &range(crate::FRAMEBUFFER, crate::FRAMEBUFFER_RESERVATION),
        );
        dt.empty("no-map");
        dt.end();
        dt.end();
    }

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
    if machine.soc.gpu_node {
        dt.begin("tegra-carveouts");
        dt.str("compatible", "nvidia,carveouts-t19x");
        dt.end();
        dt.begin("fuse@3810000");
        dt.str("compatible", "nvidia,tegra234-efuse");
        dt.cells("reg", &range(crate::EFUSE, crate::EFUSE_SIZE));
        dt.cells("clocks", &[BPMP_HANDLE, crate::bpmp::CLK_FUSE]);
        dt.str("clock-names", "fuse");
        dt.end();
        host1x_node(&mut dt);
        gpu_node(&mut dt);
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

    if machine.usb_enabled() {
        dt.begin("usb@3610000");
        dt.str("compatible", "generic-xhci");
        dt.cells("reg", &range(crate::XHCI, crate::XHCI_SIZE));
        dt.cells("interrupts", &[0, crate::XHCI_SPI, 4]);
        dt.empty("dma-coherent");
        dt.end();
    }

    if machine.wifi_enabled() {
        pcie_nodes(&mut dt);
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

const P2U_HANDLE: u32 = 24;

/// PCIe controller C1 and its lane's P2U PHY, as in the Orin Nano's own device tree
/// (`tegra234-p3768-0000+p3767-0004.dtb`) less the SMMU (`iommu-map`), the memory
/// interconnects and the regulator (a dummy stands in).
fn pcie_nodes(dt: &mut Fdt) {
    use crate::{bpmp, pcie};
    dt.begin("phy@3e30000");
    dt.str("compatible", "nvidia,tegra234-p2u");
    dt.cells("reg", &range(pcie::P2U, pcie::P2U_SIZE));
    dt.str("reg-names", "ctl");
    dt.cells("#phy-cells", &[0]);
    dt.cells("phandle", &[P2U_HANDLE]);
    dt.end();

    dt.begin("pcie@14100000");
    dt.str("compatible", "nvidia,tegra234-pcie");
    dt.cells("power-domains", &[BPMP_HANDLE, bpmp::PCIEX1A]);
    dt.cells(
        "reg",
        &[
            range(pcie::APPL, pcie::APPL_SIZE),
            range(pcie::CONFIG, pcie::CONFIG_SIZE),
            range(pcie::ATU_DMA, pcie::ATU_DMA_SIZE),
            range(pcie::DBI, pcie::DBI_SIZE),
        ]
        .concat(),
    );
    dt.strs("reg-names", &["appl", "config", "atu_dma", "dbi"]);
    dt.cells("#address-cells", &[3]);
    dt.cells("#size-cells", &[2]);
    dt.str("device_type", "pci");
    dt.cells("num-lanes", &[1]);
    dt.cells("num-viewport", &[8]);
    dt.cells("linux,pci-domain", &[1]);
    dt.cells("clocks", &[BPMP_HANDLE, bpmp::CLK_PEX1_C1_CORE]);
    dt.str("clock-names", "core");
    dt.cells("resets", &[BPMP_HANDLE, bpmp::RESET_PEX1_C1_APB, BPMP_HANDLE, bpmp::RESET_PEX1_C1_CORE]);
    dt.strs("reset-names", &["apb", "core"]);
    dt.cells("interrupts", &[0, pcie::INTR_SPI, 4, 0, pcie::MSI_SPI, 4]);
    dt.strs("interrupt-names", &["intr", "msi"]);
    dt.cells("#interrupt-cells", &[1]);
    dt.cells("interrupt-map-mask", &[0, 0, 0, 0]);
    dt.cells("interrupt-map", &[0, 0, 0, 0, 1, 0, pcie::INTR_SPI, 4]);
    dt.cells("nvidia,bpmp", &[BPMP_HANDLE, pcie::CONTROLLER]);
    dt.cells("nvidia,aspm-cmrt-us", &[60]);
    dt.cells("nvidia,aspm-pwr-on-t-us", &[20]);
    dt.cells("nvidia,aspm-l0s-entrance-latency-us", &[3]);
    dt.cells("bus-range", &[0, 0xff]);
    let [pref_hi, pref_lo] = address(pcie::PREF);
    let [mem_hi, mem_lo] = address(pcie::MEM);
    dt.cells(
        "ranges",
        &[
            0x4300_0000, pref_hi, pref_lo, pref_hi, pref_lo, 0, pcie::PREF_SIZE as u32,
            0x0200_0000, 0, pcie::MEM_BUS as u32, mem_hi, mem_lo, 0, pcie::MEM_SIZE as u32,
            0x0100_0000, 0, pcie::IO as u32, 0, pcie::IO as u32, 0, pcie::IO_SIZE as u32,
        ],
    );
    dt.empty("dma-coherent");
    dt.cells("phys", &[P2U_HANDLE]);
    dt.str("phy-names", "p2u-0");
    dt.end();
}

fn host1x_node(dt: &mut Fdt) {
    use crate::{bpmp, host1x};

    dt.begin("host1x@13e00000");
    dt.str("compatible", "nvidia,tegra234-host1x");
    dt.cells(
        "reg",
        &[
            range(host1x::COMMON, host1x::WINDOW),
            range(host1x::HYPERVISOR, host1x::WINDOW),
            range(host1x::VM, host1x::WINDOW),
            range(host1x::ACTMON, host1x::ACTMON_SIZE),
        ]
        .concat(),
    );
    dt.strs("reg-names", &["common", "hypervisor", "vm", "actmon"]);
    dt.cells(
        "interrupts",
        &[448, 449, 450, 451, 452, 453, 454, 455, 263]
            .map(|spi| [0, spi, 4])
            .concat(),
    );
    dt.strs(
        "interrupt-names",
        &[
            "syncpt0", "syncpt1", "syncpt2", "syncpt3", "syncpt4", "syncpt5", "syncpt6", "syncpt7",
            "host1x",
        ],
    );
    dt.cells(
        "clocks",
        &[BPMP_HANDLE, bpmp::CLK_HOST1X, BPMP_HANDLE, bpmp::CLK_ACTMON],
    );
    dt.strs("clock-names", &["host1x", "actmon"]);
    dt.empty("dma-coherent");
    dt.cells("#address-cells", &[2]);
    dt.cells("#size-cells", &[2]);
    dt.empty("ranges");

    dt.begin("vic@15340000");
    dt.str("compatible", "nvidia,tegra234-vic");
    dt.cells("reg", &range(0x15340000, 0x40000));
    dt.cells("interrupts", &[0, 206, 4]);
    dt.cells("clocks", &[BPMP_HANDLE, bpmp::CLK_VIC]);
    dt.str("clock-names", "vic");
    dt.cells("resets", &[BPMP_HANDLE, bpmp::RESET_VIC]);
    dt.str("reset-names", "vic");
    dt.empty("dma-coherent");
    dt.end();
    dt.end();
}

fn gpu_node(dt: &mut Fdt) {
    use crate::{bpmp, gpu};

    dt.begin("gpu@17000000");
    dt.str("compatible", "nvidia,ga10b");
    dt.cells(
        "reg",
        &[
            range(gpu::BAR0, gpu::SIZE),
            range(gpu::BAR1, gpu::SIZE),
            range(gpu::FUSE, gpu::FUSE_SIZE),
        ]
        .concat(),
    );
    dt.cells("interrupts", &gpu::SPIS.map(|spi| [0, spi, 4]).concat());
    dt.strs(
        "interrupt-names",
        &["stall0", "stall1", "stall2", "nonstall"],
    );
    dt.cells("power-domains", &[BPMP_HANDLE, bpmp::GPU]);
    dt.cells(
        "clocks",
        &[
            BPMP_HANDLE,
            bpmp::CLK_GPUSYS,
            BPMP_HANDLE,
            bpmp::CLK_GPC0,
            BPMP_HANDLE,
            bpmp::CLK_GPC1,
        ],
    );
    dt.strs("clock-names", &["sysclk", "gpc0clk", "gpc1clk"]);
    dt.cells("resets", &[BPMP_HANDLE, bpmp::RESET_GPU]);
    dt.cells("nvidia,bpmp", &[BPMP_HANDLE]);
    dt.empty("dma-coherent");
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
            if machine.display_size().is_some() && RAM_BASE + end > crate::FRAMEBUFFER {
                return Err("initrd overlaps the framebuffer");
            }
            Ok((RAM_BASE + INITRD_OFFSET, RAM_BASE + end))
        })
        .transpose()?;
    let dtb = device_tree(machine, cmdline, initrd_range);
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
