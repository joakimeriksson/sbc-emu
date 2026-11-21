//! The block JIT's WebAssembly backend (browser builds): RISC-V RV64GC to wasm.
//!
//! A region becomes a module with one function `(param $hart i32) (result i32)` importing the
//! emulator's linear memory and function table. The page (web/worker.js) compiles the bytes
//! synchronously and installs the function in the table (`sbc_jit_compile`); Rust calls it
//! through a function pointer, which in wasm is the table index.
//!
//! - **Region.** The entry basic block plus the blocks that constant branches and jumps on
//!   the same page reach (up to `MAX_BBS`), in a `loop` + `br_table` dispatch, so loops run
//!   inside wasm while the cycle budget (`chain_until`) lasts.
//! - **Registers** stay in the hart (`x[]`, i64 loads and stores); `x0` reads as 0 and
//!   writes to it are dropped.
//! - **PCs** are the page's virtual address at entry (`pc & !0xfff`) plus constants: the
//!   same physical page may be mapped at other virtual addresses.
//! - **Loads and stores** probe the micro-TLB of the current mode (`jit_ut`, see `mmu.rs`):
//!   a hit is a direct wasm memory access; a miss, a misaligned access, a device access or
//!   a page fault goes to the interpreter (`h_step`), which fills the micro-TLB.
//! - **Compiled inline:** RV64I (except system instructions), M (except `mulh*`), Zba and
//!   Zbb (except `orc.b` and `rev8`). Everything else (CSRs, `fence.i`, AMOs, F and D) runs in
//!   the interpreter through `h_step`; those that can change the mode, the address space or
//!   interrupt lines end the block.
use crate::mmu::{UEntry, UT};
use crate::{rvc, Hart};
use std::mem::{offset_of, size_of};

#[link(wasm_import_module = "env")]
extern "C" {
    /// Provided by the page: compile a module, install its export "f" in the function table,
    /// return the table index (or -1).
    fn sbc_jit_compile(ptr: *const u8, len: usize) -> i32;
    /// All compiled code was discarded: the page may reuse every slot it installed.
    fn sbc_jit_reset();
}

/// Tell the page that all compiled code is gone.
pub(crate) fn reset() {
    // SAFETY: the import takes no arguments
    unsafe { sbc_jit_reset() }
}

/// Basic blocks per compiled region.
const MAX_BBS: usize = 24;
/// Instructions per basic block.
const MAX_INSNS: u32 = 64;

// Locals: the hart pointer (parameter), then i32 and i64 temporaries.
const L_CPU: u32 = 0;
const L_HOST: u32 = 1; // host address of a load or store
const L_ST: u32 = 2; // h_step status
const L_IDX: u32 = 3; // the region's next basic block
const L_UT: u32 = 4; // micro-TLB table of the current mode
const L_E: u32 = 5; // micro-TLB entry
const I32_LOCALS: u32 = 5;
const L_PAGE: u32 = 6; // virtual address of the page at entry
const L_A: u32 = 7;
const L_B: u32 = 8;
const L_ADDR: u32 = 9; // virtual address of a load or store
const I64_LOCALS: u32 = 4;

// wasm opcodes
const I32_LOAD: u8 = 0x28;
const I64_LOAD: u8 = 0x29;
const I64_STORE: u8 = 0x37;
const I32_EQZ: u8 = 0x45;
const I32_ADD: u8 = 0x6a;
const I32_AND: u8 = 0x71;
const I32_SHL: u8 = 0x74;
const I32_SHR_U: u8 = 0x76;
const I64_EQZ: u8 = 0x50;
const I64_EQ: u8 = 0x51;
const I64_NE: u8 = 0x52;
const I64_LT_S: u8 = 0x53;
const I64_LT_U: u8 = 0x54;
const I64_GE_S: u8 = 0x59;
const I64_GE_U: u8 = 0x5a;
const I64_ADD: u8 = 0x7c;
const I64_SUB: u8 = 0x7d;
const I64_MUL: u8 = 0x7e;
const I64_AND: u8 = 0x83;
const I64_OR: u8 = 0x84;
const I64_XOR: u8 = 0x85;
const I64_SHL: u8 = 0x86;
const I64_SHR_S: u8 = 0x87;
const I64_SHR_U: u8 = 0x88;
const I32_WRAP: u8 = 0xa7;
const I64_EXT_S: u8 = 0xac; // i64.extend_i32_s
const I64_EXT_U: u8 = 0xad; // i64.extend_i32_u
const SELECT: u8 = 0x1b;
const RESULT_I64: u8 = 0x7e;
const RESULT_I32: u8 = 0x7f;

