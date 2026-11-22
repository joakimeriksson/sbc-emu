//! A display controller (`nvidia,tegra210-dc`): its timing, three windows with scaling and
//! blending, the cursor, the vertical blank interrupt and sync point. Registers are 32-bit words
//! (the driver's indices); each has an assembly copy, which the CPU writes, an armed copy
//! (STATE_CONTROL's update bits) and an active copy, which the armed one becomes at the next frame
//! (its activation bits), or at once while the controller is stopped.
use crate::smmu::{Group, Smmu};
use peripherals::dma::Memory;

pub const BASE: [u64; 2] = [0x5420_0000, 0x5424_0000];
/// Reset numbers of the two controllers.
pub const RESETS: [usize; 2] = [27, 26];
pub const IRQS: [usize; 2] = [32 + 0x49, 32 + 0x4a];
const REGS: usize = 0x1000;
pub const SIZE: u64 = 4 * REGS as u64;

const CONT_SYNCPT_VSYNC: usize = 0x028;
const DISPLAY_COMMAND: usize = 0x032;
const INT_STATUS: usize = 0x037;
const INT_MASK: usize = 0x038;
const INT_ENABLE: usize = 0x039;
const STATE_ACCESS: usize = 0x040;
const STATE_CONTROL: usize = 0x041;
const WIN_OPTIONS: usize = 0x402;
const REF_TO_SYNC: usize = 0x406;
const ACTIVE: usize = 0x409;
const CURSOR_START: usize = 0x43e;
const CURSOR_POSITION: usize = 0x440;
const BACKGROUND: usize = 0x4e4;
const CURSOR_START_HI: usize = 0x4ec;

const FRAME_END: u32 = 1 << 1;
const VBLANK: u32 = 1 << 2;

pub const WINDOWS: usize = 3;
/// A window's registers: 0x500.. at +0, 0x700.. at +0x180, 0x800.. at +0x1c0.
const fn window(w: usize) -> usize {
    0xa00 + 0x200 * w
}
const OPTIONS: usize = 0x180;
const DEPTH: usize = 0x183;
const POSITION: usize = 0x184;
const WSIZE: usize = 0x185;
const PRESCALED: usize = 0x186;
const DDA: usize = 0x189;
const STRIDE: usize = 0x18a;
const BLEND_LAYER: usize = 0x196;
const START: usize = 0x1c0;
const H_OFFSET: usize = 0x1c6;
const V_OFFSET: usize = 0x1c8;
const KIND: usize = 0x1cb;
const START_HI: usize = 0x1cd;

pub struct Dc {
    asm: Vec<u32>,
    arm: Vec<u32>,
    active: Vec<u32>,
    /// activation bits waiting for the frame's end
    pending: u32,
    int_status: u32,
    /// when the next vertical blank starts (ns)
    next_vblank: u64,
    pub frames: u64,
    /// the monitor's own mode: its totals (pixels, lines) and pixel clock (kHz)
    pub mode: Option<(u32, u32, u32)>,
}

/// The largest picture a controller scans out.
const MAX_SIZE: u32 = 4096;

/// Surfaces' addresses have 40 bits.
const ADDRESS: u64 = (1 << 40) - 1;

impl Default for Dc {
    fn default() -> Self {
        Self { asm: vec![0; REGS], arm: vec![0; REGS], active: vec![0; REGS], pending: 0, int_status: 0, next_vblank: 0, frames: 0, mode: None }
    }
}

/// The registers activation bit `b` covers.
fn covers(b: u32, r: usize) -> bool {
    match b {
        0 => r < 0xa00, // general
        1..=3 => (window(b as usize - 1)..window(b as usize)).contains(&r),
        7 => matches!(r, CURSOR_START..=0x441 | CURSOR_START_HI | 0x4f1),
        _ => false,
    }
}

impl Dc {
    /// The controller scans frames out.
    pub fn running(&self) -> bool {
        self.active[DISPLAY_COMMAND] >> 5 & 3 != 0 && self.size().is_some()
    }

    /// The visible size.
    pub fn size(&self) -> Option<(u32, u32)> {
        let (w, h) = (self.active[ACTIVE] & 0x7fff, self.active[ACTIVE] >> 16 & 0x7fff);
        (w != 0 && h != 0 && w <= MAX_SIZE && h <= MAX_SIZE).then_some((w, h))
    }

