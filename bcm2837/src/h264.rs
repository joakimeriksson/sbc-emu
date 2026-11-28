//! The H.264 decoder behind `ril.video_decode` (cargo feature `h264`): the `rust_h264` crate
//! (pure Rust, MIT OR Apache-2.0) decodes; this module turns the byte stream the guest queues
//! into NAL units, follows the timestamps through reordering and hands pictures out in display
//! order with the stream's coded size and cropping, as the firmware's decoder does.
//!
//! Evaluation and conformance results: `docs/h264-decoder-eval.md`.

use std::collections::VecDeque;

use rust_h264::decoder::{Frame, OrderedDecoder};

use crate::video::{unescape, Bits, Picture, VideoDecoder, H264};

/// What a sequence parameter set says that the decoder's client needs.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SpsInfo {
    pub id: u32,
    /// coded size (macroblocks x 16; both fields for interlaced streams)
    pub width: u32,
    pub height: u32,
    /// after the cropping rectangle
    pub visible: (u32, u32),
    /// pictures that may wait for reordering: the VUI's `max_num_reorder_frames`, else the
    /// level's decoded picture buffer size (0 for intra-only profiles)
    pub reorder: u32,
}

/// Frames the level's decoded picture buffer holds for this picture size (Table A-1).
fn dpb_frames(level: u32, constraint_set3: bool, mbs: u32) -> u32 {
    let max_dpb_mbs = match level {
        9 | 10 => 396,
        11 if constraint_set3 => 396,
        11 => 900,
        12 | 13 | 20 => 2376,
        21 => 4752,
        22 | 30 => 8100,
        31 => 18000,
        32 => 20480,
        40 | 41 => 32768,
        42 => 34816,
        50 => 110400,
        51 | 52 => 184320,
        _ => 696320,
    };
    (max_dpb_mbs / mbs.max(1)).clamp(1, 16)
}

fn skip_scaling_list(b: &mut Bits, n: usize) -> Option<()> {
    let (mut last, mut next) = (8i32, 8i32);
    for _ in 0..n {
        if next != 0 { next = (last + b.se()? + 256) % 256; }
        if next != 0 { last = next; }
    }
    Some(())
}

fn skip_hrd(b: &mut Bits) -> Option<()> {
    let n = b.ue()? + 1;
    b.bits(8)?;
    for _ in 0..n.min(32) { b.ue()?; b.ue()?; b.bit()?; }
    b.bits(20)?;
    Some(())
}

