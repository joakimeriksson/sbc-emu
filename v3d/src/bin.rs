//! The binning pipeline (control list thread 0): runs the binning control list, tracks the
//! state packets, fetches vertices, runs the coordinate shader, assembles, clips and culls
//! primitives and sorts them into per-tile lists.
//!
//! The tile lists the hardware writes to tile allocation memory are only ever read back by
//! the hardware's own render pass, so they are kept on the host side here, keyed by the
//! tile allocation address that the render control list names
//! ("Multicore Rendering Tile List Set Base"). The vertex shader runs at render time, as on
//! the real GPU.

use std::collections::HashMap;
use std::sync::Arc;

use crate::packets::*;
use crate::qpu::{Env, Stop, Thread, TmuKind, TmuReq, LANES, V};
use crate::{Code, Gmem};

/// Control list reader with sub-list calls.
pub struct Cl {
    pub pc: u32,
    pub end: u32,
    stack: Vec<u32>,
    pub steps: u64,
}

impl Cl {
    pub fn new(start: u32, end: u32) -> Cl { Cl { pc: start, end, stack: Vec::new(), steps: 0 } }

    /// The next packet (opcode, bytes including the opcode); None at the end of the list.
    pub fn next(&mut self, m: &mut Gmem) -> Option<(u8, [u8; 32])> {
        loop {
            if self.stack.is_empty() && self.pc >= self.end { return None; }
            self.steps += 1;
            if self.steps > 50_000_000 { crate::log_once("v3d: runaway control list"); return None; }
            let op = m.r8(self.pc);
            let len = PACKET_LEN[op as usize] as usize;
            if len == 0 {
                crate::log_once(&format!("v3d: unknown control list packet {op} at {:#x}", self.pc));
                return None;
            }
            let mut b = [0u8; 32];
            m.read(self.pc, &mut b[..len]);
            self.pc = self.pc.wrapping_add(len as u32);
            match op {
                0 => return None, // halt
                1 => continue,    // nop
                16 => { self.pc = u32::from_le_bytes(b[1..5].try_into().unwrap()); continue; }
                15 | 17 => { self.stack.push(self.pc); self.pc = u32::from_le_bytes(b[1..5].try_into().unwrap()); continue; }
                18 => { match self.stack.pop() { Some(r) => { self.pc = r; continue; } None => return None } }
                _ => return Some((op, b)),
            }
        }
    }
    /// Call a sub-list (generic tile lists).
    pub fn call(&mut self, addr: u32) { self.stack.push(self.pc); self.pc = addr; }
    pub fn depth(&self) -> usize { self.stack.len() }
}

/// Rendering state captured for each draw call.
#[derive(Clone, Default)]
pub struct DrawState {
    pub rec: GlShaderStateRecord,
    pub attrs: Vec<GlShaderStateAttributeRecord>,
    pub cfg: CfgBits,
    pub stencil_front: StencilCfg,
    pub stencil_back: StencilCfg,
    pub blend_enables: u32,
    /// per render target: blend config
    pub blend: [BlendCfg; 4],
    pub blend_color: [f32; 4],
    pub color_wmasks: u32,
    pub clip: [i32; 4],
    pub vp_center: [f32; 2],
    pub vp_half: [f32; 2],
    pub z_scale: f32,
    pub z_offset: f32,
    pub z_min: f32,
    pub z_max: f32,
    pub depth_offset: DepthOffset,
    pub point_size: f32,
    pub line_width: f32,
    pub flat: u128,
    pub noperspective: u128,
    pub centroid: u128,
    pub sample_mask: u32,
    pub occlusion: u32,
    pub mode: u32,
    /// the vertices this draw's primitives use: (vertex index, instance)
    pub verts: Vec<(u32, u32)>,
    /// render-time vertex shader outputs (filled lazily by the render job)
    pub base_instance: u32,
}

#[derive(Clone, Copy, Debug)]
pub struct ClipVert {
    /// clip-space position
    pub pos: [f32; 4],
    /// weights of the primitive's original vertices
    pub bary: [f32; 3],
}

#[derive(Clone, Debug)]
pub struct Prim {
    pub draw: u32,
    /// 1 = point, 2 = line, 3 = triangle
    pub n: u8,
    /// indices into the draw's `verts`
    pub v: [u32; 3],
    pub prim_id: u32,
    pub reverse: bool,
    /// set when clipping replaced the primitive by this polygon (a fan)
    pub clipped: Option<Arc<Vec<ClipVert>>>,
}

