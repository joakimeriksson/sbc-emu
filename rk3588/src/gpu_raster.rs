//! Rasterization of a frame's draws into the tile buffer: clipping, culling, depth and
//! stencil, fragment shaders in 2x2 quads, blending and occlusion queries.
//!
//! Positions are window x, y, z and 1/w (Mesa lowers the viewport transform into the
//! vertex shader and clamps 1/w to +-2^15, so w is never zero). Triangles crossing w = 0
//! are clipped in homogeneous space; the near and far planes are applied per fragment
//! through the descriptor's depth range, as on Mali. Varyings are interpolated with
//! perspective-correct weights of the original primitive's vertices. Coordinates snap to
//! 1/256 pixel so shared edges are covered exactly once (top-left rule).
use crate::{
    gpu_cs::Stop,
    gpu_mmu::AddressSpace,
    gpu_render::{Fb, TileBuffer},
    gpu_resources::Resources,
    gpu_shader::{run_quad, Code, Env, Fragment, Interpolation, RegisterAllocation, Shader, Tlb},
    gpu_tiler::{Draw, Frame, LINES, POINTS},
};
fn word(b: &[u8], n: usize) -> u32 {
    u32::from_le_bytes(b[n * 4..n * 4 + 4].try_into().unwrap())
}
fn reg(r: &[u32; 256], n: usize) -> u64 {
    r[n] as u64 | ((r[n + 1] as u64) << 32)
}
fn bad() -> Stop {
    Stop::Unsupported { pc: 0, opcode: 7 }
}

/// Shader instructions per fragment thread and per frame.
const THREAD_BUDGET: usize = 1 << 16;
const FRAME_BUDGET: u64 = 1 << 34;

