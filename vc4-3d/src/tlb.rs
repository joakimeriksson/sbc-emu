//! The tile buffer: colour (32 bits), Z (24 bits) with stencil (8 bits) and the VG mask for one
//! tile: 64x64 pixels, or 32x32 with four samples each in 4x multisample mode. Loads fill it
//! from memory, stores write it out (raster, T or LT; RGBA8888 or BGR565; one sample or the
//! samples averaged) and clear it unless told not to; full-resolution loads and stores move the
//! raw per-sample contents (16 KiB per tile).

use crate::cl::{self, le16, le32};
use crate::mem::Mem;
use crate::tiling::Layout;

/// The frame of `TILE_RENDERING_MODE_CONFIG`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Frame {
    pub addr: u32,
    pub width: u32,
    pub height: u32,
    pub ms: bool,
    pub hdr: bool,
    /// 0: BGR565 dithered, 1: RGBA8888, 2: BGR565
    pub format: u32,
    pub decimate: u32,
    pub tiling: u8,
    pub vg_mask: bool,
    pub early_z_ge: bool,
    pub early_z_off: bool,
}

impl Frame {
    pub fn parse(p: &[u8]) -> Frame {
        let f = le16(&p[9..]) as u32;
        Frame {
            addr: le32(&p[1..]), width: le16(&p[5..]) as u32, height: le16(&p[7..]) as u32,
            ms: f & 1 != 0, hdr: f & 2 != 0, format: f >> 2 & 3, decimate: f >> 4 & 3, tiling: (f >> 6 & 3) as u8,
            vg_mask: f >> 8 & 1 != 0, early_z_ge: f >> 10 & 1 != 0, early_z_off: f >> 11 & 1 != 0,
        }
    }
    pub fn tile_size(&self) -> u32 { if self.ms || self.hdr { 32 } else { 64 } }
}

#[derive(Clone)]
pub struct TileBuffer {
    pub size: u32,
    pub samples: u32,
    pub color: Vec<u32>,
    /// Z in bits 31:8, stencil in 7:0
    pub zs: Vec<u32>,
    pub vg: Vec<u8>,
    pub clear_color: [u32; 2],
    pub clear_z: u32,
    pub clear_s: u8,
    pub clear_vg: u8,
}

impl Default for TileBuffer { fn default() -> Self { TileBuffer::new() } }

impl TileBuffer {
    pub fn new() -> TileBuffer {
        TileBuffer { size: 64, samples: 1, color: vec![0; 4096], zs: vec![0; 4096], vg: vec![0; 4096], clear_color: [0; 2], clear_z: 0, clear_s: 0, clear_vg: 0 }
    }

    /// Single-sampled 64x64, or 32x32 with four samples.
    pub fn configure(&mut self, f: &Frame) {
        let (size, samples) = if f.ms { (32, 4) } else if f.hdr { (32, 1) } else { (64, 1) };
        if (size, samples) != (self.size, self.samples) {
            self.size = size;
            self.samples = samples;
            let n = (size * size * samples) as usize;
            self.color = vec![0; n];
            self.zs = vec![0; n];
            self.vg = vec![0; n];
        }
    }

    #[inline]
    pub fn idx(&self, x: u32, y: u32, s: u32) -> usize { ((y * self.size + x) * self.samples + s) as usize }

    pub fn clear(&mut self, color: bool, zs: bool, vg: bool) {
        if color { let c = self.clear_color[0]; self.color.iter_mut().for_each(|p| *p = c); }
        if zs { let v = self.clear_z << 8 | self.clear_s as u32; self.zs.iter_mut().for_each(|p| *p = v); }
        if vg { let v = self.clear_vg; self.vg.iter_mut().for_each(|p| *p = v); }
    }

    pub fn set_clear(&mut self, p: &[u8]) {
        self.clear_color = [le32(&p[1..]), le32(&p[5..])];
        self.clear_z = le32(&p[9..]) & 0xff_ffff;
        self.clear_vg = p[12];
        self.clear_s = p[13];
    }

