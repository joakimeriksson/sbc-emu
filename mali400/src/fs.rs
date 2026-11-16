//! The pixel processor's fragment shader: Mesa `ppir`'s variable-length VLIW
//! instructions.
//!
//! An instruction is a control word followed by the fields it uses, bit-packed in a
//! fixed order: varying load, texture sample, uniform load, vec4 multiply, scalar
//! multiply, vec4 add, scalar add, "combine" (rcp/sqrt/exp2/... and scalar*vector), temp
//! store / framebuffer read, branch, and two fp16 vec4 constants. The units run in that
//! order and later units see the registers earlier ones wrote; within an instruction the
//! constants, the texture result, the uniform and the two multiplier results are also
//! readable as pipeline registers.
//!
//! Fragments run as 2x2 quads in lock step (derivatives and texture LOD need the
//! neighbours); lanes that take different branches continue separately.

use crate::half;

pub const NREG: usize = 16;
const REG_CONST0: usize = 12;
const REG_CONST1: usize = 13;
const REG_TEXTURE: usize = 14;
const REG_UNIFORM: usize = 15;

#[derive(Clone, Copy, Debug, Default)]
pub struct VecSrc {
    reg: u8,
    swizzle: u8,
    abs: bool,
    neg: bool,
}

#[derive(Clone, Copy, Debug, Default)]
pub struct SclSrc {
    index: u8,
    abs: bool,
    neg: bool,
}

#[derive(Clone, Copy, Debug)]
pub enum Varying {
    /// load a varying (or gl_FragCoord / gl_FrontFacing / gl_PointCoord)
    Imm { perspective: u8, source_type: u8, alignment: u8, offset: Option<u8>, index: u8, dest: u8, mask: u8 },
    /// texture coordinates from a register
    Reg { perspective: u8, source_type: u8, normalize: bool, src: VecSrc, dest: u8, mask: u8 },
}

#[derive(Clone, Copy, Debug)]
pub struct Sampler {
    pub index: u16,
    pub offset: Option<u8>,
    pub lod_bias: Option<u8>,
    pub explicit_lod: bool,
    pub cube: bool,
}

#[derive(Clone, Copy, Debug)]
pub struct Uniform {
    temporary: bool,
    alignment: u8,
    offset: Option<u8>,
    index: i16,
}

#[derive(Clone, Copy, Debug)]
pub struct VecAlu {
    a: VecSrc,
    b: VecSrc,
    dest: u8,
    mask: u8,
    outmod: u8,
    op: u8,
    mul_in: bool,
}

#[derive(Clone, Copy, Debug)]
pub struct SclAlu {
    a: SclSrc,
    b: SclSrc,
    dest: u8,
    output: bool,
    outmod: u8,
    op: u8,
    mul_in: bool,
}

#[derive(Clone, Copy, Debug)]
pub enum Combine {
    Scalar { op: u8, a: SclSrc, outmod: u8, dest: u8 },
    /// scalar * vector
    Mul { a: SclSrc, b: VecSrc, mask: u8, dest: u8 },
}

#[derive(Clone, Copy, Debug)]
pub enum TempWrite {
    Store { source: u8, alignment: u8, offset: Option<u8>, index: i16 },
    FbRead { color: bool, dest: u8 },
}

#[derive(Clone, Copy, Debug)]
pub enum Branch {
    Discard,
    Jump { a: u8, b: u8, lt: bool, eq: bool, gt: bool, target: i32 },
}

#[derive(Clone, Debug, Default)]
pub struct Instr {
    /// size in words
    pub count: u32,
    pub stop: bool,
    pub varying: Option<Varying>,
    pub sampler: Option<Sampler>,
    pub uniform: Option<Uniform>,
    pub vmul: Option<VecAlu>,
    pub fmul: Option<SclAlu>,
    pub vacc: Option<VecAlu>,
    pub facc: Option<SclAlu>,
    pub combine: Option<Combine>,
    pub temp: Option<TempWrite>,
    pub branch: Option<Branch>,
    pub consts: [[f32; 4]; 2],
}

const FIELD_SIZE: [u32; 12] = [34, 62, 41, 43, 30, 44, 31, 30, 41, 73, 64, 64];

/// Bit reader over the words after the control word.
struct Bits<'a> {
    words: &'a [u32],
    pos: u32,
}

