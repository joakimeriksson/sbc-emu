//! The RK3288's RGA (raster graphics accelerator, 0xff920000) as Linux's `rockchip-rga`
//! V4L2 mem2mem driver uses it: the vendor's application draws its UI in software and has the
//! RGA rotate each frame onto the panel, which is mounted sideways.
//!
//! The driver writes a 32-word command buffer (the image of registers 0x100-0x17c) to
//! memory, points RGA_CMD_BASE at it and starts it with RGA_CMD_CTRL. The model runs the
//! bit-blit at once: source to destination with rotation (the driver gives the destination
//! address of the corner the first source pixel lands on), mirroring, nearest-neighbour
//! scaling and RGB format conversion. Buffers go through the RGA's MMU: a table of 32-bit
//! page addresses per buffer (RGA_MMU_*_BASE, shifted by 4). Completion raises the "all
//! commands done" interrupt (RGA_INT bit 2, enabled by bit 10, cleared by writing bit 6).

pub const RGA_SIZE: u64 = 0x1000;

const SYS_CTRL: usize = 0x000;
const CMD_CTRL: usize = 0x004;
const CMD_BASE: usize = 0x008;
const INT: usize = 0x010;
const VERSION_INFO: usize = 0x028;
const MODE_BASE: usize = 0x100;

const SRC_INFO: usize = 0x104;
const SRC_Y_RGB: usize = 0x108;
const SRC_VIR_INFO: usize = 0x118;
const SRC_ACT_INFO: usize = 0x11c;
const DST_INFO: usize = 0x138;
const DST_Y_RGB: usize = 0x13c;
const DST_VIR_INFO: usize = 0x148;
const DST_ACT_INFO: usize = 0x14c;
const MODE_CTRL: usize = 0x100;
const MMU_CTRL1: usize = 0x16c;
const MMU_SRC_BASE: usize = 0x170;
const MMU_DST_BASE: usize = 0x178;

/// Version 2.1 (nonzero: the driver's workaround for version 0 shrinks rotated sources).
const VERSION: u32 = 2 << 24 | 1 << 20;

pub struct Rga {
    regs: Vec<u32>,
    /// Blits run, for the end-of-run report.
    pub jobs: u64,
    pub log: bool,
}

impl Default for Rga {
    fn default() -> Self {
        Self { regs: vec![0; RGA_SIZE as usize / 4], jobs: 0, log: std::env::var("SBC_RGA_LOG").is_ok_and(|v| v != "0") }
    }
}

fn read_u32(ram: &[u8], base: u64, pa: u64) -> Option<u32> {
    let i = usize::try_from(pa.checked_sub(base)?).ok()?;
    Some(u32::from_le_bytes(ram.get(i..i.checked_add(4)?)?.try_into().unwrap()))
}

/// A buffer seen through the RGA MMU (or physical when the MMU is off for it).
struct Buf {
    table: Option<u64>,
}

impl Buf {
    fn pa(&self, ram: &[u8], base: u64, va: u64) -> Option<u64> {
        match self.table {
            Some(t) => Some(read_u32(ram, base, t + 4 * (va >> 12))? as u64 | (va & 0xfff)),
            None => Some(va),
        }
    }
}

/// Bytes per pixel and RGBA decode/encode of the RGB formats (Rockchip names list the
/// channels from the most significant end; memory is little-endian).
fn bpp(format: u32) -> Option<u32> {
    match format {
        0 | 1 => Some(4),
        2 => Some(3),
        4..=6 => Some(2),
        _ => None,
    }
}

fn decode(format: u32, swap: u32, b: &[u8]) -> [u8; 4] {
    let mut c = match format {
        0 => [b[0], b[1], b[2], b[3]],
        1 => [b[0], b[1], b[2], 0xff],
        2 => [b[0], b[1], b[2], 0xff],
        4 => {
            let v = u16::from_le_bytes([b[0], b[1]]) as u32;
            [((v & 31) * 255 / 31) as u8, ((v >> 5 & 63) * 255 / 63) as u8, ((v >> 11) * 255 / 31) as u8, 0xff]
        }
        5 => {
            let v = u16::from_le_bytes([b[0], b[1]]) as u32;
            [((v & 31) * 255 / 31) as u8, ((v >> 5 & 31) * 255 / 31) as u8, ((v >> 10 & 31) * 255 / 31) as u8, if v >> 15 != 0 { 255 } else { 0 }]
        }
        _ => {
            let v = u16::from_le_bytes([b[0], b[1]]) as u32;
            [((v & 15) * 17) as u8, ((v >> 4 & 15) * 17) as u8, ((v >> 8 & 15) * 17) as u8, ((v >> 12) * 17) as u8]
        }
    };
    if swap & 1 != 0 {
        c.swap(0, 2);
    }
    if swap & 2 != 0 {
        c = [c[3], c[0], c[1], c[2]];
    }
    c
}

