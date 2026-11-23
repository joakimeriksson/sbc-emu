//! Orange Pi RV (StarFive JH7110) serial bring-up adapter: OpenSBI `fw_dynamic` and a
//! RISC-V Linux `Image` with the distribution's device tree, loaded as the board's U-Boot
//! would load them.
use crate::{arg, serial_board::{self, RunConfig, SerialBoard}, vnc::Screen};
use sbc_machine::Machine as HostMachine;
use starfive_jh7110::{boot, display::{Display, Mode}, AudioSource, HartState, Machine};
use std::process::ExitCode;

impl SerialBoard for Machine {
    fn host(&mut self) -> HostMachine<'_> {
        HostMachine::OrangePiRv(self)
    }
    fn now(&self) -> u64 {
        Machine::now(self)
    }
    fn instret(&self) -> u64 {
        Machine::instret(self)
    }
    fn save(&self) -> Result<Vec<u8>, String> {
        Machine::save(self)
    }
    fn restore(&mut self, file: &[u8]) -> Result<(), String> {
        Machine::restore(self, file)
    }
    fn screen(&mut self) -> Option<&mut dyn Screen> {
        Some(self)
    }
    fn capture_audio(&mut self) -> bool {
        self.soc.pwmdac.capture.on = true;
        self.soc.i2s.capture.on = true;
        true
    }
    /// What one output plays: the first of the headphone jack (PWM DAC) and HDMI (I2S) to
    /// produce samples is recorded for the rest of the run; the other's samples are dropped
    /// (one WAV file holds one stream at one rate).
    fn take_audio(&mut self) -> (u32, Vec<i16>) {
        let soc = &mut self.soc;
        if soc.audio_source.is_none() {
            if !soc.pwmdac.capture.samples.is_empty() {
                soc.audio_source = Some(AudioSource::Dac);
            } else if !soc.i2s.capture.samples.is_empty() {
                soc.audio_source = Some(AudioSource::Hdmi);
            }
        }
        let (keep, drop) = match soc.audio_source {
            Some(AudioSource::Hdmi) => (&mut soc.i2s.capture, &mut soc.pwmdac.capture),
            _ => (&mut soc.pwmdac.capture, &mut soc.i2s.capture),
        };
        drop.samples.clear();
        (keep.rate, std::mem::take(&mut keep.samples))
    }
    fn report(&self, tag: &str) {
        eprintln!("[{tag}] insns={} virtual={:.3}s", Machine::instret(self), self.now() as f64 / 1e9);
        if let Some((compiled, invalidated, flushes)) = self.jit_stats() {
            eprintln!("[{tag}] JIT: {compiled} regions compiled, {invalidated} dropped after their code changed, {flushes} full flushes");
        }
        for (h, hart) in self.harts.iter().enumerate() {
            if self.state[h] == HartState::Off {
                continue;
            }
            eprintln!(
                "[{tag}] hart {h}: pc={:#x} prv={} insns={} traps={} fence.i={} satp={:#x} sepc={:#x} scause={:#x} stval={:#x} mepc={:#x} mcause={:#x}{}",
                hart.pc, hart.prv, hart.instret, hart.traps, hart.fence_i, hart.satp, hart.sepc, hart.scause, hart.stval, hart.mepc, hart.mcause,
                if hart.wfi { " (wfi)" } else { "" }
            );
            if let Some((pc, insn)) = hart.last_illegal {
                eprintln!("[{tag}] hart {h}: last illegal instruction {insn:#010x} at {pc:#x}");
            }
        }
        if let Some(x) = self.soc.pcie[0].xhci.as_ref() {
            let devs: Vec<String> = x.attached().iter().map(|d| format!("{d:?}")).collect();
            eprintln!("[{tag}] USB (VL805): {}; PCIe0: {}", devs.join(", "), self.soc.pcie[0].summary());
        }
        if let Some(n) = self.soc.pcie[1].nvme.as_ref() {
            eprintln!("[{tag}] NVMe (M.2): {} reads, {} writes; PCIe1: {} MSIs", n.reads, n.writes, self.soc.pcie[1].msis);
        }
        if self.soc.pwmdac.capture.frames > 0 {
            eprintln!("[{tag}] audio (PWM DAC): {} frames at {} Hz, {} DMA blocks", self.soc.pwmdac.capture.frames, self.soc.sample_rate(), self.soc.dmac.transfers);
        }
        if self.soc.i2s.capture.frames > 0 {
            eprintln!("[{tag}] audio (HDMI, I2S): {} frames at {} Hz", self.soc.i2s.capture.frames, self.soc.i2s_rate());
        }
        if self.soc.display.frames > 0 {
            eprintln!("[{tag}] HDMI: {} frames", self.soc.display.frames);
        }
        if self.soc.gmac0.tx_frames + self.soc.gmac0.rx_frames > 0 {
            eprintln!("[{tag}] Ethernet: {} frames sent, {} received", self.soc.gmac0.tx_frames, self.soc.gmac0.rx_frames);
        }
        if self.soc.sec.ops > 0 {
            eprintln!("[{tag}] PKA: {} operations", self.soc.sec.ops);
        }
        let fw = &self.soc.gpu.fw;
        if fw.boots > 0 {
            eprintln!("[{tag}] GPU (PowerVR BXE-4-32): firmware booted {} times, {} kernel commands, {}", fw.boots, fw.commands, if fw.running { "running" } else { "powered off" });
        }
        if !self.soc.stubs.is_empty() {
            let pages: Vec<String> = self.soc.stubs.keys().map(|p| format!("{p:#x}")).collect();
            eprintln!("[{tag}] unmodelled device pages (read-back stubs): {}", pages.join(" "));
        }
    }
}

