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
//! Regions are linked: an exit to a pc the hart's jump cache knows (the driver fills it with
//! each region it enters) jumps straight into that region's code, past its prologue, while
//! the cycle budget lasts. The jump cache has a table per mode and is emptied when the
//! address space (`satp`, `sfence.vma`) or the compiled code (`fence.i`, a flush) changes, so
//! a hit is a region the driver would have entered itself.
//!
//! Guest registers a region uses more than once live in host registers while it runs (up
//! to 14: the most used in x23..x28, the rest in x0..x7). They are loaded at the region's
//! linked entry and written back before `h_step` (which reads and writes the hart) and on
//! every way out. The cycle count lives in x8 from the driver's call until compiled code
//! returns, across linked regions (instret follows it: the hart's two counters are brought
//! up to date together, before `h_step` and on the way out). Two stubs at the end of each
//! region keep slow paths and exits to a few instructions: one runs an instruction in
//! `h_step` (writing back first and loading everything again after), one leaves for a pc.
//!
//! Host registers: x19 = the hart, x20 = the page's virtual address, x21 = the micro-TLB
//! table, x22 = the jump cache table (callee-saved, live across `h_step`); x8 = the cycle
//! count; x9..x17 are scratch.
use crate::mmu::{JEntry, UEntry, JC, UT};
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
const X_JC: u32 = 22;
/// Offset of a region's linked entry (after the prologue): x19, x21, x22 and the cycle count
/// in x8 are set, x20 is the virtual page of the pc entered.
pub(crate) const LINKED: usize = 12 * 4;

/// Host registers for cached guest registers, in order of preference: callee-saved first
/// (they survive `h_step`).
const CACHE_REGS: [u32; 14] = [23, 24, 25, 26, 27, 28, 0, 1, 2, 3, 4, 5, 6, 7];
/// The hart's cycle count while compiled code runs (instret is brought along at `sync`).
const X_CYC: u32 = 8;
/// Frame size: x19..x30 saved.
const FRAME: u32 = 96;
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

/// The guest registers an instruction reads and writes (bit masks; x0 included).
fn uses(i: u32) -> (u32, u32) {
    let (r1, r2, d) = (1 << rs1(i), 1 << rs2(i), 1 << rd(i));
    match i & 0x7f {
        0x37 | 0x17 | 0x6f => (0, d),
        0x67 | 0x03 | 0x13 | 0x1b => (r1, d),
        0x63 | 0x23 => (r1 | r2, 0),
        0x33 | 0x3b | 0x2f => (r1 | r2, d),
        0x07 | 0x27 => (r1, 0),
        0x73 if f3(i) != 0 => (if f3(i) < 4 { r1 } else { 0 }, d),
        _ => (0, 0),
    }
}

