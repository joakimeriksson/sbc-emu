//! Devices in the middle of things: random register accesses, a snapshot, a restore into a machine
//! nothing was done to, then the same random accesses on both. What they read must agree. Unlike
//! a comparison of saved states, this also sees state a snapshot leaves out.
use emu_core::{disk::MemDisk, Bus as _};
use rockchip_rk3288::{devices, HdmiMode, Machine};

const WINDOWS: &[(&str, u64, u64)] = &[
    ("dw_mmc sd", 0xff0c_0000, 0x240),
    ("dw_mmc emmc", 0xff0f_0000, 0x240),
    ("i2c0", 0xff65_0000, 0x224),
    ("i2c5", 0xff17_0000, 0x224),
    ("uart2", 0xff69_0000, 0x20),
    ("pmu", 0xff73_0000, 0x40),
    ("gpio0", 0xff75_0000, 0x64),
    ("gpio7", 0xff7e_0000, 0x64),
    ("cru", 0xff76_0000, 0x1d0),
    ("grf", 0xff77_0000, 0x400),
    ("i2s", 0xff89_0000, 0x28),
    ("rga", 0xff92_0000, 0x180),
    ("vop big", 0xff93_0000, 0x340),
    ("vop little", 0xff94_0000, 0x340),
    ("dsi", 0xff96_0000, 0x100),
    ("hdmi", 0xff98_0000, 0x4000),
    ("gpu control", 0xffa3_0000, 0x400),
    ("gpu jobs", 0xffa3_1000, 0xa00),
    ("gpu mmu", 0xffa3_2000, 0x500),
    ("dmac", 0xffb2_0000, 0x1000),
    ("gicd", 0xffc0_1000, 0xc00),
    ("gicc", 0xffc0_2000, 0x20),
];

fn machine() -> Machine {
    let mut m = Machine::with_ram_size(8 << 20);
    m.set_jit(false);
    m.connect_hdmi(&HdmiMode::MODES[1]);
    m.soc.periph.attach_rk808(1_700_000_000);
    m.soc.periph.insert_sd(Some(Box::new(MemDisk::new(4096))));
    m.soc.periph.mmc[devices::EMMC].insert(Some(Box::new(MemDisk::new(4096))));
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

/// A snapshot whose clock is years ahead of its devices does not hang the emulator.
#[test]
fn a_clock_far_ahead() {
    for bit in [40, 50, 60] {
        let mut m = machine();
        // audio playing and recording, a loop on core 0
        for (o, v) in [(0x08, 0x0001_0101u32), (0x10, 0x100 | 1 << 24), (0x1c, 3)] { m.soc.write32(0xff89_0000 + o, v).unwrap(); }
        m.soc.write32(0, 0xeaff_fffe).unwrap();
        m.cpus[0].r[15] = 0;
        m.run(100_000);
        for c in &mut m.cpus { c.cntpct |= 1 << bit; }
        let mut far = machine();
        far.restore(&m.save().unwrap()).unwrap();
        let start = std::time::Instant::now();
        for _ in 0..10 { far.run(10_000); let _ = far.soc.read32(0xff89_000c); }
        assert!(start.elapsed().as_secs() < 5, "bit {bit}: {:?}", start.elapsed());
    }
}
