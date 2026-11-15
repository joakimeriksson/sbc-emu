//! H3 USB host controllers: each host port has an EHCI (USB 2.0) and an OHCI (USB 1.1)
//! controller. The emulated devices are full speed, so the EHCI only has to probe and
//! keep its port idle (as with a full-speed device on real hardware, the companion OHCI
//! owns it). The OHCI runs its schedules in RAM once per 1 ms frame: the periodic
//! (interrupt) list from the HCCA, then the control and bulk lists, retiring transfer
//! descriptors to the done queue.
use emu_core::usb::{Speed, UsbDevice};

/// 24 MHz ticks per 1 ms USB frame.
const FRAME_TICKS: u64 = 24_000;

/// Little-endian access to guest RAM (physical addresses), as a bus master sees it.
pub struct Dma<'a> {
    pub mem: &'a mut [u8],
    pub base: u64,
}

impl Dma<'_> {
    fn range(&self, addr: u32, len: usize) -> Option<std::ops::Range<usize>> {
        let start = (addr as u64).checked_sub(self.base)? as usize;
        let end = start.checked_add(len)?;
        (end <= self.mem.len()).then_some(start..end)
    }
    pub fn read32(&self, addr: u32) -> u32 {
        self.range(addr, 4).map_or(0, |r| u32::from_le_bytes(self.mem[r].try_into().unwrap()))
    }
    pub fn write32(&mut self, addr: u32, value: u32) {
        if let Some(r) = self.range(addr, 4) {
            self.mem[r].copy_from_slice(&value.to_le_bytes());
        }
    }
    fn read(&self, addr: u32, out: &mut [u8]) {
        match self.range(addr, out.len()) {
            Some(r) => out.copy_from_slice(&self.mem[r]),
            None => out.fill(0),
        }
    }
    fn write(&mut self, addr: u32, data: &[u8]) {
        if let Some(r) = self.range(addr, data.len()) {
            self.mem[r].copy_from_slice(data);
        }
    }
}

/// A minimal EHCI: capability registers for one port, run/stop, reset and the schedule
/// status bits following their enables. No device ever connects to it.
pub struct Ehci {
    regs: [u32; 0x60 / 4],
}

impl Default for Ehci {
    fn default() -> Self {
        let mut e = Ehci { regs: [0; 0x60 / 4] };
        e.reset();
        e
    }
}

const EHCI_USBCMD: usize = 0x10 / 4;
const EHCI_USBSTS: usize = 0x14 / 4;
const EHCI_USBINTR: usize = 0x18 / 4;
const EHCI_PORTSC: usize = 0x54 / 4;

impl Ehci {
    pub const SIZE: u64 = 0x60;

    fn reset(&mut self) {
        self.regs = [0; 0x60 / 4];
        self.regs[0] = 0x0100_0010; // HCIVERSION 1.00, CAPLENGTH 0x10
        self.regs[1] = 1 << 12 | 1 << 8 | 1 << 4 | 1; // one companion, one port with power control
        self.regs[EHCI_USBCMD] = 0x0008_0000; // interrupt threshold 8 microframes
        self.regs[EHCI_USBSTS] = 1 << 12; // halted
        self.regs[EHCI_PORTSC] = 1 << 13; // owned by the companion, nothing connected
    }

    pub fn read(&self, offset: u64) -> u32 {
        let index = (offset / 4) as usize;
        let value = self.regs.get(index).copied().unwrap_or(0);
        if index == EHCI_USBSTS {
            let cmd = self.regs[EHCI_USBCMD];
            // halted unless running; schedule status follows the schedule enables
            let mut status = value & 0x3f;
            if cmd & 1 == 0 {
                status |= 1 << 12;
            }
            status |= (cmd >> 4 & 1) << 14 | (cmd >> 5 & 1) << 15;
            return status;
        }
        value
    }

