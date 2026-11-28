//! The Orin Nano module's thermal, power, fan and CPU-frequency hardware, for NVIDIA's L4T
//! services (nvpower, nvpmodel, nvfancontrol) and jtop:
//!
//! - the CPU complex's frequency registers (`ccplex.rs`, `tegra194-cpufreq`);
//! - the fan: PWM3, the fan itself and its tachometer (`fan.rs`, `pwm-tegra`, `pwm-fan`,
//!   `pwm-tegra-tachometer`);
//! - I²C2 (`gen2_i2c`, `nvidia,tegra194-i2c`) with the module's INA3221 power monitor at 0x40
//!   (`ti,ina3221`): VDD_IN, VDD_CPU_GPU_CV and VDD_SOC through 5 mΩ shunts;
//! - the thermal sensors the BPMP reads (`tegra-bpmp-thermal`: CPU, GPU, CV0-2, SOC0-2, TJ).
//!
//! What they report comes from one model of the module: the cores' activity (which are not
//! asleep in WFI) and clocks set the CPU rail's power, the rails add up to the module's input
//! power, and that heats the heatsink, which the fan cools. The module runs from 5 V (on the
//! rover, the carrier board's regulator from the 3S battery feeds the module's 5 V input).
use crate::{bpmp, ccplex::Ccplex, fan::Fan};
use peripherals::ina3221::Ina3221;
use peripherals::tegra_i2c::TegraI2c;

pub const I2C2: u64 = 0x0c24_0000;
pub const I2C2_SPI: u32 = 26;
pub const INA3221_ADDRESS: u8 = 0x40;
/// the INA3221's shunt resistors on the module (`shunt-resistor-micro-ohms`)
pub const SHUNT_UOHM: u32 = 5000;

/// the air around the module inside the rover (°C)
const AMBIENT_C: f64 = 28.0;
/// the heatsink's time constant (s) and its thermal resistance to the air, still and with the
/// fan at full speed (°C/W)
const HEATSINK_TAU_S: f64 = 40.0;
const R_STILL: f64 = 8.0;
const R_FAN: f64 = 2.9;
/// the dies' time constant over the heatsink (s)
const DIE_TAU_S: f64 = 2.0;
/// module input voltage (mV) and the resistance it sags through (mΩ)
const VDD_IN_MV: f64 = 5080.0;
const VDD_IN_SAG_MOHM: f64 = 40.0;
/// the CV zones (DLA and PVA) are not on the Orin Nano: their sensors do not answer
const ABSENT: [usize; 3] = [2, 3, 4];

pub struct Power {
    pub ccplex: Ccplex,
    pub fan: Fan,
    pub i2c: TegraI2c,
    /// heatsink temperature, and each zone's die temperature (°C)
    heatsink: f64,
    die: [f64; bpmp::THERMAL_ZONES],
    /// the rails' power (W): VDD_IN, VDD_CPU_GPU_CV, VDD_SOC
    rails: [f64; 3],
    since: u64,
    /// the cores that are executing (not asleep), for the next interval
    busy: u8,
    /// the physical core ids of the cores (for their clocks)
    ids: Vec<usize>,
}

impl Power {
    pub fn new(cores: usize) -> Self {
        let mut i2c = TegraI2c::default();
        i2c.attach(INA3221_ADDRESS, Box::new(Ina3221::new([SHUNT_UOHM; 3])));
        let mut power = Self {
            ccplex: Ccplex::default(),
            fan: Fan::default(),
            i2c,
            heatsink: AMBIENT_C,
            die: [AMBIENT_C; bpmp::THERMAL_ZONES],
            rails: [0.0; 3],
            since: 0,
            busy: 1,
            ids: crate::AFFINITIES[..cores].iter().map(|&a| crate::ccplex::id(a)).collect(),
        };
        power.update_rails();
        power
    }

    /// Which cores are executing from now on (bit per core).
    pub fn set_busy(&mut self, busy: u8) {
        self.busy = busy;
    }

