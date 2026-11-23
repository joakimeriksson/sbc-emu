//! Decoding shared by the WebAssembly (`jit_wasm.rs`) and x86-64 (`jit_x64.rs`) backends of
//! the block JIT: the T32 encodings that map onto A32 ones, and the scan that picks a
//! region's basic blocks. The AArch64 backend (`jit.rs`, `jit_t32.rs`) has its own copies.
use super::writes_pc;

/// How a Thumb instruction is emitted.
pub(super) enum T {
    /// the equivalent A32 instruction (condition AL)
    A32(u32),
    /// emitted by the backend; true = the block ends
    Done(bool),
    /// interpreted; true = the block ends after it
    Interp(bool),
}

pub(super) fn sign_extend(value: u32, bits: u32) -> u32 {
    ((value << (32 - bits)) as i32 >> (32 - bits)) as u32
}

pub(super) fn t32_dp_register(hw1: u32, hw2: u32) -> T {
    let (rn, rd, rm) = (hw1 & 15, (hw2 >> 8) & 15, hw2 & 15);
    let (op1, op2) = ((hw1 >> 4) & 15, (hw2 >> 4) & 15);
    if hw2 & 0xf000 != 0xf000 {
        return T::Interp(true);
    }
    if op1 & 8 == 0 && op2 == 0 {
        let s = (hw1 >> 4) & 1;
        return T::A32(0xe1a0_0010 | s << 20 | rd << 12 | rm << 8 | ((op1 >> 1) & 3) << 5 | rn);
    }
    if op1 & 8 == 0 && op2 & 8 != 0 {
        let kind = match op1 & 7 {
            0 => 0x6b,
            1 => 0x6f,
            2 => 0x68,
            3 => 0x6c,
            4 => 0x6a,
            5 => 0x6e,
            _ => return T::Interp(true),
        };
        return T::A32(0xe000_0000 | kind << 20 | 0x70 | rn << 16 | rd << 12 | (op2 & 3) << 10 | rm);
    }
    if op1 & 0xc == 8 && op2 & 0xc == 8 {
        return match (op1 & 3, op2 & 3) {
            (1, 0) => T::A32(0xe6bf_0f30 | rd << 12 | rm),
            (1, 1) => T::A32(0xe6bf_0fb0 | rd << 12 | rm),
            (1, 2) => T::A32(0xe6ff_0f30 | rd << 12 | rm),
            (3, 0) => T::A32(0xe16f_0f10 | rd << 12 | rm),
            _ => T::Interp(false),
        };
    }
    T::Interp(false)
}

pub(super) fn t32_multiply(hw1: u32, hw2: u32) -> T {
    let (rn, ra, rd, rm) = (hw1 & 15, hw2 >> 12, (hw2 >> 8) & 15, hw2 & 15);
    match ((hw1 >> 4) & 7, (hw2 >> 4) & 3) {
        (0, 0) if ra == 15 => T::A32(0xe000_0090 | rd << 16 | rm << 8 | rn),
        (0, 0) => T::A32(0xe020_0090 | rd << 16 | ra << 12 | rm << 8 | rn),
        _ => T::Interp(false),
    }
}

pub(super) fn t32_long_multiply(hw1: u32, hw2: u32) -> T {
    let (rn, lo, hi, rm) = (hw1 & 15, hw2 >> 12, (hw2 >> 8) & 15, hw2 & 15);
    let fields = hi << 16 | lo << 12 | rm << 8 | rn;
    match ((hw1 >> 4) & 7, (hw2 >> 4) & 15) {
        (0, 0) => T::A32(0xe0c0_0090 | fields),
        (1, 15) => T::A32(0xe710_f010 | hi << 16 | rm << 8 | rn),
        (2, 0) => T::A32(0xe080_0090 | fields),
        (3, 15) => T::A32(0xe730_f010 | hi << 16 | rm << 8 | rn),
        (4, 0) => T::A32(0xe0e0_0090 | fields),
        (6, 0) => T::A32(0xe0a0_0090 | fields),
        _ => T::Interp(false),
    }
}

// ---------------------------------------------------------------------- regions
/// Constant branch targets of an A32 instruction and whether straight-line code ends there
/// (for choosing the region; the emitter decides the real block ends).
pub(super) fn scan_a32(insn: u32, pc: u32) -> (Option<u32>, bool) {
    let cond = insn >> 28;
    if cond == 15 {
        return (None, insn & 0x0e00_0000 == 0x0a00_0000);
    }
    if insn & 0x0e00_0000 == 0x0a00_0000 {
        let offset = (((insn & 0x00ff_ffff) << 8) as i32 >> 6) as u32;
        return (Some(pc.wrapping_add(8).wrapping_add(offset)), cond == 14);
    }
    let bx = insn & 0x0fff_ffd0 == 0x012f_ff10;
    (None, cond == 14 && (writes_pc(insn) || bx))
}

/// The same for Thumb (`hw1`, and `hw2` of a 32-bit instruction).
pub(super) fn scan_t(hw1: u32, hw2: u32, pc: u32) -> (Option<u32>, bool) {
    let pc4 = pc.wrapping_add(4);
    if hw1 >> 11 < 0b11101 {
        return match hw1 >> 12 {
            0xd => match (hw1 >> 8) & 15 {
                14 | 15 => (None, true),
                _ => (Some(pc4.wrapping_add(sign_extend((hw1 & 0xff) << 1, 9))), false),
            },
            0xe => (Some(pc4.wrapping_add(sign_extend((hw1 & 0x7ff) << 1, 12))), true),
            0xb if hw1 & 0x0500 == 0x0100 => {
                let offset = (hw1 >> 3) & 0x1f | (hw1 >> 4) & 0x20;
                (Some(pc4.wrapping_add(offset << 1)), false)
            }
            0xb => (None, hw1 & 0xff00 == 0xbd00),
            0x4 => (None, hw1 & 0xff00 == 0x4700 || hw1 & 0xfc87 == 0x4487),
            _ => (None, false),
        };
    }
    if hw1 >> 11 == 0b11110 && hw2 & 0x8000 != 0 {
        let s = (hw1 >> 10) & 1;
        let (j1, j2) = ((hw2 >> 13) & 1, (hw2 >> 11) & 1);
        if (hw2 >> 12) & 5 == 0 {
            let cond = (hw1 >> 6) & 15;
            if cond < 14 {
                let imm = s << 20 | j2 << 19 | j1 << 18 | (hw1 & 0x3f) << 12 | (hw2 & 0x7ff) << 1;
                return (Some(pc4.wrapping_add(sign_extend(imm, 21))), false);
            }
            return (None, false);
        }
        let i1 = !(j1 ^ s) & 1;
        let i2 = !(j2 ^ s) & 1;
        let imm = s << 24 | i1 << 23 | i2 << 22 | (hw1 & 0x3ff) << 12 | (hw2 & 0x7ff) << 1;
        let target = pc4.wrapping_add(sign_extend(imm, 25));
        return match (hw2 >> 12) & 5 {
            1 | 5 => (Some(target), true),
            _ => (None, true),
        };
    }
    // LDM/POP with the PC, LDR PC
    let ldm_pc = hw1 & 0xffd0 == 0xe890 && hw2 & 0x8000 != 0 || hw1 & 0xffd0 == 0xe910 && hw2 & 0x8000 != 0;
    let ldr_pc = hw1 & 0xfff0 == 0xf8d0 && hw2 >> 12 == 15 || hw1 & 0xfff0 == 0xf850 && hw2 >> 12 == 15;
    (None, ldm_pc || ldr_pc)
}
