//! GA10B's five-level GMMU, with 4 KiB pitch-linear system-memory pages.
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
    root: u64,
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
        if address >= 1 << 48 {
            return None;
        }
        let mut table = self.root;
        for (shift, mask, stride, half) in [
            (47, 1, 8, 0),
            (38, 511, 8, 0),
            (29, 511, 8, 0),
            (21, 255, 16, 8), // The upper half of the dual PDE points to small pages.
        ] {
            let entry = self.pte(table + ((address >> shift) & mask) * stride + half)?;
            if entry & 6 < 4 {
                return None;
            }
            table = (entry << 4) & 0xff_ffff_ff00;
        }
        let entry = self.pte(table + (address >> 12 & 511) * 8)?;
        if entry & 1 == 0 || entry & 6 < 4 || (write && entry & 0x40 != 0) {
            return None;
        }
        Some(((entry << 4) & 0xff_ffff_f000) | (address & 4095))
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

    pub fn copy(&mut self, mut src: u64, mut dst: u64, mut len: usize) -> Option<()> {
        while len > 0 {
            let n = len
                .min(4096 - (src as usize & 4095))
                .min(4096 - (dst as usize & 4095));
            let from = physical(self.ram, self.translate(src, false)?, n)?;
            let to = physical(self.ram, self.translate(dst, true)?, n)?;
            self.ram.copy_within(from..from + n, to);
            src += n as u64;
            dst += n as u64;
            len -= n;
        }
        Some(())
    }
}
