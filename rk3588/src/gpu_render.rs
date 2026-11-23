//! Valhall fragment jobs: framebuffer descriptors, the tile buffer and writeback
//! (Mesa genxml v10.xml: Framebuffer Parameters, ZS CRC Extension, Render Target).
//!
//! The job's bounding box is rendered into a host tile buffer: colour per render target
//! (in the internal format's precision), depth and stencil. Render targets start from
//! their clear colour, or are loaded from memory when the frame has pre-frame (preload)
//! shaders and the target is not cleared. Draws from the tiler are then rasterized
//! (gpu_raster.rs) and the result is written back, linear, 16x16 u-interleaved or AFBC,
//! with the target's swizzle and writeback format. Depth/stencil writeback follows the
//! ZS CRC extension. Every memory destination is validated before any pixel is written.
//! AFRC targets, multisampled writeback and raw (non-blendable) internal formats fault.
//!
//! The tile buffer holds at most `PASS_BYTES`: a larger box is rendered in passes of
//! whole 16-row tile rows, each written back before the next (a fault in a later pass
//! leaves the earlier ones written, as a GPU's finished tiles are). A job that only
//! clears writes its clear values without a tile buffer.
use crate::{
    gpu_cs::Stop,
    gpu_mmu::{Access, AddressSpace, Fault},
};
fn word(b: &[u8], n: usize) -> u32 {
    u32::from_le_bytes(b[n * 4..n * 4 + 4].try_into().unwrap())
}
fn pair(b: &[u8], n: usize) -> u64 {
    word(b, n) as u64 | (word(b, n + 1) as u64) << 32
}
/// An unsupported fragment job setting (where, with `SBC_MALI_DEBUG=1`).
#[track_caller]
fn unsupported() -> Stop {
    if std::env::var_os("SBC_MALI_DEBUG").is_some() {
        eprintln!("[mali] unsupported fragment job setting at {}", std::panic::Location::caller());
    }
    Stop::Unsupported { pc: 0, opcode: 7 }
}
/// A fragment job over a host bound: a job error the guest sees as a queue fault.
#[track_caller]
fn limit(what: &str) -> Stop {
    if std::env::var_os("SBC_MALI_DEBUG").is_some() {
        eprintln!("[mali] fragment job refused at {}: {what}", std::panic::Location::caller());
    }
    Stop::Unsupported { pc: 0, opcode: 7 }
}
fn trace() -> bool {
    std::env::var_os("SBC_MALI_TRACE").is_some()
}

/// Tile-buffer channel layout of a colour internal format (Mesa pan_clear.c tib_layouts):
/// (integer bits, fraction bits) for R, G, B, A.
fn tib_layout(internal: u32) -> Option<[(u32, u32); 4]> {
    Some(match internal {
        1 => [(8, 0), (8, 0), (8, 0), (8, 0)],
        2 => [(10, 0), (10, 0), (10, 0), (2, 0)],
        3 => [(8, 2), (8, 2), (8, 2), (2, 0)],
        4 => [(4, 4), (4, 4), (4, 4), (4, 4)],
        5 => [(5, 5), (6, 4), (5, 5), (0, 2)],
        6 => [(5, 5), (5, 5), (5, 5), (1, 1)],
        _ => return None,
    })
}

/// A memory surface: linear or 16x16 u-interleaved.
#[derive(Clone, Copy, Debug)]
pub struct Surface {
    pub base: u64,
    pub row_stride: u64,
    pub tiled: bool,
    pub bpp: u64,
    /// AFBC surfaces (16x16 superblocks, sparse payloads).
    pub afbc: Option<crate::gpu_afbc::Layout>,
}
impl Surface {
    pub fn offset(&self, x: u32, y: u32) -> u64 {
        const DUP: [u64; 16] = [
            0, 3, 12, 15, 48, 51, 60, 63, 192, 195, 204, 207, 240, 243, 252, 255,
        ];
        const SPACE: [u64; 16] = [0, 1, 4, 5, 16, 17, 20, 21, 64, 65, 68, 69, 80, 81, 84, 85];
        let (x, y) = (x as u64, y as u64);
        if self.tiled {
            (y >> 4) * self.row_stride
                + ((x >> 4) * 256 + (DUP[(y & 15) as usize] ^ SPACE[(x & 15) as usize])) * self.bpp
        } else {
            y * self.row_stride + x * self.bpp
        }
    }
}

