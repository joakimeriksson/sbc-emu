//! HDMI0 as the boot firmware leaves it: one XRGB8888 framebuffer at the monitor's preferred
//! mode, handed to Linux as a `simple-framebuffer` (driven by `simpledrm`) until `rockchipdrm`
//! takes the display over through VOP2 (`crate::vop2`). Also the emulated monitor: its modes
//! and EDID.

/// A monitor mode (`--hdmi-mode WxH`): a CEA-861 timing at 60 Hz with positive syncs.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Mode {
    pub width: u32,
    pub height: u32,
    pub clock_khz: u32,
    pub hfp: u32,
    pub hsync: u32,
    pub hbp: u32,
    pub vfp: u32,
    pub vsync: u32,
    pub vbp: u32,
    /// the CEA-861 video identification code
    pub vic: u8,
}
impl Mode {
    /// The modes the emulated monitor offers; the first is the default.
    pub const MODES: [Mode; 3] = [
        Mode { width: 1280, height: 720, clock_khz: 74_250, hfp: 110, hsync: 40, hbp: 220, vfp: 5, vsync: 5, vbp: 20, vic: 4 },
        Mode { width: 1920, height: 1080, clock_khz: 148_500, hfp: 88, hsync: 44, hbp: 148, vfp: 4, vsync: 5, vbp: 36, vic: 16 },
        Mode { width: 640, height: 480, clock_khz: 25_175, hfp: 16, hsync: 96, hbp: 48, vfp: 10, vsync: 2, vbp: 33, vic: 1 },
    ];
    pub fn find(width: u32, height: u32) -> Option<Mode> { Self::MODES.iter().copied().find(|m| m.width == width && m.height == height) }
    pub fn stride(&self) -> u32 { self.width * 4 }

    /// The EDID detailed timing descriptor.
    fn descriptor(&self) -> [u8; 18] {
        let hblank = self.hfp + self.hsync + self.hbp;
        let vblank = self.vfp + self.vsync + self.vbp;
        let (hmm, vmm) = (self.width * 520 / 1920, self.height * 290 / 1080);
        let clock = (self.clock_khz / 10) as u16;
        [
            clock as u8, (clock >> 8) as u8,
            self.width as u8, hblank as u8, ((self.width >> 8) << 4 | hblank >> 8) as u8,
            self.height as u8, vblank as u8, ((self.height >> 8) << 4 | vblank >> 8) as u8,
            self.hfp as u8, self.hsync as u8, ((self.vfp & 0xf) << 4 | self.vsync & 0xf) as u8,
            ((self.hfp >> 8) << 6 | (self.hsync >> 8) << 4 | (self.vfp >> 4) << 2 | self.vsync >> 4) as u8,
            hmm as u8, vmm as u8, ((hmm >> 8) << 4 | vmm >> 8) as u8,
            0, 0,
            0x1e, // digital separate sync, positive polarities
        ]
    }
}

/// The emulated monitor's EDID (256 bytes): an EDID 1.3 block whose preferred timing is
/// `pref`, and a CEA-861 extension that makes it an HDMI sink (the HDMI vendor block) with
/// basic audio (2-channel LPCM at 32, 44.1 and 48 kHz) and the other modes as VICs.
pub fn edid(pref: &Mode) -> Vec<u8> {
    let mut e = vec![0u8; 256];
    e[..8].copy_from_slice(&[0, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0]);
    let id = |c: u8| (c - b'A' + 1) as u16;
    let maker = id(b'S') << 10 | id(b'B') << 5 | id(b'E');
    e[8..10].copy_from_slice(&maker.to_be_bytes());
    e[10..12].copy_from_slice(&5u16.to_le_bytes());
    e[16] = 1;
    e[17] = (2026 - 1990) as u8;
    e[18..20].copy_from_slice(&[1, 3]);
    e[20] = 0x80; // digital
    e[21] = 52;
    e[22] = 29;
    e[23] = 120;
    e[24] = 0x0a; // RGB, preferred timing first
    e[25..35].copy_from_slice(&[0xee, 0x91, 0xa3, 0x54, 0x4c, 0x99, 0x26, 0x0f, 0x50, 0x54]);
    e[35] = 0x21; // 640x480@60, 800x600@60
    e[36] = 0x08; // 1024x768@60
    for k in 0..8 {
        e[38 + 2 * k] = 1;
        e[39 + 2 * k] = 1;
    }
    e[54..72].copy_from_slice(&pref.descriptor());
    e[72..77].copy_from_slice(&[0, 0, 0, 0xfc, 0]);
    let name = b"sbc-emu HDMI\n";
    e[77..77 + name.len()].copy_from_slice(name);
    e[90..108].copy_from_slice(&[0, 0, 0, 0xfd, 0, 50, 75, 30, 83, 17, 0, 0x0a, 0x20, 0x20, 0x20, 0x20, 0x20, 0x20]);
    e[108..113].copy_from_slice(&[0, 0, 0, 0x10, 0]);
    e[126] = 1; // one extension
    let c = &mut e[128..];
    c[0] = 0x02; // CEA-861
    c[1] = 3;
    let mut blocks = vec![0x40 | 3u8, 0x80 | pref.vic];
    blocks.extend(Mode::MODES.iter().filter(|m| m.vic != pref.vic).map(|m| m.vic));
    blocks.extend_from_slice(&[0x20 | 3, 0x09, 0x07, 0x07]); // audio: LPCM 2ch, 32/44.1/48 kHz, 16/20/24 bit
    blocks.extend_from_slice(&[0x80 | 3, 0x01, 0x00, 0x00]); // speakers: front left/right
    blocks.extend_from_slice(&[0x60 | 5, 0x03, 0x0c, 0x00, 0x10, 0x00]); // HDMI, physical address 1.0.0.0
    c[2] = (4 + blocks.len()) as u8;
    c[3] = 0x40 | 1; // basic audio, one native DTD
    c[4..4 + blocks.len()].copy_from_slice(&blocks);
    let d = 4 + blocks.len();
    c[d..d + 18].copy_from_slice(&pref.descriptor());
    for block in e.chunks_mut(128) {
        let sum = block[..127].iter().fold(0u8, |s, &b| s.wrapping_add(b));
        block[127] = 0u8.wrapping_sub(sum);
    }
    e
}