fn uleb(out: &mut Vec<u8>, mut v: u64) {
    loop {
        let b = (v & 0x7f) as u8;
        v >>= 7;
        if v == 0 {
            out.push(b);
            break;
        }
        out.push(b | 0x80);
    }
}

fn sleb(out: &mut Vec<u8>, mut v: i64) {
    loop {
        let b = (v & 0x7f) as u8;
        v >>= 7;
        let done = (v == 0 && b & 0x40 == 0) || (v == -1 && b & 0x40 != 0);
        out.push(if done { b } else { b | 0x80 });
        if done {
            break;
        }
    }
}

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

/// Where control goes after an instruction, for the region scan: a constant target (page
/// offset relative, may leave the page) and whether the basic block ends.
fn scan(i: u32, at: u32) -> (Option<i64>, bool) {
    match i & 0x7f {
        0x6f => (Some(at as i64 + imm_j(i)), true),
        0x67 => (None, true),
        0x63 => (Some(at as i64 + imm_b(i)), false),
        0x73 => (None, true),
        0x0f if f3(i) != 0 => (None, true),
        _ if i == 0 => (None, true), // an illegal compressed instruction
        _ => (None, false),
    }
}

/// Field offsets in `Hart`.
struct Off {
    x: u32,
    pc: u32,
    instret: u32,
    cycle: u32,
    ut: u32,
    chain: u32,
}

struct W {
    c: Vec<u8>,
    o: Off,
    /// table index of `h_step`
    step_idx: u32,
    /// page offsets of the region's basic blocks (index = dispatch value)
    region: Vec<u32>,
    cur_bb: usize,
    /// structured blocks open in the current basic block's code
    nest: u32,
}

impl W {
    fn op(&mut self, b: u8) {
        self.c.push(b);
    }
    fn lget(&mut self, l: u32) {
        self.op(0x20);
        uleb(&mut self.c, l as u64);
    }
    fn lset(&mut self, l: u32) {
        self.op(0x21);
        uleb(&mut self.c, l as u64);
    }
    fn i32c(&mut self, v: i32) {
        self.op(0x41);
        sleb(&mut self.c, v as i64);
    }
    fn i64c(&mut self, v: i64) {
        self.op(0x42);
        sleb(&mut self.c, v);
    }
    fn mem(&mut self, opc: u8, align: u32, off: u32) {
        self.op(opc);
        uleb(&mut self.c, align as u64);
        uleb(&mut self.c, off as u64);
    }
    fn if_(&mut self) {
        self.op(0x04);
        self.op(0x40);
        self.nest += 1;
    }
    fn else_(&mut self) {
        self.op(0x05);
    }
    fn end(&mut self) {
        self.op(0x0b);
        self.nest -= 1;
    }
    /// `if` with a result (the arms produce a value; no exits inside)
    fn if_value(&mut self, ty: u8) {
        self.op(0x04);
        self.op(ty);
    }

