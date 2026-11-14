//! Flattened device tree writer, and the reduced Pi 4B tree this machine describes.
//!
//! The tree is a *reduced* BCM2711 description, not the vendor `bcm2711-rpi-4-b.dtb`: it lists
//! only the devices that are modelled (up to four Cortex-A72s, RAM, GIC-400, the generic timer, PL011
//! UART0 with a fixed 48 MHz clock instead of the firmware mailbox clock). Compatible strings
//! and addresses follow the vendor tree.

#[derive(Default)]
pub struct Fdt { st: Vec<u8>, strings: Vec<u8> }

const BEGIN_NODE: u32 = 1;
const END_NODE: u32 = 2;
const PROP: u32 = 3;
const END: u32 = 9;

impl Fdt {
    fn u32(&mut self, v: u32) { self.st.extend_from_slice(&v.to_be_bytes()); }
    fn pad(&mut self) { while self.st.len() % 4 != 0 { self.st.push(0); } }
    pub fn begin(&mut self, name: &str) { self.u32(BEGIN_NODE); self.st.extend_from_slice(name.as_bytes()); self.st.push(0); self.pad(); }
    pub fn end(&mut self) { self.u32(END_NODE); }
    pub fn prop(&mut self, name: &str, val: &[u8]) {
        let off = match find(&self.strings, name) {
            Some(o) => o,
            None => { let o = self.strings.len(); self.strings.extend_from_slice(name.as_bytes()); self.strings.push(0); o }
        };
        self.u32(PROP);
        self.u32(val.len() as u32);
        self.u32(off as u32);
        self.st.extend_from_slice(val);
        self.pad();
    }
    pub fn cells(&mut self, name: &str, v: &[u32]) {
        let b: Vec<u8> = v.iter().flat_map(|c| c.to_be_bytes()).collect();
        self.prop(name, &b);
    }
    pub fn str(&mut self, name: &str, v: &str) { self.strs(name, &[v]); }
    pub fn strs(&mut self, name: &str, v: &[&str]) {
        let mut b = Vec::new();
        for s in v { b.extend_from_slice(s.as_bytes()); b.push(0); }
        self.prop(name, &b);
    }
    pub fn empty(&mut self, name: &str) { self.prop(name, &[]); }

    pub fn finish(mut self) -> Vec<u8> {
        self.u32(END);
        let rsv_off = 40u32;
        let st_off = rsv_off + 16;
        let str_off = st_off + self.st.len() as u32;
        let total = str_off + self.strings.len() as u32;
        let mut out = Vec::with_capacity(total as usize);
        for v in [0xd00d_feedu32, total, st_off, str_off, rsv_off, 17, 16, 0, self.strings.len() as u32, self.st.len() as u32] {
            out.extend_from_slice(&v.to_be_bytes());
        }
        out.extend_from_slice(&[0; 16]);
        out.extend_from_slice(&self.st);
        out.extend_from_slice(&self.strings);
        out
    }
}

fn find(strings: &[u8], name: &str) -> Option<usize> {
    let mut i = 0;
    while i < strings.len() {
        let end = i + strings[i..].iter().position(|&c| c == 0)?;
        if &strings[i..end] == name.as_bytes() { return Some(i); }
        i = end + 1;
    }
    None
}

pub struct BootInfo<'a> {
    /// RAM given to Linux (below the GPU carve-out)
    pub ram: u64,
    pub bootargs: &'a str,
    pub initrd: Option<(u64, u64)>,
    /// firmware-allocated framebuffer: (base, width, height, stride)
    pub fb: Option<(u64, u32, u32, u32)>,
    /// cores (started through PSCI CPU_ON)
    pub cpus: u32,
}

const GIC_SPI: u32 = 0;
const GIC_PPI: u32 = 1;
const LEVEL_HIGH: u32 = 4;
const PPI_LEVEL_LOW_ALL: u32 = 0xf08;

