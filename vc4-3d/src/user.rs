//! User QPU programs: queued through V3D_SRQPC (with uniforms from V3D_SRQUA), run on the
//! twelve QPUs outside the GL pipeline, sharing the VPM, sixteen semaphores and a mutex.
//! (Linux's `vc4` never runs them; the firmware's "execute QPU" mailbox call and programs like
//! `hello_fft` do.) Programs run in turns, each until it ends or waits on a semaphore or the
//! mutex; a program that waits picks up again when another one gives it what it waits for, or
//! when more programs are queued.

use crate::mem::Mem;
use crate::qpu::{Code, Env, Qpu, Stop};
use std::collections::VecDeque;

pub const QPUS: usize = 12;
/// VPM size: 12 KiB, 192 rows of 16 words.
pub const VPM_WORDS: usize = 12 * 1024 / 4;

pub struct Sched {
    pub queue: VecDeque<(u32, u32)>,
    pub running: Vec<Option<Qpu>>,
    pub sem: [u8; 16],
    pub mutex: Option<usize>,
    pub vpm: Vec<u32>,
    /// programs queued and completed (mod 256), and the queue error flag
    pub made: u32,
    pub completed: u32,
    pub error: bool,
    /// QPU host interrupts raised (bit per QPU)
    pub host_int: u32,
    pub insns: u64,
    /// decoded programs (separate from the GL jobs' cache, which a GPU thread may hold)
    code: Code,
}

impl Default for Sched { fn default() -> Self { Sched::new() } }

struct UserEnv<'a> {
    me: usize,
    vpm: &'a mut [u32],
    sem: &'a mut [u8; 16],
    mutex: &'a mut Option<usize>,
    host_int: &'a mut u32,
}

impl Env for UserEnv<'_> {
    fn vpm(&mut self) -> (&mut [u32], usize, usize) { (self.vpm, 0, 0) }
    fn semaphore(&mut self, n: u8, down: bool) -> bool {
        let s = &mut self.sem[n as usize & 15];
        if down { if *s == 0 { return false; } *s -= 1; } else if *s < 15 { *s += 1; }
        true
    }
    fn mutex(&mut self, acquire: bool) -> bool {
        if acquire {
            match *self.mutex { None => { *self.mutex = Some(self.me); true } Some(o) => o == self.me }
        } else {
            if *self.mutex == Some(self.me) { *self.mutex = None; }
            true
        }
    }
    fn host_int(&mut self) { *self.host_int |= 1 << self.me; }
}

impl Sched {
    pub fn new() -> Sched {
        Sched { queue: VecDeque::new(), running: vec![None; QPUS], sem: [0; 16], mutex: None, vpm: vec![0; VPM_WORDS], made: 0, completed: 0, error: false, host_int: 0, insns: 0, code: Code::default() }
    }

    pub fn reset(&mut self) { *self = Sched::new(); }

    pub fn queue_len(&self) -> u32 { self.queue.len() as u32 }

    pub fn push(&mut self, pc: u32, unif: u32) {
        if self.queue.len() >= 16 { self.error = true; return; }
        self.queue.push_back((pc, unif));
        self.made = (self.made + 1) & 0xff;
    }

    /// Run queued and waiting programs until each has ended or waits with nothing left that
    /// could wake it.
    pub fn run(&mut self, mem: &mut Mem) {
        self.code.clear();
        const SLICE: u64 = 4096;
        let mut budget: u64 = 50_000_000;
        loop {
            // start queued programs on free QPUs
            for k in 0..QPUS {
                if self.running[k].is_none() {
                    if let Some((pc, unif)) = self.queue.pop_front() {
                        let mut q = Qpu::new();
                        q.num = k as u32;
                        q.start(pc, unif);
                        self.running[k] = Some(q);
                    }
                }
            }
            let mut progress = false;
            for k in 0..QPUS {
                let Some(mut q) = self.running[k].take() else { continue };
                let before = q.insns;
                let mut env = UserEnv { me: k, vpm: &mut self.vpm, sem: &mut self.sem, mutex: &mut self.mutex, host_int: &mut self.host_int };
                let stop = q.run(&mut self.code, mem, &mut env, SLICE);
                let ran = q.insns - before;
                self.insns += ran;
                crate::progress(ran);
                budget = budget.saturating_sub(ran);
                if ran > 0 { progress = true; }
                match stop {
                    Stop::End => { self.completed = (self.completed + 1) & 0xff; }
                    Stop::Blocked | Stop::Budget => self.running[k] = Some(q),
                }
            }
            let idle = self.running.iter().all(|r| r.is_none()) && self.queue.is_empty();
            if idle || !progress { return; }
            if budget == 0 {
                crate::log_first("vc4: user QPU programs ran out of budget; stopping them");
                for r in self.running.iter_mut() { if r.take().is_some() { self.completed = (self.completed + 1) & 0xff; } }
                return;
            }
        }
    }
}
