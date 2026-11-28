//! GPU memory: the VideoCore IV has no MMU. Linux hands it bus addresses of contiguous (CMA)
//! buffers, which on the BCM2837 alias RAM at 0xc000_0000 (the L2-uncached view); the top two
//! bits select the cache alias and are ignored here. Reads outside RAM return zero, writes there
//! are dropped (logged once).
//!
//! With capture on (`SBC_VC4_CAPTURE`), every 4 KiB page a job touches is recorded as it was
//! before the job first touched it, for `vc4-replay`.
//!
//! Guest RAM is held as a raw pointer, not a slice: render workers on several host threads
//! (`par`) each have their own `Mem` over the same RAM, as the hardware's QPUs and TMUs share
//! the bus. They write disjoint tiles of the frame; where shaders write the same memory from
//! several tiles (VPM DMA stores) the order is unspecified, as on the hardware.

use std::collections::BTreeMap;
use std::marker::PhantomData;

pub const PAGE: usize = 4096;

/// Pages a job touched, as they were before it touched them (bus address of the page → data).
pub type Pages = BTreeMap<u32, Vec<u8>>;

pub struct Mem<'a> {
    ram: *mut u8,
    len: usize,
    _ram: PhantomData<&'a mut [u8]>,
    pub capture: Option<Pages>,
    /// pages written (replay: these keep their replayed contents)
    pub written: Option<std::collections::BTreeSet<u32>>,
    /// accesses outside RAM in this job
    pub faults: u32,
    /// the last page put into `written` (+1; 0: none)
    last_written: usize,
}

/// The RAM offset of bus address `a`.
#[inline(always)]
pub fn pa(a: u32) -> usize { (a & 0x3fff_ffff) as usize }

// SAFETY: a Mem is a view of guest RAM that the machine keeps alive for the whole job; the
// workers that share it are joined before the job returns.
unsafe impl Send for Mem<'_> {}

impl<'a> Mem<'a> {
    pub fn new(ram: &'a mut [u8]) -> Self { Mem { ram: ram.as_mut_ptr(), len: ram.len(), _ram: PhantomData, capture: None, written: None, faults: 0, last_written: 0 } }

    /// Another view of the same memory, for a worker on another host thread: its own fault
    /// count and written-page set (merged back with [`Mem::merge`]); no capture (jobs being
    /// captured run on one thread).
    pub fn share(&self) -> Mem<'a> {
        Mem { ram: self.ram, len: self.len, _ram: PhantomData, capture: None, written: self.written.as_ref().map(|_| Default::default()), faults: 0, last_written: 0 }
    }

    /// Take back what a worker's view recorded.
    pub fn merge(&mut self, w: Mem) {
        self.faults += w.faults;
        if let (Some(a), Some(b)) = (self.written.as_mut(), w.written) { a.extend(b); }
    }

    pub fn len(&self) -> usize { self.len }
    pub fn is_empty(&self) -> bool { self.len == 0 }

    #[inline(always)]
    fn bytes(&self, p: usize, n: usize) -> &[u8] {
        debug_assert!(p + n <= self.len);
        // SAFETY: callers check p + n against len
        unsafe { std::slice::from_raw_parts(self.ram.add(p), n) }
    }

    #[inline(always)]
    #[allow(clippy::mut_from_ref)]
    fn bytes_mut(&mut self, p: usize, n: usize) -> &mut [u8] {
        debug_assert!(p + n <= self.len);
        // SAFETY: as in `bytes`
        unsafe { std::slice::from_raw_parts_mut(self.ram.add(p), n) }
    }

    #[cold]
    fn fault(&mut self, a: u32, write: bool) {
        self.faults += 1;
        if self.faults > 4 { return; }
        crate::log_first(&format!("vc4: {} outside RAM at {a:#010x}", if write { "write" } else { "read" }));
    }

    #[inline(always)]
    fn touch(&mut self, p: usize, n: usize) {
        if self.capture.is_some() { self.record(p, n); }
    }

    #[cold]
    fn record(&mut self, p: usize, n: usize) {
        if let Some(c) = self.capture.as_mut() {
            let (first, last) = (p / PAGE, (p + n.max(1) - 1) / PAGE);
            for pg in first..=last {
                let base = pg * PAGE;
                if base + PAGE > self.len { continue; }
                // SAFETY: inside RAM (checked above)
                c.entry(base as u32).or_insert_with(|| unsafe { std::slice::from_raw_parts(self.ram.add(base), PAGE) }.to_vec());
            }
        }
    }

    /// `n` bytes at `a`, or None if they are not all in RAM.
    #[inline]
    pub fn slice(&mut self, a: u32, n: usize) -> Option<&[u8]> {
        let p = pa(a);
        if p + n > self.len { self.fault(a, false); return None; }
        self.touch(p, n);
        Some(self.bytes(p, n))
    }

    #[inline]
    pub fn slice_mut(&mut self, a: u32, n: usize) -> Option<&mut [u8]> {
        let p = pa(a);
        if p + n > self.len { self.fault(a, true); return None; }
        self.touch(p, n);
        if let Some(w) = self.written.as_mut() {
            let (first, last) = (p / PAGE, (p + n.max(1) - 1) / PAGE);
            if first + 1 != self.last_written || last != first {
                for pg in first..=last { w.insert((pg * PAGE) as u32); }
                self.last_written = last + 1;
            }
        }
        Some(self.bytes_mut(p, n))
    }

    #[inline]
    pub fn r8(&mut self, a: u32) -> u8 { self.slice(a, 1).map_or(0, |s| s[0]) }
    #[inline]
    pub fn r16(&mut self, a: u32) -> u16 { self.slice(a, 2).map_or(0, |s| u16::from_le_bytes([s[0], s[1]])) }
    #[inline]
    pub fn r32(&mut self, a: u32) -> u32 { self.slice(a, 4).map_or(0, |s| u32::from_le_bytes([s[0], s[1], s[2], s[3]])) }
    #[inline]
    pub fn rf(&mut self, a: u32) -> f32 { f32::from_bits(self.r32(a)) }
    #[inline]
    pub fn w8(&mut self, a: u32, v: u8) { if let Some(s) = self.slice_mut(a, 1) { s[0] = v; } }
    #[inline]
    pub fn w16(&mut self, a: u32, v: u16) { if let Some(s) = self.slice_mut(a, 2) { s.copy_from_slice(&v.to_le_bytes()); } }
    #[inline]
    pub fn w32(&mut self, a: u32, v: u32) { if let Some(s) = self.slice_mut(a, 4) { s.copy_from_slice(&v.to_le_bytes()); } }
    pub fn read(&mut self, a: u32, out: &mut [u8]) {
        match self.slice(a, out.len()) { Some(s) => out.copy_from_slice(s), None => out.fill(0) }
    }
    pub fn write(&mut self, a: u32, data: &[u8]) {
        if let Some(s) = self.slice_mut(a, data.len()) { s.copy_from_slice(data); }
    }
}
