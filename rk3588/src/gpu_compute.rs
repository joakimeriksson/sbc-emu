//! Initial single-invocation Valhall compute dispatch, using the real SPD/FAU layout.
use crate::{gpu_cs::Stop, gpu_mmu::AddressSpace, gpu_shader::Shader};
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
        || r[33..37].iter().any(|&v| v != 0)
        || r[37..40] != [1, 1, 1]
        || pair(r, 0) != 0
        || pair(r, 24) != 0
    {
        return Err(unsupported());
    }
    let mut spd = [0; 32];
    m.read(ram, pair(r, 16), &mut spd, true)
        .map_err(Stop::Memory)?;
    let word = |n: usize| u32::from_le_bytes(spd[n * 4..n * 4 + 4].try_into().unwrap());
    if word(0) != 0x18 || word(1) != 0 || spd[16..].iter().any(|&v| v != 0) {
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
    Shader::new(binary)
        .run(m, ram, &uniforms, 4096)
        .map_err(Stop::Shader)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn multiple_invocations_are_rejected_without_writing_memory() {
        let m = AddressSpace {
            transtab: 0,
            transcfg: 2,
        };
        let mut ram = vec![0x55; 4096];
        let before = ram.clone();
        let mut regs = [0; 256];
        regs[37..40].copy_from_slice(&[2, 1, 1]);
        assert!(matches!(
            dispatch(&m, &mut ram, &regs, 4 << 56),
            Err(Stop::Unsupported { opcode: 4, .. })
        ));
        assert_eq!(ram, before);
    }
}
