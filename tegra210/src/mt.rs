//! Multi-threaded execution for the Tegra X1 (`--threads multi`, as QEMU's MTTCG), adapted from
//! the Raspberry Pi 4's `bcm2711/src/mt.rs` (and the RK3588S's `rk3588/src/mt.rs`): each
//! Cortex-A57 runs on its own host thread against the shared guest RAM, so busy cores really
//! run in parallel. Runs are no longer repeatable: interleavings depend on the host.
//!
//! - Devices (the SoC, the GPU among them) are behind one lock; RAM is not (cores read and write
//!   it directly, with host fences for barriers and acquire/release, and store-exclusive as
//!   compare-and-swap). A GPU submission runs on the thread of the core that wrote GP_PUT and
//!   holds the lock meanwhile: the other cores go on until they touch a device.
//! - Time follows the host clock: each core moves its virtual clock up to the wall clock every
//!   few thousand instructions; a core in WFI sleeps until an interrupt, its arch timer or the
//!   next device event is due (Linux ticks on the Tegra timers, TMR10-13), and a busy core
//!   brings the devices up to the time when one is due. The GPU's time model needs no change:
//!   its work takes wall time, which is guest time here, all of it (one thread caps a
//!   submission at a second).
//! - Interrupt lines are published per core as atomics after every device access.
//! - TLB and instruction-cache invalidations are queued for the other cores; each core has its
//!   own JIT.
//! - An I/O loop on the machine's thread brings the devices (timers, display, USB, network,
//!   GPU semaphores) up to the wall clock every half millisecond.
//! - A panic on a core thread stops the machine and is raised again on the machine's thread.
//!
//! The machine still owns the cores and the SoC: `Machine::run` lets the threads run for a
//! slice of wall time and parks them again before it returns, so everything outside `run` sees
//! a stopped machine.
use crate::{CoreState, Machine, Soc, CNTFRQ, NCPU, RAM_BASE};
use arm_a64::Cpu;
use emu_core::{Bus, Core, Fault, Stop, Trap};
use std::sync::atomic::{AtomicBool, AtomicI64, AtomicPtr, AtomicU64, AtomicU8, AtomicUsize, Ordering::{Relaxed, SeqCst}};
use std::sync::{Arc, Condvar, Mutex, MutexGuard};
use std::time::{Duration, Instant};

