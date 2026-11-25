//! Initial Ampere 3D (C797) color and depth/stencil clear methods.
//! The render-target format and surface layout are shared with Maxwell.
use super::{mme, mmu::View};
use maxwell_tex::{MsMode, RtFormat, Surface, ZetaFormat};

pub(super) const CLASS: u32 = 0xc797;
const TARGET_BASE: u32 = 0x800;
const TARGET_STRIDE: u32 = 0x40;
const SCREEN_HORIZONTAL: u32 = 0xff4;
const SCREEN_VERTICAL: u32 = 0xff8;
const VIEWPORT_CLIP: u32 = 0xc00;
const CLEAR_RECT: u32 = 0xd6c;
const CLEAR_COLOR: u32 = 0xd80;
const CLEAR_DEPTH: u32 = 0xd90;
const CLEAR_STENCIL: u32 = 0xda0;
const ZETA_BASE: u32 = 0xfe0;
const ZETA_SIZE: u32 = 0x1228;
const SCISSOR_ENABLE: u32 = 0xe00;
const SCISSOR_RECT: u32 = 0xe04;
const STENCIL_MASK: u32 = 0x139c;
const ZETA_ENABLE: u32 = 0x1538;
const ZETA_BASE_LAYER: u32 = 0x179c;
const CLEAR_CONTROL: u32 = 0x10f8;
const TARGET_CONTROL: u32 = 0x121c;
const CLEAR_SURFACE: u32 = 0x19d0;
const RESPECT_STENCIL_MASK: u32 = 1;
const USE_CLEAR_RECT: u32 = 1 << 4;
const USE_SCISSOR: u32 = 1 << 8;
const USE_VIEWPORT: u32 = 1 << 12;

/// Methods that act rather than set state. Everything else below the MME macro
/// calls (0x3800) only latches a value, which the engine keeps in `regs`.
fn is_action(method: u32) -> bool {
    matches!(
        method,
        0x10c..=0x120
            | 0x12c
            | 0x13c..=0x14c
            | 0x168
            | 0x1b0
            | 0x1b4
            | 0x21c
            | 0x268..=0x274
            | 0x27c
            | 0x2c8
            | 0x2d4
            | 0x2f0
            | 0x304
            | 0x508
            | 0x55c..=0x570
            | 0xd78
            | 0xda4
            | 0xdf0
            | 0xdf4
            | 0xf18
            | 0x1114
            | 0x1144
            | 0x1214
            | 0x1218
            | 0x123c
            | 0x1288
            | 0x1304
            | 0x1330..=0x1338
            | 0x1424
            | 0x1428
            | 0x1500
            | 0x15e8
            | 0x15f0
            | 0x1614
            | 0x1618
            | 0x1624
            | 0x17e0..=0x17f8
            | 0x1958
            | 0x19d0
            | 0x1a2c
            | 0x1b0c
            | 0x238c..=0x23cc
            | 0x2400..=0x249c
    ) || method >= 0x3800
}

pub(super) struct ThreeD {
    regs: Box<[u32; 0x3800 / 4]>,
    upload: maxwell::inline::Inline,
    mme: mme::Mme,
    mme_data: Box<[u32; mme::DATA_WORDS]>,
    mme_memory: [u32; 2],
    mme_data_address: u32,
    mme_fifo: Vec<u32>,
    in_macro: bool,
    /// Constant buffers bound per shader stage (address, size), by slot.
    pub constants: [[Option<(u64, u32)>; 18]; 5],
    /// The method a macro wrote that failed, for tracing.
    pub macro_fault: Option<(u32, u32)>,
    pub timestamp: u64,
    targets: [[u32; 9]; 8],
    zeta: [u32; 5],
    zeta_size: [u32; 3],
    zeta_enabled: bool,
    zeta_base_layer: u32,
    screen: [u32; 2],
    viewport: [u32; 2],
    rect: [u32; 2],
    scissor_enabled: bool,
    scissor: [u32; 2],
    color: [u32; 4],
    depth: f32,
    stencil: u8,
    stencil_mask: u8,
    target_control: u32,
    clear_control: u32,
}