    pub fn write(&mut self, offset: u64, value: u32) {
        let index = (offset / 4) as usize;
        match index {
            0..=3 => {}
            EHCI_USBCMD if value & 2 != 0 => self.reset(),
            EHCI_USBSTS => self.regs[index] &= !(value & 0x3f),
            EHCI_PORTSC => {
                // keep the port with the companion and empty; power and owner are writable
                let keep = value & (1 << 12 | 1 << 13);
                self.regs[index] = keep | 1 << 13;
            }
            _ if index < self.regs.len() => self.regs[index] = value,
            _ => {}
        }
    }

    pub fn irq(&self) -> bool {
        self.regs[EHCI_USBSTS] & self.regs[EHCI_USBINTR] & 0x3f != 0
    }
}

// OHCI registers (word index).
const HC_REVISION: usize = 0;
const HC_CONTROL: usize = 1;
const HC_COMMAND_STATUS: usize = 2;
const HC_INTERRUPT_STATUS: usize = 3;
const HC_INTERRUPT_ENABLE: usize = 4;
const HC_INTERRUPT_DISABLE: usize = 5;
const HC_HCCA: usize = 6;
const HC_CONTROL_HEAD: usize = 8;
const HC_CONTROL_CURRENT: usize = 9;
const HC_BULK_HEAD: usize = 10;
const HC_BULK_CURRENT: usize = 11;
const HC_DONE_HEAD: usize = 12;
const HC_FM_INTERVAL: usize = 13;
const HC_FM_REMAINING: usize = 14;
const HC_FM_NUMBER: usize = 15;
const HC_LS_THRESHOLD: usize = 17;
const HC_RH_DESCRIPTOR_A: usize = 18;
const HC_RH_STATUS: usize = 20;
const HC_RH_PORT_STATUS: usize = 21;
const OHCI_REGS: usize = 22;

// HcControl
const PLE: u32 = 1 << 2;
const CLE: u32 = 1 << 4;
const BLE: u32 = 1 << 5;
const HCFS_MASK: u32 = 3 << 6;
const HCFS_OPERATIONAL: u32 = 2 << 6;
const HCFS_SUSPEND: u32 = 3 << 6;
// HcCommandStatus
const HCR: u32 = 1 << 0;
const CLF: u32 = 1 << 1;
const BLF: u32 = 1 << 2;
// HcInterruptStatus / Enable
const WDH: u32 = 1 << 1;
const SF: u32 = 1 << 2;
const FNO: u32 = 1 << 5;
const RHSC: u32 = 1 << 6;
const MIE: u32 = 1 << 31;
// HcRhPortStatus
const CCS: u32 = 1 << 0;
const PES: u32 = 1 << 1;
const PSS: u32 = 1 << 2;
const PRS: u32 = 1 << 4;
const PPS: u32 = 1 << 8;
const LSDA: u32 = 1 << 9;
const CSC: u32 = 1 << 16;
const PSSC: u32 = 1 << 18;
const PRSC: u32 = 1 << 20;
const PORT_CHANGES: u32 = 0x1f << 16;

// Transfer descriptor condition codes.
const CC_NO_ERROR: u32 = 0;
const CC_STALL: u32 = 4;
const CC_DEVICE_NOT_RESPONDING: u32 = 5;
const CC_DATA_UNDERRUN: u32 = 9;

/// Largest transfer one general TD describes (two 4 KiB pages).
const TD_MAX: usize = 8192;

/// What a transfer descriptor did.
enum Outcome {
    /// Bytes transferred and the condition code.
    Done(usize, u32),
    /// The device has nothing yet: try again in a later frame.
    Nak,
}

/// An OHCI controller with one root-hub port.
pub struct Ohci {
    regs: [u32; OHCI_REGS],
    /// Tick of the next frame start while operational.
    next_frame: Option<u64>,
    /// Transfer descriptors retired since the done queue was last written to the HCCA.
    done_head: u32,
    /// A device is plugged into the port.
    attached: bool,
    /// The port was reset: the device returns to its default state (address 0).
    device_reset: bool,
}

impl Ohci {
    pub const SIZE: u64 = 0x400;

