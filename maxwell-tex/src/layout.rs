//! Surface layouts: block linear and pitch, mip chains, array layers, multisampling.
//!
//! A block-linear surface is a grid of blocks in row-major order. A block is one GOB (64
//! bytes by 8 rows, 512 bytes) wide, `1 << height` GOBs high and `1 << depth` GOBs deep: its
//! GOBs follow each other down, then in depth. A GOB is stored in the "Tegra" sector order.
//!
//! A mip level's block shrinks with the level ([`BlockDims::shrink`]); levels follow each
//! other in a layer, layers are `layer_stride` apart. A multisampled surface is stored as a
//! surface of samples, 2x1, 2x2 or 4x2 as large.

use crate::Memory;

pub const GOB_WIDTH: u32 = 64;
pub const GOB_HEIGHT: u32 = 8;
pub const GOB_BYTES: u32 = 512;

/// Offset in a GOB from the byte column (bits 0-5 of x): bits 8, 5 and 3-0.
const X_SWIZZLE: [u16; 64] = {
    let mut t = [0u16; 64];
    let mut x = 0;
    while x < 64 {
        t[x] = (((x & 32) << 3) | ((x & 16) << 1) | (x & 15)) as u16;
        x += 1;
    }
    t
};

/// Offset in a GOB from the row (bits 0-2 of y): bits 7, 6 and 4.
const Y_SWIZZLE: [u16; 8] = {
    let mut t = [0u16; 8];
    let mut y = 0;
    while y < 8 {
        t[y] = (((y & 6) << 5) | ((y & 1) << 4)) as u16;
        y += 1;
    }
    t
};

/// Byte offset in a GOB of byte column `x` and row `y` (taken modulo the GOB's size).
#[inline(always)]
pub fn gob_offset(x: u32, y: u32) -> u32 {
    (X_SWIZZLE[(x & 63) as usize] | Y_SWIZZLE[(y & 7) as usize]) as u32
}

/// Where the `piece`th 16 bytes of a GOB are in rows of bytes.
#[inline(always)]
fn piece_offset(piece: usize, stride: usize) -> usize {
    let x = (piece >> 1 & 1) * 16 + (piece >> 4) * 32;
    let y = (piece & 1) + (piece >> 2 & 3) * 2;
    y * stride + x
}

/// Block size in GOBs, as log2. `width` is always 0 on these surfaces as the drivers make
/// them (a block is one GOB wide).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct BlockDims {
    pub width: u8,
    pub height: u8,
    pub depth: u8,
}

impl BlockDims {
    pub const fn new(height: u8, depth: u8) -> Self {
        Self { width: 0, height, depth }
    }

    /// From the engines' tile mode words (RT_TILE_MODE, ZETA_TILE_MODE, 2D and copy engine):
    /// 4 bits each of width, height, depth.
    pub const fn from_tile_mode(m: u32) -> Self {
        Self { width: (m & 15) as u8, height: ((m >> 4) & 15) as u8, depth: ((m >> 8) & 15) as u8 }
    }

    pub const fn tile_mode(self) -> u32 {
        self.width as u32 | (self.height as u32) << 4 | (self.depth as u32) << 8
    }

    /// Bytes in a block.
    pub const fn size(self) -> u32 {
        GOB_BYTES << (self.width + self.height + self.depth)
    }

    /// Rows in a block.
    pub const fn rows(self) -> u32 {
        GOB_HEIGHT << self.height
    }

    /// The block of a mip level of `rows` rows and `depth` slices: no dimension is twice as
    /// large as the level or more. The hardware does this for each level of a texture.
    pub fn shrink(self, rows: u32, depth: u32) -> Self {
        let gobs = rows.div_ceil(GOB_HEIGHT);
        Self {
            width: self.width,
            height: self.height.min(log2_ceil(gobs)),
            depth: self.depth.min(log2_ceil(depth)),
        }
    }
}

