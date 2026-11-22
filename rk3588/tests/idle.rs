//! Time goes on while cores sleep.
use emu_core::Stop;
use rk3588::{CoreState, Machine};

/// A core parked in WFI with its interrupts masked and its timer run out (Linux's other cores after
/// a panic) does not stop time for the one that waits for its own timer.
#[test]
fn a_parked_core_does_not_stop_time() {
    let mut m = Machine::with_cpus(128 << 20, 2).unwrap();
    for (k, wait) in [(0, 2_000_000u64), (1, 1_000)] {
        m.state[k] = CoreState::Wfi;
        let c = &mut m.cpus[k];
        c.el = 1;
        c.daif = 0x3c0;
        // the timers are on and unmasked, but no interrupt is enabled at the GIC
        c.sys.vtimer.ctl = 1;
        c.sys.vtimer.cval = wait * 24 / 1000;
    }
    for _ in 0..100 {
        match m.run(1_000_000) {
            Stop::Idle(t) => m.idle_until(t),
            other => panic!("{other:?}"),
        }
    }
    assert!(m.now() > 100_000_000, "{} ns", m.now());
}
