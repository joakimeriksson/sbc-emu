//! The QPU JIT: runs of QPU instructions become blocks of the shared `simd-jit` IR, then native
//! code (AArch64 NEON) or WebAssembly. The lowering follows [`Qpu::alu`] and [`Qpu::step`]
//! exactly, instruction by instruction (reads, both operations, packing, the writes in the
//! add-then-multiply order, r5 from a varying, the flags, then the signal), so a block leaves
//! the registers, accumulators and flags as the interpreter would.
//!
//! A block takes ALU instructions and load immediates whose reads and writes stay within the
//! QPU: the register files, r0..r4 (the SFU's results into r4), and what the hardware hands
//! over in order, fetched just before the block runs, in the order the interpreter would
//! fetch it: uniforms, varyings (and their C coefficients into r5), the pixel coordinates and
//! multisample flags, TMU results and colour, coverage and alpha-mask loads into r4. It ends
//! before a branch, a semaphore, the program end, a read of the VPM or the mutex, a write to
//! r5 or to anything outside the QPU (TMU, TLB, VPM, uniforms address, host interrupt), and a
//! rotation by r5. Those run in the interpreter, and so does every instruction in a branch's
//! delay slots or after a program end.
//!
//! Blocks are compiled once per distinct run of instruction words (a global cache) and looked
//! up by address in each worker's [`Code`] (cleared with it, per job). `SBC_VC4_JIT=0` turns
//! the JIT off.

use crate::mem::Mem;
use crate::qpu::{add_float_in, add_float_out, decode, op, raddr, sig, small_imm, waddr, Env, Inst, Kind, Qpu, V, ZERO};
use simd_jit::{Bin, Builder, Id, Un};
use std::collections::HashMap;
use std::sync::{Arc, Mutex};

/// Register numbers in the block's state: regfile A, regfile B, the accumulators r0..r5.
const RA: u16 = 0;
const RB: u16 = 32;
const ACC: u16 = 64;
pub const NREGS: usize = 70;
/// Flags: Z, N, C.
const FZ: u8 = 0;
const FN: u8 = 1;
const FC: u8 = 2;

/// Instructions in a block at most, and at least (a single instruction is no faster as code).
const MAX_INSNS: usize = 256;
const MIN_INSNS: usize = 2;

/// What a block needs fetched before it runs, in order: words (uniforms, the QPU's number) go
/// to the arguments, vectors to the inputs.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Fetch {
    Unif,
    QpuNum,
    /// a varying: its value, then its C coefficient (two inputs)
    Vary,
    PixelX,
    PixelY,
    MsFlags,
    RevFlag,
    Tmu(u8),
    ColorLoad,
    CoverageLoad,
    AlphaMaskLoad,
}

/// A compiled run of instructions.
#[derive(Debug)]
pub struct Block {
    pub code: simd_jit::Code,
    /// instructions
    pub len: u32,
    pub fetch: Vec<Fetch>,
}

/// Whether the JIT is on: where `simd-jit` has a backend, unless `SBC_VC4_JIT=0` (or turned off
/// with `set_enabled`).
pub fn enabled() -> bool {
    static ENV: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    simd_jit::available() && ON.load(std::sync::atomic::Ordering::Relaxed) && *ENV.get_or_init(|| std::env::var("SBC_VC4_JIT").map_or(true, |v| v != "0"))
}

/// `SBC_VC4_JIT_DUMP=1`: each block compiled, its instructions and the size of its IR and code.
fn dump() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| std::env::var("SBC_VC4_JIT_DUMP").is_ok_and(|v| v != "0"))
}

static ON: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(true);

/// Turns the JIT on or off (for tests and the browser page's switch).
pub fn set_enabled(on: bool) {
    ON.store(on, std::sync::atomic::Ordering::Relaxed);
}

/// In the browser: the function that says how often the page has freed the JIT's table slots
/// (the CPU JIT's, `arm_a64::wasm_jit_resets`), which blocks' modules share.
#[cfg(target_arch = "wasm32")]
pub fn set_reset_count(f: fn() -> u32) {
    simd_jit::set_reset_count(f);
}

