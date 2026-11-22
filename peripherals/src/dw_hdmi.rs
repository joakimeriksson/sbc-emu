//! Synopsys DesignWare HDMI TX controller (Linux `dw-hdmi`) with a connected monitor, as the
//! Allwinner H3 and the Rockchip RK3288 use it, and the monitor's EDID.
//!
//! Only what the drivers observe is modelled: the identification registers, hot plug and
//! receiver sense in PHY_STAT0 with their interrupts, the controller's DDC I2C master (EDID
//! reads), the Synopsys PHY's I2C master (writes complete at once and are kept for reads),
//! and the interrupt mute and status registers. Everything else is register storage; the
//! picture itself comes from the SoC's display controller, not from this block.
//!
//! Registers are bytes at their `dw-hdmi` offsets; a SoC that spaces them out
//! (`reg-io-width = <4>` on the RK3288) divides the bus offset by four before calling in.
//! Boards whose device tree names a `ddc-i2c-bus` read the EDID through the SoC's own I2C
//! controller instead: attach an [`EdidEeprom`] there.

use crate::i2c::I2cDevice;

/// Register space, in bytes (the I2C master ends at 0x7e2x; the H3 decodes 64 KiB).
pub const SIZE: usize = 0x1_0000;

const DESIGN_ID: usize = 0x0000;
const IH_FIRST: usize = 0x0100;
const IH_LAST: usize = 0x0109;
const IH_PHY_STAT0: usize = 0x0104;
const IH_I2CM_STAT0: usize = 0x0105;
const IH_I2CMPHY_STAT0: usize = 0x0108;
const IH_MUTE_FIRST: usize = 0x0180;
const IH_MUTE: usize = 0x01ff;
const PHY_CONF0: usize = 0x3000;
const PHY_STAT0: usize = 0x3004;
const PHY_MASK0: usize = 0x3006;
const PHY_POL0: usize = 0x3007;
const PHY_I2CM_ADDRESS: usize = 0x3021;
const PHY_I2CM_DATAO_1: usize = 0x3022;
const PHY_I2CM_DATAO_0: usize = 0x3023;
const PHY_I2CM_DATAI_1: usize = 0x3024;
const PHY_I2CM_DATAI_0: usize = 0x3025;
const PHY_I2CM_OPERATION: usize = 0x3026;
const I2CM_SLAVE: usize = 0x7e00;
const I2CM_ADDRESS: usize = 0x7e01;
const I2CM_DATAI: usize = 0x7e03;
const I2CM_OPERATION: usize = 0x7e04;
const I2CM_SEGPTR: usize = 0x7e0a;
const I2CM_READ: u8 = 0x01;
const I2CM_READ_EXT: u8 = 0x02;
const I2CM_WRITE: u8 = 0x10;
const IH_I2CM_DONE: u8 = 0x02;
const IH_I2CM_ERROR: u8 = 0x01;
/// PHY_STAT0: TX PLL locked, hot plug detected, receiver sense on all four lanes.
const PHY_STAT_CONNECTED: u8 = 0xf3;
const PHY_TX_LOCK: u8 = 0x01;
/// PHY_CONF0 of the Synopsys PHYs: power down (PDDQ) and transmitter power (TXPWRON).
const PHY_CONF0_GEN2_PDDQ: u8 = 0x10;
const PHY_CONF0_GEN2_TXPWRON: u8 = 0x08;
/// CONFIG2_ID of a vendor PHY (not driven through PHY_CONF0).
const PHY_VENDOR: u8 = 0xfe;
/// (PHY_STAT0 bit, IH_PHY_STAT0 bit): hot plug and the four receiver-sense lines.
const PHY_INTERRUPTS: [(u8, u8); 5] = [(0x02, 0x01), (0x10, 0x04), (0x20, 0x08), (0x40, 0x10), (0x80, 0x20)];
/// The monitor's EDID EEPROM on the DDC bus.
pub const EDID_ADDRESS: u8 = 0x50;

/// What the controller reports about itself (DESIGN_ID, REVISION, PRODUCT_ID0/1,
/// CONFIG0..3_ID at offsets 0..7).
#[derive(Clone, Copy, Debug)]
pub struct Identity(pub [u8; 8]);

