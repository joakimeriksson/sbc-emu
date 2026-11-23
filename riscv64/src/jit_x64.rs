//! The block JIT's native backend on x86-64 Linux hosts: RV64GC to x86-64.
//!
//! It follows `jit_a64.rs` instruction for instruction: the same regions (the entry basic
//! block and the blocks constant branches reach on the same page, with branches between them
//! as host branches while the cycle budget `chain_until` lasts), the same instructions
//! compiled and interpreted, the same inline micro-TLB probe for loads and stores, the same
//! region linking through the hart's jump cache, and the same two stubs at the end of each
//! region (one runs an instruction in `h_step`, one leaves for a pc). See that file for the
//! reasoning; this one only says where x86-64 differs.
//!
//! - **Fewer registers.** Up to 7 guest registers a region uses more than once live in host
//!   registers (r13, r14, r15, rdi, r8, r9, r10); AArch64 has room for 14.
//! - **Two-operand arithmetic.** Results are computed in their cached register (or rax, then
//!   stored), so most instructions are a load or move and one operation. Operands not in host
//!   registers are read straight from the hart as memory operands.
//! - **Division traps on x86.** A zero divisor and the signed overflow (`MIN / -1`) are
//!   branched around: RISC-V defines both results.
//! - **The stubs are called.** `call` pushes the return address, which keeps the stack
//!   16-byte aligned for `h_step` (the prologue's six pushes leave it 8 off); leaving from
//!   inside the interpreter stub drops that return address first.
//! - `clz`/`ctz`/`cpop` use LZCNT, TZCNT and POPCNT when the host has them (and go to the
//!   interpreter otherwise).
//!
//! Host registers: rbx = the hart, rbp = the page's virtual address, r12 = the micro-TLB
//! table (callee-saved, live across `h_step`); r11 = the cycle count; rax, rcx, rdx and rsi
//! are scratch. The jump cache table is read from the hart when it is needed.
use crate::mmu::{JEntry, UEntry, JC, UT};
use crate::{rvc, Hart};
use std::mem::{offset_of, size_of};

