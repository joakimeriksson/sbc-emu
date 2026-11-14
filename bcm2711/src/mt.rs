//! Multi-threaded execution (`--threads multi`, as QEMU's MTTCG): each core runs on its own
//! host thread against the shared guest RAM, so busy cores really run in parallel. Runs are
//! no longer repeatable: interleavings depend on the host.
//!
//! - Devices (the SoC) are behind one lock; RAM is not (cores read and write it directly, with
//!   host fences for barriers and acquire/release, and store-exclusive as compare-and-swap).
//! - Time follows the host clock: each core moves its virtual clock up to the wall clock every
//!   few thousand instructions; a core in WFI sleeps until an interrupt or its timer is due.
//! - Interrupt lines are published per core as atomics after every device access.
//! - TLB and instruction-cache invalidations are queued for the other cores; each core has its
//!   own JIT.
//!
//! The machine still owns the cores and the SoC: `Machine::run` lets the threads run for a
//! slice of wall time and parks them again before it returns, so everything outside `run` sees
//! a stopped machine.
use crate::machine::{CoreState, Machine, Soc, Stop};
use arm_a64::Cpu;
use emu_core::{Bus, Core, Fault, Trap};
use std::sync::atomic::{AtomicBool, AtomicI64, AtomicPtr, AtomicU64, AtomicU8, AtomicUsize, Ordering::{Relaxed, SeqCst}};
use std::sync::{Arc, Condvar, Mutex, MutexGuard};
use std::time::{Duration, Instant};

const OFF: u8 = 0;
const RUN: u8 = 1;
const WFI: u8 = 2;
const WFE: u8 = 3;
const PPI_VTIMER: usize = 27;
const PPI_PTIMER: usize = 30;
/// instructions between a core's looks at the wall clock, its timers and the stop flag
const SLICE: u64 = 20_000;

fn enc(s: CoreState) -> u8 { match s { CoreState::Off => OFF, CoreState::Run => RUN, CoreState::Wfi => WFI, CoreState::Wfe => WFE } }
fn dec(s: u8) -> CoreState { match s { OFF => CoreState::Off, WFI => CoreState::Wfi, WFE => CoreState::Wfe, _ => CoreState::Run } }

pub(crate) struct Shared {
    lock: Mutex<()>,
    cv: Condvar,
    /// the machine wants every core parked
    stop: AtomicBool,
    quit: AtomicBool,
    /// cores executing right now
    running: AtomicUsize,
    /// published interrupt inputs per core: bit 0 IRQ
    lines: Vec<AtomicU8>,
    state: Vec<AtomicU8>,
    /// queued invalidations per core: (TLB?, operand)
    inval: Mutex<Vec<Vec<(bool, u64)>>>,
    has_inval: Vec<AtomicBool>,
    /// the first stop a core ran into (reset, power off, unimplemented, ...)
    event: Mutex<Option<Stop>>,
    soc: AtomicPtr<Soc>,
    cpus: AtomicPtr<Cpu>,
    ncpu: usize,
    t0: Instant,
    /// guest time = host time since t0 + offset (ns)
    offset: AtomicI64,
    exceptions: AtomicU64,
    irqs: AtomicU64,
    /// invalidation broadcasts so far, and the last one each core has applied
    inval_seq: AtomicU64,
    seen: Vec<AtomicU64>,
    /// each core's `chain_until` (other threads store 0 to kick it out of a compiled chain)
    kick: Vec<AtomicPtr<u64>>,
    /// core k is executing (in `run_core`)
    active: Vec<AtomicBool>,
}

// SAFETY: the raw pointers are only dereferenced while the machine is in `run` (it outlives the
// run phase and does not move); device state is only touched under `lock`.
unsafe impl Send for Shared {}
unsafe impl Sync for Shared {}