const OFF: u8 = 0;
const RUN: u8 = 1;
const WFI: u8 = 2;
const WFE: u8 = 3;
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
    /// queued invalidations per core, each behind its own lock: (TLB?, operand)
    inval: Vec<Mutex<Vec<(bool, u64)>>>,
    has_inval: Vec<AtomicBool>,
    /// per core: invalidations queued for it so far (a broadcaster's ticket) and applied so far
    queued: Vec<AtomicU64>,
    applied: Vec<AtomicU64>,
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
    /// each core's `chain_until` (other threads store 0 to kick it out of a compiled chain)
    kick: Vec<AtomicPtr<u64>>,
    /// core k is executing (in `run_core`)
    active: Vec<AtomicBool>,
    /// the next device event (`Soc::next_event`, ns; u64::MAX: none): the Tegra timers that
    /// Linux ticks on (TMR10-13, one a core), display, USB, network and GPU retries
    dev_next: AtomicU64,
    /// a core thread panicked: the payload, raised again on the machine's thread
    panic: Mutex<Option<Box<dyn std::any::Any + Send>>>,
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
            let v = soc.gic.irq_out_for(j) as u8;
            if l.swap(v, SeqCst) != v && v != 0 { woke = true; }
        }
        if woke { self.cv.notify_all(); }
    }

    /// The devices at the wall clock, their interrupts to the GIC and the cores (lock held).
    fn service(&self, soc: &mut Soc) {
        soc.service(self.now());
        soc.dirty = false;
        self.publish(soc);
        // sleeping cores wait for the device events too: one that comes sooner wakes them to
        // wait again
        let next = soc.next_event().unwrap_or(u64::MAX);
        if self.dev_next.swap(next, SeqCst) > next { self.cv.notify_all(); }
    }

    /// The devices, if an event is due (lock held).
    fn service_due(&self, soc: &mut Soc) {
        if self.dev_next.load(SeqCst) <= self.now() { self.service(soc); }
    }

    fn report(&self, s: Stop) {
        let mut e = self.event.lock().unwrap_or_else(|e| e.into_inner());
        if e.is_none() { *e = Some(s); }
        self.stop.store(true, SeqCst);
        self.cv.notify_all();
    }

    /// TLBI or IC from core `from`, for the other cores. A TLBI is applied by every executing
    /// core before `from` goes on (as the guest's DSB guarantees); an IC invalidation when each
    /// next leaves its compiled chain. Cores that are not executing apply both before they run.
    fn broadcast(&self, from: usize, tlb: bool, op: u64, cpu: &mut Cpu) {
        // a ticket per target: its queue position, taken under that core's lock
        let mut tickets = [0u64; NCPU];
        for j in 0..self.ncpu {
            if j == from { continue; }
            let mut q = self.inval[j].lock().unwrap_or_else(|e| e.into_inner());
            q.push((tlb, op));
            tickets[j] = self.queued[j].fetch_add(1, SeqCst) + 1;
            self.has_inval[j].store(true, SeqCst);
        }
        // instruction cache: another core needs the new code only after its own context
        // synchronization (an ISB, an exception entry or return). It is kicked out of its
        // compiled chain and applies the queue at the next block boundary; nobody waits for
        // that. Only TLBI waits (the guest's DSB ISH).
        if !tlb {
            for j in 0..self.ncpu {
                if j == from || !self.active[j].load(SeqCst) { continue; }
                // SAFETY: as below
                let p = self.kick[j].load(SeqCst);
                if !p.is_null() { unsafe { AtomicU64::from_ptr(p).store(0, SeqCst) }; }
            }
            return;
        }
        loop {
            let mut pending = false;
            for j in 0..self.ncpu {
                if j == from || !self.active[j].load(SeqCst) || self.applied[j].load(SeqCst) >= tickets[j] { continue; }
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
        let ops = {
            let mut q = self.inval[k].lock().unwrap_or_else(|e| e.into_inner());
            self.has_inval[k].store(false, SeqCst);
            std::mem::take(&mut *q)
        };
        for &(tlb, op) in &ops { if tlb { cpu.tlb_invalidate(op) } else { cpu.jit_invalidate(op) } }
        // queue order = ticket order, so the count applied covers every ticket up to it
        self.applied[k].fetch_add(ops.len() as u64, SeqCst);
    }
}

/// The bus a core thread sees: RAM (from `RAM_BASE`) directly, devices under the lock.
struct MtBus<'a> { sh: &'a Shared, k: usize, ram: *mut u8, ram_len: u64 }

