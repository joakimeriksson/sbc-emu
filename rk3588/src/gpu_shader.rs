//! Valhall shader execution. Encodings follow Mesa's Valhall ISA.xml and va_pack.c.
//!
//! A [`Shader`] is one thread: its registers, program counter and per-stage state. Vertex
//! and compute threads run alone ([`Shader::run`]); fragment threads run as 2x2 quads
//! ([`run_quad`]) so cross-lane permutes and texture derivatives see their neighbours.
//! Instruction words are read through a per-job page cache ([`Code`]) and memory goes
//! through a translation cache ([`Tlb`]), so guest page tables are walked once per page
//! and job. Unknown opcodes, reserved encodings, inaccessible registers and exhausted
//! budgets fault rather than complete work.
use crate::gpu_mmu::{Access, AddressSpace, Fault};
use crate::gpu_resources::Resources;
use std::collections::HashMap;

mod msg;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Error {
    Memory(Fault),
    Unsupported { pc: u64, instruction: u64 },
    Budget,
}
#[derive(Clone, Copy, Debug)]
pub enum RegisterAllocation {
    Registers32,
    Registers64,
}

/// Base of the private IDVS output table (resource table 13). Position and varying
/// records stay on the host: on the GPU they stream through an internal FIFO into the
/// tiler heap, in a format only the GPU reads.
pub const OUTPUT_BASE: u64 = 0xffff_0000_0000_0000;

/// Fragment-thread state: what fixed-function hardware supplies, and what the shader
/// sends to the blender and the depth/stencil unit.
#[derive(Clone, Debug, Default)]
pub struct Fragment {
    /// Internal blend descriptors (words 2-3 of each render target's Blend descriptor).
    pub blend: [[u32; 2]; 8],
    /// Colour sent by BLEND, per render target.
    pub color: [Option<[f32; 4]>; 8],
    /// Raw register values sent by BLEND with an integer register format.
    pub raw: [Option<[u32; 4]>; 8],
    /// Depth and stencil written by ZS_EMIT.
    pub depth: Option<f32>,
    pub stencil: Option<u32>,
    /// The thread still covers its pixel: false for helper threads and discarded pixels.
    pub active: bool,
    /// gl_FragCoord z and w, and the point-sprite coordinate, for LD_VAR_SPECIAL.
    pub frag_z: f32,
    pub frag_w: f32,
    pub point: [f32; 2],
    pub atest_datum: u32,
    pub sample_positions: u64,
}
impl Fragment {
    pub fn new(blend: [u32; 2]) -> Self {
        let mut f = Self {
            active: true,
            ..Self::default()
        };
        f.blend[0] = blend;
        f
    }
}

fn half(value: u16) -> f32 {
    let sign = ((value as u32) & 0x8000) << 16;
    let exp = ((value >> 10) & 31) as u32;
    let mant = (value & 1023) as u32;
    if exp == 0 {
        return (if sign == 0 { 1.0 } else { -1.0 }) * mant as f32 * 2f32.powi(-24);
    }
    f32::from_bits(sign | (if exp == 31 { 255 } else { exp + 112 }) << 23 | mant << 13)
}
/// IEEE binary32 to binary16, round to nearest with ties to even.
fn pack_half(value: f32) -> u16 {
    let b = value.to_bits();
    let sign = ((b >> 16) & 0x8000) as u16;
    let exp = ((b >> 23) & 255) as i32;
    let mant = b & 0x7fffff;
    if exp == 255 {
        return sign
            | 0x7c00
            | if mant == 0 {
                0
            } else {
                ((mant >> 13) as u16) | 0x200
            };
    }
    let exp = exp - 112;
    if exp <= 0 {
        if exp < -10 {
            return sign;
        }
        let mant = mant | 0x800000;
        let shift = (14 - exp) as u32;
        let base = mant >> shift;
        let tail = mant & ((1 << shift) - 1);
        return sign
            | (base
                + u32::from(tail > 1 << (shift - 1) || tail == 1 << (shift - 1) && base & 1 != 0))
                as u16;
    }
    let rounded = mant + 0xfff + ((mant >> 13) & 1);
    let exp = exp + (rounded >> 23) as i32;
    if exp >= 31 {
        sign | 0x7c00
    } else {
        sign | ((exp as u16) << 10) | ((rounded >> 13) & 1023) as u16
    }
}
pub fn f16_to_f32(v: u16) -> f32 {
    half(v)
}
pub fn f32_to_f16(v: f32) -> u16 {
    pack_half(v)
}

/// Varying data of one primitive and a pixel's interpolation weights. The weights are
/// perspective-correct barycentrics from the rasterizer; `provoking` is the vertex flat
/// varyings come from.
#[derive(Clone, Debug)]
pub struct Interpolation {
    pub data: std::sync::Arc<[u8]>,
    pub stride: usize,
    pub vertices: [usize; 3],
    pub weights: [f64; 3],
    pub provoking: usize,
}
impl Interpolation {
    pub fn load(&self, offset: usize, components: usize, width: usize) -> Option<[f32; 4]> {
        if offset % width != 0 || offset.checked_add(components * width)? > self.stride {
            return None;
        }
        let mut out = [0.; 4];
        for (n, out) in out.iter_mut().enumerate().take(components) {
            let mut value = 0.;
            for v in 0..3 {
                let source = self.fetch(self.vertices[v], offset + n * width, width)?;
                if self.weights[v] != 0. {
                    value += source as f64 * self.weights[v];
                }
            }
            *out = value as f32;
        }
        Some(out)
    }
    fn fetch(&self, vertex: usize, offset: usize, width: usize) -> Option<f32> {
        let at = vertex.checked_mul(self.stride)?.checked_add(offset)?;
        let bytes = self.data.get(at..at.checked_add(width)?)?;
        Some(if width == 4 {
            f32::from_le_bytes(bytes.try_into().ok()?)
        } else {
            half(u16::from_le_bytes(bytes.try_into().ok()?))
        })
    }
    /// Flat (provoking-vertex) load of raw 32- or 16-bit values.
    fn flat(&self, offset: usize, components: usize, width: usize) -> Option<[u32; 4]> {
        if offset % width != 0 || offset.checked_add(components * width)? > self.stride {
            return None;
        }
        let mut out = [0; 4];
        for (n, out) in out.iter_mut().enumerate().take(components) {
            let at = self
                .provoking
                .checked_mul(self.stride)?
                .checked_add(offset + n * width)?;
            let bytes = self.data.get(at..at.checked_add(width)?)?;
            *out = if width == 4 {
                u32::from_le_bytes(bytes.try_into().ok()?)
            } else {
                u16::from_le_bytes(bytes.try_into().ok()?) as u32
            };
        }
        Some(out)
    }
}

/// Instruction pages read during one job. Shader binaries cannot change while the GPU
/// executes a job, so each 4 KiB page is translated and copied once.
#[derive(Default)]
pub struct Code {
    pages: HashMap<u64, Box<[u64; 512]>>,
}
impl Code {
    pub fn fetch(&mut self, m: &AddressSpace, ram: &[u8], pc: u64) -> Result<u64, Fault> {
        let page = pc >> 12;
        if let Some(p) = self.pages.get(&page) {
            return Ok(p[((pc >> 3) & 511) as usize]);
        }
        let mut bytes = vec![0u8; 4096];
        if m.read(ram, page << 12, &mut bytes, true).is_err() {
            // A page that is not entirely mapped: read just this instruction.
            let mut b = [0u8; 8];
            m.read(ram, pc, &mut b, true)?;
            return Ok(u64::from_le_bytes(b));
        }
        let mut words = Box::new([0u64; 512]);
        for (n, w) in words.iter_mut().enumerate() {
            *w = u64::from_le_bytes(bytes[n * 8..n * 8 + 8].try_into().unwrap());
        }
        let insn = words[((pc >> 3) & 511) as usize];
        self.pages.insert(page, words);
        Ok(insn)
    }
}