impl Shared {
    fn now(&self) -> u64 { (self.t0.elapsed().as_nanos() as i64 + self.offset.load(Relaxed)).max(0) as u64 }
    fn guard(&self) -> MutexGuard<'_, ()> { self.lock.lock().unwrap_or_else(|e| e.into_inner()) }
    /// SAFETY: caller holds `lock` and the machine is in its run phase.
    #[allow(clippy::mut_from_ref)]
    unsafe fn soc(&self) -> &mut Soc { &mut *self.soc.load(Relaxed) }
    /// SAFETY: core `k`'s own thread, or the lock with core `k` off; run phase.
    #[allow(clippy::mut_from_ref)]
    unsafe fn cpu(&self, k: usize) -> &mut Cpu { &mut *self.cpus.load(Relaxed).add(k) }

    /// Re-derive every core's interrupt input from the GIC (lock held); wake sleepers.
    fn publish(&self, soc: &Soc) {
        let mut woke = false;
        for (j, l) in self.lines.iter().enumerate() {
            let v = soc.gic.irq_out(j) as u8;
            if l.swap(v, SeqCst) != v && v != 0 { woke = true; }
        }
        if woke { self.cv.notify_all(); }
    }

    fn report(&self, s: Stop) {
        let mut e = self.event.lock().unwrap_or_else(|e| e.into_inner());
        if e.is_none() { *e = Some(s); }
        self.stop.store(true, SeqCst);
        self.cv.notify_all();
    }

    /// TLBI or IC from core `from`: the other cores apply it before `from` goes on (as the
    /// guest's DSB guarantees). Cores that are not executing apply it before they next run.
    fn broadcast(&self, from: usize, tlb: bool, op: u64, cpu: &mut Cpu) {
        let seq = {
            let mut q = self.inval.lock().unwrap_or_else(|e| e.into_inner());
            for (j, v) in q.iter_mut().enumerate() {
                if j != from { v.push((tlb, op)); self.has_inval[j].store(true, SeqCst); }
            }
            self.inval_seq.fetch_add(1, SeqCst) + 1
        };
        loop {
            let mut pending = false;
            for j in 0..self.ncpu {
                if j == from || !self.active[j].load(SeqCst) || self.seen[j].load(SeqCst) >= seq { continue; }
                pending = true;
                // SAFETY: points at core j's chain_until while it is active; an atomic store
                let p = self.kick[j].load(SeqCst);
                if !p.is_null() { unsafe { AtomicU64::from_ptr(p).store(0, SeqCst) }; }
            }
            if !pending || self.stop.load(SeqCst) { break; }
            // meanwhile take the others' invalidations (two cores may be waiting for each other)
            self.apply(from, cpu);
            std::hint::spin_loop();
        }
    }

    /// Apply core `k`'s queued invalidations and acknowledge them.
    fn apply(&self, k: usize, cpu: &mut Cpu) {
        if !self.has_inval[k].load(SeqCst) { return; }
        let (ops, seq) = {
            let mut q = self.inval.lock().unwrap_or_else(|e| e.into_inner());
            self.has_inval[k].store(false, SeqCst);
            (std::mem::take(&mut q[k]), self.inval_seq.load(SeqCst))
        };
        for (tlb, op) in ops { if tlb { cpu.tlb_invalidate(op) } else { cpu.jit_invalidate(op) } }
        self.seen[k].store(seq, SeqCst);
    }
}

/// The bus a core thread sees: RAM directly, devices under the lock.
struct MtBus<'a> { sh: &'a Shared, k: usize, ram: *mut u8, ram_len: u64 }

impl MtBus<'_> {
    #[inline]
    fn ram(&self, a: u64, n: u64) -> Option<*mut u8> {
        // SAFETY: bounds checked against the RAM buffer
        (a.checked_add(n).is_some_and(|e| e <= self.ram_len)).then(|| unsafe { self.ram.add(a as usize) })
    }
    fn dev<R>(&mut self, f: impl FnOnce(&mut Soc) -> R) -> R {
        let _g = self.sh.guard();
        // SAFETY: lock held, run phase
        let soc = unsafe { self.sh.soc() };
        soc.cur = self.k;
        let r = f(soc);
        if soc.dirty { soc.device_lines(self.sh.now()); self.sh.publish(soc); }
        r
    }
}

