//! The Pi 4B machine: up to four cores, their bus, virtual time and devices.
//!
//! Virtual time: one retired instruction = 1 ns. The generic counter runs at 54 MHz (the Pi 4's
//! crystal) derived from that time. A core in `wfi` skips to the next timer deadline.
use crate::gic::Gic;
use crate::mailbox::Mailbox;
use crate::sdhci::Sdhci;
use crate::dwc2::Dwc2;
use crate::genet::Genet;
use crate::pl011::Pl011;
use arm_a64::Cpu;
use emu_core::{Bus, Core, Fault, Trap};

pub const CNTFRQ: u64 = 54_000_000;
pub const UART0: u64 = 0xfe20_1000;
pub const GICD: u64 = 0xff84_1000;
pub const GICC: u64 = 0xff84_2000;
pub const PERIPH: u64 = 0xfc00_0000;
pub const MBOX: u64 = 0xfe00_b880;
pub const EMMC2: u64 = 0xfe34_0000;
/// the second SD host, wired to the Wi-Fi chip (\"mmcnr\" in the vendor tree)
pub const SDHOST_WIFI: u64 = 0xfe30_0000;
pub const USB: u64 = 0xfe98_0000;
pub const GENET: u64 = 0xfd58_0000;
pub const GPIO: u64 = 0xfe20_0000;
pub const SPI0: u64 = 0xfe20_4000;
pub const I2C1: u64 = 0xfe80_4000;
pub const RNG: u64 = 0xfe10_4000;
pub const AVS: u64 = 0xfd5d_2000;
pub const PM: u64 = 0xfe10_0000;
pub const SYSTIMER: u64 = 0xfe00_3000;
pub const AUX: u64 = 0xfe21_5000;
pub const DMA: u64 = 0xfe00_7000;
pub const PCM: u64 = 0xfe20_3000;
/// GPU memory carve-out at the top of RAM (firmware `gpu_mem=64`), holding the framebuffer
pub const GPU_MEM: u64 = 64 << 20;
const PPI_VTIMER: usize = 27;
const PPI_PTIMER: usize = 30;
const SPI_UART0: usize = 32 + 121;
const SPI_MBOX: usize = 32 + 33;
const SPI_EMMC2: usize = 32 + 126;
const SPI_USB: usize = 32 + 73;
const SPI_GENET0: usize = 32 + 157;
const SPI_GPIO0: usize = 32 + 113;
const SPI_I2C: usize = 32 + 117;
const SPI_SPI: usize = 32 + 118;
const SPI_GENET1: usize = 32 + 158;
const SPI_SYSTIMER: usize = 32 + 64;
const SPI_AUX: usize = 32 + 93;
const SPI_DMA0: usize = 32 + 80;
const SPI_PCIE_INTA: usize = 32 + 143;

pub struct Soc {
    pub ram: Vec<u8>,
    /// the core whose access is on the bus (banked GIC registers)
    pub cur: usize,
    pub gic: Gic,
    pub uart: Pl011,
    pub mbox: Mailbox,
    pub sd: Sdhci,
    /// SDIO host with the CYW43455 Wi-Fi chip
    pub wifi: Sdhci,
    /// the network behind the Wi-Fi access point
    pub wifi_net: Option<sbc_net::net::VirtualNet>,
    pub usb: Dwc2,
    /// PCIe root complex with the VL805 xHCI (the USB-A ports)
    pub pcie: crate::pcie::Pcie,
    /// USB devices, plugged into the DWC2 or the xHCI ports
    pub devs: crate::usbdev::Devices,
    pub genet: Genet,
    pub gpio: crate::gpio::Gpio,
    pub i2c: crate::i2c::I2c,
    pub spi: crate::spi::Spi,
    pub rng: crate::periph::Rng200,
    pub avs: crate::periph::Avs,
    pub pm: crate::periph::Pm,
    pub systimer: crate::periph::SysTimer,
    /// the mini UART (ttyS0)
    pub aux: crate::periph::AuxUart,
    /// AXI bridge (ASB) control registers
    asb: [u32; 12],
    pub dma: crate::dma::Dma,
    /// PCM/I²S with a PCM5102A DAC behind it
    pub i2s: crate::i2s::I2s,
    /// the Bluetooth controller on the mini UART (None: the mini UART is a serial port)
    pub bt: Option<crate::bluetooth::Bluetooth>,
    /// the vc4 display pipeline (HVS, pixel valve, HDMI0) for the KMS driver; None: the
    /// firmware's framebuffer (`simple-framebuffer`) is the display
    pub vc4: Option<crate::vc4::Vc4>,
    /// the V3D 4.2 3D GPU for the `v3d` DRM driver (with vc4 KMS, as dtoverlay=vc4-kms-v3d)
    pub v3d: Option<v3d::V3d>,
    /// when DREQ-paced DMA wants another look (ns; Some(0): now)
    pace_next: Option<u64>,
    /// the network behind the Ethernet port (None = cable unplugged)
    pub net: Option<sbc_net::net::VirtualNet>,
    pub unknown: Vec<u64>,
    /// a device register was touched: interrupt lines must be re-derived
    pub dirty: bool,
}

/// DMA bus addresses: the peripheral window at 0x7e.., RAM itself (the device tree maps device
/// addresses 1:1, so with more than 1 GiB they reach past 0x4000_0000), and the VideoCore RAM
/// aliases at 0x40../0x80../0xc0.. above the end of RAM.
pub(crate) fn dma_phys(a: u32, ram_len: u64) -> u64 {
    let a = a as u64;
    if (0x7e00_0000..0x7f00_0000).contains(&a) { a + 0x8000_0000 }
    else if a < ram_len { a }
    else if a >= 0x4000_0000 && a < 0xfc00_0000 { a & 0x3fff_ffff } else { a }
}

