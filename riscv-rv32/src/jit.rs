//! Block JIT: a decoded block (`block.rs`) compiled to host code. The compiled block is the
//! block loop of `block::run` unrolled, with the same boundaries and exit rules, so the block
//! interpreter (and below it `exec::step`) stays the oracle and the two must leave bit-identical
//! machine state:
//!
//! - It runs only whole blocks (a budget cut runs the block loop), never while a hardware loop
//!   is counting (`hwlp::retire` then acts after every instruction) and never when the bus
//!   already asks for a break.
//! - Integer, multiply/divide, bit-manipulation and control-transfer instructions are host code.
//!   Loads and stores call `h_load`/`h_store`, which make the interpreter's translation, bus
//!   access and trap exactly; everything else calls `h_exec` (`exec::exec_insn`). After each
//!   helper the block ends as the loop would end it: on a trap, on `block_break`, after a store
//!   that moved the block's code versions, after a hardware-loop instruction.
//! - The counters (`insn_count`, `cycle_count`, `retired_count`) are added in batches: before
//!   `h_exec`, which may read them (CSRs), and at every exit, so whoever looks next sees the
//!   values the loop would have left.
//! - The bus sees `note_pc` before every access (in the helpers) and once more for the last
//!   instruction at exit (`Ctx::last_pc`), which is where the loop leaves it.
//!
//! Backends: AArch64 on macOS and Linux (`a64` below). Elsewhere [`AVAILABLE`] is false.
use crate::bus::Bus;
use crate::decode::{Insn, Op};
use crate::state::{exc, Cpu};
use emu_core::Trap;

/// How a compiled block ended (the high word of its result; the low word is the instructions
/// it used, a trapping one included).
pub const END: u32 = 0;
pub const TRAP: u32 = 1;
/// `block_break` after the last instruction used: the dispatch ends.
pub const STOP: u32 = 2;
/// The rest of the block must not run (stale code, a hardware loop): look up `pc` again.
pub const LEAVE: u32 = 3;

/// Instructions a block runs before it is compiled (`Blocks::jit_hot`).
pub const HOT: u16 = 8;

/// What a compiled block and its helpers share for one call.
#[repr(C)]
pub struct Ctx {
    /// the pc of the last instruction the block used (`note_pc` at exit)
    pub last_pc: u32,
    /// the block's code pages and the versions it was built against
    pub vidx: [u32; 2],
    pub ver: [u32; 2],
    pub trap: Option<Trap>,
}

impl Ctx {
    pub fn new(vidx: [u32; 2], ver: [u32; 2]) -> Ctx { Ctx { last_pc: 0, vidx, ver, trap: None } }
    #[inline(always)]
    fn stale<B: Bus>(&self, bus: &B) -> bool {
        let pv = bus.page_versions();
        pv.get(self.vidx[0] as usize) != Some(&self.ver[0]) || pv.get(self.vidx[1] as usize) != Some(&self.ver[1])
    }
}

/// Instructions after which the rest of a block may be stale (`block::may_write`).
#[inline(always)]
fn may_write(op: Op) -> bool {
    use Op::*;
    matches!(op, Sb | Sh | Sw | ScW | AmoSwapW | AmoAddW | AmoXorW | AmoAndW | AmoOrW | AmoMinW | AmoMaxW | AmoMinuW | AmoMaxuW | CmPush | Fsw) || op.is_pie()
}

// ---------------------------------------------------------------- helpers called from compiled code

/// Any instruction, through the interpreter: `block::run`'s body for one instruction after its
/// counting, which the compiled code has done.
extern "C" fn h_exec<B: Bus>(cpu: *mut Cpu, bus: *mut B, ctx: *mut Ctx, i: *const Insn, pc: u32) -> u32 {
    // SAFETY: compiled code passes the exclusive pointers `run` received, and the instruction
    // table the block was compiled with, which lives as long as its code.
    let (cpu, bus, ctx, i) = unsafe { (&mut *cpu, &mut *bus, &mut *ctx, &*i) };
    bus.note_pc(pc);
    if let Err(t) = crate::exec::exec_insn(cpu, bus, i, pc) { ctx.trap = Some(t); return TRAP; }
    cpu.retired_count += 1;
    let next = cpu.pc;
    crate::hwlp::retire(cpu, pc);
    if bus.block_break() { return STOP; }
    if cpu.pc != next || i.op.is_hwlp() || cpu.hwlp_count[0] | cpu.hwlp_count[1] != 0 { return LEAVE; }
    if may_write(i.op) && ctx.stale(bus) { return LEAVE; }
    END
}