impl MtBus<'_> {
    #[inline]
    fn ram(&self, a: u64, n: u64) -> Option<*mut u8> {
        let o = a.wrapping_sub(RAM_BASE);
        // SAFETY: bounds checked against the RAM buffer
        (o.checked_add(n).is_some_and(|e| e <= self.ram_len)).then(|| unsafe { self.ram.add(o as usize) })
    }
    fn dev<R>(&mut self, f: impl FnOnce(&mut Soc) -> R) -> R {
        let _g = self.sh.guard();
        // SAFETY: lock held, run phase
        let soc = unsafe { self.sh.soc() };
        // the GIC's banked registers are this core's; device registers read the time now
        soc.gic.current = self.k;
        soc.now = soc.now.max(self.sh.now());
        let r = f(soc);
        if soc.dirty { self.sh.service(soc); }
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
            inval: (0..ncpu).map(|_| Mutex::new(Vec::new())).collect(), has_inval: (0..ncpu).map(|_| AtomicBool::new(false)).collect(),
            queued: (0..ncpu).map(|_| AtomicU64::new(0)).collect(), applied: (0..ncpu).map(|_| AtomicU64::new(0)).collect(),
            event: Mutex::new(None), soc: AtomicPtr::new(std::ptr::null_mut()), cpus: AtomicPtr::new(std::ptr::null_mut()),
            ncpu, t0: Instant::now(), offset: AtomicI64::new(0), exceptions: AtomicU64::new(0), irqs: AtomicU64::new(0),
            kick: (0..ncpu).map(|_| AtomicPtr::new(std::ptr::null_mut())).collect(), active: (0..ncpu).map(|_| AtomicBool::new(false)).collect(),
            dev_next: AtomicU64::new(u64::MAX), panic: Mutex::new(None),
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
                // a panic (a device model's assertion, say) stops the machine and is raised
                // again on its thread, as with one thread; `running` stays right meanwhile
                if let Err(p) = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| run_core(sh, k))) {
                    let mut slot = sh.panic.lock().unwrap_or_else(|e| e.into_inner());
                    if slot.is_none() { *slot = Some(p); }
                    drop(slot);
                    sh.stop.store(true, SeqCst);
                }
                g = sh.guard();
                sh.running.fetch_sub(1, SeqCst);
                sh.cv.notify_all();
                continue;
            }
        }
        // asleep (WFI, off) or parked: an interrupt, a CPU_ON, the run phase, the core's own
        // timer or a device event wakes it (checked again at least every 2 ms)
        let wait = if sh.stop.load(SeqCst) || sh.state[k].load(SeqCst) == OFF { Duration::from_millis(50) } else {
            // a device event that is due (the Tegra timer this core ticks on) raises its
            // interrupt now, not at the I/O loop's next round
            // SAFETY: lock held, run phase
            sh.service_due(unsafe { sh.soc() });
            if sh.lines[k].load(SeqCst) != 0 { continue; }
            // SAFETY: run phase, own core
            let cpu = unsafe { sh.cpu(k) };
            let now = sh.now();
            let arch = if deadline(cpu).is_some_and(|d| woke_for == Some(d)) { Duration::from_millis(2) } else { timer_wait(cpu, now) };
            arch.min(Duration::from_nanos(sh.dev_next.load(SeqCst).saturating_sub(now).max(10_000)))
        };
        g = sh.cv.wait_timeout(g, wait).unwrap_or_else(|e| e.into_inner()).0;
    }
}

