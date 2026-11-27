//! The DesignWare USB 2.0 OTG controller of the BCM2711 (the Pi 4's USB-C port) and the
//! BCM2837 (the Pi 3's only USB host), in host mode. Its one root port reaches a `UsbBus`: one
//! device from `usbdev` (`write`, the Pi 4), or a tree of hubs and devices (`write_bus`, the
//! Pi 3's LAN7515).
//!
//! On a real Pi 4 this port is host-capable with `dtoverlay=dwc2,dr_mode=host`; the device
//! tree describes exactly that, and Linux's `dwc2` driver runs it. The model implements the
//! host side of the core in buffer-DMA mode: a channel transfer runs to completion the moment
//! the driver enables the channel, moving data through the channel's DMA address, and reports
//! through HCINT/HAINT/GINTSTS as the core does. A bulk or interrupt IN transfer the device
//! NAKs is held (the channel stays enabled, as the core keeps retrying) until the device has
//! data (`poll`): the driver sees one completion instead of a NAK per (micro)frame.
//!
//! The root port runs at the device's speed. Behind a high-speed hub, full-speed devices are
//! reached with split transactions (HCSPLT): the start split is acknowledged at once (an OUT
//! or SETUP's data goes to the device then), the complete split returns the device's answer.
//! Frames raise SOF every 1 ms, which drives the driver's periodic scheduler; at high speed
//! the frame number counts microframes, 8 per SOF.
//!
//! Identity registers carry the values a Pi reports (core 2.80a, 8 host channels, internal
//! DMA, 19-bit transfer size), so the driver configures itself as on hardware.
use crate::usbdev::{DevId, Devices, RootPort, Speed, UsbBus};

const GOTGCTL: usize = 0x000;
const GAHBCFG: usize = 0x008;
const GUSBCFG: usize = 0x00c;
const GRSTCTL: usize = 0x010;
const GINTSTS: usize = 0x014;
const GINTMSK: usize = 0x018;
const GSNPSID: usize = 0x040;
const GHWCFG1: usize = 0x044;
const GHWCFG2: usize = 0x048;
const GHWCFG3: usize = 0x04c;
const GHWCFG4: usize = 0x050;
const HFNUM: usize = 0x408;
const HAINT: usize = 0x414;
const HAINTMSK: usize = 0x418;
const HPRT0: usize = 0x440;
const HC_BASE: usize = 0x500;
const NCHAN: usize = 8;

// GINTSTS
const GINT_CURMOD_HOST: u32 = 1 << 0;
const GINT_SOF: u32 = 1 << 3;
const GINT_PRTINT: u32 = 1 << 24;
const GINT_HCHINT: u32 = 1 << 25;
// HPRT0
const PRT_CONNSTS: u32 = 1 << 0;
const PRT_CONNDET: u32 = 1 << 1;
const PRT_ENA: u32 = 1 << 2;
const PRT_ENACHG: u32 = 1 << 3;
const PRT_OVRCURRCHG: u32 = 1 << 5;
const PRT_RST: u32 = 1 << 8;
const PRT_PWR: u32 = 1 << 12;
const PRT_SPEED_FS: u32 = 1 << 17;
// HCINT
const HC_XFERCOMPL: u32 = 1 << 0;
const HC_CHHLTD: u32 = 1 << 1;
const HC_STALL: u32 = 1 << 3;
const HC_NAK: u32 = 1 << 4;
const HC_ACK: u32 = 1 << 5;
const HC_XACTERR: u32 = 1 << 7;
// HCCHAR
const CH_ENA: u32 = 1 << 31;
const CH_DIS: u32 = 1 << 30;
// HCSPLT
const SPLT_ENA: u32 = 1 << 31;
const SPLT_COMPL: u32 = 1 << 16;
// endpoint types (HCCHAR.EPTYPE)
const EP_BULK: u32 = 2;
const EP_INTR: u32 = 3;

const PID_DATA0: u32 = 0;
const PID_DATA1: u32 = 2;
const PID_SETUP: u32 = 3;

#[derive(Default, Clone, Copy)]
struct Chan { hcchar: u32, hcsplt: u32, hcint: u32, hcintmsk: u32, hctsiz: u32, hcdma: u32 }