/// Loads (`kind`: 0 lb, 1 lh, 2 lw, 3 lbu, 4 lhu), as `exec`'s `ld!`: the value in the low word,
/// the status in the high word. Retires the instruction unless it traps.
extern "C" fn h_load<B: Bus>(cpu: *mut Cpu, bus: *mut B, ctx: *mut Ctx, va: u32, pc: u32, kind: u32) -> u64 {
    // SAFETY: as `h_exec`.
    let (cpu, bus, ctx) = unsafe { (&mut *cpu, &mut *bus, &mut *ctx) };
    bus.note_pc(pc);
    let pa = if cpu.privileged {
        match cpu.translate(bus, va, crate::mmu::Access::Load) {
            Ok(p) => p,
            Err(c) => { cpu.trap(c, va, pc); ctx.trap = Some(Trap::Exception(c)); return (TRAP as u64) << 32; }
        }
    } else { va };
    let r = match kind {
        0 => bus.read8(pa).map(|v| v as i8 as i32 as u32),
        1 => bus.read16(pa).map(|v| v as i16 as i32 as u32),
        2 => bus.read32(pa),
        3 => bus.read8(pa).map(|v| v as u32),
        _ => bus.read16(pa).map(|v| v as u32),
    };
    match r {
        Ok(v) => { cpu.retired_count += 1; v as u64 | if bus.block_break() { (STOP as u64) << 32 } else { 0 } }
        Err(f) => {
            let c = if cpu.privileged { crate::exec::fault_cause(f, false) } else { exc::LOAD_ACCESS_FAULT };
            cpu.trap(c, va, pc); ctx.trap = Some(Trap::Exception(c)); (TRAP as u64) << 32
        }
    }
}

/// Stores (`kind`: 0 sb, 1 sh, 2 sw), as `exec`'s `st!`; returns the status.
extern "C" fn h_store<B: Bus>(cpu: *mut Cpu, bus: *mut B, ctx: *mut Ctx, va: u32, v: u32, pc: u32, kind: u32) -> u32 {
    // SAFETY: as `h_exec`.
    let (cpu, bus, ctx) = unsafe { (&mut *cpu, &mut *bus, &mut *ctx) };
    bus.note_pc(pc);
    let pa = if cpu.privileged {
        match cpu.translate(bus, va, crate::mmu::Access::Store) {
            Ok(p) => p,
            Err(c) => { cpu.trap(c, va, pc); ctx.trap = Some(Trap::Exception(c)); return TRAP; }
        }
    } else { va };
    let r = match kind { 0 => bus.write8(pa, v as u8), 1 => bus.write16(pa, v as u16), _ => bus.write32(pa, v) };
    if let Err(f) = r {
        let c = if cpu.privileged { crate::exec::fault_cause(f, true) } else { exc::STORE_ACCESS_FAULT };
        cpu.trap(c, va, pc); ctx.trap = Some(Trap::Exception(c)); return TRAP;
    }
    cpu.retired_count += 1;
    if bus.block_break() { return STOP; }
    if ctx.stale(bus) { return LEAVE; }
    END
}

/// The helper table compiled code calls through (field order = the offsets the backends use).
#[repr(C)]
pub struct Helpers { exec: *const (), load: *const (), store: *const () }
const H_EXEC: u32 = 0;
const H_LOAD: u32 = 8;
const H_STORE: u32 = 16;

impl Helpers {
    pub fn new<B: Bus>() -> Helpers {
        Helpers { exec: h_exec::<B> as *const (), load: h_load::<B> as *const (), store: h_store::<B> as *const () }
    }
}

/// How a block's instructions are compiled.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Kind { Native, Load(u32), Store(u32), Exec }

