//! The BCM2711 display pipeline for Linux's `vc4` KMS driver (`dtoverlay=vc4-kms-v3d`, what
//! Raspberry Pi OS asks for): the HVS compositor, pixel valve 2 and the HDMI0 encoder with its
//! PHY, DVP and DDC bus, and a monitor on the HDMI0 port (EDID with 1920x1080, 1280x720 and
//! 1024x768).
//!
//! The HVS is modelled at the level of what reaches the screen: it reads the display list of
//! the channel being shown (plane entries: format, position, size, pointer, pitch) and composes
//! the planes when the host wants a picture. The pixel valve produces vblank at 60 Hz of guest
//! time while it runs, and latches each HVS channel's display list then, as the hardware does
//! at the start of a frame (that is how the driver sees a page flip complete). The HDMI blocks
//! are registers; the handshakes the driver waits for complete at once.

pub const HVS: u64 = 0xfe40_0000;
pub const HVS_SIZE: u64 = 0x8000;
pub const PV2: u64 = 0xfe20_a000;
/// DVP, HDMI0 (hdmi, dvp, phy, rm, packet, metadata, csc, cec), DDC and its auto-I2C block
pub const HDMI_WIN: u64 = 0xfef0_0000;
pub const HDMI_WIN_SIZE: u64 = 0x1_0000;
/// HDMI0's "hd" block (MAI audio, VID_CTL, frame count)
pub const HD: u64 = 0xfef2_0000;
/// the transposer (writeback): registers only, never busy
pub const TXP: u64 = 0xfe00_4000;

/// DMA request line of HDMI0's audio FIFO (MAI)
pub const DREQ_HDMI0: u32 = 10;
/// the HDMI audio clock (the DVP gate on the 108 MHz oscillator)
const AUDIO_CLK_HZ: u64 = 108_000_000;

pub const SPI_HVS: usize = 32 + 97;
pub const SPI_PV2: usize = 32 + 101;

// HVS registers (byte offsets)
const DISPCTRL: usize = 0x00;
const DISPSTAT: usize = 0x04;
const DISPID: usize = 0x08;
const DISPLIST0: usize = 0x20;
const DISPLACT0: usize = 0x30;
const DISPCTRL0: usize = 0x40;
const DISPSTAT0: usize = 0x48;
const DLIST: usize = 0x4000;

// pixel valve
const PV_CONTROL: usize = 0x00;
const PV_V_CONTROL: usize = 0x04;
const PV_INTEN: usize = 0x24;
const PV_INTSTAT: usize = 0x28;
const PV_INT_VFP_START: u32 = 1 << 7;

// HDMI window offsets (from 0xfef00000)
const HDMI: usize = 0x700;
const HDMI_FIFO_CTL: usize = HDMI + 0x074;
const HDMI_RAM_PACKET_CONFIG: usize = HDMI + 0x0bc;
const HDMI_RAM_PACKET_STATUS: usize = HDMI + 0x0c4;
const HDMI_SCHEDULER_CONTROL: usize = HDMI + 0x0e0;
const HDMI_HOTPLUG: usize = HDMI + 0x1a8;
const BSC: usize = 0x4500;

const FRAME_NS: u64 = 16_666_667;
/// how far behind an audio FIFO's consumer may fall before it counts as an underrun (ns)
pub const MAX_CATCHUP_NS: u64 = 20_000_000;

pub struct Vc4 {
    hvs: Vec<u32>,
    /// frames shown on each HVS channel (6-bit counters in DISPSTAT1/2)
    frcnt: [u32; 3],
    pv: [u32; 64],
    win: Vec<u32>,
    hd: [u32; 64],
    txp: [u32; 8],
    /// the DDC bus: the EDID's address pointer
    edid_ptr: usize,
    edid: Vec<u8>,
    /// next vblank (ns of guest time)
    next_vblank: u64,
    /// HDMI audio (MAI): play position (ns), fractional frames, the channel a word belongs to
    mai_played_to: u64,
    mai_frac: u64,
    mai_ch: u32,
    mai_left: i16,
    /// HDMI audio played, 16-bit interleaved stereo, and its rate
    pub audio: Vec<i16>,
    pub audio_rate: u32,
    /// a monitor is plugged into HDMI0
    pub connected: bool,
    pub frames: u64,
    pub trace: bool,
}

impl Default for Vc4 { fn default() -> Self { Self::new() } }

impl Vc4 {
    pub fn new() -> Self { Self::with_mode(1920, 1080) }

    /// A monitor whose preferred mode is `w`x`h` (1920x1080, 1280x720, 1024x768, 800x600 or
    /// 640x480): the console, `kmscube` and `glmark2` use the preferred mode.
    pub fn with_mode(w: u32, h: u32) -> Self {
        let mut v = Self::new_inner();
        v.edid = edid_with((w, h));
        v
    }

