//! `sbc-emu` command line.
//!
//!   sbc-emu boards list
//!   sbc-emu mkinitramfs --busybox PATH -o OUT.cpio
//!   sbc-emu run --board pi4b --kernel Image [--initrd X.cpio] [--append ARGS] [--max-insns N]
mod cpio;
mod disk;

use bcm2711::{Machine, Stop};
use std::io::{Read, Write};
use std::process::ExitCode;
use std::sync::mpsc;
use std::time::{Duration, Instant};

const BOARDS: &[(&str, &str)] = &[
    ("pi4b", "experimental: BCM2711 reduced (1x A72, GIC-400, PL011 UART0, generic timer)"),
    ("pi3b", "unverified: BCM2837 SoC model not yet implemented"),
];

fn arg<'a>(args: &'a [String], name: &str) -> Option<&'a str> {
    args.iter().position(|a| a == name).and_then(|i| args.get(i + 1)).map(String::as_str)
}

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.first().map(String::as_str) {
        Some("boards") => {
            for (id, status) in BOARDS { println!("{id:<8} {status}"); }
            ExitCode::SUCCESS
        }
        Some("mkinitramfs") => {
            let (Some(bb), Some(out)) = (arg(&args, "--busybox"), arg(&args, "-o")) else { return usage() };
            let busybox = match std::fs::read(bb) { Ok(b) => b, Err(e) => { eprintln!("{bb}: {e}"); return ExitCode::FAILURE } };
            std::fs::write(out, cpio::busybox_initramfs(&busybox)).unwrap();
            ExitCode::SUCCESS
        }
        Some("run") => run(&args),
        Some("sd") => sd(&args),
        _ => usage(),
    }
}

fn sd(args: &[String]) -> ExitCode {
    let (Some(cmd), Some(img)) = (args.get(1), args.get(2)) else { return usage() };
    let mut d = match disk::CowFile::open(img) { Ok(d) => d, Err(e) => { eprintln!("{img}: {e}"); return ExitCode::FAILURE } };
    let parts = bcm2711::fat::mbr_partitions(&mut d);
    let Some(&(_, lba, _)) = parts.iter().find(|p| matches!(p.0, 0x0b | 0x0c | 0x0e | 0x06 | 0x04)) else { eprintln!("no FAT partition"); return ExitCode::FAILURE };
    let mut fat = bcm2711::fat::Fat::open(&mut d, lba).expect("FAT");
    match cmd.as_str() {
        "ls" => {
            let path = args.get(3).map(String::as_str).unwrap_or("/");
            let cl = if path == "/" { 0 } else { match fat.find(path) { Some(e) => e.cluster, None => { eprintln!("{path}: not found"); return ExitCode::FAILURE } } };
            for e in fat.list_dir(cl) { println!("{:>10} {}{}", e.size, e.name, if e.dir { "/" } else { "" }); }
        }
        "cat" => {
            let Some(path) = args.get(3) else { return usage() };
            match fat.read_file(path) { Some(b) => { let _ = std::io::stdout().write_all(&b); } None => { eprintln!("{path}: not found"); return ExitCode::FAILURE } }
        }
        _ => return usage(),
    }
    ExitCode::SUCCESS
}

fn usage() -> ExitCode {
    eprintln!("usage:\n  sbc-emu boards list\n  sbc-emu sd ls|cat IMAGE [PATH]\n  sbc-emu mkinitramfs --busybox PATH -o OUT.cpio\n  sbc-emu run --board pi4b --kernel Image [--initrd FILE] [--append ARGS] [--max-insns N] [--timeout SECS] [--screenshot OUT.png] [--shell serial|fb] [--input serial|usb] [--script FILE] [--restore SNAPSHOT] [--net nat|local|none] [--no-jit] [--ram-mb N] [--no-display]\n  sbc-emu run --board pi4b --sd IMAGE.img [same options]   (boots like the firmware: config.txt, cmdline.txt, kernel8.img)");
    ExitCode::from(2)
}


/// Restores the terminal mode on drop.
struct RawTty(bool);
impl RawTty {
    fn enable() -> Self {
        let tty = std::process::Command::new("stty").args(["raw", "-echo"]).stdin(std::process::Stdio::inherit()).status().is_ok_and(|s| s.success());
        RawTty(tty)
    }
}
impl Drop for RawTty {
    fn drop(&mut self) {
        if self.0 { let _ = std::process::Command::new("stty").arg("sane").stdin(std::process::Stdio::inherit()).status(); }
    }
}

