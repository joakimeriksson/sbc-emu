//! ASUS Tinker Board S (Rockchip RK3288) serial bring-up adapter: direct boot of an ARM32
//! zImage with the stock device tree, patched as U-Boot's distro boot would patch it, or
//! firmware-style boot of an SD card or eMMC image (Armbian's /boot).
use crate::{arg, serial_board::{self, RunConfig, SerialBoard}};
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
        if !self.soc.stub_pages.is_empty() {
            let pages: Vec<String> = self.soc.stub_pages.iter().map(|p| format!("{p:#010x}")).collect();
            eprintln!("[{tag}] unmodelled device pages (read-back stubs): {}", pages.join(" "));
        }
    }
}

pub(crate) fn run(args: &[String]) -> ExitCode {
    match execute(args) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("[tinkerboard] {e}");
            ExitCode::FAILURE
        }
    }
}

fn execute(args: &[String]) -> Result<(), String> {
    serial_board::validate_options(args, &["--dtb", "--dump-dtb", "--sd", "--emmc"])?;
    let read = |path: &str| std::fs::read(path).map_err(|e| format!("{path}: {e}"));
    // Without --kernel, a card (--sd, else --emmc) is booted the way the board's firmware
    // would: armbianEnv.txt, zImage, the board DTB and uInitrd from the card's /boot.
    let card_boot = arg(args, "--kernel").is_none() && (arg(args, "--sd").is_some() || arg(args, "--emmc").is_some());
    if card_boot {
        for option in ["--dtb", "--initrd", "--dump-dtb"] {
            if arg(args, option).is_some() {
                eprintln!("[tinkerboard] {option} is ignored when booting from a card (no --kernel)");
            }
        }
    }
    let (kernel, dtb) = if card_boot {
        (Vec::new(), Vec::new())
    } else {
        (
            read(arg(args, "--kernel").ok_or("--kernel zImage is required (or --sd IMAGE to boot from the card)")?)?,
            read(arg(args, "--dtb").ok_or("--dtb rk3288-tinker-s.dtb is required")?)?,
        )
    };
    let initrd = arg(args, "--initrd").map(read).transpose()?;
    let cmdline = arg(args, "--append").unwrap_or(boot::DEFAULT_CMDLINE);
    let ram = serial_board::ram_bytes(args, rockchip_rk3288::RAM_SIZE >> 20)?;
    if ram == 0 || ram > 0xfe00_0000 {
        return Err("--ram-mb must be between 1 and 4064 (DRAM ends below the RK3288 device window)".into());
    }
    let config = RunConfig::from_args("tinkerboard", args)?;
    // The cards survive guest reboots (one disk shared by the successive machines).
    let persist = args.iter().any(|a| a == "--sd-persist");
    let open = |option: &str| {
        arg(args, option)
            .map(|path| crate::disk::SharedDisk::open(path, persist).map_err(|e| format!("{path}: {e}")))
            .transpose()
    };
    let (sd, emmc) = (open("--sd")?, open("--emmc")?);
    // The RK808's RTC starts at the host's time, as a board whose coin cell kept it.
    let unix_now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs() as i64);
    let make_machine = || -> Result<Machine, String> {
        let mut m = Machine::with_ram_size(ram);
        m.soc.periph.attach_rk808(unix_now);
        let disk = |d: &Option<crate::disk::SharedDisk>| d.clone().map(|d| Box::new(d) as Box<dyn emu_core::disk::Disk>);
        m.soc.periph.mmc[devices::SDMMC].insert(disk(&sd));
        m.soc.periph.mmc[devices::EMMC].insert(disk(&emmc));
        if card_boot {
            let boot = boot::boot_from_card(&mut m, arg(args, "--append").unwrap_or(""))?;
            eprintln!(
                "[tinkerboard] boot from {}: {}, kernel {} bytes, initrd {} bytes, cmdline \"{}\"",
                if boot.controller == devices::SDMMC { "SD card" } else { "eMMC" },
                boot.dtb,
                boot.kernel_bytes,
                boot.initrd_bytes,
                boot.cmdline
            );
            for note in boot.notes {
                eprintln!("[tinkerboard] note: {note}");
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
    eprintln!(
        "[tinkerboard] experimental Rockchip RK3288: four Cortex-A12 (interpreter), GIC-400, UART2, \
         CRU/GRF/PMU, boot ROM SMP mailbox, SD/eMMC (dw_mmc), I2C with the RK808 PMIC; other devices are stubs"
    );
    serial_board::run(config, make_machine)
}
