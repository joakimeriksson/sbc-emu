use super::check::check;

#[test]
fn random_blocks_match_the_interpreter() {
    let ops = check(0x1234_5678_9abc_def1, 20000).unwrap_or_else(|e| panic!("{e}"));
    // every kind of instruction the JIT takes came up
    for k in ["FAdd", "FMul", "Ffma", "FMnMx", "FSet", "FSetP", "Mufu", "F2F", "F2I", "I2F", "IAdd", "Xmad", "Lop", "Lop3", "Shl", "Shr", "ISetP", "Sel", "Mov"] {
        assert!(ops.get(k).is_some_and(|&n| n > 20), "{k}: {ops:?}");
    }
}
