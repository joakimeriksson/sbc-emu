//! The texture formatting unit: copies an image between memory layouts (raster/LT/UB-linear/UIF
//! to LT/UB-linear/UIF) and optionally generates its mipmaps (2x2 box filter).
//!
//! Registers (V3D 4.2, from hub offset 0x400): CS, SU, ICFG (kick: IOC bit 0, mip count 5-8,
//! texture type 9-15, input format 18-21, output pad 22-25), IIA, ICA, IIS (raster stride in
//! pixels or UIF padded height in UIF blocks), IUA, IOA (output address | DIMTW | format 3-5),
//! IOS (height << 16 | width), COEF0-3.

use crate::formats::{decode_texel, encode_texel, tex_fmt};
use crate::tiling::{self, Tiling};
use crate::Gmem;

pub fn run(regs: &[u32], m: &mut Gmem) {
    let icfg = regs[2];
    let (iia, iis, ioa, ios) = (regs[3], regs[5], regs[7], regs[8]);
    let nummm = (icfg >> 5) & 0xf;
    let ttype = (icfg >> 9) & 0x7f;
    let iformat = (icfg >> 18) & 0xf;
    let opad = (icfg >> 22) & 0xf;
    let (w, h) = (ios & 0xffff, ios >> 16);
    let f = tex_fmt(ttype);
    let cpp = f.cpp;
    if f.bw > 1 { crate::log_once("v3d: TFU on compressed formats not modelled"); return; }
    let in_tiling = match iformat { 0 => Tiling::Raster, 11 => Tiling::Lt, 12 => Tiling::UbLinear1, 13 => Tiling::UbLinear2, 14 => Tiling::UifNoXor, 15 => Tiling::UifXor,
        _ => { crate::log_once(&format!("v3d: TFU input format {iformat} not modelled")); return; } };
    let (in_stride, in_h) = match in_tiling {
        Tiling::Raster => (iis * cpp, h),
        Tiling::UifNoXor | Tiling::UifXor => (0, iis * 2 * tiling::utile_h(cpp)),
        _ => (w * cpp, h),
    };
    let out_tiling = Tiling::from_memory_format(((ioa >> 3) & 7).saturating_sub(2));
    let out_base = ioa & !0x3f;
    crate::log(&format!("v3d: TFU {w}x{h} type {ttype} {in_tiling:?} {iia:#x} -> {out_tiling:?} {out_base:#x}, {nummm} mips"));
    // the output chain, laid out like a texture whose level 0 has this tiling
    let uif = matches!(out_tiling, Tiling::UifNoXor | Tiling::UifXor);
    let levels = tiling::mip_chain(w, h, 1, cpp, 1, 1, nummm, false, uif, out_tiling == Tiling::UifXor, opad);
    let mut src: Vec<u8> = vec![0; (w * h * cpp) as usize];
    let mut px = vec![0u8; cpp as usize];
    for y in 0..h {
        for x in 0..w {
            let o = tiling::offset(in_tiling, cpp, in_stride, in_h, x, y);
            m.read(iia.wrapping_add(o), &mut px);
            let d = ((y * w + x) * cpp) as usize;
            src[d..d + cpp as usize].copy_from_slice(&px);
        }
    }
    let (mut lw, mut lh) = (w, h);
    for l in 0..=nummm as usize {
        let lv = &levels[l];
        let base = (out_base as i64 + lv.offset) as u32;
        let t = if l == 0 { out_tiling } else { lv.tiling };
        for y in 0..lh {
            for x in 0..lw {
                let o = tiling::offset(t, cpp, lv.stride, lv.ph, x, y);
                let d = ((y * lw + x) * cpp) as usize;
                m.write(base.wrapping_add(o), &src[d..d + cpp as usize]);
            }
        }
        if l as u32 == nummm { break; }
        // next level: 2x2 box filter
        let (nw, nh) = ((lw / 2).max(1), (lh / 2).max(1));
        let mut next = vec![0u8; (nw * nh * cpp) as usize];
        for y in 0..nh {
            for x in 0..nw {
                let mut acc = [0f32; 4];
                let mut n = 0.0;
                for (dx, dy) in [(0, 0), (1, 0), (0, 1), (1, 1)] {
                    let (sx, sy) = ((2 * x + dx).min(lw - 1), (2 * y + dy).min(lh - 1));
                    let d = ((sy * lw + sx) * cpp) as usize;
                    let t = decode_texel(ttype, &src[d..d + cpp as usize]);
                    for k in 0..4 { acc[k] += t.ch(k); }
                    n += 1.0;
                }
                let d = ((y * nw + x) * cpp) as usize;
                encode_texel(ttype, [acc[0] / n, acc[1] / n, acc[2] / n, acc[3] / n], &mut next[d..d + cpp as usize]);
            }
        }
        src = next;
        lw = nw;
        lh = nh;
    }
}
