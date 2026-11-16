//! The geometry processor's vertex shader: its command list (VSCL) and the 128-bit VLIW
//! instruction set Mesa's `gpir` compiler emits.
//!
//! Each instruction feeds six units (two multipliers, two adders, a pass unit and a
//! "complex" unit for reciprocals, square roots, exp2/log2). Their operands come from the
//! loads of the same instruction (an attribute or register vec4, a second register vec4, a
//! uniform vec4) or from the unit results of the previous one or two instructions; there is
//! no general register write-back except through the store unit.

use crate::vm::Vm;

/// One decoded instruction.
#[derive(Clone, Copy, Default, Debug)]
pub struct Instr {
    mul0_src: [u8; 2],
    mul1_src: [u8; 2],
    mul0_neg: bool,
    mul1_neg: bool,
    acc0_src: [u8; 2],
    acc1_src: [u8; 2],
    acc0_neg: [bool; 2],
    acc1_neg: [bool; 2],
    load_addr: u16,
    load_offset: u8,
    reg0_addr: u8,
    reg0_attribute: bool,
    reg1_addr: u8,
    store_temporary: [bool; 2],
    branch: bool,
    branch_target_lo: bool,
    store_src: [u8; 4],
    acc_op: u8,
    complex_op: u8,
    store_addr: [u8; 2],
    store_varying: [bool; 2],
    mul_op: u8,
    pass_op: u8,
    complex_src: u8,
    pass_src: u8,
    branch_target: u16,
}

impl Instr {
    pub fn decode(words: [u32; 4]) -> Instr {
        let bits = words.iter().rev().fold(0u128, |acc, &w| acc << 32 | w as u128);
        let mut pos = 0;
        let mut take = |n: u32| {
            let v = (bits >> pos) as u32 & ((1u32 << n) - 1);
            pos += n;
            v
        };
        let mut i = Instr { mul0_src: [0; 2], ..Default::default() };
        i.mul0_src = [take(5) as u8, take(5) as u8];
        i.mul1_src = [take(5) as u8, take(5) as u8];
        i.mul0_neg = take(1) != 0;
        i.mul1_neg = take(1) != 0;
        i.acc0_src = [take(5) as u8, take(5) as u8];
        i.acc1_src = [take(5) as u8, take(5) as u8];
        i.acc0_neg = [take(1) != 0, take(1) != 0];
        i.acc1_neg = [take(1) != 0, take(1) != 0];
        i.load_addr = take(9) as u16;
        i.load_offset = take(3) as u8;
        i.reg0_addr = take(4) as u8;
        i.reg0_attribute = take(1) != 0;
        i.reg1_addr = take(4) as u8;
        i.store_temporary = [take(1) != 0, take(1) != 0];
        i.branch = take(1) != 0;
        i.branch_target_lo = take(1) != 0;
        i.store_src = [take(3) as u8, take(3) as u8, take(3) as u8, take(3) as u8];
        i.acc_op = take(3) as u8;
        i.complex_op = take(4) as u8;
        i.store_addr[0] = take(4) as u8;
        i.store_varying[0] = take(1) != 0;
        i.store_addr[1] = take(4) as u8;
        i.store_varying[1] = take(1) != 0;
        i.mul_op = take(3) as u8;
        i.pass_op = take(3) as u8;
        i.complex_src = take(5) as u8;
        i.pass_src = take(5) as u8;
        let _unknown = take(4);
        i.branch_target = take(8) as u16;
        i
    }
}

/// Unit results of one instruction: acc0, acc1, mul0, mul1, pass, complex.
#[derive(Clone, Copy, Default)]
struct Results {
    acc: [f32; 2],
    mul: [f32; 2],
    pass: f32,
    complex: f32,
    /// the vec4 the register-0 port loaded (an attribute or a register)
    reg0: [f32; 4],
}

const SRC_UNUSED: u8 = 21;
/// 22 is the identity (1 for a multiplier, 0 for an adder) as a second operand, and the
/// previous complex result anywhere else.
const SRC_IDENT: u8 = 22;

/// Vertex shader state for one draw.
pub struct VertexShader {
    pub code: Vec<Instr>,
    pub uniforms: Vec<[f32; 4]>,
    /// Temporary memory (register spills), as vec4s.
    temps: Vec<[f32; 4]>,
}

/// Per-vertex machine state.
struct Thread<'a> {
    attrs: &'a [[f32; 4]; 16],
    regs: [[f32; 4]; 16],
    out: [[f32; 4]; 16],
    written: u16,
    addr: [i32; 3],
    temp_store_addr: i32,
}

