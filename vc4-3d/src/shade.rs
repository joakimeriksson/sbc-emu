//! Vertex shading: the vertex cache DMA (VCD) loads up to 16 vertices' attributes into the
//! VPM, one vertex per column (a 32-bit word per row), the coordinate or vertex shader runs on
//! a QPU reading them with VPM reads and writing its outputs with VPM writes, one output word
//! per row. Reads start at the batch's input rows and writes at its output rows.
//!
//! Shaded vertex formats (what the shaders write, in order):
//! * coordinate shader: Xc, Yc, Zc, Wc, Xs|Ys (signed 12.4 from the viewport centre, packed
//!   16:16), Zs, 1/Wc, [point size];
//! * vertex shader: Xs|Ys, Zs, 1/Wc, [point size], varyings.

use crate::cl::ShaderRec;
use crate::par;
use crate::mem::Mem;
use crate::qpu::{Code, Env, Qpu, Stop};
use std::collections::HashMap;

/// Instructions a vertex batch may run before it is taken for an endless loop.
pub const BUDGET: u64 = 1 << 22;

const IN_ROWS: usize = 64;
const OUT_ROWS: usize = 64;

struct VertexEnv {
    vpm: Vec<u32>,
}

impl Env for VertexEnv {
    fn vpm(&mut self) -> (&mut [u32], usize, usize) { (&mut self.vpm, 0, IN_ROWS) }
}

/// `SBC_VC4_VTX=1`: the inputs and outputs of the first vertices of each batch.
fn vtx_trace() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| std::env::var("SBC_VC4_VTX").is_ok_and(|v| v != "0"))
}

/// Outputs per vertex: the coordinate shader's or the vertex shader's.
pub fn outputs(rec: &ShaderRec, coord: bool) -> usize {
    let ps = rec.point_size() as usize;
    if coord { 7 + ps } else { 3 + ps + rec.fs_nvary as usize }
}

/// Shade up to 16 vertices (by index) with the record's coordinate (`coord`) or vertex shader;
/// `nout` words per vertex, vertex after vertex.
pub fn shade_batch(rec: &ShaderRec, coord: bool, idx: &[u32], mem: &mut Mem, code: &mut Code, insns: &mut u64) -> Vec<u32> {
    let nout = outputs(rec, coord);
    let mut out = vec![0u32; nout * idx.len()];
    if let Some((base, stride)) = rec.nv {
        // pre-shaded vertices: the vertex shader's format
        for (l, &v) in idx.iter().enumerate() {
            for k in 0..nout { out[l * nout + k] = mem.r32(base.wrapping_add(v.wrapping_mul(stride)).wrapping_add(4 * k as u32)); }
        }
        return out;
    }
    if code.runaway { return out; }
    let mut env = VertexEnv { vpm: vec![0; (IN_ROWS + OUT_ROWS) * 16] };
    let sel = if coord { rec.cs_sel } else { rec.vs_sel };
    for (a, at) in rec.attrs.iter().enumerate() {
        if sel >> a & 1 == 0 { continue; }
        let off = (if coord { at.cs_off } else { at.vs_off } / 4) as usize;
        let words = at.nbytes.div_ceil(4) as usize;
        for (l, &v) in idx.iter().enumerate() {
            let src = at.addr.wrapping_add(v.wrapping_mul(at.stride));
            let mut b = [0u8; 256];
            let n = at.nbytes as usize;
            mem.read(src, &mut b[..n]);
            for k in 0..words {
                let row = off + k;
                if row < IN_ROWS { env.vpm[row * 16 + l] = u32::from_le_bytes([b[4 * k], b[4 * k + 1], b[4 * k + 2], b[4 * k + 3]]); }
            }
        }
    }
    let (pc, unif) = if coord { (rec.cs_code, rec.cs_unif) } else { (rec.vs_code, rec.vs_unif) };
    let mut q = Qpu::new();
    q.start(pc, unif);
    let stop = q.run(code, mem, &mut env, BUDGET);
    *insns += q.insns;
    crate::progress(q.insns);
    if stop == Stop::Budget { code.runaway = true; }
    if stop != Stop::End {
        crate::log_first(&format!("vc4: {} shader at {pc:#x} did not end ({stop:?})", if coord { "coordinate" } else { "vertex" }));
    }
    for (l, _) in idx.iter().enumerate() {
        for k in 0..nout.min(OUT_ROWS) { out[l * nout + k] = env.vpm[(IN_ROWS + k) * 16 + l]; }
    }
    if vtx_trace() {
        for (l, &v) in idx.iter().enumerate().take(16) {
            let ins: Vec<String> = (0..12).map(|r| format!("{:.3}", f32::from_bits(env.vpm[r * 16 + l]))).collect();
            let outs: Vec<String> = out[l * nout..(l + 1) * nout].iter().map(|&w| format!("{:.3}/{w:#x}", f32::from_bits(w))).collect();
            eprintln!("\rvc4 {} vertex {v}: in [{}] out [{}]\r", if coord { "coord" } else { "vertex" }, ins.join(" "), outs.join(" "));
        }
    }
    out
}

/// A multiplicative hash for the vertex cache's small integer keys (the standard SipHash costs
/// more than the lookup).
#[derive(Default)]
pub struct FastHash(u64);

