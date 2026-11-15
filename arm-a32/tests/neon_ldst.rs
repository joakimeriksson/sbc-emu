//! Advanced SIMD element and structure loads/stores against a reference model written
//! element by element from the ARMv7 pseudocode: every encoding of the multiple-structure,
//! single-lane and all-lanes forms (type/size/align/index fields, loads and stores), for
//! several first registers and the three addressing modes ([Rn], [Rn]!, [Rn], Rm), plus
//! alignment faults, writeback and the CPACR/FPEXC gating Linux's lazy enable relies on.
use arm_a32::{Cpu, MODE_SVC, MODE_USR};
use emu_core::{Bus, Core, FlatRam, Trap};

const CODE: u64 = 0x1000;
const BUFFER: u32 = 0x8000;
const LEN: usize = 128;

/// One element transfer: D register, lane, element bytes, offset from the base address,
/// and for all-lanes loads the extra registers that receive the replicated value.
struct Access {
    reg: usize,
    lane: usize,
    ebytes: usize,
    offset: usize,
    replicate: bool,
}

/// Decode per the ARMv7 tables: (accesses, alignment, transfer bytes); None = UNDEFINED.
fn reference(insn: u32) -> Option<(Vec<Access>, usize, usize)> {
    let single = insn >> 23 & 1 != 0;
    let load = insn >> 21 & 1 != 0;
    let d = ((insn >> 18) & 0x10 | (insn >> 12) & 0xf) as usize;
    let bits = |hi: u32, lo: u32| ((insn >> lo) & ((1 << (hi - lo + 1)) - 1)) as usize;
    let mut accesses = Vec::new();
    if !single {
        let kind = bits(11, 8);
        let size = bits(7, 6);
        let align = bits(5, 4);
        let ebytes = 1 << size;
        let (n, regs, inc) = match kind {
            0b0111 => (1, 1, 1),
            0b1010 => (1, 2, 1),
            0b0110 => (1, 3, 1),
            0b0010 => (1, 4, 1),
            0b1000 => (2, 1, 1),
            0b1001 => (2, 1, 2),
            0b0011 => (2, 2, 2),
            0b0100 => (3, 1, 1),
            0b0101 => (3, 1, 2),
            0b0000 => (4, 1, 1),
            0b0001 => (4, 1, 2),
            _ => return None,
        };
        let undefined = match (n, regs) {
            (1, 1) | (1, 3) => align & 2 != 0,
            (1, 2) => align == 3,
            (1, _) => false,
            (2, 1) => size == 3 || align == 3,
            (2, _) | (4, _) => size == 3,
            _ => size == 3 || align & 2 != 0,
        };
        if undefined {
            return None;
        }
        let mut offset = 0;
        for r in 0..regs {
            for e in 0..8 / ebytes {
                for s in 0..n {
                    let reg = if n == 1 { d + r } else { d + r + s * inc };
                    if reg > 31 {
                        return None;
                    }
                    accesses.push(Access { reg, lane: e, ebytes, offset, replicate: false });
                    offset += ebytes;
                }
            }
        }
        let alignment = if align == 0 { 1 } else { 4 << align };
        return Some((accesses, alignment, offset));
    }
    let n = bits(9, 8) + 1;
    if bits(11, 10) == 3 {
        if !load {
            return None;
        }
        let size = bits(7, 6);
        let (t, a) = (bits(5, 5), bits(4, 4));
        let (ebytes, alignment, regs, inc) = match n {
            1 if size == 3 || size == 0 && a == 1 => return None,
            1 => (1 << size, if a == 1 { 1 << size } else { 1 }, t + 1, 1),
            2 if size == 3 => return None,
            2 => (1 << size, if a == 1 { 2 << size } else { 1 }, 1, t + 1),
            3 if size == 3 || a == 1 => return None,
            3 => (1 << size, 1, 1, t + 1),
            _ if size == 3 && a == 0 => return None,
            _ => {
                let ebytes = if size == 3 { 4 } else { 1 << size };
                let alignment = match (a, size) {
                    (0, _) => 1,
                    (_, 3) => 16,
                    (_, 2) => 8,
                    _ => 4 * ebytes,
                };
                (ebytes, alignment, 1, t + 1)
            }
        };
        for s in 0..n {
            for r in 0..regs {
                let reg = d + s * inc + r;
                if reg > 31 {
                    return None;
                }
                accesses.push(Access { reg, lane: 0, ebytes, offset: s * ebytes, replicate: true });
            }
        }
        return Some((accesses, alignment, n * ebytes));
    }
    let size = bits(11, 10);
    let ebytes = 1 << size;
    let ia = bits(7, 4);
    let index = ia >> (size + 1);
    // (inc, alignment) per the single-lane tables
    let (inc, alignment) = match (n, size) {
        (1, 0) if ia & 1 != 0 => return None,
        (1, 0) => (1, 1),
        (1, 1) if ia & 2 != 0 => return None,
        (1, 1) => (1, if ia & 1 != 0 { 2 } else { 1 }),
        (1, _) if ia & 4 != 0 || ia & 3 == 1 || ia & 3 == 2 => return None,
        (1, _) => (1, if ia & 3 == 3 { 4 } else { 1 }),
        (2, 0) => (1, if ia & 1 != 0 { 2 } else { 1 }),
        (2, 1) => (if ia & 2 != 0 { 2 } else { 1 }, if ia & 1 != 0 { 4 } else { 1 }),
        (2, _) if ia & 2 != 0 => return None,
        (2, _) => (if ia & 4 != 0 { 2 } else { 1 }, if ia & 1 != 0 { 8 } else { 1 }),
        (3, 0) if ia & 1 != 0 => return None,
        (3, 0) => (1, 1),
        (3, 1) if ia & 1 != 0 => return None,
        (3, 1) => (if ia & 2 != 0 { 2 } else { 1 }, 1),
        (3, _) if ia & 3 != 0 => return None,
        (3, _) => (if ia & 4 != 0 { 2 } else { 1 }, 1),
        (_, 0) => (1, if ia & 1 != 0 { 4 } else { 1 }),
        (_, 1) => (if ia & 2 != 0 { 2 } else { 1 }, if ia & 1 != 0 { 8 } else { 1 }),
        (_, _) if ia & 3 == 3 => return None,
        (_, _) => (if ia & 4 != 0 { 2 } else { 1 }, if ia & 3 == 0 { 1 } else { 4 << (ia & 3) }),
    };
    for s in 0..n {
        let reg = d + s * inc;
        if reg > 31 {
            return None;
        }
        accesses.push(Access { reg, lane: index, ebytes, offset: s * ebytes, replicate: false });
    }
    Some((accesses, alignment, n * ebytes))
}

