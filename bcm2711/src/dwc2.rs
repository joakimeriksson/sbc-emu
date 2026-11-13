//! The BCM2711's DesignWare USB 2.0 OTG controller (the USB-C port) in host mode, with a USB
//! HID boot keyboard attached to its root port.
//!
//! On a real Pi 4 this port is host-capable with `dtoverlay=dwc2,dr_mode=host`; the device
//! tree describes exactly that, and Linux's `dwc2` driver runs it. The model implements the
//! host side of the core in buffer-DMA mode: a channel transfer runs to completion (or NAKs)
//! the moment the driver enables the channel, moving data through the channel's DMA address,
//! and reports through HCINT/HAINT/GINTSTS as the core does. Frames advance every 1 ms
//! (full-speed root port) and raise SOF, which drives the driver's periodic scheduler.
//!
//! Identity registers carry the values a Pi reports (core 2.80a, 8 host channels, internal
//! DMA, 19-bit transfer size), so the driver configures itself as on hardware.
use std::collections::VecDeque;

const GOTGCTL: usize = 0x000;
const GAHBCFG: usize = 0x008;
const GUSBCFG: usize = 0x00c;
const GRSTCTL: usize = 0x010;
const GINTSTS: usize = 0x014;
const GINTMSK: usize = 0x018;
const GSNPSID: usize = 0x040;
const GHWCFG1: usize = 0x044;
const GHWCFG2: usize = 0x048;
const GHWCFG3: usize = 0x04c;
const GHWCFG4: usize = 0x050;
const HFNUM: usize = 0x408;
const HAINT: usize = 0x414;
const HAINTMSK: usize = 0x418;
const HPRT0: usize = 0x440;
const HC_BASE: usize = 0x500;
const NCHAN: usize = 8;

// GINTSTS
const GINT_CURMOD_HOST: u32 = 1 << 0;
const GINT_SOF: u32 = 1 << 3;
const GINT_PRTINT: u32 = 1 << 24;
const GINT_HCHINT: u32 = 1 << 25;
// HPRT0
const PRT_CONNSTS: u32 = 1 << 0;
const PRT_CONNDET: u32 = 1 << 1;
const PRT_ENA: u32 = 1 << 2;
const PRT_ENACHG: u32 = 1 << 3;
const PRT_OVRCURRCHG: u32 = 1 << 5;
const PRT_RST: u32 = 1 << 8;
const PRT_PWR: u32 = 1 << 12;
const PRT_SPEED_FS: u32 = 1 << 17;
// HCINT
const HC_XFERCOMPL: u32 = 1 << 0;
const HC_CHHLTD: u32 = 1 << 1;
const HC_STALL: u32 = 1 << 3;
const HC_NAK: u32 = 1 << 4;
const HC_ACK: u32 = 1 << 5;
const HC_XACTERR: u32 = 1 << 7;

const PID_DATA0: u32 = 0;
const PID_DATA1: u32 = 2;
const PID_SETUP: u32 = 3;

#[derive(Default, Clone, Copy)]
struct Chan { hcchar: u32, hcsplt: u32, hcint: u32, hcintmsk: u32, hctsiz: u32, hcdma: u32 }

pub struct Dwc2 {
    g: [u32; 0x60],
    hprt: u32,
    hcfg: u32,
    hfir: u32,
    haintmsk: u32,
    gintsts: u32,
    chan: [Chan; NCHAN],
    pub kbd: Keyboard,
    /// frame number (1 ms frames) and when the next one starts (ns)
    frame: u32,
    next_frame_ns: u64,
    pub transfers: u64,
    pub trace: bool,
}

impl Default for Dwc2 {
    fn default() -> Self { Self::new() }
}

impl Dwc2 {
    pub fn new() -> Self {
        let mut d = Dwc2 { g: [0; 0x60], hprt: 0, hcfg: 0, hfir: 48000, haintmsk: 0, gintsts: GINT_CURMOD_HOST, chan: [Chan::default(); NCHAN],
                           kbd: Keyboard::default(), frame: 0, next_frame_ns: 0, transfers: 0, trace: false };
        d.reset();
        d
    }

