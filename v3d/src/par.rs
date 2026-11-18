//! GPU work on several host threads: the render job's tiles (in `render`), and vertex and
//! coordinate shading split into batches of vertices. Each worker has its own view of guest
//! memory and its own instruction cache; shaders running at the same time get distinct thread
//! indices (TIDX), which Mesa uses to place register spills.

use std::sync::OnceLock;

use crate::qpu::LANES;
use crate::{bin, Code, Gmem};

/// Host threads for GPU work (tiles, vertex shading) (`SBC_V3D_THREADS`, default: the host's
/// cores minus two for the CPU cores, at most 16; 1 renders on the calling thread). The
/// browser build has no threads.
pub fn threads() -> usize {
    #[cfg(target_arch = "wasm32")]
    { 1 }
    #[cfg(not(target_arch = "wasm32"))]
    {
        static N: OnceLock<usize> = OnceLock::new();
        *N.get_or_init(|| match std::env::var("SBC_V3D_THREADS").ok().and_then(|v| v.parse::<usize>().ok()) {
            Some(n) => n.min(16), // thread indices stay below 32
            None => std::thread::available_parallelism().map_or(1, |n| n.get()).saturating_sub(2).clamp(1, 16),
        }).max(&1)
    }
}

/// Run `n` independent items on `workers` host threads, each with its own view of memory and
/// instruction cache; returns (item, result) in no particular order.
#[cfg(not(target_arch = "wasm32"))]
pub fn items<T: Send>(workers: usize, n: usize, m: &mut Gmem, code: &mut Code,
                      f: &(dyn Fn(usize, usize, &mut Gmem, &mut Code) -> T + Sync)) -> Vec<(usize, T)> {
    use std::sync::atomic::{AtomicUsize, Ordering};
    let next = AtomicUsize::new(0);
    // per worker: the items it ran, whether a shader ran away, MMU faults
    type Worker<T> = (Vec<(usize, T)>, bool, u32);
    let outs: Vec<Worker<T>> = std::thread::scope(|s| {
        let handles: Vec<_> = (0..workers).map(|k| {
            let (mut wm, mut wc, next) = (m.share(), code.clone(), &next);
            s.spawn(move || {
                let mut done = Vec::new();
                loop {
                    let i = next.fetch_add(1, Ordering::Relaxed);
                    if i >= n { break; }
                    done.push((i, f(k, i, &mut wm, &mut wc)));
                }
                (done, wc.runaway, wm.faults)
            })
        }).collect();
        handles.into_iter().map(|h| h.join().unwrap_or_else(|e| std::panic::resume_unwind(e))).collect()
    });
    let mut all = Vec::with_capacity(n);
    for (done, runaway, faults) in outs {
        all.extend(done);
        code.runaway |= runaway;
        m.faults += faults;
    }
    all
}


/// Vertices per work item when a draw's vertex or coordinate shading is split between workers.
pub const VS_CHUNK: usize = 8 * LANES;

/// `bin::run_vs` over `verts`, split between the workers when there are enough vertices. The
/// shader runs 16 vertices at a time, each batch on its own, so the outputs are the same.
pub fn run_vs(m: &mut Gmem, code: &mut Code, insns: &mut u64, d: &bin::DrawState, coord: bool, verts: &[(u32, u32)]) -> Vec<Vec<u32>> {
    let chunks = verts.len().div_ceil(VS_CHUNK);
    let workers = threads().min(chunks);
    if workers <= 1 { return bin::run_vs(m, code, insns, d, coord, verts, 0); }
    #[cfg(not(target_arch = "wasm32"))]
    {
        let mut outs = items(workers, chunks, m, code, &|k, i, wm, wc| {
            let mut ins = 0;
            let o = bin::run_vs(wm, wc, &mut ins, d, coord, &verts[i * VS_CHUNK..((i + 1) * VS_CHUNK).min(verts.len())], 2 * k as u32 + 1);
            (o, ins)
        });
        outs.sort_by_key(|o| o.0);
        let mut all = Vec::with_capacity(verts.len());
        for (_, (o, ins)) in outs { *insns += ins; all.extend(o); }
        all
    }
    #[cfg(target_arch = "wasm32")]
    unreachable!()
}
