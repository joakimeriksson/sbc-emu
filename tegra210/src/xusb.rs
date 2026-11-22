//! XUSB, the Tegra210's USB 3.0 host (`nvidia,tegra210-xusb`): the xHCI core
//! (`peripherals::xhci`) behind a falcon microcontroller that runs NVIDIA's firmware.
//!
//! The driver points the falcon at the firmware it loaded into memory, starts it and waits until
//! the xHCI is no longer "not ready". The firmware is not executed: any is accepted, and the xHCI
//! behaves as it would under it. Messages to the firmware are acknowledged; it sends none.
//!
//! Root ports 1-4 are USB 3.0, 5-8 USB 2.0. On the Nano the four USB-A sockets are behind a hub on
//! port 6; here the emulated devices sit on the root ports themselves.
use emu_core::usb::{DevId, Devices};
use peripherals::dma::Dma;
use peripherals::xhci::{Layout, Xhci};
use std::collections::HashMap;

pub const BASE: u64 = 0x7009_0000;
pub const SIZE: u64 = 0xa000;
pub const IRQ: usize = 32 + 0x27;
pub const MBOX_IRQ: usize = 32 + 0x28;
/// the "xusb_host" reset in the clock controller
pub const RESET: usize = 89;

const FPCI: u64 = 0x8000;
const IPFS: u64 = 0x9000;

// FPCI
const MBOX_CMD: u64 = 0xe4;
const MBOX_OWNER: u64 = 0xf0;
const MBOX_DEST_FALC: u32 = 1 << 27;
const ARU_C11_CSBRANGE: u64 = 0x41c;
const SMI_INTR: u64 = 0x428;
const CSB_WINDOW: u64 = 0x800;
// CSB
const FALC_CPUCTL: u32 = 0x100;
const STARTCPU: u32 = 1 << 1;
const STATE_HALTED: u32 = 1 << 4;
const MEMPOOL_L2IMEMOP_RESULT: u32 = 0x10_1a18;
// IPFS
const INTR_MASK: u64 = 0x188;
const IP_INT_MASK: u32 = 1 << 16;

pub struct Xusb {
    pub xhci: Xhci,
    fpci: HashMap<u64, u32>,
    ipfs: HashMap<u64, u32>,
    csb: HashMap<u32, u32>,
    /// messages the driver sent to the firmware
    pub messages: u64,
}

impl Default for Xusb {
    fn default() -> Self {
        let mut xhci = Xhci::new(Layout { usb3: 0..4, usb2: 4..8 });
        xhci.ready = false;
        let mut s = Self { xhci, fpci: HashMap::new(), ipfs: HashMap::new(), csb: HashMap::new(), messages: 0 };
        s.attach(false);
        s
    }
}

impl Xusb {
    /// A keyboard and a tablet, and a USB stick on request.
    pub fn attach(&mut self, stick: bool) {
        self.xhci.attach(&[stick.then_some(DevId::Storage), Some(DevId::Keyboard), Some(DevId::Tablet)]);
    }

    /// The controller was held in reset, or its power was off: the firmware is gone.
    pub fn reset(&mut self, devs: &mut Devices) {
        self.fpci.clear();
        self.ipfs.clear();
        self.csb.clear();
        self.xhci.power_on_reset(devs);
        self.xhci.ready = false;
    }

    pub fn irq(&self) -> bool {
        self.xhci.ready && self.xhci.irq() && self.ipfs.get(&INTR_MASK).is_some_and(|m| m & IP_INT_MASK != 0)
    }

    fn csb_address(&self, offset: u64) -> u32 {
        (self.fpci.get(&ARU_C11_CSBRANGE).copied().unwrap_or(0) & 0x7f_ffff) << 9 | (offset - CSB_WINDOW) as u32
    }

    /// The 32-bit register at `offset`.
    pub fn read(&mut self, offset: u64) -> u32 {
        match offset {
            0..FPCI => self.xhci.read(offset),
            FPCI..IPFS => {
                let o = offset - FPCI;
                if (CSB_WINDOW..CSB_WINDOW + 0x200).contains(&o) {
                    return match self.csb_address(o) {
                        FALC_CPUCTL if !self.xhci.ready => STATE_HALTED,
                        MEMPOOL_L2IMEMOP_RESULT => 1 << 31, // loaded at once
                        a => self.csb.get(&a).copied().unwrap_or(0),
                    };
                }
                match o {
                    0 => 0x0fad_10de, // the function's PCI-like identity
                    o => self.fpci.get(&o).copied().unwrap_or(0),
                }
            }
            _ => self.ipfs.get(&(offset - IPFS)).copied().unwrap_or(0),
        }
    }

