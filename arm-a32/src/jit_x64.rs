//! The A32/T32 block JIT's x86-64 backend (x86-64 Linux hosts).
//!
//! It mirrors the WebAssembly backend (`jit_wasm.rs`) instruction for instruction: the same
//! encodings are translated, the same ones go to the interpreter through `h_step`, the same
//! micro-TLB probes guard loads and stores. A compiled unit is a *region*: the entry basic
//! block plus the blocks that constant branches on the same page reach (up to `MAX_BBS`),
//! and a branch between them is a direct jump while `instret < chain_until`, so loops stay
//! in host code. Exits to other regions go through `dispatcher`, which jumps into the next
//! compiled region while the budget lasts (`SBC_JIT_CHAIN=0`: no dispatcher, every exit
//! returns). The function returns 0 to continue at `r[15]`, 1 for a trap in `jit_trap`, 2
//! when the interpreter ran an instruction that needs the machine to look (a device access,
//! a mode change); `jit_run` does not chain any further on x86-64.
//!
//! Guest registers stay in `Cpu::r` (x86 instructions take them as memory operands). The
//! guest's NZCV lives in r14 as an image of the host's EFLAGS bits: N = SF (bit 7), Z = ZF
//! (bit 6), C = CF (bit 0), V = OF (bit 11), every other bit clear. ADDS and ADCS keep the
//! host flags as they are (`pushfq`); an x86 subtract sets CF to the borrow, the inverse of
//! the A32 carry, so SUBS, RSBS, SBCS and RSCS complement it (`cmc`) first, and SBC/RSC feed
//! the inverted guest C in as the borrow. Logical operations with S take N and Z from a
//! `test` of the result, keep V, and take C from the shifter (computed into a register when
//! the instruction needs it). Conditions test the image's bits. The image goes back to
//! CPSR[31:28] when the code leaves or calls the interpreter, and is rebuilt from the CPSR
//! after an interpreter call.
//!
//! Host registers: rbx = Cpu, r12/r13 = this privilege's read/write micro-TLB tables,
//! r14 = the flags; the rest are scratch within one instruction (`A`, `B`, `R`... below).
//! The code only uses baseline x86-64 instructions.
use super::decode::{scan_a32, scan_t, sign_extend, t32_dp_register, t32_long_multiply, t32_multiply, T};
use super::{ctx, h_step, writes_pc, BlockFn, Ctx};
use crate::cpu::Cpu;
use crate::mmu::{utlb_table, UT, U_READ, U_WRITE};

/// Basic blocks per compiled region.
const MAX_BBS: usize = 24;
/// Instructions per basic block.
const MAX_INSNS: u32 = 64;

// ---------------------------------------------------------------------- the assembler
const RAX: u8 = 0;
const RCX: u8 = 1;
const RDX: u8 = 2;
const RBX: u8 = 3;
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

// condition codes (the low nibble of Jcc/SETcc/CMOVcc)
const CC_B: u8 = 0x2;
const CC_AE: u8 = 0x3;
const CC_E: u8 = 0x4;
const CC_NE: u8 = 0x5;

// group 1 (ADD..CMP), shifts (group 2) and group 3 operations: the ModRM reg field
const ADD: u8 = 0;
const OR: u8 = 1;
const ADC: u8 = 2;
const SBB: u8 = 3;
const AND: u8 = 4;
const SUB: u8 = 5;
const XOR: u8 = 6;
const CMP: u8 = 7;
const ROR: u8 = 1;
const SHL: u8 = 4;
const SHR: u8 = 5;
const SAR: u8 = 7;
const NOT: u8 = 2;
const NEG: u8 = 3;
const DIV: u8 = 6;
const IDIV: u8 = 7;

/// A register or memory operand (`M(base, index, disp)`, index scaled by 1).
#[derive(Clone, Copy, Debug)]
enum Rm {
    R(u8),
    M(u8, Option<u8>, i32),
}
use Rm::{M, R};

/// A small x86-64 encoder: 32-bit operations unless the name ends in 64; branches to labels
/// are rel32 and patched by `finish`.
struct Asm {
    c: Vec<u8>,
    labels: Vec<Option<usize>>,
    /// (offset of a rel32 field, label)
    fixes: Vec<(usize, usize)>,
}

impl Asm {
    fn new() -> Self {
        Asm { c: Vec::with_capacity(8192), labels: Vec::new(), fixes: Vec::new() }
    }
    fn b(&mut self, x: u8) {
        self.c.push(x);
    }
    fn d32(&mut self, x: u32) {
        self.c.extend_from_slice(&x.to_le_bytes());
    }
    /// REX, `opc` and ModRM (+ SIB, displacement) for `opc reg, rm`. `w`: 64-bit operand
    /// size; `byte`: 8-bit registers (spl, bpl, sil, dil need a REX prefix).
    fn op(&mut self, w: bool, byte: bool, opc: &[u8], reg: u8, rm: Rm) {
        let (b, x) = match rm {
            R(r) => (r >> 3, 0),
            M(base, idx, _) => (base >> 3, idx.map_or(0, |i| i >> 3)),
        };
        let rex = 0x40 | u8::from(w) << 3 | (reg >> 3) << 2 | x << 1 | b;
        let low_byte = byte && (matches!(rm, R(4..=7)) || (4..=7).contains(&reg));
        if rex != 0x40 || low_byte {
            self.b(rex);
        }
        self.c.extend_from_slice(opc);
        self.modrm(reg, rm);
    }
    fn modrm(&mut self, reg: u8, rm: Rm) {
        let reg = (reg & 7) << 3;
        match rm {
            R(r) => self.b(0xc0 | reg | r & 7),
            M(base, idx, disp) => {
                // rbp/r13 as a base always take a displacement (mod 00 means RIP/disp32)
                let md = if disp == 0 && base & 7 != 5 {
                    0
                } else if disp as i8 as i32 == disp {
                    0x40
                } else {
                    0x80
                };
                if idx.is_some() || base & 7 == 4 {
                    self.b(md | reg | 4);
                    self.b((idx.unwrap_or(4) & 7) << 3 | base & 7);
                } else {
                    self.b(md | reg | base & 7);
                }
                match md {
                    0x40 => self.b(disp as u8),
                    0x80 => self.d32(disp as u32),
                    _ => {}
                }
            }
        }
    }

    // moves
    fn mov_rr(&mut self, d: u8, s: u8) {
        self.op(false, false, &[0x89], s, R(d));
    }
    fn mov64_rr(&mut self, d: u8, s: u8) {
        self.op(true, false, &[0x89], s, R(d));
    }
    /// mov r32, r/m32
    fn mov_rm(&mut self, d: u8, m: Rm) {
        self.op(false, false, &[0x8b], d, m);
    }
    fn mov64_rm(&mut self, d: u8, m: Rm) {
        self.op(true, false, &[0x8b], d, m);
    }
    /// mov r/m32, r32
    fn mov_mr(&mut self, m: Rm, s: u8) {
        self.op(false, false, &[0x89], s, m);
    }
    fn mov8_mr(&mut self, m: Rm, s: u8) {
        self.op(false, true, &[0x88], s, m);
    }
    fn mov16_mr(&mut self, m: Rm, s: u8) {
        self.b(0x66);
        self.op(false, false, &[0x89], s, m);
    }
    fn mov_ri(&mut self, d: u8, v: u32) {
        if d >= 8 {
            self.b(0x41);
        }
        self.b(0xb8 | d & 7);
        self.d32(v);
    }
    fn mov64_ri(&mut self, d: u8, v: u64) {
        self.b(0x48 | d >> 3);
        self.b(0xb8 | d & 7);
        self.c.extend_from_slice(&v.to_le_bytes());
    }
    /// mov r/m32, imm32
    fn mov_mi(&mut self, m: Rm, v: u32) {
        self.op(false, false, &[0xc7], 0, m);
        self.d32(v);
    }
    fn movzx8(&mut self, d: u8, m: Rm) {
        self.op(false, true, &[0x0f, 0xb6], d, m);
    }
    fn movzx16(&mut self, d: u8, m: Rm) {
        self.op(false, false, &[0x0f, 0xb7], d, m);
    }
    fn movsx8(&mut self, d: u8, m: Rm) {
        self.op(false, true, &[0x0f, 0xbe], d, m);
    }
    fn movsx16(&mut self, d: u8, m: Rm) {
        self.op(false, false, &[0x0f, 0xbf], d, m);
    }
    /// movsxd r64, r/m32
    fn movsxd(&mut self, d: u8, m: Rm) {
        self.op(true, false, &[0x63], d, m);
    }
    fn lea(&mut self, d: u8, m: Rm) {
        self.op(false, false, &[0x8d], d, m);
    }
    fn lea64(&mut self, d: u8, m: Rm) {
        self.op(true, false, &[0x8d], d, m);
    }
    fn cmov(&mut self, cc: u8, d: u8, m: Rm) {
        self.op(false, false, &[0x0f, 0x40 | cc], d, m);
    }

    // arithmetic
    /// `op r/m32, r32` (group 1: ADD, OR, ADC, SBB, AND, SUB, XOR, CMP)
    fn alu(&mut self, op: u8, d: Rm, s: u8) {
        self.op(false, false, &[op << 3 | 1], s, d);
    }
    fn alu64(&mut self, op: u8, d: Rm, s: u8) {
        self.op(true, false, &[op << 3 | 1], s, d);
    }
    /// `op r32, r/m32`
    fn alu_rm(&mut self, op: u8, d: u8, m: Rm) {
        self.op(false, false, &[op << 3 | 3], d, m);
    }
    fn alu64_rm(&mut self, op: u8, d: u8, m: Rm) {
        self.op(true, false, &[op << 3 | 3], d, m);
    }
    fn alui_w(&mut self, w: bool, op: u8, d: Rm, v: u32) {
        if v as i32 as i8 as i32 == v as i32 {
            self.op(w, false, &[0x83], op, d);
            self.b(v as u8);
        } else {
            self.op(w, false, &[0x81], op, d);
            self.d32(v);
        }
    }
    /// `op r/m32, imm`
    fn alui(&mut self, op: u8, d: Rm, v: u32) {
        self.alui_w(false, op, d, v);
    }
    /// `op r/m64, imm32` (sign-extended)
    fn alui64(&mut self, op: u8, d: Rm, v: u32) {
        self.alui_w(true, op, d, v);
    }
    fn test_rr(&mut self, a: u8, b: u8) {
        self.op(false, false, &[0x85], b, R(a));
    }
    fn test_ri(&mut self, d: Rm, v: u32) {
        self.op(false, false, &[0xf7], 0, d);
        self.d32(v);
    }
    /// Shift or rotate by an immediate (ROR, SHL, SHR, SAR).
    fn shift(&mut self, kind: u8, d: Rm, n: u32) {
        self.op(false, false, &[0xc1], kind, d);
        self.b(n as u8);
    }
    fn shift64(&mut self, kind: u8, d: Rm, n: u32) {
        self.op(true, false, &[0xc1], kind, d);
        self.b(n as u8);
    }
    /// Shift or rotate by CL.
    fn shift_cl(&mut self, kind: u8, d: Rm) {
        self.op(false, false, &[0xd3], kind, d);
    }
    fn shift64_cl(&mut self, kind: u8, d: Rm) {
        self.op(true, false, &[0xd3], kind, d);
    }
    /// Group 3: NOT, NEG, DIV, IDIV.
    fn unary(&mut self, op: u8, d: Rm) {
        self.op(false, false, &[0xf7], op, d);
    }
    fn imul(&mut self, d: u8, m: Rm) {
        self.op(false, false, &[0x0f, 0xaf], d, m);
    }
    fn imul64(&mut self, d: u8, m: Rm) {
        self.op(true, false, &[0x0f, 0xaf], d, m);
    }
    fn bswap(&mut self, r: u8) {
        if r >= 8 {
            self.b(0x41);
        }
        self.b(0x0f);
        self.b(0xc8 | r & 7);
    }
    fn bsr(&mut self, d: u8, m: Rm) {
        self.op(false, false, &[0x0f, 0xbd], d, m);
    }
    /// bt r/m32, imm8 (CF = the bit)
    fn bt(&mut self, d: Rm, bit: u32) {
        self.op(false, false, &[0x0f, 0xba], 4, d);
        self.b(bit as u8);
    }
    fn push(&mut self, r: u8) {
        if r >= 8 {
            self.b(0x41);
        }
        self.b(0x50 | r & 7);
    }
    fn pop(&mut self, r: u8) {
        if r >= 8 {
            self.b(0x41);
        }
        self.b(0x58 | r & 7);
    }
    fn pushfq(&mut self) {
        self.b(0x9c);
    }
    fn cmc(&mut self) {
        self.b(0xf5);
    }
    fn cdq(&mut self) {
        self.b(0x99);
    }
    fn ret(&mut self) {
        self.b(0xc3);
    }
    fn mfence(&mut self) {
        self.c.extend_from_slice(&[0x0f, 0xae, 0xf0]);
    }
    fn call_r(&mut self, r: u8) {
        self.op(false, false, &[0xff], 2, R(r));
    }
    fn jmp_r(&mut self, r: u8) {
        self.op(false, false, &[0xff], 4, R(r));
    }
    fn setcc(&mut self, cc: u8, r: u8) {
        self.op(false, true, &[0x0f, 0x90 | cc], 0, R(r));
    }

