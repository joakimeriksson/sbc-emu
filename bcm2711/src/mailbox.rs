//! VideoCore mailbox (ARM_0 mailbox 0/1 at 0xfe00_b880) with the firmware property channel
//! (channel 8): the subset of tags the Linux firmware/clock/framebuffer drivers ask for, answered
//! the way the Pi 4 firmware answers them. The framebuffer lives in the GPU memory carve-out at
//! the top of RAM (not given to Linux), exactly where firmware puts it after `gpu_mem`.
use std::collections::VecDeque;

pub const BOARD_REVISION: u32 = 0x00a0_3111; // Pi 4B 1 GiB, rev 1.1
pub const FW_REVISION: u32 = 0x6900_0000;
pub const SERIAL: u64 = 0x5bc0_e000_0000_0001;

#[derive(Clone, Debug)]
pub struct Framebuffer {
    pub base: u64,
    pub max_size: u64,
    pub allocated: bool,
    pub width: u32,
    pub height: u32,
    pub vwidth: u32,
    pub vheight: u32,
    pub depth: u32,
    /// 0 = BGR, 1 = RGB (as the firmware's "pixel order")
    pub rgb: bool,
    pub xoff: u32,
    pub yoff: u32,
    pub palette: [u32; 256],
    pub blank: bool,
}

impl Framebuffer {
    pub fn pitch(&self) -> u32 { (self.vwidth * self.depth.div_ceil(8) + 15) & !15 }
    pub fn size(&self) -> u32 { self.pitch() * self.vheight }
}

pub struct Mailbox {
    rd: VecDeque<u32>,
    pub cfg: u32,
    pub fb: Framebuffer,
    /// tags we answered with "unknown" (diagnostics)
    pub unknown: Vec<u32>,
    /// SoC temperature reported to the firmware's GET_TEMPERATURE (millidegrees C)
    pub temp_mc: u32,
    /// the firmware clock that drives the I²S bit clock (Hz; the DISP clock, id 16)
    pub audio_clk: u32,
    /// firmware clock rates as set (Hz, by clock id)
    pub rates: [u32; 32],
    /// the board revision code GET_BOARD_REVISION reports (the Pi 3 machine sets its own)
    pub revision: u32,
    /// the firmware clocks GET_CLOCKS lists (ids), the ones `clk-raspberrypi` registers
    pub clocks: &'static [u32],
    /// the firmware's GPIO expander (`raspberrypi,firmware-gpio`, lines 128..135): output
    /// levels as set, and the levels of its input lines (bit per line)
    pub exp_gpio_out: u8,
    pub exp_gpio_in: u8,
    exp_gpio_dir: u8,
}

/// The firmware clocks the Pi 4's firmware lists: ARM (cpufreq: one operating point, 1.5 GHz),
/// core (the HVS), V3D, M2MC (HDMI state machine), pixel-BVB (HDMI), and the audio clock.
pub const PI4_CLOCKS: &[u32] = &[3, 4, 5, 13, 14, AUDIO_CLK];

impl Mailbox {
    pub fn new(fb_base: u64, fb_max: u64, width: u32, height: u32) -> Self {
        Mailbox {
            temp_mc: 45_000,
            audio_clk: 0,
            rates: std::array::from_fn(|k| clock_rate(k as u32)),
            revision: BOARD_REVISION,
            clocks: PI4_CLOCKS,
            exp_gpio_out: 0, exp_gpio_in: 0, exp_gpio_dir: 0,
            rd: VecDeque::new(),
            cfg: 0,
            fb: Framebuffer { base: fb_base, max_size: fb_max, allocated: false, width, height, vwidth: width, vheight: height,
                              depth: 32, rgb: false, xoff: 0, yoff: 0, palette: [0; 256], blank: false },
            unknown: Vec::new(),
        }
    }

    pub fn irq(&self) -> bool { self.cfg & 1 != 0 && !self.rd.is_empty() }

    pub fn read(&mut self, off: u64) -> u32 {
        match off {
            0x00 => self.rd.pop_front().unwrap_or(0),
            0x10 => self.rd.front().copied().unwrap_or(0),
            0x18 => if self.rd.is_empty() { 1 << 30 } else { 0 },
            0x1c => self.cfg,
            0x38 => 0, // mailbox 1 never full
            _ => 0,
        }
    }

