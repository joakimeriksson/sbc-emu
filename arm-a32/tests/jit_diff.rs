//! The JIT against the interpreter: random straight-line A32 code (data processing with
//! every operand form and condition, multiplies, divides, bitfields, extends, loads and
//! stores of every size with writeback, LDM/STM) runs once interpreted and once compiled
//! from the same state; registers, flags, memory and the instruction count must match.
use arm_a32::{CodeWatch, Cpu};
use emu_core::{Bus, Core, FlatRam};

const CODE: u32 = 0x1000;
const DATA: u32 = 0x8000;
const RAM: usize = 0x20000;

struct Rng(u64);
impl Rng {
    fn next(&mut self) -> u32 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        (self.0 >> 16) as u32
    }
    fn below(&mut self, n: u32) -> u32 {
        self.next() % n
    }
}

/// r0-r8 and lr are data, r9 a small offset, r10-r12 and sp point into the data area.
fn data_reg(r: &mut Rng) -> u32 {
    match r.below(10) {
        9 => 14,
        n => n,
    }
}

fn cond(r: &mut Rng) -> u32 {
    if r.below(3) == 0 {
        r.below(14)
    } else {
        14
    }
}

fn gen(r: &mut Rng) -> u32 {
    let c = cond(r) << 28;
    match r.below(16) {
        0..=5 => {
            // data processing
            let opcode = r.below(16);
            let s = if (8..=11).contains(&opcode) { 1 } else { r.below(2) };
            let (rd, rn) = (data_reg(r), if r.below(8) == 0 { 15 } else { data_reg(r) });
            let op2 = match r.below(3) {
                0 => 1 << 25 | r.below(16) << 8 | r.below(256),
                1 => {
                    let rm = if r.below(10) == 0 { 15 } else { data_reg(r) };
                    r.below(32) << 7 | r.below(4) << 5 | rm
                }
                // register-specified shift (amounts 0-255 from the data registers)
                _ => data_reg(r) << 8 | r.below(4) << 5 | 0x10 | data_reg(r),
            };
            c | opcode << 21 | s << 20 | rn << 16 | rd << 12 | op2
        }
        6 => {
            let (rd, ra, rs, rm) = (data_reg(r), data_reg(r), data_reg(r), data_reg(r));
            match r.below(3) {
                0 => c | r.below(4) << 20 | rd << 16 | ra << 12 | rs << 8 | 0x90 | rm, // MUL/MLA{S}
                _ => {
                    let lo = data_reg(r);
                    let hi = [0, 1, 2, 3, 4, 5, 6, 7, 8, 14].into_iter().filter(|&x| x != lo).nth(r.below(9) as usize).unwrap();
                    c | 0x0080_0090 | r.below(4) << 21 | hi << 16 | lo << 12 | rs << 8 | rm
                }
            }
        }
        7 => {
            let (rd, rm) = (data_reg(r), data_reg(r));
            match r.below(8) {
                0 => c | 0x0300_0000 | r.below(16) << 16 | rd << 12 | r.below(4096), // MOVW
                1 => c | 0x0340_0000 | r.below(16) << 16 | rd << 12 | r.below(4096), // MOVT
                2 => c | 0x016f_0f10 | rd << 12 | rm,                                  // CLZ
                3 => c | if r.below(2) == 0 { 0x06bf_0f30 } else { 0x06bf_0fb0 } | rd << 12 | rm,
                4 => {
                    let base = [0x06e0_0070, 0x06f0_0070, 0x06a0_0070, 0x06b0_0070][r.below(4) as usize];
                    let rn = if r.below(2) == 0 { 15 } else { data_reg(r) };
                    c | base | rn << 16 | rd << 12 | r.below(4) << 10 | rm
                }
                5 => {
                    let lsb = r.below(32);
                    let msb = lsb + r.below(32 - lsb);
                    let rn = if r.below(4) == 0 { 15 } else { rm };
                    c | 0x07c0_0010 | msb << 16 | rd << 12 | lsb << 7 | rn
                }
                6 => {
                    let lsb = r.below(32);
                    let width = 1 + r.below(32 - lsb);
                    c | if r.below(2) == 0 { 0x07e0_0050 } else { 0x07a0_0050 } | (width - 1) << 16 | rd << 12 | lsb << 7 | rm
                }
                _ => c | if r.below(2) == 0 { 0x0710_f010 } else { 0x0730_f010 } | rd << 16 | data_reg(r) << 8 | rm,
            }
        }
        8..=11 => {
            // LDR/STR/LDRB/STRB: base r10-r12 or sp, small offsets
            let rn = [10, 11, 12, 13][r.below(4) as usize];
            let rd = data_reg(r);
            let (p, w) = match r.below(3) {
                0 => (1, 0),
                1 => (1, 1),
                _ => (0, 0),
            };
            let offset = if r.below(2) == 0 {
                r.below(64)
            } else {
                1 << 25 | r.below(3) << 7 | r.below(3) << 5 | 9
            };
            c | 1 << 26 | p << 24 | r.below(2) << 23 | r.below(2) << 22 | w << 21 | r.below(2) << 20 | rn << 16 | rd << 12 | offset
        }
        12 | 13 => {
            // halfword, signed and doubleword transfers
            let rn = [10, 11, 12, 13][r.below(4) as usize];
            let (p, w) = match r.below(3) {
                0 => (1, 0),
                1 => (1, 1),
                _ => (0, 0),
            };
            let (l, sh) = match r.below(6) {
                0 => (1, 1),
                1 => (1, 2),
                2 => (1, 3),
                3 => (0, 1),
                4 => (0, 2),
                _ => (0, 3),
            };
            let rd = if l == 0 && sh != 1 { 2 * r.below(4) } else { data_reg(r) };
            let off = if r.below(2) == 0 {
                1 << 22 | (r.below(16) << 8) & 0xf00 | r.below(16)
            } else {
                9
            };
            c | p << 24 | r.below(2) << 23 | w << 21 | l << 20 | rn << 16 | rd << 12 | sh << 5 | 0x90 | off
        }
        _ => {
            // LDM/STM on sp or r12, writeback, never the base or the pointers
            let rn = if r.below(2) == 0 { 13 } else { 12 };
            let mut list = r.next() & 0x41ff;
            if list == 0 {
                list = 1;
            }
            c | 0x0800_0000 | r.below(4) << 23 | r.below(2) << 21 | r.below(2) << 20 | rn << 16 | list
        }
    }
}