    // labels and branches
    fn label(&mut self) -> usize {
        self.labels.push(None);
        self.labels.len() - 1
    }
    fn bind(&mut self, l: usize) {
        debug_assert!(self.labels[l].is_none());
        self.labels[l] = Some(self.c.len());
    }
    fn jcc(&mut self, cc: u8, l: usize) {
        self.b(0x0f);
        self.b(0x80 | cc);
        self.fixes.push((self.c.len(), l));
        self.d32(0);
    }
    fn jmp(&mut self, l: usize) {
        self.b(0xe9);
        self.fixes.push((self.c.len(), l));
        self.d32(0);
    }
    /// Patch the branches; the finished code.
    fn finish(mut self) -> Vec<u8> {
        for &(at, l) in &self.fixes {
            let target = self.labels[l].expect("unbound label");
            let rel = (target as i64 - (at as i64 + 4)) as i32;
            self.c[at..at + 4].copy_from_slice(&rel.to_le_bytes());
        }
        self.c
    }
}

// ---------------------------------------------------------------------- the translator
// Fixed registers (callee-saved, so they survive `h_step`).
const CPU: u8 = RBX;
const RTAB: u8 = R12;
const WTAB: u8 = R13;
const FL: u8 = R14;
// Scratch within one instruction (the wasm backend's locals).
const A: u8 = RSI; // operand 1 (Rn)
const B: u8 = RDX; // operand 2
const RES: u8 = RAX; // result
const D: u8 = RDI; // data
const E: u8 = R11; // shift source, second data word
const CO: u8 = RBP; // shifter carry out (0/1; callee-saved, pushed by the prologue)
const AMT: u8 = RCX; // register shift amount (CL)
const BASE: u8 = R8; // base register value
const ADDR: u8 = R9; // address with the offset applied
const HOST: u8 = R10; // micro-TLB tag, then the host address

// the flag image in FL
const F_C: u32 = 1;
const F_Z: u32 = 0x40;
const F_N: u32 = 0x80;
const F_V: u32 = 0x800;
const F_ALL: u32 = F_N | F_Z | F_C | F_V;

/// Where a logical instruction's carry comes from.
#[derive(Clone, Copy)]
enum Carry {
    Unchanged,
    Const(bool),
    /// the shifter carry in `CO`
    Local,
}

/// The ALU operations of A32 and T32 data processing.
#[derive(Clone, Copy, PartialEq)]
enum Alu {
    And,
    Eor,
    Sub,
    Rsb,
    Add,
    Adc,
    Sbc,
    Rsc,
    Orr,
    Mov,
    Bic,
    Mvn,
    Orn,
}

/// An out-of-line slow path: interpret the instruction at `pc`, then resume.
struct Slow {
    label: usize,
    pc: u32,
    before: u32,
    resume: usize,
    size: u32,
    it: u32,
}

struct X {
    a: Asm,
    o: Ctx,
    thumb: bool,
    size: u32,
    it: u32,
    el0: bool,
    mt: bool,
    /// native exits continue through the dispatcher
    chain: bool,
    /// start addresses of the region's basic blocks, and their labels
    region: Vec<u32>,
    bb: Vec<usize>,
    slow: Vec<Slow>,
    /// return with the status in eax
    epi: usize,
    /// after `h_step` asked to leave: status = 3 - eax (1 -> 2, 2 -> 1)
    exit_h: usize,
}

impl X {
    fn cpu(&self, off: usize) -> Rm {
        M(CPU, None, off as i32)
    }
    fn reg(&self, n: u32) -> Rm {
        self.cpu(self.o.r + 4 * n as usize)
    }
    /// Guest register `n` into host `h` (PC reads give the address + 8, or + 4 in Thumb
    /// state).
    fn get_to(&mut self, n: u32, pc: u32, h: u8) {
        if n == 15 {
            self.a.mov_ri(h, pc.wrapping_add(if self.thumb { 4 } else { 8 }));
        } else {
            let m = self.reg(n);
            self.a.mov_rm(h, m);
        }
    }
    /// Host `h` into guest register `n` (not the PC).
    fn set(&mut self, n: u32, h: u8) {
        debug_assert!(n != 15);
        let m = self.reg(n);
        self.a.mov_mr(m, h);
    }
    fn set_const(&mut self, n: u32, v: u32) {
        let m = self.reg(n);
        self.a.mov_mi(m, v);
    }

    // ------------------------------------------------------------------ flags and exits
    /// CPSR[31:28] into the flag image (clobbers rcx, r10).
    fn flags_load(&mut self) {
        let cpsr = self.cpu(self.o.cpsr);
        let a = &mut self.a;
        a.mov_rm(RCX, cpsr);
        a.mov_rr(FL, RCX);
        a.shift(SHR, R(FL), 24);
        a.alui(AND, R(FL), F_N | F_Z);
        a.mov_rr(R10, RCX);
        a.shift(SHR, R(R10), 29);
        a.alui(AND, R(R10), 1);
        a.alu(OR, R(FL), R10);
        a.shift(SHR, R(RCX), 17);
        a.alui(AND, R(RCX), F_V);
        a.alu(OR, R(FL), RCX);
    }
    /// The flag image into CPSR[31:28] (clobbers rcx, r10).
    fn flags_save(&mut self) {
        let cpsr = self.cpu(self.o.cpsr);
        let a = &mut self.a;
        a.mov_rr(R10, FL);
        a.shift(SHL, R(R10), 24);
        a.alui(AND, R(R10), 0xc000_0000);
        a.mov_rr(RCX, FL);
        a.alui(AND, R(RCX), F_C);
        a.shift(SHL, R(RCX), 29);
        a.alu(OR, R(R10), RCX);
        a.mov_rr(RCX, FL);
        a.alui(AND, R(RCX), F_V);
        a.shift(SHL, R(RCX), 17);
        a.alu(OR, R(R10), RCX);
        a.alui(AND, cpsr, 0x0fff_ffff);
        a.alu(OR, cpsr, R10);
    }
    /// NZCV from the host flags of an add (`sub`: of a subtract, whose CF is the borrow).
    fn host_flags(&mut self, sub: bool) {
        if sub {
            self.a.cmc();
        }
        self.a.pushfq();
        self.a.pop(FL);
        self.a.alui(AND, R(FL), F_ALL);
    }
    /// N and Z from host `res`, C from `carry`, V unchanged (logical operations with S).
    fn logic_flags(&mut self, res: u8, carry: Carry) {
        let a = &mut self.a;
        a.test_rr(res, res);
        a.pushfq();
        a.pop(RCX);
        a.alui(AND, R(RCX), F_N | F_Z);
        a.alui(AND, R(FL), if matches!(carry, Carry::Unchanged) { F_V | F_C } else { F_V });
        a.alu(OR, R(FL), RCX);
        match carry {
            Carry::Const(true) => a.alui(OR, R(FL), F_C),
            Carry::Local => a.alu(OR, R(FL), CO),
            _ => {}
        }
    }
    /// Jump to `l` if A32 condition `cond` (0-13) holds (clobbers eax).
    fn jcond(&mut self, cond: u32, l: usize) {
        let a = &mut self.a;
        // (host condition when the base condition holds)
        let holds = match cond >> 1 {
            0..=3 => {
                a.test_ri(R(FL), [F_Z, F_C, F_N, F_V][(cond >> 1) as usize]);
                CC_NE
            }
            4 => {
                // HI: C and not Z
                a.mov_rr(RAX, FL);
                a.alui(AND, R(RAX), F_C | F_Z);
                a.alui(CMP, R(RAX), F_C);
                CC_E
            }
            5 | 6 => {
                // GE: N == V (bit 11 of image ^ image << 4); GT: also Z clear (bit 6; bit 2
                // of the image is always clear)
                a.mov_rr(RAX, FL);
                a.shift(SHL, R(RAX), 4);
                a.alu(XOR, R(RAX), FL);
                a.test_ri(R(RAX), if cond >> 1 == 5 { F_V } else { F_V | F_Z });
                CC_E
            }
            _ => {
                a.jmp(l);
                return;
            }
        };
        a.jcc(if cond & 1 == 0 { holds } else { holds ^ 1 }, l);
    }
    fn add_instret(&mut self, k: u32) {
        if k != 0 {
            let m = self.cpu(self.o.instret);
            self.a.alui64(ADD, m, k);
        }
    }
    fn ret(&mut self, status: u32) {
        self.a.mov_ri(RAX, status);
        let l = self.epi;
        self.a.jmp(l);
    }
    /// Continue at `r[15]` (flags saved, instructions counted): the next compiled region
    /// through the dispatcher, or back to `jit_run` with status 0.
    fn leave(&mut self) {
        if self.chain {
            let disp = self.cpu(self.o.disp);
            self.a.mov64_rm(RAX, disp);
            self.a.jmp_r(RAX);
        } else {
            self.ret(0);
        }
    }
    /// Leave for constant `target` after `count` instructions of the basic block: a target
    /// in the region is a jump to it while the budget lasts.
    fn exit_const(&mut self, target: u32, count: u32) {
        self.add_instret(count);
        if let Some(k) = self.region.iter().position(|&s| s == target) {
            let (instret, until) = (self.cpu(self.o.instret), self.cpu(self.o.chain_until));
            let l = self.bb[k];
            self.a.mov64_rm(RCX, instret);
            self.a.alu64_rm(CMP, RCX, until);
            self.a.jcc(CC_B, l);
        }
        self.exit_out_counted(target);
    }
    /// Leave for `target` (instructions already counted), never through the region.
    fn exit_out_counted(&mut self, target: u32) {
        self.flags_save();
        self.set_const(15, target);
        self.leave();
    }
    /// Store host `h` with `mask` applied into r[15] (through rcx).
    fn set_pc_masked(&mut self, h: u8, mask: u32) {
        let pcm = self.reg(15);
        self.a.mov_rr(RCX, h);
        self.a.alui(AND, R(RCX), mask);
        self.a.mov_mr(pcm, RCX);
    }
    /// Leave for the address in host `h` with BXWritePC semantics (bit 0 selects Thumb).
    fn exit_bx(&mut self, h: u8, count: u32) {
        let (arm, done) = (self.a.label(), self.a.label());
        self.a.test_ri(R(h), 1);
        self.a.jcc(CC_E, arm);
        self.set_pc_masked(h, !1);
        if !self.thumb {
            self.set_t(true);
        }
        self.a.jmp(done);
        self.a.bind(arm);
        self.set_pc_masked(h, !3);
        if self.thumb {
            self.set_t(false);
        }
        self.a.bind(done);
        self.flags_save();
        self.add_instret(count);
        self.leave();
    }
    /// BranchWritePC in Thumb state: to the address in host `h` (bit 0 ignored).
    fn exit_thumb_branch(&mut self, h: u8, count: u32) {
        self.set_pc_masked(h, !1);
        self.flags_save();
        self.add_instret(count);
        self.leave();
    }
    fn set_t(&mut self, thumb: bool) {
        let cpsr = self.cpu(self.o.cpsr);
        if thumb {
            self.a.alui(OR, cpsr, 1 << 5);
        } else {
            self.a.alui(AND, cpsr, !(1 << 5));
        }
    }
    /// Put ITSTATE `it` into the CPSR.
    fn write_itstate(&mut self, it: u32) {
        let cpsr = self.cpu(self.o.cpsr);
        self.a.alui(AND, cpsr, !0x0600_fc00);
        let bits = (it & 3) << 25 | (it >> 2) << 10;
        if bits != 0 {
            self.a.alui(OR, cpsr, bits);
        }
    }
    /// Run the instruction at `pc` (the `before`-th of the basic block) in the interpreter.
    /// `last`: the block ends here in any case.
    fn call_interp(&mut self, pc: u32, before: u32, last: bool, why: u32) {
        self.flags_save();
        if self.it != 0 {
            let it = self.it;
            self.write_itstate(it);
        }
        let a = &mut self.a;
        a.mov64_rr(RDI, CPU);
        a.mov_ri(RSI, pc);
        a.mov_ri(RDX, before);
        a.mov_ri(RCX, last as u32 | why << 1 | u32::from(self.size == 2) << 3);
        a.mov64_ri(RAX, h_step as *const () as usize as u64);
        a.call_r(RAX);
        let l = self.exit_h;
        if last {
            // h_step returns 1 or 2 here
            self.a.jmp(l);
        } else {
            self.a.test_rr(RAX, RAX);
            self.a.jcc(CC_NE, l);
            self.flags_load();
        }
    }

