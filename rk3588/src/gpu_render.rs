//! Initial Valhall fragment pass: single linear RGBA8 color-target clears.
//! Geometry, shaders, compression, depth/stencil and MSAA are rejected.
use crate::{
    gpu_cs::Stop,
    gpu_mmu::{AddressSpace, Fault},
};
fn word(b: &[u8], n: usize) -> u32 {
    u32::from_le_bytes(b[n * 4..n * 4 + 4].try_into().unwrap())
}
fn pair(b: &[u8], n: usize) -> u64 {
    word(b, n) as u64 | (word(b, n + 1) as u64) << 32
}
pub fn fragment(m: &AddressSpace, ram: &mut [u8], regs: &[u32; 256]) -> Result<(), Stop> {
    let fbd = (regs[40] as u64 | ((regs[41] as u64) << 32)) & !63;
    let mut f = [0; 192];
    m.read(ram, fbd, &mut f, true).map_err(Stop::Memory)?;
    let unsupported = || Stop::Unsupported { pc: 0, opcode: 7 };
    let width = (word(&f, 8) & 65535) + 1;
    let height = (word(&f, 8) >> 16) + 1;
    if width > 4096
        || height > 4096
        || word(&f, 0) & 0x1ff != 0
        || pair(&f, 14) != 0
        || word(&f, 11) & 0x7fffff != 0
        || word(&f, 12) & !(1 << 29) != 0
        || word(&f, 12) & (1 << 29) == 0
    {
        return Err(unsupported());
    }
    let rt = &f[128..];
    let ctl = word(rt, 1);
    if word(rt, 0) != (1 << 26) || ctl != (1 | (19 << 3) | (2 << 8) | (0x688 << 16)) {
        return Err(unsupported());
    }
    let base = pair(rt, 8);
    let stride = word(rt, 10) as u64;
    if stride < (width as u64) * 4 {
        return Err(Stop::InvalidStream);
    }
    let min = word(&f, 9);
    let max = word(&f, 10);
    let x0 = (min & 65535).max(regs[42] & 65535);
    let y0 = (min >> 16).max(regs[42] >> 16);
    let x1 = (max & 65535).min(regs[43] & 65535);
    let y1 = (max >> 16).min(regs[43] >> 16);
    if x0 > x1 || y0 > y1 || x1 >= width || y1 >= height {
        return Err(Stop::InvalidStream);
    }
    let color = word(rt, 12).to_le_bytes();
    let mut row = Vec::with_capacity((x1 - x0 + 1) as usize * 4);
    for _ in x0..=x1 {
        row.extend_from_slice(&color);
    }
    if (13..16).any(|i| word(rt, i) != word(rt, 12)) {
        return Err(unsupported());
    }
    // Keep physical spans, not a host allocation proportional to every pixel.
    let mut destinations = Vec::new();
    for y in y0..=y1 {
        let addr = base
            .checked_add(y as u64 * stride + x0 as u64 * 4)
            .ok_or(Stop::Memory(Fault::AddressSize))?;
        let spans = m
            .chunks(ram, addr, row.len(), crate::gpu_mmu::Access::Write, true)
            .map_err(Stop::Memory)?;
        destinations.push(spans);
    }
    for spans in destinations {
        let mut offset = 0;
        for (p, len) in spans {
            ram[p..p + len].copy_from_slice(&row[offset..offset + len]);
            offset += len;
        }
    }
    Ok(())
}
#[cfg(test)]
mod tests {
    use super::*;
    fn fixture() -> (AddressSpace, Vec<u8>, [u32; 256]) {
        let mut r = vec![0x55; 8192];
        let mut f = [0u32; 48];
        f[8] = 3 | (3 << 16);
        f[9] = 1 | (1 << 16);
        f[10] = 2 | (2 << 16);
        f[12] = 1 << 29;
        f[32] = 1 << 26;
        f[33] = 1 | (19 << 3) | (2 << 8) | (0x688 << 16);
        f[40] = 4096;
        f[42] = 16;
        f[44..48].fill(0xff332211);
        for (i, v) in f.iter().enumerate() {
            r[i * 4..i * 4 + 4].copy_from_slice(&v.to_le_bytes());
        }
        let mut regs = [0; 256];
        regs[43] = 3 | (3 << 16);
        (
            AddressSpace {
                transtab: 0,
                transcfg: 2,
            },
            r,
            regs,
        )
    }
    #[test]
    fn fragment_clear_respects_bounds_and_stride() {
        let (m, mut r, regs) = fixture();
        fragment(&m, &mut r, &regs).unwrap();
        for y in 0..4 {
            for x in 0..4 {
                let expected = if (1..=2).contains(&x) && (1..=2).contains(&y) {
                    [0x11, 0x22, 0x33, 0xff]
                } else {
                    [0x55; 4]
                };
                assert_eq!(&r[4096 + y * 16 + x * 4..4100 + y * 16 + x * 4], &expected);
            }
        }
    }
    #[test]
    fn unsupported_geometry_and_bad_destination_leave_memory_unchanged() {
        let (m, mut r, regs) = fixture();
        let before = r.clone();
        r[56] = 1;
        assert!(fragment(&m, &mut r, &regs).is_err());
        assert_eq!(&r[4096..], &before[4096..]);
        r[56] = 0;
        r[160..164].copy_from_slice(&u32::MAX.to_le_bytes());
        assert!(fragment(&m, &mut r, &regs).is_err());
        assert_eq!(&r[4096..], &before[4096..]);
    }
}
