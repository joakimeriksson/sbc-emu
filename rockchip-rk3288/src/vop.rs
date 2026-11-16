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
//! are programmed. Entering standby raises DSP_HOLD_VALID, which mainline Linux waits for
//! before it gates the clocks; the standby bit itself does not stop the model's output,
//! because the panel board's kernel drives its DSI panel with the bit still set from the
//! VOP's reset state.

use mali400::mmu::Mmu;

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

/// Largest picture rendered (the VOP-big's maximum output).
const MAX_SIZE: u32 = 4096;

pub struct Vop {
    regs: [u32; REGS],
    lut: Vec<u32>,
    pub mmu: Mmu,
    status: u32,
    /// Tick of the next frame start while the VOP is running.
    next_frame: Option<u64>,
    /// Frame period in 24 MHz ticks.
    frame_ticks: u64,
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
        Self { regs, lut: vec![0; 1024], mmu: Mmu::default(), status: 0, next_frame: None, frame_ticks: 400_000, frames: 0, log: std::env::var_os("SBC_VOP_LOG").is_some() }
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
                let was_standby = self.standby();
                self.regs[index] = value;
                if index == SYS_CTRL && !was_standby && self.standby() {
                    // The VOP finishes the frame and holds.
                    self.status |= DSP_HOLD_VALID;
                }
                if self.next_frame.is_none() && self.timing_programmed() {
                    self.next_frame = Some(now + self.frame_period());
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

    /// Frame period from the programmed timing at 60 Hz refresh (the pixel clock comes from
    /// the CRU's DCLK, which Linux sets for the mode's refresh rate).
    fn frame_period(&mut self) -> u64 {
        self.frame_ticks = 24_000_000 / 60;
        self.frame_ticks
    }

    /// Raise frame-start interrupts that are due at `now`.
    pub fn update(&mut self, now: u64) {
        if let Some(due) = self.next_frame {
            if now >= due {
                self.frames += 1;
                self.status |= FRAME_START;
                // A line-flag interrupt fires every frame at the programmed line.
                self.status |= LINE_FLAG;
                let missed = (now - due) / self.frame_ticks;
                self.next_frame = Some(due + (missed + 1) * self.frame_ticks);
            }
        }
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

    /// Translate a bus address through the IOMMU (identity while paging is off).
    fn translate(&self, ram: &[u8], ram_base: u64, va: u32) -> Option<u64> {
        if self.mmu.read(0x04) & 1 == 0 {
            return Some(va as u64);
        }
        let word = |pa: u64| -> u32 {
            pa.checked_sub(ram_base)
                .and_then(|i| ram.get(i as usize..i as usize + 4))
                .map_or(0, |b| u32::from_le_bytes(b.try_into().unwrap()))
        };
        let dte = self.mmu.read(0x00) as u64;
        let pde = word(dte + (va as u64 >> 22) * 4);
        if pde & 1 == 0 {
            return None;
        }
        let pte = word((pde & !0xfff) as u64 + (va as u64 >> 12 & 0x3ff) * 4);
        (pte & 1 != 0).then_some((pte & !0xfff) as u64 | (va & 0xfff) as u64)
    }

    /// Compose win0 and win1 over the background colour as RGBA.
    pub fn render(&self, ram: &[u8], ram_base: u64, out: &mut Vec<u8>) -> Option<(u32, u32)> {
        let (w, h) = self.shown_size()?;
        let bg = self.regs[DSP_BG];
        let mut frame = vec![bg & 0xff_ffff; (w * h) as usize];
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
                        page = (va >> 12, self.translate(ram, ram_base, va & !0xfff));
                    }
                    let Some(pa) = page.1.map(|p| p | (va & 0xfff) as u64) else { continue };
                    let Some(i) = pa.checked_sub(ram_base).map(|i| i as usize) else { continue };
                    let Some(px) = ram.get(i..i + bpp as usize) else { continue };
                    let (mut r, g, mut b) = match format {
                        0 | 1 => (px[2] as u32, px[1] as u32, px[0] as u32),
                        _ => {
                            let v = u16::from_le_bytes([px[0], px[1]]) as u32;
                            ((v >> 11 & 0x1f) * 255 / 31, (v >> 5 & 0x3f) * 255 / 63, (v & 0x1f) * 255 / 31)
                        }
                    };
                    if rb_swap {
                        std::mem::swap(&mut r, &mut b);
                    }
                    frame[((y0 + y) * w + x0 + x) as usize] = r << 16 | g << 8 | b;
                }
            }
        }
        out.clear();
        out.reserve(frame.len() * 4);
        for p in frame {
            out.extend_from_slice(&[(p >> 16) as u8, (p >> 8) as u8, p as u8, 0xff]);
        }
        Some((w, h))
    }
}

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
        assert_eq!(vop.render(&ram, 0, &mut out), Some((4, 2)));
        assert_eq!(&out[0..16], &[255, 0, 0, 255, 0, 255, 0, 255, 0, 0, 255, 255, 255, 255, 255, 255]);
    }
}
