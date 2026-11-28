//! The Orin's USB device-mode port (the USB-C connector a PC plugs into): the XUSB device
//! controller (XUDC) at `0x03550000`, its FPCI configuration window at `0x03558000`, and the
//! XUSB pad controller (`0x03520000`, AO block `0x03540000`) as far as Linux's `tegra-xudc` and
//! `phy-tegra-xusb` drivers program them. The other end of the cable is `usb_pc::Pc`.
//!
//! The XUDC has an xHCI-like programming model of its own: an event ring of two segments
//! (`ERSTXBA`, `ERDP`, `EREP`), 32 endpoint contexts in memory (`ECP`), a transfer ring per
//! endpoint and a doorbell. The model follows what the driver programs
//! (drivers/usb/gadget/udc/tegra-xudc.c):
//!
//! - Endpoint contexts live in guest memory and are the only copy: the controller reads one when
//!   it serves the endpoint and writes back its dequeue pointer and cycle state (`EP_RELOAD`
//!   therefore has nothing to reload).
//! - The PC's SETUP packets arrive as setup-packet events (TRB type 63) with a sequence number;
//!   the driver answers with data- and status-stage TRBs on endpoint 0, and the PC's control
//!   transfer ends with the status stage (or with a STALL: the driver halting endpoint 0).
//! - A transfer descriptor (TRBs up to one without CHAIN) is one USB transfer: on an IN endpoint
//!   the PC takes it whole when it polls the endpoint; on an OUT endpoint the PC's next message
//!   fills it, ending short (`SHORT_PACKET`, with the residue) when the message ends first.
//!   Transfer events go out for TRBs with IOC, and for short packets.
//! - The port: when the pad controller overrides VBUS on (the USB role switch went to
//!   "device") and the driver enables the pull-up (`CTRL.ENABLE`), the PC sees the device,
//!   resets the port and the controller reports a high-speed connection (`CSC`, `PRC`) through a
//!   port-status-change event. Without the pull-up or VBUS the link drops again.
//! - The pad controller is registers that keep what is written, with the tracking calibration
//!   always complete (`USB2_BIAS_PAD_CTL1.TRK_COMPLETED`); `USB2_VBUS_ID.VBUS_OVERRIDE` is the
//!   cable's VBUS as the controller sees it.
//!
//! Not modelled: SuperSpeed (the device tree gives the controller no USB3 PHY, so the link is
//! high speed), streams, isochronous endpoints, LPM and suspend, the controller's falcon, link
//! errors and power gating's loss of state (the driver reprograms everything when it ungates).
use crate::usb_pc::Pc;
use peripherals::dma::Dma;
use std::collections::VecDeque;

pub const BASE: u64 = 0x0355_0000;
pub const BASE_SIZE: u64 = 0x8000;
pub const FPCI: u64 = 0x0355_8000;
pub const FPCI_SIZE: u64 = 0x8000;
pub const SPI: u32 = 166;
pub const PADCTL: u64 = 0x0352_0000;
pub const PADCTL_SIZE: u64 = 0x2_0000;
pub const PADCTL_AO: u64 = 0x0354_0000;
pub const PADCTL_AO_SIZE: u64 = 0x1_0000;
pub const PADCTL_SPI: u32 = 167;

const DB: u32 = 0x004;
const ERSTSZ: u32 = 0x008;
const ERSTXBALO: u32 = 0x010;
const ERDPLO: u32 = 0x020;
const ERDPHI: u32 = 0x024;
const EREPLO: u32 = 0x028;
const EREPHI: u32 = 0x02c;
const CTRL: u32 = 0x030;
const CTRL_IE: u32 = 1 << 4;
const CTRL_ENABLE: u32 = 1 << 31;
const ST: u32 = 0x034;
const ST_RC: u32 = 1 << 0;
const ST_IP: u32 = 1 << 4;
const PORTSC: u32 = 0x03c;
const ECPLO: u32 = 0x040;
const ECPHI: u32 = 0x044;
const MFINDEX: u32 = 0x048;
const EP_HALT: u32 = 0x050;
const EP_PAUSE: u32 = 0x054;
const EP_RELOAD: u32 = 0x058;
const EP_STCHG: u32 = 0x05c;
const EP_THREAD_ACTIVE: u32 = 0x074;
const EP_STOPPED: u32 = 0x078;

const PORTSC_CCS: u32 = 1 << 0;
const PORTSC_PED: u32 = 1 << 1;
const PORTSC_PLS_SHIFT: u32 = 5;
const PORTSC_PLS_MASK: u32 = 0xf << PORTSC_PLS_SHIFT;
const PORTSC_PLS_U0: u32 = 0;
const PORTSC_PLS_RXDETECT: u32 = 5;
const PORTSC_PS_HS: u32 = 3 << 10;
const PORTSC_LWS: u32 = 1 << 16;
const PORTSC_CSC: u32 = 1 << 17;
const PORTSC_PRC: u32 = 1 << 21;
/// the change bits, write 1 to clear (CSC, WRC, PRC, PLC, CEC)
const PORTSC_CHANGE: u32 = (1 << 17) | (1 << 19) | (1 << 21) | (1 << 22) | (1 << 23);

