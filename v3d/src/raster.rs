//! Triangle setup and coverage: edge functions in 1/256-pixel fixed point with the top-left
//! fill rule, barycentric weights at pixel centres (or the 4x MSAA sample positions).

/// A screen-space vertex ready for rasterization.
#[derive(Clone, Debug)]
pub struct RVert {
    pub x: f32,
    pub y: f32,
    /// window depth Zs
    pub z: f32,
    /// 1 / Wc
    pub invw: f32,
    pub vary: Vec<u32>,
}

/// Standard 4x MSAA sample positions within a pixel.
pub const SAMPLES_4X: [(f32, f32); 4] = [(0.375, 0.125), (0.875, 0.375), (0.125, 0.625), (0.625, 0.875)];

pub struct Tri {
    /// fixed point (1/256 px) vertex positions
    p: [(i64, i64); 3],
    /// edge function bias for the top-left rule
    bias: [i64; 3],
    area: i64,
    pub dzdx: f32,
    pub dzdy: f32,
}

impl Tri {
    pub fn new(v: [&RVert; 3]) -> Option<Tri> {
        let fx = |f: f32| (f * 256.0).round() as i64;
        let mut p = [(fx(v[0].x), fx(v[0].y)), (fx(v[1].x), fx(v[1].y)), (fx(v[2].x), fx(v[2].y))];
        let mut area = (p[1].0 - p[0].0) * (p[2].1 - p[0].1) - (p[2].0 - p[0].0) * (p[1].1 - p[0].1);
        if area == 0 { return None; }
        let mut swap = false;
        if area < 0 { p.swap(1, 2); area = -area; swap = true; }
        // edge k is opposite vertex k: from p[(k+1)%3] to p[(k+2)%3]
        let mut bias = [0i64; 3];
        for k in 0..3 {
            let (a, b) = (p[(k + 1) % 3], p[(k + 2) % 3]);
            let (dx, dy) = (b.0 - a.0, b.1 - a.1);
            // with y down and a clockwise (positive area) winding, top edges have dy == 0 && dx > 0,
            // left edges have dy < 0
            let top_left = (dy == 0 && dx > 0) || dy < 0;
            bias[k] = if top_left { 0 } else { -1 };
        }
        let _ = swap;
        // depth slopes for polygon offset
        let (x0, y0, z0) = (v[0].x, v[0].y, v[0].z);
        let (x1, y1, z1) = (v[1].x - x0, v[1].y - y0, v[1].z - z0);
        let (x2, y2, z2) = (v[2].x - x0, v[2].y - y0, v[2].z - z0);
        let det = x1 * y2 - x2 * y1;
        let (dzdx, dzdy) = if det != 0.0 { ((z1 * y2 - z2 * y1) / det, (x1 * z2 - x2 * z1) / det) } else { (0.0, 0.0) };
        Some(Tri { p, bias, area, dzdx, dzdy })
    }

    #[inline]
    fn edge(&self, k: usize, x: i64, y: i64) -> i64 {
        let (a, b) = (self.p[(k + 1) % 3], self.p[(k + 2) % 3]);
        (b.0 - a.0) * (y - a.1) - (b.1 - a.1) * (x - a.0)
    }

    /// Whether the point (in pixels) is covered.
    #[inline]
    pub fn covers(&self, x: f32, y: f32) -> bool {
        let (px, py) = ((x * 256.0).round() as i64, (y * 256.0).round() as i64);
        (0..3).all(|k| self.edge(k, px, py) + self.bias[k] >= 0)
    }

    /// Barycentric weights of the *original* vertex order at (x, y) in pixels.
    pub fn bary(&self, v: [&RVert; 3], x: f32, y: f32) -> [f32; 3] {
        let (x0, y0) = (v[0].x, v[0].y);
        let (x1, y1) = (v[1].x, v[1].y);
        let (x2, y2) = (v[2].x, v[2].y);
        let det = (x1 - x0) * (y2 - y0) - (x2 - x0) * (y1 - y0);
        if det == 0.0 { return [1.0, 0.0, 0.0]; }
        let b1 = ((x - x0) * (y2 - y0) - (x2 - x0) * (y - y0)) / det;
        let b2 = ((x1 - x0) * (y - y0) - (x - x0) * (y1 - y0)) / det;
        [1.0 - b1 - b2, b1, b2]
    }

    pub fn bbox(&self) -> (i64, i64, i64, i64) {
        let xs = [self.p[0].0, self.p[1].0, self.p[2].0];
        let ys = [self.p[0].1, self.p[1].1, self.p[2].1];
        let _ = self.area;
        (xs.iter().min().unwrap() >> 8, ys.iter().min().unwrap() >> 8, (xs.iter().max().unwrap() >> 8) + 1, (ys.iter().max().unwrap() >> 8) + 1)
    }
}