fn deadline(cpu: &Cpu) -> Option<u64> {
    let s = &cpu.sys;
    [(s.vtimer, s.cntvoff), (s.ptimer, 0)].iter().filter(|(t, _)| t.ctl & 3 == 1)
        .map(|(t, off)| ((t.cval.wrapping_add(*off) as u128 * 1_000_000_000).div_ceil(CNTFRQ as u128)).min(u64::MAX as u128) as u64).min()
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
        // as `Machine::update`: the physical timer drives the secure and non-secure PPIs
        soc.gic.set_ppi(k, 27, lv.0);
        soc.gic.set_ppi(k, 29, lv.1);
        soc.gic.set_ppi(k, 30, lv.1);
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
            let t = cpu.now_ns();
            if let Some(d) = deadline(cpu) { if d > t { next_sync = next_sync.min(cpu.instret + (d - t)); } }
            // a device event that is due: the busy core brings the devices up to now itself
            let dn = sh.dev_next.load(Relaxed);
            if dn <= t {
                let _g = sh.guard();
                // SAFETY: lock held, run phase
                sh.service_due(unsafe { sh.soc() });
            } else {
                next_sync = next_sync.min(cpu.instret + (dn - t));
            }
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
                // WFE: another core holds what this one waits for; it looks again soon (a WFE
                // may end early)
                if cpu.last_wfe { std::thread::yield_now(); continue; }
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

/// PSCI and NVIDIA's SiP calls from core `k`: the same firmware as `Machine::psci`, for
/// parallel cores.
fn psci(sh: &Shared, k: usize) -> Option<Stop> {
    const NOT_SUPPORTED: u64 = -1i64 as u64;
    const INVALID_PARAMETERS: u64 = -2i64 as u64;
    const ALREADY_ON: u64 = -4i64 as u64;
    let _g = sh.guard();
    // SAFETY: own core; other cores' structs only while they are off, under the lock
    let cpu = unsafe { sh.cpu(k) };
    let x = cpu.x;
    let target = { let a = x[1] & 0xff_00ff_ffff; (0..sh.ncpu).find(|&t| crate::core_mpidr(t) & 0xff_00ff_ffff == a) };
    let r = match x[0] as u32 {
        0x8400_0002 => { sh.state[k].store(OFF, SeqCst); cpu.set_irq(false, false); return None; }
        0x8400_0003 | 0xc400_0003 => match target {
            None => INVALID_PARAMETERS,
            Some(t) if sh.state[t].load(SeqCst) != OFF => ALREADY_ON,
            Some(t) => {
                let (el, now) = (cpu.el, cpu.now_ns());
                // SAFETY: core t is off (its thread is not running it), lock held
                let tc = unsafe { sh.cpu(t) };
                Machine::reset_core(tc, t, el, x[2], x[3], now);
                tc.jit_off = cpu.jit_off;
                if tc.jit.is_none() && cpu.jit.is_some() { tc.enable_jit(); }
                // SAFETY: lock held
                let soc = unsafe { sh.soc() };
                soc.gic.reset_cpu_interface(t);
                sh.publish(soc);
                sh.state[t].store(RUN, SeqCst);
                sh.cv.notify_all();
                0
            }
        },
        0x8400_0004 | 0xc400_0004 => match target { None => INVALID_PARAMETERS, Some(t) => (sh.state[t].load(SeqCst) == OFF) as u64 },
        0x8400_0008 => return Some(Stop::PowerOff),
        0x8400_0009 => return Some(Stop::Reset),
        0xc2ff_fe00 => {
            // SAFETY: lock held
            let soc = unsafe { sh.soc() };
            match x[1] {
                0xaa => { cpu.x[1] = u64::from(soc.pmc.read(x[2])); 0 }
                0xbb => { soc.pmc.write(x[2], x[3] as u32); 0 }
                _ => NOT_SUPPORTED,
            }
        }
        f => Machine::psci_simple(f, x[1]),
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
            if jit && c.jit.is_none() && !c.jit_off { c.enable_jit(); }
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
            sh.service(&mut self.soc);
            sh.stop.store(false, SeqCst);
            sh.cv.notify_all();
        }
        let insns0 = self.instret();
        // the I/O side: device time (timers, display, GPU semaphores), network, USB frames
        let end = Instant::now() + wall;
        let stop = loop {
            std::thread::sleep(Duration::from_micros(500));
            let _g = sh.guard();
            // SAFETY: lock held; the cores only reach the SoC under it
            sh.service(unsafe { sh.soc() });
            if let Some(s) = sh.event.lock().unwrap_or_else(|e| e.into_inner()).take() { break s; }
            if Instant::now() >= end || sh.stop.load(SeqCst) { break Stop::Budget; }
        };
        // park the cores
        {
            let _g = sh.guard();
            sh.stop.store(true, SeqCst);
            sh.cv.notify_all();
        }
        while sh.running.load(SeqCst) != 0 { std::thread::yield_now(); }
        if let Some(p) = sh.panic.lock().unwrap_or_else(|e| e.into_inner()).take() { std::panic::resume_unwind(p); }
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
        self.soc.gic.current = self.cur;
        self.next_check = 0;
        // a slice without a burst of work counts as idle (for the caller's pacing and
        // `wait-idle`): the same 2 M instructions a one-thread `run` call takes when busy
        let stop = match stop {
            Stop::Budget if self.instret() - insns0 < 2_000_000 => Stop::Idle(self.now + 1_000_000),
            s => s,
        };
        // queued invalidations land now
        for k in 0..n {
            let mut q = sh.inval[k].lock().unwrap_or_else(|e| e.into_inner());
            let ops = std::mem::take(&mut *q);
            for &(tlb, op) in &ops { if tlb { self.cpus[k].tlb_invalidate(op) } else { self.cpus[k].jit_invalidate(op) } }
            sh.applied[k].fetch_add(ops.len() as u64, SeqCst);
            sh.has_inval[k].store(false, SeqCst);
        }
        stop
    }
}
