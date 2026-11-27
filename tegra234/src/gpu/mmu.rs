//! GA10B's five-level GMMU with 4 KiB and 64 KiB system-memory pages.
use crate::RAM_BASE;
use std::cell::{Cell, RefCell};
use std::sync::atomic::{AtomicU64, Ordering::Relaxed};
use std::sync::OnceLock;

pub(super) fn physical(ram: &[u8], address: u64, len: usize) -> Option<usize> {
    let offset = usize::try_from(address.checked_sub(RAM_BASE)?).ok()?;
    (offset.checked_add(len)? <= ram.len()).then_some(offset)
}

pub(super) fn word(ram: &[u8], address: u64) -> Option<u32> {
    let offset = physical(ram, address, 4)?;
    Some(u32::from_le_bytes(ram[offset..offset + 4].try_into().ok()?))
}

/// The physical address of `address` through the page tables at `root` (`pte` reads an entry),
/// if mapped (and writable, for a write).
fn walk(root: u64, address: u64, write: bool, pte: impl Fn(u64) -> Option<u64>) -> Option<u64> {
    let mut table = root;
    for (shift, mask) in [(47, 3), (38, 511), (29, 511)] {
        let entry = pte(table + ((address >> shift) & mask) * 8)?;
        if entry & 6 < 4 {
            return None;
        }
        table = (entry << 4) & 0xff_ffff_f000;
    }
    let dual = table + (address >> 21 & 255) * 16;
    let big = pte(dual)?;
    let small = pte(dual + 8)?;
    // nvgpu can map 4 KiB and 64 KiB pages in one 2 MiB region: the small page first
    let mut tables = [(small, 0xff_ffff_f000, 12), (big, 0xff_ffff_ff00, 16)].into_iter();
    tables.find_map(|(pde, table_mask, shift)| {
        if pde & 6 < 4 {
            return None;
        }
        let table = (pde << 4) & table_mask;
        let index_mask = (1 << (21 - shift)) - 1;
        let entry = pte(table + (address >> shift & index_mask) * 8)?;
        if entry & 1 == 0 || entry & 6 < 4 || (write && entry & 0x40 != 0) {
            return None;
        }
        let page_mask = (1 << shift) - 1;
        Some(((entry << 4) & 0xff_ffff_f000 & !page_mask) | (address & page_mask))
    })
}

pub(super) struct View<'a> {
    pub ram: &'a mut [u8],
    pub(super) root: u64,
    /// Recent 4 KiB translations, direct-mapped: (virtual page | 1, physical page | read-only).
    /// The page tables do not change while a view lives (one method or launch).
    tlb: std::cell::RefCell<[(u64, u64); TLB]>,
}

const TLB: usize = 64;

impl<'a> View<'a> {
    pub fn from_instance(ram: &'a mut [u8], instance: u64) -> Option<Self> {
        let lo = word(ram, instance.checked_add(0x200)?)?;
        let hi = word(ram, instance.checked_add(0x204)?)?;
        let root = u64::from(lo & !0xfff) | u64::from(hi & 0xff) << 32;
        Some(Self::new(ram, root))
    }

    pub fn new(ram: &'a mut [u8], root: u64) -> Self {
        Self { ram, root, tlb: std::cell::RefCell::new([(0, 0); TLB]) }
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
        let page = address >> 12;
        let slot = (page as usize) % TLB;
        let (tag, entry) = self.tlb.borrow()[slot];
        if tag == page << 1 | 1 && !(write && entry & 1 != 0) {
            return Some((entry & !0xfff) | (address & 0xfff));
        }
        let physical = self.walk(address, write)?;
        // (whether the page is read-only: a write looks it up again)
        let read_only = write.then_some(false).unwrap_or_else(|| self.walk(address, true).is_none());
        self.tlb.borrow_mut()[slot] = (page << 1 | 1, (physical & !0xfff) | read_only as u64);
        Some(physical)
    }

