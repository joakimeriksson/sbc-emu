//! The GPU, a Maxwell GM20B (`nvidia,gm20b`), as nouveau drives it. Modelled at the level of its
//! registers: the interrupt controller, timer, privileged ring, falcons (PMU, FECS and GPCCS run
//! no firmware: their side of the protocols is answered), the memory management unit, and the host
//! (channels, run lists, push buffers). Other registers read back.
pub mod copy;
pub mod mmu;
pub mod pmu;

use crate::smmu::Smmu;
use mmu::{Mmu, Space};
use std::collections::HashMap;

pub const BAR0: u64 = 0x5700_0000;
pub const BAR1: u64 = 0x5800_0000;
pub const SIZE: u64 = 0x100_0000;
pub const IRQ: usize = 32 + 0x9d;

/// GM20B revision A1.
const BOOT0: u32 = 0x12b0_00a1;
const CHANNELS: u32 = 512;
/// How often a semaphore the CPU may write is looked at (nanoseconds).
const RETRY_NS: u64 = 100_000;
const MAX_RETRY_NS: u64 = 2_000_000;

// PMC interrupt bits
const INTR_FIFO: u32 = 1 << 8;
const INTR_TIMER: u32 = 1 << 20;
const INTR_PMU: u32 = 1 << 24;
// PFIFO interrupt bits
const FIFO_RUNLIST: u32 = 1 << 30;
const FIFO_NONSTALL: u32 = 1 << 31;

/// The engine table (`0x022700`): graphics (runlist 0, engine 0) and copy engine 2 (engine 1).
const TOP: [u32; 6] = [
    1 << 31 | 12 << 15 | 12 << 9 | 0x3c | 2,
    1 << 31 | 1 << 2 | 1,
    3,
    1 << 31 | 1 << 26 | 7 << 15 | 6 << 9 | 0x3c | 2,
    1 << 31 | 0x1b << 3 | 1 << 2 | 1,
    3 << 2 | 3,
];

#[derive(Clone, Copy, Default, Debug)]
struct Channel {
    /// the instance block's register value (bit 31: bound)
    inst: u32,
    enabled: bool,
    /// the instance block of the last bind (unbinding writes 0; binding the same block again
    /// is the same channel)
    bound: u32,
}

pub struct Gpu {
    regs: HashMap<u32, u32>,
    pub pmu: pmu::Pmu,
    pub mmu: Mmu,
    channels: Vec<Channel>,
    /// channels on the run list, in order
    runlist: Vec<u32>,
    pub host: host::Host,
    fifo_intr: u32,
    runlist_intr: u32,
    timer_intr: bool,
    now: u64,
    /// the last time read (the timer never stands still, whatever the CPU's blocks do)
    last_time: u64,
    pub trace: bool,
    /// registers touched that have no model, for reports
    pub submissions: u64,
    /// host1x sync points the channels incremented, for the SoC to apply
    pub syncpts: Vec<usize>,
    /// host1x's sync point values, as the SoC last gave them (channels wait on them)
    pub points: Vec<u32>,
    /// host time spent executing them
    pub busy: std::time::Duration,
    /// a channel waits for a semaphore; when to look again
    waiting: bool,
    retry: u64,
    pause: u64,
    /// what ran last takes until then: the next submission starts no earlier
    busy_until: u64,
    /// guest time for host time (`SBC_GPU_TIME`; 0: the GPU takes none)
    time_scale: f64,
}

/// The longest that one submission keeps the GPU, in guest time: drivers give up at some point.
const MAX_BUSY_NS: u64 = 1_000_000_000;

