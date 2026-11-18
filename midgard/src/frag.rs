//! Fragment jobs: render one framebuffer's polygon list (Mesa `genxml/v5.xml`:
//! "Framebuffer", "Render Target", "ZS CRC Extension", "Renderer State", "Blend").
//!
//! The model initialises a host tile buffer with the clear values (or the preloaded
//! surfaces), rasterises the list's primitives in 2x2 quads with the top-left rule, runs
//! the fragment shader per quad (so derivatives see their neighbours), applies the alpha,
//! depth and stencil tests and fixed-function blending, and writes the touched 16x16
//! tiles back in the render targets' formats and layouts.

use crate::draw::{self, Dcd, DrawState, Prim, Rsd};
use crate::format::{linear_to_srgb, srgb_to_linear, u_interleaved_offset, PixelFormat};
use crate::half::{f16_to_f32, f32_to_f16};
use crate::isa::{Core, Env, LdSt, LdStArgs, State, TexWord, Thread};
use crate::tex;
use crate::vm::Vm;

const TILE: u32 = 16;

#[derive(Clone, Copy, Debug, Default)]
struct RenderTarget {
    internal: u32,
    write: bool,
    wb_format: u32,
    block: u32,
    srgb: bool,
    swizzle: u32,
    clean_write: bool,
    dither: bool,
    preload: bool,
    base: u64,
    row_stride: u32,
    clear: [u32; 4],
    preload_base: u64,
    preload_stride: u32,
}

#[derive(Clone, Copy, Debug, Default)]
struct Zs {
    present: bool,
    zs_format: u32,
    zs_block: u32,
    s_format: u32,
    s_block: u32,
    zs_base: u64,
    zs_stride: u32,
    s_base: u64,
    s_stride: u32,
}

struct Fb {
    width: u32,
    height: u32,
    z_clear: f32,
    s_clear: u8,
    z_write: bool,
    s_write: bool,
    z_preload: bool,
    s_preload: bool,
    z_internal: u32,
    rts: Vec<RenderTarget>,
    zs: Zs,
}

fn read_fb(vm: &mut Vm, tagged: u64) -> (Fb, u64) {
    let base = tagged & !63;
    let has_ext = tagged & 2 != 0;
    let rt_count = ((tagged >> 2) & 15) as usize + 1;
    let p: [u32; 6] = vm.words(base + 32);
    let mut fb = Fb {
        width: (p[0] & 0xffff) + 1,
        height: (p[0] >> 16) + 1,
        z_clear: f32::from_bits(p[5]),
        s_clear: (p[4] & 0xff) as u8,
        s_write: p[4] & (1 << 8) != 0,
        s_preload: p[4] & (1 << 9) != 0,
        z_internal: (p[4] >> 16) & 3,
        z_write: p[4] & (1 << 18) != 0,
        z_preload: p[4] & (1 << 19) != 0,
        rts: Vec::new(),
        zs: Zs::default(),
    };
    let mut rt_va = base + 128;
    if has_ext {
        let e: [u32; 16] = vm.words(base + 128);
        fb.zs = Zs {
            present: true,
            zs_format: e[3] & 15,
            zs_block: (e[3] >> 4) & 3,
            s_format: (e[3] >> 16) & 15,
            s_block: (e[3] >> 20) & 3,
            zs_base: e[4] as u64 | (e[5] as u64) << 32,
            zs_stride: e[6],
            s_base: e[8] as u64 | (e[9] as u64) << 32,
            s_stride: e[10],
        };
        rt_va += 64;
    }
    for i in 0..rt_count {
        let w: [u32; 16] = vm.words(rt_va + 64 * i as u64);
        fb.rts.push(RenderTarget {
            internal: (w[0] >> 26) & 63,
            write: w[1] & 1 != 0,
            wb_format: (w[1] >> 3) & 31,
            block: (w[1] >> 10) & 3,
            srgb: w[1] & (1 << 14) != 0,
            swizzle: (w[1] >> 16) & 0xfff,
            clean_write: w[1] & (1 << 31) != 0,
            dither: w[1] & (1 << 15) != 0,
            preload: w[2] & 1 != 0,
            base: w[8] as u64 | (w[9] as u64) << 32,
            row_stride: w[10],
            clear: [w[12], w[13], w[14], w[15]],
            preload_base: w[12] as u64 | (w[13] as u64) << 32,
            preload_stride: w[14],
        });
    }
    let polygon_list = vm.r64(base + 56 + 8);
    let _ = polygon_list;
    (fb, base)
}

/// Bits per channel of the blendable tile-buffer formats.
fn internal_bits(internal: u32) -> Option<[u32; 4]> {
    Some(match internal {
        1 => [8, 8, 8, 8],
        2 => [10, 10, 10, 2],
        3 => [8, 8, 8, 2],
        4 => [4, 4, 4, 4],
        5 => [5, 6, 5, 0],
        6 => [5, 5, 5, 1],
        _ => return None,
    })
}

/// Fraction bits of each channel in packed clear colours (`pan_clear.c` tib layouts).
fn internal_layout(internal: u32) -> [(u32, u32); 4] {
    match internal {
        2 => [(10, 0), (10, 0), (10, 0), (2, 0)],
        3 => [(8, 2), (8, 2), (8, 2), (2, 0)],
        4 => [(4, 4), (4, 4), (4, 4), (4, 4)],
        5 => [(5, 5), (6, 4), (5, 5), (0, 2)],
        6 => [(5, 5), (5, 5), (5, 5), (1, 1)],
        _ => [(8, 0), (8, 0), (8, 0), (8, 0)],
    }
}

fn unpack_clear(internal: u32, w: u32) -> [f32; 4] {
    let l = internal_layout(internal);
    let mut shift = 0;
    let mut out = [0f32; 4];
    for c in 0..4 {
        let (i, f) = l[c];
        let n = i + f;
        let v = (w >> shift) & ((1u64 << n) - 1) as u32;
        shift += n;
        out[c] = if i == 0 { 1.0 } else { v as f32 / (((1u32 << i) - 1) << f) as f32 };
    }
    out
}

/// Store a colour at the tile buffer's precision: each channel's integer bits plus the
/// fraction bits kept for dithering (`pan_clear.c` tib layouts).
fn quantize(internal: u32, c: [f32; 4]) -> [f32; 4] {
    if internal_bits(internal).is_none() {
        return c;
    }
    let l = internal_layout(internal);
    let mut o = [0f32; 4];
    for i in 0..4 {
        let (int, frac) = l[i];
        if int == 0 {
            o[i] = 1.0;
        } else {
            let m = (((1u32 << int) - 1) << frac) as f32;
            o[i] = (c[i].clamp(0.0, 1.0) * m).round() / m;
        }
    }
    o
}