fn kind(op: Op) -> Kind {
    use Op::*;
    match op {
        Lb => Kind::Load(0), Lh => Kind::Load(1), Lw => Kind::Load(2), Lbu => Kind::Load(3), Lhu => Kind::Load(4),
        Sb => Kind::Store(0), Sh => Kind::Store(1), Sw => Kind::Store(2),
        Lui | Auipc | Jal | Jalr | Beq | Bne | Blt | Bge | Bltu | Bgeu
        | Addi | Slti | Sltiu | Xori | Ori | Andi | Slli | Srli | Srai
        | Add | Sub | Sll | Slt | Sltu | Xor | Srl | Sra | Or | And
        | Mul | Mulh | Mulhu | Div | Divu | Rem | Remu | Fence
        | Sh1add | Sh2add | Sh3add | Andn | Orn | Xnor | Clz | Ctz | Max | Maxu | Min | Minu
        | SextB | SextH | ZextH | Rol | Ror | Rori | Rev8
        | Bclr | Bclri | Bext | Bexti | Binv | Binvi | Bset | Bseti => Kind::Native,
        _ => Kind::Exec,
    }
}

#[cfg(all(target_arch = "aarch64", any(target_os = "macos", target_os = "linux")))]
pub use a64::{Jit, AVAILABLE};

#[cfg(not(all(target_arch = "aarch64", any(target_os = "macos", target_os = "linux"))))]
mod none {
    use super::*;
    pub const AVAILABLE: bool = false;
    /// No backend on this host: nothing is ever compiled.
    pub struct Jit;
    impl Jit {
        pub fn new() -> Option<Box<Jit>> { None }
        pub fn compile(&mut self, _pc: u32, _insns: &[Insn]) -> Option<u32> { None }
        pub fn reset(&mut self) {}
        /// # Safety
        /// Never called: no code exists.
        pub unsafe fn call<B: Bus>(&self, _code: u32, _cpu: &mut Cpu, _bus: &mut B, _ctx: &mut Ctx) -> u64 { unreachable!() }
    }
}
#[cfg(not(all(target_arch = "aarch64", any(target_os = "macos", target_os = "linux"))))]
pub use none::{Jit, AVAILABLE};

#[cfg(all(target_arch = "aarch64", any(target_os = "macos", target_os = "linux")))]
mod a64 {
    use super::*;
    use emu_core::jit_a64::{Asm, Cond, Label, Reg, SP, ZR};
    use std::ffi::{c_int, c_void};
    use std::mem::offset_of;

    pub const AVAILABLE: bool = true;

    extern "C" {
        fn mmap(addr: *mut c_void, len: usize, prot: c_int, flags: c_int, fd: c_int, off: i64) -> *mut c_void;
        fn munmap(addr: *mut c_void, len: usize) -> c_int;
        #[cfg(target_os = "macos")] fn pthread_jit_write_protect_np(enabled: c_int);
        #[cfg(target_os = "macos")] fn sys_icache_invalidate(start: *mut c_void, len: usize);
        #[cfg(target_os = "linux")] fn __clear_cache(start: *mut c_void, end: *mut c_void);
    }

    /// Executable memory for one core's blocks, and the instruction tables `h_exec` reads.
    pub struct Jit { base: *mut u8, size: usize, used: usize, tables: Vec<Box<[Insn]>> }
    // SAFETY: the mapping and the tables are owned by this value and only reached through it.
    unsafe impl Send for Jit {}

    impl Drop for Jit {
        // SAFETY: the mapping was created by `new` with this size and is not used afterwards.
        fn drop(&mut self) { unsafe { munmap(self.base as *mut c_void, self.size); } }
    }

    const CODE_BYTES: usize = 16 << 20;

    impl Jit {
        pub fn new() -> Option<Box<Jit>> {
            #[cfg(target_os = "macos")] let flags = 0x0002 | 0x1000 | 0x0800;   // PRIVATE | ANON | JIT
            #[cfg(target_os = "linux")] let flags = 0x0002 | 0x0020;            // PRIVATE | ANONYMOUS
            // SAFETY: a fresh anonymous mapping; failure is checked before the pointer is kept.
            let p = unsafe { mmap(std::ptr::null_mut(), CODE_BYTES, 7, flags, -1, 0) };
            if p as isize == -1 || p.is_null() { return None; }
            Some(Box::new(Jit { base: p as *mut u8, size: CODE_BYTES, used: 0, tables: Vec::new() }))
        }
        /// Forget all code (the caller forgets every block that refers to it).
        pub fn reset(&mut self) { self.used = 0; self.tables.clear(); }

