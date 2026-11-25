//! AdvSIMD structure loads and stores (LD1-LD4/ST1-ST4 of multiple structures, one lane, and
//! LD1R-LD4R) compiled by the JIT, against the interpreter: every encoding of both groups
//! (the UNDEFINED ones too), with no offset and post-indexed by the size, by Xm and by the base
//! register itself, on X0 and on SP, at a misaligned address, across a page boundary (with
//! both pages in the micro-TLB), across the end of RAM (an external abort part-way through),
//! ending exactly at the end of RAM, and one byte past it. Then at EL0; with all 16 V cache
//! registers dirty before the access and its registers and base used after it; with the MMU on
//! and a read-only page (stores refill, then take a Permission fault); and with a reservation
//! held (in the granule stored to, or next to it). Registers, the base's writeback, memory,
//! faults, the exclusive monitor and the instruction count must match, and on AArch64 hosts the
//! accesses that fit in a page must run natively.
use arm_a64::{jit, Cpu};
use emu_core::{Core, FlatRam, Trap};

const RAM: usize = 0x1_0000;
const CODE: u64 = 0x1000;
const VEC: u64 = 0x2000;
/// the data pages: P and P + 1
const P: u64 = 0x8000;

struct G(u64);
impl G {
    fn next(&mut self) -> u64 { self.0 ^= self.0 << 13; self.0 ^= self.0 >> 7; self.0 ^= self.0 << 17; self.0 }
}

/// How a case runs: the exception level, the MMU (identity map, page P + 1 read-only), a
/// reservation held at that address before the access (STXR after it), 16 dirty V registers.
#[derive(Clone, Copy, Default)]
struct Env { el0: bool, mmu: bool, excl: Option<u64>, pressure: bool }

/// MMU on (x25 is free): MAIR, TTBR0 = TABLES, TCR (4 KiB granule, 39-bit VA), SCTLR.M.
const MMU_ON: [u32; 12] = [
    0xd280_1ff9, 0xd518_a219, // movz x25, #0xff; msr mair_el1, x25
    0xd28a_0019, 0xd518_2019, // movz x25, #0x5000; msr ttbr0_el1, x25
    0xd280_0339, 0xf2b0_0339, 0xd518_2059, // tcr_el1 = 0x80190019
    0xd503_3fdf, 0xd538_1019, 0xb240_0339, 0xd518_1019, 0xd503_3fdf, // isb; sctlr_el1 |= 1; isb
];
const TABLES: usize = 0x5000;

/// Warm the load and store micro-TLBs for the pages of x19 and x20 (the store to x20's page only
/// without the MMU: it is read-only with it), then the instruction under test (base x0 or SP,
/// Xm x1), then `brk #0`.
fn program(w: u32, env: Env) -> Vec<u32> {
    let mut c = Vec::new();
    if env.mmu { c.extend(MMU_ON); }
    // ldr x28, [x19]; ldr x28, [x20]; str x27, [x19, #8]; str x27, [x20, #8] (or a nop)
    c.extend([0xf940_027c, 0xf940_029c, 0xf900_067b, if env.mmu { 0xd503_201f } else { 0xf900_069b }]);
    if env.excl.is_some() { c.push(0xc85f_7eba); } // ldxr x26, [x21]
    // eor vN.16b, vN.16b, v31.16b for v8..v23: every V cache register dirty
    if env.pressure { c.extend((8..24).map(|n| 0x6e20_1c00 | (31 << 16) | (n << 5) | n)); }
    c.push(w);
    if env.pressure {
        // the access's registers (v22..v25) and its base again: eor v22+k, v22+k, v8; add x2,
        // x0, #1; str q22, [x19, #64]; mov x3, sp
        c.extend((22..26).map(|n| 0x6e20_1c00 | (8 << 16) | (n << 5) | n));
        c.extend([0x9100_0402, 0x3d80_1276, 0x9100_03e3]);
    }
    if env.excl.is_some() { c.push(0xc819_7ebb); } // stxr w25, x27, [x21]
    c.push(0xd420_0000);
    c
}

