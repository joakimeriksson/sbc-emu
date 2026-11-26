//! GA10B framebuffer hub and GMMU control registers.
use super::top::LTCS;

const HUB_ACTIVE_LTCS: u64 = 0x100800;
const MMU_CTRL: u64 = 0x100c80;
const MMU_INVALIDATE_PDB: u64 = 0x100cb8;
const MMU_INVALIDATE: u64 = 0x100cbc;
const MMU_ACTIVE_LTCS: u64 = 0x100ec0;
const NISO_HUB_COUNT: u64 = 0x004c7c;
const FLUSH: std::ops::RangeInclusive<u64> = 0x070000..=0x070010;
const LTC_COUNT_MASK: u32 = 0x1f;
const PRI_FIFO_IDLE: u32 = 0xff_8000;

#[derive(Default)]
pub(super) struct Fb {
    hub_ctrl: u32,
    mmu_ctrl: u32,
    invalidate_pdb: u32,
}

impl Fb {
    pub fn read(&self, offset: u64) -> Option<u32> {
        Some(match offset {
            HUB_ACTIVE_LTCS => self.hub_ctrl | LTCS,
            MMU_ACTIVE_LTCS => LTCS,
            MMU_CTRL => self.mmu_ctrl | PRI_FIFO_IDLE,
            MMU_INVALIDATE_PDB => self.invalidate_pdb,
            MMU_INVALIDATE => 0,
            NISO_HUB_COUNT => 1 << 1,
            offset if FLUSH.contains(&offset) => 0,
            _ => return None,
        })
    }

    pub fn write(&mut self, offset: u64, value: u32) -> bool {
        match offset {
            HUB_ACTIVE_LTCS => self.hub_ctrl = value & !LTC_COUNT_MASK,
            MMU_CTRL => self.mmu_ctrl = value & !PRI_FIFO_IDLE,
            MMU_INVALIDATE_PDB => self.invalidate_pdb = value,
            MMU_INVALIDATE | MMU_ACTIVE_LTCS | NISO_HUB_COUNT => {}
            offset if FLUSH.contains(&offset) => {} // GPU writes are already visible in guest RAM.
            _ => return false,
        }
        true
    }
}