    /// A write to a mailbox register; property requests are processed immediately in `ram`.
    pub fn write(&mut self, off: u64, v: u32, ram: &mut [u8]) {
        match off {
            0x1c => self.cfg = v,
            0x20 => {
                let ch = v & 0xf;
                let addr = crate::machine::dma_phys(v & !0xf, ram.len() as u64) as usize;
                if ch == 8 { self.property(ram, addr); }
                self.rd.push_back(v);
            }
            _ => {}
        }
    }

    /// Allocate the framebuffer as the firmware does before Linux starts.
    pub fn firmware_alloc(&mut self) { self.fb.allocated = true; }

    fn property(&mut self, ram: &mut [u8], addr: usize) {
        let rd = |ram: &[u8], a: usize| ram.get(a..a + 4).map(|b| u32::from_le_bytes(b.try_into().unwrap())).unwrap_or(0);
        let wr = |ram: &mut [u8], a: usize, v: u32| { if let Some(b) = ram.get_mut(a..a + 4) { b.copy_from_slice(&v.to_le_bytes()); } };
        let size = rd(ram, addr) as usize;
        if size < 12 || addr + size > ram.len() { return; }
        let end = addr + size;
        let mut p = addr + 8;
        while p + 12 <= end {
            let tag = rd(ram, p);
            if tag == 0 { break; }
            let buflen = rd(ram, p + 4) as usize;
            let vals = p + 12;
            let mut v: Vec<u32> = (0..buflen / 4).map(|k| rd(ram, vals + 4 * k)).collect();
            let resp = self.tag(tag, &mut v);
            match resp {
                Some(n) => {
                    for (k, x) in v.iter().enumerate().take(buflen / 4) { wr(ram, vals + 4 * k, *x); }
                    wr(ram, p + 8, 0x8000_0000 | n as u32);
                }
                None => {
                    if !self.unknown.contains(&tag) { self.unknown.push(tag); }
                    wr(ram, p + 8, 0x8000_0000);
                }
            }
            p = vals + ((buflen + 3) & !3);
        }
        wr(ram, addr + 4, 0x8000_0000);
    }

