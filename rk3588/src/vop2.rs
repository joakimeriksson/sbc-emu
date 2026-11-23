//! The RK3588's display controller, VOP2 (`rockchip,rk3588-vop` at 0xfdd90000), as Linux's
//! `rockchipdrm` drives it without an IOMMU (buffer addresses are physical): video port 0
//! feeding HDMI0. Registers keep what is written, and shadowed registers apply at once (the
//! per-port config-done bits read 0). While VP0 runs (DSP_CTRL.STANDBY clear), a frame starts
//! every htotal x vtotal pixel clocks (the dclk's SCMI rate), raising FS_FIELD; going to
//! standby raises DSP_HOLD_VALID. The picture is composed from the enabled windows (Cluster
//! and Esmart) on VP0's layers, bottom to top: linear ARGB/XRGB/ABGR/XBGR8888, RGB888 and
//! RGB565, scaled to their display size. AFBC-compressed windows are not decoded (they show as
//! black). The lowest layer is drawn as is; each one above it goes through its overlay mixer
//! (MIXn), and a Cluster window first through its Cluster mixer, as Linux programs them
//! (`vop2_setup_alpha`, `vop2_setup_cluster_alpha`): per-pixel or global alpha (a format
//! without alpha is mixed with the plane's global alpha only, so its X byte is ignored),
//! source colour pre-multiplied or not (the plane's "pixel blend mode": "Pre-multiplied" or
//! "Coverage"), and the source and destination factors. A mixer Linux has not set up (its
//! ALPHA_EN clear) lets the upper layer cover what is below.
//!
//! An overlay mixer works on the whole picture, not only under its upper window: outside it
//! the source is empty (colour and alpha 0), so the mixer applies only its destination terms.
//! For what Linux usually sets up that leaves the picture as it is, but with a bottom plane
//! whose global alpha is below opaque (`Cd = Cs + (1 - As) Cd Agd`) the whole bottom plane is
//! dimmed, not only the part under the upper window. The HDR0 mixer (0x6c0) comes last when
//! Linux enables it (only for such a bottom plane): the mixed picture (its source, taken as
//! pre-multiplied) over the black background. A bottom plane with global alpha and no plane
//! above it is not dimmed: no mixer Linux sets up applies its alpha.
//!
//! The Cluster windows have no XRGB format: XRGB8888 and ARGB8888 are one format code, and
//! no window register tells them apart. Whether the X byte counts as alpha is decided by the
//! mixers alone, so an XRGB plane on a Cluster window above another plane is mixed by its X
//! byte when the mixers say per-pixel alpha, as mainline Linux sets them up for every Cluster
//! format ("Mix output data only have pixel alpha"). Rockchip's own kernel sets the Cluster
//! mixer of a format without alpha to pass on the global alpha instead (its
//! `vop2_setup_cluster_alpha`, `dst_pixel_alpha_en = is_alpha_support(format)`), and then the
//! plane covers. Treating a Cluster window's alpha byte as opaque here would break ARGB
//! planes on Cluster windows, which look the same to the hardware (issue #54).

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
/// The largest picture VP0 drives.
const MAX_SIZE: (u32, u32) = (7680, 4320);
const HTOTAL: u64 = 0x48;
const HACT: u64 = 0x4c;
const VTOTAL: u64 = 0x50;
const VACT: u64 = 0x54;

/// The Cluster windows' own mixers (win1 over win0), 0x10 apart: SRC_COLOR, DST_COLOR,
/// SRC_ALPHA, DST_ALPHA control.
const CLUSTER0_MIX: u64 = 0x610;
/// The overlay mixers of layers 1 up, 0x10 apart (layer n uses MIX n-1 on VP0).
const MIX0: u64 = 0x650;
/// VP0's HDR mixer, after the overlay mixers: Linux sets it up only for a bottom plane with
/// global alpha below opaque ("Transfer pixel alpha to hdr mix"), else writes 0.
const HDR0_MIX: u64 = 0x6c0;

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
    /// VP0 has scanned out: Linux drives the display (the firmware's framebuffer is stale)
    taken_over: bool,
}

