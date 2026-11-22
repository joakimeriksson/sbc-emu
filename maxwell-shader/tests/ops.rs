//! Single instructions, assembled here from the bit layout of the GM107 emitter, against
//! reference implementations.

#![allow(clippy::needless_range_loop, clippy::precedence)]

use maxwell_shader::isa::*;
use maxwell_shader::*;

/// An instruction word: the top 16 bits and fields (position, length, value).
fn word(top: u64, fields: &[(u32, u32, u64)]) -> u64 {
    let mut w = top << 48 | 7 << 16;
    for &(pos, len, v) in fields {
        let m = (1u64 << len) - 1;
        assert!(v & !m == 0, "{v:#x} in {len} bits");
        w |= v << pos;
    }
    w
}

const EXIT: u64 = 0xe300_0000_0007_000f;

/// Code with scheduling words put in. Addresses count them: instruction n is at `at(n)`.
fn assemble(ins: &[u64]) -> Vec<u8> {
    let mut w = Vec::new();
    for (n, i) in ins.iter().chain([&EXIT]).enumerate() {
        if n % 3 == 0 {
            w.push(0x001f_8000_fc00_07e0);
        }
        w.push(*i);
    }
    w.iter().flat_map(|x| x.to_le_bytes()).collect()
}

fn at(n: usize) -> u64 {
    (n + n / 3 + 1) as u64 * 8
}

/// A 24-bit branch offset from instruction `from` to instruction `to`.
fn rel(from: usize, to: usize) -> u64 {
    at(to).wrapping_sub(at(from) + 8) & 0xff_ffff
}

#[derive(Default)]
struct Mem {
    bytes: Vec<u8>,
    shared: Vec<u8>,
    attrs: Vec<[u32; 32]>,
}

impl<const N: usize> Host<N> for Mem {
    fn cbuf(&self, index: u32, offset: u32) -> u32 {
        index << 16 | offset
    }
    fn sysval(&mut self, sys: u8) -> [u32; N] {
        [sys as u32; N]
    }
    fn load(&mut self, addr: u64, out: &mut [u8]) {
        let a = addr as usize;
        out.copy_from_slice(&self.bytes[a..a + out.len()]);
    }
    fn store(&mut self, addr: u64, data: &[u8]) {
        let a = addr as usize;
        self.bytes[a..a + data.len()].copy_from_slice(data);
    }
    fn shared(&mut self) -> &mut [u8] {
        &mut self.shared
    }
    fn attr_load(&mut self, lane: usize, a: Attr) -> u32 {
        if a.output {
            self.attrs[a.vertex as usize][a.addr as usize / 4]
        } else {
            (lane as u32) << 8 | a.addr
        }
    }
    fn attr_store(&mut self, lane: usize, a: Attr, v: u32) {
        self.attrs[lane][a.addr as usize / 4] = v;
    }
}

fn prog(ins: &[u64]) -> Program {
    let p = Program::decode(&assemble(ins)).unwrap();
    assert!(!p.any(|o| matches!(o, Op::Invalid(_))), "{p}");
    p
}

/// Runs `ins` with registers preset; all 4 lanes.
fn exec(ins: &[u64], regs: &[(u8, [u32; 4])]) -> Lanes<4> {
    let p = prog(ins);
    let mut l = Lanes::<4>::new();
    l.start(&p, 15, 0);
    for &(r, v) in regs {
        l.gpr[r as usize] = v;
    }
    let mut h = Mem { bytes: vec![0; 256], ..Mem::default() };
    assert_eq!(run(&p, &mut l, &mut h, &mut 10_000), Exit::Done, "{p}");
    l
}

struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u32 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        (self.0.wrapping_mul(0x2545_f491_4f6c_dd1d) >> 32) as u32
    }
    fn lanes(&mut self) -> [u32; 4] {
        std::array::from_fn(|_| {
            let (x, k) = (self.next(), self.next());
            match k & 7 {
                0 => x & 31,
                1 => 0x8000_0000,
                2 => !0,
                3 => x >> 16,
                _ => x,
            }
        })
    }
}

/// d = op(r0, r1, r2) in r3 for many random inputs.
fn check3(w: u64, f: impl Fn(u32, u32, u32) -> u32) {
    let w = w | 3;
    let p = prog(&[w]);
    let mut r = Rng(0x1234_5678_9abc);
    let mut l = Lanes::<4>::new();
    for _ in 0..2000 {
        let v = [r.lanes(), r.lanes(), r.lanes()];
        l.start(&p, 15, 0);
        (l.gpr[0], l.gpr[1], l.gpr[2]) = (v[0], v[1], v[2]);
        assert_eq!(run(&p, &mut l, &mut Mem::default(), &mut 100), Exit::Done);
        for i in 0..4 {
            let want = f(v[0][i], v[1][i], v[2][i]);
            assert_eq!(l.gpr[3][i], want, "{p} {:x} {:x} {:x}", v[0][i], v[1][i], v[2][i]);
        }
    }
}

/// Registers r0, r1 (and r2) as sources.
fn ab(top: u64, more: &[(u32, u32, u64)]) -> u64 {
    word(top, &[]) | 1 << 20 | more.iter().fold(0, |w, &(p, l, v)| w | word(0, &[(p, l, v)]) & !(7 << 16))
}

fn abc(top: u64, more: &[(u32, u32, u64)]) -> u64 {
    ab(top, more) | 2 << 39
}

