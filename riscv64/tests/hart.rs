//! Focused hart tests: privileged behaviour Linux and OpenSBI depend on.
use riscv64::{irq, mmu::pte, mstatus, prv, Access, Bus, Hart, Identity};

const BASE: u64 = 0x8000_0000;

struct Mem {
    ram: Vec<u8>,
    time: u64,
}

impl Bus for Mem {
    fn ram_base(&self) -> u64 {
        BASE
    }
    fn ram(&mut self) -> &mut [u8] {
        &mut self.ram
    }
    fn read(&mut self, _: u64, _: u8) -> Result<u64, ()> {
        Err(())
    }
    fn write(&mut self, _: u64, _: u8, _: u64) -> Result<(), ()> {
        Err(())
    }
    fn mtime(&mut self) -> u64 {
        self.time
    }
}

fn mem() -> Mem {
    Mem { ram: vec![0; 1 << 20], time: 0 }
}

fn put32(m: &mut Mem, a: u64, v: u32) {
    let o = (a - BASE) as usize;
    m.ram[o..o + 4].copy_from_slice(&v.to_le_bytes());
}

fn put64(m: &mut Mem, a: u64, v: u64) {
    let o = (a - BASE) as usize;
    m.ram[o..o + 8].copy_from_slice(&v.to_le_bytes());
}

#[test]
fn csrrs_on_mip_does_not_latch_the_external_seip_line() {
    let mut m = mem();
    let mut h = Hart::new(Identity::sifive_u74(1));
    h.pc = BASE;
    // csrs mip, t0 (t0 = STIP), as OpenSBI's timer handler does
    put32(&mut m, BASE, 0x3442a073);
    h.x[5] = irq::STIP;
    h.irq_lines = irq::SEIP;
    h.run(&mut m, 1);
    assert_eq!(h.mip_sw, irq::STIP);
    h.irq_lines = 0;
    assert_eq!(h.mip(), irq::STIP);
}

#[test]
fn wfi_ends_on_a_pending_interrupt_even_when_globally_masked() {
    let mut m = mem();
    let mut h = Hart::new(Identity::sifive_u74(1));
    h.pc = BASE;
    put32(&mut m, BASE, 0x1050_0073); // wfi
    put32(&mut m, BASE + 4, 0x0000_0013); // nop
    h.mie = irq::MSIP;
    h.run(&mut m, 10);
    assert!(h.wfi);
    assert_eq!(h.pc, BASE + 4);
    h.irq_lines = irq::MSIP; // mstatus.MIE is 0: resume without trapping
    h.run(&mut m, 1);
    assert!(!h.wfi);
    assert_eq!(h.pc, BASE + 8);
    assert_eq!(h.traps, 0);
}

#[test]
fn delegated_supervisor_timer_interrupt_traps_to_stvec() {
    let mut m = mem();
    let mut h = Hart::new(Identity::sifive_u74(1));
    h.prv = prv::S;
    h.pc = BASE;
    h.stvec = BASE + 0x100;
    h.mideleg = irq::STIP | irq::SSIP | irq::SEIP;
    h.mie = irq::STIP;
    h.mstatus |= mstatus::SIE;
    h.mip_sw = irq::STIP;
    put32(&mut m, BASE + 0x100, 0x0000_0013); // nop at stvec
    h.run(&mut m, 1);
    assert_eq!(h.scause, (1 << 63) | 5);
    assert_eq!(h.sepc, BASE);
    assert_eq!(h.prv, prv::S);
    assert_eq!(h.mstatus & mstatus::SIE, 0);
    assert_ne!(h.mstatus & mstatus::SPIE, 0);
}

