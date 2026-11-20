//! Linux arm64 direct boot with a reduced RK3588S DT, not the vendor firmware chain.
use emu_core::fdt::Fdt;
use crate::Machine;

pub const DEFAULT_CMDLINE:&str="console=ttyS2,1500000n8 earlycon=uart8250,mmio32,0xfeb50000 8250.nr_uarts=3 rdinit=/init loglevel=7";
pub const DTB:u64=0x03f0_0000;
/// Phandles of the cpu nodes (core k: CPU_PHANDLE + k).
const CPU_PHANDLE:u32=0x10;
pub const INITRD:u64=0x0400_0000;

/// What the reduced DT describes besides the fixed devices.
#[derive(Clone, Debug, Default)]
pub struct Hardware {
    /// a microSD card is in the slot (the DW MMC node)
    pub sd: bool,
    /// the Mali-G610 node (`--gpu probe`)
    pub gpu: bool,
    /// cores 0..cpus: 0..3 are Cortex-A55, 4..7 Cortex-A76
    pub cpus: usize,
    /// the firmware's HDMI framebuffer (address, mode)
    pub framebuffer: Option<(u64, crate::display::Mode)>,
}
impl Hardware {
    pub fn of(m:&Machine)->Self {
        Self{sd:m.soc.mmc.card.present(),gpu:m.gpu_probe,cpus:m.cpus.len(),framebuffer:m.soc.display.scanout}
    }
}

