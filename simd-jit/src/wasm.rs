//! The WebAssembly backend (being written): no code yet, `eval` runs the blocks.
use crate::ir::Block;

pub struct Module;

impl Module {
    pub fn new(_b: &Block) -> Option<Module> {
        None
    }
    pub fn run(&self, _regs: *mut u32, _flags: *mut u32, _args: *const u32, _inputs: *const u32, _outputs: *mut u32) -> bool {
        false
    }
}

/// How often the page has freed the JIT's table slots (registered by the machine).
pub fn set_reset_count(_f: fn() -> u32) {}
