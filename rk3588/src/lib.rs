//! RK3588S / Orange Pi 5: up to four Cortex-A55 and four Cortex-A76 cores, GICv3, UART2 and RAM.
//! Direct Linux boot, generated reduced DT, firmware PSCI and SCMI services handled by host.
pub mod boot;
pub mod gic;
pub mod gpu_mmu;
pub mod gpu_cs;
pub mod gpu;
pub mod gpu_fw;
pub mod gpu_render;
pub mod gpu_shader;
pub mod gpu_compute;
pub mod gpu_resources;
pub mod gpu_tiler;
pub mod gpu_raster;
pub mod gpu_blend;
pub mod gpu_texture;
pub mod uart;
pub mod display;
pub mod xhci;
pub mod nvme;
pub mod pcie;
#[cfg(not(target_arch = "wasm32"))]
mod mt;
pub mod mmc;
pub mod phy;
pub mod gmac;
pub mod i2c;
pub mod i2s_tdm;
pub mod es8388;
pub mod audio;
pub mod grf;
pub mod hdmi_qp;
pub mod scmi;
mod snapshot;
pub mod vop2;

use arm_a64::{Cpu, CpuProfile};
use emu_core::{Bus, Core, Fault, Stop, Trap};

pub const GMAC: u64 = 0xfe1c_0000;
/// The USB 3.0 host (usb_host2_xhci) and its interrupt.
pub const XHCI: u64 = 0xfcd0_0000;
pub const XHCI_IRQ: usize = 32 + 222;
pub const MMC: u64 = 0xfe2c_0000;
pub const UART2: u64 = 0xfeb5_0000;
pub const GICD: u64 = 0xfe60_0000;
pub const GICR: u64 = 0xfe68_0000;
/// One redistributor per core (the RK3588 has eight).
pub const GICR_SIZE: u64 = 0x10_0000;
pub const CNTFRQ: u64 = 24_000_000;
pub const UART_IRQ: usize = 32 + 333;

