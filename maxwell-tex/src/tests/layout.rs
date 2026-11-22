use super::{Ram, Rng};
use crate::layout::{gob_offset, nvc0_block, BlockDims, Chain, MsMode, Plane, Rect, Shape};
use crate::Memory;

/// Tegra's order in a GOB, as the Linux display driver writes it.
fn tegra(x: u64, y: u64) -> u64 {
    ((x & 32) << 3) | ((y & 6) << 5) | ((x & 16) << 1) | ((y & 1) << 4) | (x & 15)
}

/// The whole formula, written out: blocks in rows, GOBs down and then in depth.
fn reference(pitch: u64, rows: u64, b: BlockDims, x: u64, y: u64, z: u64) -> u64 {
    let (bh, bd) = (1u64 << b.height, 1u64 << b.depth);
    let block = 512 * bh * bd;
    let across = pitch / 64;
    let down = rows.div_ceil(8 * bh);
    let (bx, by, bz) = (x / 64, y / (8 * bh), z / bd);
    let gob = (z % bd) * bh + (y % (8 * bh)) / 8;
    ((bz * down + by) * across + bx) * block + gob * 512 + tegra(x % 64, y % 8)
}

#[test]
fn gob_order() {
    let mut seen = [false; 512];
    for y in 0..8 {
        for x in 0..64 {
            let o = gob_offset(x, y);
            assert_eq!(o as u64, tegra(x as u64, y as u64));
            assert!(!std::mem::replace(&mut seen[o as usize], true));
        }
    }
    // Pieces of 16 bytes in memory order, as Mesa's nil lists them: (x, y).
    let half = [(0, 0), (0, 1), (16, 0), (16, 1), (0, 2), (0, 3), (16, 2), (16, 3)];
    for i in 0..32u32 {
        let (x, y) = half[(i & 7) as usize];
        assert_eq!(gob_offset(x + (i >> 4) * 32, y + (i >> 3 & 1) * 4), i * 16);
    }
    assert_eq!(gob_offset(16, 0), 0x20);
    assert_eq!(gob_offset(0, 2), 0x40);
    assert_eq!(gob_offset(0, 4), 0x80);
    assert_eq!(gob_offset(48, 7), 0x1f0);
}

#[test]
fn planes_match_the_formula_and_are_bijective() {
    for (w, h, d, b) in [
        (200u32, 70u32, 1u32, BlockDims::new(4, 0)),
        (64, 8, 1, BlockDims::new(0, 0)),
        (130, 33, 9, BlockDims::new(2, 3)),
        (64, 16, 40, BlockDims::new(1, 5)),
    ] {
        let p = Plane::block_linear(w, h, b);
        let d_all = d.next_multiple_of(1u32 << b.depth);
        let size = p.size(d) as usize;
        assert_eq!(size as u64, p.pitch as u64 * p.rows as u64 * d_all as u64);
        let mut seen = vec![false; size];
        for z in 0..d_all {
            for y in 0..p.rows {
                for x in 0..p.pitch {
                    let o = p.offset(x, y, z);
                    assert_eq!(o, reference(p.pitch as u64, p.rows as u64, b, x as u64, y as u64, z as u64));
                    assert!(!std::mem::replace(&mut seen[o as usize], true));
                }
            }
        }
        assert!(seen.iter().all(|&s| s));
        // nil: a slice is (slice in block) * 2D block size + (block slice) * slices of blocks.
        for z in 0..d {
            let in_block = (z & ((1 << b.depth) - 1)) as u64 * (512 << b.height);
            let blocks = (p.pitch as u64 / 64) * (p.rows as u64 / b.rows() as u64) * (z >> b.depth) as u64;
            assert_eq!(p.slice_offset(z), in_block + blocks * b.size() as u64);
        }
    }
}

