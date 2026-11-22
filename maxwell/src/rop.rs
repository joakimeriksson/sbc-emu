//! Render targets: which are bound, a cache of 32 × 32 pixel tiles of them, clears, the depth and
//! stencil tests and blending.
use crate::threed::{rt_format, zeta_format, Threed};
use crate::Memory;
use maxwell_tex::{Class, MsMode, RtFormat, Surface, Tile, ZetaFormat};
use std::collections::HashMap;

pub const TILE: u32 = 32;

/// Pixels [x0, x1) × [y0, y1).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Rect {
    pub x0: u32,
    pub y0: u32,
    pub x1: u32,
    pub y1: u32,
}

impl Rect {
    pub fn empty(&self) -> bool {
        self.x0 >= self.x1 || self.y0 >= self.y1
    }

    /// Cut to a scissor's registers (max << 16 | min).
    pub fn and(self, horizontal: u32, vertical: u32) -> Rect {
        Rect { x0: self.x0.max(horizontal & 0xffff), x1: self.x1.min(horizontal >> 16), y0: self.y0.max(vertical & 0xffff), y1: self.y1.min(vertical >> 16) }
    }
}

/// The render targets as bound.
#[derive(Clone, Debug)]
pub struct Targets {
    pub color: [Option<(Surface, RtFormat)>; 8],
    pub zeta: Option<(Surface, ZetaFormat)>,
    pub ms: MsMode,
    /// the screen scissor
    pub clip: Rect,
}

impl Targets {
    pub fn new(t: &Threed) -> Self {
        let ms = MsMode::from_bits(t.r(0x15d0));
        let control = t.r(0x121c);
        let mut color = [None; 8];
        for (k, c) in color.iter_mut().enumerate().take((control & 15).min(8) as usize) {
            let rt = control >> (4 + 3 * k) & 7;
            let base = 0x800 + 0x40 * rt;
            let Some(format) = rt_format(t.r(base + 0x10)) else { continue };
            let (width, height, mode) = (t.r(base + 8), t.r(base + 0xc), t.r(base + 0x14));
            if width == 0 || height == 0 {
                continue;
            }
            let mut s = Surface::new(t.address(base), width, height, mode, t.r(base + 0x1c) as u64 * 4, format.bytes as u32, if mode >> 12 & 1 != 0 { MsMode::X1 } else { ms });
            // the first layer
            let layer = t.r(base + 0x20);
            if !s.is_3d {
                s.address += s.layer_stride * layer as u64;
            }
            *c = Some((s, format));
        }
        let zeta = (t.r(0x1538) & 1 != 0).then(|| zeta_format(t.r(0xfe8))).flatten().map(|format| {
            let mut s = Surface::new(t.address(0xfe0), t.r(0x1228), t.r(0x122c), t.r(0xfec) & 0xfff, t.r(0xff0) as u64 * 4, format.bytes as u32, ms);
            s.address += s.layer_stride * t.r(0x179c) as u64;
            (s, format)
        });
        let (h, v) = (t.r(0xff4), t.r(0xff8));
        let clip = Rect { x0: h & 0xffff, x1: (h & 0xffff) + (h >> 16), y0: v & 0xffff, y1: (v & 0xffff) + (v >> 16) };
        Self { color, zeta, ms, clip }
    }

    /// The pixels every bound target has, inside the screen scissor.
    pub fn rect(&self) -> Rect {
        let mut r = self.clip;
        for s in self.color.iter().flatten().map(|c| &c.0).chain(self.zeta.iter().map(|z| &z.0)) {
            let (w, h) = s.pixels();
            r.x1 = r.x1.min(w);
            r.y1 = r.y1.min(h);
        }
        r
    }
}

pub fn clear_color(s: &Surface, format: &RtFormat, c: [u32; 4], mask: [bool; 4], rect: Rect, layer: u32, mem: &mut dyn Memory) {
    let value = format.encode(&c);
    let n = format.bytes as usize;
    let keep = format.write_mask(mask);
    let whole = keep[..n].iter().all(|&b| b == 0xff);
    each_tile(s, rect, |x, y, w, h| {
        let mut t = Tile::new(s, x, y, w, h);
        if whole {
            t.fill(&value[..n]);
        } else {
            t.load(s, mem, layer);
            for p in t.data.chunks_exact_mut(n) {
                for k in 0..n {
                    p[k] = p[k] & !keep[k] | value[k] & keep[k];
                }
            }
        }
        t.store(s, mem, layer);
    });
}