    /// Push guest register `r`.
    fn get(&mut self, r: u32) {
        if r == 0 {
            self.i64c(0);
        } else {
            self.lget(L_CPU);
            self.mem(I64_LOAD, 3, self.o.x + 8 * r);
        }
    }
    /// The i64 on the stack into guest register `r`.
    fn put(&mut self, r: u32) {
        if r == 0 {
            self.op(0x1a); // drop
        } else {
            self.lset(L_A);
            self.lget(L_CPU);
            self.lget(L_A);
            self.mem(I64_STORE, 3, self.o.x + 8 * r);
        }
    }
    /// Push the guest address at page offset `rel` (may lie outside the page).
    fn pcv(&mut self, rel: i64) {
        self.lget(L_PAGE);
        if rel != 0 {
            self.i64c(rel);
            self.op(I64_ADD);
        }
    }
    fn set_pc(&mut self, rel: i64) {
        self.lget(L_CPU);
        self.pcv(rel);
        self.mem(I64_STORE, 3, self.o.pc);
    }
    /// instret and cycle += k
    fn count(&mut self, k: u32) {
        if k == 0 {
            return;
        }
        for off in [self.o.instret, self.o.cycle] {
            self.lget(L_CPU);
            self.lget(L_CPU);
            self.mem(I64_LOAD, 3, off);
            self.i64c(k as i64);
            self.op(I64_ADD);
            self.mem(I64_STORE, 3, off);
        }
    }
    fn ret(&mut self, status: i32) {
        self.i32c(status);
        self.op(0x0f);
    }

    /// Leave for page offset `target` after `count` instructions of the basic block: a target
    /// in the region is a branch back to the dispatch loop while the budget lasts.
    fn exit_const(&mut self, target: i64, count: u32) {
        self.count(count);
        if let Some(k) = self.region.iter().position(|&s| s as i64 == target) {
            self.lget(L_CPU);
            self.mem(I64_LOAD, 3, self.o.cycle);
            self.lget(L_CPU);
            self.mem(I64_LOAD, 3, self.o.chain);
            self.op(I64_LT_U);
            self.op(0x04);
            self.op(0x40);
            self.i32c(k as i32);
            self.lset(L_IDX);
            self.op(0x0c); // br to the loop
            uleb(&mut self.c, (self.region.len() - self.cur_bb) as u64 + self.nest as u64 + 1);
            self.op(0x0b);
        }
        self.set_pc(target);
        self.ret(0);
    }

    /// Run the instruction at page offset `at` (the `before`-th of the basic block) in the
    /// interpreter. `last`: the block ends after it in any case.
    fn interp(&mut self, at: u32, before: u32, last: bool) {
        self.set_pc(at as i64);
        self.lget(L_CPU);
        self.i32c((before | (last as u32) << 16) as i32);
        self.i32c(self.step_idx as i32);
        self.op(0x11);
        uleb(&mut self.c, 1);
        self.op(0x00); // call_indirect (type 1) table 0
        if last {
            self.op(0x0f);
        } else {
            self.lset(L_ST);
            self.lget(L_ST);
            self.if_();
            self.lget(L_ST);
            self.op(0x0f);
            self.end();
        }
    }

