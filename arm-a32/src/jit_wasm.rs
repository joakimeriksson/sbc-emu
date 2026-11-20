//! The A32/T32 block JIT's WebAssembly backend (browser builds).
//!
//! A hot block becomes a small wasm module: `(func (param $cpu i32) (result i32))` importing
//! the emulator's linear memory and function table. It mirrors the native backend
//! instruction for instruction (the same encodings are translated, the same ones go to the
//! interpreter through `h_step`, the same micro-TLB probes guard loads and stores), with
//! three differences:
//! - the guest's N, Z, C and V live in four wasm locals (written back to the CPSR before
//!   the interpreter runs an instruction and when the block leaves);
//! - a block is a *region*: the entry block plus the blocks that constant branches on the
//!   same page reach (up to `MAX_BBS`), one function with a `loop` + `br_table` dispatch, so
//!   loops stay inside wasm (wasm code cannot be patched to chain blocks directly);
//! - the function returns 0 to continue at `r[15]` (the caller may chain into the next
//!   block), 1 for a trap in `jit_trap`, 2 to continue without chaining (the interpreter
//!   ran an instruction that needs the machine to look: a device access, a mode change).
//!
//! The page compiles the bytes synchronously (`new WebAssembly.Module` in the worker) and
//! installs the function in the table (`sbc_jit_compile`); Rust calls it as a function
//! pointer. `sbc_jit_reset` frees every slot of the table, so a flush is global: every core
//! drops its blocks at once (`CodeWatch::reset_all`).
use super::{ctx, h_step, writes_pc, BlockFn, Ctx};
use crate::cpu::Cpu;
use crate::mmu::{utlb_table, UT, U_READ, U_WRITE};

#[link(wasm_import_module = "env")]
extern "C" {
    /// Provided by the page: compile a module, install its export "f" in the function table,
    /// return the table index (or -1).
    fn sbc_jit_compile(ptr: *const u8, len: usize) -> i32;
    /// All compiled code was discarded: the page may reuse every slot it installed.
    fn sbc_jit_reset();
}

/// Tell the page that all compiled code is gone.
pub(super) fn reset() {
    // SAFETY: the import takes no arguments
    unsafe { sbc_jit_reset() }
}

/// Basic blocks per compiled region.
const MAX_BBS: usize = 24;
/// Instructions per basic block (as natively).
const MAX_INSNS: u32 = 64;

// Locals: the Cpu pointer (parameter), then i32 and i64 temporaries.
const L_CPU: u32 = 0;
const L_A: u32 = 1; // operand 1 (Rn)
const L_B: u32 = 2; // operand 2
const L_R: u32 = 3; // result
const L_T: u32 = 4; // scratch for stores
const L_ADDR: u32 = 5; // address with the offset applied
const L_BASE: u32 = 6; // base register value
const L_HOST: u32 = 7; // micro-TLB entry, then the host address
const L_ST: u32 = 8; // h_step status
const L_IDX: u32 = 9; // the region's next basic block
const L_N: u32 = 10;
const L_Z: u32 = 11;
const L_C: u32 = 12;
const L_V: u32 = 13;
const L_UT: u32 = 14; // micro-TLB base pointer
const L_D: u32 = 15; // data
const L_E: u32 = 16; // shift source, second data word
const L_CO: u32 = 17; // shifter carry out
const L_AMT: u32 = 18; // register shift amount
const I32_LOCALS: u32 = 18;
const L_X: u32 = 19; // i64
const L_Y: u32 = 20; // i64
const I64_LOCALS: u32 = 2;

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

fn sign_extend(value: u32, bits: u32) -> u32 {
    ((value << (32 - bits)) as i32 >> (32 - bits)) as u32
}