macro_rules! mt_rw {
    ($r:ident, $w:ident, $t:ty) => {
        fn $r(&mut self, a: u64) -> Result<$t, Fault> {
            // SAFETY: inside RAM (other cores may write it concurrently, as on hardware)
            if let Some(p) = self.ram(a, std::mem::size_of::<$t>() as u64) { return Ok(unsafe { (p as *const $t).read_unaligned() }); }
            self.dev(|s| s.$r(a))
        }
        fn $w(&mut self, a: u64, v: $t) -> Result<(), Fault> {
            if let Some(p) = self.ram(a, std::mem::size_of::<$t>() as u64) { unsafe { (p as *mut $t).write_unaligned(v) }; return Ok(()); }
            self.dev(|s| s.$w(a, v))
        }
    };
}

impl Bus for MtBus<'_> {
    mt_rw!(read8, write8, u8);
    mt_rw!(read16, write16, u16);
    mt_rw!(read32, write32, u32);
    mt_rw!(read64, write64, u64);
}

pub(crate) struct Mt {
    sh: Arc<Shared>,
    threads: Vec<std::thread::JoinHandle<()>>,
}

impl Drop for Mt {
    fn drop(&mut self) {
        {
            let _g = self.sh.guard();
            self.sh.quit.store(true, SeqCst);
            self.sh.cv.notify_all();
        }
        for t in self.threads.drain(..) { let _ = t.join(); }
    }
}

impl Mt {
    pub(crate) fn new(ncpu: usize) -> Mt {
        let sh = Arc::new(Shared {
            lock: Mutex::new(()), cv: Condvar::new(), stop: AtomicBool::new(true), quit: AtomicBool::new(false), running: AtomicUsize::new(0),
            lines: (0..ncpu).map(|_| AtomicU8::new(0)).collect(), state: (0..ncpu).map(|_| AtomicU8::new(OFF)).collect(),
            inval: Mutex::new(vec![Vec::new(); ncpu]), has_inval: (0..ncpu).map(|_| AtomicBool::new(false)).collect(),
            event: Mutex::new(None), soc: AtomicPtr::new(std::ptr::null_mut()), cpus: AtomicPtr::new(std::ptr::null_mut()),
            ncpu, t0: Instant::now(), offset: AtomicI64::new(0), exceptions: AtomicU64::new(0), irqs: AtomicU64::new(0),
            inval_seq: AtomicU64::new(0), seen: (0..ncpu).map(|_| AtomicU64::new(0)).collect(),
            kick: (0..ncpu).map(|_| AtomicPtr::new(std::ptr::null_mut())).collect(), active: (0..ncpu).map(|_| AtomicBool::new(false)).collect(),
        });
        let threads = (0..ncpu).map(|k| {
            let sh = sh.clone();
            std::thread::Builder::new().name(format!("cpu{k}")).spawn(move || worker(&sh, k)).expect("core thread")
        }).collect();
        Mt { sh, threads }
    }
}

/// Wait for work; run the core while it is runnable and the machine is not stopping.
fn worker(sh: &Shared, k: usize) {
    let mut g = sh.guard();
    // the timer deadline a sleeping core was last woken for: a deadline whose interrupt is not
    // delivered (disabled at the GIC) wakes the core once, not in a loop
    let mut woke_for: Option<u64> = None;
    loop {
        if sh.quit.load(SeqCst) { return; }
        if !sh.stop.load(SeqCst) {
            let st = sh.state[k].load(SeqCst);
            // SAFETY: run phase; this is core k's own thread
            let dl = if st == WFI { unsafe { deadline(sh.cpu(k)) } } else { None };
            let due = sh.lines[k].load(SeqCst) == 0 && dl.is_some_and(|d| d <= sh.now() && woke_for != Some(d));
            if due { woke_for = dl; }
            if st == RUN || st == WFE || (st == WFI && (sh.lines[k].load(SeqCst) != 0 || due)) {
                sh.state[k].store(RUN, SeqCst);
                sh.running.fetch_add(1, SeqCst);
                drop(g);
                run_core(sh, k);
                g = sh.guard();
                sh.running.fetch_sub(1, SeqCst);
                sh.cv.notify_all();
                continue;
            }
        }
        // asleep (WFI, off) or parked: an interrupt, a CPU_ON, the run phase or the core's own
        // timer wakes it (checked again at least every 2 ms)
        let wait = if sh.stop.load(SeqCst) || sh.state[k].load(SeqCst) == OFF { Duration::from_millis(50) } else {
            // SAFETY: run phase, own core
            let cpu = unsafe { sh.cpu(k) };
            if deadline(cpu).is_some_and(|d| woke_for == Some(d)) { Duration::from_millis(2) } else { timer_wait(cpu, sh.now()) }
        };
        g = sh.cv.wait_timeout(g, wait).unwrap_or_else(|e| e.into_inner()).0;
    }
}