    // ------------------------------------------------------------------ memory
    /// L_ADDR = x[rs1] + imm; push whether the micro-TLB maps a `size`-byte aligned access
    /// there (L_E = the entry).
    fn probe(&mut self, base: u32, imm: i64, size: u32, write: bool) {
        self.get(base);
        if imm != 0 {
            self.i64c(imm);
            self.op(I64_ADD);
        }
        self.lset(L_ADDR);
        self.lget(L_UT);
        self.lget(L_ADDR);
        self.op(I32_WRAP);
        self.i32c(12);
        self.op(I32_SHR_U);
        self.i32c(UT as i32 - 1);
        self.op(I32_AND);
        self.i32c(4);
        self.op(I32_SHL);
        self.op(I32_ADD);
        self.lset(L_E);
        self.lget(L_E);
        self.mem(I64_LOAD, 3, Self::tab(write));
        self.lget(L_ADDR);
        self.i64c(-0x1000 | (size as i64 - 1));
        self.op(I64_AND);
        self.op(I64_EQ);
    }
    fn tab(write: bool) -> u32 {
        if write {
            (UT * size_of::<UEntry>()) as u32
        } else {
            0
        }
    }
    /// After a hit: L_HOST = the host address.
    fn host(&mut self, write: bool) {
        self.lget(L_E);
        self.mem(I32_LOAD, 2, Self::tab(write) + 8);
        self.lget(L_ADDR);
        self.op(I32_WRAP);
        self.op(I32_ADD);
        self.lset(L_HOST);
    }
    fn load(&mut self, i: u32, at: u32, n: u32) {
        let (opc, size) = match f3(i) {
            0 => (0x30, 1), // i64.load8_s
            1 => (0x32, 2), // i64.load16_s
            2 => (0x34, 4), // i64.load32_s
            3 => (I64_LOAD, 8),
            4 => (0x31, 1), // i64.load8_u
            5 => (0x33, 2), // i64.load16_u
            6 => (0x35, 4), // i64.load32_u
            _ => return self.interp(at, n, false),
        };
        self.probe(rs1(i), imm_i(i), size, false);
        self.if_();
        self.host(false);
        self.lget(L_HOST);
        self.mem(opc, 0, 0);
        self.put(rd(i));
        self.else_();
        self.interp(at, n, false);
        self.end();
    }
    fn store(&mut self, i: u32, at: u32, n: u32) {
        let (opc, size) = match f3(i) {
            0 => (0x3c, 1), // i64.store8
            1 => (0x3d, 2), // i64.store16
            2 => (0x3e, 4), // i64.store32
            3 => (I64_STORE, 8),
            _ => return self.interp(at, n, false),
        };
        self.probe(rs1(i), imm_s(i), size, true);
        self.if_();
        self.host(true);
        self.lget(L_HOST);
        self.get(rs2(i));
        self.mem(opc, 0, 0);
        self.else_();
        self.interp(at, n, false);
        self.end();
    }

    // ------------------------------------------------------------------ arithmetic
    /// rd = op(x[rs1], x[rs2]) for a plain i64 binary op
    fn bin(&mut self, i: u32, opc: u8) {
        self.get(rs1(i));
        self.get(rs2(i));
        self.op(opc);
        self.put(rd(i));
    }
    /// rd = sext32(op32(x[rs1], x[rs2]))
    fn bin32(&mut self, i: u32, opc: u8) {
        self.get(rs1(i));
        self.op(I32_WRAP);
        self.get(rs2(i));
        self.op(I32_WRAP);
        self.op(opc);
        self.op(I64_EXT_S);
        self.put(rd(i));
    }
    /// rd = (x[rs1] < x[rs2]) under comparison `cmp`
    fn cmp(&mut self, a: impl Fn(&mut Self), b: impl Fn(&mut Self), cmp: u8, rd: u32) {
        a(self);
        b(self);
        self.op(cmp);
        self.op(I64_EXT_U);
        self.put(rd);
    }
    /// rd = cmp(a, b) ? a : b
    fn minmax(&mut self, i: u32, cmp: u8) {
        self.get(rs1(i));
        self.lset(L_A);
        self.get(rs2(i));
        self.lset(L_B);
        self.lget(L_A);
        self.lget(L_B);
        self.lget(L_A);
        self.lget(L_B);
        self.op(cmp);
        self.op(SELECT);
        self.put(rd(i));
    }
    /// x[rs1] and x[rs2] into L_A and L_B (as i32 when `w`)
    fn operands(&mut self, i: u32) {
        self.get(rs1(i));
        self.lset(L_A);
        self.get(rs2(i));
        self.lset(L_B);
    }
    /// division and remainder with RISC-V's results for a zero divisor and overflow; `w`: the
    /// 32-bit forms (sign-extended results)
    fn divrem(&mut self, i: u32, w: bool) {
        let f = f3(i);
        self.operands(i);
        // the operands as the operation sees them
        let a = |s: &mut Self| {
            s.lget(L_A);
            if w {
                s.op(I32_WRAP);
            }
        };
        let b = |s: &mut Self| {
            s.lget(L_B);
            if w {
                s.op(I32_WRAP);
            }
        };
        let ty = if w { RESULT_I32 } else { RESULT_I64 };
        let (eqz, eq, and) = if w { (I32_EQZ, 0x46, I32_AND) } else { (I64_EQZ, I64_EQ, 0x71) };
        // divisor zero?
        b(self);
        self.op(eqz);
        self.if_value(ty);
        match f {
            4 | 5 => {
                if w {
                    self.i32c(-1)
                } else {
                    self.i64c(-1)
                }
            }
            _ => a(self),
        }
        self.op(0x05);
        match f {
            4 => {
                // signed overflow: MIN / -1 = MIN
                a(self);
                if w {
                    self.i32c(i32::MIN);
                } else {
                    self.i64c(i64::MIN);
                }
                self.op(eq);
                b(self);
                if w {
                    self.i32c(-1);
                } else {
                    self.i64c(-1);
                }
                self.op(eq);
                self.op(and);
                self.if_value(ty);
                if w {
                    self.i32c(i32::MIN);
                } else {
                    self.i64c(i64::MIN);
                }
                self.op(0x05);
                a(self);
                b(self);
                self.op(if w { 0x6d } else { 0x7f }); // div_s
                self.op(0x0b);
            }
            5 => {
                a(self);
                b(self);
                self.op(if w { 0x6e } else { 0x80 }); // div_u
            }
            6 => {
                // rem_s does not trap on overflow (MIN % -1 = 0)
                a(self);
                b(self);
                self.op(if w { 0x6f } else { 0x81 });
            }
            _ => {
                a(self);
                b(self);
                self.op(if w { 0x70 } else { 0x82 }); // rem_u
            }
        }
        self.op(0x0b);
        if w {
            self.op(I64_EXT_S);
        }
        self.put(rd(i));
    }

