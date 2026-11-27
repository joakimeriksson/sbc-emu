//! The reduced BCM2837 device tree the emulated firmware hands to Linux.
//!
//! Like the Pi 4's (`bcm2711::fdt`), it lists only what is modelled, but its shape follows the
//! vendor `bcm2710-rpi-3-b-plus.dtb`: one address and one size cell, devices under `/soc` at
//! their VideoCore bus addresses (0x7e..., translated to 0x3f... by `ranges`), DMA addresses
//! through the 0xc000_0000 alias (`dma-ranges`), the two interrupt controllers (no GIC), the
//! generic timer on the per-core controller. Cores start through PSCI (the vendor tree uses a
//! spin table; with our own tree either works), clocks are fixed instead of the CPRMAN's.

pub use emu_core::fdt::Fdt;

pub struct BootInfo<'a> {
    pub model: crate::Model,
    /// RAM given to Linux (below the VideoCore's share)
    pub ram: u64,
    pub bootargs: &'a str,
    pub initrd: Option<(u64, u64)>,
    /// firmware-allocated framebuffer: (base, width, height, stride)
    pub fb: Option<(u64, u32, u32, u32)>,
    pub cpus: u32,
}

/// Interrupt flags in the two controllers' second cell.
const LEVEL_HIGH: u32 = 4;
/// ARM-side controller specifiers: (bank, number); bank 1 = GPU 0..31, bank 2 = GPU 32..63.
pub(crate) const fn gpu(n: u32) -> [u32; 2] { [1 + n / 32, n % 32] }

/// The SoC's addresses on the VideoCore bus, as the vendor tree writes them.
const fn bus(phys: u64) -> u32 { (phys as u32 - crate::machine::PERIPH as u32) + 0x7e00_0000 }

