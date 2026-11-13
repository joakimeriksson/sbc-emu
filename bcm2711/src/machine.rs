//! The Pi 4B machine: one core, its bus, virtual time and devices.
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
/// GPU memory carve-out at the top of RAM (firmware `gpu_mem=64`), holding the framebuffer
pub const GPU_MEM: u64 = 64 << 20;
const PPI_VTIMER: usize = 27;
const PPI_PTIMER: usize = 30;
const SPI_UART0: usize = 32 + 121;
const SPI_MBOX: usize = 32 + 33;
const SPI_EMMC2: usize = 32 + 126;
const SPI_USB: usize = 32 + 73;
const SPI_GENET0: usize = 32 + 157;
const SPI_GENET1: usize = 32 + 158;

pub struct Soc {
    pub ram: Vec<u8>,
    pub gic: Gic,
    pub uart: Pl011,
    pub mbox: Mailbox,
    pub sd: Sdhci,
    /// SDIO host with the CYW43455 Wi-Fi chip
    pub wifi: Sdhci,
    /// the network behind the Wi-Fi access point
    pub wifi_net: Option<sbc_net::net::VirtualNet>,
    pub usb: Dwc2,
    pub genet: Genet,
    /// the network behind the Ethernet port (None = cable unplugged)
    pub net: Option<sbc_net::net::VirtualNet>,
    pub unknown: Vec<u64>,
    /// a device register was touched: interrupt lines must be re-derived
    pub dirty: bool,
}

