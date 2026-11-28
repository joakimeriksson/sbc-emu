//! Pictures for the VideoCore's media components (`mmal.rs`): the raw formats MMAL ports
//! carry, conversion and scaling between them (the ISP), and the interface the video decoder
//! behind `ril.video_decode` implements.
//!
//! The H.264 decoder is in `h264.rs`, behind the `h264` cargo feature. Without it
//! `TestPatternDecoder` stands in: it reads the stream's sequence parameter sets for the picture
//! size and emits one test picture (colour bars with a moving marker) per coded picture, so the
//! whole V4L2 path (`bcm2835-codec`, format negotiation, the source-change event, buffer
//! exchange, end of stream) runs as with the real decoder.

/// MMAL four-character codes (`MMAL_FOURCC`: first character in the low byte).
pub const fn mmal_fourcc(s: &[u8; 4]) -> u32 { s[0] as u32 | (s[1] as u32) << 8 | (s[2] as u32) << 16 | (s[3] as u32) << 24 }

pub const H264: u32 = mmal_fourcc(b"H264");
pub const JPEG: u32 = mmal_fourcc(b"JPEG");
pub const I420: u32 = mmal_fourcc(b"I420");
pub const YV12: u32 = mmal_fourcc(b"YV12");
pub const NV12: u32 = mmal_fourcc(b"NV12");
pub const NV21: u32 = mmal_fourcc(b"NV21");
pub const YUYV: u32 = mmal_fourcc(b"YUYV");
pub const YVYU: u32 = mmal_fourcc(b"YVYU");
pub const UYVY: u32 = mmal_fourcc(b"UYVY");
pub const VYUY: u32 = mmal_fourcc(b"VYUY");
pub const RGB24: u32 = mmal_fourcc(b"RGB3");
pub const BGR24: u32 = mmal_fourcc(b"BGR3");
pub const RGBA: u32 = mmal_fourcc(b"RGBA");
pub const BGRA: u32 = mmal_fourcc(b"BGRA");
pub const RGB16: u32 = mmal_fourcc(b"RGB2");
/// MMAL colour spaces (`es.video.color_space`)
pub const BT601: u32 = mmal_fourcc(b"Y601");

/// The planar and packed YUV formats the ports take, then the RGB ones.
pub const YUV_FORMATS: &[u32] = &[I420, YV12, NV12, NV21, YUYV, YVYU, UYVY, VYUY];
pub const RGB_FORMATS: &[u32] = &[RGB24, BGR24, RGBA, BGRA, RGB16];

/// Bytes of one picture of `enc` with a luma stride of `stride` pixels and `height` rows (the
/// MMAL port's `es.video.width` and `height`); None for a format this model does not handle.
pub fn frame_size(enc: u32, stride: u32, height: u32) -> Option<u32> {
    let (s, h) = (stride as u64, height as u64);
    let n = match enc {
        I420 | YV12 | NV12 | NV21 => s * h + 2 * (s / 2) * h.div_ceil(2),
        YUYV | YVYU | UYVY | VYUY | RGB16 => s * h * 2,
        RGB24 | BGR24 => s * h * 3,
        RGBA | BGRA => s * h * 4,
        _ => return None,
    };
    u32::try_from(n).ok()
}

/// A picture in the ISP's working form: full-resolution Y, U and V planes (4:4:4, BT.601
/// limited range), `w` x `h`.
#[derive(Clone, Debug, PartialEq)]
pub struct Image {
    pub w: u32,
    pub h: u32,
    pub y: Vec<u8>,
    pub u: Vec<u8>,
    pub v: Vec<u8>,
}

fn rgb_to_yuv(r: u8, g: u8, b: u8) -> (u8, u8, u8) {
    let (r, g, b) = (r as i32, g as i32, b as i32);
    let y = ((66 * r + 129 * g + 25 * b + 128) >> 8) + 16;
    let u = ((-38 * r - 74 * g + 112 * b + 128) >> 8) + 128;
    let v = ((112 * r - 94 * g - 18 * b + 128) >> 8) + 128;
    (y.clamp(0, 255) as u8, u.clamp(0, 255) as u8, v.clamp(0, 255) as u8)
}

