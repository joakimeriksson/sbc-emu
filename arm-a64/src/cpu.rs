use emu_core::{Bus, Core, Trap};

/// Exception classes we raise (ESR_ELx.EC).
pub struct Exc;
impl Exc {
    pub const UNKNOWN: u32 = 0x00;
    pub const WFX: u32 = 0x01;
    pub const FP: u32 = 0x07;
    pub const ILLEGAL: u32 = 0x0e;
    pub const SVC: u32 = 0x15;
    pub const HVC: u32 = 0x16;
    pub const SMC: u32 = 0x17;
    pub const SYSREG: u32 = 0x18;
    pub const IABT_LOWER: u32 = 0x20;
    pub const IABT_SAME: u32 = 0x21;
    pub const PC_ALIGN: u32 = 0x22;
    pub const DABT_LOWER: u32 = 0x24;
    pub const DABT_SAME: u32 = 0x25;
    pub const SP_ALIGN: u32 = 0x26;
    pub const BRK: u32 = 0x3c;
}

pub struct Cpu {
    pub x: [u64; 31],
    /// SP_EL0..SP_EL3
    pub sp: [u64; 4],
    pub pc: u64,
    /// N Z C V in bits 31..28, as in the NZCV register
    pub nzcv: u32,
    /// D A I F in bits 9..6, as in the DAIF register
    pub daif: u32,
    pub el: u8,
    /// SPSel: true = SP_ELx, false = SP_EL0
    pub spsel: bool,
    pub elr: [u64; 4],
    pub spsr: [u64; 4],
    pub esr: [u64; 4],
    pub far: [u64; 4],
    pub vbar: [u64; 4],
    pub sctlr: [u64; 4],
    pub v: [u128; 32],
    pub fpcr: u64,
    pub fpsr: u64,
    pub sys: crate::sysreg::SysRegs,
    pub irq: bool,
    pub fiq: bool,
    pub instret: u64,
    /// exclusive monitor: Some(address) after LDXR
    pub excl: Option<u64>,
    /// 1 while `excl` holds a reservation (read by JIT-generated stores)
    pub excl_armed: u64,
    /// `brk` returns Trap::Brk instead of taking an exception (bare-metal tests)
    pub brk_to_host: bool,
    /// `smc` (and `hvc`) go to the host as firmware calls instead of EL3/EL2 vectors
    pub firmware_calls_to_host: bool,
    pub(crate) cur_insn: u32,
    pub(crate) tlb: Option<Box<[crate::mmu::TlbEntry]>>,
    /// Host pointer to guest RAM at physical address 0 (and its length): set by the machine so
    /// RAM accesses can bypass the bus. The machine guarantees the buffer outlives the core and
    /// is never moved or resized.
    pub ram: *mut u8,
    pub ram_len: u64,
    /// micro-TLBs for the RAM fast path: [fetch, read, write] = (tag, physical page)
    pub(crate) utlb: [(u64, u64); 3],
    /// block JIT (AArch64 hosts); None = interpreter only
    pub jit: Option<Box<crate::jit::Jit>>,
    pub(crate) jit_bus: Option<*mut dyn emu_core::Bus>,
    pub(crate) jit_trap: Option<Trap>,
    /// JIT verify mode: (RAM offset, previous bytes) of every store, for rollback
    pub(crate) store_log: Option<Vec<(usize, Vec<u8>)>>,
}

// SAFETY: `ram` is only dereferenced by the core that owns it; see the field documentation.
unsafe impl Send for Cpu {}

impl Default for Cpu {
    fn default() -> Self {
        Self::new()
    }
}

impl Cpu {
    /// A core out of reset at EL3, MMU off, all interrupts masked.
    pub fn new() -> Self {
        Cpu {
            x: [0; 31],
            sp: [0; 4],
            pc: 0,
            nzcv: 0,
            daif: 0xf << 6,
            el: 3,
            spsel: true,
            elr: [0; 4],
            spsr: [0; 4],
            esr: [0; 4],
            far: [0; 4],
            vbar: [0; 4],
            sctlr: [0x30c5_0838; 4],
            v: [0; 32],
            fpcr: 0,
            fpsr: 0,
            sys: crate::sysreg::SysRegs::default(),
            irq: false,
            fiq: false,
            instret: 0,
            excl: None,
            excl_armed: 0,
            brk_to_host: false,
            firmware_calls_to_host: false,
            cur_insn: 0,
            tlb: None, ram: std::ptr::null_mut(), ram_len: 0, utlb: [(0, 0); 3], jit: None, jit_bus: None, jit_trap: None, store_log: None,
        }
    }