fn encode(format: u32, swap: u32, c: [u8; 4], out: &mut [u8; 4]) {
    let mut c = c;
    if swap & 2 != 0 {
        c = [c[1], c[2], c[3], c[0]];
    }
    if swap & 1 != 0 {
        c.swap(0, 2);
    }
    match format {
        0 | 1 => *out = [c[0], c[1], c[2], if format == 1 { 0xff } else { c[3] }],
        2 => out[..3].copy_from_slice(&c[..3]),
        4 => {
            let v = (c[0] as u16 >> 3) | (c[1] as u16 >> 2) << 5 | (c[2] as u16 >> 3) << 11;
            out[..2].copy_from_slice(&v.to_le_bytes());
        }
        5 => {
            let v = (c[0] as u16 >> 3) | (c[1] as u16 >> 3) << 5 | (c[2] as u16 >> 3) << 10 | ((c[3] >= 128) as u16) << 15;
            out[..2].copy_from_slice(&v.to_le_bytes());
        }
        _ => {
            let v = (c[0] as u16 >> 4) | (c[1] as u16 >> 4) << 4 | (c[2] as u16 >> 4) << 8 | (c[3] as u16 >> 4) << 12;
            out[..2].copy_from_slice(&v.to_le_bytes());
        }
    }
}

impl Rga {
    pub fn read(&self, offset: u64) -> u32 {
        let o = offset as usize;
        match o {
            VERSION_INFO => VERSION,
            CMD_CTRL => 0, // never busy
            _ => self.regs.get(o / 4).copied().unwrap_or(0),
        }
    }

    pub fn write(&mut self, offset: u64, value: u32, ram: &mut [u8], ram_base: u64) {
        let o = offset as usize;
        if o / 4 >= self.regs.len() {
            return;
        }
        match o {
            INT => {
                // bits 4-7 clear the status bits 0-3; bits 8-11 enable them
                let status = self.regs[INT / 4] & 0xf & !((value >> 4) & 0xf);
                self.regs[INT / 4] = (value & 0xf00) | status;
            }
            CMD_CTRL => {
                if value & 1 != 0 {
                    self.run(ram, ram_base);
                }
            }
            _ => self.regs[o / 4] = value,
        }
    }

    pub fn irq(&self) -> bool {
        let r = self.regs[INT / 4];
        (r & 0xf) & (r >> 8 & 0xf) != 0
    }

    fn r(&self, reg: usize) -> u32 {
        self.regs[reg / 4]
    }

    fn run(&mut self, ram: &mut [u8], ram_base: u64) {
        let cmd = self.r(CMD_BASE) as u64;
        for k in 0..32 {
            let v = read_u32(ram, ram_base, cmd + 4 * k).unwrap_or(0);
            self.regs[MODE_BASE / 4 + k as usize] = v;
        }
        self.jobs += 1;
        if self.r(MODE_CTRL) & 7 == 0 {
            self.bitblt(ram, ram_base);
        } else if self.log {
            eprintln!("[rga] render mode {} not modelled", self.r(MODE_CTRL) & 7);
        }
        let _ = self.r(SYS_CTRL);
        // all commands done
        self.regs[INT / 4] |= 0x4;
    }

