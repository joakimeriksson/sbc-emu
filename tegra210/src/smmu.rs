//! The memory controller (`nvidia,tegra210-mc`) with its SMMU: the 4 GiB address spaces the display,
//! host1x and the GPU use. Each group of clients has a register naming its address space; each
//! address space has a two-level page table (1024 × 1024 entries of 4 KiB pages, 34-bit physical
//! addresses). Tables are walked on every use: there is no TLB to flush.
use peripherals::dma::Memory;

pub const BASE: u64 = 0x7001_9000;
pub const SIZE: u64 = 0x1000;

const CONFIG: usize = 0x10;
const PTB_ASID: usize = 0x1c;
const PTB_DATA: usize = 0x20;
/// (control, status) register pairs of the clients' hot resets: a client whose DMA is blocked
/// (control bit set) is idle at once (status bit set).
const HOTRESET: [(usize, usize); 2] = [(0x200, 0x204), (0x970, 0x974)];

/// The client groups' address space registers.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Group {
    Dc = 0x240,
    DcB = 0x244,
    Host1x = 0x250,
    Vic = 0x284,
    Gpu = 0xaac,
    Sdmmc1a = 0xa94,
    Sdmmc2a = 0xa98,
    Sdmmc3a = 0xa9c,
    Sdmmc4a = 0xaa0,
}

/// The SD/MMC controllers' groups, by controller.
pub const SDMMC: [Group; 4] = [Group::Sdmmc1a, Group::Sdmmc2a, Group::Sdmmc3a, Group::Sdmmc4a];

const PFN: u32 = 0x3f_ffff;
const READABLE: u32 = 1 << 31;
const WRITABLE: u32 = 1 << 30;
const NEXT: u32 = 1 << 28;

pub struct Smmu {
    regs: Vec<u32>,
    ptb: [u32; 128],
}

impl Default for Smmu {
    fn default() -> Self {
        let mut regs = vec![0; SIZE as usize / 4];
        // CONFIG is secure-only on Tegra210: the boot firmware turns translation on (L4T's
        // kernel relies on that; mainline writes it again). Clients stay untranslated until
        // their group's register enables an address space.
        regs[CONFIG / 4] = 1;
        Self { regs, ptb: [0; 128] }
    }
}

impl Smmu {
    pub fn read(&self, offset: u64) -> u32 {
        let o = offset as usize & (SIZE as usize - 4);
        if let Some(&(control, _)) = HOTRESET.iter().find(|&&(_, s)| s == o) {
            return self.regs[control / 4];
        }
        match o {
            PTB_DATA => self.ptb[self.regs[PTB_ASID / 4] as usize & 127],
            _ => self.regs[o / 4],
        }
    }

    pub fn write(&mut self, offset: u64, v: u32) {
        let o = offset as usize & (SIZE as usize - 4);
        match o {
            PTB_DATA => self.ptb[self.regs[PTB_ASID / 4] as usize & 127] = v,
            0x00 => self.regs[0] &= !v, // the interrupt status: write 1 to clear
            _ => self.regs[o / 4] = v,
        }
    }

    /// The page directory (physical) of the group's first address space; None: not translated.
    pub fn directory(&self, group: Group) -> Option<u32> {
        self.spaces(group).map(|s| s.0[0])
    }

    /// The group's address spaces; None: not translated. A group's register holds an ASID per
    /// 4 GiB window of the 34-bit addresses its clients send (bits 33:32 choose; mainline Linux
    /// fills in the first, L4T's kernel all four).
    pub fn spaces(&self, group: Group) -> Option<Spaces> {
        let r = self.regs[group as usize / 4];
        (self.regs[CONFIG / 4] & 1 != 0 && r & 1 << 31 != 0).then(|| Spaces(std::array::from_fn(|k| self.ptb[(r >> (8 * k)) as usize & 127])))
    }

    /// The physical address behind `iova` for a client of `group`; None is a fault.
    pub fn translate(&self, group: Group, iova: u64, write: bool, dma: &dyn Memory) -> Option<u64> {
        match self.spaces(group) {
            Some(s) => s.walk(iova, write, dma),
            None => Some(iova),
        }
    }

    /// Read `out.len()` bytes at `iova`, page by page; what is not mapped reads as zero.
    pub fn read_bytes(&self, group: Group, mut iova: u64, mut out: &mut [u8], dma: &dyn Memory) {
        while !out.is_empty() {
            let n = (0x1000 - (iova & 0xfff) as usize).min(out.len());
            let (part, rest) = out.split_at_mut(n);
            match self.translate(group, iova, false, dma).and_then(|pa| dma.bytes(pa, n)) {
                Some(d) => part.copy_from_slice(d),
                None => part.fill(0),
            }
            out = rest;
            iova += n as u64;
        }
    }

    pub fn read_u32(&self, group: Group, iova: u64, dma: &dyn Memory) -> u32 {
        let mut b = [0; 4];
        self.read_bytes(group, iova, &mut b, dma);
        u32::from_le_bytes(b)
    }
}

/// A group's four address spaces (their page directory entries), by 4 GiB window.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Spaces(pub [u32; 4]);

