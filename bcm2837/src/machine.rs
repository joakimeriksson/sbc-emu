//! The Pi 3 machine: up to four Cortex-A53 cores, their bus, virtual time and devices.
//!
//! Virtual time: one retired instruction = 1 ns (as on the Pi 4). The generic counter runs at
//! 19.2 MHz (the Pi 3's crystal; the Pi 4's is 54 MHz). A core in `wfi` skips to the next timer
//! deadline. The scheduling (cores in turns on one thread, or a thread per core with
//! `--threads multi`) is the Pi 4's; what differs is the interrupt path: each core's timer
//! outputs and mailboxes go into the per-core controller, device interrupts into the ARM-side
//! controller, whose output the per-core controller routes to one core.
//!
//! USB: the DWC2 OTG core (the Pi 4's model) with the board's USB tree behind its root port:
//! on the 3B+ the LAN7515's two hubs, its LAN7800 Ethernet (connected to `sbc-net`) and the
//! keyboard and tablet on two of the USB-A ports (`usb_tree`).
//!
//! Display: with `dtoverlay=vc4-kms-v3d` (`Machine::enable_kms`) the BCM2835-family HVS, pixel
//! valves and HDMI encoder of `bcm2711::vc4` (`Gen::Bcm2835`), the monitor's EDID on BSC2 (the
//! DDC bus), hot-plug on GPIO 28 (the 3B's on the firmware's GPIO expander), HDMI audio through
//! the DMA controller into the MAI FIFO (DREQ 17), and CEC; without it the firmware framebuffer.
//! The composite encoder (VEC) is pixel valve 2's other output; the device tree lists it when
//! `config.txt` asks (`dtoverlay=vc4-kms-v3d,composite`), and the host shows whichever of the
//! two pixel valve 2 drives.
//!
//! Storage and wireless as on a real Pi 3: the SD card on the `sdhost` controller; on the 3B+
//! and 3A+ the CYW43455 on the Arasan SDHCI (Wi-Fi over SDIO, the Pi 4's model) and its
//! Bluetooth core on UART0 (H4); both chips powered by the firmware's GPIO expander (WL_ON,
//! BT_ON), which Linux drives through mailbox property tags.
use crate::intc::{ArmCtrl, LocalIntc};
use crate::sdhost::Sdhost;
use crate::Model;
use arm_a64::{Cpu, CpuProfile};
use bcm2711::dwc2::Dwc2;
use bcm2711::mailbox::Mailbox;
use bcm2711::periph::{AuxUart, Pm};
use bcm2711::sdhci::{SdioCard, Sdhci};
use emu_core::lan78xx::Lan78xx;
use emu_core::usb::{DevId, Devices};
use emu_core::usb_hub::{Hub, Node, TreeBus, UsbTree};
use emu_core::{Bus, Core, Fault, Trap};
use peripherals::pl011::Pl011;

pub const CNTFRQ: u64 = 19_200_000;
/// The peripheral window (16 MiB), the VideoCore's 0x7e00_0000 bus addresses seen by the ARM.
pub const PERIPH: u64 = 0x3f00_0000;
pub const ASB: u64 = 0x3f00_a000;
pub const ARMCTRL: u64 = 0x3f00_b200;
pub const MBOX: u64 = 0x3f00_b880;
/// VCHIQ's doorbells (`brcm,bcm2835-vchiq`)
pub const VCHIQ: u64 = 0x3f00_b840;
pub const PM: u64 = 0x3f10_0000;
pub const RNG: u64 = 0x3f10_4000;
pub const UART0: u64 = 0x3f20_1000;
/// the SD card's host (`brcm,bcm2835-sdhost`)
pub const SDHOST: u64 = 0x3f20_2000;
pub const THERMAL: u64 = 0x3f21_2000;
pub const AUX: u64 = 0x3f21_5000;
/// the Arasan SD host (`brcm,bcm2835-sdhci`, "mmcnr" in the vendor tree): the Wi-Fi's SDIO
pub const SDHCI: u64 = 0x3f30_0000;
/// the DWC2 USB 2.0 OTG controller
pub const USB: u64 = 0x3f98_0000;
pub const TXP: u64 = 0x3f00_4000;
pub const DMA: u64 = 0x3f00_7000;
pub const GPIO: u64 = 0x3f20_0000;
pub const PV0: u64 = 0x3f20_6000;
pub const PV1: u64 = 0x3f20_7000;
pub const HVS: u64 = 0x3f40_0000;
/// BSC2, the HDMI port's DDC bus
pub const DDC: u64 = 0x3f80_5000;
/// the composite video encoder
pub const VEC: u64 = 0x3f80_6000;
pub const PV2: u64 = 0x3f80_7000;
/// the HDMI encoder's "hd" block (MAI audio, VID_CTL)
pub const HD: u64 = 0x3f80_8000;
pub const HDMI: u64 = 0x3f90_2000;
/// the VideoCore IV 3D block (`brcm,vc4-v3d`): its model is the `vc4-3d` crate's
pub const V3D: u64 = 0x3fc0_0000;
/// the ARM-local block: the per-core interrupt controller and mailboxes
pub const LOCAL: u64 = 0x4000_0000;
/// ARM-side interrupt numbers (bank 0)
pub const ARM_MAILBOX: usize = 1;
/// the ARM's doorbell 0: VCHIQ
pub const ARM_DOORBELL0: usize = 2;
/// GPU interrupt numbers (banks 1 and 2 of the ARM-side controller)
pub const GPU_USB: usize = 9;
pub const GPU_V3D: usize = 10;
pub const GPU_TXP: usize = 11;
/// DMA channels 0..10 on 16..26, 11..14 share 27
pub const GPU_DMA0: usize = 16;
pub const GPU_HVS: usize = 33;
pub const GPU_HDMI0: usize = 40;
pub const GPU_HDMI1: usize = 41;
pub const GPU_PV2: usize = 42;
pub const GPU_PV0: usize = 45;
pub const GPU_PV1: usize = 46;
pub const GPU_VEC: usize = 59;
pub const GPU_GPIO0: usize = 49;
/// the BSC controllers' shared interrupt
pub const GPU_I2C: usize = 53;
pub const GPU_AUX: usize = 29;
pub const GPU_SDHOST: usize = 56;
pub const GPU_UART0: usize = 57;
pub const GPU_RNG: usize = 61;
pub const GPU_SDHCI: usize = 62;

/// The Ethernet's MAC address: locally administered (the firmware of a real Pi 3B+ writes the
/// board's own into the device tree's `local-mac-address`).
pub const ETH_MAC: [u8; 6] = [0x02, 0x5b, 0xc0, 0x00, 0x03, 0x01];
/// The Wi-Fi interface's MAC address (the firmware's OTP holds the real board's).
pub const WIFI_MAC: [u8; 6] = [0x02, 0x5b, 0xc0, 0x00, 0x03, 0x02];
/// Lines of the firmware's GPIO expander (the vendor tree's `expgpio`).
pub const EXP_BT_ON: u8 = 0;
pub const EXP_WL_ON: u8 = 1;
pub const EXP_PWR_LED: u8 = 2;
pub const EXP_LAN_RUN: u8 = 3;

