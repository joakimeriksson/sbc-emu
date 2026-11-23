//! A64 → x86-64 block JIT (x86-64 Linux hosts).
//!
//! A translator, like the WebAssembly backend: every native guest instruction becomes a short
//! x86-64 sequence; everything else calls the interpreter for that one instruction (`h_step`),
//! exactly as the AArch64 backend does. The driver is shared with `jit.rs`: the block map and
//! its front cache, hotness counters, direct links (here patched `jmp rel32`), invalidation by
//! IC IALLU/IVAU, verify mode and the profile of interpreter fallbacks.
//!
//! Host registers while generated code runs: rbx = `Cpu`, r13 = host address of guest RAM,
//! r14 = the guest's instret (in `Cpu` only around calls and on the way out), r15 = guest NZCV,
//! rax/rcx/rdx = scratch, and rbp, rsi, rdi, r8-r12 cache guest X registers and SP inside a
//! block (loaded on first use, written back before exits and interpreter calls).
//!
//! Flags: guest NZCV is kept in x86 form, with the carry inverted: N = SF, Z = ZF, V = OF and
//! C = NOT CF, the x86 borrow convention (SUB and CMP give it directly; ADD and CMN are followed
//! by CMC, logical S forms by STC). With that, every A64 condition is one x86 condition code
//! (HI = A, GE = GE, ...), ADC is CMC + ADC and SBC is plain SBB. Between instructions the flags
//! stay in EFLAGS as long as possible; before code that would clobber them they are saved to
//! r15 as `lahf; seto al; movzx r15d, ax` (SF bit 15, ZF bit 14, CF bit 8, OF bit 0) and loaded
//! back with `mov eax, r15d; add al, 0x7f; sahf` when a condition needs them. Across block
//! exits, links, the dispatcher and interpreter calls they are in r15; `fl_to_nzcv` and
//! `nzcv_to_fl` convert at the boundary to Rust. FP/SIMD data processing runs in the
//! interpreter (FPSR and the V registers stay in `Cpu`); FP/SIMD loads and stores are native.
//!
//! Loads and stores probe the core's micro-TLB inline (way 0 inline, way 1 out of line); the
//! slow path first refills the micro-TLB (`h_tlb`) and retries, then calls the interpreter.
//! Exclusives, DC ZVA and the thread-pointer MRS are native as in the AArch64 backend; with
//! cores on parallel host threads (`mt`) full DMB/DSB become MFENCE, STLR an XCHG and
//! store-exclusive a LOCK CMPXCHG against the value the load-exclusive read.
use super::{bit, bits, is_block_end, key, native_operands, sb_continue, BlockFn, Jit, Slot, MAX_INSNS, R as How, SP_G};
use crate::cpu::{Cpu, UT, UTLB_HASH};
use asm::*;

// ---------------------------------------------------------------------- encoder
/// A small x86-64 encoder: only the forms the translator emits (checked against llvm-mc in
/// the tests below).
pub(super) mod asm {
    #[derive(Clone, Copy, PartialEq, Eq, Debug)]
    pub struct Reg(pub u8);
    pub const RAX: Reg = Reg(0);
    pub const RCX: Reg = Reg(1);
    pub const RDX: Reg = Reg(2);
    pub const RBX: Reg = Reg(3);
    pub const RSP: Reg = Reg(4);
    pub const RBP: Reg = Reg(5);
    pub const RSI: Reg = Reg(6);
    pub const RDI: Reg = Reg(7);
    pub const R8: Reg = Reg(8);
    pub const R9: Reg = Reg(9);
    pub const R10: Reg = Reg(10);
    pub const R11: Reg = Reg(11);
    pub const R12: Reg = Reg(12);
    pub const R13: Reg = Reg(13);
    pub const R14: Reg = Reg(14);
    pub const R15: Reg = Reg(15);

    /// A memory operand: [base + index << scale + disp]
    #[derive(Clone, Copy, Debug)]
    pub struct Mem { pub base: Reg, pub index: Option<(Reg, u8)>, pub disp: i32 }
    pub fn m(base: Reg, disp: i32) -> Mem { Mem { base, index: None, disp } }
    pub fn mx(base: Reg, index: Reg, scale: u8, disp: i32) -> Mem { Mem { base, index: Some((index, scale)), disp } }

    #[derive(Clone, Copy, Debug)]
    pub enum Alu { Add = 0, Or = 1, Adc = 2, Sbb = 3, And = 4, Sub = 5, Xor = 6, Cmp = 7 }
    #[derive(Clone, Copy, Debug)]
    #[allow(dead_code)] // ROL: the encoder tests
    pub enum Sh { Rol = 0, Ror = 1, Shl = 4, Shr = 5, Sar = 7 }

    /// x86 condition codes
    pub const O: u8 = 0;
    pub const NO: u8 = 1;
    pub const B: u8 = 2;
    pub const AE: u8 = 3;
    pub const E: u8 = 4;
    pub const NE: u8 = 5;
    pub const BE: u8 = 6;
    pub const A: u8 = 7;
    pub const S: u8 = 8;
    pub const NS: u8 = 9;
    pub const L: u8 = 0xc;
    pub const GE: u8 = 0xd;
    pub const LE: u8 = 0xe;
    pub const G: u8 = 0xf;

    #[derive(Default)]
    pub struct Asm { pub b: Vec<u8> }

    impl Asm {
        pub fn pos(&self) -> usize { self.b.len() }
        fn byte(&mut self, v: u8) { self.b.push(v) }
        fn i32(&mut self, v: i32) { self.b.extend_from_slice(&v.to_le_bytes()) }
        fn rex(&mut self, w: bool, r: u8, x: u8, b: u8, force: bool) {
            let v = 0x40 | ((w as u8) << 3) | (((r >> 3) & 1) << 2) | (((x >> 3) & 1) << 1) | ((b >> 3) & 1);
            if v != 0x40 || force { self.byte(v); }
        }
        fn modrm_mem(&mut self, reg: u8, mm: Mem) {
            let b = mm.base.0 & 7;
            // rbp/r13 as a base always need a displacement (mod 00 means RIP-relative there)
            let md: u8 = if mm.disp == 0 && b != 5 { 0 } else if (-128..=127).contains(&mm.disp) { 1 } else { 2 };
            match mm.index {
                // rsp/r12 as a base need a SIB byte
                None if b != 4 => self.byte((md << 6) | ((reg & 7) << 3) | b),
                None => { self.byte((md << 6) | ((reg & 7) << 3) | 4); self.byte(0x24); }
                Some((ix, sc)) => {
                    debug_assert!(ix.0 != 4, "rsp cannot be an index");
                    self.byte((md << 6) | ((reg & 7) << 3) | 4);
                    self.byte((sc << 6) | ((ix.0 & 7) << 3) | b);
                }
            }
            match md { 1 => self.byte(mm.disp as i8 as u8), 2 => self.i32(mm.disp), _ => {} }
        }
        /// [prefixes] [REX] opcode, ModRM with a register `rm`. `breg`/`brm`: that operand is a
        /// byte register (spl, bpl, sil, dil need a REX prefix).
        pub fn rr(&mut self, pfx: &[u8], w: bool, opc: &[u8], reg: u8, rm: u8, breg: bool, brm: bool) {
            self.b.extend_from_slice(pfx);
            let force = (breg && (4..8).contains(&reg)) || (brm && (4..8).contains(&rm));
            self.rex(w, reg, 0, rm, force);
            self.b.extend_from_slice(opc);
            self.byte(0xc0 | ((reg & 7) << 3) | (rm & 7));
        }
        /// [prefixes] [REX] opcode, ModRM with a memory operand.
        pub fn rm(&mut self, pfx: &[u8], w: bool, opc: &[u8], reg: u8, mm: Mem, breg: bool) {
            self.b.extend_from_slice(pfx);
            let force = breg && (4..8).contains(&reg);
            self.rex(w, reg, mm.index.map_or(0, |(i, _)| i.0), mm.base.0, force);
            self.b.extend_from_slice(opc);
            self.modrm_mem(reg, mm);
        }

        pub fn mov(&mut self, w: bool, d: Reg, s: Reg) { self.rr(&[], w, &[0x89], s.0, d.0, false, false) }
        pub fn load(&mut self, w: bool, d: Reg, mm: Mem) { self.rm(&[], w, &[0x8b], d.0, mm, false) }
        /// store the low `size` bytes of `s`
        pub fn store(&mut self, size: u32, mm: Mem, s: Reg) {
            match size {
                1 => self.rm(&[], false, &[0x88], s.0, mm, true),
                2 => self.rm(&[0x66], false, &[0x89], s.0, mm, false),
                4 => self.rm(&[], false, &[0x89], s.0, mm, false),
                _ => self.rm(&[], true, &[0x89], s.0, mm, false),
            }
        }
        /// store an immediate of `size` bytes (8: sign-extended from 32 bits)
        pub fn store_imm(&mut self, size: u32, mm: Mem, v: i32) {
            match size {
                1 => { self.rm(&[], false, &[0xc6], 0, mm, false); self.byte(v as u8) }
                2 => { self.rm(&[0x66], false, &[0xc7], 0, mm, false); self.b.extend_from_slice(&(v as u16).to_le_bytes()) }
                4 => { self.rm(&[], false, &[0xc7], 0, mm, false); self.i32(v) }
                _ => { self.rm(&[], true, &[0xc7], 0, mm, false); self.i32(v) }
            }
        }
        /// zero-extending load of 1, 2, 4 or 8 bytes
        pub fn load_zx(&mut self, size: u32, d: Reg, mm: Mem) {
            match size {
                1 => self.rm(&[], false, &[0x0f, 0xb6], d.0, mm, false),
                2 => self.rm(&[], false, &[0x0f, 0xb7], d.0, mm, false),
                4 => self.load(false, d, mm),
                _ => self.load(true, d, mm),
            }
        }
        /// sign-extending load of 1, 2 (to 32 or 64 bits: `w`) or 4 bytes (to 64)
        pub fn load_sx(&mut self, size: u32, w: bool, d: Reg, mm: Mem) {
            match size {
                1 => self.rm(&[], w, &[0x0f, 0xbe], d.0, mm, false),
                2 => self.rm(&[], w, &[0x0f, 0xbf], d.0, mm, false),
                _ => self.rm(&[], true, &[0x63], d.0, mm, false),
            }
        }
        pub fn movzx8(&mut self, d: Reg, s: Reg) { self.rr(&[], false, &[0x0f, 0xb6], d.0, s.0, false, true) }
        pub fn movzx16(&mut self, d: Reg, s: Reg) { self.rr(&[], false, &[0x0f, 0xb7], d.0, s.0, false, false) }
        pub fn movsx8(&mut self, w: bool, d: Reg, s: Reg) { self.rr(&[], w, &[0x0f, 0xbe], d.0, s.0, false, true) }
        pub fn movsx16(&mut self, w: bool, d: Reg, s: Reg) { self.rr(&[], w, &[0x0f, 0xbf], d.0, s.0, false, false) }
        pub fn movsxd(&mut self, d: Reg, s: Reg) { self.rr(&[], true, &[0x63], d.0, s.0, false, false) }
        /// the shortest move of a 64-bit constant (flags untouched)
        pub fn mov_imm(&mut self, d: Reg, v: u64) {
            if v <= u32::MAX as u64 {
                self.rex(false, 0, 0, d.0, false);
                self.byte(0xb8 | (d.0 & 7));
                self.i32(v as u32 as i32);
            } else if (v as i64) == (v as i32 as i64) {
                self.rr(&[], true, &[0xc7], 0, d.0, false, false);
                self.i32(v as i32);
            } else {
                self.rex(true, 0, 0, d.0, false);
                self.byte(0xb8 | (d.0 & 7));
                self.b.extend_from_slice(&v.to_le_bytes());
            }
        }
        /// mov r16, imm16 (bits 63:16 kept)
        pub fn mov_imm16(&mut self, d: Reg, v: u16) {
            self.byte(0x66);
            self.rex(false, 0, 0, d.0, false);
            self.byte(0xb8 | (d.0 & 7));
            self.b.extend_from_slice(&v.to_le_bytes());
        }
        pub fn alu(&mut self, op: Alu, w: bool, d: Reg, s: Reg) { self.rr(&[], w, &[op as u8 * 8 + 1], s.0, d.0, false, false) }
        pub fn alu_imm(&mut self, op: Alu, w: bool, d: Reg, v: i32) {
            if (-128..=127).contains(&v) { self.rr(&[], w, &[0x83], op as u8, d.0, false, false); self.byte(v as u8) }
            else { self.rr(&[], w, &[0x81], op as u8, d.0, false, false); self.i32(v) }
        }
        /// `op d, [mm]`
        pub fn alu_load(&mut self, op: Alu, w: bool, d: Reg, mm: Mem) { self.rm(&[], w, &[op as u8 * 8 + 3], d.0, mm, false) }
        /// `op byte/qword [mm], imm8`
        pub fn alu_mem_imm8(&mut self, op: Alu, size: u32, mm: Mem, v: i8) {
            if size == 1 { self.rm(&[], false, &[0x80], op as u8, mm, false) } else { self.rm(&[], size == 8, &[0x83], op as u8, mm, false) }
            self.byte(v as u8);
        }
        pub fn test(&mut self, w: bool, a: Reg, b: Reg) { self.rr(&[], w, &[0x85], b.0, a.0, false, false) }
        pub fn test_imm(&mut self, w: bool, a: Reg, v: i32) { self.rr(&[], w, &[0xf7], 0, a.0, false, false); self.i32(v) }
        pub fn lea(&mut self, w: bool, d: Reg, mm: Mem) { self.rm(&[], w, &[0x8d], d.0, mm, false) }
        pub fn shift(&mut self, k: Sh, w: bool, d: Reg, n: u8) { self.rr(&[], w, &[0xc1], k as u8, d.0, false, false); self.byte(n) }
        pub fn shift_cl(&mut self, k: Sh, w: bool, d: Reg) { self.rr(&[], w, &[0xd3], k as u8, d.0, false, false) }
        /// shrd d, s, n: d = d >> n | s << (width - n)
        pub fn shrd(&mut self, w: bool, d: Reg, s: Reg, n: u8) { self.rr(&[], w, &[0x0f, 0xac], s.0, d.0, false, false); self.byte(n) }
        pub fn imul(&mut self, w: bool, d: Reg, s: Reg) { self.rr(&[], w, &[0x0f, 0xaf], d.0, s.0, false, false) }
        /// group 3: 2 not, 3 neg, 4 mul, 5 imul, 6 div, 7 idiv (one register operand)
        pub fn grp3(&mut self, k: u8, w: bool, r: Reg) { self.rr(&[], w, &[0xf7], k, r.0, false, false) }
        /// cqo (64-bit) / cdq (32-bit)
        pub fn cqo(&mut self, w: bool) { if w { self.byte(0x48) } self.byte(0x99) }
        pub fn bswap(&mut self, w: bool, r: Reg) { self.rex(w, 0, 0, r.0, false); self.byte(0x0f); self.byte(0xc8 | (r.0 & 7)) }
        pub fn bsr(&mut self, w: bool, d: Reg, s: Reg) { self.rr(&[], w, &[0x0f, 0xbd], d.0, s.0, false, false) }
        pub fn bt(&mut self, w: bool, r: Reg, n: u8) { self.rr(&[], w, &[0x0f, 0xba], 4, r.0, false, false); self.byte(n) }
        pub fn bts(&mut self, w: bool, r: Reg, n: u8) { self.rr(&[], w, &[0x0f, 0xba], 5, r.0, false, false); self.byte(n) }
        pub fn cmov(&mut self, cc: u8, w: bool, d: Reg, s: Reg) { self.rr(&[], w, &[0x0f, 0x40 | cc], d.0, s.0, false, false) }
        pub fn setcc(&mut self, cc: u8, r: Reg) { self.rr(&[], false, &[0x0f, 0x90 | cc], 0, r.0, false, true) }
        /// jcc rel32 to be patched: returns the position of the rel32
        pub fn jcc(&mut self, cc: u8) -> usize { self.byte(0x0f); self.byte(0x80 | cc); self.i32(0); self.pos() - 4 }
        pub fn jmp(&mut self) -> usize { self.byte(0xe9); self.i32(0); self.pos() - 4 }
        pub fn call(&mut self) -> usize { self.byte(0xe8); self.i32(0); self.pos() - 4 }
        pub fn jcc_to(&mut self, cc: u8, to: usize) { let p = self.jcc(cc); self.patch(p, to) }
        pub fn jmp_to(&mut self, to: usize) { let p = self.jmp(); self.patch(p, to) }
        pub fn jmp_mem(&mut self, mm: Mem) { self.rm(&[], false, &[0xff], 4, mm, false) }
        pub fn jmp_reg(&mut self, r: Reg) { self.rr(&[], false, &[0xff], 4, r.0, false, false) }
        pub fn call_reg(&mut self, r: Reg) { self.rr(&[], false, &[0xff], 2, r.0, false, false) }
        pub fn ret(&mut self) { self.byte(0xc3) }
        pub fn push(&mut self, r: Reg) { self.rex(false, 0, 0, r.0, false); self.byte(0x50 | (r.0 & 7)) }
        pub fn pop(&mut self, r: Reg) { self.rex(false, 0, 0, r.0, false); self.byte(0x58 | (r.0 & 7)) }
        pub fn lahf(&mut self) { self.byte(0x9f) }
        pub fn sahf(&mut self) { self.byte(0x9e) }
        pub fn cmc(&mut self) { self.byte(0xf5) }
        pub fn stc(&mut self) { self.byte(0xf9) }
        pub fn add_al(&mut self, v: u8) { self.byte(0x04); self.byte(v) }
        pub fn mfence(&mut self) { self.b.extend_from_slice(&[0x0f, 0xae, 0xf0]) }
        pub fn xchg(&mut self, size: u32, mm: Mem, r: Reg) {
            match size {
                1 => self.rm(&[], false, &[0x86], r.0, mm, true),
                2 => self.rm(&[0x66], false, &[0x87], r.0, mm, false),
                4 => self.rm(&[], false, &[0x87], r.0, mm, false),
                _ => self.rm(&[], true, &[0x87], r.0, mm, false),
            }
        }
        /// lock cmpxchg [mm], r (compares with al/ax/eax/rax)
        pub fn lock_cmpxchg(&mut self, size: u32, mm: Mem, r: Reg) {
            match size {
                1 => self.rm(&[0xf0], false, &[0x0f, 0xb0], r.0, mm, true),
                2 => self.rm(&[0xf0, 0x66], false, &[0x0f, 0xb1], r.0, mm, false),
                4 => self.rm(&[0xf0], false, &[0x0f, 0xb1], r.0, mm, false),
                _ => self.rm(&[0xf0], true, &[0x0f, 0xb1], r.0, mm, false),
            }
        }
        pub fn movdqu_load(&mut self, x: u8, mm: Mem) { self.rm(&[0xf3], false, &[0x0f, 0x6f], x, mm, false) }
        pub fn movdqu_store(&mut self, mm: Mem, x: u8) { self.rm(&[0xf3], false, &[0x0f, 0x7f], x, mm, false) }
        /// movq xmm, m64 (upper half zeroed)
        pub fn movq_load(&mut self, x: u8, mm: Mem) { self.rm(&[0xf3], false, &[0x0f, 0x7e], x, mm, false) }
        /// movd xmm, m32 (upper bits zeroed)
        pub fn movd_load(&mut self, x: u8, mm: Mem) { self.rm(&[0x66], false, &[0x0f, 0x6e], x, mm, false) }
        pub fn pxor(&mut self, x: u8, y: u8) { self.rr(&[0x66], false, &[0x0f, 0xef], x, y, false, false) }
        /// SSE2 `66 0F op /r` on two xmm registers (x = x op y): PCMPEQB 74, PCMPEQW 75,
        /// PCMPEQD 76, PMAXUB DE, PMINUB DA, PAND DB, PANDN DF, POR EB, PXOR EF, PACKUSWB 67,
        /// MOVDQA 6F
        pub fn sse(&mut self, op: u8, x: u8, y: u8) { self.rr(&[0x66], false, &[0x0f, op], x, y, false, false) }
        /// psrlw xmm, imm8
        pub fn psrlw(&mut self, x: u8, n: u8) { self.rr(&[0x66], false, &[0x0f, 0x71], 2, x, false, false); self.byte(n) }
        /// movq m64, xmm
        pub fn movq_store(&mut self, mm: Mem, x: u8) { self.rm(&[0x66], false, &[0x0f, 0xd6], x, mm, false) }
        /// point the rel32 at `at` to position `to` of this buffer
        pub fn patch(&mut self, at: usize, to: usize) {
            let v = (to as i64 - (at as i64 + 4)) as i32;
            self.b[at..at + 4].copy_from_slice(&v.to_le_bytes());
        }
        /// point the rel32 at `at` here
        pub fn bind(&mut self, at: usize) { let p = self.pos(); self.patch(at, p) }
    }
}