/// Channel bits of a writeback colour format with fewer than 8 bits per channel.
fn wb_low_bits(f: u32) -> Option<[u32; 4]> {
    match f {
        20 => Some([4, 4, 4, 4]),
        21 => Some([5, 6, 5, 8]),
        28 | 29 => Some([5, 5, 5, 1]),
        _ => None,
    }
}

/// 4x4 ordered dither offset in [-0.5, 0.5) of one quantisation step.
fn bayer(x: u32, y: u32) -> f32 {
    const M: [[u8; 4]; 4] = [[0, 8, 2, 10], [12, 4, 14, 6], [3, 11, 1, 9], [15, 7, 13, 5]];
    (M[(y & 3) as usize][(x & 3) as usize] as f32 + 0.5) / 16.0 - 0.5
}

/// Bytes per pixel of a writeback colour format.
fn wb_bytes(f: u32) -> u32 {
    match f {
        0 => 1,
        1 => 2,
        2 => 3,
        3 => 4,
        4 => 6,
        5 => 8,
        6 => 12,
        7 => 16,
        16 => 1,
        17 => 2,
        18 => 3,
        19 | 22 | 24 | 25 => 4,
        20 | 21 | 28 | 29 => 2,
        _ => 4,
    }
}

fn swz(sw: u32, v: [f32; 4]) -> [f32; 4] {
    let mut o = [0f32; 4];
    for c in 0..4 {
        o[c] = match (sw >> (3 * c)) & 7 {
            s @ 0..=3 => v[s as usize],
            5 => 1.0,
            _ => 0.0,
        };
    }
    o
}

/// Inverse of `swz` for reading a surface back into the tile buffer.
fn unswz(sw: u32, mem: [f32; 4], fallback: [f32; 4]) -> [f32; 4] {
    let mut o = fallback;
    for c in 0..4 {
        let s = (sw >> (3 * c)) & 7;
        if s < 4 {
            o[s as usize] = mem[c];
        }
    }
    o
}

fn encode_wb(f: u32, v: [f32; 4], out: &mut [u8; 16]) {
    let q = |x: f32, m: f32| (x.clamp(0.0, 1.0) * m).round() as u32;
    match f {
        16..=19 => {
            for c in 0..(f - 15) as usize {
                out[c] = q(v[c], 255.0) as u8;
            }
        }
        22 => {
            for c in 0..4 {
                out[c] = q(v[c], 255.0) as u8;
            }
        }
        20 => {
            let w = q(v[0], 15.0) | q(v[1], 15.0) << 4 | q(v[2], 15.0) << 8 | q(v[3], 15.0) << 12;
            out[..2].copy_from_slice(&(w as u16).to_le_bytes());
        }
        21 => {
            let w = q(v[0], 31.0) | q(v[1], 63.0) << 5 | q(v[2], 31.0) << 11;
            out[..2].copy_from_slice(&(w as u16).to_le_bytes());
        }
        24 => {
            let w = q(v[0], 1023.0) | q(v[1], 1023.0) << 10 | q(v[2], 1023.0) << 20 | q(v[3], 3.0) << 30;
            out[..4].copy_from_slice(&w.to_le_bytes());
        }
        25 => {
            let w = q(v[3], 3.0) | q(v[2], 1023.0) << 2 | q(v[1], 1023.0) << 12 | q(v[0], 1023.0) << 22;
            out[..4].copy_from_slice(&w.to_le_bytes());
        }
        28 => {
            let w = q(v[0], 31.0) | q(v[1], 31.0) << 5 | q(v[2], 31.0) << 10 | q(v[3], 1.0) << 15;
            out[..2].copy_from_slice(&(w as u16).to_le_bytes());
        }
        29 => {
            let w = q(v[3], 1.0) | q(v[2], 31.0) << 1 | q(v[1], 31.0) << 6 | q(v[0], 31.0) << 11;
            out[..2].copy_from_slice(&(w as u16).to_le_bytes());
        }
        _ => {
            for c in 0..4 {
                out[c] = q(v[c], 255.0) as u8;
            }
        }
    }
}

fn decode_wb(f: u32, b: &[u8; 16]) -> [f32; 4] {
    let n = |x: u32, m: f32| x as f32 / m;
    let h = u16::from_le_bytes([b[0], b[1]]) as u32;
    let w = u32::from_le_bytes([b[0], b[1], b[2], b[3]]);
    match f {
        16..=19 => {
            let mut o = [0.0, 0.0, 0.0, 1.0];
            for c in 0..(f - 15) as usize {
                o[c] = n(b[c] as u32, 255.0);
            }
            o
        }
        20 => [n(h & 15, 15.0), n((h >> 4) & 15, 15.0), n((h >> 8) & 15, 15.0), n(h >> 12, 15.0)],
        21 => [n(h & 31, 31.0), n((h >> 5) & 63, 63.0), n(h >> 11, 31.0), 1.0],
        24 => [n(w & 1023, 1023.0), n((w >> 10) & 1023, 1023.0), n((w >> 20) & 1023, 1023.0), n(w >> 30, 3.0)],
        28 => [n(h & 31, 31.0), n((h >> 5) & 31, 31.0), n((h >> 10) & 31, 31.0), n(h >> 15, 1.0)],
        _ => [n(b[0] as u32, 255.0), n(b[1] as u32, 255.0), n(b[2] as u32, 255.0), n(b[3] as u32, 255.0)],
    }
}

fn surface_offset(block: u32, x: u32, y: u32, stride: u32, bpp: u32) -> u64 {
    if block == 0 {
        u_interleaved_offset(x, y, stride, bpp)
    } else {
        y as u64 * stride as u64 + x as u64 * bpp as u64
    }
}

fn func(f: u32, a: f32, b: f32) -> bool {
    match f {
        0 => false,
        1 => a < b,
        2 => a == b,
        3 => a <= b,
        4 => a > b,
        5 => a != b,
        6 => a >= b,
        _ => true,
    }
}

fn stencil_op(op: u32, s: u8, r: u8) -> u8 {
    match op {
        1 => r,
        2 => 0,
        3 => !s,
        4 => s.wrapping_add(1),
        5 => s.wrapping_sub(1),
        6 => s.saturating_add(1),
        7 => s.saturating_sub(1),
        _ => s,
    }
}