    fn new_inner() -> Self {
        Vc4 { hvs: vec![0; (HVS_SIZE / 4) as usize], frcnt: [0; 3], pv: [0; 64], win: vec![0; (HDMI_WIN_SIZE / 4) as usize], hd: [0; 64], txp: [0; 8],
              edid_ptr: 0, edid: edid(), next_vblank: FRAME_NS, connected: true, frames: 0,
              mai_played_to: 0, mai_frac: 0, mai_ch: 0, mai_left: 0, audio: Vec::new(), audio_rate: 0,
              trace: std::env::var_os("SBC_TRACE_VC4").is_some() }
    }

    // ------------------------------------------------------------------ HVS

    pub fn hvs_read(&self, off: u64) -> u32 {
        let o = off as usize & !3;
        match o {
            DISPSTAT => 0, // no interrupt pending (underrun/EOF interrupts are not raised)
            DISPID => 0x0000_0001,
            // channel status: running or empty, the line counter at 0; DISPSTAT1/2 also carry
            // the frame counters (FRCNT0/1 in DISPSTAT1, FRCNT2 in DISPSTAT2)
            o if (DISPSTAT0..DISPSTAT0 + 0x30).contains(&o) && (o - DISPSTAT0) % 0x10 == 0 => {
                let ch = (o - DISPSTAT0) / 0x10;
                let on = self.hvs[(DISPCTRL0 + 0x10 * ch) / 4] & (1 << 31) != 0;
                let mut v = if on { 2 << 30 } else { 1 << 28 };
                match ch {
                    1 => v |= (self.frcnt[0] & 63) << 20 | (self.frcnt[1] & 63) << 14,
                    2 => v |= (self.frcnt[2] & 63) << 14,
                    _ => {}
                }
                v
            }
            o if o < HVS_SIZE as usize => self.hvs[o / 4],
            _ => 0,
        }
    }

    pub fn hvs_write(&mut self, off: u64, v: u32) {
        let o = off as usize & !3;
        if self.trace && o < DLIST { eprintln!("[hvs] {o:#x} <- {v:#x}"); }
        match o {
            DISPSTAT => {}
            o if (DISPCTRL0..DISPCTRL0 + 0x30).contains(&o) && (o - DISPCTRL0) % 0x10 == 0 => {
                // RESET clears the channel; the frame counter restarts
                if v & (1 << 30) != 0 { self.frcnt[(o - DISPCTRL0) / 0x10] = 0; }
                self.hvs[o / 4] = v & !(1 << 30);
            }
            o if o < HVS_SIZE as usize => self.hvs[o / 4] = v,
            _ => {}
        }
    }

    // ------------------------------------------------------------------ pixel valve 2

    fn pv_running(&self) -> bool { self.pv[PV_CONTROL / 4] & 1 != 0 && self.pv[PV_V_CONTROL / 4] & 1 != 0 }

    pub fn pv_read(&self, off: u64) -> u32 { self.pv[(off as usize / 4) & 63] }

    pub fn pv_write(&mut self, off: u64, v: u32) {
        let r = (off as usize / 4) & 63;
        if self.trace { eprintln!("[pv2] {:#x} <- {v:#x}", r * 4); }
        match r * 4 {
            PV_INTSTAT => self.pv[r] &= !v,
            PV_CONTROL => self.pv[r] = v & !(1 << 1), // FIFO_CLR self-clears
            _ => self.pv[r] = v,
        }
    }

    pub fn irq_pv(&self) -> bool { self.pv[PV_INTSTAT / 4] & self.pv[PV_INTEN / 4] != 0 }

    /// Guest time passes: vblanks while the pixel valve runs.
    pub fn tick(&mut self, now: u64) {
        if !self.pv_running() { self.next_vblank = now + FRAME_NS; return; }
        if now < self.next_vblank { return; }
        let n = (now - self.next_vblank) / FRAME_NS + 1;
        self.next_vblank += n * FRAME_NS;
        self.frames += n;
        // a new frame starts: each channel takes the display list it was given
        for ch in 0..3 {
            if self.hvs[(DISPCTRL0 + 0x10 * ch) / 4] & (1 << 31) != 0 {
                self.hvs[(DISPLACT0 + 4 * ch) / 4] = self.hvs[(DISPLIST0 + 4 * ch) / 4];
                self.frcnt[ch] = self.frcnt[ch].wrapping_add(n as u32);
            }
        }
        self.pv[PV_INTSTAT / 4] |= PV_INT_VFP_START;
    }

    /// The next vblank, while the driver wants vblank interrupts.
    pub fn next_event(&self) -> Option<u64> {
        (self.pv_running() && self.pv[PV_INTEN / 4] & PV_INT_VFP_START != 0).then_some(self.next_vblank)
    }

