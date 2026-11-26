//! The copy engine (class B0B5, MAXWELL_DMA_COPY_A): lines between pitch and block-linear surfaces,
//! component remapping, and a semaphore at the end.
use super::mmu::View;
use maxwell::inline::Block;

#[derive(Default, Clone)]
pub struct Copy {
    regs: std::collections::HashMap<u32, u32>,
}

impl Copy {
    fn r(&self, m: u32) -> u32 {
        self.regs.get(&m).copied().unwrap_or(0)
    }

    pub fn method(&mut self, m: u32, v: u32, view: &mut View, now: u64) {
        self.regs.insert(m, v);
        if m == 0x300 {
            self.launch(v, view, now);
        }
    }

    fn launch(&mut self, v: u32, view: &mut View, now: u64) {
        let address = |hi: u32| ((self.r(hi) & 0xff) as u64) << 32 | self.r(hi + 4) as u64;
        if v & 3 != 0 {
            self.copy(v, address(0x400), address(0x408), view);
        }
        // a semaphore: one word, or four with the time
        let at = address(0x240);
        if crate::gpu::flag::sem() && v >> 3 & 3 != 0 { eprintln!("[sem] copy release {at:#x} = {:#x} (launch {v:#x})", self.r(0x248)); }
        match v >> 3 & 3 {
            1 => view.write32(at, self.r(0x248)),
            2 => {
                view.write32(at, self.r(0x248));
                view.write32(at + 4, 0);
                view.write(at + 8, &now.to_le_bytes());
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
        // an element's bytes; with remapping, widths and origins count elements
        let (sx, dx) = (size * from, size * to);
        let block = |base: u32, element: u64| {
            let b = self.r(base);
            let unit = if remap { element } else { 1 };
            let origin = self.r(base + 0x14);
            let b = Block { width: self.r(base + 4) as u64 * unit, height: self.r(base + 8) as u64, gobs_w: b & 15, gobs_h: b >> 4 & 15, gobs_d: b >> 8 & 15 };
            (b, (origin & 0xffff) as u64 * unit, (origin >> 16) as u64, self.r(base + 0x10) as u64)
        };
        let src_block = (v >> 7 & 1 == 0).then(|| block(0x728, sx));
        let dst_block = (v >> 8 & 1 == 0).then(|| block(0x70c, dx));
        // components that are not written keep what is there
        let keep = remap && (0..to as u32).any(|c| swizzle >> (4 * c) & 7 > 5);
        let mut budget = MAX_WORK;
        let (mut line, mut out) = (Vec::new(), Vec::new());
        for y in 0..lines {
            let line_at = |base: u64, pitch: i64, x: u64| base.wrapping_add(x).wrapping_add((y as i64).wrapping_mul(pitch) as u64);
            let mut done = 0;
            while done < length {
                let n = (length - done).min(CHUNK);
                line.resize((n * sx) as usize, 0);
                match src_block {
                    None => view.read(line_at(src, src_pitch, done * sx), &mut line),
                    Some((b, ox, oy, oz)) => b.pieces(ox + done * sx, oy + y, oz, n * sx, |o, at, k| view.read(src.wrapping_add(o), &mut line[at..at + k])),
                }
                let data = if remap {
                    out.resize((n * dx) as usize, 0);
                    if keep {
                        match dst_block {
                            None => view.read(line_at(dst, dst_pitch, done * dx), &mut out),
                            Some((b, ox, oy, oz)) => b.pieces(ox + done * dx, oy + y, oz, n * dx, |o, at, k| view.read(dst.wrapping_add(o), &mut out[at..at + k])),
                        }
                    }
                    let size = size as usize;
                    for (e, o) in line.chunks_exact(sx as usize).zip(out.chunks_exact_mut(dx as usize)) {
                        for (c, o) in o.chunks_exact_mut(size).enumerate() {
                            match (swizzle >> (4 * c) & 7) as usize {
                                k @ 0..=3 => match e.get(k * size..(k + 1) * size) {
                                    Some(e) => o.copy_from_slice(e),
                                    None => o.fill(0),
                                },
                                k @ (4 | 5) => o.copy_from_slice(&self.r(0x700 + 4 * (k as u32 - 4)).to_le_bytes()[..size]),
                                _ => {}
                            }
                        }
                    }
                    &out
                } else {
                    &line
                };
                match dst_block {
                    None => view.write(line_at(dst, dst_pitch, done * dx), data),
                    Some((b, ox, oy, oz)) => b.pieces(ox + done * dx, oy + y, oz, n * dx, |o, at, k| view.write(dst.wrapping_add(o), &data[at..at + k])),
                }
                done += n;
                budget -= ((n * sx.max(dx)) as i64).max(256);
                if budget < 0 {
                    break;
                }
            }
            // (a line costs something even if it is empty)
            budget -= 64;
            if budget < 0 {
                eprintln!("[gpu] a copy of {lines} lines of {length} was cut short");
                return;
            }
        }
    }
}

/// Elements copied at a time.
const CHUNK: u64 = 4096;

/// The most bytes a launch copies: about twice the memory there is.
const MAX_WORK: i64 = 8 << 30;

#[cfg(test)]
mod tests {
    use super::super::mmu::{Mmu, Space};
    use super::*;
    use crate::smmu::Smmu;

    /// A view of 64 KiB of memory at addresses 0x1000 up, through one page table.
    fn with(f: impl FnOnce(&mut View)) -> Vec<u8> {
        let mut ram = vec![0u8; 1 << 20];
        let base = crate::RAM_BASE;
        // directory at 0x80000, small page table at 0x90000: page k at RAM offset k << 12
        let put = |ram: &mut Vec<u8>, at: usize, v: u64| ram[at..at + 8].copy_from_slice(&v.to_le_bytes());
        let table = base + 0x9_0000;
        put(&mut ram, 0x8_0000, (table >> 12) << 36 | 1 << 32);
        for k in 1..17u64 {
            put(&mut ram, 0x9_0000 + 8 * k as usize, (base + (k << 12)) >> 8 | 1);
        }
        let (smmu, mmu) = (Smmu::default(), Mmu::default());
        let mut view = View { ram: &mut ram, smmu: &smmu, mmu: &mmu, space: Space { directory: base + 0x8_0000, big64: false }, fault: Default::default() };
        f(&mut view);
        ram
    }

    fn launch(c: &mut Copy, view: &mut View, regs: &[(u32, u32)], v: u32) {
        regs.iter().for_each(|&(m, v)| c.method(m, v, view, 0));
        c.method(0x300, v, view, 0);
    }

    const PITCH: u32 = 1 << 7 | 1 << 8 | 1;

    #[test]
    fn components_are_remapped_and_kept() {
        let ram = with(|view| {
            view.write(0x1000, &[1, 2, 3, 4, 5, 6, 7, 8]);
            view.write(0x2000, &[0xee; 8]);
            let mut c = Copy::default();
            // two components to four: y, x, (not written), the constant A
            let swizzle = 3 << 24 | 1 << 20 | 4 << 12 | 6 << 8 | 1;
            launch(&mut c, view, &[(0x400, 0), (0x404, 0x1000), (0x408, 0), (0x40c, 0x2000), (0x418, 2), (0x708, swizzle), (0x700, 0x99)], PITCH | 1 << 10);
            // a component the source does not have
            launch(&mut c, view, &[(0x40c, 0x2100), (0x418, 2), (0x708, 3)], PITCH | 1 << 10);
        });
        assert_eq!(ram[0x2000..0x2008], [2, 1, 0xee, 0x99, 4, 3, 0xee, 0x99]);
        assert_eq!(ram[0x2100..0x2103], [0, 0, 0]);
    }

    #[test]
    fn long_lines_and_endless_copies() {
        let ram = with(|view| {
            let data: Vec<u8> = (0..0x5000u32).map(|k| (k * 7) as u8).collect();
            view.write(0x1000, &data);
            let mut c = Copy::default();
            launch(&mut c, view, &[(0x404, 0x1000), (0x40c, 0x8000), (0x418, 0x5000)], PITCH);
            // nothing is mapped there: it ends all the same
            let start = std::time::Instant::now();
            launch(&mut c, view, &[(0x404, 0x100_0000), (0x40c, 0x200_0000), (0x410, 0x10), (0x414, 0x10), (0x418, 0x10_0000), (0x41c, u32::MAX)], PITCH | 1 << 9);
            assert!(start.elapsed().as_secs() < 120);
        });
        assert!((0..0x5000).all(|k| ram[0x8000 + k] == (k * 7) as u8));
    }
}