impl Bits<'_> {
    fn get(&self, offset: u32, n: u32) -> u64 {
        let mut v = 0u64;
        for i in 0..n {
            let b = self.pos + offset + i;
            let w = self.words.get((b / 32) as usize).copied().unwrap_or(0);
            v |= ((w >> (b % 32)) as u64 & 1) << i;
        }
        v
    }
}

fn vec_src(bits: u64, at: u32) -> VecSrc {
    VecSrc {
        reg: (bits >> at & 0xf) as u8,
        swizzle: (bits >> (at + 4) & 0xff) as u8,
        abs: bits >> (at + 12) & 1 != 0,
        neg: bits >> (at + 13) & 1 != 0,
    }
}

fn scl_src(bits: u64, at: u32) -> SclSrc {
    SclSrc { index: (bits >> at & 0x3f) as u8, abs: bits >> (at + 6) & 1 != 0, neg: bits >> (at + 7) & 1 != 0 }
}

impl Instr {
    /// Decode the instruction at the start of `words`.
    pub fn decode(words: &[u32]) -> Instr {
        let ctrl = words[0];
        let mut i = Instr {
            count: (ctrl & 0x1f).max(1),
            stop: ctrl >> 5 & 1 != 0,
            ..Default::default()
        };
        let fields = ctrl >> 7 & 0xfff;
        let mut r = Bits { words: &words[1..], pos: 0 };
        for f in 0..12 {
            if fields >> f & 1 == 0 {
                continue;
            }
            let n = FIELD_SIZE[f];
            match f {
                0 => {
                    let v = r.get(0, 34);
                    let source_type = (v >> 2 & 3) as u8;
                    let perspective = (v & 3) as u8;
                    let dest = (v >> 24 & 0xf) as u8;
                    let mask = (v >> 28 & 0xf) as u8;
                    // register sources: coordinates from a register (type 1) or cube /
                    // normalize of a register (type 2 with perspective 1 or 2)
                    let reg_form = source_type == 1 || (source_type == 2 && (perspective == 1 || perspective == 2));
                    i.varying = Some(if reg_form {
                        Varying::Reg {
                            perspective,
                            source_type,
                            normalize: v >> 6 & 1 != 0,
                            src: VecSrc {
                                reg: (v >> 10 & 0xf) as u8,
                                neg: v >> 14 & 1 != 0,
                                abs: v >> 15 & 1 != 0,
                                swizzle: (v >> 16 & 0xff) as u8,
                            },
                            dest,
                            mask,
                        }
                    } else {
                        let offset_vector = (v >> 10 & 0xf) as u8;
                        let offset_scalar = (v >> 16 & 3) as u8;
                        Varying::Imm {
                            perspective,
                            source_type,
                            alignment: (v >> 5 & 3) as u8,
                            offset: (offset_vector != 15).then_some(offset_vector << 2 | offset_scalar),
                            index: (v >> 18 & 0x3f) as u8,
                            dest,
                            mask,
                        }
                    });
                }
                1 => {
                    let v = r.get(0, 62);
                    i.sampler = Some(Sampler {
                        lod_bias: (v >> 18 & 1 != 0).then_some((v & 0x3f) as u8),
                        explicit_lod: v >> 17 & 1 != 0,
                        offset: (v >> 29 & 1 != 0).then_some((v >> 6 & 0x3f) as u8),
                        cube: (v >> 24 & 0x1f) == 0x1f,
                        index: (v >> 30 & 0xfff) as u16,
                    });
                }
                2 => {
                    let v = r.get(0, 41);
                    i.uniform = Some(Uniform {
                        temporary: v & 3 == 3,
                        alignment: (v >> 10 & 3) as u8,
                        offset: (v >> 24 & 1 != 0).then_some((v >> 18 & 0x3f) as u8),
                        index: (v >> 25 & 0xffff) as u16 as i16,
                    });
                }
                3 | 5 => {
                    let v = r.get(0, n);
                    let alu = VecAlu {
                        a: vec_src(v, 0),
                        b: vec_src(v, 14),
                        dest: (v >> 28 & 0xf) as u8,
                        mask: (v >> 32 & 0xf) as u8,
                        outmod: (v >> 36 & 3) as u8,
                        op: (v >> 38 & 0x1f) as u8,
                        mul_in: f == 5 && v >> 43 & 1 != 0,
                    };
                    if f == 3 {
                        i.vmul = Some(alu);
                    } else {
                        i.vacc = Some(alu);
                    }
                }
                4 | 6 => {
                    let v = r.get(0, n);
                    let alu = SclAlu {
                        a: scl_src(v, 0),
                        b: scl_src(v, 8),
                        dest: (v >> 16 & 0x3f) as u8,
                        output: v >> 22 & 1 != 0,
                        outmod: (v >> 23 & 3) as u8,
                        op: (v >> 25 & 0x1f) as u8,
                        mul_in: f == 6 && v >> 30 & 1 != 0,
                    };
                    if f == 4 {
                        i.fmul = Some(alu);
                    } else {
                        i.facc = Some(alu);
                    }
                }
                7 => {
                    let v = r.get(0, 30);
                    let dest_vec = v & 1 != 0;
                    let arg1_en = v >> 1 & 1 != 0;
                    let a = scl_src(v, 16);
                    let a = SclSrc { index: a.index, abs: v >> 14 & 1 != 0, neg: v >> 15 & 1 != 0 };
                    i.combine = Some(if dest_vec && arg1_en {
                        Combine::Mul {
                            a,
                            b: VecSrc { reg: (v >> 10 & 0xf) as u8, swizzle: (v >> 2 & 0xff) as u8, abs: false, neg: false },
                            mask: (v >> 22 & 0xf) as u8,
                            dest: (v >> 26 & 0xf) as u8,
                        }
                    } else {
                        Combine::Scalar { op: (v >> 2 & 0xf) as u8, a, outmod: (v >> 22 & 3) as u8, dest: (v >> 24 & 0x3f) as u8 }
                    });
                }
                8 => {
                    let v = r.get(0, 41);
                    i.temp = Some(if (v >> 1 & 0x1f) == 0x7 {
                        TempWrite::FbRead { color: v & 1 != 0, dest: (v >> 6 & 0xf) as u8 }
                    } else {
                        TempWrite::Store {
                            source: (v >> 4 & 0x3f) as u8,
                            alignment: (v >> 10 & 3) as u8,
                            offset: (v >> 24 & 1 != 0).then_some((v >> 18 & 0x3f) as u8),
                            index: (v >> 25 & 0xffff) as u16 as i16,
                        }
                    });
                }
                9 => {
                    let lo = r.get(0, 64);
                    let hi = r.get(64, 9);
                    i.branch = Some(if lo as u32 == 0x007f_0003 && (lo >> 32) == 0 && hi == 0 {
                        Branch::Discard
                    } else {
                        // 27 bits at 41..67: 23 from the first 64 bits, 4 from the rest
                        let target = ((lo >> 41) as u32 | (hi as u32 & 0xf) << 23) & 0x7ff_ffff;
                        Branch::Jump {
                            b: (lo >> 4 & 0x3f) as u8,
                            a: (lo >> 10 & 0x3f) as u8,
                            gt: lo >> 16 & 1 != 0,
                            eq: lo >> 17 & 1 != 0,
                            lt: lo >> 18 & 1 != 0,
                            target: ((target << 5) as i32) >> 5,
                        }
                    });
                }
                10 | 11 => {
                    let v = r.get(0, 64);
                    i.consts[f - 10] = [0, 1, 2, 3].map(|c| half::to_f32((v >> (16 * c)) as u16));
                }
                _ => {}
            }
            r.pos += n;
        }
        i
    }
}

