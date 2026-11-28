//! The card as brcmfmac's PCIe bus layer and msgbuf drive it.
use super::*;

const HOST: u64 = 0x1000_0000;
const MSI_ADDR: u64 = 0xfee0_0000;

/// Host memory, with the MSI address caught.
struct Host {
    mem: Vec<u8>,
    msis: Vec<u32>,
}

impl Master for Host {
    fn read(&mut self, addr: u64, buf: &mut [u8]) -> bool {
        let o = (addr - HOST) as usize;
        buf.copy_from_slice(&self.mem[o..o + buf.len()]);
        true
    }
    fn write(&mut self, addr: u64, data: &[u8]) -> bool {
        if addr == MSI_ADDR {
            self.msis.push(u32::from_le_bytes(data.try_into().unwrap()));
            return true;
        }
        let o = (addr - HOST) as usize;
        self.mem[o..o + data.len()].copy_from_slice(data);
        true
    }
}

impl Host {
    fn put(&mut self, addr: u64, data: &[u8]) {
        let o = (addr - HOST) as usize;
        self.mem[o..o + data.len()].copy_from_slice(data);
    }
    fn get(&self, addr: u64, len: usize) -> &[u8] {
        let o = (addr - HOST) as usize;
        &self.mem[o..o + len]
    }
}

fn cfg_write(c: &mut Bcm4350, o: usize, v: u32, h: &mut Host) {
    c.config().write(o, v, 4);
    c.config_written(o, h);
}

fn bp_read(c: &mut Bcm4350, h: &mut Host, addr: u32) -> u32 {
    cfg_write(c, CFG_BAR0_WINDOW, addr & !0xfff, h);
    c.bar_read(0, (addr & 0xfff) as u64, 4, h) as u32
}

fn bp_write(c: &mut Bcm4350, h: &mut Host, addr: u32, v: u32) {
    cfg_write(c, CFG_BAR0_WINDOW, addr & !0xfff, h);
    c.bar_write(0, (addr & 0xfff) as u64, v as u64, 4, h);
}

fn tcm32(c: &mut Bcm4350, h: &mut Host, a: u32) -> u32 {
    c.bar_read(2, a as u64, 4, h) as u32
}

fn set_tcm(c: &mut Bcm4350, h: &mut Host, a: u32, v: u64, len: usize) {
    c.bar_write(2, a as u64, v, len, h);
}