fn setup(r: &mut Rng, words: &[u32]) -> (Cpu, FlatRam) {
    let mut ram = FlatRam::new(0, RAM);
    for (n, w) in words.iter().enumerate() {
        ram.write32((CODE + 4 * n as u32) as u64, *w).unwrap();
    }
    for a in (DATA..DATA + 0x4000).step_by(4) {
        ram.write32(a as u64, r.next()).unwrap();
    }
    let mut cpu = Cpu::new();
    cpu.r[15] = CODE;
    for i in 0..15 {
        cpu.r[i] = match i {
            9 => r.below(64),
            10 | 11 | 12 | 13 => DATA + 0x1000 + 0x400 * (i as u32 - 10) + 4 * r.below(16),
            _ => match r.below(5) {
                0 => r.below(64),
                1 => r.below(300),
                2 => 0x8000_0000 | r.below(64),
                _ => r.next(),
            },
        };
    }
    cpu.cpsr = (cpu.cpsr & 0x0fff_ffff) | r.below(16) << 28;
    (cpu, ram)
}

fn run(cpu: &mut Cpu, ram: &mut FlatRam) {
    for _ in 0..1000 {
        match cpu.step(ram) {
            Ok(()) => {}
            Err(emu_core::Trap::Brk(0)) => return,
            Err(t) => panic!("{t:?} at {:#x}", cpu.r[15]),
        }
    }
    panic!("did not reach the end");
}

