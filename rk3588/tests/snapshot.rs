//! A restored machine is the machine that was saved: both go on to the same state, byte for
//! byte (cores, devices, RAM), whatever the guest was doing when the snapshot was taken.
use emu_core::{disk::{Disk, MemDisk}, Stop};
use rk3588::{CoreState, Machine};

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
    let mut m = Machine::with_cpus(128 << 20, cpus).unwrap();
    #[rustfmt::skip]
    let code = [
        0xd538_00a3, // 00 mrs x3, MPIDR_EL1
        0xd348_3c63, // 04 ubfx x3, x3, #8, #8
        0xd2a0_0804, // 08 mov x4, #0x400000
        0xd2bf_d6a5, // 0c mov x5, #0xfeb50000
        0xd280_8006, // 10 mov x6, #0x400
        0xd2bf_cc0a, // 14 mov x10, #0xfe600000
        0x5280_0240, // 18 mov w0, #0x12
        0xb900_0140, // 1c str w0, [x10]
        0xd2bf_cd0b, // 20 mov x11, #0xfe680000
        0x8b03_456b, // 24 add x11, x11, x3, lsl #17
        0xb900_157f, // 28 str wzr, [x11, #0x14]
        0x9140_416c, // 2c add x12, x11, #0x10, lsl #12
        0x1280_0000, // 30 mov w0, #-0x1
        0xb900_8180, // 34 str w0, [x12, #0x80]
        0x52a8_0000, // 38 mov w0, #0x40000000
        0xb901_0180, // 3c str w0, [x12, #0x100]
        0xd280_1e00, // 40 mov x0, #0xf0
        0xd518_4600, // 44 msr ICC_PMR_EL1, x0
        0xd280_0020, // 48 mov x0, #0x1
        0xd518_cce0, // 4c msr ICC_IGRPEN1_EL1, x0
        0xd2a0_0420, // 50 mov x0, #0x210000
        0xd518_c000, // 54 msr VBAR_EL1, x0
        0xd51b_e206, // 58 msr CNTP_TVAL_EL0, x6
        0xd280_0027, // 5c mov x7, #0x1
        0xd51b_e227, // 60 msr CNTP_CTL_EL0, x7
        0xd503_42ff, // 64 msr DAIFClr, #0x2
        0x885f_7c80, // 68 ldxr w0, [x4]
        0x1100_0400, // 6c add w0, w0, #0x1
        0x8801_7c80, // 70 stxr w1, w0, [x4]
        0x35ff_ffa1, // 74 cbnz w1, 0x68 <.text+0x68>
        0x1200_1002, // 78 and w2, w0, #0x1f
        0x3500_0042, // 7c cbnz w2, 0x84 <.text+0x84>
        0xb900_00a0, // 80 str w0, [x5]
        0x9b00_7c08, // 84 mul x8, x0, x0
        0x9e67_0101, // 88 fmov d1, x8
        0x1e61_2822, // 8c fadd d2, d1, d1
        0x1200_0802, // 90 and w2, w0, #0x7
        0x35ff_fea2, // 94 cbnz w2, 0x68 <.text+0x68>
        0xd503_207f, // 98 wfi
        0x17ff_fff3, // 9c b 0x68 <.text+0x68>
    ];
    #[rustfmt::skip]
    let irq = [
        0xd538_cc09, // 100 mrs x9, ICC_IAR1_EL1
        0xd51b_e206, // 104 msr CNTP_TVAL_EL0, x6
        0xd518_cc29, // 108 msr ICC_EOIR1_EL1, x9
        0x9100_05ef, // 10c add x15, x15, #0x1
        0xd69f_03e0, // 110 eret
    ];
    let mut place = |at: usize, code: &[u32]| for (k, w) in code.iter().enumerate() { m.soc.ram[at + 4 * k..][..4].copy_from_slice(&w.to_le_bytes()); };
    place(0x20_0000, &code);
    place(0x21_0280, &irq);
    for k in 0..cpus {
        m.state[k] = CoreState::Run;
        let c = &mut m.cpus[k];
        c.el = 1;
        c.pc = 0x20_0000;
        c.sys.cpacr_el1 = 3 << 20;
    }
    m
}

/// A machine nothing has been set up on: all it has after a restore is the snapshot's.
fn blank(cpus: usize) -> Machine {
    Machine::with_cpus(128 << 20, cpus).unwrap()
}

