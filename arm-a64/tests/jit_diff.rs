//! The JIT against the interpreter on random instruction sequences (any host with a JIT): integer
//! data processing with every flag producer and consumer, loads and stores of every addressing
//! form and size (integer and FP/SIMD, pairs, AdvSIMD structures, page crossings), exclusives,
//! forward conditional branches (superblock side exits) and instructions the JIT hands to the
//! interpreter in between.
//! Each sequence runs in a loop (compiled, then linked) from the same random state on both;
//! registers, flags, FP/SIMD registers, memory and the instruction count must match exactly.
//! Half of them run with the MMU on and the data pages mapped scrambled, next to a second
//! region whose pages compete for the same micro-TLB sets (refills, way-1 hits and misses).
use arm_a64::{jit, Cpu};
use emu_core::{Bus, Core, FlatRam, Trap};

const RAM: usize = 4 << 20;
const CODE: u64 = 0x8_0000;
const VEC: u64 = 0x7_0000;
const DATA: u64 = 0x10_0000;
const DATA_LEN: usize = 0x2_0000;
/// a second data region (base x24) whose pages share micro-TLB sets with DATA's
const ALIAS: u64 = 0x18_0000;
const STACK: u64 = 0x1c_0000;
/// page tables (MMU cases): L1, L2, and L3 for VA 0..2 MiB and 2..4 MiB
const TABLES: usize = 0x30_0000;

struct G(u64);
impl G {
    fn next(&mut self) -> u64 { self.0 ^= self.0 << 13; self.0 ^= self.0 >> 7; self.0 ^= self.0 << 17; self.0 }
    fn below(&mut self, n: u32) -> u32 { (self.next() % n as u64) as u32 }
    fn bit(&mut self) -> u32 { (self.next() & 1) as u32 }
    /// a data register x0..x18
    fn d(&mut self) -> u32 { self.below(19) }
    /// a data register, sometimes the zero register
    fn dz(&mut self) -> u32 { if self.below(8) == 0 { 31 } else { self.d() } }
    fn value(&mut self) -> u64 {
        const EDGES: [u64; 16] = [0, 1, 2, 0x7f, 0x80, 0xff, 0x7fff_ffff, 0x8000_0000, 0xffff_ffff, 0x1_0000_0000, i64::MAX as u64, 1 << 63, u64::MAX, 63, 64, 32];
        match self.below(3) { 0 => EDGES[self.below(16) as usize], 1 => self.next() & 0xffff, _ => self.next() }
    }
}

/// DecodeBitMasks' validity (the logical immediate is not reserved).
fn bitmask_ok(sf: u32, n: u32, imms: u32) -> bool {
    let combined = (n << 6) | (!imms & 0x3f);
    if combined == 0 { return false; }
    let len = 31 - combined.leading_zeros();
    if len < 1 || (sf == 0 && len > 5) { return false; }
    let levels = (1u32 << len) - 1;
    imms & levels != levels
}