    // ------------------------------------------------------------------ HDMI0, DVP, DDC

    pub fn win_read(&mut self, off: u64) -> u32 {
        let o = off as usize & !3;
        let r = match o {
            HDMI_HOTPLUG => self.connected as u32,
            HDMI_RAM_PACKET_STATUS => self.win[HDMI_RAM_PACKET_CONFIG / 4] & 0xffff,
            HDMI_FIFO_CTL => self.win[o / 4] | if self.win[o / 4] & (1 << 6) != 0 { 1 << 14 } else { 0 },
            HDMI_SCHEDULER_CONTROL => (self.win[o / 4] & !2) | (self.win[o / 4] & 1) << 1,
            o if (BSC..BSC + 0x100).contains(&o) => self.win[o / 4],
            o if o < HDMI_WIN_SIZE as usize => self.win[o / 4],
            _ => 0,
        };
        if self.trace && !(BSC..BSC + 0x100).contains(&o) { eprintln!("[hdmi] read {o:#x} = {r:#x}"); }
        r
    }

    pub fn win_write(&mut self, off: u64, v: u32) {
        let o = off as usize & !3;
        if self.trace && !(BSC..BSC + 0x100).contains(&o) { eprintln!("[hdmi] {o:#x} <- {v:#x}"); }
        if o >= HDMI_WIN_SIZE as usize { return; }
        self.win[o / 4] = v;
        if o == BSC + 0x2c { self.bsc_go(v); }
    }

    pub fn txp_read(&self, off: u64) -> u32 { self.txp[(off as usize / 4) & 7] }
    pub fn txp_write(&mut self, off: u64, v: u32) { self.txp[(off as usize / 4) & 7] = v & !(1 << 31); } // TXP_BUSY never set

    pub fn hd_read(&self, off: u64) -> u32 {
        let r = (off as usize / 4) & 63;
        match r * 4 {
            0x10 => self.hd[r] | 1 << 10, // MAI_CTL: the FIFO drains as fast as it is filled (EMPTY)
            _ => self.hd[r],
        }
    }
    pub fn hd_write(&mut self, off: u64, v: u32) {
        let r = (off as usize / 4) & 63;
        match r * 4 {
            0x10 => {
                // MAI_CTL: RESET and FLUSH self-clear (and restart the channel order); error bits are W1C
                if v & (1 | 1 << 9) != 0 { self.mai_ch = 0; }
                self.hd[r] = v & !(1 | 1 << 9 | 1 << 1 | 1 << 2 | 1 << 15);
            }
            0x1c => self.mai_push(v),
            _ => self.hd[r] = v,
        }
    }

    // ------------------------------------------------------------------ HDMI audio

    fn mai_channels(&self) -> u32 { ((self.hd[0x10 / 4] >> 4) & 0xf).max(1) }

    /// The audio sample rate: the audio clock * (M + 1) / N (MAI_SMP).
    pub fn mai_rate(&self) -> u32 {
        let smp = self.hd[0x20 / 4];
        let (n, m) = ((smp >> 8) as u64, (smp & 0xff) as u64 + 1);
        if n == 0 { 0 } else { (AUDIO_CLK_HZ * m / n) as u32 }
    }

    /// Words the MAI FIFO takes from the DMA engine between the last call and `now` (DREQ 10).
    pub fn mai_demand(&mut self, now: u64) -> u64 {
        let on = self.hd[0x10 / 4] & (1 << 3) != 0;
        let rate = self.mai_rate() as u64;
        if !on || rate == 0 { self.mai_played_to = now; self.mai_frac = 0; return 0; }
        if now <= self.mai_played_to { return 0; }
        // a FIFO that ran dry underran: it never owes more than a short catch-up
        let from = self.mai_played_to.max(now.saturating_sub(MAX_CATCHUP_NS));
        let num = (now - from) * rate + self.mai_frac;
        self.mai_frac = num % 1_000_000_000;
        self.mai_played_to = now;
        num / 1_000_000_000 * self.mai_channels() as u64
    }

    /// One IEC 60958 subframe for the MAI FIFO: the sample is bits 27:4 (16 bits kept); the
    /// preamble in bits 3:0 tells the channels apart (Y = 0x4: the second, right channel;
    /// X = 0x2 and Z = 0x8, a block start: the first).
    pub fn mai_push(&mut self, w: u32) {
        if self.hd[0x10 / 4] & (1 << 3) == 0 { return; }
        let s = (w >> 12) as u16 as i16;
        let ch = match w & 0xf { 0x4 => 1, 0x2 | 0x8 => 0, _ => self.mai_ch };
        self.mai_ch = (ch + 1) % self.mai_channels();
        match (self.mai_channels(), ch) {
            (1, _) => { self.audio.push(s); self.audio.push(s); }
            (_, 0) => self.mai_left = s,
            (_, 1) => { self.audio.push(self.mai_left); self.audio.push(s); }
            _ => {} // channels beyond the first two
        }
        self.audio_rate = self.mai_rate();
        if self.audio.len() > 2 * 48_000 * 10 { self.audio.drain(..self.audio.len() / 4 * 2); } // whole frames
    }

