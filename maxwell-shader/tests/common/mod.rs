#![allow(clippy::needless_range_loop)]
//! Textures and images with known contents, as the corpus generator makes them: the texel at
//! (x, y, z) of level l has the number x + 16 y + 256 z + 4096 l, in its red component.

use maxwell_shader::isa::*;
use maxwell_shader::{Handle, SurfOp, TexOp};

/// Width, height, depth or layers, levels.
fn size(unit: u32) -> (i32, i32, i32, i32) {
    match unit {
        0 | 5 => (16, 8, 1, 4),
        1 | 6 => (8, 8, 4, 3),
        2 => (8, 8, 8, 3),
        3 | 7 => (8, 8, 6, 3),
        4 => (16, 1, 1, 3),
        8 => (8, 8, 12, 1),
        10 => (64, 1, 1, 1),
        11 => (16, 1, 4, 3),
        _ => (16, 8, 1, 1),
    }
}

fn texel(unit: u32, x: i32, y: i32, z: i32, l: i32) -> [u32; 4] {
    let (w, h, d, _) = size(unit);
    let d = if unit == 2 { (d >> l).max(1) } else { d };
    let (w, h) = ((w >> l).max(1), (h >> l).max(1));
    if x < 0 || y < 0 || z < 0 || x >= w || y >= h || z >= d {
        return [0; 4];
    }
    let id = x + 16 * y + 256 * z + 4096 * l;
    match unit {
        5..=7 => [(((id * 37 % 97) as f32) / 97.0).to_bits(), 0, 0, 0],
        12 => [id as u32, (id + 1) as u32, -id as u32, (id * 2) as u32],
        _ => {
            let f = id as f32;
            [f, f + 0.25, -f, f * 0.5].map(f32::to_bits)
        }
    }
}

fn cube(c: [f32; 3]) -> (i32, f32, f32) {
    let [x, y, z] = c;
    let (ax, ay, az) = (x.abs(), y.abs(), z.abs());
    let (f, s, t, m) = if ax >= ay && ax >= az {
        if x > 0.0 {
            (0, -z, -y, ax)
        } else {
            (1, z, -y, ax)
        }
    } else if ay >= az {
        if y > 0.0 {
            (2, x, z, ay)
        } else {
            (3, x, -z, ay)
        }
    } else if z > 0.0 {
        (4, x, -y, az)
    } else {
        (5, -x, -y, az)
    };
    (f, (s / m + 1.0) * 0.5, (t / m + 1.0) * 0.5)
}

