//! The block JIT's driver: which code is compiled, when compiled code is still valid, and the
//! loop that runs a hart through compiled blocks and the interpreter.
//!
//! Two code generators share this driver: `jit_a64.rs` (RISC-V to AArch64, on AArch64 macOS
//! and Linux hosts) and `jit_wasm.rs` (RISC-V to WebAssembly, browser builds). Elsewhere
//! [`AVAILABLE`] is false and [`run`] interprets everything.
//!
//! - **Keyed by physical address.** A compiled region lies in one 4 KiB page and is found by
//!   the physical address of its entry, so the four harts share it and a page mapped at
//!   several virtual addresses (a shared library in two processes) is compiled once. The
//!   code computes guest PCs from the page's virtual address at entry, never from constants.
//! - **Valid until `fence.i`.** RISC-V needs a `fence.i` before modified code is guaranteed
//!   to run (Linux, OpenSBI and U-Boot issue one after loading or patching code; on the U74
//!   the instruction cache is not coherent either). Each `fence.i` starts a new epoch; a
//!   region entered for the first time in a new epoch compares the bytes it was compiled
//!   from with memory, and is dropped if they changed. This also catches code written by
//!   DMA, which no store tracking would see.
//! - **Exits.** A block returns when it leaves its region, when an instruction it hands to
//!   the interpreter (`h_step`) needs the machine to look (a device access, a CSR write, a
//!   trap), or when the cycle budget is used. Interrupts are taken between blocks. Native
//!   code goes on to the next region by itself where the hart's jump cache knows it (see
//!   `jit_a64.rs`), as long as it only leaves through control flow: nothing that changes
//!   interrupts, the mode or the address space happens without a return here.
use crate::mmu::{JC, JC_TABLES, J_EMPTY, UT, UT_NONE, UT_TABLES, U_EMPTY};
use crate::{rvc, Access, Bus, Hart};
use std::collections::HashMap;
use std::hash::{BuildHasherDefault, Hasher};

/// A hasher for physical addresses (the default SipHash dominated the dispatch).
#[derive(Default)]
struct PaHasher(u64);

impl Hasher for PaHasher {
    fn write(&mut self, bytes: &[u8]) {
        for &b in bytes {
            self.write_u64(self.0 ^ b as u64);
        }
    }
    fn write_u64(&mut self, v: u64) {
        self.0 = (v ^ self.0).wrapping_mul(0x9e37_79b9_7f4a_7c15);
    }
    fn finish(&self) -> u64 {
        self.0 ^ self.0 >> 29
    }
}

type Map<V> = HashMap<u64, V, BuildHasherDefault<PaHasher>>;

/// Entries of the direct-mapped front cache of compiled regions.
const FRONT: usize = 4096;

/// A front-cache entry: a region checked in `epoch`, for the entry at physical `pa`.
#[derive(Clone, Copy)]
struct Front {
    pa: u64,
    f: usize,
    epoch: u64,
}

const FRONT_EMPTY: Front = Front { pa: u64::MAX, f: 0, epoch: u64::MAX };

fn front_slot(pa: u64) -> usize {
    (pa >> 1) as usize & (FRONT - 1)
}

/// Whether this build can compile (AArch64 macOS and Linux hosts, browser builds).
pub const AVAILABLE: bool = cfg!(any(target_arch = "wasm32", all(target_arch = "aarch64", any(target_os = "macos", target_os = "linux"))));

#[cfg(all(target_arch = "aarch64", any(target_os = "macos", target_os = "linux")))]
use crate::jit_a64 as native;

/// Executions of a block's entry before it is compiled.
pub const DEFAULT_HOT: u32 = 64;

/// Compiled functions alive (and dropped but not yet freed) before everything is flushed:
/// the browser's function table grows with them.
const SLOTS: usize = 16384;

/// A compiled region: its function and the bytes of the page it was compiled from.
struct Region {
    /// the function (a table index in browser builds)
    f: usize,
    /// the epoch in which the bytes were last checked
    epoch: u64,
    /// offset in the page of the first byte compiled
    lo: u16,
    bytes: Box<[u8]>,
}

enum Entry {
    Code(Region),
    /// the entry cannot be compiled (its first instruction straddles the page)
    Never,
}

