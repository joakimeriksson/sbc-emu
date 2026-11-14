//! The render pipeline (control list thread 1): runs the render control list tile by tile:
//! tile buffer loads and stores (with format conversion between memory layouts and the
//! tile buffer's internal types), clears, and for each tile the primitives the binner sorted
//! into it: vertex shading, rasterization into 2x2 quads, early/late depth and stencil
//! tests, fragment shading on 16-lane QPU threads, and tile buffer writes with blending.

use std::collections::HashMap;
use std::sync::Arc;

use crate::bin::{self, Binned, ClipVert, DrawState, Prim};
use crate::formats::{self, it};
use crate::packets::*;
use crate::qpu::{f16_to_f32, f32_to_f16, Env, Thread, TmuKind, TmuReq, LANES, V};
use crate::raster::{RVert, Tri, SAMPLES_4X};
use crate::tiling::{self, Tiling};
use crate::{BinResults, Code, Gmem};

#[derive(Clone, Copy, Default)]
struct RtCfg { bpp: u32, ty: u32, clamp: u32, clear: [u32; 4] }

struct TileBuf {
    w: u32,
    h: u32,
    ns: u32,
    color: [Vec<[u32; 4]>; 4],
    z: Vec<f32>,
    s: Vec<u8>,
}

impl TileBuf {
    fn new(w: u32, h: u32, ns: u32) -> TileBuf {
        let n = (w * h * ns) as usize;
        TileBuf { w, h, ns, color: [vec![[0; 4]; n], vec![[0; 4]; n], vec![[0; 4]; n], vec![[0; 4]; n]], z: vec![1.0; n], s: vec![0; n] }
    }
    #[inline] fn idx(&self, x: u32, y: u32, s: u32) -> usize { ((y * self.w + x) * self.ns + s) as usize }
}

struct Render<'r> {
    binned: &'r BinResults,
    cur: Option<Arc<Binned>>,
    width: u32,
    height: u32,
    msaa: bool,
    nrt: u32,
    rt: [RtCfg; 4],
    depth_type: u32,
    early_ds_clear: bool,
    z_clear: f32,
    s_clear: u8,
    tile_w: u32,
    tile_h: u32,
    tb: TileBuf,
    tile: (u32, u32),
    generic: (u32, u32),
    supertile: MulticoreRenderingSupertileCfg,
    /// vertex shader outputs per draw of the current bin result
    vs_cache: HashMap<u32, Arc<Vec<Vec<u32>>>>,
    occlusion: HashMap<u32, u32>,
    insns: u64,
}

pub fn run(start: u32, end: u32, binned: &BinResults, m: &mut Gmem, code: &mut Code, insns: &mut u64) {
    let mut r = Render {
        binned, cur: None, width: 0, height: 0, msaa: false, nrt: 1, rt: [RtCfg::default(); 4], depth_type: 0, early_ds_clear: false,
        z_clear: 1.0, s_clear: 0, tile_w: 64, tile_h: 64, tb: TileBuf::new(64, 64, 1), tile: (0, 0), generic: (0, 0),
        supertile: MulticoreRenderingSupertileCfg::default(), vs_cache: HashMap::new(), occlusion: HashMap::new(), insns: 0,
    };
    let mut cl = bin::Cl::new(start, end);
    while let Some((op, b)) = cl.next(m) {
        if op == 13 { break; }
        r.packet(op, &b, m, code);
    }
    for (addr, n) in r.occlusion.drain() { let v = m.r32(addr); m.w32(addr, v.wrapping_add(n)); }
    *insns += r.insns;
}

