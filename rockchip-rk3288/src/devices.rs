//! The RK3288's storage, I²C and GPIO controllers as the machine routes them: four
//! DesignWare MMC hosts, six rk3x I²C controllers and nine GPIO banks at their
//! `rk3288.dtsi` addresses and interrupts. Boards attach what sits behind them (cards,
//! PMIC, sensors, input levels).
use crate::dwmmc::{CardKind, DwMmc};
use crate::gpio::{self, GpioBank};
use crate::i2c::Rk3xI2c;
use peripherals::act8846::{self, Act8846};
use peripherals::hym8563::{self, Hym8563};
use peripherals::ili2116::{self, Ili2116};
use peripherals::rk808::{self, Rk808};
use peripherals::bcm43455::Bcm43455;
use peripherals::sdio::SdioCard;
use peripherals::syr82x::{self, Syr82x};
use emu_core::{disk::Disk, Fault, FlatRam};

/// SD card slot (`sdmmc`, SPI 32).
pub const SDMMC_BASE: u64 = 0xff0c_0000;
/// SDIO0 (Wi-Fi on the Tinker Board, SPI 33), SDIO1 (SPI 34).
pub const SDIO0_BASE: u64 = 0xff0d_0000;
pub const SDIO1_BASE: u64 = 0xff0e_0000;
/// eMMC (`emmc`, SPI 35; 8-bit, the Tinker Board S and the panel board).
pub const EMMC_BASE: u64 = 0xff0f_0000;
/// GPIO0-8 interrupts: SPI 81-89.
const GPIO_IRQ_BASE: usize = 32 + 81;
/// The panel board's touch interrupt: GPIO7_A6.
const TOUCH_GPIO: (usize, u32) = (7, 6);
/// The panel board's Wi-Fi host-wake interrupt: GPIO4_D6.
const WIFI_GPIO: (usize, u32) = (4, 30);
/// The SD slot's card-detect pin, GPIO6_C6 (active low): the RK3288's `sdmmc-cd`, which
/// the panel board's device tree uses (`cd-gpios`); the Tinker Board's slot is `broken-cd`.
const SD_DETECT_GPIO: (usize, u32) = (6, 22);
/// The panel board's Wi-Fi MAC address (locally administered).
const WIFI_MAC: [u8; 6] = [0x02, 0x4d, 0x50, 0x43, 0x01, 0x01];
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
    /// RAM written by DMA since the machine last looked (for the JIT's code watch).
    pub dma_written: Vec<(u64, usize)>,
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
            dma_written: Vec::new(),
        }
    }
}

impl Peripherals {
    /// The Tinker Board's RK808 PMIC on I²C0; its RTC starts at `unix_seconds`.
    pub fn attach_rk808(&mut self, unix_seconds: i64) {
        self.i2c[0].2.attach(rk808::ADDRESS, Box::new(Rk808::new(unix_seconds)));
    }

    /// A card in the SD slot (or none): the controller's CDETECT and the card-detect pin.
    pub fn insert_sd(&mut self, disk: Option<Box<dyn Disk>>) {
        let present = disk.is_some();
        self.mmc[SDMMC].insert(disk);
        self.gpio[SD_DETECT_GPIO.0].set_input(SD_DETECT_GPIO.1, !present);
    }

    /// The RK3288 panel board's power chips on I²C0: the ACT8846 PMIC, the SYR827 (vdd_cpu) and
    /// SYR828 (vdd_gpu) regulators and the HYM8563 RTC, which starts at `unix_seconds`.
    pub fn attach_panel_board_power(&mut self, unix_seconds: i64) {
        let bus = &mut self.i2c[0].2;
        bus.attach(act8846::ADDRESS, Box::new(Act8846::default()));
        bus.attach(syr82x::CPU_ADDRESS, Box::new(Syr82x::default()));
        bus.attach(syr82x::GPU_ADDRESS, Box::new(Syr82x::default()));
        bus.attach(hym8563::ADDRESS, Box::new(Hym8563::new(unix_seconds)));
        // the touchscreen on I²C4, its interrupt on GPIO7_A6
        self.i2c[4].2.attach(ili2116::ADDRESS, Box::new(Ili2116::default()));
        // the BCM43455 Wi-Fi on SDIO0, host-wake on GPIO4_D6
        self.mmc[SDIO0].insert_sdio(SdioCard::new(Box::new(Bcm43455::new(WIFI_MAC))));
        self.sync_wifi_irq();
    }

