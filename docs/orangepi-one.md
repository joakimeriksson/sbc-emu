# Orange Pi One (Allwinner H3)

`--board orangepi-one`, experimental.

A 32-bit machine on its own ARMv7 core (`arm-a32`, an interpreter with a block JIT to AArch64, see [below](#the-jit)): four Cortex-A7 cores with VFPv4 and NEON (started through PSCI CPU_ON, as U-Boot's secure monitor does on the board), the H3's GIC-400 with inter-processor interrupts, UART0
(DesignWare), CCU, timers and the architectural timers. It boots an unmodified Alpine Linux
kernel (6.18 `linux-lts`, armv7) with the unmodified mainline `sun8i-h3-orangepi-one.dtb`:
like U-Boot on the real board, the emulator fills in `/chosen` and `/memory` and adds a
`/psci` node whose calls (CPU on/off, power-off, reset) the host answers. The initramfs is Alpine's
armhf minirootfs, so the shell is an ordinary hard-float BusyBox userland.

```sh
scripts/orangepi-one-fixture.sh            # pinned Alpine downloads -> cache/orangepi-one/
F=cache/orangepi-one
sbc-emu run --board orangepi-one --kernel $F/zImage --dtb $F/sun8i-h3-orangepi-one.dtb \
    --initrd $F/initramfs.cpio.gz           # Ctrl-A x quits
# an SD card image: --sd card.img (copy-on-write, survives guest reboot; --sd-persist writes the file)
# boot a distribution's SD image (no --kernel): Armbian, unmodified
scripts/armbian-orangepi-one-fixture.sh
sbc-emu run --board orangepi-one --sd cache/armbian/Armbian_26.8.1_Orangepione_trixie_current_6.18.44_minimal.img
# the HDMI output: a VNC viewer on 127.0.0.1:5901, a PNG at the end (or `screenshot FILE` in a script)
sbc-emu run --board orangepi-one --sd cache/armbian/...img --vnc :1 --screenshot out/hdmi.png
```

The HDMI output is the H3's own display pipeline as Linux's `sun4i-drm` drives it: the
DE2 mixer composes its planes from RAM (UI and RGB VI layers, all RGB formats, alpha
blending, scaling), TCON0 raises 60 Hz vblank interrupts, and the DesignWare HDMI
controller with the H3 PHY reports a connected monitor whose EDID it reads over DDC. The
monitor's preferred mode is 1280x720 (`--hdmi-mode 1024x768|1280x720|1920x1080`). Armbian's
kernel console (`console=tty1`) appears on it through fbcon.

Ethernet: the H3's EMAC (`dwmac-sun8i`) with its internal 100 Mbit/s PHY, plugged into
the same virtual network as the Pi 4's (`--net nat`, the default: DHCP gives 10.0.2.15,
DNS at 10.0.2.3, TCP/UDP relayed to the host's network, 10.0.2.2 is the host; `--net
local` keeps it inside the virtual subnet; `--net none` unplugs the cable). Like U-Boot,
the emulated firmware writes the board's MAC address into the device tree.

USB: each host port has the H3's EHCI and OHCI controllers. A USB keyboard is plugged
into the board's USB-A port and a USB tablet (absolute pointer) into the OTG port; both are
full-speed HID devices that the OHCI enumerates (the EHCI starts with its ports empty, as
it does with only full-speed devices plugged in). A VNC viewer's keys and pointer go to
them; scripts type with `type TEXT` (`\n` is Enter) and wait with `sleep SECS` (virtual
time).

The NEON check (Debian armhf glibc, OpenSSL and libjpeg-turbo; about 80 s on an M-series Mac):

```sh
scripts/debian-armhf-neon-fixture.sh       # pinned Debian 13 packages -> cache/debian-armhf-neon/
sbc-emu run --board orangepi-one --kernel $F/zImage --dtb $F/sun8i-h3-orangepi-one.dtb \
    --initrd cache/debian-armhf-neon/initramfs.cpio.gz --script scripts/orangepi-one-neon.txt
```

| | Status | Evidence |
|---|---|---|
| Boot | kernel to the `orangepi-one#` BusyBox prompt on four cores (`smp: Brought up 1 node, 4 CPUs`) in 0.67 s of guest time (about 1 G instructions); the whole script in 7.3 s on an M5 Mac with the JIT, 70 s interpreted | `scripts/orangepi-one-shell.txt` |
| SMP | `nproc` = 4; four parallel shell loops; rescheduling and function-call IPIs on all four CPUs in `/proc/interrupts` | same script |
| Userland | shell input on `ttyS0`, `/proc/device-tree/model` = `Xunlong Orange Pi One`, VFPv4 floating point (`awk` arithmetic), `sleep`, tmpfs I/O | same script |
| Firmware | PSCI 1.0 over SMC from the host: `reboot -f` boots again, `poweroff -f` ends the run | same script |
| SD card | the H3's SD/MMC controller with its internal DMA (`sunxi-mmc`): a host-made 64 MiB ext4 card mounts; a 4 MiB file written by the guest verifies after dropping the page cache (read back from the card); `fstrim` (erase commands); the data is still there after a guest reboot | `scripts/orangepi-one-sd.txt` |
| Armbian | the unmodified Armbian 26.8.1 trixie image (kernel 6.18.44, a Thumb-2 kernel): emulated firmware boot from the card's ext4 `/boot` (armbianEnv.txt, zImage, DTB, uInitrd as Armbian's boot.scr does), four cores, initramfs, fsck, root by UUID, systemd to `multi-user.target`, the console login and Armbian's first-run wizard | `scripts/orangepi-one-armbian.txt` |
| HDMI | Armbian's kernel binds `sun4i-drm` (mixer, TCON, `sun8i-dw-hdmi`: "Detected HDMI TX controller v1.32a", EDID over DDC), fbcon draws the boot log at 1280x720; `--vnc` and `--screenshot` show it | `scripts/orangepi-one-hdmi.txt` |
| Ethernet | Armbian's `dwmac-sun8i` binds (chain mode, internal PHY over the MDIO mux), `end0` gets 10.0.2.15 by DHCP (the welcome banner's "IP address"); from a root shell the gateway answers pings, and a 1 MiB file fetched over TCP from a host HTTP server through the NAT matches its MD5 | `scripts/orangepi-one-ethernet.txt` |
| USB | Armbian enumerates "sbc-emu Virtual USB Keyboard" on `usb4` (OHCI1, USB-A port) and the tablet on `usb1` (OHCI0, OTG port), both `hid-generic`; the EHCIs start ("USB 2.0 started, EHCI 1.00"). Keys typed on the keyboard answer Armbian's first-run wizard on the HDMI console (tty1) | `scripts/orangepi-one-usb.txt` |
| Thumb-2 userland | Debian 13's armhf `busybox-static` (Thumb-2, hard float): SHA-256 and MD5 of known strings, `sort`, floating-point `awk`, a gzip round trip of 256 KiB, on four cores | `scripts/debian-armhf-busybox-fixture.sh`, `scripts/orangepi-one-thumb.txt` |
| NEON | Linux sets `HWCAP_NEON` (`neon` in `/proc/cpuinfo`); Debian 13's armhf glibc, OpenSSL 3.5 (`OPENSSL_armcap=0x1`) and libjpeg-turbo 2.1 run their NEON code: SHA-1/256/512, ChaCha20 and Poly1305 of 1.2 MB, a JPEG encode and decode, all equal to the host's native OpenSSL 3 / libjpeg-turbo 3 results and to the guest's scalar paths | `scripts/debian-armhf-neon-fixture.sh`, `scripts/orangepi-one-neon.txt` |
| GPU | the Mali-400 MP2 (`mali400/`): Armbian's `lima` driver binds ("gp - mali400 version major 1 minor 1", pp0/pp1, 64K L2, `/dev/dri/renderD128`) and Mesa 25's lima driver ("renderer: Mali400", OpenGL ES 2.0) renders kmscube (plain and textured) to the HDMI output; 25 GLES 2 test scenes match Mesa's softpipe | `scripts/armbian-gpu-fixture.sh`, `scripts/orangepi-one-gpu.txt`, `scripts/orangepi-one-gles2test.txt` |