    /// The frame time (ns): the monitor's own mode at its pixel clock; its other modes all have
    /// 60 Hz.
    fn frame_time(&self) -> u64 {
        let sum = |shift: u32| (REF_TO_SYNC + 1..REF_TO_SYNC + 5).map(|r| self.active[r] >> shift & 0x7fff).sum::<u32>();
        match self.mode {
            Some((w, h, khz)) if (w, h) == (sum(0), sum(16)) => w as u64 * h as u64 * 1_000_000 / khz as u64,
            _ => 16_666_667,
        }
    }

    /// The controller was held in reset.
    pub fn reset(&mut self) {
        *self = Self { mode: self.mode, frames: self.frames, ..Self::default() };
    }

    pub fn irq(&self) -> bool {
        self.int_status & self.asm[INT_MASK] & self.asm[INT_ENABLE] != 0
    }

    pub fn read(&self, offset: u64) -> u32 {
        match offset as usize / 4 % REGS {
            INT_STATUS => self.int_status,
            // the activation bits read as set until they take effect
            STATE_CONTROL => self.pending,
            r @ (INT_MASK | INT_ENABLE | STATE_ACCESS) => self.asm[r],
            r => if self.asm[STATE_ACCESS] & 1 != 0 { self.active[r] } else { self.asm[r] },
        }
    }

    pub fn write(&mut self, offset: u64, v: u32, now: u64) {
        match offset as usize / 4 % REGS {
            INT_STATUS => self.int_status &= !v,
            STATE_CONTROL => {
                for b in (0..8).filter(|b| v >> (b + 8) & 1 != 0) {
                    for r in (0..REGS).filter(|&r| covers(b, r)) {
                        self.arm[r] = self.asm[r];
                    }
                }
                self.pending |= v & 0xff;
                if !self.running() {
                    self.activate();
                    if self.running() {
                        self.next_vblank = now + self.frame_time();
                    }
                }
            }
            r => self.asm[r] = v,
        }
    }

    fn activate(&mut self) {
        for b in (0..8).filter(|b| self.pending >> b & 1 != 0) {
            for r in (0..REGS).filter(|&r| covers(b, r)) {
                self.active[r] = self.arm[r];
            }
        }
        self.pending = 0;
    }

    /// Frames that ended by `now`; the sync point to increment for each, if the driver asked.
    pub fn tick(&mut self, now: u64) -> Option<(usize, u32)> {
        if !self.running() || now < self.next_vblank {
            return None;
        }
        let period = self.frame_time();
        let n = ((now - self.next_vblank) / period + 1) as u32;
        self.next_vblank += n as u64 * period;
        self.frames += n as u64;
        // a disabled source does not set its status
        self.int_status |= (VBLANK | FRAME_END) & self.asm[INT_ENABLE];
        let point = self.active[CONT_SYNCPT_VSYNC];
        self.activate();
        (point & 1 << 8 != 0).then_some((point as usize & 0xff, n))
    }

    pub fn next_event(&self) -> Option<u64> {
        self.running().then_some(self.next_vblank)
    }

    /// The picture as RGBA.
    pub fn render(&self, group: Group, smmu: &Smmu, dma: &dyn Memory, out: &mut Vec<u8>) -> Option<(u32, u32)> {
        if !self.running() {
            return None;
        }
        let (w, h) = self.size()?;
        let bg = self.active[BACKGROUND].to_le_bytes();
        out.clear();
        out.resize((w * h * 4) as usize, 0);
        for p in out.chunks_exact_mut(4) {
            p.copy_from_slice(&[bg[0], bg[1], bg[2], 255]);
        }
        // larger layer depths are further back
        let mut order: Vec<usize> = (0..WINDOWS).filter(|&k| self.active[window(k) + OPTIONS] & 1 << 30 != 0).collect();
        order.sort_by_key(|&k| std::cmp::Reverse(self.active[window(k) + BLEND_LAYER] & 0xff));
        for k in order {
            self.draw_window(k, group, smmu, dma, out, (w, h));
        }
        if self.active[WIN_OPTIONS] & 1 << 16 != 0 {
            self.draw_cursor(group, smmu, dma, out, (w, h));
        }
        Some((w, h))
    }

