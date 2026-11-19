//! The riscv-tests ISA suites (`rv64u{i,m,a,c,f,d}-p-*`, `rv64{s,m}i-p-*`, Zba/Zbb) on one
//! bare hart. The test binaries are built by `scripts/riscv-tests.sh` (Docker) into
//! `cache/riscv-tests/isa/`; without them this test does nothing.
use riscv64::{Bus, Hart, Identity};

struct Mem {
    ram: Vec<u8>,
}

const BASE: u64 = 0x8000_0000;

impl Bus for Mem {
    fn ram_base(&self) -> u64 {
        BASE
    }
    fn ram(&mut self) -> &mut [u8] {
        &mut self.ram
    }
    fn read(&mut self, _addr: u64, _size: u8) -> Result<u64, ()> {
        Err(())
    }
    fn write(&mut self, _addr: u64, _size: u8, _v: u64) -> Result<(), ()> {
        Err(())
    }
    fn mtime(&mut self) -> u64 {
        0
    }
}

fn u16_at(b: &[u8], o: usize) -> usize {
    u16::from_le_bytes([b[o], b[o + 1]]) as usize
}
fn u32_at(b: &[u8], o: usize) -> u64 {
    u32::from_le_bytes(b[o..o + 4].try_into().unwrap()) as u64
}
fn u64_at(b: &[u8], o: usize) -> u64 {
    u64::from_le_bytes(b[o..o + 8].try_into().unwrap())
}

/// Load an ELF64 into RAM; returns (entry, address of `tohost`).
fn load(elf: &[u8], mem: &mut Mem) -> (u64, u64) {
    assert_eq!(&elf[..4], b"\x7fELF");
    let entry = u64_at(elf, 24);
    let (phoff, shoff) = (u64_at(elf, 32) as usize, u64_at(elf, 40) as usize);
    let (phentsize, phnum) = (u16_at(elf, 54), u16_at(elf, 56));
    let (shentsize, shnum) = (u16_at(elf, 58), u16_at(elf, 60));
    for k in 0..phnum {
        let p = phoff + k * phentsize;
        if u32_at(elf, p) != 1 {
            continue;
        }
        let (off, paddr, filesz) = (u64_at(elf, p + 8) as usize, u64_at(elf, p + 24), u64_at(elf, p + 32) as usize);
        let o = (paddr - BASE) as usize;
        mem.ram[o..o + filesz].copy_from_slice(&elf[off..off + filesz]);
    }
    let mut tohost = 0;
    for k in 0..shnum {
        let s = shoff + k * shentsize;
        if u32_at(elf, s + 4) != 2 {
            continue; // SHT_SYMTAB
        }
        let (off, size, link) = (u64_at(elf, s + 24) as usize, u64_at(elf, s + 32) as usize, u32_at(elf, s + 40) as usize);
        let strtab = shoff + link * shentsize;
        let stroff = u64_at(elf, strtab + 24) as usize;
        for e in (off..off + size).step_by(24) {
            let name = u32_at(elf, e) as usize + stroff;
            let end = elf[name..].iter().position(|&c| c == 0).unwrap();
            if &elf[name..name + end] == b"tohost" {
                tohost = u64_at(elf, e + 8);
            }
        }
    }
    (entry, tohost)
}

fn run(path: &std::path::Path) -> Result<(), String> {
    let elf = std::fs::read(path).unwrap();
    let mut mem = Mem { ram: vec![0; 4 << 20] };
    let (entry, tohost) = load(&elf, &mut mem);
    let mut hart = Hart::new(Identity::sifive_u74(0));
    hart.pc = entry;
    let t = (tohost - BASE) as usize;
    for _ in 0..2000 {
        hart.run(&mut mem, 1000);
        let v = u64::from_le_bytes(mem.ram[t..t + 8].try_into().unwrap());
        if v != 0 {
            return if v == 1 { Ok(()) } else { Err(format!("failed test #{} (pc {:#x})", v >> 1, hart.pc)) };
        }
    }
    Err(format!("timeout at pc {:#x}", hart.pc))
}

#[test]
fn riscv_tests_isa() {
    let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../cache/riscv-tests/isa");
    let Ok(entries) = std::fs::read_dir(&dir) else {
        eprintln!("no riscv-tests in {}: skipped", dir.display());
        return;
    };
    let mut names: Vec<_> = entries.filter_map(|e| e.ok()).map(|e| e.path())
        .filter(|p| {
            let n = p.file_name().unwrap().to_string_lossy().to_string();
            // the U74's ISA: rv64imafdc_zba_zbb, M/S/U
            ["rv64ui-", "rv64um-", "rv64ua-", "rv64uc-", "rv64uf-", "rv64ud-", "rv64si-", "rv64mi-", "rv64uzba-", "rv64uzbb-"]
                .iter().any(|s| n.starts_with(s))
                && (n.contains("-p-") || n.contains("-v-")) && !n.ends_with(".dump")
        })
        .collect();
    names.sort();
    let mut failed = Vec::new();
    for p in &names {
        if let Err(e) = run(p) {
            failed.push(format!("{}: {e}", p.file_name().unwrap().to_string_lossy()));
        }
    }
    eprintln!("riscv-tests: {} run, {} failed", names.len(), failed.len());
    assert!(failed.is_empty(), "{}", failed.join("\n"));
}
