//! What a debugger (`cli/src/gdb.rs`) sets on a machine: breakpoints by pc, and the watch ranges
//! of one core. Cores check watch ranges in their interpreter only, so a machine runs without
//! its JIT while any are set.

/// Accesses a watch range reports.
pub const WATCH_WRITE: u8 = 1;
pub const WATCH_READ: u8 = 2;

/// A machine's breakpoints.
#[derive(Clone, Default, Debug)]
pub struct Breakpoints {
    pub at: Vec<u64>,
    /// Where cores have been reported (core, pc): running on from there must not stop there again.
    resume: Vec<(usize, u64)>,
    /// While a single step runs: the core it is for. The others stay powered and get no turn (they
    /// may wake up: from WFI by an interrupt, from WFE).
    pub only: Option<usize>,
}

impl Breakpoints {
    /// Whether core `k` may have a turn (see `only`).
    pub fn may_run(&self, k: usize) -> bool { self.only.is_none_or(|o| o == k) }

    pub fn set(&mut self, addr: u64, insert: bool) {
        self.at.retain(|&a| a != addr);
        if insert { self.at.push(addr); }
    }
    /// Core `k` is stepped from `pc`: no stop there, and no turn for the others.
    pub fn begin_step(&mut self, k: usize, pc: u64) {
        self.forget(k);
        self.resume.push((k, pc));
        self.only = Some(k);
    }
    /// What the other cores have been reported for stays.
    pub fn end_step(&mut self, k: usize) {
        self.forget(k);
        self.only = None;
    }
    fn forget(&mut self, k: usize) -> Option<u64> {
        let i = self.resume.iter().position(|r| r.0 == k)?;
        Some(self.resume.swap_remove(i).1)
    }
    /// Core `k` is about to execute the instruction at `pc`: stop in front of it?
    #[inline]
    pub fn hit(&mut self, k: usize, pc: u64) -> bool {
        if self.at.is_empty() { return false; }
        // wherever the core is now, it goes on from here
        let reported = self.forget(k);
        if !self.at.contains(&pc) || reported == Some(pc) { return false; }
        self.resume.push((k, pc));
        true
    }
}

/// A core's watch ranges: (address, length, `WATCH_*` mask), virtual addresses.
#[derive(Clone, Default, Debug)]
pub struct Watch {
    ranges: Vec<(u64, u64, u8)>,
    /// the first access that hit since it was last taken: the range's address and the access
    pub hit: Option<(u64, u8)>,
}

impl Watch {
    pub fn set(&mut self, addr: u64, len: u64, kind: u8, insert: bool) {
        self.ranges.retain(|&r| r != (addr, len, kind));
        if insert { self.ranges.push((addr, len, kind)); }
    }
    pub fn clear(&mut self) { self.ranges.clear(); self.hit = None; }
    #[inline(always)]
    pub fn is_empty(&self) -> bool { self.ranges.is_empty() }
    /// An access of `size` bytes at `va`.
    #[cold]
    pub fn check(&mut self, va: u64, size: u64, access: u8) {
        if self.hit.is_some() { return; }
        // ranges are compared by distance, so one that ends at the top of the address space works
        if let Some(&(a, _, _)) = self.ranges.iter().find(|&&(a, n, k)| k & access != 0 && (va.wrapping_sub(a) < n || a.wrapping_sub(va) < size)) {
            self.hit = Some((a, access));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_breakpoint_stops_once_per_visit() {
        let mut b = Breakpoints::default();
        b.set(0x1000, true);
        assert!(!b.hit(0, 0x0ffc));
        assert!(b.hit(0, 0x1000));
        assert!(!b.hit(0, 0x1000), "running on from the stop");
        assert!(b.hit(0, 0x1000), "the next visit stops again");
        assert!(b.hit(1, 0x1000), "another core's visit is its own");
        b.set(0x1000, false);
        assert!(!b.hit(0, 0x1000));
    }

    #[test]
    fn watch_ranges_report_overlapping_accesses_of_their_kind() {
        let mut w = Watch::default();
        w.set(0x2004, 4, WATCH_WRITE, true);
        w.check(0x2004, 4, WATCH_READ);
        w.check(0x2000, 4, WATCH_WRITE);
        w.check(0x2008, 4, WATCH_WRITE);
        assert_eq!(w.hit, None);
        w.check(0x2000, 8, WATCH_WRITE);
        assert_eq!(w.hit.take(), Some((0x2004, WATCH_WRITE)));
        w.check(0x2007, 1, WATCH_WRITE);
        assert_eq!(w.hit.take(), Some((0x2004, WATCH_WRITE)));
        w.set(0x2004, 4, WATCH_WRITE, false);
        assert!(w.is_empty());
    }
}