/// Blend function A + B * C on one channel (`genxml` "Blend Function").
fn blend_channel(f: u32, s: f32, d: f32, sa: f32, da: f32, k: f32) -> f32 {
    let a = match f & 3 {
        1 => 0.0,
        2 => s,
        3 => d,
        _ => 0.0,
    };
    let a = if f & 8 != 0 { -a } else { a };
    let b = match (f >> 4) & 3 {
        0 => s - d,
        1 => s + d,
        2 => s,
        _ => d,
    };
    let b = if f & 0x80 != 0 { -b } else { b };
    let c = match (f >> 8) & 7 {
        1 => 0.0,
        2 => s,
        3 => d,
        4 => 2.0 * s,
        5 => sa,
        6 => da,
        7 => k,
        _ => 0.0,
    };
    let c = if f & 0x800 != 0 { 1.0 - c } else { c };
    a + b * c
}

/// Per-thread fragment inputs.
#[derive(Clone, Copy, Default)]
struct Pixel {
    x: u32,
    y: u32,
    /// Screen-space barycentrics.
    b: [f32; 3],
    front: bool,
    point: [f32; 2],
}

struct Output {
    rt: u32,
    color: u128,
    half: bool,
    depth: Option<f32>,
    #[allow(dead_code)]
    stencil: Option<u8>,
}

struct FragEnv<'a, 'b> {
    vm: &'a mut Vm<'b>,
    dcd: Dcd,
    prim: Prim,
    px: [Pixel; 4],
    out: [Vec<Output>; 4],
    depth_from_shader: bool,
    stencil_from_shader: bool,
    tb: *mut TileBuf,
}

struct TileBuf {
    x0: u32,
    y0: u32,
    w: u32,
    h: u32,
    color: Vec<Vec<[f32; 4]>>,
    raw: Vec<Vec<u128>>,
    depth: Vec<f32>,
    stencil: Vec<u8>,
    touched: Vec<bool>,
}

impl TileBuf {
    fn idx(&self, x: u32, y: u32) -> usize {
        ((y - self.y0) * self.w + (x - self.x0)) as usize
    }
}

impl FragEnv<'_, '_> {
    fn interpolate(&mut self, t: usize, index: u32, params: u32, op: u32) -> [u32; 4] {
        let at = draw::read_attr(self.vm, self.dcd.varyings, self.dcd.vary_bufs, index);
        let p = self.px[t];
        let pos = self.prim.pos;
        let n = match self.prim.kind {
            draw::POINTS => 1,
            draw::LINES => 2,
            _ => 3,
        };
        let z = (0..n).map(|i| p.b[i] * pos[i][2]).sum::<f32>();
        let inv_w = (0..n).map(|i| p.b[i] * pos[i][3]).sum::<f32>();
        match at.buf.special {
            37 => return [(p.x as f32 + 0.5).to_bits(), (p.y as f32 + 0.5).to_bits(), z.to_bits(), inv_w.to_bits()],
            38 => return [if p.front { u32::MAX } else { 0 }, 0, 0, 1],
            33 | 97 => {
                let y = if at.buf.special == 97 { 1.0 - p.point[1] } else { p.point[1] };
                return [p.point[0].to_bits(), y.to_bits(), 0, 1.0f32.to_bits()];
            }
            0 => {}
            _ => return [0; 4],
        }
        let flat = params & 8 != 0 || op == 0x9a || op == 0x9b;
        let perspective = params & 16 != 0;
        let mut vals = [[0u32; 4]; 3];
        for i in 0..n {
            vals[i] = draw::load_attr(self.vm, &at, self.prim.v[i]);
        }
        let class = at.format.class();
        if flat || class != crate::format::Class::Float {
            let pv = if self.prim.draw.first_provoking { 0 } else { n - 1 };
            return vals[pv];
        }
        // perspective-correct weights
        let mut wgt = p.b;
        if perspective && n > 1 {
            let mut sum = 0.0;
            for i in 0..n {
                wgt[i] = p.b[i] * pos[i][3];
                sum += wgt[i];
            }
            if sum != 0.0 {
                for w in wgt.iter_mut().take(n) {
                    *w /= sum;
                }
            }
        }
        // corner weights to weights of the original vertices (clipped triangles)
        let mut ow = [0f32; 3];
        for k in 0..n {
            for j in 0..n {
                ow[j] += wgt[k] * self.prim.corner[k][j];
            }
        }
        let mut o = [0f32; 4];
        for c in 0..4 {
            o[c] = (0..n).map(|i| ow[i] * f32::from_bits(vals[i][c])).sum();
        }
        // perspective division modifiers
        if let m @ 1..=3 = params & 7 {
            let d = o[m as usize];
            if d != 0.0 {
                for c in 0..3 {
                    o[c] /= d;
                }
            }
        }
        o.map(f32::to_bits)
    }
}

impl Env for FragEnv<'_, '_> {
    fn spend(&mut self, n: u64) -> bool {
        self.vm.spend(n)
    }

    fn code(&mut self, va: u64) -> [u8; 64] {
        let mut b = [0u8; 64];
        self.vm.read_bytes(va, &mut b);
        b
    }

    fn ldst(&mut self, t: usize, ins: &LdSt, a: LdStArgs, data: u128) -> u128 {
        let op = ins.op;
        match op {
            0x98..=0x9b => {
                let index = ((ins.offset >> 9) as i64 as u64).wrapping_add(a.index << ins.index_shift) as u32;
                let v = self.interpolate(t, index, (ins.offset & 0x1ff) as u32, op);
                if op == 0x99 { draw::to_half_lanes(v) } else { draw::to_u128(v) }
            }
            0xb8..=0xba => {
                // tile buffer read (framebuffer fetch / blend shaders)
                let rt = (ins.index_reg << 2 | ins.index_comp) as usize;
                let tb = unsafe { &*self.tb };
                let p = self.px[t];
                if p.x < tb.x0 || p.y < tb.y0 || p.x >= tb.x0 + tb.w || p.y >= tb.y0 + tb.h {
                    return 0;
                }
                let i = tb.idx(p.x, p.y);
                if op == 0xba {
                    return tb.raw.get(rt).map_or(0, |r| r[i]);
                }
                let c = tb.color.get(rt).map_or([0.0; 4], |c| c[i]);
                let v = c.map(f32::to_bits);
                if op == 0xb9 { draw::to_half_lanes(v) } else { draw::to_u128(v) }
            }
            _ => draw::common_ldst(self.vm, &self.dcd, ins, a, data, 0).unwrap_or(0),
        }
    }

    fn texture(&mut self, w: &TexWord, coords: &[[f32; 4]; 4], lod: &[f32; 4], active: u8) -> [[u32; 4]; 4] {
        tex::sample(self.vm, &self.dcd, w, coords, lod, active, true)
    }