    fn reset(&mut self) {
        self.g = [0; 0x60];
        self.g[GRSTCTL / 4] = 1 << 31; // AHB idle
        self.g[GSNPSID / 4] = 0x4f54_280a;
        self.g[GHWCFG1 / 4] = 0;
        self.g[GHWCFG2 / 4] = 0x228d_dd50;
        self.g[GHWCFG3 / 4] = 0x0ff0_00e8;
        self.g[GHWCFG4 / 4] = 0x1ff0_0020;
        self.g[0x024 / 4] = 0x1000; // GRXFSIZ
        self.g[0x028 / 4] = 0x0100_1000; // GNPTXFSIZ
        self.g[0x100 / 4] = 0x0100_1100; // HPTXFSIZ: periodic TX FIFO, 256 words
        self.gintsts = GINT_CURMOD_HOST;
        self.hprt = 0;
        self.chan = [Chan::default(); NCHAN];
        self.kbd.reset();
    }

    fn haint(&self) -> u32 {
        (0..NCHAN).fold(0, |a, n| a | (((self.chan[n].hcint & self.chan[n].hcintmsk) != 0) as u32) << n)
    }

    fn gintsts(&self) -> u32 {
        let mut s = self.gintsts | GINT_CURMOD_HOST;
        if self.hprt & (PRT_CONNDET | PRT_ENACHG | PRT_OVRCURRCHG) != 0 { s |= GINT_PRTINT; }
        if self.haint() & self.haintmsk != 0 { s |= GINT_HCHINT; }
        s
    }

    pub fn irq(&self) -> bool {
        self.g[GAHBCFG / 4] & 1 != 0 && self.gintsts() & self.g[GINTMSK / 4] != 0
    }

    /// Advance frames up to `now_ns`; returns true if interrupt state may have changed.
    pub fn tick(&mut self, now_ns: u64) -> bool {
        if self.hprt & PRT_ENA == 0 { self.next_frame_ns = now_ns + 1_000_000; return false; }
        if now_ns < self.next_frame_ns { return false; }
        let frames = (now_ns - self.next_frame_ns) / 1_000_000 + 1;
        self.frame = (self.frame + frames as u32) & 0x3fff;
        self.next_frame_ns += frames * 1_000_000;
        self.gintsts |= GINT_SOF;
        true
    }

    /// When the next frame starts, if SOF interrupts are wanted.
    pub fn next_event(&self) -> Option<u64> {
        (self.hprt & PRT_ENA != 0 && self.g[GINTMSK / 4] & GINT_SOF != 0).then_some(self.next_frame_ns)
    }

    pub fn read(&mut self, off: u64) -> u32 {
        let o = off as usize & 0xffff;
        match o {
            GINTSTS => self.gintsts(),
            HFNUM => self.frame | (0x3a98 << 16),
            HAINT => self.haint(),
            HAINTMSK => self.haintmsk,
            HPRT0 => self.hprt,
            0x400 => self.hcfg,
            0x404 => self.hfir,
            0x410 => 0x0008_0100, // HPTXSTS: queue and FIFO space available
            HC_BASE..=0x5ff => {
                let (n, r) = ((o - HC_BASE) / 0x20, (o - HC_BASE) % 0x20);
                let c = &self.chan[n.min(NCHAN - 1)];
                if n >= NCHAN { return 0; }
                match r { 0x00 => c.hcchar, 0x04 => c.hcsplt, 0x08 => c.hcint, 0x0c => c.hcintmsk, 0x10 => c.hctsiz, 0x14 => c.hcdma, _ => 0 }
            }
            0x02c => 0x0008_0100, // GNPTXSTS
            o if o < 0x180 => self.g[o / 4],
            _ => 0,
        }
    }

