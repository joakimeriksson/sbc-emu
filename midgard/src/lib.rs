//! ARM Mali-T760 MP4 (Midgard, architecture v5), the GPU of the Rockchip RK3288, as Linux's
//! `panfrost` driver and Mesa's `panfrost` Gallium driver use it.
//!
//! One 64 KiB register window:
//!
//! | offset | block |
//! |---|---|
//! | 0x0000 | GPU control: identification, power, reset, caches, the "gpu" interrupt |
//! | 0x1000 | job control: the "job" interrupt and three job slots at 0x1800 + 0x80 * n |
//! | 0x2000 | MMU: the "mmu" interrupt and address spaces at 0x2400 + 0x40 * n |
//!
//! Jobs are chains of descriptors in GPU memory (Mesa's `genxml/v5.xml`). A chain runs
//! when the driver starts its slot; it completes, with its interrupt, after the virtual
//! time the work would take.

#![allow(clippy::needless_range_loop)]

pub mod job;
pub mod mmu;
pub mod vm;

use mmu::AddressSpace;
use vm::Vm;

/// Size of the register window.
pub const SIZE: u64 = 0x1_0000;

/// GPU_ID: product 0x0750 (T760), r0p1 status 0, as on the RK3288.
const GPU_ID: u32 = 0x0750_0010;
/// L2: 64-byte lines, 4 ways, 256 KiB, 128-bit bus (log2 of each).
const L2_FEATURES: u32 = 0x0712_0206;
/// Tiler: 512-byte bins, 8 hierarchy levels.
const TILER_FEATURES: u32 = 0x0000_0809;
/// One L2 slice, coherent core groups.
const MEM_FEATURES: u32 = 0x0000_0001;
/// 48-bit virtual and 40-bit physical addresses.
const MMU_FEATURES: u32 = 0x0000_2830;
const AS_PRESENT: u32 = 0xff;
const JS_PRESENT: u32 = 0x7;
/// What each job slot can run (bit per job type: 1 null, 2 write value, 3 cache flush,
/// 4 compute, 5 vertex, 6 geometry, 7 tiler, 8 fused, 9 fragment): slot 0 fragment
/// jobs, slot 1 vertex/tiler/compute, slot 2 compute.
const JS_FEATURES: [u32; 3] = [0x20e, 0x1fe, 0x7e];
const SHADER_PRESENT: u32 = 0xf;
const TILER_PRESENT: u32 = 0x1;
const L2_PRESENT: u32 = 0x1;
const THREAD_MAX_THREADS: u32 = 256;
const THREAD_MAX_WORKGROUP_SIZE: u32 = 256;
const THREAD_MAX_BARRIER_SIZE: u32 = 256;
/// Nonzero AFBC_FEATURES tells Mesa there is no framebuffer compression (v5 GPUs may omit
/// it); the model writes and samples plain linear and u-interleaved surfaces only.
const AFBC_FEATURES: u32 = 1;

pub const NUM_SLOTS: usize = 3;
pub const NUM_AS: usize = 8;

/// Interrupt lines, in the order of the device tree's `interrupt-names`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Irq {
    Job,
    Mmu,
    Gpu,
}

/// GPU clock (Hz) and the machine's tick rate, for job durations.
const GPU_HZ: u64 = 400_000_000;
const TICK_HZ: u64 = 24_000_000;

mod gpu_irq {
    pub const RESET_COMPLETED: u32 = 1 << 8;
    pub const POWER_CHANGED: u32 = 1 << 9;
    pub const POWER_CHANGED_ALL: u32 = 1 << 10;
    pub const CLEAN_CACHES_COMPLETED: u32 = 1 << 17;
}

/// JS_STATUS / exception codes (`DRM_PANFROST_EXCEPTION_*`).
pub mod status {
    pub const DONE: u32 = 0x01;
    pub const STOPPED: u32 = 0x03;
    pub const TERMINATED: u32 = 0x04;
    pub const ACTIVE: u32 = 0x08;
    pub const JOB_CONFIG_FAULT: u32 = 0x40;
    pub const JOB_READ_FAULT: u32 = 0x42;
    pub const INSTR_INVALID_ENC: u32 = 0x51;
    pub const DATA_INVALID_FAULT: u32 = 0x58;
}