impl Render<'_> {
    fn packet(&mut self, op: u8, b: &[u8; 32], m: &mut Gmem, code: &mut Code) {
        match op {
            121 => match b[1] & 0xf {
                0 => {
                    let c = TileRenderingModeCfgCommon::parse(b);
                    self.width = c.image_width_pixels;
                    self.height = c.image_height_pixels;
                    self.msaa = c.multisample_mode_4x;
                    self.nrt = c.number_of_render_targets;
                    self.depth_type = c.internal_depth_type;
                    self.early_ds_clear = c.early_depth_stencil_clear;
                    let (tw, th) = bin::tile_size(c.number_of_render_targets, c.maximum_bpp_of_all_render_targets, c.multisample_mode_4x, c.double_buffer_in_non_ms_mode);
                    self.tile_w = tw;
                    self.tile_h = th;
                    self.tb = TileBuf::new(tw, th, if self.msaa { 4 } else { 1 });
                }
                1 => {
                    let c = TileRenderingModeCfgColor::parse(b);
                    let v = [(c.render_target_0_internal_bpp, c.render_target_0_internal_type, c.render_target_0_clamp),
                             (c.render_target_1_internal_bpp, c.render_target_1_internal_type, c.render_target_1_clamp),
                             (c.render_target_2_internal_bpp, c.render_target_2_internal_type, c.render_target_2_clamp),
                             (c.render_target_3_internal_bpp, c.render_target_3_internal_type, c.render_target_3_clamp)];
                    for k in 0..4 { self.rt[k].bpp = v[k].0; self.rt[k].ty = v[k].1; self.rt[k].clamp = v[k].2; }
                }
                2 => {
                    let c = TileRenderingModeCfgZsClearValues::parse(b);
                    self.z_clear = c.z_clear_value;
                    self.s_clear = c.stencil_clear_value as u8;
                }
                3 => {
                    let c = TileRenderingModeCfgClearColorsPart1::parse(b);
                    let rt = &mut self.rt[(c.render_target_number & 3) as usize];
                    rt.clear[0] = c.clear_color_low_32_bits;
                    rt.clear[1] = (rt.clear[1] & 0xff00_0000) | c.clear_color_next_24_bits;
                }
                4 => {
                    let c = TileRenderingModeCfgClearColorsPart2::parse(b);
                    let rt = &mut self.rt[(c.render_target_number & 3) as usize];
                    rt.clear[1] = (rt.clear[1] & 0xff_ffff) | (c.clear_color_mid_low_32_bits << 24);
                    rt.clear[2] = (c.clear_color_mid_low_32_bits >> 8) | (c.clear_color_mid_high_24_bits << 24);
                    rt.clear[3] = (rt.clear[3] & 0xffff_0000) | (c.clear_color_mid_high_24_bits >> 8);
                }
                5 => {
                    let c = TileRenderingModeCfgClearColorsPart3::parse(b);
                    let rt = &mut self.rt[(c.render_target_number & 3) as usize];
                    rt.clear[3] = (rt.clear[3] & 0xffff) | (c.clear_color_high_16_bits << 16);
                }
                _ => {}
            },
            123 => {
                let c = MulticoreRenderingTileListSetBase::parse(b);
                self.cur = self.binned.get(&c.address).cloned();
                self.vs_cache.clear();
                if self.cur.is_none() { crate::log(&format!("v3d: render job: no binned primitives for tile lists at {:#x}", c.address)); }
            }
            122 => self.supertile = MulticoreRenderingSupertileCfg::parse(b),
            124 => { let c = TileCoordinates::parse(b); self.start_tile(c.tile_column_number, c.tile_row_number); }
            125 => {}
            20 => { let c = StartAddressOfGenericTileList::parse(b); self.generic = (c.start, c.end); }
            23 => {
                let c = SupertileCoordinates::parse(b);
                let (sw, sh) = (self.supertile.supertile_width_in_tiles.max(1), self.supertile.supertile_height_in_tiles.max(1));
                let (tx_n, ty_n) = ((self.width + self.tile_w - 1) / self.tile_w, (self.height + self.tile_h - 1) / self.tile_h);
                for ty in c.row_number_in_supertiles * sh..(c.row_number_in_supertiles + 1) * sh {
                    for tx in c.column_number_in_supertiles * sw..(c.column_number_in_supertiles + 1) * sw {
                        if tx >= tx_n || ty >= ty_n { continue; }
                        self.start_tile(tx, ty);
                        let mut sub = bin::Cl::new(self.generic.0, self.generic.1);
                        while let Some((op, b)) = sub.next(m) {
                            if op == 27 { break; }
                            self.packet(op, &b, m, code);
                        }
                    }
                }
            }
            30 => self.load(&LoadTileBufferGeneral::parse(b), m),
            29 => self.store(&StoreTileBufferGeneral::parse(b), m),
            25 => { let c = ClearTileBuffers::parse(b); self.clear(c.clear_all_render_targets, c.clear_z_stencil_buffer); }
            21 => self.render_tile(m, code),
            26 | 27 | 56 | 54 | 19 | 126 => {}
            _ => {}
        }
    }

    fn start_tile(&mut self, x: u32, y: u32) {
        self.tile = (x, y);
        if self.early_ds_clear { self.clear(false, true); }
    }

    fn clear(&mut self, rts: bool, zs: bool) {
        if rts {
            for k in 0..4 {
                let c = self.rt[k].clear;
                let ty = self.rt[k].ty;
                // clear colours are given in the internal format: f16 or f32 or integer words
                let v = clear_value(ty, self.rt[k].bpp, c);
                self.tb.color[k].iter_mut().for_each(|p| *p = v);
            }
        }
        if zs {
            let z = quantize_depth(self.depth_type, self.z_clear);
            self.tb.z.iter_mut().for_each(|p| *p = z);
            let s = self.s_clear;
            self.tb.s.iter_mut().for_each(|p| *p = s);
        }
    }

    fn surface(&self, mf: u32, fmt_cpp: u32, hub: u32) -> (Tiling, u32, u32) {
        let t = Tiling::from_memory_format(mf);
        match t {
            Tiling::Raster => (t, hub, 0),
            Tiling::UifNoXor | Tiling::UifXor => (t, 0, hub * 2 * tiling::utile_h(fmt_cpp)),
            _ => {
                // LT / UB-linear: the row pitch follows from the image width
                let ubw = tiling::utile_w(fmt_cpp) * 2;
                let w = (self.width * if self.msaa { 2 } else { 1 } + ubw - 1) / ubw * ubw;
                (t, w * fmt_cpp, 0)
            }
        }
    }

    fn load(&mut self, c: &LoadTileBufferGeneral, m: &mut Gmem) {
        let buf = c.buffer_to_load;
        if buf == 8 { return; }
        let fmt = c.input_image_format;
        let cpp = formats::out_cpp(fmt);
        if cpp == 0 { formats::decode(fmt, &[]); return; }
        let (t, stride, ih) = self.surface(c.memory_format, cpp, c.height_in_ub_or_stride);
        let all = c.decimate_mode == 3 && self.msaa;
        let (x0, y0) = (self.tile.0 * self.tile_w, self.tile.1 * self.tile_h);
        let mut px = [0u8; 16];
        for y in 0..self.tile_h {
            for x in 0..self.tile_w {
                let (gx, gy) = (x0 + x, y0 + y);
                if gx >= self.width || gy >= self.height { continue; }
                for s in 0..self.tb.ns {
                    let (mx, my) = if all { (gx * 2 + (s & 1), gy * 2 + (s >> 1)) } else { (gx, gy) };
                    let my = if c.flip_y { c.height.saturating_sub(1).saturating_sub(my) } else { my };
                    let a = c.address.wrapping_add(tiling::offset(t, cpp, stride, ih, mx, my));
                    m.read(a, &mut px[..cpp as usize]);
                    let i = self.tb.idx(x, y, s);
                    match buf {
                        0..=3 => {
                            let mut v = formats::decode(fmt, &px);
                            if c.r_b_swap { v.swap(0, 2); }
                            if c.channel_reverse { v.reverse(); }
                            if c.force_alpha_1 { v[3] = if formats::internal_is_int(self.rt[buf as usize].ty) { 1 } else { 1f32.to_bits() }; }
                            let ty = self.rt[buf as usize].ty;
                            self.tb.color[buf as usize][i] = v.map(|ch| formats::quantize(ty, ch));
                        }
                        9 | 10 | 11 => {
                            let (z, st) = decode_zs(fmt, &px);
                            if buf != 10 { if let Some(z) = z { self.tb.z[i] = quantize_depth(self.depth_type, z); } }
                            if buf != 9 { if let Some(s) = st { self.tb.s[i] = s; } }
                        }
                        _ => {}
                    }
                    if !all { // the same value in every sample
                        for s2 in 1..self.tb.ns {
                            let j = self.tb.idx(x, y, s2);
                            match buf { 0..=3 => self.tb.color[buf as usize][j] = self.tb.color[buf as usize][i], _ => { self.tb.z[j] = self.tb.z[i]; self.tb.s[j] = self.tb.s[i]; } }
                        }
                        break;
                    }
                }
            }
        }
    }

    fn store(&mut self, c: &StoreTileBufferGeneral, m: &mut Gmem) {
        let buf = c.buffer_to_store;
        if buf == 8 { if c.clear_buffer_being_stored { self.clear(true, true); } return; }
        let fmt = c.output_image_format;
        let cpp = formats::out_cpp(fmt);
        if cpp == 0 { formats::encode(fmt, [0; 4], &mut []); return; }
        let (t, stride, ih) = self.surface(c.memory_format, cpp, c.height_in_ub_or_stride);
        let all = c.decimate_mode == 3 && self.msaa;
        let resolve = c.decimate_mode == 1 && self.msaa;
        let (x0, y0) = (self.tile.0 * self.tile_w, self.tile.1 * self.tile_h);
        let mut px = [0u8; 16];
        for y in 0..self.tile_h {
            for x in 0..self.tile_w {
                let (gx, gy) = (x0 + x, y0 + y);
                if gx >= self.width || gy >= self.height { continue; }
                let samples = if all { self.tb.ns } else { 1 };
                for s in 0..samples {
                    let (mx, my) = if all { (gx * 2 + (s & 1), gy * 2 + (s >> 1)) } else { (gx, gy) };
                    let my = if c.flip_y { c.height.saturating_sub(1).saturating_sub(my) } else { my };
                    let a = c.address.wrapping_add(tiling::offset(t, cpp, stride, ih, mx, my));
                    let i = self.tb.idx(x, y, s);
                    match buf {
                        0..=3 => {
                            let ty = self.rt[buf as usize].ty;
                            let mut v = if resolve && !formats::internal_is_int(ty) {
                                let mut acc = [0f32; 4];
                                for s2 in 0..self.tb.ns { let p = self.tb.color[buf as usize][self.tb.idx(x, y, s2)]; for k in 0..4 { acc[k] += f32::from_bits(p[k]); } }
                                acc.map(|a| (a / self.tb.ns as f32).to_bits())
                            } else { self.tb.color[buf as usize][i] };
                            if c.channel_reverse { v.reverse(); }
                            if c.r_b_swap { v.swap(0, 2); }
                            if !formats::internal_is_int(ty) && ty != it::F32 {
                                // 8-bit and f16 render targets are stored from f32 here
                            }
                            formats::encode(fmt, v, &mut px[..cpp as usize]);
                            m.write(a, &px[..cpp as usize]);
                        }
                        9 | 10 | 11 => {
                            m.read(a, &mut px[..cpp as usize]);
                            encode_zs(fmt, if buf != 10 { Some(self.tb.z[i]) } else { None }, if buf != 9 { Some(self.tb.s[i]) } else { None }, &mut px[..cpp as usize]);
                            m.write(a, &px[..cpp as usize]);
                        }
                        _ => {}
                    }
                }
            }
        }
        if c.clear_buffer_being_stored {
            match buf { 0..=3 => { let v = clear_value(self.rt[buf as usize].ty, self.rt[buf as usize].bpp, self.rt[buf as usize].clear); self.tb.color[buf as usize].iter_mut().for_each(|p| *p = v); } _ => self.clear(false, true) }
        }
    }

    /// Run the binned primitives of the current tile.
    fn render_tile(&mut self, m: &mut Gmem, code: &mut Code) {
        let Some(binned) = self.cur.clone() else { return };
        if binned.tile_w != self.tile_w || binned.tile_h != self.tile_h {
            crate::log_once("v3d: render and bin tile sizes differ");
        }
        let (tx, ty) = self.tile;
        if tx >= binned.tiles_x || ty >= binned.tiles_y { return; }
        let list = &binned.lists[(ty * binned.tiles_x + tx) as usize];
        for &pi in list {
            let p = &binned.prims[pi as usize];
            let d = binned.draws[p.draw as usize].clone();
            let outs = match self.vs_cache.get(&p.draw) {
                Some(o) => o.clone(),
                None => {
                    let mut ins = 0;
                    let o = Arc::new(bin::run_vs(m, code, &mut ins, &d, false, &d.verts));
                    self.insns += ins;
                    if crate::log_enabled() {
                        let row = |k: usize| o[k].iter().map(|v| format!("{:#x}/{}", v, f32::from_bits(*v))).collect::<Vec<_>>().join(" ");
                        crate::log(&format!("v3d: draw {} VS out v0: {}", p.draw, row(0)));
                    }
                    self.vs_cache.insert(p.draw, o.clone());
                    o
                }
            };
            self.draw_prim(&d, p, &outs, m, code);
        }
    }

    fn draw_prim(&mut self, d: &DrawState, p: &Prim, outs: &[Vec<u32>], m: &mut Gmem, code: &mut Code) {
        let psize = d.rec.point_size_in_shaded_vertex_data;
        let vbase = 4 + psize as usize;
        let nvary = d.rec.number_of_varyings_in_fragment_shader as usize;
        let vert = |k: usize| -> RVert {
            let o = &outs[p.v[k] as usize];
            let g = |i: usize| *o.get(i).unwrap_or(&0);
            RVert {
                x: d.vp_center[0] + (g(0) as i32) as f32 / 256.0,
                y: d.vp_center[1] + (g(1) as i32) as f32 / 256.0,
                z: f32::from_bits(g(2)),
                invw: f32::from_bits(g(3)),
                vary: (0..nvary).map(|j| g(vbase + j)).collect(),
            }
        };
        let n = p.n as usize;
        let orig: Vec<RVert> = (0..n).map(vert).collect();
        let pv = if d.cfg.direct3d_provoking_vertex { 0 } else { n - 1 };
        let prov = orig[pv].vary.clone();
        let tris: Vec<[RVert; 3]> = match &p.clipped {
            None if n == 3 => vec![[orig[0].clone(), orig[1].clone(), orig[2].clone()]],
            None => Vec::new(),
            Some(poly) if n == 3 => {
                let cv = |c: &ClipVert| -> RVert {
                    let s = bin::to_screen(d, &c.pos);
                    let iw = 1.0 / c.pos[3];
                    let vary = (0..nvary).map(|j| {
                        if d.flat & (1u128 << j) != 0 { return prov[j]; }
                        let mut a = 0.0;
                        for k in 0..3 { a += c.bary[k] * f32::from_bits(orig[k].vary[j]); }
                        a.to_bits()
                    }).collect();
                    RVert { x: s[0], y: s[1], z: c.pos[2] * iw * d.z_scale + d.z_offset, invw: iw, vary }
                };
                let vs: Vec<RVert> = poly.iter().map(cv).collect();
                (1..vs.len().saturating_sub(1)).map(|k| [vs[0].clone(), vs[k].clone(), vs[k + 1].clone()]).collect()
            }
            Some(_) => Vec::new(),
        };
        match n {
            3 => for t in &tris { self.raster_tri(d, p, [&t[0], &t[1], &t[2]], &prov, m, code); },
            2 => {
                let (a, b) = match &p.clipped {
                    Some(poly) if poly.len() == 2 => {
                        let mk = |c: &ClipVert| { let s = bin::to_screen(d, &c.pos); let iw = 1.0 / c.pos[3];
                            RVert { x: s[0], y: s[1], z: c.pos[2] * iw * d.z_scale + d.z_offset, invw: iw,
                                    vary: (0..nvary).map(|j| (c.bary[0] * f32::from_bits(orig[0].vary[j]) + c.bary[1] * f32::from_bits(orig[1].vary[j])).to_bits()).collect() } };
                        (mk(&poly[0]), mk(&poly[1]))
                    }
                    _ => (orig[0].clone(), orig[1].clone()),
                };
                self.raster_line(d, p, &a, &b, &prov, m, code);
            }
            _ => {
                let size = if psize { f32::from_bits(*outs[p.v[0] as usize].get(4).unwrap_or(&0)) } else { d.point_size };
                self.raster_point(d, p, &orig[0], size, m, code);
            }
        }
    }

    fn scissor(&self, d: &DrawState) -> (i64, i64, i64, i64) {
        let (x0, y0) = ((self.tile.0 * self.tile_w) as i64, (self.tile.1 * self.tile_h) as i64);
        let x1 = (x0 + self.tile_w as i64).min(self.width as i64).min(d.clip[2] as i64);
        let y1 = (y0 + self.tile_h as i64).min(self.height as i64).min(d.clip[3] as i64);
        (x0.max(d.clip[0] as i64), y0.max(d.clip[1] as i64), x1, y1)
    }

    fn raster_tri(&mut self, d: &DrawState, p: &Prim, v: [&RVert; 3], prov: &[u32], m: &mut Gmem, code: &mut Code) {
        let Some(tri) = Tri::new(v) else { return };
        let (bx0, by0, bx1, by1) = tri.bbox();
        let (sx0, sy0, sx1, sy1) = self.scissor(d);
        let (x0, y0, x1, y1) = (bx0.max(sx0) & !1, by0.max(sy0) & !1, bx1.min(sx1), by1.min(sy1));
        if x1 <= x0 || y1 <= y0 { return; }
        let offset = if d.cfg.enable_depth_offset {
            let o = d.depth_offset.depth_offset_factor * tri.dzdx.abs().max(tri.dzdy.abs()) + d.depth_offset.depth_offset_units / 16_777_216.0;
            if d.depth_offset.limit > 0.0 { o.min(d.depth_offset.limit) } else if d.depth_offset.limit < 0.0 { o.max(d.depth_offset.limit) } else { o }
        } else { 0.0 };
        let samples: &[(f32, f32)] = if self.msaa { &SAMPLES_4X } else { &[(0.5, 0.5)] };
        let mut quads: Vec<Quad> = Vec::new();
        let mut y = y0;
        while y < y1 {
            let mut x = x0;
            while x < x1 {
                let mut q = Quad { x: x as u32, y: y as u32, cov: [0; 4], bary: [[0.0; 3]; 4] };
                let mut any = false;
                for k in 0..4 {
                    let (px, py) = (x + (k & 1) as i64, y + (k >> 1) as i64);
                    q.bary[k] = tri.bary(v, px as f32 + 0.5, py as f32 + 0.5);
                    if px < sx0 || px >= sx1 || py < sy0 || py >= sy1 { continue; }
                    for (s, &(ox, oy)) in samples.iter().enumerate() {
                        if tri.covers(px as f32 + ox, py as f32 + oy) && d.sample_mask & (1 << s) != 0 { q.cov[k] |= 1 << s; }
                    }
                    if !self.msaa && q.cov[k] != 0 { q.cov[k] = 0xf; }
                    any |= q.cov[k] != 0;
                }
                if any { quads.push(q); }
                x += 2;
            }
            y += 2;
        }
        let pv = if d.cfg.direct3d_provoking_vertex { 0 } else { 2 };
        let geo = Geo { v: [v[0].clone(), v[1].clone(), v[2].clone()], prov: prov.to_vec(), pv, offset, kind: 3, point: None };
        self.shade(d, p, &geo, &quads, m, code);
    }

    fn raster_line(&mut self, d: &DrawState, p: &Prim, a: &RVert, b: &RVert, prov: &[u32], m: &mut Gmem, code: &mut Code) {
        // a wide line as a parallelogram: offset by half the width along the minor axis
        let w = d.line_width.max(1.0) / 2.0;
        let (dx, dy) = (b.x - a.x, b.y - a.y);
        let (ox, oy) = if dx.abs() >= dy.abs() { (0.0, w) } else { (w, 0.0) };
        let mk = |v: &RVert, sx: f32, sy: f32| RVert { x: v.x + sx, y: v.y + sy, ..v.clone() };
        let quad = [mk(a, -ox, -oy), mk(a, ox, oy), mk(b, ox, oy), mk(b, -ox, -oy)];
        let len2 = dx * dx + dy * dy;
        let (sx0, sy0, sx1, sy1) = self.scissor(d);
        let xs = quad.iter().map(|v| v.x);
        let ys = quad.iter().map(|v| v.y);
        let (bx0, bx1) = (xs.clone().fold(f32::MAX, f32::min).floor() as i64, xs.fold(f32::MIN, f32::max).ceil() as i64 + 1);
        let (by0, by1) = (ys.clone().fold(f32::MAX, f32::min).floor() as i64, ys.fold(f32::MIN, f32::max).ceil() as i64 + 1);
        let (x0, y0, x1, y1) = (bx0.max(sx0) & !1, by0.max(sy0) & !1, bx1.min(sx1), by1.min(sy1));
        if x1 <= x0 || y1 <= y0 { return; }
        let t1 = Tri::new([&quad[0], &quad[1], &quad[2]]);
        let t2 = Tri::new([&quad[0], &quad[2], &quad[3]]);
        let mut quads = Vec::new();
        let mut y = y0;
        while y < y1 {
            let mut x = x0;
            while x < x1 {
                let mut q = Quad { x: x as u32, y: y as u32, cov: [0; 4], bary: [[0.0; 3]; 4] };
                let mut any = false;
                for k in 0..4 {
                    let (px, py) = (x + (k & 1) as i64, y + (k >> 1) as i64);
                    let (cx, cy) = (px as f32 + 0.5, py as f32 + 0.5);
                    let t = if len2 > 0.0 { (((cx - a.x) * dx + (cy - a.y) * dy) / len2).clamp(0.0, 1.0) } else { 0.0 };
                    q.bary[k] = [1.0 - t, t, 0.0];
                    if px < sx0 || px >= sx1 || py < sy0 || py >= sy1 { continue; }
                    let c = t1.as_ref().map_or(false, |t| t.covers(cx, cy)) || t2.as_ref().map_or(false, |t| t.covers(cx, cy));
                    if c { q.cov[k] = 0xf; any = true; }
                }
                if any { quads.push(q); }
                x += 2;
            }
            y += 2;
        }
        let pv = if d.cfg.direct3d_provoking_vertex { 0 } else { 1 };
        let geo = Geo { v: [a.clone(), b.clone(), b.clone()], prov: prov.to_vec(), pv, offset: 0.0, kind: 2, point: None };
        self.shade(d, p, &geo, &quads, m, code);
    }

    fn raster_point(&mut self, d: &DrawState, p: &Prim, v: &RVert, size: f32, m: &mut Gmem, code: &mut Code) {
        let s = size.max(1.0);
        let (l, t) = (v.x - s / 2.0, v.y - s / 2.0);
        let (sx0, sy0, sx1, sy1) = self.scissor(d);
        let (x0, y0, x1, y1) = ((l.floor() as i64).max(sx0) & !1, (t.floor() as i64).max(sy0) & !1, ((l + s).ceil() as i64 + 1).min(sx1), ((t + s).ceil() as i64 + 1).min(sy1));
        let mut quads = Vec::new();
        let mut y = y0;
        while y < y1 {
            let mut x = x0;
            while x < x1 {
                let mut q = Quad { x: x as u32, y: y as u32, cov: [0; 4], bary: [[1.0, 0.0, 0.0]; 4] };
                let mut any = false;
                for k in 0..4 {
                    let (px, py) = (x + (k & 1) as i64, y + (k >> 1) as i64);
                    let (cx, cy) = (px as f32 + 0.5, py as f32 + 0.5);
                    if px < sx0 || px >= sx1 || py < sy0 || py >= sy1 { continue; }
                    if cx >= l && cx < l + s && cy >= t && cy < t + s { q.cov[k] = 0xf; any = true; }
                }
                if any { quads.push(q); }
                x += 2;
            }
            y += 2;
        }
        let geo = Geo { v: [v.clone(), v.clone(), v.clone()], prov: v.vary.clone(), pv: 0, offset: 0.0, kind: 1, point: Some((l, t, s)) };
        self.shade(d, p, &geo, &quads, m, code);
    }

    /// Fragment shading of covered quads, four quads per QPU thread.
    fn shade(&mut self, d: &DrawState, p: &Prim, geo: &Geo, quads: &[Quad], m: &mut Gmem, code: &mut Code) {
        let rec = &d.rec;
        let late_z = rec.fragment_shader_does_z_writes;
        let (tx0, ty0) = (self.tile.0 * self.tile_w, self.tile.1 * self.tile_h);
        for batch in quads.chunks(4) {
            let mut t = Thread::new(rec.fragment_shader_code_address, rec.fragment_shader_uniforms_address, rec.fragment_shader_start_in_final_thread_section);
            t.revf = p.reverse;
            let mut fe = FsEnv { r: self, d, geo, m, bary: [[0.0; 3]; LANES], z: [0.0; LANES], vary_n: 0, tlb_cfg: !0, pending: Vec::new(), spec: 0, need: 0,
                                 tile0: (tx0, ty0), passed: 0, prim: p };
            t.valid = 0;
            for (qi, q) in batch.iter().enumerate() {
                for k in 0..4 {
                    let l = qi * 4 + k;
                    let (px, py) = (q.x + (k & 1) as u32, q.y + (k >> 1) as u32);
                    t.xcd[l] = px as i32;
                    t.ycd[l] = py as i32;
                    t.fx[l] = px as f32 + 0.5;
                    t.fy[l] = py as f32 + 0.5;
                    t.msf[l] = q.cov[k];
                    t.valid |= 1 << l;
                    let b = q.bary[k];
                    fe.bary[l] = b;
                    let zs = b[0] * geo.v[0].z + b[1] * geo.v[1].z + b[2] * geo.v[2].z + geo.offset;
                    let iw = b[0] * geo.v[0].invw + b[1] * geo.v[1].invw + b[2] * geo.v[2].invw;
                    let w = if geo.kind == 3 && iw != 0.0 { 1.0 / iw } else if geo.v[0].invw != 0.0 { 1.0 / geo.v[0].invw } else { 1.0 };
                    fe.z[l] = zs;
                    t.rf[0][l] = w.to_bits();
                    t.rf[1][l] = w.to_bits();
                    t.rf[2][l] = zs.to_bits();
                }
            }
            for l in batch.len() * 4..LANES { t.msf[l] = 0; }
            if !late_z {
                for l in 0..LANES { if t.msf[l] != 0 { let z = fe.z[l]; fe.ds_test(&mut t, l, z); } }
                if t.msf.iter().all(|&s| s == 0) { continue; }
            }
            let before = t.insns;
            let stop = t.run(code, &mut fe, 5_000_000);
            if stop != crate::qpu::Stop::Done { crate::log_once(&format!("v3d: fragment shader stopped: {stop:?}")); }
            if d.occlusion != 0 { *fe.r.occlusion.entry(d.occlusion).or_default() += fe.passed; }
            let n = t.insns - before;
            fe.r.insns += n;
        }
    }
}

