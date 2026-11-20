//! The RK3588's display controller, VOP2 (`rockchip,rk3588-vop` at 0xfdd90000), as Linux's
//! `rockchipdrm` drives it without an IOMMU (buffer addresses are physical): video port 0
//! feeding HDMI0. Registers keep what is written, and shadowed registers apply at once (the
//! per-port config-done bits read 0). While VP0 runs (DSP_CTRL.STANDBY clear), a frame starts
//! every htotal x vtotal pixel clocks (the dclk's SCMI rate), raising FS_FIELD; going to
//! standby raises DSP_HOLD_VALID. The picture is composed from the enabled windows (Cluster
//! and Esmart) on VP0's layers, bottom to top, the lowest opaque and the others blended by
//! their alpha; linear ARGB/XRGB/ABGR/XBGR8888, RGB888 and RGB565, scaled to their display
//! size. AFBC-compressed windows are not decoded (they show as black).

pub const BASE: u64 = 0xfdd9_0000;
pub const SIZE: u64 = 0x4200;
pub const IRQ: usize = 32 + 156;

const REG_CFG_DONE: u64 = 0x000;
const VERSION_INFO: u64 = 0x004;
/// rk3588: VOP2_VERSION(0x40, 0x17, 0x6786)
const VERSION: u32 = 0x4017_6786;
const OVL_LAYER_SEL: u64 = 0x604;
const OVL_PORT_SEL: u64 = 0x608;
const DSP_IF_EN: u64 = 0x028;
/// Interrupt registers: SYS0, SYS1, then VP0..3 (EN, CLR, STATUS at +0, +4, +8).
const INT_BASES: [u64; 6] = [0x080, 0x090, 0x0a0, 0x0b0, 0x0c0, 0x0d0];
const VP_INT0: usize = 2;
const FS_FIELD: u32 = 1 << 5;
const DSP_HOLD_VALID: u32 = 1 << 6;

const VP0: u64 = 0xc00;
const DSP_CTRL: u64 = 0x00;
const STANDBY: u32 = 1 << 31;
const HTOTAL: u64 = 0x48;
const HACT: u64 = 0x4c;
const VTOTAL: u64 = 0x50;
const VACT: u64 = 0x54;

/// A window: register base, whether it is a Cluster (else Esmart), its layer select ID and
/// the position of its 2-bit field in OVL_PORT_SEL.
const WINDOWS: [(u64, bool, u32, u32); 8] = [
    (0x1000, true, 0, 16),
    (0x1200, true, 1, 18),
    (0x1400, true, 4, 20),
    (0x1600, true, 5, 22),
    (0x1800, false, 2, 24),
    (0x1a00, false, 3, 26),
    (0x1c00, false, 6, 28),
    (0x1e00, false, 7, 30),
];

pub struct Vop2 {
    regs: Vec<u32>,
    /// raw interrupt status per INT_BASES entry
    raw: [u32; 6],
    /// virtual time (ns) of VP0's next frame start
    next_frame: Option<u64>,
    /// frames VP0 started
    pub frames: u64,
    /// frames shown to a viewer
    pub shown: std::cell::Cell<u64>,
}

impl Default for Vop2 {
    fn default() -> Self {
        let mut v = Self { regs: vec![0; (SIZE / 4) as usize], raw: [0; 6], next_frame: None, frames: 0, shown: Default::default() };
        v.regs[((VP0 + DSP_CTRL) / 4) as usize] = STANDBY;
        v
    }
}

/// A window as the registers describe it.
#[derive(Debug, Clone, Copy, PartialEq)]
struct Win {
    addr: u64,
    /// bytes per line (negative: bottom-up, Y mirrored)
    stride: i64,
    format: u32,
    rb_swap: bool,
    afbc: bool,
    src: (u32, u32),
    dst: (u32, u32),
    pos: (u32, u32),
}

impl Vop2 {
    fn reg(&self, off: u64) -> u32 {
        self.regs.get((off / 4) as usize).copied().unwrap_or(0)
    }

    pub fn read(&self, off: u64) -> u32 {
        match off {
            REG_CFG_DONE => self.reg(off) & (1 << 15),
            VERSION_INFO => VERSION,
            _ => {
                if let Some(k) = INT_BASES.iter().position(|&b| off == b + 8) {
                    return self.raw[k] & self.reg(INT_BASES[k]);
                }
                self.reg(off)
            }
        }
    }

