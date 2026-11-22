use super::*;
use crate::num::*;
use crate::sample::{fetch, image_load, image_store};
use crate::texture::{TexCache, Texture};

/// A 1x1 texture of one format; the result of fetching it.
fn one(format: u32, types: [u32; 4], swizzle: [u32; 4], srgb: bool, bytes: u32, texel: u128) -> [u32; 4] {
    let mut v = View::new(format, 1, 1, 1, 1);
    v.types = types;
    v.swizzle = swizzle;
    v.srgb = srgb;
    let (v, ram) = build(v, bytes, 1, |_, _, _, _| texel);
    let tex = Texture::from_words(&v.block_linear()).expect("format");
    fetch(&tex, &mut TexCache::new(), &ram, [0; 3], 0, 0, None)
}

fn near(a: [u32; 4], b: [f32; 4]) {
    for (a, b) in a.iter().zip(b) {
        assert!((f32::from_bits(*a) - b).abs() < 1e-6, "{:?} {b}", f32::from_bits(*a));
    }
}

const RGBA: [u32; 4] = [R, G, B, A];
const BGRA: [u32; 4] = [B, G, R, A];
const U: [u32; 4] = [UNORM; 4];

#[test]
fn plain_formats() {
    let f = |n: f32, d: f32| n / d;
    // B8G8R8A8: blue in byte 0.
    near(one(8, U, BGRA, false, 4, 0x80_ff_40_20), [1.0, f(64., 255.), f(32., 255.), f(128., 255.)]);
    near(one(8, U, [R, G, B, ONE], false, 4, 0x80_ff_40_20), [f(32., 255.), f(64., 255.), 1.0, 1.0]);
    // sRGB: alpha stays linear.
    near(one(8, U, RGBA, true, 4, 0x80_ff_00_bc), [0.502_886_4, 0.0, 1.0, f(128., 255.)]);
    near(one(8, [SNORM; 4], RGBA, false, 4, 0x7f_81_80_c0), [f(-64., 127.), -1.0, -1.0, 1.0]);
    assert_eq!(one(8, [SINT; 4], RGBA, false, 4, 0x7f_81_80_ff), [!0, -128i32 as u32, -127i32 as u32, 127]);
    assert_eq!(one(8, [UINT; 4], [R, G, B, ONE_INT], false, 4, 0x7f_81_80_ff), [255, 128, 129, 1]);
    // B5G6R5: red in the top bits.
    near(one(0x15, U, [B, G, R, ONE], false, 2, 31 << 11 | 21 << 5 | 3), [1.0, f(21., 63.), f(3., 31.), 1.0]);
    near(one(0x14, U, BGRA, false, 2, 1 << 15 | 2 << 10 | 3 << 5 | 4), [f(2., 31.), f(3., 31.), f(4., 31.), 1.0]);
    near(one(0x12, U, BGRA, false, 2, 0x1234), [f(2., 15.), f(3., 15.), f(4., 15.), f(1., 15.)]);
    near(one(0x13, U, RGBA, false, 2, 1 | 2 << 1 | 3 << 6 | 4 << 11), [1.0, f(2., 31.), f(3., 31.), f(4., 31.)]);
    near(one(0x16, [SNORM, SNORM, UNORM, UNORM], [R, G, B, ONE], false, 2, 15 | 17 << 5 | 63 << 10), [1.0, -1.0, 1.0, 1.0]);
    // R10G10B10A2.
    let v = 1 | 512 << 10 | 1023 << 20 | 2 << 30;
    near(one(9, U, RGBA, false, 4, v), [f(1., 1023.), f(512., 1023.), 1.0, f(2., 3.)]);
    assert_eq!(one(9, [UINT; 4], RGBA, false, 4, v), [1, 512, 1023, 2]);
    // 8, 16 and 4 bit channels.
    near(one(0x1d, U, [ZERO, ZERO, ZERO, R], false, 1, 51), [0.0, 0.0, 0.0, 0.2]);
    near(one(0x18, U, [R, R, R, G], false, 2, 0xff33), [0.2, 0.2, 0.2, 1.0]);
    near(one(0x1e, U, [R, R, R, G], false, 1, 0xf3), [0.2, 0.2, 0.2, 1.0]);
    near(one(0x1b, U, [R, ZERO, ZERO, ONE], false, 2, 0x8000), [f(32768., 65535.), 0.0, 0.0, 1.0]);
    near(one(0x0c, [SNORM; 4], [R, G, ZERO, ONE], false, 4, 0x8000_7fff), [1.0, -1.0, 0.0, 1.0]);
    near(one(0x03, [FLOAT; 4], RGBA, false, 8, 0x3800_c000_0001_3c00), [1.0, 1.0 / 16777216.0, -2.0, 0.5]);
    // 32 bits.
    let w = 0x4_0000_0003_8000_0000_3f80_0000u128;
    assert_eq!(one(1, [UINT; 4], RGBA, false, 16, w), [0x3f80_0000, 0x8000_0000, 3, 4]);
    near(one(0x04, U, [R, G, ZERO, ONE], false, 8, 0x8000_0000_ffff_ffff), [1.0, 0.5, 0.0, 1.0]);
    near(one(0x0f, [FLOAT; 4], [R, R, R, R], false, 4, 0x3f80_0000), [1.0; 4]);
    // Small floats and shared exponents.
    near(one(0x21, [FLOAT; 4], [R, G, B, ONE], false, 4, 0x3c0 | 0x340 << 11 | 0x1c0 << 22), [1.0, 0.25, 0.5, 1.0]);
    near(one(0x20, [FLOAT; 4], [R, G, B, ONE], false, 4, 1 | 256 << 9 | 511 << 18 | 16 << 27), [1.0 / 256.0, 1.0, 511.0 / 256.0, 1.0]);
}

