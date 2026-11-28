//! End to end through the register interface, as Linux drives it: a binning list and shader
//! records as Mesa writes them (after the kernel's relocation), a rendering list as the
//! kernel's `vc4_render_cl.c` builds it, hand-assembled coordinate, vertex and fragment
//! shaders; then pixels.

use crate::asm::*;
use crate::cl;
use crate::qpu::{op, raddr, sig, waddr};
use crate::tiling;
use crate::{V3d, BPOA, BPOS, CT0CA, CT0EA, CT1CA, CT1EA, INTCTL, INTENA, INT_FLDONE, INT_FRDONE, INT_OUTOMEM};

/// Bus addresses use the 0xc000_0000 alias, as on the Pi.
const BUS: u32 = 0xc000_0000;
const CS: u32 = 0x1_0000;
const VS: u32 = 0x1_1000;
const FS: u32 = 0x1_2000;
const UNIF: u32 = 0x2_0000;
const REC: u32 = 0x3_0000;
const INDICES: u32 = 0x5_0000;
const BCL: u32 = 0x6_0000;
const RCL: u32 = 0x7_0000;
const TILE_ALLOC: u32 = 0x10_0000;
const OVERFLOW: u32 = 0x20_0000;
const COLOR: u32 = 0x40_0000;
const ZS: u32 = 0x60_0000;
const TEX: u32 = 0x80_0000;

#[derive(Default)]
struct Cl(Vec<u8>);
impl Cl {
    fn u8(&mut self, v: u8) -> &mut Self { self.0.push(v); self }
    fn u16(&mut self, v: u16) -> &mut Self { self.0.extend_from_slice(&v.to_le_bytes()); self }
    fn u32(&mut self, v: u32) -> &mut Self { self.0.extend_from_slice(&v.to_le_bytes()); self }
    fn f32(&mut self, v: f32) -> &mut Self { self.u32(v.to_bits()) }
}

fn put(ram: &mut [u8], a: u32, b: &[u8]) { ram[a as usize..a as usize + b.len()].copy_from_slice(b); }
fn put32(ram: &mut [u8], a: u32, w: &[u32]) { for (k, v) in w.iter().enumerate() { put(ram, a + 4 * k as u32, &v.to_le_bytes()); } }
fn get32(ram: &[u8], a: u32) -> u32 { u32::from_le_bytes(ram[a as usize..a as usize + 4].try_into().unwrap()) }

/// Coordinate shader: position (4 floats) from VPM row 0; writes Xc..Wc, Xs|Ys, Zs, 1/Wc.
/// Uniforms: x scale, y scale, z scale, z offset (Mesa's viewport uniforms).
fn coord_shader() -> Vec<I> {
    let mut p = vec![
        ldi(waddr::VPM_SETUP, 0x0040_1a00),       // read 4 rows from 0
        ldi(waddr::VPM_SETUP, 0x0000_1a00).ws(),  // write from row 0
        mov(0, A).ra(raddr::VPM),
        mov(1, A).ra(raddr::VPM),
        mov(2, A).ra(raddr::VPM),
        mov(3, A).ra(raddr::VPM),
        mov(waddr::VPM, A).ra(0),
        mov(waddr::VPM, A).ra(1),
        mov(waddr::VPM, A).ra(2),
        mov(waddr::VPM, A).ra(3),
    ];
    p.extend(screen_tail());
    p
}

/// What both position shaders end with: Xs|Ys, Zs, 1/Wc from ra0..ra3 and the uniforms.
fn screen_tail() -> Vec<I> {
    vec![
        mov(waddr::SFU_RECIP, A).ra(3),
        nop(),
        nop(),
        mov(4, R4),
        nop().mul(op::FMUL, waddr::R0, A, B).ra(0).rb(raddr::UNIF),
        nop().mul(op::FMUL, waddr::R0, R0, A).ra(4),
        nop().add(op::FTOI, 5, R0, R0).pack(1),
        nop().mul(op::FMUL, waddr::R0, A, B).ra(1).rb(raddr::UNIF),
        nop().mul(op::FMUL, waddr::R0, R0, A).ra(4),
        nop().add(op::FTOI, 5, R0, R0).pack(2),
        mov(waddr::VPM, A).ra(5),
        nop().mul(op::FMUL, waddr::R0, A, B).ra(2).rb(raddr::UNIF),
        nop().mul(op::FMUL, waddr::R0, R0, A).ra(4),
        nop().add(op::FADD, waddr::VPM, R0, B).rb(raddr::UNIF),
        mov(waddr::VPM, A).ra(4),
    ]
}

/// Vertex shader: position from rows 0..3, `nvary` varyings from rows 4.. copied out after
/// the screen position.
fn vertex_shader(nvary: u32) -> Vec<I> {
    let mut p = vec![
        ldi(waddr::VPM_SETUP, 0x0000_1a00 | (4 + nvary) << 20),
        ldi(waddr::VPM_SETUP, 0x0000_1a00).ws(),
        mov(0, A).ra(raddr::VPM),
        mov(1, A).ra(raddr::VPM),
        mov(2, A).ra(raddr::VPM),
        mov(3, A).ra(raddr::VPM),
    ];
    for k in 0..nvary { p.push(mov(10 + k as u8, A).ra(raddr::VPM)); }
    p.extend(screen_tail());
    for k in 0..nvary { p.push(mov(waddr::VPM, A).ra(10 + k as u8)); }
    p
}

/// Fragment shader end: the scoreboard unlock on the last instruction.
fn fs_end(mut p: Vec<I>) -> Vec<I> {
    p.push(nop().sig(sig::PROG_END));
    p.push(nop());
    p.push(nop().sig(sig::UNLOCK_SCORE));
    p
}

/// FS: the colour in the first uniform; with `z`, the interpolated depth to the TLB first.
fn flat_fs(z: bool) -> Vec<I> {
    let mut p = vec![];
    if z { p.push(mov(waddr::TLB_Z, B).rb(15)); }
    p.push(mov(waddr::TLB_COLOR_ALL, A).ra(raddr::UNIF));
    fs_end(p)
}

/// FS: four varyings (r, g, b, a in 0..1) interpolated and packed as RGBA8888.
fn gouraud_fs() -> Vec<I> {
    let mut p = vec![ldi(0, 0).ws()];
    for k in 0..4u8 {
        p.push(nop().mul(op::FMUL, waddr::R0, A, B).ra(15).rb(raddr::VARY));
        p.push(nop().add(op::FADD, waddr::R1, R0, R5));
        p.push(nop().mul(op::V8MIN, 0, R1, R1).pm().pack(4 + k));
    }
    p.push(mov(waddr::TLB_COLOR_ALL, B).rb(0));
    fs_end(p)
}