fn yuv_to_rgb(y: u8, u: u8, v: u8) -> (u8, u8, u8) {
    let c = 298 * (y as i32 - 16);
    let (d, e) = (u as i32 - 128, v as i32 - 128);
    let r = (c + 409 * e + 128) >> 8;
    let g = (c - 100 * d - 208 * e + 128) >> 8;
    let b = (c + 516 * d + 128) >> 8;
    (r.clamp(0, 255) as u8, g.clamp(0, 255) as u8, b.clamp(0, 255) as u8)
}

impl Image {
    pub fn new(w: u32, h: u32) -> Image {
        let n = (w * h) as usize;
        Image { w, h, y: vec![16; n], u: vec![128; n], v: vec![128; n] }
    }

    /// From 4:2:0 planes (luma rows of `stride` bytes, chroma of `stride / 2`).
    pub fn from_420(w: u32, h: u32, stride: u32, y: &[u8], u: &[u8], v: &[u8]) -> Image {
        let mut im = Image::new(w, h);
        let cs = (stride / 2) as usize;
        for row in 0..h as usize {
            for col in 0..w as usize {
                let k = row * w as usize + col;
                im.y[k] = y.get(row * stride as usize + col).copied().unwrap_or(16);
                let c = (row / 2) * cs + col / 2;
                im.u[k] = u.get(c).copied().unwrap_or(128);
                im.v[k] = v.get(c).copied().unwrap_or(128);
            }
        }
        im
    }

    /// Read a picture of `enc` laid out as an MMAL port describes it (`stride` pixels per row,
    /// `height` rows per plane), `w` x `h` of it visible.
    pub fn read(enc: u32, stride: u32, height: u32, w: u32, h: u32, src: &[u8]) -> Option<Image> {
        frame_size(enc, stride, height)?;
        let (s, ph) = (stride as usize, height as usize);
        let at = |k: usize| src.get(k).copied().unwrap_or(0);
        let mut im = Image::new(w, h);
        for row in 0..h as usize {
            for col in 0..w as usize {
                let k = row * w as usize + col;
                let (y, u, v) = match enc {
                    I420 | YV12 | NV12 | NV21 => {
                        let y = at(row * s + col);
                        let cbase = s * ph;
                        let (cr, cc) = (row / 2, col / 2);
                        let (u, v) = match enc {
                            I420 => (at(cbase + cr * (s / 2) + cc), at(cbase + (s / 2) * ph.div_ceil(2) + cr * (s / 2) + cc)),
                            YV12 => (at(cbase + (s / 2) * ph.div_ceil(2) + cr * (s / 2) + cc), at(cbase + cr * (s / 2) + cc)),
                            NV12 => (at(cbase + cr * s + 2 * cc), at(cbase + cr * s + 2 * cc + 1)),
                            _ => (at(cbase + cr * s + 2 * cc + 1), at(cbase + cr * s + 2 * cc)),
                        };
                        (y, u, v)
                    }
                    YUYV | YVYU | UYVY | VYUY => {
                        let p = row * s * 2 + (col & !1) * 2;
                        let q = [at(p), at(p + 1), at(p + 2), at(p + 3)];
                        let (y0, y1, u, v) = match enc {
                            YUYV => (q[0], q[2], q[1], q[3]),
                            YVYU => (q[0], q[2], q[3], q[1]),
                            UYVY => (q[1], q[3], q[0], q[2]),
                            _ => (q[1], q[3], q[2], q[0]),
                        };
                        (if col & 1 == 0 { y0 } else { y1 }, u, v)
                    }
                    RGB16 => {
                        let p = (row * s + col) * 2;
                        let px = u16::from_le_bytes([at(p), at(p + 1)]);
                        let (r, g, b) = ((px >> 11) as u8, (px >> 5 & 63) as u8, (px & 31) as u8);
                        rgb_to_yuv(r << 3 | r >> 2, g << 2 | g >> 4, b << 3 | b >> 2)
                    }
                    _ => {
                        let bpp = if matches!(enc, RGB24 | BGR24) { 3 } else { 4 };
                        let p = (row * s + col) * bpp;
                        let (a, b, c) = (at(p), at(p + 1), at(p + 2));
                        if matches!(enc, RGB24 | RGBA) { rgb_to_yuv(a, b, c) } else { rgb_to_yuv(c, b, a) }
                    }
                };
                im.y[k] = y;
                im.u[k] = u;
                im.v[k] = v;
            }
        }
        Some(im)
    }