#[test]
fn random_blocks_match_the_interpreter() {
    if !arm_a32::jit::available() {
        return;
    }
    let mut rng = Rng(0x9e37_79b9_7f4a_7c15);
    let cases = std::env::var("JIT_DIFF_CASES").ok().and_then(|v| v.parse().ok()).unwrap_or(3000);
    for case in 0..cases {
        let len = 1 + rng.below(40) as usize;
        let mut words: Vec<u32> = (0..len).map(|_| gen(&mut rng)).collect();
        words.push(0xe120_0070); // bkpt #0
        let seed = rng.next() as u64 | 1;
        let (mut a, mut ram_a) = setup(&mut Rng(seed), &words);
        let (mut b, mut ram_b) = setup(&mut Rng(seed), &words);
        run(&mut a, &mut ram_a);
        // SAFETY: ram_b outlives b
        unsafe { b.set_ram(ram_b.mem.as_mut_ptr(), 0, RAM as u32) };
        assert!(b.enable_jit(CodeWatch::new(0, RAM as u32)));
        b.jit_set_hot(1);
        run(&mut b, &mut ram_b);
        let (compiled, _, _) = b.jit_stats().unwrap();
        assert!(compiled >= 1, "case {case}: nothing compiled");
        let dis: Vec<String> = words.iter().map(|w| format!("{w:08x}")).collect();
        assert_eq!(a.r, b.r, "case {case} registers; code {}", dis.join(" "));
        assert_eq!(a.cpsr, b.cpsr, "case {case} cpsr; code {}", dis.join(" "));
        assert_eq!(a.instret(), b.instret(), "case {case} instret; code {}", dis.join(" "));
        assert!(ram_a.mem == ram_b.mem, "case {case} memory; code {}", dis.join(" "));
    }
}

/// Thumb: r0-r4 and r8-r12, lr are data, r5 a small offset, r6/r7 and sp point into the
/// data area.
fn t_low(r: &mut Rng) -> u32 {
    r.below(5)
}
fn t_any(r: &mut Rng) -> u32 {
    [0, 1, 2, 3, 4, 8, 9, 10, 11, 12, 14][r.below(11) as usize]
}
fn t_ptr(r: &mut Rng) -> u32 {
    6 + r.below(2)
}

