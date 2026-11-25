//! GA10B graphics-cluster reset and context-switch firmware handshake.
//! Respond to the boot and size queries used by nvgpu, as with Tegra210's FECS
//! model. Size responses reserve guest buffers for the modeled topology.
use super::top::TPCS_PER_GPC;

const GPC_RESET_BROADCAST: u64 = 0x41a610;
const GPC0_RESET_STATUS: u64 = 0x502610;
const RESET_RELEASED: u32 = 1;
const GPC0_TPCS: u64 = 0x502608;
const GPC0_ZCULL_BANKS: u64 = 0x500920;
const GPC0_PES0_TPC_MASK: u64 = 0x500c30;
const FECS_CPUCTL: u64 = 0x409100;
const FECS_IMEM_DATA: u64 = 0x409184;
const FECS_DMEM_DATA: u64 = 0x4091c4;
const FECS_METHOD_DATA: u64 = 0x409500;
const FECS_METHOD_PUSH: u64 = 0x409504;
const FECS_MAILBOX0: u64 = 0x409800;
const GPCCS_CPUCTL: u64 = 0x41a100;
const GPCCS_IMEM_DATA: u64 = 0x41a184;
const GPCCS_DMEM_DATA: u64 = 0x41a1c4;
const CPU_START: u32 = 1 << 1;
const UCODE_INIT_COMPLETE: u32 = 1;
const DISCOVER_CONTEXT_SIZE: u32 = 0x10;
const DISCOVER_ZCULL_SIZE: u32 = 0x16;
const DISCOVER_PREEMPT_SIZE: u32 = 0x1a;
const DISCOVER_PM_SIZE: u32 = 0x25;
const CONTEXT_BYTES: u32 = 0x20_000;
const ZCULL_BYTES: u32 = 0x1000;
const PM_BYTES: u32 = 0x1000;

#[derive(Default)]
pub(super) struct Gr {
    released: u32,
    fecs_uploaded: bool,
    gpccs_uploaded: bool,
    gpccs_started: bool,
    fecs_started: bool,
    mailbox0: u32,
    method_data: u32,
}

impl Gr {
    pub fn firmware_data_port(offset: u64) -> bool {
        matches!(
            offset,
            FECS_IMEM_DATA | FECS_DMEM_DATA | GPCCS_IMEM_DATA | GPCCS_DMEM_DATA
        )
    }

    pub fn read(&self, offset: u64) -> Option<u32> {
        match offset {
            GPC_RESET_BROADCAST | GPC0_RESET_STATUS => Some(self.released),
            GPC0_TPCS => Some(TPCS_PER_GPC | TPCS_PER_GPC << 16),
            GPC0_ZCULL_BANKS => Some(TPCS_PER_GPC),
            GPC0_PES0_TPC_MASK => Some((1 << TPCS_PER_GPC) - 1),
            FECS_MAILBOX0 => Some(self.mailbox0),
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
            GPCCS_IMEM_DATA => self.gpccs_uploaded = true,
            FECS_IMEM_DATA => self.fecs_uploaded = true,
            GPCCS_DMEM_DATA | FECS_DMEM_DATA => {}
            GPCCS_CPUCTL | FECS_CPUCTL => {
                let was_running = self.fecs_started && self.gpccs_started;
                let start = self.released != 0 && value & CPU_START != 0;
                if offset == FECS_CPUCTL {
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
                DISCOVER_CONTEXT_SIZE | DISCOVER_PREEMPT_SIZE => self.mailbox0 = CONTEXT_BYTES,
                DISCOVER_ZCULL_SIZE => self.mailbox0 = ZCULL_BYTES,
                DISCOVER_PM_SIZE => self.mailbox0 = PM_BYTES,
                _ => {}
            },
            FECS_METHOD_PUSH => {}
            FECS_MAILBOX0 => self.mailbox0 = value,
            _ => return false,
        }
        true
    }
}
