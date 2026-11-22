use super::*;
use crate::num::{bits, f32s};
use crate::sample::*;
use crate::texture::{Sampler, TexCache, Texture};

/// Reference: wrap a texel number (None: border), written from the GL specification.
fn wrap_texel(wrap: u32, i: i64, n: i64) -> Option<i64> {
    let mirror = |a: i64| if a >= 0 { a } else { -(1 + a) };
    let inside = |i: i64| (0..n).contains(&i).then_some(i);
    match wrap {
        0 => Some(i.rem_euclid(n)),
        1 => Some((n - 1) - mirror(i.rem_euclid(2 * n) - n)),
        2 => Some(i.clamp(0, n - 1)),
        3 | 4 => inside(i),
        5 => Some(mirror(i).clamp(0, n - 1)),
        _ => inside(mirror(i)),
    }
}

/// The coordinate, in texels, after the clamps GL applies to it before filtering.
fn clamp_coord(wrap: u32, u: f64, n: f64, nearest: bool) -> f64 {
    match wrap {
        4 if nearest => u.clamp(0.0, n - 0.001),
        4 => u.clamp(0.0, n),
        7 if nearest => u.abs().min(n - 0.001),
        7 => u.abs().min(n),
        _ => u,
    }
}

struct Reference<'a> {
    smp: Smp,
    /// Texels of a level: x, y, z or layer.
    texel: &'a dyn Fn(u32, i64, i64, i64) -> [f64; 4],
    border: [f64; 4],
    size: [i64; 3],
    axes: usize,
    normalized: bool,
}

impl Reference<'_> {
    fn level(&self, l: u32, c: [f64; 3], layer: i64, linear: bool, off: [i64; 3]) -> [f64; 4] {
        let mut axis = Vec::new();
        for a in 0..self.axes {
            let n = (self.size[a] >> l).max(1);
            let w = self.smp.wrap[a];
            let mut u = if self.normalized { c[a] * n as f64 } else { c[a] };
            u += off[a] as f64;
            u = clamp_coord(w, u, n as f64, !linear);
            if linear {
                let i = (u - 0.5).floor();
                let f = u - 0.5 - i;
                axis.push(vec![(wrap_texel(w, i as i64, n), 1.0 - f), (wrap_texel(w, i as i64 + 1, n), f)]);
            } else {
                axis.push(vec![(wrap_texel(w, u.floor() as i64, n), 1.0)]);
            }
        }
        while axis.len() < 3 {
            axis.push(vec![(Some(0), 1.0)]);
        }
        let mut out = [0.0; 4];
        for (x, wx) in &axis[0] {
            for (y, wy) in &axis[1] {
                for (z, wz) in &axis[2] {
                    let t = match (x, y, z) {
                        (Some(x), Some(y), Some(z)) => {
                            (self.texel)(l, *x, *y, if self.axes == 3 { *z } else { layer })
                        }
                        _ => self.border,
                    };
                    for k in 0..4 {
                        out[k] += t[k] * wx * wy * wz;
                    }
                }
            }
        }
        out
    }
}

fn close(a: [u32; 4], b: [f64; 4], eps: f64, what: &str) {
    for (a, b) in f32s(a).iter().zip(b) {
        assert!((*a as f64 - b).abs() < eps, "{what}: {a} {b}");
    }
}

fn noise(l: u32, layer: u32, x: u32, y: u32) -> u32 {
    let mut r = Rng(0x1234_5678_9abc_def1 ^ (l as u64) << 48 ^ (layer as u64) << 32 ^ (y as u64) << 16 ^ x as u64);
    r.next();
    r.next();
    (r.next() >> 13) as u32
}

fn unorm8(v: u32) -> [f64; 4] {
    v.to_le_bytes().map(|b| b as f64 / 255.0)
}

