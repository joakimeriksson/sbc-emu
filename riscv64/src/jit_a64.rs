//! The block JIT's native backend (AArch64 macOS and Linux hosts): RV64GC to A64.
//!
//! It compiles the same regions as the WebAssembly backend (`jit_wasm.rs`): the entry basic
//! block and the blocks constant branches reach on the same page, with branches between
//! them as host branches while the cycle budget (`chain_until`) lasts. Guest registers stay
//! in the hart (`x[]`, loaded and stored per instruction); PCs are the page's virtual
//! address at entry plus constants; loads and stores probe the micro-TLB of the current
//! mode inline and go to the interpreter (`h_step`) on a miss. The same instructions are
//! compiled (plus `mulh*` and `rev8`, which A64 has), and the same ones are interpreted.
//!
//! Host registers: x19 = the hart, x20 = the page's virtual address, x21 = the micro-TLB
//! table (callee-saved, live across `h_step`); x9..x17 are scratch.
use crate::mmu::{UEntry, UT};
use crate::{rvc, Hart};
use std::mem::{offset_of, size_of};

mod host {
    use std::ffi::{c_int, c_void};
    extern "C" {
        fn mmap(addr: *mut c_void, len: usize, prot: c_int, flags: c_int, fd: c_int, off: i64) -> *mut c_void;
        fn munmap(addr: *mut c_void, len: usize) -> c_int;
        #[cfg(target_os = "macos")]
        fn pthread_jit_write_protect_np(enabled: c_int);
        #[cfg(target_os = "macos")]
        fn sys_icache_invalidate(start: *mut c_void, len: usize);
        #[cfg(target_os = "linux")]
        fn __clear_cache(start: *mut c_void, end: *mut c_void);
    }

    /// Executable code buffer (bump allocated, reset as a whole).
    pub struct CodeCache {
        base: *mut u8,
        size: usize,
        used: usize,
    }

    // SAFETY: the buffer is owned by one machine's JIT and only used by the thread running it
    unsafe impl Send for CodeCache {}

    impl CodeCache {
        pub fn new(size: usize) -> Option<CodeCache> {
            #[cfg(target_os = "macos")]
            let flags = 0x0002 | 0x1000 | 0x0800; // PRIVATE | ANON | JIT
            #[cfg(target_os = "linux")]
            let flags = 0x0002 | 0x0020; // PRIVATE | ANONYMOUS
            // SAFETY: an anonymous mapping chosen by the OS; failure is checked
            let p = unsafe { mmap(std::ptr::null_mut(), size, 7, flags, -1, 0) };
            if p as isize == -1 || p.is_null() {
                return None;
            }
            Some(CodeCache { base: p as *mut u8, size, used: 0 })
        }
        pub fn reset(&mut self) {
            self.used = 0;
        }
        /// Copy machine code in; returns its entry address, or None when the buffer is full.
        pub fn write(&mut self, words: &[u32]) -> Option<*const u8> {
            let bytes = words.len() * 4;
            if self.used + bytes > self.size {
                return None;
            }
            let off = self.used;
            // SAFETY: the range is inside the mapping; writes are enabled per thread on macOS
            // and the instruction cache is synchronised for exactly the written range
            unsafe {
                #[cfg(target_os = "macos")]
                pthread_jit_write_protect_np(0);
                std::ptr::copy_nonoverlapping(words.as_ptr() as *const u8, self.base.add(off), bytes);
                #[cfg(target_os = "macos")]
                pthread_jit_write_protect_np(1);
                #[cfg(target_os = "macos")]
                sys_icache_invalidate(self.base.add(off) as *mut c_void, bytes);
                #[cfg(target_os = "linux")]
                __clear_cache(self.base.add(off) as *mut c_void, self.base.add(off + bytes) as *mut c_void);
            }
            self.used += bytes;
            Some(self.base.wrapping_add(off))
        }
    }

    impl Drop for CodeCache {
        fn drop(&mut self) {
            // SAFETY: the mapping was created by `new` with this size
            unsafe { munmap(self.base as *mut c_void, self.size) };
        }
    }
}

pub(crate) use host::CodeCache;

/// Size of the code buffer; a full buffer flushes all compiled code.
pub(crate) const CODE_BYTES: usize = 256 << 20;

/// Basic blocks per compiled region.
const MAX_BBS: usize = 24;
/// Instructions per basic block.
const MAX_INSNS: u32 = 64;

const X_CPU: u32 = 19;
const X_PAGE: u32 = 20;
const X_UT: u32 = 21;
const A: u32 = 9; // rs1
const B: u32 = 10; // rs2
const R: u32 = 11; // result
const ADDR: u32 = 12; // address of a load or store
const T: u32 = 14; // scratch
const ZR: u32 = 31;

