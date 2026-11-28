//! GPU memory: the VideoCore IV has no MMU. Linux hands it bus addresses of contiguous (CMA)
//! buffers, which on the BCM2837 alias RAM at 0xc000_0000 (the L2-uncached view); the top two
//! bits select the cache alias and are ignored here. Reads outside RAM return zero, writes there
//! are dropped (logged once).
//!
//! With capture on (`SBC_VC4_CAPTURE`), every 4 KiB page a job touches is recorded as it was
//! before the job first touched it, for `vc4-replay`.

use std::collections::BTreeMap;

pub const PAGE: usize = 4096;

/// Pages a job touched, as they were before it touched them (bus address of the page → data).
pub type Pages = BTreeMap<u32, Vec<u8>>;

pub struct Mem<'a> {
    ram: &'a mut [u8],
    pub capture: Option<Pages>,
    /// pages written (replay: these keep their replayed contents)
    pub written: Option<std::collections::BTreeSet<u32>>,
    /// accesses outside RAM in this job
    pub faults: u32,
}

/// The RAM offset of bus address `a`.
#[inline(always)]
pub fn pa(a: u32) -> usize { (a & 0x3fff_ffff) as usize }

impl<'a> Mem<'a> {
    pub fn new(ram: &'a mut [u8]) -> Self { Mem { ram, capture: None, written: None, faults: 0 } }

    pub fn len(&self) -> usize { self.ram.len() }
    pub fn is_empty(&self) -> bool { self.ram.is_empty() }

    #[cold]
    fn fault(&mut self, a: u32, write: bool) {
        self.faults += 1;
        if self.faults > 4 { return; }
        crate::log_first(&format!("vc4: {} outside RAM at {a:#010x}", if write { "write" } else { "read" }));
    }

    #[inline]
    fn touch(&mut self, p: usize, n: usize) {
        if let Some(c) = self.capture.as_mut() {
            let (first, last) = (p / PAGE, (p + n.max(1) - 1) / PAGE);
            for pg in first..=last {
                let base = pg * PAGE;
                if base + PAGE > self.ram.len() { continue; }
                c.entry(base as u32).or_insert_with(|| self.ram[base..base + PAGE].to_vec());
            }
        }
    }

    /// `n` bytes at `a`, or None if they are not all in RAM.
    #[inline]
    pub fn slice(&mut self, a: u32, n: usize) -> Option<&[u8]> {
        let p = pa(a);
        if p + n > self.ram.len() { self.fault(a, false); return None; }
        self.touch(p, n);
        Some(&self.ram[p..p + n])
    }

    #[inline]
    pub fn slice_mut(&mut self, a: u32, n: usize) -> Option<&mut [u8]> {
        let p = pa(a);
        if p + n > self.ram.len() { self.fault(a, true); return None; }
        self.touch(p, n);
        if let Some(w) = self.written.as_mut() { for pg in p / PAGE..=(p + n.max(1) - 1) / PAGE { w.insert((pg * PAGE) as u32); } }
        Some(&mut self.ram[p..p + n])
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