#[test]
fn filters_and_wraps_against_the_reference() {
    let mut rng = Rng(3);
    // (texture type, dimensions, scaled coordinates)
    for (ty, w, h, d, axes, scaled) in [
        (1, 24, 10, 1, 2, false),
        (1, 16, 16, 1, 2, false),
        (1, 37, 5, 1, 2, true),
        (0, 19, 1, 1, 1, false),
        (2, 9, 6, 7, 3, false),
        (5, 12, 20, 4, 2, false),
        (4, 33, 1, 3, 1, false),
        (7, 40, 12, 1, 2, false),
    ] {
        let mut v = View::new(8, ty, w, h, d);
        v.scaled = scaled;
        let (words, ram) = if ty == 7 {
            v.pitch = 192;
            let mut ram = Ram::new(v.address, 192 * 16);
            for y in 0..h {
                for x in 0..w {
                    ram.write(v.address + (y * 192 + x * 4) as u64, &noise(0, 0, x, y).to_le_bytes());
                }
            }
            (v.pitch(), ram)
        } else {
            let (v, ram) = build(v, 4, 1, |l, z, x, y| noise(l, z, x, y) as u128);
            (v.block_linear(), ram)
        };
        let tex = Texture::from_words(&words).expect("format");
        for wrap in 0..8 {
            for linear in [false, true] {
                let mut s = Smp::new(linear, wrap);
                s.wrap = [wrap, (wrap + rng.below(8)) & 7, wrap];
                s.border = bits([0.25, 0.5, 0.75, 1.0]);
                let smp = Sampler::from_words(&s.words());
                let texel = |_, x: i64, y: i64, z: i64| unorm8(noise(0, z as u32, x as u32, y as u32));
                let reference = Reference {
                    smp: s,
                    texel: &texel,
                    border: [0.25, 0.5, 0.75, 1.0],
                    size: [w as i64, h as i64, d as i64],
                    axes,
                    normalized: !scaled,
                };
                let mut cache = TexCache::new();
                for _ in 0..60 {
                    let mut a = Args::default();
                    for l in 0..4 {
                        for c in 0..3 {
                            let k = if scaled { [w, h, d][c] as f32 } else { 1.0 };
                            // Multiples of 1/256 texel have exact weights.
                            a.coord[c][l] = rng.coord() * k;
                            a.offset[c][l] = rng.below(16) as i8 - 8;
                        }
                        a.layer[l] = rng.below(d + 2);
                    }
                    if rng.below(2) == 0 {
                        a.offset = [[0; 4]; 3];
                    }
                    let got = sample(&tex, &smp, &mut cache, &ram, &a);
                    for l in 0..4 {
                        let c = [a.coord[0][l] as f64, a.coord[1][l] as f64, a.coord[2][l] as f64];
                        let off = [a.offset[0][l] as i64, a.offset[1][l] as i64, a.offset[2][l] as i64];
                        let layer = if axes == 3 || ty < 3 || ty == 7 { 0 } else { a.layer[l].min(d - 1) as i64 };
                        let want = reference.level(0, c, layer, linear, off);
                        close(got[l], want, if linear { 0.015 } else { 1e-6 }, &format!("{ty} {wrap} {linear} {c:?} {off:?}"));
                    }
                }
            }
        }
    }
}

#[test]
fn fast_path_equals_the_general_one() {
    // Clamp to edge has the RGBA8 path; GL_CLAMP of inner coordinates the general one.
    let mut v = View::new(8, 1, 64, 48, 1);
    v.swizzle = [B, G, R, ONE];
    for srgb in [false, true] {
        v.srgb = srgb;
        let (v, ram) = build(v, 4, 1, |l, z, x, y| noise(l, z, x, y) as u128);
        let tex = Texture::from_words(&v.block_linear()).expect("format");
        let fast = Sampler::from_words(&Smp::new(true, 2).words());
        let slow = Sampler::from_words(&Smp::new(true, 3).words());
        assert!(fast.simple_2d && !slow.simple_2d);
        let mut rng = Rng(8);
        let mut cache = TexCache::new();
        for _ in 0..500 {
            let mut a = Args::default();
            for l in 0..4 {
                a.coord[0][l] = (1.0 + rng.below(6000) as f32 / 100.0) / 64.0;
                a.coord[1][l] = (1.0 + rng.below(4600) as f32 / 100.0) / 48.0;
            }
            let x = sample(&tex, &fast, &mut cache, &ram, &a);
            let y = sample(&tex, &slow, &mut cache, &ram, &a);
            for l in 0..4 {
                close(x[l], f32s(y[l]).map(|v| v as f64), 1e-6, "fast");
            }
        }
    }
}

