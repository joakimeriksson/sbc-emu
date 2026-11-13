//! A64 → A64 block JIT (AArch64 macOS/Linux hosts).
//!
//! Guest and host share the instruction set, so integer data-processing instructions are
//! emitted *as themselves* with the guest registers renamed to host temporaries: the host CPU
//! computes exactly the architected result, flags included. Guest NZCV stays in the host flags
//! for the whole block. Branches end a block and compute the next PC natively. Every other
//! instruction (loads/stores, FP/SIMD, system) calls back into the interpreter for that single
//! instruction, so memory, MMU faults, exceptions and devices keep the interpreter's exact
//! semantics; system instructions, exception generation and ERET end the block.
//!
//! Blocks are keyed by physical address, exception level and SPSel and never cross a 4 KiB
//! page. They are discarded by `IC IALLU[IS]` / `IC IVAU` — the cache maintenance software must
//! perform after writing code, since CTR_EL0.DIC/IDC are 0 — and when the code buffer fills.
//! The interpreter stays the reference: `jit_enabled = false` gives identical results.
use crate::cpu::Cpu;
use emu_core::{Bus, Trap};
use std::collections::HashMap;
use std::hash::{BuildHasherDefault, Hasher};

/// A cheap multiplicative hasher for (address, key) pairs (the default SipHash dominated).
#[derive(Default)]
pub struct AddrHasher(u64);
impl Hasher for AddrHasher {
    fn finish(&self) -> u64 { self.0 }
    fn write(&mut self, bytes: &[u8]) { for &b in bytes { self.0 = (self.0.rotate_left(8) ^ b as u64).wrapping_mul(0x9e37_79b9_7f4a_7c15); } }
    fn write_u64(&mut self, v: u64) { self.0 = (self.0.rotate_left(29) ^ v).wrapping_mul(0x9e37_79b9_7f4a_7c15); }
}
type Map<K, V> = HashMap<K, V, BuildHasherDefault<AddrHasher>>;

#[cfg(all(target_arch = "aarch64", any(target_os = "macos", target_os = "linux")))]
mod host {
    use std::ffi::{c_int, c_void};
    extern "C" {
        fn mmap(addr: *mut c_void, len: usize, prot: c_int, flags: c_int, fd: c_int, off: i64) -> *mut c_void;
        #[cfg(target_os = "macos")] fn pthread_jit_write_protect_np(enabled: c_int);
        #[cfg(target_os = "macos")] fn sys_icache_invalidate(start: *mut c_void, len: usize);
        #[cfg(target_os = "linux")] fn __clear_cache(start: *mut c_void, end: *mut c_void);
    }

    /// Executable code buffer (bump allocated; reset as a whole).
    pub struct CodeCache { base: *mut u8, size: usize, used: usize }

    impl CodeCache {
        pub fn new(size: usize) -> Option<CodeCache> {
            #[cfg(target_os = "macos")] let flags = 0x0002 | 0x1000 | 0x0800; // PRIVATE | ANON | JIT
            #[cfg(target_os = "linux")] let flags = 0x0002 | 0x0020; // PRIVATE | ANONYMOUS
            // SAFETY: anonymous mapping chosen by the OS; failure is checked
            let p = unsafe { mmap(std::ptr::null_mut(), size, 7, flags, -1, 0) };
            if p as isize == -1 || p.is_null() { return None; }
            Some(CodeCache { base: p as *mut u8, size, used: 0 })
        }
        pub fn reset(&mut self) { self.used = 0; }
        /// Copy machine code in; returns its entry address.
        pub fn write(&mut self, words: &[u32]) -> Option<*const u8> {
            let bytes = words.len() * 4;
            if self.used + bytes > self.size { return None; }
            let off = self.used;
            // SAFETY: the range is inside the mapping; writes are toggled per thread on macOS and
            // the instruction cache is synchronised for exactly the written range
            unsafe {
                #[cfg(target_os = "macos")] pthread_jit_write_protect_np(0);
                std::ptr::copy_nonoverlapping(words.as_ptr() as *const u8, self.base.add(off), bytes);
                #[cfg(target_os = "macos")] pthread_jit_write_protect_np(1);
                #[cfg(target_os = "macos")] sys_icache_invalidate(self.base.add(off) as *mut c_void, bytes);
                #[cfg(target_os = "linux")] __clear_cache(self.base.add(off) as *mut c_void, self.base.add(off + bytes) as *mut c_void);
            }
            self.used += bytes;
            Some(self.base.wrapping_add(off))
        }
    }
    pub const AVAILABLE: bool = true;
}

#[cfg(not(all(target_arch = "aarch64", any(target_os = "macos", target_os = "linux"))))]
mod host {
    pub struct CodeCache;
    impl CodeCache {
        pub fn new(_size: usize) -> Option<CodeCache> { None }
        pub fn reset(&mut self) {}
        pub fn write(&mut self, _w: &[u32]) -> Option<*const u8> { None }
    }
    pub const AVAILABLE: bool = false;
}