    pub fn write(&mut self, offset: u64, v: u32, dma: &mut Dma, devs: &mut Devices) {
        match offset {
            0..FPCI => {
                if self.xhci.ready {
                    self.xhci.write(offset, v, dma, devs);
                }
            }
            FPCI..IPFS => {
                let o = offset - FPCI;
                if (CSB_WINDOW..CSB_WINDOW + 0x200).contains(&o) {
                    let a = self.csb_address(o);
                    if a == FALC_CPUCTL {
                        if v & STARTCPU != 0 {
                            self.xhci.ready = true;
                        }
                    } else {
                        self.csb.insert(a, v);
                    }
                    return;
                }
                match o {
                    SMI_INTR => {} // write 1 to clear; the firmware raises nothing
                    MBOX_CMD if v & MBOX_DEST_FALC != 0 => {
                        // the firmware takes the message and lets the mailbox go
                        self.messages += 1;
                        self.fpci.insert(MBOX_CMD, v & !MBOX_DEST_FALC);
                        self.fpci.insert(MBOX_OWNER, 0);
                    }
                    // an owner only lets go of what it has
                    MBOX_OWNER if v != 0 && self.fpci.get(&MBOX_OWNER).is_some_and(|&w| w != 0) => {}
                    o => {
                        self.fpci.insert(o, v);
                    }
                }
            }
            _ => {
                self.ipfs.insert(offset - IPFS, v);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_firmware_starts_and_the_mailbox_answers() {
        let mut x = Xusb::default();
        let mut devs = Devices::default();
        let mut d = Dma { ram: &mut [], base: 0x8000_0000 };
        assert_ne!(x.read(0x24) & 1 << 11, 0, "not ready");
        let mut csb = |x: &mut Xusb, a: u32, w: Option<u32>| {
            x.write(FPCI + ARU_C11_CSBRANGE, a >> 9, &mut d, &mut devs);
            match w {
                Some(v) => { x.write(FPCI + CSB_WINDOW + (a & 0x1ff) as u64, v, &mut d, &mut devs); 0 }
                None => x.read(FPCI + CSB_WINDOW + (a & 0x1ff) as u64),
            }
        };
        assert_eq!(csb(&mut x, 0x10_1a04, None), 0, "no firmware loaded");
        csb(&mut x, 0x10_1a04, Some(0x9000_0100));
        assert_eq!(csb(&mut x, 0x10_1a04, None), 0x9000_0100);
        assert_ne!(csb(&mut x, MEMPOOL_L2IMEMOP_RESULT, None) >> 31, 0);
        csb(&mut x, FALC_CPUCTL, Some(STARTCPU));
        assert_eq!(x.read(0x24) & 1 << 11, 0);
        assert_eq!(x.read(4) >> 24, 8, "eight root ports");
        assert_eq!((x.read(0x508), x.read(0x518)), (4 << 8 | 5, 4 << 8 | 1));
        // the keyboard is on port 6
        assert_eq!((x.read(0x420 + 0x10 * 4) & 1, x.read(0x420 + 0x10 * 5) & 1), (0, 1));
        let mut d = Dma { ram: &mut [], base: 0x8000_0000 };
        x.write(FPCI + MBOX_OWNER, 2, &mut d, &mut devs);
        assert_eq!(x.read(FPCI + MBOX_OWNER), 2);
        x.write(FPCI + MBOX_OWNER, 1, &mut d, &mut devs);
        assert_eq!(x.read(FPCI + MBOX_OWNER), 2, "it is taken");
        x.write(FPCI + MBOX_CMD, 1 << 31 | MBOX_DEST_FALC, &mut d, &mut devs);
        assert_eq!((x.read(FPCI + MBOX_OWNER), x.messages), (0, 1));
        x.reset(&mut devs);
        assert_ne!(x.read(0x24) & 1 << 11, 0);
        assert_eq!(x.read(0x420 + 0x10 * 5) & 1, 1, "the keyboard is still plugged in");
    }
}
