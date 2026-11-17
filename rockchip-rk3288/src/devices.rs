//! The RK3288's storage, I²C and GPIO controllers as the machine routes them: four
//! DesignWare MMC hosts, six rk3x I²C controllers and nine GPIO banks at their
//! `rk3288.dtsi` addresses and interrupts. Boards attach what sits behind them (cards,
//! PMIC, sensors, input levels).
use crate::dwmmc::{CardKind, DwMmc};
use crate::gpio::{self, GpioBank};
use crate::i2c::Rk3xI2c;
use crate::rk808::{self, Rk808};
use emu_core::{Fault, FlatRam};

/// SD card slot (`sdmmc`, SPI 32).
pub const SDMMC_BASE: u64 = 0xff0c_0000;
/// SDIO0 (Wi-Fi on the Tinker Board, SPI 33), SDIO1 (SPI 34).
pub const SDIO0_BASE: u64 = 0xff0d_0000;
pub const SDIO1_BASE: u64 = 0xff0e_0000;
/// eMMC (`emmc`, SPI 35; 8-bit, the Tinker Board S and the panel board).
pub const EMMC_BASE: u64 = 0xff0f_0000;
/// I²C0 (PMU domain: the PMIC), SPI 60.
pub const I2C0_BASE: u64 = 0xff65_0000;

/// Index of each MMC controller in `Peripherals::mmc`.
pub const SDMMC: usize = 0;
pub const SDIO0: usize = 1;
pub const SDIO1: usize = 2;
pub const EMMC: usize = 3;

/// (base, GIC interrupt) of I²C0-5.
const I2C: [(u64, usize); 6] = [
    (I2C0_BASE, 32 + 60),
    (0xff14_0000, 32 + 62),
    (0xff66_0000, 32 + 61),
    (0xff15_0000, 32 + 63),
    (0xff16_0000, 32 + 64),
    (0xff17_0000, 32 + 65),
];

pub struct Peripherals {
    pub mmc: Vec<DwMmc>,
    /// I²C0-5 (index = bus number) with their base addresses.
    pub i2c: Vec<(u64, usize, Rk3xI2c)>,
    /// GPIO0-8 (index = bank number), at `gpio::BANKS`.
    pub gpio: Vec<GpioBank>,
}

impl Default for Peripherals {
    fn default() -> Self {
        Self {
            mmc: vec![
                DwMmc::new(SDMMC_BASE, 32 + 32, CardKind::Sd),
                DwMmc::new(SDIO0_BASE, 32 + 33, CardKind::Sd),
                DwMmc::new(SDIO1_BASE, 32 + 34, CardKind::Sd),
                DwMmc::new(EMMC_BASE, 32 + 35, CardKind::Emmc),
            ],
            i2c: I2C.iter().map(|&(base, irq)| (base, irq, Rk3xI2c::default())).collect(),
            gpio: (0..gpio::BANKS.len()).map(|_| GpioBank::default()).collect(),
        }
    }
}

impl Peripherals {
    /// The Tinker Board's RK808 PMIC on I²C0; its RTC starts at `unix_seconds`.
    pub fn attach_rk808(&mut self, unix_seconds: i64) {
        self.i2c[0].2.attach(rk808::ADDRESS, Box::new(Rk808::new(unix_seconds)));
    }

    /// The PMIC was told to switch the board off.
    pub fn power_off_requested(&mut self) -> bool {
        self.i2c[0].2.device_mut::<Rk808>(rk808::ADDRESS).is_some_and(|pmic| pmic.power_off)
    }

    /// Interrupt lines (GIC number, level).
    pub fn irq_lines(&self) -> impl Iterator<Item = (usize, bool)> + '_ {
        let mmc = self.mmc.iter().map(|m| (m.irq, m.irq_level()));
        let i2c = self.i2c.iter().map(|(_, irq, c)| (*irq, c.irq()));
        mmc.chain(i2c)
    }

    /// A register read, if `addr` belongs to one of these controllers (word accesses only).
    pub fn read(&mut self, addr: u64, width: u64, now_ns: u64) -> Option<Result<u32, Fault>> {
        if let Some(mmc) = self.mmc.iter_mut().find(|m| m.contains(addr)) {
            return Some(word(addr, width).map(|_| mmc.read(addr - mmc.base)));
        }
        if let Some(bank) = gpio_bank(addr) {
            return Some(word(addr, width).map(|_| self.gpio[bank].read(addr - gpio::BANKS[bank])));
        }
        let (base, _, i2c) = self.i2c.iter_mut().find(|(base, ..)| addr.wrapping_sub(*base) < crate::i2c::SIZE)?;
        i2c.now_ns = now_ns;
        Some(word(addr, width).map(|_| i2c.read(addr - *base)))
    }

    /// A register write, if `addr` belongs to one of these controllers. DMA goes to `ram`.
    pub fn write(&mut self, addr: u64, width: u64, value: u32, ram: &mut FlatRam, now_ns: u64) -> Option<Result<(), Fault>> {
        if let Some(mmc) = self.mmc.iter_mut().find(|m| m.contains(addr)) {
            return Some(word(addr, width).map(|_| {
                mmc.write(addr - mmc.base, value, ram);
                mmc.dma_written.clear();
            }));
        }
        if let Some(bank) = gpio_bank(addr) {
            return Some(word(addr, width).map(|_| self.gpio[bank].write(addr - gpio::BANKS[bank], value)));
        }
        let (base, _, i2c) = self.i2c.iter_mut().find(|(base, ..)| addr.wrapping_sub(*base) < crate::i2c::SIZE)?;
        i2c.now_ns = now_ns;
        Some(word(addr, width).map(|_| i2c.write(addr - *base, value)))
    }
}

fn gpio_bank(addr: u64) -> Option<usize> {
    gpio::BANKS.iter().position(|&base| addr.wrapping_sub(base) < gpio::SIZE)
}

fn word(addr: u64, width: u64) -> Result<(), Fault> {
    if width == 4 && addr & 3 == 0 {
        Ok(())
    } else {
        Err(Fault::Prohibited)
    }
}