    pub fn write(&mut self, off: u64, v: u32, ram: &mut [u8]) {
        let o = off as usize & 0xffff;
        match o {
            GRSTCTL => {
                if v & 1 != 0 { self.reset(); return; } // core soft reset completes at once
                self.g[GRSTCTL / 4] = (1 << 31) | (v & !(1 | (1 << 4) | (1 << 5) | (1 << 30)));
            }
            GINTSTS => self.gintsts &= !(v & !(GINT_CURMOD_HOST | GINT_PRTINT | GINT_HCHINT)),
            HAINTMSK => self.haintmsk = v & 0xff,
            HPRT0 => self.write_hprt(v),
            0x400 => self.hcfg = v,
            0x404 => self.hfir = v,
            HC_BASE..=0x5ff => {
                let (n, r) = ((o - HC_BASE) / 0x20, (o - HC_BASE) % 0x20);
                if n >= NCHAN { return; }
                match r {
                    0x00 => {
                        self.chan[n].hcchar = v;
                        if v & (1 << 31) != 0 {
                            if v & (1 << 30) != 0 {
                                // disable request on an active channel: halt it
                                self.chan[n].hcchar &= !((1 << 31) | (1 << 30));
                                self.chan[n].hcint |= HC_CHHLTD;
                            } else {
                                self.start(n, ram);
                            }
                        }
                    }
                    0x04 => self.chan[n].hcsplt = v,
                    0x08 => self.chan[n].hcint &= !v,
                    0x0c => self.chan[n].hcintmsk = v,
                    0x10 => self.chan[n].hctsiz = v,
                    0x14 => self.chan[n].hcdma = v,
                    _ => {}
                }
            }
            o if o < 0x180 => self.g[o / 4] = v,
            _ => {}
        }
        let _ = GOTGCTL;
        let _ = GUSBCFG;
    }

    fn write_hprt(&mut self, v: u32) {
        // write-1-to-clear change bits; writing ENA disables the port
        self.hprt &= !(v & (PRT_CONNDET | PRT_ENACHG | PRT_OVRCURRCHG));
        if v & PRT_ENA != 0 && self.hprt & PRT_ENA != 0 { self.hprt &= !PRT_ENA; self.hprt |= PRT_ENACHG; }
        let was_reset = self.hprt & PRT_RST != 0;
        let was_powered = self.hprt & PRT_PWR != 0;
        self.hprt = (self.hprt & !(PRT_RST | PRT_PWR)) | (v & (PRT_RST | PRT_PWR));
        if !was_powered && self.hprt & PRT_PWR != 0 {
            // the keyboard is plugged in: connect is detected once the port has power
            self.hprt |= PRT_CONNSTS | PRT_CONNDET;
        }
        if was_reset && self.hprt & PRT_RST == 0 && self.hprt & PRT_CONNSTS != 0 {
            // end of port reset: enabled, full speed, device back to its default state
            self.hprt = (self.hprt & !(3 << 17)) | PRT_ENA | PRT_ENACHG | PRT_SPEED_FS;
            self.kbd.reset();
        }
        if self.trace { eprintln!("[usb] HPRT0 <- {v:#x} = {:#x}", self.hprt); }
    }

    /// Run a channel's transfer now (buffer DMA).
    fn start(&mut self, n: usize, ram: &mut [u8]) {
        self.transfers += 1;
        let c = self.chan[n];
        let addr = (c.hcchar >> 22) & 0x7f;
        let ep = (c.hcchar >> 11) & 0xf;
        let dir_in = c.hcchar & (1 << 15) != 0;
        let mps = (c.hcchar & 0x7ff).max(1);
        let size = (c.hctsiz & 0x7ffff) as usize;
        let pid = (c.hctsiz >> 29) & 3;
        let dma = c.hcdma as usize;
        let done = |s: &mut Self, bits: u32, moved: usize, pid_next: u32| {
            let ch = &mut s.chan[n];
            ch.hcchar &= !((1 << 31) | (1 << 30));
            ch.hctsiz = (ch.hctsiz & !(0x7ffff | (0x3ff << 19) | (3 << 29))) | ((size - moved.min(size)) as u32) | (pid_next << 29);
            ch.hcint |= bits | HC_CHHLTD;
        };
        let present = self.hprt & PRT_ENA != 0;
        if !present || addr != self.kbd.address as u32 {
            if self.trace { eprintln!("[usb] ch{n} addr {addr} ep{ep}: no device"); }
            return done(self, HC_XACTERR, 0, pid);
        }
        let toggle = |p: u32, packets: usize| if packets % 2 == 0 { p } else if p == PID_DATA0 { PID_DATA1 } else { PID_DATA0 };
        if ep == 0 {
            if pid == PID_SETUP {
                let mut setup = [0u8; 8];
                if let Some(s) = ram.get(dma..dma + 8) { setup.copy_from_slice(s); }
                let ok = self.kbd.setup(setup);
                if self.trace { eprintln!("[usb] ch{n} SETUP {setup:02x?} -> {}", if ok { "ok" } else { "STALL" }); }
                return if ok { done(self, HC_XFERCOMPL | HC_ACK, 8, PID_DATA1) } else { done(self, HC_STALL, 0, pid) };
            }
            if dir_in {
                let data = self.kbd.control_in(size);
                if let Some(d) = ram.get_mut(dma..dma + data.len()) { d.copy_from_slice(&data); }
                let packets = data.len().div_ceil(mps as usize).max(1);
                if self.trace { eprintln!("[usb] ch{n} IN ep0 {} bytes", data.len()); }
                return done(self, HC_XFERCOMPL | HC_ACK, data.len(), toggle(pid, packets));
            }
            let data = ram.get(dma..dma + size).map(|d| d.to_vec()).unwrap_or_default();
            self.kbd.control_out(&data);
            let packets = size.div_ceil(mps as usize).max(1);
            return done(self, HC_XFERCOMPL | HC_ACK, size, toggle(pid, packets));
        }
        if ep == 1 && dir_in {
            return match self.kbd.next_report() {
                Some(r) => {
                    let len = r.len().min(size);
                    if let Some(d) = ram.get_mut(dma..dma + len) { d.copy_from_slice(&r[..len]); }
                    done(self, HC_XFERCOMPL | HC_ACK, len, toggle(pid, 1))
                }
                None => done(self, HC_NAK, 0, pid),
            };
        }
        done(self, HC_STALL, 0, pid)
    }
}