/// A decoded fragment shader: instructions with their word offsets.
pub struct Program {
    pub instrs: Vec<Instr>,
    pub offsets: Vec<u32>,
}

impl Program {
    /// Decode from the instruction words (stops after the first `stop`, or at the end).
    pub fn decode(words: &[u32]) -> Program {
        let mut instrs = Vec::new();
        let mut offsets = Vec::new();
        let mut at = 0usize;
        while at < words.len() {
            let i = Instr::decode(&words[at..]);
            offsets.push(at as u32);
            at += i.count as usize;
            instrs.push(i);
            if instrs.len() > 4096 {
                break;
            }
        }
        Program { instrs, offsets }
    }

    fn index_of(&self, offset: u32) -> Option<usize> {
        self.offsets.binary_search(&offset).ok()
    }
}

/// What the shader reads from outside: varyings, uniforms, textures, the framebuffer.
pub trait Inputs {
    /// Varying component `index` (in scalar units) for a lane.
    fn varying(&self, lane: usize, index: u32) -> f32;
    fn frag_coord(&self, lane: usize) -> [f32; 4];
    fn front_facing(&self) -> bool;
    fn point_coord(&self, lane: usize) -> [f32; 2];
    fn uniform(&self, index: i32) -> [f32; 4];
    /// Sample texture `sampler` at the four lanes' coordinates (lod bias / explicit lod).
    fn sample(&self, sampler: u32, coords: &[[f32; 4]; 4], live: u8, bias: f32, explicit: Option<[f32; 4]>, cube: bool) -> [[f32; 4]; 4];
    fn fb_color(&self, lane: usize) -> [f32; 4];
    fn fb_depth(&self, lane: usize) -> f32;
}

