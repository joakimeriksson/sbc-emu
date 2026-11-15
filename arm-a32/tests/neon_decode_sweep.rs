//! Advanced SIMD decode coverage against arm-none-eabi-objdump (opt-in: needs the oracle
//! file from `tools/neon_decode_sweep.py`). Every encoding objdump treats as UNDEFINED in
//! ARMv7 must be UNDEFINED here; encodings the emulator rejects although objdump decodes
//! them are listed by mnemonic for review (objdump is more lenient than the ARMv7 UNDEFINED
//! rules: .f8 and pairwise Q forms, VMUL.P16, D-form VZIP.32, register lists past d31...).
use arm_a32::{Cpu, MODE_SVC};
use emu_core::{Bus, Core, FlatRam, Trap};
use std::collections::BTreeMap;

#[test]
#[ignore = "needs NEON_ORACLE from tools/neon_decode_sweep.py"]
fn decode_agrees_with_objdump() {
    let path = std::env::var("NEON_ORACLE").expect("NEON_ORACLE=<file from tools/neon_decode_sweep.py>");
    let text = std::fs::read_to_string(path).unwrap();
    let mut ram = FlatRam::new(0, 0x10000);
    let mut stricter = BTreeMap::<String, Vec<String>>::new();
    let mut lenient = Vec::new();
    let mut total = 0;
    for line in text.lines() {
        let mut fields = line.splitn(3, ' ');
        let word = u32::from_str_radix(fields.next().unwrap(), 16).unwrap();
        let undefined = fields.next().unwrap() == "1";
        let disassembly = fields.next().unwrap_or("").to_string();
        let mut cpu = Cpu::new();
        cpu.cpsr = MODE_SVC | 0xc0;
        cpu.cpacr = 0xf << 20;
        cpu.vfp.fpexc = 1 << 30;
        cpu.r[0] = 0x8000;
        cpu.r[15] = 0x1000;
        ram.write32(0x1000, word).unwrap();
        let emulator_undefined = matches!(cpu.step(&mut ram), Err(Trap::Unimplemented(..)));
        total += 1;
        if emulator_undefined && !undefined {
            let mnemonic = disassembly.split_whitespace().next().unwrap_or("").to_string();
            stricter.entry(mnemonic).or_default().push(format!("{word:08x} {disassembly}"));
        } else if !emulator_undefined && undefined {
            lenient.push(format!("{word:08x} {disassembly}"));
        }
    }
    for (mnemonic, words) in &stricter {
        println!("UNDEFINED here, decoded by objdump: {mnemonic} x{} e.g. {}", words.len(), words[0]);
    }
    println!("{total} encodings swept");
    assert!(lenient.is_empty(), "executed although objdump says UNDEFINED:\n{}", lenient.join("\n"));
}
