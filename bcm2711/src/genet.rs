//! GENET v5: the BCM2711's Gigabit Ethernet MAC (Linux `bcmgenet`, "brcm,bcm2711-genet-v5"),
//! with the UniMAC MDIO controller and a BCM54213PE PHY at MDIO address 1, as on the Pi 4.
//!
//! Register layout and DMA contract follow the driver (drivers/net/ethernet/broadcom/genet,
//! rpi-6.18.y): v5 reuses the v4 parameters — RDMA at 0x2000 and TDMA at 0x4000, each starting
//! with 256 on-chip descriptors of three words (length/status, address low, address high),
//! followed by 17 ring register sets of 0x40 bytes and the DMA control registers.
//!
//! - Transmit: writing a ring's PROD_INDEX sends every descriptor up to it at once (SOP..EOP
//!   fragments gathered, the 64-byte transmit status block dropped when TBUF_64B_EN is set),
//!   advances CONS_INDEX and raises the ring's TX-done interrupt.
//! - Interrupts: ring r < 16 signals on INTRL2_1 (TX bit r, RX bit 16 + r), ring 16 on INTRL2_0.
//! - Receive: a frame from the network is written to the next descriptor's buffer behind a
//!   64-byte receive status block and the 2-byte IP alignment pad (RBUF_64B_EN | RBUF_ALIGN_2B,
//!   which the driver always enables); PROD_INDEX advances and RX-done is raised. A full ring
//!   (PROD - CONS == ring size) holds frames back.
//! - The PHY reports a 1000BASE-T full-duplex link; the MAC does not model timing on the wire.
use std::collections::VecDeque;

const SIZE: usize = 0x10000;
const INTRL2_0: usize = 0x200;
const INTRL2_1: usize = 0x240;
const TBUF_CTRL: usize = 0x600;
const UMAC_CMD: usize = 0x808;
const MDIO_CMD: usize = 0xe14;
const RDMA: usize = 0x2000;
const TDMA: usize = 0x4000;
const DESC_BYTES: usize = 12;
const RINGS_OFF: usize = 256 * DESC_BYTES; // 0xc00
const DMA_REGS_OFF: usize = RINGS_OFF + 17 * 0x40; // 0x1040
// ring register offsets (v4/v5)
const R_PROD_OR_CONS_A: usize = 0x08; // TDMA_CONS_INDEX / RDMA_PROD_INDEX (hardware-owned)
const R_PROD_OR_CONS_B: usize = 0x0c; // TDMA_PROD_INDEX / RDMA_CONS_INDEX (driver-owned)
const R_START: usize = 0x14;
const R_END: usize = 0x1c;

const DMA_SOP: u32 = 0x2000;
const DMA_EOP: u32 = 0x4000;
const DMA_RX_BRDCAST: u32 = 0x0040;
const DMA_RX_MULT: u32 = 0x0020;
const IRQ0_RXDMA_DONE: u32 = 1 << 13;
const IRQ0_TXDMA_DONE: u32 = 1 << 16;
const IRQ0_MDIO_DONE: u32 = 1 << 23;
const IRQ0_LINK_UP: u32 = 1 << 4;

pub struct Genet {
    regs: Vec<u32>,
    stat0: u32,
    mask0: u32,
    stat1: u32,
    mask1: u32,
    phy: [u16; 32],
    /// frames from the network waiting for receive descriptors
    pub rx_queue: VecDeque<Vec<u8>>,
    /// frames the guest transmitted (drained by the machine)
    pub tx_out: Vec<Vec<u8>>,
    pub tx_frames: u64,
    pub rx_frames: u64,
    pub trace: bool,
}

impl Default for Genet {
    fn default() -> Self { Self::new() }
}

impl Genet {
    pub fn new() -> Self {
        let mut g = Genet { regs: vec![0; SIZE / 4], stat0: 0, mask0: !0, stat1: 0, mask1: !0, phy: [0; 32],
                            rx_queue: VecDeque::new(), tx_out: Vec::new(), tx_frames: 0, rx_frames: 0, trace: false };
        g.regs[0] = 0x0600_0000; // SYS_REV_CTRL: GENET v5 (major 6), minor 0, EPHY rev 0
        g.reset_phy();
        g
    }

    fn reset_phy(&mut self) {
        // BCM54213PE: link up, autonegotiation complete, 1000BASE-T full duplex partner
        self.phy = [0; 32];
        self.phy[0x00] = 0x1140; // BMCR: autoneg enable, full duplex, 1000 Mb/s
        self.phy[0x01] = 0x796d; // BMSR: 10/100 capabilities, ext status, AN complete, link up
        self.phy[0x02] = 0x600d; // PHYSID1
        self.phy[0x03] = 0x84a2; // PHYSID2 (BCM54213PE)
        self.phy[0x04] = 0x01e1; // ANAR
        self.phy[0x05] = 0xc5e1; // ANLPAR: partner 10/100 full/half, ack
        self.phy[0x06] = 0x000f; // ANER
        self.phy[0x09] = 0x0300; // CTRL1000: advertise 1000 full/half
        self.phy[0x0a] = 0x7c00; // STAT1000: partner 1000 full/half, local/remote receiver ok
        self.phy[0x0f] = 0x3000; // ESTATUS: 1000BASE-T full/half
    }

