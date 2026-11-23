//! The x86-64 backend (Linux, System V calling convention): the AArch64 backend's code shape
//! instruction for instruction, through `emu_core::jit_x64`. Guest registers live in `Cpu::x`
//! and every instruction loads and stores them, as there; the helpers are called at their
//! absolute addresses (`compile` is generic over the bus), so no helper table is passed.
use super::*;
use emu_core::jit_x64::{Alu, Asm, Cond, Label, Reg, Shift, R12, R13, R14, R15, R8, R9, RAX, RBP, RBX, RCX, RDI, RDX, RSI, RSP};
use std::ffi::{c_int, c_void};
use std::mem::offset_of;

pub const AVAILABLE: bool = true;

extern "C" {
    fn mmap(addr: *mut c_void, len: usize, prot: c_int, flags: c_int, fd: c_int, off: i64) -> *mut c_void;
    fn munmap(addr: *mut c_void, len: usize) -> c_int;
}

/// Executable memory for one core's regions, and the instruction tables `h_exec` reads.
pub struct Jit { base: *mut u8, size: usize, used: usize, tables: Vec<Box<[Insn]>> }
// SAFETY: the mapping and the tables are owned by this value and only reached through it.
unsafe impl Send for Jit {}

impl Drop for Jit {
    // SAFETY: the mapping was created by `new` with this size and is not used afterwards.
    fn drop(&mut self) { unsafe { munmap(self.base as *mut c_void, self.size); } }
}

const CODE_BYTES: usize = 32 << 20;

impl Jit {
    pub fn new() -> Option<Box<Jit>> {
        // SAFETY: a fresh anonymous mapping (PROT_READ|WRITE|EXEC, MAP_PRIVATE|ANONYMOUS);
        // failure is checked before the pointer is kept.
        let p = unsafe { mmap(std::ptr::null_mut(), CODE_BYTES, 7, 0x0002 | 0x0020, -1, 0) };
        if p as isize == -1 || p.is_null() { return None; }
        Some(Box::new(Jit { base: p as *mut u8, size: CODE_BYTES, used: 0, tables: Vec::new() }))
    }
    /// Forget all code (the caller forgets every block that refers to it).
    pub fn reset(&mut self) { self.used = 0; self.tables.clear(); }
    /// Code stays valid until `reset`.
    pub fn valid(&self) -> bool { true }
    pub fn adopt_epoch(&mut self) {}

    /// x86-64 keeps instruction fetch coherent with stores: no cache maintenance.
    fn write(&mut self, bytes: &[u8]) -> Option<u32> {
        let off = (self.used + 15) & !15;
        if off + bytes.len() > self.size { return None; }
        // SAFETY: the range is inside the mapping (checked above) and nothing runs code from it
        // while it is written.
        unsafe { std::ptr::copy_nonoverlapping(bytes.as_ptr(), self.base.add(off), bytes.len()); }
        self.used = off + bytes.len();
        Some(off as u32)
    }

    /// Run the region compiled at `code` with `budget` instructions left (at least its entry
    /// block's).
    /// # Safety
    /// `code` must come from `compile::<B>` on this `Jit` since its last `reset`, for a region
    /// whose entry block is still the one in memory (the caller's version check).
    pub unsafe fn call<B: Bus>(&self, code: u32, cpu: &mut Cpu, bus: &mut B, ctx: &mut Ctx, budget: u32) -> u64 {
        type F<B> = unsafe extern "C" fn(*mut Cpu, *mut B, *mut Ctx, u32) -> u64;
        let f: F<B> = std::mem::transmute(self.base.add(code as usize));
        f(cpu, bus, ctx, budget)
    }

    /// Compile a region: the code of each of its blocks as an entry (`Region::members` order);
    /// `None` when the code cache is full (the caller resets it) or the layout does not fit the
    /// encodings used.
    pub fn compile<B: Bus>(&mut self, r: &Region) -> Option<Vec<u32>> {
        let tables: Vec<Box<[Insn]>> = r.blocks.iter().map(|b| b.insns.clone().into_boxed_slice()).collect();
        let h = Helpers { exec: h_exec::<B> as *const () as u64, load: h_load::<B> as *const () as u64, store: h_store::<B> as *const () as u64 };
        let (code, entries) = emit(r, &tables, &h)?;
        let off = self.write(&code)?;
        self.tables.extend(tables);
        Some(entries.into_iter().map(|e| off + e as u32).collect())
    }
}

