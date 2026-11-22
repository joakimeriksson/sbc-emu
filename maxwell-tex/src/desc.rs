//! Texture headers (TIC) and samplers (TSC), 32 bytes each, as GM20B reads them.

use crate::layout::{BlockDims, MsMode};
use crate::num::Num;

/// Texture target.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum Dim {
    D1 = 0,
    D2 = 1,
    D3 = 2,
    Cube = 3,
    D1Array = 4,
    D2Array = 5,
    Buffer = 6,
    /// 2D of a single level, the only target of pitch textures.
    D2NoMip = 7,
    CubeArray = 8,
}

impl Dim {
    pub fn from_bits(v: u32) -> Option<Self> {
        Some(match v {
            0 => Self::D1,
            1 => Self::D2,
            2 => Self::D3,
            3 => Self::Cube,
            4 => Self::D1Array,
            5 => Self::D2Array,
            6 => Self::Buffer,
            7 => Self::D2NoMip,
            8 => Self::CubeArray,
            _ => return None,
        })
    }

    pub fn is_array(self) -> bool {
        matches!(self, Self::D1Array | Self::D2Array | Self::CubeArray)
    }

    pub fn is_cube(self) -> bool {
        matches!(self, Self::Cube | Self::CubeArray)
    }

    pub fn is_1d(self) -> bool {
        matches!(self, Self::D1 | Self::D1Array | Self::Buffer)
    }
}

/// Header version: the kind of memory layout.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Layout {
    Buffer,
    Pitch { pitch: u32 },
    BlockLinear { block: BlockDims },
}

/// Where a component of the result comes from.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum Source {
    R = 0,
    G = 1,
    B = 2,
    A = 3,
    Zero = 4,
    OneInt = 5,
    OneFloat = 6,
}

impl Source {
    fn from_bits(v: u32) -> Self {
        match v & 7 {
            2 => Self::R,
            3 => Self::G,
            4 => Self::B,
            5 => Self::A,
            6 => Self::OneInt,
            7 => Self::OneFloat,
            _ => Self::Zero,
        }
    }
}

/// A decoded texture header.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Tic {
    /// Component sizes: the format code (see [`crate::texture::TexFormat`]).
    pub format: u8,
    /// Data types of the stored red, green, blue and alpha. None for reserved codes.
    pub types: [Option<Num>; 4],
    pub swizzle: [Source; 4],
    pub address: u64,
    pub layout: Layout,
    /// log2 of the GOBs a row of blocks is rounded up to (sparse textures).
    pub tile_width: u8,
    pub dim: Dim,
    /// Texels (pixels of a multisampled texture) of level 0.
    pub width: u32,
    pub height: u32,
    /// Depth, array layers, or cubes.
    pub depth: u32,
    pub normalized: bool,
    pub srgb: bool,
    pub depth_texture: bool,
    /// Levels in memory, less one.
    pub max_level: u32,
    /// The view's first and last level.
    pub view_min: u32,
    pub view_max: u32,
    /// Relative to `view_min`.
    pub min_lod_clamp: f32,
    pub lod_bias: f32,
    pub max_aniso: u32,
    pub ms: MsMode,
    /// The border is the sampler's colour rather than texels around the image.
    pub border_from_sampler: bool,
    /// Colour key headers.
    pub color_key: Option<u32>,
}

/// The header did not decode: reserved header version or texture type.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct BadHeader;

const ANISO: [u32; 8] = [1, 2, 4, 6, 8, 10, 12, 16];

/// `bits` bits at `shift` of `w`.
fn field(w: u32, shift: u32, bits: u32) -> u32 {
    (w >> shift) & ((1 << bits) - 1)
}

/// Signed 5.8 fixed point.
fn s5_8(v: u32) -> f32 {
    ((v << 19) as i32 >> 19) as f32 / 256.0
}

impl Tic {
    pub fn decode(t: &[u32; 8]) -> Result<Self, BadHeader> {
        let version = field(t[2], 21, 3);
        let high = (t[2] as u64 & 0xffff) << 32;
        let (layout, address) = match version {
            0 => (Layout::Buffer, high | t[1] as u64),
            1 | 2 => (Layout::Pitch { pitch: (t[3] & 0xffff) << 5 }, high | (t[1] & !31) as u64),
            3 | 4 => {
                let block = BlockDims {
                    width: field(t[3], 0, 3) as u8,
                    height: field(t[3], 3, 3) as u8,
                    depth: field(t[3], 6, 3) as u8,
                };
                (Layout::BlockLinear { block }, high | (t[1] & !511) as u64)
            }
            _ => return Err(BadHeader),
        };
        let color_key = (version == 1 || version == 4).then_some(t[7]);
        let is_buffer = layout == Layout::Buffer;
        let mips = matches!(layout, Layout::BlockLinear { .. });
        let max_level = if mips { field(t[3], 28, 4) } else { 0 };
        // Dword 7 holds the colour key instead of the view in colour key headers.
        let view = if color_key.is_some() { max_level << 4 } else { t[7] };
        Ok(Self {
            format: (t[0] & 127) as u8,
            types: std::array::from_fn(|i| Num::from_tic(field(t[0], 7 + 3 * i as u32, 3))),
            swizzle: std::array::from_fn(|i| Source::from_bits(field(t[0], 19 + 3 * i as u32, 3))),
            address,
            layout,
            tile_width: if mips { field(t[3], 10, 3) as u8 } else { 0 },
            dim: Dim::from_bits(field(t[4], 23, 4)).ok_or(BadHeader)?,
            width: if is_buffer {
                ((t[3] & 0xffff) << 16 | (t[4] & 0xffff)).wrapping_add(1)
            } else {
                (t[4] & 0xffff) + 1
            },
            height: if is_buffer { 1 } else { (t[5] & 0xffff) + 1 },
            depth: if is_buffer { 1 } else { field(t[5], 16, 14) + 1 },
            normalized: t[5] >> 31 != 0,
            srgb: t[4] >> 22 & 1 != 0,
            depth_texture: t[3] >> 27 & 1 != 0 && !is_buffer,
            max_level,
            view_min: if mips { view & 15 } else { 0 },
            view_max: if mips { field(view, 4, 4) } else { 0 },
            min_lod_clamp: if mips { field(view, 12, 12) as f32 / 256.0 } else { 0.0 },
            lod_bias: if t[3] >> 26 & 1 != 0 { s5_8(field(t[6], 6, 13)) } else { 0.0 },
            max_aniso: ANISO[field(t[6], 27, 3) as usize],
            ms: if mips { MsMode::from_bits(field(view, 8, 4)) } else { MsMode::X1 },
            border_from_sampler: field(t[4], 29, 3) == 7,
            color_key,
        })
    }
}

