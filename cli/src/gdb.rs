//! GDB remote serial protocol stub (`--gdb PORT`, `--gdb-wait`), as QEMU's `-gdb` / `-s -S`, for
//! every board (`Target`): the core's registers, virtual memory as the selected core
//! sees it, software and hardware breakpoints (by pc), write/read/access watchpoints (both
//! interpreter only while set), single step, Ctrl-C, one thread per core. Works with gdb (`target remote :1234`) and lldb
//! (`gdb-remote 1234`).
use sbc_machine::Stop;
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
    /// what the debugger has set: (0 a breakpoint or a watch kind, address, length)
    points: Vec<(u8, u64, u64)>,
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

/// A machine as the stub debugs it.
pub trait Target {
    fn cores(&self) -> usize;
    /// gdb's register description: the XML, and each register's bytes in its order.
    fn layout(&self) -> Layout;
    /// Register `n` of core `c` in target byte order.
    fn reg(&self, c: usize, n: usize) -> Option<Vec<u8>>;
    fn set_reg(&mut self, c: usize, n: usize, b: &[u8]) -> bool;
    /// Memory at virtual address `va` as core `c` sees it.
    fn read(&mut self, c: usize, va: u64, len: usize) -> Option<Vec<u8>>;
    fn write(&mut self, c: usize, va: u64, data: &[u8]) -> bool;
    fn breakpoint(&mut self, addr: u64, insert: bool);
    /// Watch [addr, addr + len): `kind` 1 write, 2 read, 3 either.
    fn watchpoint(&mut self, addr: u64, len: u64, kind: u8, insert: bool);
    /// Execute one instruction on core `c`; a stop it causes, if any.
    fn step(&mut self, c: usize) -> Option<Stop>;
    /// A line about core `c` for "info threads".
    fn describe(&self, c: usize) -> String;
    /// The core to blame for a stop that does not name one.
    fn stopped_core(&self) -> usize { 0 }
}

pub struct Layout { xml: String, sizes: Vec<usize> }

