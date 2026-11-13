//! C ABI over the Pi 4B machine for the browser (no wasm-bindgen; see web/worker.js).
//! Single-threaded: one machine per module instance, driven from a Web Worker.
#![allow(clippy::missing_safety_doc)]
use bcm2711::sdhci::Disk;
use bcm2711::{Machine, Stop};
use std::cell::RefCell;
use std::collections::HashMap;

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
struct WebDisk { sectors: u64, overlay: HashMap<u64, Box<[u8; 512]>> }

impl Disk for WebDisk {
    fn sectors(&self) -> u64 { self.sectors }
    fn read(&mut self, lba: u64, out: &mut [u8]) {
        // SAFETY: `out` is a valid buffer of whole sectors; the import writes exactly that many
        unsafe { sbc_disk_read(lba as f64, (out.len() / 512) as u32, out.as_mut_ptr()) };
        if !self.overlay.is_empty() {
            for (k, chunk) in out.chunks_mut(512).enumerate() {
                if let Some(s) = self.overlay.get(&(lba + k as u64)) { chunk.copy_from_slice(&s[..chunk.len()]); }
            }
        }
    }
    fn overlay(&self) -> Vec<(u64, Vec<u8>)> { self.overlay.iter().map(|(k, s)| (*k, s.to_vec())).collect() }
    fn write(&mut self, lba: u64, data: &[u8]) {
        for (k, chunk) in data.chunks(512).enumerate() {
            let mut s = Box::new([0u8; 512]);
            s[..chunk.len()].copy_from_slice(chunk);
            self.overlay.insert(lba + k as u64, s);
        }
    }
}

struct State {
    m: Machine,
    fb: Vec<u8>,
    fb_w: u32,
    fb_h: u32,
    uart: Vec<u8>,
    wait_ms: f64,
    stop: Option<Stop>,
    /// added to the page's wall clock so a restored machine's time keeps running (ns)
    wall_offset: Option<f64>,
}

thread_local! { static ST: RefCell<Option<State>> = const { RefCell::new(None) }; }

fn with<R>(f: impl FnOnce(&mut State) -> R) -> Option<R> { ST.with(|s| s.borrow_mut().as_mut().map(f)) }

/// Allocate `len` bytes the page can copy a file into (leaked until the page reloads).
#[no_mangle]
pub extern "C" fn sbc_alloc(len: usize) -> *mut u8 { Box::leak(vec![0u8; len].into_boxed_slice()).as_mut_ptr() }

/// Boot Linux. Returns 0 on success.
#[no_mangle]
pub unsafe extern "C" fn sbc_boot(ram_mb: u32, k: *const u8, klen: usize, i: *const u8, ilen: usize, c: *const u8, clen: usize) -> i32 {
    let kernel = std::slice::from_raw_parts(k, klen);
    let initrd = if ilen > 0 { Some(std::slice::from_raw_parts(i, ilen)) } else { None };
    let cmdline = if clen > 0 { String::from_utf8_lossy(std::slice::from_raw_parts(c, clen)).into_owned() } else {
        format!("{} sbcemu.shell=fb", bcm2711::boot::DEFAULT_CMDLINE)
    };
    let mut m = Machine::new((ram_mb as usize) << 20);
    if bcm2711::boot::boot_linux(&mut m, kernel, initrd, &cmdline, true).is_err() { return -1; }
    ST.with(|s| *s.borrow_mut() = Some(State { m, fb: Vec::new(), fb_w: 0, fb_h: 0, uart: Vec::new(), wait_ms: 0.0, stop: None, wall_offset: Some(0.0) }));
    0
}

/// Boot from an SD card image of `sectors` 512-byte sectors served by the page, the way the
/// Pi firmware does (config.txt, cmdline.txt, kernel8.img, initramfs). Returns 0 on success.
#[no_mangle]
pub unsafe extern "C" fn sbc_boot_sd(ram_mb: u32, sectors: f64) -> i32 {
    let mut disk: Box<dyn Disk> = Box::new(WebDisk { sectors: sectors as u64, overlay: HashMap::new() });
    let b = match bcm2711::boot::read_sd_boot(disk.as_mut()) { Ok(b) => b, Err(_) => return -2 };
    let mut m = Machine::new((ram_mb as usize) << 20);
    m.soc.sd = bcm2711::sdhci::Sdhci::new(Some(disk));
    m.connect_network(false, false);
    if bcm2711::boot::boot_linux(&mut m, &b.kernel, b.initramfs.as_deref(), &b.cmdline, true).is_err() { return -1; }
    ST.with(|s| *s.borrow_mut() = Some(State { m, fb: Vec::new(), fb_w: 0, fb_h: 0, uart: Vec::new(), wait_ms: 0.0, stop: None, wall_offset: Some(0.0) }));
    0
}