/// One random instruction at index `k` of `n` (forward branches stay inside [k + 1, n]).
fn insn(g: &mut G, k: usize, n: usize) -> u32 {
    let sf = g.bit();
    let w = if sf == 1 { 64 } else { 32 };
    let (rd, rn, rm) = (g.d(), g.dz(), g.dz());
    match g.below(41) {
        39 | 40 => ldst_struct(g),
        0 | 1 => {
            // ADD/SUB (immediate); S forms may compare (rd = ZR), sources may be SP
            let s = g.bit();
            let rd = if s == 1 && g.below(3) == 0 { 31 } else { rd };
            let rn = if g.below(6) == 0 { 31 } else { g.d() };
            (sf << 31) | (g.bit() << 30) | (s << 29) | (0b100010 << 23) | (g.bit() << 22) | (g.below(4096) << 10) | (rn << 5) | rd
        }
        2 => loop {
            // logical (immediate)
            let (opc, nb, immr, imms) = (g.below(4), if sf == 1 { g.bit() } else { 0 }, g.below(64), g.below(64));
            if !bitmask_ok(sf, nb, imms) || (sf == 0 && immr >= 32) { continue; }
            let rd = if opc == 3 && g.below(3) == 0 { 31 } else { rd };
            break (sf << 31) | (opc << 29) | (0b100100 << 23) | (nb << 22) | (immr << 16) | (imms << 10) | (rn << 5) | rd;
        },
        3 => {
            // move wide
            let opc = [0, 2, 3][g.below(3) as usize];
            let hw = g.below(if sf == 1 { 4 } else { 2 });
            (sf << 31) | (opc << 29) | (0b100101 << 23) | (hw << 21) | ((g.next() as u32 & 0xffff) << 5) | rd
        }
        4 | 5 => {
            // bitfield (SBFM / BFM / UBFM), with the extension aliases often
            let opc = g.below(3);
            let (immr, imms) = if g.below(3) == 0 { (0, [7, 15, 31][g.below(3) as usize] % w) } else { (g.below(w), g.below(w)) };
            (sf << 31) | (opc << 29) | (0b100110 << 23) | (sf << 22) | (immr << 16) | (imms << 10) | (rn << 5) | rd
        }
        6 => (sf << 31) | (0b00100111 << 23) | (sf << 22) | (rm << 16) | (g.below(w) << 10) | (rn << 5) | rd, // EXTR
        7 | 8 => {
            // logical (shifted register): MOV/MVN aliases, TST
            let opc = g.below(4);
            let rn = if g.below(4) == 0 { 31 } else { rn };
            let amt = if g.below(2) == 0 { 0 } else { g.below(w) };
            let rd = if opc == 3 && g.below(3) == 0 { 31 } else { rd };
            (sf << 31) | (opc << 29) | (0b01010 << 24) | (g.below(4) << 22) | (g.bit() << 21) | (rm << 16) | (amt << 10) | (rn << 5) | rd
        }
        9 | 10 => {
            // add/sub (shifted register)
            let s = g.bit();
            let rd = if s == 1 && g.below(3) == 0 { 31 } else { rd };
            let amt = if g.below(2) == 0 { g.below(4) } else { g.below(w) };
            (sf << 31) | (g.bit() << 30) | (s << 29) | (0b01011 << 24) | (g.below(3) << 22) | (rm << 16) | (amt << 10) | (rn << 5) | rd
        }
        11 => {
            // add/sub (extended register), base may be SP
            let s = g.bit();
            let rd = if s == 1 && g.below(3) == 0 { 31 } else { rd };
            let rn = if g.below(4) == 0 { 31 } else { g.d() };
            (sf << 31) | (g.bit() << 30) | (s << 29) | (0b01011 << 24) | (1 << 21) | (rm << 16) | (g.below(8) << 13) | (g.below(5) << 10) | (rn << 5) | rd
        }
        12 => (sf << 31) | (g.bit() << 30) | (g.bit() << 29) | (0b11010000 << 21) | (rm << 16) | (rn << 5) | rd, // ADC/SBC(S)
        13 | 14 => {
            // CCMP / CCMN (register or immediate)
            let imm = g.bit();
            (sf << 31) | (g.bit() << 30) | (1 << 29) | (0b11010010 << 21) | ((if imm == 1 { g.below(32) } else { rm }) << 16) | (g.below(16) << 12) | (imm << 11) | (rn << 5) | g.below(16)
        }
        15 | 16 => (sf << 31) | (g.bit() << 30) | (0b11010100 << 21) | (rm << 16) | (g.below(16) << 12) | (g.bit() << 10) | (rn << 5) | rd, // CSEL family
        17 => {
            // UDIV/SDIV, LSLV/LSRV/ASRV/RORV
            let op = [2, 3, 8, 9, 10, 11][g.below(6) as usize];
            (sf << 31) | (0b11010110 << 21) | (rm << 16) | (op << 10) | (rn << 5) | rd
        }
        18 => {
            // RBIT, REV16, REV32/REV, REV, CLZ, CLS
            let op = loop { let op = g.below(6); if op != 3 || sf == 1 { break op; } };
            (sf << 31) | (1 << 30) | (0b11010110 << 21) | (op << 10) | (rn << 5) | rd
        }
        19 => loop {
            // MADD/MSUB, SMADDL/SMSUBL, UMADDL/UMSUBL, SMULH, UMULH
            let (op, o0) = ([0, 1, 5, 2, 6][g.below(5) as usize], g.bit());
            if op != 0 && sf == 0 { continue; }
            if (op == 2 || op == 6) && o0 == 1 { continue; }
            let ra = if op == 2 || op == 6 { 31 } else { g.dz() };
            break (sf << 31) | (0b11011 << 24) | (op << 21) | (rm << 16) | (o0 << 15) | (ra << 10) | (rn << 5) | rd;
        },
        20 => (g.bit() << 31) | (g.below(4) << 29) | (0b10000 << 24) | (g.below(1 << 19) << 5) | rd, // ADR / ADRP
        21..=26 => ldst(g),
        27 => ldst_pair(g),
        28 => {
            // forward conditional branches: B.cond, CBZ/CBNZ, TBZ/TBNZ
            let off = 1 + g.below((n - k).min(9) as u32);
            match g.below(3) {
                0 => (0b0101_0100 << 24) | (off << 5) | g.below(16),
                1 => (sf << 31) | (0b011010 << 25) | (g.bit() << 24) | (off << 5) | rn,
                _ => { let b = g.below(64); ((b >> 5) << 31) | (0b011011 << 25) | (g.bit() << 24) | ((b & 31) << 19) | (off << 5) | rn }
            }
        }
        29 => {
            // exclusives on x23 (aligned): LDXR/STXR, LDAXR/STLXR, LDAR/STLR, CLREX
            let size = g.below(4);
            match g.below(7) {
                0 => (size << 30) | (0b001000 << 24) | (1 << 22) | (31 << 16) | (g.bit() << 15) | (31 << 10) | (23 << 5) | rd,
                1 => (size << 30) | (0b001000 << 24) | (rd << 16) | (g.bit() << 15) | (31 << 10) | (23 << 5) | loop { let t = g.dz(); if t != rd { break t; } },
                2 => (size << 30) | (0b001000 << 24) | (1 << 23) | (1 << 22) | (31 << 16) | (1 << 15) | (31 << 10) | (23 << 5) | rd,
                3 => (size << 30) | (0b001000 << 24) | (1 << 23) | (31 << 16) | (1 << 15) | (31 << 10) | (23 << 5) | rd,
                // a plain store into or next to the reserved granule
                4 => (size << 30) | (0b111001 << 24) | (g.below(128 >> size) << 10) | (23 << 5) | rd,
                // a store pair (W, X or Q) ending in the reserved granule from below
                5 => {
                    // STP W, X or Q (opc 00, 10, 10 with V) at x23 - 1..3 elements
                    let (head, rt, rt2) = match g.below(3) {
                        0 => (0u32, rd, g.dz()),
                        1 => (2 << 30, rd, g.dz()),
                        _ => ((2 << 30) | (1 << 26), g.below(8), g.below(8)),
                    };
                    let imm7 = (-(1 + g.below(3) as i32)) as u32 & 0x7f;
                    head | (0b101 << 27) | (2 << 23) | (imm7 << 15) | (rt2 << 10) | (23 << 5) | rt
                }
                _ => 0xd503_305f,
            }
        }
        30 => {
            // handed to the interpreter: they read and write registers and NZCV in `Cpu`
            match g.below(7) {
                0 => 0xd53b_4200 | rd,                        // mrs xd, nzcv
                1 => 0xd51b_4200 | rd,                        // msr nzcv, xd (ends the block)
                2 => 0x1e60_2800 | (g.below(8) << 16) | (g.below(8) << 5) | g.below(8), // fadd d, d, d
                3 => 0x1ac0_4000 | (rm << 16) | (rn << 5) | rd, // crc32b
                4 => 0xd51b_d040 | rd,                        // msr tpidr_el0, xd
                5 => 0xd53b_d040 | rd,                        // mrs xd, tpidr_el0
                _ => 0x9e67_0000 | (rn << 5) | g.below(8),    // fmov d, xn
            }
        }
        34 => {
            // byte-vector NEON: CMEQ, CMHS, vector logic, UMAXP/UMINP, SHRN (and neighbours);
            // now and then a reserved form (.1D, UMAXP .2D): UNDEFINED, the case ends there
            let (q, size, d, n, m) = (g.bit(), g.below(4), g.below(8), g.below(8), g.below(8));
            let q = if size == 3 && g.below(8) != 0 { 1 } else { q };
            let regs = (m << 16) | (n << 5) | d;
            match g.below(6) {
                0 => 0x2e20_8c00 | (q << 30) | (size << 22) | regs,
                1 => 0x2e20_3c00 | (q << 30) | (size << 22) | regs,
                2 => 0x0e20_1c00 | (q << 30) | (g.bit() << 29) | (g.below(4) << 22) | regs,
                3 => 0x2e20_a400 | (q << 30) | (g.bit() << 11) | ((if g.below(8) == 0 { size } else { size.min(2) }) << 22) | regs,
                4 => 0x0e20_9800 | (q << 30) | (size << 22) | (n << 5) | d,
                _ => 0x0f00_8400 | (q << 30) | ((8 + g.below(8)) << 16) | (n << 5) | d,
            }
        }
        33 => {
            // DUP (general), UMOV/SMOV, MOVI/MVNI/FMOV (vector, immediate), MRS DAIF
            let el = |g: &mut G, sz: u32| (g.below(16 >> sz) << (sz + 1)) | (1 << sz); // imm5: size, index
            match g.below(5) {
                0 => { let sz = g.below(4); 0x0e00_0c00 | ((if sz == 3 { 1 } else { g.bit() }) << 30) | (el(g, sz) << 16) | (rn << 5) | g.below(8) }
                1 => { let sz = g.below(4); 0x0e00_3c00 | (((sz == 3) as u32) << 30) | (el(g, sz) << 16) | (g.below(8) << 5) | rd }
                2 => { let sz = g.below(3); 0x0e00_2c00 | ((if sz == 2 { 1 } else { g.bit() }) << 30) | (el(g, sz) << 16) | (g.below(8) << 5) | rd }
                3 => 0x0f00_0400 | (g.bit() << 30) | (g.bit() << 29) | (g.below(8) << 16) | (g.below(16) << 12) | (g.below(32) << 5) | g.below(8),
                _ => 0xd53b_4220 | rd,
            }
        }
        35 => simd(g),
        31 => [0xd503_201f, 0xd503_3bbf, 0xd503_39bf, 0xd503_3fdf, 0x9e66_0000 | (g.below(8) << 5) | rd][g.below(5) as usize], // nop, dmb ish, dmb ishld, isb, fmov x, d
        _ => {
            // flag producers right before consumers
            let cmp = (sf << 31) | (1 << 30) | (1 << 29) | (0b01011 << 24) | (rm << 16) | (rn << 5) | 31;
            if g.bit() == 0 { cmp } else { (sf << 31) | (0b11010100 << 21) | (rm << 16) | (g.below(16) << 12) | (g.bit() << 10) | (rn << 5) | rd }
        }
    }
}

