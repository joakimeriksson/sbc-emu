//! Devices in the middle of things: random register accesses, a snapshot, a restore into a machine
//! nothing was done to, then the same random accesses on both. What they read must agree. Unlike
//! a comparison of saved states, this also sees state a snapshot leaves out.
use emu_core::{disk::MemDisk, Bus as _};
use allwinner_h3::Machine;

const WINDOWS: &[(&str, u64, u64)] = &[
    ("mixer", 0x0110_0000, 0x3000),
    ("tcon", 0x01c0_c000, 0x400),
    ("mmc0", 0x01c0_f000, 0x204),
    ("ehci0", 0x01c1_a000, 0x60),
    ("ohci0", 0x01c1_a400, 0x60),
    ("ohci1", 0x01c1_b400, 0x60),
    ("ccu", 0x01c2_0000, 0x400),
    ("timer", 0x01c2_0c00, 0x40),
    ("uart0", 0x01c2_8000, 0x20),
    ("emac", 0x01c3_0000, 0x100),
    ("mali gp", 0x01c4_0000, 0x80),
    ("mali l2", 0x01c4_1000, 0x40),
    ("mali pmu", 0x01c4_2000, 0x40),
    ("mali gp mmu", 0x01c4_3000, 0x40),
    ("mali pp0 mmu", 0x01c4_4000, 0x40),
    ("mali pp0", 0x01c4_8000, 0x400),
    ("mali pp1", 0x01c4_a000, 0x400),
    ("gicd", 0x01c8_1000, 0xc00),
    ("gicc", 0x01c8_2000, 0x20),
    ("hdmi", 0x01ee_0000, 0x10000),
    ("hdmi phy", 0x01ef_0000, 0x100),
];

fn machine() -> Machine {
    let mut m = Machine::with_ram_size(8 << 20);
    m.set_jit(false);
    m.connect_network(false, false);
    m.soc.mmc.card.disk = Some(Box::new(MemDisk::new(4096)));
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
fn access(m: &mut Machine, rng: &mut Rng, window: Option<usize>) -> (u64, Option<u32>) {
    let (_, base, size) = WINDOWS[window.unwrap_or(rng.next() as usize % WINDOWS.len())];
    let at = base + rng.next() % (size / 4) * 4;
    let r = rng.next();
    if r & 1 == 0 { return (at, m.soc.read32(at).ok()); }
    // small values as much as large: many fields are narrow
    let v = match r >> 1 & 3 { 0 => r >> 32 & 1, 1 => r >> 32 & 0xff, 2 => 1 << (r >> 32 & 31), _ => r >> 32 };
    let _ = m.soc.write32(at, v as u32);
    m.advance_time(r >> 3 & 0x3ff);
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
                assert_eq!(m.soc.read32(at).ok(), fresh.soc.read32(at).ok(), "seed {seed}, {name} register {:#x}", at - base);
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

/// A snapshot whose clock is years ahead of its devices does not hang the emulator: USB (a frame
/// each millisecond), the display and the timers all catch up.
#[test]
fn a_clock_far_ahead() {
    for bit in [40, 50, 60] {
        let mut m = machine();
        // both OHCIs operational, with their frame interrupt; the timers; a loop on core 0
        for base in [0x01c1_a400u64, 0x01c1_b400] {
            m.soc.write32(base + 4, 2 << 6).unwrap();
            m.soc.write32(base + 0x10, 1 << 31 | 1 << 2).unwrap();
        }
        for (o, v) in [(0x00, 3), (0x14, 240_000), (0x10, 7), (0x24, 240_000), (0x20, 7)] { m.soc.write32(0x01c2_0c00 + o, v).unwrap(); }
        m.soc.write32(allwinner_h3::RAM_BASE, 0xeaff_fffe).unwrap();
        m.cpus[0].r[15] = allwinner_h3::RAM_BASE as u32;
        m.run(100_000);
        for c in &mut m.cpus { c.cntpct |= 1 << bit; }
        let mut far = machine();
        far.restore(&m.save().unwrap()).unwrap();
        let start = std::time::Instant::now();
        for _ in 0..10 { far.run(10_000); }
        assert!(start.elapsed().as_secs() < 5, "bit {bit}: {:?}", start.elapsed());
    }
}