#[test]
fn integer_arithmetic() {
    let add = u32::wrapping_add;
    check3(ab(0x5c10, &[]), |a, b, _| add(a, b));
    check3(ab(0x5c11, &[]), |a, b, _| a.wrapping_sub(b));
    check3(ab(0x5c12, &[]), |a, b, _| b.wrapping_sub(a));
    check3(ab(0x5c13, &[]), |a, b, _| add(add(a, b), 1));
    check3(ab(0x5c14, &[]), |a, b, _| (a as i32).saturating_add(b as i32) as u32);
    check3(ab(0x5c18, &[(39, 5, 3)]), |a, b, _| add(a << 3, b));
    check3(ab(0x5c19, &[(39, 5, 7)]), |a, b, _| (a << 7).wrapping_sub(b));
    check3(ab(0x5c1a, &[(39, 5, 7)]), |a, b, _| b.wrapping_sub(a << 7));
    check3(ab(0x5c38, &[]), |a, b, _| a.wrapping_mul(b));
    check3(ab(0x5c38, &[(39, 1, 1)]), |a, b, _| ((a as u64 * b as u64) >> 32) as u32);
    check3(ab(0x5c38, &[(39, 3, 7)]), |a, b, _| ((a as i32 as i64 * b as i32 as i64) >> 32) as u32);
    check3(abc(0x5a00, &[]), |a, b, c| add(a.wrapping_mul(b), c));
    check3(abc(0x5a00, &[(54, 1, 1)]), |a, b, c| add(((a as u64 * b as u64) >> 32) as u32, c));
    check3(abc(0x5a00, &[(51, 1, 1)]), |a, b, c| c.wrapping_sub(a.wrapping_mul(b)));
    check3(abc(0x5a00, &[(52, 1, 1)]), |a, b, c| a.wrapping_mul(b).wrapping_sub(c));
    check3(abc(0x5cc0, &[]), |a, b, c| add(add(a, b), c));
    check3(abc(0x5cc0, &[(49, 1, 1)]), |a, b, c| add(a, b).wrapping_sub(c));
    check3(abc(0x5cc0, &[(50, 2, 1)]), |a, b, c| add(a.wrapping_sub(b), c));
    check3(ab(0x5c21, &[(39, 3, 7)]), |a, b, _| (a as i32).min(b as i32) as u32);
    check3(ab(0x5c20, &[(39, 3, 7)]), |a, b, _| a.min(b));
    check3(ab(0x5c21, &[(39, 4, 15)]), |a, b, _| (a as i32).max(b as i32) as u32);
    check3(ab(0x5c20, &[(39, 4, 15)]), |a, b, _| a.max(b));
    // immediate and constant buffer forms
    check3(word(0x3810, &[(20, 19, 0x7fffe), (56, 1, 1)]), |a, _, _| a.wrapping_sub(2));
    check3(word(0x1c00, &[(20, 32, 0x8765_4321)]), |a, _, _| add(a, 0x8765_4321));
    check3(word(0x4c10, &[(34, 5, 3), (20, 14, 0x11)]), |a, _, _| add(a, 0x3_0044));
    check3(word(0x5200, &[(34, 5, 2), (20, 14, 1)]) | 2 << 39, |a, _, c| add(a.wrapping_mul(c), 0x2_0004));
    check3(word(0x1f00, &[(20, 32, 0xdead_beef)]), |a, _, _| a.wrapping_mul(0xdead_beef));
    check3(word(0x1400, &[(20, 32, 0x1234_5678), (53, 5, 4)]), |a, _, _| add(a << 4, 0x1234_5678));
}

#[test]
fn xmad_makes_a_32_bit_multiply() {
    // nv50_ir's handleMULMAD: three XMADs for a * b + c.
    let ins = [
        word(0x5b00, &[(0, 8, 4), (8, 8, 1), (20, 8, 0), (39, 8, 2)]),
        word(0x5b00, &[(0, 8, 5), (8, 8, 1), (20, 8, 0), (39, 8, 255), (37, 1, 1), (35, 1, 1)]),
        word(0x5b00, &[(0, 8, 3), (8, 8, 1), (20, 8, 5), (39, 8, 4), (36, 1, 1), (50, 3, 4), (53, 1, 1), (35, 1, 1)]),
    ];
    let mut r = Rng(99);
    for _ in 0..5000 {
        let v = [r.lanes(), r.lanes(), r.lanes()];
        let l = exec(&ins, &[(0, v[0]), (1, v[1]), (2, v[2])]);
        for i in 0..4 {
            assert_eq!(l.gpr[3][i], v[0][i].wrapping_mul(v[1][i]).wrapping_add(v[2][i]));
        }
    }
    let half = |x: u32, hi: bool| if hi { x >> 16 } else { x & 0xffff };
    check3(abc(0x5b00, &[(53, 1, 1)]), |a, b, c| (half(a, true) * half(b, false)).wrapping_add(c));
    check3(abc(0x5b00, &[(48, 2, 3), (50, 3, 1)]), |a, b, c| {
        (a as i16 as i32 * b as i16 as i32).wrapping_add((c & 0xffff) as i32) as u32
    });
    check3(abc(0x5b00, &[(50, 3, 2)]), |a, b, c| (half(a, false) * half(b, false)).wrapping_add(c >> 16));
    // constant and immediate operands
    check3(word(0x4e00, &[(34, 5, 1), (20, 14, 2), (39, 8, 2), (55, 1, 1)]), |a, _, c| {
        ((half(a, false) * 8) << 16).wrapping_add(c)
    });
    check3(word(0x5100, &[(34, 5, 1), (20, 14, 2), (39, 8, 1)]), |a, b, _| {
        (half(a, false) * half(b, false)).wrapping_add(0x1_0008)
    });
    check3(word(0x3600, &[(20, 16, 0xffff), (39, 8, 2), (53, 1, 1)]), |a, _, c| {
        (half(a, true) * 0xffff).wrapping_add(c)
    });
}

