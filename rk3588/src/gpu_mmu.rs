//! Address translation for the Mali-G610 command-stream GPU.
//!
//! Panthor configures AArch64 stage-1 page tables with a 4 KiB granule. This
//! walker intentionally has no translation cache: changes to the guest's page
//! tables take effect immediately. The caller decides how to report faults and
//! supplies the RAM backing physical addresses; no guest address is trusted.

const PA_MASK: u64 = 0x0000_ffff_ffff_f000;
const MODE_MASK: u64 = 0xf;
const MODE_UNMAPPED: u64 = 1;
const MODE_IDENTITY: u64 = 2;
const MODE_AARCH64_4K: u64 = 6;
const DISABLE_HIER_AP: u64 = 1 << 33;
const DISABLE_AF_FAULT: u64 = 1 << 34;
const WXN: u64 = 1 << 35;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Access {
    Read,
    Write,
    Execute,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Fault {
    UnsupportedConfiguration,
    AddressSize,
    Translation {
        level: u8,
    },
    AccessFlag {
        level: u8,
    },
    Permission {
        level: u8,
    },
    /// A page table entry was not backed by physical memory.
    TableBus {
        level: u8,
    },
    /// A translated physical address was outside RAM.
    PhysicalBus,
}

/// A programmed GPU address space. `transtab` and `transcfg` are the values
/// written to the corresponding AS registers, not CPU TTBR/TCR values.
#[derive(Clone, Copy, Debug, Default)]
pub struct AddressSpace {
    pub transtab: u64,
    pub transcfg: u64,
}

impl AddressSpace {
    /// Read GPU virtual memory, including ranges that cross physical pages.
    pub fn read(
        &self,
        ram: &[u8],
        va: u64,
        output: &mut [u8],
        unprivileged: bool,
    ) -> Result<(), Fault> {
        let chunks = self.chunks(ram, va, output.len(), Access::Read, unprivileged)?;
        let mut offset = 0;
        for (pa, len) in chunks {
            output[offset..offset + len].copy_from_slice(&ram[pa..pa + len]);
            offset += len;
        }
        Ok(())
    }

    /// Write GPU virtual memory. All translations are checked before modifying RAM.
    pub fn write(
        &self,
        ram: &mut [u8],
        va: u64,
        data: &[u8],
        unprivileged: bool,
    ) -> Result<(), Fault> {
        let chunks = self.chunks(ram, va, data.len(), Access::Write, unprivileged)?;
        let mut offset = 0;
        for (pa, len) in chunks {
            ram[pa..pa + len].copy_from_slice(&data[offset..offset + len]);
            offset += len;
        }
        Ok(())
    }

    pub(crate) fn chunks(
        &self,
        ram: &[u8],
        va: u64,
        len: usize,
        access: Access,
        unprivileged: bool,
    ) -> Result<Vec<(usize, usize)>, Fault> {
        let mut result = Vec::new();
        let mut offset = 0usize;
        while offset < len {
            let addr = va.checked_add(offset as u64).ok_or(Fault::AddressSize)?;
            let count = (4096 - (addr as usize & 4095)).min(len - offset);
            let pa = usize::try_from(self.translate(ram, addr, access, unprivileged)?)
                .map_err(|_| Fault::PhysicalBus)?;
            if pa.checked_add(count).is_none_or(|end| end > ram.len()) {
                return Err(Fault::PhysicalBus);
            }
            result.push((pa, count));
            offset += count;
        }
        Ok(result)
    }

    /// Translate one virtual address. `unprivileged` selects the GPU access
    /// privilege; callers should translate again when crossing a page boundary.
    pub fn translate(
        &self,
        ram: &[u8],
        va: u64,
        access: Access,
        unprivileged: bool,
    ) -> Result<u64, Fault> {
        match self.transcfg & MODE_MASK {
            MODE_UNMAPPED => return Err(Fault::Translation { level: 0 }),
            MODE_IDENTITY => return Ok(va),
            MODE_AARCH64_4K => {}
            _ => return Err(Fault::UnsupportedConfiguration),
        }
        // Panthor programs INADDR_SIZE to 55 - GPU virtual-address width.
        let input_bits = 55u32
            .checked_sub(((self.transcfg >> 6) & 0x3f) as u32)
            .ok_or(Fault::UnsupportedConfiguration)?;
        if !(25..=48).contains(&input_bits) {
            return Err(Fault::UnsupportedConfiguration);
        }
        if va >> input_bits != 0 {
            return Err(Fault::AddressSize);
        }
        let levels = (input_bits - 12).div_ceil(9);
        let mut level = (4 - levels) as u8;
        let mut table = self.transtab & PA_MASK;
        let mut table_ro = false;
        let mut table_priv = false;
        let mut table_pxn = false;
        let mut table_uxn = false;
        loop {
            let shift = 12 + 9 * (3 - level as u32);
            let index = (va >> shift) & 0x1ff;
            let addr = table
                .checked_add(index * 8)
                .and_then(|a| usize::try_from(a).ok())
                .ok_or(Fault::TableBus { level })?;
            let bytes = ram
                .get(addr..addr.checked_add(8).ok_or(Fault::TableBus { level })?)
                .ok_or(Fault::TableBus { level })?;
            let desc = u64::from_le_bytes(bytes.try_into().unwrap());
            match (level, desc & 3) {
                (_, 0 | 2) | (0, 1) | (3, 1) => return Err(Fault::Translation { level }),
                (0..=2, 3) => {
                    if self.transcfg & DISABLE_HIER_AP == 0 {
                        table_priv |= desc & (1 << 61) != 0;
                        table_ro |= desc & (1 << 62) != 0;
                    }
                    table_pxn |= desc & (1 << 59) != 0;
                    table_uxn |= desc & (1 << 60) != 0;
                    table = desc & PA_MASK;
                    level += 1;
                }
                _ => {
                    if desc & (1 << 10) == 0 && self.transcfg & DISABLE_AF_FAULT == 0 {
                        return Err(Fault::AccessFlag { level });
                    }
                    let writable = desc & (1 << 7) == 0 && !table_ro;
                    let user = desc & (1 << 6) != 0 && !table_priv;
                    let xn = if unprivileged {
                        table_uxn || desc & (1 << 54) != 0
                    } else {
                        table_pxn || desc & (1 << 53) != 0
                    };
                    if unprivileged && !user
                        || access == Access::Write && !writable
                        || access == Access::Execute && (xn || self.transcfg & WXN != 0 && writable)
                    {
                        return Err(Fault::Permission { level });
                    }
                    let mask = (1u64 << shift) - 1;
                    return Ok((desc & PA_MASK & !mask) | (va & mask));
                }
            }
        }
    }
}

emu_core::snap_struct!(AddressSpace { transtab, transcfg });

#[cfg(test)]
mod tests {
    use super::*;
    const AF: u64 = 1 << 10;
    const USER: u64 = 1 << 6;
    fn entry(ram: &mut [u8], addr: usize, value: u64) {
        ram[addr..addr + 8].copy_from_slice(&value.to_le_bytes());
    }
    fn space(bits: u64) -> AddressSpace {
        AddressSpace {
            transtab: 0x1000,
            transcfg: MODE_AARCH64_4K | ((55 - bits) << 6),
        }
    }
    #[test]
    fn panthor_48bit_page_and_block_walks() {
        let mut ram = vec![0; 0x6000];
        entry(&mut ram, 0x1000, 0x2003);
        entry(&mut ram, 0x2000, 0x3003);
        entry(&mut ram, 0x3000, 0x4003);
        entry(&mut ram, 0x4008, 0x9000 | AF | USER | 3);
        let aspace = space(48);
        assert_eq!(
            aspace.translate(&ram, 0x1234, Access::Write, true),
            Ok(0x9234)
        );
        entry(&mut ram, 0x3008, 0x20_0000 | AF | USER | 1);
        assert_eq!(
            aspace.translate(&ram, 0x20_1234, Access::Read, true),
            Ok(0x20_1234)
        );
        // Page-table writes take effect immediately (there is no stale TLB).
        entry(&mut ram, 0x4008, 0xa000 | AF | USER | 3);
        assert_eq!(
            aspace.translate(&ram, 0x1234, Access::Read, true),
            Ok(0xa234)
        );
    }
    #[test]
    fn permissions_access_flag_and_table_restrictions() {
        let mut ram = vec![0; 0x5000];
        entry(&mut ram, 0x1000, 0x2003);
        entry(&mut ram, 0x2000, 0x3003);
        entry(&mut ram, 0x3000, 0x4003);
        entry(&mut ram, 0x4000, 0x8000 | AF | USER | 3 | (1 << 7));
        let mut a = space(48);
        assert_eq!(a.translate(&ram, 0, Access::Read, true), Ok(0x8000));
        assert_eq!(
            a.translate(&ram, 0, Access::Write, true),
            Err(Fault::Permission { level: 3 })
        );
        entry(&mut ram, 0x4000, 0x8000 | USER | 3);
        assert_eq!(
            a.translate(&ram, 0, Access::Read, true),
            Err(Fault::AccessFlag { level: 3 })
        );
        a.transcfg |= DISABLE_AF_FAULT | WXN;
        assert_eq!(
            a.translate(&ram, 0, Access::Execute, true),
            Err(Fault::Permission { level: 3 })
        );
        entry(&mut ram, 0x2000, 0x3003 | (1 << 61));
        assert_eq!(
            a.translate(&ram, 0, Access::Read, true),
            Err(Fault::Permission { level: 3 })
        );
        a.transcfg |= DISABLE_HIER_AP;
        assert_eq!(a.translate(&ram, 0, Access::Read, true), Ok(0x8000));
    }
    #[test]
    fn invalid_addresses_and_malformed_descriptors_fault_without_panicking() {
        let mut ram = vec![0; 0x5000];
        let a = space(39);
        assert_eq!(
            a.translate(&ram, 1 << 39, Access::Read, false),
            Err(Fault::AddressSize)
        );
        assert_eq!(
            a.translate(&ram, 0, Access::Read, false),
            Err(Fault::Translation { level: 1 })
        );
        entry(&mut ram, 0x1000, 0xffff_f000 | 3);
        assert_eq!(
            a.translate(&ram, 0, Access::Read, false),
            Err(Fault::TableBus { level: 2 })
        );
        assert_eq!(
            AddressSpace::default().translate(&ram, 0, Access::Read, false),
            Err(Fault::UnsupportedConfiguration)
        );
    }
    #[test]
    fn cross_page_dma_respects_noncontiguous_pages_and_faults_atomically() {
        let mut ram = vec![0; 0xb000];
        entry(&mut ram, 0x1000, 0x2003);
        entry(&mut ram, 0x2000, 0x3003);
        entry(&mut ram, 0x3000, 0x4003);
        entry(&mut ram, 0x4000, 0x7000 | AF | USER | 3);
        entry(&mut ram, 0x4008, 0x9000 | AF | USER | 3);
        let a = space(48);
        a.write(&mut ram, 0xffe, b"ABCD", true).unwrap();
        assert_eq!(&ram[0x7ffe..0x8000], b"AB");
        assert_eq!(&ram[0x9000..0x9002], b"CD");
        let mut output = [0; 4];
        a.read(&ram, 0xffe, &mut output, true).unwrap();
        assert_eq!(&output, b"ABCD");
        entry(&mut ram, 0x4008, 0x9000 | AF | USER | 3 | (1 << 7));
        assert_eq!(
            a.write(&mut ram, 0xffe, b"FAIL", true),
            Err(Fault::Permission { level: 3 })
        );
        assert_eq!(&ram[0x7ffe..0x8000], b"AB");
    }
}