    /// The colour of pixel (x, y): sample 0, or the samples averaged.
    fn resolved(&self, x: u32, y: u32, average: bool) -> u32 {
        if !average || self.samples == 1 { return self.color[self.idx(x, y, 0)]; }
        let mut sum = [0u32; 4];
        for s in 0..self.samples {
            let c = self.color[self.idx(x, y, s)].to_le_bytes();
            for k in 0..4 { sum[k] += c[k] as u32; }
        }
        let n = self.samples;
        u32::from_le_bytes(sum.map(|v| ((v + n / 2) / n) as u8))
    }

    /// Store one buffer of the tile at (tx, ty) into the frame-sized image at `addr`.
    /// `buffer`: 1 colour, 2 Z+stencil, 3 Z, 4 VG mask; `format`: 0 RGBA8888, else BGR565.
    /// Pixels go out a run at a time: a tile row (raster) or a utile row (T, LT), which are
    /// contiguous in memory.
    #[allow(clippy::too_many_arguments)]
    pub fn store(&self, frame: &Frame, tile: (u32, u32), addr: u32, buffer: u32, tiling: u8, format: u32, average: bool, mem: &mut Mem) {
        let cpp = match buffer { 1 if format != 0 => 2, 4 => 1, _ => 4 };
        if !matches!(buffer, 1..=4) { return; }
        let lay = Layout::new(tiling, cpp, frame.width);
        let ts = self.size;
        let run = if tiling == crate::tiling::RASTER { ts } else { crate::tiling::utile_w(cpp) };
        let mut buf = [0u8; 64 * 4];
        for py in 0..ts {
            let y = tile.1 * ts + py;
            if y >= frame.height { break; }
            let mut px = 0;
            while px < ts {
                let x = tile.0 * ts + px;
                if x >= frame.width { break; }
                let n = run.min(frame.width - x) as usize;
                for k in 0..n {
                    let (qx, b) = (px + k as u32, &mut buf[k * cpp as usize..]);
                    match buffer {
                        1 => {
                            let c = self.resolved(qx, py, average);
                            if cpp == 2 { b[..2].copy_from_slice(&to565(c).to_le_bytes()) } else { b[..4].copy_from_slice(&c.to_le_bytes()) }
                        }
                        2 | 3 => b[..4].copy_from_slice(&self.zs[self.idx(qx, py, 0)].to_le_bytes()),
                        _ => b[0] = self.vg[self.idx(qx, py, 0)],
                    }
                }
                mem.write(addr.wrapping_add(lay.offset(x, y)), &buf[..n * cpp as usize]);
                px += run;
            }
        }
    }

    /// Load one buffer of the tile at (tx, ty) from the frame-sized image at `addr` (every
    /// sample gets the pixel's value), a run of contiguous pixels at a time.
    #[allow(clippy::too_many_arguments)]
    pub fn load(&mut self, frame: &Frame, tile: (u32, u32), addr: u32, buffer: u32, tiling: u8, format: u32, mem: &mut Mem) {
        let cpp = match buffer { 1 if format != 0 => 2, 4 => 1, _ => 4 };
        if !matches!(buffer, 1..=4) { return; }
        let lay = Layout::new(tiling, cpp, frame.width);
        let ts = self.size;
        let run = if tiling == crate::tiling::RASTER { ts } else { crate::tiling::utile_w(cpp) };
        let mut buf = [0u8; 64 * 4];
        for py in 0..ts {
            let y = tile.1 * ts + py;
            if y >= frame.height { break; }
            let mut px = 0;
            while px < ts {
                let x = tile.0 * ts + px;
                if x >= frame.width { break; }
                let n = run.min(frame.width - x) as usize;
                mem.read(addr.wrapping_add(lay.offset(x, y)), &mut buf[..n * cpp as usize]);
                for k in 0..n {
                    let b = &buf[k * cpp as usize..];
                    let w = |b: &[u8]| u32::from_le_bytes([b[0], b[1], b[2], b[3]]);
                    for s in 0..self.samples {
                        let i = self.idx(px + k as u32, py, s);
                        match buffer {
                            1 => self.color[i] = if cpp == 2 { from565(u16::from_le_bytes([b[0], b[1]])) } else { w(b) },
                            2 => self.zs[i] = w(b),
                            3 => self.zs[i] = (w(b) & !0xff) | (self.zs[i] & 0xff),
                            _ => self.vg[i] = b[0],
                        }
                    }
                }
                px += run;
            }
        }
    }

