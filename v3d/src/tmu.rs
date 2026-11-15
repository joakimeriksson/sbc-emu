//! The texture and memory unit: texture lookups configured by the texture shader state and
//! sampler state records (TMU config parameters 0-2), and general memory loads, stores and
//! atomics (TMUA/TMUAU with a lookup config word).

use crate::formats::{decode_texel, srgb_to_linear, tex_fmt, Texel};
use crate::packets::{SamplerState, TextureShaderState};
use crate::qpu::{f32_to_f16, Thread, TmuKind, TmuReq, LANES, V};
use crate::tiling::{self, Level};
use crate::Gmem;

// general lookup types
const T_8I: u32 = 0;
const T_16I: u32 = 1;
const T_VEC2: u32 = 2;
const T_VEC3: u32 = 3;
const T_VEC4: u32 = 4;
const T_8UI: u32 = 5;
const T_16UI: u32 = 6;

fn general(m: &mut Gmem, cfg: u32, req: &TmuReq, lanes: u16) -> Vec<V> {
    let ty = cfg & 7;
    let op = (cfg >> 3) & 0xf;
    let addr = req.s.unwrap_or([0; LANES]);
    let data = &req.data;
    if op == 15 {
        if data.is_empty() {
            // load
            let words = match ty { T_VEC2 => 2, T_VEC3 => 3, T_VEC4 => 4, _ => 1 };
            let mut out = vec![[0u32; LANES]; words];
            for l in 0..LANES {
                if lanes & (1 << l) == 0 { continue; }
                let a = addr[l];
                match ty {
                    T_8I => out[0][l] = m.r8(a) as i8 as i32 as u32,
                    T_8UI => out[0][l] = m.r8(a) as u32,
                    T_16I => out[0][l] = m.r16(a) as i16 as i32 as u32,
                    T_16UI => out[0][l] = m.r16(a) as u32,
                    _ => for k in 0..words { out[k][l] = m.r32(a.wrapping_add(4 * k as u32)); },
                }
            }
            return out;
        }
        // store
        for l in 0..LANES {
            if lanes & (1 << l) == 0 { continue; }
            let a = addr[l];
            match ty {
                T_8I | T_8UI => m.w8(a, data[0][l] as u8),
                T_16I | T_16UI => m.w16(a, data[0][l] as u16),
                _ => for (k, d) in data.iter().enumerate() { m.w32(a.wrapping_add(4 * k as u32), d[l]); },
            }
        }
        return Vec::new();
    }
    if data.is_empty() {
        // read-type operations: increment / decrement / not, or cache maintenance (no data)
        if !matches!(op, 8..=10) { return Vec::new(); }
        let mut out = [0u32; LANES];
        for l in 0..LANES {
            if lanes & (1 << l) == 0 { continue; }
            let old = m.r32(addr[l]);
            out[l] = old;
            m.w32(addr[l], match op { 8 => old.wrapping_add(1), 9 => old.wrapping_sub(1), _ => !old });
        }
        return vec![out];
    }
    // atomics, applied lane by lane
    let mut out = [0u32; LANES];
    for l in 0..LANES {
        if lanes & (1 << l) == 0 { continue; }
        let a = addr[l];
        let old = m.r32(a);
        let d = data[0][l];
        let new = match op {
            0 => old.wrapping_add(d),
            1 => old.wrapping_sub(d),
            2 => d,
            3 => if old == d { data.get(1).map_or(d, |v| v[l]) } else { old },
            4 => old.min(d),
            5 => old.max(d),
            6 => (old as i32).min(d as i32) as u32,
            7 => (old as i32).max(d as i32) as u32,
            8 => old & d,
            9 => old | d,
            _ => old ^ d,
        };
        m.w32(a, new);
        out[l] = old;
    }
    vec![out]
}

thread_local! {
    static TEX_CACHE: std::cell::RefCell<std::collections::HashMap<(u32, u32, u8), std::rc::Rc<Tex>>> = Default::default();
}