/// Parse an SPS RBSP (after the NAL header byte).
pub fn parse_sps(rbsp: &[u8]) -> Option<SpsInfo> {
    let mut b = Bits::new(rbsp);
    let profile = b.bits(8)?;
    let constraints = b.bits(8)?;
    let level = b.bits(8)?;
    let id = b.ue()?;
    let mut chroma_format = 1;
    if [100, 110, 122, 244, 44, 83, 86, 118, 128, 138, 139, 134, 135].contains(&profile) {
        chroma_format = b.ue()?;
        if chroma_format == 3 { b.bit()?; }
        b.ue()?;
        b.ue()?;
        b.bit()?;
        if b.bit()? == 1 {
            for k in 0..if chroma_format == 3 { 12 } else { 8 } {
                if b.bit()? == 1 { skip_scaling_list(&mut b, if k < 6 { 16 } else { 64 })?; }
            }
        }
    }
    b.ue()?; // log2_max_frame_num_minus4
    match b.ue()? {
        0 => { b.ue()?; }
        1 => { b.bit()?; b.se()?; b.se()?; for _ in 0..b.ue()?.min(256) { b.se()?; } }
        _ => {}
    }
    b.ue()?; // max_num_ref_frames
    b.bit()?;
    let w_mbs = b.ue()? + 1;
    let h_map = b.ue()? + 1;
    let frame_mbs_only = b.bit()?;
    if frame_mbs_only == 0 { b.bit()?; }
    b.bit()?; // direct_8x8_inference_flag
    let h_mbs = h_map * (2 - frame_mbs_only);
    let (width, height) = (w_mbs * 16, h_mbs * 16);
    if width > 8192 || height > 8192 { return None; }
    let mut visible = (width, height);
    if b.bit()? == 1 {
        let (l, r, t, bo) = (b.ue()?, b.ue()?, b.ue()?, b.ue()?);
        let (cx, cy) = match chroma_format { 1 => (2, 2 * (2 - frame_mbs_only)), 2 => (2, 2 - frame_mbs_only), _ => (1, 2 - frame_mbs_only) };
        visible = (width.saturating_sub(cx * (l.saturating_add(r))), height.saturating_sub(cy * (t.saturating_add(bo))));
    }
    if visible.0 == 0 || visible.1 == 0 { return None; }
    // no reordering in intra-only profiles; otherwise the VUI's limit, or what the level's DPB
    // can hold (Baseline streams reorder too, with P pictures: the conformance streams
    // HCBP1/2_HHI and MR4/5_TANDBERG)
    let intra_only = constraints & 0x10 != 0 && [44, 100, 110, 122, 244].contains(&profile) || profile == 44;
    let mut reorder = if intra_only { 0 } else { dpb_frames(level, constraints & 0x10 != 0, w_mbs * h_mbs) };
    // VUI: only the bitstream restriction at its end matters here
    if b.bit() == Some(1) {
        let vui = (|| -> Option<u32> {
            if b.bit()? == 1 && b.bits(8)? == 255 { b.bits(32)?; }
            if b.bit()? == 1 { b.bit()?; }
            if b.bit()? == 1 { b.bits(4)?; if b.bit()? == 1 { b.bits(24)?; } }
            if b.bit()? == 1 { b.ue()?; b.ue()?; }
            if b.bit()? == 1 { b.bits(32)?; b.bits(32)?; b.bit()?; }
            let nal_hrd = b.bit()?;
            if nal_hrd == 1 { skip_hrd(&mut b)?; }
            let vcl_hrd = b.bit()?;
            if vcl_hrd == 1 { skip_hrd(&mut b)?; }
            if nal_hrd == 1 || vcl_hrd == 1 { b.bit()?; }
            b.bit()?; // pic_struct_present_flag
            if b.bit()? == 0 { return None; }
            b.bit()?;
            for _ in 0..4 { b.ue()?; }
            let num_reorder = b.ue()?;
            let _max_dec_buffering = b.ue()?;
            Some(num_reorder)
        })();
        if let Some(n) = vui { reorder = n.min(16); }
    }
    Some(SpsInfo { id, width, height, visible, reorder })
}

/// The longest NAL unit kept while waiting for its end (a 1080p intra picture is a few MB).
const MAX_NAL: usize = 32 << 20;

/// What travels with a picture through the reorder buffer.
#[derive(Clone, Copy, Debug)]
struct Meta {
    pts: Option<i64>,
    keyframe: bool,
    sps: Option<SpsInfo>,
}

/// The decoder `video::decoder_for(H264)` returns with the `h264` feature.
pub struct H264Decoder {
    /// stream bytes not yet handed to the decoder; `buf[0]` is at stream offset `base`
    buf: Vec<u8>,
    base: u64,
    /// where in `buf` the search for the next start code continues, and the start codes found
    scanned: usize,
    starts: Vec<usize>,
    /// (stream offset where a queued buffer began, its timestamp)
    marks: VecDeque<(u64, i64)>,
    dec: Option<OrderedDecoder<Meta>>,
    sps: Vec<SpsInfo>,
    /// PPS id -> SPS id
    pps: Vec<(u32, u32)>,
    /// the parameter set NAL units seen (type, id, the unit with its start code), replayed into
    /// a new decoder after an end of stream or a decoder panic
    params: Vec<(u8, u32, Vec<u8>)>,
    /// a recovery point SEI since the last picture started
    recovery: bool,
    /// the SPS of the picture last started: the format of the pictures to come
    active: Option<SpsInfo>,
    out: VecDeque<Picture>,
    /// NAL units the decoder refused (corrupt or unsupported), for tracing
    pub errors: u32,
}

