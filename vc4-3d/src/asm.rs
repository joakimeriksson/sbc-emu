//! A small QPU instruction builder (field layout of Mesa's `vc4_qpu_defines.h`), for tests and
//! hand-written programs:
//!
//! ```
//! use vc4_3d::asm::*;
//! use vc4_3d::qpu::{op, waddr, raddr};
//! // r0 = ra1 + rb2 (float); rb3 = r0 * r0
//! let i = nop().add(op::FADD, waddr::R0, A, B).ra(1).rb(2);
//! let j = nop().mul(op::FMUL, 3, R0, R0);
//! # let _ = (i.0, j.0, raddr::UNIF);
//! ```

use crate::qpu::{raddr, sig, waddr};

/// Input muxes: accumulators, register file A (raddr_a), register file B (raddr_b or the small
/// immediate).
pub const R0: u8 = 0;
pub const R1: u8 = 1;
pub const R2: u8 = 2;
pub const R3: u8 = 3;
pub const R4: u8 = 4;
pub const R5: u8 = 5;
pub const A: u8 = 6;
pub const B: u8 = 7;

pub const COND_NEVER: u8 = 0;
pub const COND_ALWAYS: u8 = 1;
pub const COND_ZS: u8 = 2;
pub const COND_ZC: u8 = 3;
pub const COND_NS: u8 = 4;
pub const COND_NC: u8 = 5;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct I(pub u64);

fn set(w: u64, v: u64, hi: u32, lo: u32) -> u64 {
    let m = ((1u64 << (hi - lo + 1)) - 1) << lo;
    (w & !m) | (v << lo & m)
}

/// A no-op: no operations, no writes, no reads.
pub fn nop() -> I {
    let mut w = 0u64;
    w = set(w, sig::NONE as u64, 63, 60);
    w = set(w, waddr::NOP as u64, 43, 38);
    w = set(w, waddr::NOP as u64, 37, 32);
    w = set(w, raddr::NOP as u64, 23, 18);
    w = set(w, raddr::NOP as u64, 17, 12);
    I(w)
}

/// Load a 32-bit immediate into `dst` (add pipe, register file A side unless `ws`).
pub fn ldi(dst: u8, imm: u32) -> I {
    let mut w = 0u64;
    w = set(w, sig::LOAD_IMM as u64, 63, 60);
    w = set(w, 1, 51, 49);
    w = set(w, dst as u64, 43, 38);
    w = set(w, waddr::NOP as u64, 37, 32);
    I(w | imm as u64)
}

/// Semaphore `n`: increment, or decrement (waits at zero).
pub fn sem(n: u8, down: bool) -> I {
    let mut w = 0u64;
    w = set(w, sig::LOAD_IMM as u64, 63, 60);
    w = set(w, 4, 59, 57);
    w = set(w, waddr::NOP as u64, 43, 38);
    w = set(w, waddr::NOP as u64, 37, 32);
    I(w | (n as u64 & 15) | (down as u64) << 4)
}

/// Branch (condition 15: always) to `target` (relative to the instruction after the three
/// delay slots if `rel`), writing the return address to nothing.
pub fn branch(cond: u8, rel: bool, target: i32) -> I {
    let mut w = 0u64;
    w = set(w, sig::BRANCH as u64, 63, 60);
    w = set(w, cond as u64, 55, 52);
    w = set(w, rel as u64, 51, 51);
    w = set(w, waddr::NOP as u64, 43, 38);
    w = set(w, waddr::NOP as u64, 37, 32);
    I(w | target as u32 as u64)
}

impl I {
    /// Add-pipe operation writing `dst` (file A unless `ws`), always.
    pub fn add(self, op: u8, dst: u8, a: u8, b: u8) -> I {
        let mut w = self.0;
        w = set(w, op as u64, 28, 24);
        w = set(w, dst as u64, 43, 38);
        w = set(w, 1, 51, 49);
        w = set(w, a as u64, 11, 9);
        w = set(w, b as u64, 8, 6);
        I(w)
    }
    /// Multiply-pipe operation writing `dst` (file B unless `ws`), always.
    pub fn mul(self, op: u8, dst: u8, a: u8, b: u8) -> I {
        let mut w = self.0;
        w = set(w, op as u64, 31, 29);
        w = set(w, dst as u64, 37, 32);
        w = set(w, 1, 48, 46);
        w = set(w, a as u64, 5, 3);
        w = set(w, b as u64, 2, 0);
        I(w)
    }
    pub fn ra(self, r: u8) -> I { I(set(self.0, r as u64, 23, 18)) }
    pub fn rb(self, r: u8) -> I { I(set(self.0, r as u64, 17, 12)) }
    /// Small immediate in raddr_b (also the multiply pipe's rotations, 48..63).
    pub fn small(self, n: u8) -> I { I(set(set(self.0, n as u64, 17, 12), sig::SMALL_IMM as u64, 63, 60)) }
    pub fn ws(self) -> I { I(self.0 | 1 << 44) }
    pub fn sf(self) -> I { I(self.0 | 1 << 45) }
    pub fn cond_add(self, c: u8) -> I { I(set(self.0, c as u64, 51, 49)) }
    pub fn cond_mul(self, c: u8) -> I { I(set(self.0, c as u64, 48, 46)) }
    pub fn pack(self, p: u8) -> I { I(set(self.0, p as u64, 55, 52)) }
    pub fn pm(self) -> I { I(self.0 | 1 << 56) }
    pub fn unpack(self, u: u8) -> I { I(set(self.0, u as u64, 59, 57)) }
    pub fn sig(self, s: u8) -> I { I(set(self.0, s as u64, 63, 60)) }
}

/// Mov through the add pipe (OR a, a).
pub fn mov(dst: u8, src: u8) -> I { nop().add(crate::qpu::op::OR, dst, src, src) }

/// The usual program end: the end signal and its two delay slots.
pub fn end() -> [I; 3] { [nop().sig(sig::PROG_END), nop(), nop()] }

/// Assemble into little-endian bytes.
pub fn bytes(prog: &[I]) -> Vec<u8> { prog.iter().flat_map(|i| i.0.to_le_bytes()).collect() }
