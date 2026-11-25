//! The compute class (B1C0): a launch reads its descriptor (QMD) from memory and runs the shader
//! for every workgroup.
use crate::common_host;
use crate::inline::Inline;
use crate::pipe::{self, Common, Pipe, Work};
use crate::threed::Cbuf;
use crate::Memory;
use maxwell_shader::{Exit, Host, Lanes, Program, SurfOp, TexOp};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;

pub struct Compute {
    regs: Vec<u32>,
    inline: Inline,
    pub launches: u64,
}

impl Default for Compute {
    fn default() -> Self {
        Self { regs: vec![0; 0x1000], inline: Inline::default(), launches: 0 }
    }
}

/// What a launch descriptor says.
#[derive(Debug, PartialEq)]
struct Launch {
    entry: u32,
    /// workgroups and threads in each
    groups: [u32; 3],
    threads: [u32; 3],
    shared: usize,
    local: usize,
    cbufs: [Cbuf; 18],
    /// semaphores released at the end: (address, payload, one word)
    releases: Vec<(u64, u32, bool)>,
}

impl Launch {
    fn decode(w: &[u32; 64]) -> Self {
        let mut cbufs = [Cbuf::default(); 18];
        for (k, c) in cbufs.iter_mut().enumerate().take(8).filter(|(k, _)| w[20] >> k & 1 != 0) {
            let (lo, hi) = (w[29 + 2 * k], w[30 + 2 * k]);
            *c = Cbuf { address: ((hi & 0xff) as u64) << 32 | lo as u64, size: hi >> 15 };
        }
        let releases = (0..2)
            .filter(|k| w[6] >> (10 + k) & 1 != 0)
            .map(|k| {
                let r = 23 + 3 * k;
                (((w[r + 1] & 0xff) as u64) << 32 | w[r] as u64, w[r + 2], w[r + 1] >> 31 != 0)
            })
            .collect();
        Self {
            entry: w[8],
            groups: [w[12], w[13] & 0xffff, w[13] >> 16],
            threads: [w[18] >> 16, w[19] & 0xffff, w[19] >> 16],
            shared: (w[17] & 0x3_ffff) as usize,
            local: ((w[45] & 0xff_ffff) + (w[46] & 0xff_ffff) + (w[47] & 0xff_ffff)) as usize,
            cbufs,
            releases,
        }
    }
}

/// Lanes that run in lockstep: a warp, as CUDA's kernels assume (shuffles, votes, lane ids and
/// warp-synchronous code see all 32 threads at once).
const W: usize = 32;

/// The largest workgroup (threads), the most local memory of one, and the most instructions (of
/// sixteen threads each) for sixteen threads and in a launch: programs that do not end are stopped.
const MAX_THREADS: u64 = 4096;
const MAX_LOCAL: usize = 1 << 28;
const FUEL: u64 = 400_000_000;
const LAUNCH_FUEL: u64 = 20_000_000_000;

struct Group<'a, 'm, const N: usize> {
    common: &'a mut Common<'m>,
    shared: &'a mut [u8],
    group: [u32; 3],
    /// each lane's thread
    local: [[u32; N]; 3],
}

impl<const N: usize> Host<N> for Group<'_, '_, N> {
    common_host!(N);

    fn sysval(&mut self, sys: u8) -> [u32; N] {
        match sys {
            0x20 => std::array::from_fn(|l| self.local[0][l] | self.local[1][l] << 16 | self.local[2][l] << 26),
            0x21..=0x23 => self.local[sys as usize - 0x21],
            0x25..=0x27 => [self.group[sys as usize - 0x25]; N],
            _ => [0; N],
        }
    }

    fn shared(&mut self) -> &mut [u8] {
        self.shared
    }
}

impl Compute {
    fn r(&self, m: u32) -> u32 {
        self.regs[m as usize / 4]
    }

    fn address(&self, m: u32) -> u64 {
        ((self.r(m) & 0xff) as u64) << 32 | self.r(m + 4) as u64
    }

