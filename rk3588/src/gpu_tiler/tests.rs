
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
fn indirect_draws_start_at_their_first_index() {
    // glDrawElementsIndirect: r54 is the whole index buffer, r39 its size, r35 firstIndex.
    let (m, mut ram, mut r) = fixture();
    for (n, value) in [9u32, 9, 9, 3, 1, 2].iter().enumerate() {
        ram[0xc000 + n * 4..0xc000 + n * 4 + 4].copy_from_slice(&value.to_le_bytes());
    }
    r[33] = 3;
    r[35] = 3;
    r[36] = (-1i32) as u32;
    r[39] = 24;
    r[54] = 0xc000;
    let mut t = Tiler::default();
    t.idvs(&m, &mut ram, &r, 0x0600004200000308, (0, 0)).unwrap();
    let d = &t.frames[&key(&m, 0x8000)].draws[0];
    assert_eq!(d.triangle(0), [[8., 14., 0.5, 1.], [2., 2., 0.5, 1.], [14., 2., 0.5, 1.]]);
    // Past the end of the buffer: nothing drawn.
    r[35] = 4;
    let mut t = Tiler::default();
    assert!(t.idvs(&m, &mut ram, &r, 0x0600004200000308, (0, 0)).is_err());
    assert!(t.frames.is_empty());
}
#[test]
fn a_draw_with_no_instances_draws_nothing() {
    let (m, mut ram, mut r) = fixture();
    r[34] = 0;
    r[39] = 12;
    r[54] = 0xc000;
    ram[0x10000..0x10100].fill(0xa5);
    let mut t = Tiler::default();
    t.idvs(&m, &mut ram, &r, 0x0600004200000308, (0, 0)).unwrap();
    assert_eq!(t.vertices, 0);
    assert!(t.frames.values().all(|f| f.draws.iter().all(|d| d.primitives.is_empty())));
    assert!(ram[0x10000..0x10100].iter().all(|&v| v == 0xa5));
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
#[test]
fn parallel_fragment_bands_match_the_serial_result() {
    let render = |bands: Option<(usize, u32)>| {
        let (m, mut ram, mut r) = fixture();
        let f = coloured(&mut ram, &mut r);
        put(&mut ram, 0x5400, &[7, 0, 0, 0, (1 << 29) | (1 << 27)]);
        r[52] = 0x5400;
        let mut t = Tiler::default();
        let half = |v: f32| crate::gpu_shader::f32_to_f16(v) as u32;
        draw(&mut t, &m, &mut ram, &mut r, 0.75, [half(1.), half(1.) << 16], 0x7100);
        draw(&mut t, &m, &mut ram, &mut r, 0.25, [half(0.) | half(1.) << 16, half(1.) << 16], 0x7108);
        crate::gpu_raster::BANDS.with(|b| b.set(bands));
        crate::gpu::take_work();
        t.fragment(&m, &mut ram, &f, (0, 0)).unwrap();
        // The GPU time charged does not depend on how the job was split either.
        let work = crate::gpu::take_work();
        crate::gpu_raster::BANDS.with(|b| b.set(None));
        (ram[0xe000..0xe400].to_vec(), work)
    };
    let serial = render(Some((1, 16)));
    assert_eq!(render(Some((4, 4))), serial);
    assert_eq!(render(Some((3, 2))), serial);
    assert!(serial.1 > 0);
    assert!(serial.0.chunks(4).any(|p| p == [0, 255, 0, 255]));
}
#[test]
fn a_frame_that_falls_back_to_serial_is_charged_once() {
    // Draw 1 covers the frame without stores; draw 2's fragment shader stores to memory
    // before BLEND, so the fragment workers give up and the job renders serially. The
    // workers' work must not be charged on top of the serial pass's.
    let render = |bands: Option<(usize, u32)>| {
        let (m, mut ram, mut r) = fixture();
        let f = coloured(&mut ram, &mut r);
        put(&mut ram, 0x5400, &[7, 0, 0, 0, (1 << 29) | (1 << 27)]);
        r[52] = 0x5400;
        let mut t = Tiler::default();
        let half = |v: f32| crate::gpu_shader::f32_to_f16(v) as u32;
        draw(&mut t, &m, &mut ram, &mut r, 0.75, [half(1.), half(1.) << 16], 0x7100);
        // Draw 2: MOV r0/r1 = u0/u1, MOV r4/r5 = u2/u3, STORE.i32 r0 -> [r4:r5], ATEST, BLEND.
        put(&mut ram, 0x5240, &[0x80000128, 0x3000, 0x6400, 0, 0, 0, 0, 0]);
        let code = [
            0x0091c00000000080u64,
            0x0091c10000000081,
            0x0091c40000000082,
            0x0091c50000000083,
            0x0061400218000004,
            0x087dbc0208ea407c,
            0x787f4004333c00f0,
        ];
        for (n, i) in code.iter().enumerate() {
            ram[0x6400 + n * 8..0x6408 + n * 8].copy_from_slice(&i.to_le_bytes());
        }
        r[20] = 0x5240;
        r[13] = 2 << 24; // four uniforms
        draw(&mut t, &m, &mut ram, &mut r, 0.25, [half(0.) | half(1.) << 16, half(1.) << 16], 0x7108);
        put(&mut ram, 0x7110, &[0x1f000, 0]);
        crate::gpu_raster::BANDS.with(|b| b.set(bands));
        crate::gpu::take_work();
        t.fragment(&m, &mut ram, &f, (0, 0)).unwrap();
        let work = crate::gpu::take_work();
        crate::gpu_raster::BANDS.with(|b| b.set(None));
        (ram[0xe000..0xe400].to_vec(), ram[0x1f000..0x1f004].to_vec(), work)
    };
    let serial = render(Some((1, 16)));
    // The store happened and draw 2 rendered.
    assert_ne!(serial.1, [0; 4]);
    assert!(serial.0.chunks(4).any(|p| p == [0, 255, 0, 255]));
    assert_eq!(render(Some((4, 4))), serial);
}
#[test]
fn primitives_count_against_the_frame_limit_not_only_vertices() {
    // A million identical indices shade one vertex but make a million points.
    let (m, mut ram, mut r) = fixture();
    ram.resize(0x20000 + (1 << 20), 0);
    r[33] = 1 << 20;
    r[39] = 1 << 20;
    r[54] = 0x20000;
    let mut t = Tiler::default();
    for _ in 0..FRAME_PRIMITIVES >> 20 {
        t.idvs(&m, &mut ram, &r, 0x0600004200000101, (0, 0)).unwrap();
    }
    assert_eq!((t.vertices, t.primitives), (FRAME_PRIMITIVES >> 20, FRAME_PRIMITIVES));
    assert!(matches!(
        t.idvs(&m, &mut ram, &r, 0x0600004200000101, (0, 0)),
        Err(Stop::Unsupported { .. })
    ));
    assert_eq!(t.frames[&key(&m, 0x8000)].draws.len(), FRAME_PRIMITIVES >> 20);
    // Rendering the frame (here: dropping its queue) gives the room back.
    t.clear_queue((0, 0));
    assert_eq!((t.vertices, t.primitives, t.bytes), (0, 0, 0));
    t.idvs(&m, &mut ram, &r, 0x0600004200000101, (0, 0)).unwrap();
}
#[test]
fn varyings_and_held_bytes_are_bounded_before_allocating() {
    // 2^20 vertices with 1 KiB varying records would be a 1 GiB allocation: refused
    // before any shading or allocation.
    let (m, mut ram, mut r) = with_varyings();
    r[48] = 1024;
    r[34] = 349_525;
    let mut t = Tiler::default();
    assert!(matches!(
        t.idvs(&m, &mut ram, &r, 0x0600004200040008, (0, 0)),
        Err(Stop::Unsupported { .. })
    ));
    assert!(t.frames.is_empty());
    assert_eq!((t.vertices, t.primitives, t.bytes), (0, 0, 0));
    // The bound is on all unrendered frames together: a small draw does not fit in the
    // last few bytes either.
    r[48] = 16;
    r[34] = 1;
    t.bytes = FRAME_BYTES - 1000;
    assert!(t.idvs(&m, &mut ram, &r, 0x0600004200040008, (0, 0)).is_err());
    t.bytes = 0;
    t.idvs(&m, &mut ram, &r, 0x0600004200040008, (0, 0)).unwrap();
    let held = t.bytes;
    assert!(held >= std::mem::size_of::<Draw>() + 3 * (16 + 16) + 16);
    // Each draw's state counts, so empty draws cannot pile up without bound.
    t.idvs(&m, &mut ram, &r, 0x0600004200040008, (0, 0)).unwrap();
    assert_eq!(t.bytes, 2 * held);
    // A snapshot keeps the frames; the totals are counted again when it is loaded.
    let mut w = emu_core::snap::W(Vec::new());
    emu_core::snap::Snap::save(&t, &mut w);
    let mut loaded = Tiler::default();
    emu_core::snap::Snap::load(&mut loaded, &mut emu_core::snap::R::new(&w.0)).unwrap();
    assert_eq!((loaded.vertices, loaded.primitives, loaded.bytes), (t.vertices, t.primitives, t.bytes));
    t.clear_group(0);
    assert_eq!((t.vertices, t.primitives, t.bytes), (0, 0, 0));
}
#[test]
fn draws_with_no_instances_make_no_frame_and_do_not_count() {
    let (m, mut ram, mut r) = fixture();
    let mut t = Tiler::default();
    r[34] = 0;
    t.idvs(&m, &mut ram, &r, 0x0600004200000008, (0, 0)).unwrap();
    assert!(t.frames.is_empty());
    // A full frame still takes draws that draw nothing (a GPU-culled indirect loop).
    r[34] = 1;
    t.idvs(&m, &mut ram, &r, 0x0600004200000008, (0, 0)).unwrap();
    let frame = t.frames.get_mut(&key(&m, 0x8000)).unwrap();
    frame.draws.resize_with(1 << 16, Draw::default);
    assert!(matches!(
        t.idvs(&m, &mut ram, &r, 0x0600004200000008, (0, 0)),
        Err(Stop::Unsupported { .. })
    ));
    r[34] = 0;
    for _ in 0..4 {
        t.idvs(&m, &mut ram, &r, 0x0600004200000008, (0, 0)).unwrap();
    }
    assert_eq!(t.frames[&key(&m, 0x8000)].draws.len(), 1 << 16);
}
/// The coloured fixture enlarged to `w` x `h`, writing linear or u-interleaved.
fn enlarge(ram: &mut [u8], r: &mut [u32; 256], f: &mut [u32; 256], (w, h): (u32, u32), tiled: bool) {
    let size = (w - 1) | ((h - 1) << 16);
    put(ram, 0xd020, &[size, 0, size]);
    put(ram, 0x800c, &[size]);
    put(ram, 0x7000, &[(w / 2) as f32, (h / 2) as f32, (w / 2) as f32, (h / 2) as f32].map(f32::to_bits));
    r[43] = size;
    f[43] = size;
    if tiled {
        put(ram, 0xd084, &[1 | (19 << 3) | (1 << 8) | (0x688 << 16)]);
        put(ram, 0xd0a8, &[w / 16 * 256 * 4]);
    } else {
        put(ram, 0xd0a8, &[w * 4]);
    }
}
/// Render two overlapping draws with depth and an occlusion counter into a 32x48 target
/// whose box starts at `min` (x | y << 16), with `pass` and the fragment workers (`bands`)
/// forced: the target's bytes and the counter.
fn passes_at(pass: Option<(usize, usize)>, tiled: bool, min: u32, bands: Option<(usize, u32)>) -> Result<(Vec<u8>, u64), Stop> {
    let (m, mut ram, mut r) = fixture();
    let mut f = coloured(&mut ram, &mut r);
    enlarge(&mut ram, &mut r, &mut f, (32, 48), tiled);
    f[42] = min;
    put(&mut ram, 0x5400, &[7, 0, 0, 0, (1 << 29) | (1 << 27)]);
    r[52] = 0x5400;
    r[57] |= 3 << 14;
    r[46] = 0x1f800;
    let mut t = Tiler::default();
    let half = |v: f32| crate::gpu_shader::f32_to_f16(v) as u32;
    draw(&mut t, &m, &mut ram, &mut r, 0.75, [half(1.), half(1.) << 16], 0x7100);
    // The second (nearer) triangle covers the left half of the first.
    ram[0x4008..0x400c].copy_from_slice(&0f32.to_le_bytes());
    draw(&mut t, &m, &mut ram, &mut r, 0.25, [half(0.) | half(1.) << 16, half(1.) << 16], 0x7108);
    crate::gpu_render::PASS.with(|p| p.set(pass));
    crate::gpu_raster::BANDS.with(|p| p.set(bands));
    let result = t.fragment(&m, &mut ram, &f, (0, 0));
    crate::gpu_render::PASS.with(|p| p.set(None));
    crate::gpu_raster::BANDS.with(|p| p.set(None));
    result?;
    Ok((ram[0xe000..0xe000 + 32 * 48 * 4].to_vec(), u64::from_le_bytes(ram[0x1f800..0x1f808].try_into().unwrap())))
}
fn passes(pass: Option<(usize, usize)>, tiled: bool) -> Result<(Vec<u8>, u64), Stop> {
    passes_at(pass, tiled, 0, None)
}
#[test]
fn passes_with_an_odd_box_start_and_fragment_workers_match_one_pass() {
    for tiled in [false, true] {
        for min in [0u32, (5 << 16) | 3, (17 << 16) | 1, 31 << 16] {
            let whole = passes_at(None, tiled, min, Some((1, 16))).unwrap();
            for pass in [Some((32 * 21 * 16, 1 << 20)), Some((32 * 21 * 32, 1 << 20))] {
                for bands in [Some((1, 16)), Some((4, 4)), Some((2, 2))] {
                    let p = passes_at(pass, tiled, min, bands).unwrap();
                    assert_eq!(p, whole, "tiled {tiled} min {min:x} pass {pass:?} bands {bands:?}");
                }
            }
        }
    }
}
#[test]
fn a_large_box_renders_in_passes_of_rows_with_the_same_result() {
    let whole = passes(None, false).unwrap();
    assert!(whole.1 > 0);
    assert!(whole.0.chunks(4).any(|p| p == [255, 0, 0, 255]));
    assert!(whole.0.chunks(4).any(|p| p == [0, 255, 0, 255]));
    // 16 rows of 32 pixels with one target: three passes.
    let pass = Some((32 * 21 * 16, 1 << 20));
    assert_eq!(passes(pass, false).unwrap(), whole);
    // U-interleaved writeback, whole and in passes, holds the same pixels.
    let s = crate::gpu_render::Surface { base: 0, row_stride: 2 * 256 * 4, tiled: true, bpp: 4, afbc: None };
    for p in [None, pass] {
        let (tiled, count) = passes(p, true).unwrap();
        assert_eq!(count, whole.1);
        for y in 0..48 {
            for x in 0..32 {
                let at = s.offset(x, y) as usize;
                let linear = (y * 32 + x) as usize * 4;
                assert_eq!(tiled[at..at + 4], whole.0[linear..linear + 4], "{x},{y}");
            }
        }
    }
    // Every pass visits every primitive: too many visits is a job error, before any
    // pixel is written.
    assert!(matches!(passes(Some((32 * 21 * 16, 5)), false), Err(Stop::Unsupported { .. })));
    // So is an occlusion query that cannot be written.
    let (m, mut ram, mut r) = fixture();
    let f = coloured(&mut ram, &mut r);
    r[57] |= 3 << 14;
    r[46] = 0x10_0000;
    let mut t = Tiler::default();
    draw(&mut t, &m, &mut ram, &mut r, 0.5, [0x3c00, 0x3c00 << 16], 0x7100);
    let before = ram.clone();
    assert!(matches!(t.fragment(&m, &mut ram, &f, (0, 0)), Err(Stop::Memory(_))));
    assert_eq!(ram, before);
}
/// The lowest budget a fragment job of `frame` succeeds with (`frame` None: the tiler's).
fn least_budget(job: impl Fn(u64) -> bool) -> u64 {
    let (mut lo, mut hi) = (0u64, 1 << 24);
    assert!(job(hi));
    while lo < hi {
        let mid = (lo + hi) / 2;
        if job(mid) {
            hi = mid;
        } else {
            lo = mid + 1;
        }
    }
    lo
}
#[test]
fn the_fragment_budget_is_for_the_whole_job() {
    for bands in [Some((1, 16)), Some((4, 4))] {
        let budget = |draws: usize| {
            least_budget(|b| {
                let (m, mut ram, mut r) = fixture();
                let f = coloured(&mut ram, &mut r);
                let mut t = Tiler::default();
                let half = |v: f32| crate::gpu_shader::f32_to_f16(v) as u32;
                for _ in 0..draws {
                    draw(&mut t, &m, &mut ram, &mut r, 0.5, [half(1.), half(1.) << 16], 0x7100);
                }
                crate::gpu_raster::BANDS.with(|c| c.set(bands));
                crate::gpu_raster::BUDGET.with(|c| c.set(Some(b)));
                let ok = t.fragment(&m, &mut ram, &f, (0, 0)).is_ok();
                crate::gpu_raster::BUDGET.with(|c| c.set(None));
                crate::gpu_raster::BANDS.with(|c| c.set(None));
                ok
            })
        };
        let one = budget(1);
        assert!(one > 0);
        assert_eq!(budget(3), 3 * one);
    }
}
/// A frame of one line from `a` to `b` (window x, y, z, 1/w) without a fragment shader.
fn line_frame(a: [f32; 4], b: [f32; 4]) -> Frame {
    let mut regs = [0u32; 256];
    regs[43] = 0xf000f;
    regs[45] = 1f32.to_bits();
    regs[60] = 1f32.to_bits();
    let draw = Draw { regs, kind: LINES, positions: vec![a, b], primitives: vec![[0, 1, 1, 1]], ..Draw::default() };
    Frame { draws: vec![draw], ..Frame::default() }
}
fn line_budget(a: [f32; 4], b: [f32; 4]) -> u64 {
    least_budget(|budget| {
        let (m, mut ram, mut r) = fixture();
        let f = coloured(&mut ram, &mut r);
        crate::gpu_raster::BANDS.with(|c| c.set(Some((1, 16))));
        crate::gpu_raster::BUDGET.with(|c| c.set(Some(budget)));
        let ok = crate::gpu_render::fragment_frame(&m, &mut ram, &f, Some(&line_frame(a, b))).is_ok();
        crate::gpu_raster::BUDGET.with(|c| c.set(None));
        crate::gpu_raster::BANDS.with(|c| c.set(None));
        ok
    })
}
#[test]
fn lines_visit_only_the_quads_near_them() {
    // Not finite: nothing is visited (the bounding box used to be the whole buffer).
    for bad in [f32::NAN, f32::INFINITY, f32::NEG_INFINITY] {
        assert_eq!(line_budget([bad, 0., 0.5, 1.], [16., 16., 0.5, 1.]), 0);
        assert_eq!(line_budget([0., 0., 0.5, 1.], [16., bad, 0.5, 1.]), 0);
    }
    // A diagonal across the 16x16 buffer (8x8 quads): the quads along it, not all 64.
    let diagonal = line_budget([0., 0., 0.5, 1.], [16., 16., 0.5, 1.]);
    assert!((8..=32).contains(&diagonal), "{diagonal}");
    // A line crossing w = 0 is clipped there, not dropped; one wholly behind is dropped.
    assert!(line_budget([2., 8., 0.5, 1.], [14., 8., 0.5, -1.]) > 0);
    assert_eq!(line_budget([2., 8., 0.5, -1.], [14., 8., 0.5, -1.]), 0);
}
