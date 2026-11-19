//! Synchronous Valhall vertex/varying/tiler execution. Opaque polygon lists live on
//! the host, keyed by VM and tiler context; shaders still write GPU-mapped data.
use crate::{
    gpu_cs::Stop,
    gpu_mmu::{Access, AddressSpace},
    gpu_resources::{Buffer, Resources},
    gpu_shader::{RegisterAllocation, Shader},
};
use std::{collections::BTreeMap, sync::Arc};
type Key = (u64, u64, u64);
type Owner = (u32, u32);
#[derive(Default)]
struct Heap {
    owner: Owner,
    started: u32,
    completed: u32,
    fragments: u32,
}
pub struct Draw {
    pub regs: [u32; 256],
    pub instruction: u64,
    pub triangles: Vec<[[f32; 4]; 3]>,
    pub varyings: Option<(usize, Arc<[u8]>)>,
}
pub struct Frame {
    pub owner: Owner,
    pub width: u32,
    pub height: u32,
    pub draws: Vec<Draw>,
    vertices: usize,
}
#[derive(Default)]
pub struct Tiler {
    frames: BTreeMap<Key, Frame>,
    vertices: usize,
    heaps: BTreeMap<Key, Heap>,
}
fn word(b: &[u8], n: usize) -> u32 {
    u32::from_le_bytes(b[n * 4..n * 4 + 4].try_into().unwrap())
}
fn pair(b: &[u8], n: usize) -> u64 {
    word(b, n) as u64 | ((word(b, n + 1) as u64) << 32)
}
fn reg(r: &[u32; 256], n: usize) -> u64 {
    r[n] as u64 | ((r[n + 1] as u64) << 32)
}
fn key(m: &AddressSpace, va: u64) -> Key {
    (m.transtab, m.transcfg, va)
}
fn unsupported(opcode: u8) -> Stop {
    Stop::Unsupported { pc: 0, opcode }
}
impl Tiler {
    pub fn clear_group(&mut self, group: u32) {
        let vertices = &mut self.vertices;
        self.frames.retain(|_, f| {
            if f.owner.0 == group {
                *vertices -= f.vertices;
                false
            } else {
                true
            }
        });
        self.heaps.retain(|_, h| h.owner.0 != group);
    }
    pub fn clear_queue(&mut self, owner: Owner) {
        let vertices = &mut self.vertices;
        self.frames.retain(|_, f| {
            if f.owner == owner {
                *vertices -= f.vertices;
                false
            } else {
                true
            }
        });
        self.heaps.retain(|_, h| h.owner != owner);
    }
    pub fn heap_operation(
        &mut self,
        m: &AddressSpace,
        ram: &[u8],
        owner: Owner,
        heap: u64,
        operation: u32,
    ) -> Result<(), Stop> {
        if !matches!(operation, 0 | 1 | 3) {
            return Err(unsupported(49));
        }
        m.chunks(ram, heap, 32, Access::Write, true)
            .map_err(Stop::Memory)?;
        let k = key(m, heap);
        if self.heaps.len() >= 64 && !self.heaps.contains_key(&k) {
            return Err(unsupported(49));
        }
        let h = self.heaps.entry(k).or_insert(Heap {
            owner,
            ..Heap::default()
        });
        if h.owner != owner {
            return Err(unsupported(49));
        }
        match operation {
            0 => h.started = h.started.wrapping_add(1),
            1 if h.started.wrapping_sub(h.completed) != 0 => {
                h.completed = h.completed.wrapping_add(1)
            }
            3 if h.completed.wrapping_sub(h.fragments) != 0 => {
                h.fragments = h.fragments.wrapping_add(1)
            }
            _ => return Err(unsupported(49)),
        }
        Ok(())
    }
    pub fn fragment(
        &mut self,
        m: &AddressSpace,
        ram: &mut [u8],
        r: &[u32; 256],
        owner: Owner,
    ) -> Result<(), Stop> {
        let fbd = reg(r, 40) & !63;
        let mut data = [0; 8];
        m.read(
            ram,
            fbd.checked_add(56).ok_or(Stop::InvalidStream)?,
            &mut data,
            true,
        )
        .map_err(Stop::Memory)?;
        let ctx = u64::from_le_bytes(data);
        if ctx == 0 {
            return crate::gpu_render::fragment(m, ram, r);
        }
        let k = key(m, ctx);
        let frame = self.frames.get(&k).ok_or(unsupported(7))?;
        if frame.owner != owner {
            return Err(unsupported(7));
        }
        crate::gpu_render::fragment_frame(m, ram, r, Some(frame))?;
        self.vertices -= self.frames.remove(&k).unwrap().vertices;
        Ok(())
    }
    pub fn idvs(
        &mut self,
        m: &AddressSpace,
        ram: &mut [u8],
        r: &[u32; 256],
        instruction: u64,
        owner: Owner,
    ) -> Result<(), Stop> {
        // Triangle lists, basic FIFO,
        // no draw IDs, multiview or primitive restart.
        let flags = r[56] | instruction as u32;
        if std::env::var_os("SBC_MALI_TRACE").is_some() {
            eprintln!("[mali] IDVS flags={flags:08x} registers={:x?}", &r[32..60]);
        }
        if instruction & 0x00ff_ffa5_0000_0000 != 0
            || flags & !0x0047_830f != 0
            || flags & 15 != 8
            || r[32] != 0
            || r[35] != 0
            || r[37] != 0
            || r[38] != 0
            || r[48] > 256
            || r[48] % 16 != 0
            || (r[48] != 0) != (flags & (1 << 18) != 0)
            || r[33] == 0
            || r[33] > 4096
            || r[33] % 3 != 0
        {
            return Err(unsupported(6));
        }
        let vertices = draw_vertices(m, ram, r, flags)?;
        let vertex_count = vertices.len() * r[34] as usize;
        let ctx = reg(r, 40);
        if ctx & 63 != 0 {
            return Err(Stop::InvalidStream);
        }
        let k = key(m, ctx);
        if self.frames.len() >= 64 && !self.frames.contains_key(&k) {
            return Err(unsupported(6));
        }
        if let Some(frame) = self.frames.get(&k) {
            if frame.owner != owner || frame.draws.len() >= 1024 {
                return Err(unsupported(6));
            }
        }
        if self.vertices + vertex_count > 65536 {
            return Err(unsupported(6));
        }
        let mut tiler = [0; 128];
        m.read(ram, ctx, &mut tiler, true).map_err(Stop::Memory)?;
        let width = (word(&tiler, 3) & 65535) + 1;
        let height = (word(&tiler, 3) >> 16) + 1;
        if width > 4096
            || height > 4096
            || pair(&tiler, 0) != 0
            || word(&tiler, 2) & !0x0006_1fff != 0
            || word(&tiler, 4) != 0
            || word(&tiler, 5) != 0
        {
            return Err(unsupported(6));
        }
        if let Some(frame) = self.frames.get(&k) {
            if frame.width != width || frame.height != height {
                return Err(unsupported(6));
            }
        }
        let mut heap = [0; 32];
        m.read(ram, pair(&tiler, 6), &mut heap, true)
            .map_err(Stop::Memory)?;
        let base = pair(&heap, 2);
        let bottom = pair(&heap, 4);
        let top = pair(&heap, 6);
        if word(&heap, 0) & 255 != 0x29
            || base & 4095 != 0
            || bottom < base.checked_add(64).ok_or(Stop::InvalidStream)?
            || bottom > top
            || top.checked_sub(base) != Some(word(&heap, 1) as u64)
        {
            return Err(unsupported(6));
        }
        m.chunks(ram, bottom, 64, Access::Write, true)
            .map_err(Stop::Memory)?;
        let output = pair(&tiler, 8) & !4095;
        let capacity = ((word(&tiler, 8) & 4095) as u64) << 12;
        let bytes = vertex_count * 16;
        let vary_start = (bytes + 63) & !63;
        let vary_bytes = vertex_count * r[48] as usize;
        let required = if vary_bytes == 0 {
            bytes
        } else {
            vary_start + vary_bytes
        };
        if output == 0 || required as u64 > capacity {
            return Err(unsupported(6));
        }
        m.chunks(ram, output, required, Access::Write, true)
            .map_err(Stop::Memory)?;
        let mut budget = 1 << 22;
        vertex_stage(
            m,
            ram,
            Stage {
                spd: reg(r, 16),
                fau: reg(r, 8),
                resources: reg(r, 0),
                output: Buffer {
                    address: output,
                    size: bytes as u64,
                    stride: 4,
                },
            },
            &vertices,
            r[34],
            &mut budget,
        )?;
        let varyings = if vary_bytes != 0 {
            let address = output
                .checked_add(vary_start as u64)
                .ok_or(Stop::InvalidStream)?;
            vertex_stage(
                m,
                ram,
                Stage {
                    spd: reg(r, 18),
                    fau: reg(r, if instruction & (1 << 36) != 0 { 10 } else { 8 }),
                    resources: reg(r, if instruction & (1 << 35) != 0 { 2 } else { 0 }),
                    output: Buffer {
                        address,
                        size: vary_bytes as u64,
                        stride: r[48] / 4,
                    },
                },
                &vertices,
                r[34],
                &mut budget,
            )?;
            let mut data = vec![0; vary_bytes];
            m.read(ram, address, &mut data, true)
                .map_err(Stop::Memory)?;
            Some((r[48] as usize, Arc::from(data)))
        } else {
            None
        };
        let mut data = vec![0; bytes];
        m.read(ram, output, &mut data, true).map_err(Stop::Memory)?;
        let mut vertices = Vec::with_capacity(vertex_count);
        for v in data.chunks_exact(16) {
            let p = [
                f32::from_bits(word(v, 0)),
                f32::from_bits(word(v, 1)),
                f32::from_bits(word(v, 2)),
                f32::from_bits(word(v, 3)),
            ];
            if p.iter().any(|f| !f.is_finite())
                || p[3] != 1.0
                || p[0].abs() > 1048576.0
                || p[1].abs() > 1048576.0
                || !(0.0..=1.0).contains(&p[2])
            {
                return Err(unsupported(6));
            }
            vertices.push(p);
        }
        if std::env::var_os("SBC_MALI_TRACE").is_some() {
            eprintln!("[mali] IDVS positions={vertices:?}");
        }
        let triangles = vertices
            .chunks_exact(3)
            .map(|v| [v[0], v[1], v[2]])
            .collect();
        let frame = self.frames.entry(k).or_insert_with(|| Frame {
            owner,
            width,
            height,
            draws: Vec::new(),
            vertices: 0,
        });
        frame.draws.push(Draw {
            regs: *r,
            instruction,
            triangles,
            varyings,
        });
        frame.vertices += vertex_count;
        self.vertices += vertex_count;
        Ok(())
    }
}

