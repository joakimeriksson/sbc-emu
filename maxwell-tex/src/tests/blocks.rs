use super::{build, Ram, Rng, View, ONE, R, G, B, SNORM};
use crate::sample::fetch;
use crate::texture::{decode_block, BlockCode, TexCache, Texture};

fn le(v: u128) -> [u8; 16] {
    v.to_le_bytes()
}

fn be(v: u64) -> [u8; 16] {
    let mut b = [0; 16];
    b[..8].copy_from_slice(&v.to_be_bytes());
    b
}

fn texel(b: [u8; 16], code: BlockCode, i: usize) -> [u8; 4] {
    decode_block(&b, code, false, false)[i].map(|v| (v * 255.0).round() as u8)
}

#[test]
fn bc1_by_hand() {
    // Red and blue end points, indices 0, 1, 2, 3 in the first row.
    let b = le(0xf800 | 0x001f << 16 | 0b11_10_01_00 << 32);
    assert_eq!(texel(b, BlockCode::Bc1, 0), [255, 0, 0, 255]);
    assert_eq!(texel(b, BlockCode::Bc1, 1), [0, 0, 255, 255]);
    assert_eq!(texel(b, BlockCode::Bc1, 2), [170, 0, 85, 255]);
    assert_eq!(texel(b, BlockCode::Bc1, 3), [85, 0, 170, 255]);
    // The other order: a half and transparent black; not in BC2 and BC3.
    let b = le(0x001f | 0xf800 << 16 | 0b11_10_01_00 << 32);
    assert_eq!(texel(b, BlockCode::Bc1, 2), [128, 0, 128, 255]);
    assert_eq!(texel(b, BlockCode::Bc1, 3), [0; 4]);
    let b = le((0x001f | 0xf800 << 16 | 0b11_10_01_00 << 32) << 64 | 0x3f);
    assert_eq!(texel(b, BlockCode::Bc2, 3), [170, 0, 85, 0]);
    assert_eq!(texel(b, BlockCode::Bc2, 0), [0, 0, 255, 255]);
    assert_eq!(texel(b, BlockCode::Bc2, 1), [255, 0, 0, 51]);
}

#[test]
fn bc3_to_bc5_by_hand() {
    // End points 255, 0; indices 0, 1, 2, 7.
    let a = 255 | (1u128 << 3 | 2 << 6 | 7 << 9) << 16;
    let t = decode_block(&le(a), BlockCode::Bc4, false, false);
    assert_eq!([t[0][0], t[1][0], t[2][0], t[3][0]], [1.0, 0.0, 6.0 / 7.0, 1.0 / 7.0]);
    // Six values: 0, 1 and four between; 6 and 7 are 0 and 1.
    let a = 255 << 8 | (2u128 | 6 << 3 | 7 << 6) << 16;
    let t = decode_block(&le(a), BlockCode::Bc3, false, false);
    assert_eq!([t[0][3], t[1][3], t[2][3]], [1.0 / 5.0, 0.0, 1.0]);
    // Signed: -128 is -127.
    let a = 0x80 | 0x7f << 8 | (6u128 << 3 | 1 << 6 | 7 << 9) << 16;
    let t = decode_block(&le(a << 64 | a), BlockCode::Bc5, true, false);
    assert_eq!([t[0][0], t[1][1], t[2][0], t[3][1]], [-1.0, -1.0, 1.0, 1.0]);
}

#[test]
fn bc7_by_hand() {
    // Mode 6: one subset, 7-bit end points with p bits; both ends (127, 64, 0, 127), p = 1, 0.
    let mut v = 1u128 << 6;
    let mut at = 7;
    for c in [127u128, 64, 0, 127] {
        v |= c << at | c << (at + 7);
        at += 14;
    }
    v |= 1 << at; // p bits 1, 0; the first index 0
    v |= 15 << (at + 2 + 3); // the second 15
    assert_eq!(texel(le(v), BlockCode::Bc7, 0), [255, 129, 1, 255]);
    assert_eq!(texel(le(v), BlockCode::Bc7, 1), [254, 128, 0, 254]);
    // No mode bit.
    assert_eq!(texel(le(0x100), BlockCode::Bc7, 5), [0; 4]);
}

#[test]
fn bc6h_by_hand() {
    // Mode 11: ten bits, no differences. End point 0x3ff unquantises to the largest half.
    let v = 3u128 | 0x3ff << 5 | 0x3ff << 35;
    let t = decode_block(&le(v), BlockCode::Bc6U, false, false);
    assert_eq!(t[0], [65504.0, 0.0, 0.0, 1.0]);
    let t = decode_block(&le(v), BlockCode::Bc6S, false, false);
    assert!(t[0][0] < 0.0 && t[0][0] > -0.001);
    // Reserved.
    assert_eq!(decode_block(&le(0x13), BlockCode::Bc6U, false, false)[7], [0.0, 0.0, 0.0, 1.0]);
}

