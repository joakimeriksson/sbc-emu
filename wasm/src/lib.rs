//! C ABI over the emulated boards for the browser (no wasm-bindgen; see web/worker.js): the
//! Pi 4B, the Tinker Board S (RK3288, `sbc_boot_tinker*`) and the Orange Pi One (Allwinner H3,
//! `sbc_boot_opi1`) and the Orange Pi RV (StarFive JH7110, `sbc_boot_opirv`). Single-threaded: one machine
//! per module instance, driven from a Web Worker. Exports for devices only one board has (the
//! Pi's USB keyboard, GPIO, sensors, audio) do nothing on the other.
#![allow(clippy::missing_safety_doc)]
use emu_core::disk::Disk;
use bcm2711::{Machine, Stop};
use std::cell::RefCell;

#[cfg(target_arch = "wasm32")]
#[link(wasm_import_module = "env")]
extern "C" {
    /// Provided by the page (web/worker.js): copy `count` sectors starting at `lba` into `out`.
    fn sbc_disk_read(lba: f64, count: u32, out: *mut u8);
}
#[cfg(not(target_arch = "wasm32"))]
unsafe fn sbc_disk_read(_lba: f64, count: u32, out: *mut u8) { std::ptr::write_bytes(out, 0, count as usize * 512); }

/// The SD card image as the page serves it (read lazily by HTTP range requests in the worker);
/// writes stay in this overlay, so the served image is never modified.
/// `sectors`: what the guest sees, a whole number of an SD card's units; the page's image holds the
/// first `stored`. The rest reads as zeros until it is written (the overlay keeps it).
struct WebDisk { stored: u64, sectors: u64, overlay: emu_core::disk::Overlay }

impl WebDisk {
    /// A card for an image of `sectors` (a partial last sector counts; the page fills it with
    /// zeros), or None for one below the smallest card (512 KiB), as in the CLI.
    fn card(sectors: f64) -> Option<Box<dyn Disk>> {
        let unit = emu_core::sdcard::UNIT;
        let stored = sectors.ceil() as u64;
        (sectors >= unit as f64).then(|| Box::new(WebDisk { stored, sectors: stored.next_multiple_of(unit), overlay: Default::default() }) as Box<dyn Disk>)
    }
}

impl Disk for WebDisk {
    fn sectors(&self) -> u64 { self.sectors }
    fn read(&mut self, lba: u64, out: &mut [u8]) {
        let valid = (self.stored.saturating_sub(lba).saturating_mul(512)).min(out.len() as u64) as usize;
        out[valid..].fill(0);
        if valid > 0 {
            // SAFETY: `out[..valid]` is a buffer of whole sectors; the import writes exactly that many
            unsafe { sbc_disk_read(lba as f64, (valid / 512) as u32, out.as_mut_ptr()) };
        }
        self.overlay.apply(lba, out);
    }
    fn overlay(&self) -> Vec<(u64, Vec<u8>)> { self.overlay.sorted() }
    fn restore_overlay(&mut self, sectors: Vec<(u64, Vec<u8>)>) { self.overlay.replace(sectors, self.sectors); }
    fn write(&mut self, lba: u64, data: &[u8]) { self.overlay.store(lba, data, self.sectors); }
}

/// The board this module instance runs.
enum Board {
    Pi(Machine),
    Tinker(Box<rockchip_rk3288::Machine>, TinkerBoot),
    /// The Orange Pi One with what it was booted from (kernel, stock DTB, initrd, command line).
    OrangePiOne(Box<allwinner_h3::Machine>, DirectBoot),
    /// The Orange Pi RV with OpenSBI and what it was booted from.
    OrangePiRv(Box<starfive_jh7110::Machine>, Vec<u8>, DirectBoot),
    /// The Orange Pi 5 with what it was booted from (kernel, initrd, command line; the device
    /// tree is the emulator's own).
    OrangePi5(Box<rk3588::Machine>, Opi5Boot),
    /// The Jetson Nano with what it was booted from.
    JetsonNano(Box<tegra210::Machine>, DirectBoot),
    /// The ESP32-S31-Function-CoreBoard-1, booting from its mask ROM and flash image, with the
    /// serial input not yet taken by UART0 (it refuses input until Linux has set it up).
    Esp32S31(Box<esp32s31::Machine>, std::collections::VecDeque<u8>),
}

/// An Orange Pi 5 direct boot's inputs: the kernel, an initramfs and the command line.
type Opi5Boot = (Vec<u8>, Option<Vec<u8>>, String);

/// A direct boot's inputs: the kernel, the board's stock device tree, an initramfs and the
/// command line.
type DirectBoot = (Vec<u8>, Vec<u8>, Option<Vec<u8>>, String);

/// How a Tinker Board was started, so a reset (`reboot`) starts it the same way.
struct TinkerBoot {
    /// The RTC's time at power-on (Unix seconds).
    unix0: f64,
    /// Direct boot (kernel, stock DTB, initrd, command line); None = from the SD card.
    direct: Option<(Vec<u8>, Vec<u8>, Option<Vec<u8>>, String)>,
}

impl Board {
    fn run(&mut self, budget: u64) -> Stop {
        match self { Board::Pi(m) => m.run(budget), Board::Tinker(m, _) => m.run(budget), Board::OrangePiOne(m, _) => m.run(budget), Board::OrangePiRv(m, ..) => m.run(budget), Board::OrangePi5(m, _) => m.run(budget), Board::JetsonNano(m, _) => m.run(budget), Board::Esp32S31(m, rx) => s31_run(m, rx, budget) }
    }
    fn now(&self) -> u64 {
        match self { Board::Pi(m) => m.now, Board::Tinker(m, _) => m.now(), Board::OrangePiOne(m, _) => m.now(), Board::OrangePiRv(m, ..) => m.now(), Board::OrangePi5(m, _) => m.now(), Board::JetsonNano(m, _) => m.now(), Board::Esp32S31(m, _) => { use esp_soc::SocBus as _; m.bus.cycles() * 25 / 8 } }
    }
    fn idle_until(&mut self, t: u64) {
        match self { Board::Pi(m) => m.idle_until(t), Board::Tinker(m, _) => m.idle_until(t), Board::OrangePiOne(m, _) => m.idle_until(t), Board::OrangePiRv(m, ..) => m.idle_until(t), Board::OrangePi5(m, _) => m.idle_until(t), Board::JetsonNano(m, _) => m.idle_until(t), Board::Esp32S31(..) => {} }
    }
    fn instret(&self) -> u64 {
        match self { Board::Pi(m) => m.instret(), Board::Tinker(m, _) => m.instret(), Board::OrangePiOne(m, _) => m.instret(), Board::OrangePiRv(m, ..) => m.instret(), Board::OrangePi5(m, _) => m.instret(), Board::JetsonNano(m, _) => m.instret(), Board::Esp32S31(m, _) => m.cores.iter().map(|c| c.insn_count).sum() }
    }
}

struct State {
    m: Board,
    fb: Vec<u8>,
    fb_w: u32,
    fb_h: u32,
    uart: Vec<u8>,
    /// samples taken from the I²S DAC for the page (16-bit interleaved stereo)
    audio: Vec<i16>,
    wait_ms: f64,
    stop: Option<Stop>,
    /// added to the page's wall clock so a restored machine's time keeps running (ns)
    wall_offset: Option<f64>,
}