/// Any FP/SIMD data-processing encoding (vector and scalar, FP and integer, to and from W/X
/// registers 0..7): the AArch64 JIT emits what the interpreter accepts as itself. Encodings the
/// interpreter does not implement are drawn again; reserved ones (UNDEFINED: the case ends
/// there) are kept now and then.
fn simd(g: &mut G) -> u32 {
    thread_local! { static PROBE: std::cell::RefCell<(Cpu, FlatRam)> = std::cell::RefCell::new((Cpu::new(), FlatRam::new(0, 0x1000))); }
    loop {
        let grp = [0b01110u32, 0b01111, 0b11110, 0b11111][g.below(4) as usize];
        let i = (g.below(4) << 29) | (grp << 24) | ((g.next() as u32 & 0x3fff) << 10) | (g.below(8) << 5) | g.below(8);
        let undefined = PROBE.with(|p| {
            let (c, ram) = &mut *p.borrow_mut();
            (c.el, c.pc, c.sys.cpacr_el1, c.vbar[1]) = (1, 0, 3 << 20, 0x800);
            ram.write32(0, i).unwrap();
            match c.step(ram) { Ok(()) if c.pc == 4 => Some(false), Err(Trap::Exception(..)) => Some(true), _ => None }
        });
        match undefined { Some(false) => return i, Some(true) if g.below(8) == 0 => return i, _ => {} }
    }
}