pub struct Soc {
    pub ram: Vec<u8>,
    pub uart: uart::Uart,
    pub mmc: mmc::Mmc,
    pub gpu: gpu::Gpu,
    pub gmac: gmac::Gmac,
    pub net: Option<sbc_net::net::VirtualNet>,
    pub gic: gic::Gic,
    pub display: display::Display,
    pub xhci: xhci::Xhci,
    /// the M.2 slot (PCIe 2.0 x1): empty or an NVMe SSD
    pub pcie: pcie::Pcie,
    /// the emulated USB devices: a keyboard and a tablet, and a stick with `--usb-drive`
    pub usb: emu_core::usb::Devices,
    /// the headphone codec (ES8388 on I2C6, I2S1) and HDMI audio (I2S5), fed by the PL330s
    pub audio: audio::Audio,
    /// the GRFs (syscons): SYS, VOP, VO1, HDPTXPHY0, PMU
    pub grf: [grf::Grf; 5],
    /// the display controller
    pub vop2: vop2::Vop2,
    /// the firmware's SCMI clocks and resets
    pub scmi: scmi::Scmi,
    /// virtual time (ns) at the last device update
    pub now: u64,
    /// HDMI0's transmitter (dw-hdmi-qp)
    pub hdmi: hdmi_qp::HdmiQp,
    /// HDMI0's PHY registers (Samsung HDPTX)
    pub hdptx: Vec<u32>,
    /// virtual time of the network's next poll (ns)
    pub next_net_poll: u64,
    pub dirty: bool,
    pub unknown: Vec<u64>,
}
impl Soc {
    fn read(&mut self, addr: u64, size: usize) -> Result<u64, Fault> {
        if addr.checked_add(size as u64).is_some_and(|end| end <= self.ram.len() as u64) {
            return Ok(self.ram[addr as usize..addr as usize+size].iter().enumerate()
                .fold(0, |v,(n,b)| v | ((*b as u64) << (8*n))));
        }
        if size == 8 { return Ok(self.read(addr,4)? | (self.read(addr+4,4)? << 32)); }
        self.dirty = true;
        let word = if (0xfb000000..0xfb200000).contains(&addr) {self.gpu.read((addr-0xfb000000)&!3)}
        else if (GMAC..GMAC+0x10000).contains(&addr) { self.gmac.read((addr-GMAC)&!3) }
        else if (XHCI..XHCI+0x10000).contains(&addr) { self.xhci.read((addr-XHCI)&!3) }
        else if (pcie::ECAM..pcie::ECAM+pcie::ECAM_SIZE).contains(&addr) { self.pcie.cfg_read((addr-pcie::ECAM)&!3) }
        else if (pcie::MEM..pcie::MEM+pcie::MEM_SIZE).contains(&addr) { self.pcie.mem_read(addr&!3).unwrap_or(u32::MAX) }
        else if (MMC..MMC+0x4000).contains(&addr) { self.mmc.read((addr-MMC)&!3) }
        else if audio::Audio::contains(addr) { self.audio.read(addr&!3) }
        else if let Some((k,off))=Self::grf_at(addr) { self.grf_read(k,off) }
        else if (hdmi_qp::BASE..hdmi_qp::BASE+hdmi_qp::SIZE).contains(&addr) { self.hdmi.read((addr-hdmi_qp::BASE)&!3) }
        else if (vop2::BASE..vop2::BASE+vop2::SIZE).contains(&addr) { self.vop2.read((addr-vop2::BASE)&!3) }
        else if (grf::HDPTXPHY0..grf::HDPTXPHY0+grf::HDPTXPHY_SIZE).contains(&addr) { self.hdptx[((addr-grf::HDPTXPHY0)/4) as usize] }
        else if (UART2..UART2+0x100).contains(&addr) { self.uart.read((addr-UART2)&!3) }
        else if (GICD..GICD+0x10000).contains(&addr) {self.gic.dist_read((addr-GICD)&!3)}
        else if (GICR..GICR+GICR_SIZE).contains(&addr) {self.gic.redist_read((addr-GICR)&!3)}
        else { if !self.unknown.contains(&addr) {self.unknown.push(addr);} return Err(Fault::Unmapped); };
        Ok(((word >> ((addr&3)*8)) as u64) & ((1u64 << (size*8))-1))
    }
    fn write(&mut self, addr: u64, size: usize, value: u64) -> Result<(), Fault> {
        if addr.checked_add(size as u64).is_some_and(|end| end <= self.ram.len() as u64) {
            self.ram[addr as usize..addr as usize+size].copy_from_slice(&value.to_le_bytes()[..size]); return Ok(());
        }
        if size == 8 { self.write(addr,4,value as u32 as u64)?; return self.write(addr+4,4,value>>32); }
        self.dirty = true;
        // Linux uses byte accesses for GIC priority registers. Preserve neighbouring priorities.
        let v = if size < 4 {
            let shift=(addr&3)*8; let mask=((1u32<<(size*8))-1)<<shift;
            let off = if (GICD..GICD+0x10000).contains(&addr) { addr-GICD } else { (addr.wrapping_sub(GICR) % gic::FRAME).wrapping_sub(0x10000) };
            let is_write_one = (0x100..0x400).contains(&off);
            let old=if (GMAC..GMAC+0x10000).contains(&addr) {if (addr-GMAC)&!3==0x1160 {0}else{self.gmac.read((addr-GMAC)&!3)}}
                // config space: Linux's generic ECAM host writes bytes and halfwords (the status
                // half of word 1 is write-one-to-clear and never stored, so merging is safe)
                else if (pcie::ECAM..pcie::ECAM+pcie::ECAM_SIZE).contains(&addr) {self.pcie.cfg_read((addr-pcie::ECAM)&!3)}
                else if is_write_one { 0 } else if (GICD..GICD+0x10000).contains(&addr) {self.gic.dist_read((addr-GICD)&!3)}
                else if (GICR..GICR+GICR_SIZE).contains(&addr) {self.gic.redist_read((addr-GICR)&!3)} else {0};
            (old & !mask) | ((value as u32)<<shift)
        } else {value as u32};
        if (0xfb000000..0xfb200000).contains(&addr) {self.gpu.write_memory((addr-0xfb000000)&!3,v,&mut self.ram);}
        else if (GMAC..GMAC+0x10000).contains(&addr) {self.gmac.write((addr-GMAC)&!3,v);}
        else if (XHCI..XHCI+0x10000).contains(&addr) {self.xhci.write((addr-XHCI)&!3,v,&mut self.ram,&mut self.usb);}
        else if (pcie::ECAM..pcie::ECAM+pcie::ECAM_SIZE).contains(&addr) {self.pcie.cfg_write((addr-pcie::ECAM)&!3,v);}
        else if (pcie::MEM..pcie::MEM+pcie::MEM_SIZE).contains(&addr) {self.pcie.mem_write(addr&!3,v,&mut self.ram);}
        else if (MMC..MMC+0x4000).contains(&addr) {self.mmc.write((addr-MMC)&!3,v);}
        else if audio::Audio::contains(addr) {self.audio.write(addr&!3,v,&mut self.ram);}
        else if let Some((k,off))=Self::grf_at(addr) {self.grf[k].write(off,v);}
        else if (hdmi_qp::BASE..hdmi_qp::BASE+hdmi_qp::SIZE).contains(&addr) {self.hdmi.write((addr-hdmi_qp::BASE)&!3,v);}
        else if (vop2::BASE..vop2::BASE+vop2::SIZE).contains(&addr) {self.vop2.write((addr-vop2::BASE)&!3,v,self.now);}
        else if (grf::HDPTXPHY0..grf::HDPTXPHY0+grf::HDPTXPHY_SIZE).contains(&addr) {self.hdptx[((addr-grf::HDPTXPHY0)/4) as usize]=v;}
        else if (UART2..UART2+0x100).contains(&addr) {self.uart.write((addr-UART2)&!3,v);}
        else if (GICD..GICD+0x10000).contains(&addr) {self.gic.dist_write((addr-GICD)&!3,v);}
        else if (GICR..GICR+GICR_SIZE).contains(&addr) {self.gic.redist_write((addr-GICR)&!3,v);}
        else { if !self.unknown.contains(&addr) {self.unknown.push(addr);} return Err(Fault::Unmapped); }
        Ok(())
    }
}
impl Soc {
    /// The GRF holding `addr`: (index in `grf`, offset).
    fn grf_at(addr: u64) -> Option<(usize,u64)> {
        grf::BLOCKS.iter().position(|&(b,s)|(b..b+s).contains(&addr)).map(|k|(k,(addr-grf::BLOCKS[k].0)&!3))
    }
    /// A GRF register, with the status bits that come from other devices.
    fn grf_read(&self, k: usize, off: u64) -> u32 {
        let v=self.grf[k].read(off);
        match (grf::BLOCKS[k].0,off) {
            // the monitor is always plugged in
            (grf::SYS_GRF,grf::SYS_GRF_SOC_STATUS1)=>v|grf::HDMI0_LEVEL,
            // the PHY's PLL locks and its lanes come up as soon as the PLL is enabled
            (grf::HDPTXPHY0_GRF,grf::HDPTX_STATUS)=>if self.grf[k].read(grf::HDPTX_CON0)&grf::HDPTX_PLL_EN!=0 {grf::HDPTX_READY} else {0},
            _=>v,
        }
    }
    /// Device work up to time `now` (ns): Ethernet frames and the network's timers, USB polls.
    pub fn service(&mut self, now: u64) {
        self.gmac.service(&mut self.ram);
        if let Some(net) = &mut self.net {
            let now_us = now / 1000;
            for frame in std::mem::take(&mut self.gmac.tx_out) {
                let replies=net.handle(&frame, now_us);
                if self.gmac.receiving() {sbc_net::net::enqueue_bounded(&mut self.gmac.rx_queue,replies); }
            }
            if now >= self.next_net_poll {
                let replies=net.poll(now_us);
                if self.gmac.receiving() {sbc_net::net::enqueue_bounded(&mut self.gmac.rx_queue,replies); }
                self.next_net_poll = now.saturating_add(1_000_000);
            }
            self.gmac.service(&mut self.ram);
        } else { self.gmac.tx_out.clear(); }
        self.xhci.tick(now,&mut self.ram,&mut self.usb);
        self.audio.service(now,&mut self.ram);
        self.now=self.now.max(now);
        self.vop2.tick(now,self.scmi.rate(scmi::clk::DCLK_VP0));
    }
    /// What the HDMI monitor shows: the firmware's framebuffer until Linux drives VOP2, then
    /// VOP2's output (black while it is off).
    pub fn render(&self, out: &mut Vec<u8>) -> Option<(u32,u32)> {
        if self.vop2.taken_over() { return self.vop2.render(&self.ram,out); }
        self.display.render(&self.ram,out)
    }
    pub fn shown_size(&self) -> Option<(u32,u32)> {
        if self.vop2.taken_over() { return self.vop2.size(); }
        self.display.shown_size()
    }
    /// The SCMI call (SMC 0x82000010) of a core.
    pub fn scmi_call(&mut self) { let Soc{scmi,ram,..}=self; scmi.handle(ram); self.dirty=true; }
    /// The devices' interrupt lines into the GIC.
    pub fn device_lines(&mut self) {
        self.gic.set_line(XHCI_IRQ,self.xhci.irq());
        self.gic.set_line(pcie::IRQ,self.pcie.irq());
        self.gic.set_line(32+234,self.gmac.irq());
        self.gic.set_line(UART_IRQ,self.uart.irq());
        self.gic.set_line(32+203,self.mmc.irq());
        for (irq,level) in self.audio.lines() { self.gic.set_line(irq,level); }
        self.gic.set_line(hdmi_qp::IRQ_MAIN,self.hdmi.irq());
        self.gic.set_line(vop2::IRQ,self.vop2.irq());
        for (i,spi) in [92,93,94].into_iter().enumerate() { self.gic.set_line(32+spi,self.gpu.irq(i)); }
    }
}
macro_rules! bus_access {
    ($read:ident,$write:ident,$ty:ty,$size:expr) => {
        fn $read(&mut self,a:u64)->Result<$ty,Fault> {self.read(a,$size).map(|v|v as $ty)}
        fn $write(&mut self,a:u64,v:$ty)->Result<(),Fault> {self.write(a,$size,v as u64)}
    }
}
impl Bus for Soc {
    bus_access!(read8,write8,u8,1);
    bus_access!(read16,write16,u16,2);
    bus_access!(read32,write32,u32,4);
    bus_access!(read64,write64,u64,8);
    fn read_sysreg(&mut self,k:u32)->Option<u64> {self.dirty=true; self.gic.sys_read(k)}
    fn write_sysreg(&mut self,k:u32,v:u64)->bool {self.dirty=true; self.gic.sys_write(k,v)}
}

