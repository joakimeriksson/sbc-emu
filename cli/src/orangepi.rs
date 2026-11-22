//! Native serial-only RK3588S bring-up adapter. Unsupported Pi options fail explicitly.
use crate::{arg, serial_board::{self, RunConfig, SerialBoard}, vnc::Screen};
use sbc_machine::Machine as HostMachine;
use std::process::ExitCode;

// Reboots reconstruct the SoC while preserving the same locked image and COW overlay.
use crate::disk::SharedDisk;

impl SerialBoard for rk3588::Machine {
    fn host(&mut self) -> HostMachine<'_> { HostMachine::OrangePi5(self) }
    fn now(&self) -> u64 { rk3588::Machine::now(self) }
    fn screen(&mut self) -> Option<&mut dyn Screen> { Some(self) }
    fn instret(&self) -> u64 { rk3588::Machine::instret(self) }
    fn save(&self) -> Result<Vec<u8>, String> { rk3588::Machine::save(self) }
    fn restore(&mut self, file: &[u8]) -> Result<(), String> { rk3588::Machine::restore(self, file) }
    // --audio-out: the headphone jack (ES8388); --hdmi-audio-out: HDMI0
    fn capture_audio(&mut self) -> bool { self.soc.audio.capture(true, false); true }
    fn take_audio(&mut self) -> (u32, Vec<i16>) { let i = &mut self.soc.audio.i2s1; (i.rate, std::mem::take(&mut i.out)) }
    fn capture_hdmi_audio(&mut self) -> bool { self.soc.audio.capture(false, true); true }
    fn take_hdmi_audio(&mut self) -> (u32, Vec<i16>) { let i = &mut self.soc.audio.i2s5; (i.rate, std::mem::take(&mut i.out)) }
    fn report(&self, tag: &str) {
        let m = self;
        let pcs: Vec<String> = m.cpus.iter().zip(&m.state).map(|(c, s)| format!("{:#x}{}", c.pc, if *s == rk3588::CoreState::Off { "(off)" } else { "" })).collect();
        eprintln!("[{tag}] pc={} insns={} virtual={:.3}s irqs={} acknowledgements={} exceptions={}",pcs.join(","),m.instret(),m.now() as f64/1e9,m.irqs,m.soc.gic.acknowledges,m.exceptions);
        if !m.soc.unknown.is_empty(){eprintln!("[{tag}] unmapped MMIO: {:x?}",m.soc.unknown);}
        let mut sys: Vec<u32> = m.cpus.iter().flat_map(|c| c.sys.unknown.iter().copied()).collect();
        sys.sort_unstable(); sys.dedup();
        if !sys.is_empty(){eprintln!("[{tag}] unmodelled system registers: {:x?}",sys);}
        let usb: Vec<String> = m.soc.xhci.attached().iter().map(|d| format!("{d:?}")).collect();
        eprintln!("[{tag}] USB: {}", usb.join(", "));
        if let Some(n) = m.soc.pcie.nvme.as_ref() { eprintln!("[{tag}] NVMe (M.2): {} reads, {} writes", n.reads, n.writes); }
        if m.soc.display.frames.get() > 0 { eprintln!("[{tag}] HDMI (firmware framebuffer): {} frames shown", m.soc.display.frames.get()); }
        if m.soc.vop2.frames > 0 { eprintln!("[{tag}] HDMI (VOP2): {} frames, {} shown, mode {:?}, EDID bytes read {}, SCMI calls {}", m.soc.vop2.frames, m.soc.vop2.shown.get(), m.soc.vop2.size(), m.soc.hdmi.ddc_reads, m.soc.scmi.calls); }
        else if m.soc.scmi.calls > 0 { eprintln!("[{tag}] SCMI calls {}, EDID bytes read {}", m.soc.scmi.calls, m.soc.hdmi.ddc_reads); }
        for (name, i) in [("headphones (ES8388, I2S1)", &m.soc.audio.i2s1), ("HDMI (I2S5)", &m.soc.audio.i2s5)] {
            if i.frames > 0 || i.underruns > 0 { eprintln!("[{tag}] audio {name}: {} frames at {} Hz, {} underruns", i.frames, i.rate, i.underruns); }
        }
    }
}

impl Screen for rk3588::Machine {
    fn shown_size(&self) -> Option<(u32, u32)> { self.soc.shown_size() }
    fn display_size(&self) -> (u32, u32) { self.soc.shown_size().unwrap_or((1280, 720)) }
    fn render(&self, out: &mut Vec<u8>) -> Option<(u32, u32)> { self.soc.render(out) }
    fn name(&self) -> &'static str { "sbc-emu: Orange Pi 5" }
    fn key(&mut self, usage: u8, down: bool) { self.soc.usb.kbd.key(usage, down); self.soc.dirty = true; }
    fn type_byte(&mut self, b: u8) { self.soc.usb.kbd.type_byte(b); self.soc.dirty = true; }
    fn pointer(&mut self, buttons: u8, x: u16, y: u16, wheel: i8) { self.soc.usb.tablet.pointer(buttons, x, y, wheel); self.soc.dirty = true; }
}

