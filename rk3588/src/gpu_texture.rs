//! Valhall texture sampling (Mesa genxml v10.xml: Texture, Plane, Sampler).
//!
//! A texture descriptor points at one Plane descriptor per (layer, level), layer-major.
//! Planes are linear or 16x16 u-interleaved (Mesa pan_tiling.c). Supported: 1D/2D/3D,
//! cube maps and arrays, mipmaps with nearest/linear/trilinear filtering, all wrap
//! modes, depth comparison, gather and integer fetch, and the common uncompressed
//! unorm/snorm/float/integer/packed/depth formats with component orders, swizzles and
//! sRGB. AFBC/AFRC planes and block-compressed formats fault.
use crate::gpu_resources::{Error, Resources};
use crate::gpu_shader::Env;
use std::collections::HashMap;
use std::rc::Rc;

fn word(b: &[u8], n: usize) -> u32 {
    u32::from_le_bytes(b[n * 4..n * 4 + 4].try_into().unwrap())
}
fn pair(b: &[u8], n: usize) -> u64 {
    word(b, n) as u64 | ((word(b, n + 1) as u64) << 32)
}

/// How a sample's level of detail is chosen.
#[derive(Clone, Copy, Debug)]
pub enum Lod {
    Explicit(f32),
    /// Screen-space derivatives of the (normalized) coordinates, and a bias.
    Computed {
        dx: [f32; 3],
        dy: [f32; 3],
        bias: f32,
    },
    /// Integer fetch from a level.
    Fetch(u32),
}

/// One texture instruction's operands, already unpacked from staging registers.
#[derive(Clone, Copy, Debug)]
pub struct Request {
    /// (table, index) of the texture and sampler descriptors.
    pub texture: (u32, u32),
    pub sampler: (u32, u32),
    /// Dimension: 0 1D, 1 2D, 2 3D, 3 cube.
    pub dim: u32,
    /// Coordinate register values: floats, or integers for fetches.
    pub coords: [u32; 4],
    pub integer: bool,
    pub layer: Option<u32>,
    pub reference: Option<f32>,
    pub offset: [i32; 3],
    pub lod: Lod,
    pub gather: Option<u32>,
}

#[derive(Clone, Copy, Debug)]
struct Plane {
    pointer: u64,
    size: u64,
    row_stride: u64,
    slice_stride: u64,
    width: u32,
    height: u32,
    tiled: bool,
    /// AFBC planes: header layout (`pointer` is the headers).
    afbc: Option<crate::gpu_afbc::Layout>,
}

#[derive(Clone, Copy, Debug, PartialEq)]
enum Kind {
    Unorm,
    Snorm,
    Float,
    Uint,
    Sint,
}

/// A decoded texture/sampler pair.
struct Texture {
    format: u32,
    order: u32,
    srgb: bool,
    swizzle: [u32; 4],
    dim: u32,
    width: u32,
    height: u32,
    depth: u32,
    levels: u32,
    layers: u32,
    planes: Vec<Plane>,
    // sampler state
    wrap: [u32; 3],
    min_nearest: bool,
    mag_nearest: bool,
    cutoff: f32,
    mipmap: u32,
    min_lod: f32,
    max_lod: f32,
    bias: f32,
    compare: u32,
    border: [u32; 4],
    normalized: bool,
    srgb_override: bool,
}

/// Texture/sampler pairs decoded during one job (descriptors do not change while it runs).
#[derive(Default)]
pub struct Cache {
    map: HashMap<(u64, (u32, u32), (u32, u32)), Rc<Texture>>,
    /// Planes held by the cached textures (one texture may have up to 4096).
    planes: usize,
}

/// Bytes per texel, component count and kind of a Mali format.
fn format_info(f: u32) -> Option<(usize, usize, Kind)> {
    use Kind::*;
    Some(match f {
        163 => (1, 1, Unorm),
        171 => (2, 2, Unorm),
        179 => (3, 3, Unorm),
        187 => (4, 4, Unorm),
        99 => (1, 1, Snorm),
        107 => (2, 2, Snorm),
        115 => (3, 3, Snorm),
        123 => (4, 4, Snorm),
        164 => (2, 1, Unorm),
        172 => (4, 2, Unorm),
        180 => (6, 3, Unorm),
        188 => (8, 4, Unorm),
        100 => (2, 1, Snorm),
        108 => (4, 2, Snorm),
        116 => (6, 3, Snorm),
        124 => (8, 4, Snorm),
        199 => (2, 1, Float),
        207 => (4, 2, Float),
        215 => (6, 3, Float),
        223 => (8, 4, Float),
        167 => (4, 1, Float),
        175 => (8, 2, Float),
        183 => (12, 3, Float),
        191 => (16, 4, Float),
        131 => (1, 1, Uint),
        139 => (2, 2, Uint),
        147 => (3, 3, Uint),
        155 => (4, 4, Uint),
        132 => (2, 1, Uint),
        140 => (4, 2, Uint),
        148 => (6, 3, Uint),
        156 => (8, 4, Uint),
        133 => (4, 1, Uint),
        141 => (8, 2, Uint),
        149 => (12, 3, Uint),
        157 => (16, 4, Uint),
        195 => (1, 1, Sint),
        203 => (2, 2, Sint),
        211 => (3, 3, Sint),
        219 => (4, 4, Sint),
        196 => (2, 1, Sint),
        204 => (4, 2, Sint),
        212 => (6, 3, Sint),
        220 => (8, 4, Sint),
        197 => (4, 1, Sint),
        205 => (8, 2, Sint),
        213 => (12, 3, Sint),
        221 => (16, 4, Sint),
        // packed: RGB565, RGB5A1, A1BGR5, RGBA4, Z16 / RGB10A2, A2BGR10, Z24X8, X8Z24
        64 | 65 | 66 | 186 | 75 => (2, 4, Unorm),
        67 | 68 | 77 | 78 => (4, 4, Unorm),
        71 | 72 => (4, 4, Uint),
        76 => (4, 1, Float),
        93 => (1, 1, Uint),
        _ => return None,
    })
}

