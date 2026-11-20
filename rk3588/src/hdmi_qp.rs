//! HDMI0's transmitter: the Synopsys DesignWare HDMI QP TX (`rockchip,rk3588-dw-hdmi-qp` at
//! 0xfde80000), as Linux's `dw-hdmi-qp` bridge drives it. The registers are kept as written;
//! the DDC master (I2CM) reads the monitor's EDID: setting FM_READ (or EXT_READ for a segment)
//! in I2CM_INTERFACE_CONTROL0 reads the byte at I2CM_ADDR of the slave at I2CM_SLVADDR into
//! I2CM_INTERFACE_RDDATA_0_3 and raises OP_DONE (NACK_RCVD for a slave other than the EDID's,
//! 0x50) in MAINUNIT_1_INT_STATUS, which reaches the "main" interrupt through
//! MAINUNIT_1_INT_MASK_N. Hot-plug is in the SYS GRF (see `crate::grf`): the monitor is always
//! connected.

pub const BASE: u64 = 0xfde8_0000;
pub const SIZE: u64 = 0x2_0000;
/// Interrupts: avp, cec, earc, main, hpd.
pub const IRQS: [usize; 5] = [32 + 169, 32 + 170, 32 + 171, 32 + 172, 32 + 360];
pub const IRQ_MAIN: usize = 32 + 172;

const I2CM_INTERFACE_CONTROL0: u64 = 0xf4;
const I2CM_INTERFACE_CONTROL1: u64 = 0xf8;
const I2CM_INTERFACE_RDDATA_0_3: u64 = 0x10c;
const MAINUNIT_1_INT_STATUS: u64 = 0x3020;
const MAINUNIT_1_INT_MASK_N: u64 = 0x3024;
const MAINUNIT_1_INT_CLEAR: u64 = 0x3028;
const MAINUNIT_1_INT_FORCE: u64 = 0x302c;
const LINK_CONFIG0: u64 = 0x968;

const I2CM_FM_WRITE: u32 = 1 << 1;
const I2CM_FM_READ: u32 = 1 << 2;
const I2CM_EXT_READ: u32 = 1 << 4;
const I2CM_OP_DONE: u32 = 1 << 0;
const I2CM_NACK_RCVD: u32 = 1 << 2;
const OPMODE_DVI: u32 = 1 << 0;
/// The EDID's DDC address.
const DDC_ADDR: u32 = 0x50;

pub struct HdmiQp {
    regs: Vec<u32>,
    edid: Vec<u8>,
    rddata: u32,
    int1: u32,
    /// EDID bytes read over DDC
    pub ddc_reads: u64,
}

impl HdmiQp {
    pub fn new(edid: Vec<u8>) -> Self {
        Self { regs: vec![0; (SIZE / 4) as usize], edid, rddata: 0, int1: 0, ddc_reads: 0 }
    }

    fn reg(&self, off: u64) -> u32 {
        self.regs.get((off / 4) as usize).copied().unwrap_or(0)
    }

    pub fn read(&mut self, off: u64) -> u32 {
        match off {
            I2CM_INTERFACE_RDDATA_0_3 => self.rddata,
            MAINUNIT_1_INT_STATUS => self.int1,
            MAINUNIT_1_INT_CLEAR | MAINUNIT_1_INT_FORCE => 0,
            _ => self.reg(off),
        }
    }

    pub fn write(&mut self, off: u64, v: u32) {
        let old = self.reg(off);
        if let Some(r) = self.regs.get_mut((off / 4) as usize) {
            *r = v;
        }
        match off {
            MAINUNIT_1_INT_CLEAR => self.int1 &= !v,
            MAINUNIT_1_INT_FORCE => self.int1 |= v,
            I2CM_INTERFACE_CONTROL0 => {
                let started = v & !old;
                if started & (I2CM_FM_READ | I2CM_EXT_READ) != 0 {
                    self.ddc_read(v);
                } else if started & I2CM_FM_WRITE != 0 {
                    // the EDID ROM takes writes of its offset only
                    self.int1 |= if (v >> 5) & 0x7f == DDC_ADDR { I2CM_OP_DONE } else { I2CM_NACK_RCVD };
                }
            }
            _ => {}
        }
    }

    fn ddc_read(&mut self, ctrl: u32) {
        let slave = (ctrl >> 5) & 0x7f;
        let offset = ((ctrl >> 12) & 0xff) as usize;
        let segment = if ctrl & I2CM_EXT_READ != 0 { ((self.reg(I2CM_INTERFACE_CONTROL1) >> 7) & 0xff) as usize } else { 0 };
        match self.edid.get(segment * 256 + offset) {
            Some(&b) if slave == DDC_ADDR => {
                self.rddata = u32::from(b);
                self.int1 |= I2CM_OP_DONE;
                self.ddc_reads += 1;
            }
            _ => self.int1 |= I2CM_NACK_RCVD,
        }
    }

    /// The "main" interrupt.
    pub fn irq(&self) -> bool {
        self.int1 & self.reg(MAINUNIT_1_INT_MASK_N) != 0
    }

    /// HDMI (not DVI) signalling was chosen.
    pub fn hdmi_mode(&self) -> bool {
        self.reg(LINK_CONFIG0) & OPMODE_DVI == 0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The byte-at-a-time DDC read Linux does for the EDID, then a NACK from another slave.
    #[test]
    fn edid_over_ddc() {
        let mut h = HdmiQp::new((0..=255u8).rev().collect());
        h.write(MAINUNIT_1_INT_MASK_N, I2CM_OP_DONE | I2CM_NACK_RCVD);
        let slave = DDC_ADDR << 5;
        for addr in [0u32, 1, 200] {
            h.write(I2CM_INTERFACE_CONTROL0, slave | addr << 12);
            h.write(I2CM_INTERFACE_CONTROL0, slave | addr << 12 | I2CM_FM_READ);
            assert!(h.irq());
            assert_eq!(h.read(MAINUNIT_1_INT_STATUS), I2CM_OP_DONE);
            h.write(MAINUNIT_1_INT_CLEAR, I2CM_OP_DONE);
            assert!(!h.irq());
            assert_eq!(h.read(I2CM_INTERFACE_RDDATA_0_3), 255 - addr);
            h.write(I2CM_INTERFACE_CONTROL0, slave | addr << 12);
        }
        h.write(I2CM_INTERFACE_CONTROL0, 0x37 << 5 | I2CM_FM_READ);
        assert_eq!(h.read(MAINUNIT_1_INT_STATUS), I2CM_NACK_RCVD);
    }
}
