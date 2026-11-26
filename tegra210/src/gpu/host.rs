//! The host: a channel's push buffers. GPFIFO entries (in the ring the channel's RAMFC names) point
//! at segments of methods; the host executes its own methods (semaphores, interrupts, references)
//! and passes the others to the engine object on their subchannel.
use super::mmu::{self, View};
use std::collections::HashMap;

#[derive(Default)]
pub struct Done {
    pub entries: u64,
    pub nonstall: bool,
    /// stopped at a semaphore
    pub waiting: bool,
    /// a semaphore it waited for came
    pub resumed: bool,
    /// host1x sync points the channel incremented (nvgpu's job tracking on L4T)
    pub syncpts: Vec<usize>,
}

/// How a semaphore is compared.
#[derive(Clone, Copy, Debug)]
enum Test {
    Equal,
    AtLeast,
    And,
}

impl Test {
    fn passes(self, v: u32, value: u32) -> bool {
        match self {
            Test::Equal => v == value,
            Test::AtLeast => v.wrapping_sub(value) as i32 >= 0,
            Test::And => v & value != 0,
        }
    }
}

#[derive(Default)]
struct State {
    /// the class bound to each subchannel
    class: [u32; 8],
    semaphore: [u32; 4],
    /// a semaphore acquire that has to be retried: (address, value, how they compare)
    waiting: Option<(u64, u32, Test)>,
    /// SYNCPOINTA: the threshold of the next sync point wait
    syncpt_payload: u32,
    /// the semaphore wait last reported (SBC_TRACE_SYNCPT)
    reported: Option<(u64, u32)>,
    /// where the channel's USERD is and the GP_GET the host last wrote there: a channel bound
    /// again to its instance block is the same channel only if GP_GET is still that
    gp_get: Option<(u64, u32)>,
    /// a host1x sync point wait that has to be retried: (point, threshold)
    syncpt_wait: Option<(usize, u32)>,
    /// the rest of the segment being executed: (address, words)
    rest: Option<(u64, u32)>,
    /// the method header being executed: (method, subchannel, words left, mode)
    header: Option<(u32, usize, u32, u32)>,
    copy: crate::gpu::copy::Copy,
    engine: maxwell::Engine,
}

pub struct Host {
    channels: HashMap<u32, State>,
    /// the 3D/compute state of each graphics context, by (address space, the context's address
    /// in it): channels that share a context (CUDA's streams of one context) share it, as the
    /// engine's context holds it on the hardware
    engines: HashMap<(u64, u64), maxwell::Engine>,
    /// the context each channel last ran with
    contexts: HashMap<u32, (u64, u64)>,
    trace: bool,
}

impl Default for Host {
    fn default() -> Self {
        Self { channels: HashMap::new(), engines: HashMap::new(), contexts: HashMap::new(), trace: std::env::var_os("SBC_TRACE_GPFIFO").is_some() }
    }
}

impl Host {
    /// A channel was bound to another instance block: nothing of an earlier one with its number
    /// is left.
    pub fn forget(&mut self, id: u32) {
        if crate::gpu::flag::sem() {
            if let Some(s) = self.channels.get(&id) {
                eprintln!("[sem] forget channel {id}: rest {:?} waiting {:?} syncpt_wait {:?}", s.rest, s.waiting, s.syncpt_wait);
            }
        }
        self.channels.remove(&id);
        // a context no channel uses any more goes with its last channel
        if let Some(context) = self.contexts.remove(&id) {
            if !self.contexts.values().any(|&c| c == context) {
                self.engines.remove(&context);
            }
        }
    }

