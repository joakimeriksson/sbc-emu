//! The 2D class (902D), unchanged since Fermi. The engine is the older Jetson's; this file
//! gives it the GA10B page tables and turns a fault into a failed method.
use super::mmu::{Guest, View};

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
        let mut guest = Guest::new(memory);
        self.engine.method(method, data, &mut guest);
        guest.ok()
    }
}