#[derive(Clone)]
struct Quad { x: u32, y: u32, cov: [u8; 4], bary: [[f32; 3]; 4] }

struct Geo {
    v: [RVert; 3],
    prov: Vec<u32>,
    pv: usize,
    offset: f32,
    kind: u8,
    point: Option<(f32, f32, f32)>,
}

struct FsEnv<'a, 'r, 'm> {
    r: &'a mut Render<'r>,
    d: &'a DrawState,
    geo: &'a Geo,
    m: &'a mut Gmem<'m>,
    bary: [[f32; 3]; LANES],
    z: [f32; LANES],
    vary_n: usize,
    tlb_cfg: u32,
    pending: Vec<(u16, V)>,
    spec: u32,
    need: usize,
    tile0: (u32, u32),
    passed: u32,
    prim: &'a Prim,
}

fn cmp(func: u32, a: f32, b: f32) -> bool {
    match func { 0 => false, 1 => a < b, 2 => a == b, 3 => a <= b, 4 => a > b, 5 => a != b, 6 => a >= b, _ => true }
}

fn stencil_op(op: u32, s: u8, refv: u8) -> u8 {
    match op { 0 => 0, 2 => refv, 3 => s.saturating_add(1), 4 => s.saturating_sub(1), 5 => !s, 6 => s.wrapping_add(1), 7 => s.wrapping_sub(1), _ => s }
}

