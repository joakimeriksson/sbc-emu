//! Arm Frame Buffer Compression surfaces (16x16 superblocks).
//!
//! Each superblock has a 16-byte header: the offset of its payload from the start of the
//! headers, then sixteen 6-bit sizes of its 4x4 subblocks (1: stored uncompressed; a
//! first size of 0: the whole superblock is one colour, held in the header). Headers are
//! row-major or, with tiled headers, Morton-ordered within 8x8 groups of superblocks
//! (Mesa pan_resource.c, get_morton_index). Mesa never looks inside the payloads (it
//! blits through the GPU, and its packing shader only copies payloads by their header
//! sizes), so the model writes uncompressed subblocks and reads those or solid colours;
//! compressed subblocks from elsewhere fault.
use crate::gpu_mmu::Fault;

#[derive(Clone, Copy, Debug)]
pub struct Layout {
    /// The headers (payload offsets are relative to it).
    pub header: u64,
    /// Superblocks per header row.
    pub stride: u32,
    pub tiled: bool,
    /// Bytes per pixel.
    pub bpp: usize,
    /// Where uncompressed payloads go (sparse layout: one full-size slot per superblock).
    pub body_offset: u64,
}

#[derive(Debug, PartialEq, Eq)]
pub enum Error {
    Memory(Fault),
    Compressed,
}

impl Layout {
    fn index(&self, sx: u32, sy: u32) -> u64 {
        if self.tiled {
            let (x, y) = (sx, sy);
            let i = (x & 1) | ((y << 1) & 2) | ((x << 1) & 4) | ((y << 2) & 8) | ((x << 2) & 16) | ((y << 3) & 32);
            ((y & !7) * self.stride + ((x & !7) << 3) + i) as u64
        } else {
            (sy * self.stride + sx) as u64
        }
    }
    pub fn header_address(&self, sx: u32, sy: u32) -> u64 {
        self.header + 16 * self.index(sx, sy)
    }
    fn subblock(px: u32, py: u32) -> usize {
        ((py / 4) * 4 + px / 4) as usize
    }
    /// Read the pixel at (x, y): `bpp` bytes into `out`.
    pub fn read_pixel(
        &self,
        read: &mut impl FnMut(u64, &mut [u8]) -> Result<(), Fault>,
        x: u32,
        y: u32,
        out: &mut [u8],
    ) -> Result<(), Error> {
        let mut h = [0u8; 16];
        read(self.header_address(x / 16, y / 16), &mut h).map_err(Error::Memory)?;
        let bits = u128::from_le_bytes(h);
        let size = |n: usize| ((bits >> (32 + 6 * n)) & 63) as u64;
        let (px, py) = (x % 16, y % 16);
        if size(0) == 0 {
            // Solid colour: the pixel value follows the offset word.
            out.copy_from_slice(&h[8..8 + self.bpp]);
            return Ok(());
        }
        let k = Self::subblock(px, py);
        let raw = 16 * self.bpp as u64;
        let mut offset = (bits as u32) as u64;
        for n in 0..k {
            offset += match size(n) {
                1 => raw,
                _ => return Err(Error::Compressed),
            };
        }
        if size(k) != 1 {
            return Err(Error::Compressed);
        }
        let within = ((py % 4) * 4 + px % 4) as u64 * self.bpp as u64;
        read(self.header + offset + within, out).map_err(Error::Memory)
    }
    /// Read a whole superblock's pixels, row-major (256 * bpp bytes).
    pub fn read_superblock(
        &self,
        read: &mut impl FnMut(u64, &mut [u8]) -> Result<(), Fault>,
        sx: u32,
        sy: u32,
    ) -> Result<Vec<u8>, Error> {
        let mut pixels = vec![0u8; 256 * self.bpp];
        for py in 0..16 {
            for px in 0..16 {
                let at = ((py * 16 + px) as usize) * self.bpp;
                self.read_pixel(read, sx * 16 + px, sy * 16 + py, &mut pixels[at..at + self.bpp])?;
            }
        }
        Ok(pixels)
    }
    /// Encode a superblock (row-major pixels) as sixteen uncompressed subblocks:
    /// (header address, header, payload address, payload).
    pub fn encode(&self, sx: u32, sy: u32, pixels: &[u8]) -> (u64, [u8; 16], u64, Vec<u8>) {
        let slot = self.body_offset + self.index(sx, sy) * 256 * self.bpp as u64;
        let mut bits = slot as u32 as u128;
        for n in 0..16 {
            bits |= 1u128 << (32 + 6 * n);
        }
        let mut body = Vec::with_capacity(256 * self.bpp);
        for k in 0..16u32 {
            let (bx, by) = ((k % 4) * 4, (k / 4) * 4);
            for py in by..by + 4 {
                let at = ((py * 16 + bx) as usize) * self.bpp;
                body.extend_from_slice(&pixels[at..at + 4 * self.bpp]);
            }
        }
        (self.header_address(sx, sy), bits.to_le_bytes(), self.header + slot, body)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn uncompressed_superblocks_round_trip_and_solid_colours_read_back() {
        let mut mem = vec![0u8; 0x10000];
        for tiled in [false, true] {
            let l = Layout { header: 0x100, stride: 8, tiled, bpp: 4, body_offset: 0x1000 };
            let pixels: Vec<u8> = (0..1024u32).map(|n| (n * 7 + tiled as u32) as u8).collect();
            let (ha, h, ba, body) = l.encode(3, 2, &pixels);
            mem[ha as usize..ha as usize + 16].copy_from_slice(&h);
            mem[ba as usize..ba as usize + body.len()].copy_from_slice(&body);
            let mut read = |a: u64, out: &mut [u8]| {
                out.copy_from_slice(&mem[a as usize..a as usize + out.len()]);
                Ok(())
            };
            assert_eq!(l.read_superblock(&mut read, 3, 2).unwrap(), pixels);
        }
        // A solid superblock: first size 0, the colour after the offset.
        let l = Layout { header: 0, stride: 1, tiled: false, bpp: 4, body_offset: 64 };
        let mut h = [0u8; 16];
        h[8..12].copy_from_slice(&[1, 2, 3, 4]);
        let mut read = |a: u64, out: &mut [u8]| {
            out.copy_from_slice(&h[a as usize..a as usize + out.len()]);
            Ok(())
        };
        let mut px = [0u8; 4];
        l.read_pixel(&mut read, 5, 9, &mut px).unwrap();
        assert_eq!(px, [1, 2, 3, 4]);
    }
    #[test]
    fn tiled_headers_follow_mesas_morton_order() {
        let l = Layout { header: 0, stride: 16, tiled: true, bpp: 4, body_offset: 0 };
        // Mesa get_morton_index(1, 0) = 1, (0, 1) = 2, (2, 0) = 4, (8, 0) = 64, (0, 8) = 128.
        assert_eq!([l.index(1, 0), l.index(0, 1), l.index(2, 0), l.index(8, 0), l.index(0, 8)], [1, 2, 4, 64, 128]);
    }
}
