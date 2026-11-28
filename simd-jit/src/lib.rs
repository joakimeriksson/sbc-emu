//! A JIT for 16-lane SIMD shader processors: the VideoCore IV's QPUs (`vc4-3d`) and V3D 4.2's
//! (`v3d`). A front end lowers a run of a shader's instructions, exactly as its interpreter
//! runs them, to a block of IR (`Builder`): 16-lane vectors of 32-bit words and lane masks. The
//! block becomes code here: AArch64 with NEON natively on 64-bit ARM (macOS, Linux), WebAssembly
//! with 128-bit SIMD in the browser (compiled by the page, as the CPU JIT's blocks are). `eval`
//! runs the IR itself: it is the reference the backends are tested against.
//!
//! The front end keeps its own state; a block sees it as
//! - `regs`: vector registers, numbered as the front end likes (the VideoCore IV: regfile A,
//!   regfile B, r0..r5; V3D: rf0..63, r0..r5, ...);
//! - `flags`: lane masks (the QPU's Z/N/C, V3D's A and B);
//! - `args`: words for every lane, `inputs`: vectors, both fetched by the front end before the
//!   block runs (uniforms, varyings, texture results: what the hardware hands over in order);
//! - `outputs`: vectors the block makes for the front end to act on after it (tile buffer or
//!   VPM writes).
//!
//! Adapted from the Jetson Nano's shader JIT (`maxwell-shader/src/jit`), which could move onto
//! this crate.

pub mod ir;
pub use ir::{eval, validate, Bin, Block, Builder, Id, Mark, Node, SOp, Un, LANES, V};

#[cfg(all(target_arch = "aarch64", any(target_os = "macos", target_os = "linux")))]
mod a64;
#[cfg(target_arch = "wasm32")]
mod wasm;
#[cfg(target_arch = "wasm32")]
pub use wasm::set_reset_count;

pub mod check;
#[cfg(test)]
mod tests;

use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};

/// True where there is a native backend (64-bit ARM, macOS or Linux).
pub const NATIVE: bool = cfg!(all(target_arch = "aarch64", any(target_os = "macos", target_os = "linux")));

/// True where blocks become code (natively on 64-bit ARM, or in the browser). Elsewhere `Code`
/// runs `eval`, which is slower than an interpreter: front ends should not use blocks then.
pub const fn available() -> bool {
    NATIVE || cfg!(target_arch = "wasm32")
}

// ---------------------------------------------------------------------------- host functions

const MAX_FNS: usize = 64;
static FNS: [AtomicUsize; MAX_FNS] = [const { AtomicUsize::new(0) }; MAX_FNS];
static NFNS: AtomicUsize = AtomicUsize::new(0);

/// Registers a function for `Un::Call` (lane by lane, on the word's bits); the number to call
/// it by. The same function returns the same number. At most 64.
pub fn register(f: fn(u32) -> u32) -> u8 {
    static LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
    let _l = LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let n = NFNS.load(Ordering::Acquire);
    if let Some(k) = (0..n).find(|&k| FNS[k].load(Ordering::Relaxed) == f as usize) {
        return k as u8;
    }
    assert!(n < MAX_FNS, "simd-jit: too many registered functions");
    FNS[n].store(f as usize, Ordering::Relaxed);
    NFNS.store(n + 1, Ordering::Release);
    n as u8
}

/// Registered function `k` on `x`.
pub fn call(k: u8, x: u32) -> u32 {
    let p = FNS[k as usize].load(Ordering::Relaxed);
    assert!(p != 0, "simd-jit: function {k} is not registered");
    // SAFETY: only `register` stores here, and only `fn(u32) -> u32` pointers
    let f = unsafe { std::mem::transmute::<usize, fn(u32) -> u32>(p) };
    f(x)
}

/// What the backends hand to `helper` lane by lane (the operations they have no instructions
/// for): a unary or binary operation's number.
fn helper_code(n: &Node) -> Option<u32> {
    Some(match *n {
        Node::Un(Un::Exp2, _) => 0,
        Node::Un(Un::Log2, _) => 1,
        Node::Un(Un::HalfToF, _) => 2,
        Node::Un(Un::FToHalf, _) => 3,
        Node::Bin(Bin::FMinNum, ..) => 4,
        Node::Bin(Bin::FMaxNum, ..) => 5,
        Node::Un(Un::Call(k), _) => 256 + k as u32,
        _ => return None,
    })
}

