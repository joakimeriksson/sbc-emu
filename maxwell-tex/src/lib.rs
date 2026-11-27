//! Surfaces of NVIDIA Maxwell GPUs (GM20B, the Jetson Nano's) as Mesa's nouveau driver uses
//! them: memory layouts, texture and sampler descriptors, texture sampling, render target and
//! depth formats.
//!
//! * [`layout`]: block-linear and pitch addressing, mip chains, array layers, multisampling;
//! * [`desc`]: texture headers ([`Tic`]) and samplers ([`Tsc`]), 32 bytes each;
//! * [`texture`]: [`Texture`] and [`Sampler`], precomputed from the descriptors, and the
//!   [`TexCache`] their reads go through;
//! * [`sample`]: TEX/TXD/TLD4/TLD/TXQ/TMML and the surface (image) accesses of a quad;
//! * [`rt`]: colour and depth/stencil target formats;
//! * [`surface`]: render target surfaces and the [`Tile`]s a rasterizer works on.
//!
//! All memory is reached through [`Memory`], on GPU virtual addresses. Nothing here knows
//! about shaders, command buffers or the MMU.

#![forbid(unsafe_code)]

pub mod bc;
pub mod bptc;
pub mod desc;
pub mod etc;
pub mod layout;
pub mod num;
pub mod rt;
pub mod sample;
pub mod surface;
pub mod texture;

#[cfg(test)]
mod tests;

pub use desc::{Dim, Tic, Tsc};
pub use layout::{BlockDims, MsMode, Plane};
pub use rt::{Class, RtFormat, ZetaFormat};
pub use sample::{Args, Lod, LANES};
pub use surface::{Surface, Tile};
pub use texture::{Sampler, TexCache, TexError, Texture};

/// GPU virtual memory. Unmapped reads give zeros, unmapped writes are dropped.
pub trait Memory {
    fn read(&self, addr: u64, out: &mut [u8]);
    fn write(&mut self, addr: u64, data: &[u8]);

    /// `n` bytes (at most 16) at `at[i]` into `out[i]`, for each lane `i` of `mask` (a warp's
    /// load; what `out[i]` holds past `n` bytes is not defined). The default reads lane by lane.
    fn read_lanes(&self, at: &[u64], mask: u32, n: usize, out: &mut [[u8; 16]]) {
        let mut m = mask;
        while m != 0 {
            let i = m.trailing_zeros() as usize;
            m &= m - 1;
            self.read(at[i], &mut out[i][..n]);
        }
    }

    /// The same for a store, lane after lane.
    fn write_lanes(&mut self, at: &[u64], mask: u32, n: usize, data: &[[u8; 16]]) {
        let mut m = mask;
        while m != 0 {
            let i = m.trailing_zeros() as usize;
            m &= m - 1;
            self.write(at[i], &data[i][..n]);
        }
    }

    /// An atomic read-modify-write of `n` (at most 8) bytes at `addr`: `f` gets the old value
    /// (little-endian, the rest zero) and gives the new one; returns the old. `f` may be called
    /// more than once. The default reads, then writes: atomic for one thread only.
    fn update(&mut self, addr: u64, n: usize, f: &mut dyn FnMut(u64) -> u64) -> u64 {
        let mut buf = [0; 8];
        self.read(addr, &mut buf[..n]);
        let old = u64::from_le_bytes(buf);
        self.write(addr, &f(old).to_le_bytes()[..n]);
        old
    }

    /// Whether `update` stays atomic across the views of a `split` (then a launch with atomics
    /// may run its workgroups on several threads).
    fn split_atomics(&self) -> bool {
        false
    }

    /// The same memory for another thread, to read only (writes may be dropped). None: one thread.
    fn share(&self) -> Option<Box<dyn Memory + Send + '_>> {
        None
    }

    /// The same memory for `n` threads at once (a compute launch's workgroups): `run` gets a view
    /// for each, and each view sees what all of them wrote. An access within an aligned 8-byte
    /// word is one atomic access (it does not tear, as on the GPU; wider ones are several), and
    /// bytes several views write at once end up as one of theirs. The accesses are relaxed:
    /// MEMBAR orders them. false: one thread only.
    fn split(&mut self, _n: usize, _run: &mut dyn FnMut(Vec<Box<dyn Memory + Send + '_>>)) -> bool {
        false
    }
}
