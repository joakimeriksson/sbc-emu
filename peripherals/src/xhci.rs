//! An xHCI 1.0 host controller core, configurable for a board: where its DMA memory starts, which
//! root ports are USB 2.0 and USB 3.0 (`Layout`), and whether a controller's firmware has to start
//! first (`ready`). The wrapper around it (PCIe, a platform device, a falcon) is the board's.
//!
//! From sbc-emu's Orange Pi RV model (`starfive-jh7110/src/xhci.rs`), itself from the Raspberry
//! Pi 4's (`bcm2711/src/xhci.rs`). The emulated devices plug into USB 2.0 ports; USB 3.0 ports are
//! always empty. One interrupter, 32 device slots, 32-byte contexts, no scratchpad. Commands
//! complete the moment the command doorbell rings; a transfer descriptor runs the moment its
//! doorbell rings, except an IN transfer the device has nothing for (a NAK), which waits and is
//! retried every millisecond of virtual time while the device has input. Isochronous endpoints
//! (USB audio) run on the controller's clock instead: one TD per service interval of virtual
//! time, from the first doorbell on (Start Isoch ASAP; the controller offers no frame IDs).
use crate::dma::Dma;
use emu_core::usb::{DevId, Devices, Speed, UsbDevice};

/// The root ports (0-based): USB 2.0 and USB 3.0, each a range, together all of them.
#[derive(Clone, Debug)]
pub struct Layout {
    pub usb2: std::ops::Range<usize>,
    pub usb3: std::ops::Range<usize>,
}

const MAX_SLOTS: usize = 32;

// register blocks inside the 4 KiB BAR
const OP: u64 = 0x20;
const PORTS: u64 = OP + 0x400;
const XECP: u64 = 0x500;
const RT: u64 = 0x600;
const DB: u64 = 0x800;

// USBCMD / USBSTS
const CMD_RS: u32 = 1 << 0;
const CMD_HCRST: u32 = 1 << 1;
const CMD_INTE: u32 = 1 << 2;
const CMD_CRS: u32 = 1 << 9;
const STS_HCH: u32 = 1 << 0;
const STS_EINT: u32 = 1 << 3;
const STS_PCD: u32 = 1 << 4;
const STS_SRE: u32 = 1 << 10;
const STS_CNR: u32 = 1 << 11;

// PORTSC
const PS_CCS: u32 = 1 << 0;
const PS_PED: u32 = 1 << 1;
const PS_PR: u32 = 1 << 4;
const PS_PP: u32 = 1 << 9;
const PS_LWS: u32 = 1 << 16;
const PS_CSC: u32 = 1 << 17;
const PS_WRC: u32 = 1 << 19;
const PS_PRC: u32 = 1 << 21;
const PS_PLC: u32 = 1 << 22;
const PS_CHANGES: u32 = 0x7f << 17;
const PS_WAKE: u32 = 7 << 25;
const PS_WPR: u32 = 1 << 31;

// TRB types
const T_NORMAL: u32 = 1;
const T_SETUP: u32 = 2;
const T_DATA: u32 = 3;
const T_STATUS: u32 = 4;
const T_ISOCH: u32 = 5;
const T_LINK: u32 = 6;
const T_EVENT_DATA: u32 = 7;
const T_TRANSFER_EV: u32 = 32;
const T_CMD_DONE_EV: u32 = 33;
const T_PORT_EV: u32 = 34;

// completion codes
const CC_SUCCESS: u32 = 1;
const CC_TRB_ERROR: u32 = 5;
const CC_STALL: u32 = 6;
const CC_TRANSACTION: u32 = 4;
const CC_NO_SLOTS: u32 = 9;
const CC_SLOT_NOT_ENABLED: u32 = 11;
const CC_SHORT: u32 = 13;

// endpoint states (endpoint context)
const EP_DISABLED: u8 = 0;
const EP_RUNNING: u8 = 1;
const EP_HALTED: u8 = 2;
const EP_STOPPED: u8 = 3;

#[derive(Clone, Copy, Default)]
struct Ep {
    state: u8,
    deq: u64,
    ccs: bool,
    /// an isochronous endpoint: its service interval (ns), when its next TD is due, and whether
    /// it has TDs (false once its ring ran dry: the next doorbell starts it again)
    isoch: bool,
    period: u64,
    next: u64,
    active: bool,
}

#[derive(Clone, Copy, Default)]
struct Slot { enabled: bool, port: u8, eps: [Ep; 32] }

#[derive(Clone, Copy, Default)]
struct Port { sc: u32, pmsc: u32, dev: Option<DevId> }

pub struct Xhci {
    usbcmd: u32,
    usbsts: u32,
    dnctrl: u32,
    config: u32,
    crcr: u64,
    cmd_deq: u64,
    cmd_ccs: bool,
    crr: bool,
    dcbaap: u64,
    ports: Vec<Port>,
    layout: Layout,
    /// Controller Not Ready (USBSTS bit 11) reads as set until the board says so.
    pub ready: bool,
    iman: u32,
    imod: u32,
    /// interrupt moderation: no interrupt is signalled before this time (ns), IMODI after the
    /// pending bit last cleared (xHCI 1.0, 4.17.2)
    irq_hold: u64,
    erstsz: u32,
    erstba: u64,
    erdp: u64,
    /// the command ring was just stopped by a write to CRCR's low half
    stopping: bool,
    /// events that wait for room in the ring
    pending: std::collections::VecDeque<(u64, u32, u32)>,
    /// event ring producer: segment, index, cycle state
    ev_seg: u32,
    ev_idx: u32,
    ev_pcs: bool,
    slots: [Slot; MAX_SLOTS + 1],
    now: u64,
    next_poll: u64,
    pub transfers: u64,
    pub trace: bool,
}

fn rd32(ram: &Dma, a: u64) -> u32 { ram.get(a, 4).map_or(0, |b| u32::from_le_bytes(b.try_into().unwrap())) }
fn rd64(ram: &Dma, a: u64) -> u64 { rd32(ram, a) as u64 | (rd32(ram, a.wrapping_add(4)) as u64) << 32 }
fn wr32(ram: &mut Dma, a: u64, v: u32) { if let Some(b) = ram.slice(a, 4) { b.copy_from_slice(&v.to_le_bytes()); } }
fn wr64(ram: &mut Dma, a: u64, v: u64) { wr32(ram, a, v as u32); wr32(ram, a.wrapping_add(4), (v >> 32) as u32); }

/// The shortest time between an interrupt's acknowledgement and the next one.
const MIN_HOLD_NS: u64 = 1000;

