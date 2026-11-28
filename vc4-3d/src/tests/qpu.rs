//! QPU instruction semantics: every ALU operation, conditions and flags, packing and unpacking,
//! small and load immediates, uniforms, the SFU, branches, the TMU, the VPM and its DMA,
//! semaphores.

use crate::asm::*;
use crate::mem::Mem;
use crate::qpu::{self, op, raddr, sig, waddr, Code, Env, Qpu, Stop, V, ZERO};

const CODE: u32 = 0x1000;
const UNIF: u32 = 0x8000;

struct NoEnv { vpm: Vec<u32> }
impl Env for NoEnv {
    fn vpm(&mut self) -> (&mut [u32], usize, usize) { (&mut self.vpm, 0, 0) }
}

/// Run `prog` (an end sequence is appended) with uniforms `unif`; `init` sets registers.
fn run_with(prog: &[I], unif: &[u32], ram: &mut [u8], init: impl FnOnce(&mut Qpu)) -> Qpu {
    let mut p = prog.to_vec();
    p.extend_from_slice(&end());
    let b = bytes(&p);
    ram[CODE as usize..CODE as usize + b.len()].copy_from_slice(&b);
    for (k, u) in unif.iter().enumerate() { ram[UNIF as usize + 4 * k..UNIF as usize + 4 * k + 4].copy_from_slice(&u.to_le_bytes()); }
    let mut m = Mem::new(ram);
    let mut q = Qpu::new();
    init(&mut q);
    q.start(CODE, UNIF);
    let mut env = NoEnv { vpm: vec![0; 192 * 16] };
    let mut code = Code::default();
    assert_eq!(q.run(&mut code, &mut m, &mut env, 10_000), Stop::End);
    q
}

fn run(prog: &[I], unif: &[u32], init: impl FnOnce(&mut Qpu)) -> Qpu {
    let mut ram = vec![0u8; 0x10_0000];
    run_with(prog, unif, &mut ram, init)
}

fn lanes(f: impl Fn(usize) -> u32) -> V { std::array::from_fn(f) }
fn fl(x: f32) -> u32 { x.to_bits() }

const A_VALS: [u32; 16] = [0, 1, 2, 0x8000_0000, 0xffff_ffff, 7, 0x1234_5678, 31, 32, 0x7fff_ffff, 0x80, 0xff00_ff00, 100, 5, 0x0102_0304, 0xfefd_fcfb];
const B_VALS: [u32; 16] = [0, 1, 3, 1, 1, 33, 4, 0xffff_ffff, 1, 1, 0x80, 0x00ff_00ff, 200, 0xffff_fffb, 0x0405_0607, 0x0102_0304];

#[test]
fn integer_add_ops() {
    let ops: [(u8, fn(u32, u32) -> u32); 15] = [
        (op::ADD, |a, b| a.wrapping_add(b)), (op::SUB, |a, b| a.wrapping_sub(b)), (op::SHR, |a, b| a >> (b & 31)),
        (op::ASR, |a, b| ((a as i32) >> (b & 31)) as u32), (op::ROR, |a, b| a.rotate_right(b & 31)), (op::SHL, |a, b| a << (b & 31)),
        (op::MIN, |a, b| (a as i32).min(b as i32) as u32), (op::MAX, |a, b| (a as i32).max(b as i32) as u32),
        (op::AND, |a, b| a & b), (op::OR, |a, b| a | b), (op::XOR, |a, b| a ^ b), (op::NOT, |a, _| !a), (op::CLZ, |a, _| a.leading_zeros()),
        (op::A_V8ADDS, |a, b| { let (x, y) = (a.to_le_bytes(), b.to_le_bytes()); u32::from_le_bytes(std::array::from_fn(|k| x[k].saturating_add(y[k]))) }),
        (op::A_V8SUBS, |a, b| { let (x, y) = (a.to_le_bytes(), b.to_le_bytes()); u32::from_le_bytes(std::array::from_fn(|k| x[k].saturating_sub(y[k]))) }),
    ];
    for (o, f) in ops {
        let q = run(&[nop().add(o, 5, A, B).ra(1).rb(2)], &[], |q| { q.ra[1] = A_VALS; q.rb[2] = B_VALS; });
        for i in 0..16 { assert_eq!(q.ra[5][i], f(A_VALS[i], B_VALS[i]), "op {o} lane {i}"); }
    }
}