/// A translation cache for one job: guest page tables do not change while it runs.
#[derive(Default)]
pub struct Tlb {
    read: HashMap<u64, usize>,
    write: HashMap<u64, usize>,
}
impl Tlb {
    fn page(
        &mut self,
        m: &AddressSpace,
        ram: &[u8],
        va: u64,
        access: Access,
    ) -> Result<usize, Fault> {
        let vpn = va >> 12;
        let map = if access == Access::Write {
            &mut self.write
        } else {
            &mut self.read
        };
        if let Some(&pa) = map.get(&vpn) {
            return Ok(pa);
        }
        let pa = usize::try_from(m.translate(ram, vpn << 12, access, true)?)
            .map_err(|_| Fault::PhysicalBus)?;
        if map.len() > 65536 {
            map.clear();
        }
        map.insert(vpn, pa);
        Ok(pa)
    }
    pub fn read(
        &mut self,
        m: &AddressSpace,
        ram: &[u8],
        va: u64,
        out: &mut [u8],
    ) -> Result<(), Fault> {
        let mut done = 0;
        while done < out.len() {
            let a = va.checked_add(done as u64).ok_or(Fault::AddressSize)?;
            let n = (4096 - (a & 4095) as usize).min(out.len() - done);
            let pa = self.page(m, ram, a, Access::Read)? + (a & 4095) as usize;
            let src = ram
                .get(pa..pa.checked_add(n).ok_or(Fault::PhysicalBus)?)
                .ok_or(Fault::PhysicalBus)?;
            out[done..done + n].copy_from_slice(src);
            done += n;
        }
        Ok(())
    }
    /// Write, translating every page first: a faulting store changes nothing.
    pub fn write(
        &mut self,
        m: &AddressSpace,
        ram: &mut [u8],
        va: u64,
        data: &[u8],
    ) -> Result<(), Fault> {
        let mut spans = Vec::with_capacity(2);
        let mut done = 0;
        while done < data.len() {
            let a = va.checked_add(done as u64).ok_or(Fault::AddressSize)?;
            let n = (4096 - (a & 4095) as usize).min(data.len() - done);
            let pa = self.page(m, ram, a, Access::Write)? + (a & 4095) as usize;
            if pa.checked_add(n).is_none_or(|end| end > ram.len()) {
                return Err(Fault::PhysicalBus);
            }
            spans.push((pa, done, n));
            done += n;
        }
        for (pa, off, n) in spans {
            ram[pa..pa + n].copy_from_slice(&data[off..off + n]);
        }
        Ok(())
    }
}

/// What a running thread can reach besides its registers.
pub struct Env<'a> {
    pub m: &'a AddressSpace,
    pub ram: &'a mut [u8],
    pub uniforms: &'a [u32],
    pub code: &'a mut Code,
    pub tlb: &'a mut Tlb,
    /// Host-side IDVS output (positions or varyings). A table-13 element index `e`
    /// addresses byte `e * unit.0 / unit.1` of it.
    pub output: Option<&'a mut [u8]>,
    pub output_unit: (u32, u32),
    /// Texture and sampler descriptors decoded during this job.
    pub textures: &'a mut crate::gpu_texture::Cache,
}
impl<'a> Env<'a> {
    pub fn new(
        m: &'a AddressSpace,
        ram: &'a mut [u8],
        uniforms: &'a [u32],
        code: &'a mut Code,
        tlb: &'a mut Tlb,
        textures: &'a mut crate::gpu_texture::Cache,
    ) -> Self {
        Env {
            m,
            ram,
            uniforms,
            code,
            tlb,
            output: None,
            output_unit: (0, 1),
            textures,
        }
    }
    pub fn read(&mut self, va: u64, out: &mut [u8]) -> Result<(), Fault> {
        self.tlb.read(self.m, self.ram, va, out)
    }
    pub fn write(&mut self, va: u64, data: &[u8]) -> Result<(), Fault> {
        self.tlb.write(self.m, self.ram, va, data)
    }
}

pub struct Shader {
    pub regs: [u32; 64],
    pub resources: Resources,
    pub fragment: Option<Fragment>,
    pub interpolation: Option<Interpolation>,
    /// Thread and workgroup local storage pointers (FAU page 1).
    pub tls: u64,
    pub wls: u64,
    register_mask: u64,
    pub pc: u64,
    pub instructions: u64,
    pub done: bool,
}

const LUT: [u32; 32] = [
    0, 0xffffffff, 0x7fffffff, 0xfafcfdfe, 0x01000000, 0x80002000, 0x70605030, 0xc0b0a090,
    0x03020100, 0x07060504, 0x0b0a0908, 0x0f0e0d0c, 0x13121110, 0x17161514, 0x1b1a1918, 0x1f1e1d1c,
    0x3f800000, 0x3dcccccd, 0x3ea2f983, 0x3f317218, 0x40490fdb, 0, 0x477fff00, 0x5c005bf8,
    0x2e660000, 0x34000000, 0x38000000, 0x3c000000, 0x40000000, 0x44000000, 0x48000000, 0x42480000,
];

fn bits(i: u64, start: u32, count: u32) -> u32 {
    ((i >> start) & ((1u64 << count) - 1)) as u32
}
fn sext(v: u32, width: u32) -> i32 {
    ((v << (32 - width)) as i32) >> (32 - width)
}
/// Floating-point comparison (Condition enum: eq gt ge ne lt le gtlt).
fn fcmp(cond: u32, x: f32, y: f32) -> Option<bool> {
    Some(match cond {
        0 => x == y,
        1 => x > y,
        2 => x >= y,
        3 => x != y,
        4 => x < y,
        5 => x <= y,
        6 => x < y || x > y,
        _ => return None,
    })
}
fn icmp<T: PartialOrd>(cond: u32, x: T, y: T) -> Option<bool> {
    Some(match cond {
        0 => x == y,
        1 => x > y,
        2 => x >= y,
        3 => x != y,
        4 => x < y,
        5 => x <= y,
        _ => return None,
    })
}
fn clamp(mode: u32, v: f32) -> f32 {
    match mode {
        1 => v.max(0.),
        2 => v.clamp(-1., 1.),
        3 => v.clamp(0., 1.),
        _ => v,
    }
}
fn round(mode: u32, v: f32) -> f32 {
    match mode {
        0 => v.round_ties_even(),
        1 => v.ceil(),
        2 => v.floor(),
        _ => v.trunc(),
    }
}
/// `x * 2^e` without intermediate overflow.
fn ldexp(x: f32, e: i32) -> f32 {
    (x as f64 * 2f64.powi(e.clamp(-400, 400))) as f32
}
/// Mantissa and exponent: mantissa in [0.5, 1) like frexp(), in [0.75, 1.5) for the
/// logarithm reduction, or with an even exponent for the square root reduction.
fn frexp(x: f32, sqrt: bool, log: bool) -> (f32, i32) {
    if x == 0. || !x.is_finite() {
        return (x, 0);
    }
    let mut e = (x.abs() as f64).log2().floor() as i32 + 1;
    let mut m = (x as f64 / 2f64.powi(e)) as f32;
    if m.abs() >= 1. {
        m /= 2.;
        e += 1;
    } else if m.abs() < 0.5 {
        m *= 2.;
        e -= 1;
    }
    if log && m.abs() < 0.75 {
        m *= 2.;
        e -= 1;
    } else if sqrt && e & 1 != 0 {
        m /= 2.;
        e += 1;
    }
    (m, e)
}
fn fmin(a: f32, b: f32) -> f32 {
    a.min(b)
}
fn fmax(a: f32, b: f32) -> f32 {
    a.max(b)
}
fn widen32(v: u32, field: u32, signed: bool) -> Option<u32> {
    let ext16 = |x: u32| if signed { sext(x, 16) as u32 } else { x };
    let ext8 = |x: u32| if signed { sext(x, 8) as u32 } else { x };
    Some(match field {
        0 => v,
        2 => ext16(v & 0xffff),
        3 => ext16(v >> 16),
        4..=7 => ext8((v >> (8 * (field - 4))) & 255),
        _ => return None,
    })
}
fn pack2(lo: u32, hi: u32) -> u32 {
    (lo & 0xffff) | (hi << 16)
}
fn f2h(v: [f32; 2]) -> u32 {
    pack_half(v[0]) as u32 | (pack_half(v[1]) as u32) << 16
}
fn f32_to_int(v: f32, mode: u32, signed: bool) -> u32 {
    let r = round(mode, v);
    if r.is_nan() {
        0
    } else if signed {
        r.clamp(i32::MIN as f32, i32::MAX as f32) as i32 as u32
    } else {
        r.clamp(0., u32::MAX as f32) as u32
    }
}
/// Result type: i1, f1, m1, u1.
fn result_bool(kind: u32, v: bool) -> u32 {
    match (kind, v) {
        (_, false) => 0,
        (1, true) => 1f32.to_bits(),
        (2, true) => u32::MAX,
        _ => 1,
    }
}
fn result_bool16(kind: u32, v: bool) -> u32 {
    match (kind, v) {
        (_, false) => 0,
        (1, true) => 0x3c00,
        (2, true) => 0xffff,
        _ => 1,
    }
}

