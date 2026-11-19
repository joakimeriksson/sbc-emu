//! Initial scalar Valhall shader execution. Encodings follow Mesa's Valhall ISA.xml.
//! Only explicitly decoded instructions are supported. Unknown opcodes,
//! unsupported modifiers and exhausted budgets fault rather than complete work.
use crate::gpu_mmu::{AddressSpace, Fault};
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
/// Single-sample fragment output. Created only after fixed-function
/// blend/depth state has been validated by the rasterizer.
#[derive(Clone, Debug)]
pub struct Fragment {
    pub blend: [u32; 2],
    pub color: Option<[f32; 4]>,
    pub active: bool,
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
/// Hardware interpolation buffer, populated from native varying-shader stores.
#[derive(Clone, Debug)]
pub struct Interpolation {
    pub data: std::sync::Arc<[u8]>,
    pub stride: usize,
    pub vertices: [usize; 3],
    pub weights: [f64; 3],
    pub saved: bool,
}
impl Interpolation {
    fn load(&self, offset: usize, components: usize, width: usize) -> Option<[f32; 4]> {
        if offset % width != 0 || offset.checked_add(components * width)? > self.stride {
            return None;
        }
        let mut out = [0.; 4];
        for (n, out) in out.iter_mut().enumerate().take(components) {
            let mut value = 0.;
            for v in 0..3 {
                let at = self.vertices[v]
                    .checked_mul(self.stride)?
                    .checked_add(offset + n * width)?;
                let bytes = self.data.get(at..at.checked_add(width)?)?;
                let source = if width == 4 {
                    f32::from_le_bytes(bytes.try_into().ok()?)
                } else {
                    half(u16::from_le_bytes(bytes.try_into().ok()?))
                };
                if !source.is_finite() || !self.weights[v].is_finite() {
                    return None;
                }
                value += source as f64 * self.weights[v];
            }
            *out = value as f32;
            if !out.is_finite() {
                return None;
            }
        }
        Some(out)
    }
}
pub struct Shader {
    pub regs: [u32; 64],
    pub resources: crate::gpu_resources::Resources,
    pub position_output: Option<crate::gpu_resources::Buffer>,
    pub fragment: Option<Fragment>,
    pub interpolation: Option<Interpolation>,
    register_mask: u64,
    pub pc: u64,
    pub instructions: u64,
}
const LUT: [u32; 32] = [
    0, 0xffffffff, 0x7fffffff, 0xfafcfdfe, 0x01000000, 0x80002000, 0x70605030, 0xc0b0a090,
    0x03020100, 0x07060504, 0x0b0a0908, 0x0f0e0d0c, 0x13121110, 0x17161514, 0x1b1a1918, 0x1f1e1d1c,
    0x3f800000, 0x3dcccccd, 0x3ea2f983, 0x3f317218, 0x40490fdb, 0, 0x477fff00, 0x5c005bf8,
    0x2e660000, 0x34000000, 0x38000000, 0x3c000000, 0x40000000, 0x44000000, 0x48000000, 0x42480000,
];
impl Shader {
    pub fn new(pc: u64) -> Self {
        Self::with_allocation(pc, RegisterAllocation::Registers64)
    }
    pub fn with_allocation(pc: u64, allocation: RegisterAllocation) -> Self {
        Self {
            regs: [0; 64],
            resources: crate::gpu_resources::Resources::default(),
            position_output: None,
            fragment: None,
            interpolation: None,
            pc,
            instructions: 0,
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
    fn source(&self, i: u64, index: usize, uniforms: &[u32]) -> Option<u32> {
        let s = ((i >> (index * 8)) & 255) as usize;
        match s >> 6 {
            0 | 1 => self
                .regs
                .get(s & 63)
                .filter(|_| self.registers_available(s & 63, 1))
                .copied(), // discard register hint doesn't change value
            2 => uniforms
                .get((((i >> 57) & 3) as usize) * 64 + (s & 63))
                .copied(),
            3 => LUT.get(s & 63).copied().or_else(|| {
                if (i >> 57) & 3 != 0 {
                    return None;
                }
                let fragment = self.fragment.as_ref()?;
                match s & 63 {
                    42 | 43 => Some(0),
                    48 => Some(fragment.blend[0]),
                    49 => Some(fragment.blend[1]),
                    _ => None,
                }
            }),
            _ => None,
        }
    }
    pub fn run(
        &mut self,
        m: &AddressSpace,
        ram: &mut [u8],
        uniforms: &[u32],
        budget: usize,
    ) -> Result<(), Error> {
        for _ in 0..budget {
            self.instructions += 1;
            let mut b = [0; 8];
            m.read(ram, self.pc, &mut b, true).map_err(Error::Memory)?;
            let i = u64::from_le_bytes(b);
            let op = (i >> 48) & 511;
            let bad = || Error::Unsupported {
                pc: self.pc,
                instruction: i,
            };
            let flow = (i >> 59) & 15;
            if i >> 63 != 0 || flow > 10 && flow != 15 && !(flow == 13 && self.fragment.is_some()) {
                return Err(bad());
            }
            let dst = ((i >> 40) & 63) as usize;
            let mut next = self.pc.checked_add(8).ok_or_else(bad)?;
            let a = self.source(i, 0, uniforms);
            let c = self.source(i, 1, uniforms);
            let result = match op {
                0 => None,
                0x91 if (i >> 16) & 31 == 0 && i & 0x000000ff_ffff_ff00 == 0 => {
                    Some(a.ok_or_else(bad)?)
                }
                0xa0 if (i >> 16) & 15 == 0 && i & 0x000000ff_ffff_0000 == 0 => {
                    Some(a.ok_or_else(bad)?.wrapping_add(c.ok_or_else(bad)?))
                }
                0x90 if (i >> 16) & 31 == 0x19 && i & 0x000000ff_ffff_ff00 == 0x190000 => {
                    Some((a.ok_or_else(bad)? as f32).to_bits())
                }
                0x90 if (i >> 16) & 31 == 0xb => {
                    let allowed = 255 | (1 << 28) | (3 << 30) | (3 << 38);
                    if i & 0x000000ff_ffffffff & !allowed != 0xb0000 {
                        return Err(bad());
                    }
                    let mut v = half((a.ok_or_else(bad)? >> (((i >> 28) & 1) * 16)) as u16);
                    if i & (1 << 39) != 0 {
                        v = v.abs();
                    }
                    if i & (1 << 38) != 0 {
                        v = -v;
                    }
                    Some(v.to_bits())
                }
                0xa4 | 0xb2 => {
                    // Scalar f32 with half-source widening, round-to-nearest and optional abs/neg.
                    let allowed = if op == 0xa4 {
                        0x000000fc_3c00ffff
                    } else {
                        0x000000fc_3fffffff
                    };
                    if i & 0x000000ff_ffffffff & !allowed != 0 {
                        return Err(bad());
                    }
                    let value = |n: usize| -> Result<f32, Error> {
                        let bits = self.source(i, n, uniforms).ok_or_else(bad)?;
                        let mut v = match (i >> (28 - n * 2)) & 3 {
                            0 => f32::from_bits(bits),
                            1 => half(bits as u16),
                            2 => half((bits >> 16) as u16),
                            _ => return Err(bad()),
                        };
                        let shift = 38 - 2 * n;
                        if i & (1 << (shift + 1)) != 0 {
                            v = v.abs();
                        }
                        if i & (1 << shift) != 0 {
                            v = -v;
                        }
                        Ok(v)
                    };
                    let x = value(0)?;
                    let y = value(1)?;
                    Some(if op == 0xa4 {
                        (x + y).to_bits()
                    } else {
                        x.mul_add(y, value(2)?).to_bits()
                    })
                }
                0xf4 if (i >> 24) & 3 == 0 => {
                    if i & 0x000000ff_ffffffff & !0x000000f7_c0ffffff != 0 {
                        return Err(bad());
                    }
                    let mut x = f32::from_bits(a.ok_or_else(bad)?);
                    let mut y = f32::from_bits(c.ok_or_else(bad)?);
                    if i & (1 << 39) != 0 {
                        x = x.abs();
                    }
                    if i & (1 << 38) != 0 {
                        x = -x;
                    }
                    if i & (1 << 37) != 0 {
                        y = y.abs();
                    }
                    if i & (1 << 36) != 0 {
                        y = -y;
                    }
                    let cond = match (i >> 32) & 7 {
                        0 => x == y,
                        1 => x > y,
                        2 => x >= y,
                        3 => x != y,
                        4 => x < y,
                        5 => x <= y,
                        6 => x < y || x > y,
                        _ => return Err(bad()),
                    };
                    let flag = cond || self.source(i, 2, uniforms).ok_or_else(bad)? != 0;
                    Some(if !flag {
                        0
                    } else {
                        match (i >> 30) & 3 {
                            0 => 1,
                            1 => 1f32.to_bits(),
                            2 => u32::MAX,
                            _ => return Err(bad()),
                        }
                    })
                }
                0x5e => {
                    if (i >> 46) & 3 != 2
                        || (i >> 33) & 7 != 2
                        || (i >> 20) & 1023 != 0
                        || (i >> 36) & 15 != 0
                        || (i >> 30) & 7 > 2
                        || dst & 1 != 0
                        || !self.registers_available(dst, 2)
                    {
                        return Err(bad());
                    }
                    let address = self
                        .resources
                        .buffer_address(
                            m,
                            ram,
                            ((i >> 8) & 15) as u32,
                            ((i >> 12) & 255) as u32,
                            a.ok_or_else(bad)?,
                            self.position_output,
                        )
                        .map_err(|error| match error {
                            crate::gpu_resources::Error::Memory(f) => Error::Memory(f),
                            _ => bad(),
                        })?;
                    self.regs[dst] = address as u32;
                    self.regs[dst + 1] = (address >> 32) as u32;
                    None
                }
                0x110 => Some(a.ok_or_else(bad)?.wrapping_add((i >> 8) as u32)),
                0x7d => {
                    // Alpha-to-coverage is disabled by the validated DCD. The
                    // native .h1 alpha operand is read but does not alter coverage.
                    if self.fragment.is_none()
                        || (i >> 46) & 3 != 2
                        || (i >> 24) & 65535 != 0x208
                        || self.source(i, 2, uniforms) != Some(0)
                        || (i >> 57) & 3 != 0
                        || !self.registers_available(dst, 1)
                    {
                        return Err(bad());
                    }
                    let _alpha = half((c.ok_or_else(bad)? >> 16) as u16);
                    self.regs[dst] = a.ok_or_else(bad)?;
                    self.fragment.as_mut().unwrap().active = self.regs[dst] & 1 != 0;
                    None
                }
                0x7f => {
                    let format = (i >> 24) & 7;
                    let count = if format == 3 { 2 } else { 4 };
                    if self.fragment.is_none()
                        || !matches!(format, 2 | 3)
                        || (i >> 46) & 3 != 1
                        || (i >> 33) & 7 != count as u64
                        || (i >> 28) & 3 != 3
                        || (i >> 30) & 7 > 2
                        || (i >> 36) & 15 != 0
                        || (i >> 8) & 255 != 0
                        || i & 255 != 0xf0
                        || (i >> 57) & 3 != 0
                        || flow != 15
                        || dst & 1 != 0
                        || !self.registers_available(dst, count)
                    {
                        return Err(bad());
                    }
                    a.ok_or_else(bad)?; // Validate the special blend-descriptor operand.
                    let coverage = self.source(i, 2, uniforms).ok_or_else(bad)?;
                    if coverage & 1 != 0 {
                        let color = if format == 3 {
                            let a = self.regs[dst];
                            let b = self.regs[dst + 1];
                            [
                                half(a as u16),
                                half((a >> 16) as u16),
                                half(b as u16),
                                half((b >> 16) as u16),
                            ]
                        } else {
                            let values: [u32; 4] = self.regs[dst..dst + 4].try_into().unwrap();
                            values.map(f32::from_bits)
                        };
                        if color.iter().any(|v| !v.is_finite()) {
                            return Err(bad());
                        }
                        self.fragment.as_mut().unwrap().color = Some(color);
                    }
                    None
                }
                0x66 => {
                    let count = ((i >> 28) & 3) as usize + 1;
                    if (i >> 46) & 3 != 2
                        || (i >> 33) & 7 != count as u64
                        || !matches!((i >> 24) & 7, 0 | 2)
                        || i & (1 << 27) != 0
                        || (i >> 36) & 15 != 0
                        || (i >> 30) & 7 > 2
                        || (i >> 16) & 15 >= 12
                        || !self.registers_available(dst, count)
                        || count > 1 && dst & 1 != 0
                    {
                        return Err(bad());
                    }
                    let values = self
                        .resources
                        .attribute(
                            m,
                            ram,
                            ((i >> 16) & 15) as u32,
                            ((i >> 20) & 15) as u32,
                            a.ok_or_else(bad)?,
                            c.ok_or_else(bad)?,
                        )
                        .map_err(|error| match error {
                            crate::gpu_resources::Error::Memory(f) => Error::Memory(f),
                            _ => bad(),
                        })?;
                    self.regs[dst..dst + count].copy_from_slice(&values[..count]);
                    None
                }
                0xa5 if (i >> 16) & 15 == 4 => {
                    if i & 0x000000ff_ffffffff & !0x000000f0_0000ffff != 0x40000 {
                        return Err(bad());
                    }
                    let value = |n: usize| -> Result<f32, Error> {
                        let mut v = f32::from_bits(self.source(i, n, uniforms).ok_or_else(bad)?);
                        if i & (1 << (39 - n * 2)) != 0 {
                            v = v.abs();
                        }
                        if i & (1 << (38 - n * 2)) != 0 {
                            v = -v;
                        }
                        Ok(v)
                    };
                    Some(pack_half(value(0)?) as u32 | (pack_half(value(1)?) as u32) << 16)
                }
                0x5c | 0x5d => {
                    let components = ((i >> 28) & 3) as usize + 1;
                    let count = if op == 0x5c {
                        components
                    } else {
                        (components + 1) / 2
                    };
                    let format = (i >> 24) & 15;
                    let update = (i >> 36) & 3;
                    if self.fragment.is_none()
                        || (i >> 46) & 3 != 2
                        || i & 0xff00 != 0
                        || (i >> 30) & 7 > 2
                        || (i >> 33) & 7 != count as u64
                        || (i >> 38) & 3 > 1
                        || update == 2
                        || (i >> 57) & 3 != 0
                        || !matches!(format, 2 | 3)
                        || !self.registers_available(dst, count)
                        || count > 1 && dst & 1 != 0
                    {
                        return Err(bad());
                    }
                    let interpolation = self.interpolation.as_mut().ok_or_else(bad)?;
                    if update == 1 && !interpolation.saved {
                        return Err(bad());
                    }
                    let values = interpolation
                        .load(
                            ((i >> 16) & 255) as usize,
                            components,
                            if format == 2 { 4 } else { 2 },
                        )
                        .ok_or_else(bad)?;
                    if op == 0x5c {
                        for n in 0..count {
                            self.regs[dst + n] = values[n].to_bits();
                        }
                    } else {
                        for n in 0..count {
                            self.regs[dst + n] = pack_half(values[n * 2]) as u32
                                | (pack_half(values[n * 2 + 1]) as u32) << 16;
                        }
                    }
                    interpolation.saved = update != 3;
                    None
                }
                0x128 => {
                    // Single-level normalized 2D sampling. The compact handle's
                    // second word must contain no texel offsets or array/LOD bits.
                    let src = ((i >> 16) & 63) as usize;
                    let ptr = (i & 63) as usize;
                    let mask = ((i >> 22) & 15) as u32;
                    let wide = i & (1 << 46) != 0;
                    let channels = mask.count_ones() as usize;
                    let count = if wide { channels } else { (channels + 1) / 2 };
                    let lod = (i >> 13) & 7;
                    if i & 0x1f00 != 0
                        || lod > 1
                        || lod == 1 && self.fragment.is_none()
                        || i & 128 != 0
                        || ptr & 1 != 0
                        || !self.registers_available(ptr, 2)
                        || self.regs[ptr + 1] != 0
                        || (i >> 26) & 3 > 1
                        || (i >> 28) & 3 != 1
                        || (i >> 30) & 7 > 2
                        || (i >> 33) & 7 != 2
                        || (i >> 36) & 7 != count.saturating_sub(1) as u64
                        || i & (1 << 47) != 0
                        || (i >> 57) & 3 != 0
                        || src & 1 != 0
                        || !self.registers_available(src, 2)
                        || mask == 0
                        || count > 1 && dst & 1 != 0
                        || !self.registers_available(dst, count)
                    {
                        return Err(bad());
                    }
                    let rgba = crate::gpu_texture::sample(
                        m,
                        ram,
                        self.resources,
                        self.regs[ptr],
                        [
                            f32::from_bits(self.regs[src]),
                            f32::from_bits(self.regs[src + 1]),
                        ],
                    )
                    .map_err(|e| match e {
                        crate::gpu_resources::Error::Memory(f) => Error::Memory(f),
                        _ => bad(),
                    })?;
                    let values: Vec<f32> = (0..4)
                        .filter(|n| mask & (1 << n) != 0)
                        .map(|n| rgba[n])
                        .collect();
                    if wide {
                        for (n, value) in values.iter().enumerate() {
                            self.regs[dst + n] = value.to_bits();
                        }
                    } else {
                        for (n, pair) in values.chunks(2).enumerate() {
                            self.regs[dst + n] = pack_half(pair[0]) as u32
                                | (pair.get(1).map_or(0, |&v| pack_half(v)) as u32) << 16;
                        }
                    }
                    None
                }
                0x60 | 0x61 => {
                    // MMU transfers; streaming store hints do not change data semantics
                    // in this synchronous model. Forced discarded-thread access is unsupported.
                    if ((i >> 46) & 3) != if op == 0x61 { 1 } else { 2 }
                        || !matches!(((i >> 27) & 7, (i >> 33) & 7), (3, 1) | (5, 2) | (7, 4))
                        || (i >> 24) & 3 > if op == 0x61 { 2 } else { 0 }
                        || (i >> 36) & 15
                            != if op == 0x60 && (i >> 27) & 7 == 7 {
                                7
                            } else {
                                0
                            }
                        || i & (1 << 26) != 0
                        || (i >> 30) & 7 > 2
                    {
                        return Err(bad());
                    }
                    // Bit 6 is the source-discard hint, including for pointers.
                    // FAU/immediate pointer operands remain unsupported.
                    if i & 128 != 0 {
                        return Err(bad());
                    }
                    let s = (i & 63) as usize;
                    if !self.registers_available(s, 2) || s & 1 != 0 {
                        return Err(bad());
                    }
                    let addr = (self.regs[s] as u64 | ((self.regs[s + 1] as u64) << 32))
                        .wrapping_add((i >> 8) as u16 as i16 as i64 as u64);
                    let count = ((i >> 33) & 7) as usize;
                    if !self.registers_available(dst, count) || count > 1 && dst & 1 != 0 {
                        return Err(bad());
                    }
                    if op == 0x61 {
                        let data: Vec<u8> = self.regs[dst..dst + count]
                            .iter()
                            .flat_map(|v| v.to_le_bytes())
                            .collect();
                        m.write(ram, addr, &data, true).map_err(Error::Memory)?;
                    } else {
                        let mut v = vec![0; count * 4];
                        m.read(ram, addr, &mut v, true).map_err(Error::Memory)?;
                        for (n, b) in v.chunks_exact(4).enumerate() {
                            self.regs[dst + n] = u32::from_le_bytes(b.try_into().unwrap());
                        }
                    }
                    None
                }
                0x1f => {
                    // Combining multiple words is not implemented by the scalar branch path.
                    if (i >> 37) & 7 != 0 || (i >> 40) & 255 != 0xc0 {
                        return Err(bad());
                    }
                    let v = a.ok_or_else(bad)?;
                    let take = if i & (1 << 36) != 0 { v == 0 } else { v != 0 };
                    if take {
                        let off = ((((i >> 8) & 0x7ffffff) as i32) << 5) >> 5;
                        next = next.checked_add_signed(off as i64 * 8).ok_or_else(bad)?;
                    }
                    None
                }
                _ => return Err(bad()),
            };
            if let Some(v) = result {
                if !self.registers_available(dst, 1) {
                    return Err(bad());
                }
                let mask = (i >> 46) & 3;
                self.regs[dst] = match mask {
                    3 => v,
                    _ => return Err(bad()),
                };
            }
            self.pc = next;
            if flow == 15 || flow == 13 && self.fragment.as_ref().is_some_and(|f| !f.active) {
                return Ok(());
            }
        }
        Err(Error::Budget)
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    // Assembled with Mesa asm.py at 06efd8542f442e701685e26abe4d73c4ddf7b249.
    const ADD: [u64; 5] = [
        0x0091c00000000080,
        0x0091c10000000081,
        0x00a4c20000008382,
        0x0061420218000000,
        0x7800c00000000000,
    ];
    #[test]
    fn mesa_shader_adds_uniforms_and_stores_result() {
        let m = AddressSpace {
            transtab: 0,
            transcfg: 2,
        };
        let mut ram = vec![0; 1024];
        for (n, i) in ADD.into_iter().enumerate() {
            ram[n * 8..n * 8 + 8].copy_from_slice(&i.to_le_bytes());
        }
        Shader::new(0)
            .run(
                &m,
                &mut ram,
                &[512, 0, 1.25f32.to_bits(), 2.5f32.to_bits()],
                16,
            )
            .unwrap();
        assert_eq!(f32::from_le_bytes(ram[512..516].try_into().unwrap()), 3.75);
    }
    #[test]
    fn unknown_instruction_and_budget_cannot_report_success() {
        let m = AddressSpace {
            transtab: 0,
            transcfg: 2,
        };
        let mut ram = vec![0; 64];
        assert_eq!(Shader::new(0).run(&m, &mut ram, &[], 1), Err(Error::Budget));
        ram[..8].copy_from_slice(&u64::MAX.to_le_bytes());
        assert!(matches!(
            Shader::new(0).run(&m, &mut ram, &[], 1),
            Err(Error::Unsupported { .. })
        ));
    }
    #[test]
    fn mesa_triangle_shader_tests_coverage_and_color() {
        let code: [u64; 27] = [
            0x91c00000000080,
            0x91c10000000081,
            0x91c80000000082,
            0x91c90000000083,
            0x91ca0000000084,
            0x91cb0000000085,
            0x91cc0000000086,
            0x91cd0000000087,
            0x91ce0000000088,
            0xb2c400000b0a09,
            0xb2c300000d0c08,
            0xb2c30000030e04,
            0xa4c2400000d003,
            0xa4c24000000204,
            0xf4c60400c0c002,
            0xf4c6040006c003,
            0xf4c6040006c004,
            0x1fc00000000306,
            0x91c500000000d0,
            0x61420838000000,
            0x7800c00000000000,
            0x91c2000000008a,
            0x91c3000000008b,
            0x91c4000000008c,
            0x91c500000000d0,
            0x61420838000000,
            0x7800c00000000000,
        ];
        let m = AddressSpace {
            transtab: 0,
            transcfg: 2,
        };
        let mut ram = vec![0; 4096];
        for (n, i) in code.into_iter().enumerate() {
            ram[n * 8..n * 8 + 8].copy_from_slice(&i.to_le_bytes());
        }
        for (x, y, inside) in [(0.0f32, 0.0f32, true), (1.0, 1.0, false)] {
            let mut u = vec![0u32; 16];
            u[0] = 1024;
            for (n, v) in [
                x,
                y,
                1.0 / 1.4,
                0.6 / 1.4,
                1.0 / 1.5,
                0.7 / 1.5,
                -0.7 / 1.5,
                0.0,
                0.04,
                0.08,
                0.18,
            ]
            .into_iter()
            .enumerate()
            {
                u[n + 2] = v.to_bits();
            }
            Shader::new(0).run(&m, &mut ram, &u, 128).unwrap();
            let color: Vec<f32> = ram[1024..1040]
                .chunks_exact(4)
                .map(|b| f32::from_le_bytes(b.try_into().unwrap()))
                .collect();
            if inside {
                assert!((color[0] + color[1] + color[2] - 1.0).abs() < 0.00001);
            } else {
                assert_eq!(&color[..3], &[0.04, 0.08, 0.18]);
            }
            assert_eq!(color[3], 1.0);
        }
    }
    #[test]
    fn unsupported_half_register_mask_does_not_change_register() {
        let m = AddressSpace {
            transtab: 0,
            transcfg: 2,
        };
        let mut ram = vec![0; 8];
        ram.copy_from_slice(&(0x00914000000000d0u64).to_le_bytes());
        let mut shader = Shader::new(0);
        shader.regs[0] = 123;
        assert!(matches!(
            shader.run(&m, &mut ram, &[], 1),
            Err(Error::Unsupported { .. })
        ));
        assert_eq!(shader.regs[0], 123);
    }
    #[test]
    fn unsupported_branch_memory_fields_and_registers_fault_without_side_effects() {
        let m = AddressSpace {
            transtab: 0,
            transcfg: 2,
        };
        let cases = [
            0x001fc000000000c0 | (1 << 37),
            ADD[3] | (1 << 26),
            ADD[3] | (3 << 30),
            ADD[0] | (32 << 40),
            (ADD[0] & !255) | 32,
        ];
        for instruction in cases {
            let mut ram = vec![0x55; 1024];
            ram[..8].copy_from_slice(&instruction.to_le_bytes());
            let before = ram.clone();
            let mut shader = Shader::with_allocation(0, RegisterAllocation::Registers32);
            shader.regs[0] = 512;
            assert!(
                matches!(
                    shader.run(
                        &m,
                        &mut ram,
                        &[512, 0, 1.25f32.to_bits(), 2.5f32.to_bits()],
                        1
                    ),
                    Err(Error::Unsupported { .. })
                ),
                "{instruction:016x}"
            );
            assert_eq!(ram, before);
            assert_eq!(shader.pc, 0);
        }
    }
    #[test]
    fn mesa_split_register_shader_reads_preloads_and_transfers_high_vectors() {
        let m = AddressSpace {
            transtab: 0,
            transcfg: 2,
        };
        let code = [
            0x91c00000000080u64,
            0x91c10000000081u64,
            0x91f0000000003cu64,
            0x91f1000000003du64,
            0x91f2000000003bu64,
            0x91f3000000003eu64,
            0x61700838000000u64,
            0x60bc7838000000u64,
            0x617c0838001000u64,
            0x7800c00000000000u64,
        ];
        for allocation in [
            RegisterAllocation::Registers32,
            RegisterAllocation::Registers64,
        ] {
            let mut ram = vec![0; 4096];
            for (n, instruction) in code.into_iter().enumerate() {
                ram[n * 8..n * 8 + 8].copy_from_slice(&instruction.to_le_bytes());
            }
            let mut shader = Shader::with_allocation(0, allocation);
            shader.regs[59..63].copy_from_slice(&[9, 11, 13, 15]);
            shader.run(&m, &mut ram, &[1024, 0], 16).unwrap();
            let expected: Vec<u8> = [11u32, 13, 9, 15]
                .iter()
                .flat_map(|v| v.to_le_bytes())
                .collect();
            assert_eq!(&ram[1024..1040], &expected);
            assert_eq!(&ram[1040..1056], &expected);
            assert_eq!(&shader.regs[60..64], &[11, 13, 9, 15]);
        }
    }
    #[test]
    fn split_register_hole_rejects_scalar_address_and_crossing_vectors() {
        let m = AddressSpace {
            transtab: 0,
            transcfg: 2,
        };
        let instructions = [
            0x0091d000000000c0u64, // MOV r16, zero
            0x0091c00000000010,    // MOV r0, r16
            0x0061700838000010,    // STORE.i128 r48..r51, address r16:r17
            0x00614e0838000000,    // STORE.i128 r14..r17 crosses the hole
            0x00608e7838000000,    // LOAD.i128 r14..r17 crosses the hole
        ];
        for instruction in instructions {
            let mut ram = vec![0x55; 4096];
            ram[..8].copy_from_slice(&instruction.to_le_bytes());
            let before = ram.clone();
            let mut shader = Shader::with_allocation(0, RegisterAllocation::Registers32);
            shader.regs[0] = 1024;
            let registers = shader.regs;
            assert!(
                matches!(
                    shader.run(&m, &mut ram, &[], 1),
                    Err(Error::Unsupported { .. })
                ),
                "{instruction:x}"
            );
            assert_eq!(shader.regs, registers);
            assert_eq!(ram, before);
            assert_eq!(shader.pc, 0);
        }
    }
}

#[cfg(test)]
mod fragment_tests {
    use super::*;
    #[test]
    fn native_panfrost_fragment_runs_coverage_discard_flow_and_half_color() {
        let m = AddressSpace {
            transtab: 0,
            transcfg: 2,
        };
        let code = [
            0x4110c03c000000c0u64,
            0x087dbc0208ea407c,
            0x6891c000000000db,
            0x4891c100000000db,
            0x787f4004333c00f0,
        ];
        let mut ram: Vec<u8> = code.iter().flat_map(|i| i.to_le_bytes()).collect();
        for coverage in [0, 1] {
            let mut shader = Shader::with_allocation(0, RegisterAllocation::Registers32);
            shader.fragment = Some(Fragment {
                blend: [0x19, 237 << 12],
                color: None,
                active: true,
            });
            shader.regs[60] = coverage;
            shader.run(&m, &mut ram, &[], 16).unwrap();
            assert_eq!(
                shader.fragment.unwrap().color,
                if coverage == 0 {
                    None
                } else {
                    Some([0., 1., 0., 1.])
                }
            );
            assert_eq!(shader.instructions, if coverage == 0 { 3 } else { 5 });
        }
        let mut shader = Shader::new(8);
        assert!(matches!(
            shader.run(&m, &mut ram, &[], 1),
            Err(Error::Unsupported { .. })
        ));
    }
    #[test]
    fn atest_accepts_a_register_copy_of_the_disabled_datum() {
        let m = AddressSpace {
            transtab: 0,
            transcfg: 2,
        };
        let code = [
            0x4091c000000000eau64,
            0x087dbc020840817c,
            0x7800c00000000000,
        ];
        let mut ram: Vec<u8> = code.iter().flat_map(|i| i.to_le_bytes()).collect();
        let mut shader = Shader::new(0);
        shader.fragment = Some(Fragment {
            blend: [0; 2],
            color: None,
            active: true,
        });
        shader.regs[60] = 1;
        shader.run(&m, &mut ram, &[0, 0x38000000], 3).unwrap();
        assert_eq!(shader.regs[60], 1);
        shader.pc = 8;
        shader.regs[0] = 1;
        let before = shader.regs;
        assert!(matches!(
            shader.run(&m, &mut ram, &[0, 0x38000000], 1),
            Err(Error::Unsupported { .. })
        ));
        assert_eq!(shader.regs, before);
    }
    #[test]
    fn native_half_uniform_conversion_selects_each_lane_and_modifiers() {
        let m = AddressSpace {
            transtab: 0,
            transcfg: 2,
        };
        let mut ram = vec![0; 16];
        let code = [0x0090c000000b0080u64, 0x7890c100100b0080];
        for (n, i) in code.iter().enumerate() {
            ram[n * 8..n * 8 + 8].copy_from_slice(&i.to_le_bytes());
        }
        let mut shader = Shader::new(0);
        shader.run(&m, &mut ram, &[0xbc003400], 2).unwrap();
        assert_eq!(shader.regs[0], 0.25f32.to_bits());
        assert_eq!(shader.regs[1], (-1.0f32).to_bits());
        ram[..8].copy_from_slice(&(code[1] | (3 << 38)).to_le_bytes());
        let mut shader = Shader::new(0);
        shader.run(&m, &mut ram, &[0xbc003400], 1).unwrap();
        assert_eq!(shader.regs[1], (-1.0f32).to_bits());
    }
    #[test]
    fn texture_result_masks_and_widths_preserve_component_order() {
        for mask in [1u64, 3, 5, 15] {
            for wide in [false, true] {
                let (m, resources, mut ram, h) = crate::gpu_texture::test_fixture();
                let channels = mask.count_ones() as usize;
                let count = if wide { channels } else { (channels + 1) / 2 };
                let mut i = 0x4128009413c02042u64
                    & !((15 << 22) | (7 << 36) | (1 << 46) | (63 << 16) | (7 << 13));
                i |= (mask << 22)
                    | (((count - 1) as u64) << 36)
                    | (u64::from(wide) << 46)
                    | (4 << 16);
                ram[..8].copy_from_slice(&i.to_le_bytes());
                ram[8..16].copy_from_slice(&0x7800c00000000000u64.to_le_bytes());
                let mut shader = Shader::with_allocation(0, RegisterAllocation::Registers32);
                shader.resources = resources;
                shader.regs.fill(0xdeadbeef);
                shader.regs[2] = h;
                shader.regs[3] = 0;
                shader.regs[4] = 0.75f32.to_bits();
                shader.regs[5] = 0.25f32.to_bits();
                let before = shader.regs;
                shader.run(&m, &mut ram, &[], 2).unwrap();
                let expected = [0., 1., 0., 128. / 255.];
                let selected: Vec<f32> = (0..4)
                    .filter(|n| mask & (1 << n) != 0)
                    .map(|n| expected[n])
                    .collect();
                for n in 0..count {
                    let expected = if wide {
                        selected[n].to_bits()
                    } else {
                        pack_half(selected[n * 2]) as u32
                            | (selected.get(n * 2 + 1).map_or(0, |&v| pack_half(v)) as u32) << 16
                    };
                    assert_eq!(shader.regs[n], expected);
                }
                assert_eq!(&shader.regs[count..], &before[count..]);
            }
        }
    }
    #[test]
    fn invalid_texture_encodings_do_not_modify_destinations() {
        for bit in [8, 9, 10, 11, 12, 14, 15, 47, 57] {
            let (m, resources, mut ram, h) = crate::gpu_texture::test_fixture();
            let i = 0x4128009413c02042u64 | (1 << bit);
            ram[..8].copy_from_slice(&i.to_le_bytes());
            let mut shader = Shader::with_allocation(0, RegisterAllocation::Registers32);
            shader.resources = resources;
            shader.fragment = Some(Fragment {
                blend: [0x19, 237 << 12],
                color: None,
                active: true,
            });
            shader.regs[0] = 0.25f32.to_bits();
            shader.regs[1] = 0.25f32.to_bits();
            shader.regs[2] = h;
            let before = shader.regs;
            assert!(
                matches!(
                    shader.run(&m, &mut ram, &[], 1),
                    Err(Error::Unsupported { .. })
                ),
                "bit={bit}"
            );
            assert_eq!(shader.regs, before);
        }
    }
    fn interpolation_fixture() -> Interpolation {
        let values = [0f32, 1., 0., 0., 0.5, 0.5, 0., 0., 1., 0., 0., 0.];
        Interpolation {
            data: std::sync::Arc::from(
                values
                    .iter()
                    .flat_map(|v| v.to_le_bytes())
                    .collect::<Vec<_>>(),
            ),
            stride: 16,
            vertices: [0, 1, 2],
            weights: [0.25, 0.5, 0.25],
            saved: false,
        }
    }
    #[test]
    fn native_varying_load_checks_offsets_widths_and_cached_position() {
        let m = AddressSpace {
            transtab: 0,
            transcfg: 2,
        };
        for (op, count) in [(0x5cu64, 2), (0x5d, 1)] {
            let i =
                (0x085c80041200007du64 & !((511 << 48) | (7 << 33))) | (op << 48) | (count << 33);
            let mut ram: Vec<u8> = [i, 0x7800c00000000000]
                .iter()
                .flat_map(|i| i.to_le_bytes())
                .collect();
            let mut shader = Shader::new(0);
            shader.fragment = Some(Fragment {
                blend: [0; 2],
                color: None,
                active: true,
            });
            shader.interpolation = Some(interpolation_fixture());
            shader.run(&m, &mut ram, &[], 2).unwrap();
            assert_eq!(
                shader.regs[0],
                if op == 0x5c {
                    0.5f32.to_bits()
                } else {
                    0x38003800
                }
            );
            assert!(shader.interpolation.as_ref().unwrap().saved);
            // Retrieve is illegal until a position has been stored.
            ram[..8].copy_from_slice(&(i | (1 << 36)).to_le_bytes());
            shader.pc = 0;
            shader.interpolation.as_mut().unwrap().saved = false;
            let before = shader.regs;
            assert!(matches!(
                shader.run(&m, &mut ram, &[], 2),
                Err(Error::Unsupported { .. })
            ));
            assert_eq!(shader.regs, before);
            shader.interpolation.as_mut().unwrap().saved = true;
            shader.run(&m, &mut ram, &[], 2).unwrap();
            ram[..8].copy_from_slice(&(i | (12 << 16)).to_le_bytes()); // v2 over stride boundary
            shader.pc = 0;
            assert!(matches!(
                shader.run(&m, &mut ram, &[], 2),
                Err(Error::Unsupported { .. })
            ));
        }
    }
    #[test]
    fn interpolation_uses_byte_offsets_vertex_order_and_half_source_data() {
        let mut v = interpolation_fixture();
        assert_eq!(v.load(4, 1, 4).unwrap()[0], 0.5);
        assert!(v.load(2, 1, 4).is_none());
        v.weights = [1., 0., 0.];
        v.vertices = [2, 0, 1];
        assert_eq!(v.load(0, 2, 4).unwrap()[..2], [1., 0.]);
        v.stride = 4;
        v.vertices = [0, 1, 2];
        v.weights = [0.25, 0.5, 0.25];
        v.data = std::sync::Arc::from(
            [0x3c000000u32, 0x38003800, 0x00003c00]
                .iter()
                .flat_map(|i| i.to_le_bytes())
                .collect::<Vec<_>>(),
        );
        assert_eq!(v.load(0, 2, 2).unwrap()[..2], [0.5, 0.5]);
        v.vertices[2] = usize::MAX;
        assert!(v.load(0, 2, 2).is_none());
    }
    #[test]
    fn native_widening_fma_and_packed_conversion_execute() {
        let m = AddressSpace {
            transtab: 0,
            transcfg: 2,
        };
        let code = [0x00b2c0000adada40u64, 0x78a5c10000040000];
        let mut ram: Vec<u8> = code.iter().flat_map(|i| i.to_le_bytes()).collect();
        let mut shader = Shader::new(0);
        shader.regs[0] = (-0.75f32).to_bits();
        shader.run(&m, &mut ram, &[], 2).unwrap();
        assert_eq!(shader.regs[0], 0.125f32.to_bits());
        assert_eq!(shader.regs[1], 0x30003000);
        ram[..8].copy_from_slice(&(code[0] | (3 << 28)).to_le_bytes());
        shader.pc = 0;
        let before = shader.regs;
        assert!(matches!(
            shader.run(&m, &mut ram, &[], 1),
            Err(Error::Unsupported { .. })
        ));
        assert_eq!(shader.regs, before);
    }
    #[test]
    fn half_encoding_round_trips_and_rounds_ties_even() {
        for h in 0..=u16::MAX {
            if h & 0x7c00 == 0x7c00 && h & 1023 != 0 {
                continue;
            }
            assert_eq!(pack_half(half(h)), h, "{h:x}");
        }
        assert_eq!(pack_half(1.0 + 1.0 / 2048.), 0x3c00);
        assert_eq!(pack_half(1.0 + 3.0 / 2048.), 0x3c02);
        assert_eq!(pack_half(2f32.powi(-25)), 0);
        assert_eq!(pack_half(3.0 * 2f32.powi(-25)), 2);
    }
    #[test]
    fn native_texture_shader_samples_and_blends_without_host_color_injection() {
        let (m, resources, mut ram, _) = crate::gpu_texture::test_fixture();
        let code = [
            0x0110c03e800000c0u64, // u=v=0.25
            0x0091c10000000000,
            0x0110c220001800c0, // actual compact resource handle
            0x0091c300000000c0,
            0x4128009413c02042,
            0x487dbc0208ea017c,
            0x787f4004733c00f0,
        ];
        for (n, i) in code.iter().enumerate() {
            ram[n * 8..n * 8 + 8].copy_from_slice(&i.to_le_bytes());
        }
        let mut shader = Shader::with_allocation(0, RegisterAllocation::Registers32);
        shader.resources = resources;
        shader.regs[60] = 1;
        shader.fragment = Some(Fragment {
            blend: [0x19, 237 << 12],
            color: None,
            active: true,
        });
        shader.run(&m, &mut ram, &[], 16).unwrap();
        assert_eq!(shader.fragment.unwrap().color, Some([1., 0., 0., 1.]));
        // A faulting texel read leaves the whole destination unchanged.
        ram[0x2108..0x210c].copy_from_slice(&0xfffffff0u32.to_le_bytes());
        let mut shader = Shader::with_allocation(32, RegisterAllocation::Registers32);
        shader.resources = resources;
        shader.fragment = Some(Fragment {
            blend: [0x19, 237 << 12],
            color: None,
            active: true,
        });
        shader.regs[0] = 0.25f32.to_bits();
        shader.regs[1] = 0.25f32.to_bits();
        shader.regs[2] = 0x20001800;
        let before = shader.regs;
        assert!(matches!(
            shader.run(&m, &mut ram, &[], 1),
            Err(Error::Memory(_))
        ));
        assert_eq!(shader.regs, before);
    }
    #[test]
    fn half_conversion_preserves_special_values_and_subnormals() {
        assert_eq!(half(0), 0.0);
        assert_eq!(half(0x8000).to_bits(), (-0.0f32).to_bits());
        assert_eq!(half(1), 2f32.powi(-24));
        assert_eq!(half(0x3c00), 1.0);
        assert_eq!(half(0xbc00), -1.0);
        assert_eq!(half(0x7c00), f32::INFINITY);
        assert!(half(0x7e00).is_nan());
    }
}
