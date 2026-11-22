//! ARM Mali-400 MP2 (Utgard), the GPU of the Allwinner H3, as Linux's `lima` driver and
//! Mesa's `lima` Gallium driver use it.
//!
//! One register window (64 KiB) holds every unit at a fixed offset:
//!
//! | offset | unit |
//! |---|---|
//! | 0x0000 | GP: the geometry processor (vertex shader and the PLBU, which bins primitives into tiles) |
//! | 0x1000 | L2 cache |
//! | 0x2000 | PMU (power domains) |
//! | 0x3000 | GP MMU |
//! | 0x4000, 0x5000 | PP0/PP1 MMUs |
//! | 0x8000, 0xa000 | PP0/PP1: the pixel processors (fragment shading, one 16x16 tile at a time) |
//!
//! Each unit has its own interrupt line. Jobs start when the driver writes a GP or PP
//! command register and finish within that write.

#![allow(clippy::needless_range_loop)]

pub mod fs;
pub mod gp;
pub mod half;
pub mod mmu;
pub mod plbu;
pub mod pp;
pub mod vm;

use mmu::Mmu;
use std::collections::HashSet;
use vm::Vm;

/// Number of pixel processors (the H3 has an MP2).
pub const NUM_PP: usize = 2;
/// Size of the register window.
pub const SIZE: u64 = 0x1_0000;

/// The GP and PP version registers: Mali-400 r1p1.
const GP_VERSION: u32 = 0x0b07_0101;
const PP_VERSION: u32 = 0xcd07_0101;
/// L2 cache geometry: 64-byte lines, 4 ways, 64 KiB, 64-bit bus (log2 of each).
const L2_SIZE: u32 = 6 | 2 << 8 | 16 << 16 | 6 << 24;

/// Interrupt sources, in the order of the device tree's `interrupt-names`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Irq {
    Gp,
    GpMmu,
    Pp0,
    PpMmu0,
    Pp1,
    PpMmu1,
    Pmu,
}

/// The GPU clock (Hz) and the machine's tick rate, for job durations.
const GPU_HZ: u64 = 432_000_000;
const TICK_HZ: u64 = 24_000_000;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
enum Unit {
    #[default]
    Gp,
    Pp(usize),
}

/// Physical memory the GPU reads and writes (through its MMUs).
pub trait Memory {
    fn read_u32(&self, pa: u32) -> Option<u32>;
    fn write_u32(&mut self, pa: u32, value: u32) -> bool;
}

mod gp_reg {
    pub const VSCL_START: u64 = 0x00;
    pub const VSCL_END: u64 = 0x04;
    pub const PLBUCL_START: u64 = 0x08;
    pub const PLBUCL_END: u64 = 0x0c;
    pub const CMD: u64 = 0x20;
    pub const INT_RAWSTAT: u64 = 0x24;
    pub const INT_CLEAR: u64 = 0x28;
    pub const INT_MASK: u64 = 0x2c;
    pub const INT_STAT: u64 = 0x30;
    pub const STATUS: u64 = 0x68;
    pub const VERSION: u64 = 0x6c;
    pub const VSCL_START_READ: u64 = 0x80;
    pub const PLBCL_START_READ: u64 = 0x84;

    pub const CMD_START_VS: u32 = 1 << 0;
    pub const CMD_START_PLBU: u32 = 1 << 1;
    pub const CMD_UPDATE_PLBU_ALLOC: u32 = 1 << 4;
    pub const IRQ_PLBU_OUT_OF_MEM: u32 = 1 << 2;
    pub const STATUS_PLBU_ACTIVE: u32 = 1 << 3;
    pub const CMD_RESET: u32 = 1 << 5;
    pub const CMD_STOP_BUS: u32 = 1 << 9;
    pub const CMD_SOFT_RESET: u32 = 1 << 10;

    pub const IRQ_VS_END: u32 = 1 << 0;
    pub const IRQ_PLBU_END: u32 = 1 << 1;
    pub const IRQ_RESET_COMPLETED: u32 = 1 << 19;

