//! The display pipeline of the BCM2711 and of the older BCM2835 family (the BCM2837 of the Pi 3)
//! for Linux's `vc4` KMS driver (`dtoverlay=vc4-kms-v3d`, what Raspberry Pi OS asks for): the
//! HVS compositor, the pixel valves and the HDMI encoder, and a monitor on the HDMI port (EDID
//! with 1920x1080, 1280x720 and 1024x768).
//!
//! `Gen::Bcm2711`: the HVS5, pixel valve 2 and HDMI0 with its PHY, DVP and DDC bus (the BSC in
//! the HDMI window). `Gen::Bcm2835`: the HVS of the BCM2835/6/7 (display lists at 0x2000, 12-bit
//! sizes, 4-bit pixel formats, gamma), pixel valves 0..2 (PV2 feeds HDMI or the composite
//! encoder from HVS channel 1), the BCM2835 HDMI block with its "hd" block, and the VEC (the
//! composite video encoder, `brcm,bcm2835-vec`); HDMI's DDC is the SoC's I²C controller BSC2,
//! so the EDID EEPROM is a separate device there (`EdidEeprom`). Its CEC controller is modelled
//! with a TV on the bus that answers polls and the usual queries (`Cec`).
//!
//! The VEC is registers plus what they select: the TV standard (PAL, NTSC and their variants,
//! as `vc4_vec.c` writes them) and whether the encoder runs. Pixel valve 2 drives either HDMI
//! or the VEC (its clock select); the host's picture is the output it drives, and with the VEC
//! its fields come at the composite rate (the VEC clock / 8 = 13.5 MHz, 50 or 59.94 fields a
//! second). The picture is the decoded frame (720x576 or 720x480, both fields), not a signal.
//!
//! The HVS is modelled at the level of what reaches the screen: it reads the display list of
//! the channel being shown (plane entries: format, position, size, pointers, pitches, colour
//! conversion) and composes the planes when the host wants a picture: RGB formats, YUV 4:2:0 and
//! 4:2:2 in two or three planes, T-tiled RGB, nearest-neighbour scaling, alpha. The pixel valves
//! produce vblank while they run (the BCM2711's at 60 Hz, the BCM2835's at the rate of the mode
//! they were given and the pixel clock), and the HVS channel a valve reads latches its display
//! list then, as the hardware does at the start of a frame (that is how the driver sees a page
//! flip complete). The HDMI blocks are registers; the handshakes the driver waits for complete
//! at once.

use std::collections::VecDeque;

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
/// DMA request line of the BCM2835's HDMI audio FIFO
pub const DREQ_HDMI_BCM2835: u32 = 17;
/// the BCM2711's HDMI audio clock (the DVP gate on the 108 MHz oscillator)
const AUDIO_CLK_HZ: u64 = 108_000_000;

pub const SPI_HVS: usize = 32 + 97;
pub const SPI_PV2: usize = 32 + 101;

/// Which SoC's display pipeline.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Gen {
    /// the Pi 4's: HVS5, pixel valve 2, HDMI0
    Bcm2711,
    /// the Pi 3's (and Pi 1/2/Zero's): HVS (4), pixel valves 0..2, the BCM2835 HDMI
    Bcm2835,
}

// HVS registers (byte offsets)
const DISPCTRL: usize = 0x00;
const DISPSTAT: usize = 0x04;
const DISPID: usize = 0x08;
const DISPLIST0: usize = 0x20;
const DISPLACT0: usize = 0x30;
const DISPCTRL0: usize = 0x40;
const DISPSTAT0: usize = 0x48;
/// the BCM2835's gamma LUT address and data ports
const GAMADDR: usize = 0x78;
const GAMDATA: usize = 0xe0;

// pixel valve
const PV_CONTROL: usize = 0x00;
const PV_V_CONTROL: usize = 0x04;
const PV_HORZA: usize = 0x0c;
const PV_HORZB: usize = 0x10;
const PV_VERTA: usize = 0x14;
const PV_VERTB: usize = 0x18;
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

// the BCM2835 HDMI block (0x7e902000, 0x600 bytes)
pub const HDMI_2835_SIZE: u64 = 0x600;
const H_HOTPLUG: usize = 0x00c;
const H_FIFO_CTL: usize = 0x05c;
const H_RAM_PACKET_CONFIG: usize = 0x0a0;
const H_RAM_PACKET_STATUS: usize = 0x0a4;
const H_SCHEDULER_CONTROL: usize = 0x0c0;
const H_CEC_CNTRL_1: usize = 0x0e8;
const H_CEC_CNTRL_5: usize = 0x0f8;
const H_CEC_TX_DATA_1: usize = 0x0fc;
const H_CEC_RX_DATA_1: usize = 0x10c;
const H_CPU_STATUS: usize = 0x340;
const H_CPU_SET: usize = 0x344;
const H_CPU_CLEAR: usize = 0x348;
const H_CPU_MASK_STATUS: usize = 0x34c;
const H_CPU_MASK_SET: usize = 0x350;
const H_CPU_MASK_CLEAR: usize = 0x354;
const CPU_CEC: u32 = 1 << 6;
// CEC_CNTRL_1 / _5 bits
const CEC_TX_EOM: u32 = 1 << 31;
const CEC_TX_STATUS_GOOD: u32 = 1 << 30;
const CEC_RX_EOM: u32 = 1 << 29;
const CEC_RX_STATUS_GOOD: u32 = 1 << 28;
const CEC_CLEAR_RECEIVE_OFF: u32 = 1 << 21;
const CEC_START_XMIT_BEGIN: u32 = 1 << 20;
const CEC_TX_SW_RESET: u32 = 1 << 27;
const CEC_RX_SW_RESET: u32 = 1 << 26;
const CEC_RX_CEC_INT: u32 = 1 << 23;

const FRAME_NS: u64 = 16_666_667;
/// how far behind an audio FIFO's consumer may fall before it counts as an underrun (ns)
pub const MAX_CATCHUP_NS: u64 = 20_000_000;
/// the BCM2835's HVS channel each pixel valve reads (PV0: 0, PV1: 2, PV2: 1)
const PV_CHANNEL_2835: [usize; 3] = [0, 2, 1];
/// a CEC message takes this long on the bus before its reply (ns)
const CEC_REPLY_NS: u64 = 2_000_000;

// pixel valve: the even field's vertical timings (interlaced modes), the clock (encoder) select
const PV_VERTA_EVEN: usize = 0x1c;
const PV_VERTB_EVEN: usize = 0x20;
const PV_VCONTROL_INTERLACE: u32 = 1 << 4;
const PV_CLK_SELECT_HDMI: u32 = 1;
const PV_CLK_SELECT_VEC: u32 = 2;

// the VEC (0x7e806000, 0x1000 bytes): the registers vc4_vec.c uses
pub const VEC_SIZE: u64 = 0x1000;
const VEC_WSE_RESET: usize = 0xc0;
const VEC_REVID: usize = 0x100;
const VEC_CONFIG0: usize = 0x104;
const VEC_SOFT_RESET: usize = 0x10c;
const VEC_CONFIG1: usize = 0x188;
const VEC_INTERRUPT_STATUS: usize = 0x194;
const VEC_CFG: usize = 0x208;
const VEC_CFG_VEC_EN: u32 = 1 << 3;
const VEC_CONFIG0_SECAM_STD: u32 = 1 << 21;
const VEC_CONFIG0_CHRDIS: u32 = 1 << 7;
const VEC_CONFIG1_CUSTOM_FREQ: u32 = 1;
/// the VEC's clock (Hz) as the driver sets it; its pixel clock is an eighth of it
pub const VEC_CLK_HZ: u64 = 108_000_000;

/// What pixel valve 2 drives.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Output { Hdmi, Composite }

/// The TV on the HDMI port's CEC bus: logical address 0, physical address 0.0.0.0. It
/// acknowledges what is sent to it (or broadcast) and answers the usual queries.
#[derive(Clone, Debug)]
pub struct Cec {
    /// messages the TV sends, and the time the next may go
    rx: VecDeque<Vec<u8>>,
    rx_at: u64,
    pub tv_on: bool,
    /// messages seen on the bus (for the host's log)
    pub log: Vec<String>,
}

impl Default for Cec { fn default() -> Self { Cec { rx: VecDeque::new(), rx_at: 0, tv_on: true, log: Vec::new() } } }

/// The TV's name (`Give OSD Name`) and its vendor ID (a locally administered value).
const TV_NAME: &[u8] = b"sbc-emu TV";
const TV_VENDOR: [u8; 3] = [0x5b, 0xc0, 0xe0];