/// Held by whatever turns the JIT off and on to compare it with the interpreter.
pub static SWITCH: Mutex<()> = Mutex::new(());

static BLOCKS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
static RUNS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
static INSNS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// Instructions run in blocks so far (counted as [`runs`]).
pub fn insns() -> u64 {
    INSNS.load(std::sync::atomic::Ordering::Relaxed)
}

/// Blocks compiled so far.
pub fn blocks() -> u64 {
    BLOCKS.load(std::sync::atomic::Ordering::Relaxed)
}

/// Blocks run so far (each worker adds its count when its cache is next cleared, at the start
/// of a job).
pub fn runs() -> u64 {
    RUNS.load(std::sync::atomic::Ordering::Relaxed)
}

// ---------------------------------------------------------------------------- lowering

struct Lower {
    b: Builder,
    fetch: Vec<Fetch>,
    nargs: u16,
    ninputs: u16,
    qpu_num: Option<u16>,
}

impl Lower {
    fn new() -> Self {
        Lower { b: Builder::new(), fetch: Vec::new(), nargs: 0, ninputs: 0, qpu_num: None }
    }
    fn arg(&mut self, f: Fetch) -> Id {
        self.fetch.push(f);
        self.nargs += 1;
        self.b.arg(self.nargs - 1)
    }
    fn input(&mut self, f: Fetch) -> Id {
        self.fetch.push(f);
        self.ninputs += 1;
        self.b.input(self.ninputs - 1)
    }
    fn ftz(&mut self, x: Id) -> Id {
        self.b.un(Un::Ftz, x)
    }
    fn fimm(&mut self, x: f32) -> Id {
        self.b.imm(x.to_bits())
    }
    /// A float in [0, 1] as an 8-bit unorm value ([`crate::qpu::unorm8`]).
    fn unorm8(&mut self, u: Id) -> Id {
        let x = self.ftz(u);
        let k255 = self.fimm(255.0);
        let t = self.b.bin(Bin::FMul, x, k255);
        let half = self.fimm(0.5);
        let t = self.b.bin(Bin::FAdd, t, half);
        let c = self.b.un(Un::FToU, t);
        let one = self.fimm(1.0);
        let ge1 = self.b.bin(Bin::FLe, one, x);
        let i255 = self.b.imm(255);
        let r = self.b.sel(ge1, i255, c);
        let zero = self.b.imm(0);
        let gt0 = self.b.bin(Bin::FLt, zero, x);
        self.b.sel(gt0, r, zero)
    }
    /// `v` into byte `m` (4..7) of `old`.
    fn merge_byte(&mut self, old: Id, b: Id, m: u8) -> Id {
        let s = 8 * (m as u32 - 4);
        let keep = self.b.bin_imm(Bin::And, old, !(0xff << s));
        let b = self.b.shl(b, s);
        self.b.bin(Bin::Or, keep, b)
    }
    fn cond(&mut self, c: u8) -> Id {
        match c {
            0 => self.b.simm(0),
            1 => self.b.simm(0xffff),
            _ => {
                let f = self.b.flag([FZ, FN, FC][(c as usize - 2) / 2]);
                if c % 2 == 1 {
                    self.b.snot(f)
                } else {
                    f
                }
            }
        }
    }