#[test]
fn float_add_ops() {
    let a: [f32; 16] = [1.0, -2.5, 0.0, -0.0, 3.0, 1e30, -1e30, 0.5, 7.75, -7.75, 100.0, 1.0e-40, 2.0, -3.0, 16.0, 1.5];
    let b: [f32; 16] = [2.0, 2.5, -0.0, 0.0, -4.0, 1e30, 1e30, -0.25, 0.25, 0.25, 0.001, 1.0, -2.0, 3.0, 4.0, 1.5];
    let flush = |x: f32| if x.is_subnormal() { 0.0f32.copysign(x) } else { x };
    let ops: [(u8, fn(f32, f32) -> f32); 6] = [(op::FADD, |a, b| a + b), (op::FSUB, |a, b| a - b), (op::FMIN, |a, b| a.min(b)), (op::FMAX, |a, b| a.max(b)),
                                               (op::FMINABS, |a, b| a.abs().min(b.abs())), (op::FMAXABS, |a, b| a.abs().max(b.abs()))];
    for (o, f) in ops {
        let q = run(&[nop().add(o, 5, A, B).ra(1).rb(2)], &[], |q| { q.ra[1] = lanes(|i| fl(a[i])); q.rb[2] = lanes(|i| fl(b[i])); });
        for i in 0..16 { assert_eq!(f32::from_bits(q.ra[5][i]), flush(f(flush(a[i]), flush(b[i]))), "op {o} lane {i}"); }
    }
    // conversions
    let q = run(&[nop().add(op::FTOI, 5, A, A).ra(1), nop().add(op::ITOF, 6, B, B).rb(2)], &[], |q| {
        q.ra[1] = lanes(|i| fl(a[i]));
        q.rb[2] = lanes(|i| (i as i32 - 8) as u32);
    });
    for i in 0..16 {
        assert_eq!(q.ra[5][i], flush(a[i]) as i32 as u32, "ftoi lane {i}");
        assert_eq!(f32::from_bits(q.ra[6][i]), (i as i32 - 8) as f32);
    }
    assert_eq!(q.ra[5][1], (-2i32) as u32, "FTOI truncates toward zero");
}

#[test]
fn mul_ops() {
    let q = run(&[nop().mul(op::FMUL, 5, A, B).ra(1).rb(2)], &[], |q| { q.ra[1] = lanes(|i| fl(i as f32 * 0.5)); q.rb[2] = lanes(|i| fl(i as f32 - 3.0)); });
    for i in 0..16 { assert_eq!(f32::from_bits(q.rb[5][i]), i as f32 * 0.5 * (i as f32 - 3.0)); }
    let ops: [(u8, fn(u32, u32) -> u32); 6] = [
        (op::MUL24, |a, b| (a & 0xff_ffff).wrapping_mul(b & 0xff_ffff)),
        (op::V8MULD, |a, b| { let (x, y) = (a.to_le_bytes(), b.to_le_bytes()); u32::from_le_bytes(std::array::from_fn(|k| ((x[k] as f32 * y[k] as f32) / 255.0).round() as u8)) }),
        (op::V8MIN, |a, b| { let (x, y) = (a.to_le_bytes(), b.to_le_bytes()); u32::from_le_bytes(std::array::from_fn(|k| x[k].min(y[k]))) }),
        (op::V8MAX, |a, b| { let (x, y) = (a.to_le_bytes(), b.to_le_bytes()); u32::from_le_bytes(std::array::from_fn(|k| x[k].max(y[k]))) }),
        (op::M_V8ADDS, |a, b| { let (x, y) = (a.to_le_bytes(), b.to_le_bytes()); u32::from_le_bytes(std::array::from_fn(|k| x[k].saturating_add(y[k]))) }),
        (op::M_V8SUBS, |a, b| { let (x, y) = (a.to_le_bytes(), b.to_le_bytes()); u32::from_le_bytes(std::array::from_fn(|k| x[k].saturating_sub(y[k]))) }),
    ];
    for (o, f) in ops {
        let q = run(&[nop().mul(o, 5, A, B).ra(1).rb(2)], &[], |q| { q.ra[1] = A_VALS; q.rb[2] = B_VALS; });
        for i in 0..16 { assert_eq!(q.rb[5][i], f(A_VALS[i], B_VALS[i]), "mul op {o} lane {i}"); }
    }
}

