//! A few textures with known contents.

use maxwell_shader::isa::*;
use maxwell_shader::TexOp;

pub fn texture<const N: usize>(_t: &TexOp<N>) -> [[u32; N]; 4] {
    [[0; N]; 4]
}
