/// Known board identities. Recognition does not imply an implemented machine.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Board {
    #[default]
    Pi4,
    Pi3,
    OrangePi5,
    OrangePiOne,
    TinkerBoard,
    PanelBoard,
    OrangePiRv,
}

impl Board {
    pub const ALL: [Self; 7] =
        [Self::Pi4, Self::Pi3, Self::OrangePi5, Self::OrangePiOne, Self::TinkerBoard, Self::PanelBoard, Self::OrangePiRv];

    pub fn id(self) -> &'static str {
        match self {
            Self::Pi4 => "pi4b",
            Self::Pi3 => "pi3b",
            Self::OrangePi5 => "orangepi5",
            Self::OrangePiOne => "orangepi-one",
            Self::TinkerBoard => "tinkerboard",
            Self::PanelBoard => "panel-board",
            Self::OrangePiRv => "orangepi-rv",
        }
    }

    pub fn description(self) -> &'static str {
        match self {
            Self::Pi4 => "BCM2711 (4x Cortex-A72, GIC-400, UART, SD, USB keyboard, Ethernet, Wi-Fi, GPIO/I2C/SPI)",
            Self::Pi3 => "unverified: BCM2837 SoC model not yet implemented",
            Self::OrangePi5 => "experimental: Orange Pi 5 / RK3588S (one A55, GICv3, UART2, direct Linux boot)",
            Self::OrangePiOne => "experimental: Orange Pi One / Allwinner H3 (one A7, GIC-400, UART0, direct Linux boot)",
            Self::TinkerBoard => "experimental: ASUS Tinker Board S / RK3288 (four Cortex-A12s, GIC-400, UART2, direct Linux boot)",
            Self::PanelBoard => "experimental: RK3288 panel board / RK3288 (the vendor's own kernel and device tree, MIPI-DSI panel)",
            Self::OrangePiRv => "experimental: Orange Pi RV / StarFive JH7110 (four RV64GC U74s, CLINT, PLIC, UART0, OpenSBI + direct Linux boot)",
        }
    }

    pub fn require_implemented(self) -> Result<(), String> {
        if matches!(self, Self::Pi4 | Self::OrangePi5 | Self::OrangePiOne | Self::TinkerBoard | Self::PanelBoard | Self::OrangePiRv) {
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