    pub const STATUS_BUS_STOPPED: u32 = 1 << 2;
    pub const STATUS_VS_ACTIVE: u32 = 1 << 1;
}

mod pp_reg {
    pub const FRAME_REGS: u64 = 23;
    pub const WB_REGS: u64 = 12;
    pub const VERSION: u64 = 0x1000;
    pub const CURRENT_REND_LIST: u64 = 0x1004;
    pub const STATUS: u64 = 0x1008;
    pub const CTRL: u64 = 0x100c;
    pub const INT_RAWSTAT: u64 = 0x1020;
    pub const INT_CLEAR: u64 = 0x1024;
    pub const INT_MASK: u64 = 0x1028;
    pub const INT_STATUS: u64 = 0x102c;

    pub const CTRL_STOP_BUS: u32 = 1 << 0;
    pub const CTRL_FORCE_RESET: u32 = 1 << 5;
    pub const CTRL_START_RENDERING: u32 = 1 << 6;
    pub const CTRL_SOFT_RESET: u32 = 1 << 7;

    pub const IRQ_END_OF_FRAME: u32 = 1 << 0;
    pub const IRQ_RESET_COMPLETED: u32 = 1 << 12;

    pub const STATUS_BUS_STOPPED: u32 = 1 << 4;
    pub const STATUS_RENDERING_ACTIVE: u32 = 1 << 0;
}

/// The geometry processor.
#[derive(Default)]
pub struct Gp {
    /// 0x00-0x7c, word index
    regs: [u32; 32],
    rawstat: u32,
    mask: u32,
    status: u32,
    /// Jobs run (vertex and PLBU command lists).
    pub jobs: u64,
    /// A PLBU list stopped by OUT_OF_MEM, waiting for more heap.
    plbu: Option<plbu::Plbu>,
    /// The last vertex shader's (gl_Position, gl_PointSize) arrays.
    point_sizes: Vec<(u32, u32)>,
    /// The current job ran a vertex shader (its end is reported with the PLBU's).
    vs_ran: bool,
}

/// A pixel processor.
pub struct Pp {
    /// The frame registers (0x0000-0x0058).
    pub frame: [u32; pp_reg::FRAME_REGS as usize],
    /// The three write-back units (0x100, 0x200, 0x300).
    pub wb: [[u32; pp_reg::WB_REGS as usize]; 3],
    /// Other registers at 0x1000 and up, word index.
    regs: Vec<u32>,
    rawstat: u32,
    mask: u32,
    status: u32,
    /// Frames rendered.
    pub jobs: u64,
}

impl Default for Pp {
    fn default() -> Self {
        Self { frame: [0; 23], wb: [[0; 12]; 3], regs: vec![0; 0x100 / 4 * 2], rawstat: 0, mask: 0, status: 0, jobs: 0 }
    }
}

#[derive(Default)]
struct Pmu {
    /// Power domains that are off (bit set = off): GP, L2, PP0-PP3.
    off: u32,
    rawstat: u32,
    mask: u32,
    sw_delay: u32,
}

#[derive(Default)]
struct L2 {
    regs: [u32; 16],
}

/// The whole GPU.
#[derive(Default)]
pub struct Mali400 {
    pub gp: Gp,
    pub pp: [Pp; NUM_PP],
    /// GP MMU, PP0 MMU, PP1 MMU.
    pub mmu: [Mmu; 1 + NUM_PP],
    pmu: Pmu,
    l2: L2,
    /// diagnostics already printed
    logged: HashSet<String>,
    /// Current time (24 MHz ticks), set by the machine before register accesses.
    pub now: u64,
    /// Jobs whose completion interrupt is still to come: (due tick, unit, raw status bits).
    pending: Vec<(u64, Unit, u32)>,
    /// Cycles spent on a PLBU list that stopped for memory.
    owed: u64,
    /// print every job (`SBC_MALI_LOG=1`)
    pub trace: bool,
}

fn log_to(logged: &mut HashSet<String>, msg: String) {
    if logged.insert(msg.clone()) {
        eprintln!("\r{msg}\r");
    }
}

impl Mali400 {
    /// `trace`: print a line per job.
    pub fn new(trace: bool) -> Self {
        Self { trace, ..Default::default() }
    }

