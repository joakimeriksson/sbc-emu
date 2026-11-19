//! HDMI0 output as the boot firmware leaves it: VOP2 video port 0 scanning out one
//! XRGB8888 framebuffer at the monitor's preferred mode. Linux gets the framebuffer as a
//! `simple-framebuffer` (driven by `simpledrm`), as U-Boot hands one over on boards whose
//! display the kernel does not drive itself. The VOP2 and HDMI/eDP TX registers are not
//! modelled, so the mode is fixed for the run.

/// A monitor mode (`--hdmi-mode WxH`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Mode {
    pub width: u32,
    pub height: u32,
}
impl Mode {
    /// The modes the emulated monitor offers; the first is the default.
    pub const MODES: [Mode; 3] = [Mode { width: 1280, height: 720 }, Mode { width: 1920, height: 1080 }, Mode { width: 640, height: 480 }];
    pub fn find(width: u32, height: u32) -> Option<Mode> { Self::MODES.iter().copied().find(|m| m.width == width && m.height == height) }
    pub fn stride(&self) -> u32 { self.width * 4 }
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

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn renders_xrgb8888_from_ram() {
        let mut ram = vec![0u8; 32 << 20];
        let addr = framebuffer_addr(ram.len() as u64);
        let d = Display::new(addr, Mode { width: 640, height: 480 });
        let px = addr as usize + (10 * 640 + 3) * 4;
        ram[px..px + 4].copy_from_slice(&0x00_12_34_56u32.to_le_bytes());
        let mut out = Vec::new();
        assert_eq!(d.render(&ram, &mut out), Some((640, 480)));
        assert_eq!(&out[(10 * 640 + 3) * 4..][..4], &[0x12, 0x34, 0x56, 255]);
        assert_eq!(d.frames.get(), 1);
        assert_eq!(Display::default().render(&ram, &mut out), None, "HDMI off");
    }
}
