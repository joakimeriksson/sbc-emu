//! The render pipeline (control list thread 1): runs the render control list tile by tile:
//! tile buffer loads and stores (with format conversion between memory layouts and the
//! tile buffer's internal types), clears, and for each tile the primitives the binner sorted
//! into it: vertex shading, rasterization into 2x2 quads, early/late depth and stencil
//! tests, fragment shading on 16-lane QPU threads, and tile buffer writes with blending.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, OnceLock};

use crate::bin::{self, Binned, ClipVert, DrawState, Prim};
use crate::formats::{self, it};
use crate::packets::*;
use crate::qpu::{f16_to_f32, f32_to_f16, Env, Thread, TmuKind, TmuReq, LANES, V};
use crate::raster::{RVert, Tri, SAMPLES_4X};
use crate::tiling::{self, Tiling};
use crate::{BinResults, Code, Gmem};

#[derive(Clone, Copy, Default)]
struct RtCfg { bpp: u32, ty: u32, clamp: u32, clear: [u32; 4] }

#[derive(Clone)]
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
    /// vertex shader outputs per draw of the current bin result (this worker's copy)
    vs_cache: HashMap<u32, Arc<VsOut>>,
    /// the same, shared by the render workers: each draw's vertex shader runs once
    vs_shared: VsShared,
    occlusion: HashMap<u32, u32>,
    insns: u64,
    /// fragment shader thread storage, reused between invocations
    thread: Option<Box<Thread>>,
    /// tiles of consecutive supertile packets, rendered together (in parallel where possible)
    queue: Vec<(u32, u32)>,
    /// this worker's number: its shaders' thread indices (TIDX) are 2w (fragment) and 2w+1
    /// (vertex), distinct between workers, inside the 4 x 8 QPU threads Mesa sizes register
    /// spill memory for
    worker: u32,
}

type VsShared = Arc<Mutex<HashMap<u32, Arc<OnceLock<Arc<VsOut>>>>>>;


/// The tile-list base addresses a render control list points at (the binning results it reads).
pub fn tile_list_bases(start: u32, end: u32, m: &mut Gmem) -> Vec<u32> {
    let mut v = Vec::new();
    let mut cl = bin::Cl::new(start, end);
    while let Some((op, b)) = cl.next(m) {
        if op == 13 { break; }
        if op == 123 { let a = MulticoreRenderingTileListSetBase::parse(&b).address; if !v.contains(&a) { v.push(a); } }
    }
    v
}

pub fn run(start: u32, end: u32, binned: &BinResults, m: &mut Gmem, code: &mut Code, insns: &mut u64) {
    let mut r = Render {
        binned, cur: None, width: 0, height: 0, msaa: false, nrt: 1, rt: [RtCfg::default(); 4], depth_type: 0, early_ds_clear: false,
        z_clear: 1.0, s_clear: 0, tile_w: 64, tile_h: 64, tb: TileBuf::new(64, 64, 1), tile: (0, 0), generic: (0, 0),
        supertile: MulticoreRenderingSupertileCfg::default(), vs_cache: HashMap::new(), vs_shared: Default::default(), occlusion: HashMap::new(),
        insns: 0, thread: None, queue: Vec::new(), worker: 0,
    };
    let mut cl = bin::Cl::new(start, end);
    while let Some((op, b)) = cl.next(m) {
        if op == 13 { break; }
        // supertiles are independent: queue them, render them together before anything else
        if op == 23 { r.queue_supertile(&b); continue; }
        r.flush(m, code);
        r.packet(op, &b, m, code);
    }
    r.flush(m, code);
    for (addr, n) in r.occlusion.drain() { let v = m.r32(addr); m.w32(addr, v.wrapping_add(n)); }
    *insns += r.insns;
}