// ---------------------------------------------------------------------- the keyboard

const DEVICE_DESC: [u8; 18] = [
    18, 1, 0x00, 0x02, 0, 0, 0, 64, // USB 2.0, class per interface, EP0 64 bytes
    0x09, 0x12, 0x01, 0x00, // VID 0x1209 (pid.codes), PID 0x0001 (test)
    0x00, 0x01, 1, 2, 0, 1, // bcdDevice 1.00, strings: manufacturer 1, product 2, no serial
];

const REPORT_DESC: [u8; 63] = [
    0x05, 0x01, 0x09, 0x06, 0xa1, 0x01, 0x05, 0x07, 0x19, 0xe0, 0x29, 0xe7, 0x15, 0x00, 0x25, 0x01,
    0x75, 0x01, 0x95, 0x08, 0x81, 0x02, 0x95, 0x01, 0x75, 0x08, 0x81, 0x01, 0x95, 0x05, 0x75, 0x01,
    0x05, 0x08, 0x19, 0x01, 0x29, 0x05, 0x91, 0x02, 0x95, 0x01, 0x75, 0x03, 0x91, 0x01, 0x95, 0x06,
    0x75, 0x08, 0x15, 0x00, 0x25, 0x65, 0x05, 0x07, 0x19, 0x00, 0x29, 0x65, 0x81, 0x00, 0xc0,
];

fn config_desc() -> Vec<u8> {
    let mut d = vec![
        9, 2, 34, 0, 1, 1, 0, 0xa0, 50, // config: 1 interface, bus powered, remote wakeup, 100 mA
        9, 4, 0, 0, 1, 3, 1, 1, 0, // interface: HID, boot subclass, keyboard protocol
        9, 0x21, 0x11, 0x01, 0, 1, 0x22, REPORT_DESC.len() as u8, 0, // HID 1.11, report descriptor
        7, 5, 0x81, 3, 8, 0, 10, // endpoint 1 IN, interrupt, 8 bytes, 10 ms
    ];
    d[2] = d.len() as u8;
    d
}

fn string_desc(s: &str) -> Vec<u8> {
    let mut d = vec![0, 3];
    for u in s.encode_utf16() { d.extend_from_slice(&u.to_le_bytes()); }
    d[0] = d.len() as u8;
    d
}

#[derive(Default)]
pub struct Keyboard {
    pub address: u8,
    /// SET_ADDRESS is applied after its status stage
    pending_address: Option<u8>,
    configured: bool,
    pending_in: Vec<u8>,
    /// keys currently held (HID usages) and modifier bits
    held: Vec<u8>,
    modifiers: u8,
    /// queued state changes, each producing one report
    events: VecDeque<(u8, bool)>,
    /// a report waiting to be fetched by the next interrupt IN
    last: Option<[u8; 8]>,
    pub leds: u8,
}

