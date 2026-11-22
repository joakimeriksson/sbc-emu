//! The copy engine (class B0B5, MAXWELL_DMA_COPY_A): lines between pitch and block-linear surfaces,
//! component remapping, and a semaphore at the end.
use super::mmu::View;
use crate::dc::gob_offset;

#[derive(Default, Clone)]
pub struct Copy {
    regs: std::collections::HashMap<u32, u32>,
}

/// A block-linear surface's shape: sizes in bytes and lines, blocks in log2 GOBs.
#[derive(Clone, Copy)]
struct Block {
    width: u64,
    height: u64,
    gobs_w: u32,
    gobs_h: u32,
    gobs_d: u32,
}

impl Block {
    /// Byte `x` of line `y` in slice `z`.
    fn offset(&self, x: u64, y: u64, z: u64) -> u64 {
        let (bw, bh, bd) = (64u64 << self.gobs_w, 8u64 << self.gobs_h, 1u64 << self.gobs_d);
        let size = bw * bh * bd;
        let per_row = self.width.div_ceil(bw);
        let rows = self.height.div_ceil(bh);
        let block = (z / bd * rows + y / bh) * per_row + x / bw;
        let (x, y, z) = (x % bw, y % bh, z % bd);
        // a block's GOBs: down, then deeper, then to the right
        let gob = (x / 64 * bd + z) * (bh / 8) + y / 8;
        block * size + gob * 512 + gob_offset(x % 64, y % 8)
    }
}

impl Copy {
    fn r(&self, m: u32) -> u32 {
        self.regs.get(&m).copied().unwrap_or(0)
    }

    pub fn method(&mut self, m: u32, v: u32, view: &mut View) {
        self.regs.insert(m, v);
        if m == 0x300 {
            self.launch(v, view);
        }
    }

    fn launch(&mut self, v: u32, view: &mut View) {
        let address = |hi: u32| ((self.r(hi) & 0xff) as u64) << 32 | self.r(hi + 4) as u64;
        if v & 3 != 0 {
            self.copy(v, address(0x400), address(0x408), view);
        }
        // a semaphore: one word, or four with the time
        let at = address(0x240);
        match v >> 3 & 3 {
            1 => view.write32(at, self.r(0x248)),
            2 => {
                view.write32(at, self.r(0x248));
                view.write(at + 8, &0u64.to_le_bytes());
            }
            _ => {}
        }
    }

    fn copy(&self, v: u32, src: u64, dst: u64, view: &mut View) {
        let (src_pitch, dst_pitch) = (self.r(0x410) as i32 as i64, self.r(0x414) as i32 as i64);
        let length = self.r(0x418) as u64;
        let lines = if v >> 9 & 1 != 0 { self.r(0x41c).max(1) as u64 } else { 1 };
        // components: what is written comes from a source component, a constant, or nothing
        let remap = v >> 10 & 1 != 0;
        let swizzle = self.r(0x708);
        let (size, from, to) = if remap {
            ((swizzle >> 16 & 3) as u64 + 1, (swizzle >> 20 & 3) as u64 + 1, (swizzle >> 24 & 3) as u64 + 1)
        } else {
            (1, 1, 1)
        };
        let (src_bytes, dst_bytes) = (length * size * from, length * size * to);
        // (with remapping, widths count elements)
        let block = |base: u32, element: u64| {
            let b = self.r(base);
            Block { width: self.r(base + 4) as u64 * element, height: self.r(base + 8) as u64, gobs_w: b & 15, gobs_h: b >> 4 & 15, gobs_d: b >> 8 & 15 }
        };
        let origin = |base: u32| ((self.r(base + 0x14) & 0xffff) as u64, (self.r(base + 0x14) >> 16) as u64, self.r(base + 0x10) as u64);
        let src_block = (v >> 7 & 1 == 0).then(|| (block(0x728, size * from), origin(0x728)));
        let dst_block = (v >> 8 & 1 == 0).then(|| (block(0x70c, size * to), origin(0x70c)));
        // (a block-linear origin counts elements)
        let (sx, dx) = (size * from, size * to);
        let mut line = vec![0u8; src_bytes as usize];
        for y in 0..lines {
            match src_block {
                None => view.read((src as i64 + y as i64 * src_pitch) as u64, &mut line),
                Some((b, (ox, oy, oz))) => runs(ox * if remap { sx } else { 1 }, src_bytes, |x, at, n| view.read(src + b.offset(x, oy + y, oz), &mut line[at..at + n])),
            }
            let out: std::borrow::Cow<[u8]> = if remap {
                let mut o = Vec::with_capacity(dst_bytes as usize);
                for e in line.chunks_exact((size * from) as usize) {
                    for c in 0..to as u32 {
                        let sel = swizzle >> (4 * c) & 7;
                        match sel {
                            0..=3 => o.extend_from_slice(&e[(sel as u64 * size) as usize..((sel as u64 + 1) * size) as usize]),
                            4 | 5 => o.extend_from_slice(&self.r(0x700 + 4 * (sel - 4)).to_le_bytes()[..size as usize]),
                            _ => o.extend(std::iter::repeat_n(0, size as usize)), // not written
                        }
                    }
                }
                o.into()
            } else {
                (&line[..]).into()
            };
            match dst_block {
                None => view.write((dst as i64 + y as i64 * dst_pitch) as u64, &out),
                Some((b, (ox, oy, oz))) => runs(ox * if remap { dx } else { 1 }, dst_bytes, |x, at, n| view.write(dst + b.offset(x, oy + y, oz), &out[at..at + n])),
            }
        }
    }
}

/// A line in pieces that are contiguous in a GOB: (x, offset in the line, bytes).
fn runs(x: u64, bytes: u64, mut f: impl FnMut(u64, usize, usize)) {
    let mut at = 0;
    while at < bytes {
        let n = (16 - (x + at) % 16).min(bytes - at);
        f(x + at, at as usize, n as usize);
        at += n;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn block_offsets_are_a_bijection() {
        let b = Block { width: 256, height: 64, gobs_w: 0, gobs_h: 2, gobs_d: 1 };
        let mut seen = std::collections::HashSet::new();
        for z in 0..4 {
            for y in 0..64 {
                for x in 0..256 {
                    let o = b.offset(x, y, z);
                    assert!(o < 256 * 64 * 4 && seen.insert(o));
                }
            }
        }
        assert_eq!(b.offset(0, 8, 0), 512);
        assert_eq!(b.offset(0, 0, 1), 4 * 512);
        assert_eq!(b.offset(64, 0, 0), 8 * 512);
    }
}
