//! The PLBU ("polygon list builder unit"): the GP's second command list. It assembles
//! primitives from the vertex shader's output positions, culls them, and writes the
//! polygon lists the pixel processors read.
//!
//! The list format between the PLBU and the PPs is internal to the GPU, so the model uses
//! its own: one list per GP job, linked from every PLB block the driver allocated (the
//! PP stream points at those blocks). Records are four words:
//!
//! | word 0 | words 1-3 |
//! |---|---|
//! | `STATE` | RSW address, gl_Position array, gl_PointSize array |
//! | `STATE2` | scissor (minx, maxx), (miny, maxy), point size / line width |
//! | `PRIM` \| kind | up to three vertex indices |
//! | `LINK` | address of the next record |
//! | `END` | |

use crate::vm::Vm;

pub const TAG_STATE: u32 = 0xa100_0000;
pub const TAG_STATE2: u32 = 0xa200_0000;
pub const TAG_PRIM: u32 = 0xa300_0000;
pub const TAG_LINK: u32 = 0xae00_0000;
pub const TAG_END: u32 = 0xaf00_0000;
pub const TAG_MASK: u32 = 0xff00_0000;

pub const KIND_POINT: u32 = 0;
pub const KIND_LINE: u32 = 1;
pub const KIND_TRIANGLE: u32 = 2;
/// three corners of a parallelogram (Mesa's blits)
pub const KIND_RECT: u32 = 3;

struct Writer {
    cursor: u32,
    end: u32,
    full: bool,
}

impl Writer {
    fn put(&mut self, vm: &mut Vm, rec: [u32; 4]) {
        // keep room for the END record
        if self.cursor + 32 > self.end {
            self.full = true;
            return;
        }
        for (k, w) in rec.iter().enumerate() {
            vm.write_u32(self.cursor + k as u32 * 4, *w);
        }
        self.cursor += 16;
    }
}

