use super::*;
use crate::desc::{Compare, CubeEdges, Dim, Layout, MipFilter, Reduction, Source, Tic, Tsc, Wrap};
use crate::num::Num;
use crate::texture::{TexError, Texture};

#[test]
fn tic_block_linear() {
    let mut v = View::new(0x2b, 5, 1920, 1080, 7);
    v.types = [UNORM, UINT, SINT, SNORM];
    v.swizzle = [R, A, ZERO, ONE];
    v.last_level = 10;
    v.view_first = 2;
    v.view_last = 9;
    v.block = BlockDims::new(4, 2);
    v.ms = MsMode::X8;
    v.address = 0xab_1234_5600;
    let t = Tic::decode(&v.block_linear()).expect("valid");
    assert_eq!(t.format, 0x2b);
    assert_eq!(t.types, [Some(Num::Unorm), Some(Num::Uint), Some(Num::Sint), Some(Num::Snorm)]);
    assert_eq!(t.swizzle, [Source::R, Source::A, Source::Zero, Source::OneFloat]);
    assert_eq!(t.address, 0xab_1234_5600);
    assert_eq!(t.layout, Layout::BlockLinear { block: BlockDims::new(4, 2) });
    assert_eq!((t.dim, t.width, t.height, t.depth), (Dim::D2Array, 1920, 1080, 7));
    assert_eq!((t.max_level, t.view_min, t.view_max), (10, 2, 9));
    assert_eq!(t.ms, MsMode::X8);
    assert!(t.normalized && t.border_from_sampler && !t.srgb);
    assert_eq!(t.color_key, None);
}

#[test]
fn tic_pitch_and_buffer() {
    let mut v = View::new(8, 7, 1366, 768, 1);
    v.pitch = 5504;
    v.srgb = true;
    v.scaled = true;
    v.address = 0x1_0000_0020;
    let t = Tic::decode(&v.pitch()).expect("valid");
    assert_eq!(t.layout, Layout::Pitch { pitch: 5504 });
    assert_eq!((t.dim, t.width, t.height, t.address), (Dim::D2NoMip, 1366, 768, 0x1_0000_0020));
    assert!(t.srgb && !t.normalized);

    let mut v = View::new(1, 6, 0x12_3456, 1, 1);
    v.address = 0x7_0000_0011;
    let t = Tic::decode(&v.buffer()).expect("valid");
    assert_eq!((t.layout, t.dim), (Layout::Buffer, Dim::Buffer));
    assert_eq!((t.width, t.address), (0x12_3456, 0x7_0000_0011));
}

#[test]
fn tic_other_headers() {
    // Colour key: the last word is the key.
    let mut w = View::new(8, 1, 16, 16, 1).block_linear();
    w[2] = (w[2] & !(7 << 21)) | 4 << 21;
    w[3] |= 3 << 28;
    w[7] = 0xff00ff00;
    let t = Tic::decode(&w).expect("valid");
    assert_eq!((t.color_key, t.view_min, t.view_max), (Some(0xff00ff00), 0, 3));
    // Reserved version and texture type.
    w[2] |= 7 << 21;
    assert!(Tic::decode(&w).is_err());
    let mut w = View::new(8, 1, 16, 16, 1).block_linear();
    w[4] |= 15 << 23;
    assert!(Tic::decode(&w).is_err());
    // The minimum LOD of a Vulkan view, 4.8 fixed point.
    let mut w = View::new(8, 1, 16, 16, 1).block_linear();
    w[7] |= 0x180 << 12;
    assert_eq!(Tic::decode(&w).map(|t| t.min_lod_clamp), Ok(1.5));
    // ASTC is not implemented; a null view (first level after the last) samples as zeros.
    let w = View::new(0x40, 1, 16, 16, 1).block_linear();
    assert_eq!(Texture::from_words(&w).err(), Some(TexError::Format(0x40)));
    let mut v = View::new(0x1d, 5, 1, 1, 1);
    v.view_first = 1;
    assert!(Texture::from_words(&v.block_linear()).is_ok_and(|t| t.null));
    assert!(Texture::from_words(&[0; 8]).is_err());
}

#[test]
fn tsc() {
    let mut s = Smp::new(true, 0);
    s.wrap = [0, 1, 3];
    s.min_linear = false;
    s.mip = 2;
    s.aniso = 16;
    s.compare = Some(3);
    s.seamless = true;
    s.bias = -2.5;
    s.lod = (1.25, 9.5);
    s.border = [1, 2, 3, 4];
    s.reduction = 2;
    let t = Tsc::decode(&s.words());
    assert_eq!(t.wrap, [Wrap::Repeat, Wrap::Mirror, Wrap::Border]);
    assert!(t.mag_linear && !t.min_linear && t.srgb && !t.force_unnormalized);
    assert_eq!((t.mip, t.max_aniso, t.compare), (MipFilter::Linear, 16, Some(Compare::LessEqual)));
    assert_eq!((t.cube_edges, t.reduction), (CubeEdges::Seamless, Reduction::Max));
    assert_eq!((t.lod_bias, t.min_lod, t.max_lod), (-2.5, 1.25, 9.5));
    assert_eq!(t.border, [1, 2, 3, 4]);
    s.unnormalized = true;
    s.seamless = false;
    s.mip = 0;
    for (n, want) in [(1, 1), (2, 2), (4, 4), (8, 8), (12, 12)] {
        s.aniso = n;
        assert_eq!(Tsc::decode(&s.words()).max_aniso, want);
    }
    let t = Tsc::decode(&s.words());
    assert!(t.force_unnormalized);
    assert_eq!((t.mip, t.cube_edges), (MipFilter::None, CubeEdges::Wrap));
    // The driver's first sampler, for texel fetches: only sRGB conversion.
    let t = Tsc::decode(&[0x2000, 0, 0, 0, 0, 0, 0, 0]);
    assert!(t.srgb && !t.mag_linear && t.compare.is_none());
    for i in 0..8u32 {
        assert_eq!(Wrap::Mirror as u32 == i, Tsc::decode(&[i, 0, 0, 0, 0, 0, 0, 0]).wrap[0] == Wrap::Mirror);
        let c = Compare::from_bits(i);
        assert_eq!(c as u32, i);
        assert_eq!(c.test(0.25, 0.5), [false, true, false, true, false, true, false, true][i as usize]);
        assert_eq!(c.test(0.5, 0.5), [false, false, true, true, false, false, true, true][i as usize]);
        assert_eq!(c.test(0.75, 0.5), [false, false, false, false, true, true, true, true][i as usize]);
    }
}
