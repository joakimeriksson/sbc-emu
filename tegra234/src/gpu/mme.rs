//! The Turing-and-later macro method expander (MME) of the Ampere 3D class. Each instruction is
//! three words with two ALUs and two method outputs. The decoding and semantics follow Mesa's
//! `mme_tu104.c` and `mme_tu104_sim.c` (MIT, Collabora). The instruction RAM pointer and the
//! macro start addresses count whole instructions.

/// Instructions in the instruction RAM, and macros in the start-address RAM.
pub(super) const INSTRUCTIONS: usize = 0x800;
pub(super) const MACROS: usize = 0x100;
/// Words of the data RAM that DREAD and DWRITE address.
pub(super) const DATA_WORDS: usize = 0xc00;
/// A bound on one call. Macros loop over their parameters, so this is generous.
const MAX_STEPS: usize = 1 << 22;

const R23: u32 = 23;
const ZERO: u32 = 24;
const IMM: u32 = 25;
const IMMPAIR: u32 = 26;
const IMM32: u32 = 27;
const LOAD0: u32 = 28;
const LOAD1: u32 = 29;

/// Predicate modes: for each of ALU0, ALU1, OUT0 and OUT1, run always (U) or when the
/// predicate register is true (T) or false (F). Mesa's simulator runs a slot when the letter
/// differs from the register's truth, and so does this.
const PREDICATES: [&[u8; 4]; 16] = [
    b"UUUU", b"TTTT", b"FFFF", b"TTUU", b"FFUU", b"TFUU", b"TUUU", b"FUUU", b"UUTT", b"UUTF",
    b"UUTU", b"UUFT", b"UUFF", b"UUFU", b"UUUT", b"UUUF",
];

#[derive(Clone, Copy)]
struct Alu {
    op: u32,
    dst: u32,
    src: [u32; 2],
}

#[derive(Clone, Copy)]
struct Instruction {
    end_next: bool,
    pred_mode: usize,
    pred: u32,
    imm: [u16; 2],
    alu: [Alu; 2],
    /// (method source, emit source) per output.
    out: [(u32, u32); 2],
}

impl Instruction {
    /// The three words, highest bits first as the instruction RAM holds them.
    fn decode(words: [u32; 3]) -> Self {
        let bits = u128::from(words[2]) | u128::from(words[1]) << 32 | u128::from(words[0]) << 64;
        let field =
            |start: u32, end: u32| ((bits >> start) & ((1 << (end - start + 1)) - 1)) as u32;
        Self {
            end_next: field(0, 0) != 0,
            pred_mode: field(1, 4) as usize,
            pred: field(5, 9),
            alu: [
                Alu {
                    op: field(10, 14),
                    dst: field(15, 19),
                    src: [field(20, 24), field(25, 29)],
                },
                Alu {
                    op: field(46, 50),
                    dst: field(51, 55),
                    src: [field(56, 60), field(61, 65)],
                },
            ],
            imm: [field(30, 45) as u16, field(66, 81) as u16],
            out: [
                (field(82, 84), field(85, 88)),
                (field(89, 91), field(92, 95)),
            ],
        }
    }

    fn loads(&self, register: u32, out: u32) -> bool {
        self.pred == register
            || self.alu.iter().any(|alu| alu.src.contains(&register))
            || self
                .out
                .iter()
                .any(|&(method, emit)| method == out || emit == out)
    }
}

/// What a macro reads and writes: its parameters, the 3D state, methods and the data RAM.
pub(super) trait Host {
    /// The next parameter, or None when the call has no more.
    fn parameter(&mut self) -> Option<u32>;
    fn state(&mut self, method: u32) -> u32;
    fn method(&mut self, method: u32, value: u32) -> Option<()>;
    /// The barrier extended op: READ_FIFOED data becomes the next parameters.
    fn barrier(&mut self) -> Option<()>;
    fn data(&mut self) -> &mut [u32; DATA_WORDS];
}

pub(super) struct Mme {
    pub code: Box<[u32; INSTRUCTIONS * 3]>,
    pub code_pointer: usize,
    pub starts: Box<[u32; MACROS]>,
    pub start_pointer: usize,
    /// The macro being called and its parameters so far.
    pub call: Option<(usize, Vec<u32>)>,
}

