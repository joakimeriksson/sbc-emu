# ARM Mali-T760 MP4 (Midgard) on the RK3288

The RK3288's GPU, emulated in the `midgard` crate for Linux's `panfrost` kernel driver and
Mesa's `panfrost` Gallium driver, unmodified. It is part of the RK3288 SoC model
(`rockchip-rk3288`, register window 0xffa30000, interrupts SPI 6/7/8 "job", "mmu",
"gpu"), so the Tinker Board and the RK3288 panel board profiles both have it.

## What is modelled

| File | |
|---|---|
| `lib.rs` | registers: identification (T760 r0p1, 4 shader cores, 3 job slots, 8 address spaces), power, reset, cache flushes, cycle counter; job slots with the current/next queue and JS_STATE; interrupts; jobs finish after the virtual time their work takes |
| `mmu.rs`, `vm.rs` | the "Mali LPAE" page tables (4 levels, stage-2-style permissions: bit 6 read, bit 7 write), faults reported in AS_FAULTSTATUS with the MMU interrupt |
| `job.rs` | job chains: null, write value, cache flush, compute/vertex, tiler, fragment |
| `isa.rs` | the Midgard shader core: ALU bundles (VMUL/SADD then VADD/SMUL/VLUT, 8/16/32/64-bit lanes, swizzles, half-width expansion and shrinking, modifiers, embedded and inline constants, r31 conditions and csel), branches, discard and fragment write-out, load/store bundles, texture bundles; threads run as 2x2 quads with divergence handled lowest-PC-first |
| `draw.rs` | vertex jobs (attributes and their buffer types, push uniforms, UBOs, `st_vary`), tiler jobs (indices, primitive restart, all draw modes, instancing, near-plane clipping) |
| `frag.rs` | fragment jobs: framebuffer descriptor and render targets, tile buffer with clear, preload, raw formats and dithering, triangles/lines/points with the top-left rule, perspective-correct varyings, gl_FragCoord/gl_FrontFacing/point coordinates, early or late depth/stencil, polygon offset, alpha test, fixed-function blending and blend shaders, occlusion queries, write-back in linear and u-interleaved layouts |
| `tex.rs`, `format.rs` | textures (1D/2D/3D/cube/arrays, linear and u-interleaved, mipmaps, nearest/linear/trilinear, wrap modes, shadow compare), pixel formats |

The polygon lists between tiler and fragment jobs are internal to the GPU, so the model
keeps them on the host, keyed by framebuffer descriptor. The model reports no AFBC and no
native compressed texture formats, which Mesa handles itself.

Things the encoding does not make obvious, learnt the hard way:

- Register numbers 28/29 are two registers each: ALU and load/store writes go to the
  texture unit's inputs, ALU reads see its results (Mesa's disassembler prints `AT` and
  `TA`).
- `ld_ubo`'s buffer index is always an immediate in the argument fields.
- An attribute buffer is a special input (gl_VertexID, gl_FragCoord, ...) when its 6-bit
  type field is above 32; the pointer's low bits share the first byte.
- Mesa's vertex shaders apply the viewport transform themselves: positions arrive as
  window x, y, z and 1/w.

## Running it

kmscube and a GLES 2 test suite run on Debian's Mesa (25.0.7) in a root filesystem made
from the Orange Pi One's GPU image, booted on the panel board profile with the vendor's kernel (it
has Panfrost and the DSI panel built in):

```sh
scripts/panel-board-fixture.sh && scripts/armbian-gpu-fixture.sh && scripts/mali-t760-fixture.sh
sbc-emu run --board panel-board --kernel cache/panel-board/zImage --dtb cache/panel-board/rk3288-panel.dtb \
    --emmc cache/mali-t760/debian-gpu.img --append "$(cat cache/mali-t760/cmdline)" \
    --script scripts/mali-t760-gpu.txt --timeout 1800      # kmscube, screenshot
    # or --script scripts/mali-t760-gles2.txt               # 25 scenes vs softpipe
```

The fixture builds `gles2test` on the emulated Orange Pi One: gcc's driver finds none of
its programs on the RK3288 yet (it runs on the `arm-a32` interpreter there, with the JIT
on the Orange Pi One), which is an open CPU-side issue.

## Debugging

- `SBC_MALI_LOG=1`: a line per job and fragment pass.
- `SBC_MALI_DUMP=dir`: each fragment job's render target 0 as a PPM.
- `SBC_MALI_ISA=1`: every bundle the first thread of a group runs, and the registers it
  changes.
- `SBC_MALI_CAPTURE=dir`: records every job chain with the GPU pages it touches (as they
  were before the chain ran) in `dir/chains.bin`; `mali-replay dir/chains.bin [first]
  [count]` (in `midgard/src/bin`) replays them on the host in milliseconds, with the
  options above. Pages a chain never touched in the captured run are not recorded, so a
  fix that makes the GPU read new data (another cube face, a blend shader) needs a new
  capture.