impl Default for ThreeD {
    fn default() -> Self {
        Self {
            regs: Box::new([0; 0x3800 / 4]),
            upload: Default::default(),
            mme: Default::default(),
            mme_data: Box::new([0; mme::DATA_WORDS]),
            mme_memory: [0; 2],
            mme_data_address: 0,
            mme_fifo: Vec::new(),
            in_macro: false,
            constants: [[None; 18]; 5],
            macro_fault: None,
            timestamp: 0,
            targets: Default::default(),
            zeta: Default::default(),
            zeta_size: Default::default(),
            zeta_enabled: false,
            zeta_base_layer: 0,
            screen: Default::default(),
            viewport: Default::default(),
            rect: Default::default(),
            scissor_enabled: false,
            scissor: Default::default(),
            color: Default::default(),
            depth: 0.0,
            stencil: 0,
            stencil_mask: 0,
            target_control: 0,
            clear_control: 0,
        }
    }
}

impl ThreeD {
    pub fn method(&mut self, method: u32, value: u32, memory: &mut View) -> Option<()> {
        match method {
            0x100 | 0x110 => {}
            // Ordering, cache and performance-monitor actions. The engine runs each
            // method to completion against guest memory, so none has work left to do.
            0x13c..=0x14c
            | 0x21c
            | 0x27c
            | 0x2d4
            | 0xda4
            | 0xf18
            | 0x1114
            | 0x1144
            | 0x1288
            | 0x1330..=0x1338
            | 0x1424
            | 0x1428
            | 0x1500
            | 0x1958
            | 0x1a2c => {}
            // MME program and start-address uploads.
            0x114..=0x120 => self.mme.upload(method, value)?,
            // MME DMA: guest memory into the data RAM, directly or as the next parameters.
            0x550 | 0x554 => self.mme_memory[((method - 0x550) / 4) as usize] = value,
            0x558 => self.mme_data_address = value,
            0x55c => {
                let source = self.mme_address();
                for word in 0..u64::from(value) {
                    let at = (self.mme_data_address as usize).checked_add(word as usize)?;
                    *self.mme_data.get_mut(at)? = memory.load(source + word * 4)?;
                }
            }
            0x560 => {
                let source = self.mme_address();
                self.mme_fifo = (0..u64::from(value))
                    .map(|word| memory.load(source + word * 4))
                    .collect::<Option<_>>()?;
            }
            0x56c => {}
            0x570 => *self.mme_data.get_mut(self.mme_data_address as usize)? = value,
            _ if method >= 0x3800 => {
                if self.in_macro {
                    return None;
                }
                if let Some((index, parameters)) = self.mme.call(method, value) {
                    self.run_macro(index, parameters, memory)?;
                }
                return Some(());
            }
            0x168 => self.report(value, memory)?,
            0x1b00..=0x1b08 => {}
            0x1b0c => self.legacy_report(value, memory)?,
            // NOTIFY: a 16-byte notifier with the time and a zero (success) status.
            0x10c if value <= 1 => {
                let address =
                    u64::from(self.regs[0x104 / 4] & 0xff) << 32 | u64::from(self.regs[0x108 / 4]);
                memory.release(address + 8, 0, None)?;
                memory.store(address + 12, 0)?;
                memory.store(address, self.timestamp as u32)?;
                memory.store(address + 4, (self.timestamp >> 32) as u32)?;
            }
            // LOAD_CONSTANT_BUFFER: words into the selected buffer at its offset.
            0x238c => {}
            0x2390..=0x23cc => {
                let offset = self.regs[0x238c / 4];
                let (address, size) = self.selected_constants();
                if offset >= size {
                    return None;
                }
                memory.store(address + u64::from(offset), value)?;
                self.regs[0x238c / 4] = offset + 4;
                return Some(());
            }
            // BIND_GROUP_CONSTANT_BUFFER for the five graphics stages.
            0x2410 | 0x2430 | 0x2450 | 0x2470 | 0x2490 => {
                let stage = ((method - 0x2410) / 0x20) as usize;
                let binding = (value & 1 != 0).then(|| self.selected_constants());
                *self.constants[stage].get_mut((value >> 4 & 0x1f) as usize)? = binding;
            }
            0x2400..=0x249c => {} // The reserved bind-group words.
            method if maxwell::inline::Inline::owns(method) => {
                self.upload
                    .upload(method, value, |address, bytes| memory.write(address, bytes))?;
            }
            TARGET_BASE..=0x9ff => {
                let target = ((method - TARGET_BASE) / TARGET_STRIDE) as usize;
                let reg = ((method - TARGET_BASE) % TARGET_STRIDE / 4) as usize;
                *self.targets[target].get_mut(reg)? = value;
            }
            SCREEN_HORIZONTAL | SCREEN_VERTICAL => {
                self.screen[((method - SCREEN_HORIZONTAL) / 4) as usize] = value;
            }
            VIEWPORT_CLIP..=0xc04 => self.viewport[((method - VIEWPORT_CLIP) / 4) as usize] = value,
            CLEAR_RECT..=0xd70 => self.rect[((method - CLEAR_RECT) / 4) as usize] = value,
            SCISSOR_ENABLE => self.scissor_enabled = value & 1 != 0,
            SCISSOR_RECT..=0xe08 => self.scissor[((method - SCISSOR_RECT) / 4) as usize] = value,
            STENCIL_MASK => self.stencil_mask = value as u8,
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
            _ if is_action(method) => return None,
            _ => {}
        }
        if let Some(reg) = self.regs.get_mut(method as usize / 4) {
            *reg = value;
        }
        Some(())
    }