const TRB_NORMAL: u32 = 1;
const TRB_DATA_STAGE: u32 = 3;
const TRB_STATUS_STAGE: u32 = 4;
const TRB_LINK: u32 = 6;
const TRB_TRANSFER_EVENT: u32 = 32;
const TRB_PORT_STATUS_EVENT: u32 = 34;
const TRB_SETUP_EVENT: u32 = 63;
const CC_SUCCESS: u32 = 1;
const CC_SHORT_PACKET: u32 = 13;

/// the PC's polling interval (ns)
const POLL_NS: u64 = 1_000_000;
const EP_STATE_RUNNING: u32 = 1;
const EP_CONTEXT_SIZE: u64 = 64;

/// `XUSB_PADCTL_USB2_BIAS_PAD_CTL1` and its tracking-done bit
const BIAS_PAD_CTL1: u32 = 0x288;
const TRK_COMPLETED: u32 = 1 << 31;
/// `XUSB_PADCTL_USB2_VBUS_ID` and its VBUS override
const USB2_VBUS_ID: u32 = 0x360;
const VBUS_OVERRIDE: u32 = 1 << 14;

/// The address is the controller's or its pad controller's.
pub fn contains(addr: u64) -> bool {
    (PADCTL..BASE + BASE_SIZE + FPCI_SIZE).contains(&addr)
}

#[derive(Clone, Copy, Debug)]
struct Trb {
    addr: u64,
    data: u64,
    status: u32,
    control: u32,
}

impl Trb {
    fn kind(&self) -> u32 {
        (self.control >> 10) & 0x3f
    }
    fn len(&self) -> usize {
        (self.status & 0x1_ffff) as usize
    }
    fn chain(&self) -> bool {
        self.control & (1 << 4) != 0
    }
    fn ioc(&self) -> bool {
        self.control & (1 << 5) != 0
    }
    fn cycle(&self) -> bool {
        self.control & 1 != 0
    }
}

/// The PC's control transfer in progress.
struct Control {
    setup: crate::usb_pc::Setup,
    /// what the board sent in the IN data stage, and how much of the OUT data stage it took
    data_in: Vec<u8>,
    taken: usize,
}

pub struct Xudc {
    regs: Vec<u32>,
    fpci: Vec<u32>,
    padctl: Vec<u32>,
    ao: Vec<u32>,
    /// the event ring's producer: segment, index and cycle state
    ev_seg: usize,
    ev_index: u32,
    ev_cycle: bool,
    /// events waiting for room in the ring
    backlog: VecDeque<[u32; 4]>,
    connected: bool,
    setup_seq: u16,
    control: Option<Control>,
    pub pc: Pc,
    /// a PC is plugged in
    pub attached: bool,
    now: u64,
    /// when the PC next looks at its timers and polls its endpoints
    next_poll: u64,
    /// transfer events written (for tests and reports)
    pub events: u64,
}

impl Xudc {
    pub fn new(pc: Pc) -> Self {
        let mut regs = vec![0; (BASE_SIZE / 4) as usize];
        regs[(PORTSC / 4) as usize] = PORTSC_PLS_RXDETECT << PORTSC_PLS_SHIFT;
        Xudc {
            regs,
            fpci: vec![0; (FPCI_SIZE / 4) as usize],
            padctl: vec![0; (PADCTL_SIZE / 4) as usize],
            ao: vec![0; (PADCTL_AO_SIZE / 4) as usize],
            ev_seg: 0,
            ev_index: 0,
            ev_cycle: true,
            backlog: VecDeque::new(),
            connected: false,
            setup_seq: 0,
            control: None,
            pc,
            attached: true,
            now: 0,
            next_poll: 0,
            events: 0,
        }
    }

    fn reg(&self, off: u32) -> u32 {
        self.regs[(off / 4) as usize]
    }

    fn set(&mut self, off: u32, value: u32) {
        self.regs[(off / 4) as usize] = value;
    }

    pub fn irq(&self) -> bool {
        self.reg(ST) & ST_IP != 0 && self.reg(CTRL) & CTRL_IE != 0
    }

