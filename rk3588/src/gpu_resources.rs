//! Valhall resource tables and 1D float vertex attributes (Mesa v10.xml/v7.xml).
use crate::gpu_mmu::{AddressSpace, Fault};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Error {
    Memory(Fault),
    Unsupported,
    DescriptorBounds,
}
#[derive(Clone, Copy, Debug, Default)]
pub struct Resources(pub u64);
/// GPU-mapped structured storage supplied by the vertex engine for its private
/// position table. Contents still pass through the ordinary GPU MMU.
#[derive(Clone, Copy, Debug)]
pub struct Buffer {
    pub address: u64,
    pub size: u64,
    pub stride: u32,
}
impl Buffer {
    fn element(self, index: u32) -> Result<u64, Error> {
        let offset = index as u64 * self.stride as u64;
        if self.stride == 0
            || offset
                .checked_add(self.stride as u64)
                .is_none_or(|end| end > self.size)
        {
            return Err(Error::DescriptorBounds);
        }
        self.address
            .checked_add(offset)
            .ok_or(Error::Memory(Fault::AddressSize))
    }
}
fn word(b: &[u8], n: usize) -> u32 {
    u32::from_le_bytes(b[n * 4..n * 4 + 4].try_into().unwrap())
}
fn pair(b: &[u8], n: usize) -> u64 {
    word(b, n) as u64 | ((word(b, n + 1) as u64) << 32)
}
impl Resources {
    pub(crate) fn descriptor(
        &self,
        m: &AddressSpace,
        ram: &[u8],
        table: u32,
        index: u32,
    ) -> Result<[u8; 32], Error> {
        let count = self.0 & 63;
        if table as u64 >= count {
            return Err(Error::DescriptorBounds);
        }
        let address = (self.0 & !63)
            .checked_add(table as u64 * 16)
            .ok_or(Error::DescriptorBounds)?;
        let mut resource = [0; 16];
        m.read(ram, address, &mut resource, true)
            .map_err(Error::Memory)?;
        let pointer = pair(&resource, 0);
        if pointer >> 56 != 1 || pointer & 31 != 0 {
            return Err(Error::Unsupported);
        }
        let offset = index as u64 * 32;
        if offset + 32 > pair(&resource, 2) {
            return Err(Error::DescriptorBounds);
        }
        let address = (pointer & 0x00ff_ffff_ffff_ffff)
            .checked_add(offset)
            .ok_or(Error::DescriptorBounds)?;
        let mut descriptor = [0; 32];
        m.read(ram, address, &mut descriptor, true)
            .map_err(Error::Memory)?;
        Ok(descriptor)
    }
    /// Resolve LEA_BUF_IMM through a structured buffer descriptor, or through
    /// the hardware-provided position table (13) when running a vertex shader.
    pub fn buffer_address(
        &self,
        m: &AddressSpace,
        ram: &[u8],
        table: u32,
        index: u32,
        element: u32,
        position: Option<Buffer>,
    ) -> Result<u64, Error> {
        if table == 13 {
            if index != 0 {
                return Err(Error::Unsupported);
            }
            return position.ok_or(Error::Unsupported)?.element(element);
        }
        if table >= 12 {
            return Err(Error::Unsupported);
        }
        let b = self.descriptor(m, ram, table, index)?;
        if b[20..].iter().any(|&v| v != 0) {
            return Err(Error::Unsupported);
        }
        let stride = match word(&b, 0) {
            0x39 => word(&b, 4),
            0x19 if element == 0 && word(&b, 4) == 0 => 1,
            _ => return Err(Error::Unsupported),
        };
        Buffer {
            address: pair(&b, 2),
            size: word(&b, 1) as u64,
            stride,
        }
        .element(element)
    }
    /// Load a complete attribute, including default missing components. Buffer
    /// bounds are checked before translating data; an out-of-range fetch returns
    /// zero source channels, with the format's implicit alpha component retained.
    pub fn attribute(
        &self,
        m: &AddressSpace,
        ram: &[u8],
        table: u32,
        index: u32,
        vertex: u32,
        instance: u32,
    ) -> Result<[u32; 4], Error> {
        let a = self.descriptor(m, ram, table, index)?;
        // Mesa emits ordinary per-vertex descriptors with offset_enable,
        // and POT instance descriptors with divisor_r (including divisor one).
        let kind = word(&a, 0) & 1023;
        let frequency = (word(&a, 1) >> 6) & 3;
        let divisor = match kind {
            0x115 if word(&a, 1) & !255 == 0 && frequency <= 1 => 0,
            0x25 if word(&a, 1) & !0x1f00_00ff == 0 && frequency == 1 => (word(&a, 1) >> 24) & 31,
            _ => return Err(Error::Unsupported),
        };
        if a[20..].iter().any(|&b| b != 0) {
            return Err(Error::Unsupported);
        }
        let format = word(&a, 0) >> 10;
        let channels = match (format >> 12) & 255 {
            167 => 1,
            175 => 2,
            183 => 3,
            191 => 4,
            _ => return Err(Error::Unsupported),
        };
        if format >> 20 != 0 || !matches!(format & 4095, 0 | 16) {
            return Err(Error::Unsupported);
        }
        let b = self.descriptor(m, ram, word(&a, 1) & 63, word(&a, 3))?;
        if word(&b, 0) != 0x19 || b[16..].iter().any(|&v| v != 0) {
            return Err(Error::Unsupported);
        }
        let element = if frequency == 1 {
            instance >> divisor
        } else {
            vertex
        };
        let offset = element as i128 * word(&a, 4) as i128 + (word(&a, 2) as i32) as i128;
        let mut result = [0, 0, 0, 1f32.to_bits()];
        let size = channels * 4;
        if offset >= 0 && offset + size as i128 <= word(&b, 1) as i128 {
            let address = pair(&b, 2)
                .checked_add(offset as u64)
                .ok_or(Error::Memory(Fault::AddressSize))?;
            let mut bytes = [0; 16];
            m.read(ram, address, &mut bytes[..size], true)
                .map_err(Error::Memory)?;
            for (n, value) in result.iter_mut().enumerate().take(channels) {
                *value = word(&bytes, n);
            }
        }
        if format & 4095 == 16 {
            result[3] = 1f32.to_bits();
        }
        Ok(result)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::gpu_shader::{RegisterAllocation, Shader};
    fn put(ram: &mut [u8], at: usize, value: u32) {
        ram[at..at + 4].copy_from_slice(&value.to_le_bytes());
    }
    fn fixture() -> (AddressSpace, Resources, Vec<u8>) {
        let m = AddressSpace {
            transtab: 0,
            transcfg: 2,
        };
        let mut ram = vec![0; 0x5000];
        for (table, address) in [(1, 0x2000u64), (2, 0x3000)] {
            let at = 0x1000 + table * 16;
            ram[at..at + 8].copy_from_slice(&(address | (1 << 56)).to_le_bytes());
            ram[at + 8..at + 16].copy_from_slice(&32u64.to_le_bytes());
        }
        put(&mut ram, 0x2000, 0x115 | (((175 << 12) | 16) << 10));
        put(&mut ram, 0x2004, 2);
        put(&mut ram, 0x2010, 8);
        put(&mut ram, 0x3000, 0x19);
        put(&mut ram, 0x3004, 24);
        put(&mut ram, 0x3008, 0x4000);
        for (n, v) in [-0.75f32, -0.75, 0.75, -0.75, 0.0, 0.75]
            .into_iter()
            .enumerate()
        {
            put(&mut ram, 0x4000 + n * 4, v.to_bits());
        }
        (m, Resources(0x1003), ram)
    }
    #[test]
    fn captured_mesa_vertex_attribute_instruction_reads_the_selected_vertex() {
        let (m, resources, mut ram) = fixture();
        // Actual Mesa GLES vertex shader: LD_ATTR_IMM.v2.f32 @r0:r1,
        // ^r60, ^r61, index:0, table:1. The discard hints do not change IDs.
        ram[..8].copy_from_slice(&0x0866800412017d7cu64.to_le_bytes());
        for (id, expected) in [(0, [-0.75f32, -0.75]), (1, [0.75, -0.75]), (2, [0.0, 0.75])] {
            let mut shader = Shader::with_allocation(0, RegisterAllocation::Registers32);
            shader.resources = resources;
            shader.regs[60] = id;
            assert_eq!(
                shader.run(&m, &mut ram, &[], 1),
                Err(crate::gpu_shader::Error::Budget)
            );
            assert_eq!(&shader.regs[..2], &expected.map(f32::to_bits));
            assert_eq!(shader.pc, 8);
        }
    }
    #[test]
    fn signed_offsets_instance_frequency_and_buffer_bounds() {
        let (m, resources, mut ram) = fixture();
        put(&mut ram, 0x2008, (-8i32) as u32);
        let a = resources.attribute(&m, &ram, 1, 0, 1, 0).unwrap();
        assert_eq!(
            a,
            [
                (-0.75f32).to_bits(),
                (-0.75f32).to_bits(),
                0,
                1f32.to_bits()
            ]
        );
        put(&mut ram, 0x2008, 0);
        put(&mut ram, 0x2004, 2 | 64);
        assert_eq!(
            resources.attribute(&m, &ram, 1, 0, 0, 2).unwrap(),
            [0, 0.75f32.to_bits(), 0, 1f32.to_bits()]
        );
        // Native Mesa divisor-four descriptor selects vertex 2 for instance 9.
        put(&mut ram, 0x2000, 0x25 | (((175 << 12) | 16) << 10));
        put(&mut ram, 0x2004, 2 | 64 | (2 << 24));
        assert_eq!(
            resources.attribute(&m, &ram, 1, 0, 0, 9).unwrap(),
            [0, 0.75f32.to_bits(), 0, 1f32.to_bits()]
        );
        // Data bounds are checked before touching an otherwise unmapped base.
        put(&mut ram, 0x3008, 0x8000);
        assert_eq!(
            resources.attribute(&m, &ram, 1, 0, 0, u32::MAX).unwrap(),
            [0, 0, 0, 1f32.to_bits()]
        );
        put(&mut ram, 0x2000, 0x35 | (((175 << 12) | 16) << 10));
        assert_eq!(
            resources.attribute(&m, &ram, 1, 0, 0, 0),
            Err(Error::Unsupported)
        );
    }
    #[test]
    fn malformed_attribute_instructions_leave_registers_unchanged() {
        let (m, mut resources, mut ram) = fixture();
        // Even if SRT entries 12-15 are valid, the immediate encodings refer to
        // hardware tables 60-63 and must not access those ordinary entries.
        resources.0 = (resources.0 & !63) | 16;
        for table in 12..16 {
            let at = 0x1000 + table * 16;
            ram[at..at + 8].copy_from_slice(&(0x2000u64 | (1 << 56)).to_le_bytes());
            ram[at + 8..at + 16].copy_from_slice(&32u64.to_le_bytes());
        }
        let captured = 0x0866800412017d7cu64;
        for instruction in [
            captured | (1 << 36),
            captured & !(7 << 33),
            captured | (1 << 24),
            (captured & !(63 << 40)) | (15 << 40),
            (captured & !(15 << 16)) | (12 << 16),
            (captured & !(15 << 16)) | (13 << 16),
            (captured & !(15 << 16)) | (14 << 16),
            (captured & !(15 << 16)) | (15 << 16),
        ] {
            ram[..8].copy_from_slice(&instruction.to_le_bytes());
            let mut shader = Shader::new(0);
            shader.resources = resources;
            shader.regs[0] = 0xfeed;
            let before = shader.regs;
            assert!(matches!(
                shader.run(&m, &mut ram, &[], 1),
                Err(crate::gpu_shader::Error::Unsupported { .. })
            ));
            assert_eq!(shader.regs, before);
            assert_eq!(shader.pc, 0);
        }
    }
    #[test]
    fn descriptor_and_memory_faults_do_not_modify_shader_registers() {
        let (m, resources, mut ram) = fixture();
        assert_eq!(
            resources.attribute(&m, &ram, 3, 0, 0, 0),
            Err(Error::DescriptorBounds)
        );
        assert_eq!(
            resources.attribute(&m, &ram, 1, 1, 0, 0),
            Err(Error::DescriptorBounds)
        );
        // Valid descriptor but unmapped vertex data.
        put(&mut ram, 0x3008, 0x8000);
        ram[..8].copy_from_slice(&0x0866800412017d7cu64.to_le_bytes());
        let mut shader = Shader::new(0);
        shader.resources = resources;
        shader.regs[0] = 0xfeed;
        let before = shader.regs;
        assert!(matches!(
            shader.run(&m, &mut ram, &[], 1),
            Err(crate::gpu_shader::Error::Memory(_))
        ));
        assert_eq!(shader.regs, before);
        assert_eq!(shader.pc, 0);
    }
    #[test]
    fn complete_captured_mesa_position_shader_writes_gpu_mapped_output() {
        let (m, resources, mut ram) = fixture();
        ram.resize(0x7000, 0xa5);
        let code: [u64; 7] = [
            0x0866800412017d7c,
            0x00b2c00000818040,
            0x00b2c10000838241,
            0x0091c20000000084,
            0x0091c300000000d0,
            0x105e840440000d7b,
            0x78614008b9000044,
        ];
        for (n, instruction) in code.into_iter().enumerate() {
            ram[n * 8..n * 8 + 8].copy_from_slice(&instruction.to_le_bytes());
        }
        let uniforms = [
            8f32.to_bits(),
            8f32.to_bits(),
            8f32.to_bits(),
            8f32.to_bits(),
            0.5f32.to_bits(),
        ];
        for (id, xy) in [(0, [2f32, 2.]), (1, [14., 2.]), (2, [8., 14.])] {
            let mut shader = Shader::with_allocation(0, RegisterAllocation::Registers32);
            shader.resources = resources;
            shader.position_output = Some(Buffer {
                address: 0x6000,
                size: 48,
                stride: 4,
            });
            // Mesa adds view/attribute offsets in dwords to this internal ID.
            shader.regs[59] = id * 4;
            shader.regs[60] = id;
            shader.run(&m, &mut ram, &uniforms, 16).unwrap();
            let at = 0x6000 + id as usize * 16;
            let expected: Vec<u8> = [xy[0], xy[1], 0.5, 1.]
                .iter()
                .flat_map(|v| v.to_le_bytes())
                .collect();
            assert_eq!(&ram[at..at + 16], &expected);
        }
        assert_eq!(&ram[0x5ff0..0x6000], &[0xa5; 16]);
        assert_eq!(&ram[0x6030..0x6040], &[0xa5; 16]);
        // The reserved position table cannot leak into ordinary compute use.
        assert_eq!(
            resources.buffer_address(&m, &ram, 13, 0, 0, None),
            Err(Error::Unsupported)
        );
        assert_eq!(
            resources.buffer_address(
                &m,
                &ram,
                13,
                1,
                0,
                Some(Buffer {
                    address: 0x6000,
                    size: 48,
                    stride: 16
                })
            ),
            Err(Error::Unsupported)
        );
    }
    #[test]
    fn structured_buffer_addresses_check_bounds_before_register_changes() {
        let (m, resources, mut ram) = fixture();
        put(&mut ram, 0x3000, 0x39);
        put(&mut ram, 0x3010, 8);
        assert_eq!(
            resources.buffer_address(&m, &ram, 2, 0, 2, None).unwrap(),
            0x4010
        );
        assert_eq!(
            resources.buffer_address(&m, &ram, 2, 0, 3, None),
            Err(Error::DescriptorBounds)
        );
        // LEA d4, r59, table2; reserved or malformed fields must not write d4.
        let valid = (0x105e840440000d7bu64 & !(15 << 8)) | (2 << 8);
        for instruction in [valid | (1 << 20), valid & !(7 << 33), valid | (1 << 40)] {
            ram[..8].copy_from_slice(&instruction.to_le_bytes());
            let mut shader = Shader::new(0);
            shader.resources = resources;
            shader.regs[4] = 0xfeed;
            let before = shader.regs;
            assert!(matches!(
                shader.run(&m, &mut ram, &[], 1),
                Err(crate::gpu_shader::Error::Unsupported { .. })
            ));
            assert_eq!(shader.regs, before);
            assert_eq!(shader.pc, 0);
        }
    }
}