/// A 256x256 texture with a different grey in each level.
fn levels(first: u32, last: u32) -> (Texture, Ram) {
    let mut v = View::new(0x1d, 1, 256, 256, 1);
    v.swizzle = [R, R, R, ONE];
    v.last_level = 8;
    v.view_first = first;
    v.view_last = last;
    let (v, ram) = build(v, 1, 1, |l, _, _, _| 25 * l as u128);
    (Texture::from_words(&v.block_linear()).expect("format"), ram)
}

/// The level that a sample came from, 0..8.
fn level_of(t: [u32; 4]) -> f32 {
    f32::from_bits(t[0]) * 255.0 / 25.0
}

#[test]
fn level_selection() {
    let (tex, ram) = levels(0, 8);
    let mut c = TexCache::new();
    let mut s = Smp::new(true, 2);
    s.mip = 2;
    let trilinear = Sampler::from_words(&s.words());
    let mut get = |tex: &Texture, smp: &Sampler, lod: Lod, coord: [[f32; 4]; 3]| {
        level_of(sample(tex, smp, &mut c, &ram, &Args { lod, coord, ..Args::default() })[0])
    };
    let at = |x: f32, y: f32| [[0.5, 0.5 + x / 256.0, 0.5, 0.5 + x / 256.0], [0.5, 0.5, 0.5 + y / 256.0, 0.5 + y / 256.0], [0.0; 4]];
    // Derivatives of n texels give level log2(n); the longer one counts.
    for (x, y, want) in [(1.0, 1.0, 0.0), (2.0, 1.0, 1.0), (1.0, 4.0, 2.0), (0.1, 0.2, 0.0), (64.0, 0.0, 6.0), (1000.0, 0.0, 8.0), (3.0, 0.0, 1.584)] {
        assert!((get(&tex, &trilinear, Lod::Auto, at(x, y)) - want).abs() < 0.01, "{x} {y}");
    }
    assert!((get(&tex, &trilinear, Lod::Bias([1.5; 4]), at(2.0, 0.0)) - 2.5).abs() < 0.01);
    assert!((get(&tex, &trilinear, Lod::Level([3.25; 4]), at(9.0, 0.0)) - 3.25).abs() < 0.01);
    assert_eq!(get(&tex, &trilinear, Lod::Zero, at(9.0, 0.0)), 0.0);
    let grad = |x: f32, y: f32| Lod::Grad { dx: [[x / 256.0; 4], [0.0; 4], [0.0; 4]], dy: [[0.0; 4], [y / 256.0; 4], [0.0; 4]] };
    assert!((get(&tex, &trilinear, grad(8.0, 2.0), at(0.0, 0.0)) - 3.0).abs() < 0.01);
    // The sampler's bias and clamps.
    s.bias = -1.0;
    s.lod = (0.5, 3.0);
    let smp = Sampler::from_words(&s.words());
    assert!((get(&tex, &smp, Lod::Auto, at(8.0, 0.0)) - 2.0).abs() < 0.01);
    assert!((get(&tex, &smp, Lod::Auto, at(1.0, 0.0)) - 0.5).abs() < 0.01);
    assert!((get(&tex, &smp, Lod::Level([9.0; 4]), at(1.0, 0.0)) - 3.0).abs() < 0.01);
    // Nearest level, and none.
    s = Smp::new(true, 2);
    s.mip = 1;
    let smp = Sampler::from_words(&s.words());
    for (l, want) in [(0.4, 0.0), (0.6, 1.0), (2.4, 2.0), (2.6, 3.0), (50.0, 8.0)] {
        assert_eq!(get(&tex, &smp, Lod::Level([l; 4]), at(0.0, 0.0)).round(), want);
    }
    s.mip = 0;
    let smp = Sampler::from_words(&s.words());
    assert_eq!(get(&tex, &smp, Lod::Level([5.0; 4]), at(0.0, 0.0)), 0.0);
    // A view of levels 2 to 5: levels count from 2, derivatives are in its first level.
    let (view, _) = levels(2, 5);
    assert!((get(&view, &trilinear, Lod::Level([1.0; 4]), at(0.0, 0.0)) - 3.0).abs() < 0.01);
    assert!((get(&view, &trilinear, Lod::Level([10.0; 4]), at(0.0, 0.0)) - 5.0).abs() < 0.01);
    assert!((get(&view, &trilinear, Lod::Auto, at(8.0, 0.0)) - 3.0).abs() < 0.01);
    assert_eq!(query_dimensions(&view, 0), [64, 64, 1, 4]);
    assert_eq!(query_dimensions(&view, 3), [8, 8, 1, 4]);
    assert_eq!(query_dimensions(&view, 4), [0, 0, 0, 4]);
}

