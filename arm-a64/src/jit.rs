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
use crate::cpu::UT;
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
        /// Replace one instruction word of already written code.
        pub fn patch(&mut self, at: usize, word: u32) {
            debug_assert!(at >= self.base as usize && at + 4 <= self.base as usize + self.used);
            // SAFETY: `at` is a word of code written earlier into the mapping
            unsafe {
                #[cfg(target_os = "macos")] pthread_jit_write_protect_np(0);
                (at as *mut u32).write(word);
                #[cfg(target_os = "macos")] pthread_jit_write_protect_np(1);
                #[cfg(target_os = "macos")] sys_icache_invalidate(at as *mut c_void, 4);
                #[cfg(target_os = "linux")] __clear_cache(at as *mut c_void, (at + 4) as *mut c_void);
            }
        }
    }
    pub const AVAILABLE: bool = true;
}

#[cfg(not(all(target_arch = "aarch64", any(target_os = "macos", target_os = "linux"))))]
mod host {
    /// No native code buffer: the wasm32 build compiles through `jit_wasm` instead.
    pub struct CodeCache;
    impl CodeCache {
        pub fn new(_size: usize) -> Option<CodeCache> { if AVAILABLE { Some(CodeCache) } else { None } }
        pub fn reset(&mut self) {}
        pub fn write(&mut self, _w: &[u32]) -> Option<*const u8> { None }
        pub fn patch(&mut self, _at: usize, _word: u32) {}
    }
    pub const AVAILABLE: bool = cfg!(target_arch = "wasm32");
}

/// Compiled block entry: `fn(cpu) -> status` (0 = continue at cpu.pc, 1 = trap in cpu.jit_trap).
pub(crate) type BlockFn = unsafe extern "C" fn(*mut Cpu) -> u32;

const HOT: u32 = 8;
const MAX_INSNS: usize = 64;
const CODE_BYTES: usize = 128 << 20;

pub struct Jit {
    code: host::CodeCache,
    /// (virtual pc, physical pc | el | spsel) -> code: blocks embed virtual PCs, so both matter
    blocks: Map<(u64, u64), BlockFn>,
    /// direct-mapped front cache over `blocks`
    front: Box<[(u64, u64, usize)]>,
    /// direct-mapped execution counters for blocks not compiled yet: (pc, key, count)
    hotc: Box<[(u64, u64, u32)]>,
    /// FP/SIMD instruction words the interpreter implements (see `interp_supports`)
    probe: Map<u32, bool>,
    scratch: Option<Box<(Cpu, emu_core::FlatRam)>>,
    /// physical page -> blocks compiled from it (for IC IVAU)
    pages: Map<u64, Vec<(u64, u64)>>,
    pub compiled: u64,
    pub flushes: u64,
    pub native: u64,
    /// executions before a block is compiled
    pub hot: u32,
    /// debugging: re-run every block in the interpreter and compare (see `jit_step`)
    pub verify: bool,
    /// the first mismatch found in verify mode
    pub report: Option<String>,
    /// verify mode: hash of the code each block was compiled from (stale blocks are legal until IC)
    code_hash: Map<(u64, u64), u64>,
    /// verify mode: blocks skipped because their code changed since compilation
    pub stale_skips: u64,
    /// compiled blocks entered, and guest instructions retired inside them
    pub execs: u64,
    /// the block dispatcher in the code cache (emitted after each reset)
    disp: Option<u64>,
    /// direct links: target block key -> branch sites (code addresses) waiting for it
    links: Map<(u64, u64), Vec<usize>>,
    /// link sites of the block just compiled: (code address, target virtual pc)
    new_sites: Vec<(usize, u64)>,
    pub linked: u64,
    pub exec_insns: u64,
    /// flushes because the code cache was full (the rest come from IC IALLU or an IC broadcast)
    pub full_flushes: u64,
    /// pages invalidated (IC IVAU) and the compiled blocks they dropped
    pub inval_pages: u64,
    pub inval_blocks: u64,
}

/// front cache slots: large enough for a desktop's working set (Ubuntu's GNOME runs ~10^5 hot
/// blocks; a block evicted from here is found again in `blocks`, but only through Rust)
const FRONT: usize = 1 << 16;
const HOTC: usize = 1 << 16;
/// wasm32: live compiled blocks before the cache is flushed (each is a module in the engine)
#[cfg(target_arch = "wasm32")]
const WASM_MAX_BLOCKS: usize = 32768;

impl Jit {
    pub fn new() -> Option<Box<Jit>> {
        // SBC_JIT_HOT / SBC_JIT_MB: experiments with the compile threshold and the cache size
        let env = |k: &str| std::env::var(k).ok().and_then(|v| v.parse::<usize>().ok());
        let hot = env("SBC_JIT_HOT").map(|h| h as u32).unwrap_or(if cfg!(target_arch = "wasm32") { 1024 } else { HOT });
        Self::with(env("SBC_JIT_MB").map(|m| (m << 20).min(CODE_BYTES)).unwrap_or(CODE_BYTES), hot) // branches reach 128 MiB
    }

    pub fn with(code_bytes: usize, hot: u32) -> Option<Box<Jit>> {
        if !host::AVAILABLE { return None; }
        Some(Box::new(Jit { code: host::CodeCache::new(code_bytes)?, blocks: Map::default(), front: vec![(u64::MAX, u64::MAX, 0); FRONT].into_boxed_slice(),
                            hotc: vec![(u64::MAX, u64::MAX, 0); HOTC].into_boxed_slice(), probe: Map::default(), scratch: None, pages: Map::default(), compiled: 0, flushes: 0, native: 0, hot, verify: false, report: None, code_hash: Map::default(), stale_skips: 0, execs: 0, exec_insns: 0, disp: None, links: Map::default(), new_sites: Vec::new(), linked: 0, full_flushes: 0, inval_pages: 0, inval_blocks: 0 }))
    }

    pub fn flush(&mut self) {
        if watch_page().is_some() { eprintln!("[jit-watch] flush all"); }
        self.blocks.clear();
        self.hotc.fill((u64::MAX, u64::MAX, 0));
        self.pages.clear();
        self.front.fill((u64::MAX, u64::MAX, 0));
        self.code.reset();
        self.disp = None;
        self.links.clear();
        self.new_sites.clear();
        self.flushes += 1;
        #[cfg(target_arch = "wasm32")]
        crate::jit_wasm::reset();
    }

    /// Resolve direct branches after block `k` (code at `f`) was added: its own exits to blocks
    /// that exist, and exits of earlier blocks that were waiting for it. A linked branch enters
    /// past the prologue (the frame is already set up).
    fn link(&mut self, k: (u64, u64), f: usize) {
        const BODY: usize = 16;
        let branch = |site: usize, to: usize| 0x1400_0000 | ((((to as i64 - site as i64) >> 2) as u32) & 0x3ff_ffff);
        let mode = k.1 & !((1u64 << 48) - 1);
        let page_pa = k.1 & ((1u64 << 48) - 1) & !0xfff;
        for (site, t) in std::mem::take(&mut self.new_sites) {
            let tk = (t, page_pa | (t & 0xfff) | mode);
            if let Some(&tf) = self.blocks.get(&tk) {
                self.code.patch(site, branch(site, tf as usize + BODY));
                self.linked += 1;
            } else {
                self.links.entry(tk).or_default().push(site);
            }
        }
        if let Some(waiting) = self.links.remove(&k) {
            for site in waiting { self.code.patch(site, branch(site, f + BODY)); self.linked += 1; }
        }
    }

    /// Drop blocks compiled from physical page `page` (4 KiB).
    /// Does the interpreter implement instruction `i` (FP/SIMD)? Runs it once on a scratch core.
    fn interp_supports(&mut self, i: u32) -> bool {
        if let Some(&r) = self.probe.get(&i) { return r; }
        let (c, ram) = &mut **self.scratch.get_or_insert_with(|| Box::new((Cpu::new(), emu_core::FlatRam::new(0, 16))));
        let _ = ram.load(0, &i.to_le_bytes());
        c.el = 1;
        c.pc = 0;
        c.sys.cpacr_el1 = 3 << 20;
        c.vbar[1] = 0x800; // an exception lands elsewhere; detected by the pc
        let r = matches!(c.exec_one(ram), Ok(())) && c.pc == 4;
        if self.probe.len() < 1 << 16 { self.probe.insert(i, r); }
        r
    }

    pub fn invalidate_page(&mut self, page: u64) {
        if watch_page() == Some(page) { eprintln!("[jit-watch] invalidate page {page:#x} ({} blocks)", self.pages.get(&page).map_or(0, |v| v.len())); }
        self.inval_pages += 1;
        let Some(keys) = self.pages.remove(&page) else { return };
        self.inval_blocks += keys.len() as u64;
        for k in keys {
            self.blocks.remove(&k);
            let slot = ((k.0 ^ (k.0 >> 14)) as usize >> 2) & (FRONT - 1);
            if self.front[slot].0 == k.0 && self.front[slot].1 == k.1 { self.front[slot] = (u64::MAX, u64::MAX, 0); }
        }
    }
}

