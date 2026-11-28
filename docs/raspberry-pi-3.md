# Raspberry Pi 3 Model B+ (and 3B, 3A+, Zero 2 W)

An instruction-level BCM2837 that boots the unmodified **Raspberry Pi OS Lite (trixie, arm64)
SD-card image**, the same image the [Raspberry Pi 4](raspberry-pi-4.md) boots, on four Cortex-A53
cores: its first-boot wizard runs on the framebuffer console with the USB keyboard, the
network works over the LAN7515's USB Ethernet and over Wi-Fi, Bluetooth finds a virtual LE
sensor, and HDMI and composite video run on the stock `vc4` KMS driver the image's `config.txt`
asks for, with the VideoCore IV 3D GPU behind Mesa's `vc4` driver ([its page](raspberry-pi-3-gpu.md)).
**Experimental**: milestones 1–8 of the [plan](rpi3-plan.md) are done (serial shell, Pi OS from
the SD card, USB and Ethernet, HDMI, Wi-Fi and Bluetooth, the rest of the platform: the 3B, 3A+
and Zero 2 W, the browser page, snapshots; the 3D GPU, composite video), and of milestone 9 everything but the H.264 decoder: VCHIQ, the firmware's MMAL media
components behind `bcm2835-codec` and `bcm2835-isp`, and its audio service behind the headphone
card.

## What runs today

