//! Random A32 and Thumb code for comparing the JIT with the interpreter (`jit_diff.rs`, and
//! the browser build's `sbc_a32_jit_diff`, which runs the wasm backend): straight-line code
//! of data processing with every operand form and condition, multiplies, divides, bitfields,
//! extends, loads and stores of every size with writeback, LDM/STM, and Thumb's 16- and
//! 32-bit forms with IT blocks; each case runs once interpreted and once compiled from the
//! same state.
#![allow(dead_code)]
use arm_a32::{CodeWatch, Cpu};
use emu_core::{Bus, Core, FlatRam};

pub const CODE: u32 = 0x1000;
pub const DATA: u32 = 0x8000;
pub const RAM: usize = 0x20000;

/// Whether compiled code can run here; prints why `test` is skipped when not (no backend
/// for this host, or the system refused the JIT executable memory).
pub fn jit_ready(test: &str) -> bool {
    if arm_a32::jit::available() {
        return true;
    }
    let why = if arm_a32::jit::refused() { "the system refuses writable and executable memory" } else { "no JIT for this host" };
    eprintln!("{test}: skipped ({why})");
    false
}

/// Turn the JIT on for `cpu` (whose RAM is set); false, with a message, if it cannot map
/// its code cache.
pub fn try_enable_jit(cpu: &mut Cpu) -> bool {
    if cpu.enable_jit(CodeWatch::new(0, RAM as u32)) {
        return true;
    }
    eprintln!("skipped: the JIT could not map its code cache");
    false
}

pub struct Rng(pub u64);
impl Rng {
    pub fn next(&mut self) -> u32 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        (self.0 >> 16) as u32
    }
    pub fn below(&mut self, n: u32) -> u32 {
        self.next() % n
    }
}

/// r0-r8 and lr are data, r9 a small offset, r10-r12 and sp point into the data area.
pub fn data_reg(r: &mut Rng) -> u32 {
    match r.below(10) {
        9 => 14,
        n => n,
    }
}

pub fn cond(r: &mut Rng) -> u32 {
    if r.below(3) == 0 {
        r.below(14)
    } else {
        14
    }
}