    /// The DDC controller (brcmstb BSC): IIC_ENABLE written with ENABLE runs the transfer at
    /// once and sets INTRP (NOACK if no device answers). The EDID EEPROM answers at 0x50.
    fn bsc_go(&mut self, en: u32) {
        let r = |s: &Self, o: usize| s.win[(BSC + o) / 4];
        if en & 1 == 0 { return; }
        let addr = r(self, 0x00) & 0xff;
        let cnt = (r(self, 0x24) & 0x3f) as usize;
        let rd = r(self, 0x28) & 3 == 1;
        let wide = r(self, 0x50) & (1 << 6) != 0; // 32-bit data registers
        let regsz = if wide { 4 } else { 1 };
        let present = self.connected && (addr >> 1) == 0x50;
        let mut done = 1 << 1; // INTRP
        if !present {
            done |= 1 << 2; // NOACK
        } else if rd {
            for i in 0..cnt.div_ceil(regsz) {
                let mut w = 0u32;
                for b in 0..regsz {
                    if i * regsz + b < cnt {
                        let byte = self.edid.get(self.edid_ptr).copied().unwrap_or(0xff);
                        self.edid_ptr = (self.edid_ptr + 1) % self.edid.len().max(1);
                        w |= (byte as u32) << (8 * b);
                    }
                }
                self.win[(BSC + 0x30) / 4 + i] = w;
            }
        } else if cnt > 0 {
            // the first byte written is the EEPROM's address pointer
            self.edid_ptr = (r(self, 0x04) & 0xff) as usize % self.edid.len().max(1);
        }
        self.win[(BSC + 0x2c) / 4] = (en & !1) | done;
    }

    // ------------------------------------------------------------------ the picture

    /// The channel shown on HDMI0: the enabled one (its size from DISPCTRLx).
    fn shown(&self) -> Option<(usize, u32, u32)> {
        if !self.pv_running() || self.hvs[DISPCTRL / 4] & (1 << 31) == 0 { return None; }
        (0..3).find_map(|ch| {
            let c = self.hvs[(DISPCTRL0 + 0x10 * ch) / 4];
            let (w, h) = ((c >> 16) & 0x1fff, c & 0x1fff);
            (c & (1 << 31) != 0 && w > 0 && h > 0).then_some((ch, w, h))
        })
    }

    pub fn active(&self) -> bool { self.shown().is_some() }

    /// The mode on HDMI0, while the pipeline runs.
    pub fn size(&self) -> Option<(u32, u32)> { self.shown().map(|(_, w, h)| (w, h)) }

    /// Compose the planes of the channel shown on HDMI0 into RGBA; None if HDMI shows nothing.
    pub fn render(&self, ram: &[u8], out: &mut Vec<u8>) -> Option<(u32, u32)> {
        let (ch, w, h) = self.shown()?;
        let (wu, hu) = (w as usize, h as usize);
        out.clear();
        out.resize(wu * hu * 4, 0);
        // background fill (DISPBKGNDx, if FILL)
        let bk = self.hvs[(0x44 + 0x10 * ch) / 4];
        if bk & (1 << 24) != 0 {
            let c = [(bk >> 16) as u8, (bk >> 8) as u8, bk as u8, 255];
            for p in out.chunks_mut(4) { p.copy_from_slice(&c); }
        } else {
            for p in out.chunks_mut(4) { p[3] = 255; }
        }
        let mut i = (self.hvs[(DISPLACT0 + 4 * ch) / 4] as usize) & 0xfff;
        let dl = |k: usize| self.hvs.get(DLIST / 4 + k).copied().unwrap_or(1 << 31);
        for n in 0..64 {
            let ctl0 = dl(i);
            if ctl0 & (1 << 31) != 0 || ctl0 & (1 << 30) == 0 { break; } // end of list
            let size = ((ctl0 >> 24) & 0x3f) as usize;
            if size == 0 { break; }
            self.draw_plane(ram, &|k| dl(i + k), ctl0, n == 0, out, wu, hu);
            i += size;
        }
        Some((w, h))
    }