impl VertexShader {
    pub fn new(code: Vec<Instr>, uniforms: Vec<[f32; 4]>) -> Self {
        Self { code, uniforms, temps: Vec::new() }
    }

    /// Run the shader for one vertex: attributes in, outputs (vec4 per output slot) and the
    /// mask of written slots out.
    pub fn run(&mut self, attrs: &[[f32; 4]; 16]) -> ([[f32; 4]; 16], u16) {
        let mut t = Thread { attrs, regs: [[0.0; 4]; 16], out: [[0.0; 4]; 16], written: 0, addr: [0; 3], temp_store_addr: 0 };
        let mut p1 = Results::default();
        let mut p2 = Results::default();
        let mut pc = 0usize;
        let mut budget = 1 << 20;
        while pc < self.code.len() && budget > 0 {
            budget -= 1;
            let i = self.code[pc];
            let r = self.step(&i, &mut t, &p1, &p2);
            pc += 1;
            if i.branch && r.pass != 0.0 {
                pc = i.branch_target as usize + if i.branch_target_lo { 0 } else { 0x100 };
            }
            p2 = p1;
            p1 = r;
        }
        (t.out, t.written)
    }

    fn step(&mut self, i: &Instr, t: &mut Thread, p1: &Results, p2: &Results) -> Results {
        // this instruction's loads
        let reg0 = if i.reg0_attribute { t.attrs[i.reg0_addr as usize] } else { t.regs[i.reg0_addr as usize] };
        let reg1 = t.regs[i.reg1_addr as usize];
        let load = {
            let index = i.load_addr as i32
                + match i.load_offset {
                    1..=3 => t.addr[i.load_offset as usize - 1],
                    _ => 0,
                };
            if index >= 0 && (index as usize) < self.uniforms.len() {
                self.uniforms[index as usize]
            } else {
                let temp = index - self.uniforms.len() as i32;
                self.temps.get(temp.max(0) as usize).copied().unwrap_or([0.0; 4])
            }
        };
        let src = |s: u8| -> f32 {
            match s {
                0..=3 => reg0[s as usize],
                4..=7 => reg1[s as usize - 4],
                12..=15 => load[s as usize - 12],
                16 => p1.acc[0],
                17 => p1.acc[1],
                18 => p1.mul[0],
                19 => p1.mul[1],
                20 => p1.pass,
                SRC_IDENT => p1.complex,
                23 => p2.pass,
                24 => p2.acc[0],
                25 => p2.acc[1],
                26 => p2.mul[0],
                27 => p2.mul[1],
                28..=31 => p1.reg0[s as usize - 28],
                _ => 0.0,
            }
        };
        let mut r = Results { reg0, ..Default::default() };

        // multipliers
        match i.mul_op {
            0 | 3 => {
                for (u, (srcs, neg)) in [(i.mul0_src, i.mul0_neg), (i.mul1_src, i.mul1_neg)].into_iter().enumerate() {
                    if srcs[0] == SRC_UNUSED {
                        continue;
                    }
                    let a = src(srcs[0]);
                    let b = if srcs[1] == SRC_IDENT { 1.0 } else { src(srcs[1]) };
                    let v = if i.mul_op == 3 && u == 0 { a } else { a * b };
                    r.mul[u] = if neg { -v } else { v };
                }
            }
            // complex1: the final step of rcp/rsqrt/exp2/log2; the complex unit already
            // produced the exact result (its first operand)
            1 => r.mul[0] = src(i.mul0_src[0]),
            // select: mul0_src1 ? mul1_src0 : mul0_src0
            4 => {
                let cond = src(i.mul0_src[1]);
                r.mul[0] = if cond != 0.0 { src(i.mul1_src[0]) } else { src(i.mul0_src[0]) };
            }
            _ => {}
        }

        // adders (one operation for both)
        for (u, (srcs, neg)) in [(i.acc0_src, i.acc0_neg), (i.acc1_src, i.acc1_neg)].into_iter().enumerate() {
            if srcs[0] == SRC_UNUSED {
                continue;
            }
            let a = src(srcs[0]);
            let a = if neg[0] { -a } else { a };
            let b = if srcs[1] == SRC_IDENT { 0.0 } else { src(srcs[1]) };
            let b = if neg[1] { -b } else { b };
            r.acc[u] = match i.acc_op {
                0 => a + b,
                1 => a.floor(),
                2 => {
                    if a > 0.0 {
                        1.0
                    } else if a < 0.0 {
                        -1.0
                    } else {
                        0.0
                    }
                }
                4 => (a >= b) as u32 as f32,
                5 => (a < b) as u32 as f32,
                6 => a.min(b),
                7 => a.max(b),
                _ => 0.0,
            };
        }

        // complex unit
        if i.complex_src != SRC_UNUSED {
            let x = src(i.complex_src);
            r.complex = match i.complex_op {
                2 => x.exp2(),
                3 => x.log2(),
                4 => 1.0 / x.sqrt(),
                5 => 1.0 / x,
                12 => {
                    t.temp_store_addr = x as i32;
                    x
                }
                13..=15 => {
                    t.addr[i.complex_op as usize - 13] = x as i32;
                    x
                }
                _ => x,
            };
        }

        // pass unit
        if i.pass_src != SRC_UNUSED {
            let x = src(i.pass_src);
            r.pass = match i.pass_op {
                6 => x.max(load[0]).min(load[1]),
                _ => x,
            };
        }

        // stores: x/y of store0's vec4, z/w of store1's
        for c in 0..4 {
            let v = match i.store_src[c] {
                0 => r.acc[0],
                1 => r.acc[1],
                2 => r.mul[0],
                3 => r.mul[1],
                4 => r.pass,
                6 => r.complex,
                _ => continue,
            };
            let unit = c / 2;
            if i.store_temporary[unit] {
                let index = t.temp_store_addr.max(0) as usize;
                if self.temps.len() <= index {
                    self.temps.resize(index + 1, [0.0; 4]);
                }
                self.temps[index][c] = v;
            } else if i.store_varying[unit] {
                t.out[i.store_addr[unit] as usize][c] = v;
                t.written |= 1 << i.store_addr[unit];
            } else {
                t.regs[i.store_addr[unit] as usize][c] = v;
            }
        }
        r
    }
}

