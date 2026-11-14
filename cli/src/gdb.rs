//! GDB remote serial protocol stub (`--gdb PORT`, `--gdb-wait`), as QEMU's `-gdb` / `-s -S`:
//! registers (x0-x30, sp, pc, cpsr, v0-v31, fpsr, fpcr), virtual memory as the selected core
//! sees it, software and hardware breakpoints (by pc), write/read/access watchpoints (both
//! interpreter only while set), single step, Ctrl-C, one thread per core. Works with gdb (`target remote :1234`) and lldb
//! (`gdb-remote 1234`).
use bcm2711::{Machine, Stop};
use std::io::{ErrorKind, Read, Write};
use std::net::{TcpListener, TcpStream};

pub enum Resume { Continue, Detach, Kill }

pub enum Poll { Running, Attached, Interrupt }

pub struct Gdb {
    listener: TcpListener,
    conn: Option<TcpStream>,
    /// bytes received and not yet parsed
    rx: Vec<u8>,
    /// core for register and memory requests (Hg)
    thread: usize,
    no_ack: bool,
    /// why the guest stopped, for the next stop reply ("watch:ADDR;" and the like)
    pub reason: String,
}

fn hex(b: &[u8]) -> String { b.iter().map(|x| format!("{x:02x}")).collect() }
fn unhex(s: &str) -> Option<Vec<u8>> {
    (0..s.len() / 2).map(|i| u8::from_str_radix(s.get(2 * i..2 * i + 2)?, 16).ok()).collect()
}
fn num(s: &str) -> Option<u64> { u64::from_str_radix(s, 16).ok() }

/// The stop-reply field for a watchpoint hit (gdb's "watch", "rwatch", "awatch").
pub fn watch_reason(addr: u64, kind: u8) -> String {
    format!("{}:{addr:x};", if kind == 1 { "watch" } else { "rwatch" })
}

fn target_xml() -> String {
    let mut x = String::from("<?xml version=\"1.0\"?><!DOCTYPE target SYSTEM \"gdb-target.dtd\"><target version=\"1.0\"><architecture>aarch64</architecture><feature name=\"org.gnu.gdb.aarch64.core\">");
    for i in 0..31 { x += &format!("<reg name=\"x{i}\" bitsize=\"64\"/>"); }
    x += "<reg name=\"sp\" bitsize=\"64\" type=\"data_ptr\"/><reg name=\"pc\" bitsize=\"64\" type=\"code_ptr\"/><reg name=\"cpsr\" bitsize=\"32\"/></feature><feature name=\"org.gnu.gdb.aarch64.fpu\">";
    for i in 0..32 { x += &format!("<reg name=\"v{i}\" bitsize=\"128\" type=\"uint128\"/>"); }
    x += "<reg name=\"fpsr\" bitsize=\"32\"/><reg name=\"fpcr\" bitsize=\"32\"/></feature></target>";
    x
}

/// Register `n` of core `c` in target byte order (gdb's numbering from `target_xml`).
fn reg(m: &Machine, c: usize, n: usize) -> Option<Vec<u8>> {
    let cpu = &m.cpus[c];
    Some(match n {
        0..=30 => cpu.x[n].to_le_bytes().to_vec(),
        31 => cpu.cur_sp().to_le_bytes().to_vec(),
        32 => cpu.pc.to_le_bytes().to_vec(),
        33 => (cpu.pstate() as u32).to_le_bytes().to_vec(),
        34..=65 => cpu.v[n - 34].to_le_bytes().to_vec(),
        66 => (cpu.fpsr as u32).to_le_bytes().to_vec(),
        67 => (cpu.fpcr as u32).to_le_bytes().to_vec(),
        _ => return None,
    })
}

