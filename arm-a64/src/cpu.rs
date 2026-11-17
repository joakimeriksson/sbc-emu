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

/// Entries per micro-TLB (a power of two). At most 256: compiled code reaches the `Cpu`
/// fields laid out after the tables with short immediate offsets (512 breaks that).
pub(crate) const UT: usize = 256;

/// Board-selected CPU identity. ISA feature registers still advertise implemented features only.
#[derive(Clone, Copy, Debug)]
pub struct CpuProfile {
    pub midr: u64,
    pub gic_sysregs: bool,
}
impl CpuProfile {
    pub const CORTEX_A72: Self = Self { midr: 0x410f_d083, gic_sysregs: false };
    /// Reduced A55 profile: no LSE/FP16/RAS advertised until those features exist.
    pub const CORTEX_A55: Self = Self { midr: 0x412f_d050, gic_sysregs: true };
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
    /// exclusive monitor: the address of the last LDXR while `excl_armed` is 1 (plain fields,
    /// so JIT-generated code can read and write the reservation; see `excl()`)
    pub(crate) excl_addr: u64,
    /// 1 while the monitor holds a reservation (read by JIT-generated stores)
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
    /// micro-TLBs for fetch, read and write: direct-mapped on the VA page, (tag, physical page)
    pub(crate) utlb: [[(u64, u64); UT]; 3],
    /// the ASID the micro-TLBs' EL0 lower-half entries belong to (see `utlb_ttbr_written`)
    pub(crate) utlb_asid: u16,
    /// 0 while the micro-TLBs' EL0 entries belong to the current ASID, else a bit that makes
    /// unprivileged (LDTR/STTR) probes miss: Linux's software PAN parks the reserved ASID 0
    pub(crate) utlb_unpriv: u64,
    /// per address half (VA bit 55): the range [lo, hi) covered by translations cached from
    /// block descriptors since the last full flush. A block is cached as separate 4 KiB
    /// entries but invalidated with one TLBI (Linux does that for huge pages), so a by-VA
    /// TLBI inside this range flushes everything.
    pub(crate) tlb_blocks: [(u64, u64); 2],
    /// block JIT (AArch64 hosts); None = interpreter only
    pub jit: Option<Box<crate::jit::Jit>>,
    /// Physical pages any core of this machine has compiled code from (one bit each, shared by
    /// the cores, never cleared). IC IVAU on a page without its bit drops nothing anywhere, so it
    /// skips the cross-core broadcast. None: every IC IVAU is broadcast.
    pub code_pages: Option<std::sync::Arc<[std::sync::atomic::AtomicU64]>>,
    pub(crate) jit_bus: Option<*mut dyn emu_core::Bus>,
    pub(crate) jit_trap: Option<Trap>,
    /// JIT verify mode: (RAM offset, previous bytes) of every store, for rollback
    pub(crate) store_log: Option<Vec<(usize, Vec<u8>)>>,
    /// Block chaining (native JIT): compiled blocks jump straight into their successors while
    /// `instret < chain_until` (0, the default, never chains); the caller sets it to the next
    /// instant it must look at the machine (budget end, device deadline).
    pub chain_until: u64,
    /// set when an instruction run inside a block may need the machine's attention (device
    /// access, system register, exception level change, ...): the chain stops after the block
    pub(crate) chain_stop: bool,
    /// the native JIT's block dispatcher (code address)
    pub(crate) jit_disp: u64,
    /// the last `Trap::Wait` came from `wfe` (true) or `wfi` (false)
    pub last_wfe: bool,
    /// multi-threaded run: cores execute in parallel on shared memory, so barriers and
    /// acquire/release become host fences and store-exclusive a compare-and-swap
    pub mt: bool,
    /// multi-threaded run: virtual time is the host's monotonic clock since `.0` plus `.1` ns,
    /// the same for every core (instruction counts would let a fast core's clock run ahead)
    pub wall_clock: Option<(std::time::Instant, i64)>,
    /// value and size the last load-exclusive read (the compare value for `mt` store-exclusive)
    pub(crate) excl_val: u128,
    /// an instruction-cache invalidation ran (a physical page, or u64::MAX for all): other
    /// cores with their own JIT must drop that code too (the machine takes it)
    pub ic_bcast: Option<u64>,
    /// the optional Cryptographic Extension (AES, SHA-1, SHA-256, PMULL): off on a Pi 4
    pub crypto: bool,
    /// debugging: run the interpreter only (breakpoints and single steps see every instruction)
    pub jit_off: bool,
    /// the interpreter is running a block that is not hot yet, up to its end
    pub(crate) interp_run: bool,
    /// debugger watchpoints: (address, length, kind: 1 write, 2 read, 3 access); interpreter
    /// only (`jit_off`)
    pub watch: Vec<(u64, u64, u8)>,
    /// the access that hit a watchpoint: (address, kind of the access)
    pub watch_hit: Option<(u64, u8)>,
    /// an inner-shareable TLBI ran: the other cores must do the same (`Cpu::tlb_invalidate`
    /// operand: a VA, or u64::MAX for everything); the machine takes it
    pub tlbi_bcast: Option<u64>,
    /// Immutable board configuration; existing Pi snapshots retain their original format.
    /// Boards must reconstruct this profile before loading architectural state.
    pub profile: CpuProfile,
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
            profile: CpuProfile::CORTEX_A72,
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
            excl_addr: 0,
            excl_armed: 0,
            brk_to_host: false,
            firmware_calls_to_host: false,
            cur_insn: 0,
            tlb: None, ram: std::ptr::null_mut(), ram_len: 0, utlb: [[(0, 0); UT]; 3], utlb_asid: 0, utlb_unpriv: 0, tlb_blocks: [(u64::MAX, 0); 2], jit: None, code_pages: None, jit_bus: None, jit_trap: None, store_log: None, chain_until: 0, chain_stop: false, jit_disp: 0, last_wfe: false, tlbi_bcast: None, jit_off: false, interp_run: false, watch: Vec::new(), watch_hit: None, crypto: false, mt: false, wall_clock: None, excl_val: 0, ic_bcast: None,
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
                self.interp_run = false;
                return Err(t);
            }
        }
        if self.jit.is_some() && !self.jit_off {
            if !self.interp_run {
                if let Some(r) = self.jit_step(bus) {
                    return r;
                }
            }
            // interpreting a cold block: until a branch (or an exception) leaves the straight line
            let pc = self.pc;
            let r = self.exec_one(bus);
            if self.pc != pc.wrapping_add(4) || r.is_err() { self.interp_run = false; }
            return r;
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
        w.bool(self.irq); w.bool(self.fiq); w.u64(self.instret); w.opt_u64(self.excl());
        w.bool(self.brk_to_host); w.bool(self.firmware_calls_to_host);
        let s = &self.sys;
        for v in [s.mpidr, s.cpacr_el1, s.hcr_el2, s.scr_el3, s.ttbr1_el1, s.par_el1, s.cntfrq, s.cnt, s.time_base_ns, s.time_base_instret, s.cntvoff,
                  s.ptimer.ctl, s.ptimer.cval, s.vtimer.ctl, s.vtimer.cval] { w.u64(v); }
        for v in s.ttbr0 { w.u64(v); }
        for v in s.tcr { w.u64(v); }
        let mut other = s.other.clone();
        for (k, v) in [crate::sysreg::TPIDR_EL0, crate::sysreg::TPIDRRO_EL0, crate::sysreg::TPIDR_EL1].into_iter().zip(s.tpidr) { other.insert(k, v); }
        w.u64(other.len() as u64);
        for (k, v) in &other { w.u32(*k); w.u64(*v); }
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
        for (n, k) in [crate::sysreg::TPIDR_EL0, crate::sysreg::TPIDRRO_EL0, crate::sysreg::TPIDR_EL1].into_iter().enumerate() { s.tpidr[n] = s.other.remove(&k).unwrap_or(0); }
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
        if std::env::var("SBC_JIT").is_ok_and(|v| v == "1") { self.jit = crate::jit::Jit::with(1 << 20, 1); self.chain_until = u64::MAX; }
    }
}

