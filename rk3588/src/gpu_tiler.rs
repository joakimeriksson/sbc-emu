//! Synchronous Valhall vertex/varying/tiler execution (RUN_IDVS). Polygon lists are a
//! private GPU format, so they live on the host, keyed by VM and tiler context, as in
//! the Midgard model. Vertex positions and varyings are private too: on the GPU they
//! stream through the position FIFO into the tiler heap.
//!
//! Each draw shades every distinct (vertex, instance) once with the position shader and,
//! when the fragment stage needs them, the varying shader. Mesa's shaders apply the
//! viewport transform themselves, so positions hold window x, y, z and 1/w. Primitives
//! are assembled from the index order (triangles, strips, fans, lines, points, with
//! primitive restart); clipping, culling and rasterization happen in the fragment job.
use crate::{
    gpu_cs::Stop,
    gpu_mmu::{Access, AddressSpace},
    gpu_resources::Resources,
    gpu_shader::{Code, Env, RegisterAllocation, Shader, Tlb},
};
use std::{collections::BTreeMap, sync::Arc};
type Key = (u64, u64, u64);
type Owner = (u32, u32);
#[derive(Default)]
struct Heap {
    owner: Owner,
    started: u32,
    completed: u32,
    fragments: u32,
}

/// Reduced primitive types.
pub const POINTS: u32 = 1;
pub const LINES: u32 = 2;
pub const TRIANGLES: u32 = 3;

pub struct Draw {
    pub regs: [u32; 256],
    pub instruction: u64,
    /// POINTS, LINES or TRIANGLES.
    pub kind: u32,
    /// Per shaded vertex: window x, y, z and 1/w.
    pub positions: Vec<[f32; 4]>,
    /// Per shaded vertex point size (points with an extended position FIFO).
    pub point_sizes: Vec<f32>,
    /// Vertex indices (unused slots repeat the last) and the provoking vertex index.
    pub primitives: Vec<[u32; 4]>,
    /// Varying record stride and the records of all shaded vertices.
    pub varyings: Option<(usize, Arc<[u8]>)>,
}
impl Draw {
    /// The three positions of triangle `n`.
    pub fn triangle(&self, n: usize) -> [[f32; 4]; 3] {
        let p = self.primitives[n];
        [
            self.positions[p[0] as usize],
            self.positions[p[1] as usize],
            self.positions[p[2] as usize],
        ]
    }
}
#[derive(Default)]
pub struct Frame {
    pub owner: Owner,
    pub width: u32,
    pub height: u32,
    pub first_provoking: bool,
    pub draws: Vec<Draw>,
    vertices: usize,
}
#[derive(Default)]
pub struct Tiler {
    frames: BTreeMap<Key, Frame>,
    vertices: usize,
    heaps: BTreeMap<Key, Heap>,
}
fn word(b: &[u8], n: usize) -> u32 {
    u32::from_le_bytes(b[n * 4..n * 4 + 4].try_into().unwrap())
}
fn pair(b: &[u8], n: usize) -> u64 {
    word(b, n) as u64 | ((word(b, n + 1) as u64) << 32)
}
fn reg(r: &[u32; 256], n: usize) -> u64 {
    r[n] as u64 | ((r[n + 1] as u64) << 32)
}
fn key(m: &AddressSpace, va: u64) -> Key {
    (m.transtab, m.transcfg, va)
}
fn unsupported(opcode: u8) -> Stop {
    Stop::Unsupported { pc: 0, opcode }
}

/// Host work bounds: vertices per draw and per unrendered frame, shader instructions per
/// draw and per thread.
const DRAW_VERTICES: usize = 1 << 20;
const FRAME_VERTICES: usize = 1 << 22;
const DRAW_BUDGET: usize = 1 << 28;
const THREAD_BUDGET: usize = 1 << 16;