/// One render target: its tile-buffer precision, clear colour and writeback.
pub struct Target {
    /// Channel bits (integer + fraction) for quantizing blended values.
    pub layout: [(u32, u32); 4],
    pub clear: [f32; 4],
    pub write: Option<Surface>,
    /// Writeback format (Color Format enum) and swizzle: memory component k takes
    /// tile channel swizzle[k] (4 = zero, 5 = one).
    pub format: u32,
    pub swizzle: [u32; 4],
    pub srgb: bool,
}

/// The host tile buffer for a fragment job's bounding box.
pub struct TileBuffer {
    pub x0: u32,
    pub y0: u32,
    pub w: u32,
    pub h: u32,
    /// Per render target; empty for a target that is not written back (no shader reads
    /// the tile buffer, so it could not be seen).
    pub color: Vec<Vec<[f32; 4]>>,
    pub depth: Vec<f32>,
    pub stencil: Vec<u8>,
    /// Pixels a draw wrote, per render target.
    pub touched: bool,
    /// A fragment worker's share: the rows y with (y / STRIPE) % parts == part, stored
    /// compacted from row `base` on (parts 1: every row).
    part: u32,
    parts: u32,
    base: u32,
}
/// Rows per stripe of a fragment worker (a multiple of the 2x2 quad).
const STRIPE: u32 = 16;
/// Host bytes of a tile buffer (per pixel 16 per written render target, depth and stencil).
const PASS_BYTES: usize = 64 << 20;
/// Primitive and draw visits of a job rendered in passes (each pass visits all of them).
const PASS_VISITS: usize = 1 << 26;
thread_local! {
    /// Tests: force the bytes and visits of a pass.
    pub(crate) static PASS: std::cell::Cell<Option<(usize, usize)>> = const { std::cell::Cell::new(None) };
    /// The last tile buffer's storage, reused (allocating and faulting in tens of MB for
    /// every frame showed up in the frame time).
    static SPARE: std::cell::RefCell<(Vec<Vec<[f32; 4]>>, Vec<f32>, Vec<u8>)> = Default::default();
}
impl TileBuffer {
    /// A tile buffer for the box holding the clear values (`None`: a target that is not
    /// written back), from reused storage.
    fn cleared(x0: u32, y0: u32, w: u32, h: u32, clears: &[Option<[f32; 4]>], depth: f32, stencil: u8) -> Self {
        let mut t = TileBuffer::new(x0, y0, w, h);
        let count = (w * h) as usize;
        (t.color, t.depth, t.stencil) = SPARE.take();
        t.color.resize_with(clears.len(), Vec::new);
        for (c, v) in t.color.iter_mut().zip(clears) {
            c.clear();
            if let Some(v) = v {
                c.resize(count, *v);
            }
        }
        t.depth.clear();
        t.depth.resize(count, depth);
        t.stencil.clear();
        t.stencil.resize(count, stencil);
        t
    }
    /// Keep this buffer's storage for the next one.
    fn recycle(self) {
        SPARE.set((self.color, self.depth, self.stencil));
    }
    pub fn new(x0: u32, y0: u32, w: u32, h: u32) -> Self {
        TileBuffer {
            x0,
            y0,
            w,
            h,
            color: Vec::new(),
            depth: Vec::new(),
            stencil: Vec::new(),
            touched: false,
            part: 0,
            parts: 1,
            base: 0,
        }
    }
    fn row(&self, y: u32) -> u32 {
        if self.parts <= 1 {
            y - self.y0
        } else {
            (y / STRIPE / self.parts) * STRIPE + y % STRIPE - self.base
        }
    }
    pub fn index(&self, x: u32, y: u32) -> usize {
        (self.row(y) * self.w + (x - self.x0)) as usize
    }
    /// Whether this (worker's) tile buffer holds row `y`.
    pub fn owns(&self, y: u32) -> bool {
        self.parts <= 1 || (y / STRIPE) % self.parts == self.part
    }
    /// Worker `part` of `parts`: a copy of its interleaved stripes of rows, so that
    /// workers share geometry that covers part of the screen evenly.
    pub fn stripes(&self, part: u32, parts: u32) -> TileBuffer {
        let mut t = TileBuffer::new(self.x0, self.y0, self.w, self.h);
        (t.part, t.parts) = (part, parts);
        let owned: Vec<u32> = (self.y0..self.y0 + self.h).filter(|&y| t.owns(y)).collect();
        if let Some(&first) = owned.first() {
            t.base = (first / STRIPE / parts) * STRIPE + first % STRIPE;
        }
        let w = self.w as usize;
        for (k, c) in self.color.iter().enumerate() {
            t.color.push(Vec::with_capacity(if c.is_empty() { 0 } else { owned.len() * w }));
            for &y in owned.iter().filter(|_| !c.is_empty()) {
                let a = self.index(self.x0, y);
                t.color[k].extend_from_slice(&c[a..a + w]);
            }
        }
        for &y in &owned {
            let a = self.index(self.x0, y);
            t.depth.extend_from_slice(&self.depth[a..a + w]);
            t.stencil.extend_from_slice(&self.stencil[a..a + w]);
        }
        t
    }
    /// Copy a worker's stripes back.
    pub fn merge(&mut self, part: &TileBuffer) {
        let w = self.w as usize;
        for y in (self.y0..self.y0 + self.h).filter(|&y| part.owns(y)) {
            let (a, b) = (self.index(self.x0, y), part.index(self.x0, y));
            for (c, p) in self.color.iter_mut().zip(&part.color).filter(|(c, _)| !c.is_empty()) {
                c[a..a + w].copy_from_slice(&p[b..b + w]);
            }
            self.depth[a..a + w].copy_from_slice(&part.depth[b..b + w]);
            self.stencil[a..a + w].copy_from_slice(&part.stencil[b..b + w]);
        }
        self.touched |= part.touched;
    }
}

