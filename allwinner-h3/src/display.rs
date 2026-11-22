//! H3 display pipeline as Linux's `sun4i-drm` drives it: DE2 mixer 0 composes planes
//! from RAM, TCON0 (the TV-timing "lcd-controller@1c0c000") produces vblank interrupts,
//! and the Synopsys DesignWare HDMI controller (`peripherals::dw_hdmi`) with the H3 HDMI PHY
//! reports a connected monitor whose EDID is read over the controller's DDC I2C master.
//!
//! Only what the drivers observe is modelled: register storage, the controller IDs, hot
//! plug, EDID, PHY calibration status, interrupts. The frame shown is composed on demand
//! (`render`) from the mixer's blender and layer registers.

/// DE2 mixer 0 (`mixer@1100000`): global, blender and channel registers (the unit's first
/// 0x6000 bytes; scalers and colour units above it stay read-back stubs).
pub const MIXER0_BASE: u64 = 0x0110_0000;
const MIXER_SIZE: u64 = 0x6000;
/// TCON0, TV timing controller in front of HDMI.
pub const TCON0_BASE: u64 = 0x01c0_c000;
const TCON_SIZE: u64 = 0x1000;
/// DesignWare HDMI TX (byte registers, `reg-io-width = <1>`).
pub const HDMI_BASE: u64 = 0x01ee_0000;
const HDMI_SIZE: u64 = 0x1_0000;
/// H3 HDMI PHY.
pub const HDMI_PHY_BASE: u64 = 0x01ef_0000;
const HDMI_PHY_SIZE: u64 = 0x1000;
/// TCON0 interrupt: SPI 86; HDMI: SPI 88.
pub const TCON0_IRQ: usize = 32 + 86;
pub const HDMI_IRQ: usize = 32 + 88;

/// Largest output width or height composed.
const MAX_SIZE: u32 = 4096;
/// 24 MHz ticks per 60 Hz frame.
const FRAME_TICKS: u64 = 24_000_000 / 60;

// TCON registers.
const TCON_GCTL: usize = 0x00 / 4;
const TCON_GINT0: usize = 0x04 / 4;
const TCON0_CTL: usize = 0x40 / 4;
const TCON1_CTL: usize = 0x90 / 4;
const ENABLE: u32 = 1 << 31;

pub use peripherals::dw_hdmi::{edid, Mode};
use peripherals::dw_hdmi::{DwHdmi, Identity};

// H3 HDMI PHY: analog status (resistor calibration done, calibration and B values).
const PHY_ANA_STS: usize = 0x38 / 4;
const PHY_ANA_STS_VALUE: u32 = 0x10 << 11 | 1 << 7 | 0x20;

// Mixer registers (byte offsets in the unit).
const MIXER_GLOBAL_CTL: u32 = 0x0;
const BLD: u32 = 0x1000;
const BLD_PIPE_CTL: u32 = BLD;
const BLD_ROUTE: u32 = BLD + 0x80;
const BLD_PREMULTIPLY: u32 = BLD + 0x84;
const BLD_BKCOLOR: u32 = BLD + 0x88;
const BLD_OUTSIZE: u32 = BLD + 0x8c;
const CH_BASE: u32 = 0x2000;
const CH_SIZE: u32 = 0x1000;
/// Mixer 0 of the H3: one VI channel (channel 0) and three UI channels.
const VI_CHANNELS: u32 = 1;
const CHANNELS: u32 = 4;

/// The display pipeline's registers and state.
pub struct Display {
    mixer: Vec<u32>,
    tcon: Vec<u32>,
    hdmi: DwHdmi,
    phy: Vec<u32>,
    /// The monitor's preferred mode.
    pub mode: Mode,
    /// Tick of the next vblank while TCON0 runs.
    next_vblank: Option<u64>,
    /// Frames scanned out since reset.
    pub frames: u64,
}

impl Default for Display {
    fn default() -> Self {
        Self::new(Mode::MODES[1])
    }
}

/// Merge a `width`-byte write at byte `offset` into a word.
fn merge(old: u32, offset: u64, width: u64, value: u32) -> u32 {
    let shift = (offset & 3) * 8;
    let mask = if width >= 4 {
        u32::MAX
    } else {
        ((1u32 << (width * 8)) - 1) << shift
    };
    old & !mask | (value << shift) & mask
}