/// What hangs off the root port, by model: the 3B+'s LAN7515 (a 4-port hub, behind its port 1
/// a 3-port hub with the LAN7800 on its port 1), the 3B's LAN9514 hub (its Ethernet is not
/// modelled), or nothing but the keyboard (3A+, Zero 2 W). The keyboard and the tablet sit on
/// USB-A ports, full speed, so they are reached through the hub's transaction translator.
pub fn usb_tree(model: Model, devs: &mut Devices) -> UsbTree {
    let mut t = UsbTree::new();
    match model {
        Model::B3Plus => {
            let h1 = t.add(Node::Hub(Hub::new(4, 0x0424, 0x2514, 1 << 1, true)), None, devs);
            let h2 = t.add(Node::Hub(Hub::new(3, 0x0424, 0x2514, 1 << 1, true)), Some((h1, 1)), devs);
            t.add(Node::Lan(Box::new(Lan78xx::new())), Some((h2, 1)), devs);
            t.add(Node::Dev(DevId::Keyboard), Some((h1, 2)), devs);
            t.add(Node::Dev(DevId::Tablet), Some((h1, 3)), devs);
        }
        Model::B3 => {
            // the LAN9514: a 5-port hub with the Ethernet controller on port 1
            let h = t.add(Node::Hub(Hub::new(5, 0x0424, 0x9514, 1 << 1, true)), None, devs);
            t.add(Node::Smsc(Box::new(emu_core::smsc95xx::Smsc95xx::new())), Some((h, 1)), devs);
            t.add(Node::Dev(DevId::Keyboard), Some((h, 2)), devs);
            t.add(Node::Dev(DevId::Tablet), Some((h, 3)), devs);
        }
        Model::A3Plus | Model::Zero2W => { t.add(Node::Dev(DevId::Keyboard), None, devs); }
    }
    t
}

pub struct Soc {
    pub ram: Vec<u8>,
    /// RAM given to Linux; the VideoCore's share (and the framebuffer) is above it
    pub arm_ram: u64,
    /// the core whose access is on the bus
    pub cur: usize,
    pub local: LocalIntc,
    pub armctrl: ArmCtrl,
    /// UART0 (PL011): Bluetooth's (3B+, 3A+), else a spare serial port
    pub uart: Pl011,
    /// the mini UART: the serial console (ttyS0)
    pub aux: AuxUart,
    pub mbox: Mailbox,
    /// the SD card's host
    pub sd: Sdhost,
    /// the Arasan SDHCI: the CYW43455's SDIO bus (its card is there while WL_ON powers it)
    pub sdhci: Sdhci,
    /// the board has the CYW43455 (3B+, 3A+; the 3B's and Zero 2 W's chips are not modelled)
    pub wireless: bool,
    /// the Wi-Fi chip logs its host interface (`--trace-wifi`)
    pub wifi_trace: bool,
    /// the Wi-Fi access point's network
    pub wifi_net: Option<sbc_net::net::VirtualNet>,
    /// the Bluetooth controller on UART0 (None: UART0 is a plain serial port)
    pub bt: Option<bcm2711::bluetooth::Bluetooth>,
    pub rng: crate::periph::Rng,
    pub thermal: crate::periph::Thermal,
    pub pm: Pm,
    /// the USB host and what is plugged into it
    pub usb: Dwc2,
    pub devs: Devices,
    pub tree: UsbTree,
    /// the network behind the Ethernet port (`connect_network`)
    pub net: Option<sbc_net::net::VirtualNet>,
    /// the VideoCore IV 3D GPU (`vc4-3d`)
    pub v3d: vc4_3d::V3d,
    /// AXI bridge (ASB) control registers
    asb: [u32; 12],
    pub gpio: bcm2711::gpio::Gpio,
    pub dma: bcm2711::dma::Dma,
    /// when DREQ-paced DMA wants another look (ns; Some(0): now)
    pace_next: Option<u64>,
    /// BSC2: the HDMI port's DDC bus with the monitor's EDID EEPROM
    pub ddc: bcm2711::i2c::I2c,
    /// the KMS display pipeline (HVS, pixel valves, HDMI); None: the firmware framebuffer only
    pub vc4: Option<bcm2711::vc4::Vc4>,
    /// the outputs the device tree lists with the KMS pipeline (HDMI, composite)
    pub outputs: bcm2711::boot::KmsOutputs,
    /// HDMI audio played since the host last took it (16-bit stereo) and its rate, kept while
    /// the host wants it (`capture_audio`)
    pub audio: Vec<i16>,
    pub audio_rate: u32,
    pub capture_audio: bool,
    /// the VideoCore firmware's side of VCHIQ: its media components and audio service
    pub vchiq: crate::vchiq::Vchiq,
    /// virtual time of the last device update (ns): what the VideoCore's services see
    pub now: u64,
    pub unknown: Vec<u64>,
    /// a device register was touched: interrupt lines must be re-derived
    pub dirty: bool,
}

impl Soc {
    /// The RAM Linux gets (below the VideoCore's share).
    pub fn arm_ram(&self) -> u64 { self.arm_ram }

    /// Core `k`'s IRQ input.
    pub fn irq_out(&self, k: usize) -> bool { self.local.irq_out(k, self.armctrl.irq()) }

    pub(crate) fn mmio_read(&mut self, a: u64) -> Result<u32, Fault> {
        if (SDHOST..=0x3f20_20ff).contains(&a) { return Ok(self.sdhost_access(|sd| sd.read(a - SDHOST))); }
        self.dirty = true;
        Ok(match a {
            UART0..=0x3f20_1fff => { let v = self.uart.read(a - UART0); self.bt_pump(); v }
            AUX..=0x3f21_50ff => self.aux.read(a - AUX),
            MBOX..=0x3f00_b8bf => self.mbox.read(a - MBOX),
            VCHIQ..=0x3f00_b87f => self.vchiq.read(a - VCHIQ),
            ARMCTRL..=0x3f00_b3ff => self.armctrl.read(a - ARMCTRL),
            SDHCI..=0x3f30_00ff => self.sdhci.read(a - SDHCI),
            USB..=0x3f98_ffff => self.usb.read(a - USB),
            RNG..=0x3f10_400f => self.rng.read(a - RNG),
            THERMAL..=0x3f21_2007 => self.thermal.read(a - THERMAL),
            PM..=0x3f10_0113 => self.pm.read(a - PM),
            V3D..=0x3fc0_0fff => self.v3d.read(a - V3D),
            // AXI bridges (ASB): the ID register; a stop request (bit 0) is acknowledged (bit 1) at once
            GPIO..=0x3f20_00b3 => self.gpio.read(a - GPIO),
            DMA..=0x3f00_7fff => self.dma.read(a - DMA),
            DDC..=0x3f80_5fff => self.ddc.read(a - DDC),
            HVS..=0x3f40_5fff if self.vc4.is_some() => self.vc4.as_ref().unwrap().hvs_read(a - HVS),
            PV0..=0x3f20_60ff if self.vc4.is_some() => self.vc4.as_ref().unwrap().pv_read_n(0, a - PV0),
            PV1..=0x3f20_70ff if self.vc4.is_some() => self.vc4.as_ref().unwrap().pv_read_n(1, a - PV1),
            PV2..=0x3f80_70ff if self.vc4.is_some() => self.vc4.as_ref().unwrap().pv_read_n(2, a - PV2),
            HDMI..=0x3f90_25ff if self.vc4.is_some() => self.vc4.as_mut().unwrap().hdmi_read(a - HDMI),
            HD..=0x3f80_80ff if self.vc4.is_some() => self.vc4.as_ref().unwrap().hd_read(a - HD),
            VEC..=0x3f80_6fff if self.vc4.is_some() => self.vc4.as_ref().unwrap().vec_read(a - VEC),
            TXP..=0x3f00_401f if self.vc4.is_some() => self.vc4.as_ref().unwrap().txp_read(a - TXP),
            ASB..=0x3f00_a02f => match a & 0xff { 0x20 => 0x6272_6467, o => { let r = self.asb[(o / 4) as usize % 12]; (r & !2) | ((r & 1) << 1) } },
            LOCAL..=0x4000_00ff => self.local.read(a - LOCAL, self.armctrl.irq()),
            PERIPH..=0x3fff_ffff | 0x4000_0100..=0x4003_ffff => { self.note(a); 0 }
            _ => return Err(Fault::Unmapped),
        })
    }