/// The helpers' addresses, for this bus type.
struct Helpers { exec: u64, load: u64, store: u64 }

// register plan (callee-saved, kept across helper calls)
const CPU: Reg = RBX;
const BUS: Reg = RBP;
const CTX: Reg = R12;
/// instructions the region has used before the current block
const DONE: Reg = R13;
/// budget left at the current block's start
const REM: Reg = R14;
/// the code versions (`Ctx::pv`)
const PV: Reg = R15;
/// a helper's status, from its return to the exit
const STATUS: Reg = RDX;
/// the frame below the six pushes: the seventh argument of `h_store`, the TLB slot (`Ctx::tlb`)
const FRAME: i32 = 24;
const ARG7: i32 = 0;
const TLB_SLOT: i32 = 8;

fn off(o: usize) -> i32 { o as i32 }
fn xo(r: u8) -> i32 { off(offset_of!(Cpu, x)) + 4 * r as i32 }
fn ld(a: &mut Asm, d: Reg, r: u8) { if r == 0 { a.mov_ri(d, 0) } else { a.load32(d, CPU, xo(r)) } }
fn st(a: &mut Asm, s: Reg, r: u8) { if r != 0 { a.store32(CPU, xo(r), s) } }
/// `eax op= x[rs2]`
fn op2(a: &mut Asm, op: Alu, rs2: u8) { if rs2 == 0 { a.alu_ri(op, false, RAX, 0) } else { a.alu_rm(op, RAX, CPU, xo(rs2)) } }
fn set_pc(a: &mut Asm, v: u32) { a.store32_imm(CPU, off(offset_of!(Cpu, pc)), v); }
fn set_last(a: &mut Asm, v: u32) { a.store32_imm(CTX, off(offset_of!(Ctx, last_pc)), v); }

fn add_counter(a: &mut Asm, o: usize, d: u32) {
    if d == 0 { return; }
    a.alu_mi(Alu::Add, true, CPU, off(o), d as i32);
}
fn flush(a: &mut Asm, p: &mut Pending) {
    add_counter(a, offset_of!(Cpu, insn_count), p.insns);
    add_counter(a, offset_of!(Cpu, cycle_count), p.insns);
    add_counter(a, offset_of!(Cpu, retired_count), p.retired);
    *p = Pending::default();
}
/// The first three arguments of every helper: cpu, bus, ctx.
fn args3(a: &mut Asm) { a.mov_rr64(RDI, CPU); a.mov_rr64(RSI, BUS); a.mov_rr64(RDX, CTX); }
fn call_helper(a: &mut Asm, f: u64) { a.mov_ri64(RAX, f); a.call_r(RAX); }

/// An early exit after instruction `k` of a block: the status is in STATUS, or `status`.
struct Exit { at: Label, k: u32, pc: u32, pend: Pending, set_pc: Option<u32>, status: Option<u32> }

/// Leave the region with END: the pc, the last instruction's pc, DONE.
fn leave(a: &mut Asm, pc: u32, last: u32, epilogue: Label) {
    set_pc(a, pc); set_last(a, last); a.mov_rr(RAX, DONE); a.jmp(epilogue);
}

/// Jump to `fail` unless block `b`'s code versions still hold.
fn check_versions(a: &mut Asm, b: &RBlock, fail: Label) {
    let n = if b.vidx[0] == b.vidx[1] { 1 } else { 2 };
    for v in 0..n {
        a.alu_mi(Alu::Cmp, false, PV, (b.vidx[v] * 4) as i32, b.ver[v] as i32);
        a.jcc(Cond::Ne, fail);
    }
}