    /// One plane entry (HVS5 layout), RGB formats, nearest-neighbour scaling. The bottom plane
    /// is opaque; planes above it blend with their per-pixel alpha (the cursor).
    fn draw_plane(&self, ram: &[u8], e: &dyn Fn(usize) -> u32, ctl0: u32, bottom: bool, out: &mut [u8], w: usize, h: usize) {
        let fmt = ctl0 & 0x1f; // SCALER5_CTL0_PIXEL_FORMAT: 5 bits on the HVS5
        let order = (ctl0 >> 13) & 3;
        let unity = ctl0 & (1 << 15) != 0;
        let tiling = (ctl0 >> 20) & 3;
        let skip = |why: &str| {
            // each kind of plane the model cannot show, once
            thread_local! { static SEEN: std::cell::RefCell<Vec<u32>> = Default::default(); }
            let key = ctl0 & 0x0030_e00f;
            if SEEN.with(|s| { let mut s = s.borrow_mut(); let new = !s.contains(&key); if new { s.push(key); } new }) {
                eprintln!("[vc4] HVS plane not shown ({why}): ctl0={ctl0:#010x} format {} order {} tiling {}", ctl0 & 0xf, (ctl0 >> 13) & 3, (ctl0 >> 20) & 3);
            }
        };
        if tiling != 0 && tiling != 3 { return skip("64/128/256-byte tiling"); }
        let pos0 = e(1);
        let (x0, y0) = ((pos0 & 0xfff) as usize, ((pos0 >> 16) & 0xfff) as usize);
        let mut k = 3; // control word 2 at 2
        let (mut sw, mut sh) = (0usize, 0usize);
        if !unity { let p1 = e(k); sw = (p1 & 0x1fff) as usize; sh = ((p1 >> 16) & 0x1fff) as usize; k += 1; }
        let pos2 = e(k);
        let (pw, ph) = ((pos2 & 0x1fff) as usize, ((pos2 >> 16) & 0x1fff) as usize);
        let ptr = e(k + 2) as usize;
        let pitch0 = e(k + 4);
        let pitch = (pitch0 & 0xffff) as usize;
        let (dw, dh) = if unity { (pw, ph) } else { (sw.max(1), sh.max(1)) };
        let bpp = match fmt { 4 | 1 | 2 | 3 => 2, 5 => 3, 7 | 16 => 4, _ => return skip("pixel format") };
        // T-tiled (DRM_FORMAT_MOD_BROADCOM_VC4_T_TILED, what GNOME's compositor scans out): the
        // pointer was advanced to the source origin; undo that to the start of its even row of
        // 4 KiB tiles and address source pixels from there (vc4_plane.c, Mesa's vc4_tiling.c)
        let t = (tiling == 3).then(|| {
            if bpp == 3 { return None; }
            let tw_px = if bpp == 2 { 64 } else { 32 };
            let (x_off, y_off) = ((pitch0 >> 26) as usize, ((pitch0 >> 8) & 63) as usize);
            let (tiles_l, tiles_r) = (((pitch0 >> 16) & 0x7f) as usize, (pitch0 & 0x7f) as usize);
            let tiles_w = tiles_l + tiles_r;
            let odd = y_off & 32 != 0;
            let (tile_y, subtile_y, utile_y) = ((y_off >> 4) & 1, (y_off >> 2) & 3, y_off & 3);
            let mut adj = (subtile_y << 8) + (utile_y << 4);
            adj = if odd { adj + ((tiles_w - tiles_l) << 12) - ((1 + (tile_y ^ 1)) << 10) } else { adj + (tiles_l << 12) + (tile_y << 10) };
            if odd { adj += tiles_w << 12; }
            let base = ptr.checked_sub(adj)?;
            Some((base, tiles_l * tw_px + x_off, y_off, tiles_w))
        }).flatten();
        if tiling == 3 && t.is_none() { return skip("T-tiled layout"); }
        let addr = |sx: usize, sy: usize| -> usize {
            match t {
                Some((base, ox, oy, tiles_w)) => base + t_format_offset(ox + sx, oy + sy, bpp, tiles_w),
                None => ptr + sy * pitch + sx * bpp,
            }
        };
        for dy in 0..dh {
            let y = y0 + dy;
            if y >= h { break; }
            let sy = if unity { dy } else { dy * ph / dh };
            for dx in 0..dw {
                let x = x0 + dx;
                if x >= w { break; }
                let sx = if unity { dx } else { dx * pw / dw };
                let a0 = addr(sx, sy);
                let Some(p) = ram.get(a0..a0 + bpp) else { continue };
                let (r, g, b, a) = match (fmt, order) {
                    (7, 2) => (p[2], p[1], p[0], p[3]), // ARGB/XRGB8888: B, G, R, A in memory
                    (7, 3) => (p[0], p[1], p[2], p[3]), // ABGR/XBGR8888
                    (7, 0) => (p[3], p[2], p[1], p[0]),
                    (7, _) => (p[1], p[2], p[3], p[0]),
                    (16, _) => {
                        // XRGB/ARGB2101010 (order ARGB) or XBGR/ABGR2101010: 10 bits a channel,
                        // GNOME's choice when the display takes 30-bit colour
                        let v = u32::from_le_bytes([p[0], p[1], p[2], p[3]]);
                        let (hi, mid, lo) = (((v >> 22) & 255) as u8, ((v >> 12) & 255) as u8, ((v >> 2) & 255) as u8);
                        let a = ((v >> 30) * 85) as u8;
                        if order == 3 { (lo, mid, hi, a) } else { (hi, mid, lo, a) }
                    }
                    (5, 3) => (p[0], p[1], p[2], 255), // BGR888
                    (5, _) => (p[2], p[1], p[0], 255), // RGB888
                    (4, _) => {
                        let v = u16::from_le_bytes([p[0], p[1]]);
                        let (r5, g6, b5) = ((v >> 11) & 31, (v >> 5) & 63, v & 31);
                        let (r, g, b) = (((r5 << 3) | (r5 >> 2)) as u8, ((g6 << 2) | (g6 >> 4)) as u8, ((b5 << 3) | (b5 >> 2)) as u8);
                        if order == 3 { (b, g, r, 255) } else { (r, g, b, 255) }
                    }
                    _ => (0, 0, 0, 255),
                };
                let o = (y * w + x) * 4;
                if a == 255 || bottom || (fmt != 7 && fmt != 16) {
                    out[o..o + 4].copy_from_slice(&[r, g, b, 255]);
                } else {
                    let a = a as u32;
                    for (c, s) in out[o..o + 3].iter_mut().zip([r, g, b]) { *c = ((s as u32 * a + *c as u32 * (255 - a)) / 255) as u8; }
                }
            }
        }
    }

