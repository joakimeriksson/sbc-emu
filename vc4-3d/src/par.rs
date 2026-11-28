//! GPU work on several host threads: the render job's tiles (in `render`), and vertex and
//! coordinate shading split into chunks of vertices (`bin`, `render`). Each worker has its own
//! view of guest memory and its own instruction cache.
//!
//! The VideoCore IV has no per-thread scratch memory (Mesa's `vc4` compiler does not spill
//! registers to memory) and the GL pipeline's shaders use no semaphores, so unlike the V3D 4.2
//! model there are no thread indices to keep apart: each vertex batch has a VPM of its own, as
//! each fragment shader its tile buffer.

#[cfg(not(target_arch = "wasm32"))]
use std::sync::OnceLock;

#[cfg(not(target_arch = "wasm32"))]
use crate::mem::Mem;
use crate::qpu::Code;

/// Host threads for GPU work (`SBC_VC4_THREADS`, default: the host's cores minus two for the
/// CPU cores, at most 16; 1 renders on the calling thread). The browser build has no threads.
pub fn threads() -> usize {
    #[cfg(target_arch = "wasm32")]
    { 1 }
    #[cfg(not(target_arch = "wasm32"))]
    {
        static N: OnceLock<usize> = OnceLock::new();
        *N.get_or_init(|| match std::env::var("SBC_VC4_THREADS").ok().and_then(|v| v.parse::<usize>().ok()) {
            Some(n) => n.clamp(1, 16),
            None => std::thread::available_parallelism().map_or(1, |n| n.get()).saturating_sub(2).clamp(1, 16),
        })
    }
}

/// The workers of a job: how many, and their instruction caches (kept from job to job).
pub struct Par {
    pub threads: usize,
    pub codes: Vec<Code>,
}

impl Default for Par { fn default() -> Self { Par { threads: threads(), codes: Vec::new() } } }

impl Par {
    /// A new job: the decoded instructions may be stale.
    pub fn clear(&mut self) { for c in self.codes.iter_mut() { c.clear(); } }
}

/// Vertices per work item when vertex or coordinate shading is split between workers: eight
/// batches of 16, so the batches are the ones one thread would shade.
pub const VS_CHUNK: usize = 128;

/// Instruction caches for the workers (`codes[k]` for worker k), as many as needed, each
/// starting with the caller's runaway flag.
pub fn worker_codes(codes: &mut Vec<Code>, workers: usize, runaway: bool) -> &mut [Code] {
    while codes.len() < workers { codes.push(Code::default()); }
    for c in codes[..workers].iter_mut() { c.runaway = runaway; }
    &mut codes[..workers]
}

/// Run `n` independent items on `workers` host threads, each with its own view of memory and
/// instruction cache (from `codes`); returns (item, result) in no particular order. A shader
/// that ran away and memory faults are merged into `code` and `m`.
#[cfg(not(target_arch = "wasm32"))]
pub fn items<T: Send>(workers: usize, n: usize, m: &mut Mem, code: &mut Code, codes: &mut Vec<Code>,
                      f: &(dyn Fn(usize, &mut Mem, &mut Code) -> T + Sync)) -> Vec<(usize, T)> {
    use std::sync::atomic::{AtomicUsize, Ordering};
    let next = AtomicUsize::new(0);
    let wcodes = worker_codes(codes, workers, code.runaway);
    let outs: Vec<(Vec<(usize, T)>, Mem)> = std::thread::scope(|s| {
        let handles: Vec<_> = wcodes.iter_mut().map(|wc| {
            let (mut wm, next) = (m.share(), &next);
            s.spawn(move || {
                let mut done = Vec::new();
                loop {
                    let i = next.fetch_add(1, Ordering::Relaxed);
                    if i >= n { break; }
                    done.push((i, f(i, &mut wm, wc)));
                }
                (done, wm)
            })
        }).collect();
        handles.into_iter().map(|h| h.join().unwrap_or_else(|e| std::panic::resume_unwind(e))).collect()
    });
    let mut all = Vec::with_capacity(n);
    for (done, wm) in outs {
        all.extend(done);
        m.merge(wm);
    }
    for wc in codes[..workers].iter() { code.runaway |= wc.runaway; }
    all
}