pub struct Dwc2 {
    g: [u32; 0x60],
    hprt: u32,
    hcfg: u32,
    hfir: u32,
    haintmsk: u32,
    gintsts: u32,
    chan: [Chan; NCHAN],
    /// what start splits of OUTs and SETUPs got from their devices, for the complete splits
    /// (which the driver may issue on another channel), as the hub's TT holds them: keyed by
    /// HCCHAR's device address, endpoint and direction; (HCINT bits, next PID << 29)
    split: Vec<(u32, u32)>,
    /// the device plugged into the root port (`write`; `write_bus` brings its own bus)
    pub port: Option<DevId>,
    /// frame number (1 ms frames) and when the next one starts (ns)
    frame: u32,
    next_frame_ns: u64,
    pub transfers: u64,
    pub trace: bool,
}

impl Default for Dwc2 {
    fn default() -> Self { Self::new() }
}

impl Dwc2 {
    pub fn new() -> Self {
        let mut d = Dwc2 { g: [0; 0x60], hprt: 0, hcfg: 0, hfir: 48000, haintmsk: 0, gintsts: GINT_CURMOD_HOST, chan: [Chan::default(); NCHAN], split: Vec::new(),
                           port: Some(DevId::Keyboard), frame: 0, next_frame_ns: 0, transfers: 0, trace: false };
        d.reset();
        d
    }

    fn reset(&mut self) {
        self.g = [0; 0x60];
        self.g[GRSTCTL / 4] = 1 << 31; // AHB idle
        self.g[GSNPSID / 4] = 0x4f54_280a;
        self.g[GHWCFG1 / 4] = 0;
        self.g[GHWCFG2 / 4] = 0x228d_dd50;
        self.g[GHWCFG3 / 4] = 0x0ff0_00e8;
        self.g[GHWCFG4 / 4] = 0x1ff0_0020;
        self.g[0x024 / 4] = 0x1000; // GRXFSIZ
        self.g[0x028 / 4] = 0x0100_1000; // GNPTXFSIZ
        self.g[0x100 / 4] = 0x0100_1100; // HPTXFSIZ: periodic TX FIFO, 256 words
        self.gintsts = GINT_CURMOD_HOST;
        self.hprt = 0;
        self.chan = [Chan::default(); NCHAN];
        self.split.clear();
    }

    /// The root port is enabled at high speed (frame numbers count microframes).
    fn high_speed(&self) -> bool { self.hprt & PRT_ENA != 0 && (self.hprt >> 17) & 3 == 0 }

    fn haint(&self) -> u32 {
        (0..NCHAN).fold(0, |a, n| a | (((self.chan[n].hcint & self.chan[n].hcintmsk) != 0) as u32) << n)
    }

    fn gintsts(&self) -> u32 {
        let mut s = self.gintsts | GINT_CURMOD_HOST;
        if self.hprt & (PRT_CONNDET | PRT_ENACHG | PRT_OVRCURRCHG) != 0 { s |= GINT_PRTINT; }
        if self.haint() & self.haintmsk != 0 { s |= GINT_HCHINT; }
        s
    }

    pub fn irq(&self) -> bool {
        self.g[GAHBCFG / 4] & 1 != 0 && self.gintsts() & self.g[GINTMSK / 4] != 0
    }

    /// Advance frames up to `now_ns`; returns true if interrupt state may have changed.
    pub fn tick(&mut self, now_ns: u64) -> bool {
        if self.hprt & PRT_ENA == 0 { self.next_frame_ns = now_ns + 1_000_000; return false; }
        if now_ns < self.next_frame_ns { return false; }
        let frames = (now_ns - self.next_frame_ns) / 1_000_000 + 1;
        let step = if self.high_speed() { 8 } else { 1 };
        self.frame = (self.frame + frames as u32 * step) & 0x3fff;
        self.next_frame_ns += frames * 1_000_000;
        self.gintsts |= GINT_SOF;
        true
    }

    /// When the next frame starts, if SOF interrupts are wanted.
    pub fn next_event(&self) -> Option<u64> {
        (self.hprt & PRT_ENA != 0 && self.g[GINTMSK / 4] & GINT_SOF != 0).then_some(self.next_frame_ns)
    }

    pub fn read(&mut self, off: u64) -> u32 {
        let o = off as usize & 0xffff;
        match o {
            GINTSTS => self.gintsts(),
            HFNUM => self.frame | (0x3a98 << 16),
            HAINT => self.haint(),
            HAINTMSK => self.haintmsk,
            HPRT0 => self.hprt,
            0x400 => self.hcfg,
            0x404 => self.hfir,
            0x410 => 0x0008_0100, // HPTXSTS: queue and FIFO space available
            HC_BASE..=0x5ff => {
                let (n, r) = ((o - HC_BASE) / 0x20, (o - HC_BASE) % 0x20);
                let c = &self.chan[n.min(NCHAN - 1)];
                if n >= NCHAN { return 0; }
                match r { 0x00 => c.hcchar, 0x04 => c.hcsplt, 0x08 => c.hcint, 0x0c => c.hcintmsk, 0x10 => c.hctsiz, 0x14 => c.hcdma, _ => 0 }
            }
            0x02c => 0x0008_0100, // GNPTXSTS
            o if o < 0x180 => self.g[o / 4],
            _ => 0,
        }
    }