impl Soc {
    pub(crate) fn mmio_read(&mut self, a: u64) -> Result<u32, Fault> {
        self.dirty = true;
        Ok(match a {
            UART0..=0xfe20_1fff => self.uart.read(a - UART0),
            MBOX..=0xfe00_b8bf => self.mbox.read(a - MBOX),
            EMMC2..=0xfe34_00ff => self.sd.read(a - EMMC2),
            SDHOST_WIFI..=0xfe30_00ff => self.wifi.read(a - SDHOST_WIFI),
            USB..=0xfe98_ffff => self.usb.read(a - USB),
            crate::pcie::BASE..=0xfd50_ffff => self.pcie.read(a - crate::pcie::BASE),
            crate::pcie::MEM_CPU..=0x6_03ff_ffff => self.pcie.mem_read(a),
            GENET..=0xfd58_ffff => self.genet.read(a - GENET),
            GPIO..=0xfe20_00ff => self.gpio.read(a - GPIO),
            SPI0..=0xfe20_41ff => self.spi.read(a - SPI0),
            I2C1..=0xfe80_4fff => self.i2c.read(a - I2C1),
            RNG..=0xfe10_40ff => self.rng.read(a - RNG),
            AVS..=0xfd5d_2eff => self.avs.read(a - AVS),
            PM..=0xfe10_0113 => self.pm.read(a - PM),
            SYSTIMER..=0xfe00_301b => self.systimer.read(a - SYSTIMER),
            AUX..=0xfe21_50ff => { let v = self.aux.read(a - AUX); self.bt_pump(); v }
            DMA..=0xfe00_7fff => self.dma.read(a - DMA),
            PCM..=0xfe20_3023 => self.i2s.read(a - PCM),
            crate::vc4::HVS..=0xfe40_7fff if self.vc4.is_some() => self.vc4.as_ref().unwrap().hvs_read(a - crate::vc4::HVS),
            crate::vc4::PV2..=0xfe20_a0ff if self.vc4.is_some() => self.vc4.as_ref().unwrap().pv_read(a - crate::vc4::PV2),
            crate::vc4::HDMI_WIN..=0xfef0_ffff if self.vc4.is_some() => self.vc4.as_mut().unwrap().win_read(a - crate::vc4::HDMI_WIN),
            crate::vc4::HD..=0xfef2_00ff if self.vc4.is_some() => self.vc4.as_ref().unwrap().hd_read(a - crate::vc4::HD),
            crate::vc4::TXP..=0xfe00_401f if self.vc4.is_some() => self.vc4.as_ref().unwrap().txp_read(a - crate::vc4::TXP),
            v3d::BASE..=0xfec0_7fff if self.v3d.is_some() => self.v3d.as_mut().unwrap().read(a - v3d::BASE),
            // AXI bridges (ASB): the ID register; a stop request (bit 0) is acknowledged (bit 1) at once
            0xfe00_a000..=0xfe00_a02f | 0xfec1_1000..=0xfec1_102f => match a & 0xff { 0x20 => 0x6272_6467, o => { let r = self.asb[(o / 4) as usize % 12]; (r & !2) | ((r & 1) << 1) } },
            GICD..=0xff84_1fff => self.gic.dist_read(self.cur, a - GICD),
            GICC..=0xff84_3fff => self.gic.cpu_read(self.cur, a - GICC),
            PERIPH..=0xffff_ffff => { self.note(a); 0 }
            _ => return Err(Fault::Unmapped),
        })
    }
    /// `size`-byte write of `v` at `a` (unshifted value, any alignment within the word).
    fn mmio_write(&mut self, a: u64, v: u32, size: u32) -> Result<(), Fault> {
        self.dirty = true;
        if (EMMC2..=0xfe34_00ff).contains(&a) { self.sd.write(a - EMMC2, v, size, &mut self.ram); return Ok(()); }
        if (SDHOST_WIFI..=0xfe30_00ff).contains(&a) { self.wifi.write(a - SDHOST_WIFI, v, size, &mut self.ram); return Ok(()); }
        if (crate::pcie::BASE..=0xfd50_ffff).contains(&a) { self.pcie.write(a - crate::pcie::BASE, v, size); return Ok(()); }
        if (GICD..=0xff84_1fff).contains(&a) && size < 4 {
            // byte-accessible registers (priorities, targets): read-modify-write of the word
            let (o, sh) = ((a - GICD) & !3, 8 * (a & 3));
            let mask = if size == 1 { 0xffu32 } else { 0xffff };
            let v = if (0x400..0xc00).contains(&o) { (self.gic.dist_read(self.cur, o) & !(mask << sh)) | ((v & mask) << sh) } else { (v & mask) << sh };
            self.gic.dist_write(self.cur, o, v);
            return Ok(());
        }
        let a = a & !3;
        match a {
            UART0..=0xfe20_1fff => self.uart.write(a - UART0, v),
            MBOX..=0xfe00_b8bf => self.mbox.write(a - MBOX, v, &mut self.ram),
            USB..=0xfe98_ffff => self.usb.write(a - USB, v, &mut self.ram, &mut self.devs),
            crate::pcie::MEM_CPU..=0x6_03ff_ffff => self.pcie.mem_write(a, v, &mut self.ram, &mut self.devs),
            GENET..=0xfd58_ffff => self.genet.write(a - GENET, v, &mut self.ram),
            GPIO..=0xfe20_00ff => {
                let ce0_before = self.gpio.level(8);
                self.gpio.write(a - GPIO, v);
                if self.gpio.level(8) && !ce0_before { self.spi.deselect(); } // CE0 released
            }
            SPI0..=0xfe20_41ff => { let ce0 = !self.gpio.level(8); self.spi.write(a - SPI0, v, ce0); }
            I2C1..=0xfe80_4fff => self.i2c.write(a - I2C1, v),
            RNG..=0xfe10_40ff => self.rng.write(a - RNG, v),
            AVS..=0xfd5d_2eff => {}
            PM..=0xfe10_0113 => {
                // the V3D reset line (PM_GRAFX.V3DRSTN) going low resets the GPU
                let before = self.pm.read(0x10c);
                self.pm.write(a - PM, v);
                if before & (1 << 6) != 0 && self.pm.read(0x10c) & (1 << 6) == 0 { if let Some(g) = self.v3d.as_mut() { g.reset(); } }
            }
            SYSTIMER..=0xfe00_301b => self.systimer.write(a - SYSTIMER, v),
            AUX..=0xfe21_50ff => { self.aux.write(a - AUX, v); self.bt_pump(); }
            DMA..=0xfe00_7fff => if let Some(n) = self.dma.write(a - DMA, v) { self.dma_run(n) },
            PCM..=0xfe20_3023 => self.i2s.write(a - PCM, v),
            crate::vc4::HVS..=0xfe40_7fff if self.vc4.is_some() => self.vc4.as_mut().unwrap().hvs_write(a - crate::vc4::HVS, v),
            crate::vc4::PV2..=0xfe20_a0ff if self.vc4.is_some() => self.vc4.as_mut().unwrap().pv_write(a - crate::vc4::PV2, v),
            crate::vc4::HDMI_WIN..=0xfef0_ffff if self.vc4.is_some() => self.vc4.as_mut().unwrap().win_write(a - crate::vc4::HDMI_WIN, v),
            crate::vc4::HD..=0xfef2_00ff if self.vc4.is_some() => self.vc4.as_mut().unwrap().hd_write(a - crate::vc4::HD, v),
            crate::vc4::TXP..=0xfe00_401f if self.vc4.is_some() => self.vc4.as_mut().unwrap().txp_write(a - crate::vc4::TXP, v),
            v3d::BASE..=0xfec0_7fff if self.v3d.is_some() => self.v3d.as_mut().unwrap().write(a - v3d::BASE, v, &mut self.ram),
            0xfe00_a000..=0xfe00_a02f | 0xfec1_1000..=0xfec1_102f => self.asb[((a & 0xff) / 4) as usize % 12] = v & 0x00ff_ffff,
            GICD..=0xff84_1fff => self.gic.dist_write(self.cur, a - GICD, v),
            GICC..=0xff84_3fff => self.gic.cpu_write(self.cur, a - GICC, v),
            PERIPH..=0xffff_ffff => self.note(a),
            _ => return Err(Fault::Unmapped),
        }
        Ok(())
    }
    /// Exchange frames between the Ethernet MAC and the virtual network.
    pub(crate) fn pump_network(&mut self, now: u64) {
        let soc = self;
        let now_us = now / 1000;
        match soc.net.as_mut() {
            Some(net) => {
                for f in std::mem::take(&mut soc.genet.tx_out) { soc.genet.rx_queue.extend(net.handle(&f, now_us)); }
                soc.genet.rx_queue.extend(net.poll(now_us));
            }
            None => soc.genet.tx_out.clear(),
        }
        if soc.genet.rx_queue.len() > 512 { soc.genet.rx_queue.drain(..soc.genet.rx_queue.len() - 512); }
        soc.genet.deliver(&mut soc.ram);
        // Wi-Fi: the associated interface's frames go to the network behind the access point
        if let (Some(card), Some(wnet)) = (soc.wifi.sdio.as_mut(), soc.wifi_net.as_mut()) {
            let mut replies = Vec::new();
            for f in card.take_tx() { replies.extend(wnet.handle(&f, now_us)); }
            replies.extend(wnet.poll(now_us));
            for r in replies { card.deliver(&r); }
        }
    }