fn quantize_depth(t: u32, z: f32) -> f32 {
    let z = if z.is_nan() { 0.0 } else { z };
    match t {
        1 => (z.clamp(0.0, 1.0) as f64 * 16_777_215.0).round() as f32 / 16_777_215.0,
        2 => (z.clamp(0.0, 1.0) * 65535.0).round() / 65535.0,
        _ => z,
    }
}

fn decode_zs(fmt: u32, p: &[u8]) -> (Option<f32>, Option<u8>) {
    let w = u32::from_le_bytes([p[0], *p.get(1).unwrap_or(&0), *p.get(2).unwrap_or(&0), *p.get(3).unwrap_or(&0)]);
    match fmt {
        40 => (Some(f32::from_bits(w)), None),
        41 => (Some((w & 0xff_ffff) as f32 / 16_777_215.0), None),
        42 => (Some((w & 0xffff) as f32 / 65535.0), None),
        43 => (Some((w >> 8) as f32 / 16_777_215.0), Some(w as u8)),
        44 => (None, Some(p[0])),
        _ => (None, None),
    }
}

fn encode_zs(fmt: u32, z: Option<f32>, s: Option<u8>, p: &mut [u8]) {
    let un = |z: f32, m: f64| (z.clamp(0.0, 1.0) as f64 * m).round() as u32;
    match fmt {
        40 => if let Some(z) = z { p[..4].copy_from_slice(&z.to_bits().to_le_bytes()) },
        41 => if let Some(z) = z { p[..4].copy_from_slice(&un(z, 16_777_215.0).to_le_bytes()) },
        42 => if let Some(z) = z { p[..2].copy_from_slice(&(un(z, 65535.0) as u16).to_le_bytes()) },
        43 => {
            let mut w = u32::from_le_bytes([p[0], p[1], p[2], p[3]]);
            if let Some(z) = z { w = (w & 0xff) | (un(z, 16_777_215.0) << 8); }
            if let Some(s) = s { w = (w & !0xff) | s as u32; }
            p[..4].copy_from_slice(&w.to_le_bytes());
        }
        44 => if let Some(s) = s { p[0] = s },
        _ => {}
    }
}