    /// A register write with the device of `port` on the root port.
    pub fn write(&mut self, off: u64, v: u32, ram: &mut [u8], devs: &mut Devices) {
        let port = self.port;
        self.write_bus(off, v, ram, &mut RootPort { port, devs });
    }

    /// A register write; the root port reaches `bus`.
    pub fn write_bus(&mut self, off: u64, v: u32, ram: &mut [u8], bus: &mut dyn UsbBus) {
        let o = off as usize & 0xffff;
        match o {
            GRSTCTL => {
                if v & 1 != 0 { self.reset(); return; } // core soft reset completes at once
                self.g[GRSTCTL / 4] = (1 << 31) | (v & !(1 | (1 << 4) | (1 << 5) | (1 << 30)));
            }
            GINTSTS => self.gintsts &= !(v & !(GINT_CURMOD_HOST | GINT_PRTINT | GINT_HCHINT)),
            HAINTMSK => self.haintmsk = v & 0xff,
            HPRT0 => self.write_hprt(v, bus),
            0x400 => self.hcfg = v,
            0x404 => self.hfir = v,
            HC_BASE..=0x5ff => {
                let (n, r) = ((o - HC_BASE) / 0x20, (o - HC_BASE) % 0x20);
                if n >= NCHAN { return; }
                match r {
                    0x00 => {
                        self.chan[n].hcchar = v;
                        if v & CH_ENA != 0 {
                            if v & CH_DIS != 0 {
                                // disable request on an active (or held) channel: halt it
                                self.chan[n].hcchar &= !(CH_ENA | CH_DIS);
                                self.chan[n].hcint |= HC_CHHLTD;
                            } else {
                                self.start(n, ram, bus);
                                bus.settle();
                            }
                        }
                    }
                    0x04 => self.chan[n].hcsplt = v,
                    0x08 => self.chan[n].hcint &= !v,
                    0x0c => self.chan[n].hcintmsk = v,
                    0x10 => self.chan[n].hctsiz = v,
                    0x14 => self.chan[n].hcdma = v,
                    _ => {}
                }
            }
            o if o < 0x180 => self.g[o / 4] = v,
            _ => {}
        }
        let _ = GOTGCTL;
        let _ = GUSBCFG;
    }

    fn write_hprt(&mut self, v: u32, bus: &mut dyn UsbBus) {
        // write-1-to-clear change bits; writing ENA disables the port
        self.hprt &= !(v & (PRT_CONNDET | PRT_ENACHG | PRT_OVRCURRCHG));
        if v & PRT_ENA != 0 && self.hprt & PRT_ENA != 0 { self.hprt &= !PRT_ENA; self.hprt |= PRT_ENACHG; }
        let was_reset = self.hprt & PRT_RST != 0;
        let was_powered = self.hprt & PRT_PWR != 0;
        self.hprt = (self.hprt & !(PRT_RST | PRT_PWR)) | (v & (PRT_RST | PRT_PWR));
        if !was_powered && self.hprt & PRT_PWR != 0 && bus.root_speed().is_some() {
            // a device is plugged in: connect is detected once the port has power
            self.hprt |= PRT_CONNSTS | PRT_CONNDET;
        }
        if was_reset && self.hprt & PRT_RST == 0 && self.hprt & PRT_CONNSTS != 0 {
            // end of port reset: enabled at the device's speed, device back to its default state
            let fs = bus.root_speed().is_none_or(|s| s == Speed::Full);
            self.hprt = (self.hprt & !(3 << 17)) | PRT_ENA | PRT_ENACHG | if fs { PRT_SPEED_FS } else { 0 };
            bus.root_reset();
        }
        if self.trace { eprintln!("[usb] HPRT0 <- {v:#x} = {:#x}", self.hprt); }
    }

