//! The GPU's memory management unit: per-address-space page directories (8 bytes per 128 MiB, or
//! 64 MiB with 64 KiB big pages) whose entries point at a big and a small (4 KiB) page table.
//! Physical addresses with bit 34 set go through the SMMU.
use crate::smmu::{Group, Smmu};
use crate::RAM_BASE;
use std::cell::Cell;
use std::sync::atomic::{AtomicU64, Ordering::Relaxed};
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

impl Words for [AtomicU64] {
    fn size(&self) -> usize {
        self.len() * 8
    }
    fn word(&self, o: usize) -> Option<u32> {
        if o % 4 != 0 {
            return None;
        }
        self.get(o / 8).map(|w| (w.load(Relaxed) >> (8 * (o % 8))) as u32)
    }
}

// (the byte lanes of the shared words are counted from the least significant)
#[cfg(not(target_endian = "little"))]
compile_error!("the GPU's shared RAM words assume a little-endian host");

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

/// RAM as 64-bit words that several threads read and write at once (a naturally aligned 8-byte
/// access is one atomic access, as on the GPU). None if it is not aligned for them.
fn atomic_words(ram: &mut [u8]) -> Option<&[AtomicU64]> {
    if ram.as_ptr().align_offset(std::mem::align_of::<AtomicU64>()) != 0 {
        return None;
    }
    // SAFETY: AtomicU64 has the size and in-memory representation of u64, and the pointer is
    // aligned for it (checked above). The slice borrows `ram` mutably for its whole lifetime, so
    // every access to these bytes meanwhile is through the atomics (what `AtomicU64::from_mut_slice`
    // does, which is not stable yet).
    Some(unsafe { std::slice::from_raw_parts(ram.as_mut_ptr().cast::<AtomicU64>(), ram.len() / 8) })
}

/// Bytes of RAM at byte offset `at`, from the shared words. False if they are not all in RAM.
/// An access within one word is one atomic load.
#[inline(always)]
fn load(ram: &[AtomicU64], at: usize, out: &mut [u8]) -> bool {
    if at.checked_add(out.len()).is_none_or(|e| e > ram.len() * 8) {
        return false;
    }
    let word = |o: usize| ram[o / 8].load(Relaxed);
    let (n, sh) = (out.len(), at % 8);
    if n <= 8 && sh + n <= 8 {
        let v = word(at) >> (8 * sh);
        match n {
            1 => out[0] = v as u8,
            2 => *<&mut [u8; 2]>::try_from(out).unwrap() = (v as u16).to_le_bytes(),
            4 => *<&mut [u8; 4]>::try_from(out).unwrap() = (v as u32).to_le_bytes(),
            8 => *<&mut [u8; 8]>::try_from(out).unwrap() = v.to_le_bytes(),
            _ => out.copy_from_slice(&v.to_le_bytes()[..n]),
        }
        return true;
    }
    let mut k = 0;
    while k < n {
        let (o, sh) = ((at + k) / 8, (at + k) % 8);
        let m = (8 - sh).min(n - k);
        let w = ram[o].load(Relaxed).to_le_bytes();
        out[k..k + m].copy_from_slice(&w[sh..sh + m]);
        k += m;
    }
    true
}

/// Writes the bytes `put` selects of `v` into a shared word: a store if it is all of them, else
/// by compare and swap (so that the word's other bytes keep what another thread wrote to them).
#[inline(always)]
fn put_word(word: &AtomicU64, v: u64, put: u64) {
    if put == !0 {
        word.store(v, Relaxed);
    } else if put != 0 {
        let _ = word.fetch_update(Relaxed, Relaxed, |x| Some(x & !put | v & put));
    }
}

/// Writes bytes at byte offset `at` to the shared words, each word touched once. False if they
/// are not all in RAM.
#[inline(always)]
fn store(ram: &[AtomicU64], at: usize, data: &[u8]) -> bool {
    if at.checked_add(data.len()).is_none_or(|e| e > ram.len() * 8) {
        return false;
    }
    let mut k = 0;
    while k < data.len() {
        let (o, sh) = ((at + k) / 8, (at + k) % 8);
        let n = (8 - sh).min(data.len() - k);
        let mut b = [0u8; 8];
        b[sh..sh + n].copy_from_slice(&data[k..k + n]);
        let put = (u64::MAX >> (64 - 8 * n as u32)) << (8 * sh as u32);
        put_word(&ram[o], u64::from_le_bytes(b), put);
        k += n;
    }
    true
}