// condition codes
const EQ: u32 = 0;
const NE: u32 = 1;
const HS: u32 = 2;
const LO: u32 = 3;
const HI: u32 = 8;
const GE: u32 = 10;
const LT: u32 = 11;
const GT: u32 = 12;

fn rd(i: u32) -> u32 {
    (i >> 7) & 31
}
fn rs1(i: u32) -> u32 {
    (i >> 15) & 31
}
fn rs2(i: u32) -> u32 {
    (i >> 20) & 31
}
fn f3(i: u32) -> u32 {
    (i >> 12) & 7
}
fn f7(i: u32) -> u32 {
    i >> 25
}
fn imm_i(i: u32) -> i64 {
    ((i as i32) >> 20) as i64
}
fn imm_s(i: u32) -> i64 {
    ((((i as i32) >> 25) << 5) | ((i >> 7) & 31) as i32) as i64
}
fn imm_b(i: u32) -> i64 {
    ((((i as i32) >> 31) << 12) as u32 | ((i >> 7) & 1) << 11 | ((i >> 25) & 0x3f) << 5 | ((i >> 8) & 0xf) << 1) as i32 as i64
}
fn imm_j(i: u32) -> i64 {
    ((((i as i32) >> 31) << 20) as u32 | (i & 0xff000) | ((i >> 20) & 1) << 11 | ((i >> 21) & 0x3ff) << 1) as i32 as i64
}

/// The instruction at page offset `at`: (32-bit encoding, length), or None if it straddles
/// the end of the page.
fn fetch(code: &[u8], at: u32) -> Option<(u32, u32)> {
    let at = at as usize;
    let lo = u16::from_le_bytes([code[at], code[at + 1]]) as u32;
    if lo & 3 != 3 {
        return Some((rvc::expand(lo as u16), 2));
    }
    if at + 4 > code.len() {
        return None;
    }
    Some((lo | (u16::from_le_bytes([code[at + 2], code[at + 3]]) as u32) << 16, 4))
}

/// Where control goes after an instruction, for the region scan (as in `jit_wasm.rs`).
fn scan(i: u32, at: u32) -> (Option<i64>, bool) {
    match i & 0x7f {
        _ if i == 0 => (None, true),
        0x6f => (Some(at as i64 + imm_j(i)), true),
        0x67 => (None, true),
        0x63 => (Some(at as i64 + imm_b(i)), false),
        0x73 => (None, true),
        0x0f if f3(i) != 0 => (None, true),
        _ => (None, false),
    }
}

// ---------------------------------------------------------------------------- encodings
fn ldr_x(rt: u32, rn: u32, off: usize) -> u32 {
    assert!(off.is_multiple_of(8) && off < 32768);
    0xf940_0000 | ((off / 8) as u32) << 10 | rn << 5 | rt
}
fn str_x(rt: u32, rn: u32, off: usize) -> u32 {
    assert!(off.is_multiple_of(8) && off < 32768);
    0xf900_0000 | ((off / 8) as u32) << 10 | rn << 5 | rt
}
/// `<op> xd, xn, xm` (register forms: logical, add/sub shifted, two-source)
fn rrr(base: u32, d: u32, n: u32, m: u32) -> u32 {
    base | m << 16 | n << 5 | d
}
fn ubfm(d: u32, n: u32, immr: u32, imms: u32) -> u32 {
    0xd340_0000 | immr << 16 | imms << 10 | n << 5 | d
}
fn sbfm(d: u32, n: u32, immr: u32, imms: u32) -> u32 {
    0x9340_0000 | immr << 16 | imms << 10 | n << 5 | d
}
fn sxtw(d: u32, n: u32) -> u32 {
    sbfm(d, n, 0, 31)
}
fn csel(base: u32, d: u32, n: u32, m: u32, cond: u32) -> u32 {
    base | m << 16 | cond << 12 | n << 5 | d
}
/// `cset xd, cond`
fn cset(d: u32, cond: u32) -> u32 {
    0x9a9f_07e0 | (cond ^ 1) << 12 | d
}
fn cmp(n: u32, m: u32) -> u32 {
    0xeb00_001f | m << 16 | n << 5
}
fn mov(d: u32, m: u32) -> u32 {
    0xaa00_03e0 | m << 16 | d
}
/// ADD/SUB immediate on X registers for |v| < 4096.
fn addi(d: u32, n: u32, v: i64) -> u32 {
    assert!(v.unsigned_abs() < 4096);
    (if v < 0 { 0xd100_0000 } else { 0x9100_0000 }) | (v.unsigned_abs() as u32) << 10 | n << 5 | d
}

