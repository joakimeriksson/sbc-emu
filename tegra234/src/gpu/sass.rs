//! Initial Ampere scalar SASS interpreter. Each instruction is 128 bits.
//! Encoding reference: Mesa's Nouveau NAK SM70+ encoder.
use super::mmu::View;

const RZ: u8 = 255;
const URZ: u8 = 63;
const MAX_INSTRUCTIONS: usize = 256;
const MAX_THREAD_STEPS: usize = 1024;
pub(super) const CONSTANT_BINDINGS: usize = 8;

#[derive(Clone, Copy)]
pub(super) struct ConstantBuffer {
    pub address: u64,
    pub size: u32,
}

impl ConstantBuffer {
    fn load(self, offset: u32, memory: &View) -> Option<u32> {
        if offset.checked_add(4)? > self.size {
            return None;
        }
        memory.load(self.address + u64::from(offset))
    }
}

#[derive(Clone, Copy)]
enum Operand {
    Register(u8),
    Uniform(u8),
    Immediate(u32),
    Constant(u8, u16),
}

impl Operand {
    fn constant(raw: u128) -> Self {
        Self::Constant((raw >> 54) as u8 & 0x1f, (raw >> 38) as u16)
    }

    fn get(
        self,
        registers: &[u32; 256],
        uniform: &[u32; 64],
        constants: &[Option<ConstantBuffer>; CONSTANT_BINDINGS],
        memory: &View,
    ) -> Option<u32> {
        Some(match self {
            Self::Register(index) => registers[usize::from(index)],
            Self::Uniform(index) => uniform[usize::from(index)],
            Self::Immediate(value) => value,
            Self::Constant(binding, offset) => {
                let buffer = (*constants.get(usize::from(binding))?)?;
                buffer.load(u32::from(offset), memory)?
            }
        })
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
    MoveUniform(u8, u32),
    LoadUniformPair(u8, u8, u16),
    BarrierImmediate(u8, u32),
    BarrierMoveClear(u8, u8),
    MultiplyAdd(u8, u8, u8, Operand),
    Add(u8, u8, Operand, u8),
    Compare(u8, u8, Operand, Compare),
    System(u8, u8),
    Load(u8, Address),
    LoadConstant(u8, u8, u8, u16),
    Store(Address, u8),
}

struct Address {
    low_register: u8,
    uniform_register: u8,
    offset: i32,
}

impl Address {
    fn new(raw: u128, uniform_register: u8) -> Option<Self> {
        let low_register = (raw >> 24) as u8;
        let extended = (raw >> 90) & 3 == 3;
        let word_access = (raw >> 73) & 7 == 4;
        (extended && word_access && low_register < RZ - 1 && uniform_register <= URZ).then_some(
            Self {
                low_register,
                uniform_register,
                offset: ((raw as u64 as i64) >> 40) as i32,
            },
        )
    }