// ---------------------------------------------------------------------- flags
/// Guest NZCV (bits 31:28) to the generated code's form (see the module comment).
pub(super) fn nzcv_to_fl(nzcv: u32) -> u64 {
    let f = |b: u32| ((nzcv >> b) & 1) as u64;
    (f(31) << 15) | (f(30) << 14) | ((f(29) ^ 1) << 8) | f(28)
}
/// The generated code's flags back to NZCV (bits 31:28).
pub(super) fn fl_to_nzcv(fl: u64) -> u32 {
    let f = |b: u32| ((fl >> b) & 1) as u32;
    (f(15) << 31) | (f(14) << 30) | ((f(8) ^ 1) << 29) | (f(0) << 28)
}

/// `enter`'s result: status (0 = continue at cpu.pc, 1 = trap in cpu.jit_trap) and the flags.
#[repr(C)]
pub(super) struct Ret { pub status: u64, pub fl: u64 }
pub(super) type EnterFn = unsafe extern "C" fn(*mut Cpu, usize, u64) -> Ret;

/// The host can run generated code: LAHF/SAHF in 64-bit mode (every x86-64 CPU since 2006).
pub(super) fn host_ok() -> bool {
    let r = std::arch::x86_64::__cpuid(0x8000_0001);
    r.ecx & 1 != 0
}

/// Where the guest flags are while a block is compiled.
#[derive(Clone, Copy, PartialEq, Debug)]
enum Fl {
    /// in r15 only (EFLAGS hold something else)
    Reg,
    /// in EFLAGS only (r15 is stale)
    Eflags,
    /// in both
    Both,
}

/// The A64 condition as an x86 condition code (with C = NOT CF); None for AL/NV.
fn cc_of(cond: u32) -> Option<u8> {
    [E, NE, AE, B, S, NS, O, NO, A, BE, GE, L, G, LE].get(cond as usize).copied()
}

fn save_flags(a: &mut Asm) { a.lahf(); a.setcc(O, RAX); a.movzx16(R15, RAX) }
fn load_flags(a: &mut Asm) { a.mov(false, RAX, R15); a.add_al(0x7f); a.sahf() }

// ---------------------------------------------------------------------- host helpers
/// The interpreter for one instruction (from the interpreter stub): flags in and out in the
/// generated code's form. Returns the status of `h_step` | flags << 32.
unsafe extern "C" fn h_step_x64(cpu: *mut Cpu, expect_next: u64, fl: u64) -> u64 {
    (*cpu).nzcv = fl_to_nzcv(fl);
    let st = super::h_step(cpu, expect_next);
    st as u64 | nzcv_to_fl((*cpu).nzcv) << 32
}

// ---------------------------------------------------------------------- emitter
const CPU: Reg = RBX;
const RAMB: Reg = R13;
const ICOUNT: Reg = R14;
const FLR: Reg = R15;
/// host registers caching guest X registers and SP inside a block
const CACHE: [Reg; 8] = [RBP, RSI, RDI, R8, R9, R10, R11, R12];

/// `Cpu` field offsets the generated code uses.
struct Off { x: i32, sp: i32, pc: i32, daif: i32, instret: i32, utlb: i32, ram: i32, excl: i32, excl_addr: i32, excl_val: i32, v: i32, tpidr: i32,
             chain_until: i32, chain_stop: i32, utlb_unpriv: i32 }

fn off() -> Off {
    use std::mem::offset_of;
    let o = |v: usize| i32::try_from(v).expect("Cpu offset");
    Off {
        x: o(offset_of!(Cpu, x)), sp: o(offset_of!(Cpu, sp)), pc: o(offset_of!(Cpu, pc)), daif: o(offset_of!(Cpu, daif)), instret: o(offset_of!(Cpu, instret)),
        utlb: o(offset_of!(Cpu, utlb)), ram: o(offset_of!(Cpu, ram)), excl: o(offset_of!(Cpu, excl_armed)),
        excl_addr: o(offset_of!(Cpu, excl_addr)), excl_val: o(offset_of!(Cpu, excl_val)), v: o(offset_of!(Cpu, v)),
        tpidr: o(offset_of!(Cpu, sys) + offset_of!(crate::sysreg::SysRegs, tpidr)), chain_until: o(offset_of!(Cpu, chain_until)),
        chain_stop: o(offset_of!(Cpu, chain_stop)), utlb_unpriv: o(offset_of!(Cpu, utlb_unpriv)),
    }
}

/// Absolute addresses of the shared stubs.
#[derive(Clone, Copy)]
struct Stubs { disp: u64, exit: u64, interp: u64, refill: u64 }

/// The taken side of a conditional branch inside a superblock (see `jit.rs`): the rel32 of the
/// jcc, whether the flags are still to be saved from EFLAGS, the registers to write back.
struct SideExit { at: usize, save: bool, spill: Vec<(Reg, u32)>, target: u64 }

/// An out-of-line load/store slow path (see `jit.rs`'s `SlowPath`): entered from `sites`; the
/// way-1 check jumps back to `back` on a hit; a refill retries from `retry`; the interpreter
/// rejoins at `done`.
struct Slow { sites: Vec<usize>, way1: Option<(usize, usize, i32)>, granule: Option<(usize, usize)>, spill: Vec<(Reg, u32)>, reload: Vec<(Reg, u32)>, i: u32, pc: u64, pending: u64,
              done: usize, retry: usize, arg: u32 }

struct E<'a> {
    a: Asm,
    o: &'a Off,
    /// `Cpu` offset of the SP in use
    sp_slot: i32,
    /// natives not yet added to r14
    pending: u64,
    /// chaining: the mode bits of the block key (bits 48..), passed to the dispatcher in rdx
    chain: Option<u64>,
    page: u64,
    el: u64,
    fetch: Box<[(u64, u64); UT]>,
    /// patchable direct jumps: (position of the rel32, target pc, target's physical page if on
    /// another page)
    links: Vec<(usize, u64, Option<u64>)>,
    /// rel32 fields pointing at absolute addresses (the stubs)
    relocs: Vec<(usize, u64)>,
    ra: [Slot; CACHE.len()],
    tick: u32,
    fl: Fl,
    slow: Vec<Slow>,
    sides: Vec<SideExit>,
    sb: bool,
    st: Stubs,
    mt: bool,
}