#[test]
fn etc_by_hand() {
    // Individual mode: both halves grey 0x88, table 0 (2, 8), all indices 0.
    let v = 0x88_88_88_00_0000_0000u64;
    assert_eq!(texel(be(v), BlockCode::Etc2Rgb, 0), [138, 138, 138, 255]);
    // Texel (column 1, row 0), number 4: index 3, -8.
    assert_eq!(texel(be(v | 1 << 4 | 1 << 20), BlockCode::Etc2Rgb, 1), [128, 128, 128, 255]);
    // Planar: differential with the blue sum out of range. Origin 0, all zero.
    // Blue 0 with difference -1: bits 42-40 all set; the origin has 2 bits of them: blue 7
    // of 63 (bits 41-39 and 44-43), the corners are 0.
    let v = 2u64 << 32 | 7 << 40;
    assert_eq!(texel(be(v), BlockCode::Etc2Rgb, 0), [0, 0, 24, 255]);
    assert_eq!(texel(be(v), BlockCode::Etc2Rgb, 5), [0, 0, 12, 255]);
    // EAC: base 128, multiplier 1, table 13, index 4 is 0.
    let idx = (0..16).fold(0u64, |a, i| a | 4 << (3 * i));
    let t = decode_block(&be(128 << 56 | 1 << 52 | 13 << 48 | idx), BlockCode::EacR, false, false);
    assert_eq!(t[0][0], 1028.0 / 2047.0);
    let t = decode_block(&be(128 << 56 | 1 << 52 | 13 << 48 | idx), BlockCode::EacR, true, false);
    assert_eq!(t[9][0], -1016.0 / 1023.0);
}

/// 2048 random blocks of each code, against what a hardware texture unit (Apple's, through
/// Metal) makes of them. BC1 to BC5 are not exact there.
#[test]
fn random_blocks() {
    use BlockCode::*;
    for (code, snorm, seed, want) in [
        (Bc6S, false, 3016446u64, 0x47d078cc07afa325u64),
        (Bc6U, false, 3016448, 0x29287ff516bf4325),
        (Bc7, false, 97302, 0x63aac86cdb93f3b0),
        (Etc2Rgb, false, 93051884271, 0xd1f5df5d0f947852),
        (Etc2Rgb8A1, false, 3001673102, 0x8e2d75eed6299b65),
        (Etc2Rgba, false, 2884608412498, 0xbc4aeac04f966c6d),
        (EacR, false, 3105291, 0xc0a4fbd04ef7a6a3),
        (EacR, true, 96264136, 0x4b74fc447d1ceb4f),
        (EacRg, false, 96264124, 0x988545cba930895d),
        (EacRg, true, 2984187959, 0x3643567cea4951a4),
    ] {
        let n = code.bytes() as usize;
        let mut rng = Rng(0x9E3779B97F4A7C15u64.wrapping_add(seed));
        let mut data: Vec<u8> = (0..2048 * n).map(|_| (rng.next() >> 24) as u8).collect();
        if code == Bc7 {
            for (i, b) in data.chunks_exact_mut(16).enumerate() {
                let m = (i % 8) as u32;
                b[0] = (b[0] & !((1u8 << m).wrapping_sub(1))) | 1 << m;
                if m == 7 {
                    b[0] = 0x80;
                }
            }
        }
        let mut h = 0xcbf29ce484222325u64;
        for b in data.chunks_exact(n) {
            let mut v = [0; 16];
            v[..n].copy_from_slice(b);
            for t in decode_block(&v, code, snorm, false) {
                for c in t {
                    let bits = if c == 0.0 { 0 } else { c.to_bits() };
                    h = (h ^ bits as u64).wrapping_mul(0x100000001b3);
                }
            }
        }
        assert_eq!(h, want, "{code:?} {snorm}");
    }
}

#[test]
fn compressed_textures() {
    // 10x6 texels, so 3x2 blocks; each block one colour, by its number.
    let mut v = View::new(0x24, 1, 10, 6, 1);
    v.swizzle = [R, G, B, ONE];
    v.last_level = 3;
    v.view_last = 3;
    let (v, ram) = build(v, 8, 4, |l, _, x, y| (x + 4 * y + 8 * l) as u128);
    let tex = Texture::from_words(&v.block_linear()).expect("format");
    let mut c = TexCache::new();
    for l in 0..4u32 {
        let m = tex.levels[l as usize];
        assert_eq!((m.width, m.height), ((10 >> l).max(1), (6 >> l).max(1)));
        for y in 0..m.height {
            for x in 0..m.width {
                let t = fetch(&tex, &mut c, &ram, [x as i32, y as i32, 0], 0, l, None);
                let n = x / 4 + 4 * (y / 4) + 8 * l;
                let want = (n << 3 | n >> 2) as f32 / 255.0;
                assert_eq!(f32::from_bits(t[2]), want, "level {l} {x} {y}");
            }
        }
    }
    // Signed BC4 is told apart by its type.
    let mut v = View::new(0x27, 1, 4, 4, 1);
    v.types = [SNORM; 4];
    let mut ram = Ram::new(v.address, 512);
    c.invalidate(); // the other texture was there
    ram.data[0] = 0x81;
    ram.data[1] = 0x7f;
    let tex = Texture::from_words(&v.block_linear()).expect("format");
    assert_eq!(f32::from_bits(fetch(&tex, &mut c, &ram, [0; 3], 0, 0, None)[0]), -1.0);
}
