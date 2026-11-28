//! The dongle side of the BCM43455 in place of Broadcom's firmware: SDPCM framing to and
//! from the host, BCDC control messages (ioctls and iovars, as brcmfmac's `fwil` sends
//! them), firmware events, and the data channel.
//!
//! It is a FullMAC station with no radio: scans find the virtual access points
//! ([`Network`]), joining one links the interface at once, and data frames go to and from
//! the network behind it as Ethernet. For a WPA2-PSK network the access point's half of the
//! four-way handshake runs here (the host's supplicant runs the other half, as with the
//! real firmware when brcmfmac does not use its firmware supplicant). The protocol
//! follows the Raspberry Pi 4 model of the same chip (`bcm2711/src/wifi.rs`).
//!
//! The same dongle serves the PCIe chips (`bcm4350`): there the bus protocol is msgbuf, and
//! the ioctls, events and data frames reach it through [`Dongle::ioctl_request`],
//! [`Dongle::host_data`] and [`Dongle::take_messages`] instead of SDPCM frames. It can also
//! be a soft access point (`WLC_SET_AP`, as NetworkManager's hotspot asks): the BSS comes up
//! with no stations, since nothing else is on the air.

use sbc_net::crypto;
use std::collections::{HashMap, VecDeque};

/// A virtual access point the dongle can see and join.
#[derive(Clone, Debug)]
pub struct Network {
    pub ssid: String,
    pub bssid: [u8; 6],
    /// WPA2-PSK passphrase; None for an open network.
    pub psk: Option<String>,
}

impl Network {
    /// `sbc-emu` (open) and `sbc-emu-wpa2` (WPA2-PSK, passphrase `sbc-emu-pass`).
    pub fn defaults() -> Vec<Network> {
        vec![
            Network { ssid: "sbc-emu".into(), bssid: [0x02, 0x5b, 0xc0, 0xa9, 0x00, 0x01], psk: None },
            Network { ssid: "sbc-emu-wpa2".into(), bssid: [0x02, 0x5b, 0xc0, 0xa9, 0x00, 0x02], psk: Some("sbc-emu-pass".into()) },
        ]
    }
}

/// The access points' channel (2.4 GHz).
const CHANNEL: u8 = 6;
/// RSN IE: WPA2, CCMP group and pairwise, PSK authentication.
const RSN_IE: [u8; 22] = [48, 20, 1, 0, 0x00, 0x0f, 0xac, 4, 1, 0, 0x00, 0x0f, 0xac, 4, 1, 0, 0x00, 0x0f, 0xac, 2, 0, 0];
const RSSI: i16 = -45;

// SDPCM channels
const CONTROL: u8 = 0;
const EVENT: u8 = 1;
const DATA: u8 = 2;
const SDPCM_HDRLEN: usize = 12;
/// Frames the host may send beyond the last one seen (its credit window).
const CREDITS: u8 = 8;

// BCDC
const BCDC_DCMD_ERROR: u32 = 0x01;
const BCDC_DCMD_SET: u32 = 0x02;
/// BDC data header: protocol version 2, interface 0.
const BDC_HEADER: [u8; 4] = [2 << 4, 0, 0, 0];

// Ioctls
const C_GET_VERSION: u32 = 1;
const C_UP: u32 = 2;
const C_DOWN: u32 = 3;
const C_GET_INFRA: u32 = 19;
const C_GET_BSSID: u32 = 23;
const C_GET_SSID: u32 = 25;
const C_SET_SSID: u32 = 26;
const C_GET_CHANNEL: u32 = 29;
const C_GET_RADIO: u32 = 37;
const C_GET_PHYTYPE: u32 = 39;
const C_DISASSOC: u32 = 52;
const C_GET_REVINFO: u32 = 98;
const C_GET_RSSI: u32 = 127;
const C_GET_BSS_INFO: u32 = 136;
const C_GET_BANDLIST: u32 = 140;
const C_GET_AP: u32 = 117;
const C_SET_AP: u32 = 118;
const C_GET_PHYLIST: u32 = 180;
const C_GET_VALID_CHANNELS: u32 = 217;
const C_GET_VAR: u32 = 262;
const C_SET_VAR: u32 = 263;

// Events and their status codes
const E_SET_SSID: u32 = 0;
const E_LINK: u32 = 16;
const E_ESCAN_RESULT: u32 = 69;
const E_STATUS_SUCCESS: u32 = 0;
const E_STATUS_NO_NETWORKS: u32 = 3;
const E_STATUS_PARTIAL: u32 = 8;
const EVENT_MSG_LINK: u16 = 0x01;
/// `brcmf_bss_info_le` and escan result version.
const BSS_INFO_VERSION: u32 = 109;

// Firmware error codes (BCME_*)
const BCME_UNSUPPORTED: i32 = -23;
const BCME_NOTUP: i32 = -4;
const BCME_NOTASSOCIATED: i32 = -17;