/// Addresses have 52 bits here, so that sums of them do not overflow.
const ADDRESS: u64 = (1 << 52) - 1;

/// Segments looked at (HCSPARAMS2 allows two).
const MAX_SEGMENTS: u32 = 16;

/// Events kept while the driver's ring is full.
const MAX_PENDING: usize = 4096;

#[derive(Clone, Copy)]
struct Trb { addr: u64, param: u64, status: u32, control: u32 }
impl Trb {
    fn read(ram: &Dma, a: u64) -> Trb { Trb { addr: a, param: rd64(ram, a), status: rd32(ram, a.wrapping_add(8)), control: rd32(ram, a.wrapping_add(12)) } }
    fn ty(&self) -> u32 { (self.control >> 10) & 0x3f }
    fn cycle(&self) -> bool { self.control & 1 != 0 }
    fn chain(&self) -> bool { self.control & (1 << 4) != 0 }
    fn ioc(&self) -> bool { self.control & (1 << 5) != 0 }
    fn isp(&self) -> bool { self.control & (1 << 2) != 0 }
    fn idt(&self) -> bool { self.control & (1 << 6) != 0 }
    fn len(&self) -> usize { (self.status & 0x1ffff) as usize }
    fn slot(&self) -> usize { (self.control >> 24) as usize }
}

impl Xhci {
    pub fn new(layout: Layout) -> Self {
        let mut x = Xhci {
            ports: vec![Port::default(); layout.usb2.len() + layout.usb3.len()], layout, ready: true,
            usbcmd: 0, usbsts: STS_HCH, dnctrl: 0, config: 0, crcr: 0, cmd_deq: 0, cmd_ccs: true, crr: false, stopping: false, pending: Default::default(), dcbaap: 0,
            iman: 0, imod: 4000, irq_hold: 0, erstsz: 0, erstba: 0, erdp: 0, ev_seg: 0, ev_idx: 0, ev_pcs: true,
            slots: [Slot::default(); MAX_SLOTS + 1], now: 0, next_poll: 0, transfers: 0, trace: std::env::var_os("SBC_TRACE_XHCI").is_some(),
        };
        x.reset_ports();
        x
    }

    /// Plug devices into the USB 2.0 ports, in their order.
    pub fn attach(&mut self, devs: &[Option<DevId>]) {
        for (k, p) in self.ports[self.layout.usb2.clone()].iter_mut().enumerate() { p.dev = devs.get(k).copied().flatten(); }
        self.reset_ports();
    }

    pub fn attached(&self) -> Vec<DevId> { self.ports.iter().filter_map(|p| p.dev).collect() }

    /// Hot-plug `dev` into the first free USB 2.0 port; returns the port number.
    pub fn plug(&mut self, dev: DevId, ram: &mut Dma, devs: &mut Devices) -> Option<usize> {
        if self.ports.iter().any(|p| p.dev == Some(dev)) { return None; }
        let k = self.layout.usb2.clone().find(|&k| self.ports[k].dev.is_none())?;
        if let Some(d) = devs.get(dev) { d.bus_reset(); }
        let p = &mut self.ports[k];
        p.dev = Some(dev);
        p.sc = (p.sc & !(0xf << 5) & !PS_PED) | PS_CCS | PS_CSC | (7 << 5);
        self.port_event(ram, k);
        Some(k + 1)
    }

    /// Unplug `dev`: the port reports the disconnect; returns the port number.
    pub fn unplug(&mut self, dev: DevId, ram: &mut Dma) -> Option<usize> {
        let k = self.ports.iter().position(|p| p.dev == Some(dev))?;
        let p = &mut self.ports[k];
        p.dev = None;
        p.sc = (p.sc & !(PS_CCS | PS_PED) & !(0xf << 5) & !(0xf << 10)) | PS_CSC | (5 << 5);
        self.port_event(ram, k);
        Some(k + 1)
    }

    fn reset_ports(&mut self) {
        for (k, p) in self.ports.iter_mut().enumerate() {
            p.pmsc = 0;
            p.sc = PS_PP | if self.layout.usb2.contains(&k) { 0 } else { 5 << 5 }; // USB 3 ports wait in RxDetect
            if p.dev.is_some() { p.sc |= PS_CCS | PS_CSC | (7 << 5); } // connected, Polling
        }
    }

    fn port_speed(d: &mut dyn UsbDevice) -> u32 { match d.speed() { Speed::Full => 1, Speed::High => 3 } }

    fn reset(&mut self, devs: &mut Devices) {
        let ports: Vec<Option<DevId>> = self.ports.iter().map(|p| p.dev).collect();
        let trace = self.trace;
        *self = Xhci { trace, now: self.now, ready: self.ready, ..Xhci::new(self.layout.clone()) };
        for (p, d) in self.ports.iter_mut().zip(ports) { p.dev = d; }
        self.reset_ports();
        for p in &self.ports { if let Some(d) = p.dev.and_then(|d| devs.get(d)) { d.bus_reset(); } }
    }

    /// The controller lost its power or was held in reset.
    pub fn power_on_reset(&mut self, devs: &mut Devices) { self.reset(devs); }

    /// The interrupt was signalled as an MSI: with MSI enabled the interrupter's pending bit
    /// clears by itself (xHCI 1.0, 5.5.2.1), and Linux relies on it (`ip_autoclear`).
    pub fn msi_sent(&mut self) {
        self.iman &= !1;
        self.moderate();
    }

    /// The pending bit cleared: the moderation counter reloads from IMODI (250 ns units).
    /// Without it an interrupt the driver leaves pending (Linux 5.15's handler returns early
    /// while its controller state still says halted) would fire again at once, and the CPU
    /// that is about to change that state would never get to run.
    fn moderate(&mut self) {
        // (Tegra's driver sets IMODI to 0; the hold still has to let the CPU run)
        self.irq_hold = self.now + ((self.imod & 0xffff) as u64 * 250).max(MIN_HOLD_NS);
    }

    pub fn irq(&self) -> bool { self.usbcmd & CMD_INTE != 0 && self.iman & 3 == 3 && self.now >= self.irq_hold }

    // ------------------------------------------------------------------ registers

    fn protocol_ports(r: &std::ops::Range<usize>) -> u32 { (r.len() as u32) << 8 | (r.start as u32 + 1) }