struct Stage {
    spd: u64,
    fau: u64,
    resources: u64,
    output: Buffer,
}
fn vertex_stage(
    m: &AddressSpace,
    ram: &mut [u8],
    stage: Stage,
    vertices: &[u32],
    instances: u32,
    budget: &mut usize,
) -> Result<(), Stop> {
    let mut spd = [0; 32];
    m.read(ram, stage.spd, &mut spd, true)
        .map_err(Stop::Memory)?;
    if word(&spd, 0) & !((3 << 30) | (3 << 8)) != 0x38
        || word(&spd, 1) & !0xf800 != 0
        || spd[16..].iter().any(|&v| v != 0)
    {
        return Err(unsupported(6));
    }
    let allocation = match word(&spd, 0) >> 30 {
        0 => RegisterAllocation::Registers64,
        2 => RegisterAllocation::Registers32,
        _ => return Err(unsupported(6)),
    };
    let binary = pair(&spd, 2);
    if binary & 7 != 0 {
        return Err(Stop::InvalidStream);
    }
    let fau = stage.fau;
    let count = (fau >> 56) as usize;
    if count > 128 || fau & 0x00ff_0000_0000_0000 != 0 {
        return Err(unsupported(6));
    }
    let mut data = vec![0; count * 8];
    m.read(ram, fau & 0xffff_ffff_ffff, &mut data, true)
        .map_err(Stop::Memory)?;
    let uniforms: Vec<u32> = data
        .chunks_exact(4)
        .map(|v| u32::from_le_bytes(v.try_into().unwrap()))
        .collect();
    for invocation in 0..vertices.len() * instances as usize {
        let vertex = vertices[invocation % vertices.len()];
        let instance = (invocation / vertices.len()) as u32;
        let mut shader = Shader::with_allocation(binary, allocation);
        shader.resources = Resources(stage.resources);
        shader.position_output = Some(stage.output);
        let values = [invocation as u32 * 4, vertex, instance, 0, 0];
        for (n, &value) in values.iter().enumerate() {
            if word(&spd, 1) & (1 << (11 + n)) != 0 {
                shader.regs[59 + n] = value;
            }
        }
        shader
            .run(m, ram, &uniforms, (*budget).min(4096))
            .map_err(Stop::Shader)?;
        *budget -= shader.instructions as usize;
    }
    Ok(())
}