impl Tiler {
    pub fn clear_group(&mut self, group: u32) {
        let vertices = &mut self.vertices;
        self.frames.retain(|_, f| {
            if f.owner.0 == group {
                *vertices -= f.vertices;
                false
            } else {
                true
            }
        });
        self.heaps.retain(|_, h| h.owner.0 != group);
    }
    pub fn clear_queue(&mut self, owner: Owner) {
        let vertices = &mut self.vertices;
        self.frames.retain(|_, f| {
            if f.owner == owner {
                *vertices -= f.vertices;
                false
            } else {
                true
            }
        });
        self.heaps.retain(|_, h| h.owner != owner);
    }
    pub fn heap_operation(
        &mut self,
        m: &AddressSpace,
        ram: &[u8],
        owner: Owner,
        heap: u64,
        operation: u32,
    ) -> Result<(), Stop> {
        if !matches!(operation, 0 | 1 | 3) {
            return Err(unsupported(49));
        }
        m.chunks(ram, heap, 32, Access::Write, true)
            .map_err(Stop::Memory)?;
        let k = key(m, heap);
        if self.heaps.len() >= 64 && !self.heaps.contains_key(&k) {
            return Err(unsupported(49));
        }
        let h = self.heaps.entry(k).or_insert(Heap {
            owner,
            ..Heap::default()
        });
        if h.owner != owner {
            return Err(unsupported(49));
        }
        match operation {
            0 => h.started = h.started.wrapping_add(1),
            1 if h.started.wrapping_sub(h.completed) != 0 => {
                h.completed = h.completed.wrapping_add(1)
            }
            3 if h.completed.wrapping_sub(h.fragments) != 0 => {
                h.fragments = h.fragments.wrapping_add(1)
            }
            _ => return Err(unsupported(49)),
        }
        Ok(())
    }
    pub fn fragment(
        &mut self,
        m: &AddressSpace,
        ram: &mut [u8],
        r: &[u32; 256],
        owner: Owner,
    ) -> Result<(), Stop> {
        let fbd = reg(r, 40) & !63;
        let mut data = [0; 8];
        m.read(
            ram,
            fbd.checked_add(56).ok_or(Stop::InvalidStream)?,
            &mut data,
            true,
        )
        .map_err(Stop::Memory)?;
        let ctx = u64::from_le_bytes(data);
        if ctx == 0 {
            return crate::gpu_render::fragment(m, ram, r);
        }
        let k = key(m, ctx);
        let frame = self.frames.get(&k);
        if frame.is_some_and(|f| f.owner != owner) {
            return Err(unsupported(7));
        }
        // A frame without draws (only clears) still has a tiler context.
        let empty = Frame::default();
        let started = std::time::Instant::now();
        crate::gpu_render::fragment_frame(m, ram, r, Some(frame.unwrap_or(&empty)))?;
        if std::env::var_os("SBC_MALI_TIMING").is_some() {
            let f = frame.unwrap_or(&empty);
            let prims: usize = f.draws.iter().map(|d| d.primitives.len()).sum();
            eprintln!(
                "[mali] fragment job: {}x{}, {} draws, {} primitives, {:.1} ms",
                f.width,
                f.height,
                f.draws.len(),
                prims,
                started.elapsed().as_secs_f64() * 1e3
            );
        }
        if let Some(f) = self.frames.remove(&k) {
            self.vertices -= f.vertices;
        }
        Ok(())
    }
    pub fn idvs(
        &mut self,
        m: &AddressSpace,
        ram: &mut [u8],
        r: &[u32; 256],
        instruction: u64,
        owner: Owner,
    ) -> Result<(), Stop> {
        let flags = r[56] | instruction as u32;
        if std::env::var_os("SBC_MALI_TRACE").is_some() {
            eprintln!(
                "[mali] IDVS flags={flags:08x} insn={instruction:016x} registers={:x?}",
                &r[0..62]
            );
        }
        let mode = flags & 255;
        let kind = match mode {
            1 => POINTS,
            2 | 4 | 6 => LINES,
            8 | 10 | 12 | 13 => TRIANGLES,
            _ => return Err(unsupported(6)),
        };
        // No multiview, layers, scissor arrays or primitive index output.
        if flags & ((0xff << 24) | (1 << 20) | (1 << 21) | (3 << 13)) != 0
            || r[48] > 1024
            || r[48] % 4 != 0
        {
            return Err(unsupported(6));
        }
        let secondary = flags & (1 << 18) != 0;
        if secondary && r[48] == 0 {
            return Err(unsupported(6));
        }
        let extended = (flags >> 22) & 3 == 2;
        let ids = draw_vertices(m, ram, r, flags)?;
        // (no instances is a legal draw that draws nothing: glDrawElementsIndirect with
        // instanceCount 0)
        let instances = r[34];
        if ids.len() as u64 * instances as u64 > DRAW_VERTICES as u64 {
            return Err(unsupported(6));
        }
        let ctx = reg(r, 40);
        if ctx & 63 != 0 {
            return Err(Stop::InvalidStream);
        }
        let k = key(m, ctx);
        if self.frames.len() >= 64 && !self.frames.contains_key(&k) {
            return Err(unsupported(6));
        }
        if let Some(frame) = self.frames.get(&k) {
            if frame.owner != owner || frame.draws.len() >= 1 << 16 {
                return Err(unsupported(6));
            }
        }
        let mut tiler = [0; 128];
        m.read(ram, ctx, &mut tiler, true).map_err(Stop::Memory)?;
        let width = (word(&tiler, 3) & 65535) + 1;
        let height = (word(&tiler, 3) >> 16) + 1;
        if width > 8192 || height > 8192 || word(&tiler, 4) & 0xff != 0 {
            return Err(unsupported(6));
        }
        let first_provoking = word(&tiler, 2) & (1 << 18) != 0;
        if let Some(frame) = self.frames.get(&k) {
            if frame.width != width || frame.height != height {
                return Err(unsupported(6));
            }
        }
        let mut heap = [0; 32];
        m.read(ram, pair(&tiler, 6), &mut heap, true)
            .map_err(Stop::Memory)?;
        let base = pair(&heap, 2);
        let bottom = pair(&heap, 4);
        let top = pair(&heap, 6);
        if word(&heap, 0) & 255 != 0x29
            || base & 4095 != 0
            || bottom < base.checked_add(64).ok_or(Stop::InvalidStream)?
            || bottom > top
            || top.checked_sub(base) != Some(word(&heap, 1) as u64)
        {
            return Err(unsupported(6));
        }
        m.chunks(ram, bottom, 64, Access::Write, true)
            .map_err(Stop::Memory)?;

        // Distinct vertex IDs in first-use order; restart markers stay out of the slots.
        let restart = flags & (1 << 19) != 0;
        let restart_index = match (flags >> 8) & 7 {
            1 => 0xff,
            2 => 0xffff,
            _ => u32::MAX,
        };
        let mut slot_of = std::collections::HashMap::new();
        let mut unique = Vec::new();
        let mut order = Vec::with_capacity(ids.len());
        for &(raw, id) in &ids {
            if restart && raw == restart_index {
                order.push(u32::MAX);
                continue;
            }
            let s = *slot_of.entry(id).or_insert_with(|| {
                unique.push(id);
                unique.len() as u32 - 1
            });
            order.push(s);
        }
        let shaded = unique.len() * instances as usize;
        if self.vertices + shaded > FRAME_VERTICES {
            return Err(unsupported(6));
        }
        let record = if extended { 32 } else { 16 };
        let mut positions_raw = vec![0u8; shaded * record];
        let mut code = Code::default();
        let mut tlb = Tlb::default();
        let mut textures = crate::gpu_texture::Cache::default();
        let mut budget = DRAW_BUDGET;
        let index_scale = if extended { 8 } else { 4 };
        let stage = |spd: u64, fau: u64, resources: u64| Stage {
            spd,
            fau,
            resources,
        };
        run_stage(
            m,
            ram,
            &mut code,
            &mut tlb,
            &mut textures,
            stage(reg(r, 16), reg(r, 8), reg(r, 0)),
            &unique,
            instances,
            index_scale,
            (&mut positions_raw, (4, 1)),
            &mut budget,
        )?;
        let varyings = if secondary {
            let stride = r[48] as usize;
            let mut data = vec![0u8; shaded * stride];
            run_stage(
                m,
                ram,
                &mut code,
                &mut tlb,
                &mut textures,
                stage(
                    reg(r, 18),
                    reg(r, if instruction & (1 << 36) != 0 { 10 } else { 8 }),
                    reg(r, if instruction & (1 << 35) != 0 { 2 } else { 0 }),
                ),
                &unique,
                instances,
                index_scale,
                (&mut data, (stride as u32, index_scale)),
                &mut budget,
            )?;
            Some((stride, Arc::from(data)))
        } else {
            None
        };
        let mut positions = Vec::with_capacity(shaded);
        let mut point_sizes = Vec::new();
        for v in positions_raw.chunks_exact(record) {
            positions.push([0, 1, 2, 3].map(|n| f32::from_bits(word(v, n))));
            if extended && kind == POINTS {
                point_sizes.push(crate::gpu_shader::f16_to_f32(u16::from_le_bytes([
                    v[16], v[17],
                ])));
            }
        }
        if std::env::var_os("SBC_MALI_TRACE").is_some() {
            eprintln!(
                "[mali] IDVS mode={mode} positions={:?}",
                &positions[..positions.len().min(16)]
            );
        }
        let mut primitives = Vec::new();
        for instance in 0..instances as usize {
            let base = (instance * unique.len()) as u32;
            for run in order.split(|&s| s == u32::MAX) {
                assemble(mode, run, base, first_provoking, &mut primitives);
            }
        }
        let frame = self.frames.entry(k).or_insert_with(|| Frame {
            owner,
            width,
            height,
            first_provoking,
            draws: Vec::new(),
            vertices: 0,
        });
        frame.draws.push(Draw {
            regs: *r,
            instruction,
            kind,
            positions,
            point_sizes,
            primitives,
            varyings,
        });
        frame.vertices += shaded;
        self.vertices += shaded;
        Ok(())
    }
}

