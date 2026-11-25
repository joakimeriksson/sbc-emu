# Changelog

All notable changes to sbc-emu are listed here. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and versions follow
[Semantic Versioning](https://semver.org/). Before 1.0, a minor version may change command-line
options, snapshot formats and crate interfaces.

Numbers such as (#21) are pull requests in this repository. Work from before 2026-11-20 was merged
in an earlier repository whose history was rewritten; it is summarised here without numbers.

## [Unreleased]

### Added

- Jetson Nano: NVIDIA's own GPU stack. L4T R32.7.3's kernel with `nvgpu`, CUDA 10.2 and
  TensorRT 8.2 run with Debian's root; YOLOv8n through TensorRT finds what ONNX Runtime finds
  (docs/jetson-nano-cuda.md, `scripts/jetson-nano-l4t-fixture.sh`). Mesa's OpenCL and NVK run it
  too.
- Maxwell GPU: compute workgroups in 32-lane warps on several host threads (Mesa's OpenCL YOLOv8n
  54 -> 6.5 s per image); LEA, VADD, P2R, R2P and FCHK in the shader core.

### Fixed

- arm-a64: SEV/SEVL set an event register that the next WFE consumes (a `SEVL; WFE` lock with one
  core running slept forever).
- Jetson Nano GPU: copies on an unbound subchannel 4 (NVK's) reach the copy engine.

## [0.1.0] - 2026-11-24

The first release: seven boards, four CPU cores with JITs, and the same machines in the browser.

### Host platforms

- macOS on Apple silicon and Linux on x86-64: built and tested in CI on every pull request.
  Release binaries are built for these two.
- Linux on AArch64: the AArch64 JITs run there; not tested in CI.
- Other hosts (macOS on Intel, Windows) are not tested; where no JIT backend exists, the cores
  run in their interpreters.
- Browsers: the WebAssembly build runs every board in a Web Worker
  ([joakimeriksson.github.io/sbc-emu](https://joakimeriksson.github.io/sbc-emu/)).

### Boards

- **Raspberry Pi 4 Model B** (BCM2711, four Cortex-A72): boots Raspberry Pi OS and Ubuntu 26.04
  Desktop from their unmodified SD card images (config.txt, cmdline.txt, os_prefix), or a kernel
  and initramfs. HDMI through the stock vc4 KMS driver, with HDMI audio; the V3D 4.2 GPU with
  Mesa's OpenGL ES 3.1 and Vulkan 1.3 drivers; USB through PCIe and the VL805 xHCI (keyboard,
  tablet, USB stick, UVC webcam) and the DWC2 OTG port; Gigabit Ethernet (GENET); Wi-Fi
  (CYW43455 on SDIO, open and WPA2-PSK networks) and Bluetooth LE; I2S audio; GPIO, I2C with a
  BME280, SPI with an MCP3008; RNG, thermal sensor, watchdog, DMA. 32-bit programs under the
  64-bit kernel.
- **Orange Pi 5** (RK3588S, four Cortex-A76 and four Cortex-A55), experimental: Armbian to its
  login prompt on eight cores; SD card, NVMe, USB 3.0, HDMI (VOP2), audio, Ethernet; the
  Mali-G610 GPU running Mesa's Panfrost driver (kmscube, glmark2 scenes) on HDMI; snapshots (#29).
- **Orange Pi One** (Allwinner H3, four Cortex-A7), experimental: Armbian to its login prompt;
  SD card, HDMI (DE2, TCON, DW-HDMI), USB, Ethernet; the Mali-400 GPU with Lima; snapshots (#32).
- **ASUS Tinker Board S** (RK3288, four Cortex-A12), experimental: Armbian from SD card or eMMC
  to a login on HDMI; the Mali-T760 GPU with Panfrost; RGA, I2S audio, GPIO; snapshots (#35).
- **Orange Pi RV** (StarFive JH7110, four SiFive U74, RISC-V), experimental: the board's own
  boot chain (boot ROM, U-Boot SPL, U-Boot) from SPI flash or SD card; Alpine, Xunlong's kernel
  and Canonical's Ubuntu 24.04.5 image unmodified (#10); HDMI, Gigabit Ethernet, USB 3.0, NVMe,
  Wi-Fi and Bluetooth (AP6256), audio; the PowerVR GPU with Mesa's Vulkan driver; snapshots
  (#45, #64).
- **NVIDIA Jetson Nano** (Tegra X1, four Cortex-A57), experimental: mainline Linux to a shell
  (#21); Debian 13 from the microSD card (#24, #28); PCIe and Gigabit Ethernet (RTL8111H, #30);
  USB (XUSB, #31); HDMI (tegra-drm, #34); the GM20B (Maxwell) GPU through nouveau and Mesa,
  OpenGL ES 3.2 (#36, #46, #50).
- **ESP32-S31 Function CoreBoard** (two RV32IMAC cores), experimental: Espressif's Linux flash
  image from the mask ROM to a shell; run with `sbc-emu run --board esp32s31` (#22).

### CPU cores and JITs

- Four CPU cores written for the project: 64-bit Arm (`arm-a64`, with exact floating point and
  optional Cryptographic Extension), 32-bit Arm with Thumb-2, VFPv4 and NEON (`arm-a32`), 64-bit
  RISC-V RV64GC with Sv39 (`riscv64`) and 32-bit RISC-V (`riscv-rv32`, from esp32sim).
- Block JITs to AArch64 for all four cores (RV64: #12, #18; RV32: #17).
- Block JITs to x86-64 on Linux hosts for all four cores (#59, #60, #61, #62).
- Block JITs to WebAssembly for the browser build: A64, A32/T32 (#5), RV64 (#7), RV32 (#17).
- `--threads multi` runs a host thread per guest core on the Raspberry Pi 4 and the Orange Pi 5;
  the default, one thread, is repeatable run to run.
- arm-a64: reserved FP/SIMD encodings are UNDEFINED and misaligned exclusives fault, fixing two
  JIT host crashes (#66).

### GPUs

- V3D 4.2 (Raspberry Pi 4): QPU shader cores, binner, rasterizer, TMU, TFU, compute; TFU jobs on
  their own GPU thread (#13).
- Mali-G610 (Orange Pi 5): command stream frontend, Mesa's GLES 3D pipeline (#55, #57), AFBC,
  shader and texture fidelity fixes (#68), bounds on the host memory and time a guest can make
  the model spend (#67); VOP2 plane mixing and HDR0 (#48, #65).
- Mali-T760 (Tinker Board) and Mali-400 (Orange Pi One).
- PowerVR (Orange Pi RV): Mesa's Vulkan driver; the firmware file is a stand-in the emulator
  generates.
- Maxwell GM20B (Jetson Nano): shader interpreter and a shader JIT to WebAssembly SIMD for the
  browser (#49).

### Browser

- Every board runs in the browser: Raspberry Pi 4 (with its 3D demos, #15, and your own SD card
  image, #20), Tinker Board (#5, #8), Orange Pi One (#6), Orange Pi RV (#7), Orange Pi 5 (#16),
  ESP32-S31 (#17), Jetson Nano (#38) with GPU demos (#40).
- The demo site on GitHub Pages, built from pinned downloads (#3, #4, #9).

### Debugging and snapshots

- gdb remote stub on every board: `--gdb PORT [--gdb-wait]` (#22); breakpoints and steps in
  32-bit programs under a 64-bit kernel (#26); per-core stepping and breakpoints (#33, #39, #44).
- Monitor (Ctrl-A c, after QEMU's) and `--script` files for automated runs.
- Snapshots (`savevm`/`loadvm` in the monitor, `--restore`) on the Raspberry Pi 4, Orange Pi 5,
  Orange Pi One, Tinker Board and Orange Pi RV; nothing is restored onto `--sd-persist`, and
  damaged snapshot files are refused (#37, #41, #43).

### Tooling

- `scripts/quickstart.sh BOARD` boots any board in one command from pinned, checksummed guest
  downloads (#63).
- `sbc-emu --version` (also `-V`, `version`) prints the version and the git commit it was built
  from.
- Exit status of `sbc-emu run` is the same on every board: 0 when the run ends by a quit, or when
  the guest powers off or `--timeout` / `--max-insns` runs out with no `--script` still running;
  1 on an error (a bad option for the board included) or an unfinished `--script`; 2 on an
  unknown command or board. Before, when `--timeout` ran out, the Raspberry Pi 4 exited 0 (even
  with an unfinished `--script`) and the other boards 1 (even without one); and the Raspberry
  Pi 4 exited 1 when the guest powered off, where it now exits 0 as the other boards do.
- VNC server (`--vnc :N`), port forwarding (`--hostfwd`), virtual network with NAT, WAV audio
  capture (`--audio-out`), screenshots (`--screenshot`).
- Shared SD card model keeps the last partition and the guest's writes on odd-sized images
  (#25, #27).
- Serial-console boards no longer use a whole host core while the guest is idle (#23).
- Licensed BSD 3-Clause; the crates from esp32sim keep their MIT license (#2).

### Known limitations

- Only the Raspberry Pi 4 is complete enough for regular use; the other six boards are
  experimental, and each board's page in `docs/` lists what is not modelled.
- Snapshots are not available on the Jetson Nano and the ESP32-S31.
- `--threads multi` is not repeatable run to run.
- Orange Pi 5: an XRGB buffer on a Cluster overlay plane is blended by its X byte, as the
  registers mainline Linux writes say; not checked against a real board (documented and pinned
  by a test in #65). The Mali-G610 model ignores the Attribute offset-enable bit, does not filter
  cube faces seamlessly, and CLPER does not cover lanes 4-15 of a 16-lane subgroup (listed in
  #68).
- The browser build is single-threaded and slower than the native one.
- Raspberry Pi 4: Firefox (Ubuntu snap) drops some glyphs on text-heavy pages.

[0.1.0]: https://github.com/joakimeriksson/sbc-emu/releases/tag/v0.1.0