/// Decoded framebuffer descriptor.
pub struct Fb {
    pub width: u32,
    pub height: u32,
    pub targets: Vec<Target>,
    pub frame_arg: u64,
    pub z_bits: u32,
    pub z_float: bool,
    pub sample_positions: u64,
}

/// Bytes per pixel of a writeback (Color Format) format.
fn writeback_bytes(f: u32) -> Option<u64> {
    Some(match f {
        16 => 1,
        17 | 20 | 21 | 28 | 29 => 2,
        18 => 3,
        19 | 22 | 24 | 25 => 4,
        _ => return None,
    })
}

fn quantize(v: f32, (int, frac): (u32, u32)) -> f32 {
    if int == 0 {
        return 1.;
    }
    let max = ((1u32 << int) - 1) as f32;
    let _ = frac;
    (v.clamp(0., 1.) * max).round_ties_even() / max
}
fn linear_to_srgb(c: f32) -> f32 {
    if c <= 0.0031308 {
        c * 12.92
    } else {
        1.055 * c.powf(1. / 2.4) - 0.055
    }
}
fn srgb_to_linear(c: f32) -> f32 {
    if c <= 0.04045 {
        c / 12.92
    } else {
        ((c + 0.055) / 1.055).powf(2.4)
    }
}
impl Target {
    /// Bits of the first channel of the format Mesa packs the blend constant for
    /// (pack_blend_constant), by writeback format.
    pub fn constant_bits(&self) -> u32 {
        match self.format {
            20 => 4,           // R4G4B4A4
            21 | 28 | 29 => 5, // R5G6B5, R5G5B5A1, A1B5G5R5
            24 | 25 => 10,     // R10G10B10A2, A2B10G10R10
            _ => 8,
        }
    }
    pub fn quantize(&self, c: [f32; 4]) -> [f32; 4] {
        std::array::from_fn(|n| quantize(c[n], self.layout[n]))
    }
    /// Pack one pixel in the writeback format.
    fn pack(&self, c: [f32; 4], out: &mut [u8; 4]) -> usize {
        if matches!(self.format, 19 | 22) && !self.srgb {
            // RGBA8 (the common case): one byte per swizzled channel.
            for k in 0..4 {
                out[k] = match self.swizzle[k] {
                    4 => 0,
                    5 => 255,
                    s => (c[s as usize].clamp(0., 1.) * 255.).round_ties_even() as u8,
                };
            }
            return 4;
        }
        let mut c = c;
        if self.srgb {
            for v in c.iter_mut().take(3) {
                *v = linear_to_srgb(*v);
            }
        }
        let comp: [f32; 4] = std::array::from_fn(|k| match self.swizzle[k] {
            4 => 0.,
            5 => 1.,
            s => c[s as usize],
        });
        let unorm = |v: f32, bits: u32| {
            ((v.clamp(0., 1.) * ((1u32 << bits) - 1) as f32).round_ties_even()) as u32
        };
        let fields: &[u32] = match self.format {
            16 => &[8],
            17 => &[8, 8],
            18 => &[8, 8, 8],
            19 | 22 => &[8, 8, 8, 8],
            20 => &[4, 4, 4, 4],
            21 => &[5, 6, 5],
            24 => &[10, 10, 10, 2],
            25 => &[2, 10, 10, 10],
            28 => &[5, 5, 5, 1],
            29 => &[1, 5, 5, 5],
            _ => &[],
        };
        let bytes = writeback_bytes(self.format).unwrap_or(0) as usize;
        let mut v = 0u32;
        let mut shift = 0;
        for (k, &bits) in fields.iter().enumerate() {
            v |= unorm(comp[k], bits) << shift;
            shift += bits;
        }
        out[..4].copy_from_slice(&v.to_le_bytes());
        bytes
    }
}