/// Compiled block entry: `fn(cpu) -> status` (0 = continue at cpu.pc, 1 = trap in cpu.jit_trap).
type BlockFn = unsafe extern "C" fn(*mut Cpu) -> u32;

const HOT: u32 = 8;
const MAX_INSNS: usize = 64;
const CODE_BYTES: usize = 64 << 20;

pub struct Jit {
    code: host::CodeCache,
    /// (virtual pc, physical pc | el | spsel) -> code: blocks embed virtual PCs, so both matter
    blocks: Map<(u64, u64), BlockFn>,
    /// direct-mapped front cache over `blocks`
    front: Box<[(u64, u64, usize)]>,
    counts: Map<(u64, u64), u32>,
    /// physical page -> blocks compiled from it (for IC IVAU)
    pages: Map<u64, Vec<(u64, u64)>>,
    pub compiled: u64,
    pub flushes: u64,
    pub native: u64,
    /// executions before a block is compiled
    pub hot: u32,
}

const FRONT: usize = 1 << 14;

impl Jit {
    pub fn new() -> Option<Box<Jit>> { Self::with(CODE_BYTES, HOT) }

    pub fn with(code_bytes: usize, hot: u32) -> Option<Box<Jit>> {
        if !host::AVAILABLE { return None; }
        Some(Box::new(Jit { code: host::CodeCache::new(code_bytes)?, blocks: Map::default(), front: vec![(u64::MAX, u64::MAX, 0); FRONT].into_boxed_slice(),
                            counts: Map::default(), pages: Map::default(), compiled: 0, flushes: 0, native: 0, hot }))
    }

    pub fn flush(&mut self) {
        self.blocks.clear();
        self.counts.clear();
        self.pages.clear();
        self.front.fill((u64::MAX, u64::MAX, 0));
        self.code.reset();
        self.flushes += 1;
    }

    /// Drop blocks compiled from physical page `page` (4 KiB).
    pub fn invalidate_page(&mut self, page: u64) {
        let Some(keys) = self.pages.remove(&page) else { return };
        for k in keys {
            self.blocks.remove(&k);
            let slot = ((k.0 ^ (k.0 >> 14)) as usize >> 2) & (FRONT - 1);
            if self.front[slot].0 == k.0 && self.front[slot].1 == k.1 { self.front[slot] = (u64::MAX, u64::MAX, 0); }
        }
    }
}

fn key(pa: u64, el: u8, spsel: bool) -> u64 { pa | ((el as u64) << 56) | ((spsel as u64) << 58) }

// ---------------------------------------------------------------------- host encodings
const X_CPU: u32 = 19; // callee-saved: the Cpu pointer
const T: [u32; 4] = [9, 10, 11, 12]; // temporaries for guest registers
const X_SCRATCH: u32 = 13;
/// callee-saved: natives executed since the last flush to instret
const X_COUNT: u32 = 20;

fn ldr_x(rt: u32, rn: u32, off: usize) -> u32 { debug_assert!(off % 8 == 0 && off < 32768); 0xf940_0000 | (((off / 8) as u32) << 10) | (rn << 5) | rt }
fn str_x(rt: u32, rn: u32, off: usize) -> u32 { debug_assert!(off % 8 == 0 && off < 32768); 0xf900_0000 | (((off / 8) as u32) << 10) | (rn << 5) | rt }
fn ldr_w(rt: u32, rn: u32, off: usize) -> u32 { debug_assert!(off % 4 == 0 && off < 16384); 0xb940_0000 | (((off / 4) as u32) << 10) | (rn << 5) | rt }
fn str_w(rt: u32, rn: u32, off: usize) -> u32 { debug_assert!(off % 4 == 0 && off < 16384); 0xb900_0000 | (((off / 4) as u32) << 10) | (rn << 5) | rt }
fn mov64(out: &mut Vec<u32>, rd: u32, v: u64) {
    out.push(0xd280_0000 | (((v & 0xffff) as u32) << 5) | rd); // movz
    for k in 1..4 {
        let h = ((v >> (16 * k)) & 0xffff) as u32;
        if h != 0 { out.push(0xf280_0000 | (k << 21) | (h << 5) | rd); } // movk
    }
}
const MRS_NZCV_X9: u32 = 0xd53b_4209;
const MSR_NZCV_X9: u32 = 0xd51b_4209;

fn bits(x: u32, hi: u32, lo: u32) -> u32 { (x >> lo) & ((1u32 << (hi - lo + 1)) - 1) }
fn bit(x: u32, b: u32) -> bool { (x >> b) & 1 != 0 }

/// How a register field of a native instruction is read: register 31 means SP or ZR.
#[derive(Clone, Copy, PartialEq)]
enum R { Zr, Sp }