/// Four fragments' state.
pub struct Quad {
    pub regs: [[[f32; 4]; NREG]; 4],
    /// lanes still running (not discarded)
    pub live: u8,
    /// lanes that executed `discard`
    pub killed: u8,
    temps: Vec<[[f32; 4]; 4]>,
}

impl Default for Quad {
    fn default() -> Self {
        Self { regs: [[[0.0; 4]; NREG]; 4], live: 0xf, killed: 0, temps: Vec::new() }
    }
}

fn outmod(v: f32, m: u8) -> f32 {
    match m {
        1 => v.clamp(0.0, 1.0),
        2 => v.max(0.0),
        3 => v.round(),
        _ => v,
    }
}

fn b2f(b: bool) -> f32 {
    b as u32 as f32
}

/// Per-lane pipeline registers of the current instruction.
#[derive(Clone, Copy, Default)]
struct Pipe {
    consts: [[f32; 4]; 2],
    texture: [f32; 4],
    uniform: [f32; 4],
    vmul: [f32; 4],
    fmul: f32,
}

impl Quad {
    fn vreg(&self, lane: usize, p: &Pipe, reg: u8) -> [f32; 4] {
        match reg as usize {
            REG_CONST0 => p.consts[0],
            REG_CONST1 => p.consts[1],
            REG_TEXTURE => p.texture,
            REG_UNIFORM => p.uniform,
            r => self.regs[lane][r],
        }
    }

    fn vsrc(&self, lane: usize, p: &Pipe, s: &VecSrc) -> [f32; 4] {
        let v = self.vreg(lane, p, s.reg);
        swz(v, s)
    }

    fn ssrc(&self, lane: usize, p: &Pipe, s: &SclSrc) -> f32 {
        let v = self.vreg(lane, p, s.index >> 2)[(s.index & 3) as usize];
        let v = if s.abs { v.abs() } else { v };
        if s.neg {
            -v
        } else {
            v
        }
    }

    fn sreg_raw(&self, lane: usize, p: &Pipe, index: u8) -> f32 {
        self.vreg(lane, p, index >> 2)[(index & 3) as usize]
    }

    fn write_scalar(&mut self, lane: usize, dest: u8, v: f32) {
        let r = (dest >> 2) as usize;
        if r < NREG {
            self.regs[lane][r][(dest & 3) as usize] = v;
        }
    }

    fn write_vec(&mut self, lane: usize, dest: u8, mask: u8, v: [f32; 4]) {
        let r = dest as usize;
        if r >= NREG {
            return;
        }
        for c in 0..4 {
            if mask >> c & 1 != 0 {
                self.regs[lane][r][c] = v[c];
            }
        }
    }