    pub fn save_state(&self, w: &mut emu_core::snap::W) {
        // v2 adds the monitor's EDID, so a restore keeps the mode the guest chose from it
        w.tag("vc4-v2");
        for v in &self.hvs { w.u32(*v); }
        for v in self.frcnt { w.u32(v); }
        for v in self.pv { w.u32(v); }
        for v in &self.win { w.u32(*v); }
        for v in self.hd { w.u32(v); }
        w.u64(self.edid_ptr as u64); w.u64(self.next_vblank); w.bool(self.connected);
        w.u64(self.mai_played_to); w.u64(self.mai_frac); w.u32(self.mai_ch);
        w.bytes(&self.edid);
    }
    pub fn load_state(&mut self, r: &mut emu_core::snap::R) -> Result<(), emu_core::snap::SnapError> {
        let v2 = match r.bytes()? { b"vc4-v2" => true, b"vc4-v1" => false, t => return Err(emu_core::snap::SnapError(format!("snapshot layout mismatch: expected vc4 state, found {}", String::from_utf8_lossy(t)))) };
        for v in self.hvs.iter_mut() { *v = r.u32()?; }
        for v in self.frcnt.iter_mut() { *v = r.u32()?; }
        for v in self.pv.iter_mut() { *v = r.u32()?; }
        for v in self.win.iter_mut() { *v = r.u32()?; }
        for v in self.hd.iter_mut() { *v = r.u32()?; }
        self.edid_ptr = r.u64()? as usize; self.next_vblank = r.u64()?; self.connected = r.bool()?;
        self.mai_played_to = r.u64()?; self.mai_frac = r.u64()?; self.mai_ch = r.u32()?;
        if v2 { self.edid = r.bytes()?.to_vec(); }
        Ok(())
    }
}

/// Byte offset of pixel (x, y) in a VC4 T-format image `tiles_w` 4 KiB tiles wide (Mesa's
/// vc4_tiling.c): 64-byte micro-tiles (4x4 pixels at 32 bpp, 8x4 at 16), 1 KiB sub-tiles of 4x4
/// micro-tiles, 4 KiB tiles of 2x2 sub-tiles whose order alternates with the tile row, and
/// rows of tiles running left to right on even rows, right to left on odd ones.
fn t_format_offset(x: usize, y: usize, cpp: usize, tiles_w: usize) -> usize {
    let uw = 16 / cpp;
    let (ux, uy) = (x / uw, y / 4);
    let (tile_x, tile_y) = (ux >> 3, uy >> 3);
    let odd = tile_y & 1 == 1;
    let tile_x = if odd { tiles_w.saturating_sub(tile_x + 1) } else { tile_x };
    let stile = (((uy >> 2) & 1) << 1) | ((ux >> 2) & 1);
    let stile_off = if odd { [2, 1, 3, 0][stile] } else { [0, 3, 1, 2][stile] };
    4096 * (tile_y * tiles_w + tile_x) + 1024 * stile_off + 64 * ((uy & 3) * 4 + (ux & 3)) + (y & 3) * uw * cpp + (x % uw) * cpp
}

// ---------------------------------------------------------------------- the monitor

