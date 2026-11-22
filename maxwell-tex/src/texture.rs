//! Textures and samplers precomputed from their descriptors, and the cache texel reads go
//! through.

use crate::bc::{self, Texels};
use crate::desc::{BadHeader, Compare, CubeEdges, Dim, Layout, MipFilter, Reduction, Source, Tic, Tsc, Wrap};
use crate::layout::{BlockDims, Chain, Mip, MsMode, Plane, Shape, GOB_BYTES};
use crate::num::{bits, rgb9e5_to_f32, srgb8_table, Chan, Num, Packed};
use crate::{bptc, etc, Memory};

/// Why a header gives no texture.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TexError {
    /// Reserved header version or texture type.
    BadHeader,
    /// Unknown format code, or one that is not implemented (ASTC).
    Format(u8),
}

impl From<BadHeader> for TexError {
    fn from(_: BadHeader) -> Self {
        Self::BadHeader
    }
}

/// Compressed block encodings.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum BlockCode {
    Bc1,
    Bc2,
    Bc3,
    Bc4,
    Bc5,
    Bc6S,
    Bc6U,
    Bc7,
    Etc2Rgb,
    Etc2Rgb8A1,
    Etc2Rgba,
    EacR,
    EacRg,
}

impl BlockCode {
    pub fn bytes(self) -> u32 {
        match self {
            Self::Bc1 | Self::Bc4 | Self::Etc2Rgb | Self::Etc2Rgb8A1 | Self::EacR => 8,
            _ => 16,
        }
    }
}

/// How texels are stored.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    /// Channels in 32-bit words.
    Packed,
    /// Four 8-bit UNORM channels (perhaps sRGB).
    Rgba8,
    Rgb9e5,
    /// Two texels in 32 bits. Byte of each texel's luma, of red and of blue.
    Yuv { y: [u8; 2], r: u8, b: u8 },
    /// One bit per texel, low bit first.
    R1,
    /// 4x4 blocks.
    Block(BlockCode),
}

/// A texture format: the TIC's component sizes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TexFormat {
    pub kind: Kind,
    /// Bytes of a block (a texel of plain formats).
    pub bytes: u32,
    pub block_w: u32,
    pub block_h: u32,
    /// Word, shift and size of red, green, blue and alpha.
    chans: [(u8, u8, u8); 4],
    /// The component that is depth (0 for colour formats).
    pub depth: u8,
}