/// Clear colour words (as the RCL gives them, in the internal format) as tile buffer channels.
fn clear_value(ty: u32, bpp: u32, c: [u32; 4]) -> [u32; 4] {
    match ty {
        it::UNORM8 => [0, 8, 16, 24].map(|s| (((c[0] >> s) & 0xff) as f32 / 255.0).to_bits()),
        it::I8 => [0, 8, 16, 24].map(|s| ((c[0] >> s) as i8) as i32 as u32),
        it::UI8 => [0, 8, 16, 24].map(|s| (c[0] >> s) & 0xff),
        it::F16 => [c[0] & 0xffff, c[0] >> 16, c[1] & 0xffff, c[1] >> 16].map(|h| f16_to_f32(h as u16).to_bits()),
        it::I16 => [c[0] & 0xffff, c[0] >> 16, c[1] & 0xffff, c[1] >> 16].map(|h| h as u16 as i16 as i32 as u32),
        it::UI16 => [c[0] & 0xffff, c[0] >> 16, c[1] & 0xffff, c[1] >> 16],
        _ => { let _ = bpp; c }
    }
}

fn blend_factor(f: u32, s: [f32; 4], dd: [f32; 4], k: [f32; 4], ch: usize) -> f32 {
    match f {
        0 => 0.0, 1 => 1.0, 2 => s[ch], 3 => 1.0 - s[ch], 4 => dd[ch], 5 => 1.0 - dd[ch], 6 => s[3], 7 => 1.0 - s[3],
        8 => dd[3], 9 => 1.0 - dd[3], 10 => k[ch], 11 => 1.0 - k[ch], 12 => k[3], 13 => 1.0 - k[3],
        14 => if ch == 3 { 1.0 } else { s[3].min(1.0 - dd[3]) },
        _ => 1.0,
    }
}