fn deadline(cpu: &Cpu) -> Option<u64> {
    let s = &cpu.sys;
    [(s.vtimer, s.cntvoff), (s.ptimer, 0)].iter().filter(|(t, _)| t.ctl & 3 == 1)
        .map(|(t, off)| { let c = t.cval.wrapping_add(*off); (c / 27 * 500) + (c % 27 * 500).div_ceil(27) }).min()
}
fn timer_wait(cpu: &Cpu, now: u64) -> Duration {
    Duration::from_nanos(deadline(cpu).map_or(2_000_000, |d| d.saturating_sub(now)).clamp(10_000, 2_000_000))
}

/// The core's clock catches up with the wall clock; its timer outputs go to the GIC.
fn sync_time(sh: &Shared, cpu: &mut Cpu, k: usize, last: &mut Option<(bool, bool)>) {
    cpu.refresh_counter();
    cpu.sys.timers_changed = false;
    let cnt = cpu.sys.cnt;
    let lv = (cpu.sys.vtimer.firing(cnt.wrapping_sub(cpu.sys.cntvoff)), cpu.sys.ptimer.firing(cnt));
    if *last != Some(lv) {
        *last = Some(lv);
        let _g = sh.guard();
        // SAFETY: lock held, run phase
        let soc = unsafe { sh.soc() };
        soc.gic.set_ppi(k, PPI_VTIMER, lv.0);
        soc.gic.set_ppi(k, PPI_PTIMER, lv.1);
        sh.publish(soc);
    }
}

/// Marks core k inactive when `run_core` returns (its invalidations wait for its next run).
struct Leave<'a>(&'a Shared, usize);
impl Drop for Leave<'_> {
    fn drop(&mut self) {
        self.0.active[self.1].store(false, SeqCst);
        self.0.kick[self.1].store(std::ptr::null_mut(), SeqCst);
    }
}

