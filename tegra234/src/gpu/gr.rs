//! GA10B graphics-cluster reset and context-switch firmware handshake.
//! Respond to the boot and size queries used by nvgpu, as with Tegra210's FECS
//! model. Size responses reserve guest buffers for the modeled topology.
use super::top::TPCS_PER_GPC;

const GPC_RESET_BROADCAST: u64 = 0x41a610;
const GPC0_RESET_STATUS: u64 = 0x502610;
const RESET_RELEASED: u32 = 1;
const GPC0_TPCS: u64 = 0x502608;
const GPC0_TPC0_SM_ARCH: u64 = 0x504330;
// Each version has an eight-bit minor field.
const SM87: u32 = (8 << 8) | 7;
const GA10B_SM_ARCH: u32 = (SM87 << 20) | (SM87 << 8) | 48;
const GPC0_ZCULL_BANKS: u64 = 0x500920;
const GPC0_PES0_TPC_MASK: u64 = 0x500c30;
const FE_POWER_MODE: u64 = 0x404170;
const FE_POWER_MODE_REQUEST: u32 = 1 << 4;
const FECS_CPUCTL: u64 = 0x409100;
/// The CPU control alias, the only one writable once a falcon runs light-secure.
const FECS_CPUCTL_ALIAS: u64 = 0x409130;
const GPCCS_CPUCTL_ALIAS: u64 = 0x41a130;
const FECS_MAILBOX4: u64 = 0x409810;
const FECS_IMEM_DATA: u64 = 0x409184;
const FECS_DMEM_DATA: u64 = 0x4091c4;
const FECS_METHOD_DATA: u64 = 0x409500;
const FECS_METHOD_PUSH: u64 = 0x409504;
const FECS_MAILBOX0: u64 = 0x409800;
const FECS_MAILBOX1: u64 = 0x409804;
const GPCCS_CPUCTL: u64 = 0x41a100;
const GPCCS_IMEM_DATA: u64 = 0x41a184;
const GPCCS_DMEM_DATA: u64 = 0x41a1c4;
const CPU_START: u32 = 1 << 1;
const UCODE_INIT_COMPLETE: u32 = 1;
const BIND_CHANNEL_CONTEXT: u32 = 0x03;
const SAVE_GOLDEN_CONTEXT: u32 = 0x09;
const CONTEXT_BOUND: u32 = 1 << 4;
const CONTEXT_SAVED: u32 = 1;
const DISCOVER_CONTEXT_SIZE: u32 = 0x10;
const DISCOVER_ZCULL_SIZE: u32 = 0x16;
const DISCOVER_PREEMPT_SIZE: u32 = 0x1a;
const DISCOVER_PM_SIZE: u32 = 0x25;
/// Context switching stopped and started again (nvgpu around L2 and other global changes); the
/// driver waits for 1 in mailbox 1.
const STOP_CTXSW: u32 = 0x38;
const START_CTXSW: u32 = 0x39;
/// ELPG's register list: its size, then binding the PMU's buffer (instance, virtual address,
/// passed in mailbox 4); the binds complete with 1 in mailbox 0.
const DISCOVER_REGLIST_SIZE: u32 = 0x30;
const SET_REGLIST_BIND_INSTANCE: u32 = 0x31;
const SET_REGLIST_VIRTUAL_ADDRESS: u32 = 0x32;
const REGLIST_BYTES: u32 = 0x8000;
const FALCON_ID_FECS: u32 = 2;
const FALCON_ID_GPCCS: u32 = 3;
const CONTEXT_BYTES: u32 = 0x20_000;
const ZCULL_BYTES: u32 = 0x1000;
// Reserve space for the R36 netlist's aligned SYS, FBP and GPC HWPM register lists.
const PM_BYTES: u32 = 0x20_000;

#[derive(Default)]
pub(super) struct Gr {
    released: u32,
    fecs_uploaded: bool,
    gpccs_uploaded: bool,
    gpccs_started: bool,
    fecs_started: bool,
    mailbox0: u32,
    mailbox1: u32,
    mailbox4: u32,
    method_data: u32,
    fe_power_mode: u32,
    context_instance: Option<u32>,
}