    pub fn irq0(&self) -> bool { self.stat0 & !self.mask0 != 0 }
    pub fn irq1(&self) -> bool { self.stat1 & !self.mask1 != 0 }

    fn reg(&self, off: usize) -> u32 { self.regs[off / 4] }
    fn set(&mut self, off: usize, v: u32) { self.regs[off / 4] = v; }

    pub fn read(&mut self, off: u64) -> u32 {
        let o = off as usize & (SIZE - 4);
        match o {
            o if o == INTRL2_0 => self.stat0,
            o if o == INTRL2_0 + 0x0c => self.mask0,
            o if o == INTRL2_1 => self.stat1,
            o if o == INTRL2_1 + 0x0c => self.mask1,
            // DMA_STATUS: bit 0 = DMA disabled, bit 1+r = ring r disabled (the inverse of DMA_CTRL)
            o if o == RDMA + DMA_REGS_OFF + 0x08 => !self.reg(RDMA + DMA_REGS_OFF + 0x04) & 0x3_ffff,
            o if o == TDMA + DMA_REGS_OFF + 0x08 => !self.reg(TDMA + DMA_REGS_OFF + 0x04) & 0x3_ffff,
            _ => self.reg(o),
        }
    }

    pub fn write(&mut self, off: u64, v: u32, ram: &mut [u8]) {
        let o = off as usize & (SIZE - 4);
        match o {
            o if o == INTRL2_0 + 0x04 => self.stat0 |= v,
            o if o == INTRL2_0 + 0x08 => self.stat0 &= !v,
            o if o == INTRL2_0 + 0x10 => self.mask0 |= v,
            o if o == INTRL2_0 + 0x14 => self.mask0 &= !v,
            o if o == INTRL2_1 + 0x04 => self.stat1 |= v,
            o if o == INTRL2_1 + 0x08 => self.stat1 &= !v,
            o if o == INTRL2_1 + 0x10 => self.mask1 |= v,
            o if o == INTRL2_1 + 0x14 => self.mask1 &= !v,
            0 => {} // SYS_REV_CTRL is read-only
            UMAC_CMD => self.set(o, v & !(1 << 13)), // SW_RESET completes at once
            MDIO_CMD => self.mdio(v),
            _ => {
                self.set(o, v);
                // TDMA_PROD_INDEX of a ring: transmit
                if (TDMA + RINGS_OFF..TDMA + DMA_REGS_OFF).contains(&o) && (o - TDMA - RINGS_OFF) % 0x40 == R_PROD_OR_CONS_B {
                    self.transmit((o - TDMA - RINGS_OFF) / 0x40, ram);
                }
                // RDMA_CONS_INDEX moved or DMA enabled: room for queued frames
                if (RDMA..RDMA + 0x2000).contains(&o) { self.deliver(ram); }
                if o == TDMA + DMA_REGS_OFF + 0x04 { for r in 0..17 { self.transmit(r, ram); } }
            }
        }
    }

    fn mdio(&mut self, v: u32) {
        let mut r = v & !(1 << 29);
        if v & (1 << 29) != 0 {
            let op = (v >> 26) & 3;
            let addr = (v >> 21) & 0x1f;
            let reg = ((v >> 16) & 0x1f) as usize;
            if addr != 1 {
                if op == 2 { r |= 1 << 28; } // READ_FAIL: no PHY at this address
            } else if op == 2 {
                r = (r & !0xffff) | self.phy[reg] as u32;
            } else if op == 1 {
                let d = (v & 0xffff) as u16;
                match reg {
                    0x00 => {
                        if d & 0x8000 != 0 { self.reset_phy(); } // reset (self-clearing)
                        else { self.phy[0] = d & !0x0200; } // restart-AN completes at once
                    }
                    0x01..=0x03 | 0x05 | 0x06 | 0x0a | 0x0f => {} // read-only
                    _ => self.phy[reg] = d,
                }
            }
            self.stat0 |= IRQ0_MDIO_DONE;
        }
        self.set(MDIO_CMD, r);
    }

    /// (first descriptor, ring size) of ring `r` in the DMA block at `base`
    fn geometry(&self, base: usize, r: usize) -> (usize, usize) {
        let ring = base + RINGS_OFF + 0x40 * r;
        let start = self.reg(ring + R_START) as usize / 3;
        let end = (self.reg(ring + R_END) as usize + 1) / 3;
        (start.min(255), end.saturating_sub(start).clamp(1, 256))
    }

    fn desc(&self, base: usize, i: usize) -> (u32, u64) {
        let d = base + (i % 256) * DESC_BYTES;
        (self.reg(d), self.reg(d + 4) as u64 | ((self.reg(d + 8) as u64 & 0xff) << 32))
    }