impl Display {
    pub fn new(mode: Mode) -> Self {
        Self {
            mixer: vec![0; (MIXER_SIZE / 4) as usize],
            tcon: vec![0; (TCON_SIZE / 4) as usize],
            hdmi: DwHdmi::new(Identity::H3, &mode),
            phy: vec![0; (HDMI_PHY_SIZE / 4) as usize],
            mode,
            next_vblank: None,
            frames: 0,
        }
    }

    fn in_region(addr: u64, width: u64, base: u64, size: u64) -> bool {
        addr.checked_sub(base)
            .and_then(|offset| offset.checked_add(width))
            .is_some_and(|end| end <= size)
    }

    /// Register read; `None` if no display unit decodes `addr`.
    pub fn read(&mut self, addr: u64, width: u64) -> Option<u32> {
        let word = |regs: &[u32], offset: u64| regs[(offset / 4) as usize] >> ((offset & 3) * 8);
        if Self::in_region(addr, width, MIXER0_BASE, MIXER_SIZE) {
            return Some(word(&self.mixer, addr - MIXER0_BASE));
        }
        if Self::in_region(addr, width, TCON0_BASE, TCON_SIZE) {
            return Some(word(&self.tcon, addr - TCON0_BASE));
        }
        if Self::in_region(addr, width, HDMI_PHY_BASE, HDMI_PHY_SIZE) {
            let offset = addr - HDMI_PHY_BASE;
            let index = (offset / 4) as usize;
            let value = if index == PHY_ANA_STS {
                PHY_ANA_STS_VALUE
            } else {
                self.phy[index]
            };
            return Some(value >> ((offset & 3) * 8));
        }
        if Self::in_region(addr, width, HDMI_BASE, HDMI_SIZE) {
            let offset = (addr - HDMI_BASE) as usize;
            return Some(
                (0..width as usize).fold(0, |v, k| v | (self.hdmi.read(offset + k) as u32) << (8 * k)),
            );
        }
        None
    }

    /// Register write; false if no display unit decodes `addr`.
    pub fn write(&mut self, addr: u64, width: u64, value: u32) -> bool {
        if Self::in_region(addr, width, MIXER0_BASE, MIXER_SIZE) {
            let offset = addr - MIXER0_BASE;
            let reg = &mut self.mixer[(offset / 4) as usize];
            *reg = merge(*reg, offset, width, value);
            return true;
        }
        if Self::in_region(addr, width, TCON0_BASE, TCON_SIZE) {
            let offset = addr - TCON0_BASE;
            let reg = &mut self.tcon[(offset / 4) as usize];
            *reg = merge(*reg, offset, width, value);
            return true;
        }
        if Self::in_region(addr, width, HDMI_PHY_BASE, HDMI_PHY_SIZE) {
            let offset = addr - HDMI_PHY_BASE;
            let reg = &mut self.phy[(offset / 4) as usize];
            *reg = merge(*reg, offset, width, value);
            return true;
        }
        if Self::in_region(addr, width, HDMI_BASE, HDMI_SIZE) {
            let offset = (addr - HDMI_BASE) as usize;
            for k in 0..width as usize {
                self.hdmi.write(offset + k, (value >> (8 * k)) as u8);
            }
            return true;
        }
        false
    }

    pub fn hdmi_irq(&self) -> bool {
        self.hdmi.irq()
    }

    fn tcon_running(&self) -> bool {
        self.tcon[TCON_GCTL] & ENABLE != 0
            && (self.tcon[TCON0_CTL] | self.tcon[TCON1_CTL]) & ENABLE != 0
    }

    pub fn tcon_irq(&self) -> bool {
        let g = self.tcon[TCON_GINT0];
        (g >> 16) & g & 0xffff != 0
    }

    /// Advance to tick `now`: each vblank passed sets the vblank flag of the running
    /// timing channels.
    pub fn update(&mut self, now: u64) {
        if !self.tcon_running() {
            self.next_vblank = None;
            return;
        }
        let next = *self.next_vblank.get_or_insert(now + FRAME_TICKS);
        if now >= next {
            let frames = (now - next) / FRAME_TICKS + 1;
            self.frames += frames;
            self.next_vblank = Some(next + frames * FRAME_TICKS);
            let mut flags = 0;
            if self.tcon[TCON0_CTL] & ENABLE != 0 {
                flags |= 1 << 15;
            }
            if self.tcon[TCON1_CTL] & ENABLE != 0 {
                flags |= 1 << 14;
            }
            self.tcon[TCON_GINT0] |= flags;
        }
    }

