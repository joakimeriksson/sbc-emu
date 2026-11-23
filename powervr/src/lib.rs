//! Imagination PowerVR Rogue GPU: the BXE-4-32 (BVNC 36.50.54.182) in the StarFive JH7110.
//!
//! The kernel driver (Linux's `powervr`) does not program the GPU's pipelines. It hands work
//! to firmware running on a MIPS microcontroller inside the GPU, through command rings in
//! memory. This model has the GPU's registers as the driver uses them, and plays the
//! firmware at that interface (`fw`): it boots when the driver releases the MIPS core from
//! reset, finds the driver's init structures through the driver's MIPS page table, and
//! serves the kernel command ring.
//!
//! `firmware` makes the firmware file the driver loads (header, layout and device info; no
//! Imagination code).
//!
//! Compute jobs run: `cdm` walks the compute data master's control stream and runs each
//! kernel's tasks, `pds` interprets the data-sequencer programs that load the shader's
//! inputs, and `usc` decodes and executes the shader programs themselves.
pub mod cdm;
pub mod firmware;
pub mod fw;
pub mod gfx;
pub mod pds;
pub mod regs;
pub mod tpu;
pub mod usc;
pub mod vm;

use emu_core::FlatRam;
use std::collections::BTreeMap;

pub struct Gpu {
    /// register file, 64-bit words by 8-byte-aligned offset (reads return what was written)
    regs: BTreeMap<u64, u64>,
    pub fw: fw::Firmware,
    /// MIPS wrapper interrupt status (bit 0: the firmware's interrupt to the host)
    irq_status: u64,
    pub trace: bool,
}

impl Default for Gpu {
    fn default() -> Self {
        let trace = std::env::var_os("SBC_GPU_TRACE").is_some_and(|v| !v.is_empty());
        let mut fw = fw::Firmware::default();
        fw.trace = trace;
        fw.trace_jobs = std::env::var("SBC_GPU_TRACE_JOBS").ok().and_then(|v| v.parse().ok()).unwrap_or(0);
        Gpu { regs: BTreeMap::new(), fw, irq_status: 0, trace }
    }
}

impl Gpu {
    fn reg(&self, off: u64) -> u64 {
        use regs::*;
        match off {
            CORE_ID_PBVNC => firmware::PACKED_BVNC,
            // idle and fault-free, whatever was asked
            MARS_IDLE => 0x7,
            SIDEKICK_IDLE => 0x7f,
            SLC_IDLE => 0xff,
            BIF_STATUS_MMU | BIFPM_STATUS_MMU | BIF_READS_EXT_STATUS | BIFPM_READS_EXT_STATUS | SLC_STATUS1 => 0,
            MIPS_WRAPPER_IRQ_STATUS => self.irq_status,
            _ => self.regs.get(&off).copied().unwrap_or(0),
        }
    }

    /// A register read of `size` bytes (4 or 8) at `off`.
    pub fn read(&mut self, off: u64, size: u8) -> u64 {
        let v = self.reg(off & !7) >> (8 * (off & 4));
        let v = if size >= 8 { v } else { v & 0xffff_ffff };
        if self.trace {
            eprintln!("[gpu] R{} {off:#06x} -> {v:#x}", 8 * size as u32);
        }
        v
    }

    /// A register write of `size` bytes (4 or 8).
    pub fn write(&mut self, off: u64, size: u8, v: u64, ram: &mut FlatRam) {
        use regs::*;
        if self.trace {
            eprintln!("[gpu] W{} {off:#06x} <- {v:#x}", 8 * size as u32);
        }
        let word = off & !7;
        let old = self.reg(word);
        let new = if size >= 8 {
            v
        } else {
            let sh = 8 * (off & 4);
            (old & !(0xffff_ffff << sh)) | ((v & 0xffff_ffff) << sh)
        };
        match word {
            // the MIPS core runs while SOFT_RESET bit 32 (GARTEN/CPU) is clear
            SOFT_RESET => {
                let (was, now) = (old & SOFT_RESET_CPU != 0, new & SOFT_RESET_CPU != 0);
                self.regs.insert(word, new);
                if was && !now {
                    let boot_data = self.reg(MIPS_ADDR_REMAP2_CONFIG2) & 0xff_ffff_f000;
                    self.fw.boot(ram, boot_data);
                    self.raise_if(ram);
                } else if !was && now {
                    self.fw.halt();
                }
            }
            MIPS_WRAPPER_IRQ_CLEAR => {
                if new & 1 != 0 {
                    self.irq_status &= !1;
                }
            }
            // any write (the driver writes 0) is the kernel command ring's doorbell
            MTS_SCHEDULE => {
                self.regs.insert(word, new);
                self.fw.doorbell(ram);
                self.raise_if(ram);
            }
            _ => {
                self.regs.insert(word, new);
            }
        }
    }

    fn raise_if(&mut self, _ram: &mut FlatRam) {
        if self.fw.take_interrupt() {
            self.irq_status |= 1;
        }
    }

    /// The GPU's interrupt line.
    pub fn irq(&self) -> bool {
        self.irq_status & 1 != 0
    }
}

// Snapshots: the registers, the firmware's state and scenes in flight.
emu_core::snap_struct!(Gpu { regs, fw, irq_status } skip { trace });