/// What a core is doing between steps.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum CoreState {
    /// powered off (not yet started by PSCI CPU_ON, or CPU_OFF)
    Off,
    Run,
    /// `wfi`: asleep until its interrupt line rises
    Wfi,
    /// `wfe` with no other core running: asleep until another core runs or time is skipped
    Wfe,
}

/// Virtual time each core runs per round while several cores are runnable (ns = instructions);
/// `SBC_QUANTUM` overrides it.
fn quantum() -> u64 {
    static Q: std::sync::OnceLock<u64> = std::sync::OnceLock::new();
    *Q.get_or_init(|| std::env::var("SBC_QUANTUM").ok().and_then(|v| v.parse().ok()).unwrap_or(20_000))
}

/// The RK3588S has four Cortex-A55 (cores 0..3) and four Cortex-A76 (cores 4..7).
pub const MAX_CPUS: usize = 8;
pub fn core_profile(k: usize) -> CpuProfile { if k < 4 { CpuProfile::CORTEX_A55 } else { CpuProfile::CORTEX_A76 } }
/// MPIDR_EL1 of core `k` without the RES1 bit: MT set, Aff1 = core, as on the RK3588.
pub fn core_mpidr(k: usize) -> u64 { 0x0100_0000 | gic::affinity(k) }

