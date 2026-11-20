//! The block JIT's WebAssembly backend (browser builds).
//!
//! A hot block becomes a tiny wasm module: `(func (param $cpu i32) (result i32))` that imports
//! the emulator's linear memory and function table. Guest registers live in the `Cpu` struct
//! in that memory, so generated code reads and writes them with plain loads and stores.
//! Integer data processing (with exact NZCV), moves, ADR, bitfield extracts, conditional selects,
//! multiply-add and all branches are generated; every other instruction calls the interpreter
//! for that one instruction (`h_step`, through the table), exactly like the native backend.
//! The page compiles the bytes synchronously (`new WebAssembly.Module` in a worker) and installs
//! the function in the table (`sbc_jit_compile`); Rust then calls it as a function pointer.
#![cfg(target_arch = "wasm32")]
use crate::cpu::UT;
use crate::cpu::Cpu;

#[link(wasm_import_module = "env")]
extern "C" {
    /// Provided by the page: compile a module, install its export "f" in the function table,
    /// return the table index (or -1). The page reuses slots freed by `sbc_jit_flushed`.
    fn sbc_jit_compile(ptr: *const u8, len: usize) -> i32;
    /// All compiled blocks were discarded: their table slots may be reused.
    fn sbc_jit_reset();
}

/// Tell the page the compiled code is gone (called from `Jit::flush`).
pub(crate) fn reset() { unsafe { sbc_jit_reset() } }

/// Interpreter for one instruction (called by generated code through the table).
/// 0 = continue, 1 = leave the block (pc set), 2 = trap stored in jit_trap.
unsafe extern "C" fn h_step(cpu: *mut Cpu, expect_next: u64) -> u32 {
    let cpu = &mut *cpu;
    if super::jit::PROFILE.load(std::sync::atomic::Ordering::Relaxed) { super::jit::profile_fallback(cpu); }
    let bus = &mut *cpu.jit_bus.expect("bus");
    let r = cpu.exec_one(bus);
    // as the native h_step: system/branch-register instructions and device accesses stop chaining
    if super::jit::is_block_end(cpu.cur_insn) { cpu.chain_stop = true; }
    match r {
        Ok(()) => if cpu.pc == expect_next && !cpu.chain_stop { 0 } else { 1 },
        Err(t) => { cpu.jit_trap = Some(t); 2 }
    }
}

fn bits(x: u32, hi: u32, lo: u32) -> u32 { (x >> lo) & ((1u32 << (hi - lo + 1)) - 1) }
fn bit(x: u32, b: u32) -> bool { (x >> b) & 1 != 0 }

// ---------------------------------------------------------------------- wasm encoding
fn uleb(out: &mut Vec<u8>, mut v: u64) { loop { let b = (v & 0x7f) as u8; v >>= 7; if v == 0 { out.push(b); break; } out.push(b | 0x80); } }
fn sleb(out: &mut Vec<u8>, mut v: i64) {
    loop {
        let b = (v & 0x7f) as u8;
        v >>= 7;
        let done = (v == 0 && b & 0x40 == 0) || (v == -1 && b & 0x40 != 0);
        out.push(if done { b } else { b | 0x80 });
        if done { break; }
    }
}

const L_CPU: u32 = 0; // param
const L_A: u32 = 1; // i64 temporaries
const L_B: u32 = 2;
const L_R: u32 = 3;
const L_ST: u32 = 4; // i32 temporary
const L_C: u32 = 5; // more i64 temporaries
const L_D: u32 = 6;
const L_E: u32 = 7;
const L_IDX: u32 = 8; // i32: the region's next basic block

struct W { c: Vec<u8>, off_x: u32, off_sp_slot: u32, off_pc: u32, off_nzcv: u32, off_instret: u32, pending: u64, step_idx: u32,
           off_utlb: u32, off_ram: u32, off_excl: u32, el: u8, off_sp0: u32, off_tpidr: u32,
           /** basic blocks of the region (start pcs; index = dispatch value) */ region: Vec<u64>,
           /** the basic block being emitted */ cur_bb: usize, off_chain_until: u32 }

impl W {
    fn op(&mut self, b: u8) { self.c.push(b); }
    fn lget(&mut self, l: u32) { self.op(0x20); uleb(&mut self.c, l as u64); }
    fn lset(&mut self, l: u32) { self.op(0x21); uleb(&mut self.c, l as u64); }
    fn i64c(&mut self, v: i64) { self.op(0x42); sleb(&mut self.c, v); }
    fn i32c(&mut self, v: i32) { self.op(0x41); sleb(&mut self.c, v as i64); }
    fn load64(&mut self, off: u32) { self.lget(L_CPU); self.op(0x29); uleb(&mut self.c, 3); uleb(&mut self.c, off as u64); }
    /// value on the stack -> [cpu + off]; uses L_R
    fn store64(&mut self, off: u32) { self.lset(L_R); self.lget(L_CPU); self.lget(L_R); self.op(0x37); uleb(&mut self.c, 3); uleb(&mut self.c, off as u64); }
    fn load32(&mut self, off: u32) { self.lget(L_CPU); self.op(0x28); uleb(&mut self.c, 2); uleb(&mut self.c, off as u64); }
    fn store32(&mut self, off: u32) { self.lset(L_ST); self.lget(L_CPU); self.lget(L_ST); self.op(0x36); uleb(&mut self.c, 2); uleb(&mut self.c, off as u64); }

    /// guest register r (31 = SP or XZR)
    fn get_x(&mut self, r: u32, sp: bool) {
        if r == 31 { if sp { self.load64(self.off_sp_slot) } else { self.i64c(0) } } else { self.load64(self.off_x + 8 * r) }
    }
    /// stack value -> guest register r (32-bit results are masked)
    fn set_x(&mut self, r: u32, sp: bool, sf: bool) {
        if !sf { self.i64c(0xffff_ffff); self.op(0x83); }
        if r == 31 { if sp { self.store64(self.off_sp_slot) } else { self.op(0x1a) } } else { self.store64(self.off_x + 8 * r) }
    }
    fn mask32(&mut self, sf: bool) { if !sf { self.i64c(0xffff_ffff); self.op(0x83); } }

