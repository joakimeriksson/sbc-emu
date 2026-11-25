//! GA10B runlist, RAMFC, USERD and GPFIFO submission.
use super::{compute, copy, mmu, threed};
use crate::host1x::Host1x;
use std::collections::HashMap;

// Runlist and USERMODE register offsets within GA10B BAR0.
pub(super) const RUNLIST: u64 = 0x800000;
pub(super) const GR_RUNLIST: u64 = 0x804000;
pub(super) const SUBMIT: u64 = RUNLIST + 0x88;
pub(super) const GR_SUBMIT: u64 = GR_RUNLIST + 0x88;
pub(super) const DOORBELL: u64 = 0xbb0090;
pub(super) const LEGACY_DEMO_DOORBELL: u64 = 0x030090;
const GP_GET: u64 = 0x88;
const GP_PUT: u64 = 0x8c;
const MAX_ENTRIES: u32 = 128;
const MAX_WORDS: u32 = 8192;
const PBDMA: u32 = 0x040000;
const PBDMA_STRIDE: u64 = 0x800;
const PBDMA_COUNT: u64 = 6;
const CHANNEL_RAM: u32 = 0x900000;
const VALID: u32 = 1 << 31;

pub(super) fn read_config(offset: u64) -> Option<u32> {
    if (u64::from(PBDMA)..u64::from(PBDMA) + PBDMA_COUNT * PBDMA_STRIDE).contains(&offset)
        && matches!((offset - u64::from(PBDMA)) % PBDMA_STRIDE, 0x108 | 0x148)
    {
        // Write-one-to-clear status. The modeled PBDMAs have no pending faults.
        return Some(0);
    }
    if (GR_RUNLIST..GR_RUNLIST + 0x1000).contains(&offset) {
        return Some(match offset - GR_RUNLIST {
            0x04 => CHANNEL_RAM + 0x10000,
            0x08 => 1 << 16, // runlist ID 1
            0x10 => VALID | PBDMA,
            0x14 | 0x204 | 0x220 | 0x228 => 0,
            _ => return None,
        });
    }
    Some(match offset.checked_sub(RUNLIST)? {
        0x04 => CHANNEL_RAM,
        0x08 => 0, // runlist ID 0
        0x10 => VALID | PBDMA,
        0x14 | 0x220 => 0, // no second PBDMA
        0x204 => 1 << 16,  // copy engine interrupt ID 1
        0x228 => 1 << 24,  // copy engine ID 1
        _ => return None,
    })
}

#[derive(Default)]
pub(super) struct Host {
    channels: [HashMap<u32, Channel>; 2],
    pub submissions: u64,
    pub nonstall_pending: bool,
}

#[derive(Default)]
struct Channel {
    instance: u64,
    userd: u64,
    classes: [u32; 8],
    copy: copy::Copy,
    compute: compute::Compute,
    threed: threed::ThreeD,
    semaphore: [u32; 4],
    legacy_semaphore: [u32; 3],
}

impl Host {
    pub fn submit(&mut self, runlist: usize, lo: u32, hi: u32, count: u32, ram: &[u8]) {
        let channels = &mut self.channels[runlist];
        if count == 0 {
            channels.clear();
            return;
        }
        if count > MAX_ENTRIES {
            return;
        }
        let base = u64::from(lo & !0x3ff) | u64::from(hi & 0xff) << 32;
        let bytes = count as usize * 16;
        let Some(offset) = mmu::physical(ram, base, bytes) else {
            return;
        };
        let mut previous = std::mem::take(channels);
        for entry in ram[offset..offset + bytes].chunks_exact(16) {
            let word = |at: usize| u32::from_le_bytes(entry[at..at + 4].try_into().unwrap());
            let (userd_lo, userd_hi, inst_lo, inst_hi) = (word(0), word(4), word(8), word(12));
            if userd_lo & 1 != 0 {
                continue; // A TSG header precedes its channel entries.
            }
            let id = inst_lo & 0xfff;
            let userd = u64::from(userd_lo & !0xff) | u64::from(userd_hi) << 32;
            let instance = u64::from(inst_lo & !0xfff) | u64::from(inst_hi) << 32;
            let channel = previous
                .remove(&id)
                .filter(|c| c.instance == instance && c.userd == userd)
                .unwrap_or(Channel {
                    instance,
                    userd,
                    ..Channel::default()
                });
            channels.insert(id, channel);
        }
    }

