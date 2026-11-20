//! The firmware's SCMI services (as Trusted Firmware-A provides them on the RK3588), over
//! the SMC transport: Linux writes a message into the shared memory at 0x10f000 and calls
//! SMC 0x82000010; the reply is in place when the call returns. Base (v2.0), clock (v1.0)
//! and reset (v1.0) protocols: the clocks are the display's (the CRU is not modelled), with
//! any rate settable (each is a range with step 1) and remembered; resets are accepted.
//!
//! Shared memory: +0x04 channel status (bit 0: free, set again after every reply), +0x14
//! length (4 + payload), +0x18 message header, +0x1c payload (the reply: status, then data).

pub const SHMEM: u64 = 0x0010_f000;
pub const SHMEM_SIZE: u64 = 0x100;
pub const SMC_ID: u32 = 0x8200_0010;

const PROTO_BASE: u8 = 0x10;
const PROTO_CLOCK: u8 = 0x14;
const PROTO_RESET: u8 = 0x16;

const SUCCESS: i32 = 0;
const NOT_SUPPORTED: i32 = -1;
const INVALID_PARAMETERS: i32 = -2;
const NOT_FOUND: i32 = -4;

/// A clock: its name (unique in Linux's clock tree), rate (Hz) and whether it is on.
pub struct Clock {
    pub name: &'static str,
    pub rate: u64,
    pub enabled: bool,
}

/// The clock IDs in the DT (`scmi_clk N`).
pub mod clk {
    pub const ACLK_VOP: u32 = 0;
    pub const HCLK_VOP: u32 = 1;
    pub const DCLK_VP0: u32 = 2;
    pub const DCLK_VP1: u32 = 3;
    pub const DCLK_VP2: u32 = 4;
    pub const DCLK_VP3: u32 = 5;
    pub const PCLK_VOP: u32 = 6;
    pub const HDMI0_PCLK: u32 = 7;
    pub const HDMI0_EARC: u32 = 8;
    pub const HDMI0_REF: u32 = 9;
    pub const HDMI0_AUD: u32 = 10;
    pub const HDMI0_HDP: u32 = 11;
    pub const HCLK_VO1: u32 = 12;
    pub const HDPTX_REF: u32 = 13;
    pub const HDPTX_APB: u32 = 14;
}

/// The reset IDs in the DT (`scmi_reset N`).
pub mod rst {
    pub const HDMI0_REF: u32 = 0;
    pub const HDMI0_HDP: u32 = 1;
    pub const HDPTX_APB: u32 = 2;
    pub const HDPTX_INIT: u32 = 3;
    pub const HDPTX_CMN: u32 = 4;
    pub const HDPTX_LANE: u32 = 5;
}

const CLOCKS: [(&str, u64); 15] = [
    ("scmi_aclk_vop", 750_000_000),
    ("scmi_hclk_vop", 200_000_000),
    ("scmi_dclk_vp0", 74_250_000),
    ("scmi_dclk_vp1", 74_250_000),
    ("scmi_dclk_vp2", 74_250_000),
    ("scmi_dclk_vp3", 74_250_000),
    ("scmi_pclk_vop", 100_000_000),
    ("scmi_hdmi0_pclk", 100_000_000),
    ("scmi_hdmi0_earc", 200_000_000),
    ("scmi_hdmi0_ref", 24_000_000),
    ("scmi_hdmi0_aud", 24_576_000),
    ("scmi_hdmi0_hdp", 24_000_000),
    ("scmi_hclk_vo1", 200_000_000),
    ("scmi_hdptx_ref", 24_000_000),
    ("scmi_hdptx_apb", 100_000_000),
];
const RESETS: [&str; 6] = ["scmi_rst_hdmi_ref", "scmi_rst_hdmi_hdp", "scmi_rst_ptx_apb", "scmi_rst_ptx_ini", "scmi_rst_ptx_cmn", "scmi_rst_ptx_lan"];

pub struct Scmi {
    pub clocks: Vec<Clock>,
    /// whether each reset is asserted
    pub resets: Vec<bool>,
    /// messages served
    pub calls: u64,
}

impl Default for Scmi {
    fn default() -> Self {
        Self { clocks: CLOCKS.iter().map(|&(name, rate)| Clock { name, rate, enabled: false }).collect(), resets: vec![false; RESETS.len()], calls: 0 }
    }
}