/// Decode one texel to four components (float bits, or integers). Packed formats keep
/// component 0 in the least significant bits.
fn decode(f: u32, b: &[u8]) -> [u32; 4] {
    let (bytes, n, kind) = format_info(f).unwrap();
    let mut out = [0u32; 4];
    let unorm = |v: u32, bits: u32| (v as f64 / ((1u64 << bits) - 1) as f64) as f32;
    let packed = |fields: &[(u32, u32)]| -> [u32; 4] {
        let v = b
            .iter()
            .take(bytes)
            .enumerate()
            .fold(0u32, |a, (k, &x)| a | (x as u32) << (8 * k));
        let mut o = [0, 0, 0, 1f32.to_bits()];
        for (k, &(shift, width)) in fields.iter().enumerate() {
            o[k] = unorm((v >> shift) & ((1u64 << width) - 1) as u32, width).to_bits();
        }
        o
    };
    match f {
        64 => return packed(&[(0, 5), (5, 6), (11, 5)]),
        65 => return packed(&[(0, 5), (5, 5), (10, 5), (15, 1)]),
        66 => return packed(&[(0, 1), (1, 5), (6, 5), (11, 5)]),
        186 => return packed(&[(0, 4), (4, 4), (8, 4), (12, 4)]),
        67 => return packed(&[(0, 10), (10, 10), (20, 10), (30, 2)]),
        68 => return packed(&[(0, 2), (2, 10), (12, 10), (22, 10)]),
        75 => return packed(&[(0, 16)]),
        77 => return packed(&[(0, 24)]),
        78 => return packed(&[(8, 24)]),
        71 | 72 => {
            let v = word(b, 0);
            let w: &[(u32, u32)] = if f == 71 {
                &[(0, 10), (10, 10), (20, 10), (30, 2)]
            } else {
                &[(0, 2), (2, 10), (12, 10), (22, 10)]
            };
            for (k, &(s, wd)) in w.iter().enumerate() {
                out[k] = (v >> s) & ((1 << wd) - 1);
            }
            return out;
        }
        _ => {}
    }
    let size = bytes / n;
    for (k, o) in out.iter_mut().enumerate().take(n) {
        let c = &b[k * size..(k + 1) * size];
        let raw = c
            .iter()
            .enumerate()
            .fold(0u32, |a, (j, &x)| a | (x as u32) << (8 * j));
        *o = match (kind, size) {
            (Kind::Unorm, _) => unorm(raw, 8 * size as u32).to_bits(),
            (Kind::Snorm, 1) => ((raw as u8 as i8) as f32 / 127.).max(-1.).to_bits(),
            (Kind::Snorm, _) => ((raw as u16 as i16) as f32 / 32767.).max(-1.).to_bits(),
            (Kind::Float, 2) => crate::gpu_shader::f16_to_f32(raw as u16).to_bits(),
            (Kind::Float, _) => raw,
            (Kind::Uint, _) => raw,
            (Kind::Sint, 1) => raw as u8 as i8 as i32 as u32,
            (Kind::Sint, 2) => raw as u16 as i16 as i32 as u32,
            (Kind::Sint, _) => raw,
        };
    }
    out
}

/// The RGB component order (bits 0-7 of the pixel format): which memory component each
/// of R, G, B, A comes from (4 = zero, 5 = one).
fn order_map(order: u32) -> Option<[u32; 4]> {
    Some(match order {
        0 => [0, 1, 2, 3],
        2 => [1, 0, 2, 3],
        4 => [2, 1, 0, 3],
        8 => [1, 2, 3, 0],
        10 => [2, 1, 3, 0],
        12 => [3, 2, 1, 0],
        16 => [0, 1, 2, 5],
        18 => [1, 0, 2, 5],
        20 => [2, 1, 0, 5],
        24 => [1, 2, 3, 5],
        26 => [2, 1, 3, 5],
        28 => [3, 2, 1, 5],
        226 => [0, 0, 0, 0],
        227 => [0, 0, 0, 5],
        228 => [0, 0, 0, 1],
        229 => [4, 4, 4, 0],
        230 => [4, 4, 4, 5],
        231 => [4, 4, 4, 4],
        240 => [0, 4, 4, 4],
        242 => [0, 2, 1, 3],
        _ => return None,
    })
}

fn srgb_to_linear(c: f32) -> f32 {
    if c <= 0.04045 {
        c / 12.92
    } else {
        ((c + 0.055) / 1.055).powf(2.4)
    }
}

/// Unsigned 8.8 fixed point LOD (Sampler LOD fields).
fn ulod(v: u32) -> f32 {
    v as f32 / 256.
}

