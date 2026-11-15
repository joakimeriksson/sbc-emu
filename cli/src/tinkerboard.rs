//! ASUS Tinker Board S (Rockchip RK3288) serial bring-up adapter: direct boot of an ARM32
//! zImage with the stock device tree, patched as U-Boot's distro boot would patch it.
use crate::{arg, serial_board::{self, RunConfig, SerialBoard}};
use rockchip_rk3288::{boot, Machine};
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
    serial_board::validate_options(args, &["--dtb", "--dump-dtb"])?;
    let read = |path: &str| std::fs::read(path).map_err(|e| format!("{path}: {e}"));
    let kernel = read(arg(args, "--kernel").ok_or("--kernel zImage is required")?)?;
    let dtb = read(arg(args, "--dtb").ok_or("--dtb rk3288-tinker-s.dtb is required")?)?;
    let initrd = arg(args, "--initrd").map(read).transpose()?;
    let cmdline = arg(args, "--append").unwrap_or(boot::DEFAULT_CMDLINE);
    let ram = serial_board::ram_bytes(args, rockchip_rk3288::RAM_SIZE >> 20)?;
    if ram == 0 || ram > 0xfe00_0000 {
        return Err("--ram-mb must be between 1 and 4064 (DRAM ends below the RK3288 device window)".into());
    }
    let config = RunConfig::from_args("tinkerboard", args)?;
    let make_machine = || -> Result<Machine, String> {
        let mut m = Machine::with_ram_size(ram);
        boot::boot_linux(&mut m, &kernel, &dtb, initrd.as_deref(), cmdline)?;
        Ok(m)
    };
    if let Some(path) = arg(args, "--dump-dtb") {
        let range = initrd
            .as_ref()
            .map(|d| (boot::INITRD_ADDR, boot::INITRD_ADDR + d.len() as u32));
        let blob = boot::prepare_device_tree(&dtb, ram as u64, cmdline, range)?;
        std::fs::write(path, blob).map_err(|e| e.to_string())?;
    }
    eprintln!(
        "[tinkerboard] experimental Rockchip RK3288: four Cortex-A12 (interpreter), GIC-400, UART2, \
         CRU/GRF/PMU, boot ROM SMP mailbox; direct kernel boot with the stock DTB; other devices are stubs"
    );
    serial_board::run(config, make_machine)
}