impl Default for H264Decoder {
    fn default() -> Self {
        H264Decoder { buf: Vec::new(), base: 0, scanned: 0, starts: Vec::new(), marks: VecDeque::new(), dec: None, sps: Vec::new(), pps: Vec::new(), params: Vec::new(), recovery: false, active: None, out: VecDeque::new(), errors: 0 }
    }
}

/// Run `f`, turning a panic in the decoder into None where unwinding exists (natively); a
/// guest's corrupt stream must not take the emulator down.
fn guarded<T>(f: impl FnOnce() -> T) -> Option<T> {
    #[cfg(panic = "unwind")]
    { std::panic::catch_unwind(std::panic::AssertUnwindSafe(f)).ok() }
    #[cfg(not(panic = "unwind"))]
    { Some(f()) }
}

impl H264Decoder {
    /// Hand every complete NAL unit in `buf` to the decoder; with `all`, the last one too.
    fn scan(&mut self, all: bool) {
        let mut k = self.scanned;
        while k + 3 <= self.buf.len() {
            if self.buf[k + 2] > 1 { k += 3; continue; }
            if self.buf[k] == 0 && self.buf[k + 1] == 0 && self.buf[k + 2] == 1 { self.starts.push(k); k += 3; } else { k += 1; }
        }
        self.scanned = k;
        let n = self.starts.len();
        if n == 0 {
            // no start code yet: keep a little in case one is split across buffers
            if all || self.buf.len() > MAX_NAL {
                let keep = if all { 0 } else { 2 };
                let used = self.buf.len() - keep;
                self.buf.drain(..used);
                self.base += used as u64;
                self.scanned = 0;
            }
            return;
        }
        let complete = if all { n } else { n - 1 };
        let buf = std::mem::take(&mut self.buf);
        for w in 0..complete {
            let begin = self.starts[w];
            let mut end = self.starts.get(w + 1).copied().unwrap_or(buf.len());
            // a 4-byte start code (or trailing_zero_8bits) leaves zeros after the unit
            while end > begin + 3 && buf[end - 1] == 0 { end -= 1; }
            self.nal(&buf[begin..end], self.base + begin as u64 + 3);
        }
        self.buf = buf;
        let used = if all { self.buf.len() } else { self.starts[n - 1] };
        self.buf.drain(..used);
        self.base += used as u64;
        self.scanned -= used.min(self.scanned);
        self.starts.clear();
        if !all { self.starts.push(0); }
        if self.buf.len() > MAX_NAL {
            // a unit this long is not a picture's: drop it
            self.base += self.buf.len() as u64;
            self.buf.clear();
            self.starts.clear();
            self.scanned = 0;
        }
    }

