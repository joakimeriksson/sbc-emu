//! The PC at the other end of the Orin's USB-C device-mode cable: a USB host that enumerates the
//! board's gadget the way a Linux PC does and drives the functions a PC would use.
//!
//! - **Enumeration:** device descriptor, address, configuration descriptor, configuration.
//! - **Network:** the gadget's CDC-NCM function (NVIDIA's `l4t-usb-device-mode` script makes NCM
//!   next to RNDIS; CDC-ECM is driven too, should a gadget offer that instead), as Linux's
//!   `cdc_ncm` / `cdc_ether` host drivers do: the data interface's alternate setting 1 starts
//!   it, Ethernet frames travel in NCM transfer blocks (NTB16). The PC's network stack is
//!   sbc-net's `client::Client`: it takes its address from the board's DHCP server and carries
//!   host TCP port forwards onto the board's 192.168.55.1.
//! - **Serial:** the gadget's CDC-ACM port (the board's `/dev/ttyGS0`), with DTR raised as a
//!   terminal program opening `/dev/ttyACM0` would; its bytes go to and come from the host.
//! - RNDIS and the mass-storage function are left alone (a PC without HoRNDIS, a disk nobody
//!   mounts): their endpoints are never polled.
//!
//! The PC talks to the device controller (`xudc.rs`) in whole transfers: a control transfer
//! (SETUP, data, status), an IN transfer per transfer descriptor the gadget queues, an OUT
//! transfer per message the PC sends. emu-core's USB devices are the same thing from the other
//! side (devices a host drives); nothing of theirs runs in reverse here except the descriptor
//! layouts both follow.
use std::collections::{BTreeMap, VecDeque};

/// The board's address on the device-mode link (`net_ip` in NVIDIA's script).
pub const BOARD_IP: [u8; 4] = [192, 168, 55, 1];
/// Messages queued for one OUT endpoint before the PC drops new ones (a stalled function).
const OUT_LIMIT: usize = 512;
/// After the port reset: how long a host waits before the first request (ns).
pub const RESET_RECOVERY_NS: u64 = 10_000_000;
const NTH16: &[u8; 4] = b"NCMH";
const NDP16: &[u8; 4] = b"NCM0";

/// A control transfer: the SETUP packet and the data of an OUT data stage.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Setup {
    pub packet: [u8; 8],
    pub out: Vec<u8>,
}

impl Setup {
    fn new(request_type: u8, request: u8, value: u16, index: u16, length: u16, out: Vec<u8>) -> Self {
        let mut packet = [request_type, request, 0, 0, 0, 0, 0, 0];
        packet[2..4].copy_from_slice(&value.to_le_bytes());
        packet[4..6].copy_from_slice(&index.to_le_bytes());
        packet[6..8].copy_from_slice(&length.to_le_bytes());
        Setup { packet, out }
    }

    pub fn length(&self) -> usize {
        usize::from(u16::from_le_bytes([self.packet[6], self.packet[7]]))
    }

