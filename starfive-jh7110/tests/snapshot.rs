//! A restored machine is the machine that was saved: both go on to the same state, byte for
//! byte (harts, devices, RAM), whatever the guest was doing when the snapshot was taken.
use emu_core::{disk::{Disk, MemDisk}, Stop};
use starfive_jh7110::{HartState, Machine, RAM_BASE};

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

const CODE: usize = 0x20_0000;
const COUNTER: usize = 0x40_0000;

/// Each hart counts in memory with LR/SC, sleeps, and takes the interrupts of its timer; one in 32
/// counts is printed. Enough to have state in the harts, the CLINT and the UART.
fn bare(harts: usize) -> Machine {
    #[rustfmt::skip]
    let code: [u32; 38] = [
        0xf140_2473, // 0000: csrr s0,mhartid
        0x4040_0937, // 0004: lui s2,0x40400
        0x1000_09b7, // 0008: lui s3,0x10000
        0x0200_4a37, // 000c: lui s4,0x2004
        0x0034_1293, // 0010: slli t0,s0,0x3
        0x005a_0a33, // 0014: add s4,s4,t0
        0x0200_cab7, // 0018: lui s5,0x200c
        0xff8a_8a9b, // 001c: addiw s5,s5,-8
        0x0000_0297, // 0020: auipc t0,0x0
        0x0642_8293, // 0024: addi t0,t0,100
        0x3052_9073, // 0028: csrw mtvec,t0
        0x0000_0493, // 002c: li s1,0
        0x000a_b283, // 0030: ld t0,0(s5)
        0x0282_8293, // 0034: addi t0,t0,40
        0x005a_3023, // 0038: sd t0,0(s4)
        0x0800_0293, // 003c: li t0,128
        0x3042_9073, // 0040: csrw mie,t0
        0x0000_22b7, // 0044: lui t0,0x2
        0x3002_a073, // 0048: csrs mstatus,t0
        0x3004_6073, // 004c: csrsi mstatus,8
        0x1009_32af, // 0050: lr.d t0,(s2)
        0x0012_8293, // 0054: addi t0,t0,1
        0x1859_332f, // 0058: sc.d t1,t0,(s2)
        0xfe03_1ae3, // 005c: bnez t1,40200050
        0x01f2_f393, // 0060: andi t2,t0,31
        0x0003_9463, // 0064: bnez t2,4020006c
        0x0059_8023, // 0068: sb t0,0(s3)
        0xd222_f553, // 006c: fcvt.d.l fa0,t0
        0x02a5_75d3, // 0070: fadd.d fa1,fa0,fa0
        0x0072_f393, // 0074: andi t2,t0,7
        0xfc03_9ce3, // 0078: bnez t2,40200050
        0x1050_0073, // 007c: wfi
        0xfd1f_f06f, // 0080: j 40200050
        0x000a_be03, // 0084: ld t3,0(s5)
        0x028e_0e13, // 0088: addi t3,t3,40
        0x01ca_3023, // 008c: sd t3,0(s4)
        0x0014_8493, // 0090: addi s1,s1,1
        0x3020_0073, // 0094: mret
    ];
    let mut m = Machine::new(8 << 20);
    m.set_jit(false);
    let bytes: Vec<u8> = code.iter().flat_map(|w| w.to_le_bytes()).collect();
    m.load(RAM_BASE + CODE as u64, &bytes).unwrap();
    // the U74s (hart 0 is the S7 monitor core)
    for k in 1..=harts {
        m.state[k] = HartState::Run;
        m.harts[k].pc = RAM_BASE + CODE as u64;
    }
    m
}

fn word(m: &Machine, at: usize) -> u64 { u64::from_le_bytes(m.soc.ram.mem[at..at + 8].try_into().unwrap()) }

/// A machine nothing has been set up on: all it has after a restore is the snapshot's.
fn blank() -> Machine {
    let mut m = Machine::new(8 << 20);
    m.set_jit(false);
    m
}

#[test]
fn bare_metal() {
    for harts in [1, 2, 4] {
        let mut m = bare(harts);
        for (k, insns) in [1, 7, 333, 20_001, 100_000].into_iter().enumerate() {
            run(&mut m, insns);
            let fresh = check(&mut m, blank(), 50_000 + k as u64, &format!("{harts} harts, point {k}"));
            // a snapshot of a restored machine is as good
            m = fresh;
        }
        assert!(word(&m, COUNTER) > 300);
        assert!(!m.soc.uart.out.is_empty());
        // close together: in the interrupt handler, between LR and SC, asleep
        for k in 0..100 {
            run(&mut m, 13);
            let fresh = check(&mut m, blank(), 1_500, &format!("{harts} harts, close point {k}"));
            if k % 10 == 0 { m = fresh; }
        }
        // the restored machines went on taking interrupts
        let irqs: Vec<u64> = (1..=harts).map(|k| m.harts[k].x[9]).collect();
        assert!(irqs.iter().all(|&n| n > 5), "interrupts {irqs:?}");
    }
}