/// Up to the running firmware, its common rings in host memory: (card, host, ring bases).
fn booted() -> (Bcm4350, Host, [u64; 5]) {
    let mut c = Bcm4350::new([2, 0, 0, 0, 0, 1]);
    let mut h = Host { mem: vec![0; 16 << 20], msis: Vec::new() };
    // BARs, memory and bus mastering, MSI
    for (o, v) in [(0x10, 0x4000_0000u32), (0x14, 0), (0x18, 0x4040_0000), (0x1c, 0)] {
        cfg_write(&mut c, o, v, &mut h);
    }
    cfg_write(&mut c, 0x04, 0x6, &mut h);
    assert_eq!(c.decode(0x4000_0010), Some((0, 0x10)));
    assert_eq!(c.decode(0x4058_0000), Some((2, 0x18_0000)));
    cfg_write(&mut c, CAP_MSI + 4, MSI_ADDR as u32, &mut h);
    cfg_write(&mut c, CAP_MSI + 8, 0, &mut h);
    cfg_write(&mut c, CAP_MSI + 12, 0x21, &mut h);
    cfg_write(&mut c, CAP_MSI, 0x0081_0000 | (CAP_EXP as u32) << 8 | 5, &mut h);

    // chip recognition: chip ID, EROM, the Cortex-R4's banks
    assert_eq!(bp_read(&mut c, &mut h, CC_BASE) & 0xffff, 0x4350);
    assert_eq!(bp_read(&mut c, &mut h, CC_BASE + 0xfc), EROM_BASE);
    assert_eq!(bp_read(&mut c, &mut h, EROM_BASE) >> 8 & 0xfff, CORE_CC);
    let cap = bp_read(&mut c, &mut h, ARM_BASE + 4);
    let mut ram = 0;
    for k in 0..(cap & 0xf) + (cap >> 4 & 0xf) {
        bp_write(&mut c, &mut h, ARM_BASE + 0x40, k);
        ram += ((bp_read(&mut c, &mut h, ARM_BASE + 0x44) & 0x7f) + 1) * 8192;
    }
    assert_eq!(ram, RAM_SIZE);

    // passive (halted), firmware into TCM, last word cleared, then active
    bp_write(&mut c, &mut h, ARM_BASE + WRAP + BCMA_IOCTL, IOCTL_CPUHALT | IOCTL_CLK);
    bp_write(&mut c, &mut h, ARM_BASE + WRAP + BCMA_RESET_CTL, 0);
    for k in 0..64u32 {
        set_tcm(&mut c, &mut h, RAM_BASE + 4 * k, 0xe59f_f000 + k as u64, 4);
    }
    set_tcm(&mut c, &mut h, RAM_BASE + RAM_SIZE - 4, 0, 4);
    assert!(!c.running());
    set_tcm(&mut c, &mut h, 0, 0xe59f_f000, 4);
    bp_write(&mut c, &mut h, ARM_BASE + WRAP + BCMA_IOCTL, IOCTL_CLK);
    assert!(c.running());
    let shared = tcm32(&mut c, &mut h, RAM_BASE + RAM_SIZE - 4);
    assert!((RAM_BASE..RAM_BASE + RAM_SIZE).contains(&shared));
    assert_eq!(tcm32(&mut c, &mut h, shared) & 0xff, 5);
    assert_eq!(tcm32(&mut c, &mut h, RAM_BASE), 0xe59f_f000, "firmware image intact");

    // common rings, as brcmf_pcie_init_ringbuffers sets them up
    let info = tcm32(&mut c, &mut h, shared + 48);
    let ringmem = tcm32(&mut c, &mut h, info);
    let sizes = [(64u16, 40u16), (1024, 32), (64, 24), (1024, 16), (1024, 32)];
    let mut bases = [0u64; 5];
    for (i, (items, len)) in sizes.into_iter().enumerate() {
        bases[i] = HOST + 0x10_0000 * (i as u64 + 1);
        let d = ringmem + 16 * i as u32;
        set_tcm(&mut c, &mut h, d + 4, items as u64, 2);
        set_tcm(&mut c, &mut h, d + 6, len as u64, 2);
        set_tcm(&mut c, &mut h, d + 8, bases[i], 8);
    }
    // the mailbox interrupt unmasked
    c.bar_write(0, 0x2000 + PCIE2_MAILBOXMASK as u64, 0xff_0300, 4, &mut h);
    (c, h, bases)
}

/// Write `items` into H2D ring `id` from index `at`, then ring the doorbell.
fn submit(c: &mut Bcm4350, h: &mut Host, base: u64, id: u32, len: u64, at: u16, items: &[Vec<u8>]) -> u16 {
    let mut w = at;
    for it in items {
        h.put(base + w as u64 * len, it);
        w += 1;
    }
    set_tcm(c, h, H2D_W + 4 * id, w as u64, 2);
    c.bar_write(0, 0x2000 + PCIE2_H2D_MAILBOX_0 as u64, 1, 4, h);
    w
}

fn post(kind: u8, id: u32, addr: u64, len: u16) -> Vec<u8> {
    let mut m = vec![0u8; 40];
    m[0] = kind;
    m[4..8].copy_from_slice(&id.to_le_bytes());
    m[8..10].copy_from_slice(&len.to_le_bytes());
    m[16..24].copy_from_slice(&addr.to_le_bytes());
    m
}