    /// A channel bound again to its instance block: the same channel (nvgpu after power gating)
    /// if its USERD's GP_GET is what the host last wrote there; a new one gets a fresh state. A
    /// channel that never took a GPFIFO entry has no state of its own to keep: it is taken as a
    /// new one too, so that a new process's channel with the same number and instance block does
    /// not keep the old one's place in its context (`enter` decides about the context).
    pub fn rebound(&mut self, id: u32, smmu: &crate::smmu::Smmu, ram: &[u8]) {
        let same = self.channels.get(&id).is_some_and(|s| s.gp_get.is_some_and(|(userd, get)| mmu::read32(userd + 0x88, smmu, ram) == Some(get)));
        if crate::gpu::flag::syncpt() {
            let s = self.channels.get(&id);
            eprintln!("[chan] {id} bound again: {} (last GP_GET {:?}, now {:?})", if same { "kept" } else { "new" }, s.and_then(|s| s.gp_get).map(|g| g.1), s.and_then(|s| s.gp_get).and_then(|(u, _)| mmu::read32(u + 0x88, smmu, ram)));
        }
        if !same {
            self.forget(id);
        }
    }

    /// Channel `id` runs with `context`: which channels name which context, and which contexts
    /// are left behind. `live`: whether a bound channel other than `id` names a context.
    fn enter(&mut self, id: u32, context: (u64, u64), live: impl FnOnce((u64, u64)) -> bool) {
        let previous = self.contexts.insert(id, context);
        if let Some(old) = previous.filter(|&o| o != context) {
            if !self.contexts.values().any(|&c| c == old) {
                self.engines.remove(&old);
            }
        }
        // A new channel whose context no other bound channel names: what is left of it is a
        // closed process's, whose address space and context address the new one happens to have
        // again. The context starts fresh. (A live process's channels are bound, whether they
        // have run or not; power gating binds them again before anything runs.)
        if previous.is_none() && self.engines.contains_key(&context) && !live(context) {
            self.contexts.retain(|&k, &mut c| k == id || c != context);
            self.engines.remove(&context);
        }
    }

    /// `points`: host1x's sync point values (the channels' increments are applied to them too).
    /// `bound`: the bound channels, (number, instance block).
    pub fn run_channel(&mut self, id: u32, inst: u64, view: &mut View, now: u64, points: &mut [u32], bound: &[(u32, u64)]) -> Done {
        let context = context(id, inst, view.space.directory, view.smmu, view.ram);
        let (smmu, ram) = (view.smmu, &*view.ram);
        self.enter(id, context, |c| bound.iter().any(|&(k, inst)| k != id && context_of(k, inst, smmu, ram) == c));
        let shared = self.engines.entry(context).or_default();
        let s = self.channels.entry(id).or_default();
        std::mem::swap(&mut s.engine, shared);
        let done = self.run_channel_in(id, inst, view, now, points);
        let s = self.channels.get_mut(&id).unwrap();
        std::mem::swap(&mut s.engine, self.engines.get_mut(&context).unwrap());
        done
    }

