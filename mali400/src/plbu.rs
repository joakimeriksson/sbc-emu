//! The PLBU ("polygon list builder unit"): the GP's second command list. It assembles
//! primitives from the vertex shader's output positions, clips and culls them, and writes
//! the polygon lists the pixel processors read.
//!
//! The list format between the PLBU and the PPs is internal to the GPU, so the model uses
//! its own: one list per GP job in the tile heap, linked from every PLB block the driver
//! allocated (the PP stream points at those blocks). Records are four words:
//!
//! | word 0 | words 1-3 |
//! |---|---|
//! | `STATE` | RSW address, gl_Position array, gl_PointSize array |
//! | `STATE2` | scissor (minx, maxx), (miny, maxy), point size / line width |
//! | `PRIM` \| kind | up to three vertex indices |
//! | `PRIM_ABS` \| kind | addresses of three vertices the PLBU made by clipping |
//! | `LINK` | address of the next record |
//! | `END` | |
//!
//! A clipped vertex is 16 bytes of position (x, y, z, 1/w) followed by its varyings in the
//! layout of the draw's render state.
//!
//! When the heap runs out the PLBU stops with the OUT_OF_MEM interrupt and continues where
//! it was once the driver has grown the heap (`UPDATE_PLBU_ALLOC`), as the hardware does.

use crate::vm::Vm;

pub const TAG_STATE: u32 = 0xa100_0000;
pub const TAG_STATE2: u32 = 0xa200_0000;
pub const TAG_PRIM: u32 = 0xa300_0000;
pub const TAG_PRIM_ABS: u32 = 0xa400_0000;
pub const TAG_LINK: u32 = 0xae00_0000;
pub const TAG_END: u32 = 0xaf00_0000;
pub const TAG_MASK: u32 = 0xff00_0000;

pub const KIND_POINT: u32 = 0;
pub const KIND_LINE: u32 = 1;
pub const KIND_TRIANGLE: u32 = 2;
/// three corners of a parallelogram (Mesa's blits)
pub const KIND_RECT: u32 = 3;

/// Room kept at the end of a heap area for the LINK or END record.
const RESERVE: u32 = 16;

#[derive(Default, Clone, Copy)]
struct State {
    viewport: [f32; 4], // left, right, bottom, top
    depth_range: [f32; 2],
    cull: u32,
    index_field: u32,
    force_point_size: bool,
    rsw: u32,
    gl_pos: u32,
    indexed_gl_pos: u32,
    point_size: u32,
    indices: u32,
    scissor: [u32; 4], // minx, maxx, miny, maxy (exclusive max)
    low_prim_size: f32,
}

#[derive(Clone, Copy)]
struct Draw {
    indexed: bool,
    first: u32,
    count: u32,
    mode: u32,
    /// next primitive to emit
    next: u32,
}

/// A PLBU command list in progress.
#[derive(Default)]
pub struct Plbu {
    st: State,
    pc: u32,
    end: u32,
    cursor: u32,
    limit: u32,
    list_started: bool,
    emitted_state: bool,
    draw: Option<Draw>,
    pub prims: u32,
    /// gl_Position array -> gl_PointSize array, from this job's vertex shader
    point_sizes: Vec<(u32, u32)>,
}

pub enum Outcome {
    Done,
    OutOfMemory,
}

impl Plbu {
    pub fn new(start: u32, end: u32, heap: (u32, u32), point_sizes: Vec<(u32, u32)>) -> Plbu {
        Plbu {
            point_sizes,
            st: State { scissor: [0, 4096, 0, 4096], low_prim_size: 1.0, depth_range: [0.0, 1.0], ..Default::default() },
            pc: start,
            end,
            cursor: heap.0,
            limit: heap.1,
            ..Default::default()
        }
    }

    /// Continue in a new heap area after an OUT_OF_MEM stop.
    pub fn grow(&mut self, vm: &mut Vm, heap: (u32, u32)) {
        if heap.0 != self.cursor {
            vm.write_u32(self.cursor, TAG_LINK);
            vm.write_u32(self.cursor + 4, heap.0);
        }
        self.cursor = heap.0;
        self.limit = heap.1;
    }

    fn room(&self, bytes: u32) -> bool {
        self.cursor + bytes + RESERVE <= self.limit
    }

    fn put(&mut self, vm: &mut Vm, rec: [u32; 4]) {
        for (k, w) in rec.iter().enumerate() {
            vm.write_u32(self.cursor + k as u32 * 4, *w);
        }
        self.cursor += 16;
    }

