//! A32 → A64 block JIT (AArch64 macOS/Linux hosts).
//!
//! A hot block of ARM-state code is translated into host code that works on the guest
//! registers in `Cpu::r` (loaded and stored per instruction) with the guest's NZCV held in
//! the host flags: A32 and A64 share the flag layout, the condition codes and the
//! add/subtract carry and overflow rules, so `ADDS`, `SUBS`, `ADCS`, `SBCS` and `B.cond`
//! compute exactly the architected result. Logical operations with S take their carry
//! from the shifter, which is computed separately.
//!
//! Translated natively: data processing with immediate or immediate-shifted operands,
//! MOVW/MOVT, multiplies, divides, CLZ, bitfield and extend instructions, REV, word, byte,
//! halfword and doubleword loads and stores (with writeback), LDM/STM, B, BL, BX, BLX and
//! hints/barriers. Loads and stores probe the core's micro-TLB inline (aligned accesses
//! within one page of RAM); a miss runs that instruction in the interpreter. Everything
//! else (system instructions, VFP/NEON, exclusives, register-shifted operands, PC writes
//! other than branches) calls the interpreter for that one instruction; the block ends
//! there if it changed the PC, the mode or interrupt masks, touched a device or raised an
//! exception.
//!
//! Blocks are keyed by virtual and physical PC and privilege and never cross a 4 KiB page.
//! Code is invalidated by writes: pages holding compiled code are kept out of the write
//! micro-TLB, so every store to them takes the slow path and drops the page's blocks on
//! every core (`CodeWatch`); devices that write RAM report it through `CodeWatch::dma_write`.
//! Thumb state is interpreted for now.
use crate::cpu::Cpu;
use crate::mmu::{utlb_table, Access, U_EXEC, U_READ, U_WRITE, UT};
use emu_core::{Bus, Trap};
use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, AtomicU8, Ordering::Relaxed};
use std::sync::{Arc, Mutex};

#[cfg(all(target_arch = "aarch64", any(target_os = "macos", target_os = "linux")))]
mod host {
    use std::ffi::{c_int, c_void};
    extern "C" {
        fn mmap(addr: *mut c_void, len: usize, prot: c_int, flags: c_int, fd: c_int, off: i64) -> *mut c_void;
        fn munmap(addr: *mut c_void, len: usize) -> c_int;
        #[cfg(target_os = "macos")]
        fn pthread_jit_write_protect_np(enabled: c_int);
        #[cfg(target_os = "macos")]
        fn sys_icache_invalidate(start: *mut c_void, len: usize);
        #[cfg(target_os = "linux")]
        fn __clear_cache(start: *mut c_void, end: *mut c_void);
    }

    /// Executable code buffer (bump allocated; reset as a whole).
    pub struct CodeCache {
        base: *mut u8,
        size: usize,
        used: usize,
    }

    impl CodeCache {
        pub fn new(size: usize) -> Option<CodeCache> {
            #[cfg(target_os = "macos")]
            let flags = 0x0002 | 0x1000 | 0x0800; // PRIVATE | ANON | JIT
            #[cfg(target_os = "linux")]
            let flags = 0x0002 | 0x0020; // PRIVATE | ANONYMOUS
            // SAFETY: anonymous mapping chosen by the OS; failure is checked
            let p = unsafe { mmap(std::ptr::null_mut(), size, 7, flags, -1, 0) };
            if p as isize == -1 || p.is_null() {
                return None;
            }
            Some(CodeCache { base: p as *mut u8, size, used: 0 })
        }
        pub fn reset(&mut self) {
            self.used = 0;
        }
        /// Copy machine code in; returns its entry address.
        pub fn write(&mut self, words: &[u32]) -> Option<*const u8> {
            let bytes = words.len() * 4;
            if self.used + bytes > self.size {
                return None;
            }
            let off = self.used;
            // SAFETY: the range is inside the mapping; writes are enabled per thread on macOS
            // and the instruction cache is synchronised for exactly the written range
            unsafe {
                #[cfg(target_os = "macos")]
                pthread_jit_write_protect_np(0);
                std::ptr::copy_nonoverlapping(words.as_ptr() as *const u8, self.base.add(off), bytes);
                #[cfg(target_os = "macos")]
                pthread_jit_write_protect_np(1);
                #[cfg(target_os = "macos")]
                sys_icache_invalidate(self.base.add(off) as *mut c_void, bytes);
                #[cfg(target_os = "linux")]
                __clear_cache(self.base.add(off) as *mut c_void, self.base.add(off + bytes) as *mut c_void);
            }
            self.used += bytes;
            Some(self.base.wrapping_add(off))
        }
    }

    impl Drop for CodeCache {
        fn drop(&mut self) {
            // SAFETY: the mapping was created by `new` with this size
            unsafe { munmap(self.base as *mut c_void, self.size) };
        }
    }

    pub const AVAILABLE: bool = true;
}

#[cfg(not(all(target_arch = "aarch64", any(target_os = "macos", target_os = "linux"))))]
mod host {
    pub struct CodeCache;
    impl CodeCache {
        pub fn new(_size: usize) -> Option<CodeCache> {
            None
        }
        pub fn reset(&mut self) {}
        pub fn write(&mut self, _w: &[u32]) -> Option<*const u8> {
            None
        }
    }
    pub const AVAILABLE: bool = false;
}

/// Whether this host can run compiled code.
pub fn available() -> bool {
    host::AVAILABLE
}

/// Physical pages of guest RAM that hold compiled code, shared by the cores of a machine.
/// A store that reaches such a page through the slow path (the only way: those pages are
/// kept out of the write micro-TLBs) or a device write reported by `dma_write` drops the
/// page's code on every core.
pub struct CodeWatch {
    base: u32,
    pages: Box<[AtomicU8]>,
    /// Changes whenever a page becomes code (write micro-TLBs must drop it) or loses it.
    epoch: AtomicU64,
    /// Per registered core: pages whose code it must drop (`u32::MAX`: drop everything).
    dirty: Mutex<Vec<Vec<u32>>>,
}

const DIRTY_MAX: usize = 4096;

impl CodeWatch {
    /// Watch `len` bytes of RAM at physical `base`.
    pub fn new(base: u32, len: u32) -> Arc<CodeWatch> {
        let pages = (0..len.div_ceil(4096)).map(|_| AtomicU8::new(0)).collect();
        Arc::new(CodeWatch { base, pages, epoch: AtomicU64::new(0), dirty: Mutex::new(Vec::new()) })
    }

    fn index(&self, pa: u32) -> Option<usize> {
        let i = (pa.wrapping_sub(self.base) >> 12) as usize;
        (i < self.pages.len()).then_some(i)
    }

    pub(crate) fn is_code(&self, pa: u32) -> bool {
        self.index(pa).is_some_and(|i| self.pages[i].load(Relaxed) != 0)
    }

    /// Record that the page holding `pa` has compiled code.
    fn mark(&self, pa: u32) {
        if let Some(i) = self.index(pa) {
            if self.pages[i].swap(1, Relaxed) == 0 {
                self.epoch.fetch_add(1, Relaxed);
            }
        }
    }

    /// The page holding `pa` was written: every core drops its code there.
    pub(crate) fn invalidate(&self, pa: u32) {
        let Some(i) = self.index(pa) else { return };
        if self.pages[i].swap(0, Relaxed) == 0 {
            return;
        }
        for list in self.dirty.lock().unwrap().iter_mut() {
            if list.len() < DIRTY_MAX {
                list.push(pa >> 12);
            } else if list.last() != Some(&u32::MAX) {
                list.push(u32::MAX);
            }
        }
        self.epoch.fetch_add(1, Relaxed);
    }

    /// A device wrote `len` bytes of guest RAM at physical `pa` (DMA).
    pub fn dma_write(&self, pa: u64, len: usize) {
        if len == 0 || pa > u32::MAX as u64 {
            return;
        }
        let last = (pa + len as u64 - 1).min(u32::MAX as u64);
        for page in pa >> 12..=last >> 12 {
            let a = (page << 12) as u32;
            if self.is_code(a) {
                self.invalidate(a);
            }
        }
    }

    fn register(&self) -> usize {
        let mut d = self.dirty.lock().unwrap();
        d.push(Vec::new());
        d.len() - 1
    }

    fn take(&self, slot: usize) -> Vec<u32> {
        std::mem::take(&mut self.dirty.lock().unwrap()[slot])
    }
}

/// Compiled block entry: `fn(cpu) -> status` (0 = continue at `r[15]`, 1 = `jit_trap`).
type BlockFn = unsafe extern "C" fn(*mut Cpu) -> u32;

/// A front-cache entry (the dispatcher reads it by these offsets: 0, 8, 16).
#[repr(C)]
#[derive(Clone, Copy)]
pub struct Front {
    pc: u32,
    key: u64,
    entry: usize,
}

const NO_FRONT: Front = Front { pc: u32::MAX, key: 0, entry: 0 };

/// Words of the block prologue; chained entries start after it.
const PROLOGUE_WORDS: usize = 7;

const MAX_INSNS: u32 = 64;
const CODE_BYTES: usize = 64 << 20;
const FRONT: usize = 1 << 14;
const HOTC: usize = 1 << 14;

