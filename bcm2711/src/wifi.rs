//! The Pi 4's Wi-Fi: a CYW43455 (BCM4345 rev 6, SDIO device 0x02d0:0xa9bf) on the second SD
//! host, driven by Linux's unmodified `brcmfmac` (W2 in the plan: original driver).
//!
//! What is modelled (see docs/wifi-brcmfmac-spec.md, derived from the rpi-6.18.y driver):
//! - SDIO function 0: CCCR, FBR, CIS (Broadcom manufacturer tuples, FUNCE block sizes);
//! - function 1: the 32 KiB backplane window and the misc registers (clocks are available as
//!   soon as they are requested; keep-SDIO-on is on; save/restore stays off);
//! - the backplane: chipcommon (chip 0x4345 rev 6, AXI), an EROM listing chipcommon, D11, ARM
//!   CR4 and the SDIO device core, plain wrapper registers, CR4 bank registers (800 KiB RAM at
//!   0x198000) and the SDIO core's mailbox/interrupt registers;
//! - firmware: the driver downloads the real `brcmfmac43455-sdio.bin` into the RAM; when it
//!   releases the CR4 from reset, the *emulated* firmware takes over. The proprietary firmware
//!   is not executed — its host interface (SDPCM framing on function 2, BCDC ioctls/iovars,
//!   events) is implemented here.
//! - FullMAC behaviour: scans return the virtual access point; joining it connects the
//!   interface to the virtual network as Ethernet frames (the radio is not modelled: no RF,
//!   no 802.11 MAC timing).
use crate::sdhci::SdioCard;
use std::collections::VecDeque;

pub const SSID: &str = "sbc-emu";
const BSSID: [u8; 6] = [0x02, 0x5b, 0xc0, 0xa9, 0x00, 0x01];
const CHANNEL: u8 = 6;

const RAM_BASE: u32 = 0x198000;
const RAM_SIZE: u32 = 0xC8000;
const EROM: u32 = 0x1812_0000;
const EROM_WORDS: [u32; 17] = [
    0x4BF8_0001, 0x3608_0201, 0x1800_0005, 0x1810_0085, // chipcommon
    0x4BF8_1201, 0x2A08_0201, 0x1800_1005, 0x1810_1085, // 802.11 (D11)
    0x4BF8_3E01, 0x0708_0201, 0x1800_2005, 0x1810_2085, // ARM CR4
    0x4BF8_2901, 0x1508_0201, 0x1800_4005, 0x1810_4085, // SDIO device core
    0x0000_000F,
];
const SDIO_CORE: u32 = 0x1800_4000;
const CR4_CORE: u32 = 0x1800_2000;
const CR4_WRAP: u32 = 0x1810_2000;

// SDIO core intstatus bits
const I_HMB_FRAME_IND: u32 = 0x40;

// ioctls
const C_GET_VERSION: u32 = 1;
const C_UP: u32 = 2;
const C_SET_SSID: u32 = 26;
const C_GET_BSSID: u32 = 23;
const C_GET_SSID: u32 = 25;
const C_GET_CHANNEL: u32 = 29;
const C_GET_RSSI: u32 = 127;
const C_GET_BANDLIST: u32 = 140;
const C_GET_REVINFO: u32 = 98;
const C_GET_VAR: u32 = 262;
const C_SET_VAR: u32 = 263;
const C_DISASSOC: u32 = 52;
const C_GET_BSS_INFO: u32 = 136;

// events
const E_SET_SSID: u32 = 0;
const E_LINK: u32 = 16;
const E_ESCAN_RESULT: u32 = 69;

const BCME_UNSUPPORTED: i32 = -23;

pub struct Cyw43455 {
    // function 0
    ioe: u8,
    ien: u8,
    bus_if: u8,
    bus_speed: u8,
    brcm_cardctrl: u8,
    fn_blksize: [u16; 3],
    // function 1
    window: u32,
    watermark: u8,
    devctl: u8,
    clkcsr: u8,
    pullup: u8,
    mesbusy: u8,
    sleepcsr: u8,
    // backplane
    ram: Vec<u8>,
    regs: std::collections::HashMap<u32, u32>,
    ram_written: bool,
    running: bool,
    intstatus: u32,
    hostintmask: u32,
    // function 2
    tx_buf: Vec<u8>,
    rx: VecDeque<Vec<u8>>,
    rx_cur: Option<(Vec<u8>, usize)>,
    rx_seq: u8,
    host_seq: u8,
    // the emulated firmware
    pub mac: [u8; 6],
    event_mask: Vec<u8>,
    associated: bool,
    /// Ethernet frames from the host over the data channel (for the network)
    pub tx_out: Vec<Vec<u8>>,
    pub trace: bool,
    pub ioctls: u64,
}