/// Where a logical instruction's carry comes from.
#[derive(Clone, Copy)]
enum Carry {
    Unchanged,
    Const(bool),
    /// the shifter carry in `L_CO`
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

/// How a Thumb instruction is emitted (as natively).
enum T {
    /// the equivalent A32 instruction (condition AL)
    A32(u32),
    /// emitted here; true = the block ends
    Done(bool),
    /// interpreted; true = the block ends after it
    Interp(bool),
}

// wasm opcodes used more than once
const I32_LOAD: u8 = 0x28;
const I64_LOAD: u8 = 0x29;
const I32_STORE: u8 = 0x36;
const I64_STORE: u8 = 0x37;
const EQZ: u8 = 0x45;
const EQ: u8 = 0x46;
const LT_U: u8 = 0x49;
const GE_U: u8 = 0x4f;
const ADD: u8 = 0x6a;
const SUB: u8 = 0x6b;
const MUL: u8 = 0x6c;
const AND: u8 = 0x71;
const OR: u8 = 0x72;
const XOR: u8 = 0x73;
const SHL: u8 = 0x74;
const SHR_S: u8 = 0x75;
const SHR_U: u8 = 0x76;
const ROTR: u8 = 0x78;
const SELECT: u8 = 0x1b;
const WRAP: u8 = 0xa7;
const EXT_S: u8 = 0xac;
const EXT_U: u8 = 0xad;

struct W {
    c: Vec<u8>,
    o: Ctx,
    /// table index of `h_step`
    step_idx: u32,
    /// byte offsets of this privilege's read and write micro-TLB tables
    rtab: u32,
    wtab: u32,
    /// the table of the last probe (for `host_addr`)
    ptab: u32,
    thumb: bool,
    size: u32,
    it: u32,
    el0: bool,
    /// start addresses of the region's basic blocks (index = dispatch value)
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
    fn i32c(&mut self, v: u32) {
        self.op(0x41);
        sleb(&mut self.c, v as i32 as i64);
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
    fn cpu_ld(&mut self, off: usize) {
        self.lget(L_CPU);
        self.mem(I32_LOAD, 2, off as u32);
    }
    fn cpu_st_from(&mut self, off: usize, l: u32) {
        self.lget(L_CPU);
        self.lget(l);
        self.mem(I32_STORE, 2, off as u32);
    }
    /// The value on the stack into [cpu + off].
    fn cpu_st(&mut self, off: usize) {
        self.lset(L_T);
        self.cpu_st_from(off, L_T);
    }
    fn reg_off(&self, n: u32) -> usize {
        self.o.r + 4 * n as usize
    }
    /// Push guest register `n` (PC reads give the address + 8, or + 4 in Thumb state).
    fn get(&mut self, n: u32, pc: u32) {
        if n == 15 {
            self.i32c(pc.wrapping_add(if self.thumb { 4 } else { 8 }));
        } else {
            self.cpu_ld(self.reg_off(n));
        }
    }
    /// Guest register `n` into local `l`.
    fn get_to(&mut self, n: u32, pc: u32, l: u32) {
        self.get(n, pc);
        self.lset(l);
    }
    /// Local `l` into guest register `n` (not the PC).
    fn set(&mut self, n: u32, l: u32) {
        debug_assert!(n != 15);
        self.cpu_st_from(self.reg_off(n), l);
    }
    fn set_const(&mut self, n: u32, v: u32) {
        self.lget(L_CPU);
        self.i32c(v);
        self.mem(I32_STORE, 2, self.reg_off(n) as u32);
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

    // ------------------------------------------------------------------ flags and exits
    fn flags_load(&mut self) {
        for (l, sh) in [(L_N, 31), (L_Z, 30), (L_C, 29), (L_V, 28)] {
            self.cpu_ld(self.o.cpsr);
            self.i32c(sh);
            self.op(SHR_U);
            if sh != 31 {
                self.i32c(1);
                self.op(AND);
            }
            self.lset(l);
        }
    }
    fn flags_save(&mut self) {
        self.cpu_ld(self.o.cpsr);
        self.i32c(0x0fff_ffff);
        self.op(AND);
        for (l, sh) in [(L_N, 31), (L_Z, 30), (L_C, 29), (L_V, 28)] {
            self.lget(l);
            self.i32c(sh);
            self.op(SHL);
            self.op(OR);
        }
        self.cpu_st(self.o.cpsr);
    }
    /// N and Z from local `l`.
    fn nz(&mut self, l: u32) {
        self.lget(l);
        self.i32c(31);
        self.op(SHR_U);
        self.lset(L_N);
        self.lget(l);
        self.op(EQZ);
        self.lset(L_Z);
    }
    /// Push condition `cond` (0/1).
    fn cond(&mut self, cond: u32) {
        match cond >> 1 {
            0 => self.lget(L_Z),
            1 => self.lget(L_C),
            2 => self.lget(L_N),
            3 => self.lget(L_V),
            4 => {
                self.lget(L_C);
                self.lget(L_Z);
                self.op(EQZ);
                self.op(AND);
            }
            5 => {
                self.lget(L_N);
                self.lget(L_V);
                self.op(EQ);
            }
            6 => {
                self.lget(L_N);
                self.lget(L_V);
                self.op(EQ);
                self.lget(L_Z);
                self.op(EQZ);
                self.op(AND);
            }
            _ => self.i32c(1),
        }
        if cond & 1 == 1 && cond < 14 {
            self.op(EQZ);
        }
    }
    fn add_instret(&mut self, k: u32) {
        if k == 0 {
            return;
        }
        self.lget(L_CPU);
        self.lget(L_CPU);
        self.mem(I64_LOAD, 3, self.o.instret as u32);
        self.i64c(k as i64);
        self.op(0x7c);
        self.mem(I64_STORE, 3, self.o.instret as u32);
    }
    fn ret(&mut self, status: u32) {
        self.i32c(status);
        self.op(0x0f);
    }
    /// Leave for constant `target` after `count` instructions of the basic block: a target
    /// in the region is a branch back to the dispatch loop while the budget lasts.
    fn exit_const(&mut self, target: u32, count: u32) {
        self.add_instret(count);
        if let Some(k) = self.region.iter().position(|&s| s == target) {
            self.lget(L_CPU);
            self.mem(I64_LOAD, 3, self.o.instret as u32);
            self.lget(L_CPU);
            self.mem(I64_LOAD, 3, self.o.chain_until as u32);
            self.op(0x54); // i64.lt_u
            self.op(0x04);
            self.op(0x40);
            self.i32c(k as u32);
            self.lset(L_IDX);
            self.op(0x0c);
            uleb(&mut self.c, (self.region.len() - self.cur_bb) as u64 + self.nest as u64 + 1);
            self.op(0x0b);
        }
        self.exit_out_counted(target);
    }
    /// Leave for `target` (instructions already counted), never through the region.
    fn exit_out_counted(&mut self, target: u32) {
        self.flags_save();
        self.set_const(15, target);
        self.ret(0);
    }
    /// Leave for the address in local `l` with BXWritePC semantics (bit 0 selects Thumb).
    fn exit_bx(&mut self, l: u32, count: u32) {
        self.lget(l);
        self.i32c(1);
        self.op(AND);
        self.if_();
        self.lget(l);
        self.i32c(!1);
        self.op(AND);
        self.cpu_st(self.reg_off(15));
        if !self.thumb {
            self.set_t(true);
        }
        self.else_();
        self.lget(l);
        self.i32c(!3);
        self.op(AND);
        self.cpu_st(self.reg_off(15));
        if self.thumb {
            self.set_t(false);
        }
        self.end();
        self.flags_save();
        self.add_instret(count);
        self.ret(0);
    }
    /// BranchWritePC in Thumb state: to the address in local `l` (bit 0 ignored).
    fn exit_thumb_branch(&mut self, l: u32, count: u32) {
        self.lget(l);
        self.i32c(!1);
        self.op(AND);
        self.cpu_st(self.reg_off(15));
        self.flags_save();
        self.add_instret(count);
        self.ret(0);
    }
    fn set_t(&mut self, thumb: bool) {
        self.cpu_ld(self.o.cpsr);
        if thumb {
            self.i32c(1 << 5);
            self.op(OR);
        } else {
            self.i32c(!(1 << 5));
            self.op(AND);
        }
        self.cpu_st(self.o.cpsr);
    }
    /// Put ITSTATE `it` into the CPSR.
    fn write_itstate(&mut self, it: u32) {
        self.cpu_ld(self.o.cpsr);
        self.i32c(!0x0600_fc00);
        self.op(AND);
        let bits = (it & 3) << 25 | (it >> 2) << 10;
        if bits != 0 {
            self.i32c(bits);
            self.op(OR);
        }
        self.cpu_st(self.o.cpsr);
    }
    /// Run the instruction at `pc` (the `before`-th of the basic block) in the interpreter.
    /// `last`: the block ends here in any case.
    fn call_interp(&mut self, pc: u32, before: u32, last: bool, why: u32) {
        self.flags_save();
        if self.it != 0 {
            let it = self.it;
            self.write_itstate(it);
        }
        self.lget(L_CPU);
        self.i32c(pc);
        self.i32c(before);
        self.i32c(last as u32 | why << 1 | u32::from(self.size == 2) << 3);
        self.i32c(self.step_idx);
        self.op(0x11);
        uleb(&mut self.c, 1);
        self.op(0x00); // call_indirect (type 1) table 0
        self.lset(L_ST);
        // h_step: 1 = leave (pc set) -> 2 (no chaining), 2 = trap -> 1
        if last {
            self.i32c(3);
            self.lget(L_ST);
            self.op(SUB);
            self.op(0x0f);
        } else {
            self.lget(L_ST);
            self.if_();
            self.i32c(3);
            self.lget(L_ST);
            self.op(SUB);
            self.op(0x0f);
            self.end();
            self.flags_load();
        }
    }

    // ------------------------------------------------------------------ memory
    /// Push whether the micro-TLB maps an aligned `size`-byte access at the address in
    /// local `a` (L_HOST = the entry).
    fn probe(&mut self, a: u32, size: u32, write: bool) {
        self.ptab = if write { self.wtab } else { self.rtab };
        self.lget(L_UT);
        self.lget(a);
        self.i32c(12);
        self.op(SHR_U);
        self.i32c(UT as u32 - 1);
        self.op(AND);
        self.i32c(4);
        self.op(SHL);
        self.op(ADD);
        self.lset(L_HOST);
        self.lget(L_HOST);
        self.mem(I32_LOAD, 2, self.ptab);
        self.lget(a);
        self.i32c(0xffff_f000 | (size - 1));
        self.op(AND);
        self.op(EQ);
    }
    /// Push whether `bytes` bytes from local `a` stay in one page.
    fn same_page(&mut self, a: u32, bytes: u32) {
        self.lget(a);
        self.i32c(bytes - 1);
        self.op(ADD);
        self.lget(a);
        self.op(XOR);
        self.i32c(12);
        self.op(SHR_U);
        self.op(EQZ);
    }
    /// After a hit: L_HOST = host address of the address in local `a`.
    fn host_addr(&mut self, a: u32) {
        self.lget(L_HOST);
        self.mem(I32_LOAD, 2, self.ptab + 8);
        self.lget(a);
        self.op(ADD);
        self.lset(L_HOST);
    }
    /// Base (into L_BASE) and the offset-applied address (into L_ADDR; a register offset is
    /// in L_B); returns the local holding the accessed address.
    fn address(&mut self, rn: u32, pc: u32, pre: bool, up: bool, offset: Option<u32>) -> u32 {
        if self.thumb && rn == 15 {
            self.i32c(pc.wrapping_add(4) & !3); // Align(PC, 4)
        } else {
            self.get(rn, pc);
        }
        self.lset(L_BASE);
        self.lget(L_BASE);
        match offset {
            Some(0) => {}
            Some(imm) => {
                self.i32c(imm);
                self.op(if up { ADD } else { SUB });
            }
            None => {
                self.lget(L_B);
                self.op(if up { ADD } else { SUB });
            }
        }
        self.lset(L_ADDR);
        if pre {
            L_ADDR
        } else {
            L_BASE
        }
    }

    // ------------------------------------------------------------------ data processing
    /// The A32 shifter operand (immediate or register) of `insn` into L_B; the carry into
    /// L_CO when `carry` asks for it. None: interpret.
    fn operand2(&mut self, insn: u32, pc: u32, carry: bool) -> Option<Carry> {
        if insn & (1 << 25) != 0 {
            let rotation = (insn >> 8) & 0xf;
            let value = (insn & 0xff).rotate_right(2 * rotation);
            self.i32c(value);
            self.lset(L_B);
            return Some(if rotation == 0 { Carry::Unchanged } else { Carry::Const(value >> 31 != 0) });
        }
        let (rm, kind) = (insn & 0xf, (insn >> 5) & 3);
        if insn & 0x10 != 0 {
            // register-specified shift (amount = Rs[7:0])
            let rs = (insn >> 8) & 0xf;
            if rm == 15 || rs == 15 {
                return None;
            }
            self.get_to(rm, pc, L_E);
            self.get(rs, pc);
            self.i32c(0xff);
            self.op(AND);
            self.lset(L_AMT);
            self.shift_reg(kind, carry);
            return Some(if carry { Carry::Local } else { Carry::Unchanged });
        }
        let amount = (insn >> 7) & 0x1f;
        if kind == 0 && amount == 0 {
            self.get_to(rm, pc, L_B);
            return Some(Carry::Unchanged);
        }
        self.get_to(rm, pc, L_E);
        if kind == 3 && amount == 0 {
            // RRX
            self.lget(L_C);
            self.i32c(31);
            self.op(SHL);
            self.lget(L_E);
            self.i32c(1);
            self.op(SHR_U);
            self.op(OR);
            self.lset(L_B);
            if carry {
                self.lget(L_E);
                self.i32c(1);
                self.op(AND);
                self.lset(L_CO);
            }
            return Some(if carry { Carry::Local } else { Carry::Unchanged });
        }
        let carry_bit = match (kind, amount) {
            (0, a) => {
                self.lget(L_E);
                self.i32c(a);
                self.op(SHL);
                32 - a
            }
            (1, 0) => {
                self.i32c(0);
                31
            }
            (1, a) => {
                self.lget(L_E);
                self.i32c(a);
                self.op(SHR_U);
                a - 1
            }
            (2, 0) => {
                self.lget(L_E);
                self.i32c(31);
                self.op(SHR_S);
                31
            }
            (2, a) => {
                self.lget(L_E);
                self.i32c(a);
                self.op(SHR_S);
                a - 1
            }
            (_, a) => {
                self.lget(L_E);
                self.i32c(a);
                self.op(ROTR);
                a - 1
            }
        };
        self.lset(L_B);
        if carry {
            self.lget(L_E);
            self.i32c(carry_bit);
            self.op(SHR_U);
            self.i32c(1);
            self.op(AND);
            self.lset(L_CO);
            return Some(Carry::Local);
        }
        Some(Carry::Unchanged)
    }

    /// Shift L_E by L_AMT (0-255) into L_B; with `carry`, the shifter carry into L_CO
    /// (unchanged for amount 0).
    fn shift_reg(&mut self, kind: u32, carry: bool) {
        if kind == 3 {
            self.lget(L_E);
            self.lget(L_AMT);
            self.op(ROTR); // amount mod 32
            self.lset(L_B);
            if carry {
                self.lget(L_B);
                self.i32c(31);
                self.op(SHR_U);
                self.lget(L_C);
                self.lget(L_AMT);
                self.op(SELECT);
                self.lset(L_CO);
            }
            return;
        }
        // s = min(amount, 33) (i64); shifts of 32 and more give 0 (or the sign for ASR)
        self.lget(L_AMT);
        self.i32c(33);
        self.lget(L_AMT);
        self.i32c(33);
        self.op(LT_U);
        self.op(SELECT);
        self.op(EXT_U);
        self.lset(L_Y);
        match kind {
            0 => {
                self.lget(L_E);
                self.op(EXT_U);
                self.lget(L_Y);
                self.op(0x86); // i64.shl
                self.lset(L_X);
                self.lget(L_X);
                self.op(WRAP);
                self.lset(L_B);
                if carry {
                    self.lget(L_X);
                    self.i64c(32);
                    self.op(0x88);
                    self.op(WRAP);
                    self.i32c(1);
                    self.op(AND);
                }
            }
            _ => {
                // (x << 1) >> s: bit 0 is the carry, the rest the result
                self.lget(L_E);
                self.op(if kind == 1 { EXT_U } else { EXT_S });
                self.i64c(1);
                self.op(0x86);
                self.lget(L_Y);
                self.op(if kind == 1 { 0x88 } else { 0x87 });
                self.lset(L_X);
                self.lget(L_X);
                self.i64c(1);
                self.op(if kind == 1 { 0x88 } else { 0x87 });
                self.op(WRAP);
                self.lset(L_B);
                if carry {
                    self.lget(L_X);
                    self.op(WRAP);
                    self.i32c(1);
                    self.op(AND);
                }
            }
        }
        if carry {
            self.lget(L_C);
            self.lget(L_AMT);
            self.op(SELECT);
            self.lset(L_CO);
        }
    }

    /// L_R = L_A <op> L_B; NZCV when `s` (logical operations take C from `carry`).
    fn alu(&mut self, op: Alu, s: bool, carry: Carry) {
        let logical = |w: &mut W, f: &dyn Fn(&mut W)| {
            f(w);
            w.lset(L_R);
            if s {
                w.nz(L_R);
                match carry {
                    Carry::Unchanged => {}
                    Carry::Const(c) => {
                        w.i32c(c as u32);
                        w.lset(L_C);
                    }
                    Carry::Local => {
                        w.lget(L_CO);
                        w.lset(L_C);
                    }
                }
            }
        };
        match op {
            Alu::And => logical(self, &|w| { w.lget(L_A); w.lget(L_B); w.op(AND); }),
            Alu::Eor => logical(self, &|w| { w.lget(L_A); w.lget(L_B); w.op(XOR); }),
            Alu::Orr => logical(self, &|w| { w.lget(L_A); w.lget(L_B); w.op(OR); }),
            Alu::Mov => logical(self, &|w| w.lget(L_B)),
            Alu::Bic => logical(self, &|w| { w.lget(L_A); w.lget(L_B); w.i32c(u32::MAX); w.op(XOR); w.op(AND); }),
            Alu::Mvn => logical(self, &|w| { w.lget(L_B); w.i32c(u32::MAX); w.op(XOR); }),
            Alu::Orn => logical(self, &|w| { w.lget(L_A); w.lget(L_B); w.i32c(u32::MAX); w.op(XOR); w.op(OR); }),
            Alu::Add | Alu::Sub | Alu::Rsb => {
                let (x, y) = if op == Alu::Rsb { (L_B, L_A) } else { (L_A, L_B) };
                self.lget(x);
                self.lget(y);
                self.op(if op == Alu::Add { ADD } else { SUB });
                self.lset(L_R);
                if s {
                    self.nz(L_R);
                    if op == Alu::Add {
                        // C = r <u x; V = ((x ^ r) & (y ^ r)) >> 31
                        self.lget(L_R);
                        self.lget(x);
                        self.op(LT_U);
                        self.lset(L_C);
                        self.lget(x);
                        self.lget(L_R);
                        self.op(XOR);
                        self.lget(y);
                        self.lget(L_R);
                        self.op(XOR);
                    } else {
                        // C = x >=u y; V = ((x ^ y) & (x ^ r)) >> 31
                        self.lget(x);
                        self.lget(y);
                        self.op(GE_U);
                        self.lset(L_C);
                        self.lget(x);
                        self.lget(y);
                        self.op(XOR);
                        self.lget(x);
                        self.lget(L_R);
                        self.op(XOR);
                    }
                    self.op(AND);
                    self.i32c(31);
                    self.op(SHR_U);
                    self.lset(L_V);
                }
            }
            Alu::Adc | Alu::Sbc | Alu::Rsc => {
                // AddWithCarry(x, y', C) with y' = y (ADC) or ~y (SBC, RSC)
                let (x, y) = if op == Alu::Rsc { (L_B, L_A) } else { (L_A, L_B) };
                if op != Alu::Adc {
                    self.lget(y);
                    self.i32c(u32::MAX);
                    self.op(XOR);
                    self.lset(y);
                }
                self.lget(x);
                self.op(EXT_U);
                self.lget(y);
                self.op(EXT_U);
                self.op(0x7c);
                self.lget(L_C);
                self.op(EXT_U);
                self.op(0x7c);
                self.lset(L_X);
                self.lget(L_X);
                self.op(WRAP);
                self.lset(L_R);
                if s {
                    self.nz(L_R);
                    self.lget(L_X);
                    self.i64c(32);
                    self.op(0x88);
                    self.op(WRAP);
                    self.lset(L_C);
                    self.lget(x);
                    self.lget(L_R);
                    self.op(XOR);
                    self.lget(y);
                    self.lget(L_R);
                    self.op(XOR);
                    self.op(AND);
                    self.i32c(31);
                    self.op(SHR_U);
                    self.lset(L_V);
                }
            }
        }
    }
}

// ---------------------------------------------------------------------- A32
impl W {
    /// Emit guest instruction `insn` at `pc`, the `n`-th of the basic block. Returns true
    /// when the block ends with it.
    fn insn(&mut self, insn: u32, pc: u32, n: u32) -> bool {
        let cond = insn >> 28;
        if cond == 15 {
            return self.unconditional(insn, pc, n);
        }
        if cond != 14 {
            self.cond(cond);
            self.if_();
        }
        let end = match self.native(insn, pc, n) {
            Some(end) => end,
            None => {
                let end = cond == 14 && writes_pc(insn);
                self.call_interp(pc, n, end, 0);
                end
            }
        };
        if cond != 14 {
            self.end();
            return false;
        }
        end
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
            self.ret(0);
            return true;
        }
        // barriers and preload hints (one host thread: nothing to order)
        if insn & 0x0fff_fff0 == 0x057f_f040
            || insn & 0x0fff_fff0 == 0x057f_f050
            || insn == 0xf57f_f06f
            || insn & 0x0f70_f000 == 0x0550_f000
            || insn & 0x0f70_f000 == 0x0510_f000
            || matches!(insn & 0x0f70_f010, 0x0750_f000 | 0x0710_f000 | 0x0650_f000)
            || insn & 0x0f70_f000 == 0x0450_f000
        {
            return false;
        }
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
        if rd == 15 && !test && s || reg_shift && rn == 15 {
            return None;
        }
        let carry = self.operand2(insn, pc, s && logical)?;
        if !matches!(opcode, 13 | 15) {
            self.get_to(rn, pc, L_A);
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
            self.exit_bx(L_R, n + 1);
            return Some(insn >> 28 == 14);
        }
        if !test {
            self.set(rd, L_R);
        }
        Some(false)
    }

    fn mul(&mut self, insn: u32) -> bool {
        let (rd, ra, rs, rm) = ((insn >> 16) & 0xf, (insn >> 12) & 0xf, (insn >> 8) & 0xf, insn & 0xf);
        if [rd, ra, rs, rm].contains(&15) {
            return false;
        }
        self.get(rm, 0);
        self.get(rs, 0);
        self.op(MUL);
        if insn & (1 << 21) != 0 {
            self.get(ra, 0);
            self.op(ADD);
        }
        self.lset(L_R);
        if insn & (1 << 20) != 0 {
            self.nz(L_R);
        }
        self.set(rd, L_R);
        true
    }

    fn mul_long(&mut self, insn: u32) -> bool {
        let (hi, lo, rs, rm) = ((insn >> 16) & 0xf, (insn >> 12) & 0xf, (insn >> 8) & 0xf, insn & 0xf);
        if [hi, lo, rs, rm].contains(&15) || hi == lo || insn & (1 << 20) != 0 {
            return false;
        }
        let ext = if insn & (1 << 22) != 0 { EXT_S } else { EXT_U };
        self.get(rm, 0);
        self.op(ext);
        self.get(rs, 0);
        self.op(ext);
        self.op(0x7e); // i64.mul
        if insn & (1 << 21) != 0 {
            self.get(hi, 0);
            self.op(EXT_U);
            self.i64c(32);
            self.op(0x86);
            self.get(lo, 0);
            self.op(EXT_U);
            self.op(0x84);
            self.op(0x7c);
        }
        self.lset(L_X);
        self.lget(L_X);
        self.op(WRAP);
        self.lset(L_R);
        self.set(lo, L_R);
        self.lget(L_X);
        self.i64c(32);
        self.op(0x88);
        self.op(WRAP);
        self.lset(L_R);
        self.set(hi, L_R);
        true
    }

    /// The miscellaneous space (data-processing opcodes 8-11 without S): Some(block ends).
    fn misc(&mut self, insn: u32, pc: u32, n: u32) -> Option<bool> {
        if insn & 0x0fff_fff0 == 0x012f_ff10 || insn & 0x0fff_fff0 == 0x012f_ff30 {
            let rm = insn & 0xf;
            if rm == 15 {
                return None;
            }
            self.get_to(rm, pc, L_D);
            if insn & 0x20 != 0 {
                self.set_const(14, pc.wrapping_add(4));
            }
            self.exit_bx(L_D, n + 1);
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
                self.i32c(imm);
            } else {
                self.get(rd, pc);
                self.i32c(0xffff);
                self.op(AND);
                self.i32c(imm << 16);
                self.op(OR);
            }
            self.lset(L_R);
            self.set(rd, L_R);
            return Some(false);
        }
        // CLZ
        if insn & 0x0fff_0ff0 == 0x016f_0f10 {
            let (rd, rm) = ((insn >> 12) & 0xf, insn & 0xf);
            if rd == 15 || rm == 15 {
                return None;
            }
            self.get(rm, pc);
            self.op(0x67);
            self.lset(L_R);
            self.set(rd, L_R);
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
            self.cpu_ld(self.o.cpsr);
            self.i32c(!(0x0100_0000 | 0x0600_fc00 | 1 << 5));
            self.op(AND);
            self.lset(L_R);
            self.set(rd, L_R);
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
            self.cpu_ld(off);
            self.lset(L_R);
            self.set(rd, L_R);
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
            self.get_to(rm, 0, L_E);
            if insn & 0x80 == 0 {
                // REV
                self.lget(L_E);
                self.i32c(24);
                self.op(SHL);
                self.lget(L_E);
                self.i32c(0xff00);
                self.op(AND);
                self.i32c(8);
                self.op(SHL);
                self.op(OR);
                self.lget(L_E);
                self.i32c(8);
                self.op(SHR_U);
                self.i32c(0xff00);
                self.op(AND);
                self.op(OR);
                self.lget(L_E);
                self.i32c(24);
                self.op(SHR_U);
                self.op(OR);
            } else {
                // REV16
                self.lget(L_E);
                self.i32c(0x00ff_00ff);
                self.op(AND);
                self.i32c(8);
                self.op(SHL);
                self.lget(L_E);
                self.i32c(8);
                self.op(SHR_U);
                self.i32c(0x00ff_00ff);
                self.op(AND);
                self.op(OR);
            }
            self.lset(L_R);
            self.set(rd, L_R);
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
            self.get(rm, 0);
            let rotation = ((insn >> 10) & 3) * 8;
            if rotation != 0 {
                self.i32c(rotation);
                self.op(ROTR);
            }
            match kind {
                0 => {
                    self.i32c(0xff);
                    self.op(AND);
                }
                1 => {
                    self.i32c(0xffff);
                    self.op(AND);
                }
                2 => self.op(0xc0), // i32.extend8_s
                _ => self.op(0xc1), // i32.extend16_s
            }
            if rn != 15 {
                self.get(rn, 0);
                self.op(ADD);
            }
            self.lset(L_R);
            self.set(rd, L_R);
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
            self.get(rd, 0);
            self.i32c(!mask);
            self.op(AND);
            if rm != 15 {
                self.get(rm, 0);
                self.i32c(lsb);
                self.op(SHL);
                self.i32c(mask);
                self.op(AND);
                self.op(OR);
            }
            self.lset(L_R);
            self.set(rd, L_R);
            return true;
        }
        // UBFX, SBFX
        if matches!(insn & 0x0fe0_0070, 0x07e0_0050 | 0x07a0_0050) {
            let (width, lsb) = (((insn >> 16) & 0x1f) + 1, (insn >> 7) & 0x1f);
            if rd == 15 || rm == 15 || lsb + width > 32 {
                return false;
            }
            self.get(rm, 0);
            if insn & 0x0fe0_0070 == 0x07a0_0050 {
                self.i32c(32 - lsb - width);
                self.op(SHL);
                self.i32c(32 - width);
                self.op(SHR_S);
            } else {
                self.i32c(lsb);
                self.op(SHR_U);
                if width < 32 {
                    self.i32c((1 << width) - 1);
                    self.op(AND);
                }
            }
            self.lset(L_R);
            self.set(rd, L_R);
            return true;
        }
        // SDIV, UDIV (division by zero gives zero; INT_MIN / -1 gives INT_MIN)
        if matches!(insn & 0x0ff0_f0f0, 0x0710_f010 | 0x0730_f010) {
            let (rd, divisor, dividend) = ((insn >> 16) & 0xf, (insn >> 8) & 0xf, insn & 0xf);
            if [rd, divisor, dividend].contains(&15) {
                return false;
            }
            let unsigned = insn & (1 << 21) != 0;
            self.get_to(dividend, 0, L_A);
            self.get_to(divisor, 0, L_B);
            self.i32c(0);
            self.lset(L_R);
            self.lget(L_B);
            self.if_();
            if unsigned {
                self.lget(L_A);
                self.lget(L_B);
                self.op(0x6e); // i32.div_u
                self.lset(L_R);
            } else {
                self.lget(L_B);
                self.i32c(u32::MAX);
                self.op(EQ);
                self.if_();
                self.i32c(0);
                self.lget(L_A);
                self.op(SUB);
                self.lset(L_R);
                self.else_();
                self.lget(L_A);
                self.lget(L_B);
                self.op(0x6d); // i32.div_s
                self.lset(L_R);
                self.end();
            }
            self.end();
            self.set(rd, L_R);
            return true;
        }
        false
    }

