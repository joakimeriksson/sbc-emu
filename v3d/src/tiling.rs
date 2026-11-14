//! V3D memory layouts (Mesa `common/v3d_tiling.c`): raster, linear-tile (LT), UB-linear
//! (1 or 2 UIF blocks wide), and UIF (columns of 4 UIF blocks, optionally XOR-ed every
//! other column). Also the mipmap chain layout the TMU derives from a texture's level 0
//! (Mesa `v3d_setup_slices`): levels are stored smallest first, level 0 last.

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Tiling { Raster, Lt, UbLinear1, UbLinear2, UifNoXor, UifXor }

impl Tiling {
    pub fn from_memory_format(m: u32) -> Tiling {
        match m { 0 => Tiling::Raster, 1 => Tiling::Lt, 2 => Tiling::UbLinear1, 3 => Tiling::UbLinear2, 4 => Tiling::UifNoXor, _ => Tiling::UifXor }
    }
}

pub fn utile_w(cpp: u32) -> u32 { match cpp { 1 | 2 => 8, 4 | 8 => 4, _ => 2 } }
pub fn utile_h(cpp: u32) -> u32 { match cpp { 1 => 8, 2 | 4 => 4, _ => 2 } }

#[inline]
fn utile_off(cpp: u32, x: u32, y: u32) -> u32 { x * cpp + y * utile_w(cpp) * cpp }

/// Byte offset of pixel (x, y) in an image with the given layout. `stride` is the row pitch
/// in bytes for raster images; `image_h` the padded height in pixels for UIF.
#[inline]
pub fn offset(t: Tiling, cpp: u32, stride: u32, image_h: u32, x: u32, y: u32) -> u32 {
    let (uw, uh) = (utile_w(cpp), utile_h(cpp));
    match t {
        Tiling::Raster => y * stride + x * cpp,
        Tiling::Lt => {
            // utiles laid out row by row, `stride` bytes per utile row
            let (ux, uy) = (x / uw, y / uh);
            let utiles_per_row = (stride / (uw * cpp)).max(1);
            64 * (uy * utiles_per_row + ux) + utile_off(cpp, x & (uw - 1), y & (uh - 1))
        }
        Tiling::UbLinear1 | Tiling::UbLinear2 => {
            let n = if t == Tiling::UbLinear1 { 1 } else { 2 };
            let (ub_x, ub_y) = (x / (uw * 2), y / (uh * 2));
            256 * (ub_y * n + ub_x) + if x & uw != 0 { 64 } else { 0 } + if y & uh != 0 { 128 } else { 0 }
                + utile_off(cpp, x & (uw - 1), y & (uh - 1))
        }
        Tiling::UifNoXor | Tiling::UifXor => {
            let (mbw, mbh) = (uw * 2, uh * 2);
            let (mb_x, mut mb_y) = (x / mbw, y / mbh);
            let (px, py) = (x % mbw, y % mbh);
            if t == Tiling::UifXor && (mb_x / 4) & 1 != 0 { mb_y ^= 0x10; }
            let mb_h = (image_h + mbh - 1) / mbh;
            let mb_id = (mb_x / 4) * (mb_h * 4) + (mb_x % 4) + mb_y * 4;
            mb_id * 256 + if py >= uh { 128 } else { 0 } + if px >= uw { 64 } else { 0 } + utile_off(cpp, px & (uw - 1), py & (uh - 1))
        }
    }
}

/// One mip level of a texture as the TMU sees it.
#[derive(Clone, Copy, Debug)]
pub struct Level {
    /// byte offset from level 0's address (<= 0: smaller levels come first in memory)
    pub offset: i64,
    pub tiling: Tiling,
    /// width/height in blocks (pixels for uncompressed formats), after padding
    pub pw: u32,
    pub ph: u32,
    /// real size in pixels
    pub w: u32,
    pub h: u32,
    pub d: u32,
    pub stride: u32,
    pub size: u32,
}

const PAGE: u32 = 4096;
const PAGE_CACHE: u32 = PAGE * 8;
const UIFBLOCK_ROW: u32 = 4 * 4 * 64;
const PAGE_UB_ROWS: u32 = PAGE / UIFBLOCK_ROW;
const PAGE_UB_ROWS_1_5: u32 = (PAGE_UB_ROWS * 3) >> 1;
const PAGE_CACHE_UB_ROWS: u32 = PAGE_CACHE / UIFBLOCK_ROW;
const PAGE_CACHE_MINUS_1_5_UB_ROWS: u32 = PAGE_CACHE_UB_ROWS - PAGE_UB_ROWS_1_5;

fn ub_pad(cpp: u32, height: u32) -> u32 {
    let height_ub = height / (utile_h(cpp) * 2);
    let o = height_ub % PAGE_CACHE_UB_ROWS;
    if o == 0 { return 0; }
    if o < PAGE_UB_ROWS_1_5 { return if height_ub < PAGE_CACHE_UB_ROWS { 0 } else { PAGE_UB_ROWS_1_5 - o }; }
    if o > PAGE_CACHE_MINUS_1_5_UB_ROWS { return PAGE_CACHE_UB_ROWS - o; }
    0
}

