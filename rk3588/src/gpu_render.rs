//! Valhall fragment jobs: framebuffer descriptors, the tile buffer and writeback
//! (Mesa genxml v10.xml: Framebuffer Parameters, ZS CRC Extension, Render Target).
//!
//! The job's bounding box is rendered at once into a host tile buffer: colour per render
//! target (in the internal format's precision), depth and stencil. Render targets start
//! from their clear colour, or are loaded from memory when the frame has pre-frame
//! (preload) shaders and the target is not cleared. Draws from the tiler are then
//! rasterized (gpu_raster.rs) and the result is written back, linear or 16x16
//! u-interleaved, with the target's swizzle and writeback format. Depth/stencil
//! writeback follows the ZS CRC extension. Every memory destination is validated before
//! any pixel is written. AFBC/AFRC targets, multisampled writeback and raw (non-blendable)
//! internal formats fault.
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
fn unsupported() -> Stop {
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
    pub color: Vec<Vec<[f32; 4]>>,
    pub depth: Vec<f32>,
    pub stencil: Vec<u8>,
    /// Pixels a draw wrote, per render target.
    pub touched: bool,
}
impl TileBuffer {
    pub fn index(&self, x: u32, y: u32) -> usize {
        ((y - self.y0) * self.w + (x - self.x0)) as usize
    }
    /// A copy of `rows` rows starting `first` rows down, for a fragment worker.
    pub fn band(&self, first: u32, rows: u32) -> TileBuffer {
        let (a, b) = ((first * self.w) as usize, ((first + rows) * self.w) as usize);
        TileBuffer {
            x0: self.x0,
            y0: self.y0 + first,
            w: self.w,
            h: rows,
            color: self.color.iter().map(|c| c[a..b].to_vec()).collect(),
            depth: self.depth[a..b].to_vec(),
            stencil: self.stencil[a..b].to_vec(),
            touched: false,
        }
    }
    /// Copy a worker's band back.
    pub fn merge(&mut self, band: &TileBuffer) {
        let a = ((band.y0 - self.y0) * self.w) as usize;
        let n = (band.h * band.w) as usize;
        for (c, b) in self.color.iter_mut().zip(&band.color) {
            c[a..a + n].copy_from_slice(b);
        }
        self.depth[a..a + n].copy_from_slice(&band.depth);
        self.stencil[a..a + n].copy_from_slice(&band.stencil);
        self.touched |= band.touched;
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
        let layout = tib_layout(internal).ok_or_else(unsupported)?;
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
            let bpp = writeback_bytes(format).ok_or_else(unsupported)?;
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
                    stride: word(&rt, 10),
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
                    stride: word(&ext, 10),
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
    // The tile buffer: clear or preload.
    let count = (w * h) as usize;
    let mut tb = TileBuffer {
        x0,
        y0,
        w,
        h,
        color: Vec::with_capacity(fb.targets.len()),
        depth: vec![f32::from_bits(word(&f, 13)); count],
        stencil: vec![word(&f, 12) as u8; count],
        touched: false,
    };
    for t in fb.targets.iter() {
        tb.color.push(vec![t.clear; count]);
    }
    // Pre-frame shaders (Mesa's preload) load targets that are not cleared.
    let dcds = pair(&f, 6);
    for k in 0..2 {
        if (word(&f, 0) >> (3 * k)) & 7 != 0 {
            crate::gpu_raster::frame_shader(m, ram, &fb, &mut tb, dcds + 128 * k as u64)?;
        }
    }
    if let Some(frame) = frame {
        crate::gpu_raster::draw(m, ram, &fb, &mut tb, frame)?;
    }
    if (word(&f, 0) >> 6) & 7 != 0 {
        crate::gpu_raster::frame_shader(m, ram, &fb, &mut tb, dcds + 256)?;
    }
    // Writeback.
    let written = destinations.iter().map(|(n, _)| fb.targets[*n].write.unwrap().bpp).sum::<u64>()
        + zs.map_or(0, |(_, s)| s.bpp);
    crate::gpu::work(0, 0, written * w as u64 * h as u64);
    for (n, rows) in &destinations {
        let t = &fb.targets[*n];
        let s = t.write.unwrap();
        let mut px = [0u8; 4];
        let f = |i: usize, out: &mut [u8]| {
            let bytes = t.pack(tb.color[*n][i], &mut px);
            out.copy_from_slice(&px[..bytes]);
        };
        match s.afbc {
            Some(l) => afbc_store(m, ram, &l, x0, y0, w, h, f)?,
            None => store(ram, &s, rows, x0, y0, w, h, f),
        }
    }
    if let (Some((format, s)), Some(rows)) = (zs, zs_rows.as_ref()) {
        let f = |i: usize, out: &mut [u8]| {
            let z = tb.depth[i].clamp(0., 1.);
            let st = tb.stencil[i] as u32;
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
            Some(l) => afbc_store(m, ram, &l, x0, y0, w, h, f)?,
            None => store(ram, &s, rows, x0, y0, w, h, f),
        }
    }
    // A drawn tile is not a uniform clear: invalidate its checksum.
    let checksum = if frame.is_some_and(|f| !f.draws.is_empty()) || tb.touched {
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
                let (ha, _, ba, body) = l.encode(sx, sy, &vec![0u8; 256 * l.bpp]);
                m.chunks(ram, ha, 16, access, true).map_err(Stop::Memory)?;
                m.chunks(ram, ba, body.len(), access, true).map_err(Stop::Memory)?;
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
            let (ha, header, ba, body) = l.encode(sx, sy, &pixels);
            m.write(ram, ha, &header, true).map_err(Stop::Memory)?;
            m.write(ram, ba, &body, true).map_err(Stop::Memory)?;
        }
    }
    Ok(())
}

