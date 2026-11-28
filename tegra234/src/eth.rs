//! The developer kit carrier's (P3768) onboard Gigabit Ethernet: a Realtek RTL8111H behind PCIe
//! controller C8 (`0008:01:00.0`, as on the board), on sbc-net's virtual network (DHCP, DNS,
//! ICMP; with NAT also TCP/UDP to the host's network). The image's `r8168` (NVIDIA's build of
//! Realtek's driver) or mainline `r8169` drives it as `eth0`.

use crate::{pcie, Machine, Soc};
use peripherals::rtl8168::Rtl8111;

/// The NIC's MAC address (locally administered: "SBORN" for sbc-emu's Orin Nano).
pub const MAC: [u8; 6] = [0x02, 0x53, 0x42, 0x4f, 0x52, 0x4e];

/// Controller C8 with the RTL8111H, and the network behind the cable.
pub struct Ethernet {
    pub pcie: pcie::Pcie,
    net: sbc_net::net::VirtualNet,
    next: u64,
    /// the receiver runs (the network is polled meanwhile)
    receiving: bool,
}

impl Soc {
    /// Exchange frames between the NIC and the network: what the guest sent goes out at once,
    /// the network is polled every millisecond with NAT or while frames wait (10 ms without),
    /// then the NIC's rings run and its interrupt goes out as a message.
    pub(crate) fn pump_ethernet(&mut self, now: u64) {
        let Some(eth) = self.eth.as_mut() else { return };
        let now_us = now / 1000;
        let poll = now >= eth.next;
        eth.pcie.now = now;
        let net = &mut eth.net;
        let state = eth.pcie.with_card(&mut self.ram, |card: &mut Rtl8111, port| {
            let sent = std::mem::take(&mut card.nic.tx_out);
            for frame in sent {
                let replies = net.handle(&frame, now_us);
                card.nic.deliver(replies);
            }
            if poll {
                card.nic.deliver(net.poll(now_us));
            }
            card.nic.tick(now);
            card.service(port);
            (card.nic.receiving(), !card.nic.rx_queue.is_empty())
        });
        let (receiving, waiting) = state.unwrap_or((false, false));
        if poll {
            let busy = net.nat.is_some() || waiting;
            eth.next = now + if busy { 1_000_000 } else { 10_000_000 };
        }
        eth.receiving = receiving;
        let msi = eth.pcie.msi_irq();
        self.gic.set_line((pcie::C8.msi_spi + 32) as usize, msi);
    }

    /// When the network is next polled (while the NIC receives), or the NIC's timer is due.
    pub(crate) fn ethernet_next_event(&self) -> Option<u64> {
        let eth = self.eth.as_ref()?;
        eth.receiving.then_some(eth.next).into_iter().chain(eth.pcie.next_event()).min()
    }
}

/// Controller C8, when `addr` is one of its regions (powered as the BPMP has it).
pub(crate) fn controller<'a>(eth: &'a mut Option<Ethernet>, firmware: &Option<crate::Firmware>, now: u64, addr: u64) -> Option<&'a mut pcie::Pcie> {
    let eth = eth.as_mut().filter(|e| e.pcie.contains(addr))?;
    eth.pcie.powered = firmware.as_ref().is_some_and(|f| f.bpmp.pcie_available(pcie::C8.controller));
    eth.pcie.now = now;
    Some(&mut eth.pcie)
}

impl Machine {
    /// Plug the carrier's Ethernet port into sbc-net's virtual network (DHCP, DNS, ICMP; `nat`
    /// also relays guest TCP/UDP to the host's network, native builds only): PCIe controller C8
    /// with the RTL8111H, the cable plugged into a Gigabit partner. Needs the BPMP (the
    /// controller's clock, resets, power and UPHY).
    pub fn connect_ethernet(&mut self, nat: bool, log: bool) {
        self.enable_bpmp();
        let mut card = Rtl8111::new(MAC);
        card.nic.set_link(true);
        #[allow(unused_mut)]
        let mut net = sbc_net::net::VirtualNet::new(log);
        #[cfg(not(target_arch = "wasm32"))]
        if nat {
            net.nat = Some(sbc_net::nat::Nat::new(log));
        }
        let _ = nat;
        self.soc.eth = Some(Ethernet { pcie: pcie::Pcie::new(&pcie::C8, Some(Box::new(card))), net, next: 0, receiving: false });
    }

    pub fn ethernet_enabled(&self) -> bool {
        self.soc.eth.is_some()
    }

    /// The onboard Ethernet's network, for port forwarding.
    pub(crate) fn ethernet_net(&mut self) -> Option<&mut sbc_net::net::VirtualNet> {
        self.soc.eth.as_mut().map(|e| &mut e.net)
    }
}