impl Cec {
    /// A message on the bus: whether it is acknowledged (a broadcast always is; a directed one
    /// when it is for the TV), and the TV's answer to it.
    fn sent(&mut self, msg: &[u8], now: u64) -> bool {
        if msg.is_empty() { return false; }
        let (from, to) = (msg[0] >> 4, msg[0] & 0xf);
        let hex: Vec<String> = msg.iter().map(|b| format!("{b:02x}")).collect();
        self.log.push(format!("{from:x}->{to:x}: {}", hex.join(":")));
        if self.log.len() > 256 { self.log.drain(..128); }
        if to != 0 && to != 0xf { return false; }
        let Some(&op) = msg.get(1) else { return true }; // a poll
        let reply: Option<Vec<u8>> = match op {
            0x83 => Some(vec![0x0f, 0x84, 0x00, 0x00, 0x00]), // Give Physical Address -> Report (TV)
            0x8c => Some([&[0x0f, 0x87][..], &TV_VENDOR].concat()),
            0x46 if to == 0 => Some([&[from, 0x47][..], TV_NAME].concat()),
            0x9f if to == 0 => Some(vec![from, 0x9e, 0x05]), // CEC 1.4
            0x8f if to == 0 => Some(vec![from, 0x90, if self.tv_on { 0 } else { 1 }]),
            0x91 if to == 0 => Some(vec![0x0f, 0x32, b'e', b'n', b'g']),
            0x04 | 0x0d => { self.tv_on = true; None } // Image/Text View On
            0x36 => { self.tv_on = false; None } // Standby
            // a request the TV does not support: Feature Abort (unrecognized opcode); answers,
            // announcements and key presses are taken silently
            0x00 | 0x32 | 0x44 | 0x45 | 0x47 | 0x72 | 0x7a | 0x7e | 0x80..=0x82 | 0x84 | 0x86 | 0x87 | 0x89 | 0x8a | 0x8b | 0x8e | 0x90 | 0x9d | 0x9e | 0xa0 => None,
            _ if to == 0 => Some(vec![from, 0x00, op, 0x00]),
            _ => None,
        };
        if let Some(mut r) = reply {
            if r[0] != 0x0f { r[0] = from; } // TV (0) to the initiator
            self.rx.push_back(r);
            if self.rx.len() == 1 { self.rx_at = now + CEC_REPLY_NS; }
        }
        true
    }
}

/// The monitor's EDID EEPROM at DDC address 0x50, for boards whose DDC bus is an I²C controller
/// (the BCM2835's BSC2): a write sets the address pointer, reads continue from it.
pub struct EdidEeprom {
    pub edid: Vec<u8>,
    ptr: usize,
    /// a monitor is plugged in (no EEPROM answers otherwise)
    pub connected: bool,
}

impl EdidEeprom {
    pub fn new(edid: Vec<u8>) -> Self { EdidEeprom { edid, ptr: 0, connected: true } }
}

impl crate::i2c::I2cDevice for EdidEeprom {
    fn address(&self) -> u8 { if self.connected { 0x50 } else { 0xff } }
    fn write(&mut self, data: &[u8]) { if let Some(&p) = data.first() { self.ptr = p as usize % self.edid.len().max(1); } }
    fn read(&mut self, out: &mut [u8]) {
        for b in out.iter_mut() {
            *b = self.edid.get(self.ptr).copied().unwrap_or(0xff);
            self.ptr = (self.ptr + 1) % self.edid.len().max(1);
        }
    }
}

