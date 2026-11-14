//! Broadcom V3D 4.2 (the VideoCore VI 3D GPU of the BCM2711), as driven by the stock Linux
//! `v3d` kernel driver and Mesa's `v3d`/`v3dv` userspace drivers.
//!
//! * hub registers at 0x0000 (identification, interrupts, TFU, MMU) and core 0 at 0x4000
//!   (control list executors, PTB, CSD, caches, performance counters);
//! * the GPU's own MMU: one level of 4 KiB page table entries for a 4 GiB space;
//! * jobs run to completion when the kernel starts them (writing the end address of a
//!   control list, the TFU's ICFG or the CSD's CFG0), then raise their done interrupt:
//!   binning (`bin`), rendering (`render`), texture formatting (`tfu`), compute (`csd`).

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
pub mod raster;

use std::collections::HashMap;
pub use mem::Gmem;

static LOG_ON: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
/// QPU instructions still to trace (`SBC_V3D_TRACE=N`)
pub static TRACE: std::sync::atomic::AtomicI64 = std::sync::atomic::AtomicI64::new(0);
thread_local! { static SEEN: std::cell::RefCell<std::collections::HashSet<String>> = Default::default(); }

/// Diagnostics on stderr (`SBC_V3D_LOG=1`).
pub fn log(s: &str) { if LOG_ON.load(std::sync::atomic::Ordering::Relaxed) { eprintln!("\r{s}\r"); } }
pub fn log_enabled() -> bool { LOG_ON.load(std::sync::atomic::Ordering::Relaxed) }
/// A diagnostic printed once per message (always).
pub fn log_once(s: &str) {
    let first = SEEN.with(|h| h.borrow_mut().insert(s.to_string()));
    if first { eprintln!("\r{s}\r"); }
}

/// Decoded-instruction cache for QPU code, valid for one job (BOs get remapped between jobs).
#[derive(Default)]
pub struct Code { cache: HashMap<u32, qpu::Inst> }

impl Code {
    pub fn fetch(&mut self, pc: u32, env: &mut dyn qpu::Env) -> qpu::Inst {
        if let Some(i) = self.cache.get(&pc) { return *i; }
        let w = env.read32(pc) as u64 | (env.read32(pc.wrapping_add(4)) as u64) << 32;
        let i = qpu::decode(w);
        self.cache.insert(pc, i);
        i
    }
    pub fn clear(&mut self) { self.cache.clear(); }
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

pub struct V3d {
    hub: Vec<u32>,
    core: Vec<u32>,
    pub binned: BinResults,
    pub code: Code,
    /// statistics
    pub jobs: [u64; 4],
    pub insns: u64,
    /// the PM block powered the GPU up (bcm2835-power, V3D domain)
    pub powered: bool,
}

impl Default for V3d { fn default() -> Self { Self::new() } }

impl V3d {
    pub fn new() -> V3d {
        LOG_ON.store(std::env::var_os("SBC_V3D_LOG").is_some(), std::sync::atomic::Ordering::Relaxed);
        if let Some(n) = std::env::var("SBC_V3D_TRACE").ok().and_then(|s| s.parse().ok()) { TRACE.store(n, std::sync::atomic::Ordering::Relaxed); }
        let mut v = V3d { hub: vec![0; 0x4000 / 4], core: vec![0; 0x4000 / 4], binned: HashMap::new(), code: Code::default(), jobs: [0; 4], insns: 0, powered: true };
        v.reset();
        v
    }

    pub fn reset(&mut self) {
        self.hub.iter_mut().for_each(|r| *r = 0);
        self.core.iter_mut().for_each(|r| *r = 0);
        self.hub[HUB_AXICFG / 4] = 0xf;
        self.hub[HUB_INT_MSK_STS / 4] = !0;
        self.core[CTL_INT_MSK_STS / 4] = !0;
        self.binned.clear();
    }

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
                    let mut m = Gmem::new(ram, self.pt_base(), self.mmu_on());
                    tfu::run(&regs, &mut m);
                    self.jobs[2] += 1;
                    if v & 1 != 0 { self.hub[HUB_INT_STS / 4] |= HUB_INT_TFUC; }
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
                    self.code.clear();
                    let mut m = Gmem::new(ram, self.pt_base(), self.mmu_on());
                    let res = bin::run(start, end, qma, qms, qts, &mut m, &mut self.code, &mut self.insns);
                    if let Some(r) = res { self.binned.insert(qma, std::sync::Arc::new(r)); }
                    self.core[CLE_CT0CA / 4] = end;
                    self.core[CLE_BFC / 4] = self.core[CLE_BFC / 4].wrapping_add(1);
                    self.jobs[0] += 1;
                    self.core[CTL_INT_STS / 4] |= INT_FLDONE;
                }
                CLE_CT1QEA => {
                    self.core[o / 4] = v;
                    let (start, end) = (self.core[CLE_CT1QBA / 4], v);
                    self.code.clear();
                    let mut m = Gmem::new(ram, self.pt_base(), self.mmu_on());
                    render::run(start, end, &self.binned, &mut m, &mut self.code, &mut self.insns);
                    self.core[CLE_CT1CA / 4] = end;
                    self.core[CLE_RFC / 4] = self.core[CLE_RFC / 4].wrapping_add(1);
                    self.jobs[1] += 1;
                    self.core[CTL_INT_STS / 4] |= INT_FRDONE;
                }
                CSD_QUEUED_CFG0 => {
                    self.core[o / 4] = v;
                    let cfg: Vec<u32> = (0..7).map(|k| self.core[CSD_QUEUED_CFG0 / 4 + k]).collect();
                    for k in 0..7 { self.core[CSD_CURRENT_CFG0 / 4 + k] = cfg[k]; }
                    self.code.clear();
                    let mut m = Gmem::new(ram, self.pt_base(), self.mmu_on());
                    csd::run(&cfg, &mut m, &mut self.code, &mut self.insns);
                    self.jobs[3] += 1;
                    self.core[CTL_INT_STS / 4] |= INT_CSDDONE;
                }
                _ => self.core[o / 4] = v,
            }
        }
    }

    fn mmu_on(&self) -> bool { self.hub[MMU_CTL / 4] & 1 != 0 }
    fn pt_base(&self) -> u64 { (self.hub[MMU_PT_PA_BASE / 4] as u64) << 12 }

    /// Level of the interrupt line.
    pub fn irq(&self) -> bool {
        (self.core[CTL_INT_STS / 4] & !self.core[CTL_INT_MSK_STS / 4]) != 0
            || (self.hub[HUB_INT_STS / 4] & !self.hub[HUB_INT_MSK_STS / 4]) != 0
    }

    pub fn save_state(&self, w: &mut emu_core::snap::W) {
        w.tag("v3d-v1");
        for r in self.hub.iter().chain(self.core.iter()) { w.u32(*r); }
    }
    pub fn load_state(&mut self, r: &mut emu_core::snap::R) -> Result<(), emu_core::snap::SnapError> {
        r.tag("v3d-v1")?;
        for k in 0..self.hub.len() { self.hub[k] = r.u32()?; }
        for k in 0..self.core.len() { self.core[k] = r.u32()?; }
        self.binned.clear();
        Ok(())
    }
}
