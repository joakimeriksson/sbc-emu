//! Message-passing instructions: varyings, attributes, memory, blending and texturing.
use super::{bits, half, pack_half, sext, Env, Next, Ops, QuadView, Shader, OUTPUT_BASE, R, TLS_BASE, TLS_LIMIT};
use crate::gpu_resources::Error as ResourceError;
use crate::gpu_texture::{Lod, Request};

fn memory(e: ResourceError) -> Option<super::Error> {
    match e {
        ResourceError::Memory(f) => Some(super::Error::Memory(f)),
        _ => None,
    }
}

/// Instructions that read other lanes of the quad: CLPER and implicit-LOD texturing.
pub(super) fn needs_quad(i: u64) -> bool {
    let op = bits(i, 48, 9);
    matches!(op, 0x0a0..=0x0aa) && bits(i, 16, 4) == 0xf || matches!(op, 0x128 | 0x138 | 0x129)
}

/// CUBE_SSEL (sub 0) picks s, CUBE_TSEL (sub 1) picks t, for the face in bits 29-31.
pub(super) fn cube_select(sub: u32, a: f32, b: f32, face: u32) -> Option<u32> {
    let face = face >> 29;
    // SSEL(z, x, face): +X -z, -X +z, +-Y x, +Z x, -Z -x.
    // TSEL(y, z, face): +-X -y, +Y z, -Y -z, +-Z -y.
    let v = match (sub, face) {
        (0, 0) => -a,
        (0, 1) => a,
        (0, 2 | 3 | 4) => b,
        (0, 5) => -b,
        (1, 0 | 1 | 4 | 5) => -a,
        (1, 2) => b,
        (1, 3) => -b,
        _ => return None,
    };
    Some(v.to_bits())
}
/// CUBEFACE1: the major axis magnitude; CUBEFACE2: the face index in bits 29-31.
pub(super) fn cube_face(second: bool, x: f32, y: f32, z: f32) -> u32 {
    let (ax, ay, az) = (x.abs(), y.abs(), z.abs());
    let (face, major) = if ax >= ay && ax >= az {
        (if x < 0. { 1 } else { 0 }, ax)
    } else if ay >= az {
        (if y < 0. { 3 } else { 2 }, ay)
    } else {
        (if z < 0. { 5 } else { 4 }, az)
    };
    if second {
        face << 29
    } else {
        major.to_bits()
    }
}

/// The start of an access of `bytes` at `offset` into a window of `limit` bytes, if it
/// fits; computed in 64 bits so no offset wraps on a 32-bit host (wasm32).
fn window(offset: u64, bytes: usize, limit: usize) -> Option<usize> {
    (offset.checked_add(bytes as u64)? <= limit as u64).then_some(offset as usize)
}

/// Message slots 0-2 and 7 exist; 3-6 are reserved.
fn slot_ok(i: u64) -> bool {
    !matches!(bits(i, 30, 3), 3..=6)
}

/// Convert 32-bit float or integer results to the register format (Register format
/// enum: auto f32 - f16 s32 s16 u32 u16) and write them to consecutive registers.
fn write_values(s: &mut Shader, r: usize, values: &[u32], format: u32, float: bool) -> R<usize> {
    let narrow = matches!(format, 3 | 5 | 7);
    let regs = if narrow {
        values.len().div_ceil(2)
    } else {
        values.len()
    };
    if !s.registers_available(r, regs) {
        return Err(None);
    }
    if narrow {
        for (n, pair) in values.chunks(2).enumerate() {
            let conv = |v: u32| -> u32 {
                match format {
                    3 => {
                        if float {
                            pack_half(f32::from_bits(v)) as u32
                        } else {
                            pack_half(v as i32 as f32) as u32
                        }
                    }
                    _ => {
                        if float {
                            let f = f32::from_bits(v);
                            if format == 5 {
                                f as i32 as u32 & 0xffff
                            } else {
                                f as u32 & 0xffff
                            }
                        } else {
                            v & 0xffff
                        }
                    }
                }
            };
            s.regs[r + n] = conv(pair[0]) | pair.get(1).map_or(0, |&v| conv(v) << 16);
        }
    } else {
        for (n, &v) in values.iter().enumerate() {
            s.regs[r + n] = match (format, float) {
                (4 | 6, true) => {
                    let f = f32::from_bits(v);
                    if format == 4 {
                        f as i32 as u32
                    } else {
                        f as u32
                    }
                }
                (2, false) => (v as i32 as f32).to_bits(),
                _ => v,
            };
        }
    }
    Ok(regs)
}

