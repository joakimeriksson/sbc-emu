//! Minimal RK3588S / Orange Pi 5: one reduced Cortex-A55, GICv3, UART2 and RAM.
//! Direct Linux boot, generated reduced DT, firmware PSCI services handled by host.
pub mod boot;
pub mod gic;
pub mod gpu_mmu;
pub mod gpu_cs;
pub mod gpu;
pub mod uart;
pub mod mmc;
pub mod phy;
pub mod gmac;

use arm_a64::{Cpu, CpuProfile};
use emu_core::{Bus, Core, Fault, Stop, Trap};

pub const GMAC: u64 = 0xfe1c_0000;
pub const MMC: u64 = 0xfe2c_0000;
pub const UART2: u64 = 0xfeb5_0000;
pub const GICD: u64 = 0xfe60_0000;
pub const GICR: u64 = 0xfe68_0000;
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
        else if (MMC..MMC+0x4000).contains(&addr) { self.mmc.read((addr-MMC)&!3) }
        else if (UART2..UART2+0x100).contains(&addr) { self.uart.read((addr-UART2)&!3) }
        else if (GICD..GICD+0x10000).contains(&addr) {self.gic.dist_read((addr-GICD)&!3)}
        else if (GICR..GICR+0x20000).contains(&addr) {self.gic.redist_read((addr-GICR)&!3)}
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
            let off = if (GICD..GICD+0x10000).contains(&addr) { addr-GICD } else { addr.saturating_sub(GICR+0x10000) };
            let is_write_one = (0x100..0x400).contains(&off);
            let old=if (GMAC..GMAC+0x10000).contains(&addr) {if (addr-GMAC)&!3==0x1160 {0}else{self.gmac.read((addr-GMAC)&!3)}} else if is_write_one { 0 } else if (GICD..GICD+0x10000).contains(&addr) {self.gic.dist_read((addr-GICD)&!3)}
                else if (GICR..GICR+0x20000).contains(&addr) {self.gic.redist_read((addr-GICR)&!3)} else {0};
            (old & !mask) | ((value as u32)<<shift)
        } else {value as u32};
        if (0xfb000000..0xfb200000).contains(&addr) {self.gpu.write((addr-0xfb000000)&!3,v);}
        else if (GMAC..GMAC+0x10000).contains(&addr) {self.gmac.write((addr-GMAC)&!3,v);}
        else if (MMC..MMC+0x4000).contains(&addr) {self.mmc.write((addr-MMC)&!3,v);}
        else if (UART2..UART2+0x100).contains(&addr) {self.uart.write((addr-UART2)&!3,v);}
        else if (GICD..GICD+0x10000).contains(&addr) {self.gic.dist_write((addr-GICD)&!3,v);}
        else if (GICR..GICR+0x20000).contains(&addr) {self.gic.redist_write((addr-GICR)&!3,v);}
        else { if !self.unknown.contains(&addr) {self.unknown.push(addr);} return Err(Fault::Unmapped); }
        Ok(())
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

