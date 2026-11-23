use super::check::check;
use super::*;

#[test]
fn random_blocks_match_the_interpreter() {
    let ops = check(0x1234_5678_9abc_def1, 20000).unwrap_or_else(|e| panic!("{e}"));
    // every kind of instruction the JIT takes came up
    for k in ["FAdd", "FMul", "Ffma", "FMnMx", "FSet", "FSetP", "Mufu", "F2F", "F2I", "I2F", "IAdd", "Xmad", "Lop", "Lop3", "Shl", "Shr", "ISetP", "Sel", "Mov"] {
        assert!(ops.get(k).is_some_and(|&n| n > 20), "{k}: {ops:?}");
    }
}

/// A run longer than a block may be is cut into several, which together do what it does.
#[test]
fn long_runs_are_cut() {
    use crate::exec::{run, Lanes};
    // 4000 LOP3s (36 nodes each): far more than 16-bit ids can count
    let mut x = 0x2545_f491_4f6c_dd1du64;
    let lop3 = loop {
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        let w = x & !(0xf << 16) | 7 << 16;
        let inst = crate::decode::decode(8, w);
        if matches!(inst.op, Op::Lop3 { .. }) && lower(&mut Lower::default(), &inst).is_some() {
            break w;
        }
    };
    let ins: Vec<u64> = (0..4000u64).map(|k| lop3 & !0xff | (k % 20)).collect();
    let p = super::check::program(&ins);
    let code = Code::compile(&p).unwrap();
    assert!(code.blocks.len() > 1);
    assert_eq!(code.blocks.iter().map(|b| b.len).sum::<u32>(), 4000);
    let mut l = Lanes::<LANES>::new();
    l.start(&p, 0xffff, 0);
    for k in 0..255 {
        l.gpr[k] = std::array::from_fn(|i| ((k * 977 + i * 13) as u32).wrapping_mul(0x9e37_79b9));
    }
    let (mut gpr, mut pred) = (l.gpr.clone(), l.pred);
    let frame = Frame::default();
    for b in &code.blocks {
        eval(b, &mut gpr, &mut pred, &frame.cb, 0xffff);
    }
    let mut fuel = 10_000;
    run(&p, &mut l, &mut super::check::Cb, &mut fuel);
    assert!(*gpr == *l.gpr);
}
