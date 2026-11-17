//! Synopsys DesignWare MIPI DSI host with the RK3288's internal D-PHY
//! (`rockchip,rk3288-mipi-dsi`, `snps,dw-mipi-dsi`) at 0xff960000.
//!
//! The picture itself reaches the panel from the VOP over DPI, so the model only has to
//! satisfy the driver: the PHY reports lock and stop state, the generic command FIFO is
//! always ready, and commands the panel driver sends (DCS and generic writes) are counted
//! and, with `SBC_VOP_LOG=1`, printed with every register write. (The panel board's panel
//! driver sends none: it takes over the panel the vendor's U-Boot initialised for its splash.) Reads get a short reply from a cooperative panel: the power mode reads back as "on",
//! anything else as zeros.

pub const DSI_SIZE: u64 = 0x4000;

const VERSION: u64 = 0x00;
const GEN_HDR: u64 = 0x6c;
const GEN_PLD_DATA: u64 = 0x70;
const CMD_PKT_STATUS: u64 = 0x74;
const PHY_STATUS: u64 = 0xb0;
const INT_ST0: u64 = 0xbc;
const INT_ST1: u64 = 0xc0;

const GEN_CMD_EMPTY: u32 = 1 << 0;
const GEN_PLD_W_EMPTY: u32 = 1 << 2;
const GEN_PLD_R_EMPTY: u32 = 1 << 4;
/// PHY lock, clock lane and the four data lanes in stop state.
const PHY_READY: u32 = 1 << 0 | 1 << 2 | 1 << 4 | 1 << 7 | 1 << 9 | 1 << 11;

/// DSI data types the host sends.
const DCS_READ: u8 = 0x06;
const GENERIC_READ: [u8; 3] = [0x04, 0x14, 0x24];
const MAX_RETURN_SIZE: u8 = 0x37;
/// DCS get_power_mode and its reply for a panel that is on (booster, normal mode, display on).
const DCS_GET_POWER_MODE: u8 = 0x0a;
const POWER_MODE_ON: u8 = 0x9c;

pub struct Dsi {
    regs: Vec<u32>,
    /// Payload words written before the header that sends them.
    payload: Vec<u8>,
    /// Bytes of the pending read response.
    reply: std::collections::VecDeque<u8>,
    max_return: usize,
    /// Packets sent to the panel.
    pub packets: u64,
    /// Print each packet to stderr (`SBC_VOP_LOG=1`).
    pub log: bool,
}

impl Default for Dsi {
    fn default() -> Self {
        Self {
            regs: Vec::new(),
            payload: Vec::new(),
            reply: Default::default(),
            max_return: 0,
            packets: 0,
            log: std::env::var_os("SBC_VOP_LOG").is_some(),
        }
    }
}

/// Long packets (a word count and a payload) have data type low nibbles 9, A, C, D and E
/// (null, blanking, generic and DCS long writes, PPS, pixel streams); the rest are short.
fn is_long(data_type: u8) -> bool {
    matches!(data_type & 0xf, 0x9 | 0xa | 0xc | 0xd | 0xe)
}

impl Dsi {
    pub fn read(&mut self, offset: u64) -> u32 {
        match offset {
            // DW MIPI DSI 1.30a, the RK3288's revision.
            VERSION => 0x3133_302a,
            CMD_PKT_STATUS => {
                GEN_CMD_EMPTY | GEN_PLD_W_EMPTY | if self.reply.is_empty() { GEN_PLD_R_EMPTY } else { 0 }
            }
            GEN_PLD_DATA => {
                let mut word = 0;
                for i in 0..4 {
                    word |= (self.reply.pop_front().unwrap_or(0) as u32) << (8 * i);
                }
                word
            }
            PHY_STATUS => PHY_READY,
            INT_ST0 | INT_ST1 => 0,
            o => self.regs.get(o as usize / 4).copied().unwrap_or(0),
        }
    }

    pub fn write(&mut self, offset: u64, value: u32) {
        if self.log {
            eprintln!("[dsi] write {offset:#05x} = {value:#010x}");
        }
        match offset {
            GEN_PLD_DATA => self.payload.extend_from_slice(&value.to_le_bytes()),
            GEN_HDR => {
                self.packet(value);
            }
            o if o < 0x100 => {
                let index = o as usize / 4;
                if self.regs.len() <= index {
                    self.regs.resize(index + 1, 0);
                }
                self.regs[index] = value;
            }
            _ => {}
        }
    }

    /// A packet header: data type in bits 0-5, then either two parameter bytes (short
    /// packet) or the payload byte count (long packet, payload already in the FIFO). A long
    /// packet takes the whole FIFO: the driver pads the last word, and nothing written for
    /// one packet may lead the next. Returns the packet (data type, payload) as sent.
    fn packet(&mut self, header: u32) -> (u8, Vec<u8>) {
        let data_type = (header & 0x3f) as u8;
        let (lo, hi) = ((header >> 8) as u8, (header >> 16) as u8);
        let bytes: Vec<u8> = if is_long(data_type) {
            let count = lo as usize | (hi as usize) << 8;
            let mut fifo = std::mem::take(&mut self.payload);
            fifo.truncate(count);
            fifo
        } else {
            vec![lo, hi]
        };
        if data_type == MAX_RETURN_SIZE {
            self.max_return = lo as usize | (hi as usize) << 8;
        } else if data_type == DCS_READ || GENERIC_READ.contains(&data_type) {
            let len = self.max_return.max(1);
            let first = if data_type == DCS_READ && lo == DCS_GET_POWER_MODE { POWER_MODE_ON } else { 0 };
            self.reply = std::iter::once(first).chain(std::iter::repeat_n(0, len - 1)).collect();
        }
        self.packets += 1;
        if self.log {
            eprintln!("[dsi] packet {data_type:#04x} {bytes:02x?}");
        }
        (data_type, bytes)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn long_writes_take_their_payload_and_power_mode_reads_on() {
        let mut dsi = Dsi::default();
        assert_eq!(dsi.read(PHY_STATUS) & 1, 1);
        dsi.write(GEN_PLD_DATA, 0x0403_02b0);
        assert_eq!(dsi.packet(4 << 8 | 0x39), (0x39, vec![0xb0, 2, 3, 4]));
        // Another long type (PPS, 0x0a) and a padded word leave nothing behind.
        dsi.write(GEN_PLD_DATA, 0x1111_1111);
        dsi.write(GEN_PLD_DATA, 0x2222_2222);
        assert_eq!(dsi.packet(5 << 8 | 0x0a).1.len(), 5);
        dsi.write(GEN_PLD_DATA, 0x0000_2911);
        assert_eq!(dsi.packet(2 << 8 | 0x39), (0x39, vec![0x11, 0x29]));
        assert_eq!(dsi.packets, 3);
        dsi.write(GEN_HDR, 1 << 8 | MAX_RETURN_SIZE as u32);
        dsi.write(GEN_HDR, (DCS_GET_POWER_MODE as u32) << 8 | DCS_READ as u32);
        assert_eq!(dsi.read(CMD_PKT_STATUS) & GEN_PLD_R_EMPTY, 0);
        assert_eq!(dsi.read(GEN_PLD_DATA) & 0xff, POWER_MODE_ON as u32);
        assert_ne!(dsi.read(CMD_PKT_STATUS) & GEN_PLD_R_EMPTY, 0);
    }
}