    fn get(&self, registers: &[u32; 256], uniform: &[u32; 64]) -> u64 {
        let i = usize::from(self.low_register);
        let base = u64::from(registers[i]) | u64::from(registers[i + 1]) << 32;
        let u = usize::from(self.uniform_register);
        let uniform_base =
            u64::from(uniform[u]) | u64::from(uniform.get(u + 1).copied().unwrap_or(0)) << 32;
        base.wrapping_add(uniform_base)
            .wrapping_add_signed(i64::from(self.offset))
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
            0xa00 => Operand::constant(raw),
            0xc00 => Operand::Uniform((raw >> 32) as u8 & URZ),
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
            0xa02
                if (raw >> 72) & 0xf == 0xf && (raw >> 59) & 0x1f == 0 && (raw >> 91) & 1 == 0 =>
            {
                Self::Move(dst, b)
            }
            0x882 if dst <= URZ && (raw >> 64) & 0x0fff_ffff == 0 => {
                Self::MoveUniform(dst, (raw >> 32) as u32)
            }
            0xab9 if dst < URZ - 1 && (raw >> 73) & 7 == 5 && (raw >> 91) & 1 == 0 => {
                Self::LoadUniformPair(dst, (raw >> 54) as u8 & 0x1f, (raw >> 38) as u16)
            }
            0x956 if a < 16 && (raw >> 64) & 0xff_ffff == 0 => {
                Self::BarrierImmediate(a, (raw >> 32) as u32)
            }
            0xf55 if dst < 16 && (raw >> 32) & 0xff < 16 && (raw >> 84) & 1 != 0 => {
                Self::BarrierMoveClear(dst, (raw >> 32) as u8)
            }
            0x424 | 0x624 if ((raw >> 72) & 0x7ffff) & !2 == 0x78e00 => {
                let add = if opcode == 0x424 {
                    Operand::Immediate((raw >> 32) as u32)
                } else {
                    Operand::constant(raw)
                };
                Self::MultiplyAdd(dst, a, (raw >> 64) as u8, add)
            }
            // Plain IADD3 with neither carry input, output predicate nor source modifier.
            0x210 | 0x810
                if (raw >> 72) & 0x7ffff == 0x7ffe0
                    && (opcode == 0x810 || (raw >> 40) & 0xffffff == 0) =>
            {
                Self::Add(dst, a, b, (raw >> 64) as u8)
            }
            // ISETP without the extended comparison or source modifiers.
            0x20c | 0x80c | 0xc0c
                if (raw >> 72) & 1 == 0
                    && (raw >> 74) & 3 != 3
                    && (raw >> 84) & 7 == 7
                    && (opcode == 0x80c || (raw >> 40) & 0xffffff == 0)
                    && (opcode != 0xc0c
                        || ((raw >> 32) & 0xff <= u128::from(URZ) && (raw >> 91) & 1 != 0)) =>
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
            0xb82 if (raw >> 73) & 7 == 4 && (raw >> 78) & 3 == 0 && (raw >> 91) & 1 == 0 => {
                Self::LoadConstant(dst, a, (raw >> 54) as u8 & 0x1f, (raw >> 38) as u16)
            }
            0x919 => Self::System(dst, (raw >> 72) as u8),
            // Ampere LDG.E/STG.E with an extended register-pair address and
            // one 32-bit value. LDG here uses the zero uniform register.
            0x981
                if (raw >> 32) & 0xff == 63
                    && (raw >> 64) & 0xf == 0
                    && (raw >> 81) & 7 == 7
                    && (raw >> 87) & 1 == 0
                    && (raw >> 72) & 1 == 1 =>
            {
                Self::Load(dst, Address::new(raw, URZ)?)
            }
            0x986 if (raw >> 72) & 1 == 1 => {
                Self::Store(Address::new(raw, (raw >> 64) as u8)?, (raw >> 32) as u8)
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

    pub fn run(
        &self,
        memory: &mut View,
        local: [u32; 3],
        group: [u32; 3],
        constants: &[Option<ConstantBuffer>; CONSTANT_BINDINGS],
    ) -> Option<()> {
        let mut registers = [0u32; 256];
        let mut uniform = [0u32; 64];
        let mut barriers = [0u32; 16];
        let mut predicates = [false; 7];
        let mut pc = 0;
        for _ in 0..MAX_THREAD_STEPS {
            let (predicate, instruction) = self.0.get(pc)?;
            pc += 1;
            if !predicate.test(&predicates) {
                continue;
            }
            let operand = |src: Operand| src.get(&registers, &uniform, constants, memory);
            let (dst, value) = match *instruction {
                Instruction::Nop => continue,
                Instruction::Exit => return Some(()),
                Instruction::Branch(condition, relative) => {
                    if condition.test(&predicates) {
                        pc = usize::try_from(pc as i64 + relative).ok()?;
                    }
                    continue;
                }
                Instruction::Move(dst, src) => (dst, operand(src)?),
                Instruction::MoveUniform(dst, value) => {
                    if dst != URZ {
                        uniform[usize::from(dst)] = value;
                    }
                    continue;
                }
                Instruction::LoadUniformPair(dst, binding, offset) => {
                    let buffer = (*constants.get(usize::from(binding))?)?;
                    let lo = buffer.load(u32::from(offset), memory)?;
                    let hi = buffer.load(u32::from(offset) + 4, memory)?;
                    let dst = usize::from(dst);
                    uniform[dst] = lo;
                    uniform[dst + 1] = hi;
                    continue;
                }
                Instruction::BarrierImmediate(dst, value) => {
                    barriers[usize::from(dst)] = value;
                    continue;
                }
                Instruction::BarrierMoveClear(dst, src) => {
                    let value = std::mem::take(&mut barriers[usize::from(src)]);
                    barriers[usize::from(dst)] = value;
                    continue;
                }
                Instruction::MultiplyAdd(dst, a, b, add) => (
                    dst,
                    registers[usize::from(a)]
                        .wrapping_mul(registers[usize::from(b)])
                        .wrapping_add(operand(add)?),
                ),
                Instruction::Add(dst, a, b, c) => (
                    dst,
                    registers[a as usize]
                        .wrapping_add(operand(b)?)
                        .wrapping_add(registers[c as usize]),
                ),
                Instruction::Compare(dst, a, b, ref comparison) => {
                    if dst != Predicate::ALWAYS {
                        predicates[usize::from(dst)] =
                            comparison.test(registers[usize::from(a)], operand(b)?, &predicates);
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
                Instruction::Load(dst, ref address) => {
                    (dst, memory.load(address.get(&registers, &uniform))?)
                }
                Instruction::LoadConstant(dst, index, binding, offset) => {
                    let buffer = (*constants.get(usize::from(binding))?)?;
                    let offset = registers[usize::from(index)].wrapping_add(u32::from(offset));
                    (dst, buffer.load(offset, memory)?)
                }
                Instruction::Store(ref address, data) => {
                    memory.store(address.get(&registers, &uniform), registers[data as usize])?;
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
