//! The 3D class (B197): its registers, constant buffers, clears, semaphores and queries, and the
//! methods that draw.
use crate::inline::Inline;
use crate::mme::{self, Mme};
use crate::rop::{Rop, Targets};
use crate::{Memory, MemoryExt};
use maxwell_tex::{RtFormat, ZetaFormat};

pub const REGS: usize = 0x1000;

/// A constant buffer bound to a shader stage.
#[derive(Clone, Copy, Default, Debug, PartialEq, Eq)]
pub struct Cbuf {
    pub address: u64,
    pub size: u32,
}

/// The vertices of the draw being put together.
#[derive(Default)]
pub struct Batch {
    pub op: u32,
    pub instance: u32,
    /// inline index data (VB_ELEMENT_*)
    pub elements: Vec<u32>,
}

#[derive(Default, Clone, Copy, Debug)]
pub struct Counters {
    pub samples: u64,
    pub vertices: u64,
    pub primitives: u64,
    pub draws: u64,
    pub fragments: u64,
}

impl Counters {
    pub fn add(&mut self, o: &Self) {
        self.samples += o.samples;
        self.vertices += o.vertices;
        self.primitives += o.primitives;
        self.draws += o.draws;
        self.fragments += o.fragments;
    }
}

pub struct Threed {
    pub regs: Box<[u32; REGS]>,
    pub mme: Mme,
    pub inline: Inline,
    /// constant buffers by stage (vertex, tessellation control and evaluation, geometry, fragment)
    pub cbufs: [[Cbuf; 18]; 5],
    pub batch: Batch,
    pub rop: Rop,
    pub counters: Counters,
    pub pipe: crate::pipe::Pipe,
}

impl Default for Threed {
    fn default() -> Self {
        let mut regs: Box<[u32; REGS]> = vec![0; REGS].into_boxed_slice().try_into().unwrap();
        // what the hardware starts with where a driver relies on it
        for (m, v) in [(0x19c0, 1u32), (0x0f90, 0), (0x1558, 1), (0x0fbc, 0xffff), (0x0fc0, 0xffff), (0x0fc4, 0xffff), (0x0fc8, 0xffff)] {
            regs[m / 4] = v;
        }
        for k in 0..8 {
            regs[(0x1a00 + 4 * k) / 4] = 0x1111;
        }
        regs[0x37c / 4] = 1;
        Self { regs, mme: Mme::default(), inline: Inline::default(), cbufs: Default::default(), batch: Batch::default(), rop: Rop::default(), counters: Counters::default(), pipe: Default::default() }
    }
}

/// What a macro sees.
struct Macro<'a, 'm> {
    t: &'a mut Threed,
    mem: &'m mut dyn Memory,
    now: u64,
}

impl mme::Methods for Macro<'_, '_> {
    fn read(&mut self, method: u32) -> u32 {
        self.t.r(method)
    }
    fn write(&mut self, method: u32, v: u32) {
        self.t.method(method, v, self.mem, self.now);
    }
}

impl Threed {
    #[inline(always)]
    pub fn r(&self, m: u32) -> u32 {
        self.regs[m as usize / 4]
    }

    #[inline(always)]
    pub fn f(&self, m: u32) -> f32 {
        f32::from_bits(self.r(m))
    }

    /// An address in two registers (high, low).
    pub fn address(&self, m: u32) -> u64 {
        ((self.r(m) & 0xff) as u64) << 32 | self.r(m + 4) as u64
    }

    /// A group of methods (a push buffer header's) ended: run the macro it called.
    pub fn end_group(&mut self, mem: &mut dyn Memory, now: u64) {
        if let Some(call) = self.mme.take_call() {
            self.run_macro(call, mem, now);
        }
    }

    fn run_macro(&mut self, (k, params): (usize, Vec<u32>), mem: &mut dyn Memory, now: u64) {
        let (code, entry) = self.mme.program(k);
        let code = code.to_vec();
        mme::run(&code, entry, &params, &mut Macro { t: self, mem, now });
    }