impl Identity {
    /// The H3's: v1.32a with Allwinner's own PHY (CONFIG2 0xfe, vendor PHY).
    pub const H3: Identity = Identity([0x13, 0x2a, 0xa0, 0x01, 0, 0, 0xfe, 0]);
    /// The RK3288's: v2.00a with HDCP, I2S audio input (CONFIG0 bit 4) and the Synopsys
    /// HDMI 2.0 TX PHY (CONFIG2 0xf3), which Linux programs over the PHY I2C master.
    pub const RK3288: Identity = Identity([0x20, 0x0a, 0xa0, 0xc1, 0x10, 0, 0xf3, 0]);
}

/// A display timing, as an EDID detailed timing descriptor describes it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Mode {
    pub clock_khz: u32,
    pub width: u32,
    pub hfp: u32,
    pub hsync: u32,
    pub hbp: u32,
    pub height: u32,
    pub vfp: u32,
    pub vsync: u32,
    pub vbp: u32,
    pub positive_sync: bool,
}

impl Mode {
    /// The monitor modes offered as the EDID's preferred timing (`--hdmi-mode WxH`).
    pub const MODES: [Mode; 3] = [
        Mode {
            clock_khz: 65_000,
            width: 1024,
            hfp: 24,
            hsync: 136,
            hbp: 160,
            height: 768,
            vfp: 3,
            vsync: 6,
            vbp: 29,
            positive_sync: false,
        },
        Mode {
            clock_khz: 74_250,
            width: 1280,
            hfp: 110,
            hsync: 40,
            hbp: 220,
            height: 720,
            vfp: 5,
            vsync: 5,
            vbp: 20,
            positive_sync: true,
        },
        Mode {
            clock_khz: 148_500,
            width: 1920,
            hfp: 88,
            hsync: 44,
            hbp: 148,
            height: 1080,
            vfp: 4,
            vsync: 5,
            vbp: 36,
            positive_sync: true,
        },
    ];

    pub fn find(width: u32, height: u32) -> Option<Mode> {
        Self::MODES.iter().copied().find(|m| m.width == width && m.height == height)
    }

    /// `--hdmi-mode WxH`: one of [`Mode::MODES`].
    pub fn parse(spec: &str) -> Result<Mode, String> {
        let (w, h) = spec.split_once('x').ok_or("--hdmi-mode WxH")?;
        let (w, h) = (w.parse().map_err(|_| "--hdmi-mode WxH")?, h.parse().map_err(|_| "--hdmi-mode WxH")?);
        Mode::find(w, h).ok_or_else(|| "--hdmi-mode: the emulated monitor offers 1024x768, 1280x720 and 1920x1080".into())
    }

    /// 18-byte EDID detailed timing descriptor.
    fn descriptor(&self) -> [u8; 18] {
        let hblank = self.hfp + self.hsync + self.hbp;
        let vblank = self.vfp + self.vsync + self.vbp;
        let (hmm, vmm) = (self.width * 520 / 1920, self.height * 290 / 1080);
        let clock = (self.clock_khz / 10) as u16;
        [
            clock as u8,
            (clock >> 8) as u8,
            self.width as u8,
            hblank as u8,
            ((self.width >> 8) << 4 | hblank >> 8) as u8,
            self.height as u8,
            vblank as u8,
            ((self.height >> 8) << 4 | vblank >> 8) as u8,
            self.hfp as u8,
            self.hsync as u8,
            ((self.vfp & 0xf) << 4 | self.vsync & 0xf) as u8,
            ((self.hfp >> 8) << 6 | (self.hsync >> 8) << 4 | (self.vfp >> 4) << 2 | self.vsync >> 4) as u8,
            hmm as u8,
            vmm as u8,
            ((hmm >> 8) << 4 | vmm >> 8) as u8,
            0,
            0,
            // digital separate sync, with the polarities of the mode
            0x18 | if self.positive_sync { 0x06 } else { 0 },
        ]
    }
}