/// Assemble primitives of one restart-free run of vertex slots. The fourth element is
/// the provoking vertex.
fn assemble(mode: u32, run: &[u32], base: u32, first: bool, out: &mut Vec<[u32; 4]>) {
    let n = run.len();
    let v = |k: usize| base + run[k];
    let tri = |a: u32, b: u32, c: u32, p: u32| [a, b, c, p];
    match mode {
        1 => out.extend((0..n).map(|k| [v(k), v(k), v(k), v(k)])),
        2 => out.extend((0..n / 2).map(|k| {
            let (a, b) = (v(2 * k), v(2 * k + 1));
            [a, b, b, if first { a } else { b }]
        })),
        4 | 6 => {
            if n >= 2 {
                for k in 0..n - 1 {
                    let (a, b) = (v(k), v(k + 1));
                    out.push([a, b, b, if first { a } else { b }]);
                }
                if mode == 6 && n > 2 {
                    let (a, b) = (v(n - 1), v(0));
                    out.push([a, b, b, if first { a } else { b }]);
                }
            }
        }
        8 => out.extend((0..n / 3).map(|k| {
            let (a, b, c) = (v(3 * k), v(3 * k + 1), v(3 * k + 2));
            tri(a, b, c, if first { a } else { c })
        })),
        10 => {
            for k in 0..n.saturating_sub(2) {
                let (a, b, c) = if k % 2 == 0 {
                    (v(k), v(k + 1), v(k + 2))
                } else {
                    (v(k + 1), v(k), v(k + 2))
                };
                out.push(tri(a, b, c, if first { v(k) } else { v(k + 2) }));
            }
        }
        _ => {
            // Triangle fan and polygon.
            for k in 1..n.saturating_sub(1) {
                let (a, b, c) = (v(0), v(k), v(k + 1));
                out.push(tri(a, b, c, if first { b } else { c }));
            }
        }
    }
}

