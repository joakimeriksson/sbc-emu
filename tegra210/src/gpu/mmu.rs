//! The GPU's memory management unit: per-address-space page directories (8 bytes per 128 MiB, or
//! 64 MiB with 64 KiB big pages) whose entries point at a big and a small (4 KiB) page table.
//! Physical addresses with bit 34 set go through the SMMU.
use crate::smmu::{Group, Smmu};
use crate::RAM_BASE;
use std::cell::Cell;
use std::sync::atomic::{AtomicU32, Ordering::Relaxed};
use std::sync::OnceLock;

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
/// The TLB of each thread of a split (made for every compute launch).
const SPLIT_TLB: usize = 128;

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
        Self::with_entries(TLB)
    }
}

impl Mmu {
    /// A TLB of `n` entries (a power of two).
    pub fn with_entries(n: usize) -> Self {
        debug_assert!(n.is_power_of_two());
        Self { tlb: vec![Cell::new(NONE); n] }
    }
}

/// RAM as the page tables are read from it: bytes, or the words threads share during a split.
pub trait Words {
    /// Its size in bytes.
    fn size(&self) -> usize;
    /// The word at byte offset `o` (a multiple of four).
    fn word(&self, o: usize) -> Option<u32>;
}

impl Words for [u8] {
    fn size(&self) -> usize {
        self.len()
    }
    fn word(&self, o: usize) -> Option<u32> {
        Some(u32::from_le_bytes(self.get(o..o.checked_add(4)?)?.try_into().unwrap()))
    }
}

impl Words for [AtomicU32] {
    fn size(&self) -> usize {
        self.len() * 4
    }
    fn word(&self, o: usize) -> Option<u32> {
        if o % 4 != 0 {
            return None;
        }
        self.get(o / 4).map(|w| w.load(Relaxed))
    }
}

/// Offset in RAM of a GPU physical address.
pub fn physical(pa: u64, smmu: &Smmu, ram: &[u8]) -> Option<usize> {
    physical_in(pa, smmu, ram)
}

fn physical_in<R: Words + ?Sized>(pa: u64, smmu: &Smmu, ram: &R) -> Option<usize> {
    let pa = if pa >> 34 & 1 != 0 {
        let word = |p: u64| ram.word(usize::try_from(p.checked_sub(RAM_BASE)?).ok()?);
        smmu.spaces(Group::Gpu)?.walk_with(pa & 0x3_ffff_ffff, false, &word)?
    } else {
        pa
    };
    let o = usize::try_from(pa.checked_sub(RAM_BASE)?).ok()?;
    (o < ram.size()).then_some(o)
}

fn read64_in<R: Words + ?Sized>(pa: u64, smmu: &Smmu, ram: &R) -> Option<u64> {
    let o = physical_in(pa, smmu, ram)?;
    Some(ram.word(o)? as u64 | (ram.word(o.checked_add(4)?)? as u64) << 32)
}

pub fn read64(pa: u64, smmu: &Smmu, ram: &[u8]) -> Option<u64> {
    read64_in(pa, smmu, ram)
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
    #[inline(always)]
    pub fn translate<R: Words + ?Sized>(&self, s: Space, va: u64, smmu: &Smmu, ram: &R) -> Option<Page> {
        let page = va >> 12;
        let slot = &self.tlb[(page ^ page >> 10) as usize & (self.tlb.len() - 1)];
        let e = slot.get();
        if e.page == page && e.space == s.directory {
            return Some(e.to);
        }
        let to = walk(s, va, smmu, ram)?;
        slot.set(Entry { space: s.directory, page, to });
        Some(to)
    }
}