/// FPCR fields that change native FP results: AHP (26), DN (25), FZ (24), RMode (23:22).
#[cfg(target_arch = "aarch64")]
const FPCR_HOST_MASK: u64 = 0x1f << 22;

/// # Safety
/// Only changes how host floating point rounds and flushes; callers restore 0.
#[cfg(target_arch = "aarch64")]
#[inline(always)]
unsafe fn set_host_fpcr(v: u64) { std::arch::asm!("msr fpcr, {}", in(reg) v, options(nomem, nostack)); }

/// debugging: `SBC_JIT_CHAIN=0` turns block chaining and linking off
fn chain_enabled() -> bool {
    static C: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *C.get_or_init(|| std::env::var("SBC_JIT_CHAIN").map_or(true, |v| v != "0"))
}

/// debugging: `SBC_JIT_WATCH=<physical page number, hex>` traces compiles/invalidations there
fn watch_page() -> Option<u64> {
    static W: std::sync::OnceLock<Option<u64>> = std::sync::OnceLock::new();
    *W.get_or_init(|| std::env::var("SBC_JIT_WATCH").ok().and_then(|v| u64::from_str_radix(v.trim_start_matches("0x"), 16).ok()))
}

/// Hash of the instruction words a block at `pa` may span (to the end of its page).
fn code_hash(cpu: &Cpu, pa: u64) -> u64 {
    let end = ((pa | 0xfff) + 1).min(pa + 4 * MAX_INSNS as u64).min(cpu.ram_len);
    let mut h = 0xcbf2_9ce4_8422_2325u64;
    let mut a = pa;
    while a + 4 <= end {
        // SAFETY: inside guest RAM (bounded by ram_len)
        let w = unsafe { (cpu.ram.add(a as usize) as *const u32).read_unaligned() };
        h = (h ^ w as u64).wrapping_mul(0x100_0000_01b3);
        a += 4;
    }
    h
}

fn key(pa: u64, el: u8, spsel: bool, fpen: u64) -> u64 { pa | ((el as u64) << 56) | ((spsel as u64) << 58) | ((fpen & 3) << 59) }

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

/// A V-register operand: bit position of its field, field width (5, or 4 for a by-element
/// halfword Rm whose bit 20 is an index bit), and how many consecutive registers (TBL tables).
#[derive(Clone, Copy)]
struct VOp { shift: u32, width: u32, count: u32 }

/// Pure V-register FP/SIMD instructions: (operands, writes Rd). None = not handled natively
/// (mixed general/vector register instructions, loads/stores, anything else).
/// LD1/ST1 {Vt.<T>}, [Xn] or [Xn], #8/#16 with one register: on a little-endian machine the
/// bytes land exactly as LDR/STR Qt (Dt for 64-bit vectors) of the same address would put them.
fn ld1_as_ldr(i: u32) -> Option<u32> {
    let (q, load, rn, rt) = (i >> 30 & 1, i >> 22 & 1 != 0, (i >> 5) & 31, i & 31);
    let base = i & 0xbfbf_f000; // without Q, L
    let (post, rm) = (i >> 23 & 1 != 0, (i >> 16) & 31);
    if base == 0x0c00_7000 && !post {
        // offset 0
        return Some(match (q, load) { (1, true) => 0x3dc0_0000, (1, false) => 0x3d80_0000, (_, true) => 0xfd40_0000, (_, false) => 0xfd00_0000 } | (rn << 5) | rt);
    }
    if (i & 0xbfa0_f000) == 0x0c80_7000 && rm == 31 {
        // post-index by the register size
        let imm = if q == 1 { 16u32 } else { 8 };
        return Some(match (q, load) { (1, true) => 0x3cc0_0400, (1, false) => 0x3c80_0400, (_, true) => 0xfc40_0400, (_, false) => 0xfc00_0400 } | (imm << 12) | (rn << 5) | rt);
    }
    None
}

fn simd_operands(i: u32) -> Option<(Vec<VOp>, bool)> {
    let v = |shift| VOp { shift, width: 5, count: 1 };
    let (d, n, m, a) = (v(0), v(5), v(16), v(10));
    // scalar floating point (single/double only)
    if !bit(i, 31) && !bit(i, 30) && !bit(i, 29) && bits(i, 28, 24) == 0b11110 {
        if bits(i, 23, 22) > 1 || !bit(i, 21) { return None; }
        if bits(i, 15, 10) == 0 { return None; } // conversions with general registers
        if bits(i, 14, 10) == 0b10000 { return Some((vec![d, n], true)); }
        if bits(i, 13, 10) == 0b1000 && bits(i, 15, 14) == 0 { return Some((if bit(i, 3) { vec![n] } else { vec![n, m] }, false)); } // FCMP
        if bits(i, 12, 10) == 0b100 { return Some((vec![d], true)); } // FMOV imm
        return match bits(i, 11, 10) { 0b01 => Some((vec![n, m], false)), 0b10 | 0b11 => Some((vec![d, n, m], true)), _ => None };
    }
    if !bit(i, 31) && !bit(i, 30) && !bit(i, 29) && bits(i, 28, 24) == 0b11111 {
        if bits(i, 23, 22) > 1 { return None; }
        return Some((vec![d, n, m, a], true)); // FMADD family
    }
    let vec_grp = !bit(i, 31) && bits(i, 28, 24) == 0b01110;
    let sc_grp = !bit(i, 31) && bit(i, 30) && bits(i, 28, 24) == 0b11110;
    if vec_grp || sc_grp {
        if bit(i, 21) && bit(i, 10) { return Some((vec![d, n, m], true)); } // three same
        if bit(i, 21) && bits(i, 11, 10) == 0b10 && (bits(i, 20, 17) == 0 || bits(i, 20, 17) == 0b1000) { return Some((vec![d, n], true)); } // two-misc, across
        if vec_grp && bit(i, 21) && bits(i, 11, 10) == 0 { return Some((vec![d, n, m], true)); } // three different
        if vec_grp && bits(i, 29, 21) == 0b001110000 && bit(i, 10) && !bit(i, 15) {
            return if bits(i, 14, 11) == 0 { Some((vec![d, n], true)) } else { None }; // DUP (element); general-register forms not native
        }
        if vec_grp && bits(i, 29, 21) == 0b101110000 && bit(i, 10) && !bit(i, 15) { return Some((vec![d, n], true)); } // INS (element)
        if sc_grp && bits(i, 29, 21) == 0b011110000 && bits(i, 15, 10) == 1 { return Some((vec![d, n], true)); } // DUP scalar
        if vec_grp && !bit(i, 21) && !bit(i, 15) && bits(i, 11, 10) == 0b10 && !bit(i, 29) { return Some((vec![d, n, m], true)); } // permute
        if vec_grp && bits(i, 29, 21) & 0b111111001 == 0b101110000 && !bit(i, 10) && !bit(i, 15) { return Some((vec![d, n, m], true)); } // EXT
        if vec_grp && !bit(i, 29) && bits(i, 23, 21) == 0 && bits(i, 11, 10) == 0 && !bit(i, 15) {
            let len = bits(i, 14, 13) + 1;
            return Some((vec![d, VOp { shift: 5, width: 5, count: len }, m], true)); // TBL/TBX
        }
        return None;
    }
    if !bit(i, 31) && (bits(i, 28, 24) == 0b01111 || (bit(i, 30) && bits(i, 28, 24) == 0b11111)) {
        if bit(i, 10) {
            if bits(i, 22, 19) == 0 { return if bits(i, 28, 19) == 0b0111100000 { Some((vec![d], true)) } else { None }; } // modified immediate
            return Some((vec![d, n], true)); // shift by immediate
        }
        // by element: a halfword Rm is v0..v15 with bit 20 an index bit
        let mw = if bits(i, 23, 22) == 1 { VOp { shift: 16, width: 4, count: 1 } } else { m };
        return Some((vec![d, n, mw], true));
    }
    None
}

/// Rename the guest V registers of `i` to host v0..v7: (re-encoded instruction, (guest, host)).
/// None when the operands cannot be mapped (table overlaps, too many registers).
fn map_simd(i: u32, ops: &[VOp]) -> Option<(u32, Vec<(u32, u32)>)> {
    let mut map: Vec<(u32, u32)> = Vec::new();
    let mut enc = i;
    let mut order: Vec<VOp> = ops.to_vec();
    order.sort_by_key(|o| std::cmp::Reverse(o.count)); // tables need consecutive host registers
    let mut next = 0u32;
    for o in &order {
        let fm = if o.width == 4 { 15 } else { 31 };
        let r = (i >> o.shift) & fm;
        let host0 = if o.count > 1 {
            if (0..o.count).any(|k| map.iter().any(|m| m.0 == (r + k) % 32)) { return None; }
            let h = next;
            for k in 0..o.count { map.push(((r + k) % 32, h + k)); }
            next += o.count;
            h
        } else if let Some(&(_, h)) = map.iter().find(|m| m.0 == r) {
            h
        } else {
            let h = next;
            map.push((r, h));
            next += 1;
            h
        };
        if next > 8 || (o.width == 4 && host0 > 15) { return None; }
        enc = (enc & !(fm << o.shift)) | (host0 << o.shift);
    }
    Some((enc, map))
}

