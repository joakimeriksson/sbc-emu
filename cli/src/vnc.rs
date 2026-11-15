//! A small VNC (RFB 3.3/3.7/3.8) server for the HDMI framebuffer (`--vnc :N`, as QEMU's
//! `-vnc`): no authentication unless `,password=PW` is given (VNC Authentication, which
//! macOS Screen Sharing insists on), Raw encoding of the rows that changed, any true-colour pixel
//! format the viewer asks for, desktop resizes; keys go to the emulated USB keyboard and the
//! pointer to the USB tablet.
use bcm2711::Machine;
use std::io::{ErrorKind, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::time::{Duration, Instant};

#[derive(PartialEq)]
enum Phase { Version, Security, Auth, Init, Normal }

#[derive(Clone, Copy)]
struct PixelFormat { bpp: u8, big: bool, rmax: u16, gmax: u16, bmax: u16, rs: u8, gs: u8, bs: u8 }

const SERVER_PF: PixelFormat = PixelFormat { bpp: 32, big: false, rmax: 255, gmax: 255, bmax: 255, rs: 16, gs: 8, bs: 0 };

struct Client {
    s: TcpStream,
    rx: Vec<u8>,
    phase: Phase,
    minor: u8,
    pf: PixelFormat,
    resize_ok: bool,
    /// an update request is waiting: (incremental)
    want: Option<bool>,
    /// what the viewer has now (RGBA) and its size
    shown: Vec<u8>,
    size: (u32, u32),
    password: Option<Vec<u8>>,
    challenge: [u8; 16],
}

pub struct Vnc {
    listener: TcpListener,
    client: Option<Client>,
    frame: Vec<u8>,
    last: Instant,
    password: Option<Vec<u8>>,
}

/// A fresh 16-byte challenge (clock and address mixed; it only has to differ per connection).
fn challenge() -> [u8; 16] {
    let t = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_nanos()).unwrap_or(0) as u64;
    let mut x = t ^ (&t as *const u64 as u64).rotate_left(32) ^ 0x9e37_79b9_7f4a_7c15;
    let mut out = [0u8; 16];
    for b in out.chunks_mut(8) {
        x ^= x << 13; x ^= x >> 7; x ^= x << 17;
        b.copy_from_slice(&x.to_le_bytes());
    }
    out
}

/// X11 keysym -> HID usage.
fn keysym_usage(k: u32) -> Option<u8> {
    Some(match k {
        0x20..=0x7e => bcm2711::usbdev::ascii_to_usage(k as u8)?.0,
        0xff0d | 0xff8d => 0x28, 0xff1b => 0x29, 0xff08 => 0x2a, 0xff09 => 0x2b,
        0xff51 => 0x50, 0xff52 => 0x52, 0xff53 => 0x4f, 0xff54 => 0x51,
        0xff50 => 0x4a, 0xff57 => 0x4d, 0xff55 => 0x4b, 0xff56 => 0x4e, 0xff63 => 0x49, 0xffff => 0x4c,
        0xffbe..=0xffc9 => 0x3a + (k - 0xffbe) as u8, // F1..F12
        0xffe1 => 0xe1, 0xffe2 => 0xe5, 0xffe3 => 0xe0, 0xffe4 => 0xe4,
        0xffe9 | 0xffe7 => 0xe2, 0xffea | 0xffe8 => 0xe6, 0xffeb => 0xe3, 0xffec => 0xe7,
        0xffe5 => 0x39, // Caps Lock
        _ => return None,
    })
}

impl Vnc {
    /// `spec`: ":N" (port 5900+N, like QEMU) or a port number, optionally `,password=PW`
    /// (the first 8 bytes count, as in every VNC server); listens on 127.0.0.1.
    pub fn listen(spec: &str) -> std::io::Result<Vnc> {
        let (spec, password) = match spec.split_once(",password=") {
            Some((s, p)) => (s, Some(p.as_bytes().iter().copied().take(8).collect::<Vec<u8>>())),
            None => (spec, None),
        };
        let port = match spec.strip_prefix(':') {
            Some(n) => 5900 + n.parse::<u16>().map_err(|_| std::io::Error::new(ErrorKind::InvalidInput, "--vnc :N"))?,
            None => spec.parse::<u16>().map_err(|_| std::io::Error::new(ErrorKind::InvalidInput, "--vnc :N or PORT"))?,
        };
        let listener = TcpListener::bind(("127.0.0.1", port))?;
        listener.set_nonblocking(true)?;
        eprintln!("[sbc-emu] VNC server on 127.0.0.1:{port} (display :{})", port.wrapping_sub(5900));
        Ok(Vnc { listener, client: None, frame: Vec::new(), last: Instant::now(), password })
    }