    // ------------------------------------------------------------------ memory
    /// Jump to `slow` unless the micro-TLB maps an aligned `size`-byte access at the address
    /// in host `a`; on a hit `HOST` = its host address (clobbers rcx).
    fn probe(&mut self, a: u8, size: u32, write: bool, slow: usize) {
        let tab = if write { WTAB } else { RTAB };
        let x = &mut self.a;
        x.mov_rr(RCX, a);
        x.shift(SHR, R(RCX), 8);
        x.alui(AND, R(RCX), ((UT as u32) - 1) << 4); // entry offset: page % UT * 16
        x.mov_rr(HOST, a);
        x.alui(AND, R(HOST), 0xffff_f000 | (size - 1));
        x.alu64_rm(CMP, HOST, M(tab, Some(RCX), 0));
        x.jcc(CC_NE, slow);
        x.mov64_rm(HOST, M(tab, Some(RCX), 8));
        x.alu64(ADD, R(HOST), a);
    }
    /// Jump to `slow` unless `bytes` bytes from host `a` stay in one page (clobbers rcx).
    fn same_page(&mut self, a: u8, bytes: u32, slow: usize) {
        self.a.lea(RCX, M(a, None, bytes as i32 - 1));
        self.a.alu(XOR, R(RCX), a);
        self.a.shift(SHR, R(RCX), 12);
        self.a.jcc(CC_NE, slow);
    }
    /// A slow path for the current instruction: interpret it, then resume after it.
    fn slow_path(&mut self, pc: u32, before: u32) -> (usize, usize) {
        let (label, resume) = (self.a.label(), self.a.label());
        self.slow.push(Slow { label, pc, before, resume, size: self.size, it: self.it });
        (label, resume)
    }
    /// Base (into `BASE`) and the offset-applied address (into `ADDR`; a register offset is
    /// in `B`); returns the host register holding the accessed address.
    fn address(&mut self, rn: u32, pc: u32, pre: bool, up: bool, offset: Option<u32>) -> u8 {
        if self.thumb && rn == 15 {
            self.a.mov_ri(BASE, pc.wrapping_add(4) & !3); // Align(PC, 4)
        } else {
            self.get_to(rn, pc, BASE);
        }
        match offset {
            Some(0) => self.a.mov_rr(ADDR, BASE),
            Some(imm) => self.a.lea(ADDR, M(BASE, None, if up { imm as i32 } else { -(imm as i32) })),
            None => {
                self.a.mov_rr(ADDR, BASE);
                self.a.alu(if up { ADD } else { SUB }, R(ADDR), B);
            }
        }
        if pre {
            ADDR
        } else {
            BASE
        }
    }
    /// Load `size` bytes (sign-extended with `signed`) from [HOST + off] into host `d`.
    fn load_host(&mut self, d: u8, size: u32, signed: bool, off: i32) {
        let m = M(HOST, None, off);
        match (size, signed) {
            (1, false) => self.a.movzx8(d, m),
            (1, true) => self.a.movsx8(d, m),
            (2, false) => self.a.movzx16(d, m),
            (2, true) => self.a.movsx16(d, m),
            _ => self.a.mov_rm(d, m),
        }
    }
    /// Store the low `size` bytes of host `s` to [HOST + off].
    fn store_host(&mut self, s: u8, size: u32, off: i32) {
        let m = M(HOST, None, off);
        match size {
            1 => self.a.mov8_mr(m, s),
            2 => self.a.mov16_mr(m, s),
            _ => self.a.mov_mr(m, s),
        }
    }

    // ------------------------------------------------------------------ data processing
    /// The A32 shifter operand (immediate or register) of `insn` into `B`; the carry into
    /// `CO` when `carry` asks for it. None: interpret.
    fn operand2(&mut self, insn: u32, pc: u32, carry: bool) -> Option<Carry> {
        if insn & (1 << 25) != 0 {
            let rotation = (insn >> 8) & 0xf;
            let value = (insn & 0xff).rotate_right(2 * rotation);
            self.a.mov_ri(B, value);
            return Some(if rotation == 0 { Carry::Unchanged } else { Carry::Const(value >> 31 != 0) });
        }
        let (rm, kind) = (insn & 0xf, (insn >> 5) & 3);
        if insn & 0x10 != 0 {
            // register-specified shift (amount = Rs[7:0])
            let rs = (insn >> 8) & 0xf;
            if rm == 15 || rs == 15 {
                return None;
            }
            self.get_to(rm, pc, E);
            self.get_to(rs, pc, AMT);
            self.a.alui(AND, R(AMT), 0xff);
            self.shift_reg(kind, carry);
            return Some(if carry { Carry::Local } else { Carry::Unchanged });
        }
        let amount = (insn >> 7) & 0x1f;
        if kind == 0 && amount == 0 {
            self.get_to(rm, pc, B);
            return Some(Carry::Unchanged);
        }
        self.get_to(rm, pc, E);
        let a = &mut self.a;
        if kind == 3 && amount == 0 {
            // RRX
            a.mov_rr(B, FL);
            a.alui(AND, R(B), F_C);
            a.shift(SHL, R(B), 31);
            a.mov_rr(RCX, E);
            a.shift(SHR, R(RCX), 1);
            a.alu(OR, R(B), RCX);
            if carry {
                a.mov_rr(CO, E);
                a.alui(AND, R(CO), 1);
            }
            return Some(if carry { Carry::Local } else { Carry::Unchanged });
        }
        let carry_bit = match (kind, amount) {
            (0, n) => {
                a.mov_rr(B, E);
                a.shift(SHL, R(B), n);
                32 - n
            }
            (1, 0) => {
                a.mov_ri(B, 0);
                31
            }
            (1, n) => {
                a.mov_rr(B, E);
                a.shift(SHR, R(B), n);
                n - 1
            }
            (2, 0) => {
                a.mov_rr(B, E);
                a.shift(SAR, R(B), 31);
                31
            }
            (2, n) => {
                a.mov_rr(B, E);
                a.shift(SAR, R(B), n);
                n - 1
            }
            (_, n) => {
                a.mov_rr(B, E);
                a.shift(ROR, R(B), n);
                n - 1
            }
        };
        if carry {
            a.mov_rr(CO, E);
            if carry_bit != 0 {
                a.shift(SHR, R(CO), carry_bit);
            }
            a.alui(AND, R(CO), 1);
            return Some(Carry::Local);
        }
        Some(Carry::Unchanged)
    }

    /// Shift `E` by `AMT` (0-255) into `B`; with `carry`, the shifter carry into `CO`
    /// (unchanged for amount 0). Clobbers rdi.
    fn shift_reg(&mut self, kind: u32, carry: bool) {
        let a = &mut self.a;
        if kind == 3 {
            a.mov_rr(B, E);
            a.shift_cl(ROR, R(B)); // amount mod 32
            if carry {
                a.mov_rr(CO, B);
                a.shift(SHR, R(CO), 31);
                a.mov_rr(RDI, FL);
                a.alui(AND, R(RDI), F_C);
                a.test_rr(AMT, AMT);
                a.cmov(CC_E, CO, R(RDI));
            }
            return;
        }
        // s = min(amount, 33) as a 64-bit shift: amounts of 32 and more give 0 (or the sign)
        a.mov_ri(RDI, 33);
        a.alui(CMP, R(AMT), 33);
        a.cmov(CC_AE, AMT, R(RDI));
        if kind == 0 {
            a.mov_rr(B, E);
            a.shift64_cl(SHL, R(B));
            if carry {
                a.mov64_rr(CO, B);
                a.shift64(SHR, R(CO), 32);
                a.alui(AND, R(CO), 1);
            }
        } else {
            // (x << 1) >> s: bit 0 is the carry, the rest the result
            let op = if kind == 1 { SHR } else { SAR };
            if kind == 1 {
                a.mov_rr(B, E);
            } else {
                a.movsxd(B, R(E));
            }
            a.shift64(SHL, R(B), 1);
            a.shift64_cl(op, R(B));
            if carry {
                a.mov_rr(CO, B);
                a.alui(AND, R(CO), 1);
            }
            a.shift64(op, R(B), 1);
        }
        if carry {
            a.mov_rr(RDI, FL);
            a.alui(AND, R(RDI), F_C);
            a.test_rr(AMT, AMT);
            a.cmov(CC_E, CO, R(RDI));
        }
    }

    /// `RES` = `A` <op> `B`; NZCV when `s` (logical operations take C from `carry`).
    fn alu(&mut self, op: Alu, s: bool, carry: Carry) {
        let a = &mut self.a;
        let logical = match op {
            Alu::And | Alu::Eor | Alu::Orr => {
                a.mov_rr(RES, A);
                a.alu([AND, XOR, OR][[Alu::And, Alu::Eor, Alu::Orr].iter().position(|&o| o == op).unwrap()], R(RES), B);
                true
            }
            Alu::Mov => {
                a.mov_rr(RES, B);
                true
            }
            Alu::Bic | Alu::Mvn | Alu::Orn => {
                a.mov_rr(RES, B);
                a.unary(NOT, R(RES));
                match op {
                    Alu::Bic => a.alu(AND, R(RES), A),
                    Alu::Orn => a.alu(OR, R(RES), A),
                    _ => {}
                }
                true
            }
            Alu::Add => {
                a.mov_rr(RES, A);
                a.alu(ADD, R(RES), B);
                false
            }
            Alu::Sub | Alu::Rsb => {
                let (x, y) = if op == Alu::Rsb { (B, A) } else { (A, B) };
                a.mov_rr(RES, x);
                a.alu(SUB, R(RES), y);
                false
            }
            Alu::Adc => {
                a.bt(R(FL), 0); // CF = C
                a.mov_rr(RES, A);
                a.alu(ADC, R(RES), B);
                false
            }
            Alu::Sbc | Alu::Rsc => {
                // x - y - NOT(C): the borrow in is the inverted guest carry
                let (x, y) = if op == Alu::Rsc { (B, A) } else { (A, B) };
                a.bt(R(FL), 0);
                a.cmc();
                a.mov_rr(RES, x);
                a.alu(SBB, R(RES), y);
                false
            }
        };
        if s {
            if logical {
                self.logic_flags(RES, carry);
            } else {
                self.host_flags(!matches!(op, Alu::Add | Alu::Adc));
            }
        }
    }
}

// ---------------------------------------------------------------------- A32
impl X {
    /// Emit guest instruction `insn` at `pc`, the `n`-th of the basic block. Returns true
    /// when the block ends with it.
    fn insn(&mut self, insn: u32, pc: u32, n: u32) -> bool {
        let cond = insn >> 28;
        if cond == 15 {
            return self.unconditional(insn, pc, n);
        }
        let skip = (cond != 14).then(|| {
            let l = self.a.label();
            self.jcond(cond ^ 1, l);
            l
        });
        let end = match self.native(insn, pc, n) {
            Some(end) => end,
            None => {
                let end = cond == 14 && writes_pc(insn);
                self.call_interp(pc, n, end, 0);
                end
            }
        };
        match skip {
            Some(l) => {
                self.a.bind(l);
                false
            }
            None => end,
        }
    }

    fn unconditional(&mut self, insn: u32, pc: u32, n: u32) -> bool {
        // BLX (immediate): call Thumb code
        if insn & 0x0e00_0000 == 0x0a00_0000 {
            let offset = ((((insn & 0x00ff_ffff) << 8) as i32 >> 6) as u32) | (insn >> 23) & 2;
            let target = pc.wrapping_add(8).wrapping_add(offset);
            self.set_const(14, pc.wrapping_add(4));
            self.set_const(15, target & !1);
            self.set_t(true);
            self.flags_save();
            self.add_instret(n + 1);
            self.leave();
            return true;
        }
        // barriers and preload hints
        if insn & 0x0fff_fff0 == 0x057f_f040
            || insn & 0x0fff_fff0 == 0x057f_f050
            || insn == 0xf57f_f06f
            || insn & 0x0f70_f000 == 0x0550_f000
            || insn & 0x0f70_f000 == 0x0510_f000
            || matches!(insn & 0x0f70_f010, 0x0750_f000 | 0x0710_f000 | 0x0650_f000)
            || insn & 0x0f70_f000 == 0x0450_f000
        {
            if self.mt && insn & 0x0fff_ffe0 == 0x057f_f040 {
                self.a.mfence(); // DMB and DSB order stores against other host threads
            }
            return false;
        }
        // CPS, RFE, SRS change the mode; everything else (NEON, CLREX) continues unless
        // the interpreter says otherwise
        let end = insn & 0x0ff1_fe20 == 0x0100_0000 || insn & 0x0e50_ffff == 0x0810_0a00;
        self.call_interp(pc, n, end, 0);
        end
    }

    /// Native translation of a conditional-space instruction: Some(block ends) or None.
    fn native(&mut self, insn: u32, pc: u32, n: u32) -> Option<bool> {
        if insn & 0x0c00_0000 == 0 {
            let imm = insn & (1 << 25) != 0;
            if !imm && insn & 0x90 == 0x90 {
                if insn & 0x0fc0_00f0 == 0x0000_0090 {
                    return self.mul(insn).then_some(false);
                }
                if insn & 0x0f80_00f0 == 0x0080_0090 {
                    return self.mul_long(insn).then_some(false);
                }
                if insn & 0x60 != 0 {
                    return self.ldst_extra(insn, pc, n).then_some(false);
                }
                return None;
            }
            if insn & 0x0190_0000 == 0x0100_0000 {
                return self.misc(insn, pc, n);
            }
            return self.data_processing(insn, pc, n);
        }
        if insn & 0x0e00_0010 == 0x0600_0010 {
            return self.media(insn).then_some(false);
        }
        if insn & 0x0c00_0000 == 0x0400_0000 {
            return self.ldst(insn, pc, n);
        }
        if insn & 0x0e00_0000 == 0x0800_0000 {
            return self.ldm_stm(insn, pc, n);
        }
        if self.coprocessor(insn) {
            return Some(false);
        }
        if insn & 0x0e00_0000 == 0x0a00_0000 {
            let offset = (((insn & 0x00ff_ffff) << 8) as i32 >> 6) as u32;
            if insn & (1 << 24) != 0 {
                self.set_const(14, pc.wrapping_add(4));
            }
            self.exit_const(pc.wrapping_add(8).wrapping_add(offset), n + 1);
            return Some(true);
        }
        None
    }