#[allow(clippy::too_many_arguments)]
pub fn clear_zeta(s: &Surface, format: &ZetaFormat, depth: Option<f32>, stencil: Option<u8>, mask: u8, rect: Rect, layer: u32, mem: &mut dyn Memory) {
    let n = format.bytes as usize;
    let whole = depth.is_some() == format.has_depth() && stencil.is_some() == format.has_stencil() && mask == 0xff;
    each_tile(s, rect, |x, y, w, h| {
        let mut t = Tile::new(s, x, y, w, h);
        if whole {
            let mut b = [0; 8];
            format.store(format.encode(depth.unwrap_or(0.0), stencil.unwrap_or(0)), &mut b);
            t.fill(&b[..n]);
        } else {
            t.load(s, mem, layer);
            for p in t.data.chunks_exact_mut(n) {
                let mut v = format.load(p);
                if let Some(z) = depth {
                    v = format.with_depth(v, format.quantize(z));
                }
                if let Some(st) = stencil {
                    v = format.with_stencil(v, st, mask);
                }
                format.store(v, p);
            }
        }
        t.store(s, mem, layer);
    });
}

fn each_tile(_s: &Surface, r: Rect, mut f: impl FnMut(u32, u32, u32, u32)) {
    const N: u32 = 128;
    let mut y = r.y0;
    while y < r.y1 {
        let h = (N - y % N).min(r.y1 - y);
        let mut x = r.x0;
        while x < r.x1 {
            let w = (N - x % N).min(r.x1 - x);
            f(x, y, w, h);
            x += w;
        }
        y += h;
    }
}

/// A tile of every target.
pub struct TileSet {
    pub color: [Tile; 8],
    pub zeta: Tile,
    loaded: u32,
    dirty: u32,
}

/// The tiles that are in host memory.
#[derive(Default)]
pub struct Rop {
    tiles: HashMap<(u32, u32, u32), Box<TileSet>>,
    targets: Option<Targets>,
    spare: Vec<Box<TileSet>>,
}

const ZETA: u32 = 1 << 8;

impl Rop {
    /// Write every changed tile back.
    pub fn flush(&mut self, mem: &mut dyn Memory) {
        if self.tiles.is_empty() {
            self.targets = None;
            return;
        }
        let Some(t) = self.targets.take() else { return };
        for ((_, _, layer), set) in self.tiles.drain() {
            for (k, c) in t.color.iter().enumerate() {
                if let (Some((s, _)), true) = (c, set.dirty >> k & 1 != 0) {
                    set.color[k].store(s, mem, layer);
                }
            }
            if let (Some((s, _)), true) = (&t.zeta, set.dirty & ZETA != 0) {
                set.zeta.store(s, mem, layer);
            }
            self.spare.push(set);
        }
        self.spare.truncate(256);
    }

    pub fn begin(&mut self, targets: &Targets) {
        if self.targets.is_none() {
            self.targets = Some(targets.clone());
        }
    }

    /// The tile at tile coordinates (x, y) of `layer`, with the targets in `need` loaded and those
    /// in `write` marked as changed.
    pub fn tile(&mut self, x: u32, y: u32, layer: u32, need: u32, write: u32, mem: &dyn Memory) -> &mut TileSet {
        let t = self.targets.as_ref().unwrap();
        let spare = &mut self.spare;
        let set = self.tiles.entry((x, y, layer)).or_insert_with(|| {
            let mut s = spare.pop().unwrap_or_else(|| Box::new(TileSet { color: Default::default(), zeta: Tile::default(), loaded: 0, dirty: 0 }));
            (s.loaded, s.dirty) = (0, 0);
            s
        });
        let missing = need & !set.loaded;
        if missing != 0 {
            for (k, c) in t.color.iter().enumerate() {
                if let (Some((s, _)), true) = (c, missing >> k & 1 != 0) {
                    set.color[k].reset(s, x * TILE, y * TILE, TILE, TILE);
                    set.color[k].load(s, mem, layer);
                }
            }
            if let (Some((s, _)), true) = (&t.zeta, missing & ZETA != 0) {
                set.zeta.reset(s, x * TILE, y * TILE, TILE, TILE);
                set.zeta.load(s, mem, layer);
            }
            set.loaded |= missing;
        }
        set.dirty |= write;
        set
    }
}

pub const NEED_ZETA: u32 = ZETA;

/// A GL comparison function (0x200 never ... 0x207 always; D3D's 1 to 8 too).
#[inline]
pub fn compare<T: PartialOrd>(func: u32, a: T, b: T) -> bool {
    match if func >= 0x200 { func - 0x200 } else { func.wrapping_sub(1) } {
        0 => false,
        1 => a < b,
        2 => a == b,
        3 => a <= b,
        4 => a > b,
        5 => a != b,
        6 => a >= b,
        _ => true,
    }
}