/// Decode element IDs before executing shaders. Hardware uses unsigned vertex
/// IDs with a signed base-vertex addition; attribute bounds checks handle IDs
/// outside a buffer. Internal position IDs remain dense and unique per instance.
fn draw_vertices(
    m: &AddressSpace,
    ram: &[u8],
    r: &[u32; 256],
    flags: u32,
) -> Result<Vec<u32>, Stop> {
    if r[34] == 0 || (r[33] as u64) * (r[34] as u64) > 4096 {
        return Err(unsupported(6));
    }
    let index_type = (flags >> 8) & 7;
    let mut ids = Vec::with_capacity(r[33] as usize);
    if index_type == 0 {
        if r[39] != 0 {
            return Err(unsupported(6));
        }
        ids.extend((0..r[33]).map(|i| i.wrapping_add(r[36])));
    } else {
        let width = match index_type {
            1 => 1,
            2 => 2,
            3 => 4,
            _ => return Err(unsupported(6)),
        };
        let bytes = r[33] as usize * width;
        let address = reg(r, 54);
        if address == 0 || address % width as u64 != 0 || bytes > r[39] as usize {
            return Err(Stop::InvalidStream);
        }
        let mut indices = vec![0; bytes];
        m.read(ram, address, &mut indices, true)
            .map_err(Stop::Memory)?;
        for value in indices.chunks_exact(width) {
            let mut word = [0; 4];
            word[..width].copy_from_slice(value);
            ids.push(u32::from_le_bytes(word).wrapping_add(r[36]));
        }
    }
    Ok(ids)
}