        fn write(&mut self, words: &[u32]) -> Option<u32> {
            let bytes = words.len() * 4;
            if self.used + bytes > self.size { return None; }
            let off = self.used;
            // SAFETY: the range is inside the mapping (checked above) and nothing runs code
            // from it while it is written.
            unsafe {
                #[cfg(target_os = "macos")] pthread_jit_write_protect_np(0);
                std::ptr::copy_nonoverlapping(words.as_ptr() as *const u8, self.base.add(off), bytes);
                #[cfg(target_os = "macos")] pthread_jit_write_protect_np(1);
                #[cfg(target_os = "macos")] sys_icache_invalidate(self.base.add(off) as *mut c_void, bytes);
                #[cfg(target_os = "linux")] __clear_cache(self.base.add(off) as *mut c_void, self.base.add(off + bytes) as *mut c_void);
            }
            self.used += bytes;
            Some(off as u32)
        }

        /// Run the block compiled at `code`.
        /// # Safety
        /// `code` must come from `compile` on this `Jit` since its last `reset`, for a block
        /// whose instructions are still the ones in memory (the caller's version check).
        pub unsafe fn call<B: Bus>(&self, code: u32, cpu: &mut Cpu, bus: &mut B, ctx: &mut Ctx) -> u64 {
            let h = Helpers::new::<B>();
            type F<B> = unsafe extern "C" fn(*mut Cpu, *mut B, *const Helpers, *mut Ctx) -> u64;
            let f: F<B> = std::mem::transmute(self.base.add(code as usize));
            f(cpu, bus, &h, ctx)
        }

        /// Compile the block of `insns` at `pc`; `None` when the code cache is full (the caller
        /// resets it) or the layout does not fit the encodings used.
        pub fn compile(&mut self, pc: u32, insns: &[Insn]) -> Option<u32> {
            let table: Box<[Insn]> = insns.into();
            let words = emit(pc, &table)?;
            let off = self.write(&words)?;
            self.tables.push(table);
            Some(off)
        }
    }

    // register plan (callee-saved, kept across helper calls)
    const CPU: Reg = 19;
    const BUS: Reg = 20;
    const HLP: Reg = 21;
    const CTX: Reg = 22;
    /// a helper's status, from its return to the exit
    const STATUS: Reg = 11;

    fn xo(r: u8) -> u32 { offset_of!(Cpu, x) as u32 + 4 * r as u32 }

    /// Load guest register `r` into `d` (x0 reads as zero).
    fn ld(a: &mut Asm, d: Reg, r: u8) { if r == 0 { a.mov(d, ZR) } else { a.ldr(d, CPU, xo(r)) } }
    /// Store `s` to guest register `r` (writes to x0 are dropped).
    fn st(a: &mut Asm, s: Reg, r: u8) { if r != 0 { a.str(s, CPU, xo(r)) } }

    fn raw(a: &mut Asm, w: u32) { a.code.push(w); }
    fn orn(a: &mut Asm, d: Reg, n: Reg, m: Reg) { raw(a, 0x2a20_0000 | m << 16 | n << 5 | d); }
    fn eon(a: &mut Asm, d: Reg, n: Reg, m: Reg) { raw(a, 0x4a20_0000 | m << 16 | n << 5 | d); }
    fn rbit(a: &mut Asm, d: Reg, n: Reg) { raw(a, 0x5ac0_0000 | n << 5 | d); }
    fn rev(a: &mut Asm, d: Reg, n: Reg) { raw(a, 0x5ac0_0800 | n << 5 | d); }
    fn rorv(a: &mut Asm, d: Reg, n: Reg, m: Reg) { raw(a, 0x1ac0_2c00 | m << 16 | n << 5 | d); }
    fn ror_imm(a: &mut Asm, d: Reg, n: Reg, sh: u32) { raw(a, 0x1380_0000 | n << 16 | (sh & 31) << 10 | n << 5 | d); }

    /// Add `d` to the 64-bit counter at `off` in the Cpu.
    fn add_counter(a: &mut Asm, off: u32, d: u32) {
        if d == 0 { return; }
        a.ldr_x(9, CPU, off); a.add_imm_x(9, 9, d); a.str_x(9, CPU, off);
    }

    /// Counts not yet added to the Cpu: instructions (`insn_count`, `cycle_count`) and those
    /// retired by host code (helpers count their own).
    #[derive(Clone, Copy, Default)]
    struct Pending { insns: u32, retired: u32 }