impl State {
    fn new(m: Board, wall_offset: Option<f64>) -> State {
        State { m, fb: Vec::new(), fb_w: 0, fb_h: 0, uart: Vec::new(), audio: Vec::new(), wait_ms: 0.0, stop: None, wall_offset }
    }
}

thread_local! { static ST: RefCell<Option<State>> = const { RefCell::new(None) }; }
thread_local! { static SMP: std::cell::Cell<usize> = const { std::cell::Cell::new(4) }; }
// The HDMI monitor's preferred mode for machines booted with the display pipeline (0 = the
// firmware framebuffer only, no vc4/V3D, on the Pi; the Tinker Board's monitor then offers 720p)
thread_local! { static KMS: std::cell::Cell<(u32, u32)> = const { std::cell::Cell::new((0, 0)) }; }

/// Machines booted after this call get the vc4 display pipeline (an HDMI monitor of `w`x`h`)
/// and the V3D 3D GPU, as `dtoverlay=vc4-kms-v3d` asks; 0x0 leaves the firmware framebuffer.
/// On the Tinker Board it picks the emulated monitor's mode (1024x768, 1280x720, 1920x1080).
#[no_mangle]
pub extern "C" fn sbc_kms(w: u32, h: u32) { KMS.with(|c| c.set((w, h))); }

fn add_gpu(m: &mut Machine) {
    let (w, h) = KMS.with(|c| c.get());
    if w > 0 {
        m.soc.vc4 = Some(bcm2711::vc4::Vc4::with_mode(w, h));
        m.soc.v3d = Some(v3d::V3d::new());
    }
}
fn smp() -> usize { SMP.with(|c| c.get()) }

/// Cores of the machines booted after this call (1..4; the Pi 4 has 4, the default).
#[no_mangle]
pub extern "C" fn sbc_smp(n: u32) { SMP.with(|c| c.set((n as usize).clamp(1, 4))); }

fn with<R>(f: impl FnOnce(&mut State) -> R) -> Option<R> { ST.with(|s| s.borrow_mut().as_mut().map(f)) }

/// The Pi, if that is the board running (for the exports of its own devices).
fn with_pi<R>(f: impl FnOnce(&mut Machine) -> R) -> Option<R> {
    with(|s| match &mut s.m { Board::Pi(m) => Some(f(m)), _ => None }).flatten()
}

/// Allocate `len` bytes the page can copy a file into (leaked until the page reloads).
#[no_mangle]
pub extern "C" fn sbc_alloc(len: usize) -> *mut u8 { Box::leak(vec![0u8; len].into_boxed_slice()).as_mut_ptr() }

// ---- xz, for SD images dropped on the page (web/worker.js). An .xz file is a series of blocks;
// the worker parses the index at its end, and has one block at a time decoded here: on demand
// when the blocks are small (Ubuntu's are 1 MiB, so the card is read without unpacking it),
// or all of them once into the browser's file storage when they are large (Raspberry Pi OS's).
// Called from the worker's disk-read import, inside sbc_run, so it touches only its own state.
thread_local! {
    static XZ_IN: RefCell<Vec<u8>> = const { RefCell::new(Vec::new()) };
    static XZ_OUT: RefCell<Vec<u8>> = const { RefCell::new(Vec::new()) };
}

/// A buffer of `len` bytes for the next block's compressed bytes (header included).
#[no_mangle]
pub extern "C" fn sbc_xz_in(len: usize) -> *mut u8 {
    XZ_IN.with(|b| { let mut b = b.borrow_mut(); b.clear(); b.resize(len, 0); b.as_mut_ptr() })
}

/// Decode the block in `sbc_xz_in`'s buffer, `size` bytes uncompressed (from the index): its
/// length, or -1 (a filter other than LZMA2, such as a BCJ, or corrupt data). The bytes are at
/// `sbc_xz_out_ptr` until the next call.
#[no_mangle]
pub extern "C" fn sbc_xz_block(size: usize) -> f64 {
    XZ_IN.with(|i| XZ_OUT.with(|o| {
        let (i, mut o) = (i.borrow(), o.borrow_mut());
        o.clear();
        o.reserve(size);
        match xz_block(&i, &mut o) { Ok(()) if o.len() == size => o.len() as f64, _ => -1.0 }
    }))
}

#[no_mangle]
pub extern "C" fn sbc_xz_out_ptr() -> *const u8 { XZ_OUT.with(|o| o.borrow().as_ptr()) }

/// One xz block: its header (one LZMA2 filter, no others), then LZMA2 data, whose end marker
/// ends the decode (the padding and check after it are not read).
fn xz_block(b: &[u8], out: &mut Vec<u8>) -> Result<(), ()> {
    let hsize = (*b.first().ok_or(())? as usize + 1) * 4;
    if b[0] == 0 || b.len() < hsize { return Err(()); }
    let flags = b[1];
    if flags & 3 != 0 { return Err(()); } // more than one filter
    let mut p = 2;
    let mut varint = || -> Result<u64, ()> {
        let (mut v, mut shift) = (0u64, 0);
        loop {
            let c = *b.get(p).ok_or(())?;
            p += 1;
            v |= u64::from(c & 0x7f) << shift;
            if c & 0x80 == 0 { return Ok(v); }
            shift += 7;
            if shift > 63 { return Err(()); }
        }
    };
    if flags & 0x40 != 0 { varint()?; } // compressed size
    if flags & 0x80 != 0 { varint()?; } // uncompressed size
    if varint()? != 0x21 { return Err(()); } // LZMA2
    let mut data = &b[hsize..];
    lzma_rs::lzma2_decompress(&mut data, out).map_err(|_| ())
}

/// Boot Linux. Returns 0 on success.
#[no_mangle]
pub unsafe extern "C" fn sbc_boot(ram_mb: u32, k: *const u8, klen: usize, i: *const u8, ilen: usize, c: *const u8, clen: usize) -> i32 {
    let kernel = std::slice::from_raw_parts(k, klen);
    let initrd = if ilen > 0 { Some(std::slice::from_raw_parts(i, ilen)) } else { None };
    let cmdline = if clen > 0 { String::from_utf8_lossy(std::slice::from_raw_parts(c, clen)).into_owned() } else {
        format!("{} sbcemu.shell=fb", bcm2711::boot::DEFAULT_CMDLINE)
    };
    let mut m = Machine::with_cpus((ram_mb as usize) << 20, smp());
    if bcm2711::boot::boot_linux(&mut m, kernel, initrd, &cmdline, true).is_err() { return -1; }
    ST.with(|s| *s.borrow_mut() = Some(State::new(Board::Pi(m), Some(0.0))));
    0
}

/// Boot from an SD card image of `sectors` 512-byte sectors served by the page, the way the
/// Pi firmware does (config.txt, cmdline.txt, kernel8.img, initramfs). Returns 0 on success.
#[no_mangle]
pub unsafe extern "C" fn sbc_boot_sd(ram_mb: u32, sectors: f64) -> i32 {
    let Some(mut disk) = WebDisk::card(sectors) else { return -2 };
    let b = match bcm2711::boot::read_sd_boot(disk.as_mut()) { Ok(b) => b, Err(_) => return -2 };
    let mut m = Machine::with_cpus((ram_mb as usize) << 20, smp());
    m.soc.sd = bcm2711::sdhci::Sdhci::new(Some(disk));
    m.connect_network(false, false);
    add_gpu(&mut m);
    if bcm2711::boot::boot_linux(&mut m, &b.kernel, b.initramfs.as_deref(), &b.cmdline, true).is_err() { return -1; }
    ST.with(|s| *s.borrow_mut() = Some(State::new(Board::Pi(m), Some(0.0))));
    0
}