impl Keyboard {
    fn reset(&mut self) {
        self.address = 0;
        self.pending_address = None;
        self.configured = false;
        self.pending_in.clear();
        self.held.clear();
        self.modifiers = 0;
        self.last = None;
    }

    /// Queue a key press or release (HID usage; 0xe0..0xe7 are the modifiers).
    pub fn key(&mut self, usage: u8, down: bool) {
        if self.events.len() < 256 { self.events.push_back((usage, down)); }
    }

    /// Press and release a key with modifier bits (HID left-ctrl 0x01, left-shift 0x02, ...).
    pub fn tap(&mut self, usage: u8, mods: u8) {
        for b in 0..8 { if mods & (1 << b) != 0 { self.key(0xe0 + b, true); } }
        self.key(usage, true);
        self.key(usage, false);
        for b in (0..8).rev() { if mods & (1 << b) != 0 { self.key(0xe0 + b, false); } }
    }

    /// Type a byte of text as press/release pairs (US layout).
    pub fn type_byte(&mut self, b: u8) {
        if (1..=26).contains(&b) && !matches!(b, 8 | 9 | 10 | 13) {
            return self.tap(0x04 + b - 1, 0x01); // Ctrl-letter
        }
        if let Some((usage, shift)) = ascii_to_usage(b) {
            if shift { self.key(0xe1, true); }
            self.key(usage, true);
            self.key(usage, false);
            if shift { self.key(0xe1, false); }
        }
    }

    pub fn queued(&self) -> usize { self.events.len() }

    fn next_report(&mut self) -> Option<[u8; 8]> {
        if !self.configured { return None; }
        let (usage, down) = self.events.pop_front()?;
        if (0xe0..=0xe7).contains(&usage) {
            let bit = 1 << (usage - 0xe0);
            if down { self.modifiers |= bit } else { self.modifiers &= !bit }
        } else if down {
            if !self.held.contains(&usage) && self.held.len() < 6 { self.held.push(usage); }
        } else {
            self.held.retain(|&u| u != usage);
        }
        let mut r = [0u8; 8];
        r[0] = self.modifiers;
        for (k, &u) in self.held.iter().enumerate() { r[2 + k] = u; }
        self.last = Some(r);
        Some(r)
    }

    fn setup(&mut self, s: [u8; 8]) -> bool {
        let (rt, req) = (s[0], s[1]);
        let value = u16::from_le_bytes([s[2], s[3]]);
        let len = u16::from_le_bytes([s[6], s[7]]) as usize;
        self.pending_in.clear();
        let reply = |v: &[u8]| v[..v.len().min(len)].to_vec();
        match (rt, req) {
            (0x80, 6) => {
                let d = match value >> 8 {
                    1 => DEVICE_DESC.to_vec(),
                    2 => config_desc(),
                    3 => match value & 0xff { 0 => vec![4, 3, 0x09, 0x04], 1 => string_desc("sbc-emu"), 2 => string_desc("Virtual USB Keyboard"), _ => return false },
                    6 => return false, // device qualifier: full-speed only device
                    _ => return false,
                };
                self.pending_in = reply(&d);
            }
            (0x81, 6) if value >> 8 == 0x22 => self.pending_in = reply(&REPORT_DESC),
            (0x81, 6) if value >> 8 == 0x21 => self.pending_in = reply(&config_desc()[18..27]),
            (0x00, 5) => self.pending_address = Some((value & 0x7f) as u8),
            (0x00, 9) => self.configured = value == 1,
            (0x80, 8) => self.pending_in = vec![self.configured as u8],
            (0x80, 0) | (0x81, 0) | (0x82, 0) => self.pending_in = reply(&[0, 0]),
            (0x00, 1) | (0x00, 3) | (0x01, 11) | (0x02, 1) => {}
            (0x21, 0x0a) | (0x21, 0x0b) | (0x21, 0x09) => {} // SET_IDLE, SET_PROTOCOL, SET_REPORT
            (0xa1, 0x01) => self.pending_in = reply(&self.last.unwrap_or([0; 8])),
            (0xa1, 0x02) => self.pending_in = vec![0],
            (0xa1, 0x03) => self.pending_in = vec![0],
            _ => return false,
        }
        true
    }