    pub fn read(&mut self, off: u64) -> u32 {
        let r = match off {
            0x00 => 0x0100_0000 | OP as u32,                                  // HCIVERSION 1.0, CAPLENGTH
            0x04 => (self.ports.len() as u32) << 24 | 1 << 8 | MAX_SLOTS as u32,        // HCSPARAMS1
            0x08 => 1 << 4,                                                   // HCSPARAMS2: ERST max 2
            0x0c => 0,
            0x10 => ((XECP / 4) as u32) << 16 | 1 << 7 | 1,                   // HCCPARAMS1: xECP, NSS, AC64
            0x14 => DB as u32,
            0x18 => RT as u32,
            0x1c => 0,
            // operational
            0x20 => self.usbcmd,
            0x24 => self.usbsts | if self.ready { 0 } else { STS_CNR },
            0x28 => 1, // 4 KiB pages
            0x34 => self.dnctrl,
            0x38 => (self.crr as u32) << 3,
            0x3c => 0,
            0x50 => self.dcbaap as u32,
            0x54 => (self.dcbaap >> 32) as u32,
            0x58 => self.config,
            o if (PORTS..PORTS + 0x10 * self.ports.len() as u64).contains(&o) => {
                let p = &self.ports[((o - PORTS) / 0x10) as usize];
                match (o - PORTS) % 0x10 { 0 => p.sc, 4 => p.pmsc, _ => 0 }
            }
            // extended capabilities: the supported protocols and their ports
            0x500 => 0x0200_0402, 0x504 => 0x2042_5355, 0x508 => Self::protocol_ports(&self.layout.usb2), 0x50c => 0,
            0x510 => 0x0300_0002, 0x514 => 0x2042_5355, 0x518 => Self::protocol_ports(&self.layout.usb3), 0x51c => 0,
            // runtime
            0x600 => ((self.now / 125_000) & 0x3fff) as u32,
            0x620 => self.iman,
            0x624 => self.imod,
            0x628 => self.erstsz,
            0x630 => self.erstba as u32,
            0x634 => (self.erstba >> 32) as u32,
            0x638 => self.erdp as u32,
            0x63c => (self.erdp >> 32) as u32,
            _ => 0,
        };
        if self.trace && off < DB { eprintln!("[xhci] read {off:#x} = {r:#x}"); }
        r
    }

    pub fn write(&mut self, off: u64, v: u32, ram: &mut Dma, devs: &mut Devices) {
        if self.trace { eprintln!("[xhci] write {off:#x} <- {v:#x}"); }
        let lo = |x: u64| (x & !0xffff_ffff) | v as u64;
        let hi = |x: u64| (x & 0xffff_ffff) | (v as u64) << 32;
        match off {
            0x20 => {
                if v & CMD_HCRST != 0 { self.reset(devs); return; }
                let was = self.usbcmd & CMD_RS != 0;
                // CSS and CRS read as 0. Nothing is kept for a restore (the board resets the whole
                // controller when its power goes), which the driver has to hear of: it starts over.
                self.usbcmd = v & 0x3c0d;
                if v & CMD_CRS != 0 {
                    self.usbsts |= STS_SRE;
                }
                if v & CMD_RS != 0 {
                    self.usbsts &= !STS_HCH;
                    if !was {
                        for k in 0..self.ports.len() { if self.ports[k].sc & PS_CHANGES != 0 { self.port_event(ram, k); } }
                        if self.erstsz != 0 && self.erdp & 8 == 0 && self.erdp & !0xf != self.enqueue_addr(ram) {
                            self.erdp |= 8;
                            self.iman |= 1;
                            self.usbsts |= STS_EINT;
                        }
                    }
                } else {
                    self.usbsts |= STS_HCH;
                    self.crr = false;
                }
            }
            0x24 => self.usbsts &= !(v & 0x041c), // HSE, EINT, PCD, SRE are write-1-to-clear
            0x34 => self.dnctrl = v,
            0x38 | 0x3c => {
                if self.crr {
                    if off == 0x38 && v & 6 != 0 {
                        // command stop/abort: nothing is in flight, the ring just stops
                        self.crr = false;
                        self.stopping = true;
                        let deq = self.cmd_deq;
                        self.post(ram, deq, 24 << 24, T_CMD_DONE_EV << 10);
                    }
                    return;
                }
                // the other half of the write that stopped the ring
                if std::mem::take(&mut self.stopping) && off == 0x3c {
                    return;
                }
                self.crcr = if off == 0x38 { lo(self.crcr) } else { hi(self.crcr) };
                self.cmd_deq = self.crcr & !0x3f;
                self.cmd_ccs = self.crcr & 1 != 0;
            }
            0x50 => self.dcbaap = lo(self.dcbaap) & !0x3f,
            0x54 => self.dcbaap = hi(self.dcbaap),
            0x58 => self.config = v & 0x3ff,
            o if (PORTS..PORTS + 0x10 * self.ports.len() as u64).contains(&o) => {
                let k = ((o - PORTS) / 0x10) as usize;
                match (o - PORTS) % 0x10 {
                    0 => self.write_portsc(k, v, ram, devs),
                    4 => self.ports[k].pmsc = v,
                    _ => {}
                }
            }
            0x620 => {
                let was = self.iman & 1;
                self.iman = (self.iman & !(v & 1)) & 1 | (v & 2);
                if was != 0 && self.iman & 1 == 0 {
                    self.moderate();
                }
            }
            0x624 => self.imod = v,
            0x628 => self.erstsz = v & 0xffff,
            0x630 | 0x634 => {
                self.erstba = if off == 0x630 { lo(self.erstba) & !0x3f } else { hi(self.erstba) };
                self.ev_seg = 0;
                self.ev_idx = 0;
                self.ev_pcs = true;
                self.pending.clear();
            }
            0x638 | 0x63c => {
                let ehb = self.erdp & 8;
                let w = if off == 0x638 { lo(self.erdp) } else { hi(self.erdp) };
                // EHB is write-1-to-clear
                self.erdp = (w & !0xf) | (w & 7) | if off == 0x638 && v & 8 != 0 { 0 } else { ehb };
                // (a low half that points nowhere waits for the high one)
                let valid = self.in_ring(ram, self.erdp & !0xf);
                while let (Some(&(param, status, control)), true) = (self.pending.front(), valid && !self.ring_full(ram)) {
                    self.pending.pop_front();
                    self.put(ram, param, status, control);
                }
                // events still pending: interrupt again, but only while running (a halted
                // controller does not interrupt; Linux 5.15 ignores an interrupt without EINT
                // and would then never clear Event Handler Busy)
                if self.usbcmd & CMD_RS != 0 && self.erdp & 8 == 0 && self.erdp & !0xf != self.enqueue_addr(ram) {
                    self.erdp |= 8;
                    self.iman |= 1;
                    self.usbsts |= STS_EINT;
                }
            }
            o if (DB..DB + 4 * (MAX_SLOTS as u64 + 1)).contains(&o) => {
                if self.usbcmd & CMD_RS == 0 { return; }
                let slot = ((o - DB) / 4) as usize;
                if slot == 0 { self.crr = true; self.stopping = false; self.run_commands(ram, devs); return; }
                // ringing a stopped endpoint restarts it (a halted one waits for Reset Endpoint)
                let dci = (v & 0xff) as usize;
                if slot <= MAX_SLOTS && (1..32).contains(&dci) && self.slots[slot].eps[dci].state == EP_STOPPED {
                    self.slots[slot].eps[dci].state = EP_RUNNING;
                    self.set_ep_ctx(ram, slot, dci);
                }
                self.run_ep(slot, dci, ram, devs);
            }
            _ => {}
        }
    }