    /// A register write at virtual time `now`.
    pub fn write(&mut self, off: u64, v: u32, now: u64) {
        if let Some(k) = INT_BASES.iter().position(|&b| off == b || off == b + 4) {
            let (mask, bits) = (v >> 16, v & 0xffff);
            if off == INT_BASES[k] {
                let r = &mut self.regs[(off / 4) as usize];
                *r = (*r & !mask) | (bits & mask);
            } else {
                self.raw[k] &= !(bits & mask);
            }
            return;
        }
        let was_running = self.running();
        if let Some(r) = self.regs.get_mut((off / 4) as usize) {
            *r = v;
        }
        if off == VP0 + DSP_CTRL {
            match (was_running, self.running()) {
                (false, true) => self.next_frame = Some(now),
                (true, false) => {
                    self.next_frame = None;
                    self.raw[VP_INT0] |= DSP_HOLD_VALID;
                }
                _ => {}
            }
        }
    }

    /// VP0 is scanning out (out of standby, with a mode).
    pub fn running(&self) -> bool {
        self.reg(VP0 + DSP_CTRL) & STANDBY == 0 && self.size().is_some()
    }

    /// VP0 drives HDMI0 (DSP_IF_EN bit 3, mux 0).
    pub fn hdmi0(&self) -> bool {
        let e = self.reg(DSP_IF_EN);
        e & (1 << 3) != 0 && (e >> 16) & 3 == 0
    }

    /// VP0's active size.
    pub fn size(&self) -> Option<(u32, u32)> {
        let span = |r: u32| (r & 0xffff).saturating_sub(r >> 16);
        let (w, h) = (span(self.reg(VP0 + HACT)), span(self.reg(VP0 + VACT)));
        (w > 0 && h > 0).then_some((w, h))
    }

    /// Frame starts up to `now` with a pixel clock of `dclk` Hz.
    pub fn tick(&mut self, now: u64, dclk: u64) {
        let Some(mut t) = self.next_frame else { return };
        if t > now {
            return;
        }
        let total = u64::from(self.reg(VP0 + HTOTAL) >> 16).max(1) * u64::from(self.reg(VP0 + VTOTAL) >> 16).max(1);
        let period = (total * 1_000_000_000 / dclk.max(1)).clamp(1_000_000, 100_000_000);
        while t <= now {
            t += period;
            self.frames += 1;
        }
        self.raw[VP_INT0] |= FS_FIELD;
        self.next_frame = Some(t);
    }

    /// The next frame start (when vblank interrupts are wanted).
    pub fn next_event(&self) -> Option<u64> {
        self.next_frame.filter(|_| self.reg(INT_BASES[VP_INT0]) & FS_FIELD != 0)
    }

    pub fn irq(&self) -> bool {
        (0..6).any(|k| self.raw[k] & self.reg(INT_BASES[k]) != 0)
    }

    fn window(&self, k: usize) -> Option<Win> {
        let (b, cluster, _, _) = WINDOWS[k];
        let r = |o: u64| self.reg(b + o);
        let (ctrl, mst, vir, mirror) = if cluster {
            if r(0x100) & 1 == 0 {
                return None;
            }
            (r(0x00), r(0x10), r(0x18), r(0x54) & (1 << 3) != 0)
        } else {
            (r(0x10), r(0x14), r(0x1c), r(0x04) & (1 << 31) != 0)
        };
        if ctrl & 1 == 0 {
            return None;
        }
        let size = |v: u32| ((v & 0xffff) + 1, (v >> 16) + 1);
        let stride = i64::from(vir & 0xffff) * 4;
        Some(Win {
            addr: u64::from(mst),
            stride: if mirror { -stride } else { stride },
            format: (ctrl >> 1) & 0x1f,
            rb_swap: ctrl & (1 << 14) != 0,
            afbc: cluster && r(0x100) & 2 != 0,
            src: size(r(0x20)),
            dst: size(r(0x24)),
            pos: (r(0x28) & 0xffff, r(0x28) >> 16),
        })
    }

    /// VP0's windows, bottom to top.
    fn layers(&self) -> Vec<Win> {
        let (sel, port) = (self.reg(OVL_LAYER_SEL), self.reg(OVL_PORT_SEL));
        let top = (port & 0xf).min(7);
        (0..=top)
            .filter_map(|n| {
                let id = (sel >> (4 * n)) & 7;
                let k = WINDOWS.iter().position(|w| w.2 == id)?;
                ((port >> WINDOWS[k].3) & 3 == 0).then(|| self.window(k)).flatten()
            })
            .collect()
    }