    /// A read of port A or B: the value, and the C coefficient of a varying.
    fn read(&mut self, r: u8, b: bool) -> Option<(Id, Option<Id>)> {
        Some(match r {
            0..=31 => (self.b.reg(if b { RB } else { RA } + r as u16), None),
            raddr::UNIF => (self.arg(Fetch::Unif), None),
            raddr::VARY => {
                let v = self.input(Fetch::Vary);
                self.ninputs += 1;
                let c = self.b.input(self.ninputs - 1);
                (v, Some(c))
            }
            raddr::ELEM_QPU if b => {
                let k = match self.qpu_num {
                    Some(k) => k,
                    None => {
                        self.fetch.push(Fetch::QpuNum);
                        self.nargs += 1;
                        self.qpu_num = Some(self.nargs - 1);
                        self.nargs - 1
                    }
                };
                (self.b.arg(k), None)
            }
            raddr::ELEM_QPU => (self.b.lane(), None),
            raddr::XY_PIXEL => (self.input(if b { Fetch::PixelY } else { Fetch::PixelX }), None),
            raddr::MS_REV => (self.input(if b { Fetch::RevFlag } else { Fetch::MsFlags }), None),
            // (the interpreter's `read` gives zero for the rest)
            raddr::VPM | raddr::MUTEX => return None,
            _ => (self.b.imm(0), None),
        })
    }

    /// [`crate::qpu::unpack`].
    fn unpack(&mut self, mode: u8, v: Id, float_in: bool) -> Id {
        match mode {
            0 => v,
            1 | 2 => {
                if float_in {
                    let h = if mode == 1 { self.b.bin_imm(Bin::And, v, 0xffff) } else { self.b.shr(v, 16) };
                    self.b.half_to_f32_ftz(h)
                } else if mode == 1 {
                    let t = self.b.shl(v, 16);
                    self.b.sar(t, 16)
                } else {
                    self.b.sar(v, 16)
                }
            }
            _ => {
                let byte = if mode == 3 {
                    self.b.shr(v, 24)
                } else {
                    let t = self.b.shr(v, 8 * (mode as u32 - 4));
                    self.b.bin_imm(Bin::And, t, 0xff)
                };
                if float_in {
                    let f = self.b.un(Un::UToF, byte);
                    let k = self.fimm(255.0);
                    self.b.bin(Bin::FDiv, f, k)
                } else if mode == 3 {
                    self.b.bin_imm(Bin::Mul, byte, 0x0101_0101)
                } else {
                    byte
                }
            }
        }
    }

    /// [`crate::qpu::add_op`] (`add_vec`).
    fn add_op(&mut self, o: u8, a: Id, b: Id) -> Id {
        let f2 = |l: &mut Lower, op: Bin| {
            let (x, y) = (l.ftz(a), l.ftz(b));
            l.b.bin(op, x, y)
        };
        match o {
            op::FADD | op::FSUB => {
                let r = f2(self, if o == op::FADD { Bin::FAdd } else { Bin::FSub });
                self.ftz(r)
            }
            op::FMIN => f2(self, Bin::FMin),
            op::FMAX => f2(self, Bin::FMax),
            op::FMINABS | op::FMAXABS => {
                let (x, y) = (self.ftz(a), self.ftz(b));
                let x = self.b.bin_imm(Bin::And, x, 0x7fff_ffff);
                let y = self.b.bin_imm(Bin::And, y, 0x7fff_ffff);
                self.b.bin(if o == op::FMINABS { Bin::FMinNum } else { Bin::FMaxNum }, x, y)
            }
            op::FTOI => {
                let x = self.ftz(a);
                self.b.un(Un::FToI, x)
            }
            op::ITOF => self.b.un(Un::IToF, a),
            op::ADD => self.b.bin(Bin::Add, a, b),
            op::SUB => self.b.bin(Bin::Sub, a, b),
            op::SHR => self.b.bin(Bin::ShrU, a, b),
            op::ASR => self.b.bin(Bin::ShrS, a, b),
            op::ROR => self.b.bin(Bin::Ror, a, b),
            op::SHL => self.b.bin(Bin::Shl, a, b),
            op::MIN => self.b.bin(Bin::IMin, a, b),
            op::MAX => self.b.bin(Bin::IMax, a, b),
            op::AND => self.b.bin(Bin::And, a, b),
            op::OR => self.b.bin(Bin::Or, a, b),
            op::XOR => self.b.bin(Bin::Xor, a, b),
            op::NOT => self.b.un(Un::Not, a),
            op::CLZ => self.b.un(Un::Clz, a),
            op::A_V8ADDS => self.b.bin(Bin::V8AddSat, a, b),
            op::A_V8SUBS => self.b.bin(Bin::V8SubSat, a, b),
            _ => self.b.imm(0),
        }
    }