fn name16(s: &str) -> [u8; 16] {
    let mut n = [0u8; 16];
    n[..s.len().min(15)].copy_from_slice(&s.as_bytes()[..s.len().min(15)]);
    n
}

impl Scmi {
    /// The channel starts free.
    pub fn init_channel(ram: &mut [u8]) {
        let a = SHMEM as usize;
        if let Some(s) = ram.get_mut(a..a + SHMEM_SIZE as usize) {
            s.fill(0);
            s[4..8].copy_from_slice(&1u32.to_le_bytes());
        }
    }

    /// Serve the message in the shared memory.
    pub fn handle(&mut self, ram: &mut [u8]) {
        let a = SHMEM as usize;
        let Some(sh) = ram.get_mut(a..a + SHMEM_SIZE as usize) else { return };
        let rd = |sh: &[u8], o: usize| u32::from_le_bytes(sh[o..o + 4].try_into().unwrap());
        let header = rd(sh, 0x18);
        let len = (rd(sh, 0x14) as usize).clamp(4, SHMEM_SIZE as usize - 0x18);
        let req: Vec<u32> = sh[0x1c..0x18 + len].chunks(4).map(|c| { let mut w = [0u8; 4]; w[..c.len()].copy_from_slice(c); u32::from_le_bytes(w) }).collect();
        let (msg, proto) = ((header & 0xff) as u8, ((header >> 10) & 0xff) as u8);
        let arg = |i: usize| req.get(i).copied().unwrap_or(0);
        self.calls += 1;
        let (status, data) = self.message(proto, msg, &arg);
        let resp = &mut sh[0x1c..];
        resp[..4].copy_from_slice(&status.to_le_bytes());
        let n = data.len().min(resp.len() - 4);
        resp[4..4 + n].copy_from_slice(&data[..n]);
        sh[0x14..0x18].copy_from_slice(&((8 + n) as u32).to_le_bytes());
        sh[4..8].copy_from_slice(&1u32.to_le_bytes()); // free again
    }

    fn message(&mut self, proto: u8, msg: u8, arg: &dyn Fn(usize) -> u32) -> (i32, Vec<u8>) {
        let words = |w: &[u32]| w.iter().flat_map(|v| v.to_le_bytes()).collect::<Vec<u8>>();
        match (proto, msg) {
            (PROTO_BASE, 0) => (SUCCESS, words(&[0x2_0000])),
            (PROTO_BASE, 1) => (SUCCESS, words(&[2])), // two protocols, no other agents
            (PROTO_BASE, 3) => (SUCCESS, name16("sbc-emu").to_vec()),
            (PROTO_BASE, 4) => (SUCCESS, name16("rk3588").to_vec()),
            (PROTO_BASE, 5) => (SUCCESS, words(&[1])),
            (PROTO_BASE, 6) => {
                let list = [PROTO_CLOCK, PROTO_RESET];
                let rest: Vec<u8> = list.iter().copied().skip(arg(0) as usize).collect();
                let mut d = words(&[rest.len() as u32]);
                d.extend_from_slice(&rest);
                d.resize(4 + rest.len().div_ceil(4) * 4, 0);
                (SUCCESS, d)
            }
            (PROTO_CLOCK, 0) | (PROTO_RESET, 0) => (SUCCESS, words(&[0x1_0000])),
            (PROTO_CLOCK, 1) => (SUCCESS, words(&[self.clocks.len() as u32])), // max_async_req 0
            (PROTO_CLOCK, 3) => match self.clocks.get(arg(0) as usize) {
                Some(c) => {
                    let mut d = words(&[c.enabled as u32]);
                    d.extend_from_slice(&name16(c.name));
                    (SUCCESS, d)
                }
                None => (NOT_FOUND, Vec::new()),
            },
            (PROTO_CLOCK, 4) => match self.clocks.get(arg(0) as usize) {
                // a range [1 kHz, 2 GHz] in steps of 1 Hz: Linux asks for exact rates
                Some(_) => (SUCCESS, words(&[3 | 1 << 12, 1000, 0, 2_000_000_000, 0, 1, 0])),
                None => (NOT_FOUND, Vec::new()),
            },
            (PROTO_CLOCK, 5) => match self.clocks.get_mut(arg(1) as usize) {
                Some(c) => {
                    c.rate = u64::from(arg(2)) | u64::from(arg(3)) << 32;
                    (SUCCESS, Vec::new())
                }
                None => (NOT_FOUND, Vec::new()),
            },
            (PROTO_CLOCK, 6) => match self.clocks.get(arg(0) as usize) {
                Some(c) => (SUCCESS, words(&[c.rate as u32, (c.rate >> 32) as u32])),
                None => (NOT_FOUND, Vec::new()),
            },
            (PROTO_CLOCK, 7) => match self.clocks.get_mut(arg(0) as usize) {
                Some(c) => {
                    c.enabled = arg(1) & 1 != 0;
                    (SUCCESS, Vec::new())
                }
                None => (NOT_FOUND, Vec::new()),
            },
            (PROTO_RESET, 1) => (SUCCESS, words(&[self.resets.len() as u32])),
            (PROTO_RESET, 3) => match RESETS.get(arg(0) as usize) {
                Some(n) => {
                    let mut d = words(&[0, 0]);
                    d.extend_from_slice(&name16(n));
                    (SUCCESS, d)
                }
                None => (NOT_FOUND, Vec::new()),
            },
            (PROTO_RESET, 4) => match self.resets.get_mut(arg(0) as usize) {
                Some(r) => {
                    // assert (bit 1), deassert, or a pulse (autonomous, bit 0)
                    *r = arg(1) & 3 == 2;
                    (SUCCESS, Vec::new())
                }
                None => (INVALID_PARAMETERS, Vec::new()),
            },
            _ => (NOT_SUPPORTED, Vec::new()),
        }
    }