#[test]
fn wide_integers() {
    let mut r = Rng(5);
    for _ in 0..3000 {
        let v: [[u32; 4]; 4] = std::array::from_fn(|_| r.lanes());
        let v = [v[0], v[1], v[2], if r.next() & 3 == 0 { v[1] } else { v[3] }];
        let regs = [(0, v[0]), (1, v[1]), (2, v[2]), (3, v[3])];
        let x = |i: usize| v[0][i] as u64 | (v[1][i] as u64) << 32;
        let y = |i: usize| v[2][i] as u64 | (v[3][i] as u64) << 32;
        // add and subtract with carry
        for (neg, f) in [(0, u64::wrapping_add as fn(u64, u64) -> u64), (1, u64::wrapping_sub)] {
            let l = exec(
                &[
                    word(0x5c10, &[(0, 8, 4), (8, 8, 0), (20, 8, 2), (47, 1, 1), (48, 1, neg)]),
                    word(0x5c10, &[(0, 8, 5), (8, 8, 1), (20, 8, 3), (43, 1, 1), (48, 1, neg)]),
                ],
                &regs,
            );
            for i in 0..4 {
                let got = l.gpr[4][i] as u64 | (l.gpr[5][i] as u64) << 32;
                assert_eq!(got, f(x(i), y(i)));
            }
        }
        // compare: subtract the low halves, then ISETP.X (and ISET.X) on the high ones
        for cond in 1..7u64 {
            for signed in 0..2u64 {
                let l = exec(
                    &[
                        word(0x5c11, &[(0, 8, 255), (8, 8, 0), (20, 8, 2), (47, 1, 1)]),
                        word(0x5b60, &[(3, 3, 0), (0, 3, 1), (8, 8, 1), (20, 8, 3), (43, 1, 1), (39, 3, 7), (48, 1, signed), (49, 3, cond)]),
                        word(0x5b50, &[(0, 8, 6), (8, 8, 1), (20, 8, 3), (43, 1, 1), (39, 3, 7), (48, 1, signed), (49, 3, cond)]),
                    ],
                    &regs,
                );
                for i in 0..4 {
                    let o = if signed == 1 {
                        (x(i) as i64).cmp(&(y(i) as i64))
                    } else {
                        x(i).cmp(&y(i))
                    };
                    let want = cond >> (o as i32 + 1) & 1 != 0;
                    assert_eq!(l.pred[0] >> i & 1 != 0, want, "{:x} {:x} {cond}", x(i), y(i));
                    assert_eq!(l.pred[1] >> i & 1 != 0, !want);
                    assert_eq!(l.gpr[6][i] != 0, want);
                }
            }
        }
        // minimum and maximum: the high halves decide
        for max in 0..2u64 {
            for signed in 0..2u64 {
                let l = exec(
                    &[
                        word(0x5c20, &[(0, 8, 5), (8, 8, 1), (20, 8, 3), (47, 1, 1), (43, 2, 3), (39, 3, 7), (42, 1, max), (48, 1, signed)]),
                        word(0x5c20, &[(0, 8, 4), (8, 8, 0), (20, 8, 2), (43, 2, 1), (39, 3, 7), (42, 1, max), (48, 1, signed)]),
                    ],
                    &regs,
                );
                for i in 0..4 {
                    let (a, b) = (x(i), y(i));
                    let want = match (max, signed) {
                        (0, 0) => a.min(b),
                        (1, 0) => a.max(b),
                        (0, _) => (a as i64).min(b as i64) as u64,
                        _ => (a as i64).max(b as i64) as u64,
                    };
                    assert_eq!(l.gpr[4][i] as u64 | (l.gpr[5][i] as u64) << 32, want);
                }
            }
        }
        // funnel shifts
        let s = r.next() as u64 & 63;
        for signed in 0..2u64 {
            let l = exec(
                &[
                    word(0x38f8, &[(0, 8, 4), (8, 8, 0), (20, 19, s), (39, 8, 1), (37, 2, 2 + signed)]),
                    word(0x38f8, &[(0, 8, 5), (8, 8, 255), (20, 19, s), (39, 8, 1), (37, 2, 2 + signed), (48, 1, 1)]),
                    word(0x36f8, &[(0, 8, 6), (8, 8, 255), (20, 19, s), (39, 8, 0), (37, 2, 2)]),
                    word(0x36f8, &[(0, 8, 7), (8, 8, 0), (20, 19, s), (39, 8, 1), (37, 2, 2)]),
                ],
                &regs,
            );
            for i in 0..4 {
                let want = if signed == 1 { (x(i) as i64 >> s) as u64 } else { x(i) >> s };
                assert_eq!(l.gpr[4][i] as u64 | (l.gpr[5][i] as u64) << 32, want, "{s}");
                assert_eq!(l.gpr[6][i] as u64 | (l.gpr[7][i] as u64) << 32, x(i) << s, "{s}");
            }
        }
    }
}

#[test]
fn bits() {
    check3(ab(0x5c40, &[]), |a, b, _| a & b);
    check3(ab(0x5c40, &[(41, 2, 1), (40, 1, 1)]), |a, b, _| a | !b);
    check3(ab(0x5c40, &[(41, 2, 2)]), |a, b, _| a ^ b);
    check3(ab(0x5c40, &[(41, 2, 3), (40, 1, 1)]), |_, b, _| !b);
    check3(word(0x0400, &[(20, 32, 0xff00_ff00), (53, 2, 2), (55, 1, 1)]), |a, _, _| !a ^ 0xff00_ff00);
    // a & b | c has truth table 0xea
    check3(abc(0x5be0, &[(28, 8, 0xea)]), |a, b, c| a & b | c);
    check3(abc(0x5be0, &[(28, 8, 0x96)]), |a, b, c| a ^ b ^ c);
    check3(word(0x3c00, &[(20, 19, 0xff), (39, 8, 2), (48, 8, 0x40)]), |a, _, c| a & 0xff & !c);
    check3(ab(0x5c48, &[]), |a, b, _| if b < 32 { a << b } else { 0 });
    check3(ab(0x5c48, &[(39, 1, 1)]), |a, b, _| a << (b & 31));
    check3(ab(0x5c28, &[]), |a, b, _| if b < 32 { a >> b } else { 0 });
    check3(ab(0x5c29, &[]), |a, b, _| (a as i32 >> b.min(31)) as u32);
    check3(ab(0x5c29, &[(39, 1, 1)]), |a, b, _| (a as i32 >> (b & 31)) as u32);
    check3(word(0x5c30, &[(20, 8, 1)]), |_, b, _| 31u32.wrapping_sub(b.leading_zeros()));
    check3(word(0x5c30, &[(20, 8, 1), (41, 1, 1)]), |_, b, _| if b == 0 { !0 } else { b.leading_zeros() });
    check3(word(0x5c31, &[(20, 8, 1)]), |_, b, _| {
        31u32.wrapping_sub(if b >> 31 != 0 { !b } else { b }.leading_zeros())
    });
    check3(word(0x5c08, &[(20, 8, 1), (40, 1, 1)]), |_, b, _| b.count_zeros());
    let bits = |a: u32, pos: u32, len: u32, signed: bool| {
        let mut r = 0u32;
        if len == 0 {
            return 0;
        }
        let top = a >> (pos + len - 1).min(31) & 1;
        for k in 0..32 {
            let bit = if k < len && pos + k < 32 { a >> (pos + k) & 1 } else { top & signed as u32 };
            r |= bit << k;
        }
        r
    };
    check3(ab(0x5c00, &[]), |a, b, _| bits(a, b & 255, b >> 8 & 255, false));
    check3(ab(0x5c01, &[]), |a, b, _| bits(a, b & 255, b >> 8 & 255, true));
    check3(word(0x3800, &[(20, 19, 0x0c05)]), |a, _, _| a >> 5 & 0xfff);
    check3(word(0x3801, &[(20, 19, 0x0816)]), |a, _, _| (a as i32 >> 22) as i8 as u32);
    check3(word(0x3800, &[(20, 19, 0x2000), (40, 1, 1)]), |a, _, _| a.reverse_bits());
    check3(abc(0x5bf0, &[]), |a, b, c| {
        let mut r = c;
        for k in 0..(b >> 8 & 255) {
            let at = (b & 255) + k;
            if at < 32 {
                r = r & !(1 << at) | ((a >> k.min(31) & (k < 32) as u32) & 1) << at;
            }
        }
        r
    });
    // PRMT: index, funnel shifts, replication
    check3(word(0x36c0, &[(20, 19, 0x7531), (39, 8, 2)]), |a, _, c| {
        a >> 8 & 255 | (a >> 24) << 8 | (c >> 8 & 255) << 16 | (c >> 24) << 24
    });
    check3(abc(0x5bc0, &[(48, 3, 1)]), |a, b, c| ((a as u64 | (c as u64) << 32) >> (8 * (b & 3))) as u32);
    check3(abc(0x5bc0, &[(48, 3, 2)]), |a, b, c| {
        (((a as u64) << 32 | c as u64) << (8 * (b & 3)) >> 32) as u32
    });
    check3(abc(0x5bc0, &[(48, 3, 3)]), |a, b, _| (a >> (8 * (b & 3)) & 255) * 0x0101_0101);
    check3(abc(0x5bc0, &[(48, 3, 6)]), |a, b, _| (a >> (16 * (b & 1)) & 0xffff) * 0x1_0001);
}

