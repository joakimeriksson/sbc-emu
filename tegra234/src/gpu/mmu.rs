//! GA10B's five-level GMMU with 4 KiB and 64 KiB system-memory pages.
use crate::RAM_BASE;

pub(super) fn physical(ram: &[u8], address: u64, len: usize) -> Option<usize> {
    let offset = usize::try_from(address.checked_sub(RAM_BASE)?).ok()?;
    (offset.checked_add(len)? <= ram.len()).then_some(offset)
}

pub(super) fn word(ram: &[u8], address: u64) -> Option<u32> {
    let offset = physical(ram, address, 4)?;
    Some(u32::from_le_bytes(ram[offset..offset + 4].try_into().ok()?))
}

pub(super) struct View<'a> {
    pub ram: &'a mut [u8],
    pub(super) root: u64,
}

impl<'a> View<'a> {
    pub fn from_instance(ram: &'a mut [u8], instance: u64) -> Option<Self> {
        let lo = word(ram, instance.checked_add(0x200)?)?;
        let hi = word(ram, instance.checked_add(0x204)?)?;
        let root = u64::from(lo & !0xfff) | u64::from(hi & 0xff) << 32;
        Some(Self { ram, root })
    }

    fn pte(&self, address: u64) -> Option<u64> {
        let lo = word(self.ram, address)? as u64;
        let hi = word(self.ram, address + 4)? as u64;
        Some(lo | hi << 32)
    }

    pub fn translate(&self, address: u64, write: bool) -> Option<u64> {
        if address >= 1 << 49 {
            return None;
        }
        let mut table = self.root;
        for (shift, mask) in [(47, 3), (38, 511), (29, 511)] {
            let entry = self.pte(table + ((address >> shift) & mask) * 8)?;
            if entry & 6 < 4 {
                return None;
            }
            table = (entry << 4) & 0xff_ffff_f000;
        }
        let dual = table + (address >> 21 & 255) * 16;
        let big = self.pte(dual)?;
        let small = self.pte(dual + 8)?;
        let (table, shift) = match (small & 6, big & 6) {
            (4 | 6, 0) => ((small << 4) & 0xff_ffff_f000, 12),
            (0, 4 | 6) => ((big << 4) & 0xff_ffff_ff00, 16),
            _ => return None,
        };
        let index_mask = (1 << (21 - shift)) - 1;
        let entry = self.pte(table + (address >> shift & index_mask) * 8)?;
        if entry & 1 == 0 || entry & 6 < 4 || (write && entry & 0x40 != 0) {
            return None;
        }
        let page_mask = (1 << shift) - 1;
        Some(((entry << 4) & 0xff_ffff_f000 & !page_mask) | (address & page_mask))
    }

    pub fn load(&self, address: u64) -> Option<u32> {
        if address & 3 != 0 {
            return None;
        }
        word(self.ram, self.translate(address, false)?)
    }

    pub fn store(&mut self, address: u64, value: u32) -> Option<()> {
        if address & 3 != 0 {
            return None;
        }
        let off = physical(self.ram, self.translate(address, true)?, 4)?;
        self.ram[off..off + 4].copy_from_slice(&value.to_le_bytes());
        Some(())
    }

    /// A four-word semaphore writes its timestamp before publishing the payload.
    pub fn release(&mut self, address: u64, payload: u32, timestamp: Option<u64>) -> Option<()> {
        if let Some(timestamp) = timestamp {
            self.store(address + 4, 0)?;
            self.store(address + 8, timestamp as u32)?;
            self.store(address + 12, (timestamp >> 32) as u32)?;
        }
        self.store(address, payload)
    }

    pub fn read(&self, mut src: u64, mut out: &mut [u8]) -> Option<()> {
        while !out.is_empty() {
            let n = out.len().min(4096 - (src as usize & 4095));
            let offset = physical(self.ram, self.translate(src, false)?, n)?;
            out[..n].copy_from_slice(&self.ram[offset..offset + n]);
            src += n as u64;
            out = &mut out[n..];
        }
        Some(())
    }

    pub fn write(&mut self, mut dst: u64, mut bytes: &[u8]) -> Option<()> {
        while !bytes.is_empty() {
            let n = bytes.len().min(4096 - (dst as usize & 4095));
            let offset = physical(self.ram, self.translate(dst, true)?, n)?;
            self.ram[offset..offset + n].copy_from_slice(&bytes[..n]);
            dst += n as u64;
            bytes = &bytes[n..];
        }
        Some(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn big_page_tables_use_the_low_half_of_the_dual_pde() {
        let mut ram = vec![0; 0x40000];
        let encode = |physical: u64, flags: u64| ((physical >> 4) | flags).to_le_bytes();
        ram[0..8].copy_from_slice(&encode(RAM_BASE + 0x1000, 4));
        ram[0x1000..0x1008].copy_from_slice(&encode(RAM_BASE + 0x2000, 4));
        ram[0x2000..0x2008].copy_from_slice(&encode(RAM_BASE + 0x3000, 4));
        // The large-page table is 256-byte aligned, with 32 entries.
        ram[0x3000..0x3008].copy_from_slice(&encode(RAM_BASE + 0x4100, 4));
        ram[0x4100..0x4108].copy_from_slice(&encode(RAM_BASE + 0x20000, 5));
        ram[0x4108..0x4110].copy_from_slice(&encode(RAM_BASE + 0x10000, 5));
        let mut view = View {
            ram: &mut ram,
            root: RAM_BASE,
        };
        view.write(0xfffe, &[1, 2, 3, 4]).unwrap();
        assert_eq!(&view.ram[0x2fffe..0x30000], &[1, 2]);
        assert_eq!(&view.ram[0x10000..0x10002], &[3, 4]);
        view.ram[0x4108..0x4110].copy_from_slice(&encode(RAM_BASE + 0x10000, 0x45));
        assert_eq!(view.translate(0x10000, false), Some(RAM_BASE + 0x10000));
        assert_eq!(view.translate(0x10000, true), None);
    }
}
