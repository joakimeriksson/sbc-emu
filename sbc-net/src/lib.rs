//! The virtual network behind an emulated board's Ethernet or Wi-Fi. Ported from esp32sim
//! (`esp-soc/src/{net,nat,wifi}.rs`, commit 2d6fcafe, MIT) so both emulators share one design:
//!
//! - `net::VirtualNet`: a 10.0.2.0/24 subnet with gateway, DHCP, ARP, ICMP echo, DNS and SNTP,
//!   exchanging Ethernet II frames;
//! - `nat::Nat`: user-mode NAT relaying guest TCP/UDP through host sockets (native only);
//! - `wifi`: a virtual 802.11 access point (beacons, probe/auth/assoc, WPA2 handshake).
//! - `client::Client`: the opposite role, a DHCP client host on a link the board serves (the PC
//!   on a Jetson's USB device-mode port), with TCP forwards onto the board through the NAT relay.
pub mod net;
#[cfg(not(target_arch = "wasm32"))]
pub mod nat;

/// The browser has no host sockets: the network stays isolated (no NAT is ever attached).
#[cfg(target_arch = "wasm32")]
pub mod nat {
    pub struct Nat { pub resolver: [u8; 4], pub log: bool }
    impl Nat {
        pub fn new(log: bool) -> Self { Nat { resolver: [0; 4], log } }
        #[allow(clippy::too_many_arguments)]
        pub fn udp_out(&mut self, _gmac: &[u8; 6], _gip: &[u8; 4], _sport: u16, _dip: &[u8; 4], _reply_src: &[u8; 4], _dport: u16, _payload: &[u8], _now_us: u64) {}
        pub fn tcp_in(&mut self, _gmac: &[u8; 6], _gip: &[u8; 4], _dip: &[u8; 4], _seg: &[u8], _now_us: u64) -> Vec<Vec<u8>> { Vec::new() }
        pub fn poll(&mut self, _now_us: u64, _guest: Option<([u8; 6], [u8; 4])>, _gw_ip: [u8; 4]) -> Vec<Vec<u8>> { Vec::new() }
        pub fn has_forwards(&self) -> bool { false }
    }
}
pub mod crypto;
pub mod client;
pub mod wifi;

/// Host clock for SNTP answers.
pub mod host {
    /// Milliseconds since the Unix epoch (0 where no host clock exists).
    pub fn unix_time_ms() -> u64 {
        #[cfg(not(target_arch = "wasm32"))]
        { std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_millis() as u64).unwrap_or(0) }
        #[cfg(target_arch = "wasm32")]
        { WASM_TIME_MS.load(std::sync::atomic::Ordering::Relaxed) }
    }
    /// The page sets the wall clock (ms since the epoch) for SNTP answers in the browser.
    #[cfg(target_arch = "wasm32")]
    pub static WASM_TIME_MS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
}
