# Getting started

## In the browser: nothing to install

[joakimeriksson.github.io/sbc-emu](https://joakimeriksson.github.io/sbc-emu/) boots all seven boards
on your own computer, in a tab. Pick a board; it reaches a shell in seconds. Type in the terminal
under the board. Everything stays on your computer.

## On your computer

You need [Rust](https://rustup.rs) (stable) and, for the guests, `curl`, `tar`, `gzip`, `cpio`, `ar`
and `python3` (all there on macOS with the Xcode command line tools, and on most Linux systems).
sbc-emu runs on macOS and Linux. Its JITs, which make it fast, generate AArch64 code, so it is
fastest on Apple silicon and other AArch64 machines; on x86-64 it interprets, which is slower.

```sh
git clone https://github.com/joakimeriksson/sbc-emu
cd sbc-emu
scripts/quickstart.sh orangepi-one
```

`scripts/quickstart.sh BOARD` builds sbc-emu and boots the board to a root shell on its serial
console, in your terminal. The first time, it fetches a small guest for the board: a Linux kernel
and a shell in an initramfs, from pinned, checksummed Alpine, Debian and Raspberry Pi packages
(the board's `scripts/*-fixture.sh`, into `cache/`). Later runs start at once.

| Board | Command | Fetched once | To a shell |
|---|---|---|---|
| Raspberry Pi 4 | `scripts/quickstart.sh pi4b` | 11 MB | 2 s |
| Orange Pi 5 | `scripts/quickstart.sh orangepi5` | 97 MB | 3 s |
| Orange Pi One | `scripts/quickstart.sh orangepi-one` | 57 MB | 7 s |
| ASUS Tinker Board S | `scripts/quickstart.sh tinkerboard` | 60 MB | 7 s |
| Orange Pi RV (RISC-V) | `scripts/quickstart.sh orangepi-rv` | 71 MB | 9 s |
| NVIDIA Jetson Nano | `scripts/quickstart.sh jetson-nano` | 132 MB | 6 s |
| ESP32-S31 | `scripts/quickstart.sh esp32s31` | 11 MB | 6 s |

Times are wall-clock on an Apple M-series Mac, after the first run, to the shell and a few checks
(the boards' `scripts/*-shell.txt`).

**Ctrl-A x quits.** Ctrl-A s saves a snapshot on the boards that have them (below); on the Pi 4,
Ctrl-A c opens the monitor instead (`help` lists its commands). Ctrl-A Ctrl-A sends a Ctrl-A.
Anything after the board name goes to `sbc-emu run`:

```sh
scripts/quickstart.sh pi4b --vnc :1                # the HDMI screen on a VNC viewer at 127.0.0.1:5901
scripts/quickstart.sh pi4b --screenshot hdmi.png   # the screen as a PNG at exit
scripts/quickstart.sh pi4b --gdb 1234              # debug the guest with gdb or lldb
scripts/quickstart.sh pi4b --net none              # no network (the default is NAT to your host's)
```

`./target/release/sbc-emu boards list` lists the boards, and `./target/release/sbc-emu` with no
arguments prints the options.

## Full operating systems

These boot the distributions' own images, unmodified. Each fetch
script downloads the image from its publisher, checks the publisher's checksum and unpacks it
into `cache/`. Without `--sd-persist`, what the guest writes stays in memory: the image file is
not changed, and every boot starts fresh. With `--sd-persist` the guest writes to the file.

The commands use `sbc-emu` for `./target/release/sbc-emu`.

### Raspberry Pi 4: Raspberry Pi OS

```sh
scripts/raspios-fixture.sh                      # 550 MB download, unpacks to cache/os/raspios-lite.img
sbc-emu run --sd cache/os/raspios-lite.img --input usb --vnc :1
```

Open a VNC viewer on `127.0.0.1:5901` (macOS: `open vnc://127.0.0.1:5901` needs a password, so add
`--vnc :1,password=raspberry`). The first boot asks for a keyboard layout and a user on the screen;
`--input usb` sends what you type in the terminal to the board's USB keyboard. Ubuntu 26.04
Desktop and the GPU demos: [raspberry-pi-4.md](raspberry-pi-4.md), [gpu.md](gpu.md).

### Orange Pi 5, Orange Pi One, Tinker Board S: Armbian

```sh
scripts/armbian-orangepi5-fixture.sh            # 357 MB download
sbc-emu run --board orangepi5 --sd cache/armbian/Armbian_26.8.1_Orangepi5_trixie_current_6.18.43_minimal.img --ram-mb 1024

scripts/armbian-orangepi-one-fixture.sh
sbc-emu run --board orangepi-one --sd cache/armbian/Armbian_26.8.1_Orangepione_trixie_current_6.18.44_minimal.img

scripts/armbian-tinkerboard-fixture.sh
sbc-emu run --board tinkerboard --sd cache/tinker-board-armbian/Armbian_26.8.1_Tinkerboard_trixie_current_6.18.43_minimal.img
```

Armbian's login is on the serial console, in your terminal. On the Orange Pi One and the Tinker
Board the first login is Armbian's first-run wizard: it asks for a root password and offers to
create a user (Ctrl-C skips that); the Orange Pi 5 image logs root in by itself. Add `--vnc :1`
for HDMI. More, including the GPUs: [orangepi5.md](orangepi5.md), [orangepi-one.md](orangepi-one.md), [mali-t760.md](mali-t760.md).

### Orange Pi RV: Ubuntu 24.04

```sh
scripts/orangepi-rv-uboot.sh                    # the board's SPI flash: mainline U-Boot, built in Docker
scripts/orangepi-rv-ubuntu.sh                   # Canonical's image for JH7110 boards, 1.1 GB download
sbc-emu run --board orangepi-rv --spi-flash cache/orangepi-rv/uboot/spi-flash.img \
    --sd cache/orangepi-rv/ubuntu/ubuntu-24.04.5-preinstalled-server-riscv64+jh7110.img --ram-mb 4096
```

Log in as `ubuntu` / `ubuntu`; it asks for a new password. U-Boot needs Docker (an x86-64 or
AArch64 Linux VM; Colima works on a Mac). More: [orangepi-rv.md](orangepi-rv.md).

### Jetson Nano: Debian 13

Debian's own arm64 kernel and its Tegra drivers, with Debian's cloud image as the microSD card:

```sh
scripts/jetson-nano-debian-fixture.sh           # Debian's kernel and device tree, and an initramfs
scripts/jetson-nano-card-fixture.sh             # Debian's arm64 image, 283 MB download
D=cache/jetson-nano/debian
sbc-emu run --board jetson-nano --kernel $D/vmlinuz --dtb $D/tegra210-p3450-0000.dtb \
    --initrd $D/initramfs.cpio.gz --sd $D/disk.raw --append "console=ttyS0,115200 root=card rw quiet audit=0 \
    systemd.set_credential=passwd.plaintext-password.root:nano systemd.set_credential=firstboot.locale:C.UTF-8 \
    systemd.set_credential=firstboot.timezone:UTC systemd.set_credential=firstboot.keymap:us"
```

Log in as `root` / `nano` (the password is set on the command line). The GPU demos (kmscube,
glmark2 on nouveau and Mesa): [jetson-nano-gpu.md](jetson-nano-gpu.md). More:
[jetson-nano-plan.md](jetson-nano-plan.md).

### ESP32-S31: build the image yourself

The quick start fetches Espressif's official Linux flash image as built by
`scripts/esp32s31-fixture.sh`. That script builds it from Espressif's sources in Docker (about an
hour the first time). More: [esp32s31.md](esp32s31.md).

## Next

- **Snapshots:** save the whole machine to a file (Ctrl-A s, or `savevm` in the Pi 4's monitor)
  and `--restore FILE` goes on from there in a second, on the Pi 4, Orange Pi 5, Orange Pi One,
  Tinker Board and Orange Pi RV ([snapshots.md](snapshots.md)).
- **Debugging:** `--gdb PORT` on every board; breakpoints, watchpoints and stepping on the guest's
  kernel or programs ([debugging.md](debugging.md)).
- **Scripts:** `--script FILE` types into the guest and waits for its output; the board checks in
  `scripts/*.txt` are examples.
- **Your own kernel or initramfs:** `--kernel`, `--dtb`, `--initrd` and `--append`; each board's
  page says which kernel options its drivers need.