    /// Retry the held channels whose devices now have data for them.
    pub fn poll(&mut self, ram: &mut [u8], bus: &mut dyn UsbBus) {
        if self.hprt & PRT_ENA == 0 { return; }
        for n in 0..NCHAN {
            let c = self.chan[n];
            if c.hcchar & CH_ENA == 0 { continue; }
            let (addr, ep) = (((c.hcchar >> 22) & 0x7f) as u8, ((c.hcchar >> 11) & 0xf) as u8);
            if bus.device(addr).is_some_and(|d| d.has_input_ep(ep)) {
                self.start(n, ram, bus);
                bus.settle();
            }
        }
    }

    /// The channels held for data from their devices (bit n = channel n).
    pub fn held(&self) -> u32 { (0..NCHAN).fold(0, |a, n| a | (((self.chan[n].hcchar & CH_ENA != 0) as u32) << n)) }

    /// Run a channel's transfer now (buffer DMA), hold it, or do its part of a split transaction.
    fn start(&mut self, n: usize, ram: &mut [u8], bus: &mut dyn UsbBus) {
        self.transfers += 1;
        let c = self.chan[n];
        let addr = ((c.hcchar >> 22) & 0x7f) as u8;
        let ep = ((c.hcchar >> 11) & 0xf) as u8;
        let dir_in = c.hcchar & (1 << 15) != 0;
        let eptype = (c.hcchar >> 18) & 3;
        let size = (c.hctsiz & 0x7ffff) as usize;
        let pid = (c.hctsiz >> 29) & 3;
        let split = c.hcsplt & SPLT_ENA != 0;
        let complete = split && c.hcsplt & SPLT_COMPL != 0;
        let out = ep == 0 && pid == PID_SETUP || !dir_in;
        if self.trace && split { eprintln!("[usb] ch{n} {} split addr {addr} ep{ep} {} type {eptype} size {size} pid {pid} hcsplt {:#x}", if complete { "complete" } else { "start" }, if dir_in { "IN" } else { "OUT" }, c.hcsplt); }
        let done = |s: &mut Self, bits: u32, moved: usize, pid_next: u32| {
            let ch = &mut s.chan[n];
            ch.hcchar &= !(CH_ENA | CH_DIS);
            ch.hctsiz = (ch.hctsiz & !(0x7ffff | (0x3ff << 19) | (3 << 29))) | ((size - moved.min(size)) as u32) | (pid_next << 29);
            ch.hcint |= bits | HC_CHHLTD;
        };
        let key = c.hcchar & ((0x7f << 22) | (0xf << 11) | (1 << 15));
        if complete && out {
            // complete split of an OUT or SETUP: what the device said at the start split
            let Some(i) = self.split.iter().position(|e| e.0 == key) else { return done(self, HC_XACTERR, 0, pid) };
            let (_, r) = self.split.swap_remove(i);
            return done(self, r & 0xffff, 0, (r >> 29) & 3);
        }
        if split && !complete && !out {
            // start split of an IN: the hub's transaction translator takes it
            return done(self, HC_ACK, 0, pid);
        }
        let dev = match bus.device(addr) {
            Some(d) if self.hprt & PRT_ENA != 0 => d,
            _ => {
                if self.trace { eprintln!("[usb] ch{n} addr {addr} ep{ep}: no device"); }
                return done(self, HC_XACTERR, 0, pid);
            }
        };
        let r = Self::transact(dev, ram, c, self.trace.then_some(n));
        match (r, split) {
            (Some((bits, moved, pid_next)), false) => done(self, bits, moved, pid_next),
            // the start split of an OUT or SETUP: the data went to the device; the TT acknowledges
            (Some((bits, _, pid_next)), true) if !complete => {
                self.split.retain(|e| e.0 != key);
                self.split.push((key, (pid_next << 29) | bits));
                done(self, HC_ACK, 0, pid)
            }
            (Some((bits, moved, pid_next)), true) => done(self, bits, moved, pid_next),
            // the device has nothing: a bulk or interrupt IN waits for it, enabled; through a
            // TT the complete split returns the NAK
            (None, false) if eptype == EP_BULK || eptype == EP_INTR => {}
            (None, _) => done(self, HC_NAK, 0, pid),
        }
    }

