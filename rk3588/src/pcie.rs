//! The M.2 slot's PCIe 2.0 x1 controller (pcie2x1l2) as the firmware leaves it: described to
//! Linux as a generic ECAM host (`pci-host-ecam-generic`) in the controller's own windows
//! (configuration at 0xf4000000, 32-bit memory at 0xf4200000) with its legacy interrupt
//! (SPI 250). The DesignWare controller, its PHY and MSI through the ITS are not modelled;
//! devices signal INTA.
//!
//! Bus 0, device 0 is the slot: empty, or an NVMe SSD (`nvme.rs`).
use crate::nvme::Nvme;

pub const ECAM: u64 = 0xf400_0000;
pub const ECAM_SIZE: u64 = 0x10_0000;
pub const MEM: u64 = 0xf420_0000;
pub const MEM_SIZE: u64 = 0xe0_0000;
pub const IRQ: usize = 32 + 250;
/// sbc-emu's NVMe (the same IDs as on the Orange Pi RV)
const NVME_ID: u32 = 0x0001_fffd;
const NVME_BAR_SIZE: u64 = 0x4000;

pub struct Pcie {
    pub nvme: Option<Nvme>,
    /// the endpoint's configuration header (64 dwords)
    cfg: [u32; 64],
}

impl Default for Pcie {
    fn default() -> Self { Self { nvme: None, cfg: [0; 64] } }
}

impl Pcie {
    pub fn attach_nvme(&mut self, nvme: Nvme) {
        let mut cfg = [0u32; 64];
        cfg[0] = NVME_ID;
        cfg[2] = 0x0108_0200; // mass storage, NVM, NVMe; revision 0
        cfg[4] = 0x4; // BAR0: 64-bit memory
        cfg[0x2c / 4] = NVME_ID; // subsystem
        cfg[0x3c / 4] = 0x0100; // INTA
        self.cfg = cfg;
        self.nvme = Some(nvme);
    }
    /// Configuration read at `off` in the ECAM window (bus/device/function/register).
    pub fn cfg_read(&self, off: u64) -> u32 {
        let (bdf, reg) = (off >> 12, (off & 0xffc) as usize / 4);
        if bdf != 0 || self.nvme.is_none() { return u32::MAX; } // no device: all ones
        match reg {
            1 => self.cfg[1] | u32::from(self.intx()) << 19, // status: interrupt status
            r if r < 64 => self.cfg[r],
            _ => 0,
        }
    }
    pub fn cfg_write(&mut self, off: u64, v: u32) {
        let (bdf, reg) = (off >> 12, (off & 0xffc) as usize / 4);
        if bdf != 0 || self.nvme.is_none() { return; }
        match reg {
            // command: memory space, bus master, INTx disable
            1 => self.cfg[1] = v & 0x0406,
            // BAR0: 64-bit, non-prefetchable, 16 KiB (sizing writes all ones)
            4 => self.cfg[4] = (v & !(NVME_BAR_SIZE as u32 - 1)) | 0x4,
            5 => self.cfg[5] = v,
            15 => self.cfg[15] = (self.cfg[15] & 0xffff_ff00) | (v & 0xff),
            _ => {}
        }
    }
    /// BAR0's address while memory decoding is on.
    fn bar(&self) -> Option<u64> {
        (self.nvme.is_some() && self.cfg[1] & 2 != 0).then(|| (self.cfg[5] as u64) << 32 | (self.cfg[4] & !0xf) as u64)
    }
    /// A memory access at `addr` in the memory window: Some if it hits the BAR.
    pub fn mem_read(&mut self, addr: u64) -> Option<u32> {
        let bar = self.bar()?;
        let off = addr.checked_sub(bar).filter(|&o| o < NVME_BAR_SIZE)?;
        self.nvme.as_mut().map(|n| n.read(off))
    }
    pub fn mem_write(&mut self, addr: u64, v: u32, ram: &mut [u8]) -> bool {
        let Some(bar) = self.bar() else { return false };
        let Some(off) = addr.checked_sub(bar).filter(|&o| o < NVME_BAR_SIZE) else { return false };
        // DMA needs bus mastering; addresses are RAM offsets
        if let Some(n) = self.nvme.as_mut() { n.write(off, v, ram, 0); }
        true
    }
    fn intx(&self) -> bool { self.nvme.as_ref().is_some_and(|n| n.intx()) }
    /// INTA to the GIC (unless the driver disabled INTx).
    pub fn irq(&self) -> bool { self.cfg[1] & 0x400 == 0 && self.intx() }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn empty_slot_reads_all_ones_and_nvme_bar_sizes_to_16k() {
        let mut p = Pcie::default();
        assert_eq!(p.cfg_read(0), u32::MAX);
        p.attach_nvme(Nvme::new(None));
        assert_eq!(p.cfg_read(0), NVME_ID);
        assert_eq!(p.cfg_read(8) >> 8, 0x01_0802);
        assert_eq!(p.cfg_read(0x1000), u32::MAX, "only device 0");
        p.cfg_write(0x10, u32::MAX);
        assert_eq!(p.cfg_read(0x10), 0xffff_c004);
        p.cfg_write(0x10, MEM as u32); p.cfg_write(0x14, 0);
        assert_eq!(p.mem_read(MEM), None, "memory decoding off");
        p.cfg_write(4, 6);
        assert_eq!(p.mem_read(MEM + 8), Some(0x0001_0400), "NVMe 1.4");
    }
}