fn run_core(sh: &Shared, k: usize) {
    // SAFETY: core k's own thread during the run phase
    let cpu = unsafe { sh.cpu(k) };
    let mut bus = MtBus { sh, k, ram: cpu.ram, ram_len: cpu.ram_len };
    let mut last_lv = None;
    let mut next_sync = 0;
    let mut storm = 0u32;
    sh.kick[k].store(cpu.chain_until_ptr(), SeqCst);
    sh.active[k].store(true, SeqCst);
    let _leave = Leave(sh, k);
    // anything queued while this core was not executing is applied before it runs again
    sh.apply(k, cpu);
    loop {
        if sh.stop.load(Relaxed) { return; }
        sh.apply(k, cpu);
        if cpu.instret >= next_sync || cpu.sys.timers_changed {
            sync_time(sh, cpu, k, &mut last_lv);
            next_sync = cpu.instret + SLICE;
            if let Some(d) = deadline(cpu) { let t = cpu.now_ns(); if d > t { next_sync = next_sync.min(cpu.instret + (d - t)); } }
        }
        cpu.set_irq(sh.lines[k].load(Relaxed) & 1 != 0, false);
        // SAFETY: an atomic store to this core's own field (other cores may store 0 to it)
        unsafe { AtomicU64::from_ptr(cpu.chain_until_ptr()).store(next_sync, SeqCst) };
        // a kick that came in before this point is answered at the top of the next round
        if sh.has_inval[k].load(SeqCst) { continue; }
        let r = cpu.step(&mut bus);
        if let Some(op) = cpu.tlbi_bcast.take() { sh.broadcast(k, true, op, cpu); }
        if let Some(op) = cpu.ic_bcast.take() { sh.broadcast(k, false, op, cpu); }
        match r {
            Ok(()) => storm = 0,
            Err(Trap::Exception(_, el)) => {
                sh.exceptions.fetch_add(1, Relaxed);
                storm += 1;
                if storm >= 1_000_000 {
                    let e = el as usize;
                    sh.report(Stop::ExceptionStorm { pc: cpu.pc, esr: cpu.esr[e], elr: cpu.elr[e], far: cpu.far[e], el });
                    return;
                }
            }
            Err(Trap::Interrupt(..)) => { sh.irqs.fetch_add(1, Relaxed); }
            Err(Trap::Wait) => {
                sync_time(sh, cpu, k, &mut last_lv);
                next_sync = cpu.instret + SLICE;
                if sh.lines[k].load(SeqCst) & 1 != 0 { continue; }
                if cpu.last_wfe { std::thread::yield_now(); continue; } // waiting for another core
                let _g = sh.guard();
                if sh.lines[k].load(SeqCst) & 1 == 0 { sh.state[k].store(WFI, SeqCst); return; }
            }
            Err(Trap::HostCall(_)) => {
                if let Some(stop) = psci(sh, k) { sh.report(stop); return; }
                if sh.state[k].load(SeqCst) == OFF { return; }
            }
            Err(Trap::Unimplemented(pc, i)) => { sh.report(Stop::Unimplemented(pc, i)); return; }
            Err(Trap::Hlt(n)) => { sh.report(Stop::Hlt(n)); return; }
            Err(Trap::Brk(n)) => { sh.report(Stop::Brk(n)); return; }
        }
    }
}

/// PSCI from core `k` (see `Machine::psci`; this is the same firmware for parallel cores).
fn psci(sh: &Shared, k: usize) -> Option<Stop> {
    const NOT_SUPPORTED: u64 = -1i64 as u64;
    const INVALID_PARAMETERS: u64 = -2i64 as u64;
    const ALREADY_ON: u64 = -4i64 as u64;
    let _g = sh.guard();
    // SAFETY: own core; other cores' structs only while they are off, under the lock
    let cpu = unsafe { sh.cpu(k) };
    let x = cpu.x;
    let target = { let t = (x[1] & 0xff_00ff_ffff) as usize; (t < sh.ncpu).then_some(t) };
    let r = match x[0] as u32 {
        0x8400_0000 => 0x0001_0001,
        0x8400_0002 => { sh.state[k].store(OFF, SeqCst); cpu.set_irq(false, false); return None; }
        0x8400_0003 | 0xc400_0003 => match target {
            None => INVALID_PARAMETERS,
            Some(t) if sh.state[t].load(SeqCst) != OFF => ALREADY_ON,
            Some(t) => {
                let (el, now) = (cpu.el, cpu.now_ns());
                let tc = unsafe { sh.cpu(t) };
                Machine::reset_core(tc, t, el, x[2], x[3], now);
                tc.jit_off = cpu.jit_off;
                tc.crypto = cpu.crypto;
                tc.mt = true;
                if tc.jit.is_none() && cpu.jit.is_some() { tc.enable_jit(); }
                sh.state[t].store(RUN, SeqCst);
                sh.cv.notify_all();
                0
            }
        },
        0x8400_0004 | 0xc400_0004 => match target { None => INVALID_PARAMETERS, Some(t) => (sh.state[t].load(SeqCst) == OFF) as u64 },
        0x8400_0006 => 2,
        0x8400_0008 => return Some(Stop::PowerOff),
        0x8400_0009 => return Some(Stop::Reset),
        0x8400_000a => match x[1] as u32 {
            0x8400_0000 | 0x8400_0002 | 0x8400_0003 | 0xc400_0003 | 0x8400_0004 | 0xc400_0004 | 0x8400_0006 | 0x8400_0008 | 0x8400_0009 | 0x8400_000a => 0,
            _ => NOT_SUPPORTED,
        },
        _ => NOT_SUPPORTED,
    };
    cpu.x[0] = r;
    None
}