/// The 2.4 GHz channels offered (20 MHz, d11ac chanspecs).
const CHANNELS: std::ops::RangeInclusive<u32> = 1..=13;
const CHSPEC_BW_20: u32 = 0x1000;
const CHSPEC_BW_MASK: u32 = 0x3800;

/// The access point's side of the WPA2 four-way handshake.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Wpa {
    Open,
    AwaitM2,
    AwaitM4,
    Installed,
}

/// What the dongle reports as itself in `WLC_GET_REVINFO` and `ver`.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Identity {
    /// PCI device ID (the SDIO parts report the PCI ID of their family)
    pub device: u32,
    pub chip: u32,
    pub chiprev: u32,
    /// 1 SDIO, 2 PCIe
    pub bus: u32,
    pub ver: &'static str,
}

impl Identity {
    pub const BCM43455: Identity = Identity {
        device: 0x43ab,
        chip: 0x4345,
        chiprev: 6,
        bus: 1,
        ver: "wl0: Nov 18 2026 00:00:00 version 7.45.241 (sbc-emu dongle) FWID 01-00000000\n",
    };
}

/// A message for the host (msgbuf): a firmware event or a data frame, both Ethernet frames.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum ToHost {
    Event(Vec<u8>),
    Data(Vec<u8>),
}

pub(crate) struct Dongle {
    mac: [u8; 6],
    id: Identity,
    log: bool,
    /// Sequence number of our next frame to the host.
    seq: u8,
    /// The host's next sequence number (one past the last frame it sent).
    host_next: u8,
    out: VecDeque<(u8, Vec<u8>)>,
    /// Ethernet frames for the network.
    pub to_air: Vec<Vec<u8>>,
    /// Iovars the host set, returned when it gets them back.
    vars: HashMap<String, Vec<u8>>,
    up: bool,
    networks: Vec<Network>,
    /// The network joined (an index into `networks`).
    joined: Option<usize>,
    wpa: Wpa,
    pmk: [u8; 32],
    anonce: [u8; 32],
    replay: u64,
    /// soft access point mode (`WLC_SET_AP`) and the SSID of its BSS once started
    ap: bool,
    ap_ssid: Option<Vec<u8>>,
}

impl Dongle {
    pub fn new(mac: [u8; 6], networks: Vec<Network>, log: bool) -> Self {
        Self {
            mac,
            log,
            seq: 0,
            host_next: 0xff,
            out: VecDeque::new(),
            to_air: Vec::new(),
            vars: HashMap::new(),
            up: false,
            networks,
            joined: None,
            wpa: Wpa::Open,
            pmk: [0; 32],
            anonce: [0; 32],
            replay: 0,
            ap: false,
            ap_ssid: None,
            id: Identity::BCM43455,
        }
    }

    pub fn with_identity(mut self, id: Identity) -> Self {
        self.id = id;
        self
    }

    /// The SSID of the soft access point's BSS, when one is up.
    pub fn ap_ssid(&self) -> Option<&[u8]> {
        self.ap_ssid.as_deref()
    }

    /// A msgbuf ioctl (`MSGBUF_TYPE_IOCTLPTR_REQ`): msgbuf has no set flag, so the command
    /// number says which way it goes. Returns the BCME status and the answer (`outlen`
    /// bytes at most); the events it raises are queued after it.
    pub fn ioctl_request(&mut self, cmd: u32, data: &[u8], outlen: usize) -> (i32, Vec<u8>) {
        let set = is_set(cmd);
        let mut buf = data.to_vec();
        buf.resize(outlen.max(data.len()), 0);
        let mut events = Vec::new();
        let status = self.ioctl(cmd, set, &mut buf, &mut events);
        if self.log {
            let name = if cmd == C_GET_VAR || cmd == C_SET_VAR { iovar_name(data) } else { String::new() };
            eprintln!("[brcm] ioctl {cmd} {} {name} len {outlen} -> {status}", if set { "set" } else { "get" });
        }
        buf.truncate(if set { 0 } else { outlen });
        self.run(events);
        (status, buf)
    }

    /// An Ethernet frame the host transmits (msgbuf `TX_POST`).
    pub fn host_data(&mut self, eth: &[u8]) {
        self.data(eth);
    }

    /// The events and data frames queued for the host, without their BDC headers.
    pub fn take_messages(&mut self) -> Vec<ToHost> {
        let mut v = Vec::new();
        while let Some((channel, mut payload)) = self.out.pop_front() {
            if payload.len() <= BDC_HEADER.len() {
                continue; // an empty credit frame (SDPCM only)
            }
            payload.drain(..BDC_HEADER.len());
            match channel {
                EVENT => v.push(ToHost::Event(payload)),
                DATA => v.push(ToHost::Data(payload)),
                _ => {}
            }
        }
        v
    }