/// The firmware framebuffer's slot at the top of RAM (room for 1920x1080 XRGB8888).
pub const FB_SLOT: u64 = 16 << 20;
/// Where the framebuffer goes for `ram_bytes` of RAM.
pub fn framebuffer_addr(ram_bytes: u64) -> u64 { ram_bytes - FB_SLOT }

#[derive(Default)]
pub struct Display {
    /// The framebuffer handed over (address, mode); None: HDMI off.
    pub scanout: Option<(u64, Mode)>,
    /// Frames rendered for a viewer (VNC, screenshots).
    pub frames: std::cell::Cell<u64>,
}
impl Display {
    pub fn new(addr: u64, mode: Mode) -> Self { Self { scanout: Some((addr, mode)), frames: Default::default() } }
    pub fn shown_size(&self) -> Option<(u32, u32)> { self.scanout.map(|(_, m)| (m.width, m.height)) }
    /// The picture as RGBA.
    pub fn render(&self, ram: &[u8], out: &mut Vec<u8>) -> Option<(u32, u32)> {
        let (addr, mode) = self.scanout?;
        let (w, h) = (mode.width as usize, mode.height as usize);
        out.clear();
        out.resize(w * h * 4, 0);
        let start = addr as usize;
        let src = ram.get(start..start + w * h * 4)?;
        for (d, s) in out.chunks_exact_mut(4).zip(src.chunks_exact(4)) {
            // XRGB8888 little-endian: B, G, R, X in memory
            d.copy_from_slice(&[s[2], s[1], s[0], 255]);
        }
        self.frames.set(self.frames.get() + 1);
        Some((mode.width, mode.height))
    }
}

emu_core::snap_struct!(Mode { width, height, clock_khz, hfp, hsync, hbp, vfp, vsync, vbp, vic });
emu_core::snap_struct!(Display { scanout, frames });

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn renders_xrgb8888_from_ram() {
        let mut ram = vec![0u8; 32 << 20];
        let addr = framebuffer_addr(ram.len() as u64);
        let d = Display::new(addr, Mode::find(640, 480).unwrap());
        let px = addr as usize + (10 * 640 + 3) * 4;
        ram[px..px + 4].copy_from_slice(&0x00_12_34_56u32.to_le_bytes());
        let mut out = Vec::new();
        assert_eq!(d.render(&ram, &mut out), Some((640, 480)));
        assert_eq!(&out[(10 * 640 + 3) * 4..][..4], &[0x12, 0x34, 0x56, 255]);
        assert_eq!(d.frames.get(), 1);
        assert_eq!(Display::default().render(&ram, &mut out), None, "HDMI off");
    }

    #[test]
    fn edid_is_an_hdmi_sink_with_audio() {
        let e = edid(&Mode::MODES[0]);
        assert_eq!(e.len(), 256);
        assert!(e.chunks(128).all(|b| b.iter().fold(0u8, |s, &x| s.wrapping_add(x)) == 0), "checksums");
        assert_eq!((e[54], e[55]), (0x01, 0x1d), "74.25 MHz preferred");
        assert_eq!(e[128 + 5], 0x80 | 4, "native VIC 4");
        assert!(e[128..].windows(4).any(|w| w == [0x65, 0x03, 0x0c, 0x00]), "HDMI vendor block");
    }
}
