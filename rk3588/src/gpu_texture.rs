//! Valhall single-level linear RGBA8 texture sampling (Mesa v10.xml/v7.xml).
use crate::{
    gpu_mmu::AddressSpace,
    gpu_resources::{Error, Resources},
};
fn word(b: &[u8], n: usize) -> u32 {
    u32::from_le_bytes(b[n * 4..n * 4 + 4].try_into().unwrap())
}
fn pair(b: &[u8], n: usize) -> u64 {
    word(b, n) as u64 | ((word(b, n + 1) as u64) << 32)
}
fn coordinate(value: f32, mode: u32) -> Result<f64, Error> {
    if !value.is_finite() {
        return Err(Error::Unsupported);
    }
    let v = value as f64;
    match mode {
        8 => Ok(v.rem_euclid(1.)),
        9 => Ok(v.clamp(0., 1.)),
        12 => {
            let v = v.rem_euclid(2.);
            Ok(if v <= 1. { v } else { 2. - v })
        }
        _ => Err(Error::Unsupported),
    }
}
fn wrap(index: i32, size: u32, mode: u32) -> u32 {
    let n = size as i32;
    match mode {
        8 => index.rem_euclid(n) as u32,
        9 => index.clamp(0, n - 1) as u32,
        12 => {
            let i = index.rem_euclid(2 * n);
            (if i < n { i } else { 2 * n - 1 - i }) as u32
        }
        _ => unreachable!(), // coordinate() validates each wrap mode first.
    }
}
/// The compact Valhall handle packs texture table/index and sampler table/index.
/// Only ordinary resource tables are resolved; hardware-provided tables are not
/// aliases of similarly numbered guest resource tables.
pub fn sample(
    m: &AddressSpace,
    ram: &[u8],
    resources: Resources,
    handle: u32,
    uv: [f32; 2],
) -> Result<[f32; 4], Error> {
    let texture_table = handle >> 27;
    let sampler_table = (handle >> 11) & 31;
    if handle & ((1 << 10) | (1 << 26)) != 0 || texture_table >= 12 || sampler_table >= 12 {
        return Err(Error::Unsupported);
    }
    let t = resources.descriptor(m, ram, texture_table, (handle >> 16) & 1023)?;
    let s = resources.descriptor(m, ram, sampler_table, handle & 1023)?;
    // RGBA8 UNORM, RGBA component order, 2D; no interleave, mip chain,
    // arrays, depth, multisampling, sRGB conversion or coordinate bias.
    if word(&t, 0) != (187 << 22) | 0x22
        || word(&t, 2) & !4095 != 0
        || word(&t, 3) != 0
        || word(&t, 6) != 0
        || word(&t, 7) != 0
    {
        return Err(Error::Unsupported);
    }
    let swizzle = word(&t, 2);
    if (0..4).any(|n| (swizzle >> (n * 3)) & 7 > 5) {
        return Err(Error::Unsupported);
    }
    let ctl = word(&s, 0);
    let ignored = 0xfff00 | (1 << 21) | (1 << 23) | (1 << 26) | (3 << 27) | (1 << 29) | (1 << 30);
    if ctl & !ignored != 1 | (1 << 25)
        || (ctl >> 27) & 1 != (ctl >> 28) & 1
        || word(&s, 1) & !0x1fff0000 != 0
        || word(&s, 2) != 0
        || word(&s, 3) != 0
    {
        return Err(Error::Unsupported);
    }
    // With one mip level and identical min/mag filters, derivatives cannot
    // affect LOD or filter selection. No neighboring helper lanes are needed.
    let modes = [(ctl >> 16) & 15, (ctl >> 12) & 15];
    let uv = [coordinate(uv[0], modes[0])?, coordinate(uv[1], modes[1])?];
    let width = (word(&t, 1) & 65535) + 1;
    let height = (word(&t, 1) >> 16) + 1;
    let surfaces = pair(&t, 4);
    if surfaces & 31 != 0 {
        return Err(Error::Unsupported);
    }
    let mut p = [0; 32];
    m.read(ram, surfaces, &mut p, true).map_err(Error::Memory)?;
    let stride = word(&p, 4) as u64;
    let extent = (height as u64 - 1) * stride + width as u64 * 4;
    if word(&p, 0) != (2 << 24) | (2 << 8) | 0x1a
        || stride < width as u64 * 4
        || extent > word(&p, 1) as u64
        || word(&p, 5) != 0
        || (word(&p, 6) as u64) < extent
        || word(&p, 7) != word(&t, 1)
    {
        return Err(Error::Unsupported);
    }
    let fetch = |x: i32, y: i32| -> Result<[f64; 4], Error> {
        let offset =
            wrap(y, height, modes[1]) as u64 * stride + wrap(x, width, modes[0]) as u64 * 4;
        let at = pair(&p, 2)
            .checked_add(offset)
            .ok_or(Error::DescriptorBounds)?;
        let mut rgba = [0; 4];
        m.read(ram, at, &mut rgba, true).map_err(Error::Memory)?;
        Ok(rgba.map(|v| v as f64 / 255.))
    };
    let texel = if ctl & (1 << 27) != 0 {
        fetch(
            (uv[0] * width as f64).floor() as i32,
            (uv[1] * height as f64).floor() as i32,
        )?
    } else {
        let x = uv[0] * width as f64 - 0.5;
        let y = uv[1] * height as f64 - 0.5;
        let (ix, iy) = (x.floor() as i32, y.floor() as i32);
        let (fx, fy) = (x - x.floor(), y - y.floor());
        let a = fetch(ix, iy)?;
        let b = fetch(ix + 1, iy)?;
        let c = fetch(ix, iy + 1)?;
        let d = fetch(ix + 1, iy + 1)?;
        std::array::from_fn(|n| {
            (a[n] * (1. - fx) + b[n] * fx) * (1. - fy) + (c[n] * (1. - fx) + d[n] * fx) * fy
        })
    };
    Ok(std::array::from_fn(|n| match (swizzle >> (n * 3)) & 7 {
        4 => 0.,
        5 => 1.,
        c => texel[c as usize] as f32,
    }))
}

