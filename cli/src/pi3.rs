//! Raspberry Pi 3 (BCM2837) adapter: the serial console on the mini UART, boot like the
//! firmware from an SD card image (`config.txt` with its `[pi3]` sections, `cmdline.txt`,
//! `kernel8.img`, the initramfs) or a kernel and initramfs given directly. HDMI through the vc4
//! KMS pipeline when `config.txt` asks for it (`--display kms|fb` decides otherwise), its audio
//! to `--audio-out`.
use crate::{arg, serial_board::{self, RunConfig, SerialBoard}, vnc::Screen};
use bcm2837::{boot, Machine, Model};
use sbc_machine::Machine as HostMachine;
use std::process::ExitCode;

impl SerialBoard for Machine {
    fn host(&mut self) -> HostMachine<'_> { HostMachine::Pi3(self) }
    fn now(&self) -> u64 { self.now }
    fn instret(&self) -> u64 { Machine::instret(self) }
    fn report(&self, tag: &str) {
        eprintln!(
            "[{tag}] pc={:#x} el={} insns={} virtual={:.3}s irqs={} exceptions={} cores={:?}",
            self.cpu().pc, self.cpu().el, Machine::instret(self), self.now as f64 / 1e9, self.irqs, self.exceptions, self.state
        );
        if let Some(l) = self.soc.tree.lan_ref() {
            eprintln!("[{tag}] USB: {} channel transfers; Ethernet: {} frames out, {} in, {} dropped", self.soc.usb.transfers, l.tx_frames, l.rx_frames, l.rx_dropped);
        }
        if let Some(n) = self.soc.net.as_ref() {
            eprintln!("[{tag}] network: {} DHCP acks, {} DNS answers, {} pings answered", n.dhcp_acks, n.dns_answers, n.pings);
        }
        if let Some(n) = self.soc.wifi_net.as_ref().filter(|n| n.dhcp_acks + n.dns_answers + n.pings > 0) {
            eprintln!("[{tag}] Wi-Fi network: {} DHCP acks, {} DNS answers, {} pings answered", n.dhcp_acks, n.dns_answers, n.pings);
        }
        if let Some(bt) = self.soc.bt.as_ref().filter(|b| b.commands > 0) {
            eprintln!("[{tag}] Bluetooth: {} HCI commands", bt.commands);
        }
        if self.soc.sd.reads + self.soc.sd.writes > 0 {
            eprintln!("[{tag}] SD card: {} reads, {} writes", self.soc.sd.reads, self.soc.sd.writes);
        }
        if let Some(v) = self.soc.vc4.as_ref() {
            let mode = v.size().map_or("off".to_string(), |(w, h)| format!("{w}x{h}"));
            let out = match (v.output(), v.tv_norm()) { (Some(bcm2711::vc4::Output::Composite), n) => format!("composite ({})", n.unwrap_or("?")), _ => "HDMI".into() };
            eprintln!("[{tag}] {out}: {mode}, {} frames on pixel valve 2; {} CEC messages", v.frames, v.cec.log.len());
            if std::env::var_os("SBC_CEC_LOG").is_some() { for l in &v.cec.log { eprintln!("[{tag}] CEC {l}"); } }
        }
        let vq = &self.soc.vchiq;
        if vq.connected() {
            eprintln!("[{tag}] VCHIQ: {} messages in, {} out; MMAL: {} components, {} pictures decoded, {} buffers returned, {} shared-memory handles; audio: {} writes played",
                      vq.rx_msgs, vq.tx_msgs, vq.mmal.component_kinds().len(), vq.mmal.pictures, vq.mmal.buffers, vq.mmal.smem.len(), vq.audio.completed);
        }
        if !self.soc.mbox.unknown.is_empty() {
            let tags: Vec<String> = self.soc.mbox.unknown.iter().map(|t| format!("{t:#x}")).collect();
            eprintln!("[{tag}] firmware property tags answered as unknown: {}", tags.join(" "));
        }
    }
    fn save(&self) -> Result<Vec<u8>, String> { Machine::save(self) }
    fn restore(&mut self, file: &[u8]) -> Result<(), String> { Machine::restore(self, file) }
    fn screen(&mut self) -> Option<&mut dyn Screen> { Some(self) }
    // HDMI audio and the headphone jack's (the firmware's audio service over VCHIQ)
    fn capture_audio(&mut self) -> bool { self.soc.capture_audio = true; true }
    fn take_audio(&mut self) -> (u32, Vec<i16>) { (self.soc.audio_rate, std::mem::take(&mut self.soc.audio)) }
    fn set_hdmi_connected(&mut self, on: bool) -> bool { if self.soc.vc4.is_none() { return false; } Machine::set_hdmi_connected(self, on); true }
}

