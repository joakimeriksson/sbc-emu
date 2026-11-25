//! The GPU's memory management unit: per-address-space page directories (8 bytes per 128 MiB, or
//! 64 MiB with 64 KiB big pages) whose entries point at a big and a small (4 KiB) page table.
//! Physical addresses with bit 34 set go through the SMMU.
use crate::smmu::{Group, Smmu};
use crate::RAM_BASE;
use std::cell::Cell;

/// An address space: what a channel's instance block (or a BAR's) says at 0x200.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct Space {
    /// the page directory's address
    pub directory: u64,
    /// big pages are 64 KiB (else 128)
    pub big64: bool,
}

/// A translation: the page's physical RAM offset, whether it may be written, and its kind.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Page {
    pub offset: usize,
    pub write: bool,
    pub kind: u8,
}

const TLB: usize = 1024;

#[derive(Clone, Copy)]
struct Entry {
    space: u64,
    page: u64,
    to: Page,
}

const NONE: Entry = Entry { space: u64::MAX, page: u64::MAX, to: Page { offset: 0, write: false, kind: 0 } };

pub struct Mmu {
    tlb: Vec<Cell<Entry>>,
}

impl Default for Mmu {
    fn default() -> Self {
        Self { tlb: vec![Cell::new(NONE); TLB] }
    }
}

/// Offset in RAM of a GPU physical address.
pub fn physical(pa: u64, smmu: &Smmu, ram: &[u8]) -> Option<usize> {
    let pa = if pa >> 34 & 1 != 0 {
        smmu.spaces(Group::Gpu)?.walk(pa & 0x3_ffff_ffff, false, &peripherals::dma::Ram { ram, base: RAM_BASE })?
    } else {
        pa
    };
    let o = usize::try_from(pa.checked_sub(RAM_BASE)?).ok()?;
    (o < ram.len()).then_some(o)
}

pub fn read64(pa: u64, smmu: &Smmu, ram: &[u8]) -> Option<u64> {
    let o = physical(pa, smmu, ram)?;
    Some(u64::from_le_bytes(ram.get(o..o + 8)?.try_into().unwrap()))
}

pub fn read32(pa: u64, smmu: &Smmu, ram: &[u8]) -> Option<u32> {
    let o = physical(pa, smmu, ram)?;
    Some(u32::from_le_bytes(ram.get(o..o + 4)?.try_into().unwrap()))
}

pub fn write32(pa: u64, v: u32, smmu: &Smmu, ram: &mut [u8]) {
    if let Some(b) = physical(pa, smmu, ram).and_then(|o| ram.get_mut(o..o + 4)) {
        b.copy_from_slice(&v.to_le_bytes());
    }
}

/// The address space of the instance block at `inst`.
pub fn space(inst: u64, smmu: &Smmu, ram: &[u8]) -> Space {
    let v = read64(inst + 0x200, smmu, ram).unwrap_or(0);
    Space { directory: v & 0xff_ffff_f000, big64: v >> 11 & 1 != 0 }
}

impl Mmu {
    pub fn invalidate(&mut self) {
        self.tlb.iter().for_each(|e| e.set(NONE));
    }

    /// The 4 KiB page of `va`.
    #[inline]
    pub fn translate(&self, s: Space, va: u64, smmu: &Smmu, ram: &[u8]) -> Option<Page> {
        let page = va >> 12;
        let slot = &self.tlb[(page ^ page >> 10) as usize % TLB];
        let e = slot.get();
        if e.page == page && e.space == s.directory {
            return Some(e.to);
        }
        let to = walk(s, va, smmu, ram)?;
        slot.set(Entry { space: s.directory, page, to });
        Some(to)
    }
}

fn walk(s: Space, va: u64, smmu: &Smmu, ram: &[u8]) -> Option<Page> {
    if s.directory == 0 || va >> 40 != 0 {
        return None;
    }
    let (pde_shift, big_shift) = if s.big64 { (26, 16) } else { (27, 17) };
    let pde = read64(s.directory + (va >> pde_shift) * 8, smmu, ram)?;
    let within = va & ((1 << pde_shift) - 1);
    let mut found = None;
    if pde >> 32 & 3 != 0 {
        let table = (pde >> 36) << 12;
        found = read64(table + (within >> 12) * 8, smmu, ram).filter(|p| p & 1 != 0).map(|p| (p, 12));
    }
    if found.is_none() && pde & 3 != 0 {
        let table = (pde & 0xffff_fff0) << 8;
        found = read64(table + (within >> big_shift) * 8, smmu, ram).filter(|p| p & 1 != 0).map(|p| (p, big_shift));
    }
    let (pte, shift) = found?;
    let pa = ((pte & 0xffff_fff0) << 8) + (va & ((1 << shift) - 1) & !0xfff);
    Some(Page { offset: physical(pa, smmu, ram)?, write: pte & 4 == 0, kind: (pte >> 36) as u8 })
}

