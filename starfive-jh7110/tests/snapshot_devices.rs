//! Devices in the middle of things: random register accesses, a snapshot, a restore into a machine
//! nothing was done to, then the same random accesses on both. What they read must agree. Unlike
//! a comparison of saved states, this also sees state a snapshot leaves out.
use emu_core::disk::MemDisk;
use riscv64::Bus as _;
use starfive_jh7110::Machine;

const WINDOWS: &[(&str, u64, u64)] = &[
    ("clint", 0x0200_0000, 0x14),
    ("clint mtimecmp", 0x0200_4000, 0x28),
    ("plic priority", 0x0c00_0000, 0x200),
    ("plic pending", 0x0c00_1000, 0x14),
    ("plic enable", 0x0c00_2000, 0x280),
    ("plic context 1", 0x0c20_1000, 0x8),
    ("plic context 2", 0x0c20_2000, 0x8),
    ("uart0", 0x1000_0000, 0x20),
    ("uart1", 0x1001_0000, 0x20),
    ("i2c0", 0x1003_0000, 0x100),
    ("pwmdac", 0x100b_0000, 0x40),
    ("stgcrg", 0x1023_0000, 0x80),
    ("i2c5", 0x1205_0000, 0x100),
    ("i2s", 0x120b_0000, 0x200),
    ("temp", 0x120e_0000, 0x10),
    ("qspi", 0x1301_0000, 0x100),
    ("syscrg", 0x1302_0000, 0x310),
    ("sys_syscon", 0x1303_0000, 0x100),
    ("sysgpio", 0x1304_0000, 0x120),
    ("sec", 0x1600_0000, 0x800),
    ("trng", 0x1600_c000, 0x40),
    ("sdio0", 0x1601_0000, 0x204),
    ("sdio1", 0x1602_0000, 0x204),
    ("gmac0", 0x1603_0000, 0x1200),
    ("gmac1", 0x1604_0000, 0x1200),
    ("dmac", 0x1605_0000, 0x500),
    ("aoncrg", 0x1700_0000, 0x40),
    ("pmu", 0x1703_0000, 0x100),
    ("gpu", 0x1800_0000, 0x200),
    ("pcie0", 0x2b00_0000, 0x200),
    ("pcie1", 0x2c00_0000, 0x200),
    ("dc", 0x2940_0000, 0x2800),
    ("hdmi", 0x2959_0000, 0x4000),
    ("a stub", 0x1100_0000, 0x40),
];

fn machine() -> Machine {
    let mut m = Machine::new(8 << 20);
    m.set_jit(false);
    m.connect_network(false, false);
    m.soc.sdio1.insert(Some(Box::new(MemDisk::new(4096))));
    m.soc.attach_nvme(Box::new(MemDisk::new(4096)));
    m.soc.attach_usb_drive(Box::new(MemDisk::new(4096)));
    m.soc.qspi.flash = Some(starfive_jh7110::qspi::SpiNor::new(&[0x5a; 4096]).unwrap());
    m
}

struct Rng(u64);
impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0.wrapping_mul(0x2545_f491_4f6c_dd1d)
    }
}

/// One access; what it read.
fn access(m: &mut Machine, rng: &mut Rng, window: Option<usize>) -> (u64, Option<u64>) {
    let (_, base, size) = WINDOWS[window.unwrap_or(rng.next() as usize % WINDOWS.len())];
    let at = base + rng.next() % (size / 4) * 4;
    let r = rng.next();
    if r & 1 == 0 { return (at, m.soc.read(at, 4).ok()); }
    // small values as much as large: many fields are narrow
    let v = match r >> 1 & 3 { 0 => r >> 32 & 1, 1 => r >> 32 & 0xff, 2 => 1 << (r >> 32 & 31), _ => r >> 32 };
    let _ = m.soc.write(at, 4, v);
    let t = m.now() + (r >> 3 & 0x3ff);
    m.idle_until(t);
    m.soc.update_devices();
    (at, None)
}

#[test]
fn devices_in_the_middle_of_things() {
    for seed in 1..=(WINDOWS.len() as u64 * 6) {
        // half the time one device alone, thoroughly
        let window = (seed % 2 == 0).then_some(seed as usize / 2 % WINDOWS.len());
        let mut rng = Rng(seed.wrapping_mul(0x9e37_79b9_7f4a_7c15));
        let mut m = machine();
        for _ in 0..400 { access(&mut m, &mut rng, window); }
        let mut fresh = machine();
        fresh.restore(&m.save().unwrap()).unwrap_or_else(|e| panic!("seed {seed}: {e}"));
        // every register, and then the same accesses on both
        for (name, base, size) in WINDOWS.iter().enumerate().filter(|(k, _)| window.is_none_or(|w| w == *k)).map(|(_, w)| w) {
            for at in (*base..base + size).step_by(4) {
                assert_eq!(m.soc.read(at, 4).ok(), fresh.soc.read(at, 4).ok(), "seed {seed}, {name} register {:#x}", at - base);
            }
        }
        let mut again = Rng(rng.0);
        for k in 0..400 {
            let (a, b) = (access(&mut m, &mut rng, window), access(&mut fresh, &mut again, window));
            assert_eq!(a, b, "seed {seed}, access {k} after the restore, {:?}", WINDOWS.iter().find(|w| (w.1..w.1 + w.2).contains(&a.0)));
        }
        assert!(m.state() == fresh.state(), "seed {seed}: state after the same accesses");
    }
}
