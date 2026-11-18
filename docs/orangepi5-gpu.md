# Orange Pi 5 GPU bring-up

The RK3588S uses a Mali-G610 (Valhall, command-stream frontend), rather than the Pi 4's V3D. Linux uses Panthor and Mesa's Panfrost/PanVK drivers. A scanout device is separate from the GPU. The opt-in `--gpu probe` mode presents the experimental GPU control interface to Linux. Single-target linear RGBA8 clear passes are supported. A limited Valhall compute path also runs the shader triangle demo below; fixed-function drawing remains incomplete.

The first building blocks in `rk3588` are a GPU virtual-address walker for the 4 KiB AArch64 stage-1 page tables used by Panthor and a bounded interpreter for a small subset of Valhall command-stream instructions. Tests exercise memory mapping and faults, instructions and their interaction. The control register block is connected to guest MMIO and the three GPU interrupt lines. Address-space UPDATE commands latch the programmed translation settings. A synchronous CSF queue path now connects Linux submissions to the command interpreter, including kernel CALL and completion synchronization. Unsupported work raises a queue fault.

Remaining work toward an actual guest GPU:

1. Complete the GPU register model. Initial reset, power, interrupt and MMU address-space programming are implemented; `--gpu probe` exposes a reduced device-tree node, fixed supply and clock operating point for driver bring-up. The CSF global management interface is modeled on the host; Cortex-M7 firmware instructions are not executed.
2. Extend the CSF management interface with queue scheduling, synchronization and command-stream instructions; verify a guest command submission. Panthor initialization and the GPU-info ioctl are now verified. A GPU test fixture needs matching kernel, firmware and Mesa components.
3. Extend the initial shader ISA and compute dispatch, then implement the tiler, vertex and fragment paths and descriptor formats. Compare real guest GPU output with a software renderer; add conformance tests as features are added.
4. Model RK3588 VOP2 and a display output or another explicitly described scanout path, so rendered images can reach the host. Input and desktop support need their own devices.