impl Shader {
    /// LD_VAR_BUF(_IMM): interpolate a varying from the primitive's vertex records.
    pub(super) fn ld_var_buf(&mut self, i: u64, u: &[u32]) -> R<Next> {
        let op = bits(i, 48, 9);
        let wide = op & 1 == 0;
        let components = bits(i, 28, 2) as usize + 1;
        let format = bits(i, 24, 2);
        let count = bits(i, 33, 3) as usize;
        let expected = if wide {
            components
        } else {
            components.div_ceil(2)
        };
        if bits(i, 46, 2) != 2 || count != expected || bits(i, 26, 2) != 0 || bits(i, 36, 2) == 2 {
            return Err(None);
        }
        let r = self.staging(i, count).ok_or(None)?;
        let o = Ops {
            s: self,
            i,
            uniforms: u,
        };
        o.raw(0)?;
        // LD_VAR_BUF_IMM (0x5c/0x5d) has an immediate byte offset, LD_VAR_BUF a register.
        let offset = if op < 0x60 { bits(i, 16, 8) } else { o.raw(1)? } as usize;
        let interp = self.interpolation.as_ref().ok_or(None)?;
        let width = if format & 1 == 0 { 4 } else { 2 };
        let values: [u32; 4] = if format < 2 {
            let raw = interp.flat(offset, components, width).ok_or(None)?;
            if width == 4 {
                raw
            } else {
                raw.map(|h| half(h as u16).to_bits())
            }
        } else {
            interp
                .load(offset, components, width)
                .ok_or(None)?
                .map(f32::to_bits)
        };
        if wide {
            if format == 1 {
                // flat16 into 32-bit registers: zero-extended raw halves
                let raw = interp.flat(offset, components, 2).ok_or(None)?;
                self.regs[r..r + components].copy_from_slice(&raw[..components]);
            } else {
                self.regs[r..r + components].copy_from_slice(&values[..components]);
            }
        } else {
            for n in 0..count {
                let h = |k: usize| -> u32 {
                    if k >= components {
                        return 0;
                    }
                    if format == 0 {
                        // flat32 into 16-bit registers: truncate the raw value
                        values[k] & 0xffff
                    } else {
                        pack_half(f32::from_bits(values[k])) as u32
                    }
                };
                self.regs[r + n] = h(2 * n) | h(2 * n + 1) << 16;
            }
        }
        Ok(Next::Continue)
    }

    /// LD_VAR_SPECIAL: point coordinate, fragment w or fragment z.
    pub(super) fn ld_var_special(&mut self, i: u64) -> R<Next> {
        let f = self.fragment.as_ref().ok_or(None)?;
        let components = bits(i, 28, 2) as usize + 1;
        let base = bits(i, 12, 4) as usize;
        let all = [f.point[0], f.point[1], f.frag_w, f.frag_z];
        let values: Vec<u32> = (0..components)
            .map(|n| all.get(base + n).copied().unwrap_or(0.).to_bits())
            .collect();
        let format = bits(i, 24, 3);
        let count = bits(i, 33, 3) as usize;
        let r = self.staging(i, count).ok_or(None)?;
        write_values(self, r, &values, if format == 0 { 2 } else { format }, true)?;
        Ok(Next::Continue)
    }