/// The 64-bit logical-immediate encoding (N:immr:imms in bits 22..10) of `v`, if any.
fn logic_imm64(v: u64) -> Option<u32> {
    if v == 0 || v == u64::MAX {
        return None;
    }
    for size in [64u32, 32, 16, 8, 4, 2] {
        let mask = if size == 64 { u64::MAX } else { (1u64 << size) - 1 };
        let elem = v & mask;
        if (0..64 / size).any(|i| (v >> (i * size)) & mask != elem) {
            continue;
        }
        for r in 0..size {
            let rot = if r == 0 { elem } else { ((elem >> r) | (elem << (size - r))) & mask };
            if rot & rot.wrapping_add(1) & mask == 0 && rot != 0 && rot != mask {
                let ones = rot.count_ones();
                let immr = (size - r) % size;
                let n = (size == 64) as u32;
                let imms = ((!(size * 2 - 1)) & 0x3f) | (ones - 1);
                return Some(n << 22 | immr << 16 | (imms & 0x3f) << 10);
            }
        }
    }
    None
}

#[derive(Clone, Copy)]
enum Fix {
    /// a branch at this word index to a label
    Branch(usize, usize),
}

/// A load's or store's slow path: interpret the instruction, then resume after it.
struct Slow {
    label: usize,
    at: u32,
    before: u32,
    resume: usize,
}

struct Off {
    x: usize,
    pc: usize,
    instret: usize,
    cycle: usize,
    ut: usize,
    chain: usize,
}

struct E {
    out: Vec<u32>,
    o: Off,
    step: u64,
    labels: Vec<Option<usize>>,
    fixes: Vec<Fix>,
    slow: Vec<Slow>,
    /// return h_step's status
    exit_h: usize,
    /// page offsets of the region's basic blocks and their labels
    region: Vec<(u32, usize)>,
}

impl E {
    fn p(&mut self, w: u32) {
        self.out.push(w);
    }
    fn label(&mut self) -> usize {
        self.labels.push(None);
        self.labels.len() - 1
    }
    fn bind(&mut self, l: usize) {
        self.labels[l] = Some(self.out.len());
    }
    /// A branch-type instruction (B, B.cond, CBZ/CBNZ) to label `l`.
    fn br(&mut self, w: u32, l: usize) {
        self.fixes.push(Fix::Branch(self.out.len(), l));
        self.out.push(w);
    }
    /// xd = v
    fn movi(&mut self, d: u32, v: u64) {
        let halves = |x: u64| (0..4).filter(|&k| (x >> (16 * k)) & 0xffff != 0).count();
        if halves(!v) < halves(v) {
            // movn, then movk for the halves that are not 0xffff
            let first = (0..4u32).find(|&k| (v >> (16 * k)) & 0xffff != 0xffff).unwrap_or(0);
            self.p(0x9280_0000 | first << 21 | ((!(v >> (16 * first)) & 0xffff) as u32) << 5 | d);
            for k in first + 1..4 {
                let part = ((v >> (16 * k)) & 0xffff) as u32;
                if part != 0xffff {
                    self.p(0xf280_0000 | k << 21 | part << 5 | d);
                }
            }
        } else {
            let first = (0..4u32).find(|&k| (v >> (16 * k)) & 0xffff != 0).unwrap_or(0);
            self.p(0xd280_0000 | first << 21 | (((v >> (16 * first)) & 0xffff) as u32) << 5 | d);
            for k in first + 1..4 {
                let part = ((v >> (16 * k)) & 0xffff) as u32;
                if part != 0 {
                    self.p(0xf280_0000 | k << 21 | part << 5 | d);
                }
            }
        }
    }
    /// xd = xn + v
    fn add_const(&mut self, d: u32, n: u32, v: i64) {
        if v.unsigned_abs() < 4096 {
            if v != 0 || d != n {
                self.p(addi(d, n, v));
            }
        } else {
            self.movi(T, v as u64);
            self.p(rrr(0x8b00_0000, d, n, T));
        }
    }
    /// Guest register `r` into host `h`.
    fn get(&mut self, h: u32, r: u32) {
        if r == 0 {
            self.p(mov(h, ZR));
        } else {
            self.p(ldr_x(h, X_CPU, self.o.x + 8 * r as usize));
        }
    }
    /// Host `h` into guest register `r`.
    fn put(&mut self, h: u32, r: u32) {
        if r != 0 {
            self.p(str_x(h, X_CPU, self.o.x + 8 * r as usize));
        }
    }
    fn set_pc(&mut self, rel: i64) {
        self.add_const(15, X_PAGE, rel);
        self.p(str_x(15, X_CPU, self.o.pc));
    }
    /// instret and cycle += k (the cycle count stays in x15)
    fn count(&mut self, k: u32) {
        for off in [self.o.instret, self.o.cycle] {
            self.p(ldr_x(15, X_CPU, off));
            if k > 0 {
                self.p(addi(15, 15, k as i64));
                self.p(str_x(15, X_CPU, off));
            }
        }
    }
    fn epilogue(&mut self, status: u32) {
        self.p(0x5280_0000 | status << 5); // movz w0, #status
        self.p(0xa942_5bf5); // ldp x21, x22, [sp, #32]
        self.p(0xa941_53f3); // ldp x19, x20, [sp, #16]
        self.p(0xa8c3_7bfd); // ldp x29, x30, [sp], #48
        self.p(0xd65f_03c0); // ret
    }
    /// Leave for page offset `target` after `count` instructions of the basic block: a
    /// target in the region is a branch to it while the budget lasts.
    fn exit_const(&mut self, target: i64, count: u32) {
        self.count(count);
        if let Some(&(_, l)) = self.region.iter().find(|&&(s, _)| s as i64 == target) {
            self.p(ldr_x(16, X_CPU, self.o.chain));
            self.p(cmp(15, 16));
            self.br(0x5400_0000 | LO, l);
        }
        self.set_pc(target);
        self.epilogue(0);
    }
    /// Run the instruction at page offset `at` (the `before`-th of its basic block) in the
    /// interpreter; `last`: the block ends after it.
    fn interp(&mut self, at: u32, before: u32, last: bool) {
        self.set_pc(at as i64);
        self.p(mov(0, X_CPU));
        self.movi(1, (before | (last as u32) << 16) as u64);
        self.movi(16, self.step);
        self.p(0xd63f_0200); // blr x16
        let l = self.exit_h;
        if last {
            self.br(0x1400_0000, l);
        } else {
            self.br(0x3500_0000, l); // cbnz w0, exit_h
        }
    }