impl E<'_> {
    // ---- register cache (as `jit.rs`'s, for X registers and SP)
    fn g_off(&self, g: u32) -> i32 { if g == SP_G { self.sp_slot } else { self.o.x + 8 * g as i32 } }
    fn key_of(r: u32, how: How) -> Option<u32> { if r != 31 { Some(r) } else if how == How::Sp { Some(SP_G) } else { None } }
    fn alloc(&mut self, g: u32, load: bool) -> Reg {
        self.tick += 1;
        let tick = self.tick;
        if let Some(k) = self.ra.iter().position(|s| s.guest == Some(g)) {
            self.ra[k].used = tick;
            self.ra[k].pinned = true;
            return CACHE[k];
        }
        let k = self.ra.iter().position(|s| s.guest.is_none()).unwrap_or_else(|| {
            (0..self.ra.len()).filter(|&k| !self.ra[k].pinned).min_by_key(|&k| self.ra[k].used).expect("an unpinned cache register")
        });
        let old = self.ra[k];
        self.ra[k] = Slot { guest: Some(g), dirty: false, used: tick, pinned: true };
        if let (Some(og), true) = (old.guest, old.dirty) { let off = self.g_off(og); self.a.store(8, m(CPU, off), CACHE[k]); }
        if load { let off = self.g_off(g); self.a.load(true, CACHE[k], m(CPU, off)); }
        CACHE[k]
    }
    /// host register holding field `r` (None: the zero register)
    fn get(&mut self, r: u32, how: How) -> Option<Reg> { Self::key_of(r, how).map(|g| self.alloc(g, true)) }
    /// host register to write field `r` into (not loaded; call `dirty` after)
    fn def(&mut self, r: u32, how: How) -> Option<Reg> { Self::key_of(r, how).map(|g| self.alloc(g, false)) }
    fn dirty(&mut self, r: u32, how: How) {
        if let Some(g) = Self::key_of(r, how) { if let Some(s) = self.ra.iter_mut().find(|s| s.guest == Some(g)) { s.dirty = true; } }
    }
    fn unpin(&mut self) { for s in self.ra.iter_mut() { s.pinned = false; } }
    fn dirty_list(&self) -> Vec<(Reg, u32)> { self.ra.iter().enumerate().filter_map(|(k, s)| s.guest.filter(|_| s.dirty).map(|g| (CACHE[k], g))).collect() }
    fn cached_list(&self) -> Vec<(Reg, u32)> { self.ra.iter().enumerate().filter_map(|(k, s)| s.guest.map(|g| (CACHE[k], g))).collect() }
    fn spill(&mut self, l: &[(Reg, u32)]) { for &(h, g) in l { let off = self.g_off(g); self.a.store(8, m(CPU, off), h); } }
    fn reload(&mut self, l: &[(Reg, u32)]) { for &(h, g) in l { let off = self.g_off(g); self.a.load(true, h, m(CPU, off)); } }
    /// write the changed registers back (they stay cached, now clean)
    fn ra_flush(&mut self) {
        let l = self.dirty_list();
        self.spill(&l);
        for s in self.ra.iter_mut() { s.dirty = false; }
    }
    fn ra_forget(&mut self) { self.ra = [Slot::default(); CACHE.len()]; }
    /// a value for field `r`: its cache register, or `zero` set to 0
    fn val(&mut self, r: Option<Reg>, zero: Reg) -> Reg {
        match r { Some(h) => h, None => { self.a.mov_imm(zero, 0); zero } }
    }

    // ---- flags
    /// The code that follows changes EFLAGS: save the guest flags first if only EFLAGS hold them.
    fn clobber(&mut self) {
        if self.fl == Fl::Eflags { save_flags(&mut self.a); }
        self.fl = Fl::Reg;
    }
    /// r15 must hold the flags (EFLAGS kept). Uses rax.
    fn sync(&mut self) {
        if self.fl == Fl::Eflags { save_flags(&mut self.a); self.fl = Fl::Both; }
    }
    /// EFLAGS must hold the flags. Uses rax.
    fn need_eflags(&mut self) {
        if self.fl == Fl::Reg { load_flags(&mut self.a); self.fl = Fl::Both; }
    }

    // ---- control
    fn count_pending(&mut self) {
        if self.pending == 0 { return; }
        self.a.lea(true, ICOUNT, m(ICOUNT, self.pending as i32));
        self.pending = 0;
    }
    fn jmp_abs(&mut self, to: u64) { let p = self.a.jmp(); self.relocs.push((p, to)); }
    fn call_abs(&mut self, to: u64) { let p = self.a.call(); self.relocs.push((p, to)); }
    fn set_pc(&mut self, pc: u64) {
        if (pc as i64) == (pc as i32 as i64) { self.a.store_imm(8, m(CPU, self.o.pc), pc as i32); }
        else { self.a.mov_imm(RAX, pc); self.a.store(8, m(CPU, self.o.pc), RAX); }
    }
    /// Return `status` to Rust through the exit stub.
    fn leave(&mut self, status: u32) {
        self.a.store(8, m(CPU, self.o.instret), ICOUNT);
        self.a.mov_imm(RAX, status as u64);
        self.jmp_abs(self.st.exit);
    }
    /// Continue at cpu.pc: the dispatcher (without chaining, as in verify mode: back to Rust).
    fn to_dispatcher(&mut self) {
        self.sync();
        self.count_pending();
        match self.chain {
            Some(mode) => { self.a.mov_imm(RDX, mode); self.jmp_abs(self.st.disp); }
            None => self.leave(0),
        }
    }
    /// The physical page of `va` in the fetch micro-TLB at compile time.
    fn fetch_page(&self, va: u64) -> Option<u64> {
        let (s, tag) = (crate::cpu::uti(va), crate::cpu::utag(va, self.el));
        [self.fetch[s], self.fetch[s + 1]].into_iter().find(|e| e.0 == tag).map(|e| e.1)
    }
    /// Leave the block for constant `target` (guest registers written back): a patchable jump
    /// into its block while the budget lasts (into another page only while the fetch micro-TLB
    /// still maps it to the physical page it was linked with), else the dispatcher.
    fn exit_const(&mut self, target: u64) {
        self.sync();
        self.count_pending();
        if self.chain.is_some() {
            if target >> 12 == self.page {
                self.a.alu_load(Alu::Cmp, true, ICOUNT, m(CPU, self.o.chain_until));
                let out = self.a.jcc(AE);
                let site = self.a.jmp(); // rel32 0: falls through until linked
                self.links.push((site, target, None));
                self.a.bind(out);
            } else if let Some(tpa) = self.fetch_page(target) {
                self.cross_link(target, tpa);
            }
        }
        self.set_pc(target);
        self.to_dispatcher();
    }
    /// A patchable jump into `target`'s block on another page (physical page `tpa`), taken while
    /// the budget lasts and the fetch micro-TLB maps `target`'s page to `tpa`.
    fn cross_link(&mut self, target: u64, tpa: u64) {
        let s = crate::cpu::uti(target);
        let (e0, e1) = (self.o.utlb + 16 * s as i32, self.o.utlb + 16 * (s as i32 + 1));
        self.a.alu_load(Alu::Cmp, true, ICOUNT, m(CPU, self.o.chain_until));
        let mut fb = vec![self.a.jcc(AE)];
        self.a.mov_imm(RAX, crate::cpu::utag(target, self.el));
        self.a.mov_imm(RCX, e0 as u64);
        self.a.alu_load(Alu::Cmp, true, RAX, mx(CPU, RCX, 0, 0));
        let hit = self.a.jcc(E);
        self.a.mov_imm(RCX, e1 as u64);
        self.a.alu_load(Alu::Cmp, true, RAX, mx(CPU, RCX, 0, 0));
        fb.push(self.a.jcc(NE));
        self.a.bind(hit);
        self.a.mov_imm(RAX, tpa);
        self.a.alu_load(Alu::Cmp, true, RAX, mx(CPU, RCX, 0, 8));
        fb.push(self.a.jcc(NE));
        let site = self.a.jmp();
        self.links.push((site, target, Some(tpa)));
        for f in fb { self.a.bind(f); }
    }
    /// Call the interpreter for the instruction at `pc` (registers written back by the caller);
    /// returns true if the block ends here.
    fn interp(&mut self, i: u32, pc: u64) -> bool {
        let end = is_block_end(i);
        self.sync();
        self.count_pending();
        self.a.mov_imm(RSI, pc + 4);
        self.a.mov_imm(RDX, self.chain.unwrap_or(u64::MAX));
        self.call_abs(self.st.interp);
        self.fl = Fl::Reg;
        if end { self.to_dispatcher(); }
        end
    }

    // ---- integer data processing
    /// rd = op(n, b) on cache registers or scratch; `d` may be any of them.
    fn binop(&mut self, op: Alu, w: bool, d: Reg, n: Reg, b: Reg, commutative: bool) {
        if d == n { self.a.alu(op, w, d, b); }
        else if d == b {
            if commutative { self.a.alu(op, w, d, n); }
            else { self.a.mov(w, RAX, n); self.a.alu(op, w, RAX, b); self.a.mov(w, d, RAX); }
        } else { self.a.mov(w, d, n); self.a.alu(op, w, d, b); }
    }

    /// Integer data processing (only encodings `native_operands` accepts); false = not here.
    fn dp(&mut self, i: u32) -> bool {
        let sf = bit(i, 31);
        let (rd, rn, rm, ra) = (i & 31, (i >> 5) & 31, (i >> 16) & 31, (i >> 10) & 31);
        match bits(i, 28, 23) {
            0b100010 => { self.addsub_imm(i, sf, rd, rn); return true; }
            0b100100 => { self.logic_imm(i, sf, rd, rn); return true; }
            0b100101 => { self.movw(i, sf, rd); return true; }
            0b100110 => { self.bitfield(i, sf, rd, rn); return true; }
            0b100111 => {
                // EXTR: rd = rm >> lsb | rn << (width - lsb)
                let lsb = bits(i, 15, 10) as u8;
                if rd == 31 { return true; }
                let (n, mm) = (self.get(rn, How::Zr), self.get(rm, How::Zr));
                let d = self.def(rd, How::Zr).unwrap();
                self.clobber();
                if let (true, Some(h)) = (rn == rm, n) {
                    // ROR (immediate)
                    if d != h || !sf { self.a.mov(sf, d, h); }
                    if lsb != 0 { self.a.shift(Sh::Ror, sf, d, lsb); }
                    self.dirty(rd, How::Zr);
                    return true;
                }
                let n = self.val(n, RCX);
                match mm { Some(h) => self.a.mov(sf, RAX, h), None => self.a.mov_imm(RAX, 0) }
                if lsb != 0 { self.a.shrd(sf, RAX, n, lsb); }
                self.a.mov(sf, d, RAX);
                self.dirty(rd, How::Zr);
                return true;
            }
            _ => {}
        }
        if bits(i, 28, 24) == 0b01010 { self.logic_reg(i, sf, rd, rn, rm); return true; }
        if bits(i, 28, 24) == 0b01011 {
            if bit(i, 21) { self.addsub_ext(i, sf, rd, rn, rm); } else { self.addsub_reg(i, sf, rd, rn, rm); }
            return true;
        }
        match bits(i, 28, 21) {
            0b11010000 => { self.adc(i, sf, rd, rn, rm); true }
            0b11010010 => { self.ccmp(i, sf, rn, rm); true }
            0b11010100 => { self.csel(i, sf, rd, rn, rm); true }
            0b11010110 if !bit(i, 30) => self.dp2(i, sf, rd, rn, rm),
            0b11010110 => self.dp1(i, sf, rd, rn),
            _ if bits(i, 28, 24) == 0b11011 => { self.dp3(i, sf, rd, rn, rm, ra); true }
            _ => false,
        }
    }

    fn addsub_imm(&mut self, i: u32, sf: bool, rd: u32, rn: u32) {
        let (sub, s) = (bit(i, 30), bit(i, 29));
        let imm = (bits(i, 21, 10) as i32) << if bit(i, 22) { 12 } else { 0 };
        let n = self.get(rn, How::Sp).unwrap();
        if !s {
            // LEA: the flags stay where they are
            let d = self.def(rd, How::Sp).unwrap();
            let disp = if sub { -imm } else { imm };
            if disp == 0 { if d != n || !sf { self.a.mov(sf, d, n); } } else { self.a.lea(sf, d, m(n, disp)); }
            self.dirty(rd, How::Sp);
            return;
        }
        if rd == 31 {
            if sub { self.a.alu_imm(Alu::Cmp, sf, n, imm); }
            else { self.a.mov(sf, RAX, n); self.a.alu_imm(Alu::Add, sf, RAX, imm); self.a.cmc(); }
        } else {
            let d = self.def(rd, How::Zr).unwrap();
            if d != n { self.a.mov(sf, d, n); }
            self.a.alu_imm(if sub { Alu::Sub } else { Alu::Add }, sf, d, imm);
            if !sub { self.a.cmc(); }
            self.dirty(rd, How::Zr);
        }
        self.fl = Fl::Eflags;
    }

    fn logic_imm(&mut self, i: u32, sf: bool, rd: u32, rn: u32) {
        let opc = bits(i, 30, 29);
        let imm = crate::exec::decode_bitmask(sf, bits(i, 22, 22), bits(i, 15, 10), bits(i, 21, 16)).expect("checked");
        let n = self.get(rn, How::Zr);
        let fits = !sf || (imm as i64) == (imm as i32 as i64);
        if opc == 1 && n.is_none() {
            // MOV (bitmask immediate)
            let d = self.def(rd, How::Sp).unwrap();
            if sf { self.a.mov_imm(d, imm); } else { self.a.mov_imm(d, imm & 0xffff_ffff); }
            self.dirty(rd, How::Sp);
            return;
        }
        if opc != 3 { self.clobber(); }
        let src = self.val(n, RAX);
        let b = if fits { None } else { self.a.mov_imm(RCX, imm); Some(RCX) };
        let op = [Alu::And, Alu::Or, Alu::Xor, Alu::And][opc as usize];
        if opc == 3 && rd == 31 {
            match b { Some(r) => self.a.test(sf, src, r), None => self.a.test_imm(sf, src, imm as i32) }
        } else {
            let how = if opc == 3 { How::Zr } else { How::Sp };
            let d = self.def(rd, how).unwrap_or(RAX);
            if d != src { self.a.mov(sf, d, src); }
            match b { Some(r) => self.a.alu(op, sf, d, r), None => self.a.alu_imm(op, sf, d, imm as i32) }
            self.dirty(rd, how);
        }
        if opc == 3 { self.a.stc(); self.fl = Fl::Eflags; }
    }

    fn movw(&mut self, i: u32, sf: bool, rd: u32) {
        let hw = bits(i, 22, 21);
        let imm = (bits(i, 20, 5) as u64) << (hw * 16);
        if rd == 31 { return; }
        let mask = if sf { u64::MAX } else { 0xffff_ffff };
        match bits(i, 30, 29) {
            0 | 2 => {
                let v = if bits(i, 30, 29) == 0 { !imm & mask } else { imm };
                let d = self.def(rd, How::Zr).unwrap();
                self.a.mov_imm(d, v);
            }
            _ => {
                // MOVK
                let d = self.get(rd, How::Zr).unwrap();
                if hw == 0 {
                    self.a.mov_imm16(d, imm as u16);
                    if !sf { self.a.mov(false, d, d); }
                } else {
                    self.clobber();
                    self.a.mov_imm(RCX, !(0xffffu64 << (hw * 16)) & mask);
                    self.a.alu(Alu::And, sf, d, RCX);
                    self.a.mov_imm(RCX, imm);
                    self.a.alu(Alu::Or, sf, d, RCX);
                }
            }
        }
        self.dirty(rd, How::Zr);
    }

    /// UBFM / SBFM of the value in rax (`sf` width) into rax.
    fn bf_shifts(&mut self, sf: bool, signed: bool, immr: u32, imms: u32) {
        let w = if sf { 64 } else { 32 };
        let l = w - 1 - imms;
        let r = if imms >= immr { l + immr } else { immr - 1 - imms };
        if l != 0 { self.a.shift(Sh::Shl, sf, RAX, l as u8); }
        if r != 0 { self.a.shift(if signed { Sh::Sar } else { Sh::Shr }, sf, RAX, r as u8); }
    }

    fn bitfield(&mut self, i: u32, sf: bool, rd: u32, rn: u32) {
        let (immr, imms, opc) = (bits(i, 21, 16), bits(i, 15, 10), bits(i, 30, 29));
        if rd == 31 { return; }
        let n = self.get(rn, How::Zr);
        if opc != 1 && immr == 0 {
            // extensions: MOVZX / MOVSX, flags untouched
            let ext = match (opc, imms, sf) { (2, 7, _) | (2, 15, _) | (2, 31, true) | (0, 7, _) | (0, 15, _) | (0, 31, true) => true, _ => false };
            if ext {
                let d = self.def(rd, How::Zr).unwrap();
                match n {
                    None => self.a.mov_imm(d, 0),
                    Some(h) => match (opc, imms) {
                        (2, 7) => self.a.movzx8(d, h),
                        (2, 15) => self.a.movzx16(d, h),
                        (2, _) => self.a.mov(false, d, h),
                        (_, 7) => { self.a.movsx8(sf, d, h); }
                        (_, 15) => { self.a.movsx16(sf, d, h); }
                        _ => self.a.movsxd(d, h),
                    },
                }
                self.dirty(rd, How::Zr);
                return;
            }
        }
        let d = if opc == 1 { self.get(rd, How::Zr).unwrap() } else { self.def(rd, How::Zr).unwrap() };
        self.clobber();
        match n { Some(h) => self.a.mov(sf, RAX, h), None => self.a.mov_imm(RAX, 0) }
        self.bf_shifts(sf, opc == 0, immr, imms);
        if opc == 1 {
            // BFM: insert the field
            let w = if sf { 64 } else { 32 };
            let (width, pos) = if imms >= immr { (imms - immr + 1, 0) } else { (imms + 1, w - immr) };
            let ones = if width >= 64 { u64::MAX } else { (1u64 << width) - 1 };
            let keep = !(ones << pos) & if sf { u64::MAX } else { 0xffff_ffff };
            self.a.mov_imm(RCX, keep);
            self.a.alu(Alu::And, sf, RCX, d);
            self.a.alu(Alu::Or, sf, RCX, RAX);
            self.a.mov(sf, d, RCX);
        } else {
            self.a.mov(sf, d, RAX);
        }
        self.dirty(rd, How::Zr);
    }

    fn logic_reg(&mut self, i: u32, sf: bool, rd: u32, rn: u32, rm: u32) {
        let (opc, inv, ty, amt) = (bits(i, 30, 29), bit(i, 21), bits(i, 23, 22), bits(i, 15, 10) as u8);
        if rd == 31 && opc != 3 { return; }
        let (n, mm) = (self.get(rn, How::Zr), self.get(rm, How::Zr));
        if opc == 1 && n.is_none() && amt == 0 {
            // MOV / MVN (register): flags untouched
            let d = self.def(rd, How::Zr).unwrap();
            match mm { Some(h) => self.a.mov(sf, d, h), None => self.a.mov_imm(d, 0) }
            if inv { self.a.grp3(2, sf, d); }
            self.dirty(rd, How::Zr);
            return;
        }
        let d = if rd == 31 { RAX } else { self.def(rd, How::Zr).unwrap() };
        if opc != 3 { self.clobber(); }
        let b = match mm {
            Some(h) if amt == 0 && !inv => h,
            _ => {
                match mm { Some(h) => self.a.mov(sf, RCX, h), None => self.a.mov_imm(RCX, 0) }
                if amt != 0 { self.a.shift([Sh::Shl, Sh::Shr, Sh::Sar, Sh::Ror][ty as usize], sf, RCX, amt); }
                if inv { self.a.grp3(2, sf, RCX); }
                RCX
            }
        };
        let src = self.val(n, RAX);
        let op = [Alu::And, Alu::Or, Alu::Xor, Alu::And][opc as usize];
        if opc == 3 && rd == 31 { self.a.test(sf, src, b); } else { self.binop(op, sf, d, src, b, true); self.dirty(rd, How::Zr); }
        if opc == 3 { self.a.stc(); self.fl = Fl::Eflags; }
    }

    fn addsub_reg(&mut self, i: u32, sf: bool, rd: u32, rn: u32, rm: u32) {
        let (sub, s, ty, amt) = (bit(i, 30), bit(i, 29), bits(i, 23, 22), bits(i, 15, 10) as u8);
        if rd == 31 && !s { return; }
        let (n, mm) = (self.get(rn, How::Zr), self.get(rm, How::Zr));
        let d = if rd == 31 { None } else { self.def(rd, How::Zr) };
        if let (false, false, 0, true, Some(hn), Some(hm), Some(d)) = (sub, s, ty, amt <= 3, n, mm, d) {
            self.a.lea(sf, d, mx(hn, hm, amt, 0));
            self.dirty(rd, How::Zr);
            return;
        }
        if !s { self.clobber(); }
        let b = match mm {
            Some(h) if amt == 0 => h,
            _ => {
                match mm { Some(h) => self.a.mov(sf, RCX, h), None => self.a.mov_imm(RCX, 0) }
                if amt != 0 { self.a.shift([Sh::Shl, Sh::Shr, Sh::Sar, Sh::Ror][ty as usize], sf, RCX, amt); }
                RCX
            }
        };
        let src = self.val(n, RAX);
        self.addsub_tail(sf, sub, s, rd, d, src, b);
    }

    /// The add/sub itself: into `d` (None = the zero register: a comparison), flags if `s`.
    fn addsub_tail(&mut self, sf: bool, sub: bool, s: bool, rd: u32, d: Option<Reg>, src: Reg, b: Reg) {
        match d {
            None => {
                if sub { self.a.alu(Alu::Cmp, sf, src, b); }
                else { if src != RAX { self.a.mov(sf, RAX, src); } self.a.alu(Alu::Add, sf, RAX, b); }
            }
            Some(d) => {
                self.binop(if sub { Alu::Sub } else { Alu::Add }, sf, d, src, b, !sub);
                self.dirty(rd, if s { How::Zr } else { How::Sp });
            }
        }
        if s {
            if !sub { self.a.cmc(); }
            self.fl = Fl::Eflags;
        }
    }

    fn addsub_ext(&mut self, i: u32, sf: bool, rd: u32, rn: u32, rm: u32) {
        let (sub, s, option, sh) = (bit(i, 30), bit(i, 29), bits(i, 15, 13), bits(i, 12, 10) as u8);
        let n = self.get(rn, How::Sp).unwrap();
        let mm = self.get(rm, How::Zr);
        let d = if s && rd == 31 { None } else { self.def(rd, if s { How::Zr } else { How::Sp }) };
        // the extended operand in rcx
        match mm {
            None => self.a.mov_imm(RCX, 0),
            Some(h) => match option {
                0 => self.a.movzx8(RCX, h),
                1 => self.a.movzx16(RCX, h),
                2 => self.a.mov(false, RCX, h),
                4 => self.a.movsx8(true, RCX, h),
                5 => self.a.movsx16(true, RCX, h),
                6 => self.a.movsxd(RCX, h),
                _ => self.a.mov(true, RCX, h),
            },
        }
        if let (false, false, true, Some(d)) = (sub, s, sh <= 3, d) {
            self.a.lea(sf, d, mx(n, RCX, sh, 0));
            self.dirty(rd, How::Sp);
            return;
        }
        if !s { self.clobber(); }
        if sh != 0 { self.a.shift(Sh::Shl, true, RCX, sh); }
        self.addsub_tail(sf, sub, s, rd, d, n, RCX);
    }

    fn adc(&mut self, i: u32, sf: bool, rd: u32, rn: u32, rm: u32) {
        let (sbc, s) = (bit(i, 30), bit(i, 29));
        let (n, mm) = (self.get(rn, How::Zr), self.get(rm, How::Zr));
        let d = if rd == 31 { RAX } else { self.def(rd, How::Zr).unwrap() };
        if !s { self.sync(); }
        self.need_eflags();
        // movs keep the flags; C = NOT CF: ADC needs CF = C
        if !sbc { self.a.cmc(); }
        let src = self.val(n, RAX);
        let b = self.val(mm, RCX);
        self.binop(if sbc { Alu::Sbb } else { Alu::Adc }, sf, d, src, b, !sbc);
        if rd != 31 { self.dirty(rd, How::Zr); }
        if s {
            if !sbc { self.a.cmc(); }
            self.fl = Fl::Eflags;
        } else {
            self.fl = Fl::Reg;
        }
    }

    fn ccmp(&mut self, i: u32, sf: bool, rn: u32, rm: u32) {
        let (sub, cond, nzcv, imm) = (bit(i, 30), bits(i, 15, 12), bits(i, 3, 0), bit(i, 11));
        let n = self.get(rn, How::Zr);
        let mm = if imm { None } else { self.get(rm, How::Zr) };
        self.need_eflags();
        let skip = cc_of(cond).map(|cc| self.a.jcc(cc ^ 1));
        let src = self.val(n, RAX);
        let b = if imm { None } else { Some(self.val(mm, RCX)) };
        if sub {
            match b { Some(r) => self.a.alu(Alu::Cmp, sf, src, r), None => self.a.alu_imm(Alu::Cmp, sf, src, rm as i32) }
        } else {
            if src != RAX { self.a.mov(sf, RAX, src); }
            match b { Some(r) => self.a.alu(Alu::Add, sf, RAX, r), None => self.a.alu_imm(Alu::Add, sf, RAX, rm as i32) }
            self.a.cmc();
        }
        if let Some(s) = skip {
            let j = self.a.jmp();
            self.a.bind(s);
            self.a.mov_imm(RAX, nzcv_to_fl(nzcv << 28));
            self.a.add_al(0x7f);
            self.a.sahf();
            self.a.bind(j);
        }
        self.fl = Fl::Eflags;
    }

    fn csel(&mut self, i: u32, sf: bool, rd: u32, rn: u32, rm: u32) {
        let (op, o2, cond) = (bit(i, 30), bit(i, 10), bits(i, 15, 12));
        if rd == 31 { return; }
        let (n, mm) = (self.get(rn, How::Zr), self.get(rm, How::Zr));
        let d = self.def(rd, How::Zr).unwrap();
        let Some(cc) = cc_of(cond) else {
            match n { Some(h) => self.a.mov(sf, d, h), None => self.a.mov_imm(d, 0) }
            self.dirty(rd, How::Zr);
            return;
        };
        if op && o2 { self.clobber(); }
        match mm { Some(h) => self.a.mov(true, RCX, h), None => self.a.mov_imm(RCX, 0) }
        match (op, o2) {
            (false, false) => {}
            (false, true) => self.a.lea(true, RCX, m(RCX, 1)),
            (true, false) => self.a.grp3(2, true, RCX),
            (true, true) => self.a.grp3(3, true, RCX),
        }
        self.need_eflags();
        let nr = self.val(n, RDX);
        self.a.cmov(cc, true, RCX, nr);
        self.a.mov(sf, d, RCX);
        self.dirty(rd, How::Zr);
    }

    fn dp2(&mut self, i: u32, sf: bool, rd: u32, rn: u32, rm: u32) -> bool {
        let op = bits(i, 15, 10);
        if !matches!(op, 2 | 3 | 8..=11) { return false; }
        if rd == 31 { return true; }
        let (n, mm) = (self.get(rn, How::Zr), self.get(rm, How::Zr));
        let d = self.def(rd, How::Zr).unwrap();
        self.clobber();
        match mm { Some(h) => self.a.mov(sf, RCX, h), None => self.a.mov_imm(RCX, 0) }
        match n { Some(h) => self.a.mov(sf, RAX, h), None => self.a.mov_imm(RAX, 0) }
        if op >= 8 {
            // the count is taken modulo the width, as on A64
            self.a.shift_cl([Sh::Shl, Sh::Shr, Sh::Sar, Sh::Ror][op as usize - 8], sf, RAX);
        } else {
            // division by zero gives 0; INT_MIN / -1 wraps (x86 would fault on both)
            self.a.test(sf, RCX, RCX);
            let zero = self.a.jcc(E);
            let mut done = Vec::new();
            if op == 3 {
                self.a.alu_imm(Alu::Cmp, sf, RCX, -1);
                let div = self.a.jcc(NE);
                self.a.grp3(3, sf, RAX);
                done.push(self.a.jmp());
                self.a.bind(div);
                self.a.cqo(sf);
                self.a.grp3(7, sf, RCX);
            } else {
                self.a.mov_imm(RDX, 0);
                self.a.grp3(6, sf, RCX);
            }
            done.push(self.a.jmp());
            self.a.bind(zero);
            self.a.mov_imm(RAX, 0);
            for p in done { self.a.bind(p); }
        }
        self.a.mov(sf, d, RAX);
        self.dirty(rd, How::Zr);
        true
    }

    fn dp1(&mut self, i: u32, sf: bool, rd: u32, rn: u32) -> bool {
        let op = bits(i, 15, 10);
        if rd == 31 { return true; }
        let n = self.get(rn, How::Zr);
        let d = self.def(rd, How::Zr).unwrap();
        self.clobber();
        match n { Some(h) => self.a.mov(sf, RAX, h), None => self.a.mov_imm(RAX, 0) }
        match (op, sf) {
            (0, _) => {
                // RBIT: reverse the bytes, then swap nibbles, bit pairs and bits
                self.a.bswap(sf, RAX);
                for (sh, mask) in [(4u8, 0x0f0f_0f0f_0f0f_0f0fu64), (2, 0x3333_3333_3333_3333), (1, 0x5555_5555_5555_5555)] {
                    self.a.mov_imm(RDX, if sf { mask } else { mask & 0xffff_ffff });
                    self.a.mov(sf, RCX, RAX);
                    self.a.shift(Sh::Shr, sf, RAX, sh);
                    self.a.alu(Alu::And, sf, RAX, RDX);
                    self.a.alu(Alu::And, sf, RCX, RDX);
                    self.a.shift(Sh::Shl, sf, RCX, sh);
                    self.a.alu(Alu::Or, sf, RAX, RCX);
                }
            }
            (1, false) => { self.a.bswap(false, RAX); self.a.shift(Sh::Ror, false, RAX, 16); }
            (1, true) => {
                // swap the bytes of each halfword
                self.a.mov(true, RCX, RAX);
                self.a.shift(Sh::Shr, true, RAX, 8);
                self.a.shift(Sh::Shl, true, RCX, 8);
                self.a.mov_imm(RDX, 0x00ff_00ff_00ff_00ff);
                self.a.alu(Alu::And, true, RAX, RDX);
                self.a.grp3(2, true, RDX);
                self.a.alu(Alu::And, true, RCX, RDX);
                self.a.alu(Alu::Or, true, RAX, RCX);
            }
            (2, false) => self.a.bswap(false, RAX),
            (2, true) => { self.a.bswap(true, RAX); self.a.shift(Sh::Ror, true, RAX, 32); }
            (3, _) => self.a.bswap(true, RAX),
            (4, _) => {
                // CLZ = (width - 1) - BSR, width for 0
                let w = if sf { 64 } else { 32 };
                self.a.mov_imm(RCX, 2 * w - 1);
                self.a.bsr(sf, RAX, RAX);
                self.a.cmov(E, false, RAX, RCX);
                self.a.alu_imm(Alu::Xor, false, RAX, w as i32 - 1);
            }
            _ => {
                // CLS = CLZ((x ^ x << 1) | 1)
                let w = if sf { 64 } else { 32 };
                self.a.lea(sf, RCX, mx(RAX, RAX, 0, 0));
                self.a.alu(Alu::Xor, sf, RAX, RCX);
                self.a.alu_imm(Alu::Or, sf, RAX, 1);
                self.a.bsr(sf, RAX, RAX);
                self.a.alu_imm(Alu::Xor, false, RAX, w - 1);
            }
        }
        self.a.mov(sf, d, RAX);
        self.dirty(rd, How::Zr);
        true
    }

    fn dp3(&mut self, i: u32, sf: bool, rd: u32, rn: u32, rm: u32, ra: u32) {
        let (op, sub) = (bits(i, 23, 21), bit(i, 15));
        if rd == 31 { return; }
        let (n, mm, acc) = (self.get(rn, How::Zr), self.get(rm, How::Zr), self.get(ra, How::Zr));
        let d = self.def(rd, How::Zr).unwrap();
        self.clobber();
        let w = match op {
            0 => {
                match n { Some(h) => self.a.mov(sf, RAX, h), None => self.a.mov_imm(RAX, 0) }
                let b = self.val(mm, RCX);
                self.a.imul(sf, RAX, b);
                sf
            }
            1 | 5 => {
                // SMADDL / UMADDL: 32 x 32 -> 64
                for (r, x) in [(n, RAX), (mm, RCX)] {
                    match r { None => self.a.mov_imm(x, 0), Some(h) if op == 1 => self.a.movsxd(x, h), Some(h) => self.a.mov(false, x, h) }
                }
                self.a.imul(true, RAX, RCX);
                true
            }
            _ => {
                // SMULH / UMULH: rdx:rax = rax * rcx
                match n { Some(h) => self.a.mov(true, RAX, h), None => self.a.mov_imm(RAX, 0) }
                let b = self.val(mm, RCX);
                self.a.grp3(if op == 2 { 5 } else { 4 }, true, b);
                self.a.mov(true, d, RDX);
                self.dirty(rd, How::Zr);
                return;
            }
        };
        if sub {
            match acc { Some(h) => self.a.mov(w, RCX, h), None => self.a.mov_imm(RCX, 0) }
            self.a.alu(Alu::Sub, w, RCX, RAX);
            self.a.mov(w, d, RCX);
        } else {
            if let Some(h) = acc { self.a.alu(Alu::Add, w, RAX, h); }
            self.a.mov(w, d, RAX);
        }
        self.dirty(rd, How::Zr);
    }

    // ---- branches
    /// Branches: Some(true) = emitted and the block ends; Some(false) = a superblock continues
    /// past it; None = not a branch handled here.
    fn branch(&mut self, i: u32, pc: u64) -> Option<bool> {
        let sx = |v: u64, w: u32| ((v << (64 - w)) as i64 >> (64 - w)) as u64;
        // B / BL
        if bits(i, 30, 26) == 0b00101 {
            let target = pc.wrapping_add(sx((bits(i, 25, 0) as u64) << 2, 28));
            self.ra_flush();
            self.ra_forget();
            if bit(i, 31) {
                self.a.mov_imm(RAX, pc + 4);
                self.a.store(8, m(CPU, self.o.x + 8 * 30), RAX);
            }
            self.pending += 1;
            self.exit_const(target);
            return Some(true);
        }
        // B.cond
        if bits(i, 31, 24) == 0b0101_0100 && !bit(i, 4) {
            let target = pc.wrapping_add(sx((bits(i, 23, 5) as u64) << 2, 21));
            let Some(cc) = cc_of(bits(i, 3, 0)) else {
                // B.AL / B.NV: always taken
                self.ra_flush();
                self.ra_forget();
                self.pending += 1;
                self.exit_const(target);
                return Some(true);
            };
            if self.sb && sb_continue(pc, target) {
                self.pending += 1;
                self.count_pending();
                self.need_eflags();
                let save = self.fl == Fl::Eflags;
                let at = self.a.jcc(cc);
                let spill = self.dirty_list();
                self.sides.push(SideExit { at, save, spill, target });
                return Some(false);
            }
            self.ra_flush();
            self.pending += 1;
            self.count_pending();
            self.sync();
            self.need_eflags();
            let at = self.a.jcc(cc);
            self.exit_const(pc + 4);
            self.a.bind(at);
            self.exit_const(target);
            return Some(true);
        }
        // CBZ / CBNZ / TBZ / TBNZ
        let cb = bits(i, 30, 25) == 0b011010;
        let tb = bits(i, 30, 25) == 0b011011;
        if (cb || tb) && i & 31 != 31 {
            let target = if cb { pc.wrapping_add(sx((bits(i, 23, 5) as u64) << 2, 21)) } else { pc.wrapping_add(sx((bits(i, 18, 5) as u64) << 2, 16)) };
            let h = self.get(i & 31, How::Zr).unwrap();
            let nz = bit(i, 24);
            let sb = self.sb && sb_continue(pc, target);
            if !sb { self.ra_flush(); }
            self.pending += 1;
            self.count_pending();
            self.clobber();
            let cc = if cb {
                self.a.test(bit(i, 31), h, h);
                if nz { NE } else { E }
            } else {
                self.a.bt(true, h, ((bits(i, 31, 31) << 5) | bits(i, 23, 19)) as u8);
                if nz { B } else { AE }
            };
            let at = self.a.jcc(cc);
            if sb {
                let spill = self.dirty_list();
                self.sides.push(SideExit { at, save: false, spill, target });
                return Some(false);
            }
            self.exit_const(pc + 4);
            self.a.bind(at);
            self.exit_const(target);
            return Some(true);
        }
        // BR / BLR / RET
        if bits(i, 31, 25) == 0b110_1011 && bits(i, 20, 16) == 0b11111 && bits(i, 15, 10) == 0 && bits(i, 4, 0) == 0 {
            let opc = bits(i, 24, 21);
            if opc > 2 { return None; }
            let h = self.get(bits(i, 9, 5), How::Zr);
            match h { Some(h) => self.a.store(8, m(CPU, self.o.pc), h), None => self.a.store_imm(8, m(CPU, self.o.pc), 0) }
            self.ra_flush();
            self.ra_forget();
            if opc == 1 {
                self.a.mov_imm(RAX, pc + 4);
                self.a.store(8, m(CPU, self.o.x + 8 * 30), RAX);
            }
            self.pending += 1;
            self.to_dispatcher();
            return Some(true);
        }
        None
    }

    // ---- memory
    /// The micro-TLB lookup (see `jit.rs`'s `emit_utlb`): with the VA in rdx, leaves rax = the
    /// RAM offset of the access, or jumps to a slow path (sites pushed to `slow`). `k`: 1 load,
    /// 2 store entries; `span`: bytes accessed; `excl_check`: a held reservation takes the slow
    /// path. With `way1`, a way-0 mismatch goes to an out-of-line way-1 check (returned) that
    /// jumps back on a hit. Uses rax and rcx (rcx = the set's offset from way 0).
    fn probe(&mut self, el: u8, k: usize, span: u32, unpriv: bool, excl_check: bool, slow: &mut Vec<usize>, way1: bool) -> Option<(usize, usize, i32)> {
        self.probe_x(el, k, span, unpriv, excl_check, slow, way1).0
    }
    /// `probe`; with `way1`, a held reservation goes to an out-of-line granule check (returned:
    /// its branch and where it rejoins): as in the interpreter, only a store to the reserved
    /// 64-byte granule needs the slow path (which clears the reservation).
    fn probe_x(&mut self, el: u8, k: usize, span: u32, unpriv: bool, excl_check: bool, slow: &mut Vec<usize>, way1: bool) -> (Option<(usize, usize, i32)>, Option<(usize, usize)>) {
        debug_assert!(!UTLB_HASH);
        let ent = self.o.utlb + (16 * UT * k) as i32;
        // the tag of the access's last byte: a page crossing misses (see `jit.rs`)
        if span > 1 { self.a.lea(true, RAX, m(RDX, span as i32 - 1)); } else { self.a.mov(true, RAX, RDX); }
        self.a.shift(Sh::Shr, true, RAX, 12);
        if unpriv { self.a.alu_load(Alu::Or, true, RAX, m(CPU, self.o.utlb_unpriv)); }
        else {
            if el & 1 != 0 { self.a.bts(true, RAX, 60); }
            if el & 2 != 0 { self.a.bts(true, RAX, 61); }
        }
        // the set's offset: ((va >> 12) % (UT / 2)) * 32
        self.a.mov(false, RCX, RDX);
        self.a.shift(Sh::Shr, false, RCX, 7);
        self.a.alu_imm(Alu::And, false, RCX, ((UT / 2 - 1) << 5) as i32);
        self.a.alu_load(Alu::Cmp, true, RAX, mx(CPU, RCX, 0, ent));
        let site = self.a.jcc(NE);
        let back = self.a.pos();
        let w1 = if way1 { Some((site, back, ent)) } else { slow.push(site); None };
        let mut granule = None;
        if excl_check {
            self.a.alu_mem_imm8(Alu::Cmp, 8, m(CPU, self.o.excl), 0);
            let j = self.a.jcc(NE);
            if way1 { granule = Some((j, self.a.pos())); } else { slow.push(j); }
        }
        // data entries hold PA - VA of the page
        self.a.load(true, RAX, mx(CPU, RCX, 0, ent + 8));
        self.a.alu(Alu::Add, true, RAX, RDX);
        (w1, granule)
    }

    /// Integer and FP/SIMD LDR/STR (immediate, unscaled, pre/post-index, register offset,
    /// LDTR/STTR) and LDP/STP/LDPSW/LDNP/STNP: a micro-TLB fast path, the interpreter otherwise.
    fn ldst(&mut self, cpu: &Cpu, i: u32, pc: u64) -> bool {
        if cpu.ram.is_null() { return false; }
        enum Addr { Imm(i64), Reg(u32, u32, u32) }
        let rt = i & 31;
        let rn = (i >> 5) & 31;
        let (size, load, signed_to, pair, rt2, addr, mode): (u32, bool, u32, bool, u32, Addr, u8);
        let mut unpriv = false;
        let vec = bit(i, 26);
        if vec && (cpu.sys.cpacr_el1 >> 20) & 3 != 3 { return false; }
        if vec && bits(i, 29, 27) == 0b111 && !bit(i, 25) {
            let sz = ((bit(i, 23) as u32) << 2) | bits(i, 31, 30);
            if sz > 4 { return false; }
            size = 1 << sz; load = bit(i, 22); signed_to = 0; pair = false; rt2 = 0;
            if bit(i, 24) {
                addr = Addr::Imm((bits(i, 21, 10) as i64) << sz); mode = 0;
            } else if bit(i, 21) {
                if bits(i, 11, 10) != 0b10 || bits(i, 15, 13) & 2 == 0 { return false; }
                addr = Addr::Reg(bits(i, 20, 16), bits(i, 15, 13), if bit(i, 12) { sz } else { 0 }); mode = 0;
            } else {
                let imm = ((bits(i, 20, 12) as i64) << 55) >> 55;
                match bits(i, 11, 10) { 0 => { addr = Addr::Imm(imm); mode = 0; } 1 => { addr = Addr::Imm(imm); mode = 1; } 3 => { addr = Addr::Imm(imm); mode = 2; } _ => return false }
            }
        } else if vec && bits(i, 29, 27) == 0b101 && !bit(i, 25) {
            let sz = match bits(i, 31, 30) { 0 => 2, 1 => 3, 2 => 4, _ => return false };
            size = 1 << sz; load = bit(i, 22); signed_to = 0; pair = true; rt2 = bits(i, 14, 10);
            addr = Addr::Imm((((bits(i, 21, 15) as i64) << 57) >> 57) << sz);
            mode = match bits(i, 24, 23) { 1 => 1, 3 => 2, _ => 0 };
            if load && rt == rt2 { return false; }
        } else if vec {
            return false;
        } else if bits(i, 29, 27) == 0b111 && !bit(i, 26) && !bit(i, 25) {
            let sz = bits(i, 31, 30);
            let opc = bits(i, 23, 22);
            let (ld, st) = match (sz, opc) { (_, 0) => (false, 0), (_, 1) => (true, 0), (3, _) => return false, (2, 3) => return false, (_, 2) => (true, 64), (_, _) => (true, 32) };
            size = 1 << sz; load = ld; signed_to = st; pair = false; rt2 = 0;
            if bit(i, 24) {
                addr = Addr::Imm((bits(i, 21, 10) as i64) << sz); mode = 0;
            } else if bit(i, 21) {
                if bits(i, 11, 10) != 0b10 || bits(i, 15, 13) & 2 == 0 { return false; }
                addr = Addr::Reg(bits(i, 20, 16), bits(i, 15, 13), if bit(i, 12) { sz } else { 0 }); mode = 0;
            } else {
                let imm = ((bits(i, 20, 12) as i64) << 55) >> 55;
                match bits(i, 11, 10) {
                    0 => { addr = Addr::Imm(imm); mode = 0; }
                    1 => { addr = Addr::Imm(imm); mode = 1; }
                    3 => { addr = Addr::Imm(imm); mode = 2; }
                    _ => { if cpu.el > 1 { return false; } unpriv = cpu.el == 1; addr = Addr::Imm(imm); mode = 0; }
                }
            }
        } else if bits(i, 29, 27) == 0b101 && !bit(i, 26) && !bit(i, 25) {
            let opc = bits(i, 31, 30);
            let ld = bit(i, 22);
            if bits(i, 24, 23) == 0 && opc == 1 { return false; } // no LDNPSW
            let (sz, st) = match (opc, ld) { (0, _) => (2, 0), (1, true) => (2, 64), (2, _) => (3, 0), _ => return false };
            size = 1 << sz; load = ld; signed_to = st; pair = true; rt2 = bits(i, 14, 10);
            addr = Addr::Imm((((bits(i, 21, 15) as i64) << 57) >> 57) << sz);
            mode = match bits(i, 24, 23) { 1 => 1, 3 => 2, _ => 0 };
            if load && rt == rt2 { return false; }
        } else {
            return false;
        }
        // write-back onto a loaded/stored register (the vector forms name V registers: no clash)
        if mode != 0 && rn != 31 && !vec && (rn == rt || (pair && rn == rt2)) { return false; }
        if !load && cpu.jit.as_ref().is_some_and(|j| j.verify) { return false; } // verify mode logs stores in the interpreter
        let span = if pair { 2 * size } else { size };
        self.clobber();
        let pending = self.pending;
        // every cache register the access uses, before the first branch to the slow path
        let base = self.get(rn, How::Sp).unwrap();
        let idx = if let Addr::Reg(rm, _, _) = addr { self.get(rm, How::Zr) } else { None };
        let (h1, h2) = if vec { (None, None) }
            else if load { (self.def(rt, How::Zr), if pair { self.def(rt2, How::Zr) } else { None }) }
            else { (self.get(rt, How::Zr), if pair { self.get(rt2, How::Zr) } else { None }) };
        let spill = self.dirty_list();
        let reload = self.cached_list();
        let retry = self.a.pos();
        // the VA in rdx
        match addr {
            Addr::Imm(imm) => {
                let off = if mode == 1 { 0 } else { imm as i32 };
                if off == 0 { self.a.mov(true, RDX, base); } else { self.a.lea(true, RDX, m(base, off)); }
            }
            Addr::Reg(_, option, shift) => {
                match idx {
                    None => self.a.mov_imm(RCX, 0),
                    Some(h) => match option { 2 => self.a.mov(false, RCX, h), 6 => self.a.movsxd(RCX, h), _ => self.a.mov(true, RCX, h) },
                }
                if shift <= 3 { self.a.lea(true, RDX, mx(base, RCX, shift as u8, 0)); }
                else { self.a.shift(Sh::Shl, true, RCX, shift as u8); self.a.lea(true, RDX, mx(base, RCX, 0, 0)); }
            }
        }
        let mut slow = Vec::new();
        let (way1, granule) = self.probe_x(cpu.el, if load { 1 } else { 2 }, span, unpriv, !load, &mut slow, true);
        let at = |d: i32| mx(RAMB, RAX, 0, d);
        let regs: Vec<(u32, Option<Reg>, i32)> = if pair { vec![(rt, h1, 0), (rt2, h2, size as i32)] } else { vec![(rt, h1, 0)] };
        for &(r, h, d) in &regs {
            if vec {
                let v = m(CPU, self.o.v + 16 * r as i32);
                let v8 = m(CPU, self.o.v + 16 * r as i32 + 8);
                match (load, size) {
                    (true, 16) => { self.a.movdqu_load(0, at(d)); self.a.movdqu_store(v, 0); }
                    (true, 8) => { self.a.movq_load(0, at(d)); self.a.movdqu_store(v, 0); }
                    (true, 4) => { self.a.movd_load(0, at(d)); self.a.movdqu_store(v, 0); }
                    (true, _) => { self.a.load_zx(size, RCX, at(d)); self.a.store(8, v, RCX); self.a.store_imm(8, v8, 0); }
                    (false, 16) => { self.a.movdqu_load(0, v); self.a.movdqu_store(at(d), 0); }
                    (false, _) => { self.a.load_zx(size, RCX, v); self.a.store(size, at(d), RCX); }
                }
            } else if load {
                // a load into the zero register only probes
                if let Some(h) = h {
                    match signed_to { 0 => self.a.load_zx(size, h, at(d)), 64 => self.a.load_sx(size, true, h, at(d)), _ => self.a.load_sx(size, false, h, at(d)) }
                }
            } else {
                match h { Some(h) => self.a.store(size, at(d), h), None => self.a.store_imm(size, at(d), 0) }
            }
        }
        if load && !vec {
            self.dirty(rt, How::Zr);
            if pair { self.dirty(rt2, How::Zr); }
        }
        // base writeback (never onto a data register: refused above)
        if let (1 | 2, Addr::Imm(imm)) = (mode, &addr) {
            if mode == 1 { self.a.lea(true, base, m(base, *imm as i32)); } else { self.a.mov(true, base, RDX); }
            self.dirty(rn, How::Sp);
        }
        let arg = (if load { 1 } else { 2 }) | (unpriv as u32) << 2 | span << 8;
        let done = self.a.pos();
        self.slow.push(Slow { sites: slow, way1, granule, spill, reload, i, pc, pending, done, retry, arg });
        self.pending += 1;
        true
    }

    /// LDXR/LDAXR, STXR/STLXR (8- to 64-bit), LDAR/STLR and CLREX (see `jit.rs`'s `emit_excl`).
    fn excl(&mut self, cpu: &Cpu, i: u32, pc: u64) -> bool {
        if cpu.ram.is_null() { return false; }
        let o = self.o;
        if i & 0xffff_f0ff == 0xd503_305f {
            // CLREX
            self.a.store_imm(8, m(CPU, o.excl), 0);
            self.pending += 1;
            return true;
        }
        if bits(i, 29, 24) != 0b001000 || bit(i, 21) || bits(i, 14, 10) != 31 { return false; }
        let (sz, o2, load, rs, o0, rn, rt) = (bits(i, 31, 30), bit(i, 23), bit(i, 22), bits(i, 20, 16), bit(i, 15), (i >> 5) & 31, i & 31);
        let size = 1u32 << sz;
        if o2 && (!o0 || rs != 31) { return false; } // LDLAR/STLLR (LORegions): the interpreter
        if !o2 && load && rs != 31 { return false; }
        if !load && cpu.jit.as_ref().is_some_and(|j| j.verify) { return false; }
        self.clobber();
        self.count_pending();
        // works on the registers in `Cpu` (rare enough not to use the cache)
        self.ra_flush();
        self.ra_forget();
        self.a.load(true, RDX, m(CPU, if rn == 31 { self.sp_slot } else { o.x + 8 * rn as i32 }));
        let (mut slow, mut fail) = (Vec::new(), Vec::new());
        if sz > 0 {
            // misaligned: the interpreter raises the Alignment fault every form of these takes
            // (also a store-exclusive without the reservation, so this comes first); with
            // `mt` a host LOCK CMPXCHG or XCHG would otherwise split across cache lines
            self.a.test_imm(false, RDX, size as i32 - 1);
            slow.push(self.a.jcc(NE));
        }
        if !load && !o2 {
            // the reservation must cover this 64-byte granule
            self.a.alu_mem_imm8(Alu::Cmp, 8, m(CPU, o.excl), 0);
            fail.push(self.a.jcc(E));
            self.a.load(true, RAX, m(CPU, o.excl_addr));
            self.a.alu(Alu::Xor, true, RAX, RDX);
            self.a.shift(Sh::Shr, true, RAX, 6);
            fail.push(self.a.jcc(NE));
        }
        self.probe(cpu.el, if load { 1 } else { 2 }, size, false, !load && o2, &mut slow, false);
        let at = mx(RAMB, RAX, 0, 0);
        if load {
            self.a.load_zx(size, RCX, at);
            if rt != 31 { self.a.store(8, m(CPU, o.x + 8 * rt as i32), RCX); }
            if !o2 {
                self.a.store(8, m(CPU, o.excl_addr), RDX);
                self.a.store_imm(8, m(CPU, o.excl), 1);
                self.a.store(8, m(CPU, o.excl_val), RCX);
                self.a.store_imm(8, m(CPU, o.excl_val + 8), 0);
            }
        } else {
            if rt == 31 { self.a.mov_imm(RCX, 0); } else { self.a.load(true, RCX, m(CPU, o.x + 8 * rt as i32)); }
            if o2 {
                if self.mt { self.a.xchg(size, at, RCX); } else { self.a.store(size, at, RCX); }
            } else if self.mt {
                // compare-and-swap against what the load-exclusive read
                self.a.lea(true, RDX, at);
                self.a.load(true, RAX, m(CPU, o.excl_val));
                self.a.lock_cmpxchg(size, m(RDX, 0), RCX);
                fail.push(self.a.jcc(NE));
            } else {
                self.a.store(size, at, RCX);
            }
            if !o2 {
                // stored: the reservation is used up, status 0
                self.a.store_imm(8, m(CPU, o.excl), 0);
                if rs != 31 { self.a.store_imm(8, m(CPU, o.x + 8 * rs as i32), 0); }
            }
        }
        self.a.lea(true, ICOUNT, m(ICOUNT, 1));
        let mut done = vec![self.a.jmp()];
        if !fail.is_empty() {
            // store-exclusive failed: drop the reservation, status 1, nothing stored
            for f in fail { self.a.bind(f); }
            self.a.store_imm(8, m(CPU, o.excl), 0);
            if rs != 31 { self.a.store_imm(8, m(CPU, o.x + 8 * rs as i32), 1); }
            self.a.lea(true, ICOUNT, m(ICOUNT, 1));
            done.push(self.a.jmp());
        }
        for s in slow { self.a.bind(s); }
        let ended = self.interp(i, pc);
        debug_assert!(!ended);
        for d in done { self.a.bind(d); }
        self.fl = Fl::Reg;
        true
    }

    /// DC ZVA, Xt: zero the 64-byte block holding Xt, with a store's micro-TLB fast path.
    fn dc_zva(&mut self, cpu: &Cpu, i: u32, pc: u64) -> bool {
        if cpu.ram.is_null() || cpu.jit.as_ref().is_some_and(|j| j.verify) { return false; }
        let rt = i & 31;
        self.clobber();
        self.count_pending();
        self.ra_flush();
        self.ra_forget();
        if rt == 31 { self.a.mov_imm(RDX, 0); } else { self.a.load(true, RDX, m(CPU, self.o.x + 8 * rt as i32)); }
        self.a.alu_imm(Alu::And, true, RDX, -64);
        let mut slow = Vec::new();
        self.probe(cpu.el, 2, 0, false, true, &mut slow, false);
        self.a.pxor(0, 0);
        for k in 0..4 { self.a.movdqu_store(mx(RAMB, RAX, 0, 16 * k), 0); }
        self.a.lea(true, ICOUNT, m(ICOUNT, 1));
        let done = self.a.jmp();
        for s in slow { self.a.bind(s); }
        let ended = self.interp(i, pc);
        debug_assert!(!ended);
        self.a.bind(done);
        self.fl = Fl::Reg;
        true
    }

    /// The side exits after the block's code.
    fn emit_side_exits(&mut self) {
        debug_assert_eq!(self.pending, 0);
        for se in std::mem::take(&mut self.sides) {
            self.a.bind(se.at);
            if se.save { save_flags(&mut self.a); }
            self.fl = Fl::Reg;
            self.spill(&se.spill);
            self.exit_const(se.target);
        }
    }

    /// The load/store slow paths after the block's code.
    fn emit_slow_paths(&mut self) {
        for sp in std::mem::take(&mut self.slow) {
            let mut sites = sp.sites.clone();
            if let Some((site, back, ent)) = sp.way1 {
                // way 1: on a hit, continue the fast path with rcx at that entry
                self.a.bind(site);
                self.a.alu_load(Alu::Cmp, true, RAX, mx(CPU, RCX, 0, ent + 16));
                sites.push(self.a.jcc(NE));
                self.a.alu_imm(Alu::Add, false, RCX, 16);
                self.a.jmp_to(back);
            }
            if let Some((site, back)) = sp.granule {
                // a reservation is held: a store to another granule leaves it alone. Both the
                // first and the last byte are checked: a pair is two stores in the interpreter,
                // and either can clear the reservation (an access spans two granules at most)
                self.a.bind(site);
                let span = (sp.arg >> 8) as i32;
                let bytes: &[i32] = if span > 1 { &[0, span - 1] } else { &[0] };
                for &b in bytes {
                    if b == 0 { self.a.mov(true, RAX, RDX); } else { self.a.lea(true, RAX, m(RDX, b)); }
                    self.a.alu_load(Alu::Xor, true, RAX, m(CPU, self.o.excl_addr));
                    self.a.shift(Sh::Shr, true, RAX, 6);
                    sites.push(self.a.jcc(E));
                }
                self.a.jmp_to(back);
            }
            for s in &sites { self.a.bind(*s); }
            self.fl = Fl::Reg;
            // a micro-TLB refill (the stub keeps every cache register), then the access again
            self.a.mov_imm(RAX, sp.arg as u64);
            self.call_abs(self.st.refill);
            self.a.test(false, RAX, RAX);
            self.a.jcc_to(NE, sp.retry);
            // the interpreter
            self.spill(&sp.spill);
            self.pending = sp.pending;
            let ended = self.interp(sp.i, sp.pc);
            debug_assert!(!ended);
            // instret now includes this instruction and the `pending` before it, which the fast
            // path counts at compile time after the join: take them off again
            self.a.lea(true, ICOUNT, m(ICOUNT, -(sp.pending as i32 + 1)));
            self.reload(&sp.reload);
            self.a.jmp_to(sp.done);
        }
        self.pending = 0;
    }
}