/// FS: texture coordinates (two varyings) through TMU0; P0, P1 from the uniforms.
fn texture_fs() -> Vec<I> {
    fs_end(vec![
        nop().mul(op::FMUL, waddr::R0, A, B).ra(15).rb(raddr::VARY),
        nop().add(op::FADD, waddr::R2, R0, R5),
        nop().mul(op::FMUL, waddr::R0, A, B).ra(15).rb(raddr::VARY),
        nop().add(op::FADD, waddr::R3, R0, R5),
        mov(waddr::TMU0_T, R3),
        mov(waddr::TMU0_S, R2),
        nop().sig(sig::LOAD_TMU0),
        mov(waddr::TLB_COLOR_ALL, R4),
    ])
}

/// FS: additive blending of the uniform colour onto the tile buffer.
fn additive_fs() -> Vec<I> {
    fs_end(vec![
        nop().sig(sig::COLOR_LOAD),
        nop().add(op::A_V8ADDS, waddr::TLB_COLOR_ALL, R4, A).ra(raddr::UNIF),
    ])
}

/// FS: stencil setup from the first uniform, then the second as colour (no depth).
fn stencil_fs() -> Vec<I> {
    fs_end(vec![
        mov(waddr::TLB_STENCIL, A).ra(raddr::UNIF),
        mov(waddr::TLB_COLOR_ALL, A).ra(raddr::UNIF),
    ])
}

/// A scene: RAM, frame size, shaders, draws.
struct Scene {
    ram: Vec<u8>,
    w: u32,
    h: u32,
    bcl: Cl,
    nrec: u32,
    unif_at: u32,
    depth: bool,
    ms: bool,
    tile_alloc_size: u32,
    clear: u32,
    tiling: u8,
    /// where the next draw's vertices go
    vat: u32,
}

/// A vertex: clip position and up to four extra attributes (varyings).
#[derive(Clone, Copy)]
struct Vx { p: [f32; 4], a: [f32; 4] }

/// Clip coordinates of a pixel position (y down) in a w x h frame.
fn px(w: u32, h: u32, x: f32, y: f32, z: f32) -> [f32; 4] { [x / (w as f32 / 2.0) - 1.0, 1.0 - y / (h as f32 / 2.0), z, 1.0] }

impl Scene {
    fn new(w: u32, h: u32) -> Scene {
        let mut s = Scene { ram: vec![0u8; 48 << 20], vat: 0x100_0000, w, h, bcl: Cl::default(), nrec: 0, unif_at: UNIF, depth: false, ms: false,
                            tile_alloc_size: 0x8_0000, clear: 0xff20_1008, tiling: tiling::RASTER };
        put(&mut s.ram, CS, &bytes(&{ let mut p = coord_shader(); p.extend(end()); p }));
        s
    }

    fn tiles(&self) -> (u32, u32) { let t = if self.ms { 32 } else { 64 }; (self.w.div_ceil(t), self.h.div_ceil(t)) }

    /// Start the binning list: tile configuration, state.
    fn begin(&mut self, config: u32) {
        let (tx, ty) = self.tiles();
        let mut b = Cl::default();
        b.u8(cl::TILE_BINNING_MODE_CONFIG).u32(TILE_ALLOC | BUS).u32(self.tile_alloc_size).u32(0x0f_0000 | BUS).u8(tx as u8).u8(ty as u8)
            .u8(1 << 2 | 2 << 5 | self.ms as u8);
        b.u8(cl::START_TILE_BINNING);
        b.u8(cl::PRIMITIVE_LIST_FORMAT).u8(0x12);
        b.u8(cl::CLIP_WINDOW).u16(0).u16(0).u16(self.w as u16).u16(self.h as u16);
        b.u8(cl::CONFIGURATION_BITS).u8(config as u8).u8((config >> 8) as u8).u8((config >> 16) as u8);
        b.u8(cl::VIEWPORT_OFFSET).u16((self.w * 8) as u16).u16((self.h * 8) as u16);
        b.u8(cl::CLIPPER_XY_SCALING).f32(self.w as f32 * 8.0).f32(-(self.h as f32) * 8.0);
        b.u8(cl::CLIPPER_Z_SCALING).f32(0.5).f32(0.5);
        self.bcl = b;
    }

    fn uniforms(&mut self, u: &[u32]) -> u32 {
        let a = self.unif_at;
        put32(&mut self.ram, a, u);
        self.unif_at += 4 * u.len() as u32 + 16;
        a
    }

    /// A draw: vertices, `nvary` varyings from their attributes, the fragment shader with its
    /// uniforms, mode, optional indices.
    fn draw(&mut self, verts: &[Vx], nvary: u32, fs: &[I], fs_unif: &[u32], mode: u8, indices: Option<&[u16]>) {
        let k = self.nrec;
        self.nrec += 1;
        let (vs_at, fs_at) = (VS + 0x400 * k, FS + 0x400 * k);
        put(&mut self.ram, vs_at, &bytes(&{ let mut p = vertex_shader(nvary); p.extend(end()); p }));
        put(&mut self.ram, fs_at, &bytes(fs));
        let vbase = self.vat;
        let stride = 32u32;
        self.vat += (stride * verts.len() as u32 + 0xfff) & !0xfff;
        for (i, v) in verts.iter().enumerate() {
            let w: Vec<u32> = v.p.iter().chain(v.a.iter()).map(|f| f.to_bits()).collect();
            put32(&mut self.ram, vbase + stride * i as u32, &w);
        }
        let vp = [(self.w * 8) as f32, -(self.h as f32) * 8.0, 0.5, 0.5].map(f32::to_bits);
        let cs_u = self.uniforms(&vp);
        let vs_u = self.uniforms(&vp);
        let fs_u = self.uniforms(fs_unif);
        let rec = REC + 0x100 * k;
        let mut r = Cl::default();
        r.u16(cl::FLAG_CLIPPING | cl::FLAG_FS_SINGLE_THREAD).u8(0).u8(nvary as u8).u32(fs_at | BUS).u32(fs_u | BUS)
            .u16(0).u8(1).u8(16 + 4 * nvary as u8).u32(vs_at | BUS).u32(vs_u | BUS)
            .u16(0).u8(1).u8(16).u32(CS | BUS).u32(cs_u | BUS);
        r.u32(vbase | BUS).u8((16 + 4 * nvary - 1) as u8).u8(stride as u8).u8(0).u8(0);
        put(&mut self.ram, rec, &r.0);
        self.bcl.u8(cl::GL_SHADER_STATE).u32(rec | BUS | 1);
        match indices {
            Some(ix) => {
                let ia = INDICES + 0x1000 * k;
                for (i, &v) in ix.iter().enumerate() { put(&mut self.ram, ia + 2 * i as u32, &v.to_le_bytes()); }
                let max = *ix.iter().max().unwrap() as u32;
                self.bcl.u8(cl::GL_INDEXED_PRIMITIVE).u8(mode | 1 << 4).u32(ix.len() as u32).u32(ia | BUS).u32(max);
            }
            None => { self.bcl.u8(cl::GL_ARRAY_PRIMITIVE).u8(mode).u32(verts.len() as u32).u32(0); }
        }
    }