#[test]
fn depth_formats() {
    let d = [UNORM, UINT, UINT, UINT];
    let rrr1 = [R, R, R, ONE];
    let z = 0x123456 as f32 / 0xffffff as f32;
    // S8Z24: stencil on top.
    near(one(0x2b, d, rrr1, false, 4, 0xab_123456), [z, z, z, 1.0]);
    near(one(0x2a, d, rrr1, false, 4, 0xab_123456), [z, z, z, 1.0]);
    assert_eq!(one(0x0d, [UINT; 4], [G; 4], false, 4, 0xab_123456), [0xab; 4]);
    // Z24S8: stencil below.
    near(one(0x29, [UINT, UNORM, UINT, UINT], [G, G, G, ONE], false, 4, 0x1234_56ab), [z, z, z, 1.0]);
    assert_eq!(one(0x0e, [UINT; 4], [R; 4], false, 4, 0x1234_56ab), [0xab; 4]);
    near(one(0x3a, d, rrr1, false, 2, 0x4000), [16384.0 / 65535.0, 16384.0 / 65535.0, 16384.0 / 65535.0, 1.0]);
    let f = [FLOAT, UINT, UINT, UINT];
    near(one(0x2f, f, rrr1, false, 4, 0x3f00_0000), [0.5, 0.5, 0.5, 1.0]);
    near(one(0x30, f, rrr1, false, 8, 0xffff_ff77_3f00_0000), [0.5, 0.5, 0.5, 1.0]);
    assert_eq!(one(0x05, [UINT; 4], [G; 4], false, 8, 0xffff_ff77_3f00_0000), [0x77; 4]);
}

