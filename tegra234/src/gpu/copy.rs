//! Pitch-to-pitch copies on the Ampere DMA Copy B class (C7B5).
use super::mmu::View;

pub(super) const CLASS: u32 = 0xc7b5;

#[derive(Default)]
pub(super) struct Copy {
    semaphore: [u32; 3],
    operands: [u32; 8],
}

impl Copy {
    pub fn method(&mut self, method: u32, data: u32, memory: &mut View) -> Option<()> {
        match method {
            0x240..=0x248 => self.semaphore[((method - 0x240) / 4) as usize] = data,
            0x400..=0x41c => self.operands[((method - 0x400) / 4) as usize] = data,
            0x300 => return self.launch(data, memory),
            0x100 => {}
            _ => return None,
        }
        Some(())
    }

    fn launch(&self, flags: u32, memory: &mut View) -> Option<()> {
        const SUPPORTED: u32 = 0x39f;
        if flags & !SUPPORTED != 0 || flags & 3 == 3 || (flags >> 3) & 3 > 1 {
            return None;
        }
        let address = |hi: u32, lo: u32| u64::from(hi & 0x1ffff) << 32 | u64::from(lo);
        if flags & 3 != 0 {
            if flags & 0x180 != 0x180 {
                return None;
            }
            let [src_hi, src_lo, dst_hi, dst_lo, src_pitch, dst_pitch, width, height] =
                self.operands;
            let height = if flags & 0x200 != 0 { height } else { 1 };
            if width as u64 * height as u64 > 16 * 1024 * 1024 || height > 16384 {
                return None;
            }
            let mut src = address(src_hi, src_lo);
            let mut dst = address(dst_hi, dst_lo);
            for _ in 0..height {
                memory.copy(src, dst, width as usize)?;
                src = src.wrapping_add_signed(src_pitch as i32 as i64);
                dst = dst.wrapping_add_signed(dst_pitch as i32 as i64);
            }
        }
        if flags & 8 != 0 {
            let [hi, lo, payload] = self.semaphore;
            memory.store(address(hi, lo), payload)?;
        }
        Some(())
    }
}
