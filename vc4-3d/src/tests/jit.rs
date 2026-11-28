//! The QPU JIT against the interpreter (`jit_check`): random programs of the instructions it
//! takes, run both ways from the same state.

#[test]
fn random_programs_match_the_interpreter() {
    let ops = crate::jit_check::check(0x1234_5678_9abc_def1, if cfg!(debug_assertions) { 2000 } else { 20000 }).unwrap_or_else(|e| panic!("{e}"));
    // every add and multiply operation, and load immediates, came up
    for k in ["add1", "add3", "add7", "add8", "add12", "add16", "add24", "add30", "mul1", "mul2", "mul3", "mul7", "ldi0", "ldi1", "ldi3"] {
        assert!(ops.get(k).is_some_and(|&n| n > 20), "{k}: {ops:?}");
    }
}
