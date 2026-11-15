use allwinner_h3::Machine;
use emu_core::Trap;
use std::io::{self, Write};
use std::path::PathBuf;
use std::process::ExitCode;

const RAM_BYTES: usize = 512 << 20;
const ZIMAGE_ADDR: u64 = 0x4200_0000;
const DTB_ADDR: u64 = 0x4800_0000;
const INITRD_ADDR: u64 = 0x4900_0000;

struct Args {
    kernel: PathBuf,
    dtb: PathBuf,
    initrd: PathBuf,
    steps: u64,
    stop_on: Option<String>,
}

fn usage() -> &'static str {
    "Usage: h3-boot [--kernel PATH] [--dtb PATH] [--initrd PATH] [--steps N] [--stop-on TEXT]\n\
     Defaults to the checked-out cache/h3-boot artifacts. Serial output goes to stdout."
}

fn args() -> Result<Args, String> {
    let mut parsed = Args {
        kernel: "cache/h3-boot/kernel/boot/vmlinuz-lts".into(),
        dtb: "cache/h3-boot/sun8i-h3-orangepi-one.dtb".into(),
        initrd: "cache/h3-boot/initramfs.cpio.gz".into(),
        steps: 100_000_000,
        stop_on: None,
    };
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        if arg == "--help" {
            println!("{}", usage());
            std::process::exit(0);
        }
        let value = args.next().ok_or_else(|| format!("{arg} requires a value"))?;
        match arg.as_str() {
            "--kernel" => parsed.kernel = value.into(),
            "--dtb" => parsed.dtb = value.into(),
            "--initrd" => parsed.initrd = value.into(),
            "--steps" => {
                parsed.steps = value
                    .parse()
                    .map_err(|_| format!("invalid step budget: {value}"))?
            }
            "--stop-on" => parsed.stop_on = Some(value),
            _ => return Err(format!("unknown argument: {arg}")),
        }
    }
    Ok(parsed)
}

fn run() -> Result<(), String> {
    let args = args()?;
    let kernel = std::fs::read(&args.kernel)
        .map_err(|error| format!("cannot read {}: {error}", args.kernel.display()))?;
    let dtb = std::fs::read(&args.dtb)
        .map_err(|error| format!("cannot read {}: {error}", args.dtb.display()))?;
    let initrd = std::fs::read(&args.initrd)
        .map_err(|error| format!("cannot read {}: {error}", args.initrd.display()))?;
    let mut machine = Machine::with_ram_size(RAM_BYTES);
    machine.load(ZIMAGE_ADDR, &kernel).map_err(|error| format!("load zImage: {error:?}"))?;
    machine.load(DTB_ADDR, &dtb).map_err(|error| format!("load DTB: {error:?}"))?;
    machine.load(INITRD_ADDR, &initrd).map_err(|error| format!("load initrd: {error:?}"))?;
    machine.boot_linux(ZIMAGE_ADDR as u32, DTB_ADDR as u32);

    let mut stdout = io::stdout().lock();
    let mut transcript = Vec::new();
    let mut steps = 0;
    let mut exception_streak = 0u32;
    while steps < args.steps {
        let mut storm = false;
        for _ in 0..10_000.min(args.steps - steps) {
            steps += 1;
            match machine.step() {
                Ok(()) | Err(Trap::Wait) => exception_streak = 0,
                Err(Trap::Exception(..)) => {
                    exception_streak += 1;
                    if exception_streak >= 10_000 {
                        storm = true;
                        break;
                    }
                }
                Err(Trap::Interrupt(..)) => exception_streak = 0,
                Err(trap) => {
                    let output = machine.take_serial_output();
                    stdout.write_all(&output).ok();
                    stdout.flush().ok();
                    transcript.extend_from_slice(&output);
                    return Err(format!(
                        "stopped after {steps} steps / {} retired instructions: {trap:?}\n\
                         pc={:#010x} cpsr={:#010x} r={:08x?}",
                        machine.cpu.instret,
                        machine.cpu.r[15],
                        machine.cpu.cpsr,
                        machine.cpu.r
                    ));
                }
            }
        }
        let output = machine.take_serial_output();
        stdout.write_all(&output).map_err(|error| error.to_string())?;
        stdout.flush().map_err(|error| error.to_string())?;
        transcript.extend_from_slice(&output);
        if let Some(marker) = &args.stop_on {
            if transcript
                .windows(marker.len())
                .any(|window| window == marker.as_bytes())
            {
                eprintln!("\n[h3-boot] found {marker:?} after {steps} steps");
                return Ok(());
            }
        }
    }
    eprintln!(
        "\n[h3-boot] step budget exhausted: {steps} steps, {} retired instructions, pc={:#010x}",
        machine.cpu.instret, machine.cpu.r[15]
    );
    Ok(())
}

fn main() -> ExitCode {
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("\n[h3-boot] {error}\n{}", usage());
            ExitCode::FAILURE
        }
    }
}
