//! The Tegra Combined UART (TCU): the console Linux reaches as `ttyTCU0`.
//!
//! On the SoC the CPUs have no UART of their own for the console: the always-on cluster's
//! firmware (SPE) owns the physical UART and multiplexes every processor's console onto it.
//! The CCPLEX talks to it through HSP shared mailboxes, as Linux's `tegra-tcu` driver
//! (`drivers/tty/serial/tegra-tcu.c`) and NVIDIA's device tree wire it:
//!
//! - transmit: shared mailbox 1 of the AON HSP (`hsp@c150000`). Each message carries up to three
//!   bytes in bits 0–23 and their count in bits 24–25; the HSP driver sets bit 31 (FULL) and
//!   waits for the firmware to clear it.
//! - receive: shared mailbox 0 of HSP TOP0 (`hsp@3c00000`). The firmware writes bytes in the
//!   same format with FULL set; the driver's FULL interrupt hands them to the tty and clears
//!   the register to zero, which tells the firmware it may send more.
//!
//! The SPE's side is modelled here: a transmitted message is taken at once (the console's
//! output), and received bytes are queued and posted three at a time as the mailbox empties.
//! Register layout and interrupt behaviour follow `drivers/mailbox/tegra-hsp.c` (Tegra194
//! and later: per-mailbox interrupt enables, the common IE/IR registers).
use std::collections::VecDeque;

pub const AON_BASE: u64 = 0x0c15_0000;
pub const AON_SIZE: u64 = 0x0009_0000;
/// AON HSP shared interrupts 1–4 (0 goes to the SPE itself), as NVIDIA's device tree lists them.
pub const AON_SHARED_SPIS: [u32; 4] = [133, 134, 135, 136];
/// TOP0 HSP shared interrupts 0–7.
pub const TOP0_SHARED_SPIS: [u32; 8] = [120, 121, 122, 123, 124, 125, 126, 127];
/// The shared mailboxes the TCU uses.
pub const TX_MAILBOX: u32 = 1;
pub const RX_MAILBOX: u32 = 0;
/// Shared mailboxes and shared interrupts of an HSP instance (TOP0 and AON alike).
pub const SHARED_MAILBOXES: usize = 8;
pub const SHARED_INTERRUPTS: usize = 8;
/// What the AON HSP's HSP_INT_DIMENSIONING reports: 5 shared interrupts, 8 shared mailboxes.
const AON_DIMENSIONING: u32 = (5 << 16) | SHARED_MAILBOXES as u32;

const INT_IE: u64 = 0x100;
const INT_IR: u64 = 0x304;
const DIMENSIONING: u64 = 0x380;
const MAILBOX_BASE: u64 = 0x1_0000;
const MAILBOX_STRIDE: u64 = 0x8000;
const MBOX_FULL: u32 = 1 << 31;
const FULL_INT_IE: u64 = 4;
const EMPTY_INT_IE: u64 = 8;
const RX_QUEUE: usize = 4096;

#[derive(Default, Clone, Copy)]
struct Mailbox {
    value: u32,
    full_ie: bool,
    empty_ie: bool,
}

/// The shared-mailbox half of an HSP block: the mailboxes and the shared interrupts' enables.
#[derive(Default)]
pub struct SharedMailboxes {
    mailboxes: [Mailbox; SHARED_MAILBOXES],
    ie: [u32; SHARED_INTERRUPTS],
}

impl SharedMailboxes {
    /// HSP_INT_IR: bit n (EMPTY) and bit 8+n (FULL) for each mailbox whose own enable is set.
    fn status(&self) -> u32 {
        self.mailboxes.iter().enumerate().fold(0, |ir, (n, mb)| {
            let full = mb.value & MBOX_FULL != 0;
            ir | (u32::from(!full && mb.empty_ie) << n) | (u32::from(full && mb.full_ie) << (8 + n))
        })
    }

    /// The level of shared interrupt `si`.
    pub fn irq(&self, si: usize) -> bool {
        self.status() & self.ie[si] != 0
    }

    fn mailbox(offset: u64) -> Option<(usize, u64)> {
        let rel = offset.checked_sub(MAILBOX_BASE)?;
        let n = (rel / MAILBOX_STRIDE) as usize;
        (n < SHARED_MAILBOXES).then_some((n, rel % MAILBOX_STRIDE))
    }

    /// A register of the shared-mailbox half, or None for any other HSP offset.
    pub fn read(&self, offset: u64) -> Option<u32> {
        if let Some((n, reg)) = Self::mailbox(offset) {
            let mb = &self.mailboxes[n];
            return Some(match reg {
                0 => mb.value,
                FULL_INT_IE => mb.full_ie.into(),
                EMPTY_INT_IE => mb.empty_ie.into(),
                _ => 0,
            });
        }
        match offset {
            o if (INT_IE..INT_IE + 4 * SHARED_INTERRUPTS as u64).contains(&o) => {
                Some(self.ie[((o - INT_IE) / 4) as usize])
            }
            INT_IR => Some(self.status()),
            _ => None,
        }
    }

    /// Returns the mailbox whose value register was written, if any.
    pub fn write(&mut self, offset: u64, value: u32) -> Option<Option<usize>> {
        if let Some((n, reg)) = Self::mailbox(offset) {
            let mb = &mut self.mailboxes[n];
            match reg {
                0 => {
                    mb.value = value;
                    return Some(Some(n));
                }
                FULL_INT_IE => mb.full_ie = value & 1 != 0,
                EMPTY_INT_IE => mb.empty_ie = value & 1 != 0,
                _ => {}
            }
            return Some(None);
        }
        match offset {
            o if (INT_IE..INT_IE + 4 * SHARED_INTERRUPTS as u64).contains(&o) => {
                self.ie[((o - INT_IE) / 4) as usize] = value;
                Some(None)
            }
            INT_IR => Some(None),
            _ => None,
        }
    }
}