/// The compiled code of a machine's harts.
pub struct Jit {
    blocks: Map<Entry>,
    heat: Map<u32>,
    /// regions checked in the current epoch, by entry
    front: Vec<Front>,
    pub hot: u32,
    /// regions compiled since the start
    pub compiled: u64,
    /// regions dropped because their code changed
    pub invalidated: u64,
    /// full flushes (the function table was full)
    pub flushes: u64,
    epoch: u64,
    /// bumped whenever a jump cache entry could point at code that is no longer valid (a new
    /// epoch, a region dropped, a flush): a hart empties its jump cache when it sees a change
    generation: u64,
    /// functions installed and not yet freed by a flush
    slots: usize,
    /// the native code buffer (allocated at the first compile)
    #[cfg(all(target_arch = "aarch64", any(target_os = "macos", target_os = "linux")))]
    code: Option<native::CodeCache>,
}

impl Default for Jit {
    fn default() -> Self {
        Jit {
            blocks: Map::default(),
            heat: Map::default(),
            front: vec![FRONT_EMPTY; FRONT],
            hot: DEFAULT_HOT,
            compiled: 0,
            invalidated: 0,
            flushes: 0,
            epoch: 0,
            generation: 0,
            slots: 0,
            #[cfg(all(target_arch = "aarch64", any(target_os = "macos", target_os = "linux")))]
            code: None,
        }
    }
}

impl Jit {
    pub fn new() -> Self {
        Self::default()
    }

    /// Drop all compiled code.
    pub fn flush(&mut self) {
        self.blocks.clear();
        self.heat.clear();
        self.front.fill(FRONT_EMPTY);
        self.generation += 1;
        if self.slots > 0 {
            #[cfg(target_arch = "wasm32")]
            crate::jit_wasm::reset();
            #[cfg(all(target_arch = "aarch64", any(target_os = "macos", target_os = "linux")))]
            if let Some(c) = self.code.as_mut() {
                c.reset();
            }
            self.flushes += 1;
        }
        self.slots = 0;
    }
}

impl Drop for Jit {
    fn drop(&mut self) {
        // free the browser's function table slots
        self.flush();
    }
}

/// The compiled function for a region entry: `(hart) -> status`.
type BlockFn = extern "C" fn(*mut Hart) -> i32;

/// Take a pending interrupt, as the interpreter does before each instruction. Returns false
/// when the hart waits in WFI.
#[inline(always)]
fn interrupts<B: Bus>(h: &mut Hart, bus: &mut B) -> bool {
    if let Some(lines) = bus.irq_update() {
        h.irq_lines = lines;
    }
    if h.mip() & h.mie != 0 {
        h.wfi = false;
        h.take_interrupt();
    }
    !h.wfi
}

/// Run `h` for up to `budget` cycles (instructions and traps), through compiled regions where
/// there are any. Returns the cycles used; stops early when the hart waits in WFI.
pub fn run<B: Bus>(h: &mut Hart, bus: &mut B, jit: &mut Jit, budget: u64) -> u64 {
    let c0 = h.cycle;
    let end = c0.wrapping_add(budget);
    if h.tlb.utlb.is_empty() {
        h.tlb.utlb = vec![U_EMPTY; UT_TABLES * 2 * UT];
    }
    if LINKS && h.tlb.jc.is_empty() {
        h.tlb.jc = vec![J_EMPTY; JC_TABLES * JC];
    }
    h.jit_bus = bus as *mut B as usize;
    h.jit_step = h_step::<B> as *const () as usize;
    h.chain_until = end;
    while h.cycle.wrapping_sub(c0) < budget {
        if !interrupts(h, bus) {
            break;
        }
        if h.fence_i != h.jit_fence_seen {
            h.jit_fence_seen = h.fence_i;
            jit.epoch += 1;
            jit.generation += 1;
        }
        if h.jit_gen != jit.generation {
            h.jit_gen = jit.generation;
            h.tlb.jflush();
        }
        let pc = h.pc;
        let pa = match h.translate(bus, pc, Access::Fetch) {
            Ok(pa) => pa,
            Err(_) => {
                h.step(bus); // takes the fault
                continue;
            }
        };
        let base = bus.ram_base();
        let ram = bus.ram();
        let off = pa.wrapping_sub(base);
        if off >= ram.len() as u64 {
            interpret_bb(h, bus, end); // the boot ROM's SRAM, devices
            continue;
        }
        let page = (off & !0xfff) as usize;
        let slot = front_slot(pa);
        let fr = jit.front[slot];
        if fr.pa == pa && fr.epoch == jit.epoch {
            call(h, fr.f);
            continue;
        }
        match jit.blocks.get_mut(&pa) {
            Some(Entry::Code(r)) => {
                if r.epoch != jit.epoch {
                    let lo = page + r.lo as usize;
                    if ram[lo..lo + r.bytes.len()] != *r.bytes {
                        jit.blocks.remove(&pa);
                        jit.invalidated += 1;
                        jit.generation += 1;
                        // compile the new code at once: it ran before
                        jit.heat.insert(pa, jit.hot);
                        continue;
                    }
                    r.epoch = jit.epoch;
                }
                jit.front[slot] = Front { pa, f: r.f, epoch: jit.epoch };
                call(h, r.f);
            }
            Some(Entry::Never) => interpret_bb(h, bus, end),
            None => {
                let heat = jit.heat.entry(pa).or_insert(0);
                *heat += 1;
                if *heat < jit.hot {
                    interpret_bb(h, bus, end);
                    continue;
                }
                jit.heat.remove(&pa);
                if cfg!(target_arch = "wasm32") && jit.slots >= SLOTS {
                    jit.flush();
                }
                let entry = compile(jit, h, &ram[page..page + 0x1000], pc);
                if let Entry::Code(_) = entry {
                    jit.compiled += 1;
                    jit.slots += 1;
                }
                jit.blocks.insert(pa, entry);
            }
        }
    }
    h.cycle.wrapping_sub(c0)
}

