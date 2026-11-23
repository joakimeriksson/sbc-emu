#!/bin/sh
# Boot a board in one command. The first time, this builds sbc-emu and fetches a small guest for the
# board: a Linux kernel and a shell in an initramfs, from pinned, checksummed Alpine, Debian and
# Raspberry Pi packages (the board's fixture script), or for the ESP32-S31 its mask ROM and
# Espressif's Linux flash image. Later runs reuse them. Ctrl-A x quits.
#   scripts/quickstart.sh BOARD [sbc-emu run options...]
#   BOARD: pi4b orangepi5 orangepi-one tinkerboard orangepi-rv jetson-nano esp32s31
# The files go to the repository's cache/ (ignored by git); options with relative paths
# (--screenshot, --sd, --restore) are relative to where you run it. Full operating systems
# (Raspberry Pi OS, Armbian, Ubuntu, Debian) are in docs/getting-started.md.
set -eu
SELF=$0
ROOT=$(cd "$(dirname "$0")/.." && pwd)

usage() { sed -n 's/^#   //p' "$SELF" >&2; exit 2; }
[ $# -ge 1 ] || usage
BOARD=$1
shift
case $BOARD in
  pi4b|orangepi5|orangepi-one|tinkerboard|orangepi-rv|jetson-nano|esp32s31) ;;
  *) echo "unknown board '$BOARD'" >&2; usage ;;
esac

for tool in cargo curl tar gzip cpio ar python3 shasum; do
  command -v "$tool" >/dev/null || { echo "quickstart: needs $tool (docs/getting-started.md)" >&2; exit 1; }
done

fetch() { # URL FILE SHA256
  if ! { [ -f "$2" ] && echo "$3  $2" | shasum -a 256 -c --status; }; then
    curl -fL --retry 3 -o "$2.part" "$1"
    mv "$2.part" "$2"
  fi
  echo "$3  $2" | shasum -a 256 -c --status || { echo "$2: checksum mismatch" >&2; exit 1; }
}
# a guest file is ready when it is there and, for an initramfs, not a near-empty archive
# (what a failed cpio leaves)
ready() { # FILE
  [ -s "$1" ] || return 1
  case $1 in */initramfs*) [ "$(wc -c < "$1")" -gt 500000 ] ;; esac
}
# run a board's fixture script (from the repository root), unless its outputs are there
fixture() { # SCRIPT OUT FILE...
  script=$1 out=$2
  shift 2
  for f in "$@"; do
    ready "$ROOT/$out/$f" && continue
    echo "fetching the guest into $out (once)" >&2
    (cd "$ROOT" && sh "$script" "$out") >&2
    for g in "$@"; do
      ready "$ROOT/$out/$g" || { echo "quickstart: $script did not make $out/$g" >&2; exit 1; }
    done
    return
  done
}

# from the repository, so its rust-toolchain.toml applies
(cd "$ROOT" && cargo build --release -q -p sbc-emu)
BIN=$ROOT/target/release/sbc-emu
C=$ROOT/cache

case $BOARD in
  pi4b)
    # the Raspberry Pi kernel (the one Raspberry Pi OS ships) and Alpine's static BusyBox, as the
    # browser site's Pi 4 shell uses (tools/site-build.sh)
    D=$C/pi4
    mkdir -p "$D"
    if ! ready "$D/Image" || ! ready "$D/initramfs.cpio"; then
      fetch https://github.com/raspberrypi/firmware/raw/1.20260915/boot/kernel8.img "$D/kernel8.img" \
        f80aa8bb092c0df9401065ec1b4ee8b8846c4d9a1bb3272662b50acbd89da68b
      fetch https://dl-cdn.alpinelinux.org/alpine/v3.20/main/aarch64/busybox-static-1.36.1-r31.apk "$D/busybox-static.apk" \
        1d8e7a7fc2ed69bdb8eb7be9d962489c21a0f75b72d8a325437c8a09d4cfac70
      # an .apk is gzip members of tar archives one after another
      python3 -I -c 'import sys, tarfile