    fn selected_constants(&self) -> (u64, u32) {
        let address =
            u64::from(self.regs[0x2384 / 4] & 0xff) << 32 | u64::from(self.regs[0x2388 / 4]);
        (address, self.regs[0x2380 / 4] & 0x1ffff)
    }

    fn mme_address(&self) -> u64 {
        u64::from(self.mme_memory[0] & 0xff) << 32 | u64::from(self.mme_memory[1])
    }

    /// Run the pending macro call, at the end of the push-buffer packet that made it.
    pub fn end_packet(&mut self, memory: &mut View) -> Option<()> {
        match self.mme.call.take() {
            Some((index, parameters)) => self.run_macro(index, parameters, memory),
            None => Some(()),
        }
    }

    fn run_macro(&mut self, index: usize, parameters: Vec<u32>, memory: &mut View) -> Option<()> {
        let code = std::mem::take(&mut self.mme);
        self.in_macro = true;
        let result = code.run(
            index,
            &mut Macro {
                engine: self,
                memory,
                parameters: parameters.into(),
            },
        );
        self.in_macro = false;
        self.mme = code;
        result
    }

    /// REPORT_SEMAPHORE_EXECUTE: releases and acquires of a 32 or 64-bit payload, with no
    /// counter report. A four-word structure adds the time at byte 8.
    fn report(&self, request: u32, memory: &mut View) -> Option<()> {
        let operation = request & 3;
        let counter = request >> 7 & 0x1f;
        let structure = request >> 13 & 3;
        if counter != 0 || structure > 1 || request & (1 << 20) != 0 {
            return None;
        }
        let wide = request & (1 << 27) != 0;
        let (lower, upper) = (self.regs[0x158 / 4], self.regs[0x15c / 4]);
        let address =
            u64::from(self.regs[0x164 / 4] & 0xff) << 32 | u64::from(self.regs[0x160 / 4]);
        match operation {
            0 => {
                memory.store(address, lower)?;
                if wide {
                    memory.store(address + 4, upper)?;
                } else if structure == 0 {
                    memory.store(address + 4, 0)?;
                }
                if structure == 0 {
                    memory.store(address + 8, self.timestamp as u32)?;
                    memory.store(address + 12, (self.timestamp >> 32) as u32)?;
                }
                Some(())
            }
            1 => {
                let equal =
                    memory.load(address)? == lower && (!wide || memory.load(address + 4)? == upper);
                equal.then_some(())
            }
            _ => None,
        }
    }

