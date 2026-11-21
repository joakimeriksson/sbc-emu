//! The block JIT's driver: which code is compiled, when compiled code is still valid, and the
//! loop that runs a hart through compiled blocks and the interpreter.
//!
//! Only browser builds have a code generator so far (`jit_wasm.rs`: RISC-V to WebAssembly);
//! elsewhere [`AVAILABLE`] is false and [`run`] is never used.
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
//!   the interpreter (`h_step`) needs the machine to look (a device access, a CSR, a trap),
//!   or when the cycle budget is used. Interrupts are taken between blocks.
use crate::mmu::{UT, UT_TABLES, U_EMPTY};
use crate::{rvc, Access, Bus, Hart};
use std::collections::HashMap;

/// Whether this build can compile (browser builds).
pub const AVAILABLE: bool = cfg!(target_arch = "wasm32");

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
    #[cfg_attr(not(target_arch = "wasm32"), allow(dead_code))]
    Code(Region),
    /// the entry cannot be compiled (its first instruction straddles the page)
    Never,
}

/// The compiled code of a machine's harts.
pub struct Jit {
    blocks: HashMap<u64, Entry>,
    heat: HashMap<u64, u32>,
    pub hot: u32,
    /// regions compiled since the start
    pub compiled: u64,
    /// regions dropped because their code changed
    pub invalidated: u64,
    /// full flushes (the function table was full)
    pub flushes: u64,
    epoch: u64,
    /// functions installed and not yet freed by a flush
    slots: usize,
}

impl Default for Jit {
    fn default() -> Self {
        Jit {
            blocks: HashMap::new(),
            heat: HashMap::new(),
            hot: DEFAULT_HOT,
            compiled: 0,
            invalidated: 0,
            flushes: 0,
            epoch: 0,
            slots: 0,
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
        if self.slots > 0 {
            #[cfg(target_arch = "wasm32")]
            crate::jit_wasm::reset();
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
        match jit.blocks.get_mut(&pa) {
            Some(Entry::Code(r)) => {
                if r.epoch != jit.epoch {
                    let lo = page + r.lo as usize;
                    if ram[lo..lo + r.bytes.len()] != *r.bytes {
                        jit.blocks.remove(&pa);
                        jit.invalidated += 1;
                        // compile the new code at once: it ran before
                        jit.heat.insert(pa, jit.hot);
                        continue;
                    }
                    r.epoch = jit.epoch;
                }
                let f = r.f;
                h.jit_ut = h.tlb.utlb.as_ptr() as usize + h.utlb_table() * 2 * UT * std::mem::size_of::<crate::mmu::UEntry>();
                // SAFETY: `f` is a compiled region (in browser builds a function table index of
                // type (i32) -> i32, which is this ABI); it reads and writes the hart through the
                // pointer and calls `h_step` with the bus in `jit_bus`, which outlive the call
                let f: BlockFn = unsafe { std::mem::transmute::<usize, BlockFn>(f) };
                f(h as *mut Hart);
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
                if jit.slots >= SLOTS {
                    jit.flush();
                }
                let entry = compile(h, &ram[page..page + 0x1000], pc);
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

/// Compile the region entered at `pc` (whose page holds `code`).
#[allow(unused_variables)]
fn compile(h: &Hart, code: &[u8], pc: u64) -> Entry {
    #[cfg(target_arch = "wasm32")]
    if let Some((f, lo, hi)) = crate::jit_wasm::compile(code, pc, h.jit_step as u32) {
        return Entry::Code(Region { f, epoch: u64::MAX, lo, bytes: code[lo as usize..hi as usize].into() });
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
fn mem_access(i: u32) -> Option<(usize, u64, u8, bool)> {
    let rs1 = ((i >> 15) & 31) as usize;
    let f3 = (i >> 12) & 7;
    let imm_i = ((i as i32) >> 20) as i64 as u64;
    let imm_s = ((((i as i32) >> 25) << 5) | ((i >> 7) & 31) as i32) as i64 as u64;
    match i & 0x7f {
        0x03 if f3 != 7 => Some((rs1, imm_i, 1 << (f3 & 3), false)),
        0x07 if f3 == 2 || f3 == 3 => Some((rs1, imm_i, 1 << f3, false)),
        0x23 if f3 < 4 => Some((rs1, imm_s, 1 << f3, true)),
        0x27 if f3 == 2 || f3 == 3 => Some((rs1, imm_s, 1 << f3, true)),
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
        0x73 => 2, // CSRs, privileged instructions, ecall
        0x0f if (i >> 12) & 7 != 0 => 2, // fence.i: the driver starts a new epoch
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
                    h.tlb.ufill(table, store, va, host as u32);
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
