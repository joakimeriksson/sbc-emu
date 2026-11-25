//! Initial Ampere 3D (C797) color and depth/stencil clear methods.
//! The render-target format and surface layout are shared with Maxwell.
use super::mmu::View;
use maxwell_tex::{MsMode, RtFormat, Surface, ZetaFormat};

pub(super) const CLASS: u32 = 0xc797;
const TARGET_BASE: u32 = 0x800;
const TARGET_STRIDE: u32 = 0x40;
const SCREEN_HORIZONTAL: u32 = 0xff4;
const SCREEN_VERTICAL: u32 = 0xff8;
const CLEAR_COLOR: u32 = 0xd80;
const CLEAR_DEPTH: u32 = 0xd90;
const CLEAR_STENCIL: u32 = 0xda0;
const ZETA_BASE: u32 = 0xfe0;
const ZETA_SIZE: u32 = 0x1228;
const ZETA_ENABLE: u32 = 0x1538;
const ZETA_BASE_LAYER: u32 = 0x179c;
const CLEAR_CONTROL: u32 = 0x10f8;
const TARGET_CONTROL: u32 = 0x121c;
const CLEAR_SURFACE: u32 = 0x19d0;

#[derive(Default)]
pub(super) struct ThreeD {
    targets: [[u32; 9]; 8],
    zeta: [u32; 5],
    zeta_size: [u32; 3],
    zeta_enabled: bool,
    zeta_base_layer: u32,
    screen: [u32; 2],
    color: [u32; 4],
    depth: f32,
    stencil: u8,
    target_control: u32,
    clear_control: u32,
}

impl ThreeD {
    pub fn method(&mut self, method: u32, value: u32, memory: &mut View) -> Option<()> {
        match method {
            0x100 | 0x110 => {}
            TARGET_BASE..=0x9ff => {
                let target = ((method - TARGET_BASE) / TARGET_STRIDE) as usize;
                let reg = ((method - TARGET_BASE) % TARGET_STRIDE / 4) as usize;
                *self.targets[target].get_mut(reg)? = value;
            }
            SCREEN_HORIZONTAL | SCREEN_VERTICAL => {
                self.screen[((method - SCREEN_HORIZONTAL) / 4) as usize] = value;
            }
            CLEAR_COLOR..=0xd8c => self.color[((method - CLEAR_COLOR) / 4) as usize] = value,
            CLEAR_DEPTH => self.depth = f32::from_bits(value),
            CLEAR_STENCIL => self.stencil = value as u8,
            ZETA_BASE..=0xff0 => self.zeta[((method - ZETA_BASE) / 4) as usize] = value,
            ZETA_SIZE..=0x1230 => self.zeta_size[((method - ZETA_SIZE) / 4) as usize] = value,
            ZETA_ENABLE => self.zeta_enabled = value & 1 != 0,
            ZETA_BASE_LAYER => self.zeta_base_layer = value & 0xffff,
            TARGET_CONTROL => self.target_control = value,
            CLEAR_CONTROL => self.clear_control = value,
            CLEAR_SURFACE => self.clear(value, memory)?,
            _ => return None,
        }
        Some(())
    }

    fn clear(&self, request: u32, memory: &mut View) -> Option<()> {
        if request >> 26 != 0 || self.clear_control != 0 {
            return None;
        }
        let layer = request >> 10 & 0xffff;
        if request & 0x3c != 0 {
            self.clear_color(request, layer, memory)?;
        }
        if request & 3 != 0 && self.zeta_enabled {
            self.clear_zeta(request, layer, memory)?;
        }
        Some(())
    }

    fn clear_color(&self, request: u32, layer: u32, memory: &mut View) -> Option<()> {
        let output = request >> 6 & 0xf;
        if output >= (self.target_control & 0xf).min(8) {
            return Some(());
        }
        let slot = (self.target_control >> (4 + 3 * output) & 7) as usize;
        let [
            hi,
            lo,
            width,
            height,
            format,
            mode,
            depth,
            stride,
            base_layer,
        ] = self.targets[slot];
        let layer = layer + (base_layer & 0xffff);
        let pitch = mode == 1 << 12;
        let block_linear = mode & !0xf0 == 0 && mode >> 4 <= 5;
        if !(pitch || block_linear)
            || layer >= (depth & 0xffff)
            || lo & 3 != 0
            || (layer > 0 && stride == 0)
        {
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
            u64::from(stride) * 4,
            4,
            MsMode::X1,
        );
        let pixel = u32::from_le_bytes(format.encode(&self.color)[..4].try_into().ok()?);
        let channels = std::array::from_fn(|index| request & (4 << index) != 0);
        let mask = u32::from_le_bytes(format.write_mask(channels)[..4].try_into().ok()?);
        self.fill(&target, layer, pixel, mask, memory)
    }

    fn clear_zeta(&self, request: u32, layer: u32, memory: &mut View) -> Option<()> {
        let [hi, lo, code, mode, stride] = self.zeta;
        let [width, height, depth] = self.zeta_size;
        let layer = layer + self.zeta_base_layer;
        let layers = if depth & (1 << 16) != 0 {
            1
        } else {
            depth & 0xffff
        };
        let format = ZetaFormat::from_code((code & 0x1f) as u8)?;
        if format.bytes != 4
            || mode & !0xf0 != 0
            || mode >> 4 > 5
            || layer >= layers
            || (layer > 0 && stride == 0)
            || lo & 3 != 0
        {
            return None;
        }
        let surface = Surface::new(
            u64::from(hi & 0xff) << 32 | u64::from(lo),
            width,
            height,
            mode,
            u64::from(stride) * 4,
            4,
            MsMode::X1,
        );
        let mut mask = 0;
        if request & 1 != 0 {
            mask = format.with_depth(mask, u32::MAX);
        }
        if request & 2 != 0 {
            mask = format.with_stencil(mask, u8::MAX, u8::MAX);
        }
        self.fill(
            &surface,
            layer,
            format.encode(self.depth, self.stencil) as u32,
            mask as u32,
            memory,
        )
    }

    fn fill(
        &self,
        target: &Surface,
        layer: u32,
        pixel: u32,
        mask: u32,
        memory: &mut View,
    ) -> Option<()> {
        let [horizontal, vertical] = self.screen;
        let (x, y) = (horizontal & 0xffff, vertical & 0xffff);
        let x_end = (x + (horizontal >> 16)).min(target.width);
        let y_end = (y + (vertical >> 16)).min(target.height);
        if u64::from(x_end.saturating_sub(x)) * u64::from(y_end.saturating_sub(y)) > 16_777_216 {
            return None;
        }
        if mask == 0 {
            return Some(());
        }
        for row in y..y_end {
            for column in x..x_end {
                let address = target.sample_address(column, row, layer, 0);
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