    #[inline]
    pub fn xr(&self, r: u32) -> u64 {
        if r == 31 {
            0
        } else {
            self.x[r as usize]
        }
    }
    #[inline]
    pub fn set_x(&mut self, r: u32, v: u64) {
        if r != 31 {
            self.x[r as usize] = v;
        }
    }
    #[inline]
    fn sp_index(&self) -> usize {
        if self.spsel {
            self.el as usize
        } else {
            0
        }
    }
    #[inline]
    pub fn cur_sp(&self) -> u64 {
        self.sp[self.sp_index()]
    }
    #[inline]
    pub fn set_cur_sp(&mut self, v: u64) {
        let i = self.sp_index();
        self.sp[i] = v;
    }
    /// register-or-SP read (r = 31 means SP)
    #[inline]
    pub fn xsp(&self, r: u32) -> u64 {
        if r == 31 {
            self.cur_sp()
        } else {
            self.x[r as usize]
        }
    }
    #[inline]
    pub fn set_xsp(&mut self, r: u32, v: u64) {
        if r == 31 {
            self.set_cur_sp(v)
        } else {
            self.x[r as usize] = v
        }
    }

    /// PSTATE packed as SPSR (AArch64 state).
    pub fn pstate(&self) -> u64 {
        (self.nzcv as u64) | (self.daif as u64) | ((self.el as u64) << 2) | (self.spsel as u64)
    }
    pub fn set_pstate(&mut self, v: u64) {
        self.nzcv = (v as u32) & 0xf000_0000;
        self.daif = (v as u32) & 0x3c0;
        self.el = ((v >> 2) & 3) as u8;
        self.spsel = v & 1 != 0;
    }

    pub fn mmu_on(&self) -> bool {
        self.sctlr[self.el.max(1) as usize] & 1 != 0
    }

    /// Highest implemented EL that an exception from `el` may target for a lower-EL routing.
    /// Take an exception to `target` EL. `ret` is the preferred return address; `vec` is the
    /// vector offset within a group (0 sync, 0x80 IRQ, 0x100 FIQ, 0x180 SError).
    pub fn take_exception(&mut self, target: u8, vec: u64, ret: u64) {
        let t = target as usize;
        let group = if target > self.el {
            0x400
        } else if self.spsel {
            0x200
        } else {
            0
        };
        self.spsr[t] = self.pstate();
        self.elr[t] = ret;
        self.el = target;
        self.spsel = true;
        self.daif = 0xf << 6;
        self.set_excl(None);
        self.pc = self.vbar[t].wrapping_add(group + vec);
    }

    /// Synchronous exception with syndrome. Routes EL0 exceptions to EL1.
    pub fn sync_exception(&mut self, ec: u32, iss: u32, ret: u64, far: Option<u64>) -> Trap {
        self.sync_exception_to(self.el.max(1), ec, iss, ret, far)
    }
    pub fn sync_exception_to(
        &mut self,
        target: u8,
        ec: u32,
        iss: u32,
        ret: u64,
        far: Option<u64>,
    ) -> Trap {
        let t = target as usize;
        // IL = 1: 32-bit instruction
        self.esr[t] = ((ec as u64) << 26) | (1 << 25) | (iss as u64 & 0x1ff_ffff);
        if let Some(f) = far {
            self.far[t] = f;
        }
        self.take_exception(target, 0, ret);
        Trap::Exception(ec, target)
    }

    pub fn undefined(&mut self) -> Trap {
        self.sync_exception(Exc::UNKNOWN, 0, self.pc, None)
    }

    fn check_interrupts(&mut self) -> Option<Trap> {
        // FIQ first, then IRQ. Without SCR routing interrupts go to EL1 (or stay at EL2/EL3).
        let scr = self.sys.scr_el3;
        for (kind, pending, mask_bit, route) in [
            (1u8, self.fiq, 6, scr & 4 != 0),
            (0u8, self.irq, 7, scr & 2 != 0),
        ] {
            if !pending {
                continue;
            }
            let target = if route { 3 } else { self.el.max(1) };
            let masked = target == self.el && self.daif & (1 << mask_bit) != 0;
            if target < self.el || masked {
                continue;
            }
            let vec = if kind == 1 { 0x100 } else { 0x80 };
            self.take_exception(target, vec, self.pc);
            return Some(Trap::Interrupt(kind, target));
        }
        None
    }
}

impl Core for Cpu {
    fn step(&mut self, bus: &mut dyn Bus) -> Result<(), Trap> {
        if self.irq || self.fiq {
            if let Some(t) = self.check_interrupts() {
                return Err(t);
            }
        }
        if self.jit.is_some() {
            if let Some(r) = self.jit_step(bus) {
                return r;
            }
        }
        self.exec_one(bus)
    }
    fn pc(&self) -> u64 {
        self.pc
    }
    fn instret(&self) -> u64 {
        self.instret
    }
    fn set_irq(&mut self, irq: bool, fiq: bool) {
        self.irq = irq;
        self.fiq = fiq;
    }
}

