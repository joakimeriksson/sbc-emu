//! The Ampere DMA copy class (C7B5): lines between pitch and block-linear surfaces, component
//! remapping, and a semaphore at the end. The method layout is unchanged from the older
//! Jetson's B0B5, and so is the block-linear addressing.
use super::mmu::View;
use maxwell::inline::Block;

pub(super) const CLASS: u32 = 0xc7b5;
const REMAP: u32 = 1 << 10;
/// Transfer type, flush, semaphore, interrupt, layouts, multi-line, remap and
/// RMW-disable. Physical addressing and semaphore reductions are not modeled.
const SUPPORTED: u32 = 0xfff | 1 << 25 | 1 << 26;
/// The most bytes one launch moves.
const MAX_BYTES: u64 = 256 << 20;

#[derive(Default)]
pub(super) struct Copy {
    regs: Vec<u32>,
    pub timestamp: u64,
}

impl Copy {
    fn r(&self, method: u32) -> u32 {
        self.regs.get(method as usize / 4).copied().unwrap_or(0)
    }

    pub fn method(&mut self, method: u32, data: u32, memory: &mut View) -> Option<()> {
        match method {
            0x100 => return Some(()),
            0x240..=0x248 | 0x400..=0x41c | 0x700..=0x750 => {}
            0x300 => return self.launch(data, memory),
            _ => return None,
        }
        let index = method as usize / 4;
        if self.regs.len() <= index {
            self.regs.resize(index + 1, 0);
        }
        self.regs[index] = data;
        Some(())
    }

    fn address(&self, method: u32) -> u64 {
        u64::from(self.r(method) & 0x1ffff) << 32 | u64::from(self.r(method + 4))
    }

    fn launch(&self, flags: u32, memory: &mut View) -> Option<()> {
        let semaphore_type = (flags >> 3) & 3;
        if flags & !SUPPORTED != 0 || flags & 3 == 3 || semaphore_type == 3 {
            return None;
        }
        if flags & 3 != 0 {
            self.copy(flags, memory)?;
        }
        if semaphore_type != 0 {
            memory.release(
                self.address(0x240),
                self.r(0x248),
                (semaphore_type == 2).then_some(self.timestamp),
            )?;
        }
        Some(())
    }