pub fn gen(r: &mut Rng) -> u32 {
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

pub fn setup(r: &mut Rng, words: &[u32]) -> (Cpu, FlatRam) {
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

pub fn run(cpu: &mut Cpu, ram: &mut FlatRam) {
    if let Err(e) = try_run(cpu, ram) {
        panic!("{e}");
    }
}

pub fn try_run(cpu: &mut Cpu, ram: &mut FlatRam) -> Result<(), String> {
    for _ in 0..1000 {
        match cpu.step(ram) {
            Ok(()) => {}
            Err(emu_core::Trap::Brk(0)) => return Ok(()),
            Err(t) => return Err(format!("{t:?} at {:#x}", cpu.r[15])),
        }
    }
    Err("did not reach the end".into())
}

/// Thumb: r0-r4 and r8-r12, lr are data, r5 a small offset, r6/r7 and sp point into the
/// data area.
pub fn t_low(r: &mut Rng) -> u32 {
    r.below(5)
}
pub fn t_any(r: &mut Rng) -> u32 {
    [0, 1, 2, 3, 4, 8, 9, 10, 11, 12, 14][r.below(11) as usize]
}
pub fn t_ptr(r: &mut Rng) -> u32 {
    6 + r.below(2)
}

/// One random Thumb instruction as halfwords.
pub fn gen_thumb(r: &mut Rng) -> Vec<u16> {
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

pub fn setup_thumb(r: &mut Rng, code: &[u16]) -> (Cpu, FlatRam) {
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


/// One random A32 case: the code (ending in BKPT) and the seed of its initial state.
pub fn arm_case(rng: &mut Rng) -> (Vec<u32>, u64) {
    let len = 1 + rng.below(40) as usize;
    let mut words: Vec<u32> = (0..len).map(|_| gen(rng)).collect();
    words.push(0xe120_0070); // bkpt #0
    (words, rng.next() as u64 | 1)
}

/// One random Thumb case: the code (with IT blocks, ending in BKPT) and its state's seed.
pub fn thumb_case(rng: &mut Rng) -> (Vec<u16>, u64) {
    let len = 1 + rng.below(30) as usize;
    let mut code: Vec<u16> = Vec::new();
    for _ in 0..len {
        if rng.below(6) == 0 {
            // an IT block of 1-4 single instructions
            let firstcond = rng.below(14);
            let k = 1 + rng.below(4);
            let mut mask = 1 << (4 - k);
            for i in 1..k {
                let then = rng.below(2) == 0;
                let bit = (firstcond & 1) ^ u32::from(!then);
                mask |= bit << (4 - i);
            }
            code.push((0xbf00 | firstcond << 4 | mask) as u16);
            let mut done = 0;
            while done < k {
                let v = gen_thumb(rng);
                let single = v.len() == 1 || v.len() == 2 && v[0] >> 11 >= 0b11101;
                if single {
                    code.extend(v);
                    done += 1;
                }
            }
        } else {
            code.extend(gen_thumb(rng));
        }
    }
    code.push(0xbe00); // bkpt #0
    (code, rng.next() as u64 | 1)
}

/// Run one case interpreted and compiled (`chain_until`: how far compiled code may run on
/// before returning to the caller) and compare registers, flags, the instruction count and
/// memory. Err: the first difference.
pub fn compare_case(thumb: bool, arm: &[u32], code16: &[u16], seed: u64, chain_until: u64) -> Result<(), String> {
    let mk = || if thumb { setup_thumb(&mut Rng(seed), code16) } else { setup(&mut Rng(seed), arm) };
    let (mut a, mut ram_a) = mk();
    let (mut b, mut ram_b) = mk();
    try_run(&mut a, &mut ram_a).map_err(|e| format!("interpreter: {e}"))?;
    // SAFETY: ram_b outlives b
    unsafe { b.set_ram(ram_b.mem.as_mut_ptr(), 0, RAM as u32) };
    if !b.enable_jit(CodeWatch::new(0, RAM as u32)) {
        return Err("no JIT on this host".into());
    }
    b.jit_set_hot(1);
    b.chain_until = chain_until;
    try_run(&mut b, &mut ram_b).map_err(|e| format!("compiled: {e}"))?;
    if b.jit_stats().unwrap().0 < 1 {
        return Err("nothing compiled".into());
    }
    if a.r != b.r {
        return Err(format!("registers: interpreter {:08x?}, compiled {:08x?}", a.r, b.r));
    }
    if a.cpsr != b.cpsr {
        return Err(format!("cpsr: interpreter {:08x}, compiled {:08x}", a.cpsr, b.cpsr));
    }
    if a.instret() != b.instret() {
        return Err(format!("instret: interpreter {}, compiled {}", a.instret(), b.instret()));
    }
    if ram_a.mem != ram_b.mem {
        let at = ram_a.mem.iter().zip(&ram_b.mem).position(|(x, y)| x != y).unwrap();
        return Err(format!("memory at {at:#x}: interpreter {:02x}, compiled {:02x}", ram_a.mem[at], ram_b.mem[at]));
    }
    Ok(())
}

/// A random A32 case as a loop: `mov r9, #3`, the body, `subs r9, r9, #1`, `bne` back
/// (r9 is only ever an offset register in the body, so the loop runs three times).
pub fn arm_loop_case(rng: &mut Rng) -> (Vec<u32>, u64) {
    let (mut body, seed) = arm_case(rng);
    body.pop(); // bkpt
    let mut words = vec![0xe3a0_9003]; // mov r9, #3
    words.extend(body);
    words.push(0xe259_9001); // subs r9, r9, #1
    let bne_at = CODE + 4 * words.len() as u32;
    let offset = ((CODE + 4) as i64 - (bne_at as i64 + 8)) >> 2;
    words.push(0x1a00_0000 | (offset as u32 & 0x00ff_ffff)); // bne loop
    words.push(0xe120_0070); // bkpt #0
    (words, seed)
}

/// A random Thumb case as a loop: `movs r5, #12`, the body, `subs r5, #4`, `bne.w` back
/// (r5 is only ever an offset register in the body).
pub fn thumb_loop_case(rng: &mut Rng) -> (Vec<u16>, u64) {
    let (mut body, seed) = thumb_case(rng);
    body.pop(); // bkpt
    let mut code = vec![0x250c]; // movs r5, #12
    code.extend(body);
    code.push(0x3d04); // subs r5, #4
    let at = CODE + 2 * code.len() as u32;
    let offset = (CODE + 2).wrapping_sub(at + 4);
    let (s, j2, j1) = ((offset >> 20) & 1, (offset >> 19) & 1, (offset >> 18) & 1);
    code.push((0xf000 | s << 10 | 1 << 6 | (offset >> 12) & 0x3f) as u16); // bne.w loop
    code.push((0x8000 | j1 << 13 | j2 << 11 | (offset >> 1) & 0x7ff) as u16);
    code.push(0xbe00); // bkpt #0
    (code, seed)
}
