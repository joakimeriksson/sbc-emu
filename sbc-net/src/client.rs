//! A client host on a link the board serves: the far end of a cable whose other end is the
//! board's own DHCP server, as with a PC plugged into a Jetson's USB device-mode port (the board
//! is 192.168.55.1 and hands the PC 192.168.55.100).
//!
//! `VirtualNet` is the opposite arrangement (sbc-net plays the gateway and the board is its
//! station). Here sbc-net plays the station: it takes an address from the board's DHCP server,
//! answers ARP and ping, and carries host TCP port forwards to a fixed address on the board.
//! The forwards reuse the NAT's relay (`nat::Nat`), with this host as the address the
//! connections come from. The board's own connections to this host are refused (nothing
//! listens here).

use crate::nat::Nat;
use crate::net::packet::{checksum, ethernet, ip_packet, transport_checksum, udp_packet, GATEWAY_MAC};

const BROADCAST: [u8; 6] = [0xff; 6];
const DHCP_MAGIC: [u8; 4] = [0x63, 0x82, 0x53, 0x63];
/// How long to wait for an offer or an acknowledgement before asking again.
const DHCP_RETRY_US: u64 = 3_000_000;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Dhcp {
    /// the link is down: nothing is sent
    Down,
    /// DISCOVER sent at the time given
    Selecting(u64),
    /// REQUEST sent for the offer (address, server) at the time given
    Requesting([u8; 4], [u8; 4], u64),
    /// the lease: since when, for how long (s); renewed halfway
    Bound(u64, u32),
}

pub struct Client {
    /// this host's MAC (the one the NAT relay's segments carry)
    pub mac: [u8; 6],
    /// the address the board's DHCP server leased, once it did
    pub ip: Option<[u8; 4]>,
    pub mask: [u8; 4],
    pub server: Option<[u8; 4]>,
    /// where the forwards go (the board's address on this link)
    pub board_ip: [u8; 4],
    board_mac: Option<[u8; 6]>,
    dhcp: Dhcp,
    xid: u32,
    last_arp_us: u64,
    pub nat: Nat,
    pub log: bool,
    /// leases taken, ARP and ping answers given, board connections refused
    pub leases: u64,
    pub arp_replies: u64,
    pub pings: u64,
    pub refused: u64,
}

fn be16(b: &[u8]) -> u16 {
    u16::from_be_bytes([b[0], b[1]])
}

fn dotted(a: &[u8; 4]) -> String {
    format!("{}.{}.{}.{}", a[0], a[1], a[2], a[3])
}

impl Client {
    pub fn new(board_ip: [u8; 4], log: bool) -> Self {
        Client {
            mac: GATEWAY_MAC,
            ip: None,
            mask: [255, 255, 255, 0],
            server: None,
            board_ip,
            board_mac: None,
            dhcp: Dhcp::Down,
            xid: 0x5bc0_e700,
            last_arp_us: 0,
            nat: Nat::new(log),
            log,
            leases: 0,
            arp_replies: 0,
            pings: 0,
            refused: 0,
        }
    }

    /// The cable is in (the board's interface came up): ask for an address.
    pub fn link_up(&mut self, now_us: u64) -> Vec<Vec<u8>> {
        self.ip = None;
        self.board_mac = None;
        self.xid = self.xid.wrapping_add(1);
        self.dhcp = Dhcp::Selecting(now_us);
        vec![self.dhcp_message(1, None, None)]
    }

    /// The cable is out: forget the lease.
    pub fn link_down(&mut self) {
        self.dhcp = Dhcp::Down;
        self.ip = None;
        self.board_mac = None;
    }

    pub fn bound(&self) -> bool {
        matches!(self.dhcp, Dhcp::Bound(..))
    }