    fn copy(&self, flags: u32, memory: &mut View) -> Option<()> {
        let (src, dst) = (self.address(0x400), self.address(0x408));
        let (src_pitch, dst_pitch) = (self.r(0x410) as i32 as i64, self.r(0x414) as i32 as i64);
        let length = u64::from(self.r(0x418));
        let lines = if flags & (1 << 9) != 0 {
            u64::from(self.r(0x41c))
        } else {
            1
        };
        // With remapping, lengths and origins count elements of `size`-byte components.
        let remap = flags & REMAP != 0;
        let swizzle = self.r(0x708);
        let (size, from, to) = if remap {
            (
                u64::from(swizzle >> 16 & 3) + 1,
                u64::from(swizzle >> 20 & 3) + 1,
                u64::from(swizzle >> 24 & 3) + 1,
            )
        } else {
            (1, 1, 1)
        };
        let (source_element, destination_element) = (size * from, size * to);
        // (a line on its own too: with no lines the product is 0, but a line is still allocated)
        let line_bytes = length.checked_mul(source_element.max(destination_element))?;
        if line_bytes > MAX_BYTES || line_bytes.checked_mul(lines.max(1))? > MAX_BYTES {
            return None;
        }
        // Origins: the packed SET_*_ORIGIN of older classes plus the 32-bit *_ORIGIN_X/Y.
        let block = |base: u32, element: u64| {
            let shape = self.r(base);
            let unit = if remap { element } else { 1 };
            let origin = self.r(base + 0x14);
            let wide = if base == 0x728 { 0x744 } else { 0x74c };
            let origin_x = u64::from(origin & 0xffff) + u64::from(self.r(wide));
            let origin_y = u64::from(origin >> 16) + u64::from(self.r(wide + 4));
            let block = Block {
                width: u64::from(self.r(base + 4)) * unit,
                height: u64::from(self.r(base + 8)),
                gobs_w: shape & 15,
                gobs_h: shape >> 4 & 15,
                gobs_d: shape >> 8 & 15,
            };
            (
                block,
                origin_x * unit,
                origin_y,
                u64::from(self.r(base + 0x10)),
            )
        };
        let src_block = (flags & (1 << 7) == 0).then(|| block(0x728, source_element));
        let dst_block = (flags & (1 << 8) == 0).then(|| block(0x70c, destination_element));
        // Components that select neither a source nor a constant keep the destination's.
        let keep = remap && (0..to as u32).any(|c| swizzle >> (4 * c) & 7 > 5);
        // A memset remaps constants only and has no source to read.
        let read_source = !remap || (0..to as u32).any(|c| swizzle >> (4 * c) & 7 <= 3);
        let mut line = vec![0; (length * source_element) as usize];
        let mut out = vec![0; (length * destination_element) as usize];
        for y in 0..lines {
            let pitch_at =
                |base: u64, pitch: i64| base.wrapping_add((y as i64).wrapping_mul(pitch) as u64);
            match src_block {
                _ if !read_source => {}
                None => memory.read(pitch_at(src, src_pitch), &mut line)?,
                Some((block, ox, oy, oz)) => {
                    let mut fault = false;
                    block.pieces(ox, oy + y, oz, line.len() as u64, |offset, at, n| {
                        fault |= memory
                            .read(src.wrapping_add(offset), &mut line[at..at + n])
                            .is_none();
                    });
                    if fault {
                        return None;
                    }
                }
            }
            let data = if remap {
                if keep {
                    match dst_block {
                        None => memory.read(pitch_at(dst, dst_pitch), &mut out)?,
                        Some((block, ox, oy, oz)) => {
                            let mut fault = false;
                            block.pieces(ox, oy + y, oz, out.len() as u64, |offset, at, n| {
                                fault |= memory
                                    .read(dst.wrapping_add(offset), &mut out[at..at + n])
                                    .is_none();
                            });
                            if fault {
                                return None;
                            }
                        }
                    }
                }
                let size = size as usize;
                for (element, output) in line
                    .chunks_exact(source_element as usize)
                    .zip(out.chunks_exact_mut(destination_element as usize))
                {
                    for (component, output) in output.chunks_exact_mut(size).enumerate() {
                        match (swizzle >> (4 * component) & 7) as usize {
                            k @ 0..=3 => match element.get(k * size..(k + 1) * size) {
                                Some(value) => output.copy_from_slice(value),
                                None => output.fill(0),
                            },
                            k @ (4 | 5) => output.copy_from_slice(
                                &self.r(0x700 + 4 * (k as u32 - 4)).to_le_bytes()[..size],
                            ),
                            _ => {}
                        }
                    }
                }
                &out
            } else {
                &line
            };
            match dst_block {
                None => memory.write(pitch_at(dst, dst_pitch), data)?,
                Some((block, ox, oy, oz)) => {
                    let mut fault = false;
                    block.pieces(ox, oy + y, oz, data.len() as u64, |offset, at, n| {
                        fault |= memory
                            .write(dst.wrapping_add(offset), &data[at..at + n])
                            .is_none();
                    });
                    if fault {
                        return None;
                    }
                }
            }
        }
        Some(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::RAM_BASE;

    /// GPU addresses 0..64 KiB on one big page at RAM offset 0x20000.
    fn ram() -> Vec<u8> {
        let mut ram = vec![0; 0x30000];
        let encode = |physical: u64, flags: u64| ((physical >> 4) | flags).to_le_bytes();
        ram[0..8].copy_from_slice(&encode(RAM_BASE + 0x1000, 4));
        ram[0x1000..0x1008].copy_from_slice(&encode(RAM_BASE + 0x2000, 4));
        ram[0x2000..0x2008].copy_from_slice(&encode(RAM_BASE + 0x3000, 4));
        ram[0x3000..0x3008].copy_from_slice(&encode(RAM_BASE + 0x4100, 4));
        ram[0x4100..0x4108].copy_from_slice(&encode(RAM_BASE + 0x20000, 5));
        ram
    }

    fn run(ram: &mut [u8], methods: &[(u32, u32)]) {
        let mut view = View::new(ram, RAM_BASE);
        let mut copy = Copy::default();
        for &(method, data) in methods {
            copy.method(method, data, &mut view).unwrap();
        }
    }

    #[test]
    fn lines_go_into_a_block_linear_surface_and_back() {
        let mut ram = ram();
        for (i, byte) in ram[0x20000..0x20100].iter_mut().enumerate() {
            *byte = i as u8;
        }
        // Two 128-byte lines from pitch 0x0 into a 256-byte-wide block-linear surface at 0x1000
        // (one-GOB blocks), at x = 64, y = 3, then back out to pitch 0x2000.
        let block = [
            (0x70c, 0),
            (0x710, 256),
            (0x714, 16),
            (0x718, 1),
            (0x71c, 0),
            (0x720, 3 << 16 | 64),
        ];
        let mut methods = vec![(0x400, 0), (0x404, 0), (0x408, 0), (0x40c, 0x1000)];
        methods.extend(block);
        methods.extend([
            (0x410, 128),
            (0x418, 128),
            (0x41c, 2),
            (0x300, 0x82 | 1 << 9),
        ]);
        run(&mut ram, &methods);
        // Byte (64, 3) of the GOB: the upper 32-byte half, line 3.
        let gob = maxwell::inline::gob_offset(64 % 64, 3) + 512;
        assert_eq!(ram[0x21000 + gob as usize], 0);
        assert_eq!(ram[0x21000 + gob as usize + 1], 1);
        let mut back = vec![(0x400, 0), (0x404, 0x1000), (0x408, 0), (0x40c, 0x2000)];
        back.extend(block.map(|(method, data)| (method + 0x1c, data)));
        back.extend([
            (0x414, 128),
            (0x418, 128),
            (0x41c, 2),
            (0x300, 0x102 | 1 << 9),
        ]);
        run(&mut ram, &back);
        assert_eq!(ram[0x22000..0x22100], ram[0x20000..0x20100]);
    }

    #[test]
    fn a_memset_reads_no_source() {
        let mut ram = ram();
        // Source 0xdead_0000 is unmapped. Four-byte components from CONST_A, two per element.
        run(
            &mut ram,
            &[
                (0x400, 0),
                (0x404, 0xdead_0000),
                (0x408, 0),
                (0x40c, 0x3000),
                (0x700, 0x1234_5678),
                (0x708, 0x0103_0044),
                (0x418, 4),
                (0x300, 0x186 | REMAP),
            ],
        );
        let words: Vec<u32> = ram[0x23000..0x23020]
            .chunks_exact(4)
            .map(|word| u32::from_le_bytes(word.try_into().unwrap()))
            .collect();
        assert_eq!(words, [0x1234_5678; 8]);
    }
}