pub fn fragment(m: &AddressSpace, ram: &mut [u8], regs: &[u32; 256]) -> Result<(), Stop> {
    fragment_frame(m, ram, regs, None)
}

pub fn fragment_frame(
    m: &AddressSpace,
    ram: &mut [u8],
    regs: &[u32; 256],
    frame: Option<&crate::gpu_tiler::Frame>,
) -> Result<(), Stop> {
    let fbd = (regs[40] as u64 | ((regs[41] as u64) << 32)) & !63;
    let mut f = [0; 128];
    m.read(ram, fbd, &mut f, true).map_err(Stop::Memory)?;
    if trace() {
        eprintln!(
            "[mali] fragment regs40..48={:x?} fbd={:x?}",
            &regs[40..48],
            (0..32).map(|n| word(&f, n)).collect::<Vec<_>>()
        );
    }
    let width = (word(&f, 8) & 65535) + 1;
    let height = (word(&f, 8) >> 16) + 1;
    let rt_count = ((word(&f, 11) >> 19) & 15) as usize + 1;
    // Post-frame shaders, downscaling, CRC-only multisample patterns and preload/unload
    // fields of the legacy depth path are not modelled.
    if width > 8192
        || height > 8192
        || word(&f, 0) & !0xff00_01ff != 0
        || (pair(&f, 14) != 0) != frame.is_some()
        || rt_count > 8
        || frame.is_some_and(|f| !f.draws.is_empty()) && (word(&f, 11) >> 6) & 7 != 2
        || word(&f, 12) & ((1 << 19) | (1 << 20)) != 0
    {
        return Err(unsupported());
    }
    if let Some(frame) = frame {
        if frame.width != 0 && (frame.width != width || frame.height != height) {
            return Err(unsupported());
        }
    }
    let has_ext = word(&f, 12) & (1 << 21) != 0;
    let mut ext = [0u8; 64];
    if has_ext {
        m.read(ram, fbd + 128, &mut ext, true)
            .map_err(Stop::Memory)?;
    } else if word(&f, 12) & (3 << 30) != 0 {
        return Err(unsupported());
    }
    // Render targets.
    let mut targets = Vec::with_capacity(rt_count);
    let rt_base = fbd + if has_ext { 192 } else { 128 };
    let mut writes = Vec::new();
    for n in 0..rt_count {
        let mut rt = [0u8; 64];
        m.read(ram, rt_base + n as u64 * 64, &mut rt, true)
            .map_err(Stop::Memory)?;
        if trace() {
            eprintln!(
                "[mali] render target {n}={:x?}",
                (0..16).map(|k| word(&rt, k)).collect::<Vec<_>>()
            );
        }
        let w0 = word(&rt, 0);
        let w1 = word(&rt, 1);
        let internal = w0 >> 26;
        let layout = tib_layout(internal).ok_or_else(|| unsupported())?;
        if w0 & 15 != 0 || w0 & (1 << 24) != 0 {
            return Err(unsupported()); // AFRC/YUV writeback
        }
        let swizzle: [u32; 4] = std::array::from_fn(|k| (w1 >> (16 + 3 * k)) & 7);
        if swizzle.iter().any(|&c| c > 5) || (w1 >> 12) & 3 > 1 {
            return Err(unsupported());
        }
        // Clear colour in the tile-buffer layout; dithered clears scale by the full field.
        let packed = word(&rt, 12);
        let dithered = w0 & (1 << 25) != 0;
        let mut clear = [0f32; 4];
        let mut shift = 0;
        for (k, &(int, frac)) in layout.iter().enumerate() {
            let bits = int + frac;
            let v = (packed >> shift) & ((1u64 << bits) - 1) as u32;
            clear[k] = if int == 0 {
                1.
            } else if dithered {
                v as f32 / (((1u32 << int) - 1) << frac) as f32
            } else {
                (v >> frac) as f32 / ((1u32 << int) - 1) as f32
            };
            shift += bits;
        }
        // Mesa packs an sRGB target's clear colour already encoded (pan_pack_color); the
        // tile buffer holds linear values, encoded again at writeback.
        if w1 & (1 << 14) != 0 {
            for c in clear.iter_mut().take(3) {
                *c = srgb_to_linear(*c);
            }
        }
        let write_enable = w1 & 1 != 0;
        let block = (w1 >> 8) & 15;
        let format = (w1 >> 3) & 31;
        let write = if write_enable {
            let bpp = writeback_bytes(format).ok_or_else(|| unsupported())?;
            // AFBC: no split or wide (32x8) blocks.
            if !matches!(block, 1 | 2 | 12 | 13) || block >= 12 && word(&rt, 2) & 6 != 0 {
                return Err(unsupported());
            }
            let s = Surface {
                base: pair(&rt, 8),
                row_stride: word(&rt, 10) as u64,
                tiled: block == 1,
                bpp,
                afbc: (block >= 12).then(|| crate::gpu_afbc::Layout {
                    header: pair(&rt, 8),
                    // v9+: header bytes per superblock row (8 rows when tiled).
                    stride: word(&rt, 10) / (16 * if block == 13 { 8 } else { 1 }),
                    tiled: block == 13,
                    bpp: bpp as usize,
                    body_offset: word(&rt, 11) as u64,
                }),
            };
            if !s.tiled && s.afbc.is_none() && s.row_stride < width as u64 * bpp {
                return Err(Stop::InvalidStream);
            }
            writes.push(n);
            Some(s)
        } else {
            None
        };
        targets.push(Target {
            layout,
            clear,
            write,
            format,
            swizzle,
            srgb: w1 & (1 << 14) != 0,
        });
    }
    let min = word(&f, 9);
    let max = word(&f, 10);
    let x0 = (min & 65535).max(regs[42] & 65535);
    let y0 = (min >> 16).max(regs[42] >> 16);
    let x1 = (max & 65535).min(regs[43] & 65535);
    let y1 = (max >> 16).min(regs[43] >> 16);
    if x0 > x1 || y0 > y1 || x1 >= width || y1 >= height {
        return Err(Stop::InvalidStream);
    }
    let (w, h) = (x1 - x0 + 1, y1 - y0 + 1);
    // Depth/stencil surface (ZS CRC extension).
    let z_write = word(&f, 12) & (1 << 18) != 0;
    let s_write = word(&f, 12) & (1 << 8) != 0;
    let zs_format = word(&ext, 0) & 15;
    let zs = if has_ext && (z_write || s_write) && zs_format != 0 {
        let block = (word(&ext, 0) >> 4) & 15;
        let bpp = match zs_format {
            1 => 2,
            2 | 4 | 5 | 14 => 4,
            _ => return Err(unsupported()),
        };
        if !matches!(block, 1 | 2 | 12 | 13) || (word(&ext, 0) >> 8) & 3 > 1 {
            return Err(unsupported());
        }
        Some((
            zs_format,
            Surface {
                base: pair(&ext, 8),
                row_stride: word(&ext, 10) as u64,
                tiled: block == 1,
                bpp,
                afbc: (block >= 12).then(|| crate::gpu_afbc::Layout {
                    header: pair(&ext, 8),
                    stride: word(&ext, 10) / (16 * if block == 13 { 8 } else { 1 }),
                    tiled: block == 13,
                    bpp: bpp as usize,
                    body_offset: word(&ext, 11) as u64,
                }),
            },
        ))
    } else {
        None
    };
    let fb = Fb {
        width,
        height,
        targets,
        frame_arg: pair(&f, 2),
        z_bits: match (word(&f, 12) >> 16) & 3 {
            0 => 16,
            1 => 24,
            _ => 32,
        },
        z_float: (word(&f, 12) >> 16) & 3 == 2,
        sample_positions: pair(&f, 4),
    };
    // Validate all destinations before any memory changes.
    let mut destinations: Vec<(usize, Vec<Vec<(usize, usize)>>)> = Vec::new();
    for &n in &writes {
        let s = fb.targets[n].write.unwrap();
        destinations.push((n, rows(m, ram, &s, x0, y0, w, h, Access::Write)?));
    }
    let zs_rows = match zs {
        Some((_, s)) => Some(rows(m, ram, &s, x0, y0, w, h, Access::Write)?),
        None => None,
    };
    let mut crc = Vec::new();
    if word(&f, 12) & (3 << 30) != 0 {
        // CRC tiles (16x16): validated here, written after the pixels.
        let base = pair(&ext, 4);
        let stride = word(&ext, 1) as u64;
        let columns = width.div_ceil(16) as usize;
        if stride < (columns * 8) as u64 {
            return Err(Stop::InvalidStream);
        }
        for ty in y0 / 16..=y1 / 16 {
            let addr = base
                .checked_add(ty as u64 * stride)
                .ok_or(Stop::Memory(Fault::AddressSize))?;
            if word(&f, 12) & (1 << 30) != 0 {
                m.chunks(ram, addr, columns * 8, Access::Read, true)
                    .map_err(Stop::Memory)?;
            }
            if word(&f, 12) & (1 << 31) != 0 {
                crc.push((
                    ty,
                    m.chunks(ram, addr, columns * 8, Access::Write, true)
                        .map_err(Stop::Memory)?,
                ));
            }
        }
    }
    // The tile buffer: clear or preload, then draws, a pass of rows at a time.
    let draws = frame.map_or(0, |f| f.draws.len());
    let shaders = word(&f, 0) & 0x1ff != 0;
    let depth = f32::from_bits(word(&f, 13));
    let stencil = word(&f, 12) as u8;
    let clears: Vec<Option<[f32; 4]>> = fb.targets.iter().map(|t| t.write.map(|_| t.clear)).collect();
    let written = destinations.iter().map(|(n, _)| fb.targets[*n].write.unwrap().bpp).sum::<u64>()
        + zs.map_or(0, |(_, s)| s.bpp);
    crate::gpu::work(0, 0, written * w as u64 * h as u64);
    // Write rows py0..py0 + ph back, from the tile buffer or (only clears) the clear values.
    let writeback = |ram: &mut [u8], py0: u32, ph: u32, tb: Option<&TileBuffer>| -> Result<(), Stop> {
        // This pass's rows (linear) or tile rows (u-interleaved) of the validated spans.
        let part = |rows: &Vec<Vec<(usize, usize)>>, tiled: bool| -> std::ops::Range<usize> {
            if rows.is_empty() {
                0..0
            } else if tiled {
                (py0 / 16 - y0 / 16) as usize..((py0 + ph - 1) / 16 - y0 / 16) as usize + 1
            } else {
                (py0 - y0) as usize..(py0 - y0 + ph) as usize
            }
        };
        for (n, rows) in &destinations {
            let t = &fb.targets[*n];
            let s = t.write.unwrap();
            let mut px = [0u8; 4];
            let bytes = t.pack(t.clear, &mut px);
            let clear = px;
            let f = |i: usize, out: &mut [u8]| match tb {
                Some(tb) => {
                    let bytes = t.pack(tb.color[*n][i], &mut px);
                    out.copy_from_slice(&px[..bytes]);
                }
                None => out.copy_from_slice(&clear[..bytes]),
            };
            match s.afbc {
                Some(l) => afbc_store(m, ram, &l, x0, py0, w, ph, f)?,
                None => store(ram, &s, &rows[part(rows, s.tiled)], x0, py0, w, ph, f),
            }
        }
        if let (Some((format, s)), Some(rows)) = (zs, zs_rows.as_ref()) {
            let f = |i: usize, out: &mut [u8]| {
                let (z, st) = tb.map_or((depth, stencil), |tb| (tb.depth[i], tb.stencil[i]));
                let z = z.clamp(0., 1.);
                let st = st as u32;
                match format {
                    1 => out.copy_from_slice(&((z * 65535.).round() as u16).to_le_bytes()),
                    14 => out.copy_from_slice(&z.to_bits().to_le_bytes()),
                    _ => {
                        let d = (z as f64 * 16777215.).round() as u32;
                        let mut old = u32::from_le_bytes(out[..4].try_into().unwrap());
                        if z_write {
                            old = (old & 0xff00_0000) | d;
                        }
                        if s_write && format == 5 {
                            old = (old & 0x00ff_ffff) | st << 24;
                        }
                        out.copy_from_slice(&old.to_le_bytes());
                    }
                }
            };
            match s.afbc {
                Some(l) => afbc_store(m, ram, &l, x0, py0, w, ph, f)?,
                None => store(ram, &s, &rows[part(rows, s.tiled)], x0, py0, w, ph, f),
            }
        }
        Ok(())
    };
    let mut touched = false;
    if draws == 0 && !shaders {
        // Only clears: every pixel holds its clear values.
        writeback(ram, y0, h, None)?;
    } else {
        let row_bytes = w as usize * (16 * destinations.len() + 5);
        let (pass_bytes, pass_visits) = PASS.with(|p| p.get()).unwrap_or((PASS_BYTES, PASS_VISITS));
        let fit = pass_bytes / row_bytes;
        let pass_rows = if fit >= h as usize { h } else { (fit as u32 / 16 * 16).max(16) };
        let passes = if pass_rows >= h { 1 } else { (y1 / pass_rows - y0 / pass_rows + 1) as usize };
        let primitives: usize = frame.map_or(0, |f| f.draws.iter().map(|d| d.primitives.len()).sum());
        if passes > 1 && (primitives + draws).saturating_mul(passes) > pass_visits {
            return Err(limit("primitives times passes of rows"));
        }
        let mut budget = crate::gpu_raster::BUDGET.with(|b| b.get()).unwrap_or(crate::gpu_raster::FRAME_BUDGET);
        let mut counts = vec![0u64; draws];
        let dcds = pair(&f, 6);
        let mut py0 = y0;
        while py0 <= y1 {
            let py1 = if passes == 1 { y1 } else { ((py0 / pass_rows + 1) * pass_rows - 1).min(y1) };
            let ph = py1 - py0 + 1;
            let mut tb = TileBuffer::cleared(x0, py0, w, ph, &clears, depth, stencil);
            // Pre-frame shaders (Mesa's preload) load targets that are not cleared.
            for k in 0..2 {
                if (word(&f, 0) >> (3 * k)) & 7 != 0 {
                    crate::gpu_raster::frame_shader(m, ram, &fb, &mut tb, dcds + 128 * k as u64, &mut budget)?;
                }
            }
            if let Some(frame) = frame {
                crate::gpu_raster::draw(m, ram, &fb, &mut tb, frame, &mut budget, &mut counts)?;
            }
            if (word(&f, 0) >> 6) & 7 != 0 {
                crate::gpu_raster::frame_shader(m, ram, &fb, &mut tb, dcds + 256, &mut budget)?;
            }
            writeback(ram, py0, ph, Some(&tb))?;
            touched |= tb.touched;
            tb.recycle();
            py0 = py1 + 1;
        }
        if let Some(frame) = frame {
            crate::gpu_raster::occlusion(m, ram, frame, &counts)?;
        }
    }
    // A drawn tile is not a uniform clear: invalidate its checksum.
    let checksum = if draws != 0 || touched {
        0u64.to_le_bytes()
    } else {
        pair(&ext, 2).to_le_bytes()
    };
    for (_, spans) in crc {
        let mut offset = 0;
        for (p, len) in spans {
            for byte in &mut ram[p..p + len] {
                *byte = checksum[offset % 8];
                offset += 1;
            }
        }
    }
    Ok(())
}