Not modelled yet: USB high speed (EHCI transfers), isochronous transfers, the OTG controller (MUSB), HDMI audio and CEC, YUV video layers. The SD boot does what U-Boot and Armbian's boot.scr do, without running U-Boot (no DRAM controller, no secure monitor model); the serial console is put last on the command line so `/dev/console` is the serial port, and DT overlays from armbianEnv.txt are not applied. The cores
take turns on one host thread (round-robin quanta of 1000 instructions). Every other device node in the
distribution DTB is a plain read-back register file, so its driver probes without effect;
the run summary lists the pages that were touched. Virtual time is 1 ns per instruction.


## The GPU

`mali400/` models the H3's Mali-400 MP2 behind the same interface the real one has, so the
stock kernel (`lima`) and Mesa (`lima` Gallium driver) drive it unmodified:

- one 64 KiB register window (`gpu@1c40000`) with the GP, two PPs, the L2 cache, the PMU and
  three MMUs, each with its own interrupt; jobs start when the driver writes a GP or PP
  command register and complete within that write;
- the GPU MMUs: two-level 4 KiB page tables, page faults reported to the driver;
- the GP: the vertex shader command list and an interpreter for Mesa `gpir`'s 128-bit VLIW
  instructions (two multipliers, two adders, the pass and complex units, stores to
  varyings), then the PLBU command list: primitive assembly from the shader's screen
  positions, clipping of triangles that cross w = 0, culling, scissors, indexed and
  non-indexed draws; when the tile heap is full the PLBU stops with OUT_OF_MEM and
  continues once the driver has grown it, as the hardware does;
