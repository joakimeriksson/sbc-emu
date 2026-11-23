//! The JIT against the interpreter on the flag edge cases, exhaustively: every condition
//! under every NZCV value, carry and overflow of the add/subtract family at the boundary
//! values, and the shifter's carry-out for every shift kind at the amounts where it
//! changes (0, 1, 31, 32, 33, > 255). The x86-64 backend keeps NZCV in an image of the host
//! flags (C inverted on subtract) and the AArch64 one in the host NZCV, so these are the
//! cases where a backend's mapping would show.
use arm_a32::{CodeWatch, Cpu};
use emu_core::{Bus, Core, FlatRam};

const CODE: u32 = 0x1000;
const RAM: usize = 0x10000;
const BKPT: u32 = 0xe120_0070;

/// Run `words` (A32, or Thumb halfwords packed by `thumb`) interpreted and compiled from the
/// same registers and flags; the final registers and CPSR must match.
fn compare(words: &[u32], thumb: Option<&[u16]>, regs: &[(usize, u32)], nzcv: u32) -> Result<(), String> {
    let mk = || {
        let mut ram = FlatRam::new(0, RAM);
        match thumb {
            Some(code) => {
                for (i, h) in code.iter().enumerate() {
                    ram.write16((CODE + 2 * i as u32) as u64, *h).unwrap();
                }
            }
            None => {
                for (i, w) in words.iter().enumerate() {
                    ram.write32((CODE + 4 * i as u32) as u64, *w).unwrap();
                }
            }
        }
        let mut cpu = Cpu::new();
        cpu.r[15] = CODE;
        if thumb.is_some() {
            cpu.cpsr |= 1 << 5;
        }
        for &(r, v) in regs {
            cpu.r[r] = v;
        }
        cpu.cpsr = (cpu.cpsr & 0x0fff_ffff) | nzcv << 28;
        (cpu, ram)
    };
    let run = |cpu: &mut Cpu, ram: &mut FlatRam| -> Result<(), String> {
        for _ in 0..100 {
            match cpu.step(ram) {
                Ok(()) => {}
                Err(emu_core::Trap::Brk(0)) => return Ok(()),
                Err(t) => return Err(format!("{t:?} at {:#x}", cpu.r[15])),
            }
        }
        Err("did not reach the end".into())
    };
    let (mut a, mut ram_a) = mk();
    let (mut b, mut ram_b) = mk();
    run(&mut a, &mut ram_a)?;
    // SAFETY: ram_b outlives b
    unsafe { b.set_ram(ram_b.mem.as_mut_ptr(), 0, RAM as u32) };
    assert!(b.enable_jit(CodeWatch::new(0, RAM as u32)));
    b.jit_set_hot(1);
    run(&mut b, &mut ram_b)?;
    if b.jit_stats().unwrap().0 == 0 {
        return Err("nothing compiled".into());
    }
    if a.r != b.r || a.cpsr != b.cpsr {
        return Err(format!(
            "interpreter r={:08x?} cpsr={:08x}, compiled r={:08x?} cpsr={:08x}",
            a.r, a.cpsr, b.r, b.cpsr
        ));
    }
    Ok(())
}

const EDGES: [u32; 9] = [0, 1, 2, 0x7fff_fffe, 0x7fff_ffff, 0x8000_0000, 0x8000_0001, 0xffff_fffe, 0xffff_ffff];

#[test]
fn every_condition_under_every_flag_value() {
    if !arm_a32::jit::available() {
        return;
    }
    for nzcv in 0..16 {
        for cond in 0..15u32 {
            // mov r0, #0; mov<cond> r0, #1; add<cond>s r1, r2, r3 (flags only when taken)
            let words = [0xe3a0_0000, cond << 28 | 0x03a0_0001, cond << 28 | 0x0092_1003, BKPT];
            let regs = [(2, 0x7fff_ffff), (3, 1)];
            compare(&words, None, &regs, nzcv).unwrap_or_else(|e| panic!("A32 cond {cond} nzcv {nzcv:04b}: {e}"));
            if cond < 14 {
                // Thumb: b<cond> over a mov, then the same condition as an IT block (the MOVs
                // between them leave the flags alone)
                let code = [0xd000 | (cond as u16) << 8, 0x4608, 0x4611, 0xbf00 | (cond as u16) << 4 | 8, 0x2203, 0xbe00];
                compare(&[], Some(&code), &[(1, 5), (2, 6)], nzcv)
                    .unwrap_or_else(|e| panic!("Thumb cond {cond} nzcv {nzcv:04b}: {e}"));
            }
        }
    }
}

#[test]
fn add_and_subtract_carry_and_overflow_at_the_edges() {
    if !arm_a32::jit::available() {
        return;
    }
    // ADDS, SUBS, RSBS, ADCS, SBCS, RSCS r0, r1, r2; CMP, CMN r1, r2
    let ops = [0xe091_0002u32, 0xe051_0002, 0xe071_0002, 0xe0b1_0002, 0xe0d1_0002, 0xe0f1_0002, 0xe151_0002, 0xe171_0002];
    for &op in &ops {
        for &x in &EDGES {
            for &y in &EDGES {
                for nzcv in [0b0000, 0b0010, 0b1101, 0b1111] {
                    compare(&[op, BKPT], None, &[(1, x), (2, y)], nzcv)
                        .unwrap_or_else(|e| panic!("{op:08x} {x:#x}, {y:#x} nzcv {nzcv:04b}: {e}"));
                }
            }
        }
    }
}

#[test]
fn shifter_carry_out_at_every_boundary() {
    if !arm_a32::jit::available() {
        return;
    }
    let values = [0x8000_0001u32, 0x4000_0000, 0xffff_ffff, 1, 0x1234_5678];
    for kind in 0..4u32 {
        for &value in &values {
            for nzcv in [0b0000, 0b0010] {
                // register-specified: MOVS r0, r1, <kind> r2 and ANDS r0, r3, r1, <kind> r2
                for amount in [0u32, 1, 2, 31, 32, 33, 64, 255, 0x100, 0x101, 0xffff_ff20] {
                    for word in [0xe1b0_0211 | kind << 5, 0xe013_0211 | kind << 5] {
                        compare(&[word, BKPT], None, &[(1, value), (2, amount), (3, 0xf0f0_f0f0)], nzcv)
                            .unwrap_or_else(|e| panic!("{word:08x} {value:#x} by {amount} nzcv {nzcv:04b}: {e}"));
                    }
                }
                // immediate: MOVS r0, r1, <kind> #imm (#0 is LSL #0, LSR/ASR #32, RRX)
                for imm in 0..32u32 {
                    let word = 0xe1b0_0001 | imm << 7 | kind << 5;
                    compare(&[word, BKPT], None, &[(1, value)], nzcv)
                        .unwrap_or_else(|e| panic!("{word:08x} {value:#x} nzcv {nzcv:04b}: {e}"));
                }
            }
        }
    }
}
