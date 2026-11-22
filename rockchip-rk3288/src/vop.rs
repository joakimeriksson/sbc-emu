//! RK3288 Visual Output Processor (VOP, `rockchip,rk3288-vop`): the display controller in
//! front of DSI, HDMI, eDP and LVDS. The RK3288 has two, VOP-big (0xff930000) and
//! VOP-little (0xff940000); each has its own IOMMU at +0x300 (`rockchip,iommu`, the Mali MMU
//! design) sharing the VOP's interrupt, and a gamma LUT at +0x1000.
//!
//! Modelled: the register file as Linux's `rockchip_drm_vop` programs it (`rk3288_vop` in
//! `rockchip_vop_reg.c`), the frame-start (vblank) and display-hold interrupts, and scanout
//! of the two full windows (win0, win1) in ARGB8888/RGB888/RGB565 through the IOMMU.
//! Registers take effect at once; the hardware latches them at the next frame start after
//! REG_CFG_DONE, which a guest cannot tell apart at frame granularity. Scaling, YUV, the
//! win2/win3 overlays and the gamma LUT are not applied.
//!
//! The timing generator runs (frame-start interrupts, scanout) once the display timings
//! are programmed. Requesting standby raises DSP_HOLD_VALID, which Linux waits for before it
//! gates the clocks; the standby bit itself does not stop the model's output. A kernel that
//! takes over a display the boot loader set up for its splash may never write the
//! bit clear, so without that U-Boot it stays set from the VOP's reset state.

use mali400::{mmu::Mmu, Memory};

/// Register file size (`reg` = 0x19c) and the IOMMU and LUT offsets inside the 64 KiB slot.
pub const VOP_SIZE: u64 = 0x2000;
const REGS: usize = 0x200 / 4;
const MMU_OFFSET: u64 = 0x300;
const MMU_SIZE: u64 = 0x100;
const LUT_OFFSET: u64 = 0x1000;

const SYS_CTRL: usize = 0x08 / 4;
const INTR_CTRL0: usize = 0x20 / 4;
const DSP_HTOTAL_HS_END: usize = 0x188 / 4;
const DSP_HACT_ST_END: usize = 0x18c / 4;
const DSP_VTOTAL_VS_END: usize = 0x190 / 4;
const DSP_VACT_ST_END: usize = 0x194 / 4;
const VERSION_INFO: usize = 1; // 0x04
const DSP_BG: usize = 0x18 / 4;

const STANDBY: u32 = 1 << 22;
/// Interrupt sources (INTR_CTRL0 bits 0-3 status, 4-7 enable, 8-11 clear).
pub const DSP_HOLD_VALID: u32 = 1 << 0;
pub const FRAME_START: u32 = 1 << 1;
pub const LINE_FLAG: u32 = 1 << 2;

/// Full windows: win0 at 0x30, win1 at 0x70.
const WIN_BASES: [usize; 2] = [0x30 / 4, 0x70 / 4];
const WIN_CTRL0: usize = 0;
const WIN_VIR: usize = 3;
const WIN_YRGB_MST: usize = 4;
const WIN_ACT_INFO: usize = 6;
const WIN_DSP_ST: usize = 8;

/// Frame period in 24 MHz ticks: 60 Hz (the pixel clock comes from the CRU's DCLK, which
/// Linux sets for the mode's refresh rate).
const FRAME_TICKS: u64 = 24_000_000 / 60;

/// Largest picture rendered (the VOP-big's maximum output).
const MAX_SIZE: u32 = 4096;

pub struct Vop {
    regs: [u32; REGS],
    lut: Vec<u32>,
    pub mmu: Mmu,
    status: u32,
    /// Tick of the next frame start while the VOP is running.
    next_frame: Option<u64>,
    /// Frames scanned out (frame-start events).
    pub frames: u64,
    /// Print register writes to stderr (`SBC_VOP_LOG=1`).
    pub log: bool,
}

impl Default for Vop {
    fn default() -> Self {
        let mut regs = [0; REGS];
        // VERSION_INFO of the RK3288's VOP: major 3, minor 1 (VOP_VERSION(3, 1)).
        regs[VERSION_INFO] = 0x0301_0000;
        regs[SYS_CTRL] = STANDBY;
        Self { regs, lut: vec![0; 1024], mmu: Mmu::default(), status: 0, next_frame: None, frames: 0, log: std::env::var_os("SBC_VOP_LOG").is_some() }
    }
}