/// Block key beyond the virtual PC: physical PC and privilege.
fn key(pa: u32, privileged: bool, thumb: bool) -> u64 {
    pa as u64 | (privileged as u64) << 32 | (thumb as u64) << 33
}

pub struct Jit {
    code: host::CodeCache,
    blocks: HashMap<(u32, u64), BlockFn>,
    /// Direct-mapped cache over `blocks`: (pc, key, entry).
    front: Box<[Front]>,
    /// The chaining dispatcher (in `code`; rewritten after a flush). 0: chaining off.
    disp: usize,
    chain: bool,
    /// Execution counts of blocks not compiled yet: (pc, key, count).
    hotc: Box<[(u32, u64, u32)]>,
    /// Physical page -> blocks compiled from it.
    pages: HashMap<u32, Vec<(u32, u64)>>,
    pub(crate) watch: Arc<CodeWatch>,
    slot: usize,
    seen_epoch: u64,
    hot: u32,
    pub compiled: u64,
    pub execs: u64,
    pub invalidated: u64,
}

impl Jit {
    fn new(watch: Arc<CodeWatch>) -> Option<Box<Jit>> {
        let code = host::CodeCache::new(CODE_BYTES)?;
        let slot = watch.register();
        let hot = std::env::var("SBC_JIT_HOT").ok().and_then(|v| v.parse().ok()).unwrap_or(8);
        let mut jit = Box::new(Jit {
            code,
            blocks: HashMap::new(),
            front: vec![NO_FRONT; FRONT].into_boxed_slice(),
            disp: 0,
            chain: std::env::var("SBC_JIT_CHAIN").map_or(true, |v| v != "0"),
            hotc: vec![(u32::MAX, 0, 0); HOTC].into_boxed_slice(),
            pages: HashMap::new(),
            seen_epoch: watch.epoch.load(Relaxed),
            watch,
            slot,
            hot,
            compiled: 0,
            execs: 0,
            invalidated: 0,
        });
        jit.write_dispatcher();
        Some(jit)
    }

    /// (Re)write the dispatcher at the start of the code buffer.
    fn write_dispatcher(&mut self) {
        self.disp = if self.chain { self.code.write(&dispatcher(&ctx())).map_or(0, |p| p as usize) } else { 0 };
    }

    fn flush(&mut self) {
        self.code.reset();
        self.write_dispatcher();
        self.blocks.clear();
        self.pages.clear();
        self.front.fill(NO_FRONT);
        self.hotc.fill((u32::MAX, 0, 0));
    }

    fn drop_page(&mut self, page: u32) {
        for k in self.pages.remove(&page).unwrap_or_default() {
            self.blocks.remove(&k);
            let f = &mut self.front[front_slot(k.0)];
            if f.pc == k.0 && f.key == k.1 {
                *f = NO_FRONT;
            }
            self.invalidated += 1;
        }
    }
}

fn front_slot(pc: u32) -> usize {
    ((pc ^ (pc >> 14)) as usize >> 2) & (FRONT - 1)
}

fn hot_slot(pc: u32) -> usize {
    ((pc ^ (pc >> 16)) as usize >> 2) & (HOTC - 1)
}

impl Cpu {
    /// Compile hot ARM-state code (needs `set_ram` first and an AArch64 host). `watch`
    /// is shared by all cores of the machine. Returns whether the JIT is now on.
    pub fn enable_jit(&mut self, watch: Arc<CodeWatch>) -> bool {
        if self.ram.is_null() || !host::AVAILABLE {
            return false;
        }
        self.jit = Jit::new(watch);
        self.utlb_flush();
        self.jit.is_some()
    }

    /// Back to the interpreter alone (compiled code is dropped).
    pub fn disable_jit(&mut self) {
        self.jit = None;
        self.interp_run = false;
        self.utlb_flush();
    }

    /// Compile a block once it has started `hot` times (tests use 1).
    pub fn jit_set_hot(&mut self, hot: u32) {
        if let Some(j) = self.jit.as_mut() {
            j.hot = hot.max(1);
        }
    }

    /// (blocks compiled, blocks run, blocks invalidated by writes).
    pub fn jit_stats(&self) -> Option<(u64, u64, u64)> {
        self.jit.as_ref().map(|j| (j.compiled, j.execs, j.invalidated))
    }

    /// Apply other cores' (and devices') code invalidations.
    #[cold]
    fn jit_sync(&mut self, epoch: u64) {
        self.utlb_flush_writes();
        let Some(jit) = self.jit.as_mut() else { return };
        jit.seen_epoch = epoch;
        let pages = jit.watch.take(jit.slot);
        if pages.contains(&u32::MAX) {
            jit.flush();
            return;
        }
        for page in pages {
            jit.drop_page(page);
        }
    }

    /// Run a compiled block at the current PC if there is one (compiling hot code).
    /// None: not handled here, the caller interprets one instruction.
    /// Apply code invalidations and new code pages announced since the last step (before
    /// any store of this step can use a stale write micro-TLB entry).
    #[inline(always)]
    pub(crate) fn jit_poll(&mut self) {
        if let Some(j) = self.jit.as_ref() {
            let epoch = j.watch.epoch.load(Relaxed);
            if epoch != j.seen_epoch {
                self.jit_sync(epoch);
            }
        }
    }

    pub(crate) fn jit_step(&mut self, bus: &mut dyn Bus) -> Option<Result<(), Trap>> {
        // inside an IT block: the interpreter runs it out
        if self.cpsr & 0x0600_fc00 != 0 {
            return None;
        }
        let pc = self.r[15];
        let privileged = self.privileged();
        let slot = utlb_table(privileged, U_EXEC) * UT + (pc >> 12) as usize % UT;
        if self.utlb[slot].tag != (pc & !0xfff) as u64 {
            // fill the fetch micro-TLB; a fault is left to the interpreter, which takes it
            let pa = self.translate_fill(bus, pc, Access::Execute).ok()?;
            if !self.in_ram(pa) || self.utlb[slot].tag != (pc & !0xfff) as u64 {
                return None;
            }
        }
        let host = self.utlb[slot].delta.wrapping_add(pc as u64);
        let pa = (host - self.ram as u64) as u32 + self.ram_base;
        let thumb = self.thumb();
        let k = (pc, key(pa, privileged, thumb));
        let jit = self.jit.as_mut()?;
        let fs = front_slot(pc);
        let f: BlockFn = if jit.front[fs].pc == k.0 && jit.front[fs].key == k.1 {
            // SAFETY: front entries are only filled from `blocks`, which hold published code
            unsafe { std::mem::transmute::<usize, BlockFn>(jit.front[fs].entry) }
        } else if let Some(&f) = jit.blocks.get(&k) {
            jit.front[fs] = Front { pc: k.0, key: k.1, entry: f as usize };
            f
        } else {
            let h = &mut jit.hotc[hot_slot(pc)];
            if h.0 == k.0 && h.1 == k.1 {
                h.2 = h.2.saturating_add(1);
            } else {
                *h = (k.0, k.1, 1);
            }
            if h.2 < jit.hot {
                self.interp_run = true;
                return None;
            }
            let f = self.compile(pc, pa, privileged, thumb, host as *const u8)?;
            let jit = self.jit.as_mut()?;
            jit.blocks.insert(k, f);
            jit.pages.entry(pa >> 12).or_default().push(k);
            jit.front[fs] = Front { pc: k.0, key: k.1, entry: f as usize };
            jit.compiled += 1;
            if !jit.watch.is_code(pa) {
                jit.watch.mark(pa);
                // this core's own stores to the page must take the slow path at once (the
                // other cores drop their write entries when they see the new epoch)
                self.utlb_flush_writes();
            }
            f
        };
        Some(self.jit_run(f, bus))
    }

