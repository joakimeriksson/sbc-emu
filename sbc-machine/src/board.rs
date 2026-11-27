/// Known board identities. Recognition does not imply an implemented machine.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Board {
    #[default]
    Pi4,
    Pi3,
    OrangePi5,
    OrangePiOne,
    TinkerBoard,
    OrangePiRv,
    Esp32S31,
    JetsonNano,
    JetsonOrinNano,
}

impl Board {
    pub const ALL: [Self; 9] =
        [Self::Pi4, Self::Pi3, Self::OrangePi5, Self::OrangePiOne, Self::TinkerBoard, Self::OrangePiRv, Self::Esp32S31, Self::JetsonNano, Self::JetsonOrinNano];

    pub fn id(self) -> &'static str {
        match self {
            Self::Pi4 => "pi4b",
            Self::Pi3 => "pi3b",
            Self::OrangePi5 => "orangepi5",
            Self::OrangePiOne => "orangepi-one",
            Self::TinkerBoard => "tinkerboard",
            Self::OrangePiRv => "orangepi-rv",
            Self::Esp32S31 => "esp32s31",
            Self::JetsonNano => "jetson-nano",
            Self::JetsonOrinNano => "jetson-orin-nano-4gb",
        }
    }

    pub fn description(self) -> &'static str {
        match self {
            Self::Pi4 => "Raspberry Pi 4 Model B / BCM2711 (4x Cortex-A72): Raspberry Pi OS and Ubuntu from SD; HDMI, USB, Ethernet, Wi-Fi, audio, V3D GPU",
            Self::Pi3 => "experimental: Raspberry Pi 3 Model B+ / BCM2837 (4x Cortex-A53): Raspberry Pi OS from SD, or a kernel and initramfs; serial console (--model 3b+|3b|3a+|zero2w)",
            Self::OrangePi5 => "experimental: Orange Pi 5 / RK3588S (4x A76 + 4x A55): Armbian from SD, or a kernel and initramfs; HDMI, USB, NVMe, audio, Mali-G610 GPU",
            Self::OrangePiOne => "experimental: Orange Pi One / Allwinner H3 (4x Cortex-A7): Armbian from SD, or a kernel and initramfs; HDMI, USB, Ethernet, Mali-400 GPU",
            Self::TinkerBoard => "experimental: ASUS Tinker Board S / RK3288 (4x Cortex-A12): Armbian from SD or eMMC, or a kernel and initramfs; HDMI, Mali-T760 GPU",
            Self::OrangePiRv => "experimental: Orange Pi RV / StarFive JH7110 (4x RV64GC U74): U-Boot from SPI flash or SD to Ubuntu, or OpenSBI and a kernel; Ethernet, Wi-Fi, NVMe, PowerVR GPU",
            Self::Esp32S31 => "experimental: ESP32-S31-Function-CoreBoard-1 (2x RV32IMAC): the mask ROM and Espressif's Linux flash image",
            Self::JetsonOrinNano => "experimental: NVIDIA Jetson Orin Nano 4GB / Tegra234 (6x Cortex-A78AE): direct Linux boot over serial",
            Self::JetsonNano => "experimental: NVIDIA Jetson Nano / Tegra X1 (4x Cortex-A57): a kernel and initramfs, Debian's root on the microSD card; Ethernet, USB, HDMI, Maxwell GPU",
        }
    }

    pub fn require_implemented(self) -> Result<(), String> {
        if matches!(self, Self::Pi4 | Self::Pi3 | Self::OrangePi5 | Self::OrangePiOne | Self::TinkerBoard | Self::OrangePiRv | Self::Esp32S31 | Self::JetsonNano | Self::JetsonOrinNano) {
            Ok(())
        } else {
            Err(format!(
                "board '{}' is not implemented (see `sbc-emu boards list`)",
                self.id()
            ))
        }
    }

    /// Parse the board selector before opening images or other resources.
    /// Accept both --board ID and --board=ID; reject missing and repeated values.
    pub fn from_args(args: &[String]) -> Result<Self, String> {
        let mut board = None;
        let mut args = args.iter();
        while let Some(arg) = args.next() {
            let value = if arg == "--board" {
                Some(
                    args.next()
                        .filter(|s| !s.starts_with('-'))
                        .ok_or("--board requires a board ID")?
                        .as_str(),
                )
            } else {
                arg.strip_prefix("--board=")
            };
            if let Some(value) = value {
                if board.is_some() {
                    return Err("--board may only be specified once".into());
                }
                if value.is_empty() {
                    return Err("--board requires a board ID".into());
                }
                board = Some(value.parse()?);
            }
        }
        Ok(board.unwrap_or_default())
    }
}

impl std::str::FromStr for Board {
    type Err = String;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        Self::ALL
            .into_iter()
            .find(|b| b.id() == value)
            .ok_or_else(|| format!("unknown board '{value}' (see `sbc-emu boards list`)"))
    }
}
