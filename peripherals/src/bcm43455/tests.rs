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