fn log2_ceil(v: u32) -> u8 {
    v.max(1).next_power_of_two().trailing_zeros() as u8
}

/// The block size the nvc0 driver picks for a level of `rows` rows of texels (or of
/// compressed blocks) and `depth` slices.
pub fn nvc0_block(rows: u32, depth: u32, is_3d: bool) -> BlockDims {
    let height = match rows {
        65.. => 4,
        33.. => 3,
        17.. => 2,
        9.. => 1,
        _ => 0,
    };
    if !is_3d {
        return BlockDims::new(height, 0);
    }
    let height = height.min(2);
    let depth = match depth {
        17.. if height < 2 => 5,
        9.. => 4,
        5.. => 3,
        3.. => 2,
        2 => 1,
        _ => 0,
    };
    BlockDims::new(height, depth)
}

/// One level of one layer: where the byte at column `x`, row `y`, slice `z` is.
/// Rows are rows of texels, or of compressed blocks.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Plane {
    /// Bytes from one row to the next (block linear: a row of GOBs is `pitch` bytes wide).
    pub pitch: u32,
    /// Rows, up to whole blocks (block linear only; 0 for pitch surfaces).
    pub rows: u32,
    /// None for a pitch surface.
    pub block: Option<BlockDims>,
    // Derived: log2 rows in a block, log2 bytes in a 2D slice of a block (block column of
    // GOBs), log2 bytes in a block, bytes from one row of blocks to the next, bytes from one
    // layer of blocks to the next.
    row_shift: u8,
    slice_shift: u8,
    block_shift: u8,
    depth_shift: u8,
    row_stride: u64,
    slice_stride: u64,
}

impl Plane {
    pub fn pitch(pitch: u32) -> Self {
        Self {
            pitch,
            rows: 0,
            block: None,
            row_shift: 0,
            slice_shift: 0,
            block_shift: 0,
            depth_shift: 0,
            row_stride: 0,
            slice_stride: 0,
        }
    }

    /// `width_bytes` by `rows`, rounded up to whole blocks.
    pub fn block_linear(width_bytes: u32, rows: u32, b: BlockDims) -> Self {
        let pitch = width_bytes.max(1).next_multiple_of(GOB_WIDTH << b.width);
        let rows = rows.max(1).next_multiple_of(b.rows());
        let row_shift = 3 + b.height;
        let block_shift = 9 + b.height + b.depth;
        let row_stride = ((pitch / GOB_WIDTH) as u64) << block_shift;
        Self {
            pitch,
            rows,
            block: Some(b),
            row_shift,
            slice_shift: 9 + b.height,
            block_shift,
            depth_shift: b.depth,
            row_stride,
            slice_stride: row_stride * (rows >> row_shift) as u64,
        }
    }

    /// Bytes of `depth` slices.
    pub fn size(&self, depth: u32) -> u64 {
        match self.block {
            Some(b) => self.pitch as u64 * self.rows as u64 * depth.max(1).next_multiple_of(1 << b.depth) as u64,
            None => 0,
        }
    }

    /// Offset of the GOB holding (`x` bytes, row `y`, slice `z`) of a block-linear plane.
    #[inline(always)]
    pub fn gob(&self, x: u32, y: u32, z: u32) -> u64 {
        let in_block = (((y >> 3) & ((1 << (self.row_shift - 3)) - 1)) as u64) << 9
            | ((z & ((1 << self.depth_shift) - 1)) as u64) << self.slice_shift;
        (z >> self.depth_shift) as u64 * self.slice_stride
            + (y >> self.row_shift) as u64 * self.row_stride
            + (((x >> 6) as u64) << self.block_shift)
            + in_block
    }

    /// Offset of the byte at column `x`, row `y`, slice `z`.
    #[inline(always)]
    pub fn offset(&self, x: u32, y: u32, z: u32) -> u64 {
        if self.block.is_none() {
            y as u64 * self.pitch as u64 + x as u64
        } else {
            self.gob(x, y, z) + gob_offset(x, y) as u64
        }
    }