impl Screen for Machine {
    fn shown_size(&self) -> Option<(u32, u32)> {
        self.soc.display.shown_size()
    }
    fn display_size(&self) -> (u32, u32) {
        let mode = self.soc.display.mode;
        self.soc.display.shown_size().unwrap_or((mode.width, mode.height))
    }
    fn render(&self, out: &mut Vec<u8>) -> Option<(u32, u32)> {
        self.soc.render(out)
    }
    fn name(&self) -> &'static str {
        "sbc-emu: Orange Pi RV"
    }
    fn key(&mut self, usage: u8, down: bool) {
        self.soc.usb.kbd.key(usage, down);
    }
    fn type_byte(&mut self, b: u8) {
        self.soc.usb.kbd.type_byte(b);
    }
    fn pointer(&mut self, buttons: u8, x: u16, y: u16, wheel: i8) {
        self.soc.usb.tablet.pointer(buttons, x, y, wheel);
    }
}

pub(crate) fn run(args: &[String]) -> ExitCode {
    match execute(args) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("[orangepi-rv] {e}");
            ExitCode::FAILURE
        }
    }
}

fn execute(args: &[String]) -> Result<(), String> {
    // --smp N (1-4) starts N of the four U74 harts; the shared validation allows only 1.
    let smp: usize = arg(args, "--smp").map_or(Ok(4), |s| s.parse().map_err(|_| "--smp 1|2|3|4"))?;
    if !(1..=4).contains(&smp) {
        return Err("--smp 1|2|3|4 (the JH7110 has four U74 application harts)".into());
    }
    let mut checked: Vec<String> = Vec::new();
    let mut it = args.iter();
    while let Some(a) = it.next() {
        if a == "--smp" {
            it.next();
        } else {
            checked.push(a.clone());
        }
    }
    serial_board::validate_options(&checked, &["--dtb", "--dump-dtb", "--firmware", "--sd", "--net", "--screenshot", "--vnc", "--hdmi-mode", "--usb-drive", "--nvme", "--audio-out", "--spi-flash", "--restore", "--snapshot"])?;
    let mode = match arg(args, "--hdmi-mode") {
        Some(spec) => {
            let (w, h) = spec.split_once('x').ok_or("--hdmi-mode WxH")?;
            let (w, h) = (w.parse().map_err(|_| "--hdmi-mode WxH")?, h.parse().map_err(|_| "--hdmi-mode WxH")?);
            Mode::find(w, h).ok_or("--hdmi-mode: the emulated monitor offers 1280x720, 1920x1080 and 640x480")?
        }
        None => Mode::MODES[0],
    };
    let net = arg(args, "--net").unwrap_or("nat");
    if !matches!(net, "nat" | "local" | "none") {
        return Err("--net nat|local|none".into());
    }
    let read = |path: &str| std::fs::read(path).map_err(|e| format!("{path}: {e}"));
    // Without --kernel the board boots as it does from its own firmware: the boot ROM loads
    // U-Boot SPL from the SPI NOR flash (--spi-flash, a 16 MiB image of the board's flash) or,
    // without one, from --sd, and SPL, OpenSBI and U-Boot take it from there.
    let direct = arg(args, "--kernel").is_some();
    // a snapshot has the kernel in its RAM: something to boot is needed only when the guest reboots
    let restore = arg(args, "--restore").is_some();
    let can_boot = direct || arg(args, "--sd").is_some() || arg(args, "--spi-flash").is_some();
    let spi_flash = arg(args, "--spi-flash").map(read).transpose()?;
    let (kernel, dtb, firmware) = if direct {
        (
            read(arg(args, "--kernel").unwrap())?,
            read(arg(args, "--dtb").ok_or("--dtb jh7110-orangepi-rv.dtb is required with --kernel")?)?,
            read(arg(args, "--firmware").ok_or("--firmware fw_dynamic.bin (OpenSBI, generic platform) is required with --kernel")?)?,
        )
    } else {
        if !can_boot && !restore {
            return Err("--kernel Image (direct boot), --spi-flash FLASH.img (the board's firmware) or --sd CARD.img to boot from the card's U-Boot".into());
        }
        // the card's U-Boot loads the kernel, device tree and initrd itself
        for opt in ["--dtb", "--firmware", "--initrd", "--append", "--dump-dtb"] {
            if arg(args, opt).is_some() {
                return Err(format!("{opt} applies to direct boot (--kernel); booting from the SD card, U-Boot takes these from the card"));
            }
        }
        (Vec::new(), Vec::new(), Vec::new())
    };
    let initrd = arg(args, "--initrd").map(read).transpose()?;
    let cmdline = arg(args, "--append").unwrap_or(boot::DEFAULT_CMDLINE);
    let ram = serial_board::ram_bytes(args, starfive_jh7110::RAM_SIZE >> 20)?;
    if ram < (128 << 20) || ram > (16 << 30) {
        return Err("--ram-mb must be between 128 and 16384".into());
    }
    if !direct && can_boot && !boot::uboot_ram_ok(ram as u64) {
        return Err("booting from the SD card needs --ram-mb 2048, 4096, 8192 or 16384: U-Boot knows only the board's DRAM sizes".into());
    }
    let config = RunConfig::from_args("orangepi-rv", args)?;
    // The microSD card survives guest reboots (one disk shared by the successive machines).
    let persist = args.iter().any(|a| a == "--sd-persist");
    let disk = arg(args, "--sd")
        .map(|path| crate::disk::SharedDisk::open(path, persist).map_err(|e| format!("{path}: {e}")))
        .transpose()?;
    // --usb-drive IMAGE: a USB stick in one of the VL805's USB-A ports (copy-on-write; each
    // boot starts from the image)
    let usb_drive = arg(args, "--usb-drive");
    // --nvme IMAGE: an NVMe SSD in the M.2 slot (copy-on-write; each boot starts from the image)
    let nvme = arg(args, "--nvme");
    // the RV64 JIT (AArch64 hosts): on unless --no-jit or SBC_JIT=0
    let jit = riscv64::jit::AVAILABLE && !args.iter().any(|a| a == "--no-jit") && std::env::var("SBC_JIT").map_or(true, |v| v != "0");
    // the machine restored into is not booted first; when the guest reboots, a new one is
    let restored = std::cell::Cell::new(false);
    let make_machine = || -> Result<Machine, String> {
        let mut m = Machine::new(ram);
        m.set_jit(jit);
        if let Some(path) = usb_drive {
            let d = crate::disk::CowFile::open(path).map_err(|e| format!("{path}: {e}"))?;
            m.soc.attach_usb_drive(Box::new(d));
        }
        if let Some(path) = nvme {
            let d = crate::disk::CowFile::open(path).map_err(|e| format!("{path}: {e}"))?;
            m.soc.attach_nvme(Box::new(d));
        }
        if net != "none" {
            m.connect_network(net == "nat", std::env::var_os("SBC_NET_LOG").is_some());
        }
        m.soc.sdio1.insert(disk.clone().map(|d| Box::new(d) as Box<dyn emu_core::disk::Disk>));
        m.soc.display = Display::new(mode);
        if let Some(img) = spi_flash.as_deref() {
            m.soc.qspi.flash = Some(starfive_jh7110::qspi::SpiNor::new(img)?);
            if !direct {
                m.soc.boot_mode = starfive_jh7110::machine::BOOT_MODE_SPI;
            }
        }
        if restore && !restored.replace(true) {
            // nothing: the snapshot has it all
            return Ok(m);
        } else if !can_boot {
            return Err("the guest rebooted: give --kernel, --sd or --spi-flash with --restore to boot it again".into());
        } else if direct {
            boot::boot_linux(&mut m, &firmware, &kernel, &dtb, initrd.as_deref(), cmdline)?;
        } else {
            boot::boot_rom(&mut m)?;
        }
        for h in smp + 1..m.harts.len() {
            m.state[h] = HartState::Off;
        }
        Ok(m)
    };
    if let Some(path) = arg(args, "--dump-dtb") {
        let range = initrd.as_ref().map(|d| (boot::INITRD_ADDR, boot::INITRD_ADDR + d.len() as u64));
        let fb = (!boot::has_display_node(&dtb)).then(|| (boot::framebuffer_addr(ram as u64), mode.width, mode.height));
        let blob = boot::prepare_device_tree(&dtb, ram as u64, cmdline, range, fb)?;
        std::fs::write(path, blob).map_err(|e| format!("{path}: {e}"))?;
    }
    eprintln!(
        "[orangepi-rv] experimental StarFive JH7110: {smp} of 4 U74 harts (RV64GC, Sv39, {}), CLINT, PLIC, UART0, \
         clock/reset generators, microSD (DW MMC), Gigabit Ethernet (GMAC0 + YT8531C), HDMI (DC8200 + Inno HDMI), USB 3.0 (PCIe + VL805 xHCI: keyboard, tablet, --usb-drive), M.2 NVMe (--nvme), Wi-Fi and Bluetooth (AP6256), audio (PWM DAC and HDMI I2S, --audio-out), I2C, PKA; OpenSBI from --firmware, or the boot ROM, U-Boot SPL and U-Boot from --sd; other devices are stubs",
        if jit { "JIT to AArch64" } else { "interpreter" }
    );
    serial_board::run(config, make_machine)
}
