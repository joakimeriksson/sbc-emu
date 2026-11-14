//! Debugger watchpoints: a write and a read of a watched word stop the machine after the
//! instruction that made the access, on the core that made it.
use bcm2711::{Machine, Stop};

fn program() -> Vec<u8> {
    [
        0xd282_0000u32, // movz x0, #0x1000
        0xd280_0541,    // movz x1, #42
        0xf900_0001,    // str x1, [x0]
        0xd280_0003,    // movz x3, #0
        0xf940_0002,    // ldr x2, [x0]
        0x1400_0000,    // b .
    ].iter().flat_map(|w| w.to_le_bytes()).collect()
}

#[test]
fn write_then_read_watchpoints() {
    let mut m = Machine::with_cpus(16 << 20, 1);
    m.load(0x8_0000, &program());
    m.boot_linux(0x8_0000, 0);
    m.watchpoint(0x1004, 4, 3, true); // overlaps the 8-byte accesses at 0x1000
    match m.run(1000) {
        Stop::Watchpoint(0, a, 1) => assert_eq!(a, 0x1004),
        s => panic!("expected the write watchpoint, got {s:?}"),
    }
    assert_eq!(m.cpus[0].pc, 0x8_000c, "stops after the store");
    assert_eq!(m.cpus[0].x[1], 42);
    match m.run(1000) {
        Stop::Watchpoint(0, _, 2) => {}
        s => panic!("expected the read watchpoint, got {s:?}"),
    }
    assert_eq!(m.cpus[0].x[2], 42);
    m.watchpoint(0x1004, 4, 3, false);
    assert!(matches!(m.run(1000), Stop::Budget));
}