/// A Tinker Board S as the CLI's `--board tinkerboard` builds it: the RK808 PMIC with its RTC
/// at `unix` seconds, an HDMI monitor (`sbc_kms` picks its mode), user networking, the card.
fn tinker_machine(ram_bytes: usize, unix: f64, disk: Option<Box<dyn Disk>>) -> rockchip_rk3288::Machine {
    use rockchip_rk3288::HdmiMode;
    let mut m = rockchip_rk3288::Machine::with_ram_size(ram_bytes);
    let (w, h) = KMS.with(|c| c.get());
    m.connect_hdmi(&HdmiMode::find(w, h).unwrap_or(HdmiMode::MODES[1]));
    m.soc.periph.attach_rk808(unix as i64);
    m.connect_network(false, false);
    m.soc.periph.insert_sd(disk);
    m
}

/// Boot a Tinker Board the way `how` says.
fn tinker_boot(m: &mut rockchip_rk3288::Machine, how: &TinkerBoot) -> Result<(), String> {
    match &how.direct {
        Some((kernel, dtb, initrd, cmdline)) => rockchip_rk3288::boot::boot_linux(m, kernel, dtb, initrd.as_deref(), cmdline).map(|_| ()),
        None => rockchip_rk3288::boot::boot_from_card(m, "").map(|_| ()),
    }
}

/// Tinker Board S: boot a zImage with the stock device tree (patched the way U-Boot does) and
/// an optional initramfs; an empty command line is the board's default (console on UART2).
/// `unix_now` sets the RTC. Returns 0 on success.
#[no_mangle]
pub unsafe extern "C" fn sbc_boot_tinker(ram_mb: u32, unix_now: f64, k: *const u8, klen: usize, d: *const u8, dlen: usize,
                                         i: *const u8, ilen: usize, c: *const u8, clen: usize) -> i32 {
    let kernel = std::slice::from_raw_parts(k, klen).to_vec();
    let dtb = std::slice::from_raw_parts(d, dlen).to_vec();
    let initrd = (ilen > 0).then(|| std::slice::from_raw_parts(i, ilen).to_vec());
    let cmdline = if clen > 0 { String::from_utf8_lossy(std::slice::from_raw_parts(c, clen)).into_owned() } else {
        rockchip_rk3288::boot::DEFAULT_CMDLINE.to_string()
    };
    let how = TinkerBoot { unix0: unix_now, direct: Some((kernel, dtb, initrd, cmdline)) };
    let mut m = tinker_machine((ram_mb as usize) << 20, unix_now, None);
    if tinker_boot(&mut m, &how).is_err() { return -1; }
    ST.with(|s| *s.borrow_mut() = Some(State::new(Board::Tinker(Box::new(m), how), Some(0.0))));
    0
}

/// Tinker Board S: boot an SD card image (Armbian and similar) served by the page, the way
/// the board's U-Boot does (`/boot/armbianEnv.txt`, zImage, dtb, uInitrd). Returns 0 on success.
#[no_mangle]
pub unsafe extern "C" fn sbc_boot_tinker_sd(ram_mb: u32, unix_now: f64, sectors: f64) -> i32 {
    let Some(disk) = WebDisk::card(sectors) else { return -2 };
    let how = TinkerBoot { unix0: unix_now, direct: None };
    let mut m = tinker_machine((ram_mb as usize) << 20, unix_now, Some(disk));
    if tinker_boot(&mut m, &how).is_err() { return -2; }
    ST.with(|s| *s.borrow_mut() = Some(State::new(Board::Tinker(Box::new(m), how), Some(0.0))));
    0
}

/// An Orange Pi One as the CLI's `--board orangepi-one` builds it: the HDMI monitor in its
/// default mode, user networking, no SD card.
fn opi1_machine(ram_bytes: usize) -> allwinner_h3::Machine {
    let mut m = allwinner_h3::Machine::with_ram_size(ram_bytes);
    m.connect_network(false, false);
    m
}

/// Orange Pi One: boot a zImage with the stock device tree (patched the way U-Boot does) and an
/// optional initramfs; an empty command line is the board's default (console on UART0).
/// Returns 0 on success.
#[no_mangle]
pub unsafe extern "C" fn sbc_boot_opi1(ram_mb: u32, k: *const u8, klen: usize, d: *const u8, dlen: usize,
                                       i: *const u8, ilen: usize, c: *const u8, clen: usize) -> i32 {
    let kernel = std::slice::from_raw_parts(k, klen).to_vec();
    let dtb = std::slice::from_raw_parts(d, dlen).to_vec();
    let initrd = (ilen > 0).then(|| std::slice::from_raw_parts(i, ilen).to_vec());
    let cmdline = if clen > 0 { String::from_utf8_lossy(std::slice::from_raw_parts(c, clen)).into_owned() } else {
        allwinner_h3::boot::DEFAULT_CMDLINE.to_string()
    };
    let mut m = opi1_machine((ram_mb as usize) << 20);
    if allwinner_h3::boot::boot_linux(&mut m, &kernel, &dtb, initrd.as_deref(), &cmdline).is_err() { return -1; }
    ST.with(|s| *s.borrow_mut() = Some(State::new(Board::OrangePiOne(Box::new(m), (kernel, dtb, initrd, cmdline)), Some(0.0))));
    0
}

/// An Orange Pi RV as the CLI's `--board orangepi-rv` builds it: an HDMI monitor at the mode
/// `sbc_kms` picked (720p by default), user networking, the USB keyboard and tablet.
fn opirv_machine(ram_bytes: usize) -> starfive_jh7110::Machine {
    use starfive_jh7110::display::{Display, Mode};
    let mut m = starfive_jh7110::Machine::new(ram_bytes);
    let (w, h) = KMS.with(|c| c.get());
    m.soc.display = Display::new(Mode::find(w, h).unwrap_or(Mode::MODES[0]));
    m.connect_network(false, false);
    m
}

/// Orange Pi RV: boot OpenSBI (`fw_dynamic`, generic platform) and a RISC-V Linux `Image` with the
/// stock device tree (completed the way U-Boot does; mainline's gets the firmware framebuffer on
/// HDMI) and an optional initramfs; an empty command line is the board's default (console on
/// UART0). Returns 0 on success.
#[no_mangle]
pub unsafe extern "C" fn sbc_boot_opirv(ram_mb: u32, f: *const u8, flen: usize, k: *const u8, klen: usize, d: *const u8, dlen: usize,
                                        i: *const u8, ilen: usize, c: *const u8, clen: usize) -> i32 {
    let firmware = std::slice::from_raw_parts(f, flen).to_vec();
    let kernel = std::slice::from_raw_parts(k, klen).to_vec();
    let dtb = std::slice::from_raw_parts(d, dlen).to_vec();
    let initrd = (ilen > 0).then(|| std::slice::from_raw_parts(i, ilen).to_vec());
    let cmdline = if clen > 0 { String::from_utf8_lossy(std::slice::from_raw_parts(c, clen)).into_owned() } else {
        starfive_jh7110::boot::DEFAULT_CMDLINE.to_string()
    };
    let mut m = opirv_machine((ram_mb as usize) << 20);
    if starfive_jh7110::boot::boot_linux(&mut m, &firmware, &kernel, &dtb, initrd.as_deref(), &cmdline).is_err() { return -1; }
    for h in smp() + 1..m.harts.len() { m.state[h] = starfive_jh7110::HartState::Off; }
    ST.with(|s| *s.borrow_mut() = Some(State::new(Board::OrangePiRv(Box::new(m), firmware, (kernel, dtb, initrd, cmdline)), Some(0.0))));
    0
}

