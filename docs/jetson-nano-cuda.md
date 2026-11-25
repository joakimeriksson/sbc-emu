# CUDA and TensorRT on the Jetson Nano

NVIDIA's own GPU stack runs on the emulated Jetson Nano: the kernel of Linux for Tegra (L4T
R32.7.3, JetPack 4.6.3) with NVIDIA's GPU driver `nvgpu`, the CUDA 10.2 driver `libcuda`, and
TensorRT 8.2 with cuDNN 8.2 and cuBLAS 10.2. CUDA kernels, compiled by NVIDIA's PTX JIT or shipped
as Maxwell code in NVIDIA's libraries, run on the GM20B model ([jetson-nano-gpu.md](jetson-nano-gpu.md)).

YOLOv8n through TensorRT finds what ONNX Runtime finds on the host, box for box:

```
DET person 0.87 [669,394,140,485]
DET bus 0.87 [19,230,774,526]
DET person 0.87 [49,399,197,504]
DET person 0.83 [222,406,123,452]
DET person 0.27 [-0,550,63,323]
```

One inference takes 280–350 ms of GPU time on an Apple M-series Mac (the emulator charges a GPU job
the host time it took); building the engine, which times every candidate kernel, about ten
minutes.

## What runs where

L4T's kernel boots from the command line (as Debian's does) with NVIDIA's device tree for the
developer kit; the root file system is Debian 13's, from the card, with NVIDIA's userspace,
firmware and modules unpacked into it under `/opt/l4t` and `/usr/lib`. NVIDIA's bootloaders and
its Ubuntu 18.04 image are not used.

| Piece | From |
|---|---|
| Kernel, device tree, modules (`nvgpu.ko`) | L4T R32.7.3 driver package |
| `libcuda`, PTX JIT, GPU firmware (`gm20b/`) | its `nvidia_drivers.tbz2` |
| TensorRT, cuDNN, cuBLAS, CUDA runtime | NVIDIA's Jetson apt repository (r32.7) |
| Root file system | Debian 13 (the card of [jetson-nano.md](jetson-nano.md#debian-13)) |

These are NVIDIA's files under NVIDIA's licence: the fixture downloads them into `cache/l4t/`,
which is not committed, and checks their SHA-256.

## Steps

```sh
scripts/jetson-nano-l4t-fixture.sh                 # cache/l4t/serve: NVIDIA's files and the tests
scripts/jetson-nano-debian-fixture.sh              # Debian's kernel and initramfs (to prepare the card)
scripts/jetson-nano-card-fixture.sh                # Debian's card
cp cache/jetson-nano/debian/disk.raw cache/l4t/disk.raw && truncate -s 8G cache/l4t/disk.raw
```

Prepare the card once, in Debian, with the files served from the host (the guest reaches it at
10.0.2.2); the command line is in `scripts/jetson-nano-l4t-prepare.txt`:

```sh
python3 -m http.server 8800 --bind 127.0.0.1 -d cache/l4t/serve &   # the guest's 10.0.2.2 is the host's localhost
sbc-emu run --board jetson-nano ... --sd cache/l4t/disk.raw --sd-persist --script scripts/jetson-nano-l4t-prepare.txt
```

Then boot L4T's kernel with the card's root read-only and a shell as init:

```sh
S=cache/l4t/serve
sbc-emu run --board jetson-nano --kernel $S/Image --dtb $S/tegra210-p3448-0000-p3449-0000-b00.dtb \
  --sd cache/l4t/disk.raw --hdmi-mode off --append "console=ttyS0,115200 root=/dev/mmcblk0p1 rootwait ro \
  init=/bin/sh nowatchdog clk_ignore_unused initcall_blacklist=tegra_pcie_init_driver" \
  --script scripts/jetson-nano-cuda.txt --timeout 600
```

`scripts/jetson-nano-cuda.txt` loads `nvgpu` and runs `tests/gpu/l4t/cuda-vadd.c`: the CUDA driver
API without a toolkit (`dlopen` of `libcuda`), a hand-written PTX vector add JIT-compiled and
launched, the result checked (`CUDA_VADD OK`).

For TensorRT, export YOLOv8n and prepare its input on the host before preparing the card (the
reference detections are printed):

```sh
cd cache/l4t/serve && uv run --with ultralytics yolo export model=yolov8n.pt format=onnx imgsz=640 opset=12 && cd -
curl -LO https://ultralytics.com/images/bus.jpg
uv run --with opencv-python-headless --with onnxruntime python tests/gpu/l4t/yolo-input.py bus.jpg cache/l4t/serve
```

and run `scripts/jetson-nano-tensorrt.txt` with the same command line and `--timeout 3000`:
`trtexec` builds the engine from the ONNX file (TensorRT's own kernels: cuDNN and cuBLAS tactics
off), runs one inference, and `tests/gpu/l4t/trtpost.py` decodes the boxes.

The kernel command line: `nowatchdog` (L4T's buddy watchdog reads a core's timer interrupts as a
hard lockup when the emulated cores take turns), `clk_ignore_unused` (L4T's PMC clocks), and no
PCIe driver (L4T's fails on the emulated controller's PHY; the card is not on PCIe). `env.sh`
turns `nvgpu`'s channel watchdog off: an interpreted kernel can take longer than its five seconds.

## What the model does for NVIDIA's stack

Beyond what nouveau needs:

- The secure monitor's calls L4T makes: PMC register access (`0xc2fffe00`), VPR (`0x82000003`).
  The SMMU is on at reset, as NVIDIA's boot firmware leaves it; a client group's register holds an
  address space per 4 GiB window (L4T's domains are 16 GiB).
- I²C completion as L4T's driver waits for it, SDHCI v4 mode (16-byte ADMA descriptors), SD DMA
  through the SMMU.
- The GPU: the noise-aware PLL's calibration, unit counts, the L2's compression parameters, the
  FECS arbiter, the PMU's data ports; host1x sync point increments and waits from the push
  buffers; semaphore reductions; a channel bound again to the same instance block keeps its state
  (nvgpu after power gating); channels of one graphics context share its 3D/compute state
  (CUDA's streams).
- The shader core: LEA, VADD, P2R, R2P and FCHK, which NVIDIA's compiler uses; compute
  workgroups in 32-lane warps, on several host threads.
- The CPU: an event register (`SEVL; WFE` falls through), which L4T's 4.9 kernel's locks need.

## Not there yet

- FP16 and INT8 TensorRT engines (half-precision instructions are untested), cuDNN and cuBLAS
  tactics, CUDA's video and graphics interop, NVIDIA's Ubuntu image and bootloaders.
- L4T's boot sometimes waits minutes before starting init: a driver probe sleeping on an idle core
  is not woken in time (the per-core timer interrupt arrives late); what runs afterwards is not
  affected.

Debugging: `SBC_SHADER_FAULT=1` prints the instruction a shader stops at and unmapped GPU accesses,
  `SBC_SHADER_DUMP=1` every compute program, `SBC_TRACE_SYNCPT=1` sync point and semaphore
  waits, `SBC_TRACE_SEM=1` every semaphore release.
