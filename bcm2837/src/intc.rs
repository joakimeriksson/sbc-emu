//! The BCM2837's two interrupt controllers (it has no GIC):
//!
//! - [`ArmCtrl`]: the ARM-side controller of the BCM2835 family (`brcm,bcm2836-armctrl-ic` at
//!   0x3f00_b200): 64 VideoCore-side ("GPU") interrupts in banks 1 and 2 and eight ARM ones in
//!   bank 0, with the basic pending register's summary and shortcut bits. Its output is one
//!   line, the "GPU interrupt" of the per-core controller.
//! - [`LocalIntc`]: the per-core block at 0x4000_0000 (`brcm,bcm2836-l1-intc`, from the
//!   BCM2836): each core's generic-timer outputs (not PPIs here), four 32-bit mailboxes per
//!   core (Linux's IPIs are writes to mailbox 0), the GPU interrupt routed to one core, the
//!   PMU routing bits. Each core reads its own pending register by its offset, nothing is banked.
//!
//! Only the IRQ side is modelled: FIQ routing is stored, never raised (Linux does not use it on
//! arm64). Registers follow the BCM2835 ARM peripherals manual and the QA7 (BCM2836 ARM-local
//! peripherals) document, as Linux's `irq-bcm2835.c` and `irq-bcm2836.c` use them.

/// Basic pending register: bits 8 and 9 summarise banks 1 and 2, bits 10..20 repeat some GPU
/// interrupts ("shortcuts") so the handler finds them with one read.
const SHORTCUTS: [usize; 11] = [7, 9, 10, 18, 19, 53, 54, 55, 56, 57, 62];

/// The ARM-side interrupt controller (`brcm,bcm2836-armctrl-ic`).
#[derive(Default, Clone)]
pub struct ArmCtrl {
    /// GPU interrupt levels (sources), 0..63
    pub gpu: u64,
    /// ARM interrupt levels, bank 0 bits 0..7 (0 ARM timer, 1 ARM mailbox, 2/3 doorbells, ...)
    pub arm: u8,
    gpu_en: u64,
    arm_en: u8,
    fiq: u32,
}

impl ArmCtrl {
    pub fn new() -> Self { Self::default() }

    pub fn set_gpu(&mut self, n: usize, high: bool) {
        if high { self.gpu |= 1 << n } else { self.gpu &= !(1 << n) }
    }

    pub fn set_arm(&mut self, n: usize, high: bool) {
        if high { self.arm |= 1 << n } else { self.arm &= !(1 << n) }
    }

    fn gpu_pending(&self) -> u64 { self.gpu & self.gpu_en }

    /// IRQ basic pending (offset 0x00).
    pub fn basic_pending(&self) -> u32 {
        let g = self.gpu_pending();
        let mut v = (self.arm & self.arm_en) as u32;
        if g as u32 != 0 { v |= 1 << 8; }
        if g >> 32 != 0 { v |= 1 << 9; }
        for (k, &n) in SHORTCUTS.iter().enumerate() {
            if g >> n & 1 != 0 { v |= 1 << (10 + k); }
        }
        v
    }

    /// The controller's output: an enabled interrupt is pending.
    pub fn irq(&self) -> bool { self.gpu_pending() != 0 || self.arm & self.arm_en != 0 }

    pub fn read(&self, off: u64) -> u32 {
        match off {
            0x00 => self.basic_pending(),
            0x04 => self.gpu_pending() as u32,
            0x08 => (self.gpu_pending() >> 32) as u32,
            0x0c => self.fiq,
            0x10 => self.gpu_en as u32,
            0x14 => (self.gpu_en >> 32) as u32,
            0x18 => self.arm_en as u32,
            // the disable registers read back the enables too
            0x1c => self.gpu_en as u32,
            0x20 => (self.gpu_en >> 32) as u32,
            0x24 => self.arm_en as u32,
            _ => 0,
        }
    }

    pub fn write(&mut self, off: u64, v: u32) {
        match off {
            0x0c => self.fiq = v & 0xff,
            0x10 => self.gpu_en |= v as u64,
            0x14 => self.gpu_en |= (v as u64) << 32,
            0x18 => self.arm_en |= v as u8,
            0x1c => self.gpu_en &= !(v as u64),
            0x20 => self.gpu_en &= !((v as u64) << 32),
            0x24 => self.arm_en &= !(v as u8),
            _ => {}
        }
    }