    /// `size`-byte write of `v` at `a` (unshifted value, any alignment within the word).
    fn mmio_write(&mut self, a: u64, v: u32, size: u32) -> Result<(), Fault> {
        if (SDHOST..=0x3f20_20ff).contains(&a) { self.sdhost_access(|sd| sd.write(a & !3 & 0xff, v)); self.sdhost_dma(); return Ok(()); }
        self.dirty = true;
        if (SDHCI..=0x3f30_00ff).contains(&a) { self.sdhci.write(a - SDHCI, v, size, &mut self.ram); return Ok(()); }
        let a = a & !3;
        match a {
            UART0..=0x3f20_1fff => { self.uart.write(a - UART0, v); self.bt_pump(); }
            AUX..=0x3f21_50ff => self.aux.write(a - AUX, v),
            MBOX..=0x3f00_b8bf => {
                self.mbox.write(a - MBOX, v, &mut self.ram);
                if let Some(base) = self.mbox.vchiq_init.take() { self.vchiq.init(&mut self.ram, base); }
                self.power_lines();
            }
            VCHIQ..=0x3f00_b87f => self.vchiq.write(a - VCHIQ, v, &mut self.ram, self.now),
            ARMCTRL..=0x3f00_b3ff => self.armctrl.write(a - ARMCTRL, v),
            USB..=0x3f98_ffff => self.usb.write_bus(a - USB, v, &mut self.ram, &mut TreeBus { tree: &mut self.tree, devs: &mut self.devs }),
            RNG..=0x3f10_400f => self.rng.write(a - RNG, v),
            THERMAL..=0x3f21_2007 => self.thermal.write(a - THERMAL, v),
            PM..=0x3f10_0113 => self.pm.write(a - PM, v),
            V3D..=0x3fc0_0fff => self.v3d.write(a - V3D, v, &mut self.ram),
            ASB..=0x3f00_a02f => self.asb[((a & 0xff) / 4) as usize % 12] = v & 0x00ff_ffff,
            GPIO..=0x3f20_00b3 => self.gpio.write(a - GPIO, v),
            DMA..=0x3f00_7fff => if let Some(n) = self.dma.write(a - DMA, v) { self.dma_run(n) },
            DDC..=0x3f80_5fff => self.ddc.write(a - DDC, v),
            HVS..=0x3f40_5fff if self.vc4.is_some() => self.vc4.as_mut().unwrap().hvs_write(a - HVS, v),
            PV0..=0x3f20_60ff if self.vc4.is_some() => self.vc4.as_mut().unwrap().pv_write_n(0, a - PV0, v),
            PV1..=0x3f20_70ff if self.vc4.is_some() => self.vc4.as_mut().unwrap().pv_write_n(1, a - PV1, v),
            PV2..=0x3f80_70ff if self.vc4.is_some() => self.vc4.as_mut().unwrap().pv_write_n(2, a - PV2, v),
            HDMI..=0x3f90_25ff if self.vc4.is_some() => self.vc4.as_mut().unwrap().hdmi_write(a - HDMI, v),
            HD..=0x3f80_80ff if self.vc4.is_some() => self.vc4.as_mut().unwrap().hd_write(a - HD, v),
            VEC..=0x3f80_6fff if self.vc4.is_some() => self.vc4.as_mut().unwrap().vec_write(a - VEC, v),
            TXP..=0x3f00_401f if self.vc4.is_some() => self.vc4.as_mut().unwrap().txp_write(a - TXP, v),
            LOCAL..=0x4000_00ff => self.local.write(a - LOCAL, v),
            PERIPH..=0x3fff_ffff | 0x4000_0100..=0x4003_ffff => self.note(a),
            _ => return Err(Fault::Unmapped),
        }
        Ok(())
    }

    /// An access to `sdhost`: PIO moves a block as 128 data-register accesses plus FIFO-level
    /// polls, so only an access that changes its interrupt line has the lines re-derived.
    #[inline]
    fn sdhost_access<R>(&mut self, f: impl FnOnce(&mut Sdhost) -> R) -> R {
        let irq = self.sd.irq();
        let r = f(&mut self.sd);
        if self.sd.irq() != irq { self.dirty = true; }
        r
    }

    /// Device interrupt lines into the ARM-side controller at virtual time `now` (ns).
    pub(crate) fn device_lines(&mut self, now: u64) {
        self.armctrl.set_gpu(GPU_AUX, self.aux.irq());
        self.armctrl.set_gpu(GPU_UART0, self.uart.irq());
        self.armctrl.set_gpu(GPU_SDHOST, self.sd.irq());
        self.armctrl.set_gpu(GPU_SDHCI, self.sdhci.irq());
        self.armctrl.set_gpu(GPU_USB, self.usb.irq());
        self.armctrl.set_arm(ARM_MAILBOX, self.mbox.irq());
        self.now = self.now.max(now);
        self.vchiq.tick(&mut self.ram, now);
        if !self.vchiq.audio.out.is_empty() {
            // the headphone jack's audio goes where HDMI's does (`--audio-out`)
            let a = &mut self.vchiq.audio;
            if self.capture_audio { self.audio_rate = a.rate; self.audio.extend(a.out.drain(..)); } else { a.out.clear(); }
        }
        self.armctrl.set_arm(ARM_DOORBELL0, self.vchiq.irq());
        if self.pace_next.is_some_and(|t| now >= t) { self.dma_pace(now); }
        for n in 0..11 { self.armctrl.set_gpu(GPU_DMA0 + n, self.dma.irq(n)); }
        self.armctrl.set_gpu(GPU_DMA0 + 11, (11..15).any(|k| self.dma.irq(k)));
        // GPIO interrupt groups 0 (lines 0..27) and 1 (28..45), the two the device tree lists
        self.gpio.now_ns = now;
        let banks = self.gpio.irq();
        self.armctrl.set_gpu(GPU_GPIO0, banks[0]);
        self.armctrl.set_gpu(GPU_GPIO0 + 1, banks[1]);
        self.armctrl.set_gpu(GPU_I2C, self.ddc.irq());
        if let Some(v) = self.vc4.as_mut() {
            // the HDMI state machine clock paces the audio; the pixel clock (HDMI) or the VEC
            // clock (composite) pixel valve 2
            v.audio_clk_hz = self.mbox.rates[13] as u64;
            v.pixel_hz = self.mbox.rates[9] as u64;
            v.vec_hz = self.mbox.rates[15] as u64;
            v.tick(now);
            for (n, line) in [GPU_PV0, GPU_PV1, GPU_PV2].into_iter().enumerate() { self.armctrl.set_gpu(line, v.irq_pv_n(n)); }
            self.armctrl.set_gpu(GPU_HVS, v.irq_hvs());
            self.armctrl.set_gpu(GPU_HDMI0, v.irq_hdmi());
        }
        self.v3d.tick(now, &mut self.ram);
        self.armctrl.set_gpu(GPU_V3D, self.v3d.irq());
        self.pm.now = now;
        // nothing reads a spare UART0's output: keep it from growing without bound
        if self.uart.out.len() > 1 << 16 { self.uart.out.clear(); }
        if self.gpio.events.len() > 1024 { self.gpio.events.clear(); }
        self.dirty = false;
    }

