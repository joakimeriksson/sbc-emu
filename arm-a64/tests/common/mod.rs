//! Assemble A64 snippets with the host clang (`--target=aarch64-none-elf`) and run them on the
//! core from 0x8_0000 (where Pi firmware loads a 64-bit kernel) until `brk #0`.
#![allow(dead_code)]
use arm_a64::Cpu;
use emu_core::{Bus, Core, FlatRam, Trap};
use std::process::Command;
use std::sync::atomic::{AtomicU32, Ordering};

pub const LOAD: u64 = 0x8_0000;
pub const RAM: usize = 16 << 20;

/// Assemble and return the bytes of `.text` (no relocations allowed).
pub fn assemble(src: &str) -> Vec<u8> {
    static N: AtomicU32 = AtomicU32::new(0);
    let dir = std::env::temp_dir().join(format!(
        "arm-a64-test-{}-{}",
        std::process::id(),
        N.fetch_add(1, Ordering::Relaxed)
    ));
    std::fs::create_dir_all(&dir).unwrap();
    let s = dir.join("t.s");
    let o = dir.join("t.o");
    std::fs::write(&s, format!(".text\n.globl _start\n_start:\n{src}\n")).unwrap();
    let out = Command::new("clang")
        .args(["--target=aarch64-none-elf", "-march=armv8-a", "-c"])
        .arg(&s)
        .arg("-o")
        .arg(&o)
        .output()
        .expect("clang");
    assert!(
        out.status.success(),
        "assembler failed:\n{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let elf = std::fs::read(&o).unwrap();
    let _ = std::fs::remove_dir_all(&dir);
    elf_section(&elf, ".text").expect(".text")
}

fn u16_at(b: &[u8], o: usize) -> usize {
    u16::from_le_bytes(b[o..o + 2].try_into().unwrap()) as usize
}
fn u32_at(b: &[u8], o: usize) -> usize {
    u32::from_le_bytes(b[o..o + 4].try_into().unwrap()) as usize
}
fn u64_at(b: &[u8], o: usize) -> usize {
    u64::from_le_bytes(b[o..o + 8].try_into().unwrap()) as usize
}

/// Minimal ELF64 section lookup by name.
pub fn elf_section(elf: &[u8], name: &str) -> Option<Vec<u8>> {
    let shoff = u64_at(elf, 0x28);
    let shentsize = u16_at(elf, 0x3a);
    let shnum = u16_at(elf, 0x3c);
    let shstrndx = u16_at(elf, 0x3e);
    let sh = |i: usize| shoff + i * shentsize;
    let stroff = u64_at(elf, sh(shstrndx) + 0x18);
    for i in 0..shnum {
        let h = sh(i);
        let n = stroff + u32_at(elf, h);
        let end = elf[n..].iter().position(|&c| c == 0)? + n;
        if &elf[n..end] == name.as_bytes() {
            let (off, size) = (u64_at(elf, h + 0x18), u64_at(elf, h + 0x20));
            return Some(elf[off..off + size].to_vec());
        }
    }
    None
}

pub struct Run {
    pub cpu: Cpu,
    pub ram: FlatRam,
    pub trap: Trap,
}

/// Run `src` at EL`el` (MMU off) until a host trap; panics after `limit` instructions.
pub fn run_at(src: &str, el: u8) -> Run {
    let code = assemble(src);
    let mut ram = FlatRam::new(0, RAM);
    ram.load(LOAD, &code).unwrap();
    let mut cpu = Cpu::new();
    cpu.brk_to_host = true;
    cpu.el = el;
    cpu.pc = LOAD;
    cpu.sp = [0x40_0000, 0x50_0000, 0x60_0000, 0x70_0000];
    cpu.sys.cpacr_el1 = 3 << 20;
    let limit = 1_000_000;
    for _ in 0..limit {
        match cpu.step(&mut ram) {
            Ok(()) | Err(Trap::Exception(..)) | Err(Trap::Interrupt(..)) => {}
            Err(t) => return Run { cpu, ram, trap: t },
        }
    }
    panic!("no host trap after {limit} instructions, pc={:#x}", cpu.pc);
}

pub fn run(src: &str) -> Run {
    let r = run_at(src, 1);
    assert_eq!(
        r.trap,
        Trap::Brk(0),
        "stopped with {:?} at pc {:#x}",
        r.trap,
        r.cpu.pc
    );
    r
}

impl Run {
    pub fn x(&self, n: usize) -> u64 {
        self.cpu.x[n]
    }
    pub fn mem64(&mut self, a: u64) -> u64 {
        self.ram.read64(a).unwrap()
    }
}