/// The region's code and, per block, the byte offset of its entry.
fn emit(r: &Region, tables: &[Box<[Insn]>], h: &Helpers) -> Option<(Vec<u8>, Vec<usize>)> {
    // every displacement must fit a disp32: the Cpu fields and the code versions read
    let far = [offset_of!(Cpu, x) + 128, offset_of!(Cpu, pc), offset_of!(Cpu, insn_count), offset_of!(Cpu, cycle_count), offset_of!(Cpu, retired_count)];
    if far.iter().any(|&o| o > i32::MAX as usize) || r.blocks.iter().any(|b| b.vidx.iter().any(|&v| v >= (i32::MAX as u32) / 4)) { return None; }
    let mut a = Asm::new();
    let epilogue = a.label();
    let labels: Vec<Label> = r.blocks.iter().map(|_| a.label()).collect();
    // one entry per block: the prologue, then on to the block
    let mut entries = Vec::with_capacity(r.blocks.len());
    for &l in &labels {
        entries.push(a.here());
        for reg in [RBX, RBP, R12, R13, R14, R15] { a.push(reg); }
        a.alu_ri(Alu::Sub, true, RSP, FRAME);
        a.mov_rr64(CPU, RDI); a.mov_rr64(BUS, RSI); a.mov_rr64(CTX, RDX); a.mov_rr(REM, RCX);
        a.alu_rr(Alu::Xor, false, DONE, DONE);
        a.load64(PV, CTX, off(offset_of!(Ctx, pv)));
        a.load64(RAX, CTX, off(offset_of!(Ctx, tlb)));
        a.store64(RSP, TLB_SLOT, RAX);
        a.jmp(l);
    }
    let mut exits: Vec<Exit> = Vec::new();
    for (bi, b) in r.blocks.iter().enumerate() {
        a.bind(labels[bi]);
        block(&mut a, r, b, &tables[bi], &labels, &mut exits, epilogue, h);
    }
    for e in exits {
        a.bind(e.at);
        let mut pend = e.pend;
        flush(&mut a, &mut pend);
        if let Some(n) = e.set_pc { set_pc(&mut a, n); }
        set_last(&mut a, e.pc);
        if let Some(s) = e.status { a.mov_ri(STATUS, s); }
        a.shift_ri(Shift::Shl, true, STATUS, 32);
        a.lea32(RAX, DONE, (e.k + 1) as i32);
        a.alu_rr(Alu::Or, true, RAX, STATUS);
        a.jmp(epilogue);
    }
    a.bind(epilogue);
    a.alu_ri(Alu::Add, true, RSP, FRAME);
    for reg in [R15, R14, R13, R12, RBP, RBX] { a.pop(reg); }
    a.ret();
    Some((a.finish(), entries))
}