/// DUP (general), UMOV and SMOV: (kind 0 DUP / 1 UMOV / 2 SMOV, element size log2, index).
/// Only decoded here: `interp_supports` decides whether the encoding is allocated.
fn simd_gp(i: u32) -> Option<(u32, u32, u32)> {
    let kind = match i & 0xbfe0_fc00 { 0x0e00_0c00 => 0, 0x0e00_3c00 => 1, 0x0e00_2c00 => 2, _ => return None };
    let imm5 = bits(i, 20, 16);
    if imm5 & 0xf == 0 { return None; }
    let size = imm5.trailing_zeros();
    // SMOV: bytes and halfwords to W or X, words only to X (the interpreter takes some
    // reserved forms, which are left to it)
    if kind == 2 && (size > 2 || (size == 2 && !bit(i, 30))) { return None; }
    Some((kind, size, imm5 >> (size + 1)))
}

/// MOVI, MVNI and FMOV (vector, immediate): Vd from the immediate alone (not ORR/BIC, which
/// also read it).
fn movi_const(i: u32) -> bool {
    !bit(i, 31) && bits(i, 28, 19) == 0b0111100000 && bit(i, 10) && { let cmode = bits(i, 15, 12); cmode & 1 == 0 || cmode >= 12 }
}

/// The byte-vector NEON operations glibc's string functions use, done with SSE2 on the V
/// registers in `Cpu`: CMEQ (register, zero), CMHS (bytes), AND/BIC/ORR/ORN/EOR/BSL/BIT/BIF,
/// UMAXP/UMINP (16 bytes) and SHRN/SHRN2 (halfwords to bytes). None = not one of them.
#[derive(Clone, Copy, PartialEq, Debug)]
enum Neon { CmeqReg(u32), CmeqZero(u32), CmhsB, Logic(u32), MaxMinP(bool), Shrn(u8) }

