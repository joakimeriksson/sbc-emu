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
                1 => 0x8000_0000 | r.below(64),
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