/// With the JIT (where there is one): the restored machine starts without compiled code, and
/// counts on.
#[test]
fn bare_metal_jit() {
    let mut m = bare(2);
    m.set_jit(true);
    run(&mut m, 300_000);
    let mut fresh = bare(2);
    fresh.set_jit(true);
    // compiled code of something else, which must not survive: addi t0, t0, 5
    let add = CODE + 0x54;
    assert_eq!(word(&fresh, add) as u32, 0x0012_8293);
    fresh.soc.ram.mem[add..add + 4].copy_from_slice(&0x0052_8293u32.to_le_bytes());
    run(&mut fresh, 300_000);
    fresh.restore(&m.save().unwrap()).unwrap();
    assert_eq!(first_difference(&m.state(), &fresh.state()), None);
    let (before, insns) = (word(&fresh, COUNTER), fresh.instret());
    run(&mut fresh, 200_000);
    // one increment per round of the loop (about nine instructions), not five
    let rounds = (fresh.instret() - insns) / 9;
    let n = word(&fresh, COUNTER) - before;
    assert!(n > rounds / 2 && n < rounds * 2, "{n} increments in {rounds} rounds");
}

/// Translations the machine restored into has cached do not survive: the page tables are the
/// snapshot's.
#[test]
fn another_mapping() {
    for jit in [false, true] {
        // in S mode with Sv39: a loop storing a1 at virtual 0x8000_0000, a 2 MiB page at `pa`; the
        // rest of RAM is mapped one to one
        let make = |pa: u64| {
            let mut m = Machine::new(64 << 20);
            m.set_jit(jit);
            let table = 0x4030_0000u64;
            let put = |m: &mut Machine, at: u64, v: u64| { let o = (at - RAM_BASE) as usize; m.soc.ram.mem[o..o + 8].copy_from_slice(&v.to_le_bytes()); };
            // root: 1 GiB at 0x4000_0000 one to one (VRWXAD), 0x8000_0000 to a level-1 table
            put(&mut m, table + 8, (0x4000_0000 >> 12) << 10 | 0xcf);
            put(&mut m, table + 16, ((table + 0x1000) >> 12) << 10 | 1);
            put(&mut m, table + 0x1000, (pa >> 12) << 10 | 0xcf);
            // addi a1, a1, 1; sd a1, 0(a2); ld a3, 0(a2); j .-12
            for (k, w) in [0x0015_8593u32, 0x00b6_3023, 0x0006_3683, 0xff5f_f06f].into_iter().enumerate() {
                let o = 0x20_0000 + 4 * k;
                m.soc.ram.mem[o..o + 4].copy_from_slice(&w.to_le_bytes());
            }
            m.state[1] = HartState::Run;
            let h = &mut m.harts[1];
            (h.pc, h.x[12], h.prv) = (0x4020_0000, 0x8000_0000, 1);
            h.satp = 8 << 60 | table >> 12;
            m
        };
        let (mut m, mut other) = (make(0x4060_0000), make(0x40a0_0000));
        run(&mut m, 100_000);
        run(&mut other, 100_000);
        let at = |m: &Machine, pa: u64| word(m, (pa - RAM_BASE) as usize);
        assert!(at(&m, 0x4060_0000) > 100 && at(&other, 0x40a0_0000) > 100, "jit {jit}");
        other.restore(&m.save().unwrap()).unwrap();
        let (before, theirs) = (at(&other, 0x4060_0000), at(&other, 0x40a0_0000));
        run(&mut other, 100_000);
        assert!(at(&other, 0x4060_0000) > before + 100, "jit {jit}");
        assert_eq!(at(&other, 0x40a0_0000), theirs, "jit {jit}");
        assert_eq!(theirs, 0, "jit {jit}: the snapshot's RAM");
    }
}