fn set_reg(m: &mut Machine, c: usize, n: usize, b: &[u8]) -> bool {
    let cpu = &mut m.cpus[c];
    let u64v = || b.get(..8).map(|s| u64::from_le_bytes(s.try_into().unwrap()));
    let u32v = || b.get(..4).map(|s| u32::from_le_bytes(s.try_into().unwrap()) as u64);
    match n {
        0..=30 => match u64v() { Some(v) => cpu.x[n] = v, None => return false },
        31 => match u64v() { Some(v) => cpu.set_cur_sp(v), None => return false },
        32 => match u64v() { Some(v) => cpu.pc = v, None => return false },
        33 => match u32v() { Some(v) => cpu.set_pstate(v), None => return false },
        34..=65 => match b.get(..16) { Some(s) => cpu.v[n - 34] = u128::from_le_bytes(s.try_into().unwrap()), None => return false },
        66 => match u32v() { Some(v) => cpu.fpsr = v, None => return false },
        67 => match u32v() { Some(v) => cpu.fpcr = v, None => return false },
        _ => return false,
    }
    true
}

const REG_SIZES: [usize; 68] = {
    let mut s = [8; 68];
    s[33] = 4;
    let mut i = 34;
    while i < 66 { s[i] = 16; i += 1; }
    s[66] = 4;
    s[67] = 4;
    s
};

impl Gdb {
    pub fn listen(port: u16) -> std::io::Result<Gdb> {
        let listener = TcpListener::bind(("127.0.0.1", port))?;
        listener.set_nonblocking(true)?;
        eprintln!("[sbc-emu] gdb stub on 127.0.0.1:{port} (gdb: target remote :{port}; lldb: gdb-remote {port})");
        Ok(Gdb { listener, conn: None, rx: Vec::new(), thread: 0, no_ack: false, reason: String::new() })
    }

    pub fn connected(&self) -> bool { self.conn.is_some() }

    /// Block until a debugger connects.
    pub fn wait_for_client(&mut self) {
        eprintln!("[sbc-emu] waiting for a debugger to connect");
        let _ = self.listener.set_nonblocking(false);
        if let Ok((s, _)) = self.listener.accept() { self.attach(s); }
        let _ = self.listener.set_nonblocking(true);
    }

    fn attach(&mut self, s: TcpStream) {
        let _ = s.set_nodelay(true);
        eprintln!("\r[sbc-emu] debugger attached\r");
        self.conn = Some(s);
        self.rx.clear();
        self.no_ack = false;
    }

    /// While the guest runs: accept a debugger (`Attached`: stop quietly, it will ask why), or
    /// notice Ctrl-C or a packet (`Interrupt`: stop and say so).
    pub fn poll(&mut self) -> Poll {
        if self.conn.is_none() {
            if let Ok((s, _)) = self.listener.accept() { self.attach(s); return Poll::Attached; }
            return Poll::Running;
        }
        let Some(c) = self.conn.as_mut() else { return Poll::Running };
        let _ = c.set_nonblocking(true);
        let mut b = [0u8; 4096];
        match c.read(&mut b) {
            Ok(0) => { self.conn = None; Poll::Running }
            Ok(n) => {
                self.rx.extend_from_slice(&b[..n]);
                let stop = self.rx.iter().any(|&x| x != b'+' && x != b'-');
                self.rx.retain(|&x| x != 0x03);
                if stop { Poll::Interrupt } else { Poll::Running }
            }
            Err(e) if e.kind() == ErrorKind::WouldBlock => Poll::Running,
            Err(_) => { self.conn = None; Poll::Running }
        }
    }

    fn send(&mut self, body: &str) {
        let cs = body.bytes().fold(0u8, |a, b| a.wrapping_add(b));
        if let Some(c) = self.conn.as_mut() {
            let _ = c.set_nonblocking(false);
            if c.write_all(format!("${body}#{cs:02x}").as_bytes()).is_err() { self.conn = None; }
        }
    }