impl Default for Cyw43455 {
    fn default() -> Self { Self::new() }
}

impl Cyw43455 {
    pub fn new() -> Self {
        Cyw43455 {
            ioe: 0, ien: 0, bus_if: 0, bus_speed: 0x01, brcm_cardctrl: 0, fn_blksize: [0; 3],
            window: 0, watermark: 0, devctl: 0, clkcsr: 0, pullup: 0, mesbusy: 0, sleepcsr: 0x03,
            ram: vec![0; RAM_SIZE as usize], regs: Default::default(), ram_written: false, running: false,
            intstatus: 0, hostintmask: 0, tx_buf: Vec::new(), rx: VecDeque::new(), rx_cur: None, rx_seq: 0, host_seq: 255,
            mac: [0x02, 0x5b, 0xc0, 0x00, 0x00, 0x02], event_mask: Vec::new(), associated: false,
            tx_out: Vec::new(), trace: false, ioctls: 0,
        }
    }

    pub fn associated(&self) -> bool { self.associated }

    // ------------------------------------------------------------------ function 0
    fn cis_byte(addr: u32) -> u8 {
        let manfid = [0x20u8, 4, 0xd0, 0x02, 0xbf, 0xa9];
        let common: Vec<u8> = [&manfid[..], &[0x21, 2, 0x0c, 0x00], &[0x22, 4, 0x00, 0x00, 0x02, 0x32], &[0xff]].concat();
        let func = |blk: u16| -> Vec<u8> {
            let mut funce = vec![0u8; 42];
            funce[0] = 0x01;
            funce[12] = blk as u8;
            funce[13] = (blk >> 8) as u8;
            funce[28] = 0x64; // enable timeout 1 s (10 ms units)
            [&manfid[..], &[0x22, 42], &funce[..], &[0xff]].concat()
        };
        let (base, t) = match addr {
            0x1000..=0x10ff => (0x1000, common),
            0x1100..=0x11ff => (0x1100, func(64)),
            0x1200..=0x12ff => (0x1200, func(512)),
            _ => return 0xff,
        };
        t.get((addr - base) as usize).copied().unwrap_or(0xff)
    }

    fn f0_read(&self, addr: u32) -> u8 {
        match addr {
            0x00 => 0x32,
            0x01 => 0x02,
            0x02 => self.ioe,
            0x03 => self.ior(),
            0x04 => self.ien,
            0x05 => if self.irq_pending() { 0x02 } else { 0 },
            0x07 => self.bus_if,
            0x08 => 0x13,
            0x09 => 0x00, 0x0a => 0x10, 0x0b => 0x00, // common CIS at 0x1000
            0x13 => self.bus_speed,
            0xf1 => self.brcm_cardctrl,
            0x109 => 0x00, 0x10a => 0x11, 0x10b => 0x00, // F1 CIS at 0x1100
            0x209 => 0x00, 0x20a => 0x12, 0x20b => 0x00, // F2 CIS at 0x1200
            0x110 => self.fn_blksize[1] as u8, 0x111 => (self.fn_blksize[1] >> 8) as u8,
            0x210 => self.fn_blksize[2] as u8, 0x211 => (self.fn_blksize[2] >> 8) as u8,
            0x1000..=0x17fff => Self::cis_byte(addr),
            _ => 0,
        }
    }

    fn f0_write(&mut self, addr: u32, v: u8) {
        match addr {
            0x02 => self.ioe = v & 0x06,
            0x04 => self.ien = v & 0x07,
            0x06 => { if v & 7 == 2 { self.rx_cur = None; } }
            0x07 => self.bus_if = v,
            0x13 => self.bus_speed = (v & 0x0e) | 0x01,
            0xf1 => self.brcm_cardctrl = v,
            0x110 => self.fn_blksize[1] = (self.fn_blksize[1] & 0xff00) | v as u16,
            0x111 => self.fn_blksize[1] = (self.fn_blksize[1] & 0x00ff) | (v as u16) << 8,
            0x210 => self.fn_blksize[2] = (self.fn_blksize[2] & 0xff00) | v as u16,
            0x211 => self.fn_blksize[2] = (self.fn_blksize[2] & 0x00ff) | (v as u16) << 8,
            _ => {}
        }
    }