    /// LEA_BUF_IMM: address of a structured-buffer element, or of the private IDVS
    /// output record (table 13).
    pub(super) fn lea_buf_imm(&mut self, env: &mut Env, i: u64) -> R<Next> {
        if bits(i, 46, 2) != 2 || bits(i, 33, 3) != 2 || bits(i, 20, 10) != 0 || bits(i, 36, 4) != 0
        {
            return Err(None);
        }
        let r = self.staging(i, 2).ok_or(None)?;
        let element = Ops {
            s: self,
            i,
            uniforms: env.uniforms,
        }
        .raw(0)?;
        let (table, index) = (bits(i, 8, 4), bits(i, 12, 8));
        let address = if table == 13 {
            let (mul, div) = env.output_unit;
            if index != 0 || env.output.is_none() || mul == 0 || div == 0 {
                return Err(None);
            }
            let offset = element as u64 * mul as u64 / div as u64;
            if offset >= env.output.as_ref().unwrap().len() as u64 {
                return Err(None);
            }
            OUTPUT_BASE + offset
        } else {
            self.resources
                .buffer_address(env.m, &env.ram, table, index, element, None)
                .map_err(memory)?
        };
        self.regs[r] = address as u32;
        self.regs[r + 1] = (address >> 32) as u32;
        Ok(Next::Continue)
    }

    fn address(&self, i: u64, u: &[u32]) -> R<u64> {
        let a = self.source64(i, 0, u).ok_or(None)?;
        Ok(a.wrapping_add(sext(bits(i, 8, 16), 16) as i64 as u64))
    }

    /// LOAD: global memory into staging registers, with lane selection and extension
    /// for sub-word sizes.
    pub(super) fn load(&mut self, env: &mut Env, i: u64) -> R<Next> {
        let size = bits(i, 27, 3);
        if bits(i, 46, 2) != 2 || i & (1 << 26) != 0 || !slot_ok(i) {
            return Err(None);
        }
        let address = self.address(i, env.uniforms)?;
        let bytes: usize = [1, 2, 3, 4, 6, 8, 12, 16][size as usize];
        let mut data = [0u8; 16];
        if address >= OUTPUT_BASE {
            let out = env.output.as_ref().ok_or(None)?;
            let off = window(address - OUTPUT_BASE, bytes, out.len()).ok_or(None)?;
            data[..bytes].copy_from_slice(&out[off..off + bytes]);
        } else if address >= TLS_BASE {
            let off = window(address - TLS_BASE, bytes, TLS_LIMIT).ok_or(None)?;
            for (k, d) in data[..bytes].iter_mut().enumerate() {
                *d = self.scratch.get(off + k).copied().unwrap_or(0);
            }
        } else {
            env.read(address, &mut data[..bytes])
                .map_err(|f| Some(super::Error::Memory(f)))?;
        }
        self.load_lanes(i, size, &data, bits(i, 33, 3) as usize)?;
        Ok(Next::Continue)
    }

    /// Place loaded bytes in registers (Load lane enums).
    fn load_lanes(&mut self, i: u64, size: u32, data: &[u8; 16], count: usize) -> R<()> {
        let lane = bits(i, 36, 3);
        let unsigned = i & (1 << 39) != 0;
        let word = |n: usize| u32::from_le_bytes(data[n * 4..n * 4 + 4].try_into().unwrap());
        let ext = |v: u32, width: u32| -> u32 {
            if unsigned {
                v
            } else {
                sext(v, width) as u32
            }
        };
        let r = self.staging(i, count).ok_or(None)?;
        match (size, lane) {
            (0, 0..=3) => {
                let shift = 8 * lane;
                self.regs[r] = (self.regs[r] & !(255 << shift)) | ((data[0] as u32) << shift);
            }
            (0, 4 | 5) | (1, 0 | 1) => {
                let v = if size == 0 {
                    ext(data[0] as u32, 8) & 0xffff
                } else {
                    word(0) & 0xffff
                };
                let shift = 16 * (lane & 1);
                self.regs[r] = (self.regs[r] & !(0xffff << shift)) | (v << shift);
            }
            (0, 6) | (1, 2) | (3, 0) => {
                self.regs[r] = match size {
                    0 => ext(data[0] as u32, 8),
                    1 => ext(word(0) & 0xffff, 16),
                    _ => word(0),
                };
            }
            (0, 7) | (1, 3) | (3, 1) => {
                if count < 2 || r & 1 != 0 {
                    return Err(None);
                }
                let v = match size {
                    0 => ext(data[0] as u32, 8),
                    1 => ext(word(0) & 0xffff, 16),
                    _ => word(0),
                };
                self.regs[r] = v;
                self.regs[r + 1] = if unsigned || (v as i32) >= 0 {
                    0
                } else {
                    u32::MAX
                };
            }
            (2, _) | (4..=7, _) => {
                let words = [1, 1, 1, 1, 2, 2, 3, 4][size as usize];
                if count < words {
                    return Err(None);
                }
                for n in 0..words {
                    self.regs[r + n] = word(n);
                }
            }
            _ => return Err(None),
        }
        Ok(())
    }

