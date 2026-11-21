//! An RV64GC hart for sbc-emu's RISC-V boards: RV64IMAFDC with Zicsr, Zifencei, Zicntr,
//! Zba and Zbb (the SiFive U74's ISA, `rv64imafdc_zba_zbb`), machine, supervisor and user
//! modes (privileged ISA 1.11, no H, no Sstc), and Sv39 virtual memory.
//!
//! The hart is an interpreter. A board owns the harts and implements [`Bus`]: physical
//! memory, the RAM window for fast loads, stores and fetches, and the `time` CSR (the
//! board's `mtime`). Interrupt lines from the board's CLINT and PLIC are set in
//! [`Hart::irq_lines`] (`MSIP`, `MTIP`, `MEIP`, `SEIP`).
//!
//! Floating point uses the host's IEEE 754 arithmetic (round to nearest, ties to even)
//! with RISC-V NaN boxing and canonical NaNs; exception flags and the other rounding
//! modes are derived from exact error terms (see `fpu.rs`).

mod exec;
mod fpu;
pub mod jit;
pub mod jit_check;
#[cfg(all(target_arch = "aarch64", any(target_os = "macos", target_os = "linux")))]
mod jit_a64;
#[cfg(target_arch = "wasm32")]
mod jit_wasm;
pub mod mmu;
pub mod rvc;

pub use mmu::Access;

/// Physical memory and the platform timer as a hart sees them.
#[allow(clippy::result_unit_err)]
pub trait Bus {
    /// Base physical address of main RAM.
    fn ram_base(&self) -> u64;
    /// Main RAM, for direct loads, stores and instruction fetch.
    fn ram(&mut self) -> &mut [u8];
    /// A device (or non-RAM memory) access of 1, 2, 4 or 8 bytes. `Err` is an access fault.
    fn read(&mut self, addr: u64, size: u8) -> Result<u64, ()>;
    fn write(&mut self, addr: u64, size: u8, value: u64) -> Result<(), ()>;
    /// The platform's `mtime`, which the `time` CSR reads.
    fn mtime(&mut self) -> u64;
    /// New interrupt lines for the running hart after one of its own device accesses
    /// changed them (a CLINT or PLIC write, a UART read), checked before each instruction.
    #[inline(always)]
    fn irq_update(&mut self) -> Option<u64> {
        None
    }
}

pub mod prv {
    pub const U: u64 = 0;
    pub const S: u64 = 1;
    pub const M: u64 = 3;
}

/// Interrupt bits of `mip`/`mie` (and exception/interrupt cause numbers).
pub mod irq {
    pub const SSIP: u64 = 1 << 1;
    pub const MSIP: u64 = 1 << 3;
    pub const STIP: u64 = 1 << 5;
    pub const MTIP: u64 = 1 << 7;
    pub const SEIP: u64 = 1 << 9;
    pub const MEIP: u64 = 1 << 11;
}

pub mod exc {
    pub const INSN_MISALIGNED: u64 = 0;
    pub const INSN_ACCESS: u64 = 1;
    pub const ILLEGAL: u64 = 2;
    pub const BREAKPOINT: u64 = 3;
    pub const LOAD_MISALIGNED: u64 = 4;
    pub const LOAD_ACCESS: u64 = 5;
    pub const STORE_MISALIGNED: u64 = 6;
    pub const STORE_ACCESS: u64 = 7;
    pub const ECALL_U: u64 = 8;
    pub const ECALL_S: u64 = 9;
    pub const ECALL_M: u64 = 11;
    pub const INSN_PAGE: u64 = 12;
    pub const LOAD_PAGE: u64 = 13;
    pub const STORE_PAGE: u64 = 15;
}