pub fn device_tree(ram:u64,cmdline:&str,initrd:Option<(u64,u64)>, sd:bool)->Vec<u8> {
    device_tree_for(ram,cmdline,initrd,&Hardware{sd,cpus:1,..Default::default()})
}
pub fn device_tree_for(ram:u64,cmdline:&str,initrd:Option<(u64,u64)>,hw:&Hardware)->Vec<u8> {
    let (sd,gpu,cpus)=(hw.sd,hw.gpu,hw.cpus);
    let mut f=Fdt::default();
    f.begin("");
    f.strs("compatible", &["xunlong,orangepi-5","rockchip,rk3588s"]);
    f.str("model","Orange Pi 5 (sbc-emu reduced RK3588S DT)");
    f.cells("#address-cells",&[2]); f.cells("#size-cells",&[2]); f.cells("interrupt-parent",&[1]);
    f.begin("aliases");f.str("serial2","/serial@feb50000");f.end();
    f.begin("chosen");f.str("bootargs",cmdline);f.str("stdout-path","serial2:1500000n8");
    if let Some((a,b))=initrd {f.cells("linux,initrd-start",&[(a>>32) as u32,a as u32]);f.cells("linux,initrd-end",&[(b>>32) as u32,b as u32]);}
    if let Some((addr,mode))=hw.framebuffer {
        // The HDMI framebuffer the firmware set up, for simpledrm (its memory is reserved below).
        f.cells("#address-cells",&[2]);f.cells("#size-cells",&[2]);f.empty("ranges");
        f.begin(&format!("framebuffer@{addr:x}"));f.str("compatible","simple-framebuffer");
        f.cells("reg",&[(addr>>32) as u32,addr as u32,0,crate::display::FB_SLOT as u32]);
        f.cells("width",&[mode.width]);f.cells("height",&[mode.height]);f.cells("stride",&[mode.stride()]);
        f.str("format","x8r8g8b8");f.str("status","okay");f.end();
    }
    f.end();
    f.begin("reserved-memory");f.cells("#address-cells",&[2]);f.cells("#size-cells",&[2]);f.empty("ranges");
    if let Some((addr,_))=hw.framebuffer {
        f.begin(&format!("framebuffer@{addr:x}"));f.cells("reg",&[(addr>>32) as u32,addr as u32,0,crate::display::FB_SLOT as u32]);f.empty("no-map");f.end();
    }
    // the SCMI channel's shared memory (Linux maps it as I/O memory)
    f.begin(&format!("shmem@{:x}",crate::scmi::SHMEM));f.str("compatible","arm,scmi-shmem");
    f.cells("reg",&[0,crate::scmi::SHMEM as u32,0,crate::scmi::SHMEM_SIZE as u32]);f.empty("no-map");f.cells("phandle",&[PH_SCMI_SHMEM]);f.end();
    f.end();
    // The firmware's SCMI clocks and resets over SMC (what TF-A provides on the RK3588).
    f.begin("firmware");f.begin("scmi");f.str("compatible","arm,scmi-smc");f.cells("arm,smc-id",&[crate::scmi::SMC_ID]);
    f.cells("shmem",&[PH_SCMI_SHMEM]);f.cells("#address-cells",&[1]);f.cells("#size-cells",&[0]);
    f.begin("protocol@14");f.cells("reg",&[0x14]);f.cells("#clock-cells",&[1]);f.cells("phandle",&[PH_SCMI_CLK]);f.end();
    f.begin("protocol@16");f.cells("reg",&[0x16]);f.cells("#reset-cells",&[1]);f.cells("phandle",&[PH_SCMI_RESET]);f.end();
    f.end();f.end();
    f.begin("memory@0");f.str("device_type","memory");f.cells("reg",&[0,0,(ram>>32) as u32,ram as u32]);f.end();
    f.begin("cpus");f.cells("#address-cells",&[1]);f.cells("#size-cells",&[0]);
    for k in 0..cpus {
        let reg=crate::gic::affinity(k) as u32;
        f.begin(&format!("cpu@{reg:x}"));f.str("device_type","cpu");
        f.str("compatible",if k<4 {"arm,cortex-a55"} else {"arm,cortex-a76"});f.cells("reg",&[reg]);f.str("enable-method","psci");
        // relative capacities as in the mainline rk3588s.dtsi (energy-aware scheduling)
        f.cells("capacity-dmips-mhz",&[if k<4 {530} else {1024}]);
        f.cells("clocks",&[if k<4 {6} else {7}]);f.cells("phandle",&[CPU_PHANDLE+k as u32]);f.end();
    }
    // Clusters as in the mainline rk3588s.dtsi: the A55s share one, the A76s are in pairs.
    // Linux reads the capacities above only through a cpu-map.
    f.begin("cpu-map");
    for (c,cores) in [(0..4),(4..6),(6..8)].into_iter().enumerate() {
        if cores.start>=cpus {break;}
        f.begin(&format!("cluster{c}"));
        for (n,k) in cores.filter(|&k|k<cpus).enumerate() { f.begin(&format!("core{n}"));f.cells("cpu",&[CPU_PHANDLE+k as u32]);f.end(); }
        f.end();
    }
    f.end();
    f.end();
    // The cores' fixed rates (the board's top OPPs), which the scheduler's capacities scale by.
    for (name,hz,ph) in [("cpu-little-clock",1_800_000_000u32,6),("cpu-big-clock",2_400_000_000,7)] {
        f.begin(name);f.str("compatible","fixed-clock");f.cells("#clock-cells",&[0]);f.cells("clock-frequency",&[hz]);f.cells("phandle",&[ph]);f.end();
    }
    f.begin("psci");f.strs("compatible",&["arm,psci-1.0","arm,psci-0.2"]);f.str("method","smc");f.end();
    f.begin("timer");f.str("compatible","arm,armv8-timer");f.empty("always-on");
    f.cells("interrupts",&[1,13,4,1,14,4,1,11,4,1,10,4]);f.end();
    f.begin("interrupt-controller@fe600000");f.str("compatible","arm,gic-v3");
    f.cells("reg",&[0,0xfe600000,0,0x10000,0,0xfe680000,0,0x100000]);
    f.empty("interrupt-controller");f.cells("#interrupt-cells",&[3]);f.cells("#address-cells",&[2]);f.cells("#size-cells",&[2]);f.empty("ranges");f.cells("phandle",&[1]);f.end();
    // Firmware-initialized clocks: CRU/PMU/pinctrl are intentionally absent in this DT.
    f.begin("uart-clock");f.str("compatible","fixed-clock");f.cells("#clock-cells",&[0]);f.cells("clock-frequency",&[24_000_000]);f.cells("phandle",&[2]);f.end();
    f.begin("serial@feb50000");f.strs("compatible",&["rockchip,rk3588-uart","snps,dw-apb-uart"]);
    f.cells("reg",&[0,0xfeb50000,0,0x100]);f.cells("interrupts",&[0,333,4]);
    f.cells("reg-shift",&[2]);f.cells("reg-io-width",&[4]);f.cells("clocks",&[2,2]);f.strs("clock-names",&["baudclk","apb_pclk"]);f.end();
    if gpu {
        f.begin("gpu-supply");f.str("compatible","regulator-fixed");f.str("regulator-name","gpu");
        f.cells("regulator-min-microvolt",&[750000]);f.cells("regulator-max-microvolt",&[750000]);
        f.empty("regulator-always-on");f.cells("phandle",&[4]);f.end();
        f.begin("gpu-opp-table");f.str("compatible","operating-points-v2");f.cells("phandle",&[5]);
        f.begin("opp-24000000");f.cells("opp-hz",&[0,24000000]);f.cells("opp-microvolt",&[750000]);f.end();f.end();
        f.begin("gpu@fb000000"); f.cells("mali-supply",&[4]);f.cells("operating-points-v2",&[5]); f.strs("compatible",&["rockchip,rk3588-mali","arm,mali-valhall-csf"]);
        f.cells("reg",&[0,0xfb000000,0,0x200000]);f.cells("interrupts",&[0,92,4,0,93,4,0,94,4]);
        f.strs("interrupt-names",&["job","mmu","gpu"]);f.cells("clocks",&[2,2,2]);
        f.strs("clock-names",&["core","coregroup","stacks"]);f.end();
    }
    f.begin("ethernet@fe1c0000"); f.strs("compatible",&["snps,dwmac-4.20a","snps,dwmac"]);
    f.cells("reg",&[0,0xfe1c0000,0,0x10000]); f.cells("interrupts",&[0,234,4]);
    f.str("interrupt-names","macirq"); f.cells("clocks",&[2,2]); f.strs("clock-names",&["stmmaceth","pclk"]);
    f.prop("local-mac-address",&[0x52,0x54,0x00,0x12,0x34,0x56]);
    f.str("phy-mode","rgmii-rxid"); f.cells("phy-handle",&[3]);
    f.begin("mdio"); f.str("compatible","snps,dwmac-mdio"); f.cells("#address-cells",&[1]); f.cells("#size-cells",&[0]);
    f.begin("ethernet-phy@1"); f.cells("reg",&[1]); f.cells("phandle",&[3]); f.end(); f.end(); f.end();
    // The USB 3.0 host (usb_host2_xhci): the board's USB-A port. Its DWC3 glue is left in
    // host mode by the firmware, so Linux sees a platform xHCI.
    f.begin("usb@fcd00000");f.str("compatible","generic-xhci");
    f.cells("reg",&[0,crate::XHCI as u32,0,0x10000]);f.cells("interrupts",&[0,(crate::XHCI_IRQ-32) as u32,4]);f.end();
    // The M.2 slot's PCIe 2.0 x1 controller as a generic ECAM host with one bus; INTA of
    // device 0 is the controller's legacy interrupt.
    {
        use crate::pcie::{ECAM,ECAM_SIZE,MEM,MEM_SIZE,IRQ};
        f.begin(&format!("pcie@{ECAM:x}"));f.str("compatible","pci-host-ecam-generic");f.str("device_type","pci");
        f.cells("reg",&[0,ECAM as u32,0,ECAM_SIZE as u32]);f.cells("bus-range",&[0,0]);
        f.cells("#address-cells",&[3]);f.cells("#size-cells",&[2]);f.cells("#interrupt-cells",&[1]);
        f.cells("ranges",&[0x0200_0000,0,MEM as u32,0,MEM as u32,0,MEM_SIZE as u32]);
        f.cells("interrupt-map-mask",&[0,0,0,7]);
        f.cells("interrupt-map",&[0,0,0,1, 1,0,0, 0,(IRQ-32) as u32,4]);
        f.empty("dma-coherent");f.end();
    }
    audio_nodes(&mut f,hw.framebuffer.is_some());
    if hw.framebuffer.is_some() { display_nodes(&mut f); }
    if sd {
        f.begin("mmc@fe2c0000");f.strs("compatible",&["rockchip,rk3588-dw-mshc","rockchip,rk3288-dw-mshc"]);
        f.cells("reg",&[0,0xfe2c0000,0,0x4000]);f.cells("interrupts",&[0,203,4]);
        f.cells("clocks",&[2,2]);f.strs("clock-names",&["biu","ciu"]);f.cells("fifo-depth",&[256]);
        f.cells("bus-width",&[4]);f.cells("max-frequency",&[24000000]);f.empty("disable-wp");f.end();
    }
    f.end();f.finish()
}