    /// Serve the viewer: handshake, input, and at most ~30 updates a second.
    pub fn poll(&mut self, m: &mut Machine) {
        if let Ok((s, _)) = self.listener.accept() {
            let _ = s.set_nonblocking(true);
            let _ = s.set_nodelay(true);
            let mut c = Client { s, rx: Vec::new(), phase: Phase::Version, minor: 8, pf: SERVER_PF, resize_ok: false, want: None, shown: Vec::new(), size: (0, 0),
                                 password: self.password.clone(), challenge: challenge() };
            let _ = c.s.write_all(b"RFB 003.008\n");
            self.client = Some(c);
        }
        let Some(c) = self.client.as_mut() else { return };
        let mut buf = [0u8; 4096];
        loop {
            match c.s.read(&mut buf) {
                Ok(0) => { self.client = None; return; }
                Ok(n) => c.rx.extend_from_slice(&buf[..n]),
                Err(e) if e.kind() == ErrorKind::WouldBlock => break,
                Err(_) => { self.client = None; return; }
            }
        }
        if !c.messages(m) { self.client = None; return; }
        if c.want.is_some() && c.phase == Phase::Normal && self.last.elapsed() >= Duration::from_millis(33) {
            self.last = Instant::now();
            if let Some((w, h)) = m.soc.render(&mut self.frame) {
                if !c.update(&self.frame, w, h) { self.client = None; }
            }
        }
    }
}

impl Client {
    /// Parse what has arrived; false = drop the connection.
    fn messages(&mut self, m: &mut Machine) -> bool {
        loop {
            let need = match self.phase {
                Phase::Version => 12,
                Phase::Security | Phase::Init => 1,
                Phase::Auth => 16,
                Phase::Normal => match self.rx.first() {
                    None => return true,
                    Some(0) => 20,
                    Some(2) => { if self.rx.len() < 4 { return true; } 4 + 4 * u16::from_be_bytes([self.rx[2], self.rx[3]]) as usize }
                    Some(3) => 10,
                    Some(4) => 8,
                    Some(5) => 6,
                    Some(6) => { if self.rx.len() < 8 { return true; } 8 + u32::from_be_bytes(self.rx[4..8].try_into().unwrap()) as usize }
                    Some(_) => return false,
                },
            };
            if self.rx.len() < need { return true; }
            let msg: Vec<u8> = self.rx.drain(..need).collect();
            let ok = match self.phase {
                Phase::Version => {
                    self.minor = if msg.starts_with(b"RFB 003.00") { (msg[10] - b'0').clamp(3, 8) } else { 3 };
                    let ty: u8 = if self.password.is_some() { 2 } else { 1 };
                    if self.minor < 7 {
                        // 3.3: the server picks
                        self.phase = if ty == 2 { Phase::Auth } else { Phase::Init };
                        let mut r = (ty as u32).to_be_bytes().to_vec();
                        if ty == 2 { r.extend_from_slice(&self.challenge); }
                        self.s.write_all(&r).is_ok()
                    } else {
                        self.phase = Phase::Security;
                        self.s.write_all(&[1, ty]).is_ok()
                    }
                }
                Phase::Security => {
                    if self.password.is_some() {
                        // only VNC Authentication is on offer
                        self.phase = Phase::Auth;
                        msg[0] == 2 && self.s.write_all(&self.challenge).is_ok()
                    } else {
                        self.phase = Phase::Init;
                        msg[0] == 1 && (self.minor < 8 || self.s.write_all(&0u32.to_be_bytes()).is_ok())
                    }
                }
                Phase::Auth => {
                    let want = crate::vnc_des::vnc_response(self.password.as_deref().unwrap_or(&[]), &self.challenge);
                    if msg[..] == want[..] {
                        self.phase = Phase::Init;
                        self.s.write_all(&0u32.to_be_bytes()).is_ok()
                    } else {
                        eprintln!("[sbc-emu] VNC: wrong password");
                        let mut r = 1u32.to_be_bytes().to_vec();
                        if self.minor >= 8 { let why = b"wrong password"; r.extend_from_slice(&(why.len() as u32).to_be_bytes()); r.extend_from_slice(why); }
                        let _ = self.s.write_all(&r);
                        false
                    }
                }
                Phase::Init => {
                    self.phase = Phase::Normal;
                    let (w, h) = m.soc.display_size();
                    self.size = (w, h);
                    let mut r = Vec::new();
                    r.extend_from_slice(&(w as u16).to_be_bytes());
                    r.extend_from_slice(&(h as u16).to_be_bytes());
                    r.extend_from_slice(&[32, 24, 0, 1, 0, 255, 0, 255, 0, 255, 16, 8, 0, 0, 0, 0]);
                    let name = b"sbc-emu: Raspberry Pi 4";
                    r.extend_from_slice(&(name.len() as u32).to_be_bytes());
                    r.extend_from_slice(name);
                    self.s.write_all(&r).is_ok()
                }
                Phase::Normal => {
                    match msg[0] {
                        0 => {
                            let p = &msg[4..20];
                            if p[3] != 0 && matches!(p[0], 8 | 16 | 32) {
                                self.pf = PixelFormat { bpp: p[0], big: p[2] != 0, rmax: u16::from_be_bytes([p[4], p[5]]), gmax: u16::from_be_bytes([p[6], p[7]]),
                                                        bmax: u16::from_be_bytes([p[8], p[9]]), rs: p[10], gs: p[11], bs: p[12] };
                                self.shown.clear();
                            }
                        }
                        2 => self.resize_ok = msg[4..].chunks(4).any(|e| i32::from_be_bytes(e.try_into().unwrap()) == -223),
                        3 => self.want = Some(msg[1] != 0 && self.want != Some(false)),
                        4 => if let Some(u) = keysym_usage(u32::from_be_bytes(msg[4..8].try_into().unwrap())) { m.soc.devs.kbd.key(u, msg[1] != 0); },
                        5 => {
                            // the pointer goes to the USB tablet: buttons left/middle/right/wheel
                            let (mask, x, y) = (msg[1], u16::from_be_bytes([msg[2], msg[3]]) as u32, u16::from_be_bytes([msg[4], msg[5]]) as u32);
                            let buttons = (mask & 1) | (mask >> 1 & 2) | (mask << 1 & 4);
                            let wheel = if mask & 8 != 0 { 1 } else if mask & 16 != 0 { -1 } else { 0 };
                            let scale = |v: u32, n: u32| (v.min(n.saturating_sub(1)) * 32767 / n.saturating_sub(1).max(1)) as u16;
                            m.soc.devs.tablet.pointer(buttons, scale(x, self.size.0), scale(y, self.size.1), wheel);
                        }
                        _ => {} // clipboard
                    }
                    true
                }
            };
            if !ok { return false; }
        }
    }