    fn flush(a: &mut Asm, p: &mut Pending) {
        add_counter(a, offset_of!(Cpu, insn_count) as u32, p.insns);
        add_counter(a, offset_of!(Cpu, cycle_count) as u32, p.insns);
        add_counter(a, offset_of!(Cpu, retired_count) as u32, p.retired);
        *p = Pending::default();
    }

    /// An exit after the helper of instruction `k` returned a non-zero status (in w9).
    struct Exit { at: Label, k: u32, pc: u32, pend: Pending, set_pc: Option<u32> }

    fn call_helper(a: &mut Asm, off: u32) { a.ldr_x(9, HLP, off); a.blr(9); }

    fn emit(pc0: u32, insns: &[Insn]) -> Option<Vec<u32>> {
        // the encodings used reach these offsets
        if xo(31) >= 16384 || offset_of!(Cpu, insn_count).max(offset_of!(Cpu, cycle_count)).max(offset_of!(Cpu, retired_count)) >= 32768
            || offset_of!(Cpu, pc) >= 16384 { return None; }
        let mut a = Asm::new();
        let epilogue = a.label();
        // prologue
        a.stp_pre(29, 30, SP, -48);
        a.stp(CPU, BUS, SP, 16);
        a.stp(HLP, CTX, SP, 32);
        a.mov_x(CPU, 0); a.mov_x(BUS, 1); a.mov_x(HLP, 2); a.mov_x(CTX, 3);

        let pc_off = offset_of!(Cpu, pc) as u32;
        let last_off = offset_of!(Ctx, last_pc) as u32;
        let mut p = Pending::default();
        let mut exits: Vec<Exit> = Vec::new();
        let mut pc = pc0;
        let n = insns.len();
        let mut pc_set = false;
        for (k, i) in insns.iter().enumerate() {
            let next = pc.wrapping_add(i.len as u32);
            let k32 = k as u32;
            match kind(i.op) {
                Kind::Native => {
                    p.insns += 1; p.retired += 1;
                    if native(&mut a, i, pc, next, pc_off) { pc_set = true; }
                }
                Kind::Load(w) => {
                    p.insns += 1;
                    ld(&mut a, 3, i.rs1); a.mov32(9, i.imm as u32); a.add(3, 3, 9);
                    a.mov_x(0, CPU); a.mov_x(1, BUS); a.mov_x(2, CTX);
                    a.mov32(4, pc); a.mov32(5, w);
                    call_helper(&mut a, H_LOAD);
                    a.lsr_imm_x(STATUS, 0, 32);
                    let trap = a.label();
                    a.cmp_imm(STATUS, TRAP); a.b_cond(Cond::Eq, trap);
                    exits.push(Exit { at: trap, k: k32, pc, pend: p, set_pc: None });
                    st(&mut a, 0, i.rd);
                    let out = a.label();
                    a.cbnz(STATUS, out);
                    exits.push(Exit { at: out, k: k32, pc, pend: p, set_pc: Some(next) });
                }
                Kind::Store(w) => {
                    p.insns += 1;
                    ld(&mut a, 3, i.rs1); a.mov32(9, i.imm as u32); a.add(3, 3, 9);
                    ld(&mut a, 4, i.rs2);
                    a.mov_x(0, CPU); a.mov_x(1, BUS); a.mov_x(2, CTX);
                    a.mov32(5, pc); a.mov32(6, w);
                    call_helper(&mut a, H_STORE);
                    a.mov(STATUS, 0);
                    let trap = a.label();
                    a.cmp_imm(STATUS, TRAP); a.b_cond(Cond::Eq, trap);
                    exits.push(Exit { at: trap, k: k32, pc, pend: p, set_pc: None });
                    let out = a.label();
                    a.cbnz(STATUS, out);
                    exits.push(Exit { at: out, k: k32, pc, pend: p, set_pc: Some(next) });
                }
                Kind::Exec => {
                    // the interpreter may read the counters (CSRs) and the pc
                    p.insns += 1;
                    flush(&mut a, &mut p);
                    a.mov32(9, pc); a.str(9, CPU, pc_off);
                    a.mov_x(0, CPU); a.mov_x(1, BUS); a.mov_x(2, CTX);
                    a.mov64(3, insns.as_ptr().wrapping_add(k) as u64);
                    a.mov32(4, pc);
                    call_helper(&mut a, H_EXEC);
                    a.mov(STATUS, 0);
                    let out = a.label();
                    a.cbnz(STATUS, out);
                    // exec set the pc: a terminator leaves it where execution goes
                    exits.push(Exit { at: out, k: k32, pc, pend: p, set_pc: None });
                    pc_set = k + 1 == n;
                }
            }
            pc = next;
        }
        // the block ran to its end
        flush(&mut a, &mut p);
        if !pc_set { a.mov32(9, pc); a.str(9, CPU, pc_off); }
        let last = last_pc(pc0, insns, n as u32 - 1);
        a.mov32(9, last); a.str(9, CTX, last_off);
        a.mov32(0, n as u32);
        a.b(epilogue);
        // the early exits: status in w11 (the counters use x9, the pc w10)
        for e in exits {
            a.bind(e.at);
            let mut pend = e.pend;
            flush(&mut a, &mut pend);
            if let Some(next) = e.set_pc { a.mov32(10, next); a.str(10, CPU, pc_off); }
            a.mov32(10, e.pc); a.str(10, CTX, last_off);
            a.lsl_imm_x(0, STATUS, 32);
            a.mov32(10, e.k + 1);
            a.orr_x(0, 0, 10);
            a.b(epilogue);
        }
        a.bind(epilogue);
        a.ldp(HLP, CTX, SP, 32);
        a.ldp(CPU, BUS, SP, 16);
        a.ldp_post(29, 30, SP, 48);
        a.ret();
        Some(a.finish())
    }