| | Status | Evidence |
|---|---|---|
| Raspberry Pi kernel 6.18 (`kernel8.img` from the Pi OS image) with a BusyBox initramfs | boots to a shell on the mini UART | `scripts/rpi3-shell.txt`: shell in about 5 s of host time |
| Four cores | four Cortex-A53s (MIDR `0x410fd034`) started through PSCI; each core's generic timer through the per-core interrupt controller, IPIs through its mailboxes | `smp: Brought up 1 node, 4 CPUs`; `/proc/interrupts`: `arch_timer` ticks and `IPI0` on all four cores; four busy loops in parallel |
| Board identity | revision code `a020d3` from the mailbox and in `/proc/cpuinfo`, model string `Raspberry Pi 3 Model B Plus Rev 1.3 (sbc-emu reduced DT)`, 1 GiB with 76 MiB for the VideoCore (948 MiB for Linux) | `scripts/rpi3-shell.txt` |
| Raspberry Pi OS Lite 2026-10-06 (unmodified `.img`) | firmware-style SD boot (`config.txt` with the Pi 3's sections, `cmdline.txt`, `kernel8.img`, `initramfs8`) → initramfs → ext4 root on the card (on `sdhost`, as on a real Pi 3) → systemd 257 → `raspberrypi login:` on ttyS0 | `scripts/rpi3-raspios.txt`: login prompt in 30 s of host time on one thread (20 s of guest time, 28 G instructions, 69 k interrupts; USB enumeration, NetworkManager's DHCP lease, the HDMI console, the Wi-Fi firmware download and Bluetooth's patch RAM happen during that boot), 23 s with `--threads multi`, on an M-series Mac shared with other jobs |
| First boot | the image's first-boot wizard (keyboard layout, user name, password) on tty1, answered with the USB keyboard; a login on tty1 typed with the keyboard, then on the serial console | `scripts/rpi3-firstboot.txt`: wizard, both logins and all the USB and network checks in 94 s of host time (55 s of CPU; 93.5 s of guest time, which includes the script's waits and a 10 s idle measurement) |
| Wi-Fi | the CYW43455 on the Arasan SDHCI (SDIO), the Pi 4's model: `brcmfmac` downloads the image's own `brcmfmac43455-sdio.bin` and the 3B+'s NVRAM, then talks to the emulated FullMAC firmware; NetworkManager scans, joins the open `sbc-emu` and the WPA2-PSK `sbc-emu-wpa2` (passphrase `raspberry-pi`) access points, DHCP and ping over `wlan0` | `scripts/rpi3-wireless.txt`: the first-boot wizard, Wi-Fi and Bluetooth (an 8 s LE scan) in 101 s of host time (101 s of guest time, mostly the script's waits) |
| Bluetooth | the CYW43455's HCI controller on UART0 (H4), `hci_bcm` over serdev as the vendor tree has it: Broadcom vendor commands, the image's `BCM4345C0.raspberrypi,3-model-b-plus.hcd` patch RAM; a virtual BLE peripheral "sbc-emu sensor" nearby | `bluetoothctl` powers `hci0` on, an LE scan finds `sbc-emu sensor` (`scripts/rpi3-wireless.txt`) |
| More peripherals | BCM2835 RNG, thermal sensor, PM watchdog, PL011 | `/dev/hwrng` reads, `thermal_zone0` reads 45 °C, `bcm2835-wdt` binds; `reboot -f` (PSCI) boots again, `poweroff -f` stops |
| USB | the DWC2 OTG core with a high-speed root port, split transactions for full-speed devices behind the hub, NAKed bulk/interrupt IN transfers held until the device has data (an interrupt IN at most 0.5 s, then NAKed, so the driver's periodic schedule stays current); behind it the LAN7515 as on a 3B+: a 4-port hub, a 3-port hub on its port 1, the LAN7800 on that hub's port 1; the keyboard and tablet on two USB-A ports | `lsusb -t` (below); typing on tty1: the first-boot wizard and a login there; `scripts/rpi3-usb.txt` |
| Ethernet | LAN7800 (`lan78xx`) with its internal PHY, gigabit link, MAC address `02:5b:c0:00:03:01` from the device tree's `local-mac-address`; on sbc-net's virtual network (DHCP, DNS, user-mode NAT, `--hostfwd`) | NetworkManager's DHCP lease, ping of the gateway (3/3), `getent hosts`, `wget` of a file from the host through the NAT (`scripts/rpi3-firstboot.txt`) |
| HDMI (`dtoverlay=vc4-kms-v3d`) | the stock `vc4` driver binds the HVS, the three pixel valves, the transposer and HDMI; `HDMI-A-1` connected with the monitor's modes from its EDID (1920×1080, 1280×720, 1024×768, 800×600); the first-boot wizard and tty1 at 1920×1080 on HDMI, over VNC and in screenshots | `scripts/rpi3-hdmi.txt`: wizard, logins and all the HDMI checks in 138 s of host time (61 s of CPU; 138 s of guest time) |
| Page flips | `kmstest --flip` at 1024×768 (65 MHz) and 1920×1080 (148.5 MHz): 60.00 fps, the frame time from the pixel valve's timings and the firmware's pixel clock | `Connector 0: fps 60.00, slowest 16.68 ms` |
| YUV planes and scaling | `kmstest` with a 320×240 NV12 or YUV420 (three-plane) frame buffer on a 768×576 plane: the pattern scaled, the colours converted with the display list's matrix | `wait-pixel` on its blue and red bars; `out/rpi3-hdmi-03-nv12.png` |
| HDMI audio | the `vc4-hdmi` ALSA card: DMA (DREQ 17) into the MAI FIFO, paced at the rate the HDMI clock and the MAI divider give; `--audio-out FILE.wav` | `speaker-test -D hdmi:CARD=vc4hdmi,DEV=0 -t sine -f 440 -c 2`: the WAV holds a 440.0 Hz tone, 3.000 s per channel at 48 kHz |
| CEC | the adapter (`/dev/cec0`, physical address 1.0.0.0 from the EDID) and a TV on the bus | `cec-ctl --playback -S`: device 0 (TV), CEC 1.4, physical address 0.0.0.0, OSD name `sbc-emu TV`, power on |
| Composite video (`dtoverlay=vc4-kms-v3d,composite`) | the VEC on pixel valve 2's other output: `Composite-1` (status `unknown`, as on a real Pi: composite has no load detection) next to `HDMI-A-1`, with the driver's TV standards (NTSC, NTSC-J, NTSC-443, PAL, PAL-M, PAL-N, PAL-60, SECAM, monochrome); `sdtv_mode` becomes the firmware's `video=Composite-1:…` option; the console on composite when no monitor is on HDMI; the 720×576 (PAL) or 720×480 (NTSC) frame over VNC and in screenshots | `scripts/rpi3-composite.txt`: `kmsprint` lists `Composite-1`; PAL: `720x576i@50.00`, `VEC_CONFIG0 = 0x00000001`, `kmstest --flip` at 50.00 fps, its pattern on screen; NTSC: `720x480i@59.94`, `VEC_CONFIG0 = 0x00000040`, 59.94 fps |
| VCHIQ (`brcm,bcm2836-vchiq`) | the VideoCore's side of the message queue: `VCHIQ_INIT` through the mailbox, the slot protocol (messages, slot recycling both ways, events and both doorbells), services opened by four-character code, bulk transfers through page lists (with the fragments for unaligned receives); `vchiq`, `vc-sm-cma`, `bcm2835-mmal-vchiq`, `bcm2835-codec`, `bcm2835-isp` and `snd-bcm2835` bind on the unmodified image | `scripts/rpi3-vchiq.txt`: the drivers' probe messages, no failures in `dmesg`; the slot protocol and MMAL messages also in unit tests (`cargo test -p bcm2837`) |
| Media devices (`bcm2835-codec`, `bcm2835-isp`) | `v4l2-ctl --list-devices`: `bcm2835-codec-decode` with `/dev/video10` (decode), 11 (encode), 12 (ISP), 18 (image effects), 31 (JPEG encode), two `bcm2835-isp` instances (`/dev/video13`–`16`, `20`–`23`), `/dev/vcsm-cma`; the decoder's input formats are H.264 only (no MPEG-2 or VC-1: no licence key) | `scripts/rpi3-vchiq.txt` |
| Video decode | **the H.264 decoder itself is pending.** The rest of the path works with a stand-in (`video::TestPatternDecoder`): it reads the stream's sequence parameter set for the picture size and turns every coded picture into colour bars, so `v4l2-ctl` streams H.264 into `/dev/video10` and YU12 pictures out, with the firmware's format-changed event (V4L2's source change) and end of stream | `scripts/rpi3-vchiq.txt`: a 320×240 stream of parameter sets and four empty slices in, four 320×240 YU12 pictures of the bars out (460800 bytes, first luma byte 180), then the end of stream (`EOS EVENT`) |
| ISP | `ril.isp`: YUV 4:2:0 (I420, YV12, NV12, NV21), 4:2:2 packed and RGB (RGB24, BGR24, RGBA, BGRA, RGB565) in and out, bilinear scaling | `v4l2-ctl` on `/dev/video12`: a red 64×32 RGB24 picture to a 32×16 YU12 one, Y = 82 (BT.601 red) |
| Headphone audio (`dtparam=audio=on`) | the firmware's audio service (`AUDS`): the `bcm2835 Headphones` card; the data played at the configured rate in guest time, each write reported back once played; `--audio-out FILE.wav` | `speaker-test -D plughw:CARD=Headphones,DEV=0 -t sine -f 440 -c 2 -r 48000`: the WAV holds a 440 Hz tone, 3.000 s per channel at 48 kHz |
| Hot-plug | GPIO 28 (active low; the 3B's line 4 of the firmware's GPIO expander), polled by the driver; the `hdmi disconnect`/`hdmi connect` script commands | `HDMI-A-1` `disconnected`, then `connected` again |
| Interpreter, threads, snapshots, gdb | the shell script passes without the JIT (57 s) and with `--threads multi` (4 s); a snapshot (`save FILE` in a script, `--restore`) resumes with the shell's state, also onto `--threads multi`; `--gdb PORT` shows the four cores as threads | runs listed in the pull request |
| The other models (`--model 3b`, `3a+`, `zero2w`) | the unmodified Pi OS image on each: the revision code, model string, RAM and `config.txt` sections of the board; the 3B's LAN9514 (5-port hub, 100 Mb/s Ethernet on `smsc95xx`), the 3A+'s and Zero 2 W's single USB port with the keyboard straight on it; Wi-Fi and Bluetooth on the 3A+ | `scripts/rpi3-model-3b.txt`, `rpi3-model-3aplus.txt`, `rpi3-model-zero2w.txt`: first-boot wizard, login, `a02082` / `9020e0` / `902120`, 905 / 414 / 414 MiB `MemTotal`, `lsusb -t`; 3B: `smsc95xx` at 100 Mb/s, DHCP lease, ping 3/3; 3A+: `wlan0` and `hci0`; Zero 2 W: no network interface. Each run (wizard, logins, checks, snapshot) takes 89–94 s of host time. |
| Boot time against the Pi 4 | Pi OS Lite to the serial login prompt, the same image, on an M-series Mac shared with other jobs | one host thread: Pi 4 33.9 s (20.8 s of guest time, 28.4 G instructions); 3B+ 35.0 s, 3B 32.7 s, 3A+ 34.8 s, Zero 2 W 35.8 s (20.3–20.5 s of guest time, 27.1–28.1 G instructions): the same work as the Pi 4's at the same speed. With `--threads multi` the earlier runs on a quieter host gave 23 s for the 3B+ (`scripts/rpi3-raspios.txt`, against 33 s on one thread) and the Pi 4 is in the same range; on a host loaded by other jobs (load average 35–63 on 18 cores) the threaded runs took 26–69 s, as the guest's clock follows the wall clock there |
| Snapshot round trip | a snapshot saved after the first boot (`save` in a script) resumes with `--restore` on the image opened copy-on-write again: the sectors the first boot wrote come with it (the card's overlay is in the snapshot), new writes read back past the page cache, the USB keyboard and the network work | `scripts/rpi3-restore.txt` on the snapshots of `rpi3-firstboot.txt` (3B+) and the three model scripts; a unit test (`snapshots_carry_the_cards_written_sectors`) |
| In the browser (`web/rpi3.html`) | the same machine as WebAssembly with the `arm-a64` JIT to WebAssembly: the four models; the Pi kernel with a BusyBox initramfs (the shell on the serial console, the framebuffer console on screen), or the visitor's own Pi OS card (`.img`, `.img.xz`, `.img.gz`, read where it is) booted like the firmware, HDMI through `vc4` KMS to a 1280×720 monitor, the USB keyboard and tablet, HDMI audio | `tools/wasm-smoke-rpi3.mjs`: the BusyBox shell and four cores busy in 4.0 s (415 MIPS); Pi OS Lite to `raspberrypi login:` in 202 s (29.7 G instructions, 147 MIPS, 219 MiB of the card read) with the first-boot wizard on the 720p screen; the page itself in headless Chrome (the 3B's shell answering on the serial console) |

## The emulated hardware

| Block | Model | Linux driver |
|---|---|---|
| CPU | 4× Cortex-A53 identity on the `arm-a64` core (ARMv8.0, as the Pi 4's A72s; no Crypto Extension), generic timer at 19.2 MHz, PSCI by the host | — |
| Per-core interrupt controller (`0x4000_0000`) | BCM2836 "QA7" block: per-core timer interrupt enables and pending registers, four 32-bit mailboxes per core (write-set, read and write-clear), GPU interrupt routing to one core, PMU routing bits | `brcm,bcm2836-l1-intc` |
| ARM-side interrupt controller (`0x3f00_b200`) | 64 GPU interrupts and 8 ARM ones, enable/disable registers, basic pending with its bank summaries and shortcut bits; its output is the per-core controller's GPU input | `brcm,bcm2836-armctrl-ic` |
| Mini UART | AUX + mini UART as a 16550: the serial console (`serial0` → `ttyS0`, as on a 3B+), on before Linux as the firmware's `enable_uart=1` leaves it | `8250_bcm2835aux` |
| UART0 | PL011 with the CYW43455's Bluetooth controller on it (H4, the vendor tree's `bluetooth` child, `shutdown-gpios` on the expander's BT_ON); `--no-bluetooth` leaves it a plain serial port. Linux numbers it `ttyAMA1` after its `serial1` alias (the vendor tree's aliases); while Bluetooth holds it, it is a serdev port without a tty | `amba-pl011`, `hci_uart` (`hci_bcm`, `btbcm`), BlueZ |
| Mailbox | VideoCore property channel (board revision, memory split, clocks, power, framebuffer) | `bcm2835-mbox`, `raspberrypi-firmware` |
| SD card (`0x3f20_2000`, GPU interrupt 56) | the BCM2835 `sdhost` controller (`bcm2837/src/sdhost.rs`): command register with its NEW/FAIL flags, host status, the data FSM and FIFO level in SDEDM, block size/count, the 16-word FIFO, by DMA on DREQ 13 or by PIO; data, block and busy interrupts. The SDHC card is the Arasan model's (copy-on-write unless `--sd-persist`); `mmc0`, `mmcblk0` | `sdhost-bcm2835` (DMA for more than one block, as the vendor tree's `brcm,pio-limit` says) |
| Arasan SDHCI (`0x3f30_0000`, GPU interrupt 62) | the Pi 4's SDHCI model as the BCM2835's "mmcnr" (`brcm,bcm2835-sdhci`, 32-bit register access, PIO), with the CYW43455's SDIO functions behind it while WL_ON powers the chip (`mmc1`) | `sdhci-iproc`, `brcmfmac` |
| GPIO expander | the firmware's eight-line expander (lines 128–135) through the mailbox's GPIO property tags (GET/SET_GPIO_STATE, GET/SET_GPIO_CONFIG); on the 3B its line 4 is HDMI's hot-plug; BT_ON powers the Bluetooth core (off: it is reset and hears nothing), WL_ON the Wi-Fi chip (on at boot as the firmware leaves it; off, the chip leaves the SDIO bus) | `gpio-raspberrypi-exp` |
| RNG | BCM2835 RNG: status word count, data register | `bcm2835-rng` |
| Thermal | BCM2835-family sensor: TSENSCTL set up as the firmware leaves it, TSENSSTAT code for the zone's slope and offset | `bcm2835_thermal` |
| PM | watchdog (65536 Hz; expiry resets the machine), power domains behind the AXI bridges | `bcm2835-wdt`, `bcm2835-power` |
| Firmware framebuffer | `simple-framebuffer` (1024×768) in the VideoCore's share of RAM until the KMS driver takes over (`--display fb`: the only display); `--screenshot`, `--vnc` show it (`--no-display` leaves it out) | `simplefb`, fbcon |
| HVS (`0x3f40_0000`, GPU interrupt 33) | the BCM2835-family HVS of `bcm2711/src/vc4.rs` (`Gen::Bcm2835`): display lists in its memory at 0x2000, the list latched at each frame start of the channel's pixel valve, per-channel frame counters and end-of-frame interrupts, RGB planes (8888, 565, 5551, 4444, 555, 888, 332; linear and T-tiled), YUV 4:2:0 and 4:2:2 in two or three planes with the list's colour matrix, nearest-neighbour scaling, fixed and per-pixel alpha, the gamma table; composed when the host wants a picture | `vc4` |
| Pixel valves 0–2 (`0x3f20_6000`, `0x3f20_7000`, `0x3f80_7000`) | timings, vblank (VFP start) at the mode's rate: pixel valve 2 (HDMI, HVS channel 1) from its timings and the firmware's pixel clock, the others at 60 Hz | `vc4` |
| HDMI (`0x3f90_2000`, `hd` at `0x3f80_8000`) | the BCM2835 HDMI block: reset, PHY, scheduler and FIFO handshakes that complete at once, infoframe RAM, hot-plug status; the MAI audio FIFO (DREQ 17); CEC with a TV on the bus (GPU interrupt 40) | `vc4` (`vc4_hdmi`, its CEC and audio) |
| VEC (`0x3f80_6000`) | the composite encoder: the registers `vc4_vec.c` programs (TV standard, subcarrier, DAC), resets that complete at once; it encodes while VEC_CFG enables it and pixel valve 2 selects its clock; fields at the VEC clock / 8 (13.5 MHz): 50 a second for 625 lines, 59.94 for 525; the host gets the decoded frame (both fields; luma only in the monochrome standard), not a signal. Listed in the device tree with `dtoverlay=vc4-kms-v3d,composite`, as the overlay's dormant fragment does | `vc4` (`vc4_vec`) |
| DDC | BSC2 (`0x3f80_5000`, the BCM2835 I²C controller of `bcm2711/src/i2c.rs`) with the monitor's EDID EEPROM at 0x50 (the Pi 4's 1080p monitor, `--hdmi-mode` for others) | `i2c-bcm2835` |
| DMA controller (`0x3f00_7000`, GPU interrupts 16–27) | the Pi 4's BCM2835 DMA engine (`bcm2711/src/dma.rs`): control-block chains, 2D mode, DREQ-paced blocks fed as the peripheral asks (HDMI audio, DREQ 17) or as far as its FIFO allows (sdhost, DREQ 13); channels 0x7f35 for Linux | `bcm2835-dma` |
| GPIO (`0x3f20_0000`, GPU interrupts 49, 50) | the Pi 4's GPIO block as `brcm,bcm2835-gpio`: function select, levels, events; the vendor tree's line names; GPIO 28 is HDMI's hot-plug line; the pin groups the SD hosts and UART0 select (`sdhost_gpio48`, `sdio_pins`, `uart0_pins`, `bt_pins`) | `pinctrl-bcm2835` |
| Firmware clocks, power, GPIO expander | ARM (one operating point), core, V3D, pixel, HDMI state machine and VEC (108 MHz) clocks through the mailbox; the firmware's power domains; its GPIO expander (lines 128–135) | `clk-raspberrypi`, `raspberrypi-power`, `gpio-raspberrypi-exp` |
| USB host (`0x3f98_0000`, GPU interrupt 9) | the Pi 4's DWC2 model (`bcm2711/src/dwc2.rs`): buffer DMA through the `0xc000_0000` bus alias, a high-speed root port (frame number in microframes, SOF every 1 ms), start/complete split transactions (HCSPLT), bulk and interrupt IN transfers held while the device NAKs. `brcm,bcm2835-usb`, `dr_mode = "host"` | `dwc2` (not the vendor `dwc_otg`, which needs the FIQ) |
| Hubs | two USB 2.0 hubs (`emu-core/src/usb_hub.rs`): 0424:2514 with 4 and 3 ports, self-powered, one transaction translator; port power, reset, enable, suspend/resume, change bits, the status-change interrupt endpoint; port 1 of each marked non-removable | `hub` |
| Ethernet | LAN7800 (`emu-core/src/lan78xx.rs`, 0424:7800): registers through vendor control requests, no EEPROM and a blank OTP (the MAC comes from the device tree), the internal LAN88xx PHY at MDIO address 1 (IEEE, paged and MMD registers, 1000BASE-T full-duplex partner), bulk frames with RX command words A/B/C and the FCS, TX command words with checksum insertion, TCP segmentation and VLAN insertion done by the model, the interrupt endpoint for the PHY | `lan78xx`, `microchip` PHY (polled) |
| Ethernet (3B) | the LAN9514's LAN9500 (`emu-core/src/smsc95xx.rs`, 0424:ec00) behind its 5-port hub (0424:9514): registers through vendor control requests, no EEPROM (the MAC comes from the device tree), the internal 10/100 PHY at MDIO address 1 (ID 0007:c0c3, Linux's "SMSC LAN8700"; a 100BASE-TX full-duplex partner, its interrupt flags and mask), bulk frames behind the RX status word with the RX data offset, the FCS and the receive checksum (COE), several per transfer with HW_CFG.MEF; TX command words with the transmit checksum preamble; the PHY interrupt on the interrupt endpoint | `smsc95xx`, `smsc` PHY (interrupt-driven) |
| VCHIQ doorbells (`0x3f00_b840`, ARM interrupt 2) | the VideoCore's side of VCHIQ (`bcm2837/src/vchiq.rs`): Linux's slot memory read and written as the VideoCore firmware does, BELL2 (Linux rings the VideoCore) handled at once, BELL0 (the VideoCore rings Linux) set with each event it signals and cleared by the read; services `mmal`, `SMEM` and `AUDS`, any other refused (`CLOSE`) | `vchiq` |
| VideoCore shared memory (`SMEM`) | imports of Linux's buffers (bus address, size) answered with a handle; MMAL buffers then pass by handle and the model reads and writes guest RAM directly; frees dropped (the firmware's version message is not sent, so the kernel does not wait for releases) | `vc-sm-cma` |
| MMAL (`mmal`) | `bcm2837/src/mmal.rs`: components `ril.video_decode` (H.264 in; I420, YV12, NV12, NV21, RGB565 out), `ril.video_encode` and `ril.image_encode` (formats and buffers, no encoder: empty output buffers), `ril.isp` (one input, two image outputs, statistics), `ril.image_fx` (pictures pass through), `camera_info` (no camera); ports with formats, buffer sizes, parameters (`SUPPORTED_ENCODINGS`, `ZERO_COPY`, the rest stored), enable, disable and flush (buffers returned), buffers in both directions (by handle, or as bulk data), the `EFCH` format-changed event | `bcm2835-mmal-vchiq`, `bcm2835-codec`, `bcm2835-isp`, `bcm2835-v4l2` (finds no camera) |
| Decoder interface | `video::VideoDecoder` (`push` stream bytes with a timestamp, `flush` at end of stream, `pop` pictures in display order, `reset`); `video::decoder_for` picks one by MMAL encoding. Today the stand-in `TestPatternDecoder` | — |
| Audio service (`AUDS`, version 2) | `bcm2837/src/vcaudio.rs`: `CONFIG`, `CONTROL` (volume), `OPEN`/`CLOSE`, `START`/`STOP` (with draining), `WRITE` with its data in messages or a bulk transfer; 8-, 16-, 24- and 32-bit samples, 1–8 channels (the first two played); `COMPLETE` per write once its frames have played | `snd-bcm2835` |
| Keyboard, tablet | the shared USB HID keyboard and absolute pointer (`emu-core/src/usb.rs`), full speed, on hub ports 2 and 3; the script `type` command and a VNC viewer drive them | `usbhid` |