fn fbits(x: f32) -> u32 {
    if x.is_nan() {
        0x7fc0_0000
    } else {
        x.to_bits()
    }
}

/// As `check3`, for floats: NaNs are all the same, inputs are of sensible magnitude or special.
fn checkf(w: u64, f: impl Fn(f32, f32, f32) -> f32) {
    let p = prog(&[w | 3]);
    let mut r = Rng(42);
    let mut l = Lanes::<4>::new();
    let special =
        [0.0, -0.0, 1.0, -1.0, f32::INFINITY, f32::NEG_INFINITY, f32::NAN, 1e-40, f32::MAX, 0.5];
    let mut num = || -> f32 {
        let (x, k) = (r.next(), r.next());
        if k & 7 == 0 {
            special[x as usize % special.len()]
        } else {
            (x as i32 as f32) * 2f32.powi(-(k as i32 >> 8 & 31))
        }
    };
    for _ in 0..3000 {
        let v: [[f32; 4]; 3] = std::array::from_fn(|_| std::array::from_fn(|_| num()));
        l.start(&p, 15, 0);
        for k in 0..3 {
            l.gpr[k] = v[k].map(f32::to_bits);
        }
        assert_eq!(run(&p, &mut l, &mut Mem::default(), &mut 100), Exit::Done);
        for i in 0..4 {
            let (got, want) = (l.f32(3, i), f(v[0][i], v[1][i], v[2][i]));
            assert_eq!(fbits(got), fbits(want), "{p} {:e} {:e} {:e}: {got:e} {want:e}", v[0][i], v[1][i], v[2][i]);
        }
    }
}

#[test]
fn floats() {
    let ftz = |x: f32| f32::from_bits(fp::ftz(x.to_bits()));
    let sat = |x: f32| if x.is_nan() { 0.0 } else { x.clamp(0.0, 1.0) + 0.0 };
    checkf(ab(0x5c58, &[]), |a, b, _| a + b);
    checkf(ab(0x5c58, &[(45, 1, 1)]), |a, b, _| a - b);
    checkf(ab(0x5c58, &[(46, 1, 1), (48, 1, 1)]), |a, b, _| -a.abs() + b);
    checkf(ab(0x5c58, &[(49, 1, 1), (45, 1, 1)]), |a, b, _| a - b.abs());
    checkf(ab(0x5c58, &[(44, 1, 1)]), |a, b, _| ftz(ftz(a) + ftz(b)));
    checkf(ab(0x5c58, &[(50, 1, 1)]), |a, b, _| sat(a + b));
    checkf(word(0x3858, &[(20, 19, 0x40000), (56, 1, 1)]), |a, _, _| a - 2.0);
    checkf(word(0x0800, &[(20, 32, 0x3e99_999a)]), |a, _, _| a + 0.3);
    checkf(ab(0x5c68, &[]), |a, b, _| a * b);
    checkf(ab(0x5c68, &[(48, 1, 1), (41, 3, 5)]), |a, b, _| -(a * b) * 4.0);
    checkf(ab(0x5c68, &[(44, 2, 2)]), |a, b, _| {
        if ftz(a) == 0.0 || ftz(b) == 0.0 { 0.0 } else { ftz(a * b) }
    });
    checkf(word(0x1e00, &[(20, 32, 0x404c_cccd)]), |a, _, _| a * 3.2);
    checkf(abc(0x5980, &[]), |a, b, c| a.mul_add(b, c));
    checkf(abc(0x5980, &[(48, 2, 3), (50, 1, 1)]), |a, b, c| sat((-a).mul_add(b, -c)));
    checkf(abc(0x5980, &[(53, 2, 2)]), |a, b, c| {
        let (a, b, c) = (ftz(a), ftz(b), ftz(c));
        if a == 0.0 || b == 0.0 { c } else { ftz(a.mul_add(b, c)) }
    });
    checkf(word(0x0c00, &[(20, 32, 0x3e99_999a)]), |a, _, _| a * 0.3 + 0.0);
    checkf(ab(0x5c60, &[(39, 3, 7)]), |a, b, _| {
        if a == b { f32::from_bits(a.to_bits() | b.to_bits()) } else { a.min(b) }
    });
    checkf(ab(0x5c60, &[(39, 4, 15)]), |a, b, _| {
        if a == b { f32::from_bits(a.to_bits() & b.to_bits()) } else { a.max(b) }
    });
    // FCMP picks by comparing the third operand with zero
    checkf(abc(0x5ba0, &[(48, 4, 4)]), |a, b, c| if c > 0.0 { a } else { b });
    checkf(abc(0x5ba0, &[(48, 4, 9)]), |a, b, c| if c < 0.0 || c.is_nan() { a } else { b });
    // FSET.BF and all comparisons
    for cond in 0..16u64 {
        checkf(ab(0x5800, &[(52, 1, 1), (39, 3, 7), (48, 4, cond)]), |a, b, _| {
            let k = if a < b { 1 } else if a == b { 2 } else if a > b { 4 } else { 8 };
            (cond & k != 0) as u32 as f32
        });
    }
    checkf(word(0x5080, &[(20, 4, 4)]), |a, _, _| 1.0 / a);
    checkf(word(0x5080, &[(20, 4, 8), (46, 1, 1)]), |a, _, _| a.abs().sqrt());
    checkf(word(0x5ca8, &[(20, 8, 1), (8, 2, 2), (10, 2, 2), (42, 1, 1)]), |_, b, _| b.round_ties_even());
    checkf(word(0x5ca8, &[(20, 8, 1), (8, 2, 2), (10, 2, 2), (42, 1, 1), (39, 2, 1)]), |_, b, _| b.floor());
    checkf(word(0x5ca8, &[(20, 8, 1), (8, 2, 2), (10, 2, 2), (42, 1, 1), (39, 2, 2), (45, 1, 1)]), |_, b, _| (-b).ceil());
    checkf(word(0x5ca8, &[(20, 8, 1), (8, 2, 2), (10, 2, 2), (50, 1, 1)]), |_, b, _| sat(b));
}