/// Register operands (field shift, meaning of 31) of an integer data-processing instruction
/// the JIT runs natively; None = not native (call the interpreter). Mirrors exec.rs exactly:
/// only encodings the interpreter accepts are native, so unallocated ones still fault there.
fn native_operands(i: u32) -> Option<Vec<(u32, R)>> {
    let sf = bit(i, 31);
    let (rd, rn, rm, ra) = (0u32, 5u32, 16u32, 10u32);
    match bits(i, 28, 23) {
        0b100010 => { let s = bit(i, 29); return Some(vec![(rd, if s { R::Zr } else { R::Sp }), (rn, R::Sp)]); } // add/sub imm
        0b100100 => {
            // logical immediate: valid bitmask only
            crate::exec::decode_bitmask(sf, bits(i, 22, 22), bits(i, 15, 10), bits(i, 21, 16))?;
            let s = bits(i, 30, 29) == 3;
            return Some(vec![(rd, if s { R::Zr } else { R::Sp }), (rn, R::Zr)]);
        }
        0b100101 => { if bits(i, 30, 29) == 1 || (!sf && bits(i, 22, 21) > 1) { return None; } return Some(vec![(rd, R::Zr)]); }
        0b100110 => {
            let w = if sf { 64 } else { 32 };
            if bit(i, 22) != sf || bits(i, 21, 16) >= w || bits(i, 15, 10) >= w || bits(i, 30, 29) == 3 { return None; }
            return Some(vec![(rd, R::Zr), (rn, R::Zr)]);
        }
        0b100111 => {
            if bits(i, 30, 29) != 0 || bit(i, 22) != sf || bit(i, 21) || (!sf && bits(i, 15, 10) >= 32) { return None; }
            return Some(vec![(rd, R::Zr), (rn, R::Zr), (rm, R::Zr)]);
        }
        _ => {}
    }
    if bits(i, 28, 24) == 0b01010 {
        if !sf && bits(i, 15, 10) >= 32 { return None; }
        return Some(vec![(rd, R::Zr), (rn, R::Zr), (rm, R::Zr)]); // logical shifted
    }
    if bits(i, 28, 24) == 0b01011 {
        if !bit(i, 21) {
            if bits(i, 23, 22) == 3 || (!sf && bits(i, 15, 10) >= 32) { return None; }
            return Some(vec![(rd, R::Zr), (rn, R::Zr), (rm, R::Zr)]);
        }
        if bits(i, 12, 10) > 4 || bits(i, 23, 22) != 0 { return None; }
        let s = bit(i, 29);
        return Some(vec![(rd, if s { R::Zr } else { R::Sp }), (rn, R::Sp), (rm, R::Zr)]); // add/sub extended
    }
    if bits(i, 28, 21) == 0b11010000 && bits(i, 15, 10) == 0 { return Some(vec![(rd, R::Zr), (rn, R::Zr), (rm, R::Zr)]); } // ADC/SBC
    if bits(i, 28, 21) == 0b11010010 {
        if !bit(i, 29) || bit(i, 10) || bit(i, 4) { return None; }
        return Some(if bit(i, 11) { vec![(rn, R::Zr)] } else { vec![(rn, R::Zr), (rm, R::Zr)] }); // CCMP/CCMN
    }
    if bits(i, 28, 21) == 0b11010100 {
        if bit(i, 29) || bit(i, 11) { return None; }
        return Some(vec![(rd, R::Zr), (rn, R::Zr), (rm, R::Zr)]); // CSEL family
    }
    if bits(i, 28, 21) == 0b11010110 {
        if bit(i, 29) { return None; }
        if !bit(i, 30) {
            if !matches!(bits(i, 15, 10), 0b000010 | 0b000011 | 0b001000..=0b001011) { return None; }
            return Some(vec![(rd, R::Zr), (rn, R::Zr), (rm, R::Zr)]);
        }
        if bits(i, 20, 16) != 0 { return None; }
        let ok = match (bits(i, 15, 10), sf) { (0, _) | (1, _) | (2, _) | (4, _) | (5, _) => true, (3, true) => true, _ => false };
        if !ok { return None; }
        return Some(vec![(rd, R::Zr), (rn, R::Zr)]);
    }
    if bits(i, 28, 24) == 0b11011 {
        if bits(i, 30, 29) != 0 { return None; }
        let ok = match (bits(i, 23, 21), sf, bit(i, 15)) { (0, _, _) => true, (1, true, _) | (5, true, _) => true, (2, true, false) | (6, true, false) => true, _ => false };
        if !ok { return None; }
        return Some(vec![(rd, R::Zr), (rn, R::Zr), (rm, R::Zr), (ra, R::Zr)]);
    }
    None
}

/// The interpreter for one instruction, called from generated code.
/// Returns 0 = continue with the next instruction, 1 = leave the block (pc set), 2 = trap.
unsafe extern "C" fn h_step(cpu: *mut Cpu, expect_next: u64) -> u32 {
    let cpu = &mut *cpu;
    let bus = &mut *cpu.jit_bus.expect("bus");
    match cpu.exec_one(bus) {
        Ok(()) => if cpu.pc == expect_next { 0 } else { 1 },
        Err(t) => { cpu.jit_trap = Some(t); 2 }
    }
}