#[allow(clippy::too_many_arguments)]
fn block(a: &mut Asm, r: &Region, b: &RBlock, table: &[Insn], labels: &[Label], exits: &mut Vec<Exit>, epilogue: Label, h: &Helpers) {
    let mut p = Pending::default();
    let mut pc = b.pc;
    let n = b.insns.len();
    for (k, i) in b.insns.iter().enumerate() {
        let next = pc.wrapping_add(i.len as u32);
        let k32 = k as u32;
        let more = k + 1 < n;
        match kind(i.op) {
            Kind::Native => { p.insns += 1; p.retired += 1; native(a, i, pc, next); }
            Kind::Load(w) => {
                p.insns += 1;
                ld(a, RCX, i.rs1); if i.imm != 0 { a.alu_ri(Alu::Add, false, RCX, i.imm); }
                args3(a);
                a.mov_ri(R8, pc); a.mov_ri(R9, w);
                call_helper(a, h.load);
                a.mov_rr64(STATUS, RAX); a.shift_ri(Shift::Shr, true, STATUS, 32);
                let trap = a.label();
                a.alu_ri(Alu::Cmp, false, STATUS, TRAP as i32); a.jcc(Cond::E, trap);
                exits.push(Exit { at: trap, k: k32, pc, pend: p, set_pc: None, status: None });
                st(a, RAX, i.rd);
                let out = a.label();
                a.test_rr(false, STATUS, STATUS); a.jcc(Cond::Ne, out);
                exits.push(Exit { at: out, k: k32, pc, pend: p, set_pc: Some(next), status: None });
            }
            Kind::Store(w) => {
                p.insns += 1;
                ld(a, RCX, i.rs1); if i.imm != 0 { a.alu_ri(Alu::Add, false, RCX, i.imm); }
                ld(a, R8, i.rs2);
                args3(a);
                a.mov_ri(R9, pc); a.store32_imm(RSP, ARG7, w);
                call_helper(a, h.store);
                a.mov_rr(STATUS, RAX);
                let trap = a.label();
                a.alu_ri(Alu::Cmp, false, STATUS, TRAP as i32); a.jcc(Cond::E, trap);
                exits.push(Exit { at: trap, k: k32, pc, pend: p, set_pc: None, status: None });
                let out = a.label();
                a.test_rr(false, STATUS, STATUS); a.jcc(Cond::Ne, out);
                exits.push(Exit { at: out, k: k32, pc, pend: p, set_pc: Some(next), status: None });
                if more {
                    // the rest of this block may be stale
                    let stale = a.label();
                    check_versions(a, b, stale);
                    exits.push(Exit { at: stale, k: k32, pc, pend: p, set_pc: Some(next), status: Some(LEAVE) });
                }
            }
            Kind::Exec => {
                p.insns += 1;
                flush(a, &mut p);
                set_pc(a, pc);
                args3(a);
                a.mov_ri64(RCX, table.as_ptr().wrapping_add(k) as u64);
                a.mov_ri(R8, pc);
                call_helper(a, h.exec);
                a.mov_rr(STATUS, RAX);
                let out = a.label();
                a.test_rr(false, STATUS, STATUS); a.jcc(Cond::Ne, out);
                // exec set the pc
                exits.push(Exit { at: out, k: k32, pc, pend: p, set_pc: None, status: None });
                if more && may_write(i.op) {
                    let stale = a.label();
                    check_versions(a, b, stale);
                    exits.push(Exit { at: stale, k: k32, pc, pend: p, set_pc: None, status: Some(LEAVE) });
                }
            }
        }
        pc = next;
    }
    // the block ran whole
    flush(a, &mut p);
    a.alu_ri(Alu::Add, false, DONE, n as i32);
    a.alu_ri(Alu::Sub, false, REM, n as i32);
    let last = b.last_pc();
    match b.succ {
        Succ::Gone => { set_last(a, last); a.mov_rr(RAX, DONE); a.jmp(epilogue); }
        Succ::To(t) => transition(a, r, t, last, labels, epilogue),
        Succ::Branch { op, rs1, rs2, taken, not } => {
            ld(a, RAX, rs1); op2(a, Alu::Cmp, rs2);
            let c = match op { Op::Beq => Cond::E, Op::Bne => Cond::Ne, Op::Blt => Cond::L, Op::Bge => Cond::Ge, Op::Bltu => Cond::B, _ => Cond::Ae };
            let t_label = a.label();
            a.jcc(c, t_label);
            transition(a, r, not, last, labels, epilogue);
            a.bind(t_label);
            transition(a, r, taken, last, labels, epilogue);
        }
    }
}

/// Go on to the block at `t`: inside the region when the dispatch loop would run it whole from
/// the same lookup, else leave with the pc at `t`.
fn transition(a: &mut Asm, r: &Region, t: u32, last: u32, labels: &[Label], epilogue: Label) {
    if let Some(m) = r.target(t) {
        let b = &r.blocks[m];
        let out = a.label();
        a.alu_ri(Alu::Cmp, false, REM, b.insns.len() as i32); a.jcc(Cond::B, out);
        check_versions(a, b, out);
        // translated: the TLB slot must still map the page, executable in this mode
        let ok = a.label();
        a.load64(RAX, RSP, TLB_SLOT); a.test_rr(true, RAX, RAX); a.jcc(Cond::E, ok);
        a.alu_mi(Alu::Cmp, false, RAX, 0, (r.vpn + 1) as i32); a.jcc(Cond::Ne, out);
        a.alu_mi(Alu::Cmp, false, RAX, 4, r.phys_page as i32); a.jcc(Cond::Ne, out);
        a.load32(RCX, RAX, 8); a.alu_ri(Alu::And, false, RCX, (crate::mmu::pte::X | crate::mmu::pte::U) as i32);
        a.alu_rm(Alu::Cmp, RCX, CTX, off(offset_of!(Ctx, need))); a.jcc(Cond::Ne, out);
        a.bind(ok);
        a.jmp(labels[m]);
        a.bind(out);
    }
    leave(a, t, last, epilogue);
}