    fn data_processing(&mut self, insn: u32, pc: u32, n: u32) -> Option<bool> {
        let (opcode, s, rn, rd) = ((insn >> 21) & 0xf, insn & (1 << 20) != 0, (insn >> 16) & 0xf, (insn >> 12) & 0xf);
        let test = matches!(opcode, 8..=11);
        let logical = matches!(opcode, 0 | 1 | 8 | 9 | 12..=15);
        let reg_shift = insn & (1 << 25) == 0 && insn & 0x10 != 0;
        // SUBS PC etc. return from exceptions; PC reads with a register shift are
        // unpredictable
        if rd == 15 && !test && s || reg_shift && rn == 15 {
            return None;
        }
        let carry = self.operand2(insn, pc, s && logical)?;
        if !matches!(opcode, 13 | 15) {
            self.get_to(rn, pc, A);
        }
        let op = match opcode {
            0 | 8 => Alu::And,
            1 | 9 => Alu::Eor,
            2 | 10 => Alu::Sub,
            3 => Alu::Rsb,
            4 | 11 => Alu::Add,
            5 => Alu::Adc,
            6 => Alu::Sbc,
            7 => Alu::Rsc,
            12 => Alu::Orr,
            13 => Alu::Mov,
            14 => Alu::Bic,
            _ => Alu::Mvn,
        };
        self.alu(op, s, carry);
        if rd == 15 && !test {
            // ARM-state ALUWritePC interworks like BX
            self.exit_bx(RES, n + 1);
            return Some(insn >> 28 == 14);
        }
        if !test {
            self.set(rd, RES);
        }
        Some(false)
    }

    fn mul(&mut self, insn: u32) -> bool {
        let (rd, ra, rs, rm) = ((insn >> 16) & 0xf, (insn >> 12) & 0xf, (insn >> 8) & 0xf, insn & 0xf);
        if [rd, ra, rs, rm].contains(&15) {
            return false;
        }
        let (mrm, mrs, mra) = (self.reg(rm), self.reg(rs), self.reg(ra));
        self.a.mov_rm(RES, mrm);
        self.a.imul(RES, mrs);
        if insn & (1 << 21) != 0 {
            self.a.alu_rm(ADD, RES, mra);
        }
        if insn & (1 << 20) != 0 {
            self.logic_flags(RES, Carry::Unchanged);
        }
        self.set(rd, RES);
        true
    }

    fn mul_long(&mut self, insn: u32) -> bool {
        let (hi, lo, rs, rm) = ((insn >> 16) & 0xf, (insn >> 12) & 0xf, (insn >> 8) & 0xf, insn & 0xf);
        if [hi, lo, rs, rm].contains(&15) || hi == lo || insn & (1 << 20) != 0 {
            return false;
        }
        let (mrm, mrs, mhi, mlo) = (self.reg(rm), self.reg(rs), self.reg(hi), self.reg(lo));
        let a = &mut self.a;
        if insn & (1 << 22) != 0 {
            a.movsxd(RES, mrm);
            a.movsxd(RCX, mrs);
        } else {
            a.mov_rm(RES, mrm);
            a.mov_rm(RCX, mrs);
        }
        // the low 64 bits of the product are the same for signed and unsigned inputs
        a.imul64(RES, R(RCX));
        if insn & (1 << 21) != 0 {
            a.mov_rm(RDX, mhi);
            a.shift64(SHL, R(RDX), 32);
            a.mov_rm(RCX, mlo);
            a.alu64(OR, R(RDX), RCX);
            a.alu64(ADD, R(RES), RDX);
        }
        a.mov_mr(mlo, RES);
        a.shift64(SHR, R(RES), 32);
        a.mov_mr(mhi, RES);
        true
    }

    /// The miscellaneous space (data-processing opcodes 8-11 without S): Some(block ends).
    fn misc(&mut self, insn: u32, pc: u32, n: u32) -> Option<bool> {
        if insn & 0x0fff_fff0 == 0x012f_ff10 || insn & 0x0fff_fff0 == 0x012f_ff30 {
            let rm = insn & 0xf;
            if rm == 15 {
                return None;
            }
            self.get_to(rm, pc, D);
            if insn & 0x20 != 0 {
                self.set_const(14, pc.wrapping_add(4));
            }
            self.exit_bx(D, n + 1);
            return Some(true);
        }
        // MOVW / MOVT
        if insn & 0x0fb0_0000 == 0x0300_0000 {
            let rd = (insn >> 12) & 0xf;
            if rd == 15 {
                return None;
            }
            let imm = (insn >> 4) & 0xf000 | insn & 0xfff;
            if insn & (1 << 22) == 0 {
                self.a.mov_ri(RES, imm);
            } else {
                self.get_to(rd, pc, RES);
                self.a.alui(AND, R(RES), 0xffff);
                self.a.alui(OR, R(RES), imm << 16);
            }
            self.set(rd, RES);
            return Some(false);
        }
        // CLZ
        if insn & 0x0fff_0ff0 == 0x016f_0f10 {
            let (rd, rm) = ((insn >> 12) & 0xf, insn & 0xf);
            if rd == 15 || rm == 15 {
                return None;
            }
            let m = self.reg(rm);
            // 31 - BSR, or 32 (63 ^ 31) for zero
            self.a.mov_ri(RCX, 63);
            self.a.bsr(RES, m);
            self.a.cmov(CC_E, RES, R(RCX));
            self.a.alui(XOR, R(RES), 31);
            self.set(rd, RES);
            return Some(false);
        }
        // NOP, YIELD
        if insn & 0x0fff_ffff == 0x0320_f000 || insn & 0x0fff_ffff == 0x0320_f001 {
            return Some(false);
        }
        // MRS Rd, CPSR (execution state bits read as zero)
        if insn & 0x0fff_0fff == 0x010f_0000 {
            let rd = (insn >> 12) & 0xf;
            if rd == 15 {
                return None;
            }
            self.flags_save();
            let cpsr = self.cpu(self.o.cpsr);
            self.a.mov_rm(RES, cpsr);
            self.a.alui(AND, R(RES), !(0x0100_0000 | 0x0600_fc00 | 1 << 5));
            self.set(rd, RES);
            return Some(false);
        }
        None
    }

    /// MRC of the thread ID registers and the (no-op) cache maintenance MCRs.
    fn coprocessor(&mut self, insn: u32) -> bool {
        if insn & 0x0f00_0f10 != 0x0e00_0f10 || self.el0 {
            return false;
        }
        let (read, opc1, crn, rd, crm, opc2) = (
            insn & (1 << 20) != 0,
            (insn >> 21) & 7,
            (insn >> 16) & 0xf,
            (insn >> 12) & 0xf,
            insn & 0xf,
            (insn >> 5) & 7,
        );
        if rd == 15 || opc1 != 0 {
            return false;
        }
        if read && crn == 13 && crm == 0 && (2..=4).contains(&opc2) {
            let off = [self.o.tpidrurw, self.o.tpidrro, self.o.tpidrprw][opc2 as usize - 2];
            let m = self.cpu(off);
            self.a.mov_rm(RES, m);
            self.set(rd, RES);
            return true;
        }
        !read && crn == 7 && matches!(crm, 1 | 5 | 6 | 10 | 11 | 14)
    }

    fn media(&mut self, insn: u32) -> bool {
        let (rd, rm) = ((insn >> 12) & 0xf, insn & 0xf);
        // REV, REV16
        if matches!(insn & 0x0fff_0ff0, 0x06bf_0f30 | 0x06bf_0fb0) {
            if rd == 15 || rm == 15 {
                return false;
            }
            self.get_to(rm, 0, RES);
            self.a.bswap(RES);
            if insn & 0x80 != 0 {
                self.a.shift(ROR, R(RES), 16); // REV16
            }
            self.set(rd, RES);
            return true;
        }
        // UXTB, UXTH, SXTB, SXTH (+ accumulate)
        let extend = match insn & 0x0ff0_03f0 {
            0x06e0_0070 => Some(0u8),
            0x06f0_0070 => Some(1),
            0x06a0_0070 => Some(2),
            0x06b0_0070 => Some(3),
            _ => None,
        };
        if let Some(kind) = extend {
            let rn = (insn >> 16) & 0xf;
            if rd == 15 || rm == 15 {
                return false;
            }
            self.get_to(rm, 0, RES);
            let rotation = ((insn >> 10) & 3) * 8;
            if rotation != 0 {
                self.a.shift(ROR, R(RES), rotation);
            }
            match kind {
                0 => self.a.movzx8(RES, R(RES)),
                1 => self.a.movzx16(RES, R(RES)),
                2 => self.a.movsx8(RES, R(RES)),
                _ => self.a.movsx16(RES, R(RES)),
            }
            if rn != 15 {
                let m = self.reg(rn);
                self.a.alu_rm(ADD, RES, m);
            }
            self.set(rd, RES);
            return true;
        }
        // BFI, BFC
        if insn & 0x0fe0_0070 == 0x07c0_0010 {
            let (msb, lsb) = ((insn >> 16) & 0x1f, (insn >> 7) & 0x1f);
            if rd == 15 || msb < lsb {
                return false;
            }
            let width = msb - lsb + 1;
            let mask = (if width == 32 { u32::MAX } else { (1 << width) - 1 }) << lsb;
            self.get_to(rd, 0, RES);
            self.a.alui(AND, R(RES), !mask);
            if rm != 15 {
                self.get_to(rm, 0, RCX);
                if lsb != 0 {
                    self.a.shift(SHL, R(RCX), lsb);
                }
                self.a.alui(AND, R(RCX), mask);
                self.a.alu(OR, R(RES), RCX);
            }
            self.set(rd, RES);
            return true;
        }
        // UBFX, SBFX
        if matches!(insn & 0x0fe0_0070, 0x07e0_0050 | 0x07a0_0050) {
            let (width, lsb) = (((insn >> 16) & 0x1f) + 1, (insn >> 7) & 0x1f);
            if rd == 15 || rm == 15 || lsb + width > 32 {
                return false;
            }
            self.get_to(rm, 0, RES);
            if insn & 0x0fe0_0070 == 0x07a0_0050 {
                if 32 - lsb - width != 0 {
                    self.a.shift(SHL, R(RES), 32 - lsb - width);
                }
                if width != 32 {
                    self.a.shift(SAR, R(RES), 32 - width);
                }
            } else {
                if lsb != 0 {
                    self.a.shift(SHR, R(RES), lsb);
                }
                if width < 32 {
                    self.a.alui(AND, R(RES), (1 << width) - 1);
                }
            }
            self.set(rd, RES);
            return true;
        }
        // SDIV, UDIV (division by zero gives zero; INT_MIN / -1 gives INT_MIN)
        if matches!(insn & 0x0ff0_f0f0, 0x0710_f010 | 0x0730_f010) {
            let (rd, divisor, dividend) = ((insn >> 16) & 0xf, (insn >> 8) & 0xf, insn & 0xf);
            if [rd, divisor, dividend].contains(&15) {
                return false;
            }
            let unsigned = insn & (1 << 21) != 0;
            self.get_to(dividend, 0, RES);
            self.get_to(divisor, 0, RCX);
            let a = &mut self.a;
            let (zero, done) = (a.label(), a.label());
            a.test_rr(RCX, RCX);
            a.jcc(CC_E, zero);
            if unsigned {
                a.mov_ri(RDX, 0);
                a.unary(DIV, R(RCX));
            } else {
                let normal = a.label();
                a.alui(CMP, R(RCX), u32::MAX);
                a.jcc(CC_NE, normal);
                a.unary(NEG, R(RES));
                a.jmp(done);
                a.bind(normal);
                a.cdq();
                a.unary(IDIV, R(RCX));
            }
            a.jmp(done);
            a.bind(zero);
            a.mov_ri(RES, 0);
            a.bind(done);
            self.set(rd, RES);
            return true;
        }
        false
    }

    /// A load into the PC left the code on the fast path (its slow path resumes at the
    /// label bound just before this); the interpreted slow path continues only if it loaded
    /// the next instruction's address. Returns whether the block ends.
    fn after_pc_load(&mut self, insn: u32, pc: u32, n: u32) -> bool {
        if insn >> 28 == 14 {
            self.exit_const(pc.wrapping_add(self.size), n + 1);
            return true;
        }
        false
    }