    /// The model at `now` (ns).
    pub fn advance(&mut self, now: u64) {
        if now < self.since + 1_000_000 {
            return;
        }
        let dt = (now - self.since) as f64 / 1e9;
        self.since = now;
        self.fan.advance(now);
        self.update_rails();
        let input = self.rails[0];
        let r = R_STILL - (R_STILL - R_FAN) * (self.fan.rpm / crate::fan::MAX_RPM).min(1.0);
        self.heatsink += (AMBIENT_C + input * r - self.heatsink) * (1.0 - (-dt / HEATSINK_TAU_S).exp());
        let cpu = self.cpu_watts();
        let busy = f64::from(self.busy.count_ones()) / self.ids.len() as f64;
        // each zone's rise over the heatsink: the CPU's with its rail, the rest a little
        let rise = [2.0 + 2.5 * cpu, 1.2, 0.0, 0.0, 0.0, 1.6 + busy, 0.9 + 0.5 * busy, 1.3 + 0.5 * busy, 0.0];
        let k = 1.0 - (-dt / DIE_TAU_S).exp();
        for zone in 0..8 {
            self.die[zone] += (self.heatsink + rise[zone] - self.die[zone]) * k;
        }
        // TJ: the hottest junction
        self.die[8] = self.die[..8]
            .iter()
            .enumerate()
            .filter(|(zone, _)| !ABSENT.contains(zone))
            .map(|(_, t)| *t)
            .fold(f64::MIN, f64::max)
            + 0.5;
    }

    /// The CPU clusters' power (W): leakage, and each executing core by the square of its
    /// clock (the voltage follows the clock).
    fn cpu_watts(&self) -> f64 {
        let max = Ccplex::max_khz() as f64;
        0.25 + self
            .ids
            .iter()
            .enumerate()
            .map(|(core, &id)| {
                let f = self.ccplex.khz(id) as f64 / max;
                if self.busy & 1 << core != 0 { 0.45 * f * f } else { 0.02 * f }
            })
            .sum::<f64>()
    }

    fn update_rails(&mut self) {
        let busy = f64::from(self.busy.count_ones()) / self.ids.len() as f64;
        // CPU, GPU (idle) and CV
        let cpu_gpu_cv = self.cpu_watts() + 0.15;
        let soc = 1.1 + 0.15 * busy;
        // DRAM, I/O and the module's regulators besides
        let input = (cpu_gpu_cv + soc) / 0.88 + 1.3;
        self.rails = [input, cpu_gpu_cv, soc];
        let in_ma = input / VDD_IN_MV * 1e6;
        let bus = VDD_IN_MV - in_ma * VDD_IN_SAG_MOHM / 1000.0;
        let Some(ina) = self
            .i2c
            .device_mut(INA3221_ADDRESS)
            .and_then(|d| d.as_any_mut())
            .and_then(|a| a.downcast_mut::<Ina3221>())
        else {
            return;
        };
        for (ch, watts) in self.rails.iter().enumerate() {
            // the CPU/GPU/CV and SOC branches are measured after a little more wiring
            let mv = if ch == 0 { bus } else { bus - 8.0 };
            ina.set_channel(ch, mv as u32, (watts / mv * 1e6) as u32);
        }
    }

    /// The BPMP's view of the sensors: millidegrees in the sensors' 1/32 °C steps; the CV
    /// zones' sensors fail as power-gated (-EFAULT).
    pub fn temps(&self) -> [Result<i32, i32>; bpmp::THERMAL_ZONES] {
        std::array::from_fn(|zone| {
            if ABSENT.contains(&zone) {
                Err(bpmp::EFAULT)
            } else {
                Ok(((self.die[zone] * 32.0).round() / 32.0 * 1000.0) as i32)
            }
        })
    }

    /// The module's input power (W).
    #[cfg(test)]
    pub fn input_watts(&self) -> f64 {
        self.rails[0]
    }

    pub fn contains(addr: u64) -> bool {
        (crate::ccplex::BASE..crate::ccplex::BASE + crate::ccplex::SIZE).contains(&addr)
            || (crate::fan::PWM_BASE..crate::fan::PWM_BASE + crate::fan::PWM_SIZE).contains(&addr)
            || (crate::fan::TACH_BASE..crate::fan::TACH_BASE + crate::fan::TACH_SIZE).contains(&addr)
            || (I2C2..I2C2 + peripherals::tegra_i2c::SIZE).contains(&addr)
    }

