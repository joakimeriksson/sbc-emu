# ASUS Tinker Board S (Rockchip RK3288)

`--board tinkerboard`, experimental.

A 32-bit machine on the `arm-a32` core: four Cortex-A12 cores (MIDR 0x410fc0d1, with Thumb-2,
VFPv4 and NEON) in the `rockchip-rk3288` SoC model. It boots Debian's unmodified `armmp` kernel
with the mainline `rk3288-tinker-s.dtb`, and Armbian's unmodified SD image to a login on the
serial console and on HDMI. The Mali-T760 GPU runs Mesa's Panfrost driver.

## Quick start

```sh
scripts/quickstart.sh tinkerboard          # Debian's kernel and a BusyBox initramfs, to a shell
```

This fetches Debian 13's `linux-image-armmp` kernel, its device tree and Alpine's armhf
minirootfs (`scripts/tinker-board-fixture.sh`, pinned and checksummed, into
`cache/tinker-board/`) and boots them directly, as U-Boot would load them. The same by hand:

```sh
scripts/tinker-board-fixture.sh
D=cache/tinker-board
sbc-emu run --board tinkerboard --kernel $D/zImage --dtb $D/rk3288-tinker-s.dtb \
    --initrd $D/initramfs.cpio.gz                     # Ctrl-A x quits
```

## Armbian

```sh
scripts/armbian-tinkerboard-fixture.sh     # Armbian 26.8.1 trixie, minimal, checksummed
sbc-emu run --board tinkerboard \
    --sd cache/tinker-board-armbian/Armbian_26.8.1_Tinkerboard_trixie_current_6.18.43_minimal.img
```

Without `--kernel`, the emulator does what Armbian's boot script does on the board: it reads
`armbianEnv.txt`, the zImage, `rk3288-tinker-s.dtb` and `uInitrd` from the card's `/boot`
(or, with no SD card, from `--emmc IMAGE`) and starts the kernel. U-Boot itself does not run,
and device tree overlays from `armbianEnv.txt` are not applied. The login is on the serial
console; the first login is Armbian's first-run wizard (Ctrl-C skips creating a user). Add
`--vnc :1` to see HDMI in a VNC viewer on `127.0.0.1:5901`, or `--screenshot hdmi.png`.

Without `--sd-persist` the card is copy-on-write: what the guest writes stays in memory and the
image is not changed. `--ram-mb` sets the RAM (default 1024, at most 4064).

## In the browser

The demo site's [Tinker Board page](https://joakimeriksson.github.io/sbc-emu/board.html?b=tinker-board)
boots Debian's kernel with a BusyBox initramfs to a shell. Locally, `web/tinker.html` also boots
the Armbian image (`tools/wasm-build.sh` links it from the fixture), with HDMI on a canvas.
The 32-bit core's JIT compiles to WebAssembly there.

## What is emulated

| Block | Model |
|---|---|
| Cores | four Cortex-A12 (`arm-a32`), started through the boot ROM's SMP mailbox in SRAM as mainline Linux expects; they take turns on one host thread |
| Interrupts, timers | GIC-400 and the architectural timer |
| Clocks, power | CRU (PLLs with lock, dividers, gates), GRF/SGRF, PMU power domains; the RK808 PMIC and RTC on I²C0 (regulators, the RTC from the host clock) |
| Console | UART2 (DesignWare APB UART) |
| Storage | DesignWare MMC (dw_mshc) with IDMAC for the SD card (`--sd`) and the eMMC (`--emmc`, an eMMC 5.1 device) |
| I²C, GPIO | the six rk3x I²C controllers; GPIO input levels and interrupts |
| Display | VOP (both), its IOMMU and the DesignWare HDMI TX with a monitor whose EDID is read over I²C5 (`--hdmi-mode 1024x768|1280x720|1920x1080`, default 1280x720) |
| 2D, GPU | RGA; the Mali-T760 MP4 ([mali-t760.md](mali-t760.md)) |
| Audio | I²S fed by the PL330 DMA controller; `--audio-out FILE.wav` records it. On this board the I²S goes to the HDMI transmitter, whose audio side only stores its registers |

Other device pages in the SoC's register window read back what is written, so their drivers
probe without effect; the run summary lists the pages that were touched.

## What works

| | Status | Check |
|---|---|---|
| Boot | Debian's kernel to the BusyBox shell on four cores; `reboot -f` boots again through the CRU's global soft reset | `scripts/tinkerboard-shell.txt` |
| Armbian | the unmodified Armbian 26.8.1 image from SD: initramfs, root by UUID, systemd to `multi-user.target`, the console login, the first-run wizard; from the root shell the board model, the card as `/`, the RK808 RTC, four CPUs and both SD regulators | `scripts/tinkerboard-armbian.txt` |
| HDMI | `HDMI-A-1` connected at 1280x720; Armbian's kernel log, console and login on the screen | same script |
| GPU | Panfrost and Mesa 25.0.7: kmscube on HDMI, and 25 of 25 GLES 2 scenes equal to softpipe | `scripts/mali-t760-gpu.txt`, `scripts/mali-t760-gles2.txt` ([mali-t760.md](mali-t760.md)) |
| Snapshots | the cores, every device and the disks' written sectors ([snapshots.md](snapshots.md)) | `rockchip-rk3288/tests/snapshot.rs` |
| Debugging | `--gdb PORT` ([debugging.md](debugging.md)) | `tools/gdb-check.py` |

## Speed

The A32/T32 JIT ([jit.md](jit.md)) is on by default on AArch64 hosts (macOS and Linux), on
x86-64 Linux and in the browser; `--no-jit` or `SBC_JIT=0` runs the interpreter alone. The
quick start reaches its shell in about 7 s on an Apple M-series Mac, and Armbian reaches
`login:` in about 6 minutes (about two of them are systemd waiting for its random seed).

## Not there yet

- USB (the DWC2 controllers and the on-board hub), so there is no keyboard: input is the serial
  console.
- Ethernet (the GMAC): Armbian's first-run wizard waits about a minute for an address.
- Wi-Fi and Bluetooth, HDMI audio, the SAR ADC, video codecs.
- A host thread per core (`--threads multi`), LPAE kernels (`armmp-lpae`).
- `poweroff` ends in "System halted": Linux 6.18 does not use the RK808 to switch the board off.
- The boot ROM and U-Boot are not run; the emulator loads the kernel from the card itself.

The milestones and the notes on how each part was built are in the development record,
[history/rk3288-tinker-board-plan.md](history/rk3288-tinker-board-plan.md).