/// Operand decoding for one instruction word.
struct Ops<'s> {
    s: &'s Shader,
    i: u64,
    uniforms: &'s [u32],
}
type R<T> = Result<T, Option<Error>>;
impl Ops<'_> {
    fn raw(&self, n: usize) -> R<u32> {
        self.s.source(self.i, n, self.uniforms).ok_or(None)
    }
    fn absneg(&self, n: usize, mut v: f32) -> f32 {
        let shift = 38 - 2 * n as u32;
        if self.i & (1 << (shift + 1)) != 0 {
            v = v.abs();
        }
        if self.i & (1 << shift) != 0 {
            v = -v;
        }
        v
    }
    /// 32-bit float source with the half-widening swizzle (Widen enum) and abs/neg.
    fn f32(&self, n: usize) -> R<f32> {
        let b = self.raw(n)?;
        let v = match bits(self.i, 28 - 2 * n as u32, 2) {
            0 => f32::from_bits(b),
            1 => half(b as u16),
            2 => half((b >> 16) as u16),
            _ => return Err(None),
        };
        Ok(self.absneg(n, v))
    }
    /// 32-bit float source without a swizzle field.
    fn f32_plain(&self, n: usize) -> R<f32> {
        Ok(self.absneg(n, f32::from_bits(self.raw(n)?)))
    }
    fn swizzle16(&self, n: usize) -> R<[u16; 2]> {
        let b = self.raw(n)?;
        let h = [b as u16, (b >> 16) as u16];
        Ok(match bits(self.i, 28 - 2 * n as u32, 2) {
            0 => [h[0], h[0]],
            1 => [h[1], h[0]],
            2 => h,
            _ => [h[1], h[1]],
        })
    }
    /// Two 16-bit floats with a 16-bit swizzle (h00 h10 h01 h11) and abs/neg.
    fn v2f16(&self, n: usize) -> R<[f32; 2]> {
        Ok(self.swizzle16(n)?.map(|h| self.absneg(n, half(h))))
    }
    fn widen_field(&self, n: usize) -> u32 {
        match n {
            0 => bits(self.i, 36, 4),
            1 => bits(self.i, 26, 4),
            _ => 0,
        }
    }
    /// Integer source with the 32-bit widen (none, h0, h1, b0-b3).
    fn i32w(&self, n: usize, signed: bool) -> R<u32> {
        widen32(self.raw(n)?, self.widen_field(n), signed).ok_or(None)
    }
    /// Two 16-bit integer lanes (Swizzles 16-bit enum, bytes sign-extended if signed).
    fn v2i16w(&self, n: usize, signed: bool) -> R<[u32; 2]> {
        let v = self.raw(n)?;
        let field = if n < 2 { self.widen_field(n) } else { 2 };
        let h = |k: u32| (v >> (16 * k)) & 0xffff;
        let b = |k: u32| {
            let x = (v >> (8 * k)) & 255;
            if signed {
                sext(x, 8) as u32 & 0xffff
            } else {
                x
            }
        };
        Ok(match field {
            0 => [h(0), h(0)],
            1 => [h(1), h(0)],
            2 => [h(0), h(1)],
            3 => [h(1), h(1)],
            4 => [b(0), b(0)],
            5 => [b(2), b(0)],
            6 => [b(0), b(2)],
            7 => [b(2), b(2)],
            8 => [b(1), b(1)],
            9 => [b(3), b(1)],
            10 => [b(1), b(3)],
            11 => [b(3), b(3)],
            12 => [b(0), b(1)],
            13 => [b(2), b(3)],
            _ => return Err(None),
        })
    }
    /// Four 8-bit lanes (Swizzles 8-bit enum).
    fn v4i8w(&self, n: usize) -> R<[u32; 4]> {
        let v = self.raw(n)?;
        let b = |k: u32| (v >> (8 * k)) & 255;
        let sel: [u32; 4] = match self.widen_field(n) {
            0 => [0, 1, 2, 3],
            1 => [3, 2, 1, 0],
            2 => [0, 1, 0, 1],
            3 => [2, 3, 2, 3],
            4 => [0; 4],
            5 => [1; 4],
            6 => [2; 4],
            7 => [3; 4],
            8 => [2, 3, 0, 1],
            9 => [1, 0, 3, 2],
            10 => [0, 0, 1, 1],
            11 => [2, 2, 3, 3],
            _ => return Err(None),
        };
        Ok(sel.map(b))
    }
}

/// Effect of one instruction on control flow.
enum Next {
    Continue,
    Jump(u64),
    End,
}

/// The registers of all lanes of a 2x2 quad at the instruction being executed, and
/// which of them execute it.
pub struct QuadView {
    pub regs: [[u32; 64]; 4],
    pub active: [bool; 4],
    pub lane: usize,
    /// Texture coordinates of each lane, for implicit-LOD derivatives.
    pub tex: [[f32; 4]; 4],
}