#[cfg(test)]
mod tests {
    use super::*;
    fn fixture() -> (AddressSpace, Vec<u8>, [u32; 256]) {
        let m = AddressSpace {
            transtab: 0,
            transcfg: 2,
        };
        let mut ram = vec![0; 0x20000];
        let mut put = |at: usize, v: u64| ram[at..at + 8].copy_from_slice(&v.to_le_bytes());
        put(0x1010, 0x2000 | (1 << 56));
        put(0x1018, 32);
        put(0x1020, 0x3000 | (1 << 56));
        put(0x1028, 32);
        put(0x2000, (0x115 | (((175u64 << 12) | 16) << 10)) | (2 << 32));
        put(0x2010, 8);
        put(0x3000, 0x19 | (24 << 32));
        put(0x3008, 0x4000);
        put(0x5000, 0x80000138 | (0x3800u64 << 32));
        put(0x5008, 0x6000);
        put(
            0x7000,
            8f32.to_bits() as u64 | ((8f32.to_bits() as u64) << 32),
        );
        put(
            0x7008,
            8f32.to_bits() as u64 | ((8f32.to_bits() as u64) << 32),
        );
        put(0x7010, 0.5f32.to_bits() as u64);
        put(0x8008, 0x000f000fu64 << 32);
        put(0x8018, 0x9000);
        put(0x8020, 0x10010);
        put(0x9000, 0x29 | (4096 << 32));
        put(0x9008, 0xa000);
        put(0x9010, 0xa040);
        put(0x9018, 0xb000);
        let code = [
            0x0866800412017d7cu64,
            0x00b2c00000818040,
            0x00b2c10000838241,
            0x0091c20000000084,
            0x0091c300000000d0,
            0x105e840440000d7b,
            0x78614008b9000044,
        ];
        for (n, i) in code.into_iter().enumerate() {
            ram[0x6000 + n * 8..0x6008 + n * 8].copy_from_slice(&i.to_le_bytes());
        }
        for (n, f) in [-0.75f32, -0.75, 0.75, -0.75, 0., 0.75]
            .into_iter()
            .enumerate()
        {
            ram[0x4000 + n * 4..0x4004 + n * 4].copy_from_slice(&f.to_le_bytes());
        }
        let mut r = [0; 256];
        r[0] = 0x1003;
        r[8] = 0x7000;
        r[9] = 3 << 24;
        r[16] = 0x5000;
        r[33] = 3;
        r[34] = 1;
        r[40] = 0x8000;
        r[56] = 0x438000;
        (m, ram, r)
    }
    #[test]
    fn idvs_executes_native_positions_and_keeps_vm_and_queue_ownership() {
        let (m, mut ram, r) = fixture();
        let mut t = Tiler::default();
        t.idvs(&m, &mut ram, &r, 0x0600004200000008, (1, 2))
            .unwrap();
        assert_eq!(
            t.frames[&key(&m, 0x8000)].draws[0].triangles[0],
            [[2., 2., 0.5, 1.], [14., 2., 0.5, 1.], [8., 14., 0.5, 1.]]
        );
        assert!(t
            .idvs(&m, &mut ram, &r, 0x0600004200000008, (2, 0))
            .is_err());
        let m2 = AddressSpace { transtab: 1, ..m };
        t.idvs(&m2, &mut ram, &r, 0x0600004200000008, (2, 0))
            .unwrap();
        assert_eq!(t.frames.len(), 2);
        assert_eq!(t.vertices, 6);
        t.clear_queue((1, 2));
        assert_eq!(t.frames.len(), 1);
        t.clear_group(2);
        assert!(t.frames.is_empty());
        assert_eq!(t.vertices, 0);
    }
    #[test]
    fn unsupported_geometry_and_shader_faults_do_not_publish_primitives() {
        let (m, mut ram, mut r) = fixture();
        let mut t = Tiler::default();
        r[56] |= 1 << 19;
        assert!(t
            .idvs(&m, &mut ram, &r, 0x0600004200000008, (0, 0))
            .is_err());
        assert!(t.frames.is_empty());
        r[56] &= !(1 << 19);
        ram[0x6000..0x6008].copy_from_slice(&u64::MAX.to_le_bytes());
        assert!(matches!(
            t.idvs(&m, &mut ram, &r, 0x0600004200000008, (0, 0)),
            Err(Stop::Shader(_))
        ));
        assert!(t.frames.is_empty());
    }
    #[test]
    fn indexed_widths_signed_base_and_repeated_elements_execute_native_shader() {
        for (kind, width) in [(1, 1), (2, 2), (3, 4)] {
            let (m, mut ram, mut r) = fixture();
            // Repeated indices require separate internal position slots. A
            // signed base of -1 maps the source IDs to vertices 2, 0, 1 twice.
            for (n, value) in [3u32, 1, 2, 3, 1, 2].iter().enumerate() {
                ram[0xc000 + n * width..0xc000 + (n + 1) * width]
                    .copy_from_slice(&value.to_le_bytes()[..width]);
            }
            r[33] = 6;
            r[36] = (-1i32) as u32;
            r[39] = (6 * width) as u32;
            r[54] = 0xc000;
            let mut t = Tiler::default();
            t.idvs(&m, &mut ram, &r, 0x0600004200000008 | (kind << 8), (0, 0))
                .unwrap();
            let frame = &t.frames[&key(&m, 0x8000)];
            let expected = [[8., 14., 0.5, 1.], [2., 2., 0.5, 1.], [14., 2., 0.5, 1.]];
            assert_eq!(frame.draws[0].triangles, vec![expected, expected]);
            assert_eq!(t.vertices, 6);
        }
    }
    #[test]
    fn array_first_ignores_stale_index_buffer() {
        let (m, mut ram, mut r) = fixture();
        // Start at vertex one and place the original triangle after a dummy.
        ram.copy_within(0x4000..0x4018, 0x4008);
        ram[0x3004..0x3008].copy_from_slice(&32u32.to_le_bytes());
        r[36] = 1;
        r[54] = u32::MAX;
        r[55] = u32::MAX;
        let mut t = Tiler::default();
        t.idvs(&m, &mut ram, &r, 0x0600004200000008, (0, 0))
            .unwrap();
        assert_eq!(
            t.frames[&key(&m, 0x8000)].draws[0].triangles[0],
            [[2., 2., 0.5, 1.], [14., 2., 0.5, 1.], [8., 14., 0.5, 1.]]
        );
    }
    #[test]
    fn instance_ids_select_pot_attributes_and_unique_position_slots() {
        let (m, mut ram, mut r) = fixture();
        // Feed the position attribute per instance with divisor four. Every
        // instance is a degenerate triangle, but its three outputs are distinct
        // from other instances' slots and expose both preloaded IDs.
        ram[0x2000..0x2004].copy_from_slice(&(0x25u32 | (((175 << 12) | 16) << 10)).to_le_bytes());
        ram[0x2004..0x2008].copy_from_slice(&(2u32 | 64 | (2 << 24)).to_le_bytes());
        r[34] = 9;
        let mut t = Tiler::default();
        t.idvs(&m, &mut ram, &r, 0x0600004200000008, (0, 0))
            .unwrap();
        let frame = &t.frames[&key(&m, 0x8000)];
        for (instance, triangle) in frame.draws[0].triangles.iter().enumerate() {
            let point = [[2., 2., 0.5, 1.], [14., 2., 0.5, 1.], [8., 14., 0.5, 1.]][instance / 4];
            assert_eq!(*triangle, [point; 3]);
        }
        assert_eq!(frame.vertices, 27);
        t.clear_queue((0, 0));
        assert_eq!(t.vertices, 0);
    }
    #[test]
    fn invalid_index_ranges_and_instance_counts_publish_nothing() {
        for (address, size, instances) in [
            (0xc000u64, 11, 1), // one byte short
            (0xc002, 12, 1),    // unaligned uint32
            (0x1fffc, 12, 1),   // aligned but outside GPU RAM
            (u64::MAX - 3, 12, 1),
            (0xc000, 12, 0),
            (0xc000, 12, 1366), // aggregate invocation limit
            (0xc000, 12, u32::MAX),
        ] {
            let (m, mut ram, mut r) = fixture();
            r[34] = instances;
            r[39] = size;
            r[54] = address as u32;
            r[55] = (address >> 32) as u32;
            ram[0x10000..0x10100].fill(0xa5);
            let mut t = Tiler::default();
            assert!(t
                .idvs(&m, &mut ram, &r, 0x0600004200000308, (0, 0))
                .is_err());
            assert!(t.frames.is_empty());
            assert_eq!(t.vertices, 0);
            assert!(ram[0x10000..0x10100].iter().all(|&v| v == 0xa5));
        }
    }
    #[test]
    fn position_capacity_covers_all_instances() {
        let (m, mut ram, mut r) = fixture();
        ram[0x8020..0x8028].copy_from_slice(&0x10001u64.to_le_bytes());
        r[34] = 86; // 258 positions need 4128 bytes, exceeding one page.
        let mut t = Tiler::default();
        assert!(t
            .idvs(&m, &mut ram, &r, 0x0600004200000008, (0, 0))
            .is_err());
        assert!(t.frames.is_empty());
        // The aggregate boundary is accepted by the input decoder itself.
        r[34] = 1365;
        assert_eq!(draw_vertices(&m, &ram, &r, 8).unwrap().len(), 3);
    }
    fn with_varyings() -> (AddressSpace, Vec<u8>, [u32; 256]) {
        let (m, mut ram, mut r) = fixture();
        ram.copy_within(0x5000..0x5020, 0x5040);
        ram[0x5048..0x5050].copy_from_slice(&0x6100u64.to_le_bytes());
        let code = [
            0x0866800412017d7cu64,
            0x00b2c0000adada40,
            0x00b2c1000adada41,
            0x105e820440000d7b,
            0x78614004aa000042,
        ];
        for (n, i) in code.into_iter().enumerate() {
            ram[0x6100 + n * 8..0x6108 + n * 8].copy_from_slice(&i.to_le_bytes());
        }
        r[18] = 0x5040;
        r[48] = 16;
        (m, ram, r)
    }
    #[test]
    fn secondary_shader_stores_gpu_varyings_and_snapshots_each_draw() {
        let (m, mut ram, r) = with_varyings();
        let mut t = Tiler::default();
        t.idvs(&m, &mut ram, &r, 0x0600004200040008, (0, 0))
            .unwrap();
        let frame = &t.frames[&key(&m, 0x8000)];
        let (stride, data) = frame.draws[0].varyings.as_ref().unwrap();
        assert_eq!(*stride, 16);
        for (n, expected) in [[0.125f32, 0.125], [0.875, 0.125], [0.5, 0.875]]
            .iter()
            .enumerate()
        {
            assert_eq!(
                [
                    f32::from_bits(word(data, n * 4)),
                    f32::from_bits(word(data, n * 4 + 1))
                ],
                *expected
            );
        }
        assert_eq!(&ram[0x10040..0x10070], data.as_ref());
        let saved = data.clone();
        ram[0x4000..0x4004].copy_from_slice(&0f32.to_le_bytes());
        t.idvs(&m, &mut ram, &r, 0x0600004200040008, (0, 0))
            .unwrap();
        let draws = &t.frames[&key(&m, 0x8000)].draws;
        assert_eq!(draws[0].varyings.as_ref().unwrap().1, saved);
        assert_ne!(draws[1].varyings.as_ref().unwrap().1, saved);
        assert_eq!(t.vertices, 6);
    }
    #[test]
    fn secondary_fault_and_combined_geometry_capacity_publish_no_draw() {
        let (m, mut ram, mut r) = with_varyings();
        r[34] = 86;
        r[48] = 256;
        ram[0x10000..0x10100].fill(0xa5);
        let mut t = Tiler::default();
        assert!(t
            .idvs(&m, &mut ram, &r, 0x0600004200040008, (0, 0))
            .is_err());
        assert!(ram[0x10000..0x10100].iter().all(|&v| v == 0xa5));
        r[34] = 1;
        r[48] = 16;
        ram[0x6100..0x6108].copy_from_slice(&u64::MAX.to_le_bytes());
        assert!(matches!(
            t.idvs(&m, &mut ram, &r, 0x0600004200040008, (0, 0)),
            Err(Stop::Shader(_))
        ));
        assert!(t.frames.is_empty());
        assert_eq!(t.vertices, 0);
    }
    #[test]
    fn secondary_fau_selector_is_independent_of_position_uniforms() {
        let (m, mut ram, mut r) = with_varyings();
        let code = [
            0x0091c00000000080u64,
            0x0091c10000000081,
            0x105e820440000d7b,
            0x78614004aa000042,
        ];
        for (n, i) in code.into_iter().enumerate() {
            ram[0x6100 + n * 8..0x6108 + n * 8].copy_from_slice(&i.to_le_bytes());
        }
        ram[0x7200..0x7208].copy_from_slice(
            &(0.25f32.to_bits() as u64 | ((0.75f32.to_bits() as u64) << 32)).to_le_bytes(),
        );
        r[10] = 0x7200;
        r[11] = 1 << 24;
        let mut t = Tiler::default();
        t.idvs(&m, &mut ram, &r, 0x0600005200040008, (0, 0))
            .unwrap();
        let data = &t.frames[&key(&m, 0x8000)].draws[0]
            .varyings
            .as_ref()
            .unwrap()
            .1;
        for n in 0..3 {
            assert_eq!(word(data, n * 4), 0.25f32.to_bits());
            assert_eq!(word(data, n * 4 + 1), 0.75f32.to_bits());
        }
    }
    #[test]
    fn secondary_resource_selector_reads_its_own_attribute_table() {
        let (m, mut ram, mut r) = with_varyings();
        ram.copy_within(0x1000..0x1040, 0x1100);
        ram[0x1110..0x1118].copy_from_slice(&(0x2200u64 | (1 << 56)).to_le_bytes());
        ram[0x1120..0x1128].copy_from_slice(&(0x3200u64 | (1 << 56)).to_le_bytes());
        ram.copy_within(0x2000..0x2020, 0x2200);
        ram.copy_within(0x3000..0x3020, 0x3200);
        ram[0x3208..0x3210].copy_from_slice(&0x4200u64.to_le_bytes());
        for (n, value) in [1f32, 1., -1., -1., 0., 0.].iter().enumerate() {
            ram[0x4200 + n * 4..0x4204 + n * 4].copy_from_slice(&value.to_le_bytes());
        }
        r[2] = 0x1103;
        let mut t = Tiler::default();
        t.idvs(&m, &mut ram, &r, 0x0600004a00040008, (0, 0))
            .unwrap();
        let frame = &t.frames[&key(&m, 0x8000)];
        assert_eq!(frame.draws[0].triangles[0][0], [2., 2., 0.5, 1.]);
        let data = &frame.draws[0].varyings.as_ref().unwrap().1;
        for (n, expected) in [[1f32, 1.], [0., 0.], [0.5, 0.5]].iter().enumerate() {
            assert_eq!(
                [
                    f32::from_bits(word(data, n * 4)),
                    f32::from_bits(word(data, n * 4 + 1))
                ],
                *expected
            );
        }
    }
    #[test]
    fn secondary_instance_ids_and_slots_match_native_attribute_divisors() {
        let (m, mut ram, mut r) = with_varyings();
        ram[0x2000..0x2004].copy_from_slice(&(0x25u32 | (((175 << 12) | 16) << 10)).to_le_bytes());
        ram[0x2004..0x2008].copy_from_slice(&(2u32 | 64 | (2 << 24)).to_le_bytes());
        r[34] = 5;
        let mut t = Tiler::default();
        t.idvs(&m, &mut ram, &r, 0x0600004200040008, (0, 0))
            .unwrap();
        let data = &t.frames[&key(&m, 0x8000)].draws[0]
            .varyings
            .as_ref()
            .unwrap()
            .1;
        assert_eq!(data.len(), 15 * 16);
        for n in 0..15 {
            assert_eq!(
                word(data, n * 4),
                if n < 12 { 0.125f32 } else { 0.875f32 }.to_bits()
            );
            assert_eq!(word(data, n * 4 + 1), 0.125f32.to_bits());
        }
    }
    #[test]
    fn blended_writeback_is_ordered_and_a_late_fault_does_not_clear_the_target() {
        let (m, mut ram, mut r) = fixture();
        fn put(ram: &mut [u8], address: usize, words: &[u32]) {
            for (n, v) in words.iter().enumerate() {
                ram[address + n * 4..address + n * 4 + 4].copy_from_slice(&v.to_le_bytes());
            }
        }
        ram[0x5200..0x5500].fill(0);
        put(
            &mut ram,
            0x5200,
            &[0x80000128, 0x3000, 0x6200, 0, 0, 0, 0, 0],
        );
        put(&mut ram, 0x5300, &[0x201, 0xf0503503, 0x1a, 0xed000]);
        put(&mut ram, 0x5400, &[7, 0, 0, 0, 7 << 29]);
        let code = [
            0x4110c038000000c0u64,
            0x087dbc0208ea407c,
            0x6891c000000000db,
            0x4891c100000000da,
            0x787f4004333c00f0,
        ];
        for (n, i) in code.iter().enumerate() {
            ram[0x6200 + n * 8..0x6208 + n * 8].copy_from_slice(&i.to_le_bytes());
        }
        r[20] = 0x5200;
        // Only four low bits encode RT count; address bits 4/5 are real.
        ram.copy_within(0x5300..0x5310, 0x5330);
        r[50] = 0x5331;
        r[52] = 0x5400;
        r[57] = 0x21aa;
        r[58] = 0x1ffff;
        r[43] = 0xf000f;
        let mut t = Tiler::default();
        for _ in 0..2 {
            t.idvs(&m, &mut ram, &r, 0x0600004200000008, (0, 0))
                .unwrap();
        }
        ram[0xd000..0xd0c0].fill(0);
        put(&mut ram, 0xd020, &[0xf000f, 0, 0xf000f, 2 << 6]);
        put(&mut ram, 0xd038, &[0x8000, 0]);
        put(
            &mut ram,
            0xd080,
            &[1 << 26, 1 | (19 << 3) | (2 << 8) | (0x688 << 16)],
        );
        put(
            &mut ram,
            0xd0a0,
            &[
                0xe000, 0, 64, 0, 0xffdf8020, 0xffdf8020, 0xffdf8020, 0xffdf8020,
            ],
        );
        let mut f = [0; 256];
        f[40] = 0xd003;
        f[43] = 0xf000f;
        ram[0xe000..0xe400].fill(0x55);
        let k = key(&m, 0x8000);
        t.frames.get_mut(&k).unwrap().draws[1].regs[50] = 0x5501;
        ram[0x5500..0x5510].fill(0); // unsupported blend shader mode
        assert!(t.fragment(&m, &mut ram, &f, (0, 0)).is_err());
        assert!(ram[0xe000..0xe400].iter().all(|&v| v == 0x55));
        t.frames.get_mut(&k).unwrap().draws[1].regs[50] = 0x5301;
        t.fragment(&m, &mut ram, &f, (0, 0)).unwrap();
        assert_eq!(
            &ram[0xe000 + (8 * 16 + 8) * 4..0xe004 + (8 * 16 + 8) * 4],
            &[8, 224, 56, 159]
        );
        assert_eq!(&ram[0xe000..0xe004], &[32, 128, 223, 255]);
        assert!(t.frames.is_empty());
        assert_eq!(t.vertices, 0);
        // All color channels disabled: Mesa supplies an Off descriptor and no
        // fragment shader. Depth/stencil state still has to be side-effect-free.
        put(&mut ram, 0x5500, &[0, 0, 3, 0]);
        r[58] = 0xffff;
        r[20] = 0;
        r[50] = 0x5501;
        t.idvs(&m, &mut ram, &r, 0x0600004200000008, (0, 0))
            .unwrap();
        t.fragment(&m, &mut ram, &f, (0, 0)).unwrap();
        assert_eq!(
            &ram[0xe000 + (8 * 16 + 8) * 4..0xe004 + (8 * 16 + 8) * 4],
            &[32, 128, 223, 255]
        );
    }
    #[test]
    fn opaque_heap_lifecycle_requires_order_and_valid_context() {
        let (m, ram, _) = fixture();
        let mut t = Tiler::default();
        t.heap_operation(&m, &ram, (1, 0), 0x9400, 0).unwrap();
        t.heap_operation(&m, &ram, (1, 0), 0x9400, 1).unwrap();
        t.heap_operation(&m, &ram, (1, 0), 0x9400, 3).unwrap();
        assert!(t.heap_operation(&m, &ram, (1, 0), 0x9400, 3).is_err());
        assert!(t.heap_operation(&m, &ram, (2, 0), 0x9400, 0).is_err());
        assert!(matches!(
            t.heap_operation(&m, &ram, (1, 0), 0x20000, 0),
            Err(Stop::Memory(_))
        ));
        t.clear_group(1);
        assert!(t.heaps.is_empty());
    }
}
