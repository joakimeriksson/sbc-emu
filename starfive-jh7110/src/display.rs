//! The JH7110 display path to the Orange Pi RV's HDMI port, as StarFive's 5.15 kernel drives
//! it (`drivers/gpu/drm/verisilicon`): the Verisilicon DC8200 display controller at
//! 0x29400000 and the Innosilicon HDMI transmitter at 0x29590000.
//!
//! DC8200: identification (revision 0x5720), register storage for the plane, panel and
//! colour registers the driver programs, a 60 Hz vblank for each enabled panel (the
//! interrupt the driver acknowledges through AQ_INTR_ACKNOWLEDGE), and a scanout of the two
//! primary planes in the RGB formats Linux's fbdev and KMS clients use.
//! Inno HDMI: byte registers on a 4-byte stride, a connected monitor (HDMI_STATUS hot-plug),
//! PLLs that lock at once, and the EDID read through the transmitter's DDC FIFO.
//! The monitor and its EDID follow sbc-emu's H3 display model (`allwinner-h3/src/display.rs`).

pub const DC_BASE: u64 = 0x2940_0000;
pub const DC_SIZE: u64 = 0x2800;
pub const HDMI_BASE: u64 = 0x2959_0000;
pub const HDMI_SIZE: u64 = 0x4000;
pub const IRQ_DC: usize = 95;
pub const IRQ_HDMI: usize = 99;

// DC8200 "HI" (AHB) registers
const AQ_INTR_ACKNOWLEDGE: u64 = 0x10;
const AQ_INTR_ENBL: u64 = 0x14;
const DC_HW_REVISION: u64 = 0x24;
const DC_HW_CHIP_CID: u64 = 0x30;
// display registers (offsets from DC_BASE)
const FB_ADDRESS: u64 = 0x1400;
const FB_STRIDE: u64 = 0x1408;
const DISPLAY_PANEL_CONFIG: u64 = 0x1418;
const DISPLAY_H: u64 = 0x1430;
const DISPLAY_V: u64 = 0x1440;
const FB_CONFIG: u64 = 0x1518;
const FB_SIZE: u64 = 0x1810;
const FB_CONFIG_EX: u64 = 0x1cc0;

// Inno HDMI registers (byte offsets before the 4-byte stride)
const HDMI_EDID_SEGMENT_POINTER: usize = 0x4d;
const HDMI_EDID_WORD_ADDR: usize = 0x4e;
const HDMI_EDID_FIFO_OFFSET: usize = 0x4f;
const HDMI_EDID_FIFO_ADDR: usize = 0x50;
const HDMI_INTERRUPT_MASK1: usize = 0xc0;
const HDMI_INTERRUPT_STATUS1: usize = 0xc1;
const HDMI_STATUS: usize = 0xc8;
const M_INT_EDID_READY: u8 = 1 << 2;
const M_HOTPLUG: u8 = 1 << 7;

/// A display timing, as an EDID detailed timing descriptor describes it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Mode {
    pub clock_khz: u32,
    pub width: u32,
    pub hfp: u32,
    pub hsync: u32,
    pub hbp: u32,
    pub height: u32,
    pub vfp: u32,
    pub vsync: u32,
    pub vbp: u32,
}

impl Mode {
    /// The monitor modes offered as the EDID's preferred timing (`--hdmi-mode WxH`), CEA-861
    /// timings with positive syncs.
    pub const MODES: [Mode; 3] = [
        Mode { clock_khz: 74_250, width: 1280, hfp: 110, hsync: 40, hbp: 220, height: 720, vfp: 5, vsync: 5, vbp: 20 },
        Mode { clock_khz: 148_500, width: 1920, hfp: 88, hsync: 44, hbp: 148, height: 1080, vfp: 4, vsync: 5, vbp: 36 },
        Mode { clock_khz: 25_175, width: 640, hfp: 16, hsync: 96, hbp: 48, height: 480, vfp: 10, vsync: 2, vbp: 33 },
    ];