    /// LDR, STR, LDRB, STRB.
    fn ldst(&mut self, insn: u32, pc: u32, n: u32) -> Option<bool> {
        let (pre, up, byte, wb, load) = (
            insn & (1 << 24) != 0,
            insn & (1 << 23) != 0,
            insn & (1 << 22) != 0,
            insn & (1 << 21) != 0,
            insn & (1 << 20) != 0,
        );
        let (rn, rd) = ((insn >> 16) & 0xf, (insn >> 12) & 0xf);
        let writeback = !pre || wb;
        if !pre && wb || load && rd == 15 && byte || writeback && (rn == 15 || load && rn == rd) {
            return None;
        }
        let offset = if insn & (1 << 25) == 0 {
            Some(insn & 0xfff)
        } else {
            let (rm, kind, amount) = (insn & 0xf, (insn >> 5) & 3, (insn >> 7) & 0x1f);
            if rm == 15 || kind == 3 && amount == 0 {
                return None;
            }
            self.get_to(rm, pc, B);
            match (kind, amount) {
                (0, 0) => {}
                (0, k) => self.a.shift(SHL, R(B), k),
                (1, 0) => self.a.mov_ri(B, 0),
                (1, k) => self.a.shift(SHR, R(B), k),
                (2, 0) => self.a.shift(SAR, R(B), 31),
                (2, k) => self.a.shift(SAR, R(B), k),
                (_, k) => self.a.shift(ROR, R(B), k),
            }
            None
        };
        let a = self.address(rn, pc, pre, up, offset);
        let size = if byte { 1 } else { 4 };
        let (slow, resume) = self.slow_path(pc, n);
        self.probe(a, size, !load, slow);
        if load {
            self.load_host(D, size, false, 0);
            if writeback {
                self.set(rn, ADDR);
            }
            if rd == 15 {
                self.exit_bx(D, n + 1);
                self.a.bind(resume);
                return Some(self.after_pc_load(insn, pc, n));
            }
            self.set(rd, D);
        } else {
            self.get_to(rd, pc, RCX);
            self.store_host(RCX, size, 0);
            if writeback {
                self.set(rn, ADDR);
            }
        }
        self.a.bind(resume);
        Some(false)
    }

    /// LDRH, STRH, LDRSB, LDRSH, LDRD, STRD.
    fn ldst_extra(&mut self, insn: u32, pc: u32, n: u32) -> bool {
        let (pre, up, wb) = (insn & (1 << 24) != 0, insn & (1 << 23) != 0, insn & (1 << 21) != 0);
        let (rn, rd) = ((insn >> 16) & 0xf, (insn >> 12) & 0xf);
        let (load, signed, half) = (insn & (1 << 20) != 0, insn & (1 << 6) != 0, insn & (1 << 5) != 0);
        let pair = !load && signed;
        let writeback = !pre || wb;
        if !pre && wb
            || pair && (rd & 1 != 0 || rd >= 14)
            || !pair && load && rd == 15
            || writeback && (rn == 15 || load && rn == rd || pair && !half && (rn == rd || rn == rd + 1))
        {
            return false;
        }
        let offset = if insn & (1 << 22) != 0 {
            Some((insn >> 4) & 0xf0 | insn & 0xf)
        } else {
            let rm = insn & 0xf;
            if insn & 0xf00 != 0 || rm == 15 {
                return false;
            }
            self.get_to(rm, pc, B);
            None
        };
        let a = self.address(rn, pc, pre, up, offset);
        let (slow, resume) = self.slow_path(pc, n);
        if pair {
            // STRD when bit 5 (half) is set, LDRD otherwise
            self.same_page(a, 8, slow);
            self.probe(a, 4, half, slow);
        } else {
            self.probe(a, if half { 2 } else { 1 }, !load, slow);
        }
        if pair {
            if half {
                self.get_to(rd, pc, RCX);
                self.store_host(RCX, 4, 0);
                self.get_to(rd + 1, pc, RCX);
                self.store_host(RCX, 4, 4);
                if writeback {
                    self.set(rn, ADDR);
                }
            } else {
                self.load_host(D, 4, false, 0);
                self.load_host(E, 4, false, 4);
                self.set(rd, D);
                self.set(rd + 1, E);
                if writeback {
                    self.set(rn, ADDR);
                }
            }
        } else if load {
            self.load_host(D, if half { 2 } else { 1 }, signed, 0);
            self.set(rd, D);
            if writeback {
                self.set(rn, ADDR);
            }
        } else {
            self.get_to(rd, pc, RCX);
            self.store_host(RCX, 2, 0);
            if writeback {
                self.set(rn, ADDR);
            }
        }
        self.a.bind(resume);
        true
    }

    /// LDM/STM without the S bit: Some(block ends) or None.
    fn ldm_stm(&mut self, insn: u32, pc: u32, n: u32) -> Option<bool> {
        let (pre, up, s, wb, load) = (
            insn & (1 << 24) != 0,
            insn & (1 << 23) != 0,
            insn & (1 << 22) != 0,
            insn & (1 << 21) != 0,
            insn & (1 << 20) != 0,
        );
        let rn = (insn >> 16) & 0xf;
        let list = insn & 0xffff;
        if s || rn == 15 || list == 0 || !load && list & 0x8000 != 0 || load && wb && list & (1 << rn) != 0 {
            return None;
        }
        let bytes = list.count_ones() * 4;
        self.get_to(rn, pc, BASE);
        // the lowest address into ADDR
        let low = match (pre, up) {
            (false, true) => 0,
            (true, true) => 4,
            (false, false) => 4i32.wrapping_sub(bytes as i32),
            (true, false) => -(bytes as i32),
        };
        self.a.lea(ADDR, M(BASE, None, low));
        let (slow, resume) = self.slow_path(pc, n);
        self.same_page(ADDR, bytes, slow);
        self.probe(ADDR, 4, !load, slow);
        let mut k = 0;
        for r in 0..16 {
            if list & (1 << r) == 0 {
                continue;
            }
            if load {
                if r == 15 {
                    self.load_host(E, 4, false, 4 * k);
                } else {
                    self.load_host(D, 4, false, 4 * k);
                    self.set(r, D);
                }
            } else {
                self.get_to(r, pc, RCX);
                self.store_host(RCX, 4, 4 * k);
            }
            k += 1;
        }
        if wb {
            self.a.lea(BASE, M(BASE, None, if up { bytes as i32 } else { -(bytes as i32) }));
            self.set(rn, BASE);
        }
        if load && list & 0x8000 != 0 {
            self.exit_bx(E, n + 1);
            self.a.bind(resume);
            return Some(self.after_pc_load(insn, pc, n));
        }
        self.a.bind(resume);
        Some(false)
    }
}

// ---------------------------------------------------------------------- T32
impl X {
    /// Emit the Thumb instruction `hw1`(:`hw2`) at `pc`, the `n`-th of the basic block.
    /// Returns true when the block ends with it.
    fn thumb_insn(&mut self, hw1: u32, hw2: u32, pc: u32, n: u32) -> bool {
        let wide = hw1 >> 11 >= 0b11101;
        self.size = if wide { 4 } else { 2 };
        let it = self.it;
        let is_it = !wide && hw1 & 0xff00 == 0xbf00 && hw1 & 0xf != 0;
        if is_it {
            if it != 0 || (hw1 >> 4) & 0xf == 15 {
                self.call_interp(pc, n, true, 0);
                return true;
            }
            self.it = hw1 & 0xff;
            return false;
        }
        let guard = (it != 0 && it >> 4 != 14).then(|| {
            let l = self.a.label();
            self.jcond((it >> 4) ^ 1, l);
            l
        });
        let end = self.thumb_body(hw1, hw2, wide, pc, n);
        if it != 0 {
            // ITAdvance
            self.it = if it & 7 == 0 { 0 } else { it & 0xe0 | (it << 1) & 0x1f };
        }
        match guard {
            Some(l) => {
                self.a.bind(l);
                false
            }
            None => end,
        }
    }

    fn thumb_body(&mut self, hw1: u32, hw2: u32, wide: bool, pc: u32, n: u32) -> bool {
        let t = if wide { self.t32(hw1, hw2, pc, n) } else { self.t16(hw1, pc, n) };
        match t {
            T::A32(word) => match self.native(word, pc, n) {
                Some(end) => end,
                None => {
                    let end = writes_pc(word);
                    self.call_interp(pc, n, end, 0);
                    end
                }
            },
            T::Done(end) => end,
            T::Interp(end) => {
                self.call_interp(pc, n, end, 0);
                end
            }
        }
    }

    /// A conditional branch to `target`: leave if taken, otherwise continue the block.
    fn cond_branch(&mut self, cond: u32, target: u32, n: u32) -> T {
        let skip = self.a.label();
        self.jcond(cond ^ 1, skip);
        self.exit_const(target, n + 1);
        self.a.bind(skip);
        T::Done(false)
    }

    fn t16(&mut self, hw: u32, pc: u32, n: u32) -> T {
        // 16-bit ALU operations set the flags outside IT blocks only
        let s = if self.it == 0 { 1 << 20 } else { 0 };
        let (r0, r3, r6, r8) = (hw & 7, (hw >> 3) & 7, (hw >> 6) & 7, (hw >> 8) & 7);
        let imm8 = hw & 0xff;
        let pc4 = pc.wrapping_add(4);
        match hw >> 11 {
            0..=2 => {
                let imm5 = (hw >> 6) & 31;
                return T::A32(0xe1a0_0000 | s | r0 << 12 | imm5 << 7 | (hw >> 11) << 5 | r3);
            }
            3 => {
                let base = match (hw & (1 << 10) != 0, hw & (1 << 9) != 0) {
                    (false, false) => 0x0080_0000,
                    (false, true) => 0x0040_0000,
                    (true, false) => 0x0280_0000,
                    (true, true) => 0x0240_0000,
                };
                return T::A32(0xe000_0000 | base | s | r3 << 16 | r0 << 12 | r6);
            }
            4 => return T::A32(0xe3a0_0000 | s | r8 << 12 | imm8),
            5 => return T::A32(0xe350_0000 | r8 << 16 | imm8),
            6 => return T::A32(0xe280_0000 | s | r8 << 16 | r8 << 12 | imm8),
            7 => return T::A32(0xe240_0000 | s | r8 << 16 | r8 << 12 | imm8),
            _ => {}
        }
        match hw >> 10 {
            0b010000 => {
                let (rm, rdn) = (r3, r0);
                let word = match (hw >> 6) & 15 {
                    0 => s | rdn << 16 | rdn << 12 | rm,
                    1 => 0x0020_0000 | s | rdn << 16 | rdn << 12 | rm,
                    op @ (2 | 3 | 4 | 7) => {
                        let kind = match op {
                            2 => 0,
                            3 => 1,
                            4 => 2,
                            _ => 3,
                        };
                        0x01a0_0010 | s | rdn << 12 | rm << 8 | kind << 5 | rdn
                    }
                    5 => 0x00a0_0000 | s | rdn << 16 | rdn << 12 | rm,
                    6 => 0x00c0_0000 | s | rdn << 16 | rdn << 12 | rm,
                    8 => 0x0110_0000 | rdn << 16 | rm,
                    9 => 0x0260_0000 | s | rm << 16 | rdn << 12,
                    10 => 0x0150_0000 | rdn << 16 | rm,
                    11 => 0x0170_0000 | rdn << 16 | rm,
                    12 => 0x0180_0000 | s | rdn << 16 | rdn << 12 | rm,
                    13 => 0x0000_0090 | s | rdn << 16 | rdn << 8 | rm,
                    14 => 0x01c0_0000 | s | rdn << 16 | rdn << 12 | rm,
                    _ => 0x01e0_0000 | s | rdn << 12 | rm,
                };
                return T::A32(0xe000_0000 | word);
            }
            0b010001 => {
                let rd = (hw >> 4) & 8 | r0;
                let rm = (hw >> 3) & 15;
                return match (hw >> 8) & 3 {
                    0 if rd == 15 => {
                        if rm == 15 {
                            return T::Interp(true);
                        }
                        self.get_to(rm, pc, D);
                        self.a.alui(ADD, R(D), pc4);
                        self.exit_thumb_branch(D, n + 1);
                        T::Done(true)
                    }
                    0 => T::A32(0xe080_0000 | rd << 16 | rd << 12 | rm),
                    1 => T::A32(0xe150_0000 | rd << 16 | rm),
                    2 if rd == 15 => {
                        self.get_to(rm, pc, D);
                        self.exit_thumb_branch(D, n + 1);
                        T::Done(true)
                    }
                    2 => T::A32(0xe1a0_0000 | rd << 12 | rm),
                    _ => {
                        if rm == 15 {
                            return T::Interp(true);
                        }
                        self.get_to(rm, pc, D);
                        if hw & 0x80 != 0 {
                            self.set_const(14, pc.wrapping_add(2) | 1);
                        }
                        self.exit_bx(D, n + 1);
                        T::Done(true)
                    }
                };
            }
            _ => {}
        }
        match hw >> 12 {
            0b0100 => T::A32(0xe59f_0000 | r8 << 12 | imm8 * 4), // LDR literal (aligned PC)
            0b0101 => {
                let word = match (hw >> 9) & 7 {
                    0 => 0x0780_0000,
                    1 => 0x0180_00b0,
                    2 => 0x07c0_0000,
                    3 => 0x0190_00d0,
                    4 => 0x0790_0000,
                    5 => 0x0190_00b0,
                    6 => 0x07d0_0000,
                    _ => 0x0190_00f0,
                };
                T::A32(0xe000_0000 | word | r3 << 16 | r0 << 12 | r6)
            }
            0b0110 | 0b0111 => {
                let byte = hw & (1 << 12) != 0;
                let imm5 = (hw >> 6) & 31;
                let offset = if byte { imm5 } else { imm5 * 4 };
                let load = (hw >> 11) & 1;
                T::A32(0xe580_0000 | u32::from(byte) << 22 | load << 20 | r3 << 16 | r0 << 12 | offset)
            }
            0b1000 => {
                let offset = ((hw >> 6) & 31) * 2;
                let load = (hw >> 11) & 1;
                T::A32(0xe1c0_00b0 | load << 20 | r3 << 16 | r0 << 12 | (offset >> 4) << 8 | offset & 15)
            }
            0b1001 => T::A32(0xe58d_0000 | ((hw >> 11) & 1) << 20 | r8 << 12 | imm8 * 4),
            0b1010 => {
                if hw & (1 << 11) == 0 {
                    self.a.mov_ri(RES, (pc4 & !3).wrapping_add(imm8 * 4)); // ADR
                } else {
                    self.get_to(13, pc, RES);
                    self.a.alui(ADD, R(RES), imm8 * 4);
                }
                self.set(r8, RES);
                T::Done(false)
            }
            0b1011 => self.t16_misc(hw, pc, n),
            0b1100 => {
                let load = hw & (1 << 11) != 0;
                let writeback = !load || imm8 & (1 << r8) == 0;
                T::A32(0xe880_0000 | u32::from(writeback) << 21 | u32::from(load) << 20 | r8 << 16 | imm8)
            }
            0b1101 => match (hw >> 8) & 15 {
                14 | 15 => T::Interp(true), // UDF, SVC
                cond => self.cond_branch(cond, pc4.wrapping_add(sign_extend(imm8 << 1, 9)), n),
            },
            0b1110 if hw & 0x0800 == 0 => {
                self.exit_const(pc4.wrapping_add(sign_extend((hw & 0x7ff) << 1, 12)), n + 1);
                T::Done(true)
            }
            _ => T::Interp(true),
        }
    }

