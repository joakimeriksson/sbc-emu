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
fn word(b: &[u8], n: usize) -> u32 {
    u32::from_le_bytes(b[n * 4..n * 4 + 4].try_into().unwrap())
}
fn pair(b: &[u8], n: usize) -> u64 {
    word(b, n) as u64 | ((word(b, n + 1) as u64) << 32)
}
impl Resources {
    fn descriptor(
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
        let (m, resources, mut ram) = fixture();
        let captured = 0x0866800412017d7cu64;
        for instruction in [
            captured | (1 << 36),
            captured & !(7 << 33),
            captured | (1 << 24),
            (captured & !(63 << 40)) | (15 << 40),
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
}