fn ioctl(cmd: u32, trans: u16, addr: u64, inlen: u16, outlen: u16) -> Vec<u8> {
    let mut m = vec![0u8; 40];
    m[0] = MSG_IOCTLPTR_REQ;
    m[4..8].copy_from_slice(&0xfffeu32.to_le_bytes());
    m[8..12].copy_from_slice(&cmd.to_le_bytes());
    m[12..14].copy_from_slice(&trans.to_le_bytes());
    m[14..16].copy_from_slice(&inlen.to_le_bytes());
    m[16..18].copy_from_slice(&outlen.to_le_bytes());
    m[24..32].copy_from_slice(&addr.to_le_bytes());
    m
}

#[test]
fn ioctl_round_trip_and_msi() {
    let (mut c, mut h, bases) = booted();
    let resp = HOST + 0x80_0000;
    let req = HOST + 0x90_0000;
    // a response buffer, then "cur_etheraddr"
    h.put(req, b"cur_etheraddr\0");
    let w = submit(&mut c, &mut h, bases[0], 0, 40, 0, &[post(MSG_IOCTLRESP_BUF_POST, 7, resp, 8192), ioctl(262, 1, req, 14, 6)]);
    assert_eq!(w, 2);
    assert_eq!(c.tcm16(H2D_R), 2, "both items consumed");
    // one control completion: IOCTL_CMPLT on buffer 7 with 6 bytes
    assert_eq!(c.tcm16(D2H_W), 1);
    let item = h.get(bases[2], 24).to_vec();
    assert_eq!(item[0], MSG_IOCTL_CMPLT);
    assert_eq!(u32::from_le_bytes(item[4..8].try_into().unwrap()), 7);
    assert_eq!(u16::from_le_bytes([item[12], item[13]]), 6);
    assert_eq!(h.get(resp, 6), &[2, 0, 0, 0, 0, 1]);
    // the MSI went out once; masking and unmasking with the interrupt still pending sends it again
    assert_eq!(h.msis, vec![0x21]);
    c.bar_write(0, 0x2000 + PCIE2_MAILBOXMASK as u64, 0, 4, &mut h);
    c.bar_write(0, 0x2000 + PCIE2_MAILBOXMASK as u64, 0xff_0300, 4, &mut h);
    assert_eq!(h.msis.len(), 2);
    c.bar_write(0, 0x2000 + PCIE2_MAILBOXINT as u64, 0xffff_ffff, 4, &mut h);
    assert_eq!(c.bar_read(0, 0x2000 + PCIE2_MAILBOXINT as u64, 4, &mut h), 0);
}