/// Memory as one of several threads sees it during a split (a compute launch's workgroups):
/// RAM is shared, as 64-bit words read and written atomically (relaxed), so each thread sees what
/// the others wrote, an aligned access of up to 8 bytes does not tear, and workgroups that write
/// the same bytes at once (a race in the guest's program, as on the GPU) leave one of the values
/// written in each of them. MEMBAR orders them (`exec`). (A cache line of its
/// own: it is written as its thread goes from page to page.)
#[repr(align(128))]
struct Shared<'a> {
    ram: &'a [AtomicU64],
    smmu: &'a Smmu,
    mmu: Mmu,
    space: Space,
    /// the page used last (a warp's lanes mostly use the same): its number, RAM offset, writable
    last: Cell<(u64, usize, bool)>,
    /// the first address that did not translate
    fault: &'a OnceLock<(u64, bool)>,
}

impl Shared<'_> {
    /// The run of consecutive elements a warp's load or store is, if it is one: the first and
    /// last lane, and the RAM offset of the first's bytes (lane i's at it + (i - first) * n,
    /// all in one page).
    #[inline]
    fn run(&self, at: &[u64], mask: u32, n: usize, write: bool) -> Option<(usize, usize, usize)> {
        if mask == 0 {
            return None;
        }
        let (first, last) = (mask.trailing_zeros() as usize, 31 - mask.leading_zeros() as usize);
        let base = *at.get(first)?;
        let o = (base & 0xfff) as usize;
        if last >= at.len() || o + (last - first + 1) * n > 0x1000 {
            return None;
        }
        if (first..=last).any(|i| mask >> i & 1 != 0 && at[i] != base.wrapping_add(((i - first) * n) as u64)) {
            return None;
        }
        let p = self.page(base, write)?;
        Some((first, last, p + o))
    }

    /// A warp's load of consecutive elements: the words once, then each lane's part.
    #[inline]
    fn read_run(&self, at: &[u64], mask: u32, n: usize, out: &mut [[u8; 16]]) -> bool {
        let Some((first, last, from)) = self.run(at, mask, n, false) else { return false };
        let skip = from % 8;
        let words = (skip + (last - first + 1) * n).div_ceil(8);
        let Some(src) = self.ram.get(from / 8..from / 8 + words) else { return false };
        if last >= out.len() {
            return false;
        }
        // (16 bytes past the last lane's, so that every lane copies 16)
        let mut buf = [0u8; 33 * 16 + 8];
        for (k, w) in src.iter().enumerate() {
            buf[8 * k..8 * k + 8].copy_from_slice(&w.load(Relaxed).to_le_bytes());
        }
        let mut m = mask;
        while m != 0 {
            let i = m.trailing_zeros() as usize;
            m &= m - 1;
            let at = skip + (i - first) * n;
            out[i] = buf[at..at + 16].try_into().unwrap();
        }
        true
    }

    /// A warp's store of consecutive elements: the lanes' bytes put together, then each word
    /// written once (whole, or the bytes of it the lanes write by compare and swap).
    #[inline]
    fn write_run(&self, at: &[u64], mask: u32, n: usize, data: &[[u8; 16]]) -> bool {
        let Some((first, last, to)) = self.run(at, mask, n, true) else { return false };
        let skip = to % 8;
        let words = (skip + (last - first + 1) * n).div_ceil(8);
        if to / 8 + words > self.ram.len() || last >= data.len() {
            return false;
        }
        let mut buf = [0u8; 33 * 16 + 16];
        let mut put = [0u8; 33 * 16 + 16];
        let mut m = mask;
        while m != 0 {
            let i = m.trailing_zeros() as usize;
            m &= m - 1;
            let at = skip + (i - first) * n;
            buf[at..at + n].copy_from_slice(&data[i][..n]);
            put[at..at + n].fill(0xff);
        }
        for k in 0..words {
            let v = u64::from_le_bytes(buf[8 * k..8 * k + 8].try_into().unwrap());
            let w = u64::from_le_bytes(put[8 * k..8 * k + 8].try_into().unwrap());
            put_word(&self.ram[to / 8 + k], v, w);
        }
        true
    }

    /// The RAM offset of the page of `va` (all of it in RAM), if it may be read (written).
    #[inline(always)]
    fn page(&self, va: u64, write: bool) -> Option<usize> {
        let (page, at, w) = self.last.get();
        if page == va >> 12 && (w || !write) {
            return Some(at);
        }
        let p = self.mmu.translate(self.space, va, self.smmu, self.ram)?;
        if (write && !p.write) || p.offset + 0x1000 > self.ram.len() * 8 {
            return None;
        }
        self.last.set((va >> 12, p.offset, p.write));
        Some(p.offset)
    }
}