/// A load or store: base x19 (unsigned offset), SP, x22 (pre/post-index: it moves) or x19
/// with register offset x21; integer or FP/SIMD of every size.
fn ldst(g: &mut G) -> u32 {
    let rt = g.dz();
    let vec = g.below(3) == 0;
    // (size, opc) with the scale of the access
    let (size, opc, scale) = if vec {
        match g.below(5) { 4 => (0, 2 | g.bit(), 4), s => (s, g.bit(), s) }
    } else {
        loop {
            let (s, o) = (g.below(4), g.below(4));
            if (s == 3 && o >= 2) || (s == 2 && o == 3) { continue; }
            break (s, o, s);
        }
    };
    let rt = if vec { g.below(8) } else { rt };
    let head = (size << 30) | (0b111 << 27) | ((vec as u32) << 26) | (opc << 22);
    match g.below(4) {
        0 => {
            // unsigned offset from x19, x24 (the other region) or SP
            let rn = [31, 19, 24, 19][g.below(4) as usize];
            head | (1 << 24) | ((g.below(0x1000 >> scale.min(3))) << 10) | (rn << 5) | rt
        }
        1 => {
            // unscaled (often across a page boundary), or unprivileged
            let unpriv = !vec && g.below(4) == 0;
            head | (g.below(512) << 12) | ((if unpriv { 2 } else { 0 }) << 10) | (19 << 5) | rt
        }
        2 => head | (g.below(512) << 12) | ((if g.bit() == 0 { 1 } else { 3 }) << 10) | (22 << 5) | rt, // pre/post-index on x22
        _ => {
            // register offset, extended and scaled index x21
            let option = [2, 3, 6, 7][g.below(4) as usize];
            head | (1 << 21) | (21 << 16) | (option << 13) | (g.bit() << 12) | (0b10 << 10) | (19 << 5) | rt
        }
    }
}

/// An AdvSIMD structure load or store: LD1-LD4/ST1-ST4 of multiple structures, of one lane,
/// LD1R-LD4R (now and then a reserved form: UNDEFINED, the case ends there). Registers v0..v7,
/// sometimes v29..v31 (the list wraps to v0); no offset from x19, x24 (the other region, at
/// times across a page) or SP, or post-indexed on x22 by the size or by x21 (it moves, also
/// across pages), sometimes on x23 next to the reserved granule.
fn ldst_struct(g: &mut G) -> u32 {
    let (q, l) = (g.bit(), g.bit());
    let rt = if g.below(6) == 0 { 29 + g.below(3) } else { g.below(8) };
    let (post, rm, rn) = match g.below(6) {
        0 => (0, 0, [19, 24, 31][g.below(3) as usize]),
        1 | 2 => (1, 31, 22),
        3 | 4 => (1, 21, 22),
        _ => (1, if g.bit() == 0 { 31 } else { 21 }, 23),
    };
    let head = (q << 30) | (post << 23) | (l << 22) | (rm << 16) | (rn << 5) | rt;
    if g.bit() == 0 {
        let opc = if g.below(16) == 0 { g.below(16) } else { [0, 2, 4, 6, 7, 8, 10][g.below(7) as usize] };
        head | (0b0011000 << 23) | (opc << 12) | (g.below(4) << 10)
    } else {
        // single structure: opc 0..5 lanes (b, h, s/d), 6/7 replicate (loads)
        let (opc, s, size) = match g.below(4) {
            0 => (g.bit(), g.bit(), g.below(4)),
            1 => (2 | g.bit(), g.bit(), 2 * g.bit()),
            2 => { let d = g.bit(); (4 | g.bit(), if d == 1 { 0 } else { g.bit() }, d) }
            _ => (6 | g.bit(), if g.below(16) == 0 { 1 } else { 0 }, g.below(4)),
        };
        let l = if opc >= 6 && g.below(16) != 0 { 1 } else { l };
        (head & !(1 << 22)) | (l << 22) | (0b0011010 << 23) | (g.bit() << 21) | (opc << 13) | (s << 12) | (size << 10)
    }
}

fn ldst_pair(g: &mut G) -> u32 {
    let vec = g.below(3) == 0;
    let load = g.bit();
    let opc = if vec { g.below(3) } else { [0, 2, 1][g.below(3) as usize] };
    let opc = if !vec && opc == 1 && load == 0 { 0 } else { opc };
    let (rt, rt2) = loop {
        let (a, b) = if vec { (g.below(8), g.below(8)) } else { (g.dz(), g.dz()) };
        if load == 0 || a != b { break (a, b); }
    };
    let (mode, rn) = match g.below(6) { 0 => (2, 19), 1 => (2, 31), 2 => (if g.bit() == 0 { 1 } else { 3 }, 22), 3 => (2, 24), 4 => (2, 23), _ => (0, 19) };
    let mode = if mode == 0 && opc == 1 && !vec { 2 } else { mode };
    // around x23 (the exclusives' address): a pair that may straddle the reserved granule
    let imm7 = if rn == 23 { (g.below(8) as i32 - 4) as u32 & 0x7f } else { g.below(128) };
    (opc << 30) | (0b101 << 27) | ((vec as u32) << 26) | (mode << 23) | (load << 22) | (imm7 << 15) | (rt2 << 10) | (rn << 5) | rt
}

struct Case { code: Vec<u32>, x: [u64; 31], nzcv: u32, v: [u128; 32], mem: Vec<u8>, mmu: bool, mt: bool }

