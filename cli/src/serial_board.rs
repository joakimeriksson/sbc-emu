//! Serial-console run loop shared by the direct-boot bring-up boards (Orange Pi 5,
//! Orange Pi One): option validation, `--script` automation, host stdin with the
//! Ctrl-A x escape, wall-clock pacing of idle time and guest reset/power-off, and for
//! boards with a display `--screenshot`, `--vnc` and the `screenshot FILE` script command.
use crate::{arg, vnc::{Screen, Vnc}};
use sbc_machine::{Machine as HostMachine, Stop};
use std::{
    collections::VecDeque,
    io::{Read, Write},
    sync::mpsc,
    time::{Duration, Instant},
};

/// A board machine as the serial loop needs it.
pub(crate) trait SerialBoard {
    fn host(&mut self) -> HostMachine<'_>;
    /// Virtual time in nanoseconds.
    fn now(&self) -> u64;
    fn instret(&self) -> u64;
    /// One-line execution summary plus any unmodelled-hardware notes.
    fn report(&self, tag: &str);
    /// The board's display, if it has one.
    fn screen(&mut self) -> Option<&mut dyn Screen> {
        None
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

/// Options every serial board accepts; `extra` lists board-specific valued options.
pub(crate) fn validate_options(args: &[String], extra: &[&str]) -> Result<(), String> {
    const COMMON: [&str; 9] = [
        "--board", "--kernel", "--initrd", "--append", "--script", "--timeout", "--max-insns",
        "--ram-mb", "--smp",
    ];
    let mut index = 1;
    while index < args.len() {
        let a = args[index].as_str();
        if a.starts_with("--board=") || matches!(a, "--no-jit" | "--no-display" | "--sd-persist") {
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
    if arg(args, "--smp").is_some_and(|s| s != "1") {
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
    /// VNC server for the display (`--vnc :N`).
    pub vnc: Option<String>,
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
        let script: VecDeque<String> = match arg(args, "--script") {
            Some(p) => std::fs::read_to_string(p)
                .map_err(|e| e.to_string())?
                .lines()
                .map(str::trim)
                .filter(|s| !s.is_empty() && !s.starts_with('#'))
                .map(str::to_owned)
                .collect(),
            None => VecDeque::new(),
        };
        for line in &script {
            if !matches!(line.split_whitespace().next(), Some("wait-serial" | "serial" | "quit" | "reset" | "screenshot" | "type" | "sleep")) {
                return Err(format!("unsupported script command: {line}"));
            }
        }
        let (screenshot, vnc) = (arg(args, "--screenshot").map(str::to_owned), arg(args, "--vnc").map(str::to_owned));
        Ok(Self { tag, script, timeout, max_insns, screenshot, vnc })
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
    mut config: RunConfig,
    make_machine: impl Fn() -> Result<B, String>,
) -> Result<(), String> {
    let tag = config.tag;
    let mut m = make_machine()?;
    let mut vnc = match config.vnc.as_deref() {
        Some(spec) => Some(Vnc::listen(spec).map_err(|e| format!("--vnc {spec}: {e}"))?),
        None => None,
    };
    let _terminal = crate::terminal::RawTty::enable();
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let mut stdin = std::io::stdin();
        let mut b = [0];
        while let Ok(1) = stdin.read(&mut b) {
            if tx.send(b[0]).is_err() {
                break;
            }
        }
    });
    let script = &mut config.script;
    let mut idle_input = Vec::new();
    let mut pending = VecDeque::new();
    let mut output = std::io::stdout();
    let mut serial = String::new();
    let start = Instant::now();
    let mut anchor = Instant::now();
    let mut escape = false;
    // `sleep SECS`: the script waits until this virtual time (ns)
    let mut sleep_until: Option<u64> = None;
    let result = 'run: loop {
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
                pending.push_back(1);
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
                "wait-serial" => match serial.find(rest) {
                    // Keep output after the match: the next wait may already be in it.
                    Some(position) => {
                        consumed = position + rest.len();
                        true
                    }
                    None => false,
                },
                "serial" => {
                    // \x03 is Ctrl-C (SIGINT through the guest's tty)
                    pending.extend(
                        rest.replace("\\n", "\r").replace("\\r", "\r").replace("\\t", "\t").replace("\\x03", "\x03").bytes(),
                    );
                    true
                }
                "reset" => {
                    m = make_machine()?;
                    anchor = Instant::now();
                    serial.clear();
                    pending.clear();
                    true
                }
                "screenshot" => {
                    screenshot(&mut m, tag, rest);
                    true
                }
                "sleep" => {
                    let seconds: f64 = rest.trim().parse().map_err(|_| format!("script: {line}"))?;
                    sleep_until = Some(m.now().saturating_add((seconds * 1e9) as u64));
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
        if config.timeout.is_some_and(|t| start.elapsed() >= t) {
            break Err("host timeout before completion".into());
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
        }
        let budget = (config.max_insns - m.instret()).min(1_000_000);
        let stop = m.host().run(budget);
        let bytes = m.host().take_serial_output();
        if !bytes.is_empty() {
            output.write_all(&bytes).map_err(|e| e.to_string())?;
            output.flush().map_err(|e| e.to_string())?;
            serial.push_str(&String::from_utf8_lossy(&bytes));
            if serial.len() > 1 << 20 {
                let mut cut = serial.len() - (1 << 19);
                while !serial.is_char_boundary(cut) {
                    cut += 1;
                }
                serial.drain(..cut);
            }
        }
        match stop {
            Stop::Budget => {}
            Stop::Idle(deadline) => {
                // Virtual time never runs ahead of the wall clock while idle.
                let wall = anchor.elapsed().as_nanos() as u64;
                if wall <= m.now() {
                    let wait = Duration::from_nanos(deadline.saturating_sub(wall).min(20_000_000).max(1));
                    if let Ok(b) = rx.recv_timeout(wait) {
                        idle_input.push(b);
                    }
                }
                let target = deadline.min((anchor.elapsed().as_nanos() as u64).max(m.now()));
                m.host().idle_until(target);
            }
            Stop::Reset => {
                m = make_machine()?;
                anchor = Instant::now();
                serial.clear();
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
    m.report(tag);
    if let Some(path) = config.screenshot.as_deref() {
        screenshot(&mut m, tag, path);
    }
    result
}