impl Default for Mme {
    fn default() -> Self {
        Self {
            code: Box::new([0; INSTRUCTIONS * 3]),
            code_pointer: 0,
            starts: Box::new([0; MACROS]),
            start_pointer: 0,
            call: None,
        }
    }
}

impl Mme {
    /// The upload methods. Returns None for a method that is not one.
    pub fn upload(&mut self, method: u32, value: u32) -> Option<()> {
        match method {
            0x114 => self.code_pointer = value as usize * 3,
            0x118 => {
                *self.code.get_mut(self.code_pointer)? = value;
                self.code_pointer += 1;
            }
            0x11c => self.start_pointer = value as usize,
            0x120 => {
                *self.starts.get_mut(self.start_pointer)? = value;
                self.start_pointer += 1;
            }
            _ => return None,
        }
        Some(())
    }

    /// CALL_MME_MACRO (0x3800 + 8 k) starts a call with its first parameter and CALL_MME_DATA
    /// (+ 4) adds one. Returns the call this one replaces, which is ready to run.
    pub fn call(&mut self, method: u32, value: u32) -> Option<(usize, Vec<u32>)> {
        let index = (method as usize - 0x3800) / 8;
        if method & 4 == 0 {
            return self.call.replace((index, vec![value]));
        }
        match self.call.as_mut() {
            Some((current, parameters)) if *current == index => parameters.push(value),
            _ => self.call = Some((index, vec![value])),
        }
        None
    }

    /// Run macro `index`. None for a fault: an instruction outside the RAM, an unknown
    /// operation, or a method the host rejected.
    pub fn run(&self, index: usize, host: &mut dyn Host) -> Option<()> {
        let start = *self.starts.get(index)? as usize;
        let mut state = State::default();
        let mut ip = start;
        let mut end_next = false;
        for _ in 0..MAX_STEPS {
            if ip >= INSTRUCTIONS {
                return None;
            }
            let words = [
                self.code[ip * 3],
                self.code[ip * 3 + 1],
                self.code[ip * 3 + 2],
            ];
            let instruction = Instruction::decode(words);
            state.next_ip = ip + 1;
            state.ip = ip;
            if instruction.loads(LOAD0, 3) {
                state.load[0] = host.parameter().unwrap_or(0);
            }
            if instruction.loads(LOAD1, 4) {
                state.load[1] = host.parameter().unwrap_or(0);
            }
            let mask = state.predicate(&instruction)?;
            for slot in 0..2 {
                if mask & (1 << slot) != 0 {
                    state.alu(&instruction, slot, host)?;
                }
            }
            for slot in 0..2 {
                if mask & (4 << slot) != 0 {
                    state.output(&instruction, slot, host)?;
                }
            }
            if end_next || state.stop {
                return Some(());
            }
            end_next = instruction.end_next;
            if state.loop_count > 0 && ip == state.loop_end {
                state.loop_count -= 1;
                state.next_ip = state.loop_start + 1;
            }
            ip = state.next_ip;
        }
        None
    }
}

#[derive(Default)]
struct State {
    load: [u32; 2],
    method: u32,
    increment: u32,
    regs: [u32; 24],
    result: [u32; 2],
    carry: u32,
    ip: usize,
    next_ip: usize,
    stop: bool,
    loop_count: u32,
    loop_start: usize,
    loop_end: usize,
}

impl State {
    fn read(&self, instruction: &Instruction, slot: usize, register: u32) -> Option<u32> {
        Some(match register {
            0..=R23 => self.regs[register as usize],
            ZERO => 0,
            // Immediates are signed for the ALUs.
            IMM => instruction.imm[slot.min(1)] as i16 as u32,
            IMMPAIR => instruction.imm[1 - slot.min(1)] as i16 as u32,
            IMM32 => u32::from(instruction.imm[0]) << 16 | u32::from(instruction.imm[1]),
            LOAD0 => self.load[0],
            LOAD1 => self.load[1],
            _ => return None,
        })
    }

    fn predicate(&self, instruction: &Instruction) -> Option<u8> {
        if instruction.pred_mode == 0 {
            return Some(0xf);
        }
        let truth = if self.read(instruction, 0, instruction.pred)? != 0 {
            b'T'
        } else {
            b'F'
        };
        let letters = PREDICATES[instruction.pred_mode];
        Some(
            (0..4)
                .filter(|&i| letters[i] != truth)
                .fold(0, |mask, i| mask | 1 << i),
        )
    }