    /// A load into the PC left the block on the fast path; the interpreted slow path continues
    /// only if it loaded the next instruction's address. Returns whether the block ends.
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
            self.get(rm, pc);
            match (kind, amount) {
                (0, 0) => {}
                (0, a) => {
                    self.i32c(a);
                    self.op(SHL);
                }
                (1, 0) => {
                    self.op(0x1a); // drop: LSR #32 is 0
                    self.i32c(0);
                }
                (1, a) => {
                    self.i32c(a);
                    self.op(SHR_U);
                }
                (2, 0) => {
                    self.i32c(31);
                    self.op(SHR_S);
                }
                (2, a) => {
                    self.i32c(a);
                    self.op(SHR_S);
                }
                (_, a) => {
                    self.i32c(a);
                    self.op(ROTR);
                }
            }
            self.lset(L_B);
            None
        };
        let a = self.address(rn, pc, pre, up, offset);
        let size = if byte { 1 } else { 4 };
        self.probe(a, size, !load);
        self.if_();
        self.host_addr(a);
        if load {
            self.lget(L_HOST);
            if byte {
                self.mem(0x2d, 0, 0);
            } else {
                self.mem(I32_LOAD, 0, 0);
            }
            self.lset(L_D);
            if writeback {
                self.set(rn, L_ADDR);
            }
            if rd == 15 {
                self.exit_bx(L_D, n + 1);
            } else {
                self.set(rd, L_D);
            }
        } else {
            self.lget(L_HOST);
            self.get(rd, pc);
            if byte {
                self.mem(0x3a, 0, 0);
            } else {
                self.mem(I32_STORE, 0, 0);
            }
            if writeback {
                self.set(rn, L_ADDR);
            }
        }
        self.else_();
        self.call_interp(pc, n, false, 1);
        self.end();
        if load && rd == 15 {
            return Some(self.after_pc_load(insn, pc, n));
        }
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
            self.get_to(rm, pc, L_B);
            None
        };
        let a = self.address(rn, pc, pre, up, offset);
        if pair {
            // STRD when bit 5 (half) is set, LDRD otherwise
            self.same_page(a, 8);
            self.probe(a, 4, half);
            self.op(AND);
        } else {
            self.probe(a, if half { 2 } else { 1 }, !load);
        }
        self.if_();
        self.host_addr(a);
        if pair {
            if half {
                self.lget(L_HOST);
                self.get(rd, pc);
                self.mem(I32_STORE, 0, 0);
                self.lget(L_HOST);
                self.get(rd + 1, pc);
                self.mem(I32_STORE, 0, 4);
                if writeback {
                    self.set(rn, L_ADDR);
                }
            } else {
                self.lget(L_HOST);
                self.mem(I32_LOAD, 0, 0);
                self.lset(L_D);
                self.lget(L_HOST);
                self.mem(I32_LOAD, 0, 4);
                self.lset(L_E);
                self.set(rd, L_D);
                self.set(rd + 1, L_E);
                if writeback {
                    self.set(rn, L_ADDR);
                }
            }
        } else if load {
            self.lget(L_HOST);
            match (signed, half) {
                (false, true) => self.mem(0x2f, 0, 0), // i32.load16_u
                (true, false) => self.mem(0x2c, 0, 0), // i32.load8_s
                _ => self.mem(0x2e, 0, 0),             // i32.load16_s
            }
            self.lset(L_D);
            self.set(rd, L_D);
            if writeback {
                self.set(rn, L_ADDR);
            }
        } else {
            self.lget(L_HOST);
            self.get(rd, pc);
            self.mem(0x3b, 0, 0); // i32.store16
            if writeback {
                self.set(rn, L_ADDR);
            }
        }
        self.else_();
        self.call_interp(pc, n, false, 1);
        self.end();
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
        self.get_to(rn, pc, L_BASE);
        // the lowest address into L_ADDR
        self.lget(L_BASE);
        match (pre, up) {
            (false, true) => {}
            (true, true) => {
                self.i32c(4);
                self.op(ADD);
            }
            (false, false) => {
                self.i32c(bytes - 4);
                self.op(SUB);
            }
            (true, false) => {
                self.i32c(bytes);
                self.op(SUB);
            }
        }
        self.lset(L_ADDR);
        self.same_page(L_ADDR, bytes);
        self.probe(L_ADDR, 4, !load);
        self.op(AND);
        self.if_();
        self.host_addr(L_ADDR);
        let mut k = 0;
        for r in 0..16 {
            if list & (1 << r) == 0 {
                continue;
            }
            if load {
                self.lget(L_HOST);
                self.mem(I32_LOAD, 0, 4 * k);
                if r == 15 {
                    self.lset(L_E);
                } else {
                    self.lset(L_D);
                    self.set(r, L_D);
                }
            } else {
                self.lget(L_HOST);
                self.get(r, pc);
                self.mem(I32_STORE, 0, 4 * k);
            }
            k += 1;
        }
        if wb {
            self.lget(L_BASE);
            self.i32c(bytes);
            self.op(if up { ADD } else { SUB });
            self.lset(L_BASE);
            self.set(rn, L_BASE);
        }
        if load && list & 0x8000 != 0 {
            self.exit_bx(L_E, n + 1);
        }
        self.else_();
        self.call_interp(pc, n, false, 1);
        self.end();
        if load && list & 0x8000 != 0 {
            return Some(self.after_pc_load(insn, pc, n));
        }
        Some(false)
    }
}