/// USB keyboard: press (down=1) or release (down=0) a key by HID usage code.
#[no_mangle]
pub extern "C" fn sbc_key(usage: u32, down: u32) { with(|s| s.m.soc.usb.kbd.key(usage as u8, down != 0)); }

/// USB keyboard: type one byte of text (US layout).
#[no_mangle]
pub extern "C" fn sbc_type(b: u32) { with(|s| s.m.soc.usb.kbd.type_byte(b as u8)); }

/// Run up to `budget` instructions with the page's wall clock `wall_ns` (real-time pacing).
/// Returns 0 = budget used, 1 = idle (wait `sbc_wait_ms()` or until input), 2 = stopped.
#[no_mangle]
pub extern "C" fn sbc_run(budget: f64, wall_ns: f64) -> i32 {
    with(|s| {
        if s.stop.is_some() { return 2; }
        let off = *s.wall_offset.get_or_insert(s.m.now as f64 - wall_ns);
        let wall = (wall_ns + off).max(0.0) as u64;
        let end = s.m.cpu.instret + budget as u64;
        loop {
            let left = end.saturating_sub(s.m.cpu.instret);
            if left == 0 { break 0; }
            match s.m.run(left) {
                Stop::Budget => break 0,
                Stop::Idle(t) => {
                    let target = t.min(wall.max(s.m.now));
                    if target > s.m.now { s.m.idle_until(target); continue; }
                    s.wait_ms = ((t - s.m.now) as f64 / 1e6).min(20.0);
                    break 1;
                }
                Stop::Reset => {
                    let disk = s.m.soc.sd.disk.take();
                    match disk.map(|mut d| (bcm2711::boot::read_sd_boot(d.as_mut()), d)) {
                        Some((Ok(b), d)) => {
                            let ram = s.m.soc.ram.len();
                            let mut m = Machine::new(ram);
                            m.soc.sd = bcm2711::sdhci::Sdhci::new(Some(d));
                            if bcm2711::boot::boot_linux(&mut m, &b.kernel, b.initramfs.as_deref(), &b.cmdline, true).is_err() { s.stop = Some(Stop::Reset); break 2; }
                            s.m = m;
                        }
                        _ => { s.stop = Some(Stop::Reset); break 2; }
                    }
                }
                other => { s.stop = Some(other); break 2; }
            }
        }
    }).unwrap_or(2)
}

#[no_mangle]
pub extern "C" fn sbc_wait_ms() -> f64 { with(|s| s.wait_ms).unwrap_or(0.0) }

/// Advance idle time to at most `wall_ns` (after a wait) and re-evaluate interrupts.
#[no_mangle]
pub extern "C" fn sbc_wake(wall_ns: f64) { with(|s| { let t = ((wall_ns + s.wall_offset.unwrap_or(0.0)).max(0.0) as u64).max(s.m.now); s.m.idle_until(t) }); }

/// Resume a snapshot (made natively with the same SD image) instead of cold-booting.
#[no_mangle]
pub unsafe extern "C" fn sbc_restore(ram_mb: u32, sectors: f64, snap: *const u8, len: usize) -> i32 {
    let disk: Box<dyn Disk> = Box::new(WebDisk { sectors: sectors as u64, overlay: HashMap::new() });
    let mut m = Machine::new((ram_mb as usize) << 20);
    m.soc.sd = bcm2711::sdhci::Sdhci::new(Some(disk));
    m.connect_network(false, false);
    if m.restore(std::slice::from_raw_parts(snap, len)).is_err() { return -1; }
    ST.with(|s| *s.borrow_mut() = Some(State { m, fb: Vec::new(), fb_w: 0, fb_h: 0, uart: Vec::new(), wait_ms: 0.0, stop: None, wall_offset: None }));
    0
}