    fn run_channel_in(&mut self, id: u32, inst: u64, view: &mut View, now: u64, points: &mut [u32]) -> Done {
        let mut done = Done::default();
        let trace = self.trace;
        let s = self.channels.entry(id).or_default();
        let ramfc = |o: u64, v: &View| mmu::read32(inst + o, v.smmu, v.ram).unwrap_or(0);
        let userd = (ramfc(0x08, view) & !0x1ff) as u64 | (ramfc(0x0c, view) as u64) << 32;
        let base = ramfc(0x48, view) as u64 | ((ramfc(0x4c, view) & 0xff) as u64) << 32;
        let entries = 1u32 << (ramfc(0x4c, view) >> 16 & 31);
        for _ in 0..1 << 20 {
            if let Some((point, threshold)) = s.syncpt_wait {
                let v = points.get(point).copied().unwrap_or(threshold);
                if v.wrapping_sub(threshold) as i32 >= 0 {
                    s.syncpt_wait = None;
                    done.resumed = true;
                } else {
                    done.waiting = true;
                    s.engine.end(view);
                    if trace || crate::gpu::flag::syncpt() {
                        eprintln!("[gpfifo] channel {id}: waits for sync point {point} to reach {threshold} (is {v})");
                    }
                    return done;
                }
            }
            if let Some((at, value, test)) = s.waiting {
                let v = view.read32(at);
                if test.passes(v, value) {
                    s.waiting = None;
                    done.resumed = true;
                } else {
                    done.waiting = true;
                    s.engine.end(view);
                    if trace || (crate::gpu::flag::syncpt() && s.reported != Some((at, value))) {
                        s.reported = Some((at, value));
                        eprintln!("[gpfifo] channel {id}: waits at {at:#x} for {value:#x} (is {v:#x}, {test:?})");
                    }
                    return done;
                }
            }
            if let Some((at, words)) = s.rest.take() {
                s.execute(at, words, view, now, &mut done, trace, points);
                // how far the channel has read, which the kernel polls when its ring wraps
                let end = match s.rest {
                    Some((at, _)) => at,
                    None => at + 4 * words as u64,
                };
                mmu::write32(userd + 0x44, end as u32, view.smmu, view.ram);
                mmu::write32(userd + 0x60, (end >> 32) as u32, view.smmu, view.ram);
                continue;
            }
            let get = mmu::read32(userd + 0x88, view.smmu, view.ram).unwrap_or(0);
            let put = mmu::read32(userd + 0x8c, view.smmu, view.ram).unwrap_or(0);
            if get == put || get >= entries {
                s.engine.end(view);
                return done;
            }
            let (lo, hi) = (view.read32(base + 8 * get as u64), view.read32(base + 8 * get as u64 + 4));
            let next = (get + 1) & (entries - 1);
            mmu::write32(userd + 0x88, next, view.smmu, view.ram);
            s.gp_get = Some((userd, next));
            let (at, words) = ((lo & !3) as u64 | ((hi & 0xff) as u64) << 32, hi >> 10 & 0x1f_ffff);
            if trace || crate::gpu::flag::sem() {
                eprintln!("[gpfifo] channel {id}: entry {get} at {at:#x}, {words} words");
            }
            done.entries += 1;
            s.engine.begin();
            s.rest = Some((at, words));
        }
        // (a ring without end: the rest later)
        s.engine.end(view);
        done.waiting = true;
        done
    }
}

/// The graphics context an instance block names (words 132, 133), in its address space
/// (`directory`); none: the channel's own.
fn context(id: u32, inst: u64, directory: u64, smmu: &crate::smmu::Smmu, ram: &[u8]) -> (u64, u64) {
    let word = |k: u64| mmu::read32(inst + 4 * k, smmu, ram).unwrap_or(0);
    match (word(132) & !0xfff) as u64 | ((word(133) & 0xff) as u64) << 32 {
        0 => (u64::MAX, id as u64),
        c => (directory, c),
    }
}

/// The same, for a channel whose address space is not at hand.
fn context_of(id: u32, inst: u64, smmu: &crate::smmu::Smmu, ram: &[u8]) -> (u64, u64) {
    context(id, inst, mmu::space(inst, smmu, ram).directory, smmu, ram)
}