#[test]
fn rectangles() {
    let mut rng = Rng(7);
    for plane in [Plane::block_linear(300, 60, BlockDims::new(2, 1)), Plane::pitch(320)] {
        let mut ram = Ram::new(0x1000, 320 * 64 * 2);
        for b in &mut ram.data {
            *b = rng.next() as u8;
        }
        for _ in 0..200 {
            let (x, y) = (rng.below(250), rng.below(50));
            let rect = Rect { x, y, w: 1 + rng.below(300 - x), h: 1 + rng.below(60 - y) };
            let z = if plane.block.is_some() { rng.below(2) } else { 0 };
            let stride = rect.w as usize + 3;
            let mut out = vec![0; stride * rect.h as usize];
            plane.read_rect(&ram, 0x1000, rect, z, &mut out, stride);
            for r in 0..rect.h {
                for c in 0..rect.w {
                    let at = plane.offset(x + c, y + r, z) as usize;
                    assert_eq!(out[r as usize * stride + c as usize], ram.data[at]);
                }
            }
            // Write something else there: exactly the rectangle changes.
            let data: Vec<u8> = out.iter().map(|b| b ^ 0x5a).collect();
            let mut want = ram.data.clone();
            for r in 0..rect.h {
                for c in 0..rect.w {
                    want[plane.offset(x + c, y + r, z) as usize] = data[r as usize * stride + c as usize];
                }
            }
            plane.write_rect(&mut ram, 0x1000, rect, z, &data, stride);
            assert!(ram.data == want);
        }
    }
}

/// Counts calls: a whole GOB has to be one access.
struct Counting(std::cell::Cell<usize>, usize);

impl Memory for Counting {
    fn read(&self, _: u64, out: &mut [u8]) {
        self.0.set(self.0.get() + 1);
        out.fill(0);
    }

    fn write(&mut self, _: u64, _: &[u8]) {
        self.1 += 1;
    }
}

#[test]
fn whole_gobs_are_one_access() {
    let p = Plane::block_linear(512, 64, BlockDims::new(2, 0));
    let mut m = Counting(Default::default(), 0);
    let mut d = vec![0; 128 * 32];
    let r = Rect { x: 64, y: 32, w: 128, h: 32 };
    p.read_rect(&m, 0, r, 0, &mut d, 128);
    p.write_rect(&mut m, 0, r, 0, &d, 128);
    assert_eq!((m.0.get(), m.1), (8, 8));
    // Half a GOB (one side) is one run, its other half and the rest of the rows more.
    m.1 = 0;
    p.write_rect(&mut m, 0, Rect { x: 0, y: 0, w: 32, h: 8 }, 0, &d, 128);
    assert_eq!(m.1, 1);
}

#[test]
fn nvc0_blocks() {
    // (rows, block height): the driver's thresholds.
    for (rows, h) in [(1, 0), (8, 0), (9, 1), (16, 1), (17, 2), (32, 2), (33, 3), (64, 3), (65, 4), (5000, 4)] {
        assert_eq!(nvc0_block(rows, 1, false), BlockDims::new(h, 0));
    }
    assert_eq!(nvc0_block(1080, 1, false).tile_mode(), 0x40);
    assert_eq!(nvc0_block(100, 1, true).tile_mode(), 0x20);
    assert_eq!(nvc0_block(100, 64, true).tile_mode(), 0x420);
    assert_eq!(nvc0_block(16, 64, true).tile_mode(), 0x510);
    assert_eq!(nvc0_block(16, 16, true).tile_mode(), 0x410);
    for (d, z) in [(1, 0), (2, 1), (3, 2), (4, 2), (5, 3), (8, 3), (9, 4)] {
        assert_eq!(nvc0_block(64, d, true).depth, z);
    }
    assert_eq!(BlockDims::from_tile_mode(0x420), BlockDims::new(2, 4));
}

fn shape(width: u32, height: u32, depth: u32, is_3d: bool, levels: u32, bytes: u32) -> Shape {
    Shape { width, height, depth, is_3d, levels, block_w: 1, block_h: 1, block_bytes: bytes, ms: MsMode::X1 }
}

#[test]
fn chains_by_hand() {
    // 256x256 RGBA8: level 0 is 1024 bytes by 256 rows in blocks of 16 GOBs, level 1 a
    // quarter; 64x64 and 32x32 have smaller blocks.
    let c = Chain::nvc0(&shape(256, 256, 1, false, 9, 4));
    let got: Vec<_> =
        c.levels.iter().map(|m| (m.offset, m.plane.pitch, m.plane.block.unwrap_or_default().height)).collect();
    assert_eq!(
        got,
        [
            (0, 1024, 4),
            (0x40000, 512, 4),
            (0x50000, 256, 3),
            (0x54000, 128, 2),
            (0x55000, 64, 1),
            (0x55400, 64, 0),
            (0x55600, 64, 0),
            (0x55800, 64, 0),
            (0x55a00, 64, 0),
        ]
    );
    // Layers are aligned to the first level's block, 8 KiB.
    assert_eq!(c.layer_stride, 0x56000);
    // One level of a layer alone is not padded further.
    assert_eq!(Chain::nvc0(&shape(64, 64, 1, false, 1, 4)).layer_stride, 64 * 4 * 64);
    // BC1 counts in blocks of texels.
    let bc1 = Shape { block_w: 4, block_h: 4, block_bytes: 8, ..shape(100, 100, 1, false, 3, 8) };
    let c = Chain::nvc0(&bc1);
    assert_eq!(c.levels[0].plane.pitch, 256); // 25 blocks, 200 bytes
    assert_eq!(c.levels[0].plane.rows, 32);
    assert_eq!(c.levels[1].plane.pitch, 128);
    // 4 samples: twice as wide and high.
    let ms = Shape { ms: MsMode::X4, ..shape(100, 20, 1, false, 1, 4) };
    let m = Chain::nvc0(&ms).levels[0];
    assert_eq!((m.width, m.height, m.plane.pitch, m.plane.rows), (200, 40, 832, 64));
}