mod host {
    use std::ffi::{c_int, c_void};
    extern "C" {
        fn mmap(addr: *mut c_void, len: usize, prot: c_int, flags: c_int, fd: c_int, off: i64) -> *mut c_void;
        fn munmap(addr: *mut c_void, len: usize) -> c_int;
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
            // SAFETY: an anonymous mapping (PRIVATE | ANONYMOUS), read, write and execute,
            // chosen by the OS; failure is checked
            let p = unsafe { mmap(std::ptr::null_mut(), size, 7, 0x0002 | 0x0020, -1, 0) };
            if p as isize == -1 || p.is_null() {
                return None;
            }
            Some(CodeCache { base: p as *mut u8, size, used: 0 })
        }
        pub fn reset(&mut self) {
            self.used = 0;
        }
        /// Copy machine code in; returns its entry address, or None when the buffer is full.
        /// (x86 keeps instruction fetch coherent with stores: no cache maintenance.)
        pub fn write(&mut self, bytes: &[u8]) -> Option<*const u8> {
            // entries 16-byte aligned
            let off = (self.used + 15) & !15;
            if off + bytes.len() > self.size {
                return None;
            }
            // SAFETY: the range is inside the mapping
            unsafe { std::ptr::copy_nonoverlapping(bytes.as_ptr(), self.base.add(off), bytes.len()) };
            self.used = off + bytes.len();
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

// x86-64 registers
const RAX: u8 = 0;
const RCX: u8 = 1;
const RDX: u8 = 2;
const RBX: u8 = 3;
const RSP: u8 = 4;
const RBP: u8 = 5;
const RSI: u8 = 6;
const RDI: u8 = 7;
const R8: u8 = 8;
const R9: u8 = 9;
const R10: u8 = 10;
const R11: u8 = 11;
const R12: u8 = 12;
const R13: u8 = 13;
const R14: u8 = 14;
const R15: u8 = 15;

const X_CPU: u8 = RBX;
const X_PAGE: u8 = RBP;
const X_UT: u8 = R12;
/// The hart's cycle count while compiled code runs (instret is brought along at `sync`).
const X_CYC: u8 = R11;
/// Host registers for cached guest registers, callee-saved first.
const CACHE_REGS: [u8; 7] = [R13, R14, R15, RDI, R8, R9, R10];
/// Scratch: rax the result, rcx the second operand (and shift count), rdx the micro-TLB probe's
/// index and host delta (and the high half of products and quotients), rsi the address.
const R: u8 = RAX;
const T: u8 = RDX;
const ADDR: u8 = RSI;

/// Offset of a region's linked entry (after the prologue): rbx, r12 and the cycle count in
/// r11 are set, rbp is the virtual page of the pc entered.
pub(crate) const LINKED: usize = 41;

// condition codes
const CC_B: u8 = 0x2;
const CC_AE: u8 = 0x3;
const CC_E: u8 = 0x4;
const CC_NE: u8 = 0x5;
const CC_A: u8 = 0x7;
const CC_L: u8 = 0xc;
const CC_GE: u8 = 0xd;
const CC_G: u8 = 0xf;

// ALU operations: the `op reg, r/m` opcode, and the /digit of the immediate form
const ADD: (u8, u8) = (0x03, 0);
const OR: (u8, u8) = (0x0b, 1);
const AND: (u8, u8) = (0x23, 4);
const SUB: (u8, u8) = (0x2b, 5);
const XOR: (u8, u8) = (0x33, 6);
const CMP: (u8, u8) = (0x3b, 7);

// shift and rotate /digits
const ROL: u8 = 0;
const ROR: u8 = 1;
const SHL: u8 = 4;
const SHR: u8 = 5;
const SAR: u8 = 7;

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

// ---------------------------------------------------------------------------- encoder
/// A memory operand `[base + index * scale + disp]`.
#[derive(Clone, Copy)]
struct Mem {
    base: u8,
    index: Option<(u8, u8)>,
    disp: i32,
    /// always a 32-bit displacement (fixed-length code)
    wide: bool,
}

fn m(base: u8, disp: usize) -> Mem {
    Mem { base, index: None, disp: i32::try_from(disp).expect("displacement"), wide: false }
}
fn mi(base: u8, index: u8, scale: u8, disp: i32) -> Mem {
    assert!(index != RSP);
    Mem { base, index: Some((index, scale)), disp, wide: false }
}

/// A register or memory operand.
#[derive(Clone, Copy)]
enum Rm {
    R(u8),
    M(Mem),
}

/// x86-64 machine code.
#[derive(Default)]
struct Asm {
    out: Vec<u8>,
}

impl Asm {
    fn b(&mut self, v: u8) {
        self.out.push(v);
    }
    fn imm32(&mut self, v: i32) {
        self.out.extend_from_slice(&v.to_le_bytes());
    }
    /// One instruction: prefixes, REX (when needed: `w`, an extended register, or `byte` with
    /// a byte register spl..dil), opcode, ModRM/SIB/displacement for `reg` and `rm`.
    fn ins(&mut self, pre: &[u8], w: bool, op: &[u8], reg: u8, rm: Rm, byte: bool) {
        self.out.extend_from_slice(pre);
        let (x, b) = match rm {
            Rm::R(r) => (0, r),
            Rm::M(mm) => (mm.index.map_or(0, |(i, _)| i), mm.base),
        };
        let lowbyte = |r: u8| (4..8).contains(&r);
        let rex = 0x40 | (w as u8) << 3 | (reg >> 3 & 1) << 2 | (x >> 3 & 1) << 1 | (b >> 3 & 1);
        let byte_rex = byte && (lowbyte(reg) || matches!(rm, Rm::R(r) if lowbyte(r)));
        if rex != 0x40 || byte_rex {
            self.b(rex);
        }
        self.out.extend_from_slice(op);
        let reg = (reg & 7) << 3;
        match rm {
            Rm::R(r) => self.b(0xc0 | reg | (r & 7)),
            Rm::M(mm) => {
                let b7 = mm.base & 7;
                let sib = mm.index.is_some() || b7 == 4;
                let md = if mm.wide || i8::try_from(mm.disp).is_err() {
                    0x80
                } else if mm.disp != 0 || b7 == 5 {
                    0x40
                } else {
                    0
                };
                self.b(md | reg | if sib { 4 } else { b7 });
                if sib {
                    let (i, s) = mm.index.unwrap_or((4, 1));
                    let ss = match s {
                        1 => 0,
                        2 => 1,
                        4 => 2,
                        8 => 3,
                        _ => panic!("scale"),
                    };
                    self.b(ss << 6 | (i & 7) << 3 | b7);
                }
                match md {
                    0x40 => self.b(mm.disp as i8 as u8),
                    0x80 => self.imm32(mm.disp),
                    _ => {}
                }
            }
        }
    }
    /// `op d, rm` for the ALU operations (64-bit, or 32-bit when !w)
    fn alu(&mut self, op: (u8, u8), w: bool, d: u8, s: Rm) {
        self.ins(&[], w, &[op.0], d, s, false);
    }
    /// `op d, imm`
    fn alu_i(&mut self, op: (u8, u8), w: bool, d: Rm, v: i32) {
        if let Ok(v8) = i8::try_from(v) {
            self.ins(&[], w, &[0x83], op.1, d, false);
            self.b(v8 as u8);
        } else {
            self.ins(&[], w, &[0x81], op.1, d, false);
            self.imm32(v);
        }
    }
    /// `op [mem], s` (the store direction: add, or, and, sub, xor, cmp)
    fn alu_to_mem(&mut self, op: (u8, u8), m: Mem, s: u8) {
        self.ins(&[], true, &[op.0 - 2], s, Rm::M(m), false);
    }
    fn mov(&mut self, d: u8, s: u8) {
        if d != s {
            self.ins(&[], true, &[0x8b], d, Rm::R(s), false);
        }
    }
    /// `mov d32, s32` (zero-extends)
    fn mov32(&mut self, d: u8, s: Rm) {
        self.ins(&[], false, &[0x8b], d, s, false);
    }
    fn load(&mut self, d: u8, m: Mem) {
        self.ins(&[], true, &[0x8b], d, Rm::M(m), false);
    }
    fn store(&mut self, m: Mem, s: u8) {
        self.ins(&[], true, &[0x89], s, Rm::M(m), false);
    }
    /// A store of `size` bytes of `s`.
    fn store_n(&mut self, size: u32, m: Mem, s: u8) {
        match size {
            1 => self.ins(&[], false, &[0x88], s, Rm::M(m), true),
            2 => self.ins(&[0x66], false, &[0x89], s, Rm::M(m), false),
            4 => self.ins(&[], false, &[0x89], s, Rm::M(m), false),
            _ => self.store(m, s),
        }
    }
    fn lea(&mut self, d: u8, m: Mem) {
        self.ins(&[], true, &[0x8d], d, Rm::M(m), false);
    }
    /// d = v
    fn movi(&mut self, d: u8, v: u64) {
        if v <= u32::MAX as u64 {
            // mov r32, imm32 (zero-extends)
            if d >= 8 {
                self.b(0x41);
            }
            self.b(0xb8 + (d & 7));
            self.imm32(v as u32 as i32);
        } else if v as i64 == v as i32 as i64 {
            self.ins(&[], true, &[0xc7], 0, Rm::R(d), false);
            self.imm32(v as i32);
        } else {
            self.b(0x48 | (d >> 3 & 1));
            self.b(0xb8 + (d & 7));
            self.out.extend_from_slice(&v.to_le_bytes());
        }
    }
    /// `xor d32, d32`
    fn zero(&mut self, d: u8) {
        self.alu(XOR, false, d, Rm::R(d));
    }
    /// shift or rotate by an immediate
    fn shift_i(&mut self, op: u8, w: bool, d: u8, n: u32) {
        self.ins(&[], w, &[0xc1], op, Rm::R(d), false);
        self.b(n as u8);
    }
    /// shift or rotate by cl
    fn shift_cl(&mut self, op: u8, w: bool, d: u8) {
        self.ins(&[], w, &[0xd3], op, Rm::R(d), false);
    }
    /// the F7 group: not 2, neg 3, mul 4, imul 5, div 6, idiv 7
    fn f7(&mut self, op: u8, w: bool, s: Rm) {
        self.ins(&[], w, &[0xf7], op, s, false);
    }
    /// `imul d, s`
    fn imul(&mut self, w: bool, d: u8, s: Rm) {
        self.ins(&[], w, &[0x0f, 0xaf], d, s, false);
    }
    /// `movsxd d, s32`
    fn movsxd(&mut self, d: u8, s: Rm) {
        self.ins(&[], true, &[0x63], d, s, false);
    }
    /// `movsx d64, s8/s16`
    fn movsx(&mut self, bytes: u32, d: u8, s: Rm) {
        self.ins(&[], true, &[0x0f, if bytes == 1 { 0xbe } else { 0xbf }], d, s, bytes == 1);
    }
    /// `movzx d32, s8/s16`
    fn movzx(&mut self, bytes: u32, d: u8, s: Rm) {
        self.ins(&[], false, &[0x0f, if bytes == 1 { 0xb6 } else { 0xb7 }], d, s, bytes == 1);
    }
    /// `setcc d8`
    fn setcc(&mut self, cc: u8, d: u8) {
        self.ins(&[], false, &[0x0f, 0x90 | cc], 0, Rm::R(d), true);
    }
    fn cmov(&mut self, cc: u8, d: u8, s: Rm) {
        self.ins(&[], true, &[0x0f, 0x40 | cc], d, s, false);
    }
    fn test(&mut self, w: bool, a: u8, b: u8) {
        self.ins(&[], w, &[0x85], b, Rm::R(a), false);
    }
    fn test_i(&mut self, a: u8, v: i32) {
        self.ins(&[], false, &[0xf7], 0, Rm::R(a), false);
        self.imm32(v);
    }
    /// lzcnt (0xbd), tzcnt (0xbc), popcnt (0xb8)
    fn bitcount(&mut self, op: u8, w: bool, d: u8, s: Rm) {
        self.ins(&[0xf3], w, &[0x0f, op], d, s, false);
    }
    fn bswap(&mut self, d: u8) {
        self.b(0x48 | (d >> 3 & 1));
        self.b(0x0f);
        self.b(0xc8 + (d & 7));
    }
    /// `bts d, n`
    fn bts(&mut self, d: u8, n: u8) {
        self.ins(&[], true, &[0x0f, 0xba], 5, Rm::R(d), false);
        self.b(n);
    }
    fn push(&mut self, r: u8) {
        if r >= 8 {
            self.b(0x41);
        }
        self.b(0x50 + (r & 7));
    }
    fn pop(&mut self, r: u8) {
        if r >= 8 {
            self.b(0x41);
        }
        self.b(0x58 + (r & 7));
    }
    /// `call r`
    fn call_r(&mut self, r: u8) {
        self.ins(&[], false, &[0xff], 2, Rm::R(r), false);
    }
    /// `jmp [m]`
    fn jmp_m(&mut self, m: Mem) {
        self.ins(&[], false, &[0xff], 4, Rm::M(m), false);
    }
    fn ret(&mut self) {
        self.b(0xc3);
    }
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
    jc: usize,
    tmp: usize,
    f: usize,
    mstatus: usize,
    reservation: usize,
}

struct E {
    a: Asm,
    o: Off,
    step: u64,
    /// LZCNT, TZCNT and POPCNT
    bits: bool,
    labels: Vec<Option<usize>>,
    /// rel32 fields to fill in: (byte offset, label)
    fixes: Vec<(usize, usize)>,
    slow: Vec<Slow>,
    /// return with the status in eax (at the frame's level, and from inside the interpreter
    /// stub, with its return address on the stack)
    exit_h: usize,
    exit_h_stub: usize,
    /// the region's stubs: run an instruction in `h_step`, leave for the pc in rax
    stub_interp: usize,
    stub_leave: usize,
    /// page offsets of the region's basic blocks and their labels
    region: Vec<(u32, usize)>,
    /// the host register of each guest register (valid where `cached` has its bit)
    host: [u8; 32],
    /// guest registers in host registers, and those of them compiled code writes
    cached: u32,
    written: u32,
}

impl E {
    fn label(&mut self) -> usize {
        self.labels.push(None);
        self.labels.len() - 1
    }
    fn bind(&mut self, l: usize) {
        self.labels[l] = Some(self.a.out.len());
    }
    fn rel32(&mut self, l: usize) {
        self.fixes.push((self.a.out.len(), l));
        self.a.imm32(0);
    }
    fn jcc(&mut self, cc: u8, l: usize) {
        self.a.b(0x0f);
        self.a.b(0x80 | cc);
        self.rel32(l);
    }
    fn jmp(&mut self, l: usize) {
        self.a.b(0xe9);
        self.rel32(l);
    }
    fn call(&mut self, l: usize) {
        self.a.b(0xe8);
        self.rel32(l);
    }
    fn xm(&self, r: u32) -> Mem {
        m(X_CPU, self.o.x + 8 * r as usize)
    }
    /// d = n + v
    fn add_const(&mut self, d: u8, n: u8, v: i64) {
        if v == 0 {
            self.a.mov(d, n);
        } else if let Ok(v) = i32::try_from(v) {
            self.a.lea(d, Mem { base: n, index: None, disp: v, wide: false });
        } else {
            assert!(d != T && n != T);
            self.a.movi(T, v as u64);
            self.a.lea(d, mi(n, T, 1, 0));
        }
    }
    fn is_cached(&self, r: u32) -> bool {
        r != 0 && self.cached & 1 << r != 0
    }
    /// Guest register `r` into host `h`.
    fn get(&mut self, h: u8, r: u32) {
        if r == 0 {
            self.a.zero(h);
        } else if self.is_cached(r) {
            self.a.mov(h, self.host[r as usize]);
        } else {
            self.a.load(h, self.xm(r));
        }
    }
    /// Host `h` into guest register `r`.
    fn put(&mut self, h: u8, r: u32) {
        if r == 0 {
        } else if self.is_cached(r) {
            debug_assert!(self.written & 1 << r != 0);
            self.a.mov(self.host[r as usize], h);
        } else {
            self.a.store(self.xm(r), h);
        }
    }
    /// A host register holding guest register `r` for reading: its cached register, or
    /// `scratch` loaded from the hart (zeroed for x0).
    fn src(&mut self, scratch: u8, r: u32) -> u8 {
        if self.is_cached(r) {
            self.host[r as usize]
        } else {
            self.get(scratch, r);
            scratch
        }
    }
    /// Guest register `r` as an operand: its cached register, its place in the hart, or
    /// `scratch` zeroed for x0.
    fn src_rm(&mut self, scratch: u8, r: u32) -> Rm {
        if r == 0 {
            self.a.zero(scratch);
            Rm::R(scratch)
        } else if self.is_cached(r) {
            Rm::R(self.host[r as usize])
        } else {
            Rm::M(self.xm(r))
        }
    }
    /// Where to compute guest register `r`: its cached register, or rax (then `fin` stores
    /// it).
    fn dst(&self, r: u32) -> u8 {
        if self.is_cached(r) {
            debug_assert!(self.written & 1 << r != 0);
            self.host[r as usize]
        } else {
            R
        }
    }
    /// Finish a result computed in `dst(r)`.
    fn fin(&mut self, h: u8, r: u32) {
        if r != 0 && !self.is_cached(r) {
            self.a.store(self.xm(r), h);
        }
    }
    /// Load cached guest registers from the hart.
    fn load_cached(&mut self) {
        for r in 1..32 {
            if self.cached & 1 << r != 0 {
                self.a.load(self.host[r], self.xm(r as u32));
            }
        }
    }
    /// Write the cached guest registers compiled code changes back to the hart.
    fn write_back(&mut self) {
        for r in 1..32 {
            if self.written & self.cached & 1 << r != 0 {
                self.a.store(self.xm(r as u32), self.host[r]);
            }
        }
    }
    fn set_pc(&mut self, rel: i64) {
        self.add_const(R, X_PAGE, rel);
        self.a.store(m(X_CPU, self.o.pc), R);
    }
    /// The cycle count += k (the instructions of a basic block, counted where it ends).
    fn count(&mut self, k: u32) {
        if k > 0 {
            self.a.alu_i(ADD, true, Rm::R(X_CYC), k as i32);
        }
    }
    /// Bring the hart's counters up to date with r11 (instret moves as much as cycle did);
    /// clobbers rdx.
    fn sync(&mut self) {
        self.a.mov(RDX, X_CYC);
        self.a.alu(SUB, true, RDX, Rm::M(m(X_CPU, self.o.cycle)));
        self.a.alu_to_mem(ADD, m(X_CPU, self.o.instret), RDX);
        self.a.store(m(X_CPU, self.o.cycle), X_CYC);
    }
    /// Restore the callee-saved registers and return (eax = status).
    fn ret(&mut self) {
        for r in [R15, R14, R13, R12, RBX, RBP] {
            self.a.pop(r);
        }
        self.a.ret();
    }
    fn epilogue(&mut self, status: u32) {
        self.a.movi(RAX, status as u64);
        self.ret();
    }
    /// The chain test: below when the cycle budget lasts.
    fn cmp_chain(&mut self) {
        self.a.alu(CMP, true, X_CYC, Rm::M(m(X_CPU, self.o.chain)));
    }
    /// Leave for page offset `target` after `count` instructions of the basic block: a
    /// target in the region is a branch to it while the budget lasts.
    fn exit_const(&mut self, target: i64, count: u32) {
        self.count(count);
        if let Some(&(_, l)) = self.region.iter().find(|&&(s, _)| s as i64 == target) {
            self.cmp_chain();
            self.jcc(CC_B, l);
        }
        self.add_const(RAX, X_PAGE, target);
        let l = self.stub_leave;
        self.jmp(l);
    }
    /// The leave stub: leave for the pc in rax (the block's instructions counted in r11): into
    /// the region the jump cache has for it while the budget lasts, otherwise back to the
    /// driver.
    fn leave_stub(&mut self) {
        let l = self.stub_leave;
        self.bind(l);
        self.write_back();
        let out = self.label();
        self.cmp_chain();
        self.jcc(CC_AE, out);
        const _: () = assert!(size_of::<JEntry>() == 16);
        // rcx = the entry: jit_jc + ((pc >> 1) & (JC - 1)) * 16
        self.a.mov(RCX, RAX);
        self.a.shift_i(SHL, true, RCX, 3);
        self.a.alu_i(AND, false, Rm::R(RCX), ((JC as i32) - 1) << 4);
        self.a.alu(ADD, true, RCX, Rm::M(m(X_CPU, self.o.jc)));
        self.a.alu(CMP, true, RAX, Rm::M(m(RCX, 0)));
        self.jcc(CC_NE, out);
        self.a.mov(X_PAGE, RAX);
        self.a.alu_i(AND, true, Rm::R(X_PAGE), -4096);
        self.a.jmp_m(m(RCX, 8));
        // the counters go on in r11 into a linked region; the hart's only on the way out
        self.bind(out);
        self.sync();
        self.a.store(m(X_CPU, self.o.pc), RAX);
        self.epilogue(0);
    }
    /// Run the instruction at page offset `at` (the `before`-th of its basic block) in the
    /// interpreter; `last`: the block ends after it.
    fn interp(&mut self, at: u32, before: u32, last: bool) {
        self.a.movi(RSI, ((last as u32) << 16) as u64);
        self.a.movi(RCX, at as u64);
        self.a.movi(RAX, before as u64);
        let l = self.stub_interp;
        self.call(l);
        if last {
            let l = self.exit_h;
            self.jmp(l);
        }
    }
    /// The interpreter stub (esi = h_step's info, ecx = the instruction's page offset, eax =
    /// the instructions of its basic block before it; called): count those, bring the hart up
    /// to date, run the instruction; leave if it says so, otherwise load everything again (it
    /// may have written its rd, and the call lost the caller-saved registers).
    fn interp_stub(&mut self) {
        let l = self.stub_interp;
        self.bind(l);
        self.a.store(m(X_CPU, self.o.tmp), RAX);
        self.a.alu(ADD, true, X_CYC, Rm::R(RAX));
        self.sync();
        self.write_back();
        self.a.lea(RAX, mi(X_PAGE, RCX, 1, 0));
        self.a.store(m(X_CPU, self.o.pc), RAX);
        self.a.mov(RDI, X_CPU);
        self.a.movi(RAX, self.step);
        // the stack is 16-byte aligned here: the prologue's pushes and this stub's return
        // address
        self.a.call_r(RAX);
        self.a.test(false, RAX, RAX);
        let l = self.exit_h_stub;
        self.jcc(CC_NE, l);
        self.load_cached();
        // h_step left the counts where they were before the instruction; the block counts
        // its instructions where it ends
        self.a.load(X_CYC, m(X_CPU, self.o.cycle));
        self.a.alu(SUB, true, X_CYC, Rm::M(m(X_CPU, self.o.tmp)));
        self.a.ret();
    }

    // ------------------------------------------------------------------ memory
    /// The address x[base] + imm; probe the micro-TLB for a `size`-byte access there
    /// (`aligned`: only an aligned one, otherwise any that stays in the page): on a hit rdx =
    /// the host page minus the guest page and the access is [rdx + the returned register],
    /// otherwise branch to the slow path.
    fn probe(&mut self, base: u32, imm: i64, size: u32, write: bool, aligned: bool, slow: usize) -> u8 {
        let addr = if base == 0 {
            self.a.movi(ADDR, imm as u64);
            ADDR
        } else {
            let b = self.src(ADDR, base);
            if imm == 0 {
                b
            } else {
                self.add_const(ADDR, b, imm);
                ADDR
            }
        };
        // rdx = the entry's offset in the table
        self.a.mov(RDX, addr);
        self.a.shift_i(SHR, true, RDX, 8);
        const _: () = assert!(UT == 256 && size_of::<UEntry>() == 16);
        self.a.alu_i(AND, false, Rm::R(RDX), 0xff0);
        // rcx = the tag the access needs
        if aligned || size == 1 {
            self.a.mov(RCX, addr);
            self.a.alu_i(AND, true, Rm::R(RCX), -4096 | (size as i32 - 1));
        } else {
            // the page of the last byte: the entry at the first byte's index can only have
            // that tag if both bytes are on its page
            self.a.lea(RCX, Mem { base: addr, index: None, disp: size as i32 - 1, wide: false });
            self.a.alu_i(AND, true, Rm::R(RCX), -4096);
        }
        let table = if write { (UT * size_of::<UEntry>()) as i32 } else { 0 };
        self.a.alu(CMP, true, RCX, Rm::M(mi(X_UT, RDX, 1, table)));
        self.jcc(CC_NE, slow);
        self.a.load(RDX, mi(X_UT, RDX, 1, table + 8));
        addr
    }
    fn slow_path(&mut self, at: u32, before: u32) -> (usize, usize) {
        let label = self.label();
        let resume = self.label();
        self.slow.push(Slow { label, at, before, resume });
        (label, resume)
    }
    fn load(&mut self, i: u32, at: u32, n: u32) {
        let size = match f3(i) {
            0 | 4 => 1,
            1 | 5 => 2,
            2 | 6 => 4,
            3 => 8,
            _ => return self.interp(at, n, false),
        };
        let (slow, resume) = self.slow_path(at, n);
        let addr = self.probe(rs1(i), imm_i(i), size, false, false, slow);
        let d = self.dst(rd(i));
        let mem = Rm::M(mi(RDX, addr, 1, 0));
        match f3(i) {
            0 => self.a.movsx(1, d, mem),
            1 => self.a.movsx(2, d, mem),
            2 => self.a.movsxd(d, mem),
            3 => self.a.ins(&[], true, &[0x8b], d, mem, false),
            4 => self.a.movzx(1, d, mem),
            5 => self.a.movzx(2, d, mem),
            _ => self.a.mov32(d, mem),
        }
        self.fin(d, rd(i));
        self.bind(resume);
    }
    fn store(&mut self, i: u32, at: u32, n: u32) {
        if f3(i) >= 4 {
            return self.interp(at, n, false);
        }
        let size = 1 << f3(i);
        let (slow, resume) = self.slow_path(at, n);
        let addr = self.probe(rs1(i), imm_s(i), size, true, false, slow);
        let v = self.src(RCX, rs2(i));
        self.a.store_n(size, mi(RDX, addr, 1, 0), v);
        self.bind(resume);
    }

    /// Branch to `slow` unless the FPU is on.
    fn fs_on(&mut self, slow: usize) {
        self.a.load(RDX, m(X_CPU, self.o.mstatus));
        self.a.test_i(RDX, 3 << 13);
        self.jcc(CC_E, slow);
    }
    /// flw, fld
    fn fload(&mut self, i: u32, at: u32, n: u32) {
        let size = if f3(i) == 2 { 4 } else { 8 };
        let (slow, resume) = self.slow_path(at, n);
        self.fs_on(slow);
        let addr = self.probe(rs1(i), imm_i(i), size, false, false, slow);
        let mem = mi(RDX, addr, 1, 0);
        if size == 4 {
            self.a.mov32(RAX, Rm::M(mem));
            self.a.movi(RCX, 0xffff_ffff_0000_0000); // NaN-box
            self.a.alu(OR, true, RAX, Rm::R(RCX));
        } else {
            self.a.load(RAX, mem);
        }
        self.a.store(m(X_CPU, self.o.f + 8 * rd(i) as usize), RAX);
        // FS = dirty, SD
        self.a.load(RDX, m(X_CPU, self.o.mstatus));
        self.a.alu_i(OR, true, Rm::R(RDX), 3 << 13);
        self.a.bts(RDX, 63);
        self.a.store(m(X_CPU, self.o.mstatus), RDX);
        self.bind(resume);
    }
    /// fsw, fsd
    fn fstore(&mut self, i: u32, at: u32, n: u32) {
        let size = if f3(i) == 2 { 4 } else { 8 };
        let (slow, resume) = self.slow_path(at, n);
        self.fs_on(slow);
        let addr = self.probe(rs1(i), imm_s(i), size, true, false, slow);
        self.a.load(RAX, m(X_CPU, self.o.f + 8 * rs2(i) as usize));
        self.a.store_n(size, mi(RDX, addr, 1, 0), RAX);
        self.bind(resume);
    }
    /// AMOs other than LR and SC, on RAM with no reservation held (anything else is
    /// interpreted)
    fn amo(&mut self, i: u32, at: u32, n: u32) {
        let w = f3(i) == 2;
        let op = i >> 27;
        if !matches!(f3(i), 2 | 3) || !matches!(op, 0x00 | 0x01 | 0x04 | 0x08 | 0x0c | 0x10 | 0x14 | 0x18 | 0x1c) {
            return self.interp(at, n, false);
        }
        let (slow, resume) = self.slow_path(at, n);
        self.a.alu_i(CMP, true, Rm::M(m(X_CPU, self.o.reservation)), -1);
        self.jcc(CC_NE, slow);
        let addr = self.probe(rs1(i), 0, if w { 4 } else { 8 }, true, true, slow);
        // rdx = the host address; rax = old (sign-extended for .w), rcx = x[rs2], rsi = new
        self.a.lea(RDX, mi(RDX, addr, 1, 0));
        if w {
            self.a.movsxd(RAX, Rm::M(m(RDX, 0)));
        } else {
            self.a.load(RAX, m(RDX, 0));
        }
        self.get(RCX, rs2(i));
        let new = RSI;
        match op {
            0x01 => self.a.mov(new, RCX),
            0x00 | 0x04 | 0x0c | 0x08 => {
                self.a.mov(new, RAX);
                let alu = match op {
                    0x00 => ADD,
                    0x04 => XOR,
                    0x0c => AND,
                    _ => OR,
                };
                self.a.alu(alu, true, new, Rm::R(RCX));
            }
            _ => {
                // new = old if it wins the comparison, else x[rs2]
                let (cc, signed) = match op {
                    0x10 => (CC_L, true),
                    0x14 => (CC_G, true),
                    0x18 => (CC_B, false),
                    _ => (CC_A, false),
                };
                if w && signed {
                    self.a.movsxd(RCX, Rm::R(RCX));
                }
                self.a.mov(new, RCX);
                self.a.alu(CMP, !(w && !signed), RAX, Rm::R(RCX));
                self.a.cmov(cc, new, Rm::R(RAX));
            }
        }
        self.a.store_n(if w { 4 } else { 8 }, m(RDX, 0), new);
        self.put(RAX, rd(i));
        self.bind(resume);
    }

    // ------------------------------------------------------------------ arithmetic
    /// x[rs2] as an operand that survives writing rd's host register.
    fn operand2(&mut self, i: u32) -> Rm {
        let (r1, r2, d) = (rs1(i), rs2(i), rd(i));
        if r2 == d && r1 != r2 && self.is_cached(r2) {
            self.a.mov(RCX, self.host[r2 as usize]);
            Rm::R(RCX)
        } else {
            self.src_rm(RCX, r2)
        }
    }
    /// rd = op(x[rs1], x[rs2]): `w` 64-bit; otherwise sext32 of the 32-bit operation.
    fn bin(&mut self, i: u32, f: impl Fn(&mut Asm, bool, u8, Rm), w: bool) {
        let b = self.operand2(i);
        let d = self.dst(rd(i));
        self.get(d, rs1(i));
        f(&mut self.a, w, d, b);
        if !w {
            self.a.movsxd(d, Rm::R(d));
        }
        self.fin(d, rd(i));
    }
    fn alu_rr(&mut self, i: u32, op: (u8, u8), w: bool) {
        self.bin(i, move |a, w, d, b| a.alu(op, w, d, b), w);
    }
    /// x[rs1] `op` ~x[rs2] (andn, orn, xnor)
    fn bin_not(&mut self, i: u32, op: (u8, u8)) {
        self.get(RCX, rs2(i));
        self.a.f7(2, true, Rm::R(RCX));
        let d = self.dst(rd(i));
        self.get(d, rs1(i));
        self.a.alu(op, true, d, Rm::R(RCX));
        self.fin(d, rd(i));
    }
    /// shifts and rotates by x[rs2]
    fn shift_reg(&mut self, i: u32, op: u8, w: bool) {
        self.get(RCX, rs2(i));
        let d = self.dst(rd(i));
        self.get(d, rs1(i));
        self.a.shift_cl(op, w, d);
        if !w {
            self.a.movsxd(d, Rm::R(d));
        }
        self.fin(d, rd(i));
    }
    /// rd = f(x[rs1]) computed in place
    fn unary(&mut self, i: u32, f: impl Fn(&mut Asm, u8)) {
        let d = self.dst(rd(i));
        self.get(d, rs1(i));
        f(&mut self.a, d);
        self.fin(d, rd(i));
    }
    /// rd = f(x[rs1] as an operand) (`f` writes d without reading it)
    fn unary_rm(&mut self, i: u32, f: impl Fn(&mut Asm, u8, Rm)) {
        let s = self.src_rm(RCX, rs1(i));
        let d = self.dst(rd(i));
        f(&mut self.a, d, s);
        self.fin(d, rd(i));
    }
    fn set_cond(&mut self, i: u32, cc: u8) {
        let a = self.src(RAX, rs1(i));
        let b = self.src_rm(RCX, rs2(i));
        self.a.alu(CMP, true, a, b);
        self.a.setcc(cc, RAX);
        self.a.movzx(1, RAX, Rm::R(RAX));
        self.put(RAX, rd(i));
    }
    /// min, max: x[rs2] replaces x[rs1] when the condition holds of (x[rs1], x[rs2])
    fn minmax(&mut self, i: u32, cc: u8) {
        self.get(RAX, rs1(i));
        let b = self.src_rm(RCX, rs2(i));
        self.a.alu(CMP, true, RAX, b);
        self.a.cmov(cc, RAX, b);
        self.put(RAX, rd(i));
    }
    /// division and remainder with RISC-V's results for a zero divisor and for overflow
    /// (both of which trap on x86); `w64`: the 64-bit forms
    fn divrem(&mut self, i: u32, w64: bool) {
        self.get(RAX, rs1(i));
        self.get(RCX, rs2(i));
        let signed = f3(i) & 1 == 0;
        let rem = f3(i) >= 6;
        let (zero, done) = (self.label(), self.label());
        self.a.test(w64, RCX, RCX);
        self.jcc(CC_E, zero);
        if signed {
            // x / -1 = -x (MIN / -1 = MIN), x % -1 = 0: no idiv, which traps on MIN / -1
            let normal = self.label();
            self.a.alu_i(CMP, w64, Rm::R(RCX), -1);
            self.jcc(CC_NE, normal);
            if rem {
                self.a.zero(RAX);
            } else {
                self.a.f7(3, w64, Rm::R(RAX)); // neg
            }
            self.jmp(done);
            self.bind(normal);
            if w64 {
                self.a.b(0x48); // cqo
            }
            self.a.b(0x99); // cdq
            self.a.f7(7, w64, Rm::R(RCX));
        } else {
            self.a.zero(RDX);
            self.a.f7(6, w64, Rm::R(RCX));
        }
        if rem {
            self.a.mov(RAX, RDX);
        }
        self.jmp(done);
        // a zero divisor: all ones, or the dividend
        self.bind(zero);
        if !rem {
            self.a.alu_i(OR, true, Rm::R(RAX), -1);
        }
        self.bind(done);
        if !w64 {
            self.a.movsxd(RAX, Rm::R(RAX));
        }
        self.put(RAX, rd(i));
    }
    /// rd = x[rs2] + (x[rs1] (zero-extended from 32 bits when `uw`) << sh)
    fn shadd(&mut self, i: u32, sh: u32, uw: bool) {
        let a = if uw {
            let s = self.src_rm(RCX, rs1(i));
            self.a.mov32(RCX, s);
            RCX
        } else {
            self.src(RCX, rs1(i))
        };
        let b = self.src(RAX, rs2(i));
        let d = self.dst(rd(i));
        self.a.lea(d, mi(b, a, 1 << sh, 0));
        self.fin(d, rd(i));
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
                let d = self.dst(rd(i));
                self.a.movi(d, (i & 0xffff_f000) as i32 as i64 as u64);
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
                    self.interp(at, n, true);
                    return true;
                }
                // the target in rax before the link (rd may be rs1)
                self.get(RAX, rs1(i));
                self.add_const(RAX, RAX, imm_i(i));
                self.a.alu_i(AND, true, Rm::R(RAX), -2);
                if rd(i) != 0 {
                    let d = if self.is_cached(rd(i)) { self.host[rd(i) as usize] } else { RDX };
                    self.add_const(d, X_PAGE, next);
                    self.fin(d, rd(i));
                }
                self.count(n + 1);
                let l = self.stub_leave;
                self.jmp(l);
                return true;
            }
            0x63 => {
                let cc = match f3(i) {
                    0 => CC_E,
                    1 => CC_NE,
                    4 => CC_L,
                    5 => CC_GE,
                    6 => CC_B,
                    7 => CC_AE,
                    _ => {
                        self.interp(at, n, true);
                        return true;
                    }
                };
                let a = self.src(RAX, rs1(i));
                if rs2(i) == 0 {
                    self.a.test(true, a, a);
                } else {
                    let b = self.src_rm(RCX, rs2(i));
                    self.a.alu(CMP, true, a, b);
                }
                let skip = self.label();
                self.jcc(cc ^ 1, skip);
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
                            self.a.movi(d, imm as u64);
                        } else {
                            let a = self.src(d, rs1(i));
                            self.add_const(d, a, imm);
                        }
                        self.fin(d, rd(i));
                    }
                    2 | 3 => {
                        let a = self.src(RAX, rs1(i));
                        self.a.alu_i(CMP, true, Rm::R(a), imm as i32);
                        self.a.setcc(if f3(i) == 2 { CC_L } else { CC_B }, RAX);
                        self.a.movzx(1, RAX, Rm::R(RAX));
                        self.put(RAX, rd(i));
                    }
                    4 | 6 | 7 => {
                        let op = match f3(i) {
                            4 => XOR,
                            6 => OR,
                            _ => AND,
                        };
                        self.unary(i, |a, d| a.alu_i(op, true, Rm::R(d), imm as i32));
                    }
                    1 => match i >> 26 {
                        0 => self.unary(i, |a, d| a.shift_i(SHL, true, d, sh)),
                        0x18 => match (i >> 20) & 0x3f {
                            0 if self.bits => self.unary_rm(i, |a, d, s| a.bitcount(0xbd, true, d, s)), // clz
                            1 if self.bits => self.unary_rm(i, |a, d, s| a.bitcount(0xbc, true, d, s)), // ctz
                            2 if self.bits => self.unary_rm(i, |a, d, s| a.bitcount(0xb8, true, d, s)), // cpop
                            4 => self.unary_rm(i, |a, d, s| a.movsx(1, d, s)),
                            5 => self.unary_rm(i, |a, d, s| a.movsx(2, d, s)),
                            _ => self.interp(at, n, false),
                        },
                        _ => self.interp(at, n, false),
                    },
                    _ => match i >> 26 {
                        0 => self.unary(i, |a, d| a.shift_i(SHR, true, d, sh)),
                        0x10 => self.unary(i, |a, d| a.shift_i(SAR, true, d, sh)),
                        0x18 => self.unary(i, |a, d| a.shift_i(ROR, true, d, sh)),
                        _ if i >> 20 == 0x6b8 => self.unary(i, |a, d| a.bswap(d)), // rev8
                        _ => self.interp(at, n, false),                            // orc.b
                    },
                }
            }
            0x1b => {
                let sh = (i >> 20) & 31;
                let sext = |a: &mut Asm, d: u8| a.movsxd(d, Rm::R(d));
                match (f3(i), f7(i)) {
                    (0, _) => {
                        // addiw, sext.w
                        let imm = imm_i(i) as i32;
                        self.unary(i, |a, d| {
                            if imm != 0 {
                                a.alu_i(ADD, false, Rm::R(d), imm);
                            }
                            sext(a, d)
                        });
                    }
                    (1, 4) | (1, 5) => {
                        // slli.uw
                        let s = (i >> 20) & 63;
                        self.unary(i, |a, d| {
                            a.mov32(d, Rm::R(d));
                            if s != 0 {
                                a.shift_i(SHL, true, d, s);
                            }
                        });
                    }
                    (1, 0) => self.unary(i, |a, d| {
                        a.shift_i(SHL, false, d, sh);
                        sext(a, d)
                    }),
                    (5, 0) => self.unary(i, |a, d| {
                        a.shift_i(SHR, false, d, sh);
                        sext(a, d)
                    }),
                    (5, 0x20) => self.unary(i, |a, d| {
                        a.shift_i(SAR, false, d, sh);
                        sext(a, d)
                    }),
                    (5, 0x30) => self.unary(i, |a, d| {
                        a.shift_i(ROR, false, d, sh);
                        sext(a, d)
                    }),
                    (1, 0x30) if self.bits => match rs2(i) {
                        0 => self.unary_rm(i, |a, d, s| a.bitcount(0xbd, false, d, s)), // clzw
                        1 => self.unary_rm(i, |a, d, s| a.bitcount(0xbc, false, d, s)), // ctzw
                        2 => self.unary_rm(i, |a, d, s| a.bitcount(0xb8, false, d, s)), // cpopw
                        _ => self.interp(at, n, false),
                    },
                    _ => self.interp(at, n, false),
                }
            }
            0x33 => match (f7(i), f3(i)) {
                (0, 0) => self.alu_rr(i, ADD, true),
                (0x20, 0) => self.alu_rr(i, SUB, true),
                (0, 1) => self.shift_reg(i, SHL, true),
                (0, 2) => self.set_cond(i, CC_L),
                (0, 3) => self.set_cond(i, CC_B),
                (0, 4) => self.alu_rr(i, XOR, true),
                (0, 5) => self.shift_reg(i, SHR, true),
                (0x20, 5) => self.shift_reg(i, SAR, true),
                (0, 6) => self.alu_rr(i, OR, true),
                (0, 7) => self.alu_rr(i, AND, true),
                (1, 0) => self.bin(i, |a, w, d, b| a.imul(w, d, b), true),
                (1, 1) | (1, 3) => {
                    // mulh, mulhu: the high half in rdx
                    self.get(RAX, rs1(i));
                    let b = self.src_rm(RCX, rs2(i));
                    self.a.f7(if f3(i) == 1 { 5 } else { 4 }, true, b);
                    self.put(RDX, rd(i));
                }
                (1, 2) => {
                    // mulhsu = mulhu(a, b) - (a < 0 ? b : 0)
                    self.get(RAX, rs1(i));
                    self.a.mov(RSI, RAX);
                    self.get(RCX, rs2(i));
                    self.a.f7(4, true, Rm::R(RCX));
                    self.a.shift_i(SAR, true, RSI, 63);
                    self.a.alu(AND, true, RSI, Rm::R(RCX));
                    self.a.alu(SUB, true, RDX, Rm::R(RSI));
                    self.put(RDX, rd(i));
                }
                (1, 4..=7) => self.divrem(i, true),
                (0x20, 7) => self.bin_not(i, AND), // andn
                (0x20, 6) => self.bin_not(i, OR),  // orn
                (0x20, 4) => self.bin_not(i, XOR), // xnor
                (5, 4) => self.minmax(i, CC_G),    // min
                (5, 5) => self.minmax(i, CC_A),    // minu
                (5, 6) => self.minmax(i, CC_L),    // max
                (5, 7) => self.minmax(i, CC_B),    // maxu
                (0x30, 5) => self.shift_reg(i, ROR, true),
                (0x30, 1) => self.shift_reg(i, ROL, true),
                (0x10, 2) | (0x10, 4) | (0x10, 6) => self.shadd(i, f3(i) / 2, false),
                _ => self.interp(at, n, false),
            },
            0x3b => match (f7(i), f3(i)) {
                (0, 0) => self.alu_rr(i, ADD, false),
                (0x20, 0) => self.alu_rr(i, SUB, false),
                (1, 0) => self.bin(i, |a, w, d, b| a.imul(w, d, b), false),
                (0, 1) => self.shift_reg(i, SHL, false),
                (0, 5) => self.shift_reg(i, SHR, false),
                (0x20, 5) => self.shift_reg(i, SAR, false),
                (0x30, 5) => self.shift_reg(i, ROR, false),
                (0x30, 1) => self.shift_reg(i, ROL, false),
                (1, 4..=7) => self.divrem(i, false),
                (4, 0) => self.shadd(i, 0, true),
                (0x10, 2) | (0x10, 4) | (0x10, 6) => self.shadd(i, f3(i) / 2, true),
                (4, 4) if rs2(i) == 0 => self.unary_rm(i, |a, d, s| a.movzx(2, d, s)), // zext.h
                _ => self.interp(at, n, false),
            },
            0x0f if f3(i) == 0 => {} // fence: the harts run in turns on one host thread
            // CSR instructions: `h_step` leaves after those that can change what compiled
            // code depends on
            0x73 if f3(i) != 0 => self.interp(at, n, false),
            0x73 | 0x0f => {
                self.interp(at, n, true);
                return true;
            }
            0x07 if matches!(f3(i), 2 | 3) => self.fload(i, at, n),
            0x27 if matches!(f3(i), 2 | 3) => self.fstore(i, at, n),
            0x2f => self.amo(i, at, n),
            _ => self.interp(at, n, false), // F and D arithmetic
        }
        false
    }

    fn finish(mut self) -> Vec<u8> {
        for s in std::mem::take(&mut self.slow) {
            self.bind(s.label);
            self.interp(s.at, s.before, false);
            self.jmp(s.resume);
        }
        self.leave_stub();
        self.interp_stub();
        // h_step's status in eax (the hart is up to date: written back before the call)
        let l = self.exit_h_stub;
        self.bind(l);
        self.a.alu_i(ADD, true, Rm::R(RSP), 8); // the stub's return address
        let l = self.exit_h;
        self.bind(l);
        self.ret();
        for &(at, l) in &self.fixes {
            let target = self.labels[l].expect("unbound label");
            let rel = (target as i64 - (at as i64 + 4)) as i32;
            self.a.out[at..at + 4].copy_from_slice(&rel.to_le_bytes());
        }
        self.a.out
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
        tmp: offset_of!(Hart, jit_tmp),
        f: offset_of!(Hart, f),
        mstatus: offset_of!(Hart, mstatus),
        reservation: offset_of!(Hart, reservation),
    };
    let bits = std::arch::is_x86_feature_detected!("lzcnt") && std::arch::is_x86_feature_detected!("bmi1") && std::arch::is_x86_feature_detected!("popcnt");
    let mut e = E {
        a: Asm { out: Vec::with_capacity(4096) },
        o,
        step: step as u64,
        bits,
        labels: Vec::new(),
        fixes: Vec::new(),
        slow: Vec::new(),
        exit_h: 0,
        exit_h_stub: 0,
        stub_interp: 0,
        stub_leave: 0,
        region: Vec::new(),
        host: [0; 32],
        cached: 0,
        written: 0,
    };
    e.exit_h = e.label();
    e.exit_h_stub = e.label();
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
    // ---- prologue (fixed length: the linked entry follows it)
    for r in [RBP, RBX, R12, R13, R14, R15] {
        e.a.push(r);
    }
    e.a.mov(X_CPU, RDI);
    let wide = |disp: usize| Mem { base: X_CPU, index: None, disp: disp as i32, wide: true };
    e.a.ins(&[], true, &[0x8b], X_PAGE, Rm::M(wide(e.o.pc)), false);
    e.a.ins(&[], true, &[0x81], AND.1, Rm::R(X_PAGE), false);
    e.a.imm32(-4096);
    e.a.ins(&[], true, &[0x8b], X_UT, Rm::M(wide(e.o.ut)), false);
    e.a.ins(&[], true, &[0x8b], X_CYC, Rm::M(wide(e.o.cycle)), false);
    assert_eq!(e.a.out.len(), LINKED);
    e.load_cached();
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
    let bytes = e.finish();
    let f = cache.write(&bytes).ok_or(Fail::Full)?;
    Ok((f as usize, lo as u16, hi as u16))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Encodings, each with its Intel-syntax text.
    fn cases() -> Vec<(String, Vec<u8>)> {
        let names = ["rax", "rcx", "rdx", "rbx", "rsp", "rbp", "rsi", "rdi", "r8", "r9", "r10", "r11", "r12", "r13", "r14", "r15"];
        let n32 = ["eax", "ecx", "edx", "ebx", "esp", "ebp", "esi", "edi", "r8d", "r9d", "r10d", "r11d", "r12d", "r13d", "r14d", "r15d"];
        let n16 = ["ax", "cx", "dx", "bx", "sp", "bp", "si", "di", "r8w", "r9w", "r10w", "r11w", "r12w", "r13w", "r14w", "r15w"];
        let n8 = ["al", "cl", "dl", "bl", "spl", "bpl", "sil", "dil", "r8b", "r9b", "r10b", "r11b", "r12b", "r13b", "r14b", "r15b"];
        let regs: Vec<u8> = (0..16).filter(|&r| r != RSP).collect();
        let mut out = Vec::new();
        let mut one = |text: String, f: &dyn Fn(&mut Asm)| {
            let mut a = Asm::default();
            f(&mut a);
            out.push((text, a.out));
        };
        let mems = |r: u8| {
            let mut v = vec![];
            for &base in &regs {
                for disp in [0i32, 8, -8, 0x7f, 0x80, 0x1234, -0x1000] {
                    v.push(Mem { base, index: None, disp, wide: false });
                }
                v.push(Mem { base, index: Some((r, 1)), disp: 0, wide: false });
                v.push(Mem { base, index: Some((r, 8)), disp: 4096 + 8, wide: false });
            }
            v
        };
        let mtext = |mm: &Mem| {
            let mut s = names[mm.base as usize].to_string();
            if let Some((i, sc)) = mm.index {
                s += &format!(" + {}*{}", names[i as usize], sc);
            }
            format!("[{s} + {}]", mm.disp)
        };
        let ops = [("add", ADD), ("or", OR), ("and", AND), ("sub", SUB), ("xor", XOR), ("cmp", CMP)];
        for &d in &regs {
            for &s in &regs {
                let (dn, sn, d32, s32) = (names[d as usize], names[s as usize], n32[d as usize], n32[s as usize]);
                for (name, op) in ops {
                    one(format!("{name} {dn}, {sn}"), &|a| a.alu(op, true, d, Rm::R(s)));
                    one(format!("{name} {d32}, {s32}"), &|a| a.alu(op, false, d, Rm::R(s)));
                }
                if d != s {
                    one(format!("mov {dn}, {sn}"), &|a| a.mov(d, s));
                }
                one(format!("mov {d32}, {s32}"), &|a| a.mov32(d, Rm::R(s)));
                one(format!("imul {dn}, {sn}"), &|a| a.imul(true, d, Rm::R(s)));
                one(format!("imul {d32}, {s32}"), &|a| a.imul(false, d, Rm::R(s)));
                one(format!("movsxd {dn}, {s32}"), &|a| a.movsxd(d, Rm::R(s)));
                one(format!("movsx {dn}, {}", n8[s as usize]), &|a| a.movsx(1, d, Rm::R(s)));
                one(format!("movsx {dn}, {}", n16[s as usize]), &|a| a.movsx(2, d, Rm::R(s)));
                one(format!("movzx {d32}, {}", n8[s as usize]), &|a| a.movzx(1, d, Rm::R(s)));
                one(format!("movzx {d32}, {}", n16[s as usize]), &|a| a.movzx(2, d, Rm::R(s)));
                one(format!("cmovl {dn}, {sn}"), &|a| a.cmov(CC_L, d, Rm::R(s)));
                one(format!("cmova {dn}, {sn}"), &|a| a.cmov(CC_A, d, Rm::R(s)));
                one(format!("test {dn}, {sn}"), &|a| a.test(true, d, s));
                one(format!("test {d32}, {s32}"), &|a| a.test(false, d, s));
                one(format!("lzcnt {dn}, {sn}"), &|a| a.bitcount(0xbd, true, d, Rm::R(s)));
                one(format!("tzcnt {d32}, {s32}"), &|a| a.bitcount(0xbc, false, d, Rm::R(s)));
                one(format!("popcnt {dn}, {sn}"), &|a| a.bitcount(0xb8, true, d, Rm::R(s)));
                if s != RSP {
                    one(format!("lea {dn}, [{dn} + {sn}*4]"), &|a| a.lea(d, mi(d, s, 4, 0)));
                }
            }
            let (dn, d32) = (names[d as usize], n32[d as usize]);
            for (name, op) in ops {
                for v in [0i32, 1, -1, 127, -128, 128, 0xff0, -4096, -4089, 0x7fff_ffff] {
                    one(format!("{name} {dn}, {v}"), &|a| a.alu_i(op, true, Rm::R(d), v));
                    // (as llvm-mc prints them: imm8 signed, imm32 unsigned)
                    let t = if i8::try_from(v).is_ok() { v as i64 } else { v as u32 as i64 };
                    one(format!("{name} {d32}, {t}"), &|a| a.alu_i(op, false, Rm::R(d), v));
                }
            }
            for v in [0u64, 1, 0xffff_ffff, 0x1_0000_0000, u64::MAX, (-4096i64) as u64, 0x8000_0000, 0xffff_ffff_0000_0000, 0x1234_5678_9abc_def0] {
                let text = if v <= u32::MAX as u64 {
                    format!("mov {d32}, {v}")
                } else if v as i64 == v as i32 as i64 {
                    format!("mov {dn}, {}", v as i64)
                } else {
                    format!("movabs {dn}, {}", v as i64)
                };
                one(text, &|a| a.movi(d, v));
            }
            for (name, op) in [("rol", ROL), ("ror", ROR), ("shl", SHL), ("shr", SHR), ("sar", SAR)] {
                for k in [2u32, 3, 31, 63] {
                    one(format!("{name} {dn}, {k}"), &|a| a.shift_i(op, true, d, k));
                }
                one(format!("{name} {d32}, 5"), &|a| a.shift_i(op, false, d, 5));
                one(format!("{name} {dn}, cl"), &|a| a.shift_cl(op, true, d));
                one(format!("{name} {d32}, cl"), &|a| a.shift_cl(op, false, d));
            }
            for (name, op) in [("not", 2u8), ("neg", 3), ("mul", 4), ("imul", 5), ("div", 6), ("idiv", 7)] {
                one(format!("{name} {dn}"), &|a| a.f7(op, true, Rm::R(d)));
                one(format!("{name} {d32}"), &|a| a.f7(op, false, Rm::R(d)));
            }
            one(format!("setl {}", n8[d as usize]), &|a| a.setcc(CC_L, d));
            one(format!("setb {}", n8[d as usize]), &|a| a.setcc(CC_B, d));
            one(format!("test {d32}, 24576"), &|a| a.test_i(d, 3 << 13));
            one(format!("bswap {dn}"), &|a| a.bswap(d));
            one(format!("bts {dn}, 63"), &|a| a.bts(d, 63));
            one(format!("push {dn}"), &|a| a.push(d));
            one(format!("pop {dn}"), &|a| a.pop(d));
            one(format!("call {dn}"), &|a| a.call_r(d));
            for mm in mems(d) {
                let t = mtext(&mm);
                one(format!("mov {dn}, qword ptr {t}"), &|a| a.load(d, mm));
                one(format!("mov qword ptr {t}, {dn}"), &|a| a.store(mm, d));
                one(format!("mov byte ptr {t}, {}", n8[d as usize]), &|a| a.store_n(1, mm, d));
                one(format!("mov word ptr {t}, {}", n16[d as usize]), &|a| a.store_n(2, mm, d));
                one(format!("mov dword ptr {t}, {d32}"), &|a| a.store_n(4, mm, d));
                one(format!("lea {dn}, {t}"), &|a| a.lea(d, mm));
                one(format!("add {dn}, qword ptr {t}"), &|a| a.alu(ADD, true, d, Rm::M(mm)));
                one(format!("cmp {dn}, qword ptr {t}"), &|a| a.alu(CMP, true, d, Rm::M(mm)));
                one(format!("add qword ptr {t}, {dn}"), &|a| a.alu_to_mem(ADD, mm, d));
                one(format!("cmp qword ptr {t}, -1"), &|a| a.alu_i(CMP, true, Rm::M(mm), -1));
                one(format!("movsx {dn}, byte ptr {t}"), &|a| a.movsx(1, d, Rm::M(mm)));
                one(format!("movsx {dn}, word ptr {t}"), &|a| a.movsx(2, d, Rm::M(mm)));
                one(format!("movsxd {dn}, dword ptr {t}"), &|a| a.movsxd(d, Rm::M(mm)));
                one(format!("movzx {d32}, byte ptr {t}"), &|a| a.movzx(1, d, Rm::M(mm)));
                one(format!("movzx {d32}, word ptr {t}"), &|a| a.movzx(2, d, Rm::M(mm)));
                one(format!("mov {d32}, dword ptr {t}"), &|a| a.mov32(d, Rm::M(mm)));
                one(format!("jmp qword ptr {t}"), &|a| a.jmp_m(mm));
            }
        }
        out
    }

    fn llvm_mc() -> Option<String> {
        let mut c: Vec<String> = std::env::var("LLVM_MC").into_iter().collect();
        c.extend(["llvm-mc", "llvm-mc-18", "llvm-mc-19", "llvm-mc-17"].map(String::from));
        c.into_iter().find(|p| std::process::Command::new(p).arg("--version").output().is_ok_and(|o| o.status.success()))
    }

    fn mc(tool: &str, args: &[&str], input: &str) -> Vec<String> {
        use std::io::Write;
        let mut p = std::process::Command::new(tool)
            .args(args)
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn()
            .unwrap();
        p.stdin.take().unwrap().write_all(input.as_bytes()).unwrap();
        let o = p.wait_with_output().unwrap();
        assert!(o.status.success(), "{tool}: {}", String::from_utf8_lossy(&o.stderr));
        String::from_utf8_lossy(&o.stdout)
            .lines()
            .map(|l| l.trim())
            .filter(|l| !l.is_empty() && !l.starts_with('.'))
            .map(|l| l.split_whitespace().collect::<Vec<_>>().join(" "))
            .collect()
    }

    /// The encoder against an assembler: each encoding, disassembled by llvm-mc, is the
    /// instruction its text assembles to (printed the same way). Skipped without llvm-mc
    /// (LLVM_MC names it).
    #[test]
    fn encodings_match_llvm_mc() {
        let Some(tool) = llvm_mc() else {
            eprintln!("llvm-mc not found: encodings not checked");
            return;
        };
        let cases = cases();
        let text: String = cases.iter().map(|(t, _)| format!("{t}\n")).collect();
        let bytes: String = cases.iter().map(|(_, b)| b.iter().map(|x| format!("{x:#04x} ")).collect::<String>() + "\n").collect();
        let canonical = mc(&tool, &["-triple=x86_64", "-x86-asm-syntax=intel", "-output-asm-variant=1"], &format!(".intel_syntax noprefix\n{text}"));
        let ours = mc(&tool, &["-triple=x86_64", "--disassemble", "-output-asm-variant=1"], &bytes);
        assert_eq!(canonical.len(), cases.len(), "assembler output");
        assert_eq!(ours.len(), cases.len(), "disassembly: an encoding is not one instruction");
        let mut bad = Vec::new();
        for (k, (t, b)) in cases.iter().enumerate() {
            if canonical[k] != ours[k] {
                bad.push(format!("{t}: {b:02x?} disassembles to `{}`, expected `{}`", ours[k], canonical[k]));
            }
        }
        assert!(bad.is_empty(), "{} of {} encodings differ:\n{}", bad.len(), cases.len(), bad[..bad.len().min(40)].join("\n"));
    }
}