    pub fn find(width: u32, height: u32) -> Option<Mode> {
        Self::MODES.iter().copied().find(|m| m.width == width && m.height == height)
    }

    fn descriptor(&self) -> [u8; 18] {
        let hblank = self.hfp + self.hsync + self.hbp;
        let vblank = self.vfp + self.vsync + self.vbp;
        let (hmm, vmm) = (self.width * 520 / 1920, self.height * 290 / 1080);
        let clock = (self.clock_khz / 10) as u16;
        [
            clock as u8,
            (clock >> 8) as u8,
            self.width as u8,
            hblank as u8,
            ((self.width >> 8) << 4 | hblank >> 8) as u8,
            self.height as u8,
            vblank as u8,
            ((self.height >> 8) << 4 | vblank >> 8) as u8,
            self.hfp as u8,
            self.hsync as u8,
            ((self.vfp & 0xf) << 4 | self.vsync & 0xf) as u8,
            ((self.hfp >> 8) << 6 | (self.hsync >> 8) << 4 | (self.vfp >> 4) << 2 | self.vsync >> 4) as u8,
            hmm as u8,
            vmm as u8,
            ((hmm >> 8) << 4 | vmm >> 8) as u8,
            0,
            0,
            0x1e, // digital separate sync, positive polarities
        ]
    }
}

/// A 128-byte EDID 1.3 for an HDMI monitor whose preferred mode is `mode` (also offering
/// 640x480, 800x600 and 1024x768 at 60 Hz).
pub fn edid(mode: &Mode) -> [u8; 128] {
    let mut e = [0u8; 128];
    e[..8].copy_from_slice(&[0, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0]);
    let id = |c: u8| (c - b'A' + 1) as u16;
    let maker = id(b'S') << 10 | id(b'B') << 5 | id(b'E');
    e[8..10].copy_from_slice(&maker.to_be_bytes());
    e[10..12].copy_from_slice(&1u16.to_le_bytes());
    e[16] = 1;
    e[17] = (2026 - 1990) as u8;
    e[18..20].copy_from_slice(&[1, 3]);
    e[20] = 0x80;
    e[21] = 52;
    e[22] = 29;
    e[23] = 120;
    e[24] = 0x0a;
    e[25..35].copy_from_slice(&[0xee, 0x91, 0xa3, 0x54, 0x4c, 0x99, 0x26, 0x0f, 0x50, 0x54]);
    e[35] = 0x21;
    e[36] = 0x08;
    for k in 0..8 {
        e[38 + 2 * k] = 1;
        e[39 + 2 * k] = 1;
    }
    e[54..72].copy_from_slice(&mode.descriptor());
    e[72..77].copy_from_slice(&[0, 0, 0, 0xfc, 0]);
    let name = b"sbc-emu HDMI\n";
    e[77..77 + name.len()].copy_from_slice(name);
    e[90..108].copy_from_slice(&[0, 0, 0, 0xfd, 0, 50, 75, 30, 83, 17, 0, 0x0a, 0x20, 0x20, 0x20, 0x20, 0x20, 0x20]);
    e[108..113].copy_from_slice(&[0, 0, 0, 0x10, 0]);
    let sum = e[..127].iter().fold(0u8, |s, &b| s.wrapping_add(b));
    e[127] = 0u8.wrapping_sub(sum);
    e
}

pub struct Display {
    dc: Vec<u32>,
    hdmi: Vec<u8>,
    edid: [u8; 128],
    fifo_pos: usize,
    /// the monitor's preferred mode
    pub mode: Mode,
    /// vblanks raised (DC interrupt pending bits)
    pending: u32,
    next_vblank: u64,
    /// frames scanned out
    pub frames: u64,
}

impl Display {
    pub fn new(mode: Mode) -> Self {
        let mut hdmi = vec![0u8; (HDMI_SIZE / 4) as usize];
        hdmi[HDMI_STATUS] = M_HOTPLUG;
        Display { dc: vec![0; (DC_SIZE / 4) as usize], hdmi, edid: edid(&mode), fifo_pos: 0, mode, pending: 0, next_vblank: 0, frames: 0 }
    }

