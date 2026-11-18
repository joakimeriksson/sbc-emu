//! GPU memory: virtual addresses through the V3D MMU's single-level page table
//! (one 32-bit entry per 4 KiB page: valid bit 28, writeable bit 29, page frame in bits 0-23)
//! to bus addresses, which on the BCM2711's V3D bus are the physical RAM addresses.
//!
//! Guest RAM is held as a raw pointer, not a slice: render workers on several host threads
//! each have their own `Gmem` (own translation cache) over the same RAM, as the hardware's
//! QPUs and TMUs share the bus. They write disjoint tiles; where guest shaders write the same
//! memory from several tiles (stores, atomics) the order is unspecified, as on the hardware,
//! and atomics are made atomic in `tmu`.

use std::marker::PhantomData;

const PTE_VALID: u32 = 1 << 28;

pub struct Gmem<'a> {
    ram: *mut u8,
    len: usize,
    pt: u64,
    mmu: bool,
    /// one-entry translation cache: (virtual page, physical page base)
    last: (u32, usize),
    pub faults: u32,
    _ram: PhantomData<&'a mut [u8]>,
}

// SAFETY: a Gmem is a view of guest RAM that the machine keeps alive for the whole job; the
// render workers that share it are joined before the job returns.
unsafe impl Send for Gmem<'_> {}

impl<'a> Gmem<'a> {
    pub fn new(ram: &'a mut [u8], pt: u64, mmu: bool) -> Self {
        Gmem { ram: ram.as_mut_ptr(), len: ram.len(), pt, mmu, last: (u32::MAX, 0), faults: 0, _ram: PhantomData }
    }

    /// Another view of the same memory, for a render worker (own translation cache and fault
    /// count).
    pub fn share(&self) -> Gmem<'a> {
        Gmem { ram: self.ram, len: self.len, pt: self.pt, mmu: self.mmu, last: (u32::MAX, 0), faults: 0, _ram: PhantomData }
    }

    #[inline]
    fn get(&self, p: usize, out: &mut [u8]) {
        debug_assert!(p + out.len() <= self.len);
        // SAFETY: callers check p + n against len (via `tr`'s page bound)
        unsafe { std::ptr::copy_nonoverlapping(self.ram.add(p), out.as_mut_ptr(), out.len()) }
    }

    #[inline]
    fn put(&mut self, p: usize, data: &[u8]) {
        debug_assert!(p + data.len() <= self.len);
        // SAFETY: as in `get`
        unsafe { std::ptr::copy_nonoverlapping(data.as_ptr(), self.ram.add(p), data.len()) }
    }

    /// Physical byte offset of `va`, if mapped.
    #[inline]
    pub fn tr(&mut self, va: u32) -> Option<usize> {
        let page = va >> 12;
        if page == self.last.0 { return Some(self.last.1 + (va & 0xfff) as usize); }
        let pa = if !self.mmu { (page as usize) << 12 } else {
            let e = self.pt as usize + page as usize * 4;
            if e + 4 > self.len { self.faults += 1; return None; }
            let mut b = [0u8; 4];
            self.get(e, &mut b);
            let pte = u32::from_le_bytes(b);
            if pte & PTE_VALID == 0 {
                self.faults += 1;
                // always reported (each distinct one once): a read of an unmapped page returns zeros, which
                // shows up much later as a wrong uniform, texture state or vertex
                if self.faults <= 8 { crate::log_once(&format!("v3d: MMU fault at {va:#010x} (pte {pte:#x})")); }
                return None;
            }
            ((pte & 0x00ff_ffff) as usize) << 12
        };
        if pa + 4096 > self.len { self.faults += 1; return None; }
        self.last = (page, pa);
        Some(pa + (va & 0xfff) as usize)
    }

    #[inline]
    pub fn r32(&mut self, va: u32) -> u32 {
        let mut b = [0u8; 4];
        if va & 0xfff <= 0xffc {
            if let Some(p) = self.tr(va) { self.get(p, &mut b); }
        } else {
            self.read(va, &mut b);
        }
        u32::from_le_bytes(b)
    }
    #[inline]
    pub fn w32(&mut self, va: u32, v: u32) {
        if va & 0xfff <= 0xffc {
            if let Some(p) = self.tr(va) { self.put(p, &v.to_le_bytes()); }
        } else { self.write(va, &v.to_le_bytes()); }
    }
    #[inline]
    pub fn r8(&mut self, va: u32) -> u8 { let mut b = [0u8]; if let Some(p) = self.tr(va) { self.get(p, &mut b); } b[0] }
    #[inline]
    pub fn w8(&mut self, va: u32, v: u8) { if let Some(p) = self.tr(va) { self.put(p, &[v]); } }
    pub fn r16(&mut self, va: u32) -> u16 { self.r8(va) as u16 | (self.r8(va.wrapping_add(1)) as u16) << 8 }
    pub fn w16(&mut self, va: u32, v: u16) { self.w8(va, v as u8); self.w8(va.wrapping_add(1), (v >> 8) as u8); }

    /// Copy out `out.len()` bytes from `va` (page by page).
    pub fn read(&mut self, va: u32, out: &mut [u8]) {
        let mut done = 0;
        while done < out.len() {
            let a = va.wrapping_add(done as u32);
            let n = (4096 - (a & 0xfff) as usize).min(out.len() - done);
            match self.tr(a) {
                Some(p) => self.get(p, &mut out[done..done + n]),
                None => out[done..done + n].fill(0),
            }
            done += n;
        }
    }
    pub fn write(&mut self, va: u32, data: &[u8]) {
        let mut done = 0;
        while done < data.len() {
            let a = va.wrapping_add(done as u32);
            let n = (4096 - (a & 0xfff) as usize).min(data.len() - done);
            if let Some(p) = self.tr(a) { self.put(p, &data[done..done + n]); }
            done += n;
        }
    }
    pub fn bytes(&mut self, va: u32, n: usize) -> Vec<u8> { let mut v = vec![0; n]; self.read(va, &mut v); v }
}
