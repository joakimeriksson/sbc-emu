//! Orange Pi One (Allwinner H3) serial bring-up adapter: direct boot of an ARM32 zImage
//! with the distribution's device tree, patched as U-Boot would patch it.
use crate::{arg, serial_board::{self, RunConfig, SerialBoard}, vnc::Screen};
use allwinner_h3::display::{Display, Mode};
use sbc_machine::Machine as HostMachine;
use std::process::ExitCode;

impl SerialBoard for allwinner_h3::Machine {
    fn host(&mut self) -> HostMachine<'_> {
        HostMachine::OrangePiOne(self)
    }
    fn now(&self) -> u64 {
        allwinner_h3::Machine::now(self)
    }
    fn instret(&self) -> u64 {
        allwinner_h3::Machine::instret(self)
    }
    fn save(&self) -> Result<Vec<u8>, String> {
        allwinner_h3::Machine::save(self)
    }
    fn restore(&mut self, file: &[u8]) -> Result<(), String> {
        allwinner_h3::Machine::restore(self, file)
    }
    fn report(&self, tag: &str) {
        eprintln!(
            "[{tag}] pc={:#010x} insns={} virtual={:.3}s irqs={} exceptions={} cores={:?}",
            self.cpu().r[15],
            allwinner_h3::Machine::instret(self),
            self.now() as f64 / 1e9,
            self.irqs,
            self.exceptions,
            self.state
        );
        for (k, cpu) in self.cpus.iter().enumerate() {
            if let Some((pc, insn)) = cpu.last_undef {
                eprintln!("[{tag}] core {k}: last UNDEFINED instruction {insn:#010x} at {pc:#010x}");
            }
        }
        if self.soc.emac.tx_frames + self.soc.emac.rx_frames > 0 {
            eprintln!("[{tag}] Ethernet: {} frames sent, {} received", self.soc.emac.tx_frames, self.soc.emac.rx_frames);
        }
        if self.soc.display.frames > 0 {
            eprintln!("[{tag}] HDMI: {} frames scanned out", self.soc.display.frames);
        }
        let stats: Vec<(u64, u64, u64)> = self.cpus.iter().filter_map(|c| c.jit_stats()).collect();
        if !stats.is_empty() {
            let sum = |f: fn(&(u64, u64, u64)) -> u64| stats.iter().map(f).sum::<u64>();
            eprintln!(
                "[{tag}] JIT: {} blocks compiled, {} runs, {} dropped by code writes",
                sum(|s| s.0),
                sum(|s| s.1),
                sum(|s| s.2)
            );
        }
        if std::env::var("SBC_JIT_PROFILE").is_ok_and(|v| v == "1") {
            for (word, slow, n) in arm_a32::jit::profile_fallbacks().into_iter().take(40) {
                eprintln!("[{tag}]   interpreted {word:#010x}{} x{n}", if slow { " (micro-TLB miss)" } else { "" });
            }
        }
        if !self.soc.stub_pages.is_empty() {
            let pages: Vec<String> = self.soc.stub_pages.iter().map(|p| format!("{p:#010x}")).collect();
            eprintln!("[{tag}] unmodelled device pages (read-back stubs): {}", pages.join(" "));
        }
    }
    fn screen(&mut self) -> Option<&mut dyn Screen> {
        Some(self)
    }
}

impl Screen for allwinner_h3::Machine {
    fn shown_size(&self) -> Option<(u32, u32)> {
        self.soc.display.shown_size()
    }
    fn display_size(&self) -> (u32, u32) {
        let mode = self.soc.display.mode;
        self.soc.display.shown_size().unwrap_or((mode.width, mode.height))
    }
    fn render(&self, out: &mut Vec<u8>) -> Option<(u32, u32)> {
        self.soc.render(out)
    }
    fn name(&self) -> &'static str {
        "sbc-emu: Orange Pi One"
    }
    fn key(&mut self, usage: u8, down: bool) {
        self.soc.kbd.key(usage, down);
    }
    fn type_byte(&mut self, b: u8) {
        self.soc.kbd.type_byte(b);
    }
    fn pointer(&mut self, buttons: u8, x: u16, y: u16, wheel: i8) {
        self.soc.tablet.pointer(buttons, x, y, wheel);
    }
}

pub(crate) fn run(args: &[String]) -> ExitCode {
    arm_a32::jit::set_profile(std::env::var("SBC_JIT_PROFILE").is_ok_and(|v| v == "1"));
    match execute(args) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("[orangepi-one] {e}");
            ExitCode::FAILURE
        }
    }
}

