//! Thumb-2 against A32: every case is the same instruction (or an equivalent sequence)
//! assembled for both instruction sets by `arm-none-eabi-as`
//! (`tools/gen_thumb_diff.py`). Both run from the same random register, flag and memory
//! state; registers r0-r11, NZCVQ/GE and memory must end identical. r12 is scratch for
//! the A32 side of the equivalent sequences.
use arm_a32::{Cpu, MODE_SVC};
use emu_core::{Bus, Core, FlatRam};

#[path = "thumb_diff_cases.rs"]
mod cases;

const ARM_CODE: u64 = 0x1000;
const THUMB_CODE: u64 = 0x3000;
const BUFFER: u64 = 0x8000;
const T: u32 = 1 << 5;

struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u32 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        (self.0 >> 16) as u32
    }

    /// Mostly random words, with a share of edge values for flags and saturation.
    fn operand(&mut self) -> u32 {
        match self.next() % 8 {
            0 => [0, 1, 0x7fff_ffff, 0x8000_0000, 0xffff_ffff, 0x8000, 0x7fff, 0xff][self.next() as usize % 8],
            _ => self.next(),
        }
    }
}

fn run(mut cpu: Cpu, ram: &mut FlatRam, steps: usize) -> Cpu {
    for _ in 0..steps {
        cpu.step(ram).expect("instruction executes");
    }
    cpu
}

#[test]
fn thumb_matches_arm_for_every_case() {
    let mut rng = Rng(0x2545_f491_4f6c_dd1d);
    let mut failures = Vec::new();
    for &(name, arm, thumb) in cases::CASES {
        let memory_form = name.contains("[r9") || name.contains("r9!") || name.contains(" r9,");
        for _ in 0..64 {
            let mut regs = [0u32; 16];
            for r in regs.iter_mut().take(13) {
                *r = rng.operand();
            }
            if memory_form {
                regs[1] = rng.next() & 0x3c;
                regs[2] = rng.next() & 0x3e;
                regs[9] = BUFFER as u32 + 0x800;
            }
            let flags = rng.next() & 0xf80f_0000;
            let mut memory = vec![0u8; 0x1000];
            for byte in memory.iter_mut() {
                *byte = rng.next() as u8;
            }

            let mut results = Vec::new();
            for thumb_side in [false, true] {
                let mut ram = FlatRam::new(0, 0x10000);
                ram.load(BUFFER, &memory).unwrap();
                let mut cpu = Cpu::new();
                cpu.r[..13].copy_from_slice(&regs[..13]);
                cpu.r[13] = 0x7000;
                cpu.cpsr = MODE_SVC | flags | 0xc0;
                let steps = if thumb_side {
                    let mut address = THUMB_CODE;
                    for &(word, size) in thumb {
                        if size == 4 {
                            ram.write16(address, (word >> 16) as u16).unwrap();
                            ram.write16(address + 2, word as u16).unwrap();
                        } else {
                            ram.write16(address, word as u16).unwrap();
                        }
                        address += size as u64;
                    }
                    cpu.r[15] = THUMB_CODE as u32;
                    cpu.cpsr |= T;
                    thumb.len()
                } else {
                    for (index, &word) in arm.iter().enumerate() {
                        ram.write32(ARM_CODE + index as u64 * 4, word).unwrap();
                    }
                    cpu.r[15] = ARM_CODE as u32;
                    arm.len()
                };
                let cpu = run(cpu, &mut ram, steps);
                let mut buffer = vec![0u8; 0x1000];
                for (i, byte) in buffer.iter_mut().enumerate() {
                    *byte = ram.read8(BUFFER + i as u64).unwrap();
                }
                results.push((cpu, buffer));
            }
            let (arm_cpu, arm_mem) = &results[0];
            let (thumb_cpu, thumb_mem) = &results[1];
            let thumb_len: u32 = thumb.iter().map(|&(_, size)| size).sum();
            let mismatch = if arm_cpu.r[..12] != thumb_cpu.r[..12] {
                Some(format!("registers\n  arm   {:08x?}\n  thumb {:08x?}", &arm_cpu.r[..12], &thumb_cpu.r[..12]))
            } else if arm_cpu.cpsr & 0xf80f_0000 != thumb_cpu.cpsr & 0xf80f_0000 {
                Some(format!("flags arm {:08x} thumb {:08x}", arm_cpu.cpsr, thumb_cpu.cpsr))
            } else if arm_mem != thumb_mem {
                Some("memory".to_string())
            } else if thumb_cpu.r[15] != THUMB_CODE as u32 + thumb_len || thumb_cpu.cpsr & T == 0 {
                Some(format!("thumb pc {:#x} cpsr {:#x}", thumb_cpu.r[15], thumb_cpu.cpsr))
            } else {
                None
            };
            if let Some(what) = mismatch {
                failures.push(format!("{name}: {what}\n  initial {regs:08x?} flags {flags:08x}"));
                break;
            }
        }
    }
    assert!(failures.is_empty(), "{} of {} cases differ:\n{}", failures.len(), cases::CASES.len(), failures.join("\n"));
}
