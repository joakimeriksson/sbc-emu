//! The driver's library of built-in functions (integer division, f64 reciprocal and reciprocal
//! square root), run from Mesa's src/nouveau/codegen/lib/gm107.asm.h. MAXWELL_BUILTINS names
//! that file.

use maxwell_shader::*;

struct Nothing;

impl Host<4> for Nothing {
    fn cbuf(&self, _: u32, _: u32) -> u32 {
        0
    }
}

fn library() -> (Vec<u64>, Vec<u32>) {
    let text = std::fs::read_to_string(std::env::var("MAXWELL_BUILTINS").unwrap()).unwrap();
    let (code, offsets) = text.split_once("builtin_offsets").unwrap();
    let hex = |s: &str| -> Vec<u64> {
        s.split(|c: char| c == ',' || c.is_whitespace())
            .filter_map(|t| t.strip_prefix("0x"))
            .filter(|t| t.len() == 16)
            .map(|t| u64::from_str_radix(t, 16).unwrap())
            .collect()
    };
    (hex(code), hex(offsets).into_iter().map(|x| x as u32).collect())
}

fn call(p: &Program, regs: [[u32; 4]; 2]) -> [[u32; 4]; 2] {
    let mut l = Lanes::<4>::new();
    l.start(p, 15, 0);
    (l.gpr[0], l.gpr[1]) = (regs[0], regs[1]);
    assert_eq!(run(p, &mut l, &mut Nothing, &mut 10_000), Exit::Done);
    [l.gpr[0], l.gpr[1]]
}

struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0.wrapping_mul(0x2545_f491_4f6c_dd1d)
    }
    /// Values of all magnitudes.
    fn int(&mut self) -> u32 {
        let x = self.next();
        (x as u32) >> (x >> 59)
    }
}

#[test]
#[ignore = "needs Mesa's gm107.asm.h"]
fn division() {
    let (code, offs) = library();
    let mut fetch = |a: u32| code.get(a as usize / 8).copied();
    let udiv = Program::decode_at(offs[0], &mut fetch).unwrap();
    let sdiv = Program::decode_at(offs[1], &mut fetch).unwrap();
    let mut r = Rng(88172645463325252);
    for _ in 0..50_000 {
        let a: [u32; 4] = std::array::from_fn(|_| r.int());
        let b: [u32; 4] = std::array::from_fn(|_| r.int().max(1));
        let [q, m] = call(&udiv, [a, b]);
        for i in 0..4 {
            assert_eq!((q[i], m[i]), (a[i] / b[i], a[i] % b[i]), "{} / {}", a[i], b[i]);
        }
        let a = a.map(|x| if x & 4 != 0 { x.wrapping_neg() } else { x });
        let b = b.map(|x| if x & 8 != 0 { x.wrapping_neg() } else { x });
        let [q, m] = call(&sdiv, [a, b]);
        for i in 0..4 {
            let (x, y) = (a[i] as i32, b[i] as i32);
            let want = (x.wrapping_div(y), x.wrapping_rem(y));
            assert_eq!((q[i] as i32, m[i] as i32), want, "{x} / {y}");
        }
    }
}

#[test]
#[ignore = "needs Mesa's gm107.asm.h"]
fn reciprocals() {
    let (code, offs) = library();
    let mut fetch = |a: u32| code.get(a as usize / 8).copied();
    let rcp = Program::decode_at(offs[2], &mut fetch).unwrap();
    let rsq = Program::decode_at(offs[3], &mut fetch).unwrap();
    let mut r = Rng(1234567);
    let special = [0.0, -0.0, 1.0, f64::INFINITY, f64::NAN, f64::MIN_POSITIVE, 5e-324, f64::MAX];
    let f = |p: &Program, x: [f64; 4]| -> [f64; 4] {
        let [lo, hi] = call(
            p,
            [x.map(|v| v.to_bits() as u32), x.map(|v| (v.to_bits() >> 32) as u32)],
        );
        std::array::from_fn(|i| f64::from_bits(lo[i] as u64 | (hi[i] as u64) << 32))
    };
    let close = |a: f64, b: f64| {
        (a.is_nan() && b.is_nan()) || a == b || (a.to_bits() as i64 - b.to_bits() as i64).abs() <= 2
    };
    for k in 0..20_000 {
        // Mixed lanes: special values make them part.
        let x: [f64; 4] = std::array::from_fn(|i| {
            if (k + i) % 5 == 0 {
                special[(k / 5 + i) % 8]
            } else {
                f64::from_bits(r.next())
            }
        });
        let y = f(&rcp, x);
        for i in 0..4 {
            assert!(close(y[i], 1.0 / x[i]), "rcp {:e}: {:e} {:e}", x[i], y[i], 1.0 / x[i]);
        }
        let x = x.map(f64::abs);
        let y = f(&rsq, x);
        for i in 0..4 {
            let want = 1.0 / x[i].sqrt();
            assert!(close(y[i], want), "rsq {:e}: {:e} {:e}", x[i], y[i], want);
        }
    }
}