#[test]
fn v8muld_rounds_like_division() {
    for a in 0..=255u32 { for b in [0u32, 1, 2, 127, 128, 254, 255] {
        assert_eq!(qpu::mul_op(op::V8MULD, a, b), ((a * b) as f32 / 255.0).round() as u32, "{a} {b}");
    } }
}

#[test]
fn conditions_and_flags() {
    // flags from ra1 - rb2 (integer): Z where equal, N where less, C on borrow
    let prog = [
        nop().add(op::SUB, waddr::NOP, A, B).ra(1).rb(2).sf(),
        ldi(10, 0),
        ldi(11, 0),
        ldi(12, 0),
        ldi(13, 0),
        ldi(14, 0),
        ldi(15, 0),
        ldi(10, 1).cond_add(COND_ZS),
        ldi(11, 1).cond_add(COND_ZC),
        ldi(12, 1).cond_add(COND_NS),
        ldi(13, 1).cond_add(COND_NC),
        ldi(14, 1).cond_add(6),
        ldi(15, 1).cond_add(7),
    ];
    let a: V = lanes(|i| i as u32);
    let b: V = lanes(|i| if i < 8 { 4 } else { 0x8000_0010 });
    let q = run(&prog, &[], |q| { q.ra[1] = a; q.rb[2] = b; });
    for i in 0..16 {
        let r = a[i].wrapping_sub(b[i]);
        assert_eq!(q.ra[10][i], (r == 0) as u32, "zs {i}");
        assert_eq!(q.ra[11][i], (r != 0) as u32);
        assert_eq!(q.ra[12][i], ((r as i32) < 0) as u32, "ns {i}");
        assert_eq!(q.ra[13][i], ((r as i32) >= 0) as u32);
        assert_eq!(q.ra[14][i], (a[i] < b[i]) as u32, "cs {i}");
        assert_eq!(q.ra[15][i], (a[i] >= b[i]) as u32);
    }
    // float flags: N for negative results; flags from the mul pipe when the add pipe idles
    let prog = [nop().mul(op::FMUL, waddr::NOP, A, B).ra(1).rb(2).sf(), ldi(10, 0), ldi(10, 7).cond_add(COND_NS)];
    let q = run(&prog, &[], |q| { q.ra[1] = lanes(|i| fl(i as f32 - 4.0)); q.rb[2] = [fl(1.0); 16]; });
    for i in 0..16 { assert_eq!(q.ra[10][i], if i < 4 { 7 } else { 0 }); }
}

#[test]
fn add_carry_and_saturating_pack() {
    let prog = [nop().add(op::ADD, waddr::NOP, A, B).ra(1).rb(2).sf(), ldi(10, 0), ldi(10, 1).cond_add(6),
                nop().add(op::ADD, 11, A, B).ra(1).rb(2).pack(8)];
    let a: V = lanes(|i| if i & 1 == 0 { 0xffff_fff0 } else { 0x7fff_fff0 });
    let b: V = [0x20; 16];
    let q = run(&prog, &[], |q| { q.ra[1] = a; q.rb[2] = b; });
    for i in 0..16 {
        assert_eq!(q.ra[10][i], (i & 1 == 0) as u32, "carry out {i}");
        // 32-bit saturation: signed overflow clamps
        assert_eq!(q.ra[11][i], if i & 1 == 0 { 0x10 } else { 0x7fff_ffff }, "sat {i}");
    }
}