/// Translated spans of each row of a surface's box (one Vec per row, or per tile row for
/// u-interleaved surfaces, which cover 16 rows contiguously).
#[allow(clippy::too_many_arguments)]
fn rows(
    m: &AddressSpace,
    ram: &[u8],
    s: &Surface,
    x0: u32,
    y0: u32,
    w: u32,
    h: u32,
    access: Access,
) -> Result<Vec<Vec<(usize, usize)>>, Stop> {
    let mut out = Vec::new();
    if let Some(l) = s.afbc {
        // Headers and payload slots of every superblock the box touches.
        for sy in y0 / 16..=(y0 + h - 1) / 16 {
            for sx in x0 / 16..=(x0 + w - 1) / 16 {
                let Some((ha, _, ba, body)) = l.encode(sx, sy, &vec![0u8; 256 * l.bpp]) else {
                    return Err(Stop::Memory(Fault::AddressSize));
                };
                let check = m.chunks(ram, ha, 16, access, true).and_then(|_| m.chunks(ram, ba, body.len(), access, true));
                if let Err(f) = check {
                    if std::env::var_os("SBC_MALI_DEBUG").is_some() {
                        eprintln!("[mali] AFBC {l:x?} superblock {sx},{sy} header {ha:#x} body {ba:#x}: {f:?}");
                    }
                    return Err(Stop::Memory(f));
                }
            }
        }
        return Ok(out);
    }
    if s.tiled {
        for ty in y0 / 16..=(y0 + h - 1) / 16 {
            let first = s.offset(x0 & !15, ty * 16);
            let last = s.offset(((x0 + w - 1) | 15) - 15, ty * 16) + 256 * s.bpp;
            let addr = s
                .base
                .checked_add(first)
                .ok_or(Stop::Memory(Fault::AddressSize))?;
            out.push(
                m.chunks(ram, addr, (last - first) as usize, access, true)
                    .map_err(Stop::Memory)?,
            );
        }
    } else {
        for y in y0..y0 + h {
            let addr = s
                .base
                .checked_add(s.offset(x0, y))
                .ok_or(Stop::Memory(Fault::AddressSize))?;
            out.push(
                m.chunks(ram, addr, (w as u64 * s.bpp) as usize, access, true)
                    .map_err(Stop::Memory)?,
            );
        }
    }
    Ok(out)
}
/// Write the box into an AFBC surface: each superblock it touches is read (outside the
/// box), updated and written back uncompressed.
#[allow(clippy::too_many_arguments)]
fn afbc_store(
    m: &AddressSpace,
    ram: &mut [u8],
    l: &crate::gpu_afbc::Layout,
    x0: u32,
    y0: u32,
    w: u32,
    h: u32,
    mut f: impl FnMut(usize, &mut [u8]),
) -> Result<(), Stop> {
    let bpp = l.bpp;
    for sy in y0 / 16..=(y0 + h - 1) / 16 {
        for sx in x0 / 16..=(x0 + w - 1) / 16 {
            let full = sx * 16 >= x0 && sx * 16 + 15 < x0 + w && sy * 16 >= y0 && sy * 16 + 15 < y0 + h;
            let mut pixels = if full {
                vec![0u8; 256 * bpp]
            } else {
                let mut read = |a: u64, out: &mut [u8]| m.read(ram, a, out, true);
                l.read_superblock(&mut read, sx, sy).unwrap_or_else(|_| vec![0u8; 256 * bpp])
            };
            for py in 0..16 {
                for px in 0..16 {
                    let (x, y) = (sx * 16 + px, sy * 16 + py);
                    if x < x0 || x >= x0 + w || y < y0 || y >= y0 + h {
                        continue;
                    }
                    let at = ((py * 16 + px) as usize) * bpp;
                    f(((y - y0) * w + (x - x0)) as usize, &mut pixels[at..at + bpp]);
                }
            }
            let (ha, header, ba, body) = l.encode(sx, sy, &pixels).ok_or(Stop::Memory(Fault::AddressSize))?;
            m.write(ram, ha, &header, true).map_err(Stop::Memory)?;
            m.write(ram, ba, &body, true).map_err(Stop::Memory)?;
        }
    }
    Ok(())
}