    pub fn is_in(&self) -> bool {
        self.packet[0] & 0x80 != 0
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Step {
    DeviceDescriptor(u16),
    SetAddress,
    Config(u16),
    SetConfiguration(u8),
    SetInterface { iface: u8, alt: u8 },
    LineCoding(u8),
    LineState(u8),
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum NetKind {
    #[default]
    Ncm,
    Ecm,
}

/// The functions the PC binds: endpoint addresses (bit 7: IN).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct NetFn {
    pub kind: NetKind,
    pub control: u8,
    pub data: u8,
    pub notify: u8,
    pub bulk_in: u8,
    pub bulk_out: u8,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct SerialFn {
    pub control: u8,
    pub notify: u8,
    pub bulk_in: u8,
    pub bulk_out: u8,
}

#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct Functions {
    pub net: Option<NetFn>,
    pub serial: Option<SerialFn>,
    pub names: Vec<&'static str>,
}

pub struct Pc {
    steps: VecDeque<Step>,
    /// the transfer in flight
    current: Option<Step>,
    pub address: u8,
    pub ep0_max: u16,
    pub vendor_product: (u16, u16),
    pub functions: Functions,
    pub configured: bool,
    /// the port was reset at this time; requests start after the recovery time
    reset_at: Option<u64>,
    out: BTreeMap<u8, VecDeque<Vec<u8>>>,
    ntb_sequence: u16,
    pub net: sbc_net::client::Client,
    net_up: bool,
    /// the serial port: bytes the board wrote, not yet taken
    pub serial_rx: Vec<u8>,
    pub log: bool,
    /// frames the PC dropped (a full OUT queue)
    pub dropped: u64,
}

fn word(b: &[u8], at: usize) -> u16 {
    u16::from_le_bytes([b[at], b[at + 1]])
}

impl Pc {
    pub fn new(log: bool) -> Self {
        Pc {
            steps: VecDeque::new(),
            current: None,
            address: 0,
            ep0_max: 64,
            vendor_product: (0, 0),
            functions: Functions::default(),
            configured: false,
            reset_at: None,
            out: BTreeMap::new(),
            ntb_sequence: 0,
            net: sbc_net::client::Client::new(BOARD_IP, log),
            net_up: false,
            serial_rx: Vec::new(),
            log,
            dropped: 0,
        }
    }

    /// The device came up after a port reset at `now`: enumerate it from the start.
    pub fn port_reset(&mut self, now: u64) {
        self.steps = [Step::DeviceDescriptor(64), Step::SetAddress, Step::DeviceDescriptor(18), Step::Config(9)].into();
        self.current = None;
        self.address = 0;
        self.configured = false;
        self.functions = Functions::default();
        self.out.clear();
        self.reset_at = Some(now);
        self.net.link_down();
        self.net_up = false;
    }

    /// The cable is out (or the board pulled its pull-up).
    pub fn disconnect(&mut self) {
        self.steps.clear();
        self.current = None;
        self.reset_at = None;
        self.configured = false;
        self.out.clear();
        self.net.link_down();
        self.net_up = false;
    }

    /// The next control transfer, when the previous one finished and the port has recovered.
    pub fn next_setup(&mut self, now: u64) -> Option<Setup> {
        if self.current.is_some() || now < self.reset_at? + RESET_RECOVERY_NS {
            return None;
        }
        let step = self.steps.pop_front()?;
        self.current = Some(step);
        Some(match step {
            Step::DeviceDescriptor(len) => Setup::new(0x80, 6, 0x0100, 0, len, Vec::new()),
            Step::SetAddress => Setup::new(0x00, 5, 1, 0, 0, Vec::new()),
            Step::Config(len) => Setup::new(0x80, 6, 0x0200, 0, len, Vec::new()),
            Step::SetConfiguration(value) => Setup::new(0x00, 9, u16::from(value), 0, 0, Vec::new()),
            Step::SetInterface { iface, alt } => Setup::new(0x01, 11, u16::from(alt), u16::from(iface), 0, Vec::new()),
            // 115200 8N1, then DTR and RTS
            Step::LineCoding(iface) => Setup::new(0x21, 0x20, 0, u16::from(iface), 7, vec![0x00, 0xc2, 0x01, 0x00, 0, 0, 8]),
            Step::LineState(iface) => Setup::new(0x21, 0x22, 3, u16::from(iface), 0, Vec::new()),
        })
    }

    /// The control transfer finished: `Some(data)` (the IN data stage's bytes), `None` a STALL.
    pub fn control_done(&mut self, result: Option<Vec<u8>>, now: u64) {
        let Some(step) = self.current.take() else { return };
        let Some(data) = result else {
            match step {
                // a function's optional requests: a stall leaves it as it is
                Step::LineCoding(_) | Step::LineState(_) => {}
                _ => {
                    eprintln!("[usb-device] the board stalled {step:?}; enumeration stops");
                    self.steps.clear();
                }
            }
            return;
        };
        match step {
            Step::DeviceDescriptor(_) if data.len() >= 8 => {
                self.ep0_max = u16::from(data[7]).max(8);
                if data.len() >= 12 {
                    self.vendor_product = (word(&data, 8), word(&data, 10));
                }
            }
            Step::SetAddress => self.address = 1,
            Step::Config(9) if data.len() >= 4 => {
                let total = word(&data, 2);
                self.steps.push_front(Step::Config(total.max(9)));
            }
            Step::Config(_) => {
                let value = data.get(5).copied().unwrap_or(1);
                self.functions = parse_config(&data);
                self.steps.push_back(Step::SetConfiguration(value));
                if let Some(net) = self.functions.net {
                    self.steps.push_back(Step::SetInterface { iface: net.data, alt: 1 });
                }
                if let Some(serial) = self.functions.serial {
                    self.steps.push_back(Step::LineCoding(serial.control));
                    self.steps.push_back(Step::LineState(serial.control));
                }
            }
            Step::SetConfiguration(_) => {
                self.configured = true;
                let (v, p) = self.vendor_product;
                eprintln!(
                    "[usb-device] the PC enumerated {v:04x}:{p:04x} ({}); it drives {}",
                    self.functions.names.join("+"),
                    match (self.functions.net, self.functions.serial) {
                        (Some(n), Some(_)) => format!("{:?} and ACM", n.kind),
                        (Some(n), None) => format!("{:?}", n.kind),
                        (None, Some(_)) => "ACM".into(),
                        (None, None) => "nothing".into(),
                    }
                );
            }
            Step::SetInterface { .. } if !self.net_up => {
                self.net_up = true;
                let frames = self.net.link_up(now / 1000);
                self.send_frames(frames);
            }
            _ => {}
        }
    }

    pub fn enumerating(&self) -> bool {
        self.current.is_some() || !self.steps.is_empty()
    }

    /// The PC polls this IN endpoint (a bound function's).
    pub fn wants_in(&self, ep: u8) -> bool {
        if !self.configured {
            return false;
        }
        let net = self.functions.net.filter(|_| self.net_up).is_some_and(|n| ep == n.bulk_in || ep == n.notify);
        let serial = self.functions.serial.is_some_and(|s| ep == s.bulk_in || ep == s.notify);
        net || serial
    }

    /// An IN transfer's bytes.
    pub fn in_data(&mut self, ep: u8, data: &[u8], now: u64) {
        if data.is_empty() {
            return;
        }
        if let Some(net) = self.functions.net.filter(|n| n.bulk_in == ep) {
            let frames = match net.kind {
                NetKind::Ncm => ntb_frames(data),
                NetKind::Ecm => vec![data.to_vec()],
            };
            let mut replies = Vec::new();
            for frame in frames {
                replies.extend(self.net.handle(&frame, now / 1000));
            }
            self.send_frames(replies);
        } else if self.functions.serial.is_some_and(|s| s.bulk_in == ep) {
            self.serial_rx.extend_from_slice(data);
        }
        // notifications (NCM's connection and speed, ACM's serial state) need no answer
    }

    /// The network's timers; frames it sends go out at once.
    pub fn tick(&mut self, now: u64) {
        if self.net_up {
            let frames = self.net.poll(now / 1000);
            self.send_frames(frames);
        }
    }

    /// The PC's next timed work (ns): reset recovery, the network's polling.
    pub fn next_event(&self, now: u64) -> Option<u64> {
        let recovery = self.reset_at.map(|at| at + RESET_RECOVERY_NS).filter(|&t| t > now && self.current.is_none() && !self.steps.is_empty());
        let net = self.net_up.then_some(now + 1_000_000);
        recovery.into_iter().chain(net).min()
    }

    fn send_frames(&mut self, frames: Vec<Vec<u8>>) {
        let Some(net) = self.functions.net.filter(|_| self.net_up) else { return };
        for frame in frames {
            let message = match net.kind {
                NetKind::Ncm => {
                    self.ntb_sequence = self.ntb_sequence.wrapping_add(1);
                    ntb(&frame, self.ntb_sequence)
                }
                NetKind::Ecm => frame,
            };
            self.queue_out(net.bulk_out, message);
        }
    }

    fn queue_out(&mut self, ep: u8, message: Vec<u8>) {
        let queue = self.out.entry(ep).or_default();
        if queue.len() >= OUT_LIMIT {
            self.dropped += 1;
        } else {
            queue.push_back(message);
        }
    }

    /// Bytes for the board's serial port (kept while the port is not bound yet: a terminal
    /// types before the board listens, too, and the board drops nothing it was not given).
    pub fn serial_write(&mut self, bytes: &[u8]) -> bool {
        let Some(serial) = self.functions.serial.filter(|_| self.configured) else { return false };
        for chunk in bytes.chunks(4096) {
            self.queue_out(serial.bulk_out, chunk.to_vec());
        }
        true
    }

    /// The message at the head of an OUT endpoint's queue.
    pub fn out_front(&mut self, ep: u8) -> Option<&mut Vec<u8>> {
        self.out.get_mut(&ep)?.front_mut()
    }

    pub fn out_pop(&mut self, ep: u8) {
        if let Some(q) = self.out.get_mut(&ep) {
            q.pop_front();
        }
    }

    pub fn has_out(&self) -> bool {
        self.out.values().any(|q| !q.is_empty())
    }
}

/// The functions of a configuration descriptor the PC drives (the first NCM or ECM, the first
/// ACM), and the names of all the functions it has.
pub fn parse_config(config: &[u8]) -> Functions {
    #[derive(Default, Clone, Copy)]
    struct Iface {
        number: u8,
        alt: u8,
        class: (u8, u8, u8),
    }
    let mut functions = Functions::default();
    let mut iface = Iface::default();
    let mut unions: Vec<(u8, u8)> = Vec::new(); // (control, data)
    let mut endpoints: Vec<(Iface, u8, u8)> = Vec::new(); // interface, address, attributes
    let mut controls: Vec<Iface> = Vec::new();
    let mut i = 0;
    while i + 2 <= config.len() {
        let len = usize::from(config[i]);
        if len < 2 || i + len > config.len() {
            break;
        }
        let d = &config[i..i + len];
        match d[1] {
            4 if len >= 9 => {
                iface = Iface { number: d[2], alt: d[3], class: (d[5], d[6], d[7]) };
                if d[5] == 2 && d[3] == 0 {
                    controls.push(iface);
                }
                if d[3] == 0 {
                    functions.names.push(match iface.class {
                        (2, 0x0d, _) => "NCM",
                        (2, 0x06, _) => "ECM",
                        (2, 2, 0xff) | (0xe0, 1, 3) | (0xef, 4, 1) => "RNDIS",
                        (2, 2, _) => "ACM",
                        (8, _, _) => "UMS",
                        (0x0a, _, _) => "", // a data interface belongs to the function before it
                        _ => "other",
                    });
                }
            }
            5 if len >= 7 => endpoints.push((iface, d[2], d[3])),
            0x24 if len >= 5 && d[2] == 6 => unions.push((d[3], d[4])),
            _ => {}
        }
        i += len;
    }
    functions.names.retain(|n| !n.is_empty());
    let endpoint = |number: u8, alt: Option<u8>, input: bool, kind: u8| {
        endpoints
            .iter()
            .find(|(f, a, attr)| f.number == number && alt.is_none_or(|alt| f.alt == alt) && (a & 0x80 != 0) == input && attr & 3 == kind)
            .map(|(_, a, _)| *a)
    };
    for control in controls {
        let Some(&(_, data)) = unions.iter().find(|(c, _)| *c == control.number) else { continue };
        match control.class {
            (2, sub @ (0x0d | 0x06), _) if functions.net.is_none() => {
                let (Some(bulk_in), Some(bulk_out)) = (endpoint(data, Some(1), true, 2), endpoint(data, Some(1), false, 2)) else { continue };
                functions.net = Some(NetFn {
                    kind: if sub == 0x0d { NetKind::Ncm } else { NetKind::Ecm },
                    control: control.number,
                    data,
                    notify: endpoint(control.number, None, true, 3).unwrap_or(0),
                    bulk_in,
                    bulk_out,
                });
            }
            (2, 2, 0..=6) if functions.serial.is_none() => {
                let (Some(bulk_in), Some(bulk_out)) = (endpoint(data, None, true, 2), endpoint(data, None, false, 2)) else { continue };
                functions.serial = Some(SerialFn {
                    control: control.number,
                    notify: endpoint(control.number, None, true, 3).unwrap_or(0),
                    bulk_in,
                    bulk_out,
                });
            }
            _ => {}
        }
    }
    functions
}

/// One Ethernet frame in an NCM transfer block (NTB16, one datagram), as Linux's `cdc_ncm`
/// sends it: the header, the datagram pointer table, the frame.
pub fn ntb(frame: &[u8], sequence: u16) -> Vec<u8> {
    const NDP_AT: usize = 12;
    const DATA_AT: usize = NDP_AT + 16;
    let total = DATA_AT + frame.len();
    let mut b = Vec::with_capacity(total);
    b.extend_from_slice(NTH16);
    b.extend_from_slice(&12u16.to_le_bytes());
    b.extend_from_slice(&sequence.to_le_bytes());
    b.extend_from_slice(&(total as u16).to_le_bytes());
    b.extend_from_slice(&(NDP_AT as u16).to_le_bytes());
    b.extend_from_slice(NDP16);
    b.extend_from_slice(&16u16.to_le_bytes());
    b.extend_from_slice(&0u16.to_le_bytes());
    b.extend_from_slice(&(DATA_AT as u16).to_le_bytes());
    b.extend_from_slice(&(frame.len() as u16).to_le_bytes());
    b.extend_from_slice(&[0; 4]);
    b.extend_from_slice(frame);
    b
}

/// The Ethernet frames of an NCM transfer block (NTB16; NDPs with or without CRCs).
pub fn ntb_frames(block: &[u8]) -> Vec<Vec<u8>> {
    let mut frames = Vec::new();
    if block.len() < 12 || &block[0..4] != NTH16 {
        return frames;
    }
    let length = usize::from(word(block, 8)).min(block.len());
    let mut ndp = usize::from(word(block, 10));
    for _ in 0..32 {
        if ndp == 0 || ndp + 8 > length || &block[ndp..ndp + 3] != b"NCM" {
            break;
        }
        let ndp_len = usize::from(word(block, ndp + 4)).min(length - ndp);
        let mut entry = ndp + 8;
        while entry + 4 <= ndp + ndp_len {
            let (at, len) = (usize::from(word(block, entry)), usize::from(word(block, entry + 2)));
            if at == 0 || len == 0 {
                break;
            }
            if at + len <= length && len >= 14 {
                frames.push(block[at..at + len].to_vec());
            }
            entry += 4;
        }
        ndp = usize::from(word(block, ndp + 6));
    }
    frames
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    /// The configuration NVIDIA's script makes (RNDIS, ACM, mass storage, NCM), as the Linux
    /// gadget's composite driver describes it at high speed.
    pub fn l4t_config() -> Vec<u8> {
        let mut c = vec![9, 2, 0, 0, 7, 1, 0, 0xe0, 1];
        let iface = |n: u8, alt: u8, eps: u8, class: (u8, u8, u8)| vec![9, 4, n, alt, eps, class.0, class.1, class.2, 0];
        let ep = |addr: u8, attr: u8, size: u16| { let s = size.to_le_bytes(); vec![7, 5, addr, attr, s[0], s[1], 9] };
        let union = |c: u8, d: u8| vec![5, 0x24, 6, c, d];
        // RNDIS 0/1
        c.extend([8, 0x0b, 0, 2, 2, 2, 0xff, 0]);
        c.extend(iface(0, 0, 1, (2, 2, 0xff)));
        c.extend(union(0, 1));
        c.extend(ep(0x81, 3, 8));
        c.extend(iface(1, 0, 2, (0x0a, 0, 0)));
        c.extend(ep(0x82, 2, 512));
        c.extend(ep(0x01, 2, 512));
        // ACM 2/3
        c.extend([8, 0x0b, 2, 2, 2, 2, 1, 0]);
        c.extend(iface(2, 0, 1, (2, 2, 1)));
        c.extend(union(2, 3));
        c.extend(ep(0x83, 3, 10));
        c.extend(iface(3, 0, 2, (0x0a, 0, 0)));
        c.extend(ep(0x84, 2, 512));
        c.extend(ep(0x02, 2, 512));
        // mass storage 4
        c.extend(iface(4, 0, 2, (8, 6, 0x50)));
        c.extend(ep(0x85, 2, 512));
        c.extend(ep(0x03, 2, 512));
        // NCM 5/6
        c.extend([8, 0x0b, 5, 2, 2, 0x0d, 0, 0]);
        c.extend(iface(5, 0, 1, (2, 0x0d, 0)));
        c.extend([5, 0x24, 0, 0x10, 1]);
        c.extend(union(5, 6));
        c.extend(ep(0x86, 3, 16));
        c.extend(iface(6, 0, 0, (0x0a, 0, 1)));
        c.extend(iface(6, 1, 2, (0x0a, 0, 1)));
        c.extend(ep(0x87, 2, 512));
        c.extend(ep(0x04, 2, 512));
        let total = c.len() as u16;
        c[2..4].copy_from_slice(&total.to_le_bytes());
        c
    }

    #[test]
    fn finds_ncm_and_acm_in_nvidias_configuration() {
        let f = parse_config(&l4t_config());
        assert_eq!(f.names, ["RNDIS", "ACM", "UMS", "NCM"]);
        assert_eq!(f.net, Some(NetFn { kind: NetKind::Ncm, control: 5, data: 6, notify: 0x86, bulk_in: 0x87, bulk_out: 0x04 }));
        assert_eq!(f.serial, Some(SerialFn { control: 2, notify: 0x83, bulk_in: 0x84, bulk_out: 0x02 }));
    }

    #[test]
    fn ntb16_round_trip() {
        let frame: Vec<u8> = (0..60).collect();
        let block = ntb(&frame, 7);
        assert_eq!(&block[0..4], b"NCMH");
        assert_eq!(word(&block, 8) as usize, block.len());
        assert_eq!(ntb_frames(&block), vec![frame.clone()]);
        // two datagrams, the second NDP chained, as the gadget's aggregation may send
        let mut two = block.clone();
        two.extend_from_slice(&frame);
        let second = (two.len() - 60) as u16;
        two[12 + 12..12 + 16].copy_from_slice(&[second as u8, (second >> 8) as u8, 60, 0]);
        two.extend_from_slice(&[0; 4]);
        let total = two.len() as u16;
        two[8..10].copy_from_slice(&total.to_le_bytes());
        assert_eq!(ntb_frames(&two).len(), 2);
    }

    #[test]
    fn enumerates_then_binds_ncm_and_acm() {
        let mut pc = Pc::new(false);
        pc.port_reset(0);
        assert_eq!(pc.next_setup(0), None, "waits for the reset recovery");
        let mut device = vec![18, 1, 0, 2, 0xef, 2, 1, 64, 0x55, 0x09, 0x20, 0x70, 2, 0, 1, 2, 3, 1];
        let config = l4t_config();
        let mut requests = Vec::new();
        let now = RESET_RECOVERY_NS;
        while let Some(setup) = pc.next_setup(now) {
            requests.push(setup.packet[1]);
            let reply = match (setup.packet[1], setup.packet[3]) {
                (6, 1) => { device.truncate(setup.length()); device.clone() }
                (6, 2) => config[..setup.length().min(config.len())].to_vec(),
                _ => Vec::new(),
            };
            pc.control_done(Some(reply), now);
        }
        assert_eq!(requests, [6, 5, 6, 6, 6, 9, 11, 0x20, 0x22]);
        assert!(pc.configured);
        assert_eq!(pc.vendor_product, (0x0955, 0x7020));
        assert!(pc.wants_in(0x87) && pc.wants_in(0x84) && !pc.wants_in(0x85) && !pc.wants_in(0x82));
        // the link came up: a DHCP DISCOVER in an NTB on NCM's bulk OUT
        let block = pc.out_front(0x04).expect("DISCOVER").clone();
        let frames = ntb_frames(&block);
        assert_eq!(frames.len(), 1);
        assert_eq!(&frames[0][12..14], &[8, 0]);
        assert!(pc.serial_write(b"ls\n"));
        assert_eq!(pc.out_front(0x02).map(|m| m.clone()), Some(b"ls\n".to_vec()));
        pc.in_data(0x84, b"login: ", now);
        assert_eq!(pc.serial_rx, b"login: ");
    }
}