impl Shader {
    pub fn new(pc: u64) -> Self {
        Self::with_allocation(pc, RegisterAllocation::Registers64)
    }
    pub fn with_allocation(pc: u64, allocation: RegisterAllocation) -> Self {
        Self {
            regs: [0; 64],
            resources: Resources::default(),
            fragment: None,
            interpolation: None,
            tls: 0,
            wls: 0,
            pc,
            instructions: 0,
            done: false,
            // Mesa bi_ra.c: full occupancy uses R0-R15 and R48-R63.
            register_mask: match allocation {
                RegisterAllocation::Registers32 => 0xffff_0000_0000_ffff,
                RegisterAllocation::Registers64 => u64::MAX,
            },
        }
    }
    fn registers_available(&self, first: usize, count: usize) -> bool {
        count != 0
            && first < 64
            && count <= 64 - first
            && (first..first + count).all(|r| self.register_mask & (1 << r) != 0)
    }
    /// The 32-bit value of source byte `index`: a register, a uniform, an immediate
    /// from the lookup table or a special FAU value (page in bits 57-58).
    fn source(&self, i: u64, index: usize, uniforms: &[u32]) -> Option<u32> {
        let s = ((i >> (index * 8)) & 255) as usize;
        let page = ((i >> 57) & 3) as usize;
        match s >> 6 {
            0 | 1 => self
                .regs
                .get(s & 63)
                .filter(|_| self.registers_available(s & 63, 1))
                .copied(),
            2 => uniforms.get(page * 64 + (s & 63)).copied(),
            _ if s & 63 < 32 => {
                if page != 0 {
                    return None;
                }
                LUT.get(s & 63).copied()
            }
            _ => {
                let special = (s & 63) - 32;
                let (n, hi) = (special >> 1, special & 1 == 1);
                let half = |v: u64| if hi { (v >> 32) as u32 } else { v as u32 };
                match page {
                    0 => {
                        let f = self.fragment.as_ref()?;
                        match n {
                            2 => Some(0),           // warp ID
                            4 => Some(0xffff_ffff), // bounding box maximum
                            5 => Some(if hi { 0 } else { f.atest_datum }),
                            6 => Some(half(f.sample_positions)),
                            8..=15 => Some(f.blend[n - 8][hi as usize]),
                            _ => None,
                        }
                    }
                    1 => match n {
                        1 => Some(half(self.tls)),
                        3 => Some(half(self.wls)),
                        7 => Some(half(self.resources.0)),
                        _ => None,
                    },
                    3 => match n {
                        1 | 3 => Some(0), // lane and core ID
                        15 => Some(half(self.pc.wrapping_add(8))),
                        _ => None,
                    },
                    _ => None,
                }
            }
        }
    }
    /// A 64-bit source: an aligned register pair, or a uniform/special pair.
    fn source64(&self, i: u64, index: usize, uniforms: &[u32]) -> Option<u64> {
        let s = ((i >> (index * 8)) & 255) as usize;
        if s >> 6 <= 1 {
            let r = s & 63;
            if r & 1 != 0 || !self.registers_available(r, 2) {
                return None;
            }
            return Some(self.regs[r] as u64 | (self.regs[r + 1] as u64) << 32);
        }
        if s & 1 != 0 {
            return None;
        }
        let lo = self.source(i, index, uniforms)?;
        if s >> 6 == 3 && s & 63 < 32 {
            return Some(lo as u64); // small immediates are zero-extended
        }
        let hi = self.source(i | (1 << (index * 8)), index, uniforms)?;
        Some(lo as u64 | (hi as u64) << 32)
    }
    fn unsupported(&self, i: u64) -> Error {
        Error::Unsupported {
            pc: self.pc,
            instruction: i,
        }
    }
    fn write_dest(&mut self, i: u64, v: u32) -> R<()> {
        let dst = bits(i, 40, 6) as usize;
        if !self.registers_available(dst, 1) {
            return Err(None);
        }
        self.regs[dst] = match bits(i, 46, 2) {
            3 => v,
            1 => (self.regs[dst] & 0xffff_0000) | (v & 0xffff),
            2 => (self.regs[dst] & 0xffff) | (v & 0xffff_0000),
            _ => return Err(None),
        };
        Ok(())
    }
    fn write_dest64(&mut self, i: u64, v: u64) -> R<()> {
        let dst = bits(i, 40, 6) as usize;
        if dst & 1 != 0 || !self.registers_available(dst, 2) || bits(i, 46, 2) != 3 {
            return Err(None);
        }
        self.regs[dst] = v as u32;
        self.regs[dst + 1] = (v >> 32) as u32;
        Ok(())
    }
    /// The first staging register for `count` registers, checked for allocation and
    /// (for vectors) alignment.
    fn staging(&self, i: u64, count: usize) -> Option<usize> {
        let r = bits(i, 40, 6) as usize;
        if !self.registers_available(r, count.max(1)) || count > 1 && r & 1 != 0 {
            return None;
        }
        Some(r)
    }

    /// Run a single thread to completion.
    pub fn run(
        &mut self,
        m: &AddressSpace,
        ram: &mut [u8],
        uniforms: &[u32],
        budget: usize,
    ) -> Result<(), Error> {
        let mut code = Code::default();
        let mut tlb = Tlb::default();
        let mut textures = crate::gpu_texture::Cache::default();
        let mut env = Env::new(m, ram, uniforms, &mut code, &mut tlb, &mut textures);
        self.run_env(&mut env, budget)
    }
    pub fn run_env(&mut self, env: &mut Env, budget: usize) -> Result<(), Error> {
        self.done = false;
        for _ in 0..budget {
            if self.done {
                return Ok(());
            }
            let i = env
                .code
                .fetch(env.m, env.ram, self.pc)
                .map_err(Error::Memory)?;
            self.step(env, i, None)?;
        }
        if self.done {
            Ok(())
        } else {
            Err(Error::Budget)
        }
    }

    /// Execute one instruction. `quad` gives the other lanes' registers, for cross-lane
    /// permutes and texture derivatives.
    fn step(&mut self, env: &mut Env, i: u64, quad: Option<&QuadView>) -> Result<(), Error> {
        self.instructions += 1;
        let flow = bits(i, 59, 4);
        if i >> 63 != 0 || flow > 10 && flow != 15 && !(flow == 13 && self.fragment.is_some()) {
            return Err(self.unsupported(i));
        }
        let next = match self.execute(env, i, quad) {
            Ok(n) => n,
            Err(Some(e)) => return Err(e),
            Err(None) => return Err(self.unsupported(i)),
        };
        match next {
            Next::Continue => self.pc = self.pc.checked_add(8).ok_or(self.unsupported(i))?,
            Next::Jump(pc) => self.pc = pc,
            Next::End => {
                self.done = true;
                return Ok(());
            }
        }
        if flow == 15 || flow == 13 && self.fragment.as_ref().is_some_and(|f| !f.active) {
            self.done = true;
        }
        Ok(())
    }