fn run(w: u32, addr: u64, warm: (u64, u64), sp_base: bool, seed: u64, env: Env, jit: bool) -> (Cpu, FlatRam, Trap) {
    let mut ram = FlatRam::new(0, RAM);
    let mut g = G(seed | 1);
    for b in ram.mem[0x4000..].iter_mut() { *b = g.next() as u8; }
    if env.mmu {
        // identity for the 16 pages of RAM; page P + 1 read-only (AP[2])
        let w64 = |ram: &mut FlatRam, a: usize, v: u64| ram.mem[a..a + 8].copy_from_slice(&v.to_le_bytes());
        w64(&mut ram, TABLES, (TABLES + 0x1000) as u64 | 3);
        w64(&mut ram, TABLES + 0x1000, (TABLES + 0x2000) as u64 | 3);
        for p in 0..16u64 { w64(&mut ram, TABLES + 0x2000 + 8 * p as usize, (p << 12) | if p << 12 == P + 0x1000 { 0x783 } else { 0x703 }); }
        // (the rest of each table: invalid)
        for t in 0..3 { let base = TABLES + 0x1000 * t; for a in (base + if t == 2 { 128 } else { 8 }..base + 0x1000).step_by(8) { w64(&mut ram, a, 0); } }
    }
    let code = program(w, env);
    ram.load(CODE, &code.iter().flat_map(|w| w.to_le_bytes()).collect::<Vec<u8>>()).unwrap();
    let vecs: Vec<u8> = (0..0x800 / 4).flat_map(|_| 0xd420_0020u32.to_le_bytes()).collect();
    ram.load(VEC, &vecs).unwrap();
    let mut cpu = Cpu::new();
    cpu.brk_to_host = true;
    cpu.ram = ram.mem.as_mut_ptr();
    cpu.ram_len = RAM as u64;
    if jit {
        cpu.jit = jit::Jit::with(1 << 20, 1);
        cpu.chain_until = arm_a64::UNBOUNDED;
    }
    cpu.el = if env.el0 { 0 } else { 1 };
    cpu.pc = CODE;
    cpu.vbar[1] = VEC;
    cpu.sys.cpacr_el1 = 3 << 20;
    cpu.x[0] = addr;
    cpu.x[1] = [0x40, 0x123, (-48i64) as u64, 0][(seed % 4) as usize];
    cpu.x[19] = warm.0;
    cpu.x[20] = warm.1;
    cpu.x[21] = env.excl.unwrap_or(0);
    cpu.x[27] = g.next();
    cpu.sp = [if sp_base { addr } else { 0x4000 }; 4];
    for r in cpu.v.iter_mut() { *r = (g.next() as u128) << 64 | g.next() as u128; }
    for _ in 0..100 {
        match cpu.step(&mut ram) {
            Ok(()) => {}
            Err(t @ Trap::Brk(_)) => return (cpu, ram, t),
            Err(Trap::Exception(..)) => {}
            Err(t) => panic!("{t:?} at {:#x}", cpu.pc),
        }
    }
    panic!("no brk");
}

/// Bytes an encoding accesses (as the interpreter decodes it; UNDEFINED ones get some size).
fn span(w: u32) -> u64 {
    let (q, r, size) = ((w >> 30) & 1, (w >> 21) & 1, (w >> 10) & 3);
    if (w >> 24) & 1 == 0 {
        let regs = match (w >> 12) & 15 { 0b0000 | 0b0010 => 4, 0b0100 | 0b0110 => 3, 0b1000 | 0b1010 => 2, _ => 1 };
        regs * if q == 1 { 16 } else { 8 }
    } else {
        let opc = (w >> 13) & 7;
        let selem = (((opc & 1) << 1) | r) as u64 + 1;
        let scale = match opc >> 1 { 0 => 0, 1 => 1, 2 => if size & 1 == 1 { 3 } else { 2 }, _ => size };
        selem << scale
    }
}
fn differences(i: &(Cpu, FlatRam, Trap), j: &(Cpu, FlatRam, Trap)) -> Option<String> {
    let ((i, iram, it), (j, jram, jt)) = (i, j);
    let mut d = Vec::new();
    if it != jt { d.push(format!("trap: interp {it:?} jit {jt:?}")); }
    for r in 0..31 { if i.x[r] != j.x[r] { d.push(format!("x{r}: interp {:#x} jit {:#x}", i.x[r], j.x[r])); } }
    if i.sp != j.sp { d.push(format!("sp: interp {:x?} jit {:x?}", i.sp, j.sp)); }
    if i.pc != j.pc { d.push(format!("pc: interp {:#x} jit {:#x}", i.pc, j.pc)); }
    if i.instret != j.instret { d.push(format!("instret: interp {} jit {}", i.instret, j.instret)); }
    if (i.esr[1], i.far[1], i.elr[1]) != (j.esr[1], j.far[1], j.elr[1]) {
        d.push(format!("esr/far/elr: interp {:#x}/{:#x}/{:#x} jit {:#x}/{:#x}/{:#x}", i.esr[1], i.far[1], i.elr[1], j.esr[1], j.far[1], j.elr[1]));
    }
    for r in 0..32 { if i.v[r] != j.v[r] { d.push(format!("v{r}: interp {:#x} jit {:#x}", i.v[r], j.v[r])); } }
    if iram.mem != jram.mem {
        let a = (0..RAM).find(|&a| iram.mem[a] != jram.mem[a]).unwrap();
        d.push(format!("memory first differs at {a:#x}"));
    }
    (!d.is_empty()).then(|| d.join("; "))
}