#[test]
fn bare_metal() {
    for cpus in [1, 2, 8] {
        let mut m = bare(cpus);
        for (k, insns) in [1, 7, 333, 20_001, 100_000].into_iter().enumerate() {
            run(&mut m, insns);
            let fresh = check(&mut m, blank(cpus), 50_000 + k as u64, &format!("{cpus} cores, point {k}"));
            // a snapshot of a restored machine is as good
            m = fresh;
        }
        assert!(u32::from_le_bytes(m.soc.ram[0x40_0000..0x40_0004].try_into().unwrap()) > 300);
        assert!(!m.soc.uart.out.is_empty());
        // close together: in the interrupt handler, between the exclusives, asleep
        // (this board has at least 128 MiB of RAM to go through each time)
        for k in 0..if cpus == 2 { 30 } else { 0 } {
            run(&mut m, 13);
            let fresh = check(&mut m, blank(cpus), 1_500, &format!("{cpus} cores, close point {k}"));
            if k % 10 == 0 { m = fresh; }
        }
        // the restored machines went on taking interrupts
        assert!(m.irqs > 20 && m.cpus.iter().all(|c| c.x[15] > 5), "{} interrupts", m.irqs);
    }
}

/// With the JIT: the restored machine starts without compiled code, and counts on.
#[test]
fn bare_metal_jit() {
    let mut m = bare(2);
    m.enable_jit();
    run(&mut m, 300_000);
    let mut fresh = bare(2);
    fresh.enable_jit();
    // compiled code of something else, which must not survive
    fresh.soc.ram[0x20_006c..0x20_0070].copy_from_slice(&0x1100_1400u32.to_le_bytes()); // add w0, w0, #5
    run(&mut fresh, 300_000);
    fresh.restore(&m.save().unwrap()).unwrap();
    assert_eq!(first_difference(&m.state(), &fresh.state()), None);
    let count = |m: &Machine| u32::from_le_bytes(m.soc.ram[0x40_0000..0x40_0004].try_into().unwrap());
    let before = count(&fresh);
    let insns = fresh.instret();
    run(&mut fresh, 200_000);
    // one increment per time round (about eleven instructions), not five
    let rounds = (fresh.instret() - insns) / 10;
    let n = (count(&fresh) - before) as u64;
    assert!(n > rounds / 3 && n < rounds * 2, "{n} increments in {rounds} rounds");
}

/// A 32-bit program under a 64-bit kernel, storing `value` at 0x60_0000 in a hot loop.
fn bare_32_bit(value: u32) -> Machine {
    let mut m = Machine::with_cpus(128 << 20, 1).unwrap();
    // msr spsr_el1 (AArch32 user mode), elr_el1 = 0x50_0000, x2 = 0x60_0000; eret
    let el1 = [0xd280_0200u32, 0xd518_4000, 0xd2a0_0a01, 0xd518_4021, 0xd2a0_0c02, 0xd69f_03e0];
    // mov r0, #value; add r3, r3, #1; str r0, [r2]; b
    let user = [0xe3a0_0000 | value, 0xe283_3001, 0xe582_0000, 0xeaff_fffb];
    for (at, code) in [(0x40_0000, &el1[..]), (0x50_0000, &user[..])] {
        for (k, w) in code.iter().enumerate() { m.soc.ram[at + 4 * k..][..4].copy_from_slice(&w.to_le_bytes()); }
    }
    (m.cpus[0].el, m.cpus[0].pc) = (1, 0x40_0000);
    m.enable_jit();
    m
}

/// Compiled 32-bit code of the machine restored into does not survive either.
#[test]
fn a_32_bit_program() {
    let stored = |m: &Machine| u32::from_le_bytes(m.soc.ram[0x60_0000..0x60_0004].try_into().unwrap());
    let mut m = bare_32_bit(1);
    run(&mut m, 500_000);
    let file = m.save().unwrap();
    let mut other = bare_32_bit(2);
    run(&mut other, 500_000);
    assert_eq!((stored(&m), stored(&other)), (1, 2));
    other.restore(&file).unwrap();
    assert_eq!(first_difference(&m.state(), &other.state()), None);
    run(&mut other, 500_000);
    assert_eq!(stored(&other), 1);
}

/// Other threads: what a threaded machine saves, a machine taking turns restores, and back.
#[test]
fn threaded() {
    let mut m = bare(4);
    m.multi = true;
    for _ in 0..5 { let _ = m.run(1); }
    assert!(m.instret() > 1000);
    let file = m.save().unwrap();
    let mut turns = bare(4);
    turns.restore(&file).unwrap();
    assert_eq!(first_difference(&m.state(), &turns.state()), None);
    run(&mut turns, 100_000);
    let mut again = bare(4);
    again.multi = true;
    again.restore(&turns.save().unwrap()).unwrap();
    let before = again.instret();
    for _ in 0..5 { let _ = again.run(1); }
    assert!(again.instret() > before + 1000);
}

