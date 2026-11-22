//! Transitional Pi 4 host adapter. Owns board-specific setup, reset, scripts and optional devices.
//! Shared run/idle/serial operations go through sbc-machine; CPU and device loops stay in BCM2711.
use crate::{arg, usage, disk, gdb, monitor, vnc, wav};
use bcm2711::Machine;
use crate::terminal::RawTty;
use sbc_machine::{Machine as HostMachine, Stop};
use std::io::{Read, Write};
use std::process::ExitCode;
use std::sync::mpsc;
use std::time::{Duration, Instant};

pub(crate) fn run(args: &[String]) -> ExitCode {
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
    let smp = arg(args, "--smp").and_then(|s| s.parse::<usize>().ok()).unwrap_or(4);
    let display = !args.iter().any(|a| a == "--no-display");
    let trace_sd = args.iter().any(|a| a == "--trace-sd");
    let trace_net = args.iter().any(|a| a == "--trace-net");
    let use_jit = !args.iter().any(|a| a == "--no-jit");
    let gpio_log = args.iter().any(|a| a == "--gpio-log");
    let net_mode = arg(args, "--net").unwrap_or("nat");
    let usb_input = arg(args, "--input") == Some("usb");
    let mut gdb = match arg(args, "--gdb").map(|p| p.trim_start_matches("tcp::").parse::<u16>()) {
        Some(Ok(port)) => match gdb::Gdb::listen(port) { Ok(g) => Some(g), Err(e) => { eprintln!("--gdb {port}: {e}"); return ExitCode::FAILURE } },
        Some(Err(_)) => { eprintln!("--gdb needs a port number"); return ExitCode::FAILURE }
        None => None,
    };
    let gdb_wait = args.iter().any(|a| a == "--gdb-wait");
    let mut vnc = match arg(args, "--vnc") {
        Some(spec) => match vnc::Vnc::listen(spec) { Ok(v) => Some(v), Err(e) => { eprintln!("--vnc {spec}: {e}"); return ExitCode::FAILURE } },
        None => None,
    };
    // the mini UART (ttyS0): its output goes to a file
    let mut serial1 = match arg(args, "--serial1") {
        Some(p) => match std::fs::File::create(p) { Ok(f) => Some(f), Err(e) => { eprintln!("{p}: {e}"); return ExitCode::FAILURE } },
        None => None,
    };
    let mut script = match crate::script::load(arg(args, "--script")) {
        Ok(script) => script,
        Err(e) => { eprintln!("{}: {e}", arg(args, "--script").unwrap_or("")); return ExitCode::FAILURE; }
    };
    let mut serial_log = String::new();
    let mut script_wait_until: Option<Instant> = None;
    // last wall time the guest ran a full busy slice (for the wait-idle script step)
    let mut last_busy = Instant::now();
    let disk: Option<Box<dyn bcm2711::sdhci::Disk>> = match sd_path {
        Some(p) => match if args.iter().any(|a| a == "--sd-persist") { disk::CowFile::open_persistent(p) } else { disk::CowFile::open(p) }.and_then(disk::CowFile::as_card) {
            Ok(d) => Some(Box::new(d)), Err(e) => { eprintln!("{p}: {e}"); return ExitCode::FAILURE } },
        None => None,
    };

    // --usb-drive IMAGE: a USB stick on the third USB-A port (copy-on-write, like the SD card)
    let usb_drive: Option<Box<dyn bcm2711::sdhci::Disk + Send>> = match arg(args, "--usb-drive") {
        Some(p) => match disk::CowFile::open(p) { Ok(d) => Some(Box::new(d)), Err(e) => { eprintln!("{p}: {e}"); return ExitCode::FAILURE } },
        None => None,
    };
    // --audio-out FILE.wav: what the guest plays on the I²S DAC
    let mut audio_out = match arg(args, "--audio-out") {
        Some(p) => match wav::Wav::create(p) { Ok(w) => Some(w), Err(e) => { eprintln!("{p}: {e}"); return ExitCode::FAILURE } },
        None => None,
    };
    // --otg-keyboard: the keyboard on the USB-C (DWC2 OTG) port, as before the xHCI existed
    let otg_keyboard = args.iter().any(|a| a == "--otg-keyboard");

    // power-on: firmware-style boot from the SD card unless a kernel is given explicitly
    let power_on = |mut disk: Option<Box<dyn bcm2711::sdhci::Disk>>, usb_drive: Option<Box<dyn bcm2711::sdhci::Disk + Send>>| -> Result<Machine, String> {
        let mut m = Machine::with_cpus(ram_mb << 20, smp);
        {
            use bcm2711::usbdev::DevId;
            let mut ports = vec![DevId::Keyboard, DevId::Tablet];
            if otg_keyboard { ports.remove(0); m.soc.usb.port = Some(DevId::Keyboard); }
            if let Some(d) = usb_drive { m.soc.devs.storage = Some(bcm2711::usbdev::Storage::new(d)); ports.push(DevId::Storage); }
            // Bluetooth on the mini UART (dtoverlay=miniuart-bt), unless that UART is the
            // --serial1 port or --no-bluetooth
            if arg(args, "--serial1").is_none() && !args.iter().any(|a| a == "--no-bluetooth") { m.soc.bt = Some(bcm2711::bluetooth::Bluetooth::new()); }
            // --display kms (default): the vc4 HDMI pipeline for the KMS driver, as the image's
            // dtoverlay=vc4-kms-v3d asks; fb: the firmware's framebuffer only
            if arg(args, "--display") != Some("fb") {
                // --hdmi-mode WxH: the monitor's preferred mode (default 1920x1080)
                let (w, h) = arg(args, "--hdmi-mode").and_then(|s| s.split_once('x')).and_then(|(a, b)| Some((a.parse().ok()?, b.parse().ok()?))).unwrap_or((1920, 1080));
                m.soc.vc4 = Some(bcm2711::vc4::Vc4::with_mode(w, h));
                // and the V3D 3D GPU next to it (dtoverlay=vc4-kms-v3d), unless --no-gpu
                if !args.iter().any(|a| a == "--no-gpu") { m.soc.v3d = Some(v3d::V3d::new()); }
            }
            // --camera: a USB webcam (UVC, 640x480 YUYV, a test pattern)
            if args.iter().any(|a| a == "--camera") { m.soc.devs.camera = Some(bcm2711::usbdev::Camera::new()); ports.push(DevId::Camera); }
            m.soc.pcie.xhci.attach(&ports);
        }
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
        if use_jit { m.enable_jit(); }
        if args.iter().any(|a| a == "--cpu-crypto") { m.set_crypto(true); }
        // --threads multi: a host thread per core (faster, not repeatable); single (default): turns
        m.multi = arg(args, "--threads") == Some("multi");
        // the hardware RNG: --seed N, else a fixed seed on one thread (repeatable) and host
        // entropy with a thread per core (not repeatable anyway)
        match arg(args, "--seed").and_then(|v| v.parse::<u64>().ok()) {
            Some(seed) => m.soc.rng.reseed(seed),
            None if m.multi => m.soc.rng.reseed(bcm2711::periph::Rng200::host_seed()),
            None => {}
        }
        if std::env::var("SBC_JIT_VERIFY").is_ok_and(|v| v == "1") { if let Some(j) = m.jit_mut() { j.verify = true; } }
        if std::env::var("SBC_JIT_PROFILE").is_ok_and(|v| v == "1") { arm_a64::jit::PROFILE.store(true, std::sync::atomic::Ordering::Relaxed); }
        match net_mode { "none" => {}, "local" => m.connect_network(false, trace_net), _ => m.connect_network(true, trace_net) }
        // --hostfwd tcp:[HOSTADDR]:HOSTPORT-[GUESTADDR]:GUESTPORT (QEMU's syntax), repeatable
        for (i, a) in args.iter().enumerate() {
            if a != "--hostfwd" { continue; }
            let spec = args.get(i + 1).map(String::as_str).unwrap_or("");
            let parsed = spec.strip_prefix("tcp:").and_then(|r| r.split_once('-')).and_then(|(h, g)| {
                let (ha, hp) = h.rsplit_once(':')?;
                let gp = g.rsplit_once(':').map_or(g, |(_, p)| p).parse::<u16>().ok()?;
                let host: std::net::SocketAddr = format!("{}:{}", if ha.is_empty() { "127.0.0.1" } else { ha }, hp).parse().ok()?;
                Some((host, gp))
            });
            match parsed {
                Some((host, gp)) => { m.forward_tcp(host, gp)?; eprintln!("[sbc-emu] forwarding {host} -> guest port {gp}"); }
                None => return Err(format!("--hostfwd {spec}: expected tcp:[HOSTADDR]:HOSTPORT-[GUESTADDR]:GUESTPORT")),
            }
        }
        m.soc.genet.trace = trace_net;
        m.soc.wifi.trace = args.iter().any(|a| a == "--trace-sdio");
        if let Some(c) = m.soc.wifi.sdio.as_mut() { c.set_trace(args.iter().any(|a| a == "--trace-wifi")); }
        bcm2711::boot::boot_linux(&mut m, &k, i.as_deref(), &c, display)?;
        Ok(m)
    };
    let mut m = match power_on(disk, usb_drive) { Ok(m) => m, Err(e) => { eprintln!("boot: {e}"); return ExitCode::FAILURE } };
    let mut clock_offset = Duration::ZERO;
    if let Some(p) = arg(args, "--restore") {
        let snap = match std::fs::read(p) { Ok(b) => b, Err(e) => { eprintln!("{p}: {e}"); return ExitCode::FAILURE } };
        if let Err(e) = m.restore(&snap) { eprintln!("{p}: {e}"); return ExitCode::FAILURE; }
        clock_offset = Duration::from_nanos(m.now);
        eprintln!("[sbc-emu] restored {p}: guest time {:.1} s, {} instructions", m.now as f64 / 1e9, m.instret());
        // a USB stick given now but absent from the snapshot is plugged in
        use bcm2711::usbdev::DevId;
        for (dev, name, present) in [(DevId::Storage, "USB stick", m.soc.devs.storage.is_some()), (DevId::Camera, "USB camera", m.soc.devs.camera.is_some())] {
            if !present || m.soc.usb.port == Some(dev) { continue; }
            let soc = &mut m.soc;
            if let Some(k) = soc.pcie.xhci.plug(dev, &mut soc.ram, &mut soc.devs) { eprintln!("[sbc-emu] {name} plugged into port {k}"); }
        }
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
    let mut start = Instant::now() - clock_offset;
    if let Some(g) = gdb.as_mut() {
        if gdb_wait {
            g.wait_for_client();
            if let gdb::Resume::Kill = g.halted(&mut m, 0, 5, false) { return ExitCode::SUCCESS; }
            start = Instant::now() - Duration::from_nanos(m.now);
        }
    }
    let started = Instant::now();
    let mut escape = false;
    let mut pending: std::collections::VecDeque<u8> = Default::default();
    // keys read while waiting (idle, paused), handled with the rest
    let mut idle_keys: Vec<u8> = Vec::new();
    // the monitor's input line while it is open (Ctrl-A c)
    let mut monitor_line: Option<String> = None;
    let mut paused = false;
    let mut reset_req = false;
    let result = loop {
        // input: Ctrl-A x quits, Ctrl-A c opens/closes the monitor, Ctrl-A Ctrl-A sends Ctrl-A
        let mut quit = false;
        let mut keys = std::mem::take(&mut idle_keys);
        while let Ok(b) = rx.try_recv() { keys.push(b); }
        for b in keys {
            if escape {
                escape = false;
                match b {
                    b'x' => { quit = true; break; }
                    b'c' => {
                        if monitor_line.take().is_some() { eprint!("\r\n"); } else { monitor_line = Some(String::new()); eprint!("\r\n(sbc-emu) "); }
                        continue;
                    }
                    1 => {}
                    _ => if monitor_line.is_none() { pending.push_back(1); },
                }
            } else if b == 1 { escape = true; continue; }
            if let Some(line) = monitor_line.as_mut() {
                match b {
                    b'\r' | b'\n' => {
                        let cmd = std::mem::take(line);
                        eprint!("\r\n");
                        let (text, effect) = monitor::command(&mut m, &cmd);
                        eprint!("{}", text.replace('\n', "\r\n"));
                        match effect {
                            monitor::Effect::Quit => { quit = true; break; }
                            monitor::Effect::Pause(p) => paused = p,
                            monitor::Effect::Reset => reset_req = true,
                            monitor::Effect::Gdb(port) => match gdb::Gdb::listen(port) { Ok(g) => gdb = Some(g), Err(e) => eprint!("gdbserver: {e}\r\n") },
                            monitor::Effect::None => {}
                        }
                        eprint!("(sbc-emu) ");
                    }
                    0x7f | 8 => if line.pop().is_some() { eprint!("\x08 \x08"); },
                    0x20..=0x7e => { line.push(b as char); eprint!("{}", b as char); }
                    _ => {}
                }
                continue;
            }
            pending.push_back(b);
        }
        if quit { break Stop::Budget; }
        if usb_input {
            // keystrokes go to the USB keyboard; terminal escape sequences become arrow keys
            while m.soc.devs.kbd.queued() < 64 {
                let Some(b) = pending.pop_front() else { break };
                if b == 0x1b && pending.front() == Some(&b'[') && pending.len() >= 2 {
                    pending.pop_front();
                    let k = pending.pop_front().unwrap();
                    let usage = match k { b'A' => 0x52, b'B' => 0x51, b'C' => 0x4f, b'D' => 0x50, b'H' => 0x4a, b'F' => 0x4d, _ => 0 };
                    if usage != 0 { m.soc.devs.kbd.tap(usage, 0); }
                } else {
                    m.soc.devs.kbd.type_byte(b);
                }
            }
        } else {
            while let Some(&b) = pending.front() {
                if !HostMachine::Pi4(&mut m).serial_input(b) { break; }
                pending.pop_front();
            }
        }

        if timeout.is_some_and(|t| started.elapsed() >= t) { break Stop::Budget; }
        // action script: one step at a time
        if script_wait_until.is_none_or(|t| Instant::now() >= t) {
            script_wait_until = None;
            if let Some(line) = script.front().cloned() {
                let (cmd, rest) = line.split_once(' ').unwrap_or((line.as_str(), ""));
                let unescape = |t: &str| crate::script::unescape(t).replace("\\e", "\x1b");
                let mut done = true;
                match cmd {
                    "wait-serial" => done = serial_log.contains(rest),
                    "wait-idle" => done = last_busy.elapsed().as_secs_f64() >= rest.parse().unwrap_or(5.0),
                    "wait-pixel" => {
                        // wait-pixel X Y RRGGBB: the screen shows that colour at (X, Y), each channel
                        // within 8 (a 16-bit console framebuffer, as the KMS driver's, rounds colours)
                        let a: Vec<&str> = rest.split_whitespace().collect();
                        let (x, y, rgb) = (a.first().and_then(|v| v.parse::<u32>().ok()).unwrap_or(0), a.get(1).and_then(|v| v.parse::<u32>().ok()).unwrap_or(0),
                                           a.get(2).and_then(|v| u32::from_str_radix(v, 16).ok()).unwrap_or(0));
                        let mut rgba = Vec::new();
                        done = match m.soc.render(&mut rgba) {
                            Some((w, h)) if x < w && y < h => {
                                let o = ((y * w + x) * 4) as usize;
                                (0..3).all(|k| (rgba[o + k] as i32 - ((rgb >> (16 - 8 * k)) & 0xff) as i32).abs() <= 8)
                            }
                            _ => false,
                        };
                    }
                    "sleep" => { script_wait_until = Some(Instant::now() + Duration::from_secs_f64(rest.parse().unwrap_or(1.0))); }
                    "type" => for b in unescape(rest).bytes() { m.soc.devs.kbd.type_byte(b); },
                    "serial" => pending.extend(unescape(rest).bytes()),
                    "serial1" => for b in unescape(rest).bytes() { m.soc.aux.push_rx(b); m.soc.dirty = true; },
                    "monitor" => {
                        let (text, effect) = monitor::command(&mut m, rest);
                        eprint!("{}", text.replace('\n', "\r\n"));
                        match effect {
                            monitor::Effect::Quit => { script.pop_front(); break Stop::Budget; }
                            monitor::Effect::Pause(p) => paused = p,
                            monitor::Effect::Reset => reset_req = true,
                            monitor::Effect::Gdb(port) => if let Ok(g) = gdb::Gdb::listen(port) { gdb = Some(g) },
                            monitor::Effect::None => {}
                        }
                    }
                    "shot" => {
                        let mut rgba = Vec::new();
                        if let Some((w, h)) = m.soc.render(&mut rgba) { let _ = std::fs::write(rest, bcm2711::png::encode_rgba(w, h, &rgba)); }
                    }
                    "quit" => { script.pop_front(); break Stop::Budget; }
                    "gpio" => {
                        // gpio PIN 0|1|float
                        let mut it = rest.split_whitespace();
                        if let (Some(p), Some(v)) = (it.next().and_then(|p| p.parse::<usize>().ok()), it.next()) {
                            m.soc.gpio.set_input(p, match v { "1" | "high" => Some(true), "0" | "low" => Some(false), _ => None });
                            m.soc.dirty = true;
                        }
                    }
                    "sensor" => {
                        // sensor temp|pressure|humidity VALUE (degC, hPa, %RH)
                        let mut it = rest.split_whitespace();
                        if let (Some(k), Some(v)) = (it.next(), it.next().and_then(|v| v.parse::<f64>().ok())) {
                            m.set_sensor(k, v);
                        }
                    }
                    "adc" => {
                        // adc CHANNEL VOLTS
                        let mut it = rest.split_whitespace();
                        if let (Some(c), Some(v)) = (it.next().and_then(|c| c.parse::<usize>().ok()), it.next().and_then(|v| v.parse::<f64>().ok())) {
                            if c < 8 { m.soc.spi.adc.volts[c] = v; }
                        }
                    }
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
        if let Some(v) = vnc.as_mut() { v.poll(&mut m); }
        if let Some(g) = gdb.as_mut() {
            let p = g.poll(&mut m);
            if !matches!(p, gdb::Poll::Running) {
                let t0 = Instant::now();
                if let gdb::Resume::Kill = g.halted(&mut m, 0, 2, matches!(p, gdb::Poll::Interrupt)) { break Stop::Budget; }
                start += t0.elapsed(); // halted time does not count as guest idle time
            }
        }
        let budget = 2_000_000u64.min(max.saturating_sub(m.instret()));
        if budget == 0 { break Stop::Budget; }
        if paused && !reset_req {
            // paused from the monitor: only keys move; the pause does not count as guest time
            let t0 = Instant::now();
            if let Ok(b) = rx.recv_timeout(Duration::from_millis(20)) { idle_keys.push(b); }
            start += t0.elapsed();
            continue;
        }
        let mut stop = if std::mem::take(&mut reset_req) { Stop::Reset } else { HostMachine::Pi4(&mut m).run(budget) };
        // a debugger looks at breakpoints and at stops that would end the run
        if let Some(g) = gdb.as_mut().filter(|g| g.connected()) {
            let hit = g.stop_signal(&stop, 0);
            if let Some((k, sig)) = hit {
                let t0 = Instant::now();
                if let gdb::Resume::Kill = g.halted(&mut m, k, sig, true) { break Stop::Budget; }
                start += t0.elapsed();
                stop = Stop::Budget;
            }
        }
        if let Stop::Breakpoint(_) | Stop::Watchpoint(..) = stop { stop = Stop::Budget; }
        if gpio_log {
            for ev in std::mem::take(&mut m.soc.gpio.events) {
                eprintln!("\r[gpio] GPIO{} -> {} at t={:.6}s\r", ev.pin, ev.level as u8, ev.time_ns as f64 / 1e9);
            }
        } else { m.soc.gpio.events.clear(); }
        if !script.is_empty() {
            crate::script::append_serial(&mut serial_log, HostMachine::Pi4(&mut m).serial_output());
        }
        if monitor_line.is_none() { flush(&mut m, &mut out, tty.0); }
        if let Some(w) = audio_out.as_mut() {
            let s = std::mem::take(&mut m.soc.i2s.out);
            w.push(m.soc.i2s.rate, &s);
        } else if !m.soc.i2s.out.is_empty() { m.soc.i2s.out.clear(); }
        if !m.soc.aux.out.is_empty() {
            let b = std::mem::take(&mut m.soc.aux.out);
            if let Some(f) = serial1.as_mut() { let _ = f.write_all(&b); let _ = f.flush(); }
        }
        match stop {
            Stop::Budget => last_busy = Instant::now(),
            Stop::Idle(t) => {
                // real-time pacing: idle time may catch up with the wall clock but never run ahead
                let wall = start.elapsed().as_nanos() as u64;
                let target = t.min(wall.max(m.now));
                if target <= m.now {
                    let wait = Duration::from_nanos((t - m.now).min(20_000_000));
                    if let Ok(b) = rx.recv_timeout(wait) { idle_keys.push(b); }
                    let wall = start.elapsed().as_nanos() as u64;
                    HostMachine::Pi4(&mut m).idle_until(t.min(wall));
                } else {
                    HostMachine::Pi4(&mut m).idle_until(target);
                }
            }
            Stop::Reset => {
                eprintln!("\r\n[sbc-emu] system reset: rebooting\r");
                let d = m.soc.sd.disk.take();
                let u = m.soc.devs.storage.take().map(|s| s.disk);
                if let Some(n) = m.soc.net.as_mut() { n.nat = None; } // free forwarded host ports
                m = match power_on(d, u) { Ok(m) => m, Err(e) => { eprintln!("reboot: {e}"); break Stop::Reset } };
                if let Some(g) = gdb.as_ref() { g.rearm(&mut m); }
            }
            other => break other,
        }
    };
    flush(&mut m, &mut out, tty.0);
    drop(tty);
    if let Some(path) = screenshot {
        let mut rgba = Vec::new();
        match m.soc.render(&mut rgba) {
            Some((w, h)) => { let _ = std::fs::write(path, bcm2711::png::encode_rgba(w, h, &rgba)); eprintln!("[sbc-emu] screenshot {w}x{h} -> {path}"); }
            None => eprintln!("[sbc-emu] no framebuffer allocated: no screenshot"),
        }
    }
    if !m.soc.mbox.unknown.is_empty() { eprintln!("[sbc-emu] unanswered mailbox tags: {:x?}", m.soc.mbox.unknown); }
    eprintln!("\n[sbc-emu] stopped: {:?} pc={:#x} insns={} sim={:.3}s wall={:.1}s exceptions={} irqs={}",
        result, m.cpu().pc, m.instret(), m.now as f64 / 1e9, started.elapsed().as_secs_f64(), m.exceptions, m.irqs);
    if let Some(g) = m.soc.v3d.as_ref() { eprintln!("[sbc-emu] gpu: {}", g.stats()); }
    if let Some(j) = m.jit() {
        // every core has its own JIT (one per host thread with --threads multi): sum them
        let js: Vec<&arm_a64::jit::Jit> = m.cpus.iter().filter_map(|c| c.jit.as_deref()).collect();
        let sum = |f: fn(&arm_a64::jit::Jit) -> u64| js.iter().map(|j| f(j)).sum::<u64>();
        let (execs, insns) = (sum(|j| j.execs), sum(|j| j.exec_insns));
        eprintln!("[sbc-emu] jit ({} cores): {} blocks compiled, {} links, {} flushes ({} with the cache full), {} pages invalidated ({} blocks); {} block runs, {:.1} instructions per run, {:.0}% of instructions in blocks",
            js.len(), sum(|j| j.compiled), sum(|j| j.linked), sum(|j| j.flushes), sum(|j| j.full_flushes), sum(|j| j.inval_pages), sum(|j| j.inval_blocks), execs,
            insns as f64 / execs.max(1) as f64, 100.0 * insns as f64 / m.instret().max(1) as f64);
        if j.verify { eprintln!("[sbc-emu] jit verify ({} stale blocks skipped): {}", j.stale_skips, j.report.as_deref().unwrap_or("no mismatch")); }
    }
    if arm_a64::jit::PROFILE.load(std::sync::atomic::Ordering::Relaxed) {
        let f = arm_a64::jit::profile_fallbacks();
        let total: u64 = f.iter().map(|e| e.1).sum();
        eprintln!("[sbc-emu] jit interpreter fallbacks: {total} (instret {}); most frequent words:", m.instret());
        for (i, n) in f.iter().take(60) { eprintln!("  {i:08x} {n}"); }
    }
    if !m.cpu().sys.unknown.is_empty() {
        let names: Vec<String> = m.cpu().sys.unknown.iter().map(|k| format!("S{}_{}_C{}_C{}_{}", k >> 14, (k >> 11) & 7, (k >> 7) & 15, (k >> 3) & 15, k & 7)).collect();
        eprintln!("[sbc-emu] unmodelled system registers: {}", names.join(" "));
    }
    match result { Stop::Budget => ExitCode::SUCCESS, _ => ExitCode::FAILURE }
}

fn flush(m: &mut Machine, out: &mut std::io::Stdout, raw: bool) {
    let bytes = HostMachine::Pi4(m).take_serial_output();
    if bytes.is_empty() { return; }
    if raw {
        // in raw mode the terminal needs CR for each LF the guest did not send itself
        let _ = out.write_all(&bytes);
    } else {
        let _ = out.write_all(&bytes);
    }
    let _ = out.flush();
}