    // ------------------------------------------------------------------ instructions
    /// Emit the instruction `i` (length `len`) at page offset `at`, the `n`-th of its basic
    /// block. Returns true if the basic block ends with it.
    fn insn(&mut self, i: u32, len: u32, at: u32, n: u32) -> bool {
        let next = at as i64 + len as i64;
        match i & 0x7f {
            _ if i == 0 => {
                self.interp(at, n, true);
                return true;
            }
            0x37 => {
                self.i64c((i & 0xffff_f000) as i32 as i64);
                self.put(rd(i));
            }
            0x17 => {
                self.pcv(at as i64 + (i & 0xffff_f000) as i32 as i64);
                self.put(rd(i));
            }
            0x6f => {
                self.pcv(next);
                self.put(rd(i));
                self.exit_const(at as i64 + imm_j(i), n + 1);
                return true;
            }
            0x67 => {
                if f3(i) != 0 {
                    self.interp(at, n, true);
                    return true;
                }
                // the target before the link (rd may be rs1)
                self.get(rs1(i));
                self.i64c(imm_i(i));
                self.op(I64_ADD);
                self.i64c(-2);
                self.op(I64_AND);
                self.lset(L_B);
                self.pcv(next);
                self.put(rd(i));
                self.count(n + 1);
                self.lget(L_CPU);
                self.lget(L_B);
                self.mem(I64_STORE, 3, self.o.pc);
                self.ret(0);
                return true;
            }
            0x63 => {
                let cmp = match f3(i) {
                    0 => I64_EQ,
                    1 => I64_NE,
                    4 => I64_LT_S,
                    5 => I64_GE_S,
                    6 => I64_LT_U,
                    7 => I64_GE_U,
                    _ => {
                        self.interp(at, n, true);
                        return true;
                    }
                };
                self.get(rs1(i));
                self.get(rs2(i));
                self.op(cmp);
                self.if_();
                self.exit_const(at as i64 + imm_b(i), n + 1);
                self.end();
            }
            0x03 => self.load(i, at, n),
            0x23 => self.store(i, at, n),
            0x13 => {
                let imm = imm_i(i);
                let sh = ((i >> 20) & 63) as i64;
                match f3(i) {
                    0 => {
                        self.get(rs1(i));
                        if imm != 0 {
                            self.i64c(imm);
                            self.op(I64_ADD);
                        }
                        self.put(rd(i));
                    }
                    2 => self.cmp(|s| s.get(rs1(i)), |s| s.i64c(imm), I64_LT_S, rd(i)),
                    3 => self.cmp(|s| s.get(rs1(i)), |s| s.i64c(imm), I64_LT_U, rd(i)),
                    4 | 6 | 7 => {
                        self.get(rs1(i));
                        self.i64c(imm);
                        self.op([I64_XOR, 0, I64_OR, I64_AND][(f3(i) - 4) as usize]);
                        self.put(rd(i));
                    }
                    1 => match i >> 26 {
                        0 => {
                            self.get(rs1(i));
                            self.i64c(sh);
                            self.op(I64_SHL);
                            self.put(rd(i));
                        }
                        0x18 => {
                            let opc = match (i >> 20) & 0x3f {
                                0 => 0x79, // clz
                                1 => 0x7a, // ctz
                                2 => 0x7b, // cpop
                                4 => 0xc2, // sext.b (i64.extend8_s)
                                5 => 0xc3, // sext.h
                                _ => {
                                    self.interp(at, n, false);
                                    return false;
                                }
                            };
                            self.get(rs1(i));
                            self.op(opc);
                            self.put(rd(i));
                        }
                        _ => self.interp(at, n, false),
                    },
                    _ => {
                        let opc = match i >> 26 {
                            0 => I64_SHR_U,
                            0x10 => I64_SHR_S,
                            0x18 => 0x8a, // rori
                            _ => {
                                self.interp(at, n, false); // orc.b, rev8, illegal
                                return false;
                            }
                        };
                        self.get(rs1(i));
                        self.i64c(sh);
                        self.op(opc);
                        self.put(rd(i));
                    }
                }
            }
            0x1b => {
                let sh = ((i >> 20) & 31) as i32;
                match (f3(i), f7(i)) {
                    (0, _) => {
                        self.get(rs1(i));
                        self.i64c(imm_i(i));
                        self.op(I64_ADD);
                        self.op(0xc4); // i64.extend32_s
                        self.put(rd(i));
                    }
                    (1, 4) | (1, 5) => {
                        // slli.uw
                        self.get(rs1(i));
                        self.i64c(0xffff_ffff);
                        self.op(I64_AND);
                        self.i64c(((i >> 20) & 63) as i64);
                        self.op(I64_SHL);
                        self.put(rd(i));
                    }
                    (1, 0) | (5, 0) | (5, 0x20) | (5, 0x30) => {
                        self.get(rs1(i));
                        self.op(I32_WRAP);
                        self.i32c(sh);
                        self.op(match (f3(i), f7(i)) {
                            (1, _) => I32_SHL,
                            (5, 0) => I32_SHR_U,
                            (5, 0x20) => 0x75, // i32.shr_s
                            _ => 0x78,         // i32.rotr
                        });
                        self.op(I64_EXT_S);
                        self.put(rd(i));
                    }
                    (1, 0x30) if rs2(i) < 3 => {
                        self.get(rs1(i));
                        self.op(I32_WRAP);
                        self.op([0x67, 0x68, 0x69][rs2(i) as usize]); // clz, ctz, popcnt
                        self.op(I64_EXT_U);
                        self.put(rd(i));
                    }
                    _ => self.interp(at, n, false),
                }
            }
            0x33 => match (f7(i), f3(i)) {
                (0, 0) => self.bin(i, I64_ADD),
                (0x20, 0) => self.bin(i, I64_SUB),
                (0, 1) => self.bin(i, I64_SHL),
                (0, 2) => self.cmp(|s| s.get(rs1(i)), |s| s.get(rs2(i)), I64_LT_S, rd(i)),
                (0, 3) => self.cmp(|s| s.get(rs1(i)), |s| s.get(rs2(i)), I64_LT_U, rd(i)),
                (0, 4) => self.bin(i, I64_XOR),
                (0, 5) => self.bin(i, I64_SHR_U),
                (0x20, 5) => self.bin(i, I64_SHR_S),
                (0, 6) => self.bin(i, I64_OR),
                (0, 7) => self.bin(i, I64_AND),
                (1, 0) => self.bin(i, I64_MUL),
                (1, 4..=7) => self.divrem(i, false),
                (0x20, 7) | (0x20, 6) | (0x20, 4) => {
                    // andn, orn, xnor
                    self.get(rs1(i));
                    self.get(rs2(i));
                    if f3(i) == 4 {
                        self.op(I64_XOR);
                        self.i64c(-1);
                        self.op(I64_XOR);
                    } else {
                        self.i64c(-1);
                        self.op(I64_XOR);
                        self.op(if f3(i) == 7 { I64_AND } else { I64_OR });
                    }
                    self.put(rd(i));
                }
                (5, 4) => self.minmax(i, I64_LT_S),
                (5, 5) => self.minmax(i, I64_LT_U),
                (5, 6) => self.minmax(i, 0x55), // gt_s
                (5, 7) => self.minmax(i, 0x56), // gt_u
                (0x30, 1) => self.bin(i, 0x89), // rol
                (0x30, 5) => self.bin(i, 0x8a), // ror
                (0x10, 2) | (0x10, 4) | (0x10, 6) => {
                    self.get(rs1(i));
                    self.i64c((f3(i) / 2) as i64);
                    self.op(I64_SHL);
                    self.get(rs2(i));
                    self.op(I64_ADD);
                    self.put(rd(i));
                }
                _ => self.interp(at, n, false), // mulh, mulhsu, mulhu, illegal
            },
            0x3b => match (f7(i), f3(i)) {
                (0, 0) | (0x20, 0) | (1, 0) => {
                    self.get(rs1(i));
                    self.get(rs2(i));
                    self.op(match f7(i) {
                        0 => I64_ADD,
                        0x20 => I64_SUB,
                        _ => I64_MUL,
                    });
                    self.op(0xc4); // i64.extend32_s
                    self.put(rd(i));
                }
                (0, 1) => self.bin32(i, I32_SHL),
                (0, 5) => self.bin32(i, I32_SHR_U),
                (0x20, 5) => self.bin32(i, 0x75),
                (0x30, 1) => self.bin32(i, 0x77), // rolw
                (0x30, 5) => self.bin32(i, 0x78), // rorw
                (1, 4..=7) => self.divrem(i, true),
                (4, 0) | (0x10, 2) | (0x10, 4) | (0x10, 6) => {
                    // add.uw, sh1add.uw, sh2add.uw, sh3add.uw
                    self.get(rs1(i));
                    self.i64c(0xffff_ffff);
                    self.op(I64_AND);
                    if f3(i) != 0 {
                        self.i64c((f3(i) / 2) as i64);
                        self.op(I64_SHL);
                    }
                    self.get(rs2(i));
                    self.op(I64_ADD);
                    self.put(rd(i));
                }
                (4, 4) if rs2(i) == 0 => {
                    // zext.h
                    self.get(rs1(i));
                    self.i64c(0xffff);
                    self.op(I64_AND);
                    self.put(rd(i));
                }
                _ => self.interp(at, n, false),
            },
            0x0f if f3(i) == 0 => {} // fence: memory is sequentially consistent
            0x73 | 0x0f => {
                // CSRs, privileged instructions, fence.i: the machine looks after them
                self.interp(at, n, true);
                return true;
            }
            _ => self.interp(at, n, false), // AMOs, F and D
        }
        false
    }
}