impl Screen for Machine {
    fn shown_size(&self) -> Option<(u32, u32)> { self.soc.shown_size() }
    fn display_size(&self) -> (u32, u32) { self.soc.display_size() }
    fn render(&self, out: &mut Vec<u8>) -> Option<(u32, u32)> { self.soc.render(out) }
    fn name(&self) -> &'static str { "sbc-emu: Raspberry Pi 3" }
    // the USB keyboard and tablet (behind the hub on a 3B+)
    fn key(&mut self, usage: u8, down: bool) { self.soc.devs.kbd.key(usage, down); self.soc.dirty = true; }
    fn type_byte(&mut self, b: u8) { self.soc.devs.kbd.type_byte(b); self.soc.dirty = true; }
    fn pointer(&mut self, buttons: u8, x: u16, y: u16, wheel: i8) { self.soc.devs.tablet.pointer(buttons, x, y, wheel); self.soc.dirty = true; }
}

/// `--hostfwd tcp:[HOSTADDR]:HOSTPORT-[GUESTADDR]:GUESTPORT` (QEMU's syntax), repeatable.
fn host_forwards(args: &[String]) -> Result<Vec<(std::net::SocketAddr, u16)>, String> {
    let mut out = Vec::new();
    for (i, a) in args.iter().enumerate() {
        if a != "--hostfwd" { continue; }
        let spec = args.get(i + 1).map(String::as_str).unwrap_or("");
        let parsed = spec.strip_prefix("tcp:").and_then(|r| r.split_once('-')).and_then(|(h, g)| {
            let (ha, hp) = h.rsplit_once(':')?;
            let gp = g.rsplit_once(':').map_or(g, |(_, p)| p).parse::<u16>().ok()?;
            let host: std::net::SocketAddr = format!("{}:{}", if ha.is_empty() { "127.0.0.1" } else { ha }, hp).parse().ok()?;
            Some((host, gp))
        });
        out.push(parsed.ok_or_else(|| format!("--hostfwd {spec}: expected tcp:[HOSTADDR]:HOSTPORT-[GUESTADDR]:GUESTPORT"))?);
    }
    Ok(out)
}

pub(crate) fn run(args: &[String]) -> ExitCode {
    match execute(args) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("[pi3] {e}");
            ExitCode::FAILURE
        }
    }
}