#[test]
fn conversions() {
    let f = |r: [f32; 4]| r.map(f32::to_bits);
    let x = f([2.5, -3.7, f32::NAN, 1e20]);
    let l = exec(
        &[
            word(0x5cb0, &[(0, 8, 1), (20, 8, 0), (8, 2, 2), (10, 2, 2), (12, 1, 1), (39, 2, 3)]), // s32 trunc
            word(0x5cb0, &[(0, 8, 2), (20, 8, 0), (8, 2, 2), (10, 2, 2), (39, 2, 0)]), // u32 nearest
            word(0x5cb0, &[(0, 8, 3), (20, 8, 0), (8, 2, 1), (10, 2, 2), (39, 2, 0)]), // u16
            word(0x5cb0, &[(0, 8, 4), (20, 8, 0), (8, 2, 2), (10, 2, 2), (12, 1, 1), (39, 2, 1)]), // s32 floor
        ],
        &[(0, x)],
    );
    assert_eq!(l.gpr[1], [2, -3i32 as u32, 0, i32::MAX as u32]);
    assert_eq!(l.gpr[2], [2, 0, 0, u32::MAX]);
    assert_eq!(l.gpr[3], [2, 0, 0, 0xffff]);
    assert_eq!(l.gpr[4], [2, -4i32 as u32, 0, i32::MAX as u32]);

    let x = [0x8001_7fff, 0xff80_0102, 0xffff_ffff, 0x0000_8000];
    let i2f = |d: u64, sel: u64, size: u64, signed: u64| {
        word(0x5cb8, &[(0, 8, d), (20, 8, 0), (8, 2, 2), (10, 2, size), (13, 1, signed), (41, 2, sel)])
    };
    let i2i = |d: u64, sel: u64, size: u64, m: u64| {
        word(0x5ce0, &[(0, 8, d), (20, 8, 0), (8, 2, 2), (10, 2, size), (12, 2, 3), (41, 2, sel), (45, 5, m)])
    };
    let l = exec(
        &[
            i2f(1, 0, 2, 1),
            i2f(2, 0, 2, 0),
            i2f(3, 2, 1, 1),
            i2f(4, 3, 0, 0),
            i2f(5, 1, 0, 1),
            i2i(6, 0, 2, 1),
            i2i(7, 0, 2, 16),
            i2i(8, 2, 1, 0),
        ],
        &[(0, x)],
    );
    for i in 0..4 {
        let v = x[i];
        assert_eq!(l.f32(1, i), v as i32 as f32);
        assert_eq!(l.f32(2, i), v as f32);
        assert_eq!(l.f32(3, i), (v >> 16) as i16 as f32);
        assert_eq!(l.f32(4, i), (v >> 24) as f32);
        assert_eq!(l.f32(5, i), (v >> 8) as i8 as f32);
        assert_eq!(l.gpr[6][i], v.wrapping_neg());
        assert_eq!(l.gpr[7][i], (v as i32).unsigned_abs());
        assert_eq!(l.gpr[8][i], (v >> 16) as i16 as u32);
    }

    // halves (the upper one with .H1) and doubles
    let h = [0x3c00_c000, 0x0001_7c00, 0x3555_0000, 0xbc00_4000];
    let l = exec(
        &[
            word(0x5ca8, &[(0, 8, 1), (20, 8, 0), (8, 2, 2), (10, 2, 1)]),
            word(0x5ca8, &[(0, 8, 2), (20, 8, 0), (8, 2, 2), (10, 2, 1), (41, 1, 1)]),
            word(0x5ca8, &[(0, 8, 3), (20, 8, 2), (8, 2, 1), (10, 2, 2)]),
            word(0x5ca8, &[(0, 8, 4), (20, 8, 1), (8, 2, 3), (10, 2, 2)]),
            word(0x5c70, &[(0, 8, 6), (8, 8, 4), (20, 8, 4)]),
            word(0x5ca8, &[(0, 8, 8), (20, 8, 6), (8, 2, 2), (10, 2, 3)]),
            word(0x3880, &[(0, 8, 10), (8, 8, 4), (20, 19, 0x40100)]),
            word(0x5b80, &[(3, 3, 0), (0, 3, 1), (8, 8, 6), (20, 8, 10), (39, 3, 7), (48, 4, 1)]),
        ],
        &[(0, h)],
    );
    for i in 0..4 {
        let (lo, hi) = (fp::half_to_f32(h[i] as u16), fp::half_to_f32((h[i] >> 16) as u16));
        assert_eq!((l.f32(1, i), l.f32(2, i)), (lo, hi));
        assert_eq!(l.gpr[3][i], h[i] >> 16);
        assert_eq!(l.f32(8, i), lo + lo);
        // 0x401 << 52 is 4.0
        let d = f64::from_bits(l.gpr[10][i] as u64 | (l.gpr[11][i] as u64) << 32);
        assert_eq!(d, lo as f64 * 4.0);
        assert_eq!(l.pred[0] >> i & 1 != 0, lo + lo < lo * 4.0);
    }
}