    /// The picture VP0 sends, as RGBA.
    pub fn render(&self, ram: &[u8], out: &mut Vec<u8>) -> Option<(u32, u32)> {
        if !self.running() {
            return None;
        }
        let (w, h) = self.size()?;
        out.clear();
        out.resize((w * h * 4) as usize, 0);
        for px in out.chunks_exact_mut(4) {
            px[3] = 255;
        }
        for (n, win) in self.layers().iter().enumerate() {
            if win.afbc {
                continue;
            }
            let bpp: i64 = match win.format { 0 => 4, 1 => 3, 2 => 2, _ => continue };
            for dy in 0..win.dst.1 {
                let y = win.pos.1 + dy;
                if y >= h {
                    break;
                }
                let sy = i64::from(dy * win.src.1 / win.dst.1);
                let line = win.addr as i64 + sy * win.stride;
                for dx in 0..win.dst.0 {
                    let x = win.pos.0 + dx;
                    if x >= w {
                        break;
                    }
                    let sx = i64::from(dx * win.src.0 / win.dst.0);
                    let a = (line + sx * bpp) as usize;
                    let Some(s) = ram.get(a..a + bpp as usize) else { continue };
                    // (r, g, b, alpha)
                    let (mut r, g, mut b, al) = match win.format {
                        0 => (s[2], s[1], s[0], s[3]),
                        1 => (s[2], s[1], s[0], 255),
                        _ => {
                            let v = u16::from_le_bytes([s[0], s[1]]);
                            let c = |v: u16, bits: u32| ((u32::from(v) * 255) / ((1 << bits) - 1)) as u8;
                            (c(v >> 11, 5), c((v >> 5) & 0x3f, 6), c(v & 0x1f, 5), 255)
                        }
                    };
                    if win.rb_swap {
                        std::mem::swap(&mut r, &mut b);
                    }
                    let d = &mut out[((y * w + x) * 4) as usize..][..4];
                    if n == 0 || al == 255 {
                        d[..3].copy_from_slice(&[r, g, b]);
                    } else {
                        let mix = |s: u8, o: u8| ((u32::from(s) * u32::from(al) + u32::from(o) * (255 - u32::from(al))) / 255) as u8;
                        let m = [mix(r, d[0]), mix(g, d[1]), mix(b, d[2])];
                        d[..3].copy_from_slice(&m);
                    }
                }
            }
        }
        self.shown.set(self.shown.get() + 1);
        Some((w, h))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// What Linux writes for a 1280x720 XRGB8888 framebuffer on Cluster0, VP0 to HDMI0.
    fn modeset(v: &mut Vop2, fb: u32) {
        v.write(VP0 + HTOTAL, 1650 << 16 | 40, 0);
        v.write(VP0 + HACT, 260 << 16 | (260 + 1280), 0);
        v.write(VP0 + VTOTAL, 750 << 16 | 5, 0);
        v.write(VP0 + VACT, 25 << 16 | (25 + 720), 0);
        v.write(DSP_IF_EN, 1 << 3, 0);
        v.write(OVL_PORT_SEL, 0x7887, 0);
        v.write(OVL_LAYER_SEL, 0x7654_3210 & 0x7777_7777, 0);
        v.write(0x1000 + 0x100, 1, 0);
        v.write(0x1000 + 0x10, fb, 0);
        v.write(0x1000 + 0x18, 1280, 0);
        v.write(0x1000 + 0x20, 719 << 16 | 1279, 0);
        v.write(0x1000 + 0x24, 719 << 16 | 1279, 0);
        v.write(0x1000, 1, 0);
        v.write(INT_BASES[VP_INT0] + 4, FS_FIELD << 16 | FS_FIELD, 0);
        v.write(INT_BASES[VP_INT0], FS_FIELD << 16 | FS_FIELD, 0);
        v.write(VP0 + DSP_CTRL, 15, 1000);
    }

    #[test]
    fn scans_out_cluster0_and_raises_vblank() {
        let mut v = Vop2::default();
        assert_eq!(v.read(VERSION_INFO), VERSION);
        let mut ram = vec![0u8; 8 << 20];
        let fb = 0x10_0000u32;
        let px = fb as usize + (3 * 1280 + 2) * 4;
        ram[px..px + 4].copy_from_slice(&0x0012_3456u32.to_le_bytes());
        modeset(&mut v, fb);
        assert!(v.running() && v.hdmi0());
        let mut out = Vec::new();
        assert_eq!(v.render(&ram, &mut out), Some((1280, 720)));
        assert_eq!(&out[(3 * 1280 + 2) * 4..][..4], &[0x12, 0x34, 0x56, 255]);
        assert!(!v.irq());
        v.tick(1000, 74_250_000);
        assert!(v.irq() && v.read(INT_BASES[VP_INT0] + 8) == FS_FIELD);
        assert_eq!(v.next_event(), Some(1000 + 16_666_666));
        v.write(INT_BASES[VP_INT0] + 4, FS_FIELD << 16 | FS_FIELD, 0);
        assert!(!v.irq());
        assert_eq!(v.read(REG_CFG_DONE), 0);
        v.write(VP0 + DSP_CTRL, STANDBY, 2000);
        assert_eq!(v.raw[VP_INT0] & DSP_HOLD_VALID, DSP_HOLD_VALID);
        assert_eq!(v.render(&ram, &mut out), None);
    }
}