    /// Run until the command list ends or the heap is full.
    pub fn run(&mut self, vm: &mut Vm, log: &mut dyn FnMut(String)) -> Outcome {
        let mut guard = 0;
        loop {
            if let Some(d) = self.draw {
                if !self.emit_draw(vm, d, log) {
                    return Outcome::OutOfMemory;
                }
                self.draw = None;
            }
            if self.pc >= self.end || guard > 1 << 20 {
                break;
            }
            guard += 1;
            let (v1, v2) = (vm.read_u32(self.pc), vm.read_u32(self.pc + 4));
            self.pc += 8;
            if v2 & 0xffe0_0000 == 0 || v2 & 0xffe0_0000 == 0x0020_0000 {
                if (v1 == 0 && v2 == 0) || !self.list_started {
                    continue;
                }
                self.draw = Some(Draw {
                    indexed: v2 & 0x0020_0000 != 0,
                    count: (v1 >> 24) | (v2 & 0xffff) << 8,
                    first: v1 & 0x00ff_ffff,
                    mode: (v2 >> 16) & 0x1f,
                    next: 0,
                });
                continue;
            }
            let st = &mut self.st;
            match v2 >> 24 {
                0x10 => match v2 & 0xfff {
                    0x100 => st.indexed_gl_pos = v1,
                    0x101 => st.indices = v1,
                    0x102 => st.point_size = v1,
                    0x105 => st.viewport[2] = f32::from_bits(v1),
                    0x106 => st.viewport[3] = f32::from_bits(v1),
                    0x107 => st.viewport[0] = f32::from_bits(v1),
                    0x108 => st.viewport[1] = f32::from_bits(v1),
                    0x10e => st.depth_range[0] = f32::from_bits(v1),
                    0x10f => st.depth_range[1] = f32::from_bits(v1),
                    0x109 | 0x10a | 0x10c => {}
                    0x10b => {
                        st.cull = v1 & 0x0006_0000;
                        st.index_field = (v1 >> 9) & 7;
                        st.force_point_size = v1 & 0x1000 != 0;
                        self.emitted_state = false;
                    }
                    0x10d => {
                        st.low_prim_size = f32::from_bits(v1);
                        self.emitted_state = false;
                    }
                    _ => log(format!("[mali] PLBU: unknown command {v1:08x} {v2:08x}")),
                },
                0x28 => {
                    // the PLB block array: point every block at this job's list
                    let blocks = (v2 & 0x00ff_ffff) + 1;
                    for b in 0..blocks.min(1 << 16) {
                        let block = vm.read_u32(v1 + b * 4);
                        vm.write_u32(block, TAG_LINK);
                        vm.write_u32(block + 4, self.cursor);
                    }
                    self.list_started = true;
                    self.emitted_state = false;
                }
                0x30 | 0x60 => {}
                0x50 => {
                    self.pc = self.end;
                }
                0x70 => {
                    let minx = (v1 >> 30) | (v2 & 0x1fff) << 2;
                    let maxx = ((v2 >> 13) & 0x7fff) + 1;
                    let miny = v1 & 0x3fff;
                    let maxy = ((v1 >> 15) & 0x7fff) + 1;
                    st.scissor = [minx, maxx, miny, maxy];
                    self.emitted_state = false;
                }
                0x80..=0x8f => {
                    st.rsw = v1;
                    st.gl_pos = (v2 & 0x0fff_ffff) << 4;
                    self.emitted_state = false;
                }
                0xf0 => self.pc = v1,
                _ => log(format!("[mali] PLBU: unknown command {v1:08x} {v2:08x}")),
            }
        }
        if self.list_started {
            self.put(vm, [TAG_END, 0, 0, 0]);
        }
        Outcome::Done
    }

    /// Number of primitives of a draw and the vertices (draw-relative) of primitive `p`.
    fn prim_vertices(mode: u32, count: u32, p: u32) -> Option<(u32, [u32; 3])> {
        Some(match mode {
            0 => (KIND_POINT, [p, p, p]),
            1 => (KIND_LINE, [2 * p, 2 * p + 1, 2 * p + 1]),
            2 if p == count.saturating_sub(1) => (KIND_LINE, [p, 0, 0]),
            2 | 3 => (KIND_LINE, [p, p + 1, p + 1]),
            4 => (KIND_TRIANGLE, [3 * p, 3 * p + 1, 3 * p + 2]),
            5 if p & 1 == 0 => (KIND_TRIANGLE, [p, p + 1, p + 2]),
            5 => (KIND_TRIANGLE, [p + 1, p, p + 2]),
            6 => (KIND_TRIANGLE, [0, p + 1, p + 2]),
            0xf => (KIND_RECT, [3 * p, 3 * p + 1, 3 * p + 2]),
            _ => return None,
        })
    }