/// MMU on (x25 is free): MAIR, TTBR0 = TABLES, TCR (4 KiB granule, 39-bit VA), SCTLR.M.
const MMU_ON: [u32; 12] = [
    0xd280_1ff9, 0xd518_a219, // movz x25, #0xff; msr mair_el1, x25
    0xd2a0_0619, 0xd518_2019, // movz x25, #0x30, lsl #16; msr ttbr0_el1, x25
    0xd280_0339, 0xf2b0_0339, 0xd518_2059, // tcr_el1 = 0x80190019
    0xd503_3fdf, 0xd538_1019, 0xb240_0339, 0xd518_1019, 0xd503_3fdf, // isb; sctlr_el1 |= 1; isb
];

/// Identity for 0..4 MiB, except DATA's and ALIAS's pages, scrambled into 2 MiB.. and 2.5 MiB...
/// Code and vectors are EL1-only (EL0-writable pages cannot run at EL1); data EL0 RW too.
fn tables(ram: &mut FlatRam) {
    let w64 = |ram: &mut FlatRam, a: usize, v: u64| ram.mem[a..a + 8].copy_from_slice(&v.to_le_bytes());
    let (l1, l2, l3a, l3b) = (TABLES, TABLES + 0x1000, TABLES + 0x2000, TABLES + 0x3000);
    w64(ram, l1, l2 as u64 | 3);
    w64(ram, l2, l3a as u64 | 3);
    w64(ram, l2 + 8, l3b as u64 | 3);
    for p in 0..1024u64 {
        let va = p << 12;
        let (pa, attr) = if (DATA..DATA + DATA_LEN as u64).contains(&va) {
            (0x20_0000 + (((va - DATA) >> 12) * 7 % 32 << 12), 0x743)
        } else if (ALIAS..ALIAS + DATA_LEN as u64).contains(&va) {
            (0x28_0000 + ((31 - ((va - ALIAS) >> 12)) << 12), 0x743)
        } else if va < 0x8_0000 || (CODE..CODE + 0x1000).contains(&va) || va >= TABLES as u64 {
            (va, 0x703)
        } else {
            (va, 0x743)
        };
        w64(ram, if p < 512 { l3a } else { l3b } + 8 * (p as usize % 512), pa | attr);
    }
}

fn case(seed: u64) -> Case {
    let mut g = G(seed.wrapping_mul(0x9e37_79b9_7f4a_7c15) | 1);
    let n = 8 + g.below(40) as usize;
    // loop: x22 (the moving base) reset each time round; x20 counts down
    let mmu = seed % 2 == 1;
    let mut code = if mmu { MMU_ON.to_vec() } else { Vec::new() };
    code.push(0x9100_0000 | (19 << 5) | 22); // add x22, x19, #0
    for k in 0..n { code.push(insn(&mut g, k, n)); }
    code.push(0xf100_0694); // subs x20, x20, #1
    code.push(0x5400_0001 | ((((-(n as i32 + 2)) as u32) & 0x7ffff) << 5)); // b.ne start
    code.push(0xd420_0000); // brk #0
    let mut x = [0u64; 31];
    for r in x.iter_mut().take(19) { *r = g.value(); }
    x[19] = DATA + 0x8000;
    x[20] = 3;
    x[21] = g.below(64) as u64 * 8 + g.below(8) as u64;
    // now and then misaligned: the exclusives and acquire/release accesses take an Alignment fault
    x[23] = DATA + 0x40 * g.below(8) as u64 + if g.below(16) == 0 { [1, 2, 4, 6, 0xc, 0xe, 0xf][g.below(7) as usize] } else { 0 };
    x[24] = ALIAS + 0x1000 * g.below(16) as u64 + 8 * g.below(512) as u64;
    let mut v = [0u128; 32];
    for r in v.iter_mut() { *r = (g.next() as u128) << 64 | g.next() as u128; }
    // DATA and ALIAS where the MMU is off, and where it maps them
    let mem: Vec<u8> = (0..(TABLES - DATA as usize) / 8).flat_map(|_| g.next().to_le_bytes()).collect();
    // cores on host threads in a third of the cases: host atomics for the exclusives
    Case { code, x, nzcv: (g.below(16)) << 28, v, mem, mmu, mt: seed % 3 == 2 }
}

fn run(c: &Case, jit: bool) -> (Cpu, FlatRam, Trap) {
    let mut ram = FlatRam::new(0, RAM);
    let bytes: Vec<u8> = c.code.iter().flat_map(|w| w.to_le_bytes()).collect();
    ram.load(CODE, &bytes).unwrap();
    // every exception vector: brk #1
    let vecs: Vec<u8> = (0..0x800 / 4).flat_map(|_| 0xd420_0020u32.to_le_bytes()).collect();
    ram.load(VEC, &vecs).unwrap();
    ram.load(DATA, &c.mem).unwrap();
    if c.mmu { tables(&mut ram); }
    let mut cpu = Cpu::new();
    cpu.brk_to_host = true;
    cpu.ram = ram.mem.as_mut_ptr();
    cpu.ram_len = RAM as u64;
    cpu.mt = c.mt;
    if jit {
        cpu.jit = jit::Jit::with(1 << 20, 1);
        cpu.chain_until = arm_a64::UNBOUNDED;
    }
    cpu.el = 1;
    cpu.pc = CODE;
    cpu.vbar[1] = VEC;
    cpu.sp = [STACK, STACK + 0x1000, STACK + 0x2000, STACK + 0x3000];
    cpu.sys.cpacr_el1 = 3 << 20;
    cpu.x = c.x;
    cpu.nzcv = c.nzcv;
    cpu.v = c.v;
    for _ in 0..100_000 {
        match cpu.step(&mut ram) {
            Ok(()) => {}
            Err(t @ Trap::Brk(_)) => return (cpu, ram, t),
            Err(Trap::Exception(..)) => {}
            Err(t) => panic!("{t:?} at {:#x}", cpu.pc),
        }
    }
    panic!("no brk");
}