impl Vop {
    pub fn read(&self, offset: u64) -> u32 {
        match offset {
            o if (MMU_OFFSET..MMU_OFFSET + MMU_SIZE).contains(&o) => self.mmu.read(o - MMU_OFFSET),
            o if o >= LUT_OFFSET => self.lut.get(((o - LUT_OFFSET) / 4) as usize).copied().unwrap_or(0),
            o if (o as usize) / 4 == INTR_CTRL0 => self.regs[INTR_CTRL0] & !0xf0f | self.status,
            o => self.regs.get(o as usize / 4).copied().unwrap_or(0),
        }
    }

    pub fn write(&mut self, offset: u64, value: u32, now: u64) {
        if self.log {
            eprintln!("[vop] write {offset:#05x} = {value:#010x}");
        }
        match offset {
            o if (MMU_OFFSET..MMU_OFFSET + MMU_SIZE).contains(&o) => self.mmu.write(o - MMU_OFFSET, value),
            o if o >= LUT_OFFSET => {
                if let Some(entry) = self.lut.get_mut(((o - LUT_OFFSET) / 4) as usize) {
                    *entry = value;
                }
            }
            o => {
                let index = o as usize / 4;
                if index >= REGS {
                    return;
                }
                if index == INTR_CTRL0 {
                    // Clear bits acknowledge; they are not stored as state.
                    self.status &= !(value >> 8 & 0xf);
                    self.regs[INTR_CTRL0] = value & !0xf0f;
                    return;
                }
                self.regs[index] = value;
                if index == SYS_CTRL && self.standby() {
                    // The VOP finishes the frame and holds; it reports that on every standby
                    // request, including one made while the bit was already set (Linux's
                    // CRTC disable sets it again and waits for the hold).
                    self.raise(DSP_HOLD_VALID);
                }
                if self.next_frame.is_none() && self.timing_programmed() {
                    self.next_frame = Some(now + FRAME_TICKS);
                }
            }
        }
    }

    fn timing_programmed(&self) -> bool {
        self.regs[DSP_HTOTAL_HS_END] >> 16 != 0 && self.regs[DSP_VTOTAL_VS_END] >> 16 != 0
    }

    fn standby(&self) -> bool {
        self.regs[SYS_CTRL] & STANDBY != 0
    }

    /// Raise frame-start interrupts that are due at `now`.
    pub fn update(&mut self, now: u64) {
        if let Some(due) = self.next_frame {
            if now >= due {
                self.frames += 1;
                // A line-flag interrupt fires every frame at the programmed line.
                self.raise(FRAME_START | LINE_FLAG);
                let missed = (now - due) / FRAME_TICKS;
                self.next_frame = Some(due + (missed + 1) * FRAME_TICKS);
            }
        }
    }

    /// Latch interrupt sources. The RK3288's INTR_CTRL0 reports enabled sources only: Linux
    /// polls the frame-start status in `vop_wait_for_irq_handler` while that interrupt may be
    /// disabled, and a status bit no handler will clear stalls it ("VOP vblank IRQ stuck").
    fn raise(&mut self, sources: u32) {
        self.status |= sources & self.regs[INTR_CTRL0] >> 4 & 0xf;
    }

    pub fn next_deadline(&self) -> Option<u64> {
        self.next_frame
    }

    pub fn irq(&self) -> bool {
        self.status & (self.regs[INTR_CTRL0] >> 4 & 0xf) != 0 || self.mmu.irq()
    }

    /// Active display size, or None while the timings are unprogrammed or no window is on.
    pub fn shown_size(&self) -> Option<(u32, u32)> {
        if !self.timing_programmed() || WIN_BASES.iter().all(|&b| self.regs[b + WIN_CTRL0] & 1 == 0) {
            return None;
        }
        let span = |r: u32| (r & 0x1fff).saturating_sub(r >> 16 & 0x1fff);
        let (w, h) = (span(self.regs[DSP_HACT_ST_END]), span(self.regs[DSP_VACT_ST_END]));
        (w > 0 && h > 0).then_some((w.min(MAX_SIZE), h.min(MAX_SIZE)))
    }

