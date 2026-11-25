//! GA10B runlist, RAMFC, USERD and GPFIFO submission.
use super::{compute, copy, mmu, threed, twod};
use crate::host1x::Host1x;
use std::collections::HashMap;

const INLINE_CLASS: u32 = 0xa140;

// Runlist and USERMODE register offsets within GA10B BAR0.
pub(super) const RUNLIST: u64 = 0x800000;
pub(super) const GR_RUNLIST: u64 = 0x804000;
pub(super) const SUBMIT: u64 = RUNLIST + 0x88;
pub(super) const GR_SUBMIT: u64 = GR_RUNLIST + 0x88;
pub(super) const DOORBELL: u64 = 0xbb0090;
pub(super) const LEGACY_DEMO_DOORBELL: u64 = 0x030090;
const GP_GET: u64 = 0x88;
const GP_PUT: u64 = 0x8c;
const USERD_GET: u64 = 0x44;
const USERD_REF: u64 = 0x48;
const USERD_TOP_LEVEL_GET: u64 = 0x58;
const USERD_GET_HI: u64 = 0x60;
const MAX_ENTRIES: u32 = 128;
/// The GPFIFO entry's 21-bit length field.
const MAX_WORDS: u32 = (1 << 21) - 1;
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
    twod: twod::TwoD,
    /// Inline-to-memory (A140), for uploads outside the 3D and compute classes.
    inline: maxwell::inline::Inline,
    semaphore: [u32; 4],
    legacy_semaphore: [u32; 3],
    packet: Option<Packet>,
}