/// Host code for a native instruction at `pc` (a conditional branch is the block's end, see
/// `block`; `jal` writes its link here, `jalr` its link and the pc). The result is computed in
/// eax and stored to `rd`; with `rd` = x0 nothing is computed, except `jalr`'s pc.
fn native(a: &mut Asm, i: &Insn, pc: u32, next: u32) {
    use Op::*;
    let imm = i.imm as u32;
    let rd = i.rd;
    match i.op {
        Fence | Beq | Bne | Blt | Bge | Bltu | Bgeu => return,
        Jalr => {
            // the target first: rd may be rs1
            ld(a, RAX, i.rs1); if imm != 0 { a.alu_ri(Alu::Add, false, RAX, imm as i32); }
            a.alu_ri(Alu::And, false, RAX, -2);
            if rd != 0 { a.store32_imm(CPU, xo(rd), next); }
            a.store32(CPU, off(offset_of!(Cpu, pc)), RAX);
            return;
        }
        _ if rd == 0 => return,
        Lui => { a.store32_imm(CPU, xo(rd), imm); return; }
        Auipc => { a.store32_imm(CPU, xo(rd), pc.wrapping_add(imm)); return; }
        Jal => { a.store32_imm(CPU, xo(rd), next); return; }
        Addi if i.rs1 == 0 => { a.store32_imm(CPU, xo(rd), imm); return; }
        _ => {}
    }
    let rs1 = |a: &mut Asm| ld(a, RAX, i.rs1);
    let rr_cl = |a: &mut Asm| { ld(a, RAX, i.rs1); ld(a, RCX, i.rs2); };
    match i.op {
        Addi | Xori | Ori | Andi => {
            rs1(a);
            let op = match i.op { Addi => Alu::Add, Xori => Alu::Xor, Ori => Alu::Or, _ => Alu::And };
            a.alu_ri(op, false, RAX, imm as i32);
        }
        Slti | Sltiu => {
            rs1(a); a.alu_rr(Alu::Xor, false, RDX, RDX);
            a.alu_ri(Alu::Cmp, false, RAX, imm as i32);
            a.setcc(if i.op == Slti { Cond::L } else { Cond::B }, RDX);
            a.mov_rr(RAX, RDX);
        }
        Slli | Srli | Srai | Rori => {
            rs1(a);
            let op = match i.op { Slli => Shift::Shl, Srli => Shift::Shr, Srai => Shift::Sar, _ => Shift::Ror };
            if imm & 31 != 0 { a.shift_ri(op, false, RAX, (imm & 31) as u8); }
        }
        Add | Sub | Xor | Or | And => {
            rs1(a);
            let op = match i.op { Add => Alu::Add, Sub => Alu::Sub, Xor => Alu::Xor, Or => Alu::Or, _ => Alu::And };
            op2(a, op, i.rs2);
        }
        Sll | Srl | Sra | Rol | Ror => {
            rr_cl(a);
            let op = match i.op { Sll => Shift::Shl, Srl => Shift::Shr, Sra => Shift::Sar, Rol => Shift::Rol, _ => Shift::Ror };
            a.shift_cl(op, RAX);
        }
        Slt | Sltu => {
            rs1(a); a.alu_rr(Alu::Xor, false, RDX, RDX);
            op2(a, Alu::Cmp, i.rs2);
            a.setcc(if i.op == Slt { Cond::L } else { Cond::B }, RDX);
            a.mov_rr(RAX, RDX);
        }
        Mul => { rr_cl(a); a.imul_rr(false, RAX, RCX); }
        Mulh => { rr_cl(a); a.movsxd(RAX, RAX); a.movsxd(RCX, RCX); a.imul_rr(true, RAX, RCX); a.shift_ri(Shift::Shr, true, RAX, 32); }
        // both zero-extended by their 32-bit loads: the 64-bit product is the unsigned one
        Mulhu => { rr_cl(a); a.imul_rr(true, RAX, RCX); a.shift_ri(Shift::Shr, true, RAX, 32); }
        // x86 faults where RISC-V defines a result: divisor 0 (quotient all ones, remainder the
        // dividend) and, signed, INT_MIN / -1 (quotient INT_MIN, remainder 0)
        Div | Divu | Rem | Remu => {
            let signed = matches!(i.op, Div | Rem);
            let div = matches!(i.op, Div | Divu);
            rr_cl(a);
            let (zero, ovf, done) = (a.label(), a.label(), a.label());
            a.test_rr(false, RCX, RCX); a.jcc(Cond::E, zero);
            if signed {
                let go = a.label();
                a.alu_ri(Alu::Cmp, false, RCX, -1); a.jcc(Cond::Ne, go);
                a.alu_ri(Alu::Cmp, false, RAX, i32::MIN); a.jcc(Cond::E, ovf);
                a.bind(go);
                a.cdq(); a.idiv(RCX);
            } else {
                a.alu_rr(Alu::Xor, false, RDX, RDX); a.div(RCX);
            }
            if !div { a.mov_rr(RAX, RDX); }
            a.jmp(done);
            a.bind(zero);
            if div { a.mov_ri(RAX, u32::MAX); }
            a.jmp(done);
            a.bind(ovf);
            if !div { a.mov_ri(RAX, 0); }
            a.bind(done);
        }
        Sh1add | Sh2add | Sh3add => {
            rs1(a);
            a.shift_ri(Shift::Shl, false, RAX, match i.op { Sh1add => 1, Sh2add => 2, _ => 3 });
            op2(a, Alu::Add, i.rs2);
        }
        Andn | Orn => {
            ld(a, RCX, i.rs2); a.not(RCX); rs1(a);
            a.alu_rr(if i.op == Andn { Alu::And } else { Alu::Or }, false, RAX, RCX);
        }
        Xnor => { rs1(a); op2(a, Alu::Xor, i.rs2); a.not(RAX); }
        // bsr/bsf leave ZF set and the destination undefined for 0
        Clz => { ld(a, RCX, i.rs1); a.mov_ri(RDX, u32::MAX); a.bsr(RAX, RCX); a.cmov(Cond::E, RAX, RDX); a.neg(RAX); a.alu_ri(Alu::Add, false, RAX, 31); }
        Ctz => { ld(a, RCX, i.rs1); a.mov_ri(RDX, 32); a.bsf(RAX, RCX); a.cmov(Cond::E, RAX, RDX); }
        Max | Maxu | Min | Minu => {
            rr_cl(a); a.alu_rr(Alu::Cmp, false, RAX, RCX);
            // take rs2 when rs1 loses
            let c = match i.op { Max => Cond::L, Maxu => Cond::B, Min => Cond::G, _ => Cond::A };
            a.cmov(c, RAX, RCX);
        }
        SextB => { rs1(a); a.movsx8(RAX, RAX); }
        SextH => { rs1(a); a.movsx16(RAX, RAX); }
        ZextH => { rs1(a); a.movzx16(RAX, RAX); }
        Rev8 => { rs1(a); a.bswap(RAX); }
        Bclr | Binv | Bset => {
            ld(a, RCX, i.rs2); a.mov_ri(RDX, 1); a.shift_cl(Shift::Shl, RDX); rs1(a);
            match i.op {
                Bclr => { a.not(RDX); a.alu_rr(Alu::And, false, RAX, RDX); }
                Binv => a.alu_rr(Alu::Xor, false, RAX, RDX),
                _ => a.alu_rr(Alu::Or, false, RAX, RDX),
            }
        }
        Bext => { rr_cl(a); a.shift_cl(Shift::Shr, RAX); a.alu_ri(Alu::And, false, RAX, 1); }
        Bclri | Binvi | Bseti => {
            rs1(a);
            let bit = 1u32 << (imm & 31);
            match i.op { Bclri => a.alu_ri(Alu::And, false, RAX, !bit as i32), Binvi => a.alu_ri(Alu::Xor, false, RAX, bit as i32), _ => a.alu_ri(Alu::Or, false, RAX, bit as i32) }
        }
        Bexti => { rs1(a); if imm & 31 != 0 { a.shift_ri(Shift::Shr, false, RAX, (imm & 31) as u8); } a.alu_ri(Alu::And, false, RAX, 1); }
        _ => unreachable!("not a native instruction: {:?}", i.op),
    }
    st(a, RAX, rd);
}