/// An Orange Pi 5 as the CLI's `--board orangepi5 --no-display --net local` builds it, with the
/// first `sbc_smp` cores: the serial console is the demo, so no HDMI framebuffer.
fn opi5_machine(ram_bytes: usize) -> Option<rk3588::Machine> {
    let mut m = rk3588::Machine::with_cpus(ram_bytes, smp().min(rk3588::MAX_CPUS)).ok()?;
    m.connect_network(false, false);
    Some(m)
}

/// Orange Pi 5: boot an arm64 Linux `Image` with the emulator's generated device tree and an
/// optional initramfs; an empty command line is the board's default (console on UART2).
/// Returns 0 on success.
#[no_mangle]
pub unsafe extern "C" fn sbc_boot_opi5(ram_mb: u32, k: *const u8, klen: usize, i: *const u8, ilen: usize, c: *const u8, clen: usize) -> i32 {
    let kernel = std::slice::from_raw_parts(k, klen).to_vec();
    let initrd = (ilen > 0).then(|| std::slice::from_raw_parts(i, ilen).to_vec());
    let cmdline = if clen > 0 { String::from_utf8_lossy(std::slice::from_raw_parts(c, clen)).into_owned() } else {
        rk3588::boot::DEFAULT_CMDLINE.to_string()
    };
    let Some(mut m) = opi5_machine((ram_mb as usize) << 20) else { return -1 };
    if rk3588::boot::boot_linux(&mut m, &kernel, initrd.as_deref(), &cmdline).is_err() { return -1; }
    ST.with(|s| *s.borrow_mut() = Some(State::new(Board::OrangePi5(Box::new(m), (kernel, initrd, cmdline)), Some(0.0))));
    0
}

/// A Jetson Nano as the CLI's `--board jetson-nano --net local` builds it, with the first `sbc_smp`
/// cores and the HDMI monitor of `sbc_kms`.
fn nano_machine(ram_bytes: usize) -> Option<tegra210::Machine> {
    use peripherals::dw_hdmi::Mode;
    let mut m = tegra210::Machine::with_cpus(ram_bytes, smp().min(tegra210::NCPU)).ok()?;
    let (w, h) = KMS.with(|c| c.get());
    if let Some(mode) = Mode::MODES.iter().find(|m| (m.width, m.height) == (w, h)) {
        m.connect_monitor(mode);
    }
    m.connect_network(false, false);
    Some(m)
}

/// Jetson Nano: boot an arm64 Linux kernel with the board's stock device tree and an optional
/// initramfs; an empty command line is the board's default (console on UART A). Returns 0 on
/// success.
///
/// # Safety
/// The pointers are buffers of the lengths given.
#[no_mangle]
#[allow(clippy::too_many_arguments)]
pub unsafe extern "C" fn sbc_boot_nano(ram_mb: u32, k: *const u8, klen: usize, d: *const u8, dlen: usize, i: *const u8, ilen: usize, c: *const u8, clen: usize) -> i32 {
    let kernel = std::slice::from_raw_parts(k, klen).to_vec();
    let dtb = std::slice::from_raw_parts(d, dlen).to_vec();
    let initrd = (ilen > 0).then(|| std::slice::from_raw_parts(i, ilen).to_vec());
    let cmdline = if clen > 0 { String::from_utf8_lossy(std::slice::from_raw_parts(c, clen)).into_owned() } else {
        tegra210::boot::DEFAULT_CMDLINE.to_string()
    };
    let Some(mut m) = nano_machine((ram_mb as usize) << 20) else { return -1 };
    if tegra210::boot::boot_linux(&mut m, &kernel, &dtb, initrd.as_deref(), &cmdline).is_err() { return -1; }
    ST.with(|s| *s.borrow_mut() = Some(State::new(Board::JetsonNano(Box::new(m), (kernel, dtb, initrd, cmdline)), Some(0.0))));
    0
}

/// ESP32-S31-Function-CoreBoard-1 (`esp32s31/examples/s31_linux.rs`): the mask ROM ELF and the
/// flash image as `esptool write-flash 0x0` writes it; the chip boots from its ROM. Returns 0 on
/// success.
#[no_mangle]
pub unsafe extern "C" fn sbc_boot_s31(r: *const u8, rlen: usize, f: *const u8, flen: usize) -> i32 {
    use esp_soc::SocBus as _;
    let (rom, flash) = (std::slice::from_raw_parts(r, rlen), std::slice::from_raw_parts(f, flen));
    let mut m = esp32s31::machine([0x10, 0x20, 0x30, 0x40, 0x50, 0x60], 16 << 20);
    if m.bus.write_flash(0, flash).is_err()
        || m.bus.set_board_inputs(esp32s31::soc::BoardInputs::function_coreboard_1()).is_err()
        || m.bus.load_mask_rom(rom).is_err() { return -1; }
    m.bus.set_strap(0x10);
    m.console.mask = 2;
    m.console.capture = true;
    m.boot_rom();
    m.cores[0].x[2] = esp32s31::ROM_STACK;
    ST.with(|s| *s.borrow_mut() = Some(State::new(Board::Esp32S31(Box::new(m), Default::default()), Some(0.0))));
    0
}

/// Run the S31 for `budget` scheduling steps. Its time is the chip's cycle count, which only
/// moves forward as it runs (and skips ahead while every hart waits), so it never reports Idle;
/// a chip reset (`reboot`) goes back through the ROM, as on the board.
fn s31_run(m: &mut esp32s31::Machine, rx: &mut std::collections::VecDeque<u8>, budget: u64) -> Stop {
    use esp_soc::SocBus as _;
    // UART0 takes input once Linux has enabled its clocks and receiver: probe with no bytes,
    // then feed through the SoC (which also updates the interrupt lines)
    if !rx.is_empty() && m.bus.inner.uart_input(&[]).is_ok() { m.bus.uart_input(0, rx.make_contiguous()); rx.clear(); }
    match m.run(budget) {
        esp_soc::Stop::MaxInsns => Stop::Budget,
        esp_soc::Stop::SwReset => { m.reboot(); Stop::Budget }
        esp_soc::Stop::Unimplemented(pc, insn) => Stop::Unimplemented(pc as u64, insn),
        _ => Stop::PowerOff,
    }
}

/// Which board is running: 0 none, 1 the Pi 4, 2 the Tinker Board, 3 the Orange Pi One, 4 the
/// Orange Pi RV, 5 the Orange Pi 5, 6 the ESP32-S31, 7 the Jetson Nano.
#[no_mangle]
pub extern "C" fn sbc_board() -> u32 {
    with(|s| match s.m { Board::Pi(_) => 1, Board::Tinker(..) => 2, Board::OrangePiOne(..) => 3, Board::OrangePiRv(..) => 4, Board::OrangePi5(..) => 5, Board::Esp32S31(..) => 6, Board::JetsonNano(..) => 7 }).unwrap_or(0)
}

