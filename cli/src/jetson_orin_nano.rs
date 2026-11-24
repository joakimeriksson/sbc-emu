//! Jetson Orin Nano direct Linux boot through the headless serial runner.
use crate::serial_board::{self, RunConfig, SerialBoard};
use sbc_machine::Machine as HostMachine;
use std::process::ExitCode;
use tegra234::{boot, Machine};

const TAG: &str = "jetson-orin-nano-4gb";

impl SerialBoard for Machine {
    fn host(&mut self) -> HostMachine<'_> {
        HostMachine::JetsonOrinNano(self)
    }

    fn now(&self) -> u64 {
        Machine::now(self)
    }

    fn instret(&self) -> u64 {
        Machine::instret(self)
    }

    fn report(&self, tag: &str) {
        eprintln!(
            "[{tag}] pc={:#x} el={} insns={} virtual={:.3}s irqs={}",
            self.cpu.pc,
            self.cpu.el,
            self.instret(),
            self.now() as f64 / 1e9,
            self.interrupts,
        );
    }
}

pub(crate) fn run(args: &[String]) -> ExitCode {
    match execute(args) {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            eprintln!("[{TAG}] {err}");
            ExitCode::FAILURE
        }
    }
}

fn execute(args: &[String]) -> Result<(), String> {
    serial_board::validate_options(args, &[])?;
    let config = RunConfig::from_args(TAG, args)?;
    let ram = serial_board::ram_bytes(args, 4096)?;
    let read = |path: &str| std::fs::read(path).map_err(|err| format!("{path}: {err}"));
    let kernel = read(crate::arg(args, "--kernel").ok_or("expected --kernel IMAGE")?)?;
    let initrd = crate::arg(args, "--initrd").map(read).transpose()?;
    let cmdline = crate::arg(args, "--append").unwrap_or(boot::DEFAULT_CMDLINE);
    let no_jit = args.iter().any(|arg| arg == "--no-jit")
        || std::env::var("SBC_JIT").is_ok_and(|v| v == "0");
    let announced = std::cell::Cell::new(false);
    serial_board::run(config, || {
        let mut machine = Machine::new(ram)?;
        machine.cpu.jit_off = no_jit;
        let jit = !no_jit && machine.cpu.enable_jit();
        if !announced.replace(true) {
            eprintln!(
                "[{TAG}] experimental Tegra234: one Cortex-A78AE core ({}), GICv3, ARM timer and SBSA UART",
                crate::cpu_backend(jit),
            );
        }
        boot::boot_linux(&mut machine, &kernel, initrd.as_deref(), cmdline)?;
        Ok(machine)
    })
}