    fn flush_instret(&mut self) {
        if self.pending == 0 { return; }
        self.load64(self.off_instret);
        self.i64c(self.pending as i64);
        self.op(0x7c);
        self.store64(self.off_instret);
        self.pending = 0;
    }
    fn set_pc(&mut self, pc: u64) { self.i64c(pc as i64); self.store64(self.off_pc); }
    fn ret(&mut self, status: i32) { self.i32c(status); self.op(0x0f); }
    /// Leave the basic block for constant `target` (instret flushed). A target inside the region
    /// is a branch back to the dispatch loop while the budget lasts; `nest` = structured blocks
    /// opened since the basic block's start.
    fn exit_const(&mut self, target: u64, nest: u32) {
        if let Some(k) = self.region.iter().position(|&s| s == target) {
            self.load64(self.off_instret); self.load64(self.off_chain_until); self.op(0x54); // i64.lt_u
            self.op(0x04); self.op(0x40);
            self.i32c(k as i32); self.lset(L_IDX);
            self.op(0x0c); uleb(&mut self.c, (self.region.len() - self.cur_bb) as u64 + nest as u64 + 1); // br loop
            self.op(0x0b);
        }
        self.set_pc(target);
        self.ret(0);
    }

    /// NZCV for a result r (L_R) of an add/sub of a, b (L_A, L_B): sub = a - b.
    fn flags_addsub(&mut self, sf: bool, sub: bool) {
        // N, Z from the (masked) result; C and V from the operands
        let w = if sf { 64 } else { 32 };
        let top = w - 1;
        // N
        self.lget(L_R); self.i64c(top as i64); self.op(0x88); self.i64c(1); self.op(0x83); self.i64c(31); self.op(0x86);
        // Z
        self.lget(L_R); self.op(0x50); self.op(0xad); self.i64c(30); self.op(0x86); self.op(0x84);
        // C
        if sub { self.lget(L_A); self.lget(L_B); self.op(0x5a); } // a >=u b
        else { self.lget(L_R); self.lget(L_A); self.op(0x54); }   // r <u a (masked width)
        self.op(0xad); self.i64c(29); self.op(0x86); self.op(0x84);
        // V
        if sub { self.lget(L_A); self.lget(L_B); self.op(0x85); self.lget(L_A); self.lget(L_R); self.op(0x85); self.op(0x83); }
        else { self.lget(L_A); self.lget(L_R); self.op(0x85); self.lget(L_B); self.lget(L_R); self.op(0x85); self.op(0x83); }
        self.i64c(top as i64); self.op(0x88); self.i64c(1); self.op(0x83); self.i64c(28); self.op(0x86); self.op(0x84);
        self.op(0xa7); // wrap to i32
        self.store32(self.off_nzcv);
    }
    /// NZ from L_R, C = V = 0 (logical with S)
    fn flags_nz(&mut self, sf: bool) {
        let top = if sf { 63 } else { 31 };
        self.lget(L_R); self.i64c(top); self.op(0x88); self.i64c(1); self.op(0x83); self.i64c(31); self.op(0x86);
        self.lget(L_R); self.op(0x50); self.op(0xad); self.i64c(30); self.op(0x86); self.op(0x84);
        self.op(0xa7);
        self.store32(self.off_nzcv);
    }
    /// push the condition `cond` (i32 0/1) from the stored NZCV
    fn cond(&mut self, cond: u32) {
        let f = |w: &mut W, sh: i32| { w.load32(w.off_nzcv); w.i32c(sh); w.op(0x76); w.i32c(1); w.op(0x71); };
        match cond >> 1 {
            0 => f(self, 30),
            1 => f(self, 29),
            2 => f(self, 31),
            3 => f(self, 28),
            4 => { f(self, 29); f(self, 30); self.op(0x45); self.op(0x71); }
            5 => { f(self, 31); f(self, 28); self.op(0x46); }
            6 => { f(self, 31); f(self, 28); self.op(0x46); f(self, 30); self.op(0x45); self.op(0x71); }
            _ => self.i32c(1),
        }
        if cond & 1 == 1 && cond != 15 { self.op(0x45); }
    }
    /// shift the value on the stack: type, amount, width
    fn shift(&mut self, ty: u32, amt: u32, sf: bool) {
        if amt == 0 { return; }
        if !sf {
            match ty {
                0 => { self.i64c(amt as i64); self.op(0x86); }
                1 => { self.i64c(amt as i64); self.op(0x88); }
                2 => { self.op(0xa7); self.i32c(amt as i32); self.op(0x75); self.op(0xad); }
                _ => { self.op(0xa7); self.i32c(amt as i32); self.op(0x78); self.op(0xad); }
            }
        } else {
            self.i64c(amt as i64);
            self.op(match ty { 0 => 0x86, 1 => 0x88, 2 => 0x87, _ => 0x8a });
        }
    }
}