    /// Byte offset of (x, y, sample) in a full-resolution dump: 2x2 quads of 16 bytes per
    /// sample (Mesa's `vc4_nir_lower_txf_ms` reads multisampled ones this way).
    pub fn raw_offset(&self, x: u32, y: u32, s: u32) -> u32 {
        let quads_per_row = self.size / 2;
        ((y >> 1) * quads_per_row + (x >> 1)) * 16 * self.samples + s * 16 + (y & 1) * 8 + (x & 1) * 4
    }

    pub fn store_raw(&self, addr: u32, zs: bool, mem: &mut Mem) {
        for y in 0..self.size { for x in 0..self.size { for s in 0..self.samples {
            let i = self.idx(x, y, s);
            mem.w32(addr.wrapping_add(self.raw_offset(x, y, s)), if zs { self.zs[i] } else { self.color[i] });
        } } }
    }

    pub fn load_raw(&mut self, addr: u32, zs: bool, mem: &mut Mem) {
        for y in 0..self.size { for x in 0..self.size { for s in 0..self.samples {
            let i = self.idx(x, y, s);
            let v = mem.r32(addr.wrapping_add(self.raw_offset(x, y, s)));
            if zs { self.zs[i] = v } else { self.color[i] = v }
        } } }
    }

    /// `STORE_TILE_BUFFER_GENERAL` (`p`: the packet). Returns whether it ends the frame.
    pub fn store_general(&mut self, frame: &Frame, tile: (u32, u32), p: &[u8], mem: &mut Mem) -> bool {
        let lo = le16(&p[1..]) as u32;
        let w = le32(&p[3..]);
        let buffer = lo & 7;
        let tiling = (lo >> 4 & 3) as u8;
        let mode = lo >> 6 & 3;
        let format = lo >> 8 & 3;
        let addr = w & !15;
        if buffer != 0 {
            if buffer == 5 {
                // all buffers the dump-disable bits leave
                if w & 1 == 0 { self.store(frame, tile, addr, 1, tiling, format, mode != 0, mem); }
                if w & 2 == 0 { crate::log_first("vc4: full-buffer general store of Z not modelled"); }
            } else {
                self.store(frame, tile, addr, buffer, tiling, format, mode != 0, mem);
            }
        }
        self.clear(lo & 1 << 13 == 0, lo & 1 << 14 == 0, lo & 1 << 15 == 0);
        w & 8 != 0
    }

    pub fn load_general(&mut self, frame: &Frame, tile: (u32, u32), p: &[u8], mem: &mut Mem) {
        let lo = le16(&p[1..]) as u32;
        let w = le32(&p[3..]);
        let buffer = lo & 7;
        let tiling = (lo >> 4 & 3) as u8;
        let format = lo >> 8 & 3;
        let addr = w & !15;
        match buffer {
            0 => {}
            5 => {
                if w & 1 == 0 { self.load(frame, tile, addr, 1, tiling, format, mem); }
            }
            b => self.load(frame, tile, addr, b, tiling, format, mem),
        }
    }

    /// `STORE_MS_TILE_BUFFER(_AND_EOF)`: the colour to the frame, resolved, then clear all.
    pub fn store_ms(&mut self, frame: &Frame, tile: (u32, u32), mem: &mut Mem) {
        let fmt = if frame.format == 1 { 0 } else { 2 };
        self.store(frame, tile, frame.addr, 1, frame.tiling, fmt, frame.ms, mem);
        self.clear(true, true, true);
    }