/// Physical address of byte `offset` within a validated row's spans.
fn locate(spans: &[(usize, usize)], mut offset: usize) -> Option<usize> {
    for &(p, len) in spans {
        if offset < len {
            return Some(p + offset);
        }
        offset -= len;
    }
    None
}
/// Visit every pixel of the box with its bytes in memory.
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
    if !s.tiled {
        // Linear rows: gather, update and scatter each row through its spans.
        let mut row = vec![0u8; w as usize * bpp];
        for (r, spans) in rows.iter().enumerate() {
            let mut off = 0;
            for &(p, len) in spans {
                row[off..off + len].copy_from_slice(&ram[p..p + len]);
                off += len;
            }
            let base = r * w as usize;
            for (x, px) in row.chunks_exact_mut(bpp).enumerate() {
                f(base + x, px);
            }
            let mut off = 0;
            for &(p, len) in spans {
                ram[p..p + len].copy_from_slice(&row[off..off + len]);
                off += len;
            }
        }
        return;
    }
    for y in y0..y0 + h {
        for x in x0..x0 + w {
            let (row, off) = if s.tiled {
                let ty = (y / 16 - y0 / 16) as usize;
                let first = s.offset(x0 & !15, (y / 16) * 16);
                (ty, (s.offset(x, y) - first) as usize)
            } else {
                ((y - y0) as usize, ((x - x0) as usize) * bpp)
            };
            let i = ((y - y0) * w + (x - x0)) as usize;
            // Pixels never straddle a page within a validated span unless bpp does not
            // divide 4096 (RGB888); gather bytes individually then.
            let spans = &rows[row];
            if let (Some(a), Some(b)) = (locate(spans, off), locate(spans, off + bpp - 1)) {
                if b == a + bpp - 1 {
                    f(i, &mut ram[a..a + bpp]);
                    continue;
                }
            }
            let mut tmp = [0u8; 4];
            let addrs: Vec<usize> = (0..bpp).filter_map(|k| locate(spans, off + k)).collect();
            for (k, &a) in addrs.iter().enumerate() {
                tmp[k] = ram[a];
            }
            f(i, &mut tmp[..bpp]);
            for (k, &a) in addrs.iter().enumerate() {
                ram[a] = tmp[k];
            }
        }
    }
}
#[cfg(test)]
mod tests;