    /// STORE: staging registers to global memory, or to the IDVS output record.
    pub(super) fn store(&mut self, env: &mut Env, i: u64) -> R<Next> {
        let size = bits(i, 27, 3);
        let bytes: usize = [1, 2, 3, 4, 6, 8, 12, 16][size as usize];
        let count = bits(i, 33, 3) as usize;
        if bits(i, 46, 2) != 1
            || count != bytes.div_ceil(4)
            || i & (1 << 26) != 0
            || bits(i, 36, 4) != 0
            || !slot_ok(i)
        {
            return Err(None);
        }
        let r = self.staging(i, count).ok_or(None)?;
        let address = self.address(i, env.uniforms)?;
        // Helper threads have no memory side effects.
        if self.fragment.as_ref().is_some_and(|f| !f.active) {
            return Ok(Next::Continue);
        }
        let data: Vec<u8> = self.regs[r..r + count]
            .iter()
            .flat_map(|v| v.to_le_bytes())
            .take(bytes)
            .collect();
        if address >= OUTPUT_BASE {
            let out = env.output.as_mut().ok_or(None)?;
            let off = window(address - OUTPUT_BASE, bytes, out.len()).ok_or(None)?;
            out[off..off + bytes].copy_from_slice(&data);
        } else if address >= TLS_BASE {
            let off = window(address - TLS_BASE, bytes, TLS_LIMIT).ok_or(None)?;
            if self.scratch.len() < off + bytes {
                self.scratch.resize(off + bytes, 0);
            }
            self.scratch[off..off + bytes].copy_from_slice(&data);
        } else {
            env.write(address, &data).map_err(Some)?;
        }
        Ok(Next::Continue)
    }

    /// LD_ATTR(_IMM): fetch a vertex attribute and convert it to the register format.
    pub(super) fn ld_attr(&mut self, env: &mut Env, i: u64) -> R<Next> {
        let op = bits(i, 48, 9);
        // Descriptor type 1 (LD_TEX image loads) and reserved fields are not modelled.
        if bits(i, 36, 4) != 0 || bits(i, 46, 2) != 2 || bits(i, 24, 3) == 1 || bits(i, 30, 3) > 2 {
            return Err(None);
        }
        let o = Ops {
            s: self,
            i,
            uniforms: env.uniforms,
        };
        let (vertex, instance) = (o.raw(0)?, o.raw(1)?);
        let (table, index) = if op == 0x066 {
            (bits(i, 16, 4), bits(i, 20, 4))
        } else {
            let h = o.raw(2)?;
            (h >> 24, h & 0xff_ffff)
        };
        if table >= 12 {
            return Err(None); // hardware tables 60-63
        }
        let components = bits(i, 28, 2) as usize + 1;
        let format = bits(i, 24, 3);
        let count = bits(i, 33, 3) as usize;
        if count
            != if matches!(format, 3 | 5 | 7) {
                components.div_ceil(2)
            } else {
                components
            }
        {
            return Err(None);
        }
        let key = (self.resources.0, table, index);
        let attr = match env.attributes.get(&key) {
            Some(a) => *a,
            None => {
                let a = self.resources.attribute_desc(env.m, &env.ram, table, index).map_err(memory)?;
                env.attributes.insert(key, a);
                a
            }
        };
        let (m, ram, tlb) = (env.m, &*env.ram, &mut *env.tlb);
        let (values, float) = attr.fetch(|a, out| tlb.read(m, ram, a, out), vertex, instance).map_err(memory)?;
        let r = self.staging(i, count).ok_or(None)?;
        write_values(
            self,
            r,
            &values[..components],
            if format == 0 {
                if float {
                    2
                } else {
                    6
                }
            } else {
                format
            },
            float,
        )?;
        Ok(Next::Continue)
    }