impl Texture {
    fn load(
        env: &mut Env,
        resources: Resources,
        t: (u32, u32),
        s: (u32, u32),
    ) -> Result<Texture, Error> {
        // Tables 60-63 are hardware-provided, never guest resource tables.
        let fold = |table: u32| {
            if table >= 12 {
                Err(Error::Unsupported)
            } else {
                Ok(table)
            }
        };
        let td = resources.descriptor(env.m, &env.ram, fold(t.0)?, t.1)?;
        let sd = resources.descriptor(env.m, &env.ram, fold(s.0)?, s.1)?;
        if word(&td, 0) & 15 != 2 || word(&sd, 0) & 15 != 1 {
            return Err(Error::Unsupported);
        }
        let pixel = word(&td, 0) >> 10;
        let format = (pixel >> 12) & 255;
        let order = pixel & 255;
        format_info(format).ok_or(Error::Unsupported)?;
        order_map(order).ok_or(Error::Unsupported)?;
        let swz = word(&td, 2);
        let swizzle: [u32; 4] = std::array::from_fn(|n| (swz >> (3 * n)) & 7);
        if swizzle.iter().any(|&c| c > 5) {
            return Err(Error::Unsupported);
        }
        let dim = (word(&td, 0) >> 4) & 3;
        let levels = ((word(&td, 2) >> 16) & 31) + 1;
        // A cube's Array size counts whole cubes, but its surfaces are one per face: Mesa
        // (v7 and later) emits the six faces as consecutive layers, levels innermost.
        let faces = if dim == 0 { 6 } else { 1 };
        let layers = ((word(&td, 6) & 0xffff) + 1) * faces;
        let surfaces = pair(&td, 4);
        if surfaces & 31 != 0 || levels as u64 * layers as u64 > 4096 {
            return Err(Error::Unsupported);
        }
        let mut planes = Vec::with_capacity((levels * layers) as usize);
        for n in 0..(levels * layers) as u64 {
            let mut p = [0u8; 32];
            let at = surfaces.checked_add(n * 32).ok_or(Error::Unsupported)?;
            env.read(at, &mut p).map_err(Error::Memory)?;
            let kind = (word(&p, 0) >> 4) & 15;
            let ordering = (word(&p, 0) >> 8) & 15;
            let afbc = if kind == 6 {
                // 16x16 superblocks, no split blocks; tiled headers are 8x8 groups.
                if (word(&p, 0) >> 8) & 7 != 0 || word(&p, 0) & (1 << 17) != 0 {
                    return Err(Error::Unsupported);
                }
                let tiled = word(&p, 0) & (1 << 19) != 0;
                // AFBC has no format wider than 64 bits (a solid colour fills the rest
                // of a 16-byte header).
                let bpp = format_info(format).unwrap().0;
                if bpp > crate::gpu_afbc::MAX_BPP {
                    return Err(Error::Unsupported);
                }
                Some(crate::gpu_afbc::Layout {
                    header: pair(&p, 2),
                    stride: word(&p, 4) / (16 * if tiled { 8 } else { 1 }),
                    tiled,
                    bpp,
                    body_offset: 0,
                })
            } else {
                None
            };
            if word(&p, 0) & 15 != 10 || !(kind == 1 && matches!(ordering, 1 | 2) || afbc.is_some()) {
                return Err(Error::Unsupported); // AFRC, ASTC and YUV planes
            }
            planes.push(Plane {
                pointer: pair(&p, 2),
                size: word(&p, 1) as u64,
                row_stride: word(&p, 4) as u64,
                slice_stride: word(&p, 6) as u64,
                width: (word(&p, 7) & 0xffff) + 1,
                height: (word(&p, 7) >> 16) + 1,
                tiled: ordering == 1,
                afbc,
            });
        }
        let c = word(&sd, 0);
        Ok(Texture {
            format,
            order,
            srgb: pixel & (1 << 20) != 0,
            swizzle,
            dim,
            width: (word(&td, 1) & 0xffff) + 1,
            height: (word(&td, 1) >> 16) + 1,
            depth: (word(&td, 7) & 0xffff) + 1,
            levels,
            layers,
            planes,
            wrap: [(c >> 16) & 15, (c >> 12) & 15, (c >> 8) & 15],
            min_nearest: c & (1 << 27) != 0,
            mag_nearest: c & (1 << 28) != 0,
            cutoff: if c & (1 << 29) != 0 { 0.5 } else { 0. },
            mipmap: c >> 30,
            min_lod: ulod(word(&sd, 1) & 0x1fff),
            max_lod: ulod((word(&sd, 1) >> 16) & 0x1fff),
            bias: (word(&sd, 2) as u16 as i16) as f32 / 256.,
            compare: (word(&sd, 1) >> 13) & 7,
            border: [word(&sd, 4), word(&sd, 5), word(&sd, 6), word(&sd, 7)],
            normalized: c & (1 << 25) != 0,
            srgb_override: c & (1 << 22) != 0,
        })
    }

    fn kind(&self) -> Kind {
        format_info(self.format).unwrap().2
    }

