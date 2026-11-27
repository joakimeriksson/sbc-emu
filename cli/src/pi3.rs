//! Raspberry Pi 3 (BCM2837) adapter: the serial console on the mini UART, boot like the
//! firmware from an SD card image (`config.txt` with its `[pi3]` sections, `cmdline.txt`,
//! `kernel8.img`, the initramfs) or a kernel and initramfs given directly.
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
        if self.soc.sd.reads + self.soc.sd.writes > 0 {
            eprintln!("[{tag}] SD card: {} reads, {} writes", self.soc.sd.reads, self.soc.sd.writes);
        }
        if !self.soc.mbox.unknown.is_empty() {
            let tags: Vec<String> = self.soc.mbox.unknown.iter().map(|t| format!("{t:#x}")).collect();
            eprintln!("[{tag}] firmware property tags answered as unknown: {}", tags.join(" "));
        }
    }
    fn save(&self) -> Result<Vec<u8>, String> { Machine::save(self) }
    fn restore(&mut self, file: &[u8]) -> Result<(), String> { Machine::restore(self, file) }
    fn screen(&mut self) -> Option<&mut dyn Screen> { Some(self) }
}

impl Screen for Machine {
    fn shown_size(&self) -> Option<(u32, u32)> { self.soc.shown_size() }
    fn display_size(&self) -> (u32, u32) { self.soc.mbox.fb_size() }
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
    serial_board::validate_options(args, &["--sd", "--smp", "--model", "--threads", "--seed", "--screenshot", "--vnc", "--restore", "--snapshot", "--net", "--hostfwd"])?;
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
        let (k, i, c) = match (&kernel, disk.clone()) {
            (Some(k), _) => (k.clone(), initrd.clone(), append.clone().unwrap_or_else(|| boot::DEFAULT_CMDLINE.to_string())),
            (None, Some(mut d)) => {
                let b = boot::read_sd_boot(&mut d, model)?;
                if !announced.get() { for n in &b.notes { eprintln!("[pi3] firmware: {n}"); } }
                (b.kernel, b.initramfs, append.clone().unwrap_or(b.cmdline))
            }
            (None, None) => return Err("nothing to boot".into()),
        };
        if let Some(d) = disk.clone() { m.soc.sd = bcm2711::sdhci::Sdhci::new(Some(Box::new(d))); }
        // SBC_SD_TRACE=1: the SD host's commands and data on stderr; SBC_USB_TRACE=1: USB transfers
        m.soc.sd.trace = std::env::var("SBC_SD_TRACE").is_ok_and(|v| v == "1");
        m.soc.usb.trace = std::env::var("SBC_USB_TRACE").is_ok_and(|v| v == "1");
        if net != "none" && m.soc.tree.lan_ref().is_some() { m.connect_network(net == "nat", trace_net); }
        for &(host, guest) in &forwards {
            m.forward_tcp(host, guest)?;
            if !announced.get() { eprintln!("[pi3] forwarding {host} -> guest port {guest}"); }
        }
        if !announced.replace(true) {
            eprintln!(
                "[pi3] {} (BCM2837): {smp} Cortex-A53 core(s) ({}, {}), {} MiB RAM ({} MiB for Linux), per-core and ARM-side interrupt controllers, \
                 generic timer at 19.2 MHz, mini UART console, PL011, mailbox, SD card on the Arasan SDHCI, RNG, thermal sensor, watchdog, \
                 DWC2 USB host with {}{}",
                model.name(), crate::cpu_backend(jit), if multi { "a host thread each" } else { "one host thread" },
                ram >> 20, m.soc.arm_ram() >> 20,
                match model {
                    Model::B3Plus => format!("the LAN7515 (two hubs, LAN7800 gigabit Ethernet: {}), USB keyboard and tablet", if net == "none" { "no network".into() } else { format!("--net {net}") }),
                    Model::B3 => "a hub (the LAN9514's Ethernet is not modelled), USB keyboard and tablet".into(),
                    _ => "a USB keyboard".to_string(),
                },
                if display { ", firmware framebuffer" } else { "" }
            );
        }
        boot::boot_linux(&mut m, &k, i.as_deref(), &c, display)?;
        Ok(m)
    };
    serial_board::run(config, make_machine)
}