fn run(args: &[String]) -> ExitCode {
    let board = arg(args, "--board").unwrap_or("pi4b");
    if board != "pi4b" {
        eprintln!("board '{board}' is not implemented (see `sbc-emu boards list`)");
        return ExitCode::from(2);
    }
    let kpath = arg(args, "--kernel");
    let sd_path = arg(args, "--sd");
    if kpath.is_none() && sd_path.is_none() { return usage(); }
    let kernel = match kpath.map(std::fs::read) { Some(Ok(k)) => Some(k), Some(Err(e)) => { eprintln!("{}: {e}", kpath.unwrap()); return ExitCode::FAILURE } None => None };
    let initrd = match arg(args, "--initrd") {
        Some(p) => match std::fs::read(p) { Ok(d) => Some(d), Err(e) => { eprintln!("{p}: {e}"); return ExitCode::FAILURE } },
        None => None,
    };
    let shell = arg(args, "--shell").unwrap_or("serial");
    let append = arg(args, "--append").map(str::to_string);
    let timeout = arg(args, "--timeout").and_then(|s| s.parse::<f64>().ok()).map(Duration::from_secs_f64);
    let max = arg(args, "--max-insns").and_then(|s| s.parse::<u64>().ok()).unwrap_or(u64::MAX);
    let screenshot = arg(args, "--screenshot");
    let ram_mb = arg(args, "--ram-mb").and_then(|s| s.parse::<usize>().ok()).unwrap_or(1024);
    let display = !args.iter().any(|a| a == "--no-display");
    let trace_sd = args.iter().any(|a| a == "--trace-sd");
    let trace_net = args.iter().any(|a| a == "--trace-net");
    let use_jit = !args.iter().any(|a| a == "--no-jit");
    let net_mode = arg(args, "--net").unwrap_or("nat");
    let usb_input = arg(args, "--input") == Some("usb");
    let mut script: std::collections::VecDeque<String> = match arg(args, "--script") {
        Some(p) => match std::fs::read_to_string(p) { Ok(t) => t.lines().map(str::trim).filter(|l| !l.is_empty() && !l.starts_with('#')).map(str::to_string).collect(), Err(e) => { eprintln!("{p}: {e}"); return ExitCode::FAILURE } },
        None => Default::default(),
    };
    let mut serial_log = String::new();
    let mut script_wait_until: Option<Instant> = None;
    // last wall time the guest ran a full busy slice (for the wait-idle script step)
    let mut last_busy = Instant::now();
    let disk: Option<Box<dyn bcm2711::sdhci::Disk>> = match sd_path {
        Some(p) => match disk::CowFile::open(p) { Ok(d) => Some(Box::new(d)), Err(e) => { eprintln!("{p}: {e}"); return ExitCode::FAILURE } },
        None => None,
    };

    // power-on: firmware-style boot from the SD card unless a kernel is given explicitly
    let power_on = |mut disk: Option<Box<dyn bcm2711::sdhci::Disk>>| -> Result<Machine, String> {
        let mut m = Machine::new(ram_mb << 20);
        let (k, i, c) = match (&kernel, disk.as_mut()) {
            (Some(k), _) => (k.clone(), initrd.clone(), append.clone().unwrap_or_else(|| format!("{} sbcemu.shell={shell}", bcm2711::boot::DEFAULT_CMDLINE))),
            (None, Some(d)) => {
                let b = bcm2711::boot::read_sd_boot(d.as_mut())?;
                for n in &b.notes { eprintln!("[sbc-emu] firmware: {n}"); }
                (b.kernel, b.initramfs, append.clone().unwrap_or(b.cmdline))
            }
            (None, None) => return Err("nothing to boot".into()),
        };
        m.soc.sd = bcm2711::sdhci::Sdhci::new(disk);
        m.soc.sd.trace = trace_sd;
        if use_jit { m.cpu.enable_jit(); }
        match net_mode { "none" => {}, "local" => m.connect_network(false, trace_net), _ => m.connect_network(true, trace_net) }
        m.soc.genet.trace = trace_net;
        m.soc.wifi.trace = args.iter().any(|a| a == "--trace-sdio");
        if let Some(c) = m.soc.wifi.sdio.as_mut() { c.set_trace(args.iter().any(|a| a == "--trace-wifi")); }
        bcm2711::boot::boot_linux(&mut m, &k, i.as_deref(), &c, display)?;
        Ok(m)
    };
    let mut m = match power_on(disk) { Ok(m) => m, Err(e) => { eprintln!("boot: {e}"); return ExitCode::FAILURE } };
    let mut clock_offset = Duration::ZERO;
    if let Some(p) = arg(args, "--restore") {
        let snap = match std::fs::read(p) { Ok(b) => b, Err(e) => { eprintln!("{p}: {e}"); return ExitCode::FAILURE } };
        if let Err(e) = m.restore(&snap) { eprintln!("{p}: {e}"); return ExitCode::FAILURE; }
        clock_offset = Duration::from_nanos(m.now);
        eprintln!("[sbc-emu] restored {p}: guest time {:.1} s, {} instructions", m.now as f64 / 1e9, m.cpu.instret);
    }

    // console input: a thread forwards stdin bytes
    let (tx, rx) = mpsc::channel::<u8>();
    std::thread::spawn(move || {
        let mut b = [0u8; 1];
        let mut stdin = std::io::stdin();
        while let Ok(1) = stdin.read(&mut b) { if tx.send(b[0]).is_err() { break; } }
    });
    let tty = RawTty::enable();
    let mut out = std::io::stdout();
    // the wall clock is anchored so a restored guest's time continues where it was saved
    let start = Instant::now() - clock_offset;
    let started = Instant::now();
    let mut escape = false;
    let mut pending: std::collections::VecDeque<u8> = Default::default();
    let result = loop {
        // input: Ctrl-A x quits
        while let Ok(b) = rx.try_recv() {
            if escape { escape = false; if b == b'x' { break; } if b != 1 { pending.push_back(1); } }
            if b == 1 { escape = true; continue; }
            pending.push_back(b);
        }
        if escape && false { break Stop::Budget; }
        if usb_input {
            // keystrokes go to the USB keyboard; terminal escape sequences become arrow keys
            while m.soc.usb.kbd.queued() < 64 {
                let Some(b) = pending.pop_front() else { break };
                if b == 0x1b && pending.front() == Some(&b'[') && pending.len() >= 2 {
                    pending.pop_front();
                    let k = pending.pop_front().unwrap();
                    let usage = match k { b'A' => 0x52, b'B' => 0x51, b'C' => 0x4f, b'D' => 0x50, b'H' => 0x4a, b'F' => 0x4d, _ => 0 };
                    if usage != 0 { m.soc.usb.kbd.tap(usage, 0); }
                } else {
                    m.soc.usb.kbd.type_byte(b);
                }
            }
        } else {
            while m.soc.uart.rx_room() { match pending.pop_front() { Some(b) => { m.soc.uart.push_rx(b); m.soc.dirty = true; } None => break } }
        }

        if timeout.is_some_and(|t| started.elapsed() >= t) { break Stop::Budget; }
        // action script: one step at a time
        if script_wait_until.is_none_or(|t| Instant::now() >= t) {
            script_wait_until = None;
            if let Some(line) = script.front().cloned() {
                let (cmd, rest) = line.split_once(' ').unwrap_or((line.as_str(), ""));
                let unescape = |t: &str| t.replace("\\n", "\r").replace("\\r", "\r").replace("\\t", "\t").replace("\\e", "\x1b");
                let mut done = true;
                match cmd {
                    "wait-serial" => done = serial_log.contains(rest),
                    "wait-idle" => done = last_busy.elapsed().as_secs_f64() >= rest.parse().unwrap_or(5.0),
                    "sleep" => { script_wait_until = Some(Instant::now() + Duration::from_secs_f64(rest.parse().unwrap_or(1.0))); }
                    "type" => for b in unescape(rest).bytes() { m.soc.usb.kbd.type_byte(b); },
                    "serial" => pending.extend(unescape(rest).bytes()),
                    "shot" => {
                        let mut rgba = Vec::new();
                        if let Some((w, h)) = m.soc.mbox.render(&m.soc.ram, &mut rgba) { let _ = std::fs::write(rest, bcm2711::png::encode_rgba(w, h, &rgba)); }
                    }
                    "quit" => { script.pop_front(); break Stop::Budget; }
                    "save" => { let snap = m.save(); eprintln!("\r[script] snapshot {} MiB -> {rest}\r", snap.len() >> 20); let _ = std::fs::write(rest, snap); }
                    _ => eprintln!("[script] unknown command: {line}"),
                }
                if done {
                    eprintln!("\r[script] t={:.1}s wall={:.1}s {line}\r", m.now as f64 / 1e9, started.elapsed().as_secs_f64());
                    script.pop_front();
                    if cmd == "wait-serial" { serial_log.clear(); }
                }
            }
        }
        let budget = 2_000_000u64.min(max.saturating_sub(m.cpu.instret));
        if budget == 0 { break Stop::Budget; }
        let stop = m.run(budget);
        if !script.is_empty() {
            serial_log.push_str(&String::from_utf8_lossy(&m.soc.uart.out));
            if serial_log.len() > 1 << 20 { serial_log.drain(..serial_log.len() - (1 << 19)); }
        }
        flush(&mut m, &mut out, tty.0);
        match stop {
            Stop::Budget => last_busy = Instant::now(),
            Stop::Idle(t) => {
                // real-time pacing: idle time may catch up with the wall clock but never run ahead
                let wall = start.elapsed().as_nanos() as u64;
                let target = t.min(wall.max(m.now));
                if target <= m.now {
                    let wait = Duration::from_nanos((t - m.now).min(20_000_000));
                    if let Ok(b) = rx.recv_timeout(wait) {
                        if b == 1 { escape = true } else { pending.push_back(b) }
                    }
                    let wall = start.elapsed().as_nanos() as u64;
                    m.idle_until(t.min(wall));
                } else {
                    m.idle_until(target);
                }
            }
            Stop::Reset => {
                eprintln!("\r\n[sbc-emu] PSCI SYSTEM_RESET: rebooting\r");
                let d = m.soc.sd.disk.take();
                m = match power_on(d) { Ok(m) => m, Err(e) => { eprintln!("reboot: {e}"); break Stop::Reset } };
            }
            other => break other,
        }
    };
    flush(&mut m, &mut out, tty.0);
    drop(tty);
    if let Some(path) = screenshot {
        let mut rgba = Vec::new();
        match m.soc.mbox.render(&m.soc.ram, &mut rgba) {
            Some((w, h)) => { let _ = std::fs::write(path, bcm2711::png::encode_rgba(w, h, &rgba)); eprintln!("[sbc-emu] screenshot {w}x{h} -> {path}"); }
            None => eprintln!("[sbc-emu] no framebuffer allocated: no screenshot"),
        }
    }
    if !m.soc.mbox.unknown.is_empty() { eprintln!("[sbc-emu] unanswered mailbox tags: {:x?}", m.soc.mbox.unknown); }
    eprintln!("\n[sbc-emu] stopped: {:?} pc={:#x} insns={} sim={:.3}s wall={:.1}s exceptions={} irqs={}",
        result, m.cpu.pc, m.cpu.instret, m.now as f64 / 1e9, started.elapsed().as_secs_f64(), m.exceptions, m.irqs);
    if let Some(j) = m.cpu.jit.as_ref() { eprintln!("[sbc-emu] jit: {} blocks compiled, {} flushes", j.compiled, j.flushes); }
    if !m.cpu.sys.unknown.is_empty() {
        let names: Vec<String> = m.cpu.sys.unknown.iter().map(|k| format!("S{}_{}_C{}_C{}_{}", k >> 14, (k >> 11) & 7, (k >> 7) & 15, (k >> 3) & 15, k & 7)).collect();
        eprintln!("[sbc-emu] unmodelled system registers: {}", names.join(" "));
    }
    match result { Stop::Budget => ExitCode::SUCCESS, _ => ExitCode::FAILURE }
}

fn flush(m: &mut Machine, out: &mut std::io::Stdout, raw: bool) {
    if m.soc.uart.out.is_empty() { return; }
    let bytes = std::mem::take(&mut m.soc.uart.out);
    if raw {
        // in raw mode the terminal needs CR for each LF the guest did not send itself
        let _ = out.write_all(&bytes);
    } else {
        let _ = out.write_all(&bytes);
    }
    let _ = out.flush();
}
