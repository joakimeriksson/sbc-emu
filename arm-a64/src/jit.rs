//! A64 → A64 block JIT (AArch64 macOS/Linux hosts); the driver here (block map, front cache,
//! links, invalidation, verify mode) also serves the x86-64 Linux backend (`jit_x64.rs`, a
//! translator) and the WebAssembly one (`jit_wasm.rs`).
//!
//! Guest and host share the instruction set, so integer and FP/SIMD data-processing
//! instructions are emitted *as themselves* with the guest registers renamed to the host
//! registers that cache them: the host computes exactly the architected result, flags included.
//! Inside a block guest X registers and SP live in x0-x8/x22-x28 and V registers in v16-v31
//! (loaded on first use, written back before exits and interpreter calls); guest NZCV and FPSR
//! stay in the host's, in blocks, across linked branches and in the dispatcher (they go to
//! `Cpu` around interpreter calls and on the way back to Rust). A block continues past short
//! forward conditional branches (`sb_continue`): their taken side leaves through a side exit
//! after the block's code. Interpreter calls and micro-TLB refills go through shared stubs
//! next to the dispatcher. Loads and stores probe the core's micro-TLB inline (`ldr x14, entry; add x14,
//! x14, va; ldr rt, [x21, x14]` after the tag check); their slow path after the block first
//! refills the micro-TLB (`h_tlb`, no interpreter) and then calls the interpreter; system
//! instructions, exception generation and ERET call the interpreter and end the block, so
//! memory, MMU faults, exceptions and devices keep the interpreter's exact semantics.
//!
//! Rust enters compiled code through an enter stub (x19 = Cpu, x20 = instret, x21 = guest RAM);
//! block exits branch straight into the next block or through the shared dispatcher while
//! `instret < chain_until`, and return through the exit stub. Direct branches are patched in:
//! within a page plainly, into another page behind a check that the fetch micro-TLB still maps
//! the target page to the physical page it was linked with (unpatched when that page's code
//! is invalidated).
//!
//! Blocks are keyed by physical address, exception level and SPSel and never cross a 4 KiB
//! page. They are discarded by `IC IALLU[IS]` / `IC IVAU` — the cache maintenance software must
//! perform after writing code, since CTR_EL0.DIC/IDC are 0 — and when the code buffer fills.
//! The interpreter stays the reference: `jit_enabled = false` gives identical results.
use crate::cpu::{UT, UTLB_HASH};
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
    pub fn supported() -> bool { true }
}

#[cfg(all(target_arch = "x86_64", target_os = "linux"))]
mod host {
    use std::ffi::{c_int, c_void};
    extern "C" {
        fn mmap(addr: *mut c_void, len: usize, prot: c_int, flags: c_int, fd: c_int, off: i64) -> *mut c_void;
    }

    /// Executable code buffer (bump allocated; reset as a whole). x86 keeps instruction fetch
    /// coherent with stores of the same thread, so writes need no cache maintenance.
    pub struct CodeCache { base: *mut u8, size: usize, used: usize }

    impl CodeCache {
        pub fn new(size: usize) -> Option<CodeCache> {
            // SAFETY: anonymous mapping chosen by the OS; failure is checked
            let p = unsafe { mmap(std::ptr::null_mut(), size, 7, 0x0002 | 0x0020, -1, 0) }; // RWX, PRIVATE | ANONYMOUS
            if p as isize == -1 || p.is_null() { return None; }
            Some(CodeCache { base: p as *mut u8, size, used: 0 })
        }
        pub fn reset(&mut self) { self.used = 0; }
        /// Where the next `write_bytes` puts its code (blocks start 16-byte aligned).
        pub fn next_addr(&self) -> usize { self.base as usize + self.used.next_multiple_of(16) }
        /// Copy machine code in; returns its entry address.
        pub fn write_bytes(&mut self, bytes: &[u8]) -> Option<*const u8> {
            let off = self.used.next_multiple_of(16);
            if off + bytes.len() > self.size { return None; }
            // SAFETY: the range is inside the mapping
            unsafe { std::ptr::copy_nonoverlapping(bytes.as_ptr(), self.base.add(off), bytes.len()) };
            self.used = off + bytes.len();
            Some(self.base.wrapping_add(off))
        }
        /// (AArch64 words: not used on this host)
        pub fn write(&mut self, words: &[u32]) -> Option<*const u8> {
            let b: Vec<u8> = words.iter().flat_map(|w| w.to_le_bytes()).collect();
            self.write_bytes(&b)
        }
        /// Replace the rel32 of a direct jump (4 bytes, unaligned) in already written code.
        pub fn patch(&mut self, at: usize, v: u32) {
            debug_assert!(at >= self.base as usize && at + 4 <= self.base as usize + self.used);
            // SAFETY: `at` is a rel32 field of code written earlier into the mapping
            unsafe { (at as *mut u32).write_unaligned(v) };
        }
    }
    pub const AVAILABLE: bool = true;
    pub fn supported() -> bool { super::x64::host_ok() }
}

#[cfg(not(any(all(target_arch = "aarch64", any(target_os = "macos", target_os = "linux")), all(target_arch = "x86_64", target_os = "linux"))))]
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
    pub fn supported() -> bool { true }
}