impl Cpu {
    #[inline(never)]
    pub(crate) fn compile_wasm(&mut self, pa: u64) -> Option<crate::jit::BlockFn> {
        if self.ram.is_null() { return None; }
        let sp_slot = std::mem::offset_of!(Cpu, sp) + 8 * if self.spsel { self.el as usize } else { 0 };
        let mut w = W {
            c: Vec::with_capacity(1024),
            off_x: std::mem::offset_of!(Cpu, x) as u32,
            off_sp_slot: sp_slot as u32,
            off_pc: std::mem::offset_of!(Cpu, pc) as u32,
            off_nzcv: std::mem::offset_of!(Cpu, nzcv) as u32,
            off_instret: std::mem::offset_of!(Cpu, instret) as u32,
            pending: 0,
            step_idx: h_step as *const () as usize as u32,
            off_utlb: std::mem::offset_of!(Cpu, utlb) as u32,
            off_ram: std::mem::offset_of!(Cpu, ram) as u32,
            off_excl: std::mem::offset_of!(Cpu, excl_armed) as u32,
            el: self.el,
            off_sp0: std::mem::offset_of!(Cpu, sp) as u32,
            off_tpidr: (std::mem::offset_of!(Cpu, sys) + std::mem::offset_of!(crate::sysreg::SysRegs, tpidr)) as u32,
            region: Vec::new(),
            cur_bb: 0,
            off_chain_until: std::mem::offset_of!(Cpu, chain_until) as u32,
        };
        // ---- the region: basic blocks reachable through constant branches on this page
        let page = self.pc & !0xfff;
        let pa_page = pa & !0xfff;
        let word = |cpu: &Cpu, pc: u64| -> Option<u32> {
            let a = pa_page | (pc & 0xfff);
            // SAFETY: inside guest RAM (checked)
            (a + 4 <= cpu.ram_len).then(|| unsafe { (cpu.ram.add(a as usize) as *const u32).read_unaligned() })
        };
        let mut bbs: Vec<u64> = vec![self.pc];
        let mut q = 0;
        while q < bbs.len() {
            let mut pc = bbs[q];
            q += 1;
            for _ in 0..64 {
                let Some(i) = word(self, pc) else { break };
                if let Some(ts) = const_targets(i, pc) {
                    for t in ts { if t & !0xfff == page && bbs.len() < MAX_BBS && !bbs.contains(&t) { bbs.push(t); } }
                    break;
                }
                if super::jit::is_block_end(i) { break; }
                pc += 4;
                if pc & 0xfff == 0 { break; }
            }
        }
        let n = bbs.len();
        w.region = bbs.clone();
        w.op(0x03); w.op(0x40); // loop: the dispatch
        for _ in 0..=n { w.op(0x02); w.op(0x40); } // $exit, then one block per basic block
        w.lget(L_IDX); w.op(0x0e); uleb(&mut w.c, n as u64);
        for j in 0..n { uleb(&mut w.c, j as u64); }
        uleb(&mut w.c, n as u64); // default: $exit
        for (k, &start) in bbs.iter().enumerate() {
        w.op(0x0b); // end of the basic block's label: its code follows
        w.cur_bb = k;
        let mut pc = start;
        let mut a = pa_page | (start & 0xfff);
        let mut ended = false;
        for _ in 0..64 {
            if a + 4 > self.ram_len { break; }
            // SAFETY: inside guest RAM (checked)
            let i = unsafe { (self.ram.add(a as usize) as *const u32).read_unaligned() };
            if self.wasm_native(&mut w, i, pc) {
                w.pending += 1;
            } else if wasm_ldst(&mut w, i, pc, self.jit.as_ref().is_some_and(|j| j.verify)) {
                // load/store with an inline micro-TLB fast path
            } else if let Some(end) = self.wasm_branch(&mut w, i, pc) {
                if end { ended = true; break; }
            } else {
                // interpret this instruction
                w.flush_instret();
                w.set_pc(pc);
                w.lget(L_CPU);
                w.i64c((pc + 4) as i64);
                w.i32c(w.step_idx as i32);
                w.op(0x11); uleb(&mut w.c, 1); w.op(0x00); // call_indirect (type 1) table 0
                w.lset(L_ST);
                if super::jit::is_block_end(i) {
                    // 2 -> trap (1), otherwise leave (0)
                    w.lget(L_ST); w.i32c(2); w.op(0x46); w.op(0x0f); // return (st == 2)
                    ended = true;
                    break;
                }
                w.lget(L_ST); w.op(0x04); w.op(0x40); // if st != 0
                w.lget(L_ST); w.i32c(2); w.op(0x46); w.op(0x0f); // return (st == 2)
                w.op(0x0b);
            }
            pc += 4;
            a += 4;
            if a & 0xfff == 0 { break; }
        }
        if !ended {
            w.flush_instret();
            w.exit_const(pc, 0);
        }
        }
        w.op(0x0b); // end of $exit (not reached: every basic block ends in a branch or return)
        w.ret(0);
        w.op(0x0b); // end of the loop
        w.op(0x00); // unreachable
        w.op(0x0b); // end of function
        let module = assemble(&w.c);
        // SAFETY: the import writes nothing; it reads `len` bytes of the module
        let idx = unsafe { sbc_jit_compile(module.as_ptr(), module.len()) };
        if idx < 0 { return None; }
        // SAFETY: a wasm function pointer is its table index; the page installed a function of
        // type (i32) -> i32 there, which is the BlockFn ABI
        Some(unsafe { std::mem::transmute::<usize, crate::jit::BlockFn>(idx as usize) })
    }