pub mod mstatus {
    pub const SIE: u64 = 1 << 1;
    pub const MIE: u64 = 1 << 3;
    pub const SPIE: u64 = 1 << 5;
    pub const MPIE: u64 = 1 << 7;
    pub const SPP: u64 = 1 << 8;
    pub const MPP: u64 = 3 << 11;
    pub const FS: u64 = 3 << 13;
    pub const MPRV: u64 = 1 << 17;
    pub const SUM: u64 = 1 << 18;
    pub const MXR: u64 = 1 << 19;
    pub const TVM: u64 = 1 << 20;
    pub const TW: u64 = 1 << 21;
    pub const TSR: u64 = 1 << 22;
    /// UXL = SXL = 2 (64-bit), read-only
    pub const XL: u64 = (2 << 32) | (2 << 34);
    pub const SD: u64 = 1 << 63;
    pub const M_WRITABLE: u64 = SIE | MIE | SPIE | MPIE | SPP | MPP | FS | MPRV | SUM | MXR | TVM | TW | TSR;
    pub const S_VIEW: u64 = SIE | SPIE | SPP | FS | SUM | MXR | XL | SD;
}

/// A synchronous exception: cause and `xtval`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Exception {
    pub cause: u64,
    pub tval: u64,
}

impl Exception {
    pub fn illegal(insn: u32) -> Self {
        Exception { cause: exc::ILLEGAL, tval: insn as u64 }
    }
}

pub(crate) type R<T> = Result<T, Exception>;

/// Identity of a hart (the `m*id` CSRs and `misa`).
#[derive(Clone, Copy, Debug)]
pub struct Identity {
    pub hartid: u64,
    pub mvendorid: u64,
    pub marchid: u64,
    pub mimpid: u64,
}

impl Identity {
    /// A SiFive U74-MC core as in the StarFive JH7110 (values from the JH7110's harts).
    pub fn sifive_u74(hartid: u64) -> Self {
        Identity { hartid, mvendorid: 0x489, marchid: 0x8000_0000_0000_0007, mimpid: 0x0421_0427 }
    }
}

/// `misa` of an RV64IMAFDC hart with S and U modes.
pub const MISA_RV64GC_SU: u64 = (2 << 62)
    | (1 << 0)   // A
    | (1 << 2)   // C
    | (1 << 3)   // D
    | (1 << 5)   // F
    | (1 << 8)   // I
    | (1 << 12)  // M
    | (1 << 18)  // S
    | (1 << 20); // U

pub struct Hart {
    pub x: [u64; 32],
    pub f: [u64; 32],
    pub pc: u64,
    pub prv: u64,
    pub id: Identity,
    pub misa: u64,

    pub mstatus: u64,
    pub mie: u64,
    /// the software-writable `mip` bits (SSIP, STIP, SEIP)
    pub mip_sw: u64,
    /// interrupt lines from the platform: MSIP, MTIP, MEIP, SEIP
    pub irq_lines: u64,
    pub mideleg: u64,
    pub medeleg: u64,
    pub mtvec: u64,
    pub mscratch: u64,
    pub mepc: u64,
    pub mcause: u64,
    pub mtval: u64,
    pub mcounteren: u64,
    pub mcountinhibit: u64,
    pub stvec: u64,
    pub sscratch: u64,
    pub sepc: u64,
    pub scause: u64,
    pub stval: u64,
    pub satp: u64,
    pub scounteren: u64,
    /// frm (bits 7:5) and fflags (bits 4:0)
    pub fcsr: u64,
    pub pmpcfg: [u64; 2],
    pub pmpaddr: [u64; 16],

    pub cycle: u64,
    pub instret: u64,
    /// waiting in WFI for an interrupt
    pub wfi: bool,
    /// LR reservation (physical address), or `u64::MAX`
    pub reservation: u64,
    pub tlb: mmu::Tlb,
    /// fetch fast path: virtual page of `pc` and its RAM offset (valid when `fetch_page` != MAX)
    fetch_page: u64,
    fetch_off: usize,
    /// traps taken (exceptions and interrupts), for reports
    pub traps: u64,
    /// `fence.i` instructions executed, for reports
    pub fence_i: u64,
    /// the last instruction went to a device (not RAM): the block JIT leaves after it
    pub dev_access: bool,
    /// for compiled code (see `jit.rs`): the micro-TLB table of the current mode (host
    /// address), the bus and the `h_step` for it, and the cycle count up to which compiled
    /// regions may loop
    pub jit_ut: usize,
    pub jit_bus: usize,
    pub jit_step: usize,
    pub chain_until: u64,
    /// `fence_i` when the JIT last looked
    pub jit_fence_seen: u64,
    /// the last illegal instruction (pc, encoding), for reports
    pub last_illegal: Option<(u64, u32)>,
}