// ---------------------------------------------------------------------- T32
impl W {
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
        let guard = it != 0 && it >> 4 != 14;
        if guard {
            self.cond(it >> 4);
            self.if_();
        }
        let end = self.thumb_body(hw1, hw2, wide, pc, n);
        if it != 0 {
            // ITAdvance
            self.it = if it & 7 == 0 { 0 } else { it & 0xe0 | (it << 1) & 0x1f };
        }
        if guard {
            self.end();
            return false;
        }
        end
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
        self.cond(cond);
        self.if_();
        self.exit_const(target, n + 1);
        self.end();
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
                        self.get(rm, pc);
                        self.i32c(pc4);
                        self.op(ADD);
                        self.lset(L_D);
                        self.exit_thumb_branch(L_D, n + 1);
                        T::Done(true)
                    }
                    0 => T::A32(0xe080_0000 | rd << 16 | rd << 12 | rm),
                    1 => T::A32(0xe150_0000 | rd << 16 | rm),
                    2 if rd == 15 => {
                        self.get_to(rm, pc, L_D);
                        self.exit_thumb_branch(L_D, n + 1);
                        T::Done(true)
                    }
                    2 => T::A32(0xe1a0_0000 | rd << 12 | rm),
                    _ => {
                        if rm == 15 {
                            return T::Interp(true);
                        }
                        self.get_to(rm, pc, L_D);
                        if hw & 0x80 != 0 {
                            self.set_const(14, pc.wrapping_add(2) | 1);
                        }
                        self.exit_bx(L_D, n + 1);
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
                    self.i32c((pc4 & !3).wrapping_add(imm8 * 4)); // ADR
                } else {
                    self.get(13, pc);
                    self.i32c(imm8 * 4);
                    self.op(ADD);
                }
                self.lset(L_R);
                self.set(r8, L_R);
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
                self.get(13, pc);
                self.i32c((hw & 0x7f) * 4);
                self.op(if hw & 0x80 != 0 { SUB } else { ADD });
                self.lset(L_R);
                self.set(13, L_R);
                T::Done(false)
            }
            0b0001 | 0b0011 | 0b1001 | 0b1011 => {
                // CBZ/CBNZ: leave for the target if taken
                let offset = (hw >> 3) & 0x1f | (hw >> 4) & 0x20;
                let target = pc.wrapping_add(4).wrapping_add(offset << 1);
                self.get(r0, pc);
                if hw & (1 << 11) == 0 {
                    self.op(EQZ);
                }
                self.if_();
                self.exit_const(target, n + 1);
                self.end();
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
                    self.i32c(value);
                    self.lset(L_B);
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
                            self.i32c(if sub { base.wrapping_sub(imm12) } else { base.wrapping_add(imm12) });
                        } else {
                            self.get(rn, pc);
                            self.i32c(imm12);
                            self.op(if sub { SUB } else { ADD });
                        }
                        self.lset(L_R);
                        self.set(rd, L_R);
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

    /// T32 data processing (modified immediate or shifted register) with operand 2 in L_B.
    /// `op` is the T32 opcode; Rd = PC with S selects TST/TEQ/CMN/CMP.
    fn t_dp(&mut self, op: u32, s: bool, rn: u32, rd: u32, carry: Carry, pc: u32) -> T {
        let compare = rd == 15 && s && matches!(op, 0 | 4 | 8 | 13);
        if rd == 15 && !compare || !matches!(op, 0..=4 | 8 | 10 | 11 | 13 | 14) {
            return T::Interp(true);
        }
        if rn == 15 {
            self.i32c(0); // Rn = PC reads as zero here (ORR/ORN: MOV/MVN)
            self.lset(L_A);
        } else {
            self.get_to(rn, pc, L_A);
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
            self.set(rd, L_R);
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
                0x3b if matches!((hw2 >> 4) & 15, 4..=6) => T::Done(false),
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
                self.ret(0);
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
        // (the accessed address's local, writeback)
        let (a, writeback) = if rn == 15 {
            if !load {
                return T::Interp(true);
            }
            let base = pc.wrapping_add(4) & !3;
            let imm = hw2 & 0xfff;
            self.i32c(if hw1 & 0x80 != 0 { base.wrapping_add(imm) } else { base.wrapping_sub(imm) });
            self.lset(L_ADDR);
            (L_ADDR, false)
        } else if hw1 & 0x80 != 0 {
            self.get(rn, pc);
            self.i32c(hw2 & 0xfff);
            self.op(ADD);
            self.lset(L_ADDR);
            (L_ADDR, false)
        } else if hw2 & 0x800 != 0 {
            let (p, u, w) = (hw2 & 0x400 != 0, hw2 & 0x200 != 0, hw2 & 0x100 != 0);
            if !p && !w || w && load && rt == rn {
                return T::Interp(true);
            }
            self.get_to(rn, pc, L_BASE);
            self.lget(L_BASE);
            self.i32c(hw2 & 0xff);
            self.op(if u { ADD } else { SUB });
            self.lset(L_ADDR);
            (if p { L_ADDR } else { L_BASE }, w)
        } else if hw2 & 0xfc0 == 0 {
            let rm = hw2 & 15;
            if rm == 15 {
                return T::Interp(true);
            }
            self.get(rn, pc);
            self.get(rm, pc);
            self.i32c((hw2 >> 4) & 3);
            self.op(SHL);
            self.op(ADD);
            self.lset(L_ADDR);
            (L_ADDR, false)
        } else {
            return T::Interp(true);
        };
        self.probe(a, size, !load);
        self.if_();
        self.host_addr(a);
        if load {
            self.lget(L_HOST);
            match (size, signed) {
                (1, false) => self.mem(0x2d, 0, 0),
                (1, true) => self.mem(0x2c, 0, 0),
                (2, false) => self.mem(0x2f, 0, 0),
                (2, true) => self.mem(0x2e, 0, 0),
                _ => self.mem(I32_LOAD, 0, 0),
            }
            self.lset(L_D);
            if writeback {
                self.set(rn, L_ADDR);
            }
            if rt == 15 {
                self.exit_bx(L_D, n + 1);
            } else {
                self.set(rt, L_D);
            }
        } else {
            self.lget(L_HOST);
            self.get(rt, pc);
            match size {
                1 => self.mem(0x3a, 0, 0),
                2 => self.mem(0x3b, 0, 0),
                _ => self.mem(I32_STORE, 0, 0),
            }
            if writeback {
                self.set(rn, L_ADDR);
            }
        }
        self.else_();
        self.call_interp(pc, n, false, 1);
        self.end();
        if load && rt == 15 {
            return T::Done(self.after_pc_load(0xe000_0000, pc, n));
        }
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
        self.same_page(a, 8);
        self.probe(a, 4, !l);
        self.op(AND);
        self.if_();
        self.host_addr(a);
        if l {
            self.lget(L_HOST);
            self.mem(I32_LOAD, 0, 0);
            self.lset(L_D);
            self.lget(L_HOST);
            self.mem(I32_LOAD, 0, 4);
            self.lset(L_E);
            if w {
                self.set(rn, L_ADDR);
            }
            self.set(rt, L_D);
            self.set(rt2, L_E);
        } else {
            self.lget(L_HOST);
            self.get(rt, pc);
            self.mem(I32_STORE, 0, 0);
            self.lget(L_HOST);
            self.get(rt2, pc);
            self.mem(I32_STORE, 0, 4);
            if w {
                self.set(rn, L_ADDR);
            }
        }
        self.else_();
        self.call_interp(pc, n, false, 1);
        self.end();
        T::Done(false)
    }
}

