//! The ITU-T H.264.1 (JVT) conformance bitstreams through the Raspberry Pi 3's decoder
//! (`video::decoder_for(H264)`, feature `h264`), checked against the MD5 of FFmpeg's decode of
//! each stream (`tests/data/h264-conformance.txt`). The bitstreams are not in git: fetch them
//! as `docs/h264-decoder-eval.md` says, into `cache/h264-conformance/` (one directory per
//! stream, as the zip files unpack) or set `H264_CONFORMANCE` to where they are. Streams that
//! are missing are skipped; with none present the test passes with a note.
//!
//!   cargo test --release -p bcm2837 --features h264 --test h264_conformance -- --nocapture

use std::path::{Path, PathBuf};

use bcm2837::video::{decoder_for, Picture, H264};

const MANIFEST: &str = include_str!("data/h264-conformance.txt");

/// MD5 (RFC 1321), enough to compare with the manifest.
struct Md5 { s: [u32; 4], buf: Vec<u8>, len: u64 }

impl Md5 {
    fn new() -> Self { Md5 { s: [0x67452301, 0xefcdab89, 0x98badcfe, 0x10325476], buf: Vec::with_capacity(64), len: 0 } }
    fn block(&mut self, m: &[u8]) {
        const R: [u32; 64] = [7, 12, 17, 22, 7, 12, 17, 22, 7, 12, 17, 22, 7, 12, 17, 22, 5, 9, 14, 20, 5, 9, 14, 20, 5, 9, 14, 20, 5, 9, 14, 20, 4, 11, 16, 23, 4, 11, 16, 23, 4, 11, 16, 23, 4, 11, 16, 23, 6, 10, 15, 21, 6, 10, 15, 21, 6, 10, 15, 21, 6, 10, 15, 21];
        let w: Vec<u32> = m.chunks(4).map(|c| u32::from_le_bytes([c[0], c[1], c[2], c[3]])).collect();
        let [mut a, mut b, mut c, mut d] = self.s;
        for i in 0..64 {
            let (f, g) = match i / 16 {
                0 => ((b & c) | (!b & d), i),
                1 => ((d & b) | (!d & c), (5 * i + 1) % 16),
                2 => (b ^ c ^ d, (3 * i + 5) % 16),
                _ => (c ^ (b | !d), (7 * i) % 16),
            };
            let k = ((i as f64 + 1.0).sin().abs() * 4294967296.0) as u32;
            let t = d;
            d = c;
            c = b;
            b = b.wrapping_add(a.wrapping_add(f).wrapping_add(k).wrapping_add(w[g]).rotate_left(R[i]));
            a = t;
        }
        for (s, v) in self.s.iter_mut().zip([a, b, c, d]) { *s = s.wrapping_add(v); }
    }
    fn update(&mut self, mut d: &[u8]) {
        self.len += d.len() as u64;
        if !self.buf.is_empty() {
            let n = (64 - self.buf.len()).min(d.len());
            self.buf.extend_from_slice(&d[..n]);
            d = &d[n..];
            if self.buf.len() == 64 { let b = std::mem::take(&mut self.buf); self.block(&b); }
        }
        while d.len() >= 64 { self.block(&d[..64]); d = &d[64..]; }
        self.buf.extend_from_slice(d);
    }
    fn hex(mut self) -> String {
        let bits = self.len * 8;
        let mut pad = vec![0x80u8];
        while (self.len + pad.len() as u64) % 64 != 56 { pad.push(0); }
        pad.extend_from_slice(&bits.to_le_bytes());
        let len = self.len;
        self.update(&pad);
        self.len = len;
        self.s.iter().flat_map(|w| w.to_le_bytes()).map(|b| format!("{b:02x}")).collect()
    }
}

/// The visible part of each picture as I420, as FFmpeg writes `-pix_fmt yuv420p`.
fn hash(pics: &[Picture]) -> String {
    let mut m = Md5::new();
    for p in pics {
        let (w, h, s) = (p.visible.0 as usize, p.visible.1 as usize, p.width as usize);
        for r in 0..h { m.update(&p.y[r * s..r * s + w]); }
        for c in [&p.u, &p.v] { for r in 0..h.div_ceil(2) { m.update(&c[r * (s / 2)..r * (s / 2) + w.div_ceil(2)]); } }
    }
    m.hex()
}

fn root() -> PathBuf {
    std::env::var_os("H264_CONFORMANCE").map(PathBuf::from).unwrap_or_else(|| Path::new(env!("CARGO_MANIFEST_DIR")).join("../cache/h264-conformance"))
}

fn find(root: &Path, dir: &str, file: &str) -> Option<PathBuf> {
    [root.join(dir).join(file), root.join(file)].into_iter().find(|p| p.is_file())
}

#[test]
fn md5_known_answer() {
    let mut m = Md5::new();
    m.update(b"The quick brown fox jumps over the lazy dog");
    assert_eq!(m.hex(), "9e107d9d372bb6826bd81d3542a419d6");
    assert_eq!(Md5::new().hex(), "d41d8cd98f00b204e9800998ecf8427e");
}

#[test]
fn conformance_streams_match_ffmpeg() {
    let root = root();
    let (mut ran, mut failed, mut missing) = (0, Vec::new(), 0);
    for line in MANIFEST.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') { continue; }
        // name file frames md5 [piece size]
        let f: Vec<&str> = line.split_whitespace().collect();
        let (name, file, frames, md5) = (f[0], f[1], f[2].parse::<usize>().unwrap(), f[3]);
        let Some(path) = find(&root, name, file) else { missing += 1; continue };
        let data = std::fs::read(&path).unwrap();
        let mut d = decoder_for(H264).unwrap();
        let mut pics = Vec::new();
        // pieces of an odd size, as a guest may queue them
        for c in data.chunks(65521) {
            d.push(c, None);
            while let Some(p) = d.pop() { pics.push(p); }
        }
        d.flush();
        while let Some(p) = d.pop() { pics.push(p); }
        let got = hash(&pics);
        let ok = pics.len() == frames && got == md5;
        println!("{} {name}: {} of {frames} pictures", if ok { "PASS" } else { "FAIL" }, pics.len());
        if !ok { failed.push(name.to_string()); }
        ran += 1;
    }
    if ran == 0 {
        println!("no conformance bitstreams under {} ({missing} listed): skipped (docs/h264-decoder-eval.md says how to fetch them)", root.display());
    } else {
        println!("{ran} streams decoded, {} failed, {missing} not present", failed.len());
    }
    assert!(failed.is_empty(), "streams that differ from FFmpeg: {failed:?}");
}