/// A detailed timing descriptor: pixel clock (kHz), h active/front porch/sync/back porch,
/// v likewise, sync polarities.
fn dtd(clk_khz: u32, h: [u32; 4], v: [u32; 4], pos: bool) -> [u8; 18] {
    let (ha, hb) = (h[0], h[1] + h[2] + h[3]);
    let (va, vb) = (v[0], v[1] + v[2] + v[3]);
    let c = clk_khz / 10;
    let mut d = [0u8; 18];
    d[0] = c as u8; d[1] = (c >> 8) as u8;
    d[2] = ha as u8; d[3] = hb as u8; d[4] = (((ha >> 8) & 15) << 4 | ((hb >> 8) & 15)) as u8;
    d[5] = va as u8; d[6] = vb as u8; d[7] = (((va >> 8) & 15) << 4 | ((vb >> 8) & 15)) as u8;
    d[8] = h[1] as u8; d[9] = h[2] as u8;
    d[10] = ((v[1] & 15) << 4 | (v[2] & 15)) as u8;
    d[11] = (((h[1] >> 8) & 3) << 6 | ((h[2] >> 8) & 3) << 4 | ((v[1] >> 4) & 3) << 2 | ((v[2] >> 4) & 3)) as u8;
    d[12] = 0x2c; d[13] = 0x19; d[14] = 0x10; // 556 x 281 mm
    d[17] = if pos { 0x1e } else { 0x18 }; // digital separate sync, polarities
    d
}

fn descriptor(tag: u8, text: &[u8]) -> [u8; 18] {
    let mut d = [0u8; 18];
    d[3] = tag;
    let mut t = text.to_vec();
    t.push(b'\n');
    t.resize(13, b' ');
    d[5..18].copy_from_slice(&t[..13]);
    d
}

/// A 1080p HDMI monitor: base block (1920x1080 preferred, 1024x768 and others listed) and a
/// CEA-861 extension (HDMI, basic audio, LPCM 2 channels).
fn edid() -> Vec<u8> { edid_with((1920, 1080)) }

/// Detailed timing of a mode the monitor offers (CEA/VESA timings at 60 Hz).
fn mode_dtd(w: u32, h: u32) -> [u8; 18] {
    match (w, h) {
        (1280, 720) => dtd(74_250, [1280, 110, 40, 220], [720, 5, 5, 20], true),
        (1024, 768) => dtd(65_000, [1024, 24, 136, 160], [768, 3, 6, 29], false),
        (800, 600) => dtd(40_000, [800, 40, 128, 88], [600, 1, 4, 23], true),
        (640, 480) => dtd(25_175, [640, 16, 96, 48], [480, 10, 2, 33], false),
        _ => dtd(148_500, [1920, 88, 44, 148], [1080, 4, 5, 36], true),
    }
}

