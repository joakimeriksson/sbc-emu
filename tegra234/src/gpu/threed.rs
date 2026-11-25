//! The Ampere 3D class (C797). Its method layout is Maxwell's B197 but for a handful of
//! methods, so state, clears, draws, the rasterizer, render targets and textures are the
//! Tegra210 engine's (`maxwell::threed`) with its SM70 switch on: shaders are Volta-and-later
//! code, each stage has its own 64-bit address, and vertex and index buffers have sizes.
//! This module adds what Turing changed: the macro expander (`mme.rs`) with its DMA methods,
//! `REPORT_SEMAPHORE_EXECUTE`, and `NOTIFY`.
use super::{
    mme,
    mmu::{Guest, View},
};

pub(super) const CLASS: u32 = 0xc797;

pub(super) struct ThreeD {
    engine: maxwell::threed::Threed,
    mme: mme::Mme,
    mme_data: Box<[u32; mme::DATA_WORDS]>,
    mme_memory: [u32; 2],
    mme_data_address: u32,
    mme_fifo: Vec<u32>,
    in_macro: bool,
    /// The method a macro wrote that failed, for tracing.
    pub macro_fault: Option<(u32, u32)>,
    pub timestamp: u64,
    trace: bool,
}

impl Default for ThreeD {
    fn default() -> Self {
        let mut engine = maxwell::threed::Threed::default();
        engine.pipe.sm70 = true;
        // Reset state the NVIDIA driver relies on and never sets:
        // SET_BLEND_SEPARATE_FOR_ALPHA (it always programs the alpha factors)
        engine.regs[0x133c / 4] = 1;
        Self {
            engine,
            mme: Default::default(),
            mme_data: Box::new([0; mme::DATA_WORDS]),
            mme_memory: [0; 2],
            mme_data_address: 0,
            mme_fifo: Vec::new(),
            in_macro: false,
            macro_fault: None,
            timestamp: 0,
            trace: std::env::var_os("SBC_TRACE_3D").is_some(),
        }
    }
}

impl ThreeD {
    fn r(&self, method: u32) -> u32 {
        self.engine.r(method)
    }

    fn address(&self, method: u32) -> u64 {
        u64::from(self.r(method) & 0xff) << 32 | u64::from(self.r(method + 4))
    }

    /// A new submission: guest memory may have changed under cached shaders and textures.
    pub fn begin(&mut self) {
        self.engine.pipe.invalidate();
    }

    /// Before another engine or the CPU looks at memory: render-target tiles go out.
    pub fn flush(&mut self, memory: &mut View) -> Option<()> {
        let mut guest = Guest::new(memory);
        self.engine.rop.flush(&mut guest);
        guest.ok()
    }

    /// Memory was written other than by drawing (copies, uploads).
    pub fn written(&mut self) {
        self.engine.pipe.invalidate_textures();
        self.engine.pipe.cbufs_changed = true;
    }

    pub fn method(&mut self, method: u32, value: u32, memory: &mut View) -> Option<()> {
        if self.trace {
            eprintln!(
                "[3d] {method:#06x} = {value:#x}{}",
                if self.in_macro { " (macro)" } else { "" }
            );
        }
        match method {
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
            0x564 | 0x568 => return None, // MME DMA writes and reductions
            0x168 => {
                self.flush(memory)?;
                self.report(value, memory)?;
            }
            // NOTIFY: a 16-byte notifier with the time and a zero (success) status.
            0x10c if value <= 1 => {
                self.flush(memory)?;
                let address = self.address(0x104);
                memory.store(address, self.timestamp as u32)?;
                memory.store(address + 4, (self.timestamp >> 32) as u32)?;
                memory.store(address + 8, 0)?;
                memory.store(address + 12, 0)?;
            }
            0x10c => return None,
            _ if method >= 0x3800 => {
                if self.in_macro {
                    return None;
                }
                if let Some((index, parameters)) = self.mme.call(method, value) {
                    self.run_macro(index, parameters, memory)?;
                }
            }
            _ => {
                let mut guest = Guest::new(memory);
                self.engine
                    .method(method, value, &mut guest, self.timestamp);
                guest.ok()?;
            }
        }
        Some(())
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
        let (lower, upper) = (self.r(0x158), self.r(0x15c));
        let address = self.address(0x160);
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
        self.engine.r(method)
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