impl Layout {
    fn new(arch: &str) -> Layout {
        Layout { xml: format!("<?xml version=\"1.0\"?><!DOCTYPE target SYSTEM \"gdb-target.dtd\"><target version=\"1.0\"><architecture>{arch}</architecture>"), sizes: Vec::new() }
    }
    fn feature(mut self, name: &str, regs: impl IntoIterator<Item = (String, usize, &'static str)>) -> Layout {
        self.xml += &format!("<feature name=\"{name}\">");
        for (name, bytes, ty) in regs {
            let ty = if ty.is_empty() { String::new() } else { format!(" type=\"{ty}\"") };
            self.xml += &format!("<reg name=\"{name}\" bitsize=\"{}\"{ty}/>", bytes * 8);
            self.sizes.push(bytes);
        }
        self.xml += "</feature>";
        self
    }
    fn end(mut self) -> Layout { self.xml += "</target>"; self }
}

fn one(name: &str, bytes: usize, ty: &'static str) -> (String, usize, &'static str) { (name.into(), bytes, ty) }
fn u32_of(b: &[u8]) -> Option<u32> { Some(u32::from_le_bytes(b.get(..4)?.try_into().ok()?)) }
fn u64_of(b: &[u8]) -> Option<u64> { Some(u64::from_le_bytes(b.get(..8)?.try_into().ok()?)) }

// ---------------------------------------------------------------- AArch64

fn a64_layout() -> Layout {
    Layout::new("aarch64")
        .feature("org.gnu.gdb.aarch64.core", (0..31).map(|i| (format!("x{i}"), 8, "")).chain([one("sp", 8, "data_ptr"), one("pc", 8, "code_ptr"), one("cpsr", 4, "")]))
        .feature("org.gnu.gdb.aarch64.fpu", (0..32).map(|i| (format!("v{i}"), 16, "uint128")).chain([one("fpsr", 4, ""), one("fpcr", 4, "")]))
        .end()
}

fn a64_reg(cpu: &arm_a64::Cpu, n: usize) -> Option<Vec<u8>> {
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

fn a64_set_reg(cpu: &mut arm_a64::Cpu, n: usize, b: &[u8]) -> Option<()> {
    match n {
        0..=30 => cpu.x[n] = u64_of(b)?,
        31 => cpu.set_cur_sp(u64_of(b)?),
        32 => cpu.pc = u64_of(b)?,
        33 => cpu.set_pstate(u32_of(b)? as u64),
        34..=65 => cpu.v[n - 34] = u128::from_le_bytes(b.get(..16)?.try_into().ok()?),
        66 => cpu.fpsr = u32_of(b)? as u64,
        67 => cpu.fpcr = u32_of(b)? as u64,
        _ => return None,
    }
    Some(())
}

impl Target for bcm2711::Machine {
    fn cores(&self) -> usize { self.cpus.len() }
    fn layout(&self) -> Layout { a64_layout() }
    fn reg(&self, c: usize, n: usize) -> Option<Vec<u8>> { a64_reg(&self.cpus[c], n) }
    fn set_reg(&mut self, c: usize, n: usize, b: &[u8]) -> bool { a64_set_reg(&mut self.cpus[c], n, b).is_some() }
    fn read(&mut self, c: usize, va: u64, len: usize) -> Option<Vec<u8>> { self.debug_read(c, va, len) }
    fn write(&mut self, c: usize, va: u64, data: &[u8]) -> bool { self.debug_write(c, va, data) }
    fn breakpoint(&mut self, addr: u64, insert: bool) {
        self.breakpoints.retain(|&x| x != addr);
        if insert { self.breakpoints.push(addr); }
        let on = !self.breakpoints.is_empty() || !self.cpus[0].watch.is_empty();
        self.set_debugging(on);
    }
    fn watchpoint(&mut self, addr: u64, len: u64, kind: u8, insert: bool) { bcm2711::Machine::watchpoint(self, addr, len, kind, insert) }
    fn step(&mut self, c: usize) -> Option<Stop> { self.debug_step(c) }
    fn describe(&self, c: usize) -> String { format!("CPU#{c} {:?} EL{}", self.state[c], self.cpus[c].el) }
}

impl Target for rk3588::Machine {
    fn cores(&self) -> usize { self.cpus.len() }
    fn layout(&self) -> Layout { a64_layout() }
    fn reg(&self, c: usize, n: usize) -> Option<Vec<u8>> { a64_reg(&self.cpus[c], n) }
    fn set_reg(&mut self, c: usize, n: usize, b: &[u8]) -> bool { a64_set_reg(&mut self.cpus[c], n, b).is_some() }
    fn read(&mut self, c: usize, va: u64, len: usize) -> Option<Vec<u8>> { self.debug_read(c, va, len) }
    fn write(&mut self, c: usize, va: u64, data: &[u8]) -> bool { self.debug_write(c, va, data) }
    fn breakpoint(&mut self, addr: u64, insert: bool) { self.debug_breakpoint(addr, insert) }
    fn watchpoint(&mut self, addr: u64, len: u64, kind: u8, insert: bool) { self.debug_watch(addr, len, kind, insert) }
    fn step(&mut self, c: usize) -> Option<Stop> { self.debug_step(c) }
    fn describe(&self, c: usize) -> String { format!("CPU#{c} {} {:?} EL{}", if c < 4 { "Cortex-A55" } else { "Cortex-A76" }, self.state[c], self.cpus[c].el) }
}

impl Target for tegra210::Machine {
    fn cores(&self) -> usize { self.cpus.len() }
    fn layout(&self) -> Layout { a64_layout() }
    fn reg(&self, c: usize, n: usize) -> Option<Vec<u8>> { a64_reg(&self.cpus[c], n) }
    fn set_reg(&mut self, c: usize, n: usize, b: &[u8]) -> bool { a64_set_reg(&mut self.cpus[c], n, b).is_some() }
    fn read(&mut self, c: usize, va: u64, len: usize) -> Option<Vec<u8>> { self.debug_read(c, va, len) }
    fn write(&mut self, c: usize, va: u64, data: &[u8]) -> bool { self.debug_write(c, va, data) }
    fn breakpoint(&mut self, addr: u64, insert: bool) { self.debug_breakpoint(addr, insert) }
    fn watchpoint(&mut self, addr: u64, len: u64, kind: u8, insert: bool) { self.debug_watch(addr, len, kind, insert) }
    fn step(&mut self, c: usize) -> Option<Stop> { self.debug_step(c) }
    fn describe(&self, c: usize) -> String { format!("CPU#{c} {:?} EL{}", self.state[c], self.cpus[c].el) }
}

// ---------------------------------------------------------------- 32-bit Arm

fn a32_layout() -> Layout {
    Layout::new("arm")
        .feature("org.gnu.gdb.arm.core", (0..13).map(|i| (format!("r{i}"), 4, "uint32")).chain([one("sp", 4, "data_ptr"), one("lr", 4, ""), one("pc", 4, "code_ptr"), one("cpsr", 4, "")]))
        .feature("org.gnu.gdb.arm.vfp", (0..32).map(|i| (format!("d{i}"), 8, "ieee_double")).chain([one("fpscr", 4, "int")]))
        .end()
}

fn a32_reg(cpu: &arm_a32::Cpu, n: usize) -> Option<Vec<u8>> {
    Some(match n {
        0..=15 => cpu.r[n].to_le_bytes().to_vec(),
        16 => cpu.cpsr.to_le_bytes().to_vec(),
        17..=48 => cpu.vfp.d[n - 17].to_le_bytes().to_vec(),
        49 => cpu.vfp.fpscr.to_le_bytes().to_vec(),
        _ => return None,
    })
}

fn a32_set_reg(cpu: &mut arm_a32::Cpu, n: usize, b: &[u8]) -> Option<()> {
    match n {
        0..=15 => cpu.r[n] = u32_of(b)?,
        // not the mode: its banked registers would have to move with it
        16 => { let v = u32_of(b)?; if (v ^ cpu.cpsr) & 0x1f != 0 { return None; } cpu.cpsr = v; }
        17..=48 => cpu.vfp.d[n - 17] = u64_of(b)?,
        49 => cpu.vfp.fpscr = u32_of(b)?,
        _ => return None,
    }
    Some(())
}

fn a32_mode(cpsr: u32) -> &'static str {
    match cpsr & 0x1f { 0x10 => "USR", 0x11 => "FIQ", 0x12 => "IRQ", 0x13 => "SVC", 0x16 => "MON", 0x17 => "ABT", 0x1a => "HYP", 0x1b => "UND", 0x1f => "SYS", _ => "?" }
}

macro_rules! a32_target {
    ($machine:ty) => {
        impl Target for $machine {
            fn cores(&self) -> usize { self.cpus.len() }
            fn layout(&self) -> Layout { a32_layout() }
            fn reg(&self, c: usize, n: usize) -> Option<Vec<u8>> { a32_reg(&self.cpus[c], n) }
            fn set_reg(&mut self, c: usize, n: usize, b: &[u8]) -> bool { a32_set_reg(&mut self.cpus[c], n, b).is_some() }
            fn read(&mut self, c: usize, va: u64, len: usize) -> Option<Vec<u8>> { self.debug_read(c, va, len) }
            fn write(&mut self, c: usize, va: u64, data: &[u8]) -> bool { self.debug_write(c, va, data) }
            fn breakpoint(&mut self, addr: u64, insert: bool) { self.debug_breakpoint(addr, insert) }
            fn watchpoint(&mut self, addr: u64, len: u64, kind: u8, insert: bool) { self.debug_watch(addr, len, kind, insert) }
            fn step(&mut self, c: usize) -> Option<Stop> { self.debug_step(c) }
            fn describe(&self, c: usize) -> String { format!("CPU#{c} {:?} {}{}", self.state[c], a32_mode(self.cpus[c].cpsr), if self.cpus[c].cpsr & 0x20 != 0 { " Thumb" } else { "" }) }
            fn stopped_core(&self) -> usize { self.last_stop_core }
        }
    };
}
a32_target!(allwinner_h3::Machine);
a32_target!(rockchip_rk3288::Machine);

// ---------------------------------------------------------------- RISC-V

const RV_X: [&str; 32] = ["zero", "ra", "sp", "gp", "tp", "t0", "t1", "t2", "fp", "s1", "a0", "a1", "a2", "a3", "a4", "a5", "a6", "a7", "s2", "s3", "s4", "s5", "s6", "s7", "s8", "s9", "s10", "s11", "t3", "t4", "t5", "t6"];
const RV_F: [&str; 32] = ["ft0", "ft1", "ft2", "ft3", "ft4", "ft5", "ft6", "ft7", "fs0", "fs1", "fa0", "fa1", "fa2", "fa3", "fa4", "fa5", "fa6", "fa7", "fs2", "fs3", "fs4", "fs5", "fs6", "fs7", "fs8", "fs9", "fs10", "fs11", "ft8", "ft9", "ft10", "ft11"];

/// x0-x31, pc, f0-f31, fflags, frm, fcsr.
fn rv_layout(arch: &str, x: usize, f: usize) -> Layout {
    let ty = |n: &str| match n { "ra" => "code_ptr", "sp" | "gp" | "tp" | "fp" => "data_ptr", _ => "int" };
    Layout::new(arch)
        .feature("org.gnu.gdb.riscv.cpu", RV_X.iter().map(move |n| one(n, x, ty(n))).chain([one("pc", x, "code_ptr")]))
        .feature("org.gnu.gdb.riscv.fpu", RV_F.iter().map(move |n| one(n, f, if f == 8 { "ieee_double" } else { "ieee_single" })).chain([one("fflags", 4, "int"), one("frm", 4, "int"), one("fcsr", 4, "int")]))
        .end()
}

fn prv(p: u64) -> char { match p { 0 => 'U', 1 => 'S', _ => 'M' } }

impl Target for starfive_jh7110::Machine {
    fn cores(&self) -> usize { self.harts.len() }
    fn layout(&self) -> Layout { rv_layout("riscv:rv64", 8, 8) }
    fn reg(&self, c: usize, n: usize) -> Option<Vec<u8>> {
        let h = &self.harts[c];
        Some(match n {
            0..=31 => h.x[n].to_le_bytes().to_vec(),
            32 => h.pc.to_le_bytes().to_vec(),
            33..=64 => h.f[n - 33].to_le_bytes().to_vec(),
            65 => (h.fcsr as u32 & 0x1f).to_le_bytes().to_vec(),
            66 => (h.fcsr as u32 >> 5 & 7).to_le_bytes().to_vec(),
            67 => (h.fcsr as u32).to_le_bytes().to_vec(),
            _ => return None,
        })
    }
    fn set_reg(&mut self, c: usize, n: usize, b: &[u8]) -> bool {
        let h = &mut self.harts[c];
        let set = || -> Option<()> {
            match n {
                0 => {}
                1..=31 => h.x[n] = u64_of(b)?,
                32 => h.pc = u64_of(b)?,
                33..=64 => h.f[n - 33] = u64_of(b)?,
                65 => h.fcsr = h.fcsr & !0x1f | (u32_of(b)? & 0x1f) as u64,
                66 => h.fcsr = h.fcsr & !0xe0 | ((u32_of(b)? & 7) << 5) as u64,
                67 => h.fcsr = (u32_of(b)? & 0xff) as u64,
                _ => return None,
            }
            Some(())
        };
        let mut set = set;
        set().is_some()
    }
    fn read(&mut self, c: usize, va: u64, len: usize) -> Option<Vec<u8>> { self.debug_read(c, va, len) }
    fn write(&mut self, c: usize, va: u64, data: &[u8]) -> bool { self.debug_write(c, va, data) }
    fn breakpoint(&mut self, addr: u64, insert: bool) { self.debug_breakpoint(addr, insert) }
    fn watchpoint(&mut self, addr: u64, len: u64, kind: u8, insert: bool) { self.debug_watch(addr, len, kind, insert) }
    fn step(&mut self, c: usize) -> Option<Stop> { self.debug_step(c) }
    /// Not hart 0, the S7 monitor core: Linux leaves it off.
    fn stopped_core(&self) -> usize { self.state.iter().position(|s| *s == starfive_jh7110::HartState::Run).unwrap_or(0) }
    fn describe(&self, c: usize) -> String { format!("hart {c} {:?} {}{}", self.state[c], prv(self.harts[c].prv), if self.harts[c].wfi { " wfi" } else { "" }) }
}

impl Target for ::esp32s31::Machine {
    fn cores(&self) -> usize { self.cores.len() }
    fn layout(&self) -> Layout { rv_layout("riscv:rv32", 4, 4) }
    fn reg(&self, c: usize, n: usize) -> Option<Vec<u8>> {
        let h = &self.cores[c];
        Some(match n {
            0..=31 => h.x[n], 32 => h.pc, 33..=64 => h.f[n - 33],
            65 => h.fcsr & 0x1f, 66 => h.fcsr >> 5 & 7, 67 => h.fcsr,
            _ => return None,
        }.to_le_bytes().to_vec())
    }
    fn set_reg(&mut self, c: usize, n: usize, b: &[u8]) -> bool {
        let h = &mut self.cores[c];
        let Some(v) = u32_of(b) else { return false };
        match n {
            0 => {}
            1..=31 => h.x[n] = v,
            32 => h.pc = v,
            33..=64 => h.f[n - 33] = v,
            65 => h.fcsr = h.fcsr & !0x1f | v & 0x1f,
            66 => h.fcsr = h.fcsr & !0xe0 | (v & 7) << 5,
            67 => h.fcsr = v & 0xff,
            _ => return false,
        }
        true
    }
    fn read(&mut self, c: usize, va: u64, len: usize) -> Option<Vec<u8>> { ::esp32s31::debug::read(self, c, va as u32, len) }
    fn write(&mut self, c: usize, va: u64, data: &[u8]) -> bool { ::esp32s31::debug::write(self, c, va as u32, data) }
    fn breakpoint(&mut self, addr: u64, insert: bool) { self.debug_breakpoint(addr as u32, insert) }
    fn watchpoint(&mut self, addr: u64, len: u64, kind: u8, insert: bool) { self.debug_watch(addr as u32, len as u32, kind, insert) }
    fn step(&mut self, c: usize) -> Option<Stop> {
        match self.debug_step(c)? {
            esp_soc::Stop::Watchpoint(k, a, kind) => Some(Stop::Watchpoint(k, a as u64, kind)),
            esp_soc::Stop::Unimplemented(pc, i) => Some(Stop::Unimplemented(pc as u64, i)),
            _ => None,
        }
    }
    fn describe(&self, c: usize) -> String { format!("hart {c} {}{}", prv(self.cores[c].priv_mode as u64), if self.cores[c].waiting { " wfi" } else { "" }) }
}

impl Gdb {
    pub fn listen(port: u16) -> std::io::Result<Gdb> {
        let listener = TcpListener::bind(("127.0.0.1", port))?;
        listener.set_nonblocking(true)?;
        eprintln!("[sbc-emu] gdb stub on 127.0.0.1:{port} (gdb: target remote :{port}; lldb: gdb-remote {port})");
        Ok(Gdb { listener, conn: None, rx: Vec::new(), thread: 0, no_ack: false, reason: String::new(), points: Vec::new() })
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
    pub fn poll(&mut self, m: &mut dyn Target) -> Poll {
        if self.conn.is_none() {
            if let Ok((s, _)) = self.listener.accept() { self.attach(s); return Poll::Attached; }
            return Poll::Running;
        }
        let Some(c) = self.conn.as_mut() else { return Poll::Running };
        let _ = c.set_nonblocking(true);
        let mut b = [0u8; 4096];
        match c.read(&mut b) {
            // gone without detaching: the guest runs on, with nothing set
            Ok(0) => { self.conn = None; self.forget(m); Poll::Running }
            Ok(n) => {
                self.rx.extend_from_slice(&b[..n]);
                let stop = self.rx.iter().any(|&x| x != b'+' && x != b'-');
                self.rx.retain(|&x| x != 0x03);
                if stop { Poll::Interrupt } else { Poll::Running }
            }
            Err(e) if e.kind() == ErrorKind::WouldBlock => Poll::Running,
            Err(_) => { self.conn = None; self.forget(m); Poll::Running }
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
    pub fn halted(&mut self, m: &mut dyn Target, core: usize, sig: u8, announce: bool) -> Resume {
        if self.conn.is_none() { return Resume::Continue; }
        let mut stopped = (sig, core);
        if announce { self.stop_reply(sig, core); }
        loop {
            // a debugger that just goes away leaves nothing set
            let Some(p) = self.packet() else { self.forget(m); return Resume::Detach };
            let ncpu = m.cores();
            let tid = |s: &str, cur: usize| -> usize {
                match i64::from_str_radix(s, 16) { Ok(n) if n > 0 && (n as usize) <= ncpu => n as usize - 1, _ => cur }
            };
            let reply: String = match p.as_bytes().first().copied() {
                Some(b'?') => { let (s, c) = stopped; self.stop_reply(s, c); continue; }
                Some(b'g') => (0..m.layout().sizes.len()).filter_map(|n| m.reg(self.thread, n)).map(|b| hex(&b)).collect(),
                Some(b'G') => match unhex(&p[1..]) {
                    Some(b) => {
                        let mut off = 0;
                        for (n, sz) in m.layout().sizes.into_iter().enumerate() { if off + sz <= b.len() { m.set_reg(self.thread, n, &b[off..off + sz]); } off += sz; }
                        "OK".into()
                    }
                    None => "E01".into(),
                },
                Some(b'p') => match num(&p[1..]).and_then(|n| m.reg(self.thread, n as usize)) { Some(b) => hex(&b), None => "E01".into() },
                Some(b'P') => {
                    let ok = p[1..].split_once('=').and_then(|(n, v)| Some(m.set_reg(self.thread, num(n)? as usize, &unhex(v)?)));
                    if ok == Some(true) { "OK".into() } else { "E01".into() }
                }
                Some(b'm') => {
                    let r = p[1..].split_once(',').and_then(|(a, l)| m.read(self.thread, num(a)?, (num(l)? as usize).min(0x4000)));
                    match r { Some(b) => hex(&b), None => "E14".into() }
                }
                Some(b'M') => {
                    let ok = p[1..].split_once(':').and_then(|(al, d)| { let (a, _) = al.split_once(',')?; Some(m.write(self.thread, num(a)?, &unhex(d)?)) });
                    if ok == Some(true) { "OK".into() } else { "E14".into() }
                }
                Some(b'Z') | Some(b'z') => {
                    let mut it = p[1..].split(',');
                    let (kind, addr) = (it.next(), it.next().and_then(num));
                    match (kind, addr) {
                        (Some("0") | Some("1"), Some(a)) => { self.point(m, (0, a, 0), p.starts_with('Z')); "OK".into() }
                        // watchpoints: 2 write, 3 read, 4 access
                        (Some(k @ ("2" | "3" | "4")), Some(a)) => {
                            let len = it.next().and_then(num).unwrap_or(1).max(1);
                            let kind = match k { "2" => 1, "3" => 2, _ => 3 };
                            self.point(m, (kind, a, len), p.starts_with('Z'));
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
                Some(b'D') => { self.send("OK"); self.forget(m); self.conn = None; return Resume::Detach; }
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
                    let x = m.layout().xml;
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
                    hex(m.describe(c).as_bytes())
                }
                _ => String::new(),
            };
            self.send(&reply);
        }
    }

    /// Set or clear a breakpoint (kind 0) or watchpoint.
    fn point(&mut self, m: &mut dyn Target, point: (u8, u64, u64), insert: bool) {
        self.points.retain(|&p| p != point);
        if insert { self.points.push(point); }
        Self::apply(m, point, insert);
    }

    fn apply(m: &mut dyn Target, (kind, addr, len): (u8, u64, u64), insert: bool) {
        if kind == 0 { m.breakpoint(addr, insert); } else { m.watchpoint(addr, len, kind, insert); }
    }

    fn forget(&mut self, m: &mut dyn Target) { for p in std::mem::take(&mut self.points) { Self::apply(m, p, false); } }

    /// The machine was made anew (a reset): set what the debugger has set on it too.
    pub fn rearm(&self, m: &mut dyn Target) { for &p in &self.points { Self::apply(m, p, true); } }

    /// Whether a connected debugger gets `stop`: the core and the signal to report.
    pub fn stop_signal(&mut self, stop: &Stop, core: usize) -> Option<(usize, u8)> {
        match *stop {
            Stop::Breakpoint(k) => Some((k, 5)),
            Stop::Watchpoint(k, a, kind) => { self.reason = watch_reason(a, kind); Some((k, 5)) }
            Stop::Unimplemented(..) => Some((core, 4)),
            Stop::Brk(_) | Stop::Hlt(_) | Stop::ExceptionStorm { .. } => Some((core, 5)),
            _ => None,
        }
    }

    /// One instruction on core `c`; the stop to report.
    fn step(&mut self, m: &mut dyn Target, c: usize) -> (u8, usize) {
        let r = m.step(c);
        if let Some(Stop::Watchpoint(_, a, kind)) = r { self.reason = watch_reason(a, kind); }
        let sig = match r { Some(Stop::Unimplemented(..)) => 4, _ => 5 };
        self.stop_reply(sig, c);
        (sig, c)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use sbc_machine::Machine as Host;

    const A64: [u32; 4] = [0x9100_0421, 0xb900_0041, 0xb940_0043, 0x17ff_fffd]; // add x1,x1,#1; str w1,[x2]; ldr w3,[x2]; b
    const A32: [u32; 4] = [0xe281_1001, 0xe582_1000, 0xe592_3000, 0xeaff_fffb]; // add r1,r1,#1; str r1,[r2]; ldr r3,[r2]; b
    const RV: [u32; 4] = [0x0015_8593, 0x00b6_2023, 0x0006_2683, 0xff5f_f06f]; // addi a1,a1,1; sw a1,(a2); lw a3,(a2); j

    fn pc<M: Target>(m: &M, c: usize) -> u64 { reg(m, c, m.layout().xml_index("pc")) }
    fn reg<M: Target>(m: &M, c: usize, n: usize) -> u64 {
        let mut b = [0; 8];
        let r = m.reg(c, n).unwrap();
        b[..r.len()].copy_from_slice(&r);
        u64::from_le_bytes(b)
    }

    impl Layout {
        fn xml_index(&self, name: &str) -> usize { self.xml.split("<reg name=\"").skip(1).position(|r| r.starts_with(&format!("{name}\""))).unwrap() }
    }

    /// Debug the loop `program` at `code` (its counter in register `counter`, storing at `data`)
    /// on core `c`.
    fn exercise<M: Target>(m: &mut M, run: fn(&mut M, u64) -> Stop, c: usize, program: [u32; 4], code: u64, data: u64, counter: usize) {
        let layout = m.layout();
        assert_eq!(layout.sizes.len(), layout.xml.matches("<reg ").count());
        for (n, size) in layout.sizes.iter().enumerate() { assert_eq!(m.reg(c, n).map(|b| b.len()), Some(*size), "register {n}"); }
        assert!(m.reg(c, layout.sizes.len()).is_none());
        // memory
        let bytes: Vec<u8> = program.iter().flat_map(|w| w.to_le_bytes()).collect();
        assert_eq!(m.read(c, code, 16).unwrap(), bytes);
        assert!(m.write(c, data, b"debug"));
        assert_eq!(m.read(c, data, 5).unwrap(), b"debug");
        // registers
        let word = layout.sizes[counter];
        assert!(m.set_reg(c, counter, &vec![0; word]));
        // steps
        assert_eq!(pc(m, c), code);
        assert_eq!(m.step(c), None);
        assert_eq!((pc(m, c), reg(m, c, counter)), (code + 4, 1));
        for _ in 0..3 { assert_eq!(m.step(c), None); }
        assert_eq!((pc(m, c), reg(m, c, counter)), (code, 1));
        // a breakpoint: in front of its instruction, once per time round
        m.breakpoint(code + 8, true);
        for k in 2..6 {
            assert_eq!(run(m, 10_000), Stop::Breakpoint(c));
            assert_eq!((pc(m, c), reg(m, c, counter)), (code + 8, k));
        }
        // stepping off it executes it, and it still works afterwards
        assert_eq!(m.step(c), None);
        assert_eq!(pc(m, c), code + 12);
        assert_eq!(run(m, 10_000), Stop::Breakpoint(c));
        assert_eq!(reg(m, c, counter), 6);
        m.breakpoint(code + 8, false);
        // watchpoints: after the access, of their kind only
        m.watchpoint(data, 4, 1, true);
        assert_eq!(run(m, 10_000), Stop::Watchpoint(c, data, 1));
        assert_eq!((pc(m, c), reg(m, c, counter)), (code + 8, 7));
        // every time, not just while nothing has cached the page's translation
        for k in 8..12 {
            assert_eq!(run(m, 10_000), Stop::Watchpoint(c, data, 1));
            assert_eq!((pc(m, c), reg(m, c, counter)), (code + 8, k));
        }
        m.watchpoint(data, 4, 1, false);
        m.watchpoint(data + 2, 1, 2, true);
        for k in 11..15 {
            assert_eq!(run(m, 10_000), Stop::Watchpoint(c, data + 2, 2));
            assert_eq!((pc(m, c), reg(m, c, counter)), (code + 12, k));
        }
        m.watchpoint(data + 2, 1, 2, false);
        m.watchpoint(data + 64, 4, 3, true);
        assert_eq!(run(m, 1_000), Stop::Budget);
        m.watchpoint(data + 64, 4, 3, false);
        // nothing set: it runs freely
        let before = reg(m, c, counter);
        assert_eq!(run(m, 20_000), Stop::Budget);
        assert!(reg(m, c, counter) > before + 1000);
    }

    fn place(ram: &mut [u8], at: usize, program: [u32; 4]) { for (k, w) in program.iter().enumerate() { ram[at + 4 * k..][..4].copy_from_slice(&w.to_le_bytes()); } }

    /// A 32-bit program under a 64-bit kernel, compiled by the 32-bit JIT: breakpoints stop in
    /// the middle of its block, a step is one instruction, a write changes what runs.
    fn exercise_32_bit<M: Target>(m: &mut M, run: fn(&mut M, u64) -> Stop, base: u64) {
        let (code, data) = (base + 0x10_0000, base + 0x20_0000);
        let read = |m: &mut M, a: u64| m.read(0, a, 4).map(|b| u32::from_le_bytes(b.try_into().unwrap())).unwrap();
        let pc = |m: &M| u64::from_le_bytes(m.reg(0, 32).unwrap().try_into().unwrap());
        run(m, 2_000_000); // hot and compiled
        assert_eq!(read(m, data), 1);
        for n in [1, 2] {
            m.breakpoint(code + 4 * n, true); // not where the block starts
            assert!(matches!(run(m, 1_000_000), Stop::Breakpoint(0)), "instruction {n}");
            assert_eq!(pc(m), code + 4 * n);
            assert_eq!(m.step(0), None);
            assert_eq!(pc(m), code + 4 * n + 4);
            m.breakpoint(code + 4 * n, false);
            run(m, 1_000_000);
        }
        for value in [2u32, 3] {
            assert!(m.write(0, code, &(0xe3a0_0000 | value).to_le_bytes())); // mov r0, #value
            run(m, 1_000_000);
            assert_eq!(read(m, data), value);
        }
    }

    /// EL1 at `base` entering a 32-bit loop at `base` + 1 MiB that stores r0 at `base` + 2 MiB.
    fn place_32_bit(ram: &mut [u8], at: usize, base: u64) {
        let hi = |a: u64| ((a >> 16) as u32) << 5;
        let el1 = [
            0xd280_0200,                          // mov x0, #0x10 (AArch32 user mode)
            0xd518_4000,                          // msr spsr_el1, x0
            0xd2a0_0001 | hi(base + 0x10_0000),   // movz x1, #code, lsl #16
            0xd518_4021,                          // msr elr_el1, x1
            0xd2a0_0002 | hi(base + 0x20_0000),   // movz x2, #data, lsl #16
            0xd69f_03e0,                          // eret
        ];
        // mov r0, #1; add r3, r3, #1; add r4, r4, r3; str r0, [r2]; b
        let user = [0xe3a0_0001u32, 0xe283_3001, 0xe084_4003, 0xe582_0000, 0xeaff_fffa];
        for (o, w) in el1.iter().enumerate().map(|(k, w)| (at + 4 * k, w)).chain(user.iter().enumerate().map(|(k, w)| (at + 0x10_0000 + 4 * k, w))) {
            ram[o..o + 4].copy_from_slice(&w.to_le_bytes());
        }
    }

    #[test]
    fn a_32_bit_program_under_a_64_bit_kernel() {
        let mut m = bcm2711::Machine::new(65 << 20);
        place_32_bit(&mut m.soc.ram, 0x40_0000, 0x40_0000);
        m.cpus[0].enable_jit();
        (m.cpus[0].el, m.cpus[0].spsel, m.cpus[0].pc) = (1, true, 0x40_0000);
        exercise_32_bit(&mut m, |m, n| Host::Pi4(m).run(n), 0x40_0000);

        let mut m = rk3588::Machine::with_cpus(128 << 20, 1).unwrap();
        place_32_bit(&mut m.soc.ram, 0x40_0000, 0x40_0000);
        m.enable_jit();
        (m.cpus[0].el, m.cpus[0].pc) = (1, 0x40_0000);
        exercise_32_bit(&mut m, |m, n| Host::OrangePi5(m).run(n), 0x40_0000);

        let mut m = tegra210::Machine::with_cpus(128 << 20, 1).unwrap();
        place_32_bit(&mut m.soc.ram, 0, tegra210::RAM_BASE);
        m.enable_jit();
        (m.cpus[0].el, m.cpus[0].pc) = (1, tegra210::RAM_BASE);
        exercise_32_bit(&mut m, |m, n| Host::JetsonNano(m).run(n), tegra210::RAM_BASE);
    }

    #[test]
    fn pi4() {
        let mut m = bcm2711::Machine::new(65 << 20);
        place(&mut m.soc.ram, 0x20_0000, A64);
        m.cpus[0].pc = 0x20_0000;
        m.cpus[0].x[2] = 0x30_0000;
        exercise(&mut m, |m, n| Host::Pi4(m).run(n), 0, A64, 0x20_0000, 0x30_0000, 1);
    }

    #[test]
    fn orangepi5() {
        let mut m = rk3588::Machine::with_cpus(128 << 20, 2).unwrap();
        m.enable_jit();
        place(&mut m.soc.ram, 0x20_0000, A64);
        m.cpus[0].el = 1;
        m.cpus[0].pc = 0x20_0000;
        m.cpus[0].x[2] = 0x30_0000;
        exercise(&mut m, |m, n| Host::OrangePi5(m).run(n), 0, A64, 0x20_0000, 0x30_0000, 1);
    }

    /// RAM starts at 2 GiB (an offset in it is not its address) and reaches past 4 GiB.
    #[test]
    fn jetson_nano() {
        for at in [0x8020_0000u64, 0x1_4020_0000] {
            let mut m = tegra210::Machine::with_cpus(4 << 30, 2).unwrap();
            m.enable_jit();
            place(&mut m.soc.ram, (at - tegra210::RAM_BASE) as usize, A64);
            m.cpus[0].el = 1;
            m.cpus[0].pc = at;
            m.cpus[0].x[2] = at + 0x10_0000;
            exercise(&mut m, |m, n| Host::JetsonNano(m).run(n), 0, A64, at, at + 0x10_0000, 1);
            // device registers are not read, so looking changes nothing
            assert!(Target::read(&mut m, 0, tegra210::UARTA, 4).is_none());
            assert!(m.soc.stub_pages.is_empty());
            // what a debugger set is not lost, nor is --no-jit, and the JIT comes back
            assert!(!m.cpus[0].jit_off);
            m.set_jit(false);
            m.breakpoint(at, true);
            m.breakpoint(at, false);
            assert!(m.cpus.iter().all(|c| c.jit_off));
            // a secondary core that starts keeps the watch ranges
            m.watchpoint(at + 0x10_0000, 4, 1, true);
            m.cpus[0].x[0] = 0xc400_0003; // PSCI CPU_ON
            m.cpus[0].x[1] = tegra210::core_mpidr(1) & 0xff;
            m.cpus[0].x[2] = at;
            m.cpus[0].x[3] = 0;
            let smc = 0xd400_0003u32.to_le_bytes();
            assert!(Target::write(&mut m, 0, at + 0x1000, &smc));
            m.cpus[0].pc = at + 0x1000;
            assert_eq!(m.step(0), None);
            assert_eq!(m.state[1], tegra210::CoreState::Run);
            assert_eq!(m.cpus[1].watch, [(at + 0x10_0000, 4, 1)]);
        }
    }

    #[test]
    fn orangepi_one() {
        let mut m = allwinner_h3::Machine::with_ram_size(128 << 20);
        let base = m.soc.ram.base;
        place(&mut m.soc.ram.mem, 0x20_0000, A32);
        m.cpus[0].r[15] = base as u32 + 0x20_0000;
        m.cpus[0].r[2] = base as u32 + 0x30_0000;
        exercise(&mut m, |m, n| Host::OrangePiOne(m).run(n), 0, A32, base + 0x20_0000, base + 0x30_0000, 1);
    }

    #[test]
    fn tinkerboard() {
        let mut m = rockchip_rk3288::Machine::with_ram_size(128 << 20);
        let base = m.soc.ram.base;
        place(&mut m.soc.ram.mem, 0x20_0000, A32);
        m.cpus[0].r[15] = base as u32 + 0x20_0000;
        m.cpus[0].r[2] = base as u32 + 0x30_0000;
        exercise(&mut m, |m, n| Host::TinkerBoard(m).run(n), 0, A32, base + 0x20_0000, base + 0x30_0000, 1);
    }

    #[test]
    fn orangepi_rv() {
        let mut m = starfive_jh7110::Machine::new(64 << 20);
        m.set_jit(true);
        m.load(0x4020_0000, &RV.iter().flat_map(|w| w.to_le_bytes()).collect::<Vec<_>>()).unwrap();
        m.state[1] = starfive_jh7110::HartState::Run;
        m.harts[1].pc = 0x4020_0000;
        m.harts[1].x[12] = 0x4030_0000;
        assert_eq!(m.stopped_core(), 1);
        exercise(&mut m, |m, n| Host::OrangePiRv(m).run(n), 1, RV, 0x4020_0000, 0x4030_0000, 11);
    }

    #[test]
    fn esp32s31() {
        use esp_soc::SocBus as _;
        let mut m = ::esp32s31::machine([2, 0, 0, 0, 0, 1], 16 << 20);
        let sram = ::esp32s31::bus::SRAM_BASE as u64;
        for (k, w) in RV.iter().enumerate() { assert!(Target::write(&mut m, 0, sram + 0x1000 + 4 * k as u64, &w.to_le_bytes())); }
        m.cores[0].pc = sram as u32 + 0x1000;
        m.cores[0].x[12] = sram as u32 + 0x2000;
        let _ = m.bus.cycles();
        exercise(&mut m, |m, n| Host::Esp32S31(m).run(n), 0, RV, sram + 0x1000, sram + 0x2000, 11);
        // a chip reset (done in place) keeps what is set
        m.breakpoint(0x1000, true);
        m.watchpoint(sram + 0x2000, 4, 1, true);
        m.reboot();
        for c in &mut m.cores { assert_eq!(c.watch.len(), 1); }
        m.cores[0].pc = sram as u32 + 0x1000;
        m.cores[0].x[11] = 0;
        m.cores[0].x[12] = sram as u32 + 0x2000;
        for (k, w) in RV.iter().enumerate() { assert!(Target::write(&mut m, 0, sram + 0x1000 + 4 * k as u64, &w.to_le_bytes())); }
        assert_eq!(Host::Esp32S31(&mut m).run(10_000), Stop::Watchpoint(0, sram + 0x2000, 1));
        // ROM and flash are not written
        assert!(!Target::write(&mut m, 0, ::esp32s31::bus::ROM_BASE as u64, b"x"));
    }

    /// Multi-register, byte and VFP stores are watched too, and only where they store.
    #[test]
    fn a32_store_forms() {
        // stmdb r2!, {r0, r1, r3}; strb r1, [r2]; vstr d0, [r2, #8]; strd r0, [r2, #16]
        for (k, (insn, at, hit)) in [(0xe922_000bu32, 0x2f_fff4u32, true), (0xe922_000b, 0x2f_fff0, false), (0xe5c2_1000, 0x30_0000, true), (0xe5c2_1000, 0x30_0001, false), (0xed82_0b02, 0x30_000c, true), (0xed82_0b02, 0x30_0010, false), (0xe1c2_01f0, 0x30_0014, true)].into_iter().enumerate() {
            let mut m = allwinner_h3::Machine::with_ram_size(128 << 20);
            let base = m.soc.ram.base as u32;
            place(&mut m.soc.ram.mem, 0x20_0000, [insn, 0xe320_f000, 0xe320_f000, 0xe320_f000]);
            m.cpus[0].r[15] = base + 0x20_0000;
            m.cpus[0].r[2] = base + 0x30_0000;
            m.cpus[0].vfp.fpexc = 1 << 30;
            m.cpus[0].cpacr = 0xf << 20;
            m.watchpoint((base + at) as u64, 1, 1, true);
            let stop = Target::step(&mut m, 0);
            assert_eq!(stop, hit.then_some(Stop::Watchpoint(0, (base + at) as u64, 1)), "case {k}");
            assert_eq!(pc(&m, 0), base as u64 + 0x20_0004, "case {k}");
        }
    }

    /// Stepping one core leaves the others where and as they are, and they go on afterwards. Both
    /// ways: the core that comes first in a round would get the step anyway.
    #[track_caller]
    fn others_stay<M: Target>(m: &mut M, run: fn(&mut M, u64) -> Stop, counter: usize, now: fn(&M) -> u64) {
        for (me, other) in [(0, 1), (1, 0)] {
            for _ in 0..4 { assert_eq!(run(m, 200_000), Stop::Budget); }
            let (mine, before, at) = (reg(m, me, counter), reg(m, other, counter), pc(m, other));
            assert!(mine > 10 && before > 10, "both cores run");
            let (state, t) = (m.describe(other), now(m));
            for _ in 0..4000 { assert_eq!(m.step(me), None); }
            assert_eq!((m.describe(other), reg(m, other, counter), pc(m, other)), (state, before, at), "stepping core {me}");
            // the stepped core did run (four instructions a round), one nanosecond each
            assert_eq!(reg(m, me, counter), mine + 1000, "core {me}");
            let dt = now(m) - t;
            assert!((3800..=4200).contains(&dt), "4000 steps took {dt} ns");
            // and the other goes on from where it was
            for _ in 0..4 { assert_eq!(run(m, 200_000), Stop::Budget); }
            assert!(reg(m, other, counter) > before + 10);
            assert!(pc(m, other) & !0xf == at & !0xf);
        }
    }

    /// A stepped PSCI call sees the other cores as they are.
    #[test]
    fn a_stepped_psci_call_sees_the_others_on() {
        let mut m = rk3588::Machine::with_cpus(128 << 20, 2).unwrap();
        place(&mut m.soc.ram, 0x20_0000, A64);
        m.soc.ram[0x21_0000..0x21_0004].copy_from_slice(&0xd400_0003u32.to_le_bytes()); // smc
        for k in 0..2 {
            m.state[k] = rk3588::CoreState::Run;
            (m.cpus[k].el, m.cpus[k].pc, m.cpus[k].x[2]) = (1, 0x20_0000, 0x30_0000 + 64 * k as u64);
        }
        for _ in 0..4 { let _ = m.run(200_000); }
        let count = m.cpus[1].x[1];
        // AFFINITY_INFO: on; CPU_ON: already on, and nothing restarted
        for (call, want) in [(0x8400_0004u64, 0u64), (0xc400_0003, -4i64 as u64)] {
            (m.cpus[0].pc, m.cpus[0].x[0], m.cpus[0].x[1]) = (0x21_0000, call, 0x100);
            assert_eq!(m.debug_step(0), None);
            assert_eq!((m.cpus[0].x[0], m.cpus[1].x[1]), (want, count), "{call:x}");
        }
    }

    /// A stepped SEV wakes a core waiting for it.
    #[test]
    fn a_stepped_sev_is_heard() {
        let mut m = allwinner_h3::Machine::with_ram_size(128 << 20);
        let base = m.soc.ram.base as u32;
        place(&mut m.soc.ram.mem, 0x20_0000, [0xe320_f004, 0xe320_f000, 0xe320_f000, 0xeaff_fffb]); // sev; nop; nop; b
        place(&mut m.soc.ram.mem, 0x21_0000, [0xe320_f002, 0xe281_1001, 0xe320_f000, 0xeaff_fffe]); // wfe; add r1, r1, #1; nop; b .
        m.state[0] = allwinner_h3::CoreState::Off;
        m.cpus[0].r[15] = base + 0x20_0000;
        // the first WFE may use up an event that is already there: until one waits
        for _ in 0..2 {
            m.state[1] = allwinner_h3::CoreState::Run;
            (m.cpus[1].r[15], m.cpus[1].r[1]) = (base + 0x21_0000, 0);
            let _ = m.run(10_000);
        }
        assert_eq!((m.state[1], m.cpus[1].r[1]), (allwinner_h3::CoreState::Wait, 0));
        m.state[0] = allwinner_h3::CoreState::Run;
        assert_eq!(m.debug_step(0), None);
        for _ in 0..4 { let _ = m.run(10_000); }
        assert_eq!(m.cpus[1].r[1], 1, "the waiting core went on");
    }

    #[test]
    fn stepping_leaves_the_other_cores_alone_orangepi5() {
        let mut m = rk3588::Machine::with_cpus(128 << 20, 2).unwrap();
        place(&mut m.soc.ram, 0x20_0000, A64);
        for k in 0..2 {
            m.state[k] = rk3588::CoreState::Run;
            (m.cpus[k].el, m.cpus[k].pc, m.cpus[k].x[2]) = (1, 0x20_0000, 0x30_0000 + 64 * k as u64);
        }
        others_stay(&mut m, |m, n| Host::OrangePi5(m).run(n), 1, |m| m.now());
    }

    #[test]
    fn stepping_leaves_the_other_cores_alone_jetson_nano() {
        let mut m = tegra210::Machine::with_cpus(128 << 20, 2).unwrap();
        place(&mut m.soc.ram, 0x20_0000, A64);
        for k in 0..2 {
            m.state[k] = tegra210::CoreState::Run;
            (m.cpus[k].el, m.cpus[k].pc, m.cpus[k].x[2]) = (1, tegra210::RAM_BASE + 0x20_0000, tegra210::RAM_BASE + 0x30_0000 + 64 * k as u64);
        }
        others_stay(&mut m, |m, n| Host::JetsonNano(m).run(n), 1, |m| m.now());
    }

    #[test]
    fn stepping_leaves_the_other_cores_alone_orangepi_one() {
        let mut m = allwinner_h3::Machine::with_ram_size(128 << 20);
        let base = m.soc.ram.base as u32;
        place(&mut m.soc.ram.mem, 0x20_0000, A32);
        for k in 0..2 {
            m.state[k] = allwinner_h3::CoreState::Run;
            (m.cpus[k].r[15], m.cpus[k].r[2]) = (base + 0x20_0000, base + 0x30_0000 + 64 * k as u32);
        }
        others_stay(&mut m, |m, n| Host::OrangePiOne(m).run(n), 1, |m| m.now());
    }

    #[test]
    fn stepping_leaves_the_other_cores_alone_tinkerboard() {
        use emu_core::Bus as _;
        // core 1 powered up as Linux does it: the power controller's state decides who is on
        let mut m = rockchip_rk3288::Machine::with_ram_size(128 << 20);
        place(&mut m.soc.ram.mem, 0x20_0000, A32);
        // core 0's loop stores into a device (the UART's scratch register): that re-evaluates the
        // interrupts, and with them who is powered
        (m.cpus[0].r[15], m.cpus[0].r[2]) = (0x20_0000, rockchip_rk3288::UART2_BASE as u32 + 0x1c);
        m.soc.write32(rockchip_rk3288::SMP_MAILBOX + 8, 0x20_0000).unwrap();
        m.soc.write32(rockchip_rk3288::SMP_MAILBOX + 4, rockchip_rk3288::BOOT_MAGIC).unwrap();
        m.soc.write32(rockchip_rk3288::PMU_BASE + rockchip_rk3288::cru::PMU_PWRDN_CON, 0b1100).unwrap();
        m.soc.write32(rockchip_rk3288::CRU_BASE + rockchip_rk3288::cru::SOFTRST_CON0, 0x2_0000).unwrap();
        run_some(&mut m);
        assert_eq!(m.state[1], rockchip_rk3288::CoreState::Run);
        m.cpus[1].r[2] = 0x30_0040;
        others_stay(&mut m, |m, n| Host::TinkerBoard(m).run(n), 1, |m| m.now());
    }

    fn run_some(m: &mut rockchip_rk3288::Machine) { let _ = Host::TinkerBoard(m).run(3_000); }

    /// Records what is set on it.
    #[derive(Default)]
    struct Fake(Vec<(u8, u64, u64)>);
    impl Target for Fake {
        fn cores(&self) -> usize { 1 }
        fn layout(&self) -> Layout { rv_layout("riscv:rv32", 4, 4) }
        fn reg(&self, _: usize, _: usize) -> Option<Vec<u8>> { Some(vec![0; 4]) }
        fn set_reg(&mut self, _: usize, _: usize, _: &[u8]) -> bool { true }
        fn read(&mut self, _: usize, _: u64, _: usize) -> Option<Vec<u8>> { None }
        fn write(&mut self, _: usize, _: u64, _: &[u8]) -> bool { false }
        fn breakpoint(&mut self, addr: u64, insert: bool) { self.watchpoint(addr, 0, 0, insert) }
        fn watchpoint(&mut self, addr: u64, len: u64, kind: u8, insert: bool) {
            self.0.retain(|&p| p != (kind, addr, len));
            if insert { self.0.push((kind, addr, len)); }
        }
        fn step(&mut self, _: usize) -> Option<Stop> { None }
        fn describe(&self, _: usize) -> String { String::new() }
    }

    /// A stub with a debugger attached that set a breakpoint and a watchpoint and continued.
    fn continued() -> (Gdb, Fake, TcpStream) {
        let mut g = Gdb::listen(0).unwrap();
        let mut c = TcpStream::connect(g.listener.local_addr().unwrap()).unwrap();
        for body in ["Z0,1000,4", "Z2,2000,8", "Z0,3000,4", "z0,3000,4", "vCont;c"] {
            let sum = body.bytes().fold(0u8, |a, b| a.wrapping_add(b));
            write!(c, "${body}#{sum:02x}").unwrap();
        }
        let mut m = Fake::default();
        while !g.connected() { g.poll(&mut m); }
        assert!(matches!(g.halted(&mut m, 0, 2, false), Resume::Continue));
        assert_eq!(m.0, [(0, 0x1000, 0), (1, 0x2000, 8)]);
        (g, m, c)
    }

    /// Orderly (end of file) and with replies unread (the connection is reset).
    #[test]
    fn debugger_vanishes_while_running() {
        for orderly in [true, false] {
            let (mut g, mut m, mut c) = continued();
            assert!(matches!(g.poll(&mut m), Poll::Running));
            assert_eq!(m.0.len(), 2);
            if orderly {
                c.set_read_timeout(Some(std::time::Duration::from_millis(200))).unwrap();
                let mut b = [0; 4096];
                while c.read(&mut b).is_ok_and(|n| n > 0) {}
            }
            drop(c);
            let end = std::time::Instant::now() + std::time::Duration::from_secs(10);
            while g.connected() && std::time::Instant::now() < end { g.poll(&mut m); }
            assert!(!g.connected() && m.0.is_empty(), "orderly {orderly}: {:?}", m.0);
        }
    }

    #[test]
    fn points_survive_a_reset() {
        let (g, _m, _c) = continued();
        let mut fresh = Fake::default();
        g.rearm(&mut fresh);
        assert_eq!(fresh.0, [(0, 0x1000, 0), (1, 0x2000, 8)]);
    }
}
