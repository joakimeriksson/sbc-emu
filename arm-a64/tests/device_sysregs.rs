//! Board-owned GIC sysregs must behave identically through interpreter and JIT fallback.
use arm_a64::{Cpu, CpuProfile, sysreg::key};
use emu_core::{Bus, Core, Fault, FlatRam, Trap};

struct DeviceBus { ram:FlatRam, reads:usize, writes:usize }
macro_rules! access {($r:ident,$w:ident,$ty:ty)=>{
    fn $r(&mut self,a:u64)->Result<$ty,Fault>{self.ram.$r(a)}
    fn $w(&mut self,a:u64,v:$ty)->Result<(),Fault>{self.ram.$w(a,v)}
}}
impl Bus for DeviceBus {
    access!(read8,write8,u8);access!(read16,write16,u16);access!(read32,write32,u32);access!(read64,write64,u64);
    fn read_sysreg(&mut self,k:u32)->Option<u64>{if k==key(3,0,12,12,0){self.reads+=1;Some(27)}else{None}}
    fn write_sysreg(&mut self,k:u32,v:u64)->bool {if k==key(3,0,12,12,1){assert_eq!(v,27);self.writes+=1;true}else{false}}
}
#[test]
fn board_sysregs_execute_once_even_with_jit_verification() {
    for jit in [false,true] {
        let mut bus=DeviceBus{ram:FlatRam::new(0,4096),reads:0,writes:0};
        let mrs=|k:u32,rt:u32|0xd5200000u32|(k<<5)|rt;
        let msr=|k:u32,rt:u32|0xd5000000u32|(k<<5)|rt;
        let code=[mrs(key(3,0,0,0,0),1),mrs(key(3,0,0,4,0),2),0xd280fa03,
            mrs(key(3,0,12,12,0),0),msr(key(3,0,12,12,1),0),0xf1000463,0x54ffffa1,0xd4200000];
        for (n,i) in code.iter().enumerate(){bus.write32(n as u64*4,*i).unwrap();}
        let mut cpu=Cpu::new();cpu.profile=CpuProfile::CORTEX_A55;cpu.brk_to_host=true;
        cpu.ram=bus.ram.mem.as_mut_ptr();cpu.ram_len=4096;
        if jit {cpu.enable_jit();if let Some(j)=cpu.jit.as_mut(){j.verify=true;}}
        let mut done=false;
        for _ in 0..20000 {cpu.chain_until=cpu.instret+100;match cpu.step(&mut bus){
            Ok(())=>{},Err(Trap::Brk(0))=>{done=true;break;},other=>panic!("unexpected {other:?}")}}
        assert!(done);assert_eq!(cpu.x[1],CpuProfile::CORTEX_A55.midr);assert_eq!((cpu.x[2]>>24)&15,1);
        assert_eq!(bus.reads,2000);assert_eq!(bus.writes,2000);
        if let Some(j)=cpu.jit.as_ref(){assert!(j.compiled>0);assert!(j.report.is_none(),"{:?}",j.report);}
    }
}

#[test]
fn pi_identity_and_sysreg_feature_remain_unchanged() {
    let mut cpu=Cpu::new();let mut ram=FlatRam::new(0,64);cpu.brk_to_host=true;
    ram.write32(0,0xd5380000).unwrap(); // MRS X0,MIDR_EL1
    ram.write32(4,0xd5380401).unwrap(); // MRS X1,ID_AA64PFR0_EL1
    cpu.step(&mut ram).unwrap();cpu.step(&mut ram).unwrap();
    assert_eq!(cpu.x[0],0x410fd083);assert_eq!(cpu.x[1],0x1112); // EL0: AArch64 and AArch32
}
