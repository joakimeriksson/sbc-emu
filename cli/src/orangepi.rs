//! Native serial-only RK3588S bring-up adapter. Unsupported Pi options fail explicitly.
use crate::{arg, serial_board::{self, RunConfig, SerialBoard}};
use sbc_machine::Machine as HostMachine;
use std::process::ExitCode;

// Reboots reconstruct the SoC while preserving the same locked image and COW overlay.
use crate::disk::SharedDisk;

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
    serial_board::validate_options(args, &["--threads", "--dump-dtb", "--sd", "--gpu"])?;
    if arg(args,"--threads").is_some_and(|s|s!="single") {return Err("Orange Pi currently uses deterministic single-thread execution".into());}
    let gpu=arg(args,"--gpu").unwrap_or("none");
    if !["none","probe"].contains(&gpu) {return Err("--gpu accepts none or probe (experimental, no rendering)".into());}
    let kernel=arg(args,"--kernel").map(std::fs::read).transpose().map_err(|e|e.to_string())?;
    if kernel.is_none() && arg(args,"--sd").is_none() { return Err("provide --kernel Image or --sd IMAGE".into()); }
    if kernel.is_none() && arg(args,"--initrd").is_some() { return Err("--initrd requires --kernel".into()); }
    let initrd=arg(args,"--initrd").map(std::fs::read).transpose().map_err(|e|e.to_string())?;
    let cmdline=arg(args,"--append").unwrap_or(rk3588::boot::DEFAULT_CMDLINE);
    let ram=serial_board::ram_bytes(args, if kernel.is_some() {256} else {1024})?;
    let config=RunConfig::from_args("orangepi5", args)?;
    let persist=args.iter().any(|a|a=="--sd-persist");
    let disk=arg(args,"--sd").map(|p|SharedDisk::open(p,persist).map_err(|e|format!("{p}: {e}"))).transpose()?;
    let make_machine=||->Result<rk3588::Machine,String>{
        let mut m=rk3588::Machine::new(ram)?;
        m.gpu_probe=gpu=="probe";
        m.soc.mmc.card.disk=disk.clone().map(|d|Box::new(d) as Box<dyn emu_core::disk::Disk>);
        if let Some(kernel)=kernel.as_ref() {
            rk3588::boot::boot_linux(&mut m,kernel,initrd.as_deref(),cmdline)?;
        } else {
            let card=rk3588::boot::read_card(m.soc.mmc.card.disk.as_mut().ok_or("no SD image")?.as_mut())?;
            for note in &card.notes {eprintln!("[orangepi5 firmware] {note}");}
            let bootargs=image_cmdline(&card.cmdline,arg(args,"--append"));
            rk3588::boot::boot_linux(&mut m,&card.kernel,card.initrd.as_deref(),&bootargs)?;
        }
        if !args.iter().any(|a|a=="--no-jit") {m.cpu.enable_jit();}
        Ok(m)
    };
    if kernel.is_none() && arg(args,"--dump-dtb").is_some() {return Err("--dump-dtb is not yet supported for SD image boot".into());}
    if let Some(path)=arg(args,"--dump-dtb") {
        std::fs::write(path,rk3588::boot::device_tree_gpu(ram as u64,cmdline,initrd.as_ref().map(|d|(rk3588::boot::INITRD,rk3588::boot::INITRD+d.len() as u64)),disk.is_some(),gpu=="probe")).map_err(|e|e.to_string())?;
    }
    eprintln!("[orangepi5] experimental RK3588S: one reduced Cortex-A55, GICv3, UART2; emulated firmware, reduced DT");
    serial_board::run(config, make_machine)
}

/// For SD image boots keep the image's root and console settings and append extra options.
fn image_cmdline(base: &str, extra: Option<&str>) -> String {
    match extra.map(str::trim).filter(|s| !s.is_empty()) {
        Some(extra) => format!("{base} {extra}"),
        None => base.to_string(),
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn image_arguments_preserve_root_and_console() {
        let base = "root=UUID=example rootwait console=ttyS2";
        assert_eq!(super::image_cmdline(base, Some("  loglevel=7  ")), format!("{base} loglevel=7"));
        assert_eq!(super::image_cmdline(base, Some("  ")), base);
        assert_eq!(super::image_cmdline(base, None), base);
    }
}