    /// Answer one tag in place; returns the response length in bytes, or None if unknown.
    fn tag(&mut self, tag: u32, v: &mut Vec<u32>) -> Option<usize> {
        let set = |v: &mut Vec<u32>, vals: &[u32]| { if v.len() < vals.len() { v.resize(vals.len(), 0); } v[..vals.len()].copy_from_slice(vals); vals.len() * 4 };
        let arg = |v: &Vec<u32>, k: usize| v.get(k).copied().unwrap_or(0);
        let temp_mc = self.temp_mc;
        let fb = &mut self.fb;
        Some(match tag {
            0x0000_0001 => set(v, &[FW_REVISION]),
            0x0000_0002 => set(v, &[0]), // firmware variant: none
            0x0000_0003 => set(v, &[0x5bc0_e000, 0, 0, 0, 0]), // firmware git hash (20 bytes)
            // clocks for `raspberrypi,firmware-clocks`: (parent, id) pairs
            0x0001_0007 => { let l: Vec<u32> = self.clocks.iter().flat_map(|&id| [0, id]).collect(); set(v, &l) }
            0x0001_0001 => set(v, &[0]),
            0x0001_0002 => set(v, &[self.revision]),
            0x0001_0003 => set(v, &[0x5bc0_e0b8, 0x0000_2700]), // MAC b8:27:eb..? (stable placeholder)
            0x0001_0004 => set(v, &[SERIAL as u32, (SERIAL >> 32) as u32]),
            0x0001_0005 => set(v, &[0, fb.base as u32]),
            0x0001_0006 => set(v, &[fb.base as u32, fb.max_size as u32]),
            0x0002_0001 | 0x0002_0002 => { let id = arg(v, 0); set(v, &[id, 1]) }
            0x0002_8001 => { let id = arg(v, 0); let st = arg(v, 1) & 1; set(v, &[id, st]) }
            0x0003_0001 => { let id = arg(v, 0); set(v, &[id, 1]) }
            // the audio clock: any rate up to 100 MHz, as set
            0x0003_0002 | 0x0003_0047 if arg(v, 0) == AUDIO_CLK => set(v, &[AUDIO_CLK, self.audio_clk]),
            0x0003_0004 if arg(v, 0) == AUDIO_CLK => set(v, &[AUDIO_CLK, 100_000_000]),
            0x0003_0007 if arg(v, 0) == AUDIO_CLK => set(v, &[AUDIO_CLK, 0]),
            0x0003_8002 if arg(v, 0) == AUDIO_CLK => { self.audio_clk = arg(v, 1); set(v, &[AUDIO_CLK, self.audio_clk]) }
            0x0003_0002 | 0x0003_0047 => { let id = arg(v, 0); set(v, &[id, self.rates.get(id as usize).copied().unwrap_or(0)]) }
            // the ARM clock is fixed at the board's rate (one operating point)
            0x0003_0004 | 0x0003_0007 if arg(v, 0) == 3 => set(v, &[3, self.rates[3]]),
            0x0003_0004 => { let id = arg(v, 0); set(v, &[id, clock_range(id).1]) }
            0x0003_0007 => { let id = arg(v, 0); set(v, &[id, clock_range(id).0]) }
            0x0003_8001 => { let id = arg(v, 0); let s = arg(v, 1); set(v, &[id, s]) }
            0x0003_8002 => {
                let (id, r) = (arg(v, 0), arg(v, 1));
                let (lo, hi) = if id == 3 { (self.rates[3], self.rates[3]) } else { clock_range(id) };
                let r = r.clamp(lo, hi.max(lo));
                if let Some(x) = self.rates.get_mut(id as usize) { *x = r; }
                set(v, &[id, r])
            }
            // the GPIO expander (lines 128..135): state and config; a status of 0 in the first
            // word is success, as the firmware answers
            0x0003_0041 => { let g = arg(v, 0).wrapping_sub(128) & 7; let out = self.exp_gpio_dir >> g & 1 != 0;
                             let l = if out { self.exp_gpio_out } else { self.exp_gpio_in } >> g & 1; set(v, &[0, l as u32]) }
            0x0003_8041 => { let g = arg(v, 0).wrapping_sub(128) & 7; let b = 1u8 << g;
                             if arg(v, 1) != 0 { self.exp_gpio_out |= b } else { self.exp_gpio_out &= !b } set(v, &[0, arg(v, 1)]) }
            0x0003_0043 => { let g = arg(v, 0).wrapping_sub(128) & 7; set(v, &[0, (self.exp_gpio_dir >> g & 1) as u32, 0, 0, 0]) }
            0x0003_8043 => { let g = arg(v, 0).wrapping_sub(128) & 7; let b = 1u8 << g;
                             if arg(v, 1) != 0 { self.exp_gpio_dir |= b; if arg(v, 5) != 0 { self.exp_gpio_out |= b } else { self.exp_gpio_out &= !b } } else { self.exp_gpio_dir &= !b }
                             set(v, &[0]) }
            0x0003_0066 => set(v, &[]), // NOTIFY_DISPLAY_DONE: the kernel's display driver takes over
            0x0003_0006 => { let id = arg(v, 0); set(v, &[id, temp_mc]) }
            0x0003_000a => { let id = arg(v, 0); set(v, &[id, 85_000]) }
            0x0003_0003 => { let id = arg(v, 0); set(v, &[id, 1_200_000]) } // voltage (uV)
            0x0003_0046 => set(v, &[0]), // throttled: none
            0x0003_0030 => { let id = arg(v, 0); set(v, &[id, 1]) } // domain state
            0x0003_8030 => { let id = arg(v, 0); let s = arg(v, 1); set(v, &[id, s]) }
            0x0003_0058 | 0x0003_8058 => set(v, &[0, 0]), // POE / misc: nothing attached
            // ---- framebuffer
            0x0004_0001 => {
                if fb.size() as u64 > fb.max_size { set(v, &[0, 0]) } else {
                    fb.allocated = true;
                    set(v, &[0xc000_0000 | fb.base as u32, fb.size()])
                }
            }
            0x0004_8001 => { fb.allocated = false; 0 }
            0x0004_0002 => { fb.blank = arg(v, 0) & 1 != 0; set(v, &[arg(v, 0) & 1]) }
            0x0004_0003 => set(v, &[fb.width, fb.height]),
            0x0004_4003 => set(v, &[arg(v, 0), arg(v, 1)]),
            0x0004_8003 => { fb.width = arg(v, 0).clamp(16, 1920); fb.height = arg(v, 1).clamp(16, 1080); set(v, &[fb.width, fb.height]) }
            0x0004_0004 => set(v, &[fb.vwidth, fb.vheight]),
            0x0004_4004 => set(v, &[arg(v, 0), arg(v, 1)]),
            0x0004_8004 => { fb.vwidth = arg(v, 0).clamp(16, 1920); fb.vheight = arg(v, 1).clamp(16, 2160); set(v, &[fb.vwidth, fb.vheight]) }
            0x0004_0005 => set(v, &[fb.depth]),
            0x0004_4005 => set(v, &[arg(v, 0)]),
            0x0004_8005 => { let d = arg(v, 0); if matches!(d, 8 | 16 | 24 | 32) { fb.depth = d; } set(v, &[fb.depth]) }
            0x0004_0006 => set(v, &[fb.rgb as u32]),
            0x0004_4006 => set(v, &[arg(v, 0)]),
            0x0004_8006 => { fb.rgb = arg(v, 0) == 1; set(v, &[fb.rgb as u32]) }
            0x0004_0007 => set(v, &[2]), // alpha ignored
            0x0004_4007 | 0x0004_8007 => set(v, &[arg(v, 0)]),
            0x0004_0008 => set(v, &[fb.pitch()]),
            0x0004_0009 => set(v, &[fb.xoff, fb.yoff]),
            0x0004_4009 => set(v, &[arg(v, 0), arg(v, 1)]),
            0x0004_8009 => { fb.xoff = arg(v, 0); fb.yoff = arg(v, 1); set(v, &[fb.xoff, fb.yoff]) }
            0x0004_000a => set(v, &[0, 0, 0, 0]),
            0x0004_400a | 0x0004_800a => set(v, &[0, 0, 0, 0]),
            0x0004_000b => { let n = v.len().min(256); for k in 0..n { v[k] = fb.palette[k]; } n * 4 }
            0x0004_400b => set(v, &[0]),
            0x0004_800b => {
                let (off, len) = (arg(v, 0) as usize, arg(v, 1) as usize);
                if off + len > 256 { set(v, &[1]) } else {
                    for k in 0..len { fb.palette[off + k] = arg(v, 2 + k); }
                    set(v, &[0])
                }
            }
            0x0004_0013 => set(v, &[1]), // number of displays
            0x0004_8013 => set(v, &[arg(v, 0)]),
            0x0000_8010 | 0x0000_8011 => set(v, &[0]), // cursor info/state: accepted
            0x0004_000f => set(v, &[0]), // vsync / wait: immediate
            0x0003_0020 => set(v, &[arg(v, 0), 0]), // GET_DMA? -> none
            0x0006_0001 => set(v, &[0x7f35]), // DMA channel mask (firmware default)
            _ => return None,
        })
    }