    /// Timers: DHCP retries and renewals, the forwards' traffic; returns frames for the board.
    pub fn poll(&mut self, now_us: u64) -> Vec<Vec<u8>> {
        let mut out = Vec::new();
        match self.dhcp {
            Dhcp::Down => return out,
            Dhcp::Selecting(at) if now_us.wrapping_sub(at) >= DHCP_RETRY_US => {
                self.xid = self.xid.wrapping_add(1);
                self.dhcp = Dhcp::Selecting(now_us);
                out.push(self.dhcp_message(1, None, None));
            }
            Dhcp::Requesting(_, _, at) if now_us.wrapping_sub(at) >= DHCP_RETRY_US => {
                // no answer to the request: start over
                self.xid = self.xid.wrapping_add(1);
                self.dhcp = Dhcp::Selecting(now_us);
                out.push(self.dhcp_message(1, None, None));
            }
            Dhcp::Bound(at, lease) if now_us.wrapping_sub(at) >= u64::from(lease.max(2)) * 500_000 => {
                // renew halfway through the lease (the Jetson's is 15 s)
                let (ip, server) = (self.ip.unwrap_or([0; 4]), self.server.unwrap_or(self.board_ip));
                self.xid = self.xid.wrapping_add(1);
                self.dhcp = Dhcp::Requesting(ip, server, now_us);
                out.push(self.dhcp_message(3, Some(ip), Some(server)));
            }
            _ => {}
        }
        let Some(ip) = self.ip else { return out };
        match self.board_mac {
            Some(mac) => out.extend(self.nat.poll(now_us, Some((mac, self.board_ip)), ip)),
            // the forwards need the board's MAC: ask for it once a second until it is known
            None if now_us.wrapping_sub(self.last_arp_us) >= 1_000_000 => {
                self.last_arp_us = now_us;
                out.push(self.arp(1, &BROADCAST, &[0; 6], &self.board_ip.clone()));
            }
            None => {}
        }
        out
    }

    /// One Ethernet frame from the board; returns frames to send back.
    pub fn handle(&mut self, eth: &[u8], now_us: u64) -> Vec<Vec<u8>> {
        if eth.len() < 14 || self.dhcp == Dhcp::Down {
            return Vec::new();
        }
        let dst = &eth[0..6];
        if dst != self.mac && dst[0] & 1 == 0 {
            return Vec::new(); // unicast to someone else
        }
        let mut src = [0u8; 6];
        src.copy_from_slice(&eth[6..12]);
        match be16(&eth[12..14]) {
            0x0806 => self.on_arp(&eth[14..], &src),
            0x0800 => self.on_ipv4(&eth[14..], &src, now_us),
            _ => Vec::new(),
        }
    }

    fn arp(&self, op: u16, dst: &[u8; 6], target_mac: &[u8; 6], target_ip: &[u8; 4]) -> Vec<u8> {
        let mut p = Vec::with_capacity(28);
        p.extend_from_slice(&[0, 1, 8, 0, 6, 4]);
        p.extend_from_slice(&op.to_be_bytes());
        p.extend_from_slice(&self.mac);
        p.extend_from_slice(&self.ip.unwrap_or([0; 4]));
        p.extend_from_slice(target_mac);
        p.extend_from_slice(target_ip);
        ethernet(dst, &self.mac, 0x0806, &p)
    }

    fn on_arp(&mut self, p: &[u8], src: &[u8; 6]) -> Vec<Vec<u8>> {
        if p.len() < 28 {
            return Vec::new();
        }
        let sender_ip: [u8; 4] = p[14..18].try_into().unwrap();
        let target_ip: [u8; 4] = p[24..28].try_into().unwrap();
        if sender_ip == self.board_ip {
            self.board_mac = Some(p[8..14].try_into().unwrap());
        }
        if be16(&p[6..8]) == 1 && Some(target_ip) == self.ip {
            self.arp_replies += 1;
            let sender_mac: [u8; 6] = p[8..14].try_into().unwrap();
            return vec![self.arp(2, src, &sender_mac, &sender_ip)];
        }
        Vec::new()
    }

