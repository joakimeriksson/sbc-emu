# sbc-emu: single-board computers, emulated in Rust

[![CI](https://github.com/joakimeriksson/sbc-emu/actions/workflows/ci.yml/badge.svg)](https://github.com/joakimeriksson/sbc-emu/actions/workflows/ci.yml)

**Real boards. Virtual hardware.** sbc-emu emulates single-board computers instruction by
instruction, in the style of [esp32sim](https://github.com/mikroverk/esp32sim):
- its own CPU cores and register-level models of each board's SoC peripherals;
- one deterministic virtual clock, and no QEMU;
- the goal on every board is the stock operating-system image, booted unmodified, natively and
  (where it is far enough) in the browser.

## Boards

| Board | SoC | Status | Details |
|---|---|---|---|
| Raspberry Pi 4 Model B | Broadcom BCM2711, 4× Cortex-A72 | **complete enough for real use**: unmodified Raspberry Pi OS and Ubuntu 26.04 Desktop, V3D GPU (OpenGL ES 3.1, Vulkan 1.3), HDMI, USB, Ethernet, Wi-Fi, Bluetooth, audio, a JIT, and a WebAssembly build | [below](#raspberry-pi-4-model-b) |
| Orange Pi 5 | Rockchip RK3588S | **experimental**: one Cortex-A55, GICv3, UART, Ethernet; a Debian kernel to a BusyBox shell by direct boot | [docs/orangepi5.md](docs/orangepi5.md) |
| Orange Pi One | Allwinner H3, 4× Cortex-A7 | **experimental**: four Cortex-A7s on the 32-bit core (`arm-a32`, with a JIT to AArch64), Thumb-2, VFPv4, NEON; the unmodified Armbian 26.8.1 SD image to its login prompt (Thumb-2 kernel, systemd), HDMI console over VNC with a USB keyboard and tablet, Ethernet with NAT, the Mali-400 GPU (Mesa's lima driver renders kmscube), or Alpine by direct boot | [docs/orangepi-one.md](docs/orangepi-one.md) |
| Orange Pi RV | StarFive JH7110, 4× SiFive U74 (RV64GC) | **experimental**: four U74 harts on sbc-emu's own RV64 interpreter (`riscv64`, Sv39); real OpenSBI 1.8 with Alpine's mainline 7.1 kernel or Xunlong's own 5.15 kernel, directly or from the SD card through the board's boot chain (boot ROM → U-Boot SPL with DDR init → OpenSBI + U-Boot 2026.10 → extlinux): shell, SMP, microSD card, Gigabit Ethernet with NAT, USB 3.0 (PCIe → VL805: keyboard, tablet, USB stick), an NVMe SSD in the M.2 slot, Wi-Fi (AP6256: brcmfmac on mainline, bcmdhd on Xunlong's kernel), Bluetooth (HCI on UART1), audio (PWM DAC), HDMI console over VNC with the USB keyboard (DC8200 + Inno HDMI with HDMI audio on Xunlong's kernel, the firmware's framebuffer under simpledrm with mainline), reboot and power-off; the PowerVR BXE-4-32 GPU with mainline's powervr driver and Mesa's PowerVR Vulkan driver: compute, offscreen rendering and vkcube on HDMI | [docs/orangepi-rv.md](docs/orangepi-rv.md) |
| ASUS Tinker Board S | Rockchip RK3288, 4× Cortex-A12/A17 | **experimental**: four Cortex-A12s on `arm-a32` started through the boot ROM mailbox (`rockchip,rk3066-smp`); a Debian armmp kernel and the stock DTB to a BusyBox shell by direct boot (`scripts/tinkerboard-shell.txt`); an unmodified Armbian 26.8.1 SD image (mainline 6.18) to the console login and a root shell by emulated firmware boot, with dw_mmc SD/eMMC, rk3x I²C, the RK808 PMIC/RTC and HDMI (VOP + DesignWare HDMI, an emulated monitor at 1280x720 or `--hdmi-mode`, console and login on screen, VNC and `--screenshot`) (`scripts/tinkerboard-armbian.txt`, 5 min 52 s on the A32/T32 JIT, 23 min on the interpreter); the Mali-T760 GPU (Midgard) for the unmodified Panfrost kernel and Mesa drivers: kmscube on HDMI and all 25 GLES 2 test scenes matching softpipe (`scripts/mali-t760-gles2.txt`, [docs/mali-t760.md](docs/mali-t760.md)) | [docs/rk3288-tinker-board-plan.md](docs/rk3288-tinker-board-plan.md) |
| ESP32-S31-Function-CoreBoard-1 | Espressif ESP32-S31, 2× RV32IMAC (M/S/U, Sv32) | **experimental**: Espressif's official Linux flash image, unmodified, through the real mask ROM, U-Boot SPL, OpenSBI and U-Boot to XIP Linux 6.18 on both harts and a root shell over UART0 (`scripts/esp32s31-shell.txt`), and `reboot` back through the ROM; no Ethernet PHY or SD yet | [docs/esp32s31.md](docs/esp32s31.md) |

`sbc-emu boards list` shows what the build knows; `--board NAME` picks one (the Pi 4 is the
default). See [docs/decisions.md](docs/decisions.md) for why this is not QEMU-based,
[SBC-EMUPLAN.md](SBC-EMUPLAN.md) for the overall plan and [docs/multi-board-foundation.md](docs/multi-board-foundation.md)
for how boards share the machine layer. Contributions, human or agent, go through pull requests
([AGENTS.md](AGENTS.md)).

## Raspberry Pi 4 Model B

The first board, and the furthest along: an instruction-level BCM2711 that boots the unmodified
**Raspberry Pi OS Lite (trixie, arm64) SD-card image** and Ubuntu 26.04 Desktop, natively and in
the browser (WebAssembly). The [sbc-emu page](web/index.html) runs its 3D demos in a browser tab.

### What runs today

| | Status | Evidence |
|---|---|---|
| Raspberry Pi kernel 6.18 (firmware 1.20260915 `kernel8.img`) | boots to userspace | busybox initramfs shell on serial and HDMI |
| Raspberry Pi OS Lite 2026-10-06 (unmodified `.img`) | boots on all four cores: firmware-style SD boot → initramfs → ext4 root → systemd 257 → `raspberrypi login:` and the first-boot setup wizard on HDMI | `scripts/raspios-firstboot.txt`; ~27 G instructions to the login prompt |
| Ubuntu 26.04.1 LTS Desktop (unmodified `preinstalled-desktop-arm64+raspi` image, kernel 7.0) | GNOME on Wayland on the V3D GPU, 3 GiB RAM, four cores (`--threads multi`): boot to the GDM greeter, log in, the desktop and its Welcome app | `tools/ubuntu-image.sh` (cloud-init test account), `scripts/ubuntu-stress.txt` (40 identical digests from multi-threaded Python, no crash reports), `scripts/ubuntu-bench.txt`; see below |
| Four cores | four Cortex-A72s started through PSCI, GIC-400 with per-core interfaces, SGIs (IPIs) and SPI routing | `smp: Brought up 1 node, 4 CPUs`; four busy loops finish in 1.6 s of guest time instead of 6.5 s |
| USB | PCIe root complex + VL805 xHCI behind the four USB-A ports, as on a Pi 4: USB keyboard and tablet (absolute pointer) by default, a USB stick (`--usb-drive IMAGE`) and a UVC webcam (`--camera`) on request; the DWC2 OTG port too | `lsusb -t`: devices on the xHCI root hub; typing on tty1; `mkfs.vfat` + mount + read back on the stick; `v4l2-ctl` captures 30 frames of 640×480 YUYV |
| Audio | an I²S DAC HAT (PCM5102A on `simple-audio-card`) on the BCM2835 PCM/I²S block, fed by DREQ-paced DMA; `--audio-out FILE.wav` (native) or WebAudio (browser) | `speaker-test -t sine -f 440`: the WAV holds a 440.0 Hz tone, 3.000 s per channel at 48 kHz |
| Bluetooth | the CYW43455's HCI controller on the mini UART (as `dtoverlay=miniuart-bt`), a virtual BLE sensor nearby | `hci_bcm` downloads the real patch RAM, `bluetoothctl` powers on, a scan finds `sbc-emu sensor` |
| Ethernet | GENET v5 + UniMAC MDIO + BCM54213PE PHY on a virtual 10.0.2.0/24 network (DHCP, DNS, user-mode NAT to the host's network) | busybox: DHCP lease, ping, DNS, `wget` of a real web page |
| Wi-Fi | CYW43455 on SDIO for the stock `brcmfmac`: real firmware download, emulated FullMAC host interface | `wlan0`, NetworkManager scan lists `sbc-emu`, `nmcli dev wifi connect`, DHCP and ping over Wi-Fi |
| GPIO / I²C / SPI | BCM2711 GPIO (LED on GPIO17, button on GPIO27), BSC I²C1 + BME280, SPI0 + MCP3008 on CE0 | stock drivers: `gpioset`/`gpioget` (libgpiod), IIO sysfs reads the host-set temperature/pressure/humidity and ADC voltages exactly |
| Speed | A64→A64 block JIT on AArch64 hosts (native integer, branches, loads/stores, FP/NEON; interpreter fallback) | Raspberry Pi OS cold boot to `login:` 248 s (interpreter) → 19 s on four cores in turns, 13.5 s with a host thread per core (`--threads multi`); faster than QEMU's TCG on every workload [below](#compared-with-qemus-raspi4b); all CPU tests pass with and without it. Browser: WebAssembly region JIT (`?jit=0` turns it off), Pi OS `login:` 313 s → 160 s, verified against the interpreter |
| Floating point | exact Arm semantics: FPCR rounding modes, flush-to-zero, default NaN, all FPSR exception flags, Arm's estimate tables | `diff_fp.rs`: 18,800 random cases bit-exact against the host CPU (interpreter and JIT) |
| More peripherals | DMA controller, hardware RNG, thermal sensor, PM watchdog and power domains, system timer, mini UART | stock drivers bind: `/dev/hwrng`, `/sys/class/thermal`, `/dev/watchdog` (expiry reboots), `ttyS0`, `dma0chan*` |
| Debugging, display | GDB remote stub (`--gdb PORT`, one thread per core, breakpoints and watchpoints), monitor (Ctrl-A c: registers, memory, stop/cont, savevm/loadvm, screendump), VNC server (`--vnc :N`) | lldb: kernel breakpoint, step, memory read, detach; a VNC client sees the console and types into it |
| Port forwarding | `--hostfwd tcp::2222-:22` (QEMU's syntax) through the user-mode NAT | `ssh-keyscan -p 2222 127.0.0.1` reads the Pi OS host keys |
| HDMI | the vc4 KMS pipeline the image's `config.txt` asks for (`dtoverlay=vc4-kms-v3d`): HVS compositor, pixel valve, HDMI0 encoder + PHY, DDC with a 1080p monitor's EDID, HDMI audio; `--display fb` keeps the firmware's `simple-framebuffer` (1024×768) instead | stock `vc4`: `HDMI-A-1` connected, modes from the EDID, the console at 1920×1080; `kmstest` switches to 1024×768 and page-flips at 60.00 fps; `speaker-test -D hdmi:…`: a 440.0 Hz tone at 48 kHz on the host; screenshots via `--screenshot` / `shot` |
| 3D GPU | V3D 4.2 (VideoCore VI) for the stock `v3d` kernel driver and Mesa's `v3d` (OpenGL ES 3.1) and `v3dv` (Vulkan 1.3) drivers: QPU shader cores, binner, rasterizer, tile buffer, texture unit, TFU, compute dispatch, GPU MMU ([docs/gpu.md](docs/gpu.md)) | `kmscube` on HDMI; 33 GLES scenes (depth, stencil, blending, mipmapped/cube/float textures, points/lines, instancing, discard, derivatives, a lit cube, and the habits of Firefox's WebRender) match llvmpipe pixel for pixel in the guest, also under the Mesa that Ubuntu's snaps use, and a compute shader with shared memory and a barrier is exact; all 33 `glmark2-es2-drm` benchmark scenes run at 1920×1080 (score 117) |
| Browser | the same machine as WebAssembly; SD image read lazily over HTTP range requests | `tools/wasm-smoke-sd.mjs`: login prompt in ~2.7 min (JIT on), ~200 MiB of the image read |

Compatibility labels in the plan's terms (per-feature evidence above; nothing else is claimed):

- **boot_path**: emulated firmware (the emulator reads `config.txt`, `cmdline.txt`, `kernel8.img`,
  `initramfs8` from the FAT partition); the VideoCore boot chain itself is not executed.
- **device tree**: a *reduced* BCM2711 tree generated by the emulator (only modelled devices),
  not the vendor `bcm2711-rpi-4-b.dtb`; `dtoverlay`/`dtparam` lines are reported and ignored.
- **vendor_image_compatibility**: the stock Raspberry Pi OS Lite image boots unmodified (writes go
  to a copy-on-write overlay; the image file is never changed).
- **network**: W0 — the Pi 4's own Ethernet MAC (GENET) and PHY, behind a virtual network with
  user-mode NAT (native) or an isolated network (browser). **wifi_tier**: W2 — original driver
  (`brcmfmac`) against an emulated CYW43455 host interface (firmware download real, firmware
  behaviour emulated, open and WPA2-PSK networks, no radio model; see docs/decisions.md D5). **clock_mode**: T0 real-time pacing
  (virtual time is deterministic per run; wall-clock pacing only governs idle time).
- **tested_cpu_count**: 4, as on the Pi 4 (`--smp 1..4`).

### The emulated hardware

| Block | Model | Linux driver |
|---|---|---|
| CPU | 4× ARMv8.0-A, Cortex-A72 identity: A64 integer ISA with CRC32, FP (exact, all FPCR modes), Advanced SIMD, EL0–EL3, stage-1 MMU (4 KiB), generic timers, PSCI (CPU_ON/OFF, AFFINITY_INFO, reset, off) via the host; 32-bit programs at EL0 (AArch32: A32, Thumb-2, VFPv4, NEON on the `arm-a32` core, see [below](#32-bit-programs)) | — |
| GIC-400 | distributor + four CPU interfaces, banked SGIs/PPIs, SPI targets | `gic-400` |
| UART0 | PL011 | `amba-pl011` |
| Mailbox | VideoCore property channel (board, memory, clocks, power, framebuffer) | `bcm2835-mbox`, `raspberrypi-firmware` |
| EMMC2 | SDHCI v3 with ADMA2 + SDHC card on an image file | `sdhci-iproc` |
| PCIe | brcmstb root complex (root port, EXT_CFG window, x1 link up), VL805 on bus 1, INTA | `pcie-brcmstb` |
| USB 3 host | VL805 xHCI 1.0: 4 USB 2.0 + 4 USB 3.0 root ports, command/event/transfer rings, hot-plug | `xhci-pci` |
| USB devices | HID keyboard, HID tablet, bulk-only mass storage (SCSI), UVC 1.1 webcam (bulk YUYV) | `usbhid`, `usb-storage`, `uvcvideo` |
| USB OTG | DWC2 core, host mode (buffer DMA); takes the keyboard with `--otg-keyboard` | `dwc2` |
| PCM/I²S + DAC | BCM2835 PCM as bit clock/frame master, bit clock from a firmware clock; PCM5102A | `bcm2835-i2s`, `pcm5102a`, `simple-audio-card` |
| Bluetooth | CYW43455 HCI over H4 on the mini UART: Broadcom vendor commands, patch RAM download, LE scanning | `hci_uart` (`hci_bcm`, `btbcm`), BlueZ |
| HVS | display lists in its own memory: RGB planes (565, 888, 8888) composed with alpha when the host takes a picture, channel status and frame counters, the list latched at each frame start | `vc4` |
| Pixel valve 2 | timing registers, vblank (VFP start) at 60 Hz of guest time | `vc4` |
| HDMI0 | encoder, PHY, DVP clocks/resets, packet RAM, scheduler; MAI audio (IEC 958 subframes through DMA on DREQ 10); hot-plug from the always-on L2 interrupt controller; CEC registers | `vc4`, `clk-bcm2711-dvp`, `irq-brcmstb-l2` |
| DDC | brcmstb BSC controller (polled) with an EDID EEPROM: 1920×1080 preferred, 1280×720, 1024×768…, CEA block with HDMI and LPCM audio | `i2c-brcmstb` |
| TXP | the transposer's registers (writeback; never busy) | `vc4` |
| V3D | hub + core registers, GPU MMU, control-list executors (binning, rendering), QPUs (interpreted, 16 lanes; tiles and vertices shaded on up to 16 host threads), TMU, TLB, TFU, CSD; jobs finish when started, their interrupt follows after the host time they took (`SBC_GPU_TIME` scales it) | `v3d`, Mesa `v3d`/`v3dv` |
| Firmware framebuffer | `simple-framebuffer` in the GPU memory carve-out until `vc4` takes over (the only display with `--display fb`) | `simplefb`, fbcon |
| Ethernet | GENET v5 (DMA rings, status blocks, INTRL2), UniMAC MDIO, BCM54213PE PHY | `bcmgenet`, `unimac-mdio`, Broadcom PHY |
| Wi-Fi | second SD host (SDIO) + CYW43455: CCCR/FBR/CIS, backplane, EROM, SDIO core, emulated FullMAC; open `sbc-emu` and WPA2-PSK `sbc-emu-wpa2` (passphrase `raspberry-pi`) | `sdhci-iproc`, `brcmfmac`, `brcmfmac-wcc` |
| GPIO | function select, set/clear/level, edge/level detect, 2711 pulls, bank IRQs | `pinctrl-bcm2835` |
| I²C1 + BME280 | BSC FIFO controller; BME280 with datasheet compensation (host sets physical values) | `i2c-bcm2835`, `bmp280` (IIO) |
| SPI0 + MCP3008 | FIFO controller, GPIO chip select; bit-serial 10-bit ADC | `spi-bcm2835`, `mcp320x` (IIO) |
| DMA | legacy channels: control-block chains, 2D mode, ignore/increment; blocks that wait for a DREQ move data as the peripheral asks for it (PCM), the rest complete at once | `bcm2835-dma` |
| RNG | iproc RNG200 FIFO | `iproc-rng200` |
| Thermal | AVS monitor temperature register, thermal zone (`sensor cpu-temp C` sets it) | `bcm2711-thermal` |
| PM | watchdog (65536 Hz; expiry resets the machine), AXI bridges for the power domains | `bcm2835-wdt`, `bcm2835-power` |
| System timer | 1 MHz counter, four compare channels | `bcm2835-system-timer` |
| Mini UART | AUX + mini UART as a 16550: the Bluetooth controller's UART, or `ttyS0` with `--serial1 FILE` (script step `serial1 TEXT`) | `8250_bcm2835aux` |

### Running it

```sh
cargo build --release
# Raspberry Pi OS: download and unpack the image (sha256-checked) to cache/os/raspios-lite.img
./target/release/sbc-emu run --sd cache/os/raspios-lite.img --input usb --screenshot screen.png
#   Ctrl-A x quits; --input usb sends your keystrokes to the emulated USB keyboard (HDMI console),
#   the default sends them to the serial console. --script FILE automates (see scripts/).
./target/release/sbc-emu sd ls cache/os/raspios-lite.img          # look inside the boot partition

# quick demo: the Pi kernel with a busybox initramfs
gunzip -c kernel8.img > Image
./target/release/sbc-emu mkinitramfs --busybox busybox.static -o initramfs.cpio
./target/release/sbc-emu run --kernel Image --initrd initramfs.cpio --shell fb --input usb
```

Script steps for the board: `gpio PIN 0|1|float`, `sensor temp|pressure|humidity VALUE`,
`adc CHANNEL VOLTS`; `--gpio-log` prints output edges (the LED). Examples:
`scripts/raspios-firstboot.txt` (setup, Wi-Fi) and `scripts/raspios-peripherals.txt`.

Snapshots: a script step `save FILE` writes the whole machine (CPU, devices, RAM, disk overlay);
`--restore FILE` resumes it (the same SD image must be attached).

More options, mostly after QEMU's:

- `--smp N`: cores (1..4, default 4).
- `--threads single|multi`: how the cores are scheduled. `single` (default): the cores take turns
  on one host thread in rounds of virtual time; runs are **repeatable** (same inputs, same
  result, instruction for instruction). `multi`: a host thread per core, as QEMU's MTTCG; busy
  cores really run in parallel, guest time follows the host clock, and runs are **not**
  repeatable. Devices sit behind one lock, RAM is shared directly; barriers and
  acquire/release become host fences and store-exclusive a host compare-and-swap (QEMU's
  approach: an ABA change between LDXR and STXR goes unnoticed); TLB and instruction-cache
  invalidations reach the other cores before the issuing core goes on, as the guest's DSB
  requires. A debugger with breakpoints or watchpoints switches back to one thread. Raspberry Pi
  OS to `login:`: 37 s single, 29 s multi; the first-boot and peripherals script: 133 s → 120 s.
- `--seed N`: seed of the hardware RNG. One-thread runs use a fixed seed by default, so a run
  with the same inputs is repeatable to the instruction (`--threads multi` seeds from the host).
- `--gdb PORT [--gdb-wait]`: GDB remote stub (`gdb`: `target remote :PORT`, `lldb`:
  `gdb-remote PORT`); `--gdb-wait` holds the machine at reset until a debugger attaches. Cores are
  threads; software/hardware breakpoints switch the cores to the interpreter while set.
- `--hostfwd tcp:[HOSTADDR]:HOSTPORT-[GUESTADDR]:GUESTPORT` (repeatable): forward a host TCP port
  into the guest over Ethernet, e.g. `--hostfwd tcp::2222-:22` then `ssh -p 2222 pi@127.0.0.1`.
- `--serial1 FILE`: the mini UART as `ttyS0`, its output to FILE (Bluetooth is then off: it
  uses that UART, as `dtoverlay=miniuart-bt` arranges; `--no-bluetooth` turns it off too).
- `--display kms|fb`: `kms` (default) describes the vc4 display pipeline, as the image's
  `dtoverlay=vc4-kms-v3d` asks, so the stock `vc4` driver drives HDMI0 (a 1080p monitor; about
  2 s more boot time); `fb` leaves the firmware's 1024×768 framebuffer as the only display.
  The browser build uses `fb`.
- `--no-gpu`: leave out the V3D 3D GPU (it comes with `--display kms`); Mesa then renders in
  software.
- `--sd-persist`: writes go to the SD image file itself instead of a copy-on-write overlay (for a
  scratch copy of the image, e.g. one with Mesa and the GPU tests installed).
- `--usb-drive IMAGE`: a USB stick (copy-on-write, like the SD card) on the third USB-A port;
  given with `--restore`, it is hot-plugged after the restore.
- `--camera`: a USB webcam (UVC, 640×480 YUYV at 30 frames/s of guest time, a test pattern).
- `--audio-out FILE.wav`: what the guest plays on the I²S DAC (`aplay`, `speaker-test`), as a
  16-bit stereo WAV.
- `--otg-keyboard`: the keyboard on the USB-C (DWC2 OTG) port instead of the xHCI ports.
- `--vnc :N[,password=PW]` (port 5900+N): a VNC server for the HDMI framebuffer (Raw encoding of the
  rows that changed, desktop resize); keys go to the USB keyboard, the pointer to the USB tablet. Without
  `password=` it asks for none, which suits TigerVNC/RealVNC viewers; macOS Screen Sharing insists on a
  password, so give one (`open vnc://127.0.0.1:5901`).
- `--cpu-crypto`: the Cryptographic Extension (AES, SHA-1/256, PMULL) as QEMU's `cortex-a72`
  has it; the real BCM2711 does not, so it is off by default.
- Ctrl-A c opens the **monitor** (`help` lists the commands: `info registers|cpus|irq|network|jit`,
  `x`/`xp`/`xlate`, `stop`/`cont`, `savevm`/`loadvm`, `screendump`, `sendkey`, `mouse_move`,
  `mouse_button`, `hostfwd_add`, `gdbserver`, `system_reset`, `quit`); scripts can run them with
  `monitor CMD`.

#### Ubuntu Desktop

```sh
# the pristine image from cdimage.ubuntu.com (sha256-checked), then a seeded 16 GiB working copy
tools/ubuntu-image.sh cache/os/ubuntu-26.04.1-preinstalled-desktop-arm64+raspi.img cache/os/ubuntu-desktop.img
./target/release/sbc-emu run --sd cache/os/ubuntu-desktop.img --sd-persist --ram-mb 3072 \
    --threads multi --hdmi-mode 1280x720 --vnc :1,password=raspberry
```

The first boot (cloud-init, snaps, growing the root filesystem) takes a few minutes; after it
the GDM greeter appears about 30 s after power-on and the desktop about 30 s after logging in
(`pi` / `raspberry`, from `scripts/ubuntu-user-data.yaml`). Ubuntu boots like the firmware
would: `config.txt` with `os_prefix=current/`, a gzip `vmlinuz`, `initrd.img`. GNOME scans out
10-bit `XRGB2101010` buffers, which the HVS model shows. `--sd-persist` takes the image for
itself (a second emulator on it is refused): two guests writing one ext4 filesystem corrupt it.

A logged-in desktop resumes in seconds from a snapshot: `scripts/ubuntu-desktop-snap.txt` boots a
frozen copy of the image (`cache/os/ubuntu-desktop-base.img`, never written), logs in and saves
`cache/os/ubuntu-desktop.snap`; `tools/ubuntu-resume.sh` resumes it with VNC on :1 (password
`raspberry`). Changes live in memory and are gone at exit. Firefox (the snap, on its own Mesa
25.2) renders pages on the V3D.

#### In the browser

```sh
tools/wasm-build.sh                               # web/wasm/sbc-emu.wasm + web/assets
ln -s ../../cache/os/raspios-lite.img web/assets/raspios-lite.img
tools/serve.py                                    # needs HTTP Range support: use this, not http.server
open http://127.0.0.1:8791/            # the sbc-emu page: live 3D demos on the Pi 4's GPU
open http://127.0.0.1:8791/pi4.html    # the full Pi 4 console
```

`index.html` restores a Raspberry Pi OS snapshot with Mesa and runs `kmscube` and `glmark2`
scenes on the emulated V3D GPU (setup in [docs/gpu.md](docs/gpu.md#in-the-browser)).
`pi4.html` offers a cold boot of the SD image, a resume from `web/assets/raspios-lite.snap` (if
present) and the quick busybox demo. Click the screen to type on the USB keyboard; the mouse
over the screen is the USB tablet, and what the guest plays on the DAC comes out of the page
(after the first click or key press, as browsers require). The browser build runs the cores in
turns on one thread: a thread per core would need WebAssembly threads, which need a standard
library rebuilt with atomics (a nightly-only `-Z build-std`), and this project builds on stable.

## Tests

```sh
cargo test --release --workspace
```

GitHub Actions runs the suite on every pull request and push to `main` ([.github/workflows/ci.yml](.github/workflows/ci.yml)):
- macOS on Apple silicon: everything, including the JIT and the differential tests against the
  host CPU;
- Linux x86-64: the interpreter, as a portability check;
- the WebAssembly build.

- `arm-a64/tests/diff_host.rs`, `diff_neon.rs`, `diff_fp.rs`: **differential tests against the
  host CPU** (Apple silicon is AArch64): integer (incl. CRC32), FP and NEON snippets run natively
  and on the emulator from the same random states; registers, flags, NaN payloads and FPSR must
  match bit for bit. `diff_fp.rs` draws a random FPCR (rounding mode, FZ, DN) per case and
  operands from the hard classes (subnormals, overflow, ties, NaNs).
- `arm-a32/tests/neon_diff.rs`: the same for the 32-bit core's NEON: 407 A32 instructions (and
  their T32 encodings, from `arm-none-eabi-as`) against AArch64 equivalents run natively with
  FPCR.FZ/DN (ARMv7's Standard FPSCR value); `neon_ldst.rs` checks every VLD/VST encoding
  against a reference model.
- `bcm2711` unit tests: GPIO edges, BME280 compensation against the Linux driver's formula, MCP3008
  framing against `mcp320x`'s decode, GIC SGI/SPI routing between cores, DMA control-block chains,
  and `smp.rs`: four cores add to one counter with LDXR/STXR, one thread and a thread per core,
  interpreted and compiled (a plain store instead of the compare-and-swap loses ~70% of them).
- `arm-a64/tests/system.rs`: exceptions, EL changes, MMU faults, timer interrupts.
- `bcm2711/tests/watch.rs`: debugger watchpoints stop after the access, on the right core.
- `tools/wasm-smoke.mjs`, `tools/wasm-smoke-sd.mjs`: headless boots of the WebAssembly build.
- `tests/gpu/` + `scripts/gpu-gltest.txt`: OpenGL ES and Vulkan scenes rendered in the guest on the
  emulated V3D and on Mesa's software renderers, compared per pixel ([docs/gpu.md](docs/gpu.md)).

## Layout

```
emu-core/   Bus / Core / Trap, the snapshot serializer and a device tree reader/writer
arm-a64/    the ARMv8-A core: decode+execute, system registers, MMU, FP/NEON
arm-a32/    the ARMv7-A core for 32-bit boards: A32, Thumb-2, VFPv4, NEON, short-descriptor MMU
riscv-rv32/ the RV32 core reused from esp32sim (MIT, see its README and LICENSE), with M/S/U and Sv32 for the ESP32-S31
espressif/  esp32sim's ESP32-S31 SoC (esp32s31, esp-soc, esp-periph, its 32-bit emu-core), vendored from esp32sim d043ff1d (MIT)
mali400/      the Mali-400 MP2 GPU (Utgard): GP vertex shader and PLBU, PP fragment shader, tiles, MMUs, as Linux's and Mesa's lima drivers use it
peripherals/  device models shared between boards: GIC-400, DesignWare UART, the I²C device interface and I²C chips (RK808, ACT8846, SYR827/828 regulators, HYM8563 RTC), the 24 MHz timebase
midgard/      the Mali-T760 MP4 GPU (Midgard) of the RK3288: job manager, MMU, Midgard shader core, tiler, fragment pipeline, textures, as Linux's and Mesa's panfrost drivers use it; mali-replay for captured job chains
allwinner-h3/ the experimental Orange Pi One machine (H3): four cores, GIC-400, UART0, CCU, timers, SD/MMC, HDMI (DE2 mixer, TCON, DW-HDMI), USB (OHCI with keyboard and tablet, EHCI), Ethernet (EMAC, internal PHY), host PSCI, U-Boot-style DTB patching
riscv64/      an RV64GC hart (IMAFDC, Zba, Zbb; M/S/U; Sv39) and its riscv-tests suite
starfive-jh7110/ the experimental Orange Pi RV machine (JH7110): four U74 harts, CLINT, PLIC, UART0, clock/reset generators, PMU, DW MMC (microSD), GMAC + YT8531C PHY, PLDA PCIe + VL805 xHCI, M.2 NVMe, SDIO Wi-Fi (BCM43456), Bluetooth on UART1, GPIO interrupts, AXI DMA + PWM DAC and HDMI I2S audio, DC8200 + Inno HDMI, I²C with the AXP15060 PMIC and EEPROM, the PKA crypto engine, TRNG, the boot ROM's SD path with the L2 LIM and DDR controller (U-Boot), OpenSBI direct boot
powervr/        Imagination PowerVR Rogue GPU (BXE-4-32): registers and the firmware interface for Linux's powervr driver
v3d/        the V3D 4.2 3D GPU (packets generated from Mesa's v3d_packet.xml by tools/gen_v3d_packets.py)
bcm2711/    the Pi 4 machine: GIC, PL011, mailbox, SDHCI, DWC2, PCIe + xHCI, USB devices, I²S, Bluetooth,
            GENET, device tree, boot, FAT
rk3588/     the experimental Orange Pi 5 machine (RK3588S): GICv3, UART2, timer
sbc-machine/ the board catalogue and the host-facing machine layer the boards share
sbc-net/    virtual network: DHCP/ARP/ICMP/DNS/NTP, user-mode NAT, 802.11 AP (from esp32sim)
cli/        the sbc-emu command
wasm/       C ABI for the browser; web/ is the page and worker
scripts/    action scripts (boots, demos, GPU tests, Ubuntu)
```

## Known gaps (Raspberry Pi 4)

Firefox (the Ubuntu snap) drops some glyphs on text-heavy pages such as GitHub's: the glyphs reach
the atlas and the text shader samples them, but some never reach the page tiles (under
investigation). The 3D GPU's shader cores are interpreted (glmark2 runs at 2–258 fps at 1080p, score 117; no ASTC, transform feedback or
geometry shaders), no HDMI1 (the second port has
no monitor), no CEC devices; the HVS draws RGB planes only (no YUV video planes, no tiled
formats) and scales with nearest neighbour;
no CSI camera (the webcam is USB); no USB 3 devices (the USB 3 root ports stay empty), no USB
hubs, no isochronous transfers; Bluetooth has no connections (the sensor only advertises) and
no classic devices; Wi-Fi has no radio model (emulated firmware host interface, open and
WPA2-PSK networks); only the PCM's DREQ paces DMA (the other peripherals complete at once);
32-bit programs get ARMv7's instruction set plus ARMv8's load-acquire/store-release and SEVL
(not VSEL, VRINT, VMAXNM or CRC32; the ID registers say so) and the 32-bit core's VFP is not bit-exact
(no flush-to-zero, no cumulative exception flags);
no LSE instructions, crypto only with `--cpu-crypto` (the BCM2711's A72 has neither; the ID
registers say so); the native JIT needs an AArch64 host and is still well below a real Pi 4
core; the browser build is about 3× slower than native and single-threaded.

## Compared with QEMU's `raspi4b`

QEMU's side is what its own board page lists (["Raspberry Pi boards"](https://www.qemu.org/docs/master/system/arm/raspi.html),
documentation for QEMU 11.1.90, November 2026): "implemented devices", "missing devices", and
nothing for what the page does not mention. QEMU also models the raspi0, 1A+, 2B, 3A+ and 3B
(ARM1176, Cortex-A7, Cortex-A53 cores); sbc-emu's Pi 4 boots 64-bit kernels only, but runs
32-bit programs under them.

| | sbc-emu `pi4b` | QEMU `raspi4b` (its documentation) |
|---|---|---|
| RAM | `--ram-mb`, up to 3.9 GiB (below the peripheral window) | 2 GiB |
| Boot | the stock SD image: config.txt (`[pi4]`, `os_prefix`), cmdline.txt, gzip kernel, initramfs | not on the board page |
| CPU, GIC, system timer, DMA, mailbox and firmware properties, RNG, thermal sensor | yes | yes |
| Serial ports (PL011, mini UART) | yes | yes |
| SD host | yes (incl. erase/TRIM) | yes |
| GPIO, SPI, I²C controllers | yes, with an LED, a button, a BME280 and an MCP3008 attached | yes (controllers) |
| PWM | no | listed as missing |
| Display | HDMI through the stock `vc4` KMS driver: HVS, pixel valve, HDMI encoder, EDID (`--hdmi-mode`), HDMI audio | the firmware frame buffer |
| 3D GPU (V3D) | yes: Mesa's OpenGL ES 3.1 and Vulkan 1.3 drivers | not listed |
| PCIe root complex | yes (INTx; the device tree names no MSI controller) | yes; MSI/MSI-X listed as missing (INTx only) |
| USB | VL805 xHCI behind PCIe (keyboard, tablet, stick, webcam) and the DWC2 OTG port | DWC2 USB2 host |
| GENET Ethernet | yes, with NAT and port forwarding | yes |
| Wi-Fi, Bluetooth (CYW43455) | yes: stock `brcmfmac` (WPA2), `hci_bcm` (LE scanning) | not listed |
| I²S audio | yes: a DAC HAT, to a WAV file or the browser | not listed |
| Cores on host threads | `--threads multi` (or in turns, repeatable to the instruction) | not on the board page (QEMU's MTTCG) |
| In the browser | yes (WebAssembly build with its own JIT) | no |

Speed, measured (not from QEMU's documentation): the same Raspberry Pi OS image and kernel, four
cores, 2 GiB, on an Apple silicon Mac; QEMU 11.1.3 `raspi4b` with multi-threaded TCG, sbc-emu
`--threads multi`. Host wall-clock times (sbc-emu's guest clock is virtual, so its times are taken
from the host between serial markers); single runs, on a host with other jobs, so ±10%.

| | sbc-emu | QEMU `raspi4b` |
|---|---|---|
| Boot to `login:` | 13.5 s | 42–51 s |
| 1500 × `/bin/true` (process creation) | 5.3 s | 7.3 s |
| `find /usr -type f` three times (file system, system calls) | 1.3 s | 6.9 s |
| `python3`: sum of 10 M squares | 2.4 s | 4.0 s |
| the same, four at once | 2.6 s | 4.9 s |
| `sha256sum` of 1 GB of zeros | 3.1 s | 4.5 s |
| glmark2 at 1920×1080 | score 117 | no 3D GPU |

QEMU needed three changes to boot the image at all: the kernel uncompressed, the Bluetooth node
removed from the device tree (its PL011 is the console), and `systemd.watchdog_sec=off` (its
watchdog model reset the board every few seconds); sbc-emu boots the stock SD image.

## 32-bit programs

The Pi 4's Cortex-A72 runs 32-bit (AArch32) programs at EL0 under a 64-bit kernel, and so does
the emulated one: Raspberry Pi OS's own kernel reports `CPU features: detected: 32-bit EL0
Support` and runs armhf binaries next to arm64 ones. When the kernel returns to a 32-bit
program, the 64-bit core (`arm-a64`) hands the user registers to an `arm-a32` core, which runs
the program through the 64-bit core's page tables and asks it for the few system registers a
user program may read (thread ID, generic timer, CP15 barriers). Every exception (system call,
page fault, undefined instruction, interrupt) brings the registers back and is taken to the
64-bit kernel with an AArch32 syndrome, as on the hardware (`arm-a64/src/aarch32.rs`,
`arm-a32/src/el0.rs`). The 32-bit core's own JIT (A32/T32 to A64) compiles hot 32-bit code
(`SBC_JIT32=0` interprets it; `SBC_AARCH32_UNDEF=1` prints every UNDEFINED instruction of a 32-bit
program, which the guest otherwise only sees as SIGILL): a BusyBox shell loop of 20,000 iterations takes 0.5 s instead of
9.5 s, a gzip of 1.3 MiB 0.4 s instead of 7 s, close to the same 64-bit programs. With
`--threads multi` the 32-bit store-exclusive is a host compare-and-swap and barriers are host
fences, as for 64-bit code.

| | Evidence |
|---|---|
| Syndromes, register mapping, interrupts, snapshots | `arm-a64/tests/aarch32_el0.rs`: an EL1 kernel and a 32-bit program with A32 and 16-bit Thumb SVCs, UNDEFINED, a data abort, a trapped CP15 timer read, TPIDRURO/URW, VFP registers in V0–V15, exclusives; an IRQ at the AArch32 vector; a snapshot taken inside the program resumes it; JIT-compiled 32-bit code ends exactly as interpreted (registers, memory, instruction count); two host threads add 100,000 to one counter with LDREX/STREX, interpreted and compiled |
| 32-bit Raspberry Pi OS | the unmodified Raspberry Pi OS Lite armhf image (2026-10-06, trixie; ARMv6 userland on the 64-bit `kernel8.img`, as on a Pi 4): first-boot wizard, login, `getconf LONG_BIT` = 32, Python 3 and OpenSSL 3.5 (its NEON code) give correct digests, DHCP and ping, `apt-get update` and `apt-get install` from the Raspbian archive. Its atomics go through the kernel's kuser helpers, which use ARMv8's STLEX |
| Debian armhf BusyBox under the Pi kernel | Debian 13's `busybox-static` (Thumb-2, hard float) in an initramfs next to the arm64 BusyBox: shell loops, background jobs, `sha256sum` and a gzip round trip equal to the arm64 tools' results |
| A whole armhf Debian userland | Raspberry Pi OS with Armbian's Orange Pi One image (Debian 13 armhf) as a USB stick: `chroot` into it runs its bash, perl, `dpkg` and Python 3, with `--threads single` and `multi`: SHA-256, zlib, integer and floating-point results equal to the arm64 Python's; 8 Python threads × 20,000 increments under one lock give 160,000; four shells counting under `flock` give 400; no kernel warnings |

## The JIT

`arm-a64/src/jit.rs`. Guest and host share the A64 instruction set, so integer and FP/SIMD
data-processing instructions are emitted as themselves with guest registers renamed to the host
registers that cache them — the host computes exactly the architected result, flags included.
Inside a block guest X registers and SP live in host x0–x8/x22–x28 and V registers in v16–v31
(loaded on first use, written back before exits and interpreter calls); the guest NZCV and FPSR
stay in the host's, also across linked branches and the dispatcher. A block continues past short
forward conditional skips (their taken side leaves through a side exit after the block). Loads and
stores probe the core's micro-TLB inline (about 10 host instructions: the entry holds PA − VA, the
access is `ldr rt, [x21, x14]`); the slow path after the block first refills the micro-TLB through
a shared stub and only then calls the interpreter; system instructions, exception generation and
ERET call the interpreter and end the block. FP/SIMD instructions run natively only
after the interpreter has proven it implements them (so extensions the emulated Cortex-A72 lacks
stay undefined). Blocks are keyed by virtual and physical PC, EL, SPSel and FP enable, and are
discarded on `IC IVAU`/`IC IALLU` (CTR_EL0.DIC = 0 obliges software to issue them). Every CPU test
runs with it too: `SBC_JIT=1 cargo test -p arm-a64`. `--no-jit` turns it off.

What makes it fast (Raspberry Pi OS cold boot to `login:`, measured step by step on an M5 host
with `--no-display`; single runs, some next to other jobs, so ±10%):

| Step | `login:` after |
|---|---|
| block JIT, single-entry micro-TLBs, every block returns to the machine loop | 150 s |
| 64-entry direct-mapped micro-TLBs (fetch/read/write, in the interpreter and inline in compiled code); PAC/BTI hints, barriers, `MRS SP_EL0` native | 78 s |
| block chaining: block exits jump through a shared dispatcher (fetch micro-TLB + front cache) into the next compiled block while `instret < chain_until` (the machine's next device deadline) | 76 s |
| SIMD&FP loads/stores, PRFM and `MRS TPIDR_*` native; MRS and data-cache operations no longer end blocks | 68 s |
| direct linking: an exit to a constant target on the same page branches straight into that block (patched in when it is compiled; same-page links die with the page, so they never need unlinking) | 53 s |
| four cores, Bluetooth and USB devices added since (the measurements below start here) | 56.5 s |
| a fetch micro-TLB miss is filled where the compiled code is looked up (it used to send one instruction to the interpreter); a block that is not hot yet runs in the interpreter to its end, and only block starts count towards compiling (mid-block PCs no longer become overlapping blocks: 2.4 → 1.65 M compiles) | 52.8 s |
| 256-entry micro-TLBs (fetch misses 110 M → 33 M; more would push `Cpu` fields that compiled code reaches by immediate offsets out of range) | 46.5 s |
| native FMOV between X/W and D/S registers, LDNP/STNP, DC ZVA (64-byte zeroing), DUP from a general register, one-register LD1/ST1 (the most frequent instructions compiled blocks still handed to the interpreter) | 42.4 s |
| ADMA2 on the SD card (the driver moved every sector through the data port, 4 bytes per load) | 36.9 s |
| main in November 2026 (V3D, display and device work since; the next row's baseline) | 29.6 s |
| guest registers cached in host registers (16 X, 16 V) instead of loaded and stored around every instruction; load/store slow paths out of line; the micro-TLBs 2-way set associative with a hashed index (python3's data and its mmap heap shared a direct-mapped slot: ~10 M interpreter fallbacks per second); an enter stub instead of a prologue per block, with instret and the RAM base in host registers and NZCV live across linked blocks | 26.4 s (17.5 s with `--threads multi`) |
| main on 20 November 2026 (the Orange Pi 5 work since; the next row's baseline, measured next to it) | 24.1 s (16.4 s) |
| FPSR live in the host register across blocks (each `MRS`/`MSR FPSR` costs 3–4 ns on Apple cores; blocks with FP saved and restored it at every boundary); micro-TLB entries hold PA − VA and the set index is the page number's low bits, so pages P and P + 1 never share a set and a page-crossing access needs no separate check (about 10 host instructions per load instead of 17); a miss refills the entry through a shared stub (`h_tlb`, no interpreter) and retries; interpreter calls through a shared stub (a call site went from ~37 instructions to ~6); blocks continue past short forward conditional branches; direct branches into another page linked too, behind a check that the fetch micro-TLB still maps the target page to the same physical page (42% of dispatcher entries in python3) | 19.0 s (13.5 s) |

Blocks average about 5 guest instructions; a chain now runs about 100 before it returns to the
machine loop. A chain stops at the budget/deadline and whenever an interpreted instruction may need
the machine's attention (`chain_stop`: device access, MSR, cache/TLB maintenance, exceptions, ERET).

Debugging switches: `SBC_JIT_VERIFY=1` re-runs every block in the interpreter from the same state
(stores rolled back; blocks whose code changed before an `IC`, or that touched a device, are
skipped) and reports the first mismatch; `SBC_JIT_PROFILE=1` prints the instruction words compiled
blocks still hand to the interpreter; `SBC_JIT_CHAIN=0` turns chaining and linking off;
`SBC_JIT_SUPERBLOCK=0` ends blocks at every conditional branch;
`SBC_JIT_MAP=FILE` writes the host address, size and guest PC of every compiled block (to attribute
host profiler samples, e.g. from `samply`); `SBC_JIT_DUMP=<pc>[,<pc>...]` writes the guest and host
instruction words of the blocks at those PCs to `jit-<pc>.guest`/`.host`;
`SBC_JIT_WATCH=<page>` traces compiles and invalidations on one physical page.

### In the browser

`arm-a64/src/jit_wasm.rs` compiles a hot **region** into its own small WebAssembly module
(imports the emulator's memory and function table, exports one function) which the worker
instantiates and installs in the table. A region is the hot entry block plus the basic blocks
reachable through constant branches on the same page (up to 24): one function with a `loop` +
`br_table` dispatch, so loops and local control flow branch inside the function instead of going
back to Rust (WebAssembly code cannot be patched, so this replaces the native JIT's direct links).
Exits leaving the region return to a chaining loop in `jit_step` that calls the next block from the
front cache. Translated natively: integer data processing (incl. extended-register add/sub,
CCMP/CCMN, variable shifts, REV/RBIT/CLZ, ADC/SBC, UMULH/SMULH), flags, conditional select,
branches, integer loads/stores (inline micro-TLB probe, direct RAM access), NOP-class hints,
barriers, PRFM and thread-pointer/`SP_EL0` reads; everything else calls the interpreter for one
instruction. Execution counting before compilation uses a direct-mapped table (no hashing on the
interpreter's path).

On by default (`pi4.html?jit=0` or `index.html?jit=0` turns it off). Raspberry Pi OS cold boot to `login:` in the
browser build (`tools/wasm-smoke-sd.mjs`, Node 26, M5, runs alone):

| Step | `login:` after |
|---|---|
| block per module, every block back to the machine loop | 313 s |
| direct-mapped hot counters, chaining loop in `jit_step` | 252 s |
| more native instructions (hints, CCMP, extended add, shifts, ADC, MULH, ...) | 229 s |
| regions (loop + `br_table` per page-local control flow) | 160 s |

The interpreter alone takes about 360 s; the busybox kernel comes up after 1.9 s (13.5 s
interpreted). Checks:

- `node tools/wasm-jit-selftest.mjs` runs A64 snippets through the wasm build with the JIT off and
  on and compares registers and flags;
- `SBC_JIT=1 SBC_JIT_VERIFY=1 node tools/wasm-smoke.mjs 30` (or `wasm-smoke-sd.mjs IMAGE SECS`)
  re-runs every compiled block in the interpreter (stores are rolled back) and prints the first
  mismatch, with the block's code; `SBC_JIT_PROFILE=1 node tools/wasm-smoke-sd.mjs ...` lists the
  instructions compiled code still hands to the interpreter, `SBC_JIT_HOT=n` sets the compile
  threshold and `SBC_WASM=path` picks another build (for before/after comparisons).
