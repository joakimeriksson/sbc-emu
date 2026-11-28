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
    /// SET_INTERFACE: alternate setting `alt` of interface `iface`; false = STALL
    fn set_interface(&mut self, _iface: u8, _alt: u8) -> bool { true }

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
            (0x01, 11) => { if !self.set_interface(s[4], s[2]) { return false; } Vec::new() }
            (0x00, 1) | (0x00, 3) | (0x02, 1) | (0x02, 3) => Vec::new(),
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
pub enum DevId { #[default] Keyboard, Tablet, Storage, Camera, Net, Serial, Audio }

#[derive(Default)]
pub struct Devices {
    pub kbd: Keyboard,
    pub tablet: Tablet,
    pub storage: Option<Storage>,
    pub camera: Option<Camera>,
    pub net: Option<Net>,
    pub serial: Option<Serial>,
    pub audio: Option<Audio>,
}

impl Devices {
    pub fn get(&mut self, id: DevId) -> Option<&mut dyn UsbDevice> {
        match id {
            DevId::Keyboard => Some(&mut self.kbd),
            DevId::Tablet => Some(&mut self.tablet),
            DevId::Storage => self.storage.as_mut().map(|s| s as &mut dyn UsbDevice),
            DevId::Camera => self.camera.as_mut().map(|s| s as &mut dyn UsbDevice),
            DevId::Net => self.net.as_mut().map(|s| s as &mut dyn UsbDevice),
            DevId::Serial => self.serial.as_mut().map(|s| s as &mut dyn UsbDevice),
            DevId::Audio => self.audio.as_mut().map(|s| s as &mut dyn UsbDevice),
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
            DevId::Serial => self.serial.as_ref().is_some_and(|n| n.has_input()),
            DevId::Audio => false, // isochronous: the host controller's clock paces it
        }
    }
}

// ---------------------------------------------------------------------- snapshots

crate::snap_enum!(DevId { Keyboard, Tablet, Storage, Camera, Net, Serial, Audio });
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
        let Self { kbd, tablet, storage, camera, net, serial, audio } = self;
        kbd.save(w); tablet.save(w);
        crate::snap::save_fitted(w, storage);
        // one flag for the devices this format leaves out (refused on load)
        w.bool(camera.is_some() || net.is_some() || serial.is_some() || audio.is_some());
    }
    fn load(&mut self, r: &mut crate::snap::R) -> Result<(), crate::snap::SnapError> {
        let Self { kbd, tablet, storage, camera, net, serial, audio } = self;
        kbd.load(r)?; tablet.load(r)?;
        crate::snap::load_fitted(r, storage, "a USB stick (--usb-drive)")?;
        if r.bool()? || camera.is_some() || net.is_some() || serial.is_some() || audio.is_some() {
            return Err(crate::snap::SnapError("the USB camera, Ethernet adapter, serial port and sound card are not in this snapshot format".into()));
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

/// A UVC 1.1 webcam on a bulk endpoint (no alternate settings) with the modes a common USB webcam
/// offers: uncompressed YUYV at 640x480 and 320x240 (30 and 15 fps) and 1280x720 (10 and 5 fps,
/// what USB 2.0 bandwidth leaves a real one), and MJPEG at all three sizes at 30 and 15 fps.
/// The probe/commit negotiation picks the mode; frames come at its rate in guest time. The
/// pictures: colour bars with a box moving across, or the host's (`FrameSource`).
pub struct Camera {
    pub st: Std,
    /// the VS probe control (34 bytes, UVC 1.1) as last negotiated, and the committed mode
    probe: [u8; 34],
    mode: Mode,
    /// the control the data stage of a SET_CUR is for (1 probe, 2 commit)
    set_cs: u8,
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
    jpeg: Option<crate::jpeg::Encoder>,
}

/// The latest picture from outside, `CAM_SOURCE_W` x `CAM_SOURCE_H` YUYV (`CAM_SOURCE_FRAME`
/// bytes), for a camera to send in whatever mode it streams; a host thread fills it.
pub type FrameSource = std::sync::Arc<std::sync::Mutex<Option<Vec<u8>>>>;

/// The default mode (format 1, frame 1): YUYV 640x480.
pub const CAM_W: usize = 640;
pub const CAM_H: usize = 480;
pub const CAM_FRAME: usize = CAM_W * CAM_H * 2;
/// A source picture: 4:3, as the camera's sensor; the 4:3 modes are it scaled down, 1280x720 its
/// middle rows.
pub const CAM_SOURCE_W: usize = 1280;
pub const CAM_SOURCE_H: usize = 960;
pub const CAM_SOURCE_FRAME: usize = CAM_SOURCE_W * CAM_SOURCE_H * 2;
const CAM_PAYLOAD: usize = 65_536;
const CAM_JPEG_QUALITY: u8 = 85;

/// Formats (index 1, 2) and their frames (index 1..): width, height, frame intervals (100 ns),
/// the default first.
const CAM_YUYV: u8 = 1;
const CAM_MJPEG: u8 = 2;
type FrameDesc = (usize, usize, &'static [u32]);
const CAM_FORMATS: [&[FrameDesc]; 2] = [
    &[(640, 480, &[333_333, 666_666]), (320, 240, &[333_333, 666_666]), (1280, 720, &[1_000_000, 2_000_000])],
    &[(640, 480, &[333_333, 666_666]), (320, 240, &[333_333, 666_666]), (1280, 720, &[333_333, 666_666])],
];

/// A committed streaming mode.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Mode { pub format: u8, pub frame: u8, pub w: usize, pub h: usize, pub interval: u32 }

impl Mode {
    /// What the camera makes of a probe asking for `format`, `frame`, `interval`: an unknown
    /// format or frame becomes the first, the interval the nearest supported (0: the default).
    pub fn negotiate(format: u8, frame: u8, interval: u32) -> Mode {
        let format = if (1..=CAM_FORMATS.len() as u8).contains(&format) { format } else { CAM_YUYV };
        let frames = CAM_FORMATS[format as usize - 1];
        let frame = if (1..=frames.len() as u8).contains(&frame) { frame } else { 1 };
        let (w, h, intervals) = frames[frame as usize - 1];
        let interval = if interval == 0 { intervals[0] } else { *intervals.iter().min_by_key(|&&i| i.abs_diff(interval)).unwrap() };
        Mode { format, frame, w, h, interval }
    }
    /// The largest frame (bytes): uncompressed, or a JPEG's bound.
    fn max_frame(&self) -> usize { self.w * self.h * 2 }
    fn probe(&self, hint: [u8; 2]) -> [u8; 34] {
        let mut p = [0u8; 34];
        p[0..2].copy_from_slice(&hint);
        p[2] = self.format;
        p[3] = self.frame;
        p[4..8].copy_from_slice(&self.interval.to_le_bytes());
        p[18..22].copy_from_slice(&(self.max_frame() as u32).to_le_bytes());
        p[22..26].copy_from_slice(&(CAM_PAYLOAD as u32).to_le_bytes());
        p[26..30].copy_from_slice(&48_000_000u32.to_le_bytes());
        p[30] = 3; // framing info: FID and EOF
        p
    }
}

impl Default for Camera { fn default() -> Self { Self::new() } }

impl Camera {
    pub fn new() -> Self {
        let mode = Mode::negotiate(CAM_YUYV, 1, 0);
        Camera { st: Std::default(), probe: mode.probe([0; 2]), mode, set_cs: 0, streaming: false, now: 0, next_frame: 0, frame: Vec::new(), sent: 0, fid: 0, payload: Vec::new(), poff: 0, frames: 0, source: None, jpeg: None }
    }

    /// A camera that sends the pictures `source` holds (the test pattern until the first comes).
    pub fn with_source(source: FrameSource) -> Self {
        Camera { source: Some(source), ..Self::new() }
    }

    pub fn set_time(&mut self, now: u64) { self.now = now; }

    /// The mode the guest committed (the default until it does).
    pub fn mode(&self) -> Mode { self.mode }

    /// A YUYV picture of `w` x `h`: the source's latest, or colour bars with a white box moving
    /// across them and a grey ramp along the bottom.
    fn picture(&self, w: usize, h: usize) -> Vec<u8> {
        if let Some(f) = self.source.as_ref().and_then(|s| s.lock().ok().and_then(|f| f.clone())).filter(|f| f.len() == CAM_SOURCE_FRAME) {
            return fit_yuyv(&f, CAM_SOURCE_W, CAM_SOURCE_H, w, h);
        }
        const BARS: [(u8, u8, u8); 8] = [(235, 128, 128), (210, 16, 146), (170, 166, 16), (145, 54, 34), (106, 202, 222), (81, 90, 240), (41, 240, 110), (16, 128, 128)];
        let mut f = vec![0u8; w * h * 2];
        let (bw, ramp) = (64 * w / 640, 48 * h / 480);
        let bx = (self.frames as usize * 8 * w / 640) % (w - bw);
        let by = 200 * h / 480..264 * h / 480;
        for y in 0..h {
            for x in (0..w).step_by(2) {
                let (mut yy, mut u, mut v) = BARS[x * 8 / w];
                if y >= h - ramp { yy = (16 + x * 219 / w) as u8; u = 128; v = 128; }
                if (bx..bx + bw).contains(&x) && by.contains(&y) { yy = 235; u = 128; v = 128; }
                let o = (y * w + x) * 2;
                f[o..o + 4].copy_from_slice(&[yy, u, yy, v]);
            }
        }
        f
    }

    /// The next frame in the committed mode: YUYV, or a JPEG of it.
    fn render(&mut self) -> Vec<u8> {
        let Mode { format, w, h, .. } = self.mode;
        let f = self.picture(w, h);
        if format != CAM_MJPEG { return f; }
        self.jpeg.get_or_insert_with(|| crate::jpeg::Encoder::new(CAM_JPEG_QUALITY)).encode_yuyv(&f, w, h)
    }

    fn stop(&mut self) { self.streaming = false; self.frame.clear(); self.payload.clear(); self.poff = 0; }
}

/// Scale a YUYV picture to `w` x `h`, filling it (the middle of the source when the shapes
/// differ, as a sensor's crop): each pixel the average of the source pixels under it.
pub fn fit_yuyv(src: &[u8], sw: usize, sh: usize, w: usize, h: usize) -> Vec<u8> {
    // the source rectangle with the target's shape
    let (cw, ch) = if sw * h > w * sh { (w * sh / h, sh) } else { (sw, h * sw / w) };
    let (cx, cy) = ((sw - cw) / 2 & !1, (sh - ch) / 2);
    let mut out = vec![0u8; w * h * 2];
    // one plane at a time: luma (2 bytes a pixel), then Cb and Cr (4 bytes a pixel, half width)
    for (step, off, div) in [(2usize, 0usize, 1usize), (4, 1, 2), (4, 3, 2)] {
        let (pw, sx0, scw) = (w / div, cx / div, cw / div);
        for y in 0..h {
            let (y0, y1) = (cy + y * ch / h, (cy + (y + 1) * ch / h).max(cy + y * ch / h + 1));
            for x in 0..pw {
                let (x0, x1) = (sx0 + x * scw / pw, (sx0 + (x + 1) * scw / pw).max(sx0 + x * scw / pw + 1));
                let mut sum = 0u32;
                for yy in y0..y1 { for xx in x0..x1 { sum += src[yy * sw * 2 + xx * step + off] as u32; } }
                out[y * w * 2 + x * step + off] = (sum / ((y1 - y0) * (x1 - x0)) as u32) as u8;
            }
        }
    }
    out
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
        let n = CAM_FORMATS.len() as u8;
        // input header: the formats, EP 0x81, terminal 2, no controls per format
        let mut vs = vec![13 + n, 0x24, 0x01, n, 0, 0, 0x81, 0, 2, 0, 0, 0, 1];
        vs.extend(std::iter::repeat_n(0, n as usize));
        for (k, frames) in CAM_FORMATS.iter().enumerate() {
            let format = k as u8 + 1;
            if format == CAM_YUYV {
                vs.extend_from_slice(&[27, 0x24, 0x04, format, frames.len() as u8]); // uncompressed
                vs.extend_from_slice(&[0x59, 0x55, 0x59, 0x32, 0x00, 0x00, 0x10, 0x00, 0x80, 0x00, 0x00, 0xaa, 0x00, 0x38, 0x9b, 0x71]); // YUY2
                vs.extend_from_slice(&[16, 1, 0, 0, 0, 0]);
            } else {
                vs.extend_from_slice(&[11, 0x24, 0x06, format, frames.len() as u8, 1, 1, 0, 0, 0, 0]); // MJPEG, fixed-size samples
            }
            for (i, &(w, h, intervals)) in frames.iter().enumerate() {
                let mut fr = vec![26 + 4 * intervals.len() as u8, 0x24, if format == CAM_YUYV { 0x05 } else { 0x07 }, i as u8 + 1, 0];
                fr.extend_from_slice(&(w as u16).to_le_bytes());
                fr.extend_from_slice(&(h as u16).to_le_bytes());
                let bits = |iv: u32| (w * h * 16) as u64 * 10_000_000 / iv as u64;
                fr.extend_from_slice(&(bits(*intervals.iter().max().unwrap()) as u32).to_le_bytes());
                fr.extend_from_slice(&(bits(*intervals.iter().min().unwrap()) as u32).to_le_bytes());
                fr.extend_from_slice(&((w * h * 2) as u32).to_le_bytes());
                fr.extend_from_slice(&intervals[0].to_le_bytes());
                fr.push(intervals.len() as u8);
                for iv in intervals { fr.extend_from_slice(&iv.to_le_bytes()); }
                vs.extend_from_slice(&fr);
            }
            vs.extend_from_slice(&[6, 0x24, 0x0d, 1, 1, 4]); // colour matching: BT.709, SMPTE 170M
        }
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
            (0x81, 10) => Some(vec![0]), // GET_INTERFACE
            (0x02, 1) => { self.stop(); Some(Vec::new()) } // CLEAR_FEATURE(ENDPOINT_HALT): stream stops
            (0xa1, r) if iface == 1 && (cs == 1 || cs == 2) => {
                let (format, frame) = (self.probe[2], self.probe[3]);
                let intervals = CAM_FORMATS[format as usize - 1][frame as usize - 1].2;
                let at = |iv: u32| Mode::negotiate(format, frame, iv).probe([0; 2]).to_vec();
                match r {
                    0x81 => Some(if cs == 1 { self.probe.to_vec() } else { self.mode.probe([0; 2]).to_vec() }), // GET_CUR
                    0x82 => Some(at(*intervals.iter().min().unwrap())),     // GET_MIN
                    0x83 => Some(at(*intervals.iter().max().unwrap())),     // GET_MAX
                    0x87 => Some(Mode::negotiate(CAM_YUYV, 1, 0).probe([0; 2]).to_vec()), // GET_DEF
                    0x84 => Some(vec![0; 34]),                              // GET_RES
                    0x85 => Some(vec![34, 0]),                              // GET_LEN
                    0x86 => Some(vec![3]),                                  // GET_INFO: get and set
                    _ => None,
                }
            }
            (0x21, 0x01) if iface == 1 && (cs == 1 || cs == 2) => {
                // SET_CUR: the data stage follows (class_out)
                self.set_cs = cs;
                Some(Vec::new())
            }
            _ => None,
        }
    }
    fn class_out(&mut self, data: &[u8]) {
        let b = |i: usize| data.get(i).copied().unwrap_or(0);
        let mode = Mode::negotiate(b(2), b(3), u32::from_le_bytes([b(4), b(5), b(6), b(7)]));
        self.probe = mode.probe([b(0), b(1)]);
        if std::mem::take(&mut self.set_cs) == 2 {
            // commit: streaming starts with the next frame, in this mode
            self.mode = mode;
            self.stop();
            self.streaming = true;
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
                let period = self.mode.interval as u64 * 100;
                self.next_frame = self.next_frame.max(self.now.saturating_sub(period)) + period;
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
    fn bus_reset(&mut self) { self.st = Std::default(); self.stop(); }
}

#[cfg(test)]
mod camera_tests {
    use super::*;

    fn setup(d: &mut Camera, s: [u8; 8]) -> Vec<u8> { assert!(d.setup(s)); d.control_in(4096) }

    /// A camera with a source sends the source's picture, and the test pattern until one comes.
    #[test]
    fn pictures_from_a_source() {
        let source: FrameSource = std::sync::Arc::new(std::sync::Mutex::new(None));
        let cam = Camera::with_source(source.clone());
        let pattern = Camera::new().picture(CAM_W, CAM_H);
        assert_eq!(cam.picture(CAM_W, CAM_H), pattern);
        *source.lock().unwrap() = Some(vec![7; CAM_SOURCE_FRAME]);
        assert_eq!(cam.picture(CAM_W, CAM_H), vec![7; CAM_FRAME]);
        assert_eq!(cam.picture(1280, 720), vec![7; 1280 * 720 * 2]);
        // a picture of the wrong size is not sent
        *source.lock().unwrap() = Some(vec![7; 16]);
        assert_eq!(cam.picture(CAM_W, CAM_H), pattern);
    }

    /// 4:3 scales down; 16:9 is the middle rows.
    #[test]
    fn fitting_a_source() {
        // luma = the source row / 4, chroma = 128
        let (sw, sh) = (32, 24);
        let src: Vec<u8> = (0..sh).flat_map(|y| (0..sw).flat_map(move |_| [y as u8 * 4, 128])).collect();
        let half = fit_yuyv(&src, sw, sh, 16, 12);
        assert_eq!((half[0], half[1], half[16 * 2 * 11]), (2, 128, 90)); // rows 0-1, rows 22-23
        let wide = fit_yuyv(&src, sw, sh, 32, 18);
        assert_eq!((wide[0], wide[32 * 2 * 17]), (12, 80)); // rows 3 and 20
    }

    /// The descriptors list both formats and every frame; negotiation clamps to what exists and
    /// a commit switches the stream to it: MJPEG frames are JPEG pictures.
    #[test]
    fn formats_and_negotiation() {
        let mut d = Camera::new();
        let cfg = setup(&mut d, [0x80, 6, 0, 2, 0, 0, 0xff, 0x0f]);
        assert_eq!(u16::from_le_bytes([cfg[2], cfg[3]]) as usize, cfg.len());
        let descs: Vec<&[u8]> = {
            let mut v = Vec::new();
            let mut i = 0;
            while i < cfg.len() { v.push(&cfg[i..i + cfg[i] as usize]); i += cfg[i] as usize; }
            v
        };
        let vs: Vec<&&[u8]> = descs.iter().filter(|d| d[1] == 0x24).collect();
        let header = vs.iter().find(|d| d[2] == 1 && d.len() >= 13 && d[3] == 2).expect("VS input header, 2 formats");
        let total: usize = descs.iter().skip_while(|d| !std::ptr::eq(**d, **header)).take_while(|d| d[1] == 0x24).map(|d| d.len()).sum();
        assert_eq!(u16::from_le_bytes([header[4], header[5]]) as usize, total);
        assert!(vs.iter().any(|d| d[2] == 0x04 && d[3] == 1 && d[4] == 3 && &d[5..9] == b"YUY2"));
        assert!(vs.iter().any(|d| d[2] == 0x06 && d[3] == 2 && d[4] == 3));
        let sizes = |sub: u8| vs.iter().filter(|d| d[2] == sub).map(|d| (u16::from_le_bytes([d[5], d[6]]), u16::from_le_bytes([d[7], d[8]]), d[25])).collect::<Vec<_>>();
        assert_eq!(sizes(0x05), [(640, 480, 2), (320, 240, 2), (1280, 720, 2)]);
        assert_eq!(sizes(0x07), [(640, 480, 2), (320, 240, 2), (1280, 720, 2)]);
        setup(&mut d, [0x00, 9, 1, 0, 0, 0, 0, 0]);
        // probe MJPEG 1280x720 at 29 fps: the nearest, 30
        let mut ask = [0u8; 34];
        ask[2] = 2; ask[3] = 3; ask[4..8].copy_from_slice(&344_827u32.to_le_bytes());
        assert!(d.setup([0x21, 1, 0, 1, 1, 0, 34, 0]));
        d.control_out(&ask);
        let cur = setup(&mut d, [0xa1, 0x81, 0, 1, 1, 0, 34, 0]);
        assert_eq!((cur[2], cur[3], u32::from_le_bytes(cur[4..8].try_into().unwrap())), (2, 3, 333_333));
        assert_eq!(u32::from_le_bytes(cur[18..22].try_into().unwrap()), 1280 * 720 * 2);
        assert_eq!(d.mode(), Mode::negotiate(1, 1, 0), "a probe alone does not change the stream");
        // nonsense is clamped: format 9 frame 7 -> YUYV 640x480 at 30
        ask[2] = 9; ask[3] = 7; ask[4..8].copy_from_slice(&1u32.to_le_bytes());
        assert!(d.setup([0x21, 1, 0, 1, 1, 0, 34, 0]));
        d.control_out(&ask);
        assert_eq!(&setup(&mut d, [0xa1, 0x81, 0, 1, 1, 0, 34, 0])[2..8], &[1, 1, 0x15, 0x16, 0x05, 0]);
        // commit MJPEG 320x240 at 15 fps
        ask[2] = 2; ask[3] = 2; ask[4..8].copy_from_slice(&666_666u32.to_le_bytes());
        assert!(d.setup([0x21, 1, 0, 2, 1, 0, 34, 0]));
        d.control_out(&ask);
        assert_eq!(d.mode(), Mode { format: 2, frame: 2, w: 320, h: 240, interval: 666_666 });
        let mut frame = Vec::new();
        loop {
            let p = d.data_in(1, CAM_PAYLOAD).unwrap();
            frame.extend_from_slice(&p[2..]);
            if p[1] & 2 != 0 { break; }
        }
        assert_eq!(&frame[..2], &[0xff, 0xd8]);
        assert_eq!(&frame[frame.len() - 2..], &[0xff, 0xd9]);
        let sof = frame.windows(2).position(|m| m == [0xff, 0xc0]).unwrap();
        assert_eq!(&frame[sof + 5..sof + 9], &[0, 240, 1, 64]);
        // the next frame comes 1/15 s later
        assert!(d.data_in(1, CAM_PAYLOAD).is_none());
        d.set_time(66_666_600);
        assert!(d.has_input() && d.data_in(1, CAM_PAYLOAD).is_some());
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

// ---------------------------------------------------------------------- the serial port

/// A USB serial port of the CDC-ACM class (Linux's `cdc-acm`: `/dev/ttyACM0`), as USB lidars,
/// GPS receivers and microcontroller boards present themselves. A byte stream each way on bulk
/// endpoints; the line coding is kept and reported but does not pace the bytes.
pub struct Serial {
    pub st: Std,
    product: &'static str,
    /// bytes for the guest, and from it
    rx: VecDeque<u8>,
    tx: Vec<u8>,
    /// SET_LINE_CODING's 7 bytes: rate (LE u32), stop bits, parity, data bits
    line: [u8; 7],
    /// SET_CONTROL_LINE_STATE: DTR (bit 0), RTS (bit 1)
    pub lines: u8,
}

/// Bytes held for the guest at most; more are dropped, as a UART's overrun would.
const SERIAL_QUEUE: usize = 1 << 16;

impl Serial {
    /// `product` is the product string, which Linux shows (and udev rules match).
    pub fn new(product: &'static str) -> Self {
        Serial { st: Std::default(), product, rx: VecDeque::new(), tx: Vec::new(), line: [0x00, 0xc2, 0x01, 0x00, 0, 0, 8], lines: 0 }
    }
    /// Bytes from outside, for the guest; dropped while it has not configured the port.
    pub fn push_rx(&mut self, bytes: &[u8]) {
        if !self.st.configured { return; }
        let room = SERIAL_QUEUE - self.rx.len();
        self.rx.extend(&bytes[..bytes.len().min(room)]);
    }
    /// The bytes the guest wrote since the last call.
    pub fn take_tx(&mut self) -> Vec<u8> { std::mem::take(&mut self.tx) }
    /// The rate the guest set, in baud.
    pub fn baud(&self) -> u32 { u32::from_le_bytes([self.line[0], self.line[1], self.line[2], self.line[3]]) }
}

impl UsbDevice for Serial {
    fn std(&mut self) -> &mut Std { &mut self.st }
    fn device_desc(&self) -> Vec<u8> { let mut d = device_desc(0x0006, 64, 0x0200); d[4] = 2; d } // class: communications
    fn config_desc(&self) -> Vec<u8> {
        let mut d = vec![9, 2, 0, 0, 2, 1, 0, 0x80, 50]; // 2 interfaces, 100 mA
        d.extend_from_slice(&[9, 4, 0, 0, 1, 0x02, 0x02, 0, 0]); // interface 0: communications, ACM, 1 endpoint
        d.extend_from_slice(&[5, 0x24, 0x00, 0x10, 0x01]); // CDC header 1.10
        d.extend_from_slice(&[5, 0x24, 0x01, 0, 1]); // call management: none, data interface 1
        d.extend_from_slice(&[4, 0x24, 0x02, 0x02]); // ACM: line coding and line state requests
        d.extend_from_slice(&[5, 0x24, 0x06, 0, 1]); // union: control 0, data 1
        d.extend_from_slice(&[7, 5, 0x83, 3, 16, 0, 8]); // interrupt IN 3 (serial state; never sent)
        d.extend_from_slice(&[9, 4, 1, 0, 2, 0x0a, 0, 0, 0]); // interface 1: data, 2 endpoints
        d.extend_from_slice(&[7, 5, 0x81, 2, 0x00, 0x02, 0]); // bulk IN 1, 512 bytes
        d.extend_from_slice(&[7, 5, 0x02, 2, 0x00, 0x02, 0]); // bulk OUT 2, 512 bytes
        let n = d.len() as u16;
        d[2..4].copy_from_slice(&n.to_le_bytes());
        d
    }
    fn string(&self, i: u8) -> Option<&'static str> { match i { 1 => Some("sbc-emu"), 2 => Some(self.product), 3 => Some("ACM0001"), _ => None } }
    fn speed(&self) -> Speed { Speed::High }
    fn class_setup(&mut self, s: [u8; 8]) -> Option<Vec<u8>> {
        match (s[0], s[1]) {
            (0x21, 0x20) => Some(Vec::new()), // SET_LINE_CODING: the 7 bytes follow (class_out)
            (0xa1, 0x21) => Some(self.line.to_vec()), // GET_LINE_CODING
            (0x21, 0x22) => { self.lines = s[2] & 3; Some(Vec::new()) } // SET_CONTROL_LINE_STATE
            (0x21, 0x23) => Some(Vec::new()), // SEND_BREAK
            _ => None,
        }
    }
    fn class_out(&mut self, data: &[u8]) { if let Ok(l) = <[u8; 7]>::try_from(data) { self.line = l; } }
    fn data_in(&mut self, ep: u8, max: usize) -> Option<Vec<u8>> {
        if ep != 1 || !self.st.configured || self.rx.is_empty() || max == 0 { return None; }
        let n = self.rx.len().min(max);
        Some(self.rx.drain(..n).collect())
    }
    fn data_out(&mut self, ep: u8, data: &[u8]) { if ep == 2 && self.tx.len() < SERIAL_QUEUE { self.tx.extend_from_slice(data); } }
    fn has_input(&self) -> bool { self.st.configured && !self.rx.is_empty() }
    fn bus_reset(&mut self) { self.st = Std::default(); self.rx.clear(); self.tx.clear(); self.lines = 0; }
}

#[cfg(test)]
mod serial_tests {
    use super::*;

    fn setup(d: &mut Serial, s: [u8; 8]) -> Vec<u8> { assert!(d.setup(s)); d.control_in(4096) }

    /// Enumeration and the requests cdc-acm makes, then bytes both ways.
    #[test]
    fn an_acm_port_keeps_its_line_coding_and_passes_bytes() {
        let mut d = Serial::new("Virtual Lidar");
        let cfg = setup(&mut d, [0x80, 6, 0, 2, 0, 0, 0xff, 0]);
        assert_eq!(u16::from_le_bytes([cfg[2], cfg[3]]) as usize, cfg.len());
        assert_eq!(&cfg[9..18], &[9, 4, 0, 0, 1, 2, 2, 0, 0], "interface 0 is CDC ACM");
        let name = setup(&mut d, [0x80, 6, 2, 3, 9, 4, 0xff, 0]);
        assert_eq!(name[2..].chunks(2).map(|c| c[0] as char).collect::<String>(), "Virtual Lidar");
        d.push_rx(b"early");
        setup(&mut d, [0x00, 9, 1, 0, 0, 0, 0, 0]);
        assert!(!d.has_input(), "bytes before the port was configured are dropped");
        // 230400 baud, 8N1
        assert!(d.setup([0x21, 0x20, 0, 0, 0, 0, 7, 0]));
        d.control_out(&[0x00, 0x84, 0x03, 0x00, 0, 0, 8]);
        assert_eq!(d.baud(), 230_400);
        assert_eq!(setup(&mut d, [0xa1, 0x21, 0, 0, 0, 0, 7, 0]), [0x00, 0x84, 0x03, 0x00, 0, 0, 8]);
        setup(&mut d, [0x21, 0x22, 3, 0, 0, 0, 0, 0]);
        assert_eq!(d.lines, 3);
        // bytes in, split by the transfer size; bytes out, in order
        d.push_rx(&[0x54; 100]);
        assert_eq!(d.data_in(1, 64).map(|b| b.len()), Some(64));
        assert_eq!(d.data_in(1, 64).map(|b| b.len()), Some(36));
        assert!(!d.has_input() && d.data_in(1, 64).is_none());
        d.data_out(2, b"ab");
        d.data_out(2, b"c");
        assert_eq!(d.take_tx(), b"abc");
        assert!(d.take_tx().is_empty());
    }
}

// ---------------------------------------------------------------------- the sound card

/// A USB sound card of USB Audio Class 1.0 (Linux's `snd-usb-audio`), full speed: a stereo
/// speaker output and a mono microphone input, 16-bit PCM at 48, 44.1 or 16 kHz, each on an
/// isochronous endpoint with a sampling-frequency control. Laid out after the cheap "USB PnP
/// Audio Device" dongles (Solid State System's SSS1629 and C-Media's CM108 have this shape): one
/// AudioControl interface with USB-streaming -> feature unit (mute, per-channel volume) ->
/// speaker, and microphone -> feature unit (mute, volume) -> USB-streaming; one AudioStreaming
/// interface each way with an empty alternate setting 0 and the stream in setting 1. Their HID
/// button interface and the microphone-to-speaker monitor mixer are left out.
///
/// The strings are the ones Waveshare's UGV software looks for (its `audio_ctrl.py` names the
/// PulseAudio sink by them); ALSA calls the card `Device`. What the guest plays collects in
/// `take_output` (with the speaker's mute and volume applied); what it records comes from
/// `set_input`'s samples, from their start in each recording, silence without them or after them.
pub struct Audio {
    pub st: Std,
    /// alternate setting of interfaces 0, 1 (speaker), 2 (microphone)
    alt: [u8; 3],
    /// sample rates (Hz): speaker, microphone
    rates: [u32; 2],
    /// feature units: speaker mute, left and right volume; microphone mute and volume (1/256 dB)
    spk_mute: bool,
    spk_vol: [i16; 2],
    mic_mute: bool,
    mic_vol: i16,
    /// the control the data stage of a SET_CUR is for
    set: Option<AudioControl>,
    /// stereo samples played since `take_output` (at `AUDIO_OUT_RATE`), kept while `keep_output`;
    /// the resampler's place between the last frame played and the next
    out: Vec<i16>,
    out_last: [f32; 2],
    out_phase: f64,
    pub keep_output: bool,
    /// the microphone's source: rate and mono samples, how far this recording has played (in
    /// source samples)
    input: Option<(u32, Vec<i16>)>,
    in_pos: f64,
    /// microphone frames owed (a packet is 1 ms: rate/1000 frames, the remainder carried)
    in_acc: u32,
    /// frames played and recorded
    pub played: u64,
    pub recorded: u64,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum AudioControl { Rate(u8), Mute(u8), Volume(u8, u8) }

pub const AUDIO_RATES: [u32; 3] = [48_000, 44_100, 16_000];
/// the rate of `take_output`'s samples
pub const AUDIO_OUT_RATE: u32 = 48_000;
/// the speaker's volume range: -63 dB to 0 dB in 1/16 dB steps (1008 steps, as the dongle's)
const SPK_VOL: (i16, i16, i16) = (-63 * 256, 0, 16);
/// the microphone's gain: 0 to 31 dB in 1/16 dB steps (496 steps); reported, not applied
const MIC_VOL: (i16, i16, i16) = (0, 31 * 256, 16);
/// unit ids: speaker feature unit, microphone feature unit
const SPK_FU: u8 = 2;
const MIC_FU: u8 = 5;
/// endpoints: speaker OUT 1, microphone IN 2
const SPK_EP: u8 = 0x01;
const MIC_EP: u8 = 0x82;
/// at most this much output (stereo samples) waits for `take_output`: 10 s at 48 kHz
const AUDIO_OUT_MAX: usize = 48_000 * 2 * 10;

impl Default for Audio { fn default() -> Self { Self::new() } }

impl Audio {
    pub fn new() -> Self {
        Audio {
            st: Std::default(), alt: [0; 3], rates: [48_000; 2], spk_mute: false, spk_vol: [0; 2], mic_mute: false, mic_vol: 0,
            set: None, out: Vec::new(), out_last: [0.0; 2], out_phase: 0.0, keep_output: false, input: None, in_pos: 0.0, in_acc: 0, played: 0, recorded: 0,
        }
    }

    /// What the microphone hears: mono samples at `rate` Hz, from their beginning each time the
    /// guest starts recording (selects the streaming setting), then silence.
    pub fn set_input(&mut self, rate: u32, samples: Vec<i16>) { self.input = Some((rate.max(1), samples)); self.in_pos = 0.0; }

    /// The stereo samples played since the last call, at `AUDIO_OUT_RATE` whatever rate the guest
    /// plays at (one rate for a whole WAV file).
    pub fn take_output(&mut self) -> (u32, Vec<i16>) { (AUDIO_OUT_RATE, std::mem::take(&mut self.out)) }

    /// The sample rates in use: speaker, microphone.
    pub fn rates(&self) -> [u32; 2] { self.rates }

    /// The streaming interfaces' alternate settings (1: streaming): speaker, microphone.
    pub fn streaming(&self) -> (bool, bool) { (self.alt[1] == 1, self.alt[2] == 1) }

    fn format_desc(channels: u8) -> Vec<u8> {
        let mut d = vec![8 + 3 * AUDIO_RATES.len() as u8, 0x24, 0x02, 1, channels, 2, 16, AUDIO_RATES.len() as u8]; // type I, 16-bit
        for r in AUDIO_RATES { d.extend_from_slice(&r.to_le_bytes()[..3]); }
        d
    }

    fn next_mic_sample(&mut self) -> i16 {
        let Some((rate, s)) = self.input.as_ref() else { return 0 };
        let i = self.in_pos as usize;
        let v = match (s.get(i), s.get(i + 1)) {
            (Some(&a), Some(&b)) => { let f = self.in_pos - i as f64; (a as f64 + (b as f64 - a as f64) * f) as i16 }
            (Some(&a), None) => a,
            _ => 0,
        };
        if i < s.len() { self.in_pos += *rate as f64 / self.rates[1] as f64; }
        v
    }
}

/// The gain of a UAC volume (1/256 dB).
fn gain(v: i16) -> f32 { 10f32.powf(v as f32 / 256.0 / 20.0) }

impl UsbDevice for Audio {
    fn std(&mut self) -> &mut Std { &mut self.st }
    fn device_desc(&self) -> Vec<u8> { device_desc(0x0007, 64, 0x0200) }
    fn config_desc(&self) -> Vec<u8> {
        let mut ac = vec![10, 0x24, 0x01, 0x00, 0x01, 0, 0, 2, 1, 2]; // AC header, UAC 1.0, streaming interfaces 1 and 2
        ac.extend_from_slice(&[12, 0x24, 0x02, 1, 0x01, 0x01, 0, 2, 0x03, 0x00, 0, 0]); // input terminal 1: USB streaming, stereo L/R
        ac.extend_from_slice(&[10, 0x24, 0x06, SPK_FU, 1, 1, 0x01, 0x02, 0x02, 0]); // feature unit 2 from 1: mute; volume per channel
        ac.extend_from_slice(&[9, 0x24, 0x03, 3, 0x01, 0x03, 0, SPK_FU, 0]); // output terminal 3: speaker
        ac.extend_from_slice(&[12, 0x24, 0x02, 4, 0x01, 0x02, 0, 1, 0x00, 0x00, 0, 0]); // input terminal 4: microphone, mono
        ac.extend_from_slice(&[9, 0x24, 0x06, MIC_FU, 4, 1, 0x03, 0x00, 0]); // feature unit 5 from 4: mute and volume
        ac.extend_from_slice(&[9, 0x24, 0x03, 6, 0x01, 0x01, 0, MIC_FU, 0]); // output terminal 6: USB streaming
        let n = ac.len() as u16;
        ac[5..7].copy_from_slice(&n.to_le_bytes());
        let mut d = vec![9, 2, 0, 0, 3, 1, 0, 0x80, 50]; // 3 interfaces, 100 mA
        d.extend_from_slice(&[9, 4, 0, 0, 0, 1, 1, 0, 0]); // interface 0: audio control
        d.extend_from_slice(&ac);
        for (iface, terminal, channels, ep, attrs, size) in [(1u8, 1u8, 2u8, SPK_EP, 0x09u8, 200u16), (2, 6, 1, MIC_EP, 0x05, 100)] {
            d.extend_from_slice(&[9, 4, iface, 0, 0, 1, 2, 0, 0]); // alternate 0: no bandwidth
            d.extend_from_slice(&[9, 4, iface, 1, 1, 1, 2, 0, 0]); // alternate 1: streaming
            d.extend_from_slice(&[7, 0x24, 0x01, terminal, 1, 0x01, 0x00]); // AS general: PCM
            d.extend_from_slice(&Self::format_desc(channels));
            // isochronous, adaptive (speaker) or asynchronous (microphone), every frame
            d.extend_from_slice(&[9, 5, ep, attrs, size as u8, (size >> 8) as u8, 1, 0, 0]);
            d.extend_from_slice(&[7, 0x25, 0x01, 0x01, 0, 0, 0]); // sampling frequency control
        }
        let n = d.len() as u16;
        d[2..4].copy_from_slice(&n.to_le_bytes());
        d
    }
    fn string(&self, i: u8) -> Option<&'static str> {
        match i { 1 => Some("Solid State System Co.,Ltd."), 2 => Some("USB PnP Audio Device"), 3 => Some("000000000000"), _ => None }
    }
    fn set_interface(&mut self, iface: u8, alt: u8) -> bool {
        if iface == 0 && alt == 0 || (iface == 1 || iface == 2) && alt <= 1 {
            self.alt[iface as usize] = alt;
            // each recording starts the microphone's input from its beginning
            if iface == 2 { self.in_acc = 0; self.in_pos = 0.0; }
            true
        } else { false }
    }
    fn class_setup(&mut self, s: [u8; 8]) -> Option<Vec<u8>> {
        let (rt, req, cs, ch, ix, unit) = (s[0], s[1], s[3], s[2], s[4], s[5]);
        let rate_of = |ep: u8| match ep { SPK_EP => Some(0usize), MIC_EP => Some(1), _ => None };
        match (rt, req) {
            (0x81, 10) => self.alt.get(ix as usize).map(|&a| vec![a]), // GET_INTERFACE
            // the endpoints' sampling frequency (UAC1 5.2.3.2.3.1)
            (0x22, 0x01) if cs == 1 => { rate_of(ix)?; self.set = Some(AudioControl::Rate(ix)); Some(Vec::new()) }
            (0xa2, 0x81..=0x84) if cs == 1 => Some(self.rates[rate_of(ix)?].to_le_bytes()[..3].to_vec()),
            // the feature units' mute and volume
            (0x21, 0x01) if ix == 0 => {
                self.set = Some(match (unit, cs) {
                    (SPK_FU | MIC_FU, 1) => AudioControl::Mute(unit),
                    (SPK_FU, 2) if ch <= 2 => AudioControl::Volume(unit, ch),
                    (MIC_FU, 2) if ch == 0 => AudioControl::Volume(unit, 0),
                    _ => return None,
                });
                Some(Vec::new())
            }
            (0xa1, r @ 0x81..=0x84) if ix == 0 => match (unit, cs, r) {
                (SPK_FU, 1, 0x81) if ch == 0 => Some(vec![self.spk_mute as u8]),
                (MIC_FU, 1, 0x81) if ch == 0 => Some(vec![self.mic_mute as u8]),
                (SPK_FU, 2, _) if (1..=2).contains(&ch) || (ch == 0 && r != 0x81) => {
                    let v = match r { 0x81 => self.spk_vol[ch as usize - 1], 0x82 => SPK_VOL.0, 0x83 => SPK_VOL.1, _ => SPK_VOL.2 };
                    Some(v.to_le_bytes().to_vec())
                }
                (MIC_FU, 2, _) if ch == 0 => {
                    let v = match r { 0x81 => self.mic_vol, 0x82 => MIC_VOL.0, 0x83 => MIC_VOL.1, _ => MIC_VOL.2 };
                    Some(v.to_le_bytes().to_vec())
                }
                _ => None,
            },
            _ => None,
        }
    }
    fn class_out(&mut self, data: &[u8]) {
        match self.set.take() {
            Some(AudioControl::Rate(ep)) if data.len() >= 3 => {
                let r = u32::from_le_bytes([data[0], data[1], data[2], 0]);
                // a rate the format does not list keeps the one in use
                if AUDIO_RATES.contains(&r) { self.rates[(ep == MIC_EP) as usize] = r; }
            }
            Some(AudioControl::Mute(u)) if !data.is_empty() => { if u == SPK_FU { self.spk_mute = data[0] != 0 } else { self.mic_mute = data[0] != 0 } }
            Some(AudioControl::Volume(u, ch)) if data.len() >= 2 => {
                let v = i16::from_le_bytes([data[0], data[1]]);
                if u == SPK_FU {
                    let v = v.clamp(SPK_VOL.0, SPK_VOL.1);
                    if ch == 0 { self.spk_vol = [v; 2] } else { self.spk_vol[ch as usize - 1] = v }
                } else {
                    self.mic_vol = v.clamp(MIC_VOL.0, MIC_VOL.1);
                }
            }
            _ => {}
        }
    }
    fn data_in(&mut self, ep: u8, max: usize) -> Option<Vec<u8>> {
        if ep != MIC_EP & 0x7f || !self.st.configured { return None; }
        if self.alt[2] != 1 { return Some(Vec::new()); }
        // one packet a millisecond: the frames due in it
        self.in_acc += self.rates[1];
        let n = ((self.in_acc / 1000) as usize).min(max / 2);
        self.in_acc %= 1000;
        let mut d = Vec::with_capacity(n * 2);
        for _ in 0..n {
            let v = self.next_mic_sample();
            d.extend_from_slice(&(if self.mic_mute { 0 } else { v }).to_le_bytes());
        }
        self.recorded += n as u64;
        Some(d)
    }
    fn data_out(&mut self, ep: u8, data: &[u8]) {
        if ep != SPK_EP || self.alt[1] != 1 { return; }
        self.played += (data.len() / 4) as u64;
        if !self.keep_output || self.out.len() >= AUDIO_OUT_MAX { return; }
        let g = if self.spk_mute { [0.0; 2] } else { [gain(self.spk_vol[0]), gain(self.spk_vol[1])] };
        // linear interpolation to AUDIO_OUT_RATE (a copy at that rate)
        let step = self.rates[0] as f64 / AUDIO_OUT_RATE as f64;
        for f in data.chunks_exact(4) {
            let v = [i16::from_le_bytes([f[0], f[1]]) as f32 * g[0], i16::from_le_bytes([f[2], f[3]]) as f32 * g[1]];
            if step == 1.0 {
                self.out.extend_from_slice(&[v[0] as i16, v[1] as i16]);
            } else {
                while self.out_phase < 1.0 {
                    let t = self.out_phase as f32;
                    for c in 0..2 { self.out.push((self.out_last[c] + (v[c] - self.out_last[c]) * t) as i16); }
                    self.out_phase += step;
                }
                self.out_phase -= 1.0;
            }
            self.out_last = v;
        }
    }
    fn bus_reset(&mut self) { self.st = Std::default(); self.alt = [0; 3]; self.set = None; self.in_acc = 0; }
}

#[cfg(test)]
mod audio_tests {
    use super::*;

    fn setup(d: &mut Audio, s: [u8; 8]) -> Vec<u8> { assert!(d.setup(s)); d.control_in(4096) }

    /// The descriptors snd-usb-audio parses: the AC header's length and collection, the terminals
    /// and units, and per direction alternate 0 (empty) and 1 (an isochronous stream).
    #[test]
    fn descriptors() {
        let mut d = Audio::new();
        let dev = setup(&mut d, [0x80, 6, 0, 1, 0, 0, 18, 0]);
        assert_eq!((dev[4], dev[7]), (0, 64));
        let cfg = setup(&mut d, [0x80, 6, 0, 2, 0, 0, 0xff, 0x0f]);
        assert_eq!(u16::from_le_bytes([cfg[2], cfg[3]]) as usize, cfg.len());
        let mut descs = Vec::new();
        let mut i = 0;
        while i < cfg.len() { assert!(cfg[i] >= 2); descs.push(&cfg[i..i + cfg[i] as usize]); i += cfg[i] as usize; }
        assert_eq!(i, cfg.len());
        let hdr = descs.iter().position(|d| d[1] == 0x24 && d[2] == 1).unwrap();
        let ac_len: usize = descs[hdr..].iter().take_while(|d| d[1] == 0x24).map(|d| d.len()).sum();
        assert_eq!(u16::from_le_bytes([descs[hdr][5], descs[hdr][6]]) as usize, ac_len);
        assert_eq!(&descs[hdr][7..], &[2, 1, 2]);
        let ifaces: Vec<(u8, u8, u8)> = descs.iter().filter(|d| d[1] == 4).map(|d| (d[2], d[3], d[4])).collect();
        assert_eq!(ifaces, [(0, 0, 0), (1, 0, 0), (1, 1, 1), (2, 0, 0), (2, 1, 1)]);
        let eps: Vec<(u8, u8, u16)> = descs.iter().filter(|d| d[1] == 5).map(|d| (d[2], d[3] & 3, u16::from_le_bytes([d[4], d[5]]))).collect();
        assert_eq!(eps, [(0x01, 1, 200), (0x82, 1, 100)]);
        assert!(descs.iter().filter(|d| d[1] == 5).all(|d| d.len() == 9), "audio endpoints are 9 bytes");
        let formats: Vec<&&[u8]> = descs.iter().filter(|d| d[1] == 0x24 && d[2] == 2 && d.len() > 8 && d.len() == 8 + 3 * d[7] as usize).collect();
        assert_eq!(formats.iter().map(|f| (f[4], f[6], f[7])).collect::<Vec<_>>(), [(2, 16, 3), (1, 16, 3)]);
        assert_eq!(&formats[0][8..11], &[0x80, 0xbb, 0x00]); // 48000
        let product = setup(&mut d, [0x80, 6, 2, 3, 9, 4, 0xff, 0]);
        assert_eq!(product[2..].chunks(2).map(|c| c[0] as char).collect::<String>(), "USB PnP Audio Device");
    }

    /// Rates and volumes as snd-usb-audio sets and reads them; samples both ways.
    #[test]
    fn controls_and_streams() {
        let mut d = Audio::new();
        setup(&mut d, [0x00, 9, 1, 0, 0, 0, 0, 0]);
        // speaker volume range, set the left channel to -6 dB, mute off
        assert_eq!(setup(&mut d, [0xa1, 0x82, 1, 2, 0, SPK_FU, 2, 0]), (-63i16 * 256).to_le_bytes());
        assert_eq!(setup(&mut d, [0xa1, 0x84, 1, 2, 0, SPK_FU, 2, 0]), 16i16.to_le_bytes());
        assert!(d.setup([0x21, 1, 1, 2, 0, SPK_FU, 2, 0]));
        d.control_out(&(-6i16 * 256).to_le_bytes());
        assert_eq!(setup(&mut d, [0xa1, 0x81, 1, 2, 0, SPK_FU, 2, 0]), (-6i16 * 256).to_le_bytes());
        assert!(!d.setup([0xa1, 0x81, 0, 3, 0, SPK_FU, 1, 0]), "no such control");
        // 44.1 kHz on the speaker endpoint; a rate not listed is ignored
        assert!(d.setup([0x22, 1, 0, 1, SPK_EP, 0, 3, 0]));
        d.control_out(&[0x44, 0xac, 0x00]);
        assert!(d.setup([0x22, 1, 0, 1, MIC_EP, 0, 3, 0]));
        d.control_out(&[0x11, 0x11, 0x00]);
        assert_eq!(setup(&mut d, [0xa2, 0x81, 0, 1, SPK_EP, 0, 3, 0]), [0x44, 0xac, 0]);
        assert_eq!(d.rates(), [44_100, 48_000]);
        // playback: nothing until alternate 1; then left at -6 dB, right untouched
        d.keep_output = true;
        d.data_out(1, &[0, 0x40, 0, 0x40]);
        assert!(d.take_output().1.is_empty());
        setup(&mut d, [0x01, 11, 1, 0, 1, 0, 0, 0]);
        assert_eq!(setup(&mut d, [0x81, 10, 0, 0, 1, 0, 1, 0]), [1]);
        d.data_out(1, &[0, 0x40, 0, 0x40]);
        let (rate, out) = d.take_output();
        assert_eq!(rate, AUDIO_OUT_RATE);
        // resampled from 44.1 kHz, starting from silence: 0, then 0.92 of the way to the frame
        assert_eq!(out.len(), 4);
        assert!(out[0] == 0 && out[1] == 0 && (out[2] as i32 - 7544).abs() < 4 && (out[3] as i32 - 15054).abs() < 4, "{out:?}");
        let tone: Vec<u8> = (0..441).flat_map(|_| [0u8, 0x10, 0, 0x10]).collect();
        d.data_out(1, &tone);
        assert!((478..=482).contains(&(d.take_output().1.len() / 2)), "441 frames at 44.1 kHz are 480 at 48 kHz");
        assert!(!d.setup([0x01, 11, 2, 0, 1, 0, 0, 0]), "no alternate 2");
        // capture: 48 frames a millisecond of the input, then silence
        setup(&mut d, [0x01, 11, 1, 0, 2, 0, 0, 0]);
        d.set_input(48_000, (0..60).collect());
        let p = d.data_in(2, 100).unwrap();
        assert_eq!(p.len(), 96);
        assert_eq!((i16::from_le_bytes([p[0], p[1]]), i16::from_le_bytes([p[94], p[95]])), (0, 47));
        let p = d.data_in(2, 100).unwrap();
        assert_eq!((i16::from_le_bytes([p[22], p[23]]), i16::from_le_bytes([p[24], p[25]])), (59, 0));
        // at 44.1 kHz the packets carry 44 or 45 frames
        assert!(d.setup([0x22, 1, 0, 1, MIC_EP, 0, 3, 0]));
        d.control_out(&[0x44, 0xac, 0x00]);
        let sizes: usize = (0..10).map(|_| d.data_in(2, 100).unwrap().len() / 2).sum();
        assert_eq!(sizes, 441);
        // a new recording hears the input from its start again
        setup(&mut d, [0x01, 11, 1, 0, 2, 0, 0, 0]);
        let p = d.data_in(2, 100).unwrap();
        assert_eq!((i16::from_le_bytes([p[0], p[1]]), i16::from_le_bytes([p[2], p[3]])), (0, 1));
    }
}
