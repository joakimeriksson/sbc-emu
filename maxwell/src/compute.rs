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
                if crate::trace_sem() { eprintln!("[sem] compute report {at:#x} = {:#x} (op {v:#x})", self.r(0x1b08)); }
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
            if crate::trace_sem() { eprintln!("[sem] qmd release {at:#x} = {payload:#x}"); }
            mem.write(at, &payload.to_le_bytes());
            if !short {
                mem.write(at + 8, &now.to_le_bytes());
            }
        }
    }

    fn run(&mut self, launch: &Launch, program: &Program, pipe: &mut Pipe, mem: &mut dyn Memory) {
        // 32-lane warps, as CUDA's kernels assume; in the browser a kernel that cannot see the warp
        // width (no shuffles, votes, lane ids or lane masks) runs in the shader JIT's 16 lanes
        let narrow = cfg!(target_arch = "wasm32") && narrow(program);
        if narrow {
            run_lanes::<16>(launch, program, pipe, mem, self.address(0x1574), self.address(0x155c), self.r(0x2608));
        } else {
            run_lanes::<W>(launch, program, pipe, mem, self.address(0x1574), self.address(0x155c), self.r(0x2608));
        }
    }
}

/// Whether a program may run in 16 lanes, the shader JIT's, instead of a 32-lane warp: it cannot
/// see the warp's width (no shuffles, votes, lane ids or lane masks), and its threads do not talk
/// through shared memory (warp-synchronous code, `s[t] += s[t + 16]` with no barrier, needs the
/// whole warp to run each step before the next).
#[cfg_attr(not(target_arch = "wasm32"), allow(dead_code))]
fn narrow(program: &Program) -> bool {
    use maxwell_shader::isa::Op;
    !shares_memory(program) && !program.any(|op| matches!(op, Op::Shfl { .. } | Op::Vote { .. } | Op::S2R { sys: 0x00 | 0x38..=0x3c, .. }))
}