/// The TCU: the AON HSP (whole), TOP0's shared mailboxes, and the SPE firmware's console side.
#[derive(Default)]
pub struct Tcu {
    pub aon: SharedMailboxes,
    pub top0: SharedMailboxes,
    rx: VecDeque<u8>,
}

impl Tcu {
    pub fn aon_read(&self, offset: u64) -> u32 {
        match offset {
            DIMENSIONING => AON_DIMENSIONING,
            o => self.aon.read(o).unwrap_or(0),
        }
    }

    /// A write to the AON HSP; returns the console bytes it transmitted.
    pub fn aon_write(&mut self, offset: u64, value: u32) -> Vec<u8> {
        if self.aon.write(offset, value) != Some(Some(TX_MAILBOX as usize)) {
            return Vec::new();
        }
        let mb = &mut self.aon.mailboxes[TX_MAILBOX as usize];
        if mb.value & MBOX_FULL == 0 {
            return Vec::new();
        }
        let count = ((mb.value >> 24) & 3) as usize;
        let bytes = mb.value.to_le_bytes()[..count].to_vec();
        // The SPE has taken the message: the mailbox is empty again.
        mb.value = 0;
        bytes
    }

    /// A register of TOP0's shared-mailbox half (None: not one; the doorbells are elsewhere).
    pub fn top0_read(&self, offset: u64) -> Option<u32> {
        self.top0.read(offset)
    }

    pub fn top0_write(&mut self, offset: u64, value: u32) -> bool {
        match self.top0.write(offset, value) {
            None => false,
            Some(_) => {
                self.post();
                true
            }
        }
    }

    /// Room for another received byte.
    pub fn rx_room(&self) -> bool {
        self.rx.len() < RX_QUEUE
    }

    pub fn push_rx(&mut self, byte: u8) {
        self.rx.push_back(byte);
        self.post();
    }

    /// Post up to three queued bytes once the guest has emptied the receive mailbox (and is
    /// listening on it).
    fn post(&mut self) {
        let mb = &mut self.top0.mailboxes[RX_MAILBOX as usize];
        if self.rx.is_empty() || mb.value != 0 || !mb.full_ie {
            return;
        }
        let count = self.rx.len().min(3);
        let mut value = MBOX_FULL | (count as u32) << 24;
        for (i, byte) in self.rx.drain(..count).enumerate() {
            value |= u32::from(byte) << (8 * i);
        }
        mb.value = value;
    }

    /// Interrupt lines: (SPI, level) for AON shared 1–4 and TOP0 shared 0–7.
    pub fn lines(&self) -> impl Iterator<Item = (u32, bool)> + '_ {
        let aon = AON_SHARED_SPIS
            .iter()
            .enumerate()
            .map(|(i, &spi)| (spi, self.aon.irq(i + 1)));
        let top0 = TOP0_SHARED_SPIS
            .iter()
            .enumerate()
            .map(|(i, &spi)| (spi, self.top0.irq(i)));
        aon.chain(top0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SM: u64 = MAILBOX_BASE;

    #[test]
    fn transmit_takes_counted_bytes_and_empties_the_mailbox() {
        let mut tcu = Tcu::default();
        let tx = SM + u64::from(TX_MAILBOX) * MAILBOX_STRIDE;
        assert_eq!(tcu.aon_read(DIMENSIONING) & 0xf, 8);
        // the producer arms the EMPTY interrupt on shared interrupt 1
        tcu.aon_write(tx + EMPTY_INT_IE, 1);
        tcu.aon_write(INT_IE + 4, 1 << TX_MAILBOX);
        let out = tcu.aon_write(
            tx,
            MBOX_FULL | 2 << 24 | u32::from(b'k') << 8 | u32::from(b'o'),
        );
        assert_eq!(out, b"ok");
        assert_eq!(tcu.aon_read(tx) & MBOX_FULL, 0);
        assert!(tcu.aon.irq(1));
        // a write without FULL is not a message
        assert!(tcu.aon_write(tx, 3 << 24 | 0x414141).is_empty());
    }

    #[test]
    fn receive_posts_three_bytes_at_a_time_after_the_guest_clears() {
        let mut tcu = Tcu::default();
        let rx = SM + u64::from(RX_MAILBOX) * MAILBOX_STRIDE;
        for &b in b"hello" {
            tcu.push_rx(b);
        }
        // nothing is posted until the consumer enables its FULL interrupt
        assert_eq!(tcu.top0_read(rx), Some(0));
        assert!(tcu.top0_write(rx + FULL_INT_IE, 1));
        assert!(tcu.top0_write(INT_IE, 1 << (8 + RX_MAILBOX)));
        let v = tcu.top0_read(rx).unwrap();
        assert_eq!(v, MBOX_FULL | 3 << 24 | 0x6c6568);
        assert!(tcu.top0.irq(0));
        assert_eq!(tcu.top0_read(INT_IR), Some(1 << 8));
        assert!(tcu.top0_write(rx, 0));
        assert_eq!(tcu.top0_read(rx), Some(MBOX_FULL | 2 << 24 | 0x6f6c));
        assert!(tcu.top0_write(rx, 0));
        assert_eq!(tcu.top0_read(rx), Some(0));
        assert!(!tcu.top0.irq(0));
        // the doorbell bank and dimensioning are not the TCU's
        assert_eq!(tcu.top0_read(DIMENSIONING), None);
        assert_eq!(tcu.top0_read(0x50000), None);
    }
}
