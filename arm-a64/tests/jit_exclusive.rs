//! Exclusive and acquire/release accesses compiled by the JIT: the same results as the
//! interpreter, run natively once warm, and a correct atomic counter with two host threads
//! (multi-threaded mode: store-exclusive is a host compare-and-swap).
mod common;
use arm_a64::{jit, Cpu};
use common::{assemble, LOAD, RAM};
use emu_core::{Core, FlatRam, Trap};

const N: u64 = 1000;

/// Each iteration: successful LDXR/STXR increments at every size, then store-exclusives that
/// must fail (no reservation, a store to the granule, another granule, CLREX), then LDAR/STLR.
const SINGLE: &str = "
    mov x1, #0x100000          // 64-bit counter
    add x2, x1, #0x40          // byte
    add x11, x1, #0x42         // halfword
    add x12, x1, #0x44         // word
    add x14, x1, #0x200        // another granule
    mov x20, #1000
    mov x21, #0
    mov x22, #0
loop:
1:  ldxr x3, [x1]
    add x3, x3, #1
    stxr w4, x3, [x1]
    cbnz w4, 1b
2:  ldaxrb w5, [x2]
    add w5, w5, #3
    stlxrb w6, w5, [x2]
    cbnz w6, 2b
3:  ldxrh w7, [x11]
    add w7, w7, #5
    stxrh w8, w7, [x11]
    cbnz w8, 3b
4:  ldaxr w9, [x12]
    add w9, w9, #7
    stlxr w10, w9, [x12]
    cbnz w10, 4b
    stxr w13, x3, [x1]         // no reservation
    add x21, x21, x13
    ldxr x3, [x1]
    str xzr, [x1, #8]          // a store to the granule clears it
    stxr w13, x3, [x1]
    add x21, x21, x13
    ldxr x3, [x1]
    stxr w13, x3, [x14]        // another granule
    add x21, x21, x13
    ldaxr x3, [x1]
    clrex
    stlxr w13, x3, [x1]
    add x21, x21, x13
    ldar x15, [x1]
    stlr x15, [x14, #0]
    ldar w16, [x14]
    add x22, x22, x16
    subs x20, x20, #1
    b.ne loop
    brk #0";

/// Two cores add to one counter `N` times each.
const SHARED: &str = "
    mov x1, #0x100000
    mov x20, #1000
1:  ldaxr x3, [x1]
    add x3, x3, #1
    stlxr w4, x3, [x1]
    cbnz w4, 1b
    subs x20, x20, #1
    b.ne 1b
    brk #0";

fn core(ram: *mut u8, jit: bool, mt: bool) -> Cpu {
    let mut cpu = Cpu::new();
    cpu.brk_to_host = true;
    cpu.ram = ram;
    cpu.ram_len = RAM as u64;
    cpu.el = 1;
    cpu.pc = LOAD;
    cpu.sp = [0x40_0000, 0x50_0000, 0x60_0000, 0x70_0000];
    cpu.sys.cpacr_el1 = 3 << 20;
    cpu.mt = mt;
    if jit {
        cpu.jit = jit::Jit::with(1 << 20, 1);
        cpu.chain_until = u64::MAX;
    }
    cpu
}

fn run(cpu: &mut Cpu, bus: &mut FlatRam) {
    for _ in 0..10_000_000 {
        match cpu.step(bus) {
            Ok(()) => {}
            Err(Trap::Brk(0)) => return,
            Err(t) => panic!("{t:?} at pc {:#x}", cpu.pc),
        }
    }
    panic!("no brk, pc {:#x}", cpu.pc);
}

fn single(jit: bool, mt: bool) -> (Cpu, FlatRam) {
    let mut ram = FlatRam::new(0, RAM);
    ram.load(LOAD, &assemble(SINGLE)).unwrap();
    let mut cpu = core(ram.mem.as_mut_ptr(), jit, mt);
    run(&mut cpu, &mut ram);
    (cpu, ram)
}

fn le(ram: &FlatRam, a: usize, n: usize) -> u64 {
    ram.mem[a..a + n].iter().rev().fold(0, |v, &b| (v << 8) | b as u64)
}

#[test]
fn exclusives_match_the_interpreter_and_run_natively() {
    let (want, want_ram) = single(false, false);
    assert_eq!(le(&want_ram, 0x100000, 8), N);
    assert_eq!(le(&want_ram, 0x100040, 1), (3 * N) & 0xff);
    assert_eq!(le(&want_ram, 0x100042, 2), 5 * N);
    assert_eq!(le(&want_ram, 0x100044, 4), 7 * N);
    assert_eq!(want.x[21], 4 * N, "every store-exclusive without the reservation fails");
    if !jit::available() {
        return;
    }
    jit::PROFILE.store(true, std::sync::atomic::Ordering::Relaxed);
    for mt in [false, true] {
        let (got, got_ram) = single(true, mt);
        assert_eq!(got.x, want.x, "registers (mt = {mt})");
        assert_eq!(got_ram.mem[0x100000..0x100300], want_ram.mem[0x100000..0x100300], "memory (mt = {mt})");
        assert_eq!(got.excl(), None);
    }
    jit::PROFILE.store(false, std::sync::atomic::Ordering::Relaxed);
    // warm blocks run these natively: the interpreter only sees the first, cold passes
    let excl = |i: u32| (i >> 24) & 0x3f == 0b001000 || i & 0xffff_f0ff == 0xd503_305f;
    let slow: u64 = jit::profile_fallbacks().iter().filter(|f| excl(f.0)).map(|f| f.1).sum();
    assert!(slow < 100, "{slow} exclusive/ordered accesses went to the interpreter");
    // two host threads on one counter: the compare-and-swap path
    let mut ram = FlatRam::new(0, RAM);
    ram.load(LOAD, &assemble(SHARED)).unwrap();
    let base = ram.mem.as_mut_ptr() as usize;
    let threads: Vec<_> = (0..2)
        .map(|_| {
            std::thread::spawn(move || {
                // the bus is only reached for non-RAM addresses; guest RAM is the shared buffer
                let mut bus = FlatRam::new(0, 0x1000);
                let mut cpu = core(base as *mut u8, true, true);
                run(&mut cpu, &mut bus);
            })
        })
        .collect();
    for t in threads {
        t.join().unwrap();
    }
    assert_eq!(le(&ram, 0x100000, 8), 2 * N);
}