    fn jit_run(&mut self, f: BlockFn, bus: &mut dyn Bus) -> Result<(), Trap> {
        // SAFETY: the bus outlives the call; compiled code reaches it only through h_step
        self.jit_bus = Some(unsafe { std::mem::transmute::<&mut dyn Bus, &'static mut dyn Bus>(bus) } as *mut dyn Bus);
        // SAFETY: `f` is published code generated for this Cpu layout
        if let Some(j) = self.jit.as_ref() {
            self.jit_disp = j.disp;
            self.jit_front = j.front.as_ptr();
        }
        let r = unsafe { f(self as *mut Cpu) };
        self.jit_bus = None;
        if let Some(j) = self.jit.as_mut() {
            j.execs += 1;
        }
        if r == 0 {
            Ok(())
        } else {
            Err(self.jit_trap.take().unwrap_or(Trap::Wait))
        }
    }

    /// Translate the block at virtual `pc` (physical `pa`, host address `code`).
    fn compile(&mut self, pc: u32, pa: u32, privileged: bool, thumb: bool, code: *const u8) -> Option<BlockFn> {
        let c = ctx();
        let mut e = Emit::new(&c);
        e.chain = self.jit.as_ref().is_some_and(|j| j.disp != 0);
        e.prologue(privileged);
        assert_eq!(e.out.len(), PROLOGUE_WORDS + 3);
        let mut n = 0u32;
        if thumb {
            e.thumb = true;
            let mut at = pc;
            loop {
                let off = at.wrapping_sub(pc) as usize;
                // SAFETY: `code` maps the rest of this RAM page; the loop stops at its end
                let hw1 = unsafe { (code.add(off) as *const u16).read() } as u32;
                let wide = hw1 >> 11 >= 0b11101;
                if wide && at & 0xfff == 0xffe {
                    // straddles the page: leave it to the interpreter
                    if n == 0 {
                        return None;
                    }
                    e.exit_const(at, n);
                    break;
                }
                // SAFETY: as above (the second halfword is in the same page)
                let hw2 = if wide { u32::from(unsafe { (code.add(off + 2) as *const u16).read() }) } else { 0 };
                let end = e.thumb_insn(hw1, hw2, at, n);
                n += 1;
                at = at.wrapping_add(if wide { 4 } else { 2 });
                if end {
                    break;
                }
                if n >= MAX_INSNS || at & 0xfff == 0 {
                    e.exit_const(at, n);
                    break;
                }
            }
        }
        while !thumb {
            let at = pc.wrapping_add(4 * n);
            // SAFETY: `code` maps the rest of this RAM page; the loop stops at its end
            let insn = unsafe { (code.add(4 * n as usize) as *const u32).read() };
            let end = e.insn(insn, at, n);
            n += 1;
            if end {
                break;
            }
            let next = at.wrapping_add(4);
            if n >= MAX_INSNS || next & 0xfff == 0 {
                e.exit_const(next, n);
                break;
            }
        }
        let words = e.finish();
        let jit = self.jit.as_mut()?;
        let entry = match jit.code.write(&words) {
            Some(p) => p,
            None => {
                jit.flush();
                jit.code.write(&words)?
            }
        };
        let _ = pa;
        // SAFETY: `entry` is the start of the code just written, which follows the BlockFn ABI
        Some(unsafe { std::mem::transmute::<*const u8, BlockFn>(entry) })
    }
}

/// Run one instruction (at `pc`, the `before`-th of its block) in the interpreter for
/// compiled code. Returns 0 to continue the block, 1 to leave it (the PC is set) and 2 to
/// leave it with `jit_trap`. `last` = the block ends here in any case. While the block
/// continues, `instret` is left as it was (the block counts its instructions at its exit);
/// on leaving, it counts this instruction and the `before` ones.
unsafe extern "C" fn h_step(cpu: *mut Cpu, pc: u32, before: u32, flags: u32) -> u32 {
    let cpu = &mut *cpu;
    let bus = &mut *cpu.jit_bus.expect("bus");
    let last = flags & 1;
    let next = pc.wrapping_add(if flags & 8 != 0 { 2 } else { 4 });
    if PROFILE.load(Relaxed) {
        profile(cpu, pc, (flags >> 1) & 1);
    }
    let start = cpu.instret;
    cpu.instret = start.wrapping_add(before as u64);
    cpu.r[15] = pc;
    let state = cpu.cpsr & !0xf80f_0000;
    let epoch = cpu.jit.as_ref().map_or(0, |j| j.watch.epoch.load(Relaxed));
    cpu.io = false;
    match cpu.exec_one(bus) {
        Ok(()) => {
            let same = cpu.r[15] == next
                && cpu.cpsr & !0xf80f_0000 == state
                && !cpu.io
                && !cpu.tlb_broadcast
                && !cpu.sent_event
                && cpu.jit.as_ref().map_or(0, |j| j.watch.epoch.load(Relaxed)) == epoch;
            if last == 0 && same {
                cpu.instret = start;
                0
            } else {
                1
            }
        }
        Err(t) => {
            cpu.jit_trap = Some(t);
            2
        }
    }
}

static PROFILE: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
/// (instruction class, slow path) -> (count, example word)
static FALLBACKS: Mutex<Option<HashMap<(u32, u32), (u64, u32)>>> = Mutex::new(None);

/// Count the instructions compiled code hands to the interpreter (`profile_fallbacks`).
pub fn set_profile(on: bool) {
    PROFILE.store(on, Relaxed);
}

/// The most frequent interpreted instructions: (example word, micro-TLB miss, count).
pub fn profile_fallbacks() -> Vec<(u32, bool, u64)> {
    let map = FALLBACKS.lock().unwrap();
    let mut v: Vec<_> = map.iter().flatten().map(|(&(_, slow), &(n, w))| (w, slow == 1, n)).collect();
    v.sort_by(|a, b| b.2.cmp(&a.2));
    v
}

#[cold]
fn profile(cpu: &Cpu, pc: u32, why: u32) {
    let word = cpu
        .utlb
        .get(utlb_table(cpu.privileged(), U_EXEC) * UT + (pc >> 12) as usize % UT)
        .filter(|e| e.tag == (pc & !0xfff) as u64)
        // SAFETY: the entry maps this page of RAM
        .map_or(0, |e| unsafe { (e.delta.wrapping_add(pc as u64) as *const u32).read() });
    let (word, class) = if cpu.thumb() {
        let w = word << 16 | word >> 16;
        if w >> 27 >= 0b11101 { (w, w & 0xfff0_f000) } else { (w >> 16, w >> 16 & 0xff00 | 1) }
    } else {
        (word, word & 0x0ff0_00f0)
    };
    let mut map = FALLBACKS.lock().unwrap();
    let e = map.get_or_insert_with(HashMap::new).entry((class, why)).or_insert((0, word));
    e.0 += 1;
}

/// Offsets of the Cpu fields compiled code uses.
struct Ctx {
    r: usize,
    cpsr: usize,
    instret: usize,
    utlb: usize,
    tpidrurw: usize,
    tpidrro: usize,
    tpidrprw: usize,
    disp: usize,
    front: usize,
    chain_until: usize,
    ram: usize,
    ram_base: usize,
}

fn ctx() -> Ctx {
    Ctx {
        r: std::mem::offset_of!(Cpu, r),
        cpsr: std::mem::offset_of!(Cpu, cpsr),
        instret: std::mem::offset_of!(Cpu, instret),
        utlb: std::mem::offset_of!(Cpu, utlb),
        tpidrurw: std::mem::offset_of!(Cpu, tpidrurw),
        tpidrro: std::mem::offset_of!(Cpu, tpidrro),
        tpidrprw: std::mem::offset_of!(Cpu, tpidrprw),
        disp: std::mem::offset_of!(Cpu, jit_disp),
        front: std::mem::offset_of!(Cpu, jit_front),
        chain_until: std::mem::offset_of!(Cpu, chain_until),
        ram: std::mem::offset_of!(Cpu, ram),
        ram_base: std::mem::offset_of!(Cpu, ram_base),
    }
}

// Host registers: x19 = Cpu, x20 = micro-TLB base, x21/x22 = this privilege's read/write
// tables (callee-saved). Scratch: w9 (Rn), w10 (operand 2), w11 (result), w12 (address),
// w13/w14 (data), w15-w17 (probe, flags).
const X_CPU: u32 = 19;
const X_RTAB: u32 = 21;
const X_WTAB: u32 = 22;
const W_RN: u32 = 9;
const W_OP: u32 = 10;
const W_RES: u32 = 11;
const W_ADDR: u32 = 12;
const W_DATA: u32 = 13;
const W_SRC: u32 = 14;
const ZR: u32 = 31;

fn ldr_w(rt: u32, rn: u32, off: usize) -> u32 {
    assert!(off % 4 == 0 && off < 16384);
    0xb940_0000 | ((off / 4) as u32) << 10 | rn << 5 | rt
}
fn str_w(rt: u32, rn: u32, off: usize) -> u32 {
    assert!(off % 4 == 0 && off < 16384);
    0xb900_0000 | ((off / 4) as u32) << 10 | rn << 5 | rt
}
fn ldr_x(rt: u32, rn: u32, off: usize) -> u32 {
    assert!(off % 8 == 0 && off < 32768);
    0xf940_0000 | ((off / 8) as u32) << 10 | rn << 5 | rt
}
fn str_x(rt: u32, rn: u32, off: usize) -> u32 {
    assert!(off % 8 == 0 && off < 32768);
    0xf900_0000 | ((off / 8) as u32) << 10 | rn << 5 | rt
}
/// `<op> wd, wn, wm` for the shifted/extended-register and two-source data-processing forms.
fn rrr(base: u32, rd: u32, rn: u32, rm: u32) -> u32 {
    base | rm << 16 | rn << 5 | rd
}
fn ubfm_w(rd: u32, rn: u32, immr: u32, imms: u32) -> u32 {
    0x5300_0000 | immr << 16 | imms << 10 | rn << 5 | rd
}
fn sbfm_w(rd: u32, rn: u32, immr: u32, imms: u32) -> u32 {
    0x1300_0000 | immr << 16 | imms << 10 | rn << 5 | rd
}
fn bfm_w(rd: u32, rn: u32, immr: u32, imms: u32) -> u32 {
    0x3300_0000 | immr << 16 | imms << 10 | rn << 5 | rd
}
fn lsl_w(rd: u32, rn: u32, n: u32) -> u32 {
    ubfm_w(rd, rn, (32 - n) & 31, 31 - n)
}
fn lsr_w(rd: u32, rn: u32, n: u32) -> u32 {
    ubfm_w(rd, rn, n, 31)
}
fn asr_w(rd: u32, rn: u32, n: u32) -> u32 {
    sbfm_w(rd, rn, n, 31)
}
fn ror_w(rd: u32, rn: u32, n: u32) -> u32 {
    0x1380_0000 | rn << 16 | n << 10 | rn << 5 | rd // EXTR wd, wn, wn, #n
}
fn mov_w(rd: u32, rm: u32) -> u32 {
    0x2a00_03e0 | rm << 16 | rd
}
/// ADD/SUB (immediate, 12 bits) on W registers.
fn addi_w(rd: u32, rn: u32, imm: u32, sub: bool) -> u32 {
    assert!(imm < 4096);
    (if sub { 0x5100_0000 } else { 0x1100_0000 }) | imm << 10 | rn << 5 | rd
}
const MRS_NZCV: u32 = 0xd53b_4200;
const MSR_NZCV: u32 = 0xd51b_4200;

/// The 32-bit logical-immediate encoding (N:immr:imms in bits 22..10) of `v`, if any.
fn logic_imm32(v: u32) -> Option<u32> {
    if v == 0 || v == u32::MAX {
        return None;
    }
    for size in [32u32, 16, 8, 4, 2] {
        let mask = if size == 32 { u32::MAX } else { (1 << size) - 1 };
        let elem = v & mask;
        if (0..32 / size).any(|i| (v >> (i * size)) & mask != elem) {
            continue;
        }
        for r in 0..size {
            let rot = if r == 0 { elem } else { ((elem >> r) | (elem << (size - r))) & mask };
            if rot & rot.wrapping_add(1) == 0 && rot != 0 {
                let ones = rot.count_ones();
                let immr = (size - r) % size;
                let imms = (!(size * 2 - 1) & 0x3f) | (ones - 1);
                return Some(immr << 16 | imms << 10);
            }
        }
    }
    None
}
fn and_imm(rd: u32, rn: u32, v: u32) -> u32 {
    0x1200_0000 | logic_imm32(v).expect("logical immediate") | rn << 5 | rd
}
fn orr_imm(rd: u32, rn: u32, v: u32) -> u32 {
    0x3200_0000 | logic_imm32(v).expect("logical immediate") | rn << 5 | rd
}

/// Where a logical instruction's carry comes from.
#[derive(Clone, Copy)]
enum Carry {
    Unchanged,
    Const(bool),
    /// bit `.1` of host register `.0`
    Bit(u32, u32),
}

#[derive(Clone, Copy)]
enum Fix {
    /// branch at this word index to a label
    Branch(usize, usize),
}

struct Slow {
    label: usize,
    pc: u32,
    before: u32,
    resume: usize,
    size: u32,
}

struct Emit<'a> {
    out: Vec<u32>,
    c: &'a Ctx,
    labels: Vec<Option<usize>>,
    fixes: Vec<Fix>,
    slow: Vec<Slow>,
    /// leave after h_step: status = w0 - 1
    exit_h: usize,
    /// native exits continue through the dispatcher
    chain: bool,
    /// compiling Thumb code (PC reads are the address + 4, literal bases are aligned)
    thumb: bool,
    /// size of the instruction being emitted (2 or 4)
    size: u32,
}

impl<'a> Emit<'a> {
    fn new(c: &'a Ctx) -> Self {
        let mut e = Emit { out: Vec::with_capacity(512), c, labels: Vec::new(), fixes: Vec::new(), slow: Vec::new(), exit_h: 0, chain: false, thumb: false, size: 4 };
        e.exit_h = e.label();
        e
    }

