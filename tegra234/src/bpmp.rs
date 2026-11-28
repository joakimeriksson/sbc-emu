//! Tegra234 BPMP IVC transport and the firmware services used by early Orin bring-up.
//! The layout follows Linux's Tegra186-compatible BPMP transport and NVIDIA's BPMP ABI.

pub const SRAM: u64 = 0x4000_0000;
pub const SRAM_SIZE: u64 = 0x80000;
pub const TX_POOL: usize = 0x70000;
pub const RX_POOL: usize = 0x71000;
pub const POOL_SIZE: u64 = 0x1000;

pub const CLK_ACTMON: u32 = 1;
pub const CLK_FUSE: u32 = 40;
pub const CLK_HOST1X: u32 = 46;
pub const CLK_VIC: u32 = 167;
pub const CLK_UARTA: u32 = 155;
const CLK_PLLP_OUT0: u32 = 102;
pub const CLK_GPC0: u32 = 41;
pub const CLK_GPC1: u32 = 236;
pub const CLK_GPUSYS: u32 = 304;
pub const RESET_UARTA: u32 = 100;
pub const RESET_GPU: u32 = 19;
pub const RESET_VIC: u32 = 113;
pub const GPU: u32 = 35;
/// The PCIe controllers' core clocks, APB and core resets and power domains are those of
/// `pcie::C1` and `pcie::C8` (IDs as in the Orin Nano's device tree).
const PCIE_CONTROLLERS: [&crate::pcie::Instance; 2] = [&crate::pcie::C1, &crate::pcie::C8];

const CHANNEL_STRIDE: usize = 256;
const FRAME: usize = 128;
const TX_COUNT: usize = 0;
const STATE: usize = 4;
const RX_COUNT: usize = 64;
const ESTABLISHED: u32 = 0;
const SYNC: u32 = 1;
const ACK: u32 = 2;
const MSG_ACK: u32 = 1;
const MSG_RING: u32 = 2;

const MRQ_PING: u32 = 0;
const MRQ_THREADED_PING: u32 = 9;
const MRQ_RESET: u32 = 20;
const MRQ_CLK: u32 = 22;
const MRQ_QUERY_ABI: u32 = 23;
const MRQ_PG: u32 = 66;
const MRQ_QUERY_FW_TAG: u32 = 71;
const MRQ_UPHY: u32 = 69;

const RESET_ASSERT: u32 = 1;
const RESET_DEASSERT: u32 = 2;
const RESET_MODULE: u32 = 3;
const RESET_GET_MAX_ID: u32 = 4;

const CLK_GET_RATE: u32 = 1;
const CLK_SET_RATE: u32 = 2;
const CLK_ROUND_RATE: u32 = 3;
const CLK_GET_PARENT: u32 = 4;
const CLK_IS_ENABLED: u32 = 6;
const CLK_ENABLE: u32 = 7;
const CLK_DISABLE: u32 = 8;
const CLK_GET_ALL_INFO: u32 = 14;
const CLK_GET_MAX_ID: u32 = 15;
const CLK_HAS_SET_RATE: u32 = 1 << 1;

const PG_QUERY_ABI: u32 = 0;
const PG_SET_STATE: u32 = 1;
const PG_GET_STATE: u32 = 2;
const PG_GET_NAME: u32 = 3;
const PG_GET_MAX_ID: u32 = 4;

const ENOHANDLER: i32 = -3;
const EBADCMD: i32 = -6;
const ENODEV: i32 = -19;
const EINVAL: i32 = -22;

struct Clock {
    id: u32,
    name: &'static str,
    parent: Option<u32>,
    rate: u64,
    enabled: bool,
}

impl Clock {
    const fn new(id: u32, name: &'static str, parent: Option<u32>, rate: u64) -> Self {
        Self {
            id,
            name,
            parent,
            rate,
            enabled: true,
        }
    }
}

pub struct Bpmp {
    pub sram: Vec<u8>,
    clocks: [Clock; 11],
    uart_reset: bool,
    gpu_reset: bool,
    vic_reset: bool,
    gpu_power: bool,
    /// per PCIe controller (`PCIE_CONTROLLERS`)
    pcie: [PcieState; 2],
}