#[test]
fn regfile_a_packs() {
    let v: V = lanes(|i| fl(i as f32 / 15.0));
    // float results: 16-bit float halves, 8-bit unorm bytes (the rest of the register kept)
    let prog = [
        ldi(5, 0xaabb_ccdd),
        nop().add(op::FMAX, 5, B, B).rb(2).pack(1),
        ldi(6, 0xaabb_ccdd),
        nop().add(op::FMAX, 6, B, B).rb(2).pack(2),
        ldi(7, 0xaabb_ccdd),
        nop().add(op::FMAX, 7, B, B).rb(2).pack(5),
        nop().add(op::FMAX, 8, B, B).rb(2).pack(3),
        // integer results: low half / byte, and the saturating byte
        ldi(9, 0xaabb_ccdd),
        nop().add(op::OR, 9, B, B).rb(3).pack(4),
        nop().add(op::OR, 10, B, B).rb(3).pack(12),
        nop().add(op::OR, 11, B, B).rb(3).pack(9),
    ];
    let ints: V = lanes(|i| (i as i32 * 100 - 300) as u32);
    let q = run(&prog, &[], |q| { q.rb[2] = v; q.rb[3] = ints; });
    for i in 0..16 {
        let x = f32::from_bits(v[i]);
        let h = qpu::f32_to_f16(x) as u32;
        assert_eq!(q.ra[5][i], 0xaabb_0000 | h);
        assert_eq!(q.ra[6][i], h << 16 | 0xccdd);
        let b = (x * 255.0 + 0.5) as u32;
        assert_eq!(q.ra[7][i], 0xaabb_00dd | b << 8);
        assert_eq!(q.ra[8][i], b * 0x0101_0101);
        assert_eq!(q.ra[9][i], 0xaabb_cc00 | (ints[i] & 0xff));
        assert_eq!(q.ra[10][i], (ints[i] as i32).clamp(0, 255) as u32);
        assert_eq!(q.ra[11][i] & 0xffff, (ints[i] as i32).clamp(-32768, 32767) as u32 & 0xffff);
    }
    assert_eq!(qpu::f16_to_f32(qpu::f32_to_f16(0.333)), 0.33300781);
}

#[test]
fn mul_pack_writes_colour_bytes() {
    let prog = [
        ldi(waddr::R0, 0),
        nop().mul(op::V8MIN, waddr::R0, A, A).ra(1).pm().pack(4),
        nop().mul(op::V8MIN, waddr::R0, A, A).ra(2).pm().pack(5),
        nop().mul(op::FMUL, waddr::R0, A, B).ra(3).small(32).pm().pack(7),
        mov(5, R0),
        nop().mul(op::V8MIN, 6, A, A).ra(1).pm().pack(3),
    ];
    let q = run(&prog, &[], |q| { q.ra[1] = [fl(1.0); 16]; q.ra[2] = [fl(0.5); 16]; q.ra[3] = [fl(-1.0); 16]; });
    assert_eq!(q.ra[5][0], 0x0000_80ff);
    assert_eq!(q.rb[6][3], 0xffff_ffff);
}