/// Compile the region entered at virtual address `pc` of the page `code`. Returns the function
/// (its table index) and the page offsets of the bytes it was compiled from.
pub(crate) fn compile(code: &[u8], pc: u64, step_idx: u32) -> Option<(usize, u16, u16)> {
    let entry = (pc & 0xfff) as u32;
    fetch(code, entry)?;
    let o = Off {
        x: offset_of!(Hart, x) as u32,
        pc: offset_of!(Hart, pc) as u32,
        instret: offset_of!(Hart, instret) as u32,
        cycle: offset_of!(Hart, cycle) as u32,
        ut: offset_of!(Hart, jit_ut) as u32,
        chain: offset_of!(Hart, chain_until) as u32,
    };
    let mut w = W { c: Vec::with_capacity(4096), o, step_idx, region: Vec::new(), cur_bb: 0, nest: 0 };
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
    let n_bbs = bbs.len();
    w.region = bbs.clone();
    // ---- prologue: the page's virtual address and the micro-TLB table
    w.lget(L_CPU);
    w.mem(I64_LOAD, 3, w.o.pc);
    w.i64c(-0x1000);
    w.op(I64_AND);
    w.lset(L_PAGE);
    w.lget(L_CPU);
    w.mem(I32_LOAD, 2, w.o.ut);
    w.lset(L_UT);
    w.op(0x03);
    w.op(0x40); // loop: the dispatch
    for _ in 0..=n_bbs {
        w.op(0x02);
        w.op(0x40); // $exit, then one block per basic block
    }
    w.lget(L_IDX);
    w.op(0x0e);
    uleb(&mut w.c, n_bbs as u64);
    for j in 0..n_bbs {
        uleb(&mut w.c, j as u64);
    }
    uleb(&mut w.c, n_bbs as u64); // default: $exit
    for (k, &start) in bbs.iter().enumerate() {
        w.op(0x0b); // end of the basic block's label: its code follows
        w.cur_bb = k;
        w.nest = 0;
        let mut n = 0u32;
        let mut at = start;
        loop {
            let Some((i, len)) = fetch(code, at) else {
                // straddles the page: the interpreter runs it (never through the region: this
                // may be the basic block's own start)
                w.count(n);
                w.set_pc(at as i64);
                w.ret(0);
                break;
            };
            let end = w.insn(i, len, at, n);
            n += 1;
            at += len;
            if end {
                break;
            }
            if n >= MAX_INSNS || at >= 0x1000 {
                w.exit_const(at as i64, n);
                break;
            }
        }
        debug_assert_eq!(w.nest, 0);
    }
    w.op(0x0b); // end of $exit (not reached: every basic block ends in a branch or return)
    w.ret(0);
    w.op(0x0b); // end of the loop
    w.op(0x00); // unreachable
    w.op(0x0b); // end of the function
    let module = assemble(&w.c);
    // SAFETY: the import reads `len` bytes of the module
    let idx = unsafe { sbc_jit_compile(module.as_ptr(), module.len()) };
    if idx < 0 {
        return None;
    }
    Some((idx as usize, lo as u16, hi as u16))
}