    pub fn rate(&self, id: u32) -> u64 {
        self.clocks.get(id as usize).map_or(0, |c| c.rate)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn call(s: &mut Scmi, ram: &mut [u8], proto: u8, msg: u8, args: &[u32]) -> (i32, Vec<u32>) {
        let a = SHMEM as usize;
        let sh = &mut ram[a..a + 0x100];
        assert_eq!(sh[4] & 1, 1, "channel free");
        sh[4..8].copy_from_slice(&0u32.to_le_bytes());
        sh[0x14..0x18].copy_from_slice(&(4 + 4 * args.len() as u32).to_le_bytes());
        sh[0x18..0x1c].copy_from_slice(&((proto as u32) << 10 | msg as u32 | 7 << 18).to_le_bytes());
        for (i, v) in args.iter().enumerate() {
            sh[0x1c + 4 * i..0x20 + 4 * i].copy_from_slice(&v.to_le_bytes());
        }
        s.handle(ram);
        let sh = &ram[a..a + 0x100];
        let len = u32::from_le_bytes(sh[0x14..0x18].try_into().unwrap()) as usize;
        let w = |o: usize| u32::from_le_bytes(sh[o..o + 4].try_into().unwrap());
        (w(0x1c) as i32, (0x20..0x18 + len).step_by(4).map(w).collect())
    }

    #[test]
    fn linux_discovery_and_a_rate_change() {
        let mut ram = vec![0u8; 2 << 20];
        Scmi::init_channel(&mut ram);
        let mut s = Scmi::default();
        assert_eq!(call(&mut s, &mut ram, PROTO_BASE, 0, &[]), (0, vec![0x2_0000]));
        assert_eq!(call(&mut s, &mut ram, PROTO_BASE, 6, &[0]), (0, vec![2, 0x1614]));
        assert_eq!(call(&mut s, &mut ram, PROTO_BASE, 6, &[2]).1, vec![0]);
        let (st, attr) = call(&mut s, &mut ram, PROTO_CLOCK, 3, &[clk::DCLK_VP0]);
        assert_eq!((st, attr[0]), (0, 0));
        assert_eq!(&attr[1].to_le_bytes(), b"scmi");
        assert_eq!(call(&mut s, &mut ram, PROTO_CLOCK, 4, &[2, 0]).1[0], 3 | 1 << 12);
        assert_eq!(call(&mut s, &mut ram, PROTO_CLOCK, 5, &[0, clk::DCLK_VP0, 148_500_000, 0]).0, 0);
        assert_eq!(call(&mut s, &mut ram, PROTO_CLOCK, 6, &[clk::DCLK_VP0]).1, vec![148_500_000, 0]);
        assert_eq!(call(&mut s, &mut ram, PROTO_RESET, 4, &[rst::HDPTX_CMN, 2, 0]).0, 0);
        assert!(s.resets[rst::HDPTX_CMN as usize]);
        assert_eq!(call(&mut s, &mut ram, PROTO_CLOCK, 0x0c, &[]).0, NOT_SUPPORTED);
    }
}