impl Hart {
    pub fn new(id: Identity) -> Self {
        Hart {
            x: [0; 32],
            f: [0; 32],
            pc: 0,
            prv: prv::M,
            id,
            misa: MISA_RV64GC_SU,
            mstatus: mstatus::XL,
            mie: 0,
            mip_sw: 0,
            irq_lines: 0,
            mideleg: 0,
            medeleg: 0,
            mtvec: 0,
            mscratch: 0,
            mepc: 0,
            mcause: 0,
            mtval: 0,
            mcounteren: 0,
            mcountinhibit: 0,
            stvec: 0,
            sscratch: 0,
            sepc: 0,
            scause: 0,
            stval: 0,
            satp: 0,
            scounteren: 0,
            fcsr: 0,
            pmpcfg: [0; 2],
            pmpaddr: [0; 16],
            cycle: 0,
            instret: 0,
            wfi: false,
            reservation: u64::MAX,
            tlb: mmu::Tlb::default(),
            fetch_page: u64::MAX,
            fetch_off: 0,
            traps: 0,
            fence_i: 0,
            dev_access: false,
            jit_ut: 0,
            jit_bus: 0,
            jit_step: 0,
            chain_until: 0,
            jit_fence_seen: 0,
            last_illegal: None,
        }
    }

    /// Reset to machine mode at `pc` (registers other than pc and the mode are kept).
    pub fn reset(&mut self, pc: u64) {
        let id = self.id;
        *self = Hart::new(id);
        self.pc = pc;
    }

    #[inline(always)]
    pub fn mip(&self) -> u64 {
        self.mip_sw | self.irq_lines
    }

    /// Whether an enabled-or-not interrupt is pending (what ends WFI).
    #[inline(always)]
    pub fn interrupt_pending(&self) -> bool {
        self.mip() & self.mie != 0
    }

    pub(crate) fn invalidate_fetch(&mut self) {
        self.fetch_page = u64::MAX;
    }

    /// Execute up to `budget` instructions. Returns the number retired (or attempted);
    /// stops early when the hart waits in WFI.
    pub fn run<B: Bus>(&mut self, bus: &mut B, budget: u64) -> u64 {
        let mut n = 0;
        while n < budget {
            if let Some(lines) = bus.irq_update() {
                self.irq_lines = lines;
            }
            if self.mip() & self.mie != 0 {
                // WFI ends on any pending enabled interrupt, even one masked globally
                self.wfi = false;
                self.take_interrupt();
            }
            if self.wfi {
                break;
            }
            self.step(bus);
            n += 1;
        }
        n
    }

    /// Fetch, decode and execute one instruction (taking any exception it raises).
    #[inline]
    pub fn step<B: Bus>(&mut self, bus: &mut B) {
        let pc = self.pc;
        let r = match self.fetch(bus, pc) {
            Ok(insn) => {
                if insn & 3 == 3 {
                    self.exec(bus, insn, 4)
                } else {
                    let full = rvc::expand(insn as u16);
                    if full == 0 {
                        Err(Exception::illegal(insn & 0xffff))
                    } else {
                        self.exec(bus, full, 2)
                    }
                }
            }
            Err(e) => Err(e),
        };
        match r {
            Ok(()) => {
                self.instret = self.instret.wrapping_add(1);
                self.cycle = self.cycle.wrapping_add(1);
            }
            Err(e) => {
                self.cycle = self.cycle.wrapping_add(1);
                if e.cause == exc::ILLEGAL {
                    self.last_illegal = Some((pc, e.tval as u32));
                }
                self.pc = pc;
                self.trap(e.cause, e.tval, false);
            }
        }
    }

    /// Fetch the (possibly compressed) instruction at `pc`; the upper 16 bits are only
    /// meaningful for a 32-bit instruction.
    #[inline(always)]
    fn fetch<B: Bus>(&mut self, bus: &mut B, pc: u64) -> R<u32> {
        let page = pc >> 12;
        let off = (pc & 0xfff) as usize;
        if page == self.fetch_page && off <= 0xffc {
            let ram = bus.ram();
            let o = self.fetch_off + off;
            return Ok(u32::from_le_bytes(ram[o..o + 4].try_into().unwrap()));
        }
        self.fetch_slow(bus, pc)
    }