/// Host time: `Instant` natively, the page's `performance.now()` in the browser.
#[cfg(target_arch = "wasm32")]
#[link(wasm_import_module = "env")]
extern "C" {
    fn sbc_host_now_ms() -> f64;
}
struct Clock(#[cfg(not(target_arch = "wasm32"))] std::time::Instant, #[cfg(target_arch = "wasm32")] f64);
impl Clock {
    #[cfg(not(target_arch = "wasm32"))]
    fn now() -> Clock {
        Clock(std::time::Instant::now())
    }
    #[cfg(not(target_arch = "wasm32"))]
    fn elapsed(&self) -> std::time::Duration {
        self.0.elapsed()
    }
    #[cfg(target_arch = "wasm32")]
    fn now() -> Clock {
        // SAFETY: a clock read that the page provides
        Clock(unsafe { sbc_host_now_ms() })
    }
    #[cfg(target_arch = "wasm32")]
    fn elapsed(&self) -> std::time::Duration {
        // SAFETY: as above
        std::time::Duration::from_secs_f64((unsafe { sbc_host_now_ms() } - self.0).max(0.0) / 1000.0)
    }
}

pub mod host;

impl Default for Gpu {
    fn default() -> Self {
        // the shader JIT shares the page's function table with the CPU's
        #[cfg(target_arch = "wasm32")]
        maxwell_shader::jit::set_reset_count(arm_a64::wasm_jit_resets);
        let mut regs = HashMap::new();
        for (r, v) in [
            (0x12006c, 1),            // L2 caches (and ROPs)
            (0x17e280, 2 << 28 | 1 << 24 | 4), // L2: slices, 1 KiB lines, 4 compression tags a line
            (0x409604, 1 << 16 | 1),  // one ROP unit, one GPC
            (0x502608, 2),            // two TPCs
            (0x022430, 1),            // the unit counts nvgpu reads: GPCs,
            (0x022434, 2),            // TPCs per GPC,
            (0x022438, 1),            // frame buffer partitions
            (0x120078, 1),            // GPCs on the privileged ring
            (0x500c30, 3),
            (0x022210, 1),            // the PMU's reset is out
            (0x100c80, 0),
        ] {
            regs.insert(r, v);
        }
        Self {
            regs,
            pmu: Default::default(),
            mmu: Default::default(),
            channels: vec![Channel::default(); CHANNELS as usize],
            runlist: Vec::new(),
            host: Default::default(),
            fifo_intr: 0,
            runlist_intr: 0,
            timer_intr: false,
            now: 0,
            last_time: 0,
            trace: std::env::var_os("SBC_TRACE_GPU").is_some(),
            submissions: 0,
            syncpts: Vec::new(),
            points: vec![0; crate::host1x::POINTS],
            busy: Default::default(),
            waiting: false,
            retry: 0,
            pause: 0,
            busy_until: 0,
            time_scale: std::env::var("SBC_GPU_TIME").ok().and_then(|s| s.parse().ok()).unwrap_or(1.0),
        }
    }
}

impl Gpu {
    fn r(&self, o: u32) -> u32 {
        self.regs.get(&o).copied().unwrap_or(0)
    }

    fn time(&mut self) -> u64 {
        self.last_time = (self.now & !31).max(self.last_time + 32);
        self.last_time
    }

    /// The PMC's pending interrupts.
    fn pending(&self) -> u32 {
        let mut v = 0;
        if self.fifo_intr & self.r(0x2140) != 0 {
            v |= INTR_FIFO;
        }
        if self.timer_intr && self.r(0x9140) & 1 != 0 {
            v |= INTR_TIMER;
        }
        if self.pmu.irq() {
            v |= INTR_PMU;
        }
        v
    }

    pub fn irq(&self) -> bool {
        self.r(0x140) & 1 != 0 && self.pending() & self.r(0x640) != 0
    }

    pub fn read(&mut self, offset: u64) -> u32 {
        let o = offset as u32 & 0xff_fffc;
        let v = match o {
            0 => BOOT0,
            0x100 => self.pending(),
            0x104 => 0,
            0x1710 => 0, // BARs bound
            0x2004 => 1, // one PBDMA
            0x2008 => CHANNELS,
            0x2100 => self.fifo_intr,
            0x2390 => 1, // it serves run list 0
            0x2284 | 0x2634 => 0, // nothing pending
            0x2a00 => self.runlist_intr,
            0x9100 => self.timer_intr as u32,
            0x9400 => self.time() as u32,
            0x9410 => (self.last_time.max(self.now) >> 32) as u32,
            0x022700..=0x0227fc => TOP.get((o as usize - 0x022700) / 4).copied().unwrap_or(0),
            0x070000..=0x070010 => 0, // flushes are done
            0x100c80 => self.r(o) | 0xff_8000,
            // the write-protected region: 1 MiB of nothing (the secure falcons' code is not run)
            0x100cd4 => match self.r(o) & 0xff {
                2 => 0x17f0_0000,
                3 => 0x17f0_0e00,
                _ => self.r(o),
            },
            0x10_a000..=0x10_afff => self.pmu.read(o - pmu::BASE),
            0x12_004c => 0,
            // the graphics clock's PLL locks and ramps at once
            0x13_7000 => self.r(o) & !(1 << 17) | (self.r(o) & 1) << 17,
            // DVFS1: the noise-aware PLL's calibration (nvgpu) is done at once
            0x13_7014 => self.r(o) | 1 << 31,
            0x13_28a0 => self.r(o) | 1 << 24,
            0x40_4170 => self.r(o) & !0x10,
            0x40_9108 | 0x41_a108 => 0x4_0040, // 16 KiB of code, 8 of data
            // the context switch arbiter takes a command at once
            0x40_9a10 => self.r(o) & !0x1f,
            // L2 slices: compression-tag clean/invalidate/clear and cache flushes are done at once
            0x14_0000..=0x17_ffff if o & 0x1ff == 0x6c => self.r(o) & !7,
            0x14_0000..=0x17_ffff if o & 0x1ff == 0xa0 || o & 0x1ff == 0xa4 => self.r(o) & !1,
            0x40_912c | 0x41_a12c => 4 << 12 | 1 << 8 | 2 << 4 | 5,
            0x40_910c | 0x41_a10c => self.r(o) & !6,
            0x80_0000..=0x80_0fff => {
                let c = self.channels[(o as usize >> 3) & 0x1ff];
                if o & 4 == 0 { c.inst } else { (c.enabled as u32) << 10 }
            }
            _ => self.r(o),
        };
        if self.trace && !matches!(o, 0x9400 | 0x9410) {
            eprintln!("[gpu] read {o:06x} = {v:08x}");
        }
        v
    }

    pub fn write(&mut self, offset: u64, v: u32, smmu: &Smmu, ram: &mut [u8]) {
        let o = offset as u32 & 0xff_fffc;
        if self.trace {
            eprintln!("[gpu] write {o:06x} <- {v:08x}");
        }
        match o {
            0 | 0x100 | 0x104 => {}
            0x2100 => self.fifo_intr &= !v,
            0x2a00 => {
                self.runlist_intr &= !v;
                if self.runlist_intr == 0 {
                    self.fifo_intr &= !FIFO_RUNLIST;
                }
            }
            0x2274 => {
                self.regs.insert(o, v);
                self.commit_runlist(v, smmu, ram);
            }
            0x9100 => {
                if v & 1 != 0 {
                    self.timer_intr = false;
                }
            }
            0x10_0cbc => self.mmu.invalidate(),
            0x10_a000..=0x10_afff => self.pmu.write(o - pmu::BASE, v),
            0x40_9504 => {
                self.regs.insert(o, v);
                self.fecs(v);
            }
            0x40_9100 | 0x40_9130 if v & 2 != 0 => {
                self.regs.insert(0x40_9800, 1); // the context switch firmware is up
            }
            0x40_9800 => {
                self.regs.insert(o, v);
            }
            0x80_0000..=0x80_0fff => {
                let c = &mut self.channels[(o as usize >> 3) & 0x1ff];
                if o & 4 == 0 {
                    // a channel bound to another instance block is a new one; unbinding, and binding
                    // the same block again (nvgpu does it when the GPU comes back from power gating),
                    // keeps what the channel's context holds: its subchannels' classes, a half-run
                    // push buffer, a semaphore it waits for
                    if std::env::var_os("SBC_TRACE_SYNCPT").is_some() {
                        eprintln!("[chan] {} inst {:#x} -> {v:#x}", (o >> 3) & 0x1ff, c.inst);
                    }
                    if v >> 31 != 0 {
                        if (c.bound ^ v) & 0x0fff_ffff != 0 {
                            self.host.forget((o as u32 >> 3) & 0x1ff);
                        }
                        c.bound = v;
                    }
                    c.inst = v;
                } else {
                    if v & 1 << 10 != 0 {
                        c.enabled = true;
                    }
                    if v & 1 << 11 != 0 {
                        c.enabled = false;
                    }
                }
                self.run(smmu, ram);
            }
            _ => {
                self.regs.insert(o, v);
            }
        }
    }

    /// The context switch firmware's methods (data in 0x409500).
    fn fecs(&mut self, method: u32) {
        let status = self.r(0x40_9800);
        let (a, b) = match method {
            0x03 => (status | 0x10, None),          // bind a channel's context
            0x09 => (status | 1, None),             // save it as the golden one
            0x10 => (0x2_0000, None),               // size of a context image
            0x16 => (0x1000, None),                 // of the depth cull image
            0x25 => (0x1000, None),                 // of the performance monitor's
            0x30..=0x32 => (1, None),
            0x21 => (status, None),
            _ => (status | 1, Some(1)),             // stop and start switching, halt: done
        };
        self.regs.insert(0x40_9800, a);
        if let Some(b) = b {
            self.regs.insert(0x40_9804, b);
        }
    }

    fn commit_runlist(&mut self, v: u32, smmu: &Smmu, ram: &mut [u8]) {
        let (list, count) = (v >> 20 & 15, v & 0xffff);
        let base = ((self.r(0x2270) & 0x0fff_ffff) as u64) << 12;
        if list == 0 {
            self.runlist = (0..count as u64)
                .filter_map(|k| mmu::read32(base + 8 * k, smmu, ram))
                .filter(|e| e & 0x2000 == 0) // (a channel group's header)
                .map(|e| e & 0xfff)
                .collect();
        }
        self.runlist_intr |= 1 << list;
        self.fifo_intr |= FIFO_RUNLIST;
        self.run(smmu, ram);
    }

    /// The address space of BAR1, through which the CPU reaches the channels' USERD.
    fn bar1_space(&self, smmu: &Smmu, ram: &[u8]) -> Space {
        mmu::space(((self.r(0x1704) & 0x0fff_ffff) as u64) << 12, smmu, ram)
    }

    pub fn bar1_read(&mut self, offset: u64, smmu: &Smmu, ram: &mut [u8]) -> u32 {
        // the driver looks how far a channel has read when a ring is full, and gives up after a number
        // of reads that takes no time here: what waits for the GPU runs now, and takes its time after
        if matches!(offset & 0x1fc, 0x44 | 0x60 | 0x88) && self.waiting && self.now < self.busy_until {
            let left = self.busy_until - self.now;
            self.busy_until = 0;
            self.run(smmu, ram);
            self.busy_until = (self.busy_until.max(self.now) + left).min(self.now + MAX_BUSY_NS);
        }
        let s = self.bar1_space(smmu, ram);
        self.mmu.translate(s, offset, smmu, ram).map_or(u32::MAX, |p| {
            let o = p.offset + (offset & 0xffc) as usize;
            u32::from_le_bytes(ram[o..o + 4].try_into().unwrap())
        })
    }

    pub fn bar1_write(&mut self, offset: u64, v: u32, smmu: &Smmu, ram: &mut [u8]) {
        let s = self.bar1_space(smmu, ram);
        if let Some(p) = self.mmu.translate(s, offset, smmu, ram) {
            let o = p.offset + (offset & 0xffc) as usize;
            ram[o..o + 4].copy_from_slice(&v.to_le_bytes());
        }
        if offset & 0x1fc == 0x8c {
            self.run(smmu, ram); // GP_PUT: there is work
        }
    }

    /// Run every channel that has work. All of it is done at once, and keeps the GPU for as long as the
    /// host needed: what comes next waits, so that the guest sees the speed it gets.
    fn run(&mut self, smmu: &Smmu, ram: &mut [u8]) {
        if self.now < self.busy_until {
            (self.waiting, self.retry) = (true, self.busy_until);
            return;
        }
        let start = Clock::now();
        // a channel may release the semaphore another waits for
        let mut moved = false;
        for _ in 0..64 {
            let (waiting, progress) = self.run_once(smmu, ram);
            self.waiting = waiting;
            moved |= progress;
            if !waiting || !progress {
                break;
            }
        }
        // a semaphore that stays away is looked at less and less often
        self.pause = if self.waiting && !moved { (2 * self.pause).clamp(RETRY_NS, MAX_RETRY_NS) } else { 0 };
        self.retry = self.now + self.pause.max(RETRY_NS);
        let took = start.elapsed();
        self.busy += took;
        self.busy_until = self.now + ((took.as_nanos() as f64 * self.time_scale) as u64).min(MAX_BUSY_NS);
    }

    fn run_once(&mut self, smmu: &Smmu, ram: &mut [u8]) -> (bool, bool) {
        let (mut waiting, mut progress) = (false, false);
        let now = self.time();
        for k in 0..self.runlist.len() {
            let id = self.runlist[k];
            let Some(c) = self.channels.get(id as usize).copied() else { continue };
            if c.enabled && c.inst >> 31 != 0 {
                let inst = ((c.inst & 0x0fff_ffff) as u64) << 12;
                let space = mmu::space(inst, smmu, ram);
                let mut view = mmu::View { ram, smmu, mmu: &self.mmu, space, fault: Default::default() };
                let done = self.host.run_channel(id, inst, &mut view, now, &mut self.points);
                if let Some((va, write)) = view.fault.get() {
                    if std::env::var_os("SBC_SHADER_FAULT").is_some() {
                        eprintln!("[gpu] channel {id}: unmapped {} at {va:#x}", if write { "write" } else { "read" });
                    }
                }
                self.submissions += done.entries;
                self.syncpts.extend(&done.syncpts);
                waiting |= done.waiting;
                progress |= done.resumed || done.entries != 0;
                if done.nonstall {
                    self.fifo_intr |= FIFO_NONSTALL;
                }
            }
        }
        (waiting, progress)
    }

    /// Channels that wait for a semaphore the CPU may write.
    pub fn poll(&mut self, smmu: &Smmu, ram: &mut [u8]) {
        if self.waiting && self.now >= self.retry {
            self.run(smmu, ram);
        }
    }

    pub fn tick(&mut self, now: u64) {
        let before = std::mem::replace(&mut self.now, now);
        if self.r(0x9140) & 1 != 0 && !self.timer_intr && self.alarm(before).is_some_and(|t| now >= t) {
            self.timer_intr = true;
        }
    }

    /// The first time after `after` that the timer's alarm goes off: it compares the low word.
    fn alarm(&self, after: u64) -> Option<u64> {
        let a = self.regs.get(&0x9420).copied()? as u64;
        let t = after & !0xffff_ffff | a;
        Some(if t <= after { t + (1 << 32) } else { t })
    }

    pub fn next_event(&self) -> Option<u64> {
        let alarm = (self.r(0x9140) & 1 != 0 && !self.timer_intr).then(|| self.alarm(self.now)).flatten();
        alarm.into_iter().chain(self.waiting.then_some(self.retry)).min()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Work waits while the GPU is busy, but not for a driver that looks whether a ring has room.
    #[test]
    fn busy_and_polled() {
        let (smmu, mut ram) = (Smmu::default(), vec![0; 0x1000]);
        let mut g = Gpu { busy_until: 5_000_000, ..Default::default() };
        g.tick(1_000_000);
        g.run(&smmu, &mut ram);
        assert_eq!((g.waiting, g.next_event()), (true, Some(5_000_000)));
        g.bar1_read(0x40, &smmu, &mut ram); // (something else)
        assert!(g.waiting);
        g.bar1_read(0x288, &smmu, &mut ram);
        assert!(!g.waiting && g.busy_until >= 5_000_000);
        // what runs keeps the GPU, as long as the host took (which a coarse clock may not see at once)
        let mut f = Gpu { time_scale: 1e6, ..Default::default() };
        f.tick(1_000_000);
        assert!((0..100_000).any(|_| {
            f.run(&smmu, &mut ram);
            f.busy_until > f.now
        }));
        let mut f = Gpu { time_scale: 0.0, ..Default::default() };
        f.run(&smmu, &mut ram);
        assert_eq!(f.busy_until, 0);
        // and later by itself
        g.run(&smmu, &mut ram);
        assert!(g.waiting);
        g.tick(g.retry);
        g.poll(&smmu, &mut ram);
        assert!(!g.waiting);
    }

    #[test]
    fn alarms_compare_the_low_word() {
        let mut g = Gpu::default();
        let at = |g: &mut Gpu, now: u64| {
            g.tick(now);
            std::mem::take(&mut g.timer_intr)
        };
        g.regs.insert(0x9140, 1);
        g.tick(0x1_ffff_0000);
        // just below the wrap of the low word, stepped over
        g.regs.insert(0x9420, 0xffff_ff00);
        assert_eq!(g.next_event(), Some(0x1_ffff_ff00));
        assert!(!at(&mut g, 0x1_ffff_8000) && at(&mut g, 0x2_0000_0040) && !at(&mut g, 0x2_0000_1000));
        // three seconds ahead
        g.regs.insert(0x9420, 3_000_000_000);
        assert_eq!(g.next_event(), Some(0x2_0000_0000 + 3_000_000_000));
        assert!(!at(&mut g, 0x2_0000_0000 + 2_999_999_999) && at(&mut g, 0x2_0000_0000 + 3_000_000_000));
    }
}