    /// [`crate::qpu::mul_op`] (`mul_vec`).
    fn mul_op(&mut self, o: u8, a: Id, b: Id) -> Id {
        match o {
            op::FMUL => {
                let (x, y) = (self.ftz(a), self.ftz(b));
                let r = self.b.bin(Bin::FMul, x, y);
                self.ftz(r)
            }
            op::MUL24 => {
                let x = self.b.bin_imm(Bin::And, a, 0xff_ffff);
                let y = self.b.bin_imm(Bin::And, b, 0xff_ffff);
                self.b.bin(Bin::Mul, x, y)
            }
            op::V8MULD => self.b.bin(Bin::V8MulD, a, b),
            op::V8MIN => self.b.bin(Bin::V8Min, a, b),
            op::V8MAX => self.b.bin(Bin::V8Max, a, b),
            op::M_V8ADDS => self.b.bin(Bin::V8AddSat, a, b),
            op::M_V8SUBS => self.b.bin(Bin::V8SubSat, a, b),
            _ => self.b.imm(0),
        }
    }

    /// [`crate::qpu::pack_a`]: `sat` is the 32-bit saturating result of an add or subtract.
    fn pack_a(&mut self, mode: u8, old: Id, v: Id, float_out: bool, sat: Option<Id>) -> Id {
        let saturate = mode >= 8;
        match mode {
            0 => v,
            8 => sat.unwrap_or(v),
            1 | 2 | 9 | 10 => {
                let h = if float_out {
                    let x = self.ftz(v);
                    self.b.f32_to_half_ftz(x)
                } else if saturate {
                    let c = self.b.clamp(v, -32768, 32767);
                    self.b.bin_imm(Bin::And, c, 0xffff)
                } else {
                    self.b.bin_imm(Bin::And, v, 0xffff)
                };
                if mode == 1 || mode == 9 {
                    let keep = self.b.bin_imm(Bin::And, old, 0xffff_0000);
                    self.b.bin(Bin::Or, keep, h)
                } else {
                    let keep = self.b.bin_imm(Bin::And, old, 0xffff);
                    let h = self.b.shl(h, 16);
                    self.b.bin(Bin::Or, keep, h)
                }
            }
            _ => {
                let byte = if float_out {
                    self.unorm8(v)
                } else if saturate {
                    self.b.clamp(v, 0, 255)
                } else {
                    self.b.bin_imm(Bin::And, v, 0xff)
                };
                match mode & 7 {
                    3 => self.b.bin_imm(Bin::Mul, byte, 0x0101_0101),
                    m => self.merge_byte(old, byte, m),
                }
            }
        }
    }

    /// [`crate::qpu::pack_mul`].
    fn pack_mul(&mut self, mode: u8, old: Id, v: Id) -> Id {
        match mode {
            3 => {
                let b = self.unorm8(v);
                self.b.bin_imm(Bin::Mul, b, 0x0101_0101)
            }
            4..=7 => {
                let b = self.unorm8(v);
                self.merge_byte(old, b, mode)
            }
            _ => v,
        }
    }

    /// The current value of a destination ([`Qpu::peek`]).
    fn peek(&mut self, w: u8, file_a: bool) -> Id {
        match w {
            0..=31 => self.b.reg(if file_a { RA } else { RB } + w as u16),
            waddr::R0..=waddr::R3 => self.b.reg(ACC + (w - waddr::R0) as u16),
            waddr::R5 => self.b.reg(ACC + 5),
            _ => self.b.imm(0),
        }
    }