#[inline(never)]
fn walk<R: Words + ?Sized>(s: Space, va: u64, smmu: &Smmu, ram: &R) -> Option<Page> {
    if s.directory == 0 || va >> 40 != 0 {
        return None;
    }
    let (pde_shift, big_shift) = if s.big64 { (26, 16) } else { (27, 17) };
    let pde = read64_in(s.directory + (va >> pde_shift) * 8, smmu, ram)?;
    let within = va & ((1 << pde_shift) - 1);
    let mut found = None;
    if pde >> 32 & 3 != 0 {
        let table = (pde >> 36) << 12;
        found = read64_in(table + (within >> 12) * 8, smmu, ram).filter(|p| p & 1 != 0).map(|p| (p, 12));
    }
    if found.is_none() && pde & 3 != 0 {
        let table = (pde & 0xffff_fff0) << 8;
        found = read64_in(table + (within >> big_shift) * 8, smmu, ram).filter(|p| p & 1 != 0).map(|p| (p, big_shift));
    }
    let (pte, shift) = found?;
    let pa = ((pte & 0xffff_fff0) << 8) + (va & ((1 << shift) - 1) & !0xfff);
    Some(Page { offset: physical_in(pa, smmu, ram)?, write: pte & 4 == 0, kind: (pte >> 36) as u8 })
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
        let Some(words) = atomic_words(self.ram) else {
            return false;
        };
        let faults: Vec<OnceLock<(u64, bool)>> = (0..n).map(|_| OnceLock::new()).collect();
        let (smmu, space) = (self.smmu, self.space);
        let view = |fault| Box::new(Shared { ram: words, smmu, mmu: Mmu::with_entries(SPLIT_TLB), space, last: Cell::new((u64::MAX, 0, false)), fault }) as Box<dyn maxwell::Memory + Send + '_>;
        run(faults.iter().map(view).collect());
        if let Some(&(va, write)) = faults.iter().find_map(|f| f.get()) {
            self.note(va, write);
        }
        true
    }
}

/// RAM as words that several threads read and write at once. None if it is not word-aligned.
fn atomic_words(ram: &mut [u8]) -> Option<&[AtomicU32]> {
    if ram.as_ptr().align_offset(std::mem::align_of::<AtomicU32>()) != 0 {
        return None;
    }
    // SAFETY: AtomicU32 has the size and in-memory representation of u32, and the pointer is
    // aligned for it (checked above). The slice borrows `ram` mutably for its whole lifetime, so
    // every access to these bytes meanwhile is through the atomics (what `AtomicU32::from_mut_slice`
    // does, which is not stable yet).
    Some(unsafe { std::slice::from_raw_parts(ram.as_mut_ptr().cast::<AtomicU32>(), ram.len() / 4) })
}

/// Bytes of RAM at byte offset `at`, from the shared words. False if they are not all in RAM.
#[inline(always)]
fn load(ram: &[AtomicU32], at: usize, out: &mut [u8]) -> bool {
    let Some(end) = at.checked_add(out.len()).filter(|&e| e <= ram.len() * 4) else {
        return false;
    };
    // (what shaders load: a part of a word, or aligned words)
    let word = |o: usize| ram[o / 4].load(Relaxed);
    match out.len() {
        1 => {
            out[0] = (word(at) >> (8 * (at % 4))) as u8;
            return true;
        }
        2 if at % 4 != 3 => {
            *<&mut [u8; 2]>::try_from(out).unwrap() = ((word(at) >> (8 * (at % 4))) as u16).to_le_bytes();
            return true;
        }
        4 if at % 4 == 0 => {
            *<&mut [u8; 4]>::try_from(out).unwrap() = word(at).to_le_bytes();
            return true;
        }
        8 if at % 4 == 0 => {
            *<&mut [u8; 8]>::try_from(out).unwrap() = (word(at) as u64 | (word(at + 4) as u64) << 32).to_le_bytes();
            return true;
        }
        _ => {}
    }
    if at % 4 == 0 && out.len() % 4 == 0 {
        for (o, w) in out.chunks_exact_mut(4).zip(&ram[at / 4..end / 4]) {
            o.copy_from_slice(&w.load(Relaxed).to_le_bytes());
        }
        return true;
    }
    let mut k = 0;
    while k < out.len() {
        let (o, sh) = ((at + k) / 4, (at + k) % 4);
        let n = (4 - sh).min(out.len() - k);
        let w = ram[o].load(Relaxed).to_le_bytes();
        out[k..k + n].copy_from_slice(&w[sh..sh + n]);
        k += n;
    }
    true
}