/// What differs between the interpreter's run and the JIT's (None: nothing).
fn differences(i: (&Cpu, &FlatRam, &Trap), j: (&Cpu, &FlatRam, &Trap)) -> Option<String> {
    let ((i, iram, it), (j, jram, jt)) = (i, j);
    let mut d = Vec::new();
    if it != jt { d.push(format!("trap: interp {it:?} jit {jt:?}")); }
    for r in 0..31 { if i.x[r] != j.x[r] { d.push(format!("x{r}: interp {:#x} jit {:#x}", i.x[r], j.x[r])); } }
    if i.sp != j.sp { d.push(format!("sp: interp {:x?} jit {:x?}", i.sp, j.sp)); }
    if i.pc != j.pc { d.push(format!("pc: interp {:#x} jit {:#x}", i.pc, j.pc)); }
    if i.nzcv != j.nzcv { d.push(format!("nzcv: interp {:#x} jit {:#x}", i.nzcv, j.nzcv)); }
    if i.instret != j.instret { d.push(format!("instret: interp {} jit {}", i.instret, j.instret)); }
    if i.excl() != j.excl() { d.push(format!("exclusive monitor: interp {:x?} jit {:x?}", i.excl(), j.excl())); }
    for r in 0..32 { if i.v[r] != j.v[r] { d.push(format!("v{r}: interp {:#x} jit {:#x}", i.v[r], j.v[r])); } }
    if iram.mem != jram.mem {
        let a = (0..RAM).find(|&a| iram.mem[a] != jram.mem[a]).unwrap();
        d.push(format!("memory first differs at {a:#x}"));
    }
    (!d.is_empty()).then(|| d.join("; "))
}

/// A fixed sequence (then brk #0) from registers `x` (the rest zero), MMU off.
fn fixed(code: &[u32], x: &[(usize, u64)], mt: bool) -> Case {
    let mut c = case(0);
    c.code = code.to_vec();
    c.code.push(0xd420_0000);
    c.x = [0; 31];
    for &(r, v) in x { c.x[r] = v; }
    (c.mmu, c.mt) = (false, mt);
    c
}

fn fixed_differences(c: &Case) -> Option<String> {
    let (i, iram, it) = run(c, false);
    let (j, jram, jt) = run(c, true);
    differences((&i, &iram, &it), (&j, &jram, &jt))
}

/// Stores next to a held reservation: a pair whose second store lands in the reserved
/// granule clears it in the interpreter (two stores), so the store-exclusive after it fails.
#[test]
fn store_pairs_next_to_the_reserved_granule() {
    if !jit::available() { return; }
    let mut bad = Vec::new();
    // ldxr x1, [x19]; stp x2, x3 / q2, q3 / w2, w3, [x20]; stxr w4, x1, [x19]
    for (stp, name) in [(0xa900_0e82u32, "stp x"), (0xad00_0e82, "stp q"), (0x2900_0e82, "stp w"), (0xf900_0282, "str x"), (0x3d80_0282, "str q")] {
        for mt in [false, true] {
            for back in [4u64, 8, 12, 16, 24, 32, 64] {
                let granule = DATA + 0x440;
                let c = fixed(&[0xc85f_7e61, stp, 0xc804_7e61], &[(19, granule), (20, granule - back), (1, 0x55), (2, 0x66), (3, 0x77), (4, 9)], mt);
                if let Some(d) = fixed_differences(&c) { bad.push(format!("{name} at granule - {back} (mt {mt}): {d}")); }
            }
        }
    }
    assert!(bad.is_empty(), "{}", bad.join("\n"));
}

/// SMOV encodings the interpreter takes although they are reserved (X from a doubleword, W
/// from a word) stay with the interpreter.
#[test]
fn reserved_smov_matches_the_interpreter() {
    if !jit::available() { return; }
    for w in [0x0e04_2c25u32, 0x4e08_2c25, 0x0e08_2c25, 0x4e04_2c25, 0x0e01_2c25, 0x4e02_2c25] {
        let mut c = fixed(&[w], &[], false);
        c.v[1] = 0x8081_8283_8485_8687_f8f9_fafb_fcfd_feff;
        assert_eq!(fixed_differences(&c), None, "{w:08x}");
    }
}