    /// LD_VAR_IMM: a varying from attribute descriptors, as Mesa's frame-shader preload
    /// (pan_fb_preload.c) uses it: four vertices at the framebuffer's corners
    /// (0,0), (w,0), (0,h), (w,h), interpolated at the pixel centre. Elsewhere (draws
    /// without IDVS) it is not modelled.
    pub(super) fn ld_var_imm(&mut self, env: &mut Env, i: u64) -> R<Next> {
        let f = self.fragment.as_ref().ok_or(None)?;
        let ([w, h], [px, py]) = (f.frame.ok_or(None)?, f.pixel);
        let components = bits(i, 28, 2) as usize + 1;
        let format = bits(i, 24, 3);
        let count = bits(i, 33, 3) as usize;
        let expected = if matches!(format, 3 | 5 | 7) { components.div_ceil(2) } else { components };
        if bits(i, 46, 2) != 2 || count != expected || bits(i, 36, 4) != 0 || !matches!(format, 0 | 2 | 3) {
            return Err(None);
        }
        let (table, index) = (bits(i, 8, 4), bits(i, 12, 8));
        if table >= 12 || w == 0 || h == 0 {
            return Err(None);
        }
        let key = (self.resources.0, table, index);
        let attr = match env.attributes.get(&key) {
            Some(a) => *a,
            None => {
                let a = self.resources.attribute_desc(env.m, &env.ram, table, index).map_err(memory)?;
                env.attributes.insert(key, a);
                a
            }
        };
        let (m, ram, tlb) = (env.m, &*env.ram, &mut *env.tlb);
        let mut corners = [[0f32; 4]; 4];
        for (k, c) in corners.iter_mut().enumerate() {
            let (v, float) = attr.fetch(|a, out| tlb.read(m, ram, a, out), k as u32, 0).map_err(memory)?;
            if !float {
                return Err(None);
            }
            *c = v.map(f32::from_bits);
        }
        let u = (px as f32 + 0.5) / w as f32;
        let t = (py as f32 + 0.5) / h as f32;
        let values: [u32; 4] = std::array::from_fn(|n| {
            let top = corners[0][n] * (1. - u) + corners[1][n] * u;
            let bottom = corners[2][n] * (1. - u) + corners[3][n] * u;
            (top * (1. - t) + bottom * t).to_bits()
        });
        let r = self.staging(i, count).ok_or(None)?;
        write_values(self, r, &values[..components], if format == 0 { 2 } else { format }, true)?;
        Ok(Next::Continue)
    }

    pub(super) fn lea_attr(&mut self, _env: &mut Env, _i: u64) -> R<Next> {
        Err(None)
    }
    pub(super) fn atomic(&mut self, _env: &mut Env, _i: u64) -> R<Next> {
        Err(None)
    }

    /// LD_BUFFER: load from a buffer descriptor (UBOs and SSBOs) with bounds checking;
    /// out-of-bounds loads return zero.
    pub(super) fn ld_buffer(&mut self, env: &mut Env, i: u64) -> R<Next> {
        let size = bits(i, 27, 3);
        if bits(i, 46, 2) != 2 {
            return Err(None);
        }
        let o = Ops {
            s: self,
            i,
            uniforms: env.uniforms,
        };
        let offset = o.raw(0)? as u64;
        let handle = o.raw(1)?;
        let bytes: usize = [1, 2, 3, 4, 6, 8, 12, 16][size as usize];
        let (base, len) = self
            .resources
            .buffer(env.m, &env.ram, handle >> 24, handle & 0xff_ffff)
            .map_err(memory)?;
        let mut data = [0u8; 16];
        // Out of the buffer (or past the top of the address space): zeros.
        if let Some(at) = base.checked_add(offset).filter(|_| offset + bytes as u64 <= len) {
            env.read(at, &mut data[..bytes])
                .map_err(|f| Some(super::Error::Memory(f)))?;
        }
        self.load_lanes(i, size, &data, bits(i, 33, 3) as usize)?;
        Ok(Next::Continue)
    }
    pub(super) fn lea_buffer(&mut self, env: &mut Env, i: u64) -> R<Next> {
        if bits(i, 46, 2) != 2 || bits(i, 33, 3) != 2 {
            return Err(None);
        }
        let o = Ops {
            s: self,
            i,
            uniforms: env.uniforms,
        };
        let offset = o.raw(0)? as u64;
        let handle = o.raw(1)?;
        let (base, _) = self
            .resources
            .buffer(env.m, &env.ram, handle >> 24, handle & 0xff_ffff)
            .map_err(memory)?;
        let r = self.staging(i, 2).ok_or(None)?;
        let a = base.wrapping_add(offset);
        self.regs[r] = a as u32;
        self.regs[r + 1] = (a >> 32) as u32;
        Ok(Next::Continue)
    }

