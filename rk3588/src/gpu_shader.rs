//! Initial scalar Valhall shader execution. Encodings follow Mesa's Valhall ISA.xml.
//! Only the explicitly decoded 32-bit operations are supported. Unknown opcodes,
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
pub struct Shader {
    pub regs: [u32; 64],
    pub resources: crate::gpu_resources::Resources,
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
            3 => LUT.get(s & 63).copied(),
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
            if i >> 63 != 0 || flow > 10 && flow != 15 {
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
                0xa4 | 0xb2 => {
                    // Scalar f32 with identity swizzles, round-to-nearest and optional abs/neg.
                    let allowed = if op == 0xa4 {
                        0x000000fc_0000ffff
                    } else {
                        0x000000fc_00ffffff
                    };
                    if i & 0x000000ff_ffffffff & !allowed != 0 {
                        return Err(bad());
                    }
                    let value = |n: usize| -> Result<f32, Error> {
                        let mut v = f32::from_bits(self.source(i, n, uniforms).ok_or_else(bad)?);
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
                0x60 | 0x61 => {
                    // Global 32-bit scalar or 128-bit vector transfers, normal memory access.
                    if ((i >> 46) & 3) != if op == 0x61 { 1 } else { 2 }
                        || !matches!(((i >> 27) & 7, (i >> 33) & 7), (3, 1) | (7, 4))
                        || (i >> 24) & 3 != 0
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
                    let s = (i & 255) as usize;
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
            if flow == 15 {
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
