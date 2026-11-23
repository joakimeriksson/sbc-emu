//! Time goes on while harts sleep.
use emu_core::Stop;
use starfive_jh7110::{HartState, Machine};

/// A hart whose timer has run out with the interrupt off does not stop time for the one that waits
/// for its own timer.
#[test]
fn a_parked_hart_does_not_stop_time() {
    let mut m = Machine::new(64 << 20);
    // wfi; j .-4
    let code: Vec<u8> = [0x1050_0073u32, 0xffdf_f06f].iter().flat_map(|w| w.to_le_bytes()).collect();
    m.load(0x4020_0000, &code).unwrap();
    for (k, cmp) in [(1, 8_000), (2, 4)] {
        m.state[k] = HartState::Run;
        m.harts[k].pc = 0x4020_0000;
        m.soc.clint.mtimecmp[k] = cmp;
    }
    let mut far = false;
    for _ in 0..200 {
        if let Stop::Idle(t) = m.run(1_000_000) {
            assert!(t > m.now(), "{t} at {}", m.now());
            far |= t == u64::MAX;
            m.idle_until(t.min(m.now() + 1_000_000));
        }
    }
    assert!(m.now() > 100_000_000, "{} ns", m.now());
    // both run out: nothing is due
    assert!(far);
}