#[test]
fn magnification_and_minification() {
    // Columns 0, 1, 0, 1...: the filter in use shows between two of them.
    let (v, ram) = build(View::new(0x1d, 1, 64, 64, 1), 1, 1, |_, _, x, _| 255 * (x & 1) as u128);
    let tex = Texture::from_words(&v.block_linear()).expect("format");
    let mut s = Smp::new(false, 0);
    s.mag_linear = true;
    let smp = Sampler::from_words(&s.words());
    let mut c = TexCache::new();
    let mut get = |step: f32| {
        let x = 10.0;
        let a = Args { coord: [[x / 64.0, (x + step) / 64.0, x / 64.0, x / 64.0], [0.5; 4], [0.0; 4]], lod: Lod::Auto, ..Args::default() };
        f32::from_bits(sample(&tex, &smp, &mut c, &ram, &a)[0][0])
    };
    assert_eq!(get(0.5), 0.5);
    assert_eq!(get(2.0), 0.0);
}

#[test]
fn anisotropic() {
    let (tex, ram) = levels(0, 8);
    let mut s = Smp::new(true, 0);
    s.mip = 2;
    let mut c = TexCache::new();
    let a = Args {
        coord: [[0.5; 4]; 3],
        lod: Lod::Grad { dx: [[16.0 / 256.0; 4], [0.0; 4], [0.0; 4]], dy: [[0.0; 4], [2.0 / 256.0; 4], [0.0; 4]] },
        ..Args::default()
    };
    // Isotropic: level 4. Up to 4 probes: 16/4 texels each, level 2. 16: the short side's 1.
    for (n, want) in [(1, 4.0), (4, 2.0), (16, 1.0)] {
        s.aniso = n;
        let smp = Sampler::from_words(&s.words());
        assert!((level_of(sample(&tex, &smp, &mut c, &ram, &a)[2]) - want).abs() < 0.01, "{n}");
    }
}

#[test]
fn integer_textures() {
    let mut v = View::new(0x1b, 1, 20, 20, 1);
    v.types = [UINT; 4];
    v.swizzle = [R, ZERO, ONE_INT, R];
    let (v, ram) = build(v, 2, 1, |_, _, x, y| (x * 100 + y) as u128);
    let tex = Texture::from_words(&v.block_linear()).expect("format");
    assert!(tex.integer);
    // Linear filters do not apply; borders are integers.
    let mut s = Smp::new(true, 3);
    s.border = [7, 8, 9, 10];
    let smp = Sampler::from_words(&s.words());
    let mut a = Args::default();
    a.coord[0] = [0.45, 0.99, 1.01, -0.01];
    a.coord[1] = [0.11, 0.5, 0.5, 0.5];
    let got = sample(&tex, &smp, &mut TexCache::new(), &ram, &a);
    assert_eq!(got[0], [902, 0, 1, 902]);
    assert_eq!(got[1], [1910, 0, 1, 1910]);
    assert_eq!(got[2], [7, 8, 9, 10]);
    assert_eq!(got[3], [7, 8, 9, 10]);
}