    fn reg(&self, off: u64) -> u32 {
        self.dc.get((off / 4) as usize).copied().unwrap_or(0)
    }

    pub fn dc_read(&mut self, off: u64) -> u32 {
        match off {
            AQ_INTR_ACKNOWLEDGE => std::mem::take(&mut self.pending),
            DC_HW_REVISION => 0x5720,
            DC_HW_CHIP_CID => 0x30a,
            FB_CONFIG | 0x151c => self.reg(off) & !(1 << 5), // no underflow
            _ => self.reg(off),
        }
    }

    pub fn dc_write(&mut self, off: u64, v: u32) {
        if let Some(r) = self.dc.get_mut((off / 4) as usize) {
            *r = v;
        }
    }

    /// What the boot firmware leaves running for a kernel without a display driver (the
    /// simple-framebuffer handoff): panel 0 at the monitor's preferred mode, scanning out
    /// primary plane 0 from `addr` (XRGB8888, stride = width * 4). Interrupts stay off.
    pub fn firmware_scanout(&mut self, addr: u32) {
        let m = self.mode;
        let (htotal, vtotal) = (m.width + m.hfp + m.hsync + m.hbp, m.height + m.vfp + m.vsync + m.vbp);
        self.dc_write(DISPLAY_H, m.width | htotal << 16);
        self.dc_write(DISPLAY_V, m.height | vtotal << 16);
        self.dc_write(FB_ADDRESS, addr);
        self.dc_write(FB_STRIDE, m.width * 4);
        self.dc_write(FB_SIZE, m.width | m.height << 15);
        self.dc_write(FB_CONFIG, 5 << 26);
        self.dc_write(FB_CONFIG_EX, 1 << 13);
        self.dc_write(DISPLAY_PANEL_CONFIG, 1 << 12);
    }

    /// Panel `id` is running (DISPLAY_PANEL_CONFIG output enable).
    fn panel_on(&self, id: u64) -> bool {
        self.reg(DISPLAY_PANEL_CONFIG + 4 * id) & (1 << 12) != 0
    }

    pub fn irq(&self) -> bool {
        self.pending != 0 && self.reg(AQ_INTR_ENBL) != 0
    }

    /// Raise vblanks due by `now` (ns); returns the next deadline while a panel runs.
    pub fn update(&mut self, now: u64) -> Option<u64> {
        if !(self.panel_on(0) || self.panel_on(1)) {
            return None;
        }
        const FRAME_NS: u64 = 16_666_667;
        if now >= self.next_vblank {
            self.pending |= (self.panel_on(0) as u32) | (self.panel_on(1) as u32) << 1;
            self.frames += 1;
            self.next_vblank = now - now % FRAME_NS + FRAME_NS;
        }
        Some(self.next_vblank)
    }

    pub fn hdmi_read(&mut self, off: u64) -> u32 {
        let r = (off / 4) as usize;
        match r {
            HDMI_EDID_FIFO_ADDR => {
                let b = self.edid_byte();
                self.fifo_pos += 1;
                b as u32
            }
            // PHY pre-PLL and post-PLL locked
            0x1a9 | 0x1af => 1,
            _ => self.hdmi.get(r).copied().unwrap_or(0) as u32,
        }
    }

    fn edid_byte(&self) -> u8 {
        let segment = self.hdmi[HDMI_EDID_SEGMENT_POINTER] as usize;
        let addr = segment * 256 + self.hdmi[HDMI_EDID_WORD_ADDR] as usize + self.fifo_pos;
        // one 128-byte block, no extensions
        if addr < 128 { self.edid[addr] } else { 0 }
    }

