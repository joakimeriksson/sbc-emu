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
        if self.soc.display.frames > 0 {
            eprintln!("[{tag}] HDMI: {} frames scanned out", self.soc.display.frames);
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
    match execute(args) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("[orangepi-one] {e}");
            ExitCode::FAILURE
        }
    }
}

fn execute(args: &[String]) -> Result<(), String> {
    serial_board::validate_options(args, &["--dtb", "--dump-dtb", "--sd", "--screenshot", "--vnc", "--hdmi-mode"])?;
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
    let (kernel, dtb) = if card_boot {
        (Vec::new(), Vec::new())
    } else {
        (
            read(arg(args, "--kernel").ok_or("--kernel zImage is required (or --sd IMAGE to boot from the card)")?)?,
            read(arg(args, "--dtb").ok_or("--dtb sun8i-h3-orangepi-one.dtb is required")?)?,
        )
    };
    let initrd = arg(args, "--initrd").map(read).transpose()?;
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
    let make_machine = || -> Result<allwinner_h3::Machine, String> {
        let mut m = allwinner_h3::Machine::with_ram_size(ram);
        m.soc.display = Display::new(mode);
        m.soc.mmc.card.disk = disk.clone().map(|d| Box::new(d) as Box<dyn emu_core::disk::Disk>);
        if card_boot {
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
        use allwinner_h3::boot::{prepare_device_tree, INITRD_ADDR};
        let range = initrd.as_ref().map(|d| (INITRD_ADDR, INITRD_ADDR + d.len() as u32));
        let blob = prepare_device_tree(&dtb, ram as u64, cmdline, range)?;
        std::fs::write(path, blob).map_err(|e| format!("{path}: {e}"))?;
    }
    eprintln!(
        "[orangepi-one] experimental Allwinner H3: four Cortex-A7s (interpreter, A32 + Thumb-2 + VFPv4), GIC-400, UART0, timers, SD card, HDMI \
         (DE2 mixer, TCON, DW-HDMI); PSCI by the host; other devices are stubs"
    );
    serial_board::run(config, make_machine)
}