    /// I/O ready: F1 as soon as enabled, F2 once the firmware runs.
    fn ior(&self) -> u8 {
        let mut r = self.ioe & 0x02;
        if self.ioe & 0x04 != 0 && self.running { r |= 0x04; }
        r
    }

    fn irq_pending(&self) -> bool { self.intstatus & self.hostintmask != 0 }

    // ------------------------------------------------------------------ function 1
    fn f1_misc_read(&self, addr: u32) -> u8 {
        match addr {
            0x10008 => self.watermark,
            0x10009 => self.devctl,
            0x1000a => (self.window >> 8) as u8,
            0x1000b => (self.window >> 16) as u8,
            0x1000c => (self.window >> 24) as u8,
            0x1000e => self.clkcsr | if self.clkcsr & 0x1b != 0 { 0xc0 } else { 0 },
            0x1000f => self.pullup,
            0x1001d => self.mesbusy,
            0x1001f => self.sleepcsr,
            _ => 0,
        }
    }

    fn f1_misc_write(&mut self, addr: u32, v: u8) {
        match addr {
            0x10008 => self.watermark = v,
            0x10009 => self.devctl = v,
            0x1000a => self.window = (self.window & 0xffff_00ff) | ((v as u32) << 8 & 0x8000),
            0x1000b => self.window = (self.window & 0xff00_ffff) | (v as u32) << 16,
            0x1000c => self.window = (self.window & 0x00ff_ffff) | (v as u32) << 24,
            0x1000d => { if v & 1 != 0 { self.rx_cur = None; } }
            0x1000e => self.clkcsr = v & 0x3f,
            0x1000f => self.pullup = v,
            0x1001d => self.mesbusy = v,
            0x1001f => self.sleepcsr = (v & 1) | 0x02,
            _ => {}
        }
    }

    // ------------------------------------------------------------------ the backplane
    fn bp_read32(&mut self, a: u32) -> u32 {
        if (RAM_BASE..RAM_BASE + RAM_SIZE).contains(&a) {
            let o = (a - RAM_BASE) as usize;
            return u32::from_le_bytes(self.ram[o..o + 4].try_into().unwrap());
        }
        if (EROM..EROM + 4 * EROM_WORDS.len() as u32).contains(&a) { return EROM_WORDS[((a - EROM) / 4) as usize]; }
        match a {
            0x1800_0000 => 0x1506_4345, // chipid: AXI, 5 cores, rev 6, chip 0x4345
            0x1800_0004 => 0x1000_0000, // capabilities: PMU
            0x1800_00ac => 0,           // capabilities_ext: no separate PMU (AOB)
            0x1800_00fc => EROM,
            0x1800_0604 => 28,          // pmucapabilities: pmurev 28
            0x1800_0654 => 0,           // chipcontrol data: save/restore not capable
            x if x == CR4_CORE + 0x04 => 0x0000_0005, // 5 A banks
            x if x == CR4_CORE + 0x44 => 0x13,        // each 20 x 8 KiB
            x if x == SDIO_CORE + 0x20 => self.intstatus,
            x if x == SDIO_CORE + 0x24 => self.hostintmask,
            x if x == SDIO_CORE + 0x4c => 0,
            _ => self.regs.get(&a).copied().unwrap_or(0),
        }
    }

    fn bp_write32(&mut self, a: u32, v: u32) {
        if (RAM_BASE..RAM_BASE + RAM_SIZE).contains(&a) {
            let o = (a - RAM_BASE) as usize;
            self.ram[o..o + 4].copy_from_slice(&v.to_le_bytes());
            self.ram_written = true;
            return;
        }
        match a {
            x if x == SDIO_CORE + 0x20 => self.intstatus &= !v,
            x if x == SDIO_CORE + 0x24 => self.hostintmask = v,
            _ => {
                self.regs.insert(a, v);
                // releasing the CR4 from reset after a download starts the firmware
                if (a == CR4_WRAP + 0x408 || a == CR4_WRAP + 0x800) && self.ram_written && !self.running {
                    let ioctl = self.regs.get(&(CR4_WRAP + 0x408)).copied().unwrap_or(0);
                    let reset = self.regs.get(&(CR4_WRAP + 0x800)).copied().unwrap_or(0);
                    if ioctl & 0x20 == 0 && ioctl & 1 != 0 && reset & 1 == 0 {
                        self.running = true;
                        if self.trace { eprintln!("[wifi] firmware started (CR4 released), emulated FullMAC running"); }
                    }
                }
            }
        }
    }