    /// `Err(None)` is an unsupported encoding, `Err(Some(e))` a fault.
    fn execute(&mut self, env: &mut Env, i: u64, quad: Option<&QuadView>) -> R<Next> {
        let op = bits(i, 48, 9);
        let u = env.uniforms;
        let o = Ops {
            s: self,
            i,
            uniforms: u,
        };
        let sub = bits(i, 16, 4);
        let v: u32 = match op {
            0x000 => return Ok(Next::Continue),
            0x01f => {
                // BRANCHZ: compare a source to zero, offset in instructions.
                let v = self.combine(i, o.raw(0)?)?;
                if (v == 0) == (i & (1 << 36) != 0) {
                    let off = sext(bits(i, 8, 27), 27) as i64;
                    return Ok(Next::Jump(
                        self.pc.wrapping_add(8).wrapping_add_signed(off * 8),
                    ));
                }
                return Ok(Next::Continue);
            }
            0x020 => {
                // DISCARD.f32
                self.fragment.as_ref().ok_or(None)?;
                if fcmp(bits(i, 32, 3), o.f32(0)?, o.f32(1)?).ok_or(None)? {
                    let f = self.fragment.as_mut().unwrap();
                    f.active = false;
                    return Ok(Next::End);
                }
                return Ok(Next::Continue);
            }
            0x02f => {
                // BRANCHZI: indirect branch, used for blend shader calls and returns.
                let v = self.combine(i, o.raw(0)?)?;
                let target = o.raw(1)?;
                if (v == 0) == (i & (1 << 36) != 0) {
                    if i & (1 << 40) != 0 {
                        if target == 0 {
                            return Ok(Next::End);
                        }
                        return Ok(Next::Jump((self.pc & !0xffff_ffff) | target as u64));
                    }
                    return Ok(Next::Jump(
                        self.pc
                            .wrapping_add(8)
                            .wrapping_add_signed(target as i32 as i64),
                    ));
                }
                return Ok(Next::Continue);
            }
            0x045 => return Ok(Next::Continue), // BARRIER: threads run in order
            0x056 => return self.ld_var_special(i),
            0x05c | 0x05d | 0x06c | 0x06d => return self.ld_var_buf(i, u),
            0x05e => return self.lea_buf_imm(env, i),
            0x060 => return self.load(env, i),
            0x061 => return self.store(env, i),
            0x066 | 0x076 => return self.ld_attr(env, i),
            0x067 | 0x077 => return self.lea_attr(env, i),
            0x068 | 0x069 | 0x120 => return self.atomic(env, i),
            0x06a => return self.ld_buffer(env, i),
            0x06b => return self.lea_buffer(env, i),
            0x07d => {
                // ATEST: alpha-to-coverage is not modelled; coverage passes through.
                self.fragment.as_ref().ok_or(None)?;
                let r = self.staging(i, 1).ok_or(None)?;
                let coverage = o.raw(0)?;
                o.raw(1)?;
                // Only the disabled datum (no alpha-to-coverage) is supported.
                if o.raw(2)? != 0 {
                    return Err(None);
                }
                self.regs[r] = coverage;
                self.fragment.as_mut().unwrap().active = coverage & 1 != 0;
                return Ok(Next::Continue);
            }
            0x07e => {
                // ZS_EMIT
                self.fragment.as_ref().ok_or(None)?;
                let r = self.staging(i, 1).ok_or(None)?;
                let (z, s, coverage) = (o.raw(0)?, o.raw(1)?, o.raw(2)?);
                let f = self.fragment.as_mut().unwrap();
                if i & (1 << 25) != 0 {
                    f.depth = Some(f32::from_bits(z));
                }
                if i & (1 << 24) != 0 {
                    f.stencil = Some(s & 255);
                }
                self.regs[r] = coverage;
                return Ok(Next::Continue);
            }
            0x07f => return self.blend(i, u),
            0x090 => self.convert(i, &o)?,
            0x091 => {
                let v = o.raw(0)?;
                match sub {
                    0 => v,
                    4 => v.leading_zeros(),
                    5 => {
                        let h = o.swizzle16(0)?;
                        pack2(h[0].leading_zeros() as u32, h[1].leading_zeros() as u32)
                    }
                    6 => u32::from_le_bytes(v.to_le_bytes().map(|b| b.leading_zeros() as u8)),
                    8 => (o.i32w(0, true)? as i32).unsigned_abs(),
                    9 => {
                        let h = o.v2i16w(0, true)?;
                        pack2(
                            (h[0] as u16 as i16).unsigned_abs() as u32,
                            (h[1] as u16 as i16).unsigned_abs() as u32,
                        )
                    }
                    10 => u32::from_le_bytes(v.to_le_bytes().map(|b| (b as i8).unsigned_abs())),
                    12 => v.count_ones(),
                    13 => v.reverse_bits(),
                    14 => !v,
                    _ => return Err(None),
                }
            }
            0x095 => 1, // WMASK: subgroups are single threads here
            0x098 => match sub {
                // FLUSH: flush subnormals/NaN as requested; other values pass through.
                0 => {
                    let mut x = o.f32(0)?;
                    if i & (1 << 10) != 0 && x.is_subnormal() {
                        x = 0f32.copysign(x);
                    }
                    if bits(i, 8, 2) == 1 && x.is_nan() {
                        x = 0.;
                    }
                    x.to_bits()
                }
                1 => f2h(o.v2f16(0)?),
                _ => return Err(None),
            },
            0x099 => {
                let (sqrt, log) = (i & (1 << 24) != 0, i & (1 << 25) != 0);
                match sub {
                    0 => frexp(o.f32(0)?, sqrt, log).0.to_bits(),
                    1 => f2h(o.v2f16(0)?.map(|x| frexp(x, sqrt, log).0)),
                    2 => frexp(o.f32(0)?, sqrt, log).1 as u32,
                    3 => {
                        let v = o.v2f16(0)?;
                        pack2(
                            frexp(v[0], sqrt, log).1 as u32,
                            frexp(v[1], sqrt, log).1 as u32,
                        )
                    }
                    _ => return Err(None),
                }
            }
            0x09c => self.sfu(i, &o)?,
            0x0a0 | 0x0a1 | 0x0a2 | 0x0a8 | 0x0a9 | 0x0aa if sub == 0xf => {
                self.clper(i, &o, quad)?
            }
            0x0a0 | 0x0a8 => {
                let signed = op == 0x0a8;
                let (a, b) = (o.i32w(0, signed)?, o.i32w(1, signed)?);
                let sat = i & (1 << 30) != 0;
                match (sub, sat, signed) {
                    (0, false, _) => a.wrapping_add(b),
                    (0, true, false) => a.saturating_add(b),
                    (0, true, true) => (a as i32).saturating_add(b as i32) as u32,
                    (1, false, _) => a.wrapping_sub(b),
                    (1, true, false) => a.saturating_sub(b),
                    (1, true, true) => (a as i32).saturating_sub(b as i32) as u32,
                    (0xa, _, _) => a.wrapping_mul(b),
                    (0xb, r, true) => ((a as i32 as i64 + b as i32 as i64 + r as i64) >> 1) as u32,
                    (0xb, r, false) => ((a as u64 + b as u64 + r as u64) >> 1) as u32,
                    _ => return Err(None),
                }
            }
            0x0a1 | 0x0a9 if sub == 5 => {
                // MKVEC.v2i16
                pack2(o.swizzle16(0)?[0] as u32, o.swizzle16(1)?[0] as u32)
            }
            0x0a1 | 0x0a9 => {
                let signed = op == 0x0a9;
                let (a, b) = (o.v2i16w(0, signed)?, o.v2i16w(1, signed)?);
                let r = (i >> 30) & 1 != 0;
                let lane = |x: u32, y: u32| -> Option<u32> {
                    let (sx, sy) = (x as u16 as i16 as i32, y as u16 as i16 as i32);
                    Some(
                        match (sub, r, signed) {
                            (0, false, _) => x.wrapping_add(y),
                            (0, true, true) => (sx + sy).clamp(-32768, 32767) as u32,
                            (0, true, false) => (x + y).min(65535),
                            (1, false, _) => x.wrapping_sub(y),
                            (1, true, true) => (sx - sy).clamp(-32768, 32767) as u32,
                            (1, true, false) => x.saturating_sub(y),
                            (0xa, _, _) => x.wrapping_mul(y),
                            (0xb, r, true) => ((sx + sy + r as i32) >> 1) as u32,
                            (0xb, r, false) => (x + y + r as u32) >> 1,
                            _ => return None,
                        } & 0xffff,
                    )
                };
                pack2(lane(a[0], b[0]).ok_or(None)?, lane(a[1], b[1]).ok_or(None)?)
            }
            0x0a2 | 0x0aa => {
                let signed = op == 0x0aa;
                let (a, b) = (o.v4i8w(0)?, o.v4i8w(1)?);
                let r = (i >> 30) & 1 != 0;
                let mut out = 0;
                for k in 0..4 {
                    let (x, y) = (a[k], b[k]);
                    let (sx, sy) = (x as u8 as i8 as i32, y as u8 as i8 as i32);
                    let v = match (sub, r, signed) {
                        (0, false, _) => x.wrapping_add(y),
                        (0, true, true) => (sx + sy).clamp(-128, 127) as u32,
                        (0, true, false) => (x + y).min(255),
                        (1, false, _) => x.wrapping_sub(y),
                        (1, true, true) => (sx - sy).clamp(-128, 127) as u32,
                        (1, true, false) => x.saturating_sub(y),
                        (0xa, _, _) => x.wrapping_mul(y),
                        (0xb, r, true) => ((sx + sy + r as i32) >> 1) as u32,
                        (0xb, r, false) => (x + y + r as u32) >> 1,
                        _ => return Err(None),
                    } & 255;
                    out |= v << (8 * k);
                }
                out
            }
            0x0a4 => {
                let cl = bits(i, 32, 2);
                match sub {
                    0 => clamp(cl, o.f32(0)? + o.f32(1)?).to_bits(),
                    2 => clamp(cl, fmin(o.f32(0)?, o.f32(1)?)).to_bits(),
                    3 => clamp(cl, fmax(o.f32(0)?, o.f32(1)?)).to_bits(),
                    6 => ldexp(o.f32_plain(0)?, o.raw(1)? as i32).to_bits(),
                    8 => {
                        // FEXP: 2^x from 8:24 fixed point; the float source handles specials.
                        let fixed = o.raw(0)? as i32;
                        let f = o.f32_plain(1)?;
                        let v = if f.is_nan() {
                            f
                        } else if f > 2f32.powi(31) {
                            f32::INFINITY
                        } else if f < -(2f32.powi(31)) {
                            0.
                        } else {
                            2f64.powf(fixed as f64 / 16777216.) as f32
                        };
                        clamp(cl, v).to_bits()
                    }
                    // FADD_LSCALE: A plus the log-reduced mantissa of B (see FLOGD).
                    9 => {
                        clamp(cl, o.f32_plain(0)? + frexp(o.f32_plain(1)?, false, true).0).to_bits()
                    }
                    _ => return Err(None),
                }
            }
            0x0a5 => {
                let cl = bits(i, 32, 2);
                let c16 = |v: [f32; 2]| f2h(v.map(|x| clamp(cl, x)));
                match sub {
                    0 => {
                        let (a, b) = (o.v2f16(0)?, o.v2f16(1)?);
                        c16([a[0] + b[0], a[1] + b[1]])
                    }
                    2 | 3 => {
                        let (a, b) = (o.v2f16(0)?, o.v2f16(1)?);
                        let f = if sub == 2 { fmin } else { fmax };
                        c16([f(a[0], b[0]), f(a[1], b[1])])
                    }
                    // V2F32_TO_V2F16
                    4 => c16([o.f32_plain(0)?, o.f32_plain(1)?]),
                    6 => {
                        let a = o.v2f16(0)?;
                        let e = o.raw(1)?;
                        f2h([
                            ldexp(a[0], e as u16 as i16 as i32),
                            ldexp(a[1], (e >> 16) as u16 as i16 as i32),
                        ])
                    }
                    _ => return Err(None),
                }
            }
            0x0b2 => clamp(bits(i, 32, 2), o.f32(0)?.mul_add(o.f32(1)?, o.f32(2)?)).to_bits(),
            0x0b3 => {
                let (a, b, c) = (o.v2f16(0)?, o.v2f16(1)?, o.v2f16(2)?);
                let cl = bits(i, 32, 2);
                f2h([
                    clamp(cl, a[0].mul_add(b[0], c[0])),
                    clamp(cl, a[1].mul_add(b[1], c[1])),
                ])
            }
            0x0b4..=0x0b6 => self.shift(i, &o, op)?,
            0x0b8..=0x0ba => {
                let mode = bits(i, 32, 2);
                let (a, b, mask) = if op == 0x0b9 {
                    let s = |n| o.swizzle16(n).map(|v| pack2(v[0] as u32, v[1] as u32));
                    (s(0)?, s(1)?, s(2)?)
                } else {
                    (o.raw(0)?, o.raw(1)?, o.raw(2)?)
                };
                if mode == 3 {
                    (a & mask) | (b & !mask)
                } else {
                    let width = [32, 16, 8][(op - 0x0b8) as usize];
                    let lanes = 32 / width;
                    let m = if width == 32 {
                        u32::MAX
                    } else {
                        (1 << width) - 1
                    };
                    let mut out = 0;
                    for k in 0..lanes {
                        let s = k * width;
                        let c = (mask >> s) & m;
                        let take_a = match mode {
                            0 => c >> (width - 1) != 0,
                            1 => c == 0,
                            _ => match width {
                                32 => f32::from_bits(c) == 0.,
                                16 => half(c as u16) == 0.,
                                _ => c & 0x7f == 0,
                            },
                        };
                        out |= (if take_a { a >> s } else { b >> s } & m) << s;
                    }
                    out
                }
            }
            0x0bc => {
                // CUBE_SSEL/CUBE_TSEL: pick the s or t coordinate of the selected face.
                let (x, y, face) = (o.f32_plain(0)?, o.f32_plain(1)?, o.raw(2)?);
                msg::cube_select(sub, x, y, face).ok_or(None)?
            }
            0x0bd => {
                // MKVEC.v2i8: bytes of A and B (lanes at 38/36), high half from C.
                let a = (o.raw(0)? >> (8 * bits(i, 38, 2))) & 255;
                let b = (o.raw(1)? >> (8 * bits(i, 36, 2))) & 255;
                a | (b << 8) | (o.raw(2)? & 0xffff_0000)
            }
            0x0c0 | 0x0c1 => {
                let (x, y, z) = (o.f32_plain(0)?, o.f32_plain(1)?, o.f32_plain(2)?);
                msg::cube_face(op == 0x0c1, x, y, z)
            }
            0x0c2 => {
                let (a, b, c) = (o.raw(0)?.to_le_bytes(), o.raw(1)?.to_le_bytes(), o.raw(2)?);
                let signed = bits(i, 24, 4) == 0;
                let mut sum = c as i64;
                for k in 0..4 {
                    sum += if signed {
                        a[k] as i8 as i64 * b[k] as i8 as i64
                    } else {
                        a[k] as i64 * b[k] as i64
                    };
                }
                sum as u32
            }
            0x0f0..=0x0fa => self.compare(i, &o, op)?,
            0x110 => o.raw(0)?.wrapping_add(bits(i, 8, 32)),
            0x111 => {
                let (a, c) = (o.raw(0)?, bits(i, 8, 32));
                pack2(a.wrapping_add(c), (a >> 16).wrapping_add(c >> 16))
            }
            0x112 => {
                let (a, c) = (o.raw(0)?.to_le_bytes(), bits(i, 8, 32).to_le_bytes());
                u32::from_le_bytes(std::array::from_fn(|k| a[k].wrapping_add(c[k])))
            }
            0x114 => (f32::from_bits(o.raw(0)?) + f32::from_bits(bits(i, 8, 32))).to_bits(),
            0x115 => {
                let (a, c) = (o.raw(0)?, bits(i, 8, 32));
                f2h([
                    half(a as u16) + half(c as u16),
                    half((a >> 16) as u16) + half((c >> 16) as u16),
                ])
            }
            0x125 | 0x128 | 0x129 | 0x12a | 0x138 => return self.texture(env, i, quad),
            0x150..=0x159 => {
                let (a, b, t, f) = (o.raw(0)?, o.raw(1)?, o.raw(2)?, o.raw(3)?);
                let cond = bits(i, 32, 3);
                let pick = |x: u32, y: u32| -> Option<bool> {
                    match op {
                        0x150 => icmp(cond, x, y),
                        0x154 => fcmp(cond, f32::from_bits(x), f32::from_bits(y)),
                        0x158 => icmp(cond, x as i32, y as i32),
                        0x151 => icmp(cond, x & 0xffff, y & 0xffff),
                        0x155 => fcmp(cond, half(x as u16), half(y as u16)),
                        0x159 => icmp(cond, x as u16 as i16, y as u16 as i16),
                        _ => None,
                    }
                };
                if matches!(op, 0x150 | 0x154 | 0x158) {
                    if pick(a, b).ok_or(None)? {
                        t
                    } else {
                        f
                    }
                } else {
                    let lo = if pick(a, b).ok_or(None)? { t } else { f } & 0xffff;
                    let hi = if pick(a >> 16, b >> 16).ok_or(None)? {
                        t
                    } else {
                        f
                    } >> 16;
                    lo | (hi << 16)
                }
            }
            0x160..=0x163 => {
                let (a, b, c, d) = (o.f32_plain(0)?, o.f32_plain(1)?, o.f32_plain(2)?, o.raw(3)?);
                let v = match op {
                    0x160 => a.mul_add(b, c),
                    _ if a == 0. || b == 0. => (if op == 0x162 { a } else { 0. }) + c,
                    _ => a.mul_add(b, c),
                };
                let e = if op == 0x163 {
                    d as u16 as i16 as i32
                } else {
                    d as i32
                };
                clamp(bits(i, 32, 2), ldexp(v, e)).to_bits()
            }
            0x191 if sub == 0xe => {
                let a = o.s.source64(i, 0, u).ok_or(None)?;
                self.write_dest64(i, !a)?;
                return Ok(Next::Continue);
            }
            0x1a3 | 0x1ab => {
                let r = self.alu64(i, &o, op)?;
                self.write_dest64(i, r)?;
                return Ok(Next::Continue);
            }
            0x1b7 => {
                let r = self.shift64(i, &o)?;
                self.write_dest64(i, r)?;
                return Ok(Next::Continue);
            }
            _ => return Err(None),
        };
        self.write_dest(i, v)?;
        Ok(Next::Continue)
    }