/// The interpreter for one instruction, called from generated code.
/// Returns 0 = continue with the next instruction, 1 = leave the block (pc set), 2 = trap.
/// `SBC_JIT_PROFILE=1`: count the instruction words that compiled blocks hand to the interpreter.
pub static PROFILE: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
static FALLBACKS: std::sync::Mutex<Option<std::collections::HashMap<u32, u64>>> = std::sync::Mutex::new(None);

/// The interpreter fallbacks counted so far, most frequent first.
pub fn profile_fallbacks() -> Vec<(u32, u64)> {
    let mut v: Vec<(u32, u64)> = FALLBACKS.lock().unwrap().as_ref().map_or(Vec::new(), |f| f.iter().map(|(k, v)| (*k, *v)).collect());
    v.sort_by(|a, b| b.1.cmp(&a.1));
    v
}

#[cold]
pub(crate) fn profile_fallback(cpu: &Cpu) {
    let e = cpu.utlb[0][(cpu.pc >> 12) as usize & (UT - 1)];
    let pa = if e.0 == ((cpu.pc >> 12) + 1) | ((cpu.el as u64) << 60) { e.1 | (cpu.pc & 0xfff) } else { return };
    if pa + 4 > cpu.ram_len { return; }
    // SAFETY: inside guest RAM
    let i = unsafe { (cpu.ram.add(pa as usize) as *const u32).read_unaligned() };
    *FALLBACKS.lock().unwrap().get_or_insert_with(Default::default).entry(i).or_insert(0) += 1;
}

unsafe extern "C" fn h_step(cpu: *mut Cpu, expect_next: u64) -> u32 {
    let cpu = &mut *cpu;
    if PROFILE.load(std::sync::atomic::Ordering::Relaxed) { profile_fallback(cpu); }
    let bus = &mut *cpu.jit_bus.expect("bus");
    let r = cpu.exec_one(bus);
    if is_block_end(cpu.cur_insn) { cpu.chain_stop = true; }
    match r {
        // leaving on chain_stop keeps it clear at every native exit (linked exits rely on that)
        Ok(()) => if cpu.pc == expect_next && !cpu.chain_stop { 0 } else { 1 },
        Err(t) => { cpu.jit_trap = Some(t); 2 }
    }
}

struct Ctx { off_utlb_unpriv: usize, off_tpidr: usize, off_chain_until: usize, off_chain_stop: usize, off_disp: usize, off_x: usize, off_sp: usize, off_pc: usize, off_nzcv: usize, off_instret: usize, off_utlb: usize, off_ram: usize, off_excl: usize, off_excl_addr: usize, off_excl_val: usize, off_v: usize, off_fpsr: usize }

/// A Cpu field offset an `ldr x, [x19, #off]` can reach (scaled 12-bit immediate).
fn c_off_ok(off: usize) -> Option<usize> { (off % 8 == 0 && off < 32768).then_some(off) }

fn v_off(c: &Ctx, r: u32) -> u32 { (c.off_v + 16 * r as usize) as u32 }

fn ctx() -> Ctx {
    Ctx {
        off_utlb_unpriv: std::mem::offset_of!(Cpu, utlb_unpriv),
        off_tpidr: std::mem::offset_of!(Cpu, sys) + std::mem::offset_of!(crate::sysreg::SysRegs, tpidr),
        off_chain_until: std::mem::offset_of!(Cpu, chain_until),
        off_chain_stop: std::mem::offset_of!(Cpu, chain_stop),
        off_disp: std::mem::offset_of!(Cpu, jit_disp),
        off_x: std::mem::offset_of!(Cpu, x),
        off_sp: std::mem::offset_of!(Cpu, sp),
        off_pc: std::mem::offset_of!(Cpu, pc),
        off_nzcv: std::mem::offset_of!(Cpu, nzcv),
        off_instret: std::mem::offset_of!(Cpu, instret),
        off_utlb: std::mem::offset_of!(Cpu, utlb),
        off_ram: std::mem::offset_of!(Cpu, ram),
        off_excl: std::mem::offset_of!(Cpu, excl_armed),
        off_excl_addr: std::mem::offset_of!(Cpu, excl_addr),
        off_excl_val: std::mem::offset_of!(Cpu, excl_val),
        off_v: std::mem::offset_of!(Cpu, v),
        off_fpsr: std::mem::offset_of!(Cpu, fpsr),
    }
}