    fn bp_read(&mut self, a: u32, out: &mut [u8]) {
        let mut k = 0;
        while k < out.len() {
            let addr = a.wrapping_add(k as u32);
            let w = self.bp_read32(addr & !3).to_le_bytes();
            let s = (addr & 3) as usize;
            let n = (4 - s).min(out.len() - k);
            out[k..k + n].copy_from_slice(&w[s..s + n]);
            k += n;
        }
    }

    fn bp_write(&mut self, a: u32, data: &[u8]) {
        if (RAM_BASE..RAM_BASE + RAM_SIZE).contains(&a) && a + data.len() as u32 <= RAM_BASE + RAM_SIZE {
            let o = (a - RAM_BASE) as usize;
            self.ram[o..o + data.len()].copy_from_slice(data);
            self.ram_written = true;
            return;
        }
        let mut k = 0;
        while k < data.len() {
            let addr = a.wrapping_add(k as u32);
            let al = addr & !3;
            let mut w = self.bp_read32(al).to_le_bytes();
            let s = (addr & 3) as usize;
            let n = (4 - s).min(data.len() - k);
            w[s..s + n].copy_from_slice(&data[k..k + n]);
            self.bp_write32(al, u32::from_le_bytes(w));
            k += n;
        }
    }

    // ------------------------------------------------------------------ SDPCM on function 2
    fn queue_rx(&mut self, channel: u8, payload: &[u8]) {
        let l = 12 + payload.len();
        let mut f = Vec::with_capacity(l);
        f.extend_from_slice(&(l as u16).to_le_bytes());
        f.extend_from_slice(&(!(l as u16)).to_le_bytes());
        f.push(self.rx_seq);
        f.push(channel);
        f.push(0); // next length
        f.push(12); // data offset
        f.push(0); // flow control: all open
        f.push(self.host_seq.wrapping_add(1).wrapping_add(8)); // credit window
        f.extend_from_slice(&[0, 0]);
        f.extend_from_slice(payload);
        self.rx_seq = self.rx_seq.wrapping_add(1);
        self.rx.push_back(f);
        self.intstatus |= I_HMB_FRAME_IND;
    }

    fn f2_read(&mut self, out: &mut [u8]) {
        out.fill(0);
        if self.rx_cur.is_none() {
            match self.rx.pop_front() { Some(f) => self.rx_cur = Some((f, 0)), None => return }
        }
        let (f, pos) = self.rx_cur.as_mut().unwrap();
        let n = (f.len() - *pos).min(out.len());
        out[..n].copy_from_slice(&f[*pos..*pos + n]);
        *pos += out.len();
        if *pos >= f.len() { self.rx_cur = None; }
        if !self.rx.is_empty() || self.rx_cur.is_some() { self.intstatus |= I_HMB_FRAME_IND; }
    }

    fn f2_write(&mut self, data: &[u8]) {
        self.tx_buf.extend_from_slice(data);
        if self.tx_buf.len() < 12 { return; }
        let l = u16::from_le_bytes([self.tx_buf[0], self.tx_buf[1]]) as usize;
        if l < 12 || self.tx_buf.len() < l { if l < 12 { self.tx_buf.clear(); } return; }
        // one frame per transaction: the rest of the write is padding
        let frame = std::mem::take(&mut self.tx_buf);
        let frame = &frame[..l];
        self.host_seq = frame[4];
        let chan = frame[5] & 0x0f;
        let doff = (frame[7] as usize).clamp(12, l);
        let payload = frame[doff..].to_vec();
        match chan {
            0 => self.control(&payload),
            2 => self.data(&payload),
            _ => {}
        }
        // every frame from the host gets credit back, even without a reply
        if self.rx.is_empty() && chan != 0 { self.queue_credit(); }
    }

    /// A header-only frame on the control channel... is not valid; send the credit on an
    /// empty event-channel frame instead (BDC header only, dropped by the host).
    fn queue_credit(&mut self) { self.queue_rx(1, &[0x20, 0, 0, 0]); }