impl TexFormat {
    #[rustfmt::skip]
    pub fn from_code(code: u8) -> Option<Self> {
        use BlockCode::*;
        type C = (u8, u8, u8);
        const N: C = (0, 0, 0);
        let packed = |bytes: u32, chans: [C; 4]| Self { kind: Kind::Packed, bytes, block_w: 1, block_h: 1, chans, depth: 0 };
        let block = |b: BlockCode| Self { kind: Kind::Block(b), bytes: b.bytes(), block_w: 4, block_h: 4, chans: [N; 4], depth: 0 };
        let z24 = || packed(4, [(0, 0, 24), (0, 24, 8), N, N]);
        let z24_s8 = || packed(8, [(0, 0, 24), (1, 0, 8), N, N]);
        let zf32_s8 = || packed(8, [(0, 0, 32), (1, 0, 8), (1, 8, 24), N]);
        Some(match code {
            0x01 => packed(16, [(0, 0, 32), (1, 0, 32), (2, 0, 32), (3, 0, 32)]),
            0x02 => packed(12, [(0, 0, 32), (1, 0, 32), (2, 0, 32), N]),
            0x03 => packed(8, [(0, 0, 16), (0, 16, 16), (1, 0, 16), (1, 16, 16)]),
            0x04 => packed(8, [(0, 0, 32), (1, 0, 32), N, N]),
            0x05 => zf32_s8(),                                                      // R32_B24G8
            0x06 => block(Etc2Rgb),
            0x07 => packed(4, [(0, 0, 8), (0, 8, 8), (0, 16, 8), N]),
            0x08 => packed(4, [(0, 0, 8), (0, 8, 8), (0, 16, 8), (0, 24, 8)]),
            0x09 => packed(4, [(0, 0, 10), (0, 10, 10), (0, 20, 10), (0, 30, 2)]),
            0x0a => block(Etc2Rgb8A1),
            0x0b => block(Etc2Rgba),
            0x0c => packed(4, [(0, 0, 16), (0, 16, 16), N, N]),
            0x0d => z24(),                                                          // G8R24
            0x0e => packed(4, [(0, 0, 8), (0, 8, 24), N, N]),                       // G24R8
            0x0f => packed(4, [(0, 0, 32), N, N, N]),
            0x10 => block(Bc6S),
            0x11 => block(Bc6U),
            0x12 => packed(2, [(0, 0, 4), (0, 4, 4), (0, 8, 4), (0, 12, 4)]),
            0x13 => packed(2, [(0, 0, 1), (0, 1, 5), (0, 6, 5), (0, 11, 5)]),       // A5B5G5R1
            0x14 => packed(2, [(0, 0, 5), (0, 5, 5), (0, 10, 5), (0, 15, 1)]),
            0x15 => packed(2, [(0, 0, 5), (0, 5, 6), (0, 11, 5), N]),
            0x16 => packed(2, [(0, 0, 5), (0, 5, 5), (0, 10, 6), N]),
            0x17 => block(Bc7),
            0x18 => packed(2, [(0, 0, 8), (0, 8, 8), N, N]),
            0x19 => block(EacR),
            0x1a => block(EacRg),
            0x1b => packed(2, [(0, 0, 16), N, N, N]),
            0x1c | 0x1d => packed(1, [(0, 0, 8), N, N, N]),
            0x1e => packed(1, [(0, 0, 4), (0, 4, 4), N, N]),
            0x1f => Self { kind: Kind::R1, bytes: 1, block_w: 8, block_h: 1, chans: [N; 4], depth: 0 },
            0x20 => Self { kind: Kind::Rgb9e5, ..packed(4, [N; 4]) },
            0x21 => packed(4, [(0, 0, 11), (0, 11, 11), (0, 22, 10), N]),
            0x22 => Self { kind: Kind::Yuv { y: [1, 3], r: 0, b: 2 }, block_w: 2, ..packed(4, [N; 4]) },
            0x23 => Self { kind: Kind::Yuv { y: [0, 2], r: 1, b: 3 }, block_w: 2, ..packed(4, [N; 4]) },
            0x24 => block(Bc1),
            0x25 => block(Bc2),
            0x26 => block(Bc3),
            0x27 => block(Bc4),
            0x28 => block(Bc5),
            0x29 => Self { depth: 1, ..packed(4, [(0, 0, 8), (0, 8, 24), N, N]) },  // Z24S8
            0x2a..=0x2e | 0x3b => z24(),
            0x2f => packed(4, [(0, 0, 32), N, N, N]),
            0x30 | 0x33..=0x36 | 0x38 | 0x39 | 0x3d | 0x3e => zf32_s8(),
            0x31 | 0x32 | 0x37 | 0x3c => z24_s8(),
            0x3a => packed(2, [(0, 0, 16), N, N, N]),
            _ => return None,
        })
    }

    /// The texel with each channel's number type.
    pub fn packed(&self, types: [Option<Num>; 4], srgb: bool) -> Packed {
        Packed {
            bytes: self.bytes.min(16) as u8,
            srgb: srgb && self.chans[0].2 == 8,
            ch: std::array::from_fn(|i| {
                let (word, shift, bits) = self.chans[i];
                let mut num = types[i].unwrap_or_default();
                if num == Num::Float && !matches!(bits, 32 | 16 | 11 | 10) {
                    num = Num::Unorm;
                }
                Chan::new(word, shift, bits, num)
            }),
        }
    }

    /// Integer or unpacked texels: anything but 4x4 blocks and the like.
    pub fn is_plain(&self) -> bool {
        matches!(self.kind, Kind::Packed | Kind::Rgba8)
    }
}

