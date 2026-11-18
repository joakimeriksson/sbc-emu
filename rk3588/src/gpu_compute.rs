//! Bounded scalar Valhall workgroup dispatch using the real SPD/FAU and preload layouts.
use crate::{
    gpu_cs::Stop,
    gpu_mmu::AddressSpace,
    gpu_shader::{RegisterAllocation, Shader},
};
fn pair(r: &[u32; 256], n: usize) -> u64 {
    r[n] as u64 | ((r[n + 1] as u64) << 32)
}
pub fn dispatch(
    m: &AddressSpace,
    ram: &mut [u8],
    r: &[u32; 256],
    instruction: u64,
) -> Result<(), Stop> {
    let unsupported = || Stop::Unsupported { pc: 0, opcode: 4 };
    if instruction & 0x00ff_ffff_ffff_ffff != 0
        || r[33] >> 30 != 0
        || r[34..37].iter().any(|&v| v != 0)
        || pair(r, 0) != 0
        || pair(r, 24) != 0
    {
        return Err(unsupported());
    }
    let local = [
        (r[33] & 1023) + 1,
        ((r[33] >> 10) & 1023) + 1,
        ((r[33] >> 20) & 1023) + 1,
    ];
    let groups = [r[37], r[38], r[39]];
    let local_count = local.iter().fold(1u64, |a, &b| a * b as u64);
    let total = local
        .iter()
        .chain(groups.iter())
        .try_fold(1u64, |a, &b| a.checked_mul(b as u64));
    if local_count > 1024 || !total.is_some_and(|v| v > 0 && v <= 65536) {
        return Err(unsupported());
    }
    let mut spd = [0; 32];
    m.read(ram, pair(r, 16), &mut spd, true)
        .map_err(Stop::Memory)?;
    let word = |n: usize| u32::from_le_bytes(spd[n * 4..n * 4 + 4].try_into().unwrap());
    if word(0) & !(3 << 30) != 0x18 || word(1) & !0x7f80 != 0 || spd[16..].iter().any(|&v| v != 0) {
        return Err(unsupported());
    }
    let allocation = match word(0) >> 30 {
        0 => RegisterAllocation::Registers64,
        2 => RegisterAllocation::Registers32,
        _ => return Err(unsupported()),
    };
    let preload = word(1);
    if matches!(allocation, RegisterAllocation::Registers32) && preload != 0 {
        return Err(unsupported());
    }
    let binary = word(2) as u64 | ((word(3) as u64) << 32);
    if binary & 7 != 0 {
        return Err(Stop::InvalidStream);
    }
    let fau = pair(r, 8);
    let count = (fau >> 56) as usize;
    if count > 128 || fau & 0x00ff000000000000 != 0 {
        return Err(unsupported());
    }
    let mut data = vec![0; count * 8];
    if count != 0 {
        m.read(ram, fau & 0xffffffffffff, &mut data, true)
            .map_err(Stop::Memory)?;
    }
    let uniforms: Vec<u32> = data
        .chunks_exact(4)
        .map(|b| u32::from_le_bytes(b.try_into().unwrap()))
        .collect();
    let mut remaining = 1 << 22; // aggregate instruction cap for this doorbell dispatch
    for gz in 0..groups[2] {
        for gy in 0..groups[1] {
            for gx in 0..groups[0] {
                for lz in 0..local[2] {
                    for ly in 0..local[1] {
                        for lx in 0..local[0] {
                            let mut shader = Shader::with_allocation(binary, allocation);
                            let values = [
                                lx | (ly << 16),
                                lz,
                                gx,
                                gy,
                                gz,
                                gx * local[0] + lx,
                                gy * local[1] + ly,
                                gz * local[2] + lz,
                            ];
                            for (n, &v) in values.iter().enumerate() {
                                if preload & (1 << (n + 7)) != 0 {
                                    shader.regs[n + 55] = v;
                                }
                            }
                            shader
                                .run(m, ram, &uniforms, remaining.min(4096))
                                .map_err(Stop::Shader)?;
                            remaining -= shader.instructions as usize;
                        }
                    }
                }
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn excessive_dispatch_is_rejected_without_writing_memory() {
        let m = AddressSpace {
            transtab: 0,
            transcfg: 2,
        };
        let mut ram = vec![0x55; 4096];
        let before = ram.clone();
        let mut regs = [0; 256];
        regs[37..40].copy_from_slice(&[65537, 1, 1]);
        assert!(matches!(
            dispatch(&m, &mut ram, &regs, 4 << 56),
            Err(Stop::Unsupported { opcode: 4, .. })
        ));
        assert_eq!(ram, before);
    }
    #[test]
    fn descriptor_allocation_controls_access_to_upper_registers() {
        for (allocation, success) in [(0u32, true), (2, false), (1, false), (3, false)] {
            let m = AddressSpace {
                transtab: 0,
                transcfg: 2,
            };
            let mut ram = vec![0; 4096];
            ram[..4].copy_from_slice(&(0x18 | (allocation << 30)).to_le_bytes());
            ram[8..16].copy_from_slice(&256u64.to_le_bytes());
            let code = [
                0x0091c00000000080u64,
                0x0091c10000000081,
                0x0091e00000000082,
                0x0061600218000000,
                0x7800c00000000000,
            ];
            for (n, i) in code.into_iter().enumerate() {
                ram[256 + n * 8..264 + n * 8].copy_from_slice(&i.to_le_bytes());
            }
            for (n, v) in [1024u32, 0, 42, 0].into_iter().enumerate() {
                ram[512 + n * 4..516 + n * 4].copy_from_slice(&v.to_le_bytes());
            }
            let mut regs = [0; 256];
            regs[8] = 512;
            regs[9] = 2 << 24;
            regs[37..40].fill(1);
            let result = dispatch(&m, &mut ram, &regs, 4 << 56);
            assert_eq!(
                result.is_ok(),
                success,
                "allocation {allocation}: {result:?}"
            );
            assert_eq!(
                u32::from_le_bytes(ram[1024..1028].try_into().unwrap()),
                if success { 42 } else { 0 }
            );
        }
    }
    #[test]
    fn three_dimensional_preloads_match_mesa_register_mapping() {
        let m = AddressSpace {
            transtab: 0,
            transcfg: 2,
        };
        let mut ram = vec![0; 4096];
        ram[..4].copy_from_slice(&0x18u32.to_le_bytes());
        ram[4..8].copy_from_slice(&0x7f80u32.to_le_bytes());
        ram[8..16].copy_from_slice(&256u64.to_le_bytes());
        ram[512..520].copy_from_slice(&1024u64.to_le_bytes());
        // Mesa-assembled MOV / STORE fixture, reading all compute ID registers.
        let code: [u64; 13] = [
            0x91c00000000080,
            0x91c10000000081,
            0x91c20000000037,
            0x91c30000000039,
            0x91c4000000003c,
            0x91c5000000003d,
            0x91c60000000038,
            0x91c7000000003a,
            0x91c8000000003b,
            0x91c9000000003e,
            0x61420838000000,
            0x61460838001000,
            0x7800c00000000000,
        ];
        for (n, i) in code.into_iter().enumerate() {
            ram[256 + n * 8..264 + n * 8].copy_from_slice(&i.to_le_bytes());
        }
        let mut regs = [0; 256];
        regs[8] = 512;
        regs[9] = 1 << 24;
        regs[33] = 2 | (1 << 10) | (1 << 20);
        regs[37..40].fill(2);
        dispatch(&m, &mut ram, &regs, 4 << 56).unwrap();
        let values: Vec<u32> = ram[1024..1056]
            .chunks_exact(4)
            .map(|b| u32::from_le_bytes(b.try_into().unwrap()))
            .collect();
        assert_eq!(values, [0x10002, 1, 5, 3, 1, 1, 1, 3]);
        // Shader preloads that this model does not implement must not be ignored.
        ram[4..8].copy_from_slice(&1u32.to_le_bytes());
        assert!(matches!(
            dispatch(&m, &mut ram, &regs, 4 << 56),
            Err(Stop::Unsupported { .. })
        ));
    }
}
