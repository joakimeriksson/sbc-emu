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
use std::collections::BTreeMap;

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
    /// Polygon lists by address space (its page-table base) and framebuffer descriptor:
    /// tiler jobs append, the fragment job of the same framebuffer consumes. Processes
    /// share virtual addresses, so the descriptor alone does not name a framebuffer.
    pub lists: BTreeMap<(u64, u64), Vec<Prim>>,
}

/// Polygon lists kept at once; a list whose fragment job never came (a failed chain) is
/// dropped when the address space changes, or here when too many pile up.
const MAX_LISTS: usize = 64;

impl JobState {
    /// Drop the polygon lists of the address space with page tables at `transtab`.
    pub fn drop_lists(&mut self, transtab: u64) {
        self.lists.retain(|&(t, _), _| t != transtab);
    }

    /// Run the chain at `head` in the address space with page tables at `transtab`.
    pub fn run_chain(&mut self, vm: &mut Vm, transtab: u64, head: u64, cycle_now: u64, trace: bool) -> ChainResult {
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
                    if self.lists.len() >= MAX_LISTS && !self.lists.contains_key(&(transtab, fbd)) {
                        // the lowest key: any would do, but the same one in every run
                        let stale = *self.lists.keys().next().unwrap();
                        self.lists.remove(&stale);
                    }
                    let list = self.lists.entry((transtab, fbd)).or_default();
                    cycles += draw::tiler_job(vm, va, list);
                    status::DONE
                }
                JOB_FRAGMENT => {
                    let fbd = vm.r64(va + 32 + 8) & !63;
                    let list = self.lists.remove(&(transtab, fbd)).unwrap_or_default();
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
            if vm.exhausted {
                // the hardware would still be busy when the driver's timeout resets it
                if trace {
                    eprintln!("[mali] job {va:#x}: chain over its work budget, failed");
                }
                vm.w32(va, status::JOB_CONFIG_FAULT);
                return ChainResult { status: status::JOB_CONFIG_FAULT, tail, cycles };
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

impl emu_core::snap::Snap for JobState {
    fn save(&self, w: &mut emu_core::snap::W) {
        let Self { counts, lists } = self;
        counts.save(w);
        w.u64(lists.len() as u64);
        for (key, list) in lists {
            key.save(w);
            draw::save_list(w, list);
        }
    }
    fn load(&mut self, r: &mut emu_core::snap::R) -> Result<(), emu_core::snap::SnapError> {
        let Self { counts, lists } = self;
        counts.load(r)?;
        lists.clear();
        for _ in 0..r.u64()? {
            let mut key = (0u64, 0u64);
            key.load(r)?;
            lists.insert(key, draw::load_list(r)?);
        }
        Ok(())
    }
}