    /// Integer data processing in wasm; false = not handled here.
    fn wasm_native(&self, w: &mut W, i: u32, pc: u64) -> bool {
        let sf = bit(i, 31);
        let (rd, rn, rm) = (i & 31, bits(i, 9, 5), bits(i, 20, 16));
        // NOP and the hints that are NOPs here (PAC, BTI, YIELD, ...), DSB/DMB/ISB, PRFM
        if (i & 0xffff_f01f == 0xd503_201f && !matches!((i >> 5) & 0x7f, 2 | 3))
            || (i & 0xffff_f01f == 0xd503_301f && matches!((i >> 5) & 7, 4..=6))
            || i & 0xffc0_0000 == 0xf980_0000 || i & 0xffe0_0c00 == 0xf880_0000 || i & 0xffe0_0c00 == 0xf8a0_0800
        {
            return true;
        }
        // MRS SP_EL0 (EL1+), TPIDR_EL0, TPIDRRO_EL0, TPIDR_EL1 (EL1+)
        let mrs = match i & 0xffff_ffe0 {
            0xd538_4100 if self.el >= 1 => Some(w.off_sp0),
            0xd53b_d040 => Some(w.off_tpidr),
            0xd53b_d060 => Some(w.off_tpidr + 8),
            0xd538_d080 if self.el >= 1 => Some(w.off_tpidr + 16),
            _ => None,
        };
        if let Some(off) = mrs {
            if rd != 31 { w.load64(off); w.set_x(rd, false, true); }
            return true;
        }
        // ADD/SUB (extended register)
        if bits(i, 28, 24) == 0b01011 && bit(i, 21) && bits(i, 23, 22) == 0 {
            let (opt, sh) = (bits(i, 15, 13), bits(i, 12, 10));
            if sh > 4 { return false; }
            let (sub, s) = (bit(i, 30), bit(i, 29));
            w.get_x(rn, true); w.mask32(sf); w.lset(L_A);
            w.get_x(rm, false);
            match opt { 0 => { w.i64c(0xff); w.op(0x83); } 1 => { w.i64c(0xffff); w.op(0x83); } 2 => { w.i64c(0xffff_ffff); w.op(0x83); }
                        4 => w.op(0xc2), 5 => w.op(0xc3), 6 => w.op(0xc4), _ => {} }
            if sh != 0 { w.i64c(sh as i64); w.op(0x86); }
            w.mask32(sf); w.lset(L_B);
            w.lget(L_A); w.lget(L_B); w.op(if sub { 0x7d } else { 0x7c }); w.mask32(sf); w.lset(L_R);
            if s { w.flags_addsub(sf, sub); }
            w.lget(L_R); w.set_x(rd, !s, sf);
            return true;
        }
        // CCMP / CCMN (register, immediate)
        if bits(i, 29, 21) == 0b1_1101_0010 && !bit(i, 10) && !bit(i, 4) {
            let sub = bit(i, 30);
            w.cond(bits(i, 15, 12));
            w.op(0x04); w.op(0x40); // if cond: flags of the comparison
            w.get_x(rn, false); w.mask32(sf); w.lset(L_A);
            if bit(i, 11) { w.i64c(rm as i64); } else { w.get_x(rm, false); w.mask32(sf); }
            w.lset(L_B);
            w.lget(L_A); w.lget(L_B); w.op(if sub { 0x7d } else { 0x7c }); w.mask32(sf); w.lset(L_R);
            w.flags_addsub(sf, sub);
            w.op(0x05); // else: the immediate flags
            w.i32c((bits(i, 3, 0) << 28) as i32); w.store32(w.off_nzcv);
            w.op(0x0b);
            return true;
        }
        // data processing (2 source): LSLV / LSRV / ASRV / RORV
        if bits(i, 30, 21) == 0b00_1101_0110 && matches!(bits(i, 15, 10), 0b001000..=0b001011) {
            let op = bits(i, 11, 10);
            if sf {
                w.get_x(rn, false); w.get_x(rm, false);
                w.op([0x86, 0x88, 0x87, 0x8a][op as usize]);
            } else {
                w.get_x(rn, false); w.op(0xa7); w.get_x(rm, false); w.op(0xa7);
                w.op([0x74, 0x76, 0x75, 0x78][op as usize]); w.op(0xad);
            }
            w.set_x(rd, false, sf);
            return true;
        }
        // data processing (1 source): RBIT, REV16, REV32/REV, REV, CLZ
        if bits(i, 30, 21) == 0b10_1101_0110 && bits(i, 20, 16) == 0 && bits(i, 15, 10) <= 4 {
            let op = bits(i, 15, 10);
            if op == 3 && !sf { return false; }
            w.get_x(rn, false); w.mask32(sf); w.lset(L_A);
            // swap adjacent fields of `sh` bits: x = (x >> sh & m) | (x & m) << sh
            let swap = |w: &mut W, sh: i64, m: u64| {
                w.lget(L_A); w.i64c(sh); w.op(0x88); w.i64c(m as i64); w.op(0x83);
                w.lget(L_A); w.i64c(m as i64); w.op(0x83); w.i64c(sh); w.op(0x86);
                w.op(0x84); w.lset(L_A);
            };
            let bytes = |w: &mut W, sf: bool, upto: u32| {
                // byte swap within units of 2^upto bytes (1 = halfwords, 2 = words, 3 = doublewords)
                if upto >= 1 { swap(w, 8, 0x00ff_00ff_00ff_00ff); }
                if upto >= 2 { swap(w, 16, 0x0000_ffff_0000_ffff); }
                if upto >= 3 && sf { swap(w, 32, 0x0000_0000_ffff_ffff); }
            };
            match op {
                0 => {
                    swap(w, 1, 0x5555_5555_5555_5555); swap(w, 2, 0x3333_3333_3333_3333); swap(w, 4, 0x0f0f_0f0f_0f0f_0f0f);
                    bytes(w, sf, 3);
                    w.lget(L_A);
                }
                1 => { bytes(w, sf, 1); w.lget(L_A); }
                2 => { bytes(w, sf, 2); w.lget(L_A); }
                3 => { bytes(w, sf, 3); w.lget(L_A); }
                _ => { if sf { w.lget(L_A); w.op(0x79); } else { w.lget(L_A); w.op(0xa7); w.op(0x67); w.op(0xad); } }
            }
            w.set_x(rd, false, sf);
            return true;
        }
        // ADC / ADCS / SBC / SBCS
        if bits(i, 28, 21) == 0b1101_0000 && bits(i, 15, 10) == 0 {
            let (sbc, s) = (bit(i, 30), bit(i, 29));
            let m = if sf { u64::MAX } else { 0xffff_ffff };
            w.get_x(rn, false); w.mask32(sf); w.lset(L_A);
            w.get_x(rm, false); if sbc { w.i64c(m as i64); w.op(0x85); } w.mask32(sf); w.lset(L_B); // b' = ~b for SBC
            w.load32(w.off_nzcv); w.i32c(29); w.op(0x76); w.i32c(1); w.op(0x71); w.op(0xad); w.lset(L_C); // carry in
            // t = a + b'; r = t + c (masked); carry out = t < a || r < t (64) / bit 32 (32)
            w.lget(L_A); w.lget(L_B); w.op(0x7c); w.lset(L_D);
            w.lget(L_D); w.lget(L_C); w.op(0x7c); w.lset(L_E);
            w.lget(L_E); w.mask32(sf); w.lset(L_R);
            if s {
                // N, Z
                let top = if sf { 63 } else { 31 };
                w.lget(L_R); w.i64c(top); w.op(0x88); w.i64c(1); w.op(0x83); w.i64c(31); w.op(0x86);
                w.lget(L_R); w.op(0x50); w.op(0xad); w.i64c(30); w.op(0x86); w.op(0x84);
                // C
                if sf { w.lget(L_D); w.lget(L_A); w.op(0x54); w.lget(L_E); w.lget(L_D); w.op(0x54); w.op(0x72); w.op(0xad); }
                else { w.lget(L_E); w.i64c(32); w.op(0x88); w.i64c(1); w.op(0x83); }
                w.i64c(29); w.op(0x86); w.op(0x84);
                // V = ((a ^ r) & (b' ^ r)) top bit
                w.lget(L_A); w.lget(L_R); w.op(0x85); w.lget(L_B); w.lget(L_R); w.op(0x85); w.op(0x83);
                w.i64c(top); w.op(0x88); w.i64c(1); w.op(0x83); w.i64c(28); w.op(0x86); w.op(0x84);
                w.op(0xa7); w.store32(w.off_nzcv);
            }
            w.lget(L_R); w.set_x(rd, false, sf);
            return true;
        }
        // UMULH / SMULH: 64x64 -> high 64 from 32-bit partial products
        if bits(i, 31, 24) == 0b1001_1011 && bits(i, 22, 21) == 0b10 && bits(i, 15, 10) == 0b011111 {
            let signed = !bit(i, 23);
            w.get_x(rn, false); w.lset(L_A);
            w.get_x(rm, false); w.lset(L_B);
            let lo = |w: &mut W, l: u32| { w.lget(l); w.i64c(0xffff_ffff); w.op(0x83); };
            let hi = |w: &mut W, l: u32| { w.lget(l); w.i64c(32); w.op(0x88); };
            // L_C = a_lo*b_hi, L_D = a_hi*b_lo, L_E = (a_lo*b_lo >> 32) + lo(C) + lo(D)
            lo(w, L_A); hi(w, L_B); w.op(0x7e); w.lset(L_C);
            hi(w, L_A); lo(w, L_B); w.op(0x7e); w.lset(L_D);
            lo(w, L_A); lo(w, L_B); w.op(0x7e); w.i64c(32); w.op(0x88);
            lo(w, L_C); w.op(0x7c); lo(w, L_D); w.op(0x7c); w.lset(L_E);
            // high = a_hi*b_hi + hi(C) + hi(D) + hi(E)
            hi(w, L_A); hi(w, L_B); w.op(0x7e);
            hi(w, L_C); w.op(0x7c); hi(w, L_D); w.op(0x7c); hi(w, L_E); w.op(0x7c);
            if signed {
                // signed high = unsigned high - (a < 0 ? b : 0) - (b < 0 ? a : 0)
                w.lget(L_B); w.lget(L_A); w.i64c(63); w.op(0x87); w.op(0x83); w.op(0x7d);
                w.lget(L_A); w.lget(L_B); w.i64c(63); w.op(0x87); w.op(0x83); w.op(0x7d);
            }
            w.set_x(rd, false, true);
            return true;
        }
        match bits(i, 28, 23) {
            0b100010 => {
                // ADD/SUB (immediate)
                let (sub, s) = (bit(i, 30), bit(i, 29));
                let imm = (bits(i, 21, 10) as i64) << if bit(i, 22) { 12 } else { 0 };
                w.get_x(rn, true); w.mask32(sf); w.lset(L_A);
                w.i64c(imm); w.lset(L_B);
                w.lget(L_A); w.lget(L_B); w.op(if sub { 0x7d } else { 0x7c }); w.mask32(sf); w.lset(L_R);
                if s { w.flags_addsub(sf, sub); }
                w.lget(L_R); w.set_x(rd, !s, sf);
                return true;
            }
            0b100100 => {
                // logical (immediate)
                let Some(imm) = crate::exec::decode_bitmask(sf, bits(i, 22, 22), bits(i, 15, 10), bits(i, 21, 16)) else { return false };
                let opc = bits(i, 30, 29);
                w.get_x(rn, false); w.i64c(imm as i64);
                w.op(match opc { 0 | 3 => 0x83, 1 => 0x84, _ => 0x85 }); w.mask32(sf); w.lset(L_R);
                if opc == 3 { w.flags_nz(sf); }
                w.lget(L_R); w.set_x(rd, opc != 3, sf);
                return true;
            }
            0b100101 => {
                // move wide
                let hw = bits(i, 22, 21);
                if bits(i, 30, 29) == 1 || (!sf && hw > 1) { return false; }
                let imm = (bits(i, 20, 5) as u64) << (hw * 16);
                match bits(i, 30, 29) {
                    0 => w.i64c(!imm as i64),
                    2 => w.i64c(imm as i64),
                    _ => { w.get_x(rd, false); w.i64c(!(0xffffu64 << (hw * 16)) as i64); w.op(0x83); w.i64c(imm as i64); w.op(0x84); }
                }
                w.set_x(rd, false, sf);
                return true;
            }
            0b100110 => {
                // UBFM / SBFM (BFM via the interpreter)
                let (immr, imms) = (bits(i, 21, 16), bits(i, 15, 10));
                let wd = if sf { 64 } else { 32 };
                let opc = bits(i, 30, 29);
                if bit(i, 22) != sf || immr >= wd || imms >= wd || opc == 1 || opc == 3 { return false; }
                w.get_x(rn, false); w.mask32(sf);
                if opc == 2 {
                    if imms >= immr {
                        let width = imms - immr + 1;
                        w.i64c(immr as i64); w.op(0x88);
                        if width < 64 { w.i64c(((1u64 << width) - 1) as i64); w.op(0x83); }
                    } else {
                        let width = imms + 1;
                        if width < 64 { w.i64c(((1u64 << width) - 1) as i64); w.op(0x83); }
                        w.i64c((wd - immr) as i64); w.op(0x86);
                    }
                } else if imms >= immr {
                    // signed extract of [imms:immr]
                    w.i64c((63 - imms) as i64); w.op(0x86); w.i64c((63 - imms + immr) as i64); w.op(0x87);
                } else {
                    // sign-extend [imms:0] then shift left to (wd - immr)
                    w.i64c((63 - imms) as i64); w.op(0x86); w.i64c((63 - imms) as i64); w.op(0x87);
                    w.i64c((wd - immr) as i64); w.op(0x86);
                }
                w.set_x(rd, false, sf);
                return true;
            }
            _ => {}
        }
        // ADR / ADRP
        if bits(i, 28, 24) == 0b10000 {
            let imm = (((((bits(i, 23, 5) << 2) | bits(i, 30, 29)) as u64) << 43) as i64 >> 43) as u64;
            let v = if bit(i, 31) { (pc & !0xfff).wrapping_add(imm << 12) } else { pc.wrapping_add(imm) };
            w.i64c(v as i64); w.set_x(rd, false, true);
            return true;
        }
        // logical (shifted register)
        if bits(i, 28, 24) == 0b01010 {
            let amt = bits(i, 15, 10);
            if !sf && amt >= 32 { return false; }
            let opc = bits(i, 30, 29);
            w.get_x(rn, false);
            w.get_x(rm, false); w.mask32(sf); w.shift(bits(i, 23, 22), amt, sf);
            if bit(i, 21) { w.i64c(-1); w.op(0x85); }
            w.op(match opc { 0 | 3 => 0x83, 1 => 0x84, _ => 0x85 }); w.mask32(sf); w.lset(L_R);
            if opc == 3 { w.flags_nz(sf); }
            w.lget(L_R); w.set_x(rd, false, sf);
            return true;
        }
        // add/sub (shifted register)
        if bits(i, 28, 24) == 0b01011 && !bit(i, 21) {
            let amt = bits(i, 15, 10);
            let ty = bits(i, 23, 22);
            if ty == 3 || (!sf && amt >= 32) { return false; }
            let (sub, s) = (bit(i, 30), bit(i, 29));
            w.get_x(rn, false); w.mask32(sf); w.lset(L_A);
            w.get_x(rm, false); w.mask32(sf); w.shift(ty, amt, sf); w.mask32(sf); w.lset(L_B);
            w.lget(L_A); w.lget(L_B); w.op(if sub { 0x7d } else { 0x7c }); w.mask32(sf); w.lset(L_R);
            if s { w.flags_addsub(sf, sub); }
            w.lget(L_R); w.set_x(rd, false, sf);
            return true;
        }
        // CSEL family
        if bits(i, 28, 21) == 0b11010100 && !bit(i, 29) && !bit(i, 11) {
            w.get_x(rn, false);
            w.get_x(rm, false);
            match (bit(i, 30), bit(i, 10)) {
                (false, false) => {}
                (false, true) => { w.i64c(1); w.op(0x7c); }
                (true, false) => { w.i64c(-1); w.op(0x85); }
                (true, true) => { w.lset(L_A); w.i64c(0); w.lget(L_A); w.op(0x7d); }
            }
            w.cond(bits(i, 15, 12));
            w.op(0x1b); // select
            w.set_x(rd, false, sf);
            return true;
        }
        // MADD / MSUB
        if bits(i, 28, 21) == 0b11011000 && bits(i, 30, 29) == 0 {
            let ra = bits(i, 14, 10);
            w.get_x(ra, false);
            w.get_x(rn, false); w.get_x(rm, false); w.op(0x7e);
            w.op(if bit(i, 15) { 0x7d } else { 0x7c });
            w.set_x(rd, false, sf);
            return true;
        }
        if i == 0xd503_201f { return true; } // NOP
        false
    }