#[test]
fn unpacks() {
    let prog = [
        nop().add(op::FMAX, 5, A, A).ra(1).unpack(1),      // f16 low -> f32
        nop().add(op::OR, 6, A, A).ra(1).unpack(2),        // int16 high, sign-extended
        nop().add(op::FMAX, 7, A, A).ra(2).unpack(5),      // byte 1 as unorm float
        nop().add(op::OR, 8, A, A).ra(2).unpack(7),        // byte 3 zero-extended
        nop().add(op::OR, 9, A, A).ra(2).unpack(3),        // byte 3 replicated
        nop().mul(op::FMUL, 10, R4, B).small(32).pm().unpack(4), // r4 byte 0 as unorm (PM=1)
    ];
    let q = run(&prog, &[], |q| {
        q.ra[1] = [(qpu::f32_to_f16(-1.5) as u32) | 0x8001_0000; 16];
        q.ra[2] = [0x80ff_3311; 16];
        q.acc[4] = [0x0000_00ff; 16];
    });
    assert_eq!(f32::from_bits(q.ra[5][0]), -1.5);
    assert_eq!(q.ra[6][0], 0xffff_8001);
    assert_eq!(f32::from_bits(q.ra[7][0]), 0x33 as f32 / 255.0);
    assert_eq!(q.ra[8][0], 0x80);
    assert_eq!(q.ra[9][0], 0x8080_8080);
    assert_eq!(f32::from_bits(q.rb[10][0]), 1.0);
}

#[test]
fn small_immediates_and_rotation() {
    assert_eq!(qpu::small_imm(5), 5);
    assert_eq!(qpu::small_imm(16), -16i32 as u32);
    assert_eq!(qpu::small_imm(31), -1i32 as u32);
    assert_eq!(f32::from_bits(qpu::small_imm(32)), 1.0);
    assert_eq!(f32::from_bits(qpu::small_imm(39)), 128.0);
    assert_eq!(f32::from_bits(qpu::small_imm(40)), 1.0 / 256.0);
    assert_eq!(f32::from_bits(qpu::small_imm(47)), 0.5);
    let prog = [
        nop().add(op::ADD, 5, A, B).ra(1).small(20),
        mov(waddr::R0, A).ra(1),
        nop(),
        // r1 = r0 rotated by 1 (lane i gets lane i-1), r2 rotated by r5 (3)
        nop().mul(op::V8MIN, waddr::R1, R0, R0).small(49),
        ldi(waddr::R5, 3).ws(),
        nop(),
        nop().mul(op::V8MIN, waddr::R2, R0, R0).small(48),
    ];
    let q = run(&prog, &[], |q| q.ra[1] = lanes(|i| i as u32 * 10));
    for i in 0..16 {
        assert_eq!(q.ra[5][i], (i as u32 * 10).wrapping_sub(12));
        assert_eq!(q.acc[1][i], ((i + 15) % 16) as u32 * 10);
        assert_eq!(q.acc[2][i], ((i + 13) % 16) as u32 * 10);
    }
}

#[test]
fn load_immediates() {
    let mut per = ldi(5, 0x0003_0005); // lanes: ms bits 0..1 = 1, ls bits 0, 2 = 1
    per.0 |= 1 << 57; // signed per-element
    let mut peru = ldi(6, 0x0003_0005);
    peru.0 |= 3 << 57;
    let q = run(&[ldi(4, 0xdead_beef), per, peru, ldi(7, 12).ws()], &[], |_| {});
    assert_eq!(q.ra[4], [0xdead_beef; 16]);
    assert_eq!(q.ra[5][0], -1i32 as u32); // 0b11
    assert_eq!(q.ra[5][1], -2i32 as u32); // 0b10
    assert_eq!(q.ra[5][2], 1); // 0b01
    assert_eq!(q.ra[5][3], 0);
    assert_eq!(q.ra[6][0], 3);
    assert_eq!(q.ra[6][1], 2);
    assert_eq!(q.rb[7], [12; 16]);
}

#[test]
fn uniforms_and_their_address() {
    let prog = [
        mov(5, A).ra(raddr::UNIF),
        mov(6, B).rb(raddr::UNIF),
        // reset the stream to the first uniform again
        ldi(waddr::R0, UNIF),
        nop().add(op::OR, waddr::UNIFORMS_ADDRESS, R0, R0),
        nop(),
        nop(),
        mov(7, A).ra(raddr::UNIF),
    ];
    let q = run(&prog, &[11, 22, 33], |_| {});
    assert_eq!(q.ra[5][3], 11);
    assert_eq!(q.ra[6][3], 22);
    assert_eq!(q.ra[7][3], 11);
}