/// One face's stencil state.
#[derive(Clone, Copy, Debug, Default)]
pub struct Stencil {
    pub fail: u32,
    pub zfail: u32,
    pub zpass: u32,
    pub func: u32,
    pub reference: u8,
    pub func_mask: u8,
    pub write_mask: u8,
}

impl Stencil {
    pub fn test(&self, stored: u8) -> bool {
        compare(self.func, self.reference & self.func_mask, stored & self.func_mask)
    }

    pub fn apply(&self, op: u32, s: u8) -> u8 {
        let v = match op {
            0 | 2 => 0,
            0x1e00 | 1 => s,
            0x1e01 | 3 => self.reference,
            0x1e02 | 4 => s.saturating_add(1),
            0x1e03 | 5 => s.saturating_sub(1),
            0x150a | 6 => !s,
            0x8507 | 7 => s.wrapping_add(1),
            0x8508 | 8 => s.wrapping_sub(1),
            _ => s,
        };
        s & !self.write_mask | v & self.write_mask
    }
}

/// Depth and stencil state.
#[derive(Clone, Copy, Debug, Default)]
pub struct DepthStencil {
    pub depth_test: bool,
    pub depth_write: bool,
    pub depth_func: u32,
    pub bounds: Option<(f32, f32)>,
    /// front and back
    pub stencil: Option<[Stencil; 2]>,
}

impl DepthStencil {
    pub fn new(t: &Threed) -> Self {
        let face = |op: u32, func: u32, reference: u32, func_mask: u32, mask: u32| Stencil {
            fail: t.r(op),
            zfail: t.r(op + 4),
            zpass: t.r(op + 8),
            func: t.r(func),
            reference: t.r(reference) as u8,
            func_mask: t.r(func_mask) as u8,
            write_mask: t.r(mask) as u8,
        };
        let front = face(0x1384, 0x1390, 0x1394, 0x1398, 0x139c);
        let back = if t.r(0x1594) & 1 != 0 { face(0x1598, 0x15a4, 0xf54, 0xf5c, 0xf58) } else { front };
        Self {
            depth_test: t.r(0x12cc) & 1 != 0,
            depth_write: t.r(0x12e8) & 1 != 0,
            depth_func: t.r(0x130c),
            bounds: (t.r(0x19bc) & 1 != 0).then(|| (t.f(0xf9c), t.f(0xfa0))),
            stencil: (t.r(0x1380) & 1 != 0).then_some([front, back]),
        }
    }

    pub fn used(&self) -> bool {
        self.depth_test || self.stencil.is_some() || self.bounds.is_some()
    }

    /// Test a sample against the stored one, and update that. `back`: the back face's stencil.
    #[inline]
    pub fn test(&self, f: &ZetaFormat, p: &mut [u8], z: f32, back: bool) -> bool {
        let old = f.load(p);
        let mut new = old;
        if let Some((lo, hi)) = self.bounds {
            let d = f.to_f32(f.depth(old));
            if d < lo || d > hi {
                return false;
            }
        }
        let depth = f.quantize(z);
        let pass = !self.depth_test || !f.has_depth() || {
            match f.compare(depth, f.depth(old)) {
                Some(o) => compare(self.depth_func, o as i8, 0),
                None => matches!(self.depth_func, 0x207 | 8),
            }
        };
        let mut visible = pass;
        if let (Some(s), true) = (&self.stencil, f.has_stencil()) {
            let s = &s[back as usize];
            let stored = f.stencil(old);
            let ok = s.test(stored);
            let op = if !ok { s.fail } else if !pass { s.zfail } else { s.zpass };
            visible &= ok;
            let v = s.apply(op, stored);
            if v != stored {
                new = f.with_stencil(new, v, 0xff);
            }
        }
        if visible && self.depth_test && self.depth_write && f.has_depth() {
            new = f.with_depth(new, depth);
        }
        if new != old {
            f.store(new, p);
        }
        visible
    }
}

/// One target's blending.
#[derive(Clone, Copy, Debug, Default)]
pub struct Blend {
    pub on: bool,
    /// equation, source and destination factors: colour, then alpha
    pub rgb: [u32; 3],
    pub alpha: [u32; 3],
    pub mask: u32,
}