/// The x86-64 backend (a translator; see its module comment).
#[cfg(all(target_arch = "x86_64", target_os = "linux"))]
#[path = "jit_x64.rs"]
mod x64;

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
    /// the block dispatcher in the code cache (emitted after each reset, with the exit and
    /// enter stubs)
    disp: Option<u64>,
    exit: u64,
    enter: u64,
    interp: u64,
    refill: u64,
    /// direct links: target block key -> branch sites waiting for it (code address, the
    /// site's unlinked word, links to another page)
    links: Map<(u64, u64), Vec<(usize, u32, bool)>>,
    /// link sites of the block just compiled: (code address, target virtual pc, the target's
    /// physical page for a link to another page, the site's unlinked word)
    new_sites: Vec<(usize, u64, Option<u64>, u32)>,
    /// linked branches into another page: target block key -> (code address, unlinked word),
    /// unlinked again when the target's page is invalidated (same-page links die with their page)
    xlinks: Map<(u64, u64), Vec<(usize, u32)>>,
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
        if !available() { return None; }
        Some(Box::new(Jit { code: host::CodeCache::new(code_bytes)?, blocks: Map::default(), front: vec![(u64::MAX, u64::MAX, 0); FRONT].into_boxed_slice(),
                            hotc: vec![(u64::MAX, u64::MAX, 0); HOTC].into_boxed_slice(), probe: Map::default(), scratch: None, pages: Map::default(), compiled: 0, flushes: 0, native: 0, hot, verify: false, report: None, code_hash: Map::default(), stale_skips: 0, execs: 0, exec_insns: 0, disp: None, exit: 0, enter: 0, interp: 0, refill: 0, links: Map::default(), new_sites: Vec::new(), xlinks: Map::default(), linked: 0, full_flushes: 0, inval_pages: 0, inval_blocks: 0 }))
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
        self.xlinks.clear();
        self.flushes += 1;
        #[cfg(target_arch = "wasm32")]
        crate::jit_wasm::reset();
    }

    /// Resolve direct branches after block `k` (code at `f`) was added: its own exits to blocks
    /// that exist, and exits of earlier blocks that were waiting for it. A linked branch enters
    /// past the prologue (the frame is already set up).
    fn link(&mut self, k: (u64, u64), f: usize) {
        const BODY: usize = LINKED_ENTRY;
        // the patched word: an A64 B, or on x86-64 the rel32 of a JMP (`site` is its address)
        #[cfg(not(target_arch = "x86_64"))]
        let branch = |site: usize, to: usize| 0x1400_0000 | ((((to as i64 - site as i64) >> 2) as u32) & 0x3ff_ffff);
        #[cfg(target_arch = "x86_64")]
        let branch = |site: usize, to: usize| (to as i64 - (site as i64 + 4)) as i32 as u32;
        let mode = k.1 & !((1u64 << 48) - 1);
        let page_pa = k.1 & ((1u64 << 48) - 1) & !0xfff;
        for (site, t, tpa, orig) in std::mem::take(&mut self.new_sites) {
            let tk = (t, tpa.unwrap_or(page_pa) | (t & 0xfff) | mode);
            if let Some(&tf) = self.blocks.get(&tk) {
                self.code.patch(site, branch(site, tf as usize + BODY));
                self.linked += 1;
                if tpa.is_some() { self.xlinks.entry(tk).or_default().push((site, orig)); }
            } else {
                self.links.entry(tk).or_default().push((site, orig, tpa.is_some()));
            }
        }
        if let Some(waiting) = self.links.remove(&k) {
            for (site, orig, cross) in waiting {
                self.code.patch(site, branch(site, f + BODY));
                self.linked += 1;
                if cross { self.xlinks.entry(k).or_default().push((site, orig)); }
            }
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
            // branches from other pages into this block go back to their dispatcher path, and
            // wait for the block to be compiled again
            for (site, orig) in self.xlinks.remove(&k).unwrap_or_default() {
                self.code.patch(site, orig);
                self.links.entry(k).or_default().push((site, orig, true));
            }
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

/// Does a superblock continue past the conditional branch at `pc` to `target`? Only past
/// short forward skips (at most 8 instructions: `if (x) { a few }`, like CPython's reference
/// count checks), whose fall-through is usually the path taken. Continuing past every branch
/// compiled 2.3x the guest code (cold fall-throughs, compiled again as blocks of their own)
/// and made process start-up slower.
fn sb_continue(pc: u64, target: u64) -> bool { target > pc && target - pc <= 32 }

/// debugging: `SBC_JIT_SUPERBLOCK=0` ends blocks at every conditional branch
fn superblocks_enabled() -> bool {
    static C: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *C.get_or_init(|| std::env::var("SBC_JIT_SUPERBLOCK").map_or(true, |v| v != "0"))
}

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

/// debugging: `SBC_JIT_MAP=FILE` appends "host-address bytes guest-pc words" for every block
/// (and "dispatcher") written to the code buffer, to attribute host profiler samples
fn code_map(start: usize, bytes: usize, what: &str) {
    static F: std::sync::OnceLock<Option<std::sync::Mutex<std::fs::File>>> = std::sync::OnceLock::new();
    let f = F.get_or_init(|| std::env::var("SBC_JIT_MAP").ok().and_then(|p| std::fs::OpenOptions::new().create(true).append(true).open(p).ok()).map(std::sync::Mutex::new));
    if let Some(f) = f { use std::io::Write; let _ = writeln!(f.lock().unwrap(), "{start:x} {bytes} {what}"); }
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
/// callee-saved, set up by the enter stub: the guest's instret while blocks run (in `Cpu`
/// only at calls and returns), and the host address of guest RAM
const X_ICOUNT: u32 = 20;
const X_RAM: u32 = 21;
/// Bytes from a block's start to its linked entry: blocks have no prologue (the enter stub
/// loads the guest NZCV and FPSR into the host registers, where they stay while blocks run).
const LINKED_ENTRY: usize = 0;
/// Host registers that cache guest general registers inside a block (x22..x28 are saved by
/// the enter stub). Before any call out of generated code (the interpreter) and before every
/// exit, changed values are written back to `Cpu`; after a call they are forgotten or reloaded.
const CACHE: [u32; 16] = [0, 1, 2, 3, 4, 5, 6, 7, 8, 22, 23, 24, 25, 26, 27, 28];
/// Host registers caching guest V registers (caller-saved; v0..v7 stay free as scratch).
const VCACHE: [u32; 16] = [16, 17, 18, 19, 20, 21, 22, 23, 24, 25, 26, 27, 28, 29, 30, 31];
#[derive(Clone, Copy, PartialEq)]
enum Bank { X, V }
/// cache key of the guest stack pointer (0..=30 are X0..X30)
const SP_G: u32 = 31;

/// One host register of the guest register cache.
#[derive(Clone, Copy, Default)]
struct Slot { guest: Option<u32>, dirty: bool, used: u32, pinned: bool }

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
const MRS_FPSR_X9: u32 = 0xd53b_4429;
const MSR_FPSR_X9: u32 = 0xd51b_4429;

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
    let (s, tag) = (crate::cpu::uti(cpu.pc), crate::cpu::utag(cpu.pc, cpu.el as u64));
    let Some(e) = [cpu.utlb[0][s], cpu.utlb[0][s + 1]].into_iter().find(|e| e.0 == tag) else { return };
    let pa = e.1 | (cpu.pc & 0xfff);
    if pa + 4 > cpu.ram_len { return; }
    // SAFETY: inside guest RAM
    let i = unsafe { (cpu.ram.add(pa as usize) as *const u32).read_unaligned() };
    *FALLBACKS.lock().unwrap().get_or_insert_with(Default::default).entry(i).or_insert(0) += 1;
}

/// Micro-TLB refill, the first step of a generated load/store's slow path: fill table
/// `arg & 3` (1 load, 2 store) for `va` when its page is guest RAM and the access is allowed
/// (`arg & 4`: unprivileged; `arg >> 8`: bytes accessed). 1 = filled, the access runs again
/// natively; 0 = the interpreter runs the instruction (a fault, a device, a page crossing, an
/// entry that was there already). Integer code only: the guest FPSR stays in the host's.
unsafe extern "C" fn h_tlb(cpu: *mut Cpu, va: u64, arg: u64) -> u32 {
    let cpu = &mut *cpu;
    let (k, unpriv, span) = ((arg & 3) as usize, arg & 4 != 0, arg >> 8);
    if !cpu.watch.is_empty() || (va & 0xfff) + span > 0x1000 { return 0; }
    // the interpreter's own condition for using the EL0 entries from EL1 (see `utlb_unpriv`)
    if unpriv && !(cpu.el <= 1 && cpu.utlb_unpriv == 0) { return 0; }
    let tag = crate::cpu::utag(va, if unpriv { 0 } else { cpu.el as u64 });
    let s = crate::cpu::uti(va);
    if cpu.utlb[k][s].0 == tag || cpu.utlb[k][s + 1].0 == tag { return 0; }
    let acc = match (k, unpriv) {
        (1, false) => crate::mmu::Access::Read,
        (1, true) => crate::mmu::Access::ReadUnpriv,
        (_, false) => crate::mmu::Access::Write,
        (_, true) => crate::mmu::Access::WriteUnpriv,
    };
    let bus = &mut *cpu.jit_bus.expect("bus");
    let Ok(pa) = cpu.translate(bus, va, acc) else { return 0 };
    let off = pa.wrapping_sub(cpu.ram_base);
    if cpu.ram.is_null() || (off | 0xfff) >= cpu.ram_len { return 0; }
    cpu.utlb_fill(k, va, tag, off & !0xfff);
    1
}

unsafe extern "C" fn h_step(cpu: *mut Cpu, expect_next: u64) -> u32 {
    let cpu = &mut *cpu;
    cpu.pc = expect_next.wrapping_sub(4);
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

struct Ctx { off_interp: usize, off_refill: usize, off_exit: usize, off_utlb_unpriv: usize, off_tpidr: usize, off_chain_until: usize, off_chain_stop: usize, off_disp: usize, off_x: usize, off_sp: usize, off_pc: usize, off_nzcv: usize, off_instret: usize, off_utlb: usize, off_ram: usize, off_excl: usize, off_excl_addr: usize, off_excl_val: usize, off_v: usize, off_fpsr: usize }

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
        off_exit: std::mem::offset_of!(Cpu, jit_exit),
        off_interp: std::mem::offset_of!(Cpu, jit_interp),
        off_refill: std::mem::offset_of!(Cpu, jit_refill),
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

struct Emit<'a> { out: Vec<u32>, c: &'a Ctx, sp_slot: usize, pending_native: u64,
                  /** chain to the next block: its key's mode bits (bits 48.. of `key`), None = plain return */ chain: Option<u32>,
                  /** virtual page of the block (targets there can be linked directly) */ page: u64,
                  /** patchable direct branches: (word index, target pc, target's physical page if
                  on another page, unlinked word) */ links: Vec<(usize, u64, Option<u64>, u32)>,
                  /** the fetch micro-TLB as it was at compile time, and the exception level: the
                  physical pages of branch targets on other pages */ fetch: Box<[(u64, u64); UT]>, el: u64,
                  /** 64-bit literals after the code: (index of the LDR (literal), value) */ lits: Vec<(usize, u64)>,
                  /** guest registers held in `CACHE` / `VCACHE` */ ra: [Slot; CACHE.len()], va: [Slot; VCACHE.len()], tick: u32,
                  /** load/store slow paths, emitted after the block */ slow_paths: Vec<SlowPath>,
                  /** superblock: conditional branches do not end the block */ sb: bool,
                  /** taken sides of the conditional branches the block continues past */ side_exits: Vec<SideExit> }

impl Emit<'_> {
    fn reg_off(&self, r: u32) -> usize { self.c.off_x + 8 * r as usize }
    /// `Cpu` offset of cache key `g` (a general register, or the SP of the current EL)
    fn g_off(&self, g: u32) -> usize { if g == SP_G { self.sp_slot } else { self.reg_off(g) } }
    /// Cache key of register field value `r` read as `how`; None = the zero register.
    fn key_of(r: u32, how: R) -> Option<u32> { if r != 31 { Some(r) } else if how == R::Sp { Some(SP_G) } else { None } }
    fn slots(&mut self, b: Bank) -> &mut [Slot] { match b { Bank::X => &mut self.ra, Bank::V => &mut self.va } }
    fn slots_ref(&self, b: Bank) -> &[Slot] { match b { Bank::X => &self.ra, Bank::V => &self.va } }
    fn hosts(b: Bank) -> &'static [u32] { match b { Bank::X => &CACHE, Bank::V => &VCACHE } }
    /// Load / store of host register `h` from / to guest register `g`'s place in `Cpu`.
    fn ld(&self, b: Bank, h: u32, g: u32) -> u32 {
        match b { Bank::X => ldr_x(h, X_CPU, self.g_off(g)), Bank::V => 0x3dc0_0000 | ((v_off(self.c, g) / 16) << 10) | (X_CPU << 5) | h }
    }
    fn st(&self, b: Bank, h: u32, g: u32) -> u32 {
        match b { Bank::X => str_x(h, X_CPU, self.g_off(g)), Bank::V => 0x3d80_0000 | ((v_off(self.c, g) / 16) << 10) | (X_CPU << 5) | h }
    }
    /// A host register holding guest register `g` of bank `b` for the current instruction
    /// (pinned until `unpin`), loaded from `Cpu` when `load` and not cached yet. Evicts the
    /// least recently used unpinned register, writing it back if changed.
    fn alloc(&mut self, b: Bank, g: u32, load: bool) -> u32 {
        self.tick += 1;
        let tick = self.tick;
        let hosts = Self::hosts(b);
        let sl = self.slots(b);
        if let Some(k) = sl.iter().position(|s| s.guest == Some(g)) {
            sl[k].used = tick;
            sl[k].pinned = true;
            return hosts[k];
        }
        let k = sl.iter().position(|s| s.guest.is_none()).unwrap_or_else(|| {
            (0..sl.len()).filter(|&k| !sl[k].pinned).min_by_key(|&k| sl[k].used).expect("an unpinned cache register")
        });
        let old = sl[k];
        sl[k] = Slot { guest: Some(g), dirty: false, used: tick, pinned: true };
        if let (Some(og), true) = (old.guest, old.dirty) { let w = self.st(b, hosts[k], og); self.out.push(w); }
        if load { let w = self.ld(b, hosts[k], g); self.out.push(w); }
        hosts[k]
    }
    /// Host register to read register field `r` from (31 = the zero register when `how` is Zr).
    fn get(&mut self, r: u32, how: R) -> u32 { match Self::key_of(r, how) { Some(g) => self.alloc(Bank::X, g, true), None => 31 } }
    /// Host register to write register field `r` into (not loaded: the caller writes all of it
    /// and then calls `dirty`). Reading sources first keeps a source that is also the
    /// destination loaded.
    fn def(&mut self, r: u32, how: R) -> u32 { match Self::key_of(r, how) { Some(g) => self.alloc(Bank::X, g, false), None => 31 } }
    /// The cached register for field `r` now holds a new guest value.
    fn dirty(&mut self, r: u32, how: R) { if let Some(g) = Self::key_of(r, how) { self.mark(Bank::X, g); } }
    /// V registers: read (loaded), written whole (not loaded), changed.
    fn vget(&mut self, r: u32) -> u32 { self.alloc(Bank::V, r, true) }
    fn vdef(&mut self, r: u32) -> u32 { self.alloc(Bank::V, r, false) }
    fn vdirty(&mut self, r: u32) { self.mark(Bank::V, r); }
    fn mark(&mut self, b: Bank, g: u32) { if let Some(s) = self.slots(b).iter_mut().find(|s| s.guest == Some(g)) { s.dirty = true; } }
    fn unpin(&mut self) { for s in self.ra.iter_mut().chain(self.va.iter_mut()) { s.pinned = false; } }
    /// Stores of the changed cached registers (the state is left as it is: for slow paths).
    fn spill_code(&self) -> Vec<u32> {
        let mut v = Vec::new();
        for b in [Bank::X, Bank::V] {
            for (k, s) in self.slots_ref(b).iter().enumerate() { if let (Some(g), true) = (s.guest, s.dirty) { v.push(self.st(b, Self::hosts(b)[k], g)); } }
        }
        v
    }
    /// Loads of every cached register (after a slow path's interpreter call).
    fn reload_code(&self) -> Vec<u32> {
        let mut v = Vec::new();
        for b in [Bank::X, Bank::V] {
            for (k, s) in self.slots_ref(b).iter().enumerate() { if let Some(g) = s.guest { v.push(self.ld(b, Self::hosts(b)[k], g)); } }
        }
        v
    }
    /// Write the changed registers back (they stay cached, now clean).
    fn ra_flush(&mut self) {
        let st = self.spill_code();
        self.out.extend(st);
        for s in self.ra.iter_mut().chain(self.va.iter_mut()) { s.dirty = false; }
    }
    /// Drop the cache (the interpreter may have changed any register in `Cpu`).
    fn ra_forget(&mut self) { self.ra = [Slot::default(); CACHE.len()]; self.va = [Slot::default(); VCACHE.len()]; }
    /// Write back and drop the V registers only (code that works on them in `Cpu`).
    fn v_flush_forget(&mut self) {
        for k in 0..VCACHE.len() { if let (Some(g), true) = (self.va[k].guest, self.va[k].dirty) { let w = self.st(Bank::V, VCACHE[k], g); self.out.push(w); } }
        self.va = [Slot::default(); VCACHE.len()];
    }
    /// Fold the compile-time count of natives into x20 (the guest's instret while blocks run).
    fn count_pending(&mut self) {
        if self.pending_native == 0 { return; }
        let n = self.pending_native as u32;
        self.out.push(0x9100_0000 | (n << 10) | (X_ICOUNT << 5) | X_ICOUNT); // add x20, x20, #n (flags untouched)
        self.pending_native = 0;
    }
    /// instret into `Cpu` (before anything outside generated code can observe it).
    fn store_instret(&mut self) {
        self.count_pending();
        self.out.push(str_x(X_ICOUNT, X_CPU, self.c.off_instret));
    }
    /// Guest NZCV and FPSR live in the host registers while generated code runs (blocks,
    /// linked branches and the dispatcher touch neither); `Cpu` gets them before calls into
    /// the interpreter (which may change both) and on the way back to Rust.
    fn save_flags(&mut self) {
        self.out.push(MRS_NZCV_X9);
        self.out.push(str_w(9, X_CPU, self.c.off_nzcv));
        self.out.push(MRS_FPSR_X9);
        self.out.push(str_x(9, X_CPU, self.c.off_fpsr));
    }
    fn load_flags(&mut self) {
        self.out.push(ldr_w(9, X_CPU, self.c.off_nzcv));
        self.out.push(MSR_NZCV_X9);
        self.out.push(ldr_x(9, X_CPU, self.c.off_fpsr));
        self.out.push(MSR_FPSR_X9);
    }
    /// Return `status` to Rust through the exit stub (NZCV and FPSR already in `Cpu`).
    fn leave(&mut self, status: u32) {
        self.out.push(str_x(X_ICOUNT, X_CPU, self.c.off_instret));
        self.out.push(0x5280_0000 | (status << 5)); // movz w0, #status
        self.out.push(ldr_x(16, X_CPU, self.c.off_exit));
        self.out.push(0xd61f_0200); // br x16
    }
    /// Continue at cpu.pc: the dispatcher, with the guest NZCV and FPSR live in the host
    /// registers (without chaining, as in verify mode: back to Rust).
    fn to_dispatcher(&mut self) {
        self.count_pending();
        if let Some(kb) = self.chain {
            self.out.push(0xd2e0_0000 | (kb << 5) | 14); // movz x14, #kb, lsl #48
            self.out.push(ldr_x(16, X_CPU, self.c.off_disp));
            self.out.push(0xd61f_0200); // br x16
        } else {
            self.save_flags();
            self.leave(0);
        }
    }
    /// Leave the block for constant `target` (guest registers written back, NZCV live). A
    /// target on the same page gets a patchable direct branch into its block (past its NZCV
    /// load), taken while the budget lasts; the budget is read from `Cpu` each time, as other
    /// threads zero it to stop this core.
    fn exit_const(&mut self, target: u64) {
        self.count_pending();
        if self.chain.is_some() && target >> 12 == self.page {
            self.out.push(ldr_x(10, X_CPU, self.c.off_chain_until));
            self.out.push(0xcb00_0000 | (10 << 16) | (X_ICOUNT << 5) | 9); // sub x9, x20, x10
            self.out.push(0x3600_0000 | (1 << 31) | (31 << 19) | (2 << 5) | 9); // tbz x9, #63, +2 (budget used up)
            self.links.push((self.out.len(), target, None, 0x1400_0001));
            self.out.push(0x1400_0001); // b +1, patched to the target block's linked entry
        } else if let Some(tpa) = self.chain.and_then(|_| self.fetch_page(target)) {
            self.cross_link(target, tpa);
        }
        self.set_pc_const(target);
        self.to_dispatcher();
    }
    /// The physical page of `va` in the fetch micro-TLB at compile time.
    fn fetch_page(&self, va: u64) -> Option<u64> {
        let (s, tag) = (crate::cpu::uti(va), crate::cpu::utag(va, self.el));
        [self.fetch[s], self.fetch[s + 1]].into_iter().find(|e| e.0 == tag).map(|e| e.1)
    }
    /// A patchable branch into the block of `target` on another page, physical page `tpa`,
    /// taken while the budget lasts and the fetch micro-TLB still maps `target`'s page to
    /// `tpa` (another address space may map it elsewhere); falls through to the dispatcher
    /// path otherwise. Uses x9 and x10.
    fn cross_link(&mut self, target: u64, tpa: u64) {
        let c = self.c;
        let s = crate::cpu::uti(target);
        let (e0, e1) = (c.off_utlb + 16 * s, c.off_utlb + 16 * (s + 1));
        let mut to_fb: Vec<usize> = Vec::new();
        let sub_9_10 = 0xcb00_0000 | (10 << 16) | (9 << 5) | 9; // sub x9, x9, x10
        self.out.push(ldr_x(10, X_CPU, c.off_chain_until));
        self.out.push(0xcb00_0000 | (10 << 16) | (X_ICOUNT << 5) | 9); // sub x9, x20, x10
        to_fb.push(self.out.len()); self.out.push(0x3600_0000 | (1 << 31) | (31 << 19) | 9); // tbz x9, #63, fallback
        // way 0 holds target's page (tag), as tpa?
        self.out.push(ldr_x(9, X_CPU, e0));
        self.lits.push((self.out.len(), crate::cpu::utag(target, self.el))); self.out.push(0x5800_0000 | 10); // ldr x10, =tag
        self.out.push(sub_9_10);
        let to_w1 = self.out.len(); self.out.push(0xb500_0000 | 9); // cbnz x9, way1
        self.out.push(ldr_x(9, X_CPU, e0 + 8));
        let cmp = self.out.len();
        self.lits.push((self.out.len(), tpa)); self.out.push(0x5800_0000 | 10); // ldr x10, =tpa
        self.out.push(sub_9_10);
        to_fb.push(self.out.len()); self.out.push(0xb500_0000 | 9); // cbnz x9, fallback
        let site = self.out.len(); self.out.push(0x1400_0000); // b fallback, patched to the target block
        // way 1 (x10 still the tag)
        let w1 = self.out.len();
        self.out[to_w1] |= (((w1 - to_w1) as u32) & 0x7ffff) << 5;
        self.out.push(ldr_x(9, X_CPU, e1));
        self.out.push(sub_9_10);
        to_fb.push(self.out.len()); self.out.push(0xb500_0000 | 9); // cbnz x9, fallback
        self.out.push(ldr_x(9, X_CPU, e1 + 8));
        let b = self.out.len(); self.out.push(0x1400_0000 | (((cmp as i64 - b as i64) as u32) & 0x3ff_ffff)); // b cmp
        let fb = self.out.len();
        for k in to_fb {
            let d = (fb - k) as u32;
            self.out[k] |= if self.out[k] & 0x7e00_0000 == 0x3600_0000 { (d & 0x3fff) << 5 } else { (d & 0x7ffff) << 5 };
        }
        let orig = 0x1400_0000 | (((fb - site) as u32) & 0x3ff_ffff);
        self.out[site] = orig;
        self.links.push((site, target, Some(tpa), orig));
    }
    /// The literals after the code (LDR (literal) reaches 1 MiB).
    fn emit_lits(&mut self) {
        for (ix, v) in std::mem::take(&mut self.lits) {
            let d = (self.out.len() - ix) as u32;
            self.out[ix] |= (d & 0x7ffff) << 5;
            self.out.push(v as u32);
            self.out.push((v >> 32) as u32);
        }
    }
    fn set_pc_const(&mut self, pc: u64) { mov64(&mut self.out, X_SCRATCH, pc); self.out.push(str_x(X_SCRATCH, X_CPU, self.c.off_pc)); }
}

impl Cpu {
    /// Run a compiled block at the current pc if there is one (compiling hot code).
    /// None = not handled here: the caller interprets one instruction.
    pub(crate) fn jit_step(&mut self, bus: &mut dyn Bus) -> Option<Result<(), Trap>> {
        if self.pc & 3 != 0 { return None; }
        // physical address of pc through the fetch micro-TLB, filled here on a miss (a
        // translation fault is left to the interpreter, which takes it)
        let tag = crate::cpu::utag(self.pc, self.el as u64);
        let ti = match self.utlb_hit(0, self.pc, tag) {
            Some(t) => t,
            None => {
                let pa = self.translate(bus, self.pc, crate::mmu::Access::Exec).ok()?;
                let off = pa.wrapping_sub(self.ram_base);
                if self.ram.is_null() || (off | 0xfff) >= self.ram_len { return None; }
                self.utlb_fill(0, self.pc, tag, off & !0xfff)
            }
        };
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
        let mut r = unsafe { self.call_block(f) };
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

    /// Run compiled block `f`: natively through the enter stub, in the browser as the wasm
    /// function it is.
    /// # Safety
    /// `f` must be a block of this core's JIT, `jit_bus` set.
    #[inline(always)]
    unsafe fn call_block(&mut self, f: BlockFn) -> u32 {
        #[cfg(all(target_arch = "x86_64", target_os = "linux"))]
        {
            // the guest flags go in and come back in the generated code's form
            let enter = self.jit.as_ref().map_or(0, |j| j.enter);
            let enter = std::mem::transmute::<usize, x64::EnterFn>(enter as usize);
            let r = enter(self as *mut Cpu, f as usize, x64::nzcv_to_fl(self.nzcv));
            self.nzcv = x64::fl_to_nzcv(r.fl);
            r.status as u32
        }
        #[cfg(not(any(target_arch = "wasm32", all(target_arch = "x86_64", target_os = "linux"))))]
        {
            let enter = self.jit.as_ref().map_or(0, |j| j.enter);
            let enter = std::mem::transmute::<usize, unsafe extern "C" fn(*mut Cpu, usize) -> u32>(enter as usize);
            enter(self as *mut Cpu, f as usize)
        }
        #[cfg(target_arch = "wasm32")]
        { f(self as *mut Cpu) }
    }

    /// The compiled block for the current pc if the front cache has it.
    #[cfg(target_arch = "wasm32")]
    #[inline(always)]
    fn front_block(&self) -> Option<BlockFn> {
        if self.pc & 3 != 0 { return None; }
        let e = self.utlb[0][crate::cpu::uti(self.pc)];
        if e.0 != crate::cpu::utag(self.pc, self.el as u64) { return None; }
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
        let before = verify.then(|| (self.x, self.sp, self.pc, self.nzcv, self.instret, self.v, self.excl(), (self.el, self.spsel, self.daif), (self.sys.vtimer, self.sys.ptimer, self.sys.timers_changed), self.fpsr));
        if verify { self.store_log = Some(Vec::new()); }
        // SAFETY: the bus outlives the call; generated code only reaches it through h_step
        self.jit_bus = Some(unsafe { std::mem::transmute::<&mut dyn Bus, &'static mut dyn Bus>(bus) } as *mut dyn Bus);
        #[cfg(target_arch = "aarch64")]
        let fpcr = self.fpcr & FPCR_HOST_MASK;
        #[cfg(target_arch = "aarch64")]
        if fpcr != 0 { unsafe { set_host_fpcr(fpcr) }; }
        let r = unsafe { self.call_block(f) };
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
            let fpsr_after = self.fpsr;
            (self.x, self.sp, self.pc, self.nzcv, self.instret, self.v) = (b.0, b.1, b.2, b.3, b.4, b.5);
            self.fpsr = b.9;
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
            if self.fpsr != fpsr_after { diffs.push(format!("fpsr: jit {fpsr_after:#x} interp {:#x}", self.fpsr)); }
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
        let mut retry = false;
        let f = self.compile_with(pa, superblocks_enabled(), &mut retry);
        // a side exit out of TBZ's reach: compile again, ending the block at every branch
        if retry { self.compile_with(pa, false, &mut retry) } else { f }
    }

    /// Compile the block at `pa` (the current pc). `sb`: continue past conditional branches,
    /// their taken side leaving through a side exit after the block; sets `retry` (and returns
    /// None) when a side exit ends up out of a branch's reach.
    fn compile_with(&mut self, pa: u64, sb: bool, retry: &mut bool) -> Option<BlockFn> {
        self.mark_code_page(pa >> 12);
        #[cfg(target_arch = "wasm32")]
        {
            // browser: one small wasm module per block; keep the live set bounded
            if self.jit.as_ref().is_some_and(|j| j.blocks.len() >= WASM_MAX_BLOCKS) { self.jit.as_mut()?.flush(); }
            return self.compile_wasm(pa);
        }
        #[cfg(all(target_arch = "x86_64", target_os = "linux"))]
        {
            let _ = retry;
            return self.compile_x64(pa, sb);
        }
        #[allow(unreachable_code)]
        let c = ctx();
        let sp_slot = c.off_sp + 8 * if self.spsel { self.el as usize } else { 0 };
        let chain = self.jit.as_ref().is_some_and(|j| !j.verify).then(|| (key(0, self.el, self.spsel, (self.sys.cpacr_el1 >> 20) & 3) >> 48) as u32);
        let mut e = Emit { out: Vec::with_capacity(512), c: &c, sp_slot, pending_native: 0, chain, page: self.pc >> 12, links: Vec::new(), fetch: Box::new(self.utlb[0]), el: self.el as u64, lits: Vec::new(), ra: [Slot::default(); CACHE.len()], va: [Slot::default(); VCACHE.len()], tick: 0, slow_paths: Vec::new(), sb, side_exits: Vec::new() };
        let ram_ok = !self.ram.is_null() && pa + 4 <= self.ram_len;
        if !ram_ok { return None; }
        let mut pc = self.pc;
        let mut a = pa;
        let mut natives = 0u64;
        let mut ended = false;
        static MASK: std::sync::OnceLock<u32> = std::sync::OnceLock::new();
        let mask = *MASK.get_or_init(|| std::env::var("SBC_JIT_NATIVE").ok().and_then(|v| v.parse().ok()).unwrap_or(15));
        for _ in 0..MAX_INSNS {
            // SAFETY: `a` is inside guest RAM (checked) and blocks stop at the page end
            let i = unsafe { (self.ram.add(a as usize) as *const u32).read_unaligned() };
            if let Some(ops) = native_operands(i).filter(|_| mask & 1 != 0) {
                // rename guest registers to the host registers caching them; 31 stays ZR or is
                // the cached SP. Sources first, then the destination (field 0, unless the
                // instruction has none: CCMP), which is loaded only when also read (MOVK, BFM).
                let has_rd = !(bits(i, 28, 21) == 0b11010010);
                let rd_read = matches!((bits(i, 28, 23), bits(i, 30, 29)), (0b100101, 3) | (0b100110, 1));
                let mut enc = i;
                for (n, &(shift, how)) in ops.iter().enumerate().skip(has_rd as usize) {
                    let _ = n;
                    let h = e.get((i >> shift) & 31, how);
                    enc = (enc & !(31 << shift)) | (h << shift);
                }
                if has_rd {
                    let (rd, how) = (i & 31, ops[0].1);
                    let h = if rd_read { e.get(rd, how) } else { e.def(rd, how) };
                    enc = (enc & !31) | h;
                    e.out.push(enc);
                    e.dirty(rd, how);
                } else {
                    e.out.push(enc);
                }
                natives += 1;
                e.pending_native += 1;
            } else if bits(i, 28, 24) == 0b10000 && mask & 4 != 0 {
                // ADR / ADRP: a constant
                let imm = (((((bits(i, 23, 5) << 2) | bits(i, 30, 29)) as u64) << 43) as i64 >> 43) as u64;
                let sx = if bit(i, 31) { (pc & !0xfff).wrapping_add(imm << 12) } else { pc.wrapping_add(imm) };
                let rd = i & 31;
                if rd != 31 { let h = e.def(rd, R::Zr); mov64(&mut e.out, h, sx); e.dirty(rd, R::Zr); }
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
                    let h = e.def(rt, R::Zr);
                    e.out.push(ldr_x(h, X_CPU, c.off_tpidr + 8 * n));
                    e.dirty(rt, R::Zr);
                }
                natives += 1;
                e.pending_native += 1;
            } else if i & 0xffff_ffe0 == 0xd538_4100 && self.el >= 1 {
                // MRS Xt, SP_EL0 (Linux keeps `current` there)
                let rt = i & 31;
                if rt != 31 {
                    if e.sp_slot == c.off_sp {
                        // SP_EL0 is the SP in use (SPSel = 0): it may be cached
                        let s = e.get(31, R::Sp);
                        let h = e.def(rt, R::Zr);
                        e.out.push(0xaa00_03e0 | (s << 16) | h); // mov xh, xs
                    } else {
                        let h = e.def(rt, R::Zr);
                        e.out.push(ldr_x(h, X_CPU, c.off_sp));
                    }
                    e.dirty(rt, R::Zr);
                }
                natives += 1;
                e.pending_native += 1;
            } else if matches!(i & 0xffff_fc00, 0x9e66_0000 | 0x1e26_0000 | 0x9e67_0000 | 0x1e27_0000)
                && (self.sys.cpacr_el1 >> 20) & 3 == 3 && v_off(&c, 31) + 16 < if i >> 31 != 0 { 32768 } else { 16384 } && e.reg_off(31) < 16384 {
                // FMOV between the register files (64-bit X<->D, 32-bit W<->S): the host's own
                // FMOV on the cache registers (to a V register it zeroes the rest of the vector)
                let (rd, rn) = (i & 31, (i >> 5) & 31);
                if i & 0x0001_0000 == 0 {
                    if rd != 31 {
                        let v = e.vget(rn);
                        let h = e.def(rd, R::Zr);
                        e.out.push((i & !0x3ff) | (v << 5) | h);
                        e.dirty(rd, R::Zr);
                    }
                } else {
                    let h = e.get(rn, R::Zr);
                    let v = e.vdef(rd);
                    e.out.push((i & !0x3ff) | (h << 5) | v);
                    e.vdirty(rd);
                }
                natives += 1;
                e.pending_native += 1;
            } else if let Some(done) = if mask & 2 != 0 { self.emit_branch(&mut e, i, pc) } else { None } {
                natives += 1;
                if done { ended = true; break; }
            } else if let Some((ops, writes)) = simd_operands(i)
                .filter(|(ops, _)| ops.iter().all(|o| o.count == 1 && o.width == 5))
                .filter(|_| mask & 8 != 0 && (self.sys.cpacr_el1 >> 20) & 3 == 3 && self.jit.as_mut().is_some_and(|j| j.interp_supports(i)))
            {
                // FP/SIMD natively on the V register cache (every operand loaded: some
                // instructions also read Rd, or write only part of it)
                let mut enc = i;
                for o in &ops {
                    let h = e.vget((i >> o.shift) & 31);
                    enc = (enc & !(31 << o.shift)) | (h << o.shift);
                }
                e.out.push(enc);
                if writes { e.vdirty(i & 31); }
                natives += 1;
                e.pending_native += 1;
            } else if let Some((enc, map, writes)) = simd_operands(i)
                .filter(|_| mask & 8 != 0 && (self.sys.cpacr_el1 >> 20) & 3 == 3 && self.jit.as_mut().is_some_and(|j| j.interp_supports(i)))
                .and_then(|(ops, writes)| map_simd(i, &ops).map(|(enc, map)| (enc, map, writes)))
            {
                // tables (consecutive registers) and halfword by-element operands (v0..v15):
                // through host v0..v7, with the V registers in `Cpu`
                e.v_flush_forget();
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
                // DUP Vd.<T>, Wn/Xn: the host's own DUP on the cache registers
                let (rd, rn) = (i & 31, (i >> 5) & 31);
                let h = e.get(rn, R::Zr);
                let v = e.vdef(rd);
                e.out.push((i & !0x3ff) | (h << 5) | v);
                e.vdirty(rd);
                natives += 1;
                e.pending_native += 1;
            } else if ld1_as_ldr(i).is_some_and(|w| self.emit_ldst(&mut e, w, pc)) {
                // LD1/ST1 of one register: the same access as LDR/STR of a Q or D register
            } else if self.emit_excl(&mut e, i, pc) {
                // exclusive and acquire/release accesses, CLREX
            } else if self.emit_ldst(&mut e, i, pc) {
                // native load/store with an interpreter slow path
            } else {
                // the interpreter reads and may write any register in `Cpu`
                e.ra_flush();
                let end = emit_interp(&mut e, i, pc);
                e.ra_forget();
                if end { ended = true; break; }
            }
            e.unpin();
            pc += 4;
            a += 4;
            if a & 0xfff == 0 { break; } // never cross a page
        }
        let _ = natives;
        if !ended {
            // fell off the end of the block: continue at pc
            e.ra_flush();
            e.exit_const(pc);
        }
        if !e.emit_side_exits() { *retry = true; return None; }
        e.emit_slow_paths();
        e.emit_lits();
        if self.jit.as_ref()?.disp.is_none() {
            // the shared stubs, once per code buffer: dispatcher, exit, enter
            let (words, exit_at, enter_at, interp_at, refill_at) = stubs(&c, self.jit.as_ref()?.front.as_ptr() as u64);
            match self.jit.as_mut()?.code.write(&words) {
                Some(d) => {
                    code_map(d as usize, 4 * words.len(), "dispatcher");
                    let j = self.jit.as_mut()?;
                    j.disp = Some(d as u64);
                    j.exit = d as u64 + 4 * exit_at as u64;
                    j.enter = d as u64 + 4 * enter_at as u64;
                    j.interp = d as u64 + 4 * interp_at as u64;
                    j.refill = d as u64 + 4 * refill_at as u64;
                }
                None => { let j = self.jit.as_mut()?; j.full_flushes += 1; j.flush(); return None; }
            }
        }
        if let Some(d) = self.jit.as_ref()?.disp {
            let j = self.jit.as_ref()?;
            (self.jit_disp, self.jit_exit, self.jit_interp, self.jit_refill) = (d, j.exit, j.interp, j.refill);
        }
        let f = self.jit.as_mut()?.code.write(&e.out);
        if let Some(p) = f {
            // guest instructions in the block: `pc` is the last one when a branch or the
            // interpreter ended it, else the one after (page end, MAX_INSNS)
            let n = (pc - self.pc) / 4 + ended as u64;
            code_map(p as usize, 4 * e.out.len(), &format!("{:x} {}", self.pc, n));
            // debugging: `SBC_JIT_DUMP=PC[,PC...]` writes the guest and host words of blocks at PC
            static DUMP: std::sync::OnceLock<Vec<u64>> = std::sync::OnceLock::new();
            let dump = DUMP.get_or_init(|| std::env::var("SBC_JIT_DUMP").unwrap_or_default().split(',')
                .filter_map(|v| u64::from_str_radix(v.trim().trim_start_matches("0x"), 16).ok()).collect());
            if dump.contains(&self.pc) {
                // SAFETY: the block's instructions were read from guest RAM above
                let guest: Vec<u8> = (0..4 * n).map(|k| unsafe { *self.ram.add((pa + k) as usize) }).collect();
                let host: Vec<u8> = e.out.iter().flat_map(|w| w.to_le_bytes()).collect();
                let _ = std::fs::write(format!("jit-{:x}.guest", self.pc), guest);
                let _ = std::fs::write(format!("jit-{:x}.host", self.pc), host);
            }
        }
        let f = match f {
            Some(p) => p,
            None => { let j = self.jit.as_mut()?; j.full_flushes += 1; j.flush(); return None; }
        };
        let sites: Vec<(usize, u64, Option<u64>, u32)> = e.links.iter().map(|&(ix, t, tpa, orig)| (f as usize + 4 * ix, t, tpa, orig)).collect();
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
            // the block ends: cached registers go back first (X30 then written in `Cpu`)
            e.ra_flush();
            e.ra_forget();
            if bit(i, 31) { mov64(&mut e.out, T[0], pc + 4); e.out.push(str_x(T[0], X_CPU, e.reg_off(30))); }
            e.pending_native += 1;
            e.exit_const(target);
            return Some(true);
        }
        // B.cond: select with the guest flags still in the host NZCV
        if bits(i, 31, 24) == 0b0101_0100 && !bit(i, 4) {
            let target = pc.wrapping_add(sx((bits(i, 23, 5) as u64) << 2, 21));
            if e.sb && sb_continue(pc, target) {
                // continue with the not-taken side; the taken side leaves after the block
                e.pending_native += 1;
                e.count_pending();
                e.side_exit(0x5400_0000 | bits(i, 3, 0), false, target); // b.cond side exit
                return Some(false);
            }
            e.ra_flush();
            e.pending_native += 1;
            e.count_pending(); // before the paths split
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
            let h = e.get(rt, R::Zr); // a cache register (or xzr): survives the code below
            if e.sb && sb_continue(pc, target) {
                e.pending_native += 1;
                e.count_pending();
                e.side_exit(if cb { i & !(31 | (0x7ffff << 5)) } else { i & !(31 | (0x3fff << 5)) } | h, tb, target);
                return Some(false);
            }
            e.ra_flush();
            e.pending_native += 1;
            e.count_pending(); // before the paths split
            // the host branch (same test on the same value) jumps to the taken exit
            let at = e.out.len();
            e.out.push(if cb { i & !(31 | (0x7ffff << 5)) } else { i & !(31 | (0x3fff << 5)) } | h);
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
            let h = e.get(rn, R::Zr);
            e.out.push(str_x(h, X_CPU, e.c.off_pc)); // before BLR changes X30 (BLR X30)
            e.ra_flush();
            e.ra_forget();
            if opc == 1 { mov64(&mut e.out, T[1], pc + 4); e.out.push(str_x(T[1], X_CPU, e.reg_off(30))); }
            e.pending_native += 1;
            e.to_dispatcher();
            return Some(true);
        }
        None
    }
}

/// Call the interpreter for the instruction at `pc`; returns true if the block ends here.
fn emit_interp(e: &mut Emit, i: u32, pc: u64) -> bool {
    let end = is_block_end(i);
    if let Some(kb) = e.chain {
        // the shared interpreter stub: flags to `Cpu` and back, status 1 to the dispatcher
        // (x14 = this block's mode bits), status 2 out to Rust; returns on status 0
        e.count_pending();
        mov64(&mut e.out, 1, pc + 4);
        e.out.push(0xd2e0_0000 | (kb << 5) | 14); // movz x14, #kb, lsl #48
        e.out.push(ldr_x(16, X_CPU, e.c.off_interp));
        e.out.push(0xd63f_0200); // blr x16
        if end { e.to_dispatcher(); }
        return end;
    }
    e.store_instret();
    e.save_flags();
    e.set_pc_const(pc);
    e.out.push(0xaa13_03e0); // mov x0, x19
    mov64(&mut e.out, 1, pc + 4);
    mov64(&mut e.out, 16, h_step as *const () as usize as u64);
    e.out.push(0xd63f_0200); // blr x16
    e.out.push(ldr_x(X_ICOUNT, X_CPU, e.c.off_instret)); // now counting the instruction
    // status 0: continue in the block (unless it ends here)
    let cont = (!end).then(|| { e.out.push(0x3400_0000); e.out.len() - 1 }); // cbz w0, continue
    // status 1 (or 0 at a block end): continue at cpu.pc with the flags the interpreter left
    e.out.push(0x7100_081f); // cmp w0, #2
    let trap = e.out.len();
    e.out.push(0x5400_0000); // b.eq trap
    e.load_flags();
    e.to_dispatcher();
    // status 2: a trap, in cpu.jit_trap
    let at = e.out.len();
    e.out[trap] |= (((at - trap) as u32) & 0x7ffff) << 5;
    e.leave(1);
    if let Some(c) = cont {
        let at = e.out.len();
        e.out[c] |= (((at - c) as u32) & 0x7ffff) << 5;
        e.load_flags();
    }
    end
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
        let (vreg, tmp) = (13u32, 14u32);
        let pending = e.pending_native;
        // ---- the cache registers this access uses, all allocated before the first branch to
        // the slow path, so both paths see the same cache: base, index, data (a load's data
        // registers are written only by the fast path; the slow path reloads every cached one)
        let base = e.get(rn, R::Sp);
        let idx = if let Addr::Reg(rm, _, _) = addr { e.get(rm, R::Zr) } else { 31 };
        let (h1, h2) = if vec && load { (e.vdef(rt), if pair { e.vdef(rt2) } else { 0 }) }
            else if vec { (e.vget(rt), if pair { e.vget(rt2) } else { 0 }) }
            else if load { (e.def(rt, R::Zr), if pair { e.def(rt2, R::Zr) } else { 31 }) }
            else { (e.get(rt, R::Zr), if pair { e.get(rt2, R::Zr) } else { 31 }) };
        let spill = e.spill_code();
        let reload = e.reload_code();
        // a refilled micro-TLB runs the access again from here
        let retry = e.out.len();
        // ---- VA in x13
        let add_imm = |out: &mut Vec<u32>, rd: u32, rn: u32, imm: i64| {
            if imm == 0 { if rd != rn { out.push(0x9100_0000 | (rn << 5) | rd); } return; }
            let (op, v) = if imm < 0 { (0xd100_0000u32, (-imm) as u64) } else { (0x9100_0000u32, imm as u64) };
            if v < 4096 { out.push(op | ((v as u32) << 10) | (rn << 5) | rd); }
            else { mov64(out, 17, v); out.push(if imm < 0 { 0xcb00_0000 } else { 0x8b00_0000 } | (17 << 16) | (rn << 5) | rd); }
        };
        match addr {
            // post-index: the access uses the base as it is
            Addr::Imm(imm) => add_imm(&mut e.out, vreg, base, if mode == 1 { 0 } else { imm }),
            Addr::Reg(_, option, shift) => {
                e.out.push(0x8b20_0000 | (idx << 16) | (option << 13) | (shift << 10) | (base << 5) | vreg); // add x13, base, idx, <ext> #sh
            }
        }
        // ---- micro-TLB check: x14 = PA (single registers: [x21, x14]), host address for pairs
        let mut slow: Vec<usize> = Vec::new();
        let way1 = self.emit_utlb(e, if load { 1 } else { 2 }, span, unpriv, !load, &mut slow, true, pair);
        // ---- the access itself, as the host instruction of the same size and extension
        let sz = size.trailing_zeros();
        let one = |rt: u32| -> u32 {
            // LDR/STR (register offset) Rt, [x21, x14]
            let opc = if !load { 0 } else { match signed_to { 0 => 1, 64 => 2, _ => 3 } };
            0x3820_6800 | (sz << 30) | (opc << 22) | (tmp << 16) | (X_RAM << 5) | rt
        };
        if vec {
            // straight into / out of the V cache registers; scalar loads zero the upper bits
            let opc_q = if sz == 4 { 2 } else { 0 };
            let s_bits = if sz == 4 { 0 } else { sz };
            if load {
                if pair { e.out.push([0x2d40_0000u32, 0x6d40_0000, 0xad40_0000][sz as usize - 2] | (h2 << 10) | (tmp << 5) | h1); } // ldp
                else { e.out.push(0x3c60_6800 | (s_bits << 30) | (opc_q << 22) | (tmp << 16) | (X_RAM << 5) | h1); } // ldr b/h/s/d/q, [x21, x14]
                e.vdirty(rt);
                if pair { e.vdirty(rt2); }
            } else if pair {
                e.out.push([0x2d00_0000u32, 0x6d00_0000, 0xad00_0000][sz as usize - 2] | (h2 << 10) | (tmp << 5) | h1); // stp
            } else {
                e.out.push(0x3c20_6800 | (s_bits << 30) | (opc_q << 22) | (tmp << 16) | (X_RAM << 5) | h1); // str b/h/s/d/q, [x21, x14]
            }
        } else if load {
            // straight into the cache registers (xzr for register 31: loaded and dropped)
            if pair && signed_to == 64 {
                e.out.push(0x6940_0000 | (h2 << 10) | (tmp << 5) | h1); // ldpsw
            } else if pair {
                e.out.push(if sz == 3 { 0xa940_0000 } else { 0x2940_0000 } | (h2 << 10) | (tmp << 5) | h1); // ldp
            } else {
                e.out.push(one(h1));
            }
            e.dirty(rt, R::Zr);
            if pair { e.dirty(rt2, R::Zr); }
        } else if pair {
            e.out.push(if sz == 3 { 0xa900_0000 } else { 0x2900_0000 } | (h2 << 10) | (tmp << 5) | h1); // stp
        } else {
            e.out.push(one(h1));
        }
        // ---- base writeback (never onto a data register: refused above)
        if let (1 | 2, Addr::Imm(imm)) = (mode, &addr) {
            if mode == 1 { add_imm(&mut e.out, base, base, *imm); } else { e.out.push(0x9100_0000 | (vreg << 5) | base); } // mov base, x13
            e.dirty(rn, R::Sp);
        }
        // ---- the slow path (the interpreter) goes after the block's last exit; the fast path
        // falls through, and counts this instruction at compile time like any native one
        let refill = (retry, (if load { 1 } else { 2 }) | (unpriv as u64) << 2 | (span as u64) << 8);
        e.slow_paths.push(SlowPath { sites: slow, way1, spill, reload, i, pc, pending, done: e.out.len(), refill });
        e.pending_native += 1;
        e.unpin();
        true
    }
}

/// The taken side of a conditional branch inside a superblock: `site` (b.cond, cbz/cbnz or
/// tbz/tbnz without its offset) jumps to code after the block that writes back the registers
/// dirty at the branch (`spill`) and leaves for `target`.
struct SideExit { site: usize, tb: bool, spill: Vec<u32>, target: u64 }

/// An out-of-line slow path of a native load/store: entered from `sites` with the cache as it
/// was before the access, `pending` natives not yet in x20; rejoins the fast path at `done`.
/// `way1` (branch, continuation, entry offset): the micro-TLB way-0 mismatch branch, which
/// first checks way 1 and on a hit continues the fast path with x17 at that entry.
/// `refill`: (where the access starts, `h_tlb`'s argument): the slow path first tries a
/// micro-TLB refill and on success runs the access again.
struct SlowPath { sites: Vec<usize>, way1: Option<(usize, usize, usize)>, spill: Vec<u32>, reload: Vec<u32>, i: u32, pc: u64, pending: u64, done: usize, refill: (usize, u64) }

impl Emit<'_> {
    /// A conditional branch `w` (offset 0) to a side exit for `target`, with the register
    /// cache as it is now; the block continues with the next instruction.
    fn side_exit(&mut self, w: u32, tb: bool, target: u64) {
        let spill = self.spill_code();
        self.side_exits.push(SideExit { site: self.out.len(), tb, spill, target });
        self.out.push(w);
    }
    /// Emit the side exits after the block's code; false if one is out of reach of its branch
    /// (TBZ reaches 32 KiB).
    fn emit_side_exits(&mut self) -> bool {
        debug_assert_eq!(self.pending_native, 0);
        for se in std::mem::take(&mut self.side_exits) {
            let d = (self.out.len() - se.site) as u32;
            if se.tb {
                if d >= 1 << 13 { return false; }
                self.out[se.site] |= (d & 0x3fff) << 5;
            } else {
                self.out[se.site] |= (d & 0x7ffff) << 5;
            }
            self.out.extend(se.spill);
            self.exit_const(se.target);
        }
        true
    }
    /// Emit the slow paths after the block's code.
    fn emit_slow_paths(&mut self) {
        for sp in std::mem::take(&mut self.slow_paths) {
            if let Some((site, back, ent)) = sp.way1 {
                // way 1: ldr x15, [x17, #ent + 16]; sub x16, x14, x15; cbnz x16, slow; add x17, x17, #16; b back
                let at = self.out.len();
                self.out[site] |= (((at - site) as u32) & 0x7ffff) << 5;
                self.out.push(ldr_x(15, 17, ent + 16));
                self.out.push(0xcb00_0000 | (15 << 16) | (14 << 5) | 16);
                self.out.push(0xb500_0000 | (3 << 5) | 16); // cbnz x16, +3 (the slow path below)
                self.out.push(0x9100_4000 | (17 << 5) | 17); // add x17, x17, #16
                let b = self.out.len();
                self.out.push(0x1400_0000 | (((back as i64 - b as i64) as u32) & 0x3ff_ffff));
            }
            let at = self.out.len();
            for b in sp.sites {
                let off = (at - b) as u32;
                let w = self.out[b];
                self.out[b] = if w & 0x7e00_0000 == 0x3600_0000 { w | ((off & 0x3fff) << 5) } else { w | ((off & 0x7ffff) << 5) };
            }
            // a micro-TLB refill through the shared stub (which keeps every host register)
            let (retry, arg) = sp.refill;
            debug_assert!(arg < 1 << 16);
            self.out.push(0xd280_0000 | ((arg as u32) << 5) | 15); // movz x15, #arg
            self.out.push(ldr_x(16, X_CPU, self.c.off_refill));
            self.out.push(0xd63f_0200); // blr x16
            let b = self.out.len();
            self.out.push(0x3500_0010 | ((((retry as i64 - b as i64) as u32) & 0x7ffff) << 5)); // cbnz w16, retry
            // the interpreter
            self.out.extend(sp.spill);
            self.pending_native = sp.pending;
            let ended = emit_interp(self, sp.i, sp.pc);
            debug_assert!(!ended);
            // instret now includes this instruction and the `pending` before it, which the fast
            // path counts at compile time after the join: take them off again
            self.out.push(0xd100_0000 | (((sp.pending + 1) as u32) << 10) | (X_ICOUNT << 5) | X_ICOUNT); // sub x20, x20, #(pending + 1)
            self.out.extend(sp.reload);
            let b = self.out.len();
            self.out.push(0x1400_0000 | (((sp.done as i64 - b as i64) as u32) & 0x3ff_ffff)); // b done
        }
        self.pending_native = 0;
    }
}