/// Forget the cached texture setups (at the start of each job).
pub fn clear_cache() { TEX_CACHE.with(|c| c.borrow_mut().clear()); }

/// Texture state and sampler for one lookup.
struct Tex {
    ts: TextureShaderState,
    ss: Option<SamplerState>,
    fmt: crate::formats::TexFmt,
    base: u32,
    levels: Vec<Level>,
    one_d: bool,
    is_3d: bool,
    srgb: bool,
    /// the texture and sampler records it was built from
    raw: [u8; 48],
}

impl Tex {
    fn level_addr(&self, l: usize, layer: u32, z: u32) -> u32 {
        let lv = &self.levels[l.min(self.levels.len() - 1)];
        let mut a = (self.base as i64 + lv.offset) as u32;
        if self.is_3d { a = a.wrapping_add(z * lv.size); } else { a = a.wrapping_add(layer * self.ts.array_stride_64_byte_aligned * 64); }
        a
    }

    /// Texel (x, y) of level l (already wrapped into range).
    fn fetch(&self, m: &mut Gmem, l: usize, x: u32, y: u32, layer: u32, z: u32) -> Texel {
        let f = self.fmt;
        let a = self.level_addr(l, layer, z);
        if self.one_d {
            let mut b = [0u8; 16];
            m.read(a.wrapping_add(x * f.cpp), &mut b[..f.cpp as usize]);
            return decode_texel(self.ts.texture_type, &b);
        }
        let lv = &self.levels[l.min(self.levels.len() - 1)];
        let (bx, by) = (x / f.bw, y / f.bh);
        let off = tiling::offset(lv.tiling, f.cpp, lv.stride, lv.ph * f.bh.max(1) / f.bh.max(1), bx, by);
        let mut b = [0u8; 16];
        m.read(a.wrapping_add(off), &mut b[..f.cpp as usize]);
        if f.bw > 1 { return crate::formats::decode_block(self.ts.texture_type, &b, x % f.bw, y % f.bh); }
        decode_texel(self.ts.texture_type, &b)
    }
}

fn wrap(c: i32, size: i32, mode: u32) -> Option<i32> {
    match mode {
        0 => Some(c.rem_euclid(size)),
        2 => {
            let p = c.rem_euclid(2 * size);
            Some(if p >= size { 2 * size - 1 - p } else { p })
        }
        3 => if c < 0 || c >= size { None } else { Some(c) },
        4 => { let c = if c < 0 { -c - 1 } else { c }; Some(c.min(size - 1)) }
        _ => Some(c.clamp(0, size - 1)),
    }
}

fn cube_face(x: f32, y: f32, z: f32) -> (u32, f32, f32) {
    let (ax, ay, az) = (x.abs(), y.abs(), z.abs());
    let (face, sc, tc, ma) = if ax >= ay && ax >= az {
        if x >= 0.0 { (0, -z, -y, ax) } else { (1, z, -y, ax) }
    } else if ay >= az {
        if y >= 0.0 { (2, x, z, ay) } else { (3, x, -z, ay) }
    } else if z >= 0.0 { (4, x, -y, az) } else { (5, -x, -y, az) };
    let ma = if ma == 0.0 { 1.0 } else { ma };
    (face, (sc / ma + 1.0) * 0.5, (tc / ma + 1.0) * 0.5)
}

fn compare(func: u32, dref: f32, d: f32) -> f32 {
    let r = match func { 0 => false, 1 => dref < d, 2 => dref == d, 3 => dref <= d, 4 => dref > d, 5 => dref != d, 6 => dref >= d, _ => true };
    r as u32 as f32
}