    fn alu(&mut self, instruction: &Instruction, slot: usize, host: &mut dyn Host) -> Option<()> {
        let alu = instruction.alu[slot];
        let x = self.read(instruction, slot, alu.src[0])?;
        let y = self.read(instruction, slot, alu.src[1])?;
        let imm = instruction.imm[slot];
        let compare = |op: u32| match op {
            17 | 25 => (x as i32) < (y as i32),
            18 | 26 => x < y,
            19 | 27 => (x as i32) <= (y as i32),
            20 | 28 => x <= y,
            _ => x == y,
        };
        let result = match alu.op {
            0 => {
                let (sum, carry) = x.overflowing_add(y);
                self.carry = carry.into();
                sum
            }
            1 => x.wrapping_add(y).wrapping_add(self.carry),
            2 => {
                let (difference, borrow) = x.overflowing_sub(y);
                self.carry = borrow.into();
                difference
            }
            3 => x.wrapping_sub(y).wrapping_sub(self.carry),
            4 => {
                let product = (x as i32 as i64).wrapping_mul(y as i32 as i64) as u64;
                self.carry = (product >> 32) as u32;
                product as u32
            }
            5 => self.carry,
            6 => {
                let product = u64::from(x) * u64::from(y);
                self.carry = (product >> 32) as u32;
                product as u32
            }
            // The one known extended operation is the READ_FIFOED barrier.
            7 => {
                if x != 0x1000 || y != 1 {
                    return None;
                }
                host.barrier()?;
                0
            }
            8 => x.leading_zeros(),
            9 => x << (y & 31),
            10 => x >> (y & 31),
            11 => ((x as i32) >> (y & 31)) as u32,
            12 => x & y,
            13 => !(x & y),
            14 => x | y,
            15 => x ^ y,
            16 => {
                let (to, bits, from) = (
                    u32::from(imm >> 10 & 0x3f),
                    u32::from(imm >> 5 & 0x1f),
                    u32::from(imm & 0x1f),
                );
                let mask = ((1u64 << bits) - 1) as u32;
                (x & !(mask.checked_shl(to).unwrap_or(0)))
                    | ((y >> from) & mask).checked_shl(to).unwrap_or(0)
            }
            17..=21 => {
                if compare(alu.op) {
                    u32::MAX
                } else {
                    0
                }
            }
            22 => host.state(u32::from(x.wrapping_add(y) as u16) * 4),
            23 => {
                self.loop_count = x.max(1) - 1;
                self.loop_start = self.ip;
                self.loop_end = self.ip + usize::from(imm) - 1;
                0
            }
            24 => {
                self.next_ip = self.ip + usize::from(imm & 0x7fff);
                0
            }
            25..=29 => {
                let expect = imm & 0x8000 != 0;
                if compare(alu.op) == expect {
                    // A 13-bit signed offset; 0xf000 ends the macro.
                    let offset = ((imm << 3) as i16 >> 3) as i32;
                    if offset as u16 == 0xf000 {
                        self.stop = true;
                    } else {
                        let target = self.ip as i32 + offset;
                        if !(0..INSTRUCTIONS as i32).contains(&target) {
                            return None;
                        }
                        self.next_ip = target as usize;
                    }
                }
                0
            }
            30 => *host.data().get(x as usize)?,
            31 => {
                *host.data().get_mut(x as usize)? = y;
                0
            }
            _ => return None,
        };
        self.result[slot] = result;
        match alu.dst {
            0..=R23 => self.regs[alu.dst as usize] = result,
            ZERO..=LOAD1 => {}
            _ => return None,
        }
        Some(())
    }

    fn source(&self, instruction: &Instruction, source: u32) -> Option<u32> {
        Some(match source {
            1 => self.result[0],
            2 => self.result[1],
            3 => self.load[0],
            4 => self.load[1],
            5 => u32::from(instruction.imm[0]),
            6 => u32::from(instruction.imm[1]),
            8 => u32::from(instruction.imm[0] >> 12),
            9 => u32::from(instruction.imm[1] >> 12),
            10 => u32::from(instruction.imm[0]) << 16 | u32::from(instruction.imm[1]),
            _ => return None,
        })
    }

