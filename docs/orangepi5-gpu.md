# Orange Pi 5 GPU bring-up

The RK3588S uses a Mali-G610 (Valhall, command-stream frontend), rather than the Pi 4's V3D. Linux uses Panthor and Mesa's Panfrost/PanVK drivers. A scanout device is separate from the GPU. The opt-in `--gpu probe` mode presents the experimental GPU control interface to Linux. Single-target linear RGBA8 clear passes are supported. A limited scalar Valhall compute path also runs the shader triangle demo below; fixed-function drawing remains incomplete.

The first building blocks in `rk3588` are a GPU virtual-address walker for the 4 KiB AArch64 stage-1 page tables used by Panthor and a bounded interpreter for a small subset of Valhall command-stream instructions. Tests exercise memory mapping and faults, instructions and their interaction. The control register block is connected to guest MMIO and the three GPU interrupt lines. Address-space UPDATE commands latch the programmed translation settings. A synchronous CSF queue path now connects Linux submissions to the command interpreter, including kernel CALL and completion synchronization. Unsupported work raises a queue fault.

Remaining work toward an actual guest GPU:

1. Complete the GPU register model. Initial reset, power, interrupt and MMU address-space programming are implemented; `--gpu probe` exposes a reduced device-tree node, fixed supply and clock operating point for driver bring-up. The CSF global management interface is modeled on the host; Cortex-M7 firmware instructions are not executed.
2. Extend the CSF management interface with queue scheduling, synchronization and command-stream instructions; verify a guest command submission. Panthor initialization and the GPU-info ioctl are now verified. A GPU test fixture needs matching kernel, firmware and Mesa components.
3. Extend the initial shader ISA and scalar compute dispatch, then implement the tiler, vertex and fragment paths and descriptor formats. Compare real guest GPU output with a software renderer; add conformance tests as features are added.
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
and color calculation, and converts the float buffer to PNG. Coordinates and fixed
edge coefficients are supplied as uniforms; the CPU does not supply pixel colors
or coverage results.

The initial compute dispatch supports one invocation per `RUN_COMPUTE`, with a
restricted shader descriptor and FAU uniform layout. Supported instructions include
32-bit moves, floating add/FMA, integer add, floating comparisons, conditional
branches and 32/128-bit global loads/stores. Unsupported encodings, modifiers,
configuration or exhausted instruction budgets fault. The fixture source and Mesa
assembler provenance are in `tests/gpu/valhall/`.

This is compute-based shader rendering. Fixed-function vertex/tiler/fragment triangle
drawing, multi-invocation workgroups, textures, Mesa OpenGL/Vulkan applications and
RK3588 display scanout are still incomplete. Existing clear-pass demos remain available.