    fn write_portsc(&mut self, k: usize, v: u32, ram: &mut Dma, devs: &mut Devices) {
        let usb2 = self.layout.usb2.contains(&k);
        let p = &mut self.ports[k];
        let before = p.sc;
        p.sc &= !(v & PS_CHANGES);
        if v & PS_PED != 0 { p.sc &= !PS_PED; }
        p.sc = (p.sc & !PS_WAKE) | (v & PS_WAKE);
        let reset = (v & PS_PR != 0) || (!usb2 && v & PS_WPR != 0);
        if reset && p.sc & PS_CCS != 0 {
            // a port reset completes at once: enabled, U0, the device in its default state
            let dev = p.dev.and_then(|d| devs.get(d));
            let speed = dev.map_or(1, |d| { d.bus_reset(); Self::port_speed(d) });
            p.sc = (p.sc & !(0xf << 5) & !(0xf << 10)) | PS_PED | speed << 10 | if v & PS_WPR != 0 { PS_WRC | PS_PRC } else { PS_PRC };
            for s in 1..=MAX_SLOTS { if self.slots[s].enabled && self.slots[s].port as usize == k + 1 { self.slots[s].eps = [Ep::default(); 32]; } }
        } else if v & PS_LWS != 0 {
            let pls = (v >> 5) & 0xf;
            let old = (p.sc >> 5) & 0xf;
            p.sc = (p.sc & !(0xf << 5)) | pls << 5;
            if pls == 0 && (old == 3 || old == 15) { p.sc |= PS_PLC; } // resume finished
        }
        let p = &mut self.ports[k];
        if p.sc & PS_CHANGES & !before != 0 { self.port_event(ram, k); }
    }

    fn port_event(&mut self, ram: &mut Dma, k: usize) {
        self.usbsts |= STS_PCD;
        if self.usbcmd & CMD_RS != 0 { self.post(ram, ((k as u64) + 1) << 24, CC_SUCCESS << 24, T_PORT_EV << 10); }
    }

    // ------------------------------------------------------------------ the event ring

    fn segment(&self, ram: &Dma, seg: u32) -> (u64, u32) {
        let e = self.erstba.wrapping_add(16 * seg as u64);
        (rd64(ram, e) & !0x3f, rd32(ram, e.wrapping_add(8)) & 0xffff)
    }

    fn enqueue_addr(&self, ram: &Dma) -> u64 { self.segment(ram, self.ev_seg).0.wrapping_add(16 * self.ev_idx as u64) }

    /// Whether an address is in a segment of the event ring.
    fn in_ring(&self, ram: &Dma, a: u64) -> bool {
        (0..self.erstsz.min(MAX_SEGMENTS)).any(|s| {
            let (base, size) = self.segment(ram, s);
            a.checked_sub(base).is_some_and(|o| o < 16 * size as u64)
        })
    }

    /// The slot after the next one is the driver's dequeue pointer.
    fn ring_full(&self, ram: &Dma) -> bool {
        if self.erstsz == 0 { return false; }
        let (base, size) = self.segment(ram, self.ev_seg);
        let next = if self.ev_idx + 1 < size {
            base.wrapping_add(16 * (self.ev_idx as u64 + 1))
        } else {
            self.segment(ram, if self.ev_seg + 1 < self.erstsz { self.ev_seg + 1 } else { 0 }).0
        };
        next == self.erdp & !0xf
    }

    fn post(&mut self, ram: &mut Dma, param: u64, status: u32, control: u32) {
        if self.pending.is_empty() && !self.ring_full(ram) {
            self.put(ram, param, status, control);
        } else if self.pending.len() < MAX_PENDING {
            self.pending.push_back((param, status, control));
        }
    }

    fn put(&mut self, ram: &mut Dma, param: u64, status: u32, control: u32) {
        if self.erstsz == 0 { return; }
        let (base, size) = self.segment(ram, self.ev_seg);
        if size == 0 { return; }
        let a = base.wrapping_add(16 * self.ev_idx as u64);
        wr64(ram, a, param);
        wr32(ram, a + 8, status);
        std::sync::atomic::fence(std::sync::atomic::Ordering::Release);
        wr32(ram, a + 12, control | self.ev_pcs as u32);
        self.ev_idx += 1;
        if self.ev_idx >= size {
            self.ev_idx = 0;
            self.ev_seg += 1;
            if self.ev_seg >= self.erstsz { self.ev_seg = 0; self.ev_pcs = !self.ev_pcs; }
        }
        self.usbsts |= STS_EINT;
        if self.erdp & 8 == 0 { self.erdp |= 8; self.iman |= 1; }
    }

    // ------------------------------------------------------------------ commands

    fn ctx(&self, ram: &Dma, slot: usize) -> u64 { rd64(ram, self.dcbaap.wrapping_add(8 * slot as u64)) & ADDRESS & !0x3f }

    fn set_ep_ctx(&self, ram: &mut Dma, slot: usize, dci: usize) {
        let out = self.ctx(ram, slot).wrapping_add(32 * dci as u64);
        let ep = self.slots[slot].eps[dci];
        wr32(ram, out, (rd32(ram, out) & !7) | ep.state as u32);
        wr64(ram, out + 8, ep.deq | ep.ccs as u64);
    }

    fn set_slot_state(&self, ram: &mut Dma, slot: usize, state: u32, addr: u32) {
        let out = self.ctx(ram, slot);
        wr32(ram, out + 12, state << 27 | addr);
    }

    fn run_commands(&mut self, ram: &mut Dma, devs: &mut Devices) {
        for _ in 0..4096 {
            let t = Trb::read(ram, self.cmd_deq);
            if t.cycle() != self.cmd_ccs { return; }
            if t.ty() == T_LINK {
                self.cmd_deq = t.param & ADDRESS & !0xf;
                if t.control & 2 != 0 { self.cmd_ccs = !self.cmd_ccs; }
                continue;
            }
            self.cmd_deq = self.cmd_deq.wrapping_add(16);
            let (cc, slot) = self.command(t, ram, devs);
            if self.trace { eprintln!("[xhci] command {} slot {} -> {cc}", t.ty(), slot); }
            self.post(ram, t.addr, cc << 24, T_CMD_DONE_EV << 10 | (slot as u32) << 24);
        }
    }