t = tarfile.open(sys.argv[1], "r:gz", ignore_zeros=True)
open(sys.argv[2], "wb").write(t.extractfile("bin/busybox.static").read())' "$D/busybox-static.apk" "$D/busybox.static"
      gzip -dc "$D/kernel8.img" > "$D/Image.part"
      mv "$D/Image.part" "$D/Image"
      "$BIN" mkinitramfs --busybox "$D/busybox.static" -o "$D/initramfs.cpio.part"
      mv "$D/initramfs.cpio.part" "$D/initramfs.cpio"
    fi
    exec "$BIN" run --kernel "$D/Image" --initrd "$D/initramfs.cpio" "$@" ;;
  orangepi5)
    fixture scripts/orangepi5-fixture.sh cache/orangepi5 Image initramfs.cpio.gz
    D=$C/orangepi5
    exec "$BIN" run --board orangepi5 --kernel "$D/Image" --initrd "$D/initramfs.cpio.gz" "$@" ;;
  orangepi-one)
    fixture scripts/orangepi-one-fixture.sh cache/orangepi-one zImage sun8i-h3-orangepi-one.dtb initramfs.cpio.gz
    D=$C/orangepi-one
    exec "$BIN" run --board orangepi-one --kernel "$D/zImage" --dtb "$D/sun8i-h3-orangepi-one.dtb" \
      --initrd "$D/initramfs.cpio.gz" "$@" ;;
  tinkerboard)
    fixture scripts/tinker-board-fixture.sh cache/tinker-board zImage rk3288-tinker-s.dtb initramfs.cpio.gz
    D=$C/tinker-board
    exec "$BIN" run --board tinkerboard --kernel "$D/zImage" --dtb "$D/rk3288-tinker-s.dtb" \
      --initrd "$D/initramfs.cpio.gz" "$@" ;;
  orangepi-rv)
    fixture scripts/orangepi-rv-fixture.sh cache/orangepi-rv fw_dynamic.bin Image jh7110-orangepi-rv.dtb initramfs.cpio.gz
    D=$C/orangepi-rv
    exec "$BIN" run --board orangepi-rv --firmware "$D/fw_dynamic.bin" --kernel "$D/Image" \
      --dtb "$D/jh7110-orangepi-rv.dtb" --initrd "$D/initramfs.cpio.gz" "$@" ;;
  jetson-nano)
    fixture scripts/jetson-nano-fixture.sh cache/jetson-nano vmlinuz tegra210-p3450-0000.dtb initramfs.cpio.gz
    D=$C/jetson-nano
    exec "$BIN" run --board jetson-nano --kernel "$D/vmlinuz" --dtb "$D/tegra210-p3450-0000.dtb" \
      --initrd "$D/initramfs.cpio.gz" "$@" ;;
  esp32s31)
    # Espressif's mask ROM (esp-rom-elfs) and the flash image scripts/esp32s31-fixture.sh builds
    # (in Docker, about an hour), as published with sbc-emu's releases; the same files the site uses
    D=$C/esp32s31
    mkdir -p "$D/images"
    if ! { [ -s "$D/esp32s31_rev0_rom.elf" ] && echo "a52ad7513deb656a910a5740125f1cce2c7941f11ce57213b7b43aea93d5ab87  $D/esp32s31_rev0_rom.elf" | shasum -a 256 -c --status; }; then
      fetch https://github.com/espressif/esp-rom-elfs/releases/download/20260528/esp-rom-elfs-20260528.tar.gz \
        "$D/esp-rom-elfs.tar.gz" caa463d3cbef2430a5a35847c1d9f2f152403b17a802050927ff60c8da54fe46
      python3 -I -c 'import sys, tarfile
t = tarfile.open(sys.argv[1], "r:gz")
m = next(m for m in t.getmembers() if m.name.endswith("/esp32s31_rev0_rom.elf") or m.name == "esp32s31_rev0_rom.elf")
open(sys.argv[2], "wb").write(t.extractfile(m).read())' "$D/esp-rom-elfs.tar.gz" "$D/esp32s31_rev0_rom.elf"
      echo "a52ad7513deb656a910a5740125f1cce2c7941f11ce57213b7b43aea93d5ab87  $D/esp32s31_rev0_rom.elf" |
        shasum -a 256 -c --status || { echo "esp32s31_rev0_rom.elf: checksum mismatch" >&2; exit 1; }
    fi
    if [ ! -s "$D/images/s31_full_flash.bin" ]; then
      fetch https://github.com/joakimeriksson/sbc-emu/releases/download/esp32s31-linux-1/s31_full_flash.bin.gz \
        "$D/s31_full_flash.bin.gz" 90210bf84f6f66760bde41a34f94efb05b2da4170bc34c3e9971680fa6dd522b
      gzip -dc "$D/s31_full_flash.bin.gz" > "$D/images/s31_full_flash.bin.part"
      mv "$D/images/s31_full_flash.bin.part" "$D/images/s31_full_flash.bin"
    fi
    exec "$BIN" run --board esp32s31 --rom "$D/esp32s31_rev0_rom.elf" --flash "$D/images/s31_full_flash.bin" "$@" ;;
esac