struct Emit<'a> { out: Vec<u32>, c: &'a Ctx, sp_slot: usize, pending_native: u64, /** block runs native FP/SIMD: carry FPSR */ fpsr: bool,
                  /** chain to the next block: its key's mode bits (bits 48.. of `key`), None = plain return */ chain: Option<u32>,
                  /** virtual page of the block (targets there can be linked directly) */ page: u64,
                  /** patchable direct branches: (word index, target pc) */ links: Vec<(usize, u64)> }

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
    /// guest NZCV and FPSR live in the host registers inside a block
    fn save_flags(&mut self) {
        self.out.push(MRS_NZCV_X9);
        self.out.push(str_w(9, X_CPU, self.c.off_nzcv));
        if self.fpsr {
            self.out.push(0xd53b_4429); // mrs x9, fpsr
            self.out.push(str_x(9, X_CPU, self.c.off_fpsr));
        }
    }
    fn load_flags(&mut self) {
        self.out.push(ldr_w(9, X_CPU, self.c.off_nzcv));
        self.out.push(MSR_NZCV_X9);
        if self.fpsr {
            self.out.push(ldr_x(9, X_CPU, self.c.off_fpsr));
            self.out.push(0xd51b_4429); // msr fpsr, x9
        }
    }
    /// epilogue: return `status` in w0
    fn ret(&mut self, status: u32) {
        if let (0, Some(kb)) = (status, self.chain) {
            // continue at cpu.pc: the dispatcher (4 instructions, like the plain return)
            self.out.push(0xd2e0_0000 | (kb << 5) | 14); // movz x14, #kb, lsl #48
            self.out.push(ldr_x(16, X_CPU, self.c.off_disp));
            self.out.push(0xd61f_0200); // br x16
            self.out.push(0xd503_201f); // nop
            return;
        }
        self.out.push(0x5280_0000 | (status << 5)); // movz w0, #status
        self.out.push(0xa941_53f3); // ldp x19, x20, [sp, #16]
        self.out.push(0xa8c2_7bfd); // ldp x29, x30, [sp], #32
        self.out.push(0xd65f_03c0); // ret
    }
    /// Leave the block for constant `target` (instret flushed, flags saved). A target on the
    /// same page gets a patchable direct branch into its block, taken while the budget lasts.
    fn exit_const(&mut self, target: u64) {
        if self.chain.is_some() && target >> 12 == self.page {
            self.out.push(ldr_x(9, X_CPU, self.c.off_instret));
            self.out.push(ldr_x(10, X_CPU, self.c.off_chain_until));
            self.out.push(0xeb0a_013f); // cmp x9, x10
            self.out.push(0x5400_0042); // b.hs +2
            self.links.push((self.out.len(), target));
            self.out.push(0x1400_0001); // b +1, patched to the target block's body
        }
        self.set_pc_const(target);
        self.ret(0);
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
        // physical address of pc through the fetch micro-TLB, filled here on a miss (a
        // translation fault is left to the interpreter, which takes it)
        let tag = ((self.pc >> 12) + 1) | ((self.el as u64) << 60);
        let ti = (self.pc >> 12) as usize & (UT - 1);
        if self.utlb[0][ti].0 != tag {
            let pa = self.translate(bus, self.pc, crate::mmu::Access::Exec).ok()?;
            if self.ram.is_null() || (pa | 0xfff) >= self.ram_len { return None; }
            self.utlb[0][ti] = (tag, pa & !0xfff);
        }
        let pa = self.utlb[0][ti].1 | (self.pc & 0xfff);
        let k = (self.pc, key(pa, self.el, self.spsel, (self.sys.cpacr_el1 >> 20) & 3));
        let jit = self.jit.as_mut()?;
        let slot = ((self.pc ^ (self.pc >> 14)) as usize >> 2) & (FRONT - 1);
        let f: BlockFn = if jit.front[slot].0 == k.0 && jit.front[slot].1 == k.1 {
            // SAFETY: front entries are only filled from `blocks`, which hold published code
            unsafe { std::mem::transmute::<usize, BlockFn>(jit.front[slot].2) }
        } else {
            // a block compiled before (evicted from the front cache by another pc) runs at once:
            // one hash lookup instead of interpreting it until its counter is hot again (the
            // counters are direct-mapped too, and a large working set overwrites them)
            if let Some(&f) = jit.blocks.get(&k) {
                jit.front[slot] = (k.0, k.1, f as usize);
                return self.jit_run(f, pa, k, bus);
            }
            // count executions in a direct-mapped table (no allocation); a hot entry compiles
            let hs = ((self.pc ^ (self.pc >> 16)) as usize >> 2) & (HOTC - 1);
            let h = &mut jit.hotc[hs];
            if h.0 == k.0 && h.1 == k.1 { h.2 = h.2.saturating_add(1); } else { *h = (k.0, k.1, 1); }
            if h.2 < jit.hot {
                // cold: the interpreter runs up to the end of this block (block starts alone
                // count, so no overlapping blocks get compiled from mid-block pcs)
                self.interp_run = true;
                return None;
            }
            let f = self.compile(pa)?;
            let jit = self.jit.as_mut()?;
            jit.blocks.insert(k, f);
            jit.pages.entry(pa >> 12).or_default().push(k);
            if watch_page() == Some(pa >> 12) { eprintln!("[jit-watch] compiled pc {:#x} pa {pa:#x} instret {}", self.pc, self.instret); }
            jit.front[slot] = (k.0, k.1, f as usize);
            jit.compiled += 1;
            let hs = ((k.0 ^ (k.0 >> 16)) as usize >> 2) & (HOTC - 1);
            jit.hotc[hs] = (k.0, k.1, u32::MAX);
            jit.link(k, f as usize);
            if jit.verify { let h = code_hash(self, pa); if let Some(j) = self.jit.as_mut() { j.code_hash.insert(k, h); } }
            f
        };
        self.jit_run(f, pa, k, bus)
    }

    /// Run compiled block `f` (and, in the wasm build, the blocks it chains to).
    #[inline(always)]
    fn jit_run(&mut self, f: BlockFn, pa: u64, k: (u64, u64), bus: &mut dyn Bus) -> Option<Result<(), Trap>> {
        if self.jit.as_ref().is_some_and(|j| j.verify) { return Some(self.jit_verify_block(f, pa, k, bus)); }
        // SAFETY: the bus outlives the call; generated code only reaches it through h_step
        self.jit_bus = Some(unsafe { std::mem::transmute::<&mut dyn Bus, &'static mut dyn Bus>(bus) } as *mut dyn Bus);
        let i0 = self.instret;
        self.chain_stop = false;
        if !chain_enabled() { self.chain_until = 0; }
        // native FP in compiled code runs under the guest's FPCR (rounding, FZ, DN, AHP); the
        // host's default (all zero) is restored afterwards
        #[cfg(target_arch = "aarch64")]
        let fpcr = self.fpcr & FPCR_HOST_MASK;
        #[cfg(target_arch = "aarch64")]
        if fpcr != 0 { unsafe { set_host_fpcr(fpcr) }; }
        #[allow(unused_mut)]
        let mut r = unsafe { f(self as *mut Cpu) };
        #[cfg(target_arch = "aarch64")]
        if fpcr != 0 { unsafe { set_host_fpcr(0) }; }
        // wasm: chain in Rust (native code chains through its own dispatcher): the next block
        // from the front cache while the budget lasts and nothing needs the machine
        #[cfg(target_arch = "wasm32")]
        while r == 0 && !self.chain_stop && self.instret < self.chain_until {
            let Some(g) = self.front_block() else { break };
            r = unsafe { g(self as *mut Cpu) };
        }
        self.jit_bus = None;
        if let Some(j) = self.jit.as_mut() { j.execs += 1; j.exec_insns += self.instret - i0; }
        Some(if r == 0 { Ok(()) } else { Err(self.jit_trap.take().unwrap_or(Trap::Wait)) })
    }

    /// The compiled block for the current pc if the front cache has it.
    #[cfg(target_arch = "wasm32")]
    #[inline(always)]
    fn front_block(&self) -> Option<BlockFn> {
        if self.pc & 3 != 0 { return None; }
        let e = self.utlb[0][(self.pc >> 12) as usize & (UT - 1)];
        if e.0 != ((self.pc >> 12) + 1) | ((self.el as u64) << 60) { return None; }
        let kk = key(e.1 | (self.pc & 0xfff), self.el, self.spsel, (self.sys.cpacr_el1 >> 20) & 3);
        let jit = self.jit.as_ref()?;
        let fe = jit.front[((self.pc ^ (self.pc >> 14)) as usize >> 2) & (FRONT - 1)];
        // SAFETY: front entries are only filled from `blocks`, which hold published code
        (fe.0 == self.pc && fe.1 == kk).then(|| unsafe { std::mem::transmute::<usize, BlockFn>(fe.2) })
    }

    /// JIT verify mode: run block `f`, then re-run the same instructions in the interpreter from
    /// the same state (stores rolled back) and record the first mismatch. Kept out of `jit_step`.
    #[cold]
    #[inline(never)]
    fn jit_verify_block(&mut self, f: BlockFn, pa: u64, k: (u64, u64), bus: &mut dyn Bus) -> Result<(), Trap> {
        let mut verify = self.jit.as_ref().is_some_and(|j| j.verify && j.report.is_none());
        if verify && self.jit.as_ref().and_then(|j| j.code_hash.get(&k)) != Some(&code_hash(self, pa)) {
            // the code changed after compilation (no IC yet): running the old code is legal
            verify = false;
            if let Some(j) = self.jit.as_mut() { j.stale_skips += 1; }
        }
        let before = verify.then(|| (self.x, self.sp, self.pc, self.nzcv, self.instret, self.v, self.excl(), (self.el, self.spsel, self.daif), (self.sys.vtimer, self.sys.ptimer, self.sys.timers_changed)));
        if verify { self.store_log = Some(Vec::new()); }
        // SAFETY: the bus outlives the call; generated code only reaches it through h_step
        self.jit_bus = Some(unsafe { std::mem::transmute::<&mut dyn Bus, &'static mut dyn Bus>(bus) } as *mut dyn Bus);
        #[cfg(target_arch = "aarch64")]
        let fpcr = self.fpcr & FPCR_HOST_MASK;
        #[cfg(target_arch = "aarch64")]
        if fpcr != 0 { unsafe { set_host_fpcr(fpcr) }; }
        let r = unsafe { f(self as *mut Cpu) };
        #[cfg(target_arch = "aarch64")]
        if fpcr != 0 { unsafe { set_host_fpcr(0) }; }
        self.jit_bus = None;
        let log = self.store_log.take();
        if let (Some(b), 0, Some(log)) = (before, r, log) {
            if log.iter().any(|e| e.0 == usize::MAX) { return Ok(()); } // a store we cannot undo
            // undo the block's stores (newest first), keeping what it wrote for the comparison
            let wrote: Vec<(usize, Vec<u8>)> = log.iter().map(|(off, old)| {
                // SAFETY: logged offsets are inside guest RAM
                (*off, unsafe { std::slice::from_raw_parts(self.ram.add(*off), old.len()) }.to_vec())
            }).collect();
            for (off, old) in log.iter().rev() { unsafe { std::ptr::copy_nonoverlapping(old.as_ptr(), self.ram.add(*off), old.len()) }; }
            // re-run the same instructions in the interpreter from the same registers
            let after = (self.x, self.sp, self.pc, self.nzcv, self.instret, self.v, self.excl(), (self.el, self.spsel, self.daif));
            // the timers are restored too (MSR to them may end the block); compared via the registers
            (self.sys.vtimer, self.sys.ptimer, self.sys.timers_changed) = b.8;
            let n = after.4 - b.4;
            (self.x, self.sp, self.pc, self.nzcv, self.instret, self.v) = (b.0, b.1, b.2, b.3, b.4, b.5);
            self.set_excl(b.6);
            (self.el, self.spsel, self.daif) = b.7;
            let start_pc = self.pc;
            for _ in 0..n { if self.exec_one(bus).is_err() { break; } }
            let mut diffs = Vec::new();
            for k in 0..31 { if self.x[k] != after.0[k] { diffs.push(format!("x{k}: jit {:#x} interp {:#x}", after.0[k], self.x[k])); } }
            for k in 0..4 { if self.sp[k] != after.1[k] { diffs.push(format!("sp{k}: jit {:#x} interp {:#x}", after.1[k], self.sp[k])); } }
            if self.pc != after.2 { diffs.push(format!("pc: jit {:#x} interp {:#x}", after.2, self.pc)); }
            if (self.el, self.spsel, self.daif) != after.7 { diffs.push(format!("el/spsel/daif: jit {:?} interp {:?}", after.7, (self.el, self.spsel, self.daif))); }
            if self.nzcv != after.3 { diffs.push(format!("nzcv: jit {:#x} interp {:#x}", after.3, self.nzcv)); }
            for k in 0..32 { if self.v[k] != after.5[k] { diffs.push(format!("v{k} differs")); } }
            for (off, bytes) in &wrote {
                // SAFETY: inside guest RAM
                let now = unsafe { std::slice::from_raw_parts(self.ram.add(*off), bytes.len()) };
                if now != &bytes[..] { diffs.push(format!("memory at RAM+{off:#x}: jit {bytes:02x?} interp {now:02x?}")); }
            }
            if !diffs.is_empty() {
                let words: Vec<String> = (0..n.min(64)).map(|k| {
                    let a = (pa + 4 * k) as usize;
                    // SAFETY: inside guest RAM (the block was compiled from it)
                    format!("{:08x}", unsafe { (self.ram.add(a) as *const u32).read_unaligned() })
                }).collect();
                let rep = format!("block at pc {start_pc:#x} (pa {pa:#x}), {n} instructions: {}\n  {}", words.join(" "), diffs.join("\n  "));
                if let Some(j) = self.jit.as_mut() { j.report = Some(rep); }
            }
            return Ok(());
        }
        if r == 0 { Ok(()) } else { Err(self.jit_trap.take().unwrap_or(Trap::Wait)) }
    }

    #[inline(never)]
    fn compile(&mut self, pa: u64) -> Option<BlockFn> {
        self.mark_code_page(pa >> 12);
        #[cfg(target_arch = "wasm32")]
        {
            // browser: one small wasm module per block; keep the live set bounded
            if self.jit.as_ref().is_some_and(|j| j.blocks.len() >= WASM_MAX_BLOCKS) { self.jit.as_mut()?.flush(); }
            return self.compile_wasm(pa);
        }
        #[allow(unreachable_code)]
        let c = ctx();
        let sp_slot = c.off_sp + 8 * if self.spsel { self.el as usize } else { 0 };
        // does this block (up to the page end or the length limit) contain native FP/SIMD?
        let mut fpsr = false;
        if !self.ram.is_null() {
            let mut a2 = pa;
            for _ in 0..MAX_INSNS {
                if a2 + 4 > self.ram_len { break; }
                // SAFETY: inside guest RAM (checked)
                let w = unsafe { (self.ram.add(a2 as usize) as *const u32).read_unaligned() };
                if simd_operands(w).is_some() { fpsr = true; break; }
                a2 += 4;
                if a2 & 0xfff == 0 { break; }
            }
        }
        let chain = self.jit.as_ref().is_some_and(|j| !j.verify).then(|| (key(0, self.el, self.spsel, (self.sys.cpacr_el1 >> 20) & 3) >> 48) as u32);
        let mut e = Emit { out: Vec::with_capacity(512), c: &c, sp_slot, pending_native: 0, fpsr, chain, page: self.pc >> 12, links: Vec::new() };
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
        let mask: u32 = std::env::var("SBC_JIT_NATIVE").ok().and_then(|v| v.parse().ok()).unwrap_or(15);
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
            } else if (i & 0xffff_f01f == 0xd503_201f && !matches!((i >> 5) & 0x7f, 2 | 3))
                || (i & 0xffff_f01f == 0xd503_301f && matches!((i >> 5) & 7, 4..=6))
            {
                // NOP and the hints that are NOPs here (YIELD, SEV, PAC, BTI, ...), DSB/DMB/ISB;
                // with cores running in parallel DSB/DMB order the host's accesses too
                if self.mt && i & 0xffff_f01f == 0xd503_301f && matches!((i >> 5) & 7, 4 | 5) { e.out.push(0xd503_3bbf); } // dmb ish
                natives += 1;
                e.pending_native += 1;
            } else if i & 0xffc0_0000 == 0xf980_0000 || i & 0xffe0_0c00 == 0xf880_0000 || i & 0xffe0_0c00 == 0xf8a0_0800 {
                // PRFM / PRFUM: no caches are modelled
                natives += 1;
                e.pending_native += 1;
            } else if let Some(n) = match i & 0xffff_ffe0 { 0xd53b_d040 => Some(0), 0xd53b_d060 => Some(1), 0xd538_d080 if self.el >= 1 => Some(2), _ => None } {
                // MRS Xt, TPIDR_EL0 / TPIDRRO_EL0 / TPIDR_EL1 (thread pointers)
                let rt = i & 31;
                if rt != 31 {
                    e.out.push(ldr_x(T[0], X_CPU, c.off_tpidr + 8 * n));
                    e.out.push(str_x(T[0], X_CPU, e.reg_off(rt)));
                }
                natives += 1;
                e.pending_native += 1;
            } else if i & 0xffff_ffe0 == 0xd538_4100 && self.el >= 1 {
                // MRS Xt, SP_EL0 (Linux keeps `current` there)
                let rt = i & 31;
                if rt != 31 {
                    e.out.push(ldr_x(T[0], X_CPU, c.off_sp));
                    e.out.push(str_x(T[0], X_CPU, e.reg_off(rt)));
                }
                natives += 1;
                e.pending_native += 1;
            } else if matches!(i & 0xffff_fc00, 0x9e66_0000 | 0x1e26_0000 | 0x9e67_0000 | 0x1e27_0000)
                && (self.sys.cpacr_el1 >> 20) & 3 == 3 && v_off(&c, 31) + 16 < if i >> 31 != 0 { 32768 } else { 16384 } && e.reg_off(31) < 16384 {
                // FMOV between the register files (64-bit X<->D, 32-bit W<->S): a plain move
                let (rd, rn, x) = (i & 31, (i >> 5) & 31, i >> 31 != 0);
                if i & 0x0001_0000 == 0 {
                    // to a general register (zero-extended for W)
                    if rd != 31 {
                        e.out.push(if x { ldr_x(T[0], X_CPU, v_off(&c, rn) as usize) } else { ldr_w(T[0], X_CPU, v_off(&c, rn) as usize) });
                        e.out.push(str_x(T[0], X_CPU, e.reg_off(rd)));
                    }
                } else {
                    // to a SIMD&FP register: the rest of the vector is zeroed
                    if rn == 31 { e.out.push(0xaa1f_03e0 | T[0]); } else if x { e.out.push(ldr_x(T[0], X_CPU, e.reg_off(rn))); } else { e.out.push(ldr_w(T[0], X_CPU, e.reg_off(rn))); }
                    e.out.push(str_x(T[0], X_CPU, v_off(&c, rd) as usize));
                    e.out.push(str_x(31, X_CPU, v_off(&c, rd) as usize + 8));
                }
                natives += 1;
                e.pending_native += 1;
            } else if let Some(done) = if mask & 2 != 0 { self.emit_branch(&mut e, i, pc) } else { None } {
                if done { natives += 1; ended = true; break; }
            } else if let Some((enc, map, writes)) = simd_operands(i)
                .filter(|_| mask & 8 != 0 && (self.sys.cpacr_el1 >> 20) & 3 == 3 && self.jit.as_mut().is_some_and(|j| j.interp_supports(i)))
                .and_then(|(ops, writes)| map_simd(i, &ops).map(|(enc, map)| (enc, map, writes)))
            {
                // FP/SIMD natively: guest V registers in host v0..v7 (caller-saved)
                for &(g, h) in &map { e.out.push(0x3dc0_0000 | ((v_off(&c, g) / 16) << 10) | (X_CPU << 5) | h); } // ldr q
                e.out.push(enc);
                if writes {
                    let rd = i & 31;
                    let h = map.iter().find(|m| m.0 == rd).map(|m| m.1)?;
                    e.out.push(0x3d80_0000 | ((v_off(&c, rd) / 16) << 10) | (X_CPU << 5) | h); // str q
                }
                natives += 1;
                e.pending_native += 1;
            } else if (i & 0xbfe0_fc00) == 0x0e00_0c00 && (i >> 16) & 0xf != 0 && !((i >> 16) & 0xf == 8 && i & (1 << 30) == 0)
                && (self.sys.cpacr_el1 >> 20) & 3 == 3 && v_off(&c, 31) < 65536 {
                // DUP Vd.<T>, Wn/Xn: the host's own DUP from a scratch register into v0
                let (rd, rn) = (i & 31, (i >> 5) & 31);
                if rn == 31 { e.out.push(0xaa1f_03e0 | T[0]); } else { e.out.push(ldr_x(T[0], X_CPU, e.reg_off(rn))); }
                e.out.push((i & !0x3ff) | (T[0] << 5)); // dup v0.<T>, w9/x9
                e.out.push(0x3d80_0000 | ((v_off(&c, rd) / 16) << 10) | (X_CPU << 5)); // str q0, [cpu.v[rd]]
                natives += 1;
                e.pending_native += 1;
            } else if ld1_as_ldr(i).is_some_and(|w| self.emit_ldst(&mut e, w, pc)) {
                // LD1/ST1 of one register: the same access as LDR/STR of a Q or D register
            } else if self.emit_excl(&mut e, i, pc) {
                // exclusive and acquire/release accesses, CLREX
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
            e.exit_const(pc);
        }
        if e.chain.is_some() && self.jit.as_ref()?.disp.is_none() {
            let words = dispatcher(&c, self.jit.as_ref()?.front.as_ptr() as u64);
            match self.jit.as_mut()?.code.write(&words) {
                Some(d) => self.jit.as_mut()?.disp = Some(d as u64),
                None => { let j = self.jit.as_mut()?; j.full_flushes += 1; j.flush(); return None; }
            }
        }
        if let Some(d) = self.jit.as_ref()?.disp { self.jit_disp = d; }
        let f = self.jit.as_mut()?.code.write(&e.out);
        let f = match f {
            Some(p) => p,
            None => { let j = self.jit.as_mut()?; j.full_flushes += 1; j.flush(); return None; }
        };
        let sites: Vec<(usize, u64)> = e.links.iter().map(|&(ix, t)| (f as usize + 4 * ix, t)).collect();
        self.jit.as_mut()?.new_sites = sites;
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
            e.exit_const(target);
            return Some(true);
        }
        // B.cond: select with the guest flags still in the host NZCV
        if bits(i, 31, 24) == 0b0101_0100 && !bit(i, 4) {
            let target = pc.wrapping_add(sx((bits(i, 23, 5) as u64) << 2, 21));
            e.pending_native += 1;
            e.flush_instret();
            e.save_flags(); // keeps the host NZCV
            let at = e.out.len();
            e.out.push(0x5400_0000 | bits(i, 3, 0)); // b.cond taken (patched below)
            e.exit_const(pc + 4);
            let taken = e.out.len();
            e.out[at] |= (((taken - at) as u32) & 0x7ffff) << 5;
            e.exit_const(target);
            return Some(true);
        }
        // CBZ/CBNZ, TBZ/TBNZ: test with a host cbz/tbz (flags untouched)
        let cb = bits(i, 30, 25) == 0b011010;
        let tb = bits(i, 30, 25) == 0b011011;
        if cb || tb {
            let rt = i & 31;
            let target = if cb { pc.wrapping_add(sx((bits(i, 23, 5) as u64) << 2, 21)) } else { pc.wrapping_add(sx((bits(i, 18, 5) as u64) << 2, 16)) };
            if rt == 31 { e.out.push(0xaa1f_03e0 | T[2]); } else { e.out.push(ldr_x(T[2], X_CPU, e.reg_off(rt))); }
            e.pending_native += 1;
            e.flush_instret(); // x13, x20: x11 survives
            e.save_flags(); // x9
            // the host branch (same test on x11) jumps to the taken exit
            let at = e.out.len();
            e.out.push(if cb { i & !(31 | (0x7ffff << 5)) } else { i & !(31 | (0x3fff << 5)) } | T[2]);
            e.exit_const(pc + 4);
            let off = (e.out.len() - at) as u32;
            e.out[at] |= if cb { (off & 0x7ffff) << 5 } else { (off & 0x3fff) << 5 };
            e.exit_const(target);
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
        if i & 0xffff_ffe0 == 0xd50b_7420 { return self.emit_dc_zva(e, i, pc); }
        // ---- decode into (base, address computation, size, kind, writeback, pair)
        enum Addr { Imm(i64), Reg(u32, u32, u32) } // Reg(rm, option, shift)
        let rt = i & 31;
        let rn = (i >> 5) & 31;
        let (size, load, signed_to, pair, rt2, addr, mode): (u32, bool, u32, bool, u32, Addr, u8);
        // LDTR/STTR (and sign-extending forms): EL1's unprivileged access, as EL0 would make it
        let mut unpriv = false;
        // mode: 0 = offset, 1 = post-index, 2 = pre-index
        // SIMD&FP registers (B/H/S/D/Q, LDP/STP S/D/Q) need FP enabled at this EL
        let vec = bit(i, 26);
        if vec && (self.sys.cpacr_el1 >> 20) & 3 != 3 { return false; }
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
            // LDP/STP, and LDNP/STNP (bits 24:23 = 00: an offset access with a cache hint)
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
                    _ => { if self.el > 1 { return false; } unpriv = self.el == 1; addr = Addr::Imm(imm); mode = 0; }
                }
            }
        } else if bits(i, 29, 27) == 0b101 && !bit(i, 26) && !bit(i, 25) {
            let opc = bits(i, 31, 30);
            let ld = bit(i, 22);
            if bits(i, 24, 23) == 0 && opc == 1 { return false; } // no LDNPSW
            let (sz, st) = match (opc, ld) { (0, _) => (2, 0), (1, true) => (2, 64), (2, _) => (3, 0), _ => return false };
            size = 1 << sz; load = ld; signed_to = st; pair = true; rt2 = bits(i, 14, 10);
            let imm = (((bits(i, 21, 15) as i64) << 57) >> 57) << sz;
            addr = Addr::Imm(imm);
            mode = match bits(i, 24, 23) { 1 => 1, 3 => 2, _ => 0 };
            if load && rt == rt2 { return false; }
        } else {
            return false;
        }
        // write-back onto a loaded/stored register (the vector forms name V registers: no clash)
        if mode != 0 && rn != 31 && !vec && (rn == rt || (pair && rn == rt2)) { return false; }
        if unpriv && c_off_ok(e.c.off_utlb_unpriv).is_none() { return false; }
        if !load && self.jit.as_ref().is_some_and(|j| j.verify) { return false; } // verify mode logs stores in the interpreter
        let span = if pair { 2 * size } else { size };
        let c = e.c;
        let (vreg, tmp) = (13u32, 14u32);
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
        // ---- micro-TLB check: x14 = host address
        let mut slow: Vec<usize> = Vec::new();
        self.emit_utlb(e, if load { 1 } else { 2 }, span, unpriv, !load, &mut slow);
        // ---- the access itself, as the host instruction of the same size and extension
        let sz = size.trailing_zeros();
        let one = |rt: u32, off: u32| -> u32 {
            // LDR/STR (unsigned offset) Rt, [x14, #off]
            let opc = if !load { 0 } else { match signed_to { 0 => 1, 64 => 2, _ => 3 } };
            0x3900_0000 | (sz << 30) | (opc << 22) | ((off >> sz) << 10) | (tmp << 5) | rt
        };
        let regs = if pair { vec![(rt, 0u32, 9u32), (rt2, size, 10u32)] } else { vec![(rt, 0, 9)] };
        let ldq = |v: u32, r: u32| 0x3dc0_0000 | ((v_off(c, r) / 16) << 10) | (X_CPU << 5) | v; // ldr qV, [cpu.v[r]]
        let stq = |v: u32, r: u32| 0x3d80_0000 | ((v_off(c, r) / 16) << 10) | (X_CPU << 5) | v; // str qV, [cpu.v[r]]
        if vec {
            // through host v0/v1 (free between instructions); scalar loads zero the upper bits
            let opc_q = if sz == 4 { 2 } else { 0 };
            let s_bits = if sz == 4 { 0 } else { sz };
            if load {
                if pair { e.out.push([0x2d40_0000u32, 0x6d40_0000, 0xad40_0000][sz as usize - 2] | (1 << 10) | (tmp << 5)); } // ldp v0, v1, [x14]
                else { e.out.push(0x3d40_0000 | (s_bits << 30) | (opc_q << 22) | (tmp << 5)); } // ldr b/h/s/d/q0, [x14]
                e.out.push(stq(0, rt));
                if pair { e.out.push(stq(1, rt2)); }
            } else {
                e.out.push(ldq(0, rt));
                if pair { e.out.push(ldq(1, rt2)); }
                if pair { e.out.push([0x2d00_0000u32, 0x6d00_0000, 0xad00_0000][sz as usize - 2] | (1 << 10) | (tmp << 5)); } // stp v0, v1, [x14]
                else { e.out.push(0x3d00_0000 | (s_bits << 30) | (opc_q << 22) | (tmp << 5)); } // str b/h/s/d/q0, [x14]
            }
        } else if load {
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

impl Cpu {
    /// The micro-TLB lookup of generated loads and stores (no flags touched). With the VA in
    /// x13, leaves x14 = host address and x15 = the page offset, or branches to a slow path
    /// (pushed to `slow`, patched by the caller) on a miss. `k`: 1 = load, 2 = store entries;
    /// `span` > 0: bytes accessed (the slow path takes page-crossing accesses); `excl_check`:
    /// a held reservation takes the slow path (the interpreter's store clears it). Uses x17.
    fn emit_utlb(&self, e: &mut Emit, k: usize, span: u32, unpriv: bool, excl_check: bool, slow: &mut Vec<usize>) {
        let c = e.c;
        let (vreg, tmp, tmp2) = (13u32, 14u32, 15u32);
        // tag = (va >> 12) + 1 | el << 60; x17 = &utlb[k][(va >> 12) % UT]
        let ent = c.off_utlb + 16 * UT * k;
        assert!(ent + 16 * UT < 32768);
        e.out.push(0xd34c_fc00 | (vreg << 5) | tmp); // lsr x14, x13, #12
        e.out.push(0x9240_0000 | ((UT.trailing_zeros() - 1) << 10) | (tmp << 5) | 17); // and x17, x14, #(UT-1)
        e.out.push(0x8b00_0000 | (17 << 16) | (4 << 10) | (X_CPU << 5) | 17); // add x17, x19, x17, lsl #4
        e.out.push(0x9100_0400 | (tmp << 5) | tmp); // add x14, x14, #1
        if unpriv {
            // the EL0 entry, poisoned (bit 59) while they belong to another ASID
            e.out.push(ldr_x(tmp2, X_CPU, c.off_utlb_unpriv));
            e.out.push(0xaa00_0000 | (tmp2 << 16) | (tmp << 5) | tmp);
        } else if self.el != 0 { mov64(&mut e.out, tmp2, (self.el as u64) << 60); e.out.push(0xaa00_0000 | (tmp2 << 16) | (tmp << 5) | tmp); }
        e.out.push(ldr_x(tmp2, 17, ent));
        e.out.push(0xcb00_0000 | (tmp2 << 16) | (tmp << 5) | tmp); // sub x14, x14, x15
        slow.push(e.out.len()); e.out.push(0xb500_0000 | tmp); // cbnz x14, slow
        e.out.push(0x9240_2c00 | (vreg << 5) | tmp2); // and x15, x13, #0xfff
        if span > 0 {
            e.out.push(0x9100_0000 | ((span - 1) << 10) | (tmp2 << 5) | tmp); // add x14, x15, #span-1
            slow.push(e.out.len()); e.out.push(0x3760_0000 | tmp); // tbnz w14, #12, slow
        }
        if excl_check {
            e.out.push(ldr_x(tmp, X_CPU, c.off_excl));
            slow.push(e.out.len()); e.out.push(0xb500_0000 | tmp); // cbnz x14, slow (reservation held)
        }
        e.out.push(ldr_x(tmp, 17, ent + 8)); // physical page
        e.out.push(0x8b00_0000 | (tmp2 << 16) | (tmp << 5) | tmp); // add x14, x14, x15
        e.out.push(ldr_x(tmp2, X_CPU, c.off_ram));
        e.out.push(0x8b00_0000 | (tmp2 << 16) | (tmp << 5) | tmp); // x14 = host address
    }

    /// LDXR/LDAXR, STXR/STLXR (8- to 64-bit), LDAR/STLR and CLREX natively: the reservation
    /// lives in `excl_addr`/`excl_armed`/`excl_val` as the interpreter keeps it. With cores
    /// running in parallel (`mt`) LDAR/STLR are the host's own and store-exclusive is a host
    /// compare-and-swap loop (LDAXR/STLXR, so ARMv8.0 hosts work) against what LDXR read.
    /// Misses, page crossings and pairs go to the interpreter.
    fn emit_excl(&self, e: &mut Emit, i: u32, pc: u64) -> bool {
        if self.ram.is_null() { return false; }
        let c = e.c;
        if [c.off_excl, c.off_excl_addr, c.off_excl_val].iter().any(|&o| c_off_ok(o).is_none()) { return false; }
        if i & 0xffff_f0ff == 0xd503_305f {
            // CLREX
            e.out.push(str_x(31, X_CPU, c.off_excl));
            e.pending_native += 1;
            return true;
        }
        // load/store exclusive and ordered: size 001000 o2 L o1 Rs o0 Rt2 Rn Rt
        if bits(i, 29, 24) != 0b001000 || bit(i, 21) || bits(i, 14, 10) != 31 { return false; }
        let (sz, o2, load, rs, o0, rn, rt) = (bits(i, 31, 30), bit(i, 23), bit(i, 22), bits(i, 20, 16), bit(i, 15), (i >> 5) & 31, i & 31);
        let size = 1u32 << sz;
        let verify = self.jit.as_ref().is_some_and(|j| j.verify);
        if o2 && (!o0 || rs != 31) { return false; } // LDLAR/STLLR (LORegions): the interpreter
        if !o2 && load && rs != 31 { return false; }
        if !load && verify { return false; } // verify mode logs stores in the interpreter
        let (vreg, tmp) = (13u32, 14u32);
        e.count_pending();
        e.out.push(if rn == 31 { ldr_x(vreg, X_CPU, e.sp_slot) } else { ldr_x(vreg, X_CPU, e.reg_off(rn)) });
        let mut slow: Vec<usize> = Vec::new();
        let mut fail: Vec<usize> = Vec::new(); // store-exclusive without the reservation
        let ldr = |rt: u32| 0x3940_0000 | (sz << 30) | (tmp << 5) | rt; // ldr{b,h,,} Rt, [x14]
        let str_ = |rt: u32| 0x3900_0000 | (sz << 30) | (tmp << 5) | rt; // str{b,h,,} Rt, [x14]
        let ldar = |rt: u32| 0x08df_fc00 | (sz << 30) | (tmp << 5) | rt;
        let stlr = |rt: u32| 0x089f_fc00 | (sz << 30) | (tmp << 5) | rt;
        if !load && !o2 {
            // the reservation must cover this 64-byte granule
            e.out.push(ldr_x(tmp, X_CPU, c.off_excl));
            fail.push(e.out.len()); e.out.push(0xb400_0000 | tmp); // cbz x14, fail
            e.out.push(ldr_x(tmp, X_CPU, c.off_excl_addr));
            e.out.push(0xca00_0000 | (vreg << 16) | (tmp << 5) | tmp); // eor x14, x14, x13
            e.out.push(0xd346_fc00 | (tmp << 5) | tmp); // lsr x14, x14, #6
            fail.push(e.out.len()); e.out.push(0xb500_0000 | tmp); // cbnz x14, fail
        }
        self.emit_utlb(e, if load { 1 } else { 2 }, size, false, !load && o2, &mut slow);
        if self.mt && !load && !o2 && sz > 0 {
            // the host's exclusives need natural alignment (the interpreter stores plainly)
            e.out.push(0x9240_0000 | ((sz - 1) << 10) | (vreg << 5) | 16); // and x16, x13, #(size-1)
            slow.push(e.out.len()); e.out.push(0xb500_0000 | 16); // cbnz x16, slow
        }
        let acquire = self.mt && (o2 || bit(i, 15));
        if load {
            e.out.push(if o2 || acquire { ldar(9) } else { ldr(9) });
            if rt != 31 { e.out.push(str_x(9, X_CPU, e.reg_off(rt))); }
            if !o2 {
                e.out.push(str_x(vreg, X_CPU, c.off_excl_addr));
                e.out.push(0xd280_0020 | tmp); // movz x14, #1
                e.out.push(str_x(tmp, X_CPU, c.off_excl));
                e.out.push(str_x(9, X_CPU, c.off_excl_val));
                e.out.push(str_x(31, X_CPU, c.off_excl_val + 8));
            }
        } else {
            e.out.push(if rt == 31 { 0xaa1f_03e0 | 9 } else { ldr_x(9, X_CPU, e.reg_off(rt)) });
            if o2 {
                e.out.push(if self.mt { stlr(9) } else { str_(9) });
            } else if self.mt {
                // retry: ldaxr x11, [x14]; eor x12, x11, x10; cbnz x12, lost; stlxr w12, x9, [x14]; cbnz w12, retry
                e.out.push(ldr_x(10, X_CPU, c.off_excl_val));
                let retry = e.out.len();
                e.out.push(0x085f_fc00 | (sz << 30) | (tmp << 5) | 11);
                e.out.push(0xca00_0000 | (10 << 16) | (11 << 5) | 12);
                let lost = e.out.len(); e.out.push(0xb500_0000 | 12);
                e.out.push(0x0800_fc00 | (sz << 30) | (12 << 16) | (tmp << 5) | 9);
                e.out.push(0x3500_0000 | ((((retry as i64 - e.out.len() as i64) as u32) & 0x7ffff) << 5) | 12);
                e.out.push(0x1400_0003); // b +3 (stored)
                let at = e.out.len();
                e.out[lost] |= (((at - lost) as u32) & 0x7ffff) << 5;
                e.out.push(0xd503_3f5f); // clrex
                fail.push(e.out.len()); e.out.push(0x1400_0000); // b fail (memory changed)
            } else {
                e.out.push(str_(9));
            }
            if !o2 {
                // stored: the reservation is used up, status 0
                e.out.push(str_x(31, X_CPU, c.off_excl));
                if rs != 31 { e.out.push(str_x(31, X_CPU, e.reg_off(rs))); }
            }
        }
        e.out.push(0x9100_0400 | (X_COUNT << 5) | X_COUNT); // add x20, x20, #1
        let mut done_at = vec![e.out.len()];
        e.out.push(0x1400_0000); // b done (patched)
        if !fail.is_empty() {
            // store-exclusive failed: drop the reservation, status 1, nothing stored
            let at = e.out.len();
            for b in fail.drain(..) {
                let off = (at - b) as u32;
                e.out[b] |= if e.out[b] & 0x7c00_0000 == 0x1400_0000 { off & 0x3ff_ffff } else { (off & 0x7ffff) << 5 };
            }
            e.out.push(str_x(31, X_CPU, c.off_excl));
            if rs != 31 { e.out.push(0xd280_0020 | 9); e.out.push(str_x(9, X_CPU, e.reg_off(rs))); }
            e.out.push(0x9100_0400 | (X_COUNT << 5) | X_COUNT); // add x20, x20, #1
            done_at.push(e.out.len());
            e.out.push(0x1400_0000); // b done (patched)
        }
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
        for b in done_at { e.out[b] |= ((done - b) as u32) & 0x3ff_ffff; }
        true
    }
}

impl Cpu {
    /// DC ZVA, Xt: zero the 64-byte block holding Xt (DCZID_EL0 = 4), with the micro-TLB
    /// fast path of a store and the interpreter as the slow path.
    fn emit_dc_zva(&self, e: &mut Emit, i: u32, pc: u64) -> bool {
        if self.jit.as_ref().is_some_and(|j| j.verify) { return false; } // verify mode logs stores in the interpreter
        let rt = i & 31;
        let (vreg, tmp) = (13u32, 14u32);
        e.count_pending();
        if rt == 31 { e.out.push(0xaa1f_03e0 | vreg); } else { e.out.push(ldr_x(vreg, X_CPU, e.reg_off(rt))); }
        e.out.push(0x9240_0000 | (58 << 16) | (57 << 10) | (vreg << 5) | vreg); // and x13, x13, #~63
        let mut slow: Vec<usize> = Vec::new();
        self.emit_utlb(e, 2, 0, false, true, &mut slow);
        for k in 0..4u32 { e.out.push(0xa900_0000 | ((2 * k) << 15) | (31 << 10) | (tmp << 5) | 31); } // stp xzr, xzr, [x14, #16k]
        e.out.push(0x9100_0400 | (X_COUNT << 5) | X_COUNT); // add x20, x20, #1
        let skip_at = e.out.len();
        e.out.push(0x1400_0000); // b done (patched)
        let slow_at = e.out.len();
        for b in slow { let off = (slow_at - b) as u32; e.out[b] |= (off & 0x7ffff) << 5; }
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

/// The shared block dispatcher, entered from a block's exit with its frame still pushed, the
/// guest state stored (pc, instret, flags) and x14 = the mode bits of the block key. Jumps into
/// the next compiled block (a tail call) or returns 0 to `jit_step`.
fn dispatcher(c: &Ctx, front: u64) -> Vec<u32> {
    let probe = (u64::MAX, u64::MAX, 0usize);
    let base = &probe as *const _ as usize;
    let (o_pc, o_key, o_f) = (&probe.0 as *const _ as usize - base, &probe.1 as *const _ as usize - base, &probe.2 as *const _ as usize - base);
    assert_eq!(std::mem::size_of::<(u64, u64, usize)>(), 24);
    assert!(c.off_chain_stop < 65536 && c.off_utlb + 16 * UT < 32768, "offsets: chain_stop {} utlb {} until {} disp {} pc {} instret {}", c.off_chain_stop, c.off_utlb, c.off_chain_until, c.off_disp, c.off_pc, c.off_instret);
    let mut o: Vec<u32> = Vec::new();
    let mut to_out: Vec<(usize, bool)> = Vec::new(); // (index, is cbnz)
    let cmp = |a: u32, b: u32| 0xeb00_001f | (b << 16) | (a << 5);
    o.push(ldr_x(9, X_CPU, c.off_instret));
    o.push(ldr_x(10, X_CPU, c.off_chain_until));
    o.push(cmp(9, 10));
    to_out.push((o.len(), false)); o.push(0x5400_0002); // b.hs out
    o.push(0xd280_0000 | ((c.off_chain_stop as u32) << 5) | 9); // movz x9, #off(chain_stop)
    o.push(0x3860_6800 | (9 << 16) | (X_CPU << 5) | 9); // ldrb w9, [x19, x9]
    to_out.push((o.len(), true)); o.push(0x3500_0009); // cbnz w9, out
    o.push(ldr_x(9, X_CPU, c.off_pc));
    o.push(0xf240_0400 | (9 << 5) | 31); // tst x9, #3
    to_out.push((o.len(), false)); o.push(0x5400_0001); // b.ne out
    // fetch micro-TLB: x11 = &utlb[0][(pc >> 12) % UT], tag = (pc >> 12) + 1 | el << 60
    o.push(0xd34c_fc00 | (9 << 5) | 10); // lsr x10, x9, #12
    o.push(0x9240_0000 | ((UT.trailing_zeros() - 1) << 10) | (10 << 5) | 11); // and x11, x10, #(UT-1)
    o.push(0x8b00_0000 | (11 << 16) | (4 << 10) | (X_CPU << 5) | 11); // add x11, x19, x11, lsl #4
    o.push(ldr_x(12, 11, c.off_utlb));
    o.push(0x9100_0400 | (10 << 5) | 10); // add x10, x10, #1
    o.push(0xd340_0000 | (56 << 16) | (57 << 10) | (14 << 5) | 13); // ubfx x13, x14, #56, #2 (el)
    o.push(0xaa00_0000 | (13 << 16) | (60 << 10) | (10 << 5) | 10); // orr x10, x10, x13, lsl #60
    o.push(cmp(12, 10));
    to_out.push((o.len(), false)); o.push(0x5400_0001); // b.ne out
    // key = physical pc | mode bits
    o.push(ldr_x(12, 11, c.off_utlb + 8));
    o.push(0x9240_0000 | (62 << 16) | (9 << 10) | (9 << 5) | 10); // and x10, x9, #0xffc
    o.push(0xaa00_0000 | (10 << 16) | (12 << 5) | 12); // orr x12, x12, x10
    o.push(0xaa00_0000 | (14 << 16) | (12 << 5) | 12); // orr x12, x12, x14
    // front slot = ((pc ^ pc >> 14) >> 2) % FRONT
    o.push(0xca40_0000 | (9 << 16) | (14 << 10) | (9 << 5) | 10); // eor x10, x9, x9, lsr #14
    o.push(0xd340_0000 | (2 << 16) | ((1 + FRONT.trailing_zeros()) << 10) | (10 << 5) | 10); // ubfx x10, x10, #2, #log2(FRONT)
    mov64(&mut o, 11, front);
    o.push(0xd280_0000 | (24 << 5) | 13); // movz x13, #24
    o.push(0x9b00_0000 | (13 << 16) | (11 << 10) | (10 << 5) | 11); // madd x11, x10, x13, x11
    o.push(ldr_x(13, 11, o_pc));
    o.push(cmp(13, 9));
    to_out.push((o.len(), false)); o.push(0x5400_0001); // b.ne out
    o.push(ldr_x(13, 11, o_key));
    o.push(cmp(13, 12));
    to_out.push((o.len(), false)); o.push(0x5400_0001); // b.ne out
    o.push(ldr_x(16, 11, o_f));
    o.push(0xaa13_03e0); // mov x0, x19
    o.push(0xa941_53f3); // ldp x19, x20, [sp, #16]
    o.push(0xa8c2_7bfd); // ldp x29, x30, [sp], #32
    o.push(0xd61f_0200); // br x16
    let out = o.len();
    o.push(0x5280_0000); // movz w0, #0
    o.push(0xa941_53f3);
    o.push(0xa8c2_7bfd);
    o.push(0xd65f_03c0); // ret
    for (k, cb) in to_out {
        let d = (out - k) as u32;
        o[k] |= d << 5;
        let _ = cb;
    }
    o
}

/// cbz w0 skip length: over "cmp; b.ne; ret_inline(4); ret(4)" = 10 instructions, +1
fn e_skip_len() -> u32 { 11 }

/// Interpreted instructions after which the block must end (they can change the translation
/// regime, the exception level, the code itself or interrupt masking).
pub(crate) fn is_block_end(i: u32) -> bool {
    if bits(i, 31, 22) == 0b11_0101_0100 {
        // system: reads (MRS, SYSL) change nothing; of the SYS instructions only TLB and
        // instruction-cache maintenance and AT matter (data-cache operations are not modelled)
        let (l, op0, crn, crm) = (bit(i, 21), bits(i, 20, 19), bits(i, 15, 12), bits(i, 11, 8));
        if l { return false; }
        if op0 == 1 { return crn == 8 || (crn == 7 && matches!(crm, 1 | 5 | 8 | 9)); }
        return true; // MSR, hints (WFI/WFE), barriers, PSTATE
    }
    bits(i, 31, 24) == 0b1101_0100 // exception generation
        || (bits(i, 31, 25) == 0b110_1011) // branch-register class incl. ERET
}

pub fn available() -> bool { host::AVAILABLE }