    fn p(&mut self, w: u32) {
        self.out.push(w);
    }
    fn label(&mut self) -> usize {
        self.labels.push(None);
        self.labels.len() - 1
    }
    fn bind(&mut self, l: usize) {
        self.labels[l] = Some(self.out.len());
    }
    /// A branch-type instruction (B, B.cond, CBZ/CBNZ, TBZ/TBNZ) to label `l`.
    fn br(&mut self, w: u32, l: usize) {
        self.fixes.push(Fix::Branch(self.out.len(), l));
        self.out.push(w);
    }

    fn mov32(&mut self, rd: u32, v: u32) {
        if v >> 16 == 0xffff {
            self.p(0x1280_0000 | (!v & 0xffff) << 5 | rd); // movn
        } else {
            self.p(0x5280_0000 | (v & 0xffff) << 5 | rd); // movz
            if v >> 16 != 0 {
                self.p(0x72a0_0000 | (v >> 16) << 5 | rd); // movk #hi, lsl 16
            }
        }
    }
    fn mov64(&mut self, rd: u32, v: u64) {
        self.p(0xd280_0000 | ((v & 0xffff) as u32) << 5 | rd);
        for hw in 1..4u32 {
            let part = ((v >> (16 * hw)) & 0xffff) as u32;
            if part != 0 {
                self.p(0xf280_0000 | hw << 21 | part << 5 | rd);
            }
        }
    }

    fn reg_off(&self, n: u32) -> usize {
        self.c.r + 4 * n as usize
    }
    /// Guest register `n` (PC reads give `pc + 8`) into host `rd`.
    fn load(&mut self, rd: u32, n: u32, pc: u32) {
        if n == 15 {
            self.mov32(rd, pc.wrapping_add(if self.thumb { 4 } else { 8 }));
        } else {
            self.p(ldr_w(rd, X_CPU, self.reg_off(n)));
        }
    }
    fn store(&mut self, rs: u32, n: u32) {
        debug_assert!(n != 15);
        self.p(str_w(rs, X_CPU, self.reg_off(n)));
    }

    fn prologue(&mut self, privileged: bool) {
        self.p(0xa9bd_7bfd); // stp x29, x30, [sp, #-48]!
        self.p(0xa901_53f3); // stp x19, x20, [sp, #16]
        self.p(0xa902_5bf5); // stp x21, x22, [sp, #32]
        self.p(0xaa00_03f3); // mov x19, x0
        self.p(ldr_x(20, X_CPU, self.c.utlb));
        // each table is UT * 16 = 4 KiB: add xd, x20, #table, lsl #12
        let r = utlb_table(privileged, U_READ) as u32;
        let w = utlb_table(privileged, U_WRITE) as u32;
        self.p(0x9140_0000 | r << 10 | 20 << 5 | X_RTAB);
        self.p(0x9140_0000 | w << 10 | 20 << 5 | X_WTAB);
        self.load_flags();
    }

    fn epilogue(&mut self, status: u32) {
        self.p(0x5280_0000 | status << 5); // movz w0, #status
        self.p(0xa942_5bf5); // ldp x21, x22, [sp, #32]
        self.p(0xa941_53f3); // ldp x19, x20, [sp, #16]
        self.p(0xa8c3_7bfd); // ldp x29, x30, [sp], #48
        self.p(0xd65f_03c0); // ret
    }

    /// Guest NZCV (host flags) into CPSR[31:28].
    fn save_flags(&mut self) {
        self.p(MRS_NZCV | 15);
        self.p(ldr_w(16, X_CPU, self.c.cpsr));
        self.p(bfm_w(15, 16, 0, 27)); // bfxil w15, w16, #0, #28
        self.p(str_w(15, X_CPU, self.c.cpsr));
    }
    fn load_flags(&mut self) {
        self.p(ldr_w(15, X_CPU, self.c.cpsr));
        self.p(and_imm(15, 15, 0xf000_0000));
        self.p(MSR_NZCV | 15);
    }
    fn add_instret(&mut self, n: u32) {
        self.p(ldr_x(15, X_CPU, self.c.instret));
        self.p(0x9100_0000 | n << 10 | 15 << 5 | 15); // add x15, x15, #n
        self.p(str_x(15, X_CPU, self.c.instret));
    }

    /// Continue at `r[15]` (flags saved, instructions counted): the next compiled block
    /// through the dispatcher, or back to the machine.
    fn leave(&mut self) {
        if self.chain {
            self.p(ldr_x(16, X_CPU, self.c.disp));
            self.p(0xd61f_0200); // br x16
        } else {
            self.epilogue(0);
        }
    }

    /// Leave for constant `target` after `count` instructions of the block.
    fn exit_const(&mut self, target: u32, count: u32) {
        self.mov32(15, target);
        self.p(str_w(15, X_CPU, self.reg_off(15)));
        self.save_flags();
        self.add_instret(count);
        self.leave();
    }

    /// Leave for the address in host `r` with BXWritePC semantics (bit 0 selects Thumb).
    fn exit_bx(&mut self, r: u32, count: u32) {
        let thumb = self.label();
        self.br(0x3700_0000 | r, thumb); // tbnz wr, #0, thumb
        self.p(and_imm(r, r, 0xffff_fffc));
        self.p(str_w(r, X_CPU, self.reg_off(15)));
        if self.thumb {
            // to ARM state
            self.p(ldr_w(15, X_CPU, self.c.cpsr));
            self.p(and_imm(15, 15, !(1 << 5)));
            self.p(str_w(15, X_CPU, self.c.cpsr));
        }
        self.save_flags();
        self.add_instret(count);
        self.leave();
        self.bind(thumb);
        self.p(and_imm(r, r, 0xffff_fffe));
        self.p(str_w(r, X_CPU, self.reg_off(15)));
        if !self.thumb {
            self.set_thumb();
        }
        self.save_flags();
        self.add_instret(count);
        self.epilogue(0);
    }