    /// Offset of slice `z` from the level's start, as the nvc0 driver computes it.
    pub fn slice_offset(&self, z: u32) -> u64 {
        self.gob(0, 0, z)
    }

    /// Copy the rectangle `w` bytes by `h` rows at (`x`, `y`) of slice `z` into `out`, whose
    /// rows are `stride` bytes apart. `base` is the plane's address.
    #[allow(clippy::too_many_arguments)]
    pub fn read_rect<M: Memory + ?Sized>(
        &self,
        mem: &M,
        base: u64,
        rect: Rect,
        z: u32,
        out: &mut [u8],
        stride: usize,
    ) {
        let Rect { x, y, w, h } = rect;
        if w == 0 || h == 0 {
            return;
        }
        if self.block.is_none() {
            for r in 0..h {
                let o = r as usize * stride;
                mem.read(base + self.offset(x, y + r, z), &mut out[o..o + w as usize]);
            }
            return;
        }
        let mut gob = [0u8; GOB_BYTES as usize];
        for gy in (y / 8)..=((y + h - 1) / 8) {
            let (r0, r1) = (y.max(gy * 8), (y + h).min(gy * 8 + 8));
            for gx in (x / 64)..=((x + w - 1) / 64) {
                let (c0, c1) = (x.max(gx * 64), (x + w).min(gx * 64 + 64));
                mem.read(base + self.gob(gx * 64, gy * 8, z), &mut gob);
                if r1 - r0 == 8 && c1 - c0 == 64 {
                    let at = (gy * 8 - y) as usize * stride + (gx * 64 - x) as usize;
                    for (piece, p) in gob.as_chunks::<16>().0.iter().enumerate() {
                        let o = at + piece_offset(piece, stride);
                        out[o..o + 16].copy_from_slice(p);
                    }
                    continue;
                }
                for r in r0..r1 {
                    let row = (r - y) as usize * stride;
                    let mut c = c0;
                    while c < c1 {
                        // 16 bytes of a row are contiguous.
                        let n = (16 - (c & 15)).min(c1 - c) as usize;
                        let s = gob_offset(c, r) as usize;
                        let d = row + (c - x) as usize;
                        out[d..d + n].copy_from_slice(&gob[s..s + n]);
                        c += n as u32;
                    }
                }
            }
        }
    }

    /// The reverse of [`Self::read_rect`]. Only the bytes of the rectangle are written.
    #[allow(clippy::too_many_arguments)]
    pub fn write_rect<M: Memory + ?Sized>(
        &self,
        mem: &mut M,
        base: u64,
        rect: Rect,
        z: u32,
        data: &[u8],
        stride: usize,
    ) {
        let Rect { x, y, w, h } = rect;
        if w == 0 || h == 0 {
            return;
        }
        if self.block.is_none() {
            for r in 0..h {
                let o = r as usize * stride;
                mem.write(base + self.offset(x, y + r, z), &data[o..o + w as usize]);
            }
            return;
        }
        let mut gob = [0u8; GOB_BYTES as usize];
        for gy in (y / 8)..=((y + h - 1) / 8) {
            for gx in (x / 64)..=((x + w - 1) / 64) {
                let addr = base + self.gob(gx * 64, gy * 8, z);
                if gy * 8 >= y && gy * 8 + 8 <= y + h && gx * 64 >= x && gx * 64 + 64 <= x + w {
                    let at = (gy * 8 - y) as usize * stride + (gx * 64 - x) as usize;
                    for (piece, p) in gob.as_chunks_mut::<16>().0.iter_mut().enumerate() {
                        let o = at + piece_offset(piece, stride);
                        p.copy_from_slice(&data[o..o + 16]);
                    }
                    mem.write(addr, &gob);
                    continue;
                }
                // In memory order, the 16-byte pieces of the GOB; runs of them are written
                // together.
                let (mut start, mut end) = (0usize, 0usize);
                for piece in 0..32u32 {
                    let px = gx * 64 + ((piece >> 1) & 1) * 16 + (piece >> 4) * 32;
                    let py = gy * 8 + (piece & 1) + ((piece >> 2) & 3) * 2;
                    if py < y || py >= y + h {
                        continue;
                    }
                    let (c0, c1) = (px.max(x), (px + 16).min(x + w));
                    if c0 >= c1 {
                        continue;
                    }
                    let n = (c1 - c0) as usize;
                    let to = (piece * 16 + (c0 - px)) as usize;
                    let from = (py - y) as usize * stride + (c0 - x) as usize;
                    gob[to..to + n].copy_from_slice(&data[from..from + n]);
                    if to != end {
                        if end > start {
                            mem.write(addr + start as u64, &gob[start..end]);
                        }
                        start = to;
                    }
                    end = to + n;
                }
                if end > start {
                    mem.write(addr + start as u64, &gob[start..end]);
                }
            }
        }
    }
}