#[test]
fn scan_events_and_data_through_flow_ring() {
    let (mut c, mut h, bases) = booted();
    let req = HOST + 0x90_0000;
    let mut posts = Vec::new();
    for k in 0..4u32 {
        posts.push(post(MSG_EVENT_BUF_POST, 100 + k, HOST + 0xa0_0000 + 0x2000 * k as u64, 8192));
        posts.push(post(MSG_IOCTLRESP_BUF_POST, 200 + k, HOST + 0xb0_0000 + 0x2000 * k as u64, 8192));
    }
    let mut w = submit(&mut c, &mut h, bases[0], 0, 40, 0, &posts);
    // enable all events, bring the interface up, and escan
    let mut ev = b"event_msgs\0".to_vec();
    ev.extend_from_slice(&[0xff; 18]);
    h.put(req, &ev);
    w = submit(&mut c, &mut h, bases[0], 0, 40, w, &[ioctl(263, 1, req, ev.len() as u16, ev.len() as u16)]);
    w = submit(&mut c, &mut h, bases[0], 0, 40, w, &[ioctl(2, 2, req, 4, 4)]);
    let mut escan = b"escan\0".to_vec();
    escan.extend_from_slice(&[1, 0, 0, 0, 1, 0, 0x34, 0x12]);
    escan.resize(80, 0);
    h.put(req, &escan);
    submit(&mut c, &mut h, bases[0], 0, 40, w, &[ioctl(263, 3, req, 80, 80)]);
    // the results come when the scan has run its time
    let answered = c.tcm16(D2H_W);
    assert_eq!(c.next_event(), Some(SCAN_NS));
    c.tick(SCAN_NS - 1, &mut h);
    assert_eq!(c.tcm16(D2H_W), answered);
    c.tick(SCAN_NS, &mut h);
    // three answers, then the escan's events: a result per network and the completion
    let n = c.tcm16(D2H_W) as usize;
    let kinds: Vec<u8> = (0..n).map(|i| h.get(bases[2] + 24 * i as u64, 1)[0]).collect();
    assert_eq!(kinds[..3], [MSG_IOCTL_CMPLT; 3]);
    assert_eq!(kinds[3..].iter().filter(|&&k| k == MSG_WL_EVENT).count(), Network::defaults().len() + 1);
    // the first event: an Ethernet frame of type 0x886c in event buffer 100
    let e = h.get(bases[2] + 24 * 3, 24).to_vec();
    assert_eq!(u32::from_le_bytes(e[4..8].try_into().unwrap()), 100);
    assert_eq!(h.get(HOST + 0xa0_0000 + 12, 2), &[0x88, 0x6c]);

    // join the open network, then a flow ring for the station's frames
    let mut join = vec![0u8; 40];
    join[..4].copy_from_slice(&7u32.to_le_bytes());
    join[4..11].copy_from_slice(b"sbc-emu");
    h.put(req, &join);
    let wc = c.tcm16(H2D_W);
    let wc = submit(&mut c, &mut h, bases[0], 0, 40, wc, &[ioctl(26, 4, req, 40, 40)]);
    assert_eq!(c.joined(), Some("sbc-emu"));
    let flow = HOST + 0xc0_0000;
    let mut create = vec![0u8; 40];
    create[0] = MSG_FLOW_RING_CREATE;
    create[22..24].copy_from_slice(&2u16.to_le_bytes());
    create[28..30].copy_from_slice(&512u16.to_le_bytes());
    create[30..32].copy_from_slice(&48u16.to_le_bytes());
    create[32..40].copy_from_slice(&flow.to_le_bytes());
    submit(&mut c, &mut h, bases[0], 0, 40, wc, &[create]);
    // a frame on it: header in the message, payload in a buffer
    let payload = HOST + 0xd0_0000;
    h.put(payload, b"\x08\x00hello");
    let mut tx = vec![0u8; 48];
    tx[0] = MSG_TX_POST;
    tx[4..8].copy_from_slice(&9u32.to_le_bytes());
    tx[8..14].copy_from_slice(&[0xff; 6]);
    tx[14..20].copy_from_slice(&[2, 0, 0, 0, 0, 1]);
    tx[20..22].copy_from_slice(&[0x08, 0x06]);
    tx[32..40].copy_from_slice(&payload.to_le_bytes());
    tx[42..44].copy_from_slice(&7u16.to_le_bytes());
    submit(&mut c, &mut h, flow, 2, 48, 0, &[tx]);
    let sent = c.take_transmitted();
    assert_eq!(sent.len(), 1);
    assert_eq!(&sent[0][12..], b"\x08\x06\x08\x00hello");
    // its transmit status
    assert_eq!(c.tcm16(D2H_W + 4), 1);
    assert_eq!(h.get(bases[3], 1)[0], MSG_TX_STATUS);
    assert_eq!(u32::from_le_bytes(h.get(bases[3] + 4, 4).try_into().unwrap()), 9);

    // a frame from the network into a posted receive buffer
    submit(&mut c, &mut h, bases[1], 1, 32, 0, &[post(MSG_RXBUF_POST, 300, 0, 0)].map(|mut p| {
        p[10..12].copy_from_slice(&2048u16.to_le_bytes());
        p[24..32].copy_from_slice(&(HOST + 0xe0_0000).to_le_bytes());
        p.truncate(32);
        p
    }));
    let frame = [[2, 0, 0, 0, 0, 1].as_slice(), &[0x02, 0x5b, 0xc0, 0xa9, 0, 1], &[0x08, 0x00], b"payload"].concat();
    c.receive(&frame, &mut h);
    assert_eq!(c.tcm16(D2H_W + 8), 1);
    let rx = h.get(bases[4], 32).to_vec();
    assert_eq!(rx[0], MSG_RX_CMPLT);
    assert_eq!(u16::from_le_bytes([rx[14], rx[15]]) as usize, frame.len());
    assert_eq!(h.get(HOST + 0xe0_0000, frame.len()), frame.as_slice());
}