pub fn texture<const N: usize>(unit: u32, t: &TexOp<N>) -> [[u32; N]; 4] {
    if let Handle::Bound(b) = t.handle {
        assert_eq!(b, 8, "texture index");
    }
    let (w, h, d, levels) = size(unit);
    let mut out = [[0; N]; 4];
    if t.kind == TexKind::Query {
        for i in 0..N {
            let l = t.lod[i];
            let dd = if unit == 2 { (d >> l).max(1) } else { d };
            let dd = if matches!(unit, 3 | 7) { 1 } else { dd };
            // The layers come after the coordinates; layers of a cube array are cubes.
            let r = match t.query {
                1 if unit == 11 => [w >> l, d, 1, levels],
                1 if unit == 8 => [w >> l, h >> l, d / 6, levels],
                1 => [(w >> l).max(1), (h >> l).max(1), dd, levels],
                _ => [0, 0, 1, 0],
            };
            for c in 0..4 {
                out[c][i] = r[c] as u32;
            }
        }
        return out;
    }
    let dims = match t.dim {
        TexDim::D1 => 1,
        TexDim::D2 => 2,
        _ => 3,
    };
    assert_eq!(t.array, matches!(unit, 1 | 6 | 8 | 11), "array");
    let want = match unit {
        4 | 10 | 11 => TexDim::D1,
        2 => TexDim::D3,
        3 | 7 | 8 => TexDim::Cube,
        _ => TexDim::D2,
    };
    assert_eq!(t.dim, want);
    assert_eq!(t.shadow, matches!(unit, 5..=7), "shadow");

    // Face and face coordinates per lane.
    let mut co = [[0f32; 3]; N];
    let mut face = [0; N];
    for i in 0..N {
        let c: [f32; 3] = std::array::from_fn(|k| f32::from_bits(t.coord[k][i]));
        co[i] = c;
        if t.dim == TexDim::Cube {
            let (f, s, v) = cube(c);
            face[i] = f;
            co[i] = [s, v, 0.0];
        }
    }
    let scale = [w as f32, h as f32, d as f32];
    let spatial = if t.dim == TexDim::Cube { 2 } else { dims };
    for i in 0..N {
        if t.lanes >> i & 1 == 0 {
            continue;
        }
        let q = i & !3;
        let mut rho = 0f32;
        match t.kind {
            TexKind::Grad => {
                for k in 0..dims.min(2) {
                    rho = rho.max(t.ddx[k][i].abs().max(t.ddy[k][i].abs()) * scale[k]);
                }
            }
            _ => {
                for k in 0..spatial {
                    let dx = (co[q + 1][k] - co[q][k]).abs();
                    let dy = (co[q + 2][k] - co[q][k]).abs();
                    rho = rho.max(dx.max(dy) * scale[k]);
                }
            }
        }
        let given = f32::from_bits(t.lod[i]);
        let lod = match t.lod_mode {
            LodMode::Auto => rho.log2(),
            LodMode::Bias => rho.log2() + given,
            LodMode::Zero => 0.0,
            LodMode::Level => given,
        };
        let top = (levels - 1) as f32;
        if t.kind == TexKind::QueryLod {
            out[0][i] = lod.to_bits();
            out[1][i] = lod.clamp(0.0, top).round().to_bits();
            continue;
        }
        let layer = (t.layer[i] as i32).min(if unit == 8 { 1 } else { d - 1 });
        let off: [i32; 3] = std::array::from_fn(|k| t.offset[k][i]);
        if t.kind == TexKind::Fetch {
            let l = if t.lod_mode == LodMode::Level { t.lod[i] as i32 } else { 0 };
            let c: [i32; 3] = std::array::from_fn(|k| {
                if k < dims {
                    t.coord[k][i] as i32 + off[k]
                } else {
                    0
                }
            });
            let z = if t.array { t.layer[i] as i32 } else { c[2] };
            let r = texel(unit, c[0], c[1], z, l);
            for k in 0..4 {
                out[k][i] = r[k];
            }
            continue;
        }
        let l = lod.clamp(0.0, top).round() as i32;
        let dim = |k: usize| match k {
            0 => (w >> l).max(1),
            1 => (h >> l).max(1),
            _ if unit == 2 => (d >> l).max(1),
            _ => 1,
        };
        let norm = unit != 9;
        let pos = |k: usize| co[i][k] * if norm { dim(k) as f32 } else { 1.0 };
        let z = |zc: i32| match (t.dim, t.array) {
            (TexDim::Cube, false) => face[i],
            (TexDim::Cube, true) => layer * 6 + face[i],
            (_, true) => layer,
            _ => zc,
        };
        let get = |x: i32, y: i32, zc: i32| {
            let r = texel(
                unit,
                x.clamp(0, dim(0) - 1),
                y.clamp(0, dim(1) - 1),
                z(zc.clamp(0, dim(2) - 1)),
                l,
            );
            if t.shadow {
                let v = if t.dref[i] <= f32::from_bits(r[0]) { 1f32 } else { 0.0 }.to_bits();
                [v; 4]
            } else {
                r
            }
        };
        if t.kind == TexKind::Gather {
            for (k, (dx, dy)) in [(0, 1), (1, 1), (1, 0), (0, 0)].into_iter().enumerate() {
                let (ox, oy) = t.gather_offset(i, k).unwrap_or((off[0], off[1]));
                let x0 = (pos(0) - 0.5).floor() as i32 + ox;
                let y0 = (pos(1) - 0.5).floor() as i32 + oy;
                // One offset per texel moves the whole footprint: texel (0, 0) of it counts.
                let (dx, dy) = if t.per_texel.is_some() { (0, 0) } else { (dx, dy) };
                out[k][i] = get(x0 + dx, y0 + dy, 0)[t.comp as usize];
            }
            continue;
        }
        let x = pos(0).floor() as i32 + off[0];
        let y = if spatial > 1 { pos(1).floor() as i32 + off[1] } else { 0 };
        let zc = if spatial > 2 { pos(2).floor() as i32 + off[2] } else { 0 };
        let r = get(x, y, zc);
        for k in 0..4 {
            out[k][i] = r[k];
        }
    }
    out
}