struct Ctx { off_x: usize, off_sp: usize, off_pc: usize, off_nzcv: usize, off_instret: usize, off_utlb: usize, off_ram: usize, off_excl: usize }

fn ctx() -> Ctx {
    Ctx {
        off_x: std::mem::offset_of!(Cpu, x),
        off_sp: std::mem::offset_of!(Cpu, sp),
        off_pc: std::mem::offset_of!(Cpu, pc),
        off_nzcv: std::mem::offset_of!(Cpu, nzcv),
        off_instret: std::mem::offset_of!(Cpu, instret),
        off_utlb: std::mem::offset_of!(Cpu, utlb),
        off_ram: std::mem::offset_of!(Cpu, ram),
        off_excl: std::mem::offset_of!(Cpu, excl_armed),
    }
}

struct Emit<'a> { out: Vec<u32>, c: &'a Ctx, sp_slot: usize, pending_native: u64 }

impl Emit<'_> {
    fn reg_off(&self, r: u32) -> usize { self.c.off_x + 8 * r as usize }
    /// Fold the compile-time count of natives into x20 (the run-time native counter).
    fn count_pending(&mut self) {
        if self.pending_native == 0 { return; }
        let n = self.pending_native as u32;
        self.out.push(0x9100_0000 | (n << 10) | (X_COUNT << 5) | X_COUNT); // add x20, x20, #n
        self.pending_native = 0;
    }
    /// Add natives executed so far to instret (before anything that can observe or exit).
    fn flush_instret(&mut self) {
        self.count_pending();
        self.out.push(ldr_x(X_SCRATCH, X_CPU, self.c.off_instret));
        self.out.push(0x8b00_0000 | (X_COUNT << 16) | (X_SCRATCH << 5) | X_SCRATCH); // add x13, x13, x20
        self.out.push(str_x(X_SCRATCH, X_CPU, self.c.off_instret));
        self.out.push(0xd280_0000 | X_COUNT); // movz x20, #0
    }
    fn save_flags(&mut self) { self.out.push(MRS_NZCV_X9); self.out.push(str_w(9, X_CPU, self.c.off_nzcv)); }
    fn load_flags(&mut self) { self.out.push(ldr_w(9, X_CPU, self.c.off_nzcv)); self.out.push(MSR_NZCV_X9); }
    /// epilogue: return `status` in w0
    fn ret(&mut self, status: u32) {
        self.out.push(0x5280_0000 | (status << 5)); // movz w0, #status
        self.out.push(0xa941_53f3); // ldp x19, x20, [sp, #16]
        self.out.push(0xa8c2_7bfd); // ldp x29, x30, [sp], #32
        self.out.push(0xd65f_03c0); // ret
    }
    fn set_pc_const(&mut self, pc: u64) { mov64(&mut self.out, X_SCRATCH, pc); self.out.push(str_x(X_SCRATCH, X_CPU, self.c.off_pc)); }
    fn load_guest(&mut self, host: u32, r: u32, how: R) {
        if r == 31 { if how == R::Sp { self.out.push(ldr_x(host, X_CPU, self.sp_slot)); } } else { self.out.push(ldr_x(host, X_CPU, self.reg_off(r))); }
    }
    fn store_guest(&mut self, host: u32, r: u32, how: R) {
        if r == 31 { if how == R::Sp { self.out.push(str_x(host, X_CPU, self.sp_slot)); } } else { self.out.push(str_x(host, X_CPU, self.reg_off(r))); }
    }
}