#[test]
fn soft_ap_starts_without_joining() {
    let (mut c, mut h, bases) = booted();
    let req = HOST + 0x90_0000;
    let posts: Vec<_> = (0..4).map(|k| post(MSG_IOCTLRESP_BUF_POST, k, HOST + 0xb0_0000 + 0x2000 * k as u64, 8192)).collect();
    let mut w = submit(&mut c, &mut h, bases[0], 0, 40, 0, &posts);
    h.put(req, &1u32.to_le_bytes());
    w = submit(&mut c, &mut h, bases[0], 0, 40, w, &[ioctl(118, 1, req, 4, 4), ioctl(2, 2, req, 4, 4)]);
    let mut ssid = vec![0u8; 40];
    ssid[..4].copy_from_slice(&11u32.to_le_bytes());
    ssid[4..15].copy_from_slice(b"AccessPopup");
    h.put(req, &ssid);
    submit(&mut c, &mut h, bases[0], 0, 40, w, &[ioctl(26, 3, req, 40, 40)]);
    assert_eq!(c.soft_ap().as_deref(), Some("AccessPopup"));
    assert_eq!(c.joined(), None);
}

#[test]
fn watchdog_stops_the_firmware() {
    let (mut c, mut h, _) = booted();
    bp_write(&mut c, &mut h, CC_BASE + CC_WATCHDOG, 4);
    assert!(!c.running());
}

#[test]
fn scans_while_a_soft_ap() {
    let (mut c, mut h, bases) = booted();
    let req = HOST + 0x90_0000;
    let mut posts: Vec<_> = (0..8).map(|k| post(MSG_IOCTLRESP_BUF_POST, k, HOST + 0xb0_0000 + 0x2000 * k as u64, 8192)).collect();
    posts.extend((0..8).map(|k| post(MSG_EVENT_BUF_POST, 100 + k, HOST + 0xa0_0000 + 0x2000 * k as u64, 8192)));
    let mut w = submit(&mut c, &mut h, bases[0], 0, 40, 0, &posts);
    let mut ev = b"event_msgs\0".to_vec();
    ev.extend_from_slice(&[0xff; 18]);
    h.put(req, &ev);
    w = submit(&mut c, &mut h, bases[0], 0, 40, w, &[ioctl(263, 1, req, ev.len() as u16, ev.len() as u16)]);
    h.put(req, &1u32.to_le_bytes());
    w = submit(&mut c, &mut h, bases[0], 0, 40, w, &[ioctl(118, 2, req, 4, 4), ioctl(2, 3, req, 4, 4)]);
    let mut ssid = vec![0u8; 40];
    ssid[..4].copy_from_slice(&11u32.to_le_bytes());
    ssid[4..15].copy_from_slice(b"AccessPopup");
    h.put(req, &ssid);
    w = submit(&mut c, &mut h, bases[0], 0, 40, w, &[ioctl(26, 4, req, 40, 40)]);
    let before = c.tcm16(D2H_W);
    let mut escan = b"escan\0".to_vec();
    escan.extend_from_slice(&[1, 0, 0, 0, 1, 0, 0x34, 0x12]);
    escan.resize(142, 0);
    h.put(req, &escan);
    submit(&mut c, &mut h, bases[0], 0, 40, w, &[ioctl(263, 5, req, 142, 142)]);
    c.tick(SCAN_NS, &mut h);
    let after = c.tcm16(D2H_W);
    let kinds: Vec<u8> = (before..after).map(|i| h.get(bases[2] + 24 * i as u64, 1)[0]).collect();
    assert_eq!(kinds.iter().filter(|&&k| k == MSG_WL_EVENT).count(), Network::defaults().len() + 1, "{kinds:?}");
}
