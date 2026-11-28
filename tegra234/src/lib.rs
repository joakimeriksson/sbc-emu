//! Tegra234 direct-boot machine for early Linux bring-up.
pub mod boot;
mod bpmp;
pub mod gpu;
mod host1x;
mod hsp;
mod mc;
mod mss_nvlink;
#[cfg(not(target_arch = "wasm32"))]
mod mt;
mod uart;

use arm_a64::{Cpu, CpuProfile};
use emu_core::{Bus, Core, Fault, Stop, Trap};
use peripherals::{gicv3::Gic, pl011::Pl011};
use uart::UartA;

pub const RAM_BASE: u64 = 0x8000_0000;
pub const DISPLAY_SIZE: (u32, u32) = (640, 360);
const FRAMEBUFFER: u64 = 0x8ff0_0000;
const FRAMEBUFFER_RESERVATION: u64 = 1 << 20;
const APB_MISC: u64 = 0x0010_0000;
const APB_STRAPS: u64 = 0x0010_f000;
const EFUSE: u64 = 0x0381_0000;
const EFUSE_SIZE: u64 = 0x1_0000;
const CHIP_ID_A01: u32 = 0x0001_2310;
// Use NVIDIA's QT platform ID for the GPU model, where nvgpu skips the silicon PMU.
const CHIP_ID_A01_QT: u32 = CHIP_ID_A01 | (1 << 20);
const GICD: u64 = 0x0f40_0000;
const GICD_SIZE: u64 = 0x1_0000;
const GICR: u64 = 0x0f44_0000;
const GICR_SIZE: u64 = 0x20_0000;
const UART: u64 = 0x031d_0000;
const UART_SIZE: u64 = 0x1_0000;
const UART_SPI: u32 = 285;
const UARTA: u64 = 0x0310_0000;
const UARTA_SPI: u32 = 112;
const COUNTER_HZ: u64 = 31_250_000;
/// The xHCI host's place on the real chip (Orin TRM: XUSB_HOST at 0x03610000, SPI 163). Here it is
/// sbc-emu's shared xHCI core behind Linux's generic platform driver, without NVIDIA's XUSB
/// firmware, pad control or falcon.
pub const XHCI: u64 = 0x0361_0000;
pub const XHCI_SIZE: u64 = 0x1_0000;
pub const XHCI_SPI: u32 = 163;

#[derive(Default)]
struct Firmware {
    hsp: hsp::Hsp,
    bpmp: bpmp::Bpmp,
}

struct Soc {
    ram: Vec<u8>,
    gic: Gic,
    uart: Pl011,
    uarta: UartA,
    gpu: gpu::Gpu,
    host1x: host1x::Host1x,
    mss_nvlink: mss_nvlink::MssNvlink,
    gpu_node: bool,
    firmware: Option<Firmware>,
    xhci: Option<peripherals::xhci::Xhci>,
    usb: emu_core::usb::Devices,
    /// the network behind the USB Ethernet adapter, and when it is next polled
    net: Option<sbc_net::net::VirtualNet>,
    net_next: u64,
    now: u64,
    interrupts_changed: bool,
}

impl Soc {
    fn ram_offset(&self, addr: u64, size: usize) -> Option<usize> {
        let offset = usize::try_from(addr.checked_sub(RAM_BASE)?).ok()?;
        (offset.checked_add(size)? <= self.ram.len()).then_some(offset)
    }

    fn device_read(&mut self, addr: u64) -> Result<u32, Fault> {
        Ok(if (GICD..GICD + GICD_SIZE).contains(&addr) {
            self.gic.dist_read(addr - GICD)
        } else if (GICR..GICR + GICR_SIZE).contains(&addr) {
            self.gic.redist_read(addr - GICR)
        } else if addr == APB_MISC + 4 {
            if self.gpu_node {
                CHIP_ID_A01_QT
            } else {
                CHIP_ID_A01
            }
        } else if addr == APB_STRAPS {
            0
        } else if self.gpu_node && (EFUSE..EFUSE + EFUSE_SIZE).contains(&addr) {
            // Production mode in the shadow array. HIDREV supplies the SoC ID.
            u32::from(addr == EFUSE + 0x100)
        } else if let Some(value) = mc::read(addr) {
            value
        } else if let Some(value) = self.mss_nvlink.read(addr) {
            value
        } else if let Some(xhci) = self.xhci.as_mut().filter(|_| (XHCI..XHCI + XHCI_SIZE).contains(&addr)) {
            self.interrupts_changed = true;
            xhci.read(addr - XHCI)
        } else if (UART..UART + UART_SIZE).contains(&addr) {
            self.interrupts_changed = true;
            self.uart.read(addr - UART)
        } else if (UARTA..UARTA + UART_SIZE).contains(&addr) {
            self.interrupts_changed = true;
            self.uarta.read(addr - UARTA, self.now)
        } else if addr == uart::FIXTURE_RESET && self.firmware.is_none() {
            self.uarta
                .reset_asserted()
                .map(u32::from)
                .ok_or(Fault::Unmapped)?
        } else if (hsp::BASE..hsp::BASE + hsp::SIZE).contains(&addr) {
            self.firmware
                .as_ref()
                .ok_or(Fault::Unmapped)?
                .hsp
                .read(addr - hsp::BASE)
        } else if self.gpu_node && host1x::contains(addr) {
            self.host1x.read(addr)
        } else if self.gpu_bar_accessible(addr, gpu::BAR0) {
            self.gpu.read(addr - gpu::BAR0, self.now)
        } else if self.gpu_bar_accessible(addr, gpu::BAR1) {
            self.gpu.bar1_read(addr - gpu::BAR1, &mut self.ram)
        } else {
            return Err(Fault::Unmapped);
        })
    }

    fn device_write(&mut self, addr: u64, value: u32) -> Result<(), Fault> {
        if (GICD..GICD + GICD_SIZE).contains(&addr) {
            self.gic.dist_write(addr - GICD, value);
        } else if (GICR..GICR + GICR_SIZE).contains(&addr) {
            self.gic.redist_write(addr - GICR, value);
        } else if let Some(xhci) = self.xhci.as_mut().filter(|_| (XHCI..XHCI + XHCI_SIZE).contains(&addr)) {
            self.interrupts_changed = true;
            let mut dma = peripherals::dma::Dma { ram: &mut self.ram, base: RAM_BASE };
            xhci.write(addr - XHCI, value, &mut dma, &mut self.usb);
        } else if (UART..UART + UART_SIZE).contains(&addr) {
            self.uart.write(addr - UART, value);
        } else if (UARTA..UARTA + UART_SIZE).contains(&addr) {
            self.uarta.write(addr - UARTA, value, self.now);
        } else if addr == uart::FIXTURE_RESET && self.uarta.high_speed() && self.firmware.is_none()
        {
            self.uarta.set_reset(value & 1 != 0);
        } else if (hsp::BASE..hsp::BASE + hsp::SIZE).contains(&addr) {
            let firmware = self.firmware.as_mut().ok_or(Fault::Unmapped)?;
            if firmware.hsp.write(addr - hsp::BASE, value)
                && firmware.bpmp.service(&mut self.uarta, &mut self.gpu)
            {
                firmware.hsp.ring_ccplex();
            }
        } else if self.mss_nvlink.write(addr, value) {
            return Ok(());
        } else if self.gpu_node && host1x::contains(addr) {
            self.host1x.write(addr, value);
        } else if self.gpu_bar_accessible(addr, gpu::BAR0) {
            self.gpu.write(
                addr - gpu::BAR0,
                value,
                &mut self.ram,
                &mut self.host1x,
                self.now,
            );
        } else if self.gpu_bar_accessible(addr, gpu::BAR1) {
            self.gpu.bar1_write(addr - gpu::BAR1, value, &mut self.ram);
        } else {
            return Err(Fault::Unmapped);
        }
        self.interrupts_changed = true;
        Ok(())
    }