    #[cold]
    fn fetch_slow<B: Bus>(&mut self, bus: &mut B, pc: u64) -> R<u32> {
        if pc & 1 != 0 {
            return Err(Exception { cause: exc::INSN_MISALIGNED, tval: pc });
        }
        let lo = self.fetch16(bus, pc)?;
        if lo & 3 != 3 {
            return Ok(lo as u32);
        }
        let hi = self.fetch16(bus, pc + 2)?;
        Ok(lo as u32 | (hi as u32) << 16)
    }

    fn fetch16<B: Bus>(&mut self, bus: &mut B, va: u64) -> R<u16> {
        let pa = self.translate(bus, va, Access::Fetch).map_err(|cause| Exception { cause, tval: va })?;
        let base = bus.ram_base();
        let ram = bus.ram();
        let o = pa.wrapping_sub(base);
        if o < ram.len() as u64 {
            let page_off = (o as usize) & !0xfff;
            if page_off + 0x1000 <= ram.len() {
                self.fetch_page = va >> 12;
                self.fetch_off = page_off;
            }
            let o = o as usize;
            return Ok(u16::from_le_bytes([ram[o], ram[o + 1]]));
        }
        bus.read(pa, 2).map(|v| v as u16).map_err(|_| Exception { cause: exc::INSN_ACCESS, tval: va })
    }

    // ---- traps and interrupts ----

    /// Take an exception (`interrupt` false) or interrupt with `cause` at the current pc.
    pub fn trap(&mut self, cause: u64, tval: u64, interrupt: bool) {
        self.traps += 1;
        self.invalidate_fetch();
        let deleg = if interrupt { self.mideleg } else { self.medeleg };
        let code = if interrupt { cause | 1 << 63 } else { cause };
        if self.prv <= prv::S && (deleg >> cause) & 1 != 0 {
            self.scause = code;
            self.sepc = self.pc;
            self.stval = tval;
            let s = self.mstatus;
            let mut s2 = s & !(mstatus::SPIE | mstatus::SIE | mstatus::SPP);
            if s & mstatus::SIE != 0 {
                s2 |= mstatus::SPIE;
            }
            if self.prv == prv::S {
                s2 |= mstatus::SPP;
            }
            self.mstatus = s2;
            self.prv = prv::S;
            self.pc = if interrupt && self.stvec & 3 == 1 { (self.stvec & !3) + 4 * cause } else { self.stvec & !3 };
        } else {
            self.mcause = code;
            self.mepc = self.pc;
            self.mtval = tval;
            let s = self.mstatus;
            let mut s2 = s & !(mstatus::MPIE | mstatus::MIE | mstatus::MPP);
            if s & mstatus::MIE != 0 {
                s2 |= mstatus::MPIE;
            }
            s2 |= self.prv << 11;
            self.mstatus = s2;
            self.prv = prv::M;
            self.pc = if interrupt && self.mtvec & 3 == 1 { (self.mtvec & !3) + 4 * cause } else { self.mtvec & !3 };
        }
    }

    /// Take the highest-priority enabled pending interrupt, if any.
    pub fn take_interrupt(&mut self) -> bool {
        let pending = self.mip() & self.mie;
        if pending == 0 {
            return false;
        }
        let m_on = self.prv < prv::M || self.mstatus & mstatus::MIE != 0;
        let m = pending & !self.mideleg;
        let sel = if m_on && m != 0 {
            m
        } else {
            let s_on = self.prv < prv::S || (self.prv == prv::S && self.mstatus & mstatus::SIE != 0);
            let s = pending & self.mideleg;
            if s_on && s != 0 {
                s
            } else {
                return false;
            }
        };
        const ORDER: [u64; 6] = [11, 3, 7, 9, 1, 5];
        let cause = ORDER.into_iter().find(|&c| sel & (1 << c) != 0).unwrap_or(sel.trailing_zeros() as u64);
        self.trap(cause, 0, true);
        true
    }

