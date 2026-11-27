//! USB devices, shared by the host controllers of every board (the Pi 4's DWC2 OTG port
//! and VL805 xHCI behind PCIe, the H3's OHCI): a HID boot keyboard, a HID tablet (absolute pointer, what a VNC viewer
//! needs, like QEMU's `usb-tablet`) and a bulk-only mass-storage stick with a SCSI disk.
//!
//! A controller talks to a device in transactions, not packets: a SETUP, then the data stage
//! (`control_in`/`control_out`), and whole transfers on the other endpoints (`data_in`, where
//! `None` is a NAK: nothing to send yet).
use crate::disk::Disk;
use std::collections::VecDeque;

/// The standard part of a device: address, configuration, the reply being read.
#[derive(Default, Clone)]
pub struct Std {
    pub address: u8,
    /// SET_ADDRESS is applied after its status stage
    pub pending_address: Option<u8>,
    pub configured: bool,
    pub pending_in: Vec<u8>,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Speed { Full, High }

pub trait UsbDevice {
    fn std(&mut self) -> &mut Std;
    fn device_desc(&self) -> Vec<u8>;
    fn config_desc(&self) -> Vec<u8>;
    fn string(&self, i: u8) -> Option<&'static str>;
    fn speed(&self) -> Speed { Speed::Full }
    /// a class or interface request (descriptor reads to the interface included): the reply,
    /// or None to STALL
    fn class_setup(&mut self, _s: [u8; 8]) -> Option<Vec<u8>> { None }
    /// the OUT data stage of a class request
    fn class_out(&mut self, _data: &[u8]) {}
    /// a transfer from IN endpoint `ep` of at most `max` bytes; None = NAK
    fn data_in(&mut self, ep: u8, max: usize) -> Option<Vec<u8>>;
    /// a transfer to OUT endpoint `ep`
    fn data_out(&mut self, _ep: u8, _data: &[u8]) {}
    /// has something to send on an IN endpoint (an interrupt endpoint to poll)
    fn has_input(&self) -> bool { false }
    /// bus reset: back to the default state, address 0
    fn bus_reset(&mut self) { *self.std() = Std::default(); }

    fn address(&mut self) -> u8 { self.std().address }

    /// A SETUP packet; false = STALL.
    fn setup(&mut self, s: [u8; 8]) -> bool {
        let (rt, req) = (s[0], s[1]);
        let value = u16::from_le_bytes([s[2], s[3]]);
        let len = u16::from_le_bytes([s[6], s[7]]) as usize;
        self.std().pending_in.clear();
        let reply: Vec<u8> = match (rt, req) {
            (0x80, 6) => {
                let d = match value >> 8 {
                    1 => self.device_desc(),
                    2 => self.config_desc(),
                    3 => match value & 0xff { 0 => vec![4, 3, 0x09, 0x04], i => match self.string(i as u8) { Some(s) => string_desc(s), None => return false } },
                    _ => return false, // no device qualifier: a full-speed device, or HS-only
                };
                d
            }
            (0x00, 5) => { self.std().pending_address = Some((value & 0x7f) as u8); Vec::new() }
            (0x00, 9) => { self.std().configured = value == 1; Vec::new() }
            (0x80, 8) => { let c = self.std().configured; vec![c as u8] }
            (0x80, 0) | (0x81, 0) | (0x82, 0) => vec![0, 0],
            (0x00, 1) | (0x00, 3) | (0x01, 11) | (0x02, 1) | (0x02, 3) => Vec::new(),
            _ => match self.class_setup(s) { Some(r) => r, None => return false },
        };
        self.std().pending_in = reply[..reply.len().min(len)].to_vec();
        true
    }

    /// IN data stage (and the zero-length IN status stage of an OUT request).
    fn control_in(&mut self, max: usize) -> Vec<u8> {
        let st = self.std();
        let n = st.pending_in.len().min(max);
        let out: Vec<u8> = st.pending_in.drain(..n).collect();
        if out.is_empty() { if let Some(a) = st.pending_address.take() { st.address = a; } }
        out
    }

