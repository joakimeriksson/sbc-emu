//! Render target surfaces, and tiles of them in host memory for a rasterizer.

use crate::layout::{BlockDims, MsMode, Plane, Rect};
use crate::Memory;

/// A colour or depth target as it is bound: sizes are in samples.
#[derive(Clone, Copy, Debug)]
pub struct Surface {
    pub address: u64,
    /// Samples across and down.
    pub width: u32,
    pub height: u32,
    /// Bytes per sample.
    pub bytes: u32,
    pub plane: Plane,
    /// From one array layer to the next, in bytes.
    pub layer_stride: u64,
    /// Layers are the slices of a 3D surface.
    pub is_3d: bool,
    pub ms: MsMode,
}

impl Surface {
    /// From the registers of a target: `width` is RT_HORIZ (samples; for pitch surfaces the
    /// pitch in bytes), `height` RT_VERT, `tile_mode` RT_TILE_MODE (bit 12 pitch, bit 16 3D),
    /// `layer_stride` in bytes (the register counts in 4).
    pub fn new(
        address: u64,
        width: u32,
        height: u32,
        tile_mode: u32,
        layer_stride: u64,
        bytes: u32,
        ms: MsMode,
    ) -> Self {
        let pitch = tile_mode >> 12 & 1 != 0;
        let plane = if pitch {
            Plane::pitch(width)
        } else {
            Plane::block_linear(width * bytes, height, BlockDims::from_tile_mode(tile_mode & 0xfff))
        };
        Self {
            address,
            width: if pitch { width / bytes.max(1) } else { width },
            height,
            bytes,
            plane,
            layer_stride,
            is_3d: tile_mode >> 16 & 1 != 0,
            ms,
        }
    }

    /// Pixels across and down.
    pub fn pixels(&self) -> (u32, u32) {
        let (mx, my) = self.ms.shift();
        (self.width >> mx, self.height >> my)
    }

    /// A layer's address, and its slice there.
    pub fn layer(&self, layer: u32) -> (u64, u32) {
        if self.is_3d {
            (self.address, layer)
        } else {
            (self.address + self.layer_stride * layer as u64, 0)
        }
    }

    /// Address of a sample.
    pub fn sample_address(&self, x: u32, y: u32, layer: u32, sample: u32) -> u64 {
        let (mx, my) = self.ms.shift();
        let (sx, sy) = self.ms.sample_offset(sample);
        let (base, z) = self.layer(layer);
        base + self.plane.offset(((x << mx) + sx) * self.bytes, (y << my) + sy, z)
    }

    /// The part of a rectangle of samples that has memory: whole GOBs of block-linear
    /// surfaces count, so that tiles at the edges are copied by GOBs.
    fn clip(&self, x: u32, y: u32, w: u32, h: u32) -> Rect {
        let (pitch, rows) = if self.plane.block.is_some() {
            (self.plane.pitch, self.plane.rows)
        } else {
            (self.width * self.bytes, self.height)
        };
        let (xb, wb) = (x * self.bytes, w * self.bytes);
        Rect { x: xb, y, w: wb.min(pitch.saturating_sub(xb)), h: h.min(rows.saturating_sub(y)) }
    }
}

/// `w` by `h` pixels of a surface, with all their samples, in host memory: rows of samples,
/// as they lie in the surface.
#[derive(Clone, Debug, Default)]
pub struct Tile {
    /// First pixel.
    pub x: u32,
    pub y: u32,
    pub w: u32,
    pub h: u32,
    pub bytes: u32,
    pub ms: MsMode,
    /// Bytes from one row of samples to the next.
    pub stride: usize,
    pub data: Vec<u8>,
}

impl Tile {
    /// An empty tile for pixels of the surface's kind.
    pub fn new(s: &Surface, x: u32, y: u32, w: u32, h: u32) -> Self {
        let mut t = Self::default();
        t.reset(s, x, y, w, h);
        t
    }

