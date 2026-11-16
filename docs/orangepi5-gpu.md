# Orange Pi 5 GPU bring-up

The RK3588S uses a Mali-G610 (Valhall, command-stream frontend), rather than the Pi 4's V3D. Linux uses Panthor and Mesa's Panfrost/PanVK drivers. A scanout device is separate from the GPU. The opt-in `--gpu probe` mode presents the experimental GPU control interface to Linux. Guest graphics and GPU compute are not supported.

The first building blocks in `rk3588` are a GPU virtual-address walker for the 4 KiB AArch64 stage-1 page tables used by Panthor and a bounded interpreter for a small subset of Valhall command-stream instructions. Tests exercise memory mapping and faults, instructions and their interaction. The control register block is connected to guest MMIO and the three GPU interrupt lines. Address-space UPDATE commands latch the programmed translation settings. The command interpreter is not yet connected to a firmware scheduler.

Remaining work toward an actual guest GPU:

1. Complete the GPU register model. Initial reset, power, interrupt and MMU address-space programming are implemented; `--gpu probe` exposes a reduced device-tree node, fixed supply and clock operating point for driver bring-up. The CSF global management interface is modeled on the host; Cortex-M7 firmware instructions are not executed.
2. Extend the CSF management interface with queue scheduling, synchronization and command-stream instructions; verify a guest command submission. Panthor initialization and the GPU-info ioctl are now verified. A GPU test fixture needs matching kernel, firmware and Mesa components.
3. Implement the shader ISA and compute dispatch, then the tiler, vertex and fragment paths and descriptor formats. Compare real guest GPU output with a software renderer; add conformance tests as features are added.
4. Model RK3588 VOP2 and a display output or another explicitly described scanout path, so rendered images can reach the host. Input and desktop support need their own devices.

References: [Linux Panthor registers](https://github.com/torvalds/linux/blob/v6.18/drivers/gpu/drm/panthor/panthor_regs.h), [Panthor MMU](https://github.com/torvalds/linux/blob/v6.18/drivers/gpu/drm/panthor/panthor_mmu.c), and [Mesa Valhall command-stream descriptions](https://chromium.googlesource.com/external/gitlab.freedesktop.org/mesa/mesa/+/06efd8542f442e701685e26abe4d73c4ddf7b249/src/panfrost/genxml/v10.xml).

With the pinned Armbian 26.8.1 image, `--gpu probe --append init=/bin/bash --script scripts/orangepi5-gpu-probe.txt` initializes Panthor and creates `/dev/dri/renderD128`. The script opens that device, queries the GPU ID through the real Panthor ioctl, and waits past the first firmware watchdog ping. This verifies management and discovery, not command submission or rendering.

The host model reads the shared layout installed by Linux from the CSF firmware, validates it through GPU address translation, publishes protocol v1.0, raises the boot interrupt and services configuration, ping and halt requests. Unsupported requests are not acknowledged. Shader/graphics execution, protected mode, performance counters and queue scheduling remain unsupported. A render node alone does not establish OpenGL or Vulkan support.