    /// Scale to `w` x `h` (bilinear on luma and chroma).
    pub fn scale(&self, w: u32, h: u32) -> Image {
        if (w, h) == (self.w, self.h) { return self.clone(); }
        let mut out = Image::new(w, h);
        if self.w == 0 || self.h == 0 { return out; }
        let sample = |plane: &[u8], fx: u32, fy: u32| -> u8 {
            // 16.16 fixed point source position of the destination pixel's centre
            let (x0, y0) = ((fx >> 16) as usize, (fy >> 16) as usize);
            let (x1, y1) = ((x0 + 1).min(self.w as usize - 1), (y0 + 1).min(self.h as usize - 1));
            let (ax, ay) = (fx & 0xffff, fy & 0xffff);
            let p = |x: usize, y: usize| plane[y * self.w as usize + x] as u32;
            let top = p(x0, y0) * (0x10000 - ax) + p(x1, y0) * ax;
            let bot = p(x0, y1) * (0x10000 - ax) + p(x1, y1) * ax;
            (((top >> 16) * (0x10000 - ay) + (bot >> 16) * ay + 0x8000) >> 16) as u8
        };
        let step = |src: u32, dst: u32| ((src as u64) << 16) / dst as u64;
        let (sx, sy) = (step(self.w, w), step(self.h, h));
        for row in 0..h {
            let fy = ((row as u64 * sy + sy / 2).saturating_sub(0x8000) as u32).min((self.h - 1) << 16);
            for col in 0..w {
                let fx = ((col as u64 * sx + sx / 2).saturating_sub(0x8000) as u32).min((self.w - 1) << 16);
                let k = (row * w + col) as usize;
                out.y[k] = sample(&self.y, fx, fy);
                out.u[k] = sample(&self.u, fx, fy);
                out.v[k] = sample(&self.v, fx, fy);
            }
        }
        out
    }