/// One random Thumb instruction as halfwords.
fn gen_thumb(r: &mut Rng) -> Vec<u16> {
    let h = |v: u32| vec![v as u16];
    let w = |v: u32| vec![(v >> 16) as u16, v as u16];
    match r.below(26) {
        0 => h(r.below(3) << 11 | r.below(32) << 6 | t_low(r) << 3 | t_low(r)),
        1 => h(0x1800 | r.below(4) << 9 | t_low(r) << 6 | t_low(r) << 3 | t_low(r)),
        2 => h((4 + r.below(4)) << 11 | t_low(r) << 8 | r.below(256)),
        3 => {
            let op = r.below(16);
            let rm = if matches!(op, 2 | 3 | 4 | 7) { t_low(r) } else { t_low(r) };
            h(0x4000 | op << 6 | rm << 3 | t_low(r))
        }
        4 => {
            // ADD/CMP/MOV high registers
            let op = r.below(3);
            let rd = t_any(r);
            let rm = if op == 0 && r.below(6) == 0 { 15 } else { t_any(r) };
            h(0x4400 | op << 8 | (rd & 8) << 4 | rm << 3 | rd & 7)
        }
        5 => h(0x4800 | t_low(r) << 8 | r.below(256)), // LDR literal
        6 => h(0x5000 | r.below(8) << 9 | 5 << 6 | t_ptr(r) << 3 | t_low(r)),
        7 => h((0x6000 + r.below(5) * 0x800) | r.below(16) << 6 | t_ptr(r) << 3 | t_low(r)),
        8 => h(0x8000 | r.below(2) << 11 | r.below(16) << 6 | t_ptr(r) << 3 | t_low(r)),
        9 => h(0x9000 | r.below(2) << 11 | t_low(r) << 8 | r.below(64)),
        10 => h(0xa000 | r.below(2) << 11 | t_low(r) << 8 | r.below(256)),
        11 => {
            let op = r.below(3);
            match op {
                0 => h(0xb200 | r.below(4) << 6 | t_low(r) << 3 | t_low(r)),
                1 => h(0xba00 | [0, 1, 3][r.below(3) as usize] << 6 | t_low(r) << 3 | t_low(r)),
                _ => h(0xbf00), // NOP
            }
        }
        12 => {
            // PUSH then POP of the same list keeps sp in range
            let list = 1 + r.below(31);
            let lr = r.below(2);
            let mut v = h(0xb400 | lr << 8 | list);
            v.extend(h(0xbc00 | list));
            v
        }
        13 => {
            let rn = t_ptr(r);
            h(0xc000 | r.below(2) << 11 | rn << 8 | (1 + r.below(31)))
        }
        14 | 15 => {
            // data processing, modified immediate
            let op = [0, 1, 2, 3, 4, 8, 10, 11, 13, 14][r.below(10) as usize];
            let s = r.below(2);
            let rn = if matches!(op, 2 | 3) && r.below(3) == 0 { 15 } else { t_any(r) };
            let rd = if s == 1 && matches!(op, 0 | 4 | 8 | 13) && r.below(4) == 0 { 15 } else { t_any(r) };
            let imm12 = r.below(4096);
            w(0xf000_0000 | (imm12 >> 11) << 26 | op << 21 | s << 20 | rn << 16 | ((imm12 >> 8) & 7) << 12 | rd << 8 | imm12 & 0xff)
        }
        16 | 17 => {
            // data processing, shifted register
            let op = [0, 1, 2, 3, 4, 8, 10, 11, 13, 14][r.below(10) as usize];
            let s = r.below(2);
            let rn = if matches!(op, 2 | 3) && r.below(3) == 0 { 15 } else { t_any(r) };
            let rd = if s == 1 && matches!(op, 0 | 4 | 8 | 13) && r.below(4) == 0 { 15 } else { t_any(r) };
            let imm5 = r.below(32);
            w(0xea00_0000 | op << 21 | s << 20 | rn << 16 | (imm5 >> 2) << 12 | rd << 8 | (imm5 & 3) << 6 | r.below(4) << 4 | t_any(r))
        }
        18 => {
            let (rd, rn) = (t_any(r), if r.below(4) == 0 { 15 } else { t_any(r) });
            let imm12 = r.below(4096);
            let op = [0x00, 0x0a, 0x04, 0x0c][r.below(4) as usize];
            let hi = if matches!(op, 0x04 | 0x0c) { r.below(16) } else { rn };
            w(0xf200_0000 | (imm12 >> 11) << 26 | op << 20 | hi << 16 | ((imm12 >> 8) & 7) << 12 | rd << 8 | imm12 & 0xff)
        }
        19 => {
            let (rd, rn) = (t_any(r), t_any(r));
            let lsb = r.below(32);
            let width = 1 + r.below(32 - lsb);
            match r.below(3) {
                0 => w(0xf3c0_0000 | rn << 16 | (lsb >> 2) << 12 | rd << 8 | (lsb & 3) << 6 | (width - 1)),
                1 => w(0xf340_0000 | rn << 16 | (lsb >> 2) << 12 | rd << 8 | (lsb & 3) << 6 | (width - 1)),
                _ => w(0xf360_0000 | if r.below(4) == 0 { 15 } else { rn } << 16 | (lsb >> 2) << 12 | rd << 8 | (lsb & 3) << 6 | (lsb + width - 1)),
            }
        }
        20 | 21 => {
            // LDR/STR (T3 imm12, T4 imm8 indexed, register, literal)
            let size = r.below(3);
            let load = r.below(2);
            let signed = if load == 1 && size < 2 { r.below(2) } else { 0 };
            let rn = t_ptr(r);
            let rt = t_any(r);
            let base = 0xf800_0000 | signed << 24 | size << 21 | load << 20;
            match r.below(4) {
                0 => w(base | 1 << 23 | rn << 16 | rt << 12 | r.below(256)),
                1 => {
                    let (p, u, wb) = match r.below(3) {
                        0 => (1, r.below(2), 0),
                        1 => (1, r.below(2), 1),
                        _ => (0, r.below(2), 1),
                    };
                    w(base | rn << 16 | rt << 12 | 0x800 | p << 10 | u << 9 | wb << 8 | r.below(256))
                }
                2 => w(base | rn << 16 | rt << 12 | r.below(4) << 4 | 5),
                _ => w(0xf800_0000 | signed << 24 | size << 21 | 1 << 20 | r.below(2) << 23 | 15 << 16 | rt << 12 | r.below(256)),
            }
        }
        22 => {
            // LDRD/STRD (immediate), any pair
            let (p, wb) = match r.below(3) {
                0 => (1, 0),
                1 => (1, 1),
                _ => (0, 1),
            };
            let (rt, rt2) = (t_any(r), t_any(r));
            w(0xe840_0000 | p << 24 | r.below(2) << 23 | wb << 21 | r.below(2) << 20 | t_ptr(r) << 16 | rt << 12 | rt2 << 8 | r.below(64))
        }
        23 => {
            // shifts by register, extends with rotation, REV, CLZ
            let (rd, rn, rm) = (t_any(r), t_any(r), t_any(r));
            match r.below(4) {
                0 => w(0xfa00_f000 | r.below(4) << 21 | r.below(2) << 20 | rn << 16 | rd << 8 | rm),
                1 => w(0xfa00_f080 | [0, 1, 4, 5][r.below(4) as usize] << 20 | 15 << 16 | rd << 8 | r.below(4) << 4 | rm),
                2 => w(0xfa90_f080 | rm << 16 | rd << 8 | r.below(2) << 4 | rm),
                _ => w(0xfab0_f080 | rm << 16 | rd << 8 | rm),
            }
        }
        24 => {
            let (rd, rn, rm, ra) = (t_any(r), t_any(r), t_any(r), t_any(r));
            match r.below(5) {
                0 => w(0xfb00_f000 | rn << 16 | rd << 8 | rm),
                1 => w(0xfb00_0000 | rn << 16 | ra << 12 | rd << 8 | rm),
                2 => {
                    let hi = [0, 1, 2, 3, 4, 8, 9, 10, 11, 12, 14].into_iter().filter(|&x| x != ra).nth(r.below(10) as usize).unwrap();
                    w(0xfb80_0000 | [0, 2, 4, 6][r.below(4) as usize] << 20 | rn << 16 | ra << 12 | hi << 8 | rm)
                }
                _ => w(0xfb90_f0f0 | r.below(2) << 21 | rn << 16 | rd << 8 | rm),
            }
        }
        _ => {
            // LDM/STM.W on r6/r7 with writeback, data registers only
            let rn = t_ptr(r);
            let list = (r.next() & 0x5f1f).max(1);
            let mode = if r.below(2) == 0 { 0xe880_0000 } else { 0xe900_0000 };
            w(mode | 1 << 21 | r.below(2) << 20 | rn << 16 | list)
        }
    }
}

