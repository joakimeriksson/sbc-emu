
use super::*;
fn fixture() -> (AddressSpace, Vec<u8>, [u32; 256]) {
    let mut r = vec![0x55; 8192];
    let mut f = [0u32; 48];
    f[8] = 3 | (3 << 16);
    f[9] = 1 | (1 << 16);
    f[10] = 2 | (2 << 16);
    f[12] = 1 << 29;
    f[32] = 1 << 26;
    f[33] = 1 | (19 << 3) | (2 << 8) | (0x688 << 16);
    f[40] = 4096;
    f[42] = 16;
    f[44..48].fill(0xff332211);
    for (i, v) in f.iter().enumerate() {
        r[i * 4..i * 4 + 4].copy_from_slice(&v.to_le_bytes());
    }
    let mut regs = [0; 256];
    regs[43] = 3 | (3 << 16);
    (
        AddressSpace {
            transtab: 0,
            transcfg: 2,
        },
        r,
        regs,
    )
}
#[test]
fn fragment_clear_respects_bounds_and_stride() {
    let (m, mut r, regs) = fixture();
    fragment(&m, &mut r, &regs).unwrap();
    for y in 0..4 {
        for x in 0..4 {
            let expected = if (1..=2).contains(&x) && (1..=2).contains(&y) {
                [0x11, 0x22, 0x33, 0xff]
            } else {
                [0x55; 4]
            };
            assert_eq!(&r[4096 + y * 16 + x * 4..4100 + y * 16 + x * 4], &expected);
        }
    }
}
#[test]
fn mesa_clear_handles_crc_extension_and_prevalidates_checksum_destination() {
    let (m, mut r, mut regs) = fixture();
    let target = r[128..192].to_vec();
    r[128..192].fill(0);
    r[192..256].copy_from_slice(&target);
    r[32..36].copy_from_slice(&0x000f000fu32.to_le_bytes());
    r[36..40].fill(0);
    r[40..44].copy_from_slice(&0x000f000fu32.to_le_bytes());
    r[44..48].copy_from_slice(&0x04001480u32.to_le_bytes());
    r[48..52].copy_from_slice(&0x80210000u32.to_le_bytes());
    r[132..136].copy_from_slice(&16u32.to_le_bytes());
    let checksum = 0xc0008020ffdf8020u64;
    r[136..144].copy_from_slice(&checksum.to_le_bytes());
    r[144..152].copy_from_slice(&6144u64.to_le_bytes());
    r[196..200].copy_from_slice(
        &(1u32 | (19 << 3) | (2 << 8) | (0x688 << 16) | (1 << 15) | (1 << 31)).to_le_bytes(),
    );
    r[232..236].copy_from_slice(&64u32.to_le_bytes());
    regs[42] = 0;
    regs[43] = 0x000f000f;
    fragment(&m, &mut r, &regs).unwrap();
    assert!(r[4096..5120]
        .chunks_exact(4)
        .all(|p| p == [0x11, 0x22, 0x33, 0xff]));
    assert_eq!(&r[6144..6152], &checksum.to_le_bytes());
    assert_eq!(&r[6152..6160], &[0x55; 8]);
    r[144..152].copy_from_slice(&8192u64.to_le_bytes());
    let before = r.clone();
    assert!(matches!(fragment(&m, &mut r, &regs), Err(Stop::Memory(_))));
    assert_eq!(r, before);
    // Even though CRC reads do not elide pixels, their memory access must
    // still fault when the checksum target is unmapped.
    r[48..52].copy_from_slice(&0x40210000u32.to_le_bytes());
    let before = r.clone();
    assert!(matches!(fragment(&m, &mut r, &regs), Err(Stop::Memory(_))));
    assert_eq!(r, before);
}
/// An XRGB8888 scanout buffer (Mesa's B8G8R8X8 writeback): blue first, alpha 0.
#[test]
fn bgrx_writeback_swizzle() {
    let (m, mut r, regs) = fixture();
    r[132..136].copy_from_slice(&(1u32 | (19 << 3) | (2 << 8) | (0x80a << 16)).to_le_bytes());
    fragment(&m, &mut r, &regs).unwrap();
    assert_eq!(&r[4096 + 16 + 4..4096 + 16 + 8], &[0x33, 0x22, 0x11, 0]);
}
#[test]
fn unsupported_geometry_and_bad_destination_leave_memory_unchanged() {
    let (m, mut r, regs) = fixture();
    let before = r.clone();
    r[56] = 1;
    assert!(fragment(&m, &mut r, &regs).is_err());
    assert_eq!(&r[4096..], &before[4096..]);
    r[56] = 0;
    r[160..164].copy_from_slice(&u32::MAX.to_le_bytes());
    assert!(fragment(&m, &mut r, &regs).is_err());
    assert_eq!(&r[4096..], &before[4096..]);
}
#[test]
/// The Linux clear demo: 2x2 regions of a larger framebuffer, no tie-break rule set.
fn small_clear_region_of_a_large_framebuffer() {
    let m = AddressSpace { transtab: 0, transcfg: 2 };
    let mut r = vec![0u8; 0x80000];
    let (w, h, x, y) = (320u32, 180u32, 2u32, 4u32);
    let mut f = [0u32; 48];
    f[8] = (w - 1) | ((h - 1) << 16);
    f[9] = x | (y << 16);
    f[10] = (x + 1) | ((y + 1) << 16);
    f[12] = 1 << 29;
    f[32] = 1 << 26;
    f[33] = 1 | (19 << 3) | (2 << 8) | (0x688 << 16);
    f[40] = 0x10000;
    f[42] = w * 4;
    f[44..48].fill(0xff112233);
    for (i, v) in f.iter().enumerate() {
        r[i * 4..i * 4 + 4].copy_from_slice(&v.to_le_bytes());
    }
    let mut regs = [0; 256];
    regs[43] = (w - 1) | ((h - 1) << 16);
    fragment(&m, &mut r, &regs).unwrap();
}
#[test]
fn pre_frame_shader_loads_the_tile_buffer_before_draws() {
    // A pre-frame DCD (Mesa's preload) whose shader writes a uniform colour through an
    // opaque blend descriptor: it replaces the clear colour in the bounds.
    let (m, mut r, regs) = fixture();
    r.resize(0x4000, 0);
    let put = |r: &mut Vec<u8>, at: usize, words: &[u32]| {
        for (n, v) in words.iter().enumerate() {
            r[at + n * 4..at + n * 4 + 4].copy_from_slice(&v.to_le_bytes());
        }
    };
    put(&mut r, 0, &[1]); // pre-frame 0: always
    put(&mut r, 24, &[0x2000, 0]); // frame shader DCDs
    let mut dcd = [0u32; 32];
    dcd[1] = 0x1ffff; // sample mask, RT 0
    dcd[7] = 1f32.to_bits();
    dcd[12] = 0x2101; // one blend descriptor at 0x2100
    dcd[17] = 1; // one FAU pair
    dcd[26] = 0x2200; // shader program
    dcd[30] = 0x2300; // FAU
    put(&mut r, 0x2000, &dcd);
    put(&mut r, 0x2100, &[0x200, 0xf0122122, 0x19, 0xed000]);
    put(&mut r, 0x2200, &[0x80000128, 0x3000, 0x2400, 0]);
    let h = |v: f32| crate::gpu_shader::f32_to_f16(v) as u32;
    put(&mut r, 0x2300, &[h(1.) | h(0.) << 16, h(0.) | h(1.) << 16]);
    let code = [0x0091c00000000080u64, 0x0091c10000000081, 0x087dbc0208ea407c, 0x787f4004333c00f0];
    for (n, i) in code.iter().enumerate() {
        r[0x2400 + n * 8..0x2408 + n * 8].copy_from_slice(&i.to_le_bytes());
    }
    fragment(&m, &mut r, &regs).unwrap();
    for y in 0..4 {
        for x in 0..4 {
            let at = 4096 + y * 16 + x * 4;
            let inside = (1..=2).contains(&x) && (1..=2).contains(&y);
            assert_eq!(&r[at..at + 4], if inside { &[255, 0, 0, 255] } else { &[0x55; 4] });
        }
    }
}