    fn on_ipv4(&mut self, p: &[u8], src: &[u8; 6], now_us: u64) -> Vec<Vec<u8>> {
        if p.len() < 20 || p[0] >> 4 != 4 {
            return Vec::new();
        }
        let ihl = usize::from(p[0] & 0xf) * 4;
        let total = usize::from(be16(&p[2..4]));
        if ihl < 20 || total < ihl || total > p.len() || be16(&p[6..8]) & 0x3fff != 0 {
            return Vec::new();
        }
        let sip: [u8; 4] = p[12..16].try_into().unwrap();
        let dip: [u8; 4] = p[16..20].try_into().unwrap();
        let body = &p[ihl..total];
        if sip == self.board_ip {
            self.board_mac = Some(*src);
        }
        let to_us = Some(dip) == self.ip;
        match p[9] {
            17 if body.len() >= 8 && be16(&body[2..4]) == 68 => self.on_dhcp(&body[8..], src, now_us),
            1 if to_us && body.len() >= 8 && body[0] == 8 => {
                let mut icmp = body.to_vec();
                icmp[0] = 0;
                icmp[2..4].copy_from_slice(&[0, 0]);
                let c = checksum(&icmp, 0);
                icmp[2..4].copy_from_slice(&c.to_be_bytes());
                self.pings += 1;
                vec![ethernet(src, &self.mac, 0x0800, &ip_packet(1, &dip, &sip, &icmp))]
            }
            6 if to_us && body.len() >= 20 => {
                if body[13] & 0x16 == 0x02 {
                    // a connection from the board to this host: nothing listens here
                    return self.refuse(body, src, &sip, &dip);
                }
                self.nat.tcp_in(src, &sip, &dip, body, now_us)
            }
            _ => Vec::new(),
        }
    }

    fn refuse(&mut self, t: &[u8], src: &[u8; 6], sip: &[u8; 4], dip: &[u8; 4]) -> Vec<Vec<u8>> {
        let seq = u32::from_be_bytes([t[4], t[5], t[6], t[7]]);
        let mut r = Vec::with_capacity(20);
        r.extend_from_slice(&t[2..4]);
        r.extend_from_slice(&t[0..2]);
        r.extend_from_slice(&[0; 4]);
        r.extend_from_slice(&seq.wrapping_add(1).to_be_bytes());
        r.extend_from_slice(&[0x50, 0x14, 0, 0, 0, 0, 0, 0]); // RST|ACK
        let c = transport_checksum(dip, sip, 6, &r);
        r[16..18].copy_from_slice(&c.to_be_bytes());
        self.refused += 1;
        vec![ethernet(src, &self.mac, 0x0800, &ip_packet(6, dip, sip, &r))]
    }

    /// A BOOTREQUEST of `kind` (1 DISCOVER, 3 REQUEST), broadcast.
    fn dhcp_message(&self, kind: u8, requested: Option<[u8; 4]>, server: Option<[u8; 4]>) -> Vec<u8> {
        let mut b = vec![0u8; 240];
        b[0] = 1;
        b[1] = 1;
        b[2] = 6;
        b[4..8].copy_from_slice(&self.xid.to_be_bytes());
        b[10] = 0x80; // answer by broadcast: this host has no address yet
        b[28..34].copy_from_slice(&self.mac);
        b[236..240].copy_from_slice(&DHCP_MAGIC);
        b.extend_from_slice(&[53, 1, kind]);
        b.extend_from_slice(&[61, 7, 1]);
        b.extend_from_slice(&self.mac);
        if let Some(ip) = requested {
            b.extend_from_slice(&[50, 4]);
            b.extend_from_slice(&ip);
        }
        if let Some(server) = server {
            b.extend_from_slice(&[54, 4]);
            b.extend_from_slice(&server);
        }
        b.extend_from_slice(&[12, 7]);
        b.extend_from_slice(b"sbc-emu");
        b.extend_from_slice(&[55, 4, 1, 3, 6, 51]);
        b.push(255);
        b.resize(300, 0);
        let (any, all) = ([0u8; 4], [255u8; 4]);
        let udp = udp_packet(&any, &all, 68, 67, &b);
        ethernet(&BROADCAST, &self.mac, 0x0800, &ip_packet(17, &any, &all, &udp))
    }