    pub fn hdmi_write(&mut self, off: u64, v: u32) {
        let r = (off / 4) as usize;
        let v = v as u8;
        match r {
            HDMI_INTERRUPT_STATUS1 => self.hdmi[r] &= !v,
            // bit 7 is the hot-plug line (a monitor is connected); the hot-plug interrupt
            // flag (bit 1) is write-1-to-clear and never raised; the rest is the mask
            HDMI_STATUS => self.hdmi[r] = (v & !(M_HOTPLUG | 2)) | M_HOTPLUG,
            HDMI_EDID_SEGMENT_POINTER => {
                // the DDC reads the block into the FIFO
                self.hdmi[r] = v;
                self.fifo_pos = self.hdmi[HDMI_EDID_FIFO_OFFSET] as usize;
                self.hdmi[HDMI_INTERRUPT_STATUS1] |= M_INT_EDID_READY;
            }
            _ => {
                if let Some(x) = self.hdmi.get_mut(r) {
                    *x = v;
                }
            }
        }
    }

    pub fn hdmi_irq(&self) -> bool {
        self.hdmi[HDMI_INTERRUPT_STATUS1] & self.hdmi[HDMI_INTERRUPT_MASK1] != 0
    }

    /// The size of the picture on the HDMI output, if a panel runs.
    pub fn shown_size(&self) -> Option<(u32, u32)> {
        (0..2).find(|&id| self.panel_on(id)).map(|id| {
            (self.reg(DISPLAY_H + 4 * id) & 0x7fff, self.reg(DISPLAY_V + 4 * id) & 0x7fff)
        })
    }