    /// The next device deadline.
    pub(crate) fn next_event(&self) -> Option<u64> {
        let vc4 = self.vc4.as_ref().and_then(|v| v.next_event());
        let bt = self.bt.as_ref().and_then(|b| b.next_event());
        [self.pm.next_event(), self.usb.next_event(), self.pace_next, vc4, bt, self.vchiq.next_event(), self.v3d.next_event()].into_iter().flatten().min()
    }

    /// Feed DREQ-paced DMA channels what their peripherals asked for up to `now` (ns): the HDMI
    /// audio FIFO (DREQ 17).
    pub(crate) fn dma_pace(&mut self, now: u64) {
        use bcm2711::vc4::DREQ_HDMI_BCM2835 as DREQ;
        let mut demands = [(DREQ, self.vc4.as_mut().map_or(0, |v| v.mai_demand(now)))];
        let mut words = Vec::new();
        let mut dma = std::mem::take(&mut self.dma);
        dma.pace(&mut demands, self, &mut words);
        self.dma = dma;
        if let Some(vc) = self.vc4.as_mut() {
            for (_, v) in words { vc.mai_push(v); }
            if !vc.audio.is_empty() {
                if self.capture_audio { self.audio_rate = vc.audio_rate; self.audio.extend(vc.audio.drain(..)); } else { vc.audio.clear(); }
            }
        }
        if self.audio.len() > 2 * 48_000 * 10 { self.audio.drain(..self.audio.len() / 4 * 2); }
        self.pace_next = if self.dma.paced_active() { Some(now + 500_000) } else { None };
    }

    /// DMA channel `n` was made active: walk its control blocks now, up to one that waits for
    /// a DREQ.
    fn dma_run(&mut self, n: usize) {
        let mut dma = std::mem::take(&mut self.dma);
        let paced = dma.run(n, self);
        self.dma = dma;
        if paced { self.pace_next = Some(0); }
        self.sdhost_dma();
    }

    /// sdhost's DMA (DREQ 13): while its FIFO has data (a read) or room (a write), a channel
    /// waiting for it moves the words now. Transfers take no time, so a request's blocks are in
    /// memory (or on the card) as soon as both the command and the channel are under way.
    fn sdhost_dma(&mut self) {
        use crate::sdhost::DREQ;
        if !self.sd.dreq() || !self.dma.ch.iter().any(|c| c.cs & bcm2711::dma::ACTIVE != 0 && bcm2711::dma::paced(c.ti) == Some(DREQ)) { return; }
        let mut dma = std::mem::take(&mut self.dma);
        if dma.serve(DREQ, self) { self.dirty = true; }
        self.dma = dma;
    }

    /// Plug the monitor in or out: its EDID on the DDC bus, the HDMI block's hot-plug status,
    /// and the hot-plug line the driver reads (GPIO 28 on most models, line 4 of the firmware's
    /// GPIO expander on the 3B; both active low).
    pub fn set_hdmi_connected(&mut self, model: Model, on: bool) {
        let Some(v) = self.vc4.as_mut() else { return };
        v.connected = on;
        let mut eeprom = bcm2711::vc4::EdidEeprom::new(v.edid().to_vec());
        eeprom.connected = on;
        self.ddc.devices = vec![Box::new(eeprom)];
        if model.hpd_on_expander() {
            if on { self.mbox.exp_gpio_in &= !(1 << 4) } else { self.mbox.exp_gpio_in |= 1 << 4 }
        } else {
            self.gpio.set_input(28, Some(!on));
        }
        self.dirty = true;
    }

    /// The firmware GPIO expander's power lines, after a mailbox request: WL_ON powers the
    /// Wi-Fi chip (off, it leaves the SDIO bus; on again, it is a fresh chip whose firmware
    /// must be downloaded again), BT_ON the Bluetooth core (off, it is reset and hears nothing).
    pub(crate) fn power_lines(&mut self) {
        if !self.wireless { return; }
        let wl = self.mbox.exp_gpio_out & (1 << EXP_WL_ON) != 0;
        if wl != self.sdhci.sdio.is_some() {
            self.sdhci.sdio = wl.then(|| {
                let mut c = bcm2711::wifi::Cyw43455::new();
                c.mac = WIFI_MAC;
                c.set_trace(self.wifi_trace);
                Box::new(c) as Box<dyn SdioCard>
            });
        }
        if !self.bt_on() {
            if let Some(bt) = self.bt.as_mut().filter(|b| b.commands > 0 || !b.tx.is_empty()) {
                let trace = bt.trace;
                *bt = bcm2711::bluetooth::Bluetooth::new();
                bt.trace = trace;
            }
        }
    }

    fn bt_on(&self) -> bool { self.mbox.exp_gpio_out & (1 << EXP_BT_ON) != 0 }

    /// Bluetooth on UART0: what the host sent goes to the controller (while BT_ON powers it),
    /// the controller's bytes fill the receive FIFO as it empties.
    pub(crate) fn bt_pump(&mut self) {
        let on = self.bt_on();
        let Some(bt) = self.bt.as_mut() else { return };
        if !self.uart.out.is_empty() { let b = std::mem::take(&mut self.uart.out); if on { bt.receive(&b); } }
        while self.uart.rx_room() { match bt.tx.pop_front() { Some(b) => self.uart.push_rx(b), None => break } }
    }

    /// USB time: frames (SOF), and held transfers whose devices have data now (and the
    /// Bluetooth controller's advertising reports).
    pub(crate) fn usb_tick(&mut self, now: u64) {
        if let Some(bt) = self.bt.as_mut() { bt.tick(now); }
        self.bt_pump();
        if self.usb.tick(now) { self.dirty = true; }
        if self.usb.held() != 0 {
            self.usb.poll(&mut self.ram, &mut TreeBus { tree: &mut self.tree, devs: &mut self.devs });
        }
    }

    /// Exchange frames between the USB Ethernet and the virtual network, and between the
    /// associated Wi-Fi interface and the access point's network.
    pub(crate) fn pump_network(&mut self, now: u64) {
        let now_us = now / 1000;
        if let (Some(card), Some(wnet)) = (self.sdhci.sdio.as_mut(), self.wifi_net.as_mut()) {
            let mut replies = Vec::new();
            for f in card.take_tx() { replies.extend(wnet.handle(&f, now_us)); }
            replies.extend(wnet.poll(now_us));
            for r in replies { card.deliver(&r); }
        }
        let Some(lan) = self.tree.lan() else { return };
        match self.net.as_mut() {
            Some(net) => {
                let mut replies = Vec::new();
                for f in lan.take_tx() { replies.extend(net.handle(&f, now_us)); }
                replies.extend(net.poll(now_us));
                for f in replies { lan.receive(&f); }
            }
            None => { lan.take_tx(); }
        }
    }