    fn state(&mut self, p: &[u8]) { self.bcl.0.extend_from_slice(p); }

    /// The rendering list as `vc4_create_rcl_bo` writes it: config, clear, one dummy store, then
    /// per tile: tile coordinates, (semaphore wait), branch into its list, Z store, colour store.
    fn rcl(&self, has_bin: bool) -> Vec<u8> {
        let (tx, ty) = self.tiles();
        let mut r = Cl::default();
        let fmt = 1u16 << 2 | (self.tiling as u16) << 6 | self.ms as u16 | (self.ms as u16) << 4;
        r.u8(cl::TILE_RENDERING_MODE_CONFIG).u32(COLOR | BUS).u16(self.w as u16).u16(self.h as u16).u16(fmt);
        r.u8(cl::CLEAR_COLORS).u32(self.clear).u32(self.clear).u32(0xff_ffff).u8(0);
        r.u8(cl::TILE_COORDINATES).u8(0).u8(0);
        r.u8(cl::STORE_TILE_BUFFER_GENERAL).u16(0).u32(0);
        for y in 0..ty {
            for x in 0..tx {
                let last = x == tx - 1 && y == ty - 1;
                r.u8(cl::TILE_COORDINATES).u8(x as u8).u8(y as u8);
                if x == 0 && y == 0 && has_bin { r.u8(cl::WAIT_ON_SEMAPHORE); }
                if has_bin { r.u8(cl::BRANCH_TO_SUB_LIST).u32(TILE_ALLOC + (y * tx + x) * 32 | BUS); }
                if self.depth {
                    r.u8(cl::STORE_TILE_BUFFER_GENERAL).u16(2 | 1 << 4 | 1 << 13).u32(ZS | BUS);
                    r.u8(cl::TILE_COORDINATES).u8(x as u8).u8(y as u8);
                }
                r.u8(if last { cl::STORE_MS_TILE_BUFFER_AND_EOF } else { cl::STORE_MS_TILE_BUFFER });
            }
        }
        r.0
    }

    /// Run the frame through the registers: overflow memory, the binning job, the rendering
    /// job, with the interrupts the kernel waits for.
    fn run(&mut self) -> V3d {
        let mut v = V3d::new();
        v.time_scale = 0.0;
        self.run_on(&mut v);
        v
    }

    fn run_on(&mut self, v: &mut V3d) {
        let has_bin = self.nrec > 0;
        self.bcl.u8(cl::INCREMENT_SEMAPHORE).u8(cl::FLUSH);
        let bcl = std::mem::take(&mut self.bcl).0;
        put(&mut self.ram, BCL, &bcl);
        let rcl = self.rcl(has_bin);
        put(&mut self.ram, RCL, &rcl);
        let ram = &mut self.ram;
        v.write(INTENA as u64, INT_FLDONE | INT_FRDONE | INT_OUTOMEM, ram);
        v.write(BPOA as u64, OVERFLOW | BUS, ram);
        v.write(BPOS as u64, 0x8_0000, ram);
        if has_bin {
            v.write(CT0CA as u64, BCL | BUS, ram);
            v.write(CT0EA as u64, (BCL + bcl.len() as u32) | BUS, ram);
            assert_ne!(v.read(INTCTL as u64) & INT_FLDONE, 0, "binning done");
            assert_eq!(v.read(CT0CA as u64), (BCL + bcl.len() as u32) | BUS);
            v.write(INTCTL as u64, INT_FLDONE, ram);
        }
        v.write(CT1CA as u64, RCL | BUS, ram);
        v.write(CT1EA as u64, (RCL + rcl.len() as u32) | BUS, ram);
        assert_ne!(v.read(INTCTL as u64) & INT_FRDONE, 0, "frame done");
        assert!(v.irq());
        v.write(INTCTL as u64, INT_FRDONE, ram);
        assert!(!v.irq());
    }

    fn pixel(&self, x: u32, y: u32) -> u32 {
        let lay = tiling::Layout::new(self.tiling, 4, self.w);
        get32(&self.ram, COLOR + lay.offset(x, y))
    }
    fn zs(&self, x: u32, y: u32) -> u32 {
        let lay = tiling::Layout::new(tiling::T, 4, self.w);
        get32(&self.ram, ZS + lay.offset(x, y))
    }

    /// The frame as text (one char per pixel: '.' clear colour, '#' else), for diagnostics.
    fn dump(&self) -> String {
        let mut s = String::new();
        for y in 0..self.h { for x in 0..self.w { s.push(if self.pixel(x, y) == self.clear { '.' } else { '#' }); } s.push('\n'); }
        s
    }
}

const RED: u32 = 0xff00_00ff;
const GREEN: u32 = 0xff00_ff00;
const BLUE: u32 = 0xffff_0000;
const ALL_FACES: u32 = cl::config::ENABLE_FRONT | cl::config::ENABLE_BACK | 7 << 12;

fn vx(p: [f32; 4]) -> Vx { Vx { p, a: [0.0; 4] } }

/// A pixel-aligned rectangle as two triangles.
fn rect(w: u32, h: u32, x0: f32, y0: f32, x1: f32, y1: f32, z: f32) -> Vec<Vx> {
    let c = |x, y| vx(px(w, h, x, y, z));
    vec![c(x0, y0), c(x1, y0), c(x1, y1), c(x0, y0), c(x1, y1), c(x0, y1)]
}

#[test]
fn clear_only() {
    for tiling in [tiling::RASTER, tiling::T, tiling::LT] {
        let mut s = Scene::new(100, 70);
        s.tiling = tiling;
        s.run();
        for (x, y) in [(0, 0), (99, 0), (50, 35), (99, 69), (64, 64)] { assert_eq!(s.pixel(x, y), s.clear, "tiling {tiling} at {x},{y}"); }
    }
}

#[test]
fn rectangle_covers_exactly_its_pixels() {
    let (w, h) = (128, 96);
    let mut s = Scene::new(w, h);
    s.begin(ALL_FACES);
    s.draw(&rect(w, h, 10.0, 20.0, 100.0, 70.0, 0.5), 0, &flat_fs(false), &[RED], cl::PRIM_TRIANGLES, None);
    s.run();
    for y in 0..h { for x in 0..w {
        let inside = (10..100).contains(&x) && (20..70).contains(&y);
        assert_eq!(s.pixel(x, y), if inside { RED } else { s.clear }, "pixel {x},{y}\n{}", s.dump());
    } }
}