    /// Compose the running panel's picture as RGBA: its primary plane over black.
    pub fn render(&self, ram: &[u8], ram_base: u64, out: &mut Vec<u8>) -> Option<(u32, u32)> {
        let id = (0..2u64).find(|&id| self.panel_on(id))?;
        let (w, h) = self.shown_size()?;
        if w == 0 || h == 0 || w > 8192 || h > 8192 {
            return None;
        }
        out.clear();
        out.resize((w * h * 4) as usize, 0);
        for px in out.chunks_mut(4) {
            px[3] = 255;
        }
        // the primary plane assigned to this panel (CONFIG_EX display_id, bit 19)
        let plane = (0..2u64).find(|&p| {
            let ex = self.reg(FB_CONFIG_EX + 4 * p);
            ex & (1 << 13) != 0 && ((ex >> 19) & 1) as u64 == id
        });
        let Some(p) = plane else { return Some((w, h)) };
        let addr = self.reg(FB_ADDRESS + 4 * p) as u64;
        let stride = self.reg(FB_STRIDE + 4 * p) as usize;
        let size = self.reg(FB_SIZE + 4 * p);
        let (pw, ph) = ((size & 0x7fff).min(w), ((size >> 15) & 0x7fff).min(h));
        let cfg = self.reg(FB_CONFIG + 4 * p);
        let (format, swizzle) = ((cfg >> 26) & 0x1f, (cfg >> 23) & 3);
        let bpp = match format {
            5 | 6 | 22 => 4,
            0..=4 => 2,
            _ => return Some((w, h)),
        };
        let Some(start) = addr.checked_sub(ram_base).map(|o| o as usize) else { return Some((w, h)) };
        for y in 0..ph as usize {
            let row = start + y * stride;
            let Some(src) = ram.get(row..row + pw as usize * bpp) else { break };
            for x in 0..pw as usize {
                let (r, g, b) = match (format, bpp) {
                    (22, _) => {
                        let v = u32::from_le_bytes(src[4 * x..4 * x + 4].try_into().unwrap());
                        ((v >> 22) as u8, (v >> 12) as u8, (v >> 2) as u8)
                    }
                    (_, 4) => {
                        let v = u32::from_le_bytes(src[4 * x..4 * x + 4].try_into().unwrap());
                        let (a, b) = ((v >> 16) as u8, v as u8);
                        let g = (v >> 8) as u8;
                        match swizzle {
                            // ARGB (memory B,G,R,A) or ABGR (memory R,G,B,A)
                            0 | 1 => (a, g, b),
                            _ => (b, g, a),
                        }
                    }
                    (4, _) => {
                        let v = u16::from_le_bytes([src[2 * x], src[2 * x + 1]]);
                        let (r, g, b) = (((v >> 11) & 31) as u8, ((v >> 5) & 63) as u8, (v & 31) as u8);
                        let (r, g, b) = (r << 3 | r >> 2, g << 2 | g >> 4, b << 3 | b >> 2);
                        if swizzle >= 2 { (b, g, r) } else { (r, g, b) }
                    }
                    _ => {
                        let v = u16::from_le_bytes([src[2 * x], src[2 * x + 1]]);
                        let (r, g, b) = (((v >> 10) & 31) as u8, ((v >> 5) & 31) as u8, (v & 31) as u8);
                        (r << 3 | r >> 2, g << 3 | g >> 2, b << 3 | b >> 2)
                    }
                };
                let o = ((y as u32 * w + x as u32) * 4) as usize;
                out[o..o + 4].copy_from_slice(&[r, g, b, 255]);
            }
        }
        Some((w, h))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn edid_through_the_hdmi_ddc_fifo() {
        let mut d = Display::new(Mode::MODES[0]);
        assert_ne!(d.hdmi_read(4 * HDMI_STATUS as u64) as u8 & M_HOTPLUG, 0);
        d.hdmi_write(4 * HDMI_INTERRUPT_MASK1 as u64, M_INT_EDID_READY as u32);
        d.hdmi_write(4 * HDMI_EDID_FIFO_OFFSET as u64, 0);
        d.hdmi_write(4 * HDMI_EDID_WORD_ADDR as u64, 0);
        d.hdmi_write(4 * HDMI_EDID_SEGMENT_POINTER as u64, 0);
        assert!(d.hdmi_irq());
        let bytes: Vec<u8> = (0..128).map(|_| d.hdmi_read(4 * HDMI_EDID_FIFO_ADDR as u64) as u8).collect();
        assert_eq!(&bytes[..8], &[0, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0]);
        assert_eq!(bytes.iter().fold(0u8, |s, &b| s.wrapping_add(b)), 0);
        d.hdmi_write(4 * HDMI_INTERRUPT_STATUS1 as u64, M_INT_EDID_READY as u32);
        assert!(!d.hdmi_irq());
    }

    #[test]
    fn panel_vblank_and_xrgb_scanout() {
        let mut d = Display::new(Mode::MODES[0]);
        assert_eq!(d.dc_read(DC_HW_REVISION), 0x5720);
        d.dc_write(AQ_INTR_ENBL, !0);
        d.dc_write(DISPLAY_H, 4 | (8 << 16));
        d.dc_write(DISPLAY_V, 2 | (4 << 16));
        d.dc_write(FB_ADDRESS, 0x100);
        d.dc_write(FB_STRIDE, 16);
        d.dc_write(FB_SIZE, 4 | (2 << 15));
        d.dc_write(FB_CONFIG, 5 << 26);
        d.dc_write(FB_CONFIG_EX, 1 << 13);
        assert_eq!(d.update(0), None);
        d.dc_write(DISPLAY_PANEL_CONFIG, 1 << 12);
        assert!(d.update(0).is_some());
        assert!(d.irq());
        assert_eq!(d.dc_read(AQ_INTR_ACKNOWLEDGE), 1);
        assert!(!d.irq());
        let mut ram = vec![0u8; 0x200];
        ram[0x100..0x104].copy_from_slice(&0x00ff_8040u32.to_le_bytes());
        let mut out = Vec::new();
        assert_eq!(d.render(&ram, 0, &mut out), Some((4, 2)));
        assert_eq!(&out[..4], &[0xff, 0x80, 0x40, 255]);
    }
}

// Snapshots: the monitor's mode is the snapshot's (`--hdmi-mode`) until the guest reboots.
emu_core::snap_struct!(Mode { clock_khz, width, hfp, hsync, hbp, height, vfp, vsync, vbp });
emu_core::snap_struct!(Display { edid, fifo_pos, mode, pending, next_vblank, frames } fixed { dc, hdmi });
