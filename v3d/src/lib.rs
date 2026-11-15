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
/// `SBC_V3D_DRAWS=1`: one line per distinct draw state (targets, blend, depth) and store format.
pub fn draws_enabled() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| std::env::var("SBC_V3D_DRAWS").map_or(false, |v| v != "0"))
}
pub fn log_enabled() -> bool { LOG_ON.load(std::sync::atomic::Ordering::Relaxed) }
/// A diagnostic printed once per message (always).
pub fn log_once(s: &str) {
    let first = SEEN.with(|h| h.borrow_mut().insert(s.to_string()));
    if first { eprintln!("\r{s}\r"); }
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
/// between jobs.
pub struct Code { tags: Vec<u32>, insts: Vec<qpu::Inst> }

const CODE_SLOTS: usize = 16384;

impl Default for Code {
    fn default() -> Self { Code { tags: vec![u32::MAX; CODE_SLOTS], insts: vec![qpu::Inst::Invalid(0); CODE_SLOTS] } }
}

impl Code {
    #[inline]
    pub fn fetch(&mut self, pc: u32, env: &mut dyn qpu::Env) -> qpu::Inst {
        let k = (pc >> 3) as usize & (CODE_SLOTS - 1);
        if self.tags[k] == pc { return self.insts[k]; }
        let w = env.read32(pc) as u64 | (env.read32(pc.wrapping_add(4)) as u64) << 32;
        let i = qpu::decode(w);
        self.tags[k] = pc;
        self.insts[k] = i;
        i
    }
    pub fn clear(&mut self) { self.tags.iter_mut().for_each(|t| *t = u32::MAX); tmu::clear_cache(); }
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
    /// statistics: jobs (bin, render, TFU, CSD), QPU instructions, host time in jobs (ns)
    pub jobs: [u64; 4],
    pub insns: u64,
    pub host_ns: [u64; 4],
    /// the PM block powered the GPU up (bcm2835-power, V3D domain)
    pub powered: bool,
    /// guest time (ns), as the SoC last told it
    pub now: u64,
    /// done interrupts not yet raised: (guest time, hub?, bits). A job's results are computed when
    /// it starts; its interrupt comes after the host time it took (scaled by `time_scale`), so the
    /// guest sees the emulated GPU's real speed.
    pending: Vec<(u64, bool, u32)>,
    pub time_scale: f64,
    /// guest time follows the host's wall clock (cores on host threads): the host time a job took
    /// has already passed for the guest while it ran, so the interrupt is not held back again
    pub wall_clock: bool,
    /// the bin jobs whose tile lists are live, to re-bin after a snapshot restore
    bin_jobs: HashMap<u32, [u32; 5]>,
}

impl Default for V3d { fn default() -> Self { Self::new() } }

impl V3d {
    pub fn new() -> V3d {
        LOG_ON.store(std::env::var_os("SBC_V3D_LOG").is_some(), std::sync::atomic::Ordering::Relaxed);
        if let Some(n) = std::env::var("SBC_V3D_TRACE").ok().and_then(|s| s.parse().ok()) { TRACE.store(n, std::sync::atomic::Ordering::Relaxed); }
        let mut v = V3d { hub: vec![0; 0x4000 / 4], core: vec![0; 0x4000 / 4], binned: HashMap::new(), code: Code::default(), jobs: [0; 4], insns: 0, host_ns: [0; 4], powered: true, now: 0, pending: Vec::new(),
            time_scale: std::env::var("SBC_GPU_TIME").ok().and_then(|s| s.parse().ok()).unwrap_or(1.0), wall_clock: false, bin_jobs: HashMap::new() };
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
        self.binned.clear();
        self.pending.clear();
        self.bin_jobs.clear();
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
                    let t0 = Clock::now();
                    let regs: Vec<u32> = (0..16).map(|k| self.hub[TFU_CS / 4 + k]).collect();
                    let mut m = Gmem::new(ram, self.pt_base(), self.mmu_on());
                    tfu::run(&regs, &mut m);
                    self.jobs[2] += 1;
                    let ns = t0.elapsed().as_nanos() as u64;
                    self.host_ns[2] += ns;
                    if v & 1 != 0 { self.done(true, HUB_INT_TFUC, ns); }
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
                    let t0 = Clock::now();
                    let (start, end) = (self.core[CLE_CT0QBA / 4], v);
                    let (qma, qms, qts) = (self.core[CLE_CT0QMA / 4], self.core[CLE_CT0QMS / 4], self.core[CLE_CT0QTS / 4]);
                    self.code.clear();
                    let mut m = Gmem::new(ram, self.pt_base(), self.mmu_on());
                    let res = bin::run(start, end, qma, qms, qts, &mut m, &mut self.code, &mut self.insns);
                    if let Some(r) = res { self.binned.insert(qma, std::sync::Arc::new(r)); self.bin_jobs.insert(qma, [start, end, qma, qms, qts]); }
                    self.core[CLE_CT0CA / 4] = end;
                    self.core[CLE_BFC / 4] = self.core[CLE_BFC / 4].wrapping_add(1);
                    self.jobs[0] += 1;
                    let ns = t0.elapsed().as_nanos() as u64;
                    self.host_ns[0] += ns;
                    self.done(false, INT_FLDONE, ns);
                }
                CLE_CT1QEA => {
                    self.core[o / 4] = v;
                    let t0 = Clock::now();
                    let (start, end) = (self.core[CLE_CT1QBA / 4], v);
                    self.code.clear();
                    let mut m = Gmem::new(ram, self.pt_base(), self.mmu_on());
                    // after a snapshot restore: rebuild the tile lists the render job will read
                    if self.binned.len() < self.bin_jobs.len() {
                        for (q, j) in self.bin_jobs.clone() {
                            if self.binned.contains_key(&q) { continue; }
                            if let Some(r) = bin::run(j[0], j[1], j[2], j[3], j[4], &mut m, &mut self.code, &mut self.insns) { self.binned.insert(q, std::sync::Arc::new(r)); }
                        }
                    }
                    render::run(start, end, &self.binned, &mut m, &mut self.code, &mut self.insns);
                    self.core[CLE_CT1CA / 4] = end;
                    self.core[CLE_RFC / 4] = self.core[CLE_RFC / 4].wrapping_add(1);
                    self.jobs[1] += 1;
                    let ns = t0.elapsed().as_nanos() as u64;
                    self.host_ns[1] += ns;
                    self.done(false, INT_FRDONE, ns);
                }
                CSD_QUEUED_CFG0 => {
                    self.core[o / 4] = v;
                    let t0 = Clock::now();
                    let cfg: Vec<u32> = (0..7).map(|k| self.core[CSD_QUEUED_CFG0 / 4 + k]).collect();
                    for k in 0..7 { self.core[CSD_CURRENT_CFG0 / 4 + k] = cfg[k]; }
                    self.code.clear();
                    let mut m = Gmem::new(ram, self.pt_base(), self.mmu_on());
                    csd::run(&cfg, &mut m, &mut self.code, &mut self.insns);
                    self.jobs[3] += 1;
                    let ns = t0.elapsed().as_nanos() as u64;
                    self.host_ns[3] += ns;
                    self.done(false, INT_CSDDONE, ns);
                }
                _ => self.core[o / 4] = v,
            }
        }
    }

    /// One-line job statistics.
    pub fn stats(&self) -> String {

        let ms = |k: usize| self.host_ns[k] as f64 / 1e6;
        format!("{} bin jobs ({:.0} ms), {} render jobs ({:.0} ms), {} TFU ({:.0} ms), {} compute ({:.0} ms); {} QPU instructions",
                self.jobs[0], ms(0), self.jobs[1], ms(1), self.jobs[2], ms(2), self.jobs[3], ms(3), self.insns)
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
        w.u64(self.pending.len() as u64);
        for &(t, h, b) in &self.pending { w.u64(t); w.bool(h); w.u32(b); }
        w.u64(self.bin_jobs.len() as u64);
        for j in self.bin_jobs.values() { for &x in j { w.u32(x); } }
    }
    pub fn load_state(&mut self, r: &mut emu_core::snap::R) -> Result<(), emu_core::snap::SnapError> {
        r.tag("v3d-v1")?;
        for k in 0..self.hub.len() { self.hub[k] = r.u32()?; }
        for k in 0..self.core.len() { self.core[k] = r.u32()?; }
        self.binned.clear();
        self.pending.clear();
        for _ in 0..r.u64()? { let (t, h, b) = (r.u64()?, r.bool()?, r.u32()?); self.pending.push((t, h, b)); }
        self.bin_jobs.clear();
        for _ in 0..r.u64()? { let mut j = [0u32; 5]; for x in j.iter_mut() { *x = r.u32()?; } self.bin_jobs.insert(j[2], j); }
        Ok(())
    }
}
