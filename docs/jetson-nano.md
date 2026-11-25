# NVIDIA Jetson Nano (Tegra X1)

`--board jetson-nano`, experimental.

The Jetson Nano Developer Kit (P3450): NVIDIA's Tegra X1 (T210) with four Cortex-A57 cores on
the `arm-a64` core, 4 GiB of RAM at 0x8000_0000, a microSD card, Gigabit Ethernet, USB, HDMI
and the Maxwell GPU (GM20B), in the `tegra210` SoC model. Debian 13 boots from the card with
Debian's own kernel and Tegra drivers, and Mesa renders OpenGL ES 3.2 on the GPU through
nouveau. Nothing of NVIDIA's is needed: no bootloader, firmware or GPU userspace.

## Quick start

```sh
scripts/quickstart.sh jetson-nano          # Alpine's kernel and a BusyBox initramfs, to a shell
```

This fetches Alpine's `linux-lts` kernel for aarch64 with its unmodified
`tegra210-p3450-0000.dtb` and a BusyBox initramfs (`scripts/jetson-nano-fixture.sh`, pinned and
checksummed, into `cache/jetson-nano/`) and boots them on all four cores. The emulator loads the
kernel and fills in `/memory` and `/chosen` as a bootloader would; PSCI calls are answered by
the host, in place of the board's secure monitor.

## Debian 13

Debian's arm64 kernel has the drivers Alpine's lacks (the MAX77620 PMIC, whose regulators the
SD card needs, PCIe and the SMMU). Its cloud image, unmodified, is the microSD card:

```sh
scripts/jetson-nano-debian-fixture.sh      # Debian's kernel and device tree, and an initramfs
scripts/jetson-nano-card-fixture.sh        # Debian's arm64 image, 283 MB download, to disk.raw
D=cache/jetson-nano/debian
sbc-emu run --board jetson-nano --kernel $D/vmlinuz --dtb $D/tegra210-p3450-0000.dtb \
    --initrd $D/initramfs.cpio.gz --sd $D/disk.raw --append "console=ttyS0,115200 root=card rw quiet audit=0 \
    systemd.set_credential=passwd.plaintext-password.root:nano systemd.set_credential=firstboot.locale:C.UTF-8 \
    systemd.set_credential=firstboot.timezone:UTC systemd.set_credential=firstboot.keymap:us"
```

Log in as `root` / `nano` (the password is set on the command line). The kernel and initramfs
come from the command line; the initramfs loads the board's drivers and switches to the card.
Without `--sd-persist` the card is copy-on-write and the image file is not changed.