fn t32_dp_register(hw1: u32, hw2: u32) -> T {
    let (rn, rd, rm) = (hw1 & 15, (hw2 >> 8) & 15, hw2 & 15);
    let (op1, op2) = ((hw1 >> 4) & 15, (hw2 >> 4) & 15);
    if hw2 & 0xf000 != 0xf000 {
        return T::Interp(true);
    }
    if op1 & 8 == 0 && op2 == 0 {
        let s = (hw1 >> 4) & 1;
        return T::A32(0xe1a0_0010 | s << 20 | rd << 12 | rm << 8 | ((op1 >> 1) & 3) << 5 | rn);
    }
    if op1 & 8 == 0 && op2 & 8 != 0 {
        let kind = match op1 & 7 {
            0 => 0x6b,
            1 => 0x6f,
            2 => 0x68,
            3 => 0x6c,
            4 => 0x6a,
            5 => 0x6e,
            _ => return T::Interp(true),
        };
        return T::A32(0xe000_0000 | kind << 20 | 0x70 | rn << 16 | rd << 12 | (op2 & 3) << 10 | rm);
    }
    if op1 & 0xc == 8 && op2 & 0xc == 8 {
        return match (op1 & 3, op2 & 3) {
            (1, 0) => T::A32(0xe6bf_0f30 | rd << 12 | rm),
            (1, 1) => T::A32(0xe6bf_0fb0 | rd << 12 | rm),
            (1, 2) => T::A32(0xe6ff_0f30 | rd << 12 | rm),
            (3, 0) => T::A32(0xe16f_0f10 | rd << 12 | rm),
            _ => T::Interp(false),
        };
    }
    T::Interp(false)
}

