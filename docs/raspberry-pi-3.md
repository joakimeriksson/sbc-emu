# Raspberry Pi 3 Model B+

An instruction-level BCM2837 that boots the unmodified **Raspberry Pi OS Lite (trixie, arm64)
SD-card image**, the same image the [Raspberry Pi 4](raspberry-pi-4.md) boots, on four Cortex-A53
cores: its first-boot wizard runs on the framebuffer console with the USB keyboard, and the
network works over the LAN7515's USB Ethernet, and HDMI runs on the stock `vc4` KMS driver the
image's `config.txt` asks for. **Experimental**: milestones 1–4 of the [plan](rpi3-plan.md) are
done (serial shell, Pi OS from the SD card, USB and Ethernet, HDMI); Wi-Fi, Bluetooth and the 3D
GPU are not modelled yet.

## What runs today

| | Status | Evidence |
|---|---|---|
| Raspberry Pi kernel 6.18 (`kernel8.img` from the Pi OS image) with a BusyBox initramfs | boots to a shell on the mini UART | `scripts/rpi3-shell.txt`: shell in about 5 s of host time |
| Four cores | four Cortex-A53s (MIDR `0x410fd034`) started through PSCI; each core's generic timer through the per-core interrupt controller, IPIs through its mailboxes | `smp: Brought up 1 node, 4 CPUs`; `/proc/interrupts`: `arch_timer` ticks and `IPI0` on all four cores; four busy loops in parallel |
| Board identity | revision code `a020d3` from the mailbox and in `/proc/cpuinfo`, model string `Raspberry Pi 3 Model B Plus Rev 1.3 (sbc-emu reduced DT)`, 1 GiB with 76 MiB for the VideoCore (948 MiB for Linux) | `scripts/rpi3-shell.txt` |
| Raspberry Pi OS Lite 2026-10-06 (unmodified `.img`) | firmware-style SD boot (`config.txt` with the Pi 3's sections, `cmdline.txt`, `kernel8.img`, `initramfs8`) → initramfs → ext4 root on the card → systemd 257 → `raspberrypi login:` on ttyS0 | `scripts/rpi3-raspios.txt`: login prompt in 31 s of host time on one thread (21 s of guest time, 33 G instructions; USB enumeration and NetworkManager's DHCP lease happen during that boot), 24 s with `--threads multi`, on an M-series Mac |
| First boot | the image's first-boot wizard (keyboard layout, user name, password) on tty1, answered with the USB keyboard; a login on tty1 typed with the keyboard, then on the serial console | `scripts/rpi3-firstboot.txt`: wizard, both logins and all the USB and network checks in 94 s of host time (55 s of CPU; 93.5 s of guest time, which includes the script's waits and a 10 s idle measurement) |
| More peripherals | BCM2835 RNG, thermal sensor, PM watchdog, PL011 | `/dev/hwrng` reads, `thermal_zone0` reads 45 °C, `bcm2835-wdt` binds; `reboot -f` (PSCI) boots again, `poweroff -f` stops |
| USB | the DWC2 OTG core with a high-speed root port, split transactions for full-speed devices behind the hub, NAKed bulk/interrupt IN transfers held until the device has data; behind it the LAN7515 as on a 3B+: a 4-port hub, a 3-port hub on its port 1, the LAN7800 on that hub's port 1; the keyboard and tablet on two USB-A ports | `lsusb -t` (below); typing on tty1: the first-boot wizard and a login there; `scripts/rpi3-usb.txt` |
| Ethernet | LAN7800 (`lan78xx`) with its internal PHY, gigabit link, MAC address `02:5b:c0:00:03:01` from the device tree's `local-mac-address`; on sbc-net's virtual network (DHCP, DNS, user-mode NAT, `--hostfwd`) | NetworkManager's DHCP lease, ping of the gateway (3/3), `getent hosts`, `wget` of a file from the host through the NAT (`scripts/rpi3-firstboot.txt`) |
| HDMI (`dtoverlay=vc4-kms-v3d`) | the stock `vc4` driver binds the HVS, the three pixel valves, the transposer and HDMI; `HDMI-A-1` connected with the monitor's modes from its EDID (1920×1080, 1280×720, 1024×768, 800×600); the first-boot wizard and tty1 at 1920×1080 on HDMI, over VNC and in screenshots | `scripts/rpi3-hdmi.txt`: wizard, logins and all the HDMI checks in 138 s of host time (61 s of CPU; 138 s of guest time) |
| Page flips | `kmstest --flip` at 1024×768 (65 MHz) and 1920×1080 (148.5 MHz): 60.00 fps, the frame time from the pixel valve's timings and the firmware's pixel clock | `Connector 0: fps 60.00, slowest 16.68 ms` |
| YUV planes and scaling | `kmstest` with a 320×240 NV12 or YUV420 (three-plane) frame buffer on a 768×576 plane: the pattern scaled, the colours converted with the display list's matrix | `wait-pixel` on its blue and red bars; `out/rpi3-hdmi-03-nv12.png` |
| HDMI audio | the `vc4-hdmi` ALSA card: DMA (DREQ 17) into the MAI FIFO, paced at the rate the HDMI clock and the MAI divider give; `--audio-out FILE.wav` | `speaker-test -D hdmi:CARD=vc4hdmi,DEV=0 -t sine -f 440 -c 2`: the WAV holds a 440.0 Hz tone, 3.000 s per channel at 48 kHz |
| CEC | the adapter (`/dev/cec0`, physical address 1.0.0.0 from the EDID) and a TV on the bus | `cec-ctl --playback -S`: device 0 (TV), CEC 1.4, physical address 0.0.0.0, OSD name `sbc-emu TV`, power on |
| Hot-plug | GPIO 28 (active low; the 3B's line 4 of the firmware's GPIO expander), polled by the driver; the `hdmi disconnect`/`hdmi connect` script commands | `HDMI-A-1` `disconnected`, then `connected` again |
| Interpreter, threads, snapshots, gdb | the shell script passes without the JIT (57 s) and with `--threads multi` (4 s); a snapshot (`save FILE` in a script, `--restore`) resumes with the shell's state, also onto `--threads multi`; `--gdb PORT` shows the four cores as threads | runs listed in the pull request |