/// A texture, ready to sample.
#[derive(Clone, Debug)]
pub struct Texture {
    pub dim: Dim,
    pub format: TexFormat,
    pub packed: Packed,
    pub swizzle: [u8; 4],
    /// The swizzle is R, G, B, A.
    pub identity: bool,
    /// Results are integers: no filtering.
    pub integer: bool,
    /// The depth component is a normalised integer: references are clamped.
    pub depth_unorm: bool,
    pub normalized: bool,
    pub srgb: bool,
    pub min_lod_clamp: f32,
    pub lod_bias: f32,
    pub max_aniso: u32,
    pub address: u64,
    /// All levels of the image, and the first and last of the view.
    pub levels: Vec<Mip>,
    pub first: u32,
    pub last: u32,
    pub layer_stride: u64,
    /// Array layers, or cubes; 1 for 3D.
    pub layers: u32,
    pub ms: MsMode,
    /// Level 0 as multisample accesses see it: scaled by the samples of a pixel.
    pub ms_plane: Plane,
    pub ms_layer_stride: u64,
    pub is_buffer: bool,
    /// No texture: samples as zeros.
    pub null: bool,
}

impl Texture {
    /// From the 8 words of a texture header.
    pub fn from_words(t: &[u32; 8]) -> Result<Self, TexError> {
        Self::new(&Tic::decode(t)?)
    }

    pub fn new(t: &Tic) -> Result<Self, TexError> {
        let mut format = TexFormat::from_code(t.format).ok_or(TexError::Format(t.format))?;
        let packed = format.packed(t.types, t.srgb);
        if t.format == 8 && packed.ch.iter().all(|c| c.num == Num::Unorm) {
            format.kind = Kind::Rgba8;
        }
        let swizzle = t.swizzle.map(|s| s as u8);
        let integer = format.kind == Kind::Packed
            && t.swizzle.iter().any(|&s| {
                let c = packed.ch[(s as usize) & 3];
                (s as usize) < 4 && c.bits != 0 && c.num.is_int()
            });
        let is_3d = t.dim == Dim::D3;
        let height = if t.dim.is_1d() { 1 } else { t.height };
        let mut shape = Shape {
            width: t.width,
            height,
            depth: if is_3d { t.depth } else { 1 },
            is_3d,
            levels: t.max_level + 1,
            block_w: format.block_w,
            block_h: format.block_h,
            block_bytes: format.bytes,
            ms: MsMode::X1,
        };
        let is_buffer = t.dim == Dim::Buffer || t.layout == Layout::Buffer;
        let (chain, ms_chain) = match t.layout {
            Layout::BlockLinear { block } => {
                let block = BlockDims { width: t.tile_width, ..block };
                let chain = Chain::from_block(&shape, block);
                shape.ms = t.ms;
                let ms = if t.ms == MsMode::X1 { None } else { Some(Chain::from_block(&shape, block)) };
                (chain, ms)
            }
            Layout::Pitch { pitch } => {
                let mip = Mip { offset: 0, plane: Plane::pitch(pitch), width: t.width, height, depth: 1 };
                (Chain { levels: vec![mip], layer_stride: 0 }, None)
            }
            Layout::Buffer => {
                let mip = Mip { offset: 0, plane: Plane::pitch(0), width: t.width, height: 1, depth: 1 };
                (Chain { levels: vec![mip], layer_stride: 0 }, None)
            }
        };
        let ms_chain = ms_chain.as_ref().unwrap_or(&chain);
        let last = t.view_max.min(t.max_level);
        Ok(Self {
            dim: t.dim,
            format,
            packed,
            swizzle,
            identity: t.swizzle == [Source::R, Source::G, Source::B, Source::A],
            integer,
            depth_unorm: matches!(packed.ch[format.depth as usize].num, Num::Unorm | Num::Snorm),
            normalized: t.normalized,
            srgb: t.srgb,
            min_lod_clamp: t.min_lod_clamp,
            lod_bias: t.lod_bias,
            max_aniso: t.max_aniso,
            address: t.address,
            first: t.view_min.min(last),
            last,
            layers: if is_3d || is_buffer { 1 } else { t.depth },
            ms: t.ms,
            ms_plane: ms_chain.levels[0].plane,
            ms_layer_stride: ms_chain.layer_stride,
            layer_stride: chain.layer_stride,
            levels: chain.levels,
            is_buffer,
            null: t.view_min > t.view_max,
        })
    }

