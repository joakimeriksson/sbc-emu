//! NVIDIA Maxwell (GM20B, shader model 5) shader code, as Mesa's nouveau Gallium driver
//! (nv50_ir, the GM107 emitter) produces it: decoder, interpreter, disassembler and the program
//! header of graphics shaders.
//!
//! * `Program::decode_at` decodes the code reachable from an entry point;
//! * `Lanes` holds the registers of N invocations (a quad is 4 lanes) and `run` executes them;
//! * everything outside the shader core is behind the `Host` trait: constant buffers, memory,
//!   attributes, interpolation, textures, images;
//! * `Sph` reads the 80-byte program header.
//!
//! Range reduction (RRO) passes its operand on and MUFU takes the sine, cosine or power of that,
//! so the pair gives the right result. Physical attribute addresses (AL2P) are the logical
//! ones. A lane that executes KIL goes on as a helper (`discard` is `demote` in nouveau).
//! Scheduling words, memory barriers and cache control do nothing.

#![forbid(unsafe_code)]
#![allow(clippy::needless_range_loop, clippy::precedence, clippy::large_enum_variant)]

pub mod decode;
pub mod disasm;
pub mod exec;
pub mod fp;
pub mod host;
pub mod isa;
pub mod program;
pub mod sph;

pub use exec::{run, Exit, Lanes};
pub use host::{atomic_apply, Handle, Host, IpaOp, SurfOp, TexOp};
pub use program::Program;
pub use sph::Sph;