    fn set_thumb(&mut self) {
        self.p(ldr_w(15, X_CPU, self.c.cpsr));
        self.p(orr_imm(15, 15, 1 << 5));
        self.p(str_w(15, X_CPU, self.c.cpsr));
    }

    /// Call the interpreter for the instruction at `pc` (see `h_step`).
    fn call_interp(&mut self, pc: u32, before: u32, last: bool, why: u32) {
        self.save_flags();
        self.p(0xaa13_03e0); // mov x0, x19
        self.mov32(1, pc);
        self.mov32(2, before);
        self.mov32(3, last as u32 | why << 1 | u32::from(self.size == 2) << 3);
        self.mov64(16, h_step as *const () as usize as u64);
        self.p(0xd63f_0200); // blr x16
        if last {
            let l = self.exit_h;
            self.br(0x1400_0000, l);
        } else {
            let l = self.exit_h;
            self.br(0x3500_0000, l); // cbnz w0, exit_h
            self.load_flags();
        }
    }

    /// Probe the micro-TLB for an aligned `size`-byte access at the address in host `a`;
    /// on a hit x16 holds the delta to add to it, otherwise branch to `slow`.
    fn probe(&mut self, a: u32, size: u32, write: bool, slow: usize) {
        self.p(and_imm(17, a, 0xffff_f000 | (size - 1)));
        self.p(ubfm_w(16, a, 12, 19)); // ubfx w16, wa, #12, #8
        let t = if write { X_WTAB } else { X_RTAB };
        self.p(0x8b00_1000 | 16 << 16 | t << 5 | 16); // add x16, xt, x16, lsl #4
        self.p(0xa940_420f); // ldp x15, x16, [x16]
        self.p(0xca11_01ef); // eor x15, x15, x17
        self.br(0xb500_000f, slow); // cbnz x15, slow
    }

    /// Probe for two words at the 4-aligned address in host `a` (LDRD/STRD): both in one
    /// page; on a hit x16 holds the delta.
    fn probe_pair(&mut self, a: u32, write: bool, slow: usize) {
        self.p(addi_w(15, a, 4, false));
        self.p(rrr(0x4a00_0000, 15, 15, a)); // eor w15, w15, wa
        self.p(lsr_w(15, 15, 12));
        self.br(0x3500_000f, slow); // cbnz w15, slow
        self.probe(a, 4, write, slow);
    }

    /// A slow path for the current instruction: interpret it, then resume after it.
    fn slow_path(&mut self, pc: u32, before: u32) -> (usize, usize) {
        let label = self.label();
        let resume = self.label();
        self.slow.push(Slow { label, pc, before, resume, size: self.size });
        (label, resume)
    }

    fn finish(mut self) -> Vec<u32> {
        let slow = std::mem::take(&mut self.slow);
        for s in slow {
            self.bind(s.label);
            self.size = s.size;
            self.call_interp(s.pc, s.before, false, 1);
            self.br(0x1400_0000, s.resume);
        }
        let l = self.exit_h;
        self.bind(l);
        self.p(0x5100_0400); // sub w0, w0, #1
        self.p(0xa942_5bf5);
        self.p(0xa941_53f3);
        self.p(0xa8c3_7bfd);
        self.p(0xd65f_03c0);
        for f in &self.fixes {
            let Fix::Branch(at, l) = *f;
            let target = self.labels[l].expect("unbound label");
            let off = target as i64 - at as i64;
            let w = self.out[at];
            self.out[at] = if w & 0xfc00_0000 == 0x1400_0000 {
                w | (off as u32 & 0x03ff_ffff)
            } else if w & 0x7e00_0000 == 0x3600_0000 {
                assert!((-(1 << 13)..(1 << 13)).contains(&off));
                w | (off as u32 & 0x3fff) << 5
            } else {
                assert!((-(1 << 18)..(1 << 18)).contains(&off));
                w | (off as u32 & 0x7_ffff) << 5
            };
        }
        self.out
    }

