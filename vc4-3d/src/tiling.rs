//! VideoCore IV image layouts (as Mesa's `vc4_tiling.c` and the kernel's validator describe them):
//!
//! * raster (linear): rows of `stride` bytes;
//! * LT ("linear tile"): 64-byte micro-tiles (utiles: 4x4 pixels at 32 bits, 8x4 at 16, 8x8 at
//!   8, 2x4 at 64) in raster order, pixels in raster order within each;
//! * T: 4 KiB tiles of 2x2 1 KiB sub-tiles of 4x4 utiles; tile rows alternate direction (odd
//!   rows run right to left) and the sub-tile order within a tile depends on the row's parity.
//!
//! The TMU picks LT for a mip level when it is at most 4 utiles wide or high, T otherwise.

pub const RASTER: u8 = 0;
pub const T: u8 = 1;
pub const LT: u8 = 2;

pub fn utile_w(cpp: u32) -> u32 { match cpp { 1 | 2 => 8, 4 => 4, _ => 2 } }
pub fn utile_h(cpp: u32) -> u32 { if cpp == 1 { 8 } else { 4 } }

/// Whether a level of this size is stored LT rather than T.
pub fn size_is_lt(w: u32, h: u32, cpp: u32) -> bool { w <= 4 * utile_w(cpp) || h <= 4 * utile_h(cpp) }

/// Padded width and height of an image in `tiling`.
pub fn aligned(w: u32, h: u32, cpp: u32, tiling: u8) -> (u32, u32) {
    let (uw, uh) = (utile_w(cpp), utile_h(cpp));
    match tiling {
        T => (w.div_ceil(uw * 8) * uw * 8, h.div_ceil(uh * 8) * uh * 8),
        LT => (w.div_ceil(uw) * uw, h.div_ceil(uh) * uh),
        _ => (w.div_ceil(uw) * uw, h),
    }
}

/// Byte size of an image in `tiling`.
pub fn size(w: u32, h: u32, cpp: u32, tiling: u8) -> u32 { let (aw, ah) = aligned(w, h, cpp, tiling); aw * ah * cpp }

/// A tiled image's geometry, for addressing its pixels.
#[derive(Clone, Copy, Debug)]
pub struct Layout {
    pub tiling: u8,
    pub cpp: u32,
    /// padded width in pixels (raster: the row stride in pixels)
    pub width: u32,
    /// raster: bytes per row
    pub stride: u32,
    /// log2 of the bytes per pixel and of the utile's width and height (all powers of two)
    cpp_s: u32,
    uw_s: u32,
    uh_s: u32,
    /// utiles per row (LT), 4 KiB tiles per row (T)
    utiles_per_row: u32,
    tiles_per_row: u32,
}

impl Layout {
    /// The layout of a `w` pixels wide image (the stride and padding the hardware derives).
    pub fn new(tiling: u8, cpp: u32, w: u32) -> Layout {
        let (aw, _) = aligned(w, 1, cpp, tiling);
        let (uw, uh) = (utile_w(cpp), utile_h(cpp));
        Layout { tiling, cpp, width: aw, stride: aw * cpp, cpp_s: cpp.trailing_zeros(), uw_s: uw.trailing_zeros(), uh_s: uh.trailing_zeros(),
                 utiles_per_row: aw / uw, tiles_per_row: (aw / (uw * 8)).max(1) }
    }

    /// Byte offset of pixel (x, y).
    #[inline]
    pub fn offset(&self, x: u32, y: u32) -> u32 {
        if self.tiling == RASTER { return y * self.stride + x * self.cpp; }
        let (uw_s, uh_s) = (self.uw_s, self.uh_s);
        let inside = ((y & ((1 << uh_s) - 1)) << uw_s | (x & ((1 << uw_s) - 1))) << self.cpp_s;
        let (ux, uy) = (x >> uw_s, y >> uh_s);
        if self.tiling == LT {
            return (uy * self.utiles_per_row + ux) * 64 + inside;
        }
        let tiles_per_row = self.tiles_per_row;
        let (tx, ty) = (ux >> 3, uy >> 3);
        let odd = ty & 1 != 0;
        let tx = if odd { tiles_per_row.saturating_sub(tx + 1) } else { tx };
        let tile = 4096 * (ty * tiles_per_row + tx);
        let si = (((uy >> 2) & 1) << 1) | ((ux >> 2) & 1);
        // sub-tile order by row parity, two bits each: even 0 3 1 2, odd 2 1 3 0
        const EVEN: u32 = 0b10_01_11_00;
        const ODD: u32 = 0b00_11_01_10;
        let sub = 1024 * ((if odd { ODD } else { EVEN }) >> (2 * si) & 3);
        tile + sub + ((uy & 3) * 4 + (ux & 3)) * 64 + inside
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Mesa's `t_utile_address` (vc4_tiling.c), transcribed, for subtile-aligned utiles.
    fn mesa_t_utile(utile_x: u32, utile_y: u32, utile_stride: u32) -> u32 {
        let tile_stride = utile_stride >> 3;
        let (mut tile_x, tile_y) = (utile_x >> 3, utile_y >> 3);
        let odd = tile_y & 1 != 0;
        if odd { tile_x = tile_stride - tile_x - 1; }
        let tile_offset = 4096 * (tile_y * tile_stride + tile_x);
        let stile_index = (((utile_y >> 2) & 1) << 1) + ((utile_x >> 2) & 1);
        let odd_map = [2, 1, 3, 0];
        let even_map = [0, 3, 1, 2];
        tile_offset + 1024 * if odd { odd_map[stile_index as usize] } else { even_map[stile_index as usize] }
    }

    #[test]
    fn t_format_matches_mesa() {
        for cpp in [1u32, 2, 4, 8] {
            let (uw, uh) = (utile_w(cpp), utile_h(cpp));
            let w = uw * 8 * 5; // 5 4K tiles wide
            let l = Layout::new(T, cpp, w);
            for y in (0..uh * 8 * 4).step_by(uh as usize * 4) {
                for x in (0..w).step_by(uw as usize * 4) {
                    let want = mesa_t_utile(x / uw, y / uh, w / uw);
                    assert_eq!(l.offset(x, y), want, "cpp {cpp} at {x},{y}");
                }
            }
        }
    }

    #[test]
    fn layouts_are_bijective() {
        for tiling in [RASTER, T, LT] {
            for cpp in [1u32, 2, 4, 8] {
                let (w, h) = aligned(70, 37, cpp, tiling);
                let l = Layout::new(tiling, cpp, w);
                let mut seen = vec![false; (w * h) as usize];
                for y in 0..h {
                    for x in 0..w {
                        let o = l.offset(x, y);
                        assert_eq!(o % cpp, 0);
                        let k = (o / cpp) as usize;
                        assert!(k < seen.len(), "{tiling} cpp {cpp} ({x},{y}) -> {o} past {}", w * h * cpp);
                        assert!(!seen[k], "{tiling} cpp {cpp} ({x},{y}) collides");
                        seen[k] = true;
                    }
                }
            }
        }
    }

    #[test]
    fn lt_is_utiles_in_raster_order() {
        let l = Layout::new(LT, 4, 16);
        assert_eq!(l.offset(0, 0), 0);
        assert_eq!(l.offset(1, 0), 4);
        assert_eq!(l.offset(0, 1), 16);
        assert_eq!(l.offset(4, 0), 64);
        assert_eq!(l.offset(0, 4), 4 * 64);
        assert!(size_is_lt(16, 64, 4));
        assert!(!size_is_lt(32, 32, 4));
    }
}
