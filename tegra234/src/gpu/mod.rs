//! The Ampere GA10B register aperture, as driven by Jetson Linux's nvgpu.
//!
//! NVIDIA's GA10B HAL uses GM20B's BOOT_0 decoder and GK20A's PTIMER
//! read path. Time values are nanoseconds on the SoC virtual clock.

use std::collections::HashMap;

mod compute;
mod copy;
mod fb;
mod fuse;
mod gr;
mod gsp;
mod host;
mod mme;
mod mmu;
mod pmu;
mod threed;

mod top;
mod twod;

pub const BAR0: u64 = 0x1700_0000;
const FECS_CURRENT_CTX: u64 = 0x409b00;
pub const BAR1: u64 = 0x1800_0000;
pub const SIZE: u64 = 0x0100_0000;
pub const FUSE: u64 = 0x03b4_1000;
pub const FUSE_SIZE: u64 = 0x1000;
pub const SPIS: [u32; 4] = [68, 70, GSP_STALL_SPI, NONSTALL_SPI];
pub(crate) const NONSTALL_SPI: u32 = 67;
pub(crate) const GSP_STALL_SPI: u32 = 71;

// The host-class completion notification uses the non-stall base vector.
const COPY_NONSTALL_LEAF: u32 = 1;
const NONSTALL_SUBTREE: u32 = 1;
const NONSTALL_STATUS: u64 = 0xb81000;
const NONSTALL_ENABLE_SET: u64 = 0xb81200;
const NONSTALL_ENABLE_CLEAR: u64 = 0xb81400;
const GSP_LEAF: u32 = 1 << (155 - 128);
/// The PMU's stall vector (152) in the same leaf as the GSP's.
const PMU_LEAF: u32 = 1 << (152 - 128);
const GSP_SUBTREE: u32 = 1 << 2;
const INTR_LEAF_STATUS: u64 = 0xb81010;
const INTR_LEAF_ENABLE_SET: u64 = 0xb81210;
const INTR_LEAF_ENABLE_CLEAR: u64 = 0xb81410;
const INTR_TOP_STATUS: u64 = 0xb81600;
const INTR_TOP_ENABLE_SET: u64 = 0xb81608;
const INTR_TOP_ENABLE_CLEAR: u64 = 0xb81610;

/// PMC_BOOT_0 encodes architecture 0x17, implementation 0xb and revision A1.
const BOOT0: u32 = 0x17b0_00a1;
const BAR1_BIND: u64 = 0x001704;
const BAR1_STATUS: u64 = 0x001710;
const BAR1_BIND_VALID: u32 = 1 << 31;
const BAR1_INSTANCE_MASK: u32 = 0x0fff_ffff;
const PTIMER_TIME_0: u64 = 0x009400;
const PTIMER_TIME_1: u64 = 0x009410;
const RING_COMMAND: u64 = 0x12004c;
const RING_START_RESULTS: u64 = 0x120050;
const RING_COMMAND_MASK: u32 = 0x3f;
const RING_ENUMERATE_AND_START: u32 = 4;
const RING_CONNECTIVITY_PASS: u32 = 1;

pub struct Gpu {
    regs: HashMap<u64, u32>,
    fb: fb::Fb,
    gr: gr::Gr,
    gsp: gsp::Gsp,
    host: host::Host,
    pmu: pmu::Pmu,
    /// Production silicon (secure fuses, PMU) rather than NVIDIA's QT pre-silicon platform.
    silicon: bool,
    /// The ACR has set up the WPR and verified the light-secure images (`run_acr`).
    acr_done: bool,
    ring_started: bool,
    gsp_leaf_enabled: bool,
    pmu_leaf_enabled: bool,
    gsp_subtree_enabled: bool,
    nonstall_leaf_enabled: bool,
    nonstall_subtree_enabled: bool,
    trace: bool,
}

impl Default for Gpu {
    fn default() -> Self {
        Self {
            regs: HashMap::new(),
            fb: fb::Fb::default(),
            gr: gr::Gr::default(),
            gsp: gsp::Gsp::default(),
            host: host::Host::default(),
            pmu: pmu::Pmu::default(),
            silicon: false,
            acr_done: false,
            ring_started: false,
            gsp_leaf_enabled: false,
            pmu_leaf_enabled: false,
            gsp_subtree_enabled: false,
            nonstall_leaf_enabled: false,
            nonstall_subtree_enabled: false,
            trace: std::env::var_os("SBC_TRACE_GPU").is_some(),
        }
    }
}

impl Gpu {
    /// Report production silicon (secure fuses, LS PMU) instead of the QT platform.
    pub fn set_silicon(&mut self, silicon: bool) {
        self.silicon = silicon;
    }