#[cfg(test)]
pub(crate) fn test_fixture() -> (AddressSpace, Resources, Vec<u8>, u32) {
    let m = AddressSpace {
        transtab: 0,
        transcfg: 2,
    };
    let mut ram = vec![0; 0x10000];
    fn put(ram: &mut [u8], at: usize, words: &[u32]) {
        for (n, word) in words.iter().enumerate() {
            ram[at + n * 4..at + n * 4 + 4].copy_from_slice(&word.to_le_bytes());
        }
    }
    for table in 0..32 {
        put(&mut ram, 0x1000 + table * 16, &[0x2000, 1 << 24, 32, 0]);
    }
    put(&mut ram, 0x1040, &[0x2040, 1 << 24, 32, 0]);
    put(
        &mut ram,
        0x2000,
        &[
            1 | (8 << 8)
                | (9 << 12)
                | (9 << 16)
                | (1 << 21)
                | (1 << 23)
                | (1 << 25)
                | (1 << 26)
                | (3 << 27)
                | (1 << 30),
            0x1fff0000,
        ],
    );
    put(
        &mut ram,
        0x2040,
        &[(187 << 22) | 0x22, 0x10001, 0x688, 0, 0x2100],
    );
    put(
        &mut ram,
        0x2100,
        &[
            (2 << 24) | (2 << 8) | 0x1a,
            72,
            0x3000,
            0,
            64,
            0,
            128,
            0x10001,
        ],
    );
    ram[0x3000..0x3080].fill(0xa5);
    ram[0x3000..0x3008].copy_from_slice(&[255, 0, 0, 255, 0, 255, 0, 128]);
    ram[0x3040..0x3048].copy_from_slice(&[0, 0, 255, 64, 255, 255, 0, 255]);
    (m, Resources(0x1020), ram, (4 << 27) | (3 << 11))
}
#[cfg(test)]
mod tests {
    use super::*;
    fn set(ram: &mut [u8], at: usize, value: u32) {
        ram[at..at + 4].copy_from_slice(&value.to_le_bytes());
    }
    fn bytes(v: [f32; 4]) -> [u8; 4] {
        v.map(|v| (v * 255.).round() as u8)
    }
    #[test]
    fn native_resources_sample_padded_rows_and_descriptor_indices() {
        let (m, resources, mut ram, h) = test_fixture();
        for (uv, expected) in [
            ([0.25, 0.25], [255, 0, 0, 255]),
            ([0.75, 0.25], [0, 255, 0, 128]),
            ([0.25, 0.75], [0, 0, 255, 64]),
            ([0.75, 0.75], [255, 255, 0, 255]),
        ] {
            assert_eq!(bytes(sample(&m, &ram, resources, h, uv).unwrap()), expected);
        }
        assert_eq!(
            sample(&m, &ram, resources, h | 1, [0.25, 0.25]),
            Err(Error::DescriptorBounds)
        );
        ram.copy_within(0x2000..0x2020, 0x2020);
        ram.copy_within(0x2040..0x2060, 0x2060);
        set(&mut ram, 0x1038, 64);
        set(&mut ram, 0x1048, 64);
        assert_eq!(
            bytes(sample(&m, &ram, resources, h | (1 << 16) | 1, [0.75, 0.75]).unwrap()),
            [255, 255, 0, 255]
        );
        // Swizzle uses the fetched channels plus explicit zero/one.
        set(&mut ram, 0x2048, 2 | (1 << 3) | (4 << 6) | (5 << 9));
        assert_eq!(
            bytes(sample(&m, &ram, resources, h, [0.25, 0.75]).unwrap()),
            [255, 0, 0, 255]
        );
    }
    #[test]
    fn bilinear_centers_and_wrap_boundaries() {
        let (m, resources, mut ram, h) = test_fixture();
        let ctl = word(&ram[0x2000..], 0) & !(3 << 27);
        for mode in [8, 9, 12] {
            set(
                &mut ram,
                0x2000,
                (ctl & !0xff000) | (mode << 12) | (mode << 16),
            );
            let middle = sample(&m, &ram, resources, h, [0.5, 0.5]).unwrap();
            assert_eq!(middle, [0.5, 0.5, 0.25, 175.5 / 255.]);
            let corner = bytes(sample(&m, &ram, resources, h, [0., 0.]).unwrap());
            assert_eq!(
                corner,
                if mode == 8 {
                    [128, 128, 64, 176]
                } else {
                    [255, 0, 0, 255]
                }
            );
            let edge = bytes(sample(&m, &ram, resources, h, [-0.25, 0.25]).unwrap());
            assert_eq!(
                edge,
                if mode == 8 {
                    [0, 255, 0, 128]
                } else {
                    [255, 0, 0, 255]
                }
            );
        }
        // Reducing the normalized coordinate before multiplying prevents host
        // float/integer overflow for extreme finite shader inputs.
        assert_eq!(
            bytes(sample(&m, &ram, resources, h, [f32::MAX, -f32::MAX]).unwrap()),
            [255, 0, 0, 255]
        );
        assert_eq!(
            sample(&m, &ram, resources, h, [f32::NAN, 0.]),
            Err(Error::Unsupported)
        );
    }
    #[test]
    fn invalid_formats_filter_state_and_plane_bounds_fault() {
        for (at, value) in [
            (0x2040, 0),
            (0x2048, 1 << 16),
            (0x2048, 7),
            (0x204c, 1),
            (0x2058, 1),
            (0x205c, 1),
            (0x2004, 1),
            (0x2008, 1),
            (0x2100, 0),
            (0x2104, 71),
            (0x2110, 4),
            (0x2118, 1),
            (0x211c, 0),
        ] {
            let (m, resources, mut ram, h) = test_fixture();
            set(&mut ram, at, value);
            assert!(
                sample(&m, &ram, resources, h, [0.25, 0.25]).is_err(),
                "at={at:x}"
            );
        }
        let (m, resources, mut ram, h) = test_fixture();
        let ctl = word(&ram[0x2000..], 0);
        set(&mut ram, 0x2000, ctl ^ (1 << 27));
        assert_eq!(
            sample(&m, &ram, resources, h, [0.25, 0.25]),
            Err(Error::Unsupported)
        );
        set(&mut ram, 0x2000, ctl);
        set(&mut ram, 0x2108, 0xfffffff0);
        assert!(matches!(
            sample(&m, &ram, resources, h, [0.25, 0.25]),
            Err(Error::Memory(_))
        ));
    }
    #[test]
    fn hardware_tables_and_reserved_handle_bits_never_alias_guest_resources() {
        for table in [12, 13, 14, 15, 28, 29, 30, 31] {
            let (m, resources, mut ram, h) = test_fixture();
            // Populate a valid texture descriptor at the forbidden ordinary
            // table too, so rejecting the handle cannot be a type/bounds fluke.
            set(&mut ram, 0x1000 + table * 16, 0x2040);
            assert_eq!(
                sample(
                    &m,
                    &ram,
                    resources,
                    (h & !(31 << 27)) | ((table as u32) << 27),
                    [0.25, 0.25]
                ),
                Err(Error::Unsupported)
            );
            set(&mut ram, 0x1000 + table * 16, 0x2000);
            assert_eq!(
                sample(
                    &m,
                    &ram,
                    resources,
                    (h & !(31 << 11)) | ((table as u32) << 11),
                    [0.25, 0.25]
                ),
                Err(Error::Unsupported)
            );
        }
        let (m, r, ram, h) = test_fixture();
        for bit in [10, 26] {
            assert_eq!(
                sample(&m, &ram, r, h | (1 << bit), [0.25, 0.25]),
                Err(Error::Unsupported)
            );
        }
    }
}