    /// The branch source's combine mode (none, h0, h1, and, lowbits).
    fn combine(&self, i: u64, v: u32) -> R<u32> {
        Ok(match bits(i, 37, 3) {
            0 => v,
            1 => v & 0xffff,
            2 => v >> 16,
            3 => ((v & 0xffff != 0) && (v >> 16 != 0)) as u32,
            4 => v & 1,
            _ => return Err(None),
        })
    }

    fn convert(&self, i: u64, o: &Ops) -> R<u32> {
        let sub = bits(i, 16, 5);
        let rm = bits(i, 30, 2);
        let lane8 = |v: u32| (v >> (8 * bits(i, 28, 2))) & 255;
        let swz_lo = |o: &Ops| o.swizzle16(0).map(|h| h[0] as u32);
        Ok(match sub {
            0x00 => sext(lane8(o.raw(0)?), 8) as u32,
            0x01 => (sext(lane8(o.raw(0)?), 8) as f32).to_bits(),
            0x02 | 0x03 | 0x12 | 0x13 => {
                // V2x8_TO_V2x16: half-swizzle (bit 36) picks the two bytes.
                let v = o.raw(0)?;
                let hs = bits(i, 36, 4);
                let (b0, b1) = (hs & 3, hs >> 2);
                let get = |k: u32| (v >> (8 * k)) & 255;
                let signed = sub < 0x10;
                let conv = |x: u32| -> u32 {
                    match (sub & 1, signed) {
                        (0, true) => sext(x, 8) as u32 & 0xffff,
                        (0, false) => x,
                        (_, true) => pack_half(sext(x, 8) as f32) as u32,
                        (_, false) => pack_half(x as f32) as u32,
                    }
                };
                pack2(conv(get(b0)), conv(get(b1)))
            }
            0x04 => sext(swz_lo(o)?, 16) as u32,
            0x05 => (sext(swz_lo(o)?, 16) as f32).to_bits(),
            0x07 | 0x17 => {
                let h = o.v2i16w(0, sub == 0x07)?;
                let c = |x: u32| {
                    if sub == 0x07 {
                        x as u16 as i16 as f32
                    } else {
                        x as f32
                    }
                };
                f2h([round(rm, c(h[0])), round(rm, c(h[1]))])
            }
            0x09 => (o.i32w(0, true)? as i32 as f32).to_bits(),
            0x19 => (o.i32w(0, false)? as f32).to_bits(),
            0x0a | 0x1a => {
                let h = o.v2f16(0)?;
                f32_to_int(h[0], rm, sub == 0x0a)
            }
            0x0b => {
                // F16_TO_F32: lane at bit 28 (h0, h1), abs/neg.
                let v = o.raw(0)?;
                let h = if i & (1 << 28) != 0 { v >> 16 } else { v } as u16;
                o.absneg(0, half(h)).to_bits()
            }
            0x0c | 0x1c => f32_to_int(o.f32_plain(0)?, rm, sub == 0x0c),
            0x0d => round(rm, o.f32(0)?).to_bits(),
            0x0e | 0x1e => {
                let h = o.v2f16(0)?;
                let c = |x: f32| {
                    let r = round(rm, x);
                    if sub == 0x0e {
                        r.clamp(-32768., 32767.) as i32 as u32 & 0xffff
                    } else {
                        r.clamp(0., 65535.) as u32
                    }
                };
                pack2(c(h[0]), c(h[1]))
            }
            0x0f => f2h(o.v2f16(0)?.map(|x| round(rm, x))),
            0x10 => lane8(o.raw(0)?),
            0x11 => (lane8(o.raw(0)?) as f32).to_bits(),
            0x14 => swz_lo(o)?,
            0x15 => (swz_lo(o)? as f32).to_bits(),
            _ => return Err(None),
        })
    }

