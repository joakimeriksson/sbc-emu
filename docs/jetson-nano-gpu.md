# The Jetson Nano's GPU (GM20B, Maxwell)

Debian 13's kernel (nouveau) and Mesa (nvc0, 25.0.7) run unmodified on a register-level model of the
GPU: "NV12B / OpenGL ES 3.2". Nothing of NVIDIA's is needed or distributed.

```sh
sh scripts/jetson-nano-debian-fixture.sh cache/jetson-nano/debian
# in the guest (Debian 13 from the card): apt install libegl1 libgles2 libgl1-mesa-dri glmark2-es2-drm kmscube
kmscube -D /dev/dri/card0          # or: glmark2-es2-drm
```

## The pieces

| Crate | What |
|---|---|
| `tegra210/src/gpu/` | BAR0 and BAR1, page tables and TLB (through the SMMU where an address says so), the PMU's and FECS's answers, runlists and channels, push buffers, semaphores, the copy engine (B0B5) |
| `maxwell/` | the engine: 3D (B197) with its macro processor, 2D (902D), compute (B1C0), inline to memory (A140) |
| `maxwell-shader/` | the shader instruction set: decoder, disassembler, interpreter (16 or 32 lanes at a time), program headers |
| `maxwell-tex/` | texture and sampler descriptors, block-linear layouts, filtering, BC1-7, ETC2/EAC, render target and depth formats |

**Firmware.** The GPU's microcontrollers (PMU, FECS, GPCCS) need NVIDIA's signed firmware on real
hardware. The model does not run it: it answers the messages nouveau exchanges with it. nouveau still
wants files with valid headers, so the guest has placeholders (`tools/nano_gpu_firmware.py`) without
any code. With NVIDIA's own files in their place nouveau should load those the same way; this has not
been tried.

**A draw.** Vertices are fetched and shaded once each, sixteen at a time; tessellation, geometry shaders
and transform feedback follow. Triangles are clipped, set up and binned to 32 x 32 pixel tiles, and the
tiles are drawn on several host threads (`SBC_MAXWELL_THREADS`). Four quads, of different triangles if
need be, are shaded together; depth is tested before shading unless the shader can change it. Tiles
stay in host memory until the driver synchronises or changes render targets. Compute runs on one thread.

## Tests

Scenes rendered by the GPU and by Mesa's software renderer in the guest, compared per pixel (the test
programs' `cmp`), results on Debian 13:

| Suite | Result |
|---|---|
| `tests/gpu/gles2test.c` (GLES 2) | 24 of 25 against softpipe; `tex_etc1` has random blocks that are not valid ETC1, which ETC2 hardware decodes in ETC2's way |
| `tests/gpu/gltest.c` (GLES 3) | 48 of 49 against llvmpipe; in `atlas_pbo` llvmpipe filters at texel centres and is off itself |
| `tests/gpu/gles32test.c` (GLES 3.2) | 33 of 33 against llvmpipe |

`tests/gpu/run-nano.sh` is the guest's side (`scripts/jetson-nano-gl.txt`; start `tools/gpu_server.py`
first). `cargo test -p maxwell -p maxwell-shader -p maxwell-tex` has the unit tests: shaders compiled by
Mesa with their reference results, block formats against Apple's texture units, layouts against the
drivers' formulas.

Host time per 1280 x 720 frame on an M-series Mac (sixteen threads): glmark2 build 3 ms, phong shading
5 ms, effect2d 8 ms, jellyfish 12 ms, refract 32 ms. The GPU takes no virtual time, so the guest's own
frame rates mean little.

## Not done

- ASTC textures, colour keys, compression and ZCULL (both only speed), several viewports, conservative
  rasterization, sample shading's custom positions (the standard pattern is always used).
- Page faults are not reported to the driver: unmapped memory reads as zero.
- Multiple vertex streams in transform feedback; subgroups wider than 16 lanes in compute.
- Vulkan (NVK) and NVIDIA's own driver have not been tried.
- The GPU runs on the CPU's thread (a core waits while it draws) and is not in the browser build's pages.

`SBC_TRACE_GPU=1` prints register accesses, `SBC_TRACE_GPFIFO=1` every method.
