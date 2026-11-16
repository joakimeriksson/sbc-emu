# Orange Pi 5 GPU bring-up

The RK3588S uses a Mali-G610 (Valhall, command-stream frontend), rather than the Pi 4's V3D. Linux uses Panthor and Mesa's Panfrost/PanVK drivers. A scanout device is separate from the GPU. The emulator does not yet present a GPU or display to Linux, and no guest graphics or GPU compute is supported.

The first building blocks in `rk3588` are a GPU virtual-address walker for the 4 KiB AArch64 stage-1 page tables used by Panthor and a bounded interpreter for a small subset of Valhall command-stream instructions. Tests exercise memory mapping and faults, instructions and their interaction. These modules are not connected to guest MMIO or a firmware scheduler; passing their tests is not a Linux GPU test.

Remaining work toward an actual guest GPU:

1. Emulate the GPU registers, reset, power and interrupt behavior and the MMU address-space banks. Introduce a device-tree node only when the driver can handle it correctly.
2. Implement the CSF firmware interface, queue scheduling, synchronization and command-stream instructions; verify Panthor device initialization and a guest command submission. A GPU test fixture needs matching kernel, firmware and Mesa components.
3. Implement the shader ISA and compute dispatch, then the tiler, vertex and fragment paths and descriptor formats. Compare real guest GPU output with a software renderer; add conformance tests as features are added.
4. Model RK3588 VOP2 and a display output or another explicitly described scanout path, so rendered images can reach the host. Input and desktop support need their own devices.

References: [Linux Panthor registers](https://github.com/torvalds/linux/blob/v6.18/drivers/gpu/drm/panthor/panthor_regs.h), [Panthor MMU](https://github.com/torvalds/linux/blob/v6.18/drivers/gpu/drm/panthor/panthor_mmu.c), and [Mesa Valhall command-stream descriptions](https://chromium.googlesource.com/external/gitlab.freedesktop.org/mesa/mesa/+/06efd8542f442e701685e26abe4d73c4ddf7b249/src/panfrost/genxml/v10.xml).