    fn data(&mut self, p: &[u8]) {
        if p.len() < 4 { return; }
        let off = 4 + 4 * p[3] as usize;
        if off > p.len() { return; }
        if self.associated { self.tx_out.push(p[off..].to_vec()); }
    }

    /// A frame for the host from the network (Ethernet, delivered on the data channel).
    pub fn deliver(&mut self, eth: &[u8]) {
        if !self.associated || !self.running { return; }
        let mut p = vec![0x20, 0, 0, 0];
        p.extend_from_slice(eth);
        self.queue_rx(2, &p);
    }

    // ------------------------------------------------------------------ BCDC ioctls
    fn control(&mut self, p: &[u8]) {
        if p.len() < 16 { return; }
        let cmd = u32::from_le_bytes(p[0..4].try_into().unwrap());
        let len = u32::from_le_bytes(p[4..8].try_into().unwrap()) as usize;
        let flags = u32::from_le_bytes(p[8..12].try_into().unwrap());
        let set = flags & 2 != 0;
        let data = &p[16..];
        self.ioctls += 1;
        let r = self.ioctl(cmd, set, data, len);
        let (status, mut out) = match r { Ok(v) => (0i32, v), Err(e) => (e, Vec::new()) };
        if self.trace {
            let name = if cmd == C_GET_VAR || cmd == C_SET_VAR { String::from_utf8_lossy(data.split(|&b| b == 0).next().unwrap_or(&[])).into_owned() } else { String::new() };
            eprintln!("[wifi] ioctl {cmd} {} {name} -> {status}", if set { "set" } else { "get" });
        }
        if !set { out.resize(len.min(8192), 0); }
        let mut resp = Vec::with_capacity(16 + out.len());
        resp.extend_from_slice(&cmd.to_le_bytes());
        resp.extend_from_slice(&(len as u32).to_le_bytes());
        resp.extend_from_slice(&((flags & !1) | (status != 0) as u32).to_le_bytes());
        resp.extend_from_slice(&status.to_le_bytes());
        resp.extend_from_slice(&out);
        self.queue_rx(0, &resp);
    }

    fn ioctl(&mut self, cmd: u32, set: bool, data: &[u8], len: usize) -> Result<Vec<u8>, i32> {
        let u32v = |v: u32| v.to_le_bytes().to_vec();
        match (cmd, set) {
            (C_GET_VERSION, false) => Ok(u32v(2)),
            (C_GET_BANDLIST, false) => Ok([u32v(1), u32v(2)].concat()), // one band: 2.4 GHz
            (C_GET_REVINFO, false) => {
                let mut v = vec![0u8; 68];
                for (k, x) in [0x14e4u32, 0x43ab, 0x0 /*radio*/, 6, 0x2a, 0, 0, 0x4345, 0, 6, 0, 0, 0, 0, 0, 0, 0].iter().enumerate() { v[4 * k..4 * k + 4].copy_from_slice(&x.to_le_bytes()); }
                Ok(v)
            }
            (C_GET_VAR, false) => self.get_var(data, len),
            (C_SET_VAR, true) => self.set_var(data),
            (C_SET_SSID, true) => {
                // join: struct brcmf_ssid_le { u32 len; u8 ssid[32] } (possibly followed by params)
                let n = data.get(0..4).map(|b| u32::from_le_bytes(b.try_into().unwrap()) as usize).unwrap_or(0).min(32);
                let ssid = data.get(4..4 + n).unwrap_or(&[]);
                if ssid == SSID.as_bytes() { self.join(); } else { self.event(E_SET_SSID, 3, 0, &[]); } // 3 = no networks
                Ok(Vec::new())
            }
            (C_DISASSOC, true) => { if self.associated { self.associated = false; self.event(E_LINK, 0, 0, &[]); } Ok(Vec::new()) }
            (C_GET_BSSID, false) => if self.associated { Ok(BSSID.to_vec()) } else { Err(BCME_UNSUPPORTED) },
            (C_GET_SSID, false) => { let mut v = u32v(SSID.len() as u32); v.extend_from_slice(SSID.as_bytes()); v.resize(36, 0); Ok(v) }
            (C_GET_CHANNEL, false) => Ok([u32v(CHANNEL as u32), u32v(CHANNEL as u32), u32v(0)].concat()),
            (C_GET_RSSI, false) => Ok(u32v((-45i32) as u32)),
            (C_GET_BSS_INFO, false) => {
                // u32 buffer length, then the BSS record of the current association
                let bss = self.bss_info();
                let mut v = u32v(len as u32);
                v.extend_from_slice(&bss);
                Ok(v)
            }
            (C_UP, _) => Ok(Vec::new()),
            (_, true) => Ok(Vec::new()), // other sets are accepted (scan times, PM, roam, infra, fakefrag...)
            _ => Err(BCME_UNSUPPORTED),
        }
    }