    /// One NAL unit, with its start code; `at` is the stream offset of its header byte.
    fn nal(&mut self, unit: &[u8], at: u64) {
        let Some(&hdr) = unit.get(3) else { return };
        let t = hdr & 0x1f;
        let head = |n: usize| unescape(&unit[4..unit.len().min(4 + n)]);
        let mut meta = Meta { pts: None, keyframe: false, sps: None };
        match t {
            7 => {
                if let Some(s) = parse_sps(&unescape(&unit[4..])) {
                    self.sps.retain(|o| o.id != s.id);
                    self.sps.push(s);
                    self.params.retain(|o| (o.0, o.1) != (7, s.id));
                    self.params.push((7, s.id, unit.to_vec()));
                }
            }
            8 => {
                let r = head(8);
                let mut b = Bits::new(&r);
                if let (Some(p), Some(s)) = (b.ue(), b.ue()) {
                    self.pps.retain(|o| o.0 != p);
                    self.pps.push((p, s));
                    self.params.retain(|o| (o.0, o.1) != (8, p));
                    self.params.push((8, p, unit.to_vec()));
                }
            }
            6 => {
                // SEI: a recovery point (payload type 6) makes the next picture a random-access point
                let r = unescape(&unit[4..]);
                let mut k = 0;
                while k < r.len() && r[k] != 0x80 {
                    let mut ty = 0usize;
                    while k < r.len() && r[k] == 0xff { ty += 255; k += 1; }
                    ty += *r.get(k).unwrap_or(&0) as usize;
                    k += 1;
                    let mut size = 0usize;
                    while k < r.len() && r[k] == 0xff { size += 255; k += 1; }
                    size += *r.get(k).unwrap_or(&0) as usize;
                    k += 1;
                    if ty == 6 { self.recovery = true; }
                    k += size;
                }
            }
            1 | 5 => {
                let r = head(16);
                let mut b = Bits::new(&r);
                let first_mb = b.ue();
                let _slice_type = b.ue();
                let pps = b.ue();
                let sps_id = pps.and_then(|p| self.pps.iter().find(|o| o.0 == p)).map(|o| o.1);
                meta.sps = sps_id.and_then(|i| self.sps.iter().find(|s| s.id == i)).copied().or(self.sps.last().copied());
                if first_mb == Some(0) {
                    // the timestamp of the buffer this picture starts in
                    while let Some(&(o, p)) = self.marks.front() {
                        if o > at { break; }
                        self.marks.pop_front();
                        if self.marks.front().is_none_or(|&(n, _)| n > at) { meta.pts = Some(p); }
                    }
                    meta.keyframe = t == 5 || self.recovery;
                    self.recovery = false;
                    if meta.sps.is_some() { self.active = meta.sps; }
                }
            }
            _ => {}
        }
        if self.dec.is_none() {
            // the reorder depth comes from the first SPS (a later one with more reordering
            // would only add latency the stream does not need here)
            let Some(s) = self.sps.last() else { return };
            let mut dec = OrderedDecoder::with_max_depth(s.reorder as usize);
            // the parameter sets seen before (this unit itself goes in below)
            let replay: Vec<&Vec<u8>> = self.params.iter().filter(|p| p.2.as_slice() != unit).map(|p| &p.2).collect();
            let ok = guarded(|| for p in replay { for n in rust_h264::nal::parse_annex_b(p) { let _ = dec.decode_nal_with_meta(&n, meta); } });
            if ok.is_none() { return; }
            self.dec = Some(dec);
        }
        let dec = self.dec.as_mut().unwrap();
        let frames = guarded(|| {
            let nals = rust_h264::nal::parse_annex_b(unit);
            let mut out = Vec::new();
            let mut ok = true;
            for n in &nals {
                match dec.decode_nal_with_meta(n, meta) {
                    Ok(f) => out.extend(f),
                    Err(_) => ok = false,
                }
            }
            (out, ok)
        });
        match frames {
            Some((f, ok)) => {
                if !ok { self.errors += 1; }
                for (frame, m) in f { self.emit(frame, m); }
            }
            None => {
                // the decoder panicked: start over at the next parameter sets
                self.errors += 1;
                self.dec = None;
            }
        }
    }

    fn emit(&mut self, f: Frame, m: Meta) {
        if let Some(p) = picture(&f, m) { self.out.push_back(p); }
    }
}

/// The decoder's frame (cropped, any of its output formats) as an 8-bit 4:2:0 picture of the
/// stream's coded size.
fn picture(f: &Frame, m: Meta) -> Option<Picture> {
    let (fw, fh) = (f.width as usize, f.height as usize);
    if fw == 0 || fh == 0 { return None; }
    let (cw, ch) = (f.chroma_width() as usize, f.chroma_height() as usize);
    let (mut w, mut h) = ((fw as u32).div_ceil(16) * 16, (fh as u32).div_ceil(16) * 16);
    if let Some(s) = m.sps {
        if s.visible == (fw as u32, fh as u32) { w = w.max(s.width); h = h.max(s.height); }
    }
    let (wu, hu) = (w as usize, h as usize);
    let bytes = if f.bit_depth > 8 { 2 } else { 1 };
    if f.y.len() < fw * fh * bytes { return None; }
    // a sample (rounded to 8 bits); mid-grey where a plane is shorter than its size says
    let sample = |plane: &[u8], i: usize| -> u8 {
        if f.bit_depth > 8 {
            let Some(&[lo, hi]) = plane.get(2 * i..2 * i + 2).map(|b| <&[u8; 2]>::try_from(b).unwrap()) else { return 128 };
            ((u16::from_le_bytes([lo, hi]) as u32 + (1 << (f.bit_depth - 9))) >> (f.bit_depth - 8)).min(255) as u8
        } else {
            plane.get(i).copied().unwrap_or(128)
        }
    };
    let mut y = vec![16u8; wu * hu];
    if f.bit_depth == 8 {
        for r in 0..fh { y[r * wu..r * wu + fw].copy_from_slice(&f.y[r * fw..r * fw + fw]); }
    } else {
        for r in 0..fh { for c in 0..fw { y[r * wu + c] = sample(&f.y, r * fw + c); } }
    }
    // chroma: 4:2:0 as it is, 4:2:2 rows averaged in pairs
    let pair = ch == fh && fh > 1;
    let rows = if pair { ch / 2 } else { ch };
    let mut u = vec![128u8; (wu / 2) * (hu / 2)];
    let mut v = u.clone();
    for (dst, src) in [(&mut u, &f.u), (&mut v, &f.v)] {
        for r in 0..rows.min(hu / 2) {
            for c in 0..cw.min(wu / 2) {
                dst[r * (wu / 2) + c] = if pair {
                    ((sample(src, 2 * r * cw + c) as u32 + sample(src, (2 * r + 1) * cw + c) as u32 + 1) / 2) as u8
                } else {
                    sample(src, r * cw + c)
                };
            }
        }
    }
    Some(Picture { width: w, height: h, visible: (fw as u32, fh as u32), y, u, v, pts: m.pts, keyframe: m.keyframe })
}

