//! ESP32-S31-Function-CoreBoard-1 adapter: Espressif's mask ROM ELF and a flash image as
//! `esptool write-flash 0x0` writes it (`scripts/esp32s31-fixture.sh`); the chip boots from its ROM.
use crate::{arg, serial_board::{self, RunConfig, SerialBoard}};
use ::esp32s31::Machine;
use esp_soc::SocBus as _;
use sbc_machine::Machine as HostMachine;
use std::process::ExitCode;

impl SerialBoard for Machine {
    fn host(&mut self) -> HostMachine<'_> {
        HostMachine::Esp32S31(self)
    }
    fn now(&self) -> u64 {
        sbc_machine::s31_now(self)
    }
    /// Retired instructions (the cores' own count includes the time they slept).
    fn instret(&self) -> u64 {
        self.cores.iter().map(|c| c.retired_count).sum()
    }
    fn report(&self, tag: &str) {
        eprintln!("[{tag}] insns={} virtual={:.3}s reboots={}", SerialBoard::instret(self), SerialBoard::now(self) as f64 / 1e9, self.reboots);
        for (k, c) in self.cores.iter().enumerate() {
            eprintln!("[{tag}] hart {k}: pc={:#x} prv={} insns={} regions={} satp={:#x} mepc={:#x} mcause={:#x} sepc={:#x} scause={:#x} stval={:#x}", c.pc, c.priv_mode, c.retired_count, c.blocks.compiled, c.satp, c.mepc, c.mcause, c.sepc, c.scause, c.stval);
        }
    }
}

pub(crate) fn run(args: &[String]) -> ExitCode {
    match execute(args) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("{e}");
            ExitCode::FAILURE
        }
    }
}

fn execute(args: &[String]) -> Result<(), String> {
    serial_board::validate_options(args, &["--rom", "--flash"])?;
    let read = |name: &str, what: &str| -> Result<Vec<u8>, String> {
        let path = arg(args, name).ok_or(format!("the ESP32-S31 needs {name} {what} (scripts/esp32s31-fixture.sh)"))?;
        std::fs::read(path).map_err(|e| format!("{path}: {e}"))
    };
    let rom = read("--rom", "esp32s31_rev0_rom.elf")?;
    let flash = read("--flash", "s31_full_flash.bin")?;
    let jit = !args.iter().any(|a| a == "--no-jit");
    let make_machine = || -> Result<Machine, String> {
        let mut m = ::esp32s31::machine([0x10, 0x20, 0x30, 0x40, 0x50, 0x60], 16 << 20);
        m.bus.write_flash(0, &flash)?;
        m.bus.set_board_inputs(::esp32s31::soc::BoardInputs::function_coreboard_1())?;
        m.bus.set_strap(0x10);
        m.bus.load_mask_rom(&rom)?;
        m.console.mask = 2;
        m.console.capture = true;
        m.boot_rom();
        m.cores[0].x[2] = ::esp32s31::ROM_STACK;
        for c in &mut m.cores {
            c.blocks.jit_on &= jit;
        }
        Ok(m)
    };
    serial_board::run(RunConfig::from_args("esp32s31", args)?, make_machine)
}