pub fn tree(b: &BootInfo) -> Vec<u8> {
    use crate::machine as m;
    let (armctrl, local, clk_core, clk_uart, clk_emmc, clk_tsens, mbox, thermal, aux, pm) = (1u32, 2u32, 3u32, 4u32, 5u32, 6u32, 7u32, 8u32, 9u32, 10u32);
    let mut f = Fdt::default();
    f.begin("");
    f.strs("compatible", &[b.model.compatible(), "brcm,bcm2837"]);
    f.str("model", &format!("{} (sbc-emu reduced DT)", b.model.name()));
    f.cells("#address-cells", &[1]);
    f.cells("#size-cells", &[1]);
    f.cells("interrupt-parent", &[armctrl]);

    let node = |p: u64| format!("{:x}", bus(p));
    f.begin("aliases");
    // the 3B+'s console is the mini UART: the PL011 belongs to Bluetooth (not modelled yet)
    f.str("serial0", &format!("/soc/serial@{}", node(m::AUX + 0x40)));
    f.str("serial1", &format!("/soc/serial@{}", node(m::UART0)));
    f.str("uart0", &format!("/soc/serial@{}", node(m::UART0)));
    f.str("uart1", &format!("/soc/serial@{}", node(m::AUX + 0x40)));
    f.str("mmc0", &format!("/soc/mmc@{}", node(m::SDHCI)));
    if b.model == crate::Model::B3Plus {
        f.str("ethernet0", &format!("/soc/usb@{}/usb-port@1/usb-port@1/ethernet@1", node(m::USB)));
    }
    f.end();

    f.begin("chosen");
    f.str("bootargs", b.bootargs);
    f.str("stdout-path", "serial0:115200n8");
    if let Some((s, e)) = b.initrd {
        f.cells("linux,initrd-start", &[s as u32]);
        f.cells("linux,initrd-end", &[e as u32]);
    }
    f.end();

    // what the firmware tells the kernel about the board (/proc/cpuinfo's Revision and Serial)
    f.begin("system");
    f.cells("linux,revision", &[b.model.revision()]);
    f.cells("linux,serial", &[(bcm2711::mailbox::SERIAL >> 32) as u32, bcm2711::mailbox::SERIAL as u32]);
    f.end();

    f.begin("memory@0");
    f.str("device_type", "memory");
    f.cells("reg", &[0, b.ram as u32]);
    f.end();

    f.begin("cpus");
    f.cells("#address-cells", &[1]);
    f.cells("#size-cells", &[0]);
    for k in 0..b.cpus.max(1) {
        f.begin(&format!("cpu@{k}"));
        f.str("device_type", "cpu");
        f.str("compatible", "arm,cortex-a53");
        f.cells("reg", &[k]);
        f.str("enable-method", "psci");
        f.end();
    }
    f.end();

    f.begin("psci");
    f.strs("compatible", &["arm,psci-1.0", "arm,psci-0.2"]);
    f.str("method", "smc");
    f.end();

    // the generic timer's four outputs are inputs 0..3 of the per-core controller: secure
    // physical, non-secure physical, virtual, hypervisor (in the binding's order)
    f.begin("timer");
    f.str("compatible", "arm,armv8-timer");
    f.cells("interrupt-parent", &[local]);
    f.cells("interrupts", &[0, LEVEL_HIGH, 1, LEVEL_HIGH, 3, LEVEL_HIGH, 2, LEVEL_HIGH]);
    f.empty("always-on");
    f.end();

    for (name, hz, ph) in [("clk-core", 250_000_000, clk_core), ("clk-uart", 48_000_000, clk_uart), ("clk-emmc", 250_000_000, clk_emmc), ("clk-tsens", 1_920_000, clk_tsens)] {
        f.begin(name);
        f.str("compatible", "fixed-clock");
        f.cells("#clock-cells", &[0]);
        f.cells("clock-frequency", &[hz]);
        f.str("clock-output-names", &name[4..]);
        f.cells("phandle", &[ph]);
        f.end();
    }

    f.begin("thermal-zones");
    f.begin("cpu-thermal");
    f.cells("polling-delay-passive", &[0]);
    f.cells("polling-delay", &[1000]);
    f.cells("thermal-sensors", &[thermal]);
    f.cells("coefficients", &[crate::periph::THERMAL_SLOPE as u32, crate::periph::THERMAL_OFFSET as u32]);
    f.begin("trips");
    f.begin("cpu-crit");
    f.cells("temperature", &[110_000]);
    f.cells("hysteresis", &[0]);
    f.str("type", "critical");
    f.end();
    f.end();
    f.end();
    f.end();

    f.begin("soc");
    f.str("compatible", "simple-bus");
    f.cells("#address-cells", &[1]);
    f.cells("#size-cells", &[1]);
    f.cells("ranges", &[0x7e00_0000, m::PERIPH as u32, 0x0100_0000, m::LOCAL as u32, m::LOCAL as u32, 0x1000]);
    f.cells("dma-ranges", &[0xc000_0000, 0, 0x3f00_0000, 0x7e00_0000, m::PERIPH as u32, 0x0100_0000]);

    f.begin(&format!("interrupt-controller@{}", node(m::ARMCTRL)));
    f.str("compatible", "brcm,bcm2836-armctrl-ic");
    f.cells("reg", &[bus(m::ARMCTRL), 0x200]);
    f.empty("interrupt-controller");
    f.cells("#interrupt-cells", &[2]);
    // chained into the per-core controller's GPU input
    f.cells("interrupt-parent", &[local]);
    f.cells("interrupts", &[crate::intc::GPU as u32, LEVEL_HIGH]);
    f.cells("phandle", &[armctrl]);
    f.end();

    f.begin(&format!("interrupt-controller@{:x}", m::LOCAL));
    f.str("compatible", "brcm,bcm2836-l1-intc");
    f.cells("reg", &[m::LOCAL as u32, 0x100]);
    f.empty("interrupt-controller");
    f.cells("#interrupt-cells", &[2]);
    f.cells("interrupt-parent", &[local]);
    f.cells("phandle", &[local]);
    f.end();

    f.begin(&format!("mailbox@{}", node(m::MBOX)));
    f.str("compatible", "brcm,bcm2835-mbox");
    f.cells("reg", &[bus(m::MBOX), 0x40]);
    f.cells("interrupts", &[0, m::ARM_MAILBOX as u32]);
    f.cells("#mbox-cells", &[0]);
    f.cells("phandle", &[mbox]);
    f.end();

    f.begin("firmware");
    f.strs("compatible", &["raspberrypi,bcm2835-firmware", "simple-mfd"]);
    f.cells("mboxes", &[mbox]);
    f.end();

    // power management: the watchdog (reboot, poweroff) and the power domains behind the ASB
    f.begin(&format!("watchdog@{}", node(m::PM)));
    f.strs("compatible", &["brcm,bcm2835-pm", "brcm,bcm2835-pm-wdt"]);
    f.cells("reg", &[bus(m::PM), 0x114, bus(m::ASB), 0x24]);
    f.strs("reg-names", &["pm", "asb"]);
    f.cells("#power-domain-cells", &[1]);
    f.cells("#reset-cells", &[1]);
    f.cells("phandle", &[pm]);
    f.end();

    f.begin(&format!("rng@{}", node(m::RNG)));
    f.str("compatible", "brcm,bcm2835-rng");
    f.cells("reg", &[bus(m::RNG), 0x10]);
    f.cells("interrupts", &gpu(m::GPU_RNG as u32));
    f.end();

    f.begin(&format!("serial@{}", node(m::UART0)));
    f.strs("compatible", &["arm,pl011", "arm,primecell"]);
    f.cells("reg", &[bus(m::UART0), 0x200]);
    f.cells("interrupts", &gpu(m::GPU_UART0 as u32));
    f.cells("clocks", &[clk_uart, clk_uart]);
    f.strs("clock-names", &["uartclk", "apb_pclk"]);
    f.cells("arm,primecell-periphid", &[0x0024_1011]);
    f.end();

    f.begin(&format!("thermal@{}", node(m::THERMAL)));
    f.str("compatible", "brcm,bcm2837-thermal");
    f.cells("reg", &[bus(m::THERMAL), 0x8]);
    f.cells("clocks", &[clk_tsens]);
    f.cells("#thermal-sensor-cells", &[0]);
    f.cells("phandle", &[thermal]);
    f.end();

    f.begin(&format!("aux@{}", node(m::AUX)));
    f.str("compatible", "brcm,bcm2835-aux");
    f.cells("#clock-cells", &[1]);
    f.cells("reg", &[bus(m::AUX), 0x8]);
    f.cells("clocks", &[clk_core]);
    f.cells("phandle", &[aux]);
    f.end();

    f.begin(&format!("serial@{}", node(m::AUX + 0x40)));
    f.str("compatible", "brcm,bcm2835-aux-uart");
    f.cells("reg", &[bus(m::AUX + 0x40), 0x40]);
    f.cells("interrupts", &gpu(m::GPU_AUX as u32));
    f.cells("clocks", &[aux, 0]);
    f.str("status", "okay");
    f.end();

    // the SD card on the Arasan SDHCI (on a real Pi 3 this host has the Wi-Fi chip and the
    // card is on the `sdhost` controller; until Wi-Fi is modelled the card sits here)
    f.begin(&format!("mmc@{}", node(m::SDHCI)));
    f.str("compatible", "brcm,bcm2835-sdhci");
    f.cells("reg", &[bus(m::SDHCI), 0x100]);
    f.cells("interrupts", &gpu(m::GPU_SDHCI as u32));
    f.cells("clocks", &[clk_emmc]);
    f.cells("bus-width", &[4]);
    f.empty("non-removable");
    f.str("status", "okay");
    f.end();

    // the DWC2 OTG core, host only (the 3B+'s USB ports are behind the LAN7515's hubs), for
    // the upstream `dwc2` driver: the vendor tree's `brcm,bcm2708-usb` binds `dwc_otg`, which
    // needs the FIQ. The nodes below describe the soldered-on USB devices by port, as the vendor
    // tree does, so the Ethernet finds its MAC address.
    f.begin(&format!("usb@{}", node(m::USB)));
    f.str("compatible", "brcm,bcm2835-usb");
    f.cells("reg", &[bus(m::USB), 0x10000]);
    f.cells("interrupts", &gpu(m::GPU_USB as u32));
    f.str("dr_mode", "host");
    f.cells("#address-cells", &[1]);
    f.cells("#size-cells", &[0]);
    f.str("status", "okay");
    if b.model == crate::Model::B3Plus {
        for _ in 0..2 {
            f.begin("usb-port@1");
            f.str("compatible", "usb424,2514");
            f.cells("reg", &[1]);
            f.cells("#address-cells", &[1]);
            f.cells("#size-cells", &[0]);
        }
        f.begin("ethernet@1");
        f.str("compatible", "usb424,7800");
        f.cells("reg", &[1]);
        f.prop("local-mac-address", &m::ETH_MAC);
        f.begin("mdio");
        f.cells("#address-cells", &[1]);
        f.cells("#size-cells", &[0]);
        f.begin("ethernet-phy@1");
        f.cells("reg", &[1]);
        f.end();
        f.end();
        f.end();
        f.end();
        f.end();
    }
    f.end();

    f.end(); // soc

    if let Some((base, w, h, stride)) = b.fb {
        f.begin(&format!("framebuffer@{base:x}"));
        f.str("compatible", "simple-framebuffer");
        f.cells("reg", &[base as u32, stride * h]);
        f.cells("width", &[w]);
        f.cells("height", &[h]);
        f.cells("stride", &[stride]);
        f.str("format", "a8r8g8b8");
        f.str("status", "okay");
        f.end();
    }

    f.end();
    f.finish()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tree_parses_and_names_the_board() {
        let blob = tree(&BootInfo { model: crate::Model::B3Plus, ram: 948 << 20, bootargs: "console=ttyS0", initrd: Some((0x400_0000, 0x410_0000)), fb: None, cpus: 4 });
        let root = emu_core::fdt::Node::parse(&blob).unwrap();
        let model = root.prop("model").unwrap();
        assert!(model.starts_with(b"Raspberry Pi 3 Model B Plus Rev 1.3"));
        assert_eq!(gpu(29), [1, 29]);
        assert_eq!(gpu(62), [2, 30]);
        assert_eq!(bus(crate::machine::AUX), 0x7e21_5000);
        assert_eq!(bus(crate::machine::USB), 0x7e98_0000);
        assert!(blob.windows(b"usb424,7800".len()).any(|w| w == b"usb424,7800"));
        assert!(blob.windows(6).any(|w| w == crate::machine::ETH_MAC));
    }
}