    pub fn read(&mut self, addr: u64) -> u32 {
        let off = (addr & 0xffff_fffc) as u32;
        match addr {
            a if (BASE..BASE + BASE_SIZE).contains(&a) => match off - BASE as u32 {
                MFINDEX => ((self.now / 125_000) as u32 & 0x3fff) << 3 & 0x3ff8,
                EP_RELOAD | EP_THREAD_ACTIVE => 0,
                r => self.reg(r),
            },
            a if (FPCI..FPCI + FPCI_SIZE).contains(&a) => self.fpci[((a - FPCI) / 4) as usize],
            a if (PADCTL..PADCTL + PADCTL_SIZE).contains(&a) => {
                let r = (a - PADCTL) as u32;
                let v = self.padctl[(r / 4) as usize];
                if r == BIAS_PAD_CTL1 { v | TRK_COMPLETED } else { v }
            }
            a if (PADCTL_AO..PADCTL_AO + PADCTL_AO_SIZE).contains(&a) => self.ao[((a - PADCTL_AO) / 4) as usize],
            _ => 0,
        }
    }

    pub fn write(&mut self, addr: u64, value: u32, dma: &mut Dma) {
        match addr {
            a if (BASE..BASE + BASE_SIZE).contains(&a) => self.write_reg((a - BASE) as u32 & !3, value, dma),
            a if (FPCI..FPCI + FPCI_SIZE).contains(&a) => self.fpci[((a - FPCI) / 4) as usize] = value,
            a if (PADCTL..PADCTL + PADCTL_SIZE).contains(&a) => {
                let r = (a - PADCTL) as u32 & !3;
                self.padctl[(r / 4) as usize] = value;
                if r == USB2_VBUS_ID {
                    self.update_link(dma);
                }
            }
            a if (PADCTL_AO..PADCTL_AO + PADCTL_AO_SIZE).contains(&a) => self.ao[((a - PADCTL_AO) / 4) as usize] = value,
            _ => {}
        }
    }

    fn write_reg(&mut self, off: u32, value: u32, dma: &mut Dma) {
        match off {
            DB => {
                let ep = ((value >> 8) & 0xff) as usize;
                self.serve(ep, dma);
            }
            EREPLO | EREPHI => {
                self.set(off, value);
                self.set_producer();
            }
            ERDPLO => {
                self.set(off, value & !(1 << 3));
                self.flush_backlog(dma);
            }
            CTRL => {
                self.set(off, value);
                self.update_link(dma);
            }
            ST => self.set(off, self.reg(ST) & !(value & (ST_IP | ST_RC))),
            PORTSC => {
                let mut v = self.reg(PORTSC) & !(value & PORTSC_CHANGE);
                if value & PORTSC_LWS != 0 {
                    v = (v & !PORTSC_PLS_MASK) | (value & PORTSC_PLS_MASK);
                }
                self.set(PORTSC, v);
            }
            EP_HALT | EP_PAUSE => {
                let old = self.reg(off);
                self.set(off, value);
                self.set(EP_STCHG, self.reg(EP_STCHG) | (old ^ value));
                if off == EP_HALT && value & 1 != 0 && old & 1 == 0 && self.control.is_some() {
                    // the driver refused the request: the PC sees a STALL
                    self.control = None;
                    self.pc.control_done(None, self.now);
                }
                if value & !old == 0 {
                    self.serve_all(dma);
                }
            }
            EP_RELOAD => {}
            EP_STCHG | EP_STOPPED => self.set(off, self.reg(off) & !value),
            _ => self.set(off, value),
        }
    }

    /// `EREP` sets where the controller writes its next event (the driver does so when it
    /// initialises the ring): the address, the cycle state (`ECS`) and the segment (`SEGI`).
    fn set_producer(&mut self) {
        let lo = self.reg(EREPLO);
        let addr = (u64::from(self.reg(EREPHI)) << 32) | u64::from(lo & !0xf);
        self.ev_cycle = lo & 1 != 0;
        self.ev_seg = ((lo >> 1) & 1) as usize;
        self.ev_index = (addr.wrapping_sub(self.segment_base(self.ev_seg)) / 16) as u32;
        self.backlog.clear();
    }

    fn segment_base(&self, seg: usize) -> u64 {
        let lo = self.reg(ERSTXBALO + 8 * seg as u32);
        let hi = self.reg(ERSTXBALO + 4 + 8 * seg as u32);
        (u64::from(hi) << 32) | u64::from(lo & !0xf)
    }

    fn segment_size(&self, seg: usize) -> u32 {
        (self.reg(ERSTSZ) >> (16 * seg)) & 0xffff
    }

    fn post(&mut self, event: [u32; 4], dma: &mut Dma) {
        self.backlog.push_back(event);
        self.flush_backlog(dma);
    }