Options: `--net nat|local|none` (default `nat`: DHCP, DNS and NAT to the host's network),
`--usb-drive IMAGE` (a USB stick), `--hdmi-mode WxH` or `off` (default 1280x720), `--vnc :N`,
`--screenshot FILE`, `--smp N`, `--ram-mb N` (default 4096), `--gdb PORT`.

## Webcam

`--camera` plugs a USB webcam (UVC, 640×480 YUYV at 30 frames a second of guest time) into a
USB 2.0 port; Debian's `uvcvideo` makes it `/dev/video0`. On its own it shows a test pattern;
`--camera-source SPEC` gives it pictures from the host through `ffmpeg`, scaled to fit:

| SPEC | Pictures |
|---|---|
| `avfoundation:0` | the Mac's camera 0 (`ffmpeg -f avfoundation -list_devices true -i ""` lists them; macOS asks once whether the terminal may use the camera) |
| `v4l2:/dev/video0` | a camera on a Linux host |
| a file | a video or a picture, played in real time and looped |

The camera sends the newest picture whenever the guest asks for one, so a slow guest skips
pictures instead of falling behind. With both a USB stick and a camera, the tablet has no port.

### YOLOv8n on the webcam

`demos/jetson-yolo-cam/` finds objects in what the camera sees and draws them on the HDMI screen:
YOLOv8n through ncnn on Vulkan (Mesa's NVK on the emulated GM20B), ncnn on the CPU, or OpenCV on
Mesa's OpenCL. Set up once on a Debian card (`--sd-persist`):

```sh
demos/jetson-yolo-cam/host-setup.sh                 # cache/yolo-cam: YOLOv8n, a test video, the demo
cp cache/jetson-nano/debian/disk.raw cache/yolo-cam/disk.raw && truncate -s 6G cache/yolo-cam/disk.raw
python3 -m http.server 8800 --bind 127.0.0.1 -d cache/yolo-cam &
# boot the card (the command line of "Debian 13" above, --sd cache/yolo-cam/disk.raw --sd-persist), then:
#   python3 -c "import urllib.request as u; open('/tmp/s.sh','wb').write(u.urlopen('http://10.0.2.2:8800/setup.sh').read())"; sh /tmp/s.sh
```

Then boot it with the camera and watch the screen over VNC:

```sh
sbc-emu run --board jetson-nano ... --sd cache/yolo-cam/disk.raw --hdmi-mode 1280x720 \
    --camera-source avfoundation:0 --vnc :1           # or --camera-source cache/yolo-cam/street.mp4
# in the guest:  sh /opt/yolo-cam/run.sh nvk         # or cpu, opencl
```

`scripts/jetson-nano-yolo-cam.txt` runs eight pictures of the test video and takes a screenshot.
On an Apple M-series Mac a picture takes about 1.9 s with NVK, 0.3 s on the CPU (ncnn) and about
7 s with OpenCL, of guest time (which here runs about as fast as the wall clock).

## In the browser

The demo site's [Jetson Nano page](https://joakimeriksson.github.io/sbc-emu/board.html?b=jetson-nano)
boots Debian's kernel and initramfs to shells on the serial console and on HDMI, with the A64
JIT compiling to WebAssembly. A [second page](https://joakimeriksson.github.io/sbc-emu/board.html?b=jetson-nano-gpu)
runs GPU demos with Mesa ([jetson-nano-gpu.md](jetson-nano-gpu.md#demos-in-the-browser)).

## What is emulated

| Block | Model |
|---|---|
| Cortex-A57 ×4 | `arm-a64` with the A57's MIDR; 32-bit programs under a 64-bit kernel on `arm-a32` |
| Interrupts, timers | GIC-400; the legacy interrupt controller as read-back registers (interrupts go straight to the GIC); a Tegra timer per core and the microsecond counter |
| Clocks, power | the clock and reset controller, PMC power gates, fuses and chip ID (T210 A02); the MAX77620 PMIC as a register file with its real-time clock |
| Console | UART A (16550) |
| Storage | the four SDMMC controllers (SDHCI 3.0, ADMA2), the microSD card (`--sd`) |
| I²C, GPIO, DMA | six I²C controllers, eight GPIO banks with interrupts and the card-detect switch, APB DMA |
| Memory controller | the SMMU (per-group address spaces, two-level page tables) |
| PCIe, Ethernet | the PCIe controller with two root ports, and the RTL8111H on it (`r8169`: 1 Gbit/s, MSI, checksum and segmentation offload) |
| USB | XUSB (xHCI) with a keyboard, a tablet, an optional USB stick and an optional webcam (`--camera`, `--camera-source`); the falcon's firmware is accepted, not run |
| Display | host1x, both display controllers (three windows, scaling, blending, cursor), SOR/HDMI with a monitor whose EDID `tegra-drm` reads |
| GPU | the GM20B: memory management, the PMU's and FECS's messages, channels and runlists, the copy engine, and the 3D, 2D, compute and inline-to-memory classes ([jetson-nano-gpu.md](jetson-nano-gpu.md)) |

Everything else in the device window reads back what is written; the pages first touched are
listed at exit (`SBC_STUB_LOG=1` prints them as they happen).

## What works

| | Status | Check |
|---|---|---|
| Boot | Alpine's kernel to the BusyBox shell on four cores, each on its own Tegra timer | `scripts/jetson-nano-shell.txt` |
| Debian 13 | from the microSD card to a login with Debian's kernel, device tree and Tegra drivers | `scripts/jetson-nano-debian.txt` |
| Ethernet | the link at 1 Gbit/s with MSI, DHCP, ping; in Debian, `apt install` through the NAT | `scripts/jetson-nano-net.txt` |
| USB | keyboard, tablet and stick enumerate; the stick's data reads back, also after the controller suspends and resumes | `scripts/jetson-nano-usb.txt` |
| Webcam | `uvcvideo` streams the host's camera or a video; YOLOv8n finds what is in it, on the emulated GPU or the CPU | `scripts/jetson-nano-yolo-cam.txt` ([below](#webcam)) |
| HDMI | `tegra-drm` reads the EDID and shows the console | `scripts/jetson-nano-display.txt` |
| GPU | nouveau and Debian's Mesa 25.0.7: kmscube and every glmark2 scene on HDMI; the GLES 2, 3 and 3.2 test scenes against Mesa's software renderers; Mesa's OpenCL (rusticl) and Vulkan (NVK) compute | `scripts/jetson-nano-gl.txt` ([jetson-nano-gpu.md](jetson-nano-gpu.md#tests)) |
| CUDA, TensorRT | NVIDIA's L4T kernel, `nvgpu`, CUDA 10.2 and TensorRT 8.2: a CUDA kernel, and YOLOv8n with the host's detections | `scripts/jetson-nano-cuda.txt`, `scripts/jetson-nano-tensorrt.txt` ([jetson-nano-cuda.md](jetson-nano-cuda.md)) |
| Debugging | `--gdb PORT` ([debugging.md](debugging.md)) | `tools/gdb-check.py` |

## Speed

The A64 JIT ([jit.md](jit.md)) is on by default on AArch64 hosts (macOS and Linux), on x86-64
Linux and in the browser; `--no-jit` or `SBC_JIT=0` runs the interpreter alone. The quick start
reaches its shell in about 6 s on an Apple M-series Mac. The cores take turns on one host
thread. GPU shaders are interpreted natively, on several host threads; in the browser they have
a JIT to WebAssembly.

## Not there yet

- [Snapshots](snapshots.md).
- PSCI `CPU_SUSPEND` (CPU idle states), the PMU, interrupts from the PMIC.
- UHS modes on the SD card; booting through U-Boot or `extlinux.conf` (the kernel and initramfs
  come from the command line).
- USB 3.0 devices, the on-board hub, the device-mode port; DisplayPort, HDMI audio, YUV
  overlays, VIC.
- GPU: see [jetson-nano-gpu.md](jetson-nano-gpu.md#not-done).
- NVIDIA's JetPack image as a whole (its bootloaders and Ubuntu): its kernel, GPU driver, CUDA and
  TensorRT run with Debian's root ([jetson-nano-cuda.md](jetson-nano-cuda.md)).

The milestones and notes from the bring-up are in the development record,
[history/jetson-nano-plan.md](history/jetson-nano-plan.md).