/// What a bin job produced for its render job.
#[derive(Default)]
pub struct Binned {
    pub tiles_x: u32,
    pub tiles_y: u32,
    pub tile_w: u32,
    pub tile_h: u32,
    pub draws: Vec<Arc<DrawState>>,
    pub prims: Vec<Prim>,
    /// per tile (row-major): primitive indices in order
    pub lists: Vec<Vec<u32>>,
}

pub fn tile_size(num_rts: u32, max_bpp: u32, msaa: bool, double_buffer: bool) -> (u32, u32) {
    const SIZES: [(u32, u32); 7] = [(64, 64), (64, 32), (32, 32), (32, 16), (16, 16), (16, 8), (8, 8)];
    let mut idx = match num_rts { 0 | 1 => 0, 2 => 1, 3 | 4 => 2, _ => 3 };
    if msaa { idx += 2 } else if double_buffer { idx += 1 }
    idx += max_bpp as usize;
    SIZES[idx.min(6)]
}

// ------------------------------------------------------------------------- vertex shading

pub struct VsEnv<'a, 'm> { pub m: &'a mut Gmem<'m> }

impl Env for VsEnv<'_, '_> {
    fn read32(&mut self, va: u32) -> u32 { self.m.r32(va) }
    fn write32(&mut self, va: u32, v: u32) { self.m.w32(va, v) }
    fn tmu(&mut self, t: &Thread, kind: TmuKind, cfg: &[u32], req: &TmuReq, lanes: u16) -> Vec<V> {
        crate::tmu::lookup(self.m, t, kind, cfg, req, lanes, false)
    }
}

#[allow(clippy::too_many_arguments)]
fn fetch_attr(m: &mut Gmem, a: &GlShaderStateAttributeRecord, k: usize, defaults: u32, vtx: u32, instance: u32, base_instance: u32, n: u32, out: &mut Vec<u32>) {
    let idx = if a.instance_divisor != 0 { instance / a.instance_divisor + base_instance } else { vtx };
    let addr = a.address.wrapping_add(idx.wrapping_mul(a.stride));
    let vec = if a.vec_size == 0 { 4 } else { a.vec_size };
    let size = match a.type_ { 1 => 2, 2 | 3 | 6 => 4, 4 => 1, 5 => 2, _ => 4 };
    for c in 0..n {
        if c >= vec || a.type_ == 7 && c >= 4 {
            let d = if defaults != 0 { m.r32(defaults + (k as u32 * 4 + c) * 4) } else if c == 3 { if a.read_as_int_uint { 1 } else { 1f32.to_bits() } } else { 0 };
            out.push(d);
            continue;
        }
        let v = if a.type_ == 7 {
            let w = m.r32(addr);
            let (bits, sh) = if c < 3 { (10, c * 10) } else { (2, 30) };
            let raw = (w >> sh) & ((1 << bits) - 1);
            convert_int(raw, bits, a.signed_int_type, a.normalized_int_type, a.read_as_int_uint)
        } else {
            let ca = addr.wrapping_add(c * size);
            match a.type_ {
                1 => crate::qpu::f16_to_f32(m.r16(ca)).to_bits(),
                2 => m.r32(ca),
                3 => (m.r32(ca) as i32 as f32 / 65536.0).to_bits(),
                4 => convert_int(m.r8(ca) as u32, 8, a.signed_int_type, a.normalized_int_type, a.read_as_int_uint),
                5 => convert_int(m.r16(ca) as u32, 16, a.signed_int_type, a.normalized_int_type, a.read_as_int_uint),
                _ => convert_int(m.r32(ca), 32, a.signed_int_type, a.normalized_int_type, a.read_as_int_uint),
            }
        };
        out.push(v);
    }
}

fn convert_int(raw: u32, bits: u32, signed: bool, norm: bool, as_int: bool) -> u32 {
    let s = if signed && bits < 32 { (((raw << (32 - bits)) as i32) >> (32 - bits)) as u32 } else { raw };
    if as_int { return s; }
    let f = if norm {
        if signed { ((s as i32) as f64 / ((1u64 << (bits - 1)) - 1) as f64).max(-1.0) as f32 } else { (s as f64 / ((1u64 << bits) - 1) as f64) as f32 }
    } else if signed { s as i32 as f32 } else { s as f32 };
    f.to_bits()
}