    fn writeout(&mut self, t: usize, r0: u128, half: bool, r1: u128, written: u8) {
        let lane = |i: u32| (r1 >> (32 * i)) as u32;
        let rt_code = if written & 4 != 0 { lane(2) } else { 0 };
        let depth = if self.depth_from_shader { Some(f32::from_bits(lane(0))) } else { None };
        let stencil = if self.stencil_from_shader { Some(lane(1) as u8) } else { None };
        let rt = if rt_code == 0xff { u32::MAX } else { rt_code >> 8 };
        self.out[t].push(Output { rt, color: r0, half, depth, stencil });
    }
}

/// Largest piece of a fragment job rendered at once (a multiple of the tile size): the
/// tile buffers of a job are allocated per chunk, so a huge framebuffer (up to 65536 x
/// 65536 in the descriptor) never needs host memory in proportion to its area.
const CHUNK: u32 = 64 * TILE;

/// Render a fragment job. Returns GPU cycles.
pub fn fragment_job(vm: &mut Vm, job: u64, list: &[Prim], trace: bool) -> u64 {
    let p: [u32; 8] = vm.words(job + 32);
    let fbd = p[2] as u64 | (p[3] as u64) << 32;
    let (fb, _) = read_fb(vm, fbd);
    let tx0 = p[0] & 0xfff;
    let ty0 = (p[0] >> 16) & 0xfff;
    let tx1 = p[1] & 0xfff;
    let ty1 = (p[1] >> 16) & 0xfff;
    let x0 = (tx0 * TILE).min(fb.width);
    let y0 = (ty0 * TILE).min(fb.height);
    let x1 = ((tx1 + 1) * TILE).min(fb.width);
    let y1 = ((ty1 + 1) * TILE).min(fb.height);
    if x1 <= x0 || y1 <= y0 {
        return 100;
    }
    if trace {
        eprintln!("[mali] fragment {}x{} at ({x0},{y0}) {} primitives, fbd {fbd:#x}", x1 - x0, y1 - y0, list.len());
        for rt in &fb.rts {
            eprintln!("[mali]   rt internal {} writeback {} block {} dither {} base {:#x} stride {}", rt.internal, rt.wb_format, rt.block, rt.dither, rt.base, rt.row_stride);
        }
        for p in list.iter().take(3) {
            eprintln!("[mali]   prim {} v {:?} pos {:?}", p.kind, p.v, p.pos);
        }
    }
    let mut cycles = 0;
    let mut cy = y0;
    while cy < y1 {
        let mut cx = x0;
        while cx < x1 {
            let (cw, ch) = ((cx + CHUNK).min(x1) - cx, (cy + CHUNK).min(y1) - cy);
            if !vm.spend(cw as u64 * ch as u64) {
                return cycles;
            }
            cycles += fragment_chunk(vm, &fb, list, cx, cy, (cx + CHUNK).min(x1), (cy + CHUNK).min(y1));
            cx += CHUNK;
        }
        cy += CHUNK;
    }
    cycles
}

/// Render the part (`x0`, `y0`)..(`x1`, `y1`) of a fragment job: load the tile buffer,
/// rasterise every primitive into it, write it back. Returns GPU cycles.
fn fragment_chunk(vm: &mut Vm, fb: &Fb, list: &[Prim], x0: u32, y0: u32, x1: u32, y1: u32) -> u64 {
    let (w, h) = (x1 - x0, y1 - y0);
    let n = w as usize * h as usize;
    let mut tb = TileBuf {
        x0,
        y0,
        w,
        h,
        color: Vec::new(),
        raw: Vec::new(),
        depth: vec![fb.z_clear; n],
        stencil: vec![fb.s_clear; n],
        touched: vec![false; (w.div_ceil(TILE) * h.div_ceil(TILE)) as usize],
    };
    for rt in &fb.rts {
        let clear = if internal_bits(rt.internal).is_some() { unpack_clear(rt.internal, rt.clear[0]) } else { [0.0; 4] };
        let raw = rt.clear[0] as u128 | (rt.clear[1] as u128) << 32 | (rt.clear[2] as u128) << 64 | (rt.clear[3] as u128) << 96;
        let mut color = vec![clear; n];
        let mut raws = vec![raw; n];
        if rt.preload {
            let bpp = wb_bytes(rt.wb_format);
            for y in y0..y1 {
                for x in x0..x1 {
                    let off = surface_offset(rt.block, x, y, rt.preload_stride, bpp);
                    let mut b = [0u8; 16];
                    vm.read_bytes(rt.preload_base + off, &mut b[..bpp as usize]);
                    let i = ((y - y0) * w + (x - x0)) as usize;
                    raws[i] = u128::from_le_bytes(b);
                    let mut m = decode_wb(rt.wb_format, &b);
                    if rt.srgb {
                        for c in m.iter_mut().take(3) {
                            *c = srgb_to_linear(*c);
                        }
                    }
                    color[i] = unswz(rt.swizzle, m, [0.0, 0.0, 0.0, 1.0]);
                }
            }
        }
        tb.color.push(color);
        tb.raw.push(raws);
    }
    if fb.zs.present && (fb.z_preload || fb.s_preload) {
        for y in y0..y1 {
            for x in x0..x1 {
                let i = ((y - y0) * w + (x - x0)) as usize;
                let (z, s) = read_zs(vm, &fb.zs, x, y);
                if fb.z_preload {
                    tb.depth[i] = z;
                }
                if fb.s_preload {
                    if let Some(s) = s {
                        tb.stencil[i] = s;
                    }
                }
            }
        }
    }
    let mut cycles = n as u64 / 4;
    for prim in list {
        let c = raster(vm, fb, &mut tb, prim);
        cycles += c;
        if !vm.spend(c) {
            return cycles;
        }
    }
    // write back
    let tiles_x = w.div_ceil(TILE);
    for (k, rt) in fb.rts.iter().enumerate() {
        if !rt.write || rt.base == 0 {
            continue;
        }
        let bpp = wb_bytes(rt.wb_format);
        let raw = internal_bits(rt.internal).is_none();
        for y in y0..y1 {
            for x in x0..x1 {
                let ti = (((y - y0) / TILE) * tiles_x + (x - x0) / TILE) as usize;
                if !tb.touched[ti] && !rt.clean_write {
                    continue;
                }
                let i = tb.idx(x, y);
                let mut b = [0u8; 16];
                if raw {
                    b = tb.raw[k][i].to_le_bytes();
                } else {
                    let mut v = swz(rt.swizzle, tb.color[k][i]);
                    if rt.srgb {
                        for c in v.iter_mut().take(3) {
                            *c = linear_to_srgb(*c);
                        }
                    }
                    if rt.dither {
                        if let Some(bits) = wb_low_bits(rt.wb_format) {
                            let d = bayer(x, y);
                            for c in 0..4 {
                                let m = ((1u32 << bits[c]) - 1) as f32;
                                v[c] = (v[c] + d / m).clamp(0.0, 1.0);
                            }
                        }
                    }
                    encode_wb(rt.wb_format, v, &mut b);
                }
                let off = surface_offset(rt.block, x, y, rt.row_stride, bpp);
                vm.write_bytes(rt.base + off, &b[..bpp as usize]);
            }
        }
    }
    if fb.zs.present && (fb.z_write || fb.s_write) {
        for y in y0..y1 {
            for x in x0..x1 {
                let ti = (((y - y0) / TILE) * tiles_x + (x - x0) / TILE) as usize;
                if !tb.touched[ti] {
                    continue;
                }
                let i = tb.idx(x, y);
                write_zs(vm, fb, x, y, tb.depth[i], tb.stencil[i]);
            }
        }
    }
    if let Ok(dir) = std::env::var("SBC_MALI_DUMP") {
        dump(&dir, &tb, fb);
    }
    cycles
}