#[test]
fn sv39_gigapage_and_4k_page_translate_set_a_d_and_fault() {
    let mut m = mem();
    let root = BASE + 0x1000;
    let l1 = BASE + 0x2000;
    let l0 = BASE + 0x3000;
    // VA 0xffffffc0_00000000 (VPN2 0x100) -> gigapage PA 0x80000000, RWX kernel
    put64(&mut m, root + 8 * 0x100, ((BASE >> 12) << 10) | pte::V | pte::R | pte::W | pte::X | pte::A | pte::D);
    // VA 0x1000: root[0] -> l1[0] -> l0[1] -> PA BASE+0x5000, RW user
    put64(&mut m, root, ((l1 >> 12) << 10) | pte::V);
    put64(&mut m, l1, ((l0 >> 12) << 10) | pte::V);
    put64(&mut m, l0 + 8, (((BASE + 0x5000) >> 12) << 10) | pte::V | pte::R | pte::W | pte::U);
    let mut h = Hart::new(Identity::sifive_u74(1));
    h.prv = prv::S;
    h.satp = (8 << 60) | (root >> 12);
    assert_eq!(h.translate(&mut m, 0xffff_ffc0_0012_3456, Access::Load), Ok(BASE + 0x12_3456));
    // non-canonical
    assert_eq!(h.translate(&mut m, 0x0000_0040_0000_0000, Access::Load), Err(13));
    // S may not touch U pages without SUM
    assert_eq!(h.translate(&mut m, 0x1010, Access::Load), Err(13));
    h.mstatus |= mstatus::SUM;
    assert_eq!(h.translate(&mut m, 0x1010, Access::Store), Ok(BASE + 0x5010));
    let o = (l0 + 8 - BASE) as usize;
    let leaf = u64::from_le_bytes(m.ram[o..o + 8].try_into().unwrap());
    assert_eq!(leaf & (pte::A | pte::D), pte::A | pte::D);
    h.prv = prv::U;
    assert_eq!(h.translate(&mut m, 0x1010, Access::Fetch), Err(12));
    assert_eq!(h.translate(&mut m, 0x2000, Access::Load), Err(13));
}

#[test]
fn satp_accepts_only_bare_and_sv39_and_has_no_asid() {
    let mut m = mem();
    let mut h = Hart::new(Identity::sifive_u74(1));
    assert!(h.csr_write(0x180, (9 << 60) | 5)); // Sv48: ignored
    assert_eq!(h.csr_read(&mut m, 0x180), Some(0));
    assert!(h.csr_write(0x180, (8 << 60) | (0xffff << 44) | 5));
    assert_eq!(h.csr_read(&mut m, 0x180), Some((8 << 60) | 5));
}

#[test]
fn time_csr_reads_the_platform_timer_when_counteren_allows() {
    let mut m = mem();
    m.time = 1234;
    let mut h = Hart::new(Identity::sifive_u74(1));
    assert_eq!(h.csr_read(&mut m, 0xc01), Some(1234));
    h.prv = prv::S;
    assert_eq!(h.csr_read(&mut m, 0xc01), None);
    h.mcounteren = 2;
    assert_eq!(h.csr_read(&mut m, 0xc01), Some(1234));
    // priv 1.11 hart: no menvcfg, no stimecmp (OpenSBI's probes rely on the trap)
    h.prv = prv::M;
    assert_eq!(h.csr_read(&mut m, 0x30a), None);
    assert_eq!(h.csr_read(&mut m, 0x14d), None);
}

/// A hart restores; in the reserved mode 2, or with it in mstatus.MPP for an mret to enter, it
/// is refused (only a hand-made snapshot holds that).
#[test]
fn a_snapshot_in_the_reserved_mode_is_refused() {
    use emu_core::snap::{Snap, R, W};
    let load = |h: &Hart| {
        let mut w = W::default();
        h.save(&mut w);
        Hart::new(Identity::sifive_u74(1)).load(&mut R::new(&w.0))
    };
    let mut h = Hart::new(Identity::sifive_u74(1));
    h.prv = prv::S;
    h.mstatus = (h.mstatus & !mstatus::MPP) | prv::S << 11;
    load(&h).unwrap();
    h.mstatus = (h.mstatus & !mstatus::MPP) | 2 << 11;
    assert!(load(&h).is_err());
    h.mstatus &= !mstatus::MPP;
    h.prv = 2;
    assert!(load(&h).is_err());
}