    /// A texture that samples as zeros, for headers that give none.
    pub fn empty() -> Self {
        let plane = Plane::pitch(0);
        Self {
            dim: Dim::D2,
            format: TexFormat { kind: Kind::Packed, bytes: 1, block_w: 1, block_h: 1, chans: [(0, 0, 0); 4], depth: 0 },
            packed: Packed::default(),
            swizzle: [4; 4],
            identity: false,
            integer: false,
            depth_unorm: false,
            normalized: true,
            srgb: false,
            min_lod_clamp: 0.0,
            lod_bias: 0.0,
            max_aniso: 1,
            address: 0,
            levels: vec![Mip { offset: 0, plane, width: 1, height: 1, depth: 1 }],
            first: 0,
            last: 0,
            layer_stride: 0,
            layers: 1,
            ms: MsMode::X1,
            ms_plane: plane,
            ms_layer_stride: 0,
            is_buffer: false,
            null: true,
        }
    }

    /// The level `lod` levels from the view's first.
    #[inline]
    pub fn level(&self, lod: u32) -> Option<&Mip> {
        let l = self.first.checked_add(lod)?;
        if l > self.last {
            return None;
        }
        self.levels.get(l as usize)
    }

    /// The result's components from the stored ones.
    #[inline(always)]
    pub fn swizzled(&self, t: [u32; 4]) -> [u32; 4] {
        if self.identity {
            return t;
        }
        const ONE: u32 = 0x3f80_0000;
        let all = [t[0], t[1], t[2], t[3], 0, 1, ONE, 0];
        self.swizzle.map(|s| all[(s & 7) as usize])
    }

    /// The stored components of the texel in column `x` (row `y`, slice `z`) of a plane at
    /// `base`.
    #[inline]
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn fetch<M: Memory + ?Sized>(
        &self,
        cache: &mut TexCache,
        mem: &M,
        plane: &Plane,
        base: u64,
        x: u32,
        y: u32,
        z: u32,
        srgb: bool,
    ) -> [u32; 4] {
        let f = &self.format;
        match f.kind {
            Kind::Rgba8 => {
                let addr = base + plane.offset(x * 4, y, z);
                let b = cache.word(mem, addr).to_le_bytes();
                if srgb {
                    let t = srgb8_table();
                    bits([t[b[0] as usize], t[b[1] as usize], t[b[2] as usize], b[3] as f32 / 255.0])
                } else {
                    bits(b.map(|v| v as f32 / 255.0))
                }
            }
            Kind::Packed => {
                let addr = base + plane.offset(x * f.bytes, y, z);
                let w = cache.words(mem, addr, f.bytes);
                let mut p = self.packed;
                p.srgb &= srgb;
                p.decode(&w)
            }
            Kind::Rgb9e5 => {
                let v = rgb9e5_to_f32(cache.word(mem, base + plane.offset(x * 4, y, z)));
                bits([v[0], v[1], v[2], 1.0])
            }
            Kind::Yuv { y: luma, r, b } => {
                let w = cache.word(mem, base + plane.offset((x >> 1) * 4, y, z)).to_le_bytes();
                let v = [w[r as usize], w[luma[(x & 1) as usize] as usize], w[b as usize], 255];
                bits(v.map(|v| v as f32 / 255.0))
            }
            Kind::R1 => {
                let w = cache.words(mem, base + plane.offset(x >> 3, y, z), 1)[0];
                bits([(w >> (x & 7) & 1) as f32, 0.0, 0.0, 1.0])
            }
            Kind::Block(code) => {
                let addr = base + plane.offset((x >> 2) * f.bytes, y >> 2, z);
                let snorm = self.packed.ch[0].num == Num::Snorm;
                bits(cache.block(mem, addr, code, snorm, srgb)[((y & 3) * 4 + (x & 3)) as usize])
            }
        }
    }
}

