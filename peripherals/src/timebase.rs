//! The Arm architectural (generic) timer at 24 MHz, the counter both the H3 and the RK3288
//! run, against nanoseconds of virtual time.

/// 24 MHz ticks to nanoseconds (rounded up) and back (rounded down), so that
/// `ns_to_ticks(ticks_to_ns(t)) >= t`: an idle deadline converted to nanoseconds and back
/// always reaches the timer compare value.
pub fn ticks_to_ns(ticks: u64) -> u64 {
    (ticks as u128 * 125).div_ceil(3) as u64
}

pub fn ns_to_ticks(ns: u64) -> u64 {
    (ns as u128 * 3 / 125) as u64
}