impl State {
    fn execute(&mut self, mut at: u64, words: u32, view: &mut View, now: u64, done: &mut Done, trace: bool, points: &mut [u32]) {
        let mut data = vec![0; words as usize * 4];
        view.read(at, &mut data);
        let mut k = 0;
        while k < words as usize {
            let w = u32::from_le_bytes(data[4 * k..4 * k + 4].try_into().unwrap());
            k += 1;
            match self.header.take() {
                Some((method, sub, left, mode)) => {
                    self.method(sub, method, w, view, now, done, trace, points);
                    let next = match mode {
                        1 => method + 4,
                        5 => method + 4, // increments once
                        _ => method,
                    };
                    if left > 1 {
                        self.header = Some((next, sub, left - 1, if mode == 5 { 3 } else { mode }));
                    } else {
                        self.engine.end_group(self.class[sub], view, now);
                    }
                }
                None => {
                    let (mode, count, sub, method) = (w >> 29, w >> 16 & 0x1fff, (w >> 13 & 7) as usize, (w & 0xfff) << 2);
                    match mode {
                        4 => {
                            self.method(sub, method, count, view, now, done, trace, points);
                            self.engine.end_group(self.class[sub], view, now);
                        }
                        1 | 3 | 5 if count != 0 => self.header = Some((method, sub, count, mode)),
                        _ => {}
                    }
                }
            }
            if (self.waiting.is_some() || self.syncpt_wait.is_some()) && k < words as usize {
                at += 4 * k as u64;
                self.rest = Some((at, words - k as u32));
                return;
            }
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn method(&mut self, sub: usize, method: u32, v: u32, view: &mut View, now: u64, done: &mut Done, trace: bool, points: &mut [u32]) {
        if trace {
            eprintln!("[method] {sub} {:04x} {method:04x} {v:08x}", self.class[sub]);
        }
        if method >= 0x100 {
            // subchannel 4 of a graphics channel is its copy engine (GRCE) even unbound: NVK
            // sends copies there without binding a class (nouveau's GL driver binds 0xb0b5)
            let class = if sub == 4 && self.class[sub] == 0 { 0xb0b5 } else { self.class[sub] };
            match class {
                0xb0b5 => {
                    if method == 0x300 {
                        self.engine.end(view);
                    }
                    self.copy.method(method, v, view, now);
                    if method == 0x300 {
                        self.engine.written();
                    }
                }
                class => self.engine.method(class, method, v, view, now),
            }
            return;
        }
        match method {
            0x00 => self.class[sub] = v & 0xffff,
            0x10..=0x18 => self.semaphore[(method as usize - 0x10) / 4] = v,
            0x1c => {
                if crate::gpu::flag::sem() && v & 0x1f != 2 && v & 0x1f != 0x10 { eprintln!("[sem] host op {v:#x} at {:#x}", ((self.semaphore[0] & 0xff) as u64) << 32 | (self.semaphore[1] & !3) as u64); }
                let at = ((self.semaphore[0] & 0xff) as u64) << 32 | (self.semaphore[1] & !3) as u64;
                let payload = self.semaphore[2];
                match v & 0x1f {
                    // REDUCTION: an atomic operation on the 32-bit semaphore (CUDA counts with it)
                    0x10 => {
                        self.engine.end(view);
                        let old = view.read32(at);
                        let signed = v >> 31 == 0;
                        let new = match v >> 27 & 15 {
                            0 if signed => (old as i32).min(payload as i32) as u32,
                            0 => old.min(payload),
                            1 if signed => (old as i32).max(payload as i32) as u32,
                            1 => old.max(payload),
                            2 => old ^ payload,
                            3 => old & payload,
                            4 => old | payload,
                            5 => old.wrapping_add(payload),
                            6 => if old >= payload { 0 } else { old + 1 },
                            7 => if old == 0 || old > payload { payload } else { old - 1 },
                            _ => old,
                        };
                        // (32 bits only, whatever RELEASE_SIZE says: CUDA reduces counters that sit
                        // next to other semaphores, with that bit clear)
                        view.write32(at, new);
                        if crate::gpu::flag::sem() { eprintln!("[sem] host reduce {at:#x}: {old:#x} -> {new:#x} (op {v:#x})"); }
                    }
                    1 => self.waiting = Some((at, payload, Test::Equal)),
                    8 => self.waiting = Some((at, payload, Test::And)),
                    4 => self.waiting = Some((at, payload, Test::AtLeast)),
                    2 => {
                        self.engine.end(view);
                        if crate::gpu::flag::sem() { eprintln!("[sem] host release {at:#x} = {payload:#x} (op {v:#x})"); }
                        view.write32(at, payload);
                        if v & 1 << 24 == 0 {
                            // four words with a time stamp
                            view.write32(at + 4, 0);
                            view.write(at + 8, &now.to_le_bytes());
                        }
                    }
                    _ => {}
                }
            }
            0x20 => done.nonstall = true,
            0x70 => self.syncpt_payload = v,
            // SYNCPOINTB: increment a host1x sync point (once the work before it is done, which it
            // is: the engines run at once), or wait until one reaches SYNCPOINTA's threshold
            0x74 => {
                let point = (v >> 8 & 0xff) as usize;
                match v & 3 {
                    0 => {
                        if crate::gpu::flag::syncpt() {
                            eprintln!("[syncpt] wait {point} >= {:#x} (is {:#x})", self.syncpt_payload, points.get(point).copied().unwrap_or(0));
                        }
                        self.syncpt_wait = Some((point, self.syncpt_payload));
                    }
                    1 => {
                        self.engine.end(view);
                        if let Some(p) = points.get_mut(point) {
                            *p = p.wrapping_add(1);
                        }
                        done.syncpts.push(point);
                        if crate::gpu::flag::syncpt() {
                            eprintln!("[syncpt] incr {point} -> {:#x}", points.get(point).copied().unwrap_or(0));
                        }
                    }
                    _ => {}
                }
            }
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn contexts_of_closed_channels_are_not_inherited() {
        let mut h = Host::default();
        let k = (0x1000, 0x20_0000);
        // two streams of one context; the second one shares the first's engine
        h.enter(1, k, |_| false);
        h.engines.entry(k).or_default();
        h.enter(2, k, |c| c == k);
        assert!(h.engines.contains_key(&k));
        // another stream, while a stream that has not run yet is bound: it shares the context
        h.enter(9, k, |c| c == k);
        assert!(h.engines.contains_key(&k));
        h.forget(9);
        assert!(h.engines.contains_key(&k));
        // the process is closed: no bound channel names the context. A new process's channel
        // with the same address space and context address starts fresh
        h.enter(3, k, |_| false);
        assert!(!h.engines.contains_key(&k));
        assert_eq!(h.contexts.keys().copied().collect::<Vec<_>>(), [3]);
        // a channel that ran before keeps its context, whatever else is bound
        h.engines.entry(k).or_default();
        h.enter(3, k, |_| false);
        assert!(h.engines.contains_key(&k));
        // forgetting the last channel of a context drops it
        h.forget(3);
        assert!(!h.engines.contains_key(&k) && h.contexts.is_empty());
    }

    #[test]
    fn bound_channels_name_their_contexts() {
        // instance blocks at 0x1000 and 0x2000 of RAM in one address space, naming context
        // 0x12_3456_7000 (words 132, 133) and none
        let mut ram = vec![0u8; 0x4000];
        let base = crate::RAM_BASE;
        let put = |ram: &mut [u8], o: usize, v: u32| ram[o..o + 4].copy_from_slice(&v.to_le_bytes());
        for inst in [0x1000, 0x2000] {
            put(&mut ram, inst + 0x200, 0x8000_0000);
        }
        put(&mut ram, 0x1000 + 4 * 132, 0x3456_7abc);
        put(&mut ram, 0x1000 + 4 * 133, 0x12);
        let smmu = crate::smmu::Smmu::default();
        assert_eq!(context_of(5, base + 0x1000, &smmu, &ram), (0x8000_0000, 0x12_3456_7000));
        assert_eq!(context_of(6, base + 0x2000, &smmu, &ram), (u64::MAX, 6));
    }

    #[test]
    fn a_channel_bound_again_is_the_same_only_if_it_took_entries() {
        let mut h = Host::default();
        let smmu = crate::smmu::Smmu::default();
        let mut ram = vec![0u8; 0x1000];
        let userd = crate::RAM_BASE + 0x200;
        let k = (0x1000, 0x20_0000);
        // channel 1 took entries up to GP_GET 3; bound again with GP_GET still 3: the same
        h.enter(1, k, |_| false);
        h.engines.entry(k).or_default();
        h.channels.entry(1).or_default().gp_get = Some((userd, 3));
        mmu::write32(userd + 0x88, 3, &smmu, &mut ram);
        h.rebound(1, &smmu, &ram);
        assert!(h.channels.contains_key(&1) && h.engines.contains_key(&k));
        // GP_GET moved: another channel in the block
        mmu::write32(userd + 0x88, 0, &smmu, &mut ram);
        h.rebound(1, &smmu, &ram);
        assert!(!h.channels.contains_key(&1) && !h.engines.contains_key(&k));
        // a channel that took no entries has nothing of its own: a new process's channel with
        // its number and block does not take its place in the context
        h.enter(2, k, |_| false);
        h.engines.entry(k).or_default();
        h.channels.entry(2).or_default();
        h.rebound(2, &smmu, &ram);
        assert!(!h.contexts.contains_key(&2) && !h.engines.contains_key(&k));
    }
}
