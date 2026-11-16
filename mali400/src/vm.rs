//! GPU virtual memory: reads and writes through one of the GPU's MMUs.

use crate::mmu::Mmu;
use crate::Memory;

pub struct Vm<'a> {
    pub mem: &'a mut dyn Memory,
    pub mmu: &'a mut Mmu,
    /// the last translated page: (virtual page, physical page, writable checked)
    cache: Option<(u32, u32, bool)>,
    /// Accesses that hit a page fault (the job should report an error).
    pub faults: u32,
}

impl<'a> Vm<'a> {
    pub fn new(mem: &'a mut dyn Memory, mmu: &'a mut Mmu) -> Self {
        Self { mem, mmu, cache: None, faults: 0 }
    }

    fn translate(&mut self, va: u32, write: bool) -> Option<u32> {
        if let Some((vp, pp, w)) = self.cache {
            if vp == va >> 12 && (w || !write) {
                return Some(pp << 12 | va & 0xfff);
            }
        }
        match self.mmu.translate(&*self.mem, va, write) {
            Some(pa) => {
                self.cache = Some((va >> 12, pa >> 12, write));
                Some(pa)
            }
            None => {
                self.faults += 1;
                None
            }
        }
    }

    pub fn read_u32(&mut self, va: u32) -> u32 {
        if va & 3 != 0 {
            return self.read_u16(va) as u32 | (self.read_u16(va.wrapping_add(2)) as u32) << 16;
        }
        self.translate(va, false).and_then(|pa| self.mem.read_u32(pa)).unwrap_or(0)
    }

    pub fn read_u16(&mut self, va: u32) -> u16 {
        let w = self.read_u32(va & !3);
        (w >> ((va & 2) * 8)) as u16
    }

    pub fn read_u8(&mut self, va: u32) -> u8 {
        let w = self.read_u32(va & !3);
        (w >> ((va & 3) * 8)) as u8
    }

    pub fn read_f32(&mut self, va: u32) -> f32 {
        f32::from_bits(self.read_u32(va))
    }

    pub fn write_u32(&mut self, va: u32, value: u32) {
        if let Some(pa) = self.translate(va & !3, true) {
            self.mem.write_u32(pa, value);
        }
    }

    pub fn write_u16(&mut self, va: u32, value: u16) {
        let old = self.read_u32(va & !3);
        let shift = (va & 2) * 8;
        self.write_u32(va & !3, old & !(0xffff << shift) | (value as u32) << shift);
    }

    pub fn write_u8(&mut self, va: u32, value: u8) {
        let old = self.read_u32(va & !3);
        let shift = (va & 3) * 8;
        self.write_u32(va & !3, old & !(0xff << shift) | (value as u32) << shift);
    }
}