    pub fn read(&mut self, addr: u64, now: u64) -> u32 {
        use crate::{ccplex, fan};
        if addr >= ccplex::BASE && addr < ccplex::BASE + ccplex::SIZE {
            self.ccplex.read(addr - ccplex::BASE, now)
        } else if addr >= fan::PWM_BASE && addr < fan::PWM_BASE + fan::PWM_SIZE {
            self.fan.pwm_read(addr - fan::PWM_BASE)
        } else if addr >= fan::TACH_BASE && addr < fan::TACH_BASE + fan::TACH_SIZE {
            self.fan.tach_read(addr - fan::TACH_BASE, now)
        } else {
            self.advance(now);
            self.i2c.read(addr - I2C2)
        }
    }

    pub fn write(&mut self, addr: u64, value: u32, now: u64) {
        use crate::{ccplex, fan};
        if addr >= ccplex::BASE && addr < ccplex::BASE + ccplex::SIZE {
            self.ccplex.write(addr - ccplex::BASE, value, now);
        } else if addr >= fan::PWM_BASE && addr < fan::PWM_BASE + fan::PWM_SIZE {
            self.fan.pwm_write(addr - fan::PWM_BASE, value, now);
        } else if addr >= fan::TACH_BASE && addr < fan::TACH_BASE + fan::TACH_SIZE {
            self.fan.tach_write(addr - fan::TACH_BASE, value);
        } else {
            self.i2c.write(addr - I2C2, value, now);
        }
    }

    pub fn i2c_irq(&self) -> bool {
        self.i2c.irq()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const S: u64 = 1_000_000_000;

    fn run(p: &mut Power, from: u64, to: u64) {
        let mut t = from;
        while t < to {
            t += 100_000_000;
            p.advance(t);
        }
    }

    #[test]
    fn load_heats_the_cpu_and_the_fan_cools_it() {
        let mut p = Power::new(6);
        p.set_busy(0);
        run(&mut p, 0, 300 * S);
        let idle = p.temps()[0].unwrap();
        let idle_watts = p.input_watts();
        assert!((2.5..4.0).contains(&idle_watts), "{idle_watts}");
        p.set_busy(0x3f);
        run(&mut p, 300 * S, 600 * S);
        let busy = p.temps()[0].unwrap();
        assert!(p.input_watts() > idle_watts + 2.0);
        assert!(busy > idle + 15_000, "{idle} {busy}");
        assert!(busy < 90_000);
        // the fan at full speed
        p.write(crate::fan::PWM_BASE, 1 << 31 | 256 << 16, 600 * S);
        run(&mut p, 600 * S, 900 * S);
        let cooled = p.temps()[0].unwrap();
        assert!(cooled < busy - 10_000, "{busy} {cooled}");
        // TJ is the hottest, the CV sensors are absent
        let t = p.temps();
        assert!(t[8].unwrap() >= t[0].unwrap());
        assert_eq!(t[2], Err(bpmp::EFAULT));
    }

    #[test]
    fn the_ina3221_reports_the_rails() {
        let mut p = Power::new(6);
        p.set_busy(0x3f);
        run(&mut p, 0, 10 * S);
        let ina = p.i2c.device_mut(INA3221_ADDRESS).unwrap();
        let mut reg = |r: u8| {
            ina.start(false);
            ina.write(r);
            ina.start(true);
            let v = u16::from(ina.read()) << 8 | u16::from(ina.read());
            ina.stop();
            v
        };
        let bus_mv = u32::from(reg(2) >> 3) * 8;
        let shunt_uv = u32::from(reg(1) >> 3) * 40;
        let ma = shunt_uv * 1000 / SHUNT_UOHM;
        assert!((4950..=5080).contains(&bus_mv), "{bus_mv}");
        let mw = bus_mv * ma / 1000;
        assert!((5000..8000).contains(&mw), "{mw}");
    }
}