#[test]
fn packed_pairs() {
    // R8G8_B8G8: bytes R, G0, B, G1.
    for (code, swz, bytes) in [
        (0x22, [R, G, B, ONE], [51u8, 255, 102, 0]),
        (0x23, [R, G, B, ONE], [255, 51, 0, 102]),
    ] {
        let mut v = View::new(code, 1, 2, 1, 1);
        v.swizzle = swz;
        // One block holds both texels.
        let mut ram = Ram::new(v.address, 512);
        ram.data[..4].copy_from_slice(&bytes);
        v.block = BlockDims::default();
        let tex = Texture::from_words(&v.block_linear()).expect("format");
        let mut c = TexCache::new();
        near(fetch(&tex, &mut c, &ram, [0; 3], 0, 0, None), [0.2, 1.0, 0.4, 1.0]);
        near(fetch(&tex, &mut c, &ram, [1, 0, 0], 0, 0, None), [0.2, 0.0, 0.4, 1.0]);
    }
}

#[test]
fn buffers() {
    // RGB32F texels of 12 bytes cross the 512-byte lines.
    let mut v = View::new(2, 6, 100, 1, 1);
    v.types = [FLOAT; 4];
    v.swizzle = [R, G, B, ONE];
    v.address += 16;
    let mut ram = Ram::new(v.address, 1200);
    for i in 0..300 {
        ram.write(v.address + i * 4, &(i as f32).to_le_bytes());
    }
    let tex = Texture::from_words(&v.buffer()).expect("format");
    let mut c = TexCache::new();
    for i in 0..100 {
        let x = 3.0 * i as f32;
        near(fetch(&tex, &mut c, &ram, [i, 0, 0], 0, 0, None), [x, x + 1.0, x + 2.0, 1.0]);
    }
    near(fetch(&tex, &mut c, &ram, [100, 0, 0], 0, 0, None), [0.0, 0.0, 0.0, 1.0]);
    near(fetch(&tex, &mut c, &ram, [-1, 0, 0], 0, 0, None), [0.0, 0.0, 0.0, 1.0]);
}

#[test]
fn images() {
    let mut v = View::new(8, 5, 20, 10, 3);
    v.swizzle = BGRA;
    v.scaled = true;
    let (v, mut ram) = build(v, 4, 1, |_, l, x, y| (x | y << 8 | l << 16 | 0xff00_0000) as u128);
    let tex = Texture::from_words(&v.block_linear()).expect("format");
    near(image_load(&tex, &ram, [7, 9, 0], 2), [2.0 / 255.0, 9.0 / 255.0, 7.0 / 255.0, 1.0]);
    image_store(&tex, &mut ram, [7, 9, 0], 2, bits([1.0, 0.2, 0.0, 2.0]));
    near(image_load(&tex, &ram, [7, 9, 0], 2), [1.0, 0.2, 0.0, 1.0]);
    near(image_load(&tex, &ram, [8, 9, 0], 2), [2.0 / 255.0, 9.0 / 255.0, 8.0 / 255.0, 1.0]);
    // Outside: nothing happens.
    for p in [[20, 0, 0], [0, 10, 0], [-1, 0, 0]] {
        image_store(&tex, &mut ram, p, 0, [!0; 4]);
        assert_eq!(image_load(&tex, &ram, p, 0), [0; 4]);
    }
    assert_eq!(image_load(&tex, &ram, [0; 3], 3), [0; 4]);
    // Every format the driver has images of keeps what is stored.
    let int = |t: u32| [t; 4];
    for (format, bytes, types) in [
        (1, 16, int(FLOAT)), (1, 16, int(SINT)), (1, 16, int(UINT)), (3, 8, int(FLOAT)), (3, 8, int(UNORM)),
        (3, 8, int(SNORM)), (3, 8, int(SINT)), (3, 8, int(UINT)), (8, 4, int(UNORM)), (8, 4, int(SNORM)),
        (8, 4, int(SINT)), (8, 4, int(UINT)), (0x21, 4, int(FLOAT)), (9, 4, int(UNORM)), (9, 4, int(UINT)),
        (4, 8, int(FLOAT)), (4, 8, int(SINT)), (4, 8, int(UINT)), (0x0c, 4, int(FLOAT)), (0x0c, 4, int(UNORM)),
        (0x0c, 4, int(SNORM)), (0x0c, 4, int(SINT)), (0x0c, 4, int(UINT)), (0x18, 2, int(UNORM)),
        (0x18, 2, int(SNORM)), (0x18, 2, int(SINT)), (0x18, 2, int(UINT)), (0x0f, 4, int(FLOAT)),
        (0x0f, 4, int(SINT)), (0x0f, 4, int(UINT)), (0x1b, 2, int(FLOAT)), (0x1b, 2, int(UNORM)),
        (0x1b, 2, int(SNORM)), (0x1b, 2, int(SINT)), (0x1b, 2, int(UINT)), (0x1d, 1, int(UNORM)),
        (0x1d, 1, int(SNORM)), (0x1d, 1, int(SINT)), (0x1d, 1, int(UINT)),
    ] {
        let mut v = View::new(format, 1, 4, 4, 1);
        v.types = types;
        let mut ram = Ram::new(v.address, 4096);
        let tex = Texture::from_words(&v.block_linear()).expect("format");
        let value = if types[0] == FLOAT || types[0] == UNORM { bits([1.0, 0.0, 1.0, 1.0]) } else if types[0] == SNORM { bits([-1.0, 1.0, 0.0, -1.0]) } else { [1, 0, 1, 1] };
        image_store(&tex, &mut ram, [2, 3, 0], 0, value);
        let got = image_load(&tex, &ram, [2, 3, 0], 0);
        for (i, c) in tex.packed.ch.iter().enumerate() {
            if c.bits != 0 {
                assert_eq!(got[i], value[i], "{format:#x} {types:?}");
            }
        }
        assert_eq!(tex.format.bytes, bytes);
    }
}

