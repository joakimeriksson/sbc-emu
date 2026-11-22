//! AArch32 user code (EL0) under an AArch64 kernel, as on an ARMv8-A core that runs 32-bit
//! programs (the Cortex-A72 of a Raspberry Pi 4 with a 32-bit userland, or `linux32`
//! binaries on a 64-bit system).
//!
//! The 64-bit core owns everything privileged: it hands this core the user registers on an
//! exception return to AArch32 and takes them back on the next exception. While the program
//! runs, this core asks it (`El1Host`) to translate addresses through the EL1&0 regime and to
//! answer the few CP15 registers EL0 may reach (thread ID, generic timer, CP15 barriers), and
//! reports each exception as an `El0Exit` instead of entering an AArch32 mode: on ARMv8 the
//! exception is taken to AArch64 EL1 with a syndrome, so the guest kernel sees an SVC, abort
//! or undefined instruction from a lower exception level using AArch32.
use crate::cpu::{Cpu, MODE_USR};
use emu_core::{Bus, Trap};

/// The kind of a user access to translate.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum El0Access {
    Read,
    Write,
    Exec,
}

/// What a CP15 access at EL0 does.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Cp15 {
    /// A read: the value (64 bits for MRRC).
    Read(u64),
    /// A write (or a barrier) that took effect.
    Done,
    /// Not accessible at EL0: UNDEFINED.
    Undefined,
    /// Trapped to EL1 (an EL0 timer access the kernel has not enabled): EC 0x03/0x04.
    Trap,
}

/// The privileged side: the AArch64 core whose EL1&0 translation regime and system registers
/// the user program runs under.
pub trait El1Host {
    /// Translate a user-mode access; `Err` holds an AArch64 fault status code (DFSC/IFSC).
    fn translate(&mut self, bus: &mut dyn Bus, va: u32, access: El0Access) -> Result<u64, u32>;
    /// An MRC/MCR (or MRRC/MCRR) to coprocessor 15: `insn` in its A32 encoding (condition
    /// passed); `value` is what a write stores (Rt2:Rt for MCRR).
    fn cp15(&mut self, insn: u32, value: u64) -> Cp15;
    /// The JIT compiled code from physical `pa` (the kernel's instruction-cache maintenance
    /// of that page must reach `Cpu::code_changed`).
    fn compiled(&mut self, _pa: u32) {}
}

/// The reason the user program left: the AArch64 exception the 64-bit core must take.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum El0Event {
    /// UNDEFINED (EC 0x00).
    Undefined,
    /// SVC with its immediate (EC 0x11); returns after the instruction.
    Svc(u32),
    /// BKPT with its immediate (EC 0x38).
    Bkpt(u16),
    /// Instruction abort (EC 0x20): faulting address and fault status code.
    PrefetchAbort { va: u32, fsc: u32 },
    /// Data abort (EC 0x24).
    DataAbort { va: u32, fsc: u32, write: bool },
    /// A trapped CP15 access (EC 0x03 for MRC/MCR, 0x04 for MRRC/MCRR); A32 encoding.
    Cp15Trap(u32),
}

/// An exception from the user program, with the instruction that raised it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct El0Exit {
    pub event: El0Event,
    /// Address of the instruction.
    pub pc: u32,
    /// Its size: 2 for a 16-bit Thumb instruction (ESR.IL = 0), else 4.
    pub size: u32,
}

/// Fault status from the host (`El1Host::translate`), kept apart from short-descriptor FSRs.
pub(crate) const HOST_FSC: u32 = 1 << 31;

impl Cpu {
    /// A core for AArch32 EL0 under an AArch64 kernel: User mode, VFP and Advanced SIMD
    /// enabled (the kernel controls them through CPACR_EL1), unaligned accesses allowed,
    /// SWP/SWPB undefined (ARMv8 removed them), unimplemented encodings UNDEFINED.
    pub fn new_el0() -> Self {
        let mut cpu = Cpu::new();
        cpu.cpsr = MODE_USR;
        cpu.sctlr = 1; // translation on: page-crossing accesses are split and checked per page
        cpu.cpacr = 0xf << 20;
        cpu.vfp.fpexc = 1 << 30;
        cpu.undef_to_guest = true;
        cpu
    }