    fn get_var(&mut self, data: &[u8], len: usize) -> Result<Vec<u8>, i32> {
        let name_end = data.iter().position(|&b| b == 0).unwrap_or(data.len());
        let name = String::from_utf8_lossy(&data[..name_end]).into_owned();
        let u32v = |v: u32| v.to_le_bytes().to_vec();
        Ok(match name.as_str() {
            "cur_etheraddr" => self.mac.to_vec(),
            "ver" => b"wl0: Nov 13 2026 00:00:00 version 7.45.265 (sbc-emu emulated FullMAC) FWID 01-5bc0e000\n\0".to_vec(),
            "event_msgs" => { let mut m = self.event_mask.clone(); m.resize(len.max(1), 0); m }
            "cap" => b"sta wme 802.11d 802.11h ampdu ampdu_tx ampdu_rx\0".to_vec(),
            "chanspecs" => {
                let mut v = u32v(13);
                for ch in 1..=13u32 { v.extend_from_slice(&u32v(0x1000 | ch)); }
                v
            }
            "per_chan_info" | "nmode" | "vhtmode" | "mpc" => u32v(0),
            "chanspec" => u32v(0x1000 | CHANNEL as u32),
            "assoc_info" => vec![0u8; 8], // no association request/response IEs recorded
            "bw_cap" | "mimo_bw_cap" | "rxchain" | "txstreams" => u32v(1),
            _ => return Err(BCME_UNSUPPORTED),
        })
    }

    fn set_var(&mut self, data: &[u8]) -> Result<Vec<u8>, i32> {
        let name_end = data.iter().position(|&b| b == 0).unwrap_or(data.len());
        let name = String::from_utf8_lossy(&data[..name_end]).into_owned();
        let val = data.get(name_end + 1..).unwrap_or(&[]);
        match name.as_str() {
            // keep host-side glomming and flow-control TLVs off (they change the framing)
            "bus:rxglom" | "tlv" => return Err(BCME_UNSUPPORTED),
            "event_msgs" => self.event_mask = val.to_vec(),
            "join" => {
                // brcmf_ext_join_params_le: starts with brcmf_ssid_le { u32 len; u8 ssid[32] }
                let n = val.get(0..4).map(|b| u32::from_le_bytes(b.try_into().unwrap()) as usize).unwrap_or(0).min(32);
                let ssid = val.get(4..4 + n).unwrap_or(&[]).to_vec();
                if ssid == SSID.as_bytes() { self.join(); } else { self.event(E_SET_SSID, 3, 0, &[]); }
            }
            "escan" => {
                let sync = val.get(6..8).map(|b| u16::from_le_bytes([b[0], b[1]])).unwrap_or(0);
                self.scan_results(sync);
            }
            _ => {}
        }
        Ok(Vec::new())
    }

    // ------------------------------------------------------------------ events
    fn event_enabled(&self, ev: u32) -> bool {
        self.event_mask.get((ev / 8) as usize).is_some_and(|b| b & (1 << (ev % 8)) != 0)
    }