fn t32_multiply(hw1: u32, hw2: u32) -> T {
    let (rn, ra, rd, rm) = (hw1 & 15, hw2 >> 12, (hw2 >> 8) & 15, hw2 & 15);
    match ((hw1 >> 4) & 7, (hw2 >> 4) & 3) {
        (0, 0) if ra == 15 => T::A32(0xe000_0090 | rd << 16 | rm << 8 | rn),
        (0, 0) => T::A32(0xe020_0090 | rd << 16 | ra << 12 | rm << 8 | rn),
        _ => T::Interp(false),
    }
}

fn t32_long_multiply(hw1: u32, hw2: u32) -> T {
    let (rn, lo, hi, rm) = (hw1 & 15, hw2 >> 12, (hw2 >> 8) & 15, hw2 & 15);
    let fields = hi << 16 | lo << 12 | rm << 8 | rn;
    match ((hw1 >> 4) & 7, (hw2 >> 4) & 15) {
        (0, 0) => T::A32(0xe0c0_0090 | fields),
        (1, 15) => T::A32(0xe710_f010 | hi << 16 | rm << 8 | rn),
        (2, 0) => T::A32(0xe080_0090 | fields),
        (3, 15) => T::A32(0xe730_f010 | hi << 16 | rm << 8 | rn),
        (4, 0) => T::A32(0xe0e0_0090 | fields),
        (6, 0) => T::A32(0xe0a0_0090 | fields),
        _ => T::Interp(false),
    }
}