    /// The tick of the next vblank, if the timing controller runs.
    pub fn next_deadline(&self) -> Option<u64> {
        self.next_vblank
    }

    fn mixer_reg(&self, offset: u32) -> u32 {
        self.mixer[(offset / 4) as usize]
    }

    /// Size of the picture on the HDMI output, if the pipeline scans out.
    pub fn shown_size(&self) -> Option<(u32, u32)> {
        if !self.tcon_running() || self.mixer_reg(MIXER_GLOBAL_CTL) & 1 == 0 {
            return None;
        }
        // HDMI goes up to 4096x2160; larger guest-programmed sizes are clipped so a
        // render cannot allocate gigabytes.
        let size = self.mixer_reg(BLD_OUTSIZE);
        Some((((size & 0x1fff) + 1).min(MAX_SIZE), ((size >> 16 & 0x1fff) + 1).min(MAX_SIZE)))
    }

    /// Compose the mixer's output from `ram` (based at `ram_base`) as RGBA.
    pub fn render(&self, ram: &[u8], ram_base: u64, out: &mut Vec<u8>) -> Option<(u32, u32)> {
        let (w, h) = self.shown_size()?;
        let mut frame = vec![self.mixer_reg(BLD_BKCOLOR) | 0xff00_0000; (w * h) as usize];
        let pipes = self.mixer_reg(BLD_PIPE_CTL) >> 8 & 0x1f;
        let route = self.mixer_reg(BLD_ROUTE);
        let premultiplied = self.mixer_reg(BLD_PREMULTIPLY);
        for pipe in 0..5 {
            if pipes & 1 << pipe == 0 {
                continue;
            }
            let channel = route >> (4 * pipe) & 0xf;
            if channel >= CHANNELS {
                continue;
            }
            let coord = self.mixer_reg(BLD + 0xc + 0x10 * pipe);
            let insize = self.mixer_reg(BLD + 0x8 + 0x10 * pipe);
            let dst = (
                coord & 0xffff,
                coord >> 16,
                (insize & 0x1fff) + 1,
                (insize >> 16 & 0x1fff) + 1,
            );
            self.draw_channel(
                channel,
                dst,
                premultiplied & 1 << pipe != 0,
                ram,
                ram_base,
                &mut frame,
                w,
                h,
            );
        }
        out.clear();
        out.reserve(frame.len() * 4);
        for p in frame {
            out.extend_from_slice(&[(p >> 16) as u8, (p >> 8) as u8, p as u8, 255]);
        }
        Some((w, h))
    }

    /// Blend the layers of `channel` into the pipe rectangle `dst` (x, y, w, h), scaling
    /// the channel's overlay to it (nearest neighbour).
    #[allow(clippy::too_many_arguments)]
    fn draw_channel(
        &self,
        channel: u32,
        dst: (u32, u32, u32, u32),
        premultiplied: bool,
        ram: &[u8],
        ram_base: u64,
        frame: &mut [u32],
        w: u32,
        h: u32,
    ) {
        let base = CH_BASE + channel * CH_SIZE;
        let vi = channel < VI_CHANNELS;
        let (stride, ovl) = if vi {
            (0x30, base + 0xe8)
        } else {
            (0x20, base + 0x88)
        };
        let ovl = self.mixer_reg(ovl);
        let (ow, oh) = ((ovl & 0x1fff) + 1, (ovl >> 16 & 0x1fff) + 1);
        let (dx, dy, dw, dh) = dst;
        for layer in 0..4 {
            let regs = base + stride * layer;
            let attr = self.mixer_reg(regs);
            if attr & 1 == 0 {
                continue;
            }
            let format = attr >> 8 & 0x1f;
            if vi && attr & (1 << 15) == 0 {
                continue; // YUV layers are not modelled
            }
            let Some(bpp) = bytes_per_pixel(format) else {
                continue;
            };
            let size = self.mixer_reg(regs + 4);
            let (lw, lh) = ((size & 0x1fff) + 1, (size >> 16 & 0x1fff) + 1);
            let lcoord = self.mixer_reg(regs + 8);
            let (lx, ly) = (lcoord & 0xffff, lcoord >> 16);
            let pitch = self.mixer_reg(regs + 0xc) as u64;
            let address = self.mixer_reg(regs + if vi { 0x18 } else { 0x10 }) as u64;
            let global = attr >> 24;
            let alpha_mode = attr >> 1 & 3;
            for y in 0..dh {
                let fy = dy + y;
                if fy >= h {
                    break;
                }
                let sy = y * oh / dh;
                if sy < ly || sy >= ly + lh {
                    continue;
                }
                let row = address + (sy - ly) as u64 * pitch;
                for x in 0..dw {
                    let fx = dx + x;
                    if fx >= w {
                        break;
                    }
                    let sx = x * ow / dw;
                    if sx < lx || sx >= lx + lw {
                        continue;
                    }
                    let Some(at) = (row + (sx - lx) as u64 * bpp as u64).checked_sub(ram_base)
                    else {
                        continue;
                    };
                    let Some(bytes) = ram.get(at as usize..at as usize + bpp) else {
                        continue;
                    };
                    let pixel = decode(format, bytes);
                    let alpha = match alpha_mode {
                        0 => pixel >> 24,
                        1 => global,
                        _ => (pixel >> 24) * global / 255,
                    };
                    let target = &mut frame[(fy * w + fx) as usize];
                    *target = blend(pixel, alpha, *target, premultiplied);
                }
            }
        }
    }
}