/// Run the coordinate (`coord`) or vertex shader of a draw over `verts`; returns the VPM
/// output rows of each vertex.
pub fn run_vs(m: &mut Gmem, code: &mut Code, insns: &mut u64, d: &DrawState, coord: bool, verts: &[(u32, u32)]) -> Vec<Vec<u32>> {
    let r = &d.rec;
    let (pc, unif, final_sec, separate, out_size) = if coord {
        (r.coordinate_shader_code_address, r.coordinate_shader_uniforms_address, r.coordinate_shader_start_in_final_thread_section,
         r.coordinate_shader_has_separate_input_and_output_vpm_blocks, r.coordinate_shader_output_vpm_segment_size)
    } else {
        (r.vertex_shader_code_address, r.vertex_shader_uniforms_address, r.vertex_shader_start_in_final_thread_section,
         r.vertex_shader_has_separate_input_and_output_vpm_blocks, r.vertex_shader_output_vpm_segment_size)
    };
    let (iid, biid, vid) = if coord {
        (r.instance_id_read_by_coordinate_shader, r.base_instance_id_read_by_coordinate_shader, r.vertex_id_read_by_coordinate_shader)
    } else {
        (r.instance_id_read_by_vertex_shader, r.base_instance_id_read_by_vertex_shader, r.vertex_id_read_by_vertex_shader)
    };
    let defaults = r.address_of_default_attribute_values;
    let mut out = Vec::with_capacity(verts.len());
    let mut env = VsEnv { m };
    for chunk in verts.chunks(LANES) {
        // VPM inputs for each lane
        let mut rows: Vec<Vec<u32>> = Vec::new();
        for &(vtx, inst) in chunk {
            let mut v = Vec::new();
            if iid { v.push(inst); }
            if biid { v.push(d.base_instance); }
            if vid { v.push(vtx); }
            for (k, a) in d.attrs.iter().enumerate() {
                let n = if coord { a.number_of_values_read_by_coordinate_shader } else { a.number_of_values_read_by_vertex_shader };
                if n > 0 { fetch_attr(env.m, a, k, defaults, vtx, inst, d.base_instance, n, &mut v); }
            }
            rows.push(v);
        }
        let in_rows = rows.iter().map(|r| r.len()).max().unwrap_or(0);
        let mut t = Thread::new(pc, unif, final_sec);
        t.valid = if chunk.len() == LANES { 0xffff } else { (1u16 << chunk.len()) - 1 };
        for l in chunk.len()..LANES { t.msf[l] = 0; }
        let in_seg = if separate { in_rows.max(1) } else { 0 };
        t.vpm = vec![[0u32; LANES]; in_seg.max(in_rows) + (out_size as usize * 8).max(64)];
        for (l, row) in rows.iter().enumerate() { for (k, &v) in row.iter().enumerate() { t.vpm[k][l] = v; } }
        t.vpm_out_base = in_seg;
        let before = t.insns;
        match t.run(code, &mut env, 20_000_000) {
            Stop::Done => {}
            s => crate::log_once(&format!("v3d: {} shader stopped: {s:?}", if coord { "coordinate" } else { "vertex" })),
        }
        *insns += t.insns - before;
        for l in 0..chunk.len() {
            out.push(t.vpm[t.vpm_out_base..].iter().map(|row| row[l]).collect());
        }
    }
    out
}

// ------------------------------------------------------------------------- the bin job

struct State {
    d: DrawState,
    tiles: (u32, u32, u32, u32),
    width: u32,
    height: u32,
    index_addr: u32,
    index_size: u32,
    base_vertex: i32,
    base_instance: u32,
    prim_id: u32,
    shader_addr: u32,
    nattr: u32,
}