#[derive(Clone, Copy, Debug, Default)]
struct Slot {
    head: u64,
    tail: u64,
    affinity: u64,
    config: u32,
    xaffinity: u32,
    status: u32,
    head_next: u64,
    affinity_next: u64,
    config_next: u32,
    command_next: u32,
    flush_id_next: u32,
    /// A chain is running; it completes at this tick with this status.
    running: Option<(u64, u32)>,
}

/// The register state of the GPU plus the host-side state that jobs share (polygon lists).
pub struct MaliT760 {
    gpu_rawstat: u32,
    gpu_mask: u32,
    fault_status: u32,
    fault_address: u64,
    shader_ready: u32,
    tiler_ready: u32,
    l2_ready: u32,
    config: [u32; 4],
    perfcnt: [u32; 8],
    pwr_override: [u32; 2],
    cycle_count_running: bool,
    cycle_base: u64,
    flush_id: u32,
    job_rawstat: u32,
    job_mask: u32,
    slots: [Slot; NUM_SLOTS],
    mmu_rawstat: u32,
    mmu_mask: u32,
    aspaces: [AddressSpace; NUM_AS],
    pub jobs: job::JobState,
    /// Current machine time in ticks (24 MHz), set by `update`.
    now: u64,
    pub trace: bool,
    /// Chains run, for the end-of-run report.
    pub chains: u64,
}

impl MaliT760 {
    pub fn new(trace: bool) -> Self {
        Self {
            gpu_rawstat: 0,
            gpu_mask: 0,
            fault_status: 0,
            fault_address: 0,
            shader_ready: 0,
            tiler_ready: 0,
            l2_ready: 0,
            config: [0; 4],
            perfcnt: [0; 8],
            pwr_override: [0; 2],
            cycle_count_running: false,
            cycle_base: 0,
            flush_id: 0,
            job_rawstat: 0,
            job_mask: 0,
            slots: Default::default(),
            mmu_rawstat: 0,
            mmu_mask: 0,
            aspaces: Default::default(),
            jobs: job::JobState::default(),
            now: 0,
            trace,
            chains: 0,
        }
    }

    fn soft_reset(&mut self) {
        let trace = self.trace;
        let chains = self.chains;
        let now = self.now;
        *self = Self::new(trace);
        self.chains = chains;
        self.now = now;
        self.gpu_rawstat = gpu_irq::RESET_COMPLETED;
    }

    fn cycles(&self) -> u64 {
        self.now * (GPU_HZ / 1_000_000) / (TICK_HZ / 1_000_000)
    }

    fn js_state(&self) -> u32 {
        let mut s = 0;
        for (k, slot) in self.slots.iter().enumerate() {
            if slot.running.is_some() {
                s |= 1 << k;
            }
            if slot.command_next != 0 {
                s |= 1 << (k + 16);
            }
        }
        s
    }