impl FsEnv<'_, '_, '_> {
    fn px(&self, t: &Thread, l: usize) -> Option<(u32, u32)> {
        let (x, y) = (t.xcd[l] as u32, t.ycd[l] as u32);
        if x < self.tile0.0 || y < self.tile0.1 { return None; }
        let (lx, ly) = (x - self.tile0.0, y - self.tile0.1);
        if lx >= self.r.tile_w || ly >= self.r.tile_h { None } else { Some((lx, ly)) }
    }

    /// Depth and stencil test of lane `l` at depth `z`; clears failing sample bits.
    fn ds_test(&mut self, t: &mut Thread, l: usize, z: f32) {
        let Some((x, y)) = self.px(t, l) else { t.msf[l] = 0; return };
        let cfg = &self.d.cfg;
        let st = if self.prim.reverse { &self.d.stencil_back } else { &self.d.stencil_front };
        let dt = self.r.depth_type;
        let zq = quantize_depth(dt, if dt == 0 { z } else { z.clamp(0.0, 1.0) });
        for s in 0..self.r.tb.ns {
            if t.msf[l] & (1 << s) == 0 { continue; }
            let i = self.r.tb.idx(x, y, s);
            let mut keep = true;
            if cfg.stencil_enable {
                let sv = self.r.tb.s[i];
                let m = st.stencil_test_mask as u8;
                let refv = st.stencil_ref_value as u8;
                let pass_s = match st.stencil_test_function { 0 => false, 1 => refv & m < sv & m, 2 => refv & m == sv & m, 3 => refv & m <= sv & m, 4 => refv & m > sv & m, 5 => refv & m != sv & m, 6 => refv & m >= sv & m, _ => true };
                let pass_z = cmp(cfg.depth_test_function, zq, self.r.tb.z[i]);
                let op = if !pass_s { st.stencil_test_fail_op } else if !pass_z { st.depth_test_fail_op } else { st.stencil_pass_op };
                let wm = st.stencil_write_mask as u8;
                self.r.tb.s[i] = (sv & !wm) | (stencil_op(op, sv, refv) & wm);
                keep = pass_s && pass_z;
            } else if !cmp(cfg.depth_test_function, zq, self.r.tb.z[i]) {
                keep = false;
            }
            if keep {
                if cfg.z_updates_enable { self.r.tb.z[i] = zq; }
                self.passed += 1;
            } else { t.msf[l] &= !(1 << s); }
        }
    }

    fn write_color(&mut self, t: &Thread, rt: usize, lanes: u16, ch: &[[u32; 4]; LANES]) {
        let ty = self.r.rt[rt].ty;
        let int = formats::internal_is_int(ty);
        let blend_on = self.d.cfg.blend_enable && self.d.blend_enables & (1 << rt) != 0 && !int;
        let bc = self.d.blend[rt];
        let wmask = (self.d.color_wmasks >> (4 * rt)) & 0xf;
        let clamp_norm = self.r.rt[rt].clamp == 1;
        for l in 0..LANES {
            if lanes & (1 << l) == 0 || t.msf[l] == 0 { continue; }
            let Some((x, y)) = self.px(t, l) else { continue };
            for s in 0..self.r.tb.ns {
                if t.msf[l] & (1 << s) == 0 { continue; }
                let i = self.r.tb.idx(x, y, s);
                let dst = self.r.tb.color[rt][i];
                let mut out = ch[l];
                if blend_on {
                    let sf: [f32; 4] = out.map(f32::from_bits);
                    let df: [f32; 4] = dst.map(f32::from_bits);
                    let k = self.d.blend_color;
                    let mut o = [0f32; 4];
                    for c in 0..4 {
                        let (mode, sfac, dfac) = if c < 3 { (bc.color_blend_mode, bc.color_blend_src_factor, bc.color_blend_dst_factor) } else { (bc.alpha_blend_mode, bc.alpha_blend_src_factor, bc.alpha_blend_dst_factor) };
                        let (a, b) = (sf[c] * blend_factor(sfac, sf, df, k, c), df[c] * blend_factor(dfac, sf, df, k, c));
                        o[c] = match mode { 0 => a + b, 1 => a - b, 2 => b - a, 3 => sf[c].min(df[c]), 4 => sf[c].max(df[c]), 5 => sf[c] * df[c], _ => a + b };
                    }
                    out = o.map(f32::to_bits);
                }
                if clamp_norm && !int { out = out.map(|v| f32::from_bits(v).clamp(0.0, 1.0).to_bits()); }
                for c in 0..4 {
                    if wmask & (1 << c) != 0 { out[c] = dst[c]; } else { out[c] = formats::quantize(ty, out[c]); }
                }
                self.r.tb.color[rt][i] = out;
            }
        }
    }

    /// A complete TLB write setup (all its words received).
    fn tlb_commit(&mut self, t: &mut Thread) {
        let spec = self.spec;
        let words = std::mem::take(&mut self.pending);
        let lanes = words.first().map_or(0, |w| w.0);
        match spec >> 6 {
            2 => {
                if (spec >> 4) & 3 == 0 {
                    // depth write: the shader's Z or the interpolated Z, then the depth/stencil test
                    let per_pixel = spec & (1 << 3) != 0;
                    for l in 0..LANES {
                        if t.msf[l] == 0 { continue; }
                        let z = if per_pixel { words.first().map_or(self.z[l], |w| f32::from_bits(w.1[l])) } else { self.z[l] };
                        if lanes & (1 << l) == 0 && per_pixel { continue; }
                        self.ds_test(t, l, z);
                    }
                }
            }
            ty => {
                let rt = (7 - ((spec >> 3) & 7)) as usize;
                if rt >= 4 { return; }
                let mut ch = [[0u32; 4]; LANES];
                let one = |int: bool| if int { 1 } else { 1f32.to_bits() };
                for l in 0..LANES {
                    let mut c = [0u32, 0, 0, one(ty == 1)];
                    if ty == 3 {
                        let swap = spec & 2 != 0;
                        for (k, w) in words.iter().enumerate() {
                            let (lo, hi) = (f16_to_f32(w.1[l] as u16).to_bits(), f16_to_f32((w.1[l] >> 16) as u16).to_bits());
                            let (a, b) = if swap { (lo, hi) } else { (hi, lo) };
                            if 2 * k < 4 { c[2 * k] = a; }
                            if 2 * k + 1 < 4 { c[2 * k + 1] = b; }
                        }
                    } else {
                        for (k, w) in words.iter().enumerate() { if k < 4 { c[k] = w.1[l]; } }
                    }
                    ch[l] = c;
                }
                self.write_color(t, rt, lanes, &ch);
            }
        }
    }
}

