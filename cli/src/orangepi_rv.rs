//! Orange Pi RV (StarFive JH7110) serial bring-up adapter: OpenSBI `fw_dynamic` and a
//! RISC-V Linux `Image` with the distribution's device tree, loaded as the board's U-Boot
//! would load them.
use crate::{arg, serial_board::{self, RunConfig, SerialBoard}};
use sbc_machine::Machine as HostMachine;
use starfive_jh7110::{boot, HartState, Machine};
use std::process::ExitCode;

impl SerialBoard for Machine {
    fn host(&mut self) -> HostMachine<'_> {
        HostMachine::OrangePiRv(self)
    }
    fn now(&self) -> u64 {
        Machine::now(self)
    }
    fn instret(&self) -> u64 {
        Machine::instret(self)
    }
    fn report(&self, tag: &str) {
        eprintln!("[{tag}] insns={} virtual={:.3}s", Machine::instret(self), self.now() as f64 / 1e9);
        for (h, hart) in self.harts.iter().enumerate() {
            if self.state[h] == HartState::Off {
                continue;
            }
            eprintln!(
                "[{tag}] hart {h}: pc={:#x} prv={} insns={} traps={} satp={:#x} sepc={:#x} scause={:#x} stval={:#x} mepc={:#x} mcause={:#x}{}",
                hart.pc, hart.prv, hart.instret, hart.traps, hart.satp, hart.sepc, hart.scause, hart.stval, hart.mepc, hart.mcause,
                if hart.wfi { " (wfi)" } else { "" }
            );
            if let Some((pc, insn)) = hart.last_illegal {
                eprintln!("[{tag}] hart {h}: last illegal instruction {insn:#010x} at {pc:#x}");
            }
        }
        if !self.soc.stubs.is_empty() {
            let pages: Vec<String> = self.soc.stubs.keys().map(|p| format!("{p:#x}")).collect();
            eprintln!("[{tag}] unmodelled device pages (read-back stubs): {}", pages.join(" "));
        }
    }
}

pub(crate) fn run(args: &[String]) -> ExitCode {
    match execute(args) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("[orangepi-rv] {e}");
            ExitCode::FAILURE
        }
    }
}

fn execute(args: &[String]) -> Result<(), String> {
    // --smp N (1-4) starts N of the four U74 harts; the shared validation allows only 1.
    let smp: usize = arg(args, "--smp").map_or(Ok(4), |s| s.parse().map_err(|_| "--smp 1|2|3|4"))?;
    if !(1..=4).contains(&smp) {
        return Err("--smp 1|2|3|4 (the JH7110 has four U74 application harts)".into());
    }
    let mut checked: Vec<String> = Vec::new();
    let mut it = args.iter();
    while let Some(a) = it.next() {
        if a == "--smp" {
            it.next();
        } else {
            checked.push(a.clone());
        }
    }
    serial_board::validate_options(&checked, &["--dtb", "--dump-dtb", "--firmware"])?;
    let read = |path: &str| std::fs::read(path).map_err(|e| format!("{path}: {e}"));
    let kernel = read(arg(args, "--kernel").ok_or("--kernel Image is required")?)?;
    let dtb = read(arg(args, "--dtb").ok_or("--dtb jh7110-orangepi-rv.dtb is required")?)?;
    let firmware = read(arg(args, "--firmware").ok_or("--firmware fw_dynamic.bin (OpenSBI, generic platform) is required")?)?;
    let initrd = arg(args, "--initrd").map(read).transpose()?;
    let cmdline = arg(args, "--append").unwrap_or(boot::DEFAULT_CMDLINE);
    let ram = serial_board::ram_bytes(args, starfive_jh7110::RAM_SIZE >> 20)?;
    if ram < (128 << 20) || ram > (16 << 30) {
        return Err("--ram-mb must be between 128 and 16384".into());
    }
    let config = RunConfig::from_args("orangepi-rv", args)?;
    let make_machine = || -> Result<Machine, String> {
        let mut m = Machine::new(ram);
        boot::boot_linux(&mut m, &firmware, &kernel, &dtb, initrd.as_deref(), cmdline)?;
        for h in smp + 1..m.harts.len() {
            m.state[h] = HartState::Off;
        }
        Ok(m)
    };
    if let Some(path) = arg(args, "--dump-dtb") {
        let range = initrd.as_ref().map(|d| (boot::INITRD_ADDR, boot::INITRD_ADDR + d.len() as u64));
        let blob = boot::prepare_device_tree(&dtb, ram as u64, cmdline, range)?;
        std::fs::write(path, blob).map_err(|e| format!("{path}: {e}"))?;
    }
    eprintln!(
        "[orangepi-rv] experimental StarFive JH7110: {smp} of 4 U74 harts (RV64GC interpreter, Sv39), CLINT, PLIC, UART0, \
         clock/reset generators; OpenSBI from --firmware; other devices are stubs"
    );
    serial_board::run(config, make_machine)
}