/// A PCIe controller as the BPMP sees it.
#[derive(Clone, Copy)]
struct PcieState {
    apb_reset: bool,
    core_reset: bool,
    power: bool,
    /// its UPHY side (`MRQ_UPHY` controller state)
    uphy: bool,
}

impl Default for PcieState {
    fn default() -> Self {
        Self { apb_reset: true, core_reset: true, power: false, uphy: false }
    }
}

impl Default for Bpmp {
    fn default() -> Self {
        Self {
            sram: vec![0; SRAM_SIZE as usize],
            clocks: [
                Clock::new(CLK_ACTMON, "actmon", None, 19_200_000),
                Clock::new(CLK_FUSE, "fuse", None, 19_200_000),
                Clock::new(CLK_HOST1X, "host1x", None, 204_000_000),
                Clock::new(CLK_VIC, "vic", None, 204_000_000),
                Clock::new(CLK_PLLP_OUT0, "pllp_out0", None, 408_000_000),
                Clock::new(CLK_UARTA, "uarta", Some(CLK_PLLP_OUT0), 16 * 115_200),
                Clock::new(CLK_GPC0, "gpc0clk", None, 306_000_000),
                Clock::new(CLK_GPC1, "gpc1clk", None, 306_000_000),
                Clock::new(CLK_GPUSYS, "gpusys", None, 204_000_000),
                Clock::new(crate::pcie::C1.clock, "pex1_c1_core", None, 500_000_000),
                Clock::new(crate::pcie::C8.clock, "pex2_c8_core", None, 500_000_000),
            ],
            uart_reset: false,
            gpu_reset: false,
            vic_reset: false,
            gpu_power: false,
            pcie: [PcieState::default(); 2],
        }
    }
}

impl Bpmp {
    pub fn uart_reset(&self) -> bool {
        self.uart_reset
    }

    pub fn uart_rate(&self) -> u64 {
        self.clocks
            .iter()
            .find(|clock| clock.id == CLK_UARTA)
            .unwrap()
            .rate
    }

    pub fn gpu_available(&self) -> bool {
        self.gpu_power && !self.gpu_reset
    }

    /// Handle a CCPLEX doorbell and return whether the firmware rang a doorbell back.
    pub fn service(&mut self, uarta: &mut crate::uart::UartA, gpu: &mut crate::gpu::Gpu) -> bool {
        let mut ring = false;
        // Three threaded transmit queues, the atomic transmit queue, and the receive queue.
        for channel in [0, 1, 2, 3, 13] {
            let tx = TX_POOL + channel * CHANNEL_STRIDE;
            let rx = RX_POOL + channel * CHANNEL_STRIDE;
            let guest_state = word(&self.sram, tx + STATE);
            let peer_state = word(&self.sram, rx + STATE);
            if guest_state == SYNC && peer_state != ACK {
                put(&mut self.sram, tx + RX_COUNT, 0);
                put(&mut self.sram, rx + TX_COUNT, 0);
                put(&mut self.sram, rx + STATE, ACK);
                ring = true;
            } else if guest_state == ESTABLISHED && peer_state == ACK {
                put(&mut self.sram, rx + STATE, ESTABLISHED);
            } else if channel != 13 && guest_state == ESTABLISHED && peer_state == ESTABLISHED {
                ring |= self.service_message(tx, rx, uarta, gpu);
            }
        }
        ring
    }