#[test]
fn indexed_strip_and_fan_and_culling() {
    let (w, h) = (64, 64);
    let c = |x, y| vx(px(w, h, x, y, 0.5));
    // a strip quad (top left), a fan quad (bottom right), drawn with back faces culled: front
    // faces run clockwise on the screen (y down), so without the CW bit (the hardware's
    // clockwise is y up)
    let mut s = Scene::new(w, h);
    s.begin(cl::config::ENABLE_FRONT | 7 << 12);
    s.draw(&[c(0.0, 0.0), c(32.0, 0.0), c(0.0, 32.0), c(32.0, 32.0)], 0, &flat_fs(false), &[RED], cl::PRIM_TRIANGLE_STRIP, Some(&[0, 1, 2, 3]));
    s.draw(&[c(32.0, 32.0), c(64.0, 32.0), c(64.0, 64.0), c(32.0, 64.0)], 0, &flat_fs(false), &[GREEN], cl::PRIM_TRIANGLE_FAN, None);
    // wound the other way: culled
    s.draw(&[c(0.0, 32.0), c(32.0, 64.0), c(32.0, 32.0)], 0, &flat_fs(false), &[BLUE], cl::PRIM_TRIANGLES, None);
    s.run();
    assert_eq!(s.pixel(5, 5), RED, "\n{}", s.dump());
    assert_eq!(s.pixel(31, 31), RED);
    assert_eq!(s.pixel(40, 40), GREEN);
    assert_eq!(s.pixel(63, 63), GREEN);
    assert_eq!(s.pixel(20, 40), s.clear, "back face culled");
}

#[test]
fn gouraud_varyings_interpolate() {
    let (w, h) = (64, 64);
    let mut s = Scene::new(w, h);
    s.begin(ALL_FACES);
    let v = |x, y, c: [f32; 4]| Vx { p: px(w, h, x, y, 0.5), a: c };
    s.draw(&[v(0.0, 0.0, [1.0, 0.0, 0.0, 1.0]), v(64.0, 0.0, [0.0, 1.0, 0.0, 1.0]), v(0.0, 64.0, [0.0, 0.0, 1.0, 1.0])], 4, &gouraud_fs(), &[], cl::PRIM_TRIANGLES, None);
    s.run();
    let near = |got: u32, want: [f32; 4]| {
        let g = got.to_le_bytes();
        for k in 0..4 { assert!((g[k] as f32 - want[k] * 255.0).abs() <= 2.0, "channel {k}: got {got:#x}, want {want:?}"); }
    };
    // at pixel (x, y) the colour is (1 - u - v, u, v) with u = (x + .5) / 64, v = (y + .5) / 64
    for (x, y) in [(0u32, 0u32), (10, 10), (30, 5), (5, 50), (31, 31)] {
        let (u, vv) = ((x as f32 + 0.5) / 64.0, (y as f32 + 0.5) / 64.0);
        near(s.pixel(x, y), [1.0 - u - vv, u, vv, 1.0]);
    }
    assert_eq!(s.pixel(40, 40), s.clear);
}

#[test]
fn perspective_correct_varyings() {
    // w = 4 at the right vertex: along the top row the varying (0 at the left, 1 at the right)
    // follows (u / 4) / ((1 - u) + u / 4), not u
    let (w, h) = (64, 64);
    let mut s = Scene::new(w, h);
    s.begin(ALL_FACES);
    let v = |x: f32, y: f32, ww: f32, c: f32| { let p = px(w, h, x, y, 0.5); Vx { p: [p[0] * ww, p[1] * ww, p[2] * ww, ww], a: [c, c, c, 1.0] } };
    s.draw(&[v(0.0, 0.0, 1.0, 0.0), v(64.0, 0.0, 4.0, 1.0), v(0.0, 64.0, 1.0, 0.0)], 4, &gouraud_fs(), &[], cl::PRIM_TRIANGLES, None);
    s.run();
    for x in [8u32, 20, 32, 50] {
        // screen barycentrics at the pixel centre (x + .5, .5): l1 = (x + .5) / 64, l2 = .5 / 64
        let (l1, l2) = ((x as f32 + 0.5) / 64.0, 0.5 / 64.0);
        let l0 = 1.0 - l1 - l2;
        let want = (l1 / 4.0) / (l0 + l1 / 4.0 + l2);
        let got = s.pixel(x, 0) & 0xff;
        assert!((got as f32 - want * 255.0).abs() <= 1.5, "x {x}: got {got}, want {}", want * 255.0);
    }
}

#[test]
fn texture_mapping_nearest() {
    let (w, h) = (64, 64);
    let mut s = Scene::new(w, h);
    // a 16x16 texture (LT at this size): 2x2-texel checks, the dark ones carrying their position
    let lay = tiling::Layout::new(tiling::LT, 4, 16);
    let tex = |x: u32, y: u32| if (x / 2 + y / 2) % 2 == 0 { 0xffff_ffff } else { 0xff00_0000 | x << 4 | y << 12 };
    for y in 0..16 { for x in 0..16 { put32(&mut s.ram, TEX + lay.offset(x, y), &[tex(x, y)]); } }
    s.begin(ALL_FACES);
    let v = |x, y, u: f32, t: f32| Vx { p: px(w, h, x, y, 0.5), a: [u, t, 0.0, 0.0] };
    let quad = [v(0.0, 0.0, 0.0, 0.0), v(64.0, 0.0, 1.0, 0.0), v(64.0, 64.0, 1.0, 1.0), v(0.0, 0.0, 0.0, 0.0), v(64.0, 64.0, 1.0, 1.0), v(0.0, 64.0, 0.0, 1.0)];
    let p0 = TEX | BUS;
    let p1 = 16 << 20 | 16 << 8 | 1 << 7 | 1 << 4 | 1 << 2 | 1;
    s.draw(&quad, 2, &texture_fs(), &[p0, p1], cl::PRIM_TRIANGLES, None);
    s.run();
    for y in 0..64 { for x in 0..64 { assert_eq!(s.pixel(x, y), tex(x / 4, y / 4), "{x},{y}"); } }
}

