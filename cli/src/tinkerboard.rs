//! ASUS Tinker Board S (Rockchip RK3288) serial bring-up adapter: direct boot of an ARM32
//! zImage with the stock device tree, patched as U-Boot's distro boot would patch it, or
//! firmware-style boot of an SD card or eMMC image (Armbian's /boot).
//!
//! [`run_board`] is the RK3288 run loop shared with the RK3288 panel board (`panelboard.rs`): the
//! boards differ only in their [`Profile`].
use crate::{arg, serial_board::{self, RunConfig, SerialBoard}, vnc::Screen};
use rockchip_rk3288::{boot, devices, Machine};
use sbc_machine::Machine as HostMachine;
use std::process::ExitCode;

impl SerialBoard for Machine {
    fn host(&mut self) -> HostMachine<'_> {
        HostMachine::TinkerBoard(self)
    }
    fn now(&self) -> u64 {
        Machine::now(self)
    }
    fn instret(&self) -> u64 {
        Machine::instret(self)
    }
    fn report(&self, tag: &str) {
        eprintln!(
            "[{tag}] core {} pc={:#010x} insns={} virtual={:.3}s irqs={} exceptions={} cores={:?}",
            self.last_stop_core,
            self.cpus[self.last_stop_core].r[15],
            Machine::instret(self),
            self.now() as f64 / 1e9,
            self.irqs,
            self.exceptions,
            self.state
        );
        let frames: u64 = self.soc.vop.iter().map(|v| v.frames).sum();
        if frames > 0 {
            eprintln!("[{tag}] display: {frames} frames scanned out, {} DSI packets to the panel", self.soc.dsi.packets);
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

impl Screen for Machine {
    fn shown_size(&self) -> Option<(u32, u32)> {
        self.soc.shown_size()
    }
    fn display_size(&self) -> (u32, u32) {
        self.soc.shown_size().unwrap_or((1280, 800))
    }
    fn render(&self, out: &mut Vec<u8>) -> Option<(u32, u32)> {
        self.soc.render(out)
    }
    fn name(&self) -> &'static str {
        self.soc.display_name
    }
    /// The pointer (absolute, 0..32767 across the picture shown) as a finger on the
    /// touchscreen (the panel board's; boards without one ignore it).
    fn pointer(&mut self, buttons: u8, x: u16, y: u16, _wheel: i8) {
        let (w, h) = self.display_size();
        let pixel = |v: u16, n: u32| (v as u32 * n.saturating_sub(1) / 32767) as u16;
        self.soc.touch(buttons & 1 != 0, pixel(x, w), pixel(y, h));
    }
}

/// What differs between the RK3288 boards.
pub(crate) struct Profile<'a> {
    /// Log prefix and `RunConfig` tag.
    pub tag: &'static str,
    /// DRAM when `--ram-mb` is not given.
    pub default_ram_mb: usize,
    /// The error for a missing `--kernel` and `--dtb`.
    pub kernel_hint: &'static str,
    pub dtb_hint: &'static str,
    /// Kernel command line without `--append`.
    pub cmdline: &'a str,
    /// Firmware-style boot from `--sd`/`--emmc` without `--kernel` (Armbian's /boot layout).
    pub card_boot: bool,
    /// The power chips (PMIC, regulators, RTC) on I2C0; the RTC starts at the given Unix time.
    pub power: fn(&mut devices::Peripherals, i64),
    /// Startup banner.
    pub banner: &'static str,
    /// Board-specific machine setup, before the kernel is loaded.
    pub setup: &'a dyn Fn(&mut Machine),
    /// What the board's own boot loader changes in the device tree it passes on.
    pub fixup_dtb: fn(Vec<u8>) -> Result<Vec<u8>, String>,
    /// The default `--net` mode (nat, local or none) when the board has a modelled network
    /// interface; None: no network and no `--net`.
    pub network: Option<&'static str>,
}

pub(crate) fn run(args: &[String]) -> ExitCode {
    run_board(
        args,
        &Profile {
            tag: "tinkerboard",
            default_ram_mb: rockchip_rk3288::RAM_SIZE >> 20,
            kernel_hint: "--kernel zImage is required (or --sd IMAGE to boot from the card)",
            dtb_hint: "--dtb rk3288-tinker-s.dtb is required",
            cmdline: boot::DEFAULT_CMDLINE,
            card_boot: true,
            power: devices::Peripherals::attach_rk808,
            banner: "experimental Rockchip RK3288: four Cortex-A12 (interpreter), GIC-400, UART2, CRU/GRF/PMU, \
                     boot ROM SMP mailbox, SD/eMMC (dw_mmc), I2C with the RK808 PMIC; other devices are stubs",
            setup: &|_| {},
            fixup_dtb: Ok,
            network: None,
        },
    )
}

