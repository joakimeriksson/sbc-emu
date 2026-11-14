# sbc-emu: a Raspberry Pi 4 emulator in Rust

**Real boards. Virtual hardware.** An instruction-level emulator of the Raspberry Pi 4 Model B
in the style of [esp32sim](https://github.com/mikroverk/esp32sim): its own ARMv8-A CPU core,
register-level BCM2711 peripherals, one deterministic virtual clock, no QEMU. It boots the
unmodified **Raspberry Pi OS Lite (trixie, arm64) SD-card image**, natively and in the browser
(WebAssembly).

See [docs/decisions.md](docs/decisions.md) for why this is not QEMU-based, and
[SBC-EMUPLAN.md](SBC-EMUPLAN.md) for the overall plan.

## What runs today

| | Status | Evidence |
|---|---|---|
| Raspberry Pi kernel 6.18 (firmware 1.20260915 `kernel8.img`) | boots to userspace | busybox initramfs shell on serial and HDMI |
| Raspberry Pi OS Lite 2026-10-06 (unmodified `.img`) | boots on all four cores: firmware-style SD boot → initramfs → ext4 root → systemd 257 → `raspberrypi login:` and the first-boot setup wizard on HDMI | `scripts/raspios-firstboot.txt`; ~27 G instructions to the login prompt |
| Four cores | four Cortex-A72s started through PSCI, GIC-400 with per-core interfaces, SGIs (IPIs) and SPI routing | `smp: Brought up 1 node, 4 CPUs`; four busy loops finish in 1.6 s of guest time instead of 6.5 s |
| USB | PCIe root complex + VL805 xHCI behind the four USB-A ports, as on a Pi 4: USB keyboard and tablet (absolute pointer) by default, a USB stick (`--usb-drive IMAGE`) and a UVC webcam (`--camera`) on request; the DWC2 OTG port too | `lsusb -t`: devices on the xHCI root hub; typing on tty1; `mkfs.vfat` + mount + read back on the stick; `v4l2-ctl` captures 30 frames of 640×480 YUYV |
| Audio | an I²S DAC HAT (PCM5102A on `simple-audio-card`) on the BCM2835 PCM/I²S block, fed by DREQ-paced DMA; `--audio-out FILE.wav` (native) or WebAudio (browser) | `speaker-test -t sine -f 440`: the WAV holds a 440.0 Hz tone, 3.000 s per channel at 48 kHz |
| Bluetooth | the CYW43455's HCI controller on the mini UART (as `dtoverlay=miniuart-bt`), a virtual BLE sensor nearby | `hci_bcm` downloads the real patch RAM, `bluetoothctl` powers on, a scan finds `sbc-emu sensor` |
| Ethernet | GENET v5 + UniMAC MDIO + BCM54213PE PHY on a virtual 10.0.2.0/24 network (DHCP, DNS, user-mode NAT to the host's network) | busybox: DHCP lease, ping, DNS, `wget` of a real web page |
| Wi-Fi | CYW43455 on SDIO for the stock `brcmfmac`: real firmware download, emulated FullMAC host interface | `wlan0`, NetworkManager scan lists `sbc-emu`, `nmcli dev wifi connect`, DHCP and ping over Wi-Fi |
| GPIO / I²C / SPI | BCM2711 GPIO (LED on GPIO17, button on GPIO27), BSC I²C1 + BME280, SPI0 + MCP3008 on CE0 | stock drivers: `gpioset`/`gpioget` (libgpiod), IIO sysfs reads the host-set temperature/pressure/humidity and ADC voltages exactly |
| Speed | A64→A64 block JIT on AArch64 hosts (native integer, branches, loads/stores, FP/NEON; interpreter fallback) | Raspberry Pi OS cold boot to `login:` 248 s (interpreter) → 37 s on four cores in turns, 29 s with a host thread per core (`--threads multi`); all CPU tests pass with and without it. Browser: WebAssembly region JIT (`?jit=0` turns it off), Pi OS `login:` 313 s → 160 s, verified against the interpreter |
| Floating point | exact Arm semantics: FPCR rounding modes, flush-to-zero, default NaN, all FPSR exception flags, Arm's estimate tables | `diff_fp.rs`: 18,800 random cases bit-exact against the host CPU (interpreter and JIT) |
| More peripherals | DMA controller, hardware RNG, thermal sensor, PM watchdog and power domains, system timer, mini UART | stock drivers bind: `/dev/hwrng`, `/sys/class/thermal`, `/dev/watchdog` (expiry reboots), `ttyS0`, `dma0chan*` |
| Debugging, display | GDB remote stub (`--gdb PORT`, one thread per core, breakpoints and watchpoints), monitor (Ctrl-A c: registers, memory, stop/cont, savevm/loadvm, screendump), VNC server (`--vnc :N`) | lldb: kernel breakpoint, step, memory read, detach; a VNC client sees the console and types into it |
| Port forwarding | `--hostfwd tcp::2222-:22` (QEMU's syntax) through the user-mode NAT | `ssh-keyscan -p 2222 127.0.0.1` reads the Pi OS host keys |
| HDMI | the vc4 KMS pipeline the image's `config.txt` asks for (`dtoverlay=vc4-kms-v3d`): HVS compositor, pixel valve, HDMI0 encoder + PHY, DDC with a 1080p monitor's EDID, HDMI audio; `--display fb` keeps the firmware's `simple-framebuffer` (1024×768) instead | stock `vc4`: `HDMI-A-1` connected, modes from the EDID, the console at 1920×1080; `kmstest` switches to 1024×768 and page-flips at 60.00 fps; `speaker-test -D hdmi:…`: a 440.0 Hz tone at 48 kHz on the host; screenshots via `--screenshot` / `shot` |
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

## The emulated hardware

| Block | Model | Linux driver |
|---|---|---|
| CPU | 4× ARMv8.0-A, Cortex-A72 identity: A64 integer ISA with CRC32, FP (exact, all FPCR modes), Advanced SIMD, EL0–EL3, stage-1 MMU (4 KiB), generic timers, PSCI (CPU_ON/OFF, AFFINITY_INFO, reset, off) via the host | — |
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

## Running it

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
- `--usb-drive IMAGE`: a USB stick (copy-on-write, like the SD card) on the third USB-A port;
  given with `--restore`, it is hot-plugged after the restore.
- `--camera`: a USB webcam (UVC, 640×480 YUYV at 30 frames/s of guest time, a test pattern).
- `--audio-out FILE.wav`: what the guest plays on the I²S DAC (`aplay`, `speaker-test`), as a
  16-bit stereo WAV.
- `--otg-keyboard`: the keyboard on the USB-C (DWC2 OTG) port instead of the xHCI ports.
- `--vnc :N` (port 5900+N): a VNC server for the HDMI framebuffer (no password, Raw encoding of the
  rows that changed, desktop resize); keys go to the USB keyboard, the pointer to the USB tablet. Works with TigerVNC/RealVNC
  viewers (macOS Screen Sharing insists on a password).
- `--cpu-crypto`: the Cryptographic Extension (AES, SHA-1/256, PMULL) as QEMU's `cortex-a72`
  has it; the real BCM2711 does not, so it is off by default.
- Ctrl-A c opens the **monitor** (`help` lists the commands: `info registers|cpus|irq|network|jit`,
  `x`/`xp`/`xlate`, `stop`/`cont`, `savevm`/`loadvm`, `screendump`, `sendkey`, `mouse_move`,
  `mouse_button`, `hostfwd_add`, `gdbserver`, `system_reset`, `quit`); scripts can run them with
  `monitor CMD`.

### In the browser

```sh
tools/wasm-build.sh                               # web/wasm/sbc-emu.wasm + web/assets
ln -s ../../cache/os/raspios-lite.img web/assets/raspios-lite.img
tools/serve.py                                    # needs HTTP Range support: use this, not http.server
open http://127.0.0.1:8791/
```

The page offers a cold boot of the SD image, a resume from `web/assets/raspios-lite.snap` (if
present) and the quick busybox demo. Click the screen to type on the USB keyboard; the mouse
over the screen is the USB tablet, and what the guest plays on the DAC comes out of the page
(after the first click or key press, as browsers require). The browser build runs the cores in
turns on one thread: a thread per core would need WebAssembly threads, which need a standard
library rebuilt with atomics (a nightly-only `-Z build-std`), and this project builds on stable.

## Tests

```sh
cargo test --release --workspace
```

- `arm-a64/tests/diff_host.rs`, `diff_neon.rs`, `diff_fp.rs`: **differential tests against the
  host CPU** (Apple silicon is AArch64): integer (incl. CRC32), FP and NEON snippets run natively
  and on the emulator from the same random states; registers, flags, NaN payloads and FPSR must
  match bit for bit. `diff_fp.rs` draws a random FPCR (rounding mode, FZ, DN) per case and
  operands from the hard classes (subnormals, overflow, ties, NaNs).
- `bcm2711` unit tests: GPIO edges, BME280 compensation against the Linux driver's formula, MCP3008
  framing against `mcp320x`'s decode, GIC SGI/SPI routing between cores, DMA control-block chains,
  and `smp.rs`: four cores add to one counter with LDXR/STXR, one thread and a thread per core,
  interpreted and compiled (a plain store instead of the compare-and-swap loses ~70% of them).
- `arm-a64/tests/system.rs`: exceptions, EL changes, MMU faults, timer interrupts.
- `bcm2711/tests/watch.rs`: debugger watchpoints stop after the access, on the right core.
- `tools/wasm-smoke.mjs`, `tools/wasm-smoke-sd.mjs`: headless boots of the WebAssembly build.

## Layout

```
emu-core/   Bus / Core / Trap and the snapshot serializer
arm-a64/    the ARMv8-A core: decode+execute, system registers, MMU, FP/NEON
bcm2711/    the Pi 4 machine: GIC, PL011, mailbox, SDHCI, DWC2, PCIe + xHCI, USB devices, I²S, Bluetooth,
            GENET, device tree, boot, FAT
sbc-net/    virtual network: DHCP/ARP/ICMP/DNS/NTP, user-mode NAT, 802.11 AP (from esp32sim)
cli/        the sbc-emu command
wasm/       C ABI for the browser; web/ is the page and worker
scripts/    action scripts (first boot of Raspberry Pi OS)
```

## Known gaps

No V3D 3D GPU (a desktop compositor has to render in software), no HDMI1 (the second port has
no monitor), no CEC devices; the HVS draws RGB planes only (no YUV video planes, no tiled
formats) and scales with nearest neighbour;
no CSI camera (the webcam is USB); no USB 3 devices (the USB 3 root ports stay empty), no USB
hubs, no isochronous transfers; Bluetooth has no connections (the sensor only advertises) and
no classic devices; Wi-Fi has no radio model (emulated firmware host interface, open and
WPA2-PSK networks); only the PCM's DREQ paces DMA (the other peripherals complete at once);
no LSE instructions, crypto only with `--cpu-crypto` (the BCM2711's A72 has neither; the ID
registers say so); the native JIT needs an AArch64 host and is still well below a real Pi 4
core; the browser build is about 3× slower than native and single-threaded.

## Compared with QEMU's `raspi4b`

QEMU emulates more boards and runs the guest's cores on several host threads; for a Pi 4
specifically (QEMU 9/10 docs and its open patch series, November 2026):

| | sbc-emu | QEMU `raspi4b` |
|---|---|---|
| Boot | the stock Raspberry Pi OS `.img` (config.txt, cmdline.txt, kernel8.img) | kernel and DTB passed by hand |
| Display | HDMI through the stock `vc4` KMS driver (HVS, pixel valve, HDMI encoder, EDID, HDMI audio) | the firmware framebuffer |
| Cores | 4: in turns on one thread (repeatable) or a thread each (`--threads multi`) | 4, a thread each (MTTCG) |
| Ethernet (GENET) | yes, with NAT and port forwarding | not merged (patches in review) |
| USB | keyboard, tablet, stick, webcam on the xHCI (USB-A) ports; OTG port | keyboard does not enumerate on raspi4b |
| Wi-Fi | yes (stock `brcmfmac`, WPA2) | no |
| GPIO/I²C/SPI devices | LED, button, BME280, MCP3008 | controllers only |
| RNG, thermal, watchdog, system timer, mini UART, DMA | yes | yes |
| PCIe / xHCI | yes (VL805, stock `xhci-pci`) | not merged |
| Audio | I²S DAC HAT, to a WAV file or the browser | no |
| Bluetooth | yes (stock `hci_bcm`, LE scanning) | no |
| FP exceptions and rounding modes | exact | exact |
| GDB stub (incl. watchpoints), monitor, snapshots, port forwarding, VNC | yes | yes |
| In the browser | yes (WebAssembly build with its own JIT) | no |

## The JIT

`arm-a64/src/jit.rs`. Guest and host share the A64 instruction set, so integer and FP/SIMD
data-processing instructions are emitted as themselves with guest registers renamed to host
temporaries — the host computes exactly the architected result, flags included. Loads and stores
probe the core's micro-TLB inline and fall back to the interpreter; system instructions, exception
generation and ERET call the interpreter and end the block. FP/SIMD instructions run natively only
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

Blocks average about 5 guest instructions; a chain now runs about 100 before it returns to the
machine loop. A chain stops at the budget/deadline and whenever an interpreted instruction may need
the machine's attention (`chain_stop`: device access, MSR, cache/TLB maintenance, exceptions, ERET).

Debugging switches: `SBC_JIT_VERIFY=1` re-runs every block in the interpreter from the same state
(stores rolled back; blocks whose code changed before an `IC`, or that touched a device, are
skipped) and reports the first mismatch; `SBC_JIT_PROFILE=1` prints the instruction words compiled
blocks still hand to the interpreter; `SBC_JIT_CHAIN=0` turns chaining and linking off;
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

On by default (`index.html?jit=0` turns it off). Raspberry Pi OS cold boot to `login:` in the
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