impl Spaces {
    /// The physical address behind 34-bit `iova`; None is a fault.
    pub fn walk(&self, iova: u64, write: bool, dma: &dyn Memory) -> Option<u64> {
        self.walk_with(iova, write, &|pa| dma.bytes(pa, 4).map(|b| u32::from_le_bytes(b.try_into().unwrap())))
    }

    /// The same, reading the tables' words (by physical address) with `read32`.
    pub fn walk_with(&self, iova: u64, write: bool, read32: &dyn Fn(u64) -> Option<u32>) -> Option<u64> {
        if iova >> 34 != 0 {
            return None;
        }
        walk_with(self.0[(iova >> 32) as usize], iova & 0xffff_ffff, write, read32)
    }
}

/// Walk the page table of directory entry `ptb`.
pub fn walk(ptb: u32, iova: u64, write: bool, dma: &dyn Memory) -> Option<u64> {
    walk_with(ptb, iova, write, &|pa| dma.bytes(pa, 4).map(|b| u32::from_le_bytes(b.try_into().unwrap())))
}

fn walk_with(ptb: u32, iova: u64, write: bool, read32: &dyn Fn(u64) -> Option<u32>) -> Option<u64> {
    let need = if write { WRITABLE } else { READABLE };
    if ptb & need == 0 || iova >> 32 != 0 {
        return None;
    }
    let entry = |table: u32, index: u64| read32(((table & PFN) as u64) << 12 | index << 2);
    let pde = entry(ptb, iova >> 22 & 0x3ff)?;
    if pde & need == 0 {
        return None;
    }
    if pde & NEXT == 0 {
        return Some(((pde & PFN) as u64) << 12 | iova & 0x3f_ffff); // a 4 MiB page
    }
    let pte = entry(pde, iova >> 12 & 0x3ff)?;
    (pte & need != 0).then_some(((pte & PFN) as u64) << 12 | iova & 0xfff)
}

#[cfg(test)]
mod tests {
    use super::*;
    use peripherals::dma::Dma;

    /// L4T fills a group's register with four ASIDs, one per 4 GiB window.
    #[test]
    fn four_windows_per_group() {
        const BASE: u64 = 0x8000_0000;
        let mut ram = vec![0u8; 0x10000];
        let put = |ram: &mut [u8], o: usize, v: u32| ram[o..o + 4].copy_from_slice(&v.to_le_bytes());
        // ASID 7: directory at +0x1000 with a 4 MiB page at iova 0 -> +0x0 (phys BASE)
        put(&mut ram, 0x1000, 3 << 30 | (BASE as u32) >> 12);
        let mut s = Smmu::default();
        s.write(PTB_ASID as u64, 7);
        s.write(PTB_DATA as u64, 3 << 30 | (BASE as u32 + 0x1000) >> 12);
        s.write(Group::Gpu as u64, 1 << 31 | 7 << 8);
        let d = Dma { ram: &mut ram, base: BASE };
        assert_eq!(s.translate(Group::Gpu, 0x1_0000_1234, false, &d), Some(BASE + 0x1234), "window 1 is ASID 7");
        assert_eq!(s.translate(Group::Gpu, 0x0_0000_1234, false, &d), None, "window 0 (ASID 0) maps nothing");
        assert_eq!(s.translate(Group::Gpu, 0x4_0000_0000, false, &d), None, "beyond 34 bits");
    }

    #[test]
    fn two_levels_and_per_group_spaces() {
        const BASE: u64 = 0x8000_0000;
        let mut ram = vec![0u8; 0x10000];
        let put = |ram: &mut [u8], o: usize, v: u32| ram[o..o + 4].copy_from_slice(&v.to_le_bytes());
        // directory at +0x1000, table at +0x2000, data at +0x5000; iova 0x0040_3000
        put(&mut ram, 0x1004, 0x8_0002 | 0xf << 28);
        put(&mut ram, 0x200c, 0x8_0005 | 0xe << 28);
        put(&mut ram, 0x2010, 0x8_0006 | 1 << 31); // read only
        ram[0x5010] = 0x77;
        let mut s = Smmu::default();
        let d = Dma { ram: &mut ram, base: BASE };
        assert_eq!(s.translate(Group::Dc, 0x8000_0010, false, &d), Some(0x8000_0010), "off: untranslated");
        s.write(PTB_ASID as u64, 3);
        s.write(PTB_DATA as u64, 0x8_0001 | 7 << 29);
        s.write(CONFIG as u64, 1);
        s.write(Group::Dc as u64, 1 << 31 | 3);
        assert_eq!(s.translate(Group::Dc, 0x40_3010, true, &d), Some(0x8000_5010));
        assert_eq!(s.translate(Group::Dc, 0x40_4000, false, &d), Some(0x8000_6000));
        assert_eq!(s.translate(Group::Dc, 0x40_4000, true, &d), None);
        assert_eq!(s.translate(Group::Dc, 0x40_5000, false, &d), None);
        assert_eq!(s.translate(Group::DcB, 0x40_3010, true, &d), Some(0x40_3010));
        let mut out = [1; 2];
        s.read_bytes(Group::Dc, 0x40_3010, &mut out, &d);
        assert_eq!(out, [0x77, 0]);
        s.write(0x200, 1 << 5);
        assert_eq!(s.read(0x204), 1 << 5, "a blocked client is idle");
    }
}