impl Cpu {
    /// Drop the exclusive monitor's reservation (another core may have written the location).
    pub fn clear_exclusive(&mut self) { self.set_excl(None); }

    /// Hand the JIT (code cache and dispatcher) to another core of the same machine: compiled
    /// code only reaches core state through the pointer it is called with, so one cache serves
    /// all cores.
    pub fn hand_jit_to(&mut self, other: &mut Cpu) {
        if other.jit.is_some() { return; } // it has its own (multi-threaded runs)
        if let Some(j) = self.jit.take() { other.jit = Some(j); other.jit_disp = self.jit_disp; }
    }

    /// Address of `chain_until`: another thread may store 0 there (atomically) to make this core
    /// leave its compiled chain at the next block boundary.
    pub fn chain_until_ptr(&mut self) -> *mut u64 { &mut self.chain_until }

    /// A bitmap for `code_pages` covering `ram_len` bytes of RAM.
    pub fn code_page_map(ram_len: u64) -> std::sync::Arc<[std::sync::atomic::AtomicU64]> {
        (0..(ram_len >> 12).div_ceil(64)).map(|_| std::sync::atomic::AtomicU64::new(0)).collect()
    }

    /// Record that code is about to be compiled from physical page `page` (before its bytes are
    /// read: an IC IVAU that finds the bit clear came before that read, see `code_page_used`).
    pub(crate) fn mark_code_page(&self, page: u64) {
        if let Some(m) = self.code_pages.as_ref() {
            if let Some(w) = m.get((page / 64) as usize) { w.fetch_or(1 << (page % 64), std::sync::atomic::Ordering::SeqCst); }
        }
    }

    /// Could any core hold code compiled from physical page `page`?
    pub(crate) fn code_page_used(&self, page: u64) -> bool {
        match self.code_pages.as_ref() {
            Some(m) => m.get((page / 64) as usize).map_or(true, |w| w.load(std::sync::atomic::Ordering::SeqCst) >> (page % 64) & 1 != 0),
            None => true,
        }
    }

    /// Another core invalidated instruction cache: `page` (physical page number) or u64::MAX.
    pub fn jit_invalidate(&mut self, page: u64) {
        if let Some(j) = self.jit.as_mut() { if page == u64::MAX { j.flush() } else { j.invalidate_page(page) } }
    }

    #[inline]
    pub(crate) fn set_excl(&mut self, e: Option<u64>) { self.excl_addr = e.unwrap_or(0); self.excl_armed = e.is_some() as u64; }
    /// The exclusive monitor's reservation: Some(address) after LDXR.
    #[inline]
    pub fn excl(&self) -> Option<u64> { (self.excl_armed != 0).then_some(self.excl_addr) }
}