    /// A method; true if memory may have been written (what is cached is stale).
    pub fn method(&mut self, m: u32, v: u32, pipe: &mut Pipe, mem: &mut dyn Memory, now: u64) -> bool {
        if let Some(r) = self.regs.get_mut(m as usize / 4) {
            *r = v;
        }
        if Inline::owns(m) {
            self.inline.method(m, v, mem);
            return m == 0x1b4;
        }
        match m {
            0x21c | 0x1280 => pipe.invalidate_shaders(),
            0x1698 if v & 1 != 0 => pipe.invalidate_shaders(),
            0x2bc => {
                self.launch(pipe, mem, now);
                return true;
            }
            // a semaphore
            0x1b0c => {
                let at = self.address(0x1b00);
                if std::env::var_os("SBC_TRACE_SEM").is_some() { eprintln!("[sem] compute report {at:#x} = {:#x} (op {v:#x})", self.r(0x1b08)); }
                if v & 3 == 0 && v >> 28 & 1 != 0 {
                    mem.write(at, &self.r(0x1b08).to_le_bytes());
                } else if v & 1 == 0 {
                    let mut b = [0u8; 16];
                    b[..4].copy_from_slice(&self.r(0x1b08).to_le_bytes());
                    b[8..].copy_from_slice(&now.to_le_bytes());
                    mem.write(at, &b);
                }
                return true;
            }
            _ => {}
        }
        false
    }

    fn launch(&mut self, pipe: &mut Pipe, mem: &mut dyn Memory, now: u64) {
        let mut raw = [0u8; 256];
        mem.read((self.r(0x2b4) as u64) << 8, &mut raw);
        let words: [u32; 64] = std::array::from_fn(|k| u32::from_le_bytes(raw[4 * k..4 * k + 4].try_into().unwrap()));
        let launch = Launch::decode(&words);
        self.launches += 1;
        let base = self.address(0x1608);
        if let Some(program) = pipe.program(base, launch.entry, mem) {
            self.run(&launch, &program, pipe, mem);
        }
        for (at, payload, short) in launch.releases {
            if std::env::var_os("SBC_TRACE_SEM").is_some() { eprintln!("[sem] qmd release {at:#x} = {payload:#x}"); }
            mem.write(at, &payload.to_le_bytes());
            if !short {
                mem.write(at + 8, &now.to_le_bytes());
            }
        }
    }

    fn run(&mut self, launch: &Launch, program: &Program, pipe: &mut Pipe, mem: &mut dyn Memory) {
        // 32-lane warps, as CUDA's kernels assume; in the browser a kernel that cannot see the warp
        // width (no shuffles, votes, lane ids or lane masks) runs in the shader JIT's 16 lanes
        use maxwell_shader::isa::Op;
        let narrow = cfg!(target_arch = "wasm32")
            && !program.any(|op| matches!(op, Op::Shfl { .. } | Op::Vote { .. } | Op::S2R { sys: 0x00 | 0x38..=0x3c, .. }));
        if narrow {
            run_lanes::<16>(launch, program, pipe, mem, self.address(0x1574), self.address(0x155c), self.r(0x2608));
        } else {
            run_lanes::<W>(launch, program, pipe, mem, self.address(0x1574), self.address(0x155c), self.r(0x2608));
        }
    }
}