## The emulated hardware

| Block | Model | Linux driver |
|---|---|---|
| CPU | 4× Cortex-A53 identity on the `arm-a64` core (ARMv8.0, as the Pi 4's A72s; no Crypto Extension), generic timer at 19.2 MHz, PSCI by the host | — |
| Per-core interrupt controller (`0x4000_0000`) | BCM2836 "QA7" block: per-core timer interrupt enables and pending registers, four 32-bit mailboxes per core (write-set, read and write-clear), GPU interrupt routing to one core, PMU routing bits | `brcm,bcm2836-l1-intc` |
| ARM-side interrupt controller (`0x3f00_b200`) | 64 GPU interrupts and 8 ARM ones, enable/disable registers, basic pending with its bank summaries and shortcut bits; its output is the per-core controller's GPU input | `brcm,bcm2836-armctrl-ic` |
| Mini UART | AUX + mini UART as a 16550: the serial console (`serial0` → `ttyS0`, as on a 3B+), on before Linux as the firmware's `enable_uart=1` leaves it | `8250_bcm2835aux` |
| UART0 | PL011 (`serial1`, numbered `ttyAMA1` by that alias); on a real 3B+ it carries Bluetooth | `amba-pl011` |
| Mailbox | VideoCore property channel (board revision, memory split, clocks, power, framebuffer) | `bcm2835-mbox`, `raspberrypi-firmware` |
| SD card | the Arasan SDHCI (`brcm,bcm2835-sdhci`, PIO, 32-bit register access) with an SDHC card on the image (copy-on-write unless `--sd-persist`) | `sdhci-iproc` |
| RNG | BCM2835 RNG: status word count, data register | `bcm2835-rng` |
| Thermal | BCM2835-family sensor: TSENSCTL set up as the firmware leaves it, TSENSSTAT code for the zone's slope and offset | `bcm2835_thermal` |
| PM | watchdog (65536 Hz; expiry resets the machine), power domains behind the AXI bridges | `bcm2835-wdt`, `bcm2835-power` |
| Firmware framebuffer | `simple-framebuffer` (1024×768) in the VideoCore's share of RAM until the KMS driver takes over (`--display fb`: the only display); `--screenshot`, `--vnc` show it (`--no-display` leaves it out) | `simplefb`, fbcon |
| HVS (`0x3f40_0000`, GPU interrupt 33) | the BCM2835-family HVS of `bcm2711/src/vc4.rs` (`Gen::Bcm2835`): display lists in its memory at 0x2000, the list latched at each frame start of the channel's pixel valve, per-channel frame counters and end-of-frame interrupts, RGB planes (8888, 565, 5551, 4444, 555, 888, 332; linear and T-tiled), YUV 4:2:0 and 4:2:2 in two or three planes with the list's colour matrix, nearest-neighbour scaling, fixed and per-pixel alpha, the gamma table; composed when the host wants a picture | `vc4` |
| Pixel valves 0–2 (`0x3f20_6000`, `0x3f20_7000`, `0x3f80_7000`) | timings, vblank (VFP start) at the mode's rate: pixel valve 2 (HDMI, HVS channel 1) from its timings and the firmware's pixel clock, the others at 60 Hz | `vc4` |
| HDMI (`0x3f90_2000`, `hd` at `0x3f80_8000`) | the BCM2835 HDMI block: reset, PHY, scheduler and FIFO handshakes that complete at once, infoframe RAM, hot-plug status; the MAI audio FIFO (DREQ 17); CEC with a TV on the bus (GPU interrupt 40) | `vc4` (`vc4_hdmi`, its CEC and audio) |
| DDC | BSC2 (`0x3f80_5000`, the BCM2835 I²C controller of `bcm2711/src/i2c.rs`) with the monitor's EDID EEPROM at 0x50 (the Pi 4's 1080p monitor, `--hdmi-mode` for others) | `i2c-bcm2835` |
| DMA controller (`0x3f00_7000`, GPU interrupts 16–27) | the Pi 4's BCM2835 DMA engine (`bcm2711/src/dma.rs`): control-block chains, 2D mode, DREQ-paced blocks fed as the peripheral asks; channels 0x7f35 for Linux | `bcm2835-dma` |
| GPIO (`0x3f20_0000`, GPU interrupts 49, 50) | the Pi 4's GPIO block as `brcm,bcm2835-gpio`: function select, levels, events; GPIO 28 is HDMI's hot-plug line | `pinctrl-bcm2835` |
| Firmware clocks, power, GPIO expander | ARM (one operating point), core, V3D, pixel and HDMI state machine clocks through the mailbox; the firmware's power domains; its GPIO expander (lines 128–135) | `clk-raspberrypi`, `raspberrypi-power`, `gpio-raspberrypi-exp` |
| USB host (`0x3f98_0000`, GPU interrupt 9) | the Pi 4's DWC2 model (`bcm2711/src/dwc2.rs`): buffer DMA through the `0xc000_0000` bus alias, a high-speed root port (frame number in microframes, SOF every 1 ms), start/complete split transactions (HCSPLT), bulk and interrupt IN transfers held while the device NAKs. `brcm,bcm2835-usb`, `dr_mode = "host"` | `dwc2` (not the vendor `dwc_otg`, which needs the FIQ) |
| Hubs | two USB 2.0 hubs (`emu-core/src/usb_hub.rs`): 0424:2514 with 4 and 3 ports, self-powered, one transaction translator; port power, reset, enable, suspend/resume, change bits, the status-change interrupt endpoint; port 1 of each marked non-removable | `hub` |
| Ethernet | LAN7800 (`emu-core/src/lan78xx.rs`, 0424:7800): registers through vendor control requests, no EEPROM and a blank OTP (the MAC comes from the device tree), the internal LAN88xx PHY at MDIO address 1 (IEEE, paged and MMD registers, 1000BASE-T full-duplex partner), bulk frames with RX command words A/B/C and the FCS, TX command words with checksum insertion, TCP segmentation and VLAN insertion done by the model, the interrupt endpoint for the PHY | `lan78xx`, `microchip` PHY (polled) |
| Keyboard, tablet | the shared USB HID keyboard and absolute pointer (`emu-core/src/usb.rs`), full speed, on hub ports 2 and 3; the script `type` command and a VNC viewer drive them | `usbhid` |

