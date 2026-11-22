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
}