    /// Write as `enc` with `stride` pixels per row and `height` rows per plane; returns the
    /// bytes used (the format's frame size), None if the format is not handled or `dst` is
    /// too small. Rows and columns beyond the picture are left as they were.
    pub fn write(&self, enc: u32, stride: u32, height: u32, dst: &mut [u8]) -> Option<u32> {
        let size = frame_size(enc, stride, height)?;
        if (dst.len() as u64) < size as u64 || stride < self.w || height < self.h { return None; }
        let (s, ph, w, h) = (stride as usize, height as usize, self.w as usize, self.h as usize);
        let px = |row: usize, col: usize| { let k = row * w + col; (self.y[k], self.u[k], self.v[k]) };
        // chroma of a 2x2 block (or 2x1 pair), averaged
        let avg = |row: usize, col: usize, rows: usize| -> (u8, u8) {
            let (mut u, mut v, mut n) = (0u32, 0u32, 0u32);
            for r in row..(row + rows).min(h) {
                for c in col..(col + 2).min(w) { let (_, a, b) = px(r, c); u += a as u32; v += b as u32; n += 1; }
            }
            (((u + n / 2) / n) as u8, ((v + n / 2) / n) as u8)
        };
        match enc {
            I420 | YV12 | NV12 | NV21 => {
                for row in 0..h { for col in 0..w { dst[row * s + col] = px(row, col).0; } }
                let cbase = s * ph;
                let cplane = (s / 2) * ph.div_ceil(2);
                for cr in 0..h.div_ceil(2) {
                    for cc in 0..w.div_ceil(2) {
                        let (u, v) = avg(cr * 2, cc * 2, 2);
                        match enc {
                            I420 => { dst[cbase + cr * (s / 2) + cc] = u; dst[cbase + cplane + cr * (s / 2) + cc] = v; }
                            YV12 => { dst[cbase + cr * (s / 2) + cc] = v; dst[cbase + cplane + cr * (s / 2) + cc] = u; }
                            NV12 => { dst[cbase + cr * s + 2 * cc] = u; dst[cbase + cr * s + 2 * cc + 1] = v; }
                            _ => { dst[cbase + cr * s + 2 * cc] = v; dst[cbase + cr * s + 2 * cc + 1] = u; }
                        }
                    }
                }
            }
            YUYV | YVYU | UYVY | VYUY => {
                for row in 0..h {
                    for col in (0..w).step_by(2) {
                        let (u, v) = avg(row, col, 1);
                        let y0 = px(row, col).0;
                        let y1 = if col + 1 < w { px(row, col + 1).0 } else { y0 };
                        let q = match enc { YUYV => [y0, u, y1, v], YVYU => [y0, v, y1, u], UYVY => [u, y0, v, y1], _ => [v, y0, u, y1] };
                        let p = row * s * 2 + col * 2;
                        let n = if col + 1 < w { 4 } else { 4.min(dst.len() - p) };
                        dst[p..p + n].copy_from_slice(&q[..n]);
                    }
                }
            }
            _ => {
                let bpp = match enc { RGB16 => 2, RGB24 | BGR24 => 3, _ => 4 };
                for row in 0..h {
                    for col in 0..w {
                        let (y, u, v) = px(row, col);
                        let (r, g, b) = yuv_to_rgb(y, u, v);
                        let p = (row * s + col) * bpp;
                        match enc {
                            RGB16 => dst[p..p + 2].copy_from_slice(&((r as u16 >> 3) << 11 | (g as u16 >> 2) << 5 | b as u16 >> 3).to_le_bytes()),
                            RGB24 => dst[p..p + 3].copy_from_slice(&[r, g, b]),
                            BGR24 => dst[p..p + 3].copy_from_slice(&[b, g, r]),
                            RGBA => dst[p..p + 4].copy_from_slice(&[r, g, b, 255]),
                            _ => dst[p..p + 4].copy_from_slice(&[b, g, r, 255]),
                        }
                    }
                }
            }
        }
        Some(size)
    }
}

/// One decoded picture, as a decoder hands it out: 8-bit 4:2:0, luma `width` x `height` (the
/// coded size, multiples of 16 for H.264) with rows of `width` bytes, chroma planes half that
/// in each direction; `visible` is the part to show (the stream's cropping).
#[derive(Clone, Debug, PartialEq)]
pub struct Picture {
    pub width: u32,
    pub height: u32,
    pub visible: (u32, u32),
    pub y: Vec<u8>,
    pub u: Vec<u8>,
    pub v: Vec<u8>,
    /// the presentation time of the input buffer the picture came from (µs, as MMAL carries
    /// it), None if that buffer had none
    pub pts: Option<i64>,
    /// an IDR or other random-access point
    pub keyframe: bool,
}

impl Picture {
    /// Write the visible part as `enc` (`stride` pixels per row, `height` rows per plane):
    /// the 4:2:0 formats copied plane by plane, the others through `Image`. Returns the bytes
    /// used, None if the format is not handled or the buffer is too small.
    pub fn write(&self, enc: u32, stride: u32, height: u32, dst: &mut [u8]) -> Option<u32> {
        let (vw, vh) = (self.visible.0.min(self.width) as usize, self.visible.1.min(self.height) as usize);
        let (w, s, ph) = (self.width as usize, stride as usize, height as usize);
        let size = frame_size(enc, stride, height)?;
        if (dst.len() as u64) < size as u64 || s < vw || ph < vh { return None; }
        let (cw, ch) = (vw.div_ceil(2), vh.div_ceil(2));
        let cplane = (s / 2) * ph.div_ceil(2);
        match enc {
            I420 | YV12 => {
                for r in 0..vh { dst[r * s..r * s + vw].copy_from_slice(&self.y[r * w..r * w + vw]); }
                let (first, second) = if enc == I420 { (&self.u, &self.v) } else { (&self.v, &self.u) };
                for r in 0..ch {
                    let o = s * ph + r * (s / 2);
                    dst[o..o + cw].copy_from_slice(&first[r * (w / 2)..r * (w / 2) + cw]);
                    dst[o + cplane..o + cplane + cw].copy_from_slice(&second[r * (w / 2)..r * (w / 2) + cw]);
                }
            }
            NV12 | NV21 => {
                for r in 0..vh { dst[r * s..r * s + vw].copy_from_slice(&self.y[r * w..r * w + vw]); }
                let (a, b) = if enc == NV12 { (&self.u, &self.v) } else { (&self.v, &self.u) };
                for r in 0..ch {
                    for c in 0..cw {
                        let o = s * ph + r * s + 2 * c;
                        dst[o] = a[r * (w / 2) + c];
                        dst[o + 1] = b[r * (w / 2) + c];
                    }
                }
            }
            _ => {
                let im = Image::from_420(vw as u32, vh as u32, self.width, &self.y, &self.u, &self.v);
                return im.write(enc, stride, height, dst);
            }
        }
        Some(size)
    }
}