/// Whether compiled code links regions through the jump cache (native code).
const LINKS: bool = cfg!(all(target_arch = "aarch64", any(target_os = "macos", target_os = "linux")));

/// Run the compiled region `f` (the hart's pc is its entry).
#[inline(always)]
fn call(h: &mut Hart, f: usize) {
    // while an LR reservation is held, every access takes the interpreter's path (a store to
    // the reserved address clears it); LR and SC leave compiled code, so this is re-decided
    let table = if h.reservation != u64::MAX { UT_NONE } else { h.utlb_table() };
    h.jit_ut = h.tlb.utlb.as_ptr() as usize + table * 2 * UT * std::mem::size_of::<crate::mmu::UEntry>();
    #[cfg(all(target_arch = "aarch64", any(target_os = "macos", target_os = "linux")))]
    {
        // fetches are made in the current mode: the region's code for this pc in this mode
        let table = h.prv as usize;
        h.tlb.jfill(table, h.pc, (f + native::LINKED) as u64);
        h.jit_jc = h.tlb.jc.as_ptr() as usize + crate::mmu::Tlb::jslot(table, 0) * std::mem::size_of::<crate::mmu::JEntry>();
    }
    // SAFETY: `f` is a compiled region (natively its address; in browser builds a function
    // table index of type (i32) -> i32, which is this ABI); it reads and writes the hart through
    // the pointer and calls `h_step` with the bus in `jit_bus`, which outlive the call
    let f: BlockFn = unsafe { std::mem::transmute::<usize, BlockFn>(f) };
    f(h as *mut Hart);
}

/// Compile the region entered at `pc` (whose page holds `code`).
#[allow(unused_variables)]
fn compile(jit: &mut Jit, h: &Hart, code: &[u8], pc: u64) -> Entry {
    let region = |(f, lo, hi): (usize, u16, u16)| Entry::Code(Region { f, epoch: u64::MAX, lo, bytes: code[lo as usize..hi as usize].into() });
    #[cfg(target_arch = "wasm32")]
    if let Some(r) = crate::jit_wasm::compile(code, pc, h.jit_step as u32) {
        return region(r);
    }
    #[cfg(all(target_arch = "aarch64", any(target_os = "macos", target_os = "linux")))]
    {
        if jit.code.is_none() {
            jit.code = native::CodeCache::new(native::CODE_BYTES);
        }
        for _ in 0..2 {
            let Some(cache) = jit.code.as_mut() else { break };
            match native::compile(cache, code, pc, h.jit_step) {
                Ok(r) => return region(r),
                Err(native::Fail::Never) => break,
                // the buffer is full: start again with an empty one
                Err(native::Fail::Full) => jit.flush(),
            }
        }
    }
    Entry::Never
}

/// Interpret one basic block: up to the first instruction that does not fall through to the
/// next (a taken branch, a jump, a trap), at most 64 instructions.
fn interpret_bb<B: Bus>(h: &mut Hart, bus: &mut B, end: u64) {
    for _ in 0..64 {
        let pc = h.pc;
        h.step(bus);
        let d = h.pc.wrapping_sub(pc);
        if d != 2 && d != 4 || h.cycle.wrapping_sub(end) as i64 >= 0 || !interrupts(h, bus) {
            break;
        }
    }
}