    fn command(&mut self, t: Trb, ram: &mut Dma, devs: &mut Devices) -> (u32, usize) {
        let slot = t.slot();
        let dci = ((t.control >> 16) & 0x1f) as usize;
        let max_slots = (self.config & 0xff) as usize;
        if !matches!(t.ty(), 9 | 23 | 19 | 20 | 21 | 22) && (slot == 0 || slot > MAX_SLOTS || !self.slots[slot].enabled) {
            return (CC_SLOT_NOT_ENABLED, slot);
        }
        let cc = match t.ty() {
            9 => { // Enable Slot
                return match (1..=max_slots.min(MAX_SLOTS)).find(|&s| !self.slots[s].enabled) {
                    Some(s) => { self.slots[s] = Slot { enabled: true, ..Slot::default() }; (CC_SUCCESS, s) }
                    None => (CC_NO_SLOTS, 0),
                };
            }
            10 => { self.slots[slot] = Slot::default(); CC_SUCCESS } // Disable Slot
            11 => { // Address Device
                let input = t.param & ADDRESS & !0xf;
                let out = self.ctx(ram, slot);
                let port = ((rd32(ram, input + 32 + 4) >> 16) & 0xff) as usize;
                let Some(p) = port.checked_sub(1).and_then(|k| self.ports.get(k)).copied() else { return (CC_TRB_ERROR, slot) };
                let Some(dev) = p.dev.and_then(|d| devs.get(d)).filter(|_| p.sc & PS_PED != 0) else { return (CC_TRANSACTION, slot) };
                for k in (0..64).step_by(4) { let w = rd32(ram, input + 32 + k); wr32(ram, out + k, w); }
                let bsr = t.control & (1 << 9) != 0;
                let addr = if bsr { 0 } else {
                    dev.setup([0, 5, slot as u8, 0, 0, 0, 0, 0]);
                    dev.control_in(0);
                    slot as u32
                };
                let ep0 = rd64(ram, input + 64 + 8);
                let s = &mut self.slots[slot];
                s.port = port as u8;
                s.eps = [Ep::default(); 32];
                s.eps[1] = Ep { state: EP_RUNNING, deq: ep0 & !0xf, ccs: ep0 & 1 != 0, ..Ep::default() };
                self.set_slot_state(ram, slot, if bsr { 1 } else { 2 }, addr);
                self.set_ep_ctx(ram, slot, 1);
                CC_SUCCESS
            }
            12 => { // Configure Endpoint
                let out = self.ctx(ram, slot);
                let addr = rd32(ram, out + 12) & 0xff;
                if t.control & (1 << 9) != 0 {
                    for d in 2..32 { if self.slots[slot].eps[d].state != EP_DISABLED { self.slots[slot].eps[d].state = EP_DISABLED; self.set_ep_ctx(ram, slot, d); } }
                    self.set_slot_state(ram, slot, 2, addr);
                    return (CC_SUCCESS, slot);
                }
                let input = t.param & ADDRESS & !0xf;
                let (drop, add) = (rd32(ram, input), rd32(ram, input + 4));
                for d in 2..32 {
                    if drop >> d & 1 != 0 { self.slots[slot].eps[d] = Ep::default(); self.set_ep_ctx(ram, slot, d); }
                }
                for d in 2..32 {
                    if add >> d & 1 != 0 {
                        for k in (0..32).step_by(4) { let w = rd32(ram, input + 32 * (d as u64 + 1) + k); wr32(ram, out + 32 * d as u64 + k, w); }
                        let deq = rd64(ram, input + 32 * (d as u64 + 1) + 8);
                        let (info, info2) = (rd32(ram, input + 32 * (d as u64 + 1)), rd32(ram, input + 32 * (d as u64 + 1) + 4));
                        // EP Type 1 and 5: isochronous OUT and IN; Interval: 125 us << n
                        let isoch = matches!((info2 >> 3) & 7, 1 | 5);
                        let period = 125_000u64 << ((info >> 16) & 0xff).min(15);
                        self.slots[slot].eps[d] = Ep { state: EP_RUNNING, deq: deq & !0xf, ccs: deq & 1 != 0, isoch, period, ..Ep::default() };
                        self.set_ep_ctx(ram, slot, d);
                    }
                }
                if add & 1 != 0 { for k in (0..12).step_by(4) { let w = rd32(ram, input + 32 + k); wr32(ram, out + k, w); } }
                self.set_slot_state(ram, slot, 3, addr);
                CC_SUCCESS
            }
            13 => { // Evaluate Context
                let input = t.param & ADDRESS & !0xf;
                let out = self.ctx(ram, slot);
                let add = rd32(ram, input + 4);
                if add & 1 != 0 { for k in (0..12).step_by(4) { let w = rd32(ram, input + 32 + k); wr32(ram, out + k, w); } }
                if add & 2 != 0 {
                    let mps = rd32(ram, input + 64 + 4) & 0xffff_0000;
                    wr32(ram, out + 32 + 4, (rd32(ram, out + 32 + 4) & 0xffff) | mps);
                }
                CC_SUCCESS
            }
            14 | 15 => { // Reset Endpoint, Stop Endpoint
                if dci == 0 || dci > 31 { return (CC_TRB_ERROR, slot); }
                let ep = &mut self.slots[slot].eps[dci];
                if ep.state != EP_DISABLED { ep.state = EP_STOPPED; }
                ep.active = false;
                self.set_ep_ctx(ram, slot, dci);
                CC_SUCCESS
            }
            16 => { // Set TR Dequeue Pointer
                if dci == 0 || dci > 31 { return (CC_TRB_ERROR, slot); }
                let ep = &mut self.slots[slot].eps[dci];
                ep.deq = t.param & ADDRESS & !0xf;
                ep.ccs = t.param & 1 != 0;
                ep.active = false;
                if ep.state == EP_DISABLED { ep.state = EP_STOPPED; }
                self.set_ep_ctx(ram, slot, dci);
                CC_SUCCESS
            }
            17 => { // Reset Device
                for d in 2..32 { if self.slots[slot].eps[d].state != EP_DISABLED { self.slots[slot].eps[d] = Ep::default(); self.set_ep_ctx(ram, slot, d); } }
                self.set_slot_state(ram, slot, 1, 0);
                CC_SUCCESS
            }
            19..=23 => CC_SUCCESS, // No Op, bandwidth and latency bookkeeping
            _ => CC_TRB_ERROR,
        };
        (cc, slot)
    }