    /// A write to a destination in the lanes of `mask` ([`Qpu::write`]), or None if blocks
    /// cannot have it.
    fn write(&mut self, w: u8, file_a: bool, v: Id, mask: Id) -> Option<()> {
        match w {
            0..=31 => self.b.store(if file_a { RA } else { RB } + w as u16, v, mask),
            waddr::R0..=waddr::R3 => self.b.store(ACC + (w - waddr::R0) as u16, v, mask),
            waddr::TMU_NOSWAP | waddr::NOP | waddr::QUAD_XY => {}
            waddr::MS_FLAGS if !file_a => {}
            waddr::SFU_RECIP..=waddr::SFU_LOG => {
                let x = self.ftz(v);
                let y = match w {
                    waddr::SFU_RECIP => {
                        let one = self.fimm(1.0);
                        self.b.bin(Bin::FDiv, one, x)
                    }
                    waddr::SFU_RECIPSQRT => {
                        let s = self.b.un(Un::FSqrt, x);
                        let one = self.fimm(1.0);
                        self.b.bin(Bin::FDiv, one, s)
                    }
                    waddr::SFU_EXP => self.b.un(Un::Exp2, x),
                    _ => self.b.un(Un::Log2, x),
                };
                let y = self.ftz(y);
                self.b.store(ACC + 4, y, mask);
            }
            _ => return None,
        }
        Some(())
    }

    /// [`Qpu::write_result`].
    #[allow(clippy::too_many_arguments)]
    fn write_result(&mut self, i: &Inst, w: u8, file_a: bool, v: Id, mask: Id, float_out: bool, is_add: bool, sat: Option<Id>) -> Option<()> {
        if w == waddr::NOP {
            return Some(());
        }
        let v = if i.pm {
            if !is_add && i.pack != 0 {
                let old = self.peek(w, file_a);
                self.pack_mul(i.pack, old, v)
            } else {
                v
            }
        } else if i.pack != 0 && file_a && w < 32 {
            let old = self.b.reg(RA + w as u16);
            self.pack_a(i.pack, old, v, float_out, sat)
        } else {
            v
        };
        self.write(w, file_a, v, mask)
    }

    /// [`Qpu::set_flags`].
    fn set_flags(&mut self, r: Id, float: bool, carry: Option<Id>, mask: Id) {
        let (z, n, c) = if float {
            let x = self.ftz(r);
            let zero = self.b.imm(0);
            let z = self.b.bin(Bin::FEq, x, zero);
            let n = self.b.bin(Bin::FLt, x, zero);
            let c = self.b.bin(Bin::FLt, zero, x);
            (self.b.bits(z), self.b.bits(n), self.b.bits(c))
        } else {
            let zero = self.b.imm(0);
            let z = self.b.bin(Bin::IEq, r, zero);
            let n = self.b.bin(Bin::ILt, r, zero);
            let c = match carry {
                Some(c) => self.b.bits(c),
                None => self.b.simm(0),
            };
            (self.b.bits(z), self.b.bits(n), c)
        };
        self.b.store_flag(FZ, z, mask);
        self.b.store_flag(FN, n, mask);
        self.b.store_flag(FC, c, mask);
    }