impl Blend {
    pub fn new(t: &Threed, rt: usize) -> Self {
        let per_target = t.r(0x12e4) & 1 != 0;
        let on = t.r(0x1360 + 4 * if per_target { rt as u32 } else { rt as u32 }) & 1 != 0;
        let (rgb, alpha) = if per_target {
            let b = 0x1e00 + 0x20 * rt as u32;
            let rgb = [t.r(b + 4), t.r(b + 8), t.r(b + 0xc)];
            (rgb, if t.r(b) & 1 != 0 { [t.r(b + 0x10), t.r(b + 0x14), t.r(b + 0x18)] } else { rgb })
        } else {
            let rgb = [t.r(0x1340), t.r(0x1344), t.r(0x1348)];
            (rgb, if t.r(0x133c) & 1 != 0 { [t.r(0x134c), t.r(0x1350), t.r(0x1358)] } else { rgb })
        };
        Self { on, rgb, alpha, mask: t.color_mask(rt) }
    }

    pub fn blend(&self, src: [f32; 4], src1: [f32; 4], dst: [f32; 4], constant: [f32; 4]) -> [f32; 4] {
        let factor = |f: u32, k: usize| -> f32 {
            // GL's values, with bit 14 set or not, or D3D's
            let f = f & 0xbfff;
            match f {
                0 => 0.0,
                1 => if f == 1 { 1.0 } else { 0.0 },
                0x300 => src[k],
                0x301 => 1.0 - src[k],
                0x302 => src[3],
                0x303 => 1.0 - src[3],
                0x304 => dst[3],
                0x305 => 1.0 - dst[3],
                0x306 => dst[k],
                0x307 => 1.0 - dst[k],
                0x308 => if k == 3 { 1.0 } else { src[3].min(1.0 - dst[3]) },
                0x8001 => constant[k],
                0x8002 => 1.0 - constant[k],
                0x8003 => constant[3],
                0x8004 => 1.0 - constant[3],
                0x900 | 0x8900 => src1[k],
                0x901 | 0x8901 => 1.0 - src1[k],
                0x902 | 0x8902 => src1[3],
                0x903 | 0x8903 => 1.0 - src1[3],
                _ => 1.0,
            }
        };
        std::array::from_fn(|k| {
            let [eq, sf, df] = if k == 3 { self.alpha } else { self.rgb };
            let (s, d) = (src[k] * factor(sf, k), dst[k] * factor(df, k));
            match eq & 0xff {
                0x06 | 1 => s + d,
                0x07 | 4 => src[k].min(dst[k]),
                0x08 | 5 => src[k].max(dst[k]),
                0x0a | 2 => s - d,
                0x0b | 3 => d - s,
                _ => s + d,
            }
        })
    }
}

/// A GL logic operation on the stored bits.
pub fn logic(op: u32, s: u8, d: u8) -> u8 {
    match op & 15 {
        0 => 0,
        1 => s & d,
        2 => s & !d,
        3 => s,
        4 => !s & d,
        5 => d,
        6 => s ^ d,
        7 => s | d,
        8 => !(s | d),
        9 => !(s ^ d),
        10 => !d,
        11 => s | !d,
        12 => !s,
        13 => !s | d,
        14 => !(s & d),
        _ => 0xff,
    }
}

/// Everything that happens to a fragment's colour on its way into a target.
#[derive(Clone, Debug)]
pub struct Output {
    pub format: RtFormat,
    pub blend: Blend,
    pub srgb_write: bool,
}

impl Output {
    #[inline]
    pub fn write(&self, p: &mut [u8], color: [u32; 4], src1: [f32; 4], constant: [f32; 4], logic_op: Option<u32>) {
        let n = self.format.bytes as usize;
        let mut value = color;
        if self.format.class == Class::Float {
            let mut c = color.map(f32::from_bits);
            if self.format.is_normalized() {
                c = c.map(|v| if v.is_nan() { 0.0 } else { v });
            }
            if self.blend.on && logic_op.is_none() {
                let dst = self.format.decode_f32(p);
                c = self.blend.blend(c, src1, dst, constant);
            }
            value = c.map(f32::to_bits);
        }
        let mut bytes = self.format.encode(&value);
        if let Some(op) = logic_op {
            (0..n).for_each(|k| bytes[k] = logic(op, bytes[k], p[k]));
        }
        if self.blend.mask == 0x1111 {
            p[..n].copy_from_slice(&bytes[..n]);
        } else {
            let keep = self.format.write_mask(std::array::from_fn(|k| self.blend.mask >> (4 * k) & 1 != 0));
            for k in 0..n {
                p[k] = p[k] & !keep[k] | bytes[k] & keep[k];
            }
        }
    }
}