impl VideoDecoder for H264Decoder {
    fn encoding(&self) -> u32 { H264 }

    fn push(&mut self, data: &[u8], pts: Option<i64>) {
        if let Some(p) = pts { self.marks.push_back((self.base + self.buf.len() as u64, p)); }
        self.buf.extend_from_slice(data);
        self.scan(false);
    }

    fn flush(&mut self) {
        self.scan(true);
        if let Some(dec) = self.dec.as_mut() {
            if let Some(f) = guarded(|| dec.flush_with_meta()) {
                for (frame, m) in f { self.emit(frame, m); }
            }
        }
        // the stream may go on after the end of stream (a client draining the decoder and
        // carrying on): a new decoder, which gets the parameter sets seen so far
        let (out, params, sps, pps) = (std::mem::take(&mut self.out), std::mem::take(&mut self.params), std::mem::take(&mut self.sps), std::mem::take(&mut self.pps));
        *self = H264Decoder { out, params, sps, pps, errors: self.errors, ..H264Decoder::default() };
    }

    fn pop(&mut self) -> Option<Picture> { self.out.pop_front() }

    fn reset(&mut self) { *self = H264Decoder::default(); }

    fn format(&self) -> Option<(u32, u32, (u32, u32))> { self.active.map(|s| (s.width, s.height, s.visible)) }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Our own test stream (x264 from ffmpeg's `testsrc2`, 96x64, 30 pictures): High profile,
    /// CABAC, 8x8 transforms, weighted prediction, two slices per picture, B-pyramid with
    /// three B pictures, an IDR every 15, access unit delimiters.
    const STREAM: &[u8] = include_bytes!("../tests/data/h264-high-96x64.264");
    /// The container's timestamps of its access units, in decode order (1/12800 s)
    const PTS: [i64; 30] = [0, 2048, 1024, 512, 1536, 3584, 2560, 3072, 4096, 6144, 5120, 4608, 5632, 6656, 7168, 7680, 8192, 8704, 10752, 9728, 9216, 10240, 11776, 11264, 12800, 12288, 14848, 13824, 13312, 14336];
    /// FNV-1a of ffmpeg's decode (`-f rawvideo -pix_fmt yuv420p`, 276480 bytes)
    const FNV: u64 = 0x035a_6580_9696_e0cd;

    fn fnv(pics: &[Picture]) -> u64 {
        let mut h = 0xcbf2_9ce4_8422_2325u64;
        let mut eat = |b: &[u8]| for &x in b { h = (h ^ x as u64).wrapping_mul(0x100_0000_01b3); };
        for p in pics {
            let (w, ht, s) = (p.visible.0 as usize, p.visible.1 as usize, p.width as usize);
            for r in 0..ht { eat(&p.y[r * s..r * s + w]); }
            for c in [&p.u, &p.v] { for r in 0..ht / 2 { eat(&c[r * s / 2..r * s / 2 + w / 2]); } }
        }
        h
    }

    fn drain(d: &mut H264Decoder, out: &mut Vec<Picture>) { while let Some(p) = d.pop() { out.push(p); } }

    /// The stream's access units (split at the delimiters, NAL type 9).
    fn access_units() -> Vec<&'static [u8]> {
        let mut cuts: Vec<usize> = (0..STREAM.len() - 4).filter(|&k| STREAM[k..k + 3] == [0, 0, 1] && STREAM[k + 3] & 0x1f == 9).map(|k| if k > 0 && STREAM[k - 1] == 0 { k - 1 } else { k }).collect();
        cuts.push(STREAM.len());
        cuts.windows(2).map(|w| &STREAM[w[0]..w[1]]).collect()
    }