/// The memory access an instruction makes: (base register, offset, size, store).
pub(crate) fn mem_access(i: u32) -> Option<(usize, u64, u8, bool)> {
    let rs1 = ((i >> 15) & 31) as usize;
    let f3 = (i >> 12) & 7;
    let imm_i = ((i as i32) >> 20) as i64 as u64;
    let imm_s = ((((i as i32) >> 25) << 5) | ((i >> 7) & 31) as i32) as i64 as u64;
    match i & 0x7f {
        0x03 if f3 != 7 => Some((rs1, imm_i, 1 << (f3 & 3), false)),
        0x07 if f3 == 2 || f3 == 3 => Some((rs1, imm_i, 1 << f3, false)),
        0x23 if f3 < 4 => Some((rs1, imm_s, 1 << f3, true)),
        0x27 if f3 == 2 || f3 == 3 => Some((rs1, imm_s, 1 << f3, true)),
        // AMOs: LR reads, the others write
        0x2f if f3 == 2 || f3 == 3 => Some((rs1, 0, 1 << f3, i >> 27 != 0x02)),
        _ => None,
    }
}

/// Run the instruction at `h.pc` in the interpreter for compiled code. `info`: the number of
/// instructions of the basic block before it that the block has not counted yet (bits 0..15)
/// and whether the block ends after it (bit 16). Returns 0 to continue in the block (the
/// counts are put back as the block expects), 1 after a trap, 2 to leave the block.
extern "C" fn h_step<B: Bus>(hart: *mut Hart, info: i32) -> i32 {
    // SAFETY: compiled code passes the hart it was called with; `jit_bus` was set by `run`
    // to the bus it runs on, which outlives the call
    let (h, bus) = unsafe { (&mut *hart, &mut *(((*hart).jit_bus) as *mut B)) };
    let before = (info & 0xffff) as u64;
    let last = info & 0x1_0000 != 0;
    h.instret = h.instret.wrapping_add(before);
    h.cycle = h.cycle.wrapping_add(before);
    let traps = h.traps;
    h.dev_access = false;
    let pc = h.pc;
    let raw = match h.fetch(bus, pc) {
        Ok(i) => i,
        Err(_) => {
            h.step(bus);
            return 1;
        }
    };
    let i = if raw & 3 == 3 { raw } else { rvc::expand(raw as u16) };
    let access = mem_access(i).map(|(rs1, imm, size, store)| (h.x[rs1].wrapping_add(imm), size, store));
    let amo = (i & 0x7f == 0x2f).then(|| h.x[((i >> 15) & 31) as usize]);
    h.step(bus);
    if h.traps != traps {
        return 1;
    }
    if h.dev_access {
        return 2;
    }
    let mut st = match i & 0x7f {
        // CSR reads, and writes of CSRs compiled code does not depend on, go on in the block;
        // other CSR writes, privileged instructions and ecall leave
        0x73 => {
            let f = (i >> 12) & 7;
            let writes = match f {
                1 | 5 => true,
                2 | 3 | 6 | 7 => (i >> 15) & 31 != 0,
                _ => true,
            };
            let harmless = matches!(i >> 20, 0x001..=0x003 | 0x140 | 0x340); // fflags, frm, fcsr, s/mscratch
            if f != 0 && (!writes || harmless) {
                0
            } else {
                2
            }
        }
        0x0f if (i >> 12) & 7 != 0 => 2, // fence.i: the driver starts a new epoch
        // LR and SC: compiled code picks its fast path by whether a reservation is held
        0x2f if matches!(i >> 27, 0x02 | 0x03) => 2,
        0x2f => {
            // an AMO on a device (no AMO in RAM changes what the machine must look at)
            let va = amo.unwrap_or(0);
            match h.translate(bus, va, Access::Load) {
                Ok(pa) if pa.wrapping_sub(bus.ram_base()) < bus.ram().len() as u64 => 0,
                _ => 2,
            }
        }
        _ => 0,
    };
    if let Some((va, size, store)) = access {
        // a load or store in RAM: map its page for the fast path
        let kind = if store { Access::Store } else { Access::Load };
        if (va & 0xfff) + size as u64 <= 0x1000 {
            if let Ok(pa) = h.translate(bus, va, kind) {
                let base = bus.ram_base();
                let ram = bus.ram();
                let o = pa.wrapping_sub(base);
                if o < ram.len() as u64 {
                    let host = ram.as_ptr() as usize + (o as usize & !0xfff);
                    let table = h.utlb_table();
                    h.tlb.ufill(table, store, va, host as u64);
                }
            }
        }
    }
    if last && st == 0 {
        st = 2;
    }
    if st == 0 {
        // the block counts this instruction and the ones before it itself
        h.instret = h.instret.wrapping_sub(before + 1);
        h.cycle = h.cycle.wrapping_sub(before + 1);
    }
    st
}