    // ------------------------------------------------------------------ memory
    /// ADDR = x[base] + imm; probe the micro-TLB for an aligned `size`-byte access there:
    /// on a hit x16 = ADDR's host address, otherwise branch to the slow path.
    fn probe(&mut self, base: u32, imm: i64, size: u32, write: bool, slow: usize) {
        self.get(ADDR, base);
        self.add_const(ADDR, ADDR, imm);
        self.p(ubfm(16, ADDR, 12, 19)); // ubfx x16, ADDR, #12, #8
        self.p(0x8b00_1000 | 16 << 16 | X_UT << 5 | 16); // add x16, x21, x16, lsl #4
        if write {
            self.add_const(16, 16, (UT * size_of::<UEntry>()) as i64);
        }
        let mask = logic_imm64(!0xfffu64 | (size as u64 - 1)).unwrap();
        self.p(0x9200_0000 | mask | ADDR << 5 | 17); // and x17, ADDR, #mask
        self.p(0xa940_420f); // ldp x15, x16, [x16]
        self.p(cmp(15, 17));
        self.br(0x5400_0000 | NE, slow);
        self.p(rrr(0x8b00_0000, 16, 16, ADDR)); // add x16, x16, ADDR
    }
    fn slow_path(&mut self, at: u32, before: u32) -> (usize, usize) {
        let label = self.label();
        let resume = self.label();
        self.slow.push(Slow { label, at, before, resume });
        (label, resume)
    }
    fn load(&mut self, i: u32, at: u32, n: u32) {
        let (opc, size) = match f3(i) {
            0 => (0x38a0_6800, 1), // ldrsb x
            1 => (0x78a0_6800, 2), // ldrsh x
            2 => (0xb8a0_6800, 4), // ldrsw x
            3 => (0xf860_6800, 8), // ldr x
            4 => (0x3860_6800, 1), // ldrb w
            5 => (0x7860_6800, 2), // ldrh w
            6 => (0xb860_6800, 4), // ldr w
            _ => return self.interp(at, n, false),
        };
        let (slow, resume) = self.slow_path(at, n);
        self.probe(rs1(i), imm_i(i), size, false, slow);
        self.p(opc | ZR << 16 | 16 << 5 | R); // ldr R, [x16, xzr]
        self.put(R, rd(i));
        self.bind(resume);
    }
    fn store(&mut self, i: u32, at: u32, n: u32) {
        let (opc, size) = match f3(i) {
            0 => (0x3820_6800, 1),
            1 => (0x7820_6800, 2),
            2 => (0xb820_6800, 4),
            3 => (0xf820_6800, 8),
            _ => return self.interp(at, n, false),
        };
        let (slow, resume) = self.slow_path(at, n);
        self.probe(rs1(i), imm_s(i), size, true, slow);
        self.get(B, rs2(i));
        self.p(opc | ZR << 16 | 16 << 5 | B);
        self.bind(resume);
    }