/// A rectangle in a plane: `x` and `w` in bytes, `y` and `h` in rows.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Rect {
    pub x: u32,
    pub y: u32,
    pub w: u32,
    pub h: u32,
}

/// What a mip chain is made from. Sizes in texels.
#[derive(Clone, Copy, Debug)]
pub struct Shape {
    pub width: u32,
    pub height: u32,
    pub depth: u32,
    pub is_3d: bool,
    pub levels: u32,
    /// Compressed block: width, height (1 for plain formats) and bytes.
    pub block_w: u32,
    pub block_h: u32,
    pub block_bytes: u32,
    pub ms: MsMode,
}

/// One level of a mip chain.
#[derive(Clone, Copy, Debug)]
pub struct Mip {
    /// From the layer's start.
    pub offset: u64,
    pub plane: Plane,
    /// In texels.
    pub width: u32,
    pub height: u32,
    pub depth: u32,
}

/// The levels of a layer.
#[derive(Clone, Debug)]
pub struct Chain {
    pub levels: Vec<Mip>,
    /// From one array layer (or cube face) to the next.
    pub layer_stride: u64,
}

impl Chain {
    fn build(s: &Shape, block: impl Fn(u32, u32) -> BlockDims) -> Self {
        let (mx, my) = s.ms.shift();
        let mut levels = Vec::with_capacity(s.levels as usize);
        let mut total = 0u64;
        for l in 0..s.levels.max(1) {
            let (w, h) = (((s.width << mx) >> l).max(1), ((s.height << my) >> l).max(1));
            let d = if s.is_3d { (s.depth >> l).max(1) } else { 1 };
            let rows = h.div_ceil(s.block_h);
            let plane = Plane::block_linear(w.div_ceil(s.block_w) * s.block_bytes, rows, block(rows, d));
            levels.push(Mip { offset: total, plane, width: w, height: h, depth: d });
            total += plane.size(d);
        }
        let b0 = levels[0].plane.block.unwrap_or_default();
        Self { levels, layer_stride: total.next_multiple_of(b0.size() as u64) }
    }

    /// The chain as the nvc0 driver lays it out, choosing each level's block itself.
    pub fn nvc0(s: &Shape) -> Self {
        Self::build(s, |rows, d| nvc0_block(rows, d, s.is_3d))
    }

    /// The chain the texture unit derives from a texture header: the first level's block,
    /// shrunk for each level.
    ///
    /// nvc0's own choice for small 3D levels can be deeper than the first level's block
    /// (first level 4 GOBs high and 16 deep, then 2 high and 32 deep); a 3D texture whose
    /// first block is nvc0's choice uses nvc0's blocks throughout.
    pub fn from_block(s: &Shape, first: BlockDims) -> Self {
        let rows0 = s.height.div_ceil(s.block_h);
        if s.is_3d && first == nvc0_block(rows0, s.depth, true) {
            return Self::nvc0(s);
        }
        Self::build(s, |rows, d| first.shrink(rows, d))
    }

