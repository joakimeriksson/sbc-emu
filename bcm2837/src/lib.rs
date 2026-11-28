//! Raspberry Pi 3 (BCM2837) machine: four Cortex-A53 cores, RAM below the VideoCore's share of
//! 1 GiB, the BCM2836-style per-core interrupt controller with the BCM2835 ARM-side controller
//! chained into it, and the peripherals at 0x3f00_0000. Blocks the BCM2837 shares with the
//! BCM2711 (mailbox, SD host, mini UART, PL011, watchdog) come from the `bcm2711` crate with
//! this SoC's addresses and interrupt numbers; only what the reduced device tree (`fdt.rs`)
//! describes is modelled, the rest of the peripheral window reads as zero and is logged once.
pub mod boot;
pub mod fdt;
pub mod intc;
pub mod machine;
#[cfg(not(target_arch = "wasm32"))]
pub(crate) mod mt;
pub mod periph;

pub use machine::{CoreState, Machine, Stop};

/// The members of the family (`--model`): what the firmware reports and the device tree says.
/// The board-level differences beyond RAM (USB topology, Ethernet, Wi-Fi chip) are not modelled
/// yet; the 3 Model B+ is the one tested.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Model {
    #[default]
    B3Plus,
    B3,
    A3Plus,
    Zero2W,
}

impl Model {
    pub const ALL: [Model; 4] = [Model::B3Plus, Model::B3, Model::A3Plus, Model::Zero2W];

    pub fn parse(s: &str) -> Result<Model, String> {
        Model::ALL.into_iter().find(|m| m.id() == s).ok_or_else(|| format!("--model {s}: one of 3b+, 3b, 3a+, zero2w"))
    }
    pub fn id(self) -> &'static str {
        match self { Model::B3Plus => "3b+", Model::B3 => "3b", Model::A3Plus => "3a+", Model::Zero2W => "zero2w" }
    }
    /// The model string the vendor device tree carries.
    pub fn name(self) -> &'static str {
        match self {
            Model::B3Plus => "Raspberry Pi 3 Model B Plus Rev 1.3",
            Model::B3 => "Raspberry Pi 3 Model B Rev 1.2",
            Model::A3Plus => "Raspberry Pi 3 Model A Plus Rev 1.0",
            Model::Zero2W => "Raspberry Pi Zero 2 W Rev 1.0",
        }
    }
    pub fn compatible(self) -> &'static str {
        match self {
            Model::B3Plus => "raspberrypi,3-model-b-plus",
            Model::B3 => "raspberrypi,3-model-b",
            Model::A3Plus => "raspberrypi,3-model-a-plus",
            Model::Zero2W => "raspberrypi,model-zero-2-w",
        }
    }
    /// The revision code (new style: memory size, Sony UK, BCM2837, board type, revision).
    pub fn revision(self) -> u32 {
        match self { Model::B3Plus => 0xa0_20d3, Model::B3 => 0xa0_2082, Model::A3Plus => 0x90_20e0, Model::Zero2W => 0x90_2120 }
    }
    pub fn ram_mb(self) -> usize {
        match self { Model::B3Plus | Model::B3 => 1024, Model::A3Plus | Model::Zero2W => 512 }
    }
    /// The VideoCore's share of RAM, as the firmware's default `gpu_mem` gives it.
    pub fn gpu_mb(self) -> usize { if self.ram_mb() >= 1024 { 76 } else { 64 } }
    /// The firmware's model filters in `config.txt` for this board.
    pub fn config_sections(self) -> &'static [&'static str] {
        match self {
            Model::B3Plus => &["pi3", "pi3+", "board-type=0xd"],
            Model::B3 => &["pi3", "board-type=0x8"],
            Model::A3Plus => &["pi3", "pi3+", "board-type=0xe"],
            Model::Zero2W => &["pi02", "board-type=0x12"],
        }
    }
    /// The ARM cores' clock (MHz) as the firmware reports it.
    pub fn arm_mhz(self) -> u32 { match self { Model::B3Plus | Model::A3Plus => 1400, Model::B3 => 1200, Model::Zero2W => 1000 } }
    /// HDMI's hot-plug line is on the firmware's GPIO expander (line 4) rather than GPIO 28.
    pub fn hpd_on_expander(self) -> bool { self == Model::B3 }
    /// How this board's firmware reads `config.txt` and `cmdline.txt`: serial0 is the mini
    /// UART (the PL011 is wired to Bluetooth on every member), 32-bit kernels by default.
    pub fn firmware(self) -> bcm2711::boot::Firmware<'static> {
        bcm2711::boot::Firmware { sections: self.config_sections(), arm64_default: false, serial0: "ttyS0", serial1: "ttyAMA0", kms: true }
    }
}