    fn t16_misc(&mut self, hw: u32, pc: u32, n: u32) -> T {
        let (r0, r3) = (hw & 7, (hw >> 3) & 7);
        match (hw >> 8) & 15 {
            0b0000 => {
                // ADD/SUB SP, SP, #imm7 * 4
                self.get_to(13, pc, RES);
                self.a.alui(if hw & 0x80 != 0 { SUB } else { ADD }, R(RES), (hw & 0x7f) * 4);
                self.set(13, RES);
                T::Done(false)
            }
            0b0001 | 0b0011 | 0b1001 | 0b1011 => {
                // CBZ/CBNZ: leave for the target if taken
                let offset = (hw >> 3) & 0x1f | (hw >> 4) & 0x20;
                let target = pc.wrapping_add(4).wrapping_add(offset << 1);
                self.get_to(r0, pc, RES);
                let skip = self.a.label();
                self.a.test_rr(RES, RES);
                self.a.jcc(if hw & (1 << 11) == 0 { CC_NE } else { CC_E }, skip);
                self.exit_const(target, n + 1);
                self.a.bind(skip);
                T::Done(false)
            }
            0b0010 => {
                let word = match (hw >> 6) & 3 {
                    0 => 0x06bf_0070,
                    1 => 0x06af_0070,
                    2 => 0x06ff_0070,
                    _ => 0x06ef_0070,
                };
                T::A32(0xe000_0000 | word | r0 << 12 | r3)
            }
            0b0100 | 0b0101 => T::A32(0xe92d_0000 | (hw & 0xff) | (hw & 0x100) << 6),
            0b1010 => match (hw >> 6) & 3 {
                0 => T::A32(0xe6bf_0f30 | r0 << 12 | r3),
                1 => T::A32(0xe6bf_0fb0 | r0 << 12 | r3),
                3 => T::A32(0xe6ff_0fb0 | r0 << 12 | r3),
                _ => T::Interp(true),
            },
            0b1100 | 0b1101 => T::A32(0xe8bd_0000 | (hw & 0xff) | (hw & 0x100) << 7),
            0b1111 if hw & 0xf == 0 => T::A32(0xe320_f000 | (hw >> 4) & 0xf), // hints
            // CPS, BKPT, IT (changes ITSTATE: the block ends), undefined
            _ => T::Interp(true),
        }
    }

    fn t32(&mut self, hw1: u32, hw2: u32, pc: u32, n: u32) -> T {
        let insn = hw1 << 16 | hw2;
        let op2 = (hw1 >> 4) & 0x7f;
        match (hw1 >> 11) & 3 {
            1 => {
                if op2 & 0x64 == 0x00 {
                    let (rn, w, l) = (hw1 & 15, (hw1 >> 5) & 1, (hw1 >> 4) & 1);
                    let mode = match (hw1 >> 7) & 3 {
                        1 => 0x0880_0000,
                        2 => 0x0900_0000,
                        _ => return T::Interp(true), // RFE, SRS
                    };
                    return T::A32(0xe000_0000 | mode | w << 21 | l << 20 | rn << 16 | hw2);
                }
                if op2 & 0x64 == 0x04 {
                    return match hw1 & 0xfff0 {
                        // exclusives, TBB/TBH
                        0xe840 | 0xe850 | 0xe8c0 => T::Interp(false),
                        0xe8d0 => T::Interp((hw2 >> 4) & 15 <= 1),
                        _ => self.t_ldrd(hw1, hw2, pc, n),
                    };
                }
                if op2 & 0x60 == 0x20 {
                    let op = (hw1 >> 5) & 15;
                    let (rn, rd, rm) = (hw1 & 15, (hw2 >> 8) & 15, hw2 & 15);
                    let imm5 = ((hw2 >> 12) & 7) << 2 | (hw2 >> 6) & 3;
                    let kind = (hw2 >> 4) & 3;
                    if op == 6 {
                        return T::A32(0xe680_0010 | rn << 16 | rd << 12 | imm5 << 7 | (kind & 2) << 5 | rm);
                    }
                    let s = hw1 & 0x10 != 0;
                    // the A32 shifted-register operand form computes the same value and carry
                    let Some(carry) = self.operand2(imm5 << 7 | kind << 5 | rm, pc, s && op <= 4) else {
                        return T::Interp(false);
                    };
                    return self.t_dp(op, s, rn, rd, carry, pc);
                }
                if hw1 & 0x0300 == 0x0300 {
                    return T::Interp(false); // Advanced SIMD
                }
                if hw1 >> 12 == 0xe {
                    T::A32(insn) // coprocessor (CP15 thread registers, cache maintenance)
                } else {
                    T::Interp(false)
                }
            }
            2 => {
                if hw2 & 0x8000 != 0 {
                    return self.t32_branch(hw1, hw2, pc, n);
                }
                let i = (hw1 >> 10) & 1;
                let (imm3, rd, rn, imm8) = ((hw2 >> 12) & 7, (hw2 >> 8) & 15, hw1 & 15, hw2 & 0xff);
                let imm12 = i << 11 | imm3 << 8 | imm8;
                if op2 & 0x20 == 0 {
                    // modified immediate
                    let (value, carry) = crate::thumb::expand_imm(imm12, false);
                    self.a.mov_ri(B, value);
                    let carry = if imm12 >> 10 == 0 { Carry::Unchanged } else { Carry::Const(carry) };
                    return self.t_dp((hw1 >> 5) & 15, hw1 & 0x10 != 0, rn, rd, carry, pc);
                }
                let imm16 = (hw1 & 15) << 12 | imm12;
                let shift = imm3 << 2 | (hw2 >> 6) & 3;
                let low5 = hw2 & 31;
                match (hw1 >> 4) & 0x1f {
                    op @ (0x00 | 0x0a) => {
                        // ADDW/SUBW, ADR
                        if rd == 15 {
                            return T::Interp(true);
                        }
                        let sub = op == 0x0a;
                        if rn == 15 {
                            let base = pc.wrapping_add(4) & !3;
                            self.a.mov_ri(RES, if sub { base.wrapping_sub(imm12) } else { base.wrapping_add(imm12) });
                        } else {
                            self.get_to(rn, pc, RES);
                            self.a.alui(if sub { SUB } else { ADD }, R(RES), imm12);
                        }
                        self.set(rd, RES);
                        T::Done(false)
                    }
                    0x04 => T::A32(0xe300_0000 | (imm16 >> 12) << 16 | rd << 12 | imm16 & 0xfff),
                    0x0c => T::A32(0xe340_0000 | (imm16 >> 12) << 16 | rd << 12 | imm16 & 0xfff),
                    0x14 => T::A32(0xe7a0_0050 | low5 << 16 | rd << 12 | shift << 7 | rn),
                    0x1c => T::A32(0xe7e0_0050 | low5 << 16 | rd << 12 | shift << 7 | rn),
                    0x16 => T::A32(0xe7c0_0010 | low5 << 16 | rd << 12 | shift << 7 | rn),
                    _ => T::Interp(false), // saturation
                }
            }
            _ => {
                if hw1 & 0xff00 == 0xff00 || hw1 & 0xff10 == 0xf900 {
                    return T::Interp(false); // Advanced SIMD
                }
                if op2 & 0x71 == 0x00 || op2 & 0x67 == 0x01 || op2 & 0x67 == 0x03 || op2 & 0x67 == 0x05 {
                    return self.t_ldst(hw1, hw2, pc, n);
                }
                if op2 & 0x70 == 0x20 {
                    return t32_dp_register(hw1, hw2);
                }
                if op2 & 0x78 == 0x30 {
                    return t32_multiply(hw1, hw2);
                }
                if op2 & 0x78 == 0x38 {
                    return t32_long_multiply(hw1, hw2);
                }
                T::Interp(true)
            }
        }
    }

    /// T32 data processing (modified immediate or shifted register) with operand 2 in `B`.
    /// `op` is the T32 opcode; Rd = PC with S selects TST/TEQ/CMN/CMP.
    fn t_dp(&mut self, op: u32, s: bool, rn: u32, rd: u32, carry: Carry, pc: u32) -> T {
        let compare = rd == 15 && s && matches!(op, 0 | 4 | 8 | 13);
        if rd == 15 && !compare || !matches!(op, 0..=4 | 8 | 10 | 11 | 13 | 14) {
            return T::Interp(true);
        }
        if rn == 15 {
            self.a.mov_ri(A, 0); // Rn = PC reads as zero here (ORR/ORN: MOV/MVN)
        } else {
            self.get_to(rn, pc, A);
        }
        let alu = match op {
            0 => Alu::And,
            1 => Alu::Bic,
            2 => Alu::Orr,
            3 => Alu::Orn,
            4 => Alu::Eor,
            8 => Alu::Add,
            10 => Alu::Adc,
            11 => Alu::Sbc,
            13 => Alu::Sub,
            _ => Alu::Rsb,
        };
        self.alu(alu, s, carry);
        if !compare {
            self.set(rd, RES);
        }
        T::Done(false)
    }

    fn t32_branch(&mut self, hw1: u32, hw2: u32, pc: u32, n: u32) -> T {
        let s = (hw1 >> 10) & 1;
        let (j1, j2) = ((hw2 >> 13) & 1, (hw2 >> 11) & 1);
        let pc4 = pc.wrapping_add(4);
        if (hw2 >> 12) & 5 == 0 {
            let cond = (hw1 >> 6) & 15;
            if cond < 14 {
                let imm = s << 20 | j2 << 19 | j1 << 18 | (hw1 & 0x3f) << 12 | (hw2 & 0x7ff) << 1;
                return self.cond_branch(cond, pc4.wrapping_add(sign_extend(imm, 21)), n);
            }
            return match (hw1 >> 4) & 0x7f {
                // NOP-class hints
                0x3a if hw2 & 0x700 == 0 => T::A32(0xe320_f000 | hw2 & 0xff),
                // DMB/DSB/ISB
                0x3b if matches!((hw2 >> 4) & 15, 4..=6) => {
                    if self.mt && matches!((hw2 >> 4) & 15, 4 | 5) {
                        self.a.mfence();
                    }
                    T::Done(false)
                }
                0x3b => T::Interp(false), // CLREX
                // MRS
                0x3e | 0x3f => T::A32(0xe10f_0000 | ((hw1 >> 4) & 1) << 22 | ((hw2 >> 8) & 15) << 12),
                _ => T::Interp(true), // MSR, CPS, SUBS PC, LR, SMC
            };
        }
        let i1 = !(j1 ^ s) & 1;
        let i2 = !(j2 ^ s) & 1;
        let imm = s << 24 | i1 << 23 | i2 << 22 | (hw1 & 0x3ff) << 12 | (hw2 & 0x7ff) << 1;
        let offset = sign_extend(imm, 25);
        match (hw2 >> 12) & 5 {
            1 => {
                self.exit_const(pc4.wrapping_add(offset), n + 1);
                T::Done(true)
            }
            5 => {
                self.set_const(14, pc4 | 1);
                self.exit_const(pc4.wrapping_add(offset), n + 1);
                T::Done(true)
            }
            _ => {
                if hw2 & 1 != 0 {
                    return T::Interp(true);
                }
                // BLX immediate: to ARM code
                self.set_const(14, pc4 | 1);
                self.set_const(15, (pc4 & !3).wrapping_add(offset) & !3);
                self.set_t(false);
                self.flags_save();
                self.add_instret(n + 1);
                self.leave();
                T::Done(true)
            }
        }
    }