#[test]
fn depth_test_keeps_the_nearer_surface() {
    let (w, h) = (64, 64);
    let config = ALL_FACES & !(7 << 12) | 1 << 12 | cl::config::Z_UPDATE; // LESS, Z updates
    for order in [0, 1] {
        let mut s = Scene::new(w, h);
        s.depth = true;
        s.begin(config);
        let near = rect(w, h, 0.0, 0.0, 40.0, 40.0, -0.5);
        let far = rect(w, h, 20.0, 20.0, 64.0, 64.0, 0.5);
        if order == 0 {
            s.draw(&far, 0, &flat_fs(true), &[BLUE], cl::PRIM_TRIANGLES, None);
            s.draw(&near, 0, &flat_fs(true), &[RED], cl::PRIM_TRIANGLES, None);
        } else {
            s.draw(&near, 0, &flat_fs(true), &[RED], cl::PRIM_TRIANGLES, None);
            s.draw(&far, 0, &flat_fs(true), &[BLUE], cl::PRIM_TRIANGLES, None);
        }
        s.run();
        assert_eq!(s.pixel(30, 30), RED, "order {order}");
        assert_eq!(s.pixel(50, 50), BLUE);
        assert_eq!(s.pixel(10, 10), RED);
        // the stored depth: z = -0.5 -> Zs 0.25
        assert_eq!(s.zs(30, 30) >> 8, (0.25f64 * 16777215.0).round() as u32);
        assert_eq!(s.zs(5, 60) >> 8, 0xff_ffff, "cleared depth");
    }
}

#[test]
fn additive_blending_reads_the_tile_buffer() {
    let (w, h) = (64, 64);
    let mut s = Scene::new(w, h);
    s.clear = 0xff10_2030;
    s.begin(ALL_FACES);
    s.draw(&rect(w, h, 0.0, 0.0, 32.0, 64.0, 0.0), 0, &additive_fs(), &[0x0001_0101], cl::PRIM_TRIANGLES, None);
    s.draw(&rect(w, h, 16.0, 0.0, 64.0, 64.0, 0.0), 0, &additive_fs(), &[0x00f0_0101], cl::PRIM_TRIANGLES, None);
    s.run();
    assert_eq!(s.pixel(5, 5), 0xff11_2131);
    assert_eq!(s.pixel(20, 5), 0xffff_2232);
    assert_eq!(s.pixel(50, 5), 0xffff_2131);
}

#[test]
fn stencil_marks_and_masks() {
    let (w, h) = (64, 64);
    let mut s = Scene::new(w, h);
    s.begin(ALL_FACES);
    // pass 1: always pass, replace with ref 1 (front and back), colour red, left half
    let always_replace = 3 << 30 | 3 << 28 | 2 << 22 | 7 << 16 | 1 << 8 | 0xff;
    s.draw(&rect(w, h, 0.0, 0.0, 32.0, 64.0, 0.0), 0, &stencil_fs(), &[always_replace, RED], cl::PRIM_TRIANGLES, None);
    // pass 2: equal to 1 passes, keep; green everywhere it passes (whole frame drawn)
    let equal_one = 3 << 30 | 3 << 28 | 1 << 25 | 1 << 22 | 1 << 19 | 2 << 16 | 1 << 8 | 0xff;
    s.draw(&rect(w, h, 0.0, 0.0, 64.0, 64.0, 0.0), 0, &stencil_fs(), &[equal_one, GREEN], cl::PRIM_TRIANGLES, None);
    s.run();
    assert_eq!(s.pixel(10, 10), GREEN);
    assert_eq!(s.pixel(50, 10), s.clear);
}

#[test]
fn spans_tiles_and_clips_against_the_near_plane_and_guard_band() {
    let (w, h) = (200, 150);
    let mut s = Scene::new(w, h);
    s.begin(ALL_FACES);
    // a huge triangle (far outside the 12.4 guard band) covering the whole frame
    s.draw(&[vx([-1000.0, 1000.0, 0.0, 1.0]), vx([3000.0, 1000.0, 0.0, 1.0]), vx([-1000.0, -3000.0, 0.0, 1.0])], 0, &flat_fs(false), &[RED], cl::PRIM_TRIANGLES, None);
    s.run();
    for (x, y) in [(0, 0), (199, 0), (0, 149), (199, 149), (100, 75), (63, 64), (64, 63)] { assert_eq!(s.pixel(x, y), RED, "{x},{y}"); }

    // a triangle reaching through the near plane (z < -w at the bottom vertex): clipped where
    // Zs = 0, a third of the way down, so rows 16 to 26 are drawn
    let mut s = Scene::new(64, 64);
    s.begin(ALL_FACES);
    s.draw(&[vx([-0.5, 0.5, 0.0, 1.0]), vx([0.5, 0.5, 0.0, 1.0]), vx([0.0, -0.5, -3.0, 1.0])], 0, &flat_fs(false), &[GREEN], cl::PRIM_TRIANGLES, None);
    s.run();
    let lit: Vec<(u32, u32)> = (0..64).flat_map(|y| (0..64).map(move |x| (x, y))).filter(|&(x, y)| s.pixel(x, y) == GREEN).collect();
    assert!(lit.iter().any(|&(_, y)| y == 16) && lit.iter().any(|&(_, y)| y == 26), "rows 16..26 drawn:\n{}", s.dump());
    assert!(lit.iter().all(|&(_, y)| (16..27).contains(&y)), "nothing past the near plane:\n{}", s.dump());
    // with w < 0 at a vertex: the part in front (w > 0) projects upward from the edge at row
    // 16 and widens toward the top (x/w grows without bound as w goes to 0); nothing below
    let mut s = Scene::new(64, 64);
    s.begin(ALL_FACES);
    s.draw(&[vx([-0.5, 0.5, 0.0, 1.0]), vx([0.5, 0.5, 0.0, 1.0]), vx([0.0, 0.3, 0.0, -1.0])], 0, &flat_fs(false), &[GREEN], cl::PRIM_TRIANGLES, None);
    s.run();
    let lit: Vec<(u32, u32)> = (0..64).flat_map(|y| (0..64).map(move |x| (x, y))).filter(|&(x, y)| s.pixel(x, y) == GREEN).collect();
    assert!(lit.iter().all(|&(_, y)| y < 16), "nothing below the edge:\n{}", s.dump());
    let width = |row: u32| lit.iter().filter(|&&(_, y)| y == row).count();
    assert!(width(0) > width(15) && width(15) >= 32, "widening upward:\n{}", s.dump());
}

#[test]
fn points_and_lines() {
    let (w, h) = (64, 64);
    let mut s = Scene::new(w, h);
    s.begin(ALL_FACES);
    s.state(&[cl::POINT_SIZE, 0, 0, 0x80, 0x40]); // 4.0
    s.draw(&[vx(px(w, h, 10.0, 10.0, 0.0))], 0, &flat_fs(false), &[RED], cl::PRIM_POINTS, None);
    s.draw(&[vx(px(w, h, 20.0, 40.5, 0.0)), vx(px(w, h, 50.0, 40.5, 0.0))], 0, &flat_fs(false), &[GREEN], cl::PRIM_LINES, None);
    s.run();
    let red: Vec<(u32, u32)> = (0..64).flat_map(|y| (0..64).map(move |x| (x, y))).filter(|&(x, y)| s.pixel(x, y) == RED).collect();
    assert_eq!(red.len(), 16, "a 4x4 point\n{}", s.dump());
    assert!(red.iter().all(|&(x, y)| (8..12).contains(&x) && (8..12).contains(&y)));
    let green: Vec<(u32, u32)> = (0..64).flat_map(|y| (0..64).map(move |x| (x, y))).filter(|&(x, y)| s.pixel(x, y) == GREEN).collect();
    assert_eq!(green.len(), 30, "one row of 30 pixels\n{}", s.dump());
    assert!(green.iter().all(|&(_, y)| y == 40));
}