    pub fn read(&mut self, offset: u64) -> u32 {
        let o = offset as u32;
        match o {
            0x000 => GPU_ID,
            0x004 => L2_FEATURES,
            0x008 => 0,
            0x00c => TILER_FEATURES,
            0x010 => MEM_FEATURES,
            0x014 => MMU_FEATURES,
            0x018 => AS_PRESENT,
            0x01c => JS_PRESENT,
            0x020 => self.gpu_rawstat,
            0x028 => self.gpu_mask,
            0x02c => self.gpu_rawstat & self.gpu_mask,
            0x034 => 0, // GPU_STATUS: idle
            0x038 => self.flush_id,
            0x03c => self.fault_status,
            0x040 => self.fault_address as u32,
            0x044 => (self.fault_address >> 32) as u32,
            0x04c => AFBC_FEATURES,
            0x054 | 0x058 => self.pwr_override[(o as usize - 0x54) / 4],
            0x060..=0x07c => self.perfcnt[(o as usize - 0x60) / 4],
            0x090 | 0x094 => {
                let c = if self.cycle_count_running { self.cycles() - self.cycle_base } else { 0 };
                if o == 0x90 { c as u32 } else { (c >> 32) as u32 }
            }
            0x098 => self.now as u32,
            0x09c => (self.now >> 32) as u32,
            0x0a0 => THREAD_MAX_THREADS,
            0x0a4 => THREAD_MAX_WORKGROUP_SIZE,
            0x0a8 => THREAD_MAX_BARRIER_SIZE,
            0x0ac => 0,
            0x0b0..=0x0bc => 0, // TEXTURE_FEATURES: no native compressed formats
            0x0c0..=0x0c8 => JS_FEATURES[(o as usize - 0xc0) / 4],
            0x100 => SHADER_PRESENT,
            0x110 => TILER_PRESENT,
            0x120 => L2_PRESENT,
            0x140 => self.shader_ready,
            0x150 => self.tiler_ready,
            0x160 => self.l2_ready,
            0x200..=0x264 => 0, // PWRTRANS / PWRACTIVE: transitions are instant
            0x300 => 0,         // COHERENCY_FEATURES
            0x310 => 0,         // THREAD_TLS_ALLOC: use max threads
            0xf00..=0xf0c => self.config[(o as usize - 0xf00) / 4],
            0x1000 => self.job_rawstat,
            0x1008 => self.job_mask,
            0x100c => self.job_rawstat & self.job_mask,
            0x1010 => self.js_state(),
            0x1800..=0x1fff => self.slot_read(o),
            0x2000 => self.mmu_rawstat,
            0x2008 => self.mmu_mask,
            0x200c => self.mmu_rawstat & self.mmu_mask,
            0x2400..=0x27ff => self.as_read(o),
            _ => 0,
        }
    }

    fn slot_read(&self, o: u32) -> u32 {
        let k = ((o - 0x1800) / 0x80) as usize;
        if k >= NUM_SLOTS {
            return 0;
        }
        let s = &self.slots[k];
        let lo = |v: u64| v as u32;
        let hi = |v: u64| (v >> 32) as u32;
        match (o - 0x1800) % 0x80 {
            0x00 => lo(s.head),
            0x04 => hi(s.head),
            0x08 => lo(s.tail),
            0x0c => hi(s.tail),
            0x10 => lo(s.affinity),
            0x14 => hi(s.affinity),
            0x18 => s.config,
            0x1c => s.xaffinity,
            0x24 => if s.running.is_some() { status::ACTIVE } else { s.status },
            0x40 => lo(s.head_next),
            0x44 => hi(s.head_next),
            0x50 => lo(s.affinity_next),
            0x54 => hi(s.affinity_next),
            0x58 => s.config_next,
            0x60 => s.command_next,
            0x70 => s.flush_id_next,
            _ => 0,
        }
    }

    fn as_read(&self, o: u32) -> u32 {
        let n = ((o - 0x2400) / 0x40) as usize;
        if n >= NUM_AS {
            return 0;
        }
        let a = &self.aspaces[n];
        let lo = |v: u64| v as u32;
        let hi = |v: u64| (v >> 32) as u32;
        match (o - 0x2400) % 0x40 {
            0x00 => lo(a.transtab),
            0x04 => hi(a.transtab),
            0x08 => lo(a.memattr),
            0x0c => hi(a.memattr),
            0x10 => lo(a.lockaddr),
            0x14 => hi(a.lockaddr),
            0x1c => a.faultstatus,
            0x20 => lo(a.faultaddress),
            0x24 => hi(a.faultaddress),
            0x28 => 0, // AS_STATUS: never busy
            0x30 => lo(a.transcfg),
            0x34 => hi(a.transcfg),
            _ => 0,
        }
    }