/// A vertex attribute or varying stream: base address and descriptor word
/// (stride << 11 | type << 2 | components - 1).
#[derive(Clone, Copy, Default, Debug)]
pub struct Stream {
    pub address: u32,
    pub desc: u32,
}

impl Stream {
    pub fn stride(&self) -> u32 {
        self.desc >> 11
    }
    pub fn kind(&self) -> u32 {
        (self.desc >> 2) & 0x1ff
    }
    pub fn components(&self) -> usize {
        (self.desc & 3) as usize + 1
    }
}

/// Read attribute `n` of a stream as a vec4 (missing components 0, 0, 0, 1).
pub fn load_attribute(vm: &mut Vm, s: &Stream, n: u32) -> [f32; 4] {
    let mut v = [0.0, 0.0, 0.0, 1.0];
    let base = s.address.wrapping_add(n.wrapping_mul(s.stride()));
    for c in 0..s.components() {
        let c32 = c as u32;
        v[c] = match s.kind() {
            0x000 => f32::from_bits(vm.read_u32(base + c32 * 4)),
            0x001 => vm.read_u32(base + c32 * 4) as i32 as f32,
            0x002 => vm.read_u32(base + c32 * 4) as f32,
            0x003 => crate::half::to_f32(vm.read_u16(base + c32 * 2)),
            0x004 => vm.read_u16(base + c32 * 2) as i16 as f32,
            0x005 => vm.read_u16(base + c32 * 2) as f32,
            0x006 => vm.read_u8(base + c32) as i8 as f32,
            0x007 => vm.read_u8(base + c32) as f32,
            0x008 => (vm.read_u8(base + c32) as i8 as f32 / 127.0).max(-1.0),
            0x009 => vm.read_u8(base + c32) as f32 / 255.0,
            0x00a => (vm.read_u16(base + c32 * 2) as i16 as f32 / 32767.0).max(-1.0),
            0x00b => vm.read_u16(base + c32 * 2) as f32 / 65535.0,
            0x00d => (vm.read_u32(base + c32 * 4) as i32 as f64 / 2147483647.0).max(-1.0) as f32,
            0x00e => (vm.read_u32(base + c32 * 4) as f64 / 4294967295.0) as f32,
            0x101 => vm.read_u32(base + c32 * 4) as i32 as f32 / 65536.0,
            _ => 0.0,
        };
    }
    v
}