#[test]
fn multisample_resolve() {
    let (w, h) = (64, 64);
    let mut s = Scene::new(w, h);
    s.ms = true;
    s.clear = 0xff00_0000;
    s.begin(ALL_FACES | cl::config::OVERSAMPLE_4X);
    // a triangle whose edge crosses pixels: edge pixels get partial coverage, resolved to grey
    s.draw(&[vx(px(w, h, 0.0, 0.0, 0.0)), vx(px(w, h, 64.0, 0.0, 0.0)), vx(px(w, h, 0.0, 64.0, 0.0))], 0, &flat_fs(false), &[0xffff_ffff], cl::PRIM_TRIANGLES, None);
    s.run();
    assert_eq!(s.pixel(5, 5), 0xffff_ffff);
    assert_eq!(s.pixel(60, 60), 0xff00_0000);
    let partial = (0..64).filter(|&x| { let p = s.pixel(x, 63 - x) & 0xff; p != 0 && p != 0xff }).count();
    assert!(partial > 32, "diagonal pixels partly covered: {partial}");
}

#[test]
fn out_of_memory_waits_for_overflow_memory() {
    let (w, h) = (256, 256);
    let mut s = Scene::new(w, h);
    // tile allocation memory: just the initial blocks
    s.tile_alloc_size = 16 * 32 + 256;
    s.begin(ALL_FACES);
    for k in 0..20 {
        let o = k as f32 * 4.0;
        s.draw(&rect(w, h, o, o, 200.0 + o / 4.0, 200.0, 0.0), 0, &flat_fs(false), &[RED + k], cl::PRIM_TRIANGLES, None);
    }
    s.bcl.u8(cl::INCREMENT_SEMAPHORE).u8(cl::FLUSH);
    let bcl = std::mem::take(&mut s.bcl).0;
    put(&mut s.ram, BCL, &bcl);
    let rcl = s.rcl(true);
    put(&mut s.ram, RCL, &rcl);
    let mut v = V3d::new();
    v.time_scale = 0.0;
    let ram = &mut s.ram;
    v.write(INTENA as u64, INT_FLDONE | INT_FRDONE | INT_OUTOMEM, ram);
    // no overflow memory at first: OUTOMEM is up (and the kernel's interrupt would fire)
    assert_ne!(v.read(INTCTL as u64) & INT_OUTOMEM, 0);
    v.write(CT0CA as u64, BCL | BUS, ram);
    v.write(CT0EA as u64, (BCL + bcl.len() as u32) | BUS, ram);
    assert_eq!(v.read(INTCTL as u64) & INT_FLDONE, 0, "binning waits for memory");
    assert!(v.busy());
    // the kernel's overflow work: memory, clear OUTOMEM
    let mut chunks = 0;
    while v.read(INTCTL as u64) & INT_FLDONE == 0 {
        chunks += 1;
        assert!(chunks < 64);
        v.write(BPOA as u64, OVERFLOW + 0x400 * chunks | BUS, ram);
        v.write(BPOS as u64, 0x400, ram);
        v.write(INTCTL as u64, INT_OUTOMEM, ram);
    }
    assert!(chunks > 1, "needed several chunks: {chunks}");
    v.write(CT1CA as u64, RCL | BUS, ram);
    v.write(CT1EA as u64, (RCL + rcl.len() as u32) | BUS, ram);
    assert_ne!(v.read(INTCTL as u64) & INT_FRDONE, 0);
    assert_eq!(s.pixel(100, 100), RED + 19);
    assert_eq!(s.pixel(2, 100), s.clear);
}

#[test]
fn done_interrupts_follow_host_time() {
    let (w, h) = (64, 64);
    let mut s = Scene::new(w, h);
    s.begin(ALL_FACES);
    s.draw(&rect(w, h, 0.0, 0.0, 64.0, 64.0, 0.0), 0, &flat_fs(false), &[RED], cl::PRIM_TRIANGLES, None);
    s.bcl.u8(cl::INCREMENT_SEMAPHORE).u8(cl::FLUSH);
    let bcl = std::mem::take(&mut s.bcl).0;
    put(&mut s.ram, BCL, &bcl);
    let mut v = V3d::new();
    v.time_scale = 1000.0;
    v.write(INTENA as u64, INT_FLDONE, &mut s.ram);
    v.write(BPOS as u64, 0x8_0000, &mut s.ram);
    v.write(CT0CA as u64, BCL | BUS, &mut s.ram);
    v.write(CT0EA as u64, (BCL + bcl.len() as u32) | BUS, &mut s.ram);
    assert!(!v.irq(), "not yet");
    let t = v.next_event().expect("interrupt scheduled");
    v.tick(t, &mut s.ram);
    assert!(v.irq());
}

#[test]
fn snapshot_while_waiting_for_memory() {
    let (w, h) = (128, 128);
    let mut s = Scene::new(w, h);
    s.tile_alloc_size = 4 * 32 + 256;
    s.begin(ALL_FACES);
    for k in 0..30 { s.draw(&rect(w, h, 0.0, 0.0, 128.0, 128.0, 0.0), 0, &flat_fs(false), &[RED + k], cl::PRIM_TRIANGLES, None); }
    s.bcl.u8(cl::INCREMENT_SEMAPHORE).u8(cl::FLUSH);
    let bcl = std::mem::take(&mut s.bcl).0;
    put(&mut s.ram, BCL, &bcl);
    let mut v = V3d::new();
    v.time_scale = 0.0;
    v.write(CT0CA as u64, BCL | BUS, &mut s.ram);
    v.write(CT0EA as u64, (BCL + bcl.len() as u32) | BUS, &mut s.ram);
    assert!(v.busy());
    let mut w_ = emu_core::snap::W(Vec::new());
    v.save_state(&mut w_);
    let mut v2 = V3d::new();
    v2.time_scale = 0.0;
    v2.load_state(&mut emu_core::snap::R::new(&w_.0)).unwrap();
    assert!(v2.busy());
    v2.write(BPOA as u64, OVERFLOW | BUS, &mut s.ram);
    v2.write(BPOS as u64, 0x8_0000, &mut s.ram);
    assert_ne!(v2.read(INTCTL as u64) & INT_FLDONE, 0, "the restored binner finished");
    let rcl = s.rcl(true);
    put(&mut s.ram, RCL, &rcl);
    v2.write(CT1CA as u64, RCL | BUS, &mut s.ram);
    v2.write(CT1EA as u64, (RCL + rcl.len() as u32) | BUS, &mut s.ram);
    assert_eq!(s.pixel(64, 64), RED + 29);
}

