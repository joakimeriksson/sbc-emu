//! The VideoCore IV 3D GPU on the Pi 3's bus: its registers at 0x3fc0_0000 (bus 0x7ec0_0000),
//! a job writing RAM through the bus alias, its interrupt into the ARM-side controller (GPU
//! interrupt 10), a snapshot round trip.

use crate::machine::{Machine, ARMCTRL, GPU_V3D, V3D};
use crate::Model;
use emu_core::Bus;

/// A rendering-only job: clear a 64x64 RGBA8888 frame at `fb` (the kernel's list for a clear).
fn clear_job(m: &mut Machine, rcl: u32, fb: u32, colour: u32) -> u32 {
    let mut r = vec![113u8];
    r.extend_from_slice(&(fb | 0xc000_0000).to_le_bytes());
    r.extend_from_slice(&64u16.to_le_bytes());
    r.extend_from_slice(&64u16.to_le_bytes());
    r.extend_from_slice(&(1u16 << 2).to_le_bytes());
    r.push(114);
    r.extend_from_slice(&colour.to_le_bytes());
    r.extend_from_slice(&colour.to_le_bytes());
    r.extend_from_slice(&0xff_ffffu32.to_le_bytes());
    r.push(0);
    r.extend_from_slice(&[115, 0, 0, 28, 0, 0, 0, 0, 0, 0]); // tile (0,0), store None: clears
    r.extend_from_slice(&[115, 0, 0, 25]); // tile (0,0), store and end of frame
    m.load(rcl as u64, &r);
    rcl + r.len() as u32
}

#[test]
fn v3d_on_the_bus() {
    let mut m = Machine::new(Model::B3Plus, 256 << 20, 1);
    m.soc.v3d.time_scale = 0.0;
    assert_eq!(m.soc.read32(V3D).unwrap(), vc4_3d::IDENT0_VALUE);
    let end = clear_job(&mut m, 0x10_0000, 0x20_0000, 0x1234_5678);
    m.soc.write32(ARMCTRL + 0x10, 1 << GPU_V3D).unwrap(); // enable IRQs 1
    m.soc.write32(V3D + vc4_3d::INTENA as u64, vc4_3d::INT_FRDONE).unwrap();
    m.soc.write32(V3D + vc4_3d::CT1CA as u64, 0x10_0000 | 0xc000_0000).unwrap();
    m.soc.write32(V3D + vc4_3d::CT1EA as u64, end | 0xc000_0000).unwrap();
    assert_eq!(m.soc.read32(0x20_0000 + 4 * (63 * 64 + 63)).unwrap(), 0x1234_5678, "the frame in RAM");
    m.soc.device_lines(1_000_000);
    assert!(m.soc.armctrl.irq(), "GPU interrupt {GPU_V3D} pending at the ARM-side controller");
    assert_ne!(m.soc.armctrl.read(0x04) & 1 << GPU_V3D, 0, "pending 1 shows it");
    // acknowledge: the line drops
    m.soc.write32(V3D + vc4_3d::INTCTL as u64, vc4_3d::INT_FRDONE).unwrap();
    m.soc.device_lines(2_000_000);
    assert!(!m.soc.armctrl.irq());
    // the device's state is part of a snapshot
    let snap = m.save().unwrap();
    let mut m2 = Machine::new(Model::B3Plus, 256 << 20, 1);
    m2.restore(&snap).unwrap();
    assert_eq!(m2.soc.read32(V3D + vc4_3d::CT1CA as u64).unwrap(), end | 0xc000_0000);
    assert_eq!(m2.soc.read32(V3D + vc4_3d::INTENA as u64).unwrap(), vc4_3d::INT_FRDONE);
}