/// Turn the block JIT on (1) or off (0); returns whether it is active.
#[no_mangle]
pub extern "C" fn sbc_jit(on: u32) -> u32 {
    with(|s| match &mut s.m {
        Board::Pi(m) => if on != 0 { m.enable_jit() as u32 } else { m.disable_jit(); 0 },
        Board::Tinker(m, _) => { m.set_jit(on != 0); m.jit_enabled() as u32 }
        Board::OrangePiOne(m, _) => { m.set_jit(on != 0); m.jit_enabled() as u32 }
        Board::OrangePiRv(m, ..) => { m.set_jit(on != 0); m.jit_enabled() as u32 }
        Board::OrangePi5(m, _) => if on != 0 { m.enable_jit() as u32 } else { for c in &mut m.cpus { c.jit = None; } 0 },
        Board::JetsonNano(m, _) => if on != 0 { m.enable_jit() as u32 } else { for c in &mut m.cpus { c.jit = None; } 0 },
        Board::Esp32S31(m, _) => { for c in &mut m.cores { c.blocks.jit_on = on != 0 && riscv_rv32::jit::AVAILABLE; } (on != 0 && riscv_rv32::jit::AVAILABLE) as u32 }
    }).unwrap_or(0)
}

/// JIT profiling: count the instructions compiled blocks hand to the interpreter (1 on).
#[no_mangle]
pub extern "C" fn sbc_jit_profile(on: u32) { arm_a64::jit::PROFILE.store(on != 0, std::sync::atomic::Ordering::Relaxed); }

/// The most frequent interpreter fallbacks: up to `n` (word, count) pairs as u64s at `out`.
#[no_mangle]
pub unsafe extern "C" fn sbc_jit_fallbacks(out: *mut u64, n: usize) -> usize {
    let f = arm_a64::jit::profile_fallbacks();
    let k = f.len().min(n);
    for (i, (w, c)) in f.iter().take(k).enumerate() { *out.add(2 * i) = *w as u64; *out.add(2 * i + 1) = *c; }
    k
}

/// JIT: executions before a block is compiled.
#[no_mangle]
pub extern "C" fn sbc_jit_hot(n: u32) {
    with(|s| match &mut s.m {
        Board::Pi(m) => { if let Some(j) = m.jit_mut() { j.hot = n.max(1); } }
        Board::Tinker(m, _) => for c in &mut m.cpus { c.jit_set_hot(n) },
        Board::OrangePiOne(m, _) => for c in &mut m.cpus { c.jit_set_hot(n) },
        Board::OrangePiRv(m, ..) => m.set_jit_hot(n),
        Board::OrangePi5(m, _) => for c in &mut m.cpus { if let Some(j) = c.jit.as_deref_mut() { j.hot = n.max(1); } },
        Board::JetsonNano(m, _) => for c in &mut m.cpus { if let Some(j) = c.jit.as_deref_mut() { j.hot = n.max(1); } },
        Board::Esp32S31(m, _) => for c in &mut m.cores { c.blocks.jit_hot = n.clamp(1, u16::MAX as u32) as u16; },
    });
}

/// JIT: guest instructions retired inside compiled blocks (per mille of all).
#[no_mangle]
pub extern "C" fn sbc_jit_coverage() -> u32 { with_pi(|m| m.jit().map_or(0, |j| (j.exec_insns * 1000 / m.instret().max(1)) as u32)).unwrap_or(0) }

/// JIT verify mode (every block re-run in the interpreter): 1 on.
#[no_mangle]
pub extern "C" fn sbc_jit_verify(on: u32) { with_pi(|m| { if let Some(j) = m.jit_mut() { j.verify = on != 0; } }); }

/// The first JIT/interpreter mismatch found in verify mode (length; text via sbc_uart_ptr-style buffer).
#[no_mangle]
pub extern "C" fn sbc_jit_report(out: *mut u8, cap: usize) -> usize {
    with_pi(|m| {
        let r = m.jit().and_then(|j| j.report.clone()).unwrap_or_default();
        let n = r.len().min(cap);
        // SAFETY: the caller provides `cap` writable bytes
        unsafe { std::ptr::copy_nonoverlapping(r.as_ptr(), out, n) };
        n
    }).unwrap_or(0)
}

/// JIT counters: compiled blocks.
#[no_mangle]
pub extern "C" fn sbc_jit_blocks() -> f64 {
    with(|s| match &s.m {
        Board::Pi(m) => m.jit().map(|j| j.compiled as f64).unwrap_or(0.0),
        Board::Tinker(m, _) => m.cpus.iter().filter_map(|c| c.jit_stats()).map(|(c, _, _)| c as f64).sum(),
        Board::OrangePiOne(m, _) => m.cpus.iter().filter_map(|c| c.jit_stats()).map(|(c, _, _)| c as f64).sum(),
        Board::OrangePiRv(m, ..) => m.jit_compiled() as f64,
        Board::OrangePi5(m, _) => m.cpus.iter().filter_map(|c| c.jit.as_ref()).map(|j| j.compiled as f64).sum(),
        Board::JetsonNano(m, _) => m.cpus.iter().filter_map(|c| c.jit.as_ref()).map(|j| j.compiled as f64).sum(),
        Board::Esp32S31(m, _) => m.cores.iter().map(|c| c.blocks.compiled as f64).sum(),
    }).unwrap_or(0.0)
}

/// USB keyboard: press (down=1) or release (down=0) a key by HID usage code.
#[no_mangle]
pub extern "C" fn sbc_key(usage: u32, down: u32) {
    with(|s| match &mut s.m {
        Board::Pi(m) => m.soc.devs.kbd.key(usage as u8, down != 0),
        Board::OrangePiRv(m, ..) => m.soc.usb.kbd.key(usage as u8, down != 0),
        Board::OrangePi5(m, _) => { m.soc.usb.kbd.key(usage as u8, down != 0); m.soc.dirty = true; }
        Board::JetsonNano(m, _) => { m.soc.usb.kbd.key(usage as u8, down != 0); m.soc.dirty = true; }
        _ => {}
    });
}

/// The USB tablet: buttons (bit 0 left, 1 right, 2 middle), position 0..=32767, wheel clicks.
#[no_mangle]
pub extern "C" fn sbc_pointer(buttons: u32, x: u32, y: u32, wheel: i32) {
    let (b, x, y, w) = (buttons as u8 & 7, x.min(32767) as u16, y.min(32767) as u16, wheel.clamp(-127, 127) as i8);
    with(|s| match &mut s.m {
        Board::Pi(m) => m.soc.devs.tablet.pointer(b, x, y, w),
        Board::OrangePiRv(m, ..) => m.soc.usb.tablet.pointer(b, x, y, w),
        Board::OrangePi5(m, _) => { m.soc.usb.tablet.pointer(b, x, y, w); m.soc.dirty = true; }
        Board::JetsonNano(m, _) => { m.soc.usb.tablet.pointer(b, x, y, w); m.soc.dirty = true; }
        _ => {}
    });
}

/// What the guest played on the I²S DAC since the last call: the number of 16-bit samples
/// (interleaved stereo) now at `sbc_audio_ptr`, played at `sbc_audio_rate` Hz.
#[no_mangle]
pub extern "C" fn sbc_audio_take() -> usize {
    with(|s| {
        s.audio = match &mut s.m { Board::Pi(m) => std::mem::take(&mut m.soc.i2s.out), _ => Vec::new() };
        s.audio.len()
    }).unwrap_or(0)
}
#[no_mangle]
pub extern "C" fn sbc_audio_ptr() -> *const i16 { with(|s| s.audio.as_ptr()).unwrap_or(std::ptr::null()) }
#[no_mangle]
pub extern "C" fn sbc_audio_rate() -> u32 { with_pi(|m| m.soc.i2s.rate).unwrap_or(0) }