#[no_mangle]
pub extern "C" fn sbc_uart_in(b: u32) {
    with(|s| { if s.m.soc.uart.rx_room() { s.m.soc.uart.push_rx(b as u8); s.m.soc.dirty = true; } });
}

#[no_mangle]
pub extern "C" fn sbc_uart_room() -> i32 { with(|s| s.m.soc.uart.rx_room() as i32).unwrap_or(0) }

/// Move pending UART output to an internal buffer; returns its length (`sbc_uart_ptr` for data).
#[no_mangle]
pub extern "C" fn sbc_uart_take() -> usize { with(|s| { s.uart = std::mem::take(&mut s.m.soc.uart.out); s.uart.len() }).unwrap_or(0) }
#[no_mangle]
pub extern "C" fn sbc_uart_ptr() -> *const u8 { with(|s| s.uart.as_ptr()).unwrap_or(std::ptr::null()) }

/// Render the framebuffer as RGBA; returns a pointer (0 if no framebuffer), see sbc_fb_w/h.
#[no_mangle]
pub extern "C" fn sbc_fb_render() -> *const u8 {
    with(|s| match s.m.soc.mbox.render(&s.m.soc.ram, &mut s.fb) {
        Some((w, h)) => { s.fb_w = w; s.fb_h = h; s.fb.as_ptr() }
        None => std::ptr::null(),
    }).unwrap_or(std::ptr::null())
}
#[no_mangle]
pub extern "C" fn sbc_fb_w() -> u32 { with(|s| s.fb_w).unwrap_or(0) }
#[no_mangle]
pub extern "C" fn sbc_fb_h() -> u32 { with(|s| s.fb_h).unwrap_or(0) }

#[no_mangle]
pub extern "C" fn sbc_insns() -> f64 { with(|s| s.m.cpu.instret as f64).unwrap_or(0.0) }
#[no_mangle]
pub extern "C" fn sbc_now_ns() -> f64 { with(|s| s.m.now as f64).unwrap_or(0.0) }
/// Stop reason: 0 running, 1 unimplemented instruction (see sbc_stop_pc), 2 other.
#[no_mangle]
pub extern "C" fn sbc_stop_kind() -> i32 { with(|s| match s.stop { None => 0, Some(Stop::Unimplemented(..)) => 1, _ => 2 }).unwrap_or(2) }
#[no_mangle]
pub extern "C" fn sbc_stop_pc() -> f64 { with(|s| match s.stop { Some(Stop::Unimplemented(pc, _)) => pc as f64, _ => s.m.cpu.pc as f64 }).unwrap_or(0.0) }
#[no_mangle]
pub extern "C" fn sbc_stop_insn() -> u32 { with(|s| match s.stop { Some(Stop::Unimplemented(_, i)) => i, _ => 0 }).unwrap_or(0) }

/// GPIO line level as the pins see it (0/1).
#[no_mangle]
pub extern "C" fn sbc_gpio_level(pin: u32) -> u32 { with(|s| s.m.soc.gpio.level(pin as usize) as u32).unwrap_or(0) }

/// Drive a GPIO input from the page: 1 high, 0 low, -1 released (pull decides).
#[no_mangle]
pub extern "C" fn sbc_gpio_input(pin: u32, v: i32) {
    with(|s| { s.m.soc.gpio.set_input(pin as usize, match v { 1 => Some(true), 0 => Some(false), _ => None }); s.m.soc.dirty = true; });
}

/// Sensor inputs: kind 0 temperature (degC), 1 pressure (hPa), 2 humidity (%RH).
#[no_mangle]
pub extern "C" fn sbc_set_sensor(kind: u32, v: f64) {
    with(|s| s.m.set_sensor(match kind { 0 => "temp", 1 => "pressure", _ => "humidity" }, v));
}

/// MCP3008 channel input voltage.
#[no_mangle]
pub extern "C" fn sbc_set_adc(ch: u32, volts: f64) { with(|s| { if ch < 8 { s.m.soc.spi.adc.volts[ch as usize] = volts; } }); }
