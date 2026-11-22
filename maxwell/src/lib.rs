//! NVIDIA Maxwell's graphics engine (GM20B) as Mesa's nouveau driver programs it: the 3D class
//! (B197), compute (B1C0), inline-to-memory (A140) and 2D (902D). Methods come in from the host's
//! push buffers; everything runs at once, in order.
pub mod inline;
pub mod mme;
pub mod vertex;

/// Memory at GPU virtual addresses.
pub trait Memory {
    fn read(&self, va: u64, out: &mut [u8]);
    fn write(&mut self, va: u64, data: &[u8]);
}

pub trait MemoryExt: Memory {
    fn read32(&self, va: u64) -> u32 {
        let mut b = [0; 4];
        self.read(va, &mut b);
        u32::from_le_bytes(b)
    }
    fn write32(&mut self, va: u64, v: u32) {
        self.write(va, &v.to_le_bytes());
    }
}

impl<M: Memory + ?Sized> MemoryExt for M {}
