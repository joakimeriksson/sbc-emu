use allwinner_h3::Machine;
use emu_core::Stop;
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
    "Usage: h3-boot [--kernel PATH] [--dtb PATH] [--initrd PATH] [--steps N] [--stop-on TEXT]\n\
     Defaults to the checked-out cache/h3-boot artifacts. Serial output goes to stdout."
}

fn args() -> Result<Args, String> {
    let mut parsed = Args {
        kernel: "cache/h3-boot/kernel/boot/vmlinuz-lts".into(),
        dtb: "cache/h3-boot/kernel/boot/dtbs-lts/sun8i-h3-orangepi-one.dtb".into(),
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
    let mut machine = Machine::new();
    let cmdline = std::env::var("H3_CMDLINE")
        .unwrap_or_else(|_| allwinner_h3::boot::DEFAULT_CMDLINE.to_string());
    allwinner_h3::boot::boot_linux(&mut machine, &kernel, &dtb, Some(&initrd), &cmdline)?;
    machine.soc.log_stub_pages = std::env::var_os("H3_LOG_STUBS").is_some();


    let mut stdout = io::stdout().lock();
    let mut transcript: Vec<u8> = Vec::new();
    loop {
        let retired = machine.instret();
        if retired >= args.steps {
            eprintln!("\n[h3-boot] instruction budget exhausted: {retired} retired");
            dump_kernel_image(&machine);
            for (k, cpu) in machine.cpus.iter().enumerate() {
                eprintln!(
                    "[h3-boot] core {k} {:?} pc={:#010x} lr={:#010x} cpsr={:#010x} irq={} insns={} last_undef={:x?}",
                    machine.state[k], cpu.r[15], cpu.r[14], cpu.cpsr, cpu.irq, cpu.instret, cpu.last_undef
                );
            }
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
                    "\n[h3-boot] found {marker:?} after {} instructions, {:.3} s virtual",
                    machine.instret(),
                    machine.now() as f64 / 1e9
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
            Stop::PowerOff => {
                eprintln!("\n[h3-boot] guest powered off");
                return Ok(());
            }
            other => {
                dump_kernel_image(&machine);
                let core = machine.last_stop_core;
                let cpu = &machine.cpus[core];
                return Err(format!(
                    "stopped after {} instructions on core {core}: {other:x?}\npc={:#010x} cpsr={:#010x} r={:08x?}",
                    machine.instret(), cpu.r[15], cpu.cpsr, cpu.r
                ));
            }
        }
    }
}

/// With H3_DUMP_KERNEL=PATH, write the first 32 MiB of RAM (the linear-mapped kernel
/// image at VA 0x8000_0000) for offline disassembly.
fn dump_kernel_image(machine: &Machine) {
    if let Some(path) = std::env::var_os("H3_DUMP_KERNEL") {
        let ram = &machine.soc.ram.mem;
        let _ = std::fs::write(&path, &ram[..ram.len().min(32 << 20)]);
        eprintln!("[h3-boot] kernel image dumped to {}", PathBuf::from(path).display());
    }
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