    /// Compose win0 and win1 over the background colour as RGBA, turned by `rotation`
    /// degrees counter-clockwise (0, 90, 180 or 270; anything else is drawn unturned), the
    /// way a panel mounted that way shows it. Returns the size of the turned picture.
    pub fn render(&self, ram: &[u8], ram_base: u64, rotation: u32, out: &mut Vec<u8>) -> Option<(u32, u32)> {
        let (w, h) = self.shown_size()?;
        let rotation = match rotation % 360 {
            r @ (90 | 180 | 270) => r,
            _ => 0,
        };
        let (rw, rh) = if rotation % 180 == 90 { (h, w) } else { (w, h) };
        // Byte offset in `out` of the source pixel (x, y).
        let at = |x: u32, y: u32| -> usize {
            let (dx, dy) = match rotation {
                90 => (y, w - 1 - x),
                180 => (w - 1 - x, h - 1 - y),
                270 => (h - 1 - y, x),
                _ => (x, y),
            };
            ((dy * rw + dx) * 4) as usize
        };
        // DSP_BG: 10 bits per channel, blue 9:0, green 19:10, red 29:20.
        let bg = self.regs[DSP_BG];
        let bg = [(bg >> 22) as u8, (bg >> 12) as u8, (bg >> 2) as u8, 0xff];
        out.clear();
        out.resize((w * h * 4) as usize, 0);
        for px in out.chunks_exact_mut(4) {
            px.copy_from_slice(&bg);
        }
        let mem = RamView { ram, base: ram_base };
        let hact_st = self.regs[DSP_HACT_ST_END] >> 16 & 0x1fff;
        let vact_st = self.regs[DSP_VACT_ST_END] >> 16 & 0x1fff;
        for &base in &WIN_BASES {
            let ctrl = self.regs[base + WIN_CTRL0];
            if ctrl & 1 == 0 {
                continue;
            }
            let (format, rb_swap) = (ctrl >> 1 & 7, ctrl & 1 << 12 != 0);
            let bpp = match format {
                0 => 4,
                1 => 3,
                2 => 2,
                _ => continue, // YUV formats: not rendered
            };
            let act = self.regs[base + WIN_ACT_INFO];
            let (sw, sh) = ((act & 0x1fff) + 1, (act >> 16 & 0x1fff) + 1);
            let st = self.regs[base + WIN_DSP_ST];
            let x0 = (st & 0x1fff).saturating_sub(hact_st);
            let y0 = (st >> 16 & 0x1fff).saturating_sub(vact_st);
            let stride = (self.regs[base + WIN_VIR] & 0x3fff) as u64 * 4;
            let mst = self.regs[base + WIN_YRGB_MST];
            let mut page = (u32::MAX, None);
            for y in 0..sh.min(h.saturating_sub(y0)) {
                for x in 0..sw.min(w.saturating_sub(x0)) {
                    let va = mst.wrapping_add((y as u64 * stride + x as u64 * bpp) as u32);
                    if page.0 != va >> 12 {
                        page = (va >> 12, self.mmu.lookup(&mem, va & !0xfff, false));
                    }
                    let Some(pa) = page.1.map(|p| (p | va & 0xfff) as u64) else { continue };
                    let Some(i) = pa.checked_sub(ram_base).map(|i| i as usize) else { continue };
                    let Some(px) = ram.get(i..i + bpp as usize) else { continue };
                    let (mut r, g, mut b) = match format {
                        0 | 1 => (px[2], px[1], px[0]),
                        _ => {
                            let v = u16::from_le_bytes([px[0], px[1]]) as u32;
                            (((v >> 11 & 0x1f) * 255 / 31) as u8, ((v >> 5 & 0x3f) * 255 / 63) as u8, ((v & 0x1f) * 255 / 31) as u8)
                        }
                    };
                    if rb_swap {
                        std::mem::swap(&mut r, &mut b);
                    }
                    let d = at(x0 + x, y0 + y);
                    out[d..d + 4].copy_from_slice(&[r, g, b, 0xff]);
                }
            }
        }
        Some((rw, rh))
    }
}

/// DRAM as the IOMMU's page-table walker reads it.
struct RamView<'a> {
    ram: &'a [u8],
    base: u64,
}

impl Memory for RamView<'_> {
    fn read_u32(&self, pa: u32) -> Option<u32> {
        let i = (pa as u64).checked_sub(self.base)? as usize;
        Some(u32::from_le_bytes(self.ram.get(i..i.checked_add(4)?)?.try_into().unwrap()))
    }
    fn write_u32(&mut self, _pa: u32, _value: u32) -> bool {
        false
    }
}

emu_core::snap_struct!(Vop { regs, mmu, status, next_frame, frames } fixed { lut } skip { log });

#[cfg(test)]
mod tests {
    use super::*;