    fn output(
        &mut self,
        instruction: &Instruction,
        slot: usize,
        host: &mut dyn Host,
    ) -> Option<()> {
        let (method, emit) = instruction.out[slot];
        if method != 0 {
            let data = self.source(instruction, method)?;
            self.method = (data & 0xfff) << 2;
            self.increment = data >> 12 & 0xf;
        }
        if emit != 0 {
            let data = self.source(instruction, emit)?;
            host.method(self.method, data)?;
            self.method = self.method.wrapping_add(self.increment * 4) & 0x3ffc;
        }
        Some(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Recorder {
        parameters: Vec<u32>,
        methods: Vec<(u32, u32)>,
        data: [u32; DATA_WORDS],
    }

    impl Host for Recorder {
        fn parameter(&mut self) -> Option<u32> {
            (!self.parameters.is_empty()).then(|| self.parameters.remove(0))
        }
        fn state(&mut self, _: u32) -> u32 {
            0
        }
        fn method(&mut self, method: u32, value: u32) -> Option<()> {
            self.methods.push((method, value));
            Some(())
        }
        fn barrier(&mut self) -> Option<()> {
            Some(())
        }
        fn data(&mut self) -> &mut [u32; DATA_WORDS] {
            &mut self.data
        }
    }

    fn encode(i: &Instruction) -> [u32; 3] {
        let mut bits = 0u128;
        let mut put = |start: u32, end: u32, value: u32| {
            bits |= (u128::from(value) & ((1 << (end - start + 1)) - 1)) << start;
        };
        put(0, 0, i.end_next.into());
        put(1, 4, i.pred_mode as u32);
        put(5, 9, i.pred);
        put(10, 14, i.alu[0].op);
        put(15, 19, i.alu[0].dst);
        put(20, 24, i.alu[0].src[0]);
        put(25, 29, i.alu[0].src[1]);
        put(30, 45, i.imm[0].into());
        put(46, 50, i.alu[1].op);
        put(51, 55, i.alu[1].dst);
        put(56, 60, i.alu[1].src[0]);
        put(61, 65, i.alu[1].src[1]);
        put(66, 81, i.imm[1].into());
        put(82, 84, i.out[0].0);
        put(85, 88, i.out[0].1);
        put(89, 91, i.out[1].0);
        put(92, 95, i.out[1].1);
        [(bits >> 64) as u32, (bits >> 32) as u32, bits as u32]
    }

    const NOP: Alu = Alu {
        op: 0,
        dst: ZERO,
        src: [ZERO, ZERO],
    };

    #[test]
    fn a_macro_adds_its_parameters_and_emits_the_sum() {
        // r0 = load0 + load1; method 0x1234 with step 1; emit r0, then imm1.
        let program = [
            Instruction {
                end_next: false,
                pred_mode: 0,
                pred: ZERO,
                imm: [0x1000 | 0x48d, 7],
                alu: [
                    Alu {
                        op: 0,
                        dst: 0,
                        src: [LOAD0, LOAD1],
                    },
                    NOP,
                ],
                out: [(5, 0), (0, 0)],
            },
            Instruction {
                end_next: true,
                pred_mode: 0,
                pred: ZERO,
                imm: [0, 7],
                alu: [
                    Alu {
                        op: 0,
                        dst: 1,
                        src: [0, ZERO],
                    },
                    NOP,
                ],
                out: [(0, 1), (0, 6)],
            },
            Instruction {
                end_next: false,
                pred_mode: 0,
                pred: ZERO,
                imm: [0, 0],
                alu: [NOP, NOP],
                out: [(0, 0), (0, 0)],
            },
        ];
        let mut mme = Mme::default();
        mme.upload(0x114, 4).unwrap();
        for word in program.iter().flat_map(encode) {
            mme.upload(0x118, word).unwrap();
        }
        mme.upload(0x11c, 3).unwrap();
        mme.upload(0x120, 4).unwrap();
        let mut host = Recorder {
            parameters: vec![40, 2],
            methods: Vec::new(),
            data: [0; DATA_WORDS],
        };
        mme.run(3, &mut host).unwrap();
        assert_eq!(host.methods, [(0x1234, 42), (0x1238, 7)]);
    }
}