    #[test]
    fn sps_reorder_depth() {
        // the stand-in's test stream: Baseline level 3.0, no VUI -> the level's DPB
        let st = crate::video::tests::stream(176, 144, 0);
        let s = parse_sps(&unescape(&st[5..])).unwrap();
        assert_eq!((s.width, s.height, s.visible), (176, 144, (176, 144)));
        assert_eq!(s.reorder, 16);
        let st = crate::video::tests::stream(1920, 1080, 0);
        let s = parse_sps(&unescape(&st[5..])).unwrap();
        assert_eq!((s.width, s.height, s.visible), (1920, 1088, (1920, 1080)));
        // Main, no VUI: the level's DPB
        assert_eq!(dpb_frames(30, false, 11 * 9), 16);
        assert_eq!(dpb_frames(40, false, 120 * 68), 4);
        assert_eq!(dpb_frames(30, false, 120 * 68), 1, "level 3.0's DPB holds less than one 1080p frame");
        // scripts/rpi3-vchiq.txt's stream: a VUI with only the bitstream restriction, no reordering
        let s = parse_sps(&[0x42, 0x00, 0x1e, 0xf4, 0x0a, 0x0f, 0xd0, 0x0f, 0x08, 0x84, 0x6a]).unwrap();
        assert_eq!((s.width, s.height, s.reorder), (320, 240, 0));
        // x264 writes the VUI's bitstream restriction: three B pictures, pyramid -> 2
        let k = STREAM.windows(4).position(|w| w == [0, 0, 1, 0x67]).unwrap();
        let s = parse_sps(&unescape(&STREAM[k + 4..k + 64])).unwrap();
        assert_eq!((s.width, s.height, s.visible, s.reorder), (96, 64, (96, 64), 2));
    }

    #[test]
    fn decodes_bit_exactly_in_any_pieces() {
        for size in [1, 5, 188, 4096, STREAM.len()] {
            let mut d = H264Decoder::default();
            let mut pics = Vec::new();
            for c in STREAM.chunks(size) { d.push(c, None); drain(&mut d, &mut pics); }
            d.flush();
            drain(&mut d, &mut pics);
            assert_eq!(pics.len(), 30, "pieces of {size}");
            assert_eq!((pics[0].width, pics[0].height, pics[0].visible), (96, 64, (96, 64)));
            assert_eq!(fnv(&pics), FNV, "pieces of {size}");
            assert_eq!(d.errors, 0);
        }
    }

    #[test]
    fn timestamps_follow_reordering() {
        let aus = access_units();
        assert_eq!(aus.len(), 30);
        let mut d = H264Decoder::default();
        let mut pics = Vec::new();
        for (au, pts) in aus.iter().zip(PTS) { d.push(au, Some(pts)); drain(&mut d, &mut pics); }
        let before_eos = pics.len();
        d.flush();
        drain(&mut d, &mut pics);
        let mut want = PTS.to_vec();
        want.sort();
        assert_eq!(pics.iter().map(|p| p.pts.unwrap()).collect::<Vec<_>>(), want, "display order, each picture with its own timestamp");
        // the reorder buffer is no deeper than the stream asks: two pictures plus the one the
        // decoder finishes when the next starts
        assert!(before_eos >= 30 - 3, "{before_eos} pictures out before the end of stream");
        let keys: Vec<i64> = pics.iter().filter(|p| p.keyframe).map(|p| p.pts.unwrap()).collect();
        assert_eq!(keys[0], 0);
        assert!(keys.len() >= 2, "an IDR every 15 pictures: {keys:?}");
        assert_eq!(fnv(&pics), FNV);
    }

