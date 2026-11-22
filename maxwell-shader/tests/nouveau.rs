//! Fragment shaders compiled by Mesa 25.0.7's nv50_ir for GM20B from small GLSL programs
//! (loops, switches, discard, 64-bit integers, ...), drawn over 8x8 pixels. The expected pixels
//! are what Mesa's softpipe draws for the same GLSL. `tests/corpus.rs` describes the larger set
//! this is a sample of.

#![allow(clippy::needless_range_loop, clippy::precedence)]

use maxwell_shader::*;

struct Draw<'a> {
    ubo: &'a [u32],
    x: usize,
    y: usize,
}

impl Host<4> for Draw<'_> {
    fn cbuf(&self, index: u32, offset: u32) -> u32 {
        match index {
            // gl_FragCoord.y = y * 1 + 0
            0 => [1f32.to_bits(), 0].get(offset as usize / 4).copied().unwrap_or(0),
            1 => self.ubo.get(offset as usize / 4).copied().unwrap_or(0),
            _ => 0,
        }
    }
    fn interpolate(&mut self, op: &IpaOp<4>) -> [u32; 4] {
        std::array::from_fn(|i| {
            match op.addr {
                0x70 => (self.x + i % 2) as f32 + 0.5,
                0x74 => (self.y + i / 2) as f32 + 0.5,
                0x7c => 1.0,
                _ => 0.0,
            }
            .to_bits()
        })
    }
}

fn words(b: &[u8]) -> Vec<u32> {
    b.as_chunks().0.iter().map(|c| u32::from_le_bytes(*c)).collect()
}

#[test]
fn shaders() {
    let data = include_bytes!("data/nouveau.bin");
    let mut at = 8;
    let mut take = |n: usize| {
        at += n;
        &data[at - n..at]
    };
    let ubo = words(take(192));
    let mut count = 0;
    while take(0).as_ptr_range().end != data.as_ptr_range().end {
        let len = words(take(12));
        let name = std::str::from_utf8(take(len[0] as usize)).unwrap();
        let code = take(len[1] as usize);
        let want = words(take(len[2] as usize));
        let sph = Sph::parse(code).unwrap();
        // The file holds the header and the code, which starts at a multiple of 128.
        let p = Program::decode_at(0x80, &mut |a| {
            let a = a as usize - 0x80 + sph::SIZE as usize;
            Some(u64::from_le_bytes(code.get(a..a + 8)?.try_into().unwrap()))
        })
        .unwrap();
        assert_eq!(sph.kills_pixels, p.kills(), "{name}");
        let out = sph.frag_outputs().color[0];
        let mut got = vec![0xdead_beefu32; 256];
        let mut l = Lanes::<4>::new();
        l.set_local_size(sph.local_low as usize);
        for q in 0..16 {
            let (x, y) = (q % 4 * 2, q / 4 * 2);
            l.start(&p, 15, 0);
            let mut fuel = 100_000;
            assert_eq!(run(&p, &mut l, &mut Draw { ubo: &ubo, x, y }, &mut fuel), Exit::Done);
            for i in 0..4 {
                if l.live() >> i & 1 != 0 {
                    for c in 0..4 {
                        let o = ((y + i / 2) * 8 + x + i % 2) * 4 + c;
                        got[o] = l.gpr[out[c].unwrap() as usize][i];
                    }
                }
            }
        }
        assert_eq!(got, want, "{name}\n{p}");
        count += 1;
    }
    assert_eq!(count, 23);
}