    fn control_in(&mut self, max: usize) -> Vec<u8> {
        let n = self.pending_in.len().min(max);
        let out: Vec<u8> = self.pending_in.drain(..n).collect();
        // the zero-length IN status stage of SET_ADDRESS completes it
        if out.is_empty() { if let Some(a) = self.pending_address.take() { self.address = a; } }
        out
    }

    fn control_out(&mut self, data: &[u8]) {
        if let Some(&b) = data.first() { self.leds = b; } // SET_REPORT data: LED state
    }
}

/// US layout: ASCII byte -> (HID usage, shift).
pub fn ascii_to_usage(b: u8) -> Option<(u8, bool)> {
    Some(match b {
        b'a'..=b'z' => (0x04 + b - b'a', false),
        b'A'..=b'Z' => (0x04 + b - b'A', true),
        b'1'..=b'9' => (0x1e + b - b'1', false),
        b'0' => (0x27, false),
        b'\r' | b'\n' => (0x28, false),
        0x1b => (0x29, false),
        0x7f | 0x08 => (0x2a, false),
        b'\t' => (0x2b, false),
        b' ' => (0x2c, false),
        b'-' => (0x2d, false), b'_' => (0x2d, true),
        b'=' => (0x2e, false), b'+' => (0x2e, true),
        b'[' => (0x2f, false), b'{' => (0x2f, true),
        b']' => (0x30, false), b'}' => (0x30, true),
        b'\\' => (0x31, false), b'|' => (0x31, true),
        b';' => (0x33, false), b':' => (0x33, true),
        b'\'' => (0x34, false), b'"' => (0x34, true),
        b'`' => (0x35, false), b'~' => (0x35, true),
        b',' => (0x36, false), b'<' => (0x36, true),
        b'.' => (0x37, false), b'>' => (0x37, true),
        b'/' => (0x38, false), b'?' => (0x38, true),
        b'!' => (0x1e, true), b'@' => (0x1f, true), b'#' => (0x20, true), b'$' => (0x21, true),
        b'%' => (0x22, true), b'^' => (0x23, true), b'&' => (0x24, true), b'*' => (0x25, true),
        b'(' => (0x26, true), b')' => (0x27, true),
        _ => return None,
    })
}

impl Dwc2 {
    pub fn save_state(&self, w: &mut emu_core::snap::W) {
        w.tag("dwc2-v1");
        for v in self.g { w.u32(v); }
        for v in [self.hprt, self.hcfg, self.hfir, self.haintmsk, self.gintsts, self.frame] { w.u32(v); }
        for c in &self.chan { for v in [c.hcchar, c.hcsplt, c.hcint, c.hcintmsk, c.hctsiz, c.hcdma] { w.u32(v); } }
        w.u64(self.next_frame_ns);
        let k = &self.kbd;
        w.u8(k.address); w.bool(k.pending_address.is_some()); w.u8(k.pending_address.unwrap_or(0));
        w.bool(k.configured); w.bytes(&k.pending_in); w.bytes(&k.held); w.u8(k.modifiers); w.u8(k.leds);
        w.bool(k.last.is_some()); w.bytes(&k.last.unwrap_or([0; 8]));
    }
    pub fn load_state(&mut self, r: &mut emu_core::snap::R) -> Result<(), emu_core::snap::SnapError> {
        r.tag("dwc2-v1")?;
        for v in self.g.iter_mut() { *v = r.u32()?; }
        for v in [&mut self.hprt, &mut self.hcfg, &mut self.hfir, &mut self.haintmsk, &mut self.gintsts, &mut self.frame] { *v = r.u32()?; }
        for c in self.chan.iter_mut() { for v in [&mut c.hcchar, &mut c.hcsplt, &mut c.hcint, &mut c.hcintmsk, &mut c.hctsiz, &mut c.hcdma] { *v = r.u32()?; } }
        self.next_frame_ns = r.u64()?;
        let k = &mut self.kbd;
        k.address = r.u8()?; let pa = r.bool()?; let a = r.u8()?; k.pending_address = pa.then_some(a);
        k.configured = r.bool()?; k.pending_in = r.bytes()?.to_vec(); k.held = r.bytes()?.to_vec(); k.modifiers = r.u8()?; k.leds = r.u8()?;
        let has = r.bool()?; let l = r.bytes()?; k.last = if has { l.try_into().ok() } else { None };
        k.events.clear();
        Ok(())
    }
}
