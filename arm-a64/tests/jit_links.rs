//! The JIT's parts that its verify mode (no chaining) does not reach, against the interpreter:
//! page-crossing accesses where neighbouring virtual pages are far apart in physical memory,
//! cross-page direct links (re-checked after an ASID switch remaps the target, unlinked by
//! IC IVAU), FPSR kept live in the host register across blocks, refills and interpreter calls,
//! and superblock side exits.
mod common;
use arm_a64::{jit, Cpu};
use common::{assemble, LOAD};
use emu_core::{Core, FlatRam, Trap};

const RAM: usize = 16 << 20;

fn w64(ram: &mut FlatRam, pa: usize, v: u64) { ram.mem[pa..pa + 8].copy_from_slice(&v.to_le_bytes()); }

/// L1 0x100000 -> L2 0x101000; L2[0] -> L3 0x102000 (identity for 0..2M, page `remap` -> `remap_to`);
/// L2[1] -> L3 0x103000: VA 0x200000 + i pages -> PA 0x400000 + (511 - i) pages.
fn tables(ram: &mut FlatRam, base: usize, remap: Option<(u64, u64)>) {
    let (l1, l2, l3a, l3b) = (base, base + 0x1000, base + 0x2000, base + 0x3000);
    w64(ram, l1, l2 as u64 | 3);
    w64(ram, l2, l3a as u64 | 3);
    w64(ram, l2 + 8, l3b as u64 | 3);
    for i in 0..512u64 {
        let mut pa = i << 12;
        let mut ng = 0;
        if let Some((va, to)) = remap { if pa == va { pa = to; ng = 1 << 11; } }
        w64(ram, l3a + 8 * i as usize, pa | 0x703 | ng);
        w64(ram, l3b + 8 * i as usize, (0x400000 + ((511 - i) << 12)) | 0x703 | (1 << 54));
    }
}

const MMU_ON: &str = "
    ldr x0, =0xff
    msr mair_el1, x0
    ldr x0, =0x100000
    msr ttbr0_el1, x0
    ldr x0, =0x80190019
    msr tcr_el1, x0
    mov x0, #(3 << 20)
    msr cpacr_el1, x0
    isb
    mrs x0, sctlr_el1
    orr x0, x0, #1
    msr sctlr_el1, x0
    isb
";

fn run(prog: &str, jit: bool, setup: &dyn Fn(&mut FlatRam), max: u64) -> (Cpu, FlatRam) {
    let mut ram = FlatRam::new(0, RAM);
    ram.load(LOAD, &assemble(prog)).unwrap();
    setup(&mut ram);
    let mut cpu = Cpu::new();
    cpu.brk_to_host = true;
    cpu.ram = ram.mem.as_mut_ptr();
    cpu.ram_len = RAM as u64;
    cpu.el = 1;
    cpu.pc = LOAD;
    cpu.sp = [0x7f_0000; 4];
    if jit {
        cpu.jit = jit::Jit::with(4 << 20, 1);
        cpu.chain_until = arm_a64::UNBOUNDED;
    }
    for _ in 0..max {
        match cpu.step(&mut ram) {
            Ok(()) => {}
            Err(Trap::Brk(0)) => return (cpu, ram),
            Err(t) => panic!("{t:?} at pc {:#x}", cpu.pc),
        }
    }
    panic!("no brk, pc {:#x}", cpu.pc)
}

fn noise(ram: &mut FlatRam, from: usize, to: usize) {
    let mut s = 0x9e37_79b9_7f4a_7c15u64;
    for b in &mut ram.mem[from..to] { s ^= s << 13; s ^= s >> 7; s ^= s << 17; *b = s as u8; }
}