    pub fn ring(
        &mut self,
        token: u32,
        ram: &mut [u8],
        host1x: &mut Host1x,
        trace: bool,
        now: u64,
    ) -> Option<()> {
        let runlist = (token >> 16 & 0x7f) as usize;
        let channel = self.channels.get_mut(runlist)?.get_mut(&(token & 0xfff))?;
        channel.compute.timestamp = now;
        channel.copy.timestamp = now;
        let lo = mmu::word(ram, channel.instance + 0x48)?;
        let hi = mmu::word(ram, channel.instance + 0x4c)?;
        let base = u64::from(lo & !7) | u64::from(hi & 0xff) << 32;
        let entries = 1u32.checked_shl(hi >> 16 & 0x1f)?;
        let get_addr = channel.userd.checked_add(GP_GET)?;
        let mut get = mmu::word(ram, get_addr)?;
        let put = mmu::word(ram, channel.userd.checked_add(GP_PUT)?)?;
        if trace {
            eprintln!(
                "[gpu] GPFIFO {token:#x}: base={base:#x} get={get} put={put} entries={entries}"
            );
        }
        if get >= entries || put >= entries {
            return None;
        }
        let get_offset = mmu::physical(ram, get_addr, 4)?;
        let mut memory = mmu::View::from_instance(ram, channel.instance)?;
        for _ in 0..MAX_ENTRIES {
            if get == put {
                return Some(());
            }
            let entry = base + u64::from(get) * 8;
            let lo = memory.load(entry)?;
            let hi = memory.load(entry + 4)?;
            let address = u64::from(lo & !3) | u64::from(hi & 0xff) << 32;
            let words = hi >> 10 & 0x1f_ffff;
            if trace {
                eprintln!("[gpu] GPFIFO {token:#x}[{get}]: address={address:#x} words={words}");
            }
            if words > MAX_WORDS {
                return None;
            }
            channel.execute(
                &mut memory,
                address,
                words,
                &mut self.nonstall_pending,
                host1x,
                trace,
            )?;
            get = (get + 1) & (entries - 1);
            memory.ram[get_offset..get_offset + 4].copy_from_slice(&get.to_le_bytes());
            self.submissions += 1;
        }
        (get == put).then_some(())
    }
}

impl Channel {
    fn execute(
        &mut self,
        memory: &mut mmu::View,
        base: u64,
        words: u32,
        nonstall: &mut bool,
        host1x: &mut Host1x,
        trace: bool,
    ) -> Option<()> {
        let mut index = 0;
        while index < words {
            let header = memory.load(base + u64::from(index) * 4)?;
            index += 1;
            let mode = header >> 29;
            // DMA_NOP is a zero word. SET_SUB_DEV_MASK can select this device.
            if header == 0 || (mode == 0 && header >> 16 == 1 && header & 0x10 != 0) {
                continue;
            }
            let count = header >> 16 & 0x1fff;
            let sub = (header >> 13 & 7) as usize;
            let mut method = (header & 0x1fff) * 4;
            if mode == 4 {
                self.method(memory, sub, method, count, nonstall, host1x, trace)?;
                continue;
            }
            if !matches!(mode, 1 | 3 | 5) || count > words - index {
                return None;
            }
            for n in 0..count {
                let data = memory.load(base + u64::from(index) * 4)?;
                index += 1;
                self.method(memory, sub, method, data, nonstall, host1x, trace)?;
                if mode == 1 || (mode == 5 && n == 0) {
                    method += 4;
                }
            }
        }
        Some(())
    }

    fn method(
        &mut self,
        memory: &mut mmu::View,
        sub: usize,
        method: u32,
        data: u32,
        nonstall: &mut bool,
        host1x: &mut Host1x,
        trace: bool,
    ) -> Option<()> {
        match method {
            0 => self.classes[sub] = data & 0xffff,
            0x08 | 0x24 => {} // Host NOP and framebuffer flush.
            0x10..=0x18 => self.legacy_semaphore[((method - 0x10) / 4) as usize] = data,
            0x1c if data & !0x0110_101f == 0 => {
                let [hi, lo, payload] = self.legacy_semaphore;
                let address = (u64::from(hi & 0xff) << 32) | u64::from(lo & !3);
                match data & 0x1f {
                    1 => (memory.load(address)? == payload).then_some(())?,
                    2 => memory.release(
                        address,
                        payload,
                        (data & (1 << 24) == 0).then_some(self.compute.timestamp),
                    )?,
                    4 => (memory.load(address)?.wrapping_sub(payload) as i32 >= 0).then_some(())?,
                    8 => (memory.load(address)? & payload != 0).then_some(())?,
                    _ => return None,
                }
            }
            0x20 => *nonstall = true,
            // MEM_OP_D barriers and L2/TLB operations complete immediately.
            // GPU memory accesses use the current page tables and guest RAM.
            0x34 if matches!(data >> 27, 0x05 | 0x09 | 0x0e | 0x10 | 0x15) => {}
            0x5c..=0x68 => self.semaphore[((method - 0x5c) / 4) as usize] = data,
            0x6c if matches!(data, 0 | 2 | 3) => {
                let [lo, hi, payload, _] = self.semaphore;
                let address = u64::from(lo) | u64::from(hi & 0xff) << 32;
                let value = memory.load(address)?;
                let acquired = match data {
                    0 => value == payload,
                    2 => value >= payload,
                    _ => value.wrapping_sub(payload) as i32 >= 0,
                };
                acquired.then_some(())?;
            }
            0x6c if matches!(data, 1 | 0x10_0001) => {
                // nvgpu appends a host-class semaphore release to retire the job.
                // Bit 20 requests idle, which the synchronous engine already guarantees.
                let [lo, hi, payload, _] = self.semaphore;
                let address = u64::from(lo) | u64::from(hi & 0xff) << 32;
                let physical = memory.translate(address, true)?;
                if !host1x.gpu_increment(physical) {
                    memory.store(address, payload)?;
                }
            }
            _ => {
                let class = self.classes[sub];
                let result = match class {
                    copy::CLASS => self.copy.method(method, data, memory),
                    compute::CLASS => self.compute.method(method, data, memory),
                    threed::CLASS => self.threed.method(method, data, memory),
                    _ => None,
                };
                if trace && result.is_none() {
                    eprintln!("[gpu] class {class:#x}, method {method:#x}, value {data:#x} failed");
                }
                return result;
            }
        }
        Some(())
    }
}