pub struct Machine {
    pub cpu: Cpu,
    pub gpu_probe: bool,
    pub soc: Soc,
    waiting: bool,
    next_check: u64,
    next_net_poll: u64,
    pub irqs: u64,
    pub exceptions: u64,
}
impl Machine {
    pub fn new(ram_bytes: usize) -> Result<Self,String> {
        if !(128<<20..=2usize<<30).contains(&ram_bytes) {return Err("Orange Pi bring-up supports 128..2048 MiB RAM".into());}
        let mut soc=Soc {ram:vec![0;ram_bytes], uart:Default::default(), mmc:Default::default(), gpu:Default::default(), gmac:Default::default(), net:None, gic:Default::default(),dirty:true,unknown:Vec::new()};
        let mut cpu=Cpu::new();
        cpu.profile=CpuProfile::CORTEX_A55;
        cpu.sys.cntfrq=CNTFRQ;
        cpu.sys.mpidr=0;
        cpu.ram=soc.ram.as_mut_ptr(); cpu.ram_len=ram_bytes as u64;
        cpu.firmware_calls_to_host=true;
        Ok(Self{cpu,soc,gpu_probe:false,waiting:false,next_check:0,next_net_poll:0,irqs:0,exceptions:0})
    }
    pub fn connect_network(&mut self, nat: bool, log: bool) {
        let mut net = sbc_net::net::VirtualNet::new(log);
        #[cfg(not(target_arch = "wasm32"))]
        if nat { net.nat = Some(sbc_net::nat::Nat::new(log)); }
        let _ = nat;
        self.soc.gmac.phy.set_link(true);
        self.soc.net = Some(net);
    }
    pub fn now(&self)->u64 {self.cpu.now_ns()}
    pub fn serial_input(&mut self,b:u8)->bool {self.soc.dirty=true;self.soc.uart.input(b)}
    fn deadline(&self)->u64 {
        let now=self.now();
        [(self.cpu.sys.ptimer,0),(self.cpu.sys.vtimer,self.cpu.sys.cntvoff)]
            .into_iter().filter(|(t,_)|t.ctl&3==1)
            .map(|(t,off)| ((t.cval.wrapping_add(off) as u128*1_000_000_000).div_ceil(CNTFRQ as u128)).min(u64::MAX as u128) as u64)
            .filter(|&t|t>now).min().unwrap_or(now.saturating_add(10_000_000))
    }
    fn update(&mut self) {
        self.soc.gmac.service(&mut self.soc.ram);
        if let Some(net) = &mut self.soc.net {
            let now_us = self.cpu.now_ns() / 1000;
            for frame in std::mem::take(&mut self.soc.gmac.tx_out) {
                let replies=net.handle(&frame, now_us);
                if self.soc.gmac.receiving() {sbc_net::net::enqueue_bounded(&mut self.soc.gmac.rx_queue,replies); }
            }
            if self.cpu.now_ns() >= self.next_net_poll {
                let replies=net.poll(now_us);
                if self.soc.gmac.receiving() {sbc_net::net::enqueue_bounded(&mut self.soc.gmac.rx_queue,replies); }
                self.next_net_poll = self.cpu.now_ns().saturating_add(1_000_000);
            }
            self.soc.gmac.service(&mut self.soc.ram);
        } else { self.soc.gmac.tx_out.clear(); }
        self.cpu.refresh_counter();
        let s=&self.cpu.sys;
        self.soc.gic.set_line(30,s.ptimer.firing(s.cnt));
        self.soc.gic.set_line(27,s.vtimer.firing(s.cnt.wrapping_sub(s.cntvoff)));
        self.soc.gic.set_line(32+234,self.soc.gmac.irq());
        self.soc.gic.set_line(UART_IRQ,self.soc.uart.irq());
        self.soc.gic.set_line(32+203,self.soc.mmc.irq());
        for (i,spi) in [92,93,94].into_iter().enumerate() { self.soc.gic.set_line(32+spi,self.soc.gpu.irq(i)); }
        let irq=self.soc.gic.irq(); self.cpu.set_irq(irq,false);
        if irq {self.waiting=false;}
        self.soc.dirty=false; self.cpu.sys.timers_changed=false;
        self.next_check=self.deadline().min(self.now().saturating_add(1_000_000));
    }
    pub fn idle_until(&mut self,t:u64) {
        if t>self.now() {self.cpu.sys.time_base_ns=t;self.cpu.sys.time_base_instret=self.cpu.instret;}
        self.update();
    }
    pub fn run(&mut self,budget:u64)->Stop {
        let end=self.cpu.instret.saturating_add(budget);
        let mut storm=0;
        for _ in 0..budget {
            if self.cpu.instret>=end {return Stop::Budget;}
            if self.soc.dirty || self.cpu.sys.timers_changed || self.now()>=self.next_check {self.update();}
            if self.waiting {
                let poll=self.soc.net.as_ref().is_some_and(|n|n.nat.is_some()) || !self.soc.gmac.rx_queue.is_empty();
                return Stop::Idle(if poll {self.deadline().min(self.next_check)}else{self.deadline()});
            }
            self.cpu.chain_until=self.cpu.instret.saturating_add(self.next_check.saturating_sub(self.now())).min(end);
            match self.cpu.step(&mut self.soc) {
                Ok(())=>storm=0,
                Err(Trap::Wait)=>self.waiting=true,
                Err(Trap::HostCall(_))=>{if let Some(s)=self.psci(){return s;}},
                Err(Trap::Interrupt(..))=>{self.irqs+=1;storm=0;},
                Err(Trap::Exception(..))=>{self.exceptions+=1;storm+=1; if storm>10000 {let c=&self.cpu;let e=c.el as usize;return Stop::ExceptionStorm{pc:c.pc,esr:c.esr[e],elr:c.elr[e],far:c.far[e],el:c.el};}},
                Err(Trap::Unimplemented(pc,word))=>return Stop::Unimplemented(pc,word),
                Err(Trap::Brk(n))=>return Stop::Brk(n),
                Err(Trap::Hlt(n))=>return Stop::Hlt(n),
            }
            // The single PE already applied its own cache/TLB maintenance.
            self.cpu.tlbi_bcast=None; self.cpu.ic_bcast=None;
        }
        Stop::Budget
    }
    fn psci(&mut self)->Option<Stop> {
        let x=self.cpu.x;
        let supported=|f:u32|matches!(f,0x8400_0000|0x8400_0003|0xc400_0003|0x8400_0004|0xc400_0004|0x8400_0006|0x8400_0008|0x8400_0009|0x8400_000a);
        let ret=match x[0] as u32 {
            0x8400_0000=>0x10001,
            0x8400_0003|0xc400_0003=>if x[1]==0 {-4i64 as u64}else{-2i64 as u64},
            0x8400_0004|0xc400_0004=>if x[1]==0 && x[2]<=3 {0}else{-2i64 as u64},
            0x8400_0006=>2,
            0x8400_0008=>return Some(Stop::PowerOff),
            0x8400_0009=>return Some(Stop::Reset),
            0x8400_000a=>if supported(x[1] as u32){0}else{u64::MAX},
            // SMCCC_VERSION 1.1, other SMCCC services unsupported.
            0x8000_0000=>0x10001,
            _=>u64::MAX,
        };
        self.cpu.x[0]=ret; None
    }
}
