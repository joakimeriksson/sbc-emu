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
    let mut f = [0; 128];
    m.read(ram, fbd, &mut f, true).map_err(Stop::Memory)?;
    if std::env::var_os("SBC_MALI_TRACE").is_some() {
        eprintln!(
            "[mali] fragment regs40..48={:x?} fbd={:x?}",
            &regs[40..48],
            f.chunks_exact(4)
                .map(|b| u32::from_le_bytes(b.try_into().unwrap()))
                .collect::<Vec<_>>()
        );
    }
    let unsupported = || Stop::Unsupported { pc: 0, opcode: 7 };
    let width = (word(&f, 8) & 65535) + 1;
    let height = (word(&f, 8) >> 16) + 1;
    if width > 4096
        || height > 4096
        || word(&f, 0) != 0
        || pair(&f, 14) != 0
        || word(&f, 11) & ((7 << 0) | (7 << 3) | (15 << 19)) != 0
        || word(&f, 12) & !((3 << 16) | (1 << 21) | (15 << 28)) != 0
    {
        return Err(unsupported());
    }
    // CRC reads may elide identical tiles on hardware. This synchronous clear
    // path always writes the pixels instead, preserving the same result.
    let has_crc = word(&f, 12) & (1 << 21) != 0;
    let mut extension = [0u8; 64];
    if has_crc {
        m.read(ram, fbd + 128, &mut extension, true)
            .map_err(Stop::Memory)?;
        // This path supports CRC-only extensions, with no depth/stencil target.
        if word(&extension, 0) != 0 || extension[24..].iter().any(|&b| b != 0) {
            return Err(unsupported());
        }
    } else if word(&f, 12) & (3 << 30) != 0 {
        return Err(unsupported());
    }
    let mut target = [0u8; 64];
    m.read(
        ram,
        fbd + if has_crc { 192 } else { 128 },
        &mut target,
        true,
    )
    .map_err(Stop::Memory)?;
    let rt = &target[..];
    let ctl = word(rt, 1);
    if word(rt, 0) != (1 << 26)
        || ctl & !((1 << 15) | (1 << 31)) != (1 | (19 << 3) | (2 << 8) | (0x688 << 16))
    {
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
    let mut crc_destinations = Vec::new();
    if word(&f, 12) & (3 << 30) != 0 {
        // The descriptor supplies the checksum for a uniform clear. Only whole
        // 16x16 checksum tiles are supported until partial-tile CRC is modeled.
        if x0 != 0
            || y0 != 0
            || x1 + 1 != width
            || y1 + 1 != height
            || width % 16 != 0
            || height % 16 != 0
        {
            return Err(unsupported());
        }
        let base = pair(&extension, 4);
        let stride = word(&extension, 1) as u64;
        let columns = width as usize / 16;
        if stride < (columns * 8) as u64 {
            return Err(Stop::InvalidStream);
        }
        for y in 0..height / 16 {
            let addr = base
                .checked_add(y as u64 * stride)
                .ok_or(Stop::Memory(Fault::AddressSize))?;
            if word(&f, 12) & (1 << 30) != 0 {
                m.chunks(ram, addr, columns * 8, crate::gpu_mmu::Access::Read, true)
                    .map_err(Stop::Memory)?;
            }
            if word(&f, 12) & (1 << 31) != 0 {
                crc_destinations.push(
                    m.chunks(ram, addr, columns * 8, crate::gpu_mmu::Access::Write, true)
                        .map_err(Stop::Memory)?,
                );
            }
        }
    }
    for spans in destinations {
        let mut offset = 0;
        for (p, len) in spans {
            ram[p..p + len].copy_from_slice(&row[offset..offset + len]);
            offset += len;
        }
    }
    let checksum = pair(&extension, 2).to_le_bytes();
    for spans in crc_destinations {
        let mut offset = 0;
        for (p, len) in spans {
            for byte in &mut ram[p..p + len] {
                *byte = checksum[offset % 8];
                offset += 1;
            }
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
    fn mesa_clear_handles_crc_extension_and_prevalidates_checksum_destination() {
        let (m, mut r, mut regs) = fixture();
        let target = r[128..192].to_vec();
        r[128..192].fill(0);
        r[192..256].copy_from_slice(&target);
        r[32..36].copy_from_slice(&0x000f000fu32.to_le_bytes());
        r[36..40].fill(0);
        r[40..44].copy_from_slice(&0x000f000fu32.to_le_bytes());
        r[44..48].copy_from_slice(&0x04001480u32.to_le_bytes());
        r[48..52].copy_from_slice(&0x80210000u32.to_le_bytes());
        r[132..136].copy_from_slice(&16u32.to_le_bytes());
        let checksum = 0xc0008020ffdf8020u64;
        r[136..144].copy_from_slice(&checksum.to_le_bytes());
        r[144..152].copy_from_slice(&6144u64.to_le_bytes());
        r[196..200].copy_from_slice(
            &(1u32 | (19 << 3) | (2 << 8) | (0x688 << 16) | (1 << 15) | (1 << 31)).to_le_bytes(),
        );
        r[232..236].copy_from_slice(&64u32.to_le_bytes());
        regs[42] = 0;
        regs[43] = 0x000f000f;
        fragment(&m, &mut r, &regs).unwrap();
        assert!(r[4096..5120]
            .chunks_exact(4)
            .all(|p| p == [0x11, 0x22, 0x33, 0xff]));
        assert_eq!(&r[6144..6152], &checksum.to_le_bytes());
        assert_eq!(&r[6152..6160], &[0x55; 8]);
        r[144..152].copy_from_slice(&8192u64.to_le_bytes());
        let before = r.clone();
        assert!(matches!(fragment(&m, &mut r, &regs), Err(Stop::Memory(_))));
        assert_eq!(r, before);
        // Even though CRC reads do not elide pixels, their memory access must
        // still fault when the checksum target is unmapped.
        r[48..52].copy_from_slice(&0x40210000u32.to_le_bytes());
        let before = r.clone();
        assert!(matches!(fragment(&m, &mut r, &regs), Err(Stop::Memory(_))));
        assert_eq!(r, before);
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