pub struct Machine {
    pub cpus: Vec<Cpu>,
    pub state: Vec<CoreState>,
    pub gpu_probe: bool,
    /// the device tree handed to Linux
    pub dtb: Vec<u8>,
    pub soc: Soc,
    /// virtual time in ns: the latest time any core or device has seen
    pub now: u64,
    /// the core running now (or last): it holds the shared JIT
    cur: usize,
    /// instruction count of the current core at which interrupt lines must be re-evaluated
    next_check: u64,
    /// virtual time at which the current round ends (u64::MAX: one core runs alone)
    round_end: u64,
    /// run the cores on their own host threads (`--threads multi`: faster, not repeatable)
    pub multi: bool,
    #[cfg(not(target_arch = "wasm32"))]
    mt: Option<mt::Mt>,
    pub irqs: u64,
    pub exceptions: u64,
    /// a debugger's breakpoints (set through `debug_breakpoint`)
    pub breakpoints: emu_core::debug::Breakpoints,
}
impl Machine {
    pub fn new(ram_bytes: usize) -> Result<Self,String> { Self::with_cpus(ram_bytes, 1) }
    /// An Orange Pi 5 with the first `ncpu` cores of the RK3588S; only core 0 starts.
    pub fn with_cpus(ram_bytes: usize, ncpu: usize) -> Result<Self,String> {
        if !(128<<20..=2usize<<30).contains(&ram_bytes) {return Err("Orange Pi bring-up supports 128..2048 MiB RAM".into());}
        if !(1..=MAX_CPUS).contains(&ncpu) {return Err(format!("the RK3588S has 1..{MAX_CPUS} cores"));}
        let soc=Soc {ram:vec![0;ram_bytes], uart:Default::default(), mmc:Default::default(), gpu:Default::default(), gmac:Default::default(), net:None, gic:gic::Gic::with_cpus(ncpu),display:Default::default(),xhci:{let mut x=xhci::Xhci::new();x.attach(&[emu_core::usb::DevId::Keyboard,emu_core::usb::DevId::Tablet]);x},usb:Default::default(),pcie:Default::default(),audio:Default::default(),grf:grf::BLOCKS.map(|(_,s)|grf::Grf::new(s)),vop2:Default::default(),scmi:Default::default(),now:0,hdptx:vec![0;(grf::HDPTXPHY_SIZE/4) as usize],hdmi:hdmi_qp::HdmiQp::new(display::edid(&display::Mode::MODES[0])),next_net_poll:0,dirty:true,unknown:Vec::new()};
        let mut m=Self{cpus:Vec::new(),state:vec![CoreState::Off;ncpu],soc,gpu_probe:false,dtb:Vec::new(),now:0,cur:0,next_check:0,round_end:u64::MAX,multi:false,#[cfg(not(target_arch = "wasm32"))] mt:None,irqs:0,exceptions:0,breakpoints:Default::default()};
        for k in 0..ncpu { let c=m.fresh_cpu(k); m.cpus.push(c); }
        m.state[0]=CoreState::Run;
        Ok(m)
    }
    fn fresh_cpu(&mut self, k: usize) -> Cpu {
        let mut cpu=Cpu::new();
        cpu.profile=core_profile(k);
        cpu.sys.cntfrq=CNTFRQ;
        cpu.sys.mpidr=core_mpidr(k);
        cpu.ram=self.soc.ram.as_mut_ptr(); cpu.ram_len=self.soc.ram.len() as u64;
        cpu.firmware_calls_to_host=true;
        // one compiled-pages map for all cores (IC IVAU broadcasts only where code was compiled)
        cpu.code_pages=Some(match self.cpus.first().and_then(|c|c.code_pages.clone()) { Some(m)=>m, None=>Cpu::code_page_map(cpu.ram_len) });
        cpu
    }
    /// Core 0, the boot core.
    pub fn cpu(&self) -> &Cpu { &self.cpus[0] }
    pub fn cpu_mut(&mut self) -> &mut Cpu { &mut self.cpus[0] }
    /// Instructions retired by all cores.
    pub fn instret(&self) -> u64 { self.cpus.iter().map(|c| c.instret).sum() }
    /// Turn the block JIT on (one code cache shared by all cores).
    pub fn enable_jit(&mut self) -> bool { self.cpus[self.cur].enable_jit() }
    pub fn connect_network(&mut self, nat: bool, log: bool) {
        let mut net = sbc_net::net::VirtualNet::new(log);
        #[cfg(not(target_arch = "wasm32"))]
        if nat { net.nat = Some(sbc_net::nat::Nat::new(log)); }
        let _ = nat;
        self.soc.gmac.phy.set_link(true);
        self.soc.net = Some(net);
    }
    pub fn now(&self)->u64 {self.now.max(self.cpus[self.cur].now_ns())}
    /// Hand the HDMI output over as the firmware's framebuffer at the top of RAM.
    pub fn enable_hdmi(&mut self, mode: display::Mode) {
        let addr=display::framebuffer_addr(self.soc.ram.len() as u64);
        self.soc.ram[addr as usize..].fill(0);
        self.soc.display=display::Display::new(addr,mode);
        // the monitor's EDID prefers the same mode
        self.soc.hdmi=hdmi_qp::HdmiQp::new(display::edid(&mode));
    }
    /// Plug a USB stick backed by `disk` in next to the keyboard and the tablet.
    pub fn attach_usb_drive(&mut self, disk: Box<dyn emu_core::disk::Disk + Send>) {
        self.soc.usb.storage=Some(emu_core::usb::Storage::new(disk));
        use emu_core::usb::DevId;
        self.soc.xhci.attach(&[DevId::Keyboard,DevId::Tablet,DevId::Storage]);
    }
    /// Fit an NVMe SSD backed by `disk` in the M.2 slot.
    pub fn attach_nvme(&mut self, disk: Box<dyn emu_core::disk::Disk + Send>) { self.soc.pcie.attach_nvme(nvme::Nvme::new(Some(disk))); }
    pub fn serial_input(&mut self,b:u8)->bool {self.soc.dirty=true;self.soc.uart.input(b)}

    fn ns_of_count(c: u64) -> u64 { ((c as u128*1_000_000_000).div_ceil(CNTFRQ as u128)).min(u64::MAX as u128) as u64 }
    /// Move core `k`'s clock to `t` if it is behind (a sleeping core's time follows the machine).
    fn catch_up(&mut self, k: usize, t: u64) {
        let c=&mut self.cpus[k];
        if c.now_ns()<t { c.sys.time_base_ns=t; c.sys.time_base_instret=c.instret; }
    }
    /// Make core `k` the running one: it takes the shared JIT; the core that stops running
    /// loses its exclusive reservation.
    fn switch_to(&mut self, k: usize) {
        if k!=self.cur {
            let (a,b)=(self.cur,k);
            let (lo,hi)=self.cpus.split_at_mut(a.max(b));
            let (pa,pb)=if a<b {(&mut lo[a],&mut hi[0])} else {(&mut hi[0],&mut lo[b])};
            pa.hand_jit_to(pb);
            pa.clear_exclusive();
            self.cur=k;
        }
        self.soc.gic.cur=k;
        self.next_check=0;
    }
    fn timer_deadlines(cpu: &Cpu) -> impl Iterator<Item=u64> + '_ {
        let s=&cpu.sys;
        [(s.ptimer,0),(s.vtimer,s.cntvoff)].into_iter().filter(|(t,_)|t.ctl&3==1).map(|(t,off)|Self::ns_of_count(t.cval.wrapping_add(off)))
    }
    /// The earliest deadline after `t` that core `c` must notice: its own timers, those of
    /// sleeping cores and the network poll (cores waiting for their turn check their own).
    fn deadline_after(&self, t: u64, c: usize) -> Option<u64> {
        let mut d=[self.soc.net.as_ref().map(|_|self.soc.next_net_poll),self.soc.xhci.next_event(&self.soc.usb),self.soc.audio.next_event(),self.soc.vop2.next_event()].into_iter().flatten().filter(|&x|x>t).min();
        for (k,cpu) in self.cpus.iter().enumerate() {
            if self.state[k]==CoreState::Off || (k!=c && self.state[k]==CoreState::Run) {continue;}
            for ns in Self::timer_deadlines(cpu) { if ns>t { d=Some(d.map_or(ns,|x|x.min(ns))); } }
        }
        d
    }
    /// The next timer deadline of any core that is on.
    fn next_deadline(&self) -> Option<u64> {
        let dev=[self.soc.xhci.next_event(&self.soc.usb),self.soc.audio.next_event(),self.soc.vop2.next_event()];
        self.cpus.iter().enumerate().filter(|(k,_)|self.state[*k]!=CoreState::Off).flat_map(|(_,c)|Self::timer_deadlines(c)).chain(dev.into_iter().flatten())
            // an expired timer whose interrupt is off wakes nobody: it must not stop time
            .filter(|&t|t>self.now).min()
    }
    fn devices(&mut self) { self.soc.service(self.now); self.soc.device_lines(); }
    fn update(&mut self) {
        let c=self.cur;
        let t=self.cpus[c].now_ns();
        if t>self.now {self.now=t;}
        let now=self.now;
        for k in 0..self.cpus.len() {
            if self.state[k]==CoreState::Off {continue;}
            if k!=c && self.state[k]!=CoreState::Run {self.catch_up(k,now);}
            let cpu=&mut self.cpus[k];
            cpu.refresh_counter();
            cpu.sys.timers_changed=false;
            let s=&cpu.sys;
            let (p,v)=(s.ptimer.firing(s.cnt),s.vtimer.firing(s.cnt.wrapping_sub(s.cntvoff)));
            self.soc.gic.set_ppi(k,30,p);
            self.soc.gic.set_ppi(k,27,v);
        }
        self.devices();
        for k in 0..self.cpus.len() {
            if self.state[k]==CoreState::Off {continue;}
            let irq=self.soc.gic.irq_out(k);
            self.cpus[k].set_irq(irq,false);
            if irq && matches!(self.state[k],CoreState::Wfi|CoreState::Wfe) {
                self.state[k]=CoreState::Run;
                self.round_end=self.round_end.min(t+quantum());
            }
        }
        self.soc.dirty=false;
        let horizon=t+1_000_000;
        let due=self.deadline_after(t,c).unwrap_or(horizon).min(horizon);
        self.next_check=self.cpus[c].instret+(due-t);
    }
    /// Time moves to `t` while every core sleeps (the host waited or skipped ahead).
    pub fn idle_until(&mut self,t:u64) {
        if t>self.now {self.now=t;}
        let now=self.now;
        for k in 0..self.cpus.len() { if self.state[k]!=CoreState::Off {self.catch_up(k,now);} }
        self.update();
    }
    /// Run until the cores have retired `budget` instructions between them. The cores take
    /// turns in rounds of 20 µs of virtual time (`SBC_QUANTUM` ns), all starting the round at
    /// the same time; a core that is alone runs without rounds until something wakes another.
    pub fn run(&mut self,budget:u64)->Stop {
        #[cfg(not(target_arch = "wasm32"))]
        if self.multi && !self.debugging() { return self.run_threaded(std::time::Duration::from_millis(10)); }
        if self.cpus[0].mt { for c in self.cpus.iter_mut() { c.mt=false; } }
        let end=self.instret().saturating_add(budget);
        let mut total=self.instret();
        let mut steps=budget;
        let mut storm=0u32;
        let stop='run: loop {
            if total>=end || steps==0 {break Stop::Budget;}
            self.update();
            if self.state.contains(&CoreState::Run) { for s in self.state.iter_mut() { if *s==CoreState::Wfe {*s=CoreState::Run;} } }
            let runnable=(0..self.cpus.len()).filter(|&k|self.state[k]==CoreState::Run && self.breakpoints.may_run(k)).count();
            if runnable==0 {
                let poll=self.soc.net.as_ref().is_some_and(|n|n.nat.is_some()) || !self.soc.gmac.rx_queue.is_empty();
                let mut until=self.next_deadline().unwrap_or(self.now+10_000_000);
                if poll {until=until.min(self.soc.next_net_poll);}
                break Stop::Idle(until.max(self.now));
            }
            let round_start=self.now;
            self.round_end=if runnable==1 {u64::MAX} else {round_start+quantum()};
            let (last, n) = (self.breakpoints.last_turn(self.cur), self.cpus.len());
            let watching = !self.cpus[0].watch.is_empty();
            for i in 0..n {
                let k = self.breakpoints.turn(i, last, n, watching);
                if self.state[k]!=CoreState::Run || !self.breakpoints.may_run(k) {continue;}
                if let Some(stop)=self.slice(k,round_start,&mut total,end,&mut steps,&mut storm) {break 'run stop;}
                if total>=end || steps==0 {break;}
            }
        };
        let t=self.cpus[self.cur].now_ns();
        if t>self.now {self.now=t;}
        stop
    }
    /// Run core `k` from `round_start` until the round ends, it sleeps, or the budget is used.
    fn slice(&mut self, k: usize, round_start: u64, total: &mut u64, end: u64, steps: &mut u64, storm: &mut u32) -> Option<Stop> {
        self.switch_to(k);
        self.catch_up(k,round_start);
        // what a debugger has set only changes between runs
        let (breaks,watches)=(!self.breakpoints.at.is_empty(),!self.cpus[k].watch.is_empty());
        loop {
            let t=self.cpus[k].now_ns();
            if t>=self.round_end || *total>=end || *steps==0 {return None;}
            if breaks && self.breakpoints.hit(k,self.cpus[k].pc) {return Some(Stop::Breakpoint(k));}
            *steps-=1;
            if self.soc.dirty || self.cpus[k].sys.timers_changed || self.cpus[k].instret>=self.next_check {self.update();}
            let cpu=&mut self.cpus[k];
            let i0=cpu.instret;
            cpu.chain_until=i0.saturating_add(self.round_end-t).min(self.next_check).min(i0+(end-*total));
            let r=cpu.step(&mut self.soc);
            // GPU work runs inside the doorbell write: the core waited for it.
            let gpu_ns=self.soc.gpu.take_cost();
            if gpu_ns>0 { self.cpus[k].sys.time_base_ns+=gpu_ns; }
            *total+=self.cpus[k].instret-i0;
            if watches { if let Some((a,kind))=self.cpus[k].watch_hit.take() { if r.is_ok() {return Some(Stop::Watchpoint(k,a,kind));} } }
            if let Some(op)=self.cpus[k].tlbi_bcast.take() {
                for (j,o) in self.cpus.iter_mut().enumerate() { if j!=k {o.tlb_invalidate(op);} }
            }
            if let Some(op)=self.cpus[k].ic_bcast.take() {
                for (j,o) in self.cpus.iter_mut().enumerate() { if j!=k {o.jit_invalidate(op);} }
            }
            match r {
                Ok(())=>*storm=0,
                Err(Trap::Exception(..))=>{
                    self.exceptions+=1; *storm+=1;
                    if *storm>=1_000_000 {let c=&self.cpus[k];let e=c.el as usize;return Some(Stop::ExceptionStorm{pc:c.pc,esr:c.esr[e],elr:c.elr[e],far:c.far[e],el:c.el});}
                }
                Err(Trap::Interrupt(..))=>{self.irqs+=1;self.soc.dirty=true;}
                Err(Trap::Wait)=>{
                    self.update();
                    if self.cpus[k].irq {continue;}
                    let others=(0..self.cpus.len()).any(|j|j!=k && self.state[j]==CoreState::Run);
                    if self.cpus[k].last_wfe && others {
                        // waiting for another core (a lock, a flag): give the rest of the round away
                        if self.round_end!=u64::MAX {let r=self.round_end;self.catch_up(k,r);}
                        return None;
                    }
                    self.state[k]=if self.cpus[k].last_wfe {CoreState::Wfe} else {CoreState::Wfi};
                    return None;
                }
                Err(Trap::HostCall(_))=>{
                    if let Some(s)=self.psci(k) {return Some(s);}
                    if self.state[k]!=CoreState::Run {return None;}
                }
                Err(Trap::Unimplemented(pc,word))=>return Some(Stop::Unimplemented(pc,word)),
                Err(Trap::Brk(n))=>return Some(Stop::Brk(n)),
                Err(Trap::Hlt(n))=>return Some(Stop::Hlt(n)),
            }
        }
    }
    /// Whether a debugger has breakpoints or watchpoints set (the cores then take turns,
    /// interpreted).
    pub fn debugging(&self) -> bool { !self.breakpoints.at.is_empty() || !self.cpus[0].watch.is_empty() || self.cpus.iter().any(|c|c.jit_off) }
    fn set_debugging(&mut self) {
        let on=!self.breakpoints.at.is_empty() || !self.cpus[0].watch.is_empty();
        for c in self.cpus.iter_mut() {c.jit_off=on;}
    }
    pub fn debug_breakpoint(&mut self, addr: u64, insert: bool) { self.breakpoints.set(addr,insert); self.set_debugging(); }
    /// Watch [addr, addr + len) for accesses of `kind` (1 write, 2 read, 3 either) on every core.
    pub fn debug_watch(&mut self, addr: u64, len: u64, kind: u8, insert: bool) {
        for c in self.cpus.iter_mut() { c.watch.retain(|&w|w!=(addr,len,kind)); if insert {c.watch.push((addr,len,kind));} }
        self.set_debugging();
    }
    /// Execute one instruction on core `k` (interpreter; a sleeping core is woken); a stop it
    /// causes, if any. `run` does it, giving the other cores no turn meanwhile.
    #[cold]
    pub fn debug_step(&mut self, k: usize) -> Option<Stop> {
        if self.state[k]==CoreState::Off {return None;}
        self.state[k]=CoreState::Run;
        let jit_off=std::mem::replace(&mut self.cpus[k].jit_off,true);
        let turn = self.breakpoints.last_turn(self.cur);
        self.breakpoints.begin_step(k,self.cpus[k].pc);
        let stop=self.run(1);
        self.breakpoints.end_step(k);
        self.breakpoints.keep_turn(turn);
        self.cpus[k].jit_off=jit_off;
        (!matches!(stop,Stop::Budget|Stop::Idle(_))).then_some(stop)
    }
    /// Guest RAM at virtual address `va` as core `k` sees it.
    pub fn debug_read(&mut self, k: usize, va: u64, len: usize) -> Option<Vec<u8>> {
        let mut out=Vec::with_capacity(len);
        while out.len()<len {
            let a=va.wrapping_add(out.len() as u64);
            let pa=self.cpus[k].debug_translate(&mut self.soc,a)? as usize;
            let n=(len-out.len()).min(0x1000-(a & 0xfff) as usize);
            out.extend_from_slice(self.soc.ram.get(pa..pa.checked_add(n)?)?);
        }
        Some(out)
    }
    /// Write guest RAM at virtual address `va`; compiled code is dropped.
    pub fn debug_write(&mut self, k: usize, va: u64, data: &[u8]) -> bool {
        let mut done=0;
        while done<data.len() {
            let a=va.wrapping_add(done as u64);
            let Some(pa)=self.cpus[k].debug_translate(&mut self.soc,a) else {return false};
            let n=(data.len()-done).min(0x1000-(a & 0xfff) as usize);
            let Some(dst)=(pa as usize).checked_add(n).and_then(|e|self.soc.ram.get_mut(pa as usize..e)) else {return false};
            dst.copy_from_slice(&data[done..done+n]);
            done+=n;
        }
        for c in self.cpus.iter_mut() { c.jit_invalidate(u64::MAX); } // the 32-bit core's code too
        true
    }
    /// The core with MPIDR affinity `mpidr` (Aff1 = core, Aff0 = 0).
    fn target(&self, mpidr: u64) -> Option<usize> {
        let a=mpidr & 0xff_00ff_ffff;
        (0..self.cpus.len()).find(|&k|gic::affinity(k)==a)
    }
    /// PSCI 1.1, as Trusted Firmware-A provides it on the RK3588, called by core `k`.
    fn psci(&mut self, k: usize) -> Option<Stop> {
        const NOT_SUPPORTED: u64 = -1i64 as u64;
        const INVALID_PARAMETERS: u64 = -2i64 as u64;
        const ALREADY_ON: u64 = -4i64 as u64;
        let x=self.cpus[k].x;
        let supported=|f:u32|matches!(f,0x8000_0000|0x8400_0000|0x8400_0002|0x8400_0003|0xc400_0003|0x8400_0004|0xc400_0004|0x8400_0006|0x8400_0008|0x8400_0009|0x8400_000a);
        let ret=match x[0] as u32 {
            0x8400_0000=>0x10001,
            0x8400_0002=>{ self.state[k]=CoreState::Off; self.cpus[k].set_irq(false,false); return None; }
            0x8400_0003|0xc400_0003=>match self.target(x[1]) {
                None=>INVALID_PARAMETERS,
                Some(t) if self.state[t]!=CoreState::Off=>ALREADY_ON,
                Some(t)=>{self.cpu_on(k,t,x[2],x[3]);0}
            },
            0x8400_0004|0xc400_0004=>match self.target(x[1]) { None=>INVALID_PARAMETERS, Some(t)=>(self.state[t]==CoreState::Off) as u64 },
            0x8400_0006=>2,
            0x8400_0008=>return Some(Stop::PowerOff),
            0x8400_0009=>return Some(Stop::Reset),
            0x8400_000a=>if supported(x[1] as u32){0}else{NOT_SUPPORTED},
            // SMCCC_VERSION 1.1, other SMCCC services unsupported.
            0x8000_0000=>0x10001,
            scmi::SMC_ID=>{ self.soc.scmi_call(); 0 },
            _=>NOT_SUPPORTED,
        };
        self.cpus[k].x[0]=ret; None
    }
    /// Start core `t` at `entry` (MMU off, interrupts masked, x0 = `context`) in the caller's
    /// EL. RAM, the retired count and the JIT are kept.
    fn cpu_on(&mut self, caller: usize, t: usize, entry: u64, context: u64) {
        let (el,now)=(self.cpus[caller].el,self.cpus[caller].now_ns());
        Self::reset_core(&mut self.cpus[t],t,el,entry,context,now);
        self.state[t]=CoreState::Run;
        self.round_end=self.round_end.min(now+quantum());
        self.soc.dirty=true;
    }
    /// Core `k` out of reset at `entry` (PSCI CPU_ON: MMU off, interrupts masked, x0 =
    /// `context`, in the caller's EL `el`): RAM, the retired count, the JIT and the shared
    /// compiled-pages map are kept.
    pub(crate) fn reset_core(c: &mut Cpu, k: usize, el: u8, entry: u64, context: u64, now: u64) {
        let mut f=Cpu::new();
        f.profile=core_profile(k);
        f.sys.cntfrq=CNTFRQ;
        f.sys.mpidr=core_mpidr(k);
        f.firmware_calls_to_host=true;
        f.ram=c.ram; f.ram_len=c.ram_len;
        f.code_pages=c.code_pages.clone();
        f.instret=c.instret;
        f.sys.time_base_ns=now; f.sys.time_base_instret=c.instret;
        f.el=el; f.spsel=true; f.daif=0xf<<6; f.pc=entry; f.x[0]=context;
        f.sys.cpacr_el1=3<<20;
        f.jit_off=c.jit_off; f.watch=c.watch.clone(); f.crypto=c.crypto; f.mt=c.mt; f.wall_clock=c.wall_clock;
        let mut old=std::mem::replace(c,f);
        old.hand_jit_to(c);
    }
}