    /// LDR/STR (byte, halfword, word; signed loads) in the T32 immediate, indexed, register
    /// and literal forms.
    fn t_ldst(&mut self, hw1: u32, hw2: u32, pc: u32, n: u32) -> T {
        let size = match (hw1 >> 5) & 3 {
            0 => 1,
            1 => 2,
            2 => 4,
            _ => return T::Interp(true),
        };
        let (signed, load, rn, rt) = (hw1 & 0x100 != 0, hw1 & 0x10 != 0, hw1 & 15, hw2 >> 12);
        if !load && (signed || rt == 15) {
            return T::Interp(true);
        }
        if load && rt == 15 && size != 4 {
            return T::Done(false); // PLD/PLI
        }
        // (the accessed address's register, writeback)
        let (a, writeback) = if rn == 15 {
            if !load {
                return T::Interp(true);
            }
            let base = pc.wrapping_add(4) & !3;
            let imm = hw2 & 0xfff;
            self.a.mov_ri(ADDR, if hw1 & 0x80 != 0 { base.wrapping_add(imm) } else { base.wrapping_sub(imm) });
            (ADDR, false)
        } else if hw1 & 0x80 != 0 {
            self.get_to(rn, pc, ADDR);
            self.a.alui(ADD, R(ADDR), hw2 & 0xfff);
            (ADDR, false)
        } else if hw2 & 0x800 != 0 {
            let (p, u, w) = (hw2 & 0x400 != 0, hw2 & 0x200 != 0, hw2 & 0x100 != 0);
            if !p && !w || w && load && rt == rn {
                return T::Interp(true);
            }
            self.get_to(rn, pc, BASE);
            let imm = (hw2 & 0xff) as i32;
            self.a.lea(ADDR, M(BASE, None, if u { imm } else { -imm }));
            (if p { ADDR } else { BASE }, w)
        } else if hw2 & 0xfc0 == 0 {
            let rm = hw2 & 15;
            if rm == 15 {
                return T::Interp(true);
            }
            self.get_to(rn, pc, ADDR);
            self.get_to(rm, pc, RCX);
            let sh = (hw2 >> 4) & 3;
            if sh != 0 {
                self.a.shift(SHL, R(RCX), sh);
            }
            self.a.alu(ADD, R(ADDR), RCX);
            (ADDR, false)
        } else {
            return T::Interp(true);
        };
        let (slow, resume) = self.slow_path(pc, n);
        self.probe(a, size, !load, slow);
        if load {
            self.load_host(D, size, signed, 0);
            if writeback {
                self.set(rn, ADDR);
            }
            if rt == 15 {
                self.exit_bx(D, n + 1);
                self.a.bind(resume);
                return T::Done(self.after_pc_load(0xe000_0000, pc, n));
            }
            self.set(rt, D);
        } else {
            self.get_to(rt, pc, RCX);
            self.store_host(RCX, size, 0);
            if writeback {
                self.set(rn, ADDR);
            }
        }
        self.a.bind(resume);
        T::Done(false)
    }

    /// LDRD/STRD (immediate) with any register pair.
    fn t_ldrd(&mut self, hw1: u32, hw2: u32, pc: u32, n: u32) -> T {
        let (p, u, w, l) = (hw1 & 0x100 != 0, hw1 & 0x80 != 0, hw1 & 0x20 != 0, hw1 & 0x10 != 0);
        let (rn, rt, rt2) = (hw1 & 15, hw2 >> 12, (hw2 >> 8) & 15);
        if !p && !w || rt == 15 || rt2 == 15 || w && rn == 15 || !l && rn == 15 {
            return T::Interp(true);
        }
        let imm = (hw2 & 0xff) << 2;
        let a = self.address(rn, pc, p, u, Some(imm));
        let (slow, resume) = self.slow_path(pc, n);
        self.same_page(a, 8, slow);
        self.probe(a, 4, !l, slow);
        if l {
            self.load_host(D, 4, false, 0);
            self.load_host(E, 4, false, 4);
            if w {
                self.set(rn, ADDR);
            }
            self.set(rt, D);
            self.set(rt2, E);
        } else {
            self.get_to(rt, pc, RCX);
            self.store_host(RCX, 4, 0);
            self.get_to(rt2, pc, RCX);
            self.store_host(RCX, 4, 4);
            if w {
                self.set(rn, ADDR);
            }
        }
        self.a.bind(resume);
        T::Done(false)
    }
}

// ---------------------------------------------------------------------- regions
impl X {
    fn prologue(&mut self, rtab: u32, wtab: u32) {
        let utlb = self.cpu(self.o.utlb);
        let a = &mut self.a;
        // five pushes after the return address: rsp stays 16-byte aligned for h_step
        for r in [RBX, RBP, R12, R13, R14] {
            a.push(r);
        }
        a.mov64_rr(CPU, RDI);
        a.mov64_rm(RTAB, utlb);
        a.lea64(WTAB, M(RTAB, None, wtab as i32));
        a.lea64(RTAB, M(RTAB, None, rtab as i32));
        self.flags_load();
    }

    /// The slow paths, the shared exits, and the patched code.
    fn finish(mut self) -> Vec<u8> {
        for s in std::mem::take(&mut self.slow) {
            self.a.bind(s.label);
            self.size = s.size;
            self.it = s.it;
            self.call_interp(s.pc, s.before, false, 1);
            self.a.jmp(s.resume);
        }
        let a = &mut self.a;
        a.bind(self.exit_h);
        a.unary(NEG, R(RAX));
        a.alui(ADD, R(RAX), 3);
        a.bind(self.epi);
        for r in [R14, R13, R12, RBP, RBX] {
            a.pop(r);
        }
        a.ret();
        self.a.finish()
    }
}

impl Cpu {
    /// Translate the region at virtual `pc` (host address `code`) into x86-64 code.
    pub(super) fn compile_x64(&mut self, pc: u32, privileged: bool, thumb: bool, code: *const u8) -> Option<BlockFn> {
        let mut a = Asm::new();
        let (epi, exit_h) = (a.label(), a.label());
        let mut x = X {
            a,
            o: ctx(),
            thumb,
            size: if thumb { 2 } else { 4 },
            it: 0,
            el0: self.el1.is_some(),
            mt: self.mt,
            chain: self.jit.as_ref().is_some_and(|j| j.disp != 0),
            region: Vec::new(),
            bb: Vec::new(),
            slow: Vec::new(),
            epi,
            exit_h,
        };
        // SAFETY: `code` maps this page of guest RAM: every read below stays in it
        let page = code.wrapping_sub((pc & 0xfff) as usize);
        let rd16 = |at: u32| -> u32 { unsafe { (page.add((at & 0xfff) as usize) as *const u16).read_unaligned() as u32 } };
        let rd32 = |at: u32| -> u32 { unsafe { (page.add((at & 0xfff) as usize) as *const u32).read_unaligned() } };
        // ---- the region: basic blocks reachable through constant branches on this page
        let mut bbs = vec![pc];
        let mut q = 0;
        while q < bbs.len() && bbs.len() < MAX_BBS {
            let mut at = bbs[q];
            q += 1;
            for _ in 0..MAX_INSNS {
                let (target, stop, size) = if thumb {
                    let hw1 = rd16(at);
                    let wide = hw1 >> 11 >= 0b11101;
                    if wide && at & 0xfff == 0xffe {
                        break;
                    }
                    let (t, s) = scan_t(hw1, if wide { rd16(at + 2) } else { 0 }, at);
                    (t, s, if wide { 4 } else { 2 })
                } else {
                    let (t, s) = scan_a32(rd32(at), at);
                    (t, s, 4)
                };
                if let Some(t) = target {
                    let aligned = if thumb { t & 1 == 0 } else { t & 3 == 0 };
                    if aligned && t & !0xfff == pc & !0xfff && bbs.len() < MAX_BBS && !bbs.contains(&t) {
                        bbs.push(t);
                    }
                }
                at = at.wrapping_add(size);
                if stop || at & 0xfff == 0 {
                    break;
                }
            }
        }
        x.bb = bbs.iter().map(|_| x.a.label()).collect();
        x.region = bbs.clone();
        x.prologue(
            (utlb_table(privileged, U_READ) * UT * 16) as u32,
            (utlb_table(privileged, U_WRITE) * UT * 16) as u32,
        );
        // the dispatcher enters past the prologue: the offset is stored just before the entry
        let body = x.a.c.len() as u32;
        for (k, &start) in bbs.iter().enumerate() {
            let l = x.bb[k];
            x.a.bind(l);
            x.it = 0;
            let mut n = 0u32;
            let mut at = start;
            if thumb {
                loop {
                    let hw1 = rd16(at);
                    let wide = hw1 >> 11 >= 0b11101;
                    if wide && at & 0xfff == 0xffe {
                        // straddles the page: leave it to the interpreter
                        if n == 0 && k == 0 {
                            return None;
                        }
                        // never through the region: this may be the basic block's own start
                        if x.it != 0 {
                            let it = x.it;
                            x.write_itstate(it);
                        }
                        x.add_instret(n);
                        x.exit_out_counted(at);
                        break;
                    }
                    let hw2 = if wide { rd16(at + 2) } else { 0 };
                    let end = x.thumb_insn(hw1, hw2, at, n);
                    n += 1;
                    at = at.wrapping_add(if wide { 4 } else { 2 });
                    if end {
                        break;
                    }
                    if n >= MAX_INSNS && x.it == 0 || at & 0xfff == 0 {
                        if x.it != 0 {
                            let it = x.it;
                            x.write_itstate(it);
                            x.add_instret(n);
                            x.exit_out_counted(at);
                        } else {
                            x.exit_const(at, n);
                        }
                        break;
                    }
                }
            } else {
                loop {
                    let end = x.insn(rd32(at), at, n);
                    n += 1;
                    at = at.wrapping_add(4);
                    if end {
                        break;
                    }
                    if n >= MAX_INSNS || at & 0xfff == 0 {
                        x.exit_const(at, n);
                        break;
                    }
                }
            }
        }
        let mut bytes = body.to_le_bytes().to_vec();
        bytes.extend(x.finish());
        let jit = self.jit.as_mut()?;
        let entry = match jit.code.write_bytes(&bytes) {
            Some(p) => p,
            None => {
                jit.flush();
                jit.code.write_bytes(&bytes)?
            }
        };
        // SAFETY: `entry` + 4 is the start of the code just written, which follows the
        // BlockFn ABI (System V: the Cpu pointer in rdi, the status in eax)
        Some(unsafe { std::mem::transmute::<*const u8, BlockFn>(entry.wrapping_add(4)) })
    }
}