    fn bitblt(&mut self, ram: &mut [u8], ram_base: u64) {
        let si = self.r(SRC_INFO);
        let di = self.r(DST_INFO);
        let (sfmt, sswap) = (si & 15, si >> 4 & 7);
        let (dfmt, dswap) = (di & 15, di >> 4 & 7);
        let rot = si >> 10 & 3;
        let mir = si >> 12 & 3;
        let (Some(sb), Some(db)) = (bpp(sfmt), bpp(dfmt)) else {
            if self.log {
                eprintln!("[rga] format {sfmt} -> {dfmt} not modelled (YUV)");
            }
            return;
        };
        // The driver writes the source stride (in words) to both vir_width (15 bits) and
        // the 10-bit vir_stride, which cannot hold a 1280-pixel ARGB row; use vir_width.
        let sstride = (self.r(SRC_VIR_INFO) & 0x7fff) as u64 * 4;
        let dstride = (self.r(DST_VIR_INFO) & 0x7fff) as u64 * 4;
        let sact = self.r(SRC_ACT_INFO);
        let dact = self.r(DST_ACT_INFO);
        let (sw, sh) = ((sact & 0x1fff) + 1, (sact >> 16 & 0x1fff) + 1);
        let (dw, dh) = ((dact & 0xfff) + 1, (dact >> 16 & 0xfff) + 1);
        let mmu = self.r(MMU_CTRL1);
        let src = Buf { table: (mmu & 1 != 0).then(|| (self.r(MMU_SRC_BASE) as u64) << 4) };
        let dst = Buf { table: (mmu >> 8 & 1 != 0).then(|| (self.r(MMU_DST_BASE) as u64) << 4) };
        let sbase = self.r(SRC_Y_RGB) as u64;
        // The driver passes the destination corner the first source pixel goes to
        // (Linux rga_lookup_draw_pos); recover the top-left corner.
        const CORNER: [[u8; 4]; 4] = [[0, 2, 1, 3], [2, 0, 3, 1], [3, 1, 2, 0], [1, 3, 0, 2]];
        let corner = CORNER[rot as usize][mir as usize];
        let right = (dw as u64 - 1) * db as u64;
        let down = (dh as u64 - 1) * dstride;
        let dlt = (self.r(DST_Y_RGB) as u64).wrapping_sub(match corner {
            0 => 0,
            1 => down,
            2 => right,
            _ => down + right,
        });
        // the source is scaled to the destination size before rotation
        let (tw, th) = if rot & 1 == 1 { (dh, dw) } else { (dw, dh) };
        if self.log {
            eprintln!("[rga] blit {sw}x{sh} fmt {sfmt}/{sswap} -> {dw}x{dh} fmt {dfmt}/{dswap} rot {rot} mir {mir} src {sbase:#x}+{sstride} dst {dlt:#x}+{dstride} mmu {mmu:#x}");
        }
        for ty in 0..th {
            let sy = (ty as u64 * sh as u64 / th as u64) as u32;
            for tx in 0..tw {
                let sx = (tx as u64 * sw as u64 / tw as u64) as u32;
                // mirror in the source frame, then rotate clockwise
                let mx = if mir & 2 != 0 { tw - 1 - tx } else { tx };
                let my = if mir & 1 != 0 { th - 1 - ty } else { ty };
                let (x, y) = match rot {
                    1 => (dw - 1 - my, mx),
                    2 => (dw - 1 - mx, dh - 1 - my),
                    3 => (my, dh - 1 - mx),
                    _ => (mx, my),
                };
                let sva = sbase + sy as u64 * sstride + sx as u64 * sb as u64;
                let dva = dlt.wrapping_add(y as u64 * dstride + x as u64 * db as u64);
                let mut px = [0u8; 4];
                for k in 0..sb as u64 {
                    let Some(pa) = src.pa(ram, ram_base, sva + k) else { continue };
                    if let Some(i) = pa.checked_sub(ram_base).and_then(|i| usize::try_from(i).ok()) {
                        px[k as usize] = ram.get(i).copied().unwrap_or(0);
                    }
                }
                let mut out = px;
                if sfmt != dfmt || sswap != dswap {
                    encode(dfmt, dswap, decode(sfmt, sswap, &px), &mut out);
                }
                for k in 0..db as u64 {
                    let Some(pa) = dst.pa(ram, ram_base, dva + k) else { continue };
                    if let Some(i) = pa.checked_sub(ram_base).and_then(|i| usize::try_from(i).ok()) {
                        if let Some(b) = ram.get_mut(i) {
                            *b = out[k as usize];
                        }
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rotate_90_through_the_mmu() {
        // source 2x1 ABGR8888 at page 1, destination 1x2 at page 2; tables at 0x3000/0x3100
        let mut ram = vec![0u8; 0x4000];
        ram[0x1000..0x1008].copy_from_slice(&[1, 2, 3, 4, 5, 6, 7, 8]);
        ram[0x3000..0x3004].copy_from_slice(&0x1000u32.to_le_bytes());
        ram[0x3100..0x3104].copy_from_slice(&0x2000u32.to_le_bytes());
        let mut cmd = [0u32; 32];
        let set = |c: &mut [u32; 32], reg: usize, v: u32| c[(reg - MODE_BASE) / 4] = v;
        set(&mut cmd, SRC_INFO, 1 << 10); // ABGR8888, rotate 90
        set(&mut cmd, SRC_VIR_INFO, 2 << 16 | 2);
        set(&mut cmd, SRC_ACT_INFO, 1); // 2x1
        set(&mut cmd, DST_VIR_INFO, 1);
        set(&mut cmd, DST_ACT_INFO, 1 << 16); // 1x2
        set(&mut cmd, DST_Y_RGB, 0); // top-right of a 1-wide destination is its top-left
        set(&mut cmd, MMU_CTRL1, 0x707);
        set(&mut cmd, MMU_SRC_BASE, 0x3000 >> 4);
        set(&mut cmd, MMU_DST_BASE, 0x3100 >> 4);
        for (k, v) in cmd.iter().enumerate() {
            ram[0x3800 + 4 * k..0x3804 + 4 * k].copy_from_slice(&v.to_le_bytes());
        }
        let mut rga = Rga::default();
        rga.write(CMD_BASE as u64, 0x3800, &mut ram, 0);
        rga.write(INT as u64, 0x600, &mut ram, 0);
        rga.write(CMD_CTRL as u64, 1, &mut ram, 0);
        // the source row becomes the destination column, top to bottom
        assert_eq!(&ram[0x2000..0x2008], &[1, 2, 3, 4, 5, 6, 7, 8]);
        assert!(rga.irq());
        rga.write(INT as u64, 0x600 | 0x40, &mut ram, 0);
        assert!(!rga.irq());
    }
}
