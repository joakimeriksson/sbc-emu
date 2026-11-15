//! Thumb control flow on a linked ARM/Thumb program (assembled and linked at 0x1000 with
//! arm-none-eabi-as/ld): BLX register ARM->Thumb, PUSH/POP with POP {pc} back to ARM,
//! a conditional loop, ITE/ITT blocks (including skipped instructions), CBZ, TBB, a
//! literal-pool load, BL/BX within Thumb, BLX immediate Thumb->ARM, and an SVC taken from
//! Thumb whose ARM handler returns to Thumb with MOVS pc, lr.
//!
//! Source:
//!   _start (ARM):  adr r0, thumb_entry; orr r0, #1; blx r0; mov r11, #0xaa; bkpt #1
//!   thumb_entry:   push {r4, lr}; r1 = 5+4+3+2+1 (bne loop); cmp r1, #15
//!                  ite eq; moveq r3, #1; movne r3, #2; itt ne; movne r4, #9; movne r5, #9
//!                  movs r6, #0; cbz r6, 2f; movs r7, #99; 2: movs r1, #1; tbb [pc, r1]
//!                  (case 0: r0 = 10, case 1: r0 = 20); ldr r8, =0x12345678
//!                  bl thumb_func (r7 = 42); blx arm_func (r10 = 77); svc #5
//!                  mov r12, r4; pop {r4, pc}
//!   svc_handler at 0x1208 (VBAR 0x1200): mrs r9, spsr; mov r5, lr; movs pc, lr
use arm_a32::{Cpu, MODE_SVC};
use emu_core::{Bus, Core, FlatRam, Trap};

const IMAGE: &[u32] = &[
    0xe28f000c, 0xe3800001, 0xe12fff30, 0xe3a0b0aa, 0xe1200071, 0x2100b510, 0x18892205,
    0xd1fc3a01, 0xbf0c290f, 0x23022301, 0x2409bf1c, 0x26002509, 0x2763b106, 0xe8df2101,
    0x0301f001, 0xe000200a, 0xf8df2014, 0xf0008014, 0xf000f805, 0xdf05e808, 0xbd1046a4,
    0x4770272a, 0x12345678, 0xe3a0a04d, 0xe12fff1e,
];
const HANDLER: &[u32] = &[0xe14f9000, 0xe1a0500e, 0xe1b0f00e];

#[test]
fn mixed_arm_thumb_program_runs_to_completion() {
    let mut ram = FlatRam::new(0, 0x10000);
    for (i, word) in IMAGE.iter().enumerate() {
        ram.write32(0x1000 + i as u64 * 4, *word).unwrap();
    }
    for (i, word) in HANDLER.iter().enumerate() {
        ram.write32(0x1208 + i as u64 * 4, *word).unwrap();
    }
    let mut cpu = Cpu::new();
    cpu.r[15] = 0x1000;
    cpu.r[13] = 0x8000;
    cpu.vbar = 0x1200;
    let mut stop = None;
    for _ in 0..500 {
        match cpu.step(&mut ram) {
            Ok(()) | Err(Trap::Exception(..)) => {}
            other => {
                stop = Some(other);
                break;
            }
        }
    }
    assert_eq!(stop, Some(Err(Trap::Brk(1))), "pc={:#x} cpsr={:#x} r={:x?}", cpu.r[15], cpu.cpsr, cpu.r);
    assert_eq!(cpu.cpsr & (1 << 5), 0, "back in ARM state");
    assert_eq!(cpu.cpsr & 0x1f, MODE_SVC);
    assert_eq!(cpu.r[0], 20, "TBB took case 1");
    assert_eq!(cpu.r[1], 1);
    assert_eq!(cpu.r[3], 1, "ITE eq");
    assert_eq!(cpu.r[4], 0, "ITT ne skipped; r4 restored by POP");
    assert_eq!(cpu.r[5], 0x1050, "SVC from Thumb: LR = next instruction");
    assert_eq!(cpu.r[7], 42, "CBZ skipped the store of 99; BL/BX set 42");
    assert_eq!(cpu.r[8], 0x1234_5678, "literal pool");
    assert_ne!(cpu.r[9] & (1 << 5), 0, "SPSR recorded Thumb state");
    assert_eq!(cpu.r[10], 77, "BLX immediate into ARM and back");
    assert_eq!(cpu.r[11], 0xaa, "POP {{pc}} returned to ARM");
    assert_eq!(cpu.r[13], 0x8000, "stack balanced");
}

#[test]
fn data_abort_in_thumb_reports_address_plus_8() {
    // ldr r0, [r1] (16-bit) at 0x2000 with r1 outside RAM.
    let mut ram = FlatRam::new(0, 0x10000);
    ram.write16(0x2000, 0x6808).unwrap();
    let mut cpu = Cpu::new();
    cpu.r[15] = 0x2000;
    cpu.r[1] = 0x20000;
    cpu.cpsr |= 1 << 5;
    assert!(matches!(cpu.step(&mut ram), Err(Trap::Exception(..))));
    assert_eq!(cpu.r[14], 0x2008);
    assert_eq!(cpu.cpsr & (1 << 5), 0, "handlers run in ARM state");
}