pub(crate) fn run_board(args: &[String], profile: &Profile) -> ExitCode {
    match execute(args, profile) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("[{}] {e}", profile.tag);
            ExitCode::FAILURE
        }
    }
}

fn execute(args: &[String], profile: &Profile) -> Result<(), String> {
    let tag = profile.tag;
    let mut options = vec!["--dtb", "--dump-dtb", "--sd", "--emmc", "--screenshot", "--vnc"];
    if profile.network.is_some() {
        options.push("--net");
    }
    serial_board::validate_options(args, &options)?;
    let net = arg(args, "--net").or(profile.network).unwrap_or("none");
    if !matches!(net, "nat" | "local" | "none") {
        return Err("--net nat|local|none".into());
    }
    let trace_net = std::env::var_os("SBC_NET_LOG").is_some();
    let read = |path: &str| std::fs::read(path).map_err(|e| format!("{path}: {e}"));
    // Without --kernel, a card (--sd, else --emmc) is booted the way the board's firmware
    // would: armbianEnv.txt, zImage, the board DTB and uInitrd from the card's /boot.
    let card_boot = profile.card_boot
        && arg(args, "--kernel").is_none()
        && (arg(args, "--sd").is_some() || arg(args, "--emmc").is_some());
    if card_boot {
        for option in ["--dtb", "--initrd", "--dump-dtb"] {
            if arg(args, option).is_some() {
                eprintln!("[{tag}] {option} is ignored when booting from a card (no --kernel)");
            }
        }
    }
    let (kernel, dtb) = if card_boot {
        (Vec::new(), Vec::new())
    } else {
        (
            read(arg(args, "--kernel").ok_or(profile.kernel_hint)?)?,
            (profile.fixup_dtb)(read(arg(args, "--dtb").ok_or(profile.dtb_hint)?)?)?,
        )
    };
    let initrd = arg(args, "--initrd").map(read).transpose()?;
    let cmdline = arg(args, "--append").unwrap_or(profile.cmdline);
    let ram = serial_board::ram_bytes(args, profile.default_ram_mb)?;
    if ram == 0 || ram > 0xfe00_0000 {
        return Err("--ram-mb must be between 1 and 4064 (DRAM ends below the RK3288 device window)".into());
    }
    let config = RunConfig::from_args(tag, args)?;
    // The cards survive guest reboots (one disk shared by the successive machines).
    let persist = args.iter().any(|a| a == "--sd-persist");
    let open = |option: &str| {
        arg(args, option)
            .map(|path| crate::disk::SharedDisk::open(path, persist).map_err(|e| format!("{path}: {e}")))
            .transpose()
    };
    let (sd, emmc) = (open("--sd")?, open("--emmc")?);
    // The RTC starts at the host's time, as on a board whose coin cell kept it.
    let unix_now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs() as i64);
    let make_machine = || -> Result<Machine, String> {
        let mut m = Machine::with_ram_size(ram);
        (profile.setup)(&mut m);
        (profile.power)(&mut m.soc.periph, unix_now);
        if net != "none" {
            m.connect_network(net == "nat", trace_net);
        }
        let disk = |d: &Option<crate::disk::SharedDisk>| d.clone().map(|d| Box::new(d) as Box<dyn emu_core::disk::Disk>);
        m.soc.periph.insert_sd(disk(&sd));
        m.soc.periph.mmc[devices::EMMC].insert(disk(&emmc));
        if card_boot {
            let boot = boot::boot_from_card(&mut m, arg(args, "--append").unwrap_or(""))?;
            eprintln!(
                "[{tag}] boot from {}: {}, kernel {} bytes, initrd {} bytes, cmdline \"{}\"",
                if boot.controller == devices::SDMMC { "SD card" } else { "eMMC" },
                boot.dtb,
                boot.kernel_bytes,
                boot.initrd_bytes,
                boot.cmdline
            );
            for note in boot.notes {
                eprintln!("[{tag}] note: {note}");
            }
        } else {
            boot::boot_linux(&mut m, &kernel, &dtb, initrd.as_deref(), cmdline)?;
        }
        Ok(m)
    };
    if let Some(path) = arg(args, "--dump-dtb").filter(|_| !card_boot) {
        let range = initrd
            .as_ref()
            .map(|d| (boot::INITRD_ADDR, boot::INITRD_ADDR + d.len() as u32));
        let blob = boot::prepare_device_tree(&dtb, ram as u64, cmdline, range)?;
        std::fs::write(path, blob).map_err(|e| e.to_string())?;
    }
    eprintln!("[{tag}] {}", profile.banner);
    serial_board::run(config, make_machine)
}
