//! AdvSIMD structure loads and stores (LD1-LD4/ST1-ST4 of multiple structures, one lane, and
//! LD1R-LD4R) compiled by the JIT, against the interpreter: every encoding of both groups
//! (the UNDEFINED ones too), with no offset and post-indexed by the size, by Xm and by the base
//! register itself, on X0 and on SP, at a misaligned address, across a page boundary (with
//! both pages in the micro-TLB) and across the end of RAM (an external abort part-way through).
//! Registers, the base's writeback, memory, faults and the instruction count must match, and
//! the accesses inside a page must run natively.
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

/// One run: warm the load and store micro-TLBs for the pages of x19 and x20, then the
/// instruction under test (base x0 or SP, Xm x1), then `brk #0`.
fn run(w: u32, addr: u64, warm: (u64, u64), sp_base: bool, seed: u64, jit: bool) -> (Cpu, FlatRam, Trap) {
    let mut ram = FlatRam::new(0, RAM);
    let mut g = G(seed | 1);
    for b in ram.mem[0x4000..].iter_mut() { *b = g.next() as u8; }
    // ldr x28, [x19]; ldr x28, [x20]; str x27, [x19, #8]; str x27, [x20, #8]; <w>; brk #0
    let code = [0xf940_027c, 0xf940_029c, 0xf900_067b, 0xf900_069b, w, 0xd420_0000u32];
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
    cpu.el = 1;
    cpu.pc = CODE;
    cpu.vbar[1] = VEC;
    cpu.sys.cpacr_el1 = 3 << 20;
    cpu.x[0] = addr;
    cpu.x[1] = [0x40, 0x123, (-48i64) as u64, 0][(seed % 4) as usize];
    cpu.x[19] = warm.0;
    cpu.x[20] = warm.1;
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
        // (bit 23, Rm, base is SP): no offset, by the size, by x1, by the base (x0)
        match n % 5 { 0 => (0, 0, false), 1 => (1, 31, false), 2 => (1, 1, false), 3 => (1, 0, false), _ => (1, 31, true) }
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

#[test]
fn structure_loads_and_stores_match_the_interpreter() {
    if !jit::available() { return; }
    // the fallback profile is global: off again when the test ends, also on a panic
    struct ProfileOff;
    impl Drop for ProfileOff { fn drop(&mut self) { jit::PROFILE.store(false, std::sync::atomic::Ordering::Relaxed); } }
    let _off = ProfileOff;
    jit::PROFILE.store(true, std::sync::atomic::Ordering::Relaxed);
    let end = RAM as u64;
    // (address, pages warmed): misaligned inside P; across P / P + 1 at several depths (both
    // pages in the micro-TLBs); across the end of RAM
    let places = [(P + 0x13, (P, P)), (P + 0xfd0, (P, P + 0x1000)), (P + 0xff9, (P, P + 0x1000)), (P + 0xfff, (P, P + 0x1000)), (end - 0x18, (end - 0x1000, end - 0x1000))];
    let (mut bad, mut native, mut defined) = (Vec::new(), 0, 0);
    for (k, &(w, sp)) in encodings().iter().enumerate() {
        for (pi, &(addr, warm)) in places.iter().enumerate() {
            let seed = (k * 7 + pi) as u64;
            let before = jit::profile_fallbacks().iter().find(|f| f.0 == w).map_or(0, |f| f.1);
            let i = run(w, addr, warm, sp, seed, false);
            let j = run(w, addr, warm, sp, seed, true);
            let n = jit::profile_fallbacks().iter().find(|f| f.0 == w).map_or(0, |f| f.1) - before;
            if let Some(d) = differences(&i, &j) { bad.push(format!("{w:08x} at {addr:#x}: {d}")); }
            // inside one page and defined (no exception): native, not the interpreter
            if pi == 0 && i.0.esr[1] == 0 {
                defined += 1;
                if n == 0 { native += 1; } else { bad.push(format!("{w:08x} at {addr:#x}: {n} interpreter calls (not native)")); }
            }
        }
    }
    eprintln!("{defined} defined encodings, {native} native");
    assert!(defined > 350, "too few defined encodings: {defined}");
    assert!(bad.is_empty(), "{} differences:\n{}", bad.len(), bad[..bad.len().min(20)].join("\n"));
}
