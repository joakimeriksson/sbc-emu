//! Serial-console run loop shared by the direct-boot bring-up boards (Orange Pi 5,
//! Orange Pi One): option validation, `--script` automation, host stdin with the
//! Ctrl-A x escape, wall-clock pacing of idle time and guest reset/power-off, and for
//! boards with a display `--screenshot`, `--vnc` and the `screenshot FILE` script command.
use crate::{arg, gdb::{self, Gdb}, vnc::{Screen, Vnc}};
use sbc_machine::{Machine as HostMachine, Stop};
use std::{
    collections::VecDeque,
    io::{Read, Write},
    sync::mpsc,
    time::{Duration, Instant},
};

/// A board machine as the serial loop needs it.
pub(crate) trait SerialBoard: gdb::Target {
    fn host(&mut self) -> HostMachine<'_>;
    /// Virtual time in nanoseconds.
    fn now(&self) -> u64;
    fn instret(&self) -> u64;
    /// One-line execution summary plus any unmodelled-hardware notes.
    fn report(&self, tag: &str);
    /// The machine as a snapshot file, and back (into a machine made with the same options).
    fn save(&self) -> Result<Vec<u8>, String> {
        Err("snapshots are not implemented for this board".into())
    }
    fn restore(&mut self, _file: &[u8]) -> Result<(), String> {
        Err("snapshots are not implemented for this board".into())
    }
    /// The board's display, if it has one.
    fn screen(&mut self) -> Option<&mut dyn Screen> {
        None
    }
    /// Start keeping what the board plays (for `--audio-out`); false: it has no audio.
    fn capture_audio(&mut self) -> bool {
        false
    }
    /// The samples played since the last call: the rate (Hz) and 16-bit stereo.
    fn take_audio(&mut self) -> (u32, Vec<i16>) {
        (0, Vec::new())
    }
    /// Start keeping what the board plays on HDMI (for `--hdmi-audio-out`), when that is a
    /// separate output from `--audio-out`'s; false: it has none.
    fn capture_hdmi_audio(&mut self) -> bool {
        false
    }
    /// The HDMI samples played since the last call: the rate (Hz) and 16-bit stereo.
    fn take_hdmi_audio(&mut self) -> (u32, Vec<i16>) {
        (0, Vec::new())
    }
    /// Plug the HDMI monitor in or out (the `hdmi connect|disconnect` script command); false:
    /// the board has no such port.
    fn set_hdmi_connected(&mut self, _on: bool) -> bool {
        false
    }
}

/// Where `--audio-out` goes: a WAV file, or raw 16-bit little-endian stereo for any other
/// path (a named pipe for a live player such as `ffplay -f s16le -ar RATE -ch_layout stereo`).
enum AudioOut {
    Wav(crate::wav::Wav),
    Raw(std::fs::File),
}

impl AudioOut {
    fn open(path: &str) -> std::io::Result<Self> {
        Ok(if path.ends_with(".wav") {
            AudioOut::Wav(crate::wav::Wav::create(path)?)
        } else {
            AudioOut::Raw(std::fs::OpenOptions::new().write(true).create(true).truncate(true).open(path)?)
        })
    }

    fn push(&mut self, rate: u32, samples: &[i16]) {
        match self {
            AudioOut::Wav(w) => w.push(rate, samples),
            AudioOut::Raw(f) => {
                let bytes: Vec<u8> = samples.iter().flat_map(|s| s.to_le_bytes()).collect();
                let _ = f.write_all(&bytes);
            }
        }
    }
}

/// Save what the display shows as a PNG.
fn screenshot<B: SerialBoard>(m: &mut B, tag: &str, path: &str) {
    let mut rgba = Vec::new();
    match m.screen().and_then(|s| s.render(&mut rgba)) {
        Some((w, h)) => match std::fs::write(path, bcm2711::png::encode_rgba(w, h, &rgba)) {
            Ok(()) => eprintln!("[{tag}] screenshot {w}x{h} -> {path}"),
            Err(e) => eprintln!("[{tag}] screenshot {path}: {e}"),
        },
        None => eprintln!("[{tag}] the display is off: no screenshot"),
    }
}

/// How long a snapshot waits for a guest that takes none of the input typed before it.
const PATIENCE: Duration = Duration::from_secs(2);