/// The rest of a method packet: its data words still to come.
struct Packet {
    mode: u32,
    sub: usize,
    method: u32,
    remaining: u32,
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
        channel.threed.timestamp = now;
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
        channel.threed.begin();
        for _ in 0..entries {
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
            channel.threed.flush(&mut memory)?;
            get = (get + 1) & (entries - 1);
            memory.ram[get_offset..get_offset + 4].copy_from_slice(&get.to_le_bytes());
            // The push-buffer GET pointers: drivers poll them to reuse push-buffer space.
            let end = address + u64::from(words) * 4;
            for (low, high) in [
                (USERD_GET, USERD_GET_HI),
                (USERD_TOP_LEVEL_GET, USERD_TOP_LEVEL_GET + 4),
            ] {
                for (word, value) in [(low, end as u32), (high, (end >> 32) as u32)] {
                    let at = mmu::physical(memory.ram, channel.userd + word, 4)?;
                    memory.ram[at..at + 4].copy_from_slice(&value.to_le_bytes());
                }
            }
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
            let word = memory.load(base + u64::from(index) * 4)?;
            index += 1;
            // A packet's data words can continue in the next GPFIFO entry.
            if let Some(mut packet) = self.packet.take() {
                let method = packet.method;
                if self
                    .method(memory, packet.sub, method, word, nonstall, host1x, trace)
                    .is_none()
                {
                    if trace {
                        eprintln!(
                            "[gpu] subchannel {} method {method:#x} value {word:#x} at word {} stopped",
                            packet.sub,
                            index - 1
                        );
                    }
                    return None;
                }
                match packet.mode {
                    1 => packet.method += 4,
                    5 => {
                        packet.method += 4;
                        packet.mode = 3;
                    }
                    _ => {}
                }
                packet.remaining -= 1;
                if packet.remaining > 0 {
                    self.packet = Some(packet);
                } else {
                    self.end_packet(memory, packet.sub, trace)?;
                }
                continue;
            }
            let header = word;
            let mode = header >> 29;
            // DMA_NOP is a zero word. SET_SUB_DEV_MASK can select this device.
            if header == 0 || (mode == 0 && header >> 16 == 1 && header & 0x10 != 0) {
                continue;
            }
            let count = header >> 16 & 0x1fff;
            let sub = (header >> 13 & 7) as usize;
            let method = (header & 0x1fff) * 4;
            if mode == 4 {
                self.method(memory, sub, method, count, nonstall, host1x, trace)?;
                self.end_packet(memory, sub, trace)?;
                continue;
            }
            if !matches!(mode, 1 | 3 | 5) {
                if trace {
                    eprintln!(
                        "[gpu] bad push-buffer header {header:#x} at word {}",
                        index - 1
                    );
                }
                return None;
            }
            if count > 0 {
                self.packet = Some(Packet {
                    mode,
                    sub,
                    method,
                    remaining: count,
                });
            }
        }
        Some(())
    }

    /// SEM_EXECUTE: acquires, releases and reductions on 32 or 64-bit payloads.
    fn semaphore_execute(
        &mut self,
        data: u32,
        memory: &mut mmu::View,
        host1x: &mut Host1x,
    ) -> Option<()> {
        self.threed.flush(memory)?;
        // Operation, acquire TSG switch, release WFI, payload size, timestamp, reduction.
        if data & !0xfb10_1007 != 0 {
            return None;
        }
        let [lo, hi, payload_lo, payload_hi] = self.semaphore;
        let address = u64::from(lo & !3) | u64::from(hi & 0xff) << 32;
        let wide = data & (1 << 24) != 0;
        let payload = if wide {
            u64::from(payload_lo) | u64::from(payload_hi) << 32
        } else {
            u64::from(payload_lo)
        };
        let load = |memory: &mmu::View| -> Option<u64> {
            let low = u64::from(memory.load(address)?);
            Some(if wide {
                low | u64::from(memory.load(address + 4)?) << 32
            } else {
                low
            })
        };
        let store = |memory: &mut mmu::View, value: u64| -> Option<()> {
            memory.store(address, value as u32)?;
            if wide {
                memory.store(address + 4, (value >> 32) as u32)?;
            }
            Some(())
        };
        let operation = data & 7;
        match operation {
            0 | 2..=5 => {
                let value = load(memory)?;
                let circular = if wide {
                    value.wrapping_sub(payload) as i64 >= 0
                } else {
                    (value as u32).wrapping_sub(payload as u32) as i32 >= 0
                };
                let acquired = match operation {
                    0 => value == payload,
                    2 => value >= payload,
                    3 => circular,
                    4 => value & payload != 0,
                    _ => value & payload == 0,
                };
                acquired.then_some(())
            }
            1 | 6 => {
                // nvgpu retires jobs with a release into the host1x syncpoint aperture.
                let physical = memory.translate(address, true)?;
                if operation == 1 && host1x.gpu_increment(physical) {
                    return Some(());
                }
                let value = if operation == 1 {
                    payload
                } else {
                    let old = load(memory)?;
                    let signed = data >> 31 == 0;
                    match data >> 27 & 0xf {
                        0 if signed => (old as i64).min(payload as i64) as u64,
                        0 => old.min(payload),
                        1 if signed => (old as i64).max(payload as i64) as u64,
                        1 => old.max(payload),
                        2 => old ^ payload,
                        3 => old & payload,
                        4 => old | payload,
                        5 => old.wrapping_add(payload),
                        6 => {
                            if old >= payload {
                                0
                            } else {
                                old + 1
                            }
                        }
                        7 => {
                            if old == 0 || old > payload {
                                payload
                            } else {
                                old - 1
                            }
                        }
                        _ => return None,
                    }
                };
                store(memory, value)?;
                if data & (1 << 25) != 0 {
                    memory.store(address + 8, self.compute.timestamp as u32)?;
                    memory.store(address + 12, (self.compute.timestamp >> 32) as u32)?;
                }
                Some(())
            }
            _ => None,
        }
    }

    /// A packet's last data word: a macro call it made runs now.
    fn end_packet(&mut self, memory: &mut mmu::View, sub: usize, trace: bool) -> Option<()> {
        if self.classes[sub] != threed::CLASS {
            return Some(());
        }
        let result = self.threed.end_packet(memory);
        if trace && result.is_none() {
            let fault = self.threed.macro_fault.take();
            eprintln!("[gpu] 3D macro failed (method, value) = {fault:x?}");
        }
        result
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
            0 => {
                if trace {
                    eprintln!("[gpu] subchannel {sub} = class {data:#x}");
                }
                self.classes[sub] = data & 0xffff;
            }
            0x08 | 0x24 => {} // Host NOP and framebuffer flush.
            0x10..=0x18 => self.legacy_semaphore[((method - 0x10) / 4) as usize] = data,
            0x1c if data & !0x0110_101f == 0 => {
                self.threed.flush(memory)?;
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
            // SET_REFERENCE: the value software reads back from USERD's REF word.
            0x50 => {
                if trace {
                    eprintln!("[gpu] SET_REFERENCE {data:#x} (USERD {:#x})", self.userd);
                }
                let at = mmu::physical(memory.ram, self.userd.checked_add(USERD_REF)?, 4)?;
                memory.ram[at..at + 4].copy_from_slice(&data.to_le_bytes());
            }
            // MEM_OP_D barriers and L2/TLB operations complete immediately.
            // GPU memory accesses use the current page tables and guest RAM.
            0x28..=0x30 => {} // MEM_OP_A..C: operands of the MEM_OP_D below.
            0x34 if matches!(data >> 27, 0x05 | 0x09 | 0x0e | 0x10 | 0x15) => {}
            0x5c..=0x68 => self.semaphore[((method - 0x5c) / 4) as usize] = data,
            0x6c => self.semaphore_execute(data, memory, host1x)?,
            _ => {
                // The graphics copy engine answers on subchannel 4 without a SET_OBJECT.
                let class = match self.classes[sub] {
                    0 if sub == 4 => copy::CLASS,
                    class => class,
                };
                // Another engine sees what the 3D engine drew, and the 3D engine what it wrote.
                if class != threed::CLASS {
                    self.threed.flush(memory)?;
                    self.threed.written();
                }
                let result = match class {
                    copy::CLASS => self.copy.method(method, data, memory),
                    compute::CLASS => self.compute.method(method, data, memory),
                    threed::CLASS => self.threed.method(method, data, memory),
                    twod::CLASS => self.twod.method(method, data, memory),
                    INLINE_CLASS => match method {
                        0x100 | 0x110 => Some(()),
                        _ if maxwell::inline::Inline::owns(method) => {
                            self.inline
                                .upload(method, data, |address, bytes| memory.write(address, bytes))
                        }
                        _ => None,
                    },
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
