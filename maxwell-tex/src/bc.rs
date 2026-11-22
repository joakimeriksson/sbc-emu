//! BC1 to BC5 (S3TC, RGTC) blocks.

use crate::num::srgb8_table;

/// A decoded 4x4 block, row by row.
pub type Texels = [[f32; 4]; 16];

/// 8-bit colour to floats.
#[inline]
pub(crate) fn rgba8(c: [u8; 4], srgb: bool) -> [f32; 4] {
    let a = c[3] as f32 / 255.0;
    if srgb {
        let t = srgb8_table();
        [t[c[0] as usize], t[c[1] as usize], t[c[2] as usize], a]
    } else {
        [c[0] as f32 / 255.0, c[1] as f32 / 255.0, c[2] as f32 / 255.0, a]
    }
}

pub(crate) fn le64(b: &[u8]) -> u64 {
    let mut v = [0u8; 8];
    v.copy_from_slice(&b[..8]);
    u64::from_le_bytes(v)
}

fn rgb565(v: u32) -> [u8; 3] {
    let (r, g, b) = (v >> 11 & 31, v >> 5 & 63, v & 31);
    [(r << 3 | r >> 2) as u8, (g << 2 | g >> 4) as u8, (b << 3 | b >> 2) as u8]
}

/// The colour part: alpha is 255, or 0 for BC1's transparent black.
fn color(block: u64, bc1: bool) -> [[u8; 4]; 16] {
    let (e0, e1) = (block as u32 & 0xffff, (block >> 16) as u32 & 0xffff);
    let (c0, c1) = (rgb565(e0), rgb565(e1));
    let mix = |w0: u32, w1: u32| {
        let s = w0 + w1;
        let m = |i: usize| ((w0 * c0[i] as u32 + w1 * c1[i] as u32 + s / 2) / s) as u8;
        [m(0), m(1), m(2), 255]
    };
    let pal = if !bc1 || e0 > e1 {
        [mix(1, 0), mix(0, 1), mix(2, 1), mix(1, 2)]
    } else {
        [mix(1, 0), mix(0, 1), mix(1, 1), [0; 4]]
    };
    std::array::from_fn(|i| pal[(block >> (32 + 2 * i)) as usize & 3])
}

pub fn bc1(b: &[u8], srgb: bool) -> Texels {
    color(le64(b), true).map(|c| rgba8(c, srgb))
}

pub fn bc2(b: &[u8], srgb: bool) -> Texels {
    let alpha = le64(b);
    let mut t = color(le64(&b[8..]), false).map(|c| rgba8(c, srgb));
    for (i, c) in t.iter_mut().enumerate() {
        c[3] = (alpha >> (4 * i) & 15) as f32 / 15.0;
    }
    t
}

/// BC3: colour with a BC4 block for alpha.
pub fn bc3(b: &[u8], srgb: bool) -> Texels {
    let alpha = channel(le64(b), false);
    let mut t = color(le64(&b[8..]), false).map(|c| rgba8(c, srgb));
    for (c, a) in t.iter_mut().zip(alpha) {
        c[3] = a;
    }
    t
}

/// One BC4 channel: two 8-bit end points and 3-bit indices.
fn channel(block: u64, signed: bool) -> [f32; 16] {
    let (e0, e1, f0, f1);
    if signed {
        (e0, e1) = (block as u8 as i8 as i32, (block >> 8) as u8 as i8 as i32);
        (f0, f1) = (e0.max(-127) as f32 / 127.0, e1.max(-127) as f32 / 127.0);
    } else {
        (e0, e1) = (block as u8 as i32, (block >> 8) as u8 as i32);
        (f0, f1) = (e0 as f32 / 255.0, e1 as f32 / 255.0);
    }
    let mut pal = [f0, f1, 0.0, 0.0, 0.0, 0.0, if signed { -1.0 } else { 0.0 }, 1.0];
    let n = if e0 > e1 { 7 } else { 5 };
    for i in 1..n {
        pal[i + 1] = ((n - i) as f32 * f0 + i as f32 * f1) / n as f32;
    }
    std::array::from_fn(|i| pal[(block >> (16 + 3 * i)) as usize & 7])
}

pub fn bc4(b: &[u8], signed: bool) -> Texels {
    channel(le64(b), signed).map(|r| [r, 0.0, 0.0, 1.0])
}

pub fn bc5(b: &[u8], signed: bool) -> Texels {
    let (r, g) = (channel(le64(b), signed), channel(le64(&b[8..]), signed));
    std::array::from_fn(|i| [r[i], g[i], 0.0, 1.0])
}
