//! FP/SIMD decode against LLVM's disassembler: every encoding of the FP and Advanced SIMD
//! data-processing groups (scalar FP with every Rd, for FCMP's opcode2; the other register
//! fields fixed) and of the SIMD structure loads/stores runs once on the interpreter. What it
//! executes must be an instruction of the ARMv8.0 cores it models (with CRC32, and with the
//! Crypto Extension when that is on); what it makes UNDEFINED must not be one. The AArch64 JIT
//! emits the FP/SIMD instructions the interpreter accepts as themselves, so a reserved encoding
//! taken here reaches the host CPU (SIGILL). Needs clang and llvm-objdump (skipped without).
use arm_a64::Cpu;
use emu_core::{Bus, Core, FlatRam, Trap};
use std::process::Command;

#[derive(Clone, Copy, PartialEq)]
enum Outcome { Executed, Undefined, Other }

/// The encodings swept: FP/SIMD data processing (op0 bits 28:24 = 01110, 01111, 11110,
/// 11111 with bits 30:29 free and bit 31 clear) and LD1..LD4/ST1..ST4 (multiple and single
/// structures), Rd/Rt = 0 (every Rd for scalar FP), Rn = 1 (2 for the loads and stores).
fn space() -> Vec<u32> {
    let mut v = Vec::new();
    for b3029 in 0..4u32 {
        for grp in [0b01110u32, 0b01111, 0b11110, 0b11111] {
            let rds = if b3029 == 0 && grp == 0b11110 { 32 } else { 1 };
            for mid in 0..(1u32 << 14) {
                for rd in 0..rds { v.push((b3029 << 29) | (grp << 24) | (mid << 10) | (1 << 5) | rd); }
            }
        }
    }
    for q in 0..2u32 {
        for top in [0b001_1000u32, 0b001_1001, 0b001_1010, 0b001_1011] {
            for mid in 0..(1u32 << 13) { v.push((q << 30) | (top << 23) | (mid << 10) | (2 << 5)); }
        }
    }
    v
}

fn outcomes(words: &[u32], crypto: bool) -> Vec<Outcome> {
    let mut ram = FlatRam::new(0, 0x10000);
    let mut c = Cpu::new();
    c.crypto = crypto;
    words.iter().map(|&i| {
        (c.el, c.pc, c.sys.cpacr_el1, c.vbar[1], c.esr[1]) = (1, 0, 3 << 20, 0x800, u64::MAX);
        c.x[2] = 0x8000; // the loads' and stores' base
        ram.write32(0, i).unwrap();
        match c.step(&mut ram) {
            Ok(()) if c.pc == 4 => Outcome::Executed,
            Err(Trap::Exception(..)) if c.esr[1] >> 26 == 0 => Outcome::Undefined,
            _ => Outcome::Other,
        }
    }).collect()
}

fn tool(names: &[&str], xcrun: &str) -> Option<String> {
    let ok = |c: &str| Command::new(c).arg("--version").output().is_ok_and(|o| o.status.success());
    names.iter().map(|s| s.to_string()).find(|c| ok(c)).or_else(|| {
        let o = Command::new("xcrun").args(["--find", xcrun]).output().ok()?;
        let p = String::from_utf8(o.stdout).ok()?.trim().to_string();
        (o.status.success() && ok(&p)).then_some(p)
    })
}

/// LLVM's disassembly of each word (`<unknown>` for an unallocated one), in order.
fn disassemble(clang: &str, objdump: &str, words: &[u32], mattr: &str, dir: &std::path::Path) -> Vec<String> {
    let (s, o) = (dir.join("w.s"), dir.join("w.o"));
    let text: String = words.iter().map(|w| format!(".inst {w:#010x}\n")).collect();
    std::fs::write(&s, format!(".text\n{text}")).unwrap();
    let r = Command::new(clang).args(["--target=aarch64-none-elf", "-c"]).arg(&s).arg("-o").arg(&o).output().unwrap();
    assert!(r.status.success(), "clang: {}", String::from_utf8_lossy(&r.stderr));
    let r = Command::new(objdump).args(["-d", "--no-show-raw-insn", &format!("--mattr={mattr}")]).arg(&o).output().unwrap();
    assert!(r.status.success(), "llvm-objdump: {}", String::from_utf8_lossy(&r.stderr));
    let out: Vec<String> = String::from_utf8(r.stdout).unwrap().lines()
        .filter_map(|l| { let (a, t) = l.split_once(':')?; (a.trim().chars().all(|c| c.is_ascii_hexdigit()) && !a.trim().is_empty() && l.starts_with(' ')).then(|| t.trim().replace('\t', " ")) })
        .collect();
    assert_eq!(out.len(), words.len(), "llvm-objdump printed {} instructions for {} words", out.len(), words.len());
    out
}

#[test]
fn fp_simd_decode_agrees_with_llvm() {
    let (Some(clang), Some(objdump)) = (tool(&["clang"], "clang"), tool(&["llvm-objdump", "llvm-objdump-18", "llvm-objdump-19", "llvm-objdump-20", "llvm-objdump-17"], "llvm-objdump")) else {
        eprintln!("skipped: needs clang and llvm-objdump");
        return;
    };
    let dir = std::env::temp_dir().join(format!("arm-a64-simd-decode-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    // the oracle must be ARMv8.0 (llvm-objdump defaults to every extension): FMOV (vector,
    // immediate) of half precision is FEAT_FP16, MOVI is ARMv8.0
    let base = "+v8a,+neon,+fp-armv8,+crc";
    let probe = disassemble(&clang, &objdump, &[0x0f00_fc00, 0x0f00_e400], base, &dir);
    if !probe[0].contains("<unknown>") || probe[1].contains("<unknown>") {
        eprintln!("skipped: {objdump} --mattr={base} does not limit the disassembly to ARMv8.0 ({probe:?})");
        let _ = std::fs::remove_dir_all(&dir);
        return;
    }
    let words = space();
    let mut bad = Vec::new();
    for crypto in [false, true] {
        let mattr = if crypto { format!("{base},+aes,+sha2") } else { format!("{base},-aes,-sha2") };
        let got = outcomes(&words, crypto);
        let dis = disassemble(&clang, &objdump, &words, &mattr, &dir);
        let (mut executed, mut undefined) = (0, 0);
        for ((&w, &o), d) in words.iter().zip(&got).zip(&dis) {
            let unknown = d.contains("<unknown>");
            match o {
                Outcome::Executed => { executed += 1; if unknown { bad.push(format!("crypto {crypto}: {w:08x} executed, unallocated for LLVM")); } }
                Outcome::Undefined => { undefined += 1; if !unknown { bad.push(format!("crypto {crypto}: {w:08x} UNDEFINED, LLVM: {d}")); } }
                Outcome::Other => {}
            }
        }
        eprintln!("crypto {crypto}: {} encodings, {executed} executed, {undefined} UNDEFINED", words.len());
    }
    let _ = std::fs::remove_dir_all(&dir);
    assert!(bad.is_empty(), "{} disagreements:\n{}", bad.len(), bad[..bad.len().min(40)].join("\n"));
}