    fn event(&mut self, ev: u32, status: u32, flags: u16, data: &[u8]) {
        if !self.event_enabled(ev) { return; }
        let mut p = vec![0x20, 0, 0, 0]; // BDC: version 2, interface 0
        p.extend_from_slice(&self.mac); // ethernet: dst
        p.extend_from_slice(&self.mac); // src
        p.extend_from_slice(&0x886cu16.to_be_bytes());
        let body_len = 10 + 48 + data.len();
        p.extend_from_slice(&0x8001u16.to_be_bytes()); // brcm_ethhdr subtype
        p.extend_from_slice(&(body_len as u16).to_be_bytes());
        p.push(0); // version
        p.extend_from_slice(&[0x00, 0x10, 0x18]); // Broadcom OUI
        p.extend_from_slice(&1u16.to_be_bytes()); // usr_subtype: event
        p.extend_from_slice(&2u16.to_be_bytes()); // event message version
        p.extend_from_slice(&flags.to_be_bytes());
        p.extend_from_slice(&ev.to_be_bytes());
        p.extend_from_slice(&status.to_be_bytes());
        p.extend_from_slice(&0u32.to_be_bytes()); // reason
        p.extend_from_slice(&0u32.to_be_bytes()); // auth type
        p.extend_from_slice(&(data.len() as u32).to_be_bytes());
        p.extend_from_slice(&BSSID);
        let mut ifname = [0u8; 16];
        ifname[..5].copy_from_slice(b"wlan0");
        p.extend_from_slice(&ifname);
        p.push(0); // ifidx
        p.push(0); // bsscfgidx
        p.extend_from_slice(data);
        self.queue_rx(1, &p);
    }

    fn bss_info(&self) -> Vec<u8> {
        let mut ies = vec![0u8, SSID.len() as u8];
        ies.extend_from_slice(SSID.as_bytes());
        ies.extend_from_slice(&[1, 8, 0x82, 0x84, 0x8b, 0x96, 0x0c, 0x12, 0x18, 0x24]); // rates
        ies.extend_from_slice(&[3, 1, CHANNEL]); // DS parameter set
        let mut b = vec![0u8; 128];
        let put16 = |b: &mut Vec<u8>, o: usize, v: u16| b[o..o + 2].copy_from_slice(&v.to_le_bytes());
        let put32 = |b: &mut Vec<u8>, o: usize, v: u32| b[o..o + 4].copy_from_slice(&v.to_le_bytes());
        put32(&mut b, 0, 109); // version
        put32(&mut b, 4, (128 + ies.len()) as u32);
        b[8..14].copy_from_slice(&BSSID);
        put16(&mut b, 14, 100); // beacon period
        put16(&mut b, 16, 0x0401); // capability: ESS, short slot
        b[18] = SSID.len() as u8;
        b[19..19 + SSID.len()].copy_from_slice(SSID.as_bytes());
        put32(&mut b, 52, 8);
        b[56..64].copy_from_slice(&[0x82, 0x84, 0x8b, 0x96, 0x0c, 0x12, 0x18, 0x24]);
        put16(&mut b, 72, 0x1000 | CHANNEL as u16); // chanspec: 2.4 GHz, 20 MHz
        b[76] = 1; // DTIM
        put16(&mut b, 78, (-45i16) as u16); // RSSI
        b[80] = (-92i8) as u8; // noise
        b[88] = CHANNEL;
        put16(&mut b, 116, 128); // ie_offset
        put32(&mut b, 120, ies.len() as u32);
        put16(&mut b, 124, 40); // SNR
        b.extend_from_slice(&ies);
        b
    }

    fn scan_results(&mut self, sync: u16) {
        let bss = self.bss_info();
        let mut partial = Vec::new();
        partial.extend_from_slice(&((12 + bss.len()) as u32).to_le_bytes());
        partial.extend_from_slice(&109u32.to_le_bytes());
        partial.extend_from_slice(&sync.to_le_bytes());
        partial.extend_from_slice(&1u16.to_le_bytes());
        partial.extend_from_slice(&bss);
        self.event(E_ESCAN_RESULT, 8, 0, &partial); // partial result
        let mut done = Vec::new();
        done.extend_from_slice(&12u32.to_le_bytes());
        done.extend_from_slice(&109u32.to_le_bytes());
        done.extend_from_slice(&sync.to_le_bytes());
        done.extend_from_slice(&0u16.to_le_bytes());
        self.event(E_ESCAN_RESULT, 0, 0, &done); // scan complete
    }

    fn join(&mut self) {
        self.associated = true;
        if self.trace { eprintln!("[wifi] associated with '{SSID}'"); }
        self.event(E_LINK, 0, 1, &[]); // flags bit 0: link up
        self.event(E_SET_SSID, 0, 0, &[]);
    }
}

impl SdioCard for Cyw43455 {
    fn num_functions(&self) -> u8 { 2 }