    pub fn save_state(&self, w: &mut emu_core::snap::W) {
        w.tag("armctrl-v1");
        w.u64(self.gpu); w.u8(self.arm); w.u64(self.gpu_en); w.u8(self.arm_en); w.u32(self.fiq);
    }
    pub fn load_state(&mut self, r: &mut emu_core::snap::R) -> Result<(), emu_core::snap::SnapError> {
        r.tag("armctrl-v1")?;
        self.gpu = r.u64()?; self.arm = r.u8()?; self.gpu_en = r.u64()?; self.arm_en = r.u8()?; self.fiq = r.u32()?;
        Ok(())
    }
}

/// Per-core sources in the local pending registers (bit numbers).
pub const CNTPS: usize = 0;
pub const CNTPNS: usize = 1;
pub const CNTHP: usize = 2;
pub const CNTV: usize = 3;
pub const MAILBOX0: usize = 4;
pub const GPU: usize = 8;
pub const PMU: usize = 9;

pub const NCORES: usize = 4;

/// The BCM2836 per-core interrupt controller (`brcm,bcm2836-l1-intc`).
#[derive(Default, Clone)]
pub struct LocalIntc {
    control: u32,
    prescaler: u32,
    /// GPU interrupt routing: bits 1..0 the core for IRQ, 3..2 for FIQ
    gpu_route: u32,
    /// PMU interrupt routing: bits 3..0 IRQ per core, 7..4 FIQ
    pmu_route: u32,
    /// per core: bits 3..0 IRQ enables of CNTPS, CNTPNS, CNTHP, CNTV; 7..4 the FIQ ones
    timer_ctl: [u32; NCORES],
    /// per core: bits 3..0 IRQ enables of mailboxes 0..3; 7..4 the FIQ ones
    mbox_ctl: [u32; NCORES],
    /// mailbox words, `[core][mailbox]`
    mbox: [[u32; 4]; NCORES],
    /// per core: generic timer output levels (bits CNTPS..CNTV), set by the machine
    pub timers: [u8; NCORES],
    /// per core: the PMU's interrupt (not modelled: stays low)
    pub pmu: [bool; NCORES],
    /// the local timer's control/status and reload (stored; it never counts)
    local_timer: [u32; 2],
    local_route: u32,
}

impl LocalIntc {
    pub fn new() -> Self { Self::default() }

    /// Core `k`'s timer outputs: CNTP (non-secure physical) and CNTV (virtual). The secure
    /// physical timer is the same counter seen from EL3, which Linux does not use.
    pub fn set_timers(&mut self, k: usize, phys: bool, virt: bool) {
        self.timers[k] = ((phys as u8) << CNTPNS) | ((virt as u8) << CNTV);
    }

    /// Core `k`'s IRQ pending register, given the ARM-side controller's output.
    pub fn pending(&self, k: usize, gpu_irq: bool) -> u32 {
        let mut v = self.timers[k] as u32 & self.timer_ctl[k] & 0xf;
        for m in 0..4 {
            if self.mbox[k][m] != 0 && self.mbox_ctl[k] >> m & 1 != 0 { v |= 1 << (MAILBOX0 + m); }
        }
        if gpu_irq && self.gpu_route & 3 == k as u32 { v |= 1 << GPU; }
        if self.pmu[k] && self.pmu_route >> k & 1 != 0 { v |= 1 << PMU; }
        v
    }

    /// Core `k`'s IRQ input.
    pub fn irq_out(&self, k: usize, gpu_irq: bool) -> bool { self.pending(k, gpu_irq) != 0 }

