//! GPU memory: virtual addresses through the V3D MMU's single-level page table
//! (one 32-bit entry per 4 KiB page: valid bit 28, writeable bit 29, page frame in bits 0-23)
//! to bus addresses, which on the BCM2711's V3D bus are the physical RAM addresses.

const PTE_VALID: u32 = 1 << 28;

pub struct Gmem<'a> {
    pub ram: &'a mut [u8],
    pt: u64,
    mmu: bool,
    /// one-entry translation cache: (virtual page, physical page base)
    last: (u32, usize),
    pub faults: u32,
}

impl<'a> Gmem<'a> {
    pub fn new(ram: &'a mut [u8], pt: u64, mmu: bool) -> Self { Gmem { ram, pt, mmu, last: (u32::MAX, 0), faults: 0 } }

    /// Physical byte offset of `va`, if mapped.
    #[inline]
    pub fn tr(&mut self, va: u32) -> Option<usize> {
        let page = va >> 12;
        if page == self.last.0 { return Some(self.last.1 + (va & 0xfff) as usize); }
        let pa = if !self.mmu { (page as usize) << 12 } else {
            let e = self.pt as usize + page as usize * 4;
            let pte = u32::from_le_bytes(self.ram.get(e..e + 4)?.try_into().ok()?);
            if pte & PTE_VALID == 0 {
                self.faults += 1;
                if self.faults <= 8 { crate::log(&format!("v3d: MMU fault at {va:#010x} (pte {pte:#x})")); }
                return None;
            }
            ((pte & 0x00ff_ffff) as usize) << 12
        };
        if pa + 4096 > self.ram.len() { self.faults += 1; return None; }
        self.last = (page, pa);
        Some(pa + (va & 0xfff) as usize)
    }

    #[inline]
    pub fn r32(&mut self, va: u32) -> u32 {
        if va & 0xfff <= 0xffc {
            match self.tr(va) { Some(p) => u32::from_le_bytes(self.ram[p..p + 4].try_into().unwrap()), None => 0 }
        } else {
            let mut b = [0u8; 4];
            self.read(va, &mut b);
            u32::from_le_bytes(b)
        }
    }
    #[inline]
    pub fn w32(&mut self, va: u32, v: u32) {
        if va & 0xfff <= 0xffc {
            if let Some(p) = self.tr(va) { self.ram[p..p + 4].copy_from_slice(&v.to_le_bytes()); }
        } else { self.write(va, &v.to_le_bytes()); }
    }
    #[inline]
    pub fn r8(&mut self, va: u32) -> u8 { self.tr(va).map_or(0, |p| self.ram[p]) }
    #[inline]
    pub fn w8(&mut self, va: u32, v: u8) { if let Some(p) = self.tr(va) { self.ram[p] = v; } }
    pub fn r16(&mut self, va: u32) -> u16 { self.r8(va) as u16 | (self.r8(va.wrapping_add(1)) as u16) << 8 }
    pub fn w16(&mut self, va: u32, v: u16) { self.w8(va, v as u8); self.w8(va.wrapping_add(1), (v >> 8) as u8); }

    /// Copy out `out.len()` bytes from `va` (page by page).
    pub fn read(&mut self, va: u32, out: &mut [u8]) {
        let mut done = 0;
        while done < out.len() {
            let a = va.wrapping_add(done as u32);
            let n = (4096 - (a & 0xfff) as usize).min(out.len() - done);
            match self.tr(a) {
                Some(p) => out[done..done + n].copy_from_slice(&self.ram[p..p + n]),
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
            if let Some(p) = self.tr(a) { self.ram[p..p + n].copy_from_slice(&data[done..done + n]); }
            done += n;
        }
    }
    pub fn bytes(&mut self, va: u32, n: usize) -> Vec<u8> { let mut v = vec![0; n]; self.read(va, &mut v); v }
}