/// Wrap a function body into a module importing env.memory and env.table.
fn assemble(body: &[u8]) -> Vec<u8> {
    let mut m = b"\0asm\x01\0\0\0".to_vec();
    let section = |m: &mut Vec<u8>, id: u8, payload: &[u8]| {
        m.push(id);
        uleb(m, payload.len() as u64);
        m.extend_from_slice(payload);
    };
    // types: 0 = (i32) -> i32 (the block), 1 = (i32, i32) -> i32 (h_step)
    section(&mut m, 1, &[2, 0x60, 1, 0x7f, 1, 0x7f, 0x60, 2, 0x7f, 0x7f, 1, 0x7f]);
    // imports: env.memory (min 0), env.table (funcref, min 0)
    let mut imp = vec![2];
    for (name, desc) in [("memory", &[0x02u8, 0x00, 0x00][..]), ("table", &[0x01u8, 0x70, 0x00, 0x00][..])] {
        imp.push(3);
        imp.extend_from_slice(b"env");
        imp.push(name.len() as u8);
        imp.extend_from_slice(name.as_bytes());
        imp.extend_from_slice(desc);
    }
    section(&mut m, 2, &imp);
    section(&mut m, 3, &[1, 0]); // one function of type 0
    section(&mut m, 7, &[1, 1, b'f', 0x00, 0]); // export "f" = function 0
    let mut f = vec![2, I32_LOCALS as u8, 0x7f, I64_LOCALS as u8, 0x7e];
    f.extend_from_slice(body);
    let mut code = vec![1];
    uleb(&mut code, f.len() as u64);
    code.extend_from_slice(&f);
    section(&mut m, 10, &code);
    m
}