/// Memory as a channel sees it.
pub struct View<'a> {
    pub ram: &'a mut [u8],
    pub smmu: &'a Smmu,
    pub mmu: &'a Mmu,
    pub space: Space,
    /// the first address that did not translate
    pub fault: Cell<Option<(u64, bool)>>,
}

impl maxwell::Memory for View<'_> {
    fn read(&self, va: u64, out: &mut [u8]) {
        View::read(self, va, out);
    }
    fn write(&mut self, va: u64, data: &[u8]) {
        View::write(self, va, data);
    }
    fn share(&self) -> Option<Box<dyn maxwell::Memory + Send + '_>> {
        Some(Box::new(Reader { ram: self.ram, smmu: self.smmu, mmu: Mmu::default(), space: self.space }))
    }
    fn split(&mut self, n: usize, run: &mut dyn FnMut(Vec<Box<dyn maxwell::Memory + Send + '_>>)) -> bool {
        let mut logs: Vec<Log> = (0..n).map(|_| Log::default()).collect();
        {
            let (ram, smmu, space) = (&*self.ram, self.smmu, self.space);
            run(logs.iter_mut().map(|log| Box::new(Overlay { ram, smmu, mmu: Mmu::default(), space, log }) as Box<dyn maxwell::Memory + Send + '_>).collect());
        }
        for log in logs {
            for (page, (bytes, written)) in log.pages {
                for (k, &w) in written.iter().enumerate() {
                    let at = 64 * k;
                    if w == !0 {
                        self.ram[page + at..page + at + 64].copy_from_slice(&bytes[at..at + 64]);
                    } else if w != 0 {
                        for b in (0..64).filter(|b| w >> b & 1 != 0) {
                            self.ram[page + at + b] = bytes[at + b];
                        }
                    }
                }
            }
            if let Some((va, write)) = log.fault {
                self.note(va, write);
            }
        }
        true
    }
}

/// What one view of a split wrote: each page it wrote, copied on the first write, with a bit
/// for every byte written; and the first address that did not translate.
#[derive(Default)]
struct Log {
    pages: std::collections::HashMap<usize, (Box<[u8; 0x1000]>, Box<[u64; 64]>), std::hash::BuildHasherDefault<PageHash>>,
    fault: Option<(u64, bool)>,
}

/// Page offsets hash by their page number (every shader access looks one up).
#[derive(Default)]
struct PageHash(u64);

impl std::hash::Hasher for PageHash {
    fn finish(&self) -> u64 {
        self.0
    }
    fn write(&mut self, bytes: &[u8]) {
        for &b in bytes {
            self.0 = (self.0 << 8 | b as u64).wrapping_mul(0x9e37_79b9_7f4a_7c15);
        }
    }
    fn write_usize(&mut self, n: usize) {
        self.0 = ((n >> 12) as u64).wrapping_mul(0x9e37_79b9_7f4a_7c15);
    }
}

/// Mark bytes `o..o + n` of a page written.
fn mark(written: &mut [u64; 64], o: usize, n: usize) {
    let (mut i, end) = (o, o + n);
    while i < end {
        let (k, b) = (i / 64, i % 64);
        let len = (64 - b).min(end - i);
        written[k] |= if len == 64 { !0 } else { ((1u64 << len) - 1) << b };
        i += len;
    }
}

/// Memory as one of several threads sees it during a split: RAM as it was, read-only and shared,
/// under the thread's own writes.
struct Overlay<'a> {
    ram: &'a [u8],
    smmu: &'a Smmu,
    mmu: Mmu,
    space: Space,
    log: &'a mut Log,
}

impl maxwell::Memory for Overlay<'_> {
    fn read(&self, mut va: u64, mut out: &mut [u8]) {
        while !out.is_empty() {
            let n = (0x1000 - (va & 0xfff) as usize).min(out.len());
            let (part, rest) = out.split_at_mut(n);
            match self.mmu.translate(self.space, va, self.smmu, self.ram) {
                Some(p) => {
                    let o = (va & 0xfff) as usize;
                    match (!self.log.pages.is_empty()).then(|| self.log.pages.get(&p.offset)).flatten() {
                        Some((bytes, _)) => part.copy_from_slice(&bytes[o..o + n]),
                        None => part.copy_from_slice(&self.ram[p.offset + o..p.offset + o + n]),
                    }
                }
                None => part.fill(0),
            }
            out = rest;
            va = va.wrapping_add(n as u64);
        }
    }
    fn write(&mut self, mut va: u64, mut data: &[u8]) {
        while !data.is_empty() {
            let n = (0x1000 - (va & 0xfff) as usize).min(data.len());
            match self.mmu.translate(self.space, va, self.smmu, self.ram) {
                Some(p) if p.write && p.offset + 0x1000 <= self.ram.len() => {
                    let ram = self.ram;
                    let (bytes, written) = self.log.pages.entry(p.offset).or_insert_with(|| {
                        let mut page = Box::new([0u8; 0x1000]);
                        page.copy_from_slice(&ram[p.offset..p.offset + 0x1000]);
                        (page, Box::new([0u64; 64]))
                    });
                    let o = (va & 0xfff) as usize;
                    bytes[o..o + n].copy_from_slice(&data[..n]);
                    mark(written, o, n);
                }
                _ => {
                    self.log.fault.get_or_insert((va, true));
                }
            }
            data = &data[n..];
            va = va.wrapping_add(n as u64);
        }
    }
}