    /// Branches end the block. None = not a branch handled here.
    fn wasm_branch(&self, w: &mut W, i: u32, pc: u64) -> Option<bool> {
        let sx = |v: u64, b: u32| ((v << (64 - b)) as i64 >> (64 - b)) as u64;
        let finish = |w: &mut W| { w.pending += 1; w.flush_instret(); w.ret(0); };
        if bits(i, 30, 26) == 0b00101 {
            let t = pc.wrapping_add(sx((bits(i, 25, 0) as u64) << 2, 28));
            if bit(i, 31) { w.i64c((pc + 4) as i64); w.set_x(30, false, true); }
            w.pending += 1;
            w.flush_instret();
            w.exit_const(t, 0);
            return Some(true);
        }
        if bits(i, 31, 24) == 0b0101_0100 && !bit(i, 4) {
            let t = pc.wrapping_add(sx((bits(i, 23, 5) as u64) << 2, 21));
            w.pending += 1;
            w.flush_instret();
            w.cond(bits(i, 3, 0));
            w.op(0x04); w.op(0x40); // if taken
            w.exit_const(t, 1);
            w.op(0x0b);
            w.exit_const(pc + 4, 0);
            return Some(true);
        }
        let cb = bits(i, 30, 25) == 0b011010;
        let tb = bits(i, 30, 25) == 0b011011;
        if cb || tb {
            let rt = i & 31;
            let t = if cb { pc.wrapping_add(sx((bits(i, 23, 5) as u64) << 2, 21)) } else { pc.wrapping_add(sx((bits(i, 18, 5) as u64) << 2, 16)) };
            w.pending += 1;
            w.flush_instret();
            w.get_x(rt, false);
            if cb {
                w.mask32(bit(i, 31));
                w.op(0x50); // eqz
                if bit(i, 24) { w.op(0x45); } // CBNZ
            } else {
                let b = (bits(i, 31, 31) << 5) | bits(i, 23, 19);
                w.i64c(b as i64); w.op(0x88); w.i64c(1); w.op(0x83); w.op(0xa7);
                if !bit(i, 24) { w.op(0x45); } // TBZ: taken when the bit is 0
            }
            w.op(0x04); w.op(0x40); // if taken
            w.exit_const(t, 1);
            w.op(0x0b);
            w.exit_const(pc + 4, 0);
            return Some(true);
        }
        if bits(i, 31, 25) == 0b110_1011 && bits(i, 20, 16) == 0b11111 && bits(i, 15, 10) == 0 && bits(i, 4, 0) == 0 {
            let opc = bits(i, 24, 21);
            if opc > 2 { return None; }
            w.get_x(bits(i, 9, 5), false); w.lset(L_A);
            if opc == 1 { w.i64c((pc + 4) as i64); w.set_x(30, false, true); }
            w.lget(L_A); w.store64(w.off_pc);
            finish(w);
            return Some(true);
        }
        None
    }
}

