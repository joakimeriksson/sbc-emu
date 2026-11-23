use super::*;

mod base {
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
    fn half_register_write_masks_keep_the_other_half() {
        let m = AddressSpace {
            transtab: 0,
            transcfg: 2,
        };
        // MOV.i32 r0.h0, 1.0 and r0.h1, 1.0: write masks 1 and 2.
        for (mask, expected) in [(1u64, 0xdead_0000u32), (2, 0x3f80_beef)] {
            let mut ram = vec![0; 16];
            ram[..8].copy_from_slice(&(0x00910000000000d0u64 | (mask << 46)).to_le_bytes());
            ram[8..].copy_from_slice(&0x7800c00000000000u64.to_le_bytes());
            let mut shader = Shader::new(0);
            shader.regs[0] = 0xdead_beef;
            shader.run(&m, &mut ram, &[], 2).unwrap();
            assert_eq!(shader.regs[0], expected);
        }
        // Write mask 0 is reserved.
        let mut ram = vec![0; 8];
        ram.copy_from_slice(&(0x00910000000000d0u64).to_le_bytes());
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
            0x001fc000000000c0 | (5 << 37), // reserved combine mode
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
            shader.fragment = Some(Fragment::new([0x19, 237 << 12]));
            shader.regs[60] = coverage;
            shader.run(&m, &mut ram, &[], 16).unwrap();
            assert_eq!(
                shader.fragment.unwrap().color[0],
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
        shader.fragment = Some(Fragment::new([0, 0]));
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
                    & !((15 << 22) | (7 << 36) | (1 << 46) | (63 << 16) | (63 << 40) | (7 << 13));
                i |= (mask << 22)
                    | (((count - 1) as u64) << 36)
                    | (u64::from(wide) << 46)
                    | (4 << 40); // coordinates in r4:r5, results from r0
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
        // Reserved bit 9, the dual-texture width, a reserved LOD mode, and array/shadow/
        // offset staging the two-register source does not provide.
        for bit in [9, 10, 11, 12, 14, 47] {
            let (m, resources, mut ram, h) = crate::gpu_texture::test_fixture();
            let i = 0x4128009413c02042u64 | (1 << bit);
            ram[..8].copy_from_slice(&i.to_le_bytes());
            let mut shader = Shader::with_allocation(0, RegisterAllocation::Registers32);
            shader.resources = resources;
            shader.fragment = Some(Fragment::new([0x19, 237 << 12]));
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
            provoking: 0,
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
            shader.fragment = Some(Fragment::new([0, 0]));
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
            // Retrieve interpolates at the same (single-sample) position; the reserved
            // update mode is rejected without changing registers.
            ram[..8].copy_from_slice(&(i | (1 << 36)).to_le_bytes());
            shader.pc = 0;
            shader.done = false;
            shader.run(&m, &mut ram, &[], 2).unwrap();
            ram[..8].copy_from_slice(&(i | (2 << 36)).to_le_bytes());
            shader.pc = 0;
            shader.done = false;
            let before = shader.regs;
            assert!(matches!(
                shader.run(&m, &mut ram, &[], 2),
                Err(Error::Unsupported { .. })
            ));
            assert_eq!(shader.regs, before);
            ram[..8].copy_from_slice(&(i | (12 << 16)).to_le_bytes()); // v2 over stride boundary
            shader.pc = 0;
            shader.done = false;
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
        shader.fragment = Some(Fragment::new([0x19, 237 << 12]));
        shader.run(&m, &mut ram, &[], 16).unwrap();
        assert_eq!(shader.fragment.unwrap().color[0], Some([1., 0., 0., 1.]));
        // A faulting texel read leaves the whole destination unchanged.
        ram[0x2108..0x210c].copy_from_slice(&0xfffffff0u32.to_le_bytes());
        let mut shader = Shader::with_allocation(32, RegisterAllocation::Registers32);
        shader.resources = resources;
        shader.fragment = Some(Fragment::new([0x19, 237 << 12]));
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

/// Instruction words from Mesa's Valhall assembler test cases
/// (src/panfrost/compiler/valhall/test/assembler-cases.txt), executed on known inputs.
mod mesa_encodings {
    use super::*;
    fn word(bytes: &str) -> u64 {
        let b: Vec<u8> = bytes
            .split(' ')
            .map(|h| u8::from_str_radix(h, 16).unwrap())
            .collect();
        u64::from_le_bytes(b.try_into().unwrap())
    }
    /// Run one instruction (then an END) with the given registers and uniforms.
    fn exec(bytes: &str, regs: &[(usize, u32)], uniforms: &[u32]) -> [u32; 64] {
        let m = AddressSpace {
            transtab: 0,
            transcfg: 2,
        };
        let mut ram: Vec<u8> = [word(bytes), 0x7800c00000000000]
            .iter()
            .flat_map(|i| i.to_le_bytes())
            .collect();
        let mut s = Shader::new(0);
        for &(r, v) in regs {
            s.regs[r] = v;
        }
        s.run(&m, &mut ram, uniforms, 2).unwrap();
        s.regs
    }
    fn f(v: f32) -> u32 {
        v.to_bits()
    }
    fn h(a: f32, b: f32) -> u32 {
        pack_half(a) as u32 | (pack_half(b) as u32) << 16
    }
    #[test]
    fn float_arithmetic_modifiers_and_clamps() {
        assert_eq!(
            exec("01 02 00 00 30 c0 a4 00", &[(1, f(1.5)), (2, f(-2.))], &[])[0],
            f(-0.5)
        ); // FADD r1, r2.neg.abs
        assert_eq!(
            exec("01 02 00 00 32 c0 a4 00", &[(1, f(1.5)), (2, f(3.))], &[])[0],
            f(-1.)
        ); // .clamp_m1_1
        assert_eq!(
            exec(
                "01 00 00 08 00 c0 a4 00",
                &[(1, f(1.)), (0, h(0., 2.))],
                &[]
            )[0],
            f(3.)
        ); // FADD r1, r0.h1
        assert_eq!(
            exec(
                "01 00 00 0c 00 c0 a5 00",
                &[(1, h(1., 4.)), (0, h(8., 2.))],
                &[]
            )[0],
            h(3., 3.)
        ); // v2f16 h00, h11
        assert_eq!(
            exec("40 00 00 00 c0 c0 9c 40", &[(0, f(-4.))], &[])[0],
            f(-0.25)
        ); // FRCP r0.neg.abs
        assert_eq!(
            exec("42 00 0d 80 40 c2 90 00", &[(2, f(-1.5))], &[])[2],
            f(1.)
        ); // FROUND.rtn -x
        assert_eq!(
            exec("42 00 0c c0 40 c2 90 00", &[(2, f(2.75))], &[])[2],
            (-2i32) as u32
        ); // F32_TO_S32.rtz -x
        assert_eq!(
            exec("42 43 04 00 00 c0 a5 00", &[(2, f(0.5)), (3, f(-2.))], &[])[0],
            h(0.5, -2.)
        ); // V2F32_TO_V2F16
        assert_eq!(
            exec("40 00 0b 10 00 c3 90 00", &[(0, h(0., 0.25))], &[])[3],
            f(0.25)
        ); // F16_TO_F32 h1
    }
    #[test]
    fn integer_widening_conversions_and_selects() {
        assert_eq!(
            exec(
                "01 02 00 0c 70 c0 a0 00",
                &[(1, 0x0500_0000), (2, 0x0007_0000)],
                &[]
            )[0],
            12
        ); // IADD r1.b3, r2.h1
        assert_eq!(
            exec("00 00 00 30 00 c7 90 00", &[(0, 0xfe00_0000)], &[])[7],
            (-2i32) as u32
        ); // S8_TO_S32 b3
        assert_eq!(
            exec("77 00 15 10 00 c2 90 00", &[(55, 0x0003_0000)], &[])[2],
            f(3.)
        ); // U16_TO_F32 h10
        assert_eq!(
            exec("40 db 05 04 00 c1 a1 00", &[(0, 0x1234)], &[])[1],
            0x3c00_1234
        ); // MKVEC.v2i16
        assert_eq!(
            exec("00 c0 c0 00 c0 c7 bd 00", &[(0, 0xab00_0000)], &[])[7],
            0xab
        ); // MKVEC.v2i8 b3
        let csel = |bytes, a: i32, b: i32| {
            exec(bytes, &[(2, a as u32), (3, b as u32)], &[0, 0, 0, 0, 7, 9])[1]
        };
        assert_eq!(csel("42 43 84 85 04 c1 50 01", -1, 1), 9); // CSEL.u32.lt: 0xffffffff < 1 is false
        assert_eq!(csel("42 43 84 85 04 c1 58 01", -1, 1), 7); // CSEL.s32.lt
                                                               // MUX.i32 neg / int_zero / fp_zero / bit with mask u0.
        let mux = |bytes, mask| exec(bytes, &[(0, 0xaaaa_aaaa), (4, 0x5555_5555)], &[mask])[0];
        assert_eq!(mux("40 44 80 00 00 c0 b8 00", 0x8000_0000), 0xaaaa_aaaa);
        assert_eq!(mux("40 44 80 00 01 c0 b8 00", 1), 0x5555_5555);
        assert_eq!(mux("40 44 80 00 02 c0 b8 00", 0x8000_0000), 0xaaaa_aaaa);
        assert_eq!(mux("40 44 80 00 03 c0 b8 00", 0xffff_0000), 0xaaaa_5555);
        // ICMP_OR.u32.gt.m1 r1, r0, 0x1000000.b3 (= 1)
        assert_eq!(exec("40 c4 c0 9c 01 c1 f0 00", &[(0, 2)], &[])[1], u32::MAX);
        assert_eq!(exec("40 c4 c0 9c 01 c1 f0 00", &[(0, 1)], &[])[1], 0);
        // FCMP_OR.v2f16.gt.m1 r2, r3.h10, r2.h00
        assert_eq!(
            exec(
                "43 42 c0 90 01 c2 f5 00",
                &[(3, h(1., 3.)), (2, h(2., 0.))],
                &[]
            )[2],
            0x0000_ffff
        );
    }
    #[test]
    fn branches_follow_relative_offsets_and_combine_modes() {
        // BRANCHZ.reconverge r0.h1, offset:3 (no .eq) is taken when the high half is
        // nonzero.
        let m = AddressSpace {
            transtab: 0,
            transcfg: 2,
        };
        let mut code = vec![word("00 03 00 00 40 c0 1f 50")];
        code.extend([
            0x0091c000000000d0u64,
            0x7800c00000000000,
            0,
            0,
            0x7800c00000000000,
        ]);
        let mut ram: Vec<u8> = code.iter().flat_map(|i| i.to_le_bytes()).collect();
        for (r0, taken) in [(0x0000_1234u32, false), (0x0001_0000, true)] {
            let mut s = Shader::new(0);
            s.regs[0] = r0;
            s.run(&m, &mut ram, &[], 8).unwrap();
            assert_eq!(s.regs[0] == 0x3f80_0000, !taken);
        }
    }
}

#[test]
fn thread_local_storage_is_private_to_each_thread() {
    // MOV r0/r1 = thread_local_pointer, MOV r2 = u0, STORE.i32 r2 -> [r0:r1],
    // LOAD.i32 r4 <- [r0:r1], END. Guest memory is not touched.
    let m = AddressSpace { transtab: 0, transcfg: 2 };
    let code = [
        0x0291c000000000e2u64,
        0x0291c100000000e3,
        0x0091c20000000080,
        0x0061420218000000,
        0x0060848218000000,
        0x7800c00000000000,
    ];
    let mut ram: Vec<u8> = code.iter().flat_map(|i| i.to_le_bytes()).collect();
    ram.resize(4096, 0x55);
    let before = ram.clone();
    for value in [7u32, 9] {
        let mut s = Shader::new(0);
        s.run(&m, &mut ram, &[value], 8).unwrap();
        assert_eq!(s.regs[4], value);
        assert_eq!((s.regs[0] as u64) | (s.regs[1] as u64) << 32, TLS_BASE);
    }
    assert_eq!(ram, before);
}