/// USB keyboard: type one byte of text (US layout).
#[no_mangle]
pub extern "C" fn sbc_type(b: u32) {
    with(|s| match &mut s.m {
        Board::Pi(m) => m.soc.devs.kbd.type_byte(b as u8),
        Board::OrangePiRv(m, ..) => m.soc.usb.kbd.type_byte(b as u8),
        Board::OrangePi5(m, _) => { m.soc.usb.kbd.type_byte(b as u8); m.soc.dirty = true; }
        Board::JetsonNano(m, _) => { m.soc.usb.kbd.type_byte(b as u8); m.soc.dirty = true; }
        _ => {}
    });
}

/// Run up to `budget` instructions with the page's wall clock `wall_ns` (real-time pacing).
/// Returns 0 = budget used, 1 = idle (wait `sbc_wait_ms()` or until input), 2 = stopped.
#[no_mangle]
pub extern "C" fn sbc_run(budget: f64, wall_ns: f64) -> i32 {
    with(|s| {
        if s.stop.is_some() { return 2; }
        let off = *s.wall_offset.get_or_insert(s.m.now() as f64 - wall_ns);
        let wall = (wall_ns + off).max(0.0) as u64;
        let end = s.m.instret() + budget as u64;
        loop {
            let left = end.saturating_sub(s.m.instret());
            if left == 0 { break 0; }
            match s.m.run(left) {
                Stop::Budget => break 0,
                Stop::Idle(t) => {
                    let now = s.m.now();
                    let target = t.min(wall.max(now));
                    if target > now {
                        s.m.idle_until(target);
                        // the Tinker Board's clock moves in whole timer ticks: a target between
                        // two ticks leaves it where it was, so wait for the wall clock instead
                        if s.m.now() > now { continue; }
                    }
                    s.wait_ms = (t.saturating_sub(now) as f64 / 1e6).min(20.0);
                    break 1;
                }
                Stop::Reset => {
                    let restarted = match &mut s.m {
                        Board::Pi(old) => pi_restart(old).map(Board::Pi),
                        Board::Tinker(old, how) => tinker_restart(old, how),
                        Board::OrangePiOne(old, how) => opi1_restart(old, how),
                        Board::OrangePiRv(old, fw, how) => opirv_restart(old, fw, how),
                        Board::OrangePi5(old, how) => opi5_restart(old, how),
                        Board::JetsonNano(old, how) => nano_restart(old, how),
                        Board::Esp32S31(..) => None,
                    };
                    match restarted {
                        Some(m) => s.m = m,
                        None => { s.stop = Some(Stop::Reset); break 2; }
                    }
                }
                other => { s.stop = Some(other); break 2; }
            }
        }
    }).unwrap_or(2)
}

/// Start the Pi again from its SD card (after a reset).
fn pi_restart(old: &mut Machine) -> Option<Machine> {
    let mut d = old.soc.sd.disk.take()?;
    let b = bcm2711::boot::read_sd_boot(d.as_mut()).ok()?;
    let mut m = Machine::with_cpus(old.soc.ram.len(), smp());
    m.soc.sd = bcm2711::sdhci::Sdhci::new(Some(d));
    add_gpu(&mut m);
    bcm2711::boot::boot_linux(&mut m, &b.kernel, b.initramfs.as_deref(), &b.cmdline, true).ok()?;
    Some(m)
}

/// Start a Tinker Board again the way it was started, with the same card (after a reset).
fn tinker_restart(old: &mut rockchip_rk3288::Machine, how: &mut TinkerBoot) -> Option<Board> {
    let disk = old.soc.periph.mmc[rockchip_rk3288::devices::SDMMC].card.take_disk();
    let unix = how.unix0 + old.now() as f64 / 1e9;
    let mut m = tinker_machine(old.soc.ram.mem.len(), unix, disk);
    let jit = old.jit_enabled();
    tinker_boot(&mut m, how).ok()?;
    m.set_jit(jit);
    let how = TinkerBoot { unix0: unix, direct: how.direct.take() };
    Some(Board::Tinker(Box::new(m), how))
}

/// Start an Orange Pi One again from what it was booted with (after a reset).
fn opi1_restart(old: &allwinner_h3::Machine, how: &mut DirectBoot) -> Option<Board> {
    let mut m = opi1_machine(old.soc.ram.mem.len());
    let (kernel, dtb, initrd, cmdline) = std::mem::take(how);
    allwinner_h3::boot::boot_linux(&mut m, &kernel, &dtb, initrd.as_deref(), &cmdline).ok()?;
    m.set_jit(old.jit_enabled());
    Some(Board::OrangePiOne(Box::new(m), (kernel, dtb, initrd, cmdline)))
}

/// Start an Orange Pi RV again from what it was booted with (after `reboot`).
fn opirv_restart(old: &starfive_jh7110::Machine, firmware: &mut Vec<u8>, how: &mut DirectBoot) -> Option<Board> {
    let mut m = opirv_machine(old.soc.ram.mem.len());
    let firmware = std::mem::take(firmware);
    let (kernel, dtb, initrd, cmdline) = std::mem::take(how);
    starfive_jh7110::boot::boot_linux(&mut m, &firmware, &kernel, &dtb, initrd.as_deref(), &cmdline).ok()?;
    for (h, s) in old.state.iter().enumerate() { if *s == starfive_jh7110::HartState::Off { m.state[h] = *s; } }
    m.set_jit(old.jit_enabled());
    Some(Board::OrangePiRv(Box::new(m), firmware, (kernel, dtb, initrd, cmdline)))
}

/// Start an Orange Pi 5 again from what it was booted with (after `reboot`).
fn opi5_restart(old: &rk3588::Machine, how: &mut Opi5Boot) -> Option<Board> {
    let mut m = opi5_machine(old.soc.ram.len())?;
    let (kernel, initrd, cmdline) = std::mem::take(how);
    rk3588::boot::boot_linux(&mut m, &kernel, initrd.as_deref(), &cmdline).ok()?;
    if old.cpus.iter().any(|c| c.jit.is_some()) { m.enable_jit(); }
    Some(Board::OrangePi5(Box::new(m), (kernel, initrd, cmdline)))
}

/// Start a Jetson Nano again from what it was booted with (after `reboot`).
fn nano_restart(old: &tegra210::Machine, how: &mut DirectBoot) -> Option<Board> {
    let mut m = nano_machine(old.soc.ram.len())?;
    let (kernel, dtb, initrd, cmdline) = std::mem::take(how);
    tegra210::boot::boot_linux(&mut m, &kernel, &dtb, initrd.as_deref(), &cmdline).ok()?;
    if old.cpus.iter().any(|c| c.jit.is_some()) { m.enable_jit(); }
    Some(Board::JetsonNano(Box::new(m), (kernel, dtb, initrd, cmdline)))
}

#[no_mangle]
pub extern "C" fn sbc_wait_ms() -> f64 { with(|s| s.wait_ms).unwrap_or(0.0) }

/// Advance idle time to at most `wall_ns` (after a wait) and re-evaluate interrupts.
#[no_mangle]
pub extern "C" fn sbc_wake(wall_ns: f64) {
    with(|s| { let t = ((wall_ns + s.wall_offset.unwrap_or(0.0)).max(0.0) as u64).max(s.m.now()); s.m.idle_until(t) });
}

