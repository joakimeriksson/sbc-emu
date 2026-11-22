//! Guest RAM as a bus-mastering device reaches it.

/// RAM and the physical address of its first byte.
pub struct Dma<'a> {
    pub ram: &'a mut [u8],
    pub base: u64,
}

impl Dma<'_> {
    fn range(&self, pa: u64, len: usize) -> Option<std::ops::Range<usize>> {
        let o = usize::try_from(pa.checked_sub(self.base)?).ok()?;
        Some(o..o.checked_add(len)?)
    }

    pub fn get(&self, pa: u64, len: usize) -> Option<&[u8]> {
        self.ram.get(self.range(pa, len)?)
    }

    pub fn slice(&mut self, pa: u64, len: usize) -> Option<&mut [u8]> {
        let r = self.range(pa, len)?;
        self.ram.get_mut(r)
    }
}
