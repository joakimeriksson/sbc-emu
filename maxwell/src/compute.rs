//! The compute class (B1C0): a launch reads its descriptor (QMD) from memory and runs the shader
//! for every workgroup.
use crate::common_host;
use crate::inline::Inline;
use crate::pipe::{self, Common, Pipe, Work, L};
use crate::threed::Cbuf;
use crate::Memory;
use maxwell_shader::{Exit, Host, Lanes, Program, SurfOp, TexOp};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;

pub struct Compute {
    regs: Vec<u32>,
    inline: Inline,
    /// registers of a workgroup's further sixteen threads
    lanes: Vec<Lanes<L>>,
    shared: Vec<u8>,
    pub launches: u64,
}

impl Default for Compute {
    fn default() -> Self {
        Self { regs: vec![0; 0x1000], inline: Inline::default(), lanes: Vec::new(), shared: Vec::new(), launches: 0 }
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

/// The largest workgroup (threads), the most local memory of one, and the most instructions (of
/// sixteen threads each) for sixteen threads and in a launch: programs that do not end are stopped.
const MAX_THREADS: u64 = 4096;
const MAX_LOCAL: usize = 1 << 28;
const FUEL: u64 = 400_000_000;
const LAUNCH_FUEL: u64 = 20_000_000_000;

struct Group<'a, 'm> {
    common: &'a mut Common<'m>,
    shared: &'a mut [u8],
    group: [u32; 3],
    /// each lane's thread
    local: [[u32; L]; 3],
}

impl Host<L> for Group<'_, '_> {
    common_host!(L);

    fn sysval(&mut self, sys: u8) -> [u32; L] {
        match sys {
            0x20 => std::array::from_fn(|l| self.local[0][l] | self.local[1][l] << 16 | self.local[2][l] << 26),
            0x21..=0x23 => self.local[sys as usize - 0x21],
            0x25..=0x27 => [self.group[sys as usize - 0x25]; L],
            _ => [0; L],
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
        let [tx, ty, tz] = launch.threads.map(|t| t.max(1));
        let threads = tx as u64 * ty as u64 * tz as u64;
        if threads > MAX_THREADS {
            pipe.fail(format!("compute workgroup of {threads} threads"));
            return;
        }
        let batches = (threads as usize).div_ceil(L);
        if launch.local > pipe::MAX_LOCAL || launch.local * batches * L > MAX_LOCAL {
            pipe.fail(format!("compute shader with {} bytes of local memory", launch.local));
            return;
        }
        // the descriptor invalidates the caches
        pipe.invalidate_textures();
        pipe.cbufs_changed = true;
        let work = Work::compute(pipe, launch.cbufs, self.address(0x1574), self.address(0x155c), self.r(0x2608));
        // which thread each lane is
        let thread = |batch: usize, lane: usize| {
            let t = (batch * L + lane) as u32;
            [t % tx, t / tx % ty, t / tx / ty]
        };
        let locals: Vec<[[u32; L]; 3]> = (0..batches).map(|b| std::array::from_fn(|k| std::array::from_fn(|l| thread(b, l)[k]))).collect();
        let [gx, gy, gz] = launch.groups;
        let groups = gx as u64 * gy as u64 * gz as u64;
        // workgroups run on several threads unless they may meet in memory other than by writing
        // different bytes (global atomics)
        let atomics = program.any(|op| matches!(op, maxwell_shader::isa::Op::Atom { .. } | maxwell_shader::isa::Op::Suatom { .. }));
        // (each thread has the local memory of a workgroup's threads)
        let per_thread = (launch.local * batches * L).max(1);
        let workers = if atomics { 1 } else { pipe::threads().min(groups as usize).min(MAX_LOCAL / per_thread).max(1) };
        pipe.need_workers(workers);
        let next = AtomicU64::new(0);
        let left_fuel = AtomicU64::new(LAUNCH_FUEL);
        let failed: Mutex<Option<String>> = Mutex::new(None);
        // run workgroups until there are none left; each thread with its own lanes and shared memory
        let one = |worker: &mut pipe::Worker, mem: &mut dyn Memory, lanes: &mut Vec<Lanes<L>>, shared: &mut Vec<u8>| {
            let (_, mut common) = worker.parts(&work, mem);
            while lanes.len() < batches {
                lanes.push(Lanes::new());
            }
            for l in &mut lanes[..batches] {
                l.set_local_size(launch.local);
            }
            shared.resize(launch.shared, 0);
            loop {
                let g = next.fetch_add(1, Ordering::Relaxed);
                if g >= groups {
                    break;
                }
                let (x, y, z) = ((g % gx as u64) as u32, (g / gx as u64 % gy as u64) as u32, (g / gx as u64 / gy as u64) as u32);
                shared.fill(0);
                let mut left: Vec<bool> = vec![true; batches];
                let start = (FUEL * batches as u64).min(left_fuel.load(Ordering::Relaxed));
                let mut fuel = start;
                for (b, l) in lanes[..batches].iter_mut().enumerate() {
                    let n = (threads as usize - b * L).min(L);
                    l.start(program, (1u64 << n).wrapping_sub(1) as u32, 0);
                }
                // everyone to the next barrier, until all are done
                let mut stop = None;
                while stop.is_none() && left.iter().any(|&l| l) {
                    for b in 0..batches {
                        if !left[b] {
                            continue;
                        }
                        let mut host = Group { common: &mut common, shared: &mut shared[..], group: [x, y, z], local: locals[b] };
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
                let _ = left_fuel.fetch_update(Ordering::Relaxed, Ordering::Relaxed, |f| Some(f.saturating_sub(start - fuel)));
                if let Some(e) = stop {
                    failed.lock().unwrap().get_or_insert(e);
                    next.store(groups, Ordering::Relaxed);
                    break;
                }
            }
        };
        #[cfg(not(target_arch = "wasm32"))]
        if workers > 1 {
            if let Some(views) = mem.split_mut(workers) {
                type Slot<'a> = Mutex<(&'a mut pipe::Worker, Box<dyn Memory + Send + 'a>)>;
                let slots: Vec<Slot> = pipe.workers[..workers].iter_mut().zip(views).map(|(w, v)| Mutex::new((w, v))).collect();
                crate::pool::run(workers, &|k| {
                    let (w, v) = &mut *slots[k].lock().unwrap();
                    one(w, &mut **v, &mut Vec::new(), &mut Vec::new());
                });
                drop(slots);
                if let Some(e) = failed.into_inner().unwrap() {
                    pipe.fail(e);
                }
                pipe.invalidate_textures();
                pipe.cbufs_changed = true;
                return;
            }
        }
        one(&mut pipe.workers[0], mem, &mut self.lanes, &mut self.shared);
        let failed = failed.into_inner().unwrap();
        if let Some(e) = failed {
            pipe.fail(e);
        }
        pipe.invalidate_textures();
        pipe.cbufs_changed = true;
    }
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