/// Basic blocks per compiled region.
const MAX_BBS: usize = 24;

/// A branch ends a basic block: its constant successors (empty for register branches).
fn const_targets(i: u32, pc: u64) -> Option<Vec<u64>> {
    let sx = |v: u64, b: u32| ((v << (64 - b)) as i64 >> (64 - b)) as u64;
    if bits(i, 30, 26) == 0b00101 { return Some(vec![pc.wrapping_add(sx((bits(i, 25, 0) as u64) << 2, 28))]); }
    if bits(i, 31, 24) == 0b0101_0100 && !bit(i, 4) { return Some(vec![pc.wrapping_add(sx((bits(i, 23, 5) as u64) << 2, 21)), pc + 4]); }
    if bits(i, 30, 25) == 0b011010 { return Some(vec![pc.wrapping_add(sx((bits(i, 23, 5) as u64) << 2, 21)), pc + 4]); }
    if bits(i, 30, 25) == 0b011011 { return Some(vec![pc.wrapping_add(sx((bits(i, 18, 5) as u64) << 2, 16)), pc + 4]); }
    if bits(i, 31, 25) == 0b110_1011 { return Some(Vec::new()); }
    None
}

/// The interpreter call for the instruction at `pc` inside a block (continue on status 0).
fn emit_interp(w: &mut W, pc: u64) {
    w.flush_instret();
    w.set_pc(pc);
    w.lget(L_CPU);
    w.i64c((pc + 4) as i64);
    w.i32c(w.step_idx as i32);
    w.op(0x11); uleb(&mut w.c, 1); w.op(0x00);
    w.lset(L_ST);
    w.lget(L_ST); w.op(0x04); w.op(0x40);
    w.lget(L_ST); w.i32c(2); w.op(0x46); w.op(0x0f);
    w.op(0x0b);
}