/// A video decoder behind `ril.video_decode`. The VideoCore model feeds it the elementary
/// stream exactly as the guest queues it on the decoder's input port (Annex B byte stream for
/// H.264; a buffer may hold part of an access unit, one, or several) and collects pictures in
/// display order. It runs on the emulator's thread when the guest queues a buffer, natively and
/// in the browser build, so it must be plain Rust without host dependencies.
pub trait VideoDecoder: Send {
    /// The MMAL encoding the decoder takes (`H264`).
    fn encoding(&self) -> u32;
    /// More of the stream; `pts` belongs to the first picture that starts in `data`.
    fn push(&mut self, data: &[u8], pts: Option<i64>);
    /// End of stream: decode whatever is still buffered and release every picture held for
    /// reordering.
    fn flush(&mut self);
    /// The next picture in display order, if one is ready.
    fn pop(&mut self) -> Option<Picture>;
    /// Forget the stream (the port was flushed or disabled): the next data starts afresh and
    /// must begin with parameter sets.
    fn reset(&mut self);
    /// The format of the pictures to come, once the stream's parameter sets say it: coded width
    /// and height, and the visible size. The firmware announces a new format when it starts on
    /// a picture, before that picture is out (a decoder that holds pictures for reordering
    /// would otherwise announce it late); None if not known (the first picture announces it).
    fn format(&self) -> Option<(u32, u32, (u32, u32))> { None }
}

/// The decoders this build has, by input encoding: H.264 with the `h264` feature (`h264.rs`),
/// otherwise the stand-in.
pub fn decoder_for(encoding: u32) -> Option<Box<dyn VideoDecoder>> {
    match encoding {
        #[cfg(feature = "h264")]
        H264 => Some(Box::new(crate::h264::H264Decoder::default())),
        #[cfg(not(feature = "h264"))]
        H264 => Some(Box::new(TestPatternDecoder::default())),
        _ => None,
    }
}

/// Reads an H.264 RBSP bit by bit (emulation-prevention bytes already removed), Exp-Golomb.
pub struct Bits<'a> { d: &'a [u8], p: usize }

impl<'a> Bits<'a> {
    pub fn new(d: &'a [u8]) -> Self { Bits { d, p: 0 } }
    pub fn bit(&mut self) -> Option<u32> {
        let b = *self.d.get(self.p / 8)? >> (7 - self.p % 8) & 1;
        self.p += 1;
        Some(b as u32)
    }
    pub fn bits(&mut self, n: u32) -> Option<u32> { let mut v = 0; for _ in 0..n { v = v << 1 | self.bit()?; } Some(v) }
    pub fn ue(&mut self) -> Option<u32> {
        let mut zeros = 0;
        while self.bit()? == 0 { zeros += 1; if zeros > 31 { return None; } }
        Some(((1u64 << zeros) - 1 + self.bits(zeros)? as u64) as u32)
    }
    pub fn se(&mut self) -> Option<i32> { let k = self.ue()?; Some(if k & 1 == 1 { (k / 2 + 1) as i32 } else { -((k / 2) as i32) }) }
}

/// The NAL unit's payload without emulation-prevention bytes (00 00 03 -> 00 00).
pub fn unescape(nal: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(nal.len());
    let mut zeros = 0;
    for &b in nal {
        if zeros >= 2 && b == 3 { zeros = 0; continue; }
        zeros = if b == 0 { zeros + 1 } else { 0 };
        out.push(b);
    }
    out
}

/// What a sequence parameter set says about the picture size.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SpsSize {
    /// coded size in macroblocks x 16 (frame height: both fields with `frame_mbs_only_flag` 0)
    pub width: u32,
    pub height: u32,
    /// after the frame cropping rectangle
    pub visible: (u32, u32),
}