    pub fn write(&mut self, offset: u64, value: u32, ram: &mut [u8], ram_base: u64) {
        let o = offset as u32;
        match o {
            0x024 => self.gpu_rawstat &= !value,
            0x028 => self.gpu_mask = value,
            0x030 => match value {
                1 | 2 => self.soft_reset(),
                5 => {
                    self.cycle_count_running = true;
                    self.cycle_base = self.cycles();
                }
                6 => self.cycle_count_running = false,
                7 | 8 => {
                    self.flush_id = self.flush_id.wrapping_add(1);
                    self.gpu_rawstat |= gpu_irq::CLEAN_CACHES_COMPLETED;
                }
                _ => {}
            },
            0x050 => {}
            0x054 | 0x058 => self.pwr_override[(o as usize - 0x54) / 4] = value,
            0x060..=0x07c => self.perfcnt[(o as usize - 0x60) / 4] = value,
            0x180 => self.power(|g| &mut g.shader_ready, value & SHADER_PRESENT, true),
            0x190 => self.power(|g| &mut g.tiler_ready, value & TILER_PRESENT, true),
            0x1a0 => self.power(|g| &mut g.l2_ready, value & L2_PRESENT, true),
            0x1c0 => self.power(|g| &mut g.shader_ready, value, false),
            0x1d0 => self.power(|g| &mut g.tiler_ready, value, false),
            0x1e0 => self.power(|g| &mut g.l2_ready, value, false),
            0xf00..=0xf0c => self.config[(o as usize - 0xf00) / 4] = value,
            0x1004 => self.job_rawstat &= !value,
            0x1008 => self.job_mask = value,
            0x1800..=0x1fff => self.slot_write(o, value, ram, ram_base),
            0x2004 => {
                self.mmu_rawstat &= !value;
            }
            0x2008 => self.mmu_mask = value,
            0x2400..=0x27ff => self.as_write(o, value),
            _ => {}
        }
    }

    fn power(&mut self, reg: fn(&mut Self) -> &mut u32, bits: u32, on: bool) {
        let r = reg(self);
        let old = *r;
        if on { *r |= bits } else { *r &= !bits }
        if *r != old || bits != 0 {
            self.gpu_rawstat |= gpu_irq::POWER_CHANGED | gpu_irq::POWER_CHANGED_ALL;
        }
    }

    fn slot_write(&mut self, o: u32, value: u32, ram: &mut [u8], ram_base: u64) {
        let k = ((o - 0x1800) / 0x80) as usize;
        if k >= NUM_SLOTS {
            return;
        }
        let set_lo = |r: &mut u64, v: u32| *r = (*r & !0xffff_ffff) | v as u64;
        let set_hi = |r: &mut u64, v: u32| *r = (*r & 0xffff_ffff) | (v as u64) << 32;
        let s = &mut self.slots[k];
        match (o - 0x1800) % 0x80 {
            0x20 => self.command(k, value),
            0x40 => set_lo(&mut s.head_next, value),
            0x44 => set_hi(&mut s.head_next, value),
            0x50 => set_lo(&mut s.affinity_next, value),
            0x54 => set_hi(&mut s.affinity_next, value),
            0x58 => s.config_next = value,
            0x5c => s.xaffinity = value,
            0x60 => {
                s.command_next = value;
                self.try_start(k, ram, ram_base);
            }
            0x70 => s.flush_id_next = value,
            _ => {}
        }
    }

    /// JS_COMMAND on the running chain: stops terminate it (the model runs a chain to its
    /// end when it starts, so a stop only cancels the completion still pending).
    fn command(&mut self, k: usize, value: u32) {
        let hard = matches!(value, 3 | 5 | 7);
        let soft = matches!(value, 2 | 4 | 6);
        if (hard || soft) && self.slots[k].running.is_some() {
            // The work is already done; report it done rather than stopped so nothing is
            // replayed twice.
            let due = self.now;
            if let Some((t, _)) = &mut self.slots[k].running {
                *t = due;
            }
        }
    }