    fn on_dhcp(&mut self, d: &[u8], src: &[u8; 6], now_us: u64) -> Vec<Vec<u8>> {
        if d.len() < 240 || d[0] != 2 || d[236..240] != DHCP_MAGIC || d[4..8] != self.xid.to_be_bytes()
            || d[28..34] != self.mac
        {
            return Vec::new();
        }
        let yiaddr: [u8; 4] = d[16..20].try_into().unwrap();
        let (mut kind, mut server, mut mask, mut lease) = (0, None, None, 3600);
        let mut i = 240;
        while i + 1 < d.len() && d[i] != 255 {
            if d[i] == 0 {
                i += 1;
                continue;
            }
            let len = usize::from(d[i + 1]);
            let Some(v) = d.get(i + 2..i + 2 + len) else { break };
            match (d[i], len) {
                (53, 1) => kind = v[0],
                (54, 4) => server = Some([v[0], v[1], v[2], v[3]]),
                (1, 4) => mask = Some([v[0], v[1], v[2], v[3]]),
                (51, 4) => lease = u32::from_be_bytes([v[0], v[1], v[2], v[3]]),
                _ => {}
            }
            i += 2 + len;
        }
        let server = server.unwrap_or(self.board_ip);
        match (kind, self.dhcp) {
            (2, Dhcp::Selecting(_)) => {
                self.dhcp = Dhcp::Requesting(yiaddr, server, now_us);
                vec![self.dhcp_message(3, Some(yiaddr), Some(server))]
            }
            (5, Dhcp::Requesting(..)) => {
                let fresh = self.ip != Some(yiaddr);
                self.ip = Some(yiaddr);
                self.server = Some(server);
                self.mask = mask.unwrap_or(self.mask);
                if server == self.board_ip {
                    self.board_mac = Some(*src);
                }
                self.dhcp = Dhcp::Bound(now_us, lease);
                if fresh {
                    self.leases += 1;
                    eprintln!("[usb-device] DHCP lease {} from {} ({lease} s)", dotted(&yiaddr), dotted(&server));
                }
                Vec::new()
            }
            (6, _) => {
                // NAK: start over
                self.ip = None;
                self.xid = self.xid.wrapping_add(1);
                self.dhcp = Dhcp::Selecting(now_us);
                vec![self.dhcp_message(1, None, None)]
            }
            _ => Vec::new(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const BOARD: [u8; 4] = [192, 168, 55, 1];
    const BOARD_MAC: [u8; 6] = [0x6a, 0x27, 0x5e, 0xab, 0xe9, 0xa1];

    /// The board's DHCP server's reply of `kind` to the client's last request.
    fn reply(client: &Client, request: &[u8], kind: u8) -> Vec<u8> {
        let d = &request[14 + 20 + 8..];
        let mut b = vec![0u8; 240];
        b[0] = 2;
        b[1] = 1;
        b[2] = 6;
        b[4..8].copy_from_slice(&d[4..8]);
        b[16..20].copy_from_slice(&[192, 168, 55, 100]);
        b[28..34].copy_from_slice(&client.mac);
        b[236..240].copy_from_slice(&DHCP_MAGIC);
        b.extend_from_slice(&[53, 1, kind, 54, 4, 192, 168, 55, 1, 51, 4, 0, 0, 0, 15, 1, 4, 255, 255, 255, 0, 255]);
        let udp = udp_packet(&BOARD, &[255; 4], 67, 68, &b);
        ethernet(&BROADCAST, &BOARD_MAC, 0x0800, &ip_packet(17, &BOARD, &[255; 4], &udp))
    }

    fn dhcp_kind(frame: &[u8]) -> u8 {
        let d = &frame[14 + 20 + 8..];
        assert_eq!(&d[236..240], &DHCP_MAGIC);
        assert_eq!(&d[240..242], &[53, 1]);
        d[242]
    }

    #[test]
    fn takes_a_lease_renews_it_and_answers_arp_and_ping() {
        let mut c = Client::new(BOARD, false);
        assert!(c.handle(&[0; 60], 0).is_empty(), "nothing while the link is down");
        let discover = c.link_up(0);
        assert_eq!(dhcp_kind(&discover[0]), 1);
        // no offer: DISCOVER again after the retry time
        assert!(c.poll(1_000_000).is_empty());
        let discover = c.poll(DHCP_RETRY_US);
        assert_eq!(dhcp_kind(&discover[0]), 1);
        let request = c.handle(&reply(&c, &discover[0], 2), DHCP_RETRY_US);
        assert_eq!(dhcp_kind(&request[0]), 3);
        assert!(c.handle(&reply(&c, &request[0], 5), DHCP_RETRY_US).is_empty());
        assert!(c.bound());
        assert_eq!(c.ip, Some([192, 168, 55, 100]));
        assert_eq!(c.board_mac, Some(BOARD_MAC));
        // renewed halfway through the 15 s lease
        let renew = c.poll(DHCP_RETRY_US + 7_500_000);
        assert_eq!(dhcp_kind(&renew[0]), 3);
        assert!(c.handle(&reply(&c, &renew[0], 5), DHCP_RETRY_US + 7_500_000).is_empty());
        assert_eq!(c.leases, 1);

        // ARP who-has 192.168.55.100 from the board
        let mut arp = vec![0, 1, 8, 0, 6, 4, 0, 1];
        arp.extend_from_slice(&BOARD_MAC);
        arp.extend_from_slice(&BOARD);
        arp.extend_from_slice(&[0; 6]);
        arp.extend_from_slice(&[192, 168, 55, 100]);
        let r = c.handle(&ethernet(&BROADCAST, &BOARD_MAC, 0x0806, &arp), 0);
        assert_eq!(r.len(), 1);
        assert_eq!(&r[0][0..6], &BOARD_MAC);
        assert_eq!(&r[0][14 + 6..14 + 8], &[0, 2]);
        assert_eq!(&r[0][14 + 14..14 + 18], &[192, 168, 55, 100]);

        // a ping
        let mut icmp = vec![8, 0, 0, 0, 0, 1, 0, 1, 0xaa];
        let s = checksum(&icmp, 0);
        icmp[2..4].copy_from_slice(&s.to_be_bytes());
        let ping = ethernet(&c.mac.clone(), &BOARD_MAC, 0x0800, &ip_packet(1, &BOARD, &[192, 168, 55, 100], &icmp));
        let r = c.handle(&ping, 0);
        assert_eq!(r.len(), 1);
        assert_eq!(r[0][14 + 20], 0);
        assert_eq!(checksum(&r[0][14 + 20..], 0), 0);

        // a connection from the board is refused
        let mut syn = vec![0x9c, 0x40, 0, 22, 0, 0, 0, 7, 0, 0, 0, 0, 0x50, 0x02, 0x20, 0, 0, 0, 0, 0];
        let s = transport_checksum(&BOARD, &[192, 168, 55, 100], 6, &syn);
        syn[16..18].copy_from_slice(&s.to_be_bytes());
        let r = c.handle(&ethernet(&c.mac.clone(), &BOARD_MAC, 0x0800, &ip_packet(6, &BOARD, &[192, 168, 55, 100], &syn)), 0);
        assert_eq!(r.len(), 1);
        assert_eq!(r[0][14 + 20 + 13], 0x14);
        assert_eq!(c.refused, 1);
    }

    #[test]
    fn asks_for_the_boards_mac_while_forwards_wait() {
        let mut c = Client::new(BOARD, false);
        let d = c.link_up(0);
        let r = c.handle(&reply(&c, &d[0], 2), 0);
        // the ACK comes from a relay MAC: the board's own is still unknown
        let mut ack = reply(&c, &r[0], 5);
        ack[6..12].copy_from_slice(&[2; 6]);
        let mut ack_ip = ack[14..].to_vec();
        ack_ip[12..16].copy_from_slice(&[192, 168, 55, 2]);
        let s = checksum(&{ let mut h = ack_ip[..20].to_vec(); h[10] = 0; h[11] = 0; h }, 0);
        ack_ip[10..12].copy_from_slice(&s.to_be_bytes());
        ack.truncate(14);
        ack.extend_from_slice(&ack_ip);
        c.handle(&ack, 0);
        c.board_mac = None;
        let out = c.poll(1_000_000);
        assert_eq!(out.len(), 1);
        assert_eq!(be16(&out[0][12..14]), 0x0806);
        assert_eq!(&out[0][14 + 24..14 + 28], &BOARD);
    }
}
