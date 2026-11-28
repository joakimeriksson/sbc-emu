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
pub const CLK_PLLP_OUT0: u32 = 102;
pub const CLK_GPC0: u32 = 41;
pub const CLK_GPC1: u32 = 236;
pub const CLK_GPUSYS: u32 = 304;
/// the external memory controller's clock (`TEGRA234_CLK_EMC`), capped by nvpmodel
pub const CLK_EMC: u32 = 31;
/// I²C2's divided clock, the fan's PWM (PWM3) and the fan tachometer (`TEGRA234_CLK_*`)
pub const CLK_I2C2: u32 = 49;
pub const CLK_PWM3: u32 = 107;
pub const CLK_TACH0: u32 = 152;
pub const RESET_I2C2: u32 = 29;
pub const RESET_PWM3: u32 = 70;
pub const RESET_TACH0: u32 = 95;
pub const RESET_UARTA: u32 = 100;
pub const RESET_GPU: u32 = 19;
pub const RESET_VIC: u32 = 113;
pub const GPU: u32 = 35;

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
const MRQ_THERMAL: u32 = 27;
const MRQ_PG: u32 = 66;
const MRQ_CPU_NDIV_LIMITS: u32 = 67;
const MRQ_QUERY_FW_TAG: u32 = 71;
const MRQ_BWMGR_INT: u32 = 83;

const THERMAL_QUERY_ABI: u32 = 0;
const THERMAL_GET_TEMP: u32 = 1;
const THERMAL_SET_TRIP: u32 = 2;
const THERMAL_GET_NUM_ZONES: u32 = 3;
const BWMGR_INT_CALC_AND_SET: u32 = 2;

/// Tegra234's BPMP thermal zones (`dt-bindings/thermal/tegra234-bpmp-thermal.h`): CPU, GPU,
/// CV0-2 (the DLA and PVA, absent on the Orin Nano), SOC0-2, TJ.
pub const THERMAL_ZONES: usize = 9;

/// The CPU clusters' NDIV limits (MRQ_CPU_NDIV_LIMITS): 38.4 MHz / 3 = 12.8 MHz per NDIV step,
/// 115.2 MHz (NDIV 9) to 1510.4 MHz (NDIV 118), the Orin Nano 4GB's range. Linux's
/// tegra194-cpufreq steps the table by 3 × ceil(50 MHz / 38.4 MHz) = 6 NDIV (76.8 MHz), as on
/// the module: 115200, 192000, ... 1497600, 1510400 kHz.
pub const CPU_REF_CLK_HZ: u32 = 38_400_000;
pub const CPU_PDIV: u16 = 1;
pub const CPU_MDIV: u16 = 3;
pub const CPU_NDIV_MIN: u16 = 9;
pub const CPU_NDIV_MAX: u16 = 118;
/// clusters 0 (cores 0-3) and 1 (cores 4-5) exist; cluster 2 is fused off on the Orin Nano
pub const CPU_CLUSTERS: u32 = 2;

/// the GA10B's clocks on the Orin Nano 4GB (nvgpu walks them with round_rate for devfreq;
/// nvpmodel caps them at 306 and 408 MHz in its 7 W modes)
const GPU_RATES: &[u64] = &[306_000_000, 408_000_000, 510_000_000, 624_750_000];

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
pub const EFAULT: i32 = -14;
const ENODEV: i32 = -19;
const EINVAL: i32 = -22;

struct Clock {
    id: u32,
    name: &'static str,
    parent: Option<u32>,
    rate: u64,
    enabled: bool,
    /// the rates the clock can run at (empty: any)
    rates: &'static [u64],
    /// the range a rate is rounded into (`None`: any)
    range: Option<(u64, u64)>,
}

impl Clock {
    const fn new(id: u32, name: &'static str, parent: Option<u32>, rate: u64) -> Self {
        Self {
            id,
            name,
            parent,
            rate,
            enabled: true,
            rates: &[],
            range: None,
        }
    }

    const fn table(mut self, rates: &'static [u64]) -> Self {
        self.rates = rates;
        self
    }

    const fn range(mut self, min: u64, max: u64) -> Self {
        self.range = Some((min, max));
        self
    }

    /// The rate the clock runs at for a request: the lowest of its rates at or above the
    /// request (else its highest), or the request within its range.
    fn round(&self, requested: i64) -> Option<u64> {
        if let Some(&max) = self.rates.last() {
            let requested = requested.max(0) as u64;
            return Some(self.rates.iter().copied().find(|&r| r >= requested).unwrap_or(max));
        }
        if let Some((min, max)) = self.range {
            return Some((requested.max(0) as u64).clamp(min, max));
        }
        (requested > 0).then_some(requested as u64)
    }
}

pub struct Bpmp {
    pub sram: Vec<u8>,
    clocks: Vec<Clock>,
    uart_reset: bool,
    gpu_reset: bool,
    vic_reset: bool,
    gpu_power: bool,
    /// other modules held in reset (I²C, PWM, tachometer: nothing here models their reset)
    resets: std::collections::BTreeSet<u32>,
    /// the thermal sensors as the firmware reads them (millidegrees C; Err: a BPMP error,
    /// e.g. a sensor in a power-gated domain), kept up to date by the SoC's power model
    pub temps: [Result<i32, i32>; THERMAL_ZONES],
    /// the DRAM frequency cap nvpmodel set through the bandwidth manager
    pub emc_cap: u64,
}