    /// Device interrupt lines into the GIC at virtual time `now` (ns).
    pub(crate) fn device_lines(&mut self, now: u64) {
        self.i2s.bclk = self.mbox.audio_clk;
        if self.pace_next.is_some_and(|t| now >= t) { self.dma_pace(now); }
        let u = self.uart.irq();
        self.gic.set_level(SPI_UART0, u);
        self.gpio.now_ns = now;
        let banks = self.gpio.irq();
        for (k, b) in banks.iter().enumerate() { self.gic.set_level(SPI_GPIO0 + k, *b); }
        self.gic.set_level(SPI_GPIO0 + 3, banks.iter().any(|b| *b));
        let i2c = self.i2c.irq();
        self.gic.set_level(SPI_I2C, i2c);
        let spi = self.spi.irq();
        self.gic.set_level(SPI_SPI, spi);
        let (g0, g1) = (self.genet.irq0(), self.genet.irq1());
        self.gic.set_level(SPI_GENET0, g0);
        self.gic.set_level(SPI_GENET1, g1);
        let usb = self.usb.irq();
        self.gic.set_level(SPI_USB, usb);
        let xhci = self.pcie.irq();
        self.gic.set_level(SPI_PCIE_INTA, xhci);
        // EMMC2 and the Wi-Fi SD host share SPI 126, as on the Pi 4
        let sd = self.sd.irq() || self.wifi.irq();
        self.gic.set_level(SPI_EMMC2, sd);
        let mb = self.mbox.irq();
        self.gic.set_level(SPI_MBOX, mb);
        self.pm.now = now;
        self.systimer.set_now(now);
        for k in 0..4 { let l = self.systimer.irq(k); self.gic.set_level(SPI_SYSTIMER + k, l); }
        let aux = self.aux.irq();
        self.gic.set_level(SPI_AUX, aux);
        for n in 0..11 { let l = self.dma.irq(n); self.gic.set_level(SPI_DMA0 + n, l); }
        if let Some(v) = self.vc4.as_mut() {
            v.tick(now);
            let l = v.irq_pv();
            self.gic.set_level(crate::vc4::SPI_PV2, l);
            let h = v.irq_hvs();
            self.gic.set_level(crate::vc4::SPI_HVS, h);
        }
        if let Some(g) = self.v3d.as_mut() { g.tick(now); let l = g.irq(); self.gic.set_level(32 + v3d::SPI, l); }
        self.dirty = false;
    }

    /// Load channel `n`'s next control block into its registers; false at the end of the chain.
    fn dma_load_cb(&mut self, n: usize) -> bool {
        let cb = self.dma.ch[n].conblk & !31;
        if cb == 0 { return false; }
        let base = dma_phys(cb, self.ram.len() as u64);
        let mut w = [0u32; 8];
        for (k, x) in w.iter_mut().enumerate() { *x = self.read32(base + 4 * k as u64).unwrap_or(0); }
        let c = &mut self.dma.ch[n];
        [c.ti, c.src, c.dst, c.len, c.stride, c.next] = [w[0], w[1], w[2], w[3], w[4], w[5]];
        true
    }

    /// The current block of channel `n` is done: interrupt, then on to the next one.
    fn dma_block_done(&mut self, n: usize) -> bool {
        use crate::dma::*;
        let c = &mut self.dma.ch[n];
        if c.ti & TI_INTEN != 0 { c.cs |= INT; }
        c.conblk = c.next;
        if c.conblk == 0 || !self.dma_load_cb(n) {
            let c = &mut self.dma.ch[n];
            c.cs = (c.cs & !ACTIVE) | END;
            return false;
        }
        true
    }

    /// Feed DREQ-paced DMA channels what their peripherals asked for up to `now` (ns): the
    /// PCM's transmit FIFO (DREQ 2) and HDMI0's audio FIFO (DREQ 10).
    pub(crate) fn dma_pace(&mut self, now: u64) {
        use crate::dma::*;
        let mut demands = [(crate::i2s::DREQ_TX, self.i2s.tx_demand(now)), (crate::vc4::DREQ_HDMI0, self.vc4.as_mut().map_or(0, |v| v.mai_demand(now)))];
        for n in 0..CHANNELS {
            let Some(dreq) = paced(self.dma.ch[n].ti) else { continue };
            if self.dma.ch[n].cs & ACTIVE == 0 { continue; }
            let Some(slot) = demands.iter().position(|d| d.0 == dreq) else { continue };
            let mut demand = demands[slot].1;
            let mut guard = 0;
            while demand > 0 && guard < 1 << 20 {
                guard += 1;
                let c = self.dma.ch[n];
                if c.len < 4 {
                    if !self.dma_block_done(n) { break; }
                    if paced(self.dma.ch[n].ti).is_none() { self.dma_run(n); break; }
                    continue;
                }
                let v = if c.ti & TI_SRC_IGNORE != 0 { 0 } else { self.read32(dma_phys(c.src, self.ram.len() as u64)).unwrap_or(0) };
                if dreq == crate::i2s::DREQ_TX { self.i2s.push(v); } else if let Some(vc) = self.vc4.as_mut() { vc.mai_push(v); }
                let c = &mut self.dma.ch[n];
                if c.ti & TI_SRC_INC != 0 { c.src = c.src.wrapping_add(4); }
                c.len -= 4;
                demand -= 1;
            }
            // the block that just ran dry completes now, not at the next demand
            if self.dma.ch[n].cs & ACTIVE != 0 && self.dma.ch[n].len < 4 { self.dma_block_done(n); }
            demands[slot].1 = demand;
        }
        // HDMI audio comes out where the DAC's does
        if let Some(vc) = self.vc4.as_mut() {
            if !vc.audio.is_empty() { self.i2s.rate = vc.audio_rate; self.i2s.out.extend(vc.audio.drain(..)); }
        }
        self.pace_next = if (0..CHANNELS).any(|n| self.dma.ch[n].cs & ACTIVE != 0 && paced(self.dma.ch[n].ti).is_some()) { Some(now + 500_000) } else { None };
    }

    /// DMA channel `n` was made active: walk its control blocks now, up to one that waits
    /// for a DREQ.
    fn dma_run(&mut self, n: usize) {
        use crate::dma::*;
        let ram_len = self.ram.len() as u64;
        let phys = |a: u32| dma_phys(a, ram_len);
        for _ in 0..4096 {
            let cb = self.dma.ch[n].conblk & !31;
            if cb == 0 { break; }
            if !self.dma_load_cb(n) { break; }
            let c = self.dma.ch[n];
            if paced(c.ti).is_some() { self.pace_next = Some(0); return; } // runs as its peripheral asks
            let [ti, src, dst, len, stride, next] = [c.ti, c.src, c.dst, c.len, c.stride, c.next];
            let (xlen, rows) = if ti & TI_TDMODE != 0 { (len & 0xffff, (len >> 16) & 0x3fff) } else { (len & 0x3fff_ffff, 1) };
            let (sstride, dstride) = ((stride & 0xffff) as i16 as i64, (stride >> 16) as i16 as i64);
            let (mut s, mut d) = (phys(src), phys(dst));
            for row in 0..rows {
                let mut off = 0;
                while off < xlen {
                    let n4 = (xlen - off).min(4);
                    if n4 == 4 {
                        let v = if ti & TI_SRC_IGNORE != 0 { 0 } else { self.read32(s).unwrap_or(0) };
                        if ti & TI_DEST_IGNORE == 0 { let _ = self.write32(d, v); }
                    } else {
                        for b in 0..n4 as u64 {
                            let v = if ti & TI_SRC_IGNORE != 0 { 0 } else { self.read8(s + b).unwrap_or(0) };
                            if ti & TI_DEST_IGNORE == 0 { let _ = self.write8(d + b, v); }
                        }
                    }
                    if ti & TI_SRC_INC != 0 { s += n4 as u64; }
                    if ti & TI_DEST_INC != 0 { d += n4 as u64; }
                    off += n4;
                }
                if ti & TI_TDMODE != 0 && row + 1 < rows { s = s.wrapping_add(sstride as u64); d = d.wrapping_add(dstride as u64); }
            }
            let c = &mut self.dma.ch[n];
            c.ti = ti; c.src = src; c.dst = dst; c.len = 0; c.stride = stride; c.next = next;
            if ti & TI_INTEN != 0 { c.cs |= INT; }
            c.conblk = next;
        }
        let c = &mut self.dma.ch[n];
        c.cs = (c.cs & !ACTIVE) | END;
    }

    /// The size of what the HDMI output shows now.
    pub fn display_size(&self) -> (u32, u32) {
        self.vc4.as_ref().and_then(|v| v.size()).unwrap_or_else(|| self.mbox.fb_size())
    }