// ---------------------------------------------------------------------- regions
/// Constant branch targets of an A32 instruction and whether straight-line code ends there
/// (for choosing the region; the emitter decides the real block ends).
fn scan_a32(insn: u32, pc: u32) -> (Option<u32>, bool) {
    let cond = insn >> 28;
    if cond == 15 {
        return (None, insn & 0x0e00_0000 == 0x0a00_0000);
    }
    if insn & 0x0e00_0000 == 0x0a00_0000 {
        let offset = (((insn & 0x00ff_ffff) << 8) as i32 >> 6) as u32;
        return (Some(pc.wrapping_add(8).wrapping_add(offset)), cond == 14);
    }
    let bx = insn & 0x0fff_ffd0 == 0x012f_ff10;
    (None, cond == 14 && (writes_pc(insn) || bx))
}

/// The same for Thumb (`hw1`, and `hw2` of a 32-bit instruction).
fn scan_t(hw1: u32, hw2: u32, pc: u32) -> (Option<u32>, bool) {
    let pc4 = pc.wrapping_add(4);
    if hw1 >> 11 < 0b11101 {
        return match hw1 >> 12 {
            0xd => match (hw1 >> 8) & 15 {
                14 | 15 => (None, true),
                _ => (Some(pc4.wrapping_add(sign_extend((hw1 & 0xff) << 1, 9))), false),
            },
            0xe => (Some(pc4.wrapping_add(sign_extend((hw1 & 0x7ff) << 1, 12))), true),
            0xb if hw1 & 0x0500 == 0x0100 => {
                let offset = (hw1 >> 3) & 0x1f | (hw1 >> 4) & 0x20;
                (Some(pc4.wrapping_add(offset << 1)), false)
            }
            0xb => (None, hw1 & 0xff00 == 0xbd00),
            0x4 => (None, hw1 & 0xff00 == 0x4700 || hw1 & 0xfc87 == 0x4487),
            _ => (None, false),
        };
    }
    if hw1 >> 11 == 0b11110 && hw2 & 0x8000 != 0 {
        let s = (hw1 >> 10) & 1;
        let (j1, j2) = ((hw2 >> 13) & 1, (hw2 >> 11) & 1);
        if (hw2 >> 12) & 5 == 0 {
            let cond = (hw1 >> 6) & 15;
            if cond < 14 {
                let imm = s << 20 | j2 << 19 | j1 << 18 | (hw1 & 0x3f) << 12 | (hw2 & 0x7ff) << 1;
                return (Some(pc4.wrapping_add(sign_extend(imm, 21))), false);
            }
            return (None, false);
        }
        let i1 = !(j1 ^ s) & 1;
        let i2 = !(j2 ^ s) & 1;
        let imm = s << 24 | i1 << 23 | i2 << 22 | (hw1 & 0x3ff) << 12 | (hw2 & 0x7ff) << 1;
        let target = pc4.wrapping_add(sign_extend(imm, 25));
        return match (hw2 >> 12) & 5 {
            1 | 5 => (Some(target), true),
            _ => (None, true),
        };
    }
    // LDM/POP with the PC, LDR PC
    let ldm_pc = hw1 & 0xffd0 == 0xe890 && hw2 & 0x8000 != 0 || hw1 & 0xffd0 == 0xe910 && hw2 & 0x8000 != 0;
    let ldr_pc = hw1 & 0xfff0 == 0xf8d0 && hw2 >> 12 == 15 || hw1 & 0xfff0 == 0xf850 && hw2 >> 12 == 15;
    (None, ldm_pc || ldr_pc)
}

