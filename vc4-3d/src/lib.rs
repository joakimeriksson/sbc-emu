//! Broadcom VideoCore IV 3D (V3D 2.1), the 3D GPU of the BCM2835/6/7, as Linux's `vc4` DRM
//! driver and Mesa's `vc4` Gallium driver use it. Reference: Broadcom's *VideoCore IV 3D
//! Architecture Reference Guide* (2013) and the Mesa and Linux sources.
//!
//! * one 4 KiB register block (`V3D_*` in the kernel's `vc4_regs.h`): identification,
//!   interrupts (frame done, binner flush done, binner out of memory), the two control-list
//!   threads (CT0 binning, CT1 rendering: a job runs when the kernel writes CTnEA), the binner's
//!   overflow memory (BPOA/BPOS), user QPU programs, performance counters;
//! * no MMU: buffers are contiguous and addressed by bus address;
//! * [`bin`] (CT0 and the tile binner), [`render`] (CT1, rasterizer, fragment shading),
//!   [`tlb`] (tile buffer loads and stores), [`qpu`] (shader processor), [`tmu`] (textures),
//!   [`shade`] (vertex fetch and vertex shading), [`user`] (user QPU programs).
//!
//! Jobs run when they are started, here (deterministic; the browser) or, while the cores run on
//! host threads, on a GPU thread of their own; the done interrupt comes after the host time a
//! job took (scaled by `SBC_GPU_TIME`), or when the GPU thread has finished it.

pub mod asm;
pub mod bin;
pub mod capture;
pub mod cl;
pub mod disasm;
pub mod mem;
pub mod qpu;
pub mod render;
pub mod shade;
pub mod tiling;
pub mod tlb;
pub mod tmu;
pub mod user;

use mem::Mem;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};

static LOG_ON: AtomicBool = AtomicBool::new(false);
/// `SBC_VC4_CL=1`: every control-list packet the threads execute
static CL_ON: AtomicBool = AtomicBool::new(false);
/// QPU instructions still to trace (`SBC_VC4_TRACE=N`)
pub static TRACE: std::sync::atomic::AtomicI64 = std::sync::atomic::AtomicI64::new(0);
pub fn cl_log_enabled() -> bool { CL_ON.load(Ordering::Relaxed) }
thread_local! { static SEEN: std::cell::RefCell<std::collections::HashSet<String>> = Default::default(); }

/// Diagnostics on stderr (`SBC_VC4_LOG=1`).
pub fn log(s: &str) { if LOG_ON.load(Ordering::Relaxed) { eprintln!("\r{s}\r"); } }
pub fn log_enabled() -> bool { LOG_ON.load(Ordering::Relaxed) }

/// Log a message the first time it is seen (at most 256 distinct ones).
pub fn log_first(s: &str) -> bool {
    SEEN.with(|h| {
        let mut h = h.borrow_mut();
        if h.len() >= 256 || h.contains(s) { return false; }
        h.insert(s.to_string());
        if h.len() == 256 { eprintln!("\r{s}\r\n\rvc4: further diagnostics suppressed\r"); } else { eprintln!("\r{s}\r"); }
        true
    })
}

/// Work done by running jobs (QPU instructions), so the control-list address registers the
/// kernel's hang check reads move while a long job runs on the GPU thread.
static PROGRESS: AtomicU32 = AtomicU32::new(0);
pub(crate) fn progress(n: u64) { if n > 0 { PROGRESS.fetch_add(1, Ordering::Relaxed); } }