    // ------------------------------------------------------------------ transfers

    /// Run the transfer descriptors queued on endpoint `dci` of `slot` until the ring is empty
    /// or the device NAKs.
    fn run_ep(&mut self, slot: usize, dci: usize, ram: &mut Dma, devs: &mut Devices) {
        if slot > MAX_SLOTS || !(1..32).contains(&dci) || !self.slots[slot].enabled { return; }
        let port = self.slots[slot].port as usize;
        let Some(id) = port.checked_sub(1).and_then(|k| self.ports.get(k)).and_then(|p| p.dev) else { return };
        let now = self.now;
        let ep = &mut self.slots[slot].eps[dci];
        if ep.isoch {
            // its TDs wait for their time (tick); a stream that had stopped starts now
            if ep.state == EP_RUNNING && !ep.active { ep.active = true; ep.next = ep.next.max(now); }
            return;
        }
        let mut ran_out = false;
        for _ in 0..4096 {
            if self.slots[slot].eps[dci].state != EP_RUNNING { break; }
            let Some(dev) = devs.get(id) else { return };
            match self.run_td(slot, dci, ram, dev) {
                Some(true) => { ran_out |= dci.is_multiple_of(2); }
                _ => break,
            }
        }
        // data sent to the device may be what a waiting IN transfer needs
        if ran_out {
            for d in (3..32).step_by(2) {
                if self.slots[slot].eps[d].state == EP_RUNNING { self.run_ep(slot, d, ram, devs); }
            }
        }
    }

    /// One TD: Some(true) done, Some(false) waiting (NAK), None = nothing queued.
    fn run_td(&mut self, slot: usize, dci: usize, ram: &mut Dma, dev: &mut dyn UsbDevice) -> Option<bool> {
        let ep = self.slots[slot].eps[dci];
        // collect the TD's TRBs, following links
        let (mut a, mut ccs) = (ep.deq, ep.ccs);
        let mut td: Vec<Trb> = Vec::new();
        for _ in 0..1024 {
            let t = Trb::read(ram, a);
            if t.cycle() != ccs { return None; }
            if t.ty() == T_LINK {
                a = t.param & ADDRESS & !0xf;
                if t.control & 2 != 0 { ccs = !ccs; }
                continue;
            }
            td.push(t);
            a = a.wrapping_add(16);
            // a control transfer runs from its Setup to its Status stage (no chain bits between)
            let control = dci == 1 && td[0].ty() == T_SETUP;
            if if control { t.ty() == T_STATUS } else { !t.chain() } { break; }
        }
        if td.is_empty() { return None; }
        if self.trace { eprintln!("[xhci] slot {slot} dci {dci} TD {:?}", td.iter().map(|t| (t.ty(), t.len(), t.param)).collect::<Vec<_>>()); }
        self.transfers += 1;
        let ev = |x: &mut Self, ram: &mut Dma, t: &Trb, cc: u32, residue: usize| {
            let ed = if t.ty() == T_EVENT_DATA { 1 << 2 } else { 0 };
            let param = t.addr;
            x.post(ram, if ed != 0 { t.param } else { param }, cc << 24 | residue as u32 & 0xff_ffff, T_TRANSFER_EV << 10 | (dci as u32) << 16 | (slot as u32) << 24 | ed);
        };
        let buf_len = |t: &Trb| if matches!(t.ty(), T_NORMAL | T_DATA | T_ISOCH) { t.len() } else { 0 };
        let gather = |ram: &Dma, ts: &[Trb]| -> Vec<u8> {
            let mut d = Vec::new();
            for t in ts.iter().filter(|t| matches!(t.ty(), T_NORMAL | T_DATA | T_ISOCH)) {
                if t.idt() { d.extend_from_slice(&t.param.to_le_bytes()[..t.len().min(8)]); }
                else { d.extend_from_slice(ram.get(t.param, t.len()).unwrap_or(&vec![0; t.len()])); }
            }
            d
        };
        // completion events for data moved into/out of the TD's buffers; returns where it stopped
        let complete = |x: &mut Self, ram: &mut Dma, ts: &[Trb], mut moved: usize| -> usize {
            for (i, t) in ts.iter().enumerate() {
                let n = buf_len(t);
                let got = moved.min(n);
                moved -= got;
                if got < n && (t.isp() || t.ioc()) { ev(x, ram, t, CC_SHORT, n - got); return i + 1; }
                if t.ioc() { ev(x, ram, t, CC_SUCCESS, 0); }
            }
            ts.len()
        };
        if dci == 1 {
            // control: Setup, optional Data (+ Normal), Status
            let setup = td.iter().position(|t| t.ty() == T_SETUP);
            let Some(si) = setup else {
                // stray TRBs: complete them
                complete(self, ram, &td, 0);
                self.advance(slot, dci, a, ccs);
                return Some(true);
            };
            let st = td[si];
            if !dev.setup(st.param.to_le_bytes()) {
                ev(self, ram, &st, CC_STALL, 0);
                self.slots[slot].eps[dci].state = EP_HALTED;
                self.advance(slot, dci, a, ccs);
                self.set_ep_ctx(ram, slot, dci);
                return Some(true);
            }
            if st.ioc() { ev(self, ram, &st, CC_SUCCESS, 0); }
            let rest = &td[si + 1..];
            let status_at = rest.iter().position(|t| t.ty() == T_STATUS).unwrap_or(rest.len());
            let (data, status) = rest.split_at(status_at);
            let data_in = data.first().is_some_and(|t| t.control & (1 << 16) != 0);
            if !data.is_empty() {
                if data_in {
                    let want: usize = data.iter().map(buf_len).sum();
                    let got = dev.control_in(want);
                    self.scatter(ram, data, &got);
                    complete(self, ram, data, got.len());
                } else {
                    let d = gather(ram, data);
                    dev.control_out(&d);
                    complete(self, ram, data, d.len());
                }
            }
            if !data_in { dev.control_in(0); } // the IN status stage of an OUT or no-data request
            for t in status { if t.ioc() { ev(self, ram, t, CC_SUCCESS, 0); } }
        } else if !td.iter().any(|t| matches!(t.ty(), T_NORMAL | T_DATA | T_ISOCH)) {
            // no buffers (No-op TRBs: a cancelled transfer): nothing reaches the device
            complete(self, ram, &td, 0);
        } else {
            let epn = (dci / 2) as u8;
            if dci % 2 == 1 {
                let want: usize = td.iter().map(buf_len).sum();
                // (an isochronous IN TD is never NAKed: nothing to send is a zero-length packet)
                let got = match dev.data_in(epn, want) {
                    Some(got) => got,
                    None if ep.isoch => Vec::new(),
                    None => { self.transfers -= 1; return Some(false) }
                };
                self.scatter(ram, &td, &got);
                complete(self, ram, &td, got.len());
            } else {
                let d = gather(ram, &td);
                dev.data_out(epn, &d);
                complete(self, ram, &td, d.len());
            }
        }
        self.advance(slot, dci, a, ccs);
        Some(true)
    }