/// Where control goes after an instruction, for the region scan (as in `jit_wasm.rs`).
fn scan(i: u32, at: u32) -> (Option<i64>, bool) {
    match i & 0x7f {
        _ if i == 0 => (None, true),
        0x6f => (Some(at as i64 + imm_j(i)), true),
        0x67 => (None, true),
        0x63 => (Some(at as i64 + imm_b(i)), false),
        0x73 => (None, f3(i) == 0),
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
    i: u32,
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
    jc: usize,
    lr: usize,
    tmp: usize,
    f: usize,
    mstatus: usize,
    reservation: usize,
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
    /// the region's stubs: run an instruction in `h_step`, leave for the pc in A
    stub_interp: usize,
    stub_leave: usize,
    /// page offsets of the region's basic blocks and their labels
    region: Vec<(u32, usize)>,
    /// the host register of each guest register (0 = in the hart; x0 is never cached, and
    /// host x0 is told apart by `cached`)
    host: [u32; 32],
    /// guest registers in host registers, and those of them compiled code writes
    cached: u32,
    written: u32,
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
    fn is_cached(&self, r: u32) -> bool {
        self.cached & 1 << r != 0
    }
    /// Guest register `r` into host `h`.
    fn get(&mut self, h: u32, r: u32) {
        if r == 0 {
            self.p(mov(h, ZR));
        } else if self.is_cached(r) {
            self.p(mov(h, self.host[r as usize]));
        } else {
            self.p(ldr_x(h, X_CPU, self.o.x + 8 * r as usize));
        }
    }
    /// Host `h` into guest register `r`.
    fn put(&mut self, h: u32, r: u32) {
        if r == 0 {
        } else if self.is_cached(r) {
            debug_assert!(self.written & 1 << r != 0);
            self.p(mov(self.host[r as usize], h));
        } else {
            self.p(str_x(h, X_CPU, self.o.x + 8 * r as usize));
        }
    }
    /// A host register holding guest register `r` for reading: its cached register, xzr for
    /// x0 (only where register 31 is the zero register), or `scratch` loaded from the hart.
    fn src(&mut self, scratch: u32, r: u32) -> u32 {
        if r == 0 {
            ZR
        } else if self.is_cached(r) {
            self.host[r as usize]
        } else {
            self.p(ldr_x(scratch, X_CPU, self.o.x + 8 * r as usize));
            scratch
        }
    }
    /// As `src`, never register 31 (for ADD immediate and addresses, where it is sp).
    fn src_nz(&mut self, scratch: u32, r: u32) -> u32 {
        if r == 0 {
            self.p(mov(scratch, ZR));
            scratch
        } else {
            self.src(scratch, r)
        }
    }
    /// Where to compute guest register `r` in one instruction: its cached register, or R
    /// (then `fin` stores it).
    fn dst(&self, r: u32) -> u32 {
        if r != 0 && self.is_cached(r) {
            debug_assert!(self.written & 1 << r != 0);
            self.host[r as usize]
        } else {
            R
        }
    }
    /// Finish a result computed in `dst(r)`.
    fn fin(&mut self, h: u32, r: u32) {
        if r != 0 && !self.is_cached(r) {
            self.p(str_x(h, X_CPU, self.o.x + 8 * r as usize));
        }
    }
    /// Load cached guest registers from the hart: those in `mask`.
    fn load_cached(&mut self, mask: u32) {
        for r in 1..32 {
            if mask & self.cached & 1 << r != 0 {
                self.p(ldr_x(self.host[r], X_CPU, self.o.x + 8 * r));
            }
        }
    }
    /// Write the cached guest registers compiled code changes back to the hart.
    fn write_back(&mut self) {
        for r in 1..32 {
            if self.written & self.cached & 1 << r != 0 {
                self.p(str_x(self.host[r], X_CPU, self.o.x + 8 * r));
            }
        }
    }
    fn set_pc(&mut self, rel: i64) {
        self.add_const(15, X_PAGE, rel);
        self.p(str_x(15, X_CPU, self.o.pc));
    }
    /// The cycle count += k (the instructions of a basic block, counted where it ends).
    fn count(&mut self, k: u32) {
        if k > 0 {
            self.p(addi(X_CYC, X_CYC, k as i64));
        }
    }
    /// Bring the hart's counters up to date with x8: instret moves as much as cycle did.
    fn sync(&mut self) {
        self.p(ldr_x(15, X_CPU, self.o.cycle));
        self.p(rrr(0xcb00_0000, 15, X_CYC, 15)); // sub x15, x8, x15
        self.p(ldr_x(16, X_CPU, self.o.instret));
        self.p(rrr(0x8b00_0000, 16, 16, 15));
        self.p(str_x(16, X_CPU, self.o.instret));
        self.p(str_x(X_CYC, X_CPU, self.o.cycle));
    }
    fn epilogue(&mut self, status: u32) {
        self.p(0x5280_0000 | status << 5); // movz w0, #status
        self.ret();
    }
    /// Restore the callee-saved registers and return (w0 = status).
    fn ret(&mut self) {
        for (k, r) in [(5u32, 27u32), (4, 25), (3, 23), (2, 21), (1, 19)] {
            self.p(0xa940_0000 | (2 * k) << 15 | (r + 1) << 10 | 31 << 5 | r); // ldp xr, xr+1, [sp, #16k]
        }
        self.p(0xa8c0_0000 | (FRAME / 8) << 15 | 30 << 10 | 31 << 5 | 29); // ldp x29, x30, [sp], #FRAME
        self.p(0xd65f_03c0); // ret
    }
    /// Leave for page offset `target` after `count` instructions of the basic block: a
    /// target in the region is a branch to it while the budget lasts.
    fn exit_const(&mut self, target: i64, count: u32) {
        self.count(count);
        if let Some(&(_, l)) = self.region.iter().find(|&&(s, _)| s as i64 == target) {
            self.p(ldr_x(16, X_CPU, self.o.chain));
            self.p(cmp(X_CYC, 16));
            self.br(0x5400_0000 | LO, l);
        }
        self.add_const(A, X_PAGE, target);
        let l = self.stub_leave;
        self.br(0x1400_0000, l);
    }
    /// The leave stub: leave for the pc in A (the block's instructions counted in x8): into
    /// the region the jump cache has for it while the budget lasts, otherwise back to the
    /// driver.
    fn leave_stub(&mut self) {
        let l = self.stub_leave;
        self.bind(l);
        self.write_back();
        let out = self.label();
        self.p(ldr_x(16, X_CPU, self.o.chain));
        self.p(cmp(X_CYC, 16));
        self.br(0x5400_0000 | HS, out);
        const _: () = assert!(size_of::<JEntry>() == 16);
        self.p(ubfm(16, A, 1, JC.trailing_zeros())); // ubfx x16, A, #1, #log2(JC)
        self.p(0x8b00_1000 | 16 << 16 | X_JC << 5 | 16); // add x16, x22, x16, lsl #4
        self.p(0xa940_0000 | 17 << 10 | 16 << 5 | 15); // ldp x15, x17, [x16]
        self.p(cmp(15, A));
        self.br(0x5400_0000 | NE, out);
        self.p(0x9274_cc00 | A << 5 | X_PAGE); // and x20, A, #~0xfff
        self.p(0xd61f_0000 | 17 << 5); // br x17
        // the counters go on in x8 into a linked region; the hart's only on the way out
        self.bind(out);
        self.sync();
        self.p(str_x(A, X_CPU, self.o.pc));
        self.epilogue(0);
    }
    /// Run the instruction at page offset `at` (the `before`-th of its basic block) in the
    /// interpreter; `last`: the block ends after it.
    fn interp(&mut self, _i: u32, at: u32, before: u32, last: bool) {
        // scratch registers: x0..x7 may hold guest registers not yet written back
        self.movi(A, ((last as u32) << 16) as u64);
        self.movi(B, at as u64);
        self.movi(R, before as u64);
        let l = self.stub_interp;
        self.fixes.push(Fix::Branch(self.out.len(), l));
        self.p(0x9400_0000); // bl stub_interp
        if last {
            let l = self.exit_h;
            self.br(0x1400_0000, l);
        }
    }
    /// The interpreter stub (A = h_step's info, B = the instruction's page offset, R = the
    /// instructions of its basic block before it, x30 = where to go on): count those,
    /// bring the hart up to date, run the instruction; leave if it says so, otherwise load
    /// everything again (it may have written its rd, and the call lost x0..x18).
    fn interp_stub(&mut self) {
        let l = self.stub_interp;
        self.bind(l);
        self.p(str_x(30, X_CPU, self.o.lr));
        self.p(str_x(R, X_CPU, self.o.tmp));
        self.p(rrr(0x8b00_0000, X_CYC, X_CYC, R));
        self.sync();
        self.write_back();
        self.p(rrr(0x8b00_0000, 15, X_PAGE, B));
        self.p(str_x(15, X_CPU, self.o.pc));
        self.p(mov(0, X_CPU));
        self.p(mov(1, A));
        self.movi(16, self.step);
        self.p(0xd63f_0200); // blr x16
        let l = self.exit_h;
        self.br(0x3500_0000, l); // cbnz w0, exit_h
        self.load_cached(u32::MAX);
        // h_step left the counts where they were before the instruction; the block counts
        // its instructions where it ends
        self.p(ldr_x(X_CYC, X_CPU, self.o.cycle));
        self.p(ldr_x(R, X_CPU, self.o.tmp));
        self.p(rrr(0xcb00_0000, X_CYC, X_CYC, R));
        self.p(ldr_x(30, X_CPU, self.o.lr));
        self.p(0xd65f_03c0); // ret
    }

    // ------------------------------------------------------------------ memory
    /// ADDR = x[base] + imm; probe the micro-TLB for a `size`-byte access there (`aligned`:
    /// only an aligned one, otherwise any that stays in the page): on a hit x16 = ADDR's host
    /// address, otherwise branch to the slow path.
    fn probe(&mut self, base: u32, imm: i64, size: u32, write: bool, aligned: bool, slow: usize) -> u32 {
        let b = self.src_nz(ADDR, base);
        let addr = if imm == 0 {
            b
        } else {
            self.add_const(ADDR, b, imm);
            ADDR
        };
        self.p(ubfm(16, addr, 12, 19)); // ubfx x16, addr, #12, #8
        self.p(0x8b00_1000 | 16 << 16 | X_UT << 5 | 16); // add x16, x21, x16, lsl #4
        if write {
            const _: () = assert!(UT * size_of::<UEntry>() == 4096);
            self.p(0x9140_0400 | 16 << 5 | 16); // add x16, x16, #1, lsl #12
        }
        if aligned || size == 1 {
            let mask = logic_imm64(!0xfffu64 | (size as u64 - 1)).unwrap();
            self.p(0x9200_0000 | mask | addr << 5 | 17); // and x17, addr, #mask
        } else {
            // the page of the last byte: the entry at the first byte's index can only have
            // that tag if both bytes are on its page
            self.p(addi(17, addr, size as i64 - 1));
            self.p(0x9274_cc00 | 17 << 5 | 17); // and x17, x17, #~0xfff
        }
        self.p(0xa940_420f); // ldp x15, x16, [x16]
        self.p(cmp(15, 17));
        self.br(0x5400_0000 | NE, slow);
        // the access: [x16, addr] (x16 = the host page minus the guest page)
        addr
    }
    fn slow_path(&mut self, i: u32, at: u32, before: u32) -> (usize, usize) {
        let label = self.label();
        let resume = self.label();
        self.slow.push(Slow { i, label, at, before, resume });
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
            _ => return self.interp(i, at, n, false),
        };
        let (slow, resume) = self.slow_path(i, at, n);
        let addr = self.probe(rs1(i), imm_i(i), size, false, false, slow);
        let d = self.dst(rd(i));
        self.p(opc | addr << 16 | 16 << 5 | d); // ldr d, [x16, addr]
        self.fin(d, rd(i));
        self.bind(resume);
    }
    fn store(&mut self, i: u32, at: u32, n: u32) {
        let (opc, size) = match f3(i) {
            0 => (0x3820_6800, 1),
            1 => (0x7820_6800, 2),
            2 => (0xb820_6800, 4),
            3 => (0xf820_6800, 8),
            _ => return self.interp(i, at, n, false),
        };
        let (slow, resume) = self.slow_path(i, at, n);
        let addr = self.probe(rs1(i), imm_s(i), size, true, false, slow);
        let v = self.src(B, rs2(i));
        self.p(opc | addr << 16 | 16 << 5 | v);
        self.bind(resume);
    }

    /// Branch to `slow` unless the FPU is on (x13 = mstatus).
    fn fs_on(&mut self, slow: usize) {
        self.p(ldr_x(13, X_CPU, self.o.mstatus));
        self.p(0xf200_0000 | logic_imm64(3 << 13).unwrap() | 13 << 5 | ZR); // tst x13, #FS
        self.br(0x5400_0000 | EQ, slow);
    }
    /// flw, fld
    fn fload(&mut self, i: u32, at: u32, n: u32) {
        let size = if f3(i) == 2 { 4 } else { 8 };
        let (slow, resume) = self.slow_path(i, at, n);
        self.fs_on(slow);
        let addr = self.probe(rs1(i), imm_i(i), size, false, false, slow);
        if size == 4 {
            self.p(0xb860_6800 | addr << 16 | 16 << 5 | R); // ldr wR, [x16, addr]
            self.p(0xb200_0000 | logic_imm64(0xffff_ffff_0000_0000).unwrap() | R << 5 | R); // NaN-box
        } else {
            self.p(0xf860_6800 | addr << 16 | 16 << 5 | R);
        }
        self.p(str_x(R, X_CPU, self.o.f + 8 * rd(i) as usize));
        // FS = dirty, SD
        self.p(0xb200_0000 | logic_imm64(3 << 13).unwrap() | 13 << 5 | 13);
        self.p(0xb200_0000 | logic_imm64(1 << 63).unwrap() | 13 << 5 | 13);
        self.p(str_x(13, X_CPU, self.o.mstatus));
        self.bind(resume);
    }
    /// fsw, fsd
    fn fstore(&mut self, i: u32, at: u32, n: u32) {
        let size = if f3(i) == 2 { 4 } else { 8 };
        let (slow, resume) = self.slow_path(i, at, n);
        self.fs_on(slow);
        let addr = self.probe(rs1(i), imm_s(i), size, true, false, slow);
        self.p(ldr_x(B, X_CPU, self.o.f + 8 * rs2(i) as usize));
        self.p(if size == 4 { 0xb820_6800 } else { 0xf820_6800 } | addr << 16 | 16 << 5 | B);
        self.bind(resume);
    }
    /// AMOs other than LR and SC, on RAM with no reservation held (anything else is
    /// interpreted)
    fn amo(&mut self, i: u32, at: u32, n: u32) {
        let w = f3(i) == 2;
        let op = i >> 27;
        if !matches!(f3(i), 2 | 3) || !matches!(op, 0x00 | 0x01 | 0x04 | 0x08 | 0x0c | 0x10 | 0x14 | 0x18 | 0x1c) {
            return self.interp(i, at, n, false);
        }
        let (slow, resume) = self.slow_path(i, at, n);
        self.p(ldr_x(13, X_CPU, self.o.reservation));
        self.p(0xb100_041f | 13 << 5); // cmn x13, #1
        self.br(0x5400_0000 | NE, slow);
        let addr = self.probe(rs1(i), 0, if w { 4 } else { 8 }, true, true, slow);
        // R = old (sign-extended for .w), B = x[rs2], x13 = new
        self.p(if w { 0xb8a0_6800 } else { 0xf860_6800 } | addr << 16 | 16 << 5 | R);
        self.get(B, rs2(i));
        let new = 13;
        match op {
            0x01 => self.p(mov(new, B)),
            0x00 => self.p(rrr(0x8b00_0000, new, R, B)),
            0x04 => self.p(rrr(0xca00_0000, new, R, B)),
            0x0c => self.p(rrr(0x8a00_0000, new, R, B)),
            0x08 => self.p(rrr(0xaa00_0000, new, R, B)),
            _ => {
                let (cond, signed) = match op {
                    0x10 => (LT, true),
                    0x14 => (GT, true),
                    0x18 => (LO, false),
                    _ => (HI, false),
                };
                if w && signed {
                    self.p(sxtw(B, B));
                }
                if w && !signed {
                    self.p(0x6b00_001f | B << 16 | R << 5); // cmp wR, wB
                } else {
                    self.p(cmp(R, B));
                }
                self.p(csel(0x9a80_0000, new, R, B, cond));
            }
        }
        self.p(if w { 0xb820_6800 } else { 0xf820_6800 } | addr << 16 | 16 << 5 | new);
        self.put(R, rd(i));
        self.bind(resume);
    }

    // ------------------------------------------------------------------ arithmetic
    /// x[rs1] and x[rs2] into A and B (multi-instruction sequences that write R before they
    /// are done with the operands)
    fn ab(&mut self, i: u32) {
        self.get(A, rs1(i));
        self.get(B, rs2(i));
    }
    /// The operands' host registers (xzr for x0).
    fn ops(&mut self, i: u32) -> (u32, u32) {
        let a = self.src(A, rs1(i));
        let b = self.src(B, rs2(i));
        (a, b)
    }
    /// rd = op(x[rs1], x[rs2]) for a register-form A64 instruction
    fn bin(&mut self, i: u32, base: u32) {
        let (a, b) = self.ops(i);
        let d = self.dst(rd(i));
        self.p(rrr(base, d, a, b));
        self.fin(d, rd(i));
    }
    /// rd = sext32(op32(x[rs1], x[rs2]))
    fn bin_w(&mut self, i: u32, base: u32) {
        let (a, b) = self.ops(i);
        let d = self.dst(rd(i));
        self.p(rrr(base, d, a, b));
        self.p(sxtw(d, d));
        self.fin(d, rd(i));
    }
    fn set_cond(&mut self, i: u32, cond: u32) {
        let (a, b) = self.ops(i);
        let d = self.dst(rd(i));
        self.p(cmp(a, b));
        self.p(cset(d, cond));
        self.fin(d, rd(i));
    }
    fn minmax(&mut self, i: u32, cond: u32) {
        let (a, b) = self.ops(i);
        let d = self.dst(rd(i));
        self.p(cmp(a, b));
        self.p(csel(0x9a80_0000, d, a, b, cond));
        self.fin(d, rd(i));
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
    /// rd = f(x[rs1]): `words(a, d)` reads a and writes d (each word may read what the
    /// previous ones wrote to d, never a after d was written)
    fn unary(&mut self, i: u32, words: impl Fn(u32, u32) -> Vec<u32>) {
        let a = self.src(A, rs1(i));
        let d = self.dst(rd(i));
        for w in words(a, d) {
            self.p(w);
        }
        self.fin(d, rd(i));
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
                self.interp(i, at, n, true);
                return true;
            }
            0x37 => {
                let d = self.dst(rd(i));
                self.movi(d, (i & 0xffff_f000) as i32 as i64 as u64);
                self.fin(d, rd(i));
            }
            0x17 => {
                let d = self.dst(rd(i));
                self.add_const(d, X_PAGE, at as i64 + (i & 0xffff_f000) as i32 as i64);
                self.fin(d, rd(i));
            }
            0x6f => {
                if rd(i) != 0 {
                    let d = self.dst(rd(i));
                    self.add_const(d, X_PAGE, next);
                    self.fin(d, rd(i));
                }
                self.exit_const(at as i64 + imm_j(i), n + 1);
                return true;
            }
            0x67 => {
                if f3(i) != 0 {
                    self.interp(i, at, n, true);
                    return true;
                }
                // the target before the link (rd may be rs1)
                let a = self.src_nz(A, rs1(i));
                self.add_const(A, a, imm_i(i));
                self.p(0x9200_0000 | logic_imm64(!1).unwrap() | A << 5 | A); // and xA, xA, #~1
                if rd(i) != 0 {
                    let d = self.dst(rd(i));
                    self.add_const(d, X_PAGE, next);
                    self.fin(d, rd(i));
                }
                self.count(n + 1);
                let l = self.stub_leave;
                self.br(0x1400_0000, l);
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
                        self.interp(i, at, n, true);
                        return true;
                    }
                };
                let (a, b) = self.ops(i);
                let skip = self.label();
                if b == ZR && matches!(cond, EQ | NE) {
                    // beqz, bnez: skip with cbnz, cbz
                    self.br(if cond == EQ { 0xb500_0000 } else { 0xb400_0000 } | a, skip);
                } else {
                    self.p(cmp(a, b));
                    self.br(0x5400_0000 | (cond ^ 1), skip);
                }
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
                        // addi, li, mv
                        let d = self.dst(rd(i));
                        if rs1(i) == 0 {
                            self.movi(d, imm as u64);
                        } else {
                            let a = self.src(A, rs1(i));
                            self.add_const(d, a, imm);
                        }
                        self.fin(d, rd(i));
                    }
                    2 | 3 => {
                        let a = self.src(A, rs1(i));
                        self.movi(B, imm as u64);
                        let d = self.dst(rd(i));
                        self.p(cmp(a, B));
                        self.p(cset(d, if f3(i) == 2 { LT } else { LO }));
                        self.fin(d, rd(i));
                    }
                    4 | 6 | 7 => {
                        let a = self.src(A, rs1(i));
                        let d = self.dst(rd(i));
                        let k = (f3(i) - 4) as usize;
                        match logic_imm64(imm as u64) {
                            // eor, orr, and (immediate)
                            Some(enc) => self.p([0xd200_0000, 0, 0xb200_0000, 0x9200_0000][k] | enc | a << 5 | d),
                            None => {
                                self.movi(B, imm as u64);
                                self.p(rrr([0xca00_0000, 0, 0xaa00_0000, 0x8a00_0000][k], d, a, B));
                            }
                        }
                        self.fin(d, rd(i));
                    }
                    1 => match i >> 26 {
                        0 => self.unary(i, |a, d| vec![ubfm(d, a, (64 - sh) & 63, 63 - sh)]),
                        0x18 => match (i >> 20) & 0x3f {
                            0 => self.unary(i, |a, d| vec![0xdac0_1000 | a << 5 | d]),                         // clz
                            1 => self.unary(i, |a, d| vec![0xdac0_0000 | a << 5 | d, 0xdac0_1000 | d << 5 | d]), // ctz
                            2 => {
                                self.get(A, rs1(i));
                                self.popcount();
                                self.put(R, rd(i));
                            }
                            4 => self.unary(i, |a, d| vec![sbfm(d, a, 0, 7)]),
                            5 => self.unary(i, |a, d| vec![sbfm(d, a, 0, 15)]),
                            _ => self.interp(i, at, n, false),
                        },
                        _ => self.interp(i, at, n, false),
                    },
                    _ => match i >> 26 {
                        0 => self.unary(i, |a, d| vec![ubfm(d, a, sh, 63)]),
                        0x10 => self.unary(i, |a, d| vec![sbfm(d, a, sh, 63)]),
                        0x18 => self.unary(i, |a, d| vec![0x93c0_0000 | a << 16 | sh << 10 | a << 5 | d]), // ror (extr)
                        _ if i >> 20 == 0x6b8 => self.unary(i, |a, d| vec![0xdac0_0c00 | a << 5 | d]),    // rev8
                        _ => self.interp(i, at, n, false),                                           // orc.b
                    },
                }
            }
            0x1b => {
                let sh = (i >> 20) & 31;
                match (f3(i), f7(i)) {
                    (0, _) => {
                        // addiw, sext.w
                        let a = self.src_nz(A, rs1(i));
                        let d = self.dst(rd(i));
                        self.add_const(d, a, imm_i(i));
                        self.p(sxtw(d, d));
                        self.fin(d, rd(i));
                    }
                    (1, 4) | (1, 5) => {
                        // slli.uw: ubfiz
                        let s = (i >> 20) & 63;
                        let width = 32.min(64 - s);
                        self.unary(i, |a, d| vec![ubfm(d, a, (64 - s) & 63, width - 1)]);
                    }
                    (1, 0) => self.unary(i, |a, d| vec![0x5300_0000 | ((32 - sh) & 31) << 16 | (31 - sh) << 10 | a << 5 | d, sxtw(d, d)]),
                    (5, 0) => self.unary(i, |a, d| vec![0x5300_0000 | sh << 16 | 31 << 10 | a << 5 | d, sxtw(d, d)]),
                    (5, 0x20) => self.unary(i, |a, d| vec![0x1300_0000 | sh << 16 | 31 << 10 | a << 5 | d, sxtw(d, d)]),
                    (5, 0x30) => self.unary(i, |a, d| vec![0x1380_0000 | a << 16 | sh << 10 | a << 5 | d, sxtw(d, d)]), // extr w
                    (1, 0x30) => match rs2(i) {
                        0 => self.unary(i, |a, d| vec![0x5ac0_1000 | a << 5 | d]),
                        1 => self.unary(i, |a, d| vec![0x5ac0_0000 | a << 5 | d, 0x5ac0_1000 | d << 5 | d]),
                        2 => {
                            self.get(A, rs1(i));
                            self.p(0x2a00_03e0 | A << 16 | A); // mov wA, wA (zero-extend)
                            self.popcount();
                            self.put(R, rd(i));
                        }
                        _ => self.interp(i, at, n, false),
                    },
                    _ => self.interp(i, at, n, false),
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
                _ => self.interp(i, at, n, false),
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
                (4, 4) if rs2(i) == 0 => self.unary(i, |a, d| vec![ubfm(d, a, 0, 15)]), // zext.h
                _ => self.interp(i, at, n, false),
            },
            0x0f if f3(i) == 0 => {} // fence: the harts run in turns on one host thread
            // CSR instructions: `h_step` leaves after those that can change what compiled
            // code depends on
            0x73 if f3(i) != 0 => self.interp(i, at, n, false),
            0x73 | 0x0f => {
                self.interp(i, at, n, true);
                return true;
            }
            0x07 if matches!(f3(i), 2 | 3) => self.fload(i, at, n),
            0x27 if matches!(f3(i), 2 | 3) => self.fstore(i, at, n),
            0x2f => self.amo(i, at, n),
            _ => self.interp(i, at, n, false), // F and D arithmetic
        }
        false
    }

    fn finish(mut self) -> Vec<u32> {
        for s in std::mem::take(&mut self.slow) {
            self.bind(s.label);
            self.interp(s.i, s.at, s.before, false);
            self.br(0x1400_0000, s.resume);
        }
        self.leave_stub();
        self.interp_stub();
        // h_step's status in w0 (the hart is up to date: written back before the call)
        let l = self.exit_h;
        self.bind(l);
        self.ret();
        for f in &self.fixes {
            let Fix::Branch(at, l) = *f;
            let target = self.labels[l].expect("unbound label");
            let off = target as i64 - at as i64;
            let w = self.out[at];
            self.out[at] = if w & 0x7c00_0000 == 0x1400_0000 {
                // B, BL
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
        jc: offset_of!(Hart, jit_jc),
        lr: offset_of!(Hart, jit_lr),
        tmp: offset_of!(Hart, jit_tmp),
        f: offset_of!(Hart, f),
        mstatus: offset_of!(Hart, mstatus),
        reservation: offset_of!(Hart, reservation),
    };
    let mut e = E { out: Vec::with_capacity(1024), o, step: step as u64, labels: Vec::new(), fixes: Vec::new(), slow: Vec::new(), exit_h: 0, region: Vec::new(), host: [0; 32], cached: 0, written: 0, stub_interp: 0, stub_leave: 0 };
    e.exit_h = e.label();
    e.stub_interp = e.label();
    e.stub_leave = e.label();
    // ---- the region: basic blocks reachable through constant branches on this page
    let mut bbs = vec![entry];
    let (mut lo, mut hi) = (entry, entry);
    let mut count = [0u32; 32];
    let mut written = 0u32;
    let mut q = 0;
    while q < bbs.len() {
        let mut at = bbs[q];
        q += 1;
        for _ in 0..MAX_INSNS {
            let Some((i, len)) = fetch(code, at) else { break };
            let (target, stop) = scan(i, at);
            let (r, w) = uses(i);
            for (g, c) in count.iter_mut().enumerate().skip(1) {
                *c += (r >> g & 1) + (w >> g & 1);
            }
            written |= w;
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
    // ---- the guest registers to keep in host registers: the most used, if used twice
    let mut by_use: Vec<usize> = (1..32).filter(|&g| count[g] >= 2).collect();
    by_use.sort_by_key(|&g| std::cmp::Reverse(count[g]));
    for (&g, &h) in by_use.iter().zip(CACHE_REGS.iter()) {
        e.host[g] = h;
        e.cached |= 1 << g;
    }
    e.written = written & e.cached;
    e.region = bbs.iter().map(|&s| (s, 0)).collect();
    for k in 0..e.region.len() {
        e.region[k].1 = e.label();
    }
    // ---- prologue
    e.p(0xa980_0000 | ((-(FRAME as i32) / 8) as u32 & 0x7f) << 15 | 30 << 10 | 31 << 5 | 29); // stp x29, x30, [sp, #-FRAME]!
    for (k, r) in [(1u32, 19u32), (2, 21), (3, 23), (4, 25), (5, 27)] {
        e.p(0xa900_0000 | (2 * k) << 15 | (r + 1) << 10 | 31 << 5 | r); // stp xr, xr+1, [sp, #16k]
    }
    e.p(mov(X_CPU, 0));
    e.p(ldr_x(X_PAGE, X_CPU, e.o.pc));
    e.p(0x9274_cc00 | X_PAGE << 5 | X_PAGE); // and x20, x20, #~0xfff
    e.p(ldr_x(X_UT, X_CPU, e.o.ut));
    e.p(ldr_x(X_JC, X_CPU, e.o.jc));
    e.p(ldr_x(X_CYC, X_CPU, e.o.cycle));
    assert_eq!(e.out.len() * 4, LINKED);
    e.load_cached(u32::MAX);
    for k in 0..bbs.len() {
        let (start, l) = e.region[k];
        e.bind(l);
        let mut n = 0u32;
        let mut at = start;
        loop {
            let Some((i, len)) = fetch(code, at) else {
                // straddles the page: the interpreter runs it (never through the region)
                e.count(n);
                e.sync();
                e.write_back();
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