/// Phandles of the SCMI and display nodes.
const PH_SCMI_SHMEM:u32=0x30; const PH_SCMI_CLK:u32=0x31; const PH_SCMI_RESET:u32=0x32;
const PH_SYS_GRF:u32=0x33; const PH_VOP_GRF:u32=0x34; const PH_VO1_GRF:u32=0x35; const PH_HDPTX_GRF:u32=0x36; const PH_PMU:u32=0x37;
const PH_HDPTX:u32=0x38; const PH_VOP_PORTS:u32=0x39; const PH_VP0_HDMI0:u32=0x3a; const PH_HDMI0_IN:u32=0x3b; const PH_HDMI0:u32=0x3c;

/// HDMI0 as Linux's rockchipdrm drives it: VOP2's video port 0, the DesignWare HDMI QP
/// transmitter and its Samsung HDPTX PHY, with their GRFs, SCMI clocks and resets. No IOMMU:
/// display buffers come from CMA. (The firmware framebuffer above stays until rockchipdrm
/// takes the display over.)
fn display_nodes(f:&mut Fdt) {
    use crate::grf::*;
    use crate::scmi::{clk,rst};
    let reg=|f:&mut Fdt,base:u64,size:u64|f.cells("reg",&[0,base as u32,0,size as u32]);
    for (name,compat,base,size,ph) in [("syscon","rockchip,rk3588-sys-grf",SYS_GRF,0x1000,PH_SYS_GRF),("syscon","rockchip,rk3588-vop-grf",VOP_GRF,0x2000,PH_VOP_GRF),
        ("syscon","rockchip,rk3588-vo1-grf",VO1_GRF,0x4000,PH_VO1_GRF),("syscon","rockchip,rk3588-hdptxphy-grf",HDPTXPHY0_GRF,0x100,PH_HDPTX_GRF),("power-management","rockchip,rk3588-pmu",PMU,0x400,PH_PMU)] {
        f.begin(&format!("{name}@{base:x}"));f.strs("compatible",&[compat,"syscon"]);reg(f,base,size);f.cells("phandle",&[ph]);f.end();
    }
    f.begin(&format!("phy@{HDPTXPHY0:x}"));f.str("compatible","rockchip,rk3588-hdptx-phy");reg(f,HDPTXPHY0,HDPTXPHY_SIZE);
    f.cells("clocks",&[PH_SCMI_CLK,clk::HDPTX_REF,PH_SCMI_CLK,clk::HDPTX_APB]);f.strs("clock-names",&["ref","apb"]);
    f.cells("resets",&[PH_SCMI_RESET,rst::HDPTX_APB,PH_SCMI_RESET,rst::HDPTX_INIT,PH_SCMI_RESET,rst::HDPTX_CMN,PH_SCMI_RESET,rst::HDPTX_LANE]);
    f.strs("reset-names",&["apb","init","cmn","lane"]);f.cells("rockchip,grf",&[PH_HDPTX_GRF]);
    f.cells("#clock-cells",&[0]);f.cells("#phy-cells",&[0]);f.cells("phandle",&[PH_HDPTX]);f.end();
    {
        use crate::hdmi_qp::*;
        f.begin(&format!("hdmi@{BASE:x}"));f.str("compatible","rockchip,rk3588-dw-hdmi-qp");reg(f,BASE,SIZE);
        f.cells("clocks",&[PH_SCMI_CLK,clk::HDMI0_PCLK,PH_SCMI_CLK,clk::HDMI0_EARC,PH_SCMI_CLK,clk::HDMI0_REF,PH_SCMI_CLK,clk::HDMI0_AUD,PH_SCMI_CLK,clk::HDMI0_HDP,PH_SCMI_CLK,clk::HCLK_VO1]);
        f.strs("clock-names",&["pclk","earc","ref","aud","hdp","hclk_vo1"]);
        let irqs:Vec<u32>=IRQS.iter().flat_map(|&i|[0,(i-32) as u32,4]).collect();
        f.cells("interrupts",&irqs);f.strs("interrupt-names",&["avp","cec","earc","main","hpd"]);
        f.cells("phys",&[PH_HDPTX]);f.cells("resets",&[PH_SCMI_RESET,rst::HDMI0_REF,PH_SCMI_RESET,rst::HDMI0_HDP]);f.strs("reset-names",&["ref","hdp"]);
        f.cells("rockchip,grf",&[PH_SYS_GRF]);f.cells("rockchip,vo-grf",&[PH_VO1_GRF]);f.cells("#sound-dai-cells",&[0]);f.cells("phandle",&[PH_HDMI0]);
        f.begin("ports");f.cells("#address-cells",&[1]);f.cells("#size-cells",&[0]);
        f.begin("port@0");f.cells("reg",&[0]);f.begin("endpoint");f.cells("remote-endpoint",&[PH_VP0_HDMI0]);f.cells("phandle",&[PH_HDMI0_IN]);f.end();f.end();
        f.end();f.end();
    }
    {
        use crate::vop2::*;
        f.begin(&format!("vop@{BASE:x}"));f.str("compatible","rockchip,rk3588-vop");reg(f,BASE,SIZE);f.str("reg-names","vop");
        f.cells("interrupts",&[0,(IRQ-32) as u32,4]);
        f.cells("clocks",&[PH_SCMI_CLK,clk::ACLK_VOP,PH_SCMI_CLK,clk::HCLK_VOP,PH_SCMI_CLK,clk::DCLK_VP0,PH_SCMI_CLK,clk::DCLK_VP1,PH_SCMI_CLK,clk::DCLK_VP2,PH_SCMI_CLK,clk::DCLK_VP3,PH_SCMI_CLK,clk::PCLK_VOP]);
        f.strs("clock-names",&["aclk","hclk","dclk_vp0","dclk_vp1","dclk_vp2","dclk_vp3","pclk_vop"]);
        f.cells("rockchip,grf",&[PH_SYS_GRF]);f.cells("rockchip,vop-grf",&[PH_VOP_GRF]);f.cells("rockchip,vo1-grf",&[PH_VO1_GRF]);f.cells("rockchip,pmu",&[PH_PMU]);
        f.begin("ports");f.cells("#address-cells",&[1]);f.cells("#size-cells",&[0]);f.cells("phandle",&[PH_VOP_PORTS]);
        f.begin("port@0");f.cells("#address-cells",&[1]);f.cells("#size-cells",&[0]);f.cells("reg",&[0]);
        // endpoint 2: ROCKCHIP_VOP2_EP_HDMI0
        f.begin("endpoint@2");f.cells("reg",&[2]);f.cells("remote-endpoint",&[PH_HDMI0_IN]);f.cells("phandle",&[PH_VP0_HDMI0]);f.end();
        f.end();f.end();f.end();
    }
    f.begin("display-subsystem");f.str("compatible","rockchip,display-subsystem");f.cells("ports",&[PH_VOP_PORTS]);f.end();
}