    /// A register read; `gpu_irq` is the ARM-side controller's output.
    pub fn read(&self, off: u64, gpu_irq: bool) -> u32 {
        let k = ((off >> 2) & 3) as usize;
        match off {
            0x00 => self.control,
            0x08 => self.prescaler,
            0x0c => self.gpu_route,
            0x10 | 0x14 => self.pmu_route,
            0x24 => self.local_route,
            0x34 => self.local_timer[0],
            0x40..=0x4f => self.timer_ctl[k],
            0x50..=0x5f => self.mbox_ctl[k],
            0x60..=0x6f => self.pending(k, gpu_irq),
            0x70..=0x7f => 0, // FIQ pending: no FIQs
            // the set registers are write-only; the clear registers read the mailbox
            0xc0..=0xff => self.mbox[((off - 0xc0) / 16) as usize][((off >> 2) & 3) as usize],
            _ => 0,
        }
    }

    pub fn write(&mut self, off: u64, v: u32) {
        let k = ((off >> 2) & 3) as usize;
        match off {
            0x00 => self.control = v,
            0x08 => self.prescaler = v,
            0x0c => self.gpu_route = v & 0xf,
            0x10 => self.pmu_route |= v & 0xff,
            0x14 => self.pmu_route &= !(v & 0xff),
            0x24 => self.local_route = v & 7,
            0x34 => self.local_timer[0] = v & 0x3fff_ffff,
            0x38 => self.local_timer[1] = v,
            0x40..=0x4f => self.timer_ctl[k] = v & 0xff,
            0x50..=0x5f => self.mbox_ctl[k] = v & 0xff,
            0x80..=0xbf => self.mbox[((off - 0x80) / 16) as usize][k] |= v,
            0xc0..=0xff => self.mbox[((off - 0xc0) / 16) as usize][k] &= !v,
            _ => {}
        }
    }