#[test]
fn depth_comparison() {
    let depth = |x: u32, y: u32| (x * 16 + y) << 8; // of 24 bits
    for (format, shift, types, swizzle) in [
        (0x2b, 0, [UNORM, UINT, UINT, UINT], [R, R, R, ONE]),
        (0x29, 8, [UINT, UNORM, UINT, UINT], [G, G, G, ONE]),
    ] {
        let mut v = View::new(format, 1, 16, 16, 1);
        v.types = types;
        v.swizzle = swizzle;
        let (v, ram) = build(v, 4, 1, |_, _, x, y| ((depth(x, y) as u128) << shift) & 0xffff_ffff);
        let tex = Texture::from_words(&v.block_linear()).expect("format");
        let mut s = Smp::new(false, 2);
        s.compare = Some(3); // less or equal
        let near = Sampler::from_words(&s.words());
        s.mag_linear = true;
        s.min_linear = true;
        let lin = Sampler::from_words(&s.words());
        let mut c = TexCache::new();
        let z = |x: u32, y: u32| depth(x, y) as f32 / 16777215.0;
        let a = Args {
            coord: [[5.5 / 16.0; 4], [6.5 / 16.0; 4], [0.0; 4]],
            compare: Some([z(5, 6), z(5, 6) + 1e-5, z(5, 6) - 1e-5, 2.0]),
            ..Args::default()
        };
        let got = sample(&tex, &near, &mut c, &ram, &a);
        assert_eq!(got.map(|t| f32::from_bits(t[0])), [1.0, 0.0, 1.0, 0.0]);
        assert_eq!(f32::from_bits(got[1][3]), 1.0);
        // Between (5,6), (6,6), (5,7), (6,7): a quarter and three quarters pass.
        let a = Args {
            coord: [[6.0 / 16.0; 4], [7.0 / 16.0; 4], [0.0; 4]],
            compare: Some([z(6, 7), z(6, 6), z(5, 7), z(5, 6)]),
            ..Args::default()
        };
        let got = sample(&tex, &lin, &mut c, &ram, &a).map(|t| f32::from_bits(t[0]));
        assert_eq!(got, [0.25, 0.5, 0.75, 1.0]);
        // Gathered: (x0,y1), (x1,y1), (x1,y0), (x0,y0).
        let got = gather(&tex, &lin, &mut c, &ram, &a, 0, None);
        assert_eq!(got[1].map(f32::from_bits), [0.0, 1.0, 1.0, 0.0]);
        // Without a reference the texture's depth comes back.
        let got = sample(&tex, &near, &mut c, &ram, &Args { compare: None, ..a });
        assert!((f32::from_bits(got[0][0]) - z(6, 7)).abs() < 1e-6);
    }
}

#[test]
fn gathers() {
    let mut v = View::new(8, 5, 16, 16, 3);
    v.swizzle = [B, G, R, A];
    let (v, ram) = build(v, 4, 1, |l, z, x, y| noise(l, z, x, y) as u128);
    let tex = Texture::from_words(&v.block_linear()).expect("format");
    let smp = Sampler::from_words(&Smp::new(false, 0).words());
    let mut c = TexCache::new();
    let t = |x: u32, y: u32, b: usize| (noise(0, 2, x & 15, y & 15).to_le_bytes()[b] as f32 / 255.0).to_bits();
    let mut a = Args { coord: [[4.0 / 16.0; 4], [8.0 / 16.0; 4], [0.0; 4]], layer: [2; 4], ..Args::default() };
    // Component 0 is blue in memory byte 2.
    let got = gather(&tex, &smp, &mut c, &ram, &a, 0, None);
    assert_eq!(got[0], [t(3, 8, 2), t(4, 8, 2), t(4, 7, 2), t(3, 7, 2)]);
    a.offset = [[3; 4], [-9; 4], [0; 4]];
    let got = gather(&tex, &smp, &mut c, &ram, &a, 1, None);
    assert_eq!(got[3], [t(6, 15, 1), t(7, 15, 1), t(7, 14, 1), t(6, 14, 1)]);
    let offs = [[[0, 0], [1, 0], [-5, 2], [0, 7]]; 4];
    a.offset = [[0; 4]; 3];
    let got = gather(&tex, &smp, &mut c, &ram, &a, 3, Some(&offs));
    assert_eq!(got[2], [t(3, 7, 3), t(4, 7, 3), t(14, 9, 3), t(3, 14, 3)]);
    // Inactive lanes.
    a.active = 0b0100;
    let got = gather(&tex, &smp, &mut c, &ram, &a, 3, None);
    assert!(got[0] == [0; 4] && got[2] != [0; 4]);
}

/// Face number and texel of a cube, by its direction: GL's table.
fn gl_cube(d: [f64; 3], n: f64) -> (u32, i64, i64) {
    let a = d.map(f64::abs);
    let (f, sc, tc, ma) = if a[0] >= a[1] && a[0] >= a[2] {
        if d[0] > 0.0 { (0, -d[2], -d[1], a[0]) } else { (1, d[2], -d[1], a[0]) }
    } else if a[1] >= a[2] {
        if d[1] > 0.0 { (2, d[0], d[2], a[1]) } else { (3, d[0], -d[2], a[1]) }
    } else if d[2] > 0.0 {
        (4, d[0], -d[1], a[2])
    } else {
        (5, -d[0], -d[1], a[2])
    };
    let u = |c: f64| (((c / ma + 1.0) / 2.0 * n) as i64).min(n as i64 - 1);
    (f, u(sc), u(tc))
}