#[test]
fn sfu() {
    let prog = [
        mov(waddr::SFU_RECIP, A).ra(1), nop(), nop(), mov(5, R4),
        mov(waddr::SFU_RECIPSQRT, A).ra(1), nop(), nop(), mov(6, R4),
        mov(waddr::SFU_EXP, A).ra(1), nop(), nop(), mov(7, R4),
        mov(waddr::SFU_LOG, A).ra(1), nop(), nop(), mov(8, R4),
    ];
    let q = run(&prog, &[], |q| q.ra[1] = lanes(|i| fl(i as f32 + 1.0)));
    for i in 0..16 {
        let x = i as f32 + 1.0;
        assert_eq!(f32::from_bits(q.ra[5][i]), 1.0 / x);
        assert_eq!(f32::from_bits(q.ra[6][i]), 1.0 / x.sqrt());
        assert_eq!(f32::from_bits(q.ra[7][i]), x.exp2());
        assert_eq!(f32::from_bits(q.ra[8][i]), x.log2());
    }
}

#[test]
fn r5_writes_replicate() {
    let q = run(&[nop().add(op::OR, waddr::R5, A, A).ra(1), mov(5, R5), nop().add(op::OR, waddr::R5, A, A).ra(1).ws(), mov(6, R5)], &[], |q| q.ra[1] = lanes(|i| i as u32));
    for i in 0..16 {
        assert_eq!(q.ra[5][i], (i & !3) as u32, "per quad");
        assert_eq!(q.ra[6][i], 0, "element 0");
    }
}

#[test]
fn branches_with_delay_slots_and_link() {
    // 0: branch always +2 instructions past the delay slots; slots run; the skipped one does not
    let mut br = branch(15, true, 2 * 8);
    br.0 = (br.0 & !(0x3f << 38)) | (5u64 << 38); // link into ra5
    let prog = [
        br,
        ldi(6, 1),
        ldi(7, 2),
        ldi(8, 3),
        ldi(9, 99),
        ldi(9, 98),
        ldi(10, 4),
    ];
    let q = run(&prog, &[], |_| {});
    assert_eq!((q.ra[6][0], q.ra[7][0], q.ra[8][0], q.ra[9][0], q.ra[10][0]), (1, 2, 3, 0, 4));
    assert_eq!(q.ra[5][0], CODE + 4 * 8);
    // conditional: ANY_ZS taken when one lane has Z; ALL_ZS not
    let prog = [
        nop().add(op::OR, waddr::NOP, A, A).ra(1).sf(),
        branch(0, true, 8), nop(), nop(), nop(), ldi(5, 1), // ALL_ZS: not taken, sets ra5
        branch(2, true, 8), nop(), nop(), nop(), ldi(6, 1), // ANY_ZS: taken, skips
    ];
    let q = run(&prog, &[], |q| q.ra[1] = lanes(|i| (i != 3) as u32));
    assert_eq!(q.ra[5][0], 1);
    assert_eq!(q.ra[6][0], 0);
}

#[test]
fn program_end_runs_two_delay_slots() {
    let prog = [nop().sig(sig::PROG_END), ldi(5, 1), ldi(6, 2), ldi(7, 3)];
    let mut ram = vec![0u8; 0x10_0000];
    let b = bytes(&prog);
    ram[CODE as usize..CODE as usize + b.len()].copy_from_slice(&b);
    let mut m = Mem::new(&mut ram);
    let mut q = Qpu::new();
    q.start(CODE, UNIF);
    assert_eq!(q.run(&mut Code::default(), &mut m, &mut NoEnv { vpm: vec![] }, 100), Stop::End);
    assert_eq!((q.ra[5][0], q.ra[6][0], q.ra[7][0]), (1, 2, 0));
}

