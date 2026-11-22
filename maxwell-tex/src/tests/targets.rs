use super::{Ram, Rng};
use crate::layout::MsMode;
use crate::num::{bits, f32s, Num};
use crate::rt::{Class, RtFormat, ZetaFormat};
use crate::surface::{Surface, Tile};
use crate::Memory;

fn all() -> impl Iterator<Item = RtFormat> {
    (0..=255).filter_map(RtFormat::from_code)
}

#[test]
fn every_format_round_trips() {
    let mut rng = Rng(99);
    assert_eq!(all().count(), 60 + 8);
    for f in all() {
        let n = f.bytes as usize;
        assert!(n.is_power_of_two());
        for _ in 0..300 {
            let b = ((rng.next() as u128) << 64 | rng.next() as u128).to_le_bytes();
            let v = f.decode(&b[..n]);
            if f.class == Class::Float && f32s(v).iter().any(|v| v.is_nan()) {
                continue;
            }
            let e = f.encode(&v);
            assert_eq!(e[n..], [0; 16][n..]);
            // What was decoded is what was stored, and encodes to the same again.
            assert_eq!(f.decode(&e[..n]), v, "{:#x}", f.code);
            assert_eq!(f.encode(&f.decode(&e[..n])), e);
            // Stored channels come back bit by bit (the lowest SNORM value has two codes, a
            // 32-bit UNORM does not fit a f32).
            let (w, g) = (crate::num::words(&b[..n]), crate::num::words(&e[..n]));
            for c in f.packed.ch.iter().filter(|c| c.bits != 0) {
                if c.num != Num::Snorm && f.code != 0xff {
                    assert_eq!(c.raw(&w), c.raw(&g), "{:#x}", f.code);
                }
            }
        }
        // Missing channels.
        let v = f.decode(&[0xff; 16][..n]);
        for (i, v) in v.iter().enumerate() {
            if f.packed.ch[i].bits == 0 {
                let one = if f.class == Class::Float { 1f32.to_bits() } else { 1 };
                assert_eq!(*v, if i == 3 { one } else { 0 });
            }
        }
    }
}

fn enc(code: u8, v: [f32; 4]) -> u128 {
    u128::from_le_bytes(RtFormat::from_code(code).expect("format").encode_f32(v))
}

#[test]
fn encodings() {
    let c = [1.0, 0.5, 0.0, 0.25];
    assert_eq!(enc(0xd5, c), 0x40_00_80_ff); // RGBA8
    assert_eq!(enc(0xcf, c), 0x40_ff_80_00); // BGRA8
    assert_eq!(enc(0xe6, c), 0xff_ff_80_00); // BGRX8
    assert_eq!(enc(0xf9, c), 0xff_00_80_ff);
    assert_eq!(enc(0xfd, c), 0x00_ff_80_00);
    assert_eq!(enc(0xd6, c), 0x40_00_bc_ff); // sRGB, alpha linear
    assert_eq!(enc(0xd0, c), 0x40_ff_bc_00);
    assert_eq!(enc(0xe8, c), 31 << 11 | 32 << 5);
    assert_eq!(enc(0xe9, c), 31 << 10 | 16 << 5);
    assert_eq!(enc(0xd1, c), 1 << 30 | 512 << 10 | 1023);
    assert_eq!(enc(0xdf, c), 1 << 30 | 1023 << 20 | 512 << 10);
    assert_eq!(enc(0xe0, c), 0x380 << 11 | 0x3c0);
    assert_eq!(enc(0xca, c), 0x3400_0000_3800_3c00);
    assert_eq!(enc(0xc0, c), 0x3e80_0000_0000_0000_3f00_0000_3f80_0000);
    assert_eq!(enc(0xf7, c), 0x40);
    assert_eq!(enc(0xea, c), 0x80ff);
    assert_eq!(enc(0xd7, [1.0, -1.0, 0.5, -9.0]), 0x81_40_81_7f);
    // Saturation and NaN.
    assert_eq!(enc(0xd5, [f32::NAN, f32::INFINITY, -1.0, 2.0]), 0xff_00_ff_00);
    assert_eq!(enc(0xc6, [f32::NAN, 2.0, -1.0, 1.0]), 0xffff_0000_ffff_0000);
    // Integers saturate.
    let f = RtFormat::from_code(0xd9).expect("format");
    assert_eq!(f.class, Class::Uint);
    assert_eq!(f.encode(&[1, 255, 256, !0])[..4], [1, 255, 255, 255]);
    let f = RtFormat::from_code(0xd8).expect("format");
    assert_eq!(f.class, Class::Sint);
    assert_eq!(f.encode(&[-1i32 as u32, 127, 128, -500i32 as u32])[..4], [255, 127, 127, 128]);
    // sRGB writes disabled.
    let f = RtFormat::from_code(0xd6).expect("format").linear();
    assert_eq!(f.encode_f32(c)[..4], [255, 128, 0, 64]);
    assert!(RtFormat::from_code(0).is_none() && RtFormat::from_code(0xd3).is_none());
}

