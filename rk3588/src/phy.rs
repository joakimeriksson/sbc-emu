//! Orange Pi 5's Motorcomm YT8531 Clause 22 Ethernet PHY.
//!
//! Register-level bring-up model: identity, basic control/status, advertised 10/100/1000
//! capabilities and the driver's extended-register address/data window. It does not model
//! analogue RGMII signalling or timing. A connected virtual Ethernet backend supplies link.
use std::collections::BTreeMap;

pub const ADDRESS: u8 = 1;
pub const PHY_ID: u32 = 0x4f51_e91b;
const BMCR_RESET: u16 = 1 << 15;
const BMCR_ANENABLE: u16 = 1 << 12;
const BMCR_POWERDOWN: u16 = 1 << 11;
const BMCR_RESTART: u16 = 1 << 9;
const BMSR_LINK: u16 = 1 << 2;
const BMSR_AN_COMPLETE: u16 = 1 << 5;

pub struct Yt8531 {
    regs: [u16; 32],
    extended: BTreeMap<u16, u16>,
    page: u16,
    cable: bool,
    link_latched_low: bool,
}
impl Default for Yt8531 {
    fn default() -> Self {
        let mut phy = Self { regs: [0; 32], extended: BTreeMap::new(), page: 0,
            cable: false, link_latched_low: false };
        phy.reset();
        phy
    }
}
impl Yt8531 {
    fn reset(&mut self) {
        self.regs = [0; 32];
        self.regs[0] = 0x1140; // 1 Gbit, autonegotiation, full duplex
        self.regs[4] = 0x01e1; // 10/100 half/full, 802.3 selector
        self.regs[9] = 0x0300; // 1000BASE-T half/full advertisement
        self.regs[16] = 0x0068;
        self.extended.clear();
        self.page = 0;
        self.link_latched_low = !self.link();
    }
    fn link(&self) -> bool { self.cable && self.regs[0] & BMCR_POWERDOWN == 0 }
    /// Connect or disconnect the virtual cable. BMSR link is latched low on a falling edge.
    pub fn set_link(&mut self, connected: bool) {
        let before = self.link();
        self.cable = connected;
        if before && !self.link() { self.link_latched_low = true; }
    }
    /// Clause 22 register read. Absent PHY addresses return the bus's pulled-up value.
    pub fn read(&mut self, address: u8, reg: u8) -> u16 {
        if address != ADDRESS || reg >= 32 { return 0xffff; }
        let link = self.link();
        match reg {
            0 => self.regs[0],
            1 => {
                let latched = self.link_latched_low;
                self.link_latched_low = false;
                // Extended status, autonegotiation and 10/100 capability bits.
                0x7909 | if link && !latched { BMSR_LINK } else { 0 }
                    | if link && self.regs[0] & BMCR_ANENABLE != 0 { BMSR_AN_COMPLETE } else { 0 }
            }
            2 => (PHY_ID >> 16) as u16,
            3 => PHY_ID as u16,
            5 => if link { 0x01e1 } else { 0 },
            10 => if link { 0x0c00 } else { 0 },
            15 => 0x3000, // 1000BASE-T half/full in extended status
            17 => if link { 0x8000 | 0x2000 | 0x0800 | 0x0400 } else { 0 },
            30 => self.page,
            31 => self.extended.get(&self.page).copied().unwrap_or(0),
            _ => self.regs[reg as usize],
        }
    }
    /// Clause 22 write. PHY IDs and status registers are read-only.
    pub fn write(&mut self, address: u8, reg: u8, value: u16) {
        if address != ADDRESS || reg >= 32 { return; }
        match reg {
            0 if value & BMCR_RESET != 0 => self.reset(),
            0 => {
                let was_up = self.link();
                self.regs[0] = value & !(BMCR_RESET | BMCR_RESTART);
                if was_up && !self.link() { self.link_latched_low = true; }
            }
            1 | 2 | 3 | 5 | 10 | 15 | 17 => {}
            30 => self.page = value,
            31 => { self.extended.insert(self.page, value); }
            _ => self.regs[reg as usize] = value,
        }
    }
}

emu_core::snap_struct!(Yt8531 { regs, extended, page, cable, link_latched_low });

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn identity_and_link_latch_follow_the_virtual_cable() {
        let mut phy = Yt8531::default();
        assert_eq!((phy.read(1, 2) as u32) << 16 | phy.read(1, 3) as u32, PHY_ID);
        assert_eq!(phy.read(0, 2), 0xffff);
        assert_eq!(phy.read(1, 1) & BMSR_LINK, 0);
        phy.set_link(true);
        assert_ne!(phy.read(1, 1) & BMSR_LINK, 0);
        phy.set_link(false); phy.set_link(true);
        assert_eq!(phy.read(1, 1) & BMSR_LINK, 0);
        assert_ne!(phy.read(1, 1) & BMSR_LINK, 0);
        phy.write(1, 0, BMCR_POWERDOWN);
        assert_eq!(phy.read(1, 1) & BMSR_LINK, 0);
    }
    #[test]
    fn extended_registers_are_independent_and_reset() {
        let mut phy = Yt8531::default();
        phy.write(1, 30, 0xa003); phy.write(1, 31, 0x1234);
        phy.write(1, 30, 0xa004); phy.write(1, 31, 0x5678);
        phy.write(1, 30, 0xa003); assert_eq!(phy.read(1, 31), 0x1234);
        phy.write(1, 0, BMCR_RESET);
        phy.write(1, 30, 0xa003); assert_eq!(phy.read(1, 31), 0);
        assert_eq!(phy.read(1, 2), (PHY_ID >> 16) as u16);
    }
}
