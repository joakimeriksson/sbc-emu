//! Job chains: the job manager walks the descriptors from the slot's head through their
//! `next` pointers and runs each job (Mesa `genxml/v5.xml`, "Job Header").
//!
//! Every job starts with a 32-byte header:
//!
//! | word | field |
//! |---|---|
//! | 0 | exception status (written back: 1 = done) |
//! | 1 | first incomplete task |
//! | 2-3 | fault pointer |
//! | 4 | bit 0: 64-bit descriptors, bits 1-7: type, bit 8: barrier, bits 16-31: index |
//! | 5 | dependencies (two 16-bit job indices) |
//! | 6-7 | next job |
//!
//! Jobs in a chain are listed in an order that satisfies their dependencies (Mesa's
//! `pan_jc` appends them in submission order), so the model runs them in list order.

use crate::draw::{self, Prim};
use crate::frag;
use crate::status;
use crate::vm::Vm;
use std::collections::HashMap;

pub const JOB_NULL: u32 = 1;
pub const JOB_WRITE_VALUE: u32 = 2;
pub const JOB_CACHE_FLUSH: u32 = 3;
pub const JOB_COMPUTE: u32 = 4;
pub const JOB_VERTEX: u32 = 5;
pub const JOB_TILER: u32 = 7;
pub const JOB_FRAGMENT: u32 = 9;

/// Longest chain the model follows before declaring it corrupt (a loop in `next`).
const MAX_JOBS: usize = 1 << 16;

pub struct ChainResult {
    pub status: u32,
    /// The last job looked at.
    pub tail: u64,
    /// GPU cycles the chain took.
    pub cycles: u64,
}

/// State shared between jobs (and chains).
#[derive(Default)]
pub struct JobState {
    /// Jobs run, by type (index = job type).
    pub counts: [u64; 16],
    /// Polygon lists by framebuffer descriptor: tiler jobs append, the fragment job of
    /// the same framebuffer consumes.
    pub lists: HashMap<u64, Vec<Prim>>,
}

impl JobState {
    pub fn run_chain(&mut self, vm: &mut Vm, head: u64, cycle_now: u64, trace: bool) -> ChainResult {
        let mut va = head;
        let mut tail = head;
        let mut cycles = 0u64;
        for _ in 0..MAX_JOBS {
            if va == 0 {
                return ChainResult { status: status::DONE, tail, cycles };
            }
            tail = va;
            let w4 = vm.r32(va + 16);
            let next = vm.r64(va + 24);
            let ty = (w4 >> 1) & 0x7f;
            if trace {
                eprintln!("[mali] job {va:#x} type {ty} index {} next {next:#x}", w4 >> 16);
            }
            let code = match ty {
                JOB_NULL | JOB_CACHE_FLUSH => status::DONE,
                JOB_WRITE_VALUE => {
                    let addr = vm.r64(va + 32);
                    match vm.r32(va + 40) {
                        1 => vm.w64(addr, cycle_now + cycles),
                        2 => vm.w64(addr, cycle_now + cycles),
                        3 => vm.w64(addr, 0),
                        _ => {}
                    }
                    status::DONE
                }
                JOB_COMPUTE | JOB_VERTEX => {
                    cycles += draw::vertex_job(vm, va);
                    status::DONE
                }
                JOB_TILER => {
                    let fbd = vm.r64(va + 64 + 28 * 4) & !63;
                    let list = self.lists.entry(fbd).or_default();
                    cycles += draw::tiler_job(vm, va, list);
                    status::DONE
                }
                JOB_FRAGMENT => {
                    let fbd = vm.r64(va + 32 + 8) & !63;
                    let list = self.lists.remove(&fbd).unwrap_or_default();
                    cycles += frag::fragment_job(vm, va, &list, trace);
                    status::DONE
                }
                _ => status::JOB_CONFIG_FAULT,
            };
            self.counts[ty as usize & 15] += 1;
            cycles += 20;
            if vm.fault.is_some() {
                return ChainResult { status: status::JOB_READ_FAULT, tail, cycles };
            }
            if code != status::DONE {
                vm.w32(va, code);
                return ChainResult { status: code, tail, cycles };
            }
            vm.w32(va, status::DONE);
            va = next;
        }
        ChainResult { status: status::JOB_CONFIG_FAULT, tail, cycles }
    }
}