#[test]
fn write_masks() {
    let m = |code, mask| u128::from_le_bytes(RtFormat::from_code(code).expect("format").write_mask(mask));
    assert_eq!(m(0xcf, [true, false, false, false]), 0x00ff_0000);
    assert_eq!(m(0xcf, [true; 4]), 0xffff_ffff);
    assert_eq!(m(0xe8, [false, true, false, true]), 0x07e0);
    assert_eq!(m(0xca, [false, false, false, true]), 0xffff_0000_0000_0000);
    assert_eq!(m(0xc0, [false, true, true, true]), !0xffff_ffff);
    assert_eq!(m(0xd5, [false; 4]), 0);
}

#[test]
fn zeta() {
    let z = |code| ZetaFormat::from_code(code).expect("format");
    assert_eq!(z(0x16).encode(1.0, 0xab), 0xab_ffffff);
    assert_eq!(z(0x14).encode(1.0, 0xab), 0xffff_ffab);
    assert_eq!(z(0x15).encode(0.5, 0xab), 0x80_0000);
    assert_eq!(z(0x13).encode(0.5, 1), 0x8000);
    assert_eq!(z(0x0a).encode(-0.5, 1), 0xbf00_0000);
    assert_eq!(z(0x19).encode(0.5, 0xab), 0xab_3f00_0000);
    assert_eq!(z(0x17).encode(0.5, 0xab), 0xab);
    let mut rng = Rng(5);
    for code in [0x0a, 0x13, 0x14, 0x15, 0x16, 0x17, 0x18, 0x19, 0x1d, 0x1e, 0x1f] {
        let f = z(code);
        for _ in 0..2000 {
            let d = f.quantize(rng.below(1 << 24) as f32 / 16777215.0);
            assert_eq!(f.quantize(f.to_f32(d)), d);
            let s = rng.next() as u8;
            let v = f.with_stencil(f.with_depth(0, d), s, 0xff);
            assert_eq!(f.depth(v), if f.has_depth() { d } else { 0 });
            assert_eq!(f.stencil(v), if f.has_stencil() { s } else { 0 });
            // Each part leaves the other alone.
            assert_eq!(f.depth(f.with_stencil(v, !s, 0x0f)), f.depth(v));
            if f.has_stencil() {
                assert_eq!(f.stencil(f.with_stencil(v, !s, 0x0f)), s ^ 0x0f);
                assert_eq!(f.stencil(f.with_depth(v, 0)), s);
            }
            let mut b = [0; 8];
            f.store(v, &mut b);
            assert_eq!(f.load(&b), v);
            assert_eq!(f.decode(&b).1, f.stencil(v));
        }
    }
    // All 24-bit depths survive the way through f32.
    let f = z(0x16);
    for d in (0..1 << 24).step_by(13).chain((1 << 24) - 5000..1 << 24) {
        assert_eq!(f.quantize(f.to_f32(d)), d);
    }
    assert!(f.compare(5, 6) == Some(std::cmp::Ordering::Less));
    assert!(z(0x0a).compare((-1f32).to_bits(), 0) == Some(std::cmp::Ordering::Less));
    assert!(z(0x0a).compare(f32::NAN.to_bits(), 0).is_none());
}