    /// Emit guest instruction `insn` at `pc`, the `n`-th of the block. Returns true when
    /// the block ends with it.
    fn insn(&mut self, insn: u32, pc: u32, n: u32) -> bool {
        let cond = insn >> 28;
        if cond == 15 {
            return self.unconditional(insn, pc, n);
        }
        let skip = (cond != 14).then(|| {
            let l = self.label();
            self.br(0x5400_0000 | (cond ^ 1), l); // b.<inverse> skip
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
                self.bind(l);
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
            self.mov32(15, pc.wrapping_add(4));
            self.p(str_w(15, X_CPU, self.reg_off(14)));
            self.mov32(15, target & !1);
            self.p(str_w(15, X_CPU, self.reg_off(15)));
            self.set_thumb();
            self.save_flags();
            self.add_instret(n + 1);
            self.epilogue(0);
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
            return false;
        }
        // CPS, RFE, SRS change the mode; everything else (NEON, CLREX) continues unless
        // the interpreter says otherwise
        let end = insn & 0x0ff1_fe20 == 0x0100_0000 || insn & 0x0e50_ffff == 0x0810_0a00;
        self.call_interp(pc, n, end, 0);
        end
    }

    /// Native translation of a conditional-space instruction: Some(block ends) or None
    /// (interpret it).
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
                self.mov32(15, pc.wrapping_add(4));
                self.p(str_w(15, X_CPU, self.reg_off(14)));
            }
            self.exit_const(pc.wrapping_add(8).wrapping_add(offset), n + 1);
            return Some(true);
        }
        None
    }

    /// Operand 2 of a data-processing instruction into host `dst` (None: interpret).
    fn operand2(&mut self, insn: u32, pc: u32, dst: u32) -> Option<Carry> {
        if insn & (1 << 25) != 0 {
            let rotation = (insn >> 8) & 0xf;
            let value = (insn & 0xff).rotate_right(2 * rotation);
            self.mov32(dst, value);
            return Some(if rotation == 0 { Carry::Unchanged } else { Carry::Const(value >> 31 != 0) });
        }
        if insn & 0x10 != 0 {
            // register-specified shift (amount = Rs[7:0]); the carry is not computed, so
            // the caller interprets logical operations with S
            let (rm, rs) = (insn & 0xf, (insn >> 8) & 0xf);
            if rm == 15 || rs == 15 {
                return None;
            }
            self.load(W_SRC, rm, pc);
            self.load(15, rs, pc);
            self.p(and_imm(15, 15, 0xff));
            let kind = (insn >> 5) & 3;
            if kind == 3 {
                self.p(rrr(0x1ac0_2c00, dst, W_SRC, 15)); // rorv: amount mod 32
                return Some(Carry::Unchanged);
            }
            // amounts of 32 and more: force bit 5 so the 64-bit shift by (amount mod 64)
            // sees 32-63 (0, or the sign for ASR)
            self.p(addi_w(16, 15, 224, false));
            self.p(lsr_w(16, 16, 8));
            self.p(0x2a00_1400 | 16 << 16 | 15 << 5 | 15); // orr w15, w15, w16, lsl #5
            let op = match kind {
                0 => 0x9ac0_2000, // lslv x
                1 => 0x9ac0_2400, // lsrv x
                _ => {
                    self.p(0x9340_7c00 | W_SRC << 5 | W_SRC); // sxtw x14, w14
                    0x9ac0_2800 // asrv x
                }
            };
            self.p(rrr(op, dst, W_SRC, 15));
            return Some(Carry::Unchanged);
        }
        let rm = insn & 0xf;
        let (kind, amount) = ((insn >> 5) & 3, (insn >> 7) & 0x1f);
        if kind == 0 && amount == 0 {
            self.load(dst, rm, pc);
            return Some(Carry::Unchanged);
        }
        if kind == 3 && amount == 0 {
            return None; // RRX
        }
        self.load(W_SRC, rm, pc);
        Some(match (kind, amount) {
            (0, a) => {
                self.p(lsl_w(dst, W_SRC, a));
                Carry::Bit(W_SRC, 32 - a)
            }
            (1, 0) => {
                self.p(0x5280_0000 | dst); // movz wdst, #0 (LSR #32)
                Carry::Bit(W_SRC, 31)
            }
            (1, a) => {
                self.p(lsr_w(dst, W_SRC, a));
                Carry::Bit(W_SRC, a - 1)
            }
            (2, 0) => {
                self.p(asr_w(dst, W_SRC, 31)); // ASR #32
                Carry::Bit(W_SRC, 31)
            }
            (2, a) => {
                self.p(asr_w(dst, W_SRC, a));
                Carry::Bit(W_SRC, a - 1)
            }
            (_, a) => {
                self.p(ror_w(dst, W_SRC, a));
                Carry::Bit(W_SRC, a - 1)
            }
        })
    }

    /// NZ from host `res`, C from `carry`, V unchanged (A32 logical operations with S).
    fn logic_flags(&mut self, res: u32, carry: Carry) {
        self.p(MRS_NZCV | 15);
        self.p(0x6a00_0000 | res << 16 | res << 5 | ZR); // tst wres, wres
        self.p(MRS_NZCV | 16);
        self.p(bfm_w(16, 15, 0, 29)); // bfxil w16, w15, #0, #30: keep old C, V
        match carry {
            Carry::Unchanged => {}
            Carry::Const(true) => self.p(orr_imm(16, 16, 1 << 29)),
            Carry::Const(false) => self.p(and_imm(16, 16, !(1 << 29))),
            Carry::Bit(src, bit) => {
                self.p(ubfm_w(17, src, bit, bit)); // ubfx w17, wsrc, #bit, #1
                self.p(bfm_w(16, 17, 3, 0)); // bfi w16, w17, #29, #1
            }
        }
        self.p(MSR_NZCV | 16);
    }

    fn data_processing(&mut self, insn: u32, pc: u32, n: u32) -> Option<bool> {
        let (opcode, s, rn, rd) = ((insn >> 21) & 0xf, insn & (1 << 20) != 0, (insn >> 16) & 0xf, (insn >> 12) & 0xf);
        let test = matches!(opcode, 8..=11);
        let logical = matches!(opcode, 0 | 1 | 8 | 9 | 12..=15);
        let reg_shift = insn & (1 << 25) == 0 && insn & 0x10 != 0;
        // SUBS PC etc. return from exceptions; a register shift has no native carry; PC
        // reads with a register shift are unpredictable
        if rd == 15 && !test && s || reg_shift && (s && logical || rn == 15) {
            return None;
        }
        let carry = self.operand2(insn, pc, W_OP)?;
        if !matches!(opcode, 13 | 15) {
            self.load(W_RN, rn, pc);
        }
        let (a, b) = (W_RN, W_OP);
        let w = match opcode {
            0 | 8 => rrr(0x0a00_0000, W_RES, a, b),
            1 | 9 => rrr(0x4a00_0000, W_RES, a, b),
            2 => rrr(if s { 0x6b00_0000 } else { 0x4b00_0000 }, W_RES, a, b),
            3 => rrr(if s { 0x6b00_0000 } else { 0x4b00_0000 }, W_RES, b, a),
            4 => rrr(if s { 0x2b00_0000 } else { 0x0b00_0000 }, W_RES, a, b),
            5 => rrr(if s { 0x3a00_0000 } else { 0x1a00_0000 }, W_RES, a, b),
            6 => rrr(if s { 0x7a00_0000 } else { 0x5a00_0000 }, W_RES, a, b),
            7 => rrr(if s { 0x7a00_0000 } else { 0x5a00_0000 }, W_RES, b, a),
            10 => rrr(0x6b00_0000, ZR, a, b),
            11 => rrr(0x2b00_0000, ZR, a, b),
            12 => rrr(0x2a00_0000, W_RES, a, b),
            13 => mov_w(W_RES, b),
            14 => rrr(0x0a20_0000, W_RES, a, b),
            _ => rrr(0x2a20_0000, W_RES, ZR, b), // MVN = orn wres, wzr, wop
        };
        self.p(w);
        if s && logical {
            self.logic_flags(W_RES, carry);
        }
        if rd == 15 && !test {
            // ARM-state ALUWritePC interworks like BX
            self.exit_bx(W_RES, n + 1);
            return Some(insn >> 28 == 14);
        }
        if !test {
            self.store(W_RES, rd);
        }
        Some(false)
    }

    fn mul(&mut self, insn: u32) -> bool {
        let (rd, ra, rs, rm) = ((insn >> 16) & 0xf, (insn >> 12) & 0xf, (insn >> 8) & 0xf, insn & 0xf);
        if [rd, ra, rs, rm].contains(&15) {
            return false;
        }
        let accumulate = insn & (1 << 21) != 0;
        self.load(W_RN, rm, 0);
        self.load(W_OP, rs, 0);
        let a = if accumulate {
            self.load(W_SRC, ra, 0);
            W_SRC
        } else {
            ZR
        };
        self.p(0x1b00_0000 | W_OP << 16 | a << 10 | W_RN << 5 | W_RES); // madd
        if insn & (1 << 20) != 0 {
            self.logic_flags(W_RES, Carry::Unchanged);
        }
        self.store(W_RES, rd);
        true
    }

    fn mul_long(&mut self, insn: u32) -> bool {
        let (hi, lo, rs, rm) = ((insn >> 16) & 0xf, (insn >> 12) & 0xf, (insn >> 8) & 0xf, insn & 0xf);
        if [hi, lo, rs, rm].contains(&15) || hi == lo || insn & (1 << 20) != 0 {
            return false;
        }
        self.load(W_RN, rm, 0);
        self.load(W_OP, rs, 0);
        let a = if insn & (1 << 21) != 0 {
            self.load(W_RES, lo, 0);
            self.load(W_ADDR, hi, 0);
            self.p(0xaa00_8000 | W_ADDR << 16 | W_RES << 5 | W_RES); // orr x11, x11, x12, lsl #32
            W_RES
        } else {
            ZR
        };
        let base = if insn & (1 << 22) != 0 { 0x9b20_0000 } else { 0x9ba0_0000 }; // smaddl / umaddl
        self.p(base | W_OP << 16 | a << 10 | W_RN << 5 | W_DATA);
        self.store(W_DATA, lo);
        self.p(0xd360_fc00 | W_DATA << 5 | W_DATA); // lsr x13, x13, #32
        self.store(W_DATA, hi);
        true
    }

    /// The miscellaneous space (data-processing opcodes 8-11 without S): Some(block ends).
    fn misc(&mut self, insn: u32, pc: u32, n: u32) -> Option<bool> {
        if insn & 0x0fff_fff0 == 0x012f_ff10 || insn & 0x0fff_fff0 == 0x012f_ff30 {
            let rm = insn & 0xf;
            if rm == 15 {
                return None;
            }
            self.load(W_DATA, rm, pc);
            if insn & 0x20 != 0 {
                self.mov32(15, pc.wrapping_add(4));
                self.p(str_w(15, X_CPU, self.reg_off(14)));
            }
            self.exit_bx(W_DATA, n + 1);
            return Some(true);
        }
        // MOVW / MOVT
        if insn & 0x0fb0_0000 == 0x0300_0000 || insn & 0x0fb0_0000 == 0x0340_0000 {
            let rd = (insn >> 12) & 0xf;
            if rd == 15 {
                return None;
            }
            let imm = (insn >> 4) & 0xf000 | insn & 0xfff;
            if insn & (1 << 22) == 0 {
                self.mov32(W_RES, imm);
            } else {
                self.load(W_RES, rd, pc);
                self.p(0x72a0_0000 | imm << 5 | W_RES); // movk wres, #imm, lsl #16
            }
            self.store(W_RES, rd);
            return Some(false);
        }
        // CLZ
        if insn & 0x0fff_0ff0 == 0x016f_0f10 {
            let (rd, rm) = ((insn >> 12) & 0xf, insn & 0xf);
            if rd == 15 || rm == 15 {
                return None;
            }
            self.load(W_RN, rm, pc);
            self.p(0x5ac0_1000 | W_RN << 5 | W_RES);
            self.store(W_RES, rd);
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
            self.save_flags();
            self.p(ldr_w(W_RES, X_CPU, self.c.cpsr));
            self.mov32(W_OP, !(0x0100_0000 | 0x0600_fc00 | 1 << 5));
            self.p(rrr(0x0a00_0000, W_RES, W_RES, W_OP));
            self.store(W_RES, rd);
            return Some(false);
        }
        None
    }

    /// MRC of the thread ID registers and the (no-op) cache maintenance MCRs.
    fn coprocessor(&mut self, insn: u32) -> bool {
        if insn & 0x0f00_0f10 != 0x0e00_0f10 {
            return false;
        }
        let (read, opc1, crn, rd, crm, opc2) =
            (insn & (1 << 20) != 0, (insn >> 21) & 7, (insn >> 16) & 0xf, (insn >> 12) & 0xf, insn & 0xf, (insn >> 5) & 7);
        if rd == 15 || opc1 != 0 {
            return false;
        }
        if read && crn == 13 && crm == 0 && (2..=4).contains(&opc2) {
            let off = [self.c.tpidrurw, self.c.tpidrro, self.c.tpidrprw][opc2 as usize - 2];
            self.p(ldr_w(W_RES, X_CPU, off));
            self.store(W_RES, rd);
            return true;
        }
        // set/way and by-address cache maintenance, barriers: no cache model
        !read && crn == 7 && matches!(crm, 1 | 5 | 6 | 10 | 11 | 14)
    }

    fn media(&mut self, insn: u32) -> bool {
        let (rd, rm) = ((insn >> 12) & 0xf, insn & 0xf);
        // REV, REV16
        if matches!(insn & 0x0fff_0ff0, 0x06bf_0f30 | 0x06bf_0fb0) {
            if rd == 15 || rm == 15 {
                return false;
            }
            self.load(W_RN, rm, 0);
            self.p((if insn & 0x80 == 0 { 0x5ac0_0800 } else { 0x5ac0_0400 }) | W_RN << 5 | W_RES);
            self.store(W_RES, rd);
            return true;
        }
        // UXTB, UXTH, SXTB, SXTH (+ accumulate)
        let extend = match insn & 0x0ff0_03f0 {
            0x06e0_0070 => Some(ubfm_w(W_OP, W_OP, 0, 7)),
            0x06f0_0070 => Some(ubfm_w(W_OP, W_OP, 0, 15)),
            0x06a0_0070 => Some(sbfm_w(W_OP, W_OP, 0, 7)),
            0x06b0_0070 => Some(sbfm_w(W_OP, W_OP, 0, 15)),
            _ => None,
        };
        if let Some(op) = extend {
            let rn = (insn >> 16) & 0xf;
            if rd == 15 || rm == 15 {
                return false;
            }
            self.load(W_OP, rm, 0);
            let rotation = ((insn >> 10) & 3) * 8;
            if rotation != 0 {
                self.p(ror_w(W_OP, W_OP, rotation));
            }
            self.p(op);
            if rn == 15 {
                self.store(W_OP, rd);
            } else {
                self.load(W_RN, rn, 0);
                self.p(rrr(0x0b00_0000, W_RES, W_RN, W_OP));
                self.store(W_RES, rd);
            }
            return true;
        }
        // BFI, BFC
        if insn & 0x0fe0_0070 == 0x07c0_0010 {
            let (msb, lsb) = ((insn >> 16) & 0x1f, (insn >> 7) & 0x1f);
            if rd == 15 || msb < lsb {
                return false;
            }
            self.load(W_RES, rd, 0);
            let src = if rm == 15 {
                ZR
            } else {
                self.load(W_RN, rm, 0);
                W_RN
            };
            self.p(bfm_w(W_RES, src, (32 - lsb) & 31, msb - lsb));
            self.store(W_RES, rd);
            return true;
        }
        // UBFX, SBFX
        if matches!(insn & 0x0fe0_0070, 0x07e0_0050 | 0x07a0_0050) {
            let (width, lsb) = (((insn >> 16) & 0x1f) + 1, (insn >> 7) & 0x1f);
            if rd == 15 || rm == 15 || lsb + width > 32 {
                return false;
            }
            self.load(W_RN, rm, 0);
            let signed = insn & 0x0fe0_0070 == 0x07a0_0050;
            self.p(if signed { sbfm_w(W_RES, W_RN, lsb, lsb + width - 1) } else { ubfm_w(W_RES, W_RN, lsb, lsb + width - 1) });
            self.store(W_RES, rd);
            return true;
        }
        // SDIV, UDIV (division by zero gives zero on both)
        if matches!(insn & 0x0ff0_f0f0, 0x0710_f010 | 0x0730_f010) {
            let (rd, divisor, dividend) = ((insn >> 16) & 0xf, (insn >> 8) & 0xf, insn & 0xf);
            if [rd, divisor, dividend].contains(&15) {
                return false;
            }
            self.load(W_RN, dividend, 0);
            self.load(W_OP, divisor, 0);
            let base = if insn & (1 << 21) == 0 { 0x1ac0_0c00 } else { 0x1ac0_0800 };
            self.p(rrr(base, W_RES, W_RN, W_OP));
            self.store(W_RES, rd);
            return true;
        }
        false
    }

    /// Base (into w9) and the offset-applied address (into `W_ADDR`) of a load/store; the
    /// accessed address is in the returned register.
    fn address(&mut self, rn: u32, pc: u32, pre: bool, up: bool, offset: Option<u32>) -> u32 {
        if self.thumb && rn == 15 {
            self.mov32(W_RN, pc.wrapping_add(4) & !3); // Align(PC, 4)
        } else {
            self.load(W_RN, rn, pc);
        }
        match offset {
            Some(0) => self.p(mov_w(W_ADDR, W_RN)),
            Some(imm) if imm < 4096 => self.p(addi_w(W_ADDR, W_RN, imm, !up)),
            Some(imm) => {
                self.mov32(W_OP, imm);
                self.p(rrr(if up { 0x0b00_0000 } else { 0x4b00_0000 }, W_ADDR, W_RN, W_OP));
            }
            None => self.p(rrr(if up { 0x0b00_0000 } else { 0x4b00_0000 }, W_ADDR, W_RN, W_OP)),
        }
        if pre {
            W_ADDR
        } else {
            W_RN
        }
    }

    /// LDR, STR, LDRB, STRB.
    fn ldst(&mut self, insn: u32, pc: u32, n: u32) -> Option<bool> {
        let (pre, up, byte, wb, load) = (insn & (1 << 24) != 0, insn & (1 << 23) != 0, insn & (1 << 22) != 0, insn & (1 << 21) != 0, insn & (1 << 20) != 0);
        let (rn, rd) = ((insn >> 16) & 0xf, (insn >> 12) & 0xf);
        let writeback = !pre || wb;
        if !pre && wb || load && rd == 15 && byte || writeback && (rn == 15 || load && rn == rd) {
            return None; // LDRT/STRT, LDRB PC, unpredictable writebacks
        }
        let offset = if insn & (1 << 25) == 0 {
            Some(insn & 0xfff)
        } else {
            let (rm, kind, amount) = (insn & 0xf, (insn >> 5) & 3, (insn >> 7) & 0x1f);
            if rm == 15 || kind == 3 && amount == 0 {
                return None;
            }
            self.load(W_OP, rm, pc);
            match (kind, amount) {
                (0, 0) => {}
                (0, a) => self.p(lsl_w(W_OP, W_OP, a)),
                (1, 0) => self.p(0x5280_0000 | W_OP),
                (1, a) => self.p(lsr_w(W_OP, W_OP, a)),
                (2, 0) => self.p(asr_w(W_OP, W_OP, 31)),
                (2, a) => self.p(asr_w(W_OP, W_OP, a)),
                (_, a) => self.p(ror_w(W_OP, W_OP, a)),
            }
            None
        };
        let a = self.address(rn, pc, pre, up, offset);
        let size = if byte { 1 } else { 4 };
        let (slow, resume) = self.slow_path(pc, n);
        self.probe(a, size, !load, slow);
        if load {
            self.p((if byte { 0x3860_4800 } else { 0xb860_4800 }) | a << 16 | 16 << 5 | W_DATA);
            if writeback {
                self.store(W_ADDR, rn);
            }
            if rd == 15 {
                self.exit_bx(W_DATA, n + 1);
                return Some(self.after_pc_load(resume, insn, pc, n));
            }
            self.store(W_DATA, rd);
        } else {
            self.load(W_DATA, rd, pc);
            self.p((if byte { 0x3820_4800 } else { 0xb820_4800 }) | a << 16 | 16 << 5 | W_DATA);
            if writeback {
                self.store(W_ADDR, rn);
            }
        }
        self.bind(resume);
        Some(false)
    }

    /// After a native PC load (which has left the block): the interpreted slow path resumes
    /// here only if it loaded the next instruction's address. Returns whether the block ends.
    fn after_pc_load(&mut self, resume: usize, insn: u32, pc: u32, n: u32) -> bool {
        self.bind(resume);
        if insn >> 28 == 14 {
            self.exit_const(pc.wrapping_add(self.size), n + 1);
            return true;
        }
        false
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
            self.load(W_OP, rm, pc);
            None
        };
        let a = self.address(rn, pc, pre, up, offset);
        let size = if half { 2 } else { 1 };
        let (slow, resume) = self.slow_path(pc, n);
        if pair {
            self.probe_pair(a, half, slow);
        } else {
            self.probe(a, size, !load, slow);
        }
        if pair {
            self.p(0x8b20_4000 | a << 16 | 16 << 5 | 16); // add x16, x16, wa, uxtw
            if half {
                self.load(W_DATA, rd, pc);
                self.load(W_SRC, rd + 1, pc);
                self.p(0x2900_0000 | W_SRC << 10 | 16 << 5 | W_DATA); // stp w13, w14, [x16]
                if writeback {
                    self.store(W_ADDR, rn);
                }
            } else {
                self.p(0x2940_0000 | W_SRC << 10 | 16 << 5 | W_DATA); // ldp w13, w14, [x16]
                self.store(W_DATA, rd);
                self.store(W_SRC, rd + 1);
                if writeback {
                    self.store(W_ADDR, rn);
                }
            }
        } else if load {
            let op = match (signed, half) {
                (false, true) => 0x7860_4800, // ldrh
                (true, false) => 0x38e0_4800, // ldrsb
                _ => 0x78e0_4800,             // ldrsh
            };
            self.p(op | a << 16 | 16 << 5 | W_DATA);
            self.store(W_DATA, rd);
            if writeback {
                self.store(W_ADDR, rn);
            }
        } else {
            self.load(W_DATA, rd, pc);
            self.p(0x7820_4800 | a << 16 | 16 << 5 | W_DATA); // strh
            if writeback {
                self.store(W_ADDR, rn);
            }
        }
        self.bind(resume);
        true
    }

    /// LDM/STM without the S bit: Some(block ends) or None.
    fn ldm_stm(&mut self, insn: u32, pc: u32, n: u32) -> Option<bool> {
        let (pre, up, s, wb, load) = (insn & (1 << 24) != 0, insn & (1 << 23) != 0, insn & (1 << 22) != 0, insn & (1 << 21) != 0, insn & (1 << 20) != 0);
        let rn = (insn >> 16) & 0xf;
        let list = insn & 0xffff;
        if s || rn == 15 || list == 0 || !load && list & 0x8000 != 0 || load && wb && list & (1 << rn) != 0 {
            return None;
        }
        let count = list.count_ones();
        let bytes = count * 4;
        self.load(W_RN, rn, pc);
        // start address into w12
        match (pre, up) {
            (false, true) => self.p(mov_w(W_ADDR, W_RN)),
            (true, true) => self.p(addi_w(W_ADDR, W_RN, 4, false)),
            (false, false) => self.p(addi_w(W_ADDR, W_RN, bytes - 4, true)),
            (true, false) => self.p(addi_w(W_ADDR, W_RN, bytes, true)),
        }
        let (slow, resume) = self.slow_path(pc, n);
        // the whole transfer in one page
        self.p(addi_w(W_OP, W_ADDR, bytes - 4, false));
        self.p(rrr(0x4a00_0000, 15, W_ADDR, W_OP)); // eor w15, w12, w10
        self.p(lsr_w(15, 15, 12));
        self.br(0x3500_000f, slow); // cbnz w15, slow
        self.probe(W_ADDR, 4, !load, slow);
        self.p(0x8b20_4000 | W_ADDR << 16 | 16 << 5 | 16); // add x16, x16, w12, uxtw
        let mut k = 0;
        for r in 0..16 {
            if list & (1 << r) == 0 {
                continue;
            }
            if load {
                let dst = if r == 15 { W_SRC } else { W_DATA };
                self.p(0xb940_0000 | k << 10 | 16 << 5 | dst); // ldr wdst, [x16, #4k]
                if r != 15 {
                    self.store(W_DATA, r);
                }
            } else {
                self.load(W_DATA, r, pc);
                self.p(0xb900_0000 | k << 10 | 16 << 5 | W_DATA); // str w13, [x16, #4k]
            }
            k += 1;
        }
        if wb {
            self.p(addi_w(W_RN, W_RN, bytes, !up));
            self.store(W_RN, rn);
        }
        if load && list & 0x8000 != 0 {
            self.exit_bx(W_SRC, n + 1);
            return Some(self.after_pc_load(resume, insn, pc, n));
        }
        self.bind(resume);
        Some(false)
    }
}

