//! GA10B runlist, RAMFC, USERD and GPFIFO submission for the copy engine.
use super::{copy, mmu};
use std::collections::HashMap;

// BAR0 apertures for the modeled copy runlist and the TU104-style doorbell.
pub(super) const RUNLIST: u64 = 0x800000;
pub(super) const BASE_LO: u64 = RUNLIST + 0x80;
pub(super) const BASE_HI: u64 = RUNLIST + 0x84;
pub(super) const SUBMIT: u64 = RUNLIST + 0x88;
pub(super) const DOORBELL: u64 = 0x030090;
const GP_GET: u64 = 0x88;
const GP_PUT: u64 = 0x8c;
const MAX_ENTRIES: u32 = 128;
const MAX_WORDS: u32 = 8192;
const PBDMA: u32 = 0x040000;
const CHANNEL_RAM: u32 = 0x900000;
const VALID: u32 = 1 << 31;

pub(super) fn read_config(offset: u64) -> Option<u32> {
    Some(match offset.checked_sub(RUNLIST)? {
        0x08 => 0, // runlist ID 0
        0x04 => CHANNEL_RAM,
        0x10 => VALID | PBDMA, // PBDMA 0, BAR0 base in bits 25:10
        0x14 => 0,             // no second PBDMA
        0x204 | 0x228 => 0,    // graphics interrupt and engine ID 0
        0x244 => 1 << 16,      // copy engine interrupt ID 1
        0x268 => 1 << 24,      // copy engine ID 1
        _ => return None,
    })
}

#[derive(Default)]
pub(super) struct Host {
    channels: HashMap<u32, Channel>,
    pub submissions: u64,
}

#[derive(Default)]
struct Channel {
    instance: u64,
    userd: u64,
    classes: [u32; 8],
    copy: copy::Copy,
}

impl Host {
    pub fn submit(&mut self, lo: u32, hi: u32, count: u32, ram: &[u8]) {
        if count == 0 {
            self.channels.clear();
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
        let mut previous = std::mem::take(&mut self.channels);
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
            self.channels.insert(id, channel);
        }
    }

    pub fn ring(&mut self, token: u32, ram: &mut [u8]) -> Option<()> {
        if token >> 16 & 0x7f != 0 {
            return None;
        }
        let channel = self.channels.get_mut(&(token & 0xfff))?;
        let lo = mmu::word(ram, channel.instance + 0x48)?;
        let hi = mmu::word(ram, channel.instance + 0x4c)?;
        let base = u64::from(lo & !7) | u64::from(hi & 0xff) << 32;
        let entries = 1u32.checked_shl(hi >> 16 & 0x1f)?;
        let root_lo = mmu::word(ram, channel.instance + 0x200)?;
        let root_hi = mmu::word(ram, channel.instance + 0x204)?;
        let root = u64::from(root_lo & !0xfff) | u64::from(root_hi & 0xff) << 32;
        let get_addr = channel.userd.checked_add(GP_GET)?;
        let mut get = mmu::word(ram, get_addr)?;
        let put = mmu::word(ram, channel.userd.checked_add(GP_PUT)?)?;
        if get >= entries || put >= entries {
            return None;
        }
        let get_offset = mmu::physical(ram, get_addr, 4)?;
        let mut memory = mmu::View { ram, root };
        for _ in 0..MAX_ENTRIES {
            if get == put {
                return Some(());
            }
            let entry = base + u64::from(get) * 8;
            let lo = memory.load(entry)?;
            let hi = memory.load(entry + 4)?;
            let address = u64::from(lo & !3) | u64::from(hi & 0xff) << 32;
            let words = hi >> 10 & 0x1f_ffff;
            if words > MAX_WORDS {
                return None;
            }
            channel.execute(&mut memory, address, words)?;
            get = (get + 1) & (entries - 1);
            memory.ram[get_offset..get_offset + 4].copy_from_slice(&get.to_le_bytes());
            self.submissions += 1;
        }
        (get == put).then_some(())
    }
}

impl Channel {
    fn execute(&mut self, memory: &mut mmu::View, base: u64, words: u32) -> Option<()> {
        let mut index = 0;
        while index < words {
            let header = memory.load(base + u64::from(index) * 4)?;
            index += 1;
            let mode = header >> 29;
            let count = header >> 16 & 0x1fff;
            let sub = (header >> 13 & 7) as usize;
            let mut method = (header & 0x1fff) * 4;
            if mode == 4 {
                self.method(memory, sub, method, count)?;
                continue;
            }
            if !matches!(mode, 1 | 3 | 5) || count > words - index {
                return None;
            }
            for n in 0..count {
                let data = memory.load(base + u64::from(index) * 4)?;
                index += 1;
                self.method(memory, sub, method, data)?;
                if mode == 1 || (mode == 5 && n == 0) {
                    method += 4;
                }
            }
        }
        Some(())
    }

    fn method(&mut self, memory: &mut mmu::View, sub: usize, method: u32, data: u32) -> Option<()> {
        if method == 0 {
            self.classes[sub] = data & 0xffff;
            return Some(());
        }
        if self.classes[sub] != copy::CLASS {
            return None;
        }
        self.copy.method(method, data, memory)
    }
}