pub(crate) fn run(args:&[String])->ExitCode {
    match execute(args) {Ok(())=>ExitCode::SUCCESS,Err(e)=>{eprintln!("[orangepi5] {e}");ExitCode::FAILURE}}
}
fn execute(args:&[String])->Result<(),String> {
    serial_board::validate_options(args, &["--threads", "--dump-dtb", "--sd", "--gpu", "--net", "--smp", "--hdmi-mode", "--vnc", "--screenshot", "--usb-drive", "--nvme", "--audio-out", "--hdmi-audio-out", "--restore", "--snapshot"])?;
    // --threads multi: a host thread per core (faster, not repeatable); single (default): turns
    let multi=match arg(args,"--threads") { None|Some("single")=>false, Some("multi")=>true, Some(_)=>return Err("--threads single|multi".into()) };
    let smp=arg(args,"--smp").map_or(Ok(rk3588::MAX_CPUS),|s|s.parse::<usize>().ok().filter(|n|(1..=rk3588::MAX_CPUS).contains(n)).ok_or(format!("--smp takes 1..{}",rk3588::MAX_CPUS)))?;
    // HDMI: the firmware's framebuffer at the monitor's preferred mode, unless --no-display
    if arg(args,"--hdmi-mode").is_some() && args.iter().any(|a|a=="--no-display") {return Err("--hdmi-mode and --no-display contradict each other".into());}
    let hdmi=match arg(args,"--hdmi-mode") {
        Some(spec)=>{
            let (w,h)=spec.split_once('x').ok_or("--hdmi-mode WxH")?;
            let (w,h)=(w.parse().map_err(|_|"--hdmi-mode WxH")?,h.parse().map_err(|_|"--hdmi-mode WxH")?);
            Some(rk3588::display::Mode::find(w,h).ok_or("--hdmi-mode: the emulated monitor offers 1280x720, 1920x1080 and 640x480")?)
        }
        None=>(!args.iter().any(|a|a=="--no-display")).then_some(rk3588::display::Mode::MODES[0]),
    };
    let gpu=arg(args,"--gpu").unwrap_or("none");
    if !["none","probe"].contains(&gpu) {return Err("--gpu accepts none or probe (experimental, no rendering)".into());}
    let net=arg(args,"--net").unwrap_or("nat");
    if !["none","local","nat"].contains(&net) {return Err("--net must be none, local or nat".into());}
    let kernel=arg(args,"--kernel").map(std::fs::read).transpose().map_err(|e|e.to_string())?;
    // a snapshot has the kernel in its RAM
    let restore=arg(args,"--restore").is_some();
    if kernel.is_none() && arg(args,"--sd").is_none() && !restore { return Err("provide --kernel Image or --sd IMAGE".into()); }
    if kernel.is_none() && arg(args,"--initrd").is_some() { return Err("--initrd requires --kernel".into()); }
    let initrd=arg(args,"--initrd").map(std::fs::read).transpose().map_err(|e|e.to_string())?;
    let cmdline=arg(args,"--append").unwrap_or(rk3588::boot::DEFAULT_CMDLINE);
    let ram=serial_board::ram_bytes(args, if kernel.is_some() {256} else {1024})?;
    let config=RunConfig::from_args("orangepi5", args)?;
    let persist=args.iter().any(|a|a=="--sd-persist");
    let disk=arg(args,"--sd").map(|p|SharedDisk::open(p,persist).map_err(|e|format!("{p}: {e}"))).transpose()?;
    // the machine restored into is not booted first; when the guest reboots, a new one is
    let restored=std::cell::Cell::new(false);
    let make_machine=||->Result<rk3588::Machine,String>{
        let mut m=rk3588::Machine::with_cpus(ram,smp)?;
        m.gpu_probe=gpu=="probe";
        m.multi=multi;
        if let Some(mode)=hdmi { m.enable_hdmi(mode); }
        // --usb-drive IMAGE: a USB stick (copy-on-write; each boot starts from the image)
        // --nvme IMAGE: an NVMe SSD in the M.2 slot (copy-on-write; each boot starts from the image)
        if let Some(path)=arg(args,"--nvme") {
            m.attach_nvme(Box::new(crate::disk::CowFile::open(path).map_err(|e|format!("{path}: {e}"))?));
        }
        if let Some(path)=arg(args,"--usb-drive") {
            m.attach_usb_drive(Box::new(crate::disk::CowFile::open(path).map_err(|e|format!("{path}: {e}"))?));
        }
        if net != "none" { m.connect_network(net == "nat", false); }
        m.soc.mmc.card.disk=disk.clone().map(|d|Box::new(d) as Box<dyn emu_core::disk::Disk>);
        if restore && !restored.replace(true) {
            // nothing: the snapshot has it all
        } else if let Some(kernel)=kernel.as_ref() {
            rk3588::boot::boot_linux(&mut m,kernel,initrd.as_deref(),cmdline)?;
        } else if arg(args,"--sd").is_none() {
            return Err("the guest rebooted: give --kernel or --sd with --restore to boot it again".into());
        } else {
            let card=rk3588::boot::read_card(m.soc.mmc.card.disk.as_mut().ok_or("no SD image")?.as_mut())?;
            for note in &card.notes {eprintln!("[orangepi5 firmware] {note}");}
            let bootargs=image_cmdline(&card.cmdline,arg(args,"--append"));
            rk3588::boot::boot_linux(&mut m,&card.kernel,card.initrd.as_deref(),&bootargs)?;
        }
        if !args.iter().any(|a|a=="--no-jit") {m.enable_jit();}
        Ok(m)
    };
    if let Some(path)=arg(args,"--dump-dtb") {
        if restore {return Err("--dump-dtb: a restored machine is not booted from a device tree".into());}
        std::fs::write(path,&make_machine()?.dtb).map_err(|e|format!("{path}: {e}"))?;
    }
    eprintln!("[orangepi5] experimental RK3588S: {} core(s) ({} Cortex-A55, {} Cortex-A76), GICv3, UART2, microSD, Ethernet, USB (keyboard, tablet{}), HDMI ({}), audio (headphones, HDMI); emulated firmware, reduced DT",smp,smp.min(4),smp.saturating_sub(4),if arg(args,"--usb-drive").is_some() {", stick"} else {""},hdmi.map_or("off".to_string(),|m|format!("{}x{} framebuffer",m.width,m.height)));
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