    /// Render the visible part of the framebuffer as RGBA bytes (`out` resized to w*h*4).
    /// The screen size a display should show (the allocated framebuffer, else the default mode).
    pub fn fb_size(&self) -> (u32, u32) { (self.fb.width, self.fb.height) }

    pub fn render(&self, ram: &[u8], out: &mut Vec<u8>) -> Option<(u32, u32)> {
        let fb = &self.fb;
        if !fb.allocated { return None; }
        let (w, h) = (fb.width, fb.height);
        out.resize((w * h * 4) as usize, 0);
        if fb.blank { out.fill(0); return Some((w, h)); }
        let bpp = fb.depth.div_ceil(8) as usize;
        let pitch = fb.pitch() as usize;
        for y in 0..h as usize {
            let row = fb.base as usize + (y + fb.yoff as usize) * pitch + fb.xoff as usize * bpp;
            let Some(src) = ram.get(row..row + w as usize * bpp) else { break };
            let dst = &mut out[y * w as usize * 4..(y + 1) * w as usize * 4];
            for x in 0..w as usize {
                let (r, g, b) = match fb.depth {
                    32 | 24 => { let p = &src[x * bpp..]; (p[2], p[1], p[0]) }
                    16 => {
                        let p = u16::from_le_bytes([src[2 * x], src[2 * x + 1]]);
                        let (r5, g6, b5) = ((p >> 11) & 31, (p >> 5) & 63, p & 31);
                        (((r5 << 3) | (r5 >> 2)) as u8, ((g6 << 2) | (g6 >> 4)) as u8, ((b5 << 3) | (b5 >> 2)) as u8)
                    }
                    _ => { let c = fb.palette[src[x] as usize]; (c as u8, (c >> 8) as u8, (c >> 16) as u8) }
                };
                let (r, b) = if fb.rgb && fb.depth != 8 { (b, r) } else { (r, b) };
                dst[4 * x..4 * x + 4].copy_from_slice(&[r, g, b, 255]);
            }
        }
        Some((w, h))
    }
}