References: [Linux Panthor registers](https://github.com/torvalds/linux/blob/v6.18/drivers/gpu/drm/panthor/panthor_regs.h), [Panthor MMU](https://github.com/torvalds/linux/blob/v6.18/drivers/gpu/drm/panthor/panthor_mmu.c), and [Mesa Valhall command-stream descriptions](https://chromium.googlesource.com/external/gitlab.freedesktop.org/mesa/mesa/+/06efd8542f442e701685e26abe4d73c4ddf7b249/src/panfrost/genxml/v10.xml).

With the pinned Armbian 26.8.1 image, `--gpu probe --append init=/bin/bash --script scripts/orangepi5-gpu-probe.txt` initializes Panthor and creates `/dev/dri/renderD128`. The script opens that device, queries the GPU ID through the real Panthor ioctl, and waits past the first firmware watchdog ping. This verifies management and discovery, not command submission or rendering.

The host model reads the shared layout installed by Linux from the CSF firmware, validates it through GPU address translation, publishes protocol v1.0, raises the boot interrupt and services configuration, ping and halt requests. Unsupported requests are not acknowledged. Broader shader support, protected mode, performance counters, queue preemption and cross-queue synchronization remain incomplete. A render node alone does not establish OpenGL or Vulkan support.

## Linux clear-pass demo

Build with `cargo build --release`, then run:

```sh
python3 tools/panthor-demo.py --image /path/to/armbian.img
```

The tool boots the pinned Armbian image with the experimental GPU enabled, uploads a Python guest program through serial, and writes `out/panthor-clear-demo.png` plus a serial log. The base SD image stays read-only. The guest uses actual Panthor ioctls to create a VM, map a buffer, create a queue, submit Mali command streams, and wait on a DRM syncobj fence. It verifies every framebuffer byte against the scene before exporting the PNG.

The retro sunset scene is assembled from 2×2 clear regions. Its colors and region boundaries are calculated by the CPU; the emulated fragment engine writes the pixels through GPU memory mappings. This demonstrates Linux submission, command execution, fragment clear writeback and fence completion. It does not demonstrate shader execution, triangle rasterization, Mesa OpenGL/Vulkan, an interactive desktop or display scanout.

The supported fragment descriptor is deliberately narrow: one linear RGBA8 target, identity swizzle, no tiler geometry, shaders, depth/stencil, compression or MSAA. Unsupported descriptors fault rather than silently render incorrectly. Broader rendering still needs shader and tiler implementation.

## Valhall shader triangle

```sh
python3 tools/panthor-demo.py --shader --image /path/to/armbian.img \
  --output out/panthor-triangle.png
```

This Linux demo submits a Mesa-assembled Valhall compute shader through Panthor.
The shader calculates barycentric triangle coverage and RGB interpolation, executes
conditional branches, and writes four float channels per pixel to mapped memory.
The guest waits for a DRM fence, checks the output against an independent coverage
and color calculation, and converts the float buffer to PNG. Coordinates are derived from global invocation IDs, and fixed edge coefficients
are supplied as uniforms. The CPU does not supply pixel colors or coverage results.

Compute dispatch supports bounded three-dimensional workgroups with local, workgroup
and global ID preloads, using a restricted shader descriptor and FAU uniform layout. Supported instructions include
32-bit moves, floating add/FMA, integer add, floating comparisons, conditional
branches and 32/128-bit global loads/stores. Unsupported encodings, modifiers,
configuration or exhausted instruction budgets fault. The fixture source and Mesa
assembler provenance are in `tests/gpu/valhall/`.

This is compute-based shader rendering. Fixed-function vertex/tiler/fragment triangle
drawing, textures, Mesa OpenGL/Vulkan applications and
RK3588 display scanout are still incomplete. Existing clear-pass demos remain available.

The shader demo uses one `RUN_COMPUTE` with 16×16 workgroups of 8×8 local
invocations. Coordinates and output addresses are derived in the shader from
preloaded global IDs; per-pixel CPU uniforms are no longer needed. The source is
`tests/gpu/valhall/workgroup-triangle.asm`. Register allocation is decoded according
to Mesa: descriptor value 0 selects 64 registers per thread, and value 2 selects
32. Unsupported allocation values and preload configurations fault. Dispatch is
bounded to 1,024 local invocations per group, 65,536 total invocations and an
aggregate instruction budget. Workgroup offsets, merging, shared memory and
barriers remain unsupported; invocations execute sequentially.

Valhall's 32-register allocation is split: R0-R15 and R48-R63 are accessible;
R16-R47 are unavailable. This follows Mesa's `bi_allocate_registers` affinity in
[`bi_ra.c`](https://chromium.googlesource.com/external/gitlab.freedesktop.org/mesa/mesa/+/06efd8542f442e701685e26abe4d73c4ddf7b249/src/panfrost/compiler/bifrost/bi_ra.c).
The upper bank supports both reads and writes, including system-value preloads
and vector transfers. The interpreter checks every register in address pairs and
staging vectors before accessing guest memory. The 128-bit load's identity lane
encoding is accepted; reserved lane encodings are rejected.

Run the Linux shader demo in this allocation mode with:

```sh
python3 tools/panthor-demo.py --shader --shader-registers 32 \
  --image cache/orangepi/armbian.img --output out/panthor-shader-32.png
```

Its 32-register shader uses r48/r49 for the two temporaries that the 64-register
variant keeps in r16/r17. Both variants verify the same scene and DRM fence.
This register-layout fix prepares real vertex/fragment shaders, but fixed-function
triangle drawing and general Mesa applications remain incomplete.

### Vertex attribute execution

The interpreter decodes `LD_ATTR_IMM` through real Valhall resource tables,
attribute descriptors and buffer descriptors. It currently supports ordinary 1D
R32F/RG32F/RGB32F/RGBA32F attributes, RGBA/RGB1 ordering, signed byte
offsets, per-vertex indexing and power-of-two instance divisors. Buffer
bounds are checked before translation; an out-of-range element returns zero
source channels with the format's implicit alpha preserved. Descriptor bounds,
unsupported layouts and GPU memory faults stop the submission. Non-power-of-two divisors, vertex
packet buffers, packed/normalized/integer formats and 16-bit register formats
remain unsupported.

```sh
python3 tools/panthor-test.py --image cache/orangepi/armbian.img \
  --guest tests/gpu/panthor-attributes.py --success-marker PANTHOR_ATTRIBUTES_OK \
  --log out/panthor-attributes.log
```

This runs a Mesa-assembled 32-register compute shader through Linux Panthor. Each
invocation fetches a different vertex through an RG32F attribute descriptor and
writes an XY/0/1 vector. A second submission uses a divisor-four instance
descriptor to fetch 12 instance vectors. The guest verifies every vector, write
bounds and both fences.
The same attribute instruction is present in the captured GLES vertex shader.
The runner requires both the marker and a zero guest exit status, and preserves
logs on failure. Full vertex/tiler/fragment drawing is still incomplete.

### Position shader output

`LEA_BUF_IMM` now resolves structured buffer addresses and the vertex engine's
reserved position-output table 13. Address and descriptor bounds are checked
before updating the destination pair. Normal stores honor source-discard hints
on their address register; internally/externally streaming store hints use the
same GPU-MMU write path in the synchronous model. Forced discarded-thread memory
access remains unsupported.

A unit test runs all seven instruction words of the captured Mesa GLES position
shader in 32-register mode, with hardware position storage supplied as a
GPU-mapped buffer. It produces `(2, 2, 0.5, 1)`, `(14, 2, 0.5, 1)`, and
`(8, 14, 0.5, 1)` from the three input vertices and viewport uniforms.

```sh
python3 tools/panthor-test.py --image cache/orangepi/armbian.img \
  --guest tests/gpu/panthor-position.py --success-marker PANTHOR_POSITION_OK \
  --log out/panthor-position.log
```

The Linux test adapts that shader to a compute submission: it copies global X to
the internal-ID register and binds output to an ordinary structured buffer in
table 3 instead of hardware position table 13. It verifies the same vectors,
write bounds and a DRM fence. The actual GLES `RUN_IDVS` command, tiler heap
lifecycle, primitive assembly and fragment/rasterizer integration remain
unfinished; completing position instructions alone does not render a triangle.