#[test]
fn another_machine_is_refused() {
    let m = bare(2);
    let file = m.save().unwrap();
    let no = |mut m: Machine, what: &str| {
        let e = m.restore(&file).unwrap_err();
        assert!(e.contains(what), "{e}");
    };
    no(bare(1), "--smp 2");
    no(Machine::with_cpus(256 << 20, 2).unwrap(), "--ram-mb");
    let mut with = bare(2);
    with.connect_network(false, false);
    no(with, "a network");
    let mut with = bare(2);
    with.attach_nvme(Box::new(MemDisk::new(64)));
    no(with, "NVMe");
    let mut with = bare(2);
    with.attach_usb_drive(Box::new(MemDisk::new(64)));
    no(with, "USB stick");
    let mut with = bare(2);
    with.soc.mmc.card.disk = Some(Box::new(MemDisk::new(64)));
    no(with, "SD card");
    let mut with = bare(2);
    with.gpu_probe = true;
    no(with, "--gpu");
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

/// What the guest wrote to the card, the SSD and the stick comes back, and nothing else.
#[test]
fn written_sectors() {
    let cow = || Cow(MemDisk::new(64), Default::default());
    let make = || {
        let mut m = bare(1);
        m.soc.mmc.card.disk = Some(Box::new(cow()));
        m.attach_nvme(Box::new(cow()));
        m.attach_usb_drive(Box::new(cow()));
        m
    };
    fn disks(m: &mut Machine) -> [&mut dyn Disk; 3] {
        [m.soc.mmc.card.disk.as_mut().unwrap().as_mut(), m.soc.pcie.nvme.as_mut().unwrap().disk.as_mut().unwrap().as_mut(), m.soc.usb.storage.as_mut().unwrap().disk.as_mut()]
    }
    let (mut m, mut other) = (make(), make());
    for (k, d) in disks(&mut m).into_iter().enumerate() { d.write(10 + k as u64, &[k as u8 + 1; 512]); }
    for d in disks(&mut other) { d.write(40, &[9; 512]); }
    other.restore(&m.save().unwrap()).unwrap();
    for (k, d) in disks(&mut other).into_iter().enumerate() {
        let mut all = vec![0; 64 * 512];
        d.read(0, &mut all);
        for (lba, s) in all.chunks(512).enumerate() {
            assert!(s.iter().all(|&b| b == if lba == 10 + k { k as u8 + 1 } else { 0 }), "disk {k}, sector {lba}");
        }
    }
}

/// Linux, saved at points all through its boot and at the shell. Needs the guest of
/// `scripts/orangepi5-fixture.sh` (`CACHE`, default `cache/`); skipped without it.
#[test]
fn linux_boot() {
    let dir = std::env::var("CACHE").unwrap_or(concat!(env!("CARGO_MANIFEST_DIR"), "/../cache").into()) + "/orangepi5";
    let (Ok(kernel), Ok(initrd)) = (std::fs::read(format!("{dir}/Image")), std::fs::read(format!("{dir}/initramfs.cpio.gz"))) else {
        eprintln!("no guest in {dir}: skipped");
        return;
    };
    let make = || {
        let mut m = Machine::with_cpus(256 << 20, 4).unwrap();
        m.enable_hdmi(rk3588::display::Mode::MODES[0]);
        m.connect_network(false, false);
        m.attach_nvme(Box::new(MemDisk::new(4096)));
        m.attach_usb_drive(Box::new(MemDisk::new(4096)));
        m.soc.mmc.card.disk = Some(Box::new(MemDisk::new(4096)));
        m
    };
    let mut m = make();
    rk3588::boot::boot_linux(&mut m, &kernel, Some(&initrd), rk3588::boot::DEFAULT_CMDLINE).unwrap();
    let mut points = 0;
    // every 50 M instructions, through the boot (about 1 G) and some time at the shell
    for point in 0..28u64 {
        run(&mut m, 50_000_000 + 1237 * point);
        check(&mut m, make(), 5_000_000, &format!("point {point}"));
        points += 1;
    }
    let console = String::from_utf8_lossy(&m.soc.uart.out).into_owned();
    assert!(console.contains("orangepi5#"), "{}", &console[console.len().saturating_sub(300)..]);
    eprintln!("{points} points");
}
