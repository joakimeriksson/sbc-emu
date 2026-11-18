use super::*;

fn window(b: &mut Bcm43455, addr: u32) {
    let v = (addr & 0xffff_8000) >> 8;
    for i in 0..3 {
        b.write(1, F1_SBADDRLOW + i, (v >> (8 * i)) as u8);
    }
}

fn readl(b: &mut Bcm43455, addr: u32) -> u32 {
    window(b, addr);
    let mut w = [0; 4];
    b.read_block(1, (addr & 0x7fff) | ACCESS_4B, true, &mut w);
    u32::from_le_bytes(w)
}

fn writel(b: &mut Bcm43455, addr: u32, v: u32) {
    window(b, addr);
    b.write_block(1, (addr & 0x7fff) | ACCESS_4B, true, &v.to_le_bytes());
}

/// A chip whose firmware runs, interrupts routed out of band.
fn started() -> Bcm43455 {
    let mut b = Bcm43455::new([2, 0, 0, 0, 0, 1]);
    let arm_wrap = ARM_BASE + WRAP;
    writel(&mut b, arm_wrap + BCMA_IOCTL, IOCTL_CPUHALT | IOCTL_CLK);
    writel(&mut b, arm_wrap + BCMA_RESET_CTL, 0);
    writel(&mut b, arm_wrap + BCMA_IOCTL, IOCTL_CLK);
    assert!(b.running());
    b
}

/// An SDPCM frame from the host on `channel`, sequence number `seq`.
fn send(b: &mut Bcm43455, seq: u8, channel: u8, payload: &[u8]) {
    let len = 12 + payload.len();
    let mut f = (len as u16).to_le_bytes().to_vec();
    f.extend_from_slice(&(!(len as u16)).to_le_bytes());
    f.extend_from_slice(&[seq, channel, 0, 12, 0, 0, 0, 0]);
    f.extend_from_slice(payload);
    f.resize(len.div_ceil(64) * 64, 0);
    b.write_block(2, 0x8000, true, &f);
}

/// A BCDC ioctl (a set when `set`), sequence number `seq`.
fn ioctl(b: &mut Bcm43455, seq: u8, cmd: u32, set: bool, data: &[u8]) {
    let mut p = Vec::new();
    for w in [cmd, data.len() as u32, if set { 2 } else { 0 } | (seq as u32) << 16, 0] {
        p.extend_from_slice(&w.to_le_bytes());
    }
    p.extend_from_slice(data);
    send(b, seq, 0, &p);
}

/// Every frame waiting for the host: (channel, payload).
fn drain(b: &mut Bcm43455) -> Vec<(u8, Vec<u8>)> {
    let mut frames = Vec::new();
    loop {
        let mut head = [0u8; 4];
        b.read_block(2, 0x8000, false, &mut head);
        let len = u16::from_le_bytes([head[0], head[1]]) as usize;
        if len == 0 {
            return frames;
        }
        let mut rest = vec![0u8; len - 4];
        b.read_block(2, 0x8000, false, &mut rest);
        let doff = rest[3] as usize;
        frames.push((rest[1] & 0x0f, rest[doff - 4..].to_vec()));
    }
}

/// The event codes among frames from the host's point of view.
fn events(frames: &[(u8, Vec<u8>)]) -> Vec<(u32, u32)> {
    frames
        .iter()
        .filter(|(c, p)| *c == 1 && p.len() >= 4 + 14 + 10 + 16)
        .map(|(_, p)| {
            let m = &p[4 + 14 + 10..];
            (u32::from_be_bytes(m[4..8].try_into().unwrap()), u32::from_be_bytes(m[8..12].try_into().unwrap()))
        })
        .collect()
}

/// A scan finds the access points; joining the open one carries Ethernet both ways.
#[test]
fn scan_join_and_data() {
    let mut b = started();
    let mut mask = b"event_msgs\0".to_vec();
    mask.extend_from_slice(&[0xff; 24]);
    ioctl(&mut b, 0, 263, true, &mask);
    ioctl(&mut b, 1, 2, true, &[0; 4]); // up
    let mut escan = b"escan\0".to_vec();
    escan.extend_from_slice(&[1, 0, 0, 0, 1, 0, 0x34, 0x12]); // version, action, sync ID
    ioctl(&mut b, 2, 263, true, &escan);
    let frames = drain(&mut b);
    assert_eq!(events(&frames), vec![(69, 8), (69, 8), (69, 0)], "two partial results, then done");
    let bss = &frames.iter().find(|(c, _)| *c == 1).unwrap().1[4 + 14 + 10 + 48..];
    assert_eq!(&bss[8..10], &0x1234u16.to_le_bytes(), "sync ID echoed");
    assert_eq!(&bss[12 + 19..12 + 26], b"sbc-emu");
    // join "sbc-emu": link up, then the SSID set
    let mut ssid = 7u32.to_le_bytes().to_vec();
    ssid.extend_from_slice(b"sbc-emu");
    ssid.resize(36, 0);
    ioctl(&mut b, 3, 26, true, &ssid);
    let frames = drain(&mut b);
    assert_eq!(events(&frames), vec![(16, 0), (0, 0)]);
    assert_eq!(b.joined(), Some("sbc-emu"));
    // a data frame out (BDC header, Ethernet), one in
    let eth = [[0xff; 6].as_slice(), &[2, 0, 0, 0, 0, 1], &[0x08, 0x06], &[0; 28]].concat();
    send(&mut b, 4, 2, &[[0x20, 0, 0, 0].as_slice(), &eth].concat());
    assert_eq!(b.take_transmitted(), vec![eth.clone()]);
    drain(&mut b); // the credit frame
    b.receive(&eth);
    assert_eq!(b.oob_level(), None, "in-band interrupts: no out-of-band level");
    let frames = drain(&mut b);
    assert_eq!(frames.len(), 1);
    assert_eq!((frames[0].0, &frames[0].1[4..]), (2, eth.as_slice()));
}

