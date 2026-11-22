//! Tests. `Ram` is GPU memory; the encoders build descriptors the way the nouveau driver's
//! `gm107_create_texture_view` and `nv50_sampler_state_create` do.

mod blocks;
mod desc;
mod formats;
mod layout;
mod sampling;
mod speed;
mod targets;

use crate::layout::{BlockDims, Chain, MsMode, Shape};
use crate::Memory;

pub struct Ram {
    pub base: u64,
    pub data: Vec<u8>,
}

impl Ram {
    pub fn new(base: u64, len: usize) -> Self {
        Self { base, data: vec![0; len] }
    }
}

impl Memory for Ram {
    fn read(&self, addr: u64, out: &mut [u8]) {
        for (i, o) in out.iter_mut().enumerate() {
            let at = (addr + i as u64).wrapping_sub(self.base) as usize;
            *o = self.data.get(at).copied().unwrap_or(0);
        }
    }

    fn write(&mut self, addr: u64, data: &[u8]) {
        let at = (addr - self.base) as usize;
        self.data[at..at + data.len()].copy_from_slice(data);
    }
}

pub struct Rng(pub u64);

impl Rng {
    pub fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }

    pub fn below(&mut self, n: u32) -> u32 {
        ((self.next() >> 20) % n as u64) as u32
    }

    /// In -1..2.
    pub fn coord(&mut self) -> f32 {
        self.below(1 << 20) as f32 / (1 << 20) as f32 * 3.0 - 1.0
    }
}

/// TIC sources.
pub const ZERO: u32 = 0;
pub const R: u32 = 2;
pub const G: u32 = 3;
pub const B: u32 = 4;
pub const A: u32 = 5;
pub const ONE_INT: u32 = 6;
pub const ONE: u32 = 7;

/// TIC types.
pub const SNORM: u32 = 1;
pub const UNORM: u32 = 2;
pub const SINT: u32 = 3;
pub const UINT: u32 = 4;
pub const FLOAT: u32 = 7;

/// A sampler view as the driver makes one.
#[derive(Clone, Copy)]
pub struct View {
    pub format: u32,
    pub types: [u32; 4],
    pub swizzle: [u32; 4],
    pub srgb: bool,
    pub scaled: bool,
    /// TIC texture type.
    pub ty: u32,
    pub width: u32,
    pub height: u32,
    /// Depth, or layers (faces of cubes count).
    pub depth: u32,
    pub last_level: u32,
    pub view_first: u32,
    pub view_last: u32,
    pub ms: MsMode,
    pub block: BlockDims,
    pub pitch: u32,
    pub address: u64,
}

impl View {
    pub fn new(format: u32, ty: u32, width: u32, height: u32, depth: u32) -> Self {
        Self {
            format,
            types: [UNORM; 4],
            swizzle: [R, G, B, A],
            srgb: false,
            scaled: false,
            ty,
            width,
            height,
            depth,
            last_level: 0,
            view_first: 0,
            view_last: 0,
            ms: MsMode::X1,
            block: BlockDims::default(),
            pitch: 0,
            address: 0x10_0000_0000,
        }
    }

    fn common(&self) -> [u32; 8] {
        let mut t = [0u32; 8];
        t[0] = self.format;
        for i in 0..4 {
            t[0] |= self.types[i] << (7 + 3 * i) | self.swizzle[i] << (19 + 3 * i);
        }
        t[1] = self.address as u32;
        t[2] = (self.address >> 32) as u32;
        t[3] = 0x10000;
        t[4] = 1 << 27 | 7 << 29 | (self.srgb as u32) << 22;
        t[5] = (!self.scaled as u32) << 31;
        t
    }

    pub fn block_linear(&self) -> [u32; 8] {
        let mut t = self.common();
        t[2] |= 3 << 21;
        t[3] |= (self.block.height as u32) << 3 | (self.block.depth as u32) << 6;
        t[3] |= 1 << 17 | 1 << 18 | self.last_level << 28;
        t[4] |= self.ty << 23 | (self.width - 1);
        let depth = if self.ty == 3 || self.ty == 8 { self.depth / 6 } else { self.depth };
        t[5] |= (self.height - 1) | (depth - 1) << 16;
        t[6] = 2 << 23 | 1 << 25;
        t[7] = self.view_last << 4 | self.view_first | (self.ms as u32) << 8;
        t
    }

