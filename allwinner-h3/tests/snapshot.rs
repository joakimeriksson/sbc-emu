//! A restored machine is the machine that was saved: both go on to the same state, byte for
//! byte (cores, devices, RAM), whatever the guest was doing when the snapshot was taken.
use allwinner_h3::{CoreState, Machine, RAM_BASE};
use emu_core::{disk::{Disk, MemDisk}, Stop};

/// Run as a host would: idle time is skipped.
fn run(m: &mut Machine, insns: u64) {
    let end = m.instret() + insns;
    while m.instret() < end {
        match m.run((end - m.instret()).min(1_000_000)) {
            Stop::Budget => {}
            Stop::Idle(t) => m.idle_until(t),
            other => panic!("{other:?}"),
        }
    }
}

fn first_difference(a: &[u8], b: &[u8]) -> Option<usize> {
    if a == b { return None; }
    Some(a.iter().zip(b).position(|(x, y)| x != y).unwrap_or(a.len().min(b.len())))
}

/// Save `m`, let it go on for `insns`, and have `fresh` do the same from the snapshot.
fn check(m: &mut Machine, mut fresh: Machine, insns: u64, what: &str) -> Machine {
    let file = m.save().unwrap();
    let saved = m.state();
    fresh.restore(&file).unwrap();
    assert_eq!(first_difference(&saved, &fresh.state()), None, "{what}: restored state");
    run(m, insns);
    run(&mut fresh, insns);
    assert_eq!(first_difference(&m.state(), &fresh.state()), None, "{what}: state {insns} instructions later");
    assert_eq!(m.instret(), fresh.instret());
    fresh
}

/// Each core counts in memory with exclusives, sleeps, and takes the interrupts of its timer;
/// one in 32 counts is printed. Enough to have state in the cores, the GIC, the timers and the
/// UART.
fn bare(cpus: usize) -> Machine {
    #[rustfmt::skip]
    let code = [
        0xe300_4000, // 00 movw r4, #0x0
        0xe344_4040, // 04 movt r4, #0x4040
        0xe308_5000, // 08 movw r5, #0x8000
        0xe340_51c2, // 0c movt r5, #0x1c2
        0xe301_a000, // 10 movw r10, #0x1000
        0xe340_a1c8, // 14 movt r10, #0x1c8
        0xe302_b000, // 18 movw r11, #0x2000
        0xe340_b1c8, // 1c movt r11, #0x1c8
        0xe3a0_0001, // 20 mov r0, #1
        0xe58a_0000, // 24 str r0, [r10]
        0xe3a0_10f0, // 28 mov r1, #240
        0xe58b_1004, // 2c str r1, [r11, #0x4]
        0xe58b_0000, // 30 str r0, [r11]
        0xe3a0_1101, // 34 mov r1, #1073741824
        0xe58a_1100, // 38 str r1, [r10, #0x100]
        0xe300_0000, // 3c movw r0, #0x0
        0xe344_0021, // 40 movt r0, #0x4021
        0xee0c_0f10, // 44 mcr p15, #0x0, r0, c12, c0, #0x0
        0xe3a0_6b01, // 48 mov r6, #1024
        0xee0e_6f12, // 4c mcr p15, #0x0, r6, c14, c2, #0x0
        0xe3a0_7001, // 50 mov r7, #1
        0xee0e_7f32, // 54 mcr p15, #0x0, r7, c14, c2, #0x1
        0xe3a0_8000, // 58 mov r8, #0
        0xf108_0080, // 5c cpsie i
        0xe194_0f9f, // 60 ldrex r0, [r4]
        0xe280_0001, // 64 add r0, r0, #1
        0xe184_1f90, // 68 strex r1, r0, [r4]
        0xe351_0000, // 6c cmp r1, #0
        0x1aff_fffa, // 70 bne 0x60 <.text+0x60>
        0xe310_001f, // 74 tst r0, #31
        0x0585_0000, // 78 streq r0, [r5]
        0xee00_0a10, // 7c vmov s0, r0
        0xee70_0a00, // 80 vadd.f32 s1, s0, s0
        0xe310_0007, // 84 tst r0, #7
        0x1aff_fff4, // 88 bne 0x60 <.text+0x60>
        0xe320_f003, // 8c wfi
        0xeaff_fff2, // 90 b 0x60 <.text+0x60>
    ];
    #[rustfmt::skip]
    let irq = [
        0xe59b_900c, // 94 ldr r9, [r11, #0xc]
        0xee0e_6f12, // 98 mcr p15, #0x0, r6, c14, c2, #0x0
        0xe58b_9010, // 9c str r9, [r11, #0x10]
        0xe288_8001, // a0 add r8, r8, #1
        0xe25e_f004, // a4 subs pc, lr, #4
    ];
    let mut m = Machine::with_ram_size(8 << 20);
    m.set_jit(false);
    let mut place = |at: usize, code: &[u32]| for (k, w) in code.iter().enumerate() { m.soc.ram.mem[at + 4 * k..][..4].copy_from_slice(&w.to_le_bytes()); };
    place(0x20_0000, &code);
    place(0x21_0018, &irq);
    for k in 0..cpus {
        m.state[k] = CoreState::Run;
        let c = &mut m.cpus[k];
        c.r[15] = RAM_BASE as u32 + 0x20_0000;
        c.cpacr = 0xf << 20;
        c.vfp.fpexc = 1 << 30;
    }
    m
}