    fn draw_window(&self, k: usize, group: Group, smmu: &Smmu, dma: &dyn Memory, out: &mut [u8], (w, h): (u32, u32)) {
        let r = |o: usize| self.active[window(k) + o];
        let Some((bytes, decode)) = format(r(DEPTH)) else { return };
        let pair = |v: u32| (v & 0x1fff, v >> 16 & 0x1fff);
        let ((x0, y0), (dw, dh)) = (pair(r(POSITION)), pair(r(WSIZE)));
        let (hinc, vinc) = (r(DDA) & 0xffff, r(DDA) >> 16);
        let src_w = (r(PRESCALED) & 0x7fff) as usize / bytes;
        let src_h = (r(PRESCALED) >> 16 & 0x1fff) as usize;
        if src_w == 0 || src_h == 0 || dw == 0 || dh == 0 {
            return;
        }
        let (stride, start) = ((r(STRIDE) & 0xffff) as u64, ((r(START_HI) as u64) << 32 | r(START) as u64) & ADDRESS);
        // the origin within the surface: bytes and lines (the far corner when mirrored)
        let (ox, oy) = (r(H_OFFSET) as u64, r(V_OFFSET) as u64);
        let (flip_x, flip_y) = (r(OPTIONS) & 1 != 0, r(OPTIONS) & 4 != 0);
        let block = (r(KIND) & 3 == 2).then(|| r(KIND) >> 4 & 7);
        let alpha = has_alpha(r(DEPTH));
        let mut line = vec![0u8; src_w * bytes];
        let mut at = usize::MAX;
        for dy in 0..dh.min(h.saturating_sub(y0)) {
            let sy = ((dy * vinc) >> 12) as usize;
            let sy = sy.min(src_h - 1);
            if sy != at {
                at = sy;
                let y = if flip_y { oy.saturating_sub(sy as u64) } else { oy + sy as u64 };
                let x = if flip_x { (ox + bytes as u64).saturating_sub(line.len() as u64) } else { ox };
                match block {
                    Some(height) => read_block_linear(&mut line, start, stride, height, x, y, |a, b| smmu.read_bytes(group, a, b, dma)),
                    None => smmu.read_bytes(group, start + y * stride + x, &mut line, dma),
                }
                if flip_x {
                    let n = line.len();
                    (0..src_w / 2).for_each(|p| (0..bytes).for_each(|b| line.swap(p * bytes + b, n - (p + 1) * bytes + b)));
                }
            }
            for dx in 0..dw.min(w.saturating_sub(x0)) {
                let sx = (((dx * hinc) >> 12) as usize).min(src_w - 1);
                let c = decode(&line[sx * bytes..(sx + 1) * bytes]);
                let o = (((y0 + dy) * w + x0 + dx) * 4) as usize;
                put(&mut out[o..o + 4], c, alpha);
            }
        }
    }

    fn draw_cursor(&self, group: Group, smmu: &Smmu, dma: &dyn Memory, out: &mut [u8], (w, h): (u32, u32)) {
        let v = self.active[CURSOR_START];
        let n = 32i32 << (v >> 24 & 3);
        let start = ((self.active[CURSOR_START_HI] as u64) << 32 | ((v & 0x3f_ffff) as u64) << 10) & ADDRESS;
        // 14-bit positions; the driver masks them to its 4096 or 8192 limit
        let p = self.active[CURSOR_POSITION];
        let signed = |v: u32, size: u32| { let v = (v & 0x3fff) as i32; if v >= size as i32 && v >= 4096 - n { v - if v >= 8192 - n { 8192 } else { 4096 } } else { v } };
        let (x, y) = (signed(p, w), signed(p >> 16, h));
        let mut image = vec![0u8; (n * n * 4) as usize];
        smmu.read_bytes(group, start, &mut image, dma);
        for cy in 0..n {
            for cx in 0..n {
                let (px, py) = (x + cx, y + cy);
                if (0..w as i32).contains(&px) && (0..h as i32).contains(&py) {
                    let s = &image[((cy * n + cx) * 4) as usize..][..4];
                    let o = (py as usize * w as usize + px as usize) * 4;
                    // RGBA8888: the alpha is in the low byte
                    put(&mut out[o..o + 4], [s[3], s[2], s[1], s[0]], true);
                }
            }
        }
    }
}

fn put(d: &mut [u8], c: [u8; 4], alpha: bool) {
    match c[3] {
        _ if !alpha => d[..3].copy_from_slice(&c[..3]),
        255 => d[..3].copy_from_slice(&c[..3]),
        0 => {}
        a => (0..3).for_each(|k| d[k] = ((c[k] as u32 * a as u32 + d[k] as u32 * (255 - a as u32) + 127) / 255) as u8),
    }
}

fn has_alpha(depth: u32) -> bool {
    matches!(depth, 4 | 5 | 7 | 12 | 13 | 27 | 28 | 29 | 35 | 36)
}

type Decode = fn(&[u8]) -> [u8; 4];

