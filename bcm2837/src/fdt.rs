//! The reduced BCM2837 device tree the emulated firmware hands to Linux.
//!
//! Like the Pi 4's (`bcm2711::fdt`), it lists only what is modelled, but its shape follows the
//! vendor `bcm2710-rpi-3-b-plus.dtb`: one address and one size cell, devices under `/soc` at
//! their VideoCore bus addresses (0x7e..., translated to 0x3f... by `ranges`), DMA addresses
//! through the 0xc000_0000 alias (`dma-ranges`), the two interrupt controllers (no GIC), the
//! generic timer on the per-core controller. Cores start through PSCI (the vendor tree uses a
//! spin table; with our own tree either works), clocks are fixed instead of the CPRMAN's.
//! Storage and wireless as in the vendor tree: the SD card on `sdhost` (mmc0), the Wi-Fi chip on
//! the Arasan (mmc1), Bluetooth as a serdev child of UART0, both powered through the firmware's
//! GPIO expander, pins muxed by the GPIO controller's groups.

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
    /// the vc4 KMS pipeline (`dtoverlay=vc4-kms-v3d`): HVS, pixel valves, HDMI and its DDC bus
    pub kms: bool,
    /// the VideoCore IV 3D block (with `kms`)
    pub v3d: bool,
    /// the outputs of the KMS pipeline: HDMI (unless `nohdmi`), composite (the VEC)
    pub outputs: bcm2711::boot::KmsOutputs,
    /// the Bluetooth controller on UART0 (else UART0 is a plain serial port)
    pub bluetooth: bool,
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
    let (fwclk, power, gpio, dma, ddc, expgpio, firmware) = (11u32, 12u32, 13u32, 14u32, 15u32, 16u32, 17u32);
    let (pins_uart0, pins_bt, pins_sdhost, pins_sdio) = (18u32, 19u32, 20u32, 21u32);
    let wireless = b.model.wireless();
    let bluetooth = wireless && b.bluetooth;
    let mut f = Fdt::default();
    f.begin("");
    f.strs("compatible", &[b.model.compatible(), "brcm,bcm2837"]);
    f.str("model", &format!("{} (sbc-emu reduced DT)", b.model.name()));
    f.cells("#address-cells", &[1]);
    f.cells("#size-cells", &[1]);
    f.cells("interrupt-parent", &[armctrl]);

    let node = |p: u64| format!("{:x}", bus(p));
    f.begin("aliases");
    // the console is the mini UART, the PL011 belongs to Bluetooth: the vendor tree's aliases
    // (Linux numbers the PL011 after its `serial1` alias: ttyAMA1, a serdev port without a tty
    // while Bluetooth holds it)
    f.str("serial0", &format!("/soc/serial@{}", node(m::AUX + 0x40)));
    f.str("serial1", &format!("/soc/serial@{}", node(m::UART0)));
    f.str("uart0", &format!("/soc/serial@{}", node(m::UART0)));
    f.str("uart1", &format!("/soc/serial@{}", node(m::AUX + 0x40)));
    f.str("gpio", &format!("/soc/gpio@{}", node(m::GPIO)));
    f.str("dma", &format!("/soc/dma-controller@{}", node(m::DMA)));
    if b.kms && b.outputs.hdmi { f.str("i2c2", &format!("/soc/i2c@{}", node(m::DDC))); }
    // the card is mmc0 (mmcblk0) on sdhost, the Wi-Fi's SDIO host mmc1
    f.str("mmc0", &format!("/soc/mmc@{}", node(m::SDHOST)));
    f.str("sdhost", &format!("/soc/mmc@{}", node(m::SDHOST)));
    f.str("mmc1", &format!("/soc/mmc@{}", node(m::SDHCI)));
    if bluetooth { f.str("bluetooth", &format!("/soc/serial@{}/bluetooth", node(m::UART0))); }
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

    if b.kms {
        // contiguous memory for scanout buffers, as dtoverlay=vc4-kms-v3d sets it up (cma-256)
        f.begin("reserved-memory");
        f.cells("#address-cells", &[1]);
        f.cells("#size-cells", &[1]);
        f.empty("ranges");
        f.begin("linux,cma");
        f.str("compatible", "shared-dma-pool");
        // (CMA areas are whole 4 MiB page blocks)
        f.cells("size", &[(256u32 << 20).min((b.ram / 4) as u32 & !0x3f_ffff)]);
        f.empty("reusable");
        f.empty("linux,cma-default");
        f.end();
        f.end();
    }

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

    // VCHIQ, the message queue to the VideoCore firmware's services (MMAL, audio, shared
    // memory): its doorbells and the ARM's doorbell 0 interrupt, the BCM2836-family
    // compatible (64-byte cache lines for bulk transfers) as the vendor tree has it
    f.begin(&format!("mailbox@{}", node(m::VCHIQ)));
    f.strs("compatible", &["brcm,bcm2836-vchiq", "brcm,bcm2835-vchiq"]);
    f.cells("reg", &[bus(m::VCHIQ), 0x3c]);
    f.cells("interrupts", &[0, m::ARM_DOORBELL0 as u32]);
    f.end();

    f.begin("firmware");
    f.strs("compatible", &["raspberrypi,bcm2835-firmware", "simple-mfd"]);
    f.cells("mboxes", &[mbox]);
    f.cells("phandle", &[firmware]);
    // the clocks the firmware manages (the HDMI pixel and state machine clocks, V3D, core)
    f.begin("clocks");
    f.str("compatible", "raspberrypi,firmware-clocks");
    f.cells("#clock-cells", &[1]);
    f.cells("phandle", &[fwclk]);
    f.end();
    // the firmware's GPIO expander (lines 128..135 of the mailbox's GPIO tags)
    f.begin("expgpio");
    f.str("compatible", "raspberrypi,firmware-gpio");
    f.empty("gpio-controller");
    f.cells("#gpio-cells", &[2]);
    if b.model == crate::Model::B3 {
        f.strs("gpio-line-names", &["BT_ON", "WL_ON", "STATUS_LED", "LAN_RUN", "HDMI_HPD_N", "CAM_GPIO0", "CAM_GPIO1", "PWR_LOW_N"]);
    } else {
        f.strs("gpio-line-names", &["BT_ON", "WL_ON", "PWR_LED_R", "LAN_RUN", "NC", "CAM_GPIO0", "CAM_GPIO1", "NC"]);
    }
    f.str("status", "okay");
    f.cells("phandle", &[expgpio]);
    f.end();
    f.end();

    // the power domains the firmware switches (HDMI, V3D, ...)
    f.begin("power");
    f.str("compatible", "raspberrypi,bcm2835-power");
    f.cells("firmware", &[firmware]);
    f.cells("#power-domain-cells", &[1]);
    f.cells("phandle", &[power]);
    f.end();

    // the GPIO controller and pin multiplexer (`pinctrl-bcm2835`) with the vendor tree's line
    // names, HDMI's hot-plug input (GPIO 28) and the pin groups the SD hosts and UART0 select
    f.begin(&format!("gpio@{}", node(m::GPIO)));
    f.str("compatible", "brcm,bcm2835-gpio");
    f.cells("reg", &[bus(m::GPIO), 0xb4]);
    f.cells("interrupts", &[gpu(m::GPU_GPIO0 as u32), gpu(m::GPU_GPIO0 as u32 + 1)].concat());
    f.empty("gpio-controller");
    f.cells("#gpio-cells", &[2]);
    f.empty("interrupt-controller");
    f.cells("#interrupt-cells", &[2]);
    f.cells("gpio-ranges", &[gpio, 0, 0, 54]);
    let mut names: Vec<String> = (0..54).map(|n| format!("GPIO{n}")).collect();
    for (k, n) in [(0, "ID_SDA"), (1, "ID_SCL"), (28, "HDMI_HPD_N"), (29, "STATUS_LED_G"), (30, "CTS0"), (31, "RTS0"), (32, "TXD0"), (33, "RXD0"),
                   (34, "SD1_CLK"), (35, "SD1_CMD"), (36, "SD1_DATA0"), (37, "SD1_DATA1"), (38, "SD1_DATA2"), (39, "SD1_DATA3"), (40, "PWM0_OUT"),
                   (41, "PWM1_OUT"), (42, "ETH_CLK"), (43, "WIFI_CLK"), (44, "SDA0"), (45, "SCL0"), (46, "SMPS_SCL"), (47, "SMPS_SDA"),
                   (48, "SD_CLK_R"), (49, "SD_CMD_R"), (50, "SD_DATA0_R"), (51, "SD_DATA1_R"), (52, "SD_DATA2_R"), (53, "SD_DATA3_R")] {
        names[k] = n.into();
    }
    let refs: Vec<&str> = names.iter().map(String::as_str).collect();
    f.strs("gpio-line-names", &refs);
    f.cells("phandle", &[gpio]);
    // function numbers: 4 = ALT0, 7 = ALT3; pulls: 0 none, 2 up
    for (name, pins, func, pulls, ph) in [
        ("uart0_pins", &[32u32, 33][..], 7, &[0u32, 2][..], pins_uart0),
        ("bt_pins", &[43][..], 4, &[0][..], pins_bt),
        ("sdhost_gpio48", &[48, 49, 50, 51, 52, 53][..], 4, &[][..], pins_sdhost),
        ("sdio_pins", &[34, 35, 36, 37, 38, 39][..], 7, &[0, 2, 2, 2, 2, 2][..], pins_sdio),
    ] {
        f.begin(name);
        f.cells("brcm,pins", pins);
        f.cells("brcm,function", &[func]);
        if !pulls.is_empty() { f.cells("brcm,pull", pulls); }
        f.cells("phandle", &[ph]);
        f.end();
    }
    f.end();

    // the DMA controller: channels 0..10 with their own interrupts, 11..14 sharing one; the
    // channels the firmware leaves to Linux
    f.begin(&format!("dma-controller@{}", node(m::DMA)));
    f.str("compatible", "brcm,bcm2835-dma");
    f.cells("reg", &[bus(m::DMA), 0xf00]);
    let irqs: Vec<u32> = (0..16u32).flat_map(|n| gpu(m::GPU_DMA0 as u32 + if n < 11 { n } else if n < 15 { 11 } else { 12 })).collect();
    f.cells("interrupts", &irqs);
    let mut names: Vec<String> = (0..15).map(|n| format!("dma{n}")).collect();
    names.push("dma-shared-all".into());
    let refs: Vec<&str> = names.iter().map(String::as_str).collect();
    f.strs("interrupt-names", &refs);
    f.cells("#dma-cells", &[1]);
    f.cells("brcm,dma-channel-mask", &[0x7f35]);
    f.cells("phandle", &[dma]);
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
    f.str("pinctrl-names", "default");
    f.cells("pinctrl-0", &[pins_uart0, pins_bt]);
    f.str("status", "okay");
    if bluetooth {
        // the CYW43455's Bluetooth core (`hci_bcm` over serdev), powered by BT_ON
        f.begin("bluetooth");
        f.str("compatible", "brcm,bcm43438-bt");
        f.cells("max-speed", &[3_000_000]);
        f.cells("shutdown-gpios", &[expgpio, m::EXP_BT_ON as u32, 0]);
        f.str("status", "okay");
        f.end();
    }
    f.end();

    // the SD card's host: PIO for single blocks, DMA for more
    f.begin(&format!("mmc@{}", node(m::SDHOST)));
    f.str("compatible", "brcm,bcm2835-sdhost");
    f.cells("reg", &[bus(m::SDHOST), 0x100]);
    f.cells("interrupts", &gpu(m::GPU_SDHOST as u32));
    f.cells("clocks", &[clk_core]);
    f.cells("bus-width", &[4]);
    f.cells("brcm,overclock-50", &[0]);
    f.cells("brcm,pio-limit", &[1]);
    // DMA on DREQ 13 (with the vendor tree's flags) for transfers of more than one block
    f.cells("dmas", &[dma, 0x2000_0000 | crate::sdhost::DREQ]);
    f.str("dma-names", "rx-tx");
    f.cells("firmware", &[firmware]);
    f.str("pinctrl-names", "default");
    f.cells("pinctrl-0", &[pins_sdhost]);
    f.str("status", "okay");
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

    // the Arasan SDHCI ("mmcnr") with the CYW43455's Wi-Fi on SDIO function 1/2 (powered by
    // WL_ON); off on the boards whose wireless chip is not modelled
    f.begin(&format!("mmc@{}", node(m::SDHCI)));
    f.str("compatible", "brcm,bcm2835-sdhci");
    f.cells("reg", &[bus(m::SDHCI), 0x100]);
    f.cells("interrupts", &gpu(m::GPU_SDHCI as u32));
    f.cells("clocks", &[clk_emmc]);
    f.cells("bus-width", &[4]);
    f.empty("non-removable");
    f.str("pinctrl-names", "default");
    f.cells("pinctrl-0", &[pins_sdio]);
    f.cells("#address-cells", &[1]);
    f.cells("#size-cells", &[0]);
    f.str("status", if wireless { "okay" } else { "disabled" });
    if wireless {
        f.begin("wifi@1");
        f.cells("reg", &[1]);
        f.str("compatible", "brcm,bcm4329-fmac");
        f.end();
    }
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

    if b.kms { display(&mut f, b, Phandles { fwclk, power, gpio, dma, ddc, expgpio, clk_core }); }

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