    /// What the HDMI output shows, as RGBA: the HVS's composition while the KMS driver drives
    /// the display, else the firmware framebuffer.
    pub fn render(&self, out: &mut Vec<u8>) -> Option<(u32, u32)> {
        if let Some(v) = self.vc4.as_ref().filter(|v| v.active()) { return v.render(&self.ram, out); }
        self.mbox.render(&self.ram, out)
    }

    /// Bluetooth on the mini UART: what the host sent goes to the controller, and the
    /// controller's bytes fill the receive FIFO as it empties.
    pub(crate) fn bt_pump(&mut self) {
        let Some(bt) = self.bt.as_mut() else { return };
        if !self.aux.out.is_empty() { let b = std::mem::take(&mut self.aux.out); bt.receive(&b); }
        while self.aux.rx_room() { match bt.tx.pop_front() { Some(b) => self.aux.push_rx(b), None => break } }
    }

    /// USB time: DWC2 frames, xHCI polling of devices with input.
    pub(crate) fn usb_tick(&mut self, now: u64) {
        if let Some(bt) = self.bt.as_mut() { bt.tick(now); }
        self.bt_pump();
        self.usb.tick(now);
        self.pcie.xhci.tick(now, &mut self.ram, &mut self.devs);
    }

    /// The next USB deadline.
    pub(crate) fn usb_next_event(&self) -> Option<u64> {
        let x = self.pcie.xhci.next_event(&self.devs);
        let bt = self.bt.as_ref().and_then(|b| b.next_event());
        let vb = self.vc4.as_ref().and_then(|v| v.next_event());
        let gpu = self.v3d.as_ref().and_then(|g| g.next_event());
        [self.usb.next_event(), x, self.pace_next, bt, vb, gpu].into_iter().flatten().min()
    }

    fn note(&mut self, a: u64) {
        let page = a & !0xfff;
        if !self.unknown.contains(&page) {
            self.unknown.push(page);
            eprintln!("[sbc-emu] unmodelled peripheral access at {a:#x}");
        }
    }
    #[inline]
    fn ram_slice(&mut self, a: u64, n: usize) -> Option<&mut [u8]> {
        let o = a as usize;
        if a < self.ram.len() as u64 && o + n <= self.ram.len() { Some(&mut self.ram[o..o + n]) } else { None }
    }
}

macro_rules! ram_rw {
    ($r:ident, $w:ident, $t:ty, $n:expr) => {
        #[inline]
        fn $r(&mut self, a: u64) -> Result<$t, Fault> {
            if let Some(s) = self.ram_slice(a, $n) { return Ok(<$t>::from_le_bytes(s.try_into().unwrap())); }
            let v = self.mmio_read(a & !3)? as u64 >> (8 * (a & 3));
            Ok(v as $t)
        }
        #[inline]
        fn $w(&mut self, a: u64, v: $t) -> Result<(), Fault> {
            if let Some(s) = self.ram_slice(a, $n) { s.copy_from_slice(&v.to_le_bytes()); return Ok(()); }
            self.mmio_write(a, v as u32, $n)
        }
    };
}

impl Bus for Soc {
    ram_rw!(read8, write8, u8, 1);
    ram_rw!(read16, write16, u16, 2);
    ram_rw!(read32, write32, u32, 4);
    fn read64(&mut self, a: u64) -> Result<u64, Fault> {
        if let Some(s) = self.ram_slice(a, 8) { return Ok(u64::from_le_bytes(s.try_into().unwrap())); }
        let lo = self.mmio_read(a)? as u64;
        let hi = self.mmio_read(a + 4)? as u64;
        Ok(lo | (hi << 32))
    }
    fn write64(&mut self, a: u64, v: u64) -> Result<(), Fault> {
        if let Some(s) = self.ram_slice(a, 8) { s.copy_from_slice(&v.to_le_bytes()); return Ok(()); }
        self.mmio_write(a, v as u32, 4)?;
        self.mmio_write(a + 4, (v >> 32) as u32, 4)
    }
}

pub use emu_core::Stop;

/// What a core is doing between rounds.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CoreState {
    /// powered off (PSCI CPU_OFF, or a secondary never started)
    Off,
    Run,
    /// `wfi`: asleep until its interrupt line rises
    Wfi,
    /// `wfe` with no other core running: asleep until another core runs or time is skipped
    Wfe,
}

/// Virtual time each core runs per round while several cores are runnable (ns = instructions);
/// `SBC_QUANTUM` overrides it (experiments).
fn quantum() -> u64 {
    static Q: std::sync::OnceLock<u64> = std::sync::OnceLock::new();
    *Q.get_or_init(|| std::env::var("SBC_QUANTUM").ok().and_then(|v| v.parse().ok()).unwrap_or(20_000))
}

pub struct Machine {
    pub cpus: Vec<Cpu>,
    pub state: Vec<CoreState>,
    pub soc: Soc,
    /// virtual time in ns: the latest time any core or device has seen
    pub now: u64,
    pub exceptions: u64,
    pub irqs: u64,
    /// the core running now (or last): it holds the shared JIT
    cur: usize,
    /// instruction count of the current core at which interrupt lines must be re-evaluated
    next_check: u64,
    /// virtual time at which the current round ends (u64::MAX: one core runs alone)
    round_end: u64,
    /// debugger breakpoints (virtual pcs, any core); see `set_debugging`
    pub breakpoints: Vec<u64>,
    /// the breakpoint just reported (core, pc): resuming executes it once
    bp_resume: Option<(usize, u64)>,
    /// the deadline of the last idle stop (`idle_until` wakes sleeping cores there, not before)
    idle_target: u64,
    /// run the cores on their own host threads (faster with several busy cores, not repeatable);
    /// false: one thread, cores in turns, deterministic
    pub multi: bool,
    #[cfg(not(target_arch = "wasm32"))]
    pub(crate) mt: Option<crate::mt::Mt>,
}

impl Machine {
    pub fn new(ram: usize) -> Self { Self::with_cpus(ram, 1) }