    /// Write queued events while the ring has room (one slot stays free, as on xHCI).
    fn flush_backlog(&mut self, dma: &mut Dma) {
        while let Some(&event) = self.backlog.front() {
            let size = self.segment_size(self.ev_seg);
            if size == 0 {
                return;
            }
            let at = self.segment_base(self.ev_seg) + u64::from(self.ev_index) * 16;
            let (mut seg, mut index, mut cycle) = (self.ev_seg, self.ev_index + 1, self.ev_cycle);
            if index >= size {
                index = 0;
                seg += 1;
                if seg == 2 || self.segment_size(seg) == 0 {
                    seg = 0;
                    cycle = !cycle;
                }
            }
            let next = self.segment_base(seg) + u64::from(index) * 16;
            let dequeue = (u64::from(self.reg(ERDPHI)) << 32) | u64::from(self.reg(ERDPLO) & !0xf);
            if next == dequeue {
                return; // full: wait for the driver to consume
            }
            let Some(slot) = dma.slice(at, 16) else { return };
            for (i, w) in event.iter().enumerate() {
                let w = if i == 3 { (w & !1) | u32::from(self.ev_cycle) } else { *w };
                slot[4 * i..4 * i + 4].copy_from_slice(&w.to_le_bytes());
            }
            self.backlog.pop_front();
            (self.ev_seg, self.ev_index, self.ev_cycle) = (seg, index, cycle);
            self.set(ST, self.reg(ST) | ST_IP);
            self.events += 1;
        }
    }

    /// The cable's state as the controller sees it: a PC, VBUS from the pad controller's
    /// override, the pull-up.
    fn update_link(&mut self, dma: &mut Dma) {
        let vbus = self.padctl[(USB2_VBUS_ID / 4) as usize] & VBUS_OVERRIDE != 0;
        let up = self.attached && vbus && self.reg(CTRL) & CTRL_ENABLE != 0;
        if up == self.connected {
            return;
        }
        self.connected = up;
        self.control = None;
        let portsc = self.reg(PORTSC);
        if up {
            // the PC saw the pull-up and reset the port: a high-speed link, reset done
            let v = (portsc & !PORTSC_PLS_MASK) | PORTSC_CCS | PORTSC_PED | PORTSC_PS_HS
                | (PORTSC_PLS_U0 << PORTSC_PLS_SHIFT) | PORTSC_CSC | PORTSC_PRC;
            self.set(PORTSC, v);
            self.set(ST, self.reg(ST) | ST_RC);
            self.pc.port_reset(self.now);
        } else {
            let v = (portsc & !(PORTSC_CCS | PORTSC_PED | PORTSC_PLS_MASK | (0xf << 10)))
                | (PORTSC_PLS_RXDETECT << PORTSC_PLS_SHIFT) | PORTSC_CSC;
            self.set(PORTSC, v);
            self.pc.disconnect();
        }
        self.post([0x0100_0000, 0, CC_SUCCESS << 24, TRB_PORT_STATUS_EVENT << 10], dma);
    }

    /// Plug the PC in or out.
    pub fn set_attached(&mut self, attached: bool, dma: &mut Dma) {
        self.attached = attached;
        self.update_link(dma);
    }

    pub fn connected(&self) -> bool {
        self.connected
    }

    /// Time-driven work: the PC's next request, its network, endpoints with work pending.
    pub fn tick(&mut self, now: u64, dma: &mut Dma) {
        self.now = now;
        if !self.connected {
            return;
        }
        if self.pc.has_out() {
            self.serve_all(dma);
        }
        // the rest once a millisecond (the SoC services its devices after every register write)
        if now < self.next_poll {
            return;
        }
        self.next_poll = now + POLL_NS;
        self.pc.tick(now);
        // the driver handled the reset (the change bits are clear) before the PC goes on
        if self.control.is_none() && self.reg(PORTSC) & PORTSC_CHANGE == 0 {
            if let Some(setup) = self.pc.next_setup(now) {
                self.setup_seq = self.setup_seq.wrapping_add(1);
                let p = setup.packet;
                let event = [
                    u32::from_le_bytes([p[0], p[1], p[2], p[3]]),
                    u32::from_le_bytes([p[4], p[5], p[6], p[7]]),
                    u32::from(self.setup_seq) | (CC_SUCCESS << 24),
                    TRB_SETUP_EVENT << 10,
                ];
                self.control = Some(Control { setup, data_in: Vec::new(), taken: 0 });
                self.post(event, dma);
            }
        }
        self.serve_all(dma);
    }

    pub fn next_event(&self) -> Option<u64> {
        if !self.connected {
            return None;
        }
        let poll = (self.pc.enumerating() || self.pc.configured).then_some(self.next_poll);
        poll.into_iter().chain(self.pc.next_event(self.now)).min()
    }

    fn serve_all(&mut self, dma: &mut Dma) {
        if !self.connected {
            return;
        }
        for ep in 2..32 {
            self.serve(ep, dma);
        }
    }