#[test]
fn cubes() {
    // Cube 1 of 2, 16x16 faces, red the number of the face and cube.
    let mut v = View::new(8, 8, 16, 16, 12);
    v.last_level = 4;
    v.view_last = 4;
    let (v, ram) = build(v, 4, 1, |l, z, x, y| (z | x << 8 | y << 16 | l << 24) as u128);
    let tex = Texture::from_words(&v.block_linear()).expect("format");
    assert_eq!(tex.layers, 2);
    assert_eq!(query_dimensions(&tex, 1), [8, 8, 2, 5]);
    let smp = Sampler::from_words(&Smp::new(false, 2).words());
    let mut c = TexCache::new();
    let mut rng = Rng(21);
    for _ in 0..2000 {
        let mut a = Args { layer: [0, 1, 1, 9], ..Args::default() };
        for l in 0..4 {
            for k in 0..3 {
                a.coord[k][l] = rng.coord() - 0.5;
            }
        }
        let got = sample(&tex, &smp, &mut c, &ram, &a);
        for l in 0..4 {
            let (f, x, y) = gl_cube([a.coord[0][l] as f64, a.coord[1][l] as f64, a.coord[2][l] as f64], 16.0);
            let t = f32s(got[l]).map(|v| (v * 255.0).round() as u32);
            assert_eq!(t[..3], [f + 6 * a.layer[l].min(1), x as u32, y as u32]);
        }
    }
    // Directions a level-1 texel apart: the level follows, also across an edge.
    let mut s = Smp::new(false, 2);
    s.mip = 1;
    let smp = Sampler::from_words(&s.words());
    let a = Args { coord: [[1.0, 1.0, 1.0, 1.0], [0.9, 0.9, 1.15, 1.15], [0.0, 0.25, 0.0, 0.25]], lod: Lod::Auto, ..Args::default() };
    let got = sample(&tex, &smp, &mut c, &ram, &a);
    assert_eq!((f32::from_bits(got[0][3]) * 255.0).round(), 1.0);
}

#[test]
fn seamless_cubes() {
    // A smooth function of the direction has no seams: compare a linear sample with it.
    let n = 32u32;
    let dir = |f: u32, x: f64, y: f64| -> [f64; 3] {
        let (s, t) = (2.0 * x / n as f64 - 1.0, 2.0 * y / n as f64 - 1.0);
        match f {
            0 => [1.0, -t, -s],
            1 => [-1.0, -t, s],
            2 => [s, 1.0, t],
            3 => [s, -1.0, -t],
            4 => [s, -t, 1.0],
            _ => [-s, -t, -1.0],
        }
    };
    let f = |d: [f64; 3]| {
        let l = (d[0] * d[0] + d[1] * d[1] + d[2] * d[2]).sqrt();
        d.map(|c| c / l * 0.5 + 0.5)
    };
    let mut v = View::new(8, 3, n, n, 6);
    v.types = [UNORM; 4];
    let (v, ram) = build(v, 4, 1, |_, z, x, y| {
        let c = f(dir(z, x as f64 + 0.5, y as f64 + 0.5)).map(|c| (c * 255.0).round() as u128);
        c[0] | c[1] << 8 | c[2] << 16 | 0xff << 24
    });
    let tex = Texture::from_words(&v.block_linear()).expect("format");
    let mut s = Smp::new(true, 0);
    s.seamless = true;
    let smp = Sampler::from_words(&s.words());
    let mut c = TexCache::new();
    let mut rng = Rng(77);
    for i in 0..4000 {
        // Near edges and corners.
        let e = |r: &mut Rng| 1.0 - r.below(1000) as f32 / 15000.0;
        let mut d = [e(&mut rng), e(&mut rng), e(&mut rng)];
        if i % 2 == 0 {
            d[(i / 2) % 3] = rng.coord() - 0.5;
        }
        for v in &mut d {
            if rng.below(2) == 0 {
                *v = -*v;
            }
        }
        let a = Args { coord: d.map(|v| [v; 4]), active: 1, ..Args::default() };
        let got = sample(&tex, &smp, &mut c, &ram, &a)[0];
        let want = f(d.map(|v| v as f64));
        close(got, [want[0], want[1], want[2], 1.0], 0.02, &format!("{d:?}"));
    }
    // With clamping faces the same point shows a seam of half a texel.
    s.seamless = false;
    s.wrap = [2; 3];
    let clamp = Sampler::from_words(&s.words());
    let a = Args { coord: [[1.0; 4], [0.0; 4], [1.0, 0.999, 1.0, 1.0]], ..Args::default() };
    let x = f32::from_bits(sample(&tex, &clamp, &mut c, &ram, &a)[0][0]);
    assert!((x as f64 - f([1.0, 0.0, 1.0])[0]).abs() > 0.003);
}

