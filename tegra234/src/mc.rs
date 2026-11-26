//! DRAM configuration registers used by the Tegra234 memory-controller driver.

const MC: u64 = 0x02c1_0000;
const EMC: u64 = 0x02c6_0000;
const EMEM_ADR_CFG: u64 = MC + 0x054;
const EMEM_ADR_CFG_CHANNEL_ENABLE: u64 = MC + 0x0df8;
const MC_CONFIG: u64 = MC + 0x1880;
const EMC_DRAM_TYPE: u64 = EMC + 0x100c;

pub fn read(addr: u64) -> Option<u32> {
    Some(match addr {
        EMEM_ADR_CFG | MC_CONFIG => 0,
        EMEM_ADR_CFG_CHANNEL_ENABLE => 0xf, // Four enabled channels.
        EMC_DRAM_TYPE => 1,                 // LPDDR5.
        _ => return None,
    })
}