    #[test]
    fn a_timestamp_goes_to_the_first_picture_starting_in_its_buffer() {
        // two access units per buffer: only the first picture of each gets the buffer's pts;
        // a buffer that only continues a picture gives none
        let aus = access_units();
        let mut d = H264Decoder::default();
        let mut pics = Vec::new();
        for (k, pair) in aus.chunks(2).enumerate() {
            let mut buf = pair[0].to_vec();
            if let Some(b) = pair.get(1) { buf.extend_from_slice(b); }
            d.push(&buf, Some(k as i64));
            drain(&mut d, &mut pics);
        }
        d.flush();
        drain(&mut d, &mut pics);
        assert_eq!(pics.len(), 30);
        let mut got: Vec<i64> = pics.iter().filter_map(|p| p.pts).collect();
        got.sort();
        assert_eq!(got, (0..15).collect::<Vec<_>>());
    }

    #[test]
    fn reset_starts_afresh() {
        let mut d = H264Decoder::default();
        let mut pics = Vec::new();
        d.push(&STREAM[..STREAM.len() / 2], Some(1));
        d.reset();
        assert!(d.pop().is_none());
        d.push(STREAM, None);
        d.flush();
        drain(&mut d, &mut pics);
        assert_eq!(pics.len(), 30);
        assert_eq!(fnv(&pics), FNV);
        // after the end of stream the next stream decodes too
        d.push(STREAM, None);
        d.flush();
        pics.clear();
        drain(&mut d, &mut pics);
        assert_eq!(fnv(&pics), FNV);
    }

    #[test]
    fn the_stream_goes_on_after_an_end_of_stream() {
        // drained after the first IDR period (15 access units), then the rest without
        // parameter sets: the second half still decodes
        let aus = access_units();
        let mut d = H264Decoder::default();
        let mut pics = Vec::new();
        for au in &aus[..15] { d.push(au, None); }
        d.flush();
        drain(&mut d, &mut pics);
        assert_eq!(pics.len(), 15);
        let rest: Vec<u8> = aus[15..].iter().flat_map(|a| a.iter().copied()).collect();
        // its parameter sets (repeated before the second IDR) cut out
        let k = (0..rest.len()).find(|&k| rest[k..].starts_with(&[0, 0, 1]) && rest[k + 3] & 0x1f == 7).expect("parameter sets before the second IDR");
        let end = (k + 4..rest.len()).find(|&e| rest[e..].starts_with(&[0, 0, 1]) && !matches!(rest[e + 3] & 0x1f, 7 | 8)).unwrap();
        let mut r = rest[..k].to_vec();
        r.extend_from_slice(&rest[end..]);
        d.push(&r, None);
        d.flush();
        drain(&mut d, &mut pics);
        assert_eq!(pics.len(), 30);
        assert_eq!(fnv(&pics), FNV);
    }

    #[test]
    fn corrupt_streams_do_not_panic() {
        let mut d = H264Decoder::default();
        // slices without parameter sets, then garbage, then the stream with bytes flipped
        d.push(&STREAM[STREAM.len() / 2..], None);
        let mut x = 0x1234_5678u32;
        let mut junk = vec![0u8; 20000];
        for b in junk.iter_mut() { x ^= x << 13; x ^= x >> 17; x ^= x << 5; *b = x as u8; }
        d.push(&junk, None);
        let mut bad = STREAM.to_vec();
        for k in (200..bad.len()).step_by(97) { bad[k] ^= 0x5a; }
        d.push(&bad, None);
        d.flush();
        while d.pop().is_some() {}
        d.push(STREAM, None);
        d.flush();
        let mut pics = Vec::new();
        drain(&mut d, &mut pics);
        assert_eq!(fnv(&pics), FNV, "a good stream decodes after a bad one");
    }
}