#[test]
fn identification() {
    let mut v = V3d::new();
    assert_eq!(v.read(crate::IDENT0 as u64), 0x0244_3356);
    let i1 = v.read(crate::IDENT1 as u64);
    // Mesa: version = IDENT0's major * 10 + IDENT1's revision = 21
    assert_eq!((v.read(crate::IDENT0 as u64) >> 24) * 10 + (i1 & 0xf), 21);
    assert_eq!(i1 >> 4 & 0xf, 3, "three slices");
    assert_eq!(i1 >> 8 & 0xf, 4, "four QPUs per slice");
}

#[test]
fn capture_and_replay() {
    let dir = std::env::temp_dir().join(format!("vc4-capture-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let _ = std::fs::remove_file(dir.join("jobs.bin"));
    let (w, h) = (64, 64);
    let mut s = Scene::new(w, h);
    s.begin(ALL_FACES);
    s.draw(&rect(w, h, 8.0, 8.0, 40.0, 30.0, 0.0), 0, &flat_fs(false), &[GREEN], cl::PRIM_TRIANGLES, None);
    let mut v = V3d::new();
    v.time_scale = 0.0;
    v.exec().capture = Some(dir.to_string_lossy().into_owned());
    s.run_on(&mut v);
    assert_eq!(s.pixel(10, 10), GREEN);
    let data = std::fs::read(dir.join("jobs.bin")).unwrap();
    let jobs = crate::capture::parse(&data).unwrap();
    assert_eq!(jobs.len(), 2);
    // replay into fresh memory: the frame comes out the same
    let mut ram = vec![0u8; 48 << 20];
    let frames = crate::capture::replay(&jobs, &mut ram);
    assert_eq!(frames.len(), 1);
    assert_eq!(frames[0].addr & 0x3fff_ffff, COLOR);
    assert_eq!(get32(&ram, COLOR + 4 * (10 * 64 + 10)), GREEN);
    assert_eq!(get32(&ram, COLOR + 4 * (50 * 64 + 50)), s.clear);
    let _ = std::fs::remove_dir_all(&dir);
}

/// Throughput at 1920x1080 (run with `cargo test --release -p vc4-3d -- --ignored bench --nocapture`).
#[test]
#[ignore]
fn bench_full_hd() {
    let (w, h) = (1920u32, 1080u32);
    let mut s = Scene::new(w, h);
    s.depth = true;
    s.begin(ALL_FACES & !(7 << 12) | 1 << 12 | cl::config::Z_UPDATE);
    // 4 layers of 40x24 quads (each a 48x45-pixel cell) with gouraud shading: ~4x the frame's
    // pixels shaded, 7680 triangles
    let mut verts = Vec::new();
    for layer in 0..4 {
        let z = 0.5 - layer as f32 * 0.2;
        for cy in 0..24 { for cx in 0..40 {
            let (x0, y0) = (cx as f32 * 48.0, cy as f32 * 45.0);
            let c = [cx as f32 / 40.0, cy as f32 / 24.0, layer as f32 / 4.0, 1.0];
            let v = |x: f32, y: f32| Vx { p: px(w, h, x, y, z), a: c };
            verts.extend([v(x0, y0), v(x0 + 48.0, y0), v(x0 + 48.0, y0 + 45.0), v(x0, y0), v(x0 + 48.0, y0 + 45.0), v(x0, y0 + 45.0)]);
        } }
    }
    // shader with depth write: Z, then gouraud
    let mut fs = vec![mov(waddr::TLB_Z, B).rb(15)];
    let g = gouraud_fs();
    fs.extend_from_slice(&g);
    for chunk in verts.chunks(6 * 960) { s.draw(chunk, 4, &fs, &[], cl::PRIM_TRIANGLES, None); }
    let reps: usize = std::env::var("BENCH_REPS").ok().and_then(|v| v.parse().ok()).unwrap_or(1);
    s.bcl.u8(cl::INCREMENT_SEMAPHORE).u8(cl::FLUSH);
    let bcl = std::mem::take(&mut s.bcl).0;
    put(&mut s.ram, BCL, &bcl);
    let rcl = s.rcl(true);
    put(&mut s.ram, RCL, &rcl);
    let mut v = V3d::new();
    v.time_scale = 0.0;
    let t = std::time::Instant::now();
    for _ in 0..reps {
        let ram = &mut s.ram;
        v.write(BPOS as u64, 0x8_0000, ram);
        v.write(CT0CA as u64, BCL | BUS, ram);
        v.write(CT0EA as u64, (BCL + bcl.len() as u32) | BUS, ram);
        v.write(CT1CA as u64, RCL | BUS, ram);
        v.write(CT1EA as u64, (RCL + rcl.len() as u32) | BUS, ram);
    }
    let el = t.elapsed();
    eprintln!("1080p frame: {:.1} ms; {}", el.as_secs_f64() * 1e3 / reps as f64, v.stats());
}

/// With the cores on host threads, jobs run on the GPU thread: the register write returns at
/// once, the binning job's lists are laid out when its result is collected, and the done
/// interrupts come when the jobs have finished.
#[test]
fn gpu_thread_jobs() {
    let (w, h) = (128, 128);
    let mut s = Scene::new(w, h);
    s.begin(ALL_FACES);
    s.draw(&rect(w, h, 10.0, 10.0, 100.0, 100.0, 0.0), 0, &flat_fs(false), &[BLUE], cl::PRIM_TRIANGLES, None);
    s.bcl.u8(cl::INCREMENT_SEMAPHORE).u8(cl::FLUSH);
    let bcl = std::mem::take(&mut s.bcl).0;
    put(&mut s.ram, BCL, &bcl);
    let rcl = s.rcl(true);
    put(&mut s.ram, RCL, &rcl);
    let mut v = V3d::new();
    v.wall_clock = true;
    let wait = |v: &mut V3d, ram: &mut Vec<u8>, bit: u32| {
        let t0 = std::time::Instant::now();
        while v.read(INTCTL as u64) & bit == 0 {
            v.tick(0, ram);
            assert!(t0.elapsed() < std::time::Duration::from_secs(20), "no interrupt {bit:#x}");
            std::thread::yield_now();
        }
    };
    v.write(BPOA as u64, OVERFLOW | BUS, &mut s.ram);
    v.write(BPOS as u64, 0x8_0000, &mut s.ram);
    v.write(CT0CA as u64, BCL | BUS, &mut s.ram);
    v.write(CT0EA as u64, (BCL + bcl.len() as u32) | BUS, &mut s.ram);
    wait(&mut v, &mut s.ram, INT_FLDONE);
    assert_eq!(v.read(CT0CA as u64), (BCL + bcl.len() as u32) | BUS);
    v.write(CT1CA as u64, RCL | BUS, &mut s.ram);
    v.write(CT1EA as u64, (RCL + rcl.len() as u32) | BUS, &mut s.ram);
    wait(&mut v, &mut s.ram, INT_FRDONE);
    assert!(!v.busy());
    assert_eq!(s.pixel(50, 50), BLUE);
    assert_eq!(s.pixel(5, 50), s.clear);
}

/// Mip level selection from the quads' derivatives: a 64x64 texture drawn into 32x32 pixels
/// samples level 1 everywhere, also along the triangles' shared diagonal (quads there are only
/// partly covered).
#[test]
fn texture_mip_selection() {
    let (w, h) = (64, 64);
    let mut s = Scene::new(w, h);
    let l1 = tiling::Layout::new(tiling::T, 4, 32);
    let l0 = tiling::Layout::new(tiling::T, 4, 64);
    let size1 = tiling::size(32, 32, 4, tiling::T);
    let base = TEX + size1; // level 1 sits below level 0
    for y in 0..32 { for x in 0..32 { put32(&mut s.ram, TEX + l1.offset(x, y), &[GREEN]); } }
    for y in 0..64 { for x in 0..64 { put32(&mut s.ram, base + l0.offset(x, y), &[RED]); } }
    s.begin(ALL_FACES);
    let v = |x, y, u: f32, t: f32| Vx { p: px(w, h, x, y, 0.5), a: [u, t, 0.0, 0.0] };
    let quad = [v(8.0, 8.0, 0.0, 0.0), v(40.0, 8.0, 1.0, 0.0), v(40.0, 40.0, 1.0, 1.0), v(8.0, 8.0, 0.0, 0.0), v(40.0, 40.0, 1.0, 1.0), v(8.0, 40.0, 0.0, 1.0)];
    let p0 = base | BUS | 1; // one mip level below level 0
    let p1 = 64 << 20 | 64 << 8 | 1 << 7 | 2 << 4; // nearest, near-mip-near, repeat
    s.draw(&quad, 2, &texture_fs(), &[p0, p1], cl::PRIM_TRIANGLES, None);
    s.run();
    for y in 8..40 { for x in 8..40 { assert_eq!(s.pixel(x, y), GREEN, "{x},{y}"); } }
}

/// dFdx as Mesa computes it (`ntq_fddx`: rotate by 1 and 15, pick by the element's x parity)
/// of a varying that grows by 1/64 per pixel: 1.0 after scaling by 64, on every covered pixel,
/// also where a quad is only partly inside the triangle (its helper pixels must be
/// interpolated, not left over from another quad).
#[test]
fn derivatives_on_partly_covered_quads() {
    let (w, h) = (64, 64);
    let mut s = Scene::new(w, h);
    s.begin(ALL_FACES);
    let v = |x: f32, y: f32| Vx { p: px(w, h, x, y, 0.5), a: [x / 64.0, 0.0, 0.0, 0.0] };
    let fs = fs_end(vec![
        nop().mul(op::FMUL, waddr::R0, A, B).ra(15).rb(raddr::VARY),
        nop().add(op::FADD, waddr::R2, R0, R5),
        nop().add(op::AND, waddr::NOP, A, B).ra(raddr::ELEM_QPU).small(1).sf(),
        nop().mul(op::V8MIN, waddr::R3, R2, R2).small(49),           // from the left (i - 1)
        nop().mul(op::V8MIN, waddr::R0, R2, R2).small(63),           // from the right (i + 1)
        nop().add(op::FSUB, waddr::R1, R2, R3).cond_add(COND_ZC),    // odd x: src - left
        nop().add(op::FSUB, waddr::R1, R0, R2).cond_add(COND_ZS),    // even x: right - src
        nop().mul(op::FMUL, waddr::R1, R1, B).small(38),             // * 64.0
        nop().mul(op::V8MIN, waddr::TLB_COLOR_ALL, R1, R1).pm().pack(3),
    ]);
    s.draw(&[v(0.0, 0.0), v(64.0, 64.0), v(0.0, 64.0)], 4, &fs, &[], cl::PRIM_TRIANGLES, None);
    s.run();
    let mut n = 0;
    for y in 0..64 { for x in 0..64 {
        let p = s.pixel(x, y);
        if p == s.clear { continue; }
        n += 1;
        assert!(p & 0xff >= 0xfd, "dFdx at {x},{y}: {p:#x}");
    } }
    assert!(n > 2000, "{n} pixels drawn");
}

/// Random control lists, shader records and QPU programs: the model may draw garbage, but
/// must neither panic (overflow checks in debug builds) nor hang.
#[test]
fn random_input_does_not_panic() {
    let mut seed = 0x1234_5678_9abc_def0u64;
    let mut rnd = move || { seed ^= seed << 13; seed ^= seed >> 7; seed ^= seed << 17; seed };
    for round in 0..120 {
        let mut ram = vec![0u8; 4 << 20];
        // a mix of random bytes and valid packet opcodes
        for k in 0..4096 {
            let r = rnd();
            ram[0x1000 + k] = if r & 3 == 0 { [0u8, 1, 4, 6, 7, 16, 17, 18, 24, 25, 26, 28, 29, 32, 33, 48, 49, 56, 64, 96, 102, 112, 113, 114, 115][(r >> 8) as usize % 25] } else { (r >> 16) as u8 };
        }
        for k in (0x8000..0x40_0000).step_by(8) { let r = rnd(); if r & 15 == 0 { ram[k..k + 8].copy_from_slice(&rnd().to_le_bytes()); } }
        let mut v = V3d::new();
        v.time_scale = 0.0;
        v.write(BPOA as u64, 0x30_0000, &mut ram);
        v.write(BPOS as u64, 0x1000, &mut ram);
        let (start, len) = (0x1000u32 + (rnd() % 64) as u32, (rnd() % 4000) as u32);
        if round % 2 == 0 {
            v.write(CT0CA as u64, start, &mut ram);
            v.write(CT0EA as u64, start + len, &mut ram);
            v.write(BPOS as u64, 0x8000, &mut ram);
        } else {
            v.write(CT1CA as u64, start, &mut ram);
            v.write(CT1EA as u64, start + len, &mut ram);
        }
        // a random user program, now and then
        if round % 20 != 0 { continue; }
        for k in (0x2_0000..0x2_0400).step_by(8) { ram[k..k + 8].copy_from_slice(&rnd().to_le_bytes()); }
        v.write(crate::SRQUA as u64, 0x3_0000, &mut ram);
        v.write(crate::SRQPC as u64, 0x2_0000, &mut ram);
    }
}