fn bytes_per_pixel(format: u32) -> Option<usize> {
    match format {
        0..=7 => Some(4),
        8 | 9 => Some(3),
        10..=19 => Some(2),
        _ => None,
    }
}

/// A pixel of mixer format `format` as ARGB8888.
fn decode(format: u32, b: &[u8]) -> u32 {
    let expand = |v: u32, bits: u32| {
        if bits == 0 {
            255
        } else {
            v * 255 / ((1 << bits) - 1)
        }
    };
    let argb = |a: u32, r: u32, g: u32, b: u32| a << 24 | r << 16 | g << 8 | b;
    match format {
        0..=7 => {
            let v = u32::from_le_bytes([b[0], b[1], b[2], b[3]]);
            let (a, r, g, bl) = match format & 3 {
                0 => (v >> 24, v >> 16 & 0xff, v >> 8 & 0xff, v & 0xff), // ARGB
                1 => (v >> 24, v & 0xff, v >> 8 & 0xff, v >> 16 & 0xff), // ABGR
                2 => (v & 0xff, v >> 24, v >> 16 & 0xff, v >> 8 & 0xff), // RGBA
                _ => (v & 0xff, v >> 8 & 0xff, v >> 16 & 0xff, v >> 24), // BGRA
            };
            argb(if format >= 4 { 255 } else { a }, r, g, bl)
        }
        8 => argb(255, b[2] as u32, b[1] as u32, b[0] as u32),
        9 => argb(255, b[0] as u32, b[1] as u32, b[2] as u32),
        _ => {
            let v = u16::from_le_bytes([b[0], b[1]]) as u32;
            // (alpha, red, green, blue) as (shift, bits)
            let layout: [(u32, u32); 4] = match format {
                10 => [(0, 0), (11, 5), (5, 6), (0, 5)],
                11 => [(0, 0), (0, 5), (5, 6), (11, 5)],
                12 => [(12, 4), (8, 4), (4, 4), (0, 4)],
                13 => [(12, 4), (0, 4), (4, 4), (8, 4)],
                14 => [(0, 4), (12, 4), (8, 4), (4, 4)],
                15 => [(0, 4), (4, 4), (8, 4), (12, 4)],
                16 => [(15, 1), (10, 5), (5, 5), (0, 5)],
                17 => [(15, 1), (0, 5), (5, 5), (10, 5)],
                18 => [(0, 1), (11, 5), (6, 5), (1, 5)],
                _ => [(0, 1), (1, 5), (6, 5), (11, 5)],
            };
            let [a, r, g, bl] =
                layout.map(|(shift, bits)| expand(v >> shift & ((1 << bits) - 1), bits));
            argb(a, r, g, bl)
        }
    }
}