/// Phandles of the audio nodes.
const PH_I2S_MCLK:u32=8; const PH_CODEC_MCLK:u32=9;
const PH_DMAC:[u32;2]=[0x20,0x21]; const PH_I2S1:u32=0x22; const PH_I2S5:u32=0x23;
const PH_ES8388:u32=0x24; const PH_HDMI_TX:u32=0x25; const PH_MASTERDAI:u32=0x26;

/// The two sound cards: the ES8388 headphone codec on I2C6 + I2S1 (named and routed as in
/// the board's DT, without the jack-detect GPIO), and HDMI0 audio on I2S5 into the HDMI
/// transmitter's codec (`hdmi`: the display chain is described; without it, a plain I2S
/// transmitter stands in).
fn audio_nodes(f:&mut Fdt,hdmi:bool) {
    use crate::audio::*;
    let reg=|f:&mut Fdt,base:u64,size:u64|f.cells("reg",&[0,base as u32,0,size as u32]);
    f.begin("i2s-mclk");f.str("compatible","fixed-clock");f.cells("#clock-cells",&[0]);f.cells("clock-frequency",&[I2S_MCLK as u32]);f.cells("phandle",&[PH_I2S_MCLK]);f.end();
    f.begin("es8388-mclk");f.str("compatible","fixed-clock");f.cells("#clock-cells",&[0]);f.cells("clock-frequency",&[crate::es8388::MCLK as u32]);f.cells("phandle",&[PH_CODEC_MCLK]);f.end();
    for (k,base) in [DMAC0,DMAC2].into_iter().enumerate() {
        f.begin(&format!("dma-controller@{base:x}"));f.strs("compatible",&["arm,pl330","arm,primecell"]);reg(f,base,DMAC_SIZE);
        f.cells("interrupts",&[0,(DMAC_IRQS[k][0]-32) as u32,4,0,(DMAC_IRQS[k][1]-32) as u32,4]);f.empty("arm,pl330-periph-burst");
        f.cells("clocks",&[2]);f.str("clock-names","apb_pclk");f.cells("#dma-cells",&[1]);f.cells("phandle",&[PH_DMAC[k]]);f.end();
    }
    let i2s=|f:&mut Fdt,base:u64,irq:usize,dmas:&[u32],names:&[&str],ph:u32|{
        f.begin(&format!("i2s@{base:x}"));f.str("compatible","rockchip,rk3588-i2s-tdm");reg(f,base,crate::i2s_tdm::SIZE);
        f.cells("interrupts",&[0,(irq-32) as u32,4]);f.cells("clocks",&[PH_I2S_MCLK,PH_I2S_MCLK,2]);f.strs("clock-names",&["mclk_tx","mclk_rx","hclk"]);
        f.cells("dmas",dmas);f.strs("dma-names",names);f.cells("#sound-dai-cells",&[0]);f.cells("phandle",&[ph]);f.end();
    };
    i2s(f,I2S1,I2S1_IRQ,&[PH_DMAC[0],PERI_I2S1_TX as u32,PH_DMAC[0],PERI_I2S1_RX as u32],&["tx","rx"],PH_I2S1);
    i2s(f,I2S5,I2S5_IRQ,&[PH_DMAC[1],PERI_I2S5_TX as u32],&["tx"],PH_I2S5);
    f.begin(&format!("i2c@{I2C6:x}"));f.strs("compatible",&["rockchip,rk3588-i2c","rockchip,rk3399-i2c"]);reg(f,I2C6,0x1000);
    f.cells("clocks",&[2,2]);f.strs("clock-names",&["i2c","pclk"]);f.cells("interrupts",&[0,(I2C6_IRQ-32) as u32,4]);
    f.cells("#address-cells",&[1]);f.cells("#size-cells",&[0]);
    f.begin("audio-codec@10");f.strs("compatible",&["everest,es8388","everest,es8328"]);f.cells("reg",&[crate::es8388::ADDRESS as u32]);
    f.cells("clocks",&[PH_CODEC_MCLK]);f.cells("#sound-dai-cells",&[0]);f.cells("phandle",&[PH_ES8388]);f.end();
    f.end();
    if !hdmi { f.begin("hdmi0-audio-tx");f.str("compatible","linux,spdif-dit");f.cells("#sound-dai-cells",&[0]);f.cells("phandle",&[PH_HDMI_TX]);f.end(); }
    f.begin("analog-sound");f.str("compatible","simple-audio-card");f.str("simple-audio-card,name","rockchip,es8388");
    f.str("simple-audio-card,format","i2s");f.cells("simple-audio-card,bitclock-master",&[PH_MASTERDAI]);f.cells("simple-audio-card,frame-master",&[PH_MASTERDAI]);
    f.strs("simple-audio-card,routing",&["Headphones","LOUT1","Headphones","ROUT1","LINPUT1","Microphone Jack","RINPUT1","Microphone Jack","LINPUT2","Onboard Microphone","RINPUT2","Onboard Microphone"]);
    f.strs("simple-audio-card,widgets",&["Microphone","Microphone Jack","Microphone","Onboard Microphone","Headphone","Headphones"]);
    f.begin("simple-audio-card,cpu");f.cells("sound-dai",&[PH_I2S1]);f.end();
    // the codec runs from its fixed 12.288 MHz MCLK: the 48 kHz family of rates
    f.begin("simple-audio-card,codec");f.cells("sound-dai",&[PH_ES8388]);f.cells("system-clock-frequency",&[crate::es8388::MCLK as u32]);f.cells("phandle",&[PH_MASTERDAI]);f.end();
    f.end();
    f.begin("hdmi0-sound");f.str("compatible","simple-audio-card");f.str("simple-audio-card,name","hdmi0");
    f.str("simple-audio-card,format","i2s");f.cells("simple-audio-card,mclk-fs",&[128]);
    f.begin("simple-audio-card,cpu");f.cells("sound-dai",&[PH_I2S5]);f.end();
    f.begin("simple-audio-card,codec");f.cells("sound-dai",&[if hdmi {PH_HDMI0} else {PH_HDMI_TX}]);f.end();
    f.end();
}