/// A 128-byte EDID 1.3 for a DVI/HDMI monitor whose preferred mode is `mode`; it also lists
/// 640x480, 800x600 and 1024x768 at 60 Hz.
pub fn edid(mode: &Mode) -> [u8; 128] {
    let mut e = [0u8; 128];
    e[..8].copy_from_slice(&[0, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0]);
    let id = |c: u8| (c - b'A' + 1) as u16;
    let maker = id(b'S') << 10 | id(b'B') << 5 | id(b'E');
    e[8..10].copy_from_slice(&maker.to_be_bytes());
    e[10..12].copy_from_slice(&1u16.to_le_bytes()); // product
    e[16] = 1; // week
    e[17] = (2026 - 1990) as u8;
    e[18..20].copy_from_slice(&[1, 3]);
    e[20] = 0x80; // digital input
    e[21] = 52; // cm
    e[22] = 29;
    e[23] = 120; // gamma 2.2
    e[24] = 0x0a; // RGB, preferred timing is the first descriptor
    e[25..35].copy_from_slice(&[0xee, 0x91, 0xa3, 0x54, 0x4c, 0x99, 0x26, 0x0f, 0x50, 0x54]); // sRGB
    e[35] = 0x21; // 640x480@60, 800x600@60
    e[36] = 0x08; // 1024x768@60
    for k in 0..8 {
        e[38 + 2 * k] = 1;
        e[39 + 2 * k] = 1;
    }
    e[54..72].copy_from_slice(&mode.descriptor());
    // monitor name
    e[72..77].copy_from_slice(&[0, 0, 0, 0xfc, 0]);
    let name = b"sbc-emu HDMI\n";
    e[77..77 + name.len()].copy_from_slice(name);
    // range limits: 50-75 Hz, 30-83 kHz, 170 MHz
    e[90..108].copy_from_slice(&[0, 0, 0, 0xfd, 0, 50, 75, 30, 83, 17, 0, 0x0a, 0x20, 0x20, 0x20, 0x20, 0x20, 0x20]);
    e[108..113].copy_from_slice(&[0, 0, 0, 0x10, 0]); // dummy descriptor
    let sum = e[..127].iter().fold(0u8, |s, &b| s.wrapping_add(b));
    e[127] = 0u8.wrapping_sub(sum);
    e
}

/// The monitor's EDID EEPROM at [`EDID_ADDRESS`] on a DDC bus driven by an ordinary I2C
/// controller: a write sets the byte pointer, reads return bytes from it onwards.
pub struct EdidEeprom {
    data: [u8; 128],
    pointer: u8,
    /// The next written byte is the pointer (the first byte after a START for writing).
    pointer_next: bool,
}

impl EdidEeprom {
    pub fn new(mode: &Mode) -> Self {
        Self { data: edid(mode), pointer: 0, pointer_next: false }
    }
}

impl I2cDevice for EdidEeprom {
    fn start(&mut self, read: bool) -> bool {
        self.pointer_next = !read;
        true
    }

    fn write(&mut self, byte: u8) -> bool {
        if self.pointer_next {
            self.pointer = byte;
            self.pointer_next = false;
        }
        true
    }

    fn read(&mut self) -> u8 {
        // one 128-byte block (no extensions); a 256-byte part repeats it
        let byte = self.data[(self.pointer & 0x7f) as usize];
        self.pointer = self.pointer.wrapping_add(1);
        byte
    }
}

/// The controller's registers and its monitor.
pub struct DwHdmi {
    regs: Vec<u8>,
    edid: [u8; 128],
    /// Synopsys PHY registers written over the PHY I2C master (16-bit, by PHY address).
    phy: [u16; 256],
}

impl DwHdmi {
    pub fn new(id: Identity, mode: &Mode) -> Self {
        let mut regs = vec![0u8; SIZE];
        regs[DESIGN_ID..DESIGN_ID + 8].copy_from_slice(&id.0);
        regs[PHY_STAT0] = PHY_STAT_CONNECTED;
        if id.0[6] != PHY_VENDOR {
            // a Synopsys PHY starts powered down; its PLL locks when it is powered on
            regs[PHY_STAT0] &= !PHY_TX_LOCK;
        }
        regs[IH_MUTE] = 0x03;
        Self { regs, edid: edid(mode), phy: [0; 256] }
    }

    /// The byte register at `offset`.
    pub fn read(&self, offset: usize) -> u8 {
        self.regs.get(offset).copied().unwrap_or(0)
    }