#[derive(Default, Clone, Copy)]
struct State {
    viewport: [f32; 4], // left, right, bottom, top
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

/// Run a PLBU command list; `heap` is the allocation area (start, end) for the lists.
/// Returns the number of primitives written.
pub fn run(vm: &mut Vm, start: u32, end: u32, heap: (u32, u32), log: &mut dyn FnMut(String)) -> u32 {
    let mut st = State { scissor: [0, 4096, 0, 4096], low_prim_size: 1.0, ..Default::default() };
    let mut w = Writer { cursor: heap.0, end: heap.1, full: false };
    let mut list_started = false;
    let mut emitted_state = false;
    let mut prims = 0;
    let mut pc = start;
    let mut guard = 0;
    while pc < end && guard < 1 << 20 {
        guard += 1;
        let (v1, v2) = (vm.read_u32(pc), vm.read_u32(pc + 4));
        pc += 8;
        if v2 & 0xffe0_0000 == 0 || v2 & 0xffe0_0000 == 0x0020_0000 {
            if v1 == 0 && v2 == 0 {
                continue;
            }
            let indexed = v2 & 0x0020_0000 != 0;
            let count = (v1 >> 24) | (v2 & 0xff) << 8;
            let first = v1 & 0x00ff_ffff;
            let mode = (v2 >> 16) & 0x1f;
            if !list_started {
                continue;
            }
            if !emitted_state {
                let gl_pos = if indexed { st.indexed_gl_pos } else { st.gl_pos };
                w.put(vm, [TAG_STATE, st.rsw, gl_pos, if st.force_point_size { 0 } else { st.point_size }]);
                w.put(vm, [TAG_STATE2, st.scissor[0] | st.scissor[1] << 16, st.scissor[2] | st.scissor[3] << 16, st.low_prim_size.to_bits()]);
                emitted_state = true;
            }
            let vertex = |vm: &mut Vm, k: u32| -> u32 {
                if indexed {
                    let index = match st.index_field >> 1 {
                        1 => vm.read_u16(st.indices + k * 2) as u32,
                        2 => vm.read_u32(st.indices + k * 4),
                        _ => vm.read_u8(st.indices + k) as u32,
                    };
                    index.wrapping_sub(first)
                } else {
                    k
                }
            };
            let gl_pos = if indexed { st.indexed_gl_pos } else { st.gl_pos };
            let mut emit = |vm: &mut Vm, w: &mut Writer, kind: u32, v: [u32; 3]| {
                if kind == KIND_TRIANGLE && culled(vm, gl_pos, v, st.cull) {
                    return;
                }
                w.put(vm, [TAG_PRIM | kind, v[0], v[1], v[2]]);
                prims += 1;
            };
            match mode {
                0 => {
                    for k in 0..count {
                        let a = vertex(vm, k);
                        emit(vm, &mut w, KIND_POINT, [a, a, a]);
                    }
                }
                1 => {
                    for k in (0..count.saturating_sub(1)).step_by(2) {
                        let (a, b) = (vertex(vm, k), vertex(vm, k + 1));
                        emit(vm, &mut w, KIND_LINE, [a, b, b]);
                    }
                }
                2 | 3 => {
                    for k in 0..count.saturating_sub(1) {
                        let (a, b) = (vertex(vm, k), vertex(vm, k + 1));
                        emit(vm, &mut w, KIND_LINE, [a, b, b]);
                    }
                    if mode == 2 && count > 2 {
                        let (a, b) = (vertex(vm, count - 1), vertex(vm, 0));
                        emit(vm, &mut w, KIND_LINE, [a, b, b]);
                    }
                }
                4 => {
                    for k in (0..count.saturating_sub(2)).step_by(3) {
                        let v = [vertex(vm, k), vertex(vm, k + 1), vertex(vm, k + 2)];
                        emit(vm, &mut w, KIND_TRIANGLE, v);
                    }
                }
                5 => {
                    for k in 0..count.saturating_sub(2) {
                        let (a, b, c) = (vertex(vm, k), vertex(vm, k + 1), vertex(vm, k + 2));
                        let v = if k & 1 == 0 { [a, b, c] } else { [b, a, c] };
                        emit(vm, &mut w, KIND_TRIANGLE, v);
                    }
                }
                6 => {
                    let a = vertex(vm, 0);
                    for k in 1..count.saturating_sub(1) {
                        let v = [a, vertex(vm, k), vertex(vm, k + 1)];
                        emit(vm, &mut w, KIND_TRIANGLE, v);
                    }
                }
                0xf => {
                    for k in (0..count.saturating_sub(2)).step_by(3) {
                        let v = [vertex(vm, k), vertex(vm, k + 1), vertex(vm, k + 2)];
                        emit(vm, &mut w, KIND_RECT, v);
                    }
                }
                _ => log(format!("[mali] PLBU: primitive mode {mode:#x} not modelled")),
            }
            continue;
        }
        match v2 >> 24 {
            0x10 => match v2 & 0xfff {
                0x100 => st.indexed_gl_pos = v1,
                0x101 => st.indices = v1,
                0x102 => st.point_size = v1,
                0x105 => st.viewport[2] = f32::from_bits(v1),
                0x106 => st.viewport[3] = f32::from_bits(v1),
                0x107 => st.viewport[0] = f32::from_bits(v1),
                0x108 => st.viewport[1] = f32::from_bits(v1),
                0x109 | 0x10a | 0x10c | 0x10e | 0x10f => {}
                0x10b => {
                    st.cull = v1 & 0x0006_0000;
                    st.index_field = (v1 >> 9) & 7;
                    st.force_point_size = v1 & 0x1000 != 0;
                    emitted_state = false;
                }
                0x10d => {
                    st.low_prim_size = f32::from_bits(v1);
                    emitted_state = false;
                }
                _ => log(format!("[mali] PLBU: unknown command {v1:08x} {v2:08x}")),
            },
            0x28 => {
                // the PLB block array: point every block at this job's list
                let blocks = (v2 & 0x00ff_ffff) + 1;
                for b in 0..blocks.min(1 << 16) {
                    let block = vm.read_u32(v1 + b * 4);
                    vm.write_u32(block, TAG_LINK);
                    vm.write_u32(block + 4, heap.0);
                }
                list_started = true;
                emitted_state = false;
            }
            0x30 | 0x60 => {}
            0x50 => break,
            0x70 => {
                let minx = (v1 >> 30) | (v2 & 0x1fff) << 2;
                let maxx = ((v2 >> 13) & 0x7fff) + 1;
                let miny = v1 & 0x3fff;
                let maxy = ((v1 >> 15) & 0x7fff) + 1;
                st.scissor = [minx, maxx, miny, maxy];
                emitted_state = false;
            }
            0x80..=0x8f => {
                st.rsw = v1;
                st.gl_pos = (v2 & 0x0fff_ffff) << 4;
                emitted_state = false;
            }
            0xf0 => pc = v1,
            _ => log(format!("[mali] PLBU: unknown command {v1:08x} {v2:08x}")),
        }
    }
    if list_started {
        w.end += 16;
        w.put(vm, [TAG_END, 0, 0, 0]);
    }
    if w.full {
        log(format!("[mali] PLBU: polygon list heap full ({} bytes), primitives dropped", heap.1 - heap.0));
    }
    prims
}

/// Screen position (x, y) of a vertex.
fn pos(vm: &mut Vm, gl_pos: u32, v: u32) -> (f32, f32, f32) {
    let a = gl_pos + v * 16;
    (vm.read_f32(a), vm.read_f32(a + 4), vm.read_f32(a + 12))
}

/// Back/front-face culling: 0x20000 culls clockwise triangles, 0x40000 counter-clockwise
/// ones (in framebuffer coordinates, y down); degenerate ones always go.
fn culled(vm: &mut Vm, gl_pos: u32, v: [u32; 3], cull: u32) -> bool {
    let (x0, y0, w0) = pos(vm, gl_pos, v[0]);
    let (x1, y1, w1) = pos(vm, gl_pos, v[1]);
    let (x2, y2, w2) = pos(vm, gl_pos, v[2]);
    if !(w0 > 0.0 && w1 > 0.0 && w2 > 0.0) {
        return true;
    }
    let area = (x1 - x0) * (y2 - y0) - (x2 - x0) * (y1 - y0);
    if area == 0.0 || area.is_nan() {
        return true;
    }
    let ccw = area < 0.0;
    (ccw && cull & 0x40000 != 0) || (!ccw && cull & 0x20000 != 0)
}