    /// BLEND: hand the colour to the blender of a render target. Fixed-function and
    /// opaque descriptors finish the thread (end flow) or skip the blend-shader call.
    pub(super) fn blend(&mut self, i: u64, u: &[u32]) -> R<Next> {
        self.fragment.as_ref().ok_or(None)?;
        let count = bits(i, 33, 3) as usize;
        let descriptor = self.source64(i, 0, u).ok_or(None)?;
        let components = bits(i, 28, 2) as usize + 1;
        // An auto register format takes the blend descriptor's conversion's (Register
        // File Format F16, F32, I32, U32, I16, U16) when its width matches the staging
        // count; otherwise the registers are passed on as 32-bit values.
        let format = match bits(i, 24, 3) {
            0 => {
                let f = match (descriptor >> 56) & 7 {
                    0 => 3,
                    1 => 2,
                    2 => 4,
                    3 => 6,
                    4 => 5,
                    5 => 7,
                    _ => 0,
                };
                let half = matches!(f, 3 | 5 | 7);
                if f != 0 && count == if half { components.div_ceil(2) } else { components } {
                    f
                } else {
                    0
                }
            }
            f => f,
        };
        let wide = !matches!(format, 3 | 5 | 7);
        if bits(i, 46, 2) != 1
            || count
                != if wide {
                    components
                } else {
                    components.div_ceil(2)
                }
        {
            return Err(None);
        }
        let r = self.staging(i, count).ok_or(None)?;
        // A 64-bit source takes two source slots: coverage is source byte 2.
        let o = Ops {
            s: self,
            i,
            uniforms: u,
        };
        let coverage = o.raw(2)?;
        let mode = descriptor & 3;
        if mode == 0 {
            return Err(None); // blend shaders are not modelled
        }
        // The render target index comes from the special FAU blend descriptor slot.
        let src = bits(i, 0, 8);
        let rt = if src >= 0xe0 && bits(i, 57, 2) == 0 && (((src & 63) - 32) >> 1) >= 8 {
            (((src & 63) - 32) >> 1) as usize - 8
        } else {
            ((descriptor >> 16) & 15) as usize
        };
        if rt >= 8 {
            return Err(None);
        }
        let mut values = [0u32; 4];
        for (n, v) in values.iter_mut().enumerate().take(components) {
            *v = if wide {
                self.regs[r + n]
            } else {
                let h = (self.regs[r + n / 2] >> (16 * (n & 1))) & 0xffff;
                match format {
                    3 => half(h as u16).to_bits(),
                    5 => sext(h, 16) as u32,
                    _ => h,
                }
            };
        }
        if components < 4 {
            values[3] = if matches!(format, 2 | 3 | 0) {
                1f32.to_bits()
            } else {
                1
            };
        }
        let f = self.fragment.as_mut().unwrap();
        if f.active && coverage & 1 != 0 && mode != 3 {
            if matches!(format, 0 | 2 | 3) {
                f.color[rt] = Some(values.map(f32::from_bits));
            } else {
                f.raw[rt] = Some(values);
            }
        }
        if bits(i, 59, 4) == 15 {
            return Ok(Next::End);
        }
        // Fixed-function: branch over the blend-shader call sequence.
        let target = bits(i, 8, 8) as u64;
        Ok(Next::Jump(self.pc.wrapping_add(8).wrapping_add(target * 8)))
    }