#[allow(clippy::too_many_arguments)]
pub fn run(start: u32, end: u32, qma: u32, _qms: u32, _qts: u32, m: &mut Gmem, code: &mut Code, insns: &mut u64) -> Option<Binned> {
    let mut cl = Cl::new(start, end);
    let mut st = State {
        d: DrawState { clip: [0, 0, i32::MAX, i32::MAX], point_size: 1.0, line_width: 1.0, z_scale: 0.5, z_offset: 0.5, z_min: 0.0, z_max: 1.0, sample_mask: 0xf, ..Default::default() },
        tiles: (1, 1, 64, 64), width: 0, height: 0, index_addr: 0, index_size: 0, base_vertex: 0, base_instance: 0, prim_id: 0, shader_addr: 0, nattr: 0,
    };
    let mut out = Binned::default();
    let mut configured = false;
    while let Some((op, b)) = cl.next(m) {
        match op {
            120 => {
                let c = TileBinningModeCfg::parse(&b);
                st.width = c.width_in_pixels;
                st.height = c.height_in_pixels;
                let (tw, th) = tile_size(c.number_of_render_targets, c.maximum_bpp_of_all_render_targets, c.multisample_mode_4x, c.double_buffer_in_non_ms_mode);
                let (tx, ty) = ((st.width + tw - 1) / tw, (st.height + th - 1) / th);
                st.tiles = (tx, ty, tw, th);
                out.tiles_x = tx; out.tiles_y = ty; out.tile_w = tw; out.tile_h = th;
                out.lists = vec![Vec::new(); (tx * ty) as usize];
                configured = true;
            }
            107 => {
                let c = ClipWindow::parse(&b);
                st.d.clip = [c.clip_window_left_pixel_coordinate as i32, c.clip_window_bottom_pixel_coordinate as i32,
                             (c.clip_window_left_pixel_coordinate + c.clip_window_width_in_pixels) as i32,
                             (c.clip_window_bottom_pixel_coordinate + c.clip_window_height_in_pixels) as i32];
            }
            108 => { let c = ViewportOffset::parse(&b); st.d.vp_center = [c.coarse_x as f32 * 64.0 + c.fine_x, c.coarse_y as f32 * 64.0 + c.fine_y]; }
            109 => { let c = ClipperZMinMaxClippingPlanes::parse(&b); st.d.z_min = c.minimum_zw; st.d.z_max = c.maximum_zw; }
            110 => { let c = ClipperXyScaling::parse(&b); st.d.vp_half = [c.viewport_half_width_in_1_256th_of_pixel / 256.0, c.viewport_half_height_in_1_256th_of_pixel / 256.0]; }
            111 => { let c = ClipperZScaleAndOffset::parse(&b); st.d.z_scale = c.viewport_z_scale_zc_to_zs; st.d.z_offset = c.viewport_z_offset_zc_to_zs; }
            96 => st.d.cfg = CfgBits::parse(&b),
            80 => {
                let s = StencilCfg::parse(&b);
                if s.front_config { st.d.stencil_front = s; }
                if s.back_config { st.d.stencil_back = s; }
            }
            83 => st.d.blend_enables = BlendEnables::parse(&b).mask,
            84 => {
                let c = BlendCfg::parse(&b);
                for rt in 0..4 { if c.render_target_mask & (1 << rt) != 0 { st.d.blend[rt] = c; } }
            }
            86 => {
                let c = BlendConstantColor::parse(&b);
                let h = |v: u32| crate::qpu::f16_to_f32(v as u16);
                st.d.blend_color = [h(c.red_f16), h(c.green_f16), h(c.blue_f16), h(c.alpha_f16)];
            }
            87 => st.d.color_wmasks = ColorWriteMasks::parse(&b).mask,
            88 => st.d.centroid = 0,
            89 => { let c = CentroidFlags::parse(&b); flags(&mut st.d.centroid, c.centroid_flags_for_varyings_v0_24, c.action_for_centroid_flags_of_lower_numbered_varyings, c.action_for_centroid_flags_of_higher_numbered_varyings, c.varying_offset_v0); }
            97 => st.d.flat = 0,
            98 => { let c = FlatShadeFlags::parse(&b); flags(&mut st.d.flat, c.flat_shade_flags_for_varyings_v0_24, c.action_for_flat_shade_flags_of_lower_numbered_varyings, c.action_for_flat_shade_flags_of_higher_numbered_varyings, c.varying_offset_v0); }
            99 => st.d.noperspective = 0,
            100 => { let c = NonPerspectiveFlags::parse(&b); flags(&mut st.d.noperspective, c.non_perspective_flags_for_varyings_v0_24, c.action_for_non_perspective_flags_of_lower_numbered_varyings, c.action_for_non_perspective_flags_of_higher_numbered_varyings, c.varying_offset_v0); }
            91 => st.d.sample_mask = SampleState::parse(&b).mask,
            92 => st.d.occlusion = OcclusionQueryCounter::parse(&b).address,
            104 => st.d.point_size = PointSize::parse(&b).point_size,
            105 => st.d.line_width = LineWidth::parse(&b).line_width,
            106 => st.d.depth_offset = DepthOffset::parse(&b),
            44 => { let c = IndexBufferSetup::parse(&b); st.index_addr = c.address; st.index_size = c.size; }
            43 => { let c = BaseVertexBaseInstance::parse(&b); st.base_vertex = c.base_vertex as i32; st.base_instance = c.base_instance; }
            55 => st.prim_id = SetPrimitiveid::parse(&b).primitive_id,
            64..=67 => {
                let c = GlShaderState::parse(&b);
                if op != 64 { crate::log_once("v3d: geometry/tessellation shaders not modelled"); }
                st.shader_addr = c.address;
                st.nattr = c.number_of_attribute_arrays;
                let rb = m.bytes(st.shader_addr, GlShaderStateRecord::LEN);
                st.d.rec = GlShaderStateRecord::parse(&rb);
                st.d.attrs = (0..st.nattr.max(1)).map(|k| {
                    let ab = m.bytes(st.shader_addr + GlShaderStateRecord::LEN as u32 + k * GlShaderStateAttributeRecord::LEN as u32, GlShaderStateAttributeRecord::LEN);
                    GlShaderStateAttributeRecord::parse(&ab)
                }).collect();
            }
            32 => {
                let c = IndexedPrimList::parse(&b);
                draw(&mut st, &mut out, m, code, insns, c.mode, c.length, Some((c.index_type, c.index_offset, c.enable_primitive_restarts)), 0, 1, 0);
            }
            34 => {
                let c = IndexedInstancedPrimList::parse(&b);
                draw(&mut st, &mut out, m, code, insns, c.mode, c.instance_length, Some((c.index_type, c.index_offset, c.enable_primitive_restarts)), 0, c.number_of_instances, 0);
            }
            36 => {
                let c = VertexArrayPrims::parse(&b);
                draw(&mut st, &mut out, m, code, insns, c.mode, c.length, None, c.index_of_first_vertex, 1, 0);
            }
            38 => {
                let c = VertexArrayInstancedPrims::parse(&b);
                draw(&mut st, &mut out, m, code, insns, c.mode, c.instance_length, None, c.index_of_first_vertex, c.number_of_instances, 0);
            }
            39 => {
                let c = VertexArraySingleInstancePrims::parse(&b);
                draw(&mut st, &mut out, m, code, insns, c.mode, c.instance_length, None, c.index_of_first_vertex, 1, c.instance_id);
            }
            33 => {
                let c = IndirectIndexedInstancedPrimList::parse(&b);
                for k in 0..c.number_of_draw_indirect_indexed_records {
                    let a = c.address.wrapping_add(k * c.stride_in_multiples_of_4_bytes.max(5) * 4);
                    let (count, inst, first, bv, bi) = (m.r32(a), m.r32(a + 4), m.r32(a + 8), m.r32(a + 12), m.r32(a + 16));
                    st.base_vertex = bv as i32; st.base_instance = bi;
                    let off = first * [1, 2, 4][(c.index_type as usize).min(2)];
                    draw(&mut st, &mut out, m, code, insns, c.mode, count, Some((c.index_type, off, c.enable_primitive_restarts)), 0, inst, 0);
                }
            }
            37 => {
                let c = IndirectVertexArrayInstancedPrims::parse(&b);
                for k in 0..c.number_of_draw_indirect_array_records {
                    let a = c.address.wrapping_add(k * c.stride_in_multiples_of_4_bytes.max(4) * 4);
                    let (count, inst, first, bi) = (m.r32(a), m.r32(a + 4), m.r32(a + 8), m.r32(a + 12));
                    st.base_instance = bi;
                    draw(&mut st, &mut out, m, code, insns, c.mode, count, None, first, inst, 0);
                }
            }
            _ => {}
        }
    }
    let _ = qma;
    if !configured { return None; }
    crate::log(&format!("v3d: bin {}x{} tiles of {}x{}: {} draws, {} primitives", out.tiles_x, out.tiles_y, out.tile_w, out.tile_h, out.draws.len(), out.prims.len()));
    Some(out)
}

