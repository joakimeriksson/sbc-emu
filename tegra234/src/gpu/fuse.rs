//! GA10B's fuse block in BAR0 (0x820000-0x823fff) as on a production Orin Nano: privilege
//! security on (falcons boot through the ACR, light-secure), the WPR enabled with the VPR
//! fetched from the memory controller, production (non-debug) signing, and the PMU and GSP
//! fused to their RISC-V cores. nvgpu reads these in `ga10b_fuse_check_priv_security` and
//! `ga10b_fetch_falcon_fuse_settings`; the GPC/TPC floorsweeping fuses read 0 (all enabled).

const PRIV_SEC_EN: u64 = 0x820434;
const SECURE_SOURCE_ISOLATION_EN: u64 = 0x820410;
const WPR_ENABLED: u64 = 0x8205ec;
const VPR_ENABLED: u64 = 0x82067c;
/// The per-device identifier is latched (bit 1).
const P2PRX_PDI: u64 = 0x823000;
const PDI_0: u64 = 0x820344;
const PDI_1: u64 = 0x820348;
/// Falcon core disable and debug-core select: both set selects the RISC-V core.
const PMU_FCD: u64 = 0x820b18;
const GSP_FCD: u64 = 0x820b1c;
const PMU_DCS: u64 = 0x820af8;
const GSP_DCS: u64 = 0x820afc;
const FUSES: std::ops::RangeInclusive<u64> = 0x820000..=0x823fff;

pub(super) fn read(offset: u64) -> Option<u32> {
    if !FUSES.contains(&offset) {
        return None;
    }
    Some(match offset {
        PRIV_SEC_EN | SECURE_SOURCE_ISOLATION_EN | WPR_ENABLED | VPR_ENABLED => 1,
        PMU_FCD | GSP_FCD | PMU_DCS | GSP_DCS => 1,
        P2PRX_PDI => 1 << 1,
        PDI_0 => 0x5eb1_e0a1,
        PDI_1 => 0x0000_0234,
        _ => 0,
    })
}

pub(super) fn contains(offset: u64) -> bool {
    FUSES.contains(&offset)
}