/// Resume a Pi snapshot (made natively with the same SD image) instead of cold-booting.
#[no_mangle]
pub unsafe extern "C" fn sbc_restore(ram_mb: u32, sectors: f64, snap: *const u8, len: usize) -> i32 {
    let mut m = Machine::with_cpus((ram_mb as usize) << 20, smp());
    m.soc.sd = bcm2711::sdhci::Sdhci::new(WebDisk::card(sectors));
    m.connect_network(false, false);
    let snap = std::slice::from_raw_parts(snap, len);
    // a zstd frame holds the snapshot's inflated body (tools/snap-zstd.sh); otherwise it is the
    // emulator's own deflate format
    let r = if snap.starts_with(&[0x28, 0xb5, 0x2f, 0xfd]) {
        // the frame records the unpacked size (tools/snap-zstd.sh): one allocation, not a vector
        // that doubles past the browser's memory limit
        let size = ruzstd::frame::read_frame_header(snap).map(|(f, _)| f.header.frame_content_size()).unwrap_or(0);
        let mut raw = Vec::with_capacity(size as usize);
        let ok = ruzstd::StreamingDecoder::new(snap).map_err(|e| e.to_string())
            .and_then(|mut d| std::io::Read::read_to_end(&mut d, &mut raw).map_err(|e| e.to_string()));
        match ok { Ok(_) => m.restore_raw(&raw), Err(e) => Err(e) }
    } else {
        m.restore(snap)
    };
    if r.is_err() { return -1; }
    ST.with(|s| *s.borrow_mut() = Some(State::new(Board::Pi(m), None)));
    0
}

#[no_mangle]
pub extern "C" fn sbc_uart_in(b: u32) {
    with(|s| match &mut s.m {
        Board::Pi(m) => { if m.soc.uart.rx_room() { m.soc.uart.push_rx(b as u8); m.soc.dirty = true; } }
        Board::Tinker(m, _) => { m.serial_input(b as u8); }
        Board::OrangePiOne(m, _) => { m.serial_input(b as u8); }
        Board::OrangePiRv(m, ..) => { m.serial_input(b as u8); }
        Board::OrangePi5(m, _) => { m.serial_input(b as u8); }
        Board::JetsonNano(m, _) => { m.serial_input(b as u8); }
        Board::Esp32S31(_, rx) => rx.push_back(b as u8),
    });
}

#[no_mangle]
pub extern "C" fn sbc_uart_room() -> i32 {
    with(|s| match &s.m {
        Board::Pi(m) => m.soc.uart.rx_room() as i32,
        Board::Tinker(m, _) => m.soc.uart.rx_room() as i32,
        Board::OrangePiOne(m, _) => m.soc.uart.rx_room() as i32,
        Board::OrangePiRv(m, ..) => m.soc.uart.rx_room() as i32,
        Board::OrangePi5(m, _) => m.soc.uart.rx_room() as i32,
        Board::JetsonNano(m, _) => m.soc.uart.rx_room() as i32,
        Board::Esp32S31(_, rx) => (rx.len() < 64) as i32,
    }).unwrap_or(0)
}

/// Move pending UART output to an internal buffer; returns its length (`sbc_uart_ptr` for data).
#[no_mangle]
pub extern "C" fn sbc_uart_take() -> usize {
    with(|s| {
        s.uart = match &mut s.m {
            Board::Pi(m) => std::mem::take(&mut m.soc.uart.out),
            Board::Tinker(m, _) => m.take_serial_output(),
            Board::OrangePiOne(m, _) => m.take_serial_output(),
            Board::OrangePiRv(m, ..) => m.take_serial_output(),
            Board::OrangePi5(m, _) => std::mem::take(&mut m.soc.uart.out),
            Board::JetsonNano(m, _) => std::mem::take(&mut m.soc.uart.out),
            Board::Esp32S31(m, _) => std::mem::take(&mut m.console.uart0),
        };
        s.uart.len()
    }).unwrap_or(0)
}
#[no_mangle]
pub extern "C" fn sbc_uart_ptr() -> *const u8 { with(|s| s.uart.as_ptr()).unwrap_or(std::ptr::null()) }

/// Render the framebuffer as RGBA; returns a pointer (0 if no framebuffer), see sbc_fb_w/h.
#[no_mangle]
pub extern "C" fn sbc_fb_render() -> *const u8 {
    with(|s| {
        let size = match &s.m {
            Board::Pi(m) => m.soc.render(&mut s.fb),
            Board::Tinker(m, _) => m.soc.render(&mut s.fb),
            Board::OrangePiOne(m, _) => m.soc.render(&mut s.fb),
            Board::OrangePiRv(m, ..) => m.soc.render(&mut s.fb),
            Board::OrangePi5(m, _) => m.soc.render(&mut s.fb),
            Board::JetsonNano(m, _) => m.render(&mut s.fb),
            Board::Esp32S31(..) => None,
        };
        match size {
            Some((w, h)) => { s.fb_w = w; s.fb_h = h; s.fb.as_ptr() }
            None => std::ptr::null(),
        }
    }).unwrap_or(std::ptr::null())
}
#[no_mangle]
pub extern "C" fn sbc_fb_w() -> u32 { with(|s| s.fb_w).unwrap_or(0) }
#[no_mangle]
pub extern "C" fn sbc_fb_h() -> u32 { with(|s| s.fb_h).unwrap_or(0) }

/// Render jobs the 3D GPU has finished (one per frame for most programs), 0 without a GPU.
#[no_mangle]
pub extern "C" fn sbc_gpu_frames() -> f64 { with_pi(|m| m.soc.v3d.as_ref().map_or(0.0, |g| g.counters().0[1] as f64)).unwrap_or(0.0) }
/// Host milliseconds spent in GPU jobs.
#[no_mangle]
pub extern "C" fn sbc_gpu_ms() -> f64 { with_pi(|m| m.soc.v3d.as_ref().map_or(0.0, |g| g.counters().2.iter().sum::<u64>() as f64 / 1e6)).unwrap_or(0.0) }

#[no_mangle]
pub extern "C" fn sbc_insns() -> f64 { with(|s| s.m.instret() as f64).unwrap_or(0.0) }
#[no_mangle]
pub extern "C" fn sbc_now_ns() -> f64 { with(|s| s.m.now() as f64).unwrap_or(0.0) }
/// Stop reason: 0 running, 1 unimplemented instruction (see sbc_stop_pc), 2 other.
#[no_mangle]
pub extern "C" fn sbc_stop_kind() -> i32 { with(|s| match s.stop { None => 0, Some(Stop::Unimplemented(..)) => 1, _ => 2 }).unwrap_or(2) }
#[no_mangle]
pub extern "C" fn sbc_stop_pc() -> f64 {
    with(|s| match s.stop {
        Some(Stop::Unimplemented(pc, _)) => pc as f64,
        _ => match &s.m {
            Board::Pi(m) => m.cpu().pc as f64,
            Board::Tinker(m, _) => m.cpus[m.last_stop_core].r[15] as f64,
            Board::OrangePiOne(m, _) => m.cpus[m.last_stop_core].r[15] as f64,
            Board::OrangePiRv(m, ..) => m.harts.iter().map(|h| h.pc).next().unwrap_or(0) as f64,
            Board::OrangePi5(m, _) => m.cpu().pc as f64,
            Board::JetsonNano(m, _) => m.cpu().pc as f64,
            Board::Esp32S31(m, _) => m.cores[0].pc as f64,
        },
    }).unwrap_or(0.0)
}
#[no_mangle]
pub extern "C" fn sbc_stop_insn() -> u32 { with(|s| match s.stop { Some(Stop::Unimplemented(_, i)) => i, _ => 0 }).unwrap_or(0) }

