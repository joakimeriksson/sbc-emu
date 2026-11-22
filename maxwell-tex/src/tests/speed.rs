//! Not a check: `cargo test --release -p maxwell-tex speed -- --ignored --nocapture`.

use super::*;
use crate::layout::MsMode;
use crate::rt::RtFormat;
use crate::sample::*;
use crate::surface::{Surface, Tile};
use crate::texture::{Sampler, TexCache, Texture};
use std::time::Instant;

/// Flat memory: a slice copy per access.
struct Flat(Vec<u8>);

impl Memory for Flat {
    fn read(&self, addr: u64, out: &mut [u8]) {
        let at = (addr & 0xff_ffff) as usize;
        out.copy_from_slice(&self.0[at..at + out.len()]);
    }

    fn write(&mut self, addr: u64, data: &[u8]) {
        let at = (addr & 0xff_ffff) as usize;
        self.0[at..at + data.len()].copy_from_slice(data);
    }
}

#[test]
#[ignore]
fn speed() {
    let flat = Flat(vec![0x5a; 1 << 24]);
    for (name, format, bytes, smp, srgb) in [
        ("rgba8 nearest", 8, 4, Smp::new(false, 0), false),
        ("rgba8 bilinear", 8, 4, Smp::new(true, 0), false),
        ("srgb8 bilinear", 8, 4, Smp::new(true, 0), true),
        ("rgba8 trilinear", 8, 4, Smp { mip: 2, ..Smp::new(true, 0) }, false),
        ("rgba8 border", 8, 4, Smp::new(true, 3), false),
        ("rgba16f bilinear", 3, 8, Smp::new(true, 0), false),
        ("bc7 bilinear", 0x17, 16, Smp::new(true, 0), false),
    ] {
        let mut v = View::new(format, 1, 1024, 1024, 1);
        v.address = 0;
        v.srgb = srgb;
        v.block = BlockDims::new(4, 0);
        v.last_level = 10;
        v.view_last = 10;
        if format == 3 {
            v.types = [FLOAT; 4];
        }
        let _ = bytes;
        let tex = Texture::from_words(&v.block_linear()).expect("format");
        let smp = Sampler::from_words(&smp.words());
        let mut c = TexCache::new();
        let mut sum = 0u32;
        let t = Instant::now();
        let step = 1.3 / 1024.0;
        for y in 0..512u32 {
            for x in 0..512u32 {
                let (u, w) = (x as f32 * 2.0 * step + 0.0003, y as f32 * 2.0 * step + 0.0007);
                let a = Args { coord: [[u, u + step, u, u + step], [w, w, w + step, w + step], [0.0; 4]], lod: Lod::Auto, ..Args::default() };
                sum ^= sample(&tex, &smp, &mut c, &flat, &a)[3][1];
            }
        }
        let ns = t.elapsed().as_nanos() as f64 / (512.0 * 512.0);
        println!("{name}: {ns:.1} ns a quad ({sum:x})");
    }
    let mut flat = flat;
    let s = Surface::new(0, 1920, 1080, 0x40, 0, 4, MsMode::X1);
    let f = RtFormat::from_code(0xcf).expect("format");
    let mut tile = Tile::new(&s, 0, 0, 32, 32);
    let t = Instant::now();
    for y in 0..34 {
        for x in 0..60 {
            tile.reset(&s, x * 32, y * 32, 32, 32);
            tile.load(&s, &flat, 0);
            for py in 0..32 {
                for px in 0..32 {
                    let d = tile.sample_mut(px, py, 0);
                    let v = f.decode_f32(d).map(|v| v * 0.5 + 0.1);
                    d.copy_from_slice(&f.encode_f32(v)[..4]);
                }
            }
            tile.store(&s, &mut flat, 0);
        }
    }
    println!("1080p blend, 32x32 tiles: {:.2} ms", t.elapsed().as_secs_f64() * 1e3);
    let t = Instant::now();
    for y in 0..34 {
        for x in 0..60 {
            tile.reset(&s, x * 32, y * 32, 32, 32);
            tile.load(&s, &flat, 0);
            tile.store(&s, &mut flat, 0);
        }
    }
    println!("1080p load and store: {:.2} ms", t.elapsed().as_secs_f64() * 1e3);
}