#[test]
fn multisampled_fetches() {
    for ms in [MsMode::X2, MsMode::X4, MsMode::X8] {
        let (mx, my) = ms.shift();
        let mut v = View::new(8, 5, 50, 30, 2);
        v.ms = ms;
        // Texels are samples: the number of the pixel and where in it.
        let (v, ram) = build(v, 4, 1, |_, l, x, y| (x | y << 8 | l << 16) as u128);
        let tex = Texture::from_words(&v.block_linear()).expect("format");
        assert_eq!(query_dimensions(&tex, 0), [50, 30, 2, 1]);
        assert_eq!(query_type(&tex)[2], ms.samples());
        let mut c = TexCache::new();
        for (x, y) in [(0, 0), (49, 29), (20, 7)] {
            for s in 0..ms.samples() {
                let t = fetch(&tex, &mut c, &ram, [x, y, 0], 1, 0, Some(s));
                let (sx, sy) = ms.sample_offset(s);
                let t = f32s(t).map(|v| (v * 255.0).round() as u32);
                assert_eq!(t[..3], [((x as u32) << mx) + sx, ((y as u32) << my) + sy, 1]);
            }
        }
        assert_eq!(fetch(&tex, &mut c, &ram, [50, 0, 0], 0, 0, Some(0)), [0; 4]);
        // The driver's resolve: the same memory as a texture of samples, still with the
        // sample count in the header, filtered at the middle of a pixel's samples.
        let mut r = View::new(8, 1, 50 << mx, 30 << my, 1);
        (r.ms, r.block, r.scaled) = (ms, v.block, true);
        let tex = Texture::from_words(&r.block_linear()).expect("format");
        let smp = Sampler::from_words(&Smp::new(true, 2).words());
        let (px, py) = (9.0, 5.0);
        let a = Args {
            coord: [[(px + 0.5) * (1 << mx) as f32; 4], [(py + 0.5) * (1 << my) as f32; 4], [0.0; 4]],
            ..Args::default()
        };
        let t = f32s(sample(&tex, &smp, &mut c, &ram, &a)[0]);
        assert!((t[0] * 255.0 - ((px + 0.5) * (1 << mx) as f32 - 0.5)).abs() < 0.01);
        assert!((t[1] * 255.0 - ((py + 0.5) * (1 << my) as f32 - 0.5)).abs() < 0.01);
    }
}

#[test]
fn fetches_by_level() {
    let mut v = View::new(0x0f, 2, 20, 12, 10);
    v.types = [UINT; 4];
    v.last_level = 4;
    v.view_last = 4;
    let (v, ram) = build(v, 4, 1, |l, z, x, y| (x | y << 8 | z << 16 | l << 24) as u128);
    let tex = Texture::from_words(&v.block_linear()).expect("format");
    let mut c = TexCache::new();
    for l in 0..5 {
        let d = query_dimensions(&tex, l);
        assert_eq!(d, [20 >> l, 12 >> l, 10 >> l, 5].map(|v: u32| v.max(1)));
        for p in [[0, 0, 0], [d[0] - 1, d[1] - 1, d[2] - 1], [d[0] / 2, d[1] / 3, d[2] / 2]] {
            let t = fetch(&tex, &mut c, &ram, p.map(|v| v as i32), 0, l, None);
            assert_eq!(t[0], p[0] | p[1] << 8 | p[2] << 16 | l << 24);
        }
        assert_eq!(fetch(&tex, &mut c, &ram, [0, 0, d[2] as i32], 0, l, None)[0], 0);
    }
    assert_eq!(fetch(&tex, &mut c, &ram, [0; 3], 0, 5, None)[0], 0);
}