    fn gpu_bar_accessible(&self, addr: u64, base: u64) -> bool {
        (base..base + gpu::SIZE).contains(&addr)
            && self
                .firmware
                .as_ref()
                .is_none_or(|firmware| firmware.bpmp.gpu_available())
    }

    /// The devices' time-driven work at `now` (the network, the xHCI host) and their interrupt
    /// lines to the GIC; the cores' timers are the machine's.
    fn service(&mut self, now: u64) {
        if let Some(firmware) = &self.firmware {
            self.gic.set_line((hsp::SPI + 32) as usize, firmware.hsp.irq());
        }
        self.pump_network(now);
        if let Some(xhci) = self.xhci.as_mut() {
            let mut dma = peripherals::dma::Dma { ram: &mut self.ram, base: RAM_BASE };
            xhci.tick(now, &mut dma, &mut self.usb);
            let irq = xhci.irq();
            self.gic.set_line((XHCI_SPI + 32) as usize, irq);
        }
        self.gic.set_line((UART_SPI + 32) as usize, self.uart.irq());
        self.gic.set_line((UARTA_SPI + 32) as usize, self.uarta.irq(now));
        self.gic.set_line((gpu::GSP_STALL_SPI + 32) as usize, self.gpu.stall_irq());
        self.gic.set_line((gpu::NONSTALL_SPI + 32) as usize, self.gpu.nonstall_irq());
        if self.gpu_node {
            let mask = self.host1x.irq_mask();
            for index in 0..host1x::IRQ_COUNT {
                self.gic.set_line(host1x::FIRST_IRQ + index, mask & (1 << index) != 0);
            }
        }
    }

    /// The devices' next timed event after `now` (ns): the rover UART's receive timeout, xHCI
    /// work, the network's next poll.
    fn next_event(&self, now: u64) -> Option<u64> {
        self.uarta
            .next_event(now)
            .into_iter()
            .chain(self.xhci.as_ref().and_then(|x| x.next_event(&self.usb)))
            .chain((self.net.is_some() && self.usb.net.as_ref().is_some_and(|n| n.up())).then_some(self.net_next))
            .min()
    }

    /// Exchange frames between the USB Ethernet adapter and the network: what the guest sent goes
    /// out at once, the network is polled every millisecond with NAT (10 ms without).
    fn pump_network(&mut self, now: u64) {
        let (Some(net), Some(dev)) = (self.net.as_mut(), self.usb.net.as_mut()) else { return };
        let now_us = now / 1000;
        for frame in dev.take_tx() {
            dev.deliver(net.handle(&frame, now_us));
        }
        if now >= self.net_next {
            dev.deliver(net.poll(now_us));
            self.net_next = now + if net.nat.is_some() { 1_000_000 } else { 10_000_000 };
        }
    }

    fn sram_offset(addr: u64, size: usize) -> Option<usize> {
        let offset = addr.checked_sub(bpmp::SRAM)?;
        (offset.checked_add(size as u64)? <= bpmp::SRAM_SIZE).then_some(offset as usize)
    }

    fn read<const N: usize>(&mut self, addr: u64) -> Result<[u8; N], Fault> {
        let mut bytes = [0; N];
        if let Some(offset) = self.ram_offset(addr, N) {
            bytes.copy_from_slice(&self.ram[offset..offset + N]);
        } else if let Some(offset) = Self::sram_offset(addr, N) {
            let firmware = self.firmware.as_ref().ok_or(Fault::Unmapped)?;
            bytes.copy_from_slice(&firmware.bpmp.sram[offset..offset + N]);
        } else if N == 8 && addr.is_multiple_of(8) {
            bytes[..4].copy_from_slice(&self.device_read(addr)?.to_le_bytes());
            bytes[4..].copy_from_slice(&self.device_read(addr + 4)?.to_le_bytes());
        } else if N <= 4 && (addr as usize & 3) + N <= 4 {
            let word = self.device_read(addr & !3)?.to_le_bytes();
            let offset = addr as usize & 3;
            bytes.copy_from_slice(&word[offset..offset + N]);
        } else {
            return Err(Fault::Unmapped);
        }
        Ok(bytes)
    }

    fn write<const N: usize>(&mut self, addr: u64, bytes: [u8; N]) -> Result<(), Fault> {
        if let Some(offset) = self.ram_offset(addr, N) {
            self.ram[offset..offset + N].copy_from_slice(&bytes);
        } else if let Some(offset) = Self::sram_offset(addr, N) {
            let firmware = self.firmware.as_mut().ok_or(Fault::Unmapped)?;
            firmware.bpmp.sram[offset..offset + N].copy_from_slice(&bytes);
        } else if N == 8 && addr.is_multiple_of(8) {
            self.device_write(addr, u32::from_le_bytes(bytes[..4].try_into().unwrap()))?;
            self.device_write(addr + 4, u32::from_le_bytes(bytes[4..].try_into().unwrap()))?;
        } else if N <= 4 && (addr as usize & 3) + N <= 4 {
            let aligned = addr & !3;
            let offset = addr as usize & 3;
            let mut word = if N == 4
                || (UART..UART + UART_SIZE).contains(&addr)
                || (UARTA..UARTA + UART_SIZE).contains(&addr)
            {
                [0; 4]
            } else {
                // Linux uses byte writes for the GIC priority registers.
                self.device_read(aligned)?.to_le_bytes()
            };
            word[offset..offset + N].copy_from_slice(&bytes);
            self.device_write(aligned, u32::from_le_bytes(word))?;
        } else {
            return Err(Fault::Unmapped);
        }
        Ok(())
    }
}

macro_rules! bus_access {
    ($read:ident, $write:ident, $ty:ty) => {
        fn $read(&mut self, addr: u64) -> Result<$ty, Fault> {
            self.read(addr).map(<$ty>::from_le_bytes)
        }

        fn $write(&mut self, addr: u64, value: $ty) -> Result<(), Fault> {
            self.write(addr, value.to_le_bytes())
        }
    };
}

impl Bus for Soc {
    bus_access!(read8, write8, u8);
    bus_access!(read16, write16, u16);
    bus_access!(read32, write32, u32);
    bus_access!(read64, write64, u64);

    fn read_sysreg(&mut self, key: u32) -> Option<u64> {
        let result = self.gic.sys_read(key);
        self.interrupts_changed |= result.is_some();
        result
    }

    fn write_sysreg(&mut self, key: u32, value: u64) -> bool {
        let handled = self.gic.sys_write(key, value);
        self.interrupts_changed |= handled;
        handled
    }
}

// Observed on an Orin Nano running all six cores. Aff2 selects the cluster.
pub const AFFINITIES: [u64; 6] = [0, 0x100, 0x200, 0x300, 0x10200, 0x10300];
const QUANTUM_NS: u64 = 20_000;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CoreState {
    Off,
    Running,
    Wfi,
    Wfe,
}

pub struct Machine {
    pub cpus: Vec<Cpu>,
    pub state: Vec<CoreState>,
    soc: Soc,
    current: usize,
    now: u64,
    breakpoint: emu_core::debug::Breakpoints,
    exception_streak: Vec<u32>,
    pub interrupts: u64,
    display: bool,
    /// run the cores on their own host threads (`--threads multi`: faster, not repeatable;
    /// `mt.rs`); a debugger's breakpoints, watchpoints and steps run on one thread
    pub multi: bool,
    #[cfg(not(target_arch = "wasm32"))]
    mt: Option<mt::Mt>,
}

impl Machine {
    pub fn new(ram_bytes: usize, cores: usize) -> Result<Self, String> {
        Self::with_uart_mode(ram_bytes, cores, false)
    }