fn minify(x: u32, l: u32) -> u32 { (x >> l).max(1) }
fn mpad(dim: u32, block: u32) -> u32 { block * ((minify(dim, 1) + block - 1) / block).next_power_of_two() }

/// The mip chain of a tiled texture: `levels[l]` for l in 0..=max_level, with offsets relative
/// to level 0. `uif_top`: level 0 is UIF regardless of size ("Level 0 is strictly UIF"),
/// with `l0_xor` and `l0_ub_pad` from the texture state.
#[allow(clippy::too_many_arguments)]
pub fn mip_chain(width: u32, height: u32, depth: u32, cpp: u32, bw: u32, bh: u32, max_level: u32, is_3d: bool,
                 uif_top: bool, l0_xor: bool, l0_ub_pad: u32) -> Vec<Level> {
    let (uw, uh) = (utile_w(cpp), utile_h(cpp));
    let (ubw, ubh) = (uw * 2, uh * 2);
    let pot_w = 2 * mpad(width, bw);
    let pot_h = 2 * mpad(height, bh);
    let pot_d = 2 * mpad(depth, 1);
    let mut levels = vec![Level { offset: 0, tiling: Tiling::Lt, pw: 1, ph: 1, w: 1, h: 1, d: 1, stride: 0, size: 0 }; max_level as usize + 1];
    let mut off: i64 = 0;
    for i in (0..=max_level).rev() {
        let (lw, lh) = if i < 2 { (minify(width, i), minify(height, i)) } else { (minify(pot_w, i), minify(pot_h, i)) };
        let ld = if !is_3d { 1 } else if i < 1 { depth } else { minify(pot_d, i) };
        let mut w = (lw + bw - 1) / bw;
        let mut h = (lh + bh - 1) / bh;
        let tiling;
        if (i != 0 || !uif_top) && (w <= uw || h <= uh) {
            tiling = Tiling::Lt;
            w = (w + uw - 1) / uw * uw;
            h = (h + uh - 1) / uh * uh;
        } else if (i != 0 || !uif_top) && w <= ubw {
            tiling = Tiling::UbLinear1;
            w = (w + ubw - 1) / ubw * ubw;
            h = (h + ubh - 1) / ubh * ubh;
        } else if (i != 0 || !uif_top) && w <= 2 * ubw {
            tiling = Tiling::UbLinear2;
            w = (w + 2 * ubw - 1) / (2 * ubw) * (2 * ubw);
            h = (h + ubh - 1) / ubh * ubh;
        } else {
            w = (w + 4 * ubw - 1) / (4 * ubw) * (4 * ubw);
            h = (h + ubh - 1) / ubh * ubh;
            let pad = if i == 0 && uif_top { l0_ub_pad } else { ub_pad(cpp, h) };
            h += pad * ubh;
            let xor = if i == 0 && uif_top { l0_xor } else { (h / ubh) % (PAGE_CACHE / UIFBLOCK_ROW) == 0 };
            tiling = if xor { Tiling::UifXor } else { Tiling::UifNoXor };
        }
        let stride = w * cpp;
        let size = h * stride;
        let mut total = size as i64 * ld as i64;
        if i == 1 && w > 4 * ubw && h > PAGE_CACHE_MINUS_1_5_UB_ROWS * ubh { total = (total + PAGE as i64 - 1) / PAGE as i64 * PAGE as i64; }
        levels[i as usize] = Level { offset: off, tiling, pw: w, ph: h, w: minify(width, i), h: minify(height, i), d: ld, stride, size };
        off += total;
    }
    let l0 = levels[0].offset;
    for l in levels.iter_mut() { l.offset -= l0; }
    levels
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn uif_columns() {
        // 4-byte pixels: utiles 4x4, UIF blocks 8x8 (256 bytes), columns 4 blocks wide
        assert_eq!(offset(Tiling::UifNoXor, 4, 0, 64, 0, 0), 0);
        assert_eq!(offset(Tiling::UifNoXor, 4, 0, 64, 4, 0), 64);
        assert_eq!(offset(Tiling::UifNoXor, 4, 0, 64, 0, 4), 128);
        assert_eq!(offset(Tiling::UifNoXor, 4, 0, 64, 8, 0), 256);
        assert_eq!(offset(Tiling::UifNoXor, 4, 0, 64, 0, 8), 4 * 256);
        // next column starts after 8 block rows (64 / 8) of 4 blocks
        assert_eq!(offset(Tiling::UifNoXor, 4, 0, 64, 32, 0), 8 * 4 * 256);
    }
    #[test]
    fn mips_smallest_first() {
        let l = mip_chain(256, 256, 1, 4, 1, 1, 8, false, false, false, 0);
        assert!(l[1].offset < 0 && l[8].offset < l[1].offset);
        assert_eq!(l[0].tiling, Tiling::UifXor.min_or(l[0].tiling));
    }
    impl Tiling { fn min_or(self, o: Tiling) -> Tiling { if matches!(o, Tiling::UifXor | Tiling::UifNoXor) { o } else { self } } }
}