fn neon_op(i: u32) -> Option<Neon> {
    let (q, size) = (bit(i, 30), bits(i, 23, 22));
    if bit(i, 31) { return None; }
    if i & 0x9f20_0400 == 0x0e20_0400 {
        // three registers of the same type
        return match (bit(i, 29), bits(i, 15, 11), size) {
            (true, 0b10001, 0..=2) => Some(Neon::CmeqReg(size)),
            (true, 0b00111, 0) => Some(Neon::CmhsB),
            (u, 0b00011, _) => Some(Neon::Logic((u as u32) << 2 | size)),
            (true, 0b10100, 0) if q => Some(Neon::MaxMinP(true)),
            (true, 0b10101, 0) if q => Some(Neon::MaxMinP(false)),
            _ => None,
        };
    }
    if i & 0xbf3f_fc00 == 0x0e20_9800 && size <= 2 { return Some(Neon::CmeqZero(size)); }
    if i & 0xbf78_fc00 == 0x0f08_8400 { return Some(Neon::Shrn((16 - bits(i, 22, 16)) as u8)); }
    None
}

impl E<'_> {
    fn neon(&mut self, i: u32, op: Neon) {
        let (q, rd, rn, rm) = (bit(i, 30), i & 31, (i >> 5) & 31, (i >> 16) & 31);
        let v = |r: u32| m(CPU, self.o.v + 16 * r as i32);
        let (vd, vn, vm) = (v(rd), v(rn), v(rm));
        // xmm3 = 0x00ff in every halfword
        let low_bytes = |a: &mut Asm| { a.sse(0x76, 3, 3); a.psrlw(3, 8); };
        self.a.movdqu_load(0, vn);
        let mut res = 0;
        match op {
            Neon::CmeqReg(size) => { self.a.movdqu_load(1, vm); self.a.sse(0x74 + size as u8, 0, 1); }
            Neon::CmeqZero(size) => { self.a.pxor(1, 1); self.a.sse(0x74 + size as u8, 0, 1); }
            Neon::CmhsB => {
                // n >= m (unsigned) exactly where max(n, m) == n
                self.a.movdqu_load(1, vm);
                self.a.sse(0x6f, 2, 0);
                self.a.sse(0xde, 2, 1);
                self.a.sse(0x74, 0, 2);
            }
            Neon::Logic(k) => {
                self.a.movdqu_load(1, vm);
                match k {
                    0 => self.a.sse(0xdb, 0, 1), // AND
                    1 => { self.a.sse(0xdf, 1, 0); res = 1; } // BIC: ~m & n
                    2 => self.a.sse(0xeb, 0, 1), // ORR
                    3 => { self.a.sse(0x76, 2, 2); self.a.pxor(1, 2); self.a.sse(0xeb, 0, 1); } // ORN
                    4 => self.a.pxor(0, 1), // EOR
                    5 => { self.a.movdqu_load(2, vd); self.a.pxor(0, 1); self.a.sse(0xdb, 0, 2); self.a.pxor(0, 1); } // BSL: m ^ ((m ^ n) & d)
                    6 => { self.a.movdqu_load(2, vd); self.a.pxor(0, 2); self.a.sse(0xdb, 0, 1); self.a.pxor(0, 2); } // BIT: d ^ ((d ^ n) & m)
                    _ => { self.a.movdqu_load(2, vd); self.a.pxor(0, 2); self.a.sse(0xdf, 1, 0); self.a.pxor(1, 2); res = 1; } // BIF: d ^ (~m & (d ^ n))
                }
            }
            Neon::MaxMinP(max) => {
                // each source: max/min of its even and odd bytes into halfwords, then packed
                self.a.movdqu_load(1, vm);
                low_bytes(&mut self.a);
                for x in [0u8, 1] {
                    self.a.sse(0x6f, 2, x);
                    self.a.psrlw(2, 8);
                    self.a.sse(0xdb, x, 3);
                    self.a.sse(if max { 0xde } else { 0xda }, x, 2);
                }
                self.a.sse(0x67, 0, 1);
            }
            Neon::Shrn(sh) => {
                self.a.psrlw(0, sh);
                low_bytes(&mut self.a);
                self.a.sse(0xdb, 0, 3);
                self.a.sse(0x67, 0, 0);
                // SHRN2 writes the upper half and keeps the lower
                if q { self.a.movq_store(m(CPU, vd.disp + 8), 0); } else { self.a.movq_store(vd, 0); self.a.store_imm(8, m(CPU, vd.disp + 8), 0); }
                return;
            }
        }
        if q { self.a.movdqu_store(vd, res); } else { self.a.movq_store(vd, res); self.a.store_imm(8, m(CPU, vd.disp + 8), 0); }
    }
}