fn setup_thumb(r: &mut Rng, code: &[u16]) -> (Cpu, FlatRam) {
    let mut ram = FlatRam::new(0, RAM);
    for (n, h) in code.iter().enumerate() {
        ram.write16((CODE + 2 * n as u32) as u64, *h).unwrap();
    }
    for a in (DATA..DATA + 0x4000).step_by(4) {
        ram.write32(a as u64, r.next()).unwrap();
    }
    let mut cpu = Cpu::new();
    cpu.r[15] = CODE;
    cpu.cpsr |= 1 << 5;
    for i in 0..15 {
        cpu.r[i] = match i {
            5 => r.below(64) & !3,
            6 | 7 => DATA + 0x1000 + 0x800 * (i as u32 - 6) + 4 * r.below(16),
            13 => DATA + 0x3800,
            _ => match r.below(5) {
                0 => r.below(64),
                1 => r.below(300),
                2 => 0x8000_0000 | r.below(64),
                _ => r.next(),
            },
        };
    }
    cpu.cpsr = (cpu.cpsr & 0x0fff_ffff) | r.below(16) << 28;
    (cpu, ram)
}

#[test]
fn random_thumb_blocks_match_the_interpreter() {
    if !arm_a32::jit::available() {
        return;
    }
    let mut rng = Rng(0x2545_f491_4f6c_dd1d);
    let cases = std::env::var("JIT_DIFF_CASES").ok().and_then(|v| v.parse().ok()).unwrap_or(3000);
    for case in 0..cases {
        let len = 1 + rng.below(30) as usize;
        let mut code: Vec<u16> = Vec::new();
        for _ in 0..len {
            code.extend(gen_thumb(&mut rng));
        }
        code.push(0xbe00); // bkpt #0
        let seed = rng.next() as u64 | 1;
        let (mut a, mut ram_a) = setup_thumb(&mut Rng(seed), &code);
        let (mut b, mut ram_b) = setup_thumb(&mut Rng(seed), &code);
        if std::env::var_os("JIT_DIFF_TRACE").is_some() {
            let dis: Vec<String> = code.iter().map(|h| format!("{h:04x}")).collect();
            eprintln!("case {case}: {}", dis.join(" "));
        }
        run(&mut a, &mut ram_a);
        // SAFETY: ram_b outlives b
        unsafe { b.set_ram(ram_b.mem.as_mut_ptr(), 0, RAM as u32) };
        assert!(b.enable_jit(CodeWatch::new(0, RAM as u32)));
        b.jit_set_hot(1);
        run(&mut b, &mut ram_b);
        let dis: Vec<String> = code.iter().map(|h| format!("{h:04x}")).collect();
        assert!(b.jit_stats().unwrap().0 >= 1, "case {case}: nothing compiled");
        assert_eq!(a.r, b.r, "case {case} registers; code {}", dis.join(" "));
        assert_eq!(a.cpsr, b.cpsr, "case {case} cpsr; code {}", dis.join(" "));
        assert_eq!(a.instret(), b.instret(), "case {case} instret; code {}", dis.join(" "));
        assert!(ram_a.mem == ram_b.mem, "case {case} memory; code {}", dis.join(" "));
    }
}

