//! Initial Ampere scalar SASS interpreter. Each instruction is 128 bits.
//! Encoding reference: Mesa's Nouveau NAK SM70+ encoder.
use super::mmu::View;

const RZ: u8 = 255;
const MAX_INSTRUCTIONS: usize = 256;
const MAX_THREAD_STEPS: usize = 1024;

#[derive(Clone, Copy)]
enum Operand {
    Register(u8),
    Immediate(u32),
}

impl Operand {
    fn get(self, registers: &[u32; 256]) -> u32 {
        match self {
            Self::Register(index) => registers[index as usize],
            Self::Immediate(value) => value,
        }
    }
}

#[derive(Clone, Copy)]
struct Predicate(u8);

impl Predicate {
    const ALWAYS: u8 = 7;

    fn test(self, predicates: &[bool; 7]) -> bool {
        let index = self.0 & 7;
        let value = predicates.get(usize::from(index)).copied().unwrap_or(true);
        value ^ (self.0 & 8 != 0)
    }
}

struct Compare {
    signed: bool,
    kind: u8,
    combine: u8,
    accumulate: Predicate,
}

impl Compare {
    fn test(&self, a: u32, b: u32, predicates: &[bool; 7]) -> bool {
        use std::cmp::Ordering::{Equal, Greater, Less};
        let order = if self.signed {
            (a as i32).cmp(&(b as i32))
        } else {
            a.cmp(&b)
        };
        let compare = match self.kind {
            0 => false,
            1 => order == Less,
            2 => order == Equal,
            3 => order != Greater,
            4 => order == Greater,
            5 => order != Equal,
            6 => order != Less,
            _ => true,
        };
        let previous = self.accumulate.test(predicates);
        match self.combine {
            0 => compare && previous,
            1 => compare || previous,
            _ => compare ^ previous,
        }
    }
}

enum Instruction {
    Nop,
    Exit,
    Branch(Predicate, i64),
    Move(u8, Operand),
    Add(u8, u8, Operand, u8),
    Compare(u8, u8, Operand, Compare),
    System(u8, u8),
    Load(u8, Address),
    Store(Address, u8),
}

struct Address {
    low_register: u8,
    offset: i32,
}

impl Address {
    fn new(raw: u128) -> Option<Self> {
        let low_register = (raw >> 24) as u8;
        let extended = (raw >> 90) & 3 == 3;
        let word_access = (raw >> 73) & 7 == 4;
        (extended && word_access && low_register < RZ - 1).then_some(Self {
            low_register,
            offset: ((raw as u64 as i64) >> 40) as i32,
        })
    }

    fn get(&self, registers: &[u32; 256]) -> u64 {
        let i = usize::from(self.low_register);
        let base = u64::from(registers[i]) | u64::from(registers[i + 1]) << 32;
        base.wrapping_add_signed(i64::from(self.offset))
    }
}

impl Instruction {
    fn decode(raw: u128) -> Option<(Predicate, Self)> {
        let opcode = raw as u16 & 0xfff;
        let predication = Predicate((raw >> 12) as u8 & 0xf);
        let dst = (raw >> 16) as u8;
        let a = (raw >> 24) as u8;
        let b = match opcode & 0xe00 {
            0x200 => Operand::Register((raw >> 32) as u8),
            _ => Operand::Immediate((raw >> 32) as u32),
        };
        let instruction = match opcode {
            0x918 => Self::Nop,
            0x947 if (raw >> 32) & 1 == 0 && (raw >> 82) & 0x1f == 0 => {
                // BRA encodes a signed 48-bit offset in words from the next instruction.
                let words = ((raw >> 34) as i64) << 16 >> 16;
                if words % 4 != 0 {
                    return None;
                }
                Self::Branch(Predicate((raw >> 87) as u8 & 0xf), words / 4)
            }
            0x94d if (raw >> 84) & 0x7f == 0x38 => Self::Exit,
            0x202 | 0x802 if (raw >> 72) & 0xf == 0xf => Self::Move(dst, b),
            // Plain IADD3 with neither carry input, output predicate nor source modifier.
            0x210 | 0x810
                if (raw >> 72) & 0x7ffff == 0x7ffe0
                    && (opcode == 0x810 || (raw >> 40) & 0xffffff == 0) =>
            {
                Self::Add(dst, a, b, (raw >> 64) as u8)
            }
            // ISETP without the extended comparison or source modifiers.
            0x20c | 0x80c
                if (raw >> 72) & 1 == 0
                    && (raw >> 74) & 3 != 3
                    && (raw >> 84) & 7 == 7
                    && (opcode == 0x80c || (raw >> 40) & 0xffffff == 0) =>
            {
                Self::Compare(
                    (raw >> 81) as u8 & 7,
                    a,
                    b,
                    Compare {
                        signed: (raw >> 73) & 1 != 0,
                        kind: (raw >> 76) as u8 & 7,
                        combine: (raw >> 74) as u8 & 3,
                        accumulate: Predicate((raw >> 87) as u8 & 0xf),
                    },
                )
            }
            0x919 => Self::System(dst, (raw >> 72) as u8),
            // Ampere LDG.E/STG.E with an extended GPR-pair address, UZR,
            // and a single 32-bit value.
            0x981
                if (raw >> 32) & 0xff == 63
                    && (raw >> 64) & 0xf == 0
                    && (raw >> 81) & 7 == 7
                    && (raw >> 87) & 1 == 0
                    && (raw >> 72) & 1 == 1 =>
            {
                Self::Load(dst, Address::new(raw)?)
            }
            0x986 if (raw >> 64) & 0xff == 63 && (raw >> 72) & 1 == 1 => {
                Self::Store(Address::new(raw)?, (raw >> 32) as u8)
            }
            _ => return None,
        };
        Some((predication, instruction))
    }
}