impl Machine {
    /// Run the cores in parallel for about `wall` of host time (see the module documentation).
    pub(crate) fn run_threaded(&mut self, wall: Duration) -> Stop {
        let n = self.cpus.len();
        if self.mt.is_none() { self.mt = Some(Mt::new(n)); }
        // every core gets its own JIT and parallel memory semantics
        let jit = self.cpus.iter().any(|c| c.jit.is_some());
        for c in self.cpus.iter_mut() {
            c.mt = true;
            if jit && c.jit.is_none() { c.enable_jit(); }
        }
        let sh = self.mt.as_ref().unwrap().sh.clone();
        {
            let _g = sh.guard();
            sh.soc.store(&mut self.soc as *mut Soc, SeqCst);
            sh.cpus.store(self.cpus.as_mut_ptr(), SeqCst);
            let off = self.now as i64 - sh.t0.elapsed().as_nanos() as i64;
            sh.offset.store(off, SeqCst);
            for c in self.cpus.iter_mut() { c.wall_clock = Some((sh.t0, off)); }
            for k in 0..n { sh.state[k].store(enc(self.state[k]), SeqCst); }
            *sh.event.lock().unwrap_or_else(|e| e.into_inner()) = None;
            self.soc.device_lines(self.now);
            sh.publish(&self.soc);
            sh.stop.store(false, SeqCst);
            sh.cv.notify_all();
        }
        let insns0 = self.instret();
        // the I/O side: device time, network, USB frames, the watchdog
        let end = Instant::now() + wall;
        let stop = loop {
            std::thread::sleep(Duration::from_micros(500));
            let _g = sh.guard();
            let now = sh.now();
            // SAFETY: lock held; the cores only reach the SoC under it
            let soc = unsafe { sh.soc() };
            soc.pump_network(now);
            soc.usb.tick(now);
            soc.device_lines(now);
            sh.publish(soc);
            if let Some(halt) = soc.pm.expired() { break if halt { Stop::PowerOff } else { Stop::Reset }; }
            if let Some(s) = sh.event.lock().unwrap_or_else(|e| e.into_inner()).take() { break s; }
            if Instant::now() >= end { break Stop::Budget; }
        };
        // park the cores
        {
            let _g = sh.guard();
            sh.stop.store(true, SeqCst);
            sh.cv.notify_all();
        }
        while sh.running.load(SeqCst) != 0 { std::thread::yield_now(); }
        let _g = sh.guard();
        self.now = self.now.max(sh.now());
        self.exceptions += sh.exceptions.swap(0, Relaxed);
        self.irqs += sh.irqs.swap(0, Relaxed);
        for k in 0..n {
            // back to instruction-counted time, continuing from here
            let c = &mut self.cpus[k];
            c.wall_clock = None;
            c.sys.time_base_ns = self.now;
            c.sys.time_base_instret = c.instret;
            self.state[k] = dec(sh.state[k].load(SeqCst));
            self.cpus[k].set_irq(sh.lines[k].load(SeqCst) & 1 != 0, false);
        }
        // a slice without a burst of work counts as idle (for the caller's pacing and
        // `wait-idle`): the same 2 M instructions a one-thread `run` call takes when busy
        let stop = match stop {
            Stop::Budget if self.instret() - insns0 < 2_000_000 => Stop::Idle(self.now + 1_000_000),
            s => s,
        };
        // queued invalidations land now
        let mut q = sh.inval.lock().unwrap_or_else(|e| e.into_inner());
        for (k, v) in q.iter_mut().enumerate() {
            for (tlb, op) in v.drain(..) { if tlb { self.cpus[k].tlb_invalidate(op) } else { self.cpus[k].jit_invalidate(op) } }
            sh.has_inval[k].store(false, SeqCst);
        }
        stop
    }
}