    /// A controller whose port has a device plugged in (`attached`).
    pub fn new(attached: bool) -> Self {
        let mut o = Ohci { regs: [0; OHCI_REGS], next_frame: None, done_head: 0, attached, device_reset: false };
        o.reset();
        o.regs[HC_RH_PORT_STATUS] = if attached { CCS | CSC } else { 0 };
        o
    }

    /// Hardware or software reset: registers to their defaults, the port keeps its device.
    fn reset(&mut self) {
        let port = self.regs[HC_RH_PORT_STATUS] & CCS;
        self.regs = [0; OHCI_REGS];
        self.regs[HC_REVISION] = 0x10;
        self.regs[HC_CONTROL] = HCFS_SUSPEND;
        self.regs[HC_FM_INTERVAL] = 0x2778_2edf;
        self.regs[HC_LS_THRESHOLD] = 0x628;
        // one port, no power switching (always powered), no over-current, 2 ms power-on
        self.regs[HC_RH_DESCRIPTOR_A] = 0x0100_1201;
        self.regs[HC_RH_PORT_STATUS] = port | if port != 0 { CSC } else { 0 };
        self.next_frame = None;
        self.done_head = 0;
    }

    /// The port's status bits for a device of `speed` (low-speed devices set LSDA).
    pub fn set_speed(&mut self, speed: Speed) {
        let _ = speed; // the emulated devices are full or high speed: never LSDA
        self.regs[HC_RH_PORT_STATUS] &= !LSDA;
    }

    pub fn read(&self, offset: u64) -> u32 {
        let index = (offset / 4) as usize;
        match index {
            HC_INTERRUPT_DISABLE => self.regs[HC_INTERRUPT_ENABLE],
            HC_FM_REMAINING => self.regs[HC_FM_INTERVAL] & 0x3fff,
            HC_DONE_HEAD => self.done_head,
            HC_RH_PORT_STATUS => self.regs[index] | PPS,
            _ if index < OHCI_REGS => self.regs[index],
            _ => 0,
        }
    }

    pub fn write(&mut self, offset: u64, value: u32, now: u64) {
        let index = (offset / 4) as usize;
        match index {
            HC_REVISION | HC_FM_REMAINING | HC_FM_NUMBER | HC_DONE_HEAD => {}
            HC_CONTROL => {
                self.regs[index] = value;
                if value & HCFS_MASK == HCFS_OPERATIONAL {
                    self.next_frame.get_or_insert(now + FRAME_TICKS);
                } else {
                    self.next_frame = None;
                }
            }
            HC_COMMAND_STATUS => {
                if value & HCR != 0 {
                    self.reset();
                } else {
                    self.regs[index] |= value & (CLF | BLF);
                }
            }
            HC_INTERRUPT_STATUS => self.regs[index] &= !value,
            HC_INTERRUPT_ENABLE => self.regs[index] |= value,
            HC_INTERRUPT_DISABLE => self.regs[HC_INTERRUPT_ENABLE] &= !value,
            HC_HCCA => self.regs[index] = value & !0xff,
            HC_RH_STATUS => {} // always powered, no over-current, no remote wakeup
            HC_RH_PORT_STATUS => self.port_write(value),
            _ if index < OHCI_REGS => self.regs[index] = value,
            _ => {}
        }
    }

    fn port_write(&mut self, value: u32) {
        let port = &mut self.regs[HC_RH_PORT_STATUS];
        *port &= !(value & PORT_CHANGES);
        let connected = *port & CCS != 0;
        if value & 1 != 0 {
            *port &= !PES; // ClearPortEnable
        }
        if value & PES != 0 && connected {
            *port |= PES; // SetPortEnable
        }
        if value & PSS != 0 && connected {
            *port |= PSS; // SetPortSuspend
        }
        if value & (1 << 3) != 0 && *port & PSS != 0 {
            *port = *port & !PSS | PSSC; // ClearSuspendStatus: resume
        }
        if value & PRS != 0 && connected {
            // SetPortReset: the reset completes at once and enables the port
            *port = *port & !PSS | PES | PRSC;
            self.device_reset = true;
        }
        if value & (1 << 9) != 0 {
            *port &= !(PES | PSS); // ClearPortPower (ports stay powered: NPS)
        }
        self.update_rhsc();
    }