fn flags(f: &mut u128, bits: u32, lower: u32, higher: u32, offset: u32) {
    let base = offset * 24;
    let apply = |f: &mut u128, range: std::ops::Range<u32>, action: u32| {
        for k in range { match action { 1 => *f &= !(1u128 << k), 2 => *f |= 1u128 << k, _ => {} } }
    };
    apply(f, 0..base.min(128), lower);
    apply(f, (base + 24).min(128)..128, higher);
    for k in 0..24 { let i = base + k; if i < 128 { if bits & (1 << k) != 0 { *f |= 1u128 << i } else { *f &= !(1u128 << i) } } }
}

/// One draw call: `count` vertices (or indices) per instance.
#[allow(clippy::too_many_arguments)]
fn draw(st: &mut State, out: &mut Binned, m: &mut Gmem, code: &mut Code, insns: &mut u64, mode: u32, count: u32,
        indexed: Option<(u32, u32, bool)>, first: u32, instances: u32, instance0: u32) {
    if count == 0 || instances == 0 || out.lists.is_empty() { return; }
    let mode = mode & 0xf;
    let mut d = st.d.clone();
    d.mode = mode;
    d.base_instance = st.base_instance;
    // vertex indices of each instance, with primitive restarts as None
    let mut seq: Vec<Option<u32>> = Vec::with_capacity(count as usize);
    match indexed {
        Some((ty, off, restart)) => {
            let sz = [1u32, 2, 4][(ty as usize).min(2)];
            let restart_val = [0xffu32, 0xffff, 0xffff_ffff][(ty as usize).min(2)];
            for i in 0..count {
                let a = st.index_addr.wrapping_add(off + i * sz);
                let v = match sz { 1 => m.r8(a) as u32, 2 => m.r16(a) as u32, _ => m.r32(a) };
                if restart && v == restart_val { seq.push(None); } else { seq.push(Some((v as i32).wrapping_add(st.base_vertex) as u32)); }
            }
        }
        None => for i in 0..count { seq.push(Some(first + i)); },
    }
    // primitives as index triples into `seq`
    let mut prims: Vec<(u8, [usize; 3])> = Vec::new();
    let d3d_pv = d.cfg.direct3d_provoking_vertex;
    let mut runs: Vec<Vec<usize>> = vec![Vec::new()];
    for (i, v) in seq.iter().enumerate() { if v.is_some() { runs.last_mut().unwrap().push(i); } else { runs.push(Vec::new()); } }
    for r in &runs {
        let n = r.len();
        match mode {
            0 => for &i in r { prims.push((1, [i, i, i])); },
            1 => for k in (0..n.saturating_sub(1)).step_by(2) { prims.push((2, [r[k], r[k + 1], r[k + 1]])); },
            2 | 3 => {
                for k in 0..n.saturating_sub(1) { prims.push((2, [r[k], r[k + 1], r[k + 1]])); }
                if mode == 2 && n > 2 { prims.push((2, [r[n - 1], r[0], r[0]])); }
            }
            4 => for k in (0..n.saturating_sub(2)).step_by(3) { prims.push((3, [r[k], r[k + 1], r[k + 2]])); },
            5 => for k in 0..n.saturating_sub(2) {
                let t = if k % 2 == 0 { [r[k], r[k + 1], r[k + 2]] } else if d3d_pv { [r[k], r[k + 2], r[k + 1]] } else { [r[k + 1], r[k], r[k + 2]] };
                prims.push((3, t));
            },
            6 => for k in 1..n.saturating_sub(1) {
                prims.push((3, if d3d_pv { [r[k], r[k + 1], r[0]] } else { [r[0], r[k], r[k + 1]] }));
            },
            _ => { crate::log_once(&format!("v3d: primitive mode {mode} not modelled")); }
        }
    }
    if prims.is_empty() { return; }
    // unique vertices of all instances
    let mut index: HashMap<(u32, u32), u32> = HashMap::new();
    let mut verts: Vec<(u32, u32)> = Vec::new();
    let mut prim_verts: Vec<(u8, [u32; 3], u32)> = Vec::with_capacity(prims.len() * instances as usize);
    for inst in 0..instances {
        let instance = instance0 + inst;
        for (pi, &(n, idx)) in prims.iter().enumerate() {
            let mut v = [0u32; 3];
            for k in 0..3 {
                let key = (seq[idx[k]].unwrap(), instance);
                v[k] = *index.entry(key).or_insert_with(|| { verts.push(key); verts.len() as u32 - 1 });
            }
            prim_verts.push((n, v, st.prim_id + pi as u32));
        }
    }
    // coordinate shader: clip-space positions (and point sizes)
    let cs = run_vs(m, code, insns, &d, true, &verts);
    let psize = d.rec.point_size_in_shaded_vertex_data;
    let pos: Vec<[f32; 4]> = cs.iter().map(|o| [0, 1, 2, 3].map(|k| f32::from_bits(*o.get(k).unwrap_or(&0)))).collect();
    let psz: Vec<f32> = cs.iter().map(|o| if psize { f32::from_bits(*o.get(6).unwrap_or(&0)) } else { d.point_size }).collect();
    d.verts = verts;
    let draw_id = out.draws.len() as u32;
    let (tx, ty, tw, th) = st.tiles;
    let clip = d.clip;
    let mut added = false;
    for (n, v, prim_id) in prim_verts {
        // screen-space bounding box (or clipping) from the clip-space positions
        let p = [pos[v[0] as usize], pos[v[1] as usize], pos[v[2] as usize]];
        let mut clipped = None;
        let needs_clip = d.rec.enable_clipping && p[..n as usize].iter().any(|q| outside(&d, q));
        let screen: Vec<[f32; 2]> = if needs_clip {
            let poly = clip_poly(&d, &p[..n as usize]);
            if poly.len() < n as usize { continue; }
            let s: Vec<[f32; 2]> = poly.iter().map(|c| to_screen(&d, &c.pos)).collect();
            clipped = Some(Arc::new(poly));
            s
        } else {
            if p[..n as usize].iter().any(|q| q[3] <= 0.0) { continue; }
            p[..n as usize].iter().map(|q| to_screen(&d, q)).collect()
        };
        // culling (triangles)
        let mut reverse = false;
        if n == 3 {
            let mut area = 0.0f64;
            for k in 0..screen.len() {
                let (a, b) = (screen[k], screen[(k + 1) % screen.len()]);
                area += a[0] as f64 * b[1] as f64 - b[0] as f64 * a[1] as f64;
            }
            if area == 0.0 { continue; }
            // positive area: clockwise with y growing downwards in memory; with "clockwise
            // primitives" set (Mesa sets it for GL's counter-clockwise front faces, as GL's y
            // grows the other way) those face forwards
            let cw = area > 0.0;
            reverse = cw == d.cfg.clockwise_primitives;
            if reverse && !d.cfg.enable_reverse_facing_primitive { continue; }
            if !reverse && !d.cfg.enable_forward_facing_primitive { continue; }
        } else if !d.cfg.enable_forward_facing_primitive { continue; }
        let mut r = screen.iter().fold([f32::MAX, f32::MAX, f32::MIN, f32::MIN], |b, s| [b[0].min(s[0]), b[1].min(s[1]), b[2].max(s[0]), b[3].max(s[1])]);
        let grow = if n == 1 { psz[v[0] as usize].max(1.0) / 2.0 + 1.0 } else if n == 2 { d.line_width.max(1.0) / 2.0 + 1.0 } else { 1.0 };
        r = [r[0] - grow, r[1] - grow, r[2] + grow, r[3] + grow];
        let x0 = (r[0].max(clip[0] as f32).max(0.0)) as i64;
        let y0 = (r[1].max(clip[1] as f32).max(0.0)) as i64;
        let x1 = (r[2].min(clip[2] as f32 - 1.0).min(st.width as f32 - 1.0)) as i64;
        let y1 = (r[3].min(clip[3] as f32 - 1.0).min(st.height as f32 - 1.0)) as i64;
        if x1 < x0 || y1 < y0 { continue; }
        let idx = out.prims.len() as u32;
        out.prims.push(Prim { draw: draw_id, n, v, prim_id, reverse, clipped });
        added = true;
        let (t0x, t0y, t1x, t1y) = ((x0 / tw as i64) as u32, (y0 / th as i64) as u32, ((x1 / tw as i64) as u32).min(tx - 1), ((y1 / th as i64) as u32).min(ty - 1));
        for yy in t0y..=t1y { for xx in t0x..=t1x { out.lists[(yy * tx + xx) as usize].push(idx); } }
    }
    if added { out.draws.push(Arc::new(d)); } else { out.draws.push(Arc::new(DrawState::default())); }
    st.prim_id = st.prim_id.wrapping_add(prims.len() as u32);
}