    /// Read one texel at integer coordinates of a level and layer; None outside.
    fn texel(
        &self,
        env: &mut Env,
        level: u32,
        layer: u32,
        x: i32,
        y: i32,
        z: i32,
    ) -> Result<Option<[u32; 4]>, Error> {
        let Some(p) = self.planes.get((layer * self.levels + level) as usize) else {
            return Ok(None);
        };
        let d = if self.dim == 3 {
            (self.depth >> level).max(1)
        } else {
            1
        };
        if x < 0 || y < 0 || z < 0 || x as u32 >= p.width || y as u32 >= p.height || z as u32 >= d {
            return Ok(None);
        }
        let bpp = format_info(self.format).unwrap().0 as u64;
        if let Some(l) = p.afbc {
            let mut b = [0u8; 16];
            let mut read = |a: u64, out: &mut [u8]| env.read(a, out);
            return match l.read_pixel(&mut read, x as u32, y as u32, &mut b[..bpp as usize]) {
                Ok(()) => Ok(Some(decode(self.format, &b))),
                Err(crate::gpu_afbc::Error::Memory(f)) => {
                    if std::env::var_os("SBC_MALI_DEBUG").is_some() {
                        eprintln!("[mali] AFBC texture {l:x?} texel {x},{y}: {f:?}");
                    }
                    Err(Error::Memory(f))
                }
                Err(crate::gpu_afbc::Error::Compressed) => Err(Error::Unsupported),
            };
        }
        let (x, y) = (x as u64, y as u64);
        const DUP: [u64; 16] = [
            0, 3, 12, 15, 48, 51, 60, 63, 192, 195, 204, 207, 240, 243, 252, 255,
        ];
        const SPACE: [u64; 16] = [0, 1, 4, 5, 16, 17, 20, 21, 64, 65, 68, 69, 80, 81, 84, 85];
        let offset = if p.tiled {
            (y >> 4) * p.row_stride
                + ((x >> 4) * 256 + (DUP[(y & 15) as usize] ^ SPACE[(x & 15) as usize])) * bpp
        } else {
            y * p.row_stride + x * bpp
        } + z as u64 * p.slice_stride;
        if offset + bpp > p.size.max(p.slice_stride * d as u64) {
            return Ok(None);
        }
        let mut b = [0u8; 16];
        let Some(at) = p.pointer.checked_add(offset) else {
            return Ok(None);
        };
        env.read(at, &mut b[..bpp as usize]).map_err(Error::Memory)?;
        Ok(Some(decode(self.format, &b)))
    }

    /// Apply the component order, sRGB decoding and the descriptor swizzle.
    fn finish(&self, v: [u32; 4]) -> [u32; 4] {
        let integer = matches!(self.kind(), Kind::Uint | Kind::Sint);
        let one = if integer { 1 } else { 1f32.to_bits() };
        let map = order_map(self.order).unwrap();
        let n = format_info(self.format).unwrap().1;
        let packed = matches!(
            self.format,
            64 | 65 | 66 | 186 | 67 | 68 | 71 | 72 | 75 | 77 | 78
        );
        let mut c: [u32; 4] = std::array::from_fn(|k| match map[k] {
            4 => 0,
            5 => one,
            m if m as usize >= n && !packed => {
                if k == 3 {
                    one
                } else {
                    0
                }
            }
            m => v[m as usize],
        });
        if self.srgb && !self.srgb_override && !integer {
            for x in c.iter_mut().take(3) {
                *x = srgb_to_linear(f32::from_bits(*x)).to_bits();
            }
        }
        std::array::from_fn(|k| match self.swizzle[k] {
            4 => 0,
            5 => one,
            m => c[m as usize],
        })
    }
}

fn wrap(mode: u32, i: i32, size: u32) -> Option<i32> {
    let n = size as i32;
    Some(match mode {
        8 => i.rem_euclid(n),
        11 => {
            if i < 0 || i >= n {
                return None;
            }
            i
        }
        12 => {
            let m = i.rem_euclid(2 * n);
            if m < n {
                m
            } else {
                2 * n - 1 - m
            }
        }
        13 => (if i < 0 { -1 - i } else { i }).min(n - 1),
        15 => {
            let m = if i < 0 { -1 - i } else { i };
            if m >= n {
                return None;
            }
            m
        }
        _ => i.clamp(0, n - 1),
    })
}

fn compare(func: u32, reference: f32, value: f32) -> bool {
    match func {
        0 => false,
        1 => reference < value,
        2 => reference == value,
        3 => reference <= value,
        4 => reference > value,
        5 => reference != value,
        6 => reference >= value,
        _ => true,
    }
}

/// Bytes, component count and float-ness of a vertex attribute format.
pub(crate) fn attribute_format(f: u32) -> Option<(usize, usize, bool)> {
    format_info(f).map(|(b, n, k)| (b, n, !matches!(k, Kind::Uint | Kind::Sint)))
}
pub(crate) fn order(o: u32) -> Option<[u32; 4]> {
    order_map(o)
}
pub(crate) fn decode_texel(f: u32, b: &[u8]) -> [u32; 4] {
    decode(f, b)
}

