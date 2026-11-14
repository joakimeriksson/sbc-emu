//! Four cores (PSCI CPU_ON) add to one counter with LDXR/STXR loops; the total must be exact,
//! on one thread (cores in turns) and with a host thread per core, interpreted and JIT-compiled.
use bcm2711::{Machine, Stop};
use std::process::Command;

const N: u64 = 100_000;

fn assemble(src: &str) -> Vec<u8> {
    let dir = std::env::temp_dir().join(format!("bcm2711-smp-{}-{:?}", std::process::id(), std::thread::current().id()));
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("t.s"), src).unwrap();
    let st = Command::new("clang").current_dir(&dir).args(["--target=aarch64-none-elf", "-c", "t.s", "-o", "t.o"]).status().expect("clang");
    assert!(st.success());
    let elf = std::fs::read(dir.join("t.o")).unwrap();
    let b = text(&elf);
    let _ = std::fs::remove_dir_all(&dir);
    b
}

/// The `.text` section of an ELF64 object.
fn text(elf: &[u8]) -> Vec<u8> {
    let u = |o: usize, n: usize| -> usize { (0..n).fold(0, |a, i| a | (elf[o + i] as usize) << (8 * i)) };
    let (shoff, shentsize, shnum, shstrndx) = (u(0x28, 8), u(0x3a, 2), u(0x3c, 2), u(0x3e, 2));
    let sh = |i: usize| shoff + i * shentsize;
    let stroff = u(sh(shstrndx) + 0x18, 8);
    for i in 0..shnum {
        let n = stroff + u(sh(i), 4);
        if elf[n..].starts_with(b".text\0") { let (o, sz) = (u(sh(i) + 0x18, 8), u(sh(i) + 0x20, 8)); return elf[o..o + sz].to_vec(); }
    }
    panic!("no .text");
}

fn program() -> Vec<u8> {
    assemble(&format!(r#"
.text
_start:
    mrs x0, mpidr_el1
    and x0, x0, #0xff
    cbnz x0, work
    mov x19, #1
1:  movz x0, #0x0003
    movk x0, #0xc400, lsl #16
    mov x1, x19
    adr x2, work
    mov x3, #0
    smc #0
    add x19, x19, #1
    cmp x19, #4
    b.ne 1b
work:
    adr x5, counter
    ldr x6, ={N}
2:  ldxr x7, [x5]
    add x7, x7, #1
    stxr w8, x7, [x5]
    cbnz w8, 2b
    subs x6, x6, #1
    b.ne 2b
    adr x9, done
3:  ldaxr x7, [x9]
    add x7, x7, #1
    stlxr w8, x7, [x9]
    cbnz w8, 3b
    mrs x0, mpidr_el1
    and x0, x0, #0xff
    cbnz x0, park
4:  ldar x7, [x9]
    cmp x7, #4
    b.ne 4b
    ldr x0, [x5]
    hlt #1
park:
    wfi
    b park
.ltorg
.balign 64
counter: .quad 0
.balign 64
done: .quad 0
"#))
}

fn run(multi: bool, jit: bool) -> u64 {
    let mut m = Machine::with_cpus(64 << 20, 4);
    m.load(0x8_0000, &program());
    m.boot_linux(0x8_0000, 0);
    m.multi = multi;
    if jit { m.enable_jit(); }
    for _ in 0..20_000 {
        match m.run(2_000_000) {
            Stop::Budget => {}
            Stop::Idle(t) => m.idle_until(t),
            Stop::Hlt(1) => return m.cpus[0].x[0],
            s => panic!("stopped: {s:?}"),
        }
    }
    panic!("no result");
}

#[test]
fn exclusive_counter_one_thread() { assert_eq!(run(false, false), 4 * N); }
#[test]
fn exclusive_counter_threads() { assert_eq!(run(true, false), 4 * N); }
#[test]
fn exclusive_counter_threads_jit() { assert_eq!(run(true, true), 4 * N); }