/// GPIO line level as the pins see it (0/1).
#[no_mangle]
pub extern "C" fn sbc_gpio_level(pin: u32) -> u32 { with_pi(|m| m.soc.gpio.level(pin as usize) as u32).unwrap_or(0) }

/// Drive a GPIO input from the page: 1 high, 0 low, -1 released (pull decides).
#[no_mangle]
pub extern "C" fn sbc_gpio_input(pin: u32, v: i32) {
    with_pi(|m| { m.soc.gpio.set_input(pin as usize, match v { 1 => Some(true), 0 => Some(false), _ => None }); m.soc.dirty = true; });
}

/// Sensor inputs: kind 0 temperature (degC), 1 pressure (hPa), 2 humidity (%RH).
#[no_mangle]
pub extern "C" fn sbc_set_sensor(kind: u32, v: f64) {
    with_pi(|m| m.set_sensor(match kind { 0 => "temp", 1 => "pressure", _ => "humidity" }, v));
}

/// MCP3008 channel input voltage.
#[no_mangle]
pub extern "C" fn sbc_set_adc(ch: u32, volts: f64) { with_pi(|m| { if ch < 8 { m.soc.spi.adc.volts[ch as usize] = volts; } }); }

/// Self-test for the JIT: run `len` bytes of A64 code at 0x1000 in a 1 MiB RAM (x1 = 0x80000
/// as a data pointer) until `brk`, with the JIT compiling on first use (`jit` = 1) or not.
/// Returns a pointer to 32 u64: x0..x30 and NZCV.
#[no_mangle]
pub unsafe extern "C" fn sbc_selftest(code: *const u8, len: usize, jit: u32) -> *const u64 {
    use emu_core::{Core, FlatRam, Trap};
    static mut OUT: [u64; 32] = [0; 32];
    let mut ram = FlatRam::new(0, 1 << 20);
    let _ = ram.load(0x1000, std::slice::from_raw_parts(code, len));
    for k in 0..0x1000u64 { let _ = emu_core::Bus::write64(&mut ram, 0x80000 + 8 * k, 0x0123_4567_89ab_cdef ^ (k * 0x9e37_79b9)); }
    let mut cpu = arm_a64::Cpu::new();
    cpu.brk_to_host = true;
    cpu.el = 1;
    cpu.pc = 0x1000;
    cpu.sp[1] = 0xf_0000;
    cpu.x[1] = 0x80000;
    cpu.x[2] = 0x80100;
    cpu.sys.cpacr_el1 = 3 << 20;
    cpu.ram = ram.mem.as_mut_ptr();
    cpu.ram_len = ram.mem.len() as u64;
    if jit != 0 { cpu.jit = arm_a64::jit::Jit::with(1 << 16, 1); cpu.chain_until = u64::MAX; }
    for _ in 0..100_000 {
        match cpu.step(&mut ram) { Err(Trap::Brk(_)) => break, Err(Trap::Unimplemented(..)) => break, _ => {} }
    }
    let out = &mut *std::ptr::addr_of_mut!(OUT);
    for k in 0..31 { out[k] = cpu.x[k]; }
    out[31] = cpu.nzcv as u64;
    out.as_ptr()
}

// The A32/T32 JIT's random differential test (arm-a32/tests/jit_diff.rs), here with the
// wasm backend: tools/wasm-a32-jit-diff.mjs runs it.
#[path = "../../arm-a32/tests/common/jit_gen.rs"]
mod a32_jit_gen;

thread_local! { static REPORT: RefCell<String> = const { RefCell::new(String::new()) }; }

/// Run `cases` random cases from `seed`: A32 (`thumb` 0) or Thumb (1), straight-line
/// (`loops` 0) or in loops (1; compiled regions may loop inside wasm). Returns the number of
/// failures; the first one's description is at `sbc_report_ptr` (`sbc_report_len` bytes).
#[no_mangle]
pub extern "C" fn sbc_a32_jit_diff(thumb: u32, loops: u32, cases: u32, seed: f64) -> u32 {
    use a32_jit_gen::*;
    let mut rng = Rng(seed as u64 | 1);
    let mut failures = 0;
    for case in 0..cases {
        let ((arm, code16), s) = match (thumb != 0, loops != 0) {
            (false, false) => { let (c, s) = arm_case(&mut rng); ((c, Vec::new()), s) }
            (false, true) => { let (c, s) = arm_loop_case(&mut rng); ((c, Vec::new()), s) }
            (true, false) => { let (c, s) = thumb_case(&mut rng); ((Vec::new(), c), s) }
            (true, true) => { let (c, s) = thumb_loop_case(&mut rng); ((Vec::new(), c), s) }
        };
        let chain = if loops != 0 { u64::MAX } else { 0 };
        if let Err(e) = compare_case(thumb != 0, &arm, &code16, s, chain) {
            if failures == 0 {
                let dis: Vec<String> = if thumb != 0 { code16.iter().map(|h| format!("{h:04x}")).collect() } else { arm.iter().map(|w| format!("{w:08x}")).collect() };
                REPORT.with(|r| *r.borrow_mut() = format!("case {case} (seed {s}): {e}\ncode {}", dis.join(" ")));
            }
            failures += 1;
        }
    }
    failures
}

/// Run one case given as code (32-bit words for A32, halfwords for Thumb) from `seed`.
#[no_mangle]
pub unsafe extern "C" fn sbc_a32_jit_case(thumb: u32, code: *const u32, len: usize, seed: f64, chain: u32) -> u32 {
    let words = std::slice::from_raw_parts(code, len);
    let code16: Vec<u16> = words.iter().map(|&w| w as u16).collect();
    let r = a32_jit_gen::compare_case(thumb != 0, words, &code16, seed as u64, if chain != 0 { u64::MAX } else { 0 });
    REPORT.with(|rep| *rep.borrow_mut() = r.as_ref().err().cloned().unwrap_or_default());
    r.is_err() as u32
}

#[no_mangle]
pub extern "C" fn sbc_report_ptr() -> *const u8 { REPORT.with(|r| r.borrow().as_ptr()) }
#[no_mangle]
pub extern "C" fn sbc_report_len() -> usize { REPORT.with(|r| r.borrow().len()) }

/// The RV64 JIT's random differential test (riscv64::jit_check) with the wasm backend:
/// `cases` random programs from `seed`, in loops (`loops` 1) or straight, in S mode with Sv39
/// (`smode` 1) or M mode, compiling an entry after `hot` executions. Returns the number of
/// failures; the first one's description is at `sbc_report_ptr` (`sbc_report_len` bytes).
/// tools/wasm-rv-jit-diff.mjs runs it.
#[no_mangle]
pub extern "C" fn sbc_rv_jit_diff(loops: u32, smode: u32, cases: u32, seed: f64, hot: u32) -> u32 {
    let (fails, first) = riscv64::jit_check::run_cases(cases, seed as u64, loops != 0, smode != 0, hot.max(1));
    REPORT.with(|r| *r.borrow_mut() = first.unwrap_or_default());
    fails
}