The mini UART, PL011, mailbox, Arasan SD host, watchdog, USB host, DMA controller, GPIO, I²C and
the CYW43455's Wi-Fi and Bluetooth are the Pi 4's models (`bcm2711`) at this SoC's addresses and
interrupt numbers, and the display pipeline is the Pi 4's `vc4.rs` in its BCM2835 mode; the hubs
and the Ethernet are shared devices in `emu-core`; the interrupt controllers, `sdhost`, RNG,
thermal sensor, device tree, USB topology, boot and the VideoCore firmware's services over VCHIQ
are in `bcm2837/`.

`lsusb -t` on the emulated 3B+ (as on a real one, except that the root hub is `dwc2`'s):

```
/:  Bus 001.Port 001: Dev 001, Class=root_hub, Driver=dwc2/1p, 480M
    |__ Port 001: Dev 002, If 0, Class=Hub, Driver=hub/4p, 480M
        |__ Port 001: Dev 003, If 0, Class=Hub, Driver=hub/3p, 480M
            |__ Port 001: Dev 005, If 0, Class=Vendor Specific Class, Driver=lan78xx, 480M
        |__ Port 002: Dev 004, If 0, Class=Human Interface Device, Driver=usbhid, 12M
        |__ Port 003: Dev 006, If 0, Class=Human Interface Device, Driver=usbhid, 12M
```

On the 3B (`--model 3b`), the LAN9514:

```
/:  Bus 001.Port 001: Dev 001, Class=root_hub, Driver=dwc2/1p, 480M
    |__ Port 001: Dev 002, If 0, Class=Hub, Driver=hub/5p, 480M
        |__ Port 001: Dev 003, If 0, Class=Vendor Specific Class, Driver=smsc95xx, 480M
        |__ Port 002: Dev 004, If 0, Class=Human Interface Device, Driver=usbhid, 12M
        |__ Port 003: Dev 005, If 0, Class=Human Interface Device, Driver=usbhid, 12M
```

On the 3A+ and Zero 2 W the keyboard is the root port's only device (`Port 001: Dev 002, If 0,
Class=Human Interface Device, Driver=usbhid, 12M`).

Compatibility labels in the plan's terms:

- **boot_path**: emulated firmware (the emulator reads `config.txt`, `cmdline.txt`, `kernel8.img`,
  `initramfs8` from the FAT partition, as the Pi 3's firmware does: `[all]`, `[pi3]`, `[pi3+]` and
  `[board-type=0xd]` apply, other model sections do not; without `arm_64bit=1` the real firmware
  would start the 32-bit `kernel7.img`, which is refused). The VideoCore boot chain is not run.
- **device tree**: a *reduced* BCM2837 tree generated by the emulator, shaped like the vendor
  `bcm2710-rpi-3-b-plus.dtb` (bus addresses at 0x7e…, `dma-ranges` through 0xc000_0000) but listing
  only modelled devices, with fixed clocks for the UARTs, SD host and I²C, firmware clocks for the
  display. `dtoverlay=vc4-kms-v3d` adds what the overlay enables (the HVS, pixel valves, HDMI, its
  DDC bus, the transposer, 256 MiB of CMA), and its `composite` parameter the VEC (`nohdmi` leaves
  HDMI and its DDC bus out); the VCHIQ node is always there (the firmware always runs).
  `dtparam=audio=on` (in the stock `config.txt`) becomes the firmware's
  `snd_bcm2835.enable_headphones=1 snd_bcm2835.enable_hdmi=1` at the front of the command line,
  and `vc4-kms-v3d` adds `snd_bcm2835.enable_hdmi=0`, as on a real Pi 3; other
  `dtoverlay`/`dtparam` lines are reported and ignored.
- **composite and `config.txt`**: with the VEC listed, the emulated firmware writes
  `video=Composite-1:720x576@50i` (`sdtv_mode=2`; `720x480@60i` for 0, 1 and 3, `720x288@50` and
  `720x240@60` for 18 and 16) into the command line, the format string the Pi 3's `start.elf`
  carries, unless `disable_fw_kms_setup=1` (as in the stock `config.txt`) or `enable_tvout=0`.
  The kernel then defaults the TV standard to PAL for a 50 Hz `video=` mode and to NTSC
  otherwise (`vc4.tv_norm=` overrides it). The firmware adds no `vc4.tv_norm` itself.
- **vendor_image_compatibility**: the stock Raspberry Pi OS Lite image boots unmodified (writes go
  to a copy-on-write overlay).
- **tested_cpu_count**: 4 (`--smp 1..4`).

Where the model differs from a real 3B+:

- `sdhost` transfers take no time: a read's blocks are in the FIFO as the command completes, and
  a DMA channel on DREQ 13 moves them all as soon as it is started. The card's SCR offers CMD23,
  so reads go as CMD23 + CMD18 without a CMD12.
- The Wi-Fi chip runs no firmware: `brcmfmac` downloads the real one, then the emulated FullMAC
  answers (see the [Pi 4](raspberry-pi-4.md)'s Wi-Fi); no radio, the access points' network is
  isolated (DHCP, DNS and the host's services, no NAT). As on a real board, Pi OS keeps Wi-Fi
  and Bluetooth rfkill-blocked on a first boot until the country is set (`rfkill unblock`).
- Secondary cores start through PSCI (`enable-method = "psci"`), not the vendor tree's spin table;
  Linux at EL1, as on the Pi 4 machine.
- HDMI and composite share pixel valve 2 (HVS channel 1), as on the SoC: one of them shows a
  picture at a time. With a monitor on HDMI the console stays there and `kmstest -c Composite-1`
  fails its atomic test (the CRTC is HDMI's); unplugged (`hdmi disconnect`), the console moves to
  composite.
- Composite: no analogue signal; the WSS/VPS inserter, colour-bar generator and interrupt are
  registers without an effect, and `enable_tvout=0` only drops the firmware's `video=` option.
  The firmware's `video=` option for HDMI (with `disable_fw_kms_setup=0`) is not written.
- HDMI: the HVS composes the picture when the host asks for one (a screenshot, a VNC update),
  not line by line; its line counter reads 0, underruns never happen, the transposer (writeback)
  writes nothing. Scaling is nearest-neighbour (the hardware filters). The HDMI handshakes complete
  at once; the encoder sends no TMDS anyone checks. Audio leaves as 16-bit stereo: channels beyond
  the first two are dropped. The CEC bus has the TV only, which acknowledges what is sent to it and
  answers the common queries (and Feature Abort for the rest); messages take 2 ms.
- VCHIQ: the firmware's services run when Linux rings the doorbell, on the emulator's thread,
  and take no guest time; only the audio service is paced (by the sample rate). Synchronous
  messages (no kernel driver here uses them) are dropped, PAUSE/RESUME (VideoCore suspend) are
  ignored. Services other than MMAL, shared memory and audio are refused, as firmware without
  them would: `vcgencmd` (`GCMD`), the TV service and the camera are not there.
- Video: H.264 decode itself is not implemented yet (see the plan); the stand-in decoder emits
  colour bars of the stream's size for each coded picture. The encoders accept pictures and
  return empty output buffers. MPEG-2 and VC-1 are not offered (they need a licence key on a
  real Pi 3). Deinterlacing (`image_fx`) passes pictures through. Port tunnelling (`CONNECT`)
  is refused; kernel drivers do not use it. After a snapshot restore a decoder starts again at
  the stream's next parameter sets.
- Headphone audio leaves as 16-bit stereo on `--audio-out`, together with HDMI audio (both at
  the rate of whichever played last); the jack's PWM output and its DMA are not modelled.
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
# VCHIQ: the media devices, the decoder (with its stand-in), the ISP, the headphone card
./target/release/sbc-emu run --board pi3b --sd cache/os/raspios-lite.img --audio-out out/rpi3-vchiq.wav \
    --script scripts/rpi3-vchiq.txt --timeout 900
# composite: config.txt edited in the guest (copy-on-write), PAL and NTSC on Composite-1
./target/release/sbc-emu run --board pi3b --sd cache/os/raspios-lite.img --script scripts/rpi3-composite.txt --timeout 900
# first boot, then Wi-Fi (scan, open and WPA2 networks, DHCP, ping) and a Bluetooth LE scan
./target/release/sbc-emu run --board pi3b --sd cache/os/raspios-lite.img --script scripts/rpi3-wireless.txt --timeout 900
# the other models: identity, RAM, USB tree, network (each saves a snapshot after logging in)
./target/release/sbc-emu run --board pi3b --model 3b --sd cache/os/raspios-lite.img --script scripts/rpi3-model-3b.txt --timeout 900
./target/release/sbc-emu run --board pi3b --model 3a+ --sd cache/os/raspios-lite.img --script scripts/rpi3-model-3aplus.txt --timeout 900
./target/release/sbc-emu run --board pi3b --model zero2w --sd cache/os/raspios-lite.img --script scripts/rpi3-model-zero2w.txt --timeout 900

# the Pi kernel with a BusyBox initramfs (console on ttyS0)
mkdir -p cache/rpi3
./target/release/sbc-emu sd cat cache/os/raspios-lite.img kernel8.img | gunzip > cache/rpi3/Image
./target/release/sbc-emu mkinitramfs --board pi3b --busybox busybox.static -o cache/rpi3/initramfs.cpio
./target/release/sbc-emu run --board pi3b --kernel cache/rpi3/Image --initrd cache/rpi3/initramfs.cpio \
    --script scripts/rpi3-shell.txt --timeout 120
./target/release/sbc-emu run --board pi3b --kernel cache/rpi3/Image --initrd cache/rpi3/initramfs.cpio \
    --script scripts/rpi3-usb.txt --timeout 180
```

In the browser (`web/rpi3.html`; on the site, `rpi3.html` runs the quick demo and the visitor's
own card):

```sh
tools/wasm-build.sh             # stages kernel8.img, cache/rpi3/initramfs.cpio and the Pi OS image if present
python3 tools/serve.py 8791     # then http://127.0.0.1:8791/rpi3.html (?model=3b&os=pi3-busybox boots at once)
node tools/wasm-smoke-rpi3.mjs 120                                              # the BusyBox shell, headless
node tools/wasm-smoke-rpi3.mjs 900 --sd cache/os/raspios-lite.img out/rpi3-wasm.png   # Pi OS to login:
```

Options: `--model 3b+|3b|3a+|zero2w` (default `3b+`; the others change the RAM size, revision
code, model string, `config.txt` sections, USB topology and network: the 3B gets the LAN9514's
5-port hub with its 100 Mb/s Ethernet, the 3A+ and Zero 2 W the keyboard on the root port and no
Ethernet; `scripts/rpi3-model-*.txt` check each), `--smp N`,
`--threads single|multi`, `--ram-mb N`, `--seed N`, `--no-jit`, `--no-display`, `--sd-persist`,
`--net nat|local|none` (default `nat`; Wi-Fi gets its own isolated network), `--no-bluetooth`,
`--trace-wifi` (the Wi-Fi chip's host interface), `--trace-sdio` (the Arasan's SDIO commands),
`--display kms|fb` (default: `kms` when `config.txt` has
`dtoverlay=vc4-kms-v3d`, as the image's does; `fb` for `--kernel`), `--hdmi-mode WxH` (the
monitor's preferred mode: 1920x1080, 1280x720, 1024x768, 800x600 or 640x480), `--audio-out FILE`
(HDMI and headphone audio: a `.wav`, or raw 16-bit stereo for any other path), `--hostfwd tcp:[HOSTADDR]:HOSTPORT-[GUESTADDR]:GUESTPORT`,
`--gdb PORT [--gdb-wait]`, `--restore FILE`, `--screenshot FILE`, `--vnc :N` (the viewer's keys
and pointer go to the USB keyboard and tablet), `--script FILE`, `--timeout SECS`. Scripts take
`wait-pixel X Y RRGGBB` (the screen shows that colour), `wait-idle SECS` (every core has waited
that long in wall time) and `hdmi connect|disconnect` besides the commands of the other boards. Ctrl-A x quits,
Ctrl-A s writes a snapshot. `SBC_SD_TRACE=1` traces the SD host, `SBC_USB_TRACE=1` the USB
transfers, `SBC_NET_LOG=1` the virtual network, `SBC_TRACE_VC4=1` the display registers,
`SBC_CEC_LOG=1` the CEC messages (at the end of the run), `SBC_VCHIQ_TRACE=1` the VCHIQ
messages, `SBC_MMAL_TRACE=1` the MMAL calls.

## What does not work yet

- USB: no USB stick or camera option on the Pi 3 yet (the shared devices exist; nothing plugs
  them in), no isochronous or low-speed devices, no hub over-current or port-power faults. The
  3A+ and Zero 2 W have only the keyboard on their one port (no hub: no tablet).
- Ethernet: the statistics counters (`ethtool -S`) read zero, wake-on-LAN and EEE do nothing,
  the receive filters (perfect, hash, VLAN) are not applied: every frame from the network is
  delivered. The LAN9514's TX checksum engine does not turn a zero UDP checksum into 0xffff
  (Linux offloads only IPv4 checksums to it for that reason), and its EEPROM is absent.
- No DSI or DPI displays. The VideoCore IV 3D GPU: [raspberry-pi-3-gpu.md](raspberry-pi-3-gpu.md).
- No H.264 decoder yet (the stand-in emits test pictures), no video or JPEG encoder, no
  camera, no `vcgencmd`.
- Wireless only on the 3B+ and 3A+ (CYW43455). The 3B's BCM43438 and the Zero 2 W's CYW43436
  are another chip family (a Cortex-M3 with SOCRAM instead of the CR4, another backplane layout
  and download path in `brcmfmac`), not just another chip ID: those models come up without Wi-Fi
  and Bluetooth.
- No I²C on the header, no SPI, PCM/I²S or system timer in the device tree; the GPIO block has no
  pull-up/down state (`GPPUD`) and nothing on the header pins.
- 32-bit kernels (`kernel7.img`, `arm_64bit=0`) are not supported.
- In the browser (`web/rpi3.html`): no NAT (the virtual network has DHCP and DNS only, as on
  the other pages), no snapshot to resume from on the public site (`sbc_restore_pi3` takes one
  made natively), one host thread.