    fn wifi(&mut self) -> Option<&mut Bcm43455> {
        self.mmc[SDIO0].sdio_mut()?.device_mut::<Bcm43455>()
    }

    /// The Wi-Fi chip's out-of-band interrupt onto GPIO4_D6: its level, and a fresh edge
    /// for each new cause (the line is edge-triggered and may already be asserted).
    fn sync_wifi_irq(&mut self) {
        let Some(chip) = self.wifi() else { return };
        let edge = chip.take_edge();
        let level = chip.oob_level();
        let gpio = &mut self.gpio[WIFI_GPIO.0];
        match level {
            Some(level) => {
                if edge {
                    gpio.set_input(WIFI_GPIO.1, !level);
                }
                gpio.set_input(WIFI_GPIO.1, level);
            }
            None => gpio.set_input(WIFI_GPIO.1, true),
        }
    }

    /// Exchange the Wi-Fi chip's Ethernet frames with the network behind its access
    /// points; true while it is joined (the network is then polled for replies).
    pub fn exchange_wifi(&mut self, net: &mut sbc_net::net::VirtualNet, now_us: u64) -> bool {
        let Some(chip) = self.wifi() else { return false };
        let mut replies = Vec::new();
        for frame in chip.take_transmitted() {
            replies.extend(net.handle(&frame, now_us));
        }
        let joined = chip.joined().is_some();
        if joined {
            replies.extend(net.poll(now_us));
        }
        if replies.is_empty() {
            return joined;
        }
        for frame in replies {
            chip.receive(&frame);
        }
        self.sync_wifi_irq();
        joined
    }

    /// A touch (or release) at `x`, `y` on the panel board's touchscreen, if it has one.
    pub fn touch(&mut self, down: bool, x: u16, y: u16) {
        if let Some(t) = self.i2c[4].2.device_mut::<Ili2116>(ili2116::ADDRESS) {
            t.touch(down, x, y);
        }
        self.sync_touch_irq();
    }

    /// The touch controller's interrupt line (active low) onto GPIO7_A6.
    fn sync_touch_irq(&mut self) {
        let asserted = match self.i2c[4].2.device_mut::<Ili2116>(ili2116::ADDRESS) {
            Some(t) => t.irq_asserted(),
            None => return,
        };
        self.gpio[TOUCH_GPIO.0].set_input(TOUCH_GPIO.1, !asserted);
    }

    /// The PMIC was told to switch the board off.
    pub fn power_off_requested(&mut self) -> bool {
        let bus = &mut self.i2c[0].2;
        bus.device_mut::<Rk808>(rk808::ADDRESS).is_some_and(|pmic| pmic.power_off)
            || bus.device_mut::<Act8846>(act8846::ADDRESS).is_some_and(|pmic| pmic.power_off)
    }

    /// Interrupt lines (GIC number, level).
    pub fn irq_lines(&self) -> impl Iterator<Item = (usize, bool)> + '_ {
        let mmc = self.mmc.iter().map(|m| (m.irq, m.irq_level()));
        let i2c = self.i2c.iter().map(|(_, irq, c)| (*irq, c.irq()));
        let gpio = self.gpio.iter().enumerate().map(|(n, g)| (GPIO_IRQ_BASE + n, g.irq()));
        mmc.chain(i2c).chain(gpio)
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
        if let Some(k) = self.mmc.iter().position(|m| m.contains(addr)) {
            let result = word(addr, width).map(|_| {
                let mmc = &mut self.mmc[k];
                mmc.write(addr - mmc.base, value, ram);
                self.dma_written.append(&mut mmc.dma_written);
            });
            self.sync_wifi_irq();
            return Some(result);
        }
        if let Some(bank) = gpio_bank(addr) {
            return Some(word(addr, width).map(|_| self.gpio[bank].write(addr - gpio::BANKS[bank], value)));
        }
        let (base, _, i2c) = self.i2c.iter_mut().find(|(base, ..)| addr.wrapping_sub(*base) < crate::i2c::SIZE)?;
        i2c.now_ns = now_ns;
        let result = word(addr, width).map(|_| i2c.write(addr - *base, value));
        // a transfer may have consumed the touch controller's records
        self.sync_touch_irq();
        Some(result)
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