struct Stage {
    spd: u64,
    fau: u64,
    resources: u64,
}
/// Read a shader program descriptor: the binary, register allocation and preload mask.
pub(crate) fn shader_program(
    m: &AddressSpace,
    ram: &[u8],
    spd: u64,
    stage: u32,
) -> Result<(u64, RegisterAllocation, u32), Stop> {
    let mut d = [0; 32];
    m.read(ram, spd, &mut d, true).map_err(Stop::Memory)?;
    // Type 8 (shader); suppress NaN/Inf, flush-to-zero and helper/JUMP_EX flags are
    // accepted; other bits must be zero.
    let allowed = 0xf | (0xf << 4) | (3 << 8) | (0xf << 16) | (3 << 28) | (3 << 30);
    if word(&d, 0) & 15 != 8
        || (word(&d, 0) >> 4) & 15 != stage
        || word(&d, 0) & !allowed != 0
        || d[16..].iter().any(|&v| v != 0)
    {
        return Err(unsupported(if stage == 2 { 7 } else { 6 }));
    }
    let allocation = match word(&d, 0) >> 30 {
        0 => RegisterAllocation::Registers64,
        2 => RegisterAllocation::Registers32,
        _ => return Err(unsupported(6)),
    };
    let binary = pair(&d, 2);
    if binary & 7 != 0 {
        return Err(Stop::InvalidStream);
    }
    Ok((binary, allocation, word(&d, 1)))
}
pub(crate) fn uniforms(m: &AddressSpace, ram: &[u8], fau: u64) -> Result<Vec<u32>, Stop> {
    let count = (fau >> 56) as usize;
    if fau & 0x00ff_0000_0000_0000 != 0 {
        return Err(unsupported(6));
    }
    let mut data = vec![0; count * 8];
    m.read(ram, fau & 0xffff_ffff_ffff, &mut data, true)
        .map_err(Stop::Memory)?;
    Ok(data
        .chunks_exact(4)
        .map(|v| u32::from_le_bytes(v.try_into().unwrap()))
        .collect())
}