    pub fn read(&mut self, offset: u64, now: u64, ram: &mut [u8]) -> u32 {
        let value = match offset {
            0 => BOOT0,
            INTR_LEAF_STATUS => {
                (u32::from(self.gsp.irq()) * GSP_LEAF) | (u32::from(self.pmu.irq()) * PMU_LEAF)
            }
            NONSTALL_STATUS => u32::from(self.host.nonstall_pending) * COPY_NONSTALL_LEAF,
            INTR_TOP_STATUS => {
                (u32::from(self.gsp_pending()) * GSP_SUBTREE)
                    | (u32::from(self.nonstall_pending()) * NONSTALL_SUBTREE)
            }
            BAR1_STATUS => 0, // BAR1 binds complete synchronously.
            PTIMER_TIME_0 => now as u32,
            PTIMER_TIME_1 => (now >> 32) as u32,
            RING_COMMAND => 0, // The supported command completes on write.
            RING_START_RESULTS => u32::from(self.ring_started) * RING_CONNECTIVITY_PASS,
            // gr_fecs_current_ctx: the resident context (pointer in 4 KiB units, valid bit 31), so
            // that nvgpu reads and writes a running channel's context registers directly
            FECS_CURRENT_CTX => self.host.gr_instance.map_or(0, |i| (i >> 12) as u32 & 0x0fff_ffff | 1 << 31),
            _ if self.silicon && fuse::contains(offset) => fuse::read(offset).unwrap_or(0),
            _ => self
                .fb
                .read(offset)
                .or_else(|| self.pmu.read(offset, ram))
                .or_else(|| top::read(offset))
                .or_else(|| host::read_config(offset))
                .or_else(|| self.gr.read(offset))
                .or_else(|| self.gsp.read(offset))
                .unwrap_or_else(|| self.regs.get(&offset).copied().unwrap_or(0)),
        };
        if self.trace && !matches!(offset, PTIMER_TIME_0 | PTIMER_TIME_1) {
            eprintln!("[gpu] read {offset:06x} = {value:08x}");
        }
        value
    }

    pub(crate) fn write(
        &mut self,
        offset: u64,
        value: u32,
        ram: &mut [u8],
        host1x: &mut crate::host1x::Host1x,
        now: u64,
    ) {
        if self.trace
            && !gr::Gr::firmware_data_port(offset)
            && !gsp::Gsp::firmware_data_port(offset)
        {
            eprintln!("[gpu] write {offset:06x} <- {value:08x}");
        }
        if self.fb.write(offset, value)
            || self.gr.write(offset, value)
            || self.gsp.write(offset, value)
        {
            if let Some(desc) = self.gsp.take_acr_request() {
                self.run_acr(desc, ram);
            }
            return;
        }
        let mut effects = Vec::new();
        self.pmu.work = self.host.submissions;
        if self.pmu.write(offset, value, ram, self.acr_done, &mut effects) {
            for effect in effects {
                match effect {
                    pmu::Effect::BootstrapFalcons(mask) => self.gr.bootstrap_light_secure(mask),
                }
            }
            return;
        }
        match offset {
            INTR_LEAF_ENABLE_SET => {
                self.gsp_leaf_enabled |= value & GSP_LEAF != 0;
                self.pmu_leaf_enabled |= value & PMU_LEAF != 0;
            }
            INTR_LEAF_ENABLE_CLEAR => {
                self.gsp_leaf_enabled &= value & GSP_LEAF == 0;
                self.pmu_leaf_enabled &= value & PMU_LEAF == 0;
            }
            NONSTALL_STATUS => self.host.nonstall_pending &= value & COPY_NONSTALL_LEAF == 0,
            NONSTALL_ENABLE_SET => self.nonstall_leaf_enabled |= value & COPY_NONSTALL_LEAF != 0,
            NONSTALL_ENABLE_CLEAR => self.nonstall_leaf_enabled &= value & COPY_NONSTALL_LEAF == 0,
            INTR_TOP_ENABLE_SET => {
                self.gsp_subtree_enabled |= value & GSP_SUBTREE != 0;
                self.nonstall_subtree_enabled |= value & NONSTALL_SUBTREE != 0;
            }
            INTR_TOP_ENABLE_CLEAR => {
                self.gsp_subtree_enabled &= value & GSP_SUBTREE == 0;
                self.nonstall_subtree_enabled &= value & NONSTALL_SUBTREE == 0;
            }
            INTR_LEAF_STATUS | INTR_TOP_STATUS => {}
            RING_COMMAND => {
                if value & RING_COMMAND_MASK == RING_ENUMERATE_AND_START {
                    self.ring_started = true;
                }
            }
            host::SUBMIT | host::GR_SUBMIT => self.host.submit(
                usize::from(offset == host::GR_SUBMIT),
                self.regs.get(&(offset - 8)).copied().unwrap_or(0),
                self.regs.get(&(offset - 4)).copied().unwrap_or(0),
                value & 0xffff,
                ram,
            ),
            host::DOORBELL | host::LEGACY_DEMO_DOORBELL => {
                if self
                    .host
                    .ring(value, ram, host1x, self.trace, now)
                    .is_none()
                    && self.trace
                {
                    eprintln!("[gpu] doorbell {value:#x} could not complete");
                }
                self.host.retry_blocked(ram, host1x, self.trace, now);
            }
            0 | BAR1_STATUS | PTIMER_TIME_0 | PTIMER_TIME_1 | RING_START_RESULTS => {}
            _ if top::read(offset).is_none() && host::read_config(offset).is_none() => {
                self.regs.insert(offset, value);
            }
            _ => {}
        }
    }