#[test]
fn tmu_direct_read_and_texture() {
    let mut ram = vec![0u8; 0x10_0000];
    for k in 0..64u32 { ram[0x2_0000 + 4 * k as usize..0x2_0004 + 4 * k as usize].copy_from_slice(&(k * 3).to_le_bytes()); }
    // a 4x4 RGBA8888 texture, LT (it is small): texel (x, y) = x | y << 8
    let lay = crate::tiling::Layout::new(crate::tiling::LT, 4, 4);
    for y in 0..4 { for x in 0..4 {
        let o = 0x3_0000 + lay.offset(x, y) as usize;
        ram[o..o + 4].copy_from_slice(&(x | y << 8 | 0xff00_0000).to_le_bytes());
    } }
    let p0 = 0x3_0000; // RGBA8888, one level
    let p1 = 4 << 20 | 4 << 8 | 1 << 7 | 1 << 4; // 4x4, nearest
    let prog = [
        // direct: S only, address per lane
        nop().add(op::SHL, waddr::R0, A, B).ra(raddr::ELEM_QPU).small(2),
        nop().add(op::ADD, waddr::TMU0_S, R0, B).rb(raddr::UNIF),
        nop().sig(sig::LOAD_TMU0),
        mov(5, R4),
        // texture: T then S, P0 and P1 from the uniform stream
        mov(waddr::TMU0_T, A).ra(2),
        mov(waddr::TMU0_S, A).ra(1),
        nop().sig(sig::LOAD_TMU0),
        mov(6, R4),
    ];
    let q = run_with(&prog, &[0x2_0000, p0, p1], &mut ram, |q| {
        q.ra[1] = lanes(|i| fl((i % 4) as f32 / 4.0 + 0.125));
        q.ra[2] = lanes(|i| fl((i / 4) as f32 / 4.0 + 0.125));
    });
    for i in 0..16 {
        assert_eq!(q.ra[5][i], i as u32 * 3, "direct lane {i}");
        assert_eq!(q.ra[6][i], (i % 4) as u32 | ((i / 4) as u32) << 8 | 0xff00_0000, "texel lane {i}");
    }
}

#[test]
fn vpm_generic_reads_and_writes() {
    let mut ram = vec![0u8; 0x10_0000];
    // write rows 2 and 3 horizontally, then read row 3 back; vertical write into column 5
    let prog = [
        ldi(waddr::VPM_SETUP, 0x1a02).ws(),            // write setup: horizontal 32-bit, stride 1, row 2
        mov(waddr::VPM, A).ra(raddr::ELEM_QPU),
        nop().add(op::ADD, waddr::VPM, A, B).ra(raddr::ELEM_QPU).small(10),
        ldi(waddr::VPM_SETUP, 0x0010_1a03),             // read setup: one vector from row 3
        mov(5, A).ra(raddr::VPM),
        ldi(waddr::VPM_SETUP, 0x1205).ws(),            // vertical 32-bit, column 5, rows 0..15
        nop().add(op::ADD, waddr::VPM, A, B).ra(raddr::ELEM_QPU).small(1),
        ldi(waddr::VPM_SETUP, 0x0010_1205),             // read it back vertically
        mov(6, A).ra(raddr::VPM),
    ];
    let p = { let mut p = prog.to_vec(); p.extend_from_slice(&end()); p };
    let b = bytes(&p);
    ram[CODE as usize..CODE as usize + b.len()].copy_from_slice(&b);
    let mut m = Mem::new(&mut ram);
    let mut q = Qpu::new();
    q.start(CODE, UNIF);
    let mut env = NoEnv { vpm: vec![0; 192 * 16] };
    assert_eq!(q.run(&mut Code::default(), &mut m, &mut env, 1000), Stop::End);
    for i in 0..16 {
        assert_eq!(q.ra[5][i], i as u32 + 10);
        assert_eq!(q.ra[6][i], i as u32 + 1);
        if i != 5 { assert_eq!(env.vpm[2 * 16 + i], i as u32); }
        assert_eq!(env.vpm[i * 16 + 5], i as u32 + 1);
    }
}

