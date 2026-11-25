//! Pitch-to-pitch copies on the Ampere DMA Copy B class (C7B5).
use super::mmu::View;

pub(super) const CLASS: u32 = 0xc7b5;

#[derive(Default)]
pub(super) struct Copy {
    semaphore: [u32; 3],
    operands: [u32; 8],
    remap: [u32; 3],
    pub timestamp: u64,
}

impl Copy {
    pub fn method(&mut self, method: u32, data: u32, memory: &mut View) -> Option<()> {
        match method {
            0x240..=0x248 => self.semaphore[((method - 0x240) / 4) as usize] = data,
            0x400..=0x41c => self.operands[((method - 0x400) / 4) as usize] = data,
            0x700..=0x708 => self.remap[((method - 0x700) / 4) as usize] = data,
            0x300 => return self.launch(data, memory),
            0x100 => {}
            _ => return None,
        }
        Some(())
    }

    fn launch(&self, flags: u32, memory: &mut View) -> Option<()> {
        const REMAP: u32 = 1 << 10;
        const SUPPORTED: u32 = 0x39f | REMAP;
        let semaphore_type = (flags >> 3) & 3;
        if flags & !SUPPORTED != 0 || flags & 3 == 3 || semaphore_type == 3 {
            return None;
        }
        let address = |hi: u32, lo: u32| u64::from(hi & 0x1ffff) << 32 | u64::from(lo);
        if flags & 3 != 0 {
            if flags & 0x180 != 0x180 {
                return None;
            }
            let [
                src_hi,
                src_lo,
                dst_hi,
                dst_lo,
                src_pitch,
                dst_pitch,
                width,
                height,
            ] = self.operands;
            let height = if flags & 0x200 != 0 { height } else { 1 };
            let (value, size) = if flags & REMAP != 0 {
                // CUDA memset selects a single destination component from CONST_A or CONST_B.
                let [a, b, components] = self.remap;
                let value = match components & !0x30000 {
                    4 => a,
                    5 => b,
                    _ => return None,
                };
                let size = 1 + (components >> 16 & 3);
                if !size.is_power_of_two() {
                    return None;
                }
                (value, size as usize)
            } else {
                (0, 1)
            };
            let bytes = u64::from(width) * size as u64;
            if height > 16384 || bytes * u64::from(height) > 16 * 1024 * 1024 {
                return None;
            }
            let mut src = address(src_hi, src_lo);
            let mut dst = address(dst_hi, dst_lo);
            for _ in 0..height {
                if flags & REMAP != 0 {
                    memory.fill(dst, bytes as usize, &value.to_le_bytes()[..size])?;
                } else {
                    memory.copy(src, dst, bytes as usize)?;
                }
                src = src.wrapping_add_signed(src_pitch as i32 as i64);
                dst = dst.wrapping_add_signed(dst_pitch as i32 as i64);
            }
        }
        if semaphore_type != 0 {
            let [hi, lo, payload] = self.semaphore;
            memory.release(
                address(hi, lo),
                payload,
                (semaphore_type == 2).then_some(self.timestamp),
            )?;
        }
        Some(())
    }
}