    fn bar1_view<'a>(&self, ram: &'a mut [u8]) -> Option<mmu::View<'a>> {
        let bind = self.regs.get(&BAR1_BIND).copied()?;
        if bind & BAR1_BIND_VALID == 0 {
            return None;
        }
        let instance = u64::from(bind & BAR1_INSTANCE_MASK) << 12;
        mmu::View::from_instance(ram, instance)
    }

    pub fn bar1_read(&self, offset: u64, ram: &mut [u8]) -> u32 {
        self.bar1_view(ram)
            .and_then(|view| view.load(offset))
            .unwrap_or(0)
    }

    pub fn bar1_write(&self, offset: u64, value: u32, ram: &mut [u8]) {
        if let Some(mut view) = self.bar1_view(ram) {
            view.store(offset, value);
        }
    }

    fn gsp_pending(&self) -> bool {
        self.gsp.irq() && self.gsp_leaf_enabled || self.pmu.irq() && self.pmu_leaf_enabled
    }

    /// The ACR (`acr-gsp.*`, on the GSP's RISC-V core) at the level of its result. It reads its
    /// descriptor (`flcn2_acr_desc`: the non-WPR ucode blob nvgpu built), would copy the blob
    /// into the WPR carveout and check each light-secure falcon's signature, then halts with 0 in
    /// mailbox 0. Here the blob's WPR header table is walked: the run succeeds when it lists
    /// FECS and GPCCS (and nothing unreadable); signatures are taken as valid.
    fn run_acr(&mut self, desc: u64, ram: &mut [u8]) {
        const BLOB_SIZE: u64 = 68;
        const BLOB_START: u64 = 72;
        const WPR_HEADER: u64 = 24;
        const FALCON_ID_INVALID: u32 = u32::MAX;
        const FECS: u32 = 2;
        const GPCCS: u32 = 3;
        // ACR_ERROR codes are nonzero; 0x13 stands for a blob it could not parse.
        const BLOB_ERROR: u32 = 0x13;
        let word = |address: u64| mmu::word(ram, address);
        let size = word(desc + BLOB_SIZE);
        let start = word(desc + BLOB_START)
            .zip(word(desc + BLOB_START + 4))
            .map(|(lo, hi)| u64::from(lo) | u64::from(hi) << 32);
        let mut falcons = 0u32;
        if let (Some(size), Some(start)) = (size, start) {
            for index in 0..u64::from(size) / WPR_HEADER {
                match word(start + index * WPR_HEADER) {
                    Some(FALCON_ID_INVALID) | None => break,
                    Some(id) if id < 32 => falcons |= 1 << id,
                    Some(_) => {}
                }
            }
        }
        self.acr_done = falcons & (1 << FECS | 1 << GPCCS) == (1 << FECS | 1 << GPCCS);
        if self.trace {
            eprintln!("[gpu] ACR desc {desc:#x}: WPR falcons {falcons:#x}");
        }
        if self.acr_done {
            self.gsp.finish_acr(0, 0);
        } else {
            self.gsp.finish_acr(BLOB_ERROR, 0);
        }
    }

    pub fn stall_irq(&self) -> bool {
        self.gsp_pending() && self.gsp_subtree_enabled
    }

    fn nonstall_pending(&self) -> bool {
        self.host.nonstall_pending && self.nonstall_leaf_enabled
    }

    pub fn nonstall_irq(&self) -> bool {
        self.nonstall_pending() && self.nonstall_subtree_enabled
    }

    pub fn reset(&mut self) {
        self.regs.clear();
        self.fb = fb::Fb::default();
        self.gr = gr::Gr::default();
        self.gsp = gsp::Gsp::default();
        self.host = host::Host::default();
        self.pmu = pmu::Pmu::default();
        self.acr_done = false;
        self.ring_started = false;
        self.gsp_leaf_enabled = false;
        self.pmu_leaf_enabled = false;
        self.gsp_subtree_enabled = false;
        self.nonstall_leaf_enabled = false;
        self.nonstall_subtree_enabled = false;
    }

    pub fn submissions(&self) -> u64 {
        self.host.submissions
    }
}
