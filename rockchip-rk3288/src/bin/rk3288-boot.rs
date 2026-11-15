//! RK3288 kernel bring-up probe: direct-boots a zImage + stock DTB + initramfs on the
//! four-core model and copies the serial console to stdout.
use emu_core::Stop;
use rockchip_rk3288::Machine;
use std::io::{self, Write};
use std::path::PathBuf;
use std::process::ExitCode;

struct Args {
    kernel: PathBuf,
    dtb: PathBuf,
    initrd: PathBuf,
    steps: u64,
    stop_on: Option<String>,
}

fn usage() -> &'static str {
    "Usage: rk3288-boot [--kernel PATH] [--dtb PATH] [--initrd PATH] [--steps N] [--stop-on TEXT]\n\
     Defaults to the scripts/tinker-board-fixture.sh output in cache/tinker-board. Serial output goes to stdout."
}

fn args() -> Result<Args, String> {
    let mut parsed = Args {
        kernel: "cache/tinker-board/zImage".into(),
        dtb: "cache/tinker-board/rk3288-tinker-s.dtb".into(),
        initrd: "cache/tinker-board/initramfs.cpio.gz".into(),
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
            "--stop-on" => {
                if value.is_empty() {
                    return Err("--stop-on needs a non-empty marker".into());
                }
                parsed.stop_on = Some(value)
            }
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
    let mut machine = Machine::new();
    let cmdline = std::env::var("RK3288_CMDLINE")
        .unwrap_or_else(|_| rockchip_rk3288::boot::DEFAULT_CMDLINE.to_string());
    rockchip_rk3288::boot::boot_linux(&mut machine, &kernel, &dtb, Some(&initrd), &cmdline)?;
    machine.soc.log_stub_pages = std::env::var_os("RK3288_LOG_STUBS").is_some();


    let mut stdout = io::stdout().lock();
    let mut transcript: Vec<u8> = Vec::new();
    loop {
        let retired = machine.instret();
        if retired >= args.steps {
            eprintln!(
                "\n[rk3288-boot] instruction budget exhausted: {retired} retired, pc={:#010x}",
                machine.cpu().r[15]
            );
            return Ok(());
        }
        let stop = machine.run((args.steps - retired).min(1_000_000));
        let output = machine.take_serial_output();
        stdout.write_all(&output).map_err(|error| error.to_string())?;
        stdout.flush().map_err(|error| error.to_string())?;
        if let Some(marker) = &args.stop_on {
            // Only the tail can contain a new match.
            let keep = transcript.len().saturating_sub(marker.len());
            transcript.drain(..keep);
            transcript.extend_from_slice(&output);
            if transcript.windows(marker.len()).any(|window| window == marker.as_bytes()) {
                eprintln!(
                    "\n[rk3288-boot] found {marker:?} after {} instructions, {:.3} s virtual",
                    machine.instret(),
                    machine.now() as f64 / 1e9
                );
                eprintln!(
                    "[rk3288-boot] irqs={} exceptions={} last undefined instruction (pc, word)={:x?}",
                    machine.irqs, machine.exceptions, machine.cpu().last_undef
                );
                dump_kernel_image(&machine);
                return Ok(());
            }
        }
        match stop {
            Stop::Budget => {}
            // Fast-forward idle time: no wall-clock pacing in this probe.
            Stop::Idle(deadline) => {
                let target = if deadline == u64::MAX { machine.now() + 1_000_000 } else { deadline };
                machine.idle_until(target);
            }
            Stop::Reset => {
                eprintln!("\n[rk3288-boot] guest reset (CRU global soft reset)");
                return Ok(());
            }
            Stop::PowerOff => {
                eprintln!("\n[rk3288-boot] guest powered off");
                return Ok(());
            }
            other => {
                dump_kernel_image(&machine);
                let core = machine.last_stop_core;
                let c = &machine.cpus[core];
                return Err(format!(
                    "stopped after {} instructions: {other:x?}\ncore {core} pc={:#010x} cpsr={:#010x} r={:08x?}",
                    machine.instret(), c.r[15], c.cpsr, c.r
                ));
            }
        }
    }
}

/// With RK3288_DUMP_KERNEL=PATH, write the first 32 MiB of RAM (the linear-mapped kernel
/// image at VA 0xc000_0000 with DRAM at 0) for offline disassembly.
fn dump_kernel_image(machine: &Machine) {
    if let Some(path) = std::env::var_os("RK3288_DUMP_KERNEL") {
        let ram = &machine.soc.ram.mem;
        let _ = std::fs::write(&path, &ram[..ram.len().min(32 << 20)]);
        eprintln!("[rk3288-boot] kernel image dumped to {}", PathBuf::from(path).display());
    }
}

fn main() -> ExitCode {
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("\n[rk3288-boot] {error}\n{}", usage());
            ExitCode::FAILURE
        }
    }
}
