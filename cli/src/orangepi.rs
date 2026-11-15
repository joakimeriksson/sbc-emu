//! Native serial-only RK3588S bring-up adapter. Unsupported Pi options fail explicitly.
use crate::{arg, serial_board::{self, RunConfig, SerialBoard}};
use sbc_machine::Machine as HostMachine;
use std::process::ExitCode;

// Reboots reconstruct the SoC while preserving the same locked image and COW overlay.
struct SharedDisk(std::rc::Rc<std::cell::RefCell<crate::disk::CowFile>>);
impl emu_core::disk::Disk for SharedDisk {
    fn sectors(&self)->u64 { self.0.borrow().sectors() }
    fn read(&mut self,lba:u64,out:&mut [u8]) { self.0.borrow_mut().read(lba,out); }
    fn write(&mut self,lba:u64,data:&[u8]) { self.0.borrow_mut().write(lba,data); }
    fn overlay(&self)->Vec<(u64,Vec<u8>)> { self.0.borrow().overlay() }
    fn restore_overlay(&mut self,sectors:Vec<(u64,Vec<u8>)>) { self.0.borrow_mut().restore_overlay(sectors); }
    fn snapshots_disk(&self)->bool { self.0.borrow().snapshots_disk() }
}

impl SerialBoard for rk3588::Machine {
    fn host(&mut self) -> HostMachine<'_> { HostMachine::OrangePi5(self) }
    fn now(&self) -> u64 { rk3588::Machine::now(self) }
    fn instret(&self) -> u64 { self.cpu.instret }
    fn report(&self, tag: &str) {
        let m = self;
        eprintln!("[{tag}] pc={:#x} insns={} virtual={:.3}s irqs={} acknowledgements={} exceptions={}",m.cpu.pc,m.cpu.instret,m.now() as f64/1e9,m.irqs,m.soc.gic.acknowledges,m.exceptions);
        if !m.soc.unknown.is_empty(){eprintln!("[{tag}] unmapped MMIO: {:x?}",m.soc.unknown);}
        if !m.cpu.sys.unknown.is_empty(){eprintln!("[{tag}] unmodelled system registers: {:x?}",m.cpu.sys.unknown);}
    }
}

pub(crate) fn run(args:&[String])->ExitCode {
    match execute(args) {Ok(())=>ExitCode::SUCCESS,Err(e)=>{eprintln!("[orangepi5] {e}");ExitCode::FAILURE}}
}
fn execute(args:&[String])->Result<(),String> {
    serial_board::validate_options(args, &["--threads", "--dump-dtb", "--sd"])?;
    if arg(args,"--threads").is_some_and(|s|s!="single") {return Err("Orange Pi currently uses deterministic single-thread execution".into());}
    let kernel=std::fs::read(arg(args,"--kernel").ok_or("--kernel Image is required")?).map_err(|e|e.to_string())?;
    let initrd=arg(args,"--initrd").map(std::fs::read).transpose().map_err(|e|e.to_string())?;
    let cmdline=arg(args,"--append").unwrap_or(rk3588::boot::DEFAULT_CMDLINE);
    let ram=serial_board::ram_bytes(args, 256)?;
    let config=RunConfig::from_args("orangepi5", args)?;
    let disk=arg(args,"--sd").map(|p|crate::disk::CowFile::open(p)
        .map(|d|std::rc::Rc::new(std::cell::RefCell::new(d)))
        .map_err(|e|format!("{p}: {e}"))).transpose()?;
    if let Some(d)=disk.as_ref() {
        let sectors=emu_core::disk::Disk::sectors(&*d.borrow());
        if sectors < 1024 || sectors % 1024 != 0 || sectors > (1u64 << 32) {
            return Err("SDHC image size must be a multiple of 512 KiB between 512 KiB and 2 TiB".into());
        }
    }
    let make_machine=||->Result<rk3588::Machine,String>{
        let mut m=rk3588::Machine::new(ram)?;
        m.soc.mmc.card.disk=disk.as_ref().map(|d|Box::new(SharedDisk(d.clone())) as Box<dyn emu_core::disk::Disk>);
        rk3588::boot::boot_linux(&mut m,&kernel,initrd.as_deref(),cmdline)?;
        if !args.iter().any(|a|a=="--no-jit") {m.cpu.enable_jit();}
        Ok(m)
    };
    if let Some(path)=arg(args,"--dump-dtb") {
        std::fs::write(path,rk3588::boot::device_tree(ram as u64,cmdline,initrd.as_ref().map(|d|(rk3588::boot::INITRD,rk3588::boot::INITRD+d.len() as u64)),disk.is_some())).map_err(|e|e.to_string())?;
    }
    eprintln!("[orangepi5] experimental RK3588S: one reduced Cortex-A55, GICv3, UART2; direct kernel boot, reduced DT");
    serial_board::run(config, make_machine)
}