    fn advance(&mut self, slot: usize, dci: usize, a: u64, ccs: bool) {
        let ep = &mut self.slots[slot].eps[dci];
        ep.deq = a;
        ep.ccs = ccs;
    }

    fn scatter(&self, ram: &mut Dma, ts: &[Trb], mut data: &[u8]) {
        for t in ts.iter().filter(|t| matches!(t.ty(), T_NORMAL | T_DATA | T_ISOCH)) {
            let n = t.len().min(data.len());
            if let Some(b) = ram.slice(t.param, n) { b.copy_from_slice(&data[..n]); }
            data = &data[n..];
        }
    }

    // ------------------------------------------------------------------ time

    /// Retry waiting IN transfers of devices that have input, once per millisecond.
    pub fn tick(&mut self, now: u64, ram: &mut Dma, devs: &mut Devices) {
        self.now = now;
        devs.set_time(now);
        if self.usbcmd & CMD_RS != 0 { self.run_isoch(ram, devs); }
        if now < self.next_poll { return; }
        self.next_poll = now + 1_000_000;
        if self.usbcmd & CMD_RS == 0 { return; }
        for s in 1..=MAX_SLOTS {
            if !self.slots[s].enabled { continue; }
            let port = self.slots[s].port as usize;
            let Some(id) = port.checked_sub(1).and_then(|k| self.ports.get(k)).and_then(|p| p.dev) else { continue };
            if !devs.get(id).is_some_and(|d| d.has_input()) { continue; }
            for d in (3..32).step_by(2) {
                if self.slots[s].eps[d].state == EP_RUNNING { self.run_ep(s, d, ram, devs); }
            }
        }
    }

    /// Run the isochronous TDs that are due: one per endpoint per service interval. A guest that
    /// fell behind gets its late TDs at once, up to 64 intervals; beyond that the schedule restarts.
    fn run_isoch(&mut self, ram: &mut Dma, devs: &mut Devices) {
        for s in 1..=MAX_SLOTS {
            if !self.slots[s].enabled { continue; }
            let port = self.slots[s].port as usize;
            let Some(id) = port.checked_sub(1).and_then(|k| self.ports.get(k)).and_then(|p| p.dev) else { continue };
            for d in 2..32 {
                for _ in 0..64 {
                    let ep = self.slots[s].eps[d];
                    if !(ep.isoch && ep.active && ep.state == EP_RUNNING && ep.next <= self.now) { break; }
                    let Some(dev) = devs.get(id) else { break };
                    match self.run_td(s, d, ram, dev) {
                        Some(_) => self.slots[s].eps[d].next += ep.period,
                        None => { self.slots[s].eps[d].active = false; break; }
                    }
                }
                let now = self.now;
                let ep = &mut self.slots[s].eps[d];
                if ep.isoch && ep.active && ep.next <= now { ep.next = now + ep.period; }
            }
        }
    }

