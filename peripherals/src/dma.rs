//! Guest RAM as a bus-mastering device reaches it.

/// RAM and the physical address of its first byte.
pub struct Dma<'a> {
    pub ram: &'a mut [u8],
    pub base: u64,
}

impl Dma<'_> {
    pub fn get(&self, pa: u64, len: usize) -> Option<&[u8]> {
        self.ram.get(range(self.base, pa, len)?)
    }

    pub fn slice(&mut self, pa: u64, len: usize) -> Option<&mut [u8]> {
        let r = range(self.base, pa, len)?;
        self.ram.get_mut(r)
    }
}

fn range(base: u64, pa: u64, len: usize) -> Option<std::ops::Range<usize>> {
    let o = usize::try_from(pa.checked_sub(base)?).ok()?;
    Some(o..o.checked_add(len)?)
}

/// RAM that is only read.
pub struct Ram<'a> {
    pub ram: &'a [u8],
    pub base: u64,
}

pub trait Memory {
    fn bytes(&self, pa: u64, len: usize) -> Option<&[u8]>;
}

impl Memory for Dma<'_> {
    fn bytes(&self, pa: u64, len: usize) -> Option<&[u8]> {
        self.get(pa, len)
    }
}

impl Memory for Ram<'_> {
    fn bytes(&self, pa: u64, len: usize) -> Option<&[u8]> {
        self.ram.get(range(self.base, pa, len)?)
    }
}