    pub fn write(&mut self, offset: usize, value: u8) {
        if offset >= SIZE {
            return;
        }
        match offset {
            // identification and PHY status are read-only
            0x0000..=0x0007 | PHY_STAT0 => {}
            // interrupt status: write one to clear
            IH_FIRST..=IH_LAST => self.regs[offset] &= !value,
            I2CM_OPERATION => self.i2c_operation(value),
            PHY_I2CM_OPERATION => self.phy_operation(value),
            PHY_CONF0 => {
                self.regs[offset] = value;
                if self.regs[6] != PHY_VENDOR {
                    // the PLL locks while the transmitter is powered and not powered down
                    let on = value & PHY_CONF0_GEN2_TXPWRON != 0 && value & PHY_CONF0_GEN2_PDDQ == 0;
                    self.regs[PHY_STAT0] = self.regs[PHY_STAT0] & !PHY_TX_LOCK | if on { PHY_TX_LOCK } else { 0 };
                }
            }
            _ => self.regs[offset] = value,
        }
        self.phy_interrupts();
    }

    /// One DDC transfer of the I2C master: the monitor answers at the EDID address only.
    fn i2c_operation(&mut self, op: u8) {
        let device = self.regs[I2CM_SLAVE] & 0x7f;
        let register = self.regs[I2CM_ADDRESS] as usize;
        let status = if device != EDID_ADDRESS {
            IH_I2CM_ERROR
        } else if op & (I2CM_READ | I2CM_READ_EXT) != 0 {
            let segment = if op & I2CM_READ_EXT != 0 { self.regs[I2CM_SEGPTR] as usize } else { 0 };
            self.regs[I2CM_DATAI] = self.edid.get(segment * 256 + register).copied().unwrap_or(0xff);
            IH_I2CM_DONE
        } else if op & I2CM_WRITE != 0 {
            IH_I2CM_DONE
        } else {
            return;
        };
        self.regs[IH_I2CM_STAT0] |= status;
    }

    /// One access of the PHY I2C master (`dw_hdmi_phy_i2c_write`): completes at once.
    fn phy_operation(&mut self, op: u8) {
        let address = self.regs[PHY_I2CM_ADDRESS] as usize;
        if op & I2CM_WRITE != 0 {
            self.phy[address] = u16::from_be_bytes([self.regs[PHY_I2CM_DATAO_1], self.regs[PHY_I2CM_DATAO_0]]);
        } else if op & I2CM_READ != 0 {
            let [hi, lo] = self.phy[address].to_be_bytes();
            self.regs[PHY_I2CM_DATAI_1] = hi;
            self.regs[PHY_I2CM_DATAI_0] = lo;
        } else {
            return;
        }
        self.regs[IH_I2CMPHY_STAT0] |= IH_I2CM_DONE;
    }

    /// A PHY status line at the level PHY_POL0 selects raises its interrupt (the driver
    /// flips the polarity after each hot plug or receiver-sense interrupt).
    fn phy_interrupts(&mut self) {
        let (stat, mask, pol) = (self.regs[PHY_STAT0], self.regs[PHY_MASK0], self.regs[PHY_POL0]);
        for (line, interrupt) in PHY_INTERRUPTS {
            if mask & line == 0 && stat & line == pol & line {
                self.regs[IH_PHY_STAT0] |= interrupt;
            }
        }
    }

    pub fn irq(&self) -> bool {
        if self.regs[IH_MUTE] & 1 != 0 {
            return false;
        }
        (IH_FIRST..=IH_LAST).any(|r| self.regs[r] & !self.regs[IH_MUTE_FIRST + r - IH_FIRST] != 0)
    }
}