#[test]
fn tiles() {
    let mut rng = Rng(17);
    for (w, h, mode, bytes, ms) in [
        (300, 100, 0x40, 4, MsMode::X1),
        (70, 40, 0x10, 8, MsMode::X1),
        (200, 80, 0x30, 4, MsMode::X4),
        (400, 80, 0x20, 2, MsMode::X8),
        (1280, 33, 1 << 12, 4, MsMode::X1),
        (100, 50, 0x220 | 1 << 16, 4, MsMode::X1),
        (33, 9, 0x00, 1, MsMode::X1),
    ] {
        let s = Surface::new(0x4000, w, h, mode, 0x40000, bytes, ms);
        let mut ram = Ram::new(0x4000, 0x100000);
        for b in &mut ram.data {
            *b = rng.next() as u8;
        }
        let (pw, ph) = s.pixels();
        let layer = 3;
        for (x, y) in [(0, 0), (16, 16), (pw / 16 * 16, ph / 16 * 16), (0, ph / 16 * 16)] {
            let mut t = Tile::new(&s, x, y, 16, 16);
            t.load(&s, &ram, layer);
            let before = ram.data.clone();
            for py in 0..16.min(ph.saturating_sub(y)) {
                for px in 0..16.min(pw.saturating_sub(x)) {
                    for i in 0..ms.samples() {
                        let at = (s.sample_address(x + px, y + py, layer, i) - 0x4000) as usize;
                        assert_eq!(t.sample(px, py, i), &ram.data[at..at + bytes as usize]);
                    }
                }
            }
            // Unchanged contents store back as they were.
            t.store(&s, &mut ram, layer);
            assert!(ram.data == before);
            // One sample.
            if pw > x + 5 && ph > y + 3 {
                let i = ms.samples() - 1;
                t.sample_mut(5, 3, i).fill(0xee);
                t.store_part(&s, &mut ram, layer, 4, 2, 4, 4);
                let at = (s.sample_address(x + 5, y + 3, layer, i) - 0x4000) as usize;
                let mut want = before;
                want[at..at + bytes as usize].fill(0xee);
                assert!(ram.data == want);
            }
        }
    }
}

#[test]
fn layers_and_clears() {
    // An array layer and a slice of a 3D surface.
    let s = Surface::new(0, 64, 64, 0x30, 0x10000, 4, MsMode::X1);
    assert_eq!(s.sample_address(0, 0, 2, 0), 0x20000);
    let s = Surface::new(0, 64, 64, 0x220 | 1 << 16, 0, 4, MsMode::X1);
    // Blocks of 4 by 4 GOBs; slice 1 is 4 GOBs on, slice 4 in the next layer of blocks.
    assert_eq!(s.sample_address(0, 0, 1, 0), 4 * 512);
    assert_eq!(s.sample_address(0, 0, 4, 0), 4 * 2 * 16 * 512);
    let s = Surface::new(0, 64, 64, 0x30, 0, 4, MsMode::X4);
    let mut t = Tile::new(&s, 0, 0, 8, 8);
    t.fill(&RtFormat::from_code(0xd5).expect("format").encode_f32([1.0, 0.0, 0.0, 1.0]));
    assert_eq!(t.data.len(), 8 * 8 * 4 * 4);
    assert_eq!(t.sample(7, 7, 3), [255, 0, 0, 255]);
    assert_eq!(t.row(15).len(), 64);
    let mut ram = Ram::new(0, 0x10000);
    t.store(&s, &mut ram, 0);
    let mut b = [0; 4];
    ram.read(s.sample_address(3, 4, 0, 2), &mut b);
    assert_eq!(b, [255, 0, 0, 255]);
    // Sample 2 of 4 is below sample 0.
    assert_eq!(s.sample_address(3, 4, 0, 2), s.plane.offset(6 * 4, 9, 0));
    assert_eq!(bits([0.0; 4]), [0; 4]);
}