/// Bytes per pixel and the conversion to RGBA of a window colour depth.
fn format(depth: u32) -> Option<(usize, Decode)> {
    fn x5(v: u16) -> u8 { (v << 3 | v >> 2) as u8 }
    fn x6(v: u16) -> u8 { (v << 2 | v >> 4) as u8 }
    fn u(b: &[u8]) -> u16 { u16::from_le_bytes([b[0], b[1]]) }
    fn a(v: u16) -> u8 { if v != 0 { 255 } else { 0 } }
    Some(match depth {
        12 | 37 => (4, |b| [b[2], b[1], b[0], b[3]]),
        13 | 38 => (4, |b| [b[0], b[1], b[2], b[3]]),
        35 | 66 => (4, |b| [b[1], b[2], b[3], b[0]]),
        36 | 65 => (4, |b| [b[3], b[2], b[1], b[0]]),
        4 => (2, |b| { let v = u(b); [(v >> 8 & 15) as u8 * 17, (v >> 4 & 15) as u8 * 17, (v & 15) as u8 * 17, (v >> 12) as u8 * 17] }),
        27 => (2, |b| { let v = u(b); [(v & 15) as u8 * 17, (v >> 4 & 15) as u8 * 17, (v >> 8 & 15) as u8 * 17, (v >> 12) as u8 * 17] }),
        5 | 30 => (2, |b| { let v = u(b); [x5(v >> 10 & 31), x5(v >> 5 & 31), x5(v & 31), a(v >> 15)] }),
        28 | 32 => (2, |b| { let v = u(b); [x5(v & 31), x5(v >> 5 & 31), x5(v >> 10 & 31), a(v >> 15)] }),
        7 | 31 => (2, |b| { let v = u(b); [x5(v >> 11), x5(v >> 6 & 31), x5(v >> 1 & 31), a(v & 1)] }),
        29 | 33 => (2, |b| { let v = u(b); [x5(v >> 1 & 31), x5(v >> 6 & 31), x5(v >> 11), a(v & 1)] }),
        6 => (2, |b| { let v = u(b); [x5(v >> 11), x6(v >> 5 & 63), x5(v & 31), 255] }),
        34 => (2, |b| { let v = u(b); [x5(v & 31), x6(v >> 5 & 63), x5(v >> 11), 255] }),
        _ => return None, // YUV: overlays only
    })
}

/// A byte's place in a 64 × 8 byte block (GOB) of a block-linear surface.
pub fn gob_offset(x: u64, y: u64) -> u64 {
    (x & 32) << 3 | (y & 6) << 5 | (x & 16) << 1 | (y & 1) << 4 | x & 15
}