    fn service_message(
        &mut self,
        tx: usize,
        rx: usize,
        uarta: &mut crate::uart::UartA,
        gpu: &mut crate::gpu::Gpu,
    ) -> bool {
        let tx_read = word(&self.sram, tx + RX_COUNT);
        let rx_write = word(&self.sram, rx + TX_COUNT);
        if word(&self.sram, tx + TX_COUNT).wrapping_sub(tx_read) != 1 {
            return false;
        }
        let request: [u8; FRAME] = self.sram[tx + FRAME..tx + 2 * FRAME].try_into().unwrap();
        let flags = word(&request, 4);
        let ack = flags & MSG_ACK != 0;
        if ack && rx_write != word(&self.sram, rx + RX_COUNT) {
            return false;
        }
        let mut response = [0; FRAME];
        let mrq = word(&request, 0);
        let gpu_was_available = self.gpu_available();
        if mrq == MRQ_RESET {
            match (word(&request, 8), word(&request, 12)) {
                (RESET_ASSERT | RESET_MODULE, RESET_UARTA) => uarta.set_reset(true),
                (RESET_MODULE, RESET_GPU) => gpu.reset(),
                _ => {}
            }
        }
        let status = self.handle(mrq, &request[8..], &mut response[8..]);
        if gpu_was_available && !self.gpu_available() {
            gpu.reset();
        }
        uarta.set_reset(self.uart_reset());
        uarta.set_clock_rate(self.uart_rate());
        put(&mut response, 0, status as u32);
        put(&mut self.sram, tx + RX_COUNT, tx_read.wrapping_add(1));
        if ack {
            self.sram[rx + FRAME..rx + 2 * FRAME].copy_from_slice(&response);
            put(&mut self.sram, rx + TX_COUNT, rx_write.wrapping_add(1));
        }
        ack && flags & MSG_RING != 0
    }

    fn supports(mrq: u32) -> bool {
        matches!(
            mrq,
            MRQ_PING
                | MRQ_THREADED_PING
                | MRQ_RESET
                | MRQ_CLK
                | MRQ_QUERY_ABI
                | MRQ_PG
                | MRQ_QUERY_FW_TAG
                | MRQ_UPHY
        )
    }

    fn handle(&mut self, mrq: u32, req: &[u8], resp: &mut [u8]) -> i32 {
        match mrq {
            MRQ_PING | MRQ_THREADED_PING => put(resp, 0, word(req, 0) << 1),
            MRQ_QUERY_ABI => put(
                resp,
                0,
                if Self::supports(word(req, 0)) {
                    0
                } else {
                    ENODEV as u32
                },
            ),
            MRQ_QUERY_FW_TAG => resp[..16].copy_from_slice(b"sbc-emu-tegra234"),
            MRQ_RESET => return self.reset(word(req, 0), word(req, 4), resp),
            MRQ_CLK => return self.clock(req, resp),
            MRQ_PG => return self.power(word(req, 0), word(req, 4), word(req, 8), resp),
            MRQ_UPHY => return self.uphy(req),
            _ => return ENOHANDLER,
        }
        0
    }

    fn reset(&mut self, cmd: u32, id: u32, resp: &mut [u8]) -> i32 {
        if cmd == RESET_GET_MAX_ID {
            put(resp, 0, crate::pcie::C1.apb_reset);
            return 0;
        }
        let pcie = PCIE_CONTROLLERS.iter().position(|c| id == c.apb_reset || id == c.core_reset);
        let reset = match (id, pcie) {
            (RESET_UARTA, _) => &mut self.uart_reset,
            (RESET_GPU, _) => &mut self.gpu_reset,
            (RESET_VIC, _) => &mut self.vic_reset,
            (id, Some(k)) if id == PCIE_CONTROLLERS[k].apb_reset => &mut self.pcie[k].apb_reset,
            (_, Some(k)) => &mut self.pcie[k].core_reset,
            _ => return EINVAL,
        };
        match cmd {
            RESET_ASSERT => *reset = true,
            RESET_DEASSERT | RESET_MODULE => *reset = false,
            _ => return EBADCMD,
        }
        0
    }