    fn alu(&mut self, i: &Inst) -> Option<()> {
        let small = i.sig == sig::SMALL_IMM;
        if i.raddr_a == raddr::MUTEX || (!small && i.raddr_b == raddr::MUTEX) || (small && i.raddr_b == 48) {
            return None;
        }
        let (va, r5a) = if i.raddr_a != raddr::NOP { self.read(i.raddr_a, false)? } else { (self.b.imm(0), None) };
        let (vb, r5b) = if small {
            (self.b.imm(small_imm(i.raddr_b)), None)
        } else if i.raddr_b != raddr::NOP {
            self.read(i.raddr_b, true)?
        } else {
            (self.b.imm(0), None)
        };
        let r5 = r5b.or(r5a);
        let (af, mf) = (add_float_in(i.op_add), i.op_mul == op::FMUL);
        let operand = |l: &mut Lower, mux: u8, float_in: bool| -> Id {
            let raw = match mux {
                0..=5 => l.b.reg(ACC + mux as u16),
                6 => va,
                _ => vb,
            };
            if i.unpack != 0 && ((!i.pm && mux == 6) || (i.pm && mux == 4)) {
                l.unpack(i.unpack, raw, float_in)
            } else {
                raw
            }
        };
        let sat32 = !i.pm && i.pack == 8;
        let add = if i.op_add != op::A_NOP {
            let (a, b) = (operand(self, i.add_a, af), operand(self, i.add_b, af));
            let r = self.add_op(i.op_add, a, b);
            let carry = if !i.sf {
                None
            } else {
                match i.op_add {
                    op::ADD => Some(self.b.bin(Bin::ULt, r, a)),
                    op::SUB => Some(self.b.bin(Bin::ULt, a, b)),
                    _ => None,
                }
            };
            let sat = if !sat32 {
                None
            } else {
                match i.op_add {
                    op::ADD => Some(self.b.add_sat(a, b)),
                    op::SUB => Some(self.b.sub_sat(a, b)),
                    _ => None,
                }
            };
            Some((r, carry, sat))
        } else {
            None
        };
        let mul = if i.op_mul != op::M_NOP {
            let (mut a, mut b) = (operand(self, i.mul_a, mf), operand(self, i.mul_b, mf));
            if small && i.raddr_b > 48 {
                let n = (i.raddr_b - 48) as usize;
                a = self.b.rotate(a, n);
                b = self.b.rotate(b, n);
            }
            Some(self.mul_op(i.op_mul, a, b))
        } else {
            None
        };
        let ma = self.cond(i.cond_add);
        let mm = self.cond(i.cond_mul);
        if let Some((r, _, sat)) = add {
            self.write_result(i, i.waddr_add, !i.ws, r, ma, add_float_out(i.op_add), true, sat)?;
        }
        if let Some(r) = mul {
            self.write_result(i, i.waddr_mul, i.ws, r, mm, i.op_mul == op::FMUL, false, None)?;
        }
        if let Some(c) = r5 {
            self.b.store_all(ACC + 5, c);
        }
        if i.sf {
            if let Some((r, carry, _)) = add {
                self.set_flags(r, add_float_out(i.op_add), carry, ma);
            } else if let Some(r) = mul {
                self.set_flags(r, i.op_mul == op::FMUL, None, mm);
            }
        }
        Some(())
    }

    fn ldi(&mut self, i: &Inst, mode: u8) -> Option<()> {
        let v = match mode {
            1 => self.b.vconst(std::array::from_fn(|l| {
                let x = (i.imm >> (16 + l) & 1) << 1 | (i.imm >> l & 1);
                ((x as i32) << 30 >> 30) as u32
            })),
            3 => self.b.vconst(std::array::from_fn(|l| (i.imm >> (16 + l) & 1) << 1 | (i.imm >> l & 1))),
            _ => self.b.imm(i.imm),
        };
        let ma = self.cond(i.cond_add);
        let mm = self.cond(i.cond_mul);
        self.write_result(i, i.waddr_add, !i.ws, v, ma, false, true, None)?;
        self.write_result(i, i.waddr_mul, i.ws, v, mm, false, false, None)?;
        if i.sf {
            self.set_flags(v, false, None, ma);
        }
        Some(())
    }

    /// One instruction, or None (and nothing of it) if blocks cannot have it.
    fn inst(&mut self, i: &Inst) -> Option<()> {
        let (mark, nf, na, ni, q) = (self.b.mark(), self.fetch.len(), self.nargs, self.ninputs, self.qpu_num);
        let r = self.inst_inner(i);
        if r.is_none() {
            self.b.reset(mark);
            self.fetch.truncate(nf);
            (self.nargs, self.ninputs, self.qpu_num) = (na, ni, q);
        }
        r
    }