impl<'r> Render<'r> {
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
                // the lists a bin job made, if they are still in memory: a job with no bin job of
                // its own (Mesa's TLB blits) reads a fresh, zeroed buffer, empty lists on hardware,
                // where another job's old results would draw primitives whose state is gone
                let mark = m.r32(c.address);
                self.cur = self.binned.get(&c.address).filter(|b| b.marker == mark).cloned();
                self.vs_cache.clear();
                self.vs_shared = Default::default();
                if self.cur.is_none() { crate::log(&format!("v3d: render job: no binned primitives for tile lists at {:#x}", c.address)); }
            }
            122 => self.supertile = MulticoreRenderingSupertileCfg::parse(b),
            124 => { let c = TileCoordinates::parse(b); self.start_tile(c.tile_column_number, c.tile_row_number); }
            125 => {}
            20 => { let c = StartAddressOfGenericTileList::parse(b); self.generic = (c.start, c.end); }
            23 => {
                self.queue_supertile(b);
                self.flush(m, code);
            }
            30 => self.load(&LoadTileBufferGeneral::parse(b), m),
            29 => self.store(&StoreTileBufferGeneral::parse(b), m),
            25 => { let c = ClearTileBuffers::parse(b); self.clear(c.clear_all_render_targets, c.clear_z_stencil_buffer); }
            21 => self.render_tile(m, code),
            26 | 27 | 56 | 54 | 19 | 126 => {}
            _ => {}
        }
    }

    /// The tiles of a supertile, in order, onto the queue.
    fn queue_supertile(&mut self, b: &[u8; 32]) {
        let c = SupertileCoordinates::parse(b);
        let (sw, sh) = (self.supertile.supertile_width_in_tiles.max(1), self.supertile.supertile_height_in_tiles.max(1));
        let (tx_n, ty_n) = (self.width.div_ceil(self.tile_w), self.height.div_ceil(self.tile_h));
        for ty in c.row_number_in_supertiles * sh..(c.row_number_in_supertiles + 1) * sh {
            for tx in c.column_number_in_supertiles * sw..(c.column_number_in_supertiles + 1) * sw {
                if tx < tx_n && ty < ty_n { self.queue.push((tx, ty)); }
            }
        }
    }

    /// One tile: its generic tile list (loads, the binned primitives, stores).
    fn run_tile(&mut self, tx: u32, ty: u32, m: &mut Gmem, code: &mut Code) {
        self.start_tile(tx, ty);
        let mut sub = bin::Cl::new(self.generic.0, self.generic.1);
        while let Some((op, b)) = sub.next(m) {
            if op == 27 { break; }
            self.packet(op, &b, m, code);
        }
    }

    /// Render the queued tiles. Tiles are independent on the hardware too (its QPUs shade
    /// several at once), so with more than one render thread each worker takes the next tile
    /// from the queue, with its own tile buffer, shader thread and instruction cache. A tile
    /// starts from the tile buffer the previous tile left (Mesa clears it with the previous
    /// tile's stores): every worker starts from the buffer as it is now, which is what each
    /// tile sees serially when the stores clear or the next tile loads.
    fn flush(&mut self, m: &mut Gmem, code: &mut Code) {
        if self.queue.is_empty() { return; }
        let tiles = std::mem::take(&mut self.queue);
        let workers = crate::par::threads().min(tiles.len());
        if workers <= 1 {
            for (tx, ty) in tiles { self.run_tile(tx, ty, m, code); }
            return;
        }
        #[cfg(not(target_arch = "wasm32"))]
        self.run_parallel(&tiles, workers, m, code);
    }

    /// Shade the vertices of the draws these tiles use before rendering them, all workers
    /// taking 128-vertex chunks (the shader runs 16 vertices at a time, each batch on its own,
    /// so chunks give the same outputs as one run). Otherwise the first worker to reach a big
    /// draw would shade all of it while the others wait.
    #[cfg(not(target_arch = "wasm32"))]
    fn shade_vertices(&mut self, tiles: &[(u32, u32)], workers: usize, m: &mut Gmem, code: &mut Code) {
        const CHUNK: usize = 8 * LANES;
        let Some(binned) = self.cur.clone() else { return };
        let mut want = vec![false; binned.draws.len()];
        for &(tx, ty) in tiles {
            if tx >= binned.tiles_x || ty >= binned.tiles_y { continue; }
            for &pi in &binned.lists[(ty * binned.tiles_x + tx) as usize] { want[binned.prims[pi as usize].draw as usize] = true; }
        }
        {
            let shared = self.vs_shared.lock().unwrap_or_else(|e| e.into_inner());
            for (k, w) in want.iter_mut().enumerate() { if shared.get(&(k as u32)).is_some_and(|c| c.get().is_some()) { *w = false; } }
        }
        let items: Vec<(usize, usize)> = want.iter().enumerate().filter(|(_, &w)| w)
            .flat_map(|(k, _)| (0..binned.draws[k].verts.len().div_ceil(CHUNK)).map(move |c| (k, c))).collect();
        if items.len() < 2 { return; }
        let draws = &binned.draws;
        let outs = crate::par::items(workers.min(items.len()), items.len(), m, code, &|k, i, wm, wc| {
            let (draw, c) = items[i];
            let d = &draws[draw];
            let verts = &d.verts[c * CHUNK..((c + 1) * CHUNK).min(d.verts.len())];
            let mut ins = 0;
            let o = bin::run_vs(wm, wc, &mut ins, d, false, verts, 2 * k as u32 + 1);
            (o, ins)
        });
        // items are in draw then chunk order: sorting by item puts each draw's chunks together
        let mut outs = outs;
        outs.sort_by_key(|o| o.0);
        let mut shared = self.vs_shared.lock().unwrap_or_else(|e| e.into_inner());
        let mut k = 0;
        while k < outs.len() {
            let draw = items[outs[k].0].0;
            let mut o = Vec::with_capacity(draws[draw].verts.len());
            while k < outs.len() && items[outs[k].0].0 == draw {
                self.insns += outs[k].1 .1;
                o.append(&mut outs[k].1 .0);
                k += 1;
            }
            let _ = shared.entry(draw as u32).or_default().set(Arc::new(VsOut::new(&draws[draw], o)));
        }
    }

    #[cfg(not(target_arch = "wasm32"))]
    fn run_parallel(&mut self, tiles: &[(u32, u32)], workers: usize, m: &mut Gmem, code: &mut Code) {
        use std::sync::atomic::{AtomicUsize, Ordering};
        self.shade_vertices(tiles, workers, m, code);
        let next = AtomicUsize::new(0);
        let n = tiles.len();
        let outs: Vec<_> = std::thread::scope(|s| {
            let handles: Vec<_> = (0..workers).map(|k| {
                let (mut w, mut wm, mut wc) = (self.fork(), m.share(), code.clone());
                w.worker = k as u32;
                let next = &next;
                s.spawn(move || {
                    let mut last = None;
                    loop {
                        let i = next.fetch_add(1, Ordering::Relaxed);
                        if i >= n { break; }
                        w.run_tile(tiles[i].0, tiles[i].1, &mut wm, &mut wc);
                        if i == n - 1 { last = Some((w.tb.clone(), w.tile)); }
                    }
                    (w.occlusion, w.insns, wc.runaway, wm.faults, last)
                })
            }).collect();
            handles.into_iter().map(|h| h.join().unwrap_or_else(|e| std::panic::resume_unwind(e))).collect()
        });
        for (occlusion, insns, runaway, faults, last) in outs {
            for (a, k) in occlusion { *self.occlusion.entry(a).or_default() += k; }
            self.insns += insns;
            code.runaway |= runaway;
            m.faults += faults;
            if let Some((tb, tile)) = last { self.tb = tb; self.tile = tile; }
        }
    }

    /// A render worker: this job's state, its own tile buffer (a copy of the current one) and
    /// shader thread.
    #[cfg(not(target_arch = "wasm32"))]
    fn fork(&self) -> Render<'r> {
        Render {
            binned: self.binned, cur: self.cur.clone(), width: self.width, height: self.height, msaa: self.msaa, nrt: self.nrt, rt: self.rt,
            depth_type: self.depth_type, early_ds_clear: self.early_ds_clear, z_clear: self.z_clear, s_clear: self.s_clear,
            tile_w: self.tile_w, tile_h: self.tile_h, tb: self.tb.clone(), tile: self.tile, generic: self.generic, supertile: self.supertile,
            vs_cache: HashMap::new(), vs_shared: self.vs_shared.clone(), occlusion: HashMap::new(), insns: 0, thread: None, queue: Vec::new(), worker: 0,
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
        if crate::draws_enabled() {
            crate::log_once(&format!("v3d load: buf {buf} fmt {} mem {} stride/ub {} rt {:?}", c.input_image_format, c.memory_format, c.height_in_ub_or_stride, self.rt.map(|r| (r.ty, r.bpp))));
        }
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
        if crate::log_enabled() && self.tile == (0, 0) { crate::log(&format!("v3d: store buf {buf} fmt {} mem {} addr {:#x} rb_swap {} stride {} dither {} type {:?}", c.output_image_format, c.memory_format, c.address, c.r_b_swap, c.height_in_ub_or_stride, c.dither_mode, self.rt.map(|r| (r.ty, r.bpp)))); }
        if buf == 8 { if c.clear_buffer_being_stored { self.clear(true, true); } return; }
        if crate::draws_enabled() {
            crate::log_once(&format!("v3d store: buf {buf} fmt {} mem {} rb_swap {} dither {} rt {:?} clear {}", c.output_image_format, c.memory_format, c.r_b_swap as u8, c.dither_mode,
                self.rt.map(|r| (r.ty, r.bpp, r.clamp)), c.clear_buffer_being_stored as u8));
        }
        let fmt = c.output_image_format;
        let cpp = formats::out_cpp(fmt);
        if cpp == 0 { formats::encode(fmt, [0; 4], &mut []); return; }
        let (t, stride, ih) = self.surface(c.memory_format, cpp, c.height_in_ub_or_stride);
        let all = c.decimate_mode == 3 && self.msaa;
        let resolve = c.decimate_mode == 1 && self.msaa;
        let (x0, y0) = (self.tile.0 * self.tile_w, self.tile.1 * self.tile_h);
        // fast path: a raster colour store, one memory write per row
        if t == Tiling::Raster && (0..=3).contains(&buf) && !all && !resolve && !c.channel_reverse {
            let ty = self.rt[buf as usize].ty;
            let w = self.tile_w.min(self.width.saturating_sub(x0));
            let mut row = vec![0u8; (w * cpp) as usize];
            for y in 0..self.tile_h {
                let gy = y0 + y;
                if gy >= self.height { break; }
                let my = if c.flip_y { c.height.saturating_sub(1).saturating_sub(gy) } else { gy };
                for x in 0..w {
                    let mut v = self.tb.color[buf as usize][self.tb.idx(x, y, 0)];
                    if c.r_b_swap { v.swap(0, 2); }
                    let o = (x * cpp) as usize;
                    if fmt == 27 && ty == it::UNORM8 {
                        for k in 0..4 { row[o + k] = (f32::from_bits(v[k]) * 255.0 + 0.5) as u8; }
                    } else {
                        formats::encode(fmt, v, &mut row[o..o + cpp as usize]);
                    }
                }
                m.write(c.address.wrapping_add(my * stride + x0 * cpp), &row);
            }
            if c.clear_buffer_being_stored { let v = clear_value(ty, self.rt[buf as usize].bpp, self.rt[buf as usize].clear); self.tb.color[buf as usize].iter_mut().for_each(|p| *p = v); }
            return;
        }
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
            let d = &binned.draws[p.draw as usize];
            let outs = match self.vs_cache.get(&p.draw) {
                Some(o) => o.clone(),
                None => {
                    // the first worker to need a draw's vertices shades them; others wait for it
                    let cell = self.vs_shared.lock().unwrap_or_else(|e| e.into_inner()).entry(p.draw).or_default().clone();
                    let mut ins = 0;
                    let o = cell.get_or_init(|| {
                        let o = bin::run_vs(m, code, &mut ins, d, false, &d.verts, 2 * self.worker + 1);
                        if crate::log_enabled() {
                            let row = |k: usize| o[k].iter().map(|v| format!("{:#x}/{}", v, f32::from_bits(*v))).collect::<Vec<_>>().join(" ");
                            crate::log(&format!("v3d: draw {} VS out v0: {}", p.draw, row(0)));
                        }
                        Arc::new(VsOut::new(d, o))
                    }).clone();
                    self.insns += ins;
                    self.vs_cache.insert(p.draw, o.clone());
                    o
                }
            };
            self.draw_prim(d, p, &outs, m, code);
        }
    }

    fn draw_prim(&mut self, d: &DrawState, p: &Prim, vo: &VsOut, m: &mut Gmem, code: &mut Code) {
        let nvary = d.rec.number_of_varyings_in_fragment_shader as usize;
        let n = p.n as usize;
        let orig = [&vo.verts[p.v[0] as usize], &vo.verts[p.v[1] as usize], &vo.verts[p.v[2] as usize]];
        let pv = if d.cfg.direct3d_provoking_vertex { 0 } else { n - 1 };
        let prov: &[u32] = &orig[pv].vary;
        // a vertex the clipper made: position from clip space, varyings weighted
        let cv = |c: &ClipVert| -> RVert {
            let s = bin::to_screen(d, &c.pos);
            let iw = 1.0 / c.pos[3];
            let vary = (0..nvary).map(|j| {
                if d.flat & (1u128 << j) != 0 { return prov[j]; }
                let mut a = 0.0;
                for k in 0..n { a += c.bary[k] * f32::from_bits(orig[k].vary[j]); }
                a.to_bits()
            }).collect();
            RVert { x: s[0], y: s[1], z: c.pos[2] * iw * d.z_scale + d.z_offset, invw: iw, vary }
        };
        match (n, &p.clipped) {
            (3, None) => self.raster_tri(d, p, orig, prov, m, code),
            (3, Some(poly)) => {
                let vs: Vec<RVert> = poly.iter().map(cv).collect();
                for k in 1..vs.len().saturating_sub(1) { self.raster_tri(d, p, [&vs[0], &vs[k], &vs[k + 1]], prov, m, code); }
            }
            (2, Some(poly)) if poly.len() == 2 => { let (a, b) = (cv(&poly[0]), cv(&poly[1])); self.raster_line(d, p, &a, &b, prov, m, code); }
            (2, Some(_)) => {}
            (2, None) => self.raster_line(d, p, orig[0], orig[1], prov, m, code),
            _ => {
                let size = if d.rec.point_size_in_shaded_vertex_data { f32::from_bits(*vo.outs[p.v[0] as usize].get(4).unwrap_or(&0)) } else { d.point_size };
                self.raster_point(d, p, orig[0], size, m, code);
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
        let geo = Geo { v: [v[0], v[1], v[2]], prov, pv, offset, kind: 3, point: None };
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
        let geo = Geo { v: [a, b, b], prov, pv, offset: 0.0, kind: 2, point: None };
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
        let geo = Geo { v: [v, v, v], prov: &v.vary, pv: 0, offset: 0.0, kind: 1, point: Some((l, t, s)) };
        self.shade(d, p, &geo, &quads, m, code);
    }

    /// Fragment shading of covered quads, four quads per QPU thread.
    fn shade(&mut self, d: &DrawState, p: &Prim, geo: &Geo<'_>, quads: &[Quad], m: &mut Gmem, code: &mut Code) {
        let rec = &d.rec;
        if crate::draws_enabled() {
            let b = &d.blend[0];
            let c = &d.cfg;
            crate::log_once(&format!("v3d draw: rt {:?} msaa {} blend {}/{:#x} c(m{} s{} d{}) a(m{} s{} d{}) k {:?} wmask {:#x} z(test {} f{} upd {} fsz {}) st {} ez {} fs {:#x}",
                self.rt.map(|r| (r.ty, r.bpp, r.clamp)), self.msaa, c.blend_enable as u8, d.blend_enables, b.color_blend_mode, b.color_blend_src_factor, b.color_blend_dst_factor,
                b.alpha_blend_mode, b.alpha_blend_src_factor, b.alpha_blend_dst_factor, d.blend_color, d.color_wmasks,
                c.depth_test_function != 7, c.depth_test_function, c.z_updates_enable as u8, rec.fragment_shader_does_z_writes as u8, c.stencil_enable as u8,
                c.early_z_enable as u8, rec.fragment_shader_code_address));
        }
        let late_z = rec.fragment_shader_does_z_writes;
        let (tx0, ty0) = (self.tile.0 * self.tile_w, self.tile.1 * self.tile_h);
        for batch in quads.chunks(4) {
            // a shader in this job ran away: nothing more is shaded, and no early depth/stencil
            // test writes either
            if code.runaway { return; }
            let mut t = self.thread.take().unwrap_or_else(|| Box::new(Thread::new(0, 0, false)));
            t.reset(rec.fragment_shader_code_address, rec.fragment_shader_uniforms_address, rec.fragment_shader_start_in_final_thread_section);
            t.revf = p.reverse;
            t.tidx = 2 * self.worker;
            let mut fe = FsEnv { r: self, d, geo, m, bary: [[0.0; 3]; LANES], z: [0.0; LANES], vary_n: 0, tlb_cfg: !0, rd: 0, pending: Vec::new(), spec: 0, need: 0,
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
                if t.msf.iter().all(|&s| s == 0) { drop(fe); self.thread = Some(t); continue; }
            }
            let before = t.insns;
            let stop = t.run(code, &mut fe, 5_000_000);
            if stop != crate::qpu::Stop::Done { crate::log_once(&format!("v3d: fragment shader stopped: {stop:?}")); }
            if stop == crate::qpu::Stop::Budget { code.runaway = true; }
            if d.occlusion != 0 { *fe.r.occlusion.entry(d.occlusion).or_default() += fe.passed; }
            let n = t.insns - before;
            fe.r.insns += n;
            drop(fe);
            self.thread = Some(t);
        }
    }
}

/// Vertex shader outputs of a draw, and the vertices ready for rasterization.
struct VsOut { outs: Vec<Vec<u32>>, verts: Vec<RVert> }

impl VsOut {
    fn new(d: &DrawState, outs: Vec<Vec<u32>>) -> VsOut {
        let vbase = 4 + d.rec.point_size_in_shaded_vertex_data as usize;
        let nvary = d.rec.number_of_varyings_in_fragment_shader as usize;
        let verts = outs.iter().map(|o| {
            let g = |i: usize| *o.get(i).unwrap_or(&0);
            RVert { x: d.vp_center[0] + (g(0) as i32) as f32 / 256.0, y: d.vp_center[1] + (g(1) as i32) as f32 / 256.0,
                    z: f32::from_bits(g(2)), invw: f32::from_bits(g(3)), vary: (0..nvary).map(|j| g(vbase + j)).collect() }
        }).collect();
        VsOut { outs, verts }
    }
}

#[derive(Clone)]
struct Quad { x: u32, y: u32, cov: [u8; 4], bary: [[f32; 3]; 4] }

struct Geo<'g> {
    v: [&'g RVert; 3],
    prov: &'g [u32],
    pv: usize,
    offset: f32,
    kind: u8,
    point: Option<(f32, f32, f32)>,
}

struct FsEnv<'a, 'r, 'm> {
    r: &'a mut Render<'r>,
    d: &'a DrawState,
    geo: &'a Geo<'a>,
    m: &'a mut Gmem<'m>,
    bary: [[f32; 3]; LANES],
    z: [f32; LANES],
    vary_n: usize,
    tlb_cfg: u32,
    /// words read from the tile buffer since the last `ldtlbu` (which names what to read)
    rd: usize,
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
        // `ldtlbu` starts a read with its config; each `ldtlb` after it returns the next word:
        // f16 colour two channels a word (RG then BA), f32/i32 one a word, depth/stencil one;
        // per-sample mode returns every sample's words in turn.
        if let Some(c) = cfg { self.tlb_cfg = c; self.rd = 0; }
        let spec = self.tlb_cfg & 0xff;
        let (w, rd) = (self.r.tb.ns, self.rd);
        self.rd += 1;
        let per_sample = spec & (1 << 2) == 0 && w > 1;
        let words = match spec >> 6 { 2 => 1, 3 => if spec & 1 != 0 { 2 } else { 1 }, _ => (spec & 3) as usize + 1 };
        let (sample, word) = if per_sample { ((rd / words) as u32 % w, rd % words) } else { (0, rd % words) };
        let rt = (7 - ((spec >> 3) & 7)) as usize;
        let mut out = [0u32; LANES];
        for l in 0..LANES {
            let Some((x, y)) = self.px(t, l) else { continue };
            let i = self.r.tb.idx(x, y, sample);
            match spec >> 6 {
                2 => out[l] = if (spec >> 4) & 3 == 0 { self.r.tb.z[i].to_bits() } else { self.r.tb.s[i] as u32 },
                3 => {
                    let p = self.r.tb.color[rt.min(3)][i];
                    let h = |k: usize| f32_to_f16(f32::from_bits(p[k])) as u32;
                    let k = word * 2;
                    // SWAP_HI_LO: the first channel in the low half (as the write side reads it)
                    out[l] = if spec & 2 != 0 { h(k) | h(k + 1) << 16 } else { h(k + 1) | h(k) << 16 };
                }
                _ => out[l] = self.r.tb.color[rt.min(3)][i][word.min(3)],
            }
        }
        out
    }
}
