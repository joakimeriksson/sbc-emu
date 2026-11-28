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
/// ltc_ltc0_lts0_tstg_info_1: an L2 slice's size in KiB (15:0) and the slices (20:16), from which
/// nvgpu gives CUDA the L2's size (2 MiB on the Orin Nano); TensorRT's timing model needs it.
const LTS_TSTG_INFO_1: u64 = 0x14058c;
const L2_SLICES: u32 = 4;
const L2_SLICE_KIB: u32 = 512;
const PRI_FIFO_IDLE: u32 = 0xff_8000;
/// The compression backing store (CBC): its base, size and comptag lines. The MMU marks the
/// configuration safe (CBC_MAX bit 31) once a base inside a nonzero store is programmed; nvgpu
/// checks it on silicon.
const CBC_BASE: u64 = 0x100ec4;
const CBC_TOP: u64 = 0x100ec8;
const CBC_MAX: u64 = 0x100ecc;
const CBC_SAFE: u32 = 1 << 31;

#[derive(Default)]
pub(super) struct Fb {
    hub_ctrl: u32,
    mmu_ctrl: u32,
    invalidate_pdb: u32,
    cbc_base: u32,
    cbc_top: u32,
    cbc_max: u32,
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
            LTS_TSTG_INFO_1 => L2_SLICES << 16 | L2_SLICE_KIB,
            CBC_BASE => self.cbc_base,
            CBC_TOP => self.cbc_top,
            CBC_MAX => {
                let safe = self.cbc_base != 0 && self.cbc_top != 0 && self.cbc_max != 0;
                self.cbc_max | (u32::from(safe) * CBC_SAFE)
            }
            offset if FLUSH.contains(&offset) => 0,
            _ => return None,
        })
    }

    pub fn write(&mut self, offset: u64, value: u32) -> bool {
        match offset {
            HUB_ACTIVE_LTCS => self.hub_ctrl = value & !LTC_COUNT_MASK,
            MMU_CTRL => self.mmu_ctrl = value & !PRI_FIFO_IDLE,
            MMU_INVALIDATE_PDB => self.invalidate_pdb = value,
            CBC_BASE => self.cbc_base = value & 0x3ff_ffff,
            CBC_TOP => self.cbc_top = value & 0x7fff,
            CBC_MAX => self.cbc_max = value & !CBC_SAFE,
            MMU_INVALIDATE | MMU_ACTIVE_LTCS | NISO_HUB_COUNT => {}
            offset if FLUSH.contains(&offset) => {} // GPU writes are already visible in guest RAM.
            _ => return false,
        }
        true
    }
}