/// The picture size from an SPS NAL unit's RBSP (after the one-byte NAL header).
pub fn parse_sps(rbsp: &[u8]) -> Option<SpsSize> {
    let mut b = Bits::new(rbsp);
    let profile = b.bits(8)?;
    b.bits(16)?; // constraint flags, level
    b.ue()?; // seq_parameter_set_id
    let mut chroma_format = 1;
    if [100, 110, 122, 244, 44, 83, 86, 118, 128, 138, 139, 134, 135].contains(&profile) {
        chroma_format = b.ue()?;
        if chroma_format == 3 { b.bit()?; }
        b.ue()?; b.ue()?; // bit depths
        b.bit()?; // qpprime_y_zero_transform_bypass_flag
        if b.bit()? == 1 {
            for k in 0..if chroma_format == 3 { 12 } else { 8 } {
                if b.bit()? == 1 {
                    let n = if k < 6 { 16 } else { 64 };
                    let (mut last, mut next) = (8i32, 8i32);
                    for _ in 0..n {
                        if next != 0 { next = (last + b.se()? + 256) % 256; }
                        if next != 0 { last = next; }
                    }
                }
            }
        }
    }
    b.ue()?; // log2_max_frame_num_minus4
    match b.ue()? {
        0 => { b.ue()?; }
        1 => { b.bit()?; b.se()?; b.se()?; for _ in 0..b.ue()? { b.se()?; } }
        _ => {}
    }
    b.ue()?; // max_num_ref_frames
    b.bit()?;
    let w_mbs = b.ue()? + 1;
    let h_map = b.ue()? + 1;
    let frame_mbs_only = b.bit()?;
    if frame_mbs_only == 0 { b.bit()?; }
    b.bit()?; // direct_8x8_inference_flag
    let (width, height) = (w_mbs * 16, h_map * 16 * (2 - frame_mbs_only));
    let mut visible = (width, height);
    if b.bit()? == 1 {
        let (l, r, t, bo) = (b.ue()?, b.ue()?, b.ue()?, b.ue()?);
        let (cx, cy) = match chroma_format { 0 => (1, 2 - frame_mbs_only), 1 => (2, 2 * (2 - frame_mbs_only)), 2 => (2, 2 - frame_mbs_only), _ => (1, 2 - frame_mbs_only) };
        visible = (width.saturating_sub(cx * (l + r)), height.saturating_sub(cy * (t + bo)));
    }
    if width > 8192 || height > 8192 || visible.0 == 0 || visible.1 == 0 { return None; }
    Some(SpsSize { width, height, visible })
}

/// The stand-in for the H.264 decoder: finds the NAL units in the stream, takes the picture
/// size from the sequence parameter set and turns every coded picture (a slice with
/// `first_mb_in_slice` 0) into a test picture: colour bars, and a white square that moves one
/// step per picture. It decodes nothing.
#[derive(Default)]
pub struct TestPatternDecoder {
    buf: Vec<u8>,
    pending_pts: Option<i64>,
    sps: Option<SpsSize>,
    out: std::collections::VecDeque<Picture>,
    count: u32,
}