    fn prim_count(mode: u32, count: u32) -> u32 {
        match mode {
            0 => count,
            1 => count / 2,
            2 => if count > 2 { count } else { count.saturating_sub(1) },
            3 => count.saturating_sub(1),
            4 | 0xf => count / 3,
            5 | 6 => count.saturating_sub(2),
            _ => 0,
        }
    }

    /// Emit a draw's primitives from `d.next`; false if the heap filled up (the draw
    /// stays pending).
    fn emit_draw(&mut self, vm: &mut Vm, mut d: Draw, log: &mut dyn FnMut(String)) -> bool {
        let st = self.st;
        let gl_pos = if d.indexed { st.indexed_gl_pos } else { st.gl_pos };
        if Self::prim_vertices(d.mode, d.count, 0).is_none() {
            log(format!("[mali] PLBU: primitive mode {:#x} not modelled", d.mode));
            return true;
        }
        if !self.emitted_state {
            if !self.room(32) {
                self.draw = Some(d);
                return false;
            }
            // the shader's gl_PointSize: given for indexed draws, else where the vertex
            // shader wrote it for this gl_Position array
            let point_size = if st.force_point_size {
                0
            } else if d.indexed && st.point_size != 0 {
                st.point_size
            } else {
                self.point_sizes.iter().find(|(p, _)| *p == gl_pos).map_or(0, |(_, s)| *s)
            };
            self.put(vm, [TAG_STATE, st.rsw, gl_pos, point_size]);
            self.put(vm, [TAG_STATE2, st.scissor[0] | st.scissor[1] << 16, st.scissor[2] | st.scissor[3] << 16, st.low_prim_size.to_bits()]);
            self.emitted_state = true;
        }
        let vertex = |vm: &mut Vm, k: u32| -> u32 {
            if d.indexed {
                let index = match st.index_field >> 1 {
                    1 => vm.read_u16(st.indices + k * 2) as u32,
                    2 => vm.read_u32(st.indices + k * 4),
                    _ => vm.read_u8(st.indices + k) as u32,
                };
                index.wrapping_sub(d.first)
            } else {
                k
            }
        };
        let total = Self::prim_count(d.mode, d.count);
        while d.next < total {
            let (kind, rel) = Self::prim_vertices(d.mode, d.count, d.next).unwrap();
            let v = rel.map(|k| vertex(vm, k));
            if kind == KIND_TRIANGLE {
                let p = v.map(|i| pos(vm, gl_pos, i));
                if p.iter().all(|q| q[3] > 0.0 && q[3].is_finite()) {
                    if !culled(&p, st.cull) {
                        if !self.room(16) {
                            self.draw = Some(d);
                            return false;
                        }
                        self.put(vm, [TAG_PRIM | kind, v[0], v[1], v[2]]);
                        self.prims += 1;
                    }
                    crate::pp::spend_gp(8);
                } else if !self.clip(vm, &st, v, p) {
                    self.draw = Some(d);
                    return false;
                }
            } else {
                if !self.room(16) {
                    self.draw = Some(d);
                    return false;
                }
                self.put(vm, [TAG_PRIM | kind, v[0], v[1], v[2]]);
                self.prims += 1;
            }
            d.next += 1;
        }
        true
    }