#[test]
fn lod_queries() {
    let (tex, _) = levels(0, 8);
    let mut s = Smp::new(true, 0);
    s.mip = 2;
    s.lod = (0.0, 3.0);
    s.bias = 0.5;
    let at = |x: f32| Args { coord: [[0.5, 0.5 + x / 256.0, 0.5, 0.5], [0.5; 4], [0.0; 4]], lod: Lod::Auto, ..Args::default() };
    let q = |s: &Smp, x: f32| {
        let t = query_lod(&tex, &Sampler::from_words(&s.words()), &at(x))[0];
        // As the driver's shaders convert them.
        (t[0] as i16 as f32 / 256.0, t[1] as u16 as f32 / 256.0)
    };
    assert_eq!(q(&s, 4.0), (2.5, 2.5));
    assert_eq!(q(&s, 64.0), (6.5, 3.0));
    assert_eq!(q(&s, 0.25), (-1.5, 0.0));
    s.mip = 1;
    assert_eq!(q(&s, 4.0), (2.5, 3.0));
    s.mip = 0;
    assert_eq!(q(&s, 4.0), (2.5, 0.0));
}

#[test]
fn min_max_reduction() {
    let (v, ram) = build(View::new(0x1d, 1, 8, 8, 1), 1, 1, |_, _, x, y| (x * 10 + y) as u128);
    let tex = Texture::from_words(&v.block_linear()).expect("format");
    for (r, want) in [(1, 23.0), (2, 34.0), (0, 28.5)] {
        let mut s = Smp::new(true, 0);
        s.reduction = r;
        let smp = Sampler::from_words(&s.words());
        let a = Args { coord: [[3.0 / 8.0; 4], [4.0 / 8.0; 4], [0.0; 4]], ..Args::default() };
        let t = f32::from_bits(sample(&tex, &smp, &mut TexCache::new(), &ram, &a)[0][0]);
        assert!((t * 255.0 - want).abs() < 0.01, "{r} {t}");
    }
}

#[test]
fn null_and_strange_coordinates() {
    let smp = Sampler::from_words(&Smp::new(true, 0).words());
    let ram = Ram::new(0, 4096);
    let mut c = TexCache::new();
    assert_eq!(sample(&Texture::empty(), &smp, &mut c, &ram, &Args::default()), [[0; 4]; 4]);
    assert_eq!(query_dimensions(&Texture::empty(), 0), [0; 4]);
    for ty in [1, 2, 3] {
        let (v, ram) = build(View::new(8, ty, 16, 16, 6), 4, 1, |_, _, _, _| 0);
        let tex = Texture::from_words(&v.block_linear()).expect("format");
        for wrap in 0..8 {
            for linear in [false, true] {
                let mut s = Smp::new(linear, wrap);
                s.seamless = linear;
                s.aniso = 16;
                s.mip = 2;
                let smp = Sampler::from_words(&s.words());
                for v in [f32::NAN, f32::INFINITY, -f32::INFINITY, 1e30, -1e30, 0.0] {
                    for lod in [Lod::Auto, Lod::Level([v; 4]), Lod::Bias([v; 4])] {
                        let a = Args { coord: [[v, 0.5, 0.25, v]; 3], lod, ..Args::default() };
                        sample(&tex, &smp, &mut c, &ram, &a);
                        gather(&tex, &smp, &mut c, &ram, &a, 0, None);
                        query_lod(&tex, &smp, &a);
                    }
                }
            }
        }
    }
}

#[test]
fn cache_invalidation() {
    let (v, mut ram) = build(View::new(8, 1, 64, 64, 1), 4, 1, |_, _, _, _| 0);
    let tex = Texture::from_words(&v.block_linear()).expect("format");
    let mut c = TexCache::new();
    let get = |c: &mut TexCache, ram: &Ram, x| fetch(&tex, c, ram, [x, 0, 0], 0, 0, None)[0];
    assert_eq!((get(&mut c, &ram, 0), get(&mut c, &ram, 16)), (0, 0));
    ram.data.fill(255);
    let one = 1f32.to_bits();
    assert_eq!((get(&mut c, &ram, 0), get(&mut c, &ram, 16)), (0, 0));
    // The second GOB across is a block (8 GOBs) on.
    c.invalidate_range(v.address + 8 * 512 + 5, 1);
    assert_eq!((get(&mut c, &ram, 0), get(&mut c, &ram, 16)), (0, one));
    c.invalidate();
    assert_eq!((get(&mut c, &ram, 0), get(&mut c, &ram, 16)), (one, one));
}