pub struct Vc4 {
    pub gen: Gen,
    hvs: Vec<u32>,
    /// frames shown on each HVS channel (6-bit counters in DISPSTAT1/2)
    frcnt: [u32; 3],
    /// DISPSTAT's end-of-frame flags (bit 8 + 8 * channel), raised as each channel starts a frame
    /// and cleared by writing them; not saved (the next frame raises them again)
    eof: u32,
    /// pixel valves 0..2 (the BCM2711 model has PV2 only)
    pv: [[u32; 64]; 3],
    /// the BCM2711's HDMI window; the BCM2835's HDMI block
    win: Vec<u32>,
    hd: [u32; 64],
    txp: [u32; 8],
    /// the DDC bus: the EDID's address pointer
    edid_ptr: usize,
    edid: Vec<u8>,
    /// next vblank of each pixel valve (ns of guest time)
    next_vblank: [u64; 3],
    /// HDMI audio (MAI): play position (ns), fractional frames, the channel a word belongs to
    mai_played_to: u64,
    mai_frac: u64,
    mai_ch: u32,
    mai_left: i16,
    /// HDMI audio played, 16-bit interleaved stereo, and its rate
    pub audio: Vec<i16>,
    pub audio_rate: u32,
    /// the clock the MAI sample rate divides (Hz): the BCM2711's 108 MHz, the BCM2835's HDMI
    /// state machine clock (the machine keeps it at the firmware clock's rate)
    pub audio_clk_hz: u64,
    /// the BCM2835's pixel clock (Hz, the firmware's PIXEL clock): pixel valve 2's frame rate
    pub pixel_hz: u64,
    /// the BCM2835's VEC clock (Hz, the firmware's VEC clock): the composite pixel rate is 1/8
    pub vec_hz: u64,
    /// the VEC's registers (BCM2835 family)
    vec: Vec<u32>,
    /// the BCM2835 HVS's gamma LUT (3 channels x R, G, B x 256) and its address port
    gamma: Vec<u32>,
    gam_addr: u32,
    pub cec: Cec,
    /// the latest guest time seen (ns)
    now: u64,
    /// a monitor is plugged into HDMI
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
        let mut v = Self::new_inner(Gen::Bcm2711);
        v.edid = edid_with((w, h));
        v
    }

    /// The BCM2835-family pipeline (Pi 3) with a monitor whose preferred mode is `w`x`h`.
    pub fn bcm2835(w: u32, h: u32) -> Self {
        let mut v = Self::new_inner(Gen::Bcm2835);
        v.edid = edid_with((w, h));
        v
    }

    fn new_inner(gen: Gen) -> Self {
        let win = match gen { Gen::Bcm2711 => HDMI_WIN_SIZE, Gen::Bcm2835 => HDMI_2835_SIZE } as usize / 4;
        Vc4 { gen, hvs: vec![0; (HVS_SIZE / 4) as usize], frcnt: [0; 3], eof: 0, pv: [[0; 64]; 3], win: vec![0; win], hd: [0; 64], txp: [0; 8],
              edid_ptr: 0, edid: edid(), next_vblank: [FRAME_NS; 3], connected: true, frames: 0,
              mai_played_to: 0, mai_frac: 0, mai_ch: 0, mai_left: 0, audio: Vec::new(), audio_rate: 0,
              audio_clk_hz: AUDIO_CLK_HZ, pixel_hz: 0, vec_hz: VEC_CLK_HZ, vec: vec![0; (VEC_SIZE / 4) as usize],
              gamma: (0..9 * 256).map(|k| (k % 256) as u32).collect(), gam_addr: 0, cec: Cec::default(), now: 0,
              trace: std::env::var_os("SBC_TRACE_VC4").is_some() }
    }

    /// The monitor's EDID (for a DDC bus outside this block).
    pub fn edid(&self) -> &[u8] { &self.edid }

    // ------------------------------------------------------------------ HVS

    fn dlist_base(&self) -> usize { match self.gen { Gen::Bcm2711 => 0x4000, Gen::Bcm2835 => 0x2000 } }

    /// A channel's size from its DISPCTRLx.
    fn chan_size(&self, c: u32) -> (u32, u32) {
        match self.gen {
            Gen::Bcm2711 => ((c >> 16) & 0x1fff, c & 0x1fff),
            Gen::Bcm2835 => ((c >> 12) & 0xfff, c & 0xfff),
        }
    }

    fn chan_on(&self, ch: usize) -> bool { self.hvs[(DISPCTRL0 + 0x10 * ch) / 4] & (1 << 31) != 0 }

    pub fn hvs_read(&self, off: u64) -> u32 {
        let o = off as usize & !3;
        match o {
            DISPSTAT => self.eof, // end-of-frame flags (underruns are not modelled)
            DISPID => 0x0000_0001,
            // channel status: running or empty, the line counter at 0; DISPSTAT1/2 also carry
            // the frame counters (FRCNT0/1 in DISPSTAT1, FRCNT2 in DISPSTAT2)
            o if (DISPSTAT0..DISPSTAT0 + 0x30).contains(&o) && (o - DISPSTAT0) % 0x10 == 0 => {
                let ch = (o - DISPSTAT0) / 0x10;
                let mut v = if self.chan_on(ch) { 2 << 30 } else { 1 << 28 };
                let (s0, s1) = match self.gen { Gen::Bcm2711 => (20, 14), Gen::Bcm2835 => (18, 12) };
                match ch {
                    1 => v |= (self.frcnt[0] & 63) << s0 | (self.frcnt[1] & 63) << s1,
                    2 => v |= (self.frcnt[2] & 63) << s1,
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
        if self.trace && o < self.dlist_base() { eprintln!("[hvs] {o:#x} <- {v:#x}"); }
        match o {
            DISPSTAT => self.eof &= !v, // write-one-to-clear
            o if (DISPCTRL0..DISPCTRL0 + 0x30).contains(&o) && (o - DISPCTRL0) % 0x10 == 0 => {
                // RESET clears the channel; the frame counter restarts
                if v & (1 << 30) != 0 { self.frcnt[(o - DISPCTRL0) / 0x10] = 0; }
                self.hvs[o / 4] = v & !(1 << 30);
            }
            GAMADDR if self.gen == Gen::Bcm2835 => { self.gam_addr = v; self.hvs[o / 4] = v; }
            GAMDATA if self.gen == Gen::Bcm2835 => {
                let a = (self.gam_addr & 0xfff) as usize;
                if let Some(g) = self.gamma.get_mut(a) { *g = v; }
                if self.gam_addr & (1 << 31) != 0 { self.gam_addr = (self.gam_addr & !0xfff) | ((a as u32 + 1) & 0xfff); }
            }
            o if o < HVS_SIZE as usize => self.hvs[o / 4] = v,
            _ => {}
        }
    }

    // ------------------------------------------------------------------ pixel valves

    fn pv_running(&self, n: usize) -> bool { self.pv[n][PV_CONTROL / 4] & 1 != 0 && self.pv[n][PV_V_CONTROL / 4] & 1 != 0 }

    /// Pixel valve 2's registers (the BCM2711's only one).
    pub fn pv_read(&self, off: u64) -> u32 { self.pv_read_n(2, off) }
    pub fn pv_write(&mut self, off: u64, v: u32) { self.pv_write_n(2, off, v) }

    pub fn pv_read_n(&self, n: usize, off: u64) -> u32 { self.pv[n][(off as usize / 4) & 63] }

    pub fn pv_write_n(&mut self, n: usize, off: u64, v: u32) {
        let r = (off as usize / 4) & 63;
        if self.trace { eprintln!("[pv{n}] {:#x} <- {v:#x}", r * 4); }
        let pv = &mut self.pv[n];
        match r * 4 {
            PV_INTSTAT => pv[r] &= !v,
            PV_CONTROL => pv[r] = v & !(1 << 1), // FIFO_CLR self-clears
            _ => pv[r] = v,
        }
    }

    /// Pixel valve 2's interrupt.
    pub fn irq_pv(&self) -> bool { self.irq_pv_n(2) }
    pub fn irq_pv_n(&self, n: usize) -> bool { self.pv[n][PV_INTSTAT / 4] & self.pv[n][PV_INTEN / 4] != 0 }

    /// The HVS interrupt: an end of frame on a channel whose EOF interrupt is enabled
    /// (DISPCTRL DSPEIEOF: bit 7 + 4 * channel on the BCM2711's HVS, 7 + 2 * channel on the
    /// BCM2835's). Linux uses it to free the display-list memory of frames no longer shown.
    pub fn irq_hvs(&self) -> bool { self.eof_enabled() & self.eof != 0 }

    fn eof_enabled(&self) -> u32 {
        let c = self.hvs[DISPCTRL / 4];
        let step = match self.gen { Gen::Bcm2711 => 4, Gen::Bcm2835 => 2 };
        (0..3).filter(|ch| c & (1 << (7 + step * ch)) != 0).fold(0, |m, ch| m | 1 << (8 + 8 * ch))
    }

    /// The pixel valves that run, with the HVS channels they start frames on.
    fn valves(&self) -> Vec<(usize, Vec<usize>)> {
        match self.gen {
            // PV2 starts a frame on every channel (one display, as before)
            Gen::Bcm2711 => if self.pv_running(2) { vec![(2, vec![0, 1, 2])] } else { vec![] },
            Gen::Bcm2835 => (0..3).filter(|&n| self.pv_running(n)).map(|n| (n, vec![PV_CHANNEL_2835[n]])).collect(),
        }
    }

    /// A pixel valve's frame time (ns): the BCM2711's at 60 Hz; the BCM2835's from the mode it
    /// was given (its timings) and the pixel clock of the encoder it drives (PV2: HDMI's pixel
    /// clock, or the VEC's 13.5 MHz), else 60 Hz. Interlaced, a "frame" is a field (vblank comes
    /// once per field): the two fields' lines (the even one's from VERTA/B_EVEN) halved.
    fn frame_ns(&self, n: usize) -> u64 {
        if self.gen == Gen::Bcm2711 || n != 2 { return FRAME_NS; }
        let hz = match self.pv_output() { Some(Output::Composite) => self.vec_hz / 8, _ => self.pixel_hz };
        if hz == 0 { return FRAME_NS; }
        let p = &self.pv[n];
        let f = |r: usize| (p[r / 4] >> 16) as u64 + (p[r / 4] & 0xffff) as u64;
        let ht = f(PV_HORZA) + f(PV_HORZB);
        let vt2 = if p[PV_V_CONTROL / 4] & PV_VCONTROL_INTERLACE != 0 {
            f(PV_VERTA) + f(PV_VERTB) + f(PV_VERTA_EVEN) + f(PV_VERTB_EVEN)
        } else {
            2 * (f(PV_VERTA) + f(PV_VERTB))
        };
        let t = ht * vt2 * 1_000_000_000 / (2 * hz);
        if (1_000_000..=200_000_000).contains(&t) { t } else { FRAME_NS }
    }

    /// Guest time passes: vblanks while the pixel valves run, CEC traffic.
    pub fn tick(&mut self, now: u64) {
        self.now = now;
        if self.gen == Gen::Bcm2835 { self.cec_tick(now); }
        for n in 0..3 {
            if !self.pv_running(n) { self.next_vblank[n] = now + self.frame_ns(n); }
        }
        for (n, chans) in self.valves() {
            if now < self.next_vblank[n] { continue; }
            let frame = self.frame_ns(n);
            let k = (now - self.next_vblank[n]) / frame + 1;
            self.next_vblank[n] += k * frame;
            if n == 2 { self.frames += k; }
            // a new frame starts: each channel takes the display list it was given
            for ch in chans {
                if self.chan_on(ch) {
                    self.hvs[(DISPLACT0 + 4 * ch) / 4] = self.hvs[(DISPLIST0 + 4 * ch) / 4];
                    self.frcnt[ch] = self.frcnt[ch].wrapping_add(k as u32);
                    self.eof |= 1 << (8 + 8 * ch);
                }
            }
            self.pv[n][PV_INTSTAT / 4] |= PV_INT_VFP_START;
        }
    }

    /// The next vblank the driver waits for, and CEC traffic due.
    pub fn next_event(&self) -> Option<u64> {
        let eof = self.eof_enabled() != 0;
        let vb = self.valves().into_iter().filter(|(n, _)| self.pv[*n][PV_INTEN / 4] & PV_INT_VFP_START != 0 || eof).map(|(n, _)| self.next_vblank[n]).min();
        let cec = (self.gen == Gen::Bcm2835 && !self.cec.rx.is_empty()).then_some(self.cec.rx_at.max(self.now + 1));
        [vb, cec].into_iter().flatten().min()
    }

    // ------------------------------------------------------------------ HDMI0, DVP, DDC (BCM2711)

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

    // ------------------------------------------------------------------ the BCM2835 HDMI block

    pub fn hdmi_read(&mut self, off: u64) -> u32 {
        let o = off as usize & !3;
        let w = |o: usize| self.win.get(o / 4).copied().unwrap_or(0);
        let r = match o {
            0x000 => 0x0000_0001, // CORE_REV
            H_HOTPLUG => self.connected as u32,
            H_RAM_PACKET_STATUS => w(H_RAM_PACKET_CONFIG) & 0xffff,
            H_FIFO_CTL => w(o) | if w(o) & (1 << 6) != 0 { 1 << 14 } else { 0 }, // RECENTER_DONE
            H_SCHEDULER_CONTROL => (w(o) & !2) | (w(o) & 1) << 1, // HDMI_ACTIVE follows MODE_HDMI
            _ => w(o),
        };
        if self.trace { eprintln!("[hdmi] read {o:#x} = {r:#x}"); }
        r
    }

    pub fn hdmi_write(&mut self, off: u64, v: u32) {
        let o = off as usize & !3;
        if self.trace { eprintln!("[hdmi] {o:#x} <- {v:#x}"); }
        if o >= self.win.len() * 4 { return; }
        match o {
            H_CPU_SET => self.win[H_CPU_STATUS / 4] |= v,
            H_CPU_CLEAR => self.win[H_CPU_STATUS / 4] &= !v,
            H_CPU_MASK_SET => self.win[H_CPU_MASK_STATUS / 4] |= v,
            H_CPU_MASK_CLEAR => self.win[H_CPU_MASK_STATUS / 4] &= !v,
            H_CPU_STATUS | H_CPU_MASK_STATUS => {}
            H_CEC_CNTRL_1 => self.cec_cntrl1(v),
            H_CEC_CNTRL_5 => {
                if v & CEC_RX_SW_RESET != 0 { self.win[o / 4] &= !CEC_RX_CEC_INT; }
                self.win[o / 4] = (v & !CEC_RX_CEC_INT) | (self.win[o / 4] & CEC_RX_CEC_INT);
            }
            _ => self.win[o / 4] = v,
        }
    }

    /// The HDMI block's interrupt (the CEC controller's, through the CPU status and mask).
    pub fn irq_hdmi(&self) -> bool {
        self.gen == Gen::Bcm2835 && self.win[H_CPU_STATUS / 4] & !self.win[H_CPU_MASK_STATUS / 4] & CPU_CEC != 0
    }

    /// CEC_CNTRL_1: START_XMIT_BEGIN sends the message in TX_DATA; CLEAR_RECEIVE_OFF frees the
    /// receiver for the next message.
    fn cec_cntrl1(&mut self, v: u32) {
        let old = self.win[H_CEC_CNTRL_1 / 4];
        // the status bits are the controller's
        const STATUS: u32 = CEC_TX_EOM | CEC_TX_STATUS_GOOD | CEC_RX_EOM | CEC_RX_STATUS_GOOD | (0xf << 24);
        let mut n = (v & !STATUS) | (old & STATUS);
        if v & CEC_CLEAR_RECEIVE_OFF != 0 {
            n &= !(CEC_RX_EOM | CEC_RX_STATUS_GOOD | (0xf << 24));
            self.win[H_CEC_CNTRL_5 / 4] &= !CEC_RX_CEC_INT;
        }
        if v & CEC_START_XMIT_BEGIN != 0 && old & CEC_START_XMIT_BEGIN == 0 && self.win[H_CEC_CNTRL_5 / 4] & CEC_TX_SW_RESET == 0 {
            let len = (((v >> 16) & 0xf) + 1) as usize;
            let msg: Vec<u8> = (0..len).map(|i| (self.win[H_CEC_TX_DATA_1 / 4 + i / 4] >> (8 * (i % 4))) as u8).collect();
            let ack = self.cec.sent(&msg, self.now);
            n = (n & !CEC_TX_STATUS_GOOD) | CEC_TX_EOM | if ack { CEC_TX_STATUS_GOOD } else { 0 };
            // the transmit-done interrupt (RX_CEC_INT clear says it is not a reception)
            self.win[H_CEC_CNTRL_5 / 4] &= !CEC_RX_CEC_INT;
            self.win[H_CPU_STATUS / 4] |= CPU_CEC;
        }
        self.win[H_CEC_CNTRL_1 / 4] = n;
    }

    /// The TV's next message reaches the receiver once it is free and the controller is idle.
    fn cec_tick(&mut self, now: u64) {
        if self.cec.rx.is_empty() || now < self.cec.rx_at { return; }
        let c5 = self.win[H_CEC_CNTRL_5 / 4];
        if self.win[H_CPU_STATUS / 4] & CPU_CEC != 0 || c5 & (CEC_RX_CEC_INT | CEC_RX_SW_RESET) != 0 { return; }
        if self.win[H_CEC_CNTRL_1 / 4] & CEC_START_XMIT_BEGIN != 0 { return; }
        let msg = self.cec.rx.pop_front().unwrap();
        let hex: Vec<String> = msg.iter().map(|b| format!("{b:02x}")).collect();
        self.cec.log.push(format!("{:x}->{:x}: {}", msg[0] >> 4, msg[0] & 0xf, hex.join(":")));
        for k in 0..4 {
            let w = (0..4).fold(0u32, |a, b| a | (msg.get(4 * k + b).copied().unwrap_or(0) as u32) << (8 * b));
            self.win[H_CEC_RX_DATA_1 / 4 + k] = w;
        }
        let c1 = self.win[H_CEC_CNTRL_1 / 4] & !(0xf << 24);
        self.win[H_CEC_CNTRL_1 / 4] = c1 | ((msg.len() as u32 - 1) << 24) | CEC_RX_EOM | CEC_RX_STATUS_GOOD;
        self.win[H_CEC_CNTRL_5 / 4] |= CEC_RX_CEC_INT;
        self.win[H_CPU_STATUS / 4] |= CPU_CEC;
        self.cec.rx_at = now + CEC_REPLY_NS;
    }

    // ------------------------------------------------------------------ the VEC (BCM2835)

    pub fn vec_read(&self, off: u64) -> u32 {
        let o = off as usize & !3;
        let r = match o {
            // the revision Linux does not check; the value is the model's
            VEC_REVID => 0x0000_2708,
            o if o < VEC_SIZE as usize => self.vec[o / 4],
            _ => 0,
        };
        if self.trace { eprintln!("[vec] read {o:#x} = {r:#x}"); }
        r
    }

    pub fn vec_write(&mut self, off: u64, v: u32) {
        let o = off as usize & !3;
        if self.trace { eprintln!("[vec] {o:#x} <- {v:#x}"); }
        match o {
            // the resets complete at once
            VEC_WSE_RESET | VEC_SOFT_RESET | VEC_REVID => {}
            VEC_INTERRUPT_STATUS => self.vec[o / 4] &= !v,
            o if o < VEC_SIZE as usize => self.vec[o / 4] = v,
            _ => {}
        }
    }

    /// The VEC encodes (VEC_CFG's VEC_EN, as the driver's enable leaves it).
    fn vec_on(&self) -> bool { self.gen == Gen::Bcm2835 && self.vec[VEC_CFG / 4] & VEC_CFG_VEC_EN != 0 }

    /// The TV standard the VEC was set up for, by the names `vc4_vec.c` gives its modes.
    pub fn tv_norm(&self) -> Option<&'static str> {
        if !self.vec_on() { return None; }
        let (c0, c1) = (self.vec[VEC_CONFIG0 / 4], self.vec[VEC_CONFIG1 / 4]);
        let custom = c1 & VEC_CONFIG1_CUSTOM_FREQ != 0;
        Some(if c0 & VEC_CONFIG0_SECAM_STD != 0 { "SECAM" } else if c0 & VEC_CONFIG0_CHRDIS != 0 { "monochrome" } else {
            match (c0 & 3, custom) {
                (0, false) => "NTSC",
                (0, true) => "NTSC-443",
                (1, _) => "PAL",
                (2, false) => "PAL-M",
                (2, true) => "PAL-60",
                _ => "PAL-N",
            }
        })
    }

    /// The encoder pixel valve 2 drives (its clock select) while it runs: HDMI or the VEC.
    fn pv_output(&self) -> Option<Output> {
        if self.gen == Gen::Bcm2711 { return Some(Output::Hdmi); }
        match (self.pv[2][PV_CONTROL / 4] >> 2) & 3 {
            PV_CLK_SELECT_HDMI => Some(Output::Hdmi),
            PV_CLK_SELECT_VEC => Some(Output::Composite),
            _ => None,
        }
    }

    /// The output the host shows: HDMI, or composite while pixel valve 2 drives a running VEC.
    pub fn output(&self) -> Option<Output> {
        self.shown()?;
        match self.pv_output()? {
            Output::Composite if !self.vec_on() => None,
            o => Some(o),
        }
    }

    // ------------------------------------------------------------------ TXP, HD

    pub fn txp_read(&self, off: u64) -> u32 { self.txp[(off as usize / 4) & 7] }
    pub fn txp_write(&mut self, off: u64, v: u32) { self.txp[(off as usize / 4) & 7] = v & !(1 << 31); } // TXP_BUSY never set

    /// The "hd" block's MAI registers: control, data, sample-rate divider.
    fn mai_regs(&self) -> (usize, usize, usize) {
        match self.gen { Gen::Bcm2711 => (0x10, 0x1c, 0x20), Gen::Bcm2835 => (0x14, 0x20, 0x2c) }
    }

    pub fn hd_read(&self, off: u64) -> u32 {
        let r = (off as usize / 4) & 63;
        if r * 4 == self.mai_regs().0 { return self.hd[r] | 1 << 10; } // MAI_CTL: the FIFO drains as fast as it is filled (EMPTY)
        self.hd[r]
    }
    pub fn hd_write(&mut self, off: u64, v: u32) {
        let r = (off as usize / 4) & 63;
        let (ctl, data, _) = self.mai_regs();
        if r * 4 == ctl {
            // MAI_CTL: RESET and FLUSH self-clear (and restart the channel order); error bits are W1C
            if v & (1 | 1 << 9) != 0 { self.mai_ch = 0; }
            self.hd[r] = v & !(1 | 1 << 9 | 1 << 1 | 1 << 2 | 1 << 15);
        } else if r * 4 == data {
            self.mai_push(v);
        } else {
            self.hd[r] = v;
        }
    }

    // ------------------------------------------------------------------ HDMI audio

    fn mai_ctl(&self) -> u32 { self.hd[self.mai_regs().0 / 4] }
    fn mai_channels(&self) -> u32 { ((self.mai_ctl() >> 4) & 0xf).max(1) }

    /// The audio sample rate: the audio clock * (M + 1) / N (MAI_SMP).
    pub fn mai_rate(&self) -> u32 {
        let smp = self.hd[self.mai_regs().2 / 4];
        let (n, m) = ((smp >> 8) as u64, (smp & 0xff) as u64 + 1);
        if n == 0 { 0 } else { (self.audio_clk_hz * m / n) as u32 }
    }

    /// Words the MAI FIFO takes from the DMA engine between the last call and `now` (its DREQ).
    pub fn mai_demand(&mut self, now: u64) -> u64 {
        let on = self.mai_ctl() & (1 << 3) != 0;
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
        if self.mai_ctl() & (1 << 3) == 0 { return; }
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

    /// The channel shown on HDMI: on the BCM2711 the enabled one, on the BCM2835 the one pixel
    /// valve 2 reads (channel 1); its size from DISPCTRLx.
    fn shown(&self) -> Option<(usize, u32, u32)> {
        if !self.pv_running(2) || self.hvs[DISPCTRL / 4] & (1 << 31) == 0 { return None; }
        let chans: &[usize] = match self.gen { Gen::Bcm2711 => &[0, 1, 2], Gen::Bcm2835 => &[1] };
        chans.iter().find_map(|&ch| {
            let c = self.hvs[(DISPCTRL0 + 0x10 * ch) / 4];
            let (w, h) = self.chan_size(c);
            (c & (1 << 31) != 0 && w > 0 && h > 0).then_some((ch, w, h))
        })
    }

    pub fn active(&self) -> bool { self.output().is_some() }

    /// The mode on the output shown (HDMI or composite), while the pipeline runs.
    pub fn size(&self) -> Option<(u32, u32)> { self.output().and(self.shown()).map(|(_, w, h)| (w, h)) }

    /// Compose the planes of the channel shown into RGBA; None if no output shows anything.
    /// An interlaced mode's channel holds the whole frame (both fields); a monochrome TV
    /// standard shows the luma only.
    pub fn render(&self, ram: &[u8], out: &mut Vec<u8>) -> Option<(u32, u32)> {
        let output = self.output()?;
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
        let base = self.dlist_base() / 4;
        let mut i = (self.hvs[(DISPLACT0 + 4 * ch) / 4] as usize) & 0xfff;
        let dl = |k: usize| if base + k < self.hvs.len() { self.hvs[base + k] } else { 1 << 31 };
        for n in 0..64 {
            let ctl0 = dl(i);
            if ctl0 & (1 << 31) != 0 || ctl0 & (1 << 30) == 0 { break; } // end of list
            let size = ((ctl0 >> 24) & 0x3f) as usize;
            if size == 0 { break; }
            if let Some(p) = self.decode_plane(&|k| dl(i + k), ctl0) { self.draw_plane(ram, &p, n == 0, out, wu, hu); }
            i += size;
        }
        // the BCM2835's gamma, when the channel has it on (the LUT the driver loaded)
        if self.gen == Gen::Bcm2835 && bk & (1 << 29) != 0 {
            let lut = &self.gamma[ch * 768..ch * 768 + 768];
            for p in out.chunks_mut(4) {
                for c in 0..3 { p[c] = lut[c * 256 + p[c] as usize].min(255) as u8; }
            }
        }
        if output == Output::Composite && self.vec[VEC_CONFIG0 / 4] & VEC_CONFIG0_CHRDIS != 0 {
            for p in out.chunks_mut(4) {
                let y = ((p[0] as u32 * 299 + p[1] as u32 * 587 + p[2] as u32 * 114) / 1000) as u8;
                p[..3].fill(y);
            }
        }
        Some((w, h))
    }

    /// One display-list plane entry in this generation's layout.
    fn decode_plane(&self, e: &dyn Fn(usize) -> u32, ctl0: u32) -> Option<Plane> {
        let order = (ctl0 >> 13) & 3;
        let tiling = (ctl0 >> 20) & 3;
        let mut p = Plane { ctl0, order, tiling, ..Default::default() };
        let mut k;
        match self.gen {
            Gen::Bcm2711 => {
                p.fmt = ctl0 & 0x1f; // SCALER5_CTL0_PIXEL_FORMAT: 5 bits on the HVS5
                p.unity = ctl0 & (1 << 15) != 0;
                let pos0 = e(1);
                (p.x0, p.y0) = ((pos0 & 0xfff) as usize, ((pos0 >> 16) & 0xfff) as usize);
                k = 3; // control word 2 at 2
                if !p.unity { let p1 = e(k); (p.dw, p.dh) = ((p1 & 0x1fff) as usize, ((p1 >> 16) & 0x1fff) as usize); k += 1; }
                let pos2 = e(k);
                (p.sw, p.sh) = ((pos2 & 0x1fff) as usize, ((pos2 >> 16) & 0x1fff) as usize);
                // per-pixel alpha above the bottom plane, as before
                p.alpha_mode = 0;
                p.fixed_alpha = 255;
                p.opaque_bottom = true;
            }
            Gen::Bcm2835 => {
                p.fmt = ctl0 & 0xf;
                p.unity = ctl0 & (1 << 4) != 0;
                p.hflip = ctl0 & (1 << 16) != 0;
                p.vflip = ctl0 & (1 << 15) != 0;
                // the BCM2835's ABGR/ARGB orders are the HVS5's swapped (XRGB8888 is ABGR here)
                if matches!(p.fmt, 1 | 3 | 7) && p.order >= 2 { p.order ^= 1; }
                let pos0 = e(1);
                (p.x0, p.y0) = ((pos0 & 0xfff) as usize, ((pos0 >> 12) & 0xfff) as usize);
                p.fixed_alpha = (pos0 >> 24) as u8;
                k = 2;
                if !p.unity { let p1 = e(k); (p.dw, p.dh) = ((p1 & 0xfff) as usize, ((p1 >> 16) & 0xfff) as usize); k += 1; }
                let pos2 = e(k);
                (p.sw, p.sh) = ((pos2 & 0xfff) as usize, ((pos2 >> 16) & 0xfff) as usize);
                p.alpha_mode = pos2 >> 30;
                p.premult = pos2 & (1 << 29) != 0;
                p.mix = pos2 & (1 << 28) != 0;
            }
        }
        if p.unity { (p.dw, p.dh) = (p.sw, p.sh); }
        // pointers (one per plane of the format), their context words, the pitches
        let planes = match p.fmt { 8 | 10 => 3, 9 | 11 => 2, _ => 1 };
        let mask = if self.gen == Gen::Bcm2835 { 0x3fff_ffff } else { u32::MAX }; // the bus alias
        for j in 0..planes {
            p.ptr[j] = (e(k + 2 + j) & mask) as usize;
            p.pitch[j] = e(k + 2 + 2 * planes + j);
        }
        p.planes = planes;
        if planes > 1 {
            let c = k + 2 + 3 * planes;
            p.csc = [e(c), e(c + 1), e(c + 2)];
        }
        Some(p)
    }

    /// Draw one plane: RGB and YUV formats, linear or (RGB) T-tiled, nearest-neighbour scaling,
    /// alpha as the plane's mode says.
    fn draw_plane(&self, ram: &[u8], p: &Plane, bottom: bool, out: &mut [u8], w: usize, h: usize) {
        let (fmt, order, tiling, ctl0) = (p.fmt, p.order, p.tiling, p.ctl0);
        let skip = |why: &str| {
            // each kind of plane the model cannot show, once
            thread_local! { static SEEN: std::cell::RefCell<Vec<u32>> = Default::default(); }
            let key = ctl0 & 0x0030_e01f;
            if SEEN.with(|s| { let mut s = s.borrow_mut(); let new = !s.contains(&key); if new { s.push(key); } new }) {
                eprintln!("[vc4] HVS plane not shown ({why}): ctl0={ctl0:#010x} format {fmt} order {order} tiling {tiling}");
            }
        };
        if tiling != 0 && tiling != 3 { return skip("64/128/256-byte tiling"); }
        let (x0, y0, dw, dh, pw, ph) = (p.x0, p.y0, p.dw.max(1), p.dh.max(1), p.sw, p.sh);
        if pw == 0 || ph == 0 { return; }
        if p.planes > 1 {
            if tiling != 0 { return skip("tiled YUV"); }
            return self.draw_yuv(ram, p, out, w, h);
        }
        let ptr = p.ptr[0];
        let pitch0 = p.pitch[0];
        let pitch = (pitch0 & 0xffff) as usize;
        let bpp = match fmt { 0 => 1, 1..=4 => 2, 5 => 3, 7 | 16 => 4, _ => return skip("pixel format") };
        // T-tiled (DRM_FORMAT_MOD_BROADCOM_VC4_T_TILED, what GNOME's compositor and Mesa's vc4
        // scan out): the pointer was advanced to the source origin; undo that to the start of its
        // even row of 4 KiB tiles and address source pixels from there (vc4_plane.c, Mesa's
        // vc4_tiling.c)
        let t = (tiling == 3).then(|| {
            if bpp != 2 && bpp != 4 { return None; }
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
        let addr = |sx: usize, sy: usize| -> Option<usize> {
            Some(match t {
                Some((base, ox, oy, tiles_w)) => base + t_format_offset(ox + sx, oy + sy, bpp, tiles_w),
                None if p.vflip => ptr.checked_sub(sy * pitch)? + sx * bpp,
                None => ptr + sy * pitch + sx * bpp,
            })
        };
        let has_alpha = matches!(fmt, 1 | 3 | 7 | 16);
        for dy in 0..dh {
            let y = y0 + dy;
            if y >= h { break; }
            let sy = dy * ph / dh;
            for dx in 0..dw {
                let x = x0 + dx;
                if x >= w { break; }
                let sx = dx * pw / dw;
                let sx = if p.hflip { pw - 1 - sx } else { sx };
                let Some(a0) = addr(sx, sy) else { continue };
                let Some(px) = ram.get(a0..a0 + bpp) else { continue };
                let (r, g, b, a) = rgb_pixel(fmt, order, px);
                let o = (y * w + x) * 4;
                blend(&mut out[o..o + 3], [r, g, b], a, p, bottom, has_alpha);
            }
        }
    }

    /// A plane in two (Y, CbCr) or three (Y, Cb, Cr) planes, 4:2:0 or 4:2:2, converted with the
    /// display list's colour-space words.
    fn draw_yuv(&self, ram: &[u8], p: &Plane, out: &mut [u8], w: usize, h: usize) {
        let vsub = if matches!(p.fmt, 8 | 9) { 2 } else { 1 };
        let pitch: [usize; 3] = std::array::from_fn(|j| (p.pitch[j] & 0xffff) as usize);
        let csc = Csc::from_words(p.csc);
        let swap = p.order & 1 != 0; // XYCRCB: Cr first
        let (x0, y0, dw, dh, pw, ph) = (p.x0, p.y0, p.dw.max(1), p.dh.max(1), p.sw, p.sh);
        for dy in 0..dh {
            let y = y0 + dy;
            if y >= h { break; }
            let sy = dy * ph / dh;
            let (ly, cy) = (sy, sy / vsub);
            for dx in 0..dw {
                let x = x0 + dx;
                if x >= w { break; }
                let sx = dx * pw / dw;
                let cx = sx / 2;
                let get = |a: usize| ram.get(a).copied();
                let Some(yv) = get(p.ptr[0] + ly * pitch[0] + sx) else { continue };
                let (c1, c2) = if p.planes == 2 {
                    let a = p.ptr[1] + cy * pitch[1] + 2 * cx;
                    (get(a).unwrap_or(128), get(a + 1).unwrap_or(128))
                } else {
                    (get(p.ptr[1] + cy * pitch[1] + cx).unwrap_or(128), get(p.ptr[2] + cy * pitch[2] + cx).unwrap_or(128))
                };
                let (cb, cr) = if swap { (c2, c1) } else { (c1, c2) };
                let o = (y * w + x) * 4;
                let rgb = csc.convert(yv, cb, cr);
                blend(&mut out[o..o + 3], rgb, 255, p, true, false);
            }
        }
    }

    pub fn save_state(&self, w: &mut emu_core::snap::W) {
        if self.gen == Gen::Bcm2835 { return self.save_2835(w); }
        // v2 adds the monitor's EDID, so a restore keeps the mode the guest chose from it
        w.tag("vc4-v2");
        for v in &self.hvs { w.u32(*v); }
        for v in self.frcnt { w.u32(v); }
        for v in self.pv[2] { w.u32(v); }
        for v in &self.win { w.u32(*v); }
        for v in self.hd { w.u32(v); }
        w.u64(self.edid_ptr as u64); w.u64(self.next_vblank[2]); w.bool(self.connected);
        w.u64(self.mai_played_to); w.u64(self.mai_frac); w.u32(self.mai_ch);
        w.bytes(&self.edid);
    }
    pub fn load_state(&mut self, r: &mut emu_core::snap::R) -> Result<(), emu_core::snap::SnapError> {
        if self.gen == Gen::Bcm2835 { return self.load_2835(r); }
        let v2 = match r.bytes()? { b"vc4-v2" => true, b"vc4-v1" => false, t => return Err(emu_core::snap::SnapError(format!("snapshot layout mismatch: expected vc4 state, found {}", String::from_utf8_lossy(t)))) };
        for v in self.hvs.iter_mut() { *v = r.u32()?; }
        for v in self.frcnt.iter_mut() { *v = r.u32()?; }
        for v in self.pv[2].iter_mut() { *v = r.u32()?; }
        for v in self.win.iter_mut() { *v = r.u32()?; }
        for v in self.hd.iter_mut() { *v = r.u32()?; }
        self.edid_ptr = r.u64()? as usize; self.next_vblank[2] = r.u64()?; self.connected = r.bool()?;
        self.mai_played_to = r.u64()?; self.mai_frac = r.u64()?; self.mai_ch = r.u32()?;
        if v2 { self.edid = r.bytes()?.to_vec(); }
        Ok(())
    }

    fn save_2835(&self, w: &mut emu_core::snap::W) {
        // v2 adds the VEC
        w.tag("vc4-2835-v2");
        for v in &self.hvs { w.u32(*v); }
        for v in self.frcnt { w.u32(v); }
        w.u32(self.eof);
        for pv in &self.pv { for v in pv { w.u32(*v); } }
        for v in &self.win { w.u32(*v); }
        for v in self.hd { w.u32(v); }
        for v in self.txp { w.u32(v); }
        for v in self.next_vblank { w.u64(v); }
        w.bool(self.connected);
        w.u64(self.mai_played_to); w.u64(self.mai_frac); w.u32(self.mai_ch);
        w.bytes(&self.edid);
        for v in &self.gamma { w.u32(*v); }
        w.u32(self.gam_addr);
        w.u64(self.audio_clk_hz); w.u64(self.pixel_hz);
        w.bool(self.cec.tv_on); w.u64(self.cec.rx_at);
        w.u32(self.cec.rx.len() as u32);
        for m in &self.cec.rx { w.bytes(m); }
        for v in &self.vec { w.u32(*v); }
        w.u64(self.vec_hz);
    }
    fn load_2835(&mut self, r: &mut emu_core::snap::R) -> Result<(), emu_core::snap::SnapError> {
        let v2 = match r.bytes()? { b"vc4-2835-v2" => true, b"vc4-2835-v1" => false, t => return Err(emu_core::snap::SnapError(format!("snapshot layout mismatch: expected vc4 state, found {}", String::from_utf8_lossy(t)))) };
        for v in self.hvs.iter_mut() { *v = r.u32()?; }
        for v in self.frcnt.iter_mut() { *v = r.u32()?; }
        self.eof = r.u32()?;
        for pv in self.pv.iter_mut() { for v in pv.iter_mut() { *v = r.u32()?; } }
        for v in self.win.iter_mut() { *v = r.u32()?; }
        for v in self.hd.iter_mut() { *v = r.u32()?; }
        for v in self.txp.iter_mut() { *v = r.u32()?; }
        for v in self.next_vblank.iter_mut() { *v = r.u64()?; }
        self.connected = r.bool()?;
        self.mai_played_to = r.u64()?; self.mai_frac = r.u64()?; self.mai_ch = r.u32()?;
        self.edid = r.bytes()?.to_vec();
        for v in self.gamma.iter_mut() { *v = r.u32()?; }
        self.gam_addr = r.u32()?;
        self.audio_clk_hz = r.u64()?; self.pixel_hz = r.u64()?;
        self.cec.tv_on = r.bool()?; self.cec.rx_at = r.u64()?;
        self.cec.rx.clear();
        for _ in 0..r.u32()? { self.cec.rx.push_back(r.bytes()?.to_vec()); }
        self.vec.fill(0);
        if v2 {
            for v in self.vec.iter_mut() { *v = r.u32()?; }
            self.vec_hz = r.u64()?;
        }
        Ok(())
    }
}

/// A display-list plane, decoded.
#[derive(Default)]
struct Plane {
    ctl0: u32,
    fmt: u32,
    order: u32,
    tiling: u32,
    unity: bool,
    hflip: bool,
    vflip: bool,
    /// position on the screen, size there, size in the source
    x0: usize,
    y0: usize,
    dw: usize,
    dh: usize,
    sw: usize,
    sh: usize,
    planes: usize,
    ptr: [usize; 3],
    pitch: [u32; 3],
    csc: [u32; 3],
    /// the BCM2835's POS2 alpha mode (0 per pixel, 1 fixed), premultiplied, plane alpha mixed in
    alpha_mode: u32,
    premult: bool,
    mix: bool,
    fixed_alpha: u8,
    /// the BCM2711 model's rule: the bottom plane is opaque
    opaque_bottom: bool,
}

/// One RGB pixel of format `fmt` in `order` (the HVS5's meaning: 2 = ARGB, 3 = ABGR, 0 = RGBA,
/// 1 = BGRA from the most significant bits down; XRGB/XBGR for the formats without alpha).
fn rgb_pixel(fmt: u32, order: u32, p: &[u8]) -> (u8, u8, u8, u8) {
    // a field widened to 8 bits: its top bits repeated below it (as the HVS's RGBA_EXPAND_ROUND
    // and the BCM2711 model's 565 decode), or scaled for fields narrower than 4 bits
    let x = |v: u32, bits: u32| -> u8 {
        let v = v & ((1 << bits) - 1);
        if bits >= 4 { ((v << (8 - bits)) | (v >> (2 * bits - 8))) as u8 } else { (v * 255 / ((1 << bits) - 1)) as u8 }
    };
    // a word with four fields of widths [a, r, g, b] in `order`
    let quad = |v: u32, wa: u32, wc: u32| -> (u8, u8, u8, u8) {
        let f = |k: u32| (v >> (k * wc)) & ((1 << wc) - 1); // k-th colour field from the LSB
        match order {
            2 => { let a = v >> (3 * wc); (x(f(2), wc), x(f(1), wc), x(f(0), wc), x(a, wa)) }   // A R G B
            3 => { let a = v >> (3 * wc); (x(f(0), wc), x(f(1), wc), x(f(2), wc), x(a, wa)) }   // A B G R
            0 => { let a = v & ((1 << wa) - 1); let c = v >> wa; (x(c >> (2 * wc), wc), x(c >> wc, wc), x(c, wc), x(a, wa)) } // R G B A
            _ => { let a = v & ((1 << wa) - 1); let c = v >> wa; (x(c, wc), x(c >> wc, wc), x(c >> (2 * wc), wc), x(a, wa)) } // B G R A
        }
    };
    match fmt {
        7 => match order {
            2 => (p[2], p[1], p[0], p[3]), // ARGB/XRGB8888: B, G, R, A in memory
            3 => (p[0], p[1], p[2], p[3]), // ABGR/XBGR8888
            0 => (p[3], p[2], p[1], p[0]),
            _ => (p[1], p[2], p[3], p[0]),
        },
        16 => {
            // XRGB/ARGB2101010 (order ARGB) or XBGR/ABGR2101010: 10 bits a channel,
            // GNOME's choice when the display takes 30-bit colour
            let v = u32::from_le_bytes([p[0], p[1], p[2], p[3]]);
            let (hi, mid, lo) = (((v >> 22) & 255) as u8, ((v >> 12) & 255) as u8, ((v >> 2) & 255) as u8);
            let a = ((v >> 30) * 85) as u8;
            if order == 3 { (lo, mid, hi, a) } else { (hi, mid, lo, a) }
        }
        5 => if order == 3 { (p[0], p[1], p[2], 255) } else { (p[2], p[1], p[0], 255) }, // BGR888 / RGB888
        4 => {
            let v = u16::from_le_bytes([p[0], p[1]]) as u32;
            let (r, g, b) = (x(v >> 11, 5), x(v >> 5, 6), x(v, 5));
            if order == 3 { (b, g, r, 255) } else { (r, g, b, 255) }
        }
        3 => quad(u16::from_le_bytes([p[0], p[1]]) as u32, 1, 5), // 5551
        1 => quad(u16::from_le_bytes([p[0], p[1]]) as u32, 4, 4), // 4444
        2 => { let v = u16::from_le_bytes([p[0], p[1]]) as u32; let (r, g, b) = (x(v >> 10, 5), x(v >> 5, 5), x(v, 5)); if order == 3 { (b, g, r, 255) } else { (r, g, b, 255) } }
        0 => { let v = p[0] as u32; (x(v >> 5, 3), x(v >> 2, 3), x(v, 2), 255) }
        _ => (0, 0, 0, 255),
    }
}

/// Put a source pixel over what is below it, as the plane's alpha mode says.
fn blend(dst: &mut [u8], src: [u8; 3], a: u8, p: &Plane, bottom: bool, has_alpha: bool) {
    if p.opaque_bottom {
        // the BCM2711 model: the bottom plane is opaque; planes above it blend with their
        // per-pixel alpha (the cursor)
        if a == 255 || bottom || !has_alpha { dst.copy_from_slice(&src); return; }
        let a = a as u32;
        for (c, s) in dst.iter_mut().zip(src) { *c = ((s as u32 * a + *c as u32 * (255 - a)) / 255) as u8; }
        return;
    }
    // the BCM2835: fixed alpha, or per-pixel alpha (times the plane's when mixed)
    let mut a = if p.alpha_mode == 1 || !has_alpha { p.fixed_alpha as u32 } else { a as u32 };
    if p.alpha_mode != 1 && p.mix && has_alpha { a = a * p.fixed_alpha as u32 / 255; }
    if a >= 255 { dst.copy_from_slice(&src); return; }
    for (c, s) in dst.iter_mut().zip(src) {
        let s = if p.premult && p.alpha_mode != 1 { s as u32 * 255 } else { s as u32 * a };
        *c = ((s + *c as u32 * (255 - a)) / 255).min(255) as u8;
    }
}

/// The HVS colour-space conversion from a display list's three CSC words: Y offset and gain,
/// chroma offsets, and the Cb/Cr contributions to R, G and B in 2.8 fixed point (the green ones
/// signed).
struct Csc { yofs: i32, yall: i32, cbofs: i32, crofs: i32, cr_r: i32, cb_r: i32, cb_g: i32, cr_g: i32, cb_b: i32, cr_b: i32 }

impl Csc {
    fn from_words(w: [u32; 3]) -> Csc {
        let s10 = |v: u32| { let v = (v & 0x3ff) as i32; if v >= 512 { v - 1024 } else { v } };
        let s8 = |v: u32| (v & 0xff) as u8 as i8 as i32;
        let yall = ((w[1] >> 2) & 0x3ff) as i32;
        if yall == 0 {
            // no conversion words: BT.601 limited range
            return Csc::from_words([0x00f0_0000, 0xe733_04a8, 0x0006_6604]);
        }
        Csc {
            yofs: s8(w[0] >> 16), yall, cbofs: s8(w[0] >> 8), crofs: s8(w[0]),
            cr_b: (((w[0] >> 24) & 0xff) << 2 | (w[1] & 3)) as i32,
            cb_g: s10(w[1] >> 22), cr_g: s10(w[1] >> 12),
            cb_r: ((w[2] >> 20) & 0x3ff) as i32, cr_r: ((w[2] >> 10) & 0x3ff) as i32, cb_b: (w[2] & 0x3ff) as i32,
        }
    }

    fn convert(&self, y: u8, cb: u8, cr: u8) -> [u8; 3] {
        let yy = (y as i32 + self.yofs) * self.yall;
        let (cb, cr) = (cb as i32 - 128 + self.cbofs, cr as i32 - 128 + self.crofs);
        let c = |v: i32| ((v + 128) >> 8).clamp(0, 255) as u8;
        [c(yy + self.cr_r * cr + self.cb_r * cb), c(yy + self.cb_g * cb + self.cr_g * cr), c(yy + self.cb_b * cb + self.cr_b * cr)]
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

    /// A BCM2835 pipeline showing `w`x`h` on HDMI (pixel valve 2, HVS channel 1) with the
    /// display list `dl` at word 16 of the list memory.
    fn bcm2835_with(dl: &[u32], w: u32, h: u32) -> Vc4 {
        let mut v = Vc4::bcm2835(1920, 1080);
        v.pv_write_n(2, PV_CONTROL as u64, 1 | PV_CLK_SELECT_HDMI << 2);
        v.pv_write_n(2, PV_V_CONTROL as u64, 1);
        v.hvs_write(DISPCTRL as u64, 1 << 31);
        v.hvs_write((DISPCTRL0 + 0x10) as u64, 1 << 31 | w << 12 | h);
        for (k, &x) in dl.iter().enumerate() { v.hvs_write((0x2000 + 4 * (16 + k)) as u64, x); }
        v.hvs_write((DISPLIST0 + 4) as u64, 16);
        v.tick(FRAME_NS); // the frame start latches the list
        v
    }

    #[test]
    fn bcm2835_rgb_plane() {
        // XRGB8888 (format 7, order ABGR on this HVS), 2x2 at (1, 0), fixed alpha 0xff, unity
        let ctl0 = 1 << 30 | 7 | 3 << 13 | 1 << 4 | 7 << 24;
        let dl = [ctl0, 0xff << 24 | 1, 1 << 30 | 2 << 16 | 2, 0xc0c0_c0c0, 0xc000_1000, 0xc0c0_c0c0, 8, 1 << 31];
        let v = bcm2835_with(&dl, 4, 2);
        let mut ram = vec![0u8; 0x2000];
        // pixels as B, G, R, X in memory: red, green / blue, white
        ram[0x1000..0x1010].copy_from_slice(&[0, 0, 255, 0, 0, 255, 0, 0, 255, 0, 0, 0, 255, 255, 255, 0]);
        let mut out = Vec::new();
        assert_eq!(v.render(&ram, &mut out), Some((4, 2)));
        let px = |x: usize, y: usize| &out[(y * 4 + x) * 4..(y * 4 + x) * 4 + 3];
        assert_eq!(px(0, 0), [0, 0, 0]);
        assert_eq!(px(1, 0), [255, 0, 0]);
        assert_eq!(px(2, 0), [0, 255, 0]);
        assert_eq!(px(1, 1), [0, 0, 255]);
        assert_eq!(px(2, 1), [255, 255, 255]);
    }

    #[test]
    fn bcm2835_nv12_plane_scaled() {
        // NV12 (format 9, two planes), a 4x2 source scaled to 8x4 at (0, 0): Y = 128, Cb = Cr =
        // 128 is grey; BT.601 limited range
        let ctl0 = 1 << 30 | 9 | 18 << 24;
        let dl = [ctl0, 0xff << 24, 4 << 16 | 8, 1 << 30 | 2 << 16 | 4, 0xc0c0_c0c0, 0xc000_1000, 0xc000_1100, 0xc0c0_c0c0, 0xc0c0_c0c0, 4, 4,
                  0x00f0_0000, 0xe733_04a8, 0x0006_6604, 0, 0, 0, 0, 1 << 31];
        let v = bcm2835_with(&dl, 8, 4);
        let mut ram = vec![0u8; 0x2000];
        ram[0x1000..0x1008].fill(128);
        // chroma: (Cb, Cr) = (128, 128) left, (128, 255) right: red-ish
        ram[0x1100..0x1104].copy_from_slice(&[128, 128, 128, 255]);
        let mut out = Vec::new();
        assert_eq!(v.render(&ram, &mut out), Some((8, 4)));
        let px = |x: usize, y: usize| out[(y * 8 + x) * 4..(y * 8 + x) * 4 + 3].to_vec();
        assert_eq!(px(0, 0), vec![130, 130, 130]);
        assert_eq!(px(1, 3), vec![130, 130, 130]);
        let r = px(7, 0);
        assert!(r[0] > 230 && r[1] < 70 && r[2] == 130, "{r:?}");
    }

    #[test]
    fn bcm2835_frame_rate_from_the_mode() {
        let mut v = Vc4::bcm2835(1920, 1080);
        // 1024x768@60: 1344 x 806 at 65 MHz
        v.pv_write_n(2, PV_HORZA as u64, 160 << 16 | 136);
        v.pv_write_n(2, PV_HORZB as u64, 24 << 16 | 1024);
        v.pv_write_n(2, PV_VERTA as u64, 29 << 16 | 6);
        v.pv_write_n(2, PV_VERTB as u64, 3 << 16 | 768);
        v.pixel_hz = 65_000_000;
        assert_eq!(v.frame_ns(2), 1344 * 806 * 1_000_000_000 / 65_000_000);
        v.pixel_hz = 0;
        assert_eq!(v.frame_ns(2), FRAME_NS);
    }

    #[test]
    fn composite_pal_on_the_vec() {
        // what vc4_crtc.c and vc4_vec.c write for 720x576i (PAL): 864 x 625 lines, two fields
        // of 313 and 312 lines at the VEC's 13.5 MHz
        let mut v = bcm2835_with(&[1 << 31], 720, 576);
        v.pv_write_n(2, PV_CONTROL as u64, 1 | PV_CLK_SELECT_VEC << 2);
        v.pv_write_n(2, PV_V_CONTROL as u64, 1 | PV_VCONTROL_INTERLACE);
        v.pv_write_n(2, PV_HORZA as u64, 69 << 16 | 63);
        v.pv_write_n(2, PV_HORZB as u64, 12 << 16 | 720);
        v.pv_write_n(2, PV_VERTA as u64, 20 << 16 | 3);
        v.pv_write_n(2, PV_VERTB as u64, 2 << 16 | 288);
        v.pv_write_n(2, PV_VERTA_EVEN as u64, 19 << 16 | 3);
        v.pv_write_n(2, PV_VERTB_EVEN as u64, 2 << 16 | 288);
        // the VEC is off: nothing is shown
        assert_eq!(v.output(), None);
        assert_eq!(v.frame_ns(2), 20_000_000);
        v.vec_write(VEC_CONFIG0 as u64, 1);
        v.vec_write(VEC_CONFIG1 as u64, 7 << 10);
        v.vec_write(VEC_CFG as u64, VEC_CFG_VEC_EN);
        assert_eq!((v.output(), v.size(), v.tv_norm()), (Some(Output::Composite), Some((720, 576)), Some("PAL")));
        // PAL-60 and NTSC (59.94 fields a second: 858 x 525 / 2 at 13.5 MHz)
        v.vec_write(VEC_CONFIG0 as u64, 2);
        v.vec_write(VEC_CONFIG1 as u64, 7 << 10 | 1);
        assert_eq!(v.tv_norm(), Some("PAL-60"));
        v.vec_write(VEC_CONFIG0 as u64, 0x40);
        v.vec_write(VEC_CONFIG1 as u64, 7 << 10);
        assert_eq!(v.tv_norm(), Some("NTSC"));
        v.pv_write_n(2, PV_HORZA as u64, 57 << 16 | 62);
        v.pv_write_n(2, PV_HORZB as u64, 19 << 16 | 720);
        v.pv_write_n(2, PV_VERTA as u64, 15 << 16 | 3);
        v.pv_write_n(2, PV_VERTB as u64, 4 << 16 | 240);
        v.pv_write_n(2, PV_VERTA_EVEN as u64, 15 << 16 | 3);
        v.pv_write_n(2, PV_VERTB_EVEN as u64, 5 << 16 | 240);
        assert_eq!(v.frame_ns(2), 858 * 525 * 1_000_000_000 / (2 * 13_500_000));
        // the background colour, then monochrome (no chroma): grey
        v.hvs_write((0x44 + 0x10) as u64, 1 << 24 | 0x00ff_0000);
        let mut out = Vec::new();
        v.render(&[], &mut out).unwrap();
        assert_eq!(&out[..3], &[255, 0, 0]);
        v.vec_write(VEC_CONFIG0 as u64, 1 | VEC_CONFIG0_CHRDIS | 1 << 8);
        v.render(&[], &mut out).unwrap();
        assert_eq!((&out[..3], v.tv_norm()), (&[76u8, 76, 76][..], Some("monochrome")));
        // a snapshot keeps the VEC
        let mut w = emu_core::snap::W::default();
        v.save_state(&mut w);
        let mut u = Vc4::bcm2835(1920, 1080);
        u.load_state(&mut emu_core::snap::R::new(&w.0)).unwrap();
        assert_eq!((u.output(), u.tv_norm()), (Some(Output::Composite), Some("monochrome")));
        // back to HDMI
        v.pv_write_n(2, PV_CONTROL as u64, 1 | PV_CLK_SELECT_HDMI << 2);
        assert_eq!(v.output(), Some(Output::Hdmi));
    }

    #[test]
    fn cec_tv_answers() {
        let mut v = Vc4::bcm2835(1920, 1080);
        let send = |v: &mut Vc4, msg: &[u8]| {
            for (i, b) in msg.iter().enumerate() {
                let o = H_CEC_TX_DATA_1 + 4 * (i / 4);
                let w = v.hdmi_read(o as u64) & !(0xff << (8 * (i % 4))) | (*b as u32) << (8 * (i % 4));
                v.hdmi_write(o as u64, w);
            }
            let c1 = v.hdmi_read(H_CEC_CNTRL_1 as u64) & !(CEC_START_XMIT_BEGIN | 0xf << 16);
            v.hdmi_write(H_CEC_CNTRL_1 as u64, c1 | (msg.len() as u32 - 1) << 16 | CEC_START_XMIT_BEGIN);
            let good = v.hdmi_read(H_CEC_CNTRL_1 as u64) & CEC_TX_STATUS_GOOD != 0;
            // the driver's interrupt handler
            assert!(v.hdmi_read(H_CPU_STATUS as u64) & CPU_CEC != 0);
            let c1 = v.hdmi_read(H_CEC_CNTRL_1 as u64);
            v.hdmi_write(H_CEC_CNTRL_1 as u64, c1 & !CEC_START_XMIT_BEGIN);
            v.hdmi_write(H_CPU_CLEAR as u64, CPU_CEC);
            good
        };
        v.hdmi_write(H_CPU_MASK_CLEAR as u64, CPU_CEC);
        assert!(send(&mut v, &[0x40]), "the TV acknowledges a poll");
        assert!(!send(&mut v, &[0x44]), "logical address 4 is free");
        assert!(send(&mut v, &[0x40, 0x46]), "Give OSD Name");
        v.tick(10_000_000);
        assert!(v.irq_hdmi());
        assert_ne!(v.hdmi_read(H_CEC_CNTRL_5 as u64) & CEC_RX_CEC_INT, 0);
        let c1 = v.hdmi_read(H_CEC_CNTRL_1 as u64);
        let len = ((c1 >> 24) & 0xf) as usize + 1;
        let bytes: Vec<u8> = (0..len).map(|i| (v.hdmi_read((H_CEC_RX_DATA_1 + 4 * (i / 4)) as u64) >> (8 * (i % 4))) as u8).collect();
        assert_eq!(bytes, [&[0x04, 0x47][..], TV_NAME].concat());
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