    pub fn with_uart_mode(
        ram_bytes: usize,
        cores: usize,
        high_speed_uart: bool,
    ) -> Result<Self, String> {
        if !(256 << 20..=4096u64 << 20).contains(&(ram_bytes as u64)) {
            return Err("Orin Nano direct boot needs 256–4096 MiB of RAM".into());
        }
        if !(1..=AFFINITIES.len()).contains(&cores) {
            return Err("Orin Nano requires --smp between 1 and 6".into());
        }
        let mut soc = Soc {
            ram: vec![0; ram_bytes],
            gic: Gic::with_affinities_and_intids(&AFFINITIES[..cores], 512),
            uart: Pl011::new(),
            uarta: UartA::new(high_speed_uart),
            gpu: gpu::Gpu::default(),
            host1x: host1x::Host1x::default(),
            mss_nvlink: Default::default(),
            gpu_node: false,
            firmware: None,
            xhci: None,
            usb: Default::default(),
            net: None,
            net_next: 0,
            now: 0,
            interrupts_changed: true,
        };
        let code_pages = Cpu::code_page_map(ram_bytes as u64);
        let cpus = AFFINITIES[..cores]
            .iter()
            .map(|&affinity| {
                let mut cpu = Cpu::new();
                cpu.profile = CpuProfile::CORTEX_A78AE;
                cpu.sys.mpidr = 0x0100_0000 | affinity;
                cpu.sys.cntfrq = COUNTER_HZ;
                cpu.ram = soc.ram.as_mut_ptr();
                cpu.ram_len = ram_bytes as u64;
                cpu.ram_base = RAM_BASE;
                cpu.code_pages = Some(code_pages.clone());
                cpu.firmware_calls_to_host = true;
                cpu
            })
            .collect();
        let mut state = vec![CoreState::Off; cores];
        state[0] = CoreState::Running;
        Ok(Self {
            cpus,
            state,
            soc,
            current: 0,
            now: 0,
            breakpoint: Default::default(),
            exception_streak: vec![0; cores],
            interrupts: 0,
            display: false,
            multi: false,
            #[cfg(not(target_arch = "wasm32"))]
            mt: None,
        })
    }

    pub fn enable_bpmp(&mut self) {
        self.soc.firmware.get_or_insert_with(Firmware::default);
    }

    pub fn enable_gpu(&mut self) {
        self.enable_bpmp();
        self.soc.gpu_node = true;
    }

    /// An xHCI host with nothing plugged in; `attach_usb_drive` and `attach_camera` add devices.
    pub fn enable_usb(&mut self) {
        if self.soc.xhci.is_none() {
            self.soc.xhci = Some(peripherals::xhci::Xhci::new(peripherals::xhci::Layout { usb2: 0..4, usb3: 4..8 }));
            self.plug_usb();
        }
    }

    pub fn usb_enabled(&self) -> bool {
        self.soc.xhci.is_some()
    }

    /// Plug a USB drive in (port 1).
    pub fn attach_usb_drive(&mut self, disk: Box<dyn emu_core::disk::Disk + Send>) {
        self.soc.usb.storage = Some(emu_core::usb::Storage::new(disk));
        self.enable_usb();
        self.plug_usb();
    }

    /// Plug a USB webcam in (port 2).
    pub fn attach_camera(&mut self, camera: emu_core::usb::Camera) {
        self.soc.usb.camera = Some(camera);
        self.enable_usb();
        self.plug_usb();
    }

    /// Plug a USB Ethernet adapter (CDC-ECM, port 3) into sbc-net's virtual network (DHCP, DNS,
    /// ICMP, as on the other boards); `nat` also relays guest TCP/UDP to the host's network
    /// (native builds only).
    pub fn connect_usb_network(&mut self, nat: bool, log: bool) {
        #[allow(unused_mut)]
        let mut net = sbc_net::net::VirtualNet::new(log);
        #[cfg(not(target_arch = "wasm32"))]
        if nat {
            net.nat = Some(sbc_net::nat::Nat::new(log));
        }
        let _ = nat;
        self.soc.net = Some(net);
        self.soc.usb.net = Some(emu_core::usb::Net::new());
        self.enable_usb();
        self.plug_usb();
    }

    /// Forward a host TCP address to a guest port (needs `connect_usb_network` with NAT).
    pub fn forward_tcp(&mut self, host: std::net::SocketAddr, guest_port: u16) -> Result<(), String> {
        #[cfg(not(target_arch = "wasm32"))]
        {
            let nat = self.soc.net.as_mut().and_then(|n| n.nat.as_mut()).ok_or("port forwarding needs the USB network with NAT")?;
            nat.add_forward(host, guest_port).map_err(|e| format!("{host}: {e}"))
        }
        #[cfg(target_arch = "wasm32")]
        {
            let _ = (host, guest_port);
            Err("port forwarding needs host sockets".into())
        }
    }

    /// Plug a USB serial port in (CDC-ACM, port 4: `/dev/ttyACM0`).
    pub fn attach_usb_serial(&mut self, serial: emu_core::usb::Serial) {
        self.soc.usb.serial = Some(serial);
        self.enable_usb();
        self.plug_usb();
    }

    /// Bytes for the guest on the USB serial port (dropped while the guest has not opened it).
    pub fn usb_serial_input(&mut self, bytes: &[u8]) {
        if let Some(s) = self.soc.usb.serial.as_mut() {
            s.push_rx(bytes);
        }
    }

    /// What the guest wrote to the USB serial port since the last call.
    pub fn take_usb_serial_output(&mut self) -> Vec<u8> {
        self.soc.usb.serial.as_mut().map(|s| s.take_tx()).unwrap_or_default()
    }

    fn plug_usb(&mut self) {
        use emu_core::usb::DevId;
        let devs = [
            self.soc.usb.storage.is_some().then_some(DevId::Storage),
            self.soc.usb.camera.is_some().then_some(DevId::Camera),
            self.soc.usb.net.is_some().then_some(DevId::Net),
            self.soc.usb.serial.is_some().then_some(DevId::Serial),
        ];
        if let Some(xhci) = self.soc.xhci.as_mut() {
            xhci.attach(&devs);
        }
    }

    pub fn enable_display(&mut self) {
        self.display = true;
    }

    pub fn display_size(&self) -> Option<(u32, u32)> {
        self.display.then_some(DISPLAY_SIZE)
    }

    pub fn render(&self, out: &mut Vec<u8>) -> Option<(u32, u32)> {
        let (width, height) = self.display_size()?;
        let start = (FRAMEBUFFER - RAM_BASE) as usize;
        let size = (width * height * 4) as usize;
        out.resize(size, 0);
        for (output, input) in out
            .chunks_exact_mut(4)
            .zip(self.soc.ram[start..start + size].chunks_exact(4))
        {
            output.copy_from_slice(&[input[2], input[1], input[0], 255]);
        }
        Some((width, height))
    }

    pub fn gpu_submissions(&self) -> u64 {
        self.soc.gpu.submissions()
    }

    pub fn set_jit(&mut self, enabled: bool) -> bool {
        for cpu in &mut self.cpus {
            cpu.jit_off = !enabled;
        }
        enabled && self.cpus[self.current].enable_jit()
    }

    pub fn current_core(&self) -> usize {
        self.current
    }

    pub fn now(&self) -> u64 {
        self.now
    }

    pub fn instret(&self) -> u64 {
        self.cpus.iter().map(|cpu| cpu.instret).sum()
    }

    pub fn serial_room(&self) -> bool {
        self.soc.uart.rx_room()
    }

    pub fn serial_input(&mut self, byte: u8) -> bool {
        if !self.serial_room() {
            return false;
        }
        self.soc.uart.push_rx(byte);
        self.soc.interrupts_changed = true;
        true
    }

    pub fn serial_output(&self) -> &[u8] {
        &self.soc.uart.out
    }