/// Window coordinates of a clip-space position.
pub fn to_screen(d: &DrawState, p: &[f32; 4]) -> [f32; 2] {
    let iw = 1.0 / p[3];
    [d.vp_center[0] + p[0] * iw * d.vp_half[0], d.vp_center[1] + p[1] * iw * d.vp_half[1]]
}

/// Clip planes as (a, b, c, d) with a*x + b*y + c*z + d*w >= 0 inside.
fn planes(d: &DrawState) -> Vec<[f32; 4]> {
    // guard band: keep window coordinates within +-16k pixels
    let gx = 16384.0 / d.vp_half[0].abs().max(1.0);
    let gy = 16384.0 / d.vp_half[1].abs().max(1.0);
    let mut v = vec![[0.0, 0.0, 0.0, 1.0], [-1.0, 0.0, 0.0, gx], [1.0, 0.0, 0.0, gx], [0.0, -1.0, 0.0, gy], [0.0, 1.0, 0.0, gy]];
    if d.z_scale != 0.0 {
        let (zmin, zmax) = (d.z_min.min(d.z_max), d.z_min.max(d.z_max));
        // zs = z*scale/w + offset in [zmin, zmax]
        v.push([0.0, 0.0, d.z_scale, d.z_offset - zmin]);
        v.push([0.0, 0.0, -d.z_scale, zmax - d.z_offset]);
    }
    v
}