    /// Prepare to run user code from guest RAM at `ram` (physical 0, `len` bytes). `mt`: other
    /// cores run in parallel on host threads (barriers become host fences, store-exclusive a
    /// compare-and-swap).
    ///
    /// # Safety
    /// As `set_ram`: `ram` must point to `len` writable bytes that outlive this `Cpu`.
    pub unsafe fn el0_setup(&mut self, ram: *mut u8, base: u64, len: u64, mt: bool) {
        let len = len.min(u32::MAX as u64) as u32;
        let base = base as u32; // AArch32 EL0 reaches only RAM below 4 GiB
        if self.ram != ram || self.ram_len != len || self.ram_base != base {
            // SAFETY: the caller's guarantee
            unsafe { self.set_ram(ram, base, len) };
        }
        self.mt = mt;
    }

    /// Run one user instruction (the host has checked for interrupts). On an exception the
    /// result is `Err` and `el0_exit` says which; registers are as the exception leaves them
    /// (the instruction did not complete).
    pub fn step_el0(&mut self, bus: &mut dyn Bus, host: &mut dyn El1Host) -> Result<(), Trap> {
        // SAFETY: the pointer is only used during this call, while `host` is borrowed
        let host: *mut (dyn El1Host + '_) = host;
        self.el1 = Some(unsafe { std::mem::transmute::<*mut (dyn El1Host + '_), *mut (dyn El1Host + 'static)>(host) });
        self.el0_exit = None;
        let result = if self.jit.is_some() {
            self.jit_poll();
            match if self.interp_run { None } else { self.jit_step(bus) } {
                Some(r) => r,
                None => {
                    // a block that is not hot yet: interpreted up to its end
                    let pc = self.r[15];
                    let r = self.exec_one(bus);
                    if self.r[15] != pc.wrapping_add(self.cur_size) || r.is_err() {
                        self.interp_run = false;
                    }
                    r
                }
            }
        } else {
            self.exec_one(bus)
        };
        self.el1 = None;
        match result {
            Err(Trap::Brk(imm)) => {
                self.instret = self.instret.wrapping_sub(1); // BKPT does not complete
                self.r[15] = self.cur_pc;
                self.el0_exit = Some(self.exit(El0Event::Bkpt(imm)));
                Err(Trap::Exception(0x38, 1))
            }
            Err(Trap::Hlt(_)) => {
                // halting debug is off: UNDEFINED
                self.instret = self.instret.wrapping_sub(1);
                self.r[15] = self.cur_pc;
                self.el0_exit = Some(self.exit(El0Event::Undefined));
                Err(Trap::Exception(0, 1))
            }
            r => r,
        }
    }

    /// The exception the last `step_el0` raised, if any.
    pub fn take_el0_exit(&mut self) -> Option<El0Exit> {
        self.el0_exit.take()
    }

    pub(crate) fn exit(&self, event: El0Event) -> El0Exit {
        El0Exit { event, pc: self.cur_pc, size: self.cur_size }
    }

    /// Drop the cached translations of the page holding `va` (a TLBI by address).
    pub fn utlb_invalidate(&mut self, va: u64) {
        if va >> 32 != 0 {
            return;
        }
        let va = va as u32;
        for t in 0..6 {
            let e = &mut self.utlb[t * crate::mmu::UT + (va >> 12) as usize % crate::mmu::UT];
            if e.tag == (va & !0xfff) as u64 {
                *e = crate::mmu::Utlb::INVALID;
            }
        }
    }

    /// Drop every cached translation (TLBI ALL, a new address space).
    pub fn utlb_invalidate_all(&mut self) {
        self.utlb_flush();
    }

    /// A user-mode exception under an AArch64 kernel: record it instead of entering an
    /// AArch32 mode. Returns false when this core runs its own exception modes.
    pub(crate) fn el0_exception(&mut self, event: El0Event) -> bool {
        if self.el1.is_none() {
            return false;
        }
        self.el0_exit = Some(self.exit(event));
        true
    }

    /// CP15 at EL0 under an AArch64 kernel (`insn` is an MRC/MCR or MRRC/MCRR to CP15).
    pub(crate) fn el0_cp15(&mut self, insn: u32) -> Result<(), Trap> {
        let wide = insn & 0x0fe0_0000 == 0x0c40_0000;
        let read = insn & (1 << 20) != 0;
        let (rt, rt2) = ((insn >> 12) & 0xf, (insn >> 16) & 0xf);
        let undefined = Err(Trap::Unimplemented(self.cur_pc as u64, insn));
        if rt == 15 || wide && rt2 == 15 {
            return undefined;
        }
        let value = if wide {
            self.r[rt as usize] as u64 | (self.r[rt2 as usize] as u64) << 32
        } else {
            self.r[rt as usize] as u64
        };
        let host = self.el1.expect("el0_cp15 outside step_el0");
        // SAFETY: set by step_el0 for the duration of the step
        match unsafe { (*host).cp15(insn, value) } {
            Cp15::Read(v) if read => {
                self.r[rt as usize] = v as u32;
                if wide {
                    self.r[rt2 as usize] = (v >> 32) as u32;
                }
                Ok(())
            }
            Cp15::Done if !read => Ok(()),
            Cp15::Trap => {
                self.el0_exit = Some(self.exit(El0Event::Cp15Trap(insn)));
                Err(Trap::Exception(if wide { 0x04 } else { 0x03 }, 1))
            }
            _ => undefined,
        }
    }

    /// Translate through the host (user access); the error is (VA, `HOST_FSC` | FSC).
    pub(crate) fn el0_translate(&mut self, bus: &mut dyn Bus, va: u32, access: El0Access) -> Result<u32, (u32, u32)> {
        let host = self.el1.expect("el0_translate outside step_el0");
        // SAFETY: set by step_el0 for the duration of the step
        match unsafe { (*host).translate(bus, va, access) } {
            Ok(pa) if pa >> 32 == 0 => Ok(pa as u32),
            Ok(_) => Err((va, HOST_FSC | 0x10)), // beyond 4 GiB: not reachable by this core
            Err(fsc) => Err((va, HOST_FSC | fsc)),
        }
    }

    /// An AArch64 fault status code for a fault this core found (`status`: a short-descriptor
    /// FSR, or a host code).
    pub(crate) fn el0_fsc(status: u32) -> u32 {
        if status & HOST_FSC != 0 {
            status & 0x3f
        } else if status & 0x40f == 0x01 {
            0x21 // alignment fault
        } else {
            0x10 // synchronous external abort
        }
    }

    /// Load-exclusive bookkeeping once the value is read: the reservation, and the value for
    /// the compare-and-swap of a multi-threaded store-exclusive.
    pub(crate) fn mark_exclusive(&mut self, address: u32, size: u32, value: u64) {
        self.exclusive_addr = Some(address);
        self.exclusive_size = size;
        self.exclusive_val = value;
    }

    /// Store-exclusive of `size` bytes (8: two words, `value` low word first). Ok(0) when it
    /// stored, Ok(1) when it failed (no reservation, or another core changed the location).
    pub(crate) fn store_exclusive(
        &mut self,
        bus: &mut dyn Bus,
        address: u32,
        size: u32,
        value: u64,
    ) -> Result<u32, (u32, u32)> {
        let reserved = self.exclusive_addr == Some(address) && self.exclusive_size == size;
        self.exclusive_addr = None;
        self.exclusive_size = 0;
        if !reserved {
            return Ok(1);
        }
        if self.mt && address % size == 0 {
            // other cores run at the same time: the store happens only if memory still holds
            // what the load-exclusive read (as QEMU does; an ABA change goes unnoticed)
            self.translate_fill(bus, address, crate::mmu::Access::Write)?;
            if let Some(p) = self.host_ptr(address, size) {
                use std::sync::atomic::{AtomicU16, AtomicU32, AtomicU64, AtomicU8, Ordering::SeqCst};
                let old = self.exclusive_val;
                // SAFETY: a micro-TLB write entry maps the naturally aligned bytes into guest RAM
                let ok = unsafe {
                    match size {
                        1 => AtomicU8::from_ptr(p).compare_exchange(old as u8, value as u8, SeqCst, SeqCst).is_ok(),
                        2 => AtomicU16::from_ptr(p as *mut u16).compare_exchange(old as u16, value as u16, SeqCst, SeqCst).is_ok(),
                        4 => AtomicU32::from_ptr(p as *mut u32).compare_exchange(old as u32, value as u32, SeqCst, SeqCst).is_ok(),
                        _ => AtomicU64::from_ptr(p as *mut u64).compare_exchange(old, value, SeqCst, SeqCst).is_ok(),
                    }
                };
                return Ok(u32::from(!ok));
            }
        }
        match size {
            1 => self.mem_write8(bus, address, value as u8)?,
            2 => self.mem_write16(bus, address, value as u16)?,
            4 => self.mem_write32(bus, address, value as u32)?,
            _ => {
                self.mem_write32(bus, address, value as u32)?;
                self.mem_write32(bus, address.wrapping_add(4), (value >> 32) as u32)?;
            }
        }
        Ok(0)
    }

    /// ARMv8 load-acquire/store-release, A32 encoding (`cccc 0001 1 sz L Rn Rt 11 x0 1001 Rt`):
    /// the exclusive forms are LDREX/STREX with ordering, the others plain loads and stores.
    pub(crate) fn acquire_release(&mut self, bus: &mut dyn Bus, insn: u32) -> Result<(), Trap> {
        let undefined = Err(Trap::Unimplemented(self.cur_pc as u64, insn));
        let (load, excl, size) = (insn & (1 << 20) != 0, insn & (1 << 9) != 0, (insn >> 21) & 3);
        if excl {
            // the LDREX/STREX encoding (bits 9:8 = 11), between barriers
            if !load {
                self.barrier();
            }
            let r = self.execute(bus, self.cur_pc as u64, insn | 0x100);
            if load {
                self.barrier();
            }
            return r;
        }
        let (rn, rt) = ((insn >> 16) & 15, if load { (insn >> 12) & 15 } else { insn & 15 });
        let bits = [4, 0, 1, 2][size as usize]; // 00 word, 01 reserved (D), 10 byte, 11 halfword
        if bits == 0 || rn == 15 || rt == 15 || (if load { insn & 15 } else { (insn >> 12) & 15 }) != 15 {
            return undefined;
        }
        let address = self.r[rn as usize];
        use crate::mmu::Access;
        if load {
            let value = match bits {
                1 => self.mem_read8(bus, address, Access::Read).map(u32::from),
                2 => self.mem_read16(bus, address).map(u32::from),
                _ => self.mem_read32(bus, address, Access::Read),
            };
            match value {
                Ok(v) => self.r[rt as usize] = v,
                Err((va, status)) => return Err(self.data_abort(va, status, false)),
            }
            self.barrier();
        } else {
            self.barrier();
            let value = self.r[rt as usize];
            let r = match bits {
                1 => self.mem_write8(bus, address, value as u8),
                2 => self.mem_write16(bus, address, value as u16),
                _ => self.mem_write32(bus, address, value),
            };
            if let Err((va, status)) = r {
                return Err(self.data_abort(va, status, true));
            }
        }
        Ok(())
    }

    /// A barrier (DMB/DSB, or a CP15 barrier): a host fence when cores run in parallel.
    pub(crate) fn barrier(&self) {
        if self.mt {
            std::sync::atomic::fence(std::sync::atomic::Ordering::SeqCst);
        }
    }
}
