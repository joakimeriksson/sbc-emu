//! The AP6256's Bluetooth half (BCM4345C5) as an HCI controller on the JH7110's UART1 (H4
//! framing), as the Orange Pi RV wires it. Xunlong's kernel has no serdev node for it: user
//! space attaches the line discipline (`hciattach`), then the HCI core initialises it.
//!
//! Adapted from sbc-emu's Raspberry Pi 4 model (`bcm2711/src/bluetooth.rs`, the CYW43455's
//! Bluetooth core, the same family).
//!
//! The radio is modelled as one virtual Bluetooth LE peripheral nearby, "sbc-emu sensor": it
//! advertises (connectable, with a name, a 16-bit service UUID and manufacturer data
//! carrying a counter) and LE scans report it every 100 ms of guest time. Classic inquiry
//! finds nothing. No connections.
use std::collections::VecDeque;

const H4_CMD: u8 = 1;
const H4_ACL: u8 = 2;
const H4_EVT: u8 = 4;

const EV_CMD_COMPLETE: u8 = 0x0e;
const EV_CMD_STATUS: u8 = 0x0f;
const EV_INQUIRY_COMPLETE: u8 = 0x01;
const EV_LE_META: u8 = 0x3e;

/// The controller's own address, and the virtual sensor's.
const BD_ADDR: [u8; 6] = [0x02, 0x00, 0x56, 0x62, 0x5b, 0x02]; // 02:5B:62:56:00:02 (little-endian), locally administered
const SENSOR_ADDR: [u8; 6] = [0x01, 0x00, 0xe0, 0xbc, 0x5b, 0xc2]; // C2:5B:BC:E0:00:01, static random

pub struct Bluetooth {
    /// bytes from the host (the UART's TX) not yet parsed into packets
    rx: Vec<u8>,
    /// bytes for the host (the UART's RX)
    pub tx: VecDeque<u8>,
    name: Vec<u8>,
    event_mask: [u8; 8],
    le_event_mask: [u8; 8],
    scan_enable: u8,
    le_scanning: bool,
    /// active scanning (scan requests, so scan responses come back)
    le_active: bool,
    /// guest time (ns) and when the next advertising report is due
    now: u64,
    next_adv: u64,
    adv_count: u32,
    pub commands: u64,
    pub trace: bool,
}

impl Default for Bluetooth { fn default() -> Self { Self::new() } }

impl Bluetooth {
    pub fn new() -> Self {
        Bluetooth { rx: Vec::new(), tx: VecDeque::new(), name: b"BCM4345C5".to_vec(), event_mask: [0xff, 0xff, 0xfb, 0xff, 0x07, 0xf8, 0xbf, 0x3d],
                    le_event_mask: [0x1f, 0, 0, 0, 0, 0, 0, 0], scan_enable: 0, le_scanning: false, le_active: false, now: 0, next_adv: 0, adv_count: 0, commands: 0,
                    trace: std::env::var_os("SBC_TRACE_BT").is_some() }
    }

    fn event(&mut self, code: u8, params: &[u8]) {
        self.tx.push_back(H4_EVT);
        self.tx.push_back(code);
        self.tx.push_back(params.len() as u8);
        self.tx.extend(params.iter().copied());
    }

    fn complete(&mut self, op: u16, ret: &[u8]) {
        let mut p = vec![1];
        p.extend_from_slice(&op.to_le_bytes());
        p.extend_from_slice(ret);
        self.event(EV_CMD_COMPLETE, &p);
    }

    fn status(&mut self, op: u16, status: u8) {
        let mut p = vec![status, 1];
        p.extend_from_slice(&op.to_le_bytes());
        self.event(EV_CMD_STATUS, &p);
    }

    /// Bytes the host sent.
    pub fn receive(&mut self, bytes: &[u8]) {
        self.rx.extend_from_slice(bytes);
        loop {
            let need = match self.rx.first() {
                None => return,
                Some(&H4_CMD) => { if self.rx.len() < 4 { return; } 4 + self.rx[3] as usize }
                Some(&H4_ACL) => { if self.rx.len() < 5 { return; } 5 + u16::from_le_bytes([self.rx[3], self.rx[4]]) as usize }
                Some(_) => { self.rx.remove(0); continue; } // out of sync: skip a byte
            };
            if self.rx.len() < need { return; }
            let pkt: Vec<u8> = self.rx.drain(..need).collect();
            if pkt[0] == H4_CMD { self.command(u16::from_le_bytes([pkt[1], pkt[2]]), &pkt[4..]); }
        }
    }