    fn context_addr(&self, ep: usize) -> u64 {
        ((u64::from(self.reg(ECPHI)) << 32) | u64::from(self.reg(ECPLO) & !0xf)) + ep as u64 * EP_CONTEXT_SIZE
    }

    fn read32(dma: &Dma, addr: u64) -> Option<u32> {
        Some(u32::from_le_bytes(dma.get(addr, 4)?.try_into().unwrap()))
    }

    fn write32(dma: &mut Dma, addr: u64, value: u32) {
        if let Some(s) = dma.slice(addr, 4) {
            s.copy_from_slice(&value.to_le_bytes());
        }
    }

    fn trb(dma: &Dma, addr: u64) -> Option<Trb> {
        let b = dma.get(addr, 16)?;
        let w = |i: usize| u32::from_le_bytes(b[4 * i..4 * i + 4].try_into().unwrap());
        Some(Trb { addr, data: u64::from(w(0)) | (u64::from(w(1)) << 32), status: w(2), control: w(3) })
    }

    /// The next transfer descriptor on an endpoint's ring, if the driver has queued all of it:
    /// its TRBs, and the dequeue pointer and cycle state after it.
    fn next_td(dma: &Dma, mut deq: u64, mut dcs: bool) -> Option<(Vec<Trb>, u64, bool)> {
        let mut trbs = Vec::new();
        for _ in 0..256 {
            let trb = Self::trb(dma, deq)?;
            if trb.cycle() != dcs {
                return None;
            }
            if trb.kind() == TRB_LINK {
                deq = trb.data & !0xf;
                if trb.control & 2 != 0 {
                    dcs = !dcs;
                }
                continue;
            }
            deq += 16;
            let last = !trb.chain();
            trbs.push(trb);
            if last {
                return Some((trbs, deq, dcs));
            }
        }
        None
    }

    fn event(trb: &Trb, code: u32, residue: usize, ep: usize) -> [u32; 4] {
        [trb.addr as u32, (trb.addr >> 32) as u32, (code << 24) | residue.min(0xff_ffff) as u32, (TRB_TRANSFER_EVENT << 10) | ((ep as u32) << 16)]
    }

    /// Serve endpoint `ep`: complete the transfer descriptors the PC has data for or polls.
    fn serve(&mut self, ep: usize, dma: &mut Dma) {
        if !self.connected || ep == 1 || ep >= 32 || (self.reg(EP_HALT) | self.reg(EP_PAUSE)) & (1 << ep) != 0 {
            return;
        }
        let ctx = self.context_addr(ep);
        for _ in 0..64 {
            let (Some(info0), Some(info1), Some(deq_lo), Some(deq_hi)) =
                (Self::read32(dma, ctx), Self::read32(dma, ctx + 4), Self::read32(dma, ctx + 8), Self::read32(dma, ctx + 12))
            else {
                return;
            };
            if info0 & 7 != EP_STATE_RUNNING {
                return;
            }
            let deq = (u64::from(deq_hi) << 32) | u64::from(deq_lo & !0xf);
            let Some((trbs, next, dcs)) = Self::next_td(dma, deq, deq_lo & 1 != 0) else { return };
            let done = if ep == 0 {
                self.serve_control(&trbs, dma)
            } else {
                let kind = (info1 >> 3) & 7;
                let address = (ep / 2) as u8 | if kind >= 5 { 0x80 } else { 0 };
                if kind >= 5 { self.serve_in(ep, address, &trbs, dma) } else { self.serve_out(ep, address, &trbs, dma) }
            };
            if !done {
                return;
            }
            Self::write32(dma, ctx + 8, (next as u32 & !0xf) | u32::from(dcs));
            Self::write32(dma, ctx + 12, (next >> 32) as u32);
            // EDTLA (the low 24 bits of the first reserved word): nothing is partly done
            if let Some(w) = Self::read32(dma, ctx + 20) {
                Self::write32(dma, ctx + 20, w & !0xff_ffff);
            }
        }
    }

    fn serve_in(&mut self, ep: usize, address: u8, trbs: &[Trb], dma: &mut Dma) -> bool {
        if !self.pc.wants_in(address) {
            return false; // the PC does not poll this endpoint (a NAK)
        }
        let mut data = Vec::new();
        for trb in trbs {
            if trb.len() > 0 {
                data.extend_from_slice(dma.get(trb.data, trb.len()).unwrap_or(&[]));
            }
        }
        for trb in trbs.iter().filter(|t| t.ioc()) {
            self.post(Self::event(trb, CC_SUCCESS, 0, ep), dma);
        }
        self.pc.in_data(address, &data, self.now);
        true
    }