/// Whether a workgroup's threads meet through shared memory or wait for each other at barriers.
fn shares_memory(program: &Program) -> bool {
    use maxwell_shader::isa::Op;
    program.any(|op| matches!(op, Op::Bar | Op::Lds { .. } | Op::Sts { .. } | Op::AtomS { .. }))
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
    // Warps of a workgroup that neither waits at barriers nor shares memory do not meet: they
    // are run as work of their own, which spreads a launch of few workgroups over more threads.
    let apart = batches > 1 && !shares_memory(program);
    // (each thread has the local memory of a workgroup's threads)
    let per_thread = (launch.local * batches * N).max(1);
    let units = if apart { groups * batches as u64 } else { groups };
    let workers = if atomics { 1 } else { pipe::threads().min(usize::try_from(units).unwrap_or(usize::MAX)).min(MAX_LOCAL / per_thread).max(1) };
    pipe.need_workers(workers);
    let next = AtomicU64::new(0);
    let left_fuel = AtomicU64::new(LAUNCH_FUEL);
    let failed: Mutex<Option<String>> = Mutex::new(None);
    // run work until there is none left; each thread with its own lanes and shared memory
    let one = |worker: &mut pipe::Worker, mem: &mut dyn Memory| {
        // the warps' registers, kept from earlier launches of as many lanes. A launch starts as
        // with new ones: `start` clears the registers the program uses (it reads no others) and
        // the local memory is cleared here.
        let slot = (N == 32) as usize;
        let mut lanes: Vec<Lanes<N>> = worker.warps[slot].take().and_then(|w| w.downcast::<Vec<Lanes<N>>>().ok()).map_or_else(Vec::new, |w| *w);
        lanes.resize_with(batches, Lanes::new);
        for l in &mut lanes {
            l.set_local_size(launch.local);
            l.clear_local();
        }
        let (_, mut common) = worker.parts(&work, mem);
        let mut shared = vec![0u8; launch.shared];
        let mut left: Vec<bool> = vec![false; batches];
        loop {
            let u = next.fetch_add(1, Ordering::Relaxed);
            if u >= units {
                break;
            }
            // a workgroup, or one warp of it
            let (g, warps) = if apart { (u / batches as u64, (u % batches as u64) as usize..(u % batches as u64) as usize + 1) } else { (u, 0..batches) };
            let (x, y, z) = ((g % gx as u64) as u32, (g / gx as u64 % gy as u64) as u32, (g / gx as u64 / gy as u64) as u32);
            shared.fill(0);
            left.fill(false);
            left[warps.clone()].fill(true);
            // the instructions this work may run come out of the launch's before it starts (at
            // most a worker's share of it, so that work running at once cannot take it all)
            let want = (FUEL * warps.len() as u64).min(LAUNCH_FUEL / workers as u64);
            let start = left_fuel.fetch_update(Ordering::Relaxed, Ordering::Relaxed, |f| Some(f.saturating_sub(want))).map_or(0, |f| f.min(want));
            let mut fuel = start;
            for b in warps {
                let n = (threads as usize - b * N).min(N);
                lanes[b].start(program, (1u64 << n).wrapping_sub(1) as u32, 0);
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
                next.store(units, Ordering::Relaxed);
                break;
            }
        }
        worker.warps[slot] = Some(Box::new(lanes));
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

    /// Memory whose split views share it, as the Tegra's do.
    use std::sync::atomic::AtomicU8;
    struct Ram(Vec<AtomicU8>);
    struct View<'a>(&'a [AtomicU8]);
    impl Memory for View<'_> {
        fn read(&self, a: u64, out: &mut [u8]) {
            for (k, b) in out.iter_mut().enumerate() { *b = self.0[a as usize + k].load(Ordering::Relaxed); }
        }
        fn write(&mut self, a: u64, data: &[u8]) {
            for (k, &b) in data.iter().enumerate() { self.0[a as usize + k].store(b, Ordering::Relaxed); }
        }
    }
    impl Memory for Ram {
        fn read(&self, a: u64, out: &mut [u8]) { View(&self.0).read(a, out) }
        fn write(&mut self, a: u64, data: &[u8]) { View(&self.0).write(a, data) }
        fn split(&mut self, n: usize, run: &mut dyn FnMut(Vec<Box<dyn Memory + Send + '_>>)) -> bool {
            let ram = &self.0[..];
            run((0..n).map(|_| Box::new(View(ram)) as Box<dyn Memory + Send + '_>).collect());
            true
        }
    }

    /// An instruction word: the top 16 bits and fields (position, length, value), as in
    /// maxwell-shader's tests; `pred` is the guard (7: always, 0: P0).
    fn word(top: u64, pred: u64, fields: &[(u32, u32, u64)]) -> u64 {
        let mut w = top << 48 | pred << 16;
        for &(pos, len, v) in fields {
            w |= (v & ((1 << len) - 1)) << pos;
        }
        w
    }

    /// Code with scheduling words; instruction n is at `at(n)`.
    fn program(ins: &[u64]) -> Program {
        let exit = 0xe300_0000_0007_000f;
        let mut w = Vec::new();
        for (n, i) in ins.iter().chain([&exit]).enumerate() {
            if n % 3 == 0 {
                w.push(0x001f_8000_fc00_07e0u64);
            }
            w.push(*i);
        }
        Program::decode(&w.iter().flat_map(|x| x.to_le_bytes()).collect::<Vec<u8>>()).unwrap()
    }

    fn at(n: usize) -> u64 {
        (n + n / 3 + 1) as u64 * 8
    }

    fn bra(pred: u64, from: usize, to: usize) -> u64 {
        word(0xe240, pred, &[(0, 5, 15), (20, 24, at(to).wrapping_sub(at(from) + 8) & 0xff_ffff)])
    }

    fn launch(groups: u32) -> Launch {
        Launch { entry: 0, groups: [groups, 1, 1], threads: [1, 1, 1], shared: 0, local: 0, cbufs: [Cbuf::default(); 18], releases: Vec::new() }
    }

    const ST: u64 = 0xeed8;
    const LD: u64 = 0xeed0;

    #[test]
    fn workgroups_that_wait_for_each_other_through_memory() {
        // workgroup 0 counts to 20000, then sets the flag at 0x1000; the others wait for it and
        // store what they saw at 0x1100 + 4 * group. The workgroups' threads share memory, so the
        // waiters see the flag while workgroup 0 runs on another thread.
        let p = program(&[
            word(0xf0c8, 7, &[(0, 8, 0), (20, 8, 0x25)]),                                                // 0: r0 = group
            word(0x3660, 7, &[(3, 3, 0), (0, 3, 7), (8, 8, 0), (20, 19, 0), (39, 3, 7), (49, 3, 5)]),   // 1: p0 = r0 != 0
            bra(0, 2, 9),                                                                                // 2: (p0) bra 9
            word(0x0100, 7, &[(0, 8, 2), (12, 4, 15), (20, 32, 0)]),                                     // 3: r2 = 0
            word(0x3810, 7, &[(0, 8, 2), (8, 8, 2), (20, 19, 1)]),                                       // 4: r2 += 1
            word(0x3660, 7, &[(3, 3, 0), (0, 3, 7), (8, 8, 2), (20, 19, 20000), (39, 3, 7), (49, 3, 1)]), // 5: p0 = r2 < 20000
            bra(0, 6, 4),                                                                                // 6: (p0) bra 4
            word(ST, 7, &[(0, 8, 2), (8, 8, 0xff), (20, 24, 0x1000), (48, 3, 4)]),                       // 7: [0x1000] = r2
            word(0xe300, 7, &[(0, 5, 15)]),                                                              // 8: exit
            word(LD, 7, &[(0, 8, 1), (8, 8, 0xff), (20, 24, 0x1000), (48, 3, 4)]),                       // 9: r1 = [0x1000]
            word(0x3660, 7, &[(3, 3, 0), (0, 3, 7), (8, 8, 1), (20, 19, 0), (39, 3, 7), (49, 3, 2)]),   // 10: p0 = r1 == 0
            bra(0, 11, 9),                                                                               // 11: (p0) bra 9
            word(0x3848, 7, &[(0, 8, 3), (8, 8, 0), (20, 19, 2)]),                                       // 12: r3 = r0 << 2
            word(ST, 7, &[(0, 8, 1), (8, 8, 3), (20, 24, 0x1100), (48, 3, 4)]),                          // 13: [r3 + 0x1100] = r1
        ]);
        let mut pipe = Pipe::default();
        let mut mem = Ram((0..0x2000).map(|_| AtomicU8::new(0)).collect());
        let groups = 8;
        run_lanes::<W>(&launch(groups), &p, &mut pipe, &mut mem, 0, 0, 0);
        assert!(pipe.errors.is_empty(), "{:?}", pipe.errors);
        let mut b = [0u8; 4];
        mem.read(0x1000, &mut b);
        assert_eq!(b, 20000u32.to_le_bytes());
        for g in 1..groups as u64 {
            mem.read(0x1100 + 4 * g, &mut b);
            assert_eq!(b, 20000u32.to_le_bytes(), "group {g}");
        }
    }

    #[test]
    fn sixteen_lanes_only_for_threads_that_do_not_talk() {
        // r1 = 7; [0x1000] = r1: no lane can tell how wide the warp is
        let alone = [word(0x0100, 7, &[(0, 8, 1), (12, 4, 15), (20, 32, 7)]), word(ST, 7, &[(0, 8, 1), (8, 8, 0xff), (20, 24, 0x1000), (48, 3, 4)])];
        assert!(narrow(&program(&alone)));
        // s[r0] = r1; r2 = s[r0 + 64] with no barrier between: warp-synchronous
        let sts = word(0xef58, 7, &[(0, 8, 1), (8, 8, 0), (20, 24, 0), (48, 3, 4)]);
        let lds = word(0xef48, 7, &[(0, 8, 2), (8, 8, 0), (20, 24, 64), (48, 3, 4)]);
        assert!(!narrow(&program(&[sts, lds])));
        // the lane id
        assert!(!narrow(&program(&[word(0xf0c8, 7, &[(0, 8, 0), (20, 8, 0)])])));
    }

}