#[test]
fn predicates_and_selection() {
    // p0 = r0 < r1 (u32), p1 = !p0
    let setp = word(0x5b60, &[(3, 3, 0), (0, 3, 1), (8, 8, 0), (20, 8, 1), (39, 3, 7), (49, 3, 1)]);
    let v = [1, 5, 7, 9];
    let w = [2, 5, 100, 3];
    let l = exec(
        &[
            setp,
            // sel r2 = p0 ? r0 : r1
            word(0x5ca0, &[(0, 8, 2), (8, 8, 0), (20, 8, 1), (39, 3, 0)]),
            // (p1) mov r3 = 0x55
            word(0x0100, &[(0, 8, 3), (12, 4, 15), (20, 32, 0x55)]) & !(7 << 16) | 1 << 16,
            // (!p1) mov r4 = r0, lanes 1 and 2 of the quad only
            word(0x5c98, &[(0, 8, 4), (20, 8, 0), (39, 4, 6)]) & !(7 << 16) | 9 << 16,
            // psetp p2 = (p0 xor !p1) or p1; p3 = !(p0 xor !p1) or p1
            word(0x5090, &[(3, 3, 2), (0, 3, 3), (12, 3, 0), (29, 4, 9), (24, 2, 2), (45, 2, 1), (39, 3, 1)]),
            // pset r5 = p0 ? 1.0 : 0.0
            word(0x5088, &[(0, 8, 5), (12, 3, 0), (29, 3, 7), (39, 3, 7), (44, 1, 1)]),
            // vote.any r6 = lanes where p0, p4 = any; vote.all p5
            word(0x50d8, &[(0, 8, 6), (45, 3, 4), (39, 3, 0), (48, 2, 1)]),
            word(0x50d8, &[(0, 8, 255), (45, 3, 5), (39, 3, 0), (48, 2, 0)]),
        ],
        &[(0, v), (1, w)],
    );
    assert_eq!(l.pred[0] & 15, 0b0101);
    assert_eq!(l.pred[1] & 15, 0b1010);
    assert_eq!(l.gpr[2], [1, 5, 7, 3]);
    assert_eq!(l.gpr[3], [0, 0x55, 0, 0x55]);
    assert_eq!(l.gpr[4], [0, 0, 7, 0]);
    // p0 == !p1 everywhere
    assert_eq!(l.pred[2] & 15, 0b1010);
    assert_eq!(l.pred[3] & 15, 15);
    assert_eq!(l.gpr[5], [1.0f32, 0.0, 1.0, 0.0].map(f32::to_bits));
    assert_eq!(l.gpr[6], [5; 4]);
    assert_eq!((l.pred[4] & 15, l.pred[5] & 15), (15, 0));
}

#[test]
fn quads() {
    let f = |r: [f32; 4]| r.map(f32::to_bits);
    let x = f([1.0, 4.0, 10.0, 20.0]);
    let shfl = |d: u64, lane: u64, op: u64| {
        word(0xef10, &[(0, 8, d), (8, 8, 0), (20, 5, lane), (28, 2, 3), (34, 13, 0x1c03), (30, 2, op), (48, 3, 7)])
    };
    let swz = |d: u64, a: u64, ops: u64| word(0x50f8, &[(0, 8, d), (8, 8, a), (20, 8, 0), (28, 8, ops)]);
    let l = exec(
        &[
            shfl(1, 1, 3),
            swz(2, 1, 0x99), // d/dx
            shfl(3, 2, 3),
            swz(4, 3, 0xa5), // d/dy
            shfl(5, 2, 0),   // lane 2 to all
            // What nv50_ir does for explicit derivatives: lane 0 keeps its coordinate (MOV2),
            // the lanes to the right get the derivative added.
            swz(6, 5, 0xcc),
        ],
        &[(0, x)],
    );
    let g = |r: u8| [0, 1, 2, 3].map(|i| l.f32(r, i));
    assert_eq!(g(2), [3.0, 3.0, 10.0, 10.0]);
    assert_eq!(g(4), [9.0, 16.0, 9.0, 16.0]);
    assert_eq!(g(5), [10.0; 4]);
    assert_eq!(g(6), [1.0, 14.0, 10.0, 30.0]);
    // Two quads: SHFL stays within each.
    let p = prog(&[shfl(1, 1, 3)]);
    let mut l = Lanes::<8>::new();
    l.start(&p, 0xff, 0);
    l.gpr[0] = [0, 1, 2, 3, 4, 5, 6, 7];
    run(&p, &mut l, &mut Mem::default(), &mut 100);
    assert_eq!(l.gpr[1], [1, 0, 3, 2, 5, 4, 7, 6]);
}

#[test]
fn sam_runs_the_whole_quad() {
    // (lane != 2) exit; sam; mov r1 = 7; ram; mov r2 = 8
    let ins = [
        word(0xf0c8, &[(0, 8, 0)]),
        word(0x3660, &[(3, 3, 0), (0, 3, 7), (8, 8, 0), (20, 19, 2), (39, 3, 7), (49, 3, 5)]),
        word(0xe300, &[(0, 5, 15)]) & !(7 << 16),
        word(0xe370, &[]) & !(7 << 16),
        word(0x0100, &[(0, 8, 1), (12, 4, 15), (20, 32, 7)]),
        word(0xe380, &[]) & !(7 << 16),
        word(0x0100, &[(0, 8, 2), (12, 4, 15), (20, 32, 8)]),
    ];
    let l = exec(&ins, &[]);
    assert_eq!(l.gpr[1], [7; 4]);
    assert_eq!(l.gpr[2], [0, 0, 8, 0]);
}