/// Write a snapshot; `typed` bytes of input have not reached the guest.
fn save<B: SerialBoard>(m: &B, tag: &str, path: &str, typed: usize) -> Result<(), String> {
    let file = m.save()?;
    // never half a snapshot under the name
    let part = format!("{path}.part");
    if let Err(e) = std::fs::write(&part, &file).and_then(|()| std::fs::rename(&part, path)) {
        let _ = std::fs::remove_file(&part);
        return Err(format!("{path}: {e}"));
    }
    eprintln!("\r[{tag}] snapshot at {:.3} s, {:.1} MiB -> {path}\r", m.now() as f64 / 1e9, file.len() as f64 / (1 << 20) as f64);
    if typed > 0 {
        eprintln!("\r[{tag}] the guest has not taken {typed} bytes typed before it: they are not in the snapshot\r");
    }
    Ok(())
}

/// The moment a guest at virtual time `now` would have started for its clock to be the wall's.
fn anchored(now: u64) -> Instant {
    let t = Instant::now();
    t.checked_sub(Duration::from_nanos(now)).unwrap_or(t)
}

/// Options every serial board accepts; `extra` lists board-specific valued options. A board
/// that lists `--smp` there checks its core count itself; the others run one core.
pub(crate) fn validate_options(args: &[String], extra: &[&str]) -> Result<(), String> {
    const COMMON: [&str; 10] = [
        "--board", "--kernel", "--initrd", "--append", "--script", "--timeout", "--max-insns",
        "--ram-mb", "--smp", "--gdb",
    ];
    let mut index = 1;
    while index < args.len() {
        let a = args[index].as_str();
        if a.starts_with("--board=") || matches!(a, "--no-jit" | "--no-display" | "--sd-persist" | "--gdb-wait") {
            index += 1;
            continue;
        }
        if !COMMON.contains(&a) && !extra.contains(&a) {
            return Err(format!(
                "unsupported option '{a}'; this machine provides direct boot and serial only"
            ));
        }
        if args.get(index + 1).is_none_or(|v| v.starts_with("--")) {
            return Err(format!("{a} requires a value"));
        }
        index += 2;
    }
    if !extra.contains(&"--smp") && arg(args, "--smp").is_some_and(|s| s != "1") {
        return Err("only --smp 1 is implemented for this board".into());
    }
    Ok(())
}

pub(crate) struct RunConfig {
    pub tag: &'static str,
    pub script: VecDeque<String>,
    pub timeout: Option<Duration>,
    pub max_insns: u64,
    /// Screenshot at the end of the run (`--screenshot`).
    pub screenshot: Option<String>,
    /// VNC server for the display (`--vnc :N` or `--vnc stdio`).
    pub vnc: Option<String>,
    /// What the board plays (`--audio-out FILE.wav`, or raw PCM to any other path).
    pub audio_out: Option<String>,
    /// What the board plays on HDMI, for boards where that is a separate output.
    pub hdmi_audio_out: Option<String>,
    /// The gdb stub's port (`--gdb PORT`), and whether to wait for a debugger before starting.
    pub gdb: Option<u16>,
    pub gdb_wait: bool,
    /// Continue from this snapshot (`--restore`).
    pub restore: Option<String>,
    /// Where Ctrl-A s writes one (`--snapshot`, else BOARD.snap).
    pub snapshot: String,
}

impl RunConfig {
    pub(crate) fn from_args(tag: &'static str, args: &[String]) -> Result<Self, String> {
        let max_insns = arg(args, "--max-insns")
            .unwrap_or("18446744073709551615")
            .parse::<u64>()
            .map_err(|_| "invalid --max-insns")?;
        let timeout = arg(args, "--timeout")
            .map(|v| v.parse::<f64>().map_err(|_| "invalid --timeout"))
            .transpose()?
            .map(Duration::try_from_secs_f64)
            .transpose()
            .map_err(|_| "invalid --timeout")?;
        let script = crate::script::load(arg(args, "--script")).map_err(|e| e.to_string())?;
        for line in &script {
            if !matches!(line.split_whitespace().next(), Some("wait-serial" | "serial" | "quit" | "reset" | "screenshot" | "type" | "sleep" | "pointer" | "save" | "wait-pixel" | "wait-idle" | "hdmi")) {
                return Err(format!("unsupported script command: {line}"));
            }
        }
        let (screenshot, vnc) = (arg(args, "--screenshot").map(str::to_owned), arg(args, "--vnc").map(str::to_owned));
        let audio_out = arg(args, "--audio-out").map(str::to_owned);
        let hdmi_audio_out = arg(args, "--hdmi-audio-out").map(str::to_owned);
        let gdb = arg(args, "--gdb").map(|p| p.trim_start_matches("tcp::").parse::<u16>()).transpose().map_err(|_| "--gdb needs a port number")?;
        let gdb_wait = args.iter().any(|a| a == "--gdb-wait");
        if gdb_wait && gdb.is_none() {
            return Err("--gdb-wait needs --gdb PORT".into());
        }
        let restore = arg(args, "--restore").map(str::to_owned);
        let snapshot = arg(args, "--snapshot").map_or_else(|| format!("{tag}.snap"), str::to_owned);
        Ok(Self { tag, script, timeout, max_insns, screenshot, vnc, audio_out, hdmi_audio_out, gdb, gdb_wait, restore, snapshot })
    }
}