    pub fn take_serial_output(&mut self) -> Vec<u8> {
        std::mem::take(&mut self.soc.uart.out)
    }

    /// Deliver a byte to the rover's UART, retaining it at the peer on backpressure.
    pub fn rover_uart_input(&mut self, byte: u8) -> bool {
        let accepted = self.soc.uarta.push_rx(byte, self.now);
        self.soc.interrupts_changed |= accepted;
        accepted
    }

    pub fn take_rover_uart_output(&mut self) -> Vec<u8> {
        self.soc.uarta.take_output()
    }

    fn next_event(&self) -> Option<u64> {
        self.cpus
            .iter()
            .zip(&self.state)
            .filter(|(_, state)| **state != CoreState::Off)
            .flat_map(|(cpu, _)| {
                let sys = &cpu.sys;
                [(sys.ptimer, 0), (sys.vtimer, sys.cntvoff)]
            })
            .filter(|(timer, _)| timer.ctl & 3 == 1)
            // 31.25 MHz is one count per 32 ns.
            .map(|(timer, offset)| timer.cval.wrapping_add(offset).saturating_mul(32))
            .filter(|&deadline| deadline > self.now)
            .chain(self.soc.next_event(self.now))
            .min()
    }

    fn update_interrupts(&mut self) {
        self.soc.service(self.now);
        for (core, (cpu, state)) in self.cpus.iter_mut().zip(&mut self.state).enumerate() {
            if *state == CoreState::Off {
                continue;
            }
            Self::catch_up(cpu, self.now);
            cpu.refresh_counter();
            let sys = &mut cpu.sys;
            sys.timers_changed = false;
            self.soc.gic.set_ppi(core, 30, sys.ptimer.firing(sys.cnt));
            self.soc.gic.set_ppi(
                core,
                27,
                sys.vtimer.firing(sys.cnt.wrapping_sub(sys.cntvoff)),
            );
            let irq = self.soc.gic.irq_out(core);
            cpu.set_irq(irq, false);
            if irq {
                *state = CoreState::Running;
            }
        }
        self.soc.interrupts_changed = false;
    }

    fn catch_up(cpu: &mut Cpu, now: u64) {
        if now > cpu.now_ns() {
            cpu.sys.time_base_ns = now;
            cpu.sys.time_base_instret = cpu.instret;
        }
    }

    pub fn idle_until(&mut self, deadline: u64) {
        self.now = self.now.max(deadline);
        self.update_interrupts();
    }

    fn switch_to(&mut self, core: usize) {
        if self.current != core {
            let previous = self.current;
            let [old, next] = self.cpus.get_disjoint_mut([previous, core]).unwrap();
            old.hand_jit_to(next);
            old.clear_exclusive();
            self.current = core;
            self.soc.gic.cur = core;
        }
        Self::catch_up(&mut self.cpus[core], self.now);
    }

    /// Run up to `budget` instructions over all cores (with `multi`, about 10 ms of host time
    /// on a thread per core instead).
    pub fn run(&mut self, budget: u64) -> Stop {
        let debugging = !self.breakpoint.at.is_empty() || !self.cpus[0].watch.is_empty();
        #[cfg(not(target_arch = "wasm32"))]
        if self.multi && !debugging {
            return self.run_threaded(std::time::Duration::from_millis(10));
        }
        self.single_thread();
        let mut remaining = budget;
        let mut attempts = budget;
        while remaining != 0 && attempts != 0 {
            self.update_interrupts();
            // WFE is also allowed to return spuriously. Giving a parked core a
            // new turn lets it recheck the condition after another core runs.
            for state in &mut self.state {
                if *state == CoreState::Wfe {
                    *state = CoreState::Running;
                }
            }
            let count = self.cpus.len();
            let next = (1..=count)
                .map(|offset| (self.current + offset) % count)
                .find(|&core| {
                    self.state[core] == CoreState::Running && self.breakpoint.may_run(core)
                });
            let Some(core) = next else {
                let deadline = self
                    .next_event()
                    .unwrap_or(self.now.saturating_add(10_000_000));
                return Stop::Idle(deadline);
            };
            self.switch_to(core);
            let slice_end = self.now.saturating_add(QUANTUM_NS);
            let mut deadline = self.next_event().unwrap_or(u64::MAX);
            while self.state[core] == CoreState::Running
                && self.now < slice_end
                && remaining != 0
                && attempts != 0
            {
                if self.soc.interrupts_changed
                    || self.cpus[core].sys.timers_changed
                    || self.now >= deadline
                {
                    self.update_interrupts();
                    deadline = self.next_event().unwrap_or(u64::MAX);
                }
                let cpu = &mut self.cpus[core];
                if self.breakpoint.hit(core, cpu.pc) {
                    return Stop::Breakpoint(core);
                }
                let before = cpu.instret;
                let max_instructions = deadline.min(slice_end).saturating_sub(self.now);
                cpu.chain_until = before.saturating_add(remaining.min(max_instructions));
                attempts -= 1;
                let stop = self.execute(core, debugging);
                remaining = remaining.saturating_sub(self.cpus[core].instret - before);
                if let Some(stop) = stop {
                    return stop;
                }
            }
        }
        Stop::Budget
    }

    /// Back on one thread after a threaded run (a debugger is attached): turn-taking
    /// semantics again.
    fn single_thread(&mut self) {
        if self.cpus[0].mt {
            for cpu in &mut self.cpus {
                cpu.mt = false;
            }
        }
    }

    fn execute(&mut self, core: usize, interpreting: bool) -> Option<Stop> {
        let jit_off = self.cpus[core].jit_off;
        self.cpus[core].jit_off |= interpreting;
        self.soc.now = self.now;
        let result = self.cpus[core].step(&mut self.soc);
        let watch_hit = self.cpus[core].watch_hit.take();
        self.cpus[core].jit_off = jit_off;
        self.now = self.now.max(self.cpus[core].now_ns());
        if let Some(op) = self.cpus[core].tlbi_bcast.take() {
            for (index, cpu) in self.cpus.iter_mut().enumerate() {
                if index != core {
                    cpu.tlb_invalidate(op);
                }
            }
        }
        if let Some(op) = self.cpus[core].ic_bcast.take() {
            for (index, cpu) in self.cpus.iter_mut().enumerate() {
                if index != core {
                    cpu.jit_invalidate(op);
                }
            }
        }
        match result {
            Ok(()) => {
                self.exception_streak[core] = 0;
                if let Some((addr, kind)) = watch_hit {
                    return Some(Stop::Watchpoint(core, addr, kind));
                }
            }
            Err(Trap::Interrupt(..)) => {
                self.interrupts += 1;
                self.soc.interrupts_changed = true;
            }
            Err(Trap::Exception(..)) => {
                self.exception_streak[core] += 1;
                if self.exception_streak[core] >= 1_000_000 {
                    self.exception_streak[core] = 0;
                    let el = self.cpus[core].el as usize;
                    return Some(Stop::ExceptionStorm {
                        pc: self.cpus[core].pc,
                        esr: self.cpus[core].esr[el],
                        elr: self.cpus[core].elr[el],
                        far: self.cpus[core].far[el],
                        el: self.cpus[core].el,
                    });
                }
            }
            Err(Trap::Wait) if !self.cpus[core].irq => {
                self.state[core] = if self.cpus[core].last_wfe {
                    CoreState::Wfe
                } else {
                    CoreState::Wfi
                };
            }
            Err(Trap::Wait) => {}
            Err(Trap::HostCall(_)) => return self.psci(core),
            Err(Trap::Unimplemented(pc, word)) => return Some(Stop::Unimplemented(pc, word)),
            Err(Trap::Hlt(code)) => return Some(Stop::Hlt(code)),
            Err(Trap::Brk(code)) => return Some(Stop::Brk(code)),
        }
        None
    }