#[test]
fn crossing_accesses_with_scrambled_mapping() {
    let prog = format!("{MMU_ON}
        mov x20, #40
        mov x5, #0
    1:  mov x11, #0x200000
        mov x12, #200
    2:  add x13, x11, #0xff0
        ldr x1, [x13, #9]
        ldp q0, q1, [x13]
        ldr q2, [x13, #8]
        ldrh w3, [x13, #15]
        add x16, x13, #4
        ldp x6, x7, [x16]
        ldr x8, [x13, #-8]
        ldr x9, [x13, #16]
        ldp q3, q4, [x13, #-32]
        ldr w10, [x13, #-3]
        eor x5, x1, x5, ror #7
        add x5, x5, x3
        add x5, x5, x6
        eor x5, x5, x7
        add x5, x5, x8
        add x5, x5, x9
        add x5, x5, x10
        fmov x14, d0
        add x5, x5, x14
        mov x14, v1.d[1]
        eor x5, x5, x14
        mov x14, v2.d[1]
        add x5, x5, x14
        mov x14, v4.d[1]
        add x5, x5, x14
        add x15, x5, x20
        str x15, [x13, #12]
        stp q0, q1, [x16]
        strh w15, [x13, #15]
        stp x15, x5, [x13, #8]
        str q2, [x13, #-7]
        add x11, x11, #0x1000
        subs x12, x12, #1
        b.ne 2b
        subs x20, x20, #1
        b.ne 1b
        brk #0
        .ltorg");
    let setup = |r: &mut FlatRam| { noise(r, 0x400000, 0x600000); tables(r, 0x100000, None); };
    let (i, iram) = run(&prog, false, &setup, 50_000_000);
    let (j, jram) = run(&prog, true, &setup, 50_000_000);
    assert_eq!(j.x[5], i.x[5], "checksum");
    assert!(jram.mem[0x400000..0x600000] == iram.mem[0x400000..0x600000], "memory");
}

#[test]
fn cross_page_link_after_asid_switch() {
    // page 0x90000 maps to PA 0x500000 under ASID 1 and to 0x510000 under ASID 2
    let prog = format!("{MMU_ON}
        ldr x0, =0x100000 | (1 << 48)
        msr ttbr0_el1, x0
        isb
        mov x20, #2000
        mov x5, #0
        mov x6, #0
    1:  bl target
        cmp x20, #1000
        b.ne 3f
        ldr x0, =0x110000 | (2 << 48)
        msr ttbr0_el1, x0
        isb
    3:  subs x20, x20, #1
        b.ne 1b
        brk #0
        .ltorg
        .org 0x10000
    target:
        add x7, x7, #1
        ret");
    let b1 = assemble("add x5, x5, #1\n ret");
    let b2 = assemble("add x6, x6, #1\n ret");
    let setup = move |r: &mut FlatRam| {
        tables(r, 0x100000, Some((0x90000, 0x500000)));
        tables(r, 0x110000, Some((0x90000, 0x510000)));
        r.mem[0x500000..0x500000 + b1.len()].copy_from_slice(&b1);
        r.mem[0x510000..0x510000 + b2.len()].copy_from_slice(&b2);
    };
    let (i, _) = run(&prog, false, &setup, 10_000_000);
    assert_eq!((i.x[5], i.x[6], i.x[7]), (1001, 999, 0), "interpreter");
    let (j, _) = run(&prog, true, &setup, 10_000_000);
    assert_eq!((j.x[5], j.x[6], j.x[7]), (1001, 999, 0), "jit");
}

#[test]
fn fpsr_flags_across_blocks_refills_and_interp() {
    let prog = "
        mov x0, #(3 << 20)
        msr cpacr_el1, x0
        isb
        fmov d1, #1.0
        fmov d2, xzr
        fmov d7, #-1.0
        fmov d16, #3.0
        fmov d17, #5.0
        mov x20, #300
        mov x5, #0
        mov x7, #0
        mov x9, #0x200000
    1:  msr fpsr, xzr
        fdiv d3, d1, d2
        fadd v16.2d, v16.2d, v17.2d
        ldr x8, [x9]
        add x9, x9, #0x1000
        mrs x4, fpsr
        orr x5, x5, x4
        fadd d4, d1, d1
        b 2f
    2:  fsqrt d6, d7
        fmul v17.2d, v17.2d, v16.2d
        ldr x8, [x9, #8]
        add x7, x7, x8
        mrs x6, fpsr
        add x7, x7, x6
        cmp x20, #150
        b.ne 3f
        mov x0, #0x08000000
        msr fpsr, x0
    3:  fdiv d3, d1, d2
        subs x20, x20, #1
        b.ne 1b
        fmov x11, d16
        fmov x12, d17
        mrs x10, fpsr
        // an AArch32 program's FPSCR N, Z, C, V (FPSR 31:28) survive compiled code
        ldr x0, =0xf800009f
        msr fpsr, x0
        fadd d4, d1, d1
        b 4f
    4:  mrs x13, fpsr
        brk #0";
    let setup = |r: &mut FlatRam| noise(r, 0x200000, 0x400000);
    let (i, _) = run(prog, false, &setup, 10_000_000);
    let (j, _) = run(prog, true, &setup, 10_000_000);
    for r in [4, 5, 6, 7, 10, 11, 12, 13] { assert_eq!(j.x[r], i.x[r], "x{r}"); }
    assert_eq!(j.x[13], 0xf800009f);
    assert_eq!((j.fpsr, j.fpsr_nzcv), (i.fpsr, i.fpsr_nzcv), "fpsr");
    assert_eq!((j.fpsr, j.fpsr_nzcv), (0x0800009f, 0xf000_0000));
}

#[test]
fn cross_page_link_unlinked_on_ic_ivau() {
    let prog = "
        mov x20, #2000
        mov x5, #0
        mov x6, #0
        adr x9, target
        ldr w10, newinsn
    1:  bl target
        cmp x20, #1000
        b.ne 3f
        str w10, [x9]
        dc cvau, x9
        dsb ish
        ic ivau, x9
        dsb ish
        isb
    3:  subs x20, x20, #1
        b.ne 1b
        brk #0
    newinsn:
        add x6, x6, #1
        .org 0x3000
    target:
        add x5, x5, #1
        ret";
    let setup = |_: &mut FlatRam| {};
    let (i, _) = run(prog, false, &setup, 10_000_000);
    assert_eq!((i.x[5], i.x[6]), (1001, 999), "interpreter");
    let (j, _) = run(prog, true, &setup, 10_000_000);
    assert_eq!((j.x[5], j.x[6]), (1001, 999), "jit");
    assert!(j.jit.as_ref().unwrap().linked > 0);
}

#[test]
fn superblock_side_exits() {
    let prog = "
        mov x20, #3000
        mov x9, #0x200000
    0:  add x1, x1, #1
        add x2, x2, x1
        ldr x10, [x9, x1, lsl #3]
        tbz x1, #0, 1f
        add x3, x3, #5
        eor x2, x2, x3
        str x2, [x9, x1, lsl #3]
    1:  and x4, x1, #6
        cbnz x4, 2f
        add x5, x5, x2
        adds x6, x6, x10
    2:  cmp x1, x5
        b.hi 3f
        add x7, x7, #1
    3:  adc x8, x8, x2
        subs x20, x20, #1
        b.ne 0b
        brk #0";
    let setup = |r: &mut FlatRam| noise(r, 0x200000, 0x400000);
    let (i, iram) = run(prog, false, &setup, 10_000_000);
    let (j, jram) = run(prog, true, &setup, 10_000_000);
    assert_eq!(j.x, i.x);
    assert_eq!(j.nzcv, i.nzcv);
    assert_eq!(j.instret, i.instret, "instret");
    assert!(jram.mem[0x200000..0x400000] == iram.mem[0x200000..0x400000]);
}