/// Sample a texture. Returns four components as float bits (`true`) or integers.
pub fn sample(env: &mut Env, resources: Resources, r: &Request) -> Result<([u32; 4], bool), Error> {
    let key = (resources.0, r.texture, r.sampler);
    let t = match env.textures.map.get(&key) {
        Some(t) => t.clone(),
        None => {
            let t = Rc::new(Texture::load(env, resources, r.texture, r.sampler)?);
            // Bound the host memory by planes, not entries: 4096 entries of 4096 planes
            // each would be hundreds of MB.
            if env.textures.map.len() >= 4096 || env.textures.planes + t.planes.len() > 1 << 16 {
                env.textures.map.clear();
                env.textures.planes = 0;
            }
            env.textures.planes += t.planes.len();
            env.textures.map.insert(key, t.clone());
            t
        }
    };
    let float = !matches!(t.kind(), Kind::Uint | Kind::Sint);
    let mut layer = r.layer.map_or(0, |l| {
        if l > 0xffff {
            f32::from_bits(l).round().max(0.) as u32
        } else {
            l
        }
    });
    if r.integer {
        // TEX_FETCH: integer texel coordinates, no filtering or wrapping.
        let Lod::Fetch(level) = r.lod else {
            return Err(Error::Unsupported);
        };
        let c = r.coords.map(|v| v as i32);
        let x = c[0].wrapping_add(r.offset[0]);
        let y = if t.dim == 1 { 0 } else { c[1].wrapping_add(r.offset[1]) };
        let z = if t.dim == 3 { c[2].wrapping_add(r.offset[2]) } else { 0 };
        let v = t
            .texel(
                env,
                level.min(t.levels - 1),
                layer.min(t.layers - 1),
                x,
                y,
                z,
            )?
            .unwrap_or([0; 4]);
        return Ok((t.finish(v), float));
    }
    // Descriptor dimension: 0 cube, 1 1D, 2 2D, 3 3D.
    let (one_d, three_d) = (t.dim == 1, t.dim == 3);
    let mut c: [f32; 3] = std::array::from_fn(|n| f32::from_bits(r.coords[n]));
    for v in c.iter_mut() {
        if !v.is_finite() {
            *v = 0.;
        }
    }
    if r.dim == 3 {
        // Cube: X holds the face in bits 29-31 and s in the low bits (Mesa
        // bi_emit_texc_cube_coord); faces are layers.
        let x = r.coords[0];
        layer = layer.saturating_mul(6).saturating_add((x >> 29).min(5));
        c[0] = f32::from_bits((x & 0x1fff_ffff) | 0x2000_0000);
    }
    let (w0, h0, d0) = (t.width as f32, t.height as f32, t.depth as f32);
    let lod = match r.lod {
        Lod::Explicit(l) => l + t.bias,
        Lod::Computed { dx, dy, bias } => {
            let n = if one_d {
                1
            } else if three_d {
                3
            } else {
                2
            };
            let size = [w0, h0, d0];
            let lx: f32 = (0..n)
                .map(|k| (dx[k] * size[k]).powi(2))
                .sum::<f32>()
                .sqrt();
            let ly: f32 = (0..n)
                .map(|k| (dy[k] * size[k]).powi(2))
                .sum::<f32>()
                .sqrt();
            let rho = lx.max(ly);
            (if rho > 0. { rho.log2() } else { -64. }) + bias + t.bias
        }
        Lod::Fetch(_) => return Err(Error::Unsupported),
    };
    let lod = lod.clamp(t.min_lod, t.max_lod.max(t.min_lod));
    let magnify = lod <= t.cutoff;
    let nearest = if magnify {
        t.mag_nearest
    } else {
        t.min_nearest
    };
    let top = (t.levels - 1) as f32;
    let levels: [(u32, f64); 2] = if t.levels == 1 || magnify || t.mipmap == 1 {
        [(0, 1.), (0, 0.)]
    } else if t.mipmap == 3 {
        let l = lod.clamp(0., top);
        let f = l.floor();
        [
            (f as u32, 1. - (l - f) as f64),
            ((f as u32 + 1).min(t.levels - 1), (l - f) as f64),
        ]
    } else {
        [((lod + 0.5).floor().clamp(0., top) as u32, 1.), (0, 0.)]
    };
    let layer = layer.min(t.layers - 1);
    let border = t.border;
    let mut acc = [0f64; 4];
    for &(level, weight) in &levels {
        if weight == 0. {
            continue;
        }
        let p = t.planes[(layer * t.levels + level) as usize];
        let d = if three_d {
            (t.depth >> level).max(1)
        } else {
            1
        };
        // Texel-space coordinates, kept where the integer neighbours (x0 + 1, ...) cannot
        // overflow; anything this far out only selects wrapped or border texels anyway.
        let scale = |c: f32, size: u32| {
            let v = if t.normalized { c * size as f32 } else { c };
            if v.is_finite() {
                v.clamp(-16_777_216., 16_777_216.)
            } else {
                0.
            }
        };
        let u = scale(c[0], p.width) + r.offset[0] as f32;
        let v = if one_d {
            0.5
        } else {
            scale(c[1], p.height) + r.offset[1] as f32
        };
        let s = if three_d {
            scale(c[2], d) + r.offset[2] as f32
        } else {
            0.5
        };
        let fetch = |env: &mut Env, x: i32, y: i32, z: i32| -> Result<[u32; 4], Error> {
            let xi = wrap(t.wrap[0], x, p.width);
            let yi = if one_d {
                Some(0)
            } else {
                wrap(t.wrap[1], y, p.height)
            };
            let zi = if three_d {
                wrap(t.wrap[2], z, d)
            } else {
                Some(0)
            };
            match (xi, yi, zi) {
                (Some(x), Some(y), Some(z)) => {
                    Ok(t.finish(t.texel(env, level, layer, x, y, z)?.unwrap_or([0; 4])))
                }
                _ => Ok(border),
            }
        };
        let shadow = |v: [u32; 4]| -> f64 {
            let value = f32::from_bits(v[0]).clamp(0., 1.);
            compare(t.compare, r.reference.unwrap_or(0.).clamp(0., 1.), value) as u32 as f64
        };
        if let Some(component) = r.gather {
            let (x, y) = ((u - 0.5).floor() as i32, (v - 0.5).floor() as i32);
            let corners = [(x, y + 1), (x + 1, y + 1), (x + 1, y), (x, y)];
            let mut out = [0u32; 4];
            for (k, &(cx, cy)) in corners.iter().enumerate() {
                let texel = fetch(env, cx, cy, 0)?;
                out[k] = if r.reference.is_some() {
                    (shadow(texel) as f32).to_bits()
                } else {
                    texel[component as usize & 3]
                };
            }
            return Ok((out, float));
        }
        if !float {
            // Integer textures are never filtered.
            return Ok((
                fetch(env, u.floor() as i32, v.floor() as i32, s.floor() as i32)?,
                false,
            ));
        }
        let mut add = |texel: [u32; 4], wt: f64| {
            if r.reference.is_some() {
                let pass = shadow(texel);
                for a in acc.iter_mut().take(3) {
                    *a += pass * wt * weight;
                }
                acc[3] += wt * weight;
            } else {
                for n in 0..4 {
                    acc[n] += f32::from_bits(texel[n]) as f64 * wt * weight;
                }
            }
        };
        if nearest {
            add(
                fetch(env, u.floor() as i32, v.floor() as i32, s.floor() as i32)?,
                1.,
            );
        } else {
            let (x, y, z) = (u - 0.5, v - 0.5, s - 0.5);
            let (x0, y0, z0) = (x.floor(), y.floor(), z.floor());
            let (fx, fy, fz) = ((x - x0) as f64, (y - y0) as f64, (z - z0) as f64);
            let (x0, y0, z0) = (x0 as i32, y0 as i32, z0 as i32);
            let zs: &[(i32, f64)] = if three_d {
                &[(0, 1. - fz), (1, fz)]
            } else {
                &[(0, 1.)]
            };
            let ys: &[(i32, f64)] = if one_d {
                &[(0, 1.)]
            } else {
                &[(0, 1. - fy), (1, fy)]
            };
            for &(dz, wz) in zs {
                for &(dy, wy) in ys {
                    for &(dx, wx) in &[(0, 1. - fx), (1, fx)] {
                        let wt = wx * wy * wz;
                        if wt != 0. {
                            let texel = fetch(env, x0 + dx, y0 + dy, z0 + dz)?;
                            add(texel, wt);
                        }
                    }
                }
            }
        }
    }
    Ok((acc.map(|v| (v as f32).to_bits()), true))
}

