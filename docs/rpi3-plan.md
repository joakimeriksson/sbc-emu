# Raspberry Pi 3 (BCM2837): plan

The Raspberry Pi 3 family on the same footing as the Pi 4: the boards' stock software, unmodified,
on register-level models of their hardware. Most of the Pi 4 machine (`bcm2711/`) carries over;
the work is in the parts the BCM2837 does differently.

## The family

One SoC, BCM2837 (the same die is sold as BCM2710; B0 is the later stepping), four Cortex-A53s
without the Crypto Extension (as the Pi 4's A72s), VideoCore IV. The boards differ only in RAM,
what hangs off the USB port, the Wi-Fi chip and the revision code:

| Model | Year | SoC, clock, RAM | USB / wired network | Wi-Fi + BT | Revision |
|---|---|---|---|---|---|
| **3 Model B+** | 2018 | BCM2837B0, 1.4 GHz, 1 GiB | LAN7515: USB 2.0 hub + gigabit Ethernet over USB (`lan78xx`) | CYW43455 (2.4 + 5 GHz), the Pi 4's chip | `a020d3` |
| 3 Model B | 2016 | BCM2837, 1.2 GHz, 1 GiB | LAN9514: USB 2.0 hub + 100 Mbit Ethernet (`smsc95xx`) | BCM43438 (2.4 GHz) | `a02082` (also `a22082`, `a32082`) |
| 3 Model A+ | 2018 | BCM2837B0, 1.4 GHz, 512 MiB | one USB port on the SoC, no Ethernet | CYW43455 | `9020e0` |
| Compute Module 3 / 3+ | 2017 / 2019 | BCM2837 / B0, 1 GiB, eMMC (3+: 0–32 GB) | the SoC's USB port on the edge connector | none | `a020a0` / `a02100` |
| Zero 2 W | 2021 | RP3A0 (BCM2710A1 + 512 MiB in one package), 1 GHz | the SoC's USB OTG port, no hub | CYW43436 | `902120` |

The Pi 2 Model B v1.2 has the same SoC at 900 MHz. One machine with `--model 3b+|3b|3a+|zero2w`
covers the family; the model picks the RAM size, the revision code the mailbox reports, the USB
topology, the Wi-Fi chip and the device tree's model string.

**First target: the 3 Model B+.** It is the most common Pi 3, and its Wi-Fi/Bluetooth chip is the
CYW43455 the Pi 4 machine already models. The 3B and Zero 2 W then need only the smaller USB
Ethernet and Wi-Fi variants.

**Software:** the same Raspberry Pi OS Lite arm64 image the Pi 4 boots (its `kernel8.img` runs on
both; the emulated firmware has to honour `config.txt`'s `[pi3]` / `[pi3+]` sections instead of
`[pi4]`), then Alpine's `linux-rpi` for a small fixture. Stretch: 32-bit Raspberry Pi OS (armhf,
`kernel7.img`), which on a Pi 3 runs the A53s in AArch32 at EL1 — a job for the `arm-a32` core
plus the ARMv8 AArch32 additions (LDA/STL, CRC32, VSEL, VCVTA and friends).

## What carries over from the Pi 4