    /// Run `prog` from instruction `pc` for the lanes in `mask`.
    pub fn run(&mut self, prog: &Program, inputs: &dyn Inputs, pc: usize, mask: u8) {
        let mut pc = pc;
        let mut mask = mask & self.live;
        let mut budget = 100_000;
        while pc < prog.instrs.len() && mask != 0 && budget > 0 {
            budget -= 1;
            let i = &prog.instrs[pc];
            let mut pipes = [Pipe { consts: i.consts, ..Default::default() }; 4];
            let lanes = || (0..4).filter(move |l| mask >> l & 1 != 0);

            // 1. varying / coordinates
            let mut coords = [[0.0f32; 4]; 4];
            if let Some(v) = &i.varying {
                for l in lanes() {
                    let (value, dest, vmask) = self.varying(l, &pipes[l], v, inputs);
                    coords[l] = value;
                    if dest != 15 {
                        self.write_vec(l, dest, vmask, value);
                    }
                }
            }
            // 2. texture
            if let Some(s) = &i.sampler {
                let bias = s.lod_bias.map_or(0.0, |r| self.sreg_raw(lanes().next().unwrap_or(0), &pipes[0], r));
                let explicit = if s.explicit_lod {
                    let r = s.lod_bias.unwrap_or(0);
                    Some([0, 1, 2, 3].map(|l| self.sreg_raw(l, &pipes[l], r)))
                } else {
                    None
                };
                let base = s.index as u32 + s.offset.map_or(0, |r| self.sreg_raw(0, &pipes[0], r) as u32);
                let texels = inputs.sample(base, &coords, mask, if s.explicit_lod { 0.0 } else { bias }, explicit, s.cube);
                for l in lanes() {
                    pipes[l].texture = texels[l];
                }
            }
            // 3. uniform / temporary
            if let Some(u) = &i.uniform {
                for l in lanes() {
                    let mut index = u.index as i32;
                    if let Some(r) = u.offset {
                        index += self.sreg_raw(l, &pipes[l], r) as i32;
                    }
                    let vec_index = match u.alignment {
                        2 | 3 => index,
                        1 => index >> 1,
                        _ => index >> 2,
                    };
                    pipes[l].uniform = if u.temporary {
                        self.temps.get(vec_index.max(0) as usize).map_or([0.0; 4], |t| t[l])
                    } else {
                        inputs.uniform(vec_index)
                    };
                }
            }
            // 4. vec4 multiply
            if let Some(a) = &i.vmul {
                for l in lanes() {
                    let x = self.vsrc(l, &pipes[l], &a.a);
                    let y = self.vsrc(l, &pipes[l], &a.b);
                    let r = [0, 1, 2, 3].map(|c| outmod(mul_op(a.op, x[c], y[c]), a.outmod));
                    pipes[l].vmul = r;
                    self.write_vec(l, a.dest, a.mask, r);
                }
            }
            // 5. scalar multiply
            if let Some(a) = &i.fmul {
                for l in lanes() {
                    let x = self.ssrc(l, &pipes[l], &a.a);
                    let y = self.ssrc(l, &pipes[l], &a.b);
                    let r = outmod(mul_op(a.op, x, y), a.outmod);
                    pipes[l].fmul = r;
                    if a.output {
                        self.write_scalar(l, a.dest, r);
                    }
                }
            }
            // 6. vec4 add
            if let Some(a) = &i.vacc {
                let mut xs = [[0.0f32; 4]; 4];
                for l in 0..4 {
                    xs[l] = if a.mul_in { swz(pipes[l].vmul, &a.a) } else { self.vsrc(l, &pipes[l], &a.a) };
                }
                let ys = [0, 1, 2, 3].map(|l| self.vsrc(l, &pipes[l], &a.b));
                for l in lanes() {
                    let (x, y) = (xs[l], ys[l]);
                    let r = match a.op {
                        0x10 => [x[0] + x[1] + x[2]; 4],
                        0x11 => [x[0] + x[1] + x[2] + x[3]; 4],
                        // ppir_lower_ddxy gives both ops two operands, (v, -v) for dFdx
                        // and (-v, v) for dFdy: the un-negated copy is arg0 resp. arg1
                        0x14 => [0, 1, 2, 3].map(|c| deriv(&xs, l, c, true)),
                        0x15 => [0, 1, 2, 3].map(|c| deriv(&ys, l, c, false)),
                        0x17 => {
                            if pipes[l].fmul != 0.0 {
                                x
                            } else {
                                y
                            }
                        }
                        op => [0, 1, 2, 3].map(|c| acc_op(op, x[c], y[c])),
                    }
                    .map(|v| outmod(v, a.outmod));
                    self.write_vec(l, a.dest, a.mask, r);
                }
            }
            // 7. scalar add
            if let Some(a) = &i.facc {
                let xs = [0, 1, 2, 3].map(|l| if a.mul_in { apply(pipes[l].fmul, &a.a) } else { self.ssrc(l, &pipes[l], &a.a) });
                let ys = [0, 1, 2, 3].map(|l| self.ssrc(l, &pipes[l], &a.b));
                for l in lanes() {
                    let r = match a.op {
                        0x14 => deriv_s(&xs, l, true),
                        0x15 => deriv_s(&ys, l, false),
                        0x17 => {
                            if pipes[l].fmul != 0.0 {
                                xs[l]
                            } else {
                                ys[l]
                            }
                        }
                        op => acc_op(op, xs[l], ys[l]),
                    };
                    self.write_scalar(l, a.dest, outmod(r, a.outmod));
                }
            }
            // 8. combine
            if let Some(c) = &i.combine {
                for l in lanes() {
                    match c {
                        Combine::Scalar { op, a, outmod: m, dest } => {
                            let x = self.ssrc(l, &pipes[l], a);
                            let r = match op {
                                0 => 1.0 / x,
                                1 => x,
                                2 => x.sqrt(),
                                3 => 1.0 / x.sqrt(),
                                4 => x.exp2(),
                                5 => x.log2(),
                                6 => (x * std::f32::consts::TAU).sin(),
                                7 => (x * std::f32::consts::TAU).cos(),
                                _ => x,
                            };
                            self.write_scalar(l, *dest, outmod(r, *m));
                        }
                        Combine::Mul { a, b, mask: m, dest } => {
                            let s = self.ssrc(l, &pipes[l], a);
                            let v = self.vsrc(l, &pipes[l], b);
                            self.write_vec(l, *dest, *m, v.map(|x| s * x));
                        }
                    }
                }
            }
            // 9. temporary store / framebuffer read
            if let Some(t) = &i.temp {
                for l in lanes() {
                    match *t {
                        TempWrite::FbRead { color, dest } => {
                            let v = if color { inputs.fb_color(l) } else { [inputs.fb_depth(l); 4] };
                            self.write_vec(l, dest, 0xf, v);
                        }
                        TempWrite::Store { source, alignment, offset, index } => {
                            let mut index = index as i32;
                            if let Some(r) = offset {
                                index += self.sreg_raw(l, &pipes[l], r) as i32;
                            }
                            let (vec_index, comps): (i32, &[usize]) = match alignment {
                                2 | 3 => (index, &[0, 1, 2, 3]),
                                1 => (index >> 1, if index & 1 != 0 { &[2, 3] } else { &[0, 1] }),
                                _ => (index >> 2, std::slice::from_ref(&[0usize, 1, 2, 3][(index & 3) as usize])),
                            };
                            let vec_index = vec_index.max(0) as usize;
                            if self.temps.len() <= vec_index {
                                self.temps.resize(vec_index + 1, [[0.0; 4]; 4]);
                            }
                            if alignment == 0 {
                                let v = self.sreg_raw(l, &pipes[l], source);
                                self.temps[vec_index][l][comps[0]] = v;
                            } else {
                                let v = self.vreg(l, &pipes[l], source >> 2);
                                for &c in comps {
                                    self.temps[vec_index][l][c] = v[c];
                                }
                            }
                        }
                    }
                }
            }
            // 10. branch
            let mut next = pc + 1;
            if let Some(b) = &i.branch {
                match *b {
                    Branch::Discard => {
                        self.killed |= mask;
                        self.live &= !mask;
                        return;
                    }
                    Branch::Jump { a, b, lt, eq, gt, target } => {
                        let mut taken = 0u8;
                        for l in lanes() {
                            let x = self.sreg_raw(l, &pipes[l], a);
                            let y = self.sreg_raw(l, &pipes[l], b);
                            let t = (lt && x < y) || (eq && x == y) || (gt && x > y);
                            if t {
                                taken |= 1 << l;
                            }
                        }
                        let dest = prog.index_of((prog.offsets[pc] as i32 + target) as u32);
                        if taken != 0 {
                            match dest {
                                Some(d) if taken == mask => next = d,
                                Some(d) => {
                                    self.run(prog, inputs, d, taken);
                                    mask &= !taken;
                                }
                                None => mask &= !taken,
                            }
                        }
                    }
                }
            }
            if i.stop {
                return;
            }
            pc = next;
        }
    }