/// Writes bytes at byte offset `at` to the shared words: whole words with a store, parts of
/// one by compare and swap (so that the word's other bytes keep what another thread wrote to
/// them). False if they are not all in RAM.
#[inline(always)]
fn store(ram: &[AtomicU32], at: usize, data: &[u8]) -> bool {
    if at.checked_add(data.len()).is_none_or(|e| e > ram.len() * 4) {
        return false;
    }
    if data.len() == 4 && at % 4 == 0 {
        ram[at / 4].store(u32::from_le_bytes(data.try_into().unwrap()), Relaxed);
        return true;
    }
    let mut k = 0;
    while k < data.len() {
        let (o, sh) = ((at + k) / 4, (at + k) % 4);
        let n = (4 - sh).min(data.len() - k);
        if n == 4 {
            ram[o].store(u32::from_le_bytes(data[k..k + 4].try_into().unwrap()), Relaxed);
        } else {
            let mut b = [0u8; 4];
            b[sh..sh + n].copy_from_slice(&data[k..k + n]);
            let v = u32::from_le_bytes(b);
            let mask = (u32::MAX >> (32 - 8 * n as u32)) << (8 * sh as u32);
            let _ = ram[o].fetch_update(Relaxed, Relaxed, |w| Some(w & !mask | v));
        }
        k += n;
    }
    true
}

/// Memory as one of several threads sees it during a split (a compute launch's workgroups):
/// RAM is shared, as words read and written atomically (relaxed), so each thread sees what the
/// others wrote, and workgroups that write the same bytes at once (a race in the guest's
/// program, as on the GPU) leave one of the values written in each of them. (A cache line of its
/// own: it is written as its thread goes from page to page.)
#[repr(align(128))]
struct Shared<'a> {
    ram: &'a [AtomicU32],
    smmu: &'a Smmu,
    mmu: Mmu,
    space: Space,
    /// the page used last (a warp's lanes mostly use the same): its number, RAM offset, writable
    last: Cell<(u64, usize, bool)>,
    /// the first address that did not translate
    fault: &'a OnceLock<(u64, bool)>,
}

impl Shared<'_> {
    /// The RAM offset of the page of `va` (all of it in RAM), if it may be read (written).
    #[inline(always)]
    fn page(&self, va: u64, write: bool) -> Option<usize> {
        let (page, at, w) = self.last.get();
        if page == va >> 12 && (w || !write) {
            return Some(at);
        }
        let p = self.mmu.translate(self.space, va, self.smmu, self.ram)?;
        if (write && !p.write) || p.offset + 0x1000 > self.ram.len() * 4 {
            return None;
        }
        self.last.set((va >> 12, p.offset, p.write));
        Some(p.offset)
    }
}