impl Env for FsEnv<'_, '_, '_> {
    fn read32(&mut self, va: u32) -> u32 { self.m.r32(va) }
    fn write32(&mut self, va: u32, v: u32) { self.m.w32(va, v) }
    fn tmu(&mut self, t: &Thread, kind: TmuKind, cfg: &[u32], req: &TmuReq, lanes: u16) -> Vec<V> {
        let lanes = if kind == TmuKind::General { let mut m = lanes; for l in 0..LANES { if t.msf[l] == 0 { m &= !(1 << l); } } m } else { lanes };
        crate::tmu::lookup(self.m, t, kind, cfg, req, lanes, true)
    }

    fn ldvary(&mut self, t: &mut Thread) -> (V, V) {
        let k = self.vary_n;
        self.vary_n += 1;
        let rec = &self.d.rec;
        let mut j = k as isize;
        let (mut out, mut c) = ([0u32; LANES], [0u32; LANES]);
        if rec.insert_primitive_id_as_first_varying_to_fragment_shader {
            if j == 0 { return ([0; LANES], [(self.prim.prim_id as f32).to_bits(); LANES]); }
            j -= 1;
        }
        if self.geo.kind != 3 && !rec.disable_implicit_point_line_varyings {
            if let Some((l0, t0, s)) = self.geo.point {
                if j < 2 {
                    for l in 0..LANES {
                        let v = if j == 0 { (t.fx[l] - l0) / s } else { (t.fy[l] - t0) / s };
                        out[l] = 0; c[l] = v.to_bits();
                    }
                    return (out, c);
                }
                j -= 2;
            } else if self.geo.kind == 2 {
                if j == 0 { return ([0; LANES], [0; LANES]); }
                j -= 1;
            }
        }
        let j = j as usize;
        let d = self.d;
        let flat = d.flat & (1u128 << j) != 0;
        let noper = d.noperspective & (1u128 << j) != 0;
        let g = self.geo;
        let pvv = f32::from_bits(*g.prov.get(j).unwrap_or(&0));
        let a: [f32; 3] = [0, 1, 2].map(|k| f32::from_bits(*g.v[k].vary.get(j).unwrap_or(&0)));
        for l in 0..LANES {
            c[l] = pvv.to_bits();
            if flat { continue; }
            let b = self.bary[l];
            let mut s = 0.0;
            for k in 0..3 {
                let w = if noper || g.kind == 1 { 1.0 } else { g.v[k].invw };
                s += b[k] * (a[k] - pvv) * w;
            }
            out[l] = s.to_bits();
        }
        (out, c)
    }

    fn tlb_write(&mut self, t: &mut Thread, lanes: u16, v: &V, cfg: Option<u32>) {
        if self.need == 0 {
            if let Some(c) = cfg { self.tlb_cfg = c; }
            self.spec = self.tlb_cfg & 0xff;
            self.tlb_cfg = (self.tlb_cfg >> 8) | 0xff00_0000;
            let spec = self.spec;
            self.need = match spec >> 6 {
                3 => if spec & 1 != 0 { 2 } else { 1 },
                0 | 1 => (spec & 3) as usize + 1,
                _ => 1,
            };
            if self.r.msaa && spec & (1 << 2) == 0 && spec >> 6 != 2 { self.need *= 4; }
        }
        if crate::log_enabled() && self.r.insns < 2000 { crate::log(&format!("v3d: tlb write spec {:#x} lane0 {:#x} lanes {lanes:#x} msf0 {}", self.spec, v[0], t.msf[0])); }
        self.pending.push((lanes, *v));
        self.need -= 1;
        if self.need == 0 {
            if self.r.msaa && self.spec & (1 << 2) == 0 && self.spec >> 6 != 2 {
                // per-sample writes: keep the first sample's words (per-pixel shading of all samples)
                let per = self.pending.len() / 4;
                self.pending.truncate(per);
            }
            self.tlb_commit(t);
        }
    }

    fn tlb_read(&mut self, t: &mut Thread, cfg: Option<u32>) -> V {
        if let Some(c) = cfg { self.tlb_cfg = c; }
        let spec = self.tlb_cfg & 0xff;
        let mut out = [0u32; LANES];
        let rt = (7 - ((spec >> 3) & 7)) as usize;
        let word = self.pending.len();
        for l in 0..LANES {
            let Some((x, y)) = self.px(t, l) else { continue };
            let i = self.r.tb.idx(x, y, 0);
            match spec >> 6 {
                2 => out[l] = if (spec >> 4) & 3 == 0 { self.r.tb.z[i].to_bits() } else { self.r.tb.s[i] as u32 },
                3 => {
                    let p = self.r.tb.color[rt.min(3)][i];
                    let h = |k: usize| f32_to_f16(f32::from_bits(p[k])) as u32;
                    let k = (self.need_read() % 2) * 2;
                    out[l] = h(k) | h(k + 1) << 16;
                }
                _ => out[l] = self.r.tb.color[rt.min(3)][i][word.min(3)],
            }
        }
        out
    }
}

impl FsEnv<'_, '_, '_> {
    fn need_read(&self) -> usize { 0 }
}