/// Depth/stencil descriptor state.
#[derive(Clone, Copy, Default)]
struct Zs {
    depth_func: u32,
    depth_write: bool,
    depth_from_shader: bool,
    bias: Option<(f32, f32, f32)>,
    stencil: Option<[Stencil; 2]>,
}
#[derive(Clone, Copy, Default)]
struct Stencil {
    func: u32,
    fail: u32,
    zfail: u32,
    zpass: u32,
    write_mask: u8,
    value_mask: u8,
    reference: u8,
}
fn test(func: u32, a: f32, b: f32) -> bool {
    match func {
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
fn stencil_op(op: u32, s: u8, reference: u8) -> u8 {
    match op {
        1 => reference,
        2 => 0,
        3 => !s,
        4 => s.wrapping_add(1),
        5 => s.wrapping_sub(1),
        6 => s.saturating_add(1),
        7 => s.saturating_sub(1),
        _ => s,
    }
}

/// Per-draw fixed-function and shader state.
struct State {
    front_ccw: bool,
    cull: [bool; 2],
    scissor: [u32; 4],
    z_range: [f32; 2],
    z_cull: [bool; 2],
    zs: Option<Zs>,
    blends: Vec<(usize, crate::gpu_blend::Blend)>,
    blend_words: [[u32; 2]; 8],
    rt_mask: u32,
    may_discard: bool,
    shader: Option<(u64, RegisterAllocation, u32, Vec<u32>, Resources, bool)>,
    occlusion: Option<(u64, u32)>,
    point_size: f32,
    tls: u64,
}

/// The draw state inputs: CSF registers of RUN_IDVS, or a frame-shader Draw descriptor.
struct Inputs {
    f0: u32,
    f1: u32,
    zs: u64,
    blend: u64,
    spd: u64,
    fau: u64,
    srt: u64,
    tls: u64,
    scissor: [u32; 4],
    z_range: [f32; 2],
    z_cull: [bool; 2],
    occlusion: u64,
    point_size: f32,
}
impl Inputs {
    fn from_draw(draw: &Draw) -> Self {
        let r = &draw.regs;
        Inputs {
            f0: r[57],
            f1: r[58],
            zs: reg(r, 52),
            blend: reg(r, 50),
            spd: reg(r, 20),
            fau: reg(r, 12),
            srt: reg(
                r,
                if draw.instruction & (1 << 38) != 0 {
                    4
                } else {
                    0
                },
            ),
            tls: reg(r, 24),
            scissor: [r[42] & 0xffff, r[42] >> 16, r[43] & 0xffff, r[43] >> 16],
            z_range: [f32::from_bits(r[44]), f32::from_bits(r[45])],
            z_cull: [r[56] & (1 << 16) != 0, r[56] & (1 << 17) != 0],
            occlusion: reg(r, 46),
            point_size: f32::from_bits(r[60]),
        }
    }
    /// A Draw descriptor (v10.xml "Draw"): flags, depth/stencil, blend, occlusion and
    /// the shader environment at word 16.
    fn from_dcd(d: &[u32; 32], tb: &TileBuffer) -> Self {
        let pair = |n: usize| d[n] as u64 | (d[n + 1] as u64) << 32;
        Inputs {
            f0: d[0],
            f1: d[1],
            zs: pair(10),
            blend: pair(12),
            spd: pair(26),
            fau: (pair(30) & 0xffff_ffff_ffff) | ((d[17] as u64 & 255) << 56),
            srt: pair(24),
            tls: pair(28),
            scissor: [tb.x0, tb.y0, tb.x0 + tb.w - 1, tb.y0 + tb.h - 1],
            z_range: [f32::from_bits(d[6]), f32::from_bits(d[7])],
            z_cull: [false; 2],
            occlusion: pair(14),
            point_size: 1.,
        }
    }
}

impl State {
    fn decode(m: &AddressSpace, ram: &[u8], r: &Inputs) -> Result<State, Stop> {
        let f0 = r.f0;
        let f1 = r.f1;
        // Per-sample shading and alpha-to-coverage are not modelled.
        if f0 & ((1 << 11) | (1 << 22)) != 0 {
            return Err(bad());
        }
        let mut zs = None;
        if r.zs != 0 {
            let mut d = [0u8; 32];
            m.read(ram, r.zs, &mut d, true).map_err(Stop::Memory)?;
            if word(&d, 0) & 15 != 7 || word(&d, 0) & (1 << 30) != 0 {
                return Err(bad()); // stencil from shader
            }
            let w4 = word(&d, 4);
            let stencil = (word(&d, 0) & (1 << 31) != 0).then(|| {
                let side = |shift: u32, n: u32| Stencil {
                    func: (word(&d, 0) >> shift) & 7,
                    fail: (word(&d, 0) >> (shift + 3)) & 7,
                    zfail: (word(&d, 0) >> (shift + 6)) & 7,
                    zpass: (word(&d, 0) >> (shift + 9)) & 7,
                    write_mask: (word(&d, 1) >> (8 * n)) as u8,
                    value_mask: (word(&d, 1) >> (16 + 8 * n)) as u8,
                    reference: (word(&d, 2) >> (8 * n)) as u8,
                };
                [side(4, 0), side(16, 1)]
            });
            zs = Some(Zs {
                depth_func: w4 >> 29,
                depth_write: w4 & (1 << 27) != 0,
                depth_from_shader: (w4 >> 25) & 3 == 3,
                bias: (w4 & (1 << 28) != 0).then(|| {
                    (
                        f32::from_bits(word(&d, 5)),
                        f32::from_bits(word(&d, 6)),
                        f32::from_bits(word(&d, 7)),
                    )
                }),
                stencil,
            });
        }
        let blend_ptr = r.blend & !15;
        let blend_count = (r.blend & 15) as usize;
        if blend_count > 8 {
            return Err(bad());
        }
        let mut blends = Vec::new();
        let mut blend_words = [[0u32; 2]; 8];
        for k in 0..blend_count {
            let mut b = [0u8; 16];
            m.read(ram, blend_ptr + 16 * k as u64, &mut b, true)
                .map_err(Stop::Memory)?;
            if std::env::var_os("SBC_MALI_TRACE").is_some() {
                eprintln!(
                    "[mali] blend[{k}]={:x?} DCD={:x} {:x}",
                    (0..4).map(|n| word(&b, n)).collect::<Vec<_>>(),
                    f0,
                    f1
                );
            }
            blend_words[k] = [word(&b, 2), word(&b, 3)];
            let blend = crate::gpu_blend::Blend::decode(std::array::from_fn(|n| word(&b, n)))
                .map_err(|_| bad())?;
            blends.push((k, blend));
        }
        let spd = r.spd;
        let shader = if spd != 0 && f1 & (0xff << 16) != 0
            || spd != 0 && zs.is_some_and(|z| z.depth_from_shader)
            || spd != 0 && f0 & (1 << 20) != 0
        {
            let (binary, allocation, preload) = crate::gpu_tiler::shader_program(m, ram, spd, 2)?;
            let mut d = [0u8; 4];
            m.read(ram, spd, &mut d, true).map_err(Stop::Memory)?;
            let helpers = word(&d, 0) & (1 << 28) != 0;
            let uniforms = crate::gpu_tiler::uniforms(m, ram, r.fau)?;
            Some((
                binary,
                allocation,
                preload,
                uniforms,
                Resources(r.srt),
                helpers,
            ))
        } else {
            None
        };
        let occlusion = match (f0 >> 14) & 3 {
            0 => None,
            mode => Some((r.occlusion, mode)),
        };
        Ok(State {
            front_ccw: f0 & (1 << 16) != 0,
            cull: [f0 & (1 << 17) != 0, f0 & (1 << 18) != 0],
            scissor: r.scissor,
            z_range: r.z_range,
            z_cull: r.z_cull,
            zs,
            blends,
            blend_words,
            rt_mask: (f1 >> 16) & 0xff,
            may_discard: f0 & (1 << 20) != 0,
            shader,
            occlusion,
            point_size: r.point_size,
            tls: r.tls,
        })
    }
}

/// A primitive ready to rasterize: window positions (x, y, z, 1/w), snapped, and each
/// vertex's barycentric weights over the original primitive.
#[derive(Clone, Copy)]
struct Tri {
    p: [[f64; 4]; 3],
    orig: [[f64; 3]; 3],
}

fn snap(v: f64) -> f64 {
    (v * 256.).round() / 256.
}

/// Clip a triangle against w > 0 in homogeneous space. Returns a fan of triangles.
fn clip(pos: [[f32; 4]; 3]) -> Vec<Tri> {
    let finite = pos.iter().all(|p| p.iter().all(|v| v.is_finite()));
    if !finite {
        return Vec::new();
    }
    let ident = [[1., 0., 0.], [0., 1., 0.], [0., 0., 1.]];
    let make = |p: [[f64; 4]; 3], orig: [[f64; 3]; 3]| Tri {
        p: p.map(|v| [snap(v[0]), snap(v[1]), v[2], v[3]]),
        orig,
    };
    if pos.iter().all(|p| p[3] > 0.) {
        return vec![make(pos.map(|p| p.map(|v| v as f64)), ident)];
    }
    // Homogeneous coordinates: (x w, y w, z w, w) with w = 1/rw.
    const EPS: f64 = 1e-5;
    let h: Vec<([f64; 4], [f64; 3])> = (0..3)
        .map(|k| {
            let w = 1. / pos[k][3] as f64;
            (
                [
                    pos[k][0] as f64 * w,
                    pos[k][1] as f64 * w,
                    pos[k][2] as f64 * w,
                    w,
                ],
                ident[k],
            )
        })
        .collect();
    let mut out: Vec<([f64; 4], [f64; 3])> = Vec::new();
    for k in 0..3 {
        let (a, ba) = h[k];
        let (b, bb) = h[(k + 1) % 3];
        let (ina, inb) = (a[3] > EPS, b[3] > EPS);
        if ina {
            out.push((a, ba));
        }
        if ina != inb {
            let t = (EPS - a[3]) / (b[3] - a[3]);
            let lerp = |x: f64, y: f64| x + (y - x) * t;
            out.push((
                std::array::from_fn(|n| lerp(a[n], b[n])),
                std::array::from_fn(|n| lerp(ba[n], bb[n])),
            ));
        }
    }
    if out.len() < 3 {
        return Vec::new();
    }
    let window = |v: [f64; 4]| [v[0] / v[3], v[1] / v[3], v[2] / v[3], 1. / v[3]];
    let mut tris = Vec::new();
    for k in 1..out.len() - 1 {
        let vs = [out[0], out[k], out[k + 1]];
        tris.push(make(vs.map(|v| window(v.0)), vs.map(|v| v.1)));
    }
    tris
}

fn edge(a: [f64; 4], b: [f64; 4], x: f64, y: f64) -> f64 {
    (b[0] - a[0]) * (y - a[1]) - (b[1] - a[1]) * (x - a[0])
}
/// Top-left rule for a counter-clockwise-normalized (positive area) triangle.
fn owns(a: [f64; 4], b: [f64; 4]) -> bool {
    b[1] < a[1] || b[1] == a[1] && b[0] < a[0]
}

/// One covered (or helper) pixel of a quad.
#[derive(Clone, Copy, Default)]
struct Pixel {
    covered: bool,
    passed: bool,
    weights: [f64; 3],
    z: f32,
    w: f32,
    point: [f32; 2],
}

struct Raster<'a, 'b> {
    env: Env<'a>,
    fb: &'b Fb,
    tb: &'b mut TileBuffer,
    budget: u64,
    occlusion: u64,
}

pub fn draw(
    m: &AddressSpace,
    ram: &mut [u8],
    fb: &Fb,
    tb: &mut TileBuffer,
    frame: &Frame,
) -> Result<(), Stop> {
    let mut code = Code::default();
    let mut tlb = Tlb::default();
    let mut textures = crate::gpu_texture::Cache::default();
    for draw in &frame.draws {
        let st = State::decode(m, ram, &Inputs::from_draw(draw))?;
        let uniforms: &[u32] = st.shader.as_ref().map_or(&[], |s| &s.3);
        let env = Env::new(m, ram, uniforms, &mut code, &mut tlb, &mut textures);
        let mut r = Raster {
            env,
            fb,
            tb,
            budget: FRAME_BUDGET,
            occlusion: 0,
        };
        r.draw(draw, &st)?;
        let occlusion = r.occlusion;
        if let Some((address, mode)) = st.occlusion {
            let mut b = [0u8; 8];
            m.read(ram, address, &mut b, true).map_err(Stop::Memory)?;
            let v = u64::from_le_bytes(b);
            let v = if mode == 3 {
                v.wrapping_add(occlusion)
            } else {
                (v != 0 || occlusion != 0) as u64
            };
            m.write(ram, address, &v.to_le_bytes(), true)
                .map_err(Stop::Memory)?;
        }
    }
    Ok(())
}

/// Run a pre- or post-frame shader (Mesa's preload/resolve): its Draw descriptor's
/// fragment shader over every pixel of the tile buffer.
pub fn frame_shader(
    m: &AddressSpace,
    ram: &mut [u8],
    fb: &Fb,
    tb: &mut TileBuffer,
    dcd: u64,
) -> Result<(), Stop> {
    let mut b = [0u8; 128];
    m.read(ram, dcd, &mut b, true).map_err(Stop::Memory)?;
    let d: [u32; 32] = std::array::from_fn(|n| word(&b, n));
    if std::env::var_os("SBC_MALI_TRACE").is_some() {
        eprintln!("[mali] frame shader DCD {dcd:x}={d:x?}");
    }
    let st = State::decode(m, ram, &Inputs::from_dcd(&d, tb))?;
    let mut code = Code::default();
    let mut tlb = Tlb::default();
    let mut textures = crate::gpu_texture::Cache::default();
    let uniforms: &[u32] = st.shader.as_ref().map_or(&[], |s| &s.3);
    let env = Env::new(m, ram, uniforms, &mut code, &mut tlb, &mut textures);
    let bounds = [tb.x0, tb.y0, tb.x0 + tb.w - 1, tb.y0 + tb.h - 1];
    let mut r = Raster {
        env,
        fb,
        tb,
        budget: FRAME_BUDGET,
        occlusion: 0,
    };
    let pixel = |_x: u32, _y: u32| Pixel {
        covered: true,
        passed: true,
        weights: [1., 0., 0.],
        z: 0.,
        w: 1.,
        point: [0.; 2],
    };
    r.quads(&st, &None, [0; 3], 0, true, bounds, pixel)
}

impl Raster<'_, '_> {
    fn draw(&mut self, draw: &Draw, st: &State) -> Result<(), Stop> {
        let varyings = draw.varyings.clone();
        for prim in &draw.primitives {
            let v = [prim[0] as usize, prim[1] as usize, prim[2] as usize];
            if v.iter().any(|&k| k >= draw.positions.len()) {
                return Err(Stop::InvalidStream);
            }
            let provoking = prim[3] as usize;
            let pos = v.map(|k| draw.positions[k]);
            match draw.kind {
                POINTS => {
                    let size = draw
                        .point_sizes
                        .get(v[0])
                        .copied()
                        .unwrap_or(st.point_size)
                        .max(1.);
                    if pos[0][3] <= 0. || !pos[0].iter().all(|x| x.is_finite()) {
                        continue;
                    }
                    let p = pos[0].map(|x| x as f64);
                    let r = size as f64 / 2.;
                    let corners = [[p[0] - r, p[1] - r], [p[0] + r, p[1] + r]];
                    self.rect(st, &varyings, v, provoking, p, corners, None, true)?;
                }
                LINES => {
                    if pos[0][3] <= 0. || pos[1][3] <= 0. {
                        continue;
                    }
                    let a = pos[0].map(|x| x as f64);
                    let b = pos[1].map(|x| x as f64);
                    let width = st.point_size.max(1.) as f64;
                    let lo = [a[0].min(b[0]) - width, a[1].min(b[1]) - width];
                    let hi = [a[0].max(b[0]) + width, a[1].max(b[1]) + width];
                    self.rect(
                        st,
                        &varyings,
                        v,
                        provoking,
                        a,
                        [lo, hi],
                        Some((a, b, width)),
                        true,
                    )?;
                }
                _ => {
                    for t in clip(pos) {
                        self.triangle(st, &varyings, v, provoking, &t)?;
                    }
                }
            }
        }
        Ok(())
    }

    /// The pixel box a primitive may cover: bounds of the tile buffer, scissor and box.
    fn bounds(&self, st: &State, lo: [f64; 2], hi: [f64; 2]) -> Option<[u32; 4]> {
        let tb = &self.tb;
        let x0 = lo[0].floor().max(tb.x0 as f64).max(st.scissor[0] as f64);
        let y0 = lo[1].floor().max(tb.y0 as f64).max(st.scissor[1] as f64);
        let x1 = hi[0]
            .ceil()
            .min((tb.x0 + tb.w - 1) as f64)
            .min(st.scissor[2] as f64);
        let y1 = hi[1]
            .ceil()
            .min((tb.y0 + tb.h - 1) as f64)
            .min(st.scissor[3] as f64);
        if !(x0 <= x1 && y0 <= y1) {
            return None;
        }
        Some([x0 as u32, y0 as u32, x1 as u32, y1 as u32])
    }

    #[allow(clippy::too_many_arguments)]
    fn triangle(
        &mut self,
        st: &State,
        varyings: &Option<(usize, std::sync::Arc<[u8]>)>,
        vertices: [usize; 3],
        provoking: usize,
        t: &Tri,
    ) -> Result<(), Stop> {
        let mut p = t.p;
        let mut orig = t.orig;
        let area = edge(p[0], p[1], p[2][0], p[2][1]);
        if area == 0. || !area.is_finite() {
            return Ok(());
        }
        // In window coordinates (y down) a counter-clockwise triangle has negative area.
        let front = (area < 0.) == st.front_ccw;
        if st.cull[if front { 0 } else { 1 }] {
            return Ok(());
        }
        if area < 0. {
            p.swap(1, 2);
            orig.swap(1, 2);
        }
        let area = area.abs();
        let lo = [
            p.iter().map(|v| v[0]).fold(f64::MAX, f64::min),
            p.iter().map(|v| v[1]).fold(f64::MAX, f64::min),
        ];
        let hi = [
            p.iter().map(|v| v[0]).fold(f64::MIN, f64::max),
            p.iter().map(|v| v[1]).fold(f64::MIN, f64::max),
        ];
        let Some([x0, y0, x1, y1]) = self.bounds(st, lo, hi) else {
            return Ok(());
        };
        // Depth slope for polygon offset.
        let mut zoffset = 0f32;
        if let Some(Some((units, factor, clamp))) = st.zs.map(|z| z.bias) {
            let (a, b, c) = (p[0], p[1], p[2]);
            let det = (b[0] - a[0]) * (c[1] - a[1]) - (c[0] - a[0]) * (b[1] - a[1]);
            let dzdx = ((b[2] - a[2]) * (c[1] - a[1]) - (c[2] - a[2]) * (b[1] - a[1])) / det;
            let dzdy = ((b[0] - a[0]) * (c[2] - a[2]) - (c[0] - a[0]) * (b[2] - a[2])) / det;
            let r = 1. / ((1u64 << self.fb.z_bits.min(24)) as f64);
            let mut o = factor as f64 * dzdx.abs().max(dzdy.abs()) + units as f64 * r / 2.;
            if clamp > 0. {
                o = o.min(clamp as f64);
            } else if clamp < 0. {
                o = o.max(clamp as f64);
            }
            zoffset = o as f32;
        }
        let pixel = |x: u32, y: u32| -> Pixel {
            let (cx, cy) = (x as f64 + 0.5, y as f64 + 0.5);
            let e = [
                edge(p[1], p[2], cx, cy),
                edge(p[2], p[0], cx, cy),
                edge(p[0], p[1], cx, cy),
            ];
            let owners = [owns(p[1], p[2]), owns(p[2], p[0]), owns(p[0], p[1])];
            let covered = (0..3).all(|k| e[k] > 0. || e[k] == 0. && owners[k]);
            let b = e.map(|v| v / area);
            // Perspective-correct weights from 1/w.
            let denom: f64 = (0..3).map(|k| b[k] * p[k][3]).sum();
            let lam: [f64; 3] = std::array::from_fn(|k| {
                if denom != 0. {
                    b[k] * p[k][3] / denom
                } else {
                    b[k]
                }
            });
            let weights = std::array::from_fn(|n| (0..3).map(|k| lam[k] * orig[k][n]).sum());
            let z = (0..3).map(|k| b[k] * p[k][2]).sum::<f64>() as f32 + zoffset;
            Pixel {
                covered,
                passed: covered,
                weights,
                z,
                w: if denom != 0. { (1. / denom) as f32 } else { 1. },
                point: [0.; 2],
            }
        };
        self.quads(
            st,
            varyings,
            vertices,
            provoking,
            front,
            [x0, y0, x1, y1],
            pixel,
        )
    }

    /// Points (square sprites) and lines (rectangles around the segment).
    #[allow(clippy::too_many_arguments)]
    fn rect(
        &mut self,
        st: &State,
        varyings: &Option<(usize, std::sync::Arc<[u8]>)>,
        vertices: [usize; 3],
        provoking: usize,
        center: [f64; 4],
        corners: [[f64; 2]; 2],
        line: Option<([f64; 4], [f64; 4], f64)>,
        front: bool,
    ) -> Result<(), Stop> {
        let Some(b) = self.bounds(st, corners[0], corners[1]) else {
            return Ok(());
        };
        let pixel = |x: u32, y: u32| -> Pixel {
            let (cx, cy) = (x as f64 + 0.5, y as f64 + 0.5);
            match line {
                None => {
                    let size = corners[1][0] - corners[0][0];
                    let inside = cx >= corners[0][0]
                        && cx < corners[1][0]
                        && cy >= corners[0][1]
                        && cy < corners[1][1];
                    Pixel {
                        covered: inside,
                        passed: inside,
                        weights: [1., 0., 0.],
                        z: center[2] as f32,
                        w: (1. / center[3]) as f32,
                        point: [
                            ((cx - corners[0][0]) / size) as f32,
                            ((cy - corners[0][1]) / size) as f32,
                        ],
                    }
                }
                Some((a, b, width)) => {
                    let d = [b[0] - a[0], b[1] - a[1]];
                    let len2 = d[0] * d[0] + d[1] * d[1];
                    let t = if len2 > 0. {
                        ((cx - a[0]) * d[0] + (cy - a[1]) * d[1]) / len2
                    } else {
                        0.
                    };
                    let dist = if len2 > 0. {
                        ((cx - a[0]) * d[1] - (cy - a[1]) * d[0]).abs() / len2.sqrt()
                    } else {
                        f64::MAX
                    };
                    let inside = (0. ..1.).contains(&t) && dist <= width / 2.;
                    Pixel {
                        covered: inside,
                        passed: inside,
                        weights: [1. - t, t, 0.],
                        z: (a[2] + (b[2] - a[2]) * t) as f32,
                        w: 1.,
                        point: [0.; 2],
                    }
                }
            }
        };
        self.quads(st, varyings, vertices, provoking, front, b, pixel)
    }

    /// Visit the 2x2 quads of a box: coverage, early depth/stencil, the fragment shader
    /// and output merging.
    #[allow(clippy::too_many_arguments)]
    fn quads(
        &mut self,
        st: &State,
        varyings: &Option<(usize, std::sync::Arc<[u8]>)>,
        vertices: [usize; 3],
        provoking: usize,
        front: bool,
        [x0, y0, x1, y1]: [u32; 4],
        pixel: impl Fn(u32, u32) -> Pixel,
    ) -> Result<(), Stop> {
        let late = st.may_discard || st.zs.is_some_and(|z| z.depth_from_shader);
        let mut qy = y0 & !1;
        while qy <= y1 {
            let mut qx = x0 & !1;
            while qx <= x1 {
                let mut px = [Pixel::default(); 4];
                let mut any = false;
                for (k, p) in px.iter_mut().enumerate() {
                    let (x, y) = (qx + (k as u32 & 1), qy + (k as u32 >> 1));
                    *p = pixel(x, y);
                    let inside = x >= x0 && x <= x1 && y >= y0 && y <= y1;
                    p.covered &= inside;
                    p.passed = p.covered;
                    // Depth range: cull or clamp outside [min, max].
                    if p.covered {
                        let [zmin, zmax] = st.z_range;
                        if p.z < zmin {
                            if st.z_cull[0] {
                                p.passed = false;
                            }
                            p.z = zmin;
                        } else if p.z > zmax {
                            if st.z_cull[1] {
                                p.passed = false;
                            }
                            p.z = zmax;
                        }
                    }
                    if p.passed && !late {
                        p.passed = self.depth_stencil(st, x, y, p.z, front, true);
                    }
                    any |= p.passed;
                }
                if any {
                    self.shade(
                        st, varyings, vertices, provoking, front, qx, qy, &mut px, late,
                    )?;
                }
                qx += 2;
            }
            qy += 2;
        }
        Ok(())
    }

    /// Depth and stencil test (and update when `write`). Returns whether the pixel passes.
    fn depth_stencil(
        &mut self,
        st: &State,
        x: u32,
        y: u32,
        z: f32,
        front: bool,
        write: bool,
    ) -> bool {
        let Some(zs) = st.zs else { return true };
        let i = self.tb.index(x, y);
        let mut pass = true;
        let mut stencil_value = None;
        if let Some(sides) = zs.stencil {
            let s = sides[if front { 0 } else { 1 }];
            let cur = self.tb.stencil[i];
            let ok = test(
                s.func,
                (s.reference & s.value_mask) as f32,
                (cur & s.value_mask) as f32,
            );
            if !ok {
                stencil_value = Some(stencil_op(s.fail, cur, s.reference));
                pass = false;
            }
        }
        let zq = |v: f32| {
            if self.fb.z_float {
                v
            } else {
                (v as f64 * ((1u64 << self.fb.z_bits) - 1) as f64).round() as f32
            }
        };
        let depth_ok = !pass || test(zs.depth_func, zq(z), zq(self.tb.depth[i]));
        if pass {
            if let Some(sides) = zs.stencil {
                let s = sides[if front { 0 } else { 1 }];
                let cur = self.tb.stencil[i];
                stencil_value = Some(stencil_op(
                    if depth_ok { s.zpass } else { s.zfail },
                    cur,
                    s.reference,
                ));
            }
            pass = depth_ok;
        }
        if write {
            if let (Some(v), Some(sides)) = (stencil_value, zs.stencil) {
                let m = sides[if front { 0 } else { 1 }].write_mask;
                let cur = self.tb.stencil[i];
                self.tb.stencil[i] = (cur & !m) | (v & m);
            }
            if pass && zs.depth_write {
                self.tb.depth[i] = z;
            }
        }
        pass
    }

    #[allow(clippy::too_many_arguments)]
    fn shade(
        &mut self,
        st: &State,
        varyings: &Option<(usize, std::sync::Arc<[u8]>)>,
        vertices: [usize; 3],
        provoking: usize,
        front: bool,
        qx: u32,
        qy: u32,
        px: &mut [Pixel; 4],
        late: bool,
    ) -> Result<(), Stop> {
        let Some((binary, allocation, preload, _, resources, helpers)) = st.shader.as_ref() else {
            // No fragment shader: depth/stencil only.
            for (k, p) in px.iter().enumerate() {
                if p.passed {
                    let (x, y) = (qx + (k as u32 & 1), qy + (k as u32 >> 1));
                    if late {
                        self.depth_stencil(st, x, y, p.z, front, true);
                    }
                    self.occlusion += 1;
                }
            }
            return Ok(());
        };
        let mut lanes: [Shader; 4] = std::array::from_fn(|k| {
            let p = px[k];
            let (x, y) = (qx + (k as u32 & 1), qy + (k as u32 >> 1));
            let mut s = Shader::with_allocation(*binary, *allocation);
            s.resources = *resources;
            s.tls = st.tls;
            let mut f = Fragment {
                blend: st.blend_words,
                active: p.passed,
                frag_z: p.z,
                frag_w: p.w,
                point: p.point,
                ..Fragment::default()
            };
            f.sample_positions = self.fb.sample_positions;
            s.fragment = Some(f);
            if let Some((stride, data)) = varyings {
                s.interpolation = Some(Interpolation {
                    data: data.clone(),
                    stride: *stride,
                    vertices,
                    weights: p.weights,
                    provoking,
                });
            }
            // r58 front facing (0), r59 pixel, r60 coverage, r61 sample mask, r62-63 frame argument
            let values = [
                !front as u32,
                x | (y << 16),
                p.passed as u32,
                1,
                self.fb.frame_arg as u32,
                (self.fb.frame_arg >> 32) as u32,
            ];
            for (n, &v) in values.iter().enumerate() {
                if preload & (1 << (10 + n)) != 0 {
                    s.regs[58 + n] = v;
                }
            }
            // Threads that neither cover a pixel nor help compute derivatives are idle.
            if !p.passed && !helpers {
                s.done = true;
            }
            s
        });
        let budget = (self.budget.min(THREAD_BUDGET as u64 * 4)) as usize;
        run_quad(&mut lanes, &mut self.env, budget).map_err(Stop::Shader)?;
        let spent: u64 = lanes.iter().map(|l| l.instructions).sum();
        self.budget = self.budget.checked_sub(spent).ok_or_else(bad)?;
        for (k, lane) in lanes.iter().enumerate() {
            let p = px[k];
            let f = lane.fragment.as_ref().unwrap();
            if !p.passed || !f.active {
                continue;
            }
            let (x, y) = (qx + (k as u32 & 1), qy + (k as u32 >> 1));
            if late {
                let z = f.depth.unwrap_or(p.z);
                if !self.depth_stencil(st, x, y, z, front, true) {
                    continue;
                }
            }
            self.occlusion += 1;
            let i = self.tb.index(x, y);
            for &(slot, blend) in &st.blends {
                let rt = slot;
                if rt >= self.fb.targets.len() || st.rt_mask & (1 << rt) == 0 {
                    continue;
                }
                let Some(color) = f.color[slot] else { continue };
                if color.iter().any(|v| v.is_nan()) {
                    continue;
                }
                let target = &self.fb.targets[rt];
                let dst = self.tb.color[rt][i];
                self.tb.color[rt][i] = target.quantize(blend.apply_f(color, dst, true));
                self.tb.touched = true;
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn covered(mut v: [[f64; 4]; 3], x: f64, y: f64) -> bool {
        if edge(v[0], v[1], v[2][0], v[2][1]) < 0. {
            v.swap(1, 2);
        }
        (0..3).all(|n| {
            let a = v[n];
            let b = v[(n + 1) % 3];
            let e = edge(a, b, x, y);
            e > 0. || e == 0. && owns(a, b)
        })
    }
    #[test]
    fn shared_edge_has_one_owner_for_both_windings() {
        let a = [0., 0., 0., 1.];
        let b = [2., 0., 0., 1.];
        let c = [2., 2., 0., 1.];
        let d = [0., 2., 0., 1.];
        for first in [[a, b, c], [c, b, a]] {
            for second in [[a, c, d], [d, c, a]] {
                for y in 0..2 {
                    for x in 0..2 {
                        let count = covered(first, x as f64 + 0.5, y as f64 + 0.5) as u32
                            + covered(second, x as f64 + 0.5, y as f64 + 0.5) as u32;
                        assert_eq!(count, 1);
                    }
                }
            }
        }
    }
    #[test]
    fn clipping_at_w_zero_keeps_the_visible_part_and_its_weights() {
        // Vertex 2 is behind the eye (negative 1/w): the triangle is cut where w = 0.
        let tris = clip([[0., 0., 0.5, 1.], [10., 0., 0.5, 1.], [0., 10., 0.5, -1.]]);
        assert!(!tris.is_empty());
        for t in &tris {
            for (k, v) in t.p.iter().enumerate() {
                assert!(v[3] > 0.);
                let s: f64 = t.orig[k].iter().sum();
                assert!((s - 1.).abs() < 1e-9);
            }
        }
        assert!(clip([[0., 0., 0.5, -1.], [1., 0., 0.5, -1.], [0., 1., 0.5, -1.]]).is_empty());
        assert_eq!(
            clip([[0., 0., 0.5, 1.], [1., 0., 0.5, 1.], [0., 1., 0.5, 1.]]).len(),
            1
        );
    }
    #[test]
    fn stencil_operations_wrap_and_saturate() {
        assert_eq!(stencil_op(4, 255, 0), 0);
        assert_eq!(stencil_op(6, 255, 0), 255);
        assert_eq!(stencil_op(5, 0, 0), 255);
        assert_eq!(stencil_op(7, 0, 0), 0);
        assert_eq!(stencil_op(1, 3, 9), 9);
        assert_eq!(stencil_op(3, 0x0f, 0), 0xf0);
    }
}
