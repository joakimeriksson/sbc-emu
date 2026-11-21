//! Broadcom V3D 4.2 (the VideoCore VI 3D GPU of the BCM2711), as driven by the stock Linux
//! `v3d` kernel driver and Mesa's `v3d`/`v3dv` userspace drivers.
//!
//! * hub registers at 0x0000 (identification, interrupts, TFU, MMU) and core 0 at 0x4000
//!   (control list executors, PTB, CSD, caches, performance counters);
//! * the GPU's own MMU: one level of 4 KiB page table entries for a 4 GiB space;
//! * jobs start when the kernel writes the end address of a control list, the TFU's ICFG or
//!   the CSD's CFG0: binning (`bin`), rendering (`render`), texture formatting (`tfu`),
//!   compute (`csd`). With the cores on host threads (`--threads multi`) a job runs on a GPU
//!   thread of its own, as the real GPU runs beside the CPUs, and its done interrupt comes when
//!   it has finished; otherwise it runs to completion in the register write (repeatable runs,
//!   the browser).

pub mod bits;
pub mod packets;
pub mod qpu;
pub mod mem;
pub mod formats;
pub mod tiling;
pub mod tmu;
pub mod tfu;
pub mod csd;
pub mod bin;
pub mod render;
pub mod par;
pub mod raster;

use std::collections::HashMap;
pub use mem::Gmem;

static LOG_ON: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
/// QPU instructions still to trace (`SBC_V3D_TRACE=N`)
pub static TRACE: std::sync::atomic::AtomicI64 = std::sync::atomic::AtomicI64::new(0);
thread_local! { static SEEN: std::cell::RefCell<std::collections::HashSet<String>> = Default::default(); }

/// Diagnostics on stderr (`SBC_V3D_LOG=1`).
pub fn log(s: &str) { if LOG_ON.load(std::sync::atomic::Ordering::Relaxed) { eprintln!("\r{s}\r"); } }
/// `SBC_V3D_DRAWS=1`: one line per distinct draw state (targets, blend, depth) and store format.
pub fn draws_enabled() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| std::env::var("SBC_V3D_DRAWS").map_or(false, |v| v != "0"))
}
pub fn log_enabled() -> bool { LOG_ON.load(std::sync::atomic::Ordering::Relaxed) }
/// A diagnostic printed once per message (always).
pub fn log_once(s: &str) { log_first(s); }

/// The top byte of the word a bin job leaves at the start of its tile-list memory.
const MARK: u32 = 0xb100_0000;

/// Work done by jobs, counted by `Thread::run` as shaders run. On hardware the registers Linux
/// polls when a job overruns its 500 ms timeout (CTnCA for bin and render, CSD_CURRENT_CFG4 for
/// compute) move as the job advances, and the kernel extends the timeout while they do. Here a
/// job runs on the GPU thread and sets them only when it ends, so while one runs, reads show this
/// count mixed in: otherwise a job taking more than about a second of host time (a ray-marching
/// WebGL demo, say) looks hung, the kernel resets the GPU and the frame is lost. A shader that
/// really never ends is still stopped by its instruction budget.
static PROGRESS: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);

pub(crate) fn progress() { PROGRESS.fetch_add(1, std::sync::atomic::Ordering::Relaxed); }

/// Log a message the first time it is seen; true if it was new.
/// At most 256 distinct messages are kept (and printed), so a guest that faults at ever new
/// addresses neither floods the terminal nor grows the set without bound.
pub fn log_first(s: &str) -> bool {
    const MAX: usize = 256;
    SEEN.with(|h| {
        let mut h = h.borrow_mut();
        if h.len() >= MAX || h.contains(s) { return false; }
        h.insert(s.to_string());
        if h.len() == MAX { eprintln!("\r{s}\r\n\rv3d: further diagnostics suppressed\r"); } else { eprintln!("\r{s}\r"); }
        true
    })
}