/// A sampler, ready to use.
#[derive(Clone, Copy, Debug)]
pub struct Sampler {
    pub wrap: [Wrap; 3],
    pub compare: Option<Compare>,
    pub srgb: bool,
    pub mag_linear: bool,
    pub min_linear: bool,
    pub max_aniso: u32,
    pub mip: MipFilter,
    pub cube_edges: CubeEdges,
    pub reduction: Reduction,
    pub lod_bias: f32,
    pub min_lod: f32,
    pub max_lod: f32,
    pub force_unnormalized: bool,
    pub border: [u32; 4],
    /// No wrap mode of the first two coordinates reaches the border, and texels are averaged.
    pub simple_2d: bool,
}

impl Sampler {
    pub fn from_words(t: &[u32; 8]) -> Self {
        Self::new(&Tsc::decode(t))
    }

    pub fn new(t: &Tsc) -> Self {
        Self {
            wrap: t.wrap,
            compare: t.compare,
            srgb: t.srgb,
            mag_linear: t.mag_linear,
            min_linear: t.min_linear,
            max_aniso: t.max_aniso,
            mip: t.mip,
            cube_edges: t.cube_edges,
            reduction: t.reduction,
            lod_bias: t.lod_bias,
            min_lod: t.min_lod,
            max_lod: t.max_lod,
            force_unnormalized: t.force_unnormalized,
            border: t.border,
            simple_2d: !t.wrap[0].has_border()
                && !t.wrap[1].has_border()
                && t.reduction == Reduction::Average,
        }
    }
}

const LINES: usize = 256;
const BLOCKS: usize = 256;
const LINE: u64 = GOB_BYTES as u64;

/// Texels read lately: 512-byte lines of memory (a GOB of a block-linear texture) and decoded
/// 4x4 blocks, both direct mapped by address. It does not see writes: invalidate it where
/// the driver invalidates the texture caches.
pub struct TexCache {
    /// Valid entries have the current generation in their top bits.
    generation: u64,
    line_tags: Vec<u64>,
    lines: Vec<[u8; GOB_BYTES as usize]>,
    block_tags: Vec<(u64, u64)>,
    blocks: Vec<Texels>,
}

impl Default for TexCache {
    fn default() -> Self {
        Self::new()
    }
}

/// Above the line and block numbers of 48-bit addresses.
const GENERATION: u64 = 1 << 45;

impl TexCache {
    pub fn new() -> Self {
        Self {
            generation: GENERATION,
            line_tags: vec![0; LINES],
            lines: vec![[0; GOB_BYTES as usize]; LINES],
            block_tags: vec![(0, 0); BLOCKS],
            blocks: vec![[[0.0; 4]; 16]; BLOCKS],
        }
    }

    /// Forget everything.
    pub fn invalidate(&mut self) {
        match self.generation.checked_add(GENERATION) {
            Some(g) => self.generation = g,
            None => {
                self.generation = GENERATION;
                self.line_tags.fill(0);
                self.block_tags.fill((0, 0));
            }
        }
    }

    /// Forget what was read from `len` bytes at `addr`.
    pub fn invalidate_range(&mut self, addr: u64, len: u64) {
        if len == 0 {
            return;
        }
        let (first, last) = (addr / LINE, (addr + len - 1) / LINE);
        for t in &mut self.line_tags {
            if (first..=last).contains(&(*t & (GENERATION - 1))) {
                *t = 0;
            }
        }
        for t in &mut self.block_tags {
            let line = (t.0 & (GENERATION - 1)) * 8 / LINE;
            if (first..=last).contains(&line) {
                *t = (0, 0);
            }
        }
    }