fn apply(insn: u32, accesses: &[Access], d: &mut [u64; 32], memory: &mut [u8], at: usize) {
    let load = insn >> 21 & 1 != 0;
    for a in accesses {
        let bits = 8 * a.ebytes;
        let mask = if bits == 64 { u64::MAX } else { (1u64 << bits) - 1 };
        let shift = a.lane * bits;
        if load {
            let mut value = 0u64;
            for i in 0..a.ebytes {
                value |= (memory[at + a.offset + i] as u64) << (8 * i);
            }
            if a.replicate {
                d[a.reg] = (0..64 / bits).fold(0, |w, i| w | value << (i * bits));
            } else {
                d[a.reg] = d[a.reg] & !(mask << shift) | value << shift;
            }
        } else {
            let value = d[a.reg] >> shift & mask;
            for i in 0..a.ebytes {
                memory[at + a.offset + i] = (value >> (8 * i)) as u8;
            }
        }
    }
}

struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }
}

fn neon_cpu() -> Cpu {
    let mut cpu = Cpu::new();
    cpu.cpsr = MODE_SVC | 0xc0;
    cpu.cpacr = 0xf << 20;
    cpu.vfp.fpexc = 1 << 30;
    cpu
}

fn run(cpu: &mut Cpu, ram: &mut FlatRam, word: u32, thumb: bool) -> Result<(), Trap> {
    if thumb {
        let t32 = if word >> 24 == 0xf4 { 0xf900_0000 | word & 0x00ff_ffff } else { word };
        ram.write16(CODE, (t32 >> 16) as u16).unwrap();
        ram.write16(CODE + 2, t32 as u16).unwrap();
        cpu.cpsr |= 1 << 5;
    } else {
        ram.write32(CODE, word).unwrap();
    }
    cpu.r[15] = CODE as u32;
    cpu.step(ram)
}

#[test]
fn every_load_store_form_matches_the_reference() {
    let mut ram = FlatRam::new(0, 0x10000);
    let mut rng = Rng(0x1234_5678_9abc_def1);
    let (mut checked, mut undefined) = (0, 0);
    let mut failures = Vec::new();
    for single in 0..2u32 {
        for load in 0..2u32 {
            for low in 0..256u32 {
                for first in [0usize, 1, 5, 16, 28, 31] {
                    for rm in [15u32, 13, 2] {
                        let insn = 0xf400_0000
                            | single << 23
                            | (first as u32 >> 4) << 22
                            | load << 21
                            | (first as u32 & 15) << 12
                            | low << 4
                            | rm;
                        let reference = reference(insn);
                        for thumb in [false, true] {
                            let mut cpu = neon_cpu();
                            for w in cpu.vfp.d.iter_mut() {
                                *w = rng.next();
                            }
                            let initial = cpu.vfp.d;
                            let base = BUFFER + 32;
                            cpu.r[0] = base;
                            cpu.r[2] = (rng.next() % 64) as u32;
                            let memory: Vec<u8> = (0..LEN).map(|_| rng.next() as u8).collect();
                            ram.load(BUFFER as u64, &memory).unwrap();
                            let result = run(&mut cpu, &mut ram, insn, thumb);
                            let Some((accesses, _alignment, transfer)) = &reference else {
                                if !matches!(result, Err(Trap::Unimplemented(..))) {
                                    failures.push(format!("{insn:08x}: executed, reference says UNDEFINED"));
                                }
                                undefined += 1;
                                continue;
                            };
                            if result.is_err() {
                                failures.push(format!("{insn:08x} (thumb {thumb}): {result:?}"));
                                continue;
                            }
                            let mut want_d = initial;
                            let mut want_memory = memory.clone();
                            apply(insn, accesses, &mut want_d, &mut want_memory, 32);
                            let want_r0 = match rm {
                                15 => base,
                                13 => base + *transfer as u32,
                                _ => base.wrapping_add(cpu.r[2]),
                            };
                            let got_memory: Vec<u8> = (0..LEN).map(|i| ram.read8(BUFFER as u64 + i as u64).unwrap()).collect();
                            if cpu.vfp.d != want_d || got_memory != want_memory || cpu.r[0] != want_r0 {
                                failures.push(format!("{insn:08x} (thumb {thumb}): differs from the reference"));
                            }
                            checked += 1;
                        }
                    }
                }
            }
        }
    }
    eprintln!("{checked} load/store executions checked, {undefined} UNDEFINED encodings");
    assert!(failures.is_empty(), "{} failures:\n{}", failures.len(), failures[..failures.len().min(40)].join("\n"));
}