fn helper_apply(code: u32, x: u32, y: u32) -> u32 {
    match code {
        0 => ir::unary(Un::Exp2, x),
        1 => ir::unary(Un::Log2, x),
        2 => ir::unary(Un::HalfToF, x),
        3 => ir::unary(Un::FToHalf, x),
        4 => ir::binary(Bin::FMinNum, x, y),
        5 => ir::binary(Bin::FMaxNum, x, y),
        _ => call((code - 256) as u8, x),
    }
}

// ---------------------------------------------------------------------------- code

static BLOCKS: AtomicU64 = AtomicU64::new(0);
static FAILED: AtomicU64 = AtomicU64::new(0);

/// Blocks compiled so far (all front ends), and blocks the backend could not compile (they run
/// `eval`).
pub fn stats() -> (u64, u64) {
    (BLOCKS.load(Ordering::Relaxed), FAILED.load(Ordering::Relaxed))
}

/// A compiled block. Once made it is only run, from any thread.
pub struct Code {
    pub block: Block,
    #[cfg(all(target_arch = "aarch64", any(target_os = "macos", target_os = "linux")))]
    native: Option<a64::Func>,
    #[cfg(target_arch = "wasm32")]
    wasm: Option<wasm::Module>,
}

impl std::fmt::Debug for Code {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        write!(f, "Code({} nodes, compiled: {})", self.block.ir.len(), self.compiled())
    }
}

impl Code {
    /// Compiles a block for the backend there is (`eval` runs it if there is none, or if the
    /// backend could not take it).
    pub fn new(block: Block) -> Code {
        debug_assert!(validate(&block).is_ok(), "{:?}", validate(&block));
        BLOCKS.fetch_add(1, Ordering::Relaxed);
        #[cfg(all(target_arch = "aarch64", any(target_os = "macos", target_os = "linux")))]
        {
            let native = a64::Func::new(&block);
            if native.is_none() {
                FAILED.fetch_add(1, Ordering::Relaxed);
            }
            Code { block, native }
        }
        #[cfg(target_arch = "wasm32")]
        {
            let wasm = wasm::Module::new(&block);
            if wasm.is_none() {
                FAILED.fetch_add(1, Ordering::Relaxed);
            }
            Code { block, wasm }
        }
        #[cfg(not(any(target_arch = "wasm32", all(target_arch = "aarch64", any(target_os = "macos", target_os = "linux")))))]
        Code { block }
    }

    /// The size of the block's code in bytes (0 if it has none).
    pub fn code_bytes(&self) -> usize {
        #[cfg(all(target_arch = "aarch64", any(target_os = "macos", target_os = "linux")))]
        return self.native.as_ref().map_or(0, |f| f.bytes);
        #[allow(unreachable_code)]
        0
    }

    /// Whether the block has code (else `run` evaluates the IR).
    pub fn compiled(&self) -> bool {
        #[cfg(all(target_arch = "aarch64", any(target_os = "macos", target_os = "linux")))]
        return self.native.is_some();
        #[cfg(target_arch = "wasm32")]
        return self.wasm.is_some();
        #[allow(unreachable_code)]
        false
    }

    /// Runs the block on the state. The slices must be at least as long as the block's
    /// `nregs`, `nflags`, `nargs`, `ninputs`, `nouts` (it panics otherwise).
    #[inline]
    pub fn run(&self, regs: &mut [V], flags: &mut [u32], args: &[u32], inputs: &[V], outputs: &mut [V]) {
        let b = &self.block;
        assert!(
            regs.len() >= b.nregs as usize
                && flags.len() >= b.nflags as usize
                && args.len() >= b.nargs as usize
                && inputs.len() >= b.ninputs as usize
                && outputs.len() >= b.nouts as usize,
            "simd-jit: the state is smaller than the block's"
        );
        #[cfg(all(target_arch = "aarch64", any(target_os = "macos", target_os = "linux")))]
        if let Some(f) = &self.native {
            // SAFETY: the slices are as long as the code reads and writes (checked above)
            unsafe { f.call(regs.as_mut_ptr() as *mut u32, flags.as_mut_ptr(), args.as_ptr(), inputs.as_ptr() as *const u32, outputs.as_mut_ptr() as *mut u32) };
            return;
        }
        #[cfg(target_arch = "wasm32")]
        if let Some(m) = &self.wasm {
            if m.run(regs.as_mut_ptr() as *mut u32, flags.as_mut_ptr(), args.as_ptr(), inputs.as_ptr() as *const u32, outputs.as_mut_ptr() as *mut u32) {
                return;
            }
        }
        eval(b, regs, flags, args, inputs, outputs)
    }
}