/// Host time for job costs: `Instant` natively, the page's `performance.now()` in the browser.
#[cfg(target_arch = "wasm32")]
#[link(wasm_import_module = "env")]
extern "C" {
    /// Provided by the page (web/worker.js): `performance.now()` in milliseconds.
    fn sbc_host_now_ms() -> f64;
}
struct Clock(#[cfg(not(target_arch = "wasm32"))] std::time::Instant, #[cfg(target_arch = "wasm32")] f64);
impl Clock {
    #[cfg(not(target_arch = "wasm32"))]
    fn now() -> Clock { Clock(std::time::Instant::now()) }
    #[cfg(not(target_arch = "wasm32"))]
    fn elapsed(&self) -> std::time::Duration { self.0.elapsed() }
    // SAFETY: a pure clock read provided by the page
    #[cfg(target_arch = "wasm32")]
    fn now() -> Clock { Clock(unsafe { sbc_host_now_ms() }) }
    #[cfg(target_arch = "wasm32")]
    fn elapsed(&self) -> std::time::Duration { std::time::Duration::from_secs_f64(((unsafe { sbc_host_now_ms() }) - self.0).max(0.0) / 1000.0) }
}

/// Decoded-instruction cache for QPU code (direct-mapped), valid for one job: BOs get remapped
/// between jobs. Each slot holds the decoded instruction and its pre-decoded execution form
/// (`qpu::Pre`: operand sources resolved, whether the fast path can run it), both made once
/// when the instruction is first fetched.
#[derive(Clone)]
pub struct Code {
    tags: Vec<u32>,
    insts: Vec<qpu::Inst>,
    pre: Vec<qpu::Pre>,
    /// A shader invocation hit its instruction budget (an endless loop): the rest of the job
    /// skips its shaders, as the kernel's GPU reset would end it on the hardware, instead of
    /// spending the budget again on every remaining batch.
    pub runaway: bool,
}

const CODE_SLOTS: usize = 16384;

impl Default for Code {
    fn default() -> Self {
        Code { tags: vec![u32::MAX; CODE_SLOTS], insts: vec![qpu::Inst::Invalid(0); CODE_SLOTS], pre: vec![qpu::Pre::default(); CODE_SLOTS], runaway: false }
    }
}

impl Code {
    /// The cache slot holding the instruction at `pc`, decoding it on a miss.
    #[inline]
    pub fn slot(&mut self, pc: u32, env: &mut dyn qpu::Env) -> usize {
        let k = (pc >> 3) as usize & (CODE_SLOTS - 1);
        if self.tags[k] != pc {
            let w = env.read32(pc) as u64 | (env.read32(pc.wrapping_add(4)) as u64) << 32;
            let i = qpu::decode(w);
            self.tags[k] = pc;
            self.pre[k] = qpu::Pre::new(&i);
            self.insts[k] = i;
        }
        k
    }
    /// The instruction at `pc` (decoded, through the cache).
    pub fn fetch(&mut self, pc: u32, env: &mut dyn qpu::Env) -> qpu::Inst { let k = self.slot(pc, env); self.insts[k] }
    #[inline(always)]
    pub(crate) fn get(&self, k: usize) -> (&qpu::Inst, &qpu::Pre) { (&self.insts[k], &self.pre[k]) }
    pub fn clear(&mut self) { self.tags.iter_mut().for_each(|t| *t = u32::MAX); self.runaway = false; tmu::clear_cache(); }
}

// hub registers
const HUB_AXICFG: usize = 0x000;
const HUB_IDENT0: usize = 0x008;
const HUB_IDENT1: usize = 0x00c;
const HUB_IDENT2: usize = 0x010;
const HUB_IDENT3: usize = 0x014;
const HUB_INT_STS: usize = 0x050;
const HUB_INT_SET: usize = 0x054;
const HUB_INT_CLR: usize = 0x058;
const HUB_INT_MSK_STS: usize = 0x05c;
const HUB_INT_MSK_SET: usize = 0x060;
const HUB_INT_MSK_CLR: usize = 0x064;
const TFU_CS: usize = 0x400;
const TFU_ICFG: usize = 0x408;
const MMUC_CONTROL: usize = 0x1000;
const MMU_CTL: usize = 0x1200;
const MMU_PT_PA_BASE: usize = 0x1204;
const MMU_DEBUG_INFO: usize = 0x1238;
pub const HUB_INT_TFUC: u32 = 1 << 1;
pub const HUB_INT_MMU_PTI: u32 = 1 << 4;

// core registers
const CTL_IDENT0: usize = 0x000;
const CTL_IDENT1: usize = 0x004;
const CTL_IDENT2: usize = 0x008;
const CTL_L2TCACTL: usize = 0x030;
const CTL_INT_STS: usize = 0x050;
const CTL_INT_SET: usize = 0x054;
const CTL_INT_CLR: usize = 0x058;
const CTL_INT_MSK_STS: usize = 0x05c;
const CTL_INT_MSK_SET: usize = 0x060;
const CTL_INT_MSK_CLR: usize = 0x064;
const CLE_CT0CS: usize = 0x100;
const CLE_CT0CA: usize = 0x110;
const CLE_CT1CA: usize = 0x114;
const CLE_BFC: usize = 0x134;
const CLE_RFC: usize = 0x138;
const CLE_CT0QTS: usize = 0x15c;
const CLE_CT0QBA: usize = 0x160;
const CLE_CT1QBA: usize = 0x164;
const CLE_CT0QEA: usize = 0x168;
const CLE_CT1QEA: usize = 0x16c;
const CLE_CT0QMA: usize = 0x170;
const CLE_CT0QMS: usize = 0x174;
const GMP_STATUS: usize = 0x800;
const CSD_STATUS: usize = 0x900;
const CSD_QUEUED_CFG0: usize = 0x904;
const CSD_CURRENT_CFG0: usize = 0x920;
const CSD_CURRENT_CFG4: usize = 0x930;
pub const INT_FRDONE: u32 = 1 << 0;
pub const INT_FLDONE: u32 = 1 << 1;
pub const INT_OUTOMEM: u32 = 1 << 2;
pub const INT_CSDDONE: u32 = 1 << 7;

/// The GIC SPI of the (single, hub + core) V3D interrupt line on the BCM2711.
pub const SPI: usize = 74;
/// Bus/CPU base of the register block: hub at +0, core 0 at +0x4000.
pub const BASE: u64 = 0xfec0_0000;
pub const SIZE: u64 = 0x8000;

/// What one bin job left for the render job reading the same tile allocation memory.
pub type BinResults = HashMap<u32, std::sync::Arc<bin::Binned>>;

/// What runs jobs: the shader code cache, the tile lists bin jobs leave for render jobs,
/// statistics. Shared with the GPU thread.
pub struct Exec {
    pub code: Code,
    pub binned: BinResults,
    /// the bin jobs whose tile lists are live, to re-bin after a snapshot restore
    bin_jobs: HashMap<u32, [u32; 5]>,
    /// statistics: jobs (bin, render, TFU, CSD), QPU instructions, host time in jobs (ns)
    pub jobs: [u64; 4],
    pub insns: u64,
    pub host_ns: [u64; 4],
    /// bin jobs run, for the marks they leave in tile list memory
    bins: u32,
}

/// A job as the kernel started it.
#[derive(Clone)]
enum Job {
    Bin { start: u32, end: u32, qma: u32, qms: u32, qts: u32 },
    Render { start: u32, end: u32 },
    Csd { cfg: Vec<u32> },
    Tfu { regs: Vec<u32>, irq: bool },
}

/// A finished job: its interrupt, the register that reports where it ended, the frame counter
/// it advances, and the host time it took.
struct Done { gen: u64, hub: bool, bits: u32, reg: Option<(usize, u32)>, counter: Option<usize>, ns: u64 }

impl Exec {
    fn run(&mut self, job: &Job, ram: &mut [u8], pt: u64, mmu: bool, gen: u64) -> Done {
        let t0 = Clock::now();
        let mut m = Gmem::new(ram, pt, mmu);
        let (k, hub, bits, reg, counter) = match job {
            &Job::Bin { start, end, qma, qms, qts } => {
                self.code.clear();
                let res = bin::run(start, end, qma, qms, qts, &mut m, &mut self.code, &mut self.insns);
                if let Some(mut r) = res {
                    // Hardware writes the tile lists into this memory, and a render job reads
                    // whatever is there. Here they stay on the host, so leave a mark in their
                    // place: a render job uses them only while it is still there, not after the
                    // buffer was freed and reused.
                    self.bins = self.bins.wrapping_add(1);
                    r.marker = MARK | (self.bins & 0x00ff_ffff);
                    m.w32(qma, r.marker);
                    self.binned.insert(qma, std::sync::Arc::new(r));
                    self.bin_jobs.insert(qma, [start, end, qma, qms, qts]);
                } else {
                    // nothing binned: a render job must not find an earlier job's lists here
                    self.binned.remove(&qma);
                    self.bin_jobs.remove(&qma);
                }
                (0, false, if end != 0 { INT_FLDONE } else { 0 }, Some((CLE_CT0CA, end)), Some(CLE_BFC))
            }
            &Job::Render { start, end } => {
                self.code.clear();
                // after a snapshot restore: rebuild the tile lists this render job reads (only
                // those: older bin jobs' control lists may have been reused since)
                if self.binned.len() < self.bin_jobs.len() {
                    for q in render::tile_list_bases(start, end, &mut m) {
                        if self.binned.contains_key(&q) { continue; }
                        let Some(j) = self.bin_jobs.get(&q).copied() else { continue };
                        // only while the bin job's mark is still there: a buffer reused since
                        // (zeroed, or holding other data) has no tile lists to replay
                        let mark = m.r32(q);
                        if mark >> 24 != MARK >> 24 { continue; }
                        if let Some(mut r) = bin::run(j[0], j[1], j[2], j[3], j[4], &mut m, &mut self.code, &mut self.insns) { r.marker = mark; self.binned.insert(q, std::sync::Arc::new(r)); }
                    }
                }
                render::run(start, end, &self.binned, &mut m, &mut self.code, &mut self.insns);
                (1, false, INT_FRDONE, Some((CLE_CT1CA, end)), Some(CLE_RFC))
            }
            Job::Csd { cfg } => {
                self.code.clear();
                csd::run(cfg, &mut m, &mut self.code, &mut self.insns);
                (3, false, INT_CSDDONE, None, None)
            }
            Job::Tfu { regs, irq } => {
                tfu::run(regs, &mut m);
                (2, true, if *irq { HUB_INT_TFUC } else { 0 }, None, None)
            }
        };
        self.jobs[k] += 1;
        let ns = t0.elapsed().as_nanos() as u64;
        self.host_ns[k] += ns;
        Done { gen, hub, bits, reg, counter, ns }
    }
}

/// A job for a GPU thread: the job, guest RAM, the page table base, MMU on, the reset generation.
#[cfg(not(target_arch = "wasm32"))]
type Queued = (Job, RamPtr, u64, bool, u64);

/// The GPU threads: jobs in, finished jobs out. Bin, render and compute jobs run in order on one
/// thread; TFU jobs on another. On the chip the TFU is a unit of its own beside the control-list
/// threads, so a texture conversion never waits for a render job to end, and Linux counts on that:
/// its TFU timeout (500 ms) resets the GPU without looking for progress. Queued behind a render
/// job that takes the host most of a second (a big frame, or a slow host), a TFU job reached
/// that timeout; the reset path then oopsed in the Ubuntu 7.0 kernel. Linux orders TFU jobs and
/// the jobs around them through buffer fences, so running them side by side is what the
/// hardware does too.
#[cfg(not(target_arch = "wasm32"))]
struct Worker {
    tx: std::sync::mpsc::Sender<Queued>,
    tfu_tx: std::sync::mpsc::Sender<Queued>,
    done: std::sync::Arc<std::sync::Mutex<Vec<Done>>>,
    busy: std::sync::Arc<std::sync::atomic::AtomicUsize>,
}

/// Start a GPU thread running `run` on the jobs sent to it; finished jobs go to `done`, and `busy`
/// counts down as each job leaves the thread.
#[cfg(not(target_arch = "wasm32"))]
fn gpu_thread(name: &str, done: std::sync::Arc<std::sync::Mutex<Vec<Done>>>, busy: std::sync::Arc<std::sync::atomic::AtomicUsize>,
              gen_now: std::sync::Arc<std::sync::atomic::AtomicU64>,
              run: impl Fn(&Job, &mut [u8], u64, bool, u64) -> Done + Send + 'static) -> std::sync::mpsc::Sender<Queued> {
    let (tx, rx) = std::sync::mpsc::channel::<Queued>();
    std::thread::Builder::new().name(name.into()).spawn(move || {
        for (job, ram, pt, mmu, gen) in rx {
            // SAFETY: guest RAM lives as long as the machine, which outlives its jobs
            let ram = unsafe { std::slice::from_raw_parts_mut(ram.0, ram.1) };
            if gen == gen_now.load(std::sync::atomic::Ordering::Acquire) {
                let d = run(&job, ram, pt, mmu, gen);
                done.lock().unwrap_or_else(|e| e.into_inner()).push(d);
            }
            busy.fetch_sub(1, std::sync::atomic::Ordering::Release);
        }
    }).expect("v3d thread");
    tx
}

/// A TFU job on its own thread: it needs neither the shader cache nor the tile lists, so it does
/// not wait for the lock a render job holds.
#[cfg(not(target_arch = "wasm32"))]
fn run_tfu(job: &Job, ram: &mut [u8], pt: u64, mmu: bool, gen: u64, stats: &[std::sync::atomic::AtomicU64; 2]) -> Done {
    let Job::Tfu { regs, irq } = job else { unreachable!("only TFU jobs go to the TFU thread") };
    let t0 = Clock::now();
    tfu::run(regs, &mut Gmem::new(ram, pt, mmu));
    let ns = t0.elapsed().as_nanos() as u64;
    stats[0].fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    stats[1].fetch_add(ns, std::sync::atomic::Ordering::Relaxed);
    Done { gen, hub: true, bits: if *irq { HUB_INT_TFUC } else { 0 }, reg: None, counter: None, ns }
}

/// Guest RAM for the GPU thread. The cores already share it through raw pointers; the GPU
/// reads and writes it as DMA does, beside them.
#[cfg(not(target_arch = "wasm32"))]
struct RamPtr(*mut u8, usize);
#[cfg(not(target_arch = "wasm32"))]
unsafe impl Send for RamPtr {}

pub struct V3d {
    hub: Vec<u32>,
    core: Vec<u32>,
    pub exec: std::sync::Arc<std::sync::Mutex<Exec>>,
    #[cfg(not(target_arch = "wasm32"))]
    worker: Option<Worker>,
    /// TFU jobs run on the TFU thread and the host time they took (statistics)
    tfu_stats: std::sync::Arc<[std::sync::atomic::AtomicU64; 2]>,
    /// bumped by a reset: jobs still running from before it are dropped when they finish
    gen: u64,
    /// the same, for the GPU thread: jobs still queued from before a reset are not run at all
    /// (their control lists and buffers may already be freed and reused)
    gen_now: std::sync::Arc<std::sync::atomic::AtomicU64>,
    /// the PM block powered the GPU up (bcm2835-power, V3D domain)
    pub powered: bool,
    /// guest time (ns), as the SoC last told it
    pub now: u64,
    /// done interrupts not yet raised: (guest time, hub?, bits). A synchronous job's results are
    /// computed when it starts; its interrupt comes after the host time it took (scaled by
    /// `time_scale`), so the guest sees the emulated GPU's real speed.
    pending: Vec<(u64, bool, u32)>,
    pub time_scale: f64,
    /// guest time follows the host's wall clock (cores on host threads): jobs go to the GPU
    /// thread, and their interrupt comes when they finish
    pub wall_clock: bool,
}

impl Default for V3d { fn default() -> Self { Self::new() } }

impl V3d {
    pub fn new() -> V3d {
        LOG_ON.store(std::env::var_os("SBC_V3D_LOG").is_some(), std::sync::atomic::Ordering::Relaxed);
        if let Some(n) = std::env::var("SBC_V3D_TRACE").ok().and_then(|s| s.parse().ok()) { TRACE.store(n, std::sync::atomic::Ordering::Relaxed); }
        let exec = Exec { code: Code::default(), binned: HashMap::new(), bin_jobs: HashMap::new(), jobs: [0; 4], insns: 0, host_ns: [0; 4], bins: 0 };
        let mut v = V3d { hub: vec![0; 0x4000 / 4], core: vec![0; 0x4000 / 4], exec: std::sync::Arc::new(std::sync::Mutex::new(exec)),
            #[cfg(not(target_arch = "wasm32"))] worker: None, tfu_stats: Default::default(), gen: 0, gen_now: Default::default(), powered: true, now: 0, pending: Vec::new(),
            time_scale: std::env::var("SBC_GPU_TIME").ok().and_then(|s| s.parse().ok()).unwrap_or(1.0), wall_clock: false };
        // (std::env is empty in the browser: the defaults apply)
        v.reset();
        v
    }

    pub fn reset(&mut self) {
        self.hub.iter_mut().for_each(|r| *r = 0);
        self.core.iter_mut().for_each(|r| *r = 0);
        self.hub[HUB_AXICFG / 4] = 0xf;
        self.hub[HUB_INT_MSK_STS / 4] = !0;
        self.core[CTL_INT_MSK_STS / 4] = !0;
        // jobs still on the GPU thread finish into nothing
        self.gen += 1;
        self.gen_now.store(self.gen, std::sync::atomic::Ordering::Release);
        self.drain();
        let mut e = self.exec();
        e.binned.clear();
        e.bin_jobs.clear();
        drop(e);
        self.pending.clear();
    }

    fn exec(&self) -> std::sync::MutexGuard<'_, Exec> { self.exec.lock().unwrap_or_else(|e| e.into_inner()) }

    /// Statistics: (jobs per kind, QPU instructions, host ns per kind).
    pub fn counters(&self) -> ([u64; 4], u64, [u64; 4]) {
        let e = self.exec();
        let (mut jobs, mut host_ns) = (e.jobs, e.host_ns);
        jobs[2] += self.tfu_stats[0].load(std::sync::atomic::Ordering::Relaxed);
        host_ns[2] += self.tfu_stats[1].load(std::sync::atomic::Ordering::Relaxed);
        (jobs, e.insns, host_ns)
    }

    /// Whether the GPU thread has a job in hand.
    fn running(&self) -> bool {
        #[cfg(not(target_arch = "wasm32"))]
        if let Some(w) = self.worker.as_ref() { return w.busy.load(std::sync::atomic::Ordering::Acquire) != 0; }
        false
    }

    /// Wait for the GPU thread to finish its jobs, then take their results.
    pub fn drain(&mut self) {
        #[cfg(not(target_arch = "wasm32"))]
        if let Some(w) = self.worker.as_ref() {
            while w.busy.load(std::sync::atomic::Ordering::Acquire) != 0 { std::thread::sleep(std::time::Duration::from_micros(50)); }
        }
        self.collect();
    }

    /// Take the results of jobs the GPU thread has finished.
    fn collect(&mut self) {
        #[cfg(not(target_arch = "wasm32"))]
        {
            let Some(w) = self.worker.as_ref() else { return };
            let done: Vec<Done> = std::mem::take(&mut *w.done.lock().unwrap_or_else(|e| e.into_inner()));
            for d in done { if d.gen == self.gen { self.finish(&d, true); } }
        }
    }

    /// A job finished: where it ended, the frame counter, the interrupt (now if it ran on the
    /// GPU thread, as its time has passed; else after its host time).
    fn finish(&mut self, d: &Done, now: bool) {
        if let Some((r, v)) = d.reg { self.core[r / 4] = v; }
        if let Some(c) = d.counter { self.core[c / 4] = self.core[c / 4].wrapping_add(1); }
        if d.bits == 0 { return; }
        if now { if d.hub { self.hub[HUB_INT_STS / 4] |= d.bits } else { self.core[CTL_INT_STS / 4] |= d.bits } }
        else { self.done(d.hub, d.bits, d.ns); }
    }

    /// Start a job: on the GPU thread while the cores run on host threads, else here.
    fn submit(&mut self, job: Job, ram: &mut [u8]) {
        let (pt, mmu, gen) = (self.pt_base(), self.mmu_on(), self.gen);
        #[cfg(not(target_arch = "wasm32"))]
        if self.wall_clock {
            let w = self.worker.get_or_insert_with(|| {
                let done = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
                let busy = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
                let exec = self.exec.clone();
                let tx = gpu_thread("v3d", done.clone(), busy.clone(), self.gen_now.clone(),
                    move |job, ram, pt, mmu, gen| exec.lock().unwrap_or_else(|e| e.into_inner()).run(job, ram, pt, mmu, gen));
                let stats = self.tfu_stats.clone();
                let tfu_tx = gpu_thread("v3d-tfu", done.clone(), busy.clone(), self.gen_now.clone(),
                    move |job, ram, pt, mmu, gen| run_tfu(job, ram, pt, mmu, gen, &stats));
                Worker { tx, tfu_tx, done, busy }
            });
            w.busy.fetch_add(1, std::sync::atomic::Ordering::AcqRel);
            let tx = if matches!(job, Job::Tfu { .. }) { &w.tfu_tx } else { &w.tx };
            match tx.send((job, RamPtr(ram.as_mut_ptr(), ram.len()), pt, mmu, gen)) {
                Ok(()) => return,
                Err(e) => {
                    // a GPU thread is gone (it panicked): run here from now on
                    w.busy.fetch_sub(1, std::sync::atomic::Ordering::AcqRel);
                    self.worker = None;
                    let (job, ..) = e.0;
                    let d = self.exec().run(&job, ram, pt, mmu, gen);
                    self.finish(&d, false);
                    return;
                }
            }
        }
        // here: after anything still on the GPU thread, in order
        self.drain();
        let d = self.exec().run(&job, ram, pt, mmu, gen);
        self.finish(&d, false);
    }

    /// Schedule a done interrupt after `ns` of host time.
    fn done(&mut self, hub: bool, bits: u32, ns: u64) {
        // capped well under Linux's GPU job timeout (500 ms): a job the host took long over must
        // not look like a hung GPU (v3d resets it, and on 7.0 the timeout path can oops)
        let delay = if self.wall_clock { 0 } else { ((ns as f64 * self.time_scale) as u64).min(250_000_000) };
        if delay == 0 { if hub { self.hub[HUB_INT_STS / 4] |= bits } else { self.core[CTL_INT_STS / 4] |= bits } return; }
        self.pending.push((self.now + delay, hub, bits));
    }

    /// Raise the interrupts that are due at `now`.
    pub fn tick(&mut self, now: u64) {
        self.now = now;
        self.collect();
        if self.pending.is_empty() { return; }
        let (hub, core) = (&mut self.hub, &mut self.core);
        self.pending.retain(|&(t, h, bits)| {
            if t > now { return true; }
            if h { hub[HUB_INT_STS / 4] |= bits } else { core[CTL_INT_STS / 4] |= bits }
            false
        });
    }

    pub fn next_event(&self) -> Option<u64> { self.pending.iter().map(|p| p.0).min() }

    fn ident(&self, hub: bool, off: usize) -> Option<u32> {
        Some(match (hub, off) {
            (true, HUB_IDENT0) => 0x4255_4856,  // "VHUB"
            // tech version 4, revision 2, one core, one host, with TFU, TSY and MSO, no L3C
            (true, HUB_IDENT1) => 0x000e_1124,
            (true, HUB_IDENT2) => 0x100,         // with MMU
            (true, HUB_IDENT3) => 0x0000_0e00,   // IP revision 14
            (true, MMU_DEBUG_INFO) => (4 << 8) | (2 << 4) | 1, // 34-bit PA, 32-bit VA
            (false, CTL_IDENT0) => 0x0444_3356,  // "V3D" + version 4
            // 64 KiB VPM (8 x 8 KiB), 1 TMU, 4 QPUs per slice, 2 slices, minor version 2
            (false, CTL_IDENT1) => 0x8100_1422,
            (false, CTL_IDENT2) => 0x4007_8121,
            _ => return None,
        })
    }

    /// Register read at `off` from the block base.
    pub fn read(&mut self, off: u64) -> u32 {
        let hub = off < 0x4000;
        let o = (off & 0x3ffc) as usize;
        if let Some(v) = self.ident(hub, o) { return v; }
        if hub {
            match o {
                TFU_CS => 0x0000_0800, // 8 free input slots, not busy
                MMUC_CONTROL => self.hub[o / 4] & 1,
                MMU_CTL => self.hub[o / 4] & !((1 << 7) | (1 << 2)),
                _ => self.hub[o / 4],
            }
        } else {
            match o {
                CTL_L2TCACTL => 0, // flushes and cleans complete at once
                CLE_CT0CS | 0x104 => 0,  // not running
                GMP_STATUS => 0,
                CSD_STATUS => 0,
                CLE_CT0CA | CLE_CT1CA | CSD_CURRENT_CFG4 if self.running() => {
                    self.core[o / 4] ^ (PROGRESS.load(std::sync::atomic::Ordering::Relaxed) << 4)
                }
                _ => self.core[o / 4],
            }
        }
    }

    /// Register write; may run a job to completion.
    pub fn write(&mut self, off: u64, v: u32, ram: &mut [u8]) {
        let hub = off < 0x4000;
        let o = (off & 0x3ffc) as usize;
        if hub {
            match o {
                HUB_INT_SET => self.hub[HUB_INT_STS / 4] |= v,
                HUB_INT_CLR => self.hub[HUB_INT_STS / 4] &= !v,
                HUB_INT_MSK_SET => self.hub[HUB_INT_MSK_STS / 4] |= v,
                HUB_INT_MSK_CLR => self.hub[HUB_INT_MSK_STS / 4] &= !v,
                TFU_ICFG => {
                    self.hub[o / 4] = v;
                    let regs: Vec<u32> = (0..16).map(|k| self.hub[TFU_CS / 4 + k]).collect();
                    self.submit(Job::Tfu { regs, irq: v & 1 != 0 }, ram);
                }
                _ => self.hub[o / 4] = v,
            }
        } else {
            match o {
                CTL_INT_SET => self.core[CTL_INT_STS / 4] |= v,
                CTL_INT_CLR => self.core[CTL_INT_STS / 4] &= !v,
                CTL_INT_MSK_SET => self.core[CTL_INT_MSK_STS / 4] |= v,
                CTL_INT_MSK_CLR => self.core[CTL_INT_MSK_STS / 4] &= !v,
                CLE_CT0QEA => {
                    self.core[o / 4] = v;
                    let (start, end) = (self.core[CLE_CT0QBA / 4], v);
                    let (qma, qms, qts) = (self.core[CLE_CT0QMA / 4], self.core[CLE_CT0QMS / 4], self.core[CLE_CT0QTS / 4]);
                    self.core[CLE_CT0CA / 4] = start;
                    self.submit(Job::Bin { start, end, qma, qms, qts }, ram);
                }
                CLE_CT1QEA => {
                    self.core[o / 4] = v;
                    let start = self.core[CLE_CT1QBA / 4];
                    self.core[CLE_CT1CA / 4] = start;
                    self.submit(Job::Render { start, end: v }, ram);
                }
                CSD_QUEUED_CFG0 => {
                    self.core[o / 4] = v;
                    let cfg: Vec<u32> = (0..7).map(|k| self.core[CSD_QUEUED_CFG0 / 4 + k]).collect();
                    for k in 0..7 { self.core[CSD_CURRENT_CFG0 / 4 + k] = cfg[k]; }
                    self.submit(Job::Csd { cfg }, ram);
                }
                _ => self.core[o / 4] = v,
            }
        }
    }

    /// One-line job statistics.
    pub fn stats(&self) -> String {

        let (jobs, insns, host_ns) = self.counters();
        let ms = |k: usize| host_ns[k] as f64 / 1e6;
        format!("{} bin jobs ({:.0} ms), {} render jobs ({:.0} ms), {} TFU ({:.0} ms), {} compute ({:.0} ms); {} QPU instructions",
                jobs[0], ms(0), jobs[1], ms(1), jobs[2], ms(2), jobs[3], ms(3), insns)
    }

    fn mmu_on(&self) -> bool { self.hub[MMU_CTL / 4] & 1 != 0 }
    fn pt_base(&self) -> u64 { (self.hub[MMU_PT_PA_BASE / 4] as u64) << 12 }

    /// Level of the interrupt line.
    pub fn irq(&self) -> bool {
        (self.core[CTL_INT_STS / 4] & !self.core[CTL_INT_MSK_STS / 4]) != 0
            || (self.hub[HUB_INT_STS / 4] & !self.hub[HUB_INT_MSK_STS / 4]) != 0
    }

    pub fn save_state(&self, w: &mut emu_core::snap::W) {
        // finished-but-uncollected jobs count as finished: wait for the GPU thread, then fold
        // its results into what is written (save takes the machine by shared reference)
        let (mut hub, mut core) = (self.hub.clone(), self.core.clone());
        #[cfg(not(target_arch = "wasm32"))]
        if let Some(wk) = self.worker.as_ref() {
            while wk.busy.load(std::sync::atomic::Ordering::Acquire) != 0 { std::thread::sleep(std::time::Duration::from_micros(50)); }
            for d in wk.done.lock().unwrap_or_else(|e| e.into_inner()).iter().filter(|d| d.gen == self.gen) {
                if let Some((r, v)) = d.reg { core[r / 4] = v; }
                if let Some(c) = d.counter { core[c / 4] = core[c / 4].wrapping_add(1); }
                if d.hub { hub[HUB_INT_STS / 4] |= d.bits } else { core[CTL_INT_STS / 4] |= d.bits }
            }
        }
        w.tag("v3d-v1");
        for r in hub.iter().chain(core.iter()) { w.u32(*r); }
        w.u64(self.pending.len() as u64);
        for &(t, h, b) in &self.pending { w.u64(t); w.bool(h); w.u32(b); }
        let e = self.exec();
        w.u64(e.bin_jobs.len() as u64);
        for j in e.bin_jobs.values() { for &x in j { w.u32(x); } }
    }
    pub fn load_state(&mut self, r: &mut emu_core::snap::R) -> Result<(), emu_core::snap::SnapError> {
        r.tag("v3d-v1")?;
        self.gen += 1;
        self.gen_now.store(self.gen, std::sync::atomic::Ordering::Release);
        self.drain();
        for k in 0..self.hub.len() { self.hub[k] = r.u32()?; }
        for k in 0..self.core.len() { self.core[k] = r.u32()?; }
        self.pending.clear();
        for _ in 0..r.u64()? { let (t, h, b) = (r.u64()?, r.bool()?, r.u32()?); self.pending.push((t, h, b)); }
        let mut e = self.exec();
        e.binned.clear();
        e.bin_jobs.clear();
        for _ in 0..r.u64()? { let mut j = [0u32; 5]; for x in j.iter_mut() { *x = r.u32()?; } e.bin_jobs.insert(j[2], j); }
        Ok(())
    }
}

#[cfg(all(test, not(target_arch = "wasm32")))]
mod tests {
    use super::*;

    /// While a job runs on the GPU thread, the register Linux polls on a timeout moves, so a long
    /// job is not taken for a hung GPU; once it ends, the register reads as written.
    #[test]
    fn long_job_shows_progress() {
        let mut ram = vec![0u8; 0x4000];
        // at 0x1000, a shader that ends at once: two thread switches mark the final section, the
        // next one ends the thread after its two delay slots
        let nop = 0x3c00_3186_bb80_0000u64;
        let thrsw = nop | 1 << 53;
        for (k, w) in [thrsw, thrsw, nop, thrsw, nop, nop].iter().enumerate() { ram[0x1000 + k * 8..0x1008 + k * 8].copy_from_slice(&w.to_le_bytes()); }
        let mut v = V3d::new();
        v.wall_clock = true;
        // 65535 x 8 workgroups of 16 invocations: half a million short batches
        let cfg = [65535u32 << 16, 8 << 16, 1 << 16, 16 | 1 << 8, 7, 0x1000, 0x2000];
        for k in (1..7).rev() { v.write(0x4000 + (CSD_QUEUED_CFG0 + 4 * k) as u64, cfg[k], &mut ram); }
        v.write(0x4000 + CSD_QUEUED_CFG0 as u64, cfg[0], &mut ram);
        let mut seen = std::collections::HashSet::new();
        while v.running() { seen.insert(v.read(0x4000 + CSD_CURRENT_CFG4 as u64)); }
        v.drain();
        assert!(seen.len() > 1, "the batch register did not move while the job ran: {seen:?}");
        assert_eq!(v.read(0x4000 + CSD_CURRENT_CFG4 as u64), 7, "after the job it reads as written");
    }

    /// A TFU job does not wait for a long job on the control-list thread: on the chip the TFU is
    /// a unit of its own, and Linux resets the GPU when a TFU job takes 500 ms, progress or not.
    #[test]
    fn tfu_does_not_wait_for_long_jobs() {
        let mut ram = vec![0u8; 0x4000];
        let nop = 0x3c00_3186_bb80_0000u64;
        let thrsw = nop | 1 << 53;
        for (k, w) in [thrsw, thrsw, nop, thrsw, nop, nop].iter().enumerate() { ram[0x1000 + k * 8..0x1008 + k * 8].copy_from_slice(&w.to_le_bytes()); }
        let mut v = V3d::new();
        v.wall_clock = true;
        // a long compute job, as in long_job_shows_progress
        let cfg = [65535u32 << 16, 8 << 16, 1 << 16, 16 | 1 << 8, 7, 0x1000, 0x2000];
        for k in (1..7).rev() { v.write(0x4000 + (CSD_QUEUED_CFG0 + 4 * k) as u64, cfg[k], &mut ram); }
        v.write(0x4000 + CSD_QUEUED_CFG0 as u64, cfg[0], &mut ram);
        // then a 1x1 TFU copy from 0x2800 to 0x3000, interrupt on completion
        v.write((TFU_CS + 4 * 3) as u64, 0x2800, &mut ram);
        v.write((TFU_CS + 4 * 7) as u64, 0x3000, &mut ram);
        v.write((TFU_CS + 4 * 8) as u64, 1 << 16 | 1, &mut ram);
        v.write(TFU_ICFG as u64, 1, &mut ram);
        let t0 = std::time::Instant::now();
        while v.hub[HUB_INT_STS / 4] & HUB_INT_TFUC == 0 {
            v.tick(0);
            assert!(t0.elapsed() < std::time::Duration::from_secs(20), "no TFU interrupt");
        }
        assert_eq!(v.core[CTL_INT_STS / 4] & INT_CSDDONE, 0, "the TFU job waited for the compute job");
        v.drain();
        assert_ne!(v.core[CTL_INT_STS / 4] & INT_CSDDONE, 0, "the compute job finished");
        assert_eq!(v.counters().0[2], 1, "the TFU job counts in the statistics");
    }
}
