//! A GPU virtual address space: the Rogue MMU's three-level tables (`pvr_mmu.c`), rooted at a
//! page catalogue's physical address. 4 KiB pages.
//!
//! - page catalogue: 1024 x u32, index va[39:30]; valid bit 0, page directory at bits 31:4 << 8
//! - page directory: 512 x u64, index va[29:21]; valid bit 0, page table at bits 39:12
//! - page table: 512 x u64, index va[20:12]; valid bit 0, page at bits 39:12
use emu_core::FlatRam;

pub struct Vm {
    pc: u64,
    /// the last translated page (va page, pa page)
    last: Option<(u64, u64)>,
}

impl Vm {
    pub fn new(pc: u64) -> Self {
        Vm { pc: pc & 0xff_ffff_f000, last: None }
    }

    pub fn translate(&mut self, ram: &FlatRam, va: u64) -> Option<u64> {
        let page = va & !0xfff;
        if let Some((v, p)) = self.last {
            if v == page {
                return Some(p | (va & 0xfff));
            }
        }
        let pce = rd32(ram, self.pc + 4 * ((va >> 30) & 0x3ff))?;
        if pce & 1 == 0 {
            return None;
        }
        let pd = ((pce as u64) >> 4) << 12;
        let pde = rd64(ram, pd + 8 * ((va >> 21) & 0x1ff))?;
        if pde & 1 == 0 {
            return None;
        }
        let pte = rd64(ram, (pde & 0xff_ffff_f000) + 8 * ((va >> 12) & 0x1ff))?;
        if pte & 1 == 0 {
            return None;
        }
        let pa = pte & 0xff_ffff_f000;
        self.last = Some((page, pa));
        Some(pa | (va & 0xfff))
    }

    pub fn read32(&mut self, ram: &FlatRam, va: u64) -> Option<u32> {
        let pa = self.translate(ram, va)?;
        rd32(ram, pa)
    }

    pub fn read64(&mut self, ram: &FlatRam, va: u64) -> Option<u64> {
        Some(self.read32(ram, va)? as u64 | (self.read32(ram, va + 4)? as u64) << 32)
    }

    pub fn write32(&mut self, ram: &mut FlatRam, va: u64, v: u32) -> bool {
        match self.translate(ram, va) {
            Some(pa) => ram.load(pa, &v.to_le_bytes()).is_ok(),
            None => false,
        }
    }
}

fn rd32(ram: &FlatRam, pa: u64) -> Option<u32> {
    let o = pa.checked_sub(ram.base)? as usize;
    ram.mem.get(o..o.checked_add(4)?).map(|b| u32::from_le_bytes(b.try_into().unwrap()))
}

fn rd64(ram: &FlatRam, pa: u64) -> Option<u64> {
    let o = pa.checked_sub(ram.base)? as usize;
    ram.mem.get(o..o.checked_add(8)?).map(|b| u64::from_le_bytes(b.try_into().unwrap()))
}