impl Cpu {
    /// The micro-TLB lookup of generated loads and stores (no flags touched). With the VA in
    /// x13, leaves x14 = host address (`abs`; else the PA: x21 + x14 is the host address),
    /// or branches to a slow path (pushed to `slow`, patched by
    /// the caller) on a miss. `k`: 1 = load, 2 = store entries; `span` > 0: bytes accessed;
    /// `excl_check`: a held reservation takes the slow path (the interpreter's store clears
    /// it). Uses x15 and x17.
    /// `way1`: on a way-0 mismatch branch to a way-1 check instead of the slow path; returns
    /// (that branch, the fast path's continuation) for `SlowPath::way1`.
    fn emit_utlb(&self, e: &mut Emit, k: usize, span: u32, unpriv: bool, excl_check: bool, slow: &mut Vec<usize>, way1: bool, abs: bool) -> Option<(usize, usize, usize)> {
        let c = e.c;
        let (vreg, tmp, tmp2) = (13u32, 14u32, 15u32);
        // x17 = &utlb[k][uti(va)]; the tag compared is that of the access's last byte,
        // last >> 12 | el << 60 (`utag`): a page-crossing access cannot match an entry of the
        // first page's set (`uti` puts pages P and P + 1 in different sets), so it misses
        debug_assert!(!UTLB_HASH);
        let ent = c.off_utlb + 16 * UT * k;
        assert!(ent + 16 * UT < 32768);
        if span > 1 {
            e.out.push(0x9100_0000 | ((span - 1) << 10) | (vreg << 5) | tmp2); // add x15, x13, #span-1
            e.out.push(0xd34c_fc00 | (tmp2 << 5) | tmp); // lsr x14, x15, #12
        } else {
            e.out.push(0xd34c_fc00 | (vreg << 5) | tmp); // lsr x14, x13, #12
        }
        e.out.push(0xd340_0000 | (12 << 16) | ((10 + UT.trailing_zeros()) << 10) | (vreg << 5) | 17); // ubfx x17, x13, #12, #log2(UT / 2): `uti`
        e.out.push(0x8b00_0000 | (17 << 16) | (5 << 10) | (X_CPU << 5) | 17); // add x17, x19, x17, lsl #5 (the set's way 0)
        if unpriv {
            // the EL0 entry, poisoned (bit 59) while they belong to another ASID
            e.out.push(ldr_x(tmp2, X_CPU, c.off_utlb_unpriv));
            e.out.push(0xaa00_0000 | (tmp2 << 16) | (tmp << 5) | tmp);
        } else if self.el != 0 {
            // orr x14, x14, #(el << 60): a logical immediate (bit 60, bit 61, or both)
            let (immr, imms) = match self.el { 1 => (4, 0), 2 => (3, 0), _ => (4, 1) };
            e.out.push(0xb240_0000 | (immr << 16) | (imms << 10) | (tmp << 5) | tmp);
        }
        e.out.push(ldr_x(tmp2, 17, ent));
        e.out.push(0xcb00_0000 | (tmp2 << 16) | (tmp << 5) | 16); // sub x16, x14, x15 (x14 keeps the tag)
        let site = e.out.len();
        e.out.push(0xb500_0000 | 16); // cbnz x16, way 1 / slow
        let w1 = if way1 { Some((site, e.out.len(), ent)) } else { slow.push(site); None };
        if excl_check {
            e.out.push(ldr_x(tmp, X_CPU, c.off_excl));
            slow.push(e.out.len()); e.out.push(0xb500_0000 | tmp); // cbnz x14, slow (reservation held)
        }
        // data entries hold PA - VA of the page (see `utlb_fill`)
        e.out.push(ldr_x(tmp, 17, ent + 8));
        e.out.push(0x8b00_0000 | (vreg << 16) | (tmp << 5) | tmp); // add x14, x14, x13: the PA
        if abs { e.out.push(0x8b00_0000 | (X_RAM << 16) | (tmp << 5) | tmp); } // add x14, x14, x21: the host address
        w1
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
        // works on the registers in `Cpu` (rare enough not to use the cache)
        e.ra_flush();
        e.ra_forget();
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
        self.emit_utlb(e, if load { 1 } else { 2 }, size, false, !load && o2, &mut slow, false, true);
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
        e.out.push(0x9100_0400 | (X_ICOUNT << 5) | X_ICOUNT); // add x20, x20, #1
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
            e.out.push(0x9100_0400 | (X_ICOUNT << 5) | X_ICOUNT); // add x20, x20, #1
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
        e.ra_flush();
        e.ra_forget();
        if rt == 31 { e.out.push(0xaa1f_03e0 | vreg); } else { e.out.push(ldr_x(vreg, X_CPU, e.reg_off(rt))); }
        e.out.push(0x9240_0000 | (58 << 16) | (57 << 10) | (vreg << 5) | vreg); // and x13, x13, #~63
        let mut slow: Vec<usize> = Vec::new();
        self.emit_utlb(e, 2, 0, false, true, &mut slow, false, true);
        for k in 0..4u32 { e.out.push(0xa900_0000 | ((2 * k) << 15) | (31 << 10) | (tmp << 5) | 31); } // stp xzr, xzr, [x14, #16k]
        e.out.push(0x9100_0400 | (X_ICOUNT << 5) | X_ICOUNT); // add x20, x20, #1
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

/// The shared stubs: (words, index of the exit stub, index of the enter stub).
///
/// Dispatcher: entered from a block's exit with pc in `Cpu`, the guest NZCV live in the host
/// flags (it touches none), FPSR in `Cpu`, x20 = instret and x14 = the mode bits of the block
/// key. Jumps to the next compiled block's linked entry, or stores NZCV and instret and
/// returns 0 to `jit_step`.
///
/// Exit: restores the registers the enter stub saved and returns w0 to Rust.
///
/// Enter, `fn(cpu, block) -> status`: saves x19..x30, sets x19 = cpu, x20 = instret,
/// x21 = the host address of guest RAM, and jumps to the block's entry.
fn stubs(c: &Ctx, front: u64) -> (Vec<u32>, usize, usize, usize, usize) {
    let probe = (u64::MAX, u64::MAX, 0usize);
    let base = &probe as *const _ as usize;
    let (o_pc, o_key, o_f) = (&probe.0 as *const _ as usize - base, &probe.1 as *const _ as usize - base, &probe.2 as *const _ as usize - base);
    assert_eq!(std::mem::size_of::<(u64, u64, usize)>(), 24);
    assert!(c.off_chain_stop < 65536 && c.off_utlb + 16 * UT < 32768 && c.off_nzcv < 16384 && [c.off_instret, c.off_pc, c.off_ram, c.off_exit, c.off_chain_until, c.off_interp, c.off_refill, c.off_fpsr].iter().all(|&o| o < 32768),
        "offsets: chain_stop {} utlb {} until {} disp {} pc {} instret {}", c.off_chain_stop, c.off_utlb, c.off_chain_until, c.off_disp, c.off_pc, c.off_instret);
    let mut o: Vec<u32> = Vec::new();
    let mut to_out: Vec<usize> = Vec::new(); // cbnz/cbz/tbz sites to patch to `out`
    let sub = |d: u32, a: u32, b: u32| 0xcb00_0000 | (b << 16) | (a << 5) | d;
    // budget: instret < chain_until (read each time: other threads zero it)
    o.push(ldr_x(10, X_CPU, c.off_chain_until));
    o.push(sub(9, X_ICOUNT, 10));
    to_out.push(o.len()); o.push(0x3600_0000 | (1 << 31) | (31 << 19) | 9); // tbz x9, #63, out
    o.push(0xd280_0000 | ((c.off_chain_stop as u32) << 5) | 9); // movz x9, #off(chain_stop)
    o.push(0x3860_6800 | (9 << 16) | (X_CPU << 5) | 9); // ldrb w9, [x19, x9]
    to_out.push(o.len()); o.push(0x3500_0000 | 9); // cbnz w9, out
    o.push(ldr_x(9, X_CPU, c.off_pc));
    o.push(0x9240_0400 | (9 << 5) | 10); // and x10, x9, #3
    to_out.push(o.len()); o.push(0xb500_0000 | 10); // cbnz x10, out
    // fetch micro-TLB, way 0: x11 = &utlb[0][uti(pc)], tag = pc >> 12 | el << 60
    o.push(0xd34c_fc00 | (9 << 5) | 10); // lsr x10, x9, #12
    o.push(0xd340_0000 | (12 << 16) | ((10 + UT.trailing_zeros()) << 10) | (9 << 5) | 11); // ubfx x11, x9, #12, #log2(UT / 2): `uti`
    o.push(0x8b00_0000 | (11 << 16) | (5 << 10) | (X_CPU << 5) | 11); // add x11, x19, x11, lsl #5 (the set)
    o.push(ldr_x(12, 11, c.off_utlb));
    o.push(0xd340_0000 | (56 << 16) | (57 << 10) | (14 << 5) | 13); // ubfx x13, x14, #56, #2 (el)
    o.push(0xaa00_0000 | (13 << 16) | (60 << 10) | (10 << 5) | 10); // orr x10, x10, x13, lsl #60
    o.push(sub(12, 12, 10));
    o.push(0xb400_0000 | (5 << 5) | 12); // cbz x12, +5 (way 0 holds it)
    o.push(ldr_x(12, 11, c.off_utlb + 16)); // way 1
    o.push(sub(12, 12, 10));
    to_out.push(o.len()); o.push(0xb500_0000 | 12); // cbnz x12, out
    o.push(0x9100_4000 | (11 << 5) | 11); // add x11, x11, #16
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
    o.push(sub(13, 13, 9));
    to_out.push(o.len()); o.push(0xb500_0000 | 13); // cbnz x13, out
    o.push(ldr_x(13, 11, o_key));
    o.push(sub(13, 13, 12));
    to_out.push(o.len()); o.push(0xb500_0000 | 13); // cbnz x13, out
    o.push(ldr_x(16, 11, o_f));
    if LINKED_ENTRY != 0 { o.push(0x9100_0000 | ((LINKED_ENTRY as u32) << 10) | (16 << 5) | 16); } // add x16, x16, #LINKED_ENTRY
    o.push(0xd61f_0200); // br x16
    // out: back to jit_step with the guest state in `Cpu`
    let out = o.len();
    for k in to_out {
        let d = (out - k) as u32;
        o[k] |= if o[k] & 0x7e00_0000 == 0x3600_0000 { (d & 0x3fff) << 5 } else { (d & 0x7ffff) << 5 };
    }
    o.push(MRS_NZCV_X9);
    o.push(str_w(9, X_CPU, c.off_nzcv));
    o.push(MRS_FPSR_X9);
    o.push(str_x(9, X_CPU, c.off_fpsr));
    o.push(str_x(X_ICOUNT, X_CPU, c.off_instret));
    o.push(0x5280_0000); // movz w0, #0
    // exit stub
    let exit = o.len();
    let pair = |op: u32, imm7: i32, a: u32, b: u32| op | (((imm7 as u32) & 0x7f) << 15) | (b << 10) | (31 << 5) | a;
    for (k, r) in [(2, 19), (4, 21), (6, 23), (8, 25), (10, 27)] { o.push(pair(0xa940_0000, k, r, r + 1)); } // ldp xr, xr+1, [sp, #8k]
    o.push(pair(0xa8c0_0000, 12, 29, 30)); // ldp x29, x30, [sp], #96
    o.push(0xd65f_03c0); // ret
    // enter stub
    let enter = o.len();
    o.push(pair(0xa980_0000, -12, 29, 30)); // stp x29, x30, [sp, #-96]!
    for (k, r) in [(2, 19), (4, 21), (6, 23), (8, 25), (10, 27)] { o.push(pair(0xa900_0000, k, r, r + 1)); } // stp xr, xr+1, [sp, #8k]
    o.push(0x9100_03fd); // mov x29, sp
    o.push(0xaa00_03f3); // mov x19, x0
    o.push(ldr_x(X_ICOUNT, X_CPU, c.off_instret));
    o.push(ldr_x(X_RAM, X_CPU, c.off_ram));
    // the guest flags live in the host's while generated code runs
    o.push(ldr_w(9, X_CPU, c.off_nzcv));
    o.push(MSR_NZCV_X9);
    o.push(ldr_x(9, X_CPU, c.off_fpsr));
    o.push(MSR_FPSR_X9);
    o.push(0xd61f_0020); // br x1
    // interpreter stub (blr from a block; x1 = the instruction's pc + 4, x14 = the block's mode
    // bits): instret and flags to `Cpu`, h_step, then status 0: flags back, return; 1: flags
    // back, the dispatcher; 2 (a trap): out to Rust
    let interp = o.len();
    o.push(str_x(X_ICOUNT, X_CPU, c.off_instret));
    o.push(MRS_NZCV_X9);
    o.push(str_w(9, X_CPU, c.off_nzcv));
    o.push(MRS_FPSR_X9);
    o.push(str_x(9, X_CPU, c.off_fpsr));
    o.push(pair(0xa980_0000, -2, 14, 30)); // stp x14, x30, [sp, #-16]!
    o.push(0xaa13_03e0); // mov x0, x19
    mov64(&mut o, 16, h_step as *const () as usize as u64);
    o.push(0xd63f_0200); // blr x16
    o.push(pair(0xa8c0_0000, 2, 14, 30)); // ldp x14, x30, [sp], #16
    o.push(ldr_x(X_ICOUNT, X_CPU, c.off_instret)); // now counting the instruction
    let nz = o.len();
    o.push(0x3500_0000); // cbnz w0, nz (patched)
    let load_flags = |o: &mut Vec<u32>| {
        o.push(ldr_w(9, X_CPU, c.off_nzcv));
        o.push(MSR_NZCV_X9);
        o.push(ldr_x(9, X_CPU, c.off_fpsr));
        o.push(MSR_FPSR_X9);
    };
    load_flags(&mut o);
    o.push(0xd65f_03c0); // ret
    let at = o.len();
    o[nz] |= (((at - nz) as u32) & 0x7ffff) << 5;
    o.push(0x7100_081f); // cmp w0, #2
    let trap = o.len();
    o.push(0x5400_0000); // b.eq trap (patched)
    load_flags(&mut o);
    o.push(ldr_x(16, X_CPU, c.off_disp));
    o.push(0xd61f_0200); // br x16
    let at = o.len();
    o[trap] |= (((at - trap) as u32) & 0x7ffff) << 5;
    o.push(0x5280_0020); // movz w0, #1
    o.push(ldr_x(16, X_CPU, c.off_exit));
    o.push(0xd61f_0200); // br x16
    // micro-TLB refill stub (blr from a load/store slow path; x13 = VA, x15 = h_tlb's argument):
    // keeps every host register but x16 (w16 = h_tlb's result) and x9-x12/x14/x15/x17. FPSR
    // (the guest's, live) is not saved: h_tlb (a table walk) must not do floating point.
    let refill = o.len();
    const FRAME: u32 = 352; // x0-x9, x13, x30, q16-q31
    o.push(0xd100_0000 | (FRAME << 10) | (31 << 5) | 31); // sub sp, sp, #FRAME
    o.push(MRS_NZCV_X9);
    let gp = [(0, 1), (2, 3), (4, 5), (6, 7), (8, 9), (13, 30)];
    for (k, &(a, b)) in gp.iter().enumerate() { o.push(pair(0xa900_0000, 2 * k as i32, a, b)); } // stp
    for k in 0..8u32 { o.push(0xad00_0000 | (((6 + 2 * k) & 0x7f) << 15) | ((17 + 2 * k) << 10) | (31 << 5) | (16 + 2 * k)); } // stp q, q, [sp, #96 + 32k]
    o.push(0xaa13_03e0); // mov x0, x19
    o.push(0xaa0d_03e1); // mov x1, x13
    o.push(0xaa0f_03e2); // mov x2, x15
    mov64(&mut o, 16, h_tlb as *const () as usize as u64);
    o.push(0xd63f_0200); // blr x16
    o.push(0x2a00_03f0); // mov w16, w0
    for k in 0..8u32 { o.push(0xad40_0000 | (((6 + 2 * k) & 0x7f) << 15) | ((17 + 2 * k) << 10) | (31 << 5) | (16 + 2 * k)); } // ldp q, q
    for (k, &(a, b)) in gp.iter().enumerate() { o.push(pair(0xa940_0000, 2 * k as i32, a, b)); } // ldp
    o.push(MSR_NZCV_X9);
    o.push(0x9100_0000 | (FRAME << 10) | (31 << 5) | 31); // add sp, sp, #FRAME
    o.push(0xd65f_03c0); // ret
    (o, exit, enter, interp, refill)
}

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

pub fn available() -> bool { host::AVAILABLE && host::supported() }
