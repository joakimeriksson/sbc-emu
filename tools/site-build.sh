#!/bin/sh
# Build the public demo site into out/site (.github/workflows/site.yml deploys it to GitHub Pages):
# the browser build; the snapshot hello.html resumes (the Raspberry Pi 4), made here from pinned,
# checksummed downloads; the Orange Pi One's Alpine kernel and initramfs
# (scripts/orangepi-one-fixture.sh), the Tinker Board's Debian kernel and BusyBox initramfs
# (scripts/tinker-board-fixture.sh), the Orange Pi RV's OpenSBI, Alpine kernel and initramfs
# (scripts/orangepi-rv-fixture.sh) and the Orange Pi 5's Debian kernel and initramfs
# (scripts/orangepi5-fixture.sh), the Jetson Nano's Debian kernel, device tree and initramfs
# (scripts/jetson-nano-debian-fixture.sh) and its GPU demos with Mesa
# (scripts/jetson-nano-gpu-demo-fixture.sh), and the ESP32-S31's mask ROM (Espressif's esp-rom-elfs) and
# Linux flash image (scripts/esp32s31-fixture.sh; a release asset, as its build takes an hour), all
# pinned; and the Pi 4 3D demos' snapshot (gpu.html), from a release asset. boards.html is the
# front page. Every file is a whole-file fetch, so any static web server can serve the site:
#   tools/site-build.sh && python3 -m http.server -d out/site 8792
set -eu
cd "$(dirname "$0")/.."

# the guest (web/sources.html lists these with their source code)
KERNEL_URL=https://github.com/raspberrypi/firmware/raw/1.20260915/boot/kernel8.img
KERNEL_SHA256=f80aa8bb092c0df9401065ec1b4ee8b8846c4d9a1bb3272662b50acbd89da68b
BUSYBOX_URL=https://dl-cdn.alpinelinux.org/alpine/v3.20/main/aarch64/busybox-static-1.36.1-r31.apk
BUSYBOX_SHA256=1d8e7a7fc2ed69bdb8eb7be9d962489c21a0f75b72d8a325437c8a09d4cfac70
# the 3D demos (gpu.html): Raspberry Pi OS with Mesa, logged in with every demo already run once
# (scripts/gpu-demo-snapshot.txt, docs/gpu.md: in the browser). It is a release asset because it is
# made from an 8 GiB guest image CI cannot build. The demos run from the guest's page cache, so the
# site has no SD card; the check after the browser build runs kmscube on it. Recompressed with
# tools/snap-zstd.sh, which only the browser build reads.
GPU_SNAP_URL=https://github.com/joakimeriksson/sbc-emu/releases/download/web-gpu-demo-1/gpu-demo.snap.zst
GPU_SNAP_SHA256=ff3b8fcb11751fc32f26d6c5e5f575342915aa912318a59d2317216a910425fb
S31_ROM_URL=https://github.com/espressif/esp-rom-elfs/releases/download/20260528/esp-rom-elfs-20260528.tar.gz
S31_ROM_TGZ_SHA256=caa463d3cbef2430a5a35847c1d9f2f152403b17a802050927ff60c8da54fe46
S31_ROM_SHA256=a52ad7513deb656a910a5740125f1cce2c7941f11ce57213b7b43aea93d5ab87
S31_FLASH_URL=https://github.com/joakimeriksson/sbc-emu/releases/download/esp32s31-linux-1/s31_full_flash.bin.gz
S31_FLASH_SHA256=90210bf84f6f66760bde41a34f94efb05b2da4170bc34c3e9971680fa6dd522b
DL=cache/site
OUT=out/site

fetch() { # URL FILE SHA256
  if ! { [ -f "$2" ] && echo "$3  $2" | shasum -a 256 -c --status; }; then
    curl -fsSL --retry 3 -o "$2.part" "$1"
    mv "$2.part" "$2"
  fi
  echo "$3  $2" | shasum -a 256 -c
}
mkdir -p "$DL"
fetch "$KERNEL_URL" "$DL/kernel8.img" "$KERNEL_SHA256"
fetch "$BUSYBOX_URL" "$DL/busybox-static.apk" "$BUSYBOX_SHA256"
fetch "$GPU_SNAP_URL" "$DL/gpu-demo.snap.zst" "$GPU_SNAP_SHA256"
# an .apk is gzip members of tar archives one after another
python3 -I -c 'import sys, tarfile
t = tarfile.open(sys.argv[1], "r:gz", ignore_zeros=True)
open(sys.argv[2], "wb").write(t.extractfile("bin/busybox.static").read())' "$DL/busybox-static.apk" "$DL/busybox.static"
gzip -dc "$DL/kernel8.img" > "$DL/Image"
# the ESP32-S31: Espressif's mask ROM out of esp-rom-elfs, and the flash image as published
fetch "$S31_ROM_URL" "$DL/esp-rom-elfs.tar.gz" "$S31_ROM_TGZ_SHA256"
python3 -I -c 'import sys, tarfile
t = tarfile.open(sys.argv[1], "r:gz")
m = next(m for m in t.getmembers() if m.name.endswith("/esp32s31_rev0_rom.elf") or m.name == "esp32s31_rev0_rom.elf")
open(sys.argv[2], "wb").write(t.extractfile(m).read())' "$DL/esp-rom-elfs.tar.gz" "$DL/esp32s31_rev0_rom.elf"
echo "$S31_ROM_SHA256  $DL/esp32s31_rev0_rom.elf" | shasum -a 256 -c
fetch "$S31_FLASH_URL" "$DL/s31_full_flash.bin.gz" "$S31_FLASH_SHA256"