    fn rw_direct(&mut self, write: bool, func: u8, addr: u32, data: u8, _raw: bool) -> u8 {
        match func {
            0 => { if write { self.f0_write(addr, data); } self.f0_read(addr) }
            1 if addr >= 0x10000 => { if write { self.f1_misc_write(addr, data); } self.f1_misc_read(addr) }
            1 => {
                let a = self.window | (addr & 0x7fff);
                if write { self.bp_write(a, &[data]); }
                let mut b = [0u8];
                self.bp_read(a, &mut b);
                b[0]
            }
            _ => 0,
        }
    }

    fn read_ext(&mut self, func: u8, addr: u32, incr: bool, out: &mut [u8]) {
        match func {
            0 => for (k, b) in out.iter_mut().enumerate() { *b = self.f0_read(addr + if incr { k as u32 } else { 0 }); },
            1 if addr >= 0x10000 => for (k, b) in out.iter_mut().enumerate() { *b = self.f1_misc_read(addr + if incr { k as u32 } else { 0 }); },
            1 => { let a = self.window | (addr & 0x7fff); self.bp_read(a, out); }
            2 => self.f2_read(out),
            _ => out.fill(0),
        }
    }

    fn write_ext(&mut self, func: u8, addr: u32, incr: bool, data: &[u8]) {
        match func {
            0 => for (k, &b) in data.iter().enumerate() { self.f0_write(addr + if incr { k as u32 } else { 0 }, b); },
            1 if addr >= 0x10000 => for (k, &b) in data.iter().enumerate() { self.f1_misc_write(addr + if incr { k as u32 } else { 0 }, b); },
            1 => { let a = self.window | (addr & 0x7fff); self.bp_write(a, data); }
            2 => self.f2_write(data),
            _ => {}
        }
    }

    fn irq(&self) -> bool { self.ien & 0x03 == 0x03 && self.irq_pending() }
    fn take_tx(&mut self) -> Vec<Vec<u8>> { std::mem::take(&mut self.tx_out) }
    fn deliver(&mut self, eth: &[u8]) { Cyw43455::deliver(self, eth) }
    fn set_trace(&mut self, on: bool) { self.trace = on; }

    fn save_state(&self, w: &mut emu_core::snap::W) {
        w.tag("cyw43455-v1");
        for v in [self.ioe, self.ien, self.bus_if, self.bus_speed, self.brcm_cardctrl, self.watermark, self.devctl, self.clkcsr, self.pullup, self.mesbusy, self.sleepcsr, self.rx_seq, self.host_seq] { w.u8(v); }
        for v in self.fn_blksize { w.u16(v); }
        w.u32(self.window);
        w.bool(self.ram_written); w.bool(self.running); w.bool(self.associated);
        w.u32(self.intstatus); w.u32(self.hostintmask);
        w.bytes(&self.mac); w.bytes(&self.event_mask);
        let mut regs: Vec<(u32, u32)> = self.regs.iter().map(|(k, v)| (*k, *v)).collect();
        regs.sort();
        w.u64(regs.len() as u64);
        for (k, v) in regs { w.u32(k); w.u32(v); }
        w.u64(self.rx.len() as u64);
        for f in &self.rx { w.bytes(f); }
        // the firmware image in RAM is not needed once running (the emulated firmware replaces it)
    }

    fn load_state(&mut self, r: &mut emu_core::snap::R) -> Result<(), emu_core::snap::SnapError> {
        r.tag("cyw43455-v1")?;
        for v in [&mut self.ioe, &mut self.ien, &mut self.bus_if, &mut self.bus_speed, &mut self.brcm_cardctrl, &mut self.watermark, &mut self.devctl,
                  &mut self.clkcsr, &mut self.pullup, &mut self.mesbusy, &mut self.sleepcsr, &mut self.rx_seq, &mut self.host_seq] { *v = r.u8()?; }
        for v in self.fn_blksize.iter_mut() { *v = r.u16()?; }
        self.window = r.u32()?;
        self.ram_written = r.bool()?; self.running = r.bool()?; self.associated = r.bool()?;
        self.intstatus = r.u32()?; self.hostintmask = r.u32()?;
        self.mac.copy_from_slice(&r.bytes()?[..6]);
        self.event_mask = r.bytes()?.to_vec();
        self.regs.clear();
        for _ in 0..r.u64()? { let k = r.u32()?; let v = r.u32()?; self.regs.insert(k, v); }
        self.rx.clear();
        for _ in 0..r.u64()? { self.rx.push_back(r.bytes()?.to_vec()); }
        self.rx_cur = None;
        self.tx_buf.clear();
        Ok(())
    }
}
