use bcm2711::Machine;
use emu_core::Bus;

/// Memory-to-memory copy through a chain of two control blocks, then a zero fill.
#[test]
fn dma_chain_copy_and_fill() {
    let mut m = Machine::new(64 << 20);
    let s = &mut m.soc;
    for i in 0..256u64 { s.write8(0x2000 + i, i as u8).unwrap(); s.write8(0x5000 + i, 0xee).unwrap(); }
    // CB0: copy 100 bytes 0x2000 -> 0x3000 (bus addresses in the 0xc0000000 alias), then CB1
    let cb0 = [(1 << 8) | (1 << 4), 0xc000_2000, 0xc000_3000, 100, 0, 0x1020, 0, 0];
    // CB1: zero-fill 10 bytes at 0x5000 (SRC_IGNORE), interrupt at the end
    let cb1 = [(1 << 11) | (1 << 4) | 1, 0, 0x5000, 10, 0, 0, 0, 0];
    for (k, v) in cb0.iter().enumerate() { s.write32(0x1000 + 4 * k as u64, *v).unwrap(); }
    for (k, v) in cb1.iter().enumerate() { s.write32(0x1020 + 4 * k as u64, *v).unwrap(); }
    let ch2 = 0xfe00_7200;
    s.write32(ch2 + 4, 0x1000).unwrap();
    s.write32(ch2, 1).unwrap();
    for i in 0..100u64 { assert_eq!(s.read8(0x3000 + i).unwrap(), i as u8); }
    assert_eq!(s.read8(0x3000 + 100).unwrap(), 0);
    for i in 0..10u64 { assert_eq!(s.read8(0x5000 + i).unwrap(), 0); }
    assert_eq!(s.read8(0x5000 + 10).unwrap(), 0xee);
    let cs = s.read32(ch2).unwrap();
    assert_eq!(cs & 7, 0b110, "END and INT set, not active: {cs:#x}");
    assert_eq!(s.read32(0xfe00_7fe0).unwrap(), 1 << 2, "INT_STATUS");
    s.write32(ch2, 0b110).unwrap(); // clear END and INT
    assert_eq!(s.read32(ch2).unwrap() & 7, 0);
}