    /// TEX_SINGLE, TEX_FETCH, TEX_GATHER, TEX_GRADIENT and VAR_TEX_SINGLE.
    pub(super) fn texture(&mut self, env: &mut Env, i: u64, quad: Option<&QuadView>) -> R<Next> {
        let op = bits(i, 48, 9);
        let u = env.uniforms;
        // Reserved bit 9, the secondary width (TEX_DUAL only) and reserved LOD modes.
        if i & (1 << 9) != 0
            || i & (1 << 47) != 0 && op != 0x138
            || matches!(bits(i, 13, 3), 2 | 3 | 7) && matches!(op, 0x128 | 0x138)
        {
            return Err(None);
        }
        let handle = self.source64(i, 0, u).ok_or(None)?;
        let wide_indices = i & (1 << 8) != 0;
        let (texture, sampler) = if wide_indices {
            let t = (handle >> 32) as u32;
            let s = handle as u32;
            ((t >> 24, t & 0xff_ffff), (s >> 24, s & 0xff_ffff))
        } else {
            let h = handle as u32;
            let fold = |t: u32| if t >= 12 { t - 12 + 60 } else { t };
            (
                (fold(h >> 27), (h >> 16) & 0x3ff),
                (fold((h >> 11) & 31), h & 0x3ff),
            )
        };
        if !wide_indices && handle >> 32 != 0 {
            return Err(None);
        }
        let dim = bits(i, 28, 2);
        let array = i & (1 << 10) != 0;
        let shadow = i & (1 << 12) != 0 && op != 0x125;
        let offsets = i & (1 << 11) != 0 && op != 0x138;
        let var_tex = op == 0x138;
        // Staging layout (compacted): coordinates, array, shadow, offsets/ms, LOD, gradient.
        let (read_reg, read_count, write_reg, write_count) = if var_tex {
            (0, 0, bits(i, 40, 6) as usize, bits(i, 36, 3) as usize + 1)
        } else {
            (
                bits(i, 40, 6) as usize,
                bits(i, 33, 3) as usize,
                bits(i, 16, 6) as usize,
                bits(i, 36, 3) as usize + 1,
            )
        };
        let lod_mode = if op == 0x128 || var_tex {
            bits(i, 13, 3)
        } else {
            0
        };
        let coords_n = match dim {
            0 => 1,
            1 | 3 => 2,
            _ => 3,
        };
        let mut sr = [0u32; 12];
        let mut coords = [0u32; 4];
        if var_tex {
            let interp = self.interpolation.as_ref().ok_or(None)?;
            // The image source is 64-bit, so the varying offset is source byte 2.
            let offset = Ops {
                s: self,
                i,
                uniforms: u,
            }
            .raw(2)? as usize;
            let v = interp.load(offset, coords_n, 4).ok_or(None)?;
            for n in 0..coords_n {
                coords[n] = v[n].to_bits();
            }
        } else {
            if read_count > 8 || !self.registers_available(read_reg, read_count.max(1)) {
                return Err(None);
            }
            sr[..read_count].copy_from_slice(&self.regs[read_reg..read_reg + read_count]);
            coords[..coords_n.min(read_count)].copy_from_slice(&sr[..coords_n.min(read_count)]);
        }
        let mut k = if var_tex { 0 } else { coords_n };
        let mut take = |sr: &[u32; 12]| {
            let v = sr[k];
            k += 1;
            v
        };
        let layer = if array && !var_tex {
            Some(take(&sr))
        } else {
            None
        };
        let reference = if shadow && !var_tex {
            Some(f32::from_bits(take(&sr)))
        } else {
            None
        };
        let offset_ms = if offsets { take(&sr) } else { 0 };
        let explicit = if !var_tex && matches!(lod_mode, 4 | 5) {
            Some(take(&sr))
        } else {
            None
        };
        if !var_tex && k > read_count {
            return Err(None);
        }
        let fixed88 = |v: u32| (v as u16 as i16) as f32 / 256.;
        let lod = match (op, lod_mode) {
            (0x125, _) => Lod::Fetch(offset_ms >> 24),
            (0x129, _) | (_, 0) => Lod::Explicit(0.),
            // (VAR_TEX has no staging LOD: an explicit mode there is reserved)
            (_, 4) => Lod::Explicit(fixed88(explicit.ok_or(None)?)),
            (_, 1 | 5) => {
                let bias = explicit.map_or(0., fixed88);
                let (mut dx, mut dy) = ([0f32; 3], [0f32; 3]);
                if let Some(q) = quad {
                    // A cube's X carries the face in bits 29-31 over s (see the sampler):
                    // differentiate s alone, as the sampler will read it.
                    let coord = |lane: usize, n: usize| {
                        let v = q.tex[lane][n];
                        if dim == 3 && n == 0 {
                            f32::from_bits((v.to_bits() & 0x1fff_ffff) | 0x2000_0000)
                        } else {
                            v
                        }
                    };
                    let x = q.lane ^ 1;
                    let y = q.lane ^ 2;
                    let sx = if q.lane & 1 == 0 { 1. } else { -1. };
                    let sy = if q.lane & 2 == 0 { 1. } else { -1. };
                    for n in 0..3 {
                        if q.active[x] {
                            dx[n] = (coord(x, n) - coord(q.lane, n)) * sx;
                        }
                        if q.active[y] {
                            dy[n] = (coord(y, n) - coord(q.lane, n)) * sy;
                        }
                    }
                }
                Lod::Computed { dx, dy, bias }
            }
            _ => return Err(None),
        };
        if op == 0x12a {
            return Err(None); // explicit gradient descriptors are not modelled yet
        }
        let request = Request {
            texture,
            sampler,
            dim,
            coords,
            integer: op == 0x125,
            layer,
            reference,
            offset: [
                sext(offset_ms & 255, 8),
                sext((offset_ms >> 8) & 255, 8),
                sext((offset_ms >> 16) & 255, 8),
            ],
            lod,
            gather: (op == 0x129).then(|| bits(i, 14, 2)),
        };
        let (texel, float) =
            crate::gpu_texture::sample(env, self.resources, &request).map_err(memory)?;
        let mask = if var_tex { 15 } else { bits(i, 22, 4) };
        let values: Vec<u32> = (0..4)
            .filter(|n| mask & (1 << n) != 0)
            .map(|n| texel[n])
            .collect();
        let wide = if var_tex {
            bits(i, 24, 2) & 1 == 1
        } else {
            i & (1 << 46) != 0
        };
        let format = match (wide, bits(i, 26, 2)) {
            (true, 0 | 1) => 2,
            (false, 0 | 1) => 3,
            (true, 2) => 6,
            (false, 2) => 7,
            (true, _) => 4,
            (false, _) => 5,
        };
        let needed = if wide {
            values.len()
        } else {
            values.len().div_ceil(2)
        };
        if needed != write_count || needed > 1 && write_reg & 1 != 0 {
            return Err(None);
        }
        write_values(self, write_reg, &values, format, float)?;
        Ok(Next::Continue)
    }
}

/// Texture coordinates of every executing lane, for implicit-LOD derivatives.
pub(super) fn quad_coords(
    lanes: &[Shader; 4],
    active: &[bool; 4],
    i: u64,
    u: &[u32],
) -> [[f32; 4]; 4] {
    let op = bits(i, 48, 9);
    let n = match bits(i, 28, 2) {
        0 => 1,
        1 | 3 => 2,
        _ => 3,
    };
    std::array::from_fn(|lane| {
        let mut c = [0f32; 4];
        if !active[lane] {
            return c;
        }
        let s = &lanes[lane];
        if op == 0x138 {
            let offset = Ops { s, i, uniforms: u }.raw(2).ok();
            if let (Some(interp), Some(offset)) = (s.interpolation.as_ref(), offset) {
                if let Some(v) = interp.load(offset as usize, n, 4) {
                    c[..n].copy_from_slice(&v[..n]);
                }
            }
        } else {
            let r = bits(i, 40, 6) as usize;
            for k in 0..n {
                c[k] = f32::from_bits(s.regs[(r + k).min(63)]);
            }
        }
        c
    })
}