impl Default for Vop2 {
    fn default() -> Self {
        let mut v = Self { regs: vec![0; (SIZE / 4) as usize], raw: [0; 6], next_frame: None, frames: 0, shown: Default::default(), taken_over: false };
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
                (false, true) => {
                    self.next_frame = Some(now);
                    self.taken_over = true;
                }
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

    /// VP0's active size (none beyond what VP0 can drive, 7680x4320).
    pub fn size(&self) -> Option<(u32, u32)> {
        let span = |r: u32| (r & 0xffff).saturating_sub(r >> 16);
        let (w, h) = (span(self.reg(VP0 + HACT)), span(self.reg(VP0 + VACT)));
        (w > 0 && h > 0 && w <= MAX_SIZE.0 && h <= MAX_SIZE.1).then_some((w, h))
    }

    /// Linux has driven the display: from then on the monitor shows VOP2's output, black
    /// while VP0 is off, never the firmware's framebuffer again.
    pub fn taken_over(&self) -> bool {
        self.taken_over
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

    /// VP0's windows, bottom to top: (layer, index in WINDOWS, window). A window selected by
    /// more than one layer is on the lowest: Linux puts the planes on layers 0 up and leaves
    /// the layers above with what they selected before, starting from all zero (Cluster0).
    fn layers(&self) -> Vec<(u32, usize, Win)> {
        let (sel, port) = (self.reg(OVL_LAYER_SEL), self.reg(OVL_PORT_SEL));
        let top = (port & 0xf).min(7);
        let mut seen = 0u32;
        (0..=top)
            .filter_map(|n| {
                let id = (sel >> (4 * n)) & 7;
                let k = WINDOWS.iter().position(|w| w.2 == id)?;
                if seen & 1 << k != 0 {
                    return None;
                }
                seen |= 1 << k;
                ((port >> WINDOWS[k].3) & 3 == 0).then(|| self.window(k)).flatten().map(|w| (n, k, w))
            })
            .collect()
    }

    /// The mixer whose four control registers start at `base`.
    fn mixer(&self, base: u64) -> Mixer {
        Mixer::new([0, 4, 8, 12].map(|o| self.reg(base + o)))
    }

    /// What the HDMI monitor gets, as RGBA: VP0's picture, black while VP0 is off or drives
    /// another output (once Linux has driven it).
    pub fn render(&self, ram: &[u8], out: &mut Vec<u8>) -> Option<(u32, u32)> {
        if !self.taken_over {
            return None;
        }
        let (w, h) = self.size()?;
        out.clear();
        // black background, alpha 0
        out.resize(w as usize * h as usize * 4, 0);
        let layers = if self.running() && self.hdmi0() { self.layers() } else { Vec::new() };
        for (n, (layer, k, win)) in layers.into_iter().enumerate() {
            // the lowest layer is drawn as is, the others through their overlay mixer
            let ovl = (n > 0 && layer > 0).then(|| self.mixer(MIX0 + 0x10 * u64::from(layer - 1))).filter(|m| m.enabled);
            // the mixer also acts where the window gives no pixel, with an empty source:
            // usually that keeps what is below, but not for a bottom plane with global alpha
            // (Cd x Agd)
            let empty = ovl.filter(|m| !m.empty_keeps_dst());
            let bpp: Option<i64> = match win.format {
                _ if win.afbc => None,
                0 => Some(4),
                1 => Some(3),
                2 => Some(2),
                _ => None,
            };
            let Some(bpp) = bpp else {
                // a window not decoded (AFBC, other formats) gives no pixels, but its mixer acts
                if let Some(m) = empty {
                    for d in out.chunks_exact_mut(4) {
                        let m = m.mix_empty([d[0], d[1], d[2], d[3]]);
                        d.copy_from_slice(&m);
                    }
                }
                continue;
            };
            let cluster = WINDOWS[k].1.then(|| self.mixer(CLUSTER0_MIX + 0x10 * k as u64)).filter(|m| m.enabled && !m.passes_dst());
            if let Some(m) = empty {
                let (x0, y0) = (win.pos.0.min(w), win.pos.1.min(h));
                let (x1, y1) = ((win.pos.0 + win.dst.0).min(w), (win.pos.1 + win.dst.1).min(h));
                for y in 0..h {
                    for x in 0..w {
                        if (y0..y1).contains(&y) && (x0..x1).contains(&x) {
                            continue;
                        }
                        let d = &mut out[((y * w + x) * 4) as usize..][..4];
                        let m = m.mix_empty([d[0], d[1], d[2], d[3]]);
                        d.copy_from_slice(&m);
                    }
                }
            }
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
                    let d = &mut out[((y * w + x) * 4) as usize..][..4];
                    // (a Y-mirrored window starting low in memory reaches below 0)
                    let s = usize::try_from(line + sx * bpp).ok().and_then(|a| ram.get(a..a.checked_add(bpp as usize)?));
                    let Some(s) = s else {
                        // no pixel outside RAM: the mixer's empty source
                        if let Some(m) = empty {
                            let m = m.mix_empty([d[0], d[1], d[2], d[3]]);
                            d.copy_from_slice(&m);
                        }
                        continue;
                    };
                    // (r, g, b, pixel alpha): formats without an alpha channel have none (255)
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
                    let mut p = [r, g, b, al];
                    // a Cluster's mixer: its win1 (unused by Linux, all zero) over win0
                    if let Some(m) = &cluster {
                        p = m.mix([0; 4], p);
                    }
                    let m = match &ovl { Some(m) => m.mix(p, [d[0], d[1], d[2], d[3]]), None => p };
                    d.copy_from_slice(&m);
                }
            }
        }
        // the HDR mixer: the mixed layers over the (black) background. Its source, the overlay
        // mixers' output, is pre-multiplied already, and is taken as such whatever the colour
        // mode bit says: Linux copies that bit from the plane at zpos 1 ("Coverage" sets it,
        // and with no plane there it stays 0), and honouring it would multiply the mixed
        // colour by the mixed alpha again (the bottom plane's alpha squared, or black for an
        // XRGB bottom plane with X = 0). Not checked on hardware.
        let mut hdr = self.mixer(HDR0_MIX);
        hdr.ctrl[0].mul_alpha = false;
        // (factor one over black: the source unchanged)
        if hdr.enabled && hdr.ctrl[0].factor != 1 {
            for px in out.chunks_exact_mut(4) {
                let m = hdr.mix([px[0], px[1], px[2], px[3]], [0; 4]);
                px.copy_from_slice(&m);
            }
        }
        // (the 4th byte held the mixers' alpha; the monitor shows colour only)
        for px in out.chunks_exact_mut(4) {
            px[3] = 255;
        }
        self.shown.set(self.shown.get() + 1);
        Some((w, h))
    }
}

/// A VOP2 alpha mixer's control registers (SRC_COLOR, DST_COLOR, SRC_ALPHA, DST_ALPHA_CTRL),
/// each laid out as Linux's `union vop2_alpha_ctrl`: [0] colour mode (0: the colour is
/// pre-multiplied, 1: multiply it by its alpha), [1] alpha mode (1: inverted), [3:2] blend mode
/// (0: global alpha, 1: per-pixel alpha, 2: per-pixel times global), [7:5] factor, [8] alpha
/// enable (in SRC_COLOR), [23:16] global alpha. The global alphas are the colour registers'.
///
/// The colour mode and the factor are two separate multiplies. "Multiply by its alpha" uses
/// the alpha the blend mode selects (pixel, global, or both), and the factor comes on top.
/// Rockchip's kernel relies on that in `vop2_setup_cluster_alpha`: it sets a Cluster's win0
/// to "not pre-multiplied" with blend mode "global" to get `Cd x Agd` ("global need to do
/// Cd * Ad0, Ad0 = global"), and to "per-pixel times global" to get `Cd x Ad x Agd`; its
/// Dolby Vision path says colour mode "not pre-multiplied" with factor one is "roughly
/// equal" to "pre-multiplied" with factor 4 (source alpha). So for a plane without pixel
/// alpha in "Coverage" mode with global alpha Ag, where Linux's `vop2_parse_alpha` sets
/// blend "global", colour mode "not pre-multiplied" and factor "source global alpha", the
/// colour is Cs x Ag x Ag and the result `Ag^2 Cs + (1 - Ag) Cd`, not DRM's
/// `Ag Cs + (1 - Ag) Cd`: that is what the hardware does with those registers. In the
/// default "Pre-multiplied" mode the colour is not multiplied and the result is DRM's.
#[derive(Clone, Copy)]
struct Mixer {
    enabled: bool,
    /// SRC_COLOR, DST_COLOR, SRC_ALPHA, DST_ALPHA
    ctrl: [Ctrl; 4],
    /// global alpha of the source and the destination
    glb: [u32; 2],
}