/// A texture lookup for every lane in `lanes`.
pub fn texture(m: &mut Gmem, t: &Thread, kind: TmuKind, cfg: &[u32], req: &TmuReq, lanes: u16, fs: bool) -> Vec<V> {
    let p0 = cfg.first().copied().unwrap_or(0);
    let p1 = cfg.get(1).copied().unwrap_or(1 << 2);
    let p2 = cfg.get(2).copied().unwrap_or(15 << 20);
    let ret_mask = p0 & 0xf;
    let out32 = p1 & 1 != 0;
    let unnorm = p1 & 2 != 0;
    let ts_addr = p0 & !0xf;
    let ss_addr = p1 & !0x7;
    let gather = p2 & (1 << 7) != 0;
    let gather_comp = (p2 >> 5) & 3;
    let disable_autolod = p2 & 2 != 0;
    let sx4 = |v: u32| ((v << 28) as i32 >> 28);
    let (off_s, off_t, off_r) = (sx4((p2 >> 8) & 0xf), sx4((p2 >> 12) & 0xf), sx4((p2 >> 16) & 0xf));
    let one_d = req.t.is_none() && kind != TmuKind::TexCube;
    let is_3d = req.r.is_some() && kind != TmuKind::TexCube;
    // the parsed records and the mip chain, cached for the job by address and checked against
    // the records' bytes: a job can reuse a record's memory for another texture (Mesa's PBO
    // uploads create and free a buffer-texture view per upload, all in one job)
    let key = (ts_addr, ss_addr, one_d as u8 | (is_3d as u8) << 1);
    let mut raw = [0u8; 48];
    raw[..24].copy_from_slice(&m.bytes(ts_addr, TextureShaderState::LEN));
    if ss_addr != 0 { raw[24..].copy_from_slice(&m.bytes(ss_addr, SamplerState::LEN)); }
    let tex = TEX_CACHE.with(|c| c.borrow().get(&key).filter(|t| t.raw == raw).cloned()).unwrap_or_else(|| {
        let ts = TextureShaderState::parse(&raw[..24]);
        let ss = if ss_addr != 0 { Some(SamplerState::parse(&raw[24..])) } else { None };
        let fmt = tex_fmt(ts.texture_type);
        let width = if one_d { ts.image_width | ts.image_height << 14 } else { ts.image_width };
        let height = if one_d { 1 } else { ts.image_height.max(1) };
        let depth = ts.image_depth.max(1);
        let levels = if one_d {
            vec![Level { offset: 0, tiling: tiling::Tiling::Raster, pw: width, ph: 1, w: width, h: 1, d: 1, stride: width * fmt.cpp, size: width * fmt.cpp }]
        } else {
            tiling::mip_chain(width.max(1), height, if is_3d { depth } else { 1 }, fmt.cpp, fmt.bw, fmt.bh, ts.max_level, is_3d,
                              ts.level_0_is_strictly_uif, ts.level_0_xor_enable, ts.level_0_ub_pad)
        };
        let srgb = ts.srgb && !ss.map_or(false, |s| s.srgb_disable);
        if crate::draws_enabled() {
            crate::log_once(&format!("v3d texture: type {} {}x{}x{} levels {} tiling {:?} xor {} ubpad {} swz {:?} {} {} srgb {} sampler {} kind {:?} out32 {} ret {:#x}",
                ts.texture_type, width, height, depth, levels.len(), levels[0].tiling, ts.level_0_xor_enable as u8, ts.level_0_ub_pad,
                [ts.swizzle_r, ts.swizzle_g, ts.swizzle_b, ts.swizzle_a], if fs { "fs" } else { "vs" }, if one_d { "1d" } else if is_3d { "3d" } else { "2d" },
                srgb as u8, ss.is_some() as u8, kind, out32 as u8, ret_mask));
        }
        let t = std::rc::Rc::new(Tex { ts, ss, fmt, base: ts.texture_base_pointer & !0x3f, levels, one_d, is_3d, srgb, raw });
        TEX_CACHE.with(|c| c.borrow_mut().insert(key, t.clone()));
        t
    });
    let fmt = tex.fmt;
    let width = if one_d { tex.ts.image_width | tex.ts.image_height << 14 } else { tex.ts.image_width };
    let height = if one_d { 1 } else { tex.ts.image_height.max(1) };
    let depth = tex.ts.image_depth.max(1);

    let (mag_nearest, min_nearest, mip_nearest, wrap_s, wrap_t, wrap_r, min_lod, max_lod, bias, cmp_fn) = match &tex.ss {
        Some(s) => (s.mag_filter_nearest, s.min_filter_nearest, s.mip_filter_nearest, s.wrap_s, s.wrap_t, s.wrap_r,
                    s.min_level_of_detail, s.max_level_of_detail, s.fixed_bias, s.depth_compare_function),
        None => (true, true, true, 1, 1, 1, 0.0, 15.0, 0.0, 7),
    };
    let border = |k: usize| -> u32 {
        match tex.ss { Some(s) => match s.border_color_mode {
            0 => 0, 1 => if k == 3 { if fmt.int { 1 } else { 1f32.to_bits() } } else { 0 },
            2 => if fmt.int { 1 } else { 1f32.to_bits() },
            _ => [s.border_color_word_0, s.border_color_word_1, s.border_color_word_2, s.border_color_word_3][k],
        }, None => 0 }
    };

    let zero = [0u32; LANES];
    let s = req.s.unwrap_or(zero);
    let tq = req.t.unwrap_or(zero);
    let rq = req.r.unwrap_or(zero);
    let iq = req.i.unwrap_or(zero);
    let bq = req.b.unwrap_or(zero);
    let off = req.off;
    let base_level = tex.ts.base_level as f32;
    let nlev = tex.levels.len();
    let mut results = [[0u32; 4]; LANES];

    // per-lane normalised coordinates (u, v, w in texels of level 0), layer, and the face
    let mut uvw = [[0f32; 3]; LANES];
    let mut layer = [0u32; LANES];
    for l in 0..LANES {
        let fl = |v: &V| f32::from_bits(v[l]);
        match kind {
            TmuKind::TexCube => {
                let (face, sc, tc) = cube_face(fl(&s), fl(&tq), fl(&rq));
                uvw[l] = [sc * width as f32, tc * height as f32, 0.0];
                let li = if req.i.is_some() { fl(&iq).round().max(0.0) as u32 } else { 0 };
                layer[l] = face + 6 * li;
            }
            TmuKind::TexFetch => {
                uvw[l] = [s[l] as i32 as f32, tq[l] as i32 as f32, rq[l] as i32 as f32];
                layer[l] = if req.i.is_some() { iq[l] } else { 0 };
            }
            _ => {
                let (fs_, ft, fr) = (fl(&s), fl(&tq), fl(&rq));
                uvw[l] = if unnorm { [fs_, ft, fr] } else { [fs_ * width as f32, ft * height as f32, fr * depth as f32] };
                layer[l] = if req.i.is_some() { (fl(&iq).round().max(0.0) as u32).min(depth - 1) } else { 0 };
            }
        }
    }

    for l in 0..LANES {
        if lanes & (1 << l) == 0 && !fs { continue; }
        // level of detail
        let lambda = match kind {
            TmuKind::TexFetch => 0.0,
            TmuKind::TexLod => f32::from_bits(bq[l]),
            _ if disable_autolod => f32::from_bits(bq[l]),
            _ => {
                let mut lod = if fs && !one_d || fs {
                    let q = l & !3;
                    let d = |a: usize, b: usize, k: usize| uvw[a][k] - uvw[b][k];
                    let (x0, x1, y1) = (q | (l & 2), q | 1 | (l & 2), q | 2 | (l & 1));
                    let y0 = q | (l & 1);
                    let dx = (d(x1, x0, 0).powi(2) + d(x1, x0, 1).powi(2) + if is_3d { d(x1, x0, 2).powi(2) } else { 0.0 }).sqrt();
                    let dy = (d(y1, y0, 0).powi(2) + d(y1, y0, 1).powi(2) + if is_3d { d(y1, y0, 2).powi(2) } else { 0.0 }).sqrt();
                    let rho = dx.max(dy);
                    if rho > 0.0 { rho.log2() } else { -16.0 }
                } else { 0.0 };
                if req.b.is_some() { lod += f32::from_bits(bq[l]); }
                lod
            }
        } + bias;
        let fetch_level = |lv: f32| -> usize { ((lv + base_level).max(0.0) as usize).min(nlev - 1) };
        let mut sample_level = |lv: usize, nearest: bool| -> [u32; 4] {
            let lvw = tex.levels[lv].w.max(1) as f32;
            let lvh = tex.levels[lv].h.max(1) as f32;
            let lvd = tex.levels[lv].d.max(1) as f32;
            let scale = if kind == TmuKind::TexFetch { 1.0 } else { 1.0 / (1u32 << lv) as f32 };
            let (mut u, mut v, mut w) = (uvw[l][0] * scale, uvw[l][1] * scale, uvw[l][2] * scale);
            if kind == TmuKind::TexFetch { u = uvw[l][0]; v = uvw[l][1]; w = uvw[l][2]; }
            let (mut os, mut ot, mut or) = (off_s, off_t, off_r);
            if let Some(o) = off { os += sx4(o[l] & 0xf); ot += sx4((o[l] >> 4) & 0xf); or += sx4((o[l] >> 8) & 0xf); }
            let (iw, ih, id) = (lvw as i32, if one_d { 1 } else { lvh as i32 }, lvd as i32);
            let get = |m: &mut Gmem, x: i32, y: i32, z: i32| -> Option<Texel> {
                let (ws, wt) = if kind == TmuKind::TexCube { (1, 1) } else { (wrap_s, wrap_t) };
                let x = wrap(x, iw, ws)?;
                let y = if one_d { 0 } else { wrap(y, ih, wt)? };
                let z = if is_3d { wrap(z, id, wrap_r)? } else { 0 };
                let mut t = tex.fetch(m, lv, x as u32, y as u32, layer[l], z as u32);
                if tex.srgb && !t.int { for k in 0..3 { t.v[k] = srgb_to_linear(t.ch(k)).to_bits(); } }
                Some(t)
            };
            let bt = |m: &mut Gmem, x: i32, y: i32, z: i32| -> [u32; 4] {
                match get(m, x, y, z) { Some(t) => t.v, None => [border(0), border(1), border(2), border(3)] }
            };
            if kind == TmuKind::TexFetch {
                let (x, y, z) = (u as i32 + os, v as i32 + ot, w as i32 + or);
                if x < 0 || x >= iw || y < 0 || y >= ih.max(1) { return [0; 4]; }
                return tex.fetch(m, lv, x as u32, y as u32, layer[l], z.max(0) as u32).v;
            }
            let dref = req.dref.map(|d| f32::from_bits(d[l]));
            if nearest || fmt.int {
                let (x, y, z) = ((u.floor() as i32) + os, (v.floor() as i32) + ot, (w.floor() as i32) + or);
                let mut c = bt(m, x, y, z);
                if let Some(dr) = dref { let r = compare(cmp_fn, dr, f32::from_bits(c[0])); c = [r.to_bits(), r.to_bits(), r.to_bits(), 1f32.to_bits()]; }
                return c;
            }
            // bilinear (trilinear in z for 3D textures)
            let (fu, fv, fw) = (u - 0.5, v - 0.5, w - 0.5);
            let (x0, y0, z0) = (fu.floor() as i32 + os, fv.floor() as i32 + ot, fw.floor() as i32 + or);
            let (a, b, c3) = (fu - fu.floor(), fv - fv.floor(), fw - fw.floor());
            let mut acc = [0f32; 4];
            let mut g = [0f32; 4];
            let zs: &[(i32, f32)] = if is_3d { &[(0, 1.0 - c3), (1, c3)] } else { &[(0, 1.0)] };
            for &(dz, wz) in zs {
                for (k, (dx, dy, wxy)) in [(0, 1, (1.0 - a) * b), (1, 1, a * b), (1, 0, a * (1.0 - b)), (0, 0, (1.0 - a) * (1.0 - b))].into_iter().enumerate() {
                    let t = bt(m, x0 + dx, y0 + dy, z0 + dz);
                    let mut ch = [0f32; 4];
                    for q in 0..4 { ch[q] = f32::from_bits(t[q]); }
                    if let Some(dr) = dref { let r = compare(cmp_fn, dr, ch[0]); ch = [r, r, r, 1.0]; }
                    if dz == 0 { g[k] = ch[(gather_comp as usize).min(3)]; }
                    for q in 0..4 { acc[q] += ch[q] * wxy * wz; }
                }
            }
            if gather { return [g[0].to_bits(), g[1].to_bits(), g[2].to_bits(), g[3].to_bits()]; }
            [acc[0].to_bits(), acc[1].to_bits(), acc[2].to_bits(), acc[3].to_bits()]
        };
        let magnify = lambda <= 0.0 || kind == TmuKind::TexFetch;
        let texel = if kind == TmuKind::TexFetch {
            let lv = ((bq[l] as i32).max(0) as f32 + base_level) as usize;
            sample_level(lv.min(nlev - 1), true)
        } else if magnify || gather {
            sample_level(fetch_level(0.0), if gather { false } else { mag_nearest })
        } else {
            let lam = lambda.clamp(min_lod, max_lod.max(min_lod));
            if mip_nearest || nlev == 1 {
                sample_level(fetch_level((lam + 0.5).floor()), min_nearest)
            } else {
                let l0 = fetch_level(lam.floor());
                let l1 = fetch_level(lam.floor() + 1.0);
                let f = lam - lam.floor();
                let a = sample_level(l0, min_nearest);
                if l1 == l0 || f == 0.0 { a } else {
                    let b = sample_level(l1, min_nearest);
                    let mut o = [0u32; 4];
                    for q in 0..4 { o[q] = (f32::from_bits(a[q]) * (1.0 - f) + f32::from_bits(b[q]) * f).to_bits(); }
                    o
                }
            }
        };
        // swizzle (0, 1, R, G, B, A)
        let sw = [tex.ts.swizzle_r, tex.ts.swizzle_g, tex.ts.swizzle_b, tex.ts.swizzle_a];
        let one = if fmt.int { 1 } else { 1f32.to_bits() };
        for k in 0..4 {
            results[l][k] = if gather { texel[k] } else { match sw[k] { 0 => 0, 1 => one, s => texel[(s - 2) as usize & 3] } };
        }
    }

    // return words
    let words: Vec<usize> = (0..4).filter(|k| ret_mask & (1 << k) != 0).collect();
    let mut out = Vec::new();
    for &w in &words {
        let mut v = [0u32; LANES];
        for l in 0..LANES {
            let r = &results[l];
            v[l] = if out32 { r[w] } else {
                let h = |k: usize| -> u32 { if fmt.int { r[k] & 0xffff } else { f32_to_f16(f32::from_bits(r[k])) as u32 } };
                if w < 2 { h(w * 2) | h(w * 2 + 1) << 16 } else { 0 }
            };
        }
        out.push(v);
    }
    out
}

pub fn lookup(m: &mut Gmem, t: &Thread, kind: TmuKind, cfg: &[u32], req: &TmuReq, lanes: u16, fs: bool) -> Vec<V> {
    if kind == TmuKind::General { general(m, cfg.first().copied().unwrap_or(!0), req, lanes) } else { texture(m, t, kind, cfg, req, lanes, fs) }
}