/// A 2D sample with a compact handle at explicit LOD 0 (tests).
#[cfg(test)]
pub(crate) fn sample2d(
    m: &crate::gpu_mmu::AddressSpace,
    ram: &mut [u8],
    resources: Resources,
    handle: u32,
    uv: [f32; 2],
) -> Result<[f32; 4], Error> {
    let mut code = crate::gpu_shader::Code::default();
    let mut tlb = crate::gpu_shader::Tlb::default();
    let mut textures = Cache::default();
    let mut env = Env::new(m, ram, &[], &mut code, &mut tlb, &mut textures);
    let fold = |t: u32| if t >= 12 { t - 12 + 60 } else { t };
    let r = Request {
        texture: (fold(handle >> 27), (handle >> 16) & 0x3ff),
        sampler: (fold((handle >> 11) & 31), handle & 0x3ff),
        dim: 1,
        coords: [uv[0].to_bits(), uv[1].to_bits(), 0, 0],
        integer: false,
        layer: None,
        reference: None,
        offset: [0; 3],
        lod: Lod::Explicit(0.),
        gather: None,
    };
    sample(&mut env, resources, &r).map(|(v, _)| v.map(f32::from_bits))
}

#[cfg(test)]
pub(crate) fn test_fixture() -> (crate::gpu_mmu::AddressSpace, Resources, Vec<u8>, u32) {
    let m = crate::gpu_mmu::AddressSpace {
        transtab: 0,
        transcfg: 2,
    };
    let mut ram = vec![0; 0x10000];
    fn put(ram: &mut [u8], at: usize, words: &[u32]) {
        for (n, word) in words.iter().enumerate() {
            ram[at + n * 4..at + n * 4 + 4].copy_from_slice(&word.to_le_bytes());
        }
    }
    for table in 0..32 {
        put(&mut ram, 0x1000 + table * 16, &[0x2000, 1 << 24, 32, 0]);
    }
    put(&mut ram, 0x1040, &[0x2040, 1 << 24, 32, 0]);
    put(
        &mut ram,
        0x2000,
        &[
            1 | (8 << 8)
                | (9 << 12)
                | (9 << 16)
                | (1 << 21)
                | (1 << 23)
                | (1 << 25)
                | (1 << 26)
                | (3 << 27)
                | (1 << 30),
            0x1fff0000,
        ],
    );
    put(
        &mut ram,
        0x2040,
        &[(187 << 22) | 0x22, 0x10001, 0x688, 0, 0x2100],
    );
    put(
        &mut ram,
        0x2100,
        &[
            (2 << 24) | (2 << 8) | 0x1a,
            72,
            0x3000,
            0,
            64,
            0,
            128,
            0x10001,
        ],
    );
    ram[0x3000..0x3080].fill(0xa5);
    ram[0x3000..0x3008].copy_from_slice(&[255, 0, 0, 255, 0, 255, 0, 128]);
    ram[0x3040..0x3048].copy_from_slice(&[0, 0, 255, 64, 255, 255, 0, 255]);
    (m, Resources(0x1020), ram, (4 << 27) | (3 << 11))
}