pub fn boot_linux(m:&mut Machine,kernel:&[u8],initrd:Option<&[u8]>,cmdline:&str)->Result<(),String> {
    if kernel.len()<64 || &kernel[56..60]!=b"ARMd" {return Err("expected uncompressed arm64 Linux Image".into());}
    let offset=u64::from_le_bytes(kernel[8..16].try_into().unwrap());
    let size=u64::from_le_bytes(kernel[16..24].try_into().unwrap()).max(kernel.len() as u64);
    let entry=0x20_0000u64.checked_add(offset).ok_or("kernel offset overflow")?;
    if entry.checked_add(size).is_none_or(|end|end>DTB) {return Err("kernel exceeds direct boot layout".into());}
    m.soc.ram[entry as usize..entry as usize+kernel.len()].copy_from_slice(kernel);
    let range=if let Some(data)=initrd {
        let end=INITRD.checked_add(data.len() as u64).ok_or("initrd size overflow")?;
        // below the HDMI framebuffer (no-map) when there is one
        let top=m.soc.display.scanout.map_or(m.soc.ram.len() as u64,|(fb,_)|fb);
        if end>top {return Err(format!("initrd ({} MiB) does not fit below {:#x}; give more --ram-mb",data.len()>>20,top));}
        m.soc.ram[INITRD as usize..end as usize].copy_from_slice(data);Some((INITRD,end))
    }else{None};
    let dt=device_tree_for(m.soc.ram.len() as u64,cmdline,range,&Hardware::of(m));
    if DTB+dt.len() as u64>INITRD {return Err("device tree exceeds boot layout".into());}
    m.soc.ram[DTB as usize..DTB as usize+dt.len()].copy_from_slice(&dt);
    m.dtb=dt;
    crate::scmi::Scmi::init_channel(&mut m.soc.ram);
    let c=m.cpu_mut();
    c.el=1; c.spsel=true; c.daif=0x3c0;c.pc=entry;
    c.x=[0;31];c.x[0]=DTB;c.sys.cpacr_el1=3<<20;
    Ok(())
}

