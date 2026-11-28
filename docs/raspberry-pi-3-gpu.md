# Raspberry Pi 3: VideoCore IV 3D GPU

The BCM2837's 3D GPU, "V3D 2.1" to Mesa, modelled in `vc4-3d/` from Broadcom's *VideoCore IV 3D
Architecture Reference Guide* (2013) and the Mesa and Linux sources (Mesa's `vc4` Gallium driver,
its copy of the kernel's validator in `src/gallium/drivers/vc4/kernel/`, Linux's `vc4_regs.h`).
This is milestone 7 of the [plan](rpi3-plan.md). Linux's `vc4` binds the V3D as one component of
its DRM device, together with the HVS, pixel valves and HDMI (milestone 4): on the unmodified Pi OS
image (`dtoverlay=vc4-kms-v3d`) `vc4-drm` binds `3fc00000.v3d`, Mesa 26.2 reports
`VC4 V3D 2.1 / OpenGL ES 2.0`, and kmscube, the GLES 2 scenes of the GL suite and glmark2 run on
it ([results](#results-in-the-guest)).

## What is modelled

| Block | Model |
|---|---|
| Registers (`0x7ec0_0000`, ARM `0x3fc0_0000`, GPU interrupt 10) | IDENT0..2 (`V3D 2.1`: 3 slices of 4 QPUs, 2 TMUs per slice, 12 KiB VPM), INTCTL/INTENA/INTDIS (frame done, binner flush done, binner out of memory as a level, overflow-memory used), CT0/CT1 control-list threads (a job starts when CTnEA is written; CTnCA moves to the end address), BFC/RFC counters, PCS, BPCA/BPCS, BPOA/BPOS overflow memory, user QPU programs (SRQPC/SRQUA/SRQCS), cache controls, performance counter registers (read 0), QPU host interrupts (DBQITE/DBQITC) |
| Binning thread (CT0) and tile binner | `TILE_BINNING_MODE_CONFIG` (64x64 tiles, 32x32 in 4x multisample mode), state packets, GL and NV shader records (extended strides too), indexed (8/16-bit) and array primitives in all seven modes; vertices through the coordinate shader; clipping against the near and far planes (from the clipper Z scale) and the 12.4 guard band; face culling; each primitive into the lists of the tiles its bounding box touches, after the state packets each list has not seen; lists in the initial blocks, then blocks from the tile allocation memory, then the kernel's overflow memory: when that is gone too the binner waits with OUTOMEM up until BPOS is written again; `FLUSH` ends every list with a return |
| Rendering thread (CT1) | `TILE_RENDERING_MODE_CONFIG`, `CLEAR_COLORS`, tile coordinates (loads happen there, as on the hardware), branches and sub-lists, the semaphore with the binner, `STORE_TILE_BUFFER_GENERAL`/`LOAD_TILE_BUFFER_GENERAL` (colour RGBA8888 or BGR565, Z+stencil, Z, VG mask; raster, T or LT; sample 0 or resolved), `STORE_MS_TILE_BUFFER(_AND_EOF)`, full-resolution loads and stores (the raw per-sample layout Mesa's `txf_ms` lowering reads), clears on store unless disabled |
| Setup and rasterizer | 12.4 screen positions plus the viewport offset, top-left fill rule at pixel centres (four rotated-grid samples in 4x mode), the clip window, depth offset (factor and units), Zs to 24 bits, 1/Wc for perspective; lines and points as quads with their line and sprite coordinates as the hardware varyings Mesa expects |
| Fragment shading and the tile buffer (TLB) | four 2x2 quads per program run; the W and Z payload in ra15/rb15; varyings as the varying unit gives them (the shader multiplies by W and adds C from r5); flat shading flags; TLB Z writes with the depth test and Z updates from the configuration bits; stencil setup words (front, back, both, write masks) with all eight ops and functions; colour writes to all samples or sample by sample; colour, coverage and VG-mask loads into r4; the multisample mask; the reverse-facing flag; conditional writes for discards |
| QPU | 16 lanes; every add and multiply operation; register files A and B, accumulators r0–r5 (r5 by quad or replicated), the uniform stream and its address register, small immediates and the multiply pipe's vector rotation, 32-bit and per-element load immediates, semaphores, the mutex, branches (relative, absolute, by register; all and any conditions; three delay slots; link), program end with its two delay slots, thread switches (no-ops: each program runs to its end), flags from either pipe, regfile-A pack (16-bit float or integer, 8-bit, saturating) and unpack, multiply-pipe colour pack, r4 unpack; SFU (recip, rsqrt, exp2, log2 into r4) |
| TMU | requests by coordinate writes with the configuration words taken from the uniform stream (P0, P1, P2/P3), direct memory reads (S only); RGBA8888, RGBX8888, RGBA4444, RGBA5551, RGB565, luminance, alpha, luminance-alpha, ETC1, S8/S16, RGBA32R; T, LT and raster layouts; mip levels below the base with power-of-two sizes; repeat, clamp, mirror, border (colour from R); nearest, bilinear and the four mipmap filters with the LOD from each quad, bias or explicit LOD; cube maps (face from the major axis, face stride from P2) |
| VPM | generic reads and writes (32-bit, 16/8-bit laned, horizontal and vertical); for vertex batches the vertex cache DMA loads the attributes one vertex per column and the shaders' outputs are read back from their own rows; VPM DMA loads and stores (32-bit) for user programs |
| User QPU programs | the queue (16 deep), twelve QPUs in turns, semaphores and the mutex between programs, completion counts |

Vertices are shaded once per job and cached (the hardware shades again for each tile; the
results are the same). The tile lists' primitive records are this model's own encoding (`bin.rs`
describes it): only the rendering thread reads them, and nothing in Linux or Mesa looks inside.

## Jobs, time and threads

A job runs when the kernel writes CTnEA, either in that register write (one-thread mode, the
browser; deterministic) or, with `--threads multi`, on a GPU thread of its own while the cores run
on. A binning job's lists are laid out in memory by the device itself (it owns the overflow
registers), so the out-of-memory wait works in both modes. The done interrupt comes after the
host time the job took, scaled by `SBC_GPU_TIME` (default 1) and capped at 50 ms (well inside the
kernel's hang check), or as soon as the GPU thread has finished. While a job runs on the GPU
thread, CTnCA reads move, so the kernel's hang check sees progress. Snapshots keep the registers,
pending interrupts, the clear values and a binning job waiting for memory (or finished on the GPU
thread but not collected yet).

### Parallel rendering

The VideoCore IV's twelve QPUs shade several tiles at once, and the binning job has already
written every tile's list, so the tiles of a rendering job are independent. The model renders them
on several host threads, as the [Pi 4's V3D model](gpu.md#parallel-rendering) does
(`vc4-3d/src/par.rs`; `SBC_VC4_THREADS`, default the host's cores minus two, at most 16; the
browser build stays on one thread). The QPUs run through the [JIT](#qpu-jit).

- **Tiles:** the rendering list the kernel builds is walked first: each tile's packets (its
  loads, tile coordinates, the branch into its list, its stores) up to the store that clears the
  whole tile buffer become one work item; the tiles between two other packets (the configuration,
  clear colours) are queued and rendered together, each worker taking the next tile with its own
  tile buffer, fragment-shader QPU, view of guest memory and instruction cache. On one thread a
  tile starts from the buffer the previous tile's last store cleared; a worker clears its buffer
  before each tile, which is the same (the first tile of the queue starts from the buffer as it
  is). A tile list that stops the job (a halt, a bad packet) is found before the tiles are
  handed out, and nothing after it is rendered, as on one thread.
- **Vertices:** before the tiles are rendered, their lists are scanned for the vertices they use,
  and all workers run the vertex shader over them in 128-vertex chunks (8 batches of 16) into a
  cache the render workers share. A binning job does the same for the coordinate shader over all
  its draws before binning them (the binning itself stays on one thread). A vertex's outputs do
  not depend on the other vertices of its batch, so this gives the same outputs as one thread,
  which shades vertices as the primitives ask for them.
- **No thread indices or shared VPM:** unlike the V3D 4.2, the VideoCore IV has no per-thread
  scratch memory (Mesa's `vc4` compiler does not spill to memory), each vertex batch gets a VPM of
  its own, and the GL pipeline's shaders use no semaphores; user QPU programs (which do) are not
  part of this and still run in turns. The TMU and the uniform streams are per QPU, as before.
- **Capture:** a job being captured (`SBC_VC4_CAPTURE`) runs on one thread, so the capture
  records each page as it was before the job.

**Check:** a unit test (`threads_render_the_same`) renders five scenes (depth-tested Gouraud
quads with Z stores; additive blending over loaded tiles; mipmapped textures, points, lines and a
clipped triangle; 4x multisampling with stencil; a tile list that halts) with 1, 2, 5 and 16
threads and compares all of memory byte for byte. In the guest, see [results](#results-in-the-guest).

### QPU fast path and caches

As in the Pi 4's model, the instruction cache keeps a pre-decoded flag with each instruction: an
ALU instruction without pack, unpack or vector rotation (most of what Mesa emits outside colour
packing and texture results) runs a lean path (`Qpu::alu_fast`) that reads registers in place and
writes plain registers directly; the rest takes the general path. `SBC_VC4_QPU_FAST=0` sends
everything to the general path; a test (`fast_path_matches_general_path`) runs random fast-path
instructions on random registers (special floats included) through both and compares registers,
flags, uniforms and the VPM. Texture setups (the decoded configuration words and the mip levels'
addresses and layouts) are cached per host thread by their configuration words (the VideoCore IV
keeps no texture state in memory, so an entry never goes stale), the shaded-vertex cache hashes
with a multiplicative hash instead of SipHash, and the T and LT addressing uses shifts.

### QPU JIT

Runs of QPU instructions become native code (AArch64 with NEON, on 64-bit ARM macOS and Linux)
or WebAssembly with 128-bit SIMD in the browser, through `simd-jit/`, a JIT crate shared with the
Pi 4's V3D 4.2 (adapted from the Jetson Nano's shader JIT, `maxwell-shader/src/jit`).
`vc4-3d/src/jit.rs` lowers each instruction to the crate's IR exactly as `Qpu::alu` runs it:
both pipes' operations with the QPU's denormal flushing and NaN rules, unpack and pack (16-bit
float and integer halves, bytes, saturation, colour packs), small immediates and the multiply
pipe's rotation by a constant, conditions, flags from either pipe (carries included), the SFU
into r4, per-element load immediates. A block's registers are read once and written once at its
end, flags likewise, and equal values are made once; a flush of a value that cannot be denormal
is dropped (before that, flushes were half of a fragment shader's IR).

A block takes ALU instructions and load immediates whose reads and writes stay in the QPU, plus
what the hardware hands over in order, which is fetched just before the block runs, in the order
the interpreter would fetch it: uniforms, varyings (their C coefficients into r5), pixel
coordinates and multisample flags, TMU results and colour, coverage and alpha-mask loads into r4.
It ends before a branch, a semaphore, the program end, a VPM or mutex read, a write to r5, the
TMU, the TLB, the VPM, the uniforms address or the host interrupt; those, branch delay slots and
the instructions after a program end run in the interpreter. Fragment shaders run mostly in
blocks (glmark2's phong shader: 92% of its instructions, 35 a block; effect2d's texture-heavy
one: 67%, 7 a block). Blocks are compiled once per run of instruction words (a global cache) and
found by address in each worker's cache, which keeps them from job to job and compares their
words with memory again once per job. `SBC_VC4_JIT=0` turns it off.

**Checks:** `cargo test -p simd-jit` (random IR blocks, natively compiled, against the IR's
evaluation; every half both ways), `cargo test -p vc4-3d jit` (random QPU programs of every
operation, pack, unpack, signal, condition and fetch the JIT takes, with instructions it does not
take between them, run with the JIT and with the interpreter from the same random state: every
register, accumulator, flag, the uniform pointer, the TMU queues and the order of the
environment's answers must match; code changed between jobs is not run again),
`node tools/wasm-qpu-jit-check.mjs` (both, in the browser build), and the replays below:
every frame and every job's statistics are the same with the JIT and without, on 1 and 16
threads.

## Tests

`cargo test -p vc4-3d` (59 tests) and `cargo test -p bcm2837` (`v3d_on_the_bus`):

- QPU: every add and multiply operation against Rust references, flags and all conditions
  (integer carry and borrow, float sign), packs and unpacks (16-bit float and integer halves,
  bytes, saturation, colour packs), small immediates and rotations, load immediates, the uniform
  stream and its reset, the SFU, r5 replication, branches (delay slots, link, all/any), program end,
  the TMU (direct reads and a texture), VPM generic access, two user programs that hand data over
  with a semaphore and VPM DMA, the disassembler.
- Layouts: T, LT and raster addressing against Mesa's `t_utile_address`, bijectivity for all
  sizes, tile-buffer stores and loads in every layout, BGR565.
- TMU: T-tiled nearest with repeat, bilinear with clamp, 565/4444/alpha expansion, mip level
  placement, ETC1, cube faces.
- End to end, through the registers as Linux drives them, with hand-assembled coordinate, vertex
  and fragment shaders, shader records and binning lists as Mesa writes them and rendering lists
  as the kernel builds them: clears in raster/T/LT; a pixel-aligned rectangle covering exactly its
  pixels (fill rule); indexed strips, fans and back-face culling; Gouraud and perspective-correct
  varyings; nearest texture mapping (each texel exactly); depth test in both draw orders with the
  stored Z; additive blending through the colour load; stencil marking and masking; a triangle far
  outside the guard band covering a 2x3-tile frame, near-plane clipping, w < 0; points and wide
  lines; 4x multisampling resolved; the out-of-memory wait with the kernel's overflow chunks; done
  interrupts after host time; jobs on the GPU thread (`--threads multi`); a snapshot while waiting
  for memory; identification as Mesa reads it;
  capture and replay; the same memory, byte for byte, with 1, 2, 5 and 16 render threads
  (`threads_render_the_same`); the QPU fast path against the general path.
- The machine: the registers at 0x3fc0_0000, a job writing RAM through the 0xc000_0000 alias, the
  interrupt at the ARM-side controller and its acknowledgement, a snapshot round trip.

## Debugging

- `SBC_VC4_LOG=1`: a line per job (primitives, fragments, host time).
- `SBC_VC4_CL=1`: every control-list packet both threads execute (tile lists included).
- `SBC_VC4_TRACE=N`: the first N QPU instructions, disassembled.
- `SBC_VC4_DUMP=dir`: each finished frame as a PPM.
- `SBC_VC4_THREADS=N`: host threads for a job's tiles and vertices (default: cores − 2, at most 16).
- `SBC_VC4_QPU_FAST=0`: every QPU instruction on the general interpreter path.
- `SBC_VC4_JIT=0`: the QPUs interpreted (no JIT); `SBC_VC4_JIT_DUMP=1`: each block the JIT
  compiles, its instructions and the size of its IR and code.
- `SBC_VC4_CAPTURE=dir`: every job with the RAM pages it touches (as they were before it ran) in
  `dir/jobs.bin`; `vc4-replay dir/jobs.bin [first] [count]` (`cargo run --release -p vc4-3d
  --bin vc4-replay`) runs them again on the host in milliseconds, with the options above. This is
  how guest captures (once Mesa runs in the guest) become host tests.

## Performance

A 1920x1080 frame of 7680 Gouraud-shaded, depth-tested triangles with 4x overdraw (8.3 M
fragments, 10 M QPU instructions of 16 lanes; `cargo test --release -p vc4-3d -- --ignored bench
--nocapture`): about 310 ms of host time before the work below, 237 ms on one thread with the QPU
fast path, 50 ms with 16 threads (Apple M5 Max, 18 cores, shared with other jobs: load 9–13).

**Replays** of glmark2's scenes captured in the guest (1 s of each at 1920x1080,
`SBC_VC4_CAPTURE`, then `vc4-replay` with `SBC_VC4_THREADS=1` and `16`; whole replay including
page restores, best of two, host load 16–17). Every frame and every job's statistics are the
same with 1 and 16 threads:

| Scene | jobs | 1 thread | 16 threads | |
|---|---|---|---|---|
| build | 54 | 706 ms | 297 ms | 2.4x |
| texture (linear) | 32 | 704 ms | 168 ms | 4.2x |
| shading (phong) | 21 | 727 ms | 191 ms | 3.8x |
| bump (high-poly) | 21 | 797 ms | 285 ms | 2.8x |
| effect2d (3×3 kernel) | 11 | 1195 ms | 173 ms | 6.9x |
| desktop (shadow) | 38 | 1253 ms | 239 ms | 5.3x |
| jellyfish | 13 | 1087 ms | 199 ms | 5.5x |
| shadow | 47 | 769 ms | 243 ms | 3.2x |
| refract | 57 | 855 ms | 212 ms | 4.0x |
| terrain | 276 | 4186 ms | 510 ms | 8.2x |

The scenes with little work per frame (build, bump, shadow) gain least: their binning (after
the coordinate shader) and the tile stores between jobs stay on one thread, and short jobs pay
for starting the workers. A 1080p rendering job alone scales 8–10x (terrain's 1177 ms to
113 ms, a 1080p job of 3.2 M fragments 263 ms to 35 ms).

**Where the time goes now**, one thread, by scene (samply on the replays):

| Scene | QPU (fragment) | QPU (vertex, coordinate) | TMU | tile loads/stores | rasterization, TLB, varyings | binning, vertex cache |
|---|---|---|---|---|---|---|
| shading (phong) | 56% | 7% | – | 10% | 13% | 9% |
| effect2d | 48% | – | 39% | 4% | 9% | – |
| jellyfish | 44% | 2% | 27% | 4% | 21% | 1% |
| terrain | 35% | 2% | 53% | 2% | 5% | 2% |
| build | 12% | 12% | – | 33% | 23% | 11% |

(build's tile stores were measured before they went a utile row at a time, which took its replay
from 706 to 478 ms on one thread.) The fast path takes 63% (effect2d) to 96% (phong) of the ALU
instructions; the rest are colour packs and texture-result unpacks on the general path.

### Would a QPU JIT be next?

The QPUs are still interpreted, on every worker. A shader JIT like the Jetson Nano's
(`maxwell-shader/`: AArch64 and WebAssembly backends) would compile each QPU program once per job
(Mesa's are tens to a few hundred instructions), keep
each 16-lane register in four NEON registers, and resolve operands, conditions, flags and
packs at compile time. Of the QPU time above, a third to a half is the lane arithmetic itself;
the rest is dispatch, operand copies and the general path's packing. Estimate: QPU time 2.5–4x
lower, so frames on one thread about **1.6–1.9x faster in ALU-bound scenes** (phong), **1.4–1.55x** in
texture-heavy ones (effect2d, jellyfish), **1.2–1.4x** in terrain and build, where the TMU and the tile stores remain. With 16 threads the rendering
jobs gain about as much, the frame rate a little less (binning stays serial): roughly a
glmark2 score of 90–110 instead of 70. In the browser, which renders on one thread, the
WebAssembly backend matters more than the native one.

So the JIT is the largest single step left, but the TMU (texel addressing, format decoding,
filtering per lane: up to half of a frame in texture-heavy scenes) is a cheaper one of a similar
size there: fetch paths per format and layout and integer bilinear filtering would take a good
part of it without a code generator.

## Results in the guest

Raspberry Pi OS Lite (trixie, kernel 6.18.50, Mesa 26.2.6) with Mesa, kmscube, glmark2 and gcc
(the GPU image of `scripts/raspios-gpu-setup.txt`, copy-on-write), HDMI at 1920x1080 through vc4 KMS.

**Bring-up and kmscube** (`scripts/rpi3-v3d.txt`, from the first boot, saves `out/rpi3-v3d.snap`):
`vc4-drm soc:gpu: bound 3fc00000.v3d (ops vc4_v3d_ops [vc4])`; `eglinfo` on GBM: `OpenGL ES
profile renderer: VC4 V3D 2.1`, `OpenGL ES 2.0 Mesa 26.2.6`; kmscube's lit cube on HDMI,
600 frames at 58.8 fps (guest time; each 1080p frame takes about 14 ms of host time).

![kmscube on the emulated VideoCore IV](img/rpi3-kmscube.png)

**The GLES 2 scenes of the GL suite** (`tests/gpu/gles2test.c`, 128x128) on the V3D against
llvmpipe in the same guest, a scene at a time (`tools/serial_test_script.py` writes the script:
the source goes in over the serial console, it is built with gcc in the guest):

| Scene | Result |
|---|---|
| clear, blend_modes, tex_nearest, tex_formats, tex_etc1, stencil, scissor, polygon_offset, branches | pass, exact |
| colormask, discard_fragcoord, vs_math, near_clip, loop_fragdepth | pass (max difference 1–2) |
| tri_color, depth, blend, cube, points_lines, indexed, cubemap, fbo_texture | pass (under 0.2% of pixels differ: edges) |
| tex_linear_mip | pass (0.12% of pixels, max 17: filtering at level transitions) |
| many_tris | differs (22% of pixels): 20000 sub-pixel triangles; the image matches a model of the hardware's 12.4 snapping to 3 of 16384 pixels, llvmpipe snaps to 1/256 pixel |
| derivatives_math | not run: Mesa's vc4 does not offer `GL_OES_standard_derivatives` |

The guest runs found three model bugs, all fixed with unit tests: the configuration's "clockwise"
bit means clockwise with y up (kmscube showed the cube's back faces), a conditional instruction sets
the flags only in the lanes where it executes (Mesa's loops ended after one pass), and ETC1's
differential base colours wrap as Mesa's decoder does.

**glmark2-es2-drm** (`scripts/rpi3-glmark2.txt`, 2023.01, 1920x1080 on HDMI, all scenes at their
default durations): every scene runs to its end, `GL_RENDERER: VC4 V3D 2.1`, **glmark2 Score: 21**.
The frame rates are guest-time rates: a job's done interrupt waits for the host time it took, but
at most 50 ms (well inside the kernel's hang check), so scenes whose frames take longer report about
20 fps (most of the shading scenes; their real host cost is higher). build 57 fps; texture 31/24/23;
gouraud 41; ideas 25; pulsar 26; desktop blur 2, shadow 10; terrain 3; refract 5; shadow 14;
jellyfish 21. The pictures were not compared against a reference (glmark2's own `--validate`
references do not match llvmpipe either, as on the Pi 4).

**On several host threads** (the [parallel rendering](#parallel-rendering)): the GLES 2 scenes
above give byte-identical images (the same checksum over all of them) with the default threads,
with `SBC_VC4_THREADS=1` and with `--threads multi`, and still 23 of 24 against llvmpipe. glmark2
frame by frame through `vc4-replay`: see [Performance](#performance). Frame rates with 1 thread
and with the default (16 here), `--threads multi` so the guest's clock is the host's and nothing
caps a job's time: the table in [raspberry-pi-3.md](raspberry-pi-3.md#3d-gpu-speed)
(`scripts/rpi3-glmark2-scenes.txt`, the Pi 4's scenes, 5 s each).

## Not modelled or not verified yet

- Early Z is not used to skip fragments (results are the same: the late test decides).
- BGR565 stores do not dither; the 64-bit (HDR) tile buffer has only its tile size.
- Per-sample Z in 4x mode is the pixel centre's; sample positions are a standard rotated grid.
- TMU: child images, BW1/A4/A1/RGBA64/YUV422R, the cache swizzle; no TMU or SFU latencies (results
  are there at once: correct programs do not notice).
- VPM DMA in 16- and 8-bit modes; VG shader state; the alpha-mask and VG-mask paths are minimal.
- Performance counters read 0.

## Remaining

- The desktop (Pi OS with desktop, labwc) on the GPU.
- A QPU JIT (AArch64 and WebAssembly): 1.2–1.9x per frame by the [estimate](#would-a-qpu-jit-be-next); faster TMU paths.
- Binning after the coordinate shader is serial.