#[test]
fn misaligned_addresses_raise_alignment_faults() {
    let mut ram = FlatRam::new(0, 0x10000);
    let mut checked = 0;
    for single in 0..2u32 {
        for load in 0..2u32 {
            for low in 0..256u32 {
                let insn = 0xf400_0000 | single << 23 | load << 21 | low << 4 | 0xd;
                let Some((_, alignment, _)) = reference(insn) else { continue };
                for offset in [1u32, 2, 4, 8, 16] {
                    let mut cpu = neon_cpu();
                    cpu.vbar = 0;
                    cpu.r[0] = BUFFER + offset;
                    let before = cpu.vfp.d;
                    let result = run(&mut cpu, &mut ram, insn, false);
                    if (offset as usize) % alignment == 0 {
                        assert!(result.is_ok(), "{insn:08x} at +{offset}: {result:?}");
                        continue;
                    }
                    checked += 1;
                    assert!(matches!(result, Err(Trap::Exception(..))), "{insn:08x} at +{offset}: {result:?}");
                    assert_eq!(cpu.dfsr & 0xc0f, 0x001 | (1 - load) << 11, "{insn:08x}: DFSR {:#x}", cpu.dfsr);
                    assert_eq!(cpu.dfar, BUFFER + offset);
                    assert_eq!(cpu.cpsr & 0x1f, 0x17, "abort mode");
                    assert_eq!(cpu.vfp.d, before, "{insn:08x}: registers written before the fault");
                    assert_eq!(cpu.r[0], BUFFER + offset, "{insn:08x}: base written back");
                }
            }
        }
    }
    assert!(checked > 100);
}

#[test]
fn neon_obeys_cpacr_and_fpexc() {
    let mut ram = FlatRam::new(0, 0x10000);
    let words = [
        0xf202_0844u32, // vadd.i8 q0, q1, q2
        0xf420_070f,    // vld1.8 {d0}, [r0]
        0xee40_1b10,    // vmov.8 d0[2], r1 (Advanced SIMD scalar transfer)
        0xeea0_1b10,    // vdup.32 q0, r1
    ];
    for word in words {
        for (cpacr, fpexc, user, ok) in [
            (0xf << 20, 1 << 30, false, true),
            (0xf << 20, 0, false, false),            // FPEXC.EN clear: lazy enable traps
            (0, 1 << 30, false, false),               // no CP10/CP11 access
            (0x5 << 20, 1 << 30, false, true),        // privileged-only access
            (0x5 << 20, 1 << 30, true, false),
            (0xf << 20 | 1 << 31, 1 << 30, false, false), // CPACR.ASEDIS
        ] {
            let mut cpu = neon_cpu();
            cpu.cpacr = cpacr;
            cpu.vfp.fpexc = fpexc;
            if user {
                cpu.cpsr = MODE_USR;
            }
            cpu.r[0] = BUFFER;
            let result = run(&mut cpu, &mut ram, word, false);
            assert_eq!(result.is_ok(), ok, "{word:08x} cpacr {cpacr:#x} fpexc {fpexc:#x} user {user}: {result:?}");
            if !ok {
                assert!(matches!(result, Err(Trap::Unimplemented(..))));
            }
        }
    }
}

#[test]
fn undefined_neon_reaches_the_guest_handler_in_thumb() {
    // With FPEXC.EN clear a T32 NEON instruction must take the undefined-instruction
    // exception with LR = instruction + 2, the Thumb convention Linux expects.
    let mut ram = FlatRam::new(0, 0x10000);
    let mut cpu = neon_cpu();
    cpu.vfp.fpexc = 0;
    cpu.undef_to_guest = true;
    let result = run(&mut cpu, &mut ram, 0xef02_0844, true);
    assert!(matches!(result, Err(Trap::Exception(..))), "{result:?}");
    assert_eq!(cpu.cpsr & 0x1f, 0x1b, "undefined mode");
    assert_eq!(cpu.r[14], CODE as u32 + 2);
}