cargo build --release -p sbc-emu
./target/release/sbc-emu mkinitramfs --busybox "$DL/busybox.static" -o "$DL/initramfs.cpio"
# boot to the shell; the script saves web/assets/hello.snap
mkdir -p web/assets
rm -f web/assets/hello.snap
./target/release/sbc-emu run --kernel "$DL/Image" --initrd "$DL/initramfs.cpio" --shell fb --input usb --ram-mb 512 \
    --script scripts/web-hello-snapshot.txt --timeout 900
[ -s web/assets/hello.snap ] || { echo "no snapshot: the boot did not reach the shell" >&2; exit 1; }

# the Orange Pi One and the Tinker Board (wasm-build.sh stages them into web/assets)
sh scripts/orangepi-one-fixture.sh cache/orangepi-one
sh scripts/tinker-board-fixture.sh cache/tinker-board
# the Orange Pi RV (wasm-build.sh stages it into web/assets/orangepi-rv)
sh scripts/orangepi-rv-fixture.sh cache/orangepi-rv
# the Orange Pi 5 (wasm-build.sh stages it into web/assets/orangepi5)
sh scripts/orangepi5-fixture.sh cache/orangepi5
# the Jetson Nano (wasm-build.sh stages it into web/assets/jetson-nano)
DOWNLOADS="$PWD/$DL" sh scripts/jetson-nano-debian-fixture.sh cache/jetson-nano/debian
DOWNLOADS="$PWD/$DL" sh scripts/jetson-nano-gpu-demo-fixture.sh cache/jetson-nano/gpu-demo

tools/wasm-build.sh
# this build must still restore the 3D snapshot and draw with it, without an SD card
SBC_SNAP="$DL/gpu-demo.snap.zst" SBC_SD=/nonexistent node tools/wasm-gpu-bench.mjs 3 cube
# and its xz decoder must unpack the Jetson Nano's demos
SBC_GPU=cache/jetson-nano/gpu-demo/gpu-demo.cpio.xz node tools/wasm-smoke-nano.mjs unpack

rm -rf "$OUT"
mkdir -p "$OUT/wasm" "$OUT/assets/orangepi-one" "$OUT/assets/tinker" "$OUT/assets/orangepi-rv" "$OUT/assets/orangepi5" "$OUT/assets/jetson-nano" "$OUT/assets/esp32s31"
cp web/boards.html "$OUT/index.html"
cp web/hello.html web/board.html web/gpu.html web/pi4.html web/sources.html web/worker.js web/film-thumbnail.jpg "$OUT/"
cp web/wasm/sbc-emu.wasm "$OUT/wasm/"
cp web/assets/hello.snap "$OUT/assets/"
# GitHub Pages takes files up to 100 MB: the snapshot goes in parts, which a manifest lists
split -b 90000000 -a 1 "$DL/gpu-demo.snap.zst" "$OUT/assets/gpu-demo.snap."
python3 -I -c 'import json, os, sys
d, parts = sys.argv[1], sorted(p for p in os.listdir(sys.argv[1]) if p.startswith("gpu-demo.snap."))
json.dump({"parts": parts, "bytes": sum(os.path.getsize(os.path.join(d, p)) for p in parts), "sectors": 16777216},
          open(os.path.join(d, "gpu-demo.snap.json"), "w"))' "$OUT/assets"
cp web/assets/orangepi-one/zImage web/assets/orangepi-one/sun8i-h3-orangepi-one.dtb web/assets/orangepi-one/initramfs.cpio.gz "$OUT/assets/orangepi-one/"
cp web/assets/tinker/zImage web/assets/tinker/rk3288-tinker-s.dtb web/assets/tinker/initramfs.cpio.gz "$OUT/assets/tinker/"
cp web/assets/orangepi-rv/fw_dynamic.bin web/assets/orangepi-rv/Image.gz web/assets/orangepi-rv/jh7110-orangepi-rv.dtb web/assets/orangepi-rv/initramfs.cpio.gz "$OUT/assets/orangepi-rv/"
cp web/assets/orangepi5/Image.gz web/assets/orangepi5/initramfs.cpio.gz "$OUT/assets/orangepi5/"
cp web/assets/jetson-nano/Image.gz web/assets/jetson-nano/tegra210-p3450-0000.dtb web/assets/jetson-nano/initramfs.cpio.gz web/assets/jetson-nano/gpu-demo.cpio.xz "$OUT/assets/jetson-nano/"
cp "$DL/esp32s31_rev0_rom.elf" "$DL/s31_full_flash.bin.gz" "$OUT/assets/esp32s31/"
touch "$OUT/.nojekyll"
find "$OUT" -type f -exec ls -l {} + | awk '{ print $5, $9 }'
