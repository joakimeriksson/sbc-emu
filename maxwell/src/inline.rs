//! Inline to memory (A140, also inside the 3D and compute classes from 0x180): data written method
//! by method goes to lines of a pitch or block-linear surface.
use crate::Memory;

#[derive(Clone)]
pub struct Inline {
    regs: [u32; 64],
    /// the registers at the launch
    latched: [u32; 64],
    data: Vec<u8>,
    expected: usize,
}

impl Default for Inline {
    fn default() -> Self {
        Self { regs: [0; 64], latched: [0; 64], data: Vec::new(), expected: 0 }
    }
}

/// A byte's place in a 64 × 8 byte GOB.
pub fn gob_offset(x: u64, y: u64) -> u64 {
    (x & 32) << 3 | (y & 6) << 5 | (x & 16) << 1 | (y & 1) << 4 | x & 15
}

/// A block-linear surface's shape: bytes and lines, blocks in log2 GOBs.
#[derive(Clone, Copy, Debug)]
pub struct Block {
    pub width: u64,
    pub height: u64,
    pub gobs_w: u32,
    pub gobs_h: u32,
    pub gobs_d: u32,
}

impl Block {
    /// Byte `x` of line `y` in slice `z`.
    pub fn offset(&self, x: u64, y: u64, z: u64) -> u64 {
        let (bw, bh, bd) = (64u64 << self.gobs_w, 8u64 << self.gobs_h, 1u64 << self.gobs_d);
        let per_row = self.width.div_ceil(bw);
        let rows = self.height.div_ceil(bh);
        // (wrapping: the registers may hold anything)
        let block = (z / bd).wrapping_mul(rows).wrapping_add(y / bh).wrapping_mul(per_row).wrapping_add(x / bw);
        let (x, y, z) = (x % bw, y % bh, z % bd);
        // a block's GOBs: down, then deeper, then to the right
        let gob = (x / 64 * bd + z) * (bh / 8) + y / 8;
        block.wrapping_mul(bw * bh * bd).wrapping_add(gob * 512 + gob_offset(x % 64, y % 8))
    }

    /// A line of `bytes` from `x` in pieces contiguous in memory: (offset, position, bytes).
    pub fn pieces(&self, x: u64, y: u64, z: u64, bytes: u64, mut f: impl FnMut(u64, usize, usize)) {
        let mut at = 0;
        while at < bytes {
            let n = (16 - x.wrapping_add(at) % 16).min(bytes - at);
            f(self.offset(x.wrapping_add(at), y, z), at as usize, n as usize);
            at += n;
        }
    }
}

/// The largest image written through the push buffer.
const MAX_BYTES: u64 = 1 << 26;

const LINE_LENGTH: usize = 0x180;
const LINES: usize = 0x184;
const ADDRESS: usize = 0x188;
const PITCH: usize = 0x190;
const BLOCK: usize = 0x194;
const WIDTH: usize = 0x198;
const HEIGHT: usize = 0x19c;
const LAYER: usize = 0x1a4;
const ORIGIN_X: usize = 0x1a8;
const ORIGIN_Y: usize = 0x1ac;
const LAUNCH: usize = 0x1b0;
const DATA: usize = 0x1b4;

impl Inline {
    /// The methods of this unit, wherever they are: 0x180 to 0x1b4.
    pub fn owns(method: u32) -> bool {
        (LINE_LENGTH as u32..=DATA as u32).contains(&method)
    }

    fn r(&self, m: usize) -> u64 {
        self.latched[(m - 0x100) / 4] as u64
    }

    pub fn method(&mut self, m: u32, v: u32, mem: &mut dyn Memory) {
        self.upload(m, v, |address, data| {
            mem.write(address, data);
            Some(())
        });
    }

    pub fn upload(&mut self, m: u32, v: u32, write: impl FnMut(u64, &[u8]) -> Option<()>) -> Option<()> {
        let m = m as usize;
        if m != DATA {
            self.regs[(m - 0x100) / 4] = v;
        }
        match m {
            LAUNCH => {
                self.data.clear();
                self.latched = self.regs;
                // (more than any push buffer brings is not taken)
                let n = self.r(LINE_LENGTH).saturating_mul(self.r(LINES));
                self.expected = if n <= MAX_BYTES { n as usize } else { 0 };
            }
            DATA if self.expected > 0 => {
                self.data.extend_from_slice(&v.to_le_bytes());
                if self.data.len() >= self.expected {
                    self.flush(write)?;
                }
            }
            _ => {}
        }
        Some(())
    }