    fn run(&mut self, events: Vec<Action>) {
        for action in events {
            match action {
                Action::Scan(sync) => self.scan_results(sync),
                Action::Join(ssid) => self.join(&ssid),
                Action::Leave => self.leave(),
                Action::StartAp(ssid) => self.start_ap(ssid),
            }
        }
    }

    /// The SSID of the network joined.
    pub fn joined(&self) -> Option<&str> {
        self.joined.map(|i| self.networks[i].ssid.as_str())
    }

    /// One SDPCM frame from the host (hardware and software header included).
    pub fn host_frame(&mut self, f: &[u8]) {
        if f.len() < SDPCM_HDRLEN {
            return;
        }
        self.host_next = f[4].wrapping_add(1);
        let channel = f[5] & 0x0f;
        let doff = (f[7] as usize).clamp(SDPCM_HDRLEN, f.len());
        let payload = &f[doff..];
        match channel {
            CONTROL => self.control(payload),
            // BCDC header: flags, priority, flags2, data offset in words
            DATA if payload.len() >= 4 => {
                if let Some(eth) = payload.get(4 + 4 * payload[3] as usize..) {
                    self.data(eth);
                }
            }
            _ => {}
        }
        // every frame gets credit back: with nothing else to send, an empty event frame
        // (a BDC header only, which the host drops) carries the new window
        if self.out.is_empty() && channel != CONTROL {
            self.out.push_back((EVENT, BDC_HEADER.to_vec()));
        }
    }

    /// An Ethernet frame from the host: EAPOL for the handshake, the rest for the network
    /// once the link may carry data.
    fn data(&mut self, eth: &[u8]) {
        if eth.len() >= 14 && eth[12..14] == [0x88, 0x8e] {
            self.on_eapol(&eth[14..]);
        } else if self.data_ok() {
            self.to_air.push(eth.to_vec());
        }
    }

    /// Data may flow: joined, and either open or the handshake has installed the keys.
    fn data_ok(&self) -> bool {
        self.joined.is_some() && matches!(self.wpa, Wpa::Open | Wpa::AwaitM4 | Wpa::Installed)
    }

    /// A BCDC control message: run it and queue the answer.
    fn control(&mut self, p: &[u8]) {
        if p.len() < 16 {
            return;
        }
        let word = |i: usize| u32::from_le_bytes(p[i..i + 4].try_into().unwrap());
        let (cmd, len, flags) = (word(0), word(4), word(8));
        let outlen = (len & 0xffff) as usize;
        let data = &p[16..];
        let set = flags & BCDC_DCMD_SET != 0;
        let mut buf = data.to_vec();
        buf.resize(outlen.max(data.len()), 0);
        // events the command raises go after its answer
        let mut events = Vec::new();
        let status = self.ioctl(cmd, set, &mut buf, &mut events);
        if self.log {
            let name = if cmd == C_GET_VAR || cmd == C_SET_VAR { iovar_name(data) } else { String::new() };
            eprintln!("[brcm] ioctl {cmd} {} {name} len {outlen} -> {status}", if set { "set" } else { "get" });
        }
        buf.truncate(outlen);
        let mut msg = Vec::with_capacity(16 + buf.len());
        msg.extend_from_slice(&cmd.to_le_bytes());
        msg.extend_from_slice(&(buf.len() as u32).to_le_bytes());
        let flags = if status != 0 { flags | BCDC_DCMD_ERROR } else { flags & !BCDC_DCMD_ERROR };
        msg.extend_from_slice(&flags.to_le_bytes());
        msg.extend_from_slice(&(status as u32).to_le_bytes());
        msg.extend_from_slice(&buf);
        self.out.push_back((CONTROL, msg));
        self.run(events);
    }