impl Cpu {
    /// Architectural state for a snapshot (TLBs are rebuilt on demand and not saved).
    pub fn save_state(&self, w: &mut emu_core::snap::W) {
        w.tag("cpu-v1");
        for v in self.x { w.u64(v); }
        for v in self.sp { w.u64(v); }
        w.u64(self.pc); w.u32(self.nzcv); w.u32(self.daif); w.u8(self.el); w.bool(self.spsel);
        for a in [&self.elr, &self.spsr, &self.esr, &self.far, &self.vbar, &self.sctlr] { for v in a { w.u64(*v); } }
        for v in self.v { w.u128(v); }
        w.u64(self.fpcr); w.u64(self.fpsr);
        w.bool(self.irq); w.bool(self.fiq); w.u64(self.instret); w.opt_u64(self.excl);
        w.bool(self.brk_to_host); w.bool(self.firmware_calls_to_host);
        let s = &self.sys;
        for v in [s.mpidr, s.cpacr_el1, s.hcr_el2, s.scr_el3, s.ttbr1_el1, s.par_el1, s.cntfrq, s.cnt, s.time_base_ns, s.time_base_instret, s.cntvoff,
                  s.ptimer.ctl, s.ptimer.cval, s.vtimer.ctl, s.vtimer.cval] { w.u64(v); }
        for v in s.ttbr0 { w.u64(v); }
        for v in s.tcr { w.u64(v); }
        w.u64(s.other.len() as u64);
        for (k, v) in &s.other { w.u32(*k); w.u64(*v); }
    }

    pub fn load_state(&mut self, r: &mut emu_core::snap::R) -> Result<(), emu_core::snap::SnapError> {
        r.tag("cpu-v1")?;
        for v in self.x.iter_mut() { *v = r.u64()?; }
        for v in self.sp.iter_mut() { *v = r.u64()?; }
        self.pc = r.u64()?; self.nzcv = r.u32()?; self.daif = r.u32()?; self.el = r.u8()?; self.spsel = r.bool()?;
        for a in [&mut self.elr, &mut self.spsr, &mut self.esr, &mut self.far, &mut self.vbar, &mut self.sctlr] { for v in a.iter_mut() { *v = r.u64()?; } }
        for v in self.v.iter_mut() { *v = r.u128()?; }
        self.fpcr = r.u64()?; self.fpsr = r.u64()?;
        self.irq = r.bool()?; self.fiq = r.bool()?; self.instret = r.u64()?; { let e = r.opt_u64()?; self.set_excl(e); }
        self.brk_to_host = r.bool()?; self.firmware_calls_to_host = r.bool()?;
        let s = &mut self.sys;
        for v in [&mut s.mpidr, &mut s.cpacr_el1, &mut s.hcr_el2, &mut s.scr_el3, &mut s.ttbr1_el1, &mut s.par_el1, &mut s.cntfrq, &mut s.cnt,
                  &mut s.time_base_ns, &mut s.time_base_instret, &mut s.cntvoff, &mut s.ptimer.ctl, &mut s.ptimer.cval, &mut s.vtimer.ctl, &mut s.vtimer.cval] { *v = r.u64()?; }
        for v in s.ttbr0.iter_mut() { *v = r.u64()?; }
        for v in s.tcr.iter_mut() { *v = r.u64()?; }
        s.other.clear();
        for _ in 0..r.u64()? { let k = r.u32()?; let v = r.u64()?; s.other.insert(k, v); }
        s.timers_changed = true;
        self.tlb_flush();
        if let Some(j) = self.jit.as_mut() { j.flush(); }
        Ok(())
    }
}

impl Cpu {
    /// Turn on the block JIT where the host supports it; returns whether it is active.
    pub fn enable_jit(&mut self) -> bool {
        if self.jit.is_none() { self.jit = crate::jit::Jit::new(); }
        self.jit.is_some()
    }

    /// Tests: a small code buffer and compile-on-first-use, when `SBC_JIT=1` is set.
    pub fn enable_test_jit_from_env(&mut self) {
        if std::env::var("SBC_JIT").is_ok_and(|v| v == "1") { self.jit = crate::jit::Jit::with(1 << 20, 1); }
    }
}

impl Cpu {
    #[inline]
    pub(crate) fn set_excl(&mut self, e: Option<u64>) { self.excl = e; self.excl_armed = e.is_some() as u64; }
}