    // ------------------------------------------------------------------ arithmetic
    fn ab(&mut self, i: u32) {
        self.get(A, rs1(i));
        self.get(B, rs2(i));
    }
    /// rd = op(x[rs1], x[rs2]) for a register-form A64 instruction
    fn bin(&mut self, i: u32, base: u32) {
        self.ab(i);
        self.p(rrr(base, R, A, B));
        self.put(R, rd(i));
    }
    /// rd = sext32(op32(x[rs1], x[rs2]))
    fn bin_w(&mut self, i: u32, base: u32) {
        self.ab(i);
        self.p(rrr(base, R, A, B));
        self.p(sxtw(R, R));
        self.put(R, rd(i));
    }
    fn set_cond(&mut self, i: u32, cond: u32) {
        self.ab(i);
        self.p(cmp(A, B));
        self.p(cset(R, cond));
        self.put(R, rd(i));
    }
    fn minmax(&mut self, i: u32, cond: u32) {
        self.ab(i);
        self.p(cmp(A, B));
        self.p(csel(0x9a80_0000, R, A, B, cond));
        self.put(R, rd(i));
    }
    /// division and remainder with RISC-V's results for a zero divisor (A64 gives 0) and
    /// overflow (A64 gives the same as RISC-V); `w`: the 32-bit forms
    fn divrem(&mut self, i: u32, w: bool) {
        self.ab(i);
        let sf = if w { 0 } else { 0x8000_0000 };
        let signed = f3(i) & 1 == 0;
        let div = sf | if signed { 0x1ac0_0c00 } else { 0x1ac0_0800 };
        self.p(rrr(div, R, A, B));
        if f3(i) < 6 {
            // a zero divisor gives all ones
            self.p(if w { 0x7100_001f | B << 5 } else { 0xf100_001f | B << 5 }); // cmp b, #0
            self.p(csel(if w { 0x5a80_0000 } else { 0xda80_0000 }, R, R, ZR, NE)); // csinv
        } else {
            // rem = a - (a / b) * b, which is a for a zero divisor
            self.p(sf | 0x1b00_8000 | B << 16 | A << 10 | R << 5 | R); // msub R, R, B, A
        }
        if w {
            self.p(sxtw(R, R));
        }
        self.put(R, rd(i));
    }
    /// x[rs1] (as 64 bits) into R after `op` with an immediate shift encoded by `enc`
    fn unary(&mut self, i: u32, words: &[u32]) {
        self.get(A, rs1(i));
        for &w in words {
            self.p(w);
        }
        self.put(R, rd(i));
    }
    /// R = popcount(A)
    fn popcount(&mut self) {
        self.p(0x9e67_0000 | A << 5); // fmov d0, xA
        self.p(0x0e20_5800); // cnt v0.8b, v0.8b
        self.p(0x0e31_b800); // addv b0, v0.8b
        self.p(0x0e01_3c00 | R); // umov wR, v0.b[0]
    }

