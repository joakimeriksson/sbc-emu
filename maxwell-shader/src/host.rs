//! What the interpreter needs from the rest of the GPU.

use crate::exec::Lanes;
use crate::isa::*;

/// Where a texture or surface handle comes from.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Handle<const N: usize> {
    /// Word number in the texture constant buffer (the one TEX_CB_INDEX selects).
    Bound(u32),
    /// The handle itself, from registers.
    Bindless([u32; N]),
}

/// A texture instruction with its operands. Coordinates and levels are raw register bits:
/// floats, except for `Fetch` (integers) and `Query` (the level, an integer).
#[derive(Clone, Copy, Debug)]
pub struct TexOp<const N: usize> {
    pub kind: TexKind,
    pub dim: TexDim,
    pub array: bool,
    pub handle: Handle<N>,
    pub coord: [[u32; N]; 3],
    pub layer: [u32; N],
    pub lod_mode: LodMode,
    /// Bias or level (`LodMode::Bias`, `LodMode::Level`).
    pub lod: [u32; N],
    pub shadow: bool,
    pub dref: [f32; N],
    pub ms: bool,
    pub sample: [u32; N],
    /// Texel offsets, sign-extended.
    pub offset: [[i32; N]; 3],
    /// One offset per texel of a gather: pairs of signed bytes (see `gather_offsets`).
    pub per_texel: Option<[[u32; N]; 2]>,
    /// du/dx, dv/dx and du/dy, dv/dy (`Grad`).
    pub ddx: [[f32; N]; 2],
    pub ddy: [[f32; N]; 2],
    /// Component to gather.
    pub comp: u8,
    /// TXQ: 1 dimensions, 2 type (samples in component 2), 5 sample position, ...
    pub query: u8,
    /// Components wanted.
    pub mask: u8,
    /// Lanes that want a result. Lanes of a running quad always have coordinates (for the
    /// derivatives), whether they want one or not.
    pub lanes: u32,
    /// Use all lanes of the quad for derivatives, running or not.
    pub ndv: bool,
}

impl<const N: usize> TexOp<N> {
    /// The offset of texel `t` (0..4) of a gather with one offset per texel.
    pub fn gather_offset(&self, lane: usize, t: usize) -> Option<(i32, i32)> {
        let r = self.per_texel?[t / 2][lane] >> (t % 2 * 16);
        Some((r as i8 as i32, (r >> 8) as i8 as i32))
    }
}

/// A surface instruction with its operands.
#[derive(Clone, Copy, Debug)]
pub struct SurfOp<const N: usize> {
    pub dim: SurfDim,
    pub handle: Handle<N>,
    /// Texel coordinates; the array index follows the coordinates.
    pub coord: [[u32; N]; 3],
    /// Raw texel bits of type `ty` instead of components converted by the format.
    pub raw: bool,
    pub ty: MemTy,
    pub mask: u8,
    /// Lanes that do the access (never helper lanes for writes).
    pub lanes: u32,
}

/// An interpolation.
#[derive(Clone, Copy, Debug)]
pub struct IpaOp<const N: usize> {
    /// Attribute byte address (0x70 position.x, 0x7c 1/w, 0x80 generic 0, 0x3fc facing, ...).
    pub addr: u32,
    pub mode: IpaMode,
    pub loc: IpaLoc,
    /// Offset from the pixel centre in pixels, `IpaLoc::Offset` only.
    pub offset: [[f32; N]; 2],
}

/// The rest of the GPU, as a shader sees it. Generic, so that calls inline.
#[allow(unused_variables)]
pub trait Host<const N: usize> {
    /// c[index][offset]; 0 outside the buffer.
    fn cbuf(&self, index: u32, offset: u32) -> u32;

    /// System register `sys` (S2R, CS2R). Lane numbers (0x00), lane masks (0x38-0x3c) and
    /// "killed" (0x13) are answered by the interpreter.
    fn sysval(&mut self, sys: u8) -> [u32; N] {
        [0; N]
    }

    /// Global memory.
    fn load(&mut self, addr: u64, out: &mut [u8]) {
        out.fill(0);
    }
    fn store(&mut self, addr: u64, data: &[u8]) {}
    /// An atomic on global memory; returns the old value. The default is made of `load` and
    /// `store`.
    fn atomic(&mut self, addr: u64, op: AtomOp, ty: AtomTy, a: u64, b: u64) -> u64 {
        let mut buf = [0; 8];
        let n = ty.bytes();
        self.load(addr, &mut buf[..n]);
        let old = u64::from_le_bytes(buf);
        let new = atomic_apply(op, ty, old, a, b);
        self.store(addr, &new.to_le_bytes()[..n]);
        old
    }
    /// The workgroup's shared memory.
    fn shared(&mut self) -> &mut [u8] {
        &mut []
    }