impl Gr {
    /// The PMU's ACR unit loaded the light-secure FECS and GPCCS images from the WPR (a mask of
    /// falcon IDs); nvgpu then starts them through their CPU control aliases.
    pub fn bootstrap_light_secure(&mut self, falcons: u32) {
        self.fecs_uploaded |= falcons & (1 << FALCON_ID_FECS) != 0;
        self.gpccs_uploaded |= falcons & (1 << FALCON_ID_GPCCS) != 0;
    }

    pub fn firmware_data_port(offset: u64) -> bool {
        matches!(
            offset,
            FECS_IMEM_DATA | FECS_DMEM_DATA | GPCCS_IMEM_DATA | GPCCS_DMEM_DATA
        )
    }

    pub fn read(&self, offset: u64) -> Option<u32> {
        match offset {
            GPC_RESET_BROADCAST | GPC0_RESET_STATUS => Some(self.released),
            GPC0_TPC0_SM_ARCH => Some(GA10B_SM_ARCH),
            GPC0_TPCS => Some(TPCS_PER_GPC | TPCS_PER_GPC << 16),
            GPC0_ZCULL_BANKS => Some(TPCS_PER_GPC),
            GPC0_PES0_TPC_MASK => Some((1 << TPCS_PER_GPC) - 1),
            FE_POWER_MODE => Some(self.fe_power_mode),
            FECS_MAILBOX0 => Some(self.mailbox0),
            FECS_MAILBOX1 => Some(self.mailbox1),
            FECS_MAILBOX4 => Some(self.mailbox4),
            FECS_METHOD_DATA => Some(self.method_data),
            _ => None,
        }
    }

    pub fn write(&mut self, offset: u64, value: u32) -> bool {
        match offset {
            GPC_RESET_BROADCAST => {
                if value & RESET_RELEASED == 0 {
                    *self = Self::default();
                }
                self.released = value & RESET_RELEASED;
            }
            GPC0_RESET_STATUS | GPC0_TPCS | GPC0_ZCULL_BANKS | GPC0_PES0_TPC_MASK => {}
            FE_POWER_MODE => self.fe_power_mode = value & !FE_POWER_MODE_REQUEST,
            GPCCS_IMEM_DATA => self.gpccs_uploaded = true,
            FECS_IMEM_DATA => self.fecs_uploaded = true,
            GPCCS_DMEM_DATA | FECS_DMEM_DATA => {}
            GPCCS_CPUCTL | FECS_CPUCTL | GPCCS_CPUCTL_ALIAS | FECS_CPUCTL_ALIAS => {
                let was_running = self.fecs_started && self.gpccs_started;
                let start = self.released != 0 && value & CPU_START != 0;
                if matches!(offset, FECS_CPUCTL | FECS_CPUCTL_ALIAS) {
                    self.fecs_started = self.fecs_uploaded && start;
                } else {
                    self.gpccs_started = self.gpccs_uploaded && start;
                }
                if !was_running && self.fecs_started && self.gpccs_started {
                    self.mailbox0 = UCODE_INIT_COMPLETE;
                }
            }
            FECS_METHOD_DATA => self.method_data = value,
            FECS_METHOD_PUSH if self.fecs_started && self.gpccs_started => match value & 0xfff {
                BIND_CHANNEL_CONTEXT if self.method_data & (1 << 31) != 0 => {
                    self.context_instance = Some(self.method_data);
                    self.mailbox0 |= CONTEXT_BOUND;
                }
                SAVE_GOLDEN_CONTEXT if self.context_instance.is_some() => {
                    self.mailbox0 |= CONTEXT_SAVED;
                }
                DISCOVER_CONTEXT_SIZE | DISCOVER_PREEMPT_SIZE => self.mailbox0 = CONTEXT_BYTES,
                DISCOVER_ZCULL_SIZE => self.mailbox0 = ZCULL_BYTES,
                DISCOVER_PM_SIZE => self.mailbox0 = PM_BYTES,
                STOP_CTXSW | START_CTXSW => self.mailbox1 = 1,
                DISCOVER_REGLIST_SIZE => self.mailbox0 = REGLIST_BYTES,
                SET_REGLIST_BIND_INSTANCE | SET_REGLIST_VIRTUAL_ADDRESS => self.mailbox0 = 1,
                _ => {}
            },
            FECS_METHOD_PUSH => {}
            FECS_MAILBOX0 => self.mailbox0 = value,
            FECS_MAILBOX1 => self.mailbox1 = value,
            FECS_MAILBOX4 => self.mailbox4 = value,
            _ => return false,
        }
        true
    }
}