/// Clock rates the Pi 4 firmware reports (Hz) by firmware clock id.
/// RPI_FIRMWARE_DISP_CLK_ID: exported by `clk-raspberrypi`, used here as the I²S bit clock.
const AUDIO_CLK: u32 = 16;

/// What a clock can be set to (Hz): the firmware's min and max.
fn clock_range(id: u32) -> (u32, u32) {
    match id {
        3 => (1_500_000_000, 1_500_000_000), // ARM: fixed
        4 => (200_000_000, 500_000_000),     // core
        5 => (250_000_000, 550_000_000),     // V3D
        9 => (0, 600_000_000),               // pixel (the BCM2837's HDMI)
        13 => (120_000_000, 600_000_000),    // M2MC (HDMI)
        14 => (0, 300_000_000),              // pixel-BVB
        AUDIO_CLK => (0, 100_000_000),
        _ => { let r = clock_rate(id); (r, r) }
    }
}

fn clock_rate(id: u32) -> u32 {
    match id {
        1 => 250_000_000,   // EMMC
        2 => 48_000_000,    // UART
        3 => 1_500_000_000, // ARM
        4 => 500_000_000,   // CORE
        5 => 500_000_000,   // V3D
        6 => 500_000_000,   // H264
        7 => 500_000_000,   // ISP
        8 => 400_000_000,   // SDRAM
        9 => 250_000_000,   // PIXEL
        10 => 750_000_000,  // PWM
        12 => 100_000_000,  // EMMC2
        13 => 200_000_000,  // M2MC
        14 => 75_000_000,   // pixel-BVB
        _ => 0,
    }
}

impl Mailbox {
    pub fn save_state(&self, w: &mut emu_core::snap::W) {
        w.tag("mbox-v1");
        w.u64(self.rd.len() as u64);
        for &v in &self.rd { w.u32(v); }
        w.u32(self.cfg);
        let f = &self.fb;
        w.u64(f.base); w.u64(f.max_size); w.bool(f.allocated);
        for v in [f.width, f.height, f.vwidth, f.vheight, f.depth, f.xoff, f.yoff] { w.u32(v); }
        w.bool(f.rgb); w.bool(f.blank);
        for v in f.palette { w.u32(v); }
    }
    pub fn load_state(&mut self, r: &mut emu_core::snap::R) -> Result<(), emu_core::snap::SnapError> {
        r.tag("mbox-v1")?;
        self.rd.clear();
        for _ in 0..r.u64()? { self.rd.push_back(r.u32()?); }
        self.cfg = r.u32()?;
        let f = &mut self.fb;
        f.base = r.u64()?; f.max_size = r.u64()?; f.allocated = r.bool()?;
        for v in [&mut f.width, &mut f.height, &mut f.vwidth, &mut f.vheight, &mut f.depth, &mut f.xoff, &mut f.yoff] { *v = r.u32()?; }
        f.rgb = r.bool()?; f.blank = r.bool()?;
        for v in f.palette.iter_mut() { *v = r.u32()?; }
        Ok(())
    }
}