    fn inst_inner(&mut self, i: &Inst) -> Option<()> {
        match i.kind {
            Kind::Branch | Kind::Sem(..) => return None,
            Kind::Ldi(mode) => return self.ldi(i, mode),
            Kind::Alu => {}
        }
        if !matches!(
            i.sig,
            sig::BREAK
                | sig::NONE
                | sig::THRSW
                | sig::WAIT_SCORE
                | sig::UNLOCK_SCORE
                | sig::LAST_THRSW
                | sig::SMALL_IMM
                | sig::LOAD_TMU0
                | sig::LOAD_TMU1
                | sig::COLOR_LOAD
                | sig::COVERAGE_LOAD
                | sig::ALPHA_MASK_LOAD
        ) {
            return None;
        }
        self.alu(i)?;
        let r4 = match i.sig {
            sig::COLOR_LOAD => Some(Fetch::ColorLoad),
            sig::LOAD_TMU0 => Some(Fetch::Tmu(0)),
            sig::LOAD_TMU1 => Some(Fetch::Tmu(1)),
            sig::COVERAGE_LOAD => Some(Fetch::CoverageLoad),
            sig::ALPHA_MASK_LOAD => Some(Fetch::AlphaMaskLoad),
            _ => None,
        };
        if let Some(f) = r4 {
            let v = self.input(f);
            self.b.store_all(ACC + 4, v);
        }
        Some(())
    }
}

/// The IR of the longest run of instructions from the start of `words` that blocks can have
/// (at most `MAX_INSNS`), and how many those are.
pub fn lower(words: &[u64]) -> (usize, simd_jit::Block, Vec<Fetch>) {
    let mut l = Lower::new();
    let mut n = 0;
    while n < words.len().min(MAX_INSNS) && l.b.len() < simd_jit::ir::MAX_NODES - 600 && l.inst(&decode(words[n])).is_some() {
        n += 1;
    }
    (n, l.b.finish(), l.fetch)
}

/// Compiled blocks by their instruction words (shared by all workers and jobs).
type Key = Vec<u64>;
static CACHE: Mutex<Option<HashMap<Key, Option<Arc<Block>>>>> = Mutex::new(None);
/// Blocks kept; past that the cache starts again.
const CACHE_MAX: usize = 1 << 14;

/// The block at `pc`, compiled or from the cache; None if fewer than `MIN_INSNS` instructions
/// there can be one.
fn block_at(pc: u32, mem: &mut Mem) -> Option<Arc<Block>> {
    let mut words = Vec::with_capacity(32);
    // the run's length first (lowering is cheap next to compiling)
    let mut l = Lower::new();
    let mut a = pc;
    while words.len() < MAX_INSNS && l.b.len() < simd_jit::ir::MAX_NODES - 600 {
        let w = mem.r32(a) as u64 | (mem.r32(a.wrapping_add(4)) as u64) << 32;
        if l.inst(&decode(w)).is_none() {
            break;
        }
        words.push(w);
        a = a.wrapping_add(8);
    }
    if words.len() < MIN_INSNS {
        return None;
    }
    let mut cache = CACHE.lock().unwrap_or_else(|e| e.into_inner());
    let map = cache.get_or_insert_with(HashMap::new);
    if let Some(b) = map.get(&words) {
        return b.clone();
    }
    if map.len() >= CACHE_MAX {
        map.clear();
    }
    let block = l.b.finish();
    let code = simd_jit::Code::new(block);
    if dump() {
        let mut kinds = std::collections::BTreeMap::<String, usize>::new();
        for n in &code.block.ir {
            let k = format!("{n:?}");
            let k = match n {
                simd_jit::Node::Un(op, _) => format!("Un {op:?}"),
                simd_jit::Node::Bin(op, ..) => format!("Bin {op:?}"),
                _ => k[..k.find('(').unwrap_or(k.len())].to_string(),
            };
            *kinds.entry(k).or_default() += 1;
        }
        eprintln!("vc4 jit: block at {pc:#x}, {} instructions, {} nodes, {} bytes of code\n{}\n  {kinds:?}", words.len(), code.block.ir.len(), code.code_bytes(),
                  words.iter().map(|&w| format!("  {}", crate::disasm::disasm(w))).collect::<Vec<_>>().join("\n"));
    }
    let b = code.compiled().then(|| Arc::new(Block { code, len: words.len() as u32, fetch: l.fetch }));
    BLOCKS.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    map.insert(words, b.clone());
    b
}