    fn mret(&mut self) {
        let s = self.mstatus;
        let mpp = (s & mstatus::MPP) >> 11;
        let mut s2 = s & !(mstatus::MIE | mstatus::MPP);
        if s & mstatus::MPIE != 0 {
            s2 |= mstatus::MIE;
        }
        s2 |= mstatus::MPIE;
        if mpp != prv::M {
            s2 &= !mstatus::MPRV;
        }
        self.mstatus = s2;
        self.prv = mpp;
        self.pc = self.mepc;
        self.invalidate_fetch();
    }

    fn sret(&mut self) {
        let s = self.mstatus;
        let spp = if s & mstatus::SPP != 0 { prv::S } else { prv::U };
        let mut s2 = s & !(mstatus::SIE | mstatus::SPP | mstatus::MPRV);
        if s & mstatus::SPIE != 0 {
            s2 |= mstatus::SIE;
        }
        s2 |= mstatus::SPIE;
        self.mstatus = s2;
        self.prv = spp;
        self.pc = self.sepc;
        self.invalidate_fetch();
    }

    // ---- CSRs ----

    fn fs_on(&self) -> bool {
        self.mstatus & mstatus::FS != 0
    }

    pub(crate) fn set_fs_dirty(&mut self) {
        self.mstatus |= mstatus::FS | mstatus::SD;
    }

    fn counter_allowed(&self, csr: u16) -> bool {
        let bit = 1u64 << (csr & 31);
        match self.prv {
            prv::M => true,
            prv::S => self.mcounteren & bit != 0,
            _ => self.mcounteren & bit != 0 && self.scounteren & bit != 0,
        }
    }

    /// Read a CSR; `None` = illegal instruction.
    pub fn csr_read<B: Bus>(&mut self, bus: &mut B, csr: u16) -> Option<u64> {
        if (csr >> 8) & 3 > self.prv as u16 {
            return None;
        }
        Some(match csr {
            0x001 if self.fs_on() => self.fcsr & 0x1f,
            0x002 if self.fs_on() => (self.fcsr >> 5) & 7,
            0x003 if self.fs_on() => self.fcsr & 0xff,
            0xc00..=0xc1f => {
                if !self.counter_allowed(csr) {
                    return None;
                }
                match csr {
                    0xc00 => self.cycle,
                    0xc01 => bus.mtime(),
                    0xc02 => self.instret,
                    _ => 0,
                }
            }
            0x100 => self.sstatus(),
            0x104 => self.mie & self.mideleg,
            0x105 => self.stvec,
            0x106 => self.scounteren,
            0x140 => self.sscratch,
            0x141 => self.sepc,
            0x142 => self.scause,
            0x143 => self.stval,
            0x144 => self.mip() & self.mideleg,
            0x180 => {
                if self.prv == prv::S && self.mstatus & mstatus::TVM != 0 {
                    return None;
                }
                self.satp
            }
            0x300 => self.mstatus_read(),
            0x301 => self.misa,
            0x302 => self.medeleg,
            0x303 => self.mideleg,
            0x304 => self.mie,
            0x305 => self.mtvec,
            0x306 => self.mcounteren,
            0x320 => self.mcountinhibit,
            0x323..=0x33f => 0,
            0x340 => self.mscratch,
            0x341 => self.mepc,
            0x342 => self.mcause,
            0x343 => self.mtval,
            0x344 => self.mip(),
            0x3a0 => self.pmpcfg[0],
            0x3a2 => self.pmpcfg[1],
            0x3b0..=0x3bf => self.pmpaddr[(csr - 0x3b0) as usize],
            0xb00 => self.cycle,
            0xb02 => self.instret,
            0xb03..=0xb1f => 0,
            0xf11 => self.id.mvendorid,
            0xf12 => self.id.marchid,
            0xf13 => self.id.mimpid,
            0xf14 => self.id.hartid,
            // SiFive custom M-mode CSRs (feature disable, power dial): read as zero
            0x7c0..=0x7c2 => 0,
            0x7a0..=0x7a3 => 0,
            _ => return None,
        })
    }