impl Default for Bpmp {
    fn default() -> Self {
        Self {
            sram: vec![0; SRAM_SIZE as usize],
            clocks: vec![
                Clock::new(CLK_ACTMON, "actmon", None, 19_200_000),
                Clock::new(CLK_FUSE, "fuse", None, 19_200_000),
                Clock::new(CLK_HOST1X, "host1x", None, 204_000_000),
                Clock::new(CLK_VIC, "vic", None, 204_000_000),
                Clock::new(CLK_PLLP_OUT0, "pllp_out0", None, 408_000_000),
                Clock::new(CLK_UARTA, "uarta", Some(CLK_PLLP_OUT0), 16 * 115_200),
                Clock::new(CLK_GPC0, "gpc0clk", None, 306_000_000).table(GPU_RATES),
                Clock::new(CLK_GPC1, "gpc1clk", None, 306_000_000).table(GPU_RATES),
                Clock::new(CLK_GPUSYS, "gpusys", None, 306_000_000).table(GPU_RATES),
                // LPDDR5 at up to 2133 MT/s
                Clock::new(CLK_EMC, "emc", None, 2_133_000_000).range(204_000_000, 2_133_000_000),
                Clock::new(CLK_I2C2, "i2c2", Some(CLK_PLLP_OUT0), 102_000_000)
                    .range(1, 408_000_000),
                Clock::new(CLK_PWM3, "pwm3", Some(CLK_PLLP_OUT0), 408_000_000)
                    .range(1, 408_000_000),
                Clock::new(CLK_TACH0, "tach0", None, 1_010_526).range(1, 408_000_000),
            ],
            uart_reset: false,
            gpu_reset: false,
            vic_reset: false,
            gpu_power: false,
            resets: Default::default(),
            temps: [Ok(45_000); THERMAL_ZONES],
            emc_cap: 2_133_000_000,
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
                | MRQ_THERMAL
                | MRQ_CPU_NDIV_LIMITS
                | MRQ_BWMGR_INT
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
            MRQ_THERMAL => return self.thermal(req, resp),
            MRQ_CPU_NDIV_LIMITS => {
                if word(req, 0) >= CPU_CLUSTERS {
                    return EINVAL;
                }
                put(resp, 0, CPU_REF_CLK_HZ);
                put(resp, 4, u32::from(CPU_PDIV) | u32::from(CPU_MDIV) << 16);
                put(resp, 8, u32::from(CPU_NDIV_MAX) | u32::from(CPU_NDIV_MIN) << 16);
            }
            MRQ_BWMGR_INT => return self.bwmgr(req, resp),
            _ => return ENOHANDLER,
        }
        0
    }

    fn reset(&mut self, cmd: u32, id: u32, resp: &mut [u8]) -> i32 {
        if cmd == RESET_GET_MAX_ID {
            put(resp, 0, RESET_VIC);
            return 0;
        }
        if id > RESET_VIC {
            return EINVAL;
        }
        let assert = match cmd {
            RESET_ASSERT => true,
            RESET_DEASSERT | RESET_MODULE => false,
            _ => return EBADCMD,
        };
        match id {
            RESET_UARTA => self.uart_reset = assert,
            RESET_GPU => self.gpu_reset = assert,
            RESET_VIC => self.vic_reset = assert,
            _ if assert => {
                self.resets.insert(id);
            }
            _ => {
                self.resets.remove(&id);
            }
        }
        0
    }

    /// The temperature sensors (MRQ_THERMAL). Trip points are not offered: this firmware does
    /// not message the CCPLEX, so Linux polls the zones (their `polling-delay`).
    fn thermal(&mut self, req: &[u8], resp: &mut [u8]) -> i32 {
        match word(req, 0) {
            THERMAL_QUERY_ABI => match word(req, 4) {
                THERMAL_GET_TEMP | THERMAL_GET_NUM_ZONES => 0,
                _ => ENODEV,
            },
            THERMAL_GET_TEMP => match self.temps.get(word(req, 4) as usize) {
                Some(Ok(temp)) => {
                    put(resp, 0, *temp as u32);
                    0
                }
                Some(Err(err)) => *err,
                None => EINVAL,
            },
            THERMAL_GET_NUM_ZONES => {
                put(resp, 0, THERMAL_ZONES as u32);
                0
            }
            THERMAL_SET_TRIP => ENODEV,
            _ => EBADCMD,
        }
    }

    /// The DRAM bandwidth manager (MRQ_BWMGR_INT): requests are granted at the EMC's rate,
    /// caps are kept.
    fn bwmgr(&mut self, req: &[u8], resp: &mut [u8]) -> i32 {
        const QUERY_ABI: u32 = 1;
        const CAP_SET: u32 = 3;
        match word(req, 0) {
            QUERY_ABI => 0,
            BWMGR_INT_CALC_AND_SET => {
                let rate = self.clock_rate(CLK_EMC).min(self.emc_cap);
                resp[..8].copy_from_slice(&rate.to_le_bytes());
                0
            }
            CAP_SET => {
                self.emc_cap = u64::from_le_bytes(req[4..12].try_into().unwrap());
                0
            }
            _ => EBADCMD,
        }
    }

    /// A clock's rate (0 for one the firmware does not have).
    pub fn clock_rate(&self, id: u32) -> u64 {
        self.clocks.iter().find(|c| c.id == id).map_or(0, |c| c.rate)
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
                    _ => match clock.round(requested) {
                        Some(rate) => rate,
                        None => return EINVAL,
                    },
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