    /// The line of memory holding `addr`.
    #[inline(always)]
    pub(crate) fn line<M: Memory + ?Sized>(&mut self, mem: &M, addr: u64) -> &[u8; GOB_BYTES as usize] {
        let n = addr / LINE;
        let i = ((n ^ (n >> 8) ^ (n >> 16)) as usize) & (LINES - 1);
        let tag = self.generation | n;
        if self.line_tags[i] != tag {
            self.line_tags[i] = tag;
            fill(mem, n * LINE, &mut self.lines[i]);
        }
        &self.lines[i]
    }

    /// The aligned 32-bit word at `addr`.
    #[inline(always)]
    pub(crate) fn word<M: Memory + ?Sized>(&mut self, mem: &M, addr: u64) -> u32 {
        if addr & 3 != 0 {
            return self.words(mem, addr, 4)[0];
        }
        let o = (addr & (LINE - 4)) as usize;
        let l = self.line(mem, addr);
        u32::from_le_bytes([l[o], l[o + 1], l[o + 2], l[o + 3]])
    }

    /// `n` (up to 16) bytes at `addr` as words.
    #[inline]
    pub(crate) fn words<M: Memory + ?Sized>(&mut self, mem: &M, addr: u64, n: u32) -> [u32; 4] {
        let n = n.min(16) as usize;
        let o = (addr & (LINE - 1)) as usize;
        let mut b = [0u8; 16];
        if o + n <= LINE as usize {
            b[..n].copy_from_slice(&self.line(mem, addr)[o..o + n]);
        } else {
            for (i, v) in b.iter_mut().enumerate().take(n) {
                let a = addr + i as u64;
                *v = self.line(mem, a)[(a & (LINE - 1)) as usize];
            }
        }
        crate::num::words(&b)
    }

    /// The decoded block at `addr`.
    #[inline]
    fn block<M: Memory + ?Sized>(&mut self, mem: &M, addr: u64, code: BlockCode, snorm: bool, srgb: bool) -> &Texels {
        let n = addr / 8;
        let i = ((n ^ (n >> 8) ^ (n >> 16)) as usize) & (BLOCKS - 1);
        let tag = (self.generation | n, code as u64 | (snorm as u64) << 8 | (srgb as u64) << 9);
        if self.block_tags[i] != tag {
            let w = self.words(mem, addr, code.bytes());
            self.block_tags[i] = tag;
            self.blocks[i] = decode_block(&crate::num::bytes(&w), code, snorm, srgb);
        }
        &self.blocks[i]
    }
}

#[cold]
#[inline(never)]
fn fill<M: Memory + ?Sized>(mem: &M, addr: u64, line: &mut [u8; GOB_BYTES as usize]) {
    mem.read(addr, line);
}

/// Decode a block; the 8-byte codes use the first 8 bytes of `b`.
pub fn decode_block(b: &[u8; 16], code: BlockCode, snorm: bool, srgb: bool) -> Texels {
    match code {
        BlockCode::Bc1 => bc::bc1(b, srgb),
        BlockCode::Bc2 => bc::bc2(b, srgb),
        BlockCode::Bc3 => bc::bc3(b, srgb),
        BlockCode::Bc4 => bc::bc4(b, snorm),
        BlockCode::Bc5 => bc::bc5(b, snorm),
        BlockCode::Bc6S => bptc::bc6h(b, true),
        BlockCode::Bc6U => bptc::bc6h(b, false),
        BlockCode::Bc7 => bptc::bc7(b, srgb),
        BlockCode::Etc2Rgb => etc::etc2_rgb(b, srgb),
        BlockCode::Etc2Rgb8A1 => etc::etc2_rgb8a1(b, srgb),
        BlockCode::Etc2Rgba => etc::etc2_rgba(b, srgb),
        BlockCode::EacR => etc::eac_r(b, snorm),
        BlockCode::EacRg => etc::eac_rg(b, snorm),
    }
}