/// Write render target 0 of the tile buffer as a PPM (`SBC_MALI_DUMP=dir`), numbered per job.
fn dump(dir: &str, tb: &TileBuf, fb: &Fb) {
    use std::sync::atomic::{AtomicU32, Ordering};
    static N: AtomicU32 = AtomicU32::new(0);
    let n = N.fetch_add(1, Ordering::Relaxed);
    let Some(c) = tb.color.first() else { return };
    let mut out = format!("P6 {} {} 255\n", tb.w, tb.h).into_bytes();
    for v in c {
        for ch in 0..3 {
            out.push((v[ch].clamp(0.0, 1.0) * 255.0).round() as u8);
        }
    }
    let _ = std::fs::write(format!("{dir}/frag-{n:04}-{}x{}.ppm", fb.width, fb.height), out);
}

fn zs_bytes(f: u32) -> u32 {
    match f {
        1 => 2,
        15 => 8,
        _ => 4,
    }
}

fn read_zs(vm: &mut Vm, zs: &Zs, x: u32, y: u32) -> (f32, Option<u8>) {
    let bpp = zs_bytes(zs.zs_format);
    let off = surface_offset(zs.zs_block, x, y, zs.zs_stride, bpp);
    let v = vm.r32(zs.zs_base + off);
    match zs.zs_format {
        1 => ((v & 0xffff) as f32 / 65535.0, None),
        4 | 2 => ((v & 0xff_ffff) as f32 / 16_777_215.0, None),
        5 => ((v & 0xff_ffff) as f32 / 16_777_215.0, Some((v >> 24) as u8)),
        6 => ((v >> 8) as f32 / 16_777_215.0, None),
        7 => ((v >> 8) as f32 / 16_777_215.0, Some(v as u8)),
        _ => (f32::from_bits(v), None),
    }
}

fn write_zs(vm: &mut Vm, fb: &Fb, x: u32, y: u32, z: f32, s: u8) {
    let zs = &fb.zs;
    let bpp = zs_bytes(zs.zs_format);
    let off = surface_offset(zs.zs_block, x, y, zs.zs_stride, bpp);
    let addr = zs.zs_base + off;
    let q24 = (z.clamp(0.0, 1.0) as f64 * 16_777_215.0).round() as u32;
    if zs.zs_base != 0 {
        let old = vm.r32(addr);
        let (z_on, s_on) = (fb.z_write, fb.s_write);
        match zs.zs_format {
            1 => {
                if z_on {
                    vm.w16(addr, (z.clamp(0.0, 1.0) * 65535.0).round() as u16);
                }
            }
            2 | 4 => {
                if z_on {
                    vm.w32(addr, q24);
                }
            }
            5 => {
                let mut v = old;
                if z_on {
                    v = (v & 0xff00_0000) | q24;
                }
                if s_on {
                    v = (v & 0x00ff_ffff) | (s as u32) << 24;
                }
                vm.w32(addr, v);
            }
            6 => {
                if z_on {
                    vm.w32(addr, q24 << 8);
                }
            }
            7 => {
                let mut v = old;
                if z_on {
                    v = (v & 0xff) | q24 << 8;
                }
                if s_on {
                    v = (v & !0xff) | s as u32;
                }
                vm.w32(addr, v);
            }
            _ => {
                if z_on {
                    vm.w32(addr, z.to_bits());
                }
            }
        }
    }
    if fb.s_write && zs.s_base != 0 && zs.s_base != zs.zs_base {
        let sb = match zs.s_format {
            1 => 1,
            2 | 5 => 2,
            _ => 4,
        };
        let off = surface_offset(zs.s_block, x, y, zs.s_stride, sb);
        let addr = zs.s_base + off;
        match zs.s_format {
            4 => vm.w8(addr + 3, s),
            5 => vm.w8(addr + 1, s),
            _ => vm.w8(addr, s),
        }
    }
}

/// Edge function for the top-left rule in a y-down frame.
fn edge(a: [f32; 2], b: [f32; 2], p: [f32; 2]) -> f32 {
    (b[0] - a[0]) * (p[1] - a[1]) - (b[1] - a[1]) * (p[0] - a[0])
}