    pub fn method(&mut self, m: u32, v: u32, mem: &mut dyn Memory, now: u64) {
        if m >= 0x3800 {
            if let Some(call) = self.mme.call(m, v) {
                self.run_macro(call, mem, now);
            }
            return;
        }
        if self.mme.upload(m, v) {
            return;
        }
        let i = m as usize / 4;
        // a change of where pixels go writes out what is cached
        if self.regs[i] != v && (matches!(m, 0x800..=0x9ff | 0xfe0..=0xff0 | 0x1228..=0x1230 | 0x121c | 0x1538 | 0x179c | 0x15d0)) {
            self.rop.flush(mem);
        }
        self.regs[i] = v;
        if Inline::owns(m) {
            self.rop.flush(mem);
            self.inline.method(m, v, mem);
            return;
        }
        match m {
            0x110 => self.rop.flush(mem), // serialize
            0x1330 | 0x1334 | 0x1338 | 0x1288 | 0x1698 | 0x1210 | 0x120c => {
                self.rop.flush(mem);
                self.pipe.invalidate_textures();
            }
            0x21c | 0x1280 => self.pipe.invalidate_shaders(),
            0x238c => {}
            0x2390..=0x23cc => {
                let at = self.address(0x2384) + self.r(0x238c) as u64;
                mem.write32(at, v);
                self.regs[0x238c / 4] += 4;
                self.pipe.cbufs_changed = true;
            }
            0x2410 | 0x2430 | 0x2450 | 0x2470 | 0x2490 => {
                let stage = (m as usize - 0x2410) / 0x20;
                if let Some(c) = self.cbufs[stage].get_mut((v >> 4 & 31) as usize) {
                    *c = if v & 1 != 0 { Cbuf { address: ((self.regs[0x2384 / 4] & 0xff) as u64) << 32 | self.regs[0x2388 / 4] as u64, size: self.regs[0x2380 / 4] } } else { Cbuf::default() };
                }
                self.pipe.cbufs_changed = true;
            }
            0x19d0 => {
                if self.rendering(mem) {
                    self.clear(v, mem);
                }
            }
            0x1530 => {
                if v == 1 {
                    self.counters.samples = 0;
                }
            }
            0x1b0c => self.report(v, mem, now),
            0x1618 => {
                self.batch.op = v & 0xffff;
                match v >> 26 & 3 {
                    0 => self.batch.instance = 0,
                    1 => self.batch.instance += 1,
                    _ => {}
                }
                self.batch.elements.clear();
            }
            0x1614 => {
                if !self.batch.elements.is_empty() {
                    let e = std::mem::take(&mut self.batch.elements);
                    self.draw(crate::pipe::Indices::List(&e), mem);
                }
            }
            0xd78 => {
                let first = self.r(0xd74);
                self.draw(crate::pipe::Indices::Range(first, v), mem);
            }
            0x17e0 => self.draw_indexed(self.r(0x17dc), v, mem),
            // (first, count, op) in one word: 8, 16 and 32-bit indices, and vertex arrays
            0x17e4 | 0x17e8 | 0x17ec | 0x17f0 | 0x17f4 | 0x17f8 | 0x1214 | 0x1218 => {
                self.batch.op = v >> 28;
                self.batch.instance = if matches!(m, 0x17f0 | 0x17f4 | 0x17f8 | 0x1218) { self.batch.instance + 1 } else { 0 };
                let (first, count) = (v & 0xffff, v >> 16 & 0xfff);
                match m {
                    0x1214 | 0x1218 => self.draw(crate::pipe::Indices::Range(first, count), mem),
                    _ => {
                        let size = match m { 0x17e4 | 0x17f0 => 2, 0x17e8 | 0x17f4 => 1, _ => 0 };
                        self.regs[0x17d8 / 4] = size;
                        self.draw_indexed(first, count, mem);
                    }
                }
            }
            0x15e8 => self.batch.elements.push(v),
            0x15f0 => {
                let n = self.r(0x15ec);
                self.batch.elements.push(v & 0xffff);
                if n >> 30 == 0 || self.batch.elements.len() as u32 % 2 == 1 {
                    self.batch.elements.push(v >> 16);
                }
                self.trim(0x15ec);
            }
            0x1304 => {
                for k in 0..4 {
                    self.batch.elements.push(v >> (8 * k) & 0xff);
                }
                self.trim(0x1300);
            }
            0x1150..=0x115c => {
                // a constant attribute's value, word by word
                let d = self.r(0x114c);
                let (attribute, first) = ((d & 0xff) as usize, (d >> 8 & 7) as usize);
                let k = (m as usize - 0x1150) / 4;
                if let Some(a) = self.pipe.constants.get_mut(attribute) {
                    if k == 0 {
                        *a = [0, 0, 0, 1f32.to_bits()];
                    }
                    if first + k < 4 {
                        a[first + k] = v;
                    }
                }
            }
            _ => {}
        }
    }

    /// The number of inline indices is in the setup register.
    fn trim(&mut self, setup: u32) {
        let n = (self.r(setup) & 0x3fff_ffff) as usize;
        if n != 0 && self.batch.elements.len() > n {
            self.batch.elements.truncate(n);
        }
    }