emu_core::snap_struct!(Mode { clock_khz, width, hfp, hsync, hbp, height, vfp, vsync, vbp, positive_sync });
emu_core::snap_struct!(EdidEeprom { data, pointer, pointer_next });
emu_core::snap_struct!(DwHdmi { edid, phy } fixed { regs });

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn edid_is_valid_and_prefers_the_mode() {
        let e = edid(&Mode::MODES[1]);
        assert_eq!(e.iter().fold(0u8, |s, &b| s.wrapping_add(b)), 0, "checksum");
        assert_eq!(&e[..8], &[0, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0]);
        assert_eq!(u16::from_le_bytes([e[54], e[55]]), 7425, "74.25 MHz");
        assert_eq!(e[56] as u32 | (e[58] as u32 >> 4) << 8, 1280);
        assert_eq!(e[59] as u32 | (e[61] as u32 >> 4) << 8, 720);
    }

    #[test]
    fn identifies_and_reads_edid_over_ddc() {
        let mut d = DwHdmi::new(Identity::H3, &Mode::MODES[1]);
        assert_eq!([d.read(0), d.read(1), d.read(2), d.read(3)], [0x13, 0x2a, 0xa0, 0x01]);
        assert_eq!(d.read(6), 0xfe, "vendor PHY");
        d.write(IH_MUTE, 0);
        d.write(IH_MUTE_FIRST + 5, 0); // unmute I2CM
        d.write(I2CM_SLAVE, 0x50);
        let mut bytes = Vec::new();
        for register in 0..128u8 {
            d.write(I2CM_ADDRESS, register);
            d.write(I2CM_OPERATION, I2CM_READ);
            assert!(d.irq());
            assert_eq!(d.read(IH_I2CM_STAT0), IH_I2CM_DONE);
            d.write(IH_I2CM_STAT0, IH_I2CM_DONE);
            assert!(!d.irq());
            bytes.push(d.read(I2CM_DATAI));
        }
        assert_eq!(bytes, edid(&Mode::MODES[1]).to_vec());
        d.write(I2CM_SLAVE, 0x37);
        d.write(I2CM_OPERATION, I2CM_READ);
        assert_eq!(d.read(IH_I2CM_STAT0), IH_I2CM_ERROR);
    }

    #[test]
    fn hot_plug_interrupt_follows_polarity() {
        let mut d = DwHdmi::new(Identity::H3, &Mode::MODES[1]);
        d.write(IH_MUTE, 0);
        d.write(PHY_MASK0, !0x02);
        d.write(PHY_POL0, 0x02); // interrupt while plugged
        assert_ne!(d.read(IH_PHY_STAT0) & 1, 0);
        assert!(d.irq());
        d.write(PHY_POL0, 0); // now wait for unplug
        d.write(IH_PHY_STAT0, 0xff);
        assert!(!d.irq());
    }

    #[test]
    fn phy_i2c_writes_complete_and_read_back() {
        let mut d = DwHdmi::new(Identity::RK3288, &Mode::MODES[1]);
        assert_eq!(d.read(6), 0xf3, "Synopsys HDMI 2.0 TX PHY");
        assert_eq!(d.read(PHY_STAT0) & 1, 0, "powered down");
        d.write(PHY_CONF0, PHY_CONF0_GEN2_TXPWRON);
        assert_eq!(d.read(PHY_STAT0) & 1, 1, "TX PLL locked");
        d.write(PHY_CONF0, PHY_CONF0_GEN2_PDDQ);
        assert_eq!(d.read(PHY_STAT0) & 1, 0, "unlocked when powered down");
        assert_eq!(d.read(PHY_STAT0) & 2, 2, "the monitor stays plugged in");
        d.write(IH_I2CMPHY_STAT0, 0xff);
        d.write(PHY_I2CM_ADDRESS, 0x06);
        d.write(PHY_I2CM_DATAO_1, 0x00);
        d.write(PHY_I2CM_DATAO_0, 0x08);
        d.write(PHY_I2CM_OPERATION, I2CM_WRITE);
        assert_eq!(d.read(IH_I2CMPHY_STAT0) & 3, IH_I2CM_DONE);
        d.write(IH_I2CMPHY_STAT0, 0xff);
        d.write(PHY_I2CM_OPERATION, I2CM_READ);
        assert_eq!([d.read(PHY_I2CM_DATAI_1), d.read(PHY_I2CM_DATAI_0)], [0x00, 0x08]);
    }

    #[test]
    fn eeprom_reads_from_the_written_pointer() {
        let mut e = EdidEeprom::new(&Mode::MODES[2]);
        let block = edid(&Mode::MODES[2]);
        assert!(e.start(false));
        e.write(54);
        e.stop();
        assert!(e.start(true));
        assert_eq!([e.read(), e.read()], [block[54], block[55]]);
        assert_eq!(u16::from_le_bytes([block[54], block[55]]), 14850, "148.5 MHz");
    }
}