#[cfg(target_arch = "wasm32")]
#[link(wasm_import_module = "env")]
extern "C" {
    fn sbc_host_now_ms() -> f64;
}
struct Clock(#[cfg(not(target_arch = "wasm32"))] std::time::Instant, #[cfg(target_arch = "wasm32")] f64);
impl Clock {
    #[cfg(not(target_arch = "wasm32"))]
    fn now() -> Clock { Clock(std::time::Instant::now()) }
    #[cfg(not(target_arch = "wasm32"))]
    fn ns(&self) -> u64 { self.0.elapsed().as_nanos() as u64 }
    // SAFETY: a pure clock read the page provides
    #[cfg(target_arch = "wasm32")]
    fn now() -> Clock { Clock(unsafe { sbc_host_now_ms() }) }
    #[cfg(target_arch = "wasm32")]
    fn ns(&self) -> u64 { (((unsafe { sbc_host_now_ms() }) - self.0).max(0.0) * 1e6) as u64 }
}

// registers (offsets in the block)
pub const IDENT0: usize = 0x000;
pub const IDENT1: usize = 0x004;
pub const IDENT2: usize = 0x008;
pub const SCRATCH: usize = 0x010;
pub const L2CACTL: usize = 0x020;
pub const SLCACTL: usize = 0x024;
pub const INTCTL: usize = 0x030;
pub const INTENA: usize = 0x034;
pub const INTDIS: usize = 0x038;
pub const CT0CS: usize = 0x100;
pub const CT1CS: usize = 0x104;
pub const CT0EA: usize = 0x108;
pub const CT1EA: usize = 0x10c;
pub const CT0CA: usize = 0x110;
pub const CT1CA: usize = 0x114;
pub const CT00RA0: usize = 0x118;
pub const CT01RA0: usize = 0x11c;
pub const CT0LC: usize = 0x120;
pub const CT1LC: usize = 0x124;
pub const CT0PC: usize = 0x128;
pub const CT1PC: usize = 0x12c;
pub const PCS: usize = 0x130;
pub const BFC: usize = 0x134;
pub const RFC: usize = 0x138;
pub const BPCA: usize = 0x300;
pub const BPCS: usize = 0x304;
pub const BPOA: usize = 0x308;
pub const BPOS: usize = 0x30c;
pub const BXCF: usize = 0x310;
pub const SQRSV0: usize = 0x410;
pub const SQRSV1: usize = 0x414;
pub const SQCNTL: usize = 0x418;
pub const SRQPC: usize = 0x430;
pub const SRQUA: usize = 0x434;
pub const SRQUL: usize = 0x438;
pub const SRQCS: usize = 0x43c;
pub const VPACNTL: usize = 0x500;
pub const VPMBASE: usize = 0x504;
pub const PCTRC: usize = 0x670;
pub const PCTRE: usize = 0x674;
pub const DBQITE: usize = 0xe2c;
pub const DBQITC: usize = 0xe30;
pub const DBGE: usize = 0xf00;
pub const ERRSTAT: usize = 0xf20;

pub const INT_FRDONE: u32 = 1 << 0;
pub const INT_FLDONE: u32 = 1 << 1;
pub const INT_OUTOMEM: u32 = 1 << 2;
pub const INT_SPILLUSE: u32 = 1 << 3;

/// "V3D" and technology version 2.
pub const IDENT0_VALUE: u32 = 0x0244_3356;
/// VPM 12 KiB, HDR tile buffer, 16 semaphores, 2 TMUs and 4 QPUs per slice, 3 slices,
/// revision 1 (Mesa: V3D 2.1).
pub const IDENT1_VALUE: u32 = 12 << 28 | 1 << 24 | 16 << 16 | 2 << 12 | 4 << 8 | 3 << 4 | 1;
/// tile buffer double-buffer and size, VRI memory size
pub const IDENT2_VALUE: u32 = 1 << 8 | 4 << 4 | 1;

/// Bus address of the block (0x7ec0_0000), and the ARM's view on the BCM2837.
pub const BUS_BASE: u32 = 0x7ec0_0000;
pub const SIZE: u64 = 0x1000;
/// The GPU interrupt (ARM-side controller, GPU bank) of the V3D.
pub const GPU_IRQ: usize = 10;

/// The longest a done interrupt is held back (well under the kernel's hang check).
const MAX_DELAY_NS: u64 = 50_000_000;

/// What runs jobs: the shader code cache, the tile buffer, statistics. Shared with the GPU
/// thread.
pub struct Exec {
    pub code: qpu::Code,
    pub tb: tlb::TileBuffer,
    pub insns: u64,
    /// bin jobs, render jobs; host ns in each
    pub jobs: [u64; 2],
    pub host_ns: [u64; 2],
    pub fragments: u64,
    pub prims: u64,
    /// `SBC_VC4_CAPTURE=dir`: record jobs for `vc4-replay`
    pub capture: Option<String>,
    /// replay: the pages jobs write
    pub track: Option<std::collections::BTreeSet<u32>>,
    /// `SBC_VC4_DUMP=dir`: each finished frame as a PPM
    pub dump: Option<String>,
    frames: u64,
}

#[derive(Clone, Debug)]
pub enum Job {
    Bin { start: u32, end: u32 },
    Render { start: u32, end: u32 },
}

/// What a job left: a bin job's lists (still to be laid out in memory), or a render job's
/// outcome.
pub enum Done {
    Bin { end: u32, out: Box<bin::BinOutput>, ns: u64 },
    Render { end: u32, out: render::RenderOut, sem_used: u32, ns: u64 },
}

impl Exec {
    fn new() -> Exec {
        Exec { code: qpu::Code::default(), tb: tlb::TileBuffer::new(), insns: 0, jobs: [0; 2], host_ns: [0; 2], fragments: 0, prims: 0,
               capture: std::env::var("SBC_VC4_CAPTURE").ok().filter(|s| !s.is_empty()), track: None,
               dump: std::env::var("SBC_VC4_DUMP").ok().filter(|s| !s.is_empty()), frames: 0 }
    }

    /// Run a job on guest RAM. `sem`: the control-list semaphore as the job starts.
    pub fn run(&mut self, job: &Job, ram: &mut [u8], sem: u32) -> Done {
        let t0 = Clock::now();
        let mut m = Mem::new(ram);
        if self.capture.is_some() { m.capture = Some(Default::default()); }
        m.written = self.track.take();
        self.code.clear();
        let d = match *job {
            Job::Bin { start, end } => {
                let out = bin::run(start, end, &mut m, &mut self.code, &mut self.insns);
                self.jobs[0] += 1;
                self.prims += out.prims;
                let ns = t0.ns();
                self.host_ns[0] += ns;
                log(&format!("vc4: bin job {start:#x}..{end:#x}: {} primitives binned, {ns} ns", out.prims));
                Done::Bin { end, out: Box::new(out), ns }
            }
            Job::Render { start, end } => {
                let mut s = sem;
                let mut r = render::Render::new(&mut self.tb, &mut m, &mut self.code, &mut self.insns);
                r.run(start, end, &mut s);
                let out = r.out;
                let frame = r.frame;
                if let (Some(dir), true) = (self.dump.as_ref(), out.eof) {
                    let path = format!("{dir}/frame-{:05}.ppm", self.frames);
                    if let Err(e) = std::fs::write(&path, capture::ppm(&frame, &mut m)) { log_first(&format!("vc4: {path}: {e}")); }
                }
                if out.eof { self.frames += 1; }
                self.jobs[1] += 1;
                self.fragments += out.fragments;
                let ns = t0.ns();
                self.host_ns[1] += ns;
                log(&format!("vc4: render job {start:#x}..{end:#x}: {} primitive draws, {} fragments, {ns} ns", out.prims, out.fragments));
                Done::Render { end, out, sem_used: sem - s, ns }
            }
        };
        self.track = m.written.take();
        if let (Some(dir), Some(pages)) = (self.capture.as_ref(), m.capture.take()) {
            capture::append(dir, job, sem, &pages);
        }
        d
    }
}

#[cfg(not(target_arch = "wasm32"))]
struct RamPtr(*mut u8, usize);
// SAFETY: guest RAM outlives the machine's jobs; the GPU thread reads and writes it as DMA does.
#[cfg(not(target_arch = "wasm32"))]
unsafe impl Send for RamPtr {}

#[cfg(not(target_arch = "wasm32"))]
struct Worker {
    tx: std::sync::mpsc::Sender<(Job, RamPtr, u32, u64)>,
    done: std::sync::Arc<std::sync::Mutex<Vec<(u64, Done)>>>,
    busy: std::sync::Arc<std::sync::atomic::AtomicUsize>,
}

/// A bin job whose lists are being laid out in memory (waiting for overflow memory if
/// `PCS.BMOOM`).
struct PendingBin { end: u32, out: Box<bin::BinOutput>, w: bin::Writer, ns: u64 }

pub struct V3d {
    regs: Vec<u32>,
    /// latched interrupt status (FRDONE, FLDONE, SPILLUSE); OUTOMEM is a level
    intctl: u32,
    intena: u32,
    pub exec: std::sync::Arc<std::sync::Mutex<Exec>>,
    /// the control-list semaphore (binning increments, rendering waits)
    sem: u32,
    pending_bin: Option<PendingBin>,
    /// jobs on the GPU thread (bin, render)
    running: [bool; 2],
    /// done interrupts not raised yet: (guest time, bits)
    pending: Vec<(u64, u32)>,
    pub user: user::Sched,
    pub now: u64,
    pub time_scale: f64,
    /// cores on host threads: jobs run on the GPU thread
    pub wall_clock: bool,
    gen: u64,
    #[cfg(not(target_arch = "wasm32"))]
    worker: Option<Worker>,
}

impl Default for V3d { fn default() -> Self { V3d::new() } }

impl V3d {
    pub fn new() -> V3d {
        LOG_ON.store(std::env::var_os("SBC_VC4_LOG").is_some(), Ordering::Relaxed);
        CL_ON.store(std::env::var("SBC_VC4_CL").is_ok_and(|v| v != "0"), Ordering::Relaxed);
        if let Some(n) = std::env::var("SBC_VC4_TRACE").ok().and_then(|s| s.parse().ok()) { TRACE.store(n, Ordering::Relaxed); }
        V3d {
            regs: vec![0; SIZE as usize / 4], intctl: 0, intena: 0, exec: std::sync::Arc::new(std::sync::Mutex::new(Exec::new())),
            sem: 0, pending_bin: None, running: [false; 2], pending: Vec::new(), user: user::Sched::new(), now: 0,
            time_scale: std::env::var("SBC_GPU_TIME").ok().and_then(|s| s.parse().ok()).unwrap_or(1.0), wall_clock: false, gen: 0,
            #[cfg(not(target_arch = "wasm32"))] worker: None,
        }
    }

    /// Power-on state.
    pub fn reset(&mut self) {
        self.drain();
        self.regs.iter_mut().for_each(|r| *r = 0);
        self.intctl = 0;
        self.intena = 0;
        self.sem = 0;
        self.pending_bin = None;
        self.pending.clear();
        self.user.reset();
        self.gen += 1;
    }

    pub fn exec(&self) -> std::sync::MutexGuard<'_, Exec> { self.exec.lock().unwrap_or_else(|e| e.into_inner()) }

    fn outomem(&self) -> bool { self.regs[BPOS / 4] < 256 }

    /// INTCTL as read: the latched bits and the out-of-memory level.
    pub fn intctl(&self) -> u32 { self.intctl | if self.outomem() { INT_OUTOMEM } else { 0 } }

    /// Level of the interrupt line.
    pub fn irq(&self) -> bool { self.intctl() & self.intena != 0 || self.user.host_int & self.regs[DBQITE / 4] != 0 }

    /// Statistics line.
    pub fn stats(&self) -> String {
        let e = self.exec();
        format!("{} bin jobs ({:.0} ms), {} render jobs ({:.0} ms), {} primitives, {} fragments; {} QPU instructions",
                e.jobs[0], e.host_ns[0] as f64 / 1e6, e.jobs[1], e.host_ns[1] as f64 / 1e6, e.prims, e.fragments, e.insns + self.user.insns)
    }

    pub fn read(&mut self, off: u64) -> u32 {
        let o = (off as usize & 0xffc) % SIZE as usize;
        match o {
            IDENT0 => IDENT0_VALUE,
            IDENT1 => IDENT1_VALUE,
            IDENT2 => IDENT2_VALUE,
            L2CACTL => self.regs[o / 4] & 1,
            SLCACTL => 0,
            INTCTL => self.intctl(),
            INTENA | INTDIS => self.intena,
            CT0CS | CT1CS => {
                let k = (o - CT0CS) / 4;
                let run = self.running[k] || (k == 0 && self.pending_bin.is_some());
                (run as u32) << 5 | (self.sem.min(7)) << 12 | (k == 0) as u32
            }
            CT0CA | CT1CA => {
                let k = (o - CT0CA) / 4;
                if self.running[k] { self.regs[o / 4] ^ (PROGRESS.load(Ordering::Relaxed) & 0xff) << 4 } else { self.regs[o / 4] }
            }
            PCS => {
                let bin = self.running[0] || self.pending_bin.is_some();
                let oom = self.pending_bin.as_ref().is_some_and(|_| self.outomem());
                (bin as u32) * 3 | (self.running[1] as u32) * 0xc | (oom as u32) << 8
            }
            BPCA => self.pending_bin.as_ref().map_or(self.regs[o / 4], |p| p.w.pool),
            BPCS => self.pending_bin.as_ref().map_or(self.regs[o / 4], |p| p.w.pool_left),
            SRQCS => self.user.queue_len() | (self.user.error as u32) << 7 | self.user.made << 8 | self.user.completed << 16,
            DBQITC => self.user.host_int,
            ERRSTAT => 0,
            _ => self.regs[o / 4],
        }
    }

    pub fn write(&mut self, off: u64, v: u32, ram: &mut [u8]) {
        let o = (off as usize & 0xffc) % SIZE as usize;
        match o {
            IDENT0 | IDENT1 | IDENT2 => {}
            INTCTL => self.intctl &= !(v & !INT_OUTOMEM),
            INTENA => self.intena |= v & 0xf,
            INTDIS => self.intena &= !(v & 0xf),
            CT0CS | CT1CS => {
                if v & 1 << 15 != 0 {
                    // thread reset: a bin job waiting for memory is dropped
                    if o == CT0CS { self.pending_bin = None; }
                }
            }
            CT0EA => { self.regs[o / 4] = v; let start = self.regs[CT0CA / 4]; self.submit(Job::Bin { start, end: v }, ram); }
            CT1EA => { self.regs[o / 4] = v; let start = self.regs[CT1CA / 4]; self.submit(Job::Render { start, end: v }, ram); }
            BFC | RFC => { if v & 1 != 0 { self.regs[o / 4] = 0; } }
            BPOS => {
                self.regs[o / 4] = v;
                if self.pending_bin.is_some() { self.continue_bin(ram); }
            }
            SRQPC => {
                let unif = self.regs[SRQUA / 4];
                self.user.push(v, unif);
                let mut m = Mem::new(ram);
                let mut e = self.exec.lock().unwrap_or_else(|e| e.into_inner());
                self.user.run(&mut m, &mut e.code);
            }
            SRQCS => {
                if v & 1 << 7 != 0 { self.user.error = false; }
                if v & 1 << 8 != 0 { self.user.made = 0; }
                if v & 1 << 16 != 0 { self.user.completed = 0; }
            }
            DBQITC => self.user.host_int &= !v,
            _ => self.regs[o / 4] = v,
        }
    }

    /// Start a job: on the GPU thread while the cores run on host threads, else here.
    fn submit(&mut self, job: Job, ram: &mut [u8]) {
        let k = matches!(job, Job::Render { .. }) as usize;
        if k == 0 && self.pending_bin.is_some() {
            log_first("vc4: bin job started while the last one waits for memory (dropped the old one)");
            self.pending_bin = None;
        }
        #[cfg(not(target_arch = "wasm32"))]
        if self.wall_clock {
            let w = self.worker.get_or_insert_with(|| {
                let done = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
                let busy = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
                let (tx, rx) = std::sync::mpsc::channel::<(Job, RamPtr, u32, u64)>();
                let (exec, d2, b2) = (self.exec.clone(), done.clone(), busy.clone());
                std::thread::Builder::new().name("vc4-3d".into()).spawn(move || {
                    for (job, ram, sem, gen) in rx {
                        // SAFETY: guest RAM lives as long as the machine, which outlives its jobs
                        let ram = unsafe { std::slice::from_raw_parts_mut(ram.0, ram.1) };
                        let d = exec.lock().unwrap_or_else(|e| e.into_inner()).run(&job, ram, sem);
                        d2.lock().unwrap_or_else(|e| e.into_inner()).push((gen, d));
                        b2.fetch_sub(1, Ordering::Release);
                    }
                }).expect("vc4-3d thread");
                Worker { tx, done, busy }
            });
            w.busy.fetch_add(1, Ordering::AcqRel);
            if w.tx.send((job.clone(), RamPtr(ram.as_mut_ptr(), ram.len()), self.sem, self.gen)).is_ok() {
                self.running[k] = true;
                return;
            }
            w.busy.fetch_sub(1, Ordering::AcqRel);
            self.worker = None;
        }
        self.drain_into(ram);
        let d = self.exec().run(&job, ram, self.sem);
        self.finish(d, false, ram);
    }

    /// A job is done: lay out a bin job's lists, set the end address and counters, raise (or
    /// schedule) the interrupt.
    fn finish(&mut self, d: Done, now: bool, ram: &mut [u8]) {
        match d {
            Done::Bin { end, out, ns } => {
                self.running[0] = false;
                self.pending_bin = Some(PendingBin { end, out, w: bin::Writer::default(), ns: if now { 0 } else { ns } });
                self.continue_bin(ram);
            }
            Done::Render { end, out, sem_used, ns } => {
                self.running[1] = false;
                self.regs[CT1CA / 4] = end;
                self.sem = self.sem.saturating_sub(sem_used);
                if out.eof {
                    self.regs[RFC / 4] = (self.regs[RFC / 4] + 1) & 0xff;
                    self.raise(INT_FRDONE, if now { 0 } else { ns });
                }
            }
        }
    }

    /// Go on laying out the pending bin job's lists (after more overflow memory arrived).
    fn continue_bin(&mut self, ram: &mut [u8]) {
        let Some(mut p) = self.pending_bin.take() else { return };
        let mut bpo = (self.regs[BPOA / 4], self.regs[BPOS / 4]);
        let mut m = Mem::new(ram);
        let track = self.exec().track.take();
        m.written = track;
        let r = p.w.write(&p.out, &mut bpo, &mut m);
        self.exec().track = m.written.take();
        self.regs[BPOA / 4] = bpo.0;
        self.regs[BPOS / 4] = bpo.1;
        if p.w.spilled { self.intctl |= INT_SPILLUSE; }
        match r {
            bin::Written::OutOfMemory => {
                log("vc4: binner out of memory, waiting for overflow memory");
                self.pending_bin = Some(p);
            }
            bin::Written::Done => {
                self.regs[CT0CA / 4] = p.end;
                self.regs[BPCA / 4] = p.w.pool;
                self.regs[BPCS / 4] = p.w.pool_left;
                if p.out.flushed {
                    self.sem += p.out.sem;
                    self.regs[BFC / 4] = (self.regs[BFC / 4] + 1) & 0xff;
                    self.raise(INT_FLDONE, p.ns);
                }
            }
        }
    }

    fn raise(&mut self, bits: u32, ns: u64) {
        let delay = if self.wall_clock { 0 } else { ((ns as f64 * self.time_scale) as u64).min(MAX_DELAY_NS) };
        if delay == 0 { self.intctl |= bits; } else { self.pending.push((self.now + delay, bits)); }
    }

    /// Take jobs the GPU thread finished.
    fn collect(&mut self, ram: &mut [u8]) {
        #[cfg(not(target_arch = "wasm32"))]
        {
            let Some(w) = self.worker.as_ref() else { return };
            let done: Vec<(u64, Done)> = std::mem::take(&mut *w.done.lock().unwrap_or_else(|e| e.into_inner()));
            for (gen, d) in done {
                if gen == self.gen { self.finish(d, true, ram); } else { self.running = [false; 2]; }
            }
        }
        let _ = ram;
    }

    /// Wait for the GPU thread, then take its results.
    fn drain_into(&mut self, ram: &mut [u8]) {
        #[cfg(not(target_arch = "wasm32"))]
        if let Some(w) = self.worker.as_ref() {
            while w.busy.load(Ordering::Acquire) != 0 { std::thread::sleep(std::time::Duration::from_micros(50)); }
        }
        self.collect(ram);
    }

    /// Wait for the GPU thread and drop what it was doing (reset).
    fn drain(&mut self) {
        #[cfg(not(target_arch = "wasm32"))]
        if let Some(w) = self.worker.as_ref() {
            while w.busy.load(Ordering::Acquire) != 0 { std::thread::sleep(std::time::Duration::from_micros(50)); }
            w.done.lock().unwrap_or_else(|e| e.into_inner()).clear();
        }
        self.running = [false; 2];
    }

    /// Raise the interrupts due at `now`, take finished GPU-thread jobs.
    pub fn tick(&mut self, now: u64, ram: &mut [u8]) {
        self.now = now;
        if self.running != [false; 2] { self.collect(ram); }
        // a bin job restored from a snapshot before its lists were laid out
        if self.pending_bin.as_ref().is_some_and(|p| !p.w.started) { self.continue_bin(ram); }
        if self.pending.is_empty() { return; }
        let mut bits = 0;
        self.pending.retain(|&(t, b)| { if t <= now { bits |= b; false } else { true } });
        self.intctl |= bits;
    }

    pub fn next_event(&self) -> Option<u64> { self.pending.iter().map(|p| p.0).min() }

    /// Whether a job is in flight (the GPU thread, or the binner waiting for memory).
    pub fn busy(&self) -> bool { self.running != [false; 2] || self.pending_bin.is_some() }

    pub fn save_state(&self, w: &mut emu_core::snap::W) {
        // jobs the GPU thread finished but nobody collected yet count as finished: wait for it,
        // then fold their results into what is written (saving takes the device by reference)
        let (mut regs, mut intctl, mut sem) = (self.regs.clone(), self.intctl, self.sem);
        let mut bin_done: Option<(u32, bin::BinOutput, u64)> = None;
        #[cfg(not(target_arch = "wasm32"))]
        if let Some(wk) = self.worker.as_ref() {
            while wk.busy.load(Ordering::Acquire) != 0 { std::thread::sleep(std::time::Duration::from_micros(50)); }
            for (gen, d) in wk.done.lock().unwrap_or_else(|e| e.into_inner()).iter() {
                if *gen != self.gen { continue; }
                match d {
                    Done::Render { end, out, sem_used, .. } => {
                        regs[CT1CA / 4] = *end;
                        sem = sem.saturating_sub(*sem_used);
                        if out.eof { regs[RFC / 4] = (regs[RFC / 4] + 1) & 0xff; intctl |= INT_FRDONE; }
                    }
                    Done::Bin { end, out, .. } => bin_done = Some((*end, (**out).clone(), 0)),
                }
            }
        }
        w.tag("vc4-3d-v1");
        for r in &regs { w.u32(*r); }
        w.u32(intctl);
        w.u32(self.intena);
        w.u32(sem);
        w.u64(self.pending.len() as u64);
        for &(t, b) in &self.pending { w.u64(t); w.u32(b); }
        let e = self.exec();
        w.u32(e.tb.clear_color[0]);
        w.u32(e.tb.clear_color[1]);
        w.u32(e.tb.clear_z);
        w.u8(e.tb.clear_s);
        w.u8(e.tb.clear_vg);
        drop(e);
        let fresh = bin::Writer::default();
        let pb: Option<(u32, &bin::BinOutput, &bin::Writer, u64)> = match (&self.pending_bin, &bin_done) {
            (Some(p), _) => Some((p.end, &p.out, &p.w, p.ns)),
            (None, Some((end, out, ns))) => Some((*end, out, &fresh, *ns)),
            _ => None,
        };
        w.bool(pb.is_some());
        if let Some((end, o, wr, ns)) = pb {
            w.u32(end);
            w.u64(ns);
            let c = o.cfg.unwrap_or_default();
            for x in [c.alloc, c.alloc_size, c.tsda, c.tiles_x, c.tiles_y, c.init_block, c.block] { w.u32(x); }
            w.bool(c.ms);
            w.bool(c.hdr);
            w.bool(o.flushed);
            w.u32(o.sem);
            w.u64(o.lists.len() as u64);
            for (l, s) in o.lists.iter().zip(&o.starts) {
                w.bytes(l);
                w.u64(s.len() as u64);
                for &x in s { w.u32(x); }
            }
            for x in [wr.tile as u32, wr.rec as u32, wr.cur, wr.room, wr.pool, wr.pool_left] { w.u32(x); }
            w.bool(wr.started);
            w.bool(wr.spilled);
        }
    }

    pub fn load_state(&mut self, r: &mut emu_core::snap::R) -> Result<(), emu_core::snap::SnapError> {
        self.drain();
        r.tag("vc4-3d-v1")?;
        for k in 0..self.regs.len() { self.regs[k] = r.u32()?; }
        self.intctl = r.u32()?;
        self.intena = r.u32()?;
        self.sem = r.u32()?;
        self.pending.clear();
        for _ in 0..r.u64()? { let (t, b) = (r.u64()?, r.u32()?); self.pending.push((t, b)); }
        {
            let mut e = self.exec();
            e.tb.clear_color = [r.u32()?, r.u32()?];
            e.tb.clear_z = r.u32()?;
            e.tb.clear_s = r.u8()?;
            e.tb.clear_vg = r.u8()?;
            e.tb.clear(true, true, true);
        }
        self.pending_bin = None;
        if r.bool()? {
            let end = r.u32()?;
            let ns = r.u64()?;
            let mut c = bin::TileCfg::default();
            for x in [&mut c.alloc, &mut c.alloc_size, &mut c.tsda, &mut c.tiles_x, &mut c.tiles_y, &mut c.init_block, &mut c.block] { *x = r.u32()?; }
            c.ms = r.bool()?;
            c.hdr = r.bool()?;
            let mut out = bin::BinOutput { cfg: Some(c), flushed: r.bool()?, sem: r.u32()?, ..Default::default() };
            for _ in 0..r.u64()? {
                out.lists.push(r.bytes()?.to_vec());
                let n = r.u64()?;
                let mut s = Vec::with_capacity(n as usize);
                for _ in 0..n { s.push(r.u32()?); }
                out.starts.push(s);
            }
            let mut w = bin::Writer::default();
            w.tile = r.u32()? as usize;
            w.rec = r.u32()? as usize;
            w.cur = r.u32()?;
            w.room = r.u32()?;
            w.pool = r.u32()?;
            w.pool_left = r.u32()?;
            w.started = r.bool()?;
            w.spilled = r.bool()?;
            self.pending_bin = Some(PendingBin { end, out: Box::new(out), w, ns });
        }
        self.user.reset();
        Ok(())
    }

    /// Wait for jobs on the GPU thread and take their results (before a snapshot).
    pub fn settle(&mut self, ram: &mut [u8]) { self.drain_into(ram); }
}

#[cfg(test)]
mod tests;