    pub fn pitch(&self) -> [u32; 8] {
        let mut t = self.common();
        t[2] |= 2 << 21;
        t[3] |= self.pitch >> 5;
        t[4] |= 7 << 23 | (self.width - 1);
        t[5] |= self.height - 1;
        t
    }

    pub fn buffer(&self) -> [u32; 8] {
        let mut t = self.common();
        t[3] |= (self.width - 1) >> 16;
        t[4] |= 6 << 23 | ((self.width - 1) & 0xffff);
        t
    }
}

/// A sampler as the driver makes one.
#[derive(Clone, Copy)]
pub struct Smp {
    pub wrap: [u32; 3],
    pub mag_linear: bool,
    pub min_linear: bool,
    /// 0 none, 1 nearest, 2 linear.
    pub mip: u32,
    pub aniso: u32,
    pub compare: Option<u32>,
    pub seamless: bool,
    pub unnormalized: bool,
    pub reduction: u32,
    pub bias: f32,
    pub lod: (f32, f32),
    pub border: [u32; 4],
}

impl Smp {
    pub fn new(linear: bool, wrap: u32) -> Self {
        Self {
            wrap: [wrap; 3],
            mag_linear: linear,
            min_linear: linear,
            mip: 0,
            aniso: 1,
            compare: None,
            seamless: false,
            unnormalized: false,
            reduction: 0,
            bias: 0.0,
            lod: (0.0, 15.0),
            border: [0; 4],
        }
    }

    pub fn words(&self) -> [u32; 8] {
        let mut t = [0u32; 8];
        t[0] = 0x26000 | self.wrap[0] | self.wrap[1] << 3 | self.wrap[2] << 6;
        t[1] = if self.mag_linear { 2 } else { 1 } | if self.min_linear { 0x20 } else { 0x10 };
        t[1] |= [0x40, 0x80, 0xc0][self.mip as usize];
        t[1] |= (self.seamless as u32) << 9 | self.reduction << 10 | (self.unnormalized as u32) << 25;
        t[0] |= match self.aniso {
            16.. => 7 << 20,
            12.. => 6 << 20,
            n => (n >> 1) << 20,
        };
        if let Some(op) = self.compare {
            t[0] |= 1 << 9 | op << 10;
        }
        t[1] |= ((self.bias * 256.0) as i32 as u32 & 0x1fff) << 12;
        t[2] = ((self.lod.1.clamp(0.0, 15.0) * 256.0) as u32 & 0xfff) << 12 | ((self.lod.0 * 256.0) as u32 & 0xfff);
        t[4..].copy_from_slice(&self.border);
        t
    }
}

/// A texture in memory, laid out as the driver does: `texel(level, layer or slice, x, y)`
/// gives each texel's (or compressed block's) bytes. Returns the view with its block and
/// levels set.
pub fn build(mut v: View, bytes: u32, block: u32, texel: impl Fn(u32, u32, u32, u32) -> u128) -> (View, Ram) {
    let is_3d = v.ty == 2;
    let height = if matches!(v.ty, 0 | 4) { 1 } else { v.height };
    let shape = Shape {
        width: v.width,
        height,
        depth: if is_3d { v.depth } else { 1 },
        is_3d,
        levels: v.last_level + 1,
        block_w: block,
        block_h: block,
        block_bytes: bytes,
        ms: v.ms,
    };
    let chain = Chain::nvc0(&shape);
    let layers = if is_3d { 1 } else { v.depth };
    let mut ram = Ram::new(v.address, chain.size(layers) as usize);
    v.block = chain.levels[0].plane.block.unwrap_or_default();
    v.height = height;
    for layer in 0..layers {
        for (l, m) in chain.levels.iter().enumerate() {
            for z in 0..m.depth {
                for y in 0..m.height.div_ceil(block) {
                    for x in 0..m.width.div_ceil(block) {
                        let b = texel(l as u32, if is_3d { z } else { layer }, x, y).to_le_bytes();
                        let at = v.address + chain.layer_stride * layer as u64 + m.offset + m.plane.offset(x * bytes, y, z);
                        ram.write(at, &b[..bytes as usize]);
                    }
                }
            }
        }
    }
    (v, ram)
}
