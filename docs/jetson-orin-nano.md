# NVIDIA Jetson Orin Nano 4GB

`--board jetson-orin-nano-4gb`, experimental.

The Tegra234 target boots an arm64 Linux kernel directly using a generated
device tree. The bring-up configuration has 4 GiB of RAM and six Cortex-A78AE cores across two
clusters. It provides host PSCI, a GICv3 distributor and per-core redistributors,
ARM architectural timers, and a serial console on the SBSA UART at
`0x031d0000`. The CLI supports the serial runner, scripts, and GDB.

## Boot Alpine

```sh
cargo build --release -p sbc-emu
sh scripts/jetson-nano-fixture.sh cache/jetson-orin-nano orin
D=cache/jetson-orin-nano
target/release/sbc-emu run --board jetson-orin-nano-4gb \
  --kernel "$D/vmlinuz" --initrd "$D/initramfs.cpio.gz" \
  --script scripts/jetson-orin-nano-shell.txt --timeout 120
```

The fixture uses the checksummed Alpine packages shared with the Tegra210
fixture. For interactive use, omit `--script` and `--timeout`. Use
`--ram-mb 768` for a smaller interactive guest, `--smp 1` for single-core bring-up,
or `--no-jit` for the interpreter, and `--threads single|multi` to run the cores
on one host thread or one each ([below](#speed)). The scripted check expects six cores and 4 GiB.
It checks an architectural timer interrupt and a GIC IPI on every core, then
offlines and onlines CPU 5.

## Speed

`--threads single|multi` chooses how the six cores run. `single` (the default, and the only
way in the browser): they take turns on one host thread in rounds of virtual time, and a run
with the same inputs is repeatable. `multi`: a host thread per core, with the design of the
Jetson Nano's ([jetson-nano.md](jetson-nano.md#speed), `tegra234/src/mt.rs`): RAM is shared
directly, barriers and acquire/release are host fences and store-exclusive a host
compare-and-swap, TLB and instruction-cache invalidations reach the other cores before the
issuing core goes on, and the devices (GICv3 and its ICC system registers, HSP and BPMP,
the UARTs, xHCI and its USB devices, host1x, the GPU) sit behind one lock. Guest time follows
the host's clock, and runs are not repeatable. Linux ticks on the arch timer: a core asleep in
WFI waits for its timer, an interrupt or the next device event (the rover UART's receive
timeout, USB, the network). A GPU doorbell or BPMP request is served on the thread of the
core that rang it, holding the lock meanwhile. The host side of the rover UART and the USB
serial port is served between slices of about 10 ms; a slice ends early when the guest has
drained the rover UART's receive FIFO or written to it, so the link keeps its rate. A
debugger's breakpoints, watchpoints and single steps run on one thread.

Six CPU-bound jobs in the Alpine fixture, host seconds on an 18-core Apple M5 Max
(`--threads single` / `multi`):

| Guest work | single | multi |
|---|---|---|
| one `dd if=/dev/zero bs=1M count=480 \| md5sum` | 1.39 | 1.09 |
| six of them at once | 9.17 | 1.61 |
| one busybox `sh` loop of 400,000 increments | 1.98 | 1.99 |
| six of them at once | 15.18 | 5.51 |

The shell loops scale less: they make the kernel shoot down TLB entries about 4,000 times a
second, and each shootdown waits until every running core has applied it.

The boot scripts (shell, rover UART, USB with BPMP and the Tegra UART driver, GPU probe and
power-on) and `tests/gpu/run-orin-cuda.sh` pass in both modes.

## USB

`--usb-drive IMAGE` plugs a USB stick into the board, and `--camera` plugs in a webcam.
`--camera-source SPEC` gives the webcam pictures from the host, as on the
[Jetson Nano](jetson-nano.md). The drive is copy-on-write: each boot starts from the image.

- **The controller:** sbc-emu's shared xHCI core at the Orin's XUSB host address `0x03610000`,
  on SPI 163. The device tree names it `generic-xhci`, so Linux's platform xHCI driver runs it.
  The JetPack kernel builds that driver in, together with USB storage and ext4.
- **Not modelled:** NVIDIA's XUSB firmware, the pad controller and the device-mode port.
  The webcam needs the `uvcvideo` module from the kernel's module package.

```sh
printf SBC_EMU_USB_DRIVE > /tmp/usb.img && truncate -s 64m /tmp/usb.img
target/release/sbc-emu run --board jetson-orin-nano-4gb --bpmp on --uart-driver tegra \
  --usb-drive /tmp/usb.img --camera \
  --kernel cache/jetson-r3644/Image --initrd cache/jetson-orin-nano/initramfs.cpio.gz \
  --script scripts/jetson-orin-nano-usb.txt --timeout 300
```

The script reads the drive's first bytes through `/dev/sda` and finds the camera in sysfs.

### Booting a full JetPack image from the drive

The drive can hold a whole JetPack root filesystem, such as a vendor's ready-made image. Boot
that image's own kernel (`/boot/Image` on its `APP` partition) with
`root=PARTUUID=<APP's PARTUUID> rootwait rootfstype=ext4`. The image's own command line
(extlinux.conf's `APPEND`, without UEFI's `${cbootargs}`) works unchanged: its
`console=ttyTCU0,115200` is the [Tegra Combined UART](#console-ttytcu0). `console=ttyAMA0` with
`earlycon=pl011,mmio32,0x031d0000` still works too, and shows the kernel's first messages
earlier.

On a real Orin, `${cbootargs}` is what UEFI (L4TLauncher) puts there from the device tree's
`/chosen/bootargs`; on JetPack 6 that adds no console or earlycon, only firmware bookkeeping such
as `bl_prof_dataptr=`/`bl_prof_ro_ptr=` (the boot profiler's carve-outs), which sbc-emu does not
have.

Such images are often cut short after their last partition: the GPT describes the original disk,
which can be larger than the file. Linux rejects a GPT whose backup header lies past the end of
the disk, so sbc-emu presents the drive at the size the GPT gives. The extra sectors read as zeros.

## Console: ttyTCU0

JetPack's kernels name `console=ttyTCU0`: the Tegra Combined UART. The CPUs reach the debug UART
only through the always-on cluster's firmware (SPE), over HSP shared mailboxes. sbc-emu models that
link as NVIDIA's device tree wires it (`tegra234/src/tcu.rs`): the AON HSP (`0x0c150000`, shared
interrupts 1–4 on SPIs 133–136) and TOP0's shared mailboxes and shared interrupts 0–7 (SPIs
120–127). Linux's built-in `tegra-tcu` driver sends up to three bytes per message on AON shared
mailbox 1; the firmware side takes each at once and clears the mailbox. Typed bytes go back three at
a time on TOP0 shared mailbox 0, as the guest empties it.

TCU output reaches the same host console as the SBSA UART's (`ttyAMA0`), so the terminal and
`--script`'s `wait-serial`/`serial` work with either. Input goes to the TCU when the last serial
`console=` on the kernel command line is `ttyTCU…`, else to the SBSA UART. Without `earlycon` the
kernel log appears once the TCU driver registers (about 0.4 s into boot), from the start.

## PMC scratch registers

The PMC's scratch window (`0x0c390000`, 64 KiB) is modelled as retention registers, all zero at
power-on. NVIDIA's `nvbootctrl` maps it through `/dev/mem`: SCRATCH `0x3cc` holds the bootloader
slots' status, `0x3a4`/`0x3a8` the rootfs A/B state (`nvbootctrl verify` clears `0x3a8`). Without it
`nv-l4t-bootloader-config.service` died with a bus error. The rest of the PMC is not modelled.

`nvbootctrl dump-slots-info` and `get-current-slot` still fail: they read UEFI variables
(`BootChainFwCurrent`, `BootChainFwStatus`, `RootfsRedundancyLevel`, … in NVIDIA's GUID
`781e084c-a330-417c-b678-38e696380cb9`) from `/sys/firmware/efi/efivars`, and the ESRT
(`/sys/firmware/efi/esrt`) for the firmware version. Those need UEFI runtime services, which a
direct kernel boot does not have. The bootloader's QSPI flash is not used by `nvbootctrl` on
JetPack 6 (UEFI owns it) and is not modelled.

## Network

The Orin Nano's own Ethernet port is not modelled. `--net nat` plugs a USB Ethernet adapter into
the board instead: a CDC-ECM device, which Linux's `cdc_ether` driver runs without a vendor
driver. It is the same virtual network as on the other boards (sbc-net: DHCP, DNS, ICMP), and
`nat` relays the guest's TCP and UDP through the host's sockets. `--net local` keeps the network
isolated. Without `--net` no adapter is plugged in.

`--hostfwd tcp:[HOSTADDR]:HOSTPORT-[GUESTADDR]:GUESTPORT` (QEMU's syntax, repeatable) forwards a
host TCP port to the guest. It needs `--net nat`.

The adapter's MAC address is universally administered, so Linux names it `eth0` with
`net.ifnames=0`, or `enx001de15b0e01` with predictable names. The kernel needs `cdc_ether`; the
JetPack kernels build it as a module, which a full root filesystem loads by itself.

```sh
target/release/sbc-emu run --board jetson-orin-nano-4gb --bpmp on --uart-driver tegra \
  --usb-drive JETPACK.img --kernel Image --append "... root=PARTUUID=... net.ifnames=0" \
  --net nat --hostfwd tcp::5000-:5000
```

A web server listening on the guest's port 5000 then answers at `http://127.0.0.1:5000/` on the host.

## USB serial

`--usb-serial HOST:PORT` plugs a USB serial port into the board: a CDC-ACM device, which Linux's
`cdc-acm` driver shows as `/dev/ttyACM0`. USB lidars, GPS receivers and microcontroller boards
present themselves this way. The emulator connects to HOST:PORT over TCP, retrying every second
until a peer listens. What the peer sends, the guest reads from the port; what the guest writes
goes to the peer. A peer that goes away leaves the port idle, and the emulator reconnects.

The guest's line coding (baud rate, framing) is kept and reported back, but it does not pace the
bytes: they arrive as the peer sends them. A guest that has not opened the port loses what the peer
sends meanwhile, as a real device's output is lost before the host listens.

## Rover UART

The bring-up device tree exposes Tegra UART A at `0x03100000` as `/dev/ttyS1`
through the Alpine fixture's `8250_tegra` driver. Open it at 115200 baud in raw mode.
The port is independent of the SBSA console.

Use `--rover-uart loopback` to test the port without a peer, or pass the numeric
address of the ESP32 simulator's UART TCP listener, for example
`--rover-uart 127.0.0.1:5500` with esp32sim's `--uart-tcp 127.0.0.1:5500`.
The link carries raw bytes. Start the listener before the board and keep it
running. A disconnected peer ends the run with an error.

```sh
scripts/quickstart.sh jetson-orin-nano-4gb --rover-uart loopback \
  --script scripts/jetson-orin-nano-uart.txt --timeout 60
```

The loopback script compares returned bytes for a two-byte write and a burst
larger than the RX FIFO.

### High-speed Tegra UART driver

Use `--uart-driver tegra` with a kernel built with `CONFIG_SERIAL_TEGRA`.
It binds the same MMIO port to Linux's Tegra PIO driver at `/dev/ttyTHS1`,
the device expected by the Waveshare ROS driver.

By default, the bring-up device tree provides a 115200-baud clock and a simple
reset register at `0x70000000`. The driver can warn when it tries its initial
9600-baud setting. Configure the port at 115200 baud before sending data.

The Debian kernel pinned by the Orange Pi 5 fixture has the driver built in.
Use that kernel with the Orin initramfs to run the same byte comparison through
`ttyTHS1`:

```sh
sh scripts/orangepi5-fixture.sh cache/orin-debian-kernel
sh scripts/jetson-nano-fixture.sh cache/jetson-orin-nano orin
target/release/sbc-emu run --board jetson-orin-nano-4gb \
  --uart-driver tegra --rover-uart loopback \
  --kernel cache/orin-debian-kernel/Image \
  --initrd cache/jetson-orin-nano/initramfs.cpio.gz \
  --script scripts/jetson-orin-nano-uart.txt --timeout 60
```

## HSP and BPMP

Pass `--bpmp on` to expose the HSP TOP0 doorbells and BPMP shared-memory
queues in SYSRAM. With `--uart-driver tegra`, the device tree then points UART A
to BPMP for its programmable clock and reset. The guest kernel needs the
Tegra HSP mailbox, SRAM, and BPMP drivers for this configuration.

The firmware model follows the [Linux BPMP transport](https://github.com/torvalds/linux/blob/master/drivers/firmware/tegra/bpmp-tegra186.c)
and [BPMP ABI](https://github.com/torvalds/linux/blob/master/include/soc/tegra/bpmp-abi.h).
It implements IVC reset and request/reply handling, firmware identification, and
the clock, reset, and power-domain requests for the modeled devices. Run
`cargo test -p tegra234` to exercise atomic and threaded requests over the SoC
bus, including UART clock and reset changes and GPU power gating. The Debian
fixture can boot with the generated nodes but lacks the HSP mailbox driver.

### NVIDIA kernel smoke test

Use NVIDIA's [Jetson Linux R36.4.4](https://developer.nvidia.com/embedded/jetson-linux-r3644)
arm64 `nvidia-l4t-kernel` package version `5.15.148-tegra-36.4.4-20250616085344`.
Its SHA-256 is `f26ccf85c64f9d4c36001b0d04df8c8bdd370aa173ebdcacb4f4fc3995420500`.
Extract `boot/Image` to `cache/jetson-r3644/Image`, then use the Alpine initramfs
from the Orin fixture.

```sh
target/release/sbc-emu run --board jetson-orin-nano-4gb \
  --bpmp on --uart-driver tegra --rover-uart loopback \
  --kernel cache/jetson-r3644/Image \
  --initrd cache/jetson-orin-nano/initramfs.cpio.gz \
  --script scripts/jetson-orin-nano-uart.txt --timeout 60
```

The kernel reports `tegra-bpmp bpmp: firmware: sbc-emu-tegra234` and registers
`ttyTHS1`. The script verifies the short and burst byte comparisons through
that device at 115200 baud.

## Ampere GPU bring-up

`--gpu on` enables BPMP and exposes the GA10B node derived from NVIDIA's
P3767-0004 device tree. Like `tegra210/src/gpu/`, `tegra234/src/gpu/` owns the
register model. BAR0 exposes the GPU identity, PTIMER and the privileged-ring
startup handshake. BPMP controls register access and resets the register state.
Set `SBC_TRACE_GPU=1` to trace BAR0 accesses using the same switch as Tegra210.

### VNC copy-engine demo

The freestanding arm64 demo powers GA10B on through BPMP, builds a copy
channel and maps its buffers through the five-level GMMU. Each frame submits
Ampere C7B5 DMA copies over GPFIFO to compose a moving rover on a 640 × 360
framebuffer. A separate graphics channel uses a C797 color clear to render
the rover's headlamp. The guest checks both semaphores, both USERD pointers
and framebuffer contents before advancing. The emulator scans out the
framebuffer through the standard VNC server. [NVIDIA Ampere Copy B class](https://github.com/NVIDIA/open-gpu-kernel-modules/blob/main/src/common/sdk/nvidia/inc/class/clc7b5.h)

```sh
cargo build --release -p sbc-emu
sh demos/orin-copy/build.sh
target/release/sbc-emu run --board jetson-orin-nano-4gb --smp 1 --ram-mb 256 \
  --gpu on --kernel cache/jetson-orin-nano/copy-demo.Image --vnc :7,password=orin
```

Connect a VNC viewer to `127.0.0.1:5907` with password `orin`. The demo
keeps animating until the emulator exits. The opt-in display also provides
a Linux simple-framebuffer at `0x8ff00000`.

```sh
python3 demos/orin-copy/rfb-smoke.py --output cache/jetson-orin-nano/copy-demo-vnc.png
```

The smoke client handshakes with the same VNC server over `--vnc stdio`,
decodes full and incremental RFB updates, and checks that the rover and its
headlamp moved. The first copy runlist is provisionally placed at BAR0 offset `0x800000`
for bare-metal bring-up. The copy model covers pitch-linear and block-linear
surfaces on 4 KiB and 64 KiB pages, multiline blits, component remapping and
constant fills, and one- and four-word semaphore writes. The demo uses a simple
framebuffer for scanout.

To run the same image in the site worker, build the demo and stage a local
WebAssembly build. Open `board.html?b=jetson-orin-nano-gpu` on the local site.

```sh
sh demos/orin-copy/build.sh
tools/wasm-build.sh
node tests/gpu/orin/wasm-smoke.mjs
python3 -m http.server -d web 8791
```

The Node test runs the site worker with the AArch64 JIT and checks the rover's
motion in the 640 × 360 RGBA frames it sends to the page. With the NVIDIA
fixture prepared, run `SBC_ORIN_WASM=1 sh tests/gpu/run-orin-ioctl.sh` to
boot Linux and run the same GPU tests in WebAssembly.

### Driver binding

The probe fixture stages NVIDIA's R36.4.4 `nvgpu`, dependencies, and GA10B
firmware in a second initramfs archive. The script verifies the `gk20a` driver symlink and
the `/dev/nvgpu/igpu0/power` character device. Building the fixture needs
`bsdtar` with Zstandard support, `cpio`, and `shasum`. macOS's own `bsdtar` has no
Zstandard; Homebrew's `libarchive` has (`brew install libarchive`, then put
`$(brew --prefix libarchive)/bin` first in `PATH`).

```sh
sh scripts/jetson-nano-fixture.sh cache/jetson-orin-nano orin
sh scripts/jetson-orin-nano-gpu-fixture.sh
target/release/sbc-emu run --board jetson-orin-nano-4gb --gpu on \
  --kernel cache/jetson-r3644/Image \
  --initrd cache/jetson-r3644/probe-initramfs.cpio.gz \
  --script scripts/jetson-orin-nano-gpu-probe.txt --timeout 60
```

Use `scripts/jetson-orin-nano-gpu-power.txt` in the same command to exercise
runtime power-on. NVIDIA's R36.4.4 driver returns `NVGPU_POWER_ STATUS=0` and the
fixture opens `/dev/nvgpu/igpu0/ctrl`. Run `sh tests/gpu/run-orin-ioctl.sh`
after building the fixture to check GPU characteristics, allocate a GPU
address space, and submit a copy through nvgpu. The test checks that both
words arrive across GPU page boundaries, checks the copy-engine semaphore,
and waits on the Linux sync-file fence. It also checks the guest's interrupt
counters for delivery of Host1x syncpoint threshold interrupts. On the
graphics runlist it dispatches an Ampere shader that loads a launch constant
and runs a predicated loop. It compares the four results and surrounding guard
words with CPU expectations. It
also checks clipped and channel-masked Ampere C797 color clears in both
pitch-linear and block-linear render targets. It checks that array-layer and
attachment selection preserve neighboring layers, and verifies independent
depth and stencil clears in a packed Z24S8 target. Further checks cover the
intersection of the clear rectangle, scissor and viewport, and confirm that
a stencil write mask preserves the other bits.

The GPU model exposes separate graphics and asynchronous copy runlists and
maps BAR1 through the GA10B page tables. It models Falcon firmware uploads,
the GSP startup interrupt, and the scheduler messages for context binding,
engine registration and initial domain creation. FECS also acknowledges the
driver's initial context bind and save requests. These are firmware protocol
models. The Tegra234 Host1x model supports CPU and GPU syncpoint increments
and threshold interrupts. The Ampere Compute B model dispatches QMD 3.0
workgroups on the shared Maxwell shader interpreter (`maxwell-shader`) with its SM70
decoder: 32-lane warps, shared memory and barriers, global, local and atomic memory,
and QMD-bound constant buffers. The C797 clear path shares color and
depth/stencil formats and surface layout code with the Tegra210 renderer.
[NVIDIA Ampere 3D class](https://github.com/NVIDIA/open-gpu-doc/blob/master/classes/3d/clc797.h)

### JetPack CUDA driver

`sh tests/gpu/run-orin-cuda.sh` boots NVIDIA's R36.4.4 modules and userspace.
It verifies `cuInit`, enumeration of one Orin GPU with compute capability 8.7,
context creation, device allocation, a known-value host/device round trip,
and a PTX kernel whose result is read back. NVIDIA's JIT compiler emits SM87
instructions, and the driver submits an inline QMD and waits on its semaphore.
Set `SBC_ORIN_WASM=1` when running the CUDA test to verify the same workload
with the WebAssembly emulator. CUDA uses the Ampere copy engine for transfers and constant fills and the
compute class for inline uploads. The upload implementation is shared with
Tegra210. The fixture downloads pinned packages locally. The Tegra DRM driver creates a
render node using the Host1x/VIC discovery path, and the stock fuse driver
reports the Tegra234 A01 identity. The board reports NVIDIA's QT pre-silicon
platform ID with the GPU enabled. This selects the driver's simulation path
while production silicon PMU firmware remains unmodeled.

### JetPack EGL and OpenGL ES

`sh tests/gpu/run-orin-egl.sh` runs NVIDIA's stock EGL and OpenGL ES stack
(`libEGL_nvidia`, `libnvidia-eglcore`) through glvnd from the same R36.4.4
packages. The probe in `tests/gpu/orin/egl.c` initializes EGL 1.5 on the
device platform, creates an OpenGL ES 2 context and a 32 × 32 pbuffer, checks
that `GL_RENDERER` reports `NVIDIA Tegra Orin (nvgpu)/integrated`, clears to
red, and reads one pixel back. Set `SBC_ORIN_WASM=1` to run it in the
WebAssembly emulator.

The driver's first submission initializes the 3D class from a long state
stream. The C797 model keeps every state method in a register file and fails
only on methods that act but are not implemented, so unsupported draws stay
visible in `SBC_TRACE_GPU=1` traces. The driver also uploads and calls MME
macros. `tegra234/src/gpu/mme.rs` executes the Turing-and-later macro
instruction set, following Mesa's `mme_tu104` simulator, including the macro
DMA methods. The instruction pointer and start addresses count three-word
instructions. Other paths the stock stack needs:

- the 2D class (902D) and inline-to-memory (A140), both reusing the Tegra210 engines;
- the graphics copy engine, which answers on subchannel 4 without `SET_OBJECT`, with
  pitch and block-linear surfaces, component remapping and constant fills;
- method packets that continue in the next GPFIFO entry;
- host `SET_REFERENCE`, legacy `SEMAPHORE_A`–`D`, and `SEM_EXECUTE` with 64-bit
  payloads, timestamps, acquire modes and reductions (an entry blocked on an acquire
  resumes at that word on the channel's next doorbell);
- the USERD push-buffer `GET` pointers, `NOTIFY`, `REPORT_SEMAPHORE_EXECUTE`,
  and the legacy 3D `SET_REPORT_SEMAPHORE_A`–`D`. GL fences poll the last of these,
  so without it the process spins at exit.

### Jetson GPU parity

Use the [Tegra210 GPU test suite](jetson-nano-gpu.md#tests) as the coverage
baseline. The current mainline [nouveau platform driver](https://github.com/torvalds/linux/blob/master/drivers/gpu/drm/nouveau/nouveau_platform.c)
does not match GA10B, so the Orin fixture uses [NVIDIA's Jetson Linux R36.4.4 driver](https://developer.nvidia.com/embedded/jetson-linux-r3644).

| Milestone | Acceptance check |
|---|---|
| Power-on | NVIDIA R36.4.4 powers on GA10B and opens its control device. The stock Host1x driver handles syncpoint threshold interrupts. |
| Memory and submission | A Linux guest submits a copy across GPU page boundaries and verifies the result, completion semaphore and Host1x-backed Linux sync-file fence. GPU fault interrupts and recovery remain pending. |
| Compute | The Linux fixture checks a known-answer Ampere shader. JetPack CUDA creates a context, transfers data and launches a JIT-compiled SM87 kernel. TensorRT 10.3 builds YOLOv8n (FP32) and its detections match ONNX Runtime's (`sh tests/gpu/run-orin-trt.sh`, `TRT_YOLO MATCH`). FP16 and tensor-core kernels are untested. |
| Graphics | The stock driver submits clipped, masked and layered C797 color clears and masked block-linear depth/stencil clears. NVIDIA's stock EGL and OpenGL ES stack creates a context, clears a pbuffer and reads it back, with its MME macros executed. Draw calls run Ampere vertex and fragment shaders: the GLES 2 suite passes 23 of 25 scenes and the GLES 3 suite 31 of 49 (`tests/gpu/run-orin-gl.sh gles2test gltest`); the GLES 3.2 suite hangs after `compute_ssbo`. |
| Browser | The site worker runs the standalone rover copy demo. The stock-driver copy, compute, clear and Host1x fence tests and the full CUDA fixture also pass in the WebAssembly build. |

### Known not to work

- A channel blocked on a semaphore acquire is retried only when that channel is rung
  again, so waits on another channel, the CPU or Host1x can hang
  (`cuStreamWaitEvent`, GL waiting on the copy engine).
- GPU page faults raise no fault interrupt: the GPFIFO entry stops at the faulting method.
- The SM70 decoder models convergence barriers (BSSY/BSYNC) with the Maxwell SSY/SYNC
  stack: BREAK out of nested regions, divergent `BRA.DIV`/`BRA.CONV`, and warp-level
  reconvergence before SHFL or VOTE are approximations.
- Linux runtime-suspends the GPU after about 5 s idle, and its resume is not modeled: the
  next submission hangs. Keep it powered with
  `echo on > /sys/bus/platform/devices/*gpu*/power/control`.
- A kernel whose uniform registers do not fit below RZ keeps them past R255 and runs
  interpreted (no JIT).
- The GLES 3.2 suite hangs after `compute_ssbo`; GLES 2 and 3 have known differing scenes.
- These SM70 decoder gaps give wrong numbers without a fault (to be fixed on `claude/orin-cuda`):
  - uniform-register writes reach only the active lanes;
  - F2FP.PACK_AB ignores its rounding bits (`.RZ` decodes as round-up, `.MERGE_C` as round-down);
  - F2F `.H1` and I2F's byte select are ignored (the low half or byte is read);
  - a bound texture's index is cut to 16 bits, so a handle in cbuf 5 reads cbuf 1;
  - `highest_reg` counts uniform registers, so a kernel's register memset is too large.
- Engine state (3D and compute) is kept per channel and rebuilt from each runlist submit.
  A channel is reused only when its id, instance block and USERD all match. So two CUDA
  streams of one context each have their own engine state, and a new process that gets the
  same id and addresses before a runlist update inherits the old state. Ampere's QMDs carry
  most compute state, so neither has mattered so far.

Keep GA10B register, firmware, runlist and shader decoding in the corresponding
Orin or Ampere modules. Reuse the Maxwell renderer and texture code where the
engine semantics agree. The [NVIDIA GA10B resource list](https://developer.nvidia.com/docs/drive/drive-os/6.0.8/public/drive-os-linux-sdk/common/topics/util_setup/nvgpu_device_tree.html)
and [GA10B HAL setup](https://github.com/OE4T/linux-nvgpu/blob/patches-rel-36/drivers/gpu/nvgpu/hal/init/hal_ga10b.c)
describe the hardware interfaces the driver uses.

The CPU affinities follow an
[Orin Nano Linux boot log](https://forums.developer.nvidia.com/t/orin-nano/385080).
The other hardware addresses and interrupt bindings follow the
[Linux Tegra234 device tree](https://github.com/torvalds/linux/blob/master/arch/arm64/boot/dts/nvidia/tegra234.dtsi).