    fn varying(&self, l: usize, p: &Pipe, v: &Varying, inputs: &dyn Inputs) -> ([f32; 4], u8, u8) {
        match *v {
            Varying::Imm { perspective, source_type, alignment, offset, index, dest, mask } => {
                let value = match source_type {
                    2 if perspective == 3 => inputs.frag_coord(l),
                    3 if perspective != 0 => [b2f(inputs.front_facing()); 4],
                    3 => {
                        let pc = inputs.point_coord(l);
                        [pc[0], pc[1], 0.0, 1.0]
                    }
                    _ => {
                        let off = offset.map_or(0, |r| self.sreg_raw(l, p, r) as i32);
                        let (first, n) = match alignment {
                            0 => (index as i32 + off, 1),
                            1 => ((index as i32 + off) * 2, 2),
                            _ => ((index as i32 + off) * 4, 4),
                        };
                        let mut out = [0.0f32; 4];
                        // place the loaded components into the masked lanes, in order
                        let mut k = 0;
                        for c in 0..4 {
                            if dest == 15 || mask >> c & 1 != 0 {
                                if k < n {
                                    out[c] = inputs.varying(l, (first + k as i32).max(0) as u32);
                                }
                                k += 1;
                            }
                        }
                        if dest == 15 && n < 4 {
                            // coordinates for the sampler: the loaded components first
                            let mut o = [0.0, 0.0, 0.0, 1.0];
                            for k in 0..n {
                                o[k] = inputs.varying(l, (first + k as i32).max(0) as u32);
                            }
                            out = o;
                        }
                        out = project(out, perspective);
                        return (out, dest, mask);
                    }
                };
                (value, dest, mask)
            }
            Varying::Reg { perspective, source_type, normalize: _, src, dest, mask } => {
                let mut v = self.vsrc(l, p, &src);
                if source_type == 2 && perspective == 2 {
                    let len = (v[0] * v[0] + v[1] * v[1] + v[2] * v[2]).sqrt();
                    if len > 0.0 {
                        v = [v[0] / len, v[1] / len, v[2] / len, v[3]];
                    }
                } else if source_type == 1 {
                    v = project(v, perspective);
                }
                (v, dest, mask)
            }
        }
    }
}

