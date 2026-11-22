//! Devices in the middle of things: random register accesses, a snapshot, a restore into a machine
//! nothing was done to, then the same random accesses on both. What they read must agree. Unlike
//! a comparison of saved states, this also sees state a snapshot leaves out.
use emu_core::{disk::MemDisk, Bus as _};
use rk3588::Machine;

const WINDOWS: &[(&str, u64, u64)] = &[
    ("gpu", 0xfb00_0000, 0x4000),
    ("gmac", 0xfe1c_0000, 0x1800),
    ("xhci", 0xfcd0_0000, 0x1000),
    ("pcie config", rk3588::pcie::ECAM, 0x100),
    ("nvme", rk3588::pcie::MEM, 0x1100),
    ("mmc", 0xfe2c_0000, 0x240),
    ("hdmi", rk3588::hdmi_qp::BASE, 0x2000),
    ("vop2", rk3588::vop2::BASE, 0x3000),
    ("uart2", 0xfeb5_0000, 0x20),
    ("gicd", 0xfe60_0000, 0xc00),
    ("gicr", 0xfe68_0000, 0x400),
    ("gicr sgi", 0xfe69_0000, 0x800),
    ("i2s1", 0xfe48_0000, 0x40),
    ("i2s5", 0xfddf_0000, 0x40),
    ("i2c6", 0xfec8_0000, 0x224),
    ("dmac0", 0xfea1_0000, 0x1000),
    ("dmac2", rk3588::audio::DMAC2, 0x1000),
    ("grf sys", rk3588::grf::BLOCKS[0].0, 0x400),
    ("grf", rk3588::grf::BLOCKS[3].0, 0x100),
    ("hdptx", rk3588::grf::HDPTXPHY0, 0x1000),
];

fn machine() -> Machine {
    let mut m = Machine::with_cpus(128 << 20, 2).unwrap();
    m.enable_hdmi(rk3588::display::Mode::MODES[0]);
    m.connect_network(false, false);
    m.attach_nvme(Box::new(MemDisk::new(4096)));
    m.attach_usb_drive(Box::new(MemDisk::new(4096)));
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
    let now = m.now() + (r >> 3 & 0x3fff);
    m.idle_until(now);
    (at, None)
}

#[test]
fn devices_in_the_middle_of_things() {
    for seed in 1..=(WINDOWS.len() as u64 * 4) {
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