    /// A Pi 4 with `ncpu` Cortex-A72 cores (the board has 4); only core 0 starts.
    pub fn with_cpus(ram: usize, ncpu: usize) -> Self {
        let ncpu = ncpu.clamp(1, 4);
        let fb_base = ram as u64 - GPU_MEM;
        let soc = Soc { ram: vec![0; ram], cur: 0, gic: Gic::with_cpus(ncpu), uart: Pl011::new(), mbox: Mailbox::new(fb_base, GPU_MEM, 1024, 768), sd: Sdhci::new(None), wifi: { let mut h = Sdhci::new(None); h.sdio = Some(Box::new(crate::wifi::Cyw43455::new())); h }, wifi_net: None, usb: { let mut d = Dwc2::new(); d.port = None; d }, pcie: { let mut p = crate::pcie::Pcie::new(); p.xhci.attach(&[crate::usbdev::DevId::Keyboard, crate::usbdev::DevId::Tablet]); p }, devs: Default::default(), genet: Genet::new(), gpio: crate::gpio::Gpio::new(), i2c: crate::i2c::I2c::new(), spi: crate::spi::Spi::new(), rng: crate::periph::Rng200::new(), avs: crate::periph::Avs { temp_mc: 45_000 }, pm: crate::periph::Pm::new(), systimer: crate::periph::SysTimer::new(), aux: crate::periph::AuxUart::new(), asb: [0; 12], dma: crate::dma::Dma::new(), i2s: crate::i2s::I2s::new(), pace_next: None, bt: None, vc4: None, v3d: None, net: None, unknown: Vec::new(), dirty: true };
        let mut m = Machine { cpus: Vec::new(), state: vec![CoreState::Off; ncpu], soc, now: 0, exceptions: 0, irqs: 0, cur: 0, next_check: 0, round_end: u64::MAX, breakpoints: Vec::new(), bp_resume: None, idle_target: 0, multi: false,
                             #[cfg(not(target_arch = "wasm32"))] mt: None };
        for k in 0..ncpu { let c = m.fresh_cpu(k); m.cpus.push(c); }
        m.state[0] = CoreState::Run;
        m
    }

    fn fresh_cpu(&mut self, k: usize) -> Cpu {
        let mut cpu = Cpu::new();
        cpu.sys.cntfrq = CNTFRQ;
        cpu.sys.mpidr = k as u64;
        cpu.firmware_calls_to_host = true;
        cpu.ram = self.soc.ram.as_mut_ptr();
        cpu.ram_len = self.soc.ram.len() as u64;
        // one compiled-pages map for all cores (IC IVAU broadcasts only where code was compiled)
        cpu.code_pages = Some(match self.cpus.first().and_then(|c| c.code_pages.clone()) {
            Some(m) => m,
            None => Cpu::code_page_map(cpu.ram_len),
        });
        cpu
    }

    /// Core 0 (status displays; the boot core).
    pub fn cpu(&self) -> &Cpu { &self.cpus[0] }

    /// Instructions retired by all cores.
    pub fn instret(&self) -> u64 { self.cpus.iter().map(|c| c.instret).sum() }

    /// Turn the block JIT on (one code cache shared by all cores); returns whether it is active.
    pub fn enable_jit(&mut self) -> bool { self.cpus[self.cur].enable_jit() }
    pub fn disable_jit(&mut self) { for c in self.cpus.iter_mut() { c.jit = None; } }
    pub fn jit(&self) -> Option<&arm_a64::jit::Jit> { self.cpus[self.cur].jit.as_deref() }
    pub fn jit_mut(&mut self) -> Option<&mut arm_a64::jit::Jit> { let c = self.cur; self.cpus[c].jit.as_deref_mut() }

    pub fn load(&mut self, addr: u64, data: &[u8]) {
        self.soc.ram[addr as usize..addr as usize + data.len()].copy_from_slice(data);
    }

    /// Linux arm64 boot protocol: enter at EL1h (the firmware would enter at EL2; EL1 keeps the
    /// first bring-up free of hypervisor setup), MMU off, interrupts masked, x0 = DTB.
    pub fn boot_linux(&mut self, entry: u64, dtb: u64) {
        let cpu = &mut self.cpus[0];
        cpu.el = 1;
        cpu.spsel = true;
        cpu.daif = 0xf << 6;
        cpu.pc = entry;
        cpu.x = [0; 31];
        cpu.x[0] = dtb;
        // firmware leaves FP/SIMD accessible
        cpu.sys.cpacr_el1 = 3 << 20;
    }

    fn ns_of_count(c: u64) -> u64 { (c / 27 * 500) + (c % 27 * 500).div_ceil(27) }

    /// Move core `k`'s clock to `t` if it is behind (a sleeping core's time follows the machine).
    fn catch_up(&mut self, k: usize, t: u64) {
        let c = &mut self.cpus[k];
        if c.now_ns() < t { c.sys.time_base_ns = t; c.sys.time_base_instret = c.instret; }
    }

    /// Make core `k` the running one: it takes the shared JIT; the core that stops running
    /// loses its exclusive reservation (another core may now write the location).
    fn switch_to(&mut self, k: usize) {
        if k != self.cur {
            let (a, b) = (self.cur, k);
            let (lo, hi) = self.cpus.split_at_mut(a.max(b));
            let (pa, pb) = if a < b { (&mut lo[a], &mut hi[0]) } else { (&mut hi[0], &mut lo[b]) };
            pa.hand_jit_to(pb);
            pa.clear_exclusive();
            self.cur = k;
        }
        self.soc.cur = k;
        self.next_check = 0;
    }

    fn update_irqs(&mut self) {
        let c = self.cur;
        let t = self.cpus[c].now_ns();
        if t > self.now { self.now = t; }
        let now = self.now;
        for k in 0..self.cpus.len() {
            if self.state[k] == CoreState::Off { continue; }
            if k != c && self.state[k] != CoreState::Run { self.catch_up(k, now); }
            let cpu = &mut self.cpus[k];
            cpu.refresh_counter();
            cpu.sys.timers_changed = false;
            let cnt = cpu.sys.cnt;
            let v = cpu.sys.vtimer.firing(cnt.wrapping_sub(cpu.sys.cntvoff));
            let p = cpu.sys.ptimer.firing(cnt);
            self.soc.gic.set_ppi(k, PPI_VTIMER, v);
            self.soc.gic.set_ppi(k, PPI_PTIMER, p);
        }
        let now = self.now;
        self.soc.pump_network(now);
        self.soc.usb_tick(now);
        self.soc.device_lines(now);
        for k in 0..self.cpus.len() {
            if self.state[k] == CoreState::Off { continue; }
            let irq = self.soc.gic.irq_out(k);
            self.cpus[k].set_irq(irq, false);
            if irq && matches!(self.state[k], CoreState::Wfi | CoreState::Wfe) {
                // woken: from now on the cores take turns
                self.state[k] = CoreState::Run;
                self.round_end = self.round_end.min(t + quantum());
            }
        }
        self.soc.dirty = false;
        // next forced re-evaluation: the earliest deadline still ahead (of this core, a sleeping
        // core or a device), at most 1 ms ahead; cores waiting for their turn look at their own
        // timers when they run, and lines already raised stay raised
        let horizon = t + 1_000_000;
        let due = self.deadline_after(t, c).unwrap_or(horizon).min(horizon);
        self.next_check = self.cpus[c].instret + (due - t);
    }

    /// The earliest deadline after `t` that core `c` running alone must notice: its own timers,
    /// those of sleeping cores, devices.
    fn deadline_after(&self, t: u64, c: usize) -> Option<u64> {
        let mut d: Option<u64> = [self.soc.usb_next_event(), self.soc.pm.next_event(), self.soc.systimer.next_event()]
            .into_iter().flatten().filter(|&x| x > t).min();
        for (k, cpu) in self.cpus.iter().enumerate() {
            if self.state[k] == CoreState::Off || (k != c && self.state[k] == CoreState::Run) { continue; }
            let s = &cpu.sys;
            for (tm, off) in [(s.vtimer, s.cntvoff), (s.ptimer, 0)] {
                if tm.ctl & 3 == 1 {
                    let ns = Self::ns_of_count(tm.cval.wrapping_add(off));
                    if ns > t { d = Some(d.map_or(ns, |x: u64| x.min(ns))); }
                }
            }
        }
        d
    }

    /// Next timer (of any core that is on) or device deadline in ns.
    fn next_deadline(&self) -> Option<u64> {
        let mut d: Option<u64> = [self.soc.usb_next_event(), self.soc.pm.next_event(), self.soc.systimer.next_event()].into_iter().flatten().min();
        for (k, cpu) in self.cpus.iter().enumerate() {
            if self.state[k] == CoreState::Off { continue; }
            let s = &cpu.sys;
            for (t, off) in [(s.vtimer, s.cntvoff), (s.ptimer, 0)] {
                if t.ctl & 3 == 1 {
                    let ns = Self::ns_of_count(t.cval.wrapping_add(off));
                    d = Some(d.map_or(ns, |x: u64| x.min(ns)));
                }
            }
        }
        d
    }

    /// Run until the cores have retired `budget` instructions between them.
    ///
    /// The cores take turns in rounds of 20 µs of virtual time (`SBC_QUANTUM` ns) each, all starting the
    /// round at the same time, as if they ran in parallel. A core that is alone (the others off
    /// or asleep) runs without rounds until something wakes another one.
    pub fn run(&mut self, budget: u64) -> Stop {
        #[cfg(not(target_arch = "wasm32"))]
        if self.multi && self.breakpoints.is_empty() && !self.cpus.iter().any(|c| c.jit_off || !c.watch.is_empty()) {
            return self.run_threaded(std::time::Duration::from_millis(10));
        }
        if self.cpus[0].mt {
            // back on one thread (a debugger is attached): turn-taking semantics again
            for c in self.cpus.iter_mut() { c.mt = false; }
        }
        let end = self.instret().saturating_add(budget);
        let mut total = self.instret();
        // steps, not retired instructions, bound the call: faults do not retire
        let mut steps = budget;
        let mut storm = 0u32;
        let stop = 'run: loop {
            if total >= end || steps == 0 { break Stop::Budget; }
            self.update_irqs();
            if let Some(halt) = self.soc.pm.expired() { break if halt { Stop::PowerOff } else { Stop::Reset }; }
            let any_run = self.state.contains(&CoreState::Run);
            if any_run { for s in self.state.iter_mut() { if *s == CoreState::Wfe { *s = CoreState::Run; } } }
            let runnable = self.state.iter().filter(|s| **s == CoreState::Run).count();
            if runnable == 0 {
                let until = self.next_deadline().unwrap_or(self.now + 10_000_000).max(self.now);
                self.idle_target = until;
                break Stop::Idle(until);
            }
            let round_start = self.now;
            self.round_end = if runnable == 1 { u64::MAX } else { round_start + quantum() };
            for k in 0..self.cpus.len() {
                if self.state[k] != CoreState::Run { continue; }
                if let Some(stop) = self.slice(k, round_start, &mut total, end, &mut steps, &mut storm) { break 'run stop; }
                if total >= end || steps == 0 { break; }
            }
        };
        let t = self.cpus[self.cur].now_ns();
        if t > self.now { self.now = t; }
        stop
    }

    /// Run core `k` from `round_start` until the round ends, it sleeps, or the budget is used.
    fn slice(&mut self, k: usize, round_start: u64, total: &mut u64, end: u64, steps: &mut u64, storm: &mut u32) -> Option<Stop> {
        self.switch_to(k);
        self.catch_up(k, round_start);
        loop {
            let t = self.cpus[k].now_ns();
            if t >= self.round_end || *total >= end || *steps == 0 { return None; }
            if !self.breakpoints.is_empty() {
                let pc = self.cpus[k].pc;
                if self.breakpoints.contains(&pc) {
                    if self.bp_resume == Some((k, pc)) { self.bp_resume = None; } else { self.bp_resume = Some((k, pc)); return Some(Stop::Breakpoint(k)); }
                }
            }
            *steps -= 1;
            if self.soc.dirty || self.cpus[k].sys.timers_changed || self.cpus[k].instret >= self.next_check {
                self.update_irqs();
                if let Some(halt) = self.soc.pm.expired() { return Some(if halt { Stop::PowerOff } else { Stop::Reset }); }
            }
            let cpu = &mut self.cpus[k];
            let i0 = cpu.instret;
            // compiled code may chain blocks until the slice or budget ends or a device needs a look
            cpu.chain_until = i0.saturating_add(self.round_end - t).min(self.next_check).min(i0 + (end - *total));
            let r = cpu.step(&mut self.soc);
            *total += self.cpus[k].instret - i0;
            if let Some((a, kind)) = self.cpus[k].watch_hit.take() { if r.is_ok() { return Some(Stop::Watchpoint(k, a, kind)); } }
            if let Some(op) = self.cpus[k].tlbi_bcast.take() {
                for (j, o) in self.cpus.iter_mut().enumerate() { if j != k { o.tlb_invalidate(op); } }
            }
            if let Some(op) = self.cpus[k].ic_bcast.take() {
                // cores with their own JIT (after a multi-threaded run) drop that code too
                for (j, o) in self.cpus.iter_mut().enumerate() { if j != k { o.jit_invalidate(op); } }
            }
            match r {
                Ok(()) => *storm = 0,
                Err(Trap::Exception(_, el)) => {
                    self.exceptions += 1;
                    *storm += 1;
                    if *storm >= 1_000_000 {
                        let (e, cpu) = (el as usize, &self.cpus[k]);
                        return Some(Stop::ExceptionStorm { pc: cpu.pc, esr: cpu.esr[e], elr: cpu.elr[e], far: cpu.far[e], el });
                    }
                }
                Err(Trap::Interrupt(..)) => { self.irqs += 1; self.soc.dirty = true; }
                Err(Trap::Wait) => {
                    self.update_irqs();
                    if self.cpus[k].irq { continue; }
                    let others = (0..self.cpus.len()).any(|j| j != k && self.state[j] == CoreState::Run);
                    if self.cpus[k].last_wfe && others {
                        // waiting for another core (a lock, a flag): give the rest of the round away
                        if self.round_end != u64::MAX { let r = self.round_end; self.catch_up(k, r); }
                        return None;
                    }
                    self.state[k] = if self.cpus[k].last_wfe { CoreState::Wfe } else { CoreState::Wfi };
                    return None;
                }
                Err(Trap::HostCall(_)) => {
                    if let Some(stop) = self.psci(k) { return Some(stop); }
                    if self.state[k] != CoreState::Run { return None; }
                }
                Err(Trap::Unimplemented(pc, i)) => return Some(Stop::Unimplemented(pc, i)),
                Err(Trap::Hlt(n)) => return Some(Stop::Hlt(n)),
                Err(Trap::Brk(n)) => return Some(Stop::Brk(n)),
            }
        }
    }

    /// Plug the Ethernet port into the virtual 10.0.2.0/24 network; `nat` also relays guest
    /// TCP/UDP to the host's network (native builds only).
    pub fn connect_network(&mut self, nat: bool, log: bool) {
        let mut net = sbc_net::net::VirtualNet::new(log);
        #[cfg(not(target_arch = "wasm32"))]
        if nat { net.nat = Some(sbc_net::nat::Nat::new(log)); }
        let _ = nat;
        self.soc.net = Some(net);
        // the Wi-Fi access point's network (isolated: DHCP, DNS and local services)
        self.soc.wifi_net = Some(sbc_net::net::VirtualNet::new(log));
    }

    /// Relay host TCP address `host` to the guest's TCP `guest_port` over Ethernet (QEMU's
    /// hostfwd); needs `connect_network` with NAT first.
    #[cfg(not(target_arch = "wasm32"))]
    pub fn forward_tcp(&mut self, host: std::net::SocketAddr, guest_port: u16) -> Result<(), String> {
        let nat = self.soc.net.as_mut().and_then(|n| n.nat.as_mut()).ok_or("port forwarding needs --net nat")?;
        nat.add_forward(host, guest_port).map_err(|e| format!("{host}: {e}"))
    }

    /// Physical inputs of the emulated board's sensors: temp (degC), pressure (hPa), humidity (%RH).
    pub fn set_sensor(&mut self, what: &str, v: f64) {
        let b = &mut self.soc.i2c.bme280;
        match what {
            "temp" | "temperature" => b.temp_c = v,
            "pressure" => b.pressure_pa = v * 100.0,
            "humidity" => b.humidity = v,
            "cpu-temp" | "soc-temp" => { self.soc.avs.temp_mc = (v * 1000.0) as i32; self.soc.mbox.temp_mc = (v * 1000.0).max(0.0) as u32; }
            _ => {}
        }
    }

    /// PSCI 1.1 as the Pi 4's firmware (Trusted Firmware-A) provides it, called by core `k`.
    fn psci(&mut self, k: usize) -> Option<Stop> {
        const NOT_SUPPORTED: u64 = -1i64 as u64;
        const INVALID_PARAMETERS: u64 = -2i64 as u64;
        const ALREADY_ON: u64 = -4i64 as u64;
        let x = self.cpus[k].x;
        let f = x[0] as u32;
        let target = |m: &Self| { let t = (x[1] & 0xff_00ff_ffff) as usize; (t < m.cpus.len()).then_some(t) };
        let r = match f {
            0x8400_0000 => 0x0001_0001,                 // PSCI_VERSION 1.1
            0x8400_0002 => {                             // CPU_OFF
                self.state[k] = CoreState::Off;
                self.cpus[k].set_irq(false, false);
                return None;
            }
            0x8400_0003 | 0xc400_0003 => match target(self) { // CPU_ON(target, entry, context)
                None => INVALID_PARAMETERS,
                Some(t) if self.state[t] != CoreState::Off => ALREADY_ON,
                Some(t) => { self.cpu_on(k, t, x[2], x[3]); 0 }
            },
            0x8400_0004 | 0xc400_0004 => match target(self) { // AFFINITY_INFO: 0 on, 1 off
                None => INVALID_PARAMETERS,
                Some(t) => (self.state[t] == CoreState::Off) as u64,
            },
            0x8400_0006 => 2,                            // MIGRATE_INFO_TYPE: no migration needed
            0x8400_0008 => return Some(Stop::PowerOff),  // SYSTEM_OFF
            0x8400_0009 => return Some(Stop::Reset),     // SYSTEM_RESET
            0x8400_000a => match x[1] as u32 {           // PSCI_FEATURES
                0x8400_0000 | 0x8400_0002 | 0x8400_0003 | 0xc400_0003 | 0x8400_0004 | 0xc400_0004 | 0x8400_0006 | 0x8400_0008 | 0x8400_0009 | 0x8400_000a => 0,
                _ => NOT_SUPPORTED,
            },
            _ => NOT_SUPPORTED,
        };
        self.cpus[k].x[0] = r;
        None
    }

    /// Start core `t` at `entry` (MMU off, interrupts masked, x0 = `context`) in the caller's EL.
    fn cpu_on(&mut self, caller: usize, t: usize, entry: u64, context: u64) {
        let (el, now) = (self.cpus[caller].el, self.cpus[caller].now_ns());
        let (jit_off, crypto) = (self.cpus[caller].jit_off, self.cpus[caller].crypto);
        let c = &mut self.cpus[t];
        Self::reset_core(c, t, el, entry, context, now);
        c.jit_off = jit_off;
        c.crypto = crypto;
        self.state[t] = CoreState::Run;
        self.round_end = self.round_end.min(now + quantum());
        self.soc.dirty = true;
    }

    /// Core `k` out of reset at `entry` (PSCI CPU_ON): RAM, retired count and JIT are kept.
    pub(crate) fn reset_core(c: &mut Cpu, k: usize, el: u8, entry: u64, context: u64, now: u64) {
        let mut f = Cpu::new();
        f.sys.cntfrq = CNTFRQ;
        f.sys.mpidr = k as u64;
        f.firmware_calls_to_host = true;
        f.ram = c.ram;
        f.ram_len = c.ram_len;
        f.instret = c.instret;
        f.sys.time_base_ns = now;
        f.sys.time_base_instret = c.instret;
        f.el = el;
        f.spsel = true;
        f.daif = 0xf << 6;
        f.pc = entry;
        f.x[0] = context;
        f.sys.cpacr_el1 = 3 << 20;
        f.jit_off = c.jit_off;
        f.crypto = c.crypto;
        f.mt = c.mt;
        f.wall_clock = c.wall_clock;
        let mut old = std::mem::replace(c, f);
        old.hand_jit_to(c);
    }

    /// The optional Cryptographic Extension on every core (QEMU's cortex-a72 has it; a Pi 4 does not).
    pub fn set_crypto(&mut self, on: bool) { for c in self.cpus.iter_mut() { c.crypto = on; } }

    /// Debugging: interpreter only on all cores (needed while breakpoints are set).
    pub fn set_debugging(&mut self, on: bool) { for c in self.cpus.iter_mut() { c.jit_off = on; } }

    /// Debugging: set or clear (`insert` false) a watchpoint on [addr, addr + len) for
    /// accesses of `kind` (1 write, 2 read, 3 either), on every core.
    pub fn watchpoint(&mut self, addr: u64, len: u64, kind: u8, insert: bool) {
        for c in self.cpus.iter_mut() {
            c.watch.retain(|&w| w != (addr, len, kind));
            if insert { c.watch.push((addr, len, kind)); }
        }
        let on = !self.breakpoints.is_empty() || !self.cpus[0].watch.is_empty();
        self.set_debugging(on);
    }

    /// Debugging: execute one instruction on core `k` (interpreter); a stop it causes, if any.
    pub fn debug_step(&mut self, k: usize) -> Option<Stop> {
        if self.state[k] == CoreState::Off { return None; }
        self.state[k] = CoreState::Run;
        let jit_off = self.cpus[k].jit_off;
        self.cpus[k].jit_off = true;
        self.bp_resume = Some((k, self.cpus[k].pc));
        self.round_end = u64::MAX;
        let (mut total, mut steps, mut storm) = (self.instret(), 1, 0);
        let t = self.cpus[k].now_ns();
        let r = self.slice(k, t, &mut total, u64::MAX, &mut steps, &mut storm);
        self.bp_resume = None;
        self.cpus[k].jit_off = jit_off;
        let t = self.cpus[k].now_ns();
        if t > self.now { self.now = t; }
        r
    }

    /// Debugging: read guest memory at virtual address `va` as core `k` sees it (RAM only).
    pub fn debug_read(&mut self, k: usize, va: u64, len: usize) -> Option<Vec<u8>> {
        let mut out = Vec::with_capacity(len);
        while out.len() < len {
            let a = va.wrapping_add(out.len() as u64);
            let pa = self.cpus[k].debug_translate(&mut self.soc, a)?;
            let n = (len - out.len()).min(0x1000 - (a & 0xfff) as usize);
            out.extend_from_slice(self.soc.ram.get(pa as usize..pa as usize + n)?);
        }
        Some(out)
    }

    /// Debugging: write guest memory at virtual address `va` (RAM only); compiled code is dropped.
    pub fn debug_write(&mut self, k: usize, va: u64, data: &[u8]) -> bool {
        let mut done = 0;
        while done < data.len() {
            let a = va.wrapping_add(done as u64);
            let Some(pa) = self.cpus[k].debug_translate(&mut self.soc, a) else { return false };
            let n = (data.len() - done).min(0x1000 - (a & 0xfff) as usize);
            let Some(dst) = self.soc.ram.get_mut(pa as usize..pa as usize + n) else { return false };
            dst.copy_from_slice(&data[done..done + n]);
            done += n;
        }
        if let Some(j) = self.jit_mut() { j.flush(); }
        true
    }

    /// Advance idle time to `t` (after an `Idle` stop) and re-evaluate interrupts.
    pub fn idle_until(&mut self, t: u64) {
        if t > self.now { self.now = t; }
        let now = self.now;
        // waking happens at the idle stop's own deadline only: the caller may get there in
        // wall-clock steps, and those steps must not show (runs stay repeatable)
        let reached = now >= self.idle_target;
        for k in 0..self.cpus.len() {
            if self.state[k] == CoreState::Off { continue; }
            self.catch_up(k, now);
            // a core waiting for an event wakes when time is skipped (its timer, the event stream)
            if reached && self.state[k] == CoreState::Wfe { self.state[k] = CoreState::Run; }
        }
        self.update_irqs();
        if !reached { return; }
        // the old single-core behaviour: an idle stop ends at a deadline, and `wfi` may wake
        // spuriously: a core whose own timer deadline has passed resumes
        for k in 0..self.cpus.len() {
            if self.state[k] != CoreState::Wfi { continue; }
            let s = &self.cpus[k].sys;
            let due = [(s.vtimer, s.cntvoff), (s.ptimer, 0)].iter().any(|(tm, off)| tm.ctl & 3 == 1 && Self::ns_of_count(tm.cval.wrapping_add(*off)) <= now);
            if due { self.state[k] = CoreState::Run; }
        }
    }
}

const SNAP_MAGIC: &[u8; 8] = b"SBCSNAP1";

impl Machine {
    /// Serialize the whole machine (deflate-compressed; RAM keeps only non-zero pages).
    pub fn save(&self) -> Vec<u8> {
        let mut w = emu_core::snap::W::default();
        w.tag("machine-v10");
        w.u64(self.soc.ram.len() as u64);
        w.u64(self.now); w.u64(self.exceptions); w.u64(self.irqs);
        w.u32(self.cpus.len() as u32);
        for (c, st) in self.cpus.iter().zip(&self.state) {
            w.u8(match st { CoreState::Off => 0, CoreState::Run => 1, CoreState::Wfi => 2, CoreState::Wfe => 3 });
            c.save_state(&mut w);
        }
        self.soc.gic.save_state(&mut w);
        self.soc.uart.save_state(&mut w);
        self.soc.mbox.save_state(&mut w);
        self.soc.sd.save_state(&mut w);
        self.soc.usb.save_state(&mut w);
        self.soc.genet.save_state(&mut w);
        self.soc.wifi.save_state(&mut w);
        self.soc.gpio.save_state(&mut w);
        self.soc.pm.save_state(&mut w);
        self.soc.systimer.save_state(&mut w);
        self.soc.aux.save_state(&mut w);
        self.soc.dma.save_state(&mut w);
        self.soc.devs.save_state(&mut w);
        self.soc.pcie.save_state(&mut w);
        self.soc.rng.save_state(&mut w);
        self.soc.i2s.save_state(&mut w);
        w.bool(self.soc.bt.is_some());
        if let Some(bt) = self.soc.bt.as_ref() { bt.save_state(&mut w); }
        // v9: the display pipeline and the firmware clock rates
        w.bool(self.soc.vc4.is_some());
        if let Some(v) = self.soc.vc4.as_ref() { v.save_state(&mut w); }
        for r in self.soc.mbox.rates { w.u32(r); }
        // v10: the 3D GPU
        w.bool(self.soc.v3d.is_some());
        if let Some(g) = self.soc.v3d.as_ref() { g.save_state(&mut w); }
        // RAM: list of non-zero 4 KiB pages
        let pages: Vec<usize> = (0..self.soc.ram.len() / 4096).filter(|&p| self.soc.ram[p * 4096..(p + 1) * 4096].iter().any(|&b| b != 0)).collect();
        w.u64(pages.len() as u64);
        for p in pages { w.u32(p as u32); w.0.extend_from_slice(&self.soc.ram[p * 4096..(p + 1) * 4096]); }
        let mut out = SNAP_MAGIC.to_vec();
        out.extend_from_slice(&miniz_oxide::deflate::compress_to_vec(&w.0, 1));
        out
    }