    // ------------------------------------------------------------------ instructions
    /// Emit instruction `i` (length `len`) at page offset `at`, the `n`-th of its basic
    /// block. Returns true if the basic block ends with it.
    fn insn(&mut self, i: u32, len: u32, at: u32, n: u32) -> bool {
        let next = at as i64 + len as i64;
        match i & 0x7f {
            _ if i == 0 => {
                self.interp(at, n, true);
                return true;
            }
            0x37 => {
                self.movi(R, (i & 0xffff_f000) as i32 as i64 as u64);
                self.put(R, rd(i));
            }
            0x17 => {
                self.add_const(R, X_PAGE, at as i64 + (i & 0xffff_f000) as i32 as i64);
                self.put(R, rd(i));
            }
            0x6f => {
                self.add_const(R, X_PAGE, next);
                self.put(R, rd(i));
                self.exit_const(at as i64 + imm_j(i), n + 1);
                return true;
            }
            0x67 => {
                if f3(i) != 0 {
                    self.interp(at, n, true);
                    return true;
                }
                // the target before the link (rd may be rs1)
                self.get(A, rs1(i));
                self.add_const(A, A, imm_i(i));
                self.p(0x9200_0000 | logic_imm64(!1).unwrap() | A << 5 | A); // and xA, xA, #~1
                self.add_const(R, X_PAGE, next);
                self.put(R, rd(i));
                self.count(n + 1);
                self.p(str_x(A, X_CPU, self.o.pc));
                self.epilogue(0);
                return true;
            }
            0x63 => {
                let cond = match f3(i) {
                    0 => EQ,
                    1 => NE,
                    4 => LT,
                    5 => GE,
                    6 => LO,
                    7 => HS,
                    _ => {
                        self.interp(at, n, true);
                        return true;
                    }
                };
                self.ab(i);
                self.p(cmp(A, B));
                let skip = self.label();
                self.br(0x5400_0000 | (cond ^ 1), skip);
                self.exit_const(at as i64 + imm_b(i), n + 1);
                self.bind(skip);
            }
            0x03 => self.load(i, at, n),
            0x23 => self.store(i, at, n),
            0x13 => {
                let imm = imm_i(i);
                let sh = (i >> 20) & 63;
                match f3(i) {
                    0 => {
                        self.get(A, rs1(i));
                        self.add_const(R, A, imm);
                        self.put(R, rd(i));
                    }
                    2 | 3 => {
                        self.get(A, rs1(i));
                        self.movi(B, imm as u64);
                        self.p(cmp(A, B));
                        self.p(cset(R, if f3(i) == 2 { LT } else { LO }));
                        self.put(R, rd(i));
                    }
                    4 | 6 | 7 => {
                        self.get(A, rs1(i));
                        self.movi(B, imm as u64);
                        self.p(rrr([0xca00_0000, 0, 0xaa00_0000, 0x8a00_0000][(f3(i) - 4) as usize], R, A, B));
                        self.put(R, rd(i));
                    }
                    1 => match i >> 26 {
                        0 => self.unary(i, &[ubfm(R, A, (64 - sh) & 63, 63 - sh)]),
                        0x18 => match (i >> 20) & 0x3f {
                            0 => self.unary(i, &[0xdac0_1000 | A << 5 | R]),                         // clz
                            1 => self.unary(i, &[0xdac0_0000 | A << 5 | R, 0xdac0_1000 | R << 5 | R]), // ctz
                            2 => {
                                self.get(A, rs1(i));
                                self.popcount();
                                self.put(R, rd(i));
                            }
                            4 => self.unary(i, &[sbfm(R, A, 0, 7)]),
                            5 => self.unary(i, &[sbfm(R, A, 0, 15)]),
                            _ => self.interp(at, n, false),
                        },
                        _ => self.interp(at, n, false),
                    },
                    _ => match i >> 26 {
                        0 => self.unary(i, &[ubfm(R, A, sh, 63)]),
                        0x10 => self.unary(i, &[sbfm(R, A, sh, 63)]),
                        0x18 => self.unary(i, &[0x93c0_0000 | A << 16 | sh << 10 | A << 5 | R]), // ror (extr)
                        _ if i >> 20 == 0x6b8 => self.unary(i, &[0xdac0_0c00 | A << 5 | R]),    // rev8
                        _ => self.interp(at, n, false),                                           // orc.b
                    },
                }
            }
            0x1b => {
                let sh = (i >> 20) & 31;
                match (f3(i), f7(i)) {
                    (0, _) => {
                        self.get(A, rs1(i));
                        self.add_const(R, A, imm_i(i));
                        self.p(sxtw(R, R));
                        self.put(R, rd(i));
                    }
                    (1, 4) | (1, 5) => {
                        // slli.uw: ubfiz
                        let s = (i >> 20) & 63;
                        let width = 32.min(64 - s);
                        self.unary(i, &[ubfm(R, A, (64 - s) & 63, width - 1)]);
                    }
                    (1, 0) => self.unary(i, &[0x5300_0000 | ((32 - sh) & 31) << 16 | (31 - sh) << 10 | A << 5 | R, sxtw(R, R)]),
                    (5, 0) => self.unary(i, &[0x5300_0000 | sh << 16 | 31 << 10 | A << 5 | R, sxtw(R, R)]),
                    (5, 0x20) => self.unary(i, &[0x1300_0000 | sh << 16 | 31 << 10 | A << 5 | R, sxtw(R, R)]),
                    (5, 0x30) => self.unary(i, &[0x1380_0000 | A << 16 | sh << 10 | A << 5 | R, sxtw(R, R)]), // extr w
                    (1, 0x30) => match rs2(i) {
                        0 => self.unary(i, &[0x5ac0_1000 | A << 5 | R]),
                        1 => self.unary(i, &[0x5ac0_0000 | A << 5 | R, 0x5ac0_1000 | R << 5 | R]),
                        2 => {
                            self.get(A, rs1(i));
                            self.p(0x2a00_03e0 | A << 16 | A); // mov wA, wA (zero-extend)
                            self.popcount();
                            self.put(R, rd(i));
                        }
                        _ => self.interp(at, n, false),
                    },
                    _ => self.interp(at, n, false),
                }
            }
            0x33 => match (f7(i), f3(i)) {
                (0, 0) => self.bin(i, 0x8b00_0000),
                (0x20, 0) => self.bin(i, 0xcb00_0000),
                (0, 1) => self.bin(i, 0x9ac0_2000),
                (0, 2) => self.set_cond(i, LT),
                (0, 3) => self.set_cond(i, LO),
                (0, 4) => self.bin(i, 0xca00_0000),
                (0, 5) => self.bin(i, 0x9ac0_2400),
                (0x20, 5) => self.bin(i, 0x9ac0_2800),
                (0, 6) => self.bin(i, 0xaa00_0000),
                (0, 7) => self.bin(i, 0x8a00_0000),
                (1, 0) => self.bin(i, 0x9b00_7c00),
                (1, 1) => self.bin(i, 0x9b40_7c00), // smulh
                (1, 3) => self.bin(i, 0x9bc0_7c00), // umulh
                (1, 2) => {
                    // mulhsu = umulh(a, b) - (a < 0 ? b : 0)
                    self.ab(i);
                    self.p(rrr(0x9bc0_7c00, R, A, B));
                    self.p(sbfm(T, A, 63, 63)); // asr T, A, #63
                    self.p(rrr(0x8a00_0000, T, T, B));
                    self.p(rrr(0xcb00_0000, R, R, T));
                    self.put(R, rd(i));
                }
                (1, 4..=7) => self.divrem(i, false),
                (0x20, 7) => self.bin(i, 0x8a20_0000), // andn = bic
                (0x20, 6) => self.bin(i, 0xaa20_0000), // orn
                (0x20, 4) => self.bin(i, 0xca20_0000), // xnor = eon
                (5, 4) => self.minmax(i, LT),
                (5, 5) => self.minmax(i, LO),
                (5, 6) => self.minmax(i, GT),
                (5, 7) => self.minmax(i, HI),
                (0x30, 5) => self.bin(i, 0x9ac0_2c00), // ror
                (0x30, 1) => {
                    // rol = ror by -b
                    self.ab(i);
                    self.p(rrr(0xcb00_0000, B, ZR, B));
                    self.p(rrr(0x9ac0_2c00, R, A, B));
                    self.put(R, rd(i));
                }
                (0x10, 2) | (0x10, 4) | (0x10, 6) => {
                    // shNadd: b + (a << n)
                    self.ab(i);
                    self.p(0x8b00_0000 | A << 16 | (f3(i) / 2) << 10 | B << 5 | R);
                    self.put(R, rd(i));
                }
                _ => self.interp(at, n, false),
            },
            0x3b => match (f7(i), f3(i)) {
                (0, 0) => self.bin_w(i, 0x0b00_0000),
                (0x20, 0) => self.bin_w(i, 0x4b00_0000),
                (1, 0) => self.bin_w(i, 0x1b00_7c00),
                (0, 1) => self.bin_w(i, 0x1ac0_2000),
                (0, 5) => self.bin_w(i, 0x1ac0_2400),
                (0x20, 5) => self.bin_w(i, 0x1ac0_2800),
                (0x30, 5) => self.bin_w(i, 0x1ac0_2c00),
                (0x30, 1) => {
                    self.ab(i);
                    self.p(rrr(0x4b00_0000, B, ZR, B)); // neg wB
                    self.p(rrr(0x1ac0_2c00, R, A, B));
                    self.p(sxtw(R, R));
                    self.put(R, rd(i));
                }
                (1, 4..=7) => self.divrem(i, true),
                (4, 0) | (0x10, 2) | (0x10, 4) | (0x10, 6) => {
                    // add.uw, shNadd.uw: b + (zext32(a) << n)
                    self.ab(i);
                    let n = if f3(i) == 0 { 0 } else { f3(i) / 2 };
                    self.p(0x8b20_0000 | A << 16 | 0b010 << 13 | n << 10 | B << 5 | R);
                    self.put(R, rd(i));
                }
                (4, 4) if rs2(i) == 0 => self.unary(i, &[ubfm(R, A, 0, 15)]), // zext.h
                _ => self.interp(at, n, false),
            },
            0x0f if f3(i) == 0 => {} // fence: the harts run in turns on one host thread
            0x73 | 0x0f => {
                self.interp(at, n, true);
                return true;
            }
            _ => self.interp(at, n, false), // AMOs, F and D
        }
        false
    }