    fn update_rhsc(&mut self) {
        if self.regs[HC_RH_PORT_STATUS] & PORT_CHANGES != 0 {
            self.regs[HC_INTERRUPT_STATUS] |= RHSC;
        }
    }

    pub fn irq(&self) -> bool {
        let enable = self.regs[HC_INTERRUPT_ENABLE];
        enable & MIE != 0 && self.regs[HC_INTERRUPT_STATUS] & enable & 0x7fff_ffff != 0
    }

    pub fn next_deadline(&self) -> Option<u64> {
        self.next_frame
    }

    /// Run the frames due by tick `now` with `device` on the port.
    pub fn update(&mut self, now: u64, dma: &mut Dma, mut device: Option<&mut (dyn UsbDevice + 'static)>) {
        if std::mem::take(&mut self.device_reset) {
            if let Some(d) = device.as_deref_mut() {
                d.bus_reset();
            }
        }
        while let Some(frame) = self.next_frame.filter(|&f| f <= now) {
            self.next_frame = Some(frame + FRAME_TICKS);
            self.frame(dma, device.as_deref_mut());
        }
    }

    fn frame(&mut self, dma: &mut Dma, mut device: Option<&mut (dyn UsbDevice + 'static)>) {
        let hcca = self.regs[HC_HCCA];
        let number = (self.regs[HC_FM_NUMBER] + 1) & 0xffff;
        if number & 0x7fff == 0 {
            self.regs[HC_INTERRUPT_STATUS] |= FNO;
        }
        self.regs[HC_FM_NUMBER] = number;
        if hcca != 0 {
            dma.write32(hcca + 0x80, number);
        }
        self.regs[HC_INTERRUPT_STATUS] |= SF;
        let port_ok = self.regs[HC_RH_PORT_STATUS] & (CCS | PES) == CCS | PES && self.attached;
        if !port_ok {
            device = None;
        }
        let control = self.regs[HC_CONTROL];
        if control & PLE != 0 && hcca != 0 {
            let head = dma.read32(hcca + 4 * (number & 31));
            self.run_list(dma, head, device.as_deref_mut(), true);
        }
        if control & CLE != 0 {
            let head = self.regs[HC_CONTROL_HEAD];
            if self.run_list(dma, head, device.as_deref_mut(), false) {
                self.regs[HC_COMMAND_STATUS] &= !CLF;
            }
            self.regs[HC_CONTROL_CURRENT] = 0;
        }
        if control & BLE != 0 {
            let head = self.regs[HC_BULK_HEAD];
            if self.run_list(dma, head, device.as_deref_mut(), false) {
                self.regs[HC_COMMAND_STATUS] &= !BLF;
            }
            self.regs[HC_BULK_CURRENT] = 0;
        }
        if self.done_head != 0 && self.regs[HC_INTERRUPT_STATUS] & WDH == 0 && hcca != 0 {
            dma.write32(hcca + 0x84, self.done_head);
            self.done_head = 0;
            self.regs[HC_INTERRUPT_STATUS] |= WDH;
        }
    }

    /// Process an endpoint descriptor list; true if every endpoint is idle (no TDs left).
    fn run_list(&mut self, dma: &mut Dma, mut ed: u32, mut device: Option<&mut (dyn UsbDevice + 'static)>, periodic: bool) -> bool {
        let mut idle = true;
        let mut guard = 0;
        while ed != 0 && guard < 256 {
            guard += 1;
            ed &= !0xf;
            let info = dma.read32(ed);
            let next = dma.read32(ed + 12);
            // skip, isochronous (not modelled) and halted endpoints
            if info & (1 << 14) == 0 && info & (1 << 15) == 0 {
                if !self.run_endpoint(dma, ed, info, device.as_deref_mut()) {
                    idle = false;
                }
            }
            if periodic && next == ed {
                break;
            }
            ed = next;
        }
        idle
    }

    /// Run the transfer descriptors queued on one endpoint; true if none is left.
    fn run_endpoint(&mut self, dma: &mut Dma, ed: u32, info: u32, mut device: Option<&mut (dyn UsbDevice + 'static)>) -> bool {
        let address = (info & 0x7f) as u8;
        let endpoint = (info >> 7 & 0xf) as u8;
        let ed_direction = info >> 11 & 3;
        let mps = ((info >> 16) & 0x7ff).max(1) as usize;
        let mut count = 0;
        loop {
            let tail = dma.read32(ed + 4) & !0xf;
            let head_word = dma.read32(ed + 8);
            let td = head_word & !0xf;
            if head_word & 1 != 0 {
                return true; // halted
            }
            if td == tail || td == 0 {
                return true;
            }
            count += 1;
            if count > 64 {
                return false;
            }
            let td_info = dma.read32(td);
            let cbp = dma.read32(td + 4);
            let next_td = dma.read32(td + 8) & !0xf;
            let be = dma.read32(td + 12);
            let pid = match ed_direction {
                1 => 1, // OUT
                2 => 2, // IN
                _ => td_info >> 19 & 3,
            };
            let len = if cbp == 0 { 0 } else { buffer_len(cbp, be) };
            let outcome = match device.as_deref_mut() {
                Some(dev) => {
                    if dev.address() == address {
                        transfer(dev, endpoint, pid, cbp, be, len, dma)
                    } else {
                        Outcome::Done(0, CC_DEVICE_NOT_RESPONDING)
                    }
                }
                None => Outcome::Done(0, CC_DEVICE_NOT_RESPONDING),
            };
            let (done, mut cc) = match outcome {
                Outcome::Nak => return false,
                Outcome::Done(done, cc) => (done, cc),
            };
            // data toggle: from the TD if its T bit 1 is set, else the endpoint's carry
            let toggle = if td_info & (1 << 25) != 0 { td_info >> 24 & 1 } else { head_word >> 1 & 1 };
            // only acknowledged packets advance the toggle
            let packets = if cc == CC_NO_ERROR { done.div_ceil(mps).max(1) as u32 } else { 0 };
            let toggle = (toggle + packets) & 1;
            let short = done < len;
            if cc == CC_NO_ERROR && short && td_info & (1 << 18) == 0 {
                cc = CC_DATA_UNDERRUN;
            }
            let new_cbp = if cc == CC_NO_ERROR && !short || len == 0 { 0 } else { advance(cbp, be, done) };
            let mut new_info = td_info & !(0xf << 28) & !(3 << 26) | cc << 28;
            new_info = new_info & !(3 << 24) | (2 | toggle) << 24;
            dma.write32(td, new_info);
            dma.write32(td + 4, new_cbp);
            dma.write32(td + 8, self.done_head);
            self.done_head = td;
            let halt = (cc != CC_NO_ERROR) as u32;
            dma.write32(ed + 8, next_td | toggle << 1 | halt);
            if halt != 0 {
                return true;
            }
        }
    }
}

/// Bytes a TD's buffer spans: from CBP to BE inclusive, possibly crossing one page.
fn buffer_len(cbp: u32, be: u32) -> usize {
    let len = if cbp & !0xfff == be & !0xfff {
        (be as i64 - cbp as i64 + 1).max(0) as usize
    } else {
        (0x1000 - (cbp & 0xfff)) as usize + (be & 0xfff) as usize + 1
    };
    len.min(TD_MAX)
}

/// The buffer address `n` bytes after `cbp` (the second page starts at BE's page).
fn advance(cbp: u32, be: u32, n: usize) -> u32 {
    let first = (0x1000 - (cbp & 0xfff)) as usize;
    if cbp & !0xfff == be & !0xfff || n < first {
        cbp + n as u32
    } else {
        (be & !0xfff) + (n - first) as u32
    }
}

/// Copy between a TD buffer and `data` (`to_ram`: device to memory).
fn buffer_copy(dma: &mut Dma, cbp: u32, be: u32, data: &mut [u8], to_ram: bool) {
    let first = if cbp & !0xfff == be & !0xfff { data.len() } else { ((0x1000 - (cbp & 0xfff)) as usize).min(data.len()) };
    let (a, b) = data.split_at_mut(first);
    if to_ram {
        dma.write(cbp, a);
        dma.write(be & !0xfff, b);
    } else {
        dma.read(cbp, a);
        dma.read(be & !0xfff, b);
    }
}

/// One TD's transfer with the device: SETUP, OUT or IN.
fn transfer(dev: &mut dyn UsbDevice, endpoint: u8, pid: u32, cbp: u32, be: u32, len: usize, dma: &mut Dma) -> Outcome {
    match pid {
        0 => {
            let mut setup = [0u8; 8];
            if len < 8 {
                return Outcome::Done(0, CC_STALL);
            }
            buffer_copy(dma, cbp, be, &mut setup, false);
            if dev.setup(setup) {
                Outcome::Done(8, CC_NO_ERROR)
            } else {
                Outcome::Done(0, CC_STALL)
            }
        }
        1 => {
            let mut data = vec![0u8; len];
            buffer_copy(dma, cbp, be, &mut data, false);
            if endpoint == 0 {
                // an empty OUT is the status stage of a control read
                if !data.is_empty() {
                    dev.control_out(&data);
                }
            } else {
                dev.data_out(endpoint, &data);
            }
            Outcome::Done(len, CC_NO_ERROR)
        }
        _ => {
            let data = if endpoint == 0 {
                Some(dev.control_in(len))
            } else {
                dev.data_in(endpoint, len)
            };
            match data {
                None => Outcome::Nak,
                Some(mut data) => {
                    data.truncate(len);
                    buffer_copy(dma, cbp, be, &mut data, true);
                    Outcome::Done(data.len(), CC_NO_ERROR)
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use emu_core::usb::Keyboard;

    const BASE: u64 = 0x4000_0000;
    const HCCA: u32 = 0x4000_1000;

    struct Rig {
        ohci: Ohci,
        mem: Vec<u8>,
        kbd: Keyboard,
        now: u64,
    }

    impl Rig {
        fn new() -> Self {
            let mut ohci = Ohci::new(true);
            ohci.write(HC_HCCA as u64 * 4, HCCA, 0);
            ohci.write(HC_INTERRUPT_ENABLE as u64 * 4, MIE | WDH | RHSC, 0);
            ohci.write(HC_CONTROL as u64 * 4, HCFS_OPERATIONAL | CLE | PLE, 0);
            Rig { ohci, mem: vec![0; 0x10000], kbd: Keyboard::default(), now: 0 }
        }
        fn w32(&mut self, addr: u32, v: u32) {
            Dma { mem: &mut self.mem, base: BASE }.write32(addr, v);
        }
        fn r32(&mut self, addr: u32) -> u32 {
            Dma { mem: &mut self.mem, base: BASE }.read32(addr)
        }
        fn frame(&mut self) {
            self.now += FRAME_TICKS;
            let mut dma = Dma { mem: &mut self.mem, base: BASE };
            self.ohci.update(self.now, &mut dma, Some(&mut self.kbd));
        }
        /// A TD at `td` -> `next` with `pid`, buffer `buf..buf+len`.
        fn td(&mut self, td: u32, pid: u32, buf: u32, len: u32, next: u32) {
            self.w32(td, 0xf << 28 | 1 << 18 | pid << 19 | 7 << 21);
            self.w32(td + 4, if len == 0 { 0 } else { buf });
            self.w32(td + 8, next);
            self.w32(td + 12, if len == 0 { 0 } else { buf + len - 1 });
        }
    }

    #[test]
    fn port_reset_enables_the_port_and_reports_the_change() {
        let mut r = Rig::new();
        let port = HC_RH_PORT_STATUS as u64 * 4;
        assert_eq!(r.ohci.read(port) & (CCS | CSC), CCS | CSC, "device present at power-on");
        r.ohci.write(port, CSC, 0);
        r.ohci.write(port, PRS, 0);
        assert_eq!(r.ohci.read(port) & (PES | PRSC), PES | PRSC);
        assert!(r.ohci.irq(), "root hub status change");
    }

    #[test]
    fn control_transfer_reads_the_device_descriptor_and_sets_the_address() {
        let mut r = Rig::new();
        r.ohci.write(HC_RH_PORT_STATUS as u64 * 4, PRS, 0);
        // GET_DESCRIPTOR(device, 18): SETUP, IN, OUT status
        let (ed, td0, td1, td2, tail) = (0x4000_2000, 0x4000_2100, 0x4000_2110, 0x4000_2120, 0x4000_2130);
        let (setup, data) = (0x4000_3000u32, 0x4000_3100u32);
        let at = (setup as u64 - BASE) as usize;
        r.mem[at..at + 8].copy_from_slice(&[0x80, 6, 0, 1, 0, 0, 18, 0]);
        r.td(td0, 0, setup, 8, td1);
        r.td(td1, 2, data, 18, td2);
        r.td(td2, 1, 0, 0, tail);
        r.w32(ed, 64 << 16); // address 0, endpoint 0, MPS 64
        r.w32(ed + 4, tail);
        r.w32(ed + 8, td0);
        r.ohci.write(HC_CONTROL_HEAD as u64 * 4, ed, 0);
        r.frame();
        let d = (data as u64 - BASE) as usize;
        assert_eq!(&r.mem[d..d + 2], &[18, 1], "device descriptor");
        assert_eq!(r.r32(td1) >> 28, CC_NO_ERROR);
        assert_eq!(r.r32(ed + 8) & !0xf, tail, "all TDs retired");
        assert_eq!(r.r32(HCCA + 0x84), td2, "done queue, newest first");
        assert_eq!(r.r32(td2 + 8), td1);
        assert!(r.ohci.irq(), "writeback done head");

        // SET_ADDRESS(5): SETUP, IN status; the device answers at 5 afterwards
        r.ohci.write(HC_INTERRUPT_STATUS as u64 * 4, WDH | RHSC, 0);
        r.mem[at..at + 8].copy_from_slice(&[0, 5, 5, 0, 0, 0, 0, 0]);
        r.td(tail, 0, setup, 8, 0x4000_2140);
        r.td(0x4000_2140, 2, 0, 0, 0x4000_2150);
        r.w32(ed + 4, 0x4000_2150);
        r.w32(ed + 8, tail);
        r.frame();
        assert_eq!(r.kbd.address(), 5);
    }

    #[test]
    fn interrupt_endpoint_naks_until_a_key_is_pressed() {
        let mut r = Rig::new();
        r.ohci.write(HC_RH_PORT_STATUS as u64 * 4, PRS, 0);
        r.frame();
        r.kbd.st.configured = true;
        let (ed, td, tail, buf) = (0x4000_2000, 0x4000_2100, 0x4000_2110, 0x4000_3000u32);
        r.td(td, 2, buf, 8, tail);
        r.w32(ed, 8 << 16 | 1 << 7 | 2 << 11); // endpoint 1 IN, MPS 8
        r.w32(ed + 4, tail);
        r.w32(ed + 8, td);
        for k in 0..32 {
            r.w32(HCCA + 4 * k, ed);
        }
        r.frame();
        assert_eq!(r.r32(ed + 8) & !0xf, td, "NAK leaves the TD queued");
        r.kbd.key(0x04, true); // 'a'
        r.frame();
        let b = (buf as u64 - BASE) as usize;
        assert_eq!(r.mem[b + 2], 0x04, "report with the key");
        assert_eq!(r.r32(ed + 8) & !0xf, tail);
    }

    #[test]
    fn ehci_halts_until_run_and_keeps_its_port_with_the_companion() {
        let mut e = Ehci::default();
        assert_ne!(e.read(0x14) & 1 << 12, 0, "halted");
        e.write(0x10, 1 | 1 << 5);
        assert_eq!(e.read(0x14) & (1 << 12 | 1 << 15), 1 << 15, "running, async schedule on");
        e.write(0x54, 1 << 12);
        assert_eq!(e.read(0x54) & 3, 0, "nothing connected");
        e.write(0x10, 2);
        assert_ne!(e.read(0x14) & 1 << 12, 0, "reset halts");
    }
}