    fn transmit(&mut self, r: usize, ram: &mut [u8]) {
        if r > 16 { return; }
        let ctrl = self.reg(TDMA + DMA_REGS_OFF + 0x04);
        if ctrl & 1 == 0 || ctrl & (1 << (1 + r)) == 0 { return; }
        let ring = TDMA + RINGS_OFF + 0x40 * r;
        let prod = self.reg(ring + R_PROD_OR_CONS_B) & 0xffff;
        let mut cons = self.reg(ring + R_PROD_OR_CONS_A) & 0xffff;
        if prod == cons { return; }
        let (start, size) = self.geometry(TDMA, r);
        let tsb = self.reg(TBUF_CTRL) & 1 != 0;
        let mut frame: Vec<u8> = Vec::new();
        let mut first = true;
        while cons != prod {
            let (ls, addr) = self.desc(TDMA, start + cons as usize % size);
            let len = ((ls >> 16) & 0xfff) as usize;
            let flags = ls & 0xffff;
            if flags & DMA_SOP != 0 { frame.clear(); first = true; }
            let a = addr as usize;
            if let Some(b) = ram.get(a..a + len) {
                let skip = if first && tsb { 64.min(b.len()) } else { 0 };
                frame.extend_from_slice(&b[skip..]);
            }
            first = false;
            if flags & DMA_EOP != 0 {
                if self.trace { eprintln!("[eth] tx {} bytes on ring {r}", frame.len()); }
                self.tx_frames += 1;
                self.tx_out.push(std::mem::take(&mut frame));
            }
            cons = (cons + 1) & 0xffff;
        }
        let hi = self.reg(ring + R_PROD_OR_CONS_A) & 0xffff_0000;
        self.set(ring + R_PROD_OR_CONS_A, hi | cons);
        if r == 16 { self.stat0 |= IRQ0_TXDMA_DONE; } else { self.stat1 |= 1 << r; }
    }

    /// Move queued frames into free receive descriptors of the enabled receive ring (the driver
    /// enables ring 0 when it runs no RX priority queues; ring 16 is the legacy default ring).
    pub fn deliver(&mut self, ram: &mut [u8]) {
        let ctrl = self.reg(RDMA + DMA_REGS_OFF + 0x04);
        if ctrl & 1 == 0 || self.rx_queue.is_empty() { return; }
        let Some(r) = (0..17).find(|r| ctrl & (1 << (1 + r)) != 0) else { return };
        let ring = RDMA + RINGS_OFF + 0x40 * r;
        let (start, size) = self.geometry(RDMA, r);
        let mut delivered = false;
        loop {
            let prod_reg = self.reg(ring + R_PROD_OR_CONS_A);
            let prod = prod_reg & 0xffff;
            let cons = self.reg(ring + R_PROD_OR_CONS_B) & 0xffff;
            if (prod.wrapping_sub(cons) & 0xffff) as usize >= size { break; }
            let Some(frame) = self.rx_queue.pop_front() else { break };
            let i = start + prod as usize % size;
            let (_, addr) = self.desc(RDMA, i);
            let total = 66 + frame.len();
            let mut flags = DMA_SOP | DMA_EOP;
            if frame.first().is_some_and(|b| b & 1 != 0) {
                flags |= if frame[..6.min(frame.len())].iter().all(|&b| b == 0xff) { DMA_RX_BRDCAST } else { DMA_RX_MULT };
            }
            let ls = ((total as u32) << 16) | flags;
            let a = addr as usize;
            if let Some(buf) = ram.get_mut(a..a + total) {
                buf[..66].fill(0);
                buf[..4].copy_from_slice(&ls.to_le_bytes()); // receive status block: length_status
                buf[66..].copy_from_slice(&frame);
            }
            let d = RDMA + (i % 256) * DESC_BYTES;
            self.set(d, ls);
            self.set(ring + R_PROD_OR_CONS_A, (prod_reg & 0xffff_0000) | ((prod + 1) & 0xffff));
            self.rx_frames += 1;
            delivered = true;
            if self.trace { eprintln!("[eth] rx {} bytes", frame.len()); }
        }
        if delivered {
            if r == 16 { self.stat0 |= IRQ0_RXDMA_DONE; } else { self.stat1 |= 1 << (16 + r); }
        }
        let _ = IRQ0_LINK_UP;
    }
}

impl Genet {
    pub fn save_state(&self, w: &mut emu_core::snap::W) {
        w.tag("genet-v1");
        for v in &self.regs { w.u32(*v); }
        for v in [self.stat0, self.mask0, self.stat1, self.mask1] { w.u32(v); }
        for v in self.phy { w.u16(v); }
    }
    pub fn load_state(&mut self, r: &mut emu_core::snap::R) -> Result<(), emu_core::snap::SnapError> {
        r.tag("genet-v1")?;
        for v in self.regs.iter_mut() { *v = r.u32()?; }
        for v in [&mut self.stat0, &mut self.mask0, &mut self.stat1, &mut self.mask1] { *v = r.u32()?; }
        for v in self.phy.iter_mut() { *v = r.u16()?; }
        self.rx_queue.clear();
        self.tx_out.clear();
        Ok(())
    }
}
