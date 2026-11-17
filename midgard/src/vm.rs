//! GPU virtual memory as a job sees it: reads and writes through one address space.
//!
//! A failed translation is recorded (the first one wins) and reads return zero; the job
//! manager checks `fault` after each job and reports it as an MMU fault.

use crate::mmu::{AddressSpace, Fault};

const PAGE: u64 = 0x1000;
const TLB: usize = 64;

pub struct Vm<'a> {
    ram: &'a mut [u8],
    base: u64,
    aspace: AddressSpace,
    /// Direct-mapped translation cache: (virtual page | 1 if writable entry checked, index of the page in `ram`).
    tlb: [(u64, usize); TLB],
    pub fault: Option<Fault>,
    /// Capture for replay: the content of every page on first touch (before any write).
    pub capture: Option<std::collections::BTreeMap<u64, Vec<u8>>>,
}

impl<'a> Vm<'a> {
    pub fn new(ram: &'a mut [u8], base: u64, aspace: AddressSpace) -> Self {
        Self { ram, base, aspace, tlb: [(u64::MAX, 0); TLB], fault: None, capture: None }
    }

    /// Index into `ram` of the page holding `va`, or None (fault recorded).
    #[inline]
    fn page(&mut self, va: u64, write: bool) -> Option<usize> {
        let vpage = va & !(PAGE - 1);
        let key = vpage | write as u64;
        let slot = (va / PAGE) as usize % TLB;
        let (k, index) = self.tlb[slot];
        if k == key {
            return Some(index);
        }
        match self.aspace.translate(self.ram, self.base, vpage, write) {
            Ok(pa) => {
                let index = usize::try_from(pa.checked_sub(self.base)?).ok()?;
                if index.checked_add(PAGE as usize)? > self.ram.len() {
                    self.record(Fault { va, status: crate::mmu::TRANSLATION_FAULT | 3 });
                    return None;
                }
                self.tlb[slot] = (key, index);
                if let Some(c) = &mut self.capture {
                    c.entry(vpage).or_insert_with(|| self.ram[index..index + PAGE as usize].to_vec());
                }
                Some(index)
            }
            Err(f) => {
                self.record(Fault { va, ..f });
                None
            }
        }
    }

    fn record(&mut self, f: Fault) {
        if self.fault.is_none() {
            self.fault = Some(f);
        }
    }

    pub fn read_bytes(&mut self, va: u64, out: &mut [u8]) {
        let mut done = 0;
        while done < out.len() {
            let a = va + done as u64;
            let off = (a & (PAGE - 1)) as usize;
            let n = (PAGE as usize - off).min(out.len() - done);
            match self.page(a, false) {
                Some(p) => out[done..done + n].copy_from_slice(&self.ram[p + off..p + off + n]),
                None => out[done..done + n].fill(0),
            }
            done += n;
        }
    }

    pub fn write_bytes(&mut self, va: u64, data: &[u8]) {
        let mut done = 0;
        while done < data.len() {
            let a = va + done as u64;
            let off = (a & (PAGE - 1)) as usize;
            let n = (PAGE as usize - off).min(data.len() - done);
            if let Some(p) = self.page(a, true) {
                self.ram[p + off..p + off + n].copy_from_slice(&data[done..done + n]);
            }
            done += n;
        }
    }

    #[inline]
    pub fn r8(&mut self, va: u64) -> u8 {
        self.page(va, false).map_or(0, |p| self.ram[p + (va & (PAGE - 1)) as usize])
    }

    #[inline]
    pub fn r16(&mut self, va: u64) -> u16 {
        let mut b = [0; 2];
        self.read_bytes(va, &mut b);
        u16::from_le_bytes(b)
    }

    #[inline]
    pub fn r32(&mut self, va: u64) -> u32 {
        let off = (va & (PAGE - 1)) as usize;
        if off <= PAGE as usize - 4 {
            return self.page(va, false).map_or(0, |p| {
                u32::from_le_bytes(self.ram[p + off..p + off + 4].try_into().unwrap())
            });
        }
        let mut b = [0; 4];
        self.read_bytes(va, &mut b);
        u32::from_le_bytes(b)
    }

    #[inline]
    pub fn r64(&mut self, va: u64) -> u64 {
        self.r32(va) as u64 | (self.r32(va + 4) as u64) << 32
    }

    pub fn rf32(&mut self, va: u64) -> f32 {
        f32::from_bits(self.r32(va))
    }

    /// `n` consecutive words.
    pub fn words<const N: usize>(&mut self, va: u64) -> [u32; N] {
        let mut w = [0u32; N];
        for (i, x) in w.iter_mut().enumerate() {
            *x = self.r32(va + 4 * i as u64);
        }
        w
    }

    #[inline]
    pub fn w8(&mut self, va: u64, v: u8) {
        if let Some(p) = self.page(va, true) {
            self.ram[p + (va & (PAGE - 1)) as usize] = v;
        }
    }

    pub fn w16(&mut self, va: u64, v: u16) {
        self.write_bytes(va, &v.to_le_bytes());
    }

    #[inline]
    pub fn w32(&mut self, va: u64, v: u32) {
        let off = (va & (PAGE - 1)) as usize;
        if off <= PAGE as usize - 4 {
            if let Some(p) = self.page(va, true) {
                self.ram[p + off..p + off + 4].copy_from_slice(&v.to_le_bytes());
            }
            return;
        }
        self.write_bytes(va, &v.to_le_bytes());
    }

    pub fn w64(&mut self, va: u64, v: u64) {
        self.w32(va, v as u32);
        self.w32(va + 4, (v >> 32) as u32);
    }
}