/// Source-over blend of `src` with coverage `alpha` onto opaque `dst`.
fn blend(src: u32, alpha: u32, dst: u32, premultiplied: bool) -> u32 {
    if alpha >= 255 {
        return src | 0xff00_0000;
    }
    let channel = |shift: u32| {
        let (s, d) = (src >> shift & 0xff, dst >> shift & 0xff);
        let s = if premultiplied { s } else { s * alpha / 255 };
        (s + d * (255 - alpha) / 255).min(255) << shift
    };
    0xff00_0000 | channel(16) | channel(8) | channel(0)
}

emu_core::snap_struct!(Display { hdmi, mode, next_vblank, frames } fixed { mixer, tcon, phy });

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hdmi_controller_answers_on_the_bus() {
        let mut d = Display::default();
        assert_eq!(d.read(HDMI_BASE, 4), Some(0x01a0_2a13));
        assert_eq!(d.read(HDMI_BASE + 6, 1), Some(0xfe), "vendor PHY");
        assert!(d.write(HDMI_BASE + 0x01ff, 1, 0)); // unmute
        assert!(d.write(HDMI_BASE + 0x3006, 1, !0x02)); // hot plug unmasked
        assert!(d.write(HDMI_BASE + 0x3007, 1, 0x02)); // interrupt while plugged
        assert!(d.hdmi_irq());
    }

    #[test]
    fn tcon_raises_vblank_at_60_hz() {
        let mut d = Display::default();
        d.write(TCON0_BASE, 4, ENABLE);
        d.write(TCON0_BASE + 0x90, 4, ENABLE);
        d.write(TCON0_BASE + 4, 4, 1 << 30); // vblank interrupt of channel 1
        d.update(1000);
        assert!(!d.tcon_irq());
        d.update(1000 + FRAME_TICKS);
        assert!(d.tcon_irq());
        d.write(TCON0_BASE + 4, 4, 1 << 30); // acknowledge
        assert!(!d.tcon_irq());
        assert_eq!(d.next_deadline(), Some(1000 + 2 * FRAME_TICKS));
    }

    #[test]
    fn mixer_composes_a_ui_layer_from_ram() {
        let mut d = Display::default();
        let m =
            |d: &mut Display, reg: u32, v: u32| assert!(d.write(MIXER0_BASE + reg as u64, 4, v));
        d.write(TCON0_BASE, 4, ENABLE);
        d.write(TCON0_BASE + 0x90, 4, ENABLE);
        m(&mut d, MIXER_GLOBAL_CTL, 1);
        m(&mut d, BLD_OUTSIZE, (2 - 1) << 16 | (4 - 1));
        m(&mut d, BLD_BKCOLOR, 0xff00_0000);
        m(&mut d, BLD_ROUTE, 1); // pipe 0 <- channel 1 (first UI channel)
        m(&mut d, BLD_PIPE_CTL, 1 << 8);
        m(&mut d, BLD + 0x8, (2 - 1) << 16 | (2 - 1)); // pipe 0 is 2x2 at (1, 0)
        m(&mut d, BLD + 0xc, 1);
        let ch = CH_BASE + CH_SIZE;
        m(&mut d, ch, 1 | 4 << 8); // XRGB8888
        m(&mut d, ch + 4, (2 - 1) << 16 | (2 - 1));
        m(&mut d, ch + 0xc, 8);
        m(&mut d, ch + 0x10, 0x4000_0010);
        m(&mut d, ch + 0x88, (2 - 1) << 16 | (2 - 1));
        let mut ram = vec![0u8; 0x40];
        for (k, p) in [0x00ff_0000u32, 0x0000_ff00, 0x0000_00ff, 0x00ff_ffff]
            .iter()
            .enumerate()
        {
            ram[0x10 + 4 * k..0x14 + 4 * k].copy_from_slice(&p.to_le_bytes());
        }
        let mut out = Vec::new();
        assert_eq!(d.render(&ram, 0x4000_0000, &mut out), Some((4, 2)));
        let px = |x: usize, y: usize| &out[(y * 4 + x) * 4..(y * 4 + x) * 4 + 3];
        assert_eq!(px(0, 0), &[0, 0, 0]);
        assert_eq!(px(1, 0), &[255, 0, 0]);
        assert_eq!(px(2, 0), &[0, 255, 0]);
        assert_eq!(px(1, 1), &[0, 0, 255]);
        assert_eq!(px(2, 1), &[255, 255, 255]);
        assert_eq!(px(3, 1), &[0, 0, 0]);
    }
}