impl Jit {
    /// The value an instruction that writes Vd from its immediate alone leaves there, from
    /// running it once on the scratch core (None: the interpreter does not take it).
    fn simd_const(&mut self, i: u32) -> Option<u128> {
        let (c, ram) = &mut **self.scratch.get_or_insert_with(|| Box::new((Cpu::new(), emu_core::FlatRam::new(0, 16))));
        let _ = ram.load(0, &i.to_le_bytes());
        c.el = 1;
        c.pc = 0;
        c.sys.cpacr_el1 = 3 << 20;
        c.vbar[1] = 0x800;
        c.v = [0; 32];
        (matches!(c.exec_one(ram), Ok(())) && c.pc == 4).then(|| c.v[(i & 31) as usize])
    }
}

impl E<'_> {
    /// DUP Vd.<T>, Rn / UMOV, SMOV Rd, Vn.<T>[i] (the V registers stay in `Cpu`).
    fn simd_gp(&mut self, i: u32) {
        let (kind, size, idx) = simd_gp(i).expect("decoded");
        let (rd, rn, q) = (i & 31, (i >> 5) & 31, bit(i, 30));
        if kind == 0 {
            let h = self.get(rn, How::Zr);
            self.clobber();
            match (h, size) {
                (None, _) => self.a.mov_imm(RAX, 0),
                (Some(h), 0) => self.a.movzx8(RAX, h),
                (Some(h), 1) => self.a.movzx16(RAX, h),
                (Some(h), 2) => self.a.mov(false, RAX, h),
                (Some(h), _) => self.a.mov(true, RAX, h),
            }
            if size < 3 {
                self.a.mov_imm(RCX, [0x0101_0101_0101_0101, 0x0001_0001_0001_0001, 0x0000_0001_0000_0001][size as usize]);
                self.a.imul(true, RAX, RCX);
            }
            let v = self.o.v + 16 * rd as i32;
            self.a.store(8, m(CPU, v), RAX);
            if q { self.a.store(8, m(CPU, v + 8), RAX); } else { self.a.store_imm(8, m(CPU, v + 8), 0); }
        } else if let Some(d) = self.def(rd, How::Zr) {
            let at = m(CPU, self.o.v + 16 * rn as i32 + (idx << size) as i32);
            if kind == 1 { self.a.load_zx(1 << size, d, at); } else { self.a.load_sx(1 << size, q, d, at); }
            self.dirty(rd, How::Zr);
        }
    }
}