    /// What the display shows, as RGBA: the HVS's composition while the KMS driver drives HDMI
    /// or composite, else the firmware framebuffer.
    pub fn render(&self, out: &mut Vec<u8>) -> Option<(u32, u32)> {
        if let Some(v) = self.vc4.as_ref().filter(|v| v.active()) { return v.render(&self.ram, out); }
        self.mbox.render(&self.ram, out)
    }
    /// The mode shown right now (None while the display is off).
    pub fn shown_size(&self) -> Option<(u32, u32)> {
        match self.vc4.as_ref() { Some(v) if v.active() => v.size(), _ => self.mbox.fb.allocated.then(|| self.mbox.fb_size()) }
    }
    /// The size the display has (or last had): what a VNC viewer and the pointer scale to.
    pub fn display_size(&self) -> (u32, u32) { self.vc4.as_ref().and_then(|v| v.size()).unwrap_or_else(|| self.mbox.fb_size()) }

    fn note(&mut self, a: u64) {
        let page = a & !0xfff;
        if !self.unknown.contains(&page) {
            self.unknown.push(page);
            eprintln!("[pi3] unmodelled peripheral access at {a:#x}");
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

/// DMA bus addresses: the peripherals at 0x7e.., RAM through any of the VideoCore's aliases
/// (Linux uses 0xc000_0000, the device tree's `dma-ranges`).
pub(crate) fn dma_phys(a: u32) -> u64 {
    if (0x7e00_0000..0x7f00_0000).contains(&a) { (a - 0x3f00_0000) as u64 } else { (a & 0x3fff_ffff) as u64 }
}

impl bcm2711::dma::DmaBus for Soc {
    fn phys(&self, bus: u32) -> u64 { dma_phys(bus) }
    fn dreq(&self, dreq: u32) -> bool { dreq == crate::sdhost::DREQ && self.sd.dreq() }
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
    pub model: Model,
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
    /// a debugger's breakpoints (interpreter only, see `set_debugging`)
    pub breakpoints: emu_core::debug::Breakpoints,
    /// the deadline of the last idle stop (`idle_until` wakes sleeping cores there, not before)
    idle_target: u64,
    /// run the cores on their own host threads (faster with several busy cores, not repeatable);
    /// false: one thread, cores in turns, deterministic
    pub multi: bool,
    #[cfg(not(target_arch = "wasm32"))]
    pub(crate) mt: Option<crate::mt::Mt>,
}

/// Counter ticks to the first virtual ns at which the 19.2 MHz counter reaches them.
pub(crate) fn ns_of_count(c: u64) -> u64 { (c / 12 * 625) + (c % 12 * 625).div_ceil(12) }

/// The earliest enabled timer deadline of a core (ns).
pub(crate) fn timer_deadline(cpu: &Cpu) -> Option<u64> {
    let s = &cpu.sys;
    [(s.vtimer, s.cntvoff), (s.ptimer, 0)].iter().filter(|(t, _)| t.ctl & 3 == 1).map(|(t, off)| ns_of_count(t.cval.wrapping_add(*off))).min()
}

impl Machine {
    /// A Pi 3 of `model` with `ram` bytes of SDRAM (the board's own size: `model.ram_mb()`; at
    /// most 1 GiB, all the SoC addresses) and `ncpu` cores (the board has 4); only core 0 starts.
    pub fn new(model: Model, ram: usize, ncpu: usize) -> Self {
        let ncpu = ncpu.clamp(1, 4);
        let ram = ram.min(1 << 30);
        // the ARM sees RAM up to the peripheral window; the VideoCore's share is at the top
        let ram_arm_visible = ram.min(PERIPH as usize);
        let gpu = (model.gpu_mb() << 20).min(ram / 4);
        let arm_ram = (ram - gpu) as u64;
        let fb_max = (ram_arm_visible as u64).saturating_sub(arm_ram);
        let mut mbox = Mailbox::new(arm_ram, fb_max, 1024, 768);
        mbox.revision = model.revision();
        // the expander as the firmware leaves it: the LAN chip running, the power LED on, the
        // Wi-Fi chip powered (the vendor tree has no power sequence for it), Bluetooth off until
        // `hci_bcm` drives BT_ON; the levels the lines read back while Linux has not made them
        // outputs are the same
        mbox.exp_gpio_out = 1 << EXP_WL_ON | 1 << EXP_PWR_LED | 1 << EXP_LAN_RUN;
        mbox.exp_gpio_in = mbox.exp_gpio_out;
        let soc = Soc {
            ram: vec![0; ram_arm_visible], arm_ram, cur: 0, local: LocalIntc::new(), armctrl: ArmCtrl::new(), uart: Pl011::new(), aux: AuxUart::new(),
            mbox, sd: Sdhost::new(None), sdhci: Sdhci::new(None), wireless: model.wireless(), wifi_trace: false, wifi_net: None, bt: None,
            rng: crate::periph::Rng::new(), thermal: crate::periph::Thermal::new(), pm: Pm::new(), v3d: vc4_3d::V3d::new(),
            usb: { let mut d = Dwc2::new(); d.port = None; d }, devs: Devices::default(), tree: UsbTree::new(), net: None, asb: [0; 12],
            gpio: bcm2711::gpio::Gpio::new(), dma: bcm2711::dma::Dma::new(), pace_next: None, ddc: bcm2711::i2c::I2c::with_devices(Vec::new()), vc4: None,
            outputs: Default::default(),
            audio: Vec::new(), audio_rate: 0, capture_audio: false, vchiq: crate::vchiq::Vchiq::new(), now: 0,
            unknown: Vec::new(), dirty: true,
        };
        let mut m = Machine { model, cpus: Vec::new(), state: vec![CoreState::Off; ncpu], soc, now: 0, exceptions: 0, irqs: 0, cur: 0, next_check: 0,
                              round_end: u64::MAX, breakpoints: Default::default(), idle_target: 0, multi: false,
                              #[cfg(not(target_arch = "wasm32"))] mt: None };
        m.soc.tree = usb_tree(model, &mut m.soc.devs);
        // the firmware clocks the BCM2837's firmware lists for Linux: ARM (cpufreq, one operating
        // point at the board's clock), core, V3D, pixel (HDMI's pixel clock), M2MC (the HDMI
        // state machine), VEC (composite, 108 MHz)
        m.soc.mbox.clocks = &[3, 4, 5, 9, 13, 15];
        m.soc.mbox.rates[3] = model.arm_mhz() * 1_000_000;
        m.soc.mbox.vchiq = true;
        // nothing on HDMI's hot-plug line until a monitor is plugged in (`enable_kms`)
        m.soc.gpio.set_input(28, Some(true));
        m.soc.mbox.exp_gpio_in |= 1 << 4;
        m.soc.power_lines();
        for k in 0..ncpu { let c = m.fresh_cpu(k); m.cpus.push(c); }
        m.state[0] = CoreState::Run;
        m
    }

    /// The KMS display pipeline (`dtoverlay=vc4-kms-v3d`) with a monitor whose preferred mode
    /// is `w`x`h` plugged into HDMI, and the outputs the device tree lists (the overlay's
    /// `composite` adds the VEC).
    pub fn enable_kms(&mut self, w: u32, h: u32, outputs: bcm2711::boot::KmsOutputs) {
        self.soc.vc4 = Some(bcm2711::vc4::Vc4::bcm2835(w, h));
        self.soc.outputs = outputs;
        self.soc.set_hdmi_connected(self.model, true);
    }

    /// The VideoCore IV 3D block is modelled (`vc4-3d`): the device tree lists it next to the
    /// display pipeline, for vc4's DRM device to bind as one of its components.
    pub fn has_v3d(&self) -> bool { true }

    /// Plug the HDMI monitor in or out (the driver polls the hot-plug line).
    pub fn set_hdmi_connected(&mut self, on: bool) { let m = self.model; self.soc.set_hdmi_connected(m, on); }

    /// Plug the Ethernet port into sbc-net's virtual network (DHCP, DNS); `nat` also relays
    /// guest TCP/UDP to the host's network (native builds only). Boards without Ethernet
    /// ignore it. The Wi-Fi's access points get a network of their own (isolated: DHCP, DNS
    /// and local services), as on the Pi 4.
    pub fn connect_network(&mut self, nat: bool, log: bool) {
        if self.soc.tree.lan_ref().is_some() {
            let mut net = sbc_net::net::VirtualNet::new(log);
            #[cfg(not(target_arch = "wasm32"))]
            if nat { net.nat = Some(sbc_net::nat::Nat::new(log)); }
            self.soc.net = Some(net);
        }
        let _ = nat;
        if self.soc.wireless { self.soc.wifi_net = Some(sbc_net::net::VirtualNet::new(log)); }
    }

    /// Bluetooth on UART0, as on the board (`--no-bluetooth` leaves UART0 a serial port).
    pub fn enable_bluetooth(&mut self) { if self.soc.wireless { self.soc.bt = Some(bcm2711::bluetooth::Bluetooth::new()); } }

    /// Log the Wi-Fi chip's host interface (`--trace-wifi`) and the SDIO commands (`--trace-sdio`).
    pub fn trace_wifi(&mut self, wifi: bool, sdio: bool) {
        self.soc.wifi_trace = wifi;
        self.soc.sdhci.trace = sdio;
        if let Some(c) = self.soc.sdhci.sdio.as_mut() { c.set_trace(wifi); }
    }

    /// Relay host TCP address `host` to the guest's TCP `guest_port` (QEMU's hostfwd); needs
    /// `connect_network` with NAT first.
    #[cfg(not(target_arch = "wasm32"))]
    pub fn forward_tcp(&mut self, host: std::net::SocketAddr, guest_port: u16) -> Result<(), String> {
        let nat = self.soc.net.as_mut().and_then(|n| n.nat.as_mut()).ok_or("port forwarding needs --net nat")?;
        nat.add_forward(host, guest_port).map_err(|e| format!("{host}: {e}"))
    }

    fn fresh_cpu(&mut self, k: usize) -> Cpu {
        let mut cpu = Cpu::new();
        cpu.profile = CpuProfile::CORTEX_A53;
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

    pub fn load(&mut self, addr: u64, data: &[u8]) {
        self.soc.ram[addr as usize..addr as usize + data.len()].copy_from_slice(data);
    }

    /// The serial console's input (the mini UART); false if its FIFO is full.
    pub fn serial_input(&mut self, b: u8) -> bool {
        if !self.soc.aux.rx_room() { return false; }
        self.soc.aux.push_rx(b);
        self.soc.dirty = true;
        true
    }
    pub fn serial_output(&self) -> &[u8] { &self.soc.aux.out }
    pub fn take_serial_output(&mut self) -> Vec<u8> { std::mem::take(&mut self.soc.aux.out) }

    /// Physical inputs: the SoC temperature (`cpu-temp`, degrees C).
    pub fn set_sensor(&mut self, what: &str, v: f64) {
        if matches!(what, "cpu-temp" | "soc-temp") { self.soc.thermal.temp_mc = (v * 1000.0) as i32; self.soc.mbox.temp_mc = (v * 1000.0).max(0.0) as u32; }
    }

    /// Linux arm64 boot protocol: enter at EL1h (as on the Pi 4 machine), MMU off, interrupts
    /// masked, x0 = DTB.
    pub fn boot_linux(&mut self, entry: u64, dtb: u64) {
        let cpu = &mut self.cpus[0];
        cpu.el = 1;
        cpu.spsel = true;
        cpu.daif = 0xf << 6;
        cpu.pc = entry;
        cpu.x = [0; 31];
        cpu.x[0] = dtb;
        cpu.sys.cpacr_el1 = 3 << 20;
    }

    /// Move core `k`'s clock to `t` if it is behind (a sleeping core's time follows the machine).
    fn catch_up(&mut self, k: usize, t: u64) {
        let c = &mut self.cpus[k];
        if c.now_ns() < t { c.sys.time_base_ns = t; c.sys.time_base_instret = c.instret; }
    }

    /// Make core `k` the running one: it takes the shared JIT; the core that stops running
    /// loses its exclusive reservation.
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
            self.soc.local.set_timers(k, p, v);
        }
        self.soc.pump_network(now);
        self.soc.usb_tick(now);
        self.soc.device_lines(now);
        for k in 0..self.cpus.len() {
            if self.state[k] == CoreState::Off { continue; }
            let irq = self.soc.irq_out(k);
            self.cpus[k].set_irq(irq, false);
            if irq && matches!(self.state[k], CoreState::Wfi | CoreState::Wfe) {
                // woken: from now on the cores take turns
                self.state[k] = CoreState::Run;
                self.round_end = self.round_end.min(t + quantum());
            }
        }
        self.soc.dirty = false;
        // next forced re-evaluation: the earliest deadline still ahead, at most 1 ms ahead
        let horizon = t + 1_000_000;
        let due = self.deadline_after(t, c).unwrap_or(horizon).min(horizon);
        self.next_check = self.cpus[c].instret + (due - t);
    }

    /// The earliest deadline after `t` that core `c` running alone must notice: its own timers,
    /// those of sleeping cores, devices.
    fn deadline_after(&self, t: u64, c: usize) -> Option<u64> {
        let mut d: Option<u64> = self.soc.next_event().filter(|&x| x > t);
        for (k, cpu) in self.cpus.iter().enumerate() {
            if self.state[k] == CoreState::Off || (k != c && self.state[k] == CoreState::Run) { continue; }
            if let Some(ns) = timer_deadline(cpu).filter(|&ns| ns > t) { d = Some(d.map_or(ns, |x| x.min(ns))); }
        }
        d
    }

    /// Next timer (of any core that is on) or device deadline in ns.
    fn next_deadline(&self) -> Option<u64> {
        let mut d = self.soc.next_event();
        for (k, cpu) in self.cpus.iter().enumerate() {
            if self.state[k] == CoreState::Off { continue; }
            let s = &cpu.sys;
            for (t, off) in [(s.vtimer, s.cntvoff), (s.ptimer, 0)] {
                if t.ctl & 3 == 1 {
                    let ns = ns_of_count(t.cval.wrapping_add(off));
                    // an expired timer whose interrupt is off wakes nobody: it must not stop time
                    if ns > self.now { d = Some(d.map_or(ns, |x: u64| x.min(ns))); }
                }
            }
        }
        d
    }

    /// Run until the cores have retired `budget` instructions between them (see the Pi 4's
    /// `Machine::run`: rounds of 20 µs while several cores run, one core alone without rounds).
    pub fn run(&mut self, budget: u64) -> Stop {
        #[cfg(not(target_arch = "wasm32"))]
        if self.multi && self.breakpoints.at.is_empty() && !self.cpus.iter().any(|c| c.jit_off || !c.watch.is_empty()) {
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
            let (last, n) = (self.breakpoints.last_turn(self.cur), self.cpus.len());
            let watching = !self.cpus[0].watch.is_empty();
            for i in 0..n {
                let k = self.breakpoints.turn(i, last, n, watching);
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
        let breaks = !self.breakpoints.at.is_empty();
        loop {
            let t = self.cpus[k].now_ns();
            if t >= self.round_end || *total >= end || *steps == 0 { return None; }
            if breaks && self.breakpoints.hit(k, self.cpus[k].pc) { return Some(Stop::Breakpoint(k)); }
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

    /// PSCI 1.1 (the emulated firmware's), called by core `k`.
    fn psci(&mut self, k: usize) -> Option<Stop> {
        const NOT_SUPPORTED: u64 = -1i64 as u64;
        const INVALID_PARAMETERS: u64 = -2i64 as u64;
        const ALREADY_ON: u64 = -4i64 as u64;
        let x = self.cpus[k].x;
        let target = |m: &Self| { let t = (x[1] & 0xff_00ff_ffff) as usize; (t < m.cpus.len()).then_some(t) };
        let r = match x[0] as u32 {
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
        let jit_off = self.cpus[caller].jit_off;
        let c = &mut self.cpus[t];
        Self::reset_core(c, t, el, entry, context, now);
        c.jit_off = jit_off;
        self.state[t] = CoreState::Run;
        self.round_end = self.round_end.min(now + quantum());
        self.soc.dirty = true;
    }

    /// Core `k` out of reset at `entry` (PSCI CPU_ON): RAM, retired count and JIT are kept.
    pub(crate) fn reset_core(c: &mut Cpu, k: usize, el: u8, entry: u64, context: u64, now: u64) {
        let mut f = Cpu::new();
        f.profile = CpuProfile::CORTEX_A53;
        f.sys.cntfrq = CNTFRQ;
        f.sys.mpidr = k as u64;
        f.firmware_calls_to_host = true;
        f.ram = c.ram;
        f.ram_len = c.ram_len;
        f.code_pages = c.code_pages.clone();
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
        f.watch = c.watch.clone();
        f.mt = c.mt;
        f.wall_clock = c.wall_clock;
        let mut old = std::mem::replace(c, f);
        old.hand_jit_to(c);
    }

    /// Debugging: interpreter only on all cores (needed while breakpoints are set).
    pub fn set_debugging(&mut self, on: bool) { for c in self.cpus.iter_mut() { c.jit_off = on; } }

    /// Debugging: set or clear a breakpoint at `addr` (every core).
    pub fn breakpoint(&mut self, addr: u64, insert: bool) {
        self.breakpoints.set(addr, insert);
        let on = !self.breakpoints.at.is_empty() || !self.cpus[0].watch.is_empty();
        self.set_debugging(on);
    }

    /// Debugging: set or clear (`insert` false) a watchpoint on [addr, addr + len) for
    /// accesses of `kind` (1 write, 2 read, 3 either), on every core.
    pub fn watchpoint(&mut self, addr: u64, len: u64, kind: u8, insert: bool) {
        for c in self.cpus.iter_mut() {
            c.watch.retain(|&w| w != (addr, len, kind));
            if insert { c.watch.push((addr, len, kind)); }
        }
        let on = !self.breakpoints.at.is_empty() || !self.cpus[0].watch.is_empty();
        self.set_debugging(on);
    }

    /// Debugging: execute one instruction on core `k` (interpreter); a stop it causes, if any.
    pub fn debug_step(&mut self, k: usize) -> Option<Stop> {
        if self.state[k] == CoreState::Off { return None; }
        self.state[k] = CoreState::Run;
        let jit_off = self.cpus[k].jit_off;
        self.cpus[k].jit_off = true;
        let turn = self.breakpoints.last_turn(self.cur);
        self.breakpoints.begin_step(k, self.cpus[k].pc);
        self.round_end = u64::MAX;
        let (mut total, mut steps, mut storm) = (self.instret(), 1, 0);
        let t = self.cpus[k].now_ns().max(self.now);
        let r = self.slice(k, t, &mut total, u64::MAX, &mut steps, &mut storm);
        self.breakpoints.end_step(k);
        self.breakpoints.keep_turn(turn);
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
        for c in self.cpus.iter_mut() { c.jit_invalidate(u64::MAX); }
        true
    }

    /// Advance idle time to `t` (after an `Idle` stop) and re-evaluate interrupts.
    pub fn idle_until(&mut self, t: u64) {
        if t > self.now { self.now = t; }
        let now = self.now;
        // waking happens at the idle stop's own deadline only (runs stay repeatable)
        let reached = now >= self.idle_target;
        for k in 0..self.cpus.len() {
            if self.state[k] == CoreState::Off { continue; }
            self.catch_up(k, now);
            if reached && self.state[k] == CoreState::Wfe { self.state[k] = CoreState::Run; }
        }
        self.update_irqs();
        if !reached { return; }
        // an idle stop ends at a deadline, and `wfi` may wake spuriously: a core whose own
        // timer deadline has passed resumes
        for k in 0..self.cpus.len() {
            if self.state[k] != CoreState::Wfi { continue; }
            if timer_deadline(&self.cpus[k]).is_some_and(|d| d <= now) { self.state[k] = CoreState::Run; }
        }
    }
}

const SNAP_MAGIC: &[u8; 8] = b"SBCPI3S1";

impl Machine {
    /// Serialize the whole machine (deflate-compressed; RAM keeps only non-zero pages).
    pub fn save(&self) -> Result<Vec<u8>, String> {
        let mut w = emu_core::snap::W::default();
        w.tag("pi3-machine-v6");
        w.u8(self.model as u8);
        w.u64(self.now); w.u64(self.exceptions); w.u64(self.irqs);
        w.u32(self.cpus.len() as u32);
        for (c, st) in self.cpus.iter().zip(&self.state) {
            w.u8(match st { CoreState::Off => 0, CoreState::Run => 1, CoreState::Wfi => 2, CoreState::Wfe => 3 });
            c.save_state(&mut w);
        }
        self.soc.local.save_state(&mut w);
        self.soc.armctrl.save_state(&mut w);
        self.soc.uart.save_state(&mut w);
        self.soc.aux.save_state(&mut w);
        self.soc.mbox.save_state(&mut w);
        self.soc.sd.save_state(&mut w);
        // the expander's lines (the Wi-Fi chip's power), the Wi-Fi's host and chip, Bluetooth
        w.u8(self.soc.mbox.exp_gpio_out); w.u8(self.soc.mbox.exp_gpio_in);
        w.bool(self.soc.sdhci.sdio.is_some());
        self.soc.sdhci.save_state(&mut w);
        w.bool(self.soc.bt.is_some());
        if let Some(bt) = self.soc.bt.as_ref() { bt.save_state(&mut w); }
        self.soc.rng.save_state(&mut w);
        self.soc.pm.save_state(&mut w);
        self.soc.usb.save_state(&mut w);
        { use emu_core::snap::Snap; self.soc.devs.save(&mut w); self.soc.tree.save(&mut w); }
        for r in self.soc.asb { w.u32(r); }
        // GPIO, DMA, the display pipeline
        self.soc.gpio.save_state(&mut w);
        self.soc.dma.save_state(&mut w);
        w.bool(self.soc.vc4.is_some());
        if let Some(v) = self.soc.vc4.as_ref() { v.save_state(&mut w); }
        // VCHIQ and the VideoCore's services, the 3D GPU
        self.soc.vchiq.save_state(&mut w);
        self.soc.v3d.save_state(&mut w);
        emu_core::snap::save_ram(&mut w, &self.soc.ram);
        let mut out = SNAP_MAGIC.to_vec();
        out.extend_from_slice(&miniz_oxide::deflate::compress_to_vec(&w.0, 1));
        Ok(out)
    }

    /// Restore a snapshot into a machine made with the same options (model, RAM, cores, card).
    pub fn restore(&mut self, snap: &[u8]) -> Result<(), String> {
        if snap.len() < 8 || &snap[..8] != SNAP_MAGIC { return Err("not a Raspberry Pi 3 snapshot".into()); }
        let raw = miniz_oxide::inflate::decompress_to_vec(&snap[8..]).map_err(|e| format!("the snapshot is damaged ({:?})", e.status))?;
        let e = |e: emu_core::snap::SnapError| e.0;
        let mut r = emu_core::snap::R::new(&raw);
        // v6: the card on sdhost, the wireless chips, VCHIQ and the VideoCore's services, the 3D
        // GPU (older layouts had the card on the Arasan, no VCHIQ or no V3D)
        r.tag("pi3-machine-v6").map_err(e)?;
        let model = r.u8().map_err(e)?;
        if model != self.model as u8 { return Err(format!("the snapshot is of another model (this machine: --model {})", self.model.id())); }
        let (now, exceptions, irqs) = (r.u64().map_err(e)?, r.u64().map_err(e)?, r.u64().map_err(e)?);
        let n = r.u32().map_err(e)? as usize;
        if n != self.cpus.len() { return Err(format!("snapshot has {n} cores, machine has {} (use --smp {n})", self.cpus.len())); }
        let mut jit = self.cpus[self.cur].jit.take();
        if let Some(j) = jit.as_mut() { j.flush(); }
        (self.now, self.exceptions, self.irqs) = (now, exceptions, irqs);
        self.cur = 0;
        self.soc.cur = 0;
        for k in 0..n {
            let mut c = self.fresh_cpu(k);
            c.jit_off = self.cpus[k].jit_off;
            c.watch = std::mem::take(&mut self.cpus[k].watch);
            self.cpus[k] = c;
            self.state[k] = match r.u8().map_err(e)? { 0 => CoreState::Off, 2 => CoreState::Wfi, 3 => CoreState::Wfe, _ => CoreState::Run };
            self.cpus[k].load_state(&mut r).map_err(e)?;
        }
        self.cpus[0].jit = jit;
        self.soc.local.load_state(&mut r).map_err(e)?;
        self.soc.armctrl.load_state(&mut r).map_err(e)?;
        self.soc.uart.load_state(&mut r).map_err(e)?;
        self.soc.aux.load_state(&mut r).map_err(e)?;
        self.soc.mbox.load_state(&mut r).map_err(e)?;
        self.soc.sd.load_state(&mut r).map_err(e)?;
        self.soc.mbox.exp_gpio_out = r.u8().map_err(e)?; self.soc.mbox.exp_gpio_in = r.u8().map_err(e)?;
        let wifi_on = r.bool().map_err(e)?;
        if wifi_on && !self.soc.wireless { return Err("the snapshot has a Wi-Fi chip this machine lacks".into()); }
        // the chip as the snapshot has it: powered (its state follows) or not
        self.soc.sdhci.sdio = None;
        self.soc.power_lines();
        if wifi_on != self.soc.sdhci.sdio.is_some() { return Err("the snapshot's Wi-Fi power does not match its expander".into()); }
        self.soc.sdhci.load_state(&mut r).map_err(e)?;
        if r.bool().map_err(e)? {
            let bt = self.soc.bt.get_or_insert_with(bcm2711::bluetooth::Bluetooth::new);
            bt.load_state(&mut r).map_err(e)?;
        } else {
            self.soc.bt = None;
        }
        self.soc.rng.load_state(&mut r).map_err(e)?;
        self.soc.pm.load_state(&mut r).map_err(e)?;
        self.soc.usb.load_state(&mut r, &mut self.soc.devs.kbd, false).map_err(e)?;
        { use emu_core::snap::Snap; self.soc.devs.load(&mut r).map_err(e)?; self.soc.tree.load(&mut r).map_err(e)?; }
        for x in self.soc.asb.iter_mut() { *x = r.u32().map_err(e)?; }
        self.soc.gpio.load_state(&mut r).map_err(e)?;
        self.soc.dma.load_state(&mut r).map_err(e)?;
        let kms = r.bool().map_err(e)?;
        if kms != self.soc.vc4.is_some() {
            return Err(format!("the snapshot was taken with --display {} (this machine: --display {})", if kms { "kms" } else { "fb" }, if kms { "fb" } else { "kms" }));
        }
        if let Some(v) = self.soc.vc4.as_mut() { v.load_state(&mut r).map_err(e)?; }
        // the DDC bus's EEPROM follows the restored monitor
        if let Some(on) = self.soc.vc4.as_ref().map(|v| v.connected) { let m = self.model; self.soc.set_hdmi_connected(m, on); }
        self.soc.pace_next = Some(0);
        self.soc.vchiq = crate::vchiq::Vchiq::new();
        self.soc.vchiq.load_state(&mut r).map_err(e)?;
        self.soc.v3d.load_state(&mut r).map_err(e)?;
        emu_core::snap::load_ram(&mut r, &mut self.soc.ram).map_err(e)?;
        for c in self.cpus.iter_mut() { c.ram = self.soc.ram.as_mut_ptr(); c.ram_len = self.soc.ram.len() as u64; }
        self.next_check = 0;
        self.soc.dirty = true;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use emu_core::disk::{Disk, Overlay};

    /// An image that stays unchanged under a copy-on-write overlay, as `--sd` without
    /// `--sd-persist` opens it; the overlay is shared with the test so it can look at it.
    struct Cow(std::rc::Rc<std::cell::RefCell<Overlay>>);
    impl Disk for Cow {
        fn sectors(&self) -> u64 { 2048 }
        fn read(&mut self, lba: u64, out: &mut [u8]) { out.fill(0xaa); self.0.borrow().apply(lba, out); }
        fn write(&mut self, lba: u64, data: &[u8]) { self.0.borrow_mut().store(lba, data, 2048); }
        fn overlay(&self) -> Vec<(u64, Vec<u8>)> { self.0.borrow().sorted() }
        fn restore_overlay(&mut self, sectors: Vec<(u64, Vec<u8>)>) { self.0.borrow_mut().replace(sectors, 2048); }
    }

    /// A snapshot of a copy-on-write card carries what the guest wrote: a restore brings those
    /// sectors back and drops the ones written after the snapshot (else the restored guest's
    /// page cache and the card disagree).
    #[test]
    fn snapshots_carry_the_cards_written_sectors() {
        let ov = std::rc::Rc::new(std::cell::RefCell::new(Overlay::default()));
        let mut m = Machine::new(Model::B3Plus, 128 << 20, 1);
        m.soc.sd = Sdhost::new(Some(Box::new(Cow(ov.clone()))));
        m.soc.sd.disk.as_mut().unwrap().write(7, &[1; 512]);
        let snap = m.save().unwrap();
        m.soc.sd.disk.as_mut().unwrap().write(7, &[2; 512]);
        m.soc.sd.disk.as_mut().unwrap().write(9, &[3; 512]);
        m.restore(&snap).unwrap();
        assert_eq!(ov.borrow().sorted(), vec![(7, vec![1; 512])]);
        // into a fresh machine on the same image (another run): the sectors come with it
        let ov2 = std::rc::Rc::new(std::cell::RefCell::new(Overlay::default()));
        let mut m2 = Machine::new(Model::B3Plus, 128 << 20, 1);
        m2.soc.sd = Sdhost::new(Some(Box::new(Cow(ov2.clone()))));
        m2.restore(&snap).unwrap();
        let mut s = [0u8; 512];
        m2.soc.sd.disk.as_mut().unwrap().read(7, &mut s);
        assert_eq!(s, [1; 512]);
    }
}