impl maxwell::Memory for Shared<'_> {
    fn read_lanes(&self, at: &[u64], mask: u32, n: usize, out: &mut [[u8; 16]]) {
        if self.read_run(at, mask, n, out) {
            return;
        }
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
        if self.write_run(at, mask, n, data) {
            return;
        }
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
            // a warp's loads and stores, as a run of consecutive elements or not, are each lane's
            let bytes: Vec<u8> = (0..64).map(|k| k as u8 * 3 + 1).collect();
            views[0].write(0x0400_5100, &bytes);
            for (n, step, mask) in [(2usize, 2u64, !0u32), (4, 4, 0xffff_00f0), (2, 6, !0), (1, 1, 0x8000_0001), (4, 4, !0)] {
                let at: Vec<u64> = (0..32).map(|i| 0x0400_5100 + 0x10 + i as u64 * step).collect();
                let mut out = vec![[0u8; 16]; 32];
                views[1].read_lanes(&at, mask, n, &mut out);
                for i in (0..32).filter(|i| mask >> i & 1 != 0) {
                    let mut one = [0u8; 16];
                    views[1].read(at[i], &mut one[..n]);
                    assert_eq!(out[i][..n], one[..n], "n {n} step {step} lane {i}");
                }
            }
            for (n, start, mask) in [(2usize, 0x0400_5302u64, 0x7fff_fffeu32), (1, 0x0400_5401, 0xf0f0_f0f0), (4, 0x0400_5500, !0)] {
                let mut before = [0u8; 0x100];
                views[0].read(start - 0x10, &mut before);
                let at: Vec<u64> = (0..32).map(|i| start + (i * n) as u64).collect();
                let data: Vec<[u8; 16]> = (0..32).map(|i| [0x80 + i as u8; 16]).collect();
                views[1].write_lanes(&at, mask, n, &data);
                let mut after = [0u8; 0x100];
                views[0].read(start - 0x10, &mut after);
                for (k, (&b, &a)) in before.iter().zip(&after).enumerate() {
                    let rel = k as i64 - 0x10;
                    let lane = (rel >= 0).then(|| rel as usize / n).filter(|&i| i < 32 && mask >> i & 1 != 0);
                    assert_eq!(a, lane.map_or(b, |i| 0x80 + i as u8), "n {n} byte {rel}");
                }
            }
        });
        assert!(split);
        assert_eq!(v.ram[0x5000..0x5009], [1, 1, 2, 2, 2, 2, 0xaa, 3, 0xaa]);
        // a fault of a view reaches the view it came from
        assert_eq!(v.fault.get(), Some((0x0400_6000, true)));
    }

    /// Threads that write bytes of the same words at once: every byte ends up as one thread
    /// wrote it, bytes nobody wrote keep their value, and 8-byte accesses do not tear.
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
                            // an aligned 8-byte access is one: never half of one store and half of another
                            view.write(0x0400_5118, &[k as u8 + 1; 8]);
                            let mut b = [0u8; 8];
                            view.read(0x0400_5118, &mut b);
                            assert!(b.iter().all(|&x| x == b[0]), "torn: {b:?}");
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
