//! The monitor (Ctrl-A c, or `monitor CMD` in a script), after QEMU's: inspect and control the
//! running machine.
use bcm2711::Machine;

pub enum Effect {
    None,
    Quit,
    /// pause (true) or resume (false) the guest
    Pause(bool),
    Reset,
    /// start a gdb stub on this port
    Gdb(u16),
}

const HELP: &str = "\
info registers [N]   registers of core N (default: all cores)
info cpus            cores: state, pc, exception level
info irq             interrupt lines pending at the GIC, per core
info network         virtual network and NAT counters
info jit             JIT statistics
x /COUNT ADDR        dump guest memory at a virtual address (as core 0 sees it), 8-byte words
xp /COUNT ADDR       dump guest memory at a physical address
xlate VA             translate a virtual address (core 0's tables)
stop | cont          pause or resume the guest
savevm FILE          write a snapshot      loadvm FILE   restore one (same SD image)
screendump FILE.png  save the screen
sendkey KEYS         type on the USB keyboard (text; \\n for Enter)
hostfwd_add tcp:[HOST]:PORT-[GUEST]:PORT   forward a host TCP port to the guest
gdbserver PORT       start the gdb stub
mouse_move X Y       move the USB tablet's pointer to screen pixel X, Y
mouse_button N       tablet buttons: bit 0 left, 1 right, 2 middle
sensor NAME VALUE    temp|pressure|humidity|cpu-temp
system_reset         reset the machine     quit   stop the emulator";

fn parse_num(s: &str) -> Option<u64> {
    let s = s.trim();
    if let Some(h) = s.strip_prefix("0x") { u64::from_str_radix(h, 16).ok() } else { s.parse().ok() }
}

/// Run monitor command `line`; returns its output and what the caller must do.
pub fn command(m: &mut Machine, line: &str) -> (String, Effect) {
    let mut w = line.split_whitespace();
    let (cmd, rest) = (w.next().unwrap_or(""), w.collect::<Vec<_>>());
    let mut out = String::new();
    let effect = match (cmd, rest.as_slice()) {
        ("", _) => Effect::None,
        ("help" | "?", _) => { out = HELP.into(); Effect::None }
        ("info", ["registers", tail @ ..]) => {
            let cores: Vec<usize> = match tail.first().and_then(|v| v.parse().ok()) { Some(k) if k < m.cpus.len() => vec![k], _ => (0..m.cpus.len()).collect() };
            for k in cores {
                let c = &m.cpus[k];
                out += &format!("CPU#{k} {:?} EL{} pc={:016x} sp={:016x} pstate={:08x} (NZCV {:04b})\n", m.state[k], c.el, c.pc, c.cur_sp(), c.pstate(), c.nzcv >> 28);
                for r in (0..31).step_by(4) {
                    let line: Vec<String> = (r..(r + 4).min(31)).map(|i| format!("x{i:<2}={:016x}", c.x[i])).collect();
                    out += &format!("  {}\n", line.join(" "));
                }
                out += &format!("  vbar_el1={:016x} sctlr_el1={:08x} ttbr0={:016x} ttbr1={:016x}\n  elr_el1={:016x} esr_el1={:08x} far_el1={:016x} fpcr={:08x} fpsr={:08x}\n",
                    c.vbar[1], c.sctlr[1], c.sys.ttbr0[1], c.sys.ttbr1_el1, c.elr[1], c.esr[1], c.far[1], c.fpcr, c.fpsr);
            }
            Effect::None
        }
        ("info", ["cpus", ..]) => {
            for (k, c) in m.cpus.iter().enumerate() {
                out += &format!("* CPU #{k}: {:?} pc={:#018x} EL{} instructions={}\n", m.state[k], c.pc, c.el, c.instret);
            }
            Effect::None
        }
        ("info", ["irq", ..]) => {
            for k in 0..m.cpus.len() {
                let pending: Vec<String> = (0..256u64).step_by(32).filter_map(|base| {
                    let bits = m.soc.gic.dist_read(k, 0x200 + base / 8);
                    (bits != 0).then(|| (0..32).filter(|b| bits >> b & 1 != 0).map(|b| (base + b).to_string()).collect::<Vec<_>>().join(","))
                }).collect();
                out += &format!("CPU#{k}: irq line {} pending [{}]\n", m.cpus[k].irq as u8, pending.join(","));
            }
            Effect::None
        }
        ("info", ["network", ..]) => {
            for (name, net) in [("eth0", m.soc.net.as_ref()), ("wlan0 access point", m.soc.wifi_net.as_ref())] {
                match net {
                    Some(n) => {
                        out += &format!("{name}: gateway {:?} guest {:?}: {} DHCP acks, {} DNS answers, {} pings, {} ARP replies\n", n.gw_ip, n.sta_ip, n.dhcp_acks, n.dns_answers, n.pings, n.arp_replies);
                        if let Some(nat) = n.nat.as_ref() {
                            out += &format!("  NAT: {} TCP connections ({} refused), {} UDP flows, {} bytes out, {} bytes in\n", nat.tcp_opened, nat.tcp_refused, nat.udp_flows, nat.bytes_to_host, nat.bytes_to_guest);
                        }
                    }
                    None => out += &format!("{name}: not connected\n"),
                }
            }
            Effect::None
        }
        ("info", ["jit", ..]) => {
            out = match m.jit() {
                Some(j) => format!("{} blocks compiled, {} links, {} flushes, {} block runs, {} instructions in blocks of {}\n", j.compiled, j.linked, j.flushes, j.execs, j.exec_insns, m.instret()),
                None => "JIT off\n".into(),
            };
            Effect::None
        }
        ("x" | "xp", args) => {
            let (count, addr) = match args {
                [f, a] if f.starts_with('/') => (parse_num(&f[1..].trim_end_matches(|c: char| c.is_alphabetic())).unwrap_or(4), parse_num(a)),
                [a] => (4, parse_num(a)),
                _ => (0, None),
            };
            match addr {
                Some(a) => {
                    let n = (count.min(512) * 8) as usize;
                    let data = if cmd == "x" { m.debug_read(0, a, n) } else { m.soc.ram.get(a as usize..a as usize + n).map(<[u8]>::to_vec) };
                    match data {
                        Some(d) => for (i, c) in d.chunks(16).enumerate() {
                            let words: Vec<String> = c.chunks(8).map(|w| format!("{:016x}", u64::from_le_bytes(w.try_into().unwrap_or([0; 8])))).collect();
                            out += &format!("{:016x}: {}\n", a + 16 * i as u64, words.join(" "));
                        },
                        None => out = format!("cannot access {a:#x}\n"),
                    }
                }
                None => out = "usage: x /COUNT ADDR\n".into(),
            }
            Effect::None
        }
        ("xlate", [a]) => {
            out = match parse_num(a) {
                Some(va) => { let soc = &mut m.soc; match m.cpus[0].debug_translate(soc, va) { Some(pa) => format!("{va:#x} -> {pa:#x}\n"), None => format!("{va:#x}: not mapped\n") } }
                None => "usage: xlate VA\n".into(),
            };
            Effect::None
        }
        ("stop", _) => { out = "paused\n".into(); Effect::Pause(true) }
        ("cont" | "c", _) => Effect::Pause(false),
        ("savevm", [f]) => {
            let snap = m.save();
            out = match std::fs::write(f, &snap) { Ok(()) => format!("snapshot {} MiB -> {f}\n", snap.len() >> 20), Err(e) => format!("{f}: {e}\n") };
            Effect::None
        }
        ("loadvm", [f]) => {
            out = match std::fs::read(f).map_err(|e| e.to_string()).and_then(|b| m.restore(&b)) {
                Ok(()) => format!("restored {f}: guest time {:.1} s\n", m.now as f64 / 1e9),
                Err(e) => format!("{f}: {e}\n"),
            };
            Effect::None
        }
        ("screendump", [f]) => {
            let mut rgba = Vec::new();
            out = match m.soc.render(&mut rgba) {
                Some((w, h)) => match std::fs::write(f, bcm2711::png::encode_rgba(w, h, &rgba)) { Ok(()) => format!("{w}x{h} -> {f}\n"), Err(e) => format!("{f}: {e}\n") },
                None => "no framebuffer\n".into(),
            };
            Effect::None
        }
        ("sendkey", _) => {
            let text = line.trim_start().strip_prefix("sendkey").unwrap_or("").trim_start();
            for b in text.replace("\\n", "\r").bytes() { m.soc.devs.kbd.type_byte(b); }
            Effect::None
        }
        ("hostfwd_add", [spec]) => {
            let parsed = spec.strip_prefix("tcp:").and_then(|r| r.split_once('-')).and_then(|(h, g)| {
                let (ha, hp) = h.rsplit_once(':')?;
                let gp = g.rsplit_once(':').map_or(g, |(_, p)| p).parse::<u16>().ok()?;
                let host: std::net::SocketAddr = format!("{}:{}", if ha.is_empty() { "127.0.0.1" } else { ha }, hp).parse().ok()?;
                Some((host, gp))
            });
            out = match parsed {
                Some((h, g)) => match m.forward_tcp(h, g) { Ok(()) => format!("forwarding {h} -> guest port {g}\n"), Err(e) => format!("{e}\n") },
                None => "usage: hostfwd_add tcp:[HOST]:PORT-[GUEST]:PORT\n".into(),
            };
            Effect::None
        }
        ("gdbserver", [p]) => match p.trim_start_matches("tcp::").parse() { Ok(port) => Effect::Gdb(port), Err(_) => { out = "usage: gdbserver PORT\n".into(); Effect::None } },
        ("mouse_move", [x, y]) => {
            let (w, h) = m.soc.display_size();
            let scale = |v: &str, n: u32| v.parse::<u32>().ok().map(|v| (v.min(n.saturating_sub(1)) * 32767 / n.saturating_sub(1).max(1)) as u16);
            match (scale(x, w), scale(y, h)) {
                (Some(x), Some(y)) => { let b = m.soc.devs.tablet.state.0; m.soc.devs.tablet.pointer(b, x, y, 0); }
                _ => out = "usage: mouse_move X Y\n".into(),
            }
            Effect::None
        }
        ("mouse_button", [b]) => {
            match b.parse::<u8>() {
                Ok(b) => { let (_, x, y) = m.soc.devs.tablet.state; m.soc.devs.tablet.pointer(b & 7, x, y, 0); }
                Err(_) => out = "usage: mouse_button N\n".into(),
            }
            Effect::None
        }
        ("sensor", [name, v]) => { if let Ok(v) = v.parse() { m.set_sensor(name, v); } Effect::None }
        ("system_reset", _) => Effect::Reset,
        ("quit" | "q", _) => Effect::Quit,
        _ => { out = format!("unknown command: {line} (help lists them)\n"); Effect::None }
    };
    (out, effect)
}