fn word(m: &Machine, at: usize) -> u32 { u32::from_le_bytes(m.soc.ram.mem[at..at + 4].try_into().unwrap()) }

/// A machine nothing has been set up on: all it has after a restore is the snapshot's.
fn blank() -> Machine {
    { let mut m = Machine::with_ram_size(8 << 20); m.set_jit(false); m }
}

#[test]
fn bare_metal() {
    for cpus in [1, 2, 4] {
        let mut m = bare(cpus);
        for (k, insns) in [1, 7, 333, 20_001, 100_000].into_iter().enumerate() {
            run(&mut m, insns);
            let fresh = check(&mut m, blank(), 50_000 + k as u64, &format!("{cpus} cores, point {k}"));
            // a snapshot of a restored machine is as good
            m = fresh;
        }
        assert!(word(&m, 0x40_0000) > 300);
        assert!(!m.soc.uart.out.is_empty());
        // close together: in the interrupt handler, between the exclusives, asleep
        for k in 0..100 {
            run(&mut m, 13);
            let fresh = check(&mut m, blank(), 1_500, &format!("{cpus} cores, close point {k}"));
            if k % 10 == 0 { m = fresh; }
        }
        // the restored machines went on taking interrupts
        assert!(m.irqs > 20 && m.cpus.iter().take(cpus).all(|c| c.r[8] > 5), "{} interrupts", m.irqs);
    }
}

/// With the JIT: the restored machine starts without compiled code, and counts on.
#[test]
fn bare_metal_jit() {
    let mut m = bare(2);
    m.set_jit(true);
    run(&mut m, 300_000);
    let mut fresh = bare(2);
    fresh.set_jit(true);
    // compiled code of something else, which must not survive
    let add = 0x20_0000 + 4 * 25;
    assert_eq!(word(&fresh, add), 0xe280_0001);
    fresh.soc.ram.mem[add..add + 4].copy_from_slice(&0xe280_0005u32.to_le_bytes()); // add r0, r0, #5
    run(&mut fresh, 300_000);
    fresh.restore(&m.save().unwrap()).unwrap();
    assert_eq!(first_difference(&m.state(), &fresh.state()), None);
    let (before, insns) = (word(&fresh, 0x40_0000), fresh.instret());
    run(&mut fresh, 200_000);
    // one increment per time round (about eleven instructions), not five
    let rounds = (fresh.instret() - insns) / 11;
    let n = (word(&fresh, 0x40_0000) - before) as u64;
    assert!(n > rounds / 2 && n < rounds * 2, "{n} increments in {rounds} rounds");
}

/// Translations the machine restored into has cached do not survive: the page tables are the
/// snapshot's.
#[test]
fn another_mapping() {
    for jit in [false, true] {
        // a loop storing r1 at virtual 0x5000_0000, mapped to `pa`
        let make = |pa: u32| {
            let mut m = Machine::with_ram_size(512 << 20);
            m.set_jit(jit);
            let mut place = |at: u32, w: u32| m.soc.ram.mem[(at - RAM_BASE as u32) as usize..][..4].copy_from_slice(&w.to_le_bytes());
            for mb in 0..0x1000u32 { place(0x4030_0000 + 4 * mb, mb << 20 | 0xc02); } // sections, full access
            place(0x4030_0000 + 4 * 0x500, pa | 0xc02);
            for (k, w) in [0xe281_1001u32, 0xe582_1000, 0xe592_3000, 0xeaff_fffb].into_iter().enumerate() { place(0x4020_0000 + 4 * k as u32, w); }
            let c = &mut m.cpus[0];
            (c.r[15], c.r[2], c.ttbr0, c.dacr) = (0x4020_0000, 0x5000_0000, 0x4030_0000, u32::MAX);
            c.sctlr |= 1;
            m
        };
        let (mut m, mut other) = (make(0x5010_0000), make(0x5020_0000));
        run(&mut m, 100_000);
        run(&mut other, 100_000);
        let at = |m: &Machine, pa: u32| word(m, (pa - RAM_BASE as u32) as usize);
        assert!(at(&m, 0x5010_0000) > 100 && at(&other, 0x5020_0000) > 100);
        other.restore(&m.save().unwrap()).unwrap();
        let before = at(&other, 0x5010_0000);
        run(&mut other, 100_000);
        assert!(at(&other, 0x5010_0000) > before + 100, "jit {jit}");
        assert_eq!(at(&other, 0x5020_0000), 0, "jit {jit}");
    }
}