impl Cpu {
    /// Run a compiled block at the current pc if there is one (compiling hot code).
    /// None = not handled here: the caller interprets one instruction.
    pub(crate) fn jit_step(&mut self, bus: &mut dyn Bus) -> Option<Result<(), Trap>> {
        if self.pc & 3 != 0 { return None; }
        // physical address of pc through the fetch micro-TLB (no fault taken here)
        let tag = ((self.pc >> 12) + 1) | ((self.el as u64) << 60);
        let pa = if self.utlb[0].0 == tag { self.utlb[0].1 | (self.pc & 0xfff) } else { return None };
        let k = (self.pc, key(pa, self.el, self.spsel));
        let jit = self.jit.as_mut()?;
        let slot = ((self.pc ^ (self.pc >> 14)) as usize >> 2) & (FRONT - 1);
        let f: BlockFn = if jit.front[slot].0 == k.0 && jit.front[slot].1 == k.1 {
            // SAFETY: front entries are only filled from `blocks`, which hold published code
            unsafe { std::mem::transmute::<usize, BlockFn>(jit.front[slot].2) }
        } else if let Some(&f) = jit.blocks.get(&k) {
            jit.front[slot] = (k.0, k.1, f as usize);
            f
        } else {
            let c = jit.counts.entry(k).or_insert(0);
            *c += 1;
            if *c < jit.hot { return None; }
            let f = self.compile(pa)?;
            let jit = self.jit.as_mut()?;
            jit.blocks.insert(k, f);
            jit.pages.entry(pa >> 12).or_default().push(k);
            jit.front[slot] = (k.0, k.1, f as usize);
            jit.compiled += 1;
            f
        };
        // SAFETY: the bus outlives the call; generated code only reaches it through h_step
        self.jit_bus = Some(unsafe { std::mem::transmute::<&mut dyn Bus, &'static mut dyn Bus>(bus) } as *mut dyn Bus);
        let r = unsafe { f(self as *mut Cpu) };
        self.jit_bus = None;
        Some(if r == 0 { Ok(()) } else { Err(self.jit_trap.take().unwrap_or(Trap::Wait)) })
    }

    fn compile(&mut self, pa: u64) -> Option<BlockFn> {
        let c = ctx();
        let sp_slot = c.off_sp + 8 * if self.spsel { self.el as usize } else { 0 };
        let mut e = Emit { out: Vec::with_capacity(512), c: &c, sp_slot, pending_native: 0 };
        // prologue: save x19/x20 and the frame, x19 = cpu, guest flags into host NZCV
        e.out.push(0xa9be_7bfd); // stp x29, x30, [sp, #-32]!
        e.out.push(0xa901_53f3); // stp x19, x20, [sp, #16]
        e.out.push(0xaa00_03f3); // mov x19, x0
        e.out.push(0xd280_0000 | X_COUNT); // movz x20, #0
        e.load_flags();
        let ram_ok = !self.ram.is_null() && pa + 4 <= self.ram_len;
        if !ram_ok { return None; }
        let mut pc = self.pc;
        let mut a = pa;
        let mut natives = 0u64;
        let mut ended = false;
        let mask: u32 = std::env::var("SBC_JIT_NATIVE").ok().and_then(|v| v.parse().ok()).unwrap_or(7);
        for _ in 0..MAX_INSNS {
            // SAFETY: `a` is inside guest RAM (checked) and blocks stop at the page end
            let i = unsafe { (self.ram.add(a as usize) as *const u32).read_unaligned() };
            if let Some(ops) = native_operands(i).filter(|_| mask & 1 != 0) {
                // rename guest registers to host temporaries; 31 stays ZR or becomes the SP slot
                let mut map: Vec<(u32, R, u32)> = Vec::new(); // (guest reg, kind, host temp)
                let mut enc = i;
                for &(shift, how) in &ops {
                    let r = (i >> shift) & 31;
                    if r == 31 && how == R::Zr { continue; }
                    let host = match map.iter().find(|m| m.0 == r && m.1 == how) { Some(m) => m.2, None => { let h = T[map.len()]; map.push((r, how, h)); h } };
                    enc = (enc & !(31 << shift)) | (host << shift);
                }
                for &(r, how, h) in &map { e.load_guest(h, r, how); }
                e.out.push(enc);
                // write back the destination (field 0) unless the instruction has none (CCMP)
                let has_rd = !(bits(i, 28, 21) == 0b11010010);
                if has_rd {
                    let rd = i & 31;
                    let how = ops[0].1;
                    if !(rd == 31 && how == R::Zr) {
                        let h = map.iter().find(|m| m.0 == rd && m.1 == how).map(|m| m.2).unwrap();
                        e.store_guest(h, rd, how);
                    }
                }
                natives += 1;
                e.pending_native += 1;
            } else if bits(i, 28, 24) == 0b10000 && mask & 4 != 0 {
                // ADR / ADRP: a constant
                let imm = (((((bits(i, 23, 5) << 2) | bits(i, 30, 29)) as u64) << 43) as i64 >> 43) as u64;
                let sx = if bit(i, 31) { (pc & !0xfff).wrapping_add(imm << 12) } else { pc.wrapping_add(imm) };
                let rd = i & 31;
                if rd != 31 { mov64(&mut e.out, T[0], sx); e.out.push(str_x(T[0], X_CPU, e.reg_off(rd))); }
                natives += 1;
                e.pending_native += 1;
            } else if i == 0xd503_201f {
                natives += 1; // NOP
                e.pending_native += 1;
            } else if let Some(done) = if mask & 2 != 0 { self.emit_branch(&mut e, i, pc) } else { None } {
                if done { natives += 1; ended = true; break; }
            } else if self.emit_ldst(&mut e, i, pc) {
                // native load/store with an interpreter slow path
            } else {
                if emit_interp(&mut e, i, pc) { ended = true; break; }
            }
            pc += 4;
            a += 4;
            if a & 0xfff == 0 { break; } // never cross a page
        }
        let _ = natives;
        if !ended {
            // fell off the end of the block: continue at pc
            e.flush_instret();
            e.save_flags();
            e.set_pc_const(pc);
            e.ret(0);
        }
        let f = self.jit.as_mut()?.code.write(&e.out);
        let f = match f {
            Some(p) => p,
            None => { self.jit.as_mut()?.flush(); return None; }
        };
        // SAFETY: the buffer now holds a complete function with the BlockFn ABI
        Some(unsafe { std::mem::transmute::<*const u8, BlockFn>(f) })
    }

    /// Branches end a block: emit the next-pc computation. Some(true) = emitted and the block
    /// ends; None = not a branch handled natively.
    fn emit_branch(&self, e: &mut Emit, i: u32, pc: u64) -> Option<bool> {
        let sx = |v: u64, w: u32| ((v << (64 - w)) as i64 >> (64 - w)) as u64;
        // B / BL
        if bits(i, 30, 26) == 0b00101 {
            let target = pc.wrapping_add(sx((bits(i, 25, 0) as u64) << 2, 28));
            if bit(i, 31) { mov64(&mut e.out, T[0], pc + 4); e.out.push(str_x(T[0], X_CPU, e.reg_off(30))); }
            e.pending_native += 1;
            e.flush_instret();
            e.save_flags();
            e.set_pc_const(target);
            e.ret(0);
            return Some(true);
        }
        // B.cond: select with the guest flags still in the host NZCV
        if bits(i, 31, 24) == 0b0101_0100 && !bit(i, 4) {
            let target = pc.wrapping_add(sx((bits(i, 23, 5) as u64) << 2, 21));
            mov64(&mut e.out, T[0], pc + 4);
            mov64(&mut e.out, T[1], target);
            e.out.push(0x9a80_0000 | (T[0] << 16) | (bits(i, 3, 0) << 12) | (T[1] << 5) | T[0]); // csel x9, x10, x9, cond
            e.out.push(str_x(T[0], X_CPU, e.c.off_pc));
            e.pending_native += 1;
            e.flush_instret();
            e.save_flags();
            e.ret(0);
            return Some(true);
        }
        // CBZ/CBNZ, TBZ/TBNZ: test with a host cbz/tbz (flags untouched)
        let cb = bits(i, 30, 25) == 0b011010;
        let tb = bits(i, 30, 25) == 0b011011;
        if cb || tb {
            let rt = i & 31;
            let target = if cb { pc.wrapping_add(sx((bits(i, 23, 5) as u64) << 2, 21)) } else { pc.wrapping_add(sx((bits(i, 18, 5) as u64) << 2, 16)) };
            if rt == 31 { e.out.push(0xaa1f_03e0 | T[2]); } else { e.out.push(ldr_x(T[2], X_CPU, e.reg_off(rt))); }
            // x9 = target, x14 = fall-through; the host branch (same test, Rt = x11) skips the
            // single "mov x9, x14" when taken
            mov64(&mut e.out, T[0], target);
            mov64(&mut e.out, 14, pc + 4);
            let host = if cb { (i & !(31 | (0x7ffff << 5))) | T[2] | (2 << 5) } else { (i & !(31 | (0x3fff << 5))) | T[2] | (2 << 5) };
            e.out.push(host);
            e.out.push(0xaa0e_03e0 | T[0]); // mov x9, x14
            e.out.push(str_x(T[0], X_CPU, e.c.off_pc));
            e.pending_native += 1;
            e.flush_instret();
            e.save_flags();
            e.ret(0);
            return Some(true);
        }
        // BR / BLR / RET
        if bits(i, 31, 25) == 0b110_1011 && bits(i, 20, 16) == 0b11111 && bits(i, 15, 10) == 0 && bits(i, 4, 0) == 0 {
            let opc = bits(i, 24, 21);
            if opc > 2 { return None; }
            let rn = bits(i, 9, 5);
            if rn == 31 { e.out.push(0xaa1f_03e0 | T[0]); } else { e.out.push(ldr_x(T[0], X_CPU, e.reg_off(rn))); }
            if opc == 1 { mov64(&mut e.out, T[1], pc + 4); e.out.push(str_x(T[1], X_CPU, e.reg_off(30))); }
            e.out.push(str_x(T[0], X_CPU, e.c.off_pc));
            e.pending_native += 1;
            e.flush_instret();
            e.save_flags();
            e.ret(0);
            return Some(true);
        }
        None
    }
}

/// Call the interpreter for the instruction at `pc`; returns true if the block ends here.
fn emit_interp(e: &mut Emit, i: u32, pc: u64) -> bool {
    e.flush_instret();
    e.save_flags();
    e.set_pc_const(pc);
    e.out.push(0xaa13_03e0); // mov x0, x19
    mov64(&mut e.out, 1, pc + 4);
    mov64(&mut e.out, 16, h_step as *const () as usize as u64);
    e.out.push(0xd63f_0200); // blr x16
    if is_block_end(i) {
        // status 0 or 1: pc already set by the interpreter; 2: trap
        e.out.push(0x7100_081f); // cmp w0, #2
        e.out.push(0x5400_00a1); // b.ne +5 (to the status-0 return)
        e.ret_inline(1);
        e.ret(0);
        return true;
    }
    // continue only on status 0
    e.out.push(0x3400_0000 | (e_skip_len() << 5)); // cbz w0, continue
    e.out.push(0x7100_081f); // cmp w0, #2
    e.out.push(0x5400_00a1); // b.ne +5
    e.ret_inline(1);
    e.ret(0);
    e.load_flags();
    false
}

impl Cpu {
    /// Integer LDR/STR (unsigned offset, unscaled, pre/post-index, register offset) and
    /// LDP/STP/LDPSW: a micro-TLB fast path on guest RAM, the interpreter otherwise.
    fn emit_ldst(&self, e: &mut Emit, i: u32, pc: u64) -> bool {
        if self.ram.is_null() { return false; }
        // ---- decode into (base, address computation, size, kind, writeback, pair)
        enum Addr { Imm(i64), Reg(u32, u32, u32) } // Reg(rm, option, shift)
        let rt = i & 31;
        let rn = (i >> 5) & 31;
        let (size, load, signed_to, pair, rt2, addr, mode): (u32, bool, u32, bool, u32, Addr, u8);
        // mode: 0 = offset, 1 = post-index, 2 = pre-index
        if bits(i, 29, 27) == 0b111 && !bit(i, 26) {
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
                match bits(i, 11, 10) { 0 => { addr = Addr::Imm(imm); mode = 0; } 1 => { addr = Addr::Imm(imm); mode = 1; } 3 => { addr = Addr::Imm(imm); mode = 2; } _ => return false }
            }
        } else if bits(i, 29, 27) == 0b101 && !bit(i, 26) && bits(i, 24, 23) != 0 {
            let opc = bits(i, 31, 30);
            let ld = bit(i, 22);
            let (sz, st) = match (opc, ld) { (0, _) => (2, 0), (1, true) => (2, 64), (2, _) => (3, 0), _ => return false };
            size = 1 << sz; load = ld; signed_to = st; pair = true; rt2 = bits(i, 14, 10);
            let imm = (((bits(i, 21, 15) as i64) << 57) >> 57) << sz;
            addr = Addr::Imm(imm);
            mode = match bits(i, 24, 23) { 1 => 1, 3 => 2, _ => 0 };
            if load && rt == rt2 { return false; }
        } else {
            return false;
        }
        if mode != 0 && rn != 31 && (rn == rt || (pair && rn == rt2)) { return false; }
        let span = if pair { 2 * size } else { size };
        let c = e.c;
        let (vreg, tmp, tmp2) = (13u32, 14u32, 15u32);
        // ---- VA in x13 (post-index: the access uses the base; x16 keeps the written-back value)
        e.count_pending();
        if rn == 31 { e.out.push(ldr_x(vreg, X_CPU, e.sp_slot)); } else { e.out.push(ldr_x(vreg, X_CPU, e.reg_off(rn))); }
        let add_imm = |out: &mut Vec<u32>, rd: u32, rn: u32, imm: i64| {
            if imm == 0 { if rd != rn { out.push(0x9100_0000 | (rn << 5) | rd); } return; }
            let (op, v) = if imm < 0 { (0xd100_0000u32, (-imm) as u64) } else { (0x9100_0000u32, imm as u64) };
            if v < 4096 { out.push(op | ((v as u32) << 10) | (rn << 5) | rd); }
            else { mov64(out, 17, v); out.push(if imm < 0 { 0xcb00_0000 } else { 0x8b00_0000 } | (17 << 16) | (rn << 5) | rd); }
        };
        match addr {
            Addr::Imm(imm) => match mode {
                0 => add_imm(&mut e.out, vreg, vreg, imm),
                1 => add_imm(&mut e.out, 16, vreg, imm),
                _ => { add_imm(&mut e.out, vreg, vreg, imm); e.out.push(0xaa00_03e0 | (vreg << 16) | 16); } // x16 = x13
            },
            Addr::Reg(rm, option, shift) => {
                if rm == 31 { e.out.push(0xaa1f_03e0 | tmp); } else { e.out.push(ldr_x(tmp, X_CPU, e.reg_off(rm))); }
                e.out.push(0x8b20_0000 | (tmp << 16) | (option << 13) | (shift << 10) | (vreg << 5) | vreg); // add x13, x13, x14, <ext> #sh
            }
        }
        // ---- micro-TLB check (no flags touched): tag = (va >> 12) + 1 | el << 60
        let k = if load { 1 } else { 2 };
        let mut slow: Vec<usize> = Vec::new();
        e.out.push(0xd34c_fc00 | (vreg << 5) | tmp); // lsr x14, x13, #12
        e.out.push(0x9100_0400 | (tmp << 5) | tmp); // add x14, x14, #1
        if self.el != 0 { mov64(&mut e.out, tmp2, (self.el as u64) << 60); e.out.push(0xaa00_0000 | (tmp2 << 16) | (tmp << 5) | tmp); }
        e.out.push(ldr_x(tmp2, X_CPU, c.off_utlb + 16 * k));
        e.out.push(0xcb00_0000 | (tmp2 << 16) | (tmp << 5) | tmp); // sub x14, x14, x15
        slow.push(e.out.len()); e.out.push(0xb500_0000 | tmp); // cbnz x14, slow
        e.out.push(0x9240_2c00 | (vreg << 5) | tmp2); // and x15, x13, #0xfff
        e.out.push(0x9100_0000 | ((span - 1) << 10) | (tmp2 << 5) | tmp); // add x14, x15, #span-1
        slow.push(e.out.len()); e.out.push(0x3760_0000 | tmp); // tbnz w14, #12, slow
        if !load {
            e.out.push(ldr_x(tmp, X_CPU, c.off_excl));
            slow.push(e.out.len()); e.out.push(0xb500_0000 | tmp); // cbnz x14, slow (reservation held)
        }
        e.out.push(ldr_x(tmp, X_CPU, c.off_utlb + 16 * k + 8)); // physical page
        e.out.push(0x8b00_0000 | (tmp2 << 16) | (tmp << 5) | tmp); // add x14, x14, x15
        e.out.push(ldr_x(tmp2, X_CPU, c.off_ram));
        e.out.push(0x8b00_0000 | (tmp2 << 16) | (tmp << 5) | tmp); // x14 = host address
        // ---- the access itself, as the host instruction of the same size and extension
        let sz = size.trailing_zeros();
        let one = |rt: u32, off: u32| -> u32 {
            // LDR/STR (unsigned offset) Rt, [x14, #off]
            let opc = if !load { 0 } else { match signed_to { 0 => 1, 64 => 2, _ => 3 } };
            0x3900_0000 | (sz << 30) | (opc << 22) | ((off >> sz) << 10) | (tmp << 5) | rt
        };
        let regs = if pair { vec![(rt, 0u32, 9u32), (rt2, size, 10u32)] } else { vec![(rt, 0, 9)] };
        if load {
            if pair && signed_to == 64 {
                e.out.push(0x6940_0000 | (10 << 10) | (tmp << 5) | 9); // ldpsw x9, x10, [x14]
            } else if pair {
                e.out.push(if sz == 3 { 0xa940_0000 } else { 0x2940_0000 } | (10 << 10) | (tmp << 5) | 9); // ldp
            } else {
                e.out.push(one(9, 0));
            }
            for &(r, _, h) in &regs { if r != 31 { e.out.push(str_x(h, X_CPU, e.reg_off(r))); } }
        } else {
            for &(r, _, h) in &regs { if r == 31 { e.out.push(0xaa1f_03e0 | h); } else { e.out.push(ldr_x(h, X_CPU, e.reg_off(r))); } }
            if pair { e.out.push(if sz == 3 { 0xa900_0000 } else { 0x2900_0000 } | (10 << 10) | (tmp << 5) | 9); } // stp
            else { e.out.push(one(9, 0)); }
        }
        // ---- base writeback
        match mode {
            1 | 2 => { if rn == 31 { e.out.push(str_x(16, X_CPU, e.sp_slot)); } else { e.out.push(str_x(16, X_CPU, e.reg_off(rn))); } }
            _ => {}
        }
        e.out.push(0x9100_0400 | (X_COUNT << 5) | X_COUNT); // add x20, x20, #1
        let skip_at = e.out.len();
        e.out.push(0x1400_0000); // b done (patched)
        // ---- slow path: the interpreter
        let slow_at = e.out.len();
        for b in slow {
            let off = (slow_at - b) as u32;
            let w = e.out[b];
            e.out[b] = if w & 0x7e00_0000 == 0x3600_0000 { w | ((off & 0x3fff) << 5) } else { w | ((off & 0x7ffff) << 5) };
        }
        let ended = emit_interp(e, i, pc);
        debug_assert!(!ended);
        let done = e.out.len();
        e.out[skip_at] |= ((done - skip_at) as u32) & 0x3ff_ffff;
        true
    }
}

impl Emit<'_> {
    fn ret_inline(&mut self, status: u32) {
        self.out.push(0x5280_0000 | (status << 5)); // movz w0, #status
        self.out.push(0xa941_53f3);
        self.out.push(0xa8c2_7bfd);
        self.out.push(0xd65f_03c0);
    }
}

/// cbz w0 skip length: over "cmp; b.ne; ret_inline(4); ret(4)" = 10 instructions, +1
fn e_skip_len() -> u32 { 11 }

/// Interpreted instructions after which the block must end (they can change the translation
/// regime, the exception level, the code itself or interrupt masking).
fn is_block_end(i: u32) -> bool {
    bits(i, 31, 22) == 0b11_0101_0100 // system: MSR/MRS/SYS/hints/barriers
        || bits(i, 31, 24) == 0b1101_0100 // exception generation
        || (bits(i, 31, 25) == 0b110_1011) // branch-register class incl. ERET
}

pub fn available() -> bool { host::AVAILABLE }