/// RAM size from `--ram-mb`, with a board default.
pub(crate) fn ram_bytes(args: &[String], default_mb: usize) -> Result<usize, String> {
    match arg(args, "--ram-mb") {
        Some(v) => v.parse::<usize>().map_err(|_| "invalid --ram-mb")?,
        None => default_mb,
    }
    .checked_mul(1 << 20)
    .ok_or_else(|| "RAM size overflow".into())
}

/// Run a freshly made machine on the host terminal until the script completes, the
/// guest powers off, the user quits (Ctrl-A x) or a limit is reached.
pub(crate) fn run<B: SerialBoard>(
    config: RunConfig,
    make_machine: impl Fn() -> Result<B, String>,
) -> Result<(), String> {
    run_with_io(config, make_machine, |_| Ok(()))
}

pub(crate) fn run_with_io<B: SerialBoard>(
    mut config: RunConfig,
    make_machine: impl Fn() -> Result<B, String>,
    mut poll_io: impl FnMut(&mut B) -> Result<(), String>,
) -> Result<(), String> {
    let tag = config.tag;
    let mut m = make_machine()?;
    let vnc_stdio = config.vnc.as_deref() == Some("stdio");
    let mut vnc = match config.vnc.as_deref() {
        Some("stdio") => Some(Vnc::stdio().map_err(|e| format!("--vnc stdio: {e}"))?),
        Some(spec) => Some(Vnc::listen(spec).map_err(|e| format!("--vnc {spec}: {e}"))?),
        None => None,
    };
    let mut audio = match config.audio_out.as_deref() {
        Some(path) if m.capture_audio() => Some(AudioOut::open(path).map_err(|e| format!("--audio-out {path}: {e}"))?),
        Some(_) => return Err("--audio-out: this board has no modelled audio output".into()),
        None => None,
    };
    let mut hdmi_audio = match config.hdmi_audio_out.as_deref() {
        Some(path) if m.capture_hdmi_audio() => Some(AudioOut::open(path).map_err(|e| format!("--hdmi-audio-out {path}: {e}"))?),
        Some(_) => return Err("--hdmi-audio-out: this board has no separate HDMI audio output".into()),
        None => None,
    };
    let mut gdb = match config.gdb {
        Some(port) => Some(Gdb::listen(port).map_err(|e| format!("--gdb {port}: {e}"))?),
        None => None,
    };
    let _terminal = (!vnc_stdio).then(crate::terminal::RawTty::enable);
    let (tx, rx) = mpsc::channel();
    if !vnc_stdio {
        std::thread::spawn(move || {
            let mut stdin = std::io::stdin();
            let mut b = [0];
            while let Ok(1) = stdin.read(&mut b) {
                if tx.send(b[0]).is_err() {
                    break;
                }
            }
        });
    }
    let script = &mut config.script;
    let mut idle_input = Vec::new();
    let mut pending = VecDeque::new();
    let mut output: Box<dyn Write> = if vnc_stdio {
        Box::new(std::io::stderr())
    } else {
        Box::new(std::io::stdout())
    };
    let mut serial = String::new();
    // how much of `serial` the current wait-serial has searched without a match (searching
    // all of it again on every pass made long boots quadratic)
    let mut searched = 0usize;
    if let Some(path) = config.restore.as_deref() {
        let file = std::fs::read(path).map_err(|e| format!("{path}: {e}"))?;
        m.restore(&file).map_err(|e| format!("--restore {path}: {e}"))?;
        eprintln!("[{tag}] restored {path}: {:.3} s, {} instructions", m.now() as f64 / 1e9, m.instret());
    }
    let mut start = Instant::now();
    let mut anchor = anchored(m.now());
    let mut escape = false;
    // Ctrl-A s: a snapshot, once the guest has taken what was typed before it
    let mut save_asked = false;
    // how much typed input waits, and since when it has not become less: a guest that takes none
    // (it crashed, say) does not hold up a snapshot for ever
    let mut waiting = (0, Instant::now());
    // `sleep SECS`: the script waits until this virtual time (ns)
    let mut sleep_until: Option<u64> = None;
    // what the guest asked for while a debugger stepped it
    let mut owed: Option<Stop> = None;
    // the last wall time the guest ran a full slice without going idle (`wait-idle`)
    let mut last_busy = Instant::now();
    if let Some(g) = gdb.as_mut().filter(|_| config.gdb_wait) {
        g.wait_for_client();
        let core = m.stopped_core();
        match g.halted(&mut m, core, 5, false) {
            gdb::Resume::Kill => return Ok(()),
            gdb::Resume::Stopped(stop) => owed = Some(stop),
            _ => {}
        }
        start = Instant::now();
        anchor = anchored(m.now());
    }
    let result = 'run: loop {
        if let Err(error) = poll_io(&mut m) {
            break Err(error);
        }
        let mut input = std::mem::take(&mut idle_input);
        while let Ok(b) = rx.try_recv() {
            input.push(b);
        }
        for b in input {
            if escape {
                escape = false;
                if b == b'x' {
                    break 'run Ok(());
                }
                if b == b's' {
                    save_asked = true;
                    continue;
                }
                // Ctrl-A Ctrl-A sends one Ctrl-A; Ctrl-A and another key sends both
                if b != 1 {
                    pending.push_back(1);
                }
            } else if b == 1 {
                escape = true;
                continue;
            }
            pending.push_back(b);
        }
        if sleep_until.is_some_and(|t| m.now() >= t) {
            sleep_until = None;
        }
        if let Some(line) = script.front().cloned().filter(|_| sleep_until.is_none()) {
            let (cmd, rest) = line.split_once(' ').unwrap_or((&line, ""));
            let mut consumed = 0;
            let done = match cmd {
                "wait-serial" => {
                    let mut from = searched.saturating_sub(rest.len());
                    while !serial.is_char_boundary(from) {
                        from -= 1;
                    }
                    match serial[from..].find(rest) {
                        // Keep output after the match: the next wait may already be in it.
                        Some(position) => {
                            consumed = from + position + rest.len();
                            true
                        }
                        None => {
                            searched = serial.len();
                            false
                        }
                    }
                }
                "serial" => {
                    // \x03 is Ctrl-C (SIGINT through the guest's tty)
                    pending.extend(
                        crate::script::unescape(rest).replace("\\x03", "\x03").bytes(),
                    );
                    true
                }
                "reset" => {
                    m = make_machine()?;
                    owed = None; // it was the old machine's
                    if std::mem::take(&mut save_asked) {
                        eprintln!("\r[{tag}] no snapshot: the machine was reset\r");
                    }
                    if let Some(g) = gdb.as_ref() {
                        g.rearm(&mut m);
                    }
                    anchor = Instant::now();
                    serial.clear();
                    searched = 0;
                    pending.clear();
                    true
                }
                // once the guest has what was typed: the rest of it is not the machine's
                "save" if !pending.is_empty() && waiting.1.elapsed() < PATIENCE => false,
                "save" => {
                    if let Err(e) = save(&m, tag, rest, pending.len()) {
                        break Err(e);
                    }
                    true
                }
                "screenshot" => {
                    screenshot(&mut m, tag, rest);
                    true
                }
                // wait-idle SECS: the guest has been idle (every core waiting) for SECS of wall time
                "wait-idle" => last_busy.elapsed().as_secs_f64() >= rest.trim().parse().unwrap_or(5.0),
                // wait-pixel X Y RRGGBB: the screen shows that colour at (X, Y), each channel
                // within 8 (a 16-bit console framebuffer rounds colours)
                "wait-pixel" => {
                    let a: Vec<&str> = rest.split_whitespace().collect();
                    let (x, y, rgb) = match (a.first().and_then(|v| v.parse::<u32>().ok()), a.get(1).and_then(|v| v.parse::<u32>().ok()), a.get(2).and_then(|v| u32::from_str_radix(v, 16).ok())) {
                        (Some(x), Some(y), Some(c)) => (x, y, c),
                        _ => return Err(format!("script: {line} (wait-pixel X Y RRGGBB)")),
                    };
                    let mut rgba = Vec::new();
                    match m.screen().and_then(|s| s.render(&mut rgba)) {
                        Some((w, h)) if x < w && y < h => {
                            let o = ((y * w + x) * 4) as usize;
                            (0..3).all(|k| (rgba[o + k] as i32 - ((rgb >> (16 - 8 * k)) & 0xff) as i32).abs() <= 8)
                        }
                        _ => false,
                    }
                }
                // hdmi connect|disconnect: plug the monitor in or out
                "hdmi" => {
                    let on = match rest.trim() { "connect" => true, "disconnect" => false, _ => return Err(format!("script: {line} (hdmi connect|disconnect)")) };
                    if !m.set_hdmi_connected(on) { eprintln!("[{tag}] script: this board has no HDMI hot-plug; '{line}' ignored"); }
                    true
                }
                "sleep" => {
                    let seconds: f64 = rest.trim().parse().map_err(|_| format!("script: {line}"))?;
                    sleep_until = Some(m.now().saturating_add((seconds * 1e9) as u64));
                    true
                }
                // the pointer (a touch on touchscreen boards): "pointer BUTTONS X Y", X and Y
                // in pixels of the picture shown, passed on as absolute 0..32767 like a viewer's
                "pointer" => {
                    let v: Vec<u32> = rest.split_whitespace().filter_map(|w| w.parse().ok()).collect();
                    match (m.screen(), v.as_slice()) {
                        (Some(screen), [b, x, y]) => {
                            let (w, h) = screen.display_size();
                            let scale = |v: u32, n: u32| (v.min(n.saturating_sub(1)) * 32767 / n.saturating_sub(1).max(1)) as u16;
                            screen.pointer(*b as u8, scale(*x, w), scale(*y, h), 0)
                        }
                        _ => return Err(format!("script: {line}")),
                    }
                    true
                }
                // keystrokes on the board's USB keyboard (\n is Enter)
                "type" => {
                    match m.screen() {
                        Some(screen) => {
                            for b in rest.replace("\\n", "\r").bytes() {
                                screen.type_byte(b);
                            }
                        }
                        None => eprintln!("[{tag}] script: this board has no keyboard; 'type' ignored"),
                    }
                    true
                }
                "quit" => break Ok(()),
                _ => unreachable!(),
            };
            if done {
                eprintln!("[{tag} script] {line}");
                script.pop_front();
                searched = 0;
                if cmd == "wait-serial" {
                    serial.drain(..consumed);
                }
            }
        }
        while let Some(&b) = pending.front() {
            if !m.host().serial_input(b) {
                break;
            }
            pending.pop_front();
        }
        if pending.len() != waiting.0 {
            waiting = (pending.len(), Instant::now());
        }
        if save_asked && (pending.is_empty() || waiting.1.elapsed() >= PATIENCE) {
            save_asked = false;
            if let Err(e) = save(&m, tag, &config.snapshot, pending.len()) {
                eprintln!("\r[{tag}] no snapshot: {e}\r");
            }
        }
        // --timeout and --max-insns end a run normally, unless a --script is still running
        if config.timeout.is_some_and(|t| start.elapsed() >= t) {
            break match script.front() {
                None => {
                    eprintln!("\r\n[{tag}] --timeout reached\r");
                    Ok(())
                }
                Some(step) => Err(format!("host timeout before the script completed (at: {step})")),
            };
        }
        if m.instret() >= config.max_insns {
            break if script.is_empty() {
                Ok(())
            } else {
                Err("instruction limit before script completed".into())
            };
        }
        if let (Some(v), Some(screen)) = (vnc.as_mut(), m.screen()) {
            v.poll(screen);
            if v.stdio_disconnected() {
                break if script.is_empty() {
                    Ok(())
                } else {
                    Err("VNC client disconnected before the script completed".into())
                };
            }
        }
        if let Some(g) = gdb.as_mut() {
            let p = g.poll(&mut m);
            if !matches!(p, gdb::Poll::Running) {
                let halted = Instant::now();
                let core = m.stopped_core();
                match g.halted(&mut m, core, 2, matches!(p, gdb::Poll::Interrupt)) {
                    gdb::Resume::Kill => break Ok(()),
                    gdb::Resume::Stopped(stop) => owed = Some(stop),
                    _ => {}
                }
                // halted time is neither guest idle time nor part of --timeout
                start += halted.elapsed();
                anchor += halted.elapsed();
            }
        }
        let budget = (config.max_insns.saturating_sub(m.instret())).min(1_000_000);
        let mut stop = match owed.take() {
            Some(stop) => stop,
            None => m.host().run(budget),
        };
        if let Some(a) = audio.as_mut() {
            let (rate, samples) = m.take_audio();
            a.push(rate, &samples);
        }
        if let Some(a) = hdmi_audio.as_mut() {
            let (rate, samples) = m.take_hdmi_audio();
            a.push(rate, &samples);
        }
        let bytes = m.host().take_serial_output();
        if !bytes.is_empty() {
            output.write_all(&bytes).map_err(|e| e.to_string())?;
            output.flush().map_err(|e| e.to_string())?;
            crate::script::append_serial(&mut serial, &bytes);
        }
        // a debugger looks at its own stops and at those that would end the run
        if let Some(g) = gdb.as_mut().filter(|g| g.connected()) {
            if let Some((core, sig)) = g.stop_signal(&stop, m.stopped_core()) {
                let halted = Instant::now();
                let resume = g.halted(&mut m, core, sig, true);
                start += halted.elapsed();
                anchor += halted.elapsed();
                stop = match resume {
                    gdb::Resume::Kill => break Ok(()),
                    gdb::Resume::Stopped(stop) => stop,
                    _ => Stop::Budget,
                };
            }
        }
        if let Stop::Breakpoint(_) | Stop::Watchpoint(..) = stop {
            // the debugger went away while the guest ran
            stop = Stop::Budget;
        }
        match stop {
            Stop::Budget => last_busy = Instant::now(),
            Stop::Idle(deadline) if idle_skip() => {
                // SBC_IDLE=skip: an idle guest's time jumps to its next deadline (a batch job
                // that waits on timers, like CUDA's synchronization, need not wait them out)
                while let Ok(b) = rx.try_recv() {
                    idle_input.push(b);
                }
                m.host().idle_until(deadline);
            }
            Stop::Idle(deadline) => {
                // Virtual time never runs ahead of the wall clock while idle: sleep until the
                // deadline's wall time. (Not only while the guest is ahead: after each turn its
                // time is the wall clock a moment ago, and the loop would spin.)
                let wall = anchor.elapsed().as_nanos() as u64;
                if wall < deadline {
                    let wait = Duration::from_nanos(deadline.saturating_sub(wall).min(20_000_000).max(1));
                    match rx.recv_timeout(wait) {
                        Ok(b) => idle_input.push(b),
                        Err(mpsc::RecvTimeoutError::Timeout) => {}
                        // standard input ended: there is nothing to wait on but time
                        Err(mpsc::RecvTimeoutError::Disconnected) => std::thread::sleep(wait),
                    }
                }
                let target = deadline.min((anchor.elapsed().as_nanos() as u64).max(m.now()));
                m.host().idle_until(target);
            }
            Stop::Reset => {
                if std::mem::take(&mut save_asked) {
                    eprintln!("\r[{tag}] no snapshot: the guest has reset\r");
                }
                m = match make_machine() {
                    Ok(m) => m,
                    Err(e) => break Err(e),
                };
                if let Some(g) = gdb.as_ref() {
                    g.rearm(&mut m);
                }
                if audio.is_some() {
                    m.capture_audio();
                }
                if hdmi_audio.is_some() {
                    m.capture_hdmi_audio();
                }
                anchor = Instant::now();
                serial.clear();
                searched = 0;
                pending.clear();
            }
            Stop::PowerOff => {
                break if script.is_empty() {
                    Ok(())
                } else {
                    Err("guest powered off before script completed".into())
                }
            }
            other => break Err(format!("stopped: {other:x?}")),
        }
    };
    // asked for just before the end (Ctrl-A s, Ctrl-A x)
    if save_asked {
        if let Err(e) = save(&m, tag, &config.snapshot, pending.len()) {
            eprintln!("[{tag}] no snapshot: {e}");
        }
    }
    m.report(tag);
    if let Some(path) = config.screenshot.as_deref() {
        screenshot(&mut m, tag, path);
    }
    result
}

/// SBC_IDLE=skip: idle time passes at once instead of at the wall clock's pace.
fn idle_skip() -> bool {
    static SKIP: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *SKIP.get_or_init(|| std::env::var("SBC_IDLE").is_ok_and(|v| v == "skip"))
}