// ---------------------------------------------------------------------- compiler
impl Cpu {
    /// Compile the block at `pa` (the current pc) to x86-64. `sb`: superblocks.
    pub(super) fn compile_x64(&mut self, pa: u64, sb: bool) -> Option<BlockFn> {
        if self.ram.is_null() || pa + 4 > self.ram_len { return None; }
        let o = off();
        if self.jit.as_ref()?.disp.is_none() {
            // the shared stubs, once per code buffer
            let j = self.jit.as_mut()?;
            let base = j.code.next_addr() as u64;
            let (bytes, s) = stubs(&o, j.front.as_ptr() as u64, base);
            match j.code.write_bytes(&bytes) {
                Some(p) => {
                    debug_assert_eq!(p as u64, base);
                    super::code_map(p as usize, bytes.len(), "dispatcher");
                    (j.disp, j.exit, j.enter, j.interp, j.refill) = (Some(s.disp), s.exit, s.enter, s.interp, s.refill);
                }
                None => { j.full_flushes += 1; j.flush(); return None; }
            }
        }
        let j = self.jit.as_ref()?;
        let st = Stubs { disp: j.disp?, exit: j.exit, interp: j.interp, refill: j.refill };
        let chain = (!j.verify && super::chain_enabled()).then(|| key(0, self.el, self.spsel, (self.sys.cpacr_el1 >> 20) & 3));
        let sp_slot = o.sp + 8 * if self.spsel { self.el as i32 } else { 0 };
        let mut e = E { a: Asm::default(), o: &o, sp_slot, pending: 0, chain, page: self.pc >> 12, el: self.el as u64, fetch: Box::new(self.utlb[0]),
                        links: Vec::new(), relocs: Vec::new(), ra: [Slot::default(); CACHE.len()], tick: 0, fl: Fl::Reg, slow: Vec::new(),
                        sides: Vec::new(), sb, st, mt: self.mt };
        let fp_on = (self.sys.cpacr_el1 >> 20) & 3 == 3;
        static MASK: std::sync::OnceLock<u32> = std::sync::OnceLock::new();
        // debugging: SBC_JIT_NATIVE bits select what is translated (1 data processing,
        // 2 branches, 4 other integer, 8 loads/stores, 16 exclusives, 32 NEON); the rest is interpreted
        let mask = *MASK.get_or_init(|| std::env::var("SBC_JIT_NATIVE").ok().and_then(|v| v.parse().ok()).unwrap_or(63));
        let mut pc = self.pc;
        let mut a = pa;
        let mut ended = false;
        for _ in 0..MAX_INSNS {
            // SAFETY: `a` is inside guest RAM (checked) and blocks stop at the page end
            let i = unsafe { (self.ram.add(a as usize) as *const u32).read_unaligned() };
            if mask & 1 != 0 && native_operands(i).is_some() && e.dp(i) {
                e.pending += 1;
            } else if mask & 4 != 0 && bits(i, 28, 24) == 0b10000 {
                // ADR / ADRP: a constant
                let imm = (((((bits(i, 23, 5) << 2) | bits(i, 30, 29)) as u64) << 43) as i64 >> 43) as u64;
                let v = if bit(i, 31) { (pc & !0xfff).wrapping_add(imm << 12) } else { pc.wrapping_add(imm) };
                if let Some(d) = e.def(i & 31, How::Zr) { e.a.mov_imm(d, v); e.dirty(i & 31, How::Zr); }
                e.pending += 1;
            } else if mask & 4 != 0 && ((i & 0xffff_f01f == 0xd503_201f && !matches!((i >> 5) & 0x7f, 2 | 3))
                || (i & 0xffff_f01f == 0xd503_301f && matches!((i >> 5) & 7, 4..=6)))
            {
                // NOP and the hints that are NOPs here, DSB/DMB/ISB: with cores on parallel host
                // threads a full DMB/DSB orders stores before loads too (the rest TSO gives)
                if self.mt && i & 0xffff_f01f == 0xd503_301f && matches!((i >> 5) & 7, 4 | 5) && (i >> 8) & 3 == 3 { e.a.mfence(); }
                e.pending += 1;
            } else if mask & 4 != 0 && (i & 0xffc0_0000 == 0xf980_0000 || i & 0xffe0_0c00 == 0xf880_0000 || i & 0xffe0_0c00 == 0xf8a0_0800) {
                // PRFM / PRFUM: no caches are modelled
                e.pending += 1;
            } else if let Some(n) = match i & 0xffff_ffe0 { 0xd53b_d040 => Some(0), 0xd53b_d060 => Some(1), 0xd538_d080 if self.el >= 1 => Some(2), _ => None }.filter(|_| mask & 4 != 0) {
                // MRS Xt, TPIDR_EL0 / TPIDRRO_EL0 / TPIDR_EL1
                if let Some(d) = e.def(i & 31, How::Zr) { e.a.load(true, d, m(CPU, o.tpidr + 8 * n)); e.dirty(i & 31, How::Zr); }
                e.pending += 1;
            } else if mask & 4 != 0 && i & 0xffff_ffe0 == 0xd538_4100 && self.el >= 1 {
                // MRS Xt, SP_EL0
                let rt = i & 31;
                if rt != 31 {
                    if e.sp_slot == o.sp {
                        let s = e.get(31, How::Sp).unwrap();
                        let d = e.def(rt, How::Zr).unwrap();
                        e.a.mov(true, d, s);
                    } else {
                        let d = e.def(rt, How::Zr).unwrap();
                        e.a.load(true, d, m(CPU, o.sp));
                    }
                    e.dirty(rt, How::Zr);
                }
                e.pending += 1;
            } else if mask & 4 != 0 && fp_on && matches!(i & 0xffff_fc00, 0x9e66_0000 | 0x1e26_0000 | 0x9e67_0000 | 0x1e27_0000) {
                // FMOV between the register files (64-bit X<->D, 32-bit W<->S); the V
                // registers stay in `Cpu`
                let (rd, rn, w) = (i & 31, (i >> 5) & 31, i >> 31 != 0);
                if i & 0x0001_0000 == 0 {
                    if let Some(d) = e.def(rd, How::Zr) { e.a.load(w, d, m(CPU, o.v + 16 * rn as i32)); e.dirty(rd, How::Zr); }
                } else {
                    let v = o.v + 16 * rd as i32;
                    match e.get(rn, How::Zr) {
                        Some(h) => e.a.store(if w { 8 } else { 4 }, m(CPU, v), h),
                        None => e.a.store_imm(8, m(CPU, v), 0),
                    }
                    if !w { e.a.store_imm(4, m(CPU, v + 4), 0); }
                    e.a.store_imm(8, m(CPU, v + 8), 0);
                }
                e.pending += 1;
            } else if mask & 4 != 0 && self.el >= 1 && i & 0xffff_ffe0 == 0xd53b_4220 {
                // MRS Xt, DAIF
                if let Some(d) = e.def(i & 31, How::Zr) { e.a.load(false, d, m(CPU, o.daif)); e.dirty(i & 31, How::Zr); }
                e.pending += 1;
            } else if mask & 4 != 0 && fp_on && simd_gp(i).is_some() && self.jit.as_mut().is_some_and(|j| j.interp_supports(i)) {
                // DUP (general), UMOV, SMOV: between general and vector registers
                e.simd_gp(i);
                e.pending += 1;
            } else if let Some(op) = neon_op(i).filter(|_| mask & 32 != 0 && fp_on && self.jit.as_mut().is_some_and(|j| j.interp_supports(i))) {
                // byte-vector NEON (string functions)
                e.neon(i, op);
                e.pending += 1;
            } else if let Some(v) = if mask & 4 != 0 && fp_on && movi_const(i) { self.jit.as_mut().and_then(|j| j.simd_const(i)) } else { None } {
                // MOVI / MVNI / FMOV (vector, immediate): a constant
                let at = o.v + 16 * (i & 31) as i32;
                for (k, half) in [v as u64, (v >> 64) as u64].into_iter().enumerate() {
                    if half as i64 == half as i32 as i64 { e.a.store_imm(8, m(CPU, at + 8 * k as i32), half as i32); }
                    else { e.a.mov_imm(RAX, half); e.a.store(8, m(CPU, at + 8 * k as i32), RAX); }
                }
                e.pending += 1;
            } else if let Some(done) = if mask & 2 != 0 { e.branch(i, pc) } else { None } {
                if done { ended = true; break; }
            } else if mask & 16 != 0 && e.excl(self, i, pc) {
                // exclusive and acquire/release accesses, CLREX
            } else if mask & 8 != 0 && i & 0xffff_ffe0 == 0xd50b_7420 && e.dc_zva(self, i, pc) {
                // DC ZVA
            } else if mask & 8 != 0 && super::ld1_as_ldr(i).is_some_and(|w| e.ldst(self, w, pc)) {
                // LD1/ST1 of one register: the same access as LDR/STR of a Q or D register
            } else if mask & 8 != 0 && e.ldst(self, i, pc) {
                // native load/store with an interpreter slow path
            } else {
                // the interpreter reads and may write any register in `Cpu`
                e.ra_flush();
                let end = e.interp(i, pc);
                e.ra_forget();
                if end { ended = true; break; }
            }
            e.unpin();
            pc += 4;
            a += 4;
            if a & 0xfff == 0 { break; } // never cross a page
        }
        if !ended {
            // fell off the end of the block: continue at pc
            e.ra_flush();
            e.exit_const(pc);
        }
        e.emit_side_exits();
        e.emit_slow_paths();
        let j = self.jit.as_mut()?;
        let base = j.code.next_addr();
        for &(at, to) in &e.relocs {
            let v = to as i64 - (base as i64 + at as i64 + 4);
            e.a.b[at..at + 4].copy_from_slice(&i32::try_from(v).expect("rel32 reach").to_le_bytes());
        }
        let Some(f) = j.code.write_bytes(&e.a.b) else { j.full_flushes += 1; j.flush(); return None; };
        debug_assert_eq!(f as usize, base);
        let n = (pc - self.pc) / 4 + ended as u64;
        super::code_map(f as usize, e.a.b.len(), &format!("{:x} {}", self.pc, n));
        j.new_sites = e.links.iter().map(|&(at, t, tpa)| (f as usize + at, t, tpa, 0)).collect();
        // SAFETY: the buffer now holds a complete block entered by the enter stub or a jump
        Some(unsafe { std::mem::transmute::<*const u8, BlockFn>(f) })
    }
}

/// Addresses of the shared stubs.
struct StubAddrs { disp: u64, exit: u64, enter: u64, interp: u64, refill: u64 }

/// The shared stubs, for the code buffer at `base`.
///
/// Dispatcher: entered by a jump from a block's exit with pc in `Cpu`, the flags in r15, r14 =
/// instret and rdx = the mode bits of the block key. Jumps to the next compiled block, or
/// stores instret and returns 0 to Rust.
///
/// Exit: rax = status; returns (status, r15) to Rust, restoring the registers enter saved.
///
/// Enter, `fn(cpu, block, flags) -> Ret`: saves the callee-saved registers, sets rbx = cpu,
/// r13 = guest RAM, r14 = instret, r15 = flags and jumps to the block.
///
/// Interpreter (called from a block; rsi = the instruction's pc + 4, rdx = mode bits or -1
/// without chaining): instret and flags to `Cpu`, `h_step`, then status 0: return; 1: the
/// dispatcher (back to Rust without chaining); 2 (a trap): back to Rust with status 1.
///
/// Refill (called from a load/store slow path; rdx = VA, eax = `h_tlb`'s argument): keeps
/// every cache register; eax = `h_tlb`'s result.
fn stubs(o: &Off, front: u64, base: u64) -> (Vec<u8>, StubAddrs) {
    let probe = (u64::MAX, u64::MAX, 0usize);
    let pb = &probe as *const _ as usize;
    let (o_pc, o_key, o_f) = ((&probe.0 as *const _ as usize - pb) as i32, (&probe.1 as *const _ as usize - pb) as i32, (&probe.2 as *const _ as usize - pb) as i32);
    assert_eq!(std::mem::size_of::<(u64, u64, usize)>(), 24);
    let mut a = Asm::default();
    let mut out = Vec::new();
    // ---- dispatcher
    let disp = a.pos();
    a.alu_load(Alu::Cmp, true, ICOUNT, m(CPU, o.chain_until));
    out.push(a.jcc(AE));
    a.alu_mem_imm8(Alu::Cmp, 1, m(CPU, o.chain_stop), 0);
    out.push(a.jcc(NE));
    a.load(true, RCX, m(CPU, o.pc));
    a.test_imm(false, RCX, 3);
    out.push(a.jcc(NE));
    // fetch micro-TLB set: rax = its offset from entry 0, rsi = tag = pc >> 12 | el << 60
    a.mov(false, RAX, RCX);
    a.shift(Sh::Shr, false, RAX, 7);
    a.alu_imm(Alu::And, false, RAX, ((UT / 2 - 1) << 5) as i32);
    a.mov(true, RSI, RCX);
    a.shift(Sh::Shr, true, RSI, 12);
    a.mov(true, RDI, RDX);
    a.shift(Sh::Shr, true, RDI, 56);
    a.alu_imm(Alu::And, false, RDI, 3);
    a.shift(Sh::Shl, true, RDI, 60);
    a.alu(Alu::Or, true, RSI, RDI);
    a.lea(true, RDI, mx(CPU, RAX, 0, o.utlb));
    a.alu_load(Alu::Cmp, true, RSI, m(RDI, 0));
    let hit = a.jcc(E);
    a.alu_imm(Alu::Add, true, RDI, 16);
    a.alu_load(Alu::Cmp, true, RSI, m(RDI, 0));
    out.push(a.jcc(NE));
    a.bind(hit);
    // key = physical pc | mode bits
    a.load(true, RSI, m(RDI, 8));
    a.mov(false, RAX, RCX);
    a.alu_imm(Alu::And, false, RAX, 0xffc);
    a.alu(Alu::Or, true, RSI, RAX);
    a.alu(Alu::Or, true, RSI, RDX);
    // front slot = ((pc ^ pc >> 14) >> 2) % FRONT
    a.mov(true, RAX, RCX);
    a.shift(Sh::Shr, true, RAX, 14);
    a.alu(Alu::Xor, true, RAX, RCX);
    a.shift(Sh::Shr, true, RAX, 2);
    a.alu_imm(Alu::And, false, RAX, (super::FRONT - 1) as i32);
    a.lea(true, RAX, mx(RAX, RAX, 1, 0));
    a.mov_imm(RDI, front);
    a.lea(true, RDI, mx(RDI, RAX, 3, 0));
    a.alu_load(Alu::Cmp, true, RCX, m(RDI, o_pc));
    out.push(a.jcc(NE));
    a.alu_load(Alu::Cmp, true, RSI, m(RDI, o_key));
    out.push(a.jcc(NE));
    a.jmp_mem(m(RDI, o_f));
    // out: back to Rust with the guest state in `Cpu`
    let out_at = a.pos();
    for p in out { a.bind(p); }
    a.store(8, m(CPU, o.instret), ICOUNT);
    a.mov_imm(RAX, 0);
    // ---- exit
    let exit = a.pos();
    a.mov(true, RDX, FLR);
    a.alu_imm(Alu::Add, true, RSP, 8);
    for r in [R15, R14, R13, R12, RBX, RBP] { a.pop(r); }
    a.ret();
    // ---- enter
    let enter = a.pos();
    for r in [RBP, RBX, R12, R13, R14, R15] { a.push(r); }
    a.alu_imm(Alu::Sub, true, RSP, 8);
    a.mov(true, CPU, RDI);
    a.load(true, ICOUNT, m(CPU, o.instret));
    a.load(true, RAMB, m(CPU, o.ram));
    a.mov(true, FLR, RDX);
    a.jmp_reg(RSI);
    // ---- interpreter
    let interp = a.pos();
    a.store(8, m(CPU, o.instret), ICOUNT);
    a.push(RDX);
    a.mov(true, RDI, CPU);
    a.mov(false, RDX, FLR);
    a.mov_imm(RAX, h_step_x64 as *const () as usize as u64);
    a.call_reg(RAX);
    a.pop(RDX);
    a.load(true, ICOUNT, m(CPU, o.instret));
    a.mov(true, FLR, RAX);
    a.shift(Sh::Shr, true, FLR, 32);
    a.test(false, RAX, RAX);
    let nz = a.jcc(NE);
    a.ret();
    a.bind(nz);
    a.alu_imm(Alu::Add, true, RSP, 8); // the block's return address
    a.alu_imm(Alu::Cmp, false, RAX, 2);
    let trap = a.jcc(E);
    a.alu_imm(Alu::Cmp, true, RDX, -1);
    a.jcc_to(E, out_at);
    a.jmp_to(disp);
    a.bind(trap);
    a.mov_imm(RAX, 1);
    a.jmp_to(exit);
    // ---- micro-TLB refill
    let refill = a.pos();
    for r in [RSI, RDI, R8, R9, R10, R11] { a.push(r); }
    a.alu_imm(Alu::Sub, true, RSP, 8);
    a.mov(true, RDI, CPU);
    a.mov(true, RSI, RDX);
    a.mov(false, RDX, RAX);
    a.mov_imm(RAX, super::h_tlb as *const () as usize as u64);
    a.call_reg(RAX);
    a.alu_imm(Alu::Add, true, RSP, 8);
    for r in [R11, R10, R9, R8, RDI, RSI] { a.pop(r); }
    a.ret();
    let at = |p: usize| base + p as u64;
    (a.b, StubAddrs { disp: at(disp), exit: at(exit), enter: at(enter), interp: at(interp), refill: at(refill) })
}

#[cfg(test)]
mod tests {
    use super::asm::*;
    use super::{fl_to_nzcv, nzcv_to_fl};
    use std::io::Write;
    use std::process::{Command, Stdio};