    fn flush(&mut self, mut write: impl FnMut(u64, &[u8]) -> Option<()>) -> Option<()> {
        let base = (self.r(ADDRESS) & 0xff) << 32 | self.r(ADDRESS + 4);
        let (length, lines) = (self.r(LINE_LENGTH), self.r(LINES));
        let pitch = self.r(LAUNCH) & 1 != 0;
        let b = self.r(BLOCK) as u32;
        let block = Block { width: self.r(WIDTH), height: self.r(HEIGHT), gobs_w: b & 15, gobs_h: b >> 4 & 15, gobs_d: b >> 8 & 15 };
        let mut written = Some(());
        for y in 0..lines {
            let line = &self.data[(y * length) as usize..((y + 1) * length) as usize];
            if pitch {
                written = write(base.wrapping_add(y.wrapping_mul(self.r(PITCH))), line);
            } else {
                block.pieces(self.r(ORIGIN_X), self.r(ORIGIN_Y).wrapping_add(y), self.r(LAYER), length, |o, at, n| {
                    written = written.and_then(|_| write(base.wrapping_add(o), &line[at..at + n]));
                });
            }
            if written.is_none() {
                break;
            }
        }
        self.expected = 0;
        self.data.clear();
        written
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Ram(Vec<u8>);
    impl Memory for Ram {
        fn read(&self, va: u64, out: &mut [u8]) { out.copy_from_slice(&self.0[va as usize..va as usize + out.len()]) }
        fn write(&mut self, va: u64, d: &[u8]) { self.0[va as usize..va as usize + d.len()].copy_from_slice(d) }
    }

    #[test]
    fn lines_to_pitch_and_block_linear() {
        let mut ram = Ram(vec![0; 0x4000]);
        let mut p = Inline::default();
        for (m, v) in [(LINE_LENGTH, 6), (LINES, 2), (ADDRESS, 0), (ADDRESS + 4, 0x100), (PITCH, 16), (LAUNCH, 1)] {
            p.method(m as u32, v, &mut ram);
        }
        for v in [0x0403_0201, 0x0807_0605, 0x0c0b_0a09] {
            p.method(DATA as u32, v, &mut ram);
        }
        assert_eq!(ram.0[0x100..0x106], [1, 2, 3, 4, 5, 6]);
        assert_eq!(ram.0[0x110..0x116], [7, 8, 9, 10, 11, 12]);
        for (m, v) in [(LINE_LENGTH, 4), (LINES, 1), (ADDRESS + 4, 0x1000), (BLOCK, 1 << 4), (WIDTH, 128), (HEIGHT, 32), (ORIGIN_X, 70), (ORIGIN_Y, 9), (LAUNCH, 0)] {
            p.method(m as u32, v, &mut ram);
        }
        p.method(DATA as u32, 0xdead_beef, &mut ram);
        let at = 0x1000 + Block { width: 128, height: 32, gobs_w: 0, gobs_h: 1, gobs_d: 0 }.offset(70, 9, 0) as usize;
        assert_eq!(ram.0[at..at + 4], 0xdead_beefu32.to_le_bytes());
    }

    #[test]
    fn a_launch_keeps_its_size() {
        let mut ram = Ram(vec![0; 0x100]);
        let mut p = Inline::default();
        for (m, v) in [(LINE_LENGTH, 4), (LINES, 1), (ADDRESS + 4, 0x10), (LAUNCH, 1), (LINE_LENGTH, 8), (LINES, 100)] {
            p.method(m as u32, v, &mut ram);
        }
        p.method(DATA as u32, 0x0403_0201, &mut ram);
        assert_eq!(ram.0[0x10..0x18], [1, 2, 3, 4, 0, 0, 0, 0]);
        // more than can come
        for (m, v) in [(LINE_LENGTH, u32::MAX), (LINES, u32::MAX), (LAUNCH, 1), (DATA, 1)] {
            p.method(m as u32, v, &mut ram);
        }
        assert!(p.data.is_empty());
    }

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
        assert_eq!((b.offset(0, 8, 0), b.offset(0, 0, 1), b.offset(64, 0, 0)), (512, 2048, 4096));
    }
}