/// Every encoding of both groups with base x0 (or SP) and Xm x1 (or the base itself), the
/// first register varied so that lists wrap from V31 to V0.
fn encodings() -> Vec<(u32, bool)> {
    let mut v = Vec::new();
    let mut n = 0u32;
    let post = |n: u32| -> (u32, u32, bool) {
        // (bit 23, Rm, base is SP): no offset, by the size, by x1, by the base (x0); on SP by the
        // size and by x1
        match n % 6 { 0 => (0, 0, false), 1 => (1, 31, false), 2 => (1, 1, false), 3 => (1, 0, false), 4 => (1, 31, true), _ => (1, 1, true) }
    };
    for l in 0..2u32 {
        for q in 0..2u32 {
            for opc in [0u32, 2, 4, 6, 7, 8, 10, 1, 3] {
                for size in 0..4u32 {
                    let (p, rm, sp) = post(n);
                    let rt = [0, 5, 30, 31][(n / 5 % 4) as usize];
                    v.push(((q << 30) | (0b0011000 << 23) | (p << 23) | (l << 22) | (rm << 16) | (opc << 12) | (size << 10) | (if sp { 31 } else { 0 } << 5) | rt, sp));
                    n += 1;
                }
            }
        }
    }
    for l in 0..2u32 {
        for q in 0..2u32 {
            for r in 0..2u32 {
                for opc in 0..8u32 {
                    for s in 0..2u32 {
                        for size in 0..4u32 {
                            let (p, rm, sp) = post(n);
                            let rt = [0, 5, 30, 31][(n / 5 % 4) as usize];
                            v.push(((q << 30) | (0b0011010 << 23) | (p << 23) | (l << 22) | (r << 21) | (rm << 16) | (opc << 13) | (s << 12) | (size << 10) | (if sp { 31 } else { 0 } << 5) | rt, sp));
                            n += 1;
                        }
                    }
                }
            }
        }
    }
    v
}

/// The AArch64 backend compiles these instructions; elsewhere only the results are compared.
const NATIVE: bool = cfg!(target_arch = "aarch64");