    /// Read a register at `offset` in the window (word accesses).
    pub fn read(&mut self, offset: u64) -> u32 {
        let offset = offset & !3;
        match offset {
            0x0000..=0x0fff => self.gp_read(offset),
            0x1000..=0x1fff => self.l2_read(offset - 0x1000),
            0x2000..=0x2fff => self.pmu_read(offset - 0x2000),
            0x3000..=0x5fff => self.mmu[((offset - 0x3000) >> 12) as usize].read(offset & 0xfff),
            0x8000..=0xbfff => self.pp_read(((offset - 0x8000) >> 13) as usize, offset & 0x1fff),
            _ => 0,
        }
    }

    /// Write a register; jobs run to completion here, reading and writing `mem`.
    pub fn write(&mut self, offset: u64, value: u32, mem: &mut dyn Memory) {
        let offset = offset & !3;
        match offset {
            0x0000..=0x0fff => self.gp_write(offset, value, mem),
            0x1000..=0x1fff => self.l2_write(offset - 0x1000, value),
            0x2000..=0x2fff => self.pmu_write(offset - 0x2000, value),
            0x3000..=0x5fff => self.mmu[((offset - 0x3000) >> 12) as usize].write(offset & 0xfff, value),
            0x8000..=0xbfff => self.pp_write(((offset - 0x8000) >> 13) as usize, offset & 0x1fff, value, mem),
            _ => {}
        }
    }

    /// Deliver the completions due by `now`.
    pub fn update(&mut self, now: u64) {
        self.now = now;
        let mut k = 0;
        while k < self.pending.len() {
            let (due, unit, bits) = self.pending[k];
            if due <= now {
                self.pending.swap_remove(k);
                match unit {
                    Unit::Gp => {
                        self.gp.rawstat |= bits;
                        self.gp.status &= !(gp_reg::STATUS_VS_ACTIVE | gp_reg::STATUS_PLBU_ACTIVE);
                    }
                    Unit::Pp(n) => {
                        self.pp[n].rawstat |= bits;
                        self.pp[n].status &= !pp_reg::STATUS_RENDERING_ACTIVE;
                    }
                }
            } else {
                k += 1;
            }
        }
    }

    /// When the next job completes.
    pub fn next_deadline(&self) -> Option<u64> {
        self.pending.iter().map(|p| p.0).min()
    }

    /// Finish a job after the time its work takes on the real GPU.
    fn complete(&mut self, unit: Unit, bits: u32) {
        let cycles = std::mem::take(&mut self.owed) + pp::take_cycles();
        let due = self.now + (cycles * TICK_HZ / GPU_HZ).max(1);
        match unit {
            Unit::Gp => self.gp.status |= gp_reg::STATUS_VS_ACTIVE | gp_reg::STATUS_PLBU_ACTIVE,
            Unit::Pp(n) => self.pp[n].status |= pp_reg::STATUS_RENDERING_ACTIVE,
        }
        self.pending.push((due, unit, bits));
    }

    /// Level of an interrupt line.
    pub fn irq(&self, irq: Irq) -> bool {
        match irq {
            Irq::Gp => self.gp.rawstat & self.gp.mask != 0,
            Irq::GpMmu => self.mmu[0].irq(),
            Irq::Pp0 => self.pp[0].rawstat & self.pp[0].mask != 0,
            Irq::PpMmu0 => self.mmu[1].irq(),
            Irq::Pp1 => self.pp[1].rawstat & self.pp[1].mask != 0,
            Irq::PpMmu1 => self.mmu[2].irq(),
            Irq::Pmu => self.pmu.rawstat & self.pmu.mask != 0,
        }
    }

    fn gp_read(&self, offset: u64) -> u32 {
        use gp_reg::*;
        let gp = &self.gp;
        match offset {
            INT_RAWSTAT => gp.rawstat,
            INT_MASK => gp.mask,
            INT_STAT => gp.rawstat & gp.mask,
            STATUS => gp.status,
            VERSION => GP_VERSION,
            VSCL_START_READ => gp.regs[(VSCL_START / 4) as usize],
            PLBCL_START_READ => gp.regs[(PLBUCL_START / 4) as usize],
            _ if offset < 0x80 => gp.regs[(offset / 4) as usize],
            _ => 0,
        }
    }