/// Run a launch's workgroups in batches of `N` lanes (a warp: 32), on the GPU worker pool unless
/// the shader uses global atomics.
fn run_lanes<const N: usize>(launch: &Launch, program: &Program, pipe: &mut Pipe, mem: &mut dyn Memory, local_base: u64, shared_base: u64, tex_cb: u32) {
    let [tx, ty, tz] = launch.threads.map(|t| t.max(1));
    let threads = tx as u64 * ty as u64 * tz as u64;
    if threads > MAX_THREADS {
        pipe.fail(format!("compute workgroup of {threads} threads"));
        return;
    }
    let batches = (threads as usize).div_ceil(N);
    if launch.local > pipe::MAX_LOCAL || launch.local * batches * N > MAX_LOCAL {
        pipe.fail(format!("compute shader with {} bytes of local memory", launch.local));
        return;
    }
    // the descriptor invalidates the caches
    pipe.invalidate_textures();
    pipe.cbufs_changed = true;
    let work = Work::compute(pipe, launch.cbufs, local_base, shared_base, tex_cb);
    // which thread each lane is
    let thread = |batch: usize, lane: usize| {
        let t = (batch * N + lane) as u32;
        [t % tx, t / tx % ty, t / tx / ty]
    };
    let locals: Vec<[[u32; N]; 3]> = (0..batches).map(|b| std::array::from_fn(|k| std::array::from_fn(|l| thread(b, l)[k]))).collect();
    let [gx, gy, gz] = launch.groups;
    let groups = gx as u64 * gy as u64 * gz as u64;
    let atomics = program.any(|op| matches!(op, maxwell_shader::isa::Op::Atom { .. } | maxwell_shader::isa::Op::Suatom { .. }));
    // (each thread has the local memory of a workgroup's threads)
    let per_thread = (launch.local * batches * N).max(1);
    let workers = if atomics { 1 } else { pipe::threads().min(groups as usize).min(MAX_LOCAL / per_thread).max(1) };
    pipe.need_workers(workers);
    let next = AtomicU64::new(0);
    let left_fuel = AtomicU64::new(LAUNCH_FUEL);
    let failed: Mutex<Option<String>> = Mutex::new(None);
    // run workgroups until there are none left; each thread with its own lanes and shared memory
    let one = |worker: &mut pipe::Worker, mem: &mut dyn Memory| {
        let (_, mut common) = worker.parts(&work, mem);
        let mut lanes: Vec<Lanes<N>> = (0..batches).map(|_| Lanes::new()).collect();
        for l in &mut lanes {
            l.set_local_size(launch.local);
        }
        let mut shared = vec![0u8; launch.shared];
        loop {
            let g = next.fetch_add(1, Ordering::Relaxed);
            if g >= groups {
                break;
            }
            let (x, y, z) = ((g % gx as u64) as u32, (g / gx as u64 % gy as u64) as u32, (g / gx as u64 / gy as u64) as u32);
            shared.fill(0);
            let mut left: Vec<bool> = vec![true; batches];
            // the instructions this workgroup may run come out of the launch's before it starts (at
            // most a worker's share of it, so that workgroups running at once cannot take it all)
            let want = (FUEL * batches as u64).min(LAUNCH_FUEL / workers as u64);
            let start = left_fuel.fetch_update(Ordering::Relaxed, Ordering::Relaxed, |f| Some(f.saturating_sub(want))).map_or(0, |f| f.min(want));
            let mut fuel = start;
            for (b, l) in lanes.iter_mut().enumerate() {
                let n = (threads as usize - b * N).min(N);
                l.start(program, (1u64 << n).wrapping_sub(1) as u32, 0);
            }
            // everyone to the next barrier, until all are done
            let mut stop = None;
            while stop.is_none() && left.iter().any(|&l| l) {
                for b in 0..batches {
                    if !left[b] {
                        continue;
                    }
                    let mut host = Group::<N> { common: &mut common, shared: &mut shared[..], group: [x, y, z], local: locals[b] };
                    match maxwell_shader::run(program, &mut lanes[b], &mut host, &mut fuel) {
                        Exit::Done => left[b] = false,
                        Exit::Barrier => {}
                        e => {
                            stop = Some(format!("compute shader stopped: {e:?}"));
                            break;
                        }
                    }
                }
            }
            // what it did not use goes back
            left_fuel.fetch_add(fuel, Ordering::Relaxed);
            if let Some(e) = stop {
                failed.lock().unwrap().get_or_insert(e);
                next.store(groups, Ordering::Relaxed);
                break;
            }
        }
    };
    #[cfg(not(target_arch = "wasm32"))]
    if workers > 1 {
        let pool = &mut pipe.workers[..workers];
        let split = mem.split(workers, &mut |views| {
            let slots: Vec<Mutex<(&mut pipe::Worker, Box<dyn Memory + Send + '_>)>> = pool.iter_mut().zip(views).map(|(w, v)| Mutex::new((w, v))).collect();
            crate::pool::run(workers, &|k| {
                let (w, v) = &mut *slots[k].lock().unwrap();
                one(w, &mut **v);
            });
        });
        if split {
            if let Some(e) = failed.into_inner().unwrap() {
                pipe.fail(e);
            }
            pipe.invalidate_textures();
            pipe.cbufs_changed = true;
            return;
        }
    }
    one(&mut pipe.workers[0], mem);
    if let Some(e) = failed.into_inner().unwrap() {
        pipe.fail(e);
    }
    pipe.invalidate_textures();
    pipe.cbufs_changed = true;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn descriptors() {
        let mut w = [0u32; 64];
        w[8] = 0x1234;
        w[12] = 7;
        w[13] = 3 << 16 | 5;
        w[17] = 0x200;
        w[18] = 8 << 16;
        w[19] = 2 << 16 | 4;
        w[20] = 1 << 7;
        (w[29 + 14], w[30 + 14]) = (0x8000_0000, 0x800 << 15 | 0x12);
        w[45] = 0x40;
        w[6] = 1 << 10;
        (w[23], w[24], w[25]) = (0x1000, 1 << 31 | 2, 99);
        let l = Launch::decode(&w);
        assert_eq!((l.entry, l.groups, l.threads, l.shared, l.local), (0x1234, [7, 5, 3], [8, 4, 2], 0x200, 0x40));
        assert_eq!(l.cbufs[7], Cbuf { address: 0x12_8000_0000, size: 0x800 });
        assert_eq!(l.releases, [(0x2_0000_1000, 99, true)]);

    }
}