/// Memory as a channel sees it, for reading on another thread.
struct Reader<'a> {
    ram: &'a [u8],
    smmu: &'a Smmu,
    mmu: Mmu,
    space: Space,
}

impl maxwell::Memory for Reader<'_> {
    fn read(&self, mut va: u64, mut out: &mut [u8]) {
        while !out.is_empty() {
            let n = (0x1000 - (va & 0xfff) as usize).min(out.len());
            let (part, rest) = out.split_at_mut(n);
            match self.mmu.translate(self.space, va, self.smmu, self.ram) {
                Some(p) => {
                    let o = p.offset + (va & 0xfff) as usize;
                    part.copy_from_slice(&self.ram[o..o + n]);
                }
                None => part.fill(0),
            }
            out = rest;
            va = va.wrapping_add(n as u64);
        }
    }
    fn write(&mut self, _va: u64, _data: &[u8]) {}
}

impl View<'_> {
    pub fn read(&self, mut va: u64, mut out: &mut [u8]) {
        while !out.is_empty() {
            let n = (0x1000 - (va & 0xfff) as usize).min(out.len());
            let (part, rest) = out.split_at_mut(n);
            match self.mmu.translate(self.space, va, self.smmu, self.ram) {
                Some(p) => {
                    let o = p.offset + (va & 0xfff) as usize;
                    part.copy_from_slice(&self.ram[o..o + n]);
                }
                None => {
                    part.fill(0);
                    self.note(va, false);
                }
            }
            out = rest;
            va = va.wrapping_add(n as u64);
        }
    }

    pub fn write(&mut self, mut va: u64, mut data: &[u8]) {
        while !data.is_empty() {
            let n = (0x1000 - (va & 0xfff) as usize).min(data.len());
            match self.mmu.translate(self.space, va, self.smmu, self.ram) {
                Some(p) if p.write => {
                    let o = p.offset + (va & 0xfff) as usize;
                    self.ram[o..o + n].copy_from_slice(&data[..n]);
                }
                _ => self.note(va, true),
            }
            data = &data[n..];
            va = va.wrapping_add(n as u64);
        }
    }

    fn note(&self, va: u64, write: bool) {
        if self.fault.get().is_none() {
            self.fault.set(Some((va, write)));
        }
    }

    pub fn read32(&self, va: u64) -> u32 {
        let mut b = [0; 4];
        self.read(va, &mut b);
        u32::from_le_bytes(b)
    }

    pub fn write32(&mut self, va: u64, v: u32) {
        self.write(va, &v.to_le_bytes());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn marking_written_ranges() {
        let mut w = [0u64; 64];
        mark(&mut w, 3, 2);
        assert_eq!(w[0], 0b11000);
        mark(&mut w, 60, 70);
        assert_eq!((w[0] >> 60, w[1], w[2]), (0xf, !0, 0b11));
        mark(&mut w, 0, 0x1000);
        assert!(w.iter().all(|&x| x == !0));
    }

    #[test]
    fn split_views_see_their_writes_and_meet_in_order() {
        use maxwell::Memory;
        let mut ram = vec![0u8; 0x10_0000];
        let put = |ram: &mut [u8], o: usize, v: u64| ram[o..o + 8].copy_from_slice(&v.to_le_bytes());
        let pa = |o: u64| RAM_BASE + o;
        // va 0x0400_5000 -> RAM 0x5000 (writable), va 0x0400_6000 unmapped
        put(&mut ram, 0x1200, pa(0x2000) | 3 | 1 << 11);
        put(&mut ram, 0x2008, pa(0x10000) << 24 | 1 << 32);
        put(&mut ram, 0x10000 + 8 * 5, pa(0x5000) >> 8 | 1);
        ram[0x5000..0x5010].fill(0xaa);
        let smmu = Smmu::default();
        let s = space(pa(0x1000), &smmu, &ram);
        let mmu = Mmu::default();
        let mut v = View { ram: &mut ram, smmu: &smmu, mmu: &mmu, space: s, fault: Cell::new(None) };
        let split = v.split(2, &mut |mut views| {
            // view 0 writes bytes 0..4, view 1 bytes 2..6: each reads its own, not the other's
            views[0].write(0x0400_5000, &[1, 1, 1, 1]);
            views[1].write(0x0400_5002, &[2, 2, 2, 2]);
            let mut b = [0; 8];
            views[0].read(0x0400_5000, &mut b);
            assert_eq!(b, [1, 1, 1, 1, 0xaa, 0xaa, 0xaa, 0xaa]);
            views[1].read(0x0400_5000, &mut b);
            assert_eq!(b, [0xaa, 0xaa, 2, 2, 2, 2, 0xaa, 0xaa]);
            views[1].write(0x0400_6000, &[9]);
        });
        assert!(split);
        // written bytes only, the later view last; a fault of a view reaches the view it came from
        assert_eq!(v.ram[0x5000..0x5008], [1, 1, 2, 2, 2, 2, 0xaa, 0xaa]);
        assert_eq!(v.fault.get(), Some((0x0400_6000, true)));
    }

    #[test]
    fn small_and_big_pages() {
        let mut ram = vec![0u8; 0x10_0000];
        let put = |ram: &mut [u8], o: usize, v: u64| ram[o..o + 8].copy_from_slice(&v.to_le_bytes());
        let pa = |o: u64| RAM_BASE + o;
        // instance block at 0x1000, directory at 0x2000, big table at 0x3000, small table at 0x10000
        put(&mut ram, 0x1200, pa(0x2000) | 3 | 1 << 11);
        put(&mut ram, 0x2008, pa(0x3000) >> 8 | 1 | (pa(0x10000) << 24 | 1 << 32));
        // va 0x0400_0000 + ...: PDE 1 with 64 MiB per entry
        put(&mut ram, 0x10000 + 8 * 5, pa(0x5000) >> 8 | 1 | 0xfe << 36);
        put(&mut ram, 0x3000 + 8 * 3, pa(0x20000) >> 8 | 1 | 4);
        ram[0x5008] = 0x42;
        ram[0x20000 + 0x1234] = 0x43;
        let smmu = Smmu::default();
        let s = space(pa(0x1000), &smmu, &ram);
        assert_eq!(s, Space { directory: pa(0x2000), big64: true });
        let mmu = Mmu::default();
        let p = mmu.translate(s, 0x0400_5008, &smmu, &ram).unwrap();
        assert_eq!(p, Page { offset: 0x5000, write: true, kind: 0xfe });
        let mut v = View { ram: &mut ram, smmu: &smmu, mmu: &mmu, space: s, fault: Cell::new(None) };
        let mut b = [0; 1];
        v.read(0x0403_1234, &mut b);
        assert_eq!(b, [0x43]);
        assert_eq!(v.fault.get(), None);
        v.write(0x0403_1234, &[1]);
        assert_eq!(v.fault.get(), Some((0x0403_1234, true)), "read only");
        assert_eq!(v.mmu.translate(s, 0x0400_6000, &smmu, v.ram), None);
    }
}