    fn sfu(&self, i: u64, o: &Ops) -> R<u32> {
        let sub = bits(i, 16, 4);
        let f16 = |f: fn(f32) -> f32| -> R<u32> {
            let h = o.v2f16(0)?;
            Ok(f2h([f(h[0]), f(h[1])]))
        };
        Ok(match sub {
            0 | 14 => (1. / o.f32(0)?).to_bits(),
            1 => f16(|x| 1. / x)?,
            2 | 15 => (1. / o.f32(0)?.sqrt()).to_bits(),
            3 => f16(|x| 1. / x.sqrt())?,
            4 | 5 | 6 => {
                // Coarse tables: the low 6 bits of the source index multiples of pi/32.
                let k = o.raw(0)? & 63;
                let a = k as f64 * std::f64::consts::PI / 32.;
                match sub {
                    4 => (a.sin() as f32).to_bits(),
                    5 => (a.cos() as f32).to_bits(),
                    _ => 0,
                }
            }
            8 => {
                // FLOGD: log2(m)/(m - 1) of the log-reduced mantissa m, so that
                // FLOGD(x) * FADD_LSCALE(-1, x) + exponent = log2(x).
                let x = o.f32(0)?;
                if x < 0. || x.is_nan() {
                    f32::NAN.to_bits()
                } else if x == 0. {
                    f32::NEG_INFINITY.to_bits()
                } else if x.is_infinite() {
                    f32::INFINITY.to_bits()
                } else {
                    let m = frexp(x, false, true).0 as f64;
                    let r = if (m - 1.).abs() < 1e-12 {
                        std::f64::consts::LOG2_E
                    } else {
                        m.log2() / (m - 1.)
                    };
                    (r as f32).to_bits()
                }
            }
            10 | 11 => {
                let x = if sub == 10 { o.f32(0)? } else { o.v2f16(0)?[0] };
                let class = if x.is_nan() {
                    1
                } else if x.is_infinite() {
                    if x < 0. {
                        2
                    } else {
                        512
                    }
                } else if x == 0. {
                    if x.is_sign_negative() {
                        16
                    } else {
                        32
                    }
                } else if x.is_subnormal() {
                    if x < 0. {
                        8
                    } else {
                        64
                    }
                } else if x < 0. {
                    4
                } else {
                    128
                };
                class
            }
            12 => {
                let x = o.f32(0)?;
                (-(frexp(x, false, true).0 as f64).log2() as f32).to_bits()
            }
            13 => {
                let k = o.raw(0)? & 15;
                (2f64.powf(k as f64 / 16.) as f32).to_bits()
            }
            _ => return Err(None),
        })
    }

    fn shift(&self, i: u64, o: &Ops, op: u32) -> R<u32> {
        let kind = bits(i, 24, 2);
        let left = i & (1 << 32) != 0;
        let signed = i & (1 << 34) != 0;
        let field = bits(i, 26, 4);
        if !(4..=7).contains(&field) {
            return Err(None);
        }
        let amount = (o.raw(1)? >> (8 * (field - 4))) & 255;
        let mut c = o.raw(2)?;
        if i & (1 << 35) != 0 {
            c = !c;
        }
        let x = match op {
            0x0b4 => {
                let a = o.i32w(0, signed)?;
                let s = amount & 31;
                if left {
                    a << s
                } else if signed {
                    ((a as i32) >> s) as u32
                } else {
                    a >> s
                }
            }
            0x0b5 => {
                let a = o.v2i16w(0, signed)?;
                let s = amount & 15;
                let sh = |x: u32| {
                    (if left {
                        x << s
                    } else if signed {
                        ((x as u16 as i16 as i32) >> s) as u32
                    } else {
                        (x & 0xffff) >> s
                    }) & 0xffff
                };
                pack2(sh(a[0]), sh(a[1]))
            }
            _ => {
                let a = o.v4i8w(0)?;
                let s = amount & 7;
                let mut x = 0;
                for (k, &v) in a.iter().enumerate() {
                    let r = (if left {
                        v << s
                    } else if signed {
                        ((v as u8 as i8 as i32) >> s) as u32
                    } else {
                        v >> s
                    }) & 255;
                    x |= r << (8 * k);
                }
                x
            }
        };
        let r = match kind {
            0 => x & c,
            1 => x | c,
            2 => x ^ c,
            _ => return Err(None),
        };
        Ok(if i & (1 << 30) != 0 { !r } else { r })
    }