    /// The PSCI core a call's target affinity names.
    fn psci_target(cores: usize, arg: u64) -> Option<usize> {
        let affinity = arg & 0xff_00ff_ffff;
        AFFINITIES[..cores].iter().position(|&value| value == affinity)
    }

    /// The PSCI calls that need no core or device state: the result for `id` (shared with the
    /// threaded machine's PSCI).
    fn psci_simple(id: u32, arg: u64) -> u64 {
        const NOT_SUPPORTED: u64 = u64::MAX;
        fn supported(id: u32) -> bool {
            matches!(
                id,
                0x8000_0000
                    | 0x8400_0000
                    | 0x8400_0002
                    | 0x8400_0003
                    | 0xc400_0003
                    | 0x8400_0004
                    | 0xc400_0004
                    | 0x8400_0008
                    | 0x8400_0009
                    | 0x8400_000a
            )
        }
        match id {
            0x8000_0000 | 0x8400_0000 => 0x1_0001, // SMCCC / PSCI 1.1
            0x8400_000a if supported(arg as u32) => 0,
            _ => NOT_SUPPORTED,
        }
    }

    fn psci(&mut self, core: usize) -> Option<Stop> {
        const INVALID_PARAMETERS: u64 = -2i64 as u64;
        const ALREADY_ON: u64 = -4i64 as u64;
        let [id, arg, entry, context, ..] = self.cpus[core].x;
        let target = Self::psci_target(self.cpus.len(), arg);
        self.cpus[core].x[0] = match id as u32 {
            0x8400_0002 => {
                self.state[core] = CoreState::Off;
                self.cpus[core].set_irq(false, false);
                self.cpus[core].clear_exclusive();
                return None;
            }
            0x8400_0003 | 0xc400_0003 => match target {
                None => INVALID_PARAMETERS,
                Some(_) if self.soc.ram_offset(entry, 4).is_none() || entry % 4 != 0 => {
                    INVALID_PARAMETERS
                }
                Some(target) if self.state[target] != CoreState::Off => ALREADY_ON,
                Some(target) => {
                    self.start_core(target, entry, context);
                    0
                }
            },
            0x8400_0004 | 0xc400_0004 if entry == 0 => target
                .map_or(INVALID_PARAMETERS, |target| {
                    u64::from(self.state[target] == CoreState::Off)
                }),
            0x8400_0004 | 0xc400_0004 => INVALID_PARAMETERS,
            0x8400_0008 => return Some(Stop::PowerOff),
            0x8400_0009 => return Some(Stop::Reset),
            id => Self::psci_simple(id, arg),
        };
        None
    }

    fn start_core(&mut self, core: usize, entry: u64, context: u64) {
        Self::reset_core(&mut self.cpus[core], entry, context, self.now);
        self.exception_streak[core] = 0;
        self.state[core] = CoreState::Running;
        self.soc.interrupts_changed = true;
    }

    /// Core `old` as PSCI CPU_ON leaves it: at `entry` in EL1 with `context` in X0, its clock
    /// at `now`; it keeps its JIT (threaded runs give each core its own), watchpoints and the
    /// threaded machine's settings.
    fn reset_core(old: &mut Cpu, entry: u64, context: u64, now: u64) {
        let mut cpu = Cpu::new();
        cpu.profile = old.profile;
        cpu.ram = old.ram;
        cpu.ram_base = old.ram_base;
        cpu.ram_len = old.ram_len;
        cpu.code_pages = old.code_pages.clone();
        cpu.jit_off = old.jit_off;
        cpu.watch = std::mem::take(&mut old.watch);
        cpu.instret = old.instret;
        cpu.sys.mpidr = old.sys.mpidr;
        cpu.sys.cntfrq = COUNTER_HZ;
        cpu.sys.time_base_ns = now;
        cpu.sys.time_base_instret = cpu.instret;
        cpu.firmware_calls_to_host = true;
        cpu.el = 1;
        cpu.spsel = true;
        cpu.daif = 0x3c0;
        cpu.x[0] = context;
        cpu.pc = entry;
        cpu.mt = old.mt;
        cpu.wall_clock = old.wall_clock;
        old.hand_jit_to(&mut cpu);
        *old = cpu;
    }

    pub fn debug_breakpoint(&mut self, addr: u64, insert: bool) {
        self.breakpoint.set(addr, insert);
    }

    pub fn debug_watch(&mut self, addr: u64, len: u64, kind: u8, insert: bool) {
        for cpu in &mut self.cpus {
            cpu.watch.retain(|&watch| watch != (addr, len, kind));
            if insert {
                cpu.watch.push((addr, len, kind));
            }
        }
    }

    pub fn debug_step(&mut self, core: usize) -> Option<Stop> {
        if self.state[core] == CoreState::Off {
            return None;
        }
        self.single_thread();
        self.switch_to(core);
        self.state[core] = CoreState::Running;
        self.update_interrupts();
        self.execute(core, true)
    }

    fn debug_pages(&mut self, core: usize, addr: u64, len: usize) -> Option<Vec<(usize, usize)>> {
        let mut pages = Vec::new();
        let mut done = 0;
        while done < len {
            let va = addr.checked_add(done as u64)?;
            let pa = self.cpus[core].debug_translate(&mut self.soc, va)?;
            let size = (len - done).min(0x1000 - (va as usize & 0xfff));
            pages.push((self.soc.ram_offset(pa, size)?, size));
            done += size;
        }
        Some(pages)
    }

    pub fn debug_read(&mut self, core: usize, addr: u64, len: usize) -> Option<Vec<u8>> {
        let mut data = Vec::new();
        for (offset, size) in self.debug_pages(core, addr, len)? {
            data.extend_from_slice(&self.soc.ram[offset..offset + size]);
        }
        Some(data)
    }

