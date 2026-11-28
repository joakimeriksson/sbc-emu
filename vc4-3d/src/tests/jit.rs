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

/// A block is not run after its instructions changed in memory between jobs (the worker's slots
/// are kept from job to job, and checked against memory).
#[test]
fn changed_code_is_not_run_again() {
    use crate::asm::*;
    use crate::mem::Mem;
    use crate::qpu::{op, waddr, Code, Env, Qpu};
    struct E;
    impl Env for E {}
    let _switch = crate::jit::SWITCH.lock().unwrap_or_else(|e| e.into_inner());
    let mut ram = vec![0u8; 0x1_0000];
    let mut code = Code::default();
    let result = |ram: &mut Vec<u8>, code: &mut Code, k: u32| {
        // r0 = 1 + 2 (or 1 - 2), r1 = r0 + r0, then the end
        let o = if k == 0 { op::ADD } else { op::SUB };
        let mut p = vec![ldi(waddr::R0, 1), ldi(waddr::R1, 2), nop().add(o, waddr::R0, R0, R1), nop().add(op::ADD, waddr::R1, R0, R0)];
        p.extend_from_slice(&end());
        let b = bytes(&p);
        ram[0x1000..0x1000 + b.len()].copy_from_slice(&b);
        code.clear();
        let mut q = Qpu::new();
        q.start(0x1000, 0x8000);
        q.run(code, &mut Mem::new(ram), &mut E, 1000);
        q.acc[1][0]
    };
    assert_eq!(result(&mut ram, &mut code, 0), 6);
    assert_eq!(result(&mut ram, &mut code, 1), (-2i32) as u32);
    assert_eq!(result(&mut ram, &mut code, 0), 6);
    if simd_jit::available() && crate::jit::enabled() {
        assert!(code.jit.runs() > 0);
    }
}