/// One control register, decoded.
#[derive(Clone, Copy)]
struct Ctrl {
    /// multiply the colour by its alpha (the colour is not pre-multiplied)
    mul_alpha: bool,
    inverted: bool,
    blend: u32,
    factor: u32,
}

/// a x b / 255, rounded
fn mul(a: u32, b: u32) -> u32 {
    (a * b + 127) / 255
}

impl Ctrl {
    fn new(v: u32) -> Self {
        Self { mul_alpha: v & 1 != 0, inverted: v & 2 != 0, blend: (v >> 2) & 3, factor: (v >> 5) & 7 }
    }

    /// The alpha it selects from a pixel alpha and a global alpha.
    fn alpha(&self, pixel: u32, global: u32) -> u32 {
        let pixel = if self.inverted { 255 - pixel } else { pixel };
        match self.blend {
            0 => global,
            1 => pixel,
            _ => mul(pixel, global),
        }
    }
}

impl Mixer {
    fn new(regs: [u32; 4]) -> Self {
        Self { enabled: regs[0] & (1 << 8) != 0, ctrl: regs.map(Ctrl::new), glb: [(regs[0] >> 16) & 0xff, (regs[1] >> 16) & 0xff] }
    }

    /// `src` mixed over `dst` (both r, g, b, alpha).
    fn mix(&self, src: [u8; 4], dst: [u8; 4]) -> [u8; 4] {
        self.blend(Some(src), dst)
    }

    /// What the mixer makes of `dst` where its upper layer has no window: an empty source,
    /// colour and alpha 0 whatever the blend mode (its global alpha does not reach there).
    fn mix_empty(&self, dst: [u8; 4]) -> [u8; 4] {
        self.blend(None, dst)
    }

    fn blend(&self, src: Option<[u8; 4]>, dst: [u8; 4]) -> [u8; 4] {
        let [sc, dc, sa, da] = &self.ctrl;
        let [gs, gd] = self.glb;
        let pd = u32::from(dst[3]);
        // (the source's colour alpha, and its alpha for the alpha output)
        let (a_s, src_alpha) = match src {
            Some(s) => (sc.alpha(u32::from(s[3]), gs), sa.alpha(u32::from(s[3]), gs)),
            None => (0, 0),
        };
        let src = src.unwrap_or([0; 4]);
        let a_d = dc.alpha(pd, gd);
        // colour factors: source (zero, one, dst alpha, 1 - dst alpha, src alpha, src global),
        // destination (zero, one, src alpha, 1 - src alpha, dst alpha, dst global)
        let fs = match sc.factor { 1 => 255, 2 => a_d, 3 => 255 - a_d, 4 => a_s, 5 => gs, _ => 0 };
        let fd = match dc.factor { 1 => 255, 2 => a_s, 3 => 255 - a_s, 4 => a_d, 5 => gd, _ => 0 };
        let mut out = [0u8; 4];
        for i in 0..3 {
            let s = if sc.mul_alpha { mul(u32::from(src[i]), a_s) } else { u32::from(src[i]) };
            let d = if dc.mul_alpha { mul(u32::from(dst[i]), a_d) } else { u32::from(dst[i]) };
            out[i] = (mul(s, fs) + mul(d, fd)).min(255) as u8;
        }
        // alpha factors: zero, one, src alpha, 1 - src alpha, src global
        let f = |c: &Ctrl| match c.factor { 1 => 255, 2 => a_s, 3 => 255 - a_s, 4 => gs, _ => 0 };
        out[3] = (mul(src_alpha, f(sa)) + mul(da.alpha(pd, gd), f(da))).min(255) as u8;
        out
    }

    /// An empty source (`mix_empty`) leaves any destination unchanged: what Linux sets up for
    /// every overlay mixer except the one above a bottom plane with global alpha.
    fn empty_keeps_dst(&self) -> bool {
        let [_, dc, _, da] = &self.ctrl;
        let [gs, gd] = self.glb;
        // the destination alpha is 255 whatever the pixel
        let a_d_one = dc.blend == 0 && gd == 255;
        // colour: Cd x (its alpha, if not pre-multiplied) x the destination factor (with the
        // source alpha 0: one, 1 - src alpha, dst alpha, dst global), all 1
        let colour = (!dc.mul_alpha || a_d_one) && match dc.factor { 1 | 3 => true, 4 => a_d_one, 5 => gd == 255, _ => false };
        // alpha: the destination's pixel alpha times one, 1 - src alpha or src global
        let alpha = !da.inverted && da.blend == 1 && match da.factor { 1 | 3 => true, 4 => gs == 255, _ => false };
        colour && alpha
    }

    /// An all-zero source mixed over any destination gives the destination unchanged (a
    /// Cluster mixer as Linux sets it up for a plane with opaque global alpha).
    fn passes_dst(&self) -> bool {
        let [sc, dc, sa, da] = &self.ctrl;
        let [gs, gd] = self.glb;
        let a_s = sc.alpha(0, gs);
        let alpha_factor = |c: &Ctrl| match c.factor { 1 => 255, 2 => a_s, 3 => 255 - a_s, 4 => gs, _ => 0 };
        // colour: Cd x (its alpha, if not pre-multiplied) x the destination factor, all 1
        let colour = dc.blend == 0
            && (!dc.mul_alpha || gd == 255)
            && match dc.factor { 1 => true, 2 => a_s == 255, 3 => a_s == 0, 5 => gd == 255, _ => false };
        // alpha: the destination's pixel alpha, nothing from the source
        let alpha = !da.inverted && da.blend == 1 && alpha_factor(da) == 255 && mul(sa.alpha(0, gs), alpha_factor(sa)) == 0;
        colour && alpha
    }
}

