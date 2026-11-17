//! Initial Mali Valhall command-stream interpreter.
//!
//! This implements a small synchronous subset used to move data and prepare
//! commands. Only linear RGBA8 clear passes are supported; shader execution is
//! not implemented; unknown commands stop explicitly rather than succeeding.
use crate::gpu_mmu::{AddressSpace, Fault};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Stop {
    Complete,
    Budget,
    Memory(Fault),
    Shader(crate::gpu_shader::Error),
    InvalidRegister,
    Unsupported { pc: u64, opcode: u8 },
    InvalidStream,
}

pub struct Stream {
    pub regs: [u32; 256],
    pub start: u64,
    pub pc: u64,
    pub end: u64,
    stack: Vec<(u64, u64, u64)>,
}
impl Stream {
    pub fn new(start: u64, size: u64) -> Result<Self, Stop> {
        if start & 7 != 0 || size & 7 != 0 {
            return Err(Stop::InvalidStream);
        }
        let end = start.checked_add(size).ok_or(Stop::InvalidStream)?;
        Ok(Self {
            regs: [0; 256],
            stack: Vec::new(),
            start,
            pc: start,
            end,
        })
    }
    fn pair(&self, reg: usize) -> Result<u64, Stop> {
        if reg >= 255 || reg & 1 != 0 {
            return Err(Stop::InvalidRegister);
        }
        Ok(self.regs[reg] as u64 | (self.regs[reg + 1] as u64) << 32)
    }
    fn set_pair(&mut self, reg: usize, value: u64) -> Result<(), Stop> {
        if reg >= 255 || reg & 1 != 0 {
            return Err(Stop::InvalidRegister);
        }
        self.regs[reg] = value as u32;
        self.regs[reg + 1] = (value >> 32) as u32;
        Ok(())
    }
    fn unwind(&mut self) {
        while self.pc == self.end {
            let Some((start, end, pc)) = self.stack.pop() else {
                break;
            };
            self.start = start;
            self.end = end;
            self.pc = pc;
        }
    }
    /// Execute at most `budget` instructions, leaving PC on an unhandled or faulting instruction.
    pub fn run(&mut self, mmu: &AddressSpace, ram: &mut [u8], budget: usize) -> Stop {
        for _ in 0..budget {
            self.unwind();
            if self.pc == self.end {
                return Stop::Complete;
            }
            if self.pc > self.end || self.end - self.pc < 8 {
                return Stop::InvalidStream;
            }
            let mut bytes = [0; 8];
            if let Err(f) = mmu.read(ram, self.pc, &mut bytes, true) {
                return Stop::Memory(f);
            }
            let insn = u64::from_le_bytes(bytes);
            let opcode = (insn >> 56) as u8;
            let dst = ((insn >> 48) & 255) as usize;
            let src = ((insn >> 40) & 255) as usize;
            let mut next = self.pc + 8;
            let result = match opcode {
                0 => Ok(()), // NOP
                1 => self.set_pair(dst, insn & 0x0000_ffff_ffff_ffff),
                2 => {
                    self.regs[dst] = insn as u32;
                    Ok(())
                }
                3 if (insn >> 32) & 1 == 0 => Ok(()),
                4 => crate::gpu_compute::dispatch(mmu,ram,&self.regs,insn),
                7 if insn & 0x00ff_ffff_ffff_ffff == 0 => {
                    crate::gpu_render::fragment(mmu, ram, &self.regs)
                }
                16 => {
                    self.regs[dst] = self.regs[src].wrapping_add(insn as u32);
                    Ok(())
                }
                17 => self.pair(src).and_then(|v| {
                    self.set_pair(dst, v.wrapping_add(insn as u32 as i32 as i64 as u64))
                }),
                32 => {
                    let addr = match self.pair(src) {
                        Ok(v) => v,
                        Err(e) => return e,
                    };
                    let len = self.regs[((insn >> 32) & 255) as usize] as u64;
                    if addr & 7 != 0 || len & 7 != 0 || self.stack.len() >= 16 {
                        return Stop::InvalidStream;
                    }
                    let Some(end) = addr.checked_add(len) else {
                        return Stop::InvalidStream;
                    };
                    self.stack.push((self.start, self.end, next));
                    self.start = addr;
                    self.end = end;
                    next = addr;
                    Ok(())
                }
                34 | 36 | 47 => Ok(()), // synchronous resources/cache; no in-flight operations
                51 => {
                    let a = match self.pair(src) {
                        Ok(v) => v,
                        Err(e) => return e,
                    };
                    let v = match self.pair(((insn >> 32) & 255) as usize) {
                        Ok(v) => v,
                        Err(e) => return e,
                    };
                    let mut b = [0; 8];
                    if let Err(f) = mmu.read(ram, a, &mut b, true) {
                        return Stop::Memory(f);
                    }
                    mmu.write(
                        ram,
                        a,
                        &u64::from_le_bytes(b).wrapping_add(v).to_le_bytes(),
                        true,
                    )
                    .map_err(Stop::Memory)
                }
                20 | 21 => self.multiple(mmu, ram, insn, opcode == 21),
                22 => {
                    let val = self.regs[src] as i32;
                    let take = match (insn >> 28) & 15 {
                        0 => val <= 0,
                        1 => val > 0,
                        2 => val == 0,
                        3 => val != 0,
                        4 => val < 0,
                        5 => val >= 0,
                        6 => true,
                        _ => {
                            return Stop::Unsupported {
                                pc: self.pc,
                                opcode,
                            }
                        }
                    };
                    if take {
                        let delta = (insn as u16 as i16 as i64) * 8;
                        let Some(target) = next.checked_add_signed(delta) else {
                            return Stop::InvalidStream;
                        };
                        if target < self.start || target > self.end {
                            return Stop::InvalidStream;
                        }
                        next = target;
                    }
                    Ok(())
                }
                _ => {
                    return Stop::Unsupported {
                        pc: self.pc,
                        opcode,
                    }
                }
            };
            if let Err(stop) = result {
                return stop;
            }
            self.pc = next;
        }
        self.unwind();
        if self.pc == self.end {
            Stop::Complete
        } else {
            Stop::Budget
        }
    }
    fn multiple(
        &mut self,
        mmu: &AddressSpace,
        ram: &mut [u8],
        insn: u64,
        store: bool,
    ) -> Result<(), Stop> {
        let base = ((insn >> 48) & 255) as usize;
        let addr_reg = ((insn >> 40) & 255) as usize;
        let mask = ((insn >> 16) & 0xffff) as u16;
        let offset = insn as u16 as i16 as i64 as u64;
        let addr = self.pair(addr_reg)?.wrapping_add(offset);
        if mask == 0 {
            return Ok(());
        }
        let count = (16 - mask.leading_zeros()) as usize;
        if base + count > self.regs.len() {
            return Err(Stop::InvalidRegister);
        }
        // Sparse lanes are accessed separately so absent lanes do not cause spurious faults.
        // Validate all lanes first so a fault does not leave a partial register/memory update.
        let mut lanes = Vec::new();
        for i in 0..count {
            if mask & (1 << i) == 0 {
                continue;
            }
            let va = addr
                .checked_add((i * 4) as u64)
                .ok_or(Stop::Memory(Fault::AddressSize))?;
            let mut physical = [0usize; 4];
            let mut value = [0; 4];
            for byte in 0..4 {
                let v = va
                    .checked_add(byte as u64)
                    .ok_or(Stop::Memory(Fault::AddressSize))?;
                let access = if store {
                    crate::gpu_mmu::Access::Write
                } else {
                    crate::gpu_mmu::Access::Read
                };
                let pa = mmu.translate(ram, v, access, true).map_err(Stop::Memory)?;
                let p = usize::try_from(pa)
                    .ok()
                    .filter(|&p| p < ram.len())
                    .ok_or(Stop::Memory(Fault::PhysicalBus))?;
                physical[byte] = p;
                value[byte] = ram[p];
            }
            lanes.push((i, physical, value));
        }
        for (i, physical, value) in lanes {
            if store {
                for (p, value) in physical.into_iter().zip(self.regs[base + i].to_le_bytes()) {
                    ram[p] = value;
                }
            } else {
                self.regs[base + i] = u32::from_le_bytes(value);
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn ins(op: u64, dst: u64, src: u64, imm: u64) -> u64 {
        op << 56 | dst << 48 | src << 40 | imm
    }
    #[test]
    fn executes_real_move_add_store_and_load_encodings() {
        let mmu = AddressSpace {
            transtab: 0,
            transcfg: 2,
        }; // identity mapping
        let mut ram = vec![0u8; 4096];
        let program = [
            ins(1, 0, 0, 0x400),
            ins(2, 4, 0, 37),
            ins(16, 5, 4, 5),
            ins(21, 4, 0, 0x0003_0000),
            ins(20, 8, 0, 0x0003_0000),
        ];
        for (i, word) in program.iter().enumerate() {
            ram[i * 8..i * 8 + 8].copy_from_slice(&word.to_le_bytes());
        }
        let mut s = Stream::new(0, 40).unwrap();
        assert_eq!(s.run(&mmu, &mut ram, 2), Stop::Budget);
        assert_eq!(s.run(&mmu, &mut ram, 8), Stop::Complete);
        assert_eq!((s.regs[8], s.regs[9]), (37, 42));
        assert_eq!(&ram[0x400..0x408], &[37, 0, 0, 0, 42, 0, 0, 0]);
    }
    #[test]
    fn unsupported_gpu_work_does_not_report_success_or_advance() {
        let mmu = AddressSpace {
            transtab: 0,
            transcfg: 2,
        };
        let mut ram = (4u64 << 56).to_le_bytes().to_vec();
        let mut s = Stream::new(0, 8).unwrap();
        assert_eq!(
            s.run(&mmu, &mut ram, 1),
            Stop::Unsupported { pc: 0, opcode: 4 }
        );
        assert_eq!(s.pc, 0);
    }
    #[test]
    fn gpu_stream_uses_guest_page_tables_for_code_and_data() {
        let mut ram = vec![0u8; 0x10000];
        let mut put =
            |addr: usize, val: u64| ram[addr..addr + 8].copy_from_slice(&val.to_le_bytes());
        put(0x1000, 0x2003);
        put(0x2000, 0x3003);
        put(0x3000, 0x4003);
        // Command buffer at VA 0x10000; output buffer at VA 0x20000.
        put(0x4080, 0x8000 | 0x443);
        put(0x4100, 0xa000 | 0x443);
        let program = [
            ins(1, 0, 0, 0x20000),
            ins(2, 4, 0, 0x12345678),
            ins(21, 4, 0, 1 << 16),
        ];
        for (i, word) in program.iter().enumerate() {
            put(0x8000 + i * 8, *word);
        }
        let mmu = AddressSpace {
            transtab: 0x1000,
            transcfg: 6 | (7 << 6),
        };
        let mut stream = Stream::new(0x10000, 24).unwrap();
        assert_eq!(stream.run(&mmu, &mut ram, 3), Stop::Complete);
        assert_eq!(&ram[0xa000..0xa004], &0x12345678u32.to_le_bytes());
    }

    #[test]
    fn branch_uses_signed_instruction_offset_and_honors_budget() {
        let mmu = AddressSpace {
            transtab: 0,
            transcfg: 2,
        };
        let mut ram = vec![0; 64];
        let program = [
            ins(2, 1, 0, 3),
            ins(16, 1, 1, 0xffff_ffff),
            ins(22, 0, 1, (3 << 28) | 0xfffe),
        ];
        for (i, word) in program.iter().enumerate() {
            ram[i * 8..i * 8 + 8].copy_from_slice(&word.to_le_bytes());
        }
        let mut stream = Stream::new(0, 24).unwrap();
        assert_eq!(stream.run(&mmu, &mut ram, 3), Stop::Budget);
        assert_eq!(stream.regs[1], 2);
        assert_eq!(stream.run(&mmu, &mut ram, 8), Stop::Complete);
        assert_eq!(stream.regs[1], 0);
    }
    #[test]
    fn call_returns_to_kernel_stream_and_sync_signals_completion() {
        let mmu = AddressSpace {
            transtab: 0,
            transcfg: 2,
        };
        let mut ram = vec![0u8; 1024];
        let code = [
            ins(1, 0, 0, 128),
            ins(2, 2, 0, 8),
            ins(32, 0, 0, 2 << 32),
            ins(1, 0, 0, 512),
            ins(1, 2, 0, 1),
            ins(51, 0, 0, 2 << 32),
        ];
        for (i, v) in code.into_iter().enumerate() {
            ram[i * 8..i * 8 + 8].copy_from_slice(&v.to_le_bytes());
        }
        ram[128..136].copy_from_slice(&ins(2, 4, 0, 42).to_le_bytes());
        let mut s = Stream::new(0, 48).unwrap();
        assert_eq!(s.run(&mmu, &mut ram, 64), Stop::Complete);
        assert_eq!(s.regs[4], 42);
        assert_eq!(u64::from_le_bytes(ram[512..520].try_into().unwrap()), 1);
    }
}
