//! The Mali MMU in front of each processor: a two-level table of 4 KiB pages (a 1024-entry
//! directory at DTE_ADDR, each entry pointing at a 1024-entry page table).

use crate::Memory;

const DTE_ADDR: u64 = 0x00;
const STATUS: u64 = 0x04;
const COMMAND: u64 = 0x08;
const PAGE_FAULT_ADDR: u64 = 0x0c;
const INT_RAWSTAT: u64 = 0x14;
const INT_CLEAR: u64 = 0x18;
const INT_MASK: u64 = 0x1c;
const INT_STATUS: u64 = 0x20;

const STATUS_PAGING_ENABLED: u32 = 1 << 0;
const STATUS_PAGE_FAULT_ACTIVE: u32 = 1 << 1;
const STATUS_STALL_ACTIVE: u32 = 1 << 2;
const STATUS_IDLE: u32 = 1 << 3;
const STATUS_REPLAY_BUFFER_EMPTY: u32 = 1 << 4;
const STATUS_PAGE_FAULT_IS_WRITE: u32 = 1 << 5;

pub const INT_PAGE_FAULT: u32 = 1 << 0;

/// Entry bits: present, read, write.
const PRESENT: u32 = 1 << 0;
const READ: u32 = 1 << 1;
const WRITE: u32 = 1 << 2;

#[derive(Default)]
pub struct Mmu {
    dte: u32,
    paging: bool,
    stall: bool,
    fault: Option<(u32, bool)>,
    rawstat: u32,
    mask: u32,
}

impl Mmu {
    pub fn read(&self, offset: u64) -> u32 {
        match offset {
            DTE_ADDR => self.dte,
            STATUS => {
                let mut s = STATUS_IDLE | STATUS_REPLAY_BUFFER_EMPTY;
                if self.paging {
                    s |= STATUS_PAGING_ENABLED;
                }
                if self.stall {
                    s |= STATUS_STALL_ACTIVE;
                }
                if let Some((_, write)) = self.fault {
                    s |= STATUS_PAGE_FAULT_ACTIVE | if write { STATUS_PAGE_FAULT_IS_WRITE } else { 0 };
                }
                s
            }
            PAGE_FAULT_ADDR => self.fault.map_or(0, |(va, _)| va),
            INT_RAWSTAT => self.rawstat,
            INT_MASK => self.mask,
            INT_STATUS => self.rawstat & self.mask,
            _ => 0,
        }
    }

    pub fn write(&mut self, offset: u64, value: u32) {
        match offset {
            DTE_ADDR => self.dte = value & !0xfff,
            COMMAND => match value {
                0 => self.paging = true,
                1 => self.paging = false,
                2 => self.stall = true,
                3 => self.stall = false,
                4 => {} // zap the TLB: nothing is cached
                5 => self.fault = None,
                6 => *self = Mmu { mask: 0, ..Default::default() },
                _ => {}
            },
            INT_RAWSTAT => self.rawstat |= value,
            INT_CLEAR => self.rawstat &= !value,
            INT_MASK => self.mask = value,
            _ => {}
        }
    }

    pub fn irq(&self) -> bool {
        self.rawstat & self.mask != 0
    }

    /// Whether paging is on (COMMAND 0 enables it).
    pub fn paging(&self) -> bool {
        self.paging
    }

    /// Walk the page tables for `va` without side effects: the physical address, or None
    /// for a missing or forbidden page. Identity while paging is off.
    pub fn lookup(&self, mem: &dyn Memory, va: u32, write: bool) -> Option<u32> {
        if !self.paging {
            return Some(va);
        }
        let pde = mem.read_u32(self.dte + (va >> 22) * 4).unwrap_or(0);
        let pte = if pde & PRESENT != 0 { mem.read_u32((pde & !0xfff) + (va >> 12 & 0x3ff) * 4).unwrap_or(0) } else { 0 };
        let allowed = pte & PRESENT != 0 && pte & if write { WRITE } else { READ } != 0;
        allowed.then_some(pte & !0xfff | va & 0xfff)
    }

    /// Translate a GPU virtual address. A missing or forbidden page raises a page fault
    /// (the first one is kept) and returns None.
    pub fn translate(&mut self, mem: &dyn Memory, va: u32, write: bool) -> Option<u32> {
        let pa = self.lookup(mem, va, write);
        if pa.is_none() && self.fault.is_none() {
            self.fault = Some((va, write));
            self.rawstat |= INT_PAGE_FAULT;
        }
        pa
    }
}

emu_core::snap_struct!(Mmu { dte, paging, stall, fault, rawstat, mask });