fn dist(pl: &[f32; 4], p: &[f32; 4]) -> f32 { pl[0] * p[0] + pl[1] * p[1] + pl[2] * p[2] + pl[3] * p[3] }

fn outside(d: &DrawState, p: &[f32; 4]) -> bool {
    if p[3] <= 1e-6 { return true; }
    planes(d).iter().skip(1).any(|pl| dist(pl, p) < 0.0)
}

/// Sutherland-Hodgman clipping of a point, line or triangle against the clip planes.
pub fn clip_poly(d: &DrawState, p: &[[f32; 4]]) -> Vec<ClipVert> {
    let mut poly: Vec<ClipVert> = p.iter().enumerate().map(|(k, &pos)| { let mut b = [0.0; 3]; b[k] = 1.0; ClipVert { pos, bary: b } }).collect();
    let line = p.len() == 2;
    let mut pls = planes(d);
    pls[0] = [0.0, 0.0, 0.0, 1.0];
    for (pi, pl) in pls.iter().enumerate() {
        let pl = if pi == 0 { [0.0, 0.0, 0.0, 1.0] } else { *pl };
        let eps = if pi == 0 { 1e-6 } else { 0.0 };
        if poly.is_empty() { break; }
        let mut out = Vec::new();
        let n = poly.len();
        let segs = if line { 1 } else { n };
        if poly.len() == 1 {
            if dist(&pl, &poly[0].pos) - eps >= 0.0 { out.push(poly[0]); }
            poly = out;
            continue;
        }
        for k in 0..segs {
            let a = poly[k];
            let b = poly[(k + 1) % n];
            let (da, db) = (dist(&pl, &a.pos) - eps, dist(&pl, &b.pos) - eps);
            if line {
                match (da >= 0.0, db >= 0.0) {
                    (true, true) => { out.push(a); out.push(b); }
                    (false, false) => {}
                    (ia, _) => {
                        let t = da / (da - db);
                        let m = lerp(&a, &b, t);
                        if ia { out.push(a); out.push(m); } else { out.push(m); out.push(b); }
                    }
                }
                continue;
            }
            if da >= 0.0 { out.push(a); }
            if (da >= 0.0) != (db >= 0.0) { out.push(lerp(&a, &b, da / (da - db))); }
        }
        poly = out;
    }
    poly
}

fn lerp(a: &ClipVert, b: &ClipVert, t: f32) -> ClipVert {
    let mut c = *a;
    for k in 0..4 { c.pos[k] = a.pos[k] + (b.pos[k] - a.pos[k]) * t; }
    for k in 0..3 { c.bary[k] = a.bary[k] + (b.bary[k] - a.bary[k]) * t; }
    c
}