    /// An ioctl: `buf` holds the input and receives the output. Returns a BCME status.
    fn ioctl(&mut self, cmd: u32, set: bool, buf: &mut [u8], then: &mut Vec<Action>) -> i32 {
        let put = |buf: &mut [u8], v: &[u8]| {
            let n = v.len().min(buf.len());
            buf[..n].copy_from_slice(&v[..n]);
            buf[n..].fill(0);
        };
        let word = |v: u32| v.to_le_bytes();
        match cmd {
            C_GET_VAR => {
                let name = iovar_name(buf);
                match self.get_var(&name, buf) {
                    Some(v) => {
                        put(buf, &v);
                        0
                    }
                    None => BCME_UNSUPPORTED,
                }
            }
            C_SET_VAR => {
                let name = iovar_name(buf);
                let value = buf.get(name.len() + 1..).unwrap_or(&[]).to_vec();
                self.set_var(&name, value, then)
            }
            C_UP => {
                self.up = true;
                0
            }
            C_DOWN => {
                self.up = false;
                self.ap_ssid = None;
                then.push(Action::Leave);
                0
            }
            // join: brcmf_ssid_le { u32 len; u8 ssid[32] }, possibly with parameters after;
            // in AP mode it starts the soft AP's BSS (an empty SSID stops it)
            C_SET_SSID if self.ap => {
                then.push(Action::StartAp(ssid_of(buf)));
                0
            }
            C_SET_SSID => {
                then.push(Action::Join(ssid_of(buf)));
                0
            }
            C_SET_AP => {
                let on = buf.get(..4).is_some_and(|b| b != [0; 4]);
                if !on {
                    self.ap_ssid = None;
                } else {
                    then.push(Action::Leave);
                }
                self.ap = on;
                0
            }
            C_GET_AP => {
                put(buf, &word(u32::from(self.ap)));
                0
            }
            C_DISASSOC => {
                then.push(Action::Leave);
                0
            }
            _ if set => 0,
            // io type 2: d11ac chanspecs
            C_GET_VERSION => {
                put(buf, &word(2));
                0
            }
            C_GET_REVINFO => {
                let mut v = Vec::new();
                // vendor, device, radio rev, chip rev, core rev, board id, board vendor,
                // board rev, driver rev, ucode rev, bus (SDIO), chip, phy type (AC), phy
                // rev, ana rev, chip package, NVRAM rev
                let id = self.id;
                for x in [0x14e4, id.device, 0, id.chiprev, 46, 0, 0x14e4, 0x1101, 0x0700_2d00, 0x4b0, id.bus, id.chip, 11, 1, 0, 2, 0] {
                    v.extend_from_slice(&word(x));
                }
                put(buf, &v);
                0
            }
            C_GET_BANDLIST => {
                // one band: 2.4 GHz (WLC_BAND_2G = 2)
                put(buf, &[word(1), word(2)].concat());
                0
            }
            C_GET_PHYLIST => {
                put(buf, b"v\0");
                0
            }
            C_GET_PHYTYPE => {
                put(buf, &word(11)); // PHY_TYPE_AC
                0
            }
            C_GET_INFRA => {
                put(buf, &word(1));
                0
            }
            C_GET_RADIO => {
                put(buf, &word(0)); // on
                0
            }
            C_GET_CHANNEL => {
                put(buf, &[word(CHANNEL as u32), word(CHANNEL as u32), word(0)].concat());
                0
            }
            C_GET_VALID_CHANNELS => {
                let mut v = word(CHANNELS.count() as u32).to_vec();
                for c in CHANNELS {
                    v.extend_from_slice(&word(c));
                }
                put(buf, &v);
                0
            }
            C_GET_BSSID => match self.joined {
                Some(i) => {
                    put(buf, &self.networks[i].bssid);
                    0
                }
                None => BCME_NOTASSOCIATED,
            },
            C_GET_SSID => {
                let ssid = self.joined().unwrap_or("").as_bytes().to_vec();
                let mut v = word(ssid.len() as u32).to_vec();
                v.extend_from_slice(&ssid);
                put(buf, &v);
                0
            }
            C_GET_RSSI => {
                put(buf, &word(RSSI as i32 as u32));
                0
            }
            C_GET_BSS_INFO => match self.joined {
                // the buffer length, then the record of the network joined
                Some(i) => {
                    let mut v = word(buf.len() as u32).to_vec();
                    v.extend_from_slice(&self.bss_info(i));
                    put(buf, &v);
                    0
                }
                None => BCME_NOTASSOCIATED,
            },
            _ => {
                buf.fill(0);
                0
            }
        }
    }