fn execute(args: &[String]) -> Result<(), String> {
    // the Pi 3's flags (no value), then the valued options
    const FLAGS: [&str; 3] = ["--no-bluetooth", "--trace-wifi", "--trace-sdio"];
    let valued: Vec<String> = args.iter().filter(|a| !FLAGS.contains(&a.as_str())).cloned().collect();
    serial_board::validate_options(&valued, &["--sd", "--smp", "--model", "--threads", "--seed", "--screenshot", "--vnc", "--restore", "--snapshot", "--net", "--hostfwd",
                                              "--display", "--hdmi-mode", "--audio-out"])?;
    let model = arg(args, "--model").map_or(Ok(Model::B3Plus), Model::parse)?;
    // the 3B+'s Ethernet on the virtual network: nat (default), local (DHCP and DNS only), none
    let net = arg(args, "--net").unwrap_or("nat");
    if !["nat", "local", "none"].contains(&net) { return Err("--net nat|local|none".into()); }
    let forwards = host_forwards(args)?;
    if !forwards.is_empty() && net != "nat" { return Err("--hostfwd needs --net nat".into()); }
    let trace_net = std::env::var_os("SBC_NET_LOG").is_some();
    let multi = match arg(args, "--threads") {
        None | Some("single") => false,
        Some("multi") => true,
        Some(_) => return Err("--threads single|multi".into()),
    };
    let smp = arg(args, "--smp").map_or(Ok(4), |v| v.parse::<usize>().ok().filter(|n| (1..=4).contains(n)).ok_or("--smp 1..4"))?;
    let ram = serial_board::ram_bytes(args, model.ram_mb())?;
    if !(64 << 20..=1 << 30).contains(&ram) { return Err("--ram-mb: 64 to 1024 (the BCM2837 addresses 1 GiB)".into()); }
    let seed = arg(args, "--seed").map(|v| v.parse::<u64>().map_err(|_| "--seed N")).transpose()?;
    let read = |path: &str| std::fs::read(path).map_err(|e| format!("{path}: {e}"));
    let kernel = arg(args, "--kernel").map(read).transpose()?;
    let initrd = arg(args, "--initrd").map(read).transpose()?;
    let append = arg(args, "--append").map(str::to_string);
    if kernel.is_none() && arg(args, "--sd").is_none() { return Err("--kernel Image or --sd IMAGE is required".into()); }
    let display = !args.iter().any(|a| a == "--no-display");
    // --display kms: the vc4 KMS pipeline on HDMI (the default when config.txt asks for it with
    // dtoverlay=vc4-kms-v3d); fb: the firmware's framebuffer only (the default for --kernel)
    let display_mode = arg(args, "--display");
    if display_mode.is_some_and(|d| d != "kms" && d != "fb") { return Err("--display kms|fb".into()); }
    if display_mode == Some("kms") && !display { return Err("--display kms and --no-display contradict each other".into()); }
    // --hdmi-mode WxH: the monitor's preferred mode (default 1920x1080)
    let hdmi_mode = match arg(args, "--hdmi-mode") {
        None => (1920, 1080),
        Some(s) => s.split_once('x').and_then(|(a, b)| Some((a.parse::<u32>().ok()?, b.parse::<u32>().ok()?)))
            .filter(|m| [(1920, 1080), (1280, 720), (1024, 768), (800, 600), (640, 480)].contains(m))
            .ok_or("--hdmi-mode 1920x1080|1280x720|1024x768|800x600|640x480")?,
    };
    let no_jit = args.iter().any(|a| a == "--no-jit") || std::env::var("SBC_JIT").is_ok_and(|v| v == "0");
    // the card survives guest reboots (one disk shared by the successive machines)
    let persist = args.iter().any(|a| a == "--sd-persist");
    let disk = arg(args, "--sd")
        .map(|path| crate::disk::SharedDisk::open(path, persist).map_err(|e| format!("{path}: {e}")))
        .transpose()?;
    let config = RunConfig::from_args("pi3", args)?;
    let announced = std::cell::Cell::new(false);
    let make_machine = || -> Result<Machine, String> {
        let mut m = Machine::new(model, ram, smp);
        m.multi = multi;
        let jit = !no_jit && m.enable_jit();
        match seed {
            Some(s) => m.soc.rng.reseed(s),
            None if multi => m.soc.rng.reseed(bcm2711::periph::Rng200::host_seed()),
            None => {}
        }
        let (k, i, c, asked_kms, outputs) = match (&kernel, disk.clone()) {
            (Some(k), _) => (k.clone(), initrd.clone(), append.clone().unwrap_or_else(|| boot::DEFAULT_CMDLINE.to_string()), false, Default::default()),
            (None, Some(mut d)) => {
                let b = boot::read_sd_boot(&mut d, model)?;
                if !announced.get() { for n in &b.notes { eprintln!("[pi3] firmware: {n}"); } }
                (b.kernel, b.initramfs, append.clone().unwrap_or(b.cmdline), b.kms, b.outputs)
            }
            (None, None) => return Err("nothing to boot".into()),
        };
        let kms = display && match display_mode { Some(d) => d == "kms", None => asked_kms };
        if kms { m.enable_kms(hdmi_mode.0, hdmi_mode.1, outputs); }
        if let Some(d) = disk.clone() { m.soc.sd = bcm2837::sdhost::Sdhost::new(Some(Box::new(d))); }
        // SBC_SD_TRACE=1: the SD host's commands and data on stderr; SBC_USB_TRACE=1: USB transfers
        m.soc.sd.trace = std::env::var("SBC_SD_TRACE").is_ok_and(|v| v == "1");
        m.soc.usb.trace = std::env::var("SBC_USB_TRACE").is_ok_and(|v| v == "1");
        if net != "none" { m.connect_network(net == "nat", trace_net); }
        // the CYW43455's Bluetooth on UART0 (3B+, 3A+), unless --no-bluetooth; its Wi-Fi traces
        if !args.iter().any(|a| a == "--no-bluetooth") { m.enable_bluetooth(); }
        m.trace_wifi(args.iter().any(|a| a == "--trace-wifi"), args.iter().any(|a| a == "--trace-sdio"));
        for &(host, guest) in &forwards {
            m.forward_tcp(host, guest)?;
            if !announced.get() { eprintln!("[pi3] forwarding {host} -> guest port {guest}"); }
        }
        if !announced.replace(true) {
            eprintln!(
                "[pi3] {} (BCM2837): {smp} Cortex-A53 core(s) ({}, {}), {} MiB RAM ({} MiB for Linux), per-core and ARM-side interrupt controllers, \
                 generic timer at 19.2 MHz, mini UART console, mailbox, GPIO, SD card on sdhost, {}, RNG, thermal sensor, watchdog, \
                 DWC2 USB host with {}{}",
                model.name(), crate::cpu_backend(jit), if multi { "a host thread each" } else { "one host thread" },
                ram >> 20, m.soc.arm_ram() >> 20,
                match (model.wireless(), m.soc.bt.is_some()) {
                    (true, true) => "CYW43455 Wi-Fi on the Arasan SDHCI and Bluetooth on the PL011",
                    (true, false) => "CYW43455 Wi-Fi on the Arasan SDHCI, PL011",
                    (false, _) => "PL011 (this model's Wi-Fi chip is not modelled)",
                },
                match model {
                    Model::B3Plus => format!("the LAN7515 (two hubs, LAN7800 gigabit Ethernet: {}), USB keyboard and tablet", if net == "none" { "no network".into() } else { format!("--net {net}") }),
                    Model::B3 => "a hub (the LAN9514's Ethernet is not modelled), USB keyboard and tablet".into(),
                    _ => "a USB keyboard".to_string(),
                },
                if kms {
                    format!(", {}through the vc4 KMS pipeline (HVS, pixel valves, HDMI with CEC and audio, a {}x{} monitor{})",
                            if outputs.composite { "HDMI and composite " } else { "HDMI " }, hdmi_mode.0, hdmi_mode.1, if outputs.composite { "; the VEC" } else { "" })
                } else if display { ", firmware framebuffer".into() } else { String::new() }
            );
        }
        boot::boot_linux(&mut m, &k, i.as_deref(), &c, display)?;
        Ok(m)
    };
    serial_board::run(config, make_machine)
}
