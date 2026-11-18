//! The dongle side of the BCM43455 in place of Broadcom's firmware: SDPCM framing to and
//! from the host, BCDC control messages (ioctls and iovars, as brcmfmac's `fwil` sends
//! them), and the data channel.

use std::collections::{HashMap, VecDeque};

/// An access point the dongle can see.
#[derive(Clone, Debug)]
pub struct Station {
    pub ssid: String,
    pub bssid: [u8; 6],
    pub channel: u8,
}

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
const BCDC_PROTO_VER: u8 = 2;

// Ioctls
const C_GET_VERSION: u32 = 1;
const C_UP: u32 = 2;
const C_DOWN: u32 = 3;
const C_GET_INFRA: u32 = 19;
const C_GET_CHANNEL: u32 = 29;
const C_GET_RADIO: u32 = 37;
const C_GET_PHYTYPE: u32 = 39;
const C_GET_REVINFO: u32 = 98;
const C_GET_BANDLIST: u32 = 140;
const C_GET_PHYLIST: u32 = 180;
const C_GET_VALID_CHANNELS: u32 = 217;
const C_GET_VAR: u32 = 262;
const C_SET_VAR: u32 = 263;

// Firmware error codes (BCME_*)
const BCME_UNSUPPORTED: i32 = -23;
const BCME_NOTUP: i32 = -4;

/// The 2.4 GHz channels offered (20 MHz, d11ac chanspecs).
const CHANNELS: std::ops::RangeInclusive<u32> = 1..=13;
const CHSPEC_BW_20: u32 = 0x1000;
const CHSPEC_BW_MASK: u32 = 0x3800;

pub(super) struct Dongle {
    mac: [u8; 6],
    log: bool,
    /// Sequence number of our next frame to the host.
    seq: u8,
    /// The host's next sequence number (one past the last frame it sent).
    host_next: u8,
    out: VecDeque<(u8, Vec<u8>)>,
    /// Ethernet frames for the air.
    pub to_air: Vec<Vec<u8>>,
    /// Iovars the host set, returned when it gets them back.
    vars: HashMap<String, Vec<u8>>,
    up: bool,
}

impl Dongle {
    pub fn new(mac: [u8; 6], log: bool) -> Self {
        Self { mac, log, seq: 0, host_next: 0xff, out: VecDeque::new(), to_air: Vec::new(), vars: HashMap::new(), up: false }
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
                    self.to_air.push(eth.to_vec());
                }
            }
            _ => {}
        }
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
        let status = self.ioctl(cmd, set, &mut buf);
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
    }

    /// An ioctl: `buf` holds the input and receives the output. Returns a BCME status.
    fn ioctl(&mut self, cmd: u32, set: bool, buf: &mut [u8]) -> i32 {
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
                self.set_var(&name, value)
            }
            C_UP => {
                self.up = true;
                0
            }
            C_DOWN => {
                self.up = false;
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
                for x in [0x14e4, 0x43ab, 0, 6, 46, 0, 0x14e4, 0x1101, 0x0700_2d00, 0x4b0, 1, 0x4345, 11, 1, 0, 2, 0] {
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
                put(buf, &[word(1), word(1), word(0)].concat());
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
            "ver" => b"wl0: Nov 18 2026 00:00:00 version 7.45.241 (sbc-emu dongle) FWID 01-00000000\n\0".to_vec(),
            "clmver" => b"API: 18.2 Data: sbc-emu Compiler: none\0".to_vec(),
            "cap" => b"sta wme 802.11d 802.11h rm cac dualband ampdu ampdu_tx ampdu_rx amsdurx mfp\0".to_vec(),
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
            "chanspec" => word(CHSPEC_BW_20 | 1),
            "bw_cap" => word(1), // 20 MHz
            "nmode" | "rxchain" | "txchain" | "txstreams" | "rxstreams" | "mpc" | "wme" | "ampdu" => word(1),
            "vhtmode" | "ldpc_cap" | "stbc_rx" | "stbc_tx" | "txbf_bfe_cap" | "txbf_bfr_cap" => word(0),
            "country" => {
                let mut v = b"XZ\0\0".to_vec();
                v.extend_from_slice(&word(0));
                v.extend_from_slice(b"XZ\0\0");
                v
            }
            "bus:rxglom" | "tlv" | "p2p_disc" | "pfn" | "wowl" | "wowl_cap" | "rsdb_mode" | "scb_probe" | "mchan" | "mbss" | "fwevent_ext" => return None,
            n => self.vars.get(n).cloned().unwrap_or_else(|| word(0)),
        })
    }

    fn set_var(&mut self, name: &str, value: Vec<u8>) -> i32 {
        let name = name.strip_prefix("bsscfg:").unwrap_or(name);
        match name {
            // no superframes (glom) on either side
            "bus:rxglom" => BCME_UNSUPPORTED,
            "tlv" | "p2p_disc" | "fwevent_ext" | "mchan" | "mbss" => BCME_UNSUPPORTED,
            "escan" if !self.up => BCME_NOTUP,
            _ => {
                self.vars.insert(name.to_string(), value);
                0
            }
        }
    }

    /// An Ethernet frame from the air.
    pub fn receive(&mut self, eth: &[u8]) {
        let mut p = vec![BCDC_PROTO_VER << 4, 0, 0, 0];
        p.extend_from_slice(eth);
        self.out.push_back((DATA, p));
    }

    pub fn tick(&mut self, _now_ms: u64) {}

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

    #[allow(dead_code)]
    fn event(&mut self, _p: Vec<u8>) {
        self.out.push_back((EVENT, _p));
    }
}

/// The NUL-terminated iovar name at the start of a buffer.
fn iovar_name(buf: &[u8]) -> String {
    let end = buf.iter().position(|&b| b == 0).unwrap_or(buf.len());
    String::from_utf8_lossy(&buf[..end]).into_owned()
}