    /// Conditional rendering.
    pub fn rendering(&self, mem: &dyn Memory) -> bool {
        let at = self.address(0x1550);
        match self.r(0x1558) & 7 {
            0 => false,
            1 => true,
            2 => mem.read32(at) != 0 || mem.read32(at + 4) != 0,
            mode => {
                let same = mem.read32(at) == mem.read32(at + 16) && mem.read32(at + 4) == mem.read32(at + 20);
                same == (mode == 3)
            }
        }
    }

    /// A semaphore release or a query's report.
    fn report(&mut self, v: u32, mem: &mut dyn Memory, now: u64) {
        self.rop.flush(mem);
        let at = self.address(0x1b00);
        let value = match v >> 23 & 31 {
            2 | 0x15 => Some(self.counters.samples),
            1 | 5 => Some(self.counters.vertices),
            3 | 0x0f | 0x11 | 0x12 => Some(self.counters.primitives),
            0x13 => Some(self.counters.fragments),
            0 => None,
            _ => Some(0),
        };
        match v & 3 {
            // (one word: the payload)
            0 if v >> 28 & 1 != 0 => mem.write32(at, self.r(0x1b08)),
            0 | 2 => {
                let mut b = [0u8; 16];
                match value {
                    Some(n) if v & 3 == 2 => b[..8].copy_from_slice(&n.to_le_bytes()),
                    _ => b[..4].copy_from_slice(&self.r(0x1b08).to_le_bytes()),
                }
                b[8..].copy_from_slice(&now.to_le_bytes());
                mem.write(at, &b);
            }
            _ => {}
        }
    }

    fn clear(&mut self, v: u32, mem: &mut dyn Memory) {
        let targets = Targets::new(self);
        let layer = v >> 10 & 0xffff;
        let mut rect = targets.rect();
        let flags = self.r(0x10f8);
        if flags & 0x100 != 0 && self.r(0xe00) & 1 != 0 {
            rect = rect.and(self.r(0xe04), self.r(0xe08));
        }
        if flags & 0x1000 != 0 {
            rect = rect.and(self.r(0xc00).wrapping_add(self.r(0xc00) << 16), self.r(0xc04).wrapping_add(self.r(0xc04) << 16));
        }
        if rect.empty() {
            return;
        }
        self.rop.flush(mem);
        if v & 0x3c != 0 {
            let rt = (v >> 6 & 15) as usize;
            if let Some((surface, format)) = targets.color.get(rt).copied().flatten() {
                let c = [self.r(0xd80), self.r(0xd84), self.r(0xd88), self.r(0xd8c)];
                let mut mask: [bool; 4] = std::array::from_fn(|k| v >> (2 + k) & 1 != 0);
                if flags & 1 == 0 {
                    // (the colour mask does not apply)
                } else {
                    let m = self.color_mask(rt);
                    (0..4).for_each(|k| mask[k] &= m >> (4 * k) & 1 != 0);
                }
                let format = if self.r(0x19c8 - 4) == 0 && false { format.linear() } else { format };
                crate::rop::clear_color(&surface, &format, c, mask, rect, layer, mem);
            }
        }
        if v & 3 != 0 {
            if let Some((surface, format)) = targets.zeta {
                let depth = (v & 1 != 0 && format.has_depth()).then(|| self.f(0xd90));
                let stencil = (v & 2 != 0 && format.has_stencil()).then(|| self.r(0xda0) as u8);
                crate::rop::clear_zeta(&surface, &format, depth, stencil, 0xff, rect, layer, mem);
            }
        }
    }

    pub fn color_mask(&self, rt: usize) -> u32 {
        let rt = if self.r(0xf90) != 0 { 0 } else { rt };
        self.r(0x1a00 + 4 * rt as u32)
    }

    fn draw_indexed(&mut self, first: u32, count: u32, mem: &mut dyn Memory) {
        let size = self.r(0x17d8) & 3;
        let at = self.address(0x17c8) + ((first as u64) << size);
        let limit = self.address(0x17d0);
        let count = count.min(((limit + 1).saturating_sub(at) >> size) as u32);
        let list = crate::vertex::indices(at, size, count as usize, mem);
        self.draw(crate::pipe::Indices::List(&list), mem);
    }

    fn draw(&mut self, indices: crate::pipe::Indices, mem: &mut dyn Memory) {
        self.counters.draws += 1;
        if self.rendering(mem) {
            crate::pipe::draw(self, indices, mem);
        }
    }
}

/// The formats of a target's registers.
pub fn rt_format(v: u32) -> Option<RtFormat> {
    RtFormat::from_code(v as u8)
}

pub fn zeta_format(v: u32) -> Option<ZetaFormat> {
    ZetaFormat::from_code(v as u8)
}