/// The images of the corpus.
pub struct Images {
    /// Texels of the image in use, four words each.
    pub t: Vec<[u32; 4]>,
    pub unit: u32,
}

impl Images {
    pub fn new(unit: u32) -> Self {
        let t = (0..512u32)
            .map(|i| {
                let f = i as f32;
                match unit {
                    0 => [i * 3, 0, 0, 1],
                    1 => [f, f + 0.25, -f, 0.5 * f].map(f32::to_bits),
                    2 => [1000u32.wrapping_sub(i), 0, 0, 1],
                    3 => std::array::from_fn(|k| (i * 4 + k as u32) & 255),
                    4 => [f.to_bits(), (-0.5 * f).to_bits(), 0, 1f32.to_bits()],
                    _ => [i * 7, 0, 0, 1],
                }
            })
            .collect();
        Images { t, unit }
    }

    /// What c[aux][su info] holds.
    pub fn info(&self, o: u32) -> u32 {
        match o {
            0 => 1,
            0x30 => [4, 16, 4, 4, 4, 4][self.unit as usize],
            _ => 0,
        }
    }

    fn at<const N: usize>(&self, s: &SurfOp<N>, i: usize) -> Option<usize> {
        let c: [i32; 3] = std::array::from_fn(|k| s.coord[k][i] as i32);
        let (dim, w) = match self.unit {
            0 | 3 => (SurfDim::D2, 8),
            1 => (SurfDim::D2Array, 8),
            2 => (SurfDim::Buffer, 64),
            4 => (SurfDim::D3, 8),
            _ => (SurfDim::D1, 8),
        };
        assert_eq!(s.dim, dim);
        match s.handle {
            Handle::Bound(b) => assert_eq!(b, 8 + 32),
            Handle::Bindless(h) => assert_eq!(h[i], 8 + 32),
        }
        let d = [1, 4, 1, 1, 8, 1][self.unit as usize];
        let h = if w == 64 || self.unit == 5 { 1 } else { 8 };
        if c[0] < 0 || c[1] < 0 || c[2] < 0 || c[0] >= w || c[1] >= h || c[2] >= d {
            return None;
        }
        Some((c[0] + w * (c[1] + h * c[2])) as usize)
    }

    pub fn load<const N: usize>(&self, s: &SurfOp<N>) -> [[u32; N]; 4] {
        let mut out = [[0; N]; 4];
        for i in 0..N {
            if s.lanes >> i & 1 == 0 {
                continue;
            }
            let Some(a) = self.at(s, i) else { continue };
            let t = self.t[a];
            let r = match (s.raw, self.unit) {
                (true, 3) => [t[0] | t[1] << 8 | t[2] << 16 | t[3] << 24, 0, 0, 0],
                (false, 3) => t.map(|x| (x as f32 / 255.0).to_bits()),
                (true, 4) => {
                    let h = |x| maxwell_shader::fp::f32_to_half(f32::from_bits(x), Rnd::N) as u32;
                    [h(t[0]) | h(t[1]) << 16, 0, 0, 0]
                }
                _ => t,
            };
            for k in 0..4 {
                out[k][i] = r[k];
            }
        }
        out
    }

    pub fn store<const N: usize>(&mut self, s: &SurfOp<N>, d: &[[u32; N]; 4]) {
        for i in 0..N {
            if s.lanes >> i & 1 == 0 {
                continue;
            }
            let Some(a) = self.at(s, i) else { continue };
            let v: [u32; 4] = std::array::from_fn(|k| d[k][i]);
            self.t[a] = match self.unit {
                0 | 2 | 5 => [v[0], 0, 0, 1],
                3 => v.map(|x| (f32::from_bits(x).clamp(0.0, 1.0) * 255.0).round() as u32),
                _ => v,
            };
        }
    }

    pub fn atomic<const N: usize>(
        &mut self,
        s: &SurfOp<N>,
        op: AtomOp,
        ty: AtomTy,
        a: &[u64; N],
        b: &[u64; N],
    ) -> [u64; N] {
        std::array::from_fn(|i| {
            if s.lanes >> i & 1 == 0 {
                return 0;
            }
            let Some(at) = self.at(s, i) else { return 0 };
            let old = self.t[at][0] as u64;
            self.t[at][0] = maxwell_shader::atomic_apply(op, ty, old, a[i], b[i]) as u32;
            old
        })
    }
}