    fn command(&mut self, op: u16, p: &[u8]) {
        self.commands += 1;
        if self.trace { eprintln!("[bt] command {op:#06x} {p:02x?}"); }
        let ok = |v: &[u8]| { let mut r = vec![0u8]; r.extend_from_slice(v); r };
        let r: Vec<u8> = match op {
            0x0c03 => {
                // Reset: back to the power-on state
                // (the bytes still queued in both directions, the clock and the counters stay)
                let (rx, tx) = (std::mem::take(&mut self.rx), std::mem::take(&mut self.tx));
                let (now, commands, trace) = (self.now, self.commands, self.trace);
                *self = Bluetooth { rx, tx, now, next_adv: now, commands, trace, ..Bluetooth::new() };
                ok(&[])
            }
            0x1001 => ok(&[0x09, 0x00, 0x00, 0x09, 0x0f, 0x00, 0x19, 0x61]), // version 5.0, Broadcom, subversion 0x6119 (BCM4345C0)
            // Read Local Supported Commands: only LE Set Scan Parameters and LE Set Scan Enable
            // among the optional ones (Linux enables advertising report events for them), so
            // Linux sends only commands answered here with their full return parameters
            0x1002 => { let mut c = [0u8; 64]; c[26] = 0x04 | 0x08; ok(&c) }
            0x1003 => ok(&[0xbf, 0xfe, 0xcf, 0xfe, 0xdb, 0xff, 0x7b, 0x87]), // LMP features (BR/EDR + LE, SSP, EIR)
            0x1004 => ok(&[p.first().copied().unwrap_or(0), 1, if p.first() == Some(&1) { 0x01 } else { 0xbf }, 0, 0, 0, 0, 0, 0, 0]), // extended features
            0x1005 => ok(&[0xfd, 0x03, 0x40, 0x08, 0x00, 0x01, 0x00]), // buffers: ACL 1021 x 8, SCO 64 x 1
            0x1009 => ok(&BD_ADDR),
            0x100b => ok(&[0, 0]), // local supported codecs: none
            0x0c14 => { let mut n = self.name.clone(); n.resize(248, 0); ok(&n) } // Read Local Name
            0x0c13 => { self.name = p.iter().copied().take_while(|&b| b != 0).collect(); ok(&[]) } // Write Local Name
            0x0c23 => ok(&[0, 0, 0]), // class of device
            0x0c25 => ok(&[0x60, 0x00]), // voice setting
            0x0c38 => ok(&[1]), // number of supported IAC
            0x0c39 => ok(&[1, 0x33, 0x8b, 0x9e]), // current IAC LAP: GIAC
            0x0c1b => ok(&[0x00, 0x08, 0x12, 0x00]), // page scan activity
            0x0c46 => ok(&[0]), // page scan type
            0x0c58 => ok(&[0x04]), // inquiry response TX power
            0x0c0d => ok(&[0x00, 0x00, 0x00, 0x00]), // stored link keys: none
            0x0c12 => ok(&[0x00, 0x00]), // delete stored link key
            0x0c01 => { if p.len() >= 8 { self.event_mask.copy_from_slice(&p[..8]); } ok(&[]) }
            0x0c1a => { self.scan_enable = p.first().copied().unwrap_or(0); ok(&[]) } // Write Scan Enable
            0x0c19 => ok(&[self.scan_enable]), // Read Scan Enable
            0x0c4f => ok(&[0x00]), // read inquiry mode... (default)
            0x0c55 => ok(&[0x01]), // simple pairing mode
            0x0c51 => { let mut e = vec![0u8]; e.resize(241, 0); ok(&e) } // read EIR
            0x0c79 => ok(&[0x00]), // read secure connections host support
            0x0c2d => ok(&[0x00, 0x00, 0x00]), // read transmit power level (handle, level)
            0x0c6c => ok(&[0x01, 0x00]), // read LE host support
            0x100a..=0x100f => ok(&[]),
            0x0401 => { // Inquiry: no classic devices around
                self.status(op, 0);
                self.event(EV_INQUIRY_COMPLETE, &[0]);
                return;
            }
            0x0402 => ok(&[]), // inquiry cancel
            // LE
            0x2001 => { if p.len() >= 8 { self.le_event_mask.copy_from_slice(&p[..8]); } ok(&[]) }
            0x2002 => ok(&[0xfb, 0x00, 0x08]), // LE buffer size: 251 x 8
            0x2003 => ok(&[0x01, 0, 0, 0, 0, 0, 0, 0]), // LE features: encryption
            0x2007 => ok(&[0x04]), // advertising channel TX power
            0x200f => ok(&[0x10]), // accept list size
            0x201c => ok(&[0xff, 0xff, 0xff, 0xff, 0xff, 0x03, 0x00, 0x00]), // supported states
            0x2023 => ok(&[0x1b, 0x00, 0x48, 0x01]), // suggested default data length
            0x202a => ok(&[0x00]), // resolving list size
            0x202f => ok(&[0xfb, 0x00, 0x48, 0x08, 0xfb, 0x00, 0x48, 0x08]), // max data length
            0x2018 => ok(&[0x5b, 0xc0, 0xe0, 0xb8, 0x12, 0x34, 0x56, 0x78]), // LE Rand
            0x200b => { self.le_active = p.first() == Some(&1); ok(&[]) } // LE Set Scan Parameters
            0x200c => { // LE Set Scan Enable
                self.le_scanning = p.first() == Some(&1);
                self.next_adv = self.now;
                ok(&[])
            }
            // Broadcom vendor commands (hci_bcm / btbcm)
            0xfc79 => ok(&[0x05, 0x00, 0x00, 0x16, 0x01, 0x00]), // read verbose config: chip 0x05, build 278
            0xfc6e => ok(&[0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00]), // read controller features
            0xfc5a => ok(&[0x00, 0x00, 0x00, 0x00]), // read USB product
            0xfc2e | 0xfc4c | 0xfc4e | 0xfc18 | 0xfc45 | 0xfc1c | 0xfc27 | 0xfc01 | 0xfc6d => ok(&[]), // patch RAM, baud rate, PCM, sleep mode, BD_ADDR
            // everything else that only configures: done
            _ => ok(&[]),
        };
        self.complete(op, &r);
    }