    fn programmed(vop: &mut Vop) {
        // 4x2 active area starting at (10, 3), win0 ARGB8888 at 0x1000, stride 4 pixels.
        vop.write(0x188, 20 << 16 | 2, 0);
        vop.write(0x18c, 10 << 16 | 14, 0);
        vop.write(0x190, 8 << 16 | 1, 0);
        vop.write(0x194, 3 << 16 | 5, 0);
        vop.write(0x30, 1, 0);
        vop.write(0x3c, 4, 0);
        vop.write(0x40, 0x1000, 0);
        vop.write(0x48, 1 << 16 | 3, 0);
        vop.write(0x50, 3 << 16 | 10, 0);
        vop.write(0x08, 0, 0);
    }

    #[test]
    fn disabled_sources_do_not_latch() {
        let mut vop = Vop::default();
        programmed(&mut vop);
        let due = vop.next_deadline().unwrap();
        vop.update(due);
        assert_eq!(vop.read(0x20) & 0xf, 0, "frame start passed with its interrupt off");
        vop.write(0x20, FRAME_START << 4, due);
        assert!(!vop.irq(), "enabling does not report an old frame");
        vop.update(due + FRAME_TICKS);
        assert!(vop.irq());
    }

    #[test]
    fn frame_start_interrupts_while_running_and_hold_on_standby() {
        let mut vop = Vop::default();
        assert!(vop.shown_size().is_none());
        vop.write(0x20, FRAME_START << 4 | DSP_HOLD_VALID << 4, 0);
        programmed(&mut vop);
        assert_eq!(vop.shown_size(), Some((4, 2)));
        let due = vop.next_deadline().unwrap();
        vop.update(due - 1);
        assert!(!vop.irq());
        vop.update(due);
        assert!(vop.irq());
        assert_ne!(vop.read(0x20) & FRAME_START, 0);
        vop.write(0x20, FRAME_START << 8 | (FRAME_START | DSP_HOLD_VALID) << 4, due);
        assert!(!vop.irq());
        vop.write(0x08, STANDBY, due);
        assert_ne!(vop.read(0x20) & DSP_HOLD_VALID, 0);
        // A standby request while already in standby (a kernel may never clear it)
        // is acknowledged too.
        vop.write(0x20, DSP_HOLD_VALID << 8 | DSP_HOLD_VALID << 4, due);
        assert!(!vop.irq());
        vop.write(0x08, STANDBY, due);
        assert!(vop.irq());
    }

    #[test]
    fn renders_win0_through_the_iommu() {
        let mut ram = vec![0u8; 0x10000];
        // Directory at 0x8000 -> table at 0x9000 -> VA page 0x1000 maps to PA 0x3000.
        ram[0x8000..0x8004].copy_from_slice(&(0x9000u32 | 1).to_le_bytes());
        ram[0x9004..0x9008].copy_from_slice(&(0x3000u32 | 7).to_le_bytes());
        for (i, px) in [0x00ff_0000u32, 0x0000_ff00, 0x0000_00ff, 0x00ff_ffff].iter().enumerate() {
            ram[0x3000 + i * 4..0x3004 + i * 4].copy_from_slice(&px.to_le_bytes());
        }
        let mut vop = Vop::default();
        programmed(&mut vop);
        vop.write(0x300, 0x8000, 0);
        vop.write(0x308, 0, 0); // enable paging
        let mut out = Vec::new();
        assert_eq!(vop.render(&ram, 0, 0, &mut out), Some((4, 2)));
        assert_eq!(&out[0..16], &[255, 0, 0, 255, 0, 255, 0, 255, 0, 0, 255, 255, 255, 255, 255, 255]);
        // Mounted at 270 degrees the picture is 2x4: source row 0 becomes the right column.
        vop.write(0x18, 0x3ff << 20, 0); // red background (10 bits per channel)
        vop.write(0x48, 3, 0); // win0 is one row: source row 1 shows the background
        assert_eq!(vop.render(&ram, 0, 270, &mut out), Some((2, 4)));
        assert_eq!(&out[4..8], &[255, 0, 0, 255]);
        assert_eq!(&out[12..16], &[0, 255, 0, 255]);
        assert_eq!(&out[0..4], &[255, 0, 0, 255]); // source (0, 1): the background
        // A non-right-angle rotation is drawn unturned instead of indexing out of bounds.
        assert_eq!(vop.render(&ram, 0, 45, &mut out), Some((4, 2)));
    }
}