#[test]
fn control_flow() {
    let f = |top: u64, fields: &[(u32, u32, u64)], pred: u64| word(top, fields) & !(15 << 16) | pred << 16;
    let iadd = |d: u64, imm: u64| word(0x3810, &[(0, 8, d), (8, 8, d), (20, 19, imm)]);
    let lt = |p: u64, a: u64, imm: u64| {
        word(0x3660, &[(3, 3, p), (0, 3, 7), (8, 8, a), (20, 19, imm), (39, 3, 7), (49, 3, 1)])
    };
    // r0 = lane; loop until r1 >= r0 * 2 + 1; odd iterations are skipped by CONT
    let ins = vec![
        word(0xf0c8, &[(0, 8, 0)]),
        word(0x5c48, &[(0, 8, 3), (8, 8, 0), (20, 8, 4)]), // r3 = r0 << r4 (1)
        iadd(3, 1),
        f(0xe2a0, &[(20, 24, rel(3, 12))], 0), // pbk 12
        f(0xe2b0, &[(20, 24, rel(4, 5))], 0),  // 4: pcnt 5 (the next iteration's test)
        // 5: p0 = r1 >= r3: break
        word(0x5b60, &[(3, 3, 0), (0, 3, 7), (8, 8, 1), (20, 8, 3), (39, 3, 7), (49, 3, 6)]),
        f(0xe340, &[(0, 5, 15)], 0), // (p0) brk
        iadd(1, 1),
        word(0x3840, &[(0, 8, 5), (8, 8, 1), (20, 19, 1)]), // r5 = r1 & 1
        word(0x3660, &[(3, 3, 1), (0, 3, 7), (8, 8, 5), (20, 19, 0), (39, 3, 7), (49, 3, 5)]), // 9: p1 = r5 != 0
        f(0xe240, &[(0, 5, 15), (20, 24, rel(10, 13))], 1), // (p1) bra 13
        f(0xe240, &[(0, 5, 15), (20, 24, rel(11, 14))], 7), // bra 14
        f(0xe240, &[(0, 5, 15), (20, 24, rel(12, 16))], 7), // 12: bra 16 (after the loop)
        f(0xe350, &[(0, 5, 15)], 7),                        // 13: cont
        iadd(2, 1),                                         // 14: count even ones
        f(0xe240, &[(0, 5, 15), (20, 24, rel(15, 4))], 7),  // bra 4
        iadd(6, 1),                                         // 16: all lanes are here
        lt(2, 0, 2),
        f(0xe260, &[(20, 24, rel(18, 21))], 0), // cal 21
        iadd(6, 100),
        f(0xe300, &[(0, 5, 15)], 7),
        f(0xe320, &[(0, 5, 15)], 2), // 21: (p2) ret
        iadd(6, 10),
        f(0xe320, &[(0, 5, 15)], 7),
    ];
    // Odd and even iterations alternate, so there is a PCNT token for every CONT.
    let p = prog(&ins);
    let mut l = Lanes::<4>::new();
    l.start(&p, 15, 0);
    l.gpr[4] = [1; 4];
    assert_eq!(run(&p, &mut l, &mut Mem::default(), &mut 1000), Exit::Done, "{p}");
    assert_eq!(l.gpr[1], [1, 3, 5, 7]);
    assert_eq!(l.gpr[2], [0, 1, 2, 3]);
    assert_eq!(l.gpr[6], [101, 101, 111, 111]);
}

#[test]
fn indirect_branch() {
    let bra = |from: usize, to: usize, pred: u64| {
        word(0xe240, &[(0, 5, 15), (20, 24, rel(from, to))]) & !(15 << 16) | pred << 16
    };
    let mov = |v: u64| word(0x0100, &[(0, 8, 1), (12, 4, 15), (20, 32, v)]);
    // Branches that are never taken make the targets known to the decoder.
    let ins = [
        bra(0, 2, 15),
        word(0xe250, &[(0, 5, 15), (8, 8, 0)]), // brx r0
        mov(1),
        bra(3, 7, 7),
        mov(3),
        bra(5, 7, 7),
        mov(5),
        bra(7, 4, 15),
        bra(8, 6, 15),
    ];
    let off = |n: usize| (at(n) - (at(1) + 8)) as u32;
    let l = exec(&ins, &[(0, [off(4), off(6), off(4), off(2)])]);
    assert_eq!(l.gpr[1], [3, 5, 3, 1]);
}

#[test]
fn kill_and_helpers() {
    let st = word(0xa000, &[(0, 8, 1), (8, 8, 0), (58, 3, 7), (53, 3, 4)]);
    let ins = [
        // p0 = lane 1; (p0) kil; st [r0] = r1; r2 = killed; atom.add r3 = [r0], r1
        word(0xf0c8, &[(0, 8, 4)]),
        word(0x3660, &[(3, 3, 0), (0, 3, 7), (8, 8, 4), (20, 19, 1), (39, 3, 7), (49, 3, 2)]),
        word(0xe330, &[(0, 5, 15)]) & !(7 << 16),
        st,
        word(0xf0c8, &[(0, 8, 2), (20, 8, 0x13)]),
        word(0xed00, &[(0, 8, 3), (8, 8, 0), (20, 8, 1)]),
    ];
    let p = prog(&ins);
    let mut l = Lanes::<4>::new();
    let mut h = Mem { bytes: vec![0; 64], ..Mem::default() };
    l.start(&p, 15, 0b1000);
    l.gpr[0] = [0, 4, 8, 12];
    l.gpr[1] = [1, 2, 3, 4];
    assert_eq!(run(&p, &mut l, &mut h, &mut 1000), Exit::Done);
    assert_eq!(h.bytes[..16], [2, 0, 0, 0, 0, 0, 0, 0, 6, 0, 0, 0, 0, 0, 0, 0]);
    assert_eq!((l.killed(), l.live()), (0b0010, 0b0101));
    assert_eq!(l.gpr[2], [0, !0, 0, !0]);
    assert_eq!(l.gpr[3], [1, 0, 3, 0]);
    // All lanes that count are killed: the program stops there.
    l.start(&p, 0b1010, 0b1000);
    let mut fuel = 1000;
    assert_eq!(run(&p, &mut l, &mut h, &mut fuel), Exit::Done);
    assert_eq!(1000 - fuel, 3);
}

