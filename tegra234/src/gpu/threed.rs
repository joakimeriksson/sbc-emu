//! Initial Ampere 3D (C797) color-target clear methods.
//! The render-target format and surface layout are shared with Maxwell.
use super::mmu::View;
use maxwell_tex::{MsMode, RtFormat, Surface};

pub(super) const CLASS: u32 = 0xc797;
const TARGET_BASE: u32 = 0x800;
const SCREEN_HORIZONTAL: u32 = 0xff4;
const SCREEN_VERTICAL: u32 = 0xff8;
const CLEAR_COLOR: u32 = 0xd80;
const CLEAR_CONTROL: u32 = 0x10f8;
const TARGET_CONTROL: u32 = 0x121c;
const CLEAR_SURFACE: u32 = 0x19d0;

#[derive(Default)]
pub(super) struct ThreeD {
    target: [u32; 9],
    screen: [u32; 2],
    color: [u32; 4],
    target_control: u32,
    clear_control: u32,
}

impl ThreeD {
    pub fn method(&mut self, method: u32, value: u32, memory: &mut View) -> Option<()> {
        match method {
            0x100 | 0x110 => {}
            TARGET_BASE..=0x820 => self.target[((method - TARGET_BASE) / 4) as usize] = value,
            SCREEN_HORIZONTAL | SCREEN_VERTICAL => {
                self.screen[((method - SCREEN_HORIZONTAL) / 4) as usize] = value;
            }
            CLEAR_COLOR..=0xd8c => self.color[((method - CLEAR_COLOR) / 4) as usize] = value,
            TARGET_CONTROL => self.target_control = value,
            CLEAR_CONTROL => self.clear_control = value,
            CLEAR_SURFACE => self.clear(value, memory)?,
            _ => return None,
        }
        Some(())
    }

    fn clear(&self, request: u32, memory: &mut View) -> Option<()> {
        // One color attachment and layer. The request selects color channels.
        if request & !0x3c != 0 || self.target_control != 1 || self.clear_control != 0 {
            return None;
        }
        let [hi, lo, width, height, format, mode, depth, _, base_layer] = self.target;
        let pitch = mode == 1 << 12;
        let block_linear = mode & !0xf0 == 0 && mode >> 4 <= 5;
        if !(pitch || block_linear) || depth != 1 || base_layer != 0 || lo & 3 != 0 {
            return None;
        }
        let format = RtFormat::from_code(format as u8)?;
        if format.bytes != 4 || (pitch && width & 3 != 0) {
            return None;
        }
        let target = Surface::new(
            u64::from(hi & 0xff) << 32 | u64::from(lo),
            width,
            height,
            mode,
            0,
            4,
            MsMode::X1,
        );
        let [horizontal, vertical] = self.screen;
        let (x, y) = (horizontal & 0xffff, vertical & 0xffff);
        let x_end = (x + (horizontal >> 16)).min(target.width);
        let y_end = (y + (vertical >> 16)).min(target.height);
        if u64::from(x_end.saturating_sub(x)) * u64::from(y_end.saturating_sub(y)) > 16_777_216 {
            return None;
        }
        let pixel = u32::from_le_bytes(format.encode(&self.color)[..4].try_into().ok()?);
        let channels = std::array::from_fn(|index| request & (4 << index) != 0);
        let mask = u32::from_le_bytes(format.write_mask(channels)[..4].try_into().ok()?);
        if mask == 0 {
            return Some(());
        }
        for row in y..y_end {
            for column in x..x_end {
                let address = target.sample_address(column, row, 0, 0);
                let value = if mask == u32::MAX {
                    pixel
                } else {
                    (pixel & mask) | (memory.load(address)? & !mask)
                };
                memory.store(address, value)?;
            }
        }
        Some(())
    }
}