    /// The value of a get iovar (`buf` holds its name and any parameters).
    fn get_var(&mut self, name: &str, buf: &[u8]) -> Option<Vec<u8>> {
        let word = |v: u32| v.to_le_bytes().to_vec();
        let name = name.strip_prefix("bsscfg:").unwrap_or(name);
        let full = buf.iter().position(|&b| b == 0).unwrap_or(buf.len());
        let params = buf.get(full + 1..).unwrap_or(&[]);
        Some(match name {
            "cur_etheraddr" => self.mac.to_vec(),
            "ver" => [self.id.ver.as_bytes(), b"\0"].concat(),
            "clmver" => b"API: 18.2 Data: sbc-emu Compiler: none\0".to_vec(),
            "cap" => b"ap sta wme 802.11d 802.11h rm cac dualband ampdu ampdu_tx ampdu_rx amsdurx mfp\0".to_vec(),
            "chanspecs" => {
                // the parameter is a chanspec filter: only its bandwidth matters here
                let filter = params.get(..2).map_or(0, |p| u16::from_le_bytes([p[0], p[1]]) as u32);
                if filter & CHSPEC_BW_MASK != 0 && filter & CHSPEC_BW_MASK != CHSPEC_BW_20 {
                    return Some(word(0));
                }
                let mut v = word(CHANNELS.count() as u32);
                for c in CHANNELS {
                    v.extend_from_slice(&word(CHSPEC_BW_20 | c));
                }
                v
            }
            "chanspec" => word(CHSPEC_BW_20 | CHANNEL as u32),
            "bw_cap" => word(1), // 20 MHz
            "nmode" | "rxchain" | "txchain" | "txstreams" | "rxstreams" | "mpc" | "wme" | "ampdu" => word(1),
            "vhtmode" | "ldpc_cap" | "stbc_rx" | "stbc_tx" | "txbf_bfe_cap" | "txbf_bfr_cap" | "per_chan_info" => word(0),
            // transmit power in quarter dBm: 20 dBm
            "qtxpower" => word(80),
            // the ARP offload's host IP table (brcmf_inetaddr_changed reads it back): the
            // addresses set, zero-filled to its eight entries
            "arp_hostip" => {
                let mut v = self.vars.get("arp_hostip").cloned().unwrap_or_default();
                v.resize(8 * 4, 0);
                v
            }
            "dtim_assoc" => word(1),
            // Bluetooth coexistence parameters (brcmf_btcoex reads number 27): none active
            "btc_params" => word(0),
            // the access point's station record, as `brcmf_cfg80211_get_station` reads it
            // (version 4, associated and authorized, with per-station statistics)
            "sta_info" => return self.sta_info(params),
            // the event mask (read before it is changed): none enabled at first
            "event_msgs" => self.vars.get(name).cloned().unwrap_or_default(),
            // no association request/response IEs recorded
            "assoc_info" => vec![0; 12],
            "country" => {
                let mut v = b"XZ\0\0".to_vec();
                v.extend_from_slice(&word(0));
                v.extend_from_slice(b"XZ\0\0");
                v
            }
            "bus:rxglom" | "tlv" | "p2p_disc" | "pfn" | "wowl" | "wowl_cap" | "rsdb_mode" | "scb_probe" | "mchan" | "mbss" | "fwevent_ext" => return None,
            // anything else only as the host set it: a get that succeeds tells brcmfmac
            // the firmware has the feature (`sup_wpa` would hand it the handshake)
            n => return self.vars.get(n).cloned(),
        })
    }

    /// `brcmf_sta_info_le` for the access point joined, when `params` (its MAC address)
    /// names it.
    fn sta_info(&self, params: &[u8]) -> Option<Vec<u8>> {
        let i = self.joined?;
        if params.get(..6) != Some(&self.networks[i].bssid[..]) {
            return None;
        }
        const AUTHE: u32 = 0x08;
        const ASSOC: u32 = 0x10;
        const AUTHO: u32 = 0x20;
        const SCBSTATS: u32 = 0x4000;
        let mut v = vec![0u8; 0x100];
        let put32 = |v: &mut Vec<u8>, o: usize, x: u32| v[o..o + 4].copy_from_slice(&x.to_le_bytes());
        v[0..2].copy_from_slice(&4u16.to_le_bytes());
        v[2..4].copy_from_slice(&0x100u16.to_le_bytes());
        put32(&mut v, 8, AUTHE | ASSOC | AUTHO | SCBSTATS);
        v[16..22].copy_from_slice(&self.networks[i].bssid);
        put32(&mut v, 52, 1); // tx packets
        put32(&mut v, 60, 1); // rx unicast packets
        put32(&mut v, 68, 54_000); // tx and rx rate (kbit/s)
        put32(&mut v, 72, 54_000);
        v[144] = RSSI as i8 as u8; // average RSSI, antenna 0
        v[168] = RSSI as i8 as u8; // last packet's RSSI, antenna 0
        Some(v)
    }

    fn set_var(&mut self, name: &str, value: Vec<u8>, then: &mut Vec<Action>) -> i32 {
        let name = name.strip_prefix("bsscfg:").unwrap_or(name);
        match name {
            // no superframes (glom) on either side
            "bus:rxglom" => BCME_UNSUPPORTED,
            "tlv" | "p2p_disc" | "fwevent_ext" | "mchan" | "mbss" => BCME_UNSUPPORTED,
            "escan" if !self.up => BCME_NOTUP,
            // brcmf_escan_params_le: version, action, sync ID, then the scan parameters
            "escan" => {
                let sync = value.get(6..8).map_or(0, |b| u16::from_le_bytes([b[0], b[1]]));
                then.push(Action::Scan(sync));
                0
            }
            // brcmf_ext_join_params_le: starts with brcmf_ssid_le
            "join" => {
                then.push(Action::Join(ssid_of(&value)));
                0
            }
            "arp_hostip" => {
                let table = self.vars.entry(name.to_string()).or_default();
                if table.len() < 8 * 4 {
                    table.extend_from_slice(&value[..value.len().min(4)]);
                }
                0
            }
            "arp_hostip_clear" => {
                self.vars.remove("arp_hostip");
                0
            }
            _ => {
                self.vars.insert(name.to_string(), value);
                0
            }
        }
    }

