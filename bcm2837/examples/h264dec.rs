//! Decode an H.264 Annex B file through the Raspberry Pi 3's decoder interface
//! (`video::decoder_for(H264)`, the `h264` feature) and write the visible part of each picture
//! as I420, in display order:
//!   cargo run --release -p bcm2837 --features h264 --example h264dec -- in.264 out.yuv [chunk]
//! The stream goes in as pieces of `chunk` bytes (default 4096; 0: one NAL unit per push), as a
//! guest queues it. Prints the picture count, size and decode time.
use std::io::Write;

fn main() {
    let a: Vec<String> = std::env::args().collect();
    if a.len() < 3 { eprintln!("usage: h264dec in.264 out.yuv [chunk]"); std::process::exit(2); }
    let data = std::fs::read(&a[1]).expect("read input");
    let chunk: usize = a.get(3).map(|s| s.parse().unwrap()).unwrap_or(4096);
    let mut out = std::io::BufWriter::new(std::fs::File::create(&a[2]).expect("create output"));
    let mut d = bcm2837::video::decoder_for(bcm2837::video::H264).unwrap();
    let t = std::time::Instant::now();
    let mut pics = Vec::new();
    let pieces: Vec<&[u8]> = if chunk == 0 {
        // split before each start code
        let mut v = Vec::new();
        let mut last = 0;
        for k in 1..data.len().saturating_sub(3) {
            if data[k..k + 3] == [0, 0, 1] && data[k - 1] != 0 { v.push(&data[last..k]); last = k; }
        }
        v.push(&data[last..]);
        v
    } else { data.chunks(chunk.max(1)).collect() };
    for (k, p) in pieces.iter().enumerate() {
        d.push(p, Some(k as i64));
        while let Some(p) = d.pop() { pics.push(p); }
    }
    d.flush();
    while let Some(p) = d.pop() { pics.push(p); }
    let ms = t.elapsed().as_secs_f64() * 1000.0;
    for p in &pics {
        let (w, h) = (p.visible.0 as usize, p.visible.1 as usize);
        let s = p.width as usize;
        for r in 0..h { out.write_all(&p.y[r * s..r * s + w]).unwrap(); }
        for plane in [&p.u, &p.v] {
            for r in 0..h.div_ceil(2) { out.write_all(&plane[r * (s / 2)..r * (s / 2) + w.div_ceil(2)]).unwrap(); }
        }
    }
    let dims = pics.first().map(|p| format!("{}x{} coded {}x{}", p.visible.0, p.visible.1, p.width, p.height)).unwrap_or_default();
    println!("frames {} size {} time_ms {:.1} status ok", pics.len(), dims, ms);
}