    /// SET_REPORT_SEMAPHORE_D: a release of the payload in SET_REPORT_SEMAPHORE_C, or a
    /// counter report. The engine keeps no statistics, so every counter reads zero.
    fn legacy_report(&self, request: u32, memory: &mut View) -> Option<()> {
        let address =
            u64::from(self.regs[0x1b00 / 4] & 0xff) << 32 | u64::from(self.regs[0x1b04 / 4]);
        let payload = self.regs[0x1b08 / 4];
        let one_word = request & (1 << 28) != 0;
        let counter = request >> 23 & 0x1f;
        if request & (1 << 3) != 0 {
            return None; // Reductions.
        }
        match request & 3 {
            0 if one_word => memory.store(address, payload),
            0 | 2 => {
                // Four words: the payload (or a 64-bit counter for a report), then the time.
                let (low, high) = if request & 3 == 2 && counter != 0 {
                    (0, 0)
                } else {
                    (payload, 0)
                };
                memory.store(address, low)?;
                memory.store(address + 4, high)?;
                memory.store(address + 8, self.timestamp as u32)?;
                memory.store(address + 12, (self.timestamp >> 32) as u32)
            }
            1 => {
                let value = memory.load(address)?;
                let passed = if request & (1 << 16) != 0 {
                    value >= payload
                } else {
                    value == payload
                };
                passed.then_some(())
            }
            _ => None,
        }
    }

    fn clear(&self, request: u32, memory: &mut View) -> Option<()> {
        if request >> 26 != 0
            || self.clear_control
                & !(RESPECT_STENCIL_MASK | USE_CLEAR_RECT | USE_SCISSOR | USE_VIEWPORT)
                != 0
        {
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
            let write_mask = if self.clear_control & RESPECT_STENCIL_MASK != 0 {
                self.stencil_mask
            } else {
                u8::MAX
            };
            mask = format.with_stencil(mask, u8::MAX, write_mask);
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
        let bounds = |axis: usize, size: u32| {
            let range = |packed: u32| (packed & 0xffff, packed >> 16);
            let (mut start, length) = range(self.screen[axis]);
            let mut end = (start + length).min(size);
            let mut clip = |near: u32, far: u32| {
                start = start.max(near);
                end = end.min(far);
            };
            if self.clear_control & USE_CLEAR_RECT != 0 {
                let (near, far) = range(self.rect[axis]);
                clip(near, far);
            }
            if self.clear_control & USE_SCISSOR != 0 && self.scissor_enabled {
                let (near, far) = range(self.scissor[axis]);
                clip(near, far);
            }
            if self.clear_control & USE_VIEWPORT != 0 {
                let (near, length) = range(self.viewport[axis]);
                clip(near, near + length);
            }
            (start, end)
        };
        let (x, x_end) = bounds(0, target.width);
        let (y, y_end) = bounds(1, target.height);
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

struct Macro<'a, 'b, 'c> {
    engine: &'a mut ThreeD,
    memory: &'b mut View<'c>,
    parameters: std::collections::VecDeque<u32>,
}

impl mme::Host for Macro<'_, '_, '_> {
    fn parameter(&mut self) -> Option<u32> {
        self.parameters.pop_front()
    }

    fn state(&mut self, method: u32) -> u32 {
        self.engine
            .regs
            .get(method as usize / 4)
            .copied()
            .unwrap_or(0)
    }

    fn method(&mut self, method: u32, value: u32) -> Option<()> {
        let result = self.engine.method(method, value, self.memory);
        if result.is_none() {
            self.engine.macro_fault = Some((method, value));
        }
        result
    }

    fn barrier(&mut self) -> Option<()> {
        self.parameters = std::mem::take(&mut self.engine.mme_fifo).into();
        Some(())
    }

    fn data(&mut self) -> &mut [u32; mme::DATA_WORDS] {
        &mut self.engine.mme_data
    }
}