    fn gp_write(&mut self, offset: u64, value: u32, mem: &mut dyn Memory) {
        use gp_reg::*;
        match offset {
            CMD => {
                if value & (CMD_SOFT_RESET | CMD_RESET) != 0 {
                    self.pending.retain(|p| p.1 != Unit::Gp);
                }
                if value & CMD_SOFT_RESET != 0 {
                    self.gp.status = 0;
                    self.gp.plbu = None;
                    self.gp.rawstat |= IRQ_RESET_COMPLETED;
                }
                if value & CMD_RESET != 0 {
                    self.gp.status = 0;
                    self.gp.plbu = None;
                    self.gp.rawstat = 0;
                }
                if value & CMD_UPDATE_PLBU_ALLOC != 0 && self.gp.plbu.is_some() {
                    self.plbu_resume(mem);
                }
                if value & CMD_STOP_BUS != 0 {
                    self.gp.status |= STATUS_BUS_STOPPED;
                }
                let (vs, plbu) = (value & CMD_START_VS != 0, value & CMD_START_PLBU != 0);
                if vs || plbu {
                    self.gp_run(vs, plbu, mem);
                }
            }
            INT_RAWSTAT => self.gp.rawstat |= value,
            INT_CLEAR => self.gp.rawstat &= !value,
            INT_MASK => self.gp.mask = value,
            INT_STAT | STATUS | VERSION => {}
            _ if offset < 0x80 => self.gp.regs[(offset / 4) as usize] = value,
            _ => {}
        }
    }

    /// Run the vertex and/or PLBU command lists of a GP job.
    fn gp_run(&mut self, vs: bool, plbu: bool, mem: &mut dyn Memory) {
        use gp_reg::*;
        self.gp.jobs += 1;
        let r = self.gp.regs;
        {
            let logged = &mut self.logged;
            let mut log = |m: String| log_to(logged, m);
            let mut vm = Vm::new(mem, &mut self.mmu[0]);
            if vs {
                self.gp.point_sizes = gp::run_vscl(&mut vm, r[(VSCL_START / 4) as usize], r[(VSCL_END / 4) as usize], &mut log);
            }
            if vm.faults != 0 {
                log(format!("[mali] GP: {} accesses hit unmapped GPU pages", vm.faults));
            }
        }
        if !plbu {
            self.complete(Unit::Gp, IRQ_VS_END);
        }
        if plbu {
            self.gp.vs_ran = vs;
            let sizes = std::mem::take(&mut self.gp.point_sizes);
            self.gp.plbu = Some(plbu::Plbu::new(r[(PLBUCL_START / 4) as usize], r[(PLBUCL_END / 4) as usize], (r[4], r[5]), sizes));
            self.plbu_run(mem, false);
        }
    }

    /// The driver grew the tile heap after OUT_OF_MEM: continue the PLBU list there.
    fn plbu_resume(&mut self, mem: &mut dyn Memory) {
        self.plbu_run(mem, true);
    }

    fn plbu_run(&mut self, mem: &mut dyn Memory, grow: bool) {
        use gp_reg::*;
        let Some(mut p) = self.gp.plbu.take() else { return };
        let heap = (self.gp.regs[4], self.gp.regs[5]);
        let done = {
            let logged = &mut self.logged;
            let mut log = |m: String| log_to(logged, m);
            let mut vm = Vm::new(mem, &mut self.mmu[0]);
            if grow {
                p.grow(&mut vm, heap);
            }
            let outcome = p.run(&mut vm, &mut log);
            if vm.faults != 0 {
                log(format!("[mali] PLBU: {} accesses hit unmapped GPU pages", vm.faults));
            }
            matches!(outcome, plbu::Outcome::Done)
        };
        if !done {
            // a stopped list's work so far counts when it completes: in this GPU's state, not the
            // thread's, which another machine or a snapshot does not share
            self.owed += pp::take_cycles();
        }
        if done {
            if self.trace {
                eprintln!("\r[mali] GP job {}: {} primitives\r", self.gp.jobs, p.prims);
            }
            let vs = if self.gp.vs_ran { IRQ_VS_END } else { 0 };
            self.complete(Unit::Gp, vs | IRQ_PLBU_END);
        } else {
            if self.trace {
                eprintln!("\r[mali] GP job {}: tile heap full at {} primitives\r", self.gp.jobs, p.prims);
            }
            self.gp.status |= STATUS_PLBU_ACTIVE;
            if self.gp.vs_ran {
                self.gp.rawstat |= IRQ_VS_END;
                self.gp.vs_ran = false;
            }
            self.gp.rawstat |= IRQ_PLBU_OUT_OF_MEM;
            self.gp.plbu = Some(p);
        }
    }

