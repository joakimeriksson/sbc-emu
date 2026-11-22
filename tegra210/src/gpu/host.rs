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
}

#[derive(Default)]
struct State {
    /// the class bound to each subchannel
    class: [u32; 8],
    semaphore: [u32; 4],
    /// a semaphore acquire that has to be retried: (address, value, at least)
    waiting: Option<(u64, u32, bool)>,
    /// the rest of the segment being executed: (address, words)
    rest: Option<(u64, u32)>,
    /// the method header being executed: (method, subchannel, words left, mode)
    header: Option<(u32, usize, u32, u32)>,
    copy: crate::gpu::copy::Copy,
    engine: maxwell::Engine,
}

#[derive(Default)]
pub struct Host {
    channels: HashMap<u32, State>,
    pub trace: bool,
}

impl Host {
    pub fn run_channel(&mut self, id: u32, inst: u64, view: &mut View, now: u64) -> Done {
        let mut done = Done::default();
        let trace = std::env::var_os("SBC_TRACE_GPFIFO").is_some();
        let s = self.channels.entry(id).or_default();
        let ramfc = |o: u64, v: &View| mmu::read32(inst + o, v.smmu, v.ram).unwrap_or(0);
        let userd = (ramfc(0x08, view) & !0x1ff) as u64 | (ramfc(0x0c, view) as u64) << 32;
        let base = ramfc(0x48, view) as u64 | ((ramfc(0x4c, view) & 0xff) as u64) << 32;
        let entries = 1u32 << (ramfc(0x4c, view) >> 16 & 31);
        for _ in 0..1 << 20 {
            if let Some((at, value, ge)) = s.waiting {
                let v = view.read32(at);
                if if ge { v.wrapping_sub(value) as i32 >= 0 } else { v == value } {
                    s.waiting = None;
                    done.resumed = true;
                } else {
                    done.waiting = true;
                    s.engine.end(view);
                    if trace {
                        eprintln!("[gpfifo] channel {id}: waits at {at:#x} for {value:#x} (is {v:#x}, at least: {ge})");
                    }
                    return done;
                }
            }
            if let Some((at, words)) = s.rest.take() {
                s.execute(at, words, view, now, &mut done, trace);
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
            let (at, words) = ((lo & !3) as u64 | ((hi & 0xff) as u64) << 32, hi >> 10 & 0x1f_ffff);
            if trace {
                eprintln!("[gpfifo] channel {id}: entry {get} at {at:#x}, {words} words");
            }
            done.entries += 1;
            s.engine.begin();
            s.rest = Some((at, words));
        }
        done
    }
}

impl State {
    fn execute(&mut self, mut at: u64, words: u32, view: &mut View, now: u64, done: &mut Done, trace: bool) {
        let mut data = vec![0; words as usize * 4];
        view.read(at, &mut data);
        let mut k = 0;
        while k < words as usize {
            let w = u32::from_le_bytes(data[4 * k..4 * k + 4].try_into().unwrap());
            k += 1;
            match self.header.take() {
                Some((method, sub, left, mode)) => {
                    self.method(sub, method, w, view, now, done, trace);
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
                            self.method(sub, method, count, view, now, done, trace);
                            self.engine.end_group(self.class[sub], view, now);
                        }
                        1 | 3 | 5 if count != 0 => self.header = Some((method, sub, count, mode)),
                        _ => {}
                    }
                }
            }
            if self.waiting.is_some() && k < words as usize {
                at += 4 * k as u64;
                self.rest = Some((at, words - k as u32));
                return;
            }
        }
    }

    fn method(&mut self, sub: usize, method: u32, v: u32, view: &mut View, now: u64, done: &mut Done, trace: bool) {
        if trace {
            eprintln!("[method] {sub} {:04x} {method:04x} {v:08x}", self.class[sub]);
        }
        if method >= 0x100 {
            match self.class[sub] {
                0xb0b5 => {
                    if method == 0x300 {
                        self.engine.end(view);
                    }
                    self.copy.method(method, v, view);
                }
                class => self.engine.method(class, method, v, view, now),
            }
            return;
        }
        match method {
            0x00 => self.class[sub] = v & 0xffff,
            0x10..=0x18 => self.semaphore[(method as usize - 0x10) / 4] = v,
            0x1c => {
                let at = ((self.semaphore[0] & 0xff) as u64) << 32 | (self.semaphore[1] & !3) as u64;
                let payload = self.semaphore[2];
                match v & 0xf {
                    1 => self.waiting = Some((at, payload, false)),
                    4 => self.waiting = Some((at, payload, true)),
                    2 => {
                        self.engine.end(view);
                        view.write32(at, payload);
                        if v & 1 << 24 == 0 {
                            view.write(at + 8, &now.to_le_bytes()); // four words with a time stamp
                        }
                    }
                    _ => {}
                }
            }
            0x20 => done.nonstall = true,
            _ => {}
        }
    }
}