#[test]
fn halves() {
    for h in 0..=0xffffu16 {
        let f = half_to_f32(h);
        if f.is_nan() {
            assert!(h & 0x7fff > 0x7c00);
        } else {
            assert_eq!(f32_to_half(f), h);
        }
    }
    assert_eq!(half_to_f32(0x3555), 0.333_251_95);
    assert_eq!(f32_to_half(65519.0), 0x7bff);
    assert_eq!(f32_to_half(65520.0), 0x7c00);
    assert_eq!(f32_to_half(1.0 + 1.0 / 2048.0), 0x3c00); // tie to even
    assert_eq!(f32_to_half(1.0 + 3.0 / 2048.0), 0x3c02);
    assert!(half_to_f32(f32_to_half(f32::NAN)).is_nan());
    for mant in [5, 6] {
        for v in 0..32 << mant {
            let f = ufloat_to_f32(v, mant);
            if !f.is_nan() {
                assert_eq!(f32_to_ufloat(f, mant), v);
            }
        }
    }
    assert_eq!(f32_to_ufloat(-1.0, 6), 0);
    assert_eq!(ufloat_to_f32(f32_to_ufloat(1e9, 6), 6), 65024.0);
    assert_eq!(ufloat_to_f32(f32_to_ufloat(1e9, 5), 5), 64512.0);
}

#[test]
fn srgb() {
    let t = srgb8_table();
    for i in 0..=255u8 {
        assert_eq!(linear_to_srgb8(t[i as usize]), i);
    }
    assert_eq!((t[0], t[255]), (0.0, 1.0));
    assert!((t[188] - 0.502_886_4).abs() < 1e-6);
    assert_eq!((linear_to_srgb8(0.5), linear_to_srgb8(-3.0), linear_to_srgb8(7.0), linear_to_srgb8(f32::NAN)), (188, 0, 255, 0));
    for i in 0..20000 {
        let v = i as f64 / 19999.0;
        let s = if v <= 0.0031308 { v * 12.92 } else { 1.055 * v.powf(1.0 / 2.4) - 0.055 };
        let e = s * 255.0;
        assert!((linear_to_srgb8(v as f32) as f64 - e).abs() < 0.501, "{v}");
    }
}