/// Write output `n` of a varying stream.
pub fn store_varying(vm: &mut Vm, s: &Stream, n: u32, v: &[f32; 4]) {
    let base = s.address.wrapping_add(n.wrapping_mul(s.stride()));
    match s.kind() {
        // gl_Position (stride 16) and gl_PointSize (stride 4): 32-bit floats
        8 => {
            let count = (s.stride() / 4).clamp(1, 4) as usize;
            for (c, x) in v.iter().take(count).enumerate() {
                vm.write_u32(base + c as u32 * 4, x.to_bits());
            }
        }
        3 => {
            for c in 0..s.components() {
                vm.write_u16(base + c as u32 * 2, crate::half::from_f32(v[c]));
            }
        }
        _ => {
            for c in 0..s.components() {
                vm.write_u32(base + c as u32 * 4, v[c].to_bits());
            }
        }
    }
}

/// Execute a vertex shader command list.
pub fn run_vscl(vm: &mut Vm, start: u32, end: u32, log: &mut dyn FnMut(String)) {
    let mut shader: Option<VertexShader> = None;
    let (mut shader_addr, mut shader_size) = (0u32, 0u32);
    let (mut uniforms_addr, mut uniforms_size) = (0u32, 0u32);
    let (mut attr_addr, mut nattr) = (0u32, 0u32);
    let (mut vary_addr, mut nvary) = (0u32, 0u32);
    let mut pc = start;
    let mut guard = 0;
    while pc < end && guard < 1 << 16 {
        guard += 1;
        let (v1, v2) = (vm.read_u32(pc), vm.read_u32(pc + 4));
        pc += 8;
        match v2 >> 24 {
            0x00 if v2 & 0xffff_0000 == 0 => {
                if v1 == 0 && v2 == 0 {
                    continue;
                }
                let num = (v1 >> 24) | (v2 & 0xff) << 8;
                let sh = match &mut shader {
                    Some(s) => s,
                    None => {
                        let words = shader_size / 4;
                        let code = (0..words / 4)
                            .map(|k| {
                                let a = shader_addr + k * 16;
                                Instr::decode([vm.read_u32(a), vm.read_u32(a + 4), vm.read_u32(a + 8), vm.read_u32(a + 12)])
                            })
                            .collect();
                        let uniforms = (0..uniforms_size / 16)
                            .map(|k| {
                                let a = uniforms_addr + k * 16;
                                [0, 1, 2, 3].map(|c| f32::from_bits(vm.read_u32(a + c * 4)))
                            })
                            .collect();
                        shader.insert(VertexShader::new(code, uniforms))
                    }
                };
                let attrs: Vec<Stream> = (0..nattr).map(|k| Stream { address: vm.read_u32(attr_addr + k * 8), desc: vm.read_u32(attr_addr + k * 8 + 4) }).collect();
                let varys: Vec<Stream> = (0..nvary).map(|k| Stream { address: vm.read_u32(vary_addr + k * 8), desc: vm.read_u32(vary_addr + k * 8 + 4) }).collect();
                for n in 0..num {
                    let mut a = [[0.0, 0.0, 0.0, 1.0]; 16];
                    for (k, s) in attrs.iter().enumerate().take(16) {
                        a[k] = load_attribute(vm, s, n);
                    }
                    let (out, written) = sh.run(&a);
                    for (k, s) in varys.iter().enumerate().take(16) {
                        if written & (1 << k) != 0 {
                            store_varying(vm, s, n, &out[k]);
                        }
                    }
                }
            }
            0x10 => match v2 & 0xff {
                0x40 | 0x41 => {}
                0x42 => {
                    nvary = ((v1 & 0x00ff_ffff) >> 8) + 1;
                    nattr = (v1 >> 24) + 1;
                }
                _ => log(format!("[mali] VS: unknown command {v1:08x} {v2:08x}")),
            },
            0x20 => {
                let n = (v2 & 0x0fff_ffff) >> 17;
                if v2 & 0xff == 0x08 {
                    vary_addr = v1;
                    nvary = n;
                } else {
                    attr_addr = v1;
                    nattr = n;
                }
            }
            0x30 => {
                uniforms_addr = v1;
                uniforms_size = (v2 & 0x0fff_ffff) >> 12;
                shader = None;
            }
            0x40 => {
                shader_addr = v1;
                shader_size = (v2 & 0x0fff_ffff) >> 12;
                shader = None;
            }
            0x50 | 0x60 => {}
            0xf0 => pc = v1,
            _ => log(format!("[mali] VS: unknown command {v1:08x} {v2:08x}")),
        }
    }
}