    /// ALD: one 32-bit component.
    fn attr_load(&mut self, lane: usize, a: Attr) -> u32 {
        0
    }
    /// AST: one 32-bit component.
    fn attr_store(&mut self, lane: usize, a: Attr, v: u32) {}
    /// ISBERD: the handle ALD's vertex operand takes for input vertex `v`.
    fn isberd(&mut self, lane: usize, v: u32) -> u32 {
        v
    }
    /// OUT: end the vertex whose attributes were stored with `handle` (if `emit`) and then end
    /// the primitive (if `cut`). Returns the handle for the next vertex.
    fn emit(&mut self, lane: usize, handle: u32, stream: u32, emit: bool, cut: bool) -> u32 {
        handle
    }

    /// The attribute interpolated linearly in screen space, as the rasterizer set it up: an
    /// attribute the program header calls perspective was divided by w, and `IpaMode::MulW`
    /// means that the interpreter multiplies the result with w (from a register). Flat
    /// attributes (`Constant`) are returned as bits, unchanged.
    fn interpolate(&mut self, op: &IpaOp<N>) -> [u32; N] {
        [0; N]
    }
    /// PIXLD; `arg` is the sample number for the offset queries.
    fn pixld(&mut self, what: Pixld, arg: &[u32; N]) -> [u32; N] {
        [0; N]
    }

    /// Returns the components r, g, b, a by lane. `QueryLod` returns the level of detail before
    /// and after clamping, as floats; the interpreter converts them to the fixed point TMML
    /// gives.
    fn texture(&mut self, op: &TexOp<N>) -> [[u32; N]; 4] {
        [[0; N]; 4]
    }
    /// Converted components, or raw texel bits from the low word up (`op.raw`).
    fn surface_load(&mut self, op: &SurfOp<N>) -> [[u32; N]; 4] {
        [[0; N]; 4]
    }
    fn surface_store(&mut self, op: &SurfOp<N>, data: &[[u32; N]; 4]) {}
    /// Returns the old values. `a` is the operand (or the expected value), `b` the new value of
    /// a compare-and-swap.
    fn surface_atomic(
        &mut self,
        op: &SurfOp<N>,
        atom: AtomOp,
        ty: AtomTy,
        a: &[u64; N],
        b: &[u64; N],
    ) -> [u64; N] {
        [0; N]
    }

    /// Called before every instruction (for tracing); compiled out when not implemented.
    #[inline(always)]
    fn trace(&mut self, addr: u32, inst: &Inst, lanes: &Lanes<N>) {}
}

/// The value an atomic leaves in memory.
pub fn atomic_apply(op: AtomOp, ty: AtomTy, old: u64, a: u64, b: u64) -> u64 {
    if ty.bytes() == 4 {
        let (o, a, b) = (old as u32, a as u32, b as u32);
        let signed = ty == AtomTy::S32;
        let float = ty == AtomTy::F32;
        let (fo, fa) = (f32::from_bits(o), f32::from_bits(a));
        (match op {
            AtomOp::Add if float => (fo + fa).to_bits(),
            AtomOp::Min if float => fo.min(fa).to_bits(),
            AtomOp::Max if float => fo.max(fa).to_bits(),
            AtomOp::Add => o.wrapping_add(a),
            AtomOp::Min if signed => (o as i32).min(a as i32) as u32,
            AtomOp::Max if signed => (o as i32).max(a as i32) as u32,
            AtomOp::Min => o.min(a),
            AtomOp::Max => o.max(a),
            AtomOp::Inc => {
                if o >= a {
                    0
                } else {
                    o + 1
                }
            }
            AtomOp::Dec => {
                if o == 0 || o > a {
                    a
                } else {
                    o - 1
                }
            }
            AtomOp::And => o & a,
            AtomOp::Or => o | a,
            AtomOp::Xor => o ^ a,
            AtomOp::Exch => a,
            AtomOp::Cas => {
                if o == a {
                    b
                } else {
                    o
                }
            }
        }) as u64
    } else {
        let signed = ty == AtomTy::S64;
        match op {
            AtomOp::Add => old.wrapping_add(a),
            AtomOp::Min if signed => (old as i64).min(a as i64) as u64,
            AtomOp::Max if signed => (old as i64).max(a as i64) as u64,
            AtomOp::Min => old.min(a),
            AtomOp::Max => old.max(a),
            AtomOp::Inc => {
                if old >= a {
                    0
                } else {
                    old + 1
                }
            }
            AtomOp::Dec => {
                if old == 0 || old > a {
                    a
                } else {
                    old - 1
                }
            }
            AtomOp::And => old & a,
            AtomOp::Or => old | a,
            AtomOp::Xor => old ^ a,
            AtomOp::Exch => a,
            AtomOp::Cas => {
                if old == a {
                    b
                } else {
                    old
                }
            }
        }
    }
}
