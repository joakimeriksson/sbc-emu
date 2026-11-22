//! NVIDIA Maxwell's graphics engine (GM20B) as Mesa's nouveau driver programs it: the 3D class
//! (B197), compute (B1C0), inline-to-memory (A140) and 2D (902D). Methods come in from the host's
//! push buffers; everything runs at once, in order.
pub mod inline;
pub mod mme;
pub mod pipe;
pub mod raster;
pub mod rop;
pub mod threed;
pub mod vertex;

pub use maxwell_tex::Memory;

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

/// One channel's engine objects.
#[derive(Default)]
pub struct Engine {
    pub threed: threed::Threed,
    inline: inline::Inline,
    /// (class, method) pairs without an effect, for reports
    pub unknown: std::collections::BTreeSet<(u32, u32)>,
}

impl Engine {
    /// A new submission: memory may have changed.
    pub fn begin(&mut self) {
        self.threed.pipe.invalidate();
    }

    /// The end of one: what is cached goes to memory.
    pub fn end(&mut self, mem: &mut dyn Memory) {
        self.threed.rop.flush(mem);
    }

    pub fn method(&mut self, class: u32, m: u32, v: u32, mem: &mut dyn Memory, now: u64) {
        match class {
            0xb197 => self.threed.method(m, v, mem, now),
            0xa140 => {
                if inline::Inline::owns(m) {
                    if m == 0x1b0 {
                        self.threed.rop.flush(mem);
                    }
                    self.inline.method(m, v, mem);
                }
            }
            _ => {
                self.unknown.insert((class, m));
            }
        }
    }

    /// A push buffer header's methods have all come.
    pub fn end_group(&mut self, class: u32, mem: &mut dyn Memory, now: u64) {
        if class == 0xb197 {
            self.threed.end_group(mem, now);
        }
    }
}