    // ------------------------------------------------------------------ events

    fn event_enabled(&self, ev: u32) -> bool {
        self.vars
            .get("event_msgs")
            .and_then(|mask| mask.get(ev as usize / 8))
            .is_some_and(|b| b & (1 << (ev % 8)) != 0)
    }

    /// A firmware event (`brcmf_event`: Ethernet header, Broadcom header, event message).
    fn event(&mut self, ev: u32, status: u32, flags: u16, data: &[u8]) {
        if !self.event_enabled(ev) {
            return;
        }
        let bssid = self.joined.map_or([0; 6], |i| self.networks[i].bssid);
        let mut p = BDC_HEADER.to_vec();
        p.extend_from_slice(&self.mac);
        p.extend_from_slice(&self.mac);
        p.extend_from_slice(&0x886cu16.to_be_bytes()); // ETH_P_LINK_CTL
        p.extend_from_slice(&0x8001u16.to_be_bytes()); // brcm_ethhdr subtype
        p.extend_from_slice(&((10 + 48 + data.len()) as u16).to_be_bytes());
        p.push(0); // version
        p.extend_from_slice(&[0x00, 0x10, 0x18]); // Broadcom OUI
        p.extend_from_slice(&1u16.to_be_bytes()); // user subtype: event
        p.extend_from_slice(&2u16.to_be_bytes()); // event message version
        p.extend_from_slice(&flags.to_be_bytes());
        p.extend_from_slice(&ev.to_be_bytes());
        p.extend_from_slice(&status.to_be_bytes());
        p.extend_from_slice(&0u32.to_be_bytes()); // reason
        p.extend_from_slice(&0u32.to_be_bytes()); // auth type
        p.extend_from_slice(&(data.len() as u32).to_be_bytes());
        p.extend_from_slice(&bssid);
        let mut ifname = [0u8; 16];
        ifname[..5].copy_from_slice(b"wlan0");
        p.extend_from_slice(&ifname);
        p.push(0); // interface index
        p.push(0); // BSS configuration index
        p.extend_from_slice(data);
        self.out.push_back((EVENT, p));
    }

    /// `brcmf_bss_info_le` for network `i`, its IEs after it.
    fn bss_info(&self, i: usize) -> Vec<u8> {
        let net = &self.networks[i];
        let ssid = &net.ssid.as_bytes()[..net.ssid.len().min(32)];
        const RATES: [u8; 8] = [0x82, 0x84, 0x8b, 0x96, 0x0c, 0x12, 0x18, 0x24];
        let mut ies = vec![0u8, ssid.len() as u8];
        ies.extend_from_slice(ssid);
        ies.extend_from_slice(&[1, RATES.len() as u8]);
        ies.extend_from_slice(&RATES);
        ies.extend_from_slice(&[3, 1, CHANNEL]); // DS parameter set
        if net.psk.is_some() {
            ies.extend_from_slice(&RSN_IE);
        }
        let mut b = vec![0u8; 128];
        let put16 = |b: &mut Vec<u8>, o: usize, v: u16| b[o..o + 2].copy_from_slice(&v.to_le_bytes());
        let put32 = |b: &mut Vec<u8>, o: usize, v: u32| b[o..o + 4].copy_from_slice(&v.to_le_bytes());
        put32(&mut b, 0, BSS_INFO_VERSION);
        put32(&mut b, 4, (128 + ies.len()) as u32);
        b[8..14].copy_from_slice(&net.bssid);
        put16(&mut b, 14, 100); // beacon period
        // capabilities: ESS, short slot time, privacy for WPA2
        put16(&mut b, 16, if net.psk.is_some() { 0x0411 } else { 0x0401 });
        b[18] = ssid.len() as u8;
        b[19..19 + ssid.len()].copy_from_slice(ssid);
        put32(&mut b, 52, RATES.len() as u32);
        b[56..64].copy_from_slice(&RATES);
        put16(&mut b, 72, (CHSPEC_BW_20 | CHANNEL as u32) as u16);
        b[76] = 1; // DTIM period
        put16(&mut b, 78, RSSI as u16);
        b[80] = (-92i8) as u8; // noise
        b[88] = CHANNEL; // control channel
        put16(&mut b, 116, 128); // IE offset
        put32(&mut b, 120, ies.len() as u32);
        put16(&mut b, 124, 40); // SNR
        b.extend_from_slice(&ies);
        b
    }

    /// An escan: one partial result per network, then completion.
    fn scan_results(&mut self, sync: u16) {
        let header = |len: usize, count: u16| {
            let mut v = (len as u32).to_le_bytes().to_vec();
            v.extend_from_slice(&BSS_INFO_VERSION.to_le_bytes());
            v.extend_from_slice(&sync.to_le_bytes());
            v.extend_from_slice(&count.to_le_bytes());
            v
        };
        for i in 0..self.networks.len() {
            let bss = self.bss_info(i);
            let mut partial = header(12 + bss.len(), 1);
            partial.extend_from_slice(&bss);
            self.event(E_ESCAN_RESULT, E_STATUS_PARTIAL, 0, &partial);
        }
        self.event(E_ESCAN_RESULT, E_STATUS_SUCCESS, 0, &header(12, 0));
    }