/// The chaining dispatcher, entered by a region's native exit with the guest flags saved,
/// its instructions counted and `r[15]` set (rbx = Cpu, r12/r13 = this privilege's micro-TLB
/// tables, r14 = the flags: native exits never change the mode). It jumps past the prologue
/// of the next compiled region while `instret < chain_until`, the core is outside an IT
/// block and the fetch micro-TLB and front cache know the target; otherwise it returns 0
/// to `jit_run`. The same code as the AArch64 backend's dispatcher.
pub(super) fn dispatcher(o: &Ctx) -> Vec<u8> {
    let mut a = Asm::new();
    let out = a.label();
    let m = |off: usize| M(CPU, None, off as i32);
    a.mov64_rm(RAX, m(o.instret));
    a.alu64_rm(CMP, RAX, m(o.chain_until));
    a.jcc(CC_AE, out);
    a.mov_rm(RAX, m(o.cpsr));
    a.test_ri(R(RAX), 0x0600_fc00);
    a.jcc(CC_NE, out); // the interpreter runs IT blocks
    a.mov_rm(RDX, m(o.r + 60));
    // the fetch micro-TLB: two tables after this privilege's read table
    let exec = (2 * UT * 16) as i32;
    a.mov_rr(RCX, RDX);
    a.shift(SHR, R(RCX), 8);
    a.alui(AND, R(RCX), ((UT as u32) - 1) << 4);
    a.mov_rr(R10, RDX);
    a.alui(AND, R(R10), 0xffff_f000);
    a.alu64_rm(CMP, R10, M(RTAB, Some(RCX), exec));
    a.jcc(CC_NE, out);
    a.mov64_rm(R10, M(RTAB, Some(RCX), exec + 8));
    a.alu64(ADD, R(R10), RDX);
    a.alu64_rm(SUB, R10, m(o.ram));
    a.alu_rm(ADD, R10, m(o.ram_base)); // the physical PC (upper half zero)
    // key = pa | privileged << 32 | thumb << 33
    a.mov_rr(RCX, RAX);
    a.alui(AND, R(RCX), 0x1f);
    a.alui(CMP, R(RCX), 0x10);
    a.setcc(CC_NE, RCX);
    a.movzx8(RCX, R(RCX));
    a.shift64(SHL, R(RCX), 32);
    a.alu64(OR, R(R10), RCX);
    a.mov_rr(RCX, RAX);
    a.alui(AND, R(RCX), 1 << 5);
    a.shift64(SHL, R(RCX), 28);
    a.alu64(OR, R(R10), RCX);
    // front cache slot ((pc ^ pc >> 14) >> 2) & (FRONT - 1), 24-byte entries
    a.mov_rr(RCX, RDX);
    a.shift(SHR, R(RCX), 14);
    a.alu(XOR, R(RCX), RDX);
    a.shift(SHR, R(RCX), 2);
    a.alui(AND, R(RCX), super::FRONT as u32 - 1);
    a.mov64_rr(RAX, RCX);
    a.shift64(SHL, R(RCX), 1);
    a.alu64(ADD, R(RCX), RAX);
    a.shift64(SHL, R(RCX), 3);
    a.alu64_rm(ADD, RCX, m(o.front));
    a.alu_rm(CMP, RDX, M(RCX, None, 0));
    a.jcc(CC_NE, out);
    a.alu64_rm(CMP, R10, M(RCX, None, 8));
    a.jcc(CC_NE, out);
    a.mov64_rm(RAX, M(RCX, None, 16));
    a.mov_rm(RCX, M(RAX, None, -4)); // the region's body offset
    a.alu64(ADD, R(RAX), RCX);
    a.jmp_r(RAX);
    a.bind(out);
    a.mov_ri(RAX, 0);
    for r in [R14, R13, R12, RBP, RBX] {
        a.pop(r);
    }
    a.ret();
    a.finish()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Encodings checked against LLVM's assembler (`llvm-mc -triple=x86_64 -show-encoding`).
    #[test]
    fn encodings_match_the_assembler() {
        let cases: Vec<(&str, Box<dyn Fn(&mut Asm)>, &str)> = vec![
            ("mov ecx, r14d", Box::new(|a| a.mov_rr(RCX, R14)), "4489f1"),
            ("mov r14d, ecx", Box::new(|a| a.mov_rr(R14, RCX)), "4189ce"),
            ("mov rdi, rbx", Box::new(|a| a.mov64_rr(RDI, RBX)), "4889df"),
            ("mov eax, dword ptr [rbx + 0x10]", Box::new(|a| a.mov_rm(RAX, M(RBX, None, 0x10))), "8b4310"),
            ("mov r11d, dword ptr [rbx + 0x400]", Box::new(|a| a.mov_rm(R11, M(RBX, None, 0x400))), "448b9b00040000"),
            ("mov dword ptr [rbx + 0x3c], ecx", Box::new(|a| a.mov_mr(M(RBX, None, 0x3c), RCX)), "894b3c"),
            ("mov rcx, qword ptr [rbx + 0x208]", Box::new(|a| a.mov64_rm(RCX, M(RBX, None, 0x208))), "488b8b08020000"),
            ("mov r10, qword ptr [r12 + rcx + 8]", Box::new(|a| a.mov64_rm(R10, M(R12, Some(RCX), 8))), "4d8b540c08"),
            ("mov r10, qword ptr [r13 + rcx + 8]", Box::new(|a| a.mov64_rm(R10, M(R13, Some(RCX), 8))), "4d8b540d08"),
            ("cmp r10, qword ptr [r12 + rcx]", Box::new(|a| a.alu64_rm(CMP, R10, M(R12, Some(RCX), 0))), "4d3b140c"),
            ("cmp r10, qword ptr [r13 + rcx]", Box::new(|a| a.alu64_rm(CMP, R10, M(R13, Some(RCX), 0))), "4d3b540d00"),
            ("mov byte ptr [r10], cl", Box::new(|a| a.mov8_mr(M(R10, None, 0), RCX)), "41880a"),
            ("mov byte ptr [r10], dil", Box::new(|a| a.mov8_mr(M(R10, None, 0), RDI)), "41883a"),
            ("mov word ptr [r10 + 4], cx", Box::new(|a| a.mov16_mr(M(R10, None, 4), RCX)), "6641894a04"),
            ("mov dword ptr [r10 + 0x3c], ecx", Box::new(|a| a.mov_mr(M(R10, None, 0x3c), RCX)), "41894a3c"),
            ("mov r9d, 0x12345678", Box::new(|a| a.mov_ri(R9, 0x1234_5678)), "41b978563412"),
            ("mov eax, 0xffffffff", Box::new(|a| a.mov_ri(RAX, u32::MAX)), "b8ffffffff"),
            ("movabs rax, 0x123456789abcdef0", Box::new(|a| a.mov64_ri(RAX, 0x1234_5678_9abc_def0)), "48b8f0debc9a78563412"),
            ("mov dword ptr [rbx + 0x3c], 0x8000", Box::new(|a| a.mov_mi(M(RBX, None, 0x3c), 0x8000)), "c7433c00800000"),
            ("movzx edi, byte ptr [r10]", Box::new(|a| a.movzx8(RDI, M(R10, None, 0))), "410fb63a"),
            ("movsx edi, byte ptr [r10]", Box::new(|a| a.movsx8(RDI, M(R10, None, 0))), "410fbe3a"),
            ("movzx edi, word ptr [r10]", Box::new(|a| a.movzx16(RDI, M(R10, None, 0))), "410fb73a"),
            ("movsx edi, word ptr [r10]", Box::new(|a| a.movsx16(RDI, M(R10, None, 0))), "410fbf3a"),
            ("movzx eax, al", Box::new(|a| a.movzx8(RAX, R(RAX))), "0fb6c0"),
            ("movsx eax, ax", Box::new(|a| a.movsx16(RAX, R(RAX))), "0fbfc0"),
            ("movsxd rdx, r11d", Box::new(|a| a.movsxd(RDX, R(R11))), "4963d3"),
            ("movsxd rax, dword ptr [rbx + 0x14]", Box::new(|a| a.movsxd(RAX, M(RBX, None, 0x14))), "48634314"),
            ("lea ecx, [r9 + 7]", Box::new(|a| a.lea(RCX, M(R9, None, 7))), "418d4907"),
            ("lea r9d, [r8 - 0xfff]", Box::new(|a| a.lea(R9, M(R8, None, -0xfff))), "458d8801f0ffff"),
            ("lea r13, [r12 + 0x1000]", Box::new(|a| a.lea64(R13, M(R12, None, 0x1000))), "4d8dac2400100000"),
            ("lea r12, [r12]", Box::new(|a| a.lea64(R12, M(R12, None, 0))), "4d8d2424"),
            ("cmove ebp, edi", Box::new(|a| a.cmov(CC_E, RBP, R(RDI))), "0f44ef"),
            ("cmovae ecx, edi", Box::new(|a| a.cmov(CC_AE, RCX, R(RDI))), "0f43cf"),
            ("add eax, edx", Box::new(|a| a.alu(ADD, R(RAX), RDX)), "01d0"),
            ("sub eax, esi", Box::new(|a| a.alu(SUB, R(RAX), RSI)), "29f0"),
            ("adc eax, edx", Box::new(|a| a.alu(ADC, R(RAX), RDX)), "11d0"),
            ("sbb eax, esi", Box::new(|a| a.alu(SBB, R(RAX), RSI)), "19f0"),
            ("or r14d, ebp", Box::new(|a| a.alu(OR, R(R14), RBP)), "4109ee"),
            ("or dword ptr [rbx + 0x40], r10d", Box::new(|a| a.alu(OR, M(RBX, None, 0x40), R10)), "44095340"),
            ("add rax, rdx", Box::new(|a| a.alu64(ADD, R(RAX), RDX)), "4801d0"),
            ("add r10, r9", Box::new(|a| a.alu64(ADD, R(R10), R9)), "4d01ca"),
            ("add eax, dword ptr [rbx + 8]", Box::new(|a| a.alu_rm(ADD, RAX, M(RBX, None, 8))), "034308"),
            ("cmp rcx, qword ptr [rbx + 0x210]", Box::new(|a| a.alu64_rm(CMP, RCX, M(RBX, None, 0x210))), "483b8b10020000"),
            ("and r14d, 0x8c1", Box::new(|a| a.alui(AND, R(R14), 0x8c1)), "4181e6c1080000"),
            ("and ecx, 0xc0", Box::new(|a| a.alui(AND, R(RCX), 0xc0)), "81e1c0000000"),
            ("and ecx, 0xfffffffe", Box::new(|a| a.alui(AND, R(RCX), 0xffff_fffe)), "83e1fe"),
            ("and r10d, 0xfffff003", Box::new(|a| a.alui(AND, R(R10), 0xffff_f003)), "4181e203f0ffff"),
            ("cmp eax, 1", Box::new(|a| a.alui(CMP, R(RAX), 1)), "83f801"),
            ("and dword ptr [rbx + 0x40], 0xfffffff", Box::new(|a| a.alui(AND, M(RBX, None, 0x40), 0x0fff_ffff)), "816340ffffff0f"),
            ("or dword ptr [rbx + 0x40], 0x20", Box::new(|a| a.alui(OR, M(RBX, None, 0x40), 0x20)), "834b4020"),
            ("add qword ptr [rbx + 0x200], 5", Box::new(|a| a.alui64(ADD, M(RBX, None, 0x200), 5)), "4883830002000005"),
            ("test eax, eax", Box::new(|a| a.test_rr(RAX, RAX)), "85c0"),
            ("test r14d, 0x40", Box::new(|a| a.test_ri(R(R14), 0x40)), "41f7c640000000"),
            ("shl r10d, 0x18", Box::new(|a| a.shift(SHL, R(R10), 24)), "41c1e218"),
            ("shr ecx, 8", Box::new(|a| a.shift(SHR, R(RCX), 8)), "c1e908"),
            ("sar edx, 0x1f", Box::new(|a| a.shift(SAR, R(RDX), 31)), "c1fa1f"),
            ("ror eax, 0x10", Box::new(|a| a.shift(ROR, R(RAX), 16)), "c1c810"),
            ("shl rdx, 0x20", Box::new(|a| a.shift64(SHL, R(RDX), 32)), "48c1e220"),
            ("shr rbp, 0x20", Box::new(|a| a.shift64(SHR, R(RBP), 32)), "48c1ed20"),
            ("ror edx, cl", Box::new(|a| a.shift_cl(ROR, R(RDX))), "d3ca"),
            ("shl rdx, cl", Box::new(|a| a.shift64_cl(SHL, R(RDX))), "48d3e2"),
            ("sar rdx, cl", Box::new(|a| a.shift64_cl(SAR, R(RDX))), "48d3fa"),
            ("not eax", Box::new(|a| a.unary(NOT, R(RAX))), "f7d0"),
            ("neg eax", Box::new(|a| a.unary(NEG, R(RAX))), "f7d8"),
            ("div ecx", Box::new(|a| a.unary(DIV, R(RCX))), "f7f1"),
            ("idiv ecx", Box::new(|a| a.unary(IDIV, R(RCX))), "f7f9"),
            ("imul eax, dword ptr [rbx + 0xc]", Box::new(|a| a.imul(RAX, M(RBX, None, 0xc))), "0faf430c"),
            ("imul rax, rcx", Box::new(|a| a.imul64(RAX, R(RCX))), "480fafc1"),
            ("bswap eax", Box::new(|a| a.bswap(RAX)), "0fc8"),
            ("bsr eax, dword ptr [rbx + 4]", Box::new(|a| a.bsr(RAX, M(RBX, None, 4))), "0fbd4304"),
            ("bt r14d, 0", Box::new(|a| a.bt(R(R14), 0)), "410fbae600"),
            ("push r12", Box::new(|a| a.push(R12)), "4154"),
            ("push rbx", Box::new(|a| a.push(RBX)), "53"),
            ("pop r14", Box::new(|a| a.pop(R14)), "415e"),
            ("pop rcx", Box::new(|a| a.pop(RCX)), "59"),
            ("pushfq", Box::new(|a| a.pushfq()), "9c"),
            ("cmc", Box::new(|a| a.cmc()), "f5"),
            ("cdq", Box::new(|a| a.cdq()), "99"),
            ("mfence", Box::new(|a| a.mfence()), "0faef0"),
            ("call rax", Box::new(|a| a.call_r(RAX)), "ffd0"),
            ("jmp rax", Box::new(|a| a.jmp_r(RAX)), "ffe0"),
            ("setne cl", Box::new(|a| a.setcc(CC_NE, RCX)), "0f95c1"),
            ("movzx ecx, cl", Box::new(|a| a.movzx8(RCX, R(RCX))), "0fb6c9"),
            ("cmp r10, qword ptr [r12 + rcx + 0x2000]", Box::new(|a| a.alu64_rm(CMP, R10, M(R12, Some(RCX), 0x2000))), "4d3b940c00200000"),
            ("mov ecx, dword ptr [rax - 4]", Box::new(|a| a.mov_rm(RCX, M(RAX, None, -4))), "8b48fc"),
            ("cmp edx, dword ptr [rcx]", Box::new(|a| a.alu_rm(CMP, RDX, M(RCX, None, 0))), "3b11"),
            ("ret", Box::new(|a| a.ret()), "c3"),
        ];
        for (text, emit, want) in &cases {
            let mut a = Asm::new();
            emit(&mut a);
            let got: String = a.c.iter().map(|b| format!("{b:02x}")).collect();
            assert_eq!(&got, want, "{text}");
        }
    }

    #[test]
    fn branches_are_patched() {
        let mut a = Asm::new();
        let (back, fwd) = (a.label(), a.label());
        a.bind(back);
        a.jcc(CC_NE, fwd); // 6 bytes
        a.jmp(back); // 5 bytes
        a.bind(fwd);
        a.ret();
        let c = a.finish();
        assert_eq!(c, [0x0f, 0x85, 5, 0, 0, 0, 0xe9, 0xf5, 0xff, 0xff, 0xff, 0xc3]);
    }
}