pub(super) struct Program(Vec<(Predicate, Instruction)>);

impl Program {
    pub fn read(memory: &View, address: u64) -> Option<Self> {
        let mut instructions = Vec::new();
        let mut last_target = 0;
        for index in 0..MAX_INSTRUCTIONS {
            let address = address + 16 * index as u64;
            let mut raw = 0;
            for word in 0..4 {
                raw |= u128::from(memory.load(address + 4 * word)?) << (32 * word);
            }
            let (predicate, instruction) = Instruction::decode(raw)?;
            if let Instruction::Branch(_, relative) = instruction {
                let target = usize::try_from(index as i64 + 1 + relative).ok()?;
                if target >= MAX_INSTRUCTIONS {
                    return None;
                }
                last_target = last_target.max(target);
            }
            let done = predicate.0 == Predicate::ALWAYS
                && index >= last_target
                && matches!(instruction, Instruction::Exit);
            instructions.push((predicate, instruction));
            if done {
                return Some(Self(instructions));
            }
        }
        None
    }

    pub fn run(&self, memory: &mut View, local: [u32; 3], group: [u32; 3]) -> Option<()> {
        let mut registers = [0u32; 256];
        let mut predicates = [false; 7];
        let mut pc = 0;
        for _ in 0..MAX_THREAD_STEPS {
            let (predicate, instruction) = self.0.get(pc)?;
            pc += 1;
            if !predicate.test(&predicates) {
                continue;
            }
            let (dst, value) = match *instruction {
                Instruction::Nop => continue,
                Instruction::Exit => return Some(()),
                Instruction::Branch(condition, relative) => {
                    if condition.test(&predicates) {
                        pc = usize::try_from(pc as i64 + relative).ok()?;
                    }
                    continue;
                }
                Instruction::Move(dst, src) => (dst, src.get(&registers)),
                Instruction::Add(dst, a, b, c) => (
                    dst,
                    registers[a as usize]
                        .wrapping_add(b.get(&registers))
                        .wrapping_add(registers[c as usize]),
                ),
                Instruction::Compare(dst, a, b, ref comparison) => {
                    if dst != Predicate::ALWAYS {
                        predicates[usize::from(dst)] = comparison.test(
                            registers[usize::from(a)],
                            b.get(&registers),
                            &predicates,
                        );
                    }
                    continue;
                }
                Instruction::System(dst, selector) => (
                    dst,
                    match selector {
                        0x21..=0x23 => local[(selector - 0x21) as usize],
                        0x25..=0x27 => group[(selector - 0x25) as usize],
                        _ => return None,
                    },
                ),
                Instruction::Load(dst, ref address) => (dst, memory.load(address.get(&registers))?),
                Instruction::Store(ref address, data) => {
                    memory.store(address.get(&registers), registers[data as usize])?;
                    continue;
                }
            };
            if dst != RZ {
                registers[usize::from(dst)] = value;
            }
        }
        None
    }
}