#[cfg(test)]
mod tests {
    use super::*;
    fn set(ram: &mut [u8], at: usize, value: u32) {
        ram[at..at + 4].copy_from_slice(&value.to_le_bytes());
    }
    fn bytes(v: [f32; 4]) -> [u8; 4] {
        v.map(|v| (v * 255.).round() as u8)
    }
    #[test]
    fn afbc_planes_of_formats_wider_than_64_bits_fault() {
        // An AFBC plane (type 6) with RGBA32F and a zeroed (solid-colour) header: the
        // 16-byte texel cannot come from the header; this used to panic the host.
        let (m, resources, mut ram, h) = test_fixture();
        set(&mut ram, 0x2100, (2 << 24) | 0x6a);
        ram[0x3000..0x3080].fill(0);
        set(&mut ram, 0x2040, (191 << 22) | 0x22);
        assert_eq!(sample2d(&m, &mut ram, resources, h, [0.25, 0.25]), Err(Error::Unsupported));
        // The same header as RGBA8 is a solid colour.
        set(&mut ram, 0x2040, (187 << 22) | 0x22);
        assert_eq!(sample2d(&m, &mut ram, resources, h, [0.25, 0.25]), Ok([0.; 4]));
    }
    #[test]
    fn native_resources_sample_padded_rows_and_descriptor_indices() {
        let (m, resources, mut ram, h) = test_fixture();
        for (uv, expected) in [
            ([0.25, 0.25], [255, 0, 0, 255]),
            ([0.75, 0.25], [0, 255, 0, 128]),
            ([0.25, 0.75], [0, 0, 255, 64]),
            ([0.75, 0.75], [255, 255, 0, 255]),
        ] {
            assert_eq!(
                bytes(sample2d(&m, &mut ram, resources, h, uv).unwrap()),
                expected
            );
        }
        assert_eq!(
            sample2d(&m, &mut ram, resources, h | 1, [0.25, 0.25]),
            Err(Error::DescriptorBounds)
        );
        ram.copy_within(0x2000..0x2020, 0x2020);
        ram.copy_within(0x2040..0x2060, 0x2060);
        set(&mut ram, 0x1038, 64);
        set(&mut ram, 0x1048, 64);
        assert_eq!(
            bytes(sample2d(&m, &mut ram, resources, h | (1 << 16) | 1, [0.75, 0.75]).unwrap()),
            [255, 255, 0, 255]
        );
        // The swizzle uses the fetched channels plus explicit zero/one.
        set(&mut ram, 0x2048, 2 | (1 << 3) | (4 << 6) | (5 << 9));
        assert_eq!(
            bytes(sample2d(&m, &mut ram, resources, h, [0.25, 0.75]).unwrap()),
            [255, 0, 0, 255]
        );
    }
    #[test]
    fn cube_maps_sample_the_requested_face() {
        // One cube (Array size 1): six face surfaces, a 2x2 texture each, face f red = 40f.
        let (m, resources, mut ram, _) = test_fixture();
        set(&mut ram, 0x2040, (187 << 22) | 0x02); // texture, dimension cube
        for face in 0..6u32 {
            let plane = 0x2100 + face as usize * 32;
            let texels = 0x4000 + face * 0x80;
            let words = [(2 << 24) | (2 << 8) | 0x1a, 72, texels, 0, 64, 0, 128, 0x10001];
            for (n, &w) in words.iter().enumerate() {
                set(&mut ram, plane + n * 4, w);
            }
            for at in [0, 4, 64, 68] {
                let at = texels as usize + at;
                ram[at..at + 4].copy_from_slice(&[face as u8 * 40, 0, 0, 255]);
            }
        }
        for face in 0..6u32 {
            let mut code = crate::gpu_shader::Code::default();
            let mut tlb = crate::gpu_shader::Tlb::default();
            let mut textures = Cache::default();
            let mut env = Env::new(&m, &mut ram, &[], &mut code, &mut tlb, &mut textures);
            // X: the face in bits 29-31 over s (Mesa bi_emit_texc_cube_coord)
            let s = 0.5f32.to_bits();
            let r = Request {
                texture: (4, 0),
                sampler: (3, 0),
                dim: 3,
                coords: [(face << 29) | (s & 0x1fff_ffff), 0.5f32.to_bits(), 0, 0],
                integer: false,
                layer: None,
                reference: None,
                offset: [0; 3],
                lod: Lod::Explicit(0.),
                gather: None,
            };
            let (v, _) = sample(&mut env, resources, &r).unwrap();
            assert_eq!(bytes(v.map(f32::from_bits)), [face as u8 * 40, 0, 0, 255], "face {face}");
        }
    }
    #[test]
    fn far_out_coordinates_and_addresses_do_not_overflow() {
        let (m, resources, mut ram, h) = test_fixture();
        for uv in [[1e10, 0.5], [-1e30, 3e38], [f32::MAX, f32::MAX]] {
            sample2d(&m, &mut ram, resources, h, uv).unwrap();
        }
        // a texel past the top of the address space (pointer + 68 wraps) reads as
        // outside the texture
        set(&mut ram, 0x2108, 0xffff_ffe0);
        set(&mut ram, 0x210c, 0xffff_ffff);
        assert_eq!(sample2d(&m, &mut ram, resources, h, [0.75, 0.75]), Ok([0.; 4]));
    }
    #[test]
    fn bilinear_centers_and_wrap_boundaries() {
        let (m, resources, mut ram, h) = test_fixture();
        let ctl = word(&ram[0x2000..], 0) & !(3 << 27);
        for mode in [8, 9, 12] {
            set(
                &mut ram,
                0x2000,
                (ctl & !0xff000) | (mode << 12) | (mode << 16),
            );
            let middle = sample2d(&m, &mut ram, resources, h, [0.5, 0.5]).unwrap();
            assert_eq!(middle, [0.5, 0.5, 0.25, 175.5 / 255.]);
            let corner = bytes(sample2d(&m, &mut ram, resources, h, [0., 0.]).unwrap());
            assert_eq!(
                corner,
                if mode == 8 {
                    [128, 128, 64, 176]
                } else {
                    [255, 0, 0, 255]
                }
            );
            let edge = bytes(sample2d(&m, &mut ram, resources, h, [-0.25, 0.25]).unwrap());
            assert_eq!(
                edge,
                if mode == 8 {
                    [0, 255, 0, 128]
                } else {
                    [255, 0, 0, 255]
                }
            );
        }
    }
    #[test]
    fn mipmaps_select_levels_and_trilinear_blends() {
        // A 4x4 level 0 (all red) and a 2x2 level 1 (all blue), bilinear.
        let (m, resources, mut ram, _) = test_fixture();
        let ctl = word(&ram[0x2000..], 0) & !(3 << 27) & !(3 << 30);
        set(&mut ram, 0x2000, ctl); // bilinear, mipmap nearest
        set(&mut ram, 0x2004, 0x0100_0000); // sampler LOD range [0, 1]
        set(&mut ram, 0x2044, 0x30003); // 4x4
        set(&mut ram, 0x2048, 0x688 | (1 << 16)); // two levels
        set(&mut ram, 0x2100, (2 << 24) | (2 << 8) | 0x1a);
        set(&mut ram, 0x2104, 64);
        set(&mut ram, 0x2108, 0x4000);
        set(&mut ram, 0x2110, 16);
        set(&mut ram, 0x211c, 0x30003);
        ram.copy_within(0x2100..0x2120, 0x2120);
        set(&mut ram, 0x2128, 0x5000);
        set(&mut ram, 0x2130, 8);
        set(&mut ram, 0x213c, 0x10001);
        for p in 0..16 {
            ram[0x4000 + p * 4..0x4004 + p * 4].copy_from_slice(&[255, 0, 0, 255]);
        }
        for p in 0..4 {
            ram[0x5000 + p * 4..0x5004 + p * 4].copy_from_slice(&[0, 0, 255, 255]);
        }
        let mut code = crate::gpu_shader::Code::default();
        let mut tlb = crate::gpu_shader::Tlb::default();
        let mut textures = Cache::default();
        let mut env = Env::new(&m, &mut ram, &[], &mut code, &mut tlb, &mut textures);
        let mut r = Request {
            texture: (4, 0),
            sampler: (3, 0),
            dim: 1,
            coords: [0.5f32.to_bits(), 0.5f32.to_bits(), 0, 0],
            integer: false,
            layer: None,
            reference: None,
            offset: [0; 3],
            lod: Lod::Computed {
                dx: [0.25, 0., 0.],
                dy: [0., 0.25, 0.],
                bias: 0.,
            },
            gather: None,
        };
        // One texel per pixel selects level 0 (red), two select level 1 (blue).
        assert_eq!(
            sample(&mut env, resources, &r)
                .unwrap()
                .0
                .map(f32::from_bits),
            [1., 0., 0., 1.]
        );
        r.lod = Lod::Computed {
            dx: [0.5, 0., 0.],
            dy: [0., 0.5, 0.],
            bias: 0.,
        };
        assert_eq!(
            sample(&mut env, resources, &r)
                .unwrap()
                .0
                .map(f32::from_bits),
            [0., 0., 1., 1.]
        );
        // Trilinear blends the two levels.
        let ctl = word(&env.ram[0x2000..], 0) | (3 << 30);
        if let crate::gpu_shader::Ram::Rw(ram) = &mut env.ram {
            set(ram, 0x2000, ctl);
        }
        env.textures.map.clear();
        r.lod = Lod::Explicit(0.5);
        assert_eq!(
            sample(&mut env, resources, &r)
                .unwrap()
                .0
                .map(f32::from_bits),
            [0.5, 0., 0.5, 1.]
        );
    }
    #[test]
    fn u_interleaved_tiles_follow_the_space_filling_curve() {
        let (m, resources, mut ram, h) = test_fixture();
        set(&mut ram, 0x2044, 0x000f_000f); // 16x16
        set(&mut ram, 0x2100, (2 << 24) | (1 << 8) | 0x1a); // tiled
        set(&mut ram, 0x2104, 1024);
        set(&mut ram, 0x2110, 1024);
        set(&mut ram, 0x211c, 0x000f_000f);
        ram[0x3000..0x3400].fill(0);
        // Texel (x=3, y=2) is element dup[2] ^ space[3] = 12 ^ 5 = 9 of its tile.
        ram[0x3000 + 9 * 4..0x3000 + 9 * 4 + 4].copy_from_slice(&[10, 20, 30, 40]);
        let v = sample2d(&m, &mut ram, resources, h, [3.5 / 16., 2.5 / 16.]).unwrap();
        assert_eq!(bytes(v), [10, 20, 30, 40]);
    }
    #[test]
    fn hardware_tables_never_alias_guest_resources() {
        for table in [12, 13, 14, 15] {
            let (m, resources, mut ram, h) = test_fixture();
            set(&mut ram, 0x1000 + table * 16, 0x2040);
            assert!(sample2d(
                &m,
                &mut ram,
                resources,
                (h & !(31 << 27)) | ((table as u32) << 27),
                [0.25, 0.25]
            )
            .is_err());
        }
    }
    #[test]
    fn formats_orders_and_srgb_decode() {
        assert_eq!(decode(64, &[0x1f, 0]).map(f32::from_bits), [1., 0., 0., 1.]);
        assert_eq!(
            decode(223, &[0, 0x3c, 0, 0x38, 0, 0, 0, 0xbc]).map(f32::from_bits),
            [1., 0.5, 0., -1.]
        );
        assert_eq!(order_map(4), Some([2, 1, 0, 3]));
        assert!((srgb_to_linear(0.5) - 0.214).abs() < 0.001);
    }
}