    /// Bytes of `layers` layers.
    pub fn size(&self, layers: u32) -> u64 {
        self.layer_stride * layers.max(1) as u64
    }
}

/// Multisample mode (MULTISAMPLE_MODE, and the sample count field of a texture header).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
#[repr(u8)]
pub enum MsMode {
    #[default]
    X1 = 0,
    X2 = 1,
    X4 = 2,
    X8 = 3,
    /// 4x2 in D3D sample order.
    X8D3d = 4,
    /// 2x1 in D3D sample order.
    X2D3d = 5,
    X16 = 6,
}

impl MsMode {
    /// The modes with more coverage than colour samples (8 to 11) store as their colour mode.
    pub fn from_bits(v: u32) -> Self {
        match v & 15 {
            1 => Self::X2,
            2 | 8 | 9 => Self::X4,
            3 | 10 | 11 => Self::X8,
            4 => Self::X8D3d,
            5 => Self::X2D3d,
            6 => Self::X16,
            _ => Self::X1,
        }
    }

    /// The mode nvc0 uses for a sample count.
    pub fn from_samples(n: u32) -> Self {
        match n {
            2 => Self::X2,
            4 => Self::X4,
            8 => Self::X8,
            16 => Self::X16,
            _ => Self::X1,
        }
    }

    pub fn samples(self) -> u32 {
        let (x, y) = self.shift();
        1 << (x + y)
    }

    /// log2 of the samples of a pixel across and down.
    #[inline]
    pub fn shift(self) -> (u32, u32) {
        match self {
            Self::X1 => (0, 0),
            Self::X2 | Self::X2D3d => (1, 0),
            Self::X4 => (1, 1),
            Self::X8 | Self::X8D3d => (2, 1),
            Self::X16 => (2, 2),
        }
    }

    /// Where sample `s` of a pixel is stored in the pixel's samples. The order of the plain
    /// modes is the one nvc0 gives its shaders; that of the 4x4 mode is a guess.
    #[inline]
    pub fn sample_offset(self, s: u32) -> (u32, u32) {
        const X8D3D: [(u32, u32); 8] = [(2, 0), (1, 1), (3, 1), (1, 0), (0, 1), (0, 0), (2, 1), (3, 0)];
        match self {
            Self::X1 => (0, 0),
            Self::X2 => (s & 1, 0),
            Self::X2D3d => (!s & 1, 0),
            Self::X4 => (s & 1, (s >> 1) & 1),
            Self::X8 => ((s & 1) | ((s >> 1) & 2), (s >> 1) & 1),
            Self::X8D3d => X8D3D[(s & 7) as usize],
            Self::X16 => (s & 3, (s >> 2) & 3),
        }
    }

    /// Default sample positions in a pixel, in 1/16 pixel (nvc0's, which it also programs).
    pub fn positions(self) -> &'static [(u8, u8)] {
        match self {
            Self::X1 => &[(8, 8)],
            Self::X2 => &[(4, 4), (12, 12)],
            Self::X2D3d => &[(12, 12), (4, 4)],
            Self::X4 => &[(6, 2), (14, 6), (2, 10), (10, 14)],
            Self::X8 => &[(1, 7), (5, 3), (3, 13), (7, 11), (9, 5), (15, 1), (11, 15), (13, 9)],
            Self::X8D3d => &[(9, 5), (7, 11), (13, 9), (5, 3), (3, 13), (1, 7), (11, 15), (15, 1)],
            Self::X16 => &[
                (9, 9), (7, 5), (5, 10), (12, 7), (3, 6), (10, 13), (13, 11), (11, 3),
                (6, 14), (8, 1), (4, 2), (2, 12), (0, 8), (15, 4), (14, 15), (1, 0),
            ],
        }
    }
}