#[test]
fn the_texture_unit_finds_the_drivers_levels() {
    // The level blocks derived from the first one are the driver's, for all sizes.
    for w in [1u32, 5, 64, 300] {
        for h in (1u32..300).step_by(7).chain([16, 32, 64, 128, 1024]) {
            for d in [1u32, 2, 3, 6, 9, 16, 17, 33, 64, 100] {
                for is_3d in [false, true] {
                    let levels = 32 - w.max(h).max(if is_3d { d } else { 1 }).leading_zeros();
                    let s = shape(w, h, d, is_3d, levels, 4);
                    let drv = Chain::nvc0(&s);
                    let hw = Chain::from_block(&s, drv.levels[0].plane.block.unwrap_or_default());
                    assert_eq!(drv.layer_stride, hw.layer_stride);
                    for (a, b) in drv.levels.iter().zip(&hw.levels) {
                        assert_eq!((a.offset, a.plane), (b.offset, b.plane), "{s:?}");
                    }
                    // By shrinking alone all 2D chains agree, and the sizes of 3D ones.
                    let first = drv.levels[0].plane.block.unwrap_or_default();
                    for m in &drv.levels {
                        let b = first.shrink(m.height, m.depth);
                        if !is_3d {
                            assert_eq!(m.plane.block, Some(b));
                        } else if m.plane.block != Some(b) {
                            // The known exception: deeper than the first level's block.
                            assert!(m.plane.block.unwrap_or_default().depth > first.depth);
                        }
                    }
                }
            }
        }
    }
}

#[test]
fn nil_tiling_clamp() {
    // NVK starts from 32x32 GOBs and clamps each level.
    let b = BlockDims::new(5, 5);
    assert_eq!(b.shrink(1080, 1), BlockDims::new(5, 0));
    assert_eq!(b.shrink(128, 1), BlockDims::new(4, 0));
    assert_eq!(b.shrink(129, 1), BlockDims::new(5, 0));
    assert_eq!(b.shrink(9, 3), BlockDims::new(1, 2));
    assert_eq!(b.shrink(1, 1), BlockDims::new(0, 0));
}

#[test]
fn samples_of_a_pixel() {
    // nvc0's table for its shaders (NVC0_CB_AUX_MS_INFO).
    let nvc0 = [(0, 0), (1, 0), (0, 1), (1, 1), (2, 0), (3, 0), (2, 1), (3, 1)];
    for (s, &o) in nvc0.iter().enumerate() {
        assert_eq!(MsMode::X8.sample_offset(s as u32), o);
        if s < 4 {
            assert_eq!(MsMode::X4.sample_offset(s as u32), o);
        }
        if s < 2 {
            assert_eq!(MsMode::X2.sample_offset(s as u32), o);
        }
    }
    for m in [MsMode::X1, MsMode::X2, MsMode::X4, MsMode::X8, MsMode::X8D3d, MsMode::X2D3d, MsMode::X16] {
        let (mx, my) = m.shift();
        assert_eq!(m.samples() as usize, m.positions().len());
        let mut seen = std::collections::HashSet::new();
        for s in 0..m.samples() {
            let (x, y) = m.sample_offset(s);
            assert!(x < 1 << mx && y < 1 << my);
            assert!(seen.insert((x, y)));
        }
    }
    // The plain and D3D orders of a mode keep a sample position in the same place.
    for (a, b) in [(MsMode::X8, MsMode::X8D3d), (MsMode::X2, MsMode::X2D3d)] {
        for i in 0..a.samples() {
            let j = (0..a.samples()).find(|&j| b.positions()[j as usize] == a.positions()[i as usize]);
            assert_eq!(j.map(|j| b.sample_offset(j)), Some(a.sample_offset(i)));
        }
    }
}