    fn pp_read(&self, n: usize, offset: u64) -> u32 {
        use pp_reg::*;
        let Some(pp) = self.pp.get(n) else { return 0 };
        match offset {
            0x000..=0x05c if offset / 4 < FRAME_REGS => pp.frame[(offset / 4) as usize],
            0x100..=0x3ff if offset & 0xff < WB_REGS * 4 => pp.wb[(offset / 0x100 - 1) as usize][(offset as usize & 0xff) / 4],
            VERSION => PP_VERSION,
            CURRENT_REND_LIST => pp.frame[0],
            STATUS => pp.status,
            INT_RAWSTAT => pp.rawstat,
            INT_MASK => pp.mask,
            INT_STATUS => pp.rawstat & pp.mask,
            0x1010..=0x101c | 0x1030..=0x10ff => pp.regs[((offset - 0x1000) / 4) as usize],
            _ => 0,
        }
    }

    fn pp_write(&mut self, n: usize, offset: u64, value: u32, mem: &mut dyn Memory) {
        use pp_reg::*;
        if n >= NUM_PP {
            return;
        }
        let pp = &mut self.pp[n];
        match offset {
            0x000..=0x05c if offset / 4 < FRAME_REGS => pp.frame[(offset / 4) as usize] = value,
            0x100..=0x3ff if offset & 0xff < WB_REGS * 4 => pp.wb[(offset / 0x100 - 1) as usize][(offset as usize & 0xff) / 4] = value,
            CTRL => {
                if value & (CTRL_SOFT_RESET | CTRL_FORCE_RESET) != 0 {
                    self.pending.retain(|p| p.1 != Unit::Pp(n));
                }
                let pp = &mut self.pp[n];
                if value & CTRL_SOFT_RESET != 0 {
                    pp.status = 0;
                    pp.rawstat = IRQ_RESET_COMPLETED;
                }
                if value & CTRL_FORCE_RESET != 0 {
                    pp.status = 0;
                    pp.rawstat = 0;
                }
                if value & CTRL_STOP_BUS != 0 {
                    pp.status |= STATUS_BUS_STOPPED;
                }
                if value & CTRL_START_RENDERING != 0 {
                    self.pp_run(n, mem);
                }
            }
            INT_RAWSTAT => pp.rawstat |= value,
            INT_CLEAR => pp.rawstat &= !value,
            INT_MASK => pp.mask = value,
            VERSION | STATUS | INT_STATUS | CURRENT_REND_LIST => {}
            0x1000..=0x10ff => pp.regs[((offset - 0x1000) / 4) as usize] = value,
            _ => {}
        }
    }

    /// Render the frame PP `n` was given.
    fn pp_run(&mut self, n: usize, mem: &mut dyn Memory) {
        let pp = &mut self.pp[n];
        pp.jobs += 1;
        let (frame, wb) = (pp.frame, pp.wb);
        let logged = &mut self.logged;
        let mut log = |m: String| log_to(logged, m);
        let mut vm = Vm::new(mem, &mut self.mmu[1 + n]);
        let mut programs = std::collections::HashMap::new();
        let tiles = pp::render(&mut vm, &frame, &wb, &mut programs, &mut log);
        if self.trace {
            eprintln!("\r[mali] PP{n} frame {}: {tiles} tiles {}x{} clear {:08x} wb0 {:08x?}\r", self.pp[n].jobs, frame[10] + 1, frame[11] + 1, frame[6], &wb[0][..7]);
        }
        if vm.faults != 0 {
            log(format!("[mali] PP{n}: {} accesses hit unmapped GPU pages", vm.faults));
        }
        self.complete(Unit::Pp(n), pp_reg::IRQ_END_OF_FRAME);
    }