impl TestPatternDecoder {
    /// Handle complete NAL units in `buf`; with `all`, the last one too (end of stream).
    fn scan(&mut self, all: bool) {
        let starts: Vec<usize> = (0..self.buf.len().saturating_sub(2)).filter(|&k| self.buf[k] == 0 && self.buf[k + 1] == 0 && self.buf[k + 2] == 1).collect();
        let n = starts.len();
        if n == 0 { return; }
        // the last unit is complete only at the end of the stream
        for w in 0..if all { n } else { n - 1 } {
            let begin = starts[w] + 3;
            let mut end = starts.get(w + 1).copied().unwrap_or(self.buf.len());
            // a 4-byte start code (or trailing_zero_8bits) leaves zeros after the unit
            while end > begin && self.buf[end - 1] == 0 { end -= 1; }
            let nal = self.buf[begin..end].to_vec();
            self.nal(&nal);
        }
        let used = if all { self.buf.len() } else { starts[n - 1] };
        self.buf.drain(..used);
    }

    fn nal(&mut self, nal: &[u8]) {
        let Some(&hdr) = nal.first() else { return };
        let rbsp = unescape(&nal[1..]);
        match hdr & 0x1f {
            7 => if let Some(s) = parse_sps(&rbsp) { self.sps = Some(s); },
            t @ (1 | 5) => {
                if Bits::new(&rbsp).ue() != Some(0) { return; }
                let Some(s) = self.sps else { return };
                let pts = self.pending_pts.take();
                self.out.push_back(self.picture(s, pts, t == 5));
                self.count += 1;
            }
            _ => {}
        }
    }

    fn picture(&self, s: SpsSize, pts: Option<i64>, keyframe: bool) -> Picture {
        // 75% colour bars: white, yellow, cyan, green, magenta, red, blue, black (Y, U, V)
        const BARS: [(u8, u8, u8); 8] = [(180, 128, 128), (162, 44, 142), (131, 156, 44), (112, 72, 58), (84, 184, 198), (65, 100, 212), (35, 212, 114), (16, 128, 128)];
        let (w, h) = (s.width as usize, s.height as usize);
        let (vw, vh) = (s.visible.0 as usize, s.visible.1 as usize);
        let mut y = vec![16u8; w * h];
        let mut u = vec![128u8; (w / 2) * h.div_ceil(2)];
        let mut v = u.clone();
        let side = (vh / 4).max(2) & !1;
        let travel = vw.saturating_sub(side).max(1);
        let mx = (self.count as usize * 8) % travel & !1;
        let my = (vh - side) / 2 & !1;
        for row in 0..vh {
            for col in 0..vw {
                let (by, bu, bv) = if (mx..mx + side).contains(&col) && (my..my + side).contains(&row) { (235, 128, 128) } else { BARS[col * 8 / vw] };
                y[row * w + col] = by;
                if row % 2 == 0 && col % 2 == 0 { u[(row / 2) * (w / 2) + col / 2] = bu; v[(row / 2) * (w / 2) + col / 2] = bv; }
            }
        }
        Picture { width: s.width, height: s.height, visible: s.visible, y, u, v, pts, keyframe }
    }
}

impl VideoDecoder for TestPatternDecoder {
    fn encoding(&self) -> u32 { H264 }
    fn push(&mut self, data: &[u8], pts: Option<i64>) {
        if self.pending_pts.is_none() { self.pending_pts = pts; }
        self.buf.extend_from_slice(data);
        self.scan(false);
    }
    fn flush(&mut self) { self.scan(true); self.buf.clear(); }
    fn pop(&mut self) -> Option<Picture> { self.out.pop_front() }
    fn reset(&mut self) { *self = TestPatternDecoder::default(); }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    /// Write `v` as `n` bits.
    pub(crate) struct BitWriter { pub out: Vec<u8>, acc: u32, n: u32 }
    impl BitWriter {
        pub fn new() -> Self { BitWriter { out: Vec::new(), acc: 0, n: 0 } }
        pub fn put(&mut self, v: u32, n: u32) { for k in (0..n).rev() { self.acc = self.acc << 1 | (v >> k & 1); self.n += 1; if self.n == 8 { self.out.push(self.acc as u8); self.acc = 0; self.n = 0; } } }
        pub fn ue(&mut self, v: u32) { let x = v + 1; let len = 32 - x.leading_zeros(); self.put(0, len - 1); self.put(x, len); }
        pub fn finish(mut self) -> Vec<u8> { self.put(1, 1); while self.n != 0 { self.put(0, 1); } self.out }
    }