    fn join(&mut self, ssid: &[u8]) {
        let Some(i) = self.networks.iter().position(|n| n.ssid.as_bytes() == ssid) else {
            self.event(E_SET_SSID, E_STATUS_NO_NETWORKS, 0, &[]);
            return;
        };
        self.joined = Some(i);
        if self.log {
            eprintln!("[brcm] joined '{}'", self.networks[i].ssid);
        }
        self.event(E_LINK, E_STATUS_SUCCESS, EVENT_MSG_LINK, &[]);
        self.event(E_SET_SSID, E_STATUS_SUCCESS, 0, &[]);
        match self.networks[i].psk.clone() {
            None => self.wpa = Wpa::Open,
            Some(pass) => {
                // WPA2-PSK: the access point starts the four-way handshake (message 1)
                let pmk = crypto::pbkdf2_sha1(pass.as_bytes(), self.networks[i].ssid.as_bytes(), 4096, 32);
                self.pmk.copy_from_slice(&pmk);
                let seed = crypto::sha1(&[&self.pmk[..], &self.mac[..], &self.replay.to_le_bytes()[..]].concat());
                for (k, b) in self.anonce.iter_mut().enumerate() {
                    *b = seed[k % 20] ^ k as u8;
                }
                self.replay += 1;
                let m1 = self.eapol_key(0x008a, self.anonce, &[], None);
                self.wpa = Wpa::AwaitM2;
                self.send_eapol(m1);
            }
        }
    }

    /// The soft AP's BSS is up (or down for an empty SSID): there is no air, so no station
    /// ever associates; the host sees its own beaconing BSS only.
    fn start_ap(&mut self, ssid: Vec<u8>) {
        if self.log {
            eprintln!("[brcm] soft AP '{}' {}", String::from_utf8_lossy(&ssid), if ssid.is_empty() { "stopped" } else { "started" });
        }
        self.ap_ssid = (!ssid.is_empty()).then_some(ssid);
        if self.ap_ssid.is_some() {
            self.event(E_SET_SSID, E_STATUS_SUCCESS, 0, &[]);
        }
    }

    fn leave(&mut self) {
        if self.joined.is_some() {
            self.event(E_LINK, E_STATUS_SUCCESS, 0, &[]);
            self.joined = None;
            self.wpa = Wpa::Open;
        }
    }

    // ------------------------------------------------------------------ WPA2

    /// An EAPOL-Key frame (802.1X-2004, RSN key descriptor, HMAC-SHA1 MIC).
    fn eapol_key(&self, key_info: u16, nonce: [u8; 32], key_data: &[u8], kck: Option<&[u8]>) -> Vec<u8> {
        let mut b = vec![2u8, 3];
        b.extend_from_slice(&((95 + key_data.len()) as u16).to_be_bytes());
        b.push(2); // RSN key descriptor
        b.extend_from_slice(&key_info.to_be_bytes());
        b.extend_from_slice(&16u16.to_be_bytes()); // key length (CCMP)
        b.extend_from_slice(&self.replay.to_be_bytes());
        b.extend_from_slice(&nonce);
        b.extend_from_slice(&[0u8; 32]); // IV, RSC, reserved
        let mic_at = b.len();
        b.extend_from_slice(&[0u8; 16]);
        b.extend_from_slice(&(key_data.len() as u16).to_be_bytes());
        b.extend_from_slice(key_data);
        if let Some(k) = kck {
            let mic = crypto::hmac_sha1(k, &b);
            b[mic_at..mic_at + 16].copy_from_slice(&mic[..16]);
        }
        b
    }

    fn send_eapol(&mut self, body: Vec<u8>) {
        let Some(i) = self.joined else { return };
        let mut p = BDC_HEADER.to_vec();
        p.extend_from_slice(&self.mac);
        p.extend_from_slice(&self.networks[i].bssid);
        p.extend_from_slice(&0x888eu16.to_be_bytes());
        p.extend_from_slice(&body);
        self.out.push_back((DATA, p));
    }