#[test]
fn memory() {
    let p = prog(&[
        // ldc.b32 r1 = c5[r0 + 0x10]; ldc.u8 r2 = c5[r0 + 0x11]; is: c[5 + r3.hi][r3.lo + 4]
        word(0xef90, &[(0, 8, 1), (8, 8, 0), (20, 16, 0x10), (36, 5, 5), (48, 3, 4)]),
        word(0xef90, &[(0, 8, 2), (8, 8, 0), (20, 16, 0x11), (36, 5, 5), (48, 3, 0)]),
        word(0xef90, &[(0, 8, 4), (8, 8, 3), (20, 16, 4), (36, 5, 5), (44, 2, 2), (48, 3, 4)]),
        // l[], negative offset; 64 bits
        word(0xef50, &[(0, 8, 1), (8, 8, 0), (20, 24, 0xff_fffc), (48, 3, 5)]),
        word(0xef40, &[(0, 8, 6), (8, 8, 0), (20, 24, 0xff_fffc), (48, 3, 5)]),
        word(0xef40, &[(0, 8, 8), (8, 8, 0), (20, 24, 0xff_fffd), (48, 3, 1)]),
        // s[]: stores, atomics
        word(0xef58, &[(0, 8, 0), (8, 8, 0), (48, 3, 4)]),
        word(0xec00, &[(0, 8, 9), (8, 8, 255), (20, 8, 10), (30, 22, 8)]),
        word(0xee40, &[(0, 8, 11), (8, 8, 255), (20, 8, 12), (30, 22, 9)]),
        // global: 64-bit address pair, 128-bit load
        word(0xa000, &[(0, 8, 1), (8, 8, 14), (20, 32, 8), (52, 1, 1), (53, 3, 4), (58, 3, 7)]),
        word(0x8000, &[(0, 8, 16), (8, 8, 14), (20, 32, 0xffff_fffc), (52, 1, 1), (53, 3, 6), (58, 3, 7)]),
    ]);
    let mut h = Mem { bytes: vec![0; 256], shared: vec![0; 64], ..Mem::default() };
    h.bytes[100..104].copy_from_slice(&0xabcd_1234u32.to_le_bytes());
    let mut l = Lanes::<4>::new();
    l.set_local_size(64);
    l.start(&p, 15, 0);
    l.gpr[0] = [4, 8, 12, 16];
    l.gpr[3] = [0, 1 << 16 | 8, 2 << 16, 3 << 16];
    l.gpr[10] = [1, 2, 3, 4];
    l.gpr[12] = [0, 7, 7, 1];
    l.gpr[13] = [7, 8, 9, 10];
    l.gpr[14] = [104, 48, 120, 140];
    assert_eq!(run(&p, &mut l, &mut h, &mut 1000), Exit::Done);
    assert_eq!(l.gpr[1], [0x5_0014, 0x5_0018, 0x5_001c, 0x5_0020]);
    assert_eq!(l.gpr[2], [0, 0, 0, 0]);
    assert_eq!(l.gpr[4], [0x5_0004, 0x6_000c, 0x7_0004, 0x8_0004]);
    assert_eq!((l.gpr[6], l.gpr[7]), (l.gpr[1], l.gpr[2]));
    assert_eq!(l.gpr[8], [0; 4]);
    assert_eq!(u32::from_le_bytes(h.shared[32..36].try_into().unwrap()), 10);
    assert_eq!(l.gpr[9], [0, 1, 3, 6]);
    // compare and swap in lane order: 0 -> 7 -> 8, then 7 fails, 1 fails
    assert_eq!(l.gpr[11], [0, 7, 8, 8]);
    assert_eq!(u32::from_le_bytes(h.shared[36..40].try_into().unwrap()), 8);
    assert_eq!(h.bytes[112..116], 0x5_0014u32.to_le_bytes());
    // lane 0 loads 16 bytes from 104 - 4
    assert_eq!(l.gpr[16][0], 0xabcd_1234);
    assert_eq!(l.gpr[19][0], 0x5_0014);
}

/// A tessellation control shader whose invocations read each other's outputs without a barrier
/// instruction: all 32 lanes go through the instructions together.
#[test]
fn lockstep_of_32_lanes() {
    let p = prog(&[
        word(0xf0c8, &[(0, 8, 0)]),
        word(0xeff0, &[(0, 8, 0), (8, 8, 255), (20, 10, 0x40), (39, 8, 255)]),
        // r1 = 31 - lane
        word(0x3812, &[(0, 8, 1), (8, 8, 0), (20, 19, 31)]),
        word(0xefd8, &[(0, 8, 2), (8, 8, 255), (20, 10, 0x40), (39, 8, 1), (32, 1, 1)]),
        word(0xefd8, &[(0, 8, 4), (8, 8, 255), (20, 10, 0x10), (39, 8, 1), (47, 2, 1)]),
    ]);
    let mut l = Lanes::<32>::new();
    let mut h = Mem { attrs: vec![[0; 32]; 32], ..Mem::default() };
    l.start(&p, !0, 0);
    assert_eq!(run(&p, &mut l, &mut h, &mut 1000), Exit::Done);
    for i in 0..32 {
        assert_eq!(l.gpr[2][i], 31 - i as u32);
        assert_eq!((l.gpr[4][i], l.gpr[5][i]), ((i as u32) << 8 | 0x10, (i as u32) << 8 | 0x14));
    }
}

#[test]
fn barriers_fuel_and_faults() {
    let p = prog(&[
        word(0x0100, &[(0, 8, 1), (12, 4, 15), (20, 32, 1)]),
        word(0xf0a8, &[(39, 3, 7), (32, 8, 0x80)]),
        word(0x0100, &[(0, 8, 1), (12, 4, 15), (20, 32, 2)]),
    ]);
    let mut l = Lanes::<4>::new();
    let mut h = Mem::default();
    l.start(&p, 15, 0);
    let mut fuel = 1;
    assert_eq!(run(&p, &mut l, &mut h, &mut fuel), Exit::Timeout);
    fuel = 100;
    assert_eq!(run(&p, &mut l, &mut h, &mut fuel), Exit::Barrier);
    assert_eq!(l.gpr[1], [1; 4]);
    assert_eq!(run(&p, &mut l, &mut h, &mut fuel), Exit::Done);
    assert_eq!(l.gpr[1], [2; 4]);

    let p = Program::decode(&assemble(&[0xffff_ffff_fff7_ffff])).unwrap();
    l.start(&p, 15, 0);
    assert_eq!(run(&p, &mut l, &mut h, &mut fuel), Exit::Fault(8));
    assert!(Program::decode(&[0; 8]).is_err());
}