impl std::hash::Hasher for FastHash {
    fn finish(&self) -> u64 { self.0 }
    fn write(&mut self, b: &[u8]) { for &x in b { self.write_u64(x as u64); } }
    fn write_u8(&mut self, x: u8) { self.write_u64(x as u64); }
    fn write_u32(&mut self, x: u32) { self.write_u64(x as u64); }
    fn write_u64(&mut self, x: u64) { self.0 = (self.0.rotate_left(5) ^ x).wrapping_mul(0x517c_c1b7_2722_0a95); }
}

/// Shaded vertices of a job, by (shader record, vertex index): a vertex is shaded once per job
/// however many primitives and tiles use it (the hardware shades it again per tile; the
/// results are the same).
#[derive(Default)]
pub struct Cache {
    slots: HashMap<(u32, u32, bool), usize, std::hash::BuildHasherDefault<FastHash>>,
    data: Vec<u32>,
}

impl Cache {
    pub fn clear(&mut self) { self.slots.clear(); self.data.clear(); }

    /// Make sure the vertices `idx` are shaded; shades the missing ones (and, to fill batches,
    /// the indices following each, `speculate`) 16 at a time.
    pub fn ensure(&mut self, rec: &ShaderRec, coord: bool, idx: &[u32], speculate: bool, mem: &mut Mem, code: &mut Code, insns: &mut u64) {
        let nout = outputs(rec, coord);
        let mut missing: Vec<u32> = Vec::new();
        let mut seen: std::collections::HashSet<u32, std::hash::BuildHasherDefault<FastHash>> = Default::default();
        for &v in idx {
            if self.slots.contains_key(&(rec.addr, v, coord)) || !seen.insert(v) { continue; }
            missing.push(v);
            if speculate {
                let mut k = 1;
                while missing.len() % 16 != 0 && k < 16 {
                    let w = v.wrapping_add(k);
                    if !self.slots.contains_key(&(rec.addr, w, coord)) && seen.insert(w) { missing.push(w); }
                    k += 1;
                }
            }
        }
        for batch in missing.chunks(16) {
            let out = shade_batch(rec, coord, batch, mem, code, insns);
            for (l, &v) in batch.iter().enumerate() {
                self.slots.insert((rec.addr, v, coord), self.data.len());
                self.data.extend_from_slice(&out[l * nout..(l + 1) * nout]);
            }
        }
    }

    pub fn get(&self, rec: u32, v: u32, coord: bool) -> Option<usize> { self.slots.get(&(rec, v, coord)).copied() }
    pub fn has(&self, rec: u32, v: u32, coord: bool) -> bool { self.slots.contains_key(&(rec, v, coord)) }

    /// Add a shaded vertex.
    pub fn insert(&mut self, rec: u32, v: u32, coord: bool, out: &[u32]) {
        self.slots.insert((rec, v, coord), self.data.len());
        self.data.extend_from_slice(out);
    }
    pub fn data(&self) -> &[u32] { &self.data }
}

/// Shade the vertices of `want` (shader records with their vertex indices) into `cache` on the
/// workers, in chunks of [`par::VS_CHUNK`] vertices of each record's sorted distinct indices:
/// the 16-vertex batches are those [`Cache::ensure`] shades for the same indices on one thread.
/// Vertices already in the cache and pre-shaded records are left alone.
pub fn shade_parallel(want: Vec<(ShaderRec, Vec<u32>)>, coord: bool, cache: &mut Cache, mem: &mut Mem, code: &mut Code, par: &mut par::Par, insns: &mut u64) {
    let want: Vec<(ShaderRec, Vec<u32>)> = want.into_iter().filter(|(r, _)| r.nv.is_none()).map(|(r, mut idx)| {
        idx.sort_unstable();
        idx.dedup();
        idx.retain(|&v| !cache.has(r.addr, v, coord));
        (r, idx)
    }).filter(|(_, idx)| !idx.is_empty()).collect();
    let items: Vec<(usize, usize)> = want.iter().enumerate()
        .flat_map(|(k, (_, idx))| (0..idx.len().div_ceil(par::VS_CHUNK)).map(move |c| (k, c))).collect();
    let workers = par.threads.min(items.len());
    if workers <= 1 || code.runaway {
        for (r, idx) in &want { cache.ensure(r, coord, idx, false, mem, code, insns); }
        return;
    }
    #[cfg(not(target_arch = "wasm32"))]
    {
        let mut outs = par::items(workers, items.len(), mem, code, &mut par.codes, &|i, wm, wc| {
            let (k, c) = items[i];
            let (r, idx) = &want[k];
            let chunk = &idx[c * par::VS_CHUNK..((c + 1) * par::VS_CHUNK).min(idx.len())];
            let mut ins = 0;
            let o: Vec<Vec<u32>> = chunk.chunks(16).map(|b| shade_batch(r, coord, b, wm, wc, &mut ins)).collect();
            (o, ins)
        });
        outs.sort_by_key(|o| o.0);
        for (i, (o, ins)) in outs {
            *insns += ins;
            let (k, c) = items[i];
            let (r, idx) = &want[k];
            let nout = outputs(r, coord);
            for (b, batch) in idx[c * par::VS_CHUNK..].chunks(16).zip(&o) {
                for (l, &v) in b.iter().enumerate() { cache.insert(r.addr, v, coord, &batch[l * nout..(l + 1) * nout]); }
            }
        }
    }
}