    fn clock(&mut self, req: &[u8], resp: &mut [u8]) -> i32 {
        let cmd_id = word(req, 0);
        let cmd = cmd_id >> 24;
        if cmd == CLK_GET_MAX_ID {
            put(resp, 0, CLK_GPUSYS);
            return 0;
        }
        let id = cmd_id & 0x00ff_ffff;
        let Some(clock) = self.clocks.iter_mut().find(|clock| clock.id == id) else {
            return EINVAL;
        };
        match cmd {
            CLK_GET_RATE => resp[..8].copy_from_slice(&clock.rate.to_le_bytes()),
            CLK_SET_RATE | CLK_ROUND_RATE => {
                let requested = i64::from_le_bytes(req[8..16].try_into().unwrap());
                let rate = match id {
                    CLK_VIC => {
                        // Supply a small, discrete rate table for the VIC driver's OPP discovery.
                        let step = 102_000_000;
                        let requested = requested.clamp(step, 10 * step) as u64;
                        requested.div_ceil(step as u64) * step as u64
                    }
                    CLK_PLLP_OUT0 if requested > 0 => clock.rate,
                    _ if requested > 0 => requested as u64,
                    _ => return EINVAL,
                };
                if cmd == CLK_SET_RATE {
                    clock.rate = rate;
                }
                resp[..8].copy_from_slice(&rate.to_le_bytes());
            }
            CLK_GET_PARENT => put(resp, 0, clock.parent.unwrap_or(u32::MAX)),
            CLK_IS_ENABLED => put(resp, 0, u32::from(clock.enabled)),
            CLK_ENABLE => clock.enabled = true,
            CLK_DISABLE => clock.enabled = false,
            CLK_GET_ALL_INFO => {
                put(
                    resp,
                    0,
                    if id == CLK_PLLP_OUT0 {
                        0
                    } else {
                        CLK_HAS_SET_RATE
                    },
                );
                put(resp, 4, clock.parent.unwrap_or(u32::MAX));
                if let Some(parent) = clock.parent {
                    put(resp, 8, parent);
                    resp[72] = 1;
                }
                resp[73..73 + clock.name.len()].copy_from_slice(clock.name.as_bytes());
            }
            _ => return EBADCMD,
        }
        0
    }

    fn power(&mut self, cmd: u32, id: u32, arg: u32, resp: &mut [u8]) -> i32 {
        let pcie = PCIE_CONTROLLERS.iter().position(|c| c.power_domain == id);
        let (state, name): (&mut bool, &[u8]) = match (cmd, id) {
            (PG_QUERY_ABI, _) if arg <= PG_GET_MAX_ID => return 0,
            (PG_QUERY_ABI, _) => return EBADCMD,
            (PG_GET_MAX_ID, _) => {
                put(resp, 0, GPU);
                return 0;
            }
            (_, GPU) => (&mut self.gpu_power, b"gpu"),
            _ if pcie.is_some() => {
                let k = pcie.unwrap();
                (&mut self.pcie[k].power, PCIE_CONTROLLERS[k].power_name.as_bytes())
            }
            _ => return EINVAL,
        };
        match cmd {
            PG_SET_STATE if arg <= 1 => *state = arg == 1,
            PG_SET_STATE => return EINVAL,
            PG_GET_STATE => put(resp, 0, u32::from(*state)),
            PG_GET_NAME => resp[..name.len()].copy_from_slice(name),
            _ => return EBADCMD,
        }
        0
    }

    /// `MRQ_UPHY`: `CMD_UPHY_PCIE_CONTROLLER_STATE` (4) turns a controller's UPHY side on or
    /// off (`{ u16 lane; u16 cmd; u8 controller; u8 enable }`); the lane commands succeed.
    fn uphy(&mut self, req: &[u8]) -> i32 {
        const CMD_UPHY_PCIE_CONTROLLER_STATE: u16 = 4;
        let cmd = u16::from_le_bytes([req[2], req[3]]);
        if cmd == CMD_UPHY_PCIE_CONTROLLER_STATE {
            if let Some(k) = PCIE_CONTROLLERS.iter().position(|c| c.controller == u32::from(req[4])) {
                self.pcie[k].uphy = req[5] != 0;
            }
        }
        0
    }

    /// The PCIe controller with BPMP controller ID `controller` is powered, out of reset and
    /// its UPHY enabled.
    pub fn pcie_available(&self, controller: u32) -> bool {
        PCIE_CONTROLLERS
            .iter()
            .position(|c| c.controller == controller)
            .is_some_and(|k| self.pcie[k].power && self.pcie[k].uphy && !self.pcie[k].apb_reset)
    }
}

fn word(data: &[u8], offset: usize) -> u32 {
    u32::from_le_bytes(data[offset..offset + 4].try_into().unwrap())
}

fn put(data: &mut [u8], offset: usize, value: u32) {
    data[offset..offset + 4].copy_from_slice(&value.to_le_bytes());
}