    /// A Baseline SPS for `w` x `h` (cropped from the macroblock grid), and the start of an
    /// IDR slice: what the decoder needs to emit pictures of that size.
    pub(crate) fn stream(w: u32, h: u32, pictures: usize) -> Vec<u8> {
        let mut s = BitWriter::new();
        s.put(66, 8); s.put(0, 8); s.put(30, 8); // profile, constraints, level
        s.ue(0); s.ue(0); s.ue(0); s.ue(0); s.ue(1); s.put(0, 1);
        let (mw, mh) = (w.div_ceil(16), h.div_ceil(16));
        s.ue(mw - 1); s.ue(mh - 1); s.put(1, 1); s.put(1, 1);
        let crop = (mw * 16 - w, mh * 16 - h);
        if crop != (0, 0) { s.put(1, 1); s.ue(0); s.ue(crop.0 / 2); s.ue(0); s.ue(crop.1 / 2); } else { s.put(0, 1); }
        s.put(0, 1); // no VUI
        let mut out = vec![0, 0, 0, 1, 0x67];
        out.extend(s.finish());
        for k in 0..pictures {
            let mut sl = BitWriter::new();
            sl.ue(0); sl.ue(if k == 0 { 7 } else { 5 }); sl.ue(0);
            out.extend_from_slice(&[0, 0, 0, 1, if k == 0 { 0x65 } else { 0x41 }]);
            out.extend(sl.finish());
        }
        out
    }

    #[test]
    fn sps_size_and_cropping() {
        let st = stream(320, 240, 0);
        let s = parse_sps(&unescape(&st[5..])).unwrap();
        assert_eq!((s.width, s.height, s.visible), (320, 240, (320, 240)));
        let st = stream(1920, 1080, 0);
        let s = parse_sps(&unescape(&st[5..])).unwrap();
        assert_eq!((s.width, s.height, s.visible), (1920, 1088, (1920, 1080)));
    }

    #[test]
    fn test_decoder_one_picture_per_coded_picture() {
        let mut d = TestPatternDecoder::default();
        let st = stream(176, 144, 3);
        // in odd pieces, as a guest may queue them
        for (k, c) in st.chunks(7).enumerate() { d.push(c, Some(k as i64 * 1000)); }
        let mut n = 0;
        while let Some(p) = d.pop() { assert_eq!((p.width, p.height), (176, 144)); n += 1; }
        assert_eq!(n, 2, "the last slice waits for the next start code");
        d.flush();
        let p = d.pop().unwrap();
        assert!(!p.keyframe);
        assert!(d.pop().is_none());
        // colour bars: the left edge is white-ish, the right edge black
        assert_eq!(p.y[10 * 176 + 1], 180);
        assert_eq!(p.y[10 * 176 + 175], 16);
    }

    #[test]
    fn conversions_round_trip() {
        let mut im = Image::new(8, 4);
        for (k, y) in im.y.iter_mut().enumerate() { *y = 16 + k as u8 * 5; }
        for f in YUV_FORMATS {
            let size = frame_size(*f, 16, 4).unwrap() as usize;
            let mut buf = vec![0u8; size];
            assert_eq!(im.write(*f, 16, 4, &mut buf), Some(size as u32));
            let back = Image::read(*f, 16, 4, 8, 4, &buf).unwrap();
            assert_eq!(back.y, im.y, "{f:08x}");
            assert_eq!(back.u, im.u);
        }
        // RGB: grey stays grey within rounding
        let mut buf = vec![0u8; frame_size(RGB24, 8, 4).unwrap() as usize];
        im.write(RGB24, 8, 4, &mut buf).unwrap();
        let back = Image::read(RGB24, 8, 4, 8, 4, &buf).unwrap();
        for (a, b) in back.y.iter().zip(&im.y) { assert!((*a as i32 - *b as i32).abs() <= 1); }
        assert_eq!(im.scale(4, 2).y.len(), 8);
    }
}