    fn control_out(&mut self, data: &[u8]) { self.class_out(data); }
}

pub fn string_desc(s: &str) -> Vec<u8> {
    let mut d = vec![0, 3];
    for u in s.encode_utf16() { d.extend_from_slice(&u.to_le_bytes()); }
    d[0] = d.len() as u8;
    d
}

fn device_desc(product: u16, ep0: u8, bcd_usb: u16) -> Vec<u8> {
    let mut d = vec![18, 1, 0, 0, 0, 0, 0, ep0, 0x09, 0x12]; // class per interface; VID 0x1209 (pid.codes)
    d[2..4].copy_from_slice(&bcd_usb.to_le_bytes());
    d.extend_from_slice(&product.to_le_bytes());
    d.extend_from_slice(&[0x00, 0x01, 1, 2, 3, 1]); // bcdDevice 1.00, strings, 1 configuration
    d
}

fn hid_config(protocol: u8, report_len: usize, ep_size: u8, interval: u8) -> Vec<u8> {
    let mut d = vec![
        9, 2, 0, 0, 1, 1, 0, 0xa0, 50, // config: 1 interface, bus powered, remote wakeup, 100 mA
        9, 4, 0, 0, 1, 3, (protocol != 0) as u8, protocol, 0, // interface: HID (boot subclass if a boot protocol)
        9, 0x21, 0x11, 0x01, 0, 1, 0x22, report_len as u8, (report_len >> 8) as u8, // HID 1.11
        7, 5, 0x81, 3, ep_size, 0, interval, // endpoint 1 IN, interrupt
    ];
    d[2] = d.len() as u8;
    d
}

/// HID class requests both HID devices answer.
fn hid_class(s: [u8; 8], report: &[u8], config: &[u8], last: &[u8]) -> Option<Vec<u8>> {
    let value = u16::from_le_bytes([s[2], s[3]]);
    match (s[0], s[1]) {
        (0x81, 6) if value >> 8 == 0x22 => Some(report.to_vec()),
        (0x81, 6) if value >> 8 == 0x21 => Some(config[18..27].to_vec()),
        (0x21, 0x0a) | (0x21, 0x0b) | (0x21, 0x09) => Some(Vec::new()), // SET_IDLE, SET_PROTOCOL, SET_REPORT
        (0xa1, 0x01) => Some(last.to_vec()),
        (0xa1, 0x02) | (0xa1, 0x03) => Some(vec![0]),
        _ => None,
    }
}

// ---------------------------------------------------------------------- the keyboard

const KBD_REPORT: [u8; 63] = [
    0x05, 0x01, 0x09, 0x06, 0xa1, 0x01, 0x05, 0x07, 0x19, 0xe0, 0x29, 0xe7, 0x15, 0x00, 0x25, 0x01,
    0x75, 0x01, 0x95, 0x08, 0x81, 0x02, 0x95, 0x01, 0x75, 0x08, 0x81, 0x01, 0x95, 0x05, 0x75, 0x01,
    0x05, 0x08, 0x19, 0x01, 0x29, 0x05, 0x91, 0x02, 0x95, 0x01, 0x75, 0x03, 0x91, 0x01, 0x95, 0x06,
    0x75, 0x08, 0x15, 0x00, 0x25, 0x65, 0x05, 0x07, 0x19, 0x00, 0x29, 0x65, 0x81, 0x00, 0xc0,
];

#[derive(Default)]
pub struct Keyboard {
    pub st: Std,
    /// keys currently held (HID usages) and modifier bits
    pub held: Vec<u8>,
    pub modifiers: u8,
    /// queued state changes, each producing one report
    pub events: VecDeque<(u8, bool)>,
    /// the last report sent (GET_REPORT)
    pub last: Option<[u8; 8]>,
    pub leds: u8,
}

impl Keyboard {
    /// Queue a key press or release (HID usage; 0xe0..0xe7 are the modifiers).
    pub fn key(&mut self, usage: u8, down: bool) {
        if self.events.len() < 16384 { self.events.push_back((usage, down)); }
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
        if !self.st.configured { return None; }
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
}

impl UsbDevice for Keyboard {
    fn std(&mut self) -> &mut Std { &mut self.st }
    fn device_desc(&self) -> Vec<u8> { let mut d = device_desc(0x0001, 64, 0x0200); d[16] = 0; d } // no serial number
    fn config_desc(&self) -> Vec<u8> { hid_config(1, KBD_REPORT.len(), 8, 10) }
    fn string(&self, i: u8) -> Option<&'static str> { match i { 1 => Some("sbc-emu"), 2 => Some("Virtual USB Keyboard"), _ => None } }
    fn class_setup(&mut self, s: [u8; 8]) -> Option<Vec<u8>> { hid_class(s, &KBD_REPORT, &self.config_desc(), &self.last.unwrap_or([0; 8])) }
    fn class_out(&mut self, data: &[u8]) { if let Some(&b) = data.first() { self.leds = b; } } // SET_REPORT: LEDs
    fn data_in(&mut self, ep: u8, max: usize) -> Option<Vec<u8>> {
        if ep != 1 { return None; }
        self.next_report().map(|r| r[..max.min(8)].to_vec())
    }
    fn has_input(&self) -> bool { self.st.configured && !self.events.is_empty() }
    fn bus_reset(&mut self) {
        self.st = Std::default();
        self.held.clear();
        self.modifiers = 0;
        self.last = None;
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

// ---------------------------------------------------------------------- the tablet

/// Buttons (1 byte), X and Y (0..=32767, 16 bits each), wheel (signed byte).
const TABLET_REPORT: [u8; 74] = [
    0x05, 0x01, 0x09, 0x02, 0xa1, 0x01, 0x09, 0x01, 0xa1, 0x00, // mouse, physical pointer
    0x05, 0x09, 0x19, 0x01, 0x29, 0x03, 0x15, 0x00, 0x25, 0x01, 0x95, 0x03, 0x75, 0x01, 0x81, 0x02, // 3 buttons
    0x95, 0x01, 0x75, 0x05, 0x81, 0x01, // padding
    0x05, 0x01, 0x09, 0x30, 0x09, 0x31, 0x15, 0x00, 0x26, 0xff, 0x7f, 0x35, 0x00, 0x46, 0xff, 0x7f,
    0x75, 0x10, 0x95, 0x02, 0x81, 0x02, // X, Y absolute
    0x05, 0x01, 0x09, 0x38, 0x15, 0x81, 0x25, 0x7f, 0x35, 0x00, 0x45, 0x00, 0x75, 0x08, 0x95, 0x01,
    0x81, 0x06, // wheel, relative
    0xc0, 0xc0,
];

#[derive(Default)]
pub struct Tablet {
    pub st: Std,
    /// queued pointer states: (buttons, x, y, wheel)
    events: VecDeque<(u8, u16, u16, i8)>,
    last: [u8; 6],
    /// where the pointer is and which buttons are down
    pub state: (u8, u16, u16),
}

impl Tablet {
    /// The pointer at (x, y) in 0..=32767 with buttons (bit 0 left, 1 right, 2 middle) and
    /// wheel clicks.
    pub fn pointer(&mut self, buttons: u8, x: u16, y: u16, wheel: i8) {
        self.state = (buttons, x.min(32767), y.min(32767));
        if let Some(l) = self.events.back_mut() {
            // coalesce motion while the buttons stay the same
            if l.0 == buttons && l.3 == 0 && wheel == 0 { l.1 = x; l.2 = y; return; }
        }
        if self.events.len() < 1024 { self.events.push_back((buttons, x.min(32767), y.min(32767), wheel)); }
    }
}

impl UsbDevice for Tablet {
    fn std(&mut self) -> &mut Std { &mut self.st }
    fn device_desc(&self) -> Vec<u8> { let mut d = device_desc(0x0002, 64, 0x0200); d[16] = 0; d }
    fn config_desc(&self) -> Vec<u8> { hid_config(0, TABLET_REPORT.len(), 6, 10) }
    fn string(&self, i: u8) -> Option<&'static str> { match i { 1 => Some("sbc-emu"), 2 => Some("Virtual USB Tablet"), _ => None } }
    fn class_setup(&mut self, s: [u8; 8]) -> Option<Vec<u8>> { hid_class(s, &TABLET_REPORT, &self.config_desc(), &self.last) }
    fn data_in(&mut self, ep: u8, max: usize) -> Option<Vec<u8>> {
        if ep != 1 || !self.st.configured { return None; }
        let (b, x, y, w) = self.events.pop_front()?;
        let mut r = [b, 0, 0, 0, 0, w as u8];
        r[1..3].copy_from_slice(&x.to_le_bytes());
        r[3..5].copy_from_slice(&y.to_le_bytes());
        self.last = r;
        Some(r[..max.min(6)].to_vec())
    }
    fn has_input(&self) -> bool { self.st.configured && !self.events.is_empty() }
}

// ---------------------------------------------------------------------- the USB stick

/// Bulk-only transport mass storage with a SCSI direct-access disk (512-byte blocks).
pub struct Storage {
    pub st: Std,
    pub disk: Box<dyn Disk + Send>,
    /// what the next IN transfer returns: data of the current command, then its CSW
    data: Vec<u8>,
    csw: Option<[u8; 13]>,
    /// a WRITE waiting for its data: (tag, lba, bytes expected)
    write: Option<(u32, u64, usize)>,
    /// sense key, additional sense code (REQUEST SENSE)
    sense: (u8, u8),
}

impl Storage {
    pub fn new(disk: Box<dyn Disk + Send>) -> Self {
        Storage { st: Std::default(), disk, data: Vec::new(), csw: None, write: None, sense: (0, 0) }
    }

    fn status(&mut self, tag: u32, residue: u32, failed: bool) {
        let mut c = [0u8; 13];
        c[0..4].copy_from_slice(b"USBS");
        c[4..8].copy_from_slice(&tag.to_le_bytes());
        c[8..12].copy_from_slice(&residue.to_le_bytes());
        c[12] = failed as u8;
        self.csw = Some(c);
    }

    /// A CBW arrived: run its SCSI command.
    fn command(&mut self, cbw: &[u8]) {
        if cbw.len() != 31 || &cbw[0..4] != b"USBC" { return; }
        let tag = u32::from_le_bytes(cbw[4..8].try_into().unwrap());
        let want = u32::from_le_bytes(cbw[8..12].try_into().unwrap()) as usize;
        let cdb = &cbw[15..31];
        let blocks = self.disk.sectors();
        let be32 = |b: &[u8]| u32::from_be_bytes(b.try_into().unwrap()) as u64;
        let be64 = |b: &[u8]| u64::from_be_bytes(b.try_into().unwrap());
        let fail = |s: &mut Self, key: u8, asc: u8| { s.sense = (key, asc); s.status(tag, want as u32, true); };
        let reply: Vec<u8> = match cdb[0] {
            0x00 | 0x1e | 0x1b | 0x35 | 0x91 => Vec::new(), // TEST UNIT READY, PREVENT/ALLOW REMOVAL, START STOP, SYNCHRONIZE CACHE
            0x03 => { // REQUEST SENSE
                let mut s = vec![0u8; 18];
                s[0] = 0x70; s[2] = self.sense.0; s[7] = 10; s[12] = self.sense.1;
                self.sense = (0, 0);
                s
            }
            0x12 => { // INQUIRY
                if cdb[1] & 1 != 0 { return fail(self, 5, 0x24); } // no vital product data pages
                let mut d = vec![0u8, 0x80, 0x06, 0x02, 31, 0, 0, 0];
                d.extend_from_slice(b"sbc-emu USB Stick       1.00"); // vendor (8), product (16), revision (4)
                d.resize(36, b' ');
                d
            }
            0x1a => vec![3, 0, 0, 0],                 // MODE SENSE(6): no pages, not write protected
            0x5a => vec![0, 6, 0, 0, 0, 0, 0, 0],     // MODE SENSE(10)
            0x23 => {                                 // READ FORMAT CAPACITIES
                let mut d = vec![0, 0, 0, 8];
                d.extend_from_slice(&(blocks as u32).to_be_bytes());
                d.extend_from_slice(&[2, 0, 2, 0]); // formatted media, 512-byte blocks
                d
            }
            0x25 => { let mut d = ((blocks.saturating_sub(1)).min(0xffff_ffff) as u32).to_be_bytes().to_vec(); d.extend_from_slice(&512u32.to_be_bytes()); d }
            0x9e if cdb[1] & 0x1f == 0x10 => { // READ CAPACITY(16)
                let mut d = vec![0u8; 32];
                d[0..8].copy_from_slice(&blocks.saturating_sub(1).to_be_bytes());
                d[8..12].copy_from_slice(&512u32.to_be_bytes());
                d
            }
            0x28 | 0x88 => { // READ(10), READ(16)
                let (lba, n) = if cdb[0] == 0x28 { (be32(&cdb[2..6]), u16::from_be_bytes([cdb[7], cdb[8]]) as u64) } else { (be64(&cdb[2..10]), be32(&cdb[10..14])) };
                if lba + n > blocks { return fail(self, 5, 0x21); }
                let mut d = vec![0u8; (n * 512) as usize];
                self.disk.read(lba, &mut d);
                d
            }
            0x2a | 0x8a => { // WRITE(10), WRITE(16)
                let (lba, n) = if cdb[0] == 0x2a { (be32(&cdb[2..6]), u16::from_be_bytes([cdb[7], cdb[8]]) as u64) } else { (be64(&cdb[2..10]), be32(&cdb[10..14])) };
                if lba + n > blocks { return fail(self, 5, 0x21); }
                if n == 0 { Vec::new() } else { self.write = Some((tag, lba, (n * 512) as usize)); return; }
            }
            _ => return fail(self, 5, 0x20), // ILLEGAL REQUEST, invalid command
        };
        let n = reply.len().min(want);
        self.data = reply[..n].to_vec();
        self.status(tag, (want - n) as u32, false);
    }
}

impl UsbDevice for Storage {
    fn std(&mut self) -> &mut Std { &mut self.st }
    fn device_desc(&self) -> Vec<u8> { device_desc(0x0003, 64, 0x0200) }
    fn config_desc(&self) -> Vec<u8> {
        let mut d = vec![
            9, 2, 0, 0, 1, 1, 0, 0x80, 100, // config: 1 interface, bus powered, 200 mA
            9, 4, 0, 0, 2, 8, 6, 0x50, 0, // interface: mass storage, SCSI, bulk-only
            7, 5, 0x81, 2, 0x00, 0x02, 0, // endpoint 1 IN, bulk, 512 bytes
            7, 5, 0x02, 2, 0x00, 0x02, 0, // endpoint 2 OUT, bulk, 512 bytes
        ];
        d[2] = d.len() as u8;
        d
    }
    fn string(&self, i: u8) -> Option<&'static str> { match i { 1 => Some("sbc-emu"), 2 => Some("Virtual USB Stick"), 3 => Some("0123456789AB"), _ => None } }
    fn speed(&self) -> Speed { Speed::High }
    fn class_setup(&mut self, s: [u8; 8]) -> Option<Vec<u8>> {
        match (s[0], s[1]) {
            (0xa1, 0xfe) => Some(vec![0]), // GET MAX LUN
            (0x21, 0xff) => { self.data.clear(); self.csw = None; self.write = None; Some(Vec::new()) } // bulk-only reset
            _ => None,
        }
    }
    fn data_in(&mut self, ep: u8, max: usize) -> Option<Vec<u8>> {
        if ep != 1 { return None; }
        if !self.data.is_empty() {
            let n = self.data.len().min(max);
            return Some(self.data.drain(..n).collect());
        }
        self.csw.take().map(|c| c[..max.min(13)].to_vec())
    }
    fn data_out(&mut self, ep: u8, data: &[u8]) {
        if ep != 2 { return; }
        match self.write.take() {
            Some((tag, lba, want)) => {
                let n = data.len().min(want);
                let whole = n / 512 * 512;
                if whole > 0 { self.disk.write(lba, &data[..whole]); }
                if want - n > 0 && n > 0 { self.write = Some((tag, lba + whole as u64 / 512, want - n)); } else { self.status(tag, (want - n) as u32, false); }
            }
            None => self.command(data),
        }
    }
    fn bus_reset(&mut self) {
        self.st = Std::default();
        self.data.clear();
        self.csw = None;
        self.write = None;
    }
}

// ---------------------------------------------------------------------- where devices sit

/// The devices that exist, and which controller port each one is plugged into.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum DevId { #[default] Keyboard, Tablet, Storage, Camera, Net }

#[derive(Default)]
pub struct Devices {
    pub kbd: Keyboard,
    pub tablet: Tablet,
    pub storage: Option<Storage>,
    pub camera: Option<Camera>,
    pub net: Option<Net>,
}

impl Devices {
    pub fn get(&mut self, id: DevId) -> Option<&mut dyn UsbDevice> {
        match id {
            DevId::Keyboard => Some(&mut self.kbd),
            DevId::Tablet => Some(&mut self.tablet),
            DevId::Storage => self.storage.as_mut().map(|s| s as &mut dyn UsbDevice),
            DevId::Camera => self.camera.as_mut().map(|s| s as &mut dyn UsbDevice),
            DevId::Net => self.net.as_mut().map(|s| s as &mut dyn UsbDevice),
        }
    }

    /// Guest time for devices that keep time (the camera's frame rate).
    pub fn set_time(&mut self, now: u64) { if let Some(c) = self.camera.as_mut() { c.set_time(now); } }
    pub fn has_input(&self, id: DevId) -> bool {
        match id {
            DevId::Keyboard => self.kbd.has_input(),
            DevId::Tablet => self.tablet.has_input(),
            DevId::Storage => false,
            DevId::Camera => self.camera.as_ref().is_some_and(|c| c.has_input()),
            DevId::Net => self.net.as_ref().is_some_and(|n| n.has_input()),
        }
    }
}

// ---------------------------------------------------------------------- snapshots

crate::snap_enum!(DevId { Keyboard, Tablet, Storage, Camera, Net });
crate::snap_struct!(Std { address, pending_address, configured, pending_in });
crate::snap_struct!(Keyboard { st, held, modifiers, events, last, leds });
crate::snap_struct!(Tablet { st, events, last, state });

impl crate::snap::Snap for Storage {
    fn save(&self, w: &mut crate::snap::W) {
        let Self { st, disk, data, csw, write, sense } = self;
        st.save(w); data.save(w); csw.save(w); write.save(w); sense.save(w);
        crate::snap::save_disk(w, Some(disk.as_ref()));
    }
    fn load(&mut self, r: &mut crate::snap::R) -> Result<(), crate::snap::SnapError> {
        let Self { st, disk, data, csw, write, sense } = self;
        st.load(r)?; data.load(r)?; csw.load(r)?; write.load(r)?; sense.load(r)?;
        crate::snap::load_disk(r, Some(disk.as_mut()), "a USB stick (--usb-drive)")
    }
}

/// All of it, pending input included (`save_state` is the Pi 4's older format).
impl crate::snap::Snap for Devices {
    fn save(&self, w: &mut crate::snap::W) {
        let Self { kbd, tablet, storage, camera, net } = self;
        kbd.save(w); tablet.save(w);
        crate::snap::save_fitted(w, storage);
        // one flag for the devices this format leaves out (refused on load)
        w.bool(camera.is_some() || net.is_some());
    }
    fn load(&mut self, r: &mut crate::snap::R) -> Result<(), crate::snap::SnapError> {
        let Self { kbd, tablet, storage, camera, net } = self;
        kbd.load(r)?; tablet.load(r)?;
        crate::snap::load_fitted(r, storage, "a USB stick (--usb-drive)")?;
        if r.bool()? || camera.is_some() || net.is_some() {
            return Err(crate::snap::SnapError("the USB camera and the USB Ethernet adapter are not in this snapshot format".into()));
        }
        Ok(())
    }
}


fn save_std(w: &mut crate::snap::W, s: &Std) {
    w.u8(s.address); w.bool(s.pending_address.is_some()); w.u8(s.pending_address.unwrap_or(0));
    w.bool(s.configured); w.bytes(&s.pending_in);
}
fn load_std(r: &mut crate::snap::R) -> Result<Std, crate::snap::SnapError> {
    let address = r.u8()?; let pa = r.bool()?; let a = r.u8()?;
    Ok(Std { address, pending_address: pa.then_some(a), configured: r.bool()?, pending_in: r.bytes()?.to_vec() })
}

impl Devices {
    pub fn save_state(&self, w: &mut crate::snap::W) {
        w.tag("usbdev-v1");
        let k = &self.kbd;
        save_std(w, &k.st); w.bytes(&k.held); w.u8(k.modifiers); w.u8(k.leds);
        w.bool(k.last.is_some()); w.bytes(&k.last.unwrap_or([0; 8]));
        save_std(w, &self.tablet.st); w.bytes(&self.tablet.last);
        w.bool(self.storage.is_some());
        if let Some(s) = self.storage.as_ref() {
            save_std(w, &s.st); w.bytes(&s.data); w.bool(s.csw.is_some()); w.bytes(&s.csw.unwrap_or([0; 13]));
            w.bool(s.write.is_some());
            let (t, l, n) = s.write.unwrap_or((0, 0, 0));
            w.u32(t); w.u64(l); w.u64(n as u64);
            w.u8(s.sense.0); w.u8(s.sense.1);
            let ov = s.disk.overlay();
            w.u64(ov.len() as u64);
            for (lba, data) in ov { w.u64(lba); w.bytes(&data); }
        }
    }
    pub fn load_state(&mut self, r: &mut crate::snap::R) -> Result<(), crate::snap::SnapError> {
        r.tag("usbdev-v1")?;
        let k = &mut self.kbd;
        k.st = load_std(r)?; k.held = r.bytes()?.to_vec(); k.modifiers = r.u8()?; k.leds = r.u8()?;
        let has = r.bool()?; let l = r.bytes()?; k.last = if has { l.try_into().ok() } else { None };
        k.events.clear();
        self.tablet.st = load_std(r)?; self.tablet.last = r.bytes()?.try_into().unwrap_or([0; 6]);
        self.tablet.events.clear();
        if r.bool()? {
            let st = load_std(r)?; let data = r.bytes()?.to_vec(); let has = r.bool()?; let c = r.bytes()?.try_into().unwrap_or([0; 13]);
            let hw = r.bool()?; let (t, l, n) = (r.u32()?, r.u64()?, r.u64()? as usize);
            let sense = (r.u8()?, r.u8()?);
            let n_ov = r.u64()?;
            let mut ov = Vec::new();
            for _ in 0..n_ov { ov.push((r.u64()?, r.bytes()?.to_vec())); }
            if let Some(s) = self.storage.as_mut() {
                s.st = st; s.data = data; s.csw = has.then_some(c); s.write = hw.then_some((t, l, n)); s.sense = sense;
                // Exactly the snapshot's sectors: writes made after it was taken are dropped.
                s.disk.restore_overlay(ov);
            }
        }
        Ok(())
    }
}

// ---------------------------------------------------------------------- the webcam

/// A UVC 1.1 webcam streaming uncompressed YUYV over a bulk endpoint (no alternate settings),
/// 640x480 at 30 frames per second of guest time: colour bars with a box moving across.
pub struct Camera {
    pub st: Std,
    /// the VS probe/commit control (34 bytes, UVC 1.1)
    probe: [u8; 34],
    streaming: bool,
    /// guest time (ns), and when the next frame is due
    now: u64,
    next_frame: u64,
    /// the frame being sent: its bytes, how much has gone, the frame id bit
    frame: Vec<u8>,
    sent: usize,
    fid: u8,
    /// the payload being sent (header and data; it may span several bulk transfers)
    payload: Vec<u8>,
    poff: usize,
    pub frames: u64,
    /// where the pictures come from (the host's camera, a video); none: a test pattern
    source: Option<FrameSource>,
}

/// The latest picture from outside, 640x480 YUYV (`CAM_FRAME` bytes), for a camera to send; a
/// host thread fills it.
pub type FrameSource = std::sync::Arc<std::sync::Mutex<Option<Vec<u8>>>>;

pub const CAM_W: usize = 640;
pub const CAM_H: usize = 480;
pub const CAM_FRAME: usize = CAM_W * CAM_H * 2;
const CAM_INTERVAL: u32 = 333_333; // 100 ns units: 30 fps
const CAM_PAYLOAD: usize = 65_536;

impl Default for Camera { fn default() -> Self { Self::new() } }

impl Camera {
    pub fn new() -> Self {
        let mut c = Camera { st: Std::default(), probe: [0; 34], streaming: false, now: 0, next_frame: 0, frame: Vec::new(), sent: 0, fid: 0, payload: Vec::new(), poff: 0, frames: 0, source: None };
        c.probe = Self::default_probe();
        c
    }

    /// A camera that sends the pictures `source` holds (the test pattern until the first comes).
    pub fn with_source(source: FrameSource) -> Self {
        Camera { source: Some(source), ..Self::new() }
    }

    fn default_probe() -> [u8; 34] {
        let mut p = [0u8; 34];
        p[2] = 1; // format 1
        p[3] = 1; // frame 1
        p[4..8].copy_from_slice(&CAM_INTERVAL.to_le_bytes());
        p[18..22].copy_from_slice(&(CAM_FRAME as u32).to_le_bytes());
        p[22..26].copy_from_slice(&(CAM_PAYLOAD as u32).to_le_bytes());
        p[26..30].copy_from_slice(&48_000_000u32.to_le_bytes());
        p[30] = 3; // framing info: FID and EOF
        p
    }

    pub fn set_time(&mut self, now: u64) { self.now = now; }

    /// The source's latest picture, or colour bars with a white box moving across them and a grey
    /// ramp along the bottom.
    fn render(&self) -> Vec<u8> {
        if let Some(f) = self.source.as_ref().and_then(|s| s.lock().ok().and_then(|f| f.clone())).filter(|f| f.len() == CAM_FRAME) {
            return f;
        }
        const BARS: [(u8, u8, u8); 8] = [(235, 128, 128), (210, 16, 146), (170, 166, 16), (145, 54, 34), (106, 202, 222), (81, 90, 240), (41, 240, 110), (16, 128, 128)];
        let mut f = vec![0u8; CAM_FRAME];
        let bx = (self.frames as usize * 8) % (CAM_W - 64);
        for y in 0..CAM_H {
            for x in (0..CAM_W).step_by(2) {
                let (mut yy, mut u, mut v) = BARS[x * 8 / CAM_W];
                if y >= CAM_H - 48 { yy = (16 + x * 219 / CAM_W) as u8; u = 128; v = 128; }
                if (bx..bx + 64).contains(&x) && (200..264).contains(&y) { yy = 235; u = 128; v = 128; }
                let o = (y * CAM_W + x) * 2;
                f[o..o + 4].copy_from_slice(&[yy, u, yy, v]);
            }
        }
        f
    }
}

impl UsbDevice for Camera {
    fn std(&mut self) -> &mut Std { &mut self.st }
    fn device_desc(&self) -> Vec<u8> {
        // class 0xef/2/1: a function made of an interface association
        let mut d = device_desc(0x0004, 64, 0x0200);
        d[4] = 0xef; d[5] = 2; d[6] = 1;
        d
    }
    fn config_desc(&self) -> Vec<u8> {
        let mut vc = vec![
            13, 0x24, 0x01, 0x10, 0x01, 0, 0, 0x00, 0x6c, 0xdc, 0x02, 1, 1, // VC header, UVC 1.1, 48 MHz, streaming interface 1
            18, 0x24, 0x02, 1, 0x01, 0x02, 0, 0, 0, 0, 0, 0, 0, 0, 3, 0, 0, 0, // camera terminal 1
            9, 0x24, 0x03, 2, 0x01, 0x01, 0, 1, 0, // output terminal 2 (USB streaming) from 1
        ];
        let vc_len = vc.len() as u16;
        vc[5..7].copy_from_slice(&vc_len.to_le_bytes());
        let mut vs = vec![14, 0x24, 0x01, 1, 0, 0, 0x81, 0, 2, 0, 0, 0, 1, 0]; // input header: 1 format, EP 0x81, terminal 2
        vs.extend_from_slice(&[27, 0x24, 0x04, 1, 1]); // uncompressed format 1, 1 frame
        vs.extend_from_slice(&[0x59, 0x55, 0x59, 0x32, 0x00, 0x00, 0x10, 0x00, 0x80, 0x00, 0x00, 0xaa, 0x00, 0x38, 0x9b, 0x71]); // YUY2
        vs.extend_from_slice(&[16, 1, 0, 0, 0, 0]);
        let mut fr = vec![30, 0x24, 0x05, 1, 0];
        fr.extend_from_slice(&(CAM_W as u16).to_le_bytes());
        fr.extend_from_slice(&(CAM_H as u16).to_le_bytes());
        let rate = (CAM_FRAME * 8 * 30) as u32;
        fr.extend_from_slice(&rate.to_le_bytes());
        fr.extend_from_slice(&rate.to_le_bytes());
        fr.extend_from_slice(&(CAM_FRAME as u32).to_le_bytes());
        fr.extend_from_slice(&CAM_INTERVAL.to_le_bytes());
        fr.push(1);
        fr.extend_from_slice(&CAM_INTERVAL.to_le_bytes());
        vs.extend_from_slice(&fr);
        vs.extend_from_slice(&[6, 0x24, 0x0d, 1, 1, 4]); // colour matching: BT.709, SMPTE 170M
        let vs_len = vs.len() as u16;
        vs[4..6].copy_from_slice(&vs_len.to_le_bytes());
        let mut d = vec![9, 2, 0, 0, 2, 1, 0, 0x80, 250]; // 2 interfaces, 500 mA
        d.extend_from_slice(&[8, 0x0b, 0, 2, 0x0e, 0x03, 0, 2]); // interface association: video
        d.extend_from_slice(&[9, 4, 0, 0, 0, 0x0e, 1, 0, 2]); // interface 0: video control, no endpoints
        d.extend_from_slice(&vc);
        d.extend_from_slice(&[9, 4, 1, 0, 1, 0x0e, 2, 0, 0]); // interface 1: video streaming, 1 endpoint
        d.extend_from_slice(&vs);
        d.extend_from_slice(&[7, 5, 0x81, 2, 0x00, 0x02, 0]); // bulk IN, 512 bytes
        let n = d.len() as u16;
        d[2..4].copy_from_slice(&n.to_le_bytes());
        d
    }
    fn string(&self, i: u8) -> Option<&'static str> { match i { 1 => Some("sbc-emu"), 2 => Some("Virtual USB Camera"), 3 => Some("CAM0001"), _ => None } }
    fn speed(&self) -> Speed { Speed::High }
    fn class_setup(&mut self, s: [u8; 8]) -> Option<Vec<u8>> {
        let (rt, req) = (s[0], s[1]);
        let (cs, iface) = (s[3], s[4]);
        match (rt, req) {
            (0x01, 11) => { if u16::from_le_bytes([s[2], s[3]]) != 0 { return None; } self.streaming = false; Some(Vec::new()) } // SET_INTERFACE alt 0
            (0x81, 10) => Some(vec![0]), // GET_INTERFACE
            (0x02, 1) => { self.streaming = false; self.frame.clear(); self.payload.clear(); self.poff = 0; Some(Vec::new()) } // CLEAR_FEATURE(ENDPOINT_HALT): stream stops
            (0xa1, r) if iface == 1 && (cs == 1 || cs == 2) => match r {
                0x81 => Some(self.probe.to_vec()),                       // GET_CUR
                0x82 | 0x83 | 0x87 => Some(Self::default_probe().to_vec()), // GET_MIN, GET_MAX, GET_DEF
                0x84 => Some(vec![0; 34]),                               // GET_RES
                0x85 => Some(vec![34, 0]),                               // GET_LEN
                0x86 => Some(vec![3]),                                   // GET_INFO: get and set
                _ => None,
            },
            (0x21, 0x01) if iface == 1 && (cs == 1 || cs == 2) => {
                // SET_CUR: the data stage follows (class_out); remember which control
                self.probe[33] = cs;
                Some(Vec::new())
            }
            _ => None,
        }
    }
    fn class_out(&mut self, data: &[u8]) {
        let cs = self.probe[33];
        // only one format, frame size and interval exist: keep ours, whatever was asked
        let _ = data;
        self.probe = Self::default_probe();
        if cs == 2 {
            // commit: streaming starts with the next frame
            self.streaming = true;
            self.frame.clear();
            self.payload.clear();
            self.poff = 0;
            self.next_frame = self.now;
        }
    }
    fn data_in(&mut self, ep: u8, max: usize) -> Option<Vec<u8>> {
        if ep != 1 || !self.streaming || !self.st.configured || max == 0 { return None; }
        if self.poff >= self.payload.len() {
            // next payload: a header, then as much of the frame as fits
            if self.frame.is_empty() {
                if self.now < self.next_frame { return None; }
                self.frame = self.render();
                self.sent = 0;
                self.fid ^= 1;
                self.frames += 1;
                self.next_frame = self.next_frame.max(self.now.saturating_sub(33_333_333)) + 33_333_333;
            }
            let n = (self.frame.len() - self.sent).min(CAM_PAYLOAD - 2);
            let eof = self.sent + n == self.frame.len();
            self.payload.clear();
            self.payload.extend_from_slice(&[2, 0x80 | self.fid | if eof { 2 } else { 0 }]);
            self.payload.extend_from_slice(&self.frame[self.sent..self.sent + n]);
            self.poff = 0;
            self.sent += n;
            if eof { self.frame.clear(); }
        }
        // a transfer never runs past the end of a payload (a short one ends it)
        let n = (self.payload.len() - self.poff).min(max);
        let out = self.payload[self.poff..self.poff + n].to_vec();
        self.poff += n;
        Some(out)
    }
    fn has_input(&self) -> bool { self.streaming && (self.poff < self.payload.len() || !self.frame.is_empty() || self.now >= self.next_frame) }
    fn bus_reset(&mut self) { self.st = Std::default(); self.streaming = false; self.frame.clear(); self.payload.clear(); self.poff = 0; }
}

#[cfg(test)]
mod camera_tests {
    use super::*;

    /// A camera with a source sends the source's picture, and the test pattern until one comes.
    #[test]
    fn pictures_from_a_source() {
        let source: FrameSource = std::sync::Arc::new(std::sync::Mutex::new(None));
        let cam = Camera::with_source(source.clone());
        let pattern = Camera::new().render();
        assert_eq!(cam.render(), pattern);
        *source.lock().unwrap() = Some(vec![7; CAM_FRAME]);
        assert_eq!(cam.render(), vec![7; CAM_FRAME]);
        // a picture of the wrong size is not sent
        *source.lock().unwrap() = Some(vec![7; 16]);
        assert_eq!(cam.render(), pattern);
    }
}

// ---------------------------------------------------------------------- the Ethernet adapter

/// A USB Ethernet adapter of the CDC-ECM class (Linux's `cdc_ether`, no vendor driver needed):
/// one Ethernet frame per bulk transfer each way, and the link state on an interrupt endpoint.
/// The board moves the frames between `take_tx`/`deliver` and its virtual network.
pub struct Net {
    pub st: Std,
    /// frames from the guest, for the network
    tx: VecDeque<Vec<u8>>,
    /// frames for the guest
    rx: VecDeque<Vec<u8>>,
    /// link notifications sent so far (connection, then speed)
    notified: u8,
}

/// The adapter's MAC address, as its iMACAddress string gives it. Universally administered
/// (bit 1 of the first byte clear): Linux names such an ECM adapter `eth0`, a locally administered
/// one `usb0`.
pub const NET_MAC: [u8; 6] = [0x00, 0x1d, 0xe1, 0x5b, 0x0e, 0x01];
const NET_MAC_STR: &str = "001DE15B0E01";
/// The most frames waiting in either direction; more are dropped, as a full NIC would.
const NET_QUEUE: usize = 512;
const NET_MAX_FRAME: usize = 1514;

impl Default for Net { fn default() -> Self { Self::new() } }

impl Net {
    pub fn new() -> Self { Net { st: Std::default(), tx: VecDeque::new(), rx: VecDeque::new(), notified: 0 } }
    /// The frames the guest sent since the last call.
    pub fn take_tx(&mut self) -> Vec<Vec<u8>> { self.tx.drain(..).collect() }
    /// Frames from the network; dropped while the guest has not configured the adapter.
    pub fn deliver(&mut self, frames: impl IntoIterator<Item = Vec<u8>>) {
        for f in frames {
            if self.st.configured && self.rx.len() < NET_QUEUE && (14..=NET_MAX_FRAME).contains(&f.len()) { self.rx.push_back(f); }
        }
    }
    /// The guest has configured the adapter (the link is up for it).
    pub fn up(&self) -> bool { self.st.configured }
}

impl UsbDevice for Net {
    fn std(&mut self) -> &mut Std { &mut self.st }
    fn device_desc(&self) -> Vec<u8> { let mut d = device_desc(0x0005, 64, 0x0200); d[4] = 2; d } // class: communications
    fn config_desc(&self) -> Vec<u8> {
        let mut d = vec![9, 2, 0, 0, 2, 1, 0, 0x80, 50]; // 2 interfaces, 100 mA
        d.extend_from_slice(&[9, 4, 0, 0, 1, 0x02, 0x06, 0, 0]); // interface 0: communications, ECM, 1 endpoint
        d.extend_from_slice(&[5, 0x24, 0x00, 0x10, 0x01]); // CDC header 1.10
        d.extend_from_slice(&[5, 0x24, 0x06, 0, 1]); // union: control 0, data 1
        d.extend_from_slice(&[13, 0x24, 0x0f, 3, 0, 0, 0, 0, 0xea, 0x05, 0, 0, 0]); // Ethernet: MAC in string 3, 1514-byte segments
        d.extend_from_slice(&[7, 5, 0x83, 3, 16, 0, 8]); // interrupt IN 3, 16 bytes, every 16 ms (high speed)
        d.extend_from_slice(&[9, 4, 1, 0, 0, 0x0a, 0, 0, 0]); // interface 1 alt 0: data, no endpoints (no traffic)
        d.extend_from_slice(&[9, 4, 1, 1, 2, 0x0a, 0, 0, 0]); // interface 1 alt 1: data, 2 endpoints
        d.extend_from_slice(&[7, 5, 0x81, 2, 0x00, 0x02, 0]); // bulk IN 1, 512 bytes
        d.extend_from_slice(&[7, 5, 0x02, 2, 0x00, 0x02, 0]); // bulk OUT 2, 512 bytes
        let n = d.len() as u16;
        d[2..4].copy_from_slice(&n.to_le_bytes());
        d
    }
    fn string(&self, i: u8) -> Option<&'static str> { match i { 1 => Some("sbc-emu"), 2 => Some("Virtual USB Ethernet"), 3 => Some(NET_MAC_STR), _ => None } }
    fn speed(&self) -> Speed { Speed::High }
    fn class_setup(&mut self, s: [u8; 8]) -> Option<Vec<u8>> {
        match (s[0], s[1]) {
            (0x21, 0x40..=0x43) => Some(Vec::new()), // multicast filters, power filters, packet filter: everything passes
            (0x81, 10) => Some(vec![1]),              // GET_INTERFACE
            _ => None,
        }
    }
    fn data_in(&mut self, ep: u8, max: usize) -> Option<Vec<u8>> {
        if !self.st.configured { return None; }
        match ep {
            // NETWORK_CONNECTION (connected), then CONNECTION_SPEED_CHANGE (480 Mb/s both ways)
            3 if self.notified == 0 && max >= 8 => { self.notified = 1; Some(vec![0xa1, 0x00, 1, 0, 0, 0, 0, 0]) }
            3 if self.notified == 1 && max >= 16 => {
                self.notified = 2;
                let mut n = vec![0xa1, 0x2a, 0, 0, 0, 0, 8, 0];
                n.extend_from_slice(&480_000_000u32.to_le_bytes());
                n.extend_from_slice(&480_000_000u32.to_le_bytes());
                Some(n)
            }
            1 => {
                // a frame larger than the transfer cannot be split (ECM ends a frame with a short packet): drop it
                while self.rx.front().is_some_and(|f| f.len() > max) { self.rx.pop_front(); }
                self.rx.pop_front()
            }
            _ => None,
        }
    }
    fn data_out(&mut self, ep: u8, data: &[u8]) {
        if ep == 2 && data.len() >= 14 && self.tx.len() < NET_QUEUE {
            // Linux pads a frame that fills whole packets with one byte; Ethernet's own length rules ignore it
            self.tx.push_back(data[..data.len().min(NET_MAX_FRAME)].to_vec());
        }
    }
    fn has_input(&self) -> bool { self.st.configured && (self.notified < 2 || !self.rx.is_empty()) }
    fn bus_reset(&mut self) { self.st = Std::default(); self.notified = 0; self.tx.clear(); self.rx.clear(); }
}

#[cfg(test)]
mod net_tests {
    use super::*;

    fn setup(d: &mut Net, s: [u8; 8]) -> Vec<u8> { assert!(d.setup(s)); d.control_in(4096) }

    /// Enumeration as cdc_ether does it, the link notifications, and frames both ways.
    #[test]
    fn an_ecm_adapter_reports_its_mac_and_link_and_passes_frames() {
        let mut d = Net::new();
        let cfg = setup(&mut d, [0x80, 6, 0, 2, 0, 0, 0xff, 0]);
        assert_eq!(u16::from_le_bytes([cfg[2], cfg[3]]) as usize, cfg.len());
        // the Ethernet functional descriptor names string 3, which holds the MAC as 12 hex digits
        let eth = cfg.windows(3).position(|w| w == [13, 0x24, 0x0f]).unwrap();
        let mac = setup(&mut d, [0x80, 6, cfg[eth + 3], 3, 9, 4, 0xff, 0]);
        let mac: String = mac[2..].chunks(2).map(|c| c[0] as char).collect();
        assert_eq!(mac, NET_MAC.iter().map(|b| format!("{b:02X}")).collect::<String>());
        assert_eq!(NET_MAC[0] & 2, 0, "universally administered: Linux calls it eth0");
        // nothing moves before SET_CONFIGURATION
        d.deliver([vec![0; 60]]);
        assert!(!d.has_input() && d.data_in(1, 2048).is_none());
        setup(&mut d, [0x00, 9, 1, 0, 0, 0, 0, 0]);
        setup(&mut d, [0x21, 0x43, 0x0e, 0, 0, 0, 0, 0]); // SET_ETHERNET_PACKET_FILTER
        assert!(d.has_input());
        assert_eq!(d.data_in(3, 16).unwrap()[..4], [0xa1, 0x00, 1, 0]);
        assert_eq!(d.data_in(3, 16).unwrap()[1], 0x2a);
        assert!(!d.has_input() && d.data_in(3, 16).is_none());
        // frames in: one per transfer; not Ethernet-sized (5000, 3 bytes): never queued
        d.deliver([vec![1; 60], vec![2; 1514], vec![3; 5000], vec![4; 3], vec![5; 100]]);
        assert_eq!(d.data_in(1, 2048), Some(vec![1; 60]));
        // too big for the transfer: dropped, the next one comes
        assert_eq!(d.data_in(1, 1000), Some(vec![5; 100]));
        assert_eq!(d.data_in(1, 2048), None);
        // frames out, the padding byte trimmed to the largest frame
        d.data_out(2, &[5; 1515]);
        d.data_out(2, &[6; 10]);
        assert_eq!(d.take_tx(), vec![vec![5; 1514]]);
        d.bus_reset();
        assert!(!d.has_input());
    }
}