    fn pixel(&self, out: &mut Vec<u8>, r: u8, g: u8, b: u8) {
        let f = &self.pf;
        let v = ((r as u32 * f.rmax as u32 / 255) << f.rs) | ((g as u32 * f.gmax as u32 / 255) << f.gs) | ((b as u32 * f.bmax as u32 / 255) << f.bs);
        match (f.bpp, f.big) {
            (32, false) => out.extend_from_slice(&v.to_le_bytes()),
            (32, true) => out.extend_from_slice(&v.to_be_bytes()),
            (16, false) => out.extend_from_slice(&(v as u16).to_le_bytes()),
            (16, true) => out.extend_from_slice(&(v as u16).to_be_bytes()),
            _ => out.push(v as u8),
        }
    }

    /// Send the rows that changed (all of them for a full request); false = connection lost.
    fn update(&mut self, frame: &[u8], w: u32, h: u32) -> bool {
        let incremental = self.want.unwrap_or(false);
        // FramebufferUpdate: type, padding, rectangle count (filled in below)
        let mut msg = vec![0u8, 0, 0, 0];
        let mut rects = 0u16;
        if (w, h) != self.size && self.resize_ok {
            self.size = (w, h);
            self.shown.clear();
            msg.extend_from_slice(&[0, 0, 0, 0]);
            msg.extend_from_slice(&(w as u16).to_be_bytes());
            msg.extend_from_slice(&(h as u16).to_be_bytes());
            msg.extend_from_slice(&(-223i32).to_be_bytes());
            rects += 1;
        }
        // a viewer that cannot resize sees the part that fits
        let (cw, chh) = (w.min(self.size.0) as usize, h.min(self.size.1) as usize);
        let row = w as usize * 4;
        let same = |y: usize| self.shown.len() == frame.len() && self.shown[y * row..(y + 1) * row] == frame[y * row..(y + 1) * row];
        let (mut y0, mut y1) = (0usize, chh);
        if incremental && self.shown.len() == frame.len() {
            while y0 < y1 && same(y0) { y0 += 1; }
            while y1 > y0 && same(y1 - 1) { y1 -= 1; }
            if y0 == y1 && rects == 0 { return true; } // nothing new: answer when something changes
        }
        if y1 > y0 {
            msg.extend_from_slice(&[0, 0]);
            msg.extend_from_slice(&(y0 as u16).to_be_bytes());
            msg.extend_from_slice(&(cw as u16).to_be_bytes());
            msg.extend_from_slice(&((y1 - y0) as u16).to_be_bytes());
            msg.extend_from_slice(&0i32.to_be_bytes());
            for y in y0..y1 {
                for px in frame[y * row..y * row + cw * 4].chunks(4) { self.pixel(&mut msg, px[0], px[1], px[2]); }
            }
            rects += 1;
        }
        msg[2..4].copy_from_slice(&rects.to_be_bytes());
        self.shown.clear();
        self.shown.extend_from_slice(frame);
        self.want = None;
        let _ = self.s.set_nonblocking(false);
        let ok = self.s.write_all(&msg).is_ok();
        let _ = self.s.set_nonblocking(true);
        ok
    }
}