#[test]
fn structure_loads_and_stores_match_the_interpreter() {
    if !jit::available() { return; }
    // the fallback profile is global: off again when the test ends, also on a panic
    struct ProfileOff;
    impl Drop for ProfileOff { fn drop(&mut self) { jit::PROFILE.store(false, std::sync::atomic::Ordering::Relaxed); } }
    let _off = ProfileOff;
    jit::PROFILE.store(true, std::sync::atomic::Ordering::Relaxed);
    let end = RAM as u64;
    let last = (end - 0x1000, end - 0x1000);
    let cross = (P, P + 0x1000);
    let plain = Env::default();
    let (mut bad, mut native, mut defined, mut perm) = (Vec::new(), 0, 0, 0);
    for (k, &(w, sp)) in encodings().iter().enumerate() {
        let n = span(w);
        let rw = |a: u64| Some(a & !7);
        // (address, pages warmed, environment, must run natively when defined)
        let places: Vec<(u64, (u64, u64), Env, bool)> = vec![
            // misaligned inside P; across P / P + 1 at several depths (both pages in the
            // micro-TLBs); across the end of RAM; ending at it exactly; one byte past it
            (P + 0x13, (P, P), plain, true),
            (P + 0xfd0, cross, plain, false),
            (P + 0xff9, cross, plain, false),
            (P + 0xfff, cross, plain, false),
            (end - 0x18, last, plain, false),
            (end - n, last, plain, true),
            (end + 1 - n, last, plain, false),
            // EL0
            (P + 0x13, (P, P), Env { el0: true, ..plain }, true),
            (P + 0x1000 - n, cross, Env { el0: true, ..plain }, true),
            (P + 0xff9, cross, Env { el0: true, ..plain }, false),
            (end + 1 - n, last, Env { el0: true, ..plain }, false),
            // every V cache register dirty before the access
            (P + 0x13, (P, P), Env { pressure: true, ..plain }, true),
            (P + 0xff9, cross, Env { pressure: true, ..plain }, false),
            // MMU on, P + 1 read-only: inside P, inside P + 1, across into P + 1
            (P + 0x13, cross, Env { mmu: true, ..plain }, true),
            (P + 0x1013, cross, Env { mmu: true, ..plain }, false),
            (P + 0xff9, cross, Env { mmu: true, ..plain }, false),
            // a reservation in the granule accessed, and in the one before it
            (P + 0x13, (P, P), Env { excl: rw(P + 0x13), ..plain }, false),
            (P + 0x13, (P, P), Env { excl: Some(P - 0x40), ..plain }, false),
            (P + 0xff9, cross, Env { excl: rw(P + 0xff9), ..plain }, false),
        ];
        for (pi, &(addr, warm, env, fits)) in places.iter().enumerate() {
            let seed = (k * 31 + pi) as u64;
            let w = if env.pressure { (w & !31) | 22 } else { w };
            let before = jit::profile_fallbacks().iter().find(|f| f.0 == w).map_or(0, |f| f.1);
            let i = run(w, addr, warm, sp, seed, env, false);
            let j = run(w, addr, warm, sp, seed, env, true);
            let calls = jit::profile_fallbacks().iter().find(|f| f.0 == w).map_or(0, |f| f.1) - before;
            let what = format!("{w:08x} at {addr:#x} (el0 {}, mmu {}, excl {:x?}, pressure {})", env.el0, env.mmu, env.excl, env.pressure);
            if let Some(d) = differences(&i, &j) { bad.push(format!("{what}: {d}")); }
            if i.0.excl() != j.0.excl() { bad.push(format!("{what}: exclusive monitor: interp {:x?} jit {:x?}", i.0.excl(), j.0.excl())); }
            // inside one page and defined (no exception, a load's page readable or a store's
            // writable): native, not the interpreter (on AArch64 hosts: the x86-64 backend,
            // `jit_x64.rs`, hands these instructions to the interpreter)
            let ro = env.mmu && (w >> 22) & 1 == 0 && addr + n > P + 0x1000;
            if fits && i.0.esr[1] == 0 && !ro {
                defined += 1;
                if calls == 0 { native += 1; } else if NATIVE { bad.push(format!("{what}: {calls} interpreter calls (not native)")); }
            }
            // the cases themselves: the end of RAM aborts, the MMU's read-only page faults stores
            if pi == 6 && i.0.esr[1] >> 26 != 0x25 && i.0.esr[1] >> 26 != 0 { bad.push(format!("{what}: no data abort past the end of RAM (ESR {:#x})", i.0.esr[1])); }
            if pi == 14 && (w >> 22) & 1 == 0 && i.0.esr[1] >> 26 == 0x25 {
                if i.0.esr[1] & 0x3c == 0x0c { perm += 1; } else { bad.push(format!("{what}: not a Permission fault (ESR {:#x})", i.0.esr[1])); }
            }
        }
    }
    eprintln!("{defined} defined cases that fit a page, {native} native; {perm} stores to the read-only page");
    assert!(defined > 2000, "too few defined cases: {defined}");
    assert!(perm > 100, "too few Permission faults: {perm}");
    assert!(bad.is_empty(), "{} differences:\n{}", bad.len(), bad[..bad.len().min(20)].join("\n"));
}