    fn walk(&self, address: u64, write: bool) -> Option<u64> {
        walk(self.root, address, write, |a| self.pte(a))
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

/// Guest memory through the GPU page tables for the Tegra210 engines, which take `maxwell::Memory`.
/// A fault reads zeros and drops writes; `ok` reports whether one happened.
pub(super) struct Guest<'a, 'b> {
    view: &'a mut View<'b>,
    fault: std::cell::Cell<bool>,
    /// The first address that faulted, whether a write, and the shader's last memory instruction.
    pub first_fault: std::cell::Cell<Option<(u64, bool, u32)>>,
}

impl<'a, 'b> Guest<'a, 'b> {
    pub fn new(view: &'a mut View<'b>) -> Self {
        Self {
            view,
            fault: Default::default(),
            first_fault: Default::default(),
        }
    }

    pub fn ok(&self) -> Option<()> {
        (!self.fault.get()).then_some(())
    }

    /// For a fault report: the mapped 4 KiB pages next to an unmapped address, the end of those
    /// below it and the start of those above it (searching 256 MiB each way).
    pub fn extent(&self, address: u64) -> (Option<u64>, Option<u64>) {
        let page = address & !4095;
        let mapped = |a: u64| self.view.translate(a, false).is_some();
        let below = (1..65536u64).map(|k| page.wrapping_sub(k * 4096)).find(|&a| mapped(a)).map(|a| a + 4096);
        let above = (1..65536u64).map(|k| page + k * 4096).find(|&a| mapped(a));
        (below, above)
    }
}

impl maxwell::Memory for Guest<'_, '_> {
    fn split_atomics(&self) -> bool {
        true
    }

    /// A launch's workgroups on several threads: each its view of RAM shared as atomic words
    /// (as Tegra210's GPU does it), with its own translations.
    fn split(&mut self, n: usize, run: &mut dyn FnMut(Vec<Box<dyn maxwell::Memory + Send + '_>>)) -> bool {
        let root = self.view.root;
        let Some(words) = atomic_words(self.view.ram) else {
            return false;
        };
        let faults: Vec<OnceLock<(u64, bool, u32)>> = (0..n).map(|_| OnceLock::new()).collect();
        let view = |fault| {
            Box::new(Shared { ram: words, root, tlb: RefCell::new(vec![(0, 0); SPLIT_TLB]), last: Cell::new((u64::MAX, 0, false)), fault })
                as Box<dyn maxwell::Memory + Send + '_>
        };
        run(faults.iter().map(view).collect());
        if let Some(&f) = faults.iter().find_map(|f| f.get()) {
            self.fault.set(true);
            if self.first_fault.get().is_none() {
                self.first_fault.set(Some(f));
            }
        }
        true
    }

    fn read(&self, mut address: u64, mut out: &mut [u8]) {
        while !out.is_empty() {
            let n = out.len().min(4096 - (address as usize & 4095));
            let source = self
                .view
                .translate(address, false)
                .and_then(|physical_address| physical(self.view.ram, physical_address, n));
            match source {
                Some(offset) => out[..n].copy_from_slice(&self.view.ram[offset..offset + n]),
                None => {
                    out[..n].fill(0);
                    self.fault.set(true);
                    if self.first_fault.get().is_none() {
                        self.first_fault.set(Some((address, false, maxwell::compute::last_memory_pc())));
                    }
                }
            }
            address += n as u64;
            out = &mut out[n..];
        }
    }

    fn write(&mut self, address: u64, data: &[u8]) {
        if self.view.write(address, data).is_none() {
            self.fault.set(true);
            if self.first_fault.get().is_none() {
                self.first_fault.set(Some((address, true, maxwell::compute::last_memory_pc())));
            }
        }
    }
}

const SPLIT_TLB: usize = 256;

/// RAM as 64-bit words that several threads read and write at once (a naturally aligned 8-byte
/// access is one atomic access, as on the GPU). None if it is not aligned for them.
fn atomic_words(ram: &mut [u8]) -> Option<&[AtomicU64]> {
    if ram.as_ptr().align_offset(std::mem::align_of::<AtomicU64>()) != 0 {
        return None;
    }
    // SAFETY: AtomicU64 has the size and in-memory representation of u64, and the pointer is
    // aligned for it (checked above). The slice borrows `ram` mutably for its whole lifetime, so
    // every access to these bytes meanwhile is through the atomics.
    Some(unsafe { std::slice::from_raw_parts(ram.as_mut_ptr().cast::<AtomicU64>(), ram.len() / 8) })
}

/// Bytes of RAM at byte offset `at`, from the shared words. False if they are not all in RAM.
#[inline(always)]
fn load(ram: &[AtomicU64], at: usize, out: &mut [u8]) -> bool {
    if at.checked_add(out.len()).is_none_or(|e| e > ram.len() * 8) {
        return false;
    }
    let (n, sh) = (out.len(), at % 8);
    if n <= 8 && sh + n <= 8 {
        let v = ram[at / 8].load(Relaxed) >> (8 * sh);
        out.copy_from_slice(&v.to_le_bytes()[..n]);
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

/// Writes bytes at byte offset `at` to the shared words, each word touched once.
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
/// RAM shared as relaxed atomic words, so each thread sees what the others wrote and an aligned
/// access of up to 8 bytes does not tear. (A cache line of its own.)
#[repr(align(128))]
struct Shared<'a> {
    ram: &'a [AtomicU64],
    root: u64,
    /// 4 KiB translations, direct-mapped: (virtual page | 1, RAM offset | writable)
    tlb: RefCell<Vec<(u64, u64)>>,
    /// the page used last: its number, RAM offset, writable
    last: Cell<(u64, usize, bool)>,
    /// the first address that did not translate, whether a write, the shader's memory PC
    fault: &'a OnceLock<(u64, bool, u32)>,
}

impl Shared<'_> {
    fn pte(&self, address: u64) -> Option<u64> {
        let o = usize::try_from(address.checked_sub(RAM_BASE)?).ok()?;
        (o % 8 == 0).then_some(())?;
        Some(self.ram.get(o / 8)?.load(Relaxed))
    }

    /// The RAM offset of the page of `va` (all of it in RAM), if it may be read (written).
    #[inline(always)]
    fn page(&self, va: u64, write: bool) -> Option<usize> {
        let (page, at, w) = self.last.get();
        if page == va >> 12 && (w || !write) {
            return Some(at);
        }
        if va >= 1 << 49 {
            return None;
        }
        let slot = (va >> 12) as usize % SPLIT_TLB;
        let (tag, entry) = self.tlb.borrow()[slot];
        let (offset, writable) = if tag == (va >> 12) << 1 | 1 {
            ((entry & !1) as usize, entry & 1 != 0)
        } else {
            let physical = walk(self.root, va, false, |a| self.pte(a))?;
            let writable = walk(self.root, va, true, |a| self.pte(a)).is_some();
            let offset = usize::try_from((physical & !0xfff).checked_sub(RAM_BASE)?).ok()?;
            if offset + 0x1000 > self.ram.len() * 8 {
                return None;
            }
            self.tlb.borrow_mut()[slot] = ((va >> 12) << 1 | 1, offset as u64 | writable as u64);
            (offset, writable)
        };
        if write && !writable {
            return None;
        }
        self.last.set((va >> 12, offset, writable));
        Some(offset)
    }

    /// The run of consecutive elements a warp's load or store is, if it is one: the first and
    /// last lane, and the RAM offset of the first's bytes (all in one page).
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

    #[inline]
    fn read_run(&self, at: &[u64], mask: u32, n: usize, out: &mut [[u8; 16]]) -> bool {
        let Some((first, last, from)) = self.run(at, mask, n, false) else { return false };
        let skip = from % 8;
        let words = (skip + (last - first + 1) * n).div_ceil(8);
        let Some(src) = self.ram.get(from / 8..from / 8 + words) else { return false };
        if last >= out.len() {
            return false;
        }
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

    fn note(&self, va: u64, write: bool) {
        let _ = self.fault.set((va, write, maxwell::compute::last_memory_pc()));
    }
}

impl maxwell::Memory for Shared<'_> {
    /// One compare and swap on the word that holds the bytes (an access within an aligned
    /// 8-byte word, as the GPU's atomics are).
    fn update(&mut self, addr: u64, n: usize, f: &mut dyn FnMut(u64) -> u64) -> u64 {
        let o = (addr & 0xfff) as usize;
        let within = (addr % 8) as usize + n <= 8;
        let Some(page) = self.page(addr, true).filter(|_| within && n > 0) else {
            // (straddling a word: not atomic; unmapped: a fault)
            let mut buf = [0; 8];
            self.read(addr, &mut buf[..n]);
            let old = u64::from_le_bytes(buf);
            self.write(addr, &f(old).to_le_bytes()[..n]);
            return old;
        };
        let at = page + o;
        let (sh, mask) = (8 * (at % 8) as u32, u64::MAX >> (64 - 8 * n as u32));
        let word = &self.ram[at / 8];
        let prev = word.fetch_update(Relaxed, Relaxed, |x| Some(x & !(mask << sh) | (f(x >> sh & mask) & mask) << sh));
        let x = prev.unwrap_or_else(|x| x);
        x >> sh & mask
    }
    fn read_lanes(&self, at: &[u64], mask: u32, n: usize, out: &mut [[u8; 16]]) {
        if self.read_run(at, mask, n, out) {
            return;
        }
        let mut m = mask;
        while m != 0 {
            let i = m.trailing_zeros() as usize;
            m &= m - 1;
            let va = at[i];
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
    fn read(&self, mut va: u64, mut out: &mut [u8]) {
        while !out.is_empty() {
            let n = (0x1000 - (va & 0xfff) as usize).min(out.len());
            let (part, rest) = out.split_at_mut(n);
            if !self.page(va, false).is_some_and(|at| load(self.ram, at + (va & 0xfff) as usize, part)) {
                part.fill(0);
                self.note(va, false);
            }
            out = rest;
            va = va.wrapping_add(n as u64);
        }
    }
    fn write(&mut self, mut va: u64, mut data: &[u8]) {
        while !data.is_empty() {
            let n = (0x1000 - (va & 0xfff) as usize).min(data.len());
            if !self.page(va, true).is_some_and(|at| store(self.ram, at + (va & 0xfff) as usize, &data[..n])) {
                self.note(va, true);
            }
            data = &data[n..];
            va = va.wrapping_add(n as u64);
        }
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
        let mut view = View::new(&mut ram, RAM_BASE);
        view.write(0xfffe, &[1, 2, 3, 4]).unwrap();
        assert_eq!(&view.ram[0x2fffe..0x30000], &[1, 2]);
        assert_eq!(&view.ram[0x10000..0x10002], &[3, 4]);
        view.ram[0x4108..0x4110].copy_from_slice(&encode(RAM_BASE + 0x10000, 0x45));
        // (a new view: one caches translations while the page tables stay as they are)
        let view = View::new(view.ram, RAM_BASE);
        assert_eq!(view.translate(0x10000, false), Some(RAM_BASE + 0x10000));
        assert_eq!(view.translate(0x10000, true), None);
        // a read-only page found by a read is still refused to a write
        assert_eq!(view.translate(0x10000, false), Some(RAM_BASE + 0x10000));
        assert_eq!(view.translate(0x10000, true), None);
    }

    #[test]
    fn split_views_share_memory() {
        use maxwell::Memory;
        // 8-byte aligned RAM, as the machine's
        let mut words = vec![0u64; 0x40000 / 8];
        let ram: &mut [u8] = unsafe { std::slice::from_raw_parts_mut(words.as_mut_ptr().cast::<u8>(), 0x40000) };
        let encode = |physical: u64, flags: u64| ((physical >> 4) | flags).to_le_bytes();
        ram[0..8].copy_from_slice(&encode(RAM_BASE + 0x1000, 4));
        ram[0x1000..0x1008].copy_from_slice(&encode(RAM_BASE + 0x2000, 4));
        ram[0x2000..0x2008].copy_from_slice(&encode(RAM_BASE + 0x3000, 4));
        ram[0x3000..0x3008].copy_from_slice(&encode(RAM_BASE + 0x4100, 4));
        // 64 KiB page 0 at 0x20000, page 1 (0x10000) read-only at 0x10000
        ram[0x4100..0x4108].copy_from_slice(&encode(RAM_BASE + 0x20000, 5));
        ram[0x4108..0x4110].copy_from_slice(&encode(RAM_BASE + 0x10000, 0x45));
        let mut view = View::new(ram, RAM_BASE);
        let mut guest = Guest::new(&mut view);
        assert!(guest.split(2, &mut |mut views| {
            views[0].write(0xfffc, &[1, 2, 3, 4]);
            let mut out = [0u8; 4];
            views[1].read(0xfffc, &mut out);
            assert_eq!(out, [1, 2, 3, 4]);
            // contiguous lanes, then a write to the read-only page
            let at: Vec<u64> = (0..4).map(|i| 0x100 + 4 * i).collect();
            let data: Vec<[u8; 16]> = (0..4).map(|i| [i as u8 + 10; 16]).collect();
            views[1].write_lanes(&at, 0xf, 4, &data);
            let mut back = vec![[0u8; 16]; 4];
            views[0].read_lanes(&at, 0xf, 4, &mut back);
            assert_eq!(back.iter().map(|b| b[0]).collect::<Vec<_>>(), [10, 11, 12, 13]);
            views[0].write(0x10000, &[9]);
        }));
        assert_eq!(guest.first_fault.get().map(|f| (f.0, f.1)), Some((0x10000, true)));
        drop(guest);
        assert_eq!(&view.ram[0x2fffc..0x30000], &[1, 2, 3, 4]);
        assert_eq!(view.ram[0x10000], 0);
    }

    #[test]
    fn split_views_update_atomically() {
        use maxwell::Memory;
        let mut words = vec![0u64; 0x40000 / 8];
        let ram: &mut [u8] = unsafe { std::slice::from_raw_parts_mut(words.as_mut_ptr().cast::<u8>(), 0x40000) };
        let encode = |physical: u64, flags: u64| ((physical >> 4) | flags).to_le_bytes();
        ram[0..8].copy_from_slice(&encode(RAM_BASE + 0x1000, 4));
        ram[0x1000..0x1008].copy_from_slice(&encode(RAM_BASE + 0x2000, 4));
        ram[0x2000..0x2008].copy_from_slice(&encode(RAM_BASE + 0x3000, 4));
        ram[0x3000..0x3008].copy_from_slice(&encode(RAM_BASE + 0x4100, 4));
        ram[0x4100..0x4108].copy_from_slice(&encode(RAM_BASE + 0x20000, 5));
        let mut view = View::new(ram, RAM_BASE);
        let mut guest = Guest::new(&mut view);
        assert!(guest.split_atomics());
        assert!(guest.split(8, &mut |views| {
            std::thread::scope(|scope| {
                for mut v in views {
                    scope.spawn(move || {
                        for _ in 0..10_000 {
                            // a 32-bit counter in the upper half of a word, beside another
                            v.update(0x104, 4, &mut |x| x + 1);
                            v.update(0x100, 4, &mut |x| x + 2);
                        }
                    });
                }
            });
        }));
        drop(guest);
        assert_eq!(&view.ram[0x20100..0x20108], &[0x00, 0x71, 0x02, 0x00, 0x80, 0x38, 0x01, 0x00]);
    }
}
