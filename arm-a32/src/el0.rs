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
    /// The JIT compiled code from RAM offset `pa` (the kernel's instruction-cache maintenance
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

    /// Prepare to run user code from guest RAM at `ram` (`len` bytes at physical `base`).
    /// The first 4 GiB of it are reached directly (and compiled); anything else the 64-bit MMU
    /// maps, RAM beyond that or a device, goes through the bus. `mt`: other cores run in
    /// parallel on host threads (barriers become host fences, store-exclusive a
    /// compare-and-swap).
    ///
    /// # Safety
    /// As `set_ram`: `ram` must point to `len` writable bytes that outlive this `Cpu`.
    pub unsafe fn el0_setup(&mut self, ram: *mut u8, base: u64, len: u64, mt: bool) {
        let len = len.min(u32::MAX as u64) as u32;
        if self.ram != ram || self.ram_len != len || self.ram_base != 0 || self.ram_phys != base {
            // SAFETY: the caller's guarantee
            unsafe { self.set_ram(ram, 0, len) };
            self.ram_phys = base; // the JIT's keys stay RAM offsets
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
    pub(crate) fn el0_translate(&mut self, bus: &mut dyn Bus, va: u32, access: El0Access) -> Result<u64, (u32, u32)> {
        let host = self.el1.expect("el0_translate outside step_el0");
        // SAFETY: set by step_el0 for the duration of the step
        match unsafe { (*host).translate(bus, va, access) } {
            Ok(pa) => Ok(pa),
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

#[cfg(test)]
mod tests {
    use super::*;

    /// Physical memory at 4 GiB and up (nothing below), as a Jetson Nano's upper RAM.
    struct HighBus {
        base: u64,
        mem: Vec<u8>,
        accesses: usize,
    }

    impl HighBus {
        fn at(&mut self, addr: u64, n: usize) -> &mut [u8] {
            assert!(addr >= self.base, "bus access below RAM: {addr:#x}");
            self.accesses += 1;
            let o = (addr - self.base) as usize;
            &mut self.mem[o..o + n]
        }
    }

    macro_rules! bus {
        ($($r:ident $w:ident $t:ty),*) => { $(
            fn $r(&mut self, a: u64) -> Result<$t, emu_core::Fault> {
                Ok(<$t>::from_le_bytes(self.at(a, std::mem::size_of::<$t>()).try_into().unwrap()))
            }
            fn $w(&mut self, a: u64, v: $t) -> Result<(), emu_core::Fault> {
                let n = std::mem::size_of::<$t>();
                self.at(a, n).copy_from_slice(&v.to_le_bytes());
                Ok(())
            }
        )* };
    }

    impl Bus for HighBus {
        bus!(read8 write8 u8, read16 write16 u16, read32 write32 u32, read64 write64 u64);
    }

    /// Every user address maps to RAM at `base + va`.
    struct Host {
        base: u64,
    }

    impl El1Host for Host {
        fn translate(&mut self, _bus: &mut dyn Bus, va: u32, _access: El0Access) -> Result<u64, u32> {
            Ok(self.base + va as u64)
        }
        fn cp15(&mut self, _insn: u32, _value: u64) -> Cp15 {
            Cp15::Undefined
        }
    }

    /// 32-bit user code, data and page-crossing accesses in RAM above 4 GiB: the core works
    /// in RAM offsets and the bus sees the real addresses.
    #[test]
    fn user_code_in_ram_above_4_gib() {
        const BASE: u64 = 0x1_0000_0000;
        let mut mem = vec![0u8; 0x10000];
        let code: [u32; 4] = [
            0xe591_0000, // ldr r0, [r1]
            0xe280_0001, // add r0, r0, #1
            0xe581_0004, // str r0, [r1, #4]
            0xe591_2fff, // ldr r2, [r1, #0xfff] (crosses into the next page)
        ];
        for (i, w) in code.iter().enumerate() {
            mem[0x1000 + 4 * i..0x1004 + 4 * i].copy_from_slice(&w.to_le_bytes());
        }
        mem[0x2000..0x2004].copy_from_slice(&41u32.to_le_bytes());
        mem[0x2fff..0x3003].copy_from_slice(&0x1234_5678u32.to_le_bytes());
        let mut bus = HighBus { base: BASE, mem, accesses: 0 };
        let (ram, len) = (bus.mem.as_mut_ptr(), bus.mem.len() as u64);
        let mut cpu = Cpu::new_el0();
        // SAFETY: `bus.mem` outlives `cpu`
        unsafe { cpu.el0_setup(ram, BASE, len, false) };
        cpu.r[15] = 0x1000;
        cpu.r[1] = 0x2000;
        let mut host = Host { base: BASE };
        for _ in 0..4 {
            cpu.step_el0(&mut bus, &mut host).expect("no exception");
        }
        assert_eq!(cpu.r[0], 42);
        assert_eq!(cpu.r[2], 0x1234_5678);
        assert_eq!(u32::from_le_bytes(bus.mem[0x2004..0x2008].try_into().unwrap()), 42);
        assert!(bus.accesses > 0, "the slow path goes to the bus");
    }

    /// A device page a user process mapped, below RAM: the access reaches the bus at the
    /// device's own address, and RAM is untouched.
    #[test]
    fn user_mapping_of_a_device_below_ram() {
        const RAM: u64 = 0x8000_0000;
        const DEVICE: u64 = 0x7000_0000;
        struct Split {
            ram: HighBus,
            device: HighBus,
        }
        macro_rules! split {
            ($($r:ident $w:ident $t:ty),*) => { $(
                fn $r(&mut self, a: u64) -> Result<$t, emu_core::Fault> {
                    if a < RAM { self.device.$r(a) } else { self.ram.$r(a) }
                }
                fn $w(&mut self, a: u64, v: $t) -> Result<(), emu_core::Fault> {
                    if a < RAM { self.device.$w(a, v) } else { self.ram.$w(a, v) }
                }
            )* };
        }
        impl Bus for Split {
            split!(read8 write8 u8, read16 write16 u16, read32 write32 u32, read64 write64 u64);
        }
        /// The page at 0x4000 is the device, the rest RAM.
        struct Maps;
        impl El1Host for Maps {
            fn translate(&mut self, _bus: &mut dyn Bus, va: u32, _access: El0Access) -> Result<u64, u32> {
                Ok(if va >> 12 == 4 { DEVICE + (va as u64 & 0xfff) } else { RAM + va as u64 })
            }
            fn cp15(&mut self, _insn: u32, _value: u64) -> Cp15 {
                Cp15::Undefined
            }
        }
        let mut ram = vec![0u8; 0x10000];
        let code: [u32; 3] = [
            0xe591_0804, // ldr r0, [r1, #0x804]
            0xe280_0001, // add r0, r0, #1
            0xe581_0808, // str r0, [r1, #0x808]
        ];
        for (i, w) in code.iter().enumerate() {
            ram[0x1000 + 4 * i..0x1004 + 4 * i].copy_from_slice(&w.to_le_bytes());
        }
        let before = ram.clone();
        let mut device = vec![0u8; 0x1000];
        device[0x804..0x808].copy_from_slice(&0x0002_2110u32.to_le_bytes());
        let mut bus = Split {
            ram: HighBus { base: RAM, mem: ram, accesses: 0 },
            device: HighBus { base: DEVICE, mem: device, accesses: 0 },
        };
        let mut cpu = Cpu::new_el0();
        // 4 GiB claimed, as on the Nano: a 32-bit offset of the device would alias RAM. Only
        // the first 64 KiB are touched.
        // SAFETY: `bus.ram.mem` outlives `cpu`; the test accesses only its first 64 KiB
        unsafe { cpu.el0_setup(bus.ram.mem.as_mut_ptr(), RAM, 4 << 30, false) };
        cpu.r[15] = 0x1000;
        cpu.r[1] = 0x4000;
        for _ in 0..3 {
            cpu.step_el0(&mut bus, &mut Maps).expect("no exception");
        }
        assert_eq!(cpu.r[0], 0x0002_2111);
        assert_eq!(u32::from_le_bytes(bus.device.mem[0x808..0x80c].try_into().unwrap()), 0x0002_2111);
        assert_eq!(bus.ram.mem, before);
    }

    /// Compiled user code in RAM at 0x8000_0000 that the program rewrites, with a plain store
    /// and (cores on host threads) a store-exclusive: the block is dropped. The JIT's keys are
    /// RAM offsets, so `ram_key` must find the page.
    #[test]
    fn compiled_code_rewritten_in_ram_above_zero() {
        // at 0x3000: str r2, [r1]; subs r3, r3, #1; bne 0x1000; svc #0
        rewrite_compiled_code(false, &[0xe581_2000, 0xe253_3001, 0x1aff_f7fc, 0xef00_0000]);
        // at 0x3000: ldrex r4, [r1]; strex r4, r2, [r1]; subs; bne; svc
        rewrite_compiled_code(true, &[0xe191_4f9f, 0xe181_4f92, 0xe253_3001, 0x1aff_f7fb, 0xef00_0000]);
    }

    fn rewrite_compiled_code(mt: bool, store: &[u32]) {
        const BASE: u64 = 0x8000_0000;
        let mut mem = vec![0u8; 0x10000];
        let mut put = |a: usize, w: u32| mem[a..a + 4].copy_from_slice(&w.to_le_bytes());
        put(0x1000, 0xe3a0_0001); // mov r0, #1 (rewritten to #2)
        put(0x1004, 0xea00_07fd); // b 0x3000
        for (i, &w) in store.iter().enumerate() {
            put(0x3000 + 4 * i, w);
        }
        let mut bus = HighBus { base: BASE, mem, accesses: 0 };
        let mut cpu = Cpu::new_el0();
        let len = bus.mem.len() as u32;
        // SAFETY: `bus.mem` outlives `cpu`
        unsafe { cpu.el0_setup(bus.mem.as_mut_ptr(), BASE, len as u64, mt) };
        if !cpu.enable_jit(crate::jit::CodeWatch::new(0, len)) {
            return; // no JIT on this host
        }
        cpu.jit_set_hot(1);
        // first with the store going elsewhere, so the block is compiled and stays compiled;
        // then onto the block itself: the second pass must run the new instruction
        for (target, passes, expect) in [(0x2000, 8, 1), (0x1000, 2, 2)] {
            cpu.r[15] = 0x1000;
            cpu.r[1] = target;
            cpu.r[2] = 0xe3a0_0002; // mov r0, #2
            cpu.r[3] = passes;
            let ended = (0..1000).any(|_| cpu.step_el0(&mut bus, &mut Host { base: BASE }).is_err());
            assert!(ended, "the program reaches its SVC");
            assert_eq!(cpu.r[3], 0);
            assert_eq!(cpu.r[0], expect, "mt {mt}");
            assert!(cpu.jit_stats().unwrap().0 > 0, "the code is compiled");
        }
        assert!(cpu.jit_stats().unwrap().2 > 0, "the rewritten block is dropped");
    }
}