impl Soc {
    fn mmio_read(&mut self, a: u64) -> Result<u32, Fault> {
        self.dirty = true;
        Ok(match a {
            UART0..=0xfe20_1fff => self.uart.read(a - UART0),
            MBOX..=0xfe00_b8bf => self.mbox.read(a - MBOX),
            EMMC2..=0xfe34_00ff => self.sd.read(a - EMMC2),
            SDHOST_WIFI..=0xfe30_00ff => self.wifi.read(a - SDHOST_WIFI),
            USB..=0xfe98_ffff => self.usb.read(a - USB),
            GENET..=0xfd58_ffff => self.genet.read(a - GENET),
            GICD..=0xff84_1fff => self.gic.dist_read(a - GICD),
            GICC..=0xff84_3fff => self.gic.cpu_read(a - GICC),
            PERIPH..=0xffff_ffff => { self.note(a); 0 }
            _ => return Err(Fault::Unmapped),
        })
    }
    /// `size`-byte write of `v` at `a` (unshifted value, any alignment within the word).
    fn mmio_write(&mut self, a: u64, v: u32, size: u32) -> Result<(), Fault> {
        self.dirty = true;
        if (EMMC2..=0xfe34_00ff).contains(&a) { self.sd.write(a - EMMC2, v, size); return Ok(()); }
        if (SDHOST_WIFI..=0xfe30_00ff).contains(&a) { self.wifi.write(a - SDHOST_WIFI, v, size); return Ok(()); }
        let a = a & !3;
        match a {
            UART0..=0xfe20_1fff => self.uart.write(a - UART0, v),
            MBOX..=0xfe00_b8bf => self.mbox.write(a - MBOX, v, &mut self.ram),
            USB..=0xfe98_ffff => self.usb.write(a - USB, v, &mut self.ram),
            GENET..=0xfd58_ffff => self.genet.write(a - GENET, v, &mut self.ram),
            GICD..=0xff84_1fff => self.gic.dist_write(a - GICD, v),
            GICC..=0xff84_3fff => self.gic.cpu_write(a - GICC, v),
            PERIPH..=0xffff_ffff => self.note(a),
            _ => return Err(Fault::Unmapped),
        }
        Ok(())
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Stop {
    /// instruction budget for this call used up
    Budget,
    /// idle: core waits and nothing is due before the returned time (ns)
    Idle(u64),
    Unimplemented(u64, u32),
    Hlt(u16),
    Brk(u16),
    /// PSCI SYSTEM_OFF
    PowerOff,
    /// PSCI SYSTEM_RESET
    Reset,
    /// the core keeps taking exceptions without retiring instructions (a fault loop)
    ExceptionStorm { pc: u64, esr: u64, elr: u64, far: u64, el: u8 },
}

pub struct Machine {
    pub cpu: Cpu,
    pub soc: Soc,
    /// virtual time in ns
    pub now: u64,
    pub exceptions: u64,
    pub irqs: u64,
    /// instruction count at which interrupt lines must be re-evaluated (next timer deadline)
    next_check: u64,
}

impl Machine {
    pub fn new(ram: usize) -> Self {
        let mut cpu = Cpu::new();
        cpu.sys.cntfrq = CNTFRQ;
        cpu.firmware_calls_to_host = true;
        let fb_base = ram as u64 - GPU_MEM;
        let soc = Soc { ram: vec![0; ram], gic: Gic::new(), uart: Pl011::new(), mbox: Mailbox::new(fb_base, GPU_MEM, 1024, 768), sd: Sdhci::new(None), wifi: { let mut h = Sdhci::new(None); h.sdio = Some(Box::new(crate::wifi::Cyw43455::new())); h }, wifi_net: None, usb: Dwc2::new(), genet: Genet::new(), net: None, unknown: Vec::new(), dirty: true };
        let mut m = Machine { cpu, soc, now: 0, exceptions: 0, irqs: 0, next_check: 0 };
        m.cpu.ram = m.soc.ram.as_mut_ptr();
        m.cpu.ram_len = m.soc.ram.len() as u64;
        m
    }

    pub fn load(&mut self, addr: u64, data: &[u8]) {
        self.soc.ram[addr as usize..addr as usize + data.len()].copy_from_slice(data);
    }

    /// Linux arm64 boot protocol: enter at EL1h (the firmware would enter at EL2; EL1 keeps the
    /// first bring-up free of hypervisor setup), MMU off, interrupts masked, x0 = DTB.
    pub fn boot_linux(&mut self, entry: u64, dtb: u64) {
        self.cpu.el = 1;
        self.cpu.spsel = true;
        self.cpu.daif = 0xf << 6;
        self.cpu.pc = entry;
        self.cpu.x = [0; 31];
        self.cpu.x[0] = dtb;
        // firmware leaves FP/SIMD accessible
        self.cpu.sys.cpacr_el1 = 3 << 20;
    }

    fn ns_of_count(c: u64) -> u64 { (c / 27 * 500) + (c % 27 * 500).div_ceil(27) }

    /// Virtual time from the core's retired-instruction count.
    fn sync_time(&mut self) {
        self.now = self.cpu.now_ns();
        self.cpu.refresh_counter();
    }

    fn update_irqs(&mut self) {
        self.sync_time();
        self.cpu.sys.timers_changed = false;
        let cnt = self.cpu.sys.cnt;
        let v = self.cpu.sys.vtimer.firing(cnt.wrapping_sub(self.cpu.sys.cntvoff));
        let p = self.cpu.sys.ptimer.firing(cnt);
        self.soc.gic.set_level(PPI_VTIMER, v);
        self.soc.gic.set_level(PPI_PTIMER, p);
        let u = self.soc.uart.irq();
        self.soc.gic.set_level(SPI_UART0, u);
        self.pump_network();
        let (g0, g1) = (self.soc.genet.irq0(), self.soc.genet.irq1());
        self.soc.gic.set_level(SPI_GENET0, g0);
        self.soc.gic.set_level(SPI_GENET1, g1);
        self.soc.usb.tick(self.now);
        let usb = self.soc.usb.irq();
        self.soc.gic.set_level(SPI_USB, usb);
        // EMMC2 and the Wi-Fi SD host share SPI 126, as on the Pi 4
        let sd = self.soc.sd.irq() || self.soc.wifi.irq();
        self.soc.gic.set_level(SPI_EMMC2, sd);
        let mb = self.soc.mbox.irq();
        self.soc.gic.set_level(SPI_MBOX, mb);
        let irq = self.soc.gic.irq_out();
        self.cpu.set_irq(irq, false);
        self.soc.dirty = false;
        // next forced re-evaluation: the earliest armed timer deadline, at most 1 ms ahead
        let horizon = self.now + 1_000_000;
        let due = self.next_deadline().map_or(horizon, |d| d.clamp(self.now + 1, horizon));
        self.next_check = self.cpu.instret + (due - self.now);
    }

    /// Next timer or device deadline in ns.
    fn next_deadline(&self) -> Option<u64> {
        let s = &self.cpu.sys;
        let mut d: Option<u64> = self.soc.usb.next_event();
        for (t, off) in [(s.vtimer, s.cntvoff), (s.ptimer, 0)] {
            if t.ctl & 3 == 1 {
                let ns = Self::ns_of_count(t.cval.wrapping_add(off));
                d = Some(d.map_or(ns, |x: u64| x.min(ns)));
            }
        }
        d
    }

    /// Run up to `budget` instructions.
    pub fn run(&mut self, budget: u64) -> Stop {
        let end = self.cpu.instret + budget;
        // steps, not retired instructions, bound the call: faults do not retire
        let mut steps = budget;
        let mut storm = 0u32;
        let stop = loop {
            if self.cpu.instret >= end || steps == 0 { break Stop::Budget; }
            steps -= 1;
            if self.soc.dirty || self.cpu.sys.timers_changed || self.cpu.instret >= self.next_check { self.update_irqs(); }
            match self.cpu.step(&mut self.soc) {
                Ok(()) => storm = 0,
                Err(Trap::Exception(_, el)) => {
                    self.exceptions += 1;
                    storm += 1;
                    if storm >= 1_000_000 {
                        let e = el as usize;
                        break Stop::ExceptionStorm { pc: self.cpu.pc, esr: self.cpu.esr[e], elr: self.cpu.elr[e], far: self.cpu.far[e], el };
                    }
                    continue;
                }
                Err(Trap::Interrupt(..)) => { self.irqs += 1; self.soc.dirty = true; }
                Err(Trap::Wait) => {
                    self.update_irqs();
                    if self.cpu.irq { continue; }
                    let until = self.next_deadline().unwrap_or(self.now + 10_000_000).max(self.now);
                    break Stop::Idle(until);
                }
                Err(Trap::HostCall(_)) => {
                    if let Some(stop) = self.psci() { break stop; }
                }
                Err(Trap::Unimplemented(pc, i)) => break Stop::Unimplemented(pc, i),
                Err(Trap::Hlt(n)) => break Stop::Hlt(n),
                Err(Trap::Brk(n)) => break Stop::Brk(n),
            }
        };
        self.sync_time();
        stop
    }

    /// Exchange frames between the Ethernet MAC and the virtual network.
    fn pump_network(&mut self) {
        let soc = &mut self.soc;
        let now_us = self.now / 1000;
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

    /// PSCI 1.1 as the Pi 4's firmware (Trusted Firmware-A) provides it, for one CPU.
    fn psci(&mut self) -> Option<Stop> {
        const NOT_SUPPORTED: u64 = -1i64 as u64;
        let f = self.cpu.x[0] as u32;
        let r = match f {
            0x8400_0000 => 0x0001_0001,                 // PSCI_VERSION 1.1
            0x8400_0006 => 2,                            // MIGRATE_INFO_TYPE: no migration needed
            0x8400_0008 => return Some(Stop::PowerOff),  // SYSTEM_OFF
            0x8400_0009 => return Some(Stop::Reset),     // SYSTEM_RESET
            0x8400_000a => match self.cpu.x[1] as u32 {  // PSCI_FEATURES
                0x8400_0000 | 0x8400_0006 | 0x8400_0008 | 0x8400_0009 | 0x8400_000a => 0,
                _ => NOT_SUPPORTED,
            },
            _ => NOT_SUPPORTED,
        };
        self.cpu.x[0] = r;
        None
    }

    /// Advance idle time to `t` (after an `Idle` stop) and re-evaluate interrupts.
    pub fn idle_until(&mut self, t: u64) {
        self.sync_time();
        if t > self.now {
            // skip idle time: move the core's time base forward
            self.cpu.sys.time_base_ns = t;
            self.cpu.sys.time_base_instret = self.cpu.instret;
        }
        self.update_irqs();
    }
}

const SNAP_MAGIC: &[u8; 8] = b"SBCSNAP1";

impl Machine {
    /// Serialize the whole machine (deflate-compressed; RAM keeps only non-zero pages).
    pub fn save(&self) -> Vec<u8> {
        let mut w = emu_core::snap::W::default();
        w.tag("machine-v3");
        w.u64(self.soc.ram.len() as u64);
        w.u64(self.now); w.u64(self.exceptions); w.u64(self.irqs);
        self.cpu.save_state(&mut w);
        self.soc.gic.save_state(&mut w);
        self.soc.uart.save_state(&mut w);
        self.soc.mbox.save_state(&mut w);
        self.soc.sd.save_state(&mut w);
        self.soc.usb.save_state(&mut w);
        self.soc.genet.save_state(&mut w);
        self.soc.wifi.save_state(&mut w);
        // RAM: list of non-zero 4 KiB pages
        let pages: Vec<usize> = (0..self.soc.ram.len() / 4096).filter(|&p| self.soc.ram[p * 4096..(p + 1) * 4096].iter().any(|&b| b != 0)).collect();
        w.u64(pages.len() as u64);
        for p in pages { w.u32(p as u32); w.0.extend_from_slice(&self.soc.ram[p * 4096..(p + 1) * 4096]); }
        let mut out = SNAP_MAGIC.to_vec();
        out.extend_from_slice(&miniz_oxide::deflate::compress_to_vec(&w.0, 1));
        out
    }

    /// Restore a snapshot into a machine that already has the same SD card attached.
    pub fn restore(&mut self, snap: &[u8]) -> Result<(), String> {
        if snap.len() < 8 || &snap[..8] != SNAP_MAGIC { return Err("not an sbc-emu snapshot".into()); }
        let raw = miniz_oxide::inflate::decompress_to_vec(&snap[8..]).map_err(|e| format!("snapshot inflate: {e:?}"))?;
        let mut r = emu_core::snap::R::new(&raw);
        let e = |e: emu_core::snap::SnapError| e.0;
        r.tag("machine-v3").map_err(e)?;
        let ram = r.u64().map_err(e)? as usize;
        if ram != self.soc.ram.len() { return Err(format!("snapshot RAM is {} MiB, machine has {} MiB", ram >> 20, self.soc.ram.len() >> 20)); }
        self.now = r.u64().map_err(e)?; self.exceptions = r.u64().map_err(e)?; self.irqs = r.u64().map_err(e)?;
        self.cpu.load_state(&mut r).map_err(e)?;
        self.soc.gic.load_state(&mut r).map_err(e)?;
        self.soc.uart.load_state(&mut r).map_err(e)?;
        self.soc.mbox.load_state(&mut r).map_err(e)?;
        self.soc.sd.load_state(&mut r).map_err(e)?;
        self.soc.usb.load_state(&mut r).map_err(e)?;
        self.soc.genet.load_state(&mut r).map_err(e)?;
        self.soc.wifi.load_state(&mut r).map_err(e)?;
        self.soc.ram.fill(0);
        for _ in 0..r.u64().map_err(e)? {
            let p = r.u32().map_err(e)? as usize;
            let b = r.b.get(r.p..r.p + 4096).ok_or("truncated RAM page")?;
            self.soc.ram[p * 4096..(p + 1) * 4096].copy_from_slice(b);
            r.p += 4096;
        }
        self.cpu.ram = self.soc.ram.as_mut_ptr();
        self.cpu.ram_len = self.soc.ram.len() as u64;
        self.next_check = 0;
        self.soc.dirty = true;
        Ok(())
    }
}