/// Boot inputs read by emulated firmware from a GPT/ext4 Armbian image.
pub struct CardBoot {
    pub kernel: Vec<u8>,
    pub initrd: Option<Vec<u8>>,
    pub cmdline: String,
    pub notes: Vec<String>,
}

/// Implements the relevant Armbian `boot.cmd` inputs. The emulator generates its
/// reduced DT; neither U-Boot nor DT overlays execute here.
pub fn read_card(disk: &mut dyn emu_core::disk::Disk) -> Result<CardBoot, String> {
    use emu_core::ext4::Ext4;
    let parts=emu_core::gpt::partitions(disk)?;
    for part in parts {
        let Ok(mut fs)=Ext4::open(disk,part.first_lba) else {continue};
        for prefix in ["/boot/", "/"] {
            if !fs.exists(&format!("{prefix}Image")) {continue;}
            let mut notes=Vec::new();
            if fs.needs_recovery {notes.push("ext4 journal needs recovery; firmware does not replay it".to_string());}
            let env_path=format!("{prefix}armbianEnv.txt");
            let env_bytes=if fs.exists(&env_path){fs.read_file(&env_path)?}else{Vec::new()};
            let env_text=std::str::from_utf8(&env_bytes).map_err(|_|"armbianEnv.txt is not UTF-8")?;
            let mut env=std::collections::HashMap::new();
            for line in env_text.lines() {
                let line=line.trim();if line.is_empty()||line.starts_with('#'){continue;}
                if let Some((k,v))=line.split_once('='){env.insert(k.trim(),v.trim());}
            }
            let get=|key:&str,default:&str|env.get(key).copied().unwrap_or(default).to_string();
            if ["overlays","user_overlays"].iter().any(|k|env.get(k).is_some_and(|v|!v.is_empty())) {
                notes.push("device-tree overlays are not applied".into());
            }
            let kernel=fs.read_file(&format!("{prefix}Image"))?;
            let init_path=format!("{prefix}uInitrd");
            let initrd=if fs.exists(&init_path) {
                let data=fs.read_file(&init_path)?;
                if data.len()<64 || data[..4]!=[0x27,0x05,0x19,0x56] {return Err("uInitrd has no U-Boot legacy header".into());}
                let len=u32::from_be_bytes(data[12..16].try_into().unwrap()) as usize;
                let end=64usize.checked_add(len).filter(|&v|v<=data.len()).ok_or("truncated uInitrd")?;
                Some(data[64..end].to_vec())
            }else{None};
            let root_default=format!("UUID={}",fs.uuid_string());
            let cmdline=format!("root={} rootwait rootfstype={} console=ttyS2,1500000 earlycon=uart8250,mmio32,0xfeb50000 8250.nr_uarts=3 loglevel={} {} {}",
                get("rootdev",&root_default),get("rootfstype","ext4"),get("verbosity","1"),get("extraargs",""),get("extraboardargs",""));
            notes.push(format!("GPT partition {} at LBA {}; {}Image",part.number,part.first_lba,prefix));
            return Ok(CardBoot{kernel,initrd,cmdline,notes});
        }
    }
    Err("no GPT ext4 partition containing Image in /boot or /".into())
}