    /// The next poll, while an attached device has input waiting or an isochronous TD is due.
    pub fn next_event(&self, devs: &Devices) -> Option<u64> {
        let running = self.usbcmd & CMD_RS != 0;
        let waiting = running && self.ports.iter().any(|p| p.dev.is_some_and(|d| devs.has_input(d)));
        let isoch = self.slots.iter().filter(|s| running && s.enabled).flat_map(|s| s.eps.iter())
            .filter(|e| e.isoch && e.active && e.state == EP_RUNNING).map(|e| e.next).min();
        // a pending interrupt held back by moderation
        let held = (self.usbcmd & CMD_INTE != 0 && self.iman & 3 == 3 && self.now < self.irq_hold).then_some(self.irq_hold);
        [waiting.then_some(self.next_poll), held, isoch].into_iter().flatten().min()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const RING: u64 = 0x1000;
    const EVENTS: u64 = 0x2000;
    const ERST: u64 = 0x3000;

    fn running(ram: &mut [u8], devs: &mut Devices) -> Xhci {
        let mut x = Xhci::new(Layout { usb3: 0..1, usb2: 1..2 });
        x.trace = false;
        let mut d = Dma { ram, base: 0 };
        wr64(&mut d, ERST, EVENTS);
        wr32(&mut d, ERST + 8, 16);
        for (o, v) in [(0x58, 8), (0x38, RING as u32 | 1), (0x3c, 0), (0x628, 1), (0x638, EVENTS as u32), (0x630, ERST as u32), (0x634, 0), (0x620, 2), (0x20, 5)] {
            x.write(o, v, &mut d, devs);
        }
        x
    }

    /// Queue Enable Slot commands from index `from`.
    fn commands(ram: &mut [u8], from: u64, n: u64) {
        for k in from..from + n {
            wr32(&mut Dma { ram, base: 0 }, RING + 16 * k + 12, 9 << 10 | 1);
        }
    }

    #[test]
    fn a_restore_is_refused() {
        let (mut ram, mut devs) = (vec![0; 0x4000], Devices::default());
        let mut x = running(&mut ram, &mut devs);
        let mut d = Dma { ram: &mut ram, base: 0 };
        x.write(0x20, 0x204, &mut d, &mut devs);
        assert_eq!((x.read(0x20) & 0x300, x.read(0x24) & STS_SRE), (0, STS_SRE));
        x.write(0x24, STS_SRE, &mut d, &mut devs);
        assert_eq!(x.read(0x24) & STS_SRE, 0);
    }

    #[test]
    fn a_stopped_command_ring_goes_on_where_it_was() {
        let (mut ram, mut devs) = (vec![0; 0x4000], Devices::default());
        let mut x = running(&mut ram, &mut devs);
        commands(&mut ram, 0, 2);
        let mut d = Dma { ram: &mut ram, base: 0 };
        x.write(DB, 0, &mut d, &mut devs);
        x.write(0x38, 4, &mut d, &mut devs); // abort
        x.write(0x3c, 0, &mut d, &mut devs);
        x.write(DB, 0, &mut d, &mut devs);
        assert_eq!(x.slots.iter().filter(|s| s.enabled).count(), 2);
    }

    #[test]
    fn events_wait_for_room_in_the_ring() {
        let (mut ram, mut devs) = (vec![0; 0x4000], Devices::default());
        let mut x = running(&mut ram, &mut devs);
        commands(&mut ram, 0, 20);
        let mut d = Dma { ram: &mut ram, base: 0 };
        x.write(DB, 0, &mut d, &mut devs);
        let event = |d: &Dma, k: u64| (rd64(d, EVENTS + 16 * k), rd32(d, EVENTS + 16 * k + 12) & 1);
        assert_eq!(event(&d, 14), (RING + 16 * 14, 1));
        assert_eq!(event(&d, 15), (0, 0));
        x.write(0x638, EVENTS as u32 + 16 * 15 | 8, &mut d, &mut devs);
        x.write(0x63c, 0, &mut d, &mut devs);
        assert_eq!((event(&d, 15), event(&d, 0), event(&d, 3)), ((RING + 16 * 15, 1), (RING + 16 * 16, 0), (RING + 16 * 19, 0)));
    }

    #[test]
    fn an_unserviced_interrupt_leaves_the_cpu_time() {
        let (mut ram, mut devs) = (vec![0; 0x4000], Devices::default());
        let mut x = running(&mut ram, &mut devs);
        commands(&mut ram, 0, 1);
        let mut d = Dma { ram: &mut ram, base: 0 };
        x.write(0x624, 0, &mut d, &mut devs);
        x.write(DB, 0, &mut d, &mut devs);
        assert!(x.irq());
        x.write(0x620, 3, &mut d, &mut devs);
        x.write(0x638, EVENTS as u32 | 8, &mut d, &mut devs); // nothing consumed
        assert!(!x.irq() && x.next_event(&devs) == Some(MIN_HOLD_NS));
        x.tick(MIN_HOLD_NS, &mut d, &mut devs);
        assert!(x.irq());
    }
    /// An isochronous OUT endpoint (a USB sound card's speaker): one TD a service interval of
    /// virtual time from the doorbell on, a transfer event each, and a ring that runs dry stops
    /// the schedule until the next doorbell.
    #[test]
    fn isochronous_tds_run_one_per_interval() {
        let (mut ram, mut devs) = (vec![0u8; 0x10000], Devices { audio: Some(emu_core::usb::Audio::new()), ..Default::default() });
        let mut x = Xhci::new(Layout { usb3: 0..1, usb2: 1..2 });
        x.trace = false;
        x.attach(&[Some(DevId::Audio)]);
        let (dcbaa, out, input, ring, buf) = (0x4000u64, 0x5000u64, 0x6000u64, 0x7000u64, 0x8000u64);
        let mut d = Dma { ram: &mut ram, base: 0 };
        wr64(&mut d, ERST, EVENTS);
        wr32(&mut d, ERST + 8, 64);
        wr64(&mut d, dcbaa + 8, out);
        for (o, v) in [(0x58, 8), (0x50, dcbaa as u32), (0x54, 0), (0x38, RING as u32 | 1), (0x3c, 0), (0x628, 1), (0x638, EVENTS as u32), (0x630, ERST as u32), (0x634, 0), (0x620, 2), (0x20, 5)] {
            x.write(o, v, &mut d, &mut devs);
        }
        x.write(PORTS + 0x10, PS_PR | PS_PP, &mut d, &mut devs);
        // Enable Slot, Address Device (root port 2), Configure Endpoint (EP 1 OUT, isochronous, 1 ms)
        wr32(&mut d, input + 4, 3);
        wr32(&mut d, input + 32 + 4, 2 << 16);
        wr64(&mut d, input + 64 + 8, 0x7800 | 1);
        let cmd = |d: &mut Dma, k: u64, param: u64, control: u32| { wr64(d, RING + 16 * k, param); wr32(d, RING + 16 * k + 12, control | 1); };
        cmd(&mut d, 0, 0, 9 << 10);
        cmd(&mut d, 1, input, 11 << 10 | 1 << 24);
        x.write(DB, 0, &mut d, &mut devs);
        wr32(&mut d, input + 4, 1 << 2);
        wr32(&mut d, input + 96, 3 << 16);
        wr32(&mut d, input + 96 + 4, 1 << 3);
        wr64(&mut d, input + 96 + 8, ring | 1);
        cmd(&mut d, 2, input, 12 << 10 | 1 << 24);
        x.write(DB, 0, &mut d, &mut devs);
        let a = devs.audio.as_mut().unwrap();
        assert!(a.setup([0, 9, 1, 0, 0, 0, 0, 0]) && a.setup([1, 11, 1, 0, 1, 0, 0, 0]));
        a.keep_output = true;
        // three TDs of 48 stereo frames (1 ms at 48 kHz)
        for k in 0..3 {
            wr64(&mut d, ring + 16 * k, buf);
            wr32(&mut d, ring + 16 * k + 8, 192);
            wr32(&mut d, ring + 16 * k + 12, T_ISOCH << 10 | 1 << 5 | 1);
        }
        x.tick(10_000_000, &mut d, &mut devs);
        x.write(DB + 4, 2, &mut d, &mut devs);
        assert_eq!(devs.audio.as_ref().unwrap().played, 0, "a TD waits for its interval");
        x.tick(10_000_000, &mut d, &mut devs);
        assert_eq!(devs.audio.as_ref().unwrap().played, 48);
        assert_eq!(x.next_event(&devs), Some(11_000_000));
        x.tick(10_900_000, &mut d, &mut devs);
        assert_eq!(devs.audio.as_ref().unwrap().played, 48);
        x.tick(13_500_000, &mut d, &mut devs);
        assert_eq!(devs.audio.as_ref().unwrap().played, 144);
        // the ring ran dry: nothing is due until the next doorbell
        assert_eq!(x.next_event(&devs), None);
        let events = (0..64).map(|k| (rd64(&d, EVENTS + 16 * k), rd32(&d, EVENTS + 16 * k + 8), rd32(&d, EVENTS + 16 * k + 12)))
            .filter(|e| e.2 & 1 == 1 && (e.2 >> 10) & 0x3f == T_TRANSFER_EV).collect::<Vec<_>>();
        assert_eq!(events.iter().map(|e| (e.0, e.1 >> 24, (e.2 >> 16) & 0x1f)).collect::<Vec<_>>(), [(ring, 1, 2), (ring + 16, 1, 2), (ring + 32, 1, 2)]);
        assert_eq!(devs.audio.as_mut().unwrap().take_output().1.len(), 288);
    }
}