/// User programs through the register interface: two programs hand a VPM row over with a
/// semaphore; one stores it to memory with VPM DMA, the other loads memory into the VPM first.
#[test]
fn user_programs_semaphores_and_vpm_dma() {
    let mut ram = vec![0u8; 0x10_0000];
    for k in 0..16u32 { ram[0x4_0000 + 4 * k as usize..0x4_0004 + 4 * k as usize].copy_from_slice(&(k * 7 + 1).to_le_bytes()); }
    // program A (queued first): waits for semaphore 3, then DMA-stores VPM row 4 to 0x50000
    let a = [
        sem(3, true),
        ldi(waddr::VPM_SETUP, 2 << 30 | 1 << 23 | 16 << 16 | 1 << 14 | (4 << 4) << 3).ws(), // VDW: 1 unit of 16 words, horizontal, row 4
        ldi(waddr::VPM_ADDR, 0x5_0000).ws(),
        mov(waddr::NOP, B).rb(raddr::VPM_WAIT),
    ];
    // program B: DMA-loads 16 words from 0x40000 into row 4, adds 100 through a generic read and
    // write, then signals A
    let b = [
        ldi(waddr::VPM_SETUP, 1 << 31 | 1 << 16 | 4 << 4),  // VDR: one row of 16 words (0 = 16) to row 4
        ldi(waddr::VPM_ADDR, 0x4_0000),
        mov(waddr::NOP, A).ra(raddr::VPM_WAIT),
        ldi(waddr::VPM_SETUP, 0x0010_1a04),
        ldi(waddr::VPM_SETUP, 0x1a04).ws(),
        nop().add(op::ADD, waddr::VPM, A, B).ra(raddr::VPM).small(15),
        sem(3, false),
    ];
    for (base, p) in [(0x1000usize, a.to_vec()), (0x2000, b.to_vec())] {
        let mut p = p;
        p.extend_from_slice(&end());
        let bb = bytes(&p);
        ram[base..base + bb.len()].copy_from_slice(&bb);
    }
    let mut v = crate::V3d::new();
    v.write(crate::SRQCS as u64, 1 << 7 | 1 << 8 | 1 << 16, &mut ram);
    v.write(crate::SRQUA as u64, 0x6000, &mut ram);
    v.write(crate::SRQPC as u64, 0x1000, &mut ram);
    assert_eq!(v.read(crate::SRQCS as u64) >> 16 & 0xff, 0, "A waits for B");
    v.write(crate::SRQPC as u64, 0x2000, &mut ram);
    assert_eq!(v.read(crate::SRQCS as u64) >> 16 & 0xff, 2, "both completed");
    assert_eq!(v.read(crate::SRQCS as u64) >> 8 & 0xff, 2);
    for k in 0..16u32 {
        let got = u32::from_le_bytes(ram[0x5_0000 + 4 * k as usize..0x5_0004 + 4 * k as usize].try_into().unwrap());
        assert_eq!(got, k * 7 + 1 + 15, "word {k}");
    }
}

#[test]
fn decode_fields() {
    let i = nop().add(op::FADD, 7, R1, A).mul(op::FMUL, 9, R2, B).ra(3).rb(4).ws().sf().cond_add(COND_ZS).pack(4).pm();
    let d = qpu::decode(i.0);
    assert_eq!((d.op_add, d.waddr_add, d.add_a, d.add_b), (op::FADD, 7, R1, A));
    assert_eq!((d.op_mul, d.waddr_mul, d.mul_a, d.mul_b), (op::FMUL, 9, R2, B));
    assert_eq!((d.raddr_a, d.raddr_b, d.ws, d.sf, d.cond_add, d.pack, d.pm), (3, 4, true, true, COND_ZS, 4, true));
    let _ = ZERO;
}