fn project(v: [f32; 4], perspective: u8) -> [f32; 4] {
    match perspective {
        2 => [v[0] / v[2], v[1] / v[2], 1.0, v[3]],
        3 => [v[0] / v[3], v[1] / v[3], v[2] / v[3], 1.0],
        _ => v,
    }
}

fn swz(v: [f32; 4], s: &VecSrc) -> [f32; 4] {
    [0, 1, 2, 3].map(|c| {
        let x = v[(s.swizzle >> (2 * c) & 3) as usize];
        let x = if s.abs { x.abs() } else { x };
        if s.neg {
            -x
        } else {
            x
        }
    })
}

fn apply(x: f32, s: &SclSrc) -> f32 {
    let x = if s.abs { x.abs() } else { x };
    if s.neg {
        -x
    } else {
        x
    }
}

fn mul_op(op: u8, x: f32, y: f32) -> f32 {
    match op {
        0..=7 => {
            let shift = if op < 4 { op as i32 } else { op as i32 - 8 };
            x * y * (shift as f32).exp2()
        }
        0x08 => b2f(x == 0.0),
        0x09 => b2f(x != 0.0 && y != 0.0),
        0x0a => b2f(x != 0.0 || y != 0.0),
        0x0b => b2f((x != 0.0) != (y != 0.0)),
        0x0c => b2f(x != y),
        0x0d => b2f(x > y),
        0x0e => b2f(x >= y),
        0x0f => b2f(x == y),
        0x10 => x.min(y),
        0x11 => x.max(y),
        _ => x,
    }
}

fn acc_op(op: u8, x: f32, y: f32) -> f32 {
    match op {
        0..=3 | 5..=7 => {
            let shift = if op < 4 { op as i32 } else { op as i32 - 8 };
            (x + y) * (shift as f32).exp2()
        }
        0x04 => x - x.floor(),
        0x08 => b2f(x != y),
        0x09 => b2f(x > y),
        0x0a => b2f(x >= y),
        0x0b => b2f(x == y),
        0x0c => x.floor(),
        0x0d => x.ceil(),
        0x0e => x.min(y),
        0x0f => x.max(y),
        _ => x,
    }
}

/// dFdx / dFdy of component `c` across the quad (lanes: 0 1 / 2 3).
fn deriv(v: &[[f32; 4]; 4], lane: usize, c: usize, x: bool) -> f32 {
    if x {
        let row = lane & 2;
        v[row + 1][c] - v[row][c]
    } else {
        let col = lane & 1;
        v[col + 2][c] - v[col][c]
    }
}

fn deriv_s(v: &[f32; 4], lane: usize, x: bool) -> f32 {
    if x {
        let row = lane & 2;
        v[row + 1] - v[row]
    } else {
        let col = lane & 1;
        v[col + 2] - v[col]
    }
}