impl Cpu {
    /// Translate the region at virtual `pc` (host address `code`) into a wasm function.
    pub(super) fn compile_wasm(&mut self, pc: u32, privileged: bool, thumb: bool, code: *const u8) -> Option<BlockFn> {
        let o = ctx();
        let mut w = W {
            c: Vec::with_capacity(4096),
            step_idx: h_step as *const () as usize as u32,
            rtab: (utlb_table(privileged, U_READ) * UT * 16) as u32,
            wtab: (utlb_table(privileged, U_WRITE) * UT * 16) as u32,
            ptab: 0,
            thumb,
            size: if thumb { 2 } else { 4 },
            it: 0,
            el0: self.el1.is_some(),
            region: Vec::new(),
            cur_bb: 0,
            nest: 0,
            o,
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
        let n_bbs = bbs.len();
        w.region = bbs.clone();
        // ---- prologue: the micro-TLB base and the guest flags into locals
        w.cpu_ld(w.o.utlb);
        w.lset(L_UT);
        w.flags_load();
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
            w.it = 0;
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
                        if w.it != 0 {
                            let it = w.it;
                            w.write_itstate(it);
                        }
                        w.add_instret(n);
                        w.exit_out_counted(at);
                        break;
                    }
                    let hw2 = if wide { rd16(at + 2) } else { 0 };
                    let end = w.thumb_insn(hw1, hw2, at, n);
                    n += 1;
                    at = at.wrapping_add(if wide { 4 } else { 2 });
                    if end {
                        break;
                    }
                    if n >= MAX_INSNS && w.it == 0 || at & 0xfff == 0 {
                        if w.it != 0 {
                            let it = w.it;
                            w.write_itstate(it);
                            w.add_instret(n);
                            w.exit_out_counted(at);
                        } else {
                            w.exit_const(at, n);
                        }
                        break;
                    }
                }
            } else {
                loop {
                    let end = w.insn(rd32(at), at, n);
                    n += 1;
                    at = at.wrapping_add(4);
                    if end {
                        break;
                    }
                    if n >= MAX_INSNS || at & 0xfff == 0 {
                        w.exit_const(at, n);
                        break;
                    }
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
        // SAFETY: a wasm function pointer is its table index; the page installed a function of
        // type (i32) -> i32 there, which is the BlockFn ABI
        Some(unsafe { std::mem::transmute::<usize, BlockFn>(idx as usize) })
    }
}

/// Wrap a function body into a module importing env.memory and env.table.
fn assemble(body: &[u8]) -> Vec<u8> {
    let mut m = b"\0asm\x01\0\0\0".to_vec();
    let section = |m: &mut Vec<u8>, id: u8, payload: &[u8]| {
        m.push(id);
        uleb(m, payload.len() as u64);
        m.extend_from_slice(payload);
    };
    // types: 0 = (i32) -> i32 (the block), 1 = (i32, i32, i32, i32) -> i32 (h_step)
    section(&mut m, 1, &[2, 0x60, 1, 0x7f, 1, 0x7f, 0x60, 4, 0x7f, 0x7f, 0x7f, 0x7f, 1, 0x7f]);
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
