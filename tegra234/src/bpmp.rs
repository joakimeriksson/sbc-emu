//! Tegra234 BPMP IVC transport and the firmware services used by early Orin bring-up.
//! The layout follows Linux's Tegra186-compatible BPMP transport and NVIDIA's BPMP ABI.

pub const SRAM: u64 = 0x4000_0000;
pub const SRAM_SIZE: u64 = 0x80000;
pub const TX_POOL: usize = 0x70000;
pub const RX_POOL: usize = 0x71000;
pub const POOL_SIZE: u64 = 0x1000;

pub const CLK_UARTA: u32 = 155;
const CLK_PLLP_OUT0: u32 = 102;
const CLK_GPC0: u32 = 41;
const CLK_GPC1: u32 = 236;
const CLK_GPUSYS: u32 = 304;
pub const RESET_UARTA: u32 = 100;
const RESET_GPU: u32 = 19;
const GPU: u32 = 35;

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
    clocks: [Clock; 5],
    uart_reset: bool,
    gpu_reset: bool,
    gpu_power: bool,
}

impl Default for Bpmp {
    fn default() -> Self {
        Self {
            sram: vec![0; SRAM_SIZE as usize],
            clocks: [
                Clock::new(CLK_PLLP_OUT0, "pllp_out0", None, 408_000_000),
                Clock::new(CLK_UARTA, "uarta", Some(CLK_PLLP_OUT0), 16 * 115_200),
                Clock::new(CLK_GPC0, "gpc0clk", None, 306_000_000),
                Clock::new(CLK_GPC1, "gpc1clk", None, 306_000_000),
                Clock::new(CLK_GPUSYS, "gpusys", None, 204_000_000),
            ],
            uart_reset: false,
            gpu_reset: false,
            gpu_power: false,
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
    pub fn service(&mut self, uarta: &mut crate::uart::UartA) -> bool {
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
                ring |= self.service_message(tx, rx, uarta);
            }
        }
        ring
    }

    fn service_message(&mut self, tx: usize, rx: usize, uarta: &mut crate::uart::UartA) -> bool {
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
        if word(&request, 0) == MRQ_RESET
            && word(&request, 12) == RESET_UARTA
            && matches!(word(&request, 8), RESET_ASSERT | RESET_MODULE)
        {
            uarta.set_reset(true);
        }
        let status = self.handle(word(&request, 0), &request[8..], &mut response[8..]);
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
            _ => return ENOHANDLER,
        }
        0
    }

    fn reset(&mut self, cmd: u32, id: u32, resp: &mut [u8]) -> i32 {
        if cmd == RESET_GET_MAX_ID {
            put(resp, 0, RESET_UARTA);
            return 0;
        }
        let reset = match id {
            RESET_UARTA => &mut self.uart_reset,
            RESET_GPU => &mut self.gpu_reset,
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
                if requested <= 0 {
                    return EINVAL;
                }
                let rate = if id == CLK_PLLP_OUT0 {
                    clock.rate
                } else {
                    requested as u64
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
        match cmd {
            PG_QUERY_ABI if arg <= PG_GET_MAX_ID => return 0,
            PG_QUERY_ABI => return EBADCMD,
            PG_GET_MAX_ID => put(resp, 0, GPU),
            _ if id != GPU => return EINVAL,
            PG_SET_STATE if arg <= 1 => self.gpu_power = arg == 1,
            PG_SET_STATE => return EINVAL,
            PG_GET_STATE => put(resp, 0, u32::from(self.gpu_power)),
            PG_GET_NAME => resp[..3].copy_from_slice(b"gpu"),
            _ => return EBADCMD,
        }
        0
    }
}

fn word(data: &[u8], offset: usize) -> u32 {
    u32::from_le_bytes(data[offset..offset + 4].try_into().unwrap())
}

fn put(data: &mut [u8], offset: usize, value: u32) {
    data[offset..offset + 4].copy_from_slice(&value.to_le_bytes());
}