/// The chaining dispatcher, entered by a block's native exit with the guest flags saved,
/// its instructions counted and `r[15]` set (x19 = Cpu, x20 = micro-TLB base, x21/x22 =
/// this privilege's tables: native exits never change the mode). It jumps past the
/// prologue of the next compiled block while `instret < chain_until`, the core stays in
/// ARM state and the fetch micro-TLB and front cache know the target; otherwise it returns
/// to the machine.
fn dispatcher(c: &Ctx) -> Vec<u32> {
    let mut e = Emit::new(c);
    let ret = e.label();
    e.p(ldr_x(9, X_CPU, c.instret));
    e.p(ldr_x(10, X_CPU, c.chain_until));
    e.p(0xeb0a_013f); // cmp x9, x10
    e.br(0x5400_0002, ret); // b.hs ret
    e.p(ldr_w(9, X_CPU, c.cpsr));
    e.mov32(12, 0x0600_fc00);
    e.p(0x6a0c_013f); // tst w9, w12 (ITSTATE)
    e.br(0x5400_0001, ret); // b.ne ret: the interpreter runs IT blocks
    e.p(and_imm(10, 9, 0x1f));
    e.p(0x7100_415f); // cmp w10, #0x10
    e.p(0x1a9f_07ea); // cset w10, ne (privileged)
    e.p(ldr_w(11, X_CPU, c.r + 60));
    // fetch micro-TLB: table 2 + 3 * privileged
    e.p(0x0b0a_054d); // add w13, w10, w10, lsl #1
    e.p(addi_w(13, 13, U_EXEC as u32, false));
    e.p(ubfm_w(14, 11, 12, 19)); // ubfx w14, w11, #12, #8
    e.p(0x8b0d_21ce); // add x14, x14, x13, lsl #8
    e.p(0x8b0e_128e); // add x14, x20, x14, lsl #4
    e.p(0xa940_41cf); // ldp x15, x16, [x14]
    e.p(and_imm(12, 11, 0xffff_f000));
    e.p(0xeb0c_01ff); // cmp x15, x12
    e.br(0x5400_0001, ret); // b.ne ret
    e.p(0x8b2b_4210); // add x16, x16, w11, uxtw
    e.p(ldr_x(17, X_CPU, c.ram));
    e.p(0xcb11_0210); // sub x16, x16, x17
    e.p(ldr_w(17, X_CPU, c.ram_base));
    e.p(0x0b11_0210); // add w16, w16, w17 (physical pc, upper half zero)
    e.p(0xaa0a_8210); // orr x16, x16, x10, lsl #32 (key)
    e.p(ubfm_w(12, 9, 5, 5)); // ubfx w12, w9, #5, #1 (Thumb)
    e.p(0xaa0c_8610); // orr x16, x16, x12, lsl #33
    // front cache slot ((pc ^ pc >> 14) >> 2) & (FRONT - 1), 24-byte entries
    e.p(0x4a4b_396c); // eor w12, w11, w11, lsr #14
    e.p(ubfm_w(12, 12, 2, 15)); // ubfx w12, w12, #2, #14
    e.p(ldr_x(13, X_CPU, c.front));
    e.p(0x8b0c_058c); // add x12, x12, x12, lsl #1
    e.p(0x8b0c_0dad); // add x13, x13, x12, lsl #3
    e.p(0xb940_01ae); // ldr w14, [x13]
    e.p(0x6b0b_01df); // cmp w14, w11
    e.br(0x5400_0001, ret); // b.ne ret
    e.p(0xf940_05ae); // ldr x14, [x13, #8]
    e.p(0xeb10_01df); // cmp x14, x16
    e.br(0x5400_0001, ret); // b.ne ret
    e.p(0xf940_09b1); // ldr x17, [x13, #16]
    e.p(0x9100_0231 | (PROLOGUE_WORDS as u32 * 4) << 10); // add x17, x17, #prologue
    e.p(0xd61f_0220); // br x17
    e.bind(ret);
    e.epilogue(0);
    // finish() appends the (unused) h_step exit and patches the branches
    e.finish()
}