    /// The next packet (blocking), or None if the debugger went away.
    fn packet(&mut self) -> Option<String> {
        loop {
            // drop acks and stray interrupts before a packet
            while let Some(&b) = self.rx.first() { if b == b'$' { break; } self.rx.remove(0); }
            if let Some(hash) = self.rx.iter().position(|&b| b == b'#') {
                if self.rx.len() >= hash + 3 {
                    let body = String::from_utf8_lossy(&self.rx[1..hash]).into_owned();
                    self.rx.drain(..hash + 3);
                    if !self.no_ack { if let Some(c) = self.conn.as_mut() { let _ = c.write_all(b"+"); } }
                    return Some(body);
                }
            }
            let c = self.conn.as_mut()?;
            let _ = c.set_nonblocking(false);
            let mut b = [0u8; 4096];
            match c.read(&mut b) {
                Ok(0) | Err(_) => { self.conn = None; return None; }
                Ok(n) => self.rx.extend_from_slice(&b[..n]),
            }
        }
    }

    fn stop_reply(&mut self, sig: u8, core: usize) {
        self.thread = core;
        let why = std::mem::take(&mut self.reason);
        self.send(&format!("T{sig:02x}{why}thread:{:x};", core + 1));
    }

    /// The guest is stopped (by `sig` on core `core`): serve the debugger until it resumes.
    /// `announce`: report the stop now (breakpoint, Ctrl-C); a fresh attach waits for "?".
    pub fn halted(&mut self, m: &mut Machine, core: usize, sig: u8, announce: bool) -> Resume {
        if self.conn.is_none() { return Resume::Continue; }
        let mut stopped = (sig, core);
        if announce { self.stop_reply(sig, core); }
        loop {
            let Some(p) = self.packet() else { return Resume::Detach };
            let ncpu = m.cpus.len();
            let tid = |s: &str, cur: usize| -> usize {
                match i64::from_str_radix(s, 16) { Ok(n) if n > 0 && (n as usize) <= ncpu => n as usize - 1, _ => cur }
            };
            let reply: String = match p.as_bytes().first().copied() {
                Some(b'?') => { let (s, c) = stopped; self.stop_reply(s, c); continue; }
                Some(b'g') => (0..68).filter_map(|n| reg(m, self.thread, n)).map(|b| hex(&b)).collect(),
                Some(b'G') => match unhex(&p[1..]) {
                    Some(b) => {
                        let mut off = 0;
                        for (n, sz) in REG_SIZES.iter().enumerate() { if off + sz <= b.len() { set_reg(m, self.thread, n, &b[off..off + sz]); } off += sz; }
                        "OK".into()
                    }
                    None => "E01".into(),
                },
                Some(b'p') => match num(&p[1..]).and_then(|n| reg(m, self.thread, n as usize)) { Some(b) => hex(&b), None => "E01".into() },
                Some(b'P') => {
                    let ok = p[1..].split_once('=').and_then(|(n, v)| Some(set_reg(m, self.thread, num(n)? as usize, &unhex(v)?)));
                    if ok == Some(true) { "OK".into() } else { "E01".into() }
                }
                Some(b'm') => {
                    let r = p[1..].split_once(',').and_then(|(a, l)| m.debug_read(self.thread, num(a)?, (num(l)? as usize).min(0x4000)));
                    match r { Some(b) => hex(&b), None => "E14".into() }
                }
                Some(b'M') => {
                    let ok = p[1..].split_once(':').and_then(|(al, d)| { let (a, _) = al.split_once(',')?; Some(m.debug_write(self.thread, num(a)?, &unhex(d)?)) });
                    if ok == Some(true) { "OK".into() } else { "E14".into() }
                }
                Some(b'Z') | Some(b'z') => {
                    let mut it = p[1..].split(',');
                    let (kind, addr) = (it.next(), it.next().and_then(num));
                    match (kind, addr) {
                        (Some("0") | Some("1"), Some(a)) => {
                            if p.starts_with('Z') { if !m.breakpoints.contains(&a) { m.breakpoints.push(a); } } else { m.breakpoints.retain(|&x| x != a); }
                            let on = !m.breakpoints.is_empty() || !m.cpus[0].watch.is_empty();
                            m.set_debugging(on);
                            "OK".into()
                        }
                        // watchpoints: 2 write, 3 read, 4 access
                        (Some(k @ ("2" | "3" | "4")), Some(a)) => {
                            let len = it.next().and_then(num).unwrap_or(1).max(1);
                            let kind = match k { "2" => 1, "3" => 2, _ => 3 };
                            m.watchpoint(a, len, kind, p.starts_with('Z'));
                            if m.breakpoints.is_empty() && m.cpus[0].watch.is_empty() { m.set_debugging(false); }
                            "OK".into()
                        }
                        _ => String::new(),
                    }
                }
                Some(b'H') => { if p.len() > 2 && &p[1..2] == "g" { self.thread = tid(&p[2..], self.thread); } "OK".into() }
                Some(b'T') => if tid(&p[1..], usize::MAX) != usize::MAX { "OK".into() } else { "E01".into() },
                Some(b'c') => return Resume::Continue,
                Some(b's') => {
                    let c = self.thread;
                    stopped = self.step(m, c);
                    continue;
                }
                Some(b'D') => { self.send("OK"); m.breakpoints.clear(); for c in m.cpus.iter_mut() { c.watch.clear(); } m.set_debugging(false); self.conn = None; return Resume::Detach; }
                Some(b'k') => { self.conn = None; return Resume::Kill; }
                _ if p == "vCont?" => "vCont;c;C;s;S".into(),
                _ if p.starts_with("vCont;") => {
                    // the first step action (with its thread) wins; otherwise continue everything
                    let step = p[6..].split(';').find(|a| a.starts_with('s') || a.starts_with('S'));
                    match step {
                        Some(a) => {
                            let c = a.split_once(':').map_or(self.thread, |(_, t)| tid(t, self.thread));
                            stopped = self.step(m, c);
                            continue;
                        }
                        None => return Resume::Continue,
                    }
                }
                _ if p == "qWatchpointSupportInfo:" || p == "qWatchpointSupportInfo" => "num:16;".into(),
                _ if p.starts_with("qSupported") => "PacketSize=4000;qXfer:features:read+;vContSupported+;QStartNoAckMode+".into(),
                _ if p == "QStartNoAckMode" => { self.send("OK"); self.no_ack = true; continue; }
                _ if p.starts_with("qXfer:features:read:target.xml:") => {
                    let x = target_xml();
                    let (o, l) = p.rsplit_once(':').and_then(|(_, r)| r.split_once(',')).map(|(o, l)| (num(o).unwrap_or(0) as usize, num(l).unwrap_or(0) as usize)).unwrap_or((0, 0));
                    let part = x.get(o.min(x.len())..(o + l).min(x.len())).unwrap_or("");
                    format!("{}{}", if o + l >= x.len() { 'l' } else { 'm' }, part)
                }
                _ if p == "qAttached" => "1".into(),
                _ if p == "qC" => format!("QC{:x}", self.thread + 1),
                _ if p == "qfThreadInfo" => format!("m{}", (1..=ncpu).map(|t| format!("{t:x}")).collect::<Vec<_>>().join(",")),
                _ if p == "qsThreadInfo" => "l".into(),
                _ if p.starts_with("qThreadExtraInfo,") => {
                    let c = tid(&p[17..], 0);
                    hex(format!("CPU#{c} {:?} EL{}", m.state[c], m.cpus[c].el).as_bytes())
                }
                _ => String::new(),
            };
            self.send(&reply);
        }
    }

    /// One instruction on core `c`; the stop to report.
    fn step(&mut self, m: &mut Machine, c: usize) -> (u8, usize) {
        let r = m.debug_step(c);
        if let Some(Stop::Watchpoint(_, a, kind)) = r { self.reason = watch_reason(a, kind); }
        let sig = match r { Some(Stop::Unimplemented(..)) => 4, _ => 5 };
        self.stop_reply(sig, c);
        (sig, c)
    }
}