    /// Clip a triangle with vertices behind the eye (1/w <= 0) against w = epsilon and
    /// emit the pieces as new vertices. False if the heap is full.
    fn clip(&mut self, vm: &mut Vm, st: &State, v: [u32; 3], p: [[f32; 4]; 3]) -> bool {
        // undo the viewport transform: clip = ((screen - translate) / scale) * w
        let sx = (st.viewport[1] - st.viewport[0]) / 2.0;
        let tx = (st.viewport[1] + st.viewport[0]) / 2.0;
        let sy = (st.viewport[3] - st.viewport[2]) / 2.0;
        let ty = (st.viewport[3] + st.viewport[2]) / 2.0;
        let sz = (st.depth_range[1] - st.depth_range[0]) / 2.0;
        let tz = (st.depth_range[1] + st.depth_range[0]) / 2.0;
        if sx == 0.0 || sy == 0.0 || p.iter().any(|q| !q.iter().all(|c| c.is_finite()) || q[3] == 0.0) {
            return true;
        }
        let clip: Vec<[f32; 4]> = p
            .iter()
            .map(|q| {
                let w = 1.0 / q[3];
                let z = if sz != 0.0 { (q[2] - tz) / sz } else { 0.0 };
                [(q[0] - tx) / sx * w, (q[1] - ty) / sy * w, z * w, w]
            })
            .collect();
        // the render state's varying layout, to interpolate the varyings
        let aux0 = vm.read_u32(st.rsw + 13 * 4);
        let types = vm.read_u32(st.rsw + 10 * 4);
        let vaddr = vm.read_u32(st.rsw + 15 * 4);
        let stride = (aux0 & 0x1f) * 8;
        let layout = crate::pp::varying_layout(aux0, types, vaddr);
        let vary: Vec<Vec<f32>> = v
            .iter()
            .map(|&i| crate::pp::read_varyings(vm, &layout, (vaddr & !0xf) + i * stride))
            .collect();
        // Sutherland-Hodgman against w >= eps
        const EPS: f32 = 1e-5;
        let mut poly: Vec<([f32; 4], Vec<f32>)> = Vec::new();
        for k in 0..3 {
            let (a, b) = (k, (k + 1) % 3);
            let (wa, wb) = (clip[a][3], clip[b][3]);
            if wa >= EPS {
                poly.push((clip[a], vary[a].clone()));
            }
            if (wa >= EPS) != (wb >= EPS) {
                let t = (EPS - wa) / (wb - wa);
                let c = [0, 1, 2, 3].map(|i| clip[a][i] + (clip[b][i] - clip[a][i]) * t);
                let va = vary[a].iter().zip(&vary[b]).map(|(x, y)| x + (y - x) * t).collect();
                poly.push((c, va));
            }
        }
        if poly.len() < 3 {
            return true;
        }
        // back to screen space
        let screen: Vec<[f32; 4]> = poly.iter().map(|(c, _)| [c[0] / c[3] * sx + tx, c[1] / c[3] * sy + ty, c[2] / c[3] * sz + tz, 1.0 / c[3]]).collect();
        let tris = poly.len() - 2;
        let vsize = 16 + layout.len() as u32 * 16;
        if !self.room(16 + poly.len() as u32 * vsize + tris as u32 * 16) {
            return false;
        }
        // the vertex data sits in the list: a LINK record skips it
        let link = self.cursor;
        self.cursor += 16;
        let mut addrs = Vec::new();
        for (k, (_, va)) in poly.iter().enumerate() {
            let a = self.cursor;
            for c in 0..4 {
                vm.write_u32(a + c * 4, screen[k][c as usize].to_bits());
            }
            for (i, x) in va.iter().enumerate() {
                vm.write_u32(a + 16 + i as u32 * 4, x.to_bits());
            }
            self.cursor += vsize;
            addrs.push(a);
        }
        vm.write_u32(link, TAG_LINK);
        vm.write_u32(link + 4, self.cursor);
        for k in 1..poly.len() - 1 {
            let tri = [screen[0], screen[k], screen[k + 1]];
            if culled(&tri, st.cull) {
                continue;
            }
            self.put(vm, [TAG_PRIM_ABS | KIND_TRIANGLE, addrs[0], addrs[k], addrs[k + 1]]);
            self.prims += 1;
        }
        true
    }
}

/// Screen position (x, y, z, 1/w) of a vertex.
fn pos(vm: &mut Vm, gl_pos: u32, v: u32) -> [f32; 4] {
    let a = gl_pos + v * 16;
    [vm.read_f32(a), vm.read_f32(a + 4), vm.read_f32(a + 8), vm.read_f32(a + 12)]
}

/// Back/front-face culling: 0x20000 culls clockwise triangles, 0x40000 counter-clockwise
/// ones (in framebuffer coordinates, y down); degenerate ones always go.
fn culled(p: &[[f32; 4]; 3], cull: u32) -> bool {
    let area = (p[1][0] - p[0][0]) * (p[2][1] - p[0][1]) - (p[2][0] - p[0][0]) * (p[1][1] - p[0][1]);
    if area == 0.0 || area.is_nan() {
        return true;
    }
    let ccw = area < 0.0;
    (ccw && cull & 0x40000 != 0) || (!ccw && cull & 0x20000 != 0)
}