    pub fn save_state(&self, w: &mut emu_core::snap::W) {
        w.tag("l1-intc-v1");
        for v in [self.control, self.prescaler, self.gpu_route, self.pmu_route, self.local_route, self.local_timer[0], self.local_timer[1]] { w.u32(v); }
        for k in 0..NCORES {
            w.u32(self.timer_ctl[k]); w.u32(self.mbox_ctl[k]); w.u8(self.timers[k]);
            for m in 0..4 { w.u32(self.mbox[k][m]); }
        }
    }
    pub fn load_state(&mut self, r: &mut emu_core::snap::R) -> Result<(), emu_core::snap::SnapError> {
        r.tag("l1-intc-v1")?;
        let [t0, t1] = &mut self.local_timer;
        for v in [&mut self.control, &mut self.prescaler, &mut self.gpu_route, &mut self.pmu_route, &mut self.local_route, t0, t1] { *v = r.u32()?; }
        for k in 0..NCORES {
            self.timer_ctl[k] = r.u32()?; self.mbox_ctl[k] = r.u32()?; self.timers[k] = r.u8()?;
            for m in 0..4 { self.mbox[k][m] = r.u32()?; }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn armctrl_pending_needs_enable_and_reports_banks() {
        let mut c = ArmCtrl::new();
        c.set_gpu(29, true); // the mini UART (AUX)
        assert!(!c.irq());
        assert_eq!(c.read(0x04), 0);
        c.write(0x10, 1 << 29);
        assert!(c.irq());
        assert_eq!(c.read(0x04), 1 << 29);
        assert_eq!(c.read(0x00), 1 << 8);
        c.write(0x1c, 1 << 29);
        assert!(!c.irq());
        assert_eq!(c.read(0x00), 0);
    }

    #[test]
    fn armctrl_shortcuts_and_arm_bank() {
        let mut c = ArmCtrl::new();
        // GPU 57 (PL011) and 62 (the Arasan SD host) have shortcuts in bank 2
        c.set_gpu(57, true);
        c.set_gpu(62, true);
        c.write(0x14, (1 << 25) | (1 << 30));
        assert_eq!(c.read(0x08), (1 << 25) | (1 << 30));
        assert_eq!(c.read(0x00), (1 << 9) | (1 << 19) | (1 << 20));
        // GPU 7 is the first shortcut of bank 1
        c.set_gpu(7, true);
        c.write(0x10, 1 << 7);
        assert_eq!(c.read(0x00) & 0x7ff, (1 << 8) | (1 << 9) | (1 << 10));
        // bank 0: the ARM mailbox interrupt
        c.set_arm(1, true);
        assert_eq!(c.read(0x00) & 0xff, 0);
        c.write(0x18, 2);
        assert_eq!(c.read(0x00) & 0xff, 2);
        c.write(0x24, 2);
        assert_eq!(c.read(0x00) & 0xff, 0);
    }

    #[test]
    fn local_timers_are_per_core_and_need_enable() {
        let mut l = LocalIntc::new();
        l.set_timers(2, false, true);
        assert!(!l.irq_out(2, false));
        l.write(0x48, 1 << CNTV); // core 2's timer control
        assert!(l.irq_out(2, false));
        assert_eq!(l.read(0x68, false), 1 << CNTV);
        assert!(!l.irq_out(0, false) && !l.irq_out(1, false) && !l.irq_out(3, false));
        l.set_timers(2, true, false);
        assert!(!l.irq_out(2, false));
        l.write(0x48, (1 << CNTV) | (1 << CNTPNS));
        assert_eq!(l.read(0x68, false), 1 << CNTPNS);
    }

    #[test]
    fn mailboxes_are_ipis() {
        let mut l = LocalIntc::new();
        // core 3 enables mailbox 0's IRQ, as Linux does when a core starts
        l.write(0x5c, 1);
        // core 0 sends IPI 2 and IPI 5 to core 3: set bits of core 3's mailbox 0
        l.write(0x80 + 16 * 3, 1 << 2);
        l.write(0x80 + 16 * 3, 1 << 5);
        assert!(l.irq_out(3, false));
        assert_eq!(l.read(0x6c, false), 1 << MAILBOX0);
        assert_eq!(l.read(0xc0 + 16 * 3, false), (1 << 2) | (1 << 5));
        // a mailbox without its enable raises nothing, and set registers read as zero
        l.write(0x80 + 16 * 1, 1);
        assert!(!l.irq_out(1, false));
        assert_eq!(l.read(0x80 + 16 * 1, false), 0);
        // the handler acknowledges one IPI at a time through the clear register
        l.write(0xc0 + 16 * 3, 1 << 2);
        assert!(l.irq_out(3, false));
        l.write(0xc0 + 16 * 3, 1 << 5);
        assert!(!l.irq_out(3, false));
        // mailbox 3 of core 1 (the 32-bit kernels' boot mailbox)
        l.write(0x54, 1 << 3);
        l.write(0x8c + 16, 0x8000);
        assert_eq!(l.read(0x64, false), 1 << (MAILBOX0 + 3));
        assert_eq!(l.read(0xcc + 16, false), 0x8000);
    }

    #[test]
    fn gpu_interrupt_goes_to_the_routed_core() {
        let mut l = LocalIntc::new();
        let mut c = ArmCtrl::new();
        c.set_gpu(29, true);
        c.write(0x10, 1 << 29);
        assert!(l.irq_out(0, c.irq()));
        assert_eq!(l.read(0x60, c.irq()), 1 << GPU);
        assert!(!l.irq_out(1, c.irq()));
        l.write(0x0c, 2);
        assert!(!l.irq_out(0, c.irq()));
        assert_eq!(l.read(0x68, c.irq()), 1 << GPU);
        c.set_gpu(29, false);
        assert!(!l.irq_out(2, c.irq()));
    }

    #[test]
    fn state_round_trips() {
        let mut l = LocalIntc::new();
        l.write(0x5c, 1);
        l.write(0xb0, 9);
        l.write(0x44, 8);
        l.set_timers(1, false, true);
        let mut c = ArmCtrl::new();
        c.set_gpu(62, true);
        c.write(0x14, 1 << 30);
        let mut w = emu_core::snap::W::default();
        l.save_state(&mut w);
        c.save_state(&mut w);
        let (mut l2, mut c2) = (LocalIntc::new(), ArmCtrl::new());
        let mut r = emu_core::snap::R::new(&w.0);
        l2.load_state(&mut r).unwrap();
        c2.load_state(&mut r).unwrap();
        for off in (0..0x100).step_by(4) { assert_eq!(l.read(off, true), l2.read(off, true), "{off:#x}"); }
        for off in (0..0x28).step_by(4) { assert_eq!(c.read(off), c2.read(off), "{off:#x}"); }
    }
}
