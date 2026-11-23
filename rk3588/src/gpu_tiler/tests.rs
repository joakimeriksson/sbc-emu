
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
    r[45] = 1f32.to_bits(); // depth range [0, 1]
    (m, ram, r)
}
#[test]
fn idvs_executes_native_positions_and_keeps_vm_and_queue_ownership() {
    let (m, mut ram, r) = fixture();
    let mut t = Tiler::default();
    t.idvs(&m, &mut ram, &r, 0x0600004200000008, (1, 2))
        .unwrap();
    assert_eq!(
        t.frames[&key(&m, 0x8000)].draws[0].triangle(0),
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
    r[56] |= 1 << 20; // layer index output
    assert!(t
        .idvs(&m, &mut ram, &r, 0x0600004200000008, (0, 0))
        .is_err());
    assert!(t.frames.is_empty());
    r[56] &= !(1 << 20);
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
        let d = &frame.draws[0];
        assert_eq!([d.triangle(0), d.triangle(1)], [expected, expected]);
        // Repeated elements are shaded once.
        assert_eq!(t.vertices, 3);
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
        t.frames[&key(&m, 0x8000)].draws[0].triangle(0),
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
    for instance in 0..frame.draws[0].primitives.len() {
        let triangle = &frame.draws[0].triangle(instance);
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
        (0xc000, 12, 349_526), // aggregate vertex limit
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
fn positions_and_varyings_stay_out_of_the_geometry_fifo() {
    // The 64 KiB geometry buffer is a FIFO on the GPU: whole draws do not have to
    // fit in it, and its memory is left alone.
    let (m, mut ram, mut r) = fixture();
    ram[0x8020..0x8028].copy_from_slice(&0x10001u64.to_le_bytes());
    ram[0x10000..0x11000].fill(0xa5);
    r[34] = 1365;
    let mut t = Tiler::default();
    t.idvs(&m, &mut ram, &r, 0x0600004200000008, (0, 0))
        .unwrap();
    assert_eq!(t.frames[&key(&m, 0x8000)].draws[0].primitives.len(), 1365);
    assert!(ram[0x10000..0x11000].iter().all(|&v| v == 0xa5));
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
    r[48] = 1028; // varying records above 1 KiB
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
    assert_eq!(frame.draws[0].triangle(0)[0], [2., 2., 0.5, 1.]);
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
fn put(ram: &mut [u8], address: usize, words: &[u32]) {
    for (n, v) in words.iter().enumerate() {
        ram[address + n * 4..address + n * 4 + 4].copy_from_slice(&v.to_le_bytes());
    }
}
/// A uniform-colour fragment shader (MOV r0/r1 from u0/u1, ATEST, BLEND), an opaque
/// RGBA8 target and a 16x16 framebuffer at 0xd000 writing to 0xe000.
fn coloured(ram: &mut [u8], r: &mut [u32; 256]) -> [u32; 256] {
    put(ram, 0x5200, &[0x80000128, 0x3000, 0x6200, 0, 0, 0, 0, 0]);
    put(ram, 0x5300, &[0x200, 0xf0122122, 0x19, 0xed000]);
    let code = [
        0x0091c00000000080u64,
        0x0091c10000000081,
        0x087dbc0208ea407c,
        0x787f4004333c00f0,
    ];
    for (n, i) in code.iter().enumerate() {
        ram[0x6200 + n * 8..0x6208 + n * 8].copy_from_slice(&i.to_le_bytes());
    }
    r[20] = 0x5200;
    r[13] = 1 << 24;
    r[50] = 0x5301;
    r[58] = 0x1ffff;
    r[43] = 0xf000f;
    ram[0xd000..0xd0c0].fill(0);
    // 16x16, D24 depth cleared to 1.0, one RGBA8 target cleared to black.
    put(
        ram,
        0xd020,
        &[
            0xf000f,
            0,
            0xf000f,
            2 << 6,
            1 << 16,
            1f32.to_bits(),
            0x8000,
            0,
        ],
    );
    put(
        ram,
        0xd080,
        &[1 << 26, 1 | (19 << 3) | (2 << 8) | (0x688 << 16)],
    );
    put(
        ram,
        0xd0a0,
        &[
            0xe000, 0, 64, 0, 0xff000000, 0xff000000, 0xff000000, 0xff000000,
        ],
    );
    let mut f = [0; 256];
    f[40] = 0xd003;
    f[43] = 0xf000f;
    f
}
fn draw(
    t: &mut Tiler,
    m: &AddressSpace,
    ram: &mut [u8],
    r: &mut [u32; 256],
    z: f32,
    colour: [u32; 2],
    at: usize,
) {
    ram[0x7010..0x7014].copy_from_slice(&z.to_bits().to_le_bytes());
    put(ram, at, &colour);
    r[12] = at as u32;
    t.idvs(m, ram, r, 0x0600004200000008, (0, 0)).unwrap();
}
#[test]
fn depth_test_and_culling_keep_the_nearest_front_face() {
    let (m, mut ram, mut r) = fixture();
    let f = coloured(&mut ram, &mut r);
    // Depth LESS with writes; cull back faces.
    put(&mut ram, 0x5400, &[7, 0, 0, 0, (1 << 29) | (1 << 27)]);
    r[52] = 0x5400;
    r[57] = 1 << 18;
    let mut t = Tiler::default();
    let half = |v: f32| crate::gpu_shader::f32_to_f16(v) as u32;
    let red = [half(1.), half(1.) << 16];
    let green = [half(0.) | half(1.) << 16, half(1.) << 16];
    let blue = [0, half(1.) | half(1.) << 16];
    draw(&mut t, &m, &mut ram, &mut r, 0.75, red, 0x7100);
    draw(&mut t, &m, &mut ram, &mut r, 0.25, green, 0x7108);
    draw(&mut t, &m, &mut ram, &mut r, 0.5, blue, 0x7110);
    // The same triangle with reversed winding is a back face: culled.
    for (n, v) in [-0.75f32, -0.75, 0., 0.75, 0.75, -0.75]
        .into_iter()
        .enumerate()
    {
        ram[0x4000 + n * 4..0x4004 + n * 4].copy_from_slice(&v.to_le_bytes());
    }
    draw(&mut t, &m, &mut ram, &mut r, 0.0, red, 0x7118);
    t.fragment(&m, &mut ram, &f, (0, 0)).unwrap();
    let at = 0xe000 + (8 * 16 + 8) * 4;
    assert_eq!(&ram[at..at + 4], &[0, 255, 0, 255]);
    // Outside the triangle the clear colour remains.
    assert_eq!(&ram[0xe000..0xe004], &[0, 0, 0, 255]);
}
#[test]
fn strips_fans_lines_and_restart_assemble_with_provoking_vertices() {
    let mut out = Vec::new();
    assemble(10, &[0, 1, 2, 3], 0, false, &mut out);
    // Odd strip triangles swap their first two vertices to keep the winding.
    assert_eq!(out, vec![[0, 1, 2, 2], [2, 1, 3, 3]]);
    out.clear();
    assemble(12, &[0, 1, 2, 3], 10, true, &mut out);
    assert_eq!(out, vec![[10, 11, 12, 11], [10, 12, 13, 12]]);
    out.clear();
    assemble(6, &[0, 1, 2], 0, false, &mut out);
    assert_eq!(out, vec![[0, 1, 1, 1], [1, 2, 2, 2], [2, 0, 0, 0]]);
    // Primitive restart splits the index stream into separate strips.
    let (m, mut ram, mut r) = fixture();
    for (n, v) in [0u16, 1, 2, 0xffff, 0, 1, 2].iter().enumerate() {
        ram[0xc000 + n * 2..0xc002 + n * 2].copy_from_slice(&v.to_le_bytes());
    }
    r[33] = 7;
    r[39] = 14;
    r[54] = 0xc000;
    r[56] |= 1 << 19;
    let mut t = Tiler::default();
    t.idvs(&m, &mut ram, &r, 0x060000420000020a, (0, 0))
        .unwrap();
    let d = &t.frames[&key(&m, 0x8000)].draws[0];
    assert_eq!(d.primitives.len(), 2);
    assert_eq!(d.positions.len(), 3);
}