fn raster(vm: &mut Vm, fb: &Fb, tb: &mut TileBuf, prim: &Prim) -> u64 {
    let ds: &DrawState = &prim.draw;
    let dcd = ds.dcd;
    let rsd = Rsd::read(vm, ds.rsd_va);
    let vp: [u32; 8] = vm.words(dcd.viewport);
    let vmin = [f32::from_bits(vp[0]), f32::from_bits(vp[1])];
    let vmax = [f32::from_bits(vp[2]), f32::from_bits(vp[3])];
    let zmin = f32::from_bits(vp[4]);
    let zmax = f32::from_bits(vp[5]);
    let sx0 = (vp[6] & 0xffff).max(tb.x0).max(vmin[0].max(0.0) as u32);
    let sy0 = (vp[6] >> 16).max(tb.y0).max(vmin[1].max(0.0) as u32);
    let sx1 = ((vp[7] & 0xffff) + 1).min(tb.x0 + tb.w).min(vmax[0].max(0.0).ceil() as u32);
    let sy1 = ((vp[7] >> 16) + 1).min(tb.y0 + tb.h).min(vmax[1].max(0.0).ceil() as u32);
    if sx1 <= sx0 || sy1 <= sy0 {
        return 1;
    }
    let pos = prim.pos;
    let v2 = |i: usize| [pos[i][0], pos[i][1]];
    // coverage setup
    let (bx0, by0, bx1, by1, front, area);
    match prim.kind {
        draw::TRIS => {
            let a = edge(v2(0), v2(1), v2(2));
            if a == 0.0 || !a.is_finite() {
                return 1;
            }
            // In framebuffer coordinates (y down) a counter-clockwise triangle has a negative
            // edge-function area.
            front = (a < 0.0) == dcd.front_ccw();
            if (front && dcd.cull_front()) || (!front && dcd.cull_back()) {
                return 1;
            }
            area = a;
            let xs = [pos[0][0], pos[1][0], pos[2][0]];
            let ys = [pos[0][1], pos[1][1], pos[2][1]];
            bx0 = xs.iter().cloned().fold(f32::MAX, f32::min);
            bx1 = xs.iter().cloned().fold(f32::MIN, f32::max);
            by0 = ys.iter().cloned().fold(f32::MAX, f32::min);
            by1 = ys.iter().cloned().fold(f32::MIN, f32::max);
        }
        draw::LINES => {
            front = true;
            area = 1.0;
            let r = ds.line_width.max(1.0) / 2.0 + 1.0;
            bx0 = pos[0][0].min(pos[1][0]) - r;
            bx1 = pos[0][0].max(pos[1][0]) + r;
            by0 = pos[0][1].min(pos[1][1]) - r;
            by1 = pos[0][1].max(pos[1][1]) + r;
        }
        _ => {
            front = true;
            area = 1.0;
            let r = ds.point_size / 2.0;
            bx0 = pos[0][0] - r;
            bx1 = pos[0][0] + r;
            by0 = pos[0][1] - r;
            by1 = pos[0][1] + r;
        }
    }
    if !(bx0.is_finite() && by0.is_finite() && bx1.is_finite() && by1.is_finite()) {
        return 1;
    }
    let px0 = (bx0.floor().max(sx0 as f32) as u32) & !1;
    let py0 = (by0.floor().max(sy0 as f32) as u32) & !1;
    let px1 = (bx1.ceil() as i64 + 1).clamp(0, sx1 as i64) as u32;
    let py1 = (by1.ceil() as i64 + 1).clamp(0, sy1 as i64) as u32;
    if px1 <= px0 || py1 <= py0 {
        return 1;
    }
    let props = rsd.props;
    let has_shader = rsd.pc() != 0;
    let discard = props & (1 << 12) != 0;
    let depth_src = (props >> 8) & 3;
    let depth_from_shader = depth_src == 3;
    let stencil_from_shader = props & (1 << 28) != 0;
    let depth_func = (rsd.ms_misc >> 24) & 7;
    let depth_write = rsd.ms_misc & (1 << 27) != 0;
    let stencil_on = rsd.stencil_misc & (1 << 16) != 0;
    let alpha_func = (rsd.stencil_misc >> 21) & 7;
    let st = if front { rsd.stencil_front } else { rsd.stencil_back };
    let st_wmask = if front { rsd.stencil_misc & 0xff } else { (rsd.stencil_misc >> 8) & 0xff } as u8;
    let bias_on = rsd.stencil_misc & (1 << if front { 28 } else { 29 }) != 0;
    // depth slope for polygon offset
    let mut zoffset = 0.0;
    if bias_on && prim.kind == draw::TRIS {
        let (x0, y0, z0) = (pos[0][0], pos[0][1], pos[0][2]);
        let (x1, y1, z1) = (pos[1][0], pos[1][1], pos[1][2]);
        let (x2, y2, z2) = (pos[2][0], pos[2][1], pos[2][2]);
        let det = (x1 - x0) * (y2 - y0) - (x2 - x0) * (y1 - y0);
        let dzdx = ((z1 - z0) * (y2 - y0) - (z2 - z0) * (y1 - y0)) / det;
        let dzdy = ((x1 - x0) * (z2 - z0) - (x2 - x0) * (z1 - z0)) / det;
        let r = if fb.z_internal == 0 { 1.0 / 65535.0 } else { 1.0 / 16_777_215.0 };
        let mut o = rsd.depth_factor * dzdx.abs().max(dzdy.abs()) + rsd.depth_units * r / 2.0;
        if rsd.depth_clamp > 0.0 {
            o = o.min(rsd.depth_clamp);
        } else if rsd.depth_clamp < 0.0 {
            o = o.max(rsd.depth_clamp);
        }
        zoffset = o;
    }
    let blend: Vec<[u32; 4]> = (0..fb.rts.len()).map(|k| vm.words(ds.rsd_va + 64 + 16 * k as u64)).collect();
    let tiles_x = tb.w.div_ceil(TILE);
    // The tile buffer is reached through `tbp` from here on (the shader environments hold
    // it too): every access reborrows it for that access only.
    let (bx0, by0, bx1, by1) = (tb.x0, tb.y0, tb.x0 + tb.w, tb.y0 + tb.h);
    let mut uniforms = Thread::new(rsd.pc());
    if has_shader {
        draw::preload_uniforms(vm, &dcd, &rsd, &mut uniforms);
    }
    let tbp: *mut TileBuf = tb;
    let mut env = FragEnv {
        vm,
        dcd,
        prim: prim.clone(),
        px: [Pixel::default(); 4],
        out: Default::default(),
        depth_from_shader,
        stencil_from_shader,
        tb: tbp,
    };
    let mut core = Core::new(&mut env);
    let mut occlusion = 0u64;
    let mut quads = 0u64;
    let mut qy = py0;
    while qy < py1 {
        let mut qx = px0;
        while qx < px1 {
            // coverage and barycentrics of the four pixels
            let mut cover = [false; 4];
            let mut pix = [Pixel::default(); 4];
            for k in 0..4 {
                let x = qx + (k as u32 & 1);
                let y = qy + (k as u32 >> 1);
                let c = [x as f32 + 0.5, y as f32 + 0.5];
                let inside_scissor = x >= sx0 && x < sx1 && y >= sy0 && y < sy1;
                let mut b = [1.0, 0.0, 0.0];
                let mut pt = [0.0f32; 2];
                let cov = match prim.kind {
                    draw::TRIS => {
                        let w0 = edge(v2(1), v2(2), c) / area;
                        let w1 = edge(v2(2), v2(0), c) / area;
                        let w2 = edge(v2(0), v2(1), c) / area;
                        b = [w0, w1, w2];
                        let tl = |e: f32, a: [f32; 2], bb: [f32; 2]| -> bool {
                            if e > 0.0 {
                                return true;
                            }
                            if e < 0.0 {
                                return false;
                            }
                            // top-left rule (y down): top edge or left edge
                            let (dx, dy) = (bb[0] - a[0], bb[1] - a[1]);
                            let (dx, dy) = if area > 0.0 { (dx, dy) } else { (-dx, -dy) };
                            (dy == 0.0 && dx < 0.0) || dy > 0.0
                        };
                        tl(w0, v2(1), v2(2)) && tl(w1, v2(2), v2(0)) && tl(w2, v2(0), v2(1))
                    }
                    draw::LINES => {
                        let (a, bb) = (v2(0), v2(1));
                        let d = [bb[0] - a[0], bb[1] - a[1]];
                        let len2 = d[0] * d[0] + d[1] * d[1];
                        if len2 == 0.0 {
                            false
                        } else {
                            let t = ((c[0] - a[0]) * d[0] + (c[1] - a[1]) * d[1]) / len2;
                            let dist = ((c[0] - a[0]) * d[1] - (c[1] - a[1]) * d[0]).abs() / len2.sqrt();
                            b = [1.0 - t, t, 0.0];
                            (0.0..1.0).contains(&t) && dist <= ds.line_width / 2.0
                        }
                    }
                    _ => {
                        let r = ds.point_size / 2.0;
                        let (dx, dy) = (c[0] - pos[0][0], c[1] - pos[0][1]);
                        pt = [dx / ds.point_size + 0.5, dy / ds.point_size + 0.5];
                        dx >= -r && dx < r && dy >= -r && dy < r
                    }
                };
                cover[k] = cov && inside_scissor && x < bx1 && y < by1;
                pix[k] = Pixel { x, y, b, front, point: pt };
            }
            if !cover.iter().any(|&c| c) {
                qx += 2;
                continue;
            }
            quads += 1;
            // depth of each pixel
            let mut z = [0f32; 4];
            for k in 0..4 {
                let b = pix[k].b;
                z[k] = match prim.kind {
                    draw::TRIS => b[0] * pos[0][2] + b[1] * pos[1][2] + b[2] * pos[2][2],
                    draw::LINES => b[0] * pos[0][2] + b[1] * pos[1][2],
                    _ => pos[0][2],
                } + zoffset;
                let near_discard = rsd.ms_misc & (1 << 28) != 0;
                let far_discard = rsd.ms_misc & (1 << 29) != 0;
                if (near_discard && z[k] < zmin) || (far_discard && z[k] > zmax) {
                    cover[k] = false;
                }
                z[k] = z[k].clamp(zmin.min(zmax), zmax.max(zmin));
            }
            for k in 0..4 {
                if cover[k] {
                    let ti = (((pix[k].y - by0) / TILE) * tiles_x + (pix[k].x - bx0) / TILE) as usize;
                    unsafe { (&mut *tbp).touched[ti] = true };
                }
            }
            // early depth/stencil when the shader cannot change the outcome
            let late = discard || depth_from_shader || stencil_from_shader || alpha_func != 7;
            let mut pass = cover;
            if !late {
                for k in 0..4 {
                    if pass[k] {
                        pass[k] = zs_test(fb, unsafe { &mut *tbp }, &pix[k], z[k], depth_func, depth_write, stencil_on, st, st_wmask, true);
                    }
                }
                if !pass.iter().any(|&p| p) {
                    qx += 2;
                    continue;
                }
            }
            for k in 0..4 {
                core.env.out[k].clear();
            }
            core.env.px = pix;
            if has_shader {
                let mut threads = [uniforms; 4];
                core.run(&mut threads);
                for k in 0..4 {
                    if threads[k].state == State::Discarded {
                        pass[k] = false;
                    }
                }
            } else {
                for k in 0..4 {
                    core.env.out[k].push(Output { rt: u32::MAX, color: 0, half: false, depth: None, stencil: None });
                }
            }
            for k in 0..4 {
                if !pass[k] {
                    continue;
                }
                let outs = std::mem::take(&mut core.env.out[k]);
                let mut zk = z[k];
                if let Some(d) = outs.iter().rev().find_map(|o| o.depth) {
                    zk = d.clamp(0.0, 1.0);
                }
                // alpha test on render target 0's alpha
                if alpha_func != 7 {
                    let a = outs.iter().find(|o| o.rt == 0).map(|o| color_of(o)[3]).unwrap_or(1.0);
                    if !func(alpha_func, a, rsd.alpha_ref) {
                        continue;
                    }
                }
                if late && !zs_test(fb, unsafe { &mut *tbp }, &pix[k], zk, depth_func, depth_write, stencil_on, st, st_wmask, true) {
                    continue;
                }
                occlusion += 1;
                let i = unsafe { (*tbp).idx(pix[k].x, pix[k].y) };
                for o in &outs {
                    if o.rt as usize >= fb.rts.len() {
                        continue;
                    }
                    let rt = &fb.rts[o.rt as usize];
                    let bd = blend[o.rt as usize];
                    let raw = internal_bits(rt.internal).is_none();
                    if bd[0] & 2 != 0 && bd[0] & (1 << 9) != 0 {
                        // blend shader: it reads the destination from the tile buffer and
                        // writes the final value out (no fixed-function blending after)
                        let pc = (bd[2] as u64 | (bd[3] as u64) << 32) & !0xf;
                        let (x, y) = (pix[k].x, pix[k].y);
                        // Its write-out is the raw tile-buffer value: packed in the internal
                        // format as clear colours are (Mesa's pan_lower_framebuffer).
                        if let Some((c, _)) = run_blend(core.env.vm, &dcd, tbp, x, y, o.rt as usize, pc, o.color, o.half) {
                            let tbm = unsafe { &mut *tbp };
                            if raw {
                                tbm.raw[o.rt as usize][i] = c;
                            } else {
                                tbm.color[o.rt as usize][i] = unpack_clear(rt.internal, c as u32);
                            }
                        }
                        continue;
                    }
                    let tbm = unsafe { &mut *tbp };
                    if raw {
                        tbm.raw[o.rt as usize][i] = o.color;
                        continue;
                    }
                    let src = color_of(o);
                    let dst = tbm.color[o.rt as usize][i];
                    let enable = bd[0] & (1 << 9) != 0;
                    if !enable {
                        continue;
                    }
                    let (eq, k) = (bd[2], f32::from_bits(bd[3]));
                    let mut src = src;
                    if bd[0] & (1 << 8) != 0 {
                        src[3] = 1.0;
                    }
                    let mut c = dst;
                    for ch in 0..4 {
                        if (eq >> (28 + ch)) & 1 == 0 {
                            continue;
                        }
                        let f = if ch == 3 { (eq >> 12) & 0xfff } else { eq & 0xfff };
                        let s = src[ch].clamp(0.0, 1.0);
                        c[ch] = blend_channel(f, s, dst[ch], src[3].clamp(0.0, 1.0), dst[3], k);
                    }
                    tbm.color[o.rt as usize][i] = quantize(rt.internal, c);
                }
            }
            qx += 2;
        }
        qy += 2;
    }
    let cycles = core.cycles + quads;
    drop(core);
    if dcd.occlusion != 0 && occlusion > 0 {
        let mode = (dcd.flags >> 3) & 3;
        if mode == 3 {
            let old = env.vm.r64(dcd.occlusion);
            env.vm.w64(dcd.occlusion, old + occlusion);
        } else if mode == 1 {
            env.vm.w64(dcd.occlusion, 1);
        }
    }
    cycles
}