    fn finish(mut self) -> Vec<u32> {
        for s in std::mem::take(&mut self.slow) {
            self.bind(s.label);
            self.interp(s.at, s.before, false);
            self.br(0x1400_0000, s.resume);
        }
        let l = self.exit_h;
        self.bind(l);
        self.p(0xa942_5bf5); // ldp x21, x22, [sp, #32]
        self.p(0xa941_53f3); // ldp x19, x20, [sp, #16]
        self.p(0xa8c3_7bfd); // ldp x29, x30, [sp], #48
        self.p(0xd65f_03c0); // ret (w0 = h_step's status)
        for f in &self.fixes {
            let Fix::Branch(at, l) = *f;
            let target = self.labels[l].expect("unbound label");
            let off = target as i64 - at as i64;
            let w = self.out[at];
            self.out[at] = if w & 0xfc00_0000 == 0x1400_0000 {
                w | (off as u32 & 0x03ff_ffff)
            } else {
                assert!((-(1 << 18)..(1 << 18)).contains(&off));
                w | (off as u32 & 0x7_ffff) << 5
            };
        }
        self.out
    }
}

/// Why a region was not compiled.
pub(crate) enum Fail {
    /// the code buffer is full (flush and try again)
    Full,
    /// this entry cannot be compiled (its first instruction straddles the page)
    Never,
}