impl maxwell::Memory for Shared<'_> {
    fn read_lanes(&self, at: &[u64], mask: u32, n: usize, out: &mut [[u8; 16]]) {
        let mut m = mask;
        while m != 0 {
            let i = m.trailing_zeros() as usize;
            m &= m - 1;
            let va = at[i];
            // (within a page, as nearly every access is)
            let o = (va & 0xfff) as usize;
            if o + n <= 0x1000 {
                if let Some(p) = self.page(va, false) {
                    if load(self.ram, p + o, &mut out[i][..n]) {
                        continue;
                    }
                }
            }
            self.read(va, &mut out[i][..n]);
        }
    }
    fn write_lanes(&mut self, at: &[u64], mask: u32, n: usize, data: &[[u8; 16]]) {
        let mut m = mask;
        while m != 0 {
            let i = m.trailing_zeros() as usize;
            m &= m - 1;
            let va = at[i];
            let o = (va & 0xfff) as usize;
            if o + n <= 0x1000 {
                if let Some(p) = self.page(va, true) {
                    if store(self.ram, p + o, &data[i][..n]) {
                        continue;
                    }
                }
            }
            self.write(va, &data[i][..n]);
        }
    }
    #[inline]
    fn read(&self, mut va: u64, mut out: &mut [u8]) {
        while !out.is_empty() {
            let n = (0x1000 - (va & 0xfff) as usize).min(out.len());
            let (part, rest) = out.split_at_mut(n);
            if !self.page(va, false).is_some_and(|at| load(self.ram, at + (va & 0xfff) as usize, part)) {
                part.fill(0);
                let _ = self.fault.set((va, false));
            }
            out = rest;
            va = va.wrapping_add(n as u64);
        }
    }
    #[inline]
    fn write(&mut self, mut va: u64, mut data: &[u8]) {
        while !data.is_empty() {
            let n = (0x1000 - (va & 0xfff) as usize).min(data.len());
            if !self.page(va, true).is_some_and(|at| store(self.ram, at + (va & 0xfff) as usize, &data[..n])) {
                let _ = self.fault.set((va, true));
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

    /// RAM with va 0x0400_5000 -> RAM 0x5000 (writable, 0xaa in its first 16 bytes) and va
    /// 0x0400_6000 unmapped; the address space.
    fn mapped() -> (Vec<u8>, Space) {
        let mut ram = vec![0u8; 0x10_0000];
        let put = |ram: &mut [u8], o: usize, v: u64| ram[o..o + 8].copy_from_slice(&v.to_le_bytes());
        let pa = |o: u64| RAM_BASE + o;
        put(&mut ram, 0x1200, pa(0x2000) | 3 | 1 << 11);
        put(&mut ram, 0x2008, pa(0x10000) << 24 | 1 << 32);
        put(&mut ram, 0x10000 + 8 * 5, pa(0x5000) >> 8 | 1);
        ram[0x5000..0x5010].fill(0xaa);
        let s = space(pa(0x1000), &Smmu::default(), &ram);
        (ram, s)
    }

    #[test]
    fn split_views_share_memory() {
        use maxwell::Memory;
        let (mut ram, s) = mapped();
        let (smmu, mmu) = (Smmu::default(), Mmu::default());
        let mut v = View { ram: &mut ram, smmu: &smmu, mmu: &mmu, space: s, fault: Cell::new(None) };
        let split = v.split(2, &mut |mut views| {
            // parts of words keep the bytes around them; each view sees the other's writes
            views[0].write(0x0400_5000, &[1, 1, 1, 1]);
            views[1].write(0x0400_5002, &[2, 2, 2, 2]);
            let mut b = [0; 8];
            views[0].read(0x0400_5000, &mut b);
            assert_eq!(b, [1, 1, 2, 2, 2, 2, 0xaa, 0xaa]);
            views[1].write(0x0400_5007, &[3]);
            views[0].read(0x0400_5005, &mut b[..3]);
            assert_eq!(b[..3], [2, 0xaa, 3]);
            views[1].write(0x0400_6000, &[9]);
        });
        assert!(split);
        assert_eq!(v.ram[0x5000..0x5009], [1, 1, 2, 2, 2, 2, 0xaa, 3, 0xaa]);
        // a fault of a view reaches the view it came from
        assert_eq!(v.fault.get(), Some((0x0400_6000, true)));
    }

    /// Threads that write bytes of the same words at once: every byte ends up as one thread
    /// wrote it, and bytes nobody wrote keep their value.
    #[test]
    fn split_views_race_on_bytes() {
        use maxwell::Memory;
        let (mut ram, s) = mapped();
        let (smmu, mmu) = (Smmu::default(), Mmu::default());
        let mut v = View { ram: &mut ram, smmu: &smmu, mmu: &mmu, space: s, fault: Cell::new(None) };
        let split = v.split(4, &mut |views| {
            std::thread::scope(|sc| {
                for (k, mut view) in views.into_iter().enumerate() {
                    sc.spawn(move || {
                        for round in 0..20_000u32 {
                            // thread k writes byte k of word 0 and words 1..4, and all of word 4 now and then
                            view.write(0x0400_5100 + k as u64, &[k as u8 + 1]);
                            view.write(0x0400_5104 + 4 * k as u64, &(round ^ k as u32).to_le_bytes()[..2]);
                            if round % 7 == 0 {
                                view.write(0x0400_5114, &[k as u8; 4]);
                            }
                        }
                    });
                }
            });
        });
        assert!(split);
        let r = &v.ram[0x5100..0x5118];
        assert_eq!(r[..4], [1, 2, 3, 4]);
        for k in 0..4 {
            assert_eq!(r[4 + 4 * k + 2..4 + 4 * k + 4], [0, 0], "untouched halves");
            assert_eq!(u16::from_le_bytes([r[4 + 4 * k], r[5 + 4 * k]]), (19_999 ^ k) as u16);
        }
        assert!(r[20..24].iter().all(|&b| b == r[20]) && r[20] < 4, "a whole word store is one thread's: {:?}", &r[20..24]);
        assert_eq!(v.fault.get(), None);
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
        let p = mmu.translate(s, 0x0400_5008, &smmu, &ram[..]).unwrap();
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