    /// Guest time passes: advertising reports while an LE scan runs.
    pub fn tick(&mut self, now: u64) {
        self.now = now;
        if !self.le_scanning || now < self.next_adv { return; }
        self.next_adv = now + 100_000_000;
        // a host that stopped reading (the line discipline detached mid-scan) loses the reports
        // rather than piling them up; time keeps moving either way
        if self.tx.len() > 4096 { return; }
        self.adv_count = self.adv_count.wrapping_add(1);
        let mut ad = vec![2, 0x01, 0x06]; // flags: LE general discoverable, no BR/EDR
        let name = b"sbc-emu sensor";
        ad.push(name.len() as u8 + 1);
        ad.push(0x09);
        ad.extend_from_slice(name);
        ad.extend_from_slice(&[3, 0x03, 0x1a, 0x18]); // 16-bit service UUID 0x181a: environmental sensing
        ad.extend_from_slice(&[7, 0xff, 0xff, 0xff]); // manufacturer data (test company id), a counter
        ad.extend_from_slice(&self.adv_count.to_le_bytes());
        let mut p = vec![0x02, 1, 0x00, 0x01]; // advertising report: 1 report, ADV_IND, random address
        p.extend_from_slice(&SENSOR_ADDR);
        p.push(ad.len() as u8);
        p.extend_from_slice(&ad);
        p.push((-52i8) as u8); // RSSI
        if self.le_event_mask[0] & 0x02 == 0 { return; }
        self.event(EV_LE_META, &p);
        if self.le_active {
            // the scan request is answered: a scan response with the TX power level
            let mut p = vec![0x02, 1, 0x04, 0x01];
            p.extend_from_slice(&SENSOR_ADDR);
            p.extend_from_slice(&[3, 2, 0x0a, 0x04, (-52i8) as u8]);
            self.event(EV_LE_META, &p);
        }
    }

    pub fn next_event(&self) -> Option<u64> { self.le_scanning.then_some(self.next_adv) }

    pub fn save_state(&self, w: &mut emu_core::snap::W) {
        w.tag("bt-v1");
        w.bytes(&self.name); w.bytes(&self.event_mask); w.bytes(&self.le_event_mask);
        w.u8(self.scan_enable); w.bool(self.le_scanning); w.u64(self.next_adv); w.u32(self.adv_count); w.bool(self.le_active);
    }
    pub fn load_state(&mut self, r: &mut emu_core::snap::R) -> Result<(), emu_core::snap::SnapError> {
        r.tag("bt-v1")?;
        self.name = r.bytes()?.to_vec();
        self.event_mask = r.bytes()?.try_into().unwrap_or([0xff; 8]);
        self.le_event_mask = r.bytes()?.try_into().unwrap_or([0x1f, 0, 0, 0, 0, 0, 0, 0]);
        self.scan_enable = r.u8()?; self.le_scanning = r.bool()?; self.next_adv = r.u64()?; self.adv_count = r.u32()?; self.le_active = r.bool()?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_full_backlog_does_not_stop_the_clock() {
        let mut bt = Bluetooth::new();
        // LE Set Scan Enable (0x200c): on
        bt.receive(&[H4_CMD, 0x0c, 0x20, 2, 1, 0]);
        // nobody reads: the host's side of the line is gone
        for t in 1..200u64 {
            bt.tick(t * 100_000_000);
            let next = bt.next_event().unwrap();
            assert!(next > t * 100_000_000, "the next report is always in the future");
        }
        assert!(bt.tx.len() <= 4096 + 64);
    }

    #[test]
    fn reset_restores_the_defaults() {
        let mut bt = Bluetooth::new();
        // Write Local Name (0x0c13) then Reset (0x0c03)
        let mut name = vec![H4_CMD, 0x13, 0x0c, 248];
        name.extend_from_slice(b"renamed");
        name.resize(4 + 248, 0);
        bt.receive(&name);
        bt.receive(&[H4_CMD, 0x03, 0x0c, 0]);
        assert_eq!(bt.name, b"BCM4345C5");
    }
}