| Block | Pi 4 code | Notes for the Pi 3 |
|---|---|---|
| CPU cores, JITs | `arm-a64` | Cortex-A53 identity (MIDR `0x410fd034`); ARMv8.0 like the A72, no new instructions |
| PL011 UART0, mini UART | `bcm2711/src/periph.rs`, `pl011` | Same blocks; on the 3B+ UART0 goes to Bluetooth and the mini UART is the console, as on the Pi 4 without `miniuart-bt` |
| Mailbox, firmware properties | `mailbox.rs` | Revision code, memory split (RAM below the VideoCore's share of 1 GiB), clocks |
| GPIO, I²C (BSC), SPI, PCM/I²S, DMA, PM/watchdog, system timer | `gpio.rs`, `i2c.rs`, `spi.rs`, `i2s.rs`, `dma.rs`, `periph.rs` | Same BCM2835-family blocks; the Pi 4 has more instances |
| USB host: DWC2 OTG core | `dwc2.rs` | **The Pi 3's only USB host.** Needs a high-speed root port and split transactions (below) |
| USB devices: keyboard, tablet, storage, UVC camera | `emu-core/src/usb.rs` | Unchanged |
| SDHCI (Arasan) | `sdhci.rs` | The Pi 3's Arasan is the Pi 4's `mmcnr` (Wi-Fi) block: `brcm,bcm2835-sdhci`, 32-bit register writes, no ADMA |
| CYW43455 Wi-Fi (SDIO) and Bluetooth (H4) | `wifi.rs`, `bluetooth.rs` | As is on the 3B+ / 3A+ |
| Display, firmware framebuffer | `mailbox.rs` | `simple-framebuffer` works first, as `--display fb` on the Pi 4 |
| Network | `sbc-net` | DHCP, DNS, NAT, `--hostfwd` |
| Snapshots, gdb, monitor, VNC, `--threads multi`, WebAssembly | the board facade, `mt.rs` | `mt.rs` needs the new interrupt controllers |

The bus-address alias (devices see RAM at `0xc000_0000`) and the `0x7e00_0000` peripheral bus
addresses are the same as on the Pi 4.

## What is new

| Part | Rough size | Notes |
|---|---|---|
| Memory map | small | Peripherals at `0x3f00_0000` (the Pi 4's `0xfe00_0000`), the per-core block at `0x4000_0000`, RAM from 0 up to the VideoCore's share. Generic timer at 19.2 MHz (Pi 4: 54 MHz) |
| BCM2836 per-core interrupt controller (`brcm,bcm2836-l1-intc`, `0x4000_0000`) | ~250 lines | No GIC on the Pi 3. Routes each core's generic-timer interrupts (they are not PPIs here), four mailboxes per core (the IPIs), the GPU interrupt to one core, the local timer |
| ARM-side interrupt controller (`brcm,bcm2836-armctrl-ic`, `0x3f00_b200`) | ~150 lines | 64 VideoCore-side interrupts + 8 ARM ones, chained into the per-core controller |
| `mt.rs` for these controllers | ~150 lines | IPIs become mailbox writes; a core waiting in WFI wakes on its own controller's lines |
| Machine, generated device tree, boot | ~800 lines, from `bcm2711/src/machine.rs`, `fdt.rs`, `boot.rs` | Secondary cores by PSCI as on the Pi 4 (the real tree uses a spin table at `0xd8`; either works with our own tree) |
| `sdhost` (`brcm,bcm2835-sdhost`, `0x3f20_2000`) | ~400 lines | The SD card's controller on a real Pi 3 (the Arasan is the Wi-Fi's). Until Wi-Fi is on, the card can sit on the Arasan instead |
| DWC2: high-speed root port and split transactions | ~200 lines | The hub is high-speed; the keyboard behind it is low/full-speed, so the driver uses start/complete splits (HCSPLT). `dwc2.rs` today has a full-speed root port |
| USB hub (shared, `emu-core`) | ~250 lines | Hub class: port status/change, power, reset, the status-change interrupt endpoint. Useful to every board |
| LAN7515 Ethernet (3B+, `lan78xx`) | ~500 lines | Vendor control requests for registers, MDIO to the internal PHY, bulk frames with RX/TX command words, the interrupt endpoint for link |
| LAN9514 Ethernet (3B, `smsc95xx`) | ~400 lines | The same idea, simpler framing |
| RNG (`brcm,bcm2835-rng`), thermal (`brcm,bcm2837-thermal`) | ~150 lines | Older, simpler blocks than the Pi 4's RNG200 and AVS |
| HDMI through `vc4` KMS: HVS (not HVS5), pixel valve 2, BCM2835 HDMI | ~300 lines of changes to `vc4.rs` | The same pipeline one generation older: different display-list words and HDMI registers, no separate PHY |
| BCM43438 / CYW43436 Wi-Fi (3B, Zero 2 W) | ~200 lines | Chip ID, RAM size, firmware name in `wifi.rs` |
| VideoCore IV 3D (`brcm,bcm2835-v3d`, Mesa `vc4`) | 5–8k lines, its own crate `vc4-3d/` | In scope (decided 2026-11-27). See [Graphics and video](#graphics-and-video) |
| HDMI audio, CEC, composite (VEC) | ~400 lines | HDMI audio through the MAI FIFO and DMA, as on the Pi 4; the composite encoder on pixel valve 2's other input |
| Hardware video decode (`bcm2835-codec` over VCHIQ) | ~1.5–2k lines | See [Graphics and video](#graphics-and-video) |

About 2.5–3k new lines for the board without graphics, against the Pi 4 crate's 8k; the 3D GPU
and video add another 7–10k.

## Graphics and video

Full support for the display, the 3D GPU and the video blocks, with the stock drivers: Linux's
`vc4` (one DRM driver for both display and 3D on this SoC), Mesa's `vc4` Gallium driver, and the
Raspberry Pi kernel's VCHIQ-based media drivers.

**HDMI (milestone 4).** The KMS pipeline the image's `config.txt` asks for (`dtoverlay=vc4-kms-v3d`):

- HVS (the pre-HVS5 one): display lists in its own memory, RGB and YUV planes (the video path
  needs YUV420 / NV12 planes and scaling), alpha composition when the host takes a picture,
  per-channel frame counters, the list latched at each frame start.
- Pixel valves 0–2 with vblank at the mode's rate; pixel valve 2 drives HDMI.
- The BCM2835 HDMI encoder: no separate PHY block, the HD register block, DDC on the BSC
  controller at `0x7e80_5000` with the same 1080p EDID as the Pi 4's monitor, hot-plug on a GPIO.
- HDMI audio: the MAI FIFO fed by DMA, as `vc4.rs` does it for the Pi 4 (`speaker-test -D hdmi`).
- CEC: the register block so `cec-ctl` sees an adapter; a virtual TV that answers polls.
- Composite (VEC on pixel valve 2's other input) after HDMI, with a fixed PAL/NTSC picture.

**VideoCore IV 3D (milestone 7).** Broadcom published the *VideoCore IV 3D Architecture Reference
Guide* (2013), so unlike the Pi 4's V3D 4.2 the whole GPU is documented. A new crate `vc4-3d/`:

- **Control list executor**: the binning and rendering threads (CT0/CT1), their control-list
  records (state, primitives, branch/return, tile coordinates, store/load tile buffer).
- **Binner** (PTB): primitives into per-tile lists in the overflow memory the kernel hands out,
  with the out-of-memory interrupt the driver feeds (`OUTOMEM`) and the flush/frame-done ones.
- **Vertex path**: VCD (vertex cache DMA) into the VPM, the coordinate and vertex shaders on the
  QPUs, VDW (VPM DMA writer) out, primitive setup with clipping and viewport.
- **QPUs**: 12 (three slices of four), 16-wide SIMD, dual-issue add and multiply ALUs, register
  files A and B, accumulators r0–r5, the SFU (recip, rsqrt, exp2, log2), the TMU (texture fetches
  through writes to its registers, results in r4), uniforms, varyings, semaphores, conditional
  branches, thread switches. Interpreter first; a JIT (to AArch64 and WebAssembly, as for the
  Maxwell shaders) when the interpreter is the bottleneck.
- **Fragment path**: rasterizer, early Z, the tile buffer (64×64 tiles: colour, Z, stencil),
  blending and the stores to memory in the T- and LT-tiled formats the TMU also reads.
- **Kernel interface**: Linux's `vc4` validates the submitted lists and shaders and builds the
  render control list itself, so the model sees exactly what real hardware gets. No GPU MMU:
  buffers are contiguous (CMA) and addressed physically.
- **Evidence**: Mesa's `vc4` reports `V3D 2.1` (OpenGL ES 2.0, desktop GL 2.1); `kmscube`; the
  GLES 2 scenes of the Pi 4's GL suite pixel for pixel against llvmpipe; `glmark2-es2-drm`.

**Hardware video decode (milestone 9).** On the Pi 3 the H.264, MPEG-2 and VC-1 decoders, the JPEG
codec and the ISP sit behind the VideoCore firmware, reached through VCHIQ (a message queue in
shared memory, `brcm,bcm2835-vchiq`). The Raspberry Pi kernel's `bcm2835-codec` turns that into a
V4L2 memory-to-memory decoder (`/dev/video10`), which `ffmpeg -c:v h264_v4l2m2m`, GStreamer and VLC use.

- Model VCHIQ's slots and doorbells, and the MMAL services the codec driver opens (decoder,
  encoder, ISP for format conversion).
- Decode on the host with an H.264 decoder in Rust (decided 2026-11-27), so the same code runs
  natively and in the browser, writing the guest's output buffers. Candidates to evaluate first,
  all young (2026): `rusty_h264` / `rusty_h264-decoder` (safe Rust, fuzzed; its own list of gaps
  includes High-profile 8×8 CABAC residuals and CABAC I_PCM), `rust_h264` (claims Baseline, Main
  and High with CABAC and MBAFF), `yscv-video` (its docs disagree on CABAC). For each: the licence
  (it ships in our BSD-3-Clause binary: MIT, Apache-2.0 or BSD are fine, GPL/LGPL are not), a `wasm32-unknown-unknown` build, and conformance on the JVT
  bitstreams plus files from Pi OS's own encoders, checked frame by frame against the host's
  ffmpeg. If none covers High profile with CABAC (what cameras and most files use), write our
  own as a crate (`h264/`, progressive 8-bit 4:2:0 High profile first, interlaced later), with
  the decoder's hot loops in SIMD-friendly Rust for the browser.
- Then the guest decodes a 1080p H.264 file into the HVS's YUV plane at the stream's rate, which
  a software decoder on four emulated A53s can't do.
- The same VCHIQ work gives `bcm2835-audio` (the headphone jack through the firmware) and the
  firmware camera service; unicam (CSI-2) is a separate, direct block.

**Crate layout:** a new `bcm2837/` crate that uses the Pi 4's modules, with base addresses and
instance counts as parameters where they differ. That changes the Pi 4 crate the least. Folding both
into one `bcm283x` crate can come later, if the two diverge less than expected.

## Milestones

Each ends with something a script checks, as on the other boards.

1. **Serial shell.** Four A53s, both interrupt controllers, the generic and system timers, the mini
   UART, the mailbox, a generated device tree; the Pi OS `kernel8.img` with a busybox initramfs.
   *Check:* `smp: Brought up 1 node, 4 CPUs` and a shell (`scripts/rpi3-shell.txt`).
2. **Raspberry Pi OS Lite from the SD card.** `config.txt` sections for the Pi 3, the card on the
   Arasan (or `sdhost`). *Check:* `raspberrypi login:` with the unmodified image, all four cores.
3. **USB and Ethernet.** High-speed DWC2 root port with splits, the hub, the keyboard, the LAN7515.
   *Check:* `lsusb -t` shows the hub tree; DHCP, ping, DNS and `wget` over `eth0`; typing on tty1.
4. **HDMI.** The HVS (RGB and YUV planes, scaling), pixel valves, HDMI with audio and CEC for
   `vc4`. *Check:* `HDMI-A-1` connected, the console at 1920×1080 over VNC, `kmstest` page flips at
   60 fps, `speaker-test -D hdmi` gives a 440 Hz tone on the host, `cec-ctl` finds the TV.
   *Done* (`scripts/rpi3-hdmi.txt`); composite (VEC) stays with milestone 8.
5. **Wi-Fi and Bluetooth.** `sdhost` for the card, the CYW43455 on the Arasan, Bluetooth on UART0.
   *Check:* `nmcli dev wifi connect`, DHCP over Wi-Fi; `bluetoothctl` scan finds the virtual sensor.
6. **The rest of the platform.** `--threads multi`, snapshots, gdb, a browser page, the `3a+` and
   `zero2w` models (`3b` once the LAN9514 and BCM43438 are in). *Check:* boot times against the
   Pi 4's, snapshot round trip, the browser smoke test.
7. **VideoCore IV 3D**, in steps: control lists and the tile buffer (clears and stores), then the
   binner and a flat-shaded triangle, the QPU interpreter, textures, the rest of the GLES 2 state.
   *Check:* Mesa reports `V3D 2.1`; `kmscube`; the GLES 2 scenes of the Pi 4's GL suite against
   llvmpipe; all `glmark2-es2-drm` scenes; the desktop (Pi OS with desktop, labwc) on the GPU.
8. **Composite video** (VEC). *Check:* `modetest` lists `Composite-1`, a PAL picture over VNC.
   *Done* (`scripts/rpi3-composite.txt`; the image has `kmsprint`, not `modetest`): PAL and NTSC
   with `dtoverlay=vc4-kms-v3d,composite` and `sdtv_mode`; HDMI and composite share pixel valve 2.
9. **Hardware video decode** through VCHIQ and `bcm2835-codec`, decoded on the host. *Check:*
   `v4l2-ctl --list-devices` shows `bcm2835-codec-decode`; `ffmpeg -c:v h264_v4l2m2m` decodes a test
   file bit-exactly against the host's decoder; `bcm2835-audio` plays to the host too.
10. *Stretch:* **32-bit Raspberry Pi OS** on `arm-a32` with the ARMv8 AArch32 additions.

## Risks

- **DWC2 cost.** The `dwc2` driver retries NAKed bulk-IN transfers on the Ethernet's endpoint all
  the time; done naively, that can eat the CPU time the guest needs. The model should hold a NAKed
  channel until data arrives instead of completing it at once.
- **Split transactions** are the part of DWC2 that is easiest to get subtly wrong; test them with the
  keyboard behind the hub before the Ethernet.
- **Interrupt routing in `--threads multi`.** The Pi 4's threads lean on the GIC model; the per-core
  controller has to give the same guarantees (an IPI wakes the core it is for).
- **`config.txt`.** Pi OS's file has `[pi4]`, `[pi5]` and `[all]` sections; the emulated firmware
  must pick the Pi 3's the way the real one does, or the kernel gets the wrong overlays.

## Open questions

- ~~Start with the 3 Model B+ or the 3 Model B?~~ Decided 2026-11-27: the 3 Model B+.
- ~~Is VideoCore IV 3D in scope?~~ Decided 2026-11-27: full support for HDMI, the 3D GPU and video.
- ~~Video decode on the host: Rust or ffmpeg?~~ Decided 2026-11-27: an H.264 decoder in Rust; a
  crate if one passes the evaluation above, our own otherwise.
- 32-bit Raspberry Pi OS: worth doing?
- Who owns it: add a row to the areas table in `AGENTS.md` when work starts.