    pub fn debug_write(&mut self, core: usize, addr: u64, data: &[u8]) -> bool {
        let Some(pages) = self.debug_pages(core, addr, data.len()) else {
            return false;
        };
        let mut written = 0;
        for (offset, size) in pages {
            self.soc.ram[offset..offset + size].copy_from_slice(&data[written..written + size]);
            written += size;
        }
        for cpu in &mut self.cpus {
            cpu.jit_invalidate(u64::MAX);
        }
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct BpmpClient {
        tx: u64,
        rx: u64,
        trigger: u64,
        pending: u64,
        count: u32,
    }

    impl BpmpClient {
        fn new(soc: &mut Soc, channel: u64) -> Self {
            let dim = soc.read32(hsp::BASE + 0x380).unwrap();
            assert_ne!(dim & 15, 0); // The Linux mailbox core requires a nonempty controller.
            let bank = (1 + (dim & 15) / 2 + ((dim >> 4) & 15) + ((dim >> 8) & 15)) as u64;
            let ccplex = hsp::BASE + bank * 0x10000 + 0x100;
            let bpmp_db = ccplex + 0x200;
            assert_ne!(soc.read32(bpmp_db + 4).unwrap() & (1 << 17), 0);
            soc.write32(ccplex + 4, 1 << hsp::BPMP_MASTER).unwrap();
            let offset = channel * 256;
            let tx = bpmp::SRAM + bpmp::TX_POOL as u64 + offset;
            let rx = bpmp::SRAM + bpmp::RX_POOL as u64 + offset;

            // Publish SYNC, see the remote ACK, then publish ESTABLISHED.
            soc.write32(tx + 4, 1).unwrap();
            soc.write32(bpmp_db, 1).unwrap();
            assert_eq!(soc.read32(rx + 4).unwrap(), 2);
            assert_eq!(soc.read32(ccplex + 12).unwrap(), 1 << hsp::BPMP_MASTER);
            soc.write32(ccplex + 12, 1 << hsp::BPMP_MASTER).unwrap();
            soc.write32(tx + 4, 0).unwrap();
            soc.write32(bpmp_db, 1).unwrap();
            assert_eq!(soc.read32(rx + 4).unwrap(), 0);
            Self {
                tx,
                rx,
                trigger: bpmp_db,
                pending: ccplex + 12,
                count: 0,
            }
        }

        fn call(&mut self, soc: &mut Soc, mrq: u32, words: &[u32], irq: bool) -> i32 {
            soc.write32(self.tx + 128, mrq).unwrap();
            soc.write32(self.tx + 132, 1 | if irq { 2 } else { 0 })
                .unwrap();
            for (index, value) in words.iter().enumerate() {
                soc.write32(self.tx + 136 + 4 * index as u64, *value)
                    .unwrap();
            }

            self.count = self.count.wrapping_add(1);
            soc.write32(self.tx, self.count).unwrap();
            soc.write32(self.trigger, 1).unwrap();
            assert_eq!(soc.read32(self.tx + 64).unwrap(), self.count);
            assert_eq!(soc.read32(self.rx).unwrap(), self.count);
            assert_eq!(
                soc.read32(self.pending).unwrap(),
                if irq { 1 << hsp::BPMP_MASTER } else { 0 }
            );
            soc.write32(self.pending, 1 << hsp::BPMP_MASTER).unwrap();
            let status = soc.read32(self.rx + 128).unwrap() as i32;
            soc.write32(self.rx + 64, self.count).unwrap();
            status
        }
    }

    #[test]
    fn bpmp_routes_atomic_and_threaded_messages_through_hsp_and_ivc() {
        let mut machine = Machine::with_uart_mode(256 << 20, 1, true).unwrap();
        machine.enable_bpmp();
        let mut atomic = BpmpClient::new(&mut machine.soc, 3);

        // Restart the receive queue and check that its doorbell reaches the GIC.
        let receive_state = bpmp::SRAM + bpmp::TX_POOL as u64 + 13 * 256 + 4;
        let intid = hsp::SPI + 32;
        let gic_pending = GICD + 0x200 + u64::from(intid / 32) * 4;
        let gic_mask = 1 << (intid % 32);
        machine.soc.write32(receive_state, 1).unwrap();
        machine.soc.write32(atomic.trigger, 1).unwrap();
        machine.update_interrupts();
        assert_ne!(machine.soc.read32(gic_pending).unwrap() & gic_mask, 0);
        machine
            .soc
            .write32(atomic.pending, 1 << hsp::BPMP_MASTER)
            .unwrap();
        machine.soc.write32(receive_state, 0).unwrap();
        machine.soc.write32(atomic.trigger, 1).unwrap();
        machine.update_interrupts();
        assert_eq!(machine.soc.read32(gic_pending).unwrap() & gic_mask, 0);

        let soc = &mut machine.soc;
        assert_eq!(atomic.call(soc, 0, &[41], false), 0);
        assert_eq!(soc.read32(atomic.rx + 136).unwrap(), 82);
        assert_eq!(atomic.call(soc, 23, &[71], false), 0);
        assert_eq!(soc.read32(atomic.rx + 136).unwrap(), 0);
        assert_eq!(atomic.call(soc, 71, &[], false), 0);
        assert_eq!(soc.read32(atomic.rx + 136).unwrap().to_le_bytes(), *b"sbc-");

        let mut threaded = BpmpClient::new(soc, 0);
        // Program UART A for a 9600-baud clock and verify the same rate is returned.
        assert_eq!(
            threaded.call(soc, 22, &[(2 << 24) | bpmp::CLK_UARTA, 0, 153_600, 0], true),
            0
        );
        assert_eq!(soc.read64(threaded.rx + 136).unwrap(), 153_600);
        assert_eq!(
            threaded.call(soc, 22, &[(1 << 24) | bpmp::CLK_UARTA], true),
            0
        );
        assert_eq!(soc.read64(threaded.rx + 136).unwrap(), 153_600);

        // Check that the firmware reset changes the physical UART block state.
        soc.write32(UARTA + 0x0c, 3).unwrap();
        assert_eq!(atomic.call(soc, 20, &[1, bpmp::RESET_UARTA], false), 0);
        assert_eq!(soc.read32(UARTA + 0x0c).unwrap(), 0);
        assert!(!soc.uarta.push_rx(b'a', 0));
        assert_eq!(atomic.call(soc, 20, &[2, bpmp::RESET_UARTA], false), 0);
        assert!(soc.uarta.push_rx(b'a', 0));
        assert_eq!(soc.read32(UARTA).unwrap(), b'a' as u32);
        assert_eq!(atomic.call(soc, 0xffff, &[], false), -3);

        // The GPU register aperture is available exactly while its domain is on
        // and the firmware has released its reset.
        assert_eq!(soc.read32(gpu::BAR0), Err(Fault::Unmapped));
        assert_eq!(threaded.call(soc, 66, &[1, 35, 1], true), 0);
        assert_eq!(soc.read32(gpu::BAR0), Ok(0x17b0_00a1));
        let ring_command = gpu::BAR0 + 0x12004c;
        let ring_result = gpu::BAR0 + 0x120050;
        assert_eq!(soc.read32(ring_result), Ok(0));
        soc.write32(ring_command, 4).unwrap();
        assert_eq!(soc.read32(ring_command), Ok(0));
        assert_eq!(soc.read32(ring_result), Ok(1));
        assert_eq!(atomic.call(soc, 20, &[1, 19], false), 0);
        assert_eq!(soc.read32(gpu::BAR0), Err(Fault::Unmapped));
        assert_eq!(atomic.call(soc, 20, &[2, 19], false), 0);
        assert_eq!(soc.read32(gpu::BAR0), Ok(0x17b0_00a1));
        assert_eq!(soc.read32(ring_result), Ok(0));
        soc.write32(ring_command, 4).unwrap();
        assert_eq!(atomic.call(soc, 20, &[3, 19], false), 0);
        assert_eq!(soc.read32(ring_result), Ok(0));
        assert_eq!(threaded.call(soc, 66, &[1, 35, 0], true), 0);
        assert_eq!(soc.read32(gpu::BAR0), Err(Fault::Unmapped));
    }

    #[test]
    fn ga10b_engine_discovery_and_fecs_startup_use_consistent_hardware_state() {
        let mut machine = Machine::new(256 << 20, 1).unwrap();
        let soc = &mut machine.soc;
        let reg = |offset| gpu::BAR0 + offset;

        let discovery = soc.read32(reg(0x0224fc)).unwrap();
        assert_eq!(discovery, (6 << 20) | 2);
        let runlist = u64::from(soc.read32(reg(0x022808)).unwrap() & !0x3);
        assert_eq!(soc.read32(reg(runlist + 0x10)).unwrap(), 0x8004_0000);
        assert_eq!(soc.read32(reg(runlist + 0x08)).unwrap() >> 16, 1);
        let copy_runlist = u64::from(soc.read32(reg(0x022814)).unwrap() & !0x3);
        assert_ne!(runlist, copy_runlist);
        assert_eq!(soc.read32(reg(copy_runlist + 0x228)).unwrap() >> 24, 1);
        assert_eq!(soc.read32(reg(copy_runlist + 0x08)).unwrap() >> 16, 0);
        for status in [0x040108, 0x040148, runlist + 0x220, copy_runlist + 0x220] {
            soc.write32(reg(status), u32::MAX).unwrap();
            assert_eq!(soc.read32(reg(status)), Ok(0));
        }

        for base in [0x01f2_0000, 0x01e0_0000] {
            soc.write32(base + 0x10, 1).unwrap();
            assert_eq!(soc.read32(base + 0x10).unwrap(), 0);
            soc.write32(base + 0x40, 3).unwrap();
            assert_eq!(soc.read32(base + 0x40).unwrap(), 3);
        }
        soc.write32(reg(0x100cbc), 1 << 31).unwrap();
        assert_eq!(soc.read32(reg(0x100cbc)).unwrap(), 0);
        assert_ne!(soc.read32(reg(0x100c80)).unwrap() & (1 << 15), 0);

        soc.write32(reg(0x41a610), 1).unwrap();
        assert_eq!(soc.read32(reg(0x502610)).unwrap(), 1);
        soc.write32(reg(0x409100), 2).unwrap();
        assert_eq!(soc.read32(reg(0x409800)).unwrap(), 0);
        soc.write32(reg(0x409184), 0).unwrap();
        soc.write32(reg(0x41a184), 0).unwrap();
        soc.write32(reg(0x409100), 2).unwrap();
        soc.write32(reg(0x41a100), 2).unwrap();
        assert_eq!(soc.read32(reg(0x409800)).unwrap(), 1);
        soc.write32(reg(0x409504), 0x10).unwrap();
        assert_eq!(soc.read32(reg(0x409800)).unwrap(), 0x20_000);

        soc.write32(reg(0x404170), 0x12).unwrap();
        assert_eq!(soc.read32(reg(0x404170)), Ok(2));
        soc.write32(reg(0x409504), 0x09).unwrap();
        assert_eq!(soc.read32(reg(0x409800)), Ok(0x20_000));
        soc.write32(reg(0x409500), 0xb008_0200).unwrap();
        soc.write32(reg(0x409504), 0x03).unwrap();
        assert_eq!(soc.read32(reg(0x409800)), Ok(0x20_010));
        soc.write32(reg(0x409800), 0).unwrap();
        soc.write32(reg(0x409504), 0x09).unwrap();
        assert_eq!(soc.read32(reg(0x409800)), Ok(1));
        soc.write32(reg(0x41a610), 0).unwrap();
        assert_eq!(soc.read32(reg(0x409800)).unwrap(), 0);
    }

    #[test]
    fn ga10b_gsp_boot_and_scheduler_messages_raise_stall_interrupts() {
        let mut gpu = gpu::Gpu::default();
        let mut ram = [];
        let mut host1x = host1x::Host1x::default();
        gpu.write(0x110100, 2, &mut ram, &mut host1x, 0);
        assert_eq!(gpu.read(0x11165c, 0), 2);
        assert!(!gpu.stall_irq());

        gpu.write(0x111668, 0x11, &mut ram, &mut host1x, 0);
        gpu.write(0x110180, 1 << 24, &mut ram, &mut host1x, 0);
        gpu.write(0x110184, 0x0123_4567, &mut ram, &mut host1x, 0);
        gpu.write(0x110184, 0x89ab_cdef, &mut ram, &mut host1x, 0);
        gpu.write(0x110180, 1 << 25, &mut ram, &mut host1x, 0);
        assert_eq!(gpu.read(0x110184, 0), 0x0123_4567);
        assert_eq!(gpu.read(0x110184, 0), 0x89ab_cdef);
        gpu.write(0x1101c0, 1 << 24, &mut ram, &mut host1x, 0);
        gpu.write(0x1101c4, 1, &mut ram, &mut host1x, 0);
        gpu.write(0x1101c0, 0x7ffc, &mut ram, &mut host1x, 0);
        gpu.write(0x1101c4, 2, &mut ram, &mut host1x, 0);
        gpu.write(0x110100, 2, &mut ram, &mut host1x, 0);
        assert_eq!(gpu.read(0x11165c, 0), 3);
        assert!(!gpu.stall_irq());

        gpu.write(0xb81210, 1 << 27, &mut ram, &mut host1x, 0);
        gpu.write(0xb81608, 1 << 2, &mut ram, &mut host1x, 0);
        assert!(gpu.stall_irq());
        gpu.write(0xb81610, 1 << 2, &mut ram, &mut host1x, 0);
        assert!(!gpu.stall_irq());
        assert_eq!(gpu.read(0xb81600, 0), 1 << 2);
        assert_eq!(gpu.read(0xb81010, 0), 1 << 27);
        gpu.write(0x110004, 1 << 6, &mut ram, &mut host1x, 0);
        gpu.write(0xb81608, 1 << 2, &mut ram, &mut host1x, 0);
        assert!(!gpu.stall_irq());

        assert_eq!(gpu.read(0x110c84, 0), 0x8800);
        gpu.write(0x110ac0, (1 << 25) | 0x800, &mut ram, &mut host1x, 0);
        for word in [0x1802, 0, 0x8000, 0x0800, 0x8800, 0x0100_0800] {
            assert_eq!(gpu.read(0x110ac4, 0), word);
        }
        gpu.write(0x110c84, 0x8818, &mut ram, &mut host1x, 0);
        gpu.write(0x110ac0, 1 << 24, &mut ram, &mut host1x, 0);
        gpu.write(0x110ac4, 0x2a01_040e, &mut ram, &mut host1x, 0);
        gpu.write(0x110c00, 0x8004, &mut ram, &mut host1x, 0);
        assert!(gpu.stall_irq());
        assert_eq!(gpu.read(0x110c04, 0), 0x8004);
        assert_eq!(gpu.read(0x110c80, 0), 0x881c);
        gpu.write(0x110ac0, 0x818, &mut ram, &mut host1x, 0);
        assert_eq!(gpu.read(0x110ac4, 0), 0x2a00_040e);

        gpu.write(0x1103c0, 1, &mut ram, &mut host1x, 0);
        assert_eq!(gpu.read(0x11165c, 0), 0);
        assert!(!gpu.stall_irq());
        assert_eq!(gpu.read(0x110c80, 0), 0);
    }

    #[test]
    fn ga10b_bar1_walks_bound_gpu_page_tables() {
        let mut machine = Machine::new(256 << 20, 1).unwrap();
        let soc = &mut machine.soc;
        let instance = RAM_BASE + 0x1000;
        let root = RAM_BASE + 0x2000;
        let page = RAM_BASE + 0x9000;
        let bind = gpu::BAR0 + 0x1704;
        let bar = gpu::BAR1 + 0x1024;

        soc.write64(instance + 0x200, root).unwrap();
        for (entry, next) in [
            (root, root + 0x1000),
            (root + 0x1000, root + 0x2000),
            (root + 0x2000, root + 0x3000),
            (root + 0x3008, root + 0x4000),
        ] {
            soc.write64(entry, (next >> 4) | 4).unwrap();
        }
        soc.write64(root + 0x4008, (page >> 4) | 5).unwrap();
        soc.write32(page + 0x24, 0x1234_5678).unwrap();

        assert_eq!(soc.read32(bar), Ok(0));
        soc.write32(bind, (1 << 31) | (instance >> 12) as u32)
            .unwrap();
        assert_eq!(soc.read32(gpu::BAR0 + 0x1710), Ok(0));
        assert_eq!(soc.read32(bar), Ok(0x1234_5678));
        soc.write8(bar + 1, 0xab).unwrap();
        assert_eq!(soc.read32(page + 0x24), Ok(0x1234_ab78));

        soc.write64(root + 0x4008, (page >> 4) | 0x45).unwrap();
        soc.write32(bar, 0).unwrap();
        assert_eq!(soc.read32(page + 0x24), Ok(0x1234_ab78));
        soc.gpu.reset();
        assert_eq!(soc.read32(bar), Ok(0));
    }

    fn load(machine: &mut Machine, at: u64, code: &[u32]) {
        for (index, word) in code.iter().enumerate() {
            machine.soc.write32(RAM_BASE + at + 4 * index as u64, *word).unwrap();
        }
    }

    fn word(machine: &Machine, offset: usize) -> u32 {
        u32::from_le_bytes(machine.soc.ram[offset..offset + 4].try_into().unwrap())
    }

    /// Run until `done` or 30 s of host time, as the CLI does (an idle machine's time moves on).
    fn run_until(machine: &mut Machine, done: impl Fn(&Machine) -> bool) {
        let start = std::time::Instant::now();
        while !done(machine) && start.elapsed().as_secs() < 30 {
            match machine.run(1_000_000) {
                Stop::Budget => {}
                Stop::Idle(deadline) => machine.idle_until(deadline),
                stop => panic!("{stop:?}"),
            }
        }
    }

    /// Six cores add 100,000 each to one counter with LDXR/STXR (interpreted and compiled), then
    /// sleep; no increment is lost. With `--threads multi` they run on their own host threads.
    #[test]
    fn six_cores_count_with_exclusives() {
        for multi in [false, true] {
            if multi && cfg!(target_arch = "wasm32") {
                continue;
            }
            for jit in [false, true] {
                let mut machine = Machine::new(256 << 20, 6).unwrap();
                if machine.set_jit(jit) != jit {
                    continue;
                }
                machine.multi = multi;
                load(&mut machine, 0, &[
                    0xd2b0_0201, // mov x1, #0x80100000
                    0xd290_d402, // mov x2, #0x86a0
                    0xf2a0_0022, // movk x2, #1, lsl #16 (100,000)
                    0x885f_7c23, // 1: ldxr w3, [x1]
                    0x1100_0463, // add w3, w3, #1
                    0x8804_7c23, // stxr w4, w3, [x1]
                    0x35ff_ffa4, // cbnz w4, 1b
                    0xf100_0442, // subs x2, x2, #1
                    0x54ff_ff61, // b.ne 1b
                    0xd503_207f, // 2: wfi
                    0x17ff_ffff, // b 2b
                ]);
                for core in 0..6 {
                    machine.cpus[core].pc = RAM_BASE;
                    machine.cpus[core].el = 1;
                    machine.state[core] = CoreState::Running;
                }
                run_until(&mut machine, |m| m.state.iter().all(|&s| s == CoreState::Wfi));
                assert_eq!(machine.state, [CoreState::Wfi; 6], "multi {multi} jit {jit}");
                assert_eq!(word(&machine, 0x10_0000), 600_000, "multi {multi} jit {jit}");
                // the threaded runs gave every core its own JIT
                assert_eq!(machine.cpus.iter().all(|c| c.mt && (c.jit.is_some() || !jit)), multi);
            }
        }
    }

    /// Interrupts wake cores that sleep in WFI, through the GICv3. Core 0 arms its virtual arch
    /// timer (100 us, PPI 27) and sleeps; when it wakes it sends SGI 1 to core 1 (Aff1 1) through
    /// ICC_SGI1R_EL1; core 1 has slept since it started. Each stores the INTID it acknowledges.
    #[test]
    fn cores_wake_from_wfi_on_the_arch_timer_and_an_sgi() {
        for multi in [false, true] {
            if multi && cfg!(target_arch = "wasm32") {
                continue;
            }
            let mut machine = Machine::new(256 << 20, 2).unwrap();
            machine.multi = multi;
            let core0 = [
                0xd2a1_e800, // movz x0, #0x0f40, lsl #16 (GICD)
                0x5280_0041, // mov w1, #2
                0xb900_0001, // str w1, [x0] (GICD_CTLR: EnableGrp1NS)
                0xd2a1_e882, // movz x2, #0x0f44, lsl #16 (core 0's GICR)
                0xb900_145f, // str wzr, [x2, #0x14] (GICR_WAKER)
                0x9140_4043, // add x3, x2, #0x10, lsl #12 (its SGI frame)
                0x52a1_0001, // mov w1, #0x8000000
                0xb900_8061, // str w1, [x3, #0x80] (IGROUPR0: PPI 27)
                0xb901_0061, // str w1, [x3, #0x100] (ISENABLER0)
                0xd280_1fe1, // mov x1, #0xff
                0xd518_4601, // msr ICC_PMR_EL1, x1
                0xd280_0021, // mov x1, #1
                0xd518_cce1, // msr ICC_IGRPEN1_EL1, x1
                0xd281_86a1, // mov x1, #3125 (100 us at 31.25 MHz)
                0xd51b_e301, // msr cntv_tval_el0, x1
                0xd280_0021, // mov x1, #1
                0xd51b_e321, // msr cntv_ctl_el0, x1
                0xd503_207f, // 1: wfi
                0xd538_cc08, // mrs x8, ICC_IAR1_EL1
                0xf10f_fd1f, // cmp x8, #1023
                0x54ff_ffa0, // b.eq 1b
                0xd2b0_0207, // movz x7, #0x8010, lsl #16
                0xb900_00e8, // str w8, [x7]
                0xd280_0061, // mov x1, #3
                0xd51b_e321, // msr cntv_ctl_el0, x1 (masked)
                0xd2a0_2029, // movz x9, #0x0101, lsl #16
                0xf280_0029, // movk x9, #1 (SGI 1 to Aff1 1, Aff0 0)
                0xd518_cba9, // msr ICC_SGI1R_EL1, x9
                0xd503_207f, // 2: wfi
                0x17ff_ffff, // b 2b
            ];
            let core1 = [
                0xd2a1_e8c2, // movz x2, #0x0f46, lsl #16 (core 1's GICR)
                0xb900_145f, // str wzr, [x2, #0x14] (GICR_WAKER)
                0x9140_4043, // add x3, x2, #0x10, lsl #12
                0x5280_0041, // mov w1, #2
                0xb900_8061, // str w1, [x3, #0x80] (IGROUPR0: SGI 1)
                0xb901_0061, // str w1, [x3, #0x100] (ISENABLER0)
                0xd280_1fe1, // mov x1, #0xff
                0xd518_4601, // msr ICC_PMR_EL1, x1
                0xd280_0021, // mov x1, #1
                0xd518_cce1, // msr ICC_IGRPEN1_EL1, x1
                0xd503_207f, // 1: wfi
                0xd538_cc08, // mrs x8, ICC_IAR1_EL1
                0xf10f_fd1f, // cmp x8, #1023
                0x54ff_ffa0, // b.eq 1b
                0xd2b0_0207, // movz x7, #0x8010, lsl #16
                0xb900_04e8, // str w8, [x7, #4]
                0xd503_207f, // 2: wfi
                0x17ff_ffff, // b 2b
            ];
            for (core, (at, code)) in [(0, &core0[..]), (0x200, &core1[..])].into_iter().enumerate() {
                load(&mut machine, at, code);
                machine.cpus[core].pc = RAM_BASE + at;
                machine.cpus[core].el = 1;
                machine.state[core] = CoreState::Running;
            }
            run_until(&mut machine, |m| word(m, 0x10_0004) != 0);
            assert_eq!((word(&machine, 0x10_0000), word(&machine, 0x10_0004)), (27, 1), "multi {multi}: the INTIDs each core acknowledged");
        }
    }

    #[test]
    fn guest_reads_ampere_identity_and_virtual_timer() {
        let mut machine = Machine::new(256 << 20, 1).unwrap();
        for (offset, word) in [
            0xd2a2_e001, // movz x1, #0x1700, lsl #16
            0xb940_0020, // ldr  w0, [x1]
            0xf292_8001, // movk x1, #0x9400
            0xb940_0022, // ldr  w2, [x1]
            0xb940_1023, // ldr  w3, [x1, #16]
            0xd440_0000, // hlt  #0
        ]
        .into_iter()
        .enumerate()
        {
            machine
                .soc
                .write32(RAM_BASE + (offset * 4) as u64, word)
                .unwrap();
        }
        machine.cpus[0].pc = RAM_BASE;
        machine.idle_until(1 << 32);
        assert_eq!(machine.run(12), Stop::Hlt(0));
        assert_eq!(machine.cpus[0].x[0], 0x17b0_00a1);
        assert_eq!(machine.cpus[0].x[2], 3);
        assert_eq!(machine.cpus[0].x[3], 1);
    }
}