#[test]
fn another_machine_is_refused() {
    let file = bare(2).save().unwrap();
    let no = |mut m: Machine, what: &str| {
        let e = m.restore(&file).unwrap_err();
        assert!(e.contains(what), "{e}");
    };
    no(Machine::with_ram_size(256 << 20), "--ram-mb");
    let mut with = bare(2);
    with.connect_network(false, false);
    no(with, "a network");
    let mut with = bare(2);
    with.soc.mmc.card.disk = Some(Box::new(MemDisk::new(64)));
    no(with, "SD card");
    for cut in [0, 8, 20, file.len() / 2, file.len() - 1] { assert!(bare(2).restore(&file[..cut]).is_err(), "cut at {cut}"); }
}

/// A card as the emulator's copy-on-write image: what was written is kept apart.
struct Cow(MemDisk, emu_core::disk::Overlay);
impl Disk for Cow {
    fn sectors(&self) -> u64 { self.0.sectors() }
    fn read(&mut self, lba: u64, out: &mut [u8]) { self.0.read(lba, out); self.1.apply(lba, out); }
    fn write(&mut self, lba: u64, data: &[u8]) { let n = self.sectors(); self.1.store(lba, data, n); }
    fn overlay(&self) -> Vec<(u64, Vec<u8>)> { self.1.sorted() }
    fn restore_overlay(&mut self, sectors: Vec<(u64, Vec<u8>)>) { let n = self.sectors(); self.1.replace(sectors, n); }
}

/// The same image opened to write through (`--sd-persist`), as `CowFile` does it.
struct Through(MemDisk);
impl Disk for Through {
    fn sectors(&self) -> u64 { self.0.sectors() }
    fn read(&mut self, lba: u64, out: &mut [u8]) { self.0.read(lba, out) }
    fn write(&mut self, lba: u64, data: &[u8]) { self.0.write(lba, data) }
    fn restore_overlay(&mut self, _: Vec<(u64, Vec<u8>)>) {}
    fn snapshots_disk(&self) -> bool { false }
}

/// A disk that writes through cannot be brought back: no snapshot, rather than a wrong one. Nor is
/// one restored onto such a disk: the guest would go on with caches of sectors the image does not
/// have, and write them into it.
#[test]
fn a_persistent_card_is_refused() {
    let with = |card: Box<dyn Disk>| { let mut m = bare(1); m.soc.mmc.card.disk = Some(card); m };
    assert!(with(Box::new(Through(MemDisk::new(64)))).save().unwrap_err().contains("--sd-persist"));
    let mut m = with(Box::new(Cow(MemDisk::new(64), Default::default())));
    for written in [false, true] {
        if written { m.soc.mmc.card.disk.as_mut().unwrap().write(3, &[7; 512]); }
        let file = m.save().unwrap();
        let e = with(Box::new(Through(MemDisk::new(64)))).restore(&file).unwrap_err();
        assert!(e.contains("--sd-persist"), "{e}");
    }
}

/// What the guest wrote to the card comes back, and nothing else.
#[test]
fn written_sectors() {
    let make = || { let mut m = bare(1); m.soc.mmc.card.disk = Some(Box::new(Cow(MemDisk::new(64), Default::default()))); m };
    let (mut m, mut other) = (make(), make());
    m.soc.mmc.card.disk.as_mut().unwrap().write(10, &[5; 512]);
    other.soc.mmc.card.disk.as_mut().unwrap().write(40, &[9; 512]);
    other.restore(&m.save().unwrap()).unwrap();
    let mut all = vec![0; 64 * 512];
    other.soc.mmc.card.disk.as_mut().unwrap().read(0, &mut all);
    for (lba, s) in all.chunks(512).enumerate() { assert!(s.iter().all(|&b| b == if lba == 10 { 5 } else { 0 }), "sector {lba}"); }
}

/// Linux, saved at points all through its boot and at the shell. Needs the guest of
/// `scripts/orangepi-one-fixture.sh` (`CACHE`, default `cache/`); skipped without it.
#[test]
fn linux_boot() {
    let dir = std::env::var("CACHE").unwrap_or(concat!(env!("CARGO_MANIFEST_DIR"), "/../cache").into()) + "/orangepi-one";
    let read = |f: &str| std::fs::read(format!("{dir}/{f}"));
    let (Ok(kernel), Ok(dtb), Ok(initrd)) = (read("zImage"), read("sun8i-h3-orangepi-one.dtb"), read("initramfs.cpio.gz")) else {
        eprintln!("no guest in {dir}: skipped");
        return;
    };
    let make = || {
        let mut m = Machine::with_ram_size(512 << 20);
        m.set_jit(false);
        m.connect_network(false, false);
        m.soc.mmc.card.disk = Some(Box::new(MemDisk::new(65536)));
        m
    };
    let mut m = make();
    allwinner_h3::boot::boot_linux(&mut m, &kernel, &dtb, Some(&initrd), allwinner_h3::boot::DEFAULT_CMDLINE).unwrap();
    // every 50 M instructions, through the boot (about 1.1 G) and some time at the shell
    for point in 0..30u64 {
        run(&mut m, 50_000_000 + 1237 * point);
        check(&mut m, make(), 5_000_000, &format!("point {point}"));
    }
    let console = String::from_utf8_lossy(&m.soc.uart.out).into_owned();
    assert!(console.contains("orangepi-one#"), "{}", &console[console.len().saturating_sub(300)..]);
    eprintln!("30 points");
}