    /// Write a CSR; `false` = illegal instruction.
    pub fn csr_write(&mut self, csr: u16, v: u64) -> bool {
        if (csr >> 8) & 3 > self.prv as u16 || csr >> 10 == 3 {
            return false;
        }
        match csr {
            0x001 if self.fs_on() => {
                self.fcsr = (self.fcsr & !0x1f) | (v & 0x1f);
                self.set_fs_dirty();
            }
            0x002 if self.fs_on() => {
                self.fcsr = (self.fcsr & 0x1f) | ((v & 7) << 5);
                self.set_fs_dirty();
            }
            0x003 if self.fs_on() => {
                self.fcsr = v & 0xff;
                self.set_fs_dirty();
            }
            0x100 => {
                let m = mstatus::S_VIEW & !(mstatus::XL | mstatus::SD);
                self.mstatus_write((self.mstatus & !m) | (v & m));
            }
            0x104 => self.mie = (self.mie & !self.mideleg) | (v & self.mideleg),
            0x105 => self.stvec = v & !2,
            0x106 => self.scounteren = v & 0xffff_ffff,
            0x140 => self.sscratch = v,
            0x141 => self.sepc = v & !1,
            0x142 => self.scause = v,
            0x143 => self.stval = v,
            0x144 => {
                let m = self.mideleg & irq::SSIP;
                self.mip_sw = (self.mip_sw & !m) | (v & m);
            }
            0x180 => {
                if self.prv == prv::S && self.mstatus & mstatus::TVM != 0 {
                    return false;
                }
                let mode = v >> 60;
                if mode == 0 || mode == 8 {
                    // no ASIDs: ASID bits read as zero
                    self.satp = v & (0xf << 60 | 0xfff_ffff_ffff);
                }
                self.tlb.flush();
                self.invalidate_fetch();
            }
            0x300 => self.mstatus_write(v),
            0x301 => {}
            0x302 => self.medeleg = v & 0xb3ff,
            0x303 => self.mideleg = v & (irq::SSIP | irq::STIP | irq::SEIP),
            0x304 => self.mie = v & 0xaaa,
            0x305 => self.mtvec = v & !2,
            0x306 => self.mcounteren = v & 0xffff_ffff,
            0x320 => self.mcountinhibit = v & 0xffff_fffd,
            0x323..=0x33f => {}
            0x340 => self.mscratch = v,
            0x341 => self.mepc = v & !1,
            0x342 => self.mcause = v,
            0x343 => self.mtval = v,
            0x344 => {
                let m = irq::SSIP | irq::STIP | irq::SEIP;
                self.mip_sw = (self.mip_sw & !m) | (v & m);
            }
            0x3a0 => self.pmpcfg[0] = v,
            0x3a2 => self.pmpcfg[1] = v,
            0x3b0..=0x3bf => self.pmpaddr[(csr - 0x3b0) as usize] = v & ((1 << 54) - 1),
            0xb00 => self.cycle = v.wrapping_sub(1),
            0xb02 => self.instret = v.wrapping_sub(1),
            0xb03..=0xb1f => {}
            0x7c0..=0x7c2 => {}
            // trigger module with no usable triggers (tdata1.type = 0)
            0x7a0..=0x7a3 => {}
            _ => return false,
        }
        true
    }

    fn mstatus_read(&self) -> u64 {
        let mut s = self.mstatus | mstatus::XL;
        if s & mstatus::FS == mstatus::FS {
            s |= mstatus::SD;
        } else {
            s &= !mstatus::SD;
        }
        s
    }

    fn sstatus(&self) -> u64 {
        self.mstatus_read() & mstatus::S_VIEW
    }

    fn mstatus_write(&mut self, v: u64) {
        let mut v = v & mstatus::M_WRITABLE;
        if (v & mstatus::MPP) >> 11 == 2 {
            v = (v & !mstatus::MPP) | (self.mstatus & mstatus::MPP);
        }
        let changed = (self.mstatus ^ v) & (mstatus::MPRV | mstatus::SUM | mstatus::MXR | mstatus::MPP);
        self.mstatus = v | mstatus::XL;
        if changed != 0 {
            self.invalidate_fetch();
        }
    }
}
