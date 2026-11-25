//! The 2D class (902D), unchanged since Fermi. The engine is the older Jetson's; this file
//! gives it the GA10B page tables and turns a fault into a failed method.
use super::mmu::{View, physical};
use maxwell::Memory;
use std::cell::Cell;

pub(super) const CLASS: u32 = 0x902d;

#[derive(Default)]
pub(super) struct TwoD {
    engine: maxwell::twod::Twod,
}

impl TwoD {
    pub fn method(&mut self, method: u32, data: u32, memory: &mut View) -> Option<()> {
        if method >= 0x1000 {
            return None;
        }
        let mut adapter = Adapter {
            view: memory,
            fault: Cell::new(false),
        };
        self.engine.method(method, data, &mut adapter);
        (!adapter.fault.get()).then_some(())
    }
}

struct Adapter<'a, 'b> {
    view: &'a mut View<'b>,
    fault: Cell<bool>,
}

impl Memory for Adapter<'_, '_> {
    fn read(&self, mut address: u64, mut out: &mut [u8]) {
        while !out.is_empty() {
            let n = out.len().min(4096 - (address as usize & 4095));
            let source = self
                .view
                .translate(address, false)
                .and_then(|physical_address| physical(self.view.ram, physical_address, n));
            match source {
                Some(offset) => out[..n].copy_from_slice(&self.view.ram[offset..offset + n]),
                None => {
                    out[..n].fill(0);
                    self.fault.set(true);
                }
            }
            address += n as u64;
            out = &mut out[n..];
        }
    }

    fn write(&mut self, address: u64, data: &[u8]) {
        if self.view.write(address, data).is_none() {
            self.fault.set(true);
        }
    }
}