/// A blend shader's environment: the destination pixel in the tile buffer.
struct BlendEnv<'a, 'b> {
    vm: &'a mut Vm<'b>,
    dcd: Dcd,
    tb: *mut TileBuf,
    x: u32,
    y: u32,
    rt: usize,
    out: Option<(u128, bool)>,
}

impl Env for BlendEnv<'_, '_> {
    fn spend(&mut self, n: u64) -> bool {
        self.vm.spend(n)
    }

    fn code(&mut self, va: u64) -> [u8; 64] {
        let mut b = [0u8; 64];
        self.vm.read_bytes(va, &mut b);
        b
    }

    fn ldst(&mut self, _t: usize, ins: &LdSt, a: LdStArgs, data: u128) -> u128 {
        match ins.op {
            0xb8..=0xba | 0x9c..=0x9f => {
                let tb = unsafe { &*self.tb };
                let i = tb.idx(self.x, self.y);
                if ins.op == 0xba || ins.op == 0x9e || ins.op == 0x9f {
                    return tb.raw.get(self.rt).map_or(0, |r| r[i]);
                }
                let v = tb.color.get(self.rt).map_or([0.0; 4], |c| c[i]).map(f32::to_bits);
                if ins.op == 0xb9 || ins.op == 0x9d { draw::to_half_lanes(v) } else { draw::to_u128(v) }
            }
            _ => draw::common_ldst(self.vm, &self.dcd, ins, a, data, 0).unwrap_or(0),
        }
    }