#[test]
fn another_machine_is_refused() {
    let file = bare(2).save().unwrap();
    let no = |mut m: Machine, what: &str| {
        let e = m.restore(&file).unwrap_err();
        assert!(e.contains(what), "{e}");
    };
    no(Machine::new(16 << 20), "--ram-mb");
    let mut with = bare(2);
    with.connect_network(false, false);
    no(with, "a network");
    let mut with = bare(2);
    with.soc.sdio1.insert(Some(Box::new(MemDisk::new(64))));
    no(with, "SD card");
    let mut with = bare(2);
    with.soc.attach_nvme(Box::new(MemDisk::new(64)));
    no(with, "NVMe");
    let mut with = bare(2);
    with.soc.qspi.flash = Some(starfive_jh7110::qspi::SpiNor::new(&[0xff; 16 << 20]).unwrap());
    no(with, "SPI flash");
    assert!(bare(2).restore(b"SBCSNAP1 the Pi 4's").unwrap_err().contains("Raspberry Pi 4"));
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

/// A card that writes through cannot be brought back: no snapshot, rather than a wrong one. Nor is
/// one restored onto such a card.
#[test]
fn a_persistent_card_is_refused() {
    let with = |card: Box<dyn Disk>| { let mut m = bare(1); m.soc.sdio1.insert(Some(card)); m };
    assert!(with(Box::new(Through(MemDisk::new(64)))).save().unwrap_err().contains("--sd-persist"));
    let mut m = with(Box::new(Cow(MemDisk::new(64), Default::default())));
    for written in [false, true] {
        if written { m.soc.sdio1.card.disk_mut().unwrap().write(3, &[7; 512]); }
        let file = m.save().unwrap();
        let e = with(Box::new(Through(MemDisk::new(64)))).restore(&file).unwrap_err();
        assert!(e.contains("--sd-persist"), "{e}");
    }
}

fn nvme(m: &mut Machine) -> &mut (dyn Disk + Send) { m.soc.pcie[1].nvme.as_mut().unwrap().disk.as_mut().unwrap().as_mut() }

/// What the guest wrote to the card, the SSD and the flash comes back, and nothing else.
#[test]
fn written_sectors() {
    let make = || {
        let mut m = bare(1);
        m.soc.sdio1.insert(Some(Box::new(Cow(MemDisk::new(64), Default::default()))));
        m.soc.attach_nvme(Box::new(Cow(MemDisk::new(64), Default::default())));
        m.soc.qspi.flash = Some(starfive_jh7110::qspi::SpiNor::new(&[0xff; 16 << 20]).unwrap());
        m
    };
    let (mut m, mut other) = (make(), make());
    m.soc.sdio1.card.disk_mut().unwrap().write(10, &[5; 512]);
    nvme(&mut m).write(20, &[6; 512]);
    m.soc.qspi.flash.as_mut().unwrap().data[12345] = 0x42;
    other.soc.sdio1.card.disk_mut().unwrap().write(40, &[9; 512]);
    nvme(&mut other).write(41, &[9; 512]);
    other.soc.qspi.flash.as_mut().unwrap().data[999] = 0;
    other.restore(&m.save().unwrap()).unwrap();
    let mut all = vec![0; 64 * 512];
    other.soc.sdio1.card.disk_mut().unwrap().read(0, &mut all);
    for (lba, s) in all.chunks(512).enumerate() { assert!(s.iter().all(|&b| b == if lba == 10 { 5 } else { 0 }), "SD sector {lba}"); }
    nvme(&mut other).read(0, &mut all);
    for (lba, s) in all.chunks(512).enumerate() { assert!(s.iter().all(|&b| b == if lba == 20 { 6 } else { 0 }), "NVMe sector {lba}"); }
    let flash = &other.soc.qspi.flash.as_ref().unwrap().data;
    assert!(flash.iter().enumerate().all(|(k, &b)| b == if k == 12345 { 0x42 } else { 0xff }));
}

/// Linux, saved at points all through its boot and at the shell, with every device the board has
/// that a snapshot holds. Needs the guest of `scripts/orangepi-rv-fixture.sh` (`CACHE`, default
/// `cache/`); skipped without it.
#[test]
fn linux_boot() {
    let dir = std::env::var("CACHE").unwrap_or(concat!(env!("CARGO_MANIFEST_DIR"), "/../cache").into()) + "/orangepi-rv";
    let read = |f: &str| std::fs::read(format!("{dir}/{f}"));
    let (Ok(firmware), Ok(kernel), Ok(dtb), Ok(initrd)) = (read("fw_dynamic.bin"), read("Image"), read("jh7110-orangepi-rv.dtb"), read("initramfs.cpio.gz")) else {
        eprintln!("no guest in {dir}: skipped");
        return;
    };
    let make = || {
        let mut m = Machine::new(1 << 30);
        m.set_jit(false);
        m.connect_network(false, false);
        m.soc.sdio1.insert(Some(Box::new(MemDisk::new(65536))));
        m.soc.attach_nvme(Box::new(MemDisk::new(65536)));
        m
    };
    let mut m = make();
    starfive_jh7110::boot::boot_linux(&mut m, &firmware, &kernel, &dtb, Some(&initrd), starfive_jh7110::boot::DEFAULT_CMDLINE).unwrap();
    // every 150 M instructions, through the boot (about 6.7 G: the Wi-Fi's firmware is loaded at
    // the end) and some time at the shell
    for point in 0..50u64 {
        run(&mut m, 150_000_000 + 1237 * point);
        check(&mut m, make(), 5_000_000, &format!("point {point}"));
    }
    let console = String::from_utf8_lossy(&m.soc.uart.out).into_owned();
    assert!(console.contains("orangepi-rv#"), "{}", &console[console.len().saturating_sub(300)..]);
    eprintln!("50 points");
}