fn execute(args: &[String]) -> Result<(), String> {
    serial_board::validate_options(args, &["--dtb", "--dump-dtb", "--sd", "--screenshot", "--vnc", "--hdmi-mode", "--net", "--restore", "--snapshot"])?;
    let net = arg(args, "--net").unwrap_or("nat");
    if !matches!(net, "nat" | "local" | "none") {
        return Err("--net nat|local|none".into());
    }
    let mode = match arg(args, "--hdmi-mode") {
        Some(spec) => {
            let (w, h) = spec.split_once('x').ok_or("--hdmi-mode WxH")?;
            let (w, h) = (w.parse().map_err(|_| "--hdmi-mode WxH")?, h.parse().map_err(|_| "--hdmi-mode WxH")?);
            Mode::find(w, h).ok_or("--hdmi-mode: the emulated monitor offers 1024x768, 1280x720 and 1920x1080")?
        }
        None => Mode::MODES[1],
    };
    let read = |path: &str| std::fs::read(path).map_err(|e| format!("{path}: {e}"));
    // Without --kernel, an --sd card is booted the way the board's firmware would
    // (armbianEnv.txt, zImage, the board DTB and uInitrd from the card's ext4 /boot).
    let card_boot = arg(args, "--kernel").is_none() && arg(args, "--sd").is_some();
    if card_boot {
        for option in ["--dtb", "--initrd", "--dump-dtb"] {
            if arg(args, option).is_some() {
                eprintln!("[orangepi-one] {option} is ignored when booting the SD card (no --kernel)");
            }
        }
    }
    // a snapshot has the kernel in its RAM
    let restore = arg(args, "--restore").is_some();
    let no_kernel = arg(args, "--kernel").is_none();
    let (kernel, dtb) = if no_kernel && (card_boot || restore) {
        (Vec::new(), Vec::new())
    } else {
        (
            read(arg(args, "--kernel").ok_or("--kernel zImage is required (or --sd IMAGE to boot from the card)")?)?,
            read(arg(args, "--dtb").ok_or("--dtb sun8i-h3-orangepi-one.dtb is required")?)?,
        )
    };
    let initrd = arg(args, "--initrd").filter(|_| !(no_kernel && restore)).map(read).transpose()?;
    let cmdline = arg(args, "--append").unwrap_or(allwinner_h3::boot::DEFAULT_CMDLINE);
    let ram = serial_board::ram_bytes(args, allwinner_h3::RAM_SIZE >> 20)?;
    if ram == 0 || ram > 2 << 30 {
        return Err("--ram-mb must be between 1 and 2048 (the H3 maps up to 2 GiB of DRAM)".into());
    }
    let config = RunConfig::from_args("orangepi-one", args)?;
    // The SD card survives guest reboots (one disk shared by the successive machines).
    let persist = args.iter().any(|a| a == "--sd-persist");
    let disk = arg(args, "--sd")
        .map(|path| crate::disk::SharedDisk::open(path, persist).map_err(|e| format!("{path}: {e}")))
        .transpose()?;
    // the machine restored into is not booted first; when the guest reboots, a new one is
    let restored = std::cell::Cell::new(false);
    let make_machine = || -> Result<allwinner_h3::Machine, String> {
        let mut m = allwinner_h3::Machine::with_ram_size(ram);
        if args.iter().any(|a| a == "--no-jit") {
            m.set_jit(false);
        }
        m.soc.display = Display::new(mode);
        if net != "none" {
            m.connect_network(net == "nat", false);
        }
        m.soc.mmc.card.disk = disk.clone().map(|d| Box::new(d) as Box<dyn emu_core::disk::Disk>);
        if restore && !restored.replace(true) {
            // nothing: the snapshot has it all
        } else if no_kernel && !card_boot {
            return Err("the guest rebooted: give --kernel or --sd with --restore to boot it again".into());
        } else if card_boot {
            let boot = allwinner_h3::boot::boot_from_card(&mut m, arg(args, "--append").unwrap_or(""))?;
            eprintln!(
                "[orangepi-one] boot from SD card: kernel {} bytes, initrd {} bytes, cmdline \"{}\"",
                boot.kernel_bytes, boot.initrd_bytes, boot.cmdline
            );
            for note in boot.notes {
                eprintln!("[orangepi-one] note: {note}");
            }
        } else {
            allwinner_h3::boot::boot_linux(&mut m, &kernel, &dtb, initrd.as_deref(), cmdline)?;
        }
        Ok(m)
    };
    if let Some(path) = arg(args, "--dump-dtb").filter(|_| !card_boot) {
        if restore {
            return Err("--dump-dtb: a restored machine is not booted from a device tree".into());
        }
        use allwinner_h3::boot::{prepare_device_tree, INITRD_ADDR};
        let range = initrd.as_ref().map(|d| (INITRD_ADDR, INITRD_ADDR + d.len() as u32));
        let blob = prepare_device_tree(&dtb, ram as u64, cmdline, range)?;
        std::fs::write(path, blob).map_err(|e| format!("{path}: {e}"))?;
    }
    let wanted = !args.iter().any(|a| a == "--no-jit") && std::env::var("SBC_JIT").map_or(true, |v| v != "0");
    // `available` is false when the system refused the JIT executable memory
    let jit = wanted && arm_a32::jit::available();
    if wanted && arm_a32::jit::refused() {
        eprintln!("[orangepi-one] warning: the system refuses writable and executable memory (SELinux deny_execmem, MemoryDenyWriteExecute?): no JIT, interpreting");
    }
    eprintln!(
        "[orangepi-one] experimental Allwinner H3: four Cortex-A7s ({}, A32 + Thumb-2 + VFPv4 + NEON), GIC-400, UART0, timers, SD card, \
         HDMI (DE2 mixer, TCON, DW-HDMI), USB, Ethernet; PSCI by the host; other devices are stubs",
        crate::cpu_backend(jit)
    );
    serial_board::run(config, make_machine)
}
