//! Loads and stores that straddle a page boundary, compiled by the JIT, against the
//! interpreter. The micro-TLB's hashed sets put page P and P + 1 in one set when P ends in 14
//! one bits (the last page before each 64 MiB), so a crossing access must not be served from
//! the next page's entry.
mod common;
use arm_a64::{jit, Cpu};
use common::{assemble, LOAD};
use emu_core::{Core, FlatRam, Trap};

const RAM: usize = 80 << 20;

const PROG: &str = "
    mov x1, #0x4000000         // the first page past 64 MiB
    movz x2, #0x3ff, lsl #16
    movk x2, #0xfffc           // 4 bytes before it
    movz x6, #0x1234
    mov x5, #0
    mov x20, #2000
loop:
    ldr x3, [x1]               // page P + 1 into the load and store micro-TLBs
    str x3, [x1, #8]
    ldr x4, [x2]               // crosses into P + 1
    ldp x7, x8, [x2, #-8]      // crosses too
    add x5, x5, x4
    eor x5, x5, x7, ror #7
    add x5, x5, x8
    add x6, x6, #1
    str x6, [x2]               // crossing store
    sub x9, x2, #12
    stp x6, x5, [x9]           // crossing pair store
    subs x20, x20, #1
    b.ne loop
    brk #0";

fn run(jit: bool) -> (Cpu, FlatRam) {
    let mut ram = FlatRam::new(0, RAM);
    ram.load(LOAD, &assemble(PROG)).unwrap();
    let mut s = 0x9e37_79b9_7f4a_7c15u64;
    for b in &mut ram.mem[0x3ff_e000..0x400_2000] {
        s ^= s << 13;
        s ^= s >> 7;
        s ^= s << 17;
        *b = s as u8;
    }
    let mut cpu = Cpu::new();
    cpu.brk_to_host = true;
    cpu.ram = ram.mem.as_mut_ptr();
    cpu.ram_len = RAM as u64;
    cpu.el = 1;
    cpu.pc = LOAD;
    cpu.sp = [0x40_0000; 4];
    if jit {
        cpu.jit = jit::Jit::with(1 << 20, 1);
        cpu.chain_until = arm_a64::UNBOUNDED;
    }
    for _ in 0..10_000_000 {
        match cpu.step(&mut ram) {
            Ok(()) => {}
            Err(Trap::Brk(0)) => return (cpu, ram),
            Err(t) => panic!("{t:?} at pc {:#x}", cpu.pc),
        }
    }
    panic!("no brk, pc {:#x}", cpu.pc)
}

#[test]
fn page_crossing_accesses_before_64_mib_match_the_interpreter() {
    let (i, iram) = run(false);
    let (j, jram) = run(true);
    for r in [4, 5, 6, 7, 8] {
        assert_eq!(j.x[r], i.x[r], "x{r}");
    }
    assert!(jram.mem[0x3ff_e000..0x400_2000] == iram.mem[0x3ff_e000..0x400_2000], "memory around 64 MiB");
}