fn edid_with(pref: (u32, u32)) -> Vec<u8> {
    let mut b = vec![0x00, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0x00];
    // manufacturer "SBC" (5-bit letters), product 0x0001, serial, week 46 of 2026, EDID 1.4
    let m = |c: u8| (c - b'A' + 1) as u16;
    let id = m(b'S') << 10 | m(b'B') << 5 | m(b'C');
    b.extend_from_slice(&id.to_be_bytes());
    b.extend_from_slice(&[0x01, 0x00, 0x01, 0x00, 0x00, 0x00, 46, (2026 - 1990) as u8, 1, 4]);
    b.extend_from_slice(&[0xa5, 56, 32, 120, 0x3a]); // digital 8 bpc HDMI-a, 56x32 cm, gamma 2.2, RGB, preferred timing
    b.extend_from_slice(&[0xee, 0x91, 0xa3, 0x54, 0x4c, 0x99, 0x26, 0x0f, 0x50, 0x54]); // chromaticity (sRGB-ish)
    let full = pref == (1920, 1080);
    // established: 640x480@60, 800x600@60 and (unless the monitor is smaller) 1024x768@60
    b.extend_from_slice(&[0x21, if pref.0 >= 1024 { 0x08 } else { 0x00 }, 0x00]);
    if full {
        b.extend_from_slice(&[0x81, 0xc0, 0xd1, 0xc0, 0x01, 0x01, 0x01, 0x01, 0x01, 0x01, 0x01, 0x01, 0x01, 0x01, 0x01, 0x01]); // standard: 1280x720, 1920x1080
    } else {
        b.extend_from_slice(&[0x01; 16]); // a smaller monitor: nothing above its preferred mode
    }
    // the first detailed timing is the preferred mode
    b.extend_from_slice(&mode_dtd(pref.0, pref.1));
    b.extend_from_slice(&if full { mode_dtd(1024, 768) } else { mode_dtd(640, 480) });
    b.extend_from_slice(&[0, 0, 0, 0xfd, 0, 50, 75, 30, 80, 15, 0, 0x0a, 0x20, 0x20, 0x20, 0x20, 0x20, 0x20]); // range limits
    b.extend_from_slice(&descriptor(0xfc, b"sbc-emu HDMI"));
    b.push(1); // one extension
    let sum: u8 = b.iter().fold(0u8, |a, &x| a.wrapping_add(x));
    b.push(0u8.wrapping_sub(sum));
    // CEA-861 extension: audio, video (1080p native, 720p, 480p, 640x480), speakers, HDMI VSDB
    let blocks: Vec<u8> = [
        &[0x23, 0x09, 0x07, 0x07][..],             // audio: LPCM, 2 channels, 32/44.1/48 kHz, 16/20/24 bit
        if full { &[0x44, 0x90, 0x04, 0x02, 0x01][..] } else { &[0x41, 0x01][..] }, // video: VIC 16 (native), 4, 2, 1 / 640x480
        &[0x83, 0x01, 0x00, 0x00][..],             // speakers: front left/right
        &[0x65, 0x03, 0x0c, 0x00, 0x10, 0x00][..], // HDMI vendor block, physical address 1.0.0.0
    ].concat();
    let mut c = vec![0x02, 0x03, (4 + blocks.len()) as u8, 0x40 | 0x01]; // basic audio, 1 native DTD
    c.extend_from_slice(&blocks);
    c.extend_from_slice(&if full { mode_dtd(1280, 720) } else { mode_dtd(pref.0, pref.1) });
    c.resize(127, 0);
    let sum: u8 = c.iter().fold(0u8, |a, &x| a.wrapping_add(x));
    c.push(0u8.wrapping_sub(sum));
    b.extend_from_slice(&c);
    b
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn t_format_is_a_permutation() {
        for cpp in [2usize, 4] {
            let (tiles_w, tiles_h) = (3, 3);
            let (w, h) = (tiles_w * if cpp == 2 { 64 } else { 32 }, tiles_h * 32);
            let mut seen = vec![false; w * h];
            for y in 0..h { for x in 0..w {
                let o = t_format_offset(x, y, cpp, tiles_w);
                assert_eq!(o % cpp, 0);
                assert!(!seen[o / cpp], "cpp {cpp}: ({x},{y}) collides");
                seen[o / cpp] = true;
            } }
        }
        // Mesa's examples: the first 1 KiB sub-tile of an odd tile row is the top right
        assert_eq!(t_format_offset(0, 32, 4, 2), 4096 * 3 + 1024 * 2);
    }

    #[test]
    fn t_tiled_plane_origin() {
        // vc4_plane.c's pointer and pitch for a source origin, then the HVS model's way back
        for (sx, sy) in [(0, 0), (40, 0), (5, 33), (70, 47), (33, 70)] {
            let (tiles_w, cpp) = (4usize, 4usize);
            let (tiles_l, x_off, tiles_t, y_off) = (sx >> 5, sx & 31, sy >> 5, sy & 63);
            let (tile_y, subtile_y, utile_y) = ((sy >> 4) & 1, (sy >> 2) & 3, sy & 3);
            let mut off = tiles_t * (tiles_w << 12) + (subtile_y << 8) + (utile_y << 4);
            if tiles_t & 1 == 1 { off += (tiles_w - tiles_l) << 12; off -= (1 + (tile_y ^ 1)) << 10; } else { off += (tiles_l << 12) + (tile_y << 10); }
            let base = 0x10_0000usize;
            let ptr = base + off;
            // the model: undo to the even tile row, then address (x_off + 0, y_off + 0)
            let odd = y_off & 32 != 0;
            let mut adj = (subtile_y << 8) + (utile_y << 4);
            adj = if odd { adj + ((tiles_w - tiles_l) << 12) - ((1 + (tile_y ^ 1)) << 10) } else { adj + (tiles_l << 12) + (tile_y << 10) };
            if odd { adj += tiles_w << 12; }
            let even_row = ptr - adj;
            assert_eq!(even_row, base + (tiles_t & !1) * (tiles_w << 12), "origin ({sx},{sy})");
            assert_eq!(even_row + t_format_offset(tiles_l * 32 + x_off, y_off, cpp, tiles_w),
                       base + t_format_offset(sx, sy, cpp, tiles_w), "origin ({sx},{sy})");
        }
    }

    #[test]
    fn edid_checksums() {
        for mode in [(1920, 1080), (800, 600), (640, 480)] {
            let e = edid_with(mode);
            assert_eq!(e.len(), 256);
            for blk in e.chunks(128) { assert_eq!(blk.iter().fold(0u8, |a, &x| a.wrapping_add(x)), 0); }
        }
        let e = edid();
        assert_eq!(&e[0..8], &[0, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0]);
    }
}