#[path = "jit_t32.rs"]
mod t32;

/// Does this (conditional-space, interpreted) instruction always write the PC?
fn writes_pc(insn: u32) -> bool {
    let rd = (insn >> 12) & 0xf;
    // data processing with Rd = PC (not TST/TEQ/CMP/CMN)
    if insn & 0x0c00_0000 == 0 && rd == 15 && !(insn & 0x0190_0000 == 0x0100_0000) && insn & 0x0180_0000 != 0x0100_0000 {
        return true;
    }
    // LDR PC
    if insn & 0x0c10_0000 == 0x0410_0000 && rd == 15 {
        return true;
    }
    // LDM with PC, SVC
    insn & 0x0e10_8000 == 0x0810_8000 || insn & 0x0f00_0000 == 0x0f00_0000
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn logical_immediates() {
        // known encodings (N:immr:imms << 10) from an assembler
        assert_eq!(logic_imm32(0xf000_0000), Some(4 << 16 | 3 << 10)); // and w0, w0, #0xf0000000
        assert_eq!(logic_imm32(0xffff_fffc), Some(30 << 16 | 29 << 10));
        assert_eq!(logic_imm32(0xffff_f000), Some(20 << 16 | 19 << 10));
        assert_eq!(logic_imm32(0x0000_00ff), Some(7 << 10));
        assert_eq!(logic_imm32(0x5555_5555), Some(0x3c << 10));
        assert!(logic_imm32(0xffff_f003).is_some());
        assert!(logic_imm32(0xffff_f007).is_some());
        assert_eq!(logic_imm32(0x1234_5678), None);
        // every encodable value decodes back
        for v in [0x2000_0000u32, 0xdfff_ffff, 0x20, 0xffff_fffe, 0xffff_f001, 0x0f0f_0f0f] {
            let e = logic_imm32(v).unwrap();
            let (immr, imms) = ((e >> 16) & 0x3f, (e >> 10) & 0x3f);
            let size = match imms {
                0..=0x1f => 32,
                0x20..=0x2f => 16,
                0x30..=0x37 => 8,
                0x38..=0x3b => 4,
                _ => 2,
            };
            let ones = (imms & (size - 1)) + 1;
            let mask = if size == 32 { u32::MAX } else { (1 << size) - 1 };
            let run = if ones == 32 { u32::MAX } else { (1u32 << ones) - 1 };
            let elem = if immr == 0 { run } else { ((run >> immr) | (run << (size - immr))) & mask };
            let mut back = 0;
            for i in 0..32 / size {
                back |= elem << (i * size);
            }
            assert_eq!(back, v, "{v:#x}");
        }
    }
}
