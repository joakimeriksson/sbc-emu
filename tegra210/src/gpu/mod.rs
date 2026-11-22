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
    pub trace: bool,
    /// registers touched that have no model, for reports
    pub submissions: u64,
}

pub mod host;

impl Default for Gpu {
    fn default() -> Self {
        let mut regs = HashMap::new();
        for (r, v) in [
            (0x12006c, 1),            // L2 caches (and ROPs)
            (0x17e280, 2 << 28),      // L2 slices
            (0x409604, 1 << 16 | 1),  // one ROP unit, one GPC
            (0x502608, 2),            // two TPCs
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
            trace: std::env::var_os("SBC_TRACE_GPU").is_some(),
            submissions: 0,
        }
    }
}

impl Gpu {
    fn r(&self, o: u32) -> u32 {
        self.regs.get(&o).copied().unwrap_or(0)
    }

    fn time(&self) -> u64 {
        self.now & !31
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
            0x9410 => (self.time() >> 32) as u32,
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
            0x40_4170 => self.r(o) & !0x10,
            0x40_9108 | 0x41_a108 => 0x4_0040, // 16 KiB of code, 8 of data
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

    pub fn bar1_read(&mut self, offset: u64, smmu: &Smmu, ram: &[u8]) -> u32 {
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

    /// Run every channel that has work.
    fn run(&mut self, smmu: &Smmu, ram: &mut [u8]) {
        for k in 0..self.runlist.len() {
            let id = self.runlist[k];
            let Some(c) = self.channels.get(id as usize).copied() else { continue };
            if c.enabled && c.inst >> 31 != 0 {
                let inst = ((c.inst & 0x0fff_ffff) as u64) << 12;
                let space = mmu::space(inst, smmu, ram);
                let mut view = mmu::View { ram, smmu, mmu: &self.mmu, space, fault: Default::default() };
                let done = self.host.run_channel(id, inst, &mut view, self.now);
                self.submissions += done.entries;
                if done.nonstall {
                    self.fifo_intr |= FIFO_NONSTALL;
                }
            }
        }
    }

    pub fn tick(&mut self, now: u64) {
        self.now = now;
        if self.r(0x9140) & 1 != 0 && !self.timer_intr && self.alarm().is_some_and(|t| now >= t) {
            self.timer_intr = true;
        }
    }

    /// When the timer's alarm goes off (it compares the time's low word).
    fn alarm(&self) -> Option<u64> {
        let a = self.regs.get(&0x9420).copied()? as u64;
        let t = self.now & !0xffff_ffff | a;
        Some(if t + (1 << 31) < self.now { t + (1 << 32) } else { t })
    }

    pub fn next_event(&self) -> Option<u64> {
        (self.r(0x9140) & 1 != 0 && !self.timer_intr).then(|| self.alarm()).flatten().filter(|&t| t > self.now)
    }
}