#[allow(clippy::too_many_arguments)]
fn run_stage(
    m: &AddressSpace,
    ram: &mut [u8],
    code: &mut Code,
    tlb: &mut Tlb,
    textures: &mut crate::gpu_texture::Cache,
    stage: Stage,
    vertices: &[u32],
    instances: u32,
    index_scale: u32,
    output: (&mut [u8], (u32, u32)),
    budget: &mut usize,
) -> Result<(), Stop> {
    let (binary, allocation, preload) = shader_program(m, ram, stage.spd, 3)?;
    let uniforms = uniforms(m, ram, stage.fau)?;
    let (out, unit) = output;
    let mut env = Env::new(m, ram, &uniforms, code, tlb, textures);
    env.output = Some(out);
    env.output_unit = unit;
    for instance in 0..instances {
        for (n, &vertex) in vertices.iter().enumerate() {
            let slot = instance * vertices.len() as u32 + n as u32;
            let mut shader = Shader::with_allocation(binary, allocation);
            shader.resources = Resources(stage.resources);
            let values = [slot * index_scale, vertex, instance, 0, 0];
            for (k, &value) in values.iter().enumerate() {
                if preload & (1 << (11 + k)) != 0 {
                    shader.regs[59 + k] = value;
                }
            }
            shader
                .run_env(&mut env, (*budget).min(THREAD_BUDGET))
                .map_err(Stop::Shader)?;
            *budget = budget.saturating_sub(shader.instructions as usize);
            crate::gpu::work(shader.instructions, 0, 0);
            if *budget == 0 {
                return Err(unsupported(6));
            }
        }
    }
    Ok(())
}

/// Decode element IDs before executing shaders: (raw index, vertex ID). Hardware uses
/// unsigned vertex IDs with a signed base-vertex addition; attribute bounds checks
/// handle IDs outside a buffer.
fn draw_vertices(
    m: &AddressSpace,
    ram: &[u8],
    r: &[u32; 256],
    flags: u32,
) -> Result<Vec<(u32, u32)>, Stop> {
    if r[33] as usize > DRAW_VERTICES {
        return Err(unsupported(6));
    }
    let index_type = (flags >> 8) & 7;
    let mut ids = Vec::with_capacity(r[33] as usize);
    if index_type == 0 {
        ids.extend((0..r[33]).map(|i| (i, i.wrapping_add(r[36]))));
    } else {
        let width = match index_type {
            1 => 1,
            2 => 2,
            3 => 4,
            _ => return Err(unsupported(6)),
        };
        // r54 the index buffer, r39 its size in bytes, r35 the first index (Mesa's
        // indirect draws load firstIndex there; direct draws pass 0 and offset r54)
        let bytes = r[33] as usize * width;
        let skip = r[35] as u64 * width as u64;
        let base = reg(r, 54);
        let address = base.wrapping_add(skip);
        if base == 0 || base % width as u64 != 0 || skip + bytes as u64 > r[39] as u64 {
            return Err(Stop::InvalidStream);
        }
        let mut indices = vec![0; bytes];
        m.read(ram, address, &mut indices, true)
            .map_err(Stop::Memory)?;
        for value in indices.chunks_exact(width) {
            let mut word = [0; 4];
            word[..width].copy_from_slice(value);
            let raw = u32::from_le_bytes(word);
            ids.push((raw, raw.wrapping_add(r[36])));
        }
    }
    Ok(ids)
}

impl Default for Draw {
    fn default() -> Self {
        Self {
            regs: [0; 256],
            instruction: 0,
            kind: TRIANGLES,
            positions: Vec::new(),
            point_sizes: Vec::new(),
            primitives: Vec::new(),
            varyings: None,
        }
    }
}
emu_core::snap_struct!(Heap {
    owner,
    started,
    completed,
    fragments
});
emu_core::snap_struct!(Draw {
    regs,
    instruction,
    kind,
    positions,
    point_sizes,
    primitives,
    varyings
});
emu_core::snap_struct!(Frame {
    owner,
    width,
    height,
    first_provoking,
    draws,
    vertices
});
emu_core::snap_struct!(Tiler {
    frames,
    vertices,
    heaps
});

#[cfg(test)]
mod tests;