/// Visit every pixel of the box with its bytes in memory: `rows` are the box's validated
/// spans, one per row (linear) or per tile row (u-interleaved, 16 rows contiguously). Each
/// is gathered, updated and scattered back.
#[allow(clippy::too_many_arguments)]
fn store(
    ram: &mut [u8],
    s: &Surface,
    rows: &[Vec<(usize, usize)>],
    x0: u32,
    y0: u32,
    w: u32,
    h: u32,
    mut f: impl FnMut(usize, &mut [u8]),
) {
    let bpp = s.bpp as usize;
    let mut buf = Vec::new();
    for (r, spans) in rows.iter().enumerate() {
        buf.clear();
        for &(p, len) in spans {
            buf.extend_from_slice(&ram[p..p + len]);
        }
        if s.tiled {
            let ty = y0 / 16 + r as u32;
            let first = s.offset(x0 & !15, ty * 16);
            for y in (ty * 16).max(y0)..(ty * 16 + 16).min(y0 + h) {
                let base = ((y - y0) * w) as usize;
                for x in x0..x0 + w {
                    let at = (s.offset(x, y) - first) as usize;
                    f(base + (x - x0) as usize, &mut buf[at..at + bpp]);
                }
            }
        } else {
            let base = r * w as usize;
            for (x, px) in buf.chunks_exact_mut(bpp).enumerate() {
                f(base + x, px);
            }
        }
        let mut off = 0;
        for &(p, len) in spans {
            ram[p..p + len].copy_from_slice(&buf[off..off + len]);
            off += len;
        }
    }
}
#[cfg(test)]
mod tests;