    #[test]
    fn branch_encodings() {
        // rel32 branches (llvm-mc's Intel syntax wants labels for these): opcode, then the
        // displacement from the end of the instruction
        for cc in 0..16u8 {
            let mut a = Asm::default();
            let p = a.jcc(cc);
            a.patch(p, p + 4 + 0x100);
            assert_eq!(a.b, [0x0f, 0x80 | cc, 0x00, 0x01, 0, 0]);
        }
        let mut a = Asm::default();
        let p = a.jmp();
        a.bind(p);
        let q = a.call();
        a.patch(q, 0);
        assert_eq!(a.b, [0xe9, 0, 0, 0, 0, 0xe8, 0xf6, 0xff, 0xff, 0xff]);
    }

    #[test]
    fn flag_forms_round_trip() {
        for v in 0..16u32 { assert_eq!(fl_to_nzcv(nzcv_to_fl(v << 28)), v << 28); }
    }

    fn llvm_mc() -> Option<String> {
        ["llvm-mc", "llvm-mc-18", "llvm-mc-19", "llvm-mc-17", "llvm-mc-20"].into_iter().map(String::from)
            .find(|c| Command::new(c).arg("--version").output().is_ok_and(|o| o.status.success()))
    }

    fn run_mc(mc: &str, args: &[&str], input: &str) -> String {
        let mut p = Command::new(mc).args(args).stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::piped()).spawn().unwrap();
        p.stdin.take().unwrap().write_all(input.as_bytes()).unwrap();
        let o = p.wait_with_output().unwrap();
        assert!(o.status.success(), "llvm-mc: {}", String::from_utf8_lossy(&o.stderr));
        String::from_utf8(o.stdout).unwrap()
    }

    /// Disassembly of each byte sequence (one instruction each), normalized.
    fn disasm(mc: &str, seqs: &[Vec<u8>]) -> Vec<String> {
        let input: String = seqs.iter().map(|s| s.iter().map(|b| format!("0x{b:02x} ")).collect::<String>() + "\n").collect();
        let out = run_mc(mc, &["--disassemble", "-triple=x86_64", "-output-asm-variant=1"], &input);
        let mut lines: Vec<String> = Vec::new();
        let mut prefix = String::new();
        for l in out.lines().map(|l| l.split('#').next().unwrap().split_whitespace().collect::<Vec<_>>().join(" ")).filter(|l| !l.is_empty() && !l.starts_with('.')) {
            // a prefix the disassembler prints on a line of its own
            if l == "lock" { prefix = l + " "; continue; }
            lines.push(std::mem::take(&mut prefix) + &l);
        }
        assert_eq!(lines.len(), seqs.len(), "disassembly:\n{out}");
        lines
    }

    const REGS: [&str; 16] = ["rax", "rcx", "rdx", "rbx", "rsp", "rbp", "rsi", "rdi", "r8", "r9", "r10", "r11", "r12", "r13", "r14", "r15"];
    fn r32(r: Reg) -> String { if r.0 < 8 { format!("e{}", &REGS[r.0 as usize][1..]) } else { format!("{}d", REGS[r.0 as usize]) } }
    fn r16(r: Reg) -> String { if r.0 < 8 { REGS[r.0 as usize][1..].to_string() } else { format!("{}w", REGS[r.0 as usize]) } }
    fn r8(r: Reg) -> String { ["al", "cl", "dl", "bl", "spl", "bpl", "sil", "dil"].get(r.0 as usize).map_or(format!("{}b", REGS[r.0 as usize]), |s| s.to_string()) }
    fn rw(w: bool, r: Reg) -> String { if w { REGS[r.0 as usize].to_string() } else { r32(r) } }
    fn mem(mm: Mem) -> String {
        let mut s = format!("[{}", REGS[mm.base.0 as usize]);
        if let Some((i, sc)) = mm.index { s += &format!(" + {}*{}", REGS[i.0 as usize], 1 << sc); }
        if mm.disp != 0 { s += &format!(" + {}", mm.disp); }
        s + "]"
    }
    fn ptr(size: u32) -> &'static str { match size { 1 => "byte ptr", 2 => "word ptr", 4 => "dword ptr", 8 => "qword ptr", _ => "xmmword ptr" } }

    /// Every encoder form, over registers and operands that exercise REX, SIB and displacement
    /// edge cases, against llvm-mc (assembled, and compared as disassembly when llvm-mc picks
    /// another valid encoding).
    #[test]
    fn encodings_match_llvm_mc() {
        let Some(mc) = llvm_mc() else {
            // CI must run it (GitHub's ubuntu-24.04 image has llvm-mc-18)
            assert!(std::env::var_os("CI").is_none(), "llvm-mc not found (CI is set): the encoder test must run");
            eprintln!("llvm-mc not found: encoder test skipped");
            return;
        };
        let regs = [RAX, RCX, RDX, RBX, RBP, RSI, RDI, R8, R12, R13, R15];
        let mems = [m(RBX, 0), m(RBX, 8), m(RBX, -128), m(RBX, 4096), m(RBP, 0), m(R13, 0), m(R12, 0), m(RSP, 16), m(R12, 300), mx(RBX, RCX, 0, 0x1000), mx(R13, RAX, 0, 8), mx(RBP, R12, 3, 0), mx(RAX, RAX, 1, 0), mx(R12, R9, 2, -8)];
        let alus = [(Alu::Add, "add"), (Alu::Or, "or"), (Alu::Adc, "adc"), (Alu::Sbb, "sbb"), (Alu::And, "and"), (Alu::Sub, "sub"), (Alu::Xor, "xor"), (Alu::Cmp, "cmp")];
        let shs = [(Sh::Rol, "rol"), (Sh::Ror, "ror"), (Sh::Shl, "shl"), (Sh::Shr, "shr"), (Sh::Sar, "sar")];
        let ccs = ["o", "no", "b", "ae", "e", "ne", "be", "a", "s", "ns", "p", "np", "l", "ge", "le", "g"];
        let mut cases: Vec<(Vec<u8>, String)> = Vec::new();
        let mut c = |f: &dyn Fn(&mut Asm), s: String| { let mut a = Asm::default(); f(&mut a); cases.push((a.b, s)); };
        for &w in &[true, false] {
            for &d in &regs {
                for &s in &regs {
                    c(&|a| a.mov(w, d, s), format!("mov {}, {}", rw(w, d), rw(w, s)));
                    for &(op, n) in &alus { c(&|a| a.alu(op, w, d, s), format!("{n} {}, {}", rw(w, d), rw(w, s))); }
                    c(&|a| a.test(w, d, s), format!("test {}, {}", rw(w, d), rw(w, s)));
                    c(&|a| a.imul(w, d, s), format!("imul {}, {}", rw(w, d), rw(w, s)));
                    c(&|a| a.cmov(0xd, w, d, s), format!("cmovge {}, {}", rw(w, d), rw(w, s)));
                    c(&|a| a.shrd(w, d, s, 13), format!("shrd {}, {}, 13", rw(w, d), rw(w, s)));
                    c(&|a| a.bsr(w, d, s), format!("bsr {}, {}", rw(w, d), rw(w, s)));
                    c(&|a| a.movsx8(w, d, s), format!("movsx {}, {}", rw(w, d), r8(s)));
                    c(&|a| a.movsx16(w, d, s), format!("movsx {}, {}", rw(w, d), r16(s)));
                }
                for &v in &[0i32, 1, -1, 127, -128, 128, 0xfff, 0xfff000, -0x8000_0000] {
                    for &(op, n) in &alus { c(&|a| a.alu_imm(op, w, d, v), format!("{n} {}, {v}", rw(w, d))); }
                }
                c(&|a| a.test_imm(w, d, 3), format!("test {}, 3", rw(w, d)));
                for &(k, n) in &shs { c(&|a| a.shift(k, w, d, 7), format!("{n} {}, 7", rw(w, d))); c(&|a| a.shift_cl(k, w, d), format!("{n} {}, cl", rw(w, d))); }
                for (k, n) in [(2u8, "not"), (3, "neg"), (4, "mul"), (5, "imul"), (6, "div"), (7, "idiv")] { c(&|a| a.grp3(k, w, d), format!("{n} {}", rw(w, d))); }
                c(&|a| a.bswap(w, d), format!("bswap {}", rw(w, d)));
                c(&|a| a.bt(w, d, 37 % if w { 64 } else { 32 }), format!("bt {}, {}", rw(w, d), 37 % if w { 64 } else { 32 }));
                c(&|a| a.bts(w, d, 5), format!("bts {}, 5", rw(w, d)));
                for &mm in &mems {
                    c(&|a| a.load(w, d, mm), format!("mov {}, qword ptr {}", rw(w, d), mem(mm)).replace(if w { "@" } else { "qword" }, "dword"));
                    c(&|a| a.lea(w, d, mm), format!("lea {}, {}", rw(w, d), mem(mm)));
                    c(&|a| a.alu_load(Alu::Cmp, w, d, mm), format!("cmp {}, {} {}", rw(w, d), if w { "qword ptr" } else { "dword ptr" }, mem(mm)));
                    c(&|a| a.alu_load(Alu::Or, w, d, mm), format!("or {}, {} {}", rw(w, d), if w { "qword ptr" } else { "dword ptr" }, mem(mm)));
                    c(&|a| a.load_sx(1, w, d, mm), format!("movsx {}, byte ptr {}", rw(w, d), mem(mm)));
                    c(&|a| a.load_sx(2, w, d, mm), format!("movsx {}, word ptr {}", rw(w, d), mem(mm)));
                }
            }
        }
        for &d in &regs {
            for &s in &regs {
                c(&|a| a.movzx8(d, s), format!("movzx {}, {}", r32(d), r8(s)));
                c(&|a| a.movzx16(d, s), format!("movzx {}, {}", r32(d), r16(s)));
                c(&|a| a.movsxd(d, s), format!("movsxd {}, {}", REGS[d.0 as usize], r32(s)));
            }
            for v in [0u64, 1, 0xffff_ffff, 0x1_0000_0000, u64::MAX, 0xffff_ffff_8000_0000, 0xffff_8000_0123_4567, 0x7fff_ffff] {
                c(&|a| a.mov_imm(d, v), format!("mov {}, {}", if v > u32::MAX as u64 { REGS[d.0 as usize].to_string() } else { r32(d) }, if v > u32::MAX as u64 { (v as i64).to_string() } else { v.to_string() }));
            }
            c(&|a| a.mov_imm16(d, 0xbeef), format!("mov {}, 48879", r16(d)));
            c(&|a| a.setcc(0, d), format!("seto {}", r8(d)));
            c(&|a| a.push(d), format!("push {}", REGS[d.0 as usize]));
            c(&|a| a.pop(d), format!("pop {}", REGS[d.0 as usize]));
            c(&|a| a.jmp_reg(d), format!("jmp {}", REGS[d.0 as usize]));
            c(&|a| a.call_reg(d), format!("call {}", REGS[d.0 as usize]));
            for &mm in &mems {
                for size in [1u32, 2, 4, 8] {
                    let rn = match size { 1 => r8(d), 2 => r16(d), 4 => r32(d), _ => REGS[d.0 as usize].to_string() };
                    c(&|a| a.store(size, mm, d), format!("mov {} {}, {rn}", ptr(size), mem(mm)));
                    c(&|a| a.xchg(size, mm, d), format!("xchg {} {}, {rn}", ptr(size), mem(mm)));
                    c(&|a| a.lock_cmpxchg(size, mm, d), format!("lock cmpxchg {} {}, {rn}", ptr(size), mem(mm)));
                    c(&|a| a.load_zx(size, d, mm), match size { 1 | 2 => format!("movzx {}, {} {}", r32(d), ptr(size), mem(mm)), 4 => format!("mov {}, dword ptr {}", r32(d), mem(mm)), _ => format!("mov {}, qword ptr {}", REGS[d.0 as usize], mem(mm)) });
                }
                c(&|a| a.load_sx(4, true, d, mm), format!("movsxd {}, dword ptr {}", REGS[d.0 as usize], mem(mm)));
            }
        }
        for &mm in &mems {
            for size in [1u32, 2, 4, 8] { c(&|a| a.store_imm(size, mm, -3), format!("mov {} {}, -3", ptr(size), mem(mm))); }
            c(&|a| a.alu_mem_imm8(Alu::Cmp, 8, mm, 0), format!("cmp qword ptr {}, 0", mem(mm)));
            c(&|a| a.alu_mem_imm8(Alu::Cmp, 1, mm, 0), format!("cmp byte ptr {}, 0", mem(mm)));
            c(&|a| a.jmp_mem(mm), format!("jmp qword ptr {}", mem(mm)));
            for x in [0u8, 9] {
                c(&|a| a.movdqu_load(x, mm), format!("movdqu xmm{x}, xmmword ptr {}", mem(mm)));
                c(&|a| a.movdqu_store(mm, x), format!("movdqu xmmword ptr {}, xmm{x}", mem(mm)));
                c(&|a| a.movq_load(x, mm), format!("movq xmm{x}, qword ptr {}", mem(mm)));
                c(&|a| a.movd_load(x, mm), format!("movd xmm{x}, dword ptr {}", mem(mm)));
            }
        }
        let _ = ccs;
        for (f, s) in [(Asm::lahf as fn(&mut Asm), "lahf"), (Asm::sahf, "sahf"), (Asm::cmc, "cmc"), (Asm::stc, "stc"), (Asm::ret, "ret"), (Asm::mfence, "mfence")] { c(&|a| f(a), s.into()); }
        c(&|a| a.add_al(0x7f), "add al, 127".into());
        c(&|a| a.cqo(true), "cqo".into());
        c(&|a| a.cqo(false), "cdq".into());
        c(&|a| a.pxor(0, 9), "pxor xmm0, xmm9".into());
        for (op, n) in [(0x74u8, "pcmpeqb"), (0x75, "pcmpeqw"), (0x76, "pcmpeqd"), (0xde, "pmaxub"), (0xda, "pminub"), (0xdb, "pand"), (0xdf, "pandn"), (0xeb, "por"), (0xef, "pxor"), (0x67, "packuswb"), (0x6f, "movdqa")] {
            for (x, y) in [(0u8, 1u8), (3, 2), (8, 1), (2, 12)] { c(&|a| a.sse(op, x, y), format!("{n} xmm{x}, xmm{y}")); }
        }
        for x in [0u8, 3, 9] { c(&|a| a.psrlw(x, 4), format!("psrlw xmm{x}, 4")); }
        for &mm in &mems { c(&|a| a.movq_store(mm, 0), format!("movq qword ptr {}, xmm0", mem(mm))); }
        // assemble the expected text; byte-identical or the same instruction when disassembled
        let input: String = cases.iter().map(|(_, s)| format!("{s}\n")).collect();
        let out = run_mc(&mc, &["-triple=x86_64", "-x86-asm-syntax=intel", "-show-encoding"], &input);
        let refs: Vec<Vec<u8>> = out.lines().filter_map(|l| l.split("encoding: [").nth(1)).map(|e| {
            e.trim_end_matches(']').split(',').map(|b| u8::from_str_radix(b.trim().trim_start_matches("0x"), 16).unwrap()).collect()
        }).collect();
        assert_eq!(refs.len(), cases.len(), "llvm-mc output:\n{out}");
        let differ: Vec<usize> = (0..cases.len()).filter(|&k| cases[k].0 != refs[k]).collect();
        if differ.is_empty() { return; }
        let mine = disasm(&mc, &differ.iter().map(|&k| cases[k].0.clone()).collect::<Vec<_>>());
        let theirs = disasm(&mc, &differ.iter().map(|&k| refs[k].clone()).collect::<Vec<_>>());
        let bad: Vec<String> = differ.iter().enumerate().filter(|&(n, _)| mine[n] != theirs[n])
            .map(|(n, &k)| format!("{}: ours {:02x?} = `{}`, llvm {:02x?} = `{}`", cases[k].1, cases[k].0, mine[n], refs[k], theirs[n])).collect();
        assert!(bad.is_empty(), "{} of {} encodings differ:\n{}", bad.len(), cases.len(), bad[..bad.len().min(40)].join("\n"));
    }
}