/// Exclusive and acquire/release accesses must be aligned to their whole size whatever SCTLR.A
/// says (ARMv8.0: no FEAT_LSE2): at a misaligned address an Alignment fault, in the interpreter
/// and through the JIT, with cores on host threads (`mt`) and without. With `mt` the AArch64
/// backend's own LDAR/STLR, LDAXR/STLXR would raise SIGBUS on the host, the x86-64 one's XCHG and
/// LOCK CMPXCHG would split across cache lines; misaligned ones go to the interpreter. Aligned
/// ones stay native. Each runs in a loop; the store-exclusives after a load-exclusive of the
/// same size from the granule (x21), so they hold the reservation (they fault even without it),
/// and a plain store to the page (it fills the micro-TLB, which `mt`'s host CAS goes around).
#[test]
fn misaligned_exclusives_and_acquire_release_fault_as_in_the_interpreter() {
    if !jit::available() { return; }
    // (instruction on [x19], data x1/x3, status w2; bytes it accesses; compiled natively)
    const FORMS: [(u32, u64, bool, &str); 17] = [
        (0xc89f_fe61, 8, true, "stlr x"), (0x889f_fe61, 4, true, "stlr w"), (0x489f_fe61, 2, true, "stlrh"),
        (0xc8df_fe61, 8, true, "ldar x"), (0x88df_fe61, 4, true, "ldar w"), (0x48df_fe61, 2, true, "ldarh"),
        (0xc85f_fe61, 8, true, "ldaxr x"), (0xc85f_7e61, 8, true, "ldxr x"), (0x885f_7e61, 4, true, "ldxr w"),
        (0x485f_fe61, 2, true, "ldaxrh"), (0xc802_7e61, 8, true, "stxr x"), (0xc802_fe61, 8, true, "stlxr x"),
        (0x4802_7e61, 2, true, "stxrh"), (0x8802_fe61, 4, true, "stlxr w"),
        (0xc87f_0e61, 16, false, "ldxp x"), (0xc822_0e61, 16, false, "stxp x"), (0x887f_8e61, 8, false, "ldaxp w"),
    ];
    const NOP: u32 = 0xd503_201f;
    // the fallback profile is global: off again when the test ends, also on a panic
    struct ProfileOff;
    impl Drop for ProfileOff { fn drop(&mut self) { jit::PROFILE.store(false, std::sync::atomic::Ordering::Relaxed); } }
    let _off = ProfileOff;
    jit::PROFILE.store(true, std::sync::atomic::Ordering::Relaxed);
    let granule = DATA + 0x440;
    let mut bad = Vec::new();
    for (w, size, native, name) in FORMS {
        // store-exclusives: str xzr, [x22]; ldxr{h,,} x5, [x21] (their size); then the access;
        // subs x20, x20, #1; b.ne -16
        let store_excl = w & 0x3fe0_0000 == 0x0800_0000;
        let prefix = if store_excl { [0xf900_02df, (w & 0xc000_0000) | 0x085f_7ea5] } else { [NOP, NOP] };
        let code = [prefix[0], prefix[1], w, 0xf100_0694, 0x54ff_ff81];
        for mt in [false, true] {
            // aligned; inside one 16-byte block (no fault on a host with FEAT_LSE2); across one
            for addr in [granule, granule + 1, granule - 4, granule + 6, granule + 0xf, granule + 0xe] {
                let misaligned = addr % size != 0;
                let what = format!("{name} at granule + {}{} (mt {mt})", addr as i64 - granule as i64, if misaligned { ", misaligned" } else { "" });
                let c = fixed(&code, &[(19, addr), (20, 500), (21, granule), (22, DATA + 0x800), (1, 0x1122_3344_5566_7788), (3, 0x99aa_bbcc_ddee_ff00)], mt);
                let before = jit::profile_fallbacks().iter().find(|f| f.0 == w).map_or(0, |f| f.1);
                let (i, iram, it) = run(&c, false);
                let (j, jram, jt) = run(&c, true);
                if let Some(d) = differences((&i, &iram, &it), (&j, &jram, &jt)) { bad.push(format!("{what}: {d}")); }
                // the interpreter's own run is not counted (only compiled blocks' fallbacks are)
                let n = jit::profile_fallbacks().iter().find(|f| f.0 == w).map_or(0, |f| f.1) - before;
                let (ec, dfsc) = (i.esr[1] >> 26, i.esr[1] & 0x3f);
                if misaligned && (ec != 0x25 || dfsc != 0x21 || i.far[1] != addr || i.elr[1] != CODE + 8) {
                    bad.push(format!("{what}: no Alignment fault in the interpreter (ESR {:#x}, FAR {:#x})", i.esr[1], i.far[1]));
                } else if !misaligned && (i.x[20] != 0 || i.elr[1] != 0) {
                    bad.push(format!("{what}: the loop did not finish (x20 {}, ESR {:#x})", i.x[20], i.esr[1]));
                } else if !misaligned && native && n >= 10 {
                    bad.push(format!("{what}: {n} interpreter calls (not native)"));
                }
            }
        }
    }
    assert!(bad.is_empty(), "{}", bad.join("\n"));
}