/// Compile the region entered at virtual address `pc` of the page `code`. Returns the
/// function's address and the page offsets of the bytes it was compiled from.
pub(crate) fn compile(cache: &mut CodeCache, code: &[u8], pc: u64, step: usize) -> Result<(usize, u16, u16), Fail> {
    let entry = (pc & 0xfff) as u32;
    fetch(code, entry).ok_or(Fail::Never)?;
    let o = Off {
        x: offset_of!(Hart, x),
        pc: offset_of!(Hart, pc),
        instret: offset_of!(Hart, instret),
        cycle: offset_of!(Hart, cycle),
        ut: offset_of!(Hart, jit_ut),
        chain: offset_of!(Hart, chain_until),
    };
    let mut e = E { out: Vec::with_capacity(1024), o, step: step as u64, labels: Vec::new(), fixes: Vec::new(), slow: Vec::new(), exit_h: 0, region: Vec::new() };
    e.exit_h = e.label();
    // ---- the region: basic blocks reachable through constant branches on this page
    let mut bbs = vec![entry];
    let (mut lo, mut hi) = (entry, entry);
    let mut q = 0;
    while q < bbs.len() {
        let mut at = bbs[q];
        q += 1;
        for _ in 0..MAX_INSNS {
            let Some((i, len)) = fetch(code, at) else { break };
            let (target, stop) = scan(i, at);
            if let Some(t) = target {
                if (0..0x1000).contains(&t) && bbs.len() < MAX_BBS && !bbs.contains(&(t as u32)) && fetch(code, t as u32).is_some() {
                    bbs.push(t as u32);
                }
            }
            lo = lo.min(at);
            hi = hi.max(at + len);
            at += len;
            if stop || at >= 0x1000 {
                break;
            }
        }
    }
    e.region = bbs.iter().map(|&s| (s, 0)).collect();
    for k in 0..e.region.len() {
        e.region[k].1 = e.label();
    }
    // ---- prologue
    e.p(0xa9bd_7bfd); // stp x29, x30, [sp, #-48]!
    e.p(0xa901_53f3); // stp x19, x20, [sp, #16]
    e.p(0xa902_5bf5); // stp x21, x22, [sp, #32]
    e.p(mov(X_CPU, 0));
    e.p(ldr_x(X_PAGE, X_CPU, e.o.pc));
    e.p(0x9274_cc00 | X_PAGE << 5 | X_PAGE); // and x20, x20, #~0xfff
    e.p(ldr_x(X_UT, X_CPU, e.o.ut));
    for k in 0..bbs.len() {
        let (start, l) = e.region[k];
        e.bind(l);
        let mut n = 0u32;
        let mut at = start;
        loop {
            let Some((i, len)) = fetch(code, at) else {
                // straddles the page: the interpreter runs it (never through the region)
                e.count(n);
                e.set_pc(at as i64);
                e.epilogue(0);
                break;
            };
            let end = e.insn(i, len, at, n);
            n += 1;
            at += len;
            if end {
                break;
            }
            if n >= MAX_INSNS || at >= 0x1000 {
                e.exit_const(at as i64, n);
                break;
            }
        }
    }
    let words = e.finish();
    let f = cache.write(&words).ok_or(Fail::Full)?;
    Ok((f as usize, lo as u16, hi as u16))
}

#[cfg(test)]
mod tests {
    use super::logic_imm64;
    #[test]
    fn logical_immediates() {
        // and x20, x20, #~0xfff is 0x9274cc00 | 20 << 5 | 20
        assert_eq!(0x9200_0000 | logic_imm64(!0xfff).unwrap(), 0x9274_cc00);
        assert!(logic_imm64(!1).is_some());
        for size in [1u64, 2, 4, 8] {
            assert!(logic_imm64(!0xfffu64 | (size - 1)).is_some());
        }
        assert!(logic_imm64(0).is_none() && logic_imm64(u64::MAX).is_none());
        assert!(logic_imm64(0x1234_5678).is_none());
    }
}