The mini UART, PL011, mailbox, SD host, watchdog, USB host, DMA controller, GPIO and I²C are the
Pi 4's models (`bcm2711`) at this SoC's addresses and interrupt numbers, and the display pipeline
is the Pi 4's `vc4.rs` in its BCM2835 mode; the hubs and the Ethernet are shared devices in
`emu-core`; the interrupt controllers, RNG, thermal sensor, device tree, USB topology and boot are
in `bcm2837/`.

`lsusb -t` on the emulated 3B+ (as on a real one, except that the root hub is `dwc2`'s):

```
/:  Bus 001.Port 001: Dev 001, Class=root_hub, Driver=dwc2/1p, 480M
    |__ Port 001: Dev 002, If 0, Class=Hub, Driver=hub/4p, 480M
        |__ Port 001: Dev 003, If 0, Class=Hub, Driver=hub/3p, 480M
            |__ Port 001: Dev 005, If 0, Class=Vendor Specific Class, Driver=lan78xx, 480M
        |__ Port 002: Dev 004, If 0, Class=Human Interface Device, Driver=usbhid, 12M
        |__ Port 003: Dev 006, If 0, Class=Human Interface Device, Driver=usbhid, 12M
```

Compatibility labels in the plan's terms:

- **boot_path**: emulated firmware (the emulator reads `config.txt`, `cmdline.txt`, `kernel8.img`,
  `initramfs8` from the FAT partition, as the Pi 3's firmware does: `[all]`, `[pi3]`, `[pi3+]` and
  `[board-type=0xd]` apply, other model sections do not; without `arm_64bit=1` the real firmware
  would start the 32-bit `kernel7.img`, which is refused). The VideoCore boot chain is not run.
- **device tree**: a *reduced* BCM2837 tree generated by the emulator, shaped like the vendor
  `bcm2710-rpi-3-b-plus.dtb` (bus addresses at 0x7e…, `dma-ranges` through 0xc000_0000) but listing
  only modelled devices, with fixed clocks for the UARTs, SD host and I²C, firmware clocks for the
  display. `dtoverlay=vc4-kms-v3d` adds what the overlay enables (the HVS, pixel valves, HDMI, its
  DDC bus, the transposer, 256 MiB of CMA); other `dtoverlay`/`dtparam` lines are reported and
  ignored.
- **vendor_image_compatibility**: the stock Raspberry Pi OS Lite image boots unmodified (writes go
  to a copy-on-write overlay).
- **tested_cpu_count**: 4 (`--smp 1..4`).

Where the model differs from a real 3B+:

- The SD card is on the Arasan host; a real Pi 3 has it on the `sdhost` controller
  (`brcm,bcm2835-sdhost`) and the Arasan drives the Wi-Fi chip. The card moves to `sdhost` when
  Wi-Fi arrives (milestone 5).
- Secondary cores start through PSCI (`enable-method = "psci"`), not the vendor tree's spin table;
  Linux at EL1, as on the Pi 4 machine.
- HDMI: the HVS composes the picture when the host asks for one (a screenshot, a VNC update),
  not line by line; its line counter reads 0, underruns never happen, the transposer (writeback)
  writes nothing. Scaling is nearest-neighbour (the hardware filters). The HDMI handshakes complete
  at once; the encoder sends no TMDS anyone checks. Audio leaves as 16-bit stereo: channels beyond
  the first two are dropped. The CEC bus has the TV only, which acknowledges what is sent to it and
  answers the common queries (and Feature Abort for the rest); messages take 2 ms.
- No FIQs (Linux on arm64 does not use them), the ARM timer, the local timer and the PMU interrupt
  are registers without a source.
- USB runs on the upstream `dwc2` driver (`brcm,bcm2835-usb`), not the vendor tree's `dwc_otg`
  with its FIQ handler. The model raises SOF once per millisecond and counts 8 microframes per
  SOF (a real high-speed port has a SOF every 125 µs); `dwc2` schedules its periodic transfers on
  that coarser clock. Idle, the guest takes about 1270 USB interrupts per second (the SOFs plus
  the keyboard's and tablet's split polls every 10 ms), about 15 M instructions per second of
  guest time.
- The hubs, keyboard and tablet carry the sbc-emu strings; the hubs and the Ethernet use
  Microchip's USB IDs (0424:2514, 0424:7800) so that the stock drivers bind and `lsusb` names
  them as on a real board. The MAC address is a locally administered one.

## Running it

```sh
cargo build --release
# Raspberry Pi OS (scripts/raspios-fixture.sh downloads and checks it): boots like the firmware
./target/release/sbc-emu run --board pi3b --sd cache/os/raspios-lite.img
./target/release/sbc-emu run --board pi3b --sd cache/os/raspios-lite.img --script scripts/rpi3-raspios.txt --timeout 600
# first boot with the USB keyboard on tty1, then the USB and network checks (the HTTP fetch
# wants a server on the host, see the script)
python3 -m http.server 8731 --bind 127.0.0.1 --directory scripts &
./target/release/sbc-emu run --board pi3b --sd cache/os/raspios-lite.img --script scripts/rpi3-firstboot.txt --timeout 900
# HDMI: the wizard on the KMS console, the connector, page flips, YUV planes, audio, CEC, hot-plug
./target/release/sbc-emu run --board pi3b --sd cache/os/raspios-lite.img --audio-out out/rpi3-hdmi.wav \
    --script scripts/rpi3-hdmi.txt --timeout 900

# the Pi kernel with a BusyBox initramfs (console on ttyS0)
mkdir -p cache/rpi3
./target/release/sbc-emu sd cat cache/os/raspios-lite.img kernel8.img | gunzip > cache/rpi3/Image
./target/release/sbc-emu mkinitramfs --board pi3b --busybox busybox.static -o cache/rpi3/initramfs.cpio
./target/release/sbc-emu run --board pi3b --kernel cache/rpi3/Image --initrd cache/rpi3/initramfs.cpio \
    --script scripts/rpi3-shell.txt --timeout 120
./target/release/sbc-emu run --board pi3b --kernel cache/rpi3/Image --initrd cache/rpi3/initramfs.cpio \
    --script scripts/rpi3-usb.txt --timeout 180
```

Options: `--model 3b+|3b|3a+|zero2w` (default `3b+`; the others change the RAM size, revision
code, model string, `config.txt` sections and USB topology: the 3B gets a 5-port hub without its
Ethernet, the 3A+ and Zero 2 W the keyboard on the root port; they are untested), `--smp N`,
`--threads single|multi`, `--ram-mb N`, `--seed N`, `--no-jit`, `--no-display`, `--sd-persist`,
`--net nat|local|none` (default `nat`), `--display kms|fb` (default: `kms` when `config.txt` has
`dtoverlay=vc4-kms-v3d`, as the image's does; `fb` for `--kernel`), `--hdmi-mode WxH` (the
monitor's preferred mode: 1920x1080, 1280x720, 1024x768, 800x600 or 640x480), `--audio-out FILE`
(HDMI audio: a `.wav`, or raw 16-bit stereo for any other path), `--hostfwd tcp:[HOSTADDR]:HOSTPORT-[GUESTADDR]:GUESTPORT`,
`--gdb PORT [--gdb-wait]`, `--restore FILE`, `--screenshot FILE`, `--vnc :N` (the viewer's keys
and pointer go to the USB keyboard and tablet), `--script FILE`, `--timeout SECS`. Scripts take
`wait-pixel X Y RRGGBB` (the screen shows that colour), `wait-idle SECS` (every core has waited
that long in wall time) and `hdmi connect|disconnect` besides the commands of the other boards. Ctrl-A x quits,
Ctrl-A s writes a snapshot. `SBC_SD_TRACE=1` traces the SD host, `SBC_USB_TRACE=1` the USB
transfers, `SBC_NET_LOG=1` the virtual network, `SBC_TRACE_VC4=1` the display registers,
`SBC_CEC_LOG=1` the CEC messages (at the end of the run).

## What does not work yet

- USB: no USB stick or camera option on the Pi 3 yet (the shared devices exist; nothing plugs
  them in), no isochronous or low-speed devices, no hub over-current or port-power faults. The
  3B's LAN9514 Ethernet (`smsc95xx`) is not modelled.
- Ethernet: the statistics counters (`ethtool -S`) read zero, wake-on-LAN and EEE do nothing,
  the receive filters (perfect, hash, VLAN) are not applied: every frame from the network is
  delivered.
- No VideoCore IV 3D (the device tree's V3D node waits for its model), no composite output (VEC),
  no DSI or DPI displays, no video decode.
- No Wi-Fi or Bluetooth (the PL011 is a plain serial port), no `sdhost`.
- No I²C on the header, no SPI, PCM/I²S or system timer in the device tree; the GPIO block has no
  pull-up/down state (`GPPUD`) and nothing on the header pins.
- 32-bit kernels (`kernel7.img`, `arm_64bit=0`) are not supported.
- Not in the browser build yet.
