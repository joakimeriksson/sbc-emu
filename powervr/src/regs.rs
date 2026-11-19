//! Rogue register offsets (`pvr_rogue_cr_defs.h`) that the model treats specially.

pub const CORE_ID_PBVNC: u64 = 0x0020;
pub const SOFT_RESET: u64 = 0x0100;
/// SOFT_RESET bit 32: GARTEN (the MIPS core and its wrapper) held in reset.
pub const SOFT_RESET_CPU: u64 = 1 << 32;
pub const SIDEKICK_IDLE: u64 = 0x03c8;
pub const MIPS_ADDR_REMAP2_CONFIG2: u64 = 0x0830;
pub const MIPS_WRAPPER_IRQ_STATUS: u64 = 0x08a8;
pub const MIPS_WRAPPER_IRQ_CLEAR: u64 = 0x08b0;
pub const MARS_IDLE: u64 = 0x08f8;
pub const MTS_SCHEDULE: u64 = 0x0b00;
pub const BIF_READS_EXT_STATUS: u64 = 0x1320;
pub const BIFPM_READS_EXT_STATUS: u64 = 0x1338;
pub const BIFPM_STATUS_MMU: u64 = 0x1350;
pub const BIF_STATUS_MMU: u64 = 0x1358;
pub const SLC_STATUS1: u64 = 0x3870;
pub const SLC_IDLE: u64 = 0x3898;