/// Reserved FP/SIMD encodings the interpreter used to execute, one of each kind (found by a
/// sweep of the encoding space against LLVM's disassembler, see `simd_decode.rs`): the AArch64
/// JIT, which emits what the interpreter accepts as itself, then ran them natively, and the host
/// raised SIGILL (or, for the half-precision ones, computed something else). UNDEFINED now.
const RESERVED_SIMD: &[u32] = &[
    // the first ones found: UMAXP .2D, CMEQ .1D, CMHS .1D
    0x6ee0_a420, 0x2ee0_8c20, 0x2ee0_3c20,
    // FMOV (scalar, immediate) with imm5 set; FCMP with opcode2 bits 2:0 set
    0x1e20_1020, 0x1e20_2021,
    // scalar two-register misc
    0x5e20_4820, 0x5e20_8820, 0x5e20_9820, 0x5e20_a820, 0x5e20_b820, 0x5ea0_f820, 0x5e21_2820, 0x5e21_6820,
    0x5e21_8820, 0x5e21_9820, 0x7ea1_f820,
    // scalar three same
    0x5e20_0420, 0x5e20_1420, 0x5e20_2420, 0x5e20_3420, 0x5e20_3c20, 0x5e20_4420, 0x5e20_5420, 0x5e20_6420,
    0x5e20_6c20, 0x5e20_7420, 0x5e20_7c20, 0x5e20_8420, 0x5e20_8c20, 0x5e20_9420, 0x5e20_9c20, 0x5e20_b420,
    0x5e20_c420, 0x5e20_cc20, 0x5e20_d420, 0x7e20_dc20, 0x5e20_f420, 0x7e20_fc20,
    // scalar by element
    0x7f40_0020, 0x5fe0_1020, 0x5f40_2020, 0x7f40_4020, 0x5fe0_5020, 0x5f40_6020, 0x5f40_8020, 0x5fe0_9020,
    0x5f40_a020,
    // scalar shift by immediate
    0x5f08_0420, 0x5f08_1420, 0x5f08_2420, 0x5f08_3420, 0x7f08_4420, 0x5f08_5420, 0x7f88_6420, 0x5f88_7420,
    0x5f08_8420, 0x5f08_8c20, 0x5f88_9420, 0x5f88_9c20, 0x5fa0_e420, 0x5fa0_fc20,
    // vector two-register misc
    0x2e20_1820, 0x0ee0_2820, 0x0ee0_4820, 0x0ee0_6820, 0x0ee0_7820, 0x0ee0_8820, 0x0ee0_9820, 0x0ee0_a820,
    0x0ee0_b820, 0x2ee1_3820,
    // vector three different (SQDMLAL, SQDMLSL, SQDMULL of bytes)
    0x0e20_9020, 0x0e20_b020, 0x0e20_d020,
    // vector three same
    0x0ee0_0420, 0x0ee0_0c20, 0x0ee0_1420, 0x0ee0_2420, 0x0ee0_2c20, 0x0ee0_3420, 0x0ee0_3c20, 0x0ee0_4420,
    0x0ee0_4c20, 0x0ee0_5420, 0x0ee0_5c20, 0x0ee0_6420, 0x0ee0_6c20, 0x0ee0_7420, 0x0ee0_7c20, 0x0ee0_8420,
    0x0ee0_8c20, 0x0ee0_9420, 0x0ee0_9c20, 0x0ee0_a420, 0x0ee0_ac20, 0x0e20_b420, 0x0ee0_bc20,
    // across lanes (with the half-precision FMAXNMV/FMAXV, FEAT_FP16, which the host has)
    0x0eb0_3820, 0x0eb0_a820, 0x0e30_c820, 0x0e30_f820, 0x0eb1_a820, 0x0eb1_b820,
    // vector by element (doubles with L set, or of a .1D)
    0x0fc0_1020, 0x0fc0_5020, 0x0fc0_9020,
    // DUP (element, general) .1D, INS (general) with Q clear, SMOV to W from a word, UMOV to W from a doubleword
    0x0e08_0420, 0x0e08_0c20, 0x0e01_1c20, 0x0e04_2c20, 0x0e08_3c20,
    // EXT: an index past an 8-byte vector, op2 set; INS (element) with Q clear
    0x2e00_4020, 0x2e40_0020, 0x2e01_0420,
    // modified immediate with o2 set (and the half-precision FMOV); UZP1 .1D
    0x0f00_0c20, 0x0f00_fc20, 0x0ec0_1820,
    // vector shift by immediate (.1D; narrowing and widening from 64 bits; bit 23 set)
    0x0f40_0420, 0x0f40_1420, 0x0f40_2420, 0x0f40_3420, 0x2f40_4420, 0x0f40_5420, 0x2f40_6420, 0x0f40_7420,
    0x0f88_8420, 0x0f88_8c20, 0x0f88_9420, 0x0f88_9c20, 0x0f40_a420, 0x0f40_e420, 0x0f40_fc20, 0x0f88_0420,
];

#[test]
fn reserved_simd_is_undefined_as_in_the_interpreter() {
    if !jit::available() { return; }
    let mut c = fixed(&[], &[], false);
    let mut bad = Vec::new();
    for &w in RESERVED_SIMD {
        c.code = vec![w, 0xd420_0000];
        let (i, iram, it) = run(&c, false);
        let (j, jram, jt) = run(&c, true);
        // EC 0 (unknown reason) at the instruction
        if i.esr[1] >> 26 != 0 || i.elr[1] != CODE { bad.push(format!("{w:08x}: not UNDEFINED in the interpreter (ESR {:#x}, ELR {:#x})", i.esr[1], i.elr[1])); }
        if let Some(d) = differences((&i, &iram, &it), (&j, &jram, &jt)) { bad.push(format!("{w:08x}: {d}")); }
    }
    assert!(bad.is_empty(), "{}", bad.join("\n"));
}

#[test]
fn random_sequences_match_the_interpreter() {
    if !jit::available() { return; }
    let n: u64 = std::env::var("SBC_JIT_DIFF_CASES").ok().and_then(|v| v.parse().ok()).unwrap_or(3000);
    let first: u64 = std::env::var("SBC_JIT_DIFF_SEED").ok().and_then(|v| v.parse().ok()).unwrap_or(0);
    let mut bad = Vec::new();
    let (mut compiled, mut linked, mut insns) = (0, 0, 0);
    for seed in first..first + n {
        let c = case(seed);
        let (i, iram, it) = run(&c, false);
        assert_eq!(i.sctlr[1] & 1 != 0, c.mmu, "seed {seed}: the MMU");
        let (j, jram, jt) = run(&c, true);
        if let Some(s) = j.jit.as_ref() { compiled += s.compiled; linked += s.linked; insns += s.exec_insns; }
        if let Some(d) = differences((&i, &iram, &it), (&j, &jram, &jt)) {
            let words: Vec<String> = c.code.iter().map(|w| format!("{w:08x}")).collect();
            bad.push(format!("seed {seed}: {d}\n  code: {}", words.join(" ")));
        }
    }
    eprintln!("{compiled} blocks compiled, {linked} links, {insns} instructions in compiled blocks");
    assert!(compiled > n && linked > n / 4 && insns > 50 * n, "the JIT ran too little of the code");
    assert!(bad.is_empty(), "{} of {n} sequences differ:\n{}", bad.len(), bad[..bad.len().min(8)].join("\n"));
}
