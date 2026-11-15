//! Orange Pi One (Allwinner H3) serial bring-up adapter: direct boot of an ARM32 zImage
//! with the distribution's device tree, patched as U-Boot would patch it.
use crate::{arg, serial_board::{self, RunConfig, SerialBoard}};
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
            eprintln!("[orangepi-one] {e}");
            ExitCode::FAILURE
        }
    }
}

fn execute(args: &[String]) -> Result<(), String> {
    serial_board::validate_options(args, &["--dtb", "--dump-dtb", "--sd"])?;
    let read = |path: &str| std::fs::read(path).map_err(|e| format!("{path}: {e}"));
    let kernel = read(arg(args, "--kernel").ok_or("--kernel zImage is required")?)?;
    let dtb = read(arg(args, "--dtb").ok_or("--dtb sun8i-h3-orangepi-one.dtb is required")?)?;
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
        m.soc.mmc.card.disk = disk.clone().map(|d| Box::new(d) as Box<dyn emu_core::disk::Disk>);
        allwinner_h3::boot::boot_linux(&mut m, &kernel, &dtb, initrd.as_deref(), cmdline)?;
        Ok(m)
    };
    if let Some(path) = arg(args, "--dump-dtb") {
        use allwinner_h3::boot::{prepare_device_tree, INITRD_ADDR};
        let range = initrd.as_ref().map(|d| (INITRD_ADDR, INITRD_ADDR + d.len() as u32));
        let blob = prepare_device_tree(&dtb, ram as u64, cmdline, range)?;
        std::fs::write(path, blob).map_err(|e| format!("{path}: {e}"))?;
    }
    eprintln!(
        "[orangepi-one] experimental Allwinner H3: four Cortex-A7s (interpreter, A32 + Thumb-2 + VFPv4), GIC-400, UART0, timers; \
         direct kernel boot with the distribution DTB, PSCI by the host; other devices are stubs"
    );
    serial_board::run(config, make_machine)
}