struct Phandles { fwclk: u32, power: u32, gpio: u32, dma: u32, ddc: u32, expgpio: u32, clk_core: u32 }

/// The display pipeline as `dtoverlay=vc4-kms-v3d` enables it on the vendor tree: the HVS,
/// pixel valves 0..2, the transposer, HDMI (fed by pixel valve 2) with its DDC bus (BSC2) unless
/// `nohdmi`, the composite encoder (VEC, pixel valve 2's other output) with `composite`, and
/// the V3D when it is modelled.
fn display(f: &mut Fdt, b: &BootInfo, p: Phandles) {
    use crate::machine as m;
    let node = |a: u64| format!("{:x}", bus(a));
    f.begin("gpu");
    f.str("compatible", "brcm,bcm2835-vc4");
    f.end();
    f.begin(&format!("hvs@{}", node(m::HVS)));
    f.str("compatible", "brcm,bcm2835-hvs");
    f.cells("reg", &[bus(m::HVS), 0x6000]);
    f.cells("interrupts", &gpu(m::GPU_HVS as u32));
    f.end();
    for (k, base, irq) in [(0, m::PV0, m::GPU_PV0), (1, m::PV1, m::GPU_PV1), (2, m::PV2, m::GPU_PV2)] {
        f.begin(&format!("pixelvalve@{}", node(base)));
        f.str("compatible", &format!("brcm,bcm2835-pixelvalve{k}"));
        f.cells("reg", &[bus(base), 0x100]);
        f.cells("interrupts", &gpu(irq as u32));
        f.end();
    }
    f.begin(&format!("txp@{}", node(m::TXP)));
    f.str("compatible", "brcm,bcm2835-txp");
    f.cells("reg", &[bus(m::TXP), 0x20]);
    f.cells("interrupts", &gpu(m::GPU_TXP as u32));
    f.end();
    if b.outputs.composite {
        // the VEC: the firmware's VEC clock, its power domain (RPI_POWER_DOMAIN_VEC)
        f.begin(&format!("vec@{}", node(m::VEC)));
        f.str("compatible", "brcm,bcm2835-vec");
        f.cells("reg", &[bus(m::VEC), bcm2711::vc4::VEC_SIZE as u32]);
        f.cells("clocks", &[p.fwclk, 15]);
        f.cells("interrupts", &gpu(m::GPU_VEC as u32));
        f.cells("power-domains", &[p.power, 7]);
        f.str("status", "okay");
        f.end();
    }
    if b.outputs.hdmi {
        f.begin(&format!("i2c@{}", node(m::DDC)));
        f.str("compatible", "brcm,bcm2835-i2c");
        f.cells("reg", &[bus(m::DDC), 0x1000]);
        f.cells("interrupts", &gpu(m::GPU_I2C as u32));
        f.cells("clocks", &[p.clk_core]);
        f.cells("#address-cells", &[1]);
        f.cells("#size-cells", &[0]);
        f.cells("clock-frequency", &[100_000]);
        f.str("status", "okay");
        f.cells("phandle", &[p.ddc]);
        f.end();
        f.begin(&format!("hdmi@{}", node(m::HDMI)));
        f.str("compatible", "brcm,bcm2835-hdmi");
        f.cells("reg", &[bus(m::HDMI), bcm2711::vc4::HDMI_2835_SIZE as u32, bus(m::HD), 0x100]);
        f.strs("reg-names", &["hdmi", "hd"]);
        f.cells("interrupts", &[gpu(m::GPU_HDMI0 as u32), gpu(m::GPU_HDMI1 as u32)].concat());
        f.cells("ddc", &[p.ddc]);
        // the firmware's pixel clock and HDMI state machine (M2MC) clock
        f.cells("clocks", &[p.fwclk, 9, p.fwclk, 13]);
        f.strs("clock-names", &["pixel", "hdmi"]);
        // audio through DREQ 17, with the vendor tree's flags (wait for write responses, burst)
        f.cells("dmas", &[p.dma, 0x0900_0000 | bcm2711::vc4::DREQ_HDMI_BCM2835]);
        f.str("dma-names", "audio-rx");
        f.cells("power-domains", &[p.power, 5]);
        // hot-plug: GPIO 28 (the 3B: the firmware's GPIO expander, line 4), active low
        if b.model.hpd_on_expander() { f.cells("hpd-gpios", &[p.expgpio, 4, 1]); } else { f.cells("hpd-gpios", &[p.gpio, 28, 1]); }
        f.str("status", "okay");
        f.end();
    }
    if b.v3d {
        // the VideoCore IV 3D block: one register page, GPU interrupt 10, the V3D power domain
        // and the firmware's V3D clock
        f.begin(&format!("v3d@{}", node(m::V3D)));
        f.str("compatible", "brcm,vc4-v3d");
        f.cells("reg", &[bus(m::V3D), 0x1000]);
        f.cells("interrupts", &gpu(m::GPU_V3D as u32));
        f.cells("power-domains", &[p.power, 10]);
        f.cells("clocks", &[p.fwclk, 5]);
        f.end();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tree_parses_and_names_the_board() {
        let blob = tree(&BootInfo { model: crate::Model::B3Plus, ram: 948 << 20, bootargs: "console=ttyS0", initrd: Some((0x400_0000, 0x410_0000)), fb: None, cpus: 4, kms: true, v3d: true,
                                   outputs: bcm2711::boot::KmsOutputs { hdmi: true, composite: true }, bluetooth: true });
        let root = emu_core::fdt::Node::parse(&blob).unwrap();
        let model = root.prop("model").unwrap();
        assert!(model.starts_with(b"Raspberry Pi 3 Model B Plus Rev 1.3"));
        assert_eq!(gpu(29), [1, 29]);
        assert_eq!(gpu(62), [2, 30]);
        assert_eq!(bus(crate::machine::AUX), 0x7e21_5000);
        assert_eq!(bus(crate::machine::USB), 0x7e98_0000);
        assert!(blob.windows(b"usb424,7800".len()).any(|w| w == b"usb424,7800"));
        assert!(blob.windows(6).any(|w| w == crate::machine::ETH_MAC));
        assert!(blob.windows(b"brcm,bcm2835-vec".len()).any(|w| w == b"brcm,bcm2835-vec"));
        assert!(blob.windows(b"brcm,bcm2836-vchiq".len()).any(|w| w == b"brcm,bcm2836-vchiq"));
        let child = |n: &'_ emu_core::fdt::Node, name: &str| n.children.iter().find(|c| c.name == name).cloned();
        let soc = child(&root, "soc").unwrap();
        let sdhost = child(&soc, "mmc@7e202000").unwrap();
        assert_eq!(sdhost.prop("compatible").unwrap(), b"brcm,bcm2835-sdhost\0");
        assert!(child(&child(&soc, "serial@7e201000").unwrap(), "bluetooth").is_some());
        assert!(child(&child(&soc, "mmc@7e300000").unwrap(), "wifi@1").is_some());
    }
}