/// Integer LDR/STR (unsigned offset, unscaled, pre/post-index, register offset) and
/// LDP/STP/LDPSW: inline micro-TLB probe and direct RAM access, the interpreter otherwise.
fn wasm_ldst(w: &mut W, i: u32, pc: u64, verify: bool) -> bool {
    let rt = i & 31;
    let rn = bits(i, 9, 5);
    // (size log2, load, sign: 0 none / 32 / 64, pair, rt2, offset or register, mode 0 off / 1 post / 2 pre)
    let (sz, load, sign, pair, rt2, imm, regoff, mode): (u32, bool, u32, bool, u32, i64, Option<(u32, u32, u32)>, u8);
    if bits(i, 29, 27) == 0b111 && !bit(i, 26) && !bit(i, 25) {
        let s = bits(i, 31, 30);
        let (ld, sg) = match (s, bits(i, 23, 22)) { (_, 0) => (false, 0), (_, 1) => (true, 0), (3, _) => return false, (2, 3) => return false, (_, 2) => (true, 64), _ => (true, 32) };
        sz = s; load = ld; sign = sg; pair = false; rt2 = 0;
        if bit(i, 24) { imm = (bits(i, 21, 10) as i64) << s; regoff = None; mode = 0; }
        else if bit(i, 21) {
            if bits(i, 11, 10) != 0b10 || bits(i, 15, 13) & 2 == 0 { return false; }
            imm = 0; regoff = Some((bits(i, 20, 16), bits(i, 15, 13), if bit(i, 12) { s } else { 0 })); mode = 0;
        } else {
            imm = ((bits(i, 20, 12) as i64) << 55) >> 55; regoff = None;
            mode = match bits(i, 11, 10) { 0 => 0, 1 => 1, 3 => 2, _ => return false };
        }
    } else if bits(i, 29, 27) == 0b101 && !bit(i, 26) && !bit(i, 25) && bits(i, 24, 23) != 0 {
        let ld = bit(i, 22);
        let (s, sg) = match (bits(i, 31, 30), ld) { (0, _) => (2, 0), (1, true) => (2, 64), (2, _) => (3, 0), _ => return false };
        sz = s; load = ld; sign = sg; pair = true; rt2 = bits(i, 14, 10);
        imm = (((bits(i, 21, 15) as i64) << 57) >> 57) << s; regoff = None;
        mode = match bits(i, 24, 23) { 1 => 1, 3 => 2, _ => 0 };
        if load && rt == rt2 { return false; }
    } else { return false; }
    if mode != 0 && rn != 31 && (rn == rt || (pair && rn == rt2)) { return false; }
    if verify && !load { return false; } // verify mode logs every store in the interpreter
    let size = 1u64 << sz;
    let span = if pair { 2 * size } else { size };
    w.flush_instret();
    // L_A = base, L_B = address of the access
    w.get_x(rn, true); w.lset(L_A);
    w.lget(L_A);
    if let Some((rm, opt, sh)) = regoff {
        w.get_x(rm, false);
        match opt { 2 => { w.i64c(0xffff_ffff); w.op(0x83); } 6 => { w.op(0xa7); w.op(0xac); } _ => {} } // UXTW / SXTW / LSL,SXTX
        if sh != 0 { w.i64c(sh as i64); w.op(0x86); }
        w.op(0x7c);
    } else if mode != 1 && imm != 0 { w.i64c(imm); w.op(0x7c); }
    w.lset(L_B);
    // tag check and page-crossing check -> i32 condition
    let k = if load { 1 } else { 2 };
    // L_ST = &utlb[k][uti(va)] - cpu offset (an i32 address)
    let ent = w.off_utlb + (16 * UT * k) as u32;
    w.lget(L_CPU); w.lget(L_B); w.lget(L_B); w.i64c(8); w.op(0x88); w.op(0x85); w.i64c(12); w.op(0x88); w.op(0xa7); w.i32c(UT as i32 / 2 - 1); w.op(0x71); w.i32c(5); w.op(0x74); w.op(0x6a); w.lset(L_ST); // way 0 of the set
    w.lget(L_B); w.i64c(12); w.op(0x88); w.i64c(1); w.op(0x7c); w.i64c(((w.el as u64) << 60) as i64); w.op(0x84);
    w.lget(L_ST); w.op(0x29); uleb(&mut w.c, 3); uleb(&mut w.c, ent as u64); w.op(0x51); // eq
    w.lget(L_B); w.i64c(0xfff); w.op(0x83); w.i64c(span as i64 - 1); w.op(0x7c); w.i64c(12); w.op(0x88); w.op(0x50); // no crossing
    w.op(0x71);
    if !load { w.load64(w.off_excl); w.op(0x50); w.op(0x71); }
    w.op(0x04); w.op(0x40); // if fast
    // host address (i32) in L_ST: ram + page + (va & 0xfff)
    w.load32(w.off_ram);
    w.lget(L_ST); w.op(0x29); uleb(&mut w.c, 3); uleb(&mut w.c, ent as u64 + 8); w.lget(L_B); w.i64c(0xfff); w.op(0x83); w.op(0x84); w.op(0xa7);
    w.op(0x6a); w.lset(L_ST);
    let ld_op = |sz: u32, sign: u32| -> u8 { match (sz, sign != 0) { (0, false) => 0x31, (0, true) => 0x30, (1, false) => 0x33, (1, true) => 0x32, (2, false) => 0x35, (2, true) => 0x34, _ => 0x29 } };
    let st_op = |sz: u32| -> u8 { match sz { 0 => 0x3c, 1 => 0x3d, 2 => 0x3e, _ => 0x37 } };
    let regs: Vec<(u32, u64)> = if pair { vec![(rt, 0), (rt2, size)] } else { vec![(rt, 0)] };
    for &(r, off) in &regs {
        if load {
            w.lget(L_ST); w.op(ld_op(sz, sign)); uleb(&mut w.c, 0); uleb(&mut w.c, off);
            if sign == 32 { w.i64c(0xffff_ffff); w.op(0x83); }
            if r != 31 { w.store64(w.off_x + 8 * r); } else { w.op(0x1a); }
        } else {
            w.lget(L_ST); w.get_x(r, false); w.op(st_op(sz)); uleb(&mut w.c, 0); uleb(&mut w.c, off);
        }
    }
    // base writeback
    match mode {
        1 => { w.lget(L_A); w.i64c(imm); w.op(0x7c); w.set_x(rn, true, true); }
        2 => { w.lget(L_B); w.set_x(rn, true, true); }
        _ => {}
    }
    w.load64(w.off_instret); w.i64c(1); w.op(0x7c); w.store64(w.off_instret);
    w.op(0x05); // else: the interpreter
    emit_interp(w, pc);
    w.op(0x0b);
    true
}