    fn serve_out(&mut self, ep: usize, address: u8, trbs: &[Trb], dma: &mut Dma) -> bool {
        let Some(message) = self.pc.out_front(address) else { return false };
        let total: usize = trbs.iter().map(Trb::len).sum();
        let mut events = Vec::new();
        let mut offset = 0;
        for trb in trbs {
            let n = trb.len().min(message.len() - offset);
            if n > 0 {
                if let Some(buf) = dma.slice(trb.data, n) {
                    buf.copy_from_slice(&message[offset..offset + n]);
                }
            }
            offset += n;
            if n < trb.len() {
                // the message ended inside this TRB: a short packet ends the descriptor
                events.push(Self::event(trb, CC_SHORT_PACKET, total - offset, ep));
                break;
            }
            if trb.ioc() {
                events.push(Self::event(trb, CC_SUCCESS, 0, ep));
            }
        }
        if offset >= message.len() {
            self.pc.out_pop(address);
        } else {
            message.drain(..offset);
        }
        for e in events {
            self.post(e, dma);
        }
        true
    }

    /// Endpoint 0: the data and status stages of the PC's control transfer.
    fn serve_control(&mut self, trbs: &[Trb], dma: &mut Dma) -> bool {
        let Some(control) = self.control.as_mut() else { return false };
        let mut events = Vec::new();
        let mut finished = false;
        for trb in trbs {
            match trb.kind() {
                TRB_DATA_STAGE | TRB_NORMAL if trb.control & (1 << 16) != 0 => {
                    let room = control.setup.length().saturating_sub(control.data_in.len());
                    let n = trb.len().min(room);
                    control.data_in.extend_from_slice(dma.get(trb.data, n).unwrap_or(&[]));
                    if trb.ioc() {
                        events.push(Self::event(trb, CC_SUCCESS, 0, 0));
                    }
                }
                TRB_DATA_STAGE | TRB_NORMAL => {
                    let rest = &control.setup.out[control.taken.min(control.setup.out.len())..];
                    let n = trb.len().min(rest.len());
                    if let Some(buf) = dma.slice(trb.data, n) {
                        buf.copy_from_slice(&rest[..n]);
                    }
                    control.taken += n;
                    if n < trb.len() {
                        events.push(Self::event(trb, CC_SHORT_PACKET, trb.len() - n, 0));
                        break;
                    } else if trb.ioc() {
                        events.push(Self::event(trb, CC_SUCCESS, 0, 0));
                    }
                }
                TRB_STATUS_STAGE => {
                    events.push(Self::event(trb, CC_SUCCESS, 0, 0));
                    finished = true;
                }
                _ => events.push(Self::event(trb, CC_SUCCESS, 0, 0)),
            }
        }
        let result = finished.then(|| self.control.take().unwrap().data_in);
        for e in events {
            self.post(e, dma);
        }
        if let Some(data) = result {
            self.pc.control_done(Some(data), self.now);
        }
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::usb_pc::tests::l4t_config;

    const RAM: u64 = 0x8000_0000;
    const ERST0: u64 = RAM + 0x1000;
    const ERST1: u64 = RAM + 0x2000;
    const ECP: u64 = RAM + 0x3000;
    const RING0: u64 = RAM + 0x4000;
    const BUF: u64 = RAM + 0x8000;
    const SEG: u32 = 64;

    /// A small stand-in for Linux's tegra-xudc and a gadget behind it: rings, contexts, the
    /// event loop, descriptors.
    struct Driver {
        ram: Vec<u8>,
        x: Xudc,
        ev: (usize, u32, bool),
        enq: [u64; 32],
        pcs: [bool; 32],
    }

    impl Driver {
        fn new() -> Self {
            let mut d = Driver { ram: vec![0; 1 << 20], x: Xudc::new(Pc::new(false)), ev: (0, 0, true), enq: [0; 32], pcs: [true; 32] };
            d.w(ERSTSZ, SEG | (SEG << 16));
            d.w(ERSTXBALO, ERST0 as u32);
            d.w(ERSTXBALO + 4, 0);
            d.w(ERSTXBALO + 8, ERST1 as u32);
            d.w(ERSTXBALO + 12, 0);
            d.w(ERDPLO, ERST0 as u32);
            d.w(EREPLO, ERST0 as u32 | 1);
            d.w(ECPLO, ECP as u32);
            d.enable_ep(0, 4, 64);
            d.w(CTRL, CTRL_IE | (1 << 1));
            d
        }
        fn w(&mut self, off: u32, v: u32) {
            let mut ram = std::mem::take(&mut self.ram);
            self.x.write(BASE + u64::from(off), v, &mut Dma { ram: &mut ram, base: RAM });
            self.ram = ram;
        }
        fn r(&mut self, off: u32) -> u32 {
            self.x.read(BASE + u64::from(off))
        }
        fn put(&mut self, at: u64, words: &[u32]) {
            for (i, w) in words.iter().enumerate() {
                let o = (at - RAM) as usize + 4 * i;
                self.ram[o..o + 4].copy_from_slice(&w.to_le_bytes());
            }
        }
        fn get(&self, at: u64) -> u32 {
            let o = (at - RAM) as usize;
            u32::from_le_bytes(self.ram[o..o + 4].try_into().unwrap())
        }
        fn ring(ep: usize) -> u64 {
            RING0 + ep as u64 * 0x400
        }
        fn enable_ep(&mut self, ep: usize, kind: u32, maxp: u32) {
            let ring = Self::ring(ep);
            // a 64-TRB ring with a link back at the end
            self.put(ring + 63 * 16, &[ring as u32, 0, 0, (TRB_LINK << 10) | 2]);
            self.enq[ep] = ring;
            self.pcs[ep] = true;
            self.put(ECP + ep as u64 * 64, &[EP_STATE_RUNNING, (kind << 3) | (maxp << 16), ring as u32 | 1, 0]);
        }
        fn queue(&mut self, ep: usize, kind: u32, buf: u64, len: u32, extra: u32) {
            let at = self.enq[ep];
            self.put(at, &[buf as u32, 0, len, (kind << 10) | (1 << 5) | extra | u32::from(self.pcs[ep])]);
            self.enq[ep] += 16;
            self.w(DB, (ep as u32) << 8);
        }
        fn tick(&mut self, now: u64) {
            let mut ram = std::mem::take(&mut self.ram);
            self.x.tick(now, &mut Dma { ram: &mut ram, base: RAM });
            self.ram = ram;
        }
        /// The events since the last call.
        fn events(&mut self) -> Vec<[u32; 4]> {
            let mut out = Vec::new();
            loop {
                let base = if self.ev.0 == 0 { ERST0 } else { ERST1 };
                let at = base + u64::from(self.ev.1) * 16;
                let e = [self.get(at), self.get(at + 4), self.get(at + 8), self.get(at + 12)];
                if (e[3] & 1 != 0) != self.ev.2 {
                    break;
                }
                out.push(e);
                self.ev.1 += 1;
                if self.ev.1 == SEG {
                    self.ev.1 = 0;
                    self.ev.0 ^= 1;
                    if self.ev.0 == 0 {
                        self.ev.2 = !self.ev.2;
                    }
                }
                let next = if self.ev.0 == 0 { ERST0 } else { ERST1 } + u64::from(self.ev.1) * 16;
                self.w(ERDPLO, next as u32 | 8);
            }
            self.w(ST, ST_IP);
            out
        }
    }

    #[test]
    fn connects_when_vbus_and_pullup_are_on() {
        let mut d = Driver::new();
        assert!(!d.x.connected());
        let mut ram = std::mem::take(&mut d.ram);
        d.x.write(PADCTL + u64::from(USB2_VBUS_ID), VBUS_OVERRIDE, &mut Dma { ram: &mut ram, base: RAM });
        d.ram = ram;
        assert!(!d.x.connected(), "no pull-up yet");
        assert_ne!(d.x.read(PADCTL + 0x288) & TRK_COMPLETED, 0);
        let c = d.r(CTRL);
        d.w(CTRL, c | CTRL_ENABLE);
        assert!(d.x.connected());
        assert!(d.x.irq());
        let e = d.events();
        assert_eq!(e.len(), 1);
        assert_eq!((e[0][3] >> 10) & 0x3f, TRB_PORT_STATUS_EVENT);
        let portsc = d.r(PORTSC);
        assert_eq!(portsc & (PORTSC_CCS | PORTSC_CSC | PORTSC_PRC), PORTSC_CCS | PORTSC_CSC | PORTSC_PRC);
        assert_eq!((portsc >> 10) & 0xf, 3, "high speed");
        d.w(PORTSC, portsc & !PORTSC_CHANGE | PORTSC_CSC | PORTSC_PRC);
        assert_eq!(d.r(PORTSC) & PORTSC_CHANGE, 0);
        // pulling the pull-up drops the link
        let c = d.r(CTRL);
        d.w(CTRL, c & !CTRL_ENABLE);
        assert!(!d.x.connected());
        assert_eq!(d.r(PORTSC) & PORTSC_CCS, 0);
    }

    /// The PC enumerates a gadget with NVIDIA's configuration through the controller: setup
    /// events, data and status stages, a stalled request, bulk IN and OUT transfers.
    #[test]
    fn pc_enumerates_and_moves_data_through_the_rings() {
        let mut d = Driver::new();
        let mut ram = std::mem::take(&mut d.ram);
        d.x.write(PADCTL + u64::from(USB2_VBUS_ID), VBUS_OVERRIDE, &mut Dma { ram: &mut ram, base: RAM });
        d.ram = ram;
        let c = d.r(CTRL);
        d.w(CTRL, c | CTRL_ENABLE);
        d.events();
        d.w(PORTSC, PORTSC_CSC | PORTSC_PRC);
        let device = [18u8, 1, 0, 2, 0xef, 2, 1, 64, 0x55, 0x09, 0x20, 0x70, 2, 0, 1, 2, 3, 1];
        let config = l4t_config();
        let mut now = 0;
        let mut seen = Vec::new();
        let mut stalled = false;
        for _ in 0..200 {
            now += 1_000_000;
            d.tick(now);
            for e in d.events() {
                if (e[3] >> 10) & 0x3f != TRB_SETUP_EVENT {
                    continue;
                }
                let p: Vec<u8> = e[0].to_le_bytes().into_iter().chain(e[1].to_le_bytes()).collect();
                let len = usize::from(u16::from_le_bytes([p[6], p[7]]));
                seen.push(p[1]);
                if p[1] == 0x20 && !stalled {
                    // refuse SET_LINE_CODING once: the driver halts endpoint 0
                    stalled = true;
                    d.w(EP_HALT, 1);
                    assert_ne!(d.r(EP_STCHG) & 1, 0);
                    d.w(EP_STCHG, 1);
                    d.w(EP_HALT, 0);
                    continue;
                }
                let reply: Vec<u8> = match (p[1], p[3]) {
                    (6, 1) => device[..len.min(18)].to_vec(),
                    (6, 2) => config[..len.min(config.len())].to_vec(),
                    _ => Vec::new(),
                };
                if p[0] & 0x80 != 0 {
                    let at = BUF;
                    let o = (at - RAM) as usize;
                    d.ram[o..o + reply.len()].copy_from_slice(&reply);
                    d.queue(0, TRB_DATA_STAGE, at, reply.len() as u32, 1 << 16);
                    d.queue(0, TRB_STATUS_STAGE, 0, 0, 0);
                } else if len > 0 {
                    d.queue(0, TRB_DATA_STAGE, BUF + 0x100, len as u32, 0);
                    d.queue(0, TRB_STATUS_STAGE, 0, 0, 1 << 16);
                } else {
                    if p[1] == 9 {
                        // SET_CONFIGURATION: ACM's and NCM's bulk endpoints (index 2n+dir)
                        d.enable_ep(2 * 4 + 1, 6, 512); // 0x84 in
                        d.enable_ep(2 * 2, 2, 512); // 0x02 out
                        d.enable_ep(2 * 7 + 1, 6, 512); // 0x87 in
                        d.enable_ep(2 * 4, 2, 512); // 0x04 out
                    }
                    d.queue(0, TRB_STATUS_STAGE, 0, 0, 1 << 16);
                }
            }
        }
        assert_eq!(seen, [6, 5, 6, 6, 6, 9, 11, 0x20, 0x22]);
        assert!(d.x.pc.configured);
        // NCM's bulk OUT: the PC's DHCP DISCOVER fills a 2 KiB request short
        d.queue(8, TRB_NORMAL, BUF + 0x1000, 2048, 1 << 2);
        let e = d.events();
        assert_eq!(e.len(), 1);
        assert_eq!(e[0][2] >> 24, CC_SHORT_PACKET);
        let got = 2048 - (e[0][2] & 0xff_ffff) as usize;
        let o = (BUF + 0x1000 - RAM) as usize;
        let frames = crate::usb_pc::ntb_frames(&d.ram[o..o + got]);
        assert_eq!(frames.len(), 1);
        assert_eq!(&frames[0][12..14], &[8, 0]);
        // ACM's bulk IN: the board's bytes reach the PC
        let o = (BUF + 0x2000 - RAM) as usize;
        d.ram[o..o + 7].copy_from_slice(b"login: ");
        d.queue(9, TRB_NORMAL, BUF + 0x2000, 7, 0);
        assert_eq!(d.events()[0][2] >> 24, CC_SUCCESS);
        assert_eq!(d.x.pc.serial_rx, b"login: ");
        // ACM's bulk OUT: 600 bytes from the PC fill a 512-byte request, then 88 the next
        assert!(d.x.pc.serial_write(&[b'x'; 600]));
        d.queue(4, TRB_NORMAL, BUF + 0x3000, 512, 1 << 2);
        d.queue(4, TRB_NORMAL, BUF + 0x3200, 512, 1 << 2);
        let e = d.events();
        assert_eq!(e.len(), 2);
        assert_eq!((e[0][2] >> 24, e[0][2] & 0xffffff), (CC_SUCCESS, 0));
        assert_eq!((e[1][2] >> 24, e[1][2] & 0xffffff), (CC_SHORT_PACKET, 512 - 88));
        // the context's dequeue pointer moved past both
        assert_eq!(d.get(ECP + 4 * 64 + 8) & !0xf, Driver::ring(4) as u32 + 32);
    }
}