/// Texture coordinate wrap modes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum Wrap {
    Repeat = 0,
    Mirror = 1,
    ClampToEdge = 2,
    Border = 3,
    /// GL_CLAMP: half edge, half border.
    ClampGl = 4,
    MirrorOnceClampToEdge = 5,
    MirrorOnceBorder = 6,
    MirrorOnceClampGl = 7,
}

impl Wrap {
    fn from_bits(v: u32) -> Self {
        match v & 7 {
            0 => Self::Repeat,
            1 => Self::Mirror,
            2 => Self::ClampToEdge,
            3 => Self::Border,
            4 => Self::ClampGl,
            5 => Self::MirrorOnceClampToEdge,
            6 => Self::MirrorOnceBorder,
            _ => Self::MirrorOnceClampGl,
        }
    }

    /// Can give texels of the border.
    pub fn has_border(self) -> bool {
        matches!(self, Self::Border | Self::ClampGl | Self::MirrorOnceBorder | Self::MirrorOnceClampGl)
    }
}

/// Depth comparison: reference `op` texel.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum Compare {
    Never,
    Less,
    Equal,
    LessEqual,
    Greater,
    NotEqual,
    GreaterEqual,
    Always,
}

impl Compare {
    pub fn from_bits(v: u32) -> Self {
        match v & 7 {
            0 => Self::Never,
            1 => Self::Less,
            2 => Self::Equal,
            3 => Self::LessEqual,
            4 => Self::Greater,
            5 => Self::NotEqual,
            6 => Self::GreaterEqual,
            _ => Self::Always,
        }
    }

    #[inline]
    pub fn test(self, reference: f32, texel: f32) -> bool {
        match self {
            Self::Never => false,
            Self::Less => reference < texel,
            Self::Equal => reference == texel,
            Self::LessEqual => reference <= texel,
            Self::Greater => reference > texel,
            Self::NotEqual => reference != texel,
            Self::GreaterEqual => reference >= texel,
            Self::Always => true,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MipFilter {
    None,
    Nearest,
    Linear,
}

/// What is done with the texels of the footprint.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Reduction {
    Average,
    Min,
    Max,
}

/// What happens at the edges of a cube face.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CubeEdges {
    /// The sampler's wrap modes.
    Wrap,
    ClampToEdge,
    /// Filter across the faces.
    Seamless,
}

/// A decoded sampler.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Tsc {
    pub wrap: [Wrap; 3],
    pub compare: Option<Compare>,
    /// Decode sRGB textures.
    pub srgb: bool,
    pub max_aniso: u32,
    pub mag_linear: bool,
    pub min_linear: bool,
    pub min_aniso: bool,
    pub mip: MipFilter,
    pub cube_edges: CubeEdges,
    pub reduction: Reduction,
    pub lod_bias: f32,
    pub min_lod: f32,
    pub max_lod: f32,
    pub force_unnormalized: bool,
    /// f32 bits or integers, in the order of the result (the texture's swizzle does not apply).
    pub border: [u32; 4],
    /// The border for sRGB textures.
    pub srgb_border: [u8; 3],
}

impl Tsc {
    pub fn decode(t: &[u32; 8]) -> Self {
        let min = field(t[1], 4, 2);
        Self {
            wrap: std::array::from_fn(|i| Wrap::from_bits(t[0] >> (3 * i))),
            compare: (t[0] >> 9 & 1 != 0).then(|| Compare::from_bits(t[0] >> 10)),
            srgb: t[0] >> 13 & 1 != 0,
            max_aniso: ANISO[field(t[0], 20, 3) as usize],
            mag_linear: field(t[1], 0, 3) >= 2,
            min_linear: min >= 2,
            min_aniso: min == 3,
            mip: match field(t[1], 6, 2) {
                2 => MipFilter::Nearest,
                3 => MipFilter::Linear,
                _ => MipFilter::None,
            },
            cube_edges: match field(t[1], 8, 2) {
                0 => CubeEdges::Wrap,
                1 => CubeEdges::ClampToEdge,
                _ => CubeEdges::Seamless,
            },
            reduction: match field(t[1], 10, 2) {
                1 => Reduction::Min,
                2 => Reduction::Max,
                _ => Reduction::Average,
            },
            lod_bias: s5_8(field(t[1], 12, 13)),
            min_lod: field(t[2], 0, 12) as f32 / 256.0,
            max_lod: field(t[2], 12, 12) as f32 / 256.0,
            force_unnormalized: t[1] >> 25 & 1 != 0,
            border: [t[4], t[5], t[6], t[7]],
            srgb_border: [(t[2] >> 24) as u8, (t[3] >> 12) as u8, (t[3] >> 20) as u8],
        }
    }
}