    fn last_pc(pc0: u32, insns: &[Insn], k: u32) -> u32 {
        insns[..k as usize].iter().fold(pc0, |pc, i| pc.wrapping_add(i.len as u32))
    }

    /// Host code for a native instruction at `pc`; true when it set the pc (a control transfer).
    fn native(a: &mut Asm, i: &Insn, pc: u32, next: u32, pc_off: u32) -> bool {
        use Op::*;
        let imm = i.imm as u32;
        // operands: w0 = rs1, w1 = rs2 or the immediate; result in w2
        let rr = |a: &mut Asm| { ld(a, 0, i.rs1); ld(a, 1, i.rs2); };
        let ri = |a: &mut Asm| { ld(a, 0, i.rs1); a.mov32(1, imm); };
        match i.op {
            Fence => return false,
            Lui => { a.mov32(2, imm); }
            Auipc => { a.mov32(2, pc.wrapping_add(imm)); }
            Jal => {
                if i.rd != 0 { a.mov32(2, next); st(a, 2, i.rd); }
                a.mov32(9, imm); a.str(9, CPU, pc_off);
                return true;
            }
            Jalr => {
                ld(a, 0, i.rs1); a.mov32(1, imm); a.add(3, 0, 1);
                a.mov32(4, !1); a.and(3, 3, 4);
                if i.rd != 0 { a.mov32(2, next); st(a, 2, i.rd); }
                a.str(3, CPU, pc_off);
                return true;
            }
            Beq | Bne | Blt | Bge | Bltu | Bgeu => {
                rr(a); a.cmp(0, 1);
                let c = match i.op { Beq => Cond::Eq, Bne => Cond::Ne, Blt => Cond::Lt, Bge => Cond::Ge, Bltu => Cond::Lo, _ => Cond::Hs };
                a.mov32(2, imm); a.mov32(3, next); a.csel(2, 2, 3, c);
                a.str(2, CPU, pc_off);
                return true;
            }
            Addi => { ri(a); a.add(2, 0, 1); }
            Slti => { ri(a); a.cmp(0, 1); a.cset(2, Cond::Lt); }
            Sltiu => { ri(a); a.cmp(0, 1); a.cset(2, Cond::Lo); }
            Xori => { ri(a); a.eor(2, 0, 1); }
            Ori => { ri(a); a.orr(2, 0, 1); }
            Andi => { ri(a); a.and(2, 0, 1); }
            Slli => { ld(a, 0, i.rs1); a.lsl_imm(2, 0, imm & 31); }
            Srli => { ld(a, 0, i.rs1); a.lsr_imm(2, 0, imm & 31); }
            Srai => { ld(a, 0, i.rs1); a.asr_imm(2, 0, imm & 31); }
            Add => { rr(a); a.add(2, 0, 1); }
            Sub => { rr(a); a.sub(2, 0, 1); }
            Sll => { rr(a); a.lslv(2, 0, 1); }
            Srl => { rr(a); a.lsrv(2, 0, 1); }
            Sra => { rr(a); a.asrv(2, 0, 1); }
            Slt => { rr(a); a.cmp(0, 1); a.cset(2, Cond::Lt); }
            Sltu => { rr(a); a.cmp(0, 1); a.cset(2, Cond::Lo); }
            Xor => { rr(a); a.eor(2, 0, 1); }
            Or => { rr(a); a.orr(2, 0, 1); }
            And => { rr(a); a.and(2, 0, 1); }
            Mul => { rr(a); a.mul(2, 0, 1); }
            Mulh => { rr(a); a.smull(2, 0, 1); a.lsr_imm_x(2, 2, 32); }
            Mulhu => { rr(a); a.umull(2, 0, 1); a.lsr_imm_x(2, 2, 32); }
            // AArch64 divides by zero to 0, RISC-V to all ones; the signed overflow case agrees
            Div | Divu => {
                rr(a);
                if i.op == Div { a.sdiv(2, 0, 1) } else { a.udiv(2, 0, 1) }
                a.movn(3, 0, 0); a.cmp_imm(1, 0); a.csel(2, 3, 2, Cond::Eq);
            }
            // a - (a / b) * b: b = 0 leaves a, the overflow case gives 0, as RISC-V defines
            Rem | Remu => {
                rr(a);
                if i.op == Rem { a.sdiv(2, 0, 1) } else { a.udiv(2, 0, 1) }
                a.msub(2, 2, 1, 0);
            }
            Sh1add | Sh2add | Sh3add => {
                rr(a);
                let sh = match i.op { Sh1add => 1, Sh2add => 2, _ => 3 };
                a.add_lsl(2, 1, 0, sh);
            }
            Andn => { rr(a); a.bic(2, 0, 1); }
            Orn => { rr(a); orn(a, 2, 0, 1); }
            Xnor => { rr(a); eon(a, 2, 0, 1); }
            Clz => { ld(a, 0, i.rs1); a.clz(2, 0); }
            Ctz => { ld(a, 0, i.rs1); rbit(a, 2, 0); a.clz(2, 2); }
            Max | Maxu | Min | Minu => {
                rr(a); a.cmp(0, 1);
                let c = match i.op { Max => Cond::Gt, Maxu => Cond::Hi, Min => Cond::Lt, _ => Cond::Lo };
                a.csel(2, 0, 1, c);
            }
            SextB => { ld(a, 0, i.rs1); a.sbfx(2, 0, 0, 8); }
            SextH => { ld(a, 0, i.rs1); a.sbfx(2, 0, 0, 16); }
            ZextH => { ld(a, 0, i.rs1); a.ubfx(2, 0, 0, 16); }
            Rol => { rr(a); a.neg(1, 1); rorv(a, 2, 0, 1); }
            Ror => { rr(a); rorv(a, 2, 0, 1); }
            Rori => { ld(a, 0, i.rs1); ror_imm(a, 2, 0, imm & 31); }
            Rev8 => { ld(a, 0, i.rs1); rev(a, 2, 0); }
            Bclr | Binv | Bset | Bext => {
                rr(a);
                if i.op == Bext { a.lsrv(2, 0, 1); a.ubfx(2, 2, 0, 1); } else {
                    a.mov32(3, 1); a.lslv(3, 3, 1);
                    match i.op { Bclr => a.bic(2, 0, 3), Binv => a.eor(2, 0, 3), _ => a.orr(2, 0, 3) }
                }
            }
            Bclri | Binvi | Bseti => {
                ld(a, 0, i.rs1); a.mov32(1, 1 << (imm & 31));
                match i.op { Bclri => a.bic(2, 0, 1), Binvi => a.eor(2, 0, 1), _ => a.orr(2, 0, 1) }
            }
            Bexti => { ld(a, 0, i.rs1); a.ubfx(2, 0, imm & 31, 1); }
            _ => unreachable!("not a native instruction: {:?}", i.op),
        }
        st(a, 2, i.rd);
        false
    }
}