- the PPs: the PP stream's tile list, a rasteriser for 16x16 tiles (fixed-point edges with
  the top-left rule, GL's line rule, perspective-correct varyings), an interpreter for Mesa
  `ppir`'s variable-length VLIW instructions run on 2x2 quads (varyings, textures with
  mipmaps, wrap modes, cube maps and ETC1, fp16 uniforms, vec4 and scalar pipelines,
  rcp/rsqrt/exp2/log2/sin/cos, derivatives, discard, branches and loops, gl_FragDepth),
  depth, stencil, alpha test and blending, and the write-back units (linear and 16x16
  u-interleaved tiled, RGBA8888/565/5551/4444/fp16, Z24S8);
- jobs take virtual time as on the real GPU at 432 MHz (cycles from the fragments shaded,
  the tiles written and the vertices processed) and complete with their interrupt then;
- the polygon lists between the PLBU and the PPs are internal to the GPU, so the model uses
  its own record format inside the buffers the driver allocates for them.

`tests/gpu/gles2test.c` renders 25 GLES 2 scenes on the GPU and on Mesa's softpipe in the
same guest and compares them per pixel (`scripts/orangepi-one-gles2test.txt`, with
`tools/gpu_server.py` serving the files): all pass. llvmpipe is not used as the reference:
LLVM's code for it calls a helper through an unresolved stub on this armhf image.

`SBC_MALI_LOG=1` prints a line per GP and PP job; `SBC_MALI_DUMP=1` dumps the first
primitive's render state, varyings and decoded fragment shader of each frame.

Known gaps: multisampling is not modelled, and the timing model is a rough cycle count.

## The JIT

`arm-a32/src/jit.rs` (ARM state) and `jit_t32.rs` (Thumb). On AArch64 hosts (macOS, Linux)
hot blocks are translated to host code; `--no-jit` or `SBC_JIT=0` runs the interpreter alone.

- A32 and A64 share the NZCV layout, the condition codes and the add/subtract carry and
  overflow rules, so the guest flags live in the host flags inside a block: `ADDS`, `SUBS`,
  `ADCS`, `SBCS` and `B.cond` give the architected result. Logical operations with S take
  their carry from the shifter (immediate and register-specified shifts), computed
  separately.
- Native: data processing in every operand form, MOVW/MOVT, multiplies, divides, CLZ,
  bitfields, extends, REV, loads and stores of every size (with writeback, LDRD/STRD,
  LDM/STM, PUSH/POP), branches, BL/BLX/BX and returns through the PC with interworking,
  CBZ/CBNZ, MRS, the thread-ID MRCs, cache-maintenance MCRs, barriers and hints. Thumb
  instructions are mostly the A32 instruction the interpreter maps them to, plus the
  Thumb-only forms (modified immediates, ORN, ADDW/SUBW, 12-bit and indexed offsets,
  literal loads). IT blocks compile with their ITSTATE tracked at compile time.
- Everything else (VFP/NEON, exclusives, CPS/MSR, system registers, TBB/TBH, parallel
  arithmetic) runs one instruction in the interpreter; the block ends there if the PC, the
  mode, the interrupt masks or ITSTATE changed, a device was touched or an exception taken.
- Loads and stores probe a 256-entry micro-TLB per privilege and access type inline (the
  interpreter uses the same tables); a miss, a misaligned access or a page crossing runs
  the instruction in the interpreter.
- Blocks are keyed by virtual and physical PC, privilege and instruction set, and chain
  through a dispatcher until the machine's next device deadline.
- Code is invalidated by writes, not by cache maintenance: pages holding compiled code never
  enter the write micro-TLB, so every store to them (from any core) takes the slow path and
  drops their blocks on all cores; the SD controller's DMA and image loads report the RAM
  they write.
- TLB maintenance by MVA drops one page (or the whole MiB if it came from a section) instead
  of the whole TLB, which the interpreter benefits from too.

`arm-a32/tests/jit_diff.rs` runs random A32 and Thumb blocks (every operand form and
condition, IT blocks, all load/store forms) once interpreted and once compiled from the same
state and compares registers, flags, memory and the instruction count; `JIT_DIFF_CASES=N`
runs more. `SBC_JIT_PROFILE=1` lists the instructions compiled code still hands to the
interpreter, `SBC_JIT_CHAIN=0` turns chaining off and `SBC_JIT_HOT=N` sets how often a block
starts before it is compiled (default 8).

| Workload (M5 Mac) | Interpreter | JIT |
|---|---|---|
| Alpine: `scripts/orangepi-one-shell.txt` (boot, SMP, reboot, power-off) | 70 s | 7.3 s |
| Debian Thumb-2 userland: `scripts/orangepi-one-thumb.txt` | 22.4 s | 1.3 s |
| Armbian 26.8.1 (Thumb-2 kernel and userland) to the first-run wizard: `scripts/orangepi-one-armbian.txt` | 22 min | 5.4 min |

Design notes, fixture hashes and the bugs found during bring-up: [multi-board-foundation.md](multi-board-foundation.md#orange-pi-one-allwinner-h3-2026-11-15).