    /// One transfer with the device: (HCINT bits, bytes moved, next PID), None for a NAK.
    fn transact(dev: &mut dyn crate::usbdev::UsbDevice, ram: &mut [u8], c: Chan, trace: Option<usize>) -> Option<(u32, usize, u32)> {
        let ep = ((c.hcchar >> 11) & 0xf) as u8;
        let dir_in = c.hcchar & (1 << 15) != 0;
        let mps = (c.hcchar & 0x7ff).max(1) as usize;
        let size = (c.hctsiz & 0x7ffff) as usize;
        let pid = (c.hctsiz >> 29) & 3;
        let dma = crate::machine::dma_phys(c.hcdma, ram.len() as u64) as usize;
        let toggle = |p: u32, packets: usize| if packets % 2 == 0 { p } else if p == PID_DATA0 { PID_DATA1 } else { PID_DATA0 };
        let ok = HC_XFERCOMPL | HC_ACK;
        if ep == 0 && pid == PID_SETUP {
            let mut setup = [0u8; 8];
            if let Some(s) = ram.get(dma..dma + 8) { setup.copy_from_slice(s); }
            let accepted = dev.setup(setup);
            if let Some(n) = trace { eprintln!("[usb] ch{n} SETUP {setup:02x?} -> {}", if accepted { "ok" } else { "STALL" }); }
            return Some(if accepted { (ok, 8, PID_DATA1) } else { (HC_STALL, 0, pid) });
        }
        if dir_in {
            let data = if ep == 0 { dev.control_in(size) } else { dev.data_in(ep, size)? };
            let len = data.len().min(size);
            if let Some(d) = ram.get_mut(dma..dma + len) { d.copy_from_slice(&data[..len]); }
            if let Some(n) = trace { eprintln!("[usb] ch{n} IN ep{ep} {len} bytes"); }
            return Some((ok, len, toggle(pid, len.div_ceil(mps).max(1))));
        }
        let data = ram.get(dma..dma + size).map(|d| d.to_vec()).unwrap_or_default();
        if ep == 0 { dev.control_out(&data); } else { dev.data_out(ep, &data); }
        Some((ok, size, toggle(pid, size.div_ceil(mps).max(1))))
    }
}

impl Dwc2 {
    pub fn save_state(&self, w: &mut emu_core::snap::W) {
        w.tag("dwc2-v3");
        for v in self.g { w.u32(v); }
        for v in [self.hprt, self.hcfg, self.hfir, self.haintmsk, self.gintsts, self.frame] { w.u32(v); }
        for c in &self.chan { for v in [c.hcchar, c.hcsplt, c.hcint, c.hcintmsk, c.hctsiz, c.hcdma] { w.u32(v); } }
        w.u64(self.next_frame_ns);
        w.u8(crate::xhci::dev_code(self.port));
        w.u32(self.split.len() as u32);
        for &(k, v) in &self.split { w.u32(k); w.u32(v); }
    }
    /// `v1`: a snapshot from before the device layer, with the keyboard state inline. `dwc2-v2`
    /// (before split transactions) loads too.
    pub fn load_state(&mut self, r: &mut emu_core::snap::R, kbd: &mut crate::usbdev::Keyboard, v1: bool) -> Result<(), emu_core::snap::SnapError> {
        let v3 = !v1 && (emu_core::snap::R { b: r.b, p: r.p }).tag("dwc2-v3").is_ok();
        r.tag(if v1 { "dwc2-v1" } else if v3 { "dwc2-v3" } else { "dwc2-v2" })?;
        for v in self.g.iter_mut() { *v = r.u32()?; }
        for v in [&mut self.hprt, &mut self.hcfg, &mut self.hfir, &mut self.haintmsk, &mut self.gintsts, &mut self.frame] { *v = r.u32()?; }
        for c in self.chan.iter_mut() { for v in [&mut c.hcchar, &mut c.hcsplt, &mut c.hcint, &mut c.hcintmsk, &mut c.hctsiz, &mut c.hcdma] { *v = r.u32()?; } }
        self.next_frame_ns = r.u64()?;
        if v1 {
            let k = kbd;
            k.st.address = r.u8()?; let pa = r.bool()?; let a = r.u8()?; k.st.pending_address = pa.then_some(a);
            k.st.configured = r.bool()?; k.st.pending_in = r.bytes()?.to_vec(); k.held = r.bytes()?.to_vec(); k.modifiers = r.u8()?; k.leds = r.u8()?;
            let has = r.bool()?; let l = r.bytes()?; k.last = if has { l.try_into().ok() } else { None };
            k.events.clear();
            self.port = Some(DevId::Keyboard);
        } else {
            self.port = crate::xhci::dev_of(r.u8()?);
        }
        self.split.clear();
        if v3 { for _ in 0..r.u32()?.min(64) { let k = r.u32()?; let v = r.u32()?; self.split.push((k, v)); } }
        Ok(())
    }
}