pub fn pi4b(b: &BootInfo) -> Vec<u8> {
    let (gic, clk48, mbox, clk100, phy1, gpio, clkvpu, vref) = (1u32, 2u32, 3u32, 4u32, 5u32, 6u32, 7u32, 8u32);
    let (thermal, aux) = (9u32, 10u32);
    let mut f = Fdt::default();
    f.begin("");
    f.strs("compatible", &["raspberrypi,4-model-b", "brcm,bcm2711"]);
    f.str("model", "Raspberry Pi 4 Model B (sbc-emu reduced DT)");
    f.cells("#address-cells", &[2]);
    f.cells("#size-cells", &[1]);
    f.cells("interrupt-parent", &[gic]);

    f.begin("aliases");
    f.str("serial0", "/serial@fe201000");
    f.str("serial1", "/soc/serial@fe215040");
    f.str("mmc0", "/soc/mmc@fe340000");
    f.str("mmc1", "/soc/mmc@fe300000");
    f.str("i2c1", "/soc/i2c@fe804000");
    f.str("spi0", "/soc/spi@fe204000");
    f.end();

    f.begin("chosen");
    f.str("bootargs", b.bootargs);
    f.str("stdout-path", "serial0:115200n8");
    if let Some((s, e)) = b.initrd {
        f.cells("linux,initrd-start", &[(s >> 32) as u32, s as u32]);
        f.cells("linux,initrd-end", &[(e >> 32) as u32, e as u32]);
    }
    f.end();

    f.begin("memory@0");
    f.str("device_type", "memory");
    // RAM below the 0xfc00_0000 peripheral window, as the 32-bit-addressable part of a Pi 4
    f.cells("reg", &[0, 0, b.ram as u32]);
    f.end();

    f.begin("cpus");
    f.cells("#address-cells", &[1]);
    f.cells("#size-cells", &[0]);
    for k in 0..b.cpus.max(1) {
        f.begin(&format!("cpu@{k}"));
        f.str("device_type", "cpu");
        f.str("compatible", "arm,cortex-a72");
        f.cells("reg", &[k]);
        f.str("enable-method", "psci");
        f.end();
    }
    f.end();

    f.begin("psci");
    f.strs("compatible", &["arm,psci-1.0", "arm,psci-0.2"]);
    f.str("method", "smc");
    f.end();

    f.begin("clk-emmc2");
    f.str("compatible", "fixed-clock");
    f.cells("#clock-cells", &[0]);
    f.cells("clock-frequency", &[100_000_000]);
    f.str("clock-output-names", "emmc2");
    f.cells("phandle", &[clk100]);
    f.end();

    f.begin("clk-vpu");
    f.str("compatible", "fixed-clock");
    f.cells("#clock-cells", &[0]);
    f.cells("clock-frequency", &[500_000_000]);
    f.str("clock-output-names", "vpu");
    f.cells("phandle", &[clkvpu]);
    f.end();

    f.begin("regulator-3v3");
    f.str("compatible", "regulator-fixed");
    f.str("regulator-name", "3v3");
    f.cells("regulator-min-microvolt", &[3_300_000]);
    f.cells("regulator-max-microvolt", &[3_300_000]);
    f.empty("regulator-always-on");
    f.cells("phandle", &[vref]);
    f.end();

    f.begin("timer");
    f.str("compatible", "arm,armv8-timer");
    f.cells("interrupts", &[GIC_PPI, 13, PPI_LEVEL_LOW_ALL, GIC_PPI, 14, PPI_LEVEL_LOW_ALL, GIC_PPI, 11, PPI_LEVEL_LOW_ALL, GIC_PPI, 10, PPI_LEVEL_LOW_ALL]);
    f.empty("always-on");
    f.end();

    f.begin("interrupt-controller@ff841000");
    f.str("compatible", "arm,gic-400");
    f.empty("interrupt-controller");
    f.cells("#interrupt-cells", &[3]);
    f.cells("reg", &[0, 0xff84_1000, 0x1000, 0, 0xff84_2000, 0x2000]);
    f.cells("phandle", &[gic]);
    f.end();

    f.begin("clk48");
    f.str("compatible", "fixed-clock");
    f.cells("#clock-cells", &[0]);
    f.cells("clock-frequency", &[48_000_000]);
    f.str("clock-output-names", "uart0_pclk");
    f.cells("phandle", &[clk48]);
    f.end();

    f.begin("serial@fe201000");
    f.strs("compatible", &["arm,pl011", "arm,primecell"]);
    f.cells("reg", &[0, 0xfe20_1000, 0x200]);
    f.cells("interrupts", &[GIC_SPI, 121, LEVEL_HIGH]);
    f.cells("clocks", &[clk48, clk48]);
    f.strs("clock-names", &["uartclk", "apb_pclk"]);
    f.cells("arm,primecell-periphid", &[0x0024_1011]);
    f.end();

    f.begin("thermal-zones");
    f.begin("cpu-thermal");
    f.cells("polling-delay-passive", &[0]);
    f.cells("polling-delay", &[1000]);
    f.cells("thermal-sensors", &[thermal]);
    f.cells("coefficients", &[(-487i32) as u32, 410_040]);
    f.begin("trips");
    f.begin("cpu-crit");
    f.cells("temperature", &[90_000]);
    f.cells("hysteresis", &[0]);
    f.str("type", "critical");
    f.end();
    f.end();
    f.end();
    f.end();

    f.begin("soc");
    f.str("compatible", "simple-bus");
    f.cells("#address-cells", &[2]);
    f.cells("#size-cells", &[1]);
    f.empty("ranges");
    f.empty("dma-ranges");

    f.begin("gpio@fe200000");
    f.str("compatible", "brcm,bcm2711-gpio");
    f.cells("reg", &[0, 0xfe20_0000, 0xb4]);
    f.cells("interrupts", &[GIC_SPI, 113, LEVEL_HIGH, GIC_SPI, 114, LEVEL_HIGH, GIC_SPI, 115, LEVEL_HIGH, GIC_SPI, 116, LEVEL_HIGH]);
    f.empty("gpio-controller");
    f.cells("#gpio-cells", &[2]);
    f.empty("interrupt-controller");
    f.cells("#interrupt-cells", &[2]);
    // header names as on the Pi 4, with the emulated board's LED (GPIO17) and button (GPIO27)
    let mut names: Vec<String> = (0..58).map(|n| format!("GPIO{n}")).collect();
    names[17] = "GPIO17_LED".into();
    names[27] = "GPIO27_BUTTON".into();
    let refs: Vec<&str> = names.iter().map(String::as_str).collect();
    f.strs("gpio-line-names", &refs);
    f.cells("phandle", &[gpio]);
    f.end();

    f.begin("rng@fe104000");
    f.str("compatible", "brcm,bcm2711-rng200");
    f.cells("reg", &[0, 0xfe10_4000, 0x28]);
    f.end();

    f.begin("avs-monitor@fd5d2000");
    f.strs("compatible", &["brcm,bcm2711-avs-monitor", "syscon", "simple-mfd"]);
    f.cells("reg", &[0, 0xfd5d_2000, 0xf00]);
    f.begin("thermal");
    f.str("compatible", "brcm,bcm2711-thermal");
    f.cells("#thermal-sensor-cells", &[0]);
    f.cells("phandle", &[thermal]);
    f.end();
    f.end();

    // power management: watchdog, and power domains behind the AXI bridges (ASB)
    f.begin("watchdog@fe100000");
    f.strs("compatible", &["brcm,bcm2711-pm", "brcm,bcm2835-pm-wdt"]);
    f.cells("reg", &[0, 0xfe10_0000, 0x114, 0, 0xfe00_a000, 0x24, 0, 0xfec1_1000, 0x20]);
    f.strs("reg-names", &["pm", "asb", "rpivid_asb"]);
    f.cells("#power-domain-cells", &[1]);
    f.cells("#reset-cells", &[1]);
    f.end();

    f.begin("timer@fe003000");
    f.str("compatible", "brcm,bcm2835-system-timer");
    f.cells("reg", &[0, 0xfe00_3000, 0x1000]);
    f.cells("interrupts", &[GIC_SPI, 64, LEVEL_HIGH, GIC_SPI, 65, LEVEL_HIGH, GIC_SPI, 66, LEVEL_HIGH, GIC_SPI, 67, LEVEL_HIGH]);
    f.cells("clock-frequency", &[1_000_000]);
    f.end();

    f.begin("aux@fe215000");
    f.str("compatible", "brcm,bcm2835-aux");
    f.cells("#clock-cells", &[1]);
    f.cells("reg", &[0, 0xfe21_5000, 0x8]);
    f.cells("clocks", &[clkvpu]);
    f.cells("phandle", &[aux]);
    f.end();

    f.begin("serial@fe215040");
    f.str("compatible", "brcm,bcm2835-aux-uart");
    f.cells("reg", &[0, 0xfe21_5040, 0x40]);
    f.cells("interrupts", &[GIC_SPI, 93, LEVEL_HIGH]);
    f.cells("clocks", &[aux, 0]);
    f.str("status", "okay");
    f.end();

    f.begin("spi@fe204000");
    f.str("compatible", "brcm,bcm2835-spi");
    f.cells("reg", &[0, 0xfe20_4000, 0x200]);
    f.cells("interrupts", &[GIC_SPI, 118, LEVEL_HIGH]);
    f.cells("clocks", &[clkvpu]);
    f.cells("#address-cells", &[1]);
    f.cells("#size-cells", &[0]);
    f.cells("cs-gpios", &[gpio, 8, 1, gpio, 7, 1]);
    f.str("status", "okay");
    f.begin("adc@0");
    f.str("compatible", "microchip,mcp3008");
    f.cells("reg", &[0]);
    f.cells("spi-max-frequency", &[1_000_000]);
    f.cells("vref-supply", &[vref]);
    f.end();
    f.end();

    f.begin("i2c@fe804000");
    f.strs("compatible", &["brcm,bcm2711-i2c", "brcm,bcm2835-i2c"]);
    f.cells("reg", &[0, 0xfe80_4000, 0x1000]);
    f.cells("interrupts", &[GIC_SPI, 117, LEVEL_HIGH]);
    f.cells("clocks", &[clkvpu]);
    f.cells("#address-cells", &[1]);
    f.cells("#size-cells", &[0]);
    f.cells("clock-frequency", &[100_000]);
    f.str("status", "okay");
    f.begin("sensor@76");
    f.str("compatible", "bosch,bme280");
    f.cells("reg", &[0x76]);
    f.end();
    f.end();

    f.begin("mailbox@fe00b880");
    f.str("compatible", "brcm,bcm2835-mbox");
    f.cells("reg", &[0, 0xfe00_b880, 0x40]);
    f.cells("interrupts", &[GIC_SPI, 33, LEVEL_HIGH]);
    f.cells("#mbox-cells", &[0]);
    f.cells("phandle", &[mbox]);
    f.end();

    f.begin("firmware");
    f.strs("compatible", &["raspberrypi,bcm2835-firmware", "simple-mfd"]);
    f.cells("mboxes", &[mbox]);
    f.end();
    // Gigabit Ethernet: GENET v5 with the BCM54213PE PHY at MDIO address 1
    f.begin("ethernet@fd580000");
    f.str("compatible", "brcm,bcm2711-genet-v5");
    f.cells("reg", &[0, 0xfd58_0000, 0x10000]);
    f.cells("#address-cells", &[1]);
    f.cells("#size-cells", &[1]);
    f.cells("interrupts", &[GIC_SPI, 157, LEVEL_HIGH, GIC_SPI, 158, LEVEL_HIGH]);
    f.cells("phy-handle", &[phy1]);
    f.str("phy-mode", "rgmii-rxid");
    // locally administered address (the firmware would copy the board's OTP MAC here)
    f.prop("local-mac-address", &[0x02, 0x5b, 0xc0, 0x00, 0x00, 0x01]);
    f.str("status", "okay");
    f.begin("mdio@e14");
    f.str("compatible", "brcm,genet-mdio-v5");
    f.cells("reg", &[0xe14, 0x8]);
    f.str("reg-names", "mdio");
    f.cells("#address-cells", &[1]);
    f.cells("#size-cells", &[0]);
    f.cells("clock-frequency", &[1_950_000]);
    f.begin("ethernet-phy@1");
    f.cells("reg", &[1]);
    f.cells("phandle", &[phy1]);
    f.end();
    f.end();
    f.end();

    // the USB-C OTG port in host mode, as with `dtoverlay=dwc2,dr_mode=host`
    f.begin("usb@fe980000");
    f.str("compatible", "brcm,bcm2835-usb");
    f.cells("reg", &[0, 0xfe98_0000, 0x10000]);
    f.cells("interrupts", &[GIC_SPI, 73, LEVEL_HIGH]);
    f.str("dr_mode", "host");
    f.cells("g-np-tx-fifo-size", &[32]);
    f.cells("g-rx-fifo-size", &[558]);
    f.cells("g-tx-fifo-size", &[512, 512, 512, 512, 512, 256, 256]);
    f.cells("#address-cells", &[1]);
    f.cells("#size-cells", &[0]);
    f.str("status", "okay");
    f.end();

    // the second SD host ("mmcnr") with the CYW43455 Wi-Fi chip on SDIO function 1/2
    f.begin("mmc@fe300000");
    f.str("compatible", "brcm,bcm2835-sdhci");
    f.cells("reg", &[0, 0xfe30_0000, 0x100]);
    f.cells("interrupts", &[GIC_SPI, 126, LEVEL_HIGH]);
    f.cells("clocks", &[clk100]);
    f.cells("bus-width", &[4]);
    f.empty("non-removable");
    f.cells("#address-cells", &[1]);
    f.cells("#size-cells", &[0]);
    f.str("status", "okay");
    f.begin("wifi@1");
    f.cells("reg", &[1]);
    f.str("compatible", "brcm,bcm4329-fmac");
    f.end();
    f.end();

    f.begin("mmc@fe340000");
    f.str("compatible", "brcm,bcm2711-emmc2");
    f.cells("reg", &[0, 0xfe34_0000, 0x100]);
    f.cells("interrupts", &[GIC_SPI, 126, LEVEL_HIGH]);
    f.cells("clocks", &[clk100]);
    f.cells("bus-width", &[4]);
    f.empty("non-removable");
    f.str("status", "okay");
    f.end();
    f.end(); // soc

    if let Some((base, w, h, stride)) = b.fb {
        f.begin(&format!("framebuffer@{base:x}"));
        f.str("compatible", "simple-framebuffer");
        f.cells("reg", &[(base >> 32) as u32, base as u32, stride * h]);
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