/// Read a line of a block-linear surface: blocks of 2^height GOBs, `stride` bytes per line.
fn read_block_linear(line: &mut [u8], start: u64, stride: u64, height: u32, x: u64, y: u64, read: impl Fn(u64, &mut [u8])) {
    let block_lines = 8u64 << height;
    let block_bytes = 512u64 << height;
    let row = start + y / block_lines * stride.div_ceil(64) * block_bytes + (y % block_lines / 8) * 512;
    let mut done = 0;
    while done < line.len() {
        let xx = x + done as u64;
        let n = (16 - (xx & 15) as usize).min(line.len() - done);
        read(row + xx / 64 * block_bytes + gob_offset(xx & 63, y & 7), &mut line[done..done + n]);
        done += n;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use peripherals::dma::Dma;

    fn w(d: &mut Dc, r: usize, v: u32) {
        d.write(4 * r as u64, v, 0);
    }

    fn on(d: &mut Dc) {
        for (r, v) in [(0x406, 1 << 16 | 1), (0x407, 5 << 16 | 40), (0x408, 20 << 16 | 220), (ACTIVE, 4 << 16 | 8), (0x40a, 5 << 16 | 110), (INT_MASK, VBLANK), (INT_ENABLE, VBLANK), (CONT_SYNCPT_VSYNC, 1 << 8 | 12), (DISPLAY_COMMAND, 1 << 5)] {
            w(d, r, v);
        }
        w(d, STATE_CONTROL, 1 << 8);
        w(d, STATE_CONTROL, 1);
    }

    #[test]
    fn frames_interrupts_and_activation() {
        let mut d = Dc::default();
        assert!(!d.running());
        on(&mut d);
        assert!(d.running(), "taken at once while stopped");
        assert_eq!(d.next_event(), Some(16_666_667));
        assert_eq!(d.tick(16_666_666), None);
        assert_eq!(d.tick(16_666_667), Some((12, 1)));
        assert!(d.irq());
        w(&mut d, INT_STATUS, VBLANK);
        assert!(!d.irq());
        // stopping waits for the frame's end
        w(&mut d, DISPLAY_COMMAND, 0);
        w(&mut d, STATE_CONTROL, 1 << 8);
        w(&mut d, STATE_CONTROL, 1);
        w(&mut d, STATE_ACCESS, 1);
        assert_eq!((d.read(4 * DISPLAY_COMMAND as u64), d.read(4 * STATE_CONTROL as u64)), (1 << 5, 1));
        d.tick(40_000_000);
        assert_eq!((d.read(4 * DISPLAY_COMMAND as u64), d.read(4 * STATE_CONTROL as u64)), (0, 0));
        assert!(!d.running());
    }

    #[test]
    fn windows_scale_and_blend_and_the_cursor_is_on_top() {
        let mut ram = vec![0u8; 0x10000];
        // window A: 4 x 2 of B8G8R8X8, shown at 8 x 4
        for (k, p) in ram[0x1000..0x1020].chunks_exact_mut(4).enumerate() {
            p.copy_from_slice(&[k as u8, 0x10, 0x20, 0]);
        }
        // the cursor: one half-transparent green pixel
        ram[0x2000..0x2004].copy_from_slice(&[128, 0, 255, 0]);
        let mut d = Dc::default();
        on(&mut d);
        let a = window(0);
        for (o, v) in [(OPTIONS, 1 << 30), (DEPTH, 37), (POSITION, 0), (WSIZE, 4 << 16 | 8), (PRESCALED, 2 << 16 | 16), (DDA, 0x800 << 16 | 0x800), (STRIDE, 16), (START, 0x8000_1000), (BLEND_LAYER, 255)] {
            w(&mut d, a + o, v);
        }
        for (r, v) in [(WIN_OPTIONS, 1 << 16), (CURSOR_START, 0x8000_2000u32 >> 10), (CURSOR_POSITION, 0xfff)] {
            w(&mut d, r, v);
        }
        w(&mut d, CURSOR_POSITION, 1 << 16 | 2);
        w(&mut d, STATE_CONTROL, 3 << 8);
        w(&mut d, STATE_CONTROL, 3);
        let dma = Dma { ram: &mut ram, base: 0x8000_0000 };
        let mut out = Vec::new();
        d.tick(20_000_000);
        assert_eq!(d.render(Group::Dc, &Smmu::default(), &dma, &mut out), Some((8, 4)));
        let px = |x: usize, y: usize| out[(y * 8 + x) * 4..][..3].to_vec();
        assert_eq!((px(0, 0), px(1, 1), px(7, 3)), (vec![0x20, 0x10, 0], vec![0x20, 0x10, 0], vec![0x20, 0x10, 7]));
        assert_eq!(px(2, 1), [16, 136, 0], "the cursor's pixel, blended");
    }

    #[test]
    fn rates_limits_and_reset() {
        let mut d = Dc { mode: Some((378, 34, 12_852)), ..Dc::default() };
        on(&mut d);
        assert_eq!(d.frame_time(), 1_000_000, "the mode's own totals");
        d.mode = Some((1650, 750, 74_250));
        assert_eq!(d.frame_time(), 16_666_667, "another mode");
        w(&mut d, ACTIVE, 0x7fff << 16 | 0x7fff);
        w(&mut d, STATE_CONTROL, 1 << 8);
        w(&mut d, STATE_CONTROL, 1);
        d.tick(20_000_000);
        assert_eq!((d.size(), d.running()), (None, false));
        on(&mut d);
        assert!(d.running());
        d.reset();
        assert!(!d.running() && d.mode.is_some() && d.int_status == 0);
    }

    #[test]
    fn block_linear_lines() {
        // 128 bytes x 16 lines, blocks of two GOBs: address of every byte by the definition
        let (stride, height) = (128u64, 1u32);
        let at = |x: u64, y: u64| (x / 64) * 1024 + (y / 8) * 512 + gob_offset(x & 63, y & 7);
        let mut mem = vec![0u8; 2048];
        for y in 0..16 {
            for x in 0..128 {
                mem[at(x, y) as usize] = (x * 3 + y * 7) as u8;
            }
        }
        for y in 0..16 {
            let mut line = vec![0; 100];
            read_block_linear(&mut line, 0, stride, height, 20, y, |a, b| b.copy_from_slice(&mem[a as usize..a as usize + b.len()]));
            assert!(line.iter().enumerate().all(|(k, &v)| v == ((20 + k as u64) * 3 + y * 7) as u8), "line {y}");
        }
        // every byte of a GOB once
        let mut seen: Vec<u64> = (0..512).map(|k| gob_offset(k % 64, k / 64)).collect();
        seen.sort_unstable();
        assert!(seen.iter().enumerate().all(|(k, &v)| v == k as u64));
    }
}