/// Wrap a function body into a module importing env.memory and env.table.
fn assemble(body: &[u8]) -> Vec<u8> {
    let mut m = b"\0asm\x01\0\0\0".to_vec();
    let section = |m: &mut Vec<u8>, id: u8, payload: &[u8]| { m.push(id); uleb(m, payload.len() as u64); m.extend_from_slice(payload); };
    // types: 0 = (i32) -> i32 (the block), 1 = (i32, i64) -> i32 (h_step)
    section(&mut m, 1, &[2, 0x60, 1, 0x7f, 1, 0x7f, 0x60, 2, 0x7f, 0x7e, 1, 0x7f]);
    // imports: env.memory (min 0), env.table (funcref, min 0)
    let mut imp = vec![2];
    for (name, desc) in [("memory", &[0x02u8, 0x00, 0x00][..]), ("table", &[0x01u8, 0x70, 0x00, 0x00][..])] {
        imp.push(3); imp.extend_from_slice(b"env");
        imp.push(name.len() as u8); imp.extend_from_slice(name.as_bytes());
        imp.extend_from_slice(desc);
    }
    section(&mut m, 2, &imp);
    section(&mut m, 3, &[1, 0]); // one function of type 0
    section(&mut m, 7, &[1, 1, b'f', 0x00, 0]); // export "f" = function 0
    // code: locals = 3 x i64, 1 x i32, 3 x i64 (L_C, L_D, L_E), 1 x i32 (L_IDX)
    let mut f = vec![4, 3, 0x7e, 1, 0x7f, 3, 0x7e, 1, 0x7f];
    f.extend_from_slice(body);
    let mut code = vec![1];
    uleb(&mut code, f.len() as u64);
    code.extend_from_slice(&f);
    section(&mut m, 10, &code);
    m
}