    /// EAPOL from the station: message 2 (check its MIC, send message 3) and message 4.
    fn on_eapol(&mut self, body: &[u8]) {
        let Some(i) = self.joined else { return };
        if body.len() < 99 || body[1] != 3 {
            return;
        }
        let n = (4 + u16::from_be_bytes([body[2], body[3]]) as usize).min(body.len());
        let body = &body[..n];
        let key_info = u16::from_be_bytes([body[5], body[6]]);
        if key_info & 0x0100 == 0 {
            return; // no MIC
        }
        let secure = key_info & 0x0200 != 0;
        if !secure && self.wpa == Wpa::AwaitM2 {
            let snonce: [u8; 32] = body[17..49].try_into().unwrap();
            let (aa, spa) = (self.networks[i].bssid, self.mac);
            let (lo_mac, hi_mac) = if aa <= spa { (aa, spa) } else { (spa, aa) };
            let (lo_n, hi_n) = if self.anonce <= snonce { (self.anonce, snonce) } else { (snonce, self.anonce) };
            let data = [&lo_mac[..], &hi_mac[..], &lo_n[..], &hi_n[..]].concat();
            let ptk = crypto::prf(&self.pmk, "Pairwise key expansion", &data, 384);
            let mut probe = body.to_vec();
            let mic = probe[81..97].to_vec();
            probe[81..97].fill(0);
            if crypto::hmac_sha1(&ptk[..16], &probe)[..16] != mic[..] {
                // a wrong passphrase: no message 3, the station's handshake times out
                if self.log {
                    eprintln!("[brcm] WPA2 message 2 MIC mismatch: wrong passphrase");
                }
                return;
            }
            let gtk = crypto::sha1(&[&self.pmk[..], b"gtk"].concat())[..16].to_vec();
            let mut kd = RSN_IE.to_vec();
            kd.extend_from_slice(&[0xdd, 22, 0x00, 0x0f, 0xac, 0x01, 0x01, 0x00]); // GTK KDE, key 1
            kd.extend_from_slice(&gtk);
            if !kd.len().is_multiple_of(8) {
                kd.push(0xdd);
                while !kd.len().is_multiple_of(8) {
                    kd.push(0);
                }
            }
            let kek: [u8; 16] = ptk[16..32].try_into().unwrap();
            let wrapped = crypto::aes_key_wrap(&kek, &kd);
            self.replay += 1;
            let m3 = self.eapol_key(0x13ca, self.anonce, &wrapped, Some(&ptk[..16]));
            self.wpa = Wpa::AwaitM4;
            self.send_eapol(m3);
        } else if secure && self.wpa == Wpa::AwaitM4 {
            self.wpa = Wpa::Installed;
            if self.log {
                eprintln!("[brcm] WPA2 four-way handshake complete");
            }
        }
    }

    /// An Ethernet frame from the network.
    pub fn receive(&mut self, eth: &[u8]) {
        if !self.data_ok() {
            return;
        }
        let mut p = BDC_HEADER.to_vec();
        p.extend_from_slice(eth);
        self.out.push_back((DATA, p));
    }

    /// The queued messages as SDPCM frames.
    pub fn take_frames(&mut self) -> Vec<Vec<u8>> {
        let mut frames = Vec::new();
        while let Some((channel, payload)) = self.out.pop_front() {
            let len = SDPCM_HDRLEN + payload.len();
            let mut f = Vec::with_capacity(len);
            f.extend_from_slice(&(len as u16).to_le_bytes());
            f.extend_from_slice(&(!(len as u16)).to_le_bytes());
            let window = self.host_next.wrapping_add(CREDITS);
            f.extend_from_slice(&[self.seq, channel, 0, SDPCM_HDRLEN as u8, 0, window, 0, 0]);
            f.extend_from_slice(&payload);
            self.seq = self.seq.wrapping_add(1);
            frames.push(f);
        }
        frames
    }
}

/// Work an ioctl starts, done after its answer is queued.
enum Action {
    Scan(u16),
    Join(Vec<u8>),
    Leave,
    StartAp(Vec<u8>),
}

/// The ioctls that set something (brcmfmac's `fwil.h`): msgbuf requests carry no direction.
fn is_set(cmd: u32) -> bool {
    matches!(
        cmd,
        C_UP | C_DOWN | 10 | 20 | 22 | C_SET_SSID | 28 | 30 | 32 | 34 | 38 | 45 | 47 | 49 | 50 | C_DISASSOC | 53 | 55 | 57 | 76 | 78 | 84 | 86 | 108 | C_SET_AP | 121 | 122 | 134 | 158 | 185 | 187 | 201 | 205 | 219 | 236 | 258 | C_SET_VAR | 268
    )
}

/// The SSID of a `brcmf_ssid_le` at the start of a buffer.
fn ssid_of(buf: &[u8]) -> Vec<u8> {
    let n = buf.get(..4).map_or(0, |b| u32::from_le_bytes(b.try_into().unwrap()) as usize).min(32);
    buf.get(4..4 + n).unwrap_or(&[]).to_vec()
}

/// The NUL-terminated iovar name at the start of a buffer.
fn iovar_name(buf: &[u8]) -> String {
    let end = buf.iter().position(|&b| b == 0).unwrap_or(buf.len());
    String::from_utf8_lossy(&buf[..end]).into_owned()
}