/// brcmfmac's attach, download and first ioctl, in its order.
#[test]
fn attach_download_and_an_iovar() {
    let mut b = Bcm43455::new([2, 0, 0, 0, 0, 1]);
    // clock request reads back with the availability bits
    b.write(1, F1_CHIPCLKCSR, 0x28);
    assert_eq!(b.read(1, F1_CHIPCLKCSR) & 0x3f, 0x28);
    assert_ne!(b.read(1, F1_CHIPCLKCSR) & CLK_ALP_AVAIL, 0);
    // chip ID and the enumeration ROM
    assert_eq!(readl(&mut b, CC_BASE) & 0xffff, 0x4345);
    let mut erom = readl(&mut b, CC_BASE + 0xfc);
    let mut cores = Vec::new();
    loop {
        let a = readl(&mut b, erom);
        if a & 0xf == 0xf {
            break;
        }
        let _b = readl(&mut b, erom + 4);
        let base = readl(&mut b, erom + 8) & !0xfff;
        let wrap = readl(&mut b, erom + 12) & !0xfff;
        cores.push((a >> 8 & 0xfff, base, wrap));
        erom += 16;
    }
    assert_eq!(cores[0], (CORE_CC, CC_BASE, CC_BASE + WRAP));
    assert!(cores.iter().any(|c| c.0 == CORE_ARM_CR4));
    // TCM size from the CR4's banks
    let cap = readl(&mut b, ARM_BASE + 4);
    let mut size = 0;
    for i in 0..(cap & 0xf) + (cap >> 4 & 0xf) {
        writel(&mut b, ARM_BASE + 0x40, i);
        size += ((readl(&mut b, ARM_BASE + 0x44) & 0x7f) + 1) * 8192;
    }
    assert_eq!(size, RAM_SIZE);
    // halt the CPU (set passive), download, release it (set active)
    let arm_wrap = ARM_BASE + WRAP;
    writel(&mut b, arm_wrap + BCMA_IOCTL, IOCTL_CPUHALT | IOCTL_CLK);
    writel(&mut b, arm_wrap + BCMA_RESET_CTL, 0);
    assert!(!b.running());
    window(&mut b, RAM_BASE);
    b.write_block(1, RAM_BASE & 0x7fff, true, &[0xde, 0xad, 0xbe, 0xef]);
    assert_eq!(readl(&mut b, RAM_BASE), 0xefbe_adde);
    writel(&mut b, arm_wrap + BCMA_RESET_CTL, 1);
    writel(&mut b, arm_wrap + BCMA_IOCTL, IOCTL_CLK);
    writel(&mut b, arm_wrap + BCMA_RESET_CTL, 0);
    assert!(b.running());
    assert_eq!(b.ready() & 0x04, 0x04, "F2 ready");
    // the mailbox says ready, the out-of-band line (active low) asserts once routed
    writel(&mut b, SDIOD_BASE + SD_HOSTINTMASK, 0xf0);
    assert_eq!(readl(&mut b, SDIOD_BASE + SD_TOHOSTMAILBOXDATA) >> 16, 4);
    assert_eq!(b.oob_level(), None);
    b.vendor_write(CCCR_SEPINT, SEPINT_MASK | SEPINT_OE);
    assert_eq!(b.oob_level(), Some(false));
    writel(&mut b, SDIOD_BASE + SD_INTSTATUS, 0xffff_ffff);
    assert_eq!(b.oob_level(), Some(true));
    // GET_VAR "cur_etheraddr" on the control channel
    let mut dcmd = Vec::new();
    for w in [262u32, 6, 0x0001_0000, 0] {
        dcmd.extend_from_slice(&w.to_le_bytes());
    }
    dcmd.extend_from_slice(b"cur_etheraddr\0");
    let len = 12 + dcmd.len();
    let mut f = (len as u16).to_le_bytes().to_vec();
    f.extend_from_slice(&(!(len as u16)).to_le_bytes());
    f.extend_from_slice(&[0xff, 0, 0, 12, 0, 0, 0, 0]);
    f.extend_from_slice(&dcmd);
    f.resize(512, 0);
    b.write_block(2, 0x8000, true, &f);
    assert_eq!(b.oob_level(), Some(false), "frame indication");
    let mut first = [0u8; 64];
    b.read_block(2, 0x8000, false, &mut first);
    let rlen = u16::from_le_bytes([first[0], first[1]]) as usize;
    assert_eq!(rlen, 12 + 16 + 6);
    assert_eq!(first[4], 0, "first sequence number");
    assert_eq!(first[9], 0xff_u8.wrapping_add(1 + 8), "credit window");
    assert_eq!(&first[12..16], &262u32.to_le_bytes());
    assert_eq!(u32::from_le_bytes(first[24..28].try_into().unwrap()), 0, "status");
    assert_eq!(&first[28..34], &[2, 0, 0, 0, 0, 1]);
    // nothing more: a null header
    b.read_block(2, 0x8000, false, &mut first);
    assert_eq!(&first[..4], &[0, 0, 0, 0]);
}