// ---------------------------------------------------------------------------- running

/// A worker's blocks by address (direct-mapped, as its decoded instructions), and the buffers
/// a block's fetches go into.
#[derive(Default)]
pub struct Cache {
    tags: Vec<u32>,
    slots: Vec<Option<Arc<Block>>>,
    args: Vec<u32>,
    inputs: Vec<V>,
    /// blocks run and their instructions (added to the global counts when the cache is cleared)
    runs: u64,
    insns: u64,
}

const SLOTS: usize = 4096;

impl Cache {
    /// Blocks this cache ran since it was last cleared.
    pub fn runs(&self) -> u64 {
        self.runs
    }

    pub fn clear(&mut self) {
        self.tags.iter_mut().for_each(|t| *t = u32::MAX);
        self.slots.iter_mut().for_each(|s| *s = None);
        RUNS.fetch_add(std::mem::take(&mut self.runs), std::sync::atomic::Ordering::Relaxed);
        INSNS.fetch_add(std::mem::take(&mut self.insns), std::sync::atomic::Ordering::Relaxed);
    }

    /// Runs the block at the QPU's instruction if there is one of at most `left` instructions:
    /// how many it ran (0: none, the interpreter goes on).
    #[inline]
    pub fn run(&mut self, q: &mut Qpu, mem: &mut Mem, env: &mut dyn Env, left: u64) -> u32 {
        if self.tags.is_empty() {
            self.tags = vec![u32::MAX; SLOTS];
            self.slots = vec![None; SLOTS];
        }
        let k = (q.pc >> 3) as usize & (SLOTS - 1);
        if self.tags[k] != q.pc {
            self.tags[k] = q.pc;
            self.slots[k] = block_at(q.pc, mem);
        }
        let Some(b) = &self.slots[k] else { return 0 };
        if b.len as u64 > left {
            return 0;
        }
        self.args.clear();
        self.inputs.clear();
        for f in &b.fetch {
            match *f {
                Fetch::Unif => self.args.push(q.uniform(mem)),
                Fetch::QpuNum => self.args.push(q.num),
                Fetch::Vary => {
                    let (v, c) = env.vary();
                    self.inputs.push(v);
                    self.inputs.push(c);
                }
                Fetch::PixelX => self.inputs.push(env.pixel_xy().0),
                Fetch::PixelY => self.inputs.push(env.pixel_xy().1),
                Fetch::MsFlags => self.inputs.push(env.ms_flags()),
                Fetch::RevFlag => self.inputs.push(env.rev_flag()),
                Fetch::Tmu(t) => {
                    let v = q.tmu[t as usize].fifo.pop_front().unwrap_or_else(|| {
                        crate::log_first("vc4: TMU load with no request outstanding");
                        ZERO
                    });
                    self.inputs.push(v);
                }
                Fetch::ColorLoad => self.inputs.push(env.color_load()),
                Fetch::CoverageLoad => self.inputs.push(env.coverage_load()),
                Fetch::AlphaMaskLoad => self.inputs.push(env.alpha_mask_load()),
            }
        }
        let mut flags = [q.z as u32, q.n as u32, q.c as u32];
        b.code.run(q.regs_mut(), &mut flags, &self.args, &self.inputs, &mut []);
        (q.z, q.n, q.c) = (flags[0] as u16, flags[1] as u16, flags[2] as u16);
        q.pc = q.pc.wrapping_add(8 * b.len);
        q.insns += b.len as u64;
        self.runs += 1;
        self.insns += b.len as u64;
        b.len
    }
}