    /// Start the queued chain if the slot is idle.
    fn try_start(&mut self, k: usize, ram: &mut [u8], ram_base: u64) {
        if self.slots[k].command_next != 1 || self.slots[k].running.is_some() {
            return;
        }
        let s = &mut self.slots[k];
        s.command_next = 0;
        s.head = s.head_next;
        s.affinity = s.affinity_next;
        s.config = s.config_next;
        let (head, config) = (s.head, s.config);
        let aspace = self.aspaces[(config & 0xf) as usize % NUM_AS];
        let mut vm = Vm::new(ram, ram_base, aspace);
        let result = self.jobs.run_chain(&mut vm, head, self.cycles(), self.trace);
        self.chains += 1;
        let mut code = result.status;
        if let Some(f) = vm.fault {
            let n = (config & 0xf) as usize % NUM_AS;
            let a = &mut self.aspaces[n];
            a.faultstatus = f.status | 1 << 10; // AS_FAULTSTATUS[10]: the job manager
            a.faultaddress = f.va;
            self.mmu_rawstat |= 1 << n;
            code = f.status & 0xff;
            if self.trace {
                eprintln!("[mali] slot {k}: MMU fault at {:#x} status {:#x}", f.va, f.status);
            }
        }
        let s = &mut self.slots[k];
        s.tail = result.tail;
        let ticks = (result.cycles * TICK_HZ / GPU_HZ).max(24);
        s.running = Some((self.now + ticks, code));
    }

    fn as_write(&mut self, o: u32, value: u32) {
        let n = ((o - 0x2400) / 0x40) as usize;
        if n >= NUM_AS {
            return;
        }
        let a = &mut self.aspaces[n];
        let set_lo = |r: &mut u64, v: u32| *r = (*r & !0xffff_ffff) | v as u64;
        let set_hi = |r: &mut u64, v: u32| *r = (*r & 0xffff_ffff) | (v as u64) << 32;
        match (o - 0x2400) % 0x40 {
            0x00 => set_lo(&mut a.transtab, value),
            0x04 => set_hi(&mut a.transtab, value),
            0x08 => set_lo(&mut a.memattr, value),
            0x0c => set_hi(&mut a.memattr, value),
            0x10 => set_lo(&mut a.lockaddr, value),
            0x14 => set_hi(&mut a.lockaddr, value),
            0x18 => {
                if value == 1 {
                    a.active_transtab = a.transtab;
                }
            }
            0x30 => set_lo(&mut a.transcfg, value),
            0x34 => set_hi(&mut a.transcfg, value),
            _ => {}
        }
    }

    /// Advance to `now` (24 MHz ticks): complete chains whose time has come.
    pub fn update(&mut self, now: u64, ram: &mut [u8], ram_base: u64) {
        self.now = now;
        for k in 0..NUM_SLOTS {
            if let Some((due, code)) = self.slots[k].running {
                if now >= due {
                    self.slots[k].running = None;
                    self.slots[k].status = code;
                    self.job_rawstat |= if code == status::DONE { 1 << k } else { 1 << (k + 16) };
                    if code != status::DONE && self.trace {
                        eprintln!("[mali] slot {k}: chain {:#x} failed with {code:#x}", self.slots[k].head);
                    }
                    self.try_start(k, ram, ram_base);
                }
            }
        }
    }

    /// The next tick at which `update` has work.
    pub fn next_deadline(&self) -> Option<u64> {
        self.slots.iter().filter_map(|s| s.running.map(|(t, _)| t)).min()
    }

    pub fn irq(&self, irq: Irq) -> bool {
        match irq {
            Irq::Job => self.job_rawstat & self.job_mask != 0,
            Irq::Mmu => self.mmu_rawstat & self.mmu_mask != 0,
            Irq::Gpu => self.gpu_rawstat & self.gpu_mask != 0,
        }
    }
}

impl Default for MaliT760 {
    fn default() -> Self {
        Self::new(false)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn identification_power_and_reset() {
        let mut g = MaliT760::default();
        let mut ram = vec![0u8; 0x1000];
        assert_eq!(g.read(0) >> 16, 0x750);
        g.write(0x28, u32::MAX, &mut ram, 0);
        g.write(0x30, 1, &mut ram, 0);
        assert_ne!(g.read(0x20) & gpu_irq::RESET_COMPLETED, 0);
        g.write(0x1a0, 1, &mut ram, 0);
        g.write(0x180, 0xf, &mut ram, 0);
        assert_eq!(g.read(0x140), 0xf);
        assert_eq!(g.read(0x160), 1);
        g.write(0x1c0, 0xf, &mut ram, 0);
        assert_eq!(g.read(0x140), 0);
    }
}
