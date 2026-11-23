//! Time goes on while cores sleep.
use bcm2711::{CoreState, Machine, Stop};

/// A core parked in WFI with its interrupts masked and its timer run out (Linux's other cores after
/// a panic) does not stop time for the one that waits for its own timer.
#[test]
fn a_parked_core_does_not_stop_time() {
    let mut m = Machine::with_cpus(65 << 20, 2);
    // wfi; b .-4
    for (i, w) in [0xd503_207fu32, 0x17ff_ffff].iter().enumerate() {
        m.soc.ram[0x20_0000 + 4 * i..0x20_0004 + 4 * i].copy_from_slice(&w.to_le_bytes());
    }
    for (k, cval) in [(0, 108_000), (1, 54)] {
        m.state[k] = CoreState::Run;
        let c = &mut m.cpus[k];
        (c.el, c.pc, c.daif) = (1, 0x20_0000, 0x3c0);
        // the timers are on and unmasked, but no interrupt is enabled at the GIC
        c.sys.vtimer.ctl = 1;
        c.sys.vtimer.cval = cval;
    }
    for _ in 0..200 {
        if let Stop::Idle(t) = m.run(1_000_000) { m.idle_until(t) }
    }
    assert!(m.now > 100_000_000, "{} ns", m.now);
}