/// Debugging aid: `JIT_REPRO="f83f 1033 ..." cargo test --test jit_diff thumb_repro -- --ignored`
#[test]
#[ignore]
fn thumb_repro() {
    let code: Vec<u16> = std::env::var("JIT_REPRO").unwrap().split_whitespace().map(|h| u16::from_str_radix(h, 16).unwrap()).collect();
    let seed = std::env::var("JIT_SEED").ok().and_then(|v| v.parse().ok()).unwrap_or(1);
    let (mut a, mut ram_a) = setup_thumb(&mut Rng(seed), &code);
    let (mut b, mut ram_b) = setup_thumb(&mut Rng(seed), &code);
    run(&mut a, &mut ram_a);
    unsafe { b.set_ram(ram_b.mem.as_mut_ptr(), 0, RAM as u32) };
    assert!(b.enable_jit(CodeWatch::new(0, RAM as u32)));
    b.jit_set_hot(1);
    run(&mut b, &mut ram_b);
    assert_eq!(a.r, b.r);
    assert_eq!(a.cpsr, b.cpsr);
}

/// Interworking in compiled code: Thumb calls ARM code with BLX (register) and returns with
/// BX LR; ARM calls Thumb with BLX (immediate); POP {pc} and LDR pc switch state.
#[test]
fn interworking_calls() {
    if !arm_a32::jit::available() {
        return;
    }
    let mut ram = FlatRam::new(0, RAM);
    // Thumb at 0x1000: r4 counts down; each round calls the ARM routine at 0x2000 (blx r3),
    // which calls the Thumb routine at 0x3000 (blx #imm) and returns with bx lr
    let thumb: [u16; 9] = [
        0x2405, // movs r4, #5
        0x4798, // loop: blx r3
        0x3c01, // subs r4, #1
        0xd1fc, // bne loop
        0xbe00, // bkpt
        0, 0, 0, 0,
    ];
    for (i, h) in thumb.iter().enumerate() {
        ram.write16(0x1000 + 2 * i as u64, *h).unwrap();
    }
    let arm: [u32; 5] = [
        0xe92d_4010, // push {r4, lr}
        0xe280_0001, // add r0, r0, #1
        0xfa00_03fc, // blx 0x3000 (Thumb)
        0xe8bd_4010, // pop {r4, lr}
        0xe12f_ff1e, // bx lr
    ];
    for (i, w) in arm.iter().enumerate() {
        ram.write32(0x2000 + 4 * i as u64, *w).unwrap();
    }
    let callee: [u16; 4] = [
        0xb500, // push {lr}
        0x3102, // adds r1, #2
        0xbd00, // pop {pc}
        0,
    ];
    for (i, h) in callee.iter().enumerate() {
        ram.write16(0x3000 + 2 * i as u64, *h).unwrap();
    }
    let mut ram_b = FlatRam::new(0, RAM);
    ram_b.mem.copy_from_slice(&ram.mem);
    let init = |cpu: &mut Cpu| {
        cpu.r[15] = 0x1000;
        cpu.cpsr |= 1 << 5;
        cpu.r[3] = 0x2000;
        cpu.r[13] = 0x9000;
    };
    let (mut a, mut b) = (Cpu::new(), Cpu::new());
    init(&mut a);
    init(&mut b);
    run(&mut a, &mut ram);
    unsafe { b.set_ram(ram_b.mem.as_mut_ptr(), 0, RAM as u32) };
    assert!(b.enable_jit(CodeWatch::new(0, RAM as u32)));
    b.jit_set_hot(1);
    run(&mut b, &mut ram_b);
    assert_eq!((a.r[0], a.r[1]), (5, 10));
    assert_eq!(a.r, b.r);
    assert_eq!(a.cpsr, b.cpsr);
    assert_eq!(a.instret(), b.instret());
}
