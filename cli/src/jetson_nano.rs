//! NVIDIA Jetson Nano (Tegra X1) serial bring-up adapter: direct boot of an arm64 kernel
//! (an Image or a distribution's EFI zboot vmlinuz) with the stock device tree
//! (`tegra210-p3450-0000.dtb`), patched as the bootloader would patch it.
use crate::{arg, serial_board::{self, RunConfig, SerialBoard}, vnc::Screen};
use sbc_machine::Machine as HostMachine;
use std::process::ExitCode;
use tegra210::{boot, Machine};

impl SerialBoard for Machine {
    fn host(&mut self) -> HostMachine<'_> {
        HostMachine::JetsonNano(self)
    }
    fn now(&self) -> u64 {
        Machine::now(self)
    }
    fn instret(&self) -> u64 {
        Machine::instret(self)
    }
    fn screen(&mut self) -> Option<&mut dyn Screen> {
        Some(self)
    }
    fn report(&self, tag: &str) {
        eprintln!(
            "[{tag}] pc={:#x} el={} insns={} virtual={:.3}s irqs={} exceptions={} cores={:?}",
            self.cpu().pc,
            self.cpu().el,
            Machine::instret(self),
            self.now() as f64 / 1e9,
            self.irqs,
            self.exceptions,
            self.state
        );
        if self.soc.sd[0].blocks_read + self.soc.sd[0].blocks_written > 0 {
            eprintln!("[{tag}] SD card: {} blocks read, {} written", self.soc.sd[0].blocks_read, self.soc.sd[0].blocks_written);
        }
        let nic = &self.soc.pcie.nic;
        if nic.tx_frames + nic.rx_frames > 0 {
            eprintln!("[{tag}] Ethernet: {} frames sent, {} received", nic.tx_frames, nic.rx_frames);
        }
        for (k, d) in self.soc.dc.iter().enumerate().filter(|(_, d)| d.frames > 0) {
            eprintln!("[{tag}] display controller {k}: {} frames, {:?}", d.frames, d.size());
        }
        if !self.soc.stub_pages.is_empty() {
            let pages: Vec<String> = self.soc.stub_pages.iter().map(|p| format!("{p:#010x}")).collect();
            eprintln!("[{tag}] unmodelled device pages (read-back stubs): {}", pages.join(" "));
        }
    }
}

impl Screen for Machine {
    fn shown_size(&self) -> Option<(u32, u32)> {
        self.screen_size()
    }
    fn display_size(&self) -> (u32, u32) {
        self.screen_size().unwrap_or((1280, 720))
    }
    fn render(&self, out: &mut Vec<u8>) -> Option<(u32, u32)> {
        Machine::render(self, out)
    }
    fn name(&self) -> &'static str {
        "sbc-emu: Jetson Nano"
    }
    fn key(&mut self, usage: u8, down: bool) {
        self.soc.usb.kbd.key(usage, down);
        self.soc.dirty = true;
    }
    fn type_byte(&mut self, b: u8) {
        self.soc.usb.kbd.type_byte(b);
        self.soc.dirty = true;
    }
    fn pointer(&mut self, buttons: u8, x: u16, y: u16, wheel: i8) {
        self.soc.usb.tablet.pointer(buttons, x, y, wheel);
        self.soc.dirty = true;
    }
}

pub(crate) fn run(args: &[String]) -> ExitCode {
    match execute(args) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("[jetson-nano] {e}");
            ExitCode::FAILURE
        }
    }
}

fn execute(args: &[String]) -> Result<(), String> {
    serial_board::validate_options(args, &["--dtb", "--dump-dtb", "--sd", "--smp", "--net", "--usb-drive", "--hdmi-mode", "--vnc", "--screenshot"])?;
    let read = |path: &str| std::fs::read(path).map_err(|e| format!("{path}: {e}"));
    let kernel = read(arg(args, "--kernel").ok_or("--kernel Image|vmlinuz is required")?)?;
    let dtb = read(arg(args, "--dtb").ok_or("--dtb tegra210-p3450-0000.dtb is required")?)?;
    let initrd = arg(args, "--initrd").map(read).transpose()?;
    let cmdline = arg(args, "--append").unwrap_or(boot::DEFAULT_CMDLINE).to_string();
    let ram = serial_board::ram_bytes(args, 4096)?;
    let smp = arg(args, "--smp").map_or(Ok(tegra210::NCPU), |v| v.parse::<usize>().map_err(|_| "--smp N"))?;
    let config = RunConfig::from_args("jetson-nano", args)?;
    if let Some(path) = arg(args, "--dump-dtb") {
        let range = initrd.as_ref().map(|d| {
            let start = tegra210::RAM_BASE + boot::INITRD_OFFSET;
            (start, start + d.len() as u64)
        });
        std::fs::write(path, boot::prepare_device_tree(&dtb, ram as u64, &cmdline, range)?).map_err(|e| format!("{path}: {e}"))?;
    }
    // the card survives guest reboots (one disk shared by the successive machines)
    let persist = args.iter().any(|a| a == "--sd-persist");
    let disk = arg(args, "--sd")
        .map(|path| crate::disk::SharedDisk::open(path, persist).map_err(|e| format!("{path}: {e}")))
        .transpose()?;
    let net = arg(args, "--net").unwrap_or("nat");
    if !matches!(net, "nat" | "local" | "none") {
        return Err("--net nat|local|none".into());
    }
    let hdmi = match arg(args, "--hdmi-mode").unwrap_or("1280x720") {
        "off" => None,
        mode => Some(tegra210::Mode::parse(mode)?),
    };
    let no_jit = args.iter().any(|a| a == "--no-jit") || std::env::var("SBC_JIT").is_ok_and(|v| v == "0");
    let log_stubs = std::env::var("SBC_STUB_LOG").is_ok_and(|v| v == "1");
    let make_machine = || -> Result<Machine, String> {
        let unix = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_secs() as i64);
        let mut m = Machine::with_clock(ram, smp, unix)?;
        m.soc.log_stub_pages = log_stubs;
        if no_jit {
            m.set_jit(false);
        } else {
            m.enable_jit();
        }
        if let Some(d) = disk.clone() {
            m.insert_sd(Box::new(d));
        }
        // --usb-drive IMAGE: a USB stick (copy-on-write; each boot starts from the image)
        if let Some(path) = arg(args, "--usb-drive") {
            m.attach_usb_drive(Box::new(crate::disk::CowFile::open(path).map_err(|e| format!("{path}: {e}"))?));
        }
        if let Some(mode) = &hdmi {
            m.connect_monitor(mode);
        }
        if net != "none" {
            m.connect_network(net == "nat", std::env::var_os("SBC_NET_LOG").is_some());
        }
        boot::boot_linux(&mut m, &kernel, &dtb, initrd.as_deref(), &cmdline)?;
        Ok(m)
    };
    eprintln!(
        "[jetson-nano] experimental Tegra X1: {smp} Cortex-A57 core(s) ({}), GIC-400, UART A, timers, clock/reset controller, PMC, \
         fuses, I2C with the PMIC, GPIO, SD card, PCIe with the RTL8111H Ethernet controller, USB (keyboard, tablet, --usb-drive), HDMI; PSCI by the host; other devices are read-back stubs",
        if no_jit { "interpreter" } else { "JIT" }
    );
    serial_board::run(config, make_machine)
}