    /// Make it a tile at another place, perhaps of another surface. Its contents are then
    /// what they were, or zeros: load it, or write every sample.
    pub fn reset(&mut self, s: &Surface, x: u32, y: u32, w: u32, h: u32) {
        let (mx, my) = s.ms.shift();
        self.stride = ((w << mx) * s.bytes) as usize;
        let len = self.stride * ((h << my) as usize);
        self.data.resize(len, 0);
        (self.x, self.y, self.w, self.h) = (x, y, w, h);
        self.bytes = s.bytes;
        self.ms = s.ms;
    }

    fn rect(&self, s: &Surface) -> Rect {
        let (mx, my) = self.ms.shift();
        s.clip(self.x << mx, self.y << my, self.w << mx, self.h << my)
    }

    /// Copy from the surface. Pixels outside it keep their contents.
    pub fn load<M: Memory + ?Sized>(&mut self, s: &Surface, mem: &M, layer: u32) {
        let (base, z) = s.layer(layer);
        let r = self.rect(s);
        s.plane.read_rect(mem, base, r, z, &mut self.data, self.stride);
    }

    /// Copy to the surface.
    pub fn store<M: Memory + ?Sized>(&self, s: &Surface, mem: &mut M, layer: u32) {
        let (base, z) = s.layer(layer);
        s.plane.write_rect(mem, base, self.rect(s), z, &self.data, self.stride);
    }

    /// Copy `w` by `h` pixels at (`x`, `y`) of the tile to the surface.
    #[allow(clippy::too_many_arguments)]
    pub fn store_part<M: Memory + ?Sized>(
        &self,
        s: &Surface,
        mem: &mut M,
        layer: u32,
        x: u32,
        y: u32,
        w: u32,
        h: u32,
    ) {
        let (base, z) = s.layer(layer);
        let (mx, my) = self.ms.shift();
        let (w, h) = (w.min(self.w.saturating_sub(x)), h.min(self.h.saturating_sub(y)));
        let r = s.clip((self.x + x) << mx, (self.y + y) << my, w << mx, h << my);
        let o = self.offset(x.min(self.w.saturating_sub(1)), y.min(self.h.saturating_sub(1)));
        s.plane.write_rect(mem, base, r, z, &self.data[o..], self.stride);
    }

    /// Offset in `data` of a pixel's first stored sample (column 0, row 0 of its samples).
    #[inline(always)]
    fn offset(&self, x: u32, y: u32) -> usize {
        let (mx, my) = self.ms.shift();
        ((y << my) as usize) * self.stride + (((x << mx) * self.bytes) as usize)
    }

    /// Offset in `data` of sample `s` of pixel (`x`, `y`) of the tile.
    #[inline(always)]
    pub fn sample_offset(&self, x: u32, y: u32, s: u32) -> usize {
        let (sx, sy) = self.ms.sample_offset(s);
        self.offset(x, y) + sy as usize * self.stride + (sx * self.bytes) as usize
    }

    /// The bytes of a sample.
    #[inline(always)]
    pub fn sample(&self, x: u32, y: u32, s: u32) -> &[u8] {
        let o = self.sample_offset(x, y, s);
        &self.data[o..o + self.bytes as usize]
    }

    #[inline(always)]
    pub fn sample_mut(&mut self, x: u32, y: u32, s: u32) -> &mut [u8] {
        let o = self.sample_offset(x, y, s);
        let n = self.bytes as usize;
        &mut self.data[o..o + n]
    }

    /// A row of samples (`row` counts rows of samples).
    pub fn row(&self, row: u32) -> &[u8] {
        let o = row as usize * self.stride;
        &self.data[o..o + self.stride]
    }

    pub fn row_mut(&mut self, row: u32) -> &mut [u8] {
        let o = row as usize * self.stride;
        let n = self.stride;
        &mut self.data[o..o + n]
    }

    /// Set every sample to the first `bytes` of `value` (a clear).
    pub fn fill(&mut self, value: &[u8]) {
        let v = &value[..self.bytes as usize];
        for s in self.data.chunks_exact_mut(v.len().max(1)) {
            s.copy_from_slice(v);
        }
    }
}