    /// `STORE_FULL_RES_TILE_BUFFER`. Returns whether it ends the frame.
    pub fn store_full(&mut self, p: &[u8], mem: &mut Mem) -> bool {
        let w = le32(&p[1..]);
        let addr = w & !15;
        if w & 1 == 0 { self.store_raw(addr, false, mem); }
        else if w & 2 == 0 { self.store_raw(addr, true, mem); }
        if w & 4 == 0 { self.clear(true, true, true); }
        w & 8 != 0
    }

    pub fn load_full(&mut self, p: &[u8], mem: &mut Mem) {
        let w = le32(&p[1..]);
        let addr = w & !15;
        if w & 1 == 0 { self.load_raw(addr, false, mem); }
        else if w & 2 == 0 { self.load_raw(addr, true, mem); }
    }
}

/// A tile-buffer colour (byte 0 red) as BGR565 (red in the top bits, Mesa's B5G6R5).
pub fn to565(c: u32) -> u16 {
    let b = c.to_le_bytes();
    ((b[0] as u16 >> 3) << 11) | ((b[1] as u16 >> 2) << 5) | (b[2] as u16 >> 3)
}

pub fn from565(h: u16) -> u32 {
    let h = h as u32;
    let r = h >> 11;
    let g = h >> 5 & 63;
    let b = h & 31;
    u32::from_le_bytes([((r << 3) | (r >> 2)) as u8, ((g << 2) | (g >> 4)) as u8, ((b << 3) | (b >> 2)) as u8, 255])
}

/// Size of a packet that only rendering lists contain, decoded for tracing.
pub fn describe_store(p: &[u8]) -> String {
    let lo = le16(&p[1..]) as u32;
    let w = le32(&p[3..]);
    format!("{} buffer {} tiling {} format {} addr {:#x}{}", cl::name(p[0]), lo & 7, lo >> 4 & 3, lo >> 8 & 3, w & !15, if w & 8 != 0 { " EOF" } else { "" })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tiling;

    #[test]
    fn store_and_load_round_trip_in_every_layout() {
        for tiling in [tiling::RASTER, tiling::T, tiling::LT] {
            let mut ram = vec![0u8; 1 << 20];
            let mut m = Mem::new(&mut ram);
            let frame = Frame { addr: 0x1_0000, width: 100, height: 70, format: 1, tiling, ..Default::default() };
            let mut tb = TileBuffer::new();
            tb.configure(&frame);
            for t in [(0u32, 0u32), (1, 0), (0, 1), (1, 1)] {
                for y in 0..64 { for x in 0..64 { let i = tb.idx(x, y, 0); tb.color[i] = (t.0 * 64 + x) | (t.1 * 64 + y) << 16; } }
                tb.store(&frame, t, frame.addr, 1, tiling, 0, false, &mut m);
            }
            let mut tb2 = TileBuffer::new();
            tb2.load(&frame, (1, 1), frame.addr, 1, tiling, 0, &mut m);
            assert_eq!(tb2.color[tb2.idx(3, 5, 0)], 67 | 69 << 16, "tiling {tiling}");
        }
    }

    #[test]
    fn rgb565_conversion() {
        assert_eq!(to565(0x00ff_00ff), 0xf81f);
        assert_eq!(from565(0xf81f), 0xffff_00ff);
    }

    #[test]
    fn general_store_clears_unless_disabled() {
        let mut ram = vec![0u8; 1 << 16];
        let mut m = Mem::new(&mut ram);
        let frame = Frame { width: 64, height: 64, format: 1, ..Default::default() };
        let mut tb = TileBuffer::new();
        tb.clear_color = [0x1234_5678, 0];
        tb.color.iter_mut().for_each(|c| *c = 7);
        // buffer None, colour clear disabled: nothing changes
        let p = [cl::STORE_TILE_BUFFER_GENERAL, 0, 0x20, 0, 0, 0, 0];
        tb.store_general(&frame, (0, 0), &p, &mut m);
        assert_eq!(tb.color[0], 7);
        let p = [cl::STORE_TILE_BUFFER_GENERAL, 0, 0, 0, 0, 0, 0];
        tb.store_general(&frame, (0, 0), &p, &mut m);
        assert_eq!(tb.color[0], 0x1234_5678);
    }
}