    fn shift64(&self, i: u64, o: &Ops) -> R<u64> {
        let kind = bits(i, 24, 2);
        let left = i & (1 << 32) != 0;
        let signed = i & (1 << 34) != 0;
        let field = bits(i, 26, 4);
        if !(4..=7).contains(&field) {
            return Err(None);
        }
        let amount = (o.raw(1)? >> (8 * (field - 4))) & 63;
        let a = self.source64(i, 0, o.uniforms).ok_or(None)?;
        let mut c = self.source64(i, 2, o.uniforms).ok_or(None)?;
        if i & (1 << 35) != 0 {
            c = !c;
        }
        let x = if left {
            a << amount
        } else if signed {
            ((a as i64) >> amount) as u64
        } else {
            a >> amount
        };
        let r = match kind {
            0 => x & c,
            1 => x | c,
            2 => x ^ c,
            _ => return Err(None),
        };
        Ok(if i & (1 << 30) != 0 { !r } else { r })
    }

    fn alu64(&self, i: u64, o: &Ops, op: u32) -> R<u64> {
        let sub = bits(i, 16, 4);
        let signed = op == 0x1ab;
        let a = self.source64(i, 0, o.uniforms).ok_or(None)?;
        // Source 1 is a 64-bit pair, or a widened 32-bit value (Swizzles 64-bit: w0).
        let b = if bits(i, 26, 4) == 8 {
            let v = o.raw(1)?;
            if signed {
                v as i32 as i64 as u64
            } else {
                v as u64
            }
        } else if bits(i, 26, 4) == 0 {
            self.source64(i, 1, o.uniforms).ok_or(None)?
        } else {
            return Err(None);
        };
        Ok(match sub {
            0 => a.wrapping_add(b),
            1 => a.wrapping_sub(b),
            6 => {
                // SEG_ADD: segment base plus offset; preserve_null keeps a null pointer.
                if i & (1 << 39) != 0 && b == 0 {
                    0
                } else if i & (1 << 38) != 0 {
                    a.wrapping_sub(b)
                } else {
                    a.wrapping_add(b)
                }
            }
            7 => a.wrapping_add(b << bits(i, 20, 3)), // SHADDX
            0xa => a.wrapping_mul(b),
            _ => return Err(None),
        })
    }

    fn compare(&self, i: u64, o: &Ops, op: u32) -> R<u32> {
        let combine = bits(i, 24, 2);
        let cond = bits(i, 32, 3);
        let kind = bits(i, 30, 2);
        let c = o.raw(2)?;
        let merge = |r: bool, other: bool, equal: bool| -> Option<bool> {
            match combine {
                0 => Some(r || other),
                1 => Some(r && other),
                // MULTI: the high halves decide unless they are equal.
                2 => Some(if equal { other } else { r }),
                _ => None,
            }
        };
        match op {
            0x0f0 | 0x0f8 | 0x0f4 => {
                let (r, eq) = match op {
                    0x0f4 => {
                        let (x, y) = (o.f32(0)?, o.f32(1)?);
                        (fcmp(cond, x, y).ok_or(None)?, x == y)
                    }
                    0x0f0 => {
                        let (x, y) = (o.i32w(0, false)?, o.i32w(1, false)?);
                        (icmp(cond, x, y).ok_or(None)?, x == y)
                    }
                    _ => {
                        let (x, y) = (o.i32w(0, true)? as i32, o.i32w(1, true)? as i32);
                        (icmp(cond, x, y).ok_or(None)?, x == y)
                    }
                };
                if op == 0x0f4 && combine == 2 {
                    return Err(None);
                }
                Ok(result_bool(kind, merge(r, c != 0, eq).ok_or(None)?))
            }
            0x0f1 | 0x0f9 | 0x0f5 => {
                let mut out = 0;
                for k in 0..2 {
                    let r = match op {
                        0x0f5 => {
                            let (x, y) = (o.v2f16(0)?, o.v2f16(1)?);
                            fcmp(cond, x[k], y[k]).ok_or(None)?
                        }
                        0x0f1 => {
                            let (x, y) = (o.v2i16w(0, false)?, o.v2i16w(1, false)?);
                            icmp(cond, x[k], y[k]).ok_or(None)?
                        }
                        _ => {
                            let (x, y) = (o.v2i16w(0, true)?, o.v2i16w(1, true)?);
                            icmp(cond, x[k] as u16 as i16, y[k] as u16 as i16).ok_or(None)?
                        }
                    };
                    let other = (c >> (16 * k)) & 0xffff != 0;
                    out |= result_bool16(kind, merge(r, other, false).ok_or(None)?) << (16 * k);
                }
                Ok(out)
            }
            0x0f2 | 0x0fa => {
                let (x, y) = (o.v4i8w(0)?, o.v4i8w(1)?);
                let mut out = 0;
                for k in 0..4 {
                    let r = if op == 0x0fa {
                        icmp(cond, x[k] as u8 as i8, y[k] as u8 as i8)
                    } else {
                        icmp(cond, x[k], y[k])
                    }
                    .ok_or(None)?;
                    let other = (c >> (8 * k)) & 255 != 0;
                    let v = merge(r, other, false).ok_or(None)?;
                    out |= (if v {
                        if kind == 0 {
                            1
                        } else {
                            255
                        }
                    } else {
                        0
                    }) << (8 * k);
                }
                Ok(out)
            }
            _ => Err(None),
        }
    }

    /// CLPER: read source A in another lane of the quad.
    fn clper(&self, i: u64, o: &Ops, quad: Option<&QuadView>) -> R<u32> {
        let a_index = bits(i, 0, 8) as usize;
        let lane_sel = o.raw(1)?;
        let field = bits(i, 26, 4);
        let b = if (4..=7).contains(&field) {
            (lane_sel >> (8 * (field - 4))) & 255
        } else {
            lane_sel & 255
        };
        let inactive = [
            0,
            0xffff_ffff,
            1,
            0x0001_0001,
            0x8000_0000,
            0x7fff_ffff,
            0x8000_8000,
            0x7fff_7fff,
            0x8080_8080,
            0x7f7f_7f7f,
            0x3f80_0000,
            0x3c00_3c00,
            0xff80_0000,
            0x7f80_0000,
            0xfc00_fc00,
            0x7c00_7c00,
        ][bits(i, 22, 4) as usize];
        let Some(q) = quad else {
            // A lone thread is lane 0 of its subgroup; other lanes are inactive.
            return if b == 0 { o.raw(0) } else { Ok(inactive) };
        };
        let me = q.lane as u32;
        let target = match bits(i, 32, 2) {
            0 => b,
            1 => me ^ b,
            2 => me.wrapping_add(b),
            _ => me.wrapping_sub(b),
        } as usize;
        if target >= 4 || !q.active[target] {
            return Ok(inactive);
        }
        if a_index >> 6 <= 1 {
            let r = a_index & 63;
            if !self.registers_available(r, 1) {
                return Err(None);
            }
            Ok(q.regs[target][r])
        } else {
            o.raw(0)
        }
    }
}

/// Run the four threads of a 2x2 quad together (lane = x + 2y). The lowest program
/// counter executes first, so threads that branch apart reconverge, and cross-lane
/// instructions see their neighbours at the same point.
pub fn run_quad(lanes: &mut [Shader; 4], env: &mut Env, budget: usize) -> Result<(), Error> {
    let mut spent = 0;
    loop {
        let mut pc = u64::MAX;
        for l in lanes.iter() {
            if !l.done {
                pc = pc.min(l.pc);
            }
        }
        if pc == u64::MAX {
            return Ok(());
        }
        if spent >= budget {
            return Err(Error::Budget);
        }
        let i = env.code.fetch(env.m, env.ram, pc).map_err(Error::Memory)?;
        let active: [bool; 4] = std::array::from_fn(|n| !lanes[n].done && lanes[n].pc == pc);
        let cross = msg::needs_quad(i);
        let mut view = cross.then(|| QuadView {
            regs: std::array::from_fn(|n| lanes[n].regs),
            active,
            lane: 0,
            tex: msg::quad_coords(lanes, &active, i, env.uniforms),
        });
        for n in 0..4 {
            if active[n] {
                if let Some(v) = view.as_mut() {
                    v.lane = n;
                }
                lanes[n].step(env, i, view.as_ref())?;
                spent += 1;
            }
        }
    }
}

#[cfg(test)]
mod tests;