emu_core::snap_struct!(Vop2 { raw, next_frame, frames, shown, taken_over } fixed { regs });

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
        // off: black, not the firmware's framebuffer
        assert!(v.taken_over());
        assert_eq!(v.render(&ram, &mut out), Some((1280, 720)));
        assert!(out.chunks_exact(4).all(|p| p == [0, 0, 0, 255]));
    }

    /// Linux's `vop2_parse_alpha` (rockchip_vop2_reg.c): the four control registers of a
    /// mixer, from (src premultiplied, dst premultiplied, src pixel alpha, dst pixel alpha,
    /// src global alpha, dst global alpha), global alphas 8-bit.
    fn parse_alpha(src_pre: bool, dst_pre: bool, src_px: bool, dst_px: bool, src_glb: u32, dst_glb: u32) -> [u32; 4] {
        // bitfields of union vop2_alpha_ctrl
        let ctrl = |color_mode: u32, blend: u32, cal: u32, factor: u32, en: u32, glb: u32| {
            color_mode | blend << 2 | cal << 4 | factor << 5 | en << 8 | glb << 16
        };
        let (src_glb_en, dst_glb_en) = (src_glb != 0xff, dst_glb != 0xff);
        // enum color_mode: ALPHA_SRC_PRE_MUL = 0, ALPHA_SRC_NO_PRE_MUL = 1
        let (scm, dcm) = (u32::from(!src_pre), u32::from(!dst_pre));
        // enum global_blend_mode: ALPHA_GLOBAL, ALPHA_PER_PIX, ALPHA_PER_PIX_GLOBAL
        let sblend = if !src_px { 0 } else if !src_glb_en { 1 } else { 2 };
        // enum src_factor_mode: SRC_FAC_ALPHA_ONE = 1, SRC_FAC_ALPHA_SRC_GLOBAL = 5
        let (scm, sfac) = match sblend { 0 => (scm, 5), 1 => (scm, 1), _ => (0, 5) };
        let src_color = ctrl(scm, sblend, 0, sfac, 1, src_glb);
        // ALPHA_SRC_INVERSE = 3
        let dst_color = ctrl(dcm, 0, 0, 3, 0, dst_glb);
        let src_alpha = ctrl(0, sblend, 0, 1, 0, 0);
        let dblend = if dst_px && !dst_glb_en { 1 } else { 2 };
        let dst_alpha = ctrl(0, dblend, 1, 3, 0, 0);
        [src_color, dst_color, src_alpha, dst_alpha]
    }

    fn write_mixer(v: &mut Vop2, base: u64, regs: [u32; 4]) {
        for (i, r) in regs.into_iter().enumerate() {
            v.write(base + 4 * i as u64, r, 0);
        }
    }

    /// A 64x64 window at (100, 100) on Esmart0 (layer select ID 2), on VP0.
    fn esmart0(v: &mut Vop2, fb: u32) {
        let b = 0x1800;
        v.write(b + 0x14, fb, 0);
        v.write(b + 0x1c, 64, 0);
        v.write(b + 0x20, 63 << 16 | 63, 0);
        v.write(b + 0x24, 63 << 16 | 63, 0);
        v.write(b + 0x28, 100 << 16 | 100, 0);
        v.write(b + 0x10, 1, 0); // enabled, format 0 (ARGB8888/XRGB8888)
    }

    /// Cluster0 at the bottom (layer 0) and Esmart0 above it (layer 1), with the mixers set
    /// up as `vop2_setup_cluster_alpha` and `vop2_setup_alpha` do for an Esmart plane with
    /// opaque global alpha, of a format with alpha or not, pre-multiplied or coverage.
    fn two_planes(ram: &mut [u8], has_alpha: bool, premulti: bool, under: u32, over: u32) -> Vop2 {
        let mut v = Vop2::default();
        let (fb, ov) = (0x10_0000u32, 0x60_0000u32);
        for i in 0..1280 * 720 {
            ram[fb as usize + 4 * i..][..4].copy_from_slice(&under.to_le_bytes());
        }
        for i in 0..64 * 64 {
            ram[ov as usize + 4 * i..][..4].copy_from_slice(&over.to_le_bytes());
        }
        modeset(&mut v, fb);
        esmart0(&mut v, ov);
        v.write(OVL_LAYER_SEL, 0x7654_3120, 0); // layer 0 Cluster0, layer 1 Esmart0
        v.write(OVL_PORT_SEL, 0x7881, 0); // VP0 has layers 0..1
        // Cluster0's mixer: an all-zero win1 over win0, global alpha opaque
        write_mixer(&mut v, CLUSTER0_MIX, parse_alpha(false, false, false, true, 0, 0xff));
        // MIX0, layer 1: Cd = Cs + (1 - As) * Cd
        write_mixer(&mut v, MIX0, parse_alpha(premulti, true, has_alpha, true, 0xff, 0xff));
        v
    }

    fn pixel(out: &[u8], x: usize, y: usize) -> [u8; 4] {
        out[(y * 1280 + x) * 4..][..4].try_into().unwrap()
    }

    /// An XRGB8888 overlay plane covers what is below whatever its X byte holds.
    #[test]
    fn xrgb_overlay_plane_is_opaque() {
        let mut ram = vec![0u8; 8 << 20];
        let mut out = Vec::new();
        // X byte 0, as Mesa writes it
        let v = two_planes(&mut ram, false, true, 0x00ff_ffff, 0x0010_2030);
        assert_eq!(v.render(&ram, &mut out), Some((1280, 720)));
        assert_eq!(pixel(&out, 120, 120), [0x10, 0x20, 0x30, 255]);
        assert_eq!(pixel(&out, 99, 120), [0xff, 0xff, 0xff, 255]);
        let v = two_planes(&mut ram, false, false, 0x00ff_ffff, 0x4410_2030);
        v.render(&ram, &mut out);
        assert_eq!(pixel(&out, 120, 120), [0x10, 0x20, 0x30, 255]);

        // the same over an Esmart plane (Esmart1 at the bottom)
        let mut v = two_planes(&mut ram, false, true, 0, 0x0010_2030);
        let b = 0x1a00;
        v.write(b + 0x14, 0x10_0000, 0);
        v.write(b + 0x1c, 1280, 0);
        v.write(b + 0x20, 719 << 16 | 1279, 0);
        v.write(b + 0x24, 719 << 16 | 1279, 0);
        v.write(b + 0x10, 1, 0);
        v.write(OVL_LAYER_SEL, 0x7654_1023, 0); // layer 0 Esmart1, layer 1 Esmart0
        v.write(OVL_PORT_SEL, 0x7881, 0);
        for i in 0..1280 * 720 {
            ram[0x10_0000 + 4 * i..][..4].copy_from_slice(&0x00c0_c0c0u32.to_le_bytes());
        }
        v.render(&ram, &mut out);
        assert_eq!(pixel(&out, 120, 120), [0x10, 0x20, 0x30, 255]);
        assert_eq!(pixel(&out, 10, 10), [0xc0, 0xc0, 0xc0, 255]);
    }

    /// ARGB8888 with the default "Pre-multiplied" blend mode: Cs + (1 - As) * Cd; with
    /// "Coverage": As * Cs + (1 - As) * Cd.
    #[test]
    fn argb_overlay_blends_by_the_planes_blend_mode() {
        let mut ram = vec![0u8; 8 << 20];
        let mut out = Vec::new();
        // half-transparent grey 0x40 (pre-multiplied: 0x40 is already alpha x colour) over white
        let v = two_planes(&mut ram, true, true, 0x00ff_ffff, 0x8040_4040);
        v.render(&ram, &mut out);
        // 0x40 + 255 x 127/255
        assert_eq!(pixel(&out, 120, 120), [191, 191, 191, 255]);
        let v = two_planes(&mut ram, true, false, 0x00ff_ffff, 0x8040_4040);
        v.render(&ram, &mut out);
        // 0x40 x 128/255 + 255 x 127/255
        assert_eq!(pixel(&out, 120, 120), [159, 159, 159, 255]);
        // fully transparent: the plane below shows, in both modes
        for premulti in [true, false] {
            let v = two_planes(&mut ram, true, premulti, 0x0012_3456, 0x0000_0000);
            v.render(&ram, &mut out);
            assert_eq!(pixel(&out, 120, 120), [0x12, 0x34, 0x56, 255]);
        }
        // opaque: covers
        let v = two_planes(&mut ram, true, true, 0x00ff_ffff, 0xff01_0203);
        v.render(&ram, &mut out);
        assert_eq!(pixel(&out, 120, 120), [1, 2, 3, 255]);
    }

    /// A plane's global alpha ("alpha" property) with a format without alpha:
    /// Ag x Cs + (1 - Ag) x Cd.
    #[test]
    fn global_alpha_of_an_xrgb_plane() {
        let mut ram = vec![0u8; 8 << 20];
        let mut out = Vec::new();
        let mut v = two_planes(&mut ram, false, true, 0x0000_0000, 0x00ff_ffff);
        write_mixer(&mut v, MIX0, parse_alpha(true, true, false, true, 0x80, 0xff));
        v.render(&ram, &mut out);
        assert_eq!(pixel(&out, 120, 120), [128, 128, 128, 255]);
    }

    /// What Linux leaves in OVL_LAYER_SEL with two planes on VP0 (all eight layers): the
    /// layers above select Cluster0 again, with mixers it does not set up.
    #[test]
    fn a_window_on_several_layers_is_drawn_once() {
        let mut ram = vec![0u8; 8 << 20];
        let mut out = Vec::new();
        let mut v = two_planes(&mut ram, false, true, 0x00ff_ffff, 0x0010_2030);
        v.write(OVL_LAYER_SEL, 0x0000_0020, 0);
        v.write(OVL_PORT_SEL, 0x7887, 0);
        v.render(&ram, &mut out);
        assert_eq!(pixel(&out, 120, 120), [0x10, 0x20, 0x30, 255]);
        assert_eq!(pixel(&out, 10, 10), [0xff, 0xff, 0xff, 255]);
    }

    /// Skipping a Cluster mixer that passes win0 through changes nothing: Linux's set-up for
    /// an opaque plane does; one with global alpha 0x80 halves the colour.
    #[test]
    fn cluster_mixer_pass_through_is_exact() {
        let opaque = Mixer::new(parse_alpha(false, false, false, true, 0, 0xff));
        let half = Mixer::new(parse_alpha(false, false, false, true, 0, 0x80));
        assert!(opaque.passes_dst() && !half.passes_dst());
        assert_eq!(half.mix([0; 4], [200, 100, 50, 255]), [100, 50, 25, 128]);
        let mut regs = 0u32;
        for _ in 0..4096 {
            // a fixed pseudo-random walk over control values and pixels
            regs = regs.wrapping_mul(1_103_515_245).wrapping_add(12345);
            let pick = |r: u32| r & 0x00ff_01ff;
            let m = Mixer::new([pick(regs), pick(regs.rotate_left(7)) | 0x00ff_0000 * (regs >> 31), pick(regs.rotate_left(13)), pick(regs.rotate_left(19))]);
            if m.passes_dst() {
                for p in [[0, 0, 0, 0], [10, 20, 30, 40], [200, 100, 50, 255], [255, 255, 255, 0], [1, 128, 254, 127]] {
                    assert_eq!(m.mix([0; 4], p), p, "{regs:08x}");
                }
            }
        }
    }

    /// Mixers Linux has not set up (ALPHA_EN clear): the upper layer covers.
    #[test]
    fn unset_mixers_cover() {
        let mut ram = vec![0u8; 8 << 20];
        let mut out = Vec::new();
        let mut v = two_planes(&mut ram, true, true, 0x00ff_ffff, 0x0010_2030);
        write_mixer(&mut v, CLUSTER0_MIX, [0; 4]);
        write_mixer(&mut v, MIX0, [0; 4]);
        v.render(&ram, &mut out);
        assert_eq!(pixel(&out, 120, 120), [0x10, 0x20, 0x30, 255]);
        assert_eq!(pixel(&out, 10, 10), [0xff, 0xff, 0xff, 255]);
    }

    /// DRM's blend of one colour channel (all values 0..255): "Pre-multiplied" gives
    /// `Ag Cs + (1 - Ag As) Cd`, "Coverage" gives `Ag As Cs + (1 - Ag As) Cd`.
    fn drm_blend(premulti: bool, cs: u8, a_s: u8, ag: u8, cd: f64) -> f64 {
        let (cs, a_s, ag) = (f64::from(cs), f64::from(a_s) / 255.0, f64::from(ag) / 255.0);
        if premulti { ag * cs + (1.0 - ag * a_s) * cd } else { ag * a_s * cs + (1.0 - ag * a_s) * cd }
    }

    /// The pixel is grey `v` within 1 (8-bit rounding in the mixers).
    fn assert_grey(px: [u8; 4], v: f64) {
        for c in &px[..3] {
            assert!((f64::from(*c) - v).abs() <= 1.0, "{px:?}, expected {v:.2}");
        }
    }

    fn fill(ram: &mut [u8], addr: u32, pixels: usize, px: u32) {
        for i in 0..pixels {
            ram[addr as usize + 4 * i..][..4].copy_from_slice(&px.to_le_bytes());
        }
    }

    /// Esmart1 full-screen (1280x720 at 0x10_0000) at the bottom, filled with `under`.
    fn esmart1_bottom(v: &mut Vop2, ram: &mut [u8], under: u32) {
        fill(ram, 0x10_0000, 1280 * 720, under);
        let b = 0x1a00;
        v.write(b + 0x14, 0x10_0000, 0);
        v.write(b + 0x1c, 1280, 0);
        v.write(b + 0x20, 719 << 16 | 1279, 0);
        v.write(b + 0x24, 719 << 16 | 1279, 0);
        v.write(b + 0x10, 1, 0);
    }

    /// Cluster1 as a 64x64 window at (100, 100), filled with `over` (format 0: ARGB8888 and
    /// XRGB8888 alike), and Cluster0 off.
    fn cluster1_overlay(v: &mut Vop2, ram: &mut [u8], over: u32) {
        let ov = 0x60_0000u32;
        fill(ram, ov, 64 * 64, over);
        v.write(0x1000, 0, 0);
        let b = 0x1200;
        v.write(b + 0x100, 1, 0);
        v.write(b + 0x10, ov, 0);
        v.write(b + 0x18, 64, 0);
        v.write(b + 0x20, 63 << 16 | 63, 0);
        v.write(b + 0x24, 63 << 16 | 63, 0);
        v.write(b + 0x28, 100 << 16 | 100, 0);
        v.write(b, 1, 0);
    }

    /// Esmart1 at the bottom, Cluster1 above it, the mixers as mainline sets them up for a
    /// Cluster plane at zpos 1: its Cluster mixer with the plane's global alpha `ag`, then MIX0
    /// with per-pixel alpha ("Mix output data only have pixel alpha").
    fn cluster_overlay_scene(ram: &mut [u8], premulti: bool, ag: u32, under: u32, over: u32) -> Vop2 {
        let mut v = Vop2::default();
        modeset(&mut v, 0x10_0000);
        esmart1_bottom(&mut v, ram, under);
        cluster1_overlay(&mut v, ram, over);
        v.write(OVL_LAYER_SEL, 0x7654_2013, 0); // layer 0 Esmart1, layer 1 Cluster1
        v.write(OVL_PORT_SEL, 0x7881, 0);
        write_mixer(&mut v, CLUSTER0_MIX + 0x10, parse_alpha(false, false, false, true, 0, ag));
        write_mixer(&mut v, MIX0, parse_alpha(premulti, true, true, true, 0xff, 0xff));
        v
    }

    /// A Cluster window as an overlay plane: ARGB grey 0x40 at alpha 0x80 over white, with
    /// and without the plane's global alpha, in both blend modes, as DRM defines them. The
    /// exception is "Coverage" with global alpha: the Cluster mixer multiplies the colour by
    /// Ag (to make it pre-multiplied), and MIX0, told the colour is not pre-multiplied,
    /// multiplies it by its alpha As x Ag again: `Ag^2 As Cs + (1 - Ag As) Cd`. Rockchip's
    /// kernel avoids that by telling MIX0 that Cluster data is always pre-multiplied.
    #[test]
    fn cluster_window_as_an_overlay_plane() {
        let mut ram = vec![0u8; 8 << 20];
        let mut out = Vec::new();
        for (premulti, ag) in [(true, 0xff), (false, 0xff), (true, 0x80), (false, 0x80)] {
            let v = cluster_overlay_scene(&mut ram, premulti, ag, 0x00ff_ffff, 0x8040_4040);
            v.render(&ram, &mut out);
            let want = match (premulti, ag) {
                (false, 0x80) => {
                    let (g, a) = (128.0 / 255.0, 128.0 / 255.0);
                    g * g * a * 64.0 + (1.0 - g * a) * 255.0
                }
                _ => drm_blend(premulti, 0x40, 0x80, ag as u8, 255.0),
            };
            assert_grey(pixel(&out, 120, 120), want);
            assert_grey(pixel(&out, 10, 10), 255.0);
            assert_grey(pixel(&out, 164, 120), 255.0);
        }
        // the docstring values of tests/gpu/rockchip-kms-planes.py, over white and grey 0x80
        for (premulti, under, want) in [(true, 255, 191), (true, 128, 128), (false, 255, 159), (false, 128, 96)] {
            let v = cluster_overlay_scene(&mut ram, premulti, 0xff, under * 0x1_0101, 0x8040_4040);
            v.render(&ram, &mut out);
            assert_grey(pixel(&out, 120, 120), f64::from(want));
        }
    }

    /// An XRGB8888 plane on a Cluster window (issue #54): the hardware sees ARGB8888, and the
    /// mixers decide. As mainline Linux sets them up (per-pixel alpha) an X byte of 0 adds the
    /// plane to what is below (Cs + (1 - 0) Cd, saturated); as Rockchip's kernel sets up the
    /// Cluster mixer for a format without alpha (the destination alpha is the global alpha,
    /// not the pixel's), the plane covers.
    #[test]
    fn xrgb_on_a_cluster_overlay_follows_the_mixers() {
        let mut ram = vec![0u8; 8 << 20];
        let mut out = Vec::new();
        let mut v = cluster_overlay_scene(&mut ram, true, 0xff, 0x0080_8080, 0x00ff_0000);
        v.render(&ram, &mut out);
        assert_eq!(pixel(&out, 120, 120), [255, 128, 128, 255]);
        // Rockchip's vop2_setup_cluster_alpha: dst_pixel_alpha_en = is_alpha_support(format),
        // and its vop2_parse_alpha then sets the destination alpha's blend mode to global
        let mut regs = parse_alpha(true, false, false, false, 0, 0xff);
        regs[3] &= !(3 << 2);
        write_mixer(&mut v, CLUSTER0_MIX + 0x10, regs);
        v.render(&ram, &mut out);
        assert_eq!(pixel(&out, 120, 120), [255, 0, 0, 255]);
        // ... and an ARGB plane still blends by its alpha there
        fill(&mut ram, 0x60_0000, 64 * 64, 0x8040_4040);
        write_mixer(&mut v, CLUSTER0_MIX + 0x10, parse_alpha(true, false, false, true, 0, 0xff));
        v.render(&ram, &mut out);
        assert_grey(pixel(&out, 120, 120), drm_blend(true, 0x40, 0x80, 0xff, 128.0));
    }

    /// A bottom plane whose global alpha is below opaque (issue #51) is dimmed all over, not
    /// only under the plane above it: Esmart1 white at alpha 0x80 over the black background,
    /// with ARGB grey 0x40 at alpha 0x80 ("Pre-multiplied") on Esmart0 above it. Linux sets
    /// MIX0 to `Cs + (1 - As) Cd Agd` and the HDR0 mixer to pass the result on.
    #[test]
    fn bottom_plane_with_global_alpha_is_dimmed_everywhere() {
        let mut ram = vec![0u8; 8 << 20];
        let mut out = Vec::new();
        let mut v = two_planes(&mut ram, true, true, 0x00ff_ffff, 0x8040_4040);
        esmart1_bottom(&mut v, &mut ram, 0x00ff_ffff);
        v.write(0x1000, 0, 0); // Cluster0 off
        v.write(OVL_LAYER_SEL, 0x7654_1023, 0); // layer 0 Esmart1, layer 1 Esmart0
        // vop2_setup_alpha with bottom_layer_alpha_en: dst not pre-multiplied, Agd 0x80
        write_mixer(&mut v, MIX0, parse_alpha(true, false, true, true, 0xff, 0x80));
        write_mixer(&mut v, HDR0_MIX, parse_alpha(true, true, true, true, 0xff, 0xff));
        v.render(&ram, &mut out);
        let bottom = 255.0 * 128.0 / 255.0;
        assert_grey(pixel(&out, 10, 10), bottom);
        assert_grey(pixel(&out, 1279, 719), bottom);
        assert_grey(pixel(&out, 99, 120), bottom);
        assert_grey(pixel(&out, 120, 120), drm_blend(true, 0x40, 0x80, 0xff, bottom));

        // the same picture with the bottom plane on Cluster0, whose own mixer applies the
        // global alpha (Linux then sets MIX0 up as usual and leaves HDR0 off)
        let mut v = two_planes(&mut ram, true, true, 0x00ff_ffff, 0x8040_4040);
        write_mixer(&mut v, CLUSTER0_MIX, parse_alpha(false, false, false, true, 0, 0x80));
        v.render(&ram, &mut out);
        assert_grey(pixel(&out, 10, 10), bottom);
        assert_grey(pixel(&out, 120, 120), drm_blend(true, 0x40, 0x80, 0xff, bottom));
    }

    /// "Coverage" with global alpha on a plane without pixel alpha: the colour is multiplied
    /// by the global alpha for the colour mode and again for the factor (see `Mixer`), so
    /// white at Ag = 0x80 over black gives 255 x (128/255)^2, not DRM's 128.
    #[test]
    fn coverage_with_global_alpha_and_no_pixel_alpha() {
        let mut ram = vec![0u8; 8 << 20];
        let mut out = Vec::new();
        let mut v = two_planes(&mut ram, false, false, 0x0000_0000, 0x00ff_ffff);
        write_mixer(&mut v, MIX0, parse_alpha(false, true, false, true, 0x80, 0xff));
        v.render(&ram, &mut out);
        assert_grey(pixel(&out, 120, 120), 255.0 * (128.0f64 / 255.0).powi(2));
        assert_grey(pixel(&out, 10, 10), 0.0);
    }

    /// Skipping a mixer outside its window when an empty source keeps the destination is
    /// exact; Linux's usual overlay set-up does, the one above a bottom plane with global
    /// alpha does not. Exhaustive over what an empty source depends on: the destination colour
    /// and alpha controls (colour mode, alpha mode, blend mode, factor) and both global alphas.
    #[test]
    fn empty_source_skip_is_exact() {
        assert!(Mixer::new(parse_alpha(true, true, true, true, 0xff, 0xff)).empty_keeps_dst());
        assert!(Mixer::new(parse_alpha(false, true, false, true, 0x80, 0xff)).empty_keeps_dst());
        assert!(!Mixer::new(parse_alpha(true, false, true, true, 0xff, 0x80)).empty_keeps_dst());
        let pixels = [[0, 0, 0, 0], [10, 20, 30, 40], [200, 100, 50, 255], [255, 255, 255, 0], [1, 128, 254, 127], [255, 0, 128, 1]];
        // (skips seen, and the destination colour and alpha factors they used)
        let (mut kept, mut factors) = (0, [[false; 8]; 2]);
        for dc in 0..256u32 {
            // (bit 4, the saturation mode, is not modelled)
            if dc & 0x10 != 0 {
                continue;
            }
            for da in (0..256u32).filter(|v| v & 0x10 == 0) {
                for (gs, gd) in [(0xff, 0xff), (0xff, 0x80), (0x80, 0xff), (0x80, 0x80), (0, 0)] {
                    let m = Mixer::new([1 << 8 | gs << 16, dc | gd << 16, 0, da]);
                    if m.empty_keeps_dst() {
                        kept += 1;
                        factors[0][(dc >> 5) as usize] = true;
                        factors[1][(da >> 5) as usize] = true;
                        for p in pixels {
                            assert_eq!(m.mix_empty(p), p, "{dc:02x} {da:02x} {gs:02x} {gd:02x}");
                        }
                    }
                }
            }
        }
        assert!(kept > 500, "{kept}");
        // every factor that can keep the destination did: one, 1 - src alpha, dst alpha, dst
        // global (colour); one, 1 - src alpha, src global (alpha)
        assert_eq!(factors, [[false, true, false, true, true, true, false, false], [false, true, false, true, true, false, false, false]]);
    }

    /// "Coverage" on a plane with pixel alpha and global alpha: `vop2_parse_alpha` picks
    /// "per-pixel times global", for which it sets the colour mode to pre-multiplied and the
    /// factor to the global alpha whatever the blend mode, so the plane is mixed as if
    /// pre-multiplied: `Ag Cs + (1 - Ag As) Cd` (223 here), not DRM's Coverage (207).
    #[test]
    fn coverage_with_pixel_and_global_alpha_mixes_as_pre_multiplied() {
        let mut ram = vec![0u8; 8 << 20];
        let mut out = Vec::new();
        let mut v = two_planes(&mut ram, true, false, 0x00ff_ffff, 0x8040_4040);
        write_mixer(&mut v, MIX0, parse_alpha(false, true, true, true, 0x80, 0xff));
        v.render(&ram, &mut out);
        assert_grey(pixel(&out, 120, 120), drm_blend(true, 0x40, 0x80, 0x80, 255.0));
        assert_grey(pixel(&out, 10, 10), 255.0);
    }

    /// Esmart1 white at the bottom with plane alpha 0x80, its X byte `x`; Esmart0 above it
    /// with `over` as Linux sets the mixers up: MIX0 `Cs + (1 - As) Cd Agd` in the plane's
    /// blend mode, and the HDR0 mixer with that blend mode as its source's colour mode.
    fn bottom_alpha_scene(ram: &mut [u8], premulti: bool, x: u32, over: u32) -> Vop2 {
        let mut v = two_planes(ram, true, premulti, 0x00ff_ffff, over);
        esmart1_bottom(&mut v, ram, x << 24 | 0x00ff_ffff);
        v.write(0x1000, 0, 0); // Cluster0 off
        v.write(OVL_LAYER_SEL, 0x7654_1023, 0); // layer 0 Esmart1, layer 1 Esmart0
        write_mixer(&mut v, MIX0, parse_alpha(premulti, false, true, true, 0xff, 0x80));
        write_mixer(&mut v, HDR0_MIX, parse_alpha(premulti, true, true, true, 0xff, 0xff));
        v
    }

    /// A "Coverage" plane at zpos 1 above a bottom plane with plane alpha, whatever the bottom
    /// plane's X byte: DRM's values (the HDR0 mixer takes its source as pre-multiplied).
    #[test]
    fn coverage_plane_above_a_bottom_plane_with_global_alpha() {
        let mut ram = vec![0u8; 8 << 20];
        let mut out = Vec::new();
        let bottom = 255.0 * 128.0 / 255.0;
        for x in [0, 0xff] {
            let v = bottom_alpha_scene(&mut ram, false, x, 0x8040_4040);
            v.render(&ram, &mut out);
            assert_grey(pixel(&out, 10, 10), bottom);
            assert_grey(pixel(&out, 120, 120), drm_blend(false, 0x40, 0x80, 0xff, bottom));
        }
    }

    /// A bottom plane with plane alpha and nothing above it: Linux sets up no mixer that
    /// applies the alpha (MIX0 is for the layer above, which has no window, and HDR0 passes
    /// the picture on), so it shows at full brightness, not DRM's half.
    #[test]
    fn lone_bottom_plane_with_global_alpha() {
        let mut ram = vec![0u8; 8 << 20];
        let mut out = Vec::new();
        for x in [0, 0xff] {
            let mut v = bottom_alpha_scene(&mut ram, true, x, 0);
            v.write(0x1800 + 0x10, 0, 0); // Esmart0 off
            write_mixer(&mut v, MIX0, [0; 4]);
            // vop2_setup_alpha with no plane at zpos 1: gpremulti_en stays 0
            write_mixer(&mut v, HDR0_MIX, parse_alpha(false, true, true, true, 0xff, 0xff));
            v.render(&ram, &mut out);
            assert_grey(pixel(&out, 10, 10), 255.0);
            assert_grey(pixel(&out, 120, 120), 255.0);
        }
    }

    /// A window above a bottom plane with plane alpha that gives no pixels (a format not
    /// decoded, or a buffer outside RAM): its mixer still dims the bottom plane everywhere.
    #[test]
    fn mixer_of_a_window_without_pixels_still_acts() {
        let mut ram = vec![0u8; 8 << 20];
        let mut out = Vec::new();
        let bottom = 255.0 * 128.0 / 255.0;
        let mut v = bottom_alpha_scene(&mut ram, true, 0, 0x8040_4040);
        v.write(0x1800 + 0x10, 1 | 4 << 1, 0); // Esmart0 in format 4 (YUV420SP)
        v.render(&ram, &mut out);
        assert_grey(pixel(&out, 10, 10), bottom);
        assert_grey(pixel(&out, 120, 120), bottom);
        let mut v = bottom_alpha_scene(&mut ram, true, 0, 0x8040_4040);
        v.write(0x1800 + 0x14, 0xff00_0000, 0); // Esmart0's buffer beyond RAM
        v.render(&ram, &mut out);
        assert_grey(pixel(&out, 10, 10), bottom);
        assert_grey(pixel(&out, 120, 120), bottom);
    }

    #[test]
    fn nothing_before_linux_drives_it() {
        let v = Vop2::default();
        assert_eq!(v.render(&[], &mut Vec::new()), None);
    }

    /// Guest-chosen sizes and addresses: no overflow, no huge buffers.
    #[test]
    fn oversized_modes_and_mirrored_windows_at_0_are_safe() {
        let mut v = Vop2::default();
        let ram = vec![0u8; 8 << 20];
        modeset(&mut v, 0);
        v.write(VP0 + HACT, 0xffff, 0);
        v.write(VP0 + VACT, 0xffff, 0);
        assert!(!v.running());
        assert_eq!(v.size(), None);
        v.write(VP0 + HACT, 260 << 16 | (260 + 1280), 0);
        v.write(VP0 + VACT, 25 << 16 | (25 + 720), 0);
        v.write(0x1000 + 0x54, 1 << 3, 0); // Y mirror, YRGB_MST 0
        let mut out = Vec::new();
        assert_eq!(v.render(&ram, &mut out), Some((1280, 720)));
    }
}