    /// Restore a snapshot into a machine that already has the same SD card attached. Snapshots
    /// of the single-core machine (v4) restore into any machine; their extra cores stay off.
    pub fn restore(&mut self, snap: &[u8]) -> Result<(), String> {
        if snap.len() < 8 || &snap[..8] != SNAP_MAGIC { return Err("not an sbc-emu snapshot".into()); }
        let raw = miniz_oxide::inflate::decompress_to_vec(&snap[8..]).map_err(|e| format!("snapshot inflate: {e:?}"))?;
        let mut r = emu_core::snap::R::new(&raw);
        let e = |e: emu_core::snap::SnapError| e.0;
        let v10 = r.tag("machine-v10").is_ok();
        if !v10 { r.p = 0; }
        let v9 = v10 || r.tag("machine-v9").is_ok();
        if !v9 { r.p = 0; }
        let v8 = v9 || r.tag("machine-v8").is_ok();
        if !v8 { r.p = 0; }
        let v7 = v8 || r.tag("machine-v7").is_ok();
        if !v7 { r.p = 0; }
        let v6 = v7 || r.tag("machine-v6").is_ok();
        if !v6 { r.p = 0; }
        let v5 = v6 || r.tag("machine-v5").is_ok();
        if !v5 { r.p = 0; r.tag("machine-v4").map_err(e)?; }
        let ram = r.u64().map_err(e)? as usize;
        if ram != self.soc.ram.len() { return Err(format!("snapshot RAM is {} MiB, machine has {} MiB", ram >> 20, self.soc.ram.len() >> 20)); }
        self.now = r.u64().map_err(e)?; self.exceptions = r.u64().map_err(e)?; self.irqs = r.u64().map_err(e)?;
        // the JIT's code is stale after a restore; keep one (empty) cache with core 0
        let mut jit = self.cpus[self.cur].jit.take();
        if let Some(j) = jit.as_mut() { j.flush(); }
        self.cur = 0;
        self.soc.cur = 0;
        for k in 0..self.cpus.len() { self.cpus[k] = self.fresh_cpu(k); self.state[k] = CoreState::Off; }
        if v5 {
            let n = r.u32().map_err(e)? as usize;
            if n != self.cpus.len() { return Err(format!("snapshot has {n} cores, machine has {} (use --smp {n})", self.cpus.len())); }
            for k in 0..n {
                self.state[k] = match r.u8().map_err(e)? { 0 => CoreState::Off, 2 => CoreState::Wfi, 3 => CoreState::Wfe, _ => CoreState::Run };
                self.cpus[k].load_state(&mut r).map_err(e)?;
            }
        } else {
            self.cpus[0].load_state(&mut r).map_err(e)?;
            self.state[0] = CoreState::Run;
        }
        self.soc.gic.load_state(&mut r).map_err(e)?;
        self.soc.uart.load_state(&mut r).map_err(e)?;
        self.soc.mbox.load_state(&mut r).map_err(e)?;
        self.soc.sd.load_state(&mut r).map_err(e)?;
        self.soc.usb.load_state(&mut r, &mut self.soc.devs.kbd, !v8).map_err(e)?;
        self.soc.genet.load_state(&mut r).map_err(e)?;
        self.soc.wifi.load_state(&mut r).map_err(e)?;
        self.soc.gpio.load_state(&mut r).map_err(e)?;
        if v6 {
            self.soc.pm.load_state(&mut r).map_err(e)?;
            self.soc.systimer.load_state(&mut r).map_err(e)?;
            self.soc.aux.load_state(&mut r).map_err(e)?;
        }
        if v7 { self.soc.dma.load_state(&mut r).map_err(e)?; }
        if v8 {
            self.soc.devs.load_state(&mut r).map_err(e)?;
            self.soc.pcie.load_state(&mut r).map_err(e)?;
            self.soc.rng.load_state(&mut r).map_err(e)?;
            self.soc.i2s.load_state(&mut r).map_err(e)?;
            self.soc.mbox.audio_clk = self.soc.i2s.bclk;
            if r.bool().map_err(e)? {
                let bt = self.soc.bt.get_or_insert_with(crate::bluetooth::Bluetooth::new);
                bt.load_state(&mut r).map_err(e)?;
            } else {
                self.soc.bt = None;
            }
            if v9 {
                if r.bool().map_err(e)? {
                    let v = self.soc.vc4.get_or_insert_with(crate::vc4::Vc4::new);
                    v.load_state(&mut r).map_err(e)?;
                } else {
                    self.soc.vc4 = None;
                }
                for x in self.soc.mbox.rates.iter_mut() { *x = r.u32().map_err(e)?; }
            }
            if v10 {
                if r.bool().map_err(e)? {
                    let g = self.soc.v3d.get_or_insert_with(v3d::V3d::new);
                    g.load_state(&mut r).map_err(e)?;
                } else {
                    self.soc.v3d = None;
                }
            }
            self.soc.pace_next = Some(0);
        } else {
            // before PCIe: the keyboard was on the OTG port and the guest never saw the xHCI
            self.soc.pcie = crate::pcie::Pcie::new();
            // and the RNG's control register was not saved: it was enabled by then
            if v7 { self.soc.rng.write(0, 0x00ff_2001); }
        }
        self.soc.ram.fill(0);
        for _ in 0..r.u64().map_err(e)? {
            let p = r.u32().map_err(e)? as usize;
            let b = r.b.get(r.p..r.p + 4096).ok_or("truncated RAM page")?;
            self.soc.ram[p * 4096..(p + 1) * 4096].copy_from_slice(b);
            r.p += 4096;
        }
        for c in self.cpus.iter_mut() { c.ram = self.soc.ram.as_mut_ptr(); c.ram_len = self.soc.ram.len() as u64; }
        self.cpus[0].jit = jit;
        self.next_check = 0;
        self.soc.dirty = true;
        Ok(())
    }
}