    fn texture(&mut self, _w: &TexWord, _c: &[[f32; 4]; 4], _l: &[f32; 4], _a: u8) -> [[u32; 4]; 4] {
        [[0; 4]; 4]
    }

    fn writeout(&mut self, _t: usize, r0: u128, half: bool, _r1: u128, _w: u8) {
        if self.out.is_none() {
            self.out = Some((r0, half));
        }
    }
}

/// Run a blend shader for one pixel: the source colour in r0, the shader's write-out is
/// the value to store.
#[allow(clippy::too_many_arguments)]
fn run_blend(vm: &mut Vm, dcd: &Dcd, tb: *mut TileBuf, x: u32, y: u32, rt: usize, pc: u64, src: u128, half: bool) -> Option<(u128, bool)> {
    let mut env = BlendEnv { vm, dcd: *dcd, tb, x, y, rt, out: None };
    let mut t = Thread::new(pc);
    t.r[0] = src;
    t.r0_half = half;
    let mut core = Core::new(&mut env);
    core.run(std::slice::from_mut(&mut t));
    env.out
}

fn color_of(o: &Output) -> [f32; 4] {
    let mut c = [0f32; 4];
    for ch in 0..4 {
        c[ch] = if o.half {
            f16_to_f32((o.color >> (16 * ch)) as u16)
        } else {
            f32::from_bits((o.color >> (32 * ch)) as u32)
        };
    }
    c
}

#[allow(clippy::too_many_arguments)]
fn zs_test(fb: &Fb, tb: &mut TileBuf, p: &Pixel, z: f32, depth_func: u32, depth_write: bool, stencil_on: bool, st: u32, wmask: u8, _commit: bool) -> bool {
    let i = tb.idx(p.x, p.y);
    let _ = fb;
    let dz = tb.depth[i];
    let depth_ok = func(depth_func, z, dz);
    if stencil_on {
        let s = tb.stencil[i];
        let r = (st & 0xff) as u8;
        let m = ((st >> 8) & 0xff) as u8;
        let f = (st >> 16) & 7;
        let sok = func(f, (r & m) as f32, (s & m) as f32);
        let op = if !sok {
            (st >> 19) & 7
        } else if !depth_ok {
            (st >> 22) & 7
        } else {
            (st >> 25) & 7
        };
        let ns = stencil_op(op, s, r);
        tb.stencil[i] = (s & !wmask) | (ns & wmask);
        if !sok {
            return false;
        }
    }
    if !depth_ok {
        return false;
    }
    if depth_write {
        tb.depth[i] = z;
    }
    true
}

// fp16 helpers kept for blend-shader colours
#[allow(dead_code)]
fn pack_half(c: [f32; 4]) -> u128 {
    let mut o = 0u128;
    for ch in 0..4 {
        o |= (f32_to_f16(c[ch]) as u128) << (16 * ch);
    }
    o
}

#[allow(dead_code)]
fn _unused(_: PixelFormat) {}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mmu::AddressSpace;

    /// A fragment job over the largest framebuffer the descriptor can name (65536 x 65536,
    /// tiles 0..0xfff) with a preloaded render target: rendered in chunks, so it neither
    /// overflows the tile-buffer size nor allocates the whole area, and the work budget
    /// ends it.
    #[test]
    fn huge_framebuffer_is_chunked_and_bounded() {
        let mut ram = vec![0u8; 0x4000];
        let put = |ram: &mut [u8], a: usize, v: u32| ram[a..a + 4].copy_from_slice(&v.to_le_bytes());
        let (job, fbd) = (0x1000, 0x2000);
        put(&mut ram, job + 36, 0x0fff_0fff); // tile max
        put(&mut ram, job + 40, fbd as u32);
        put(&mut ram, fbd + 32, 0xffff_ffff); // width, height - 1
        put(&mut ram, fbd + 128 + 4, 1); // RT0 written back (to address 0: skipped)
        put(&mut ram, fbd + 128 + 8, 1); // and preloaded
        let aspace = AddressSpace { active_transtab: 2, ..Default::default() };
        let mut vm = Vm::new(&mut ram, 0, aspace);
        vm.set_budget(3 * (CHUNK as u64 * CHUNK as u64));
        fragment_job(&mut vm, job as u64, &[], false);
        assert!(vm.exhausted);
    }
}