    fn l2_read(&self, offset: u64) -> u32 {
        match offset {
            0x04 => L2_SIZE,
            0x08 => 0, // never busy
            _ if offset < 0x40 => self.l2.regs[(offset / 4) as usize],
            _ => 0,
        }
    }

    fn l2_write(&mut self, offset: u64, value: u32) {
        // 0x10 (COMMAND) and 0x14 (CLEAR_PAGE) complete at once: the model has no cache
        if offset < 0x40 && !matches!(offset, 0x04 | 0x08 | 0x10 | 0x14) {
            self.l2.regs[(offset / 4) as usize] = value;
        }
    }

    fn pmu_read(&self, offset: u64) -> u32 {
        let pmu = &self.pmu;
        match offset {
            0x08 => pmu.off,
            0x0c => pmu.mask,
            0x10 => pmu.rawstat,
            0x1c => pmu.sw_delay,
            _ => 0,
        }
    }

    fn pmu_write(&mut self, offset: u64, value: u32) {
        let pmu = &mut self.pmu;
        match offset {
            0x00 => {
                pmu.off &= !value;
                pmu.rawstat |= 1;
            }
            0x04 => {
                pmu.off |= value & 0x3f;
                pmu.rawstat |= 1;
            }
            0x0c => pmu.mask = value,
            0x18 => pmu.rawstat &= !value,
            0x1c => pmu.sw_delay = value,
            _ => {}
        }
    }
}

impl emu_core::snap::Snap for Unit {
    fn save(&self, w: &mut emu_core::snap::W) {
        w.u8(match *self { Unit::Gp => 0, Unit::Pp(k) => 1 + k as u8 });
    }
    fn load(&mut self, r: &mut emu_core::snap::R) -> Result<(), emu_core::snap::SnapError> {
        *self = match r.u8()? as usize {
            0 => Unit::Gp,
            k if k <= NUM_PP => Unit::Pp(k - 1),
            k => return Err(emu_core::snap::SnapError(format!("Mali-400 unit {k}"))),
        };
        Ok(())
    }
}
emu_core::snap_struct!(Gp { regs, rawstat, mask, status, jobs, plbu, point_sizes, vs_ran });
emu_core::snap_struct!(Pp { frame, wb, rawstat, mask, status, jobs } fixed { regs });
emu_core::snap_struct!(Pmu { off, rawstat, mask, sw_delay });
emu_core::snap_struct!(L2 { regs });
emu_core::snap_struct!(Mali400 { gp, pp, mmu, pmu, l2, now, pending, owed } skip { logged, trace });

#[cfg(test)]
mod tests {
    use super::*;

    /// Jobs on their way, a PLBU list that stopped for memory and what it has cost so far.
    #[test]
    fn unfinished_work_is_in_a_snapshot() {
        use emu_core::snap::Snap as _;
        let mut m = Mali400::default();
        m.pending = vec![(5000, Unit::Pp(1), 3), (4000, Unit::Gp, 1)];
        m.owed = 77;
        m.gp.plbu = Some(plbu::Plbu::new(0x1000, 0x2000, (0x3000, 0x4000), vec![(1, 2)]));
        m.gp.vs_ran = true;
        let mut w = emu_core::snap::W::default();
        m.save(&mut w);
        let mut back = Mali400::default();
        back.load(&mut emu_core::snap::R::new(&w.0)).unwrap();
        assert_eq!(back.pending, m.pending);
        assert_eq!((back.owed, back.next_deadline(), back.gp.vs_ran), (77, Some(4000), true));
        let mut again = emu_core::snap::W::default();
        back.save(&mut again);
        assert!(back.gp.plbu.is_some() && w.0 == again.0);
    }
}
