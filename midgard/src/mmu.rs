//! The Midgard MMU: one set of registers per address space and the "Mali LPAE" page tables
//! (Linux `io-pgtable-arm.c`, `ARM_MALI_LPAE`).
//!
//! The tables are always four levels of 512 64-bit entries with a 4 KiB granule (48-bit
//! virtual addresses, 40-bit physical). An entry's low two bits are its type: 3 is a table
//! at levels 0-2, 1 is a block (levels 1-2: 1 GiB, 2 MiB) or a page (level 3); 0 and 2 are
//! invalid. The output address is bits [39:12]. `AS_TRANSTAB` holds the level-0 table's
//! physical address with the address mode in its low two bits (3: tables; 0/1: unmapped,
//! 2: identity).

/// Physical address bits (MMU_FEATURES: 40-bit PA, 48-bit VA).
const PA_MASK: u64 = (1 << 40) - 1;

/// Exception codes written to AS_FAULTSTATUS[7:0] (and JS_STATUS for the job that faulted).
pub const TRANSLATION_FAULT: u32 = 0xc0;
pub const PERMISSION_FAULT: u32 = 0xc8;
/// AS_FAULTSTATUS[9:8]: access type.
pub const ACCESS_READ: u32 = 2 << 8;
pub const ACCESS_WRITE: u32 = 3 << 8;

#[derive(Clone, Copy, Debug, Default)]
pub struct AddressSpace {
    pub transtab: u64,
    pub memattr: u64,
    pub lockaddr: u64,
    pub transcfg: u64,
    pub faultstatus: u32,
    pub faultaddress: u64,
    /// The tables in use (latched by AS_COMMAND_UPDATE).
    pub active_transtab: u64,
}

/// A translation failure: the level that had no entry (or the permission fault).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Fault {
    pub va: u64,
    pub status: u32,
}

impl AddressSpace {
    /// Translate a GPU virtual address through this address space's tables.
    pub fn translate(&self, ram: &[u8], ram_base: u64, va: u64, write: bool) -> Result<u64, Fault> {
        let access = if write { ACCESS_WRITE } else { ACCESS_READ };
        let fault = |level: u32| Fault { va, status: TRANSLATION_FAULT | level | access };
        match self.active_transtab & 3 {
            3 => {}
            2 => return Ok(va & PA_MASK),
            _ => return Err(fault(0)),
        }
        let mut table = self.active_transtab & PA_MASK & !0xfff;
        for level in 0..4u32 {
            let shift = 39 - 9 * level;
            let index = (va >> shift) & 0x1ff;
            let pte = read_u64(ram, ram_base, table + index * 8).ok_or(fault(level))?;
            match pte & 3 {
                3 if level < 3 => table = pte & PA_MASK & !0xfff,
                1 if level > 0 => {
                    if write && pte & (1 << 7) != 0 {
                        return Err(Fault { va, status: PERMISSION_FAULT | level | access });
                    }
                    let mask = (1u64 << shift) - 1;
                    return Ok((pte & PA_MASK & !mask) | (va & mask));
                }
                _ => return Err(fault(level)),
            }
        }
        Err(fault(3))
    }
}

fn read_u64(ram: &[u8], base: u64, pa: u64) -> Option<u64> {
    let i = usize::try_from(pa.checked_sub(base)?).ok()?;
    Some(u64::from_le_bytes(ram.get(i..i.checked_add(8)?)?.try_into().unwrap()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn put(ram: &mut [u8], pa: u64, v: u64) {
        ram[pa as usize..pa as usize + 8].copy_from_slice(&v.to_le_bytes());
    }

    #[test]
    fn four_level_walk_pages_and_blocks() {
        let mut ram = vec![0u8; 0x10000];
        // L0 at 0x1000 -> L1 at 0x2000 -> L2 at 0x3000 -> L3 at 0x4000
        put(&mut ram, 0x1000, 0x2000 | 3);
        put(&mut ram, 0x2000, 0x3000 | 3);
        put(&mut ram, 0x3000 + 8, 0x4000 | 3); // VA 2 MiB..4 MiB
        put(&mut ram, 0x4000 + 5 * 8, 0x8000 | 1 | 1 << 7); // VA 0x205000, read-only
        put(&mut ram, 0x3000 + 2 * 8, 0x4000_0000 | 1); // 2 MiB block at VA 4 MiB
        let a = AddressSpace { active_transtab: 0x1000 | 4 | 3, ..Default::default() };
        assert_eq!(a.translate(&ram, 0, 0x205123, false), Ok(0x8123));
        assert_eq!(a.translate(&ram, 0, 0x412345, true), Ok(0x4001_2345));
        let w = a.translate(&ram, 0, 0x205000, true).unwrap_err();
        assert_eq!(w.status, PERMISSION_FAULT | 3 | ACCESS_WRITE);
        let miss = a.translate(&ram, 0, 0x206000, false).unwrap_err();
        assert_eq!(miss.status, TRANSLATION_FAULT | 3 | ACCESS_READ);
        let off = AddressSpace::default();
        assert!(off.translate(&ram, 0, 0, false).is_err());
    }
}
